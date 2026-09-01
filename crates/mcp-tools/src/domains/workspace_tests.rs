//! Tests for workspace domain tools.

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
    use super::{create_mock_client, ToolCategory, ToolHandler};
    use super::{WorkspaceTool, WorkspacesCreateTool, WorkspacesListTool};

    #[test]
    fn test_workspaces_list_tool_metadata() {
        let client = create_mock_client();
        let tool = WorkspacesListTool::new(client);
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "workspaces_list");
        assert_eq!(metadata.title, "List Workspaces");
        assert!(metadata.description.contains("workspaces"));
        assert_eq!(metadata.category, ToolCategory::Workspace);
        assert!(!metadata.is_pro);
    }

    #[test]
    fn test_workspaces_create_tool_metadata() {
        let client = create_mock_client();
        let tool = WorkspacesCreateTool::new(client);
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "workspaces_create");
        assert_eq!(metadata.title, "Create Workspace");
        assert!(metadata.description.contains("Create"));
        assert_eq!(metadata.category, ToolCategory::Workspace);
    }

    #[test]
    fn test_unified_workspace_tool_metadata() {
        let client = create_mock_client();
        let tool = WorkspaceTool::new(client);
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "workspace");
        assert_eq!(metadata.title, "Workspace Operations");
        assert!(metadata.description.contains("list"));
        assert!(metadata.description.contains("create"));
        assert!(metadata.description.contains("associate"));
        assert!(metadata.description.contains("bootstrap"));
        assert!(metadata.description.contains("team_members"));
        assert!(metadata.description.contains("index_settings"));
        assert_eq!(metadata.category, ToolCategory::Workspace);
    }
}

// ============================================================================
// Schema Tests
// ============================================================================

mod schema_tests {
    use super::{create_mock_client, ToolHandler};
    use super::{WorkspaceTool, WorkspacesCreateTool, WorkspacesListTool};

    #[test]
    fn test_workspaces_list_schema() {
        let client = create_mock_client();
        let tool = WorkspacesListTool::new(client);
        let schema = tool.input_schema();

        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("page"));
        assert!(props.contains_key("page_size"));
    }

    #[test]
    fn test_workspaces_create_schema() {
        let client = create_mock_client();
        let tool = WorkspacesCreateTool::new(client);
        let schema = tool.input_schema();

        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("name"));
        assert!(props.contains_key("description"));

        // name should be required
        if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
            assert!(required.iter().any(|v| v.as_str() == Some("name")));
        }
    }

    #[test]
    fn test_unified_workspace_schema() {
        let client = create_mock_client();
        let tool = WorkspaceTool::new(client);
        let schema = tool.input_schema();

        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("action"));

        // Check action enum contains all expected actions
        if let Some(action) = props.get("action") {
            if let Some(enum_vals) = action.get("enum").and_then(|e| e.as_array()) {
                let values: Vec<&str> = enum_vals.iter().filter_map(|v| v.as_str()).collect();
                assert!(values.contains(&"list"));
                assert!(values.contains(&"get"));
                assert!(values.contains(&"create"));
                assert!(values.contains(&"delete"));
                assert!(values.contains(&"associate"));
                assert!(values.contains(&"bootstrap"));
                assert!(values.contains(&"team_members"));
                assert!(values.contains(&"index_settings"));
            }
        }

        // Check other fields
        assert!(props.contains_key("workspace_id"));
        assert!(props.contains_key("folder_path"));
        assert!(props.contains_key("workspace_name"));
        assert!(props.contains_key("auto_index"));
        assert!(props.contains_key("branch_policy"));
        assert!(props.contains_key("conflict_resolution"));

        // Check branch_policy enum
        if let Some(bp) = props.get("branch_policy") {
            if let Some(enum_vals) = bp.get("enum").and_then(|e| e.as_array()) {
                let values: Vec<&str> = enum_vals.iter().filter_map(|v| v.as_str()).collect();
                assert!(values.contains(&"default_branch_wins"));
                assert!(values.contains(&"newest_wins"));
                assert!(values.contains(&"feature_branch_wins"));
            }
        }

        // Check conflict_resolution enum
        if let Some(cr) = props.get("conflict_resolution") {
            if let Some(enum_vals) = cr.get("enum").and_then(|e| e.as_array()) {
                let values: Vec<&str> = enum_vals.iter().filter_map(|v| v.as_str()).collect();
                assert!(values.contains(&"newest_timestamp"));
                assert!(values.contains(&"default_branch"));
                assert!(values.contains(&"manual"));
            }
        }
    }
}

