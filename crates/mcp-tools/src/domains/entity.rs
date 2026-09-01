//! Unified entity tool — CRUD across the Phase 1-3 taxonomy expansion tables.
//!
//! Mirrors the `memory()` design philosophy of dispatching across many record
//! kinds via a `kind` discriminator instead of fanning out to 10 separate
//! tools. The kinds map to URL fragments under `/api/v1/<segment>` from the
//! contextstream API.
//!
//! ## Supported kinds
//!
//! | kind            | URL segment       | Notes                                        |
//! |-----------------|-------------------|----------------------------------------------|
//! | `ticket`        | `tickets`         | Bugs / features / tasks / chores / incidents / epics |
//! | `handoff`       | `handoffs`        | Wraps a ContextCapsule with workflow shell   |
//! | `backlog_view`  | `backlog-views`   | Saved ordered slices over tickets            |
//! | `incident`      | `incidents`       | Operational incidents w/ severity + status   |
//! | `release`       | `releases`        | Versioned releases                           |
//! | `experiment`    | `experiments`     | A/B tests + product experiments              |
//! | `goal`          | `goals`           | OKR objectives (parent of key_results)       |
//! | `key_result`    | `key-results`     | Nested under a goal on create                |
//! | `sprint`        | `sprints`         | Time-boxed iterations                        |
//! | `review`        | `reviews`         | PR / code / design / security / architecture |
//! | `risk`          | `risks`           | Active risk register                         |
//! | `coordination`  | `coordinations`   | Cross-workspace shared knowledge items       |
//!
//! ## Actions
//!
//! - `list`   — list entities, with optional filter `query`. Defaults
//!   `workspace_id`/`project_id` to the active scope.
//! - `get`    — fetch one entity by `id`.
//! - `create` — create with `body` (free-form JSON; defaults filled in).
//! - `update` — patch entity by `id` with `body`.
//! - `delete` — soft-delete entity by `id`.
//!
//! ## Examples
//!
//! ```text
//! entity(kind="ticket",   action="create", body={"title": "Fix replication lag", "kind": "bug", "priority": "high", "assignees": [{"email": "alice@example.com", "role": "owner"}], "linked_items": [{"kind": "runbook", "id": "<doc-uuid>", "title_snapshot": "Replication runbook"}]})
//! entity(kind="ticket",   action="list",   query={"status": "open", "kind": "bug"})
//! entity(kind="goal",     action="create", body={"objective": "Ship 100 customers", "period": "2026-Q2"})
//! entity(kind="key_result", action="create", body={"goal_id": "<uuid>", "title": "MAU > 10k", "target_value": 10000})
//! entity(kind="incident", action="update", id="<uuid>", body={"status": "mitigated"})
//! entity(kind="risk",     action="list",   query={"impact": "severe", "status": "open"})
//! ```

use async_trait::async_trait;
use mcp_client::{
    append_ticket_extras, enrich_ticket_result_from_request, entity_kind_to_path,
    normalize_ticket_body, ContextStreamClient, VALID_ENTITY_KINDS,
};
use mcp_session::SessionManager;
use mcp_types::{
    atlas_layer::AtlasWarmCacheKind,
    tool::{ToolAnnotations, ToolCategory, ToolMetadata, ToolResult},
    AtlasLayer, Error, Result,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;

use crate::domains::account_mode::apply_is_personal_to_body;
use crate::registry::ToolHandler;
use crate::schema::SchemaBuilder;

const VALID_ACTIONS: &[&str] = &["list", "get", "create", "update", "delete"];

/// Input for the unified entity tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityInput {
    /// Entity kind: ticket, handoff, backlog_view, incident, release,
    /// experiment, goal, key_result, sprint, review, risk.
    pub kind: String,
    /// Action: list, get, create, update, delete.
    pub action: String,
    /// Entity ID (required for get / update / delete).
    pub id: Option<String>,
    /// Workspace scope. Defaults to the active workspace if omitted.
    pub workspace_id: Option<String>,
    /// Project scope. Defaults to the active project if omitted.
    pub project_id: Option<String>,
    /// Body for create / update. Free-form JSON forwarded to the API.
    pub body: Option<Value>,
    /// Query / filter params for list. Object whose keys map to query params.
    pub query: Option<Value>,
}

/// Unified entity tool handler.
pub struct EntityTool {
    client: ContextStreamClient,
    session: Arc<SessionManager>,
    atlas_layer: AtlasLayer,
}

impl EntityTool {
    pub fn new(client: ContextStreamClient, session: Arc<SessionManager>) -> Self {
        Self::with_atlas(client, session, mcp_types::atlas_layer::noop_layer())
    }

    pub fn with_atlas(
        client: ContextStreamClient,
        session: Arc<SessionManager>,
        atlas_layer: AtlasLayer,
    ) -> Self {
        Self {
            client,
            session,
            atlas_layer,
        }
    }

    fn warm_cache_kind(kind: &str) -> Option<AtlasWarmCacheKind> {
        match kind {
            "ticket" => Some(AtlasWarmCacheKind::TicketsHot),
            "handoff" => Some(AtlasWarmCacheKind::HandoffsHot),
            "incident" => Some(AtlasWarmCacheKind::IncidentsHot),
            _ => None,
        }
    }

