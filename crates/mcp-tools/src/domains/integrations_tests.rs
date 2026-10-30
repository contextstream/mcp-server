//! Tests for integration domain tools.

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

fn create_test_session(
    client: &ContextStreamClient,
) -> std::sync::Arc<mcp_session::SessionManager> {
    std::sync::Arc::new(mcp_session::SessionManager::new(
        client.clone(),
        TestFixtures::test_config(),
    ))
}

// ============================================================================
// Metadata Tests
// ============================================================================

mod metadata_tests {
    use super::IntegrationTool;
    use super::{create_mock_client, create_test_session, ToolCategory, ToolHandler};

    #[test]
    fn test_integration_tool_metadata() {
        let client = create_mock_client();
        let tool = IntegrationTool::new(client.clone(), create_test_session(&client));
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "integration");
        assert_eq!(metadata.title, "Integration Operations");
        assert!(metadata.description.contains("Slack"));
        assert!(metadata.description.contains("GitHub"));
        assert!(metadata.description.contains("Notion"));
        assert!(metadata.description.contains("status"));
        assert!(metadata.description.contains("search"));
        assert!(metadata.description.contains("create_page"));
        assert!(metadata.description.contains("team_activity"));
        assert_eq!(metadata.category, ToolCategory::Integrations);
        assert!(!metadata.is_pro);
    }
}

// ============================================================================
// Schema Tests
// ============================================================================

mod schema_tests {
    use super::IntegrationTool;
    use super::{create_mock_client, create_test_session, ToolHandler};

    #[test]
    fn test_integration_tool_schema() {
        let client = create_mock_client();
        let tool = IntegrationTool::new(client.clone(), create_test_session(&client));
        let schema = tool.input_schema();

        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("provider"));
        assert!(props.contains_key("action"));

        // Check provider enum
        if let Some(provider) = props.get("provider") {
            if let Some(enum_vals) = provider.get("enum").and_then(|e| e.as_array()) {
                let values: Vec<&str> = enum_vals.iter().filter_map(|v| v.as_str()).collect();
                assert!(values.contains(&"slack"));
                assert!(values.contains(&"github"));
                assert!(values.contains(&"notion"));
                assert!(values.contains(&"all"));
            }
        }

        // Check action enum contains many expected actions
        if let Some(action) = props.get("action") {
            if let Some(enum_vals) = action.get("enum").and_then(|e| e.as_array()) {
                let values: Vec<&str> = enum_vals.iter().filter_map(|v| v.as_str()).collect();
                // Common actions
                assert!(values.contains(&"status"));
                assert!(values.contains(&"search"));
                assert!(values.contains(&"stats"));
                assert!(values.contains(&"activity"));
                // Slack actions
                assert!(values.contains(&"channels"));
                assert!(values.contains(&"discussions"));
                // GitHub actions
                assert!(values.contains(&"repos"));
                assert!(values.contains(&"issues"));
                // Notion actions
                assert!(values.contains(&"create_page"));
                assert!(values.contains(&"get_page"));
                assert!(values.contains(&"update_page"));
                assert!(values.contains(&"search_pages"));
                assert!(values.contains(&"create_database"));
                assert!(values.contains(&"list_databases"));
                assert!(values.contains(&"query_database"));
            }
        }

        // Check other fields
        assert!(props.contains_key("workspace_id"));
        assert!(props.contains_key("query"));
        assert!(props.contains_key("limit"));
        assert!(props.contains_key("days"));
        assert!(props.contains_key("since"));
        assert!(props.contains_key("until"));
        assert!(props.contains_key("database_id"));
        assert!(props.contains_key("title"));
        assert!(props.contains_key("content"));
        assert!(props.contains_key("page_id"));
        assert!(props.contains_key("event_type"));
        assert!(props.contains_key("filter"));
        assert!(props.contains_key("sorts"));
    }

    #[test]
    fn test_integration_schema_notion_event_types() {
        let client = create_mock_client();
        let tool = IntegrationTool::new(client.clone(), create_test_session(&client));
        let schema = tool.input_schema();

        let props = schema["properties"].as_object().unwrap();
        if let Some(event_type) = props.get("event_type") {
            if let Some(enum_vals) = event_type.get("enum").and_then(|e| e.as_array()) {
                let values: Vec<&str> = enum_vals.iter().filter_map(|v| v.as_str()).collect();
                assert!(values.contains(&"NotionTask"));
                assert!(values.contains(&"NotionMeeting"));
                assert!(values.contains(&"NotionWiki"));
                assert!(values.contains(&"NotionBugReport"));
                assert!(values.contains(&"NotionFeature"));
                assert!(values.contains(&"NotionJournal"));
                assert!(values.contains(&"NotionPage"));
            }
        }
    }
}

