//! Integration tests for the MCP server.
//!
//! These tests require a valid ContextStream API key and network access.
//! They are ignored by default and can be run with:
//!
//! ```bash
//! CONTEXTSTREAM_API_KEY="your_key" cargo test --test integration_tests -- --ignored
//! ```
//!
//! For non-production environments, set CONTEXTSTREAM_API_URL:
//! ```bash
//! CONTEXTSTREAM_API_URL="https://your-api-url" \
//! CONTEXTSTREAM_API_KEY="your_key" \
//! cargo test --test integration_tests -- --ignored
//! ```

use std::sync::{Arc, OnceLock};
use uuid::Uuid;

// ============================================================================
// Test Helpers
// ============================================================================

/// Check if API credentials are available.
fn has_api_credentials() -> bool {
    std::env::var("CONTEXTSTREAM_API_KEY").is_ok() || std::env::var("CONTEXTSTREAM_JWT").is_ok()
}

fn env_uuid(name: &str) -> Option<Uuid> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .and_then(|value| Uuid::parse_str(&value).ok())
}

fn env_scope_string(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn with_scope(mut input: serde_json::Value) -> serde_json::Value {
    if let Some(workspace_id) = env_scope_string("CONTEXTSTREAM_WORKSPACE_ID") {
        input["workspace_id"] = serde_json::json!(workspace_id);
    }
    input
}

fn parse_bool_env(name: &str) -> bool {
    std::env::var(name)
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

fn should_run_search_integration_tests() -> bool {
    if parse_bool_env("CONTEXTSTREAM_RUN_SEARCH_INTEGRATION_TESTS") {
        return true;
    }

    !std::env::var("CONTEXTSTREAM_API_URL")
        .unwrap_or_default()
        .contains("localhost")
}

fn api_test_mutex() -> &'static tokio::sync::Mutex<()> {
    static API_TEST_MUTEX: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    API_TEST_MUTEX.get_or_init(|| tokio::sync::Mutex::new(()))
}

async fn lock_api_test() -> tokio::sync::MutexGuard<'static, ()> {
    api_test_mutex().lock().await
}

/// Skip test if no credentials available.
macro_rules! require_credentials {
    () => {
        if !has_api_credentials() {
            eprintln!("Skipping test: No API credentials available");
            eprintln!("Set CONTEXTSTREAM_API_KEY or CONTEXTSTREAM_JWT to run this test");
            return;
        }
    };
}

macro_rules! require_search_backend {
    () => {
        if !should_run_search_integration_tests() {
            eprintln!("Skipping search integration test: local search backend is opt-in");
            eprintln!("Set CONTEXTSTREAM_RUN_SEARCH_INTEGRATION_TESTS=true to force it");
            return;
        }
    };
}

/// Create a test client with credentials from environment.
fn create_test_client() -> mcp_client::ContextStreamClient {
    let config = mcp_types::config::Config {
        api_url: std::env::var("CONTEXTSTREAM_API_URL")
            .unwrap_or_else(|_| "https://api.contextstream.io".to_string()),
        api_key: std::env::var("CONTEXTSTREAM_API_KEY").ok(),
        jwt: std::env::var("CONTEXTSTREAM_JWT").ok(),
        default_workspace_id: env_uuid("CONTEXTSTREAM_WORKSPACE_ID"),
        ..Default::default()
    };
    mcp_client::ContextStreamClient::new(config)
}

/// Create a test tool registry with all tools registered.
fn create_test_registry() -> mcp_tools::ToolRegistry {
    let config = mcp_types::config::Config {
        api_url: std::env::var("CONTEXTSTREAM_API_URL")
            .unwrap_or_else(|_| "https://api.contextstream.io".to_string()),
        api_key: std::env::var("CONTEXTSTREAM_API_KEY").ok(),
        jwt: std::env::var("CONTEXTSTREAM_JWT").ok(),
        default_workspace_id: env_uuid("CONTEXTSTREAM_WORKSPACE_ID"),
        consolidated_mode: true,
        ..Default::default()
    };

    let client = mcp_client::ContextStreamClient::new(config.clone());
    let session = Arc::new(mcp_session::SessionManager::new(
        client.clone(),
        config.clone(),
    ));

    let mut registry = mcp_tools::ToolRegistry::new(&config);

    // Register all domain tools
    let index_keeper = Arc::new(mcp_tools::domains::index_keeper::IndexKeeper::new(
        client.clone(),
        session.clone(),
        mcp_types::atlas_layer::noop_layer(),
        mcp_types::acceleration_layer::noop_acceleration_layer(),
    ));
    mcp_tools::domains::session::register_session_tools(
        &mut registry,
        client.clone(),
        session.clone(),
        index_keeper.clone(),
    );
    mcp_tools::domains::search::register_search_tools(
        &mut registry,
        client.clone(),
        session.clone(),
        index_keeper,
    );
    mcp_tools::domains::memory::register_memory_tools(
        &mut registry,
        client.clone(),
        session.clone(),
    );
    mcp_tools::domains::graph::register_graph_tools(&mut registry, client.clone(), session.clone());
    mcp_tools::domains::workspace::register_workspace_tools(&mut registry, client.clone());
    mcp_tools::domains::project::register_project_tools(
        &mut registry,
        client.clone(),
        session.clone(),
    );
    mcp_tools::domains::integrations::register_integration_tools(
        &mut registry,
        client.clone(),
        session.clone(),
    );
    mcp_tools::domains::reminder::register_reminder_tools(&mut registry, client.clone());
    mcp_tools::domains::coordination::register_coordination_tools(&mut registry, client.clone());
    mcp_tools::domains::media::register_media_tools(&mut registry, client.clone(), session.clone());
    mcp_tools::domains::help::register_help_tools(&mut registry, client.clone());

    registry
}