    /// Resolve scope IDs for an entity call. Precedence (highest first):
    ///   1. Explicit `workspace_id` / `project_id` in the call body.
    ///   2. SessionManager's current per-task active scope (set by
    ///      `init` / `context` and partitioned by `TASK_SESSION_KEY` so
    ///      it doesn't leak across users on the hosted gateway).
    ///   3. Falls through to None — letting the API server's own
    ///      per-user defaulting kick in.
    ///
    /// Returns `(workspace_id, project_id)` as `Option<String>` so the
    /// existing query/body builders can ignore None values cleanly.
    /// Emits a warn-level trace when the session active scope is being
    /// applied (i.e., when the caller didn't pass IDs and the session
    /// supplied them) — useful for spotting cases where an agent is
    /// relying on implicit scope when it shouldn't.
    async fn resolve_scope(
        &self,
        explicit_workspace_id: &Option<String>,
        explicit_project_id: &Option<String>,
    ) -> (Option<String>, Option<String>) {
        let mut ws = explicit_workspace_id.clone();
        let mut proj = explicit_project_id.clone();
        if ws.is_some() && proj.is_some() {
            return (ws, proj);
        }

        let state = self.session.state().await;
        let mut applied_session_ws = false;
        let mut applied_session_proj = false;
        if ws.is_none() {
            if let Some(id) = state.workspace_id {
                ws = Some(id.to_string());
                applied_session_ws = true;
            }
        }
        if proj.is_none() {
            if let Some(id) = state.project_id {
                proj = Some(id.to_string());
                applied_session_proj = true;
            }
        }

        if applied_session_ws || applied_session_proj {
            tracing::debug!(
                target: "entity",
                applied_session_ws,
                applied_session_proj,
                ws = ?ws,
                proj = ?proj,
                "entity: applied session active scope to fill missing ids"
            );
        }

        (ws, proj)
    }

    /// Flatten a JSON `query` object plus explicit `workspace_id` /
    /// `project_id` into URL query param pairs. Nested objects + non-scalar
    /// values are silently dropped (URL queries can't represent them).
    fn build_list_params(
        query: &Option<Value>,
        workspace_id: &Option<String>,
        project_id: &Option<String>,
    ) -> Vec<(String, String)> {
        let mut params: Vec<(String, String)> = Vec::new();

        if let Some(ws) = workspace_id {
            params.push(("workspace_id".to_string(), ws.clone()));
        }
        if let Some(pj) = project_id {
            params.push(("project_id".to_string(), pj.clone()));
        }

        if let Some(q) = query {
            if let Some(obj) = q.as_object() {
                for (k, v) in obj {
                    let val: Option<String> = match v {
                        Value::String(s) => Some(s.clone()),
                        Value::Number(n) => Some(n.to_string()),
                        Value::Bool(b) => Some(b.to_string()),
                        Value::Null => None,
                        // Arrays/objects can't be represented in URL params —
                        // skip rather than half-encode.
                        _ => None,
                    };
                    if let Some(v) = val {
                        params.push((k.clone(), v));
                    }
                }
            }
        }

        params
    }

    /// Inject `workspace_id` / `project_id` from the explicit input fields
    /// into the body if not already present. The client also fills in
    /// config defaults — this just preserves explicit caller intent.
    fn inject_scope_into_body(
        body: &mut Value,
        workspace_id: &Option<String>,
        project_id: &Option<String>,
    ) {
        if let Some(obj) = body.as_object_mut() {
            if !obj.contains_key("workspace_id") {
                if let Some(ws) = workspace_id {
                    obj.insert("workspace_id".to_string(), Value::String(ws.clone()));
                }
            }
            if !obj.contains_key("project_id") {
                if let Some(pj) = project_id {
                    obj.insert("project_id".to_string(), Value::String(pj.clone()));
                }
            }
        }
    }

    async fn resolve_entity_id_for_action(
        &self,
        kind: &str,
        lookup: &str,
        workspace_id: &Option<String>,
        project_id: &Option<String>,
    ) -> Result<(Uuid, Option<String>)> {
        if let Ok(id) = Uuid::parse_str(lookup.trim()) {
            return Ok((id, None));
        }

        let mut params = Vec::new();
        if let Some(ws) = workspace_id {
            params.push(("workspace_id".to_string(), ws.clone()));
        }
        if let Some(pj) = project_id {
            params.push(("project_id".to_string(), pj.clone()));
        }
        params.push(("limit".to_string(), "100".to_string()));
        params.push(("query".to_string(), lookup.trim().to_string()));

        let list = self.client.entity_list(kind, params).await?;
        let items = extract_items_array(&list).cloned().unwrap_or_default();
        let ranked = rank_entity_matches(&items, lookup);
        let best = ranked.first().ok_or_else(|| {
            Error::Validation(format!(
                "No {} found matching \"{}\". Use entity(kind=\"{}\", action=\"list\", query={{\"query\": \"{}\"}}) to inspect candidates.",
                plural_kind(kind),
                lookup,
                kind,
                lookup
            ))
        })?;

        let second_score = ranked.get(1).map(|m| m.score).unwrap_or_default();
        if !best.exact && second_score > 0 && best.score <= second_score + 200 {
            return Err(Error::Validation(format_entity_match_disambiguation(
                kind, lookup, &ranked,
            )));
        }

        let note = if best.exact {
            None
        } else {
            Some(format!(
                "Resolved \"{}\" to {} **{}** (id: {}).",
                lookup, kind, best.title, best.id
            ))
        };
        Ok((best.id, note))
    }
}

/// Drill through the common list-response shapes to a flat `Vec<Value>` of
/// entity items. Mirrors `count_items` so the formatter and the count never
/// drift apart.
fn extract_items_array(result: &Value) -> Option<&Vec<Value>> {
    if let Some(arr) = result.as_array() {
        return Some(arr);
    }
    let obj = result.as_object()?;
    if let Some(arr) = obj.get("items").and_then(|v| v.as_array()) {
        return Some(arr);
    }
    obj.get("data")
        .and_then(|v| v.as_object())
        .and_then(|o| o.get("items"))
        .and_then(|v| v.as_array())
}

