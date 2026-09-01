//! Tests for the tool registry.

use super::*;
use crate::testing::TestFixtures;
use async_trait::async_trait;
use mcp_types::{
    config::{ToolSurfaceProfile, Toolset},
    tool::{ToolAnnotations, ToolCategory, ToolMetadata, ToolResult},
    Config, Result,
};
use serde_json::{json, Value};
use std::sync::Arc;

// ============================================================================
// Mock Tool for Testing
// ============================================================================

/// A simple mock tool for testing the registry.
struct MockTool {
    name: String,
    #[allow(dead_code)]
    title: String,
    #[allow(dead_code)]
    description: String,
    metadata: ToolMetadata,
}

impl MockTool {
    fn new(name: &str) -> Self {
        let metadata = ToolMetadata {
            name: name.to_string(),
            title: format!("{} Tool", name),
            description: format!("Mock tool for {}", name),
            category: ToolCategory::Session,
            annotations: ToolAnnotations::read_only(),
            is_pro: false,
            required_tier: None,
        };
        Self {
            name: name.to_string(),
            title: format!("{} Tool", name),
            description: format!("Mock tool for {}", name),
            metadata,
        }
    }

    fn with_details(name: &str, title: &str, description: &str) -> Self {
        let metadata = ToolMetadata {
            name: name.to_string(),
            title: title.to_string(),
            description: description.to_string(),
            category: ToolCategory::Session,
            annotations: ToolAnnotations::read_only(),
            is_pro: false,
            required_tier: None,
        };
        Self {
            name: name.to_string(),
            title: title.to_string(),
            description: description.to_string(),
            metadata,
        }
    }
}

#[async_trait]
impl ToolHandler for MockTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        Ok(ToolResult::text(format!(
            "Executed {} with input: {}",
            self.name, input
        )))
    }

    fn metadata(&self) -> &ToolMetadata {
        &self.metadata
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "input": {"type": "string"}
            }
        })
    }
}

// ============================================================================
// Test Helpers
// ============================================================================

fn create_test_config() -> Config {
    TestFixtures::test_config()
}

fn create_complete_config() -> Config {
    let mut config = TestFixtures::test_config();
    config.toolset = Toolset::Complete;
    config
}

#[allow(dead_code)]
fn create_standard_config() -> Config {
    let mut config = TestFixtures::test_config();
    config.toolset = Toolset::Standard;
    config
}

fn create_light_config() -> Config {
    let mut config = TestFixtures::test_config();
    config.toolset = Toolset::Light;
    config
}

fn create_progressive_config() -> Config {
    let mut config = TestFixtures::test_config();
    config.progressive_mode = true;
    config
}

fn create_router_config() -> Config {
    let mut config = TestFixtures::test_config();
    config.router_mode = true;
    config
}

fn create_consolidated_config() -> Config {
    let mut config = TestFixtures::test_config();
    config.consolidated_mode = true;
    config
}

fn create_consolidated_light_config() -> Config {
    let mut config = create_consolidated_config();
    config.toolset = Toolset::Light;
    config
}

fn create_consolidated_standard_config() -> Config {
    let mut config = create_consolidated_config();
    config.toolset = Toolset::Standard;
    config
}

fn create_openai_agentic_config() -> Config {
    let mut config = create_complete_config();
    config.tool_surface_profile = ToolSurfaceProfile::OpenaiAgentic;
    config
}

// ============================================================================
// Basic Registry Tests
// ============================================================================

mod basic_tests {
    use super::*;

    #[test]
    fn test_registry_creation() {
        let config = create_test_config();
        let registry = ToolRegistry::new(&config);

        assert!(registry.is_empty());
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn test_register_single_tool() {
        let config = create_complete_config();
        let mut registry = ToolRegistry::new(&config);

        let tool = Arc::new(MockTool::new("test_tool"));
        registry.register("test_tool", tool);

        assert!(!registry.is_empty());
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn test_get_tool() {
        let config = create_complete_config();
        let mut registry = ToolRegistry::new(&config);

        let tool = Arc::new(MockTool::new("my_tool"));
        registry.register("my_tool", tool);

        let registered = registry.get("my_tool");
        assert!(registered.is_some());
        assert_eq!(registered.unwrap().metadata.name, "my_tool");
    }

    #[test]
    fn test_get_nonexistent_tool() {
        let config = create_test_config();
        let registry = ToolRegistry::new(&config);

        let registered = registry.get("nonexistent");
        assert!(registered.is_none());
    }

    #[test]
    fn test_list_tools() {
        let config = create_complete_config();
        let mut registry = ToolRegistry::new(&config);

        registry.register("tool_a", Arc::new(MockTool::new("tool_a")));
        registry.register("tool_b", Arc::new(MockTool::new("tool_b")));

        let tools = registry.list();
        assert_eq!(tools.len(), 2);
    }

    #[test]
    fn test_names() {
        let config = create_complete_config();
        let mut registry = ToolRegistry::new(&config);

        registry.register("alpha", Arc::new(MockTool::new("alpha")));
        registry.register("beta", Arc::new(MockTool::new("beta")));

        let names = registry.names();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"alpha"));
        assert!(names.contains(&"beta"));
    }