// ============================================================================
// Authentication Tests
// ============================================================================

mod auth_tests {
    use super::*;

    #[tokio::test]
    #[ignore = "Requires API credentials"]
    async fn test_api_key_authentication() {
        require_credentials!();

        let client = create_test_client();
        let result = client.me().await;

        assert!(result.is_ok(), "API key authentication should succeed");
        let user = result.unwrap();
        assert!(!user.email.is_empty(), "User email should be present");
    }

    #[tokio::test]
    #[ignore = "Requires API credentials"]
    async fn test_invalid_api_key() {
        let config = mcp_types::config::Config {
            api_key: Some("invalid_key_12345".to_string()),
            ..Default::default()
        };
        let client = mcp_client::ContextStreamClient::new(config);

        let result = client.me().await;
        assert!(result.is_err(), "Invalid API key should fail");
    }
}

// ============================================================================
// Session Lifecycle Tests
// ============================================================================

mod session_tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    #[ignore = "Requires API credentials"]
    async fn test_session_init() {
        require_credentials!();

        let registry = create_test_registry();

        let result = registry.execute("init", json!({})).await;

        assert!(result.is_ok(), "Session init should succeed");
        let response = result.unwrap();
        assert!(!response.is_error, "Response should not be an error");
    }

    #[tokio::test]
    #[ignore = "Requires API credentials"]
    async fn test_session_context() {
        require_credentials!();
        let _guard = lock_api_test().await;

        let registry = create_test_registry();

        // First init
        let _ = registry.execute("init", with_scope(json!({}))).await;

        // Then get context
        let result = registry
            .execute(
                "context",
                with_scope(json!({
                    "user_message": "test message for context",
                    "mode": "fast"
                })),
            )
            .await;

        assert!(result.is_ok(), "Session context should succeed");
    }

    #[tokio::test]
    #[ignore = "Requires API credentials"]
    async fn test_session_capture_and_recall() {
        require_credentials!();
        require_search_backend!();
        let _guard = lock_api_test().await;

        let registry = create_test_registry();
        let marker = Uuid::new_v4().to_string();

        // Init session
        let _ = registry.execute("init", with_scope(json!({}))).await;

        // Capture a decision
        let capture_result = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            registry.execute(
                "session",
                with_scope(json!({
                    "action": "capture",
                    "event_type": "decision",
                    "title": format!("Integration test decision {marker}"),
                    "content": format!("This is a test decision for integration testing marker {marker}")
                })),
            ),
        )
        .await;

        assert!(capture_result.is_ok(), "Capture timed out");
        let capture_result = capture_result.unwrap();
        assert!(capture_result.is_ok(), "Capture should succeed");

        // Recall the decision
        let recall_result = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            registry.execute(
                "session",
                with_scope(json!({
                    "action": "recall",
                    "query": marker
                })),
            ),
        )
        .await;

        assert!(recall_result.is_ok(), "Recall timed out");
        let recall_result = recall_result.unwrap();
        assert!(recall_result.is_ok(), "Recall should succeed");
    }

    #[tokio::test]
    #[ignore = "Requires API credentials"]
    async fn test_full_session_workflow() {
        require_credentials!();
        let _guard = lock_api_test().await;

        let registry = create_test_registry();

        // 1. Initialize session
        let init_result = registry.execute("init", with_scope(json!({}))).await;
        assert!(init_result.is_ok(), "Init should succeed");

        // 2. Get context for a message
        let context_result = registry
            .execute(
                "context",
                with_scope(json!({
                    "user_message": "How do I implement authentication?",
                    "mode": "fast"
                })),
            )
            .await;
        assert!(context_result.is_ok(), "Context should succeed");

        // 3. Capture a note
        let capture_result = registry
            .execute(
                "session",
                json!({
                    "action": "remember",
                    "content": "User is working on authentication feature"
                }),
            )
            .await;
        assert!(capture_result.is_ok(), "Remember should succeed");

        // 4. Get summary
        let summary_result = registry
            .execute(
                "session",
                with_scope(json!({
                    "action": "summary"
                })),
            )
            .await;
        assert!(summary_result.is_ok(), "Summary should succeed");
    }
}

