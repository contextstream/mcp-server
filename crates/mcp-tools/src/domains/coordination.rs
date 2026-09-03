//! Cross-workspace agent coordination.

use async_trait::async_trait;
use mcp_client::ContextStreamClient;
use mcp_types::{
    tool::{ToolAnnotations, ToolCategory, ToolMetadata, ToolResult},
    Error, Result,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;

use crate::registry::ToolHandler;
use crate::schema::SchemaBuilder;

const VALID_ACTIONS: &[&str] = &[
    "check_in", "inbox", "list", "get", "share", "ack", "reply", "dismiss", "settings",
];

/// Coordination item kinds accepted by `POST /coordinations`. Validated
/// client-side so a typo fails fast instead of round-tripping a 4xx.
pub const VALID_KINDS: &[&str] = &[
    "decision",
    "constraint",
    "warning",
    "insight",
    "blocker",
    "request",
    "handoff",
    "note",
];

/// Default number of `[COORDINATION]` lines rendered by `context()` /
/// `session(action="ground")` before the `… N more` trailer.
pub const NOTICE_RENDER_LIMIT: usize = 5;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoordinationInput {
    pub action: String,
    pub workspace_id: Option<String>,
    pub project_id: Option<String>,
    pub session_id: Option<String>,
    pub task_summary: Option<String>,
    pub id: Option<String>,
    pub notice_id: Option<String>,
    pub title: Option<String>,
    pub summary: Option<String>,
    pub why_it_matters: Option<String>,
    pub kind: Option<String>,
    pub target_workspace_ids: Option<Vec<String>>,
    pub target_project_ids: Option<Vec<String>>,
    pub enabled: Option<bool>,
    pub limit: Option<i64>,
    /// Reply text for action=reply.
    pub message: Option<String>,
    /// Presence metadata for check_in, e.g. {"git": {"branch", "commit"}}.
    pub metadata: Option<Value>,
}

pub struct CoordinationTool {
    client: ContextStreamClient,
}

impl CoordinationTool {
    pub fn new(client: ContextStreamClient) -> Self {
        Self { client }
    }

    fn parse_uuid(value: &Option<String>, field: &str) -> Result<Option<Uuid>> {
        match value {
            Some(s) => {
                Ok(Some(Uuid::parse_str(s).map_err(|_| {
                    Error::Validation(format!("Invalid {field}"))
                })?))
            }
            None => Ok(None),
        }
    }

    fn parse_uuid_list(values: &Option<Vec<String>>) -> Result<Vec<Uuid>> {
        let Some(values) = values else {
            return Ok(Vec::new());
        };
        values
            .iter()
            .map(|s| {
                Uuid::parse_str(s).map_err(|_| Error::Validation(format!("Invalid uuid: {s}")))
            })
            .collect()
    }
}

#[async_trait]
impl ToolHandler for CoordinationTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let input: CoordinationInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;
        let action = input.action.to_lowercase();
        let workspace_id = Self::parse_uuid(&input.workspace_id, "workspace_id")?;
        let project_id = Self::parse_uuid(&input.project_id, "project_id")?;

        match action.as_str() {
            "check_in" => {
                let session_id = input.session_id.ok_or_else(|| {
                    Error::Validation("session_id is required for check_in".into())
                })?;
                let result = self
                    .client
                    .coordination_check_in(
                        workspace_id,
                        project_id,
                        &session_id,
                        input.task_summary.as_deref(),
                        input.metadata.as_ref(),
                    )
                    .await?;
                Ok(ToolResult::with_structured(
                    format!(
                        "Coordination check-in recorded for session {session_id}. context() already heartbeats presence."
                    ),
                    result,
                ))
            }
            "inbox" | "list" => {
                let result = self
                    .client
                    .coordination_inbox(
                        workspace_id,
                        project_id,
                        input.session_id.as_deref(),
                        input.limit,
                    )
                    .await?;
                Ok(ToolResult::with_structured(format_inbox(&result), result))
            }
            "get" => {
                let id = input
                    .id
                    .as_deref()
                    .or(input.notice_id.as_deref())
                    .ok_or_else(|| Error::Validation("id is required for get".into()))?;
                let uuid = Uuid::parse_str(id)
                    .map_err(|_| Error::Validation("Invalid coordination id".into()))?;
                match self.client.get_coordination_item(uuid).await {
                    Ok(result) => Ok(ToolResult::with_structured(
                        format_item(&result, id),
                        result,
                    )),
                    Err(_) => {
                        let result = self.client.get_coordination_notice(uuid).await?;
                        Ok(ToolResult::with_structured(
                            format_notice(&result, id),
                            result,
                        ))
                    }
                }
            }
            "share" => {
                let title = input
                    .title
                    .ok_or_else(|| Error::Validation("title is required for share".into()))?;
                let kind = validate_kind(input.kind.as_deref())?;
                let mut body = serde_json::json!({
                    "title": title,
                    "summary": input.summary,
                    "why_it_matters": input.why_it_matters,
                    "kind": kind,
                    "workspace_id": workspace_id,
                    "project_id": project_id,
                    "created_by": "agent",
                });
                let targets = Self::parse_uuid_list(&input.target_workspace_ids)?;
                if !targets.is_empty() {
                    body["target_workspace_ids"] = serde_json::json!(targets);
                }
                let target_projects = Self::parse_uuid_list(&input.target_project_ids)?;
                if !target_projects.is_empty() {
                    body["target_project_ids"] = serde_json::json!(target_projects);
                }
                let result = self.client.coordination_share(body).await?;
                Ok(ToolResult::with_structured(
                    format_share(&result, &title),
                    result,
                ))
            }
            "ack" => {
                let notice_id = input
                    .notice_id
                    .or(input.id)
                    .ok_or_else(|| Error::Validation("notice_id is required for ack".into()))?;
                let uuid = Uuid::parse_str(&notice_id)
                    .map_err(|_| Error::Validation("Invalid notice_id".into()))?;
                let result = self.client.ack_coordination_notice(uuid).await?;
                Ok(ToolResult::with_structured(
                    format!("Acked coordination notice {notice_id}"),
                    result,
                ))
            }
            "reply" => {
                let notice_id = input
                    .notice_id
                    .or(input.id)
                    .ok_or_else(|| Error::Validation("notice_id is required for reply".into()))?;
                let uuid = Uuid::parse_str(&notice_id)
                    .map_err(|_| Error::Validation("Invalid notice_id".into()))?;
                let message = input
                    .message
                    .as_deref()
                    .map(str::trim)
                    .filter(|m| !m.is_empty())
                    .ok_or_else(|| Error::Validation("message is required for reply".into()))?;
                let result = self.client.reply_coordination_notice(uuid, message).await?;
                Ok(ToolResult::with_structured(
                    format_reply(&result, &notice_id),
                    result,
                ))
            }
            "dismiss" => {
                let notice_id = input
                    .notice_id
                    .or(input.id)
                    .ok_or_else(|| Error::Validation("notice_id is required for dismiss".into()))?;
                let uuid = Uuid::parse_str(&notice_id)
                    .map_err(|_| Error::Validation("Invalid notice_id".into()))?;
                let result = self.client.dismiss_coordination_notice(uuid).await?;
                Ok(ToolResult::with_structured(
                    format!("Dismissed coordination notice {notice_id}"),
                    result,
                ))
            }
            "settings" => {
                let result = if let Some(enabled) = input.enabled {
                    self.client
                        .update_coordination_settings(workspace_id, project_id, enabled)
                        .await?
                } else {
                    self.client
                        .get_coordination_settings(workspace_id, project_id)
                        .await?
                };
                Ok(ToolResult::with_structured(
                    format_settings(&result),
                    result,
                ))
            }
            _ => Err(Error::Validation(format!(
                "Unknown action: {}. Available: {}",
                action,
                VALID_ACTIONS.join(", ")
            ))),
        }
    }

    fn metadata(&self) -> &ToolMetadata {
        static METADATA: std::sync::OnceLock<ToolMetadata> = std::sync::OnceLock::new();
        METADATA.get_or_init(|| ToolMetadata {
            name: "coordination".to_string(),
            title: "Cross-workspace Agent Coordination".to_string(),
            description: "Ongoing shared awareness between live agents, plus durable \
                coordination items when knowledge from one workspace/project is needed \
                in another. Distinct from handoffs (ownership transfer) and capsules \
                (portable bundles). Actions: check_in, inbox/list, get, share, ack, \
                reply, dismiss, settings. context() and init() already heartbeat presence. \
                When [COORDINATION] appears, read it before continuing and ack after use. \
                A (blocking) notice is a direct conflict with another session: read it \
                before continuing, then ack or reply (action=reply, notice_id, message) so \
                the other session sees your answer on its next turn."
                .to_string(),
            category: ToolCategory::Memory,
            annotations: ToolAnnotations::destructive(),
            is_pro: false,
            required_tier: None,
        })
    }

    fn input_schema(&self) -> Value {
        SchemaBuilder::new()
            .description("Coordinate live agents across workspaces/projects. Not a handoff.")
            .string_enum("action", "Action to perform", VALID_ACTIONS, true)
            .uuid(
                "workspace_id",
                "Workspace ID. Defaults to active scope.",
                false,
            )
            .uuid("project_id", "Project ID. Defaults to active scope.", false)
            .string("session_id", "Live session id for check_in / inbox.", false)
            .string("task_summary", "What this agent is working on.", false)
            .object(
                "metadata",
                "Presence metadata for check_in shown to the judge as evidence, e.g. {\"git\": {\"branch\": \"...\", \"commit\": \"...\"}}.",
                false,
            )
            .string("id", "Coordination item id for get.", false)
            .string("notice_id", "Notice id for ack / reply / dismiss.", false)
            .string(
                "message",
                "Reply text for action=reply; delivered to the session that raised the notice.",
                false,
            )
            .string("title", "Title when sharing an item.", false)
            .string("summary", "Short summary of the shared knowledge.", false)
            .string(
                "why_it_matters",
                "Why another workspace/project needs this.",
                false,
            )
            .string_enum(
                "kind",
                "Kind of shared item (defaults to note).",
                VALID_KINDS,
                false,
            )
            .array(
                "target_workspace_ids",
                "Workspaces this knowledge should reach.",
                "string",
                false,
            )
            .array(
                "target_project_ids",
                "Projects this knowledge should reach.",
                "string",
                false,
            )
            .boolean(
                "enabled",
                "When set with action=settings, updates enablement.",
                false,
            )
            .build()
    }
}