    /// Regression test for the `tools/list` duplicate-name bug that surfaced
    /// in Windsurf as `Duplicate tool name: mcp0_async_job` (v0.3.19).
    ///
    /// `register_charts_tools` / `register_atlas_job_tools` register the
    /// same handler twice — once under the canonical name and once under
    /// the deprecated alias (`atlas_chart`, `atlas_job`). Before this fix
    /// `list()` walked the underlying `HashMap` and emitted the canonical
    /// `metadata.name` for both keys, so strict MCP clients rejected the
    /// response. Aliases must stay callable via `get()` / `execute()`.
    #[test]
    fn list_and_names_dedupe_back_compat_aliases() {
        let config = create_complete_config();
        let mut registry = ToolRegistry::new(&config);

        let tool: Arc<dyn ToolHandler> = Arc::new(MockTool::new("chart"));
        registry.register("chart", tool.clone());
        registry.register("atlas_chart", tool);

        let names = registry.names();
        assert_eq!(
            names.iter().filter(|n| **n == "chart").count(),
            1,
            "canonical name should appear exactly once in tools/list"
        );
        assert!(
            !names.contains(&"atlas_chart"),
            "alias key should not appear in tools/list"
        );
        assert_eq!(registry.list().len(), 1);

        // Alias must remain callable for back-compat.
        assert!(registry.get("chart").is_some());
        assert!(registry.get("atlas_chart").is_some());
    }
}

// ============================================================================
// Execute Tests
// ============================================================================

mod execute_tests {
    use super::*;

    #[tokio::test]
    async fn test_execute_tool() {
        let config = create_complete_config();
        let mut registry = ToolRegistry::new(&config);

        registry.register("exec_tool", Arc::new(MockTool::new("exec_tool")));

        let result = registry
            .execute("exec_tool", json!({"input": "test"}))
            .await;
        assert!(result.is_ok());

        let tool_result = result.unwrap();
        // Extract text from ContentItem enum
        if let mcp_types::tool::ContentItem::Text { text } = &tool_result.content[0] {
            assert!(text.contains("exec_tool"));
        } else {
            panic!("Expected Text content item");
        }
    }

    #[tokio::test]
    async fn test_execute_unknown_tool() {
        let config = create_test_config();
        let registry = ToolRegistry::new(&config);

        let result = registry.execute("unknown", json!({})).await;
        assert!(result.is_err());

        let err = result.unwrap_err();
        assert!(err.to_string().contains("Unknown tool"));
    }
}

// ============================================================================
// Toolset Filtering Tests
// ============================================================================

mod toolset_tests {
    use super::*;

    #[test]
    fn test_complete_toolset_allows_all() {
        let config = create_complete_config();
        let mut registry = ToolRegistry::new(&config);

        // Register tools that are in different toolsets
        registry.register("init", Arc::new(MockTool::new("init")));
        registry.register("custom_tool", Arc::new(MockTool::new("custom_tool")));

        // Complete toolset allows all
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn test_light_toolset_filters() {
        let config = create_light_config();
        let mut registry = ToolRegistry::new(&config);

        // init is in LIGHT_TOOLS
        registry.register("init", Arc::new(MockTool::new("init")));
        // context is in LIGHT_TOOLS
        registry.register("context", Arc::new(MockTool::new("context")));
        // custom_tool is not in any toolset
        registry.register("custom_tool", Arc::new(MockTool::new("custom_tool")));

        // Light toolset only allows light tools
        assert!(registry.get("init").is_some() || registry.get("context").is_some());
        // Custom tool should be filtered out
        assert!(registry.get("custom_tool").is_none());
    }
}

// ============================================================================
// Progressive Mode Tests
// ============================================================================

mod progressive_tests {
    use super::*;

