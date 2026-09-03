//! Typed decision, lesson, and suggested-rule endpoints (Wave 3 parity).
//!
//! Every method here targets the typed hosted contract first:
//!
//! * `GET /memory/decisions?format=envelope` → `decisions.v1` envelope
//! * `POST /memory/decisions`, `POST /memory/decisions/:id/actions`,
//!   `GET /memory/decisions/:id/trace`
//! * `GET|POST /lessons`, `GET|PATCH|DELETE /lessons/:id`,
//!   `POST /lessons/:id/supersede`, `GET /lessons/warnings`
//!
//! The client never invents fields. When a server still answers with the
//! legacy decision array the envelope is synthesised with an explicit
//! `degraded` entry so tools can say `[PARTIAL]`; a `404` on `/lessons` is
//! surfaced unchanged so callers can fall back to the events-based path and
//! state that in their tool text.

use crate::client::{scope_ids_with_defaults, strip_nulls, ContextStreamClient};
use mcp_types::Result;
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

/// Query parameters for the typed decisions listing.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ListDecisionsParams {
    pub workspace_id: Option<Uuid>,
    pub project_id: Option<Uuid>,
    pub query: Option<String>,
    pub category: Option<String>,
    /// `recency` | `relevance`
    pub sort: Option<String>,
    /// `active` | `superseded` | `disputed` | `verified` | `all`
    pub status: Option<String>,
    /// ISO-8601 lower bound.
    pub since: Option<String>,
    pub offset: Option<i64>,
    pub limit: Option<i64>,
    pub source: Option<String>,
}

/// Body for `POST /memory/decisions`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CreateDecisionParams {
    pub workspace_id: Option<Uuid>,
    pub project_id: Option<Uuid>,
    pub title: String,
    pub content: String,
    pub rationale: Option<String>,
    /// `[{"option": "...", "rejected_reason": "..."}]`
    pub alternatives: Option<Vec<Value>>,
    pub scope: Option<String>,
    pub confidence: Option<f64>,
    pub supersedes: Option<Uuid>,
    pub category: Option<String>,
    pub tags: Option<Vec<String>>,
    pub session_id: Option<String>,
}

/// Body for `POST /memory/decisions/:id/actions`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct DecisionActionParams {
    /// `supersede` | `dispute` | `verify` | `invalidate` | `choose_successor`
    pub action: String,
    pub successor_id: Option<Uuid>,
    pub reason: Option<String>,
    pub title: Option<String>,
}

/// Valid `action` values for [`DecisionActionParams`].
pub const DECISION_ACTIONS: &[&str] = &[
    "supersede",
    "dispute",
    "verify",
    "invalidate",
    "choose_successor",
];

/// Query parameters for `GET /lessons`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ListLessonsParams {
    pub workspace_id: Option<Uuid>,
    pub project_id: Option<Uuid>,
    pub query: Option<String>,
    pub severity: Option<Vec<String>>,
    pub min_severity: Option<String>,
    pub category: Option<String>,
    pub keywords: Option<Vec<String>>,
    pub scope: Option<String>,
    pub include_superseded: Option<bool>,
    pub since: Option<String>,
    pub sort: Option<String>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// Body for `POST /lessons`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CreateLessonParams {
    pub workspace_id: Option<Uuid>,
    pub project_id: Option<Uuid>,
    pub title: String,
    pub trigger: String,
    pub impact: String,
    pub prevention: String,
    pub severity: Option<String>,
    pub category: Option<String>,
    pub keywords: Option<Vec<String>>,
}

/// Body for `PATCH /lessons/:id`. Every field is optional.
#[derive(Debug, Clone, Default, Serialize)]
pub struct UpdateLessonParams {
    pub title: Option<String>,
    pub trigger: Option<String>,
    pub impact: Option<String>,
    pub prevention: Option<String>,
    pub severity: Option<String>,
    pub category: Option<String>,
    pub keywords: Option<Vec<String>>,
}