pub fn inbox_payload(inbox: &Value) -> &Value {
    inbox.get("data").unwrap_or(inbox)
}

pub fn inbox_notices(inbox: &Value) -> &[Value] {
    inbox_payload(inbox)
        .get("notices")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn coordination_id(value: &Value) -> Option<&str> {
    let payload = value.get("data").unwrap_or(value);
    payload.get("id").and_then(Value::as_str)
}

/// Validate a `kind` for `share`; `None` defaults to `note`.
pub fn validate_kind(kind: Option<&str>) -> Result<String> {
    let kind = kind.map(str::trim).filter(|value| !value.is_empty());
    match kind {
        None => Ok("note".to_string()),
        Some(value) => {
            let normalized = value.to_ascii_lowercase();
            if VALID_KINDS.contains(&normalized.as_str()) {
                Ok(normalized)
            } else {
                Err(Error::Validation(format!(
                    "Invalid coordination kind '{value}'. Use one of: {}",
                    VALID_KINDS.join(", ")
                )))
            }
        }
    }
}

fn notice_project_id(notice: &Value) -> Option<Uuid> {
    ["from_project_id", "project_id", "source_project_id"]
        .iter()
        .find_map(|field| notice.get(*field).and_then(Value::as_str))
        .and_then(|raw| Uuid::parse_str(raw).ok())
}

/// Render `[COORDINATION]` lines for an inbox payload.
///
/// * At most `limit` notices are rendered; the rest collapse into a
///   `… N more` trailer (using `total_pending` when the server reports it).
/// * Notices from another project are prefixed `[other project]`.
/// * The lines only *describe* the manual ack call. Nothing here (or in any
///   caller) acks a notice automatically.
pub fn format_coordination_notices(
    inbox: &Value,
    current_project_id: Option<Uuid>,
    limit: usize,
) -> String {
    let payload = inbox_payload(inbox);
    let notices = inbox_notices(inbox);
    if notices.is_empty() {
        return String::new();
    }
    let limit = limit.max(1);
    let mut text = String::new();
    for notice in notices.iter().take(limit) {
        let reason = notice
            .get("reason")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .or_else(|| notice.get("title").and_then(Value::as_str))
            .unwrap_or("shared context");
        let id = notice.get("id").and_then(Value::as_str).unwrap_or_default();
        let other_project = match (notice_project_id(notice), current_project_id) {
            (Some(from), Some(current)) => from != current,
            (Some(_), None) => false,
            (None, _) => false,
        };
        text.push_str(&crate::notices::coordination_notice_line(
            reason,
            id,
            other_project,
            notice.get("urgency").and_then(Value::as_str),
        ));
        text.push('\n');
    }
    let shown = notices.len().min(limit);
    let total_pending = payload
        .get("total_pending")
        .and_then(Value::as_u64)
        .map(|value| value as usize);
    let truncated = payload
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let remaining = match total_pending {
        Some(total) if total > shown => total - shown,
        _ if notices.len() > shown => notices.len() - shown,
        _ if truncated => 1,
        _ => 0,
    };
    if remaining > 0 {
        text.push_str(&crate::notices::coordination_more_line(remaining));
        text.push('\n');
    }
    text
}

fn format_inbox(result: &Value) -> String {
    let data = inbox_payload(result);
    let notices = inbox_notices(result);
    let items = data
        .get("items")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);
    let mut text = format!(
        "Coordination inbox: {} notice(s), {items} item(s).",
        notices.len()
    );
    let rendered = format_coordination_notices(result, None, NOTICE_RENDER_LIMIT);
    if !rendered.is_empty() {
        text.push('\n');
        text.push_str(rendered.trim_end());
    }
    text
}