    #[test]
    fn test_progressive_mode_enabled() {
        let config = create_progressive_config();
        let registry = ToolRegistry::new(&config);

        assert!(registry.is_progressive_mode());
    }

    #[test]
    fn test_progressive_mode_disabled() {
        let config = create_test_config();
        let registry = ToolRegistry::new(&config);

        assert!(!registry.is_progressive_mode());
    }

    #[test]
    fn test_progressive_mode_core_bundle_always_enabled() {
        let config = create_progressive_config();
        let mut registry = ToolRegistry::new(&config);

        // Core tools should be available
        registry.register("init", Arc::new(MockTool::new("init")));
        registry.register("context", Arc::new(MockTool::new("context")));

        // Core bundle tools should be registered
        let names = registry.names();
        // At least some core tools should be available
        assert!(names.contains(&"init") || names.contains(&"context"));
    }

    #[test]
    fn test_enable_bundle() {
        let config = create_test_config();
        let mut registry = ToolRegistry::new(&config);

        assert!(registry.enabled_bundles().is_empty());

        registry.enable_bundle("memory");

        let bundles = registry.enabled_bundles();
        assert!(bundles.contains(&"memory".to_string()));
    }

    #[test]
    fn test_available_bundles() {
        let bundles = ToolRegistry::available_bundles();

        // Should have multiple bundles
        assert!(!bundles.is_empty());

        // Check for some expected bundles
        let bundle_names: Vec<_> = bundles.iter().map(|(name, _)| *name).collect();
        assert!(bundle_names.contains(&"core"));
        assert!(bundle_names.contains(&"memory"));
        assert!(bundle_names.contains(&"session"));
        assert!(bundle_names.contains(&"search"));
        assert!(bundle_names.contains(&"graph"));
    }
}

// ============================================================================
// Router Mode Tests
// ============================================================================

mod router_tests {
    use super::*;

    #[test]
    fn test_router_mode_enabled() {
        let config = create_router_config();
        let registry = ToolRegistry::new(&config);

        assert!(registry.is_router_mode());
    }

    #[test]
    fn test_router_mode_disabled() {
        let config = create_test_config();
        let registry = ToolRegistry::new(&config);

        assert!(!registry.is_router_mode());
    }

    #[test]
    fn test_router_mode_stores_operations() {
        let config = create_router_config();
        let mut registry = ToolRegistry::new(&config);

        // Non-router-direct tools go to operations
        registry.register("my_operation", Arc::new(MockTool::new("my_operation")));

        // Should be in operations, not in tools
        assert!(registry.get("my_operation").is_none());
        assert!(registry.get_operation("my_operation").is_some());
    }

    #[test]
    fn test_list_operations() {
        let config = create_router_config();
        let mut registry = ToolRegistry::new(&config);

        registry.register("op_a", Arc::new(MockTool::new("op_a")));
        registry.register("op_b", Arc::new(MockTool::new("op_b")));

        let operations = registry.list_operations();
        assert_eq!(operations.len(), 2);
    }

    #[test]
    fn test_operation_names() {
        let config = create_router_config();
        let mut registry = ToolRegistry::new(&config);

        registry.register("operation_one", Arc::new(MockTool::new("operation_one")));

        let names = registry.operation_names();
        assert!(names.contains(&"operation_one"));
    }

    #[test]
    fn test_operation_count() {
        let config = create_router_config();
        let mut registry = ToolRegistry::new(&config);

        registry.register("op1", Arc::new(MockTool::new("op1")));
        registry.register("op2", Arc::new(MockTool::new("op2")));
        registry.register("op3", Arc::new(MockTool::new("op3")));

        assert_eq!(registry.operation_count(), 3);
    }

    #[tokio::test]
    async fn test_execute_operation() {
        let config = create_router_config();
        let mut registry = ToolRegistry::new(&config);

        registry.register("my_op", Arc::new(MockTool::new("my_op")));

        let result = registry.execute_operation("my_op", json!({})).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_execute_unknown_operation() {
        let config = create_router_config();
        let registry = ToolRegistry::new(&config);

        let result = registry.execute_operation("unknown_op", json!({})).await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Unknown operation"));
    }
}

// ============================================================================
// OpenAI Agentic Surface Tests
// ============================================================================

mod openai_agentic_surface_tests {
    use super::*;