impl UpdateLessonParams {
    pub fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.trigger.is_none()
            && self.impact.is_none()
            && self.prevention.is_none()
            && self.severity.is_none()
            && self.category.is_none()
            && self.keywords.is_none()
    }
}

fn push_param(params: &mut Vec<String>, key: &str, value: Option<&str>) {
    if let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) {
        params.push(format!("{key}={}", urlencoding::encode(value)));
    }
}

/// Wrap a legacy decision array into the `decisions.v1` envelope shape so
/// every consumer sees one contract. The synthesised envelope carries an
/// explicit `degraded` entry and `legacy_array: true`; it never fabricates
/// `status` / `freshness` / `source` on the items.
pub fn normalize_decisions_envelope(raw: Value, sort: Option<&str>) -> Value {
    match raw {
        Value::Array(items) => {
            let total = items.len();
            serde_json::json!({
                "items": items,
                "total": total,
                "next_offset": Value::Null,
                "sort": sort.unwrap_or("relevance"),
                "scope": Value::Null,
                "degraded": [{
                    "source": "decisions_envelope",
                    "detail": "server returned the legacy decision array; status, freshness, and source fields are unavailable"
                }],
                "legacy_array": true,
            })
        }
        Value::Object(mut object) => {
            if !object.contains_key("items") {
                let items = object
                    .remove("results")
                    .or_else(|| object.remove("decisions"))
                    .unwrap_or_else(|| Value::Array(Vec::new()));
                object.insert("items".to_string(), items);
            }
            if !object.contains_key("degraded") {
                object.insert("degraded".to_string(), Value::Array(Vec::new()));
            }
            if !object.contains_key("schema_version") {
                object.insert("legacy_array".to_string(), Value::Bool(true));
                if let Some(Value::Array(degraded)) = object.get_mut("degraded") {
                    degraded.push(serde_json::json!({
                        "source": "decisions_envelope",
                        "detail": "server response carried no decisions.v1 schema_version; typed fields may be missing"
                    }));
                }
            }
            Value::Object(object)
        }
        other => serde_json::json!({
            "items": [],
            "total": 0,
            "degraded": [{
                "source": "decisions_envelope",
                "detail": format!("unexpected decisions payload shape: {}", other)
            }],
            "legacy_array": true,
        }),
    }
}

impl ContextStreamClient {
    /// `GET /memory/decisions?…&format=envelope`.
    ///
    /// Returns the `decisions.v1` envelope. Servers that still answer with
    /// the legacy array are normalised through
    /// [`normalize_decisions_envelope`] (with a `degraded` entry) so tools
    /// can render `[PARTIAL]` instead of guessing.
    pub async fn list_decisions_envelope(&self, params: ListDecisionsParams) -> Result<Value> {
        let config = self.config().await;
        let (ws_id, proj_id) =
            scope_ids_with_defaults(params.workspace_id, params.project_id, &config);
        let ws_id = ws_id.ok_or_else(|| {
            mcp_types::Error::Validation("workspace_id is required for decisions".to_string())
        })?;

        let mut query = vec![format!("workspace_id={ws_id}")];
        if let Some(id) = proj_id {
            query.push(format!("project_id={id}"));
        }
        push_param(&mut query, "query", params.query.as_deref());
        push_param(&mut query, "category", params.category.as_deref());
        push_param(&mut query, "sort", params.sort.as_deref());
        push_param(&mut query, "status", params.status.as_deref());
        push_param(&mut query, "since", params.since.as_deref());
        push_param(&mut query, "source", params.source.as_deref());
        if let Some(offset) = params.offset {
            query.push(format!("offset={offset}"));
        }
        if let Some(limit) = params.limit {
            query.push(format!("limit={limit}"));
        }
        query.push("format=envelope".to_string());

        let raw: Value = self
            .get(&format!("/memory/decisions?{}", query.join("&")))
            .await?;
        Ok(normalize_decisions_envelope(raw, params.sort.as_deref()))
    }