// ============================================================================
// Input Validation Tests
// ============================================================================

mod validation_tests {
    use super::IntegrationTool;
    use super::{create_mock_client, create_test_session, json, ToolHandler};

    #[tokio::test]
    async fn test_integration_unknown_action() {
        let client = create_mock_client();
        let tool = IntegrationTool::new(client.clone(), create_test_session(&client));

        let result = tool
            .execute(json!({
                "provider": "slack",
                "action": "unknown_action"
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Unknown action"));
    }

    #[tokio::test]
    async fn test_integration_search_requires_query() {
        let client = create_mock_client();
        let tool = IntegrationTool::new(client.clone(), create_test_session(&client));

        let result = tool
            .execute(json!({
                "provider": "slack",
                "action": "search"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("query"));
    }

    #[tokio::test]
    async fn test_integration_channels_only_for_slack() {
        let client = create_mock_client();
        let tool = IntegrationTool::new(client.clone(), create_test_session(&client));

        let result = tool
            .execute(json!({
                "provider": "github",
                "action": "channels"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Slack"));
    }

    #[tokio::test]
    async fn test_integration_discussions_only_for_slack() {
        let client = create_mock_client();
        let tool = IntegrationTool::new(client.clone(), create_test_session(&client));

        let result = tool
            .execute(json!({
                "provider": "notion",
                "action": "discussions"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Slack"));
    }

    #[tokio::test]
    async fn test_integration_sync_users_only_for_slack() {
        let client = create_mock_client();
        let tool = IntegrationTool::new(client.clone(), create_test_session(&client));

        let result = tool
            .execute(json!({
                "provider": "github",
                "action": "sync_users"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Slack"));
    }

    #[tokio::test]
    async fn test_integration_repos_only_for_github() {
        let client = create_mock_client();
        let tool = IntegrationTool::new(client.clone(), create_test_session(&client));

        let result = tool
            .execute(json!({
                "provider": "slack",
                "action": "repos"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("GitHub"));
    }

    #[tokio::test]
    async fn test_integration_issues_only_for_github() {
        let client = create_mock_client();
        let tool = IntegrationTool::new(client.clone(), create_test_session(&client));

        let result = tool
            .execute(json!({
                "provider": "notion",
                "action": "issues"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("GitHub"));
    }

    #[tokio::test]
    async fn test_integration_create_page_only_for_notion() {
        let client = create_mock_client();
        let tool = IntegrationTool::new(client.clone(), create_test_session(&client));

        let result = tool
            .execute(json!({
                "provider": "slack",
                "action": "create_page",
                "title": "Test Page"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Notion"));
    }

    #[tokio::test]
    async fn test_integration_create_page_requires_title() {
        let client = create_mock_client();
        let tool = IntegrationTool::new(client.clone(), create_test_session(&client));

        let result = tool
            .execute(json!({
                "provider": "notion",
                "action": "create_page"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("title"));
    }

    #[tokio::test]
    async fn test_integration_get_page_requires_page_id() {
        let client = create_mock_client();
        let tool = IntegrationTool::new(client.clone(), create_test_session(&client));

        let result = tool
            .execute(json!({
                "provider": "notion",
                "action": "get_page"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("page_id"));
    }

    #[tokio::test]
    async fn test_integration_update_page_requires_page_id() {
        let client = create_mock_client();
        let tool = IntegrationTool::new(client.clone(), create_test_session(&client));

        let result = tool
            .execute(json!({
                "provider": "notion",
                "action": "update_page"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("page_id"));
    }

    #[tokio::test]
    async fn test_integration_create_database_requires_title() {
        let client = create_mock_client();
        let tool = IntegrationTool::new(client.clone(), create_test_session(&client));

        let result = tool
            .execute(json!({
                "provider": "notion",
                "action": "create_database"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("title"));
    }

    #[tokio::test]
    async fn test_integration_query_database_requires_database_id() {
        let client = create_mock_client();
        let tool = IntegrationTool::new(client.clone(), create_test_session(&client));

        let result = tool
            .execute(json!({
                "provider": "notion",
                "action": "query_database"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("database_id"));
    }

    #[tokio::test]
    async fn test_integration_validates_workspace_uuid() {
        let client = create_mock_client();
        let tool = IntegrationTool::new(client.clone(), create_test_session(&client));

        let result = tool
            .execute(json!({
                "provider": "slack",
                "action": "status",
                "workspace_id": "invalid-uuid"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Invalid"));
    }

    #[tokio::test]
    async fn test_integration_status_no_required_params() {
        let client = create_mock_client();
        let tool = IntegrationTool::new(client.clone(), create_test_session(&client));

        // status action has no required parameters
        let result = tool
            .execute(json!({
                "provider": "slack",
                "action": "status"
            }))
            .await;

        // Should fail due to network (not validation)
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(!err.contains("Unknown action"));
        assert!(!err.contains("required"));
    }

    #[tokio::test]
    async fn test_integration_list_databases_only_for_notion() {
        let client = create_mock_client();
        let tool = IntegrationTool::new(client.clone(), create_test_session(&client));

        let result = tool
            .execute(json!({
                "provider": "github",
                "action": "list_databases"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Notion"));
    }

    #[tokio::test]
    async fn test_integration_search_pages_only_for_notion() {
        let client = create_mock_client();
        let tool = IntegrationTool::new(client.clone(), create_test_session(&client));

        let result = tool
            .execute(json!({
                "provider": "slack",
                "action": "search_pages"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Notion"));
    }

    #[tokio::test]
    async fn test_integration_stats_no_required_params() {
        let client = create_mock_client();
        let tool = IntegrationTool::new(client.clone(), create_test_session(&client));

        let result = tool
            .execute(json!({
                "provider": "github",
                "action": "stats"
            }))
            .await;

        // Should fail due to network (not validation)
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(!err.contains("Unknown action"));
    }

    #[tokio::test]
    async fn test_integration_activity_no_required_params() {
        let client = create_mock_client();
        let tool = IntegrationTool::new(client.clone(), create_test_session(&client));

        let result = tool
            .execute(json!({
                "provider": "notion",
                "action": "activity"
            }))
            .await;

        // Should fail due to network (not validation)
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(!err.contains("Unknown action"));
    }

    #[tokio::test]
    async fn test_integration_team_activity_no_required_params() {
        let client = create_mock_client();
        let tool = IntegrationTool::new(client.clone(), create_test_session(&client));

        // team_activity is a team-only feature
        let result = tool
            .execute(json!({
                "provider": "all",
                "action": "team_activity"
            }))
            .await;

        // Should fail due to network (not validation)
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(!err.contains("Unknown action"));
    }
}

// ============================================================================
// Input Struct Tests
// ============================================================================

mod input_struct_tests {
    use super::json;
    use super::{IntegrationInput, NotionSortInput};

    #[test]
    fn test_integration_input_status_deserialization() {
        let input: IntegrationInput = serde_json::from_value(json!({
            "provider": "slack",
            "action": "status",
            "workspace_id": "550e8400-e29b-41d4-a716-446655440000"
        }))
        .unwrap();

        assert_eq!(input.provider, "slack");
        assert_eq!(input.action, "status");
        assert!(input.workspace_id.is_some());
    }

    #[test]
    fn test_integration_input_search_deserialization() {
        let input: IntegrationInput = serde_json::from_value(json!({
            "provider": "github",
            "action": "search",
            "query": "bug fix",
            "limit": 20
        }))
        .unwrap();

        assert_eq!(input.provider, "github");
        assert_eq!(input.action, "search");
        assert_eq!(input.query, Some("bug fix".to_string()));
        assert_eq!(input.limit, Some(20));
    }

    #[test]
    fn test_integration_input_activity_deserialization() {
        let input: IntegrationInput = serde_json::from_value(json!({
            "provider": "notion",
            "action": "activity",
            "since": "2024-01-01T00:00:00Z",
            "until": "2024-12-31T23:59:59Z",
            "database_id": "abc123"
        }))
        .unwrap();

        assert_eq!(input.action, "activity");
        assert_eq!(input.since, Some("2024-01-01T00:00:00Z".to_string()));
        assert_eq!(input.until, Some("2024-12-31T23:59:59Z".to_string()));
        assert_eq!(input.database_id, Some("abc123".to_string()));
    }

    #[test]
    fn test_integration_input_notion_create_page_deserialization() {
        let input: IntegrationInput = serde_json::from_value(json!({
            "provider": "notion",
            "action": "create_page",
            "title": "Meeting Notes",
            "content": "# Discussion Points\n\n- Item 1\n- Item 2",
            "parent_page_id": "parent123",
            "parent_database_id": "db123"
        }))
        .unwrap();

        assert_eq!(input.action, "create_page");
        assert_eq!(input.title, Some("Meeting Notes".to_string()));
        assert!(input.content.is_some());
        assert_eq!(input.parent_page_id, Some("parent123".to_string()));
        assert_eq!(input.parent_database_id, Some("db123".to_string()));
    }

    #[test]
    fn test_integration_input_notion_search_pages_deserialization() {
        let input: IntegrationInput = serde_json::from_value(json!({
            "provider": "notion",
            "action": "search_pages",
            "query": "authentication",
            "event_type": "NotionTask",
            "status": "In Progress",
            "priority": "High",
            "has_due_date": true,
            "tags": "urgent,backend"
        }))
        .unwrap();

        assert_eq!(input.action, "search_pages");
        assert_eq!(input.event_type, Some("NotionTask".to_string()));
        assert_eq!(input.status, Some("In Progress".to_string()));
        assert_eq!(input.priority, Some("High".to_string()));
        assert_eq!(input.has_due_date, Some(true));
        assert_eq!(input.tags, Some("urgent,backend".to_string()));
    }

    #[test]
    fn test_integration_input_query_database_with_sorts() {
        let input: IntegrationInput = serde_json::from_value(json!({
            "provider": "notion",
            "action": "query_database",
            "database_id": "db123",
            "filter": {"property": "Status", "equals": "Done"},
            "sorts": [
                {"property": "Created", "direction": "descending"},
                {"property": "Name", "direction": "ascending"}
            ]
        }))
        .unwrap();

        assert_eq!(input.action, "query_database");
        assert_eq!(input.database_id, Some("db123".to_string()));
        assert!(input.filter.is_some());

        let sorts = input.sorts.unwrap();
        assert_eq!(sorts.len(), 2);
        assert_eq!(sorts[0].property, "Created");
        assert_eq!(sorts[0].direction, "descending");
    }

    #[test]
    fn test_notion_sort_input_deserialization() {
        let sort: NotionSortInput = serde_json::from_value(json!({
            "property": "Date",
            "direction": "ascending"
        }))
        .unwrap();

        assert_eq!(sort.property, "Date");
        assert_eq!(sort.direction, "ascending");
    }
}

// ============================================================================
// Constants Tests
// ============================================================================

mod constants_tests {
    use super::*;

    #[test]
    fn test_valid_providers() {
        assert!(VALID_PROVIDERS.contains(&"slack"));
        assert!(VALID_PROVIDERS.contains(&"github"));
        assert!(VALID_PROVIDERS.contains(&"notion"));
        assert!(VALID_PROVIDERS.contains(&"linear"));
        assert!(VALID_PROVIDERS.contains(&"jira"));
        assert!(VALID_PROVIDERS.contains(&"figma"));
        assert!(VALID_PROVIDERS.contains(&"all"));
        // Connected apps served by the integration runtime.
        for provider in ["microsoft", "google_workspace", "dropbox", "box"] {
            assert!(VALID_PROVIDERS.contains(&provider), "{provider}");
        }
        assert_eq!(VALID_PROVIDERS.len(), 11);
    }

    #[test]
    fn test_valid_actions() {
        // Common actions
        assert!(VALID_ACTIONS.contains(&"status"));
        assert!(VALID_ACTIONS.contains(&"search"));
        assert!(VALID_ACTIONS.contains(&"stats"));
        assert!(VALID_ACTIONS.contains(&"activity"));
        assert!(VALID_ACTIONS.contains(&"contributors"));
        assert!(VALID_ACTIONS.contains(&"knowledge"));
        assert!(VALID_ACTIONS.contains(&"summary"));
        // Slack-specific
        assert!(VALID_ACTIONS.contains(&"channels"));
        assert!(VALID_ACTIONS.contains(&"discussions"));
        assert!(VALID_ACTIONS.contains(&"sync_users"));
        // GitHub-specific
        assert!(VALID_ACTIONS.contains(&"repos"));
        assert!(VALID_ACTIONS.contains(&"issues"));
        // Notion-specific
        assert!(VALID_ACTIONS.contains(&"create_page"));
        assert!(VALID_ACTIONS.contains(&"create_database"));
        assert!(VALID_ACTIONS.contains(&"list_databases"));
        assert!(VALID_ACTIONS.contains(&"search_pages"));
        assert!(VALID_ACTIONS.contains(&"get_page"));
        assert!(VALID_ACTIONS.contains(&"query_database"));
        assert!(VALID_ACTIONS.contains(&"update_page"));
        // Figma-specific
        assert!(VALID_ACTIONS.contains(&"files"));
        // Team action
        assert!(VALID_ACTIONS.contains(&"team_activity"));
    }

    #[test]
    fn test_valid_notion_event_types() {
        assert!(VALID_NOTION_EVENT_TYPES.contains(&"NotionTask"));
        assert!(VALID_NOTION_EVENT_TYPES.contains(&"NotionMeeting"));
        assert!(VALID_NOTION_EVENT_TYPES.contains(&"NotionWiki"));
        assert!(VALID_NOTION_EVENT_TYPES.contains(&"NotionBugReport"));
        assert!(VALID_NOTION_EVENT_TYPES.contains(&"NotionFeature"));
        assert!(VALID_NOTION_EVENT_TYPES.contains(&"NotionJournal"));
        assert!(VALID_NOTION_EVENT_TYPES.contains(&"NotionPage"));
        assert_eq!(VALID_NOTION_EVENT_TYPES.len(), 7);
    }
}

// ============================================================================
// Tool Registration Tests
// ============================================================================

mod registration_tests {
    #[test]
    fn test_integration_tool_count() {
        // Expected integration tools:
        // - integration (unified)
        // Total: 1 tool

        let expected_tools = ["integration"];
        assert_eq!(expected_tools.len(), 1);
    }

    #[test]
    fn test_integration_actions_coverage() {
        // Document all integration actions:
        //
        // Common actions (work with all providers):
        // - status: Get integration status
        // - search: Search content (requires query)
        // - stats: Get statistics
        // - activity: Get recent activity
        // - contributors: Get top contributors
        // - knowledge: Get knowledge items
        // - summary: Get workspace summary
        //
        // Slack-only actions:
        // - channels: List Slack channels
        // - discussions: List discussions
        // - sync_users: Sync Slack users
        //
        // GitHub-only actions:
        // - repos: List repositories
        // - issues: List issues
        //
        // Notion-only actions:
        // - create_page: Create a page (requires title)
        // - get_page: Get page details (requires page_id)
        // - update_page: Update a page (requires page_id)
        // - create_database: Create database (requires title)
        // - list_databases: List all databases
        // - search_pages: Search pages with smart type detection
        // - query_database: Query database (requires database_id)
        //
        // Team-only actions:
        // - team_activity: Aggregated team activity

        let common_actions = [
            "status",
            "search",
            "stats",
            "activity",
            "contributors",
            "knowledge",
            "summary",
        ];
        let slack_actions = ["channels", "discussions", "sync_users"];
        let github_actions = ["repos", "issues"];
        let notion_actions = [
            "create_page",
            "get_page",
            "update_page",
            "create_database",
            "list_databases",
            "search_pages",
            "query_database",
        ];
        let team_actions = ["team_activity"];

        assert_eq!(common_actions.len(), 7);
        assert_eq!(slack_actions.len(), 3);
        assert_eq!(github_actions.len(), 2);
        assert_eq!(notion_actions.len(), 7);
        assert_eq!(team_actions.len(), 1);

        // Total: 20 unique actions
        let total = common_actions.len()
            + slack_actions.len()
            + github_actions.len()
            + notion_actions.len()
            + team_actions.len();
        assert_eq!(total, 20);
    }
}

// ============================================================================
// Connected App Tests
// ============================================================================

mod app_action_tests {
    use super::IntegrationTool;
    use super::{create_mock_client, create_test_session, json, ToolHandler};

    fn tool() -> IntegrationTool {
        let client = create_mock_client();
        IntegrationTool::new(client.clone(), create_test_session(&client))
    }

    async fn error_for(input: serde_json::Value) -> String {
        tool().execute(input).await.unwrap_err().to_string()
    }

    #[test]
    fn the_schema_offers_connected_apps() {
        let schema = tool().input_schema();
        let props = schema["properties"].as_object().unwrap();
        let providers: Vec<&str> = props["provider"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        for provider in ["microsoft", "google_workspace", "dropbox", "box"] {
            assert!(providers.contains(&provider), "{provider}");
        }
        let actions: Vec<&str> = props["action"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        for action in ["apps", "find", "read", "capabilities", "edit"] {
            assert!(actions.contains(&action), "{action}");
        }
        for field in [
            "reference_id",
            "connection_id",
            "resource_id",
            "resource_type",
            "mime_type",
            "container",
            "operation",
            "arguments",
            "base_revisions",
            "idempotency_key",
        ] {
            assert!(props.contains_key(field), "{field}");
        }
        assert!(tool().metadata().description.contains("Microsoft 365"));
    }

    #[tokio::test]
    async fn find_needs_a_query() {
        let error = error_for(json!({"provider": "all", "action": "find"})).await;
        assert!(error.contains("query is required for find"), "{error}");
    }

    #[tokio::test]
    async fn read_needs_an_item() {
        let error = error_for(json!({"provider": "all", "action": "read"})).await;
        assert!(error.contains("connection_id or reference_id"), "{error}");

        let error = error_for(json!({
            "provider": "microsoft",
            "action": "read",
            "connection_id": "550e8400-e29b-41d4-a716-446655440000"
        }))
        .await;
        assert!(error.contains("resource_id or operation"), "{error}");
    }

    #[tokio::test]
    async fn edit_needs_an_operation_its_arguments_and_a_session() {
        let connection = "550e8400-e29b-41d4-a716-446655440000";
        let error = error_for(json!({"provider": "microsoft", "action": "edit"})).await;
        assert!(
            error.contains("connection_id is required for edit"),
            "{error}"
        );

        let error = error_for(json!({
            "provider": "microsoft", "action": "edit", "connection_id": connection
        }))
        .await;
        assert!(error.contains("operation is required for edit"), "{error}");

        let error = error_for(json!({
            "provider": "microsoft", "action": "edit", "connection_id": connection,
            "operation": "excel.range.update", "arguments": "not an object"
        }))
        .await;
        assert!(error.contains("arguments must be an object"), "{error}");

        // An uninitialized session has nowhere to record the change, and the
        // refusal says how to give it one (a stateless client cannot run init
        // first and have it remembered).
        let error = error_for(json!({
            "provider": "microsoft", "action": "edit", "connection_id": connection,
            "operation": "excel.range.update", "arguments": {"range": "B2"}
        }))
        .await;
        assert!(error.contains("session_id"), "{error}");
        assert!(error.contains("init"), "{error}");

        // The id init returned, passed as session_id, is all a client with no
        // stored state needs: validation passes and the call reaches the
        // runtime (unreachable here, so the error is not a validation one).
        let error = error_for(json!({
            "provider": "microsoft", "action": "edit", "connection_id": connection,
            "operation": "excel.range.update", "arguments": {"range": "B2"},
            "session_id": "api-assigned-session", "workspace_id": "550e8400-e29b-41d4-a716-446655440001"
        }))
        .await;
        assert!(!error.contains("Validation"), "{error}");
    }

    #[tokio::test]
    async fn app_actions_reach_the_runtime() {
        // The mock API is unreachable: getting that far means validation passed.
        let error =
            error_for(json!({"provider": "all", "action": "find", "query": "budget"})).await;
        assert!(!error.contains("Validation"), "{error}");
        let error = error_for(json!({"provider": "all", "action": "apps"})).await;
        assert!(!error.contains("Validation"), "{error}");
    }
}

// ============================================================================
// Search (#275)
// ============================================================================

#[test]
fn search_counts_bare_and_cross_source_answers() {
    assert_eq!(search_result_count(&json!([1, 2])), 2);
    assert_eq!(
        search_result_count(&json!({"results": [1, 2, 3], "total_results": 3})),
        3
    );
    assert_eq!(search_result_count(&json!({"data": {"results": [1]}})), 1);
    assert_eq!(search_result_count(&json!({})), 0);
}

#[tokio::test]
async fn search_refuses_providers_the_api_cannot_search() {
    let client = create_mock_client();
    let tool = IntegrationTool::new(client.clone(), create_test_session(&client));
    let error = tool
        .execute(json!({"action": "search", "provider": "notion", "query": "deploy"}))
        .await
        .expect_err("notion search is not served");
    assert!(error.to_string().contains("not searchable"), "{error}");
}

#[test]
fn status_says_each_providers_state_and_which_are_not_connected() {
    // #398: "Retrieved 2 integration statuses." and a 404 for slack.
    let all = json!([
        {"provider": "github", "status": "disconnected", "error_message": "GitHub authentication needs attention."},
        {"provider": "local", "status": "connected", "error_message": null}
    ]);
    let text = super::integration_status_text("all", &all);
    assert!(text.starts_with("Integration status (2):\n- github: disconnected — GitHub authentication needs attention.\n- local: connected\nNot connected: slack, notion,"), "{text}");
    assert!(!text.contains("Not connected: slack, github"), "{text}");
    assert_eq!(
        super::integration_status_text(
            "slack",
            &json!({"provider": "slack", "status": "not_connected"})
        ),
        "slack integration status: not connected."
    );
}

#[test]
fn a_generic_action_the_api_does_not_route_is_refused_in_words() {
    // #458: these pairs answered a bare "HTTP error (404): HTTP 404 error".
    assert_eq!(
        generic_action_problem("figma", "summary").as_deref(),
        Some("summary is not available for figma; it is available for slack, github, notion, all.")
    );
    assert_eq!(
        generic_action_problem("microsoft", "contributors").as_deref(),
        Some("contributors is not available for microsoft; it is available for slack, github.")
    );
    assert!(generic_action_problem("all", "stats").is_some());
    assert_eq!(generic_action_problem("github", "stats"), None);
    assert_eq!(generic_action_problem("all", "activity"), None);
    assert_eq!(generic_action_problem("all", "summary"), None);
    assert_eq!(generic_action_problem("github", "repos"), None);
    assert_eq!(generic_action_problem("not-a-provider", "stats"), None);
}

#[test]
fn an_unconnected_or_disabled_provider_is_an_answer_in_words() {
    let http = |status: u16, message: &str| Error::Http {
        status,
        message: message.to_string(),
        code: mcp_types::ErrorCode::UnknownError,
        source: None,
    };
    let not_connected = http(
        503,
        "GitHub integration is not connected for this workspace. Connect it in the dashboard.",
    );
    assert_eq!(
        integration_unavailable_text("github", &not_connected).as_deref(),
        Some(
            "GitHub integration is not connected for this workspace. Connect it in the dashboard."
        )
    );
    assert!(integration_unavailable_text(
        "notion",
        &http(404, "Not found: Notion integration not found")
    )
    .is_some_and(|text| text.starts_with("notion is not connected for this workspace")));
    assert!(integration_unavailable_text(
        "box",
        &http(403, "Forbidden: External integrations are not enabled yet")
    )
    .is_some_and(|text| text.starts_with("box is not available yet")));
    assert_eq!(
        integration_unavailable_text("github", &http(503, "upstream down")),
        None
    );
    assert_eq!(
        integration_unavailable_text("github", &http(500, "is not connected")),
        None
    );
}

#[tokio::test]
async fn notion_search_pages_reads_the_search_route() {
    // #458: it POSTed to the create-page route and got "Failed to deserialize
    // the JSON body" on every call.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = vec![0_u8; 16 * 1024];
        let count = socket.read(&mut buffer).await.unwrap();
        let body = r#"{"success":true,"data":[]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        socket.write_all(response.as_bytes()).await.unwrap();
        String::from_utf8_lossy(&buffer[..count]).to_string()
    });
    let mut config = TestFixtures::test_config();
    config.api_url = format!("http://{address}");
    let client = ContextStreamClient::new(config);
    let tool = IntegrationTool::new(client.clone(), create_test_session(&client));
    let result = tool
        .execute(json!({
            "action": "search_pages",
            "provider": "notion",
            "workspace_id": "11111111-1111-4111-8111-111111111111",
            "query": "release notes",
            "limit": 3,
        }))
        .await
        .expect("search answered");
    assert!(!result.is_error);
    let request = server.await.unwrap();
    assert!(
        request.starts_with(
            "GET /api/v1/integrations/workspaces/11111111-1111-4111-8111-111111111111/notion/pages?query=release%20notes&limit=3 "
        ),
        "{}",
        request.lines().next().unwrap_or_default()
    );

    let refused = tool
        .execute(
            json!({"action": "search_pages", "provider": "notion", "query": "x", "status": "done"}),
        )
        .await
        .expect_err("an unsupported filter is refused");
    assert!(refused
        .to_string()
        .contains("search_pages filters by query and database_id only"));
}

#[test]
fn an_integration_summary_says_what_it_found() {
    // #466: "all summary retrieved." and nothing else.
    let all = serde_json::json!({"period_days": 7, "sources_included": ["local"], "github": null, "slack": null,
        "combined_insights": {"total_activity_events": 0, "decisions_extracted": 0, "lessons_extracted": 0, "key_topics": []}});
    assert_eq!(
        format_integration_summary("all", &all),
        "Cross-source summary, last 7 days (sources: local): 0 activity events, 0 decisions, 0 lessons; no key topics."
    );
    let busy = serde_json::json!({"period_days": 30, "sources_included": ["github", "slack"],
        "github": {"total_issues": 4, "total_prs": 2, "total_releases": 1, "total_comments": 9,
                   "top_repos": [{"repo": "org/api", "issues": 3, "prs": 2, "comments": 7}],
                   "recent_decisions": ["Use Postgres enums"]},
        "slack": {"total_messages": 40, "total_threads": 6, "active_users": 3,
                  "top_channels": [{"channel": "eng", "messages": 30, "threads": 5}], "recent_decisions": []},
        "combined_insights": {"total_activity_events": 55, "decisions_extracted": 1, "lessons_extracted": 2,
                              "key_topics": ["billing", "search"]}});
    let text = format_integration_summary("all", &busy);
    assert!(text.starts_with("Cross-source summary, last 30 days (sources: github, slack): 55 activity events, 1 decisions, 2 lessons; key topics: billing, search."), "{text}");
    assert!(text.contains("\nGitHub: 4 issues, 2 PRs, 1 releases, 9 comments.\n- org/api: 3 issues, 2 PRs, 7 comments"), "{text}");
    assert!(
        text.contains(
            "\nSlack: 40 messages, 6 threads, 3 active users.\n- #eng: 30 messages, 5 threads"
        ),
        "{text}"
    );
    assert!(
        text.contains("\nRecent decisions:\n- Use Postgres enums"),
        "{text}"
    );

    let notion = serde_json::json!({"pages_synced": 3, "knowledge_extracted": 1, "summary_text": "3 pages synced, 1 knowledge item."});
    assert_eq!(
        format_integration_summary("notion", &notion),
        "notion summary: 3 pages synced, 1 knowledge item."
    );
    let github = serde_json::json!({"period": {"days": 7, "start_date": "2026-10-23", "end_date": "2026-10-30"},
        "overview": {"new_issues": 2, "merged_prs": 1}, "knowledge_extracted": [{"title": "Retry on 503"}]});
    let text = format_integration_summary("github", &github);
    assert!(text.starts_with("github summary, last 7 days: "), "{text}");
    assert!(
        text.contains("new issues 2") && text.contains("merged prs 1"),
        "{text}"
    );
    assert!(
        text.ends_with(".\nKnowledge extracted:\n- Retry on 503"),
        "{text}"
    );
}