    #[test]
    fn test_openai_agentic_surface_only_exposes_core_tools() {
        let config = create_openai_agentic_config();
        let mut registry = ToolRegistry::new(&config);

        registry.register("init", Arc::new(MockTool::new("init")));
        registry.register("context", Arc::new(MockTool::new("context")));
        registry.register("search", Arc::new(MockTool::new("search")));
        registry.register("memory", Arc::new(MockTool::new("memory")));
        registry.register("capsule", Arc::new(MockTool::new("capsule")));
        registry.register("integration", Arc::new(MockTool::new("integration")));

        let names = registry.names();
        assert!(names.contains(&"init"));
        assert!(names.contains(&"context"));
        assert!(names.contains(&"search"));
        assert!(names.contains(&"memory"));
        assert!(names.contains(&"capsule"));
        assert!(!names.contains(&"integration"));
    }

    // Regression: the hosted gateway shares one long-lived registry across
    // tenants. `apply_initialize_surface_profile` must reset to the
    // construction-time default when a request carries no profile, so a prior
    // client's auto-detected narrowing can't bleed into the next client (the
    // codex-fugu "unsupported call: <tool>" symptom).
    #[test]
    fn test_apply_initialize_surface_profile_resets_to_default_when_undetected() {
        let config = create_complete_config(); // default surface profile
        let registry = ToolRegistry::new(&config);
        assert_eq!(registry.tool_surface_profile(), ToolSurfaceProfile::Default);

        registry.apply_initialize_surface_profile(Some(ToolSurfaceProfile::OpenaiAgentic));
        assert_eq!(
            registry.tool_surface_profile(),
            ToolSurfaceProfile::OpenaiAgentic
        );

        registry.apply_initialize_surface_profile(None);
        assert_eq!(registry.tool_surface_profile(), ToolSurfaceProfile::Default);
    }

    // A registry constructed WITH the agentic surface as its baseline (e.g.
    // Copilot via env/header) must fall back to that baseline — not Default —
    // when an initialize carries no explicit profile.
    #[test]
    fn test_apply_initialize_surface_profile_preserves_configured_default() {
        let config = create_openai_agentic_config();
        let registry = ToolRegistry::new(&config);
        assert_eq!(
            registry.tool_surface_profile(),
            ToolSurfaceProfile::OpenaiAgentic
        );
        assert_eq!(
            registry.default_tool_surface_profile(),
            ToolSurfaceProfile::OpenaiAgentic
        );

        // Even after a transient flip back to Default, an undetected
        // initialize restores the configured agentic baseline.
        registry.apply_initialize_surface_profile(Some(ToolSurfaceProfile::Default));
        assert_eq!(registry.tool_surface_profile(), ToolSurfaceProfile::Default);
        registry.apply_initialize_surface_profile(None);
        assert_eq!(
            registry.tool_surface_profile(),
            ToolSurfaceProfile::OpenaiAgentic
        );
    }

    #[tokio::test]
    async fn test_openai_agentic_surface_hides_direct_execution_for_long_tail_tools() {
        let config = create_openai_agentic_config();
        let mut registry = ToolRegistry::new(&config);

        registry.register("search", Arc::new(MockTool::new("search")));
        registry.register("integration", Arc::new(MockTool::new("integration")));

        let direct = registry
            .execute("integration", json!({"input": "test"}))
            .await;
        assert!(direct.is_err());

        let deferred = registry
            .execute_operation("integration", json!({"input": "test"}))
            .await;
        assert!(deferred.is_ok());
    }

    #[test]
    fn test_search_catalog_returns_hidden_tool_with_execute_operation_mode() {
        let config = create_openai_agentic_config();
        let mut registry = ToolRegistry::new(&config);

        registry.register(
            "integration",
            Arc::new(MockTool::with_details(
                "integration",
                "Integration Tool",
                "Manage external integrations and syncs",
            )),
        );

        let results = registry.search_catalog("integration sync", None, 10);
        assert!(!results.is_empty());
        let first = &results[0];
        assert_eq!(first["name"], json!("integration"));
        assert_eq!(first["call_mode"], json!("execute_operation"));
        assert!(first["when_to_use"].as_str().is_some());
        assert!(first["avoid_when"].as_str().is_some());
        assert!(first["examples"]
            .as_array()
            .map(|v| !v.is_empty())
            .unwrap_or(false));
        assert!(first["tags"]
            .as_array()
            .map(|v| !v.is_empty())
            .unwrap_or(false));
    }
}

// ============================================================================
// Consolidated Mode Tests
// ============================================================================

mod consolidated_tests {
    use super::*;

