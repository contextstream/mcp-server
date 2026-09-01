//! Reminder domain tools: list, active, create, snooze, complete, dismiss.

use async_trait::async_trait;
use mcp_client::{ContextStreamClient, CreateReminderParams, ListRemindersParams};
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

// ============================================================================
// Valid Constants
// ============================================================================

/// Valid actions.
const VALID_ACTIONS: &[&str] = &["list", "active", "create", "snooze", "complete", "dismiss"];

/// Valid statuses.
const VALID_STATUSES: &[&str] = &["pending", "completed", "dismissed", "snoozed"];

/// Valid priorities.
const VALID_PRIORITIES: &[&str] = &["low", "normal", "high", "urgent"];

/// Valid recurrence patterns.
const VALID_RECURRENCE: &[&str] = &["daily", "weekly", "monthly"];

// ============================================================================
// Unified Reminder Tool
// ============================================================================

/// Input for the unified reminder tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReminderInput {
    pub action: String,
    // Common fields
    pub workspace_id: Option<String>,
    pub project_id: Option<String>,
    pub limit: Option<i64>,
    // List/active fields
    pub status: Option<String>,
    pub context: Option<String>,
    // Create fields
    pub title: Option<String>,
    pub content: Option<String>,
    pub remind_at: Option<String>,
    pub priority: Option<String>,
    pub recurrence: Option<String>,
    pub keywords: Option<Vec<String>>,
    // Snooze/complete/dismiss fields
    pub reminder_id: Option<String>,
    pub until: Option<String>,
}

/// Unified reminder tool handler.
pub struct ReminderTool {
    client: ContextStreamClient,
}

impl ReminderTool {
    pub fn new(client: ContextStreamClient) -> Self {
        Self { client }
    }

    fn parse_workspace_id(input: &Option<String>) -> Result<Option<Uuid>> {
        match input {
            Some(s) => {
                Ok(Some(Uuid::parse_str(s).map_err(|_| {
                    Error::Validation("Invalid workspace_id".to_string())
                })?))
            }
            None => Ok(None),
        }
    }

    fn parse_project_id(input: &Option<String>) -> Result<Option<Uuid>> {
        match input {
            Some(s) => {
                Ok(Some(Uuid::parse_str(s).map_err(|_| {
                    Error::Validation("Invalid project_id".to_string())
                })?))
            }
            None => Ok(None),
        }
    }

    fn parse_reminder_id(input: &Option<String>) -> Result<Uuid> {
        match input {
            Some(s) => {
                Uuid::parse_str(s).map_err(|_| Error::Validation("Invalid reminder_id".to_string()))
            }
            None => Err(Error::Validation("reminder_id is required".to_string())),
        }
    }
}

#[async_trait]
impl ToolHandler for ReminderTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let input: ReminderInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;

        let action = input.action.to_lowercase();
        let workspace_id = Self::parse_workspace_id(&input.workspace_id)?;
        let project_id = Self::parse_project_id(&input.project_id)?;

        match action.as_str() {
            "list" => {
                let params = ListRemindersParams {
                    workspace_id,
                    project_id,
                    status: input.status,
                    context: input.context,
                    limit: input.limit,
                };
                let result = self.client.list_reminders(params).await?;
                Ok(ToolResult::with_structured(
                    format_reminder_list(&result, false),
                    result,
                ))
            }

            "active" => {
                let result = self
                    .client
                    .active_reminders(
                        workspace_id,
                        project_id,
                        input.context.as_deref(),
                        input.limit,
                    )
                    .await?;
                Ok(ToolResult::with_structured(
                    format_reminder_list(&result, true),
                    result,
                ))
            }

            "create" => {
                let title = input
                    .title
                    .ok_or_else(|| Error::Validation("title is required for create".to_string()))?;
                let remind_at = input.remind_at.ok_or_else(|| {
                    Error::Validation("remind_at is required for create".to_string())
                })?;

                let params = CreateReminderParams {
                    title,
                    content: input.content,
                    remind_at,
                    priority: input.priority,
                    recurrence: input.recurrence,
                    keywords: input.keywords,
                    workspace_id,
                    project_id,
                };
                let result = self.client.create_reminder(params).await?;
                Ok(ToolResult::with_structured(
                    format_reminder_created(&result),
                    result,
                ))
            }

            "snooze" => {
                let reminder_id = Self::parse_reminder_id(&input.reminder_id)?;
                let until = input
                    .until
                    .ok_or_else(|| Error::Validation("until is required for snooze".to_string()))?;
                let result = self.client.snooze_reminder(reminder_id, &until).await?;
                Ok(ToolResult::with_structured(
                    format!("Reminder {} snoozed until {}.", reminder_id, until),
                    result,
                ))
            }

            "complete" => {
                let reminder_id = Self::parse_reminder_id(&input.reminder_id)?;
                let result = self.client.complete_reminder(reminder_id).await?;
                Ok(ToolResult::with_structured(
                    format!("Reminder {} completed.", reminder_id),
                    result,
                ))
            }

            "dismiss" => {
                let reminder_id = Self::parse_reminder_id(&input.reminder_id)?;
                let result = self.client.dismiss_reminder(reminder_id).await?;
                Ok(ToolResult::with_structured(
                    format!("Reminder {} dismissed.", reminder_id),
                    result,
                ))
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
            name: "reminder".to_string(),
            title: "Reminder Operations".to_string(),
            description: "Reminder management. Actions: list (all reminders), active (pending/overdue), create (new reminder), snooze, complete, dismiss.".to_string(),
            category: ToolCategory::Reminders,
            annotations: ToolAnnotations::destructive(),
            is_pro: false,
            required_tier: None,
        })
    }

    fn input_schema(&self) -> Value {
        SchemaBuilder::new()
            .description("Reminder operations")
            .string_enum("action", "Action to perform", VALID_ACTIONS, true)
            // Common fields
            .uuid("workspace_id", "Workspace ID", false)
            .uuid("project_id", "Project ID", false)
            .integer("limit", "Maximum results", false)
            // List/active fields
            .string_enum("status", "Reminder status filter", VALID_STATUSES, false)
            .string("context", "Context to match relevant reminders", false)
            // Create fields
            .string("title", "Reminder title", false)
            .string("content", "Reminder content/description", false)
            .string("remind_at", "When to remind (ISO 8601 datetime)", false)
            .string_enum("priority", "Reminder priority", VALID_PRIORITIES, false)
            .string_enum("recurrence", "Recurrence pattern", VALID_RECURRENCE, false)
            .array("keywords", "Keywords for matching", "string", false)
            // Snooze/complete/dismiss fields
            .uuid(
                "reminder_id",
                "Reminder ID (for snooze/complete/dismiss)",
                false,
            )
            .string("until", "Snooze until (ISO 8601 datetime)", false)
            .build()
    }
}