// ============================================================================
// Multi-Tool Workflow Tests
// ============================================================================

mod workflow_tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    #[ignore = "Requires API credentials"]
    async fn test_workspace_and_project_workflow() {
        require_credentials!();

        let registry = create_test_registry();

        // List workspaces
        let ws_result = registry
            .execute(
                "workspace",
                json!({
                    "action": "list"
                }),
            )
            .await;
        assert!(ws_result.is_ok(), "Workspace list should succeed");

        // List projects
        let proj_result = registry
            .execute(
                "project",
                json!({
                    "action": "list"
                }),
            )
            .await;
        assert!(proj_result.is_ok(), "Project list should succeed");
    }

    #[tokio::test]
    #[ignore = "Requires API credentials"]
    async fn test_memory_crud_workflow() {
        require_credentials!();

        let registry = create_test_registry();

        // Create an event
        let create_result = registry
            .execute(
                "memory",
                with_scope(json!({
                    "action": "create_event",
                    "event_type": "note",
                    "title": "Integration test note",
                    "content": "This is a test note created during integration testing"
                })),
            )
            .await;
        assert!(create_result.is_ok(), "Create event should succeed");

        // List events
        let list_result = registry
            .execute(
                "memory",
                with_scope(json!({
                    "action": "list_events",
                    "limit": 10
                })),
            )
            .await;
        assert!(list_result.is_ok(), "List events should succeed");
    }

    #[tokio::test]
    #[ignore = "Requires API credentials"]
    async fn test_search_workflow() {
        require_credentials!();
        require_search_backend!();
        let _guard = lock_api_test().await;

        let registry = create_test_registry();

        // Keyword search
        let search_result = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            registry.execute(
                "search",
                json!({
                    "mode": "keyword",
                    "query": "authentication",
                    "limit": 5
                }),
            ),
        )
        .await;
        assert!(search_result.is_ok(), "Keyword search timed out");
        let search_result = search_result.unwrap();
        assert!(search_result.is_ok(), "Keyword search should succeed");

        // Hybrid search
        let hybrid_result = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            registry.execute(
                "search",
                json!({
                    "mode": "hybrid",
                    "query": "authentication",
                    "limit": 5
                }),
            ),
        )
        .await;
        assert!(hybrid_result.is_ok(), "Hybrid search timed out");
        let hybrid_result = hybrid_result.unwrap();
        assert!(hybrid_result.is_ok(), "Hybrid search should succeed");
    }
}

// ============================================================================
// Error Handling Tests
// ============================================================================

mod error_tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    #[ignore = "Requires API credentials"]
    async fn test_invalid_workspace_id() {
        require_credentials!();

        let registry = create_test_registry();

        let result = registry
            .execute(
                "workspace",
                json!({
                    "action": "get",
                    "workspace_id": "00000000-0000-0000-0000-000000000000"
                }),
            )
            .await;

        assert!(result.is_err(), "Invalid workspace lookup should fail");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("not found") || err.contains("Not found"),
            "expected not found error, got: {}",
            err
        );
    }

    #[tokio::test]
    #[ignore = "Requires API credentials"]
    async fn test_missing_required_parameter() {
        require_credentials!();

        let registry = create_test_registry();

        // search requires query for most modes
        let result = registry
            .execute(
                "search",
                json!({
                    "mode": "semantic"
                    // missing query
                }),
            )
            .await;

        assert!(result.is_err(), "Missing required parameter should fail");
    }
}

// ============================================================================
// Help Tool Tests
// ============================================================================