/// Best-effort title for an entity item, walking the per-kind field naming
/// before falling back to a content snippet. Different entity kinds keep their
/// human-readable label under different keys (`objective` for goals,
/// `version` for releases, `name` for sprints/experiments) so we have to try
/// several before giving up.
fn entity_display_title(item: &Value) -> String {
    for field in ["title", "objective", "version", "name", "summary"] {
        if let Some(raw) = item.get(field).and_then(|v| v.as_str()) {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    "(no title)".to_string()
}

/// Pull the most useful "what kind of thing is this" label for an entity row.
/// Tickets store this in `kind`, incidents/risks in `severity`/`impact`,
/// backlog views in `bucket`, etc. Returning `None` is fine — the formatter
/// just omits the bracketed label.
fn entity_type_label(item: &Value) -> Option<String> {
    for field in [
        "kind", "severity", "impact", "bucket", "doc_type", "priority",
    ] {
        if let Some(value) = item.get(field).and_then(|v| v.as_str()) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

/// Drill through common single-object envelope shapes to the actual entity
/// body. APIs sometimes return `{...entity...}` directly, sometimes wrap
/// under `data` or the kind-name (`ticket`/`incident`/etc.), so we walk a
/// short list of likely keys before giving up.
fn extract_entity_object<'a>(kind: &str, result: &'a Value) -> &'a Value {
    if let Some(obj) = result.as_object() {
        for key in [kind, "data", "entity", "item", "result"] {
            if let Some(inner) = obj.get(key) {
                if inner.is_object() {
                    return inner;
                }
            }
        }
    }
    result
}

fn normalize_entity_lookup(input: &str) -> String {
    input
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, Clone)]
struct RankedEntityMatch {
    id: Uuid,
    title: String,
    score: i64,
    exact: bool,
}

fn rank_entity_matches(items: &[Value], lookup: &str) -> Vec<RankedEntityMatch> {
    let raw = lookup.trim();
    let normalized = normalize_entity_lookup(raw);
    if raw.is_empty() {
        return Vec::new();
    }

    let mut ranked = Vec::new();
    for item in items {
        let id = item
            .get("id")
            .and_then(|v| v.as_str())
            .and_then(|value| Uuid::parse_str(value).ok());
        let Some(id) = id else { continue };

        let title = entity_display_title(item);
        let title_norm = normalize_entity_lookup(&title);
        let mut score = 0i64;
        let mut exact = false;

        if id.to_string().eq_ignore_ascii_case(raw) {
            score = 10_000;
            exact = true;
        } else if !normalized.is_empty() && title_norm == normalized {
            score = 9_000;
            exact = true;
        } else if !normalized.is_empty() && title_norm.contains(&normalized) {
            score = 7_200;
        } else if !normalized.is_empty()
            && normalized.contains(&title_norm)
            && title_norm.len() >= 8
        {
            score = 6_500;
        } else if !normalized.is_empty() {
            let terms: Vec<&str> = normalized.split_whitespace().collect();
            let matched = terms
                .iter()
                .filter(|term| title_norm.contains(**term))
                .count();
            if matched > 0 {
                score = 2_500 + (matched as i64 * 140);
                if matched == terms.len() && !terms.is_empty() {
                    score += 600;
                }
            }
        }

        if score > 0 {
            ranked.push(RankedEntityMatch {
                id,
                title,
                score,
                exact,
            });
        }
    }

    ranked.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.title.cmp(&b.title)));
    ranked
}

fn format_entity_match_disambiguation(
    kind: &str,
    lookup: &str,
    matches: &[RankedEntityMatch],
) -> String {
    let mut text = format!(
        "Multiple {} match \"{}\". Please retry with an explicit ID:\n\n",
        plural_kind(kind),
        lookup
    );
    for (idx, item) in matches.iter().take(5).enumerate() {
        text.push_str(&format!(
            "{}. **{}** (id: {})\n",
            idx + 1,
            item.title,
            item.id
        ));
    }
    text
}

/// One-line summary of a single entity for use in the LLM-visible text
/// content of get / create / update responses. Produces something like
/// `**Fix replication lag** (id: abc123) [bug] — open` so even clients that
/// drop or mis-render the structured payload (e.g. JS clients that
/// `String(obj)` it into "[object Object]") still surface the meaningful
/// fields. Falls back gracefully when fields are missing.
fn format_entity_summary(kind: &str, result: &Value) -> String {
    let item = extract_entity_object(kind, result);

    let id = item
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            item.get("id")
                .and_then(|v| v.as_i64())
                .map(|n| n.to_string())
        });
    let title = entity_display_title(item);
    let has_title = title != "(no title)";

    let mut summary = if has_title {
        format!("**{}**", title)
    } else {
        String::new()
    };
    if let Some(id) = id {
        if has_title {
            summary.push_str(&format!(" (id: {})", id));
        } else {
            summary.push_str(&format!("id: {}", id));
        }
    }
    if let Some(t) = entity_type_label(item) {
        if !summary.is_empty() {
            summary.push(' ');
        }
        summary.push_str(&format!("[{}]", t));
    }
    if let Some(s) = item.get("status").and_then(|v| v.as_str()) {
        if !summary.is_empty() {
            summary.push_str(" — ");
        }
        summary.push_str(s);
    }
    if kind == "ticket" {
        return append_ticket_extras(&summary, item);
    }
    summary
}