// ============================================================================
// Rendering helpers
//
// Many MCP clients drop/mis-render structured_content, so the human-readable
// text MUST carry the reminder id + key fields — otherwise created reminders
// can never be targeted by snooze/complete/dismiss (which require reminder_id).
// ============================================================================

/// Pull a non-empty string field from a reminder JSON object.
fn reminder_field<'a>(obj: &'a Value, key: &str) -> Option<&'a str> {
    obj.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
}

/// Render a single reminder object as a one-line summary including its id.
fn format_reminder_object(obj: &Value) -> String {
    let title = reminder_field(obj, "title")
        .or_else(|| reminder_field(obj, "content"))
        .unwrap_or("(untitled)");
    let mut line = format!("**{}**", title);
    if let Some(id) = reminder_field(obj, "id").or_else(|| reminder_field(obj, "reminder_id")) {
        line.push_str(&format!(" (id: {})", id));
    }
    let mut meta: Vec<String> = Vec::new();
    if let Some(status) = reminder_field(obj, "status") {
        meta.push(status.to_string());
    }
    if let Some(remind_at) = reminder_field(obj, "remind_at") {
        meta.push(format!("due {}", remind_at));
    }
    if let Some(priority) = reminder_field(obj, "priority") {
        meta.push(format!("[{}]", priority));
    }
    if !meta.is_empty() {
        line.push_str(&format!(" — {}", meta.join(", ")));
    }
    line
}

/// Unwrap a single reminder object from common envelopes.
fn reminder_object(result: &Value) -> &Value {
    result
        .get("reminder")
        .or_else(|| result.get("data"))
        .unwrap_or(result)
}

/// Extract the reminder array from common envelopes (bare array / {items|reminders|data}).
fn reminder_items(result: &Value) -> Vec<&Value> {
    if let Some(arr) = result.as_array() {
        return arr.iter().collect();
    }
    for key in ["items", "reminders", "data"] {
        if let Some(arr) = result.get(key).and_then(|v| v.as_array()) {
            return arr.iter().collect();
        }
    }
    Vec::new()
}

/// Render a reminder list/active result as numbered lines, each carrying its id.
fn format_reminder_list(result: &Value, active: bool) -> String {
    let items = reminder_items(result);
    if items.is_empty() {
        return if active {
            "No active reminders.".to_string()
        } else {
            "No reminders found.".to_string()
        };
    }
    let mut out = format!(
        "Found {} {}reminder(s):\n\n",
        items.len(),
        if active { "active " } else { "" }
    );
    for (i, obj) in items.iter().enumerate() {
        out.push_str(&format!("{}. {}\n", i + 1, format_reminder_object(obj)));
    }
    out.trim_end().to_string()
}

/// Render a create result, surfacing the new reminder's id.
fn format_reminder_created(result: &Value) -> String {
    format!(
        "Reminder created: {}",
        format_reminder_object(reminder_object(result))
    )
}

/// Register all reminder tools.
pub fn register_reminder_tools(
    registry: &mut crate::registry::ToolRegistry,
    client: ContextStreamClient,
) {
    registry.register("reminder", Arc::new(ReminderTool::new(client)));
}

#[cfg(test)]
#[path = "reminder_tests.rs"]
mod tests;
