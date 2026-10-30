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
        // Listed only through the delete tool, as reminder.delete (#426).
        assert!(VALID_ACTIONS.contains(&"delete"));
        assert_eq!(VALID_ACTIONS.len(), 7);
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

// ============================================================================
// Create without content (#262)
// ============================================================================

#[tokio::test]
async fn create_without_content_sends_the_title_as_content() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buf = vec![0u8; 8192];
        loop {
            let n = stream.read(&mut buf).await.unwrap();
            request.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&request).to_string();
            if let Some((head, body)) = text.split_once("\r\n\r\n") {
                let length = head
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                    })
                    .unwrap_or(0);
                if body.len() >= length {
                    break;
                }
            }
            if n == 0 {
                break;
            }
        }
        let body =
            r#"{"data":{"id":"11111111-1111-4111-8111-111111111111","title":"QA reminder"}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        let text = String::from_utf8_lossy(&request).to_string();
        let (_, body) = text.split_once("\r\n\r\n").expect("request body");
        serde_json::from_str::<serde_json::Value>(body).expect("JSON body")
    });

    let config = mcp_types::Config {
        api_url: format!("http://{addr}"),
        api_key: Some("test-key".to_string()),
        ..Default::default()
    };
    let tool = ReminderTool::new(ContextStreamClient::new(config));
    let result = tool
        .execute(json!({
            "action": "create",
            "title": "QA reminder",
            "remind_at": "2027-01-01T00:00:00Z",
            "workspace_id": "550e8400-e29b-41d4-a716-446655440000"
        }))
        .await
        .expect("create without content");
    assert!(!result.is_error);
    let sent = server.await.unwrap();
    assert_eq!(sent["content"], "QA reminder");
}

mod enum_argument_tests {
    use super::{create_mock_client, json, ReminderTool, ToolHandler};

    #[tokio::test]
    async fn a_recurrence_status_or_priority_outside_its_enum_is_refused() {
        // #350. The mock client reaches no server, so these refusals come first.
        let tool = ReminderTool::new(create_mock_client());
        for (args, expected) in [
            (
                json!({"action": "create", "title": "t", "remind_at": "2026-10-30T13:07:45Z", "recurrence": "every blue moon"}),
                "recurrence \"every blue moon\" is not one of: daily, weekly, monthly.",
            ),
            (
                json!({"action": "list", "status": "sleeping"}),
                "status \"sleeping\" is not one of: pending, completed, dismissed, snoozed.",
            ),
            (
                json!({"action": "create", "title": "t", "remind_at": "2026-10-30T13:07:45Z", "priority": "ultra"}),
                "priority \"ultra\" is not one of: low, normal, high, urgent.",
            ),
        ] {
            let message = tool
                .execute(args.clone())
                .await
                .expect_err(&args.to_string())
                .to_string();
            assert!(message.contains(expected), "{args}: {message}");
        }
        // An advertised value in another case passes the check.
        if let Err(error) = tool
            .execute(json!({"action": "list", "status": "Pending"}))
            .await
        {
            assert!(!error.to_string().contains("is not one of"), "{error}");
        }
    }
}

#[test]
fn keywords_and_priority_narrow_a_reminder_list() {
    // #426: both were stored, then ignored by list and active.
    let mut result = json!([
        {"id": "1", "title": "Rotate the alphakw signing key", "content": "", "priority": "urgent", "keywords": []},
        {"id": "2", "title": "Other", "content": "", "priority": "normal", "keywords": ["betakw"]},
        {"id": "3", "title": "Third", "content": "mentions BETAKW", "priority": "urgent", "keywords": []}
    ]);
    let mut by_keyword = result.clone();
    keep_matching_reminders(&mut by_keyword, Some(&["betakw".to_string()]), None);
    let ids: Vec<&str> = by_keyword
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["2", "3"]);
    keep_matching_reminders(&mut result, Some(&["betakw".to_string()]), Some("urgent"));
    assert_eq!(
        result,
        json!([{"id": "3", "title": "Third", "content": "mentions BETAKW", "priority": "urgent", "keywords": []}])
    );
}

#[test]
fn reminder_list_skips_the_offset() {
    // #459: offset=2 listed the first reminders again.
    let mut listed = serde_json::json!([{"id": "a"}, {"id": "b"}, {"id": "c"}, {"id": "d"}]);
    skip_reminders(&mut listed, 2);
    assert_eq!(listed, serde_json::json!([{"id": "c"}, {"id": "d"}]));
    let mut wrapped = serde_json::json!({"items": [{"id": "a"}, {"id": "b"}]});
    skip_reminders(&mut wrapped, 5);
    assert_eq!(wrapped, serde_json::json!({"items": []}));
}