/// Render the list response as a numbered, human-readable summary so the
/// LLM-visible text content carries enough info to act on the entities (id +
/// title + kind + status). Without this the structured payload alone is
/// invisible to clients that only surface text content.
fn format_entity_list(kind: &str, result: &Value) -> String {
    let items = extract_items_array(result);
    let count = items.map(|a| a.len()).unwrap_or(0);

    if count == 0 {
        return format!("No {} found.", plural_kind(kind));
    }

    let mut text = format!("Found {} {}:\n\n", count, plural_kind(kind));
    if let Some(arr) = items {
        for (i, item) in arr.iter().enumerate() {
            let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("—");
            let title = entity_display_title(item);
            let mut line = format!("{}. **{}** (id: {})", i + 1, title, id);
            if let Some(t) = entity_type_label(item) {
                line.push_str(&format!(" [{}]", t));
            }
            if let Some(s) = item.get("status").and_then(|v| v.as_str()) {
                line.push_str(&format!(" — {}", s));
            }
            if kind == "ticket" {
                line = append_ticket_extras(&line, item);
            }
            text.push_str(&line);
            text.push('\n');
        }
    }
    text
}

#[async_trait]
impl ToolHandler for EntityTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let input: EntityInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;

        let kind = input.kind.to_lowercase();
        let action = input.action.to_lowercase();

        // Up-front kind validation so we fail fast with a useful error
        // (instead of leaking the URL-builder error from the client).
        if entity_kind_to_path(&kind).is_none() {
            return Err(Error::Validation(format!(
                "Unknown entity kind: '{}'. Valid kinds: {}",
                kind,
                VALID_ENTITY_KINDS.join(", ")
            )));
        }

        match action.as_str() {
            "list" => {
                let (ws, proj) = self
                    .resolve_scope(&input.workspace_id, &input.project_id)
                    .await;
                let params = Self::build_list_params(&input.query, &ws, &proj);
                let result = if let (Some(cache_kind), Some(ws_id)) = (
                    Self::warm_cache_kind(&kind),
                    ws.as_ref().and_then(|s| uuid::Uuid::parse_str(s).ok()),
                ) {
                    let filter_str = serde_json::to_string(&params).unwrap_or_default();
                    let user_scope = super::atlas_warm_cache::current_user_scope_token();
                    let project_id = proj.as_ref().and_then(|p| uuid::Uuid::parse_str(p).ok());
                    let scope_hash = super::atlas_warm_cache::scope_hash_for_list(
                        ws_id,
                        user_scope.as_deref(),
                        project_id,
                        &kind,
                        Some(&filter_str),
                    );
                    let client = self.client.clone();
                    let kind_owned = kind.clone();
                    super::atlas_warm_cache::fetch_or_cache(
                        &self.atlas_layer,
                        cache_kind,
                        Some(ws_id),
                        user_scope.as_deref(),
                        project_id,
                        scope_hash,
                        150,
                        || async move { client.entity_list(&kind_owned, params).await },
                    )
                    .await?
                } else {
                    self.client.entity_list(&kind, params).await?
                };
                let text = format_entity_list(&kind, &result);
                Ok(ToolResult::with_structured(text, result))
            }

            "get" => {
                let (ws, proj) = self
                    .resolve_scope(&input.workspace_id, &input.project_id)
                    .await;
                let lookup = input
                    .id
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| Error::Validation("id is required".to_string()))?
                    .to_string();
                let (id, resolution_note) = self
                    .resolve_entity_id_for_action(&kind, &lookup, &ws, &proj)
                    .await?;
                let workspace_id = ws.as_ref().and_then(|s| uuid::Uuid::parse_str(s).ok());
                let project_id = proj.as_ref().and_then(|p| uuid::Uuid::parse_str(p).ok());
                let result = if let (Some(cache_kind), Some(workspace_id)) =
                    (Self::warm_cache_kind(&kind), workspace_id)
                {
                    let user_scope = super::atlas_warm_cache::current_user_scope_token();
                    let scope_hash = super::atlas_warm_cache::scope_hash_for_list(
                        workspace_id,
                        user_scope.as_deref(),
                        project_id,
                        &kind,
                        Some(&id.to_string()),
                    );
                    let client = self.client.clone();
                    let kind_owned = kind.clone();
                    super::atlas_warm_cache::fetch_or_cache(
                        &self.atlas_layer,
                        cache_kind,
                        Some(workspace_id),
                        user_scope.as_deref(),
                        project_id,
                        scope_hash,
                        150,
                        || async move { client.entity_get(&kind_owned, id).await },
                    )
                    .await?
                } else {
                    self.client.entity_get(&kind, id).await?
                };
                let summary = format_entity_summary(&kind, &result);
                let mut text = if summary.is_empty() {
                    format!("Fetched {} {}.", kind, id)
                } else {
                    format!("Fetched {}: {}", kind, summary)
                };
                if let Some(note) = resolution_note {
                    text = format!("{}\n{}", note, text);
                }
                Ok(ToolResult::with_structured(text, result))
            }

            "create" => {
                let mut body = input.body.unwrap_or_else(|| serde_json::json!({}));
                if !body.is_object() {
                    return Err(Error::Validation(
                        "body must be a JSON object for create".to_string(),
                    ));
                }
                let (ws, proj) = self
                    .resolve_scope(&input.workspace_id, &input.project_id)
                    .await;
                Self::inject_scope_into_body(&mut body, &ws, &proj);
                let state = self.session.state().await;
                apply_is_personal_to_body(
                    &mut body,
                    state.active_execution_mode,
                    state.team_context_degraded,
                );
                if kind == "ticket" {
                    normalize_ticket_body(&mut body)?;
                }
                let mut result = self.client.entity_create(&kind, body.clone()).await?;
                if kind == "ticket" {
                    enrich_ticket_result_from_request(&body, &mut result);
                }
                let summary = format_entity_summary(&kind, &result);
                let text = if summary.is_empty() {
                    format!("Created {}.", kind)
                } else {
                    format!("Created {}: {}", kind, summary)
                };
                Ok(ToolResult::with_structured(text, result))
            }

            "update" => {
                let (ws, proj) = self
                    .resolve_scope(&input.workspace_id, &input.project_id)
                    .await;
                let lookup = input
                    .id
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| Error::Validation("id is required".to_string()))?
                    .to_string();
                let (id, resolution_note) = self
                    .resolve_entity_id_for_action(&kind, &lookup, &ws, &proj)
                    .await?;
                let mut body = input.body.unwrap_or_else(|| serde_json::json!({}));
                if !body.is_object() {
                    return Err(Error::Validation(
                        "body must be a JSON object for update".to_string(),
                    ));
                }
                let state = self.session.state().await;
                apply_is_personal_to_body(
                    &mut body,
                    state.active_execution_mode,
                    state.team_context_degraded,
                );
                if kind == "ticket" {
                    normalize_ticket_body(&mut body)?;
                }
                let mut result = self.client.entity_update(&kind, id, body.clone()).await?;
                if kind == "ticket" {
                    enrich_ticket_result_from_request(&body, &mut result);
                }
                let summary = format_entity_summary(&kind, &result);
                let mut text = if summary.is_empty() {
                    format!("Updated {} {}.", kind, id)
                } else {
                    format!("Updated {}: {}", kind, summary)
                };
                if let Some(note) = resolution_note {
                    text = format!("{}\n{}", note, text);
                }
                Ok(ToolResult::with_structured(text, result))
            }

            "delete" => {
                let (ws, proj) = self
                    .resolve_scope(&input.workspace_id, &input.project_id)
                    .await;
                let lookup = input
                    .id
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| Error::Validation("id is required".to_string()))?
                    .to_string();
                let (id, resolution_note) = self
                    .resolve_entity_id_for_action(&kind, &lookup, &ws, &proj)
                    .await?;
                let result = self.client.entity_delete(&kind, id).await?;
                let mut text = format!("Deleted {} {}.", kind, id);
                if let Some(note) = resolution_note {
                    text = format!("{}\n{}", note, text);
                }
                Ok(ToolResult::with_structured(text, result))
            }

            _ => Err(Error::Validation(format!(
                "Unknown action: {}. Available actions: {}",
                action,
                VALID_ACTIONS.join(", ")
            ))),
        }
    }

    fn metadata(&self) -> &ToolMetadata {
        static METADATA: std::sync::OnceLock<ToolMetadata> = std::sync::OnceLock::new();
        METADATA.get_or_init(|| ToolMetadata {
            name: "entity".to_string(),
            title: "Structured Handoffs, Tickets, and Workflow Entities".to_string(),
            description: "Structured taxonomy entities — tickets, handoffs, incidents, \
                releases, experiments, goals, key_results, sprints, reviews, risks, \
                backlog_views. USE THIS TOOL when the user says any of: \
                'create a handoff' / 'prepare a handoff' / 'hand this over' / \
                'continue with another agent or session' / 'package context for handoff' \
                (kind=handoff, action=create, body={title, summary, scope, next_steps}; \
                add to_user_id only when known and never invent it) · \
                'create a ticket' / 'file a bug' / 'track a feature' / 'log a chore' / \
                'assign a ticket' / 'link a doc or plan to a ticket' \
                (kind=ticket, body.kind=bug|feature|task|chore|epic; \
                body.assignees=[{user_id?, email?, handle?, entity_type=human|agent, role?}]; \
                body.linked_items=[{kind=doc|diagram|plan|task|todo|handoff|runbook|capsule, id, \
                title_snapshot?, status_snapshot?, updated_at?}]) · \
                'log an incident' / 'open a sev1' (kind=incident) · \
                'track this release' / 'log a deployment' (kind=release) · \
                'start an experiment' / 'A/B test' (kind=experiment) · \
                'create an OKR' / 'new goal this quarter' (kind=goal, then kind=key_result for KRs) · \
                'plan a sprint' (kind=sprint) · 'request a review' / 'design review' (kind=review) · \
                'log a risk' / 'risk register' (kind=risk) · 'save a backlog filter' (kind=backlog_view). \
                \n\nDISTINCT FROM (don't use entity for these):\n\
                · memory(action=create_task) — a lightweight project-tracking todo with \
                  priority/status. NOT a 'ticket' (which is a structured entity with kind, \
                  status timeline, assignees, links).\n\
                · memory(action=create_doc, doc_type=runbook|adr|rfc|postmortem|...) — \
                  a versioned markdown document. A 'runbook' is a doc, NOT a handoff.\n\
                · session(action=capture, event_type=...) — append-only timeline events \
                  (decisions, lessons, notes). NOT a structured entity.\n\
                · capsule(...) — portable context bundle for handoff to ANOTHER agent. \
                  A generic handoff always creates entity(kind=handoff); additionally call \
                  capsule when the user requests a portable bundle, capsule, or share link.\n\
                · HANDOFF.md / a scratch prompt / a prose-only response — local text is NOT \
                  the canonical handoff and must not replace entity(kind=handoff). If the user \
                  explicitly requests a local file, create the entity first and treat the file \
                  only as an additional artifact.\n\n\
                Actions: list | get | create | update | delete. Body is free-form JSON \
                forwarded to the API; see the API schema for per-kind fields. Defaults \
                workspace_id/project_id to the active session scope when omitted."
                .to_string(),
            category: ToolCategory::Memory,
            annotations: ToolAnnotations::destructive(),
            is_pro: false,
            required_tier: None,
        })
    }

    fn input_schema(&self) -> Value {
        SchemaBuilder::new()
            .description(
                "CRUD across structured taxonomy entities. Use this when the user says \
                'create/prepare a handoff, hand this over, continue with another agent, or \
                create a ticket/bug/incident/release/experiment/goal/sprint/review/risk' \
                — pick the matching `kind`. Distinct from memory(create_task) (todo, not ticket), \
                memory(create_doc, doc_type=runbook) (doc, not handoff), and \
                session(capture) (timeline event, not entity).",
            )
            .string_enum(
                "kind",
                "Entity kind: ticket (bug/feature/task/chore/epic — see body.kind), \
                 handoff (canonical durable agent/session handoff; capsule is an optional \
                 additional portable artifact), \
                 incident (sev1-4, status timeline), release (versioned deploy), \
                 experiment (A/B test), goal (OKR objective; child key_results), \
                 key_result (measurable child of a goal), sprint (iteration), \
                 review (PR/code/design/security/architecture review), \
                 risk (active risk register), backlog_view (saved filter), \
                 coordination (durable cross-workspace knowledge item).",
                VALID_ENTITY_KINDS,
                true,
            )
            .string_enum("action", "Action to perform", VALID_ACTIONS, true)
            .string(
                "id",
                "Entity ID or lookup text (title/name/objective/version) for get / update / delete",
                false,
            )
            .uuid(
                "workspace_id",
                "Workspace ID. Defaults to the active workspace.",
                false,
            )
            .uuid(
                "project_id",
                "Project ID. Defaults to the active project.",
                false,
            )
            .object(
                "body",
                "JSON body for create / update. Free-form; see API schema per kind. \
                 For kind=handoff create: title, summary, scope, and next_steps preserve the \
                 handoff; to_user_id is optional and must be omitted rather than invented. \
                 For kind=ticket: assignees (hybrid user_id/email/handle, entity_type human|agent) \
                 and linked_items (indexed refs: kind+id+title_snapshot+status_snapshot+updated_at; \
                 no URLs). Linked kinds: doc, diagram, plan, task, todo, handoff, runbook, capsule.",
                false,
            )
            .object(
                "query",
                "Filter params for list. Object whose keys/values become URL query params.",
                false,
            )
            .build()
    }
}