fn format_share(result: &Value, title: &str) -> String {
    match coordination_id(result) {
        Some(id) => format!("Shared coordination item {id} ({title}). This is not a handoff."),
        None => format!("Shared coordination item ({title}). This is not a handoff."),
    }
}

fn format_item(result: &Value, id: &str) -> String {
    let payload = result.get("data").unwrap_or(result);
    let title = payload.get("title").and_then(Value::as_str).unwrap_or(id);
    let kind = payload
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("item");
    format!("Coordination item {id} ({kind}): {title}")
}

fn format_reply(result: &Value, notice_id: &str) -> String {
    let payload = result.get("data").unwrap_or(result);
    let created = payload
        .get("created")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let to_session = payload
        .pointer("/notice/to_session_id")
        .and_then(Value::as_str)
        .unwrap_or("the origin session");
    if created {
        format!(
            "Replied to coordination notice {notice_id}; {to_session} sees it on its next turn. The original notice is still open — ack it when you are done."
        )
    } else {
        format!("An identical reply to coordination notice {notice_id} already exists; nothing new was sent.")
    }
}

fn format_notice(result: &Value, id: &str) -> String {
    let payload = result.get("data").unwrap_or(result);
    let reason = payload
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("shared context");
    format!("[COORDINATION] {reason} — ack via coordination(action=\"ack\", notice_id=\"{id}\")")
}