mod help_tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    #[ignore = "Requires API credentials"]
    async fn test_help_tools_list() {
        require_credentials!();

        let registry = create_test_registry();

        let result = registry
            .execute(
                "help",
                json!({
                    "action": "tools"
                }),
            )
            .await;

        assert!(result.is_ok(), "Help tools should succeed");
    }

    #[tokio::test]
    #[ignore = "Requires API credentials"]
    async fn test_help_auth() {
        require_credentials!();

        let registry = create_test_registry();

        let result = registry
            .execute(
                "help",
                json!({
                    "action": "auth"
                }),
            )
            .await;

        assert!(result.is_ok(), "Help auth should succeed");
    }

    #[tokio::test]
    #[ignore = "Requires API credentials"]
    async fn test_help_version() {
        require_credentials!();

        let registry = create_test_registry();

        let result = registry
            .execute(
                "help",
                json!({
                    "action": "version"
                }),
            )
            .await;

        assert!(result.is_ok(), "Help version should succeed");
    }
}

// ============================================================================
// Performance Tests
// ============================================================================

mod performance_tests {
    use super::*;
    use serde_json::json;
    use std::time::Instant;

    #[tokio::test]
    #[ignore = "Requires API credentials"]
    async fn test_init_latency() {
        require_credentials!();

        let registry = create_test_registry();

        let start = Instant::now();
        let result = registry.execute("init", with_scope(json!({}))).await;
        let duration = start.elapsed();

        assert!(result.is_ok(), "Init should succeed");
        eprintln!("Init latency: {:?}", duration);

        // Warn if latency is high (but don't fail)
        if duration.as_millis() > 2000 {
            eprintln!("WARNING: Init latency is high (>2s)");
        }
    }

    #[tokio::test]
    #[ignore = "Requires API credentials"]
    async fn test_context_latency() {
        require_credentials!();
        let _guard = lock_api_test().await;

        let registry = create_test_registry();

        // Init first
        let _ = registry.execute("init", with_scope(json!({}))).await;

        let start = Instant::now();
        let result = registry
            .execute(
                "context",
                with_scope(json!({
                    "user_message": "test message",
                    "mode": "fast"
                })),
            )
            .await;
        let duration = start.elapsed();

        assert!(result.is_ok(), "Context should succeed");
        eprintln!("Context latency: {:?}", duration);
    }

    #[tokio::test]
    #[ignore = "Requires API credentials"]
    async fn test_search_latency() {
        require_credentials!();
        require_search_backend!();
        let _guard = lock_api_test().await;

        let registry = create_test_registry();

        let start = Instant::now();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            registry.execute(
                "search",
                json!({
                    "mode": "hybrid",
                    "query": "authentication"
                }),
            ),
        )
        .await;
        let duration = start.elapsed();

        assert!(result.is_ok(), "Search timed out");
        let result = result.unwrap();
        assert!(result.is_ok(), "Search should succeed");
        eprintln!("Search latency: {:?}", duration);
    }
}

// ============================================================================
// Concurrency Tests
// ============================================================================

mod concurrency_tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    #[ignore = "Requires API credentials"]
    async fn test_concurrent_requests() {
        require_credentials!();

        let registry = Arc::new(create_test_registry());

        // Spawn multiple concurrent requests
        let handles: Vec<_> = (0..5)
            .map(|_i| {
                let reg = Arc::clone(&registry);
                tokio::spawn(async move {
                    reg.execute(
                        "help",
                        json!({
                            "action": "version"
                        }),
                    )
                    .await
                })
            })
            .collect();

        // Wait for all to complete
        let results: Vec<_> = futures::future::join_all(handles).await;

        // All should succeed
        for (i, result) in results.into_iter().enumerate() {
            assert!(result.is_ok(), "Task {} should not panic", i);
            assert!(result.unwrap().is_ok(), "Request {} should succeed", i);
        }
    }
}

// ============================================================================
// Coverage Documentation
// ============================================================================

#[cfg(test)]
mod coverage {
    #[test]
    fn test_integration_test_coverage() {
        // Document what integration tests cover:
        //
        // Authentication:
        // - API key authentication
        // - Invalid API key handling
        //
        // Session Lifecycle:
        // - init -> context -> capture -> recall workflow
        // - Session summary
        //
        // Multi-Tool Workflows:
        // - Workspace + Project listing
        // - Memory CRUD operations
        // - Search modes (semantic, hybrid)
        //
        // Error Handling:
        // - Invalid UUIDs
        // - Missing required parameters
        //
        // Help Tools:
        // - tools, auth, version actions
        //
        // Performance:
        // - Latency measurements for init, context, search
        //
        // Concurrency:
        // - Multiple concurrent requests

        let categories = [
            "Authentication",
            "Session Lifecycle",
            "Multi-Tool Workflows",
            "Error Handling",
            "Help Tools",
            "Performance",
            "Concurrency",
        ];
        assert_eq!(categories.len(), 7);
    }
}