    /// `POST /memory/decisions` — typed decision create.
    pub async fn create_decision(&self, params: CreateDecisionParams) -> Result<Value> {
        let config = self.config().await;
        let (ws_id, proj_id) =
            scope_ids_with_defaults(params.workspace_id, params.project_id, &config);
        let ws_id = ws_id.ok_or_else(|| {
            mcp_types::Error::Validation(
                "workspace_id is required to create a decision. Run init first.".to_string(),
            )
        })?;
        let body = strip_nulls(serde_json::json!({
            "workspace_id": ws_id,
            "project_id": proj_id,
            "title": params.title,
            "content": params.content,
            "rationale": params.rationale,
            "alternatives": params.alternatives,
            "scope": params.scope,
            "confidence": params.confidence,
            "supersedes": params.supersedes,
            "category": params.category,
            "tags": params.tags,
            "session_id": params.session_id,
        }));
        let result = self.post("/memory/decisions", body).await?;
        self.invalidate_memory_read_caches();
        Ok(result)
    }

    /// `POST /memory/decisions/:id/actions`.
    pub async fn decision_action(
        &self,
        decision_id: Uuid,
        params: DecisionActionParams,
    ) -> Result<Value> {
        let body = strip_nulls(serde_json::json!({
            "action": params.action,
            "successor_id": params.successor_id,
            "reason": params.reason,
            "title": params.title,
        }));
        let result = self
            .post(&format!("/memory/decisions/{decision_id}/actions"), body)
            .await?;
        self.invalidate_memory_read_caches();
        Ok(result)
    }

    /// `GET /memory/decisions/:id/trace`.
    pub async fn get_decision_trace(&self, decision_id: Uuid) -> Result<Value> {
        self.get(&format!("/memory/decisions/{decision_id}/trace"))
            .await
    }

    /// Events-era supersede link (`POST /memory/nodes/:id/supersede`) used
    /// as the fallback for `decision_action(action="supersede")` when the
    /// typed decisions endpoint is absent.
    pub async fn link_node_superseded_by(
        &self,
        node_id: Uuid,
        successor_id: Uuid,
    ) -> Result<Value> {
        let result = self
            .post(
                &format!("/memory/nodes/{node_id}/supersede"),
                serde_json::json!({ "superseded_by": successor_id }),
            )
            .await?;
        self.invalidate_memory_read_caches();
        Ok(result)
    }

    /// `GET /lessons?…&format=envelope` → `lessons.v1` envelope.
    pub async fn list_lessons(&self, params: ListLessonsParams) -> Result<Value> {
        let config = self.config().await;
        let (ws_id, proj_id) =
            scope_ids_with_defaults(params.workspace_id, params.project_id, &config);
        let mut query = Vec::new();
        if let Some(id) = ws_id {
            query.push(format!("workspace_id={id}"));
        }
        if let Some(id) = proj_id {
            query.push(format!("project_id={id}"));
        }
        push_param(&mut query, "query", params.query.as_deref());
        for severity in params.severity.iter().flatten() {
            push_param(&mut query, "severity[]", Some(severity));
        }
        push_param(&mut query, "min_severity", params.min_severity.as_deref());
        push_param(&mut query, "category", params.category.as_deref());
        for keyword in params.keywords.iter().flatten() {
            push_param(&mut query, "keywords[]", Some(keyword));
        }
        push_param(&mut query, "scope", params.scope.as_deref());
        if let Some(include) = params.include_superseded {
            query.push(format!("include_superseded={include}"));
        }
        push_param(&mut query, "since", params.since.as_deref());
        push_param(&mut query, "sort", params.sort.as_deref());
        if let Some(limit) = params.limit {
            query.push(format!("limit={limit}"));
        }
        if let Some(offset) = params.offset {
            query.push(format!("offset={offset}"));
        }
        query.push("format=envelope".to_string());
        self.get(&format!("/lessons?{}", query.join("&"))).await
    }

    /// `GET /lessons/:id`.
    pub async fn get_lesson(&self, lesson_id: Uuid) -> Result<Value> {
        self.get(&format!("/lessons/{lesson_id}")).await
    }

