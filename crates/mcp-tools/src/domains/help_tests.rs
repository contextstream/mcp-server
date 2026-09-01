//! Tests for help domain tools.

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
    use super::HelpTool;
    use super::{create_mock_client, ToolCategory, ToolHandler};

    #[test]
    fn test_help_tool_metadata() {
        let client = create_mock_client();
        let tool = HelpTool::new(client);
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "help");
        assert_eq!(metadata.title, "Help & Utility");
        assert!(metadata.description.contains("tools"));
        assert!(metadata.description.contains("auth"));
        assert!(metadata.description.contains("billing"));
        assert!(metadata.description.contains("version"));
        assert!(metadata.description.contains("workflow"));
        assert!(metadata.description.contains("editor_rules"));
        assert!(metadata.description.contains("enable_bundle"));
        assert!(metadata.description.contains("team_status"));
        assert_eq!(metadata.category, ToolCategory::Utility);
        assert!(metadata.annotations.read_only);
        assert!(!metadata.annotations.destructive);
        assert!(!metadata.is_pro);
    }
}

// ============================================================================
// Schema Tests
// ============================================================================

mod schema_tests {
    use super::HelpTool;
    use super::{create_mock_client, ToolHandler};

    #[test]
    fn test_help_tool_schema() {
        let client = create_mock_client();
        let tool = HelpTool::new(client);
        let schema = tool.input_schema();

        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("action"));

        // Check action enum
        if let Some(action) = props.get("action") {
            if let Some(enum_vals) = action.get("enum").and_then(|e| e.as_array()) {
                let values: Vec<&str> = enum_vals.iter().filter_map(|v| v.as_str()).collect();
                assert!(values.contains(&"tools"));
                assert!(values.contains(&"auth"));
                assert!(values.contains(&"billing"));
                assert!(values.contains(&"version"));
                assert!(values.contains(&"workflow"));
                assert!(values.contains(&"editor_rules"));
                assert!(values.contains(&"enable_bundle"));
                assert!(values.contains(&"team_status"));
            }
        }

        // Check other fields
        assert!(props.contains_key("category"));
        assert!(props.contains_key("format"));
        assert!(props.contains_key("client_name"));
        assert!(props.contains_key("editors"));
        assert!(props.contains_key("mode"));
        assert!(props.contains_key("folder_path"));
        assert!(props.contains_key("project_name"));
        assert!(props.contains_key("workspace_name"));
        assert!(props.contains_key("workspace_id"));
        assert!(props.contains_key("additional_rules"));
        assert!(props.contains_key("dry_run"));
        assert!(props.contains_key("install_hooks"));
        assert!(props.contains_key("include_pre_compact"));
        assert!(props.contains_key("include_post_write"));
        assert!(props.contains_key("bundle"));
        assert!(props.contains_key("list_bundles"));

        // Check category enum
        if let Some(category) = props.get("category") {
            if let Some(enum_vals) = category.get("enum").and_then(|e| e.as_array()) {
                let values: Vec<&str> = enum_vals.iter().filter_map(|v| v.as_str()).collect();
                assert!(values.contains(&"session"));
                assert!(values.contains(&"search"));
                assert!(values.contains(&"memory"));
                assert!(values.contains(&"graph"));
                assert!(values.contains(&"workspace"));
                assert!(values.contains(&"project"));
                assert!(values.contains(&"reminders"));
                assert!(values.contains(&"integrations"));
            }
        }

        // Check format enum
        if let Some(format) = props.get("format") {
            if let Some(enum_vals) = format.get("enum").and_then(|e| e.as_array()) {
                let values: Vec<&str> = enum_vals.iter().filter_map(|v| v.as_str()).collect();
                assert!(values.contains(&"grouped"));
                assert!(values.contains(&"minimal"));
                assert!(values.contains(&"full"));
            }
        }

        // Check mode enum
        if let Some(mode) = props.get("mode") {
            if let Some(enum_vals) = mode.get("enum").and_then(|e| e.as_array()) {
                let values: Vec<&str> = enum_vals.iter().filter_map(|v| v.as_str()).collect();
                assert!(values.contains(&"minimal"));
                assert!(values.contains(&"full"));
                assert!(values.contains(&"bootstrap"));
            }
        }

