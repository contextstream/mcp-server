//! Test fixtures for common test scenarios.

use mcp_types::Config;
use serde_json::{json, Value};
use uuid::Uuid;

/// Test fixtures for common test data.
pub struct TestFixtures;

impl TestFixtures {
    // =========================================================================
    // IDs
    // =========================================================================

    /// Generate a random workspace ID.
    pub fn workspace_id() -> String {
        Uuid::new_v4().to_string()
    }

    /// Generate a random project ID.
    pub fn project_id() -> String {
        Uuid::new_v4().to_string()
    }

    /// Generate a random session ID.
    pub fn session_id() -> String {
        format!("session-{}", Uuid::new_v4())
    }

    /// Generate a random event ID.
    pub fn event_id() -> String {
        Uuid::new_v4().to_string()
    }

    // =========================================================================
    // API Responses
    // =========================================================================

    /// Mock user response from /me endpoint.
    pub fn user_response() -> Value {
        json!({
            "id": Uuid::new_v4().to_string(),
            "email": "test@example.com",
            "name": "Test User",
            "created_at": "2024-01-01T00:00:00Z"
        })
    }

    /// Mock workspace response.
    pub fn workspace_response() -> Value {
        json!({
            "id": Self::workspace_id(),
            "name": "Test Workspace",
            "description": "A test workspace",
            "visibility": "private",
            "created_at": "2024-01-01T00:00:00Z",
            "updated_at": "2024-01-01T00:00:00Z"
        })
    }

    /// Mock workspace list response.
    pub fn workspaces_response(count: usize) -> Value {
        let workspaces: Vec<Value> = (0..count)
            .map(|i| {
                json!({
                    "id": Self::workspace_id(),
                    "name": format!("Workspace {}", i + 1),
                    "description": format!("Description for workspace {}", i + 1),
                    "visibility": "private",
                    "created_at": "2024-01-01T00:00:00Z"
                })
            })
            .collect();

        json!({
            "workspaces": workspaces,
            "total": count
        })
    }

    /// Mock project response.
    pub fn project_response() -> Value {
        json!({
            "id": Self::project_id(),
            "workspace_id": Self::workspace_id(),
            "name": "Test Project",
            "description": "A test project",
            "created_at": "2024-01-01T00:00:00Z",
            "updated_at": "2024-01-01T00:00:00Z"
        })
    }

    /// Mock project list response.
    pub fn projects_response(count: usize) -> Value {
        let workspace_id = Self::workspace_id();
        let projects: Vec<Value> = (0..count)
            .map(|i| {
                json!({
                    "id": Self::project_id(),
                    "workspace_id": workspace_id,
                    "name": format!("Project {}", i + 1),
                    "description": format!("Description for project {}", i + 1),
                    "created_at": "2024-01-01T00:00:00Z"
                })
            })
            .collect();

        json!({
            "projects": projects,
            "total": count
        })
    }

    /// Mock init response.
    pub fn init_response() -> Value {
        json!({
            "session_id": Self::session_id(),
            "workspace": Self::workspace_response(),
            "project": Self::project_response(),
            "recent_memory": [],
            "recent_decisions": [],
            "high_priority_lessons": [],
            "ingest_recommendation": null
        })
    }

    /// Mock context response.
    pub fn context_response() -> Value {
        json!({
            "context": "W:Test|P:test-project|D:Use Rust for MCP|M:Session initialized",
            "token_estimate": 50,
            "format": "minified",
            "sources_used": 3,
            "workspace_id": Self::workspace_id(),
            "project_id": Self::project_id()
        })
    }

    /// Mock search response.
    pub fn search_response(count: usize) -> Value {
        let results: Vec<Value> = (0..count)
            .map(|i| {
                json!({
                    "id": Self::event_id(),
                    "title": format!("Search Result {}", i + 1),
                    "content": format!("Content for result {}", i + 1),
                    "score": 0.9 - (i as f64 * 0.1),
                    "event_type": "decision"
                })
            })
            .collect();

        json!({
            "results": results,
            "total": count,
            "query": "test query"
        })
    }

    /// Mock memory event response.
    pub fn event_response() -> Value {
        json!({
            "id": Self::event_id(),
            "workspace_id": Self::workspace_id(),
            "project_id": Self::project_id(),
            "event_type": "decision",
            "title": "Test Decision",
            "content": "We decided to use Rust for the MCP server.",
            "tags": ["rust", "architecture"],
            "created_at": "2024-01-01T00:00:00Z",
            "updated_at": "2024-01-01T00:00:00Z"
        })
    }

    /// Mock events list response.
    pub fn events_response(count: usize) -> Value {
        let events: Vec<Value> = (0..count)
            .map(|i| {
                json!({
                    "id": Self::event_id(),
                    "event_type": "decision",
                    "title": format!("Event {}", i + 1),
                    "content": format!("Content for event {}", i + 1),
                    "created_at": "2024-01-01T00:00:00Z"
                })
            })
            .collect();

        json!({
            "events": events,
            "total": count
        })
    }