fn plural_kind(kind: &str) -> String {
    match kind {
        "ticket" => "tickets".to_string(),
        "handoff" => "handoffs".to_string(),
        "backlog_view" => "backlog views".to_string(),
        "incident" => "incidents".to_string(),
        "release" => "releases".to_string(),
        "experiment" => "experiments".to_string(),
        "goal" => "goals".to_string(),
        "key_result" => "key results".to_string(),
        "sprint" => "sprints".to_string(),
        "review" => "reviews".to_string(),
        "risk" => "risks".to_string(),
        other => format!("{}s", other),
    }
}

/// Register the entity tool with the registry.
pub fn register_entity_tools(
    registry: &mut crate::registry::ToolRegistry,
    client: ContextStreamClient,
    session: Arc<SessionManager>,
) {
    let atlas_layer = registry.atlas_layer().clone();
    registry.register(
        "entity",
        Arc::new(EntityTool::with_atlas(client, session, atlas_layer)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_types::Config;

    #[test]
    fn metadata_description_carries_natural_language_trigger_phrases() {
        // Regression guard. AI agents (Claude Code, Cursor, opencode, etc.)
        // pick which tool to call by reading tool descriptions surfaced in
        // tools/list. Without explicit phrase mappings here, agents
        // routinely confuse `entity` with `memory(create_task)` /
        // `session(capture)` / `memory(create_doc)`. This test pins the
        // disambiguating language we depend on so future tightening of
        // the description doesn't silently regress the discoverability.
        //
        // We intentionally don't pin every keyword — just the ones that
        // most often confuse agents. Dropping any of these has been
        // observed to break the routing.
        let session = std::sync::Arc::new(mcp_session::SessionManager::new(
            ContextStreamClient::new(Config::default()),
            Config::default(),
        ));
        let tool = EntityTool::new(ContextStreamClient::new(Config::default()), session);
        let desc = tool.metadata().description.as_str();

        // Trigger phrases per kind.
        for phrase in [
            "create a handoff",
            "prepare a handoff",
            "hand this over",
            "continue with another agent",
            "create a ticket",
            "file a bug",
            "log an incident",
            "track this release",
            "start an experiment",
            "create an OKR",
            "log a risk",
            "design review",
            "plan a sprint",
        ] {
            assert!(
                desc.to_lowercase().contains(&phrase.to_lowercase()),
                "entity description must contain trigger phrase '{}' so agents map it to entity tool",
                phrase
            );
        }

        // Cross-tool disambiguation.
        for phrase in [
            "memory(action=create_task)",
            "memory(action=create_doc",
            "session(action=capture",
            "HANDOFF.md",
            "entity(kind=handoff)",
        ] {
            assert!(
                desc.contains(phrase),
                "entity description must distinguish itself from '{}'",
                phrase
            );
        }

        assert!(desc.contains("add to_user_id only when known"));
        assert!(desc.contains("never invent it"));
        assert!(desc.contains("local text is NOT the canonical handoff"));
        assert!(desc.contains("additionally call capsule"));
    }

    #[test]
    fn flattens_query_into_url_params() {
        let query = serde_json::json!({
            "status": "open",
            "kind": "bug",
            "include_closed": false,
            "page": 1,
            "ignored_array": ["a", "b"],
            "ignored_null": null,
        });
        let params = EntityTool::build_list_params(&Some(query), &None, &None);
        let lookup: std::collections::HashMap<String, String> = params.into_iter().collect();
        assert_eq!(lookup.get("status").map(String::as_str), Some("open"));
        assert_eq!(lookup.get("kind").map(String::as_str), Some("bug"));
        assert_eq!(
            lookup.get("include_closed").map(String::as_str),
            Some("false")
        );
        assert_eq!(lookup.get("page").map(String::as_str), Some("1"));
        // Arrays/nulls are skipped.
        assert!(!lookup.contains_key("ignored_array"));
        assert!(!lookup.contains_key("ignored_null"));
    }

    #[test]
    fn injects_scope_when_absent_and_preserves_when_present() {
        let mut body = serde_json::json!({"title": "Fix bug"});
        EntityTool::inject_scope_into_body(
            &mut body,
            &Some("11111111-1111-1111-1111-111111111111".to_string()),
            &Some("22222222-2222-2222-2222-222222222222".to_string()),
        );
        assert_eq!(body["workspace_id"], "11111111-1111-1111-1111-111111111111");
        assert_eq!(body["project_id"], "22222222-2222-2222-2222-222222222222");

        // Pre-existing scope is preserved.
        let mut body2 = serde_json::json!({
            "title": "Fix bug",
            "workspace_id": "33333333-3333-3333-3333-333333333333"
        });
        EntityTool::inject_scope_into_body(
            &mut body2,
            &Some("11111111-1111-1111-1111-111111111111".to_string()),
            &None,
        );
        assert_eq!(
            body2["workspace_id"],
            "33333333-3333-3333-3333-333333333333"
        );
    }

    #[test]
    fn extract_items_array_unwraps_common_response_shapes() {
        let bare_array = serde_json::json!([{}, {}, {}]);
        assert_eq!(extract_items_array(&bare_array).map(|a| a.len()), Some(3));

        let wrapped = serde_json::json!({"items": [{}, {}], "total": 2});
        assert_eq!(extract_items_array(&wrapped).map(|a| a.len()), Some(2));

        let nested = serde_json::json!({"data": {"items": [{}], "total": 1}});
        assert_eq!(extract_items_array(&nested).map(|a| a.len()), Some(1));

        let empty = serde_json::json!({});
        assert!(extract_items_array(&empty).is_none());
    }

    #[test]
    fn pluralization_uses_natural_forms() {
        assert_eq!(plural_kind("ticket"), "tickets");
        assert_eq!(plural_kind("backlog_view"), "backlog views");
        assert_eq!(plural_kind("key_result"), "key results");
        // Unknown kinds get naive 's' suffix.
        assert_eq!(plural_kind("widget"), "widgets");
    }

    #[test]
    fn format_entity_list_renders_id_title_kind_status_so_llm_can_act() {
        // Regression guard for the bug where list responses surfaced only
        // "Found N tickets." in the text content. Many MCP clients only show
        // text to the LLM, so without these fields the agent had no way to
        // pick a ticket to act on without a separate `get` call per id —
        // which it couldn't do because the ids were also hidden.
        let result = serde_json::json!({
            "items": [
                {"id": "11111111-1111-1111-1111-111111111111", "title": "Fix replication lag", "kind": "bug", "status": "open"},
                {"id": "22222222-2222-2222-2222-222222222222", "title": "Add OIDC", "kind": "feature", "status": "in_progress"},
            ],
            "total": 2
        });
        let text = format_entity_list("ticket", &result);
        assert!(text.starts_with("Found 2 tickets:"));
        assert!(text.contains("Fix replication lag"));
        assert!(text.contains("11111111-1111-1111-1111-111111111111"));
        assert!(text.contains("[bug]"));
        assert!(text.contains("— open"));
        assert!(text.contains("Add OIDC"));
        assert!(text.contains("[feature]"));
    }

    #[test]
    fn format_entity_list_handles_per_kind_title_fields() {
        // Goals use `objective`, releases use `version`, sprints use `name`.
        // Without per-kind title fallbacks the formatter would print
        // "(no title)" for everything except tickets — a silent regression
        // that's hard to notice in tests that only exercise tickets.
        let goals = serde_json::json!([
            {"id": "aaaa", "objective": "Ship 100 customers", "status": "active"}
        ]);
        let goal_text = format_entity_list("goal", &goals);
        assert!(goal_text.contains("Ship 100 customers"));

        let releases = serde_json::json!([
            {"id": "bbbb", "version": "1.4.0", "status": "released"}
        ]);
        let release_text = format_entity_list("release", &releases);
        assert!(release_text.contains("1.4.0"));

        let sprints = serde_json::json!([
            {"id": "cccc", "name": "Sprint 42", "status": "active"}
        ]);
        let sprint_text = format_entity_list("sprint", &sprints);
        assert!(sprint_text.contains("Sprint 42"));
    }

    #[test]
    fn format_entity_list_uses_severity_or_impact_when_kind_absent() {
        // Incidents have `severity` rather than `kind`; risks have `impact`.
        // The bracketed type label should fall through to whichever field is
        // populated so the LLM can sort/filter by it.
        let incidents = serde_json::json!([
            {"id": "abc", "title": "API down", "severity": "sev1", "status": "investigating"}
        ]);
        let text = format_entity_list("incident", &incidents);
        assert!(text.contains("[sev1]"));

        let risks = serde_json::json!([
            {"id": "def", "title": "Single-region database", "impact": "severe", "status": "open"}
        ]);
        let risk_text = format_entity_list("risk", &risks);
        assert!(risk_text.contains("[severe]"));
    }

    #[test]
    fn format_entity_list_says_none_when_empty() {
        let empty_arr = serde_json::json!([]);
        assert_eq!(
            format_entity_list("ticket", &empty_arr),
            "No tickets found."
        );

        let empty_wrapped = serde_json::json!({"items": [], "total": 0});
        assert_eq!(
            format_entity_list("incident", &empty_wrapped),
            "No incidents found."
        );
    }

    #[test]
    fn format_entity_summary_carries_meaning_in_text_content() {
        // The structured payload is a JS object that some clients render via
        // `String(obj)` → "[object Object]". Make sure the text content
        // alone communicates what the user needs to know about the entity,
        // so the agent never depends on structured_content rendering well.
        let ticket = serde_json::json!({
            "id": "11111111-1111-1111-1111-111111111111",
            "title": "Fix replication lag",
            "kind": "bug",
            "status": "open"
        });
        let summary = format_entity_summary("ticket", &ticket);
        assert!(summary.contains("Fix replication lag"));
        assert!(summary.contains("11111111-1111-1111-1111-111111111111"));
        assert!(summary.contains("[bug]"));
        assert!(summary.contains("— open"));
    }

    #[test]
    fn format_entity_summary_unwraps_kind_keyed_envelope() {
        // Some endpoints wrap as `{"ticket": {...}}` or `{"data": {...}}`.
        // Without unwrapping, the summary would print "(no title)" because
        // the top-level object has no `title` field — defeating the
        // purpose of the helper.
        let wrapped = serde_json::json!({
            "ticket": {
                "id": "abc",
                "title": "Wrapped",
                "kind": "feature",
                "status": "in_progress"
            }
        });
        let summary = format_entity_summary("ticket", &wrapped);
        assert!(summary.contains("Wrapped"));
        assert!(summary.contains("[feature]"));

        let data_wrapped = serde_json::json!({
            "data": {
                "id": "def",
                "title": "Data wrapped",
                "kind": "task",
                "status": "open"
            }
        });
        let summary2 = format_entity_summary("ticket", &data_wrapped);
        assert!(summary2.contains("Data wrapped"));
    }

    #[test]
    fn format_entity_summary_handles_per_kind_title_field() {
        // Goals/releases/sprints use different title field names; the
        // summary needs to find them so the LLM-visible text is meaningful
        // regardless of entity kind.
        let goal = serde_json::json!({
            "id": "g1",
            "objective": "Ship 100 customers",
            "status": "active"
        });
        assert!(format_entity_summary("goal", &goal).contains("Ship 100 customers"));

        let release = serde_json::json!({
            "id": "r1",
            "version": "1.4.0",
            "status": "released"
        });
        assert!(format_entity_summary("release", &release).contains("1.4.0"));
    }

    #[test]
    fn format_entity_summary_returns_empty_when_no_useful_fields() {
        // If the API gave us nothing actionable, return an empty string so
        // the caller falls back to the kind+id-only text. Avoids printing
        // "(no title)" inside an otherwise-meaningful "Created ticket: ..."
        // message.
        let bare = serde_json::json!({});
        assert_eq!(format_entity_summary("ticket", &bare), "");
    }

    #[test]
    fn format_entity_list_handles_nested_data_envelope() {
        let nested = serde_json::json!({
            "data": {
                "items": [{"id": "x", "title": "Wrapped item", "status": "open"}],
                "total": 1
            }
        });
        let text = format_entity_list("ticket", &nested);
        assert!(text.starts_with("Found 1 tickets:"));
        assert!(text.contains("Wrapped item"));
    }

    #[test]
    fn ticket_summary_includes_assignees_and_linked_items() {
        let ticket = serde_json::json!({
            "id": "11111111-1111-1111-1111-111111111111",
            "title": "Fix replication lag",
            "kind": "bug",
            "status": "open",
            "assignees": [
                {"email": "alice@example.com", "role": "owner"},
                {"handle": "triage-bot", "entity_type": "agent"}
            ],
            "linked_items": [
                {"kind": "runbook", "id": "doc-1", "title_snapshot": "Replication runbook", "updated_at": "2026-05-19T00:00:00Z"},
                {"kind": "plan", "id": "plan-1", "title_snapshot": "Q2 rollout"}
            ]
        });
        let summary = format_entity_summary("ticket", &ticket);
        assert!(summary.contains("assignees:"));
        assert!(summary.contains("alice@example.com"));
        assert!(summary.contains("[agent]"));
        assert!(summary.contains("linked: 2"));
        assert!(summary.contains("runbook=1"));
        assert!(summary.contains("plan=1"));
    }

    #[test]
    fn ticket_list_renders_assignment_and_link_summary() {
        let result = serde_json::json!({
            "items": [{
                "id": "11111111-1111-1111-1111-111111111111",
                "title": "Ship team assignment",
                "kind": "feature",
                "status": "open",
                "assignees": [{"email": "bob@example.com"}],
                "linked_items": [{"kind": "task", "id": "task-1", "title_snapshot": "Implement API"}]
            }]
        });
        let text = format_entity_list("ticket", &result);
        assert!(text.contains("assignees:"));
        assert!(text.contains("bob@example.com"));
        assert!(text.contains("linked: 1"));
    }

    #[test]
    fn normalize_ticket_body_rejects_invalid_linked_kind() {
        let mut body = serde_json::json!({
            "title": "Bad link",
            "linked_items": [{"kind": "unknown_kind", "id": "x"}]
        });
        let err = normalize_ticket_body(&mut body).unwrap_err().to_string();
        assert!(err.contains("invalid"));
    }

    #[test]
    fn normalize_ticket_body_requires_assignee_identity() {
        let mut body = serde_json::json!({
            "assignees": [{"role": "owner"}]
        });
        let err = normalize_ticket_body(&mut body).unwrap_err().to_string();
        assert!(err.contains("user_id, email, or handle"));
    }

    #[test]
    fn non_ticket_entities_unaffected_by_ticket_extras() {
        let incident = serde_json::json!({
            "id": "abc",
            "title": "API down",
            "severity": "sev1",
            "status": "investigating"
        });
        let summary = format_entity_summary("incident", &incident);
        assert!(!summary.contains("assignees:"));
        assert!(!summary.contains("linked:"));
    }
}