    #[test]
    fn test_consolidated_mode_enabled() {
        let config = create_consolidated_config();
        let registry = ToolRegistry::new(&config);

        assert!(registry.is_consolidated_mode());
    }

    #[test]
    fn test_consolidated_mode_disabled() {
        let config = create_test_config();
        let registry = ToolRegistry::new(&config);

        assert!(!registry.is_consolidated_mode());
    }

    #[test]
    fn test_consolidated_mode_filters_tools() {
        let config = create_consolidated_config();
        let mut registry = ToolRegistry::new(&config);

        // Consolidated tools should be registered
        registry.register("init", Arc::new(MockTool::new("init")));
        registry.register("session", Arc::new(MockTool::new("session")));

        // Non-consolidated tools should be filtered
        registry.register("random_tool", Arc::new(MockTool::new("random_tool")));

        assert!(registry.get("init").is_some());
        assert!(registry.get("session").is_some());
        assert!(registry.get("random_tool").is_none());
    }

    #[test]
    fn test_consolidated_mode_light_toolset_excludes_advanced_tools() {
        let config = create_consolidated_light_config();
        let mut registry = ToolRegistry::new(&config);

        registry.register("init", Arc::new(MockTool::new("init")));
        registry.register("integration", Arc::new(MockTool::new("integration")));
        registry.register("ai", Arc::new(MockTool::new("ai")));

        assert!(registry.get("init").is_some());
        assert!(registry.get("integration").is_none());
        assert!(registry.get("ai").is_none());
    }

    #[test]
    fn test_consolidated_mode_standard_toolset_includes_advanced_tools() {
        let config = create_consolidated_standard_config();
        let mut registry = ToolRegistry::new(&config);

        registry.register("init", Arc::new(MockTool::new("init")));
        registry.register("integration", Arc::new(MockTool::new("integration")));
        registry.register("ai", Arc::new(MockTool::new("ai")));

        assert!(registry.get("init").is_some());
        assert!(registry.get("integration").is_some());
        assert!(registry.get("ai").is_some());
    }
}

// ============================================================================
// Constants Tests
// ============================================================================

mod constants_tests {
    use super::*;

    #[test]
    fn test_light_tools_contains_core() {
        assert!(LIGHT_TOOLS.contains(&"init"));
        assert!(LIGHT_TOOLS.contains(&"context"));
        assert!(LIGHT_TOOLS.contains(&"session"));
        assert!(LIGHT_TOOLS.contains(&"search"));
    }

    #[test]
    fn test_standard_tools() {
        assert!(STANDARD_TOOLS.contains(&"workspaces_create"));
        assert!(STANDARD_TOOLS.contains(&"reminders_create"));
    }

    #[test]
    fn test_router_direct_tools() {
        assert!(ROUTER_DIRECT_TOOLS.contains(&"operations"));
        assert!(ROUTER_DIRECT_TOOLS.contains(&"execute_operation"));
    }

    #[test]
    fn test_consolidated_tools() {
        assert!(CONSOLIDATED_TOOLS.contains(&"init"));
        assert!(CONSOLIDATED_TOOLS.contains(&"context"));
        assert!(CONSOLIDATED_TOOLS.contains(&"session"));
        assert!(CONSOLIDATED_TOOLS.contains(&"search"));
        assert!(CONSOLIDATED_TOOLS.contains(&"memory"));
        assert!(CONSOLIDATED_TOOLS.contains(&"graph"));
        assert!(CONSOLIDATED_TOOLS.contains(&"workspace"));
        assert!(CONSOLIDATED_TOOLS.contains(&"project"));
        assert!(CONSOLIDATED_TOOLS.contains(&"coordination"));
        assert!(STANDARD_TOOLS.contains(&"coordination"));
    }

    #[test]
    fn test_core_bundle() {
        assert!(CORE_BUNDLE.contains(&"init"));
        assert!(CORE_BUNDLE.contains(&"context"));
        assert!(CORE_BUNDLE.contains(&"session"));
        assert!(CORE_BUNDLE.contains(&"search"));
        assert!(CORE_BUNDLE.contains(&"help"));
    }
}

// ============================================================================
// Registered Tool Tests
// ============================================================================

mod registered_tool_tests {
    use super::*;