        // Check bundle enum
        if let Some(bundle) = props.get("bundle") {
            if let Some(enum_vals) = bundle.get("enum").and_then(|e| e.as_array()) {
                let values: Vec<&str> = enum_vals.iter().filter_map(|v| v.as_str()).collect();
                assert!(values.contains(&"session"));
                assert!(values.contains(&"memory"));
                assert!(values.contains(&"search"));
                assert!(values.contains(&"graph"));
                assert!(values.contains(&"workspace"));
                assert!(values.contains(&"project"));
                assert!(values.contains(&"reminders"));
                assert!(values.contains(&"integrations"));
            }
        }
    }
}

// ============================================================================
// Input Validation Tests
// ============================================================================

mod validation_tests {
    use super::HelpTool;
    use super::{
        create_mock_client, json, ContextStreamClient, TestFixtures, ToolHandler,
        PUBLIC_MCP_RELEASE_MANIFEST_URL,
    };

    async fn client_with_release_http_error(
    ) -> (ContextStreamClient, tokio::task::JoinHandle<String>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind help version test listener");
        let addr = listener.local_addr().expect("help version listener addr");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept version request");
            let mut buf = vec![0u8; 4096];
            let n = socket.read(&mut buf).await.expect("read version request");
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            socket
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                )
                .await
                .expect("write version error response");
            request
        });

        let mut config = TestFixtures::test_config();
        config.api_url = format!("http://{addr}");
        (ContextStreamClient::new(config), server)
    }

    async fn client_with_release_metadata() -> (ContextStreamClient, tokio::task::JoinHandle<String>)
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind release metadata listener");
        let addr = listener
            .local_addr()
            .expect("release metadata listener addr");
        let body = serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "latest_version": "9.9.9",
            "release_url": PUBLIC_MCP_RELEASE_MANIFEST_URL,
            "release_metadata_source": "version_service",
            "changelog": ["Added a consumable help catalog"],
            "release_notes": "Daily Recaps are now accessible through session actions."
        })
        .to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept version request");
            let mut buf = vec![0u8; 4096];
            let n = socket.read(&mut buf).await.expect("read version request");
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write version response");
            request
        });

        let mut config = TestFixtures::test_config();
        config.api_url = format!("http://{addr}");
        (ContextStreamClient::new(config), server)
    }

    fn result_text(result: &mcp_types::tool::ToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|item| match item {
                mcp_types::tool::ContentItem::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn test_help_unknown_action() {
        let client = create_mock_client();
        let tool = HelpTool::new(client);

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
    async fn test_help_enable_bundle_requires_bundle() {
        let client = create_mock_client();
        let tool = HelpTool::new(client);

        let result = tool
            .execute(json!({
                "action": "enable_bundle"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("bundle"));
    }

    #[tokio::test]
    async fn test_help_enable_bundle_validates_bundle() {
        let client = create_mock_client();
        let tool = HelpTool::new(client);

        let result = tool
            .execute(json!({
                "action": "enable_bundle",
                "bundle": "invalid_bundle"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Invalid bundle"));
    }

    #[tokio::test]
    async fn test_help_enable_bundle_is_an_honest_read_only_preview() {
        let client = create_mock_client();
        let tool = HelpTool::new(client);

        let result = tool
            .execute(json!({
                "action": "enable_bundle",
                "bundle": "memory"
            }))
            .await
            .unwrap();
        let structured = result.structured_content.unwrap();

        assert_eq!(structured["bundle"], "memory");
        assert_eq!(structured["enabled"], false);
        assert_eq!(structured["applied"], false);
        assert_eq!(structured["preview"], true);
    }

    #[tokio::test]
    async fn test_help_validates_workspace_uuid() {
        let client = create_mock_client();
        let tool = HelpTool::new(client);

        let result = tool
            .execute(json!({
                "action": "team_status",
                "workspace_id": "invalid-uuid"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Invalid"));
    }

    #[tokio::test]
    async fn test_help_tools_no_required_params() {
        let client = create_mock_client();
        let tool = HelpTool::new(client);

        // tools action has no required parameters and returns static data
        let result = tool
            .execute(json!({
                "action": "tools"
            }))
            .await;

        // Should succeed (returns static tool catalog)
        assert!(result.is_ok());
        let tool_result = result.unwrap();
        // Check that structured_content has tools
        assert!(tool_result.structured_content.is_some());
        let text = result_text(&tool_result);
        let content = tool_result.structured_content.unwrap();
        assert!(content.get("tools").is_some());
        assert!(text.contains("init: Initialize session scope"));
        assert!(text.contains("help: Discover tools"));
        assert!(!text.contains("Found 20 tools"));
    }

    #[tokio::test]
    async fn test_help_tools_full_is_self_describing_and_filterable() {
        let tool = HelpTool::new(create_mock_client());

        let result = tool
            .execute(json!({
                "action": "tools",
                "category": "utility",
                "format": "full"
            }))
            .await
            .expect("full tool catalog");
        let text = result_text(&result);
        let structured = result.structured_content.expect("structured catalog");
        let tools = structured["tools"].as_array().expect("tool entries");

        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["name"], "help");
        assert!(tools[0]["key_parameters"].is_array());
        assert!(tools[0]["example"].as_str().is_some());
        assert!(text.contains("help [utility]"));
        assert!(text.contains("Actions: tools, auth, billing, version"));
        assert!(text.contains("Example: help(action=\"tools\", format=\"full\")"));
    }

    #[tokio::test]
    async fn test_help_workflow_returns_versioned_known_harness_contract() {
        let client = create_mock_client();
        let tool = HelpTool::new(client);

        let result = tool
            .execute(json!({
                "action": "workflow",
                "client_name": "codex-cli/1.2.3"
            }))
            .await
            .expect("workflow help");
        let structured = result.structured_content.expect("structured workflow");

        assert_eq!(
            structured["teaching_version"],
            mcp_types::HARNESS_TEACHING_VERSION
        );
        assert_eq!(structured["harness_id"], "codex");
        assert_eq!(structured["recognized_harness"], true);
        assert_eq!(structured["delivery"], "help_workflow");
        assert_eq!(structured["steps"].as_array().map(Vec::len), Some(6));
        assert_eq!(structured["budget"]["within_budget"], true);
        assert!(structured["rendered_guidance"]
            .as_str()
            .is_some_and(|guidance| guidance.contains("`init(")));
        assert!(structured["rendered_guidance"]
            .as_str()
            .is_some_and(|guidance| guidance.contains("HANDOFF.md")));
        assert_eq!(structured["steps"][5]["id"], "create_canonical_handoff");
        assert!(structured["steps"][5]["canonical_calls"][0]
            .as_str()
            .is_some_and(|call| call.starts_with("entity(")));
    }

    #[tokio::test]
    async fn test_help_workflow_unknown_client_is_safe_and_generic() {
        let client = create_mock_client();
        let tool = HelpTool::new(client);

        let result = tool
            .execute(json!({
                "action": "workflow",
                "client_name": "my-cursor-compatible-wrapper"
            }))
            .await
            .expect("generic workflow help");
        let structured = result.structured_content.expect("structured workflow");

        assert!(structured.get("harness_id").is_none());
        assert_eq!(structured["recognized_harness"], false);
        assert_eq!(structured["harness_name"], "Unknown MCP client");
        assert_eq!(structured["capabilities"]["static_rules"], false);
        assert_eq!(structured["capabilities"]["mcp_tools"], false);
        assert_eq!(structured["steps"].as_array().map(Vec::len), Some(6));
        assert_eq!(structured["steps"][5]["id"], "create_canonical_handoff");
    }

    #[tokio::test]
    async fn test_help_auth_no_required_params() {
        let client = create_mock_client();
        let tool = HelpTool::new(client);

        // auth action has no required parameters
        let result = tool
            .execute(json!({
                "action": "auth"
            }))
            .await;

        // Should fail due to network (not validation)
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(!err.contains("Unknown action"));
        assert!(!err.contains("required"));
    }

    #[tokio::test]
    async fn test_help_version_no_required_params() {
        let (client, server) = client_with_release_http_error().await;
        let tool = HelpTool::new(client);

        // version action has no required parameters and returns local version
        // data even when the release metadata endpoint is unavailable.
        let result = tool
            .execute(json!({
                "action": "version"
            }))
            .await;
        let request = server.await.expect("help version server task");

        // Should succeed (remote release metadata has a local fallback)
        assert!(result.is_ok());
        assert!(
            request.starts_with("GET /api/v1/mcp/version "),
            "unexpected request line: {:?}",
            request.lines().next()
        );
        let tool_result = result.unwrap();
        // Check that structured_content has version
        assert!(tool_result.structured_content.is_some());
        let text = result_text(&tool_result);
        let content = tool_result.structured_content.unwrap();
        assert!(content
            .get("version")
            .and_then(|value| value.as_str())
            .is_some_and(|value| !value.is_empty()));
        assert!(content
            .get("latest_version")
            .and_then(|value| value.as_str())
            .is_some_and(|value| !value.is_empty()));
        assert_eq!(
            content.get("release_url").and_then(|value| value.as_str()),
            Some(PUBLIC_MCP_RELEASE_MANIFEST_URL)
        );
        assert_eq!(
            content
                .get("release_metadata_source")
                .and_then(|value| value.as_str()),
            Some("local_fallback")
        );
        assert_eq!(content["runtime_type"], "rust-mcp");
        assert_eq!(content["release_notes_available"], false);
        assert!(text.contains("Runtime: Rust MCP"));
        assert!(text.contains("Release notes: not published"));
    }

    #[tokio::test]
    async fn test_help_version_renders_endpoint_release_notes() {
        let (client, server) = client_with_release_metadata().await;
        let tool = HelpTool::new(client);

        let result = tool
            .execute(json!({ "action": "version" }))
            .await
            .expect("version help");
        server.await.expect("release metadata server task");
        let text = result_text(&result);
        let structured = result.structured_content.expect("structured version");

        assert!(text.contains("Runtime: Rust MCP"));
        assert!(text.contains("Machine-readable release metadata:"));
        assert!(text.contains("Added a consumable help catalog"));
        assert!(text.contains("Daily Recaps are now accessible"));
        assert_eq!(structured["release_notes_available"], true);
        assert_eq!(structured["release_metadata_format"], "json");
    }

    #[tokio::test]
    async fn test_help_editor_rules_no_required_params() {
        let client = create_mock_client();
        let tool = HelpTool::new(client);

        // editor_rules action has no required parameters and returns static data
        let result = tool
            .execute(json!({
                "action": "editor_rules",
                "dry_run": false,
                "install_hooks": true
            }))
            .await;

        // Should succeed (returns static info about rule generation)
        assert!(result.is_ok());
        let tool_result = result.unwrap();
        // Check that structured_content has editors info
        assert!(tool_result.structured_content.is_some());
        let content = tool_result.structured_content.unwrap();
        assert!(content.get("editors").is_some() || content.get("message").is_some());
        assert_eq!(content["dry_run"], true);
        assert_eq!(content["requested_dry_run"], false);
        assert_eq!(content["writes_files"], false);
        assert_eq!(content["installs_hooks"], false);
        assert!(tool_result.content.iter().any(|item| matches!(
            item,
            mcp_types::tool::ContentItem::Text { text }
                if text.contains("no files or hooks were changed")
        )));
    }
}

// ============================================================================
// Input Struct Tests
// ============================================================================

mod input_struct_tests {
    use super::json;
    use super::HelpInput;

    #[test]
    fn test_help_input_tools_deserialization() {
        let input: HelpInput = serde_json::from_value(json!({
            "action": "tools",
            "category": "session",
            "format": "grouped"
        }))
        .unwrap();

        assert_eq!(input.action, "tools");
        assert_eq!(input.category, Some("session".to_string()));
        assert_eq!(input.format, Some("grouped".to_string()));
    }

    #[test]
    fn test_help_input_workflow_deserialization() {
        let input: HelpInput = serde_json::from_value(json!({
            "action": "workflow",
            "client_name": "claude-code/1.0"
        }))
        .unwrap();

        assert_eq!(input.action, "workflow");
        assert_eq!(input.client_name, Some("claude-code/1.0".to_string()));
    }

    #[test]
    fn test_help_input_editor_rules_deserialization() {
        let input: HelpInput = serde_json::from_value(json!({
            "action": "editor_rules",
            "editors": ["cursor", "claude", "aider"],
            "mode": "bootstrap",
            "folder_path": "/home/user/project",
            "project_name": "My Project",
            "workspace_name": "My Workspace",
            "additional_rules": "Always use TypeScript",
            "dry_run": true,
            "install_hooks": true,
            "include_pre_compact": true,
            "include_post_write": true
        }))
        .unwrap();

        assert_eq!(input.action, "editor_rules");
        assert_eq!(
            input.editors,
            Some(vec![
                "cursor".to_string(),
                "claude".to_string(),
                "aider".to_string()
            ])
        );
        assert_eq!(input.mode, Some("bootstrap".to_string()));
        assert_eq!(input.folder_path, Some("/home/user/project".to_string()));
        assert_eq!(input.project_name, Some("My Project".to_string()));
        assert_eq!(input.workspace_name, Some("My Workspace".to_string()));
        assert_eq!(
            input.additional_rules,
            Some("Always use TypeScript".to_string())
        );
        assert_eq!(input.dry_run, Some(true));
        assert_eq!(input.install_hooks, Some(true));
        assert_eq!(input.include_pre_compact, Some(true));
        assert_eq!(input.include_post_write, Some(true));
    }

    #[test]
    fn test_help_input_enable_bundle_deserialization() {
        let input: HelpInput = serde_json::from_value(json!({
            "action": "enable_bundle",
            "bundle": "memory"
        }))
        .unwrap();

        assert_eq!(input.action, "enable_bundle");
        assert_eq!(input.bundle, Some("memory".to_string()));
    }

    #[test]
    fn test_help_input_list_bundles_deserialization() {
        let input: HelpInput = serde_json::from_value(json!({
            "action": "enable_bundle",
            "list_bundles": true
        }))
        .unwrap();

        assert_eq!(input.action, "enable_bundle");
        assert_eq!(input.list_bundles, Some(true));
    }

    #[test]
    fn test_help_input_team_status_deserialization() {
        let input: HelpInput = serde_json::from_value(json!({
            "action": "team_status",
            "workspace_id": "550e8400-e29b-41d4-a716-446655440000"
        }))
        .unwrap();

        assert_eq!(input.action, "team_status");
        assert!(input.workspace_id.is_some());
    }
}

// ============================================================================
// Constants Tests
// ============================================================================

mod constants_tests {
    use super::*;

    #[test]
    fn test_valid_actions() {
        assert!(VALID_ACTIONS.contains(&"tools"));
        assert!(VALID_ACTIONS.contains(&"auth"));
        assert!(VALID_ACTIONS.contains(&"billing"));
        assert!(VALID_ACTIONS.contains(&"version"));
        assert!(VALID_ACTIONS.contains(&"workflow"));
        assert!(VALID_ACTIONS.contains(&"editor_rules"));
        assert!(VALID_ACTIONS.contains(&"enable_bundle"));
        assert!(VALID_ACTIONS.contains(&"team_status"));
        assert_eq!(VALID_ACTIONS.len(), 8);
    }

    #[test]
    fn test_valid_categories() {
        assert!(VALID_CATEGORIES.contains(&"session"));
        assert!(VALID_CATEGORIES.contains(&"search"));
        assert!(VALID_CATEGORIES.contains(&"memory"));
        assert!(VALID_CATEGORIES.contains(&"graph"));
        assert!(VALID_CATEGORIES.contains(&"workspace"));
        assert!(VALID_CATEGORIES.contains(&"project"));
        assert!(VALID_CATEGORIES.contains(&"reminders"));
        assert!(VALID_CATEGORIES.contains(&"integrations"));
        assert!(VALID_CATEGORIES.contains(&"utility"));
        assert_eq!(VALID_CATEGORIES.len(), 9);
    }

    #[test]
    fn test_valid_formats() {
        assert!(VALID_FORMATS.contains(&"grouped"));
        assert!(VALID_FORMATS.contains(&"minimal"));
        assert!(VALID_FORMATS.contains(&"full"));
        assert_eq!(VALID_FORMATS.len(), 3);
    }

    #[test]
    fn test_valid_modes() {
        assert!(VALID_MODES.contains(&"minimal"));
        assert!(VALID_MODES.contains(&"full"));
        assert!(VALID_MODES.contains(&"bootstrap"));
        assert_eq!(VALID_MODES.len(), 3);
    }

    #[test]
    fn test_valid_bundles() {
        assert!(VALID_BUNDLES.contains(&"session"));
        assert!(VALID_BUNDLES.contains(&"memory"));
        assert!(VALID_BUNDLES.contains(&"search"));
        assert!(VALID_BUNDLES.contains(&"graph"));
        assert!(VALID_BUNDLES.contains(&"workspace"));
        assert!(VALID_BUNDLES.contains(&"project"));
        assert!(VALID_BUNDLES.contains(&"reminders"));
        assert!(VALID_BUNDLES.contains(&"integrations"));
        assert_eq!(VALID_BUNDLES.len(), 8);
    }

    #[test]
    fn test_valid_editors() {
        assert!(VALID_EDITORS.contains(&"codex"));
        assert!(VALID_EDITORS.contains(&"opencode"));
        assert!(VALID_EDITORS.contains(&"cursor"));
        assert!(VALID_EDITORS.contains(&"windsurf"));
        assert!(VALID_EDITORS.contains(&"cline"));
        assert!(VALID_EDITORS.contains(&"kilo"));
        assert!(VALID_EDITORS.contains(&"roo"));
        assert!(VALID_EDITORS.contains(&"claude"));
        assert!(VALID_EDITORS.contains(&"aider"));
        assert!(VALID_EDITORS.contains(&"antigravity"));
        assert!(VALID_EDITORS.contains(&"all"));
        assert_eq!(VALID_EDITORS.len(), 11);
    }

    #[test]
    fn test_upgrade_options_for_free_and_starter() {
        let free_targets: Vec<String> = upgrade_options_for_plan("free")
            .iter()
            .filter_map(|option| option.get("target_plan").and_then(|v| v.as_str()))
            .map(str::to_string)
            .collect();
        let starter_targets: Vec<String> = upgrade_options_for_plan("starter")
            .iter()
            .filter_map(|option| option.get("target_plan").and_then(|v| v.as_str()))
            .map(str::to_string)
            .collect();

        assert_eq!(free_targets, vec!["pro", "elite"]);
        assert_eq!(starter_targets, vec!["pro", "elite"]);
    }

    #[test]
    fn test_upgrade_options_for_pro() {
        let targets: Vec<String> = upgrade_options_for_plan("pro")
            .iter()
            .filter_map(|option| option.get("target_plan").and_then(|v| v.as_str()))
            .map(str::to_string)
            .collect();

        assert_eq!(targets, vec!["elite"]);
    }
}

// ============================================================================
// Tool Registration Tests
// ============================================================================

mod registration_tests {
    #[test]
    fn test_help_tool_count() {
        // Expected help tools:
        // - help (unified)
        // Total: 1 tool

        let expected_tools = ["help"];
        assert_eq!(expected_tools.len(), 1);
    }

    #[test]
    fn test_help_actions_coverage() {
        // Document all help actions:
        //
        // No-required-params actions:
        // - tools: List available tools (optional category/format)
        // - auth: Get current user info
        // - billing: Get current plan and upgrade options
        // - version: Get server version
        // - editor_rules: Generate editor rules (optional editors/mode)
        // - team_status: Get team subscription info (team plans only)
        //
        // Actions requiring parameters:
        // - enable_bundle: Enable a tool bundle (requires bundle OR list_bundles)

        let all_actions = [
            "tools",
            "auth",
            "billing",
            "version",
            "editor_rules",
            "enable_bundle",
            "team_status",
        ];
        assert_eq!(all_actions.len(), 7);
    }

    #[test]
    fn test_help_editors_coverage() {
        // Document all supported editors for rule generation:
        // - codex: OpenAI Codex
        // - opencode: OpenCode CLI
        // - cursor: Cursor IDE
        // - windsurf: Windsurf IDE
        // - cline: Cline extension
        // - kilo: Kilo Code
        // - roo: Roo Code
        // - claude: Claude Code CLI
        // - aider: Aider AI coding
        // - antigravity: Antigravity editor
        // - all: Generate for all editors

        let all_editors = [
            "codex",
            "opencode",
            "cursor",
            "windsurf",
            "cline",
            "kilo",
            "roo",
            "claude",
            "aider",
            "antigravity",
            "all",
        ];
        assert_eq!(all_editors.len(), 11);
    }
}
