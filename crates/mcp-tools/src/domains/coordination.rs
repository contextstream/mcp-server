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
    "check_in", "inbox", "list", "get", "share", "ack", "dismiss", "settings",
];

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
                let mut body = serde_json::json!({
                    "title": title,
                    "summary": input.summary,
                    "why_it_matters": input.why_it_matters,
                    "kind": input.kind.unwrap_or_else(|| "knowledge".into()),
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
                dismiss, settings. context() and init() already heartbeat presence. \
                When [COORDINATION] appears, read it before continuing and ack after use."
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
            .string("id", "Coordination item id for get.", false)
            .string("notice_id", "Notice id for ack / dismiss.", false)
            .string("title", "Title when sharing an item.", false)
            .string("summary", "Short summary of the shared knowledge.", false)
            .string(
                "why_it_matters",
                "Why another workspace/project needs this.",
                false,
            )
            .string(
                "kind",
                "decision | constraint | api_contract | risk | status | knowledge",
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
    for notice in notices.iter().take(5) {
        let reason = notice
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("shared context");
        let id = notice.get("id").and_then(Value::as_str).unwrap_or_default();
        text.push_str(&format!(
            "\n[COORDINATION] {reason} — ack via coordination(action=\"ack\", notice_id=\"{id}\")"
        ));
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