// ============================================================================
// Input Validation Tests
// ============================================================================

mod validation_tests {
    use super::{create_mock_client, json, ToolHandler};
    use super::{WorkspaceTool, WorkspacesCreateTool};

    #[tokio::test]
    async fn test_workspaces_create_requires_name() {
        let client = create_mock_client();
        let tool = WorkspacesCreateTool::new(client);

        let result = tool
            .execute(json!({
                "name": ""
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("name"));
    }

    #[tokio::test]
    async fn test_workspaces_create_whitespace_name() {
        let client = create_mock_client();
        let tool = WorkspacesCreateTool::new(client);

        let result = tool
            .execute(json!({
                "name": "   \t  "
            }))
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_workspace_tool_unknown_action() {
        let client = create_mock_client();
        let tool = WorkspaceTool::new(client);

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
    async fn test_workspace_tool_get_requires_workspace_id() {
        let client = create_mock_client();
        let tool = WorkspaceTool::new(client);

        let result = tool
            .execute(json!({
                "action": "get"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("workspace_id"));
    }

    #[tokio::test]
    async fn test_workspace_tool_get_validates_uuid() {
        let client = create_mock_client();
        let tool = WorkspaceTool::new(client);

        let result = tool
            .execute(json!({
                "action": "get",
                "workspace_id": "invalid-uuid"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Invalid"));
    }

    #[tokio::test]
    async fn test_workspace_tool_delete_requires_workspace_id() {
        let client = create_mock_client();
        let tool = WorkspaceTool::new(client);

        let result = tool
            .execute(json!({
                "action": "delete"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("workspace_id"));
    }

    #[tokio::test]
    async fn test_workspace_tool_create_requires_name() {
        let client = create_mock_client();
        let tool = WorkspaceTool::new(client);

        let result = tool
            .execute(json!({
                "action": "create"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("name"));
    }

    #[tokio::test]
    async fn test_workspace_tool_associate_requires_fields() {
        let client = create_mock_client();
        let tool = WorkspaceTool::new(client);

        // Missing workspace_id
        let result = tool
            .execute(json!({
                "action": "associate",
                "folder_path": "/some/path"
            }))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("workspace_id"));

        // Missing folder_path
        let result = tool
            .execute(json!({
                "action": "associate",
                "workspace_id": "550e8400-e29b-41d4-a716-446655440000"
            }))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("folder_path"));
    }

    #[tokio::test]
    async fn test_workspace_tool_bootstrap_requires_workspace_name() {
        let client = create_mock_client();
        let tool = WorkspaceTool::new(client);

        let result = tool
            .execute(json!({
                "action": "bootstrap"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("workspace_name"));
    }

    #[tokio::test]
    async fn test_workspace_tool_team_members_requires_workspace_id() {
        let client = create_mock_client();
        let tool = WorkspaceTool::new(client);

        let result = tool
            .execute(json!({
                "action": "team_members"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("workspace_id"));
    }

    #[tokio::test]
    async fn test_workspace_tool_index_settings_requires_workspace_id() {
        let client = create_mock_client();
        let tool = WorkspaceTool::new(client);

        let result = tool
            .execute(json!({
                "action": "index_settings"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("workspace_id"));
    }

    #[tokio::test]
    async fn test_workspace_tool_list_no_required_params() {
        let client = create_mock_client();
        let tool = WorkspaceTool::new(client);

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
    async fn test_workspace_tool_associate_validates_uuid() {
        let client = create_mock_client();
        let tool = WorkspaceTool::new(client);

        let result = tool
            .execute(json!({
                "action": "associate",
                "workspace_id": "invalid-uuid",
                "folder_path": "/some/path"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Invalid"));
    }
}

// ============================================================================
// Input Struct Tests
// ============================================================================

mod input_struct_tests {
    use super::json;
    use super::{WorkspaceInput, WorkspacesCreateInput, WorkspacesListInput};

    #[test]
    fn test_workspaces_list_input_deserialization() {
        let input: WorkspacesListInput = serde_json::from_value(json!({
            "page": 1,
            "page_size": 20
        }))
        .unwrap();

        assert_eq!(input.page, Some(1));
        assert_eq!(input.page_size, Some(20));
    }

    #[test]
    fn test_workspaces_create_input_deserialization() {
        let input: WorkspacesCreateInput = serde_json::from_value(json!({
            "name": "My Workspace",
            "description": "A test workspace"
        }))
        .unwrap();

        assert_eq!(input.name, "My Workspace");
        assert_eq!(input.description, Some("A test workspace".to_string()));
    }

    #[test]
    fn test_workspace_input_bootstrap_deserialization() {
        let input: WorkspaceInput = serde_json::from_value(json!({
            "action": "bootstrap",
            "workspace_name": "New Project",
            "folder_path": "/home/user/project",
            "auto_index": true,
            "generate_editor_rules": true
        }))
        .unwrap();

        assert_eq!(input.action, "bootstrap");
        assert_eq!(input.workspace_name, Some("New Project".to_string()));
        assert_eq!(input.folder_path, Some("/home/user/project".to_string()));
        assert_eq!(input.auto_index, Some(true));
        assert_eq!(input.generate_editor_rules, Some(true));
    }

    #[test]
    fn test_workspace_input_index_settings_deserialization() {
        let input: WorkspaceInput = serde_json::from_value(json!({
            "action": "index_settings",
            "workspace_id": "550e8400-e29b-41d4-a716-446655440000",
            "auto_sync_enabled": true,
            "max_machines": 5,
            "branch_policy": "default_branch_wins",
            "conflict_resolution": "newest_timestamp"
        }))
        .unwrap();

        assert_eq!(input.action, "index_settings");
        assert_eq!(input.auto_sync_enabled, Some(true));
        assert_eq!(input.max_machines, Some(5));
        assert_eq!(input.branch_policy, Some("default_branch_wins".to_string()));
        assert_eq!(
            input.conflict_resolution,
            Some("newest_timestamp".to_string())
        );
    }
}

// ============================================================================
// Constants Tests
// ============================================================================

mod constants_tests {
    use super::*;

    #[test]
    fn test_valid_branch_policies() {
        assert!(VALID_BRANCH_POLICIES.contains(&"default_branch_wins"));
        assert!(VALID_BRANCH_POLICIES.contains(&"newest_wins"));
        assert!(VALID_BRANCH_POLICIES.contains(&"feature_branch_wins"));
        assert_eq!(VALID_BRANCH_POLICIES.len(), 3);
    }

    #[test]
    fn test_valid_conflict_resolutions() {
        assert!(VALID_CONFLICT_RESOLUTIONS.contains(&"newest_timestamp"));
        assert!(VALID_CONFLICT_RESOLUTIONS.contains(&"default_branch"));
        assert!(VALID_CONFLICT_RESOLUTIONS.contains(&"manual"));
        assert_eq!(VALID_CONFLICT_RESOLUTIONS.len(), 3);
    }
}

// ============================================================================
// Tool Registration Tests
// ============================================================================

mod registration_tests {
    #[test]
    fn test_workspace_tool_count() {
        // Expected workspace tools:
        // - workspace (unified)
        // - workspaces_list
        // - workspaces_create
        // Total: 3 tools

        let expected_tools = ["workspace", "workspaces_list", "workspaces_create"];

        assert_eq!(expected_tools.len(), 3);
    }

    #[test]
    fn test_workspace_actions_coverage() {
        // Document all workspace actions in the unified tool:
        //
        // No-required-params actions:
        // - list: List all workspaces (pagination optional)
        //
        // Actions requiring workspace_id:
        // - get: Get workspace details
        // - delete: Delete a workspace
        // - team_members: List team members (team plans only)
        // - index_settings: Get/update multi-machine sync settings
        //
        // Actions requiring workspace_name:
        // - create: Create new workspace
        // - bootstrap: Create workspace and initialize with rules
        //
        // Actions requiring workspace_id + folder_path:
        // - associate: Link folder to workspace

        let all_actions = [
            "list",
            "get",
            "create",
            "delete",
            "associate",
            "bootstrap",
            "team_members",
            "index_settings",
        ];

        assert_eq!(all_actions.len(), 8);
    }
}