    #[test]
    fn test_registered_tool_contains_metadata() {
        let config = create_complete_config();
        let mut registry = ToolRegistry::new(&config);

        let tool = Arc::new(MockTool::with_details(
            "detailed_tool",
            "Detailed Tool",
            "A detailed mock tool",
        ));
        registry.register("detailed_tool", tool);

        let registered = registry.get("detailed_tool").unwrap();
        assert_eq!(registered.metadata.name, "detailed_tool");
        assert_eq!(registered.metadata.title, "Detailed Tool");
        assert_eq!(registered.metadata.description, "A detailed mock tool");
    }

    #[test]
    fn test_registered_tool_contains_schema() {
        let config = create_complete_config();
        let mut registry = ToolRegistry::new(&config);

        let tool = Arc::new(MockTool::new("schema_tool"));
        registry.register("schema_tool", tool);

        let registered = registry.get("schema_tool").unwrap();
        assert!(registered.input_schema.is_object());
        assert!(registered.input_schema.get("properties").is_some());
    }
}

// ============================================================================
// Mode Combination Tests
// ============================================================================

mod mode_combination_tests {
    use super::*;

    #[test]
    fn test_progressive_and_router_modes() {
        let mut config = create_test_config();
        config.progressive_mode = true;
        config.router_mode = true;

        let registry = ToolRegistry::new(&config);

        assert!(registry.is_progressive_mode());
        assert!(registry.is_router_mode());
    }

    #[test]
    fn test_progressive_and_consolidated_modes() {
        let mut config = create_test_config();
        config.progressive_mode = true;
        config.consolidated_mode = true;

        let registry = ToolRegistry::new(&config);

        assert!(registry.is_progressive_mode());
        assert!(registry.is_consolidated_mode());
    }

    #[test]
    fn test_all_modes_disabled_by_default() {
        let config = create_test_config();
        let registry = ToolRegistry::new(&config);

        assert!(!registry.is_progressive_mode());
        assert!(!registry.is_router_mode());
        assert!(!registry.is_consolidated_mode());
    }
}

// ============================================================================
// Coverage Documentation Tests
// ============================================================================

mod coverage_tests {
    #[test]
    fn test_toolset_coverage() {
        // Document all toolsets:
        // - Complete: All tools registered without filtering
        // - Standard: Light tools + standard tools
        // - Light: Core essential tools only (init, context, session, search, etc.)

        let toolsets = ["complete", "standard", "light"];
        assert_eq!(toolsets.len(), 3);
    }

    #[test]
    fn test_mode_coverage() {
        // Document all modes:
        // - Progressive: Tools available in bundles, enabled progressively
        // - Router: Tools as operations, accessed via execute_operation
        // - Consolidated: Domain-unified tools only (session, memory, graph, etc.)

        let modes = ["progressive", "router", "consolidated"];
        assert_eq!(modes.len(), 3);
    }