fn format_settings(result: &Value) -> String {
    let payload = result.get("data").unwrap_or(result);
    let enabled = payload.get("enabled").and_then(Value::as_bool);
    let workspace_enabled = payload.get("workspace_enabled").and_then(Value::as_bool);
    let project_enabled = payload.get("project_enabled").and_then(Value::as_bool);
    match (enabled, workspace_enabled, project_enabled) {
        (Some(enabled), Some(workspace_enabled), Some(project_enabled)) => format!(
            "Coordination settings: enabled={enabled} workspace_enabled={workspace_enabled} project_enabled={project_enabled}"
        ),
        (Some(enabled), _, _) => format!("Coordination settings: enabled={enabled}"),
        _ => "Coordination settings".to_string(),
    }
}

pub fn register_coordination_tools(
    registry: &mut crate::registry::ToolRegistry,
    client: ContextStreamClient,
) {
    registry.register("coordination", Arc::new(CoordinationTool::new(client)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn inbox_notices_read_unwrapped_client_payload() {
        let inbox = json!({
            "notices": [{"id": "n1", "reason": "API contract changed"}],
            "items": [{"id": "i1"}]
        });
        let notices = inbox_notices(&inbox);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0]["id"], "n1");
        let text = format_inbox(&inbox);
        assert!(text.contains("[COORDINATION] API contract changed"));
        assert!(text.contains("notice_id=\"n1\""));
    }

    #[test]
    fn inbox_notices_read_wrapped_api_payload() {
        let inbox = json!({
            "data": {
                "notices": [{"id": "n2", "reason": "Shared decision"}],
                "items": []
            }
        });
        assert_eq!(inbox_notices(&inbox).len(), 1);
        assert!(format_inbox(&inbox).contains("notice_id=\"n2\""));
    }

    #[test]
    fn other_project_notices_are_prefixed_and_never_auto_acked() {
        let current = Uuid::new_v4();
        let other = Uuid::new_v4();
        let inbox = json!({
            "notices": [
                {"id": "n1", "reason": "Shared decision", "from_project_id": current.to_string()},
                {"id": "n2", "reason": "Schema freeze", "from_project_id": other.to_string(), "urgency": "high"},
                {"id": "n3", "reason": "No project on notice"}
            ]
        });
        let text = format_coordination_notices(&inbox, Some(current), 5);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines[0],
            "[COORDINATION] Shared decision — ack via coordination(action=\"ack\", notice_id=\"n1\")"
        );
        assert!(lines[1].starts_with("[COORDINATION] [other project] Schema freeze (urgency=high)"));
        assert!(!lines[2].contains("[other project]"));
        // Only the manual ack call is described; no line claims an ack happened.
        assert!(!text.to_ascii_lowercase().contains("acked"));
    }

    #[test]
    fn truncation_trailer_counts_remaining_notices() {
        let notices: Vec<Value> = (0..7)
            .map(|index| json!({"id": format!("n{index}"), "reason": format!("reason {index}")}))
            .collect();
        let inbox = json!({"notices": notices, "truncated": true, "total_pending": 12});
        let text = format_coordination_notices(&inbox, None, 5);
        assert_eq!(text.matches("[COORDINATION]").count(), 6);
        assert!(text.contains("[COORDINATION] … 7 more"));

        let inbox = json!({"notices": [{"id": "a", "reason": "r"}], "truncated": true});
        let text = format_coordination_notices(&inbox, None, 5);
        assert!(text.contains("… 1 more"));

        let inbox = json!({"notices": [{"id": "a", "reason": "r"}]});
        assert!(!format_coordination_notices(&inbox, None, 5).contains("more"));
        assert!(format_coordination_notices(&json!({"notices": []}), None, 5).is_empty());
    }

    #[test]
    fn share_kind_is_validated_client_side() {
        assert_eq!(validate_kind(None).unwrap(), "note");
        assert_eq!(validate_kind(Some("Decision")).unwrap(), "decision");
        for kind in VALID_KINDS {
            assert_eq!(validate_kind(Some(kind)).unwrap(), *kind);
        }
        let err = validate_kind(Some("knowledge")).unwrap_err().to_string();
        assert!(err.contains("Invalid coordination kind"));
        assert!(err.contains("handoff"));
    }

    #[test]
    fn reply_text_names_the_origin_session_and_dedup_outcome() {
        let created = json!({"data": {"created": true, "notice": {"to_session_id": "origin-1"}, "reply_to": "n1"}});
        let text = format_reply(&created, "n1");
        assert!(text.contains("Replied to coordination notice n1"));
        assert!(text.contains("origin-1 sees it on its next turn"));
        let duplicate = json!({"created": false, "notice": {"to_session_id": "origin-1"}});
        assert!(format_reply(&duplicate, "n1").contains("already exists"));
        assert!(VALID_ACTIONS.contains(&"reply"));
    }

    #[test]
    fn share_and_settings_text_include_ids_and_flags() {
        let share = json!({"id": "item-1", "title": "Ranking"});
        assert!(format_share(&share, "Ranking").contains("item-1"));
        let settings = json!({
            "enabled": true,
            "workspace_enabled": true,
            "project_enabled": false
        });
        assert_eq!(
            format_settings(&settings),
            "Coordination settings: enabled=true workspace_enabled=true project_enabled=false"
        );
    }
}