    /// Mock graph dependencies response.
    pub fn graph_dependencies_response() -> Value {
        json!({
            "dependencies": [
                {"id": "mod-1", "name": "mcp_types", "type": "module"},
                {"id": "mod-2", "name": "mcp_client", "type": "module"},
                {"id": "mod-3", "name": "serde", "type": "external"}
            ],
            "target": {"id": "mod-0", "name": "mcp_tools", "type": "module"}
        })
    }

    /// Mock reminder response.
    pub fn reminder_response() -> Value {
        json!({
            "id": Uuid::new_v4().to_string(),
            "title": "Test Reminder",
            "content": "Remember to review the PR",
            "remind_at": "2024-12-01T10:00:00Z",
            "status": "pending",
            "priority": "normal",
            "created_at": "2024-01-01T00:00:00Z"
        })
    }

    /// Mock integration status response.
    pub fn integration_status_response(provider: &str) -> Value {
        json!({
            "provider": provider,
            "connected": true,
            "last_sync": "2024-01-01T00:00:00Z",
            "scopes": ["read", "write"]
        })
    }

    /// Mock plan response.
    pub fn plan_response() -> Value {
        json!({
            "id": Uuid::new_v4().to_string(),
            "title": "Test Plan",
            "description": "A test implementation plan",
            "status": "active",
            "progress": 0.5,
            "steps": [
                {"id": "1", "title": "Step 1", "order": 1, "estimated_effort": "small"},
                {"id": "2", "title": "Step 2", "order": 2, "estimated_effort": "medium"}
            ],
            "created_at": "2024-01-01T00:00:00Z"
        })
    }

    // =========================================================================
    // Configs
    // =========================================================================

    /// Create a test config with mock values.
    pub fn test_config() -> Config {
        Config {
            api_url: "https://mock.contextstream.io".to_string(),
            api_key: Some("test-api-key".to_string()),
            jwt: None,
            default_workspace_id: Some(Uuid::new_v4()),
            default_project_id: Some(Uuid::new_v4()),
            user_agent: "mcp-test/0.1.0".to_string(),
            allow_header_auth: false,
            context_pack_enabled: false,
            show_timing: false,
            toolset: mcp_types::config::Toolset::Complete,
            log_level: mcp_types::config::LogLevel::Quiet,
            output_format: mcp_types::config::OutputFormat::Compact,
            progressive_mode: false,
            router_mode: false,
            consolidated_mode: false,
            auto_hide_integrations: false,
            capsule_enabled: false,
            search_limit: 10,
            search_max_chars: 500,
            transcripts_enabled: false,
            hook_transcripts_enabled: false,
            tool_surface_profile: mcp_types::config::ToolSurfaceProfile::Default,
            is_http_transport: false,
            account_mode_preference: Default::default(),
        }
    }

    /// Create a test config for progressive mode.
    pub fn progressive_config() -> Config {
        Config {
            progressive_mode: true,
            ..Self::test_config()
        }
    }

    /// Create a test config for router mode.
    pub fn router_config() -> Config {
        Config {
            router_mode: true,
            ..Self::test_config()
        }
    }

    /// Create a test config for consolidated mode.
    pub fn consolidated_config() -> Config {
        Config {
            consolidated_mode: true,
            ..Self::test_config()
        }
    }

    // =========================================================================
    // Tool Inputs
    // =========================================================================

    /// Sample init tool input.
    pub fn init_input() -> Value {
        json!({
            "folder_path": "/test/project",
            "context_hint": "starting new session"
        })
    }

    /// Sample context tool input.
    pub fn context_input() -> Value {
        json!({
            "user_message": "How do I implement authentication?",
            "format": "minified"
        })
    }

    /// Sample search tool input.
    pub fn search_input() -> Value {
        json!({
            "mode": "hybrid",
            "query": "authentication implementation"
        })
    }

    /// Sample session capture input.
    pub fn session_capture_input() -> Value {
        json!({
            "action": "capture",
            "event_type": "decision",
            "title": "Use JWT for auth",
            "content": "We decided to use JWT tokens for authentication."
        })
    }

    /// Sample memory create event input.
    pub fn memory_create_event_input() -> Value {
        json!({
            "action": "create_event",
            "event_type": "decision",
            "title": "Test Decision",
            "content": "Test content for the decision."
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fixtures_generate_unique_ids() {
        let id1 = TestFixtures::workspace_id();
        let id2 = TestFixtures::workspace_id();
        assert_ne!(id1, id2);
    }

    #[test]
    fn test_fixtures_create_valid_responses() {
        let user = TestFixtures::user_response();
        assert!(user.get("id").is_some());
        assert!(user.get("email").is_some());

        let workspace = TestFixtures::workspace_response();
        assert!(workspace.get("id").is_some());
        assert!(workspace.get("name").is_some());
    }

    #[test]
    fn test_fixtures_list_responses() {
        let workspaces = TestFixtures::workspaces_response(5);
        assert_eq!(workspaces["workspaces"].as_array().unwrap().len(), 5);
        assert_eq!(workspaces["total"], 5);
    }

    #[test]
    fn test_config_fixtures() {
        let config = TestFixtures::test_config();
        assert!(config.api_key.is_some());
        assert!(!config.progressive_mode);

        let prog_config = TestFixtures::progressive_config();
        assert!(prog_config.progressive_mode);
    }
}