    /// `POST /lessons`.
    pub async fn create_lesson(&self, params: CreateLessonParams) -> Result<Value> {
        let config = self.config().await;
        let (ws_id, proj_id) =
            scope_ids_with_defaults(params.workspace_id, params.project_id, &config);
        let body = strip_nulls(serde_json::json!({
            "workspace_id": ws_id,
            "project_id": proj_id,
            "title": params.title,
            "trigger": params.trigger,
            "impact": params.impact,
            "prevention": params.prevention,
            "severity": params.severity,
            "category": params.category,
            "keywords": params.keywords,
        }));
        let result = self.post("/lessons", body).await?;
        self.invalidate_memory_read_caches();
        Ok(result)
    }

    /// `PATCH /lessons/:id`.
    pub async fn update_lesson(
        &self,
        lesson_id: Uuid,
        params: UpdateLessonParams,
    ) -> Result<Value> {
        let body = strip_nulls(serde_json::to_value(&params).unwrap_or(Value::Null));
        let result = self.patch(&format!("/lessons/{lesson_id}"), body).await?;
        self.invalidate_memory_read_caches();
        Ok(result)
    }

    /// `POST /lessons/:id/supersede` with either `{successor_id}` or the
    /// replacement lesson fields.
    pub async fn supersede_lesson(&self, lesson_id: Uuid, body: Value) -> Result<Value> {
        let result = self
            .post(
                &format!("/lessons/{lesson_id}/supersede"),
                strip_nulls(body),
            )
            .await?;
        self.invalidate_memory_read_caches();
        Ok(result)
    }

    /// `DELETE /lessons/:id`.
    pub async fn delete_lesson(&self, lesson_id: Uuid) -> Result<Value> {
        let result = self.delete(&format!("/lessons/{lesson_id}")).await?;
        self.invalidate_memory_read_caches();
        Ok(result)
    }

    /// `GET /lessons/warnings?user_message=` → `{items:[{lesson, relevance, reason}], rule, degraded}`.
    pub async fn lessons_warnings(
        &self,
        workspace_id: Option<Uuid>,
        project_id: Option<Uuid>,
        user_message: &str,
        limit: Option<i64>,
    ) -> Result<Value> {
        let config = self.config().await;
        let (ws_id, proj_id) = scope_ids_with_defaults(workspace_id, project_id, &config);
        let mut query = Vec::new();
        push_param(&mut query, "user_message", Some(user_message));
        if let Some(id) = ws_id {
            query.push(format!("workspace_id={id}"));
        }
        if let Some(id) = proj_id {
            query.push(format!("project_id={id}"));
        }
        if let Some(limit) = limit {
            query.push(format!("limit={limit}"));
        }
        self.get(&format!("/lessons/warnings?{}", query.join("&")))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn legacy_array_is_wrapped_with_degraded_entry() {
        let wrapped = normalize_decisions_envelope(json!([{"id": "a"}]), Some("recency"));
        assert_eq!(wrapped["items"].as_array().map(Vec::len), Some(1));
        assert_eq!(wrapped["total"], 1);
        assert_eq!(wrapped["sort"], "recency");
        assert_eq!(wrapped["legacy_array"], true);
        assert_eq!(wrapped["degraded"][0]["source"], "decisions_envelope");
        // No typed fields are invented on the item.
        assert!(wrapped["items"][0].get("status").is_none());
    }

    #[test]
    fn envelope_passes_through_untouched() {
        let envelope = json!({
            "items": [{"id": "a", "status": "active"}],
            "total": 1,
            "degraded": [],
            "schema_version": "decisions.v1"
        });
        let normalized = normalize_decisions_envelope(envelope.clone(), None);
        assert_eq!(normalized, envelope);
    }

    #[test]
    fn object_without_schema_version_is_marked_legacy() {
        let normalized = normalize_decisions_envelope(json!({"results": [{"id": "a"}]}), None);
        assert_eq!(normalized["items"].as_array().map(Vec::len), Some(1));
        assert_eq!(normalized["legacy_array"], true);
        assert_eq!(normalized["degraded"].as_array().map(Vec::len), Some(1));
    }
}
