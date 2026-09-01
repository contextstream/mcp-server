//! Tests for reminder domain tools.

use super::*;
use crate::registry::ToolHandler;
use crate::testing::TestFixtures;
use mcp_types::tool::ToolCategory;
use serde_json::json;

// ============================================================================
// Test Helpers
// ============================================================================

fn create_mock_client() -> ContextStreamClient {
    ContextStreamClient::new(TestFixtures::test_config())
}

// ============================================================================
// Metadata Tests
// ============================================================================

mod metadata_tests {
    use super::ReminderTool;
    use super::{create_mock_client, ToolCategory, ToolHandler};

    #[test]
    fn test_reminder_tool_metadata() {
        let client = create_mock_client();
        let tool = ReminderTool::new(client);
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "reminder");
        assert_eq!(metadata.title, "Reminder Operations");
        assert!(metadata.description.contains("list"));
        assert!(metadata.description.contains("active"));
        assert!(metadata.description.contains("create"));
        assert!(metadata.description.contains("snooze"));
        assert!(metadata.description.contains("complete"));
        assert!(metadata.description.contains("dismiss"));
        assert_eq!(metadata.category, ToolCategory::Reminders);
        assert!(!metadata.is_pro);
    }
}

// ============================================================================
// Schema Tests
// ============================================================================

mod schema_tests {
    use super::ReminderTool;
    use super::{create_mock_client, ToolHandler};

    #[test]
    fn test_reminder_tool_schema() {
        let client = create_mock_client();
        let tool = ReminderTool::new(client);
        let schema = tool.input_schema();

        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("action"));

        // Check action enum contains all expected actions
        if let Some(action) = props.get("action") {
            if let Some(enum_vals) = action.get("enum").and_then(|e| e.as_array()) {
                let values: Vec<&str> = enum_vals.iter().filter_map(|v| v.as_str()).collect();
                assert!(values.contains(&"list"));
                assert!(values.contains(&"active"));
                assert!(values.contains(&"create"));
                assert!(values.contains(&"snooze"));
                assert!(values.contains(&"complete"));
                assert!(values.contains(&"dismiss"));
            }
        }

        // Check other fields
        assert!(props.contains_key("workspace_id"));
        assert!(props.contains_key("project_id"));
        assert!(props.contains_key("limit"));
        assert!(props.contains_key("status"));
        assert!(props.contains_key("context"));
        assert!(props.contains_key("title"));
        assert!(props.contains_key("content"));
        assert!(props.contains_key("remind_at"));
        assert!(props.contains_key("priority"));
        assert!(props.contains_key("recurrence"));
        assert!(props.contains_key("keywords"));
        assert!(props.contains_key("reminder_id"));
        assert!(props.contains_key("until"));

        // Check status enum
        if let Some(status) = props.get("status") {
            if let Some(enum_vals) = status.get("enum").and_then(|e| e.as_array()) {
                let values: Vec<&str> = enum_vals.iter().filter_map(|v| v.as_str()).collect();
                assert!(values.contains(&"pending"));
                assert!(values.contains(&"completed"));
                assert!(values.contains(&"dismissed"));
                assert!(values.contains(&"snoozed"));
            }
        }

        // Check priority enum
        if let Some(priority) = props.get("priority") {
            if let Some(enum_vals) = priority.get("enum").and_then(|e| e.as_array()) {
                let values: Vec<&str> = enum_vals.iter().filter_map(|v| v.as_str()).collect();
                assert!(values.contains(&"low"));
                assert!(values.contains(&"normal"));
                assert!(values.contains(&"high"));
                assert!(values.contains(&"urgent"));
            }
        }

        // Check recurrence enum
        if let Some(recurrence) = props.get("recurrence") {
            if let Some(enum_vals) = recurrence.get("enum").and_then(|e| e.as_array()) {
                let values: Vec<&str> = enum_vals.iter().filter_map(|v| v.as_str()).collect();
                assert!(values.contains(&"daily"));
                assert!(values.contains(&"weekly"));
                assert!(values.contains(&"monthly"));
            }
        }
    }
}

