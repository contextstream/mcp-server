//! Test harness for testing tool handlers.

use crate::registry::{RegisteredTool, ToolHandler, ToolRegistry};
use mcp_types::tool::ToolResult;
use mcp_types::{Config, Error, Result};
use serde_json::Value;
use std::sync::Arc;

use super::{MockClient, TestFixtures};

/// Test harness for testing tool handlers.
///
/// Provides a convenient way to test tool handlers in isolation.
pub struct ToolTestHarness {
    /// The tool registry.
    pub registry: ToolRegistry,
    /// The mock client.
    pub mock_client: MockClient,
    /// Test configuration.
    pub config: Config,
}

impl Default for ToolTestHarness {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolTestHarness {
    /// Create a new test harness with default configuration.
    pub fn new() -> Self {
        let config = TestFixtures::test_config();
        Self {
            registry: ToolRegistry::new(&config),
            mock_client: MockClient::new(),
            config,
        }
    }

    /// Create a test harness with custom configuration.
    pub fn with_config(config: Config) -> Self {
        Self {
            registry: ToolRegistry::new(&config),
            mock_client: MockClient::new(),
            config,
        }
    }

    /// Create a test harness for progressive mode.
    pub fn progressive() -> Self {
        Self::with_config(TestFixtures::progressive_config())
    }

    /// Create a test harness for router mode.
    pub fn router() -> Self {
        Self::with_config(TestFixtures::router_config())
    }

    /// Create a test harness for consolidated mode.
    pub fn consolidated() -> Self {
        Self::with_config(TestFixtures::consolidated_config())
    }

    /// Get a reference to the mock client.
    pub fn client(&self) -> &MockClient {
        &self.mock_client
    }

    /// Register a tool handler.
    pub fn register<T: ToolHandler + 'static>(&mut self, name: &str, handler: T) {
        self.registry.register(name, Arc::new(handler));
    }

    /// Execute a tool by name with the given input.
    pub async fn execute(&self, name: &str, input: Value) -> Result<ToolResult> {
        self.registry.execute(name, input).await
    }

    /// Execute a tool and expect success.
    pub async fn execute_ok(&self, name: &str, input: Value) -> ToolResult {
        self.execute(name, input)
            .await
            .expect("Expected tool execution to succeed")
    }

    /// Execute a tool and expect error.
    pub async fn execute_err(&self, name: &str, input: Value) -> Error {
        self.execute(name, input)
            .await
            .expect_err("Expected tool execution to fail")
    }

    /// Get a registered tool by name.
    pub fn get_tool(&self, name: &str) -> Option<&RegisteredTool> {
        self.registry.get(name)
    }

    /// List all registered tool names.
    pub fn tool_names(&self) -> Vec<&str> {
        self.registry.names()
    }

    /// Get tool count.
    pub fn tool_count(&self) -> usize {
        self.registry.len()
    }

    /// Enable a bundle (for progressive mode testing).
    pub fn enable_bundle(&mut self, bundle: &str) {
        self.registry.enable_bundle(bundle);
    }

    /// Get enabled bundles.
    pub fn enabled_bundles(&self) -> Vec<String> {
        self.registry.enabled_bundles()
    }

    /// Check if registry is in progressive mode.
    pub fn is_progressive(&self) -> bool {
        self.registry.is_progressive_mode()
    }

    /// Check if registry is in router mode.
    pub fn is_router(&self) -> bool {
        self.registry.is_router_mode()
    }

    /// Check if registry is in consolidated mode.
    pub fn is_consolidated(&self) -> bool {
        self.registry.is_consolidated_mode()
    }

    /// Get operation count (for router mode).
    pub fn operation_count(&self) -> usize {
        self.registry.operation_count()
    }

    /// Execute an operation (for router mode).
    pub async fn execute_operation(&self, name: &str, input: Value) -> Result<ToolResult> {
        self.registry.execute_operation(name, input).await
    }
}

/// Builder for creating test scenarios.
pub struct TestScenarioBuilder {
    harness: ToolTestHarness,
}

impl TestScenarioBuilder {
    /// Create a new scenario builder.
    pub fn new() -> Self {
        Self {
            harness: ToolTestHarness::new(),
        }
    }

    /// Use custom config.
    pub fn with_config(mut self, config: Config) -> Self {
        self.harness = ToolTestHarness::with_config(config);
        self
    }

    /// Add a mock response.
    pub fn mock(self, endpoint: &str, response: super::MockResponse) -> Self {
        self.harness.mock_client.on(endpoint).respond(response);
        self
    }

    /// Build the harness.
    pub fn build(self) -> ToolTestHarness {
        self.harness
    }
}

impl Default for TestScenarioBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::MockResponse;

    #[test]
    fn test_harness_creation() {
        let harness = ToolTestHarness::new();
        assert_eq!(harness.tool_count(), 0);
        assert!(!harness.is_progressive());
        assert!(!harness.is_router());
    }

    #[test]
    fn test_harness_progressive() {
        let harness = ToolTestHarness::progressive();
        assert!(harness.is_progressive());
    }

    #[test]
    fn test_harness_router() {
        let harness = ToolTestHarness::router();
        assert!(harness.is_router());
    }

    #[test]
    fn test_scenario_builder() {
        let harness = TestScenarioBuilder::new()
            .mock(
                "GET /api/v1/me",
                MockResponse::ok(serde_json::json!({"id": "123"})),
            )
            .build();

        assert_eq!(harness.tool_count(), 0);
    }
}