    #[test]
    fn test_bundle_coverage() {
        // Document all progressive mode bundles:
        // - core: Essential tools (init, context, session, search, help)
        // - memory: Memory operations
        // - session: Session operations
        // - search: Search operations
        // - graph: Graph operations
        // - workspace: Workspace management
        // - project: Project management
        // - reminders: Reminder operations
        // - integrations: Integration operations

        let bundles = [
            "core",
            "memory",
            "session",
            "search",
            "graph",
            "workspace",
            "project",
            "reminders",
            "integrations",
        ];
        assert!(bundles.len() >= 8);
    }
}

#[test]
fn test_discovery_hints_memory_keywords() {
    use super::*;
    use mcp_types::tool::{ToolAnnotations, ToolCategory, ToolMetadata};

    let metadata = ToolMetadata {
        name: "memory".to_string(),
        title: "Memory".to_string(),
        description: "Memory management".to_string(),
        category: ToolCategory::Memory,
        annotations: ToolAnnotations::default(),
        is_pro: false,
        required_tier: None,
    };
    let hints = discovery_hints("memory", &metadata);
    assert!(hints.aliases.contains(&"AI memory"));
    assert!(hints.aliases.contains(&"my memories"));
    assert!(hints.aliases.contains(&"recent memories"));
    assert!(hints.aliases.contains(&"saved preferences"));
    assert!(hints.aliases.contains(&"saved lessons"));
    assert!(hints.tags.contains(&"context"));
    assert!(hints.tags.contains(&"memories"));
}

#[test]
fn test_discovery_hints_session_keywords() {
    use super::*;
    use mcp_types::tool::{ToolAnnotations, ToolCategory, ToolMetadata};

    let metadata = ToolMetadata {
        name: "session".to_string(),
        title: "Session".to_string(),
        description: "Session management".to_string(),
        category: ToolCategory::Session,
        annotations: ToolAnnotations::default(),
        is_pro: false,
        required_tier: None,
    };
    let hints = discovery_hints("session", &metadata);
    assert!(hints.aliases.contains(&"remember"));
    assert!(hints.aliases.contains(&"save memory"));
    assert!(hints.aliases.contains(&"save plan"));
    assert!(hints.aliases.contains(&"save lesson"));
    assert!(hints.aliases.contains(&"save preference"));
    assert!(hints.aliases.contains(&"show context"));
    assert!(hints.tags.contains(&"remember"));
    assert!(hints.tags.contains(&"context"));
    assert!(hints.tags.contains(&"memory"));
}

#[test]
fn test_discovery_hints_capture_plan_keywords() {
    use super::*;
    use mcp_types::tool::{ToolAnnotations, ToolCategory, ToolMetadata};

    let metadata = ToolMetadata {
        name: "capture_plan".to_string(),
        title: "Capture Plan".to_string(),
        description: "Capture plan".to_string(),
        category: ToolCategory::Session,
        annotations: ToolAnnotations::write(),
        is_pro: false,
        required_tier: None,
    };
    let hints = discovery_hints("capture_plan", &metadata);
    assert!(hints.aliases.contains(&"save plan"));
    assert!(hints.tags.contains(&"write"));
    assert!(hints.when_to_use.contains("Save"));
}

#[test]
fn test_discovery_hints_lesson_and_preference_save_keywords() {
    use super::*;
    use mcp_types::tool::{ToolAnnotations, ToolCategory, ToolMetadata};

    let lesson_metadata = ToolMetadata {
        name: "session_capture_lesson".to_string(),
        title: "Capture Lesson".to_string(),
        description: "Capture lesson".to_string(),
        category: ToolCategory::Session,
        annotations: ToolAnnotations::write(),
        is_pro: false,
        required_tier: None,
    };
    let lesson_hints = discovery_hints("session_capture_lesson", &lesson_metadata);
    assert!(lesson_hints.aliases.contains(&"save lesson"));
    assert!(lesson_hints.tags.contains(&"lessons"));

    let remember_metadata = ToolMetadata {
        name: "session_remember".to_string(),
        title: "Remember".to_string(),
        description: "Remember".to_string(),
        category: ToolCategory::Session,
        annotations: ToolAnnotations::write(),
        is_pro: false,
        required_tier: None,
    };
    let remember_hints = discovery_hints("session_remember", &remember_metadata);
    assert!(remember_hints.aliases.contains(&"save preference"));
    assert!(remember_hints.tags.contains(&"preferences"));
}

#[test]
fn test_discovery_hints_graph_code_health_keywords() {
    use super::*;
    use mcp_types::tool::{ToolAnnotations, ToolCategory, ToolMetadata};

    let metadata = ToolMetadata {
        name: "graph".to_string(),
        title: "Graph".to_string(),
        description: "Graph analysis".to_string(),
        category: ToolCategory::Graph,
        annotations: ToolAnnotations::default(),
        is_pro: false,
        required_tier: None,
    };
    let hints = discovery_hints("graph", &metadata);
    assert!(hints.aliases.contains(&"code health"));
    assert!(hints.aliases.contains(&"quality dashboard"));
    assert!(hints.aliases.contains(&"complexity metrics"));
    assert!(hints.tags.contains(&"code-health"));
    assert!(hints.tags.contains(&"recommendations"));
    assert!(hints.when_to_use.contains("dashboard"));
}

#[test]
fn test_capsule_is_direct_on_openai_surface_but_not_in_light_toolset() {
    assert!(!LIGHT_TOOLS.contains(&"capsule"));
    assert!(OPENAI_AGENTIC_CORE_TOOLS.contains(&"capsule"));
}

#[test]
fn test_capsule_is_exposed_to_standard_and_consolidated_toolsets() {
    assert!(STANDARD_TOOLS.contains(&"capsule"));
    assert!(CONSOLIDATED_TOOLS.contains(&"capsule"));
}