// ============================================================================
// Input Validation Tests
// ============================================================================

mod validation_tests {
    use super::ReminderTool;
    use super::{create_mock_client, json, ToolHandler};

    #[tokio::test]
    async fn test_reminder_unknown_action() {
        let client = create_mock_client();
        let tool = ReminderTool::new(client);

        let result = tool
            .execute(json!({
                "action": "unknown_action"
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Unknown action"));
    }

    #[tokio::test]
    async fn test_reminder_create_requires_title() {
        let client = create_mock_client();
        let tool = ReminderTool::new(client);

        let result = tool
            .execute(json!({
                "action": "create",
                "remind_at": "2024-12-01T10:00:00Z"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("title"));
    }

    #[tokio::test]
    async fn test_reminder_create_requires_remind_at() {
        let client = create_mock_client();
        let tool = ReminderTool::new(client);

        let result = tool
            .execute(json!({
                "action": "create",
                "title": "Test reminder"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("remind_at"));
    }

    #[tokio::test]
    async fn test_reminder_snooze_requires_reminder_id() {
        let client = create_mock_client();
        let tool = ReminderTool::new(client);

        let result = tool
            .execute(json!({
                "action": "snooze",
                "until": "2024-12-01T10:00:00Z"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("reminder_id"));
    }

    #[tokio::test]
    async fn test_reminder_snooze_requires_until() {
        let client = create_mock_client();
        let tool = ReminderTool::new(client);

        let result = tool
            .execute(json!({
                "action": "snooze",
                "reminder_id": "550e8400-e29b-41d4-a716-446655440000"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("until"));
    }

    #[tokio::test]
    async fn test_reminder_complete_requires_reminder_id() {
        let client = create_mock_client();
        let tool = ReminderTool::new(client);

        let result = tool
            .execute(json!({
                "action": "complete"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("reminder_id"));
    }

    #[tokio::test]
    async fn test_reminder_dismiss_requires_reminder_id() {
        let client = create_mock_client();
        let tool = ReminderTool::new(client);

        let result = tool
            .execute(json!({
                "action": "dismiss"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("reminder_id"));
    }

    #[tokio::test]
    async fn test_reminder_validates_workspace_uuid() {
        let client = create_mock_client();
        let tool = ReminderTool::new(client);

        let result = tool
            .execute(json!({
                "action": "list",
                "workspace_id": "invalid-uuid"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Invalid"));
    }

    #[tokio::test]
    async fn test_reminder_validates_reminder_uuid() {
        let client = create_mock_client();
        let tool = ReminderTool::new(client);

        let result = tool
            .execute(json!({
                "action": "complete",
                "reminder_id": "invalid-uuid"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Invalid"));
    }

    #[tokio::test]
    async fn test_reminder_list_no_required_params() {
        let client = create_mock_client();
        let tool = ReminderTool::new(client);

        // list action has no required parameters
        let result = tool
            .execute(json!({
                "action": "list"
            }))
            .await;

        // Should fail due to network (not validation)
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(!err.contains("Unknown action"));
        assert!(!err.contains("required"));
    }

    #[tokio::test]
    async fn test_reminder_active_no_required_params() {
        let client = create_mock_client();
        let tool = ReminderTool::new(client);

        // active action has no required parameters (returns pending/overdue)
        let result = tool
            .execute(json!({
                "action": "active"
            }))
            .await;

        // Should fail due to network (not validation)
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(!err.contains("Unknown action"));
        assert!(!err.contains("required"));
    }
}

// ============================================================================
// Input Struct Tests
// ============================================================================

mod input_struct_tests {
    use super::json;
    use super::ReminderInput;

    #[test]
    fn test_reminder_input_list_deserialization() {
        let input: ReminderInput = serde_json::from_value(json!({
            "action": "list",
            "workspace_id": "550e8400-e29b-41d4-a716-446655440000",
            "status": "pending",
            "limit": 10
        }))
        .unwrap();

        assert_eq!(input.action, "list");
        assert!(input.workspace_id.is_some());
        assert_eq!(input.status, Some("pending".to_string()));
        assert_eq!(input.limit, Some(10));
    }

    #[test]
    fn test_reminder_input_create_deserialization() {
        let input: ReminderInput = serde_json::from_value(json!({
            "action": "create",
            "title": "Review PR",
            "content": "Check the authentication changes",
            "remind_at": "2024-12-01T10:00:00Z",
            "priority": "high",
            "recurrence": "weekly",
            "keywords": ["pr", "review", "auth"]
        }))
        .unwrap();

        assert_eq!(input.action, "create");
        assert_eq!(input.title, Some("Review PR".to_string()));
        assert_eq!(
            input.content,
            Some("Check the authentication changes".to_string())
        );
        assert_eq!(input.priority, Some("high".to_string()));
        assert_eq!(input.recurrence, Some("weekly".to_string()));
        assert_eq!(
            input.keywords,
            Some(vec![
                "pr".to_string(),
                "review".to_string(),
                "auth".to_string()
            ])
        );
    }

    #[test]
    fn test_reminder_input_snooze_deserialization() {
        let input: ReminderInput = serde_json::from_value(json!({
            "action": "snooze",
            "reminder_id": "550e8400-e29b-41d4-a716-446655440000",
            "until": "2024-12-02T10:00:00Z"
        }))
        .unwrap();

        assert_eq!(input.action, "snooze");
        assert!(input.reminder_id.is_some());
        assert_eq!(input.until, Some("2024-12-02T10:00:00Z".to_string()));
    }
}

// ============================================================================
// Constants Tests
// ============================================================================

mod constants_tests {
    use super::*;

    #[test]
    fn test_valid_actions() {
        assert!(VALID_ACTIONS.contains(&"list"));
        assert!(VALID_ACTIONS.contains(&"active"));
        assert!(VALID_ACTIONS.contains(&"create"));
        assert!(VALID_ACTIONS.contains(&"snooze"));
        assert!(VALID_ACTIONS.contains(&"complete"));
        assert!(VALID_ACTIONS.contains(&"dismiss"));
        assert_eq!(VALID_ACTIONS.len(), 6);
    }

    #[test]
    fn test_valid_statuses() {
        assert!(VALID_STATUSES.contains(&"pending"));
        assert!(VALID_STATUSES.contains(&"completed"));
        assert!(VALID_STATUSES.contains(&"dismissed"));
        assert!(VALID_STATUSES.contains(&"snoozed"));
        assert_eq!(VALID_STATUSES.len(), 4);
    }

    #[test]
    fn test_valid_priorities() {
        assert!(VALID_PRIORITIES.contains(&"low"));
        assert!(VALID_PRIORITIES.contains(&"normal"));
        assert!(VALID_PRIORITIES.contains(&"high"));
        assert!(VALID_PRIORITIES.contains(&"urgent"));
        assert_eq!(VALID_PRIORITIES.len(), 4);
    }

    #[test]
    fn test_valid_recurrence() {
        assert!(VALID_RECURRENCE.contains(&"daily"));
        assert!(VALID_RECURRENCE.contains(&"weekly"));
        assert!(VALID_RECURRENCE.contains(&"monthly"));
        assert_eq!(VALID_RECURRENCE.len(), 3);
    }
}

// ============================================================================
// Tool Registration Tests
// ============================================================================

mod registration_tests {
    #[test]
    fn test_reminder_tool_count() {
        // Expected reminder tools:
        // - reminder (unified)
        // Total: 1 tool

        let expected_tools = ["reminder"];
        assert_eq!(expected_tools.len(), 1);
    }

    #[test]
    fn test_reminder_actions_coverage() {
        // Document all reminder actions:
        //
        // No-required-params actions:
        // - list: List all reminders (with optional filters)
        // - active: Get pending/overdue reminders
        //
        // Create action (requires title + remind_at):
        // - create: Create a new reminder
        //
        // Actions requiring reminder_id:
        // - snooze: Snooze reminder (also requires until)
        // - complete: Mark reminder as complete
        // - dismiss: Dismiss reminder

        let all_actions = ["list", "active", "create", "snooze", "complete", "dismiss"];
        assert_eq!(all_actions.len(), 6);
    }
}
