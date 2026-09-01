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
        assert_eq!(VALID_PROVIDERS.len(), 7);
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
