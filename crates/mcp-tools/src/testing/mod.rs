//! Test utilities for MCP tools.
//!
//! This module provides:
//! - `MockClient`: A mock API client for testing tools without network calls
//! - `TestFixtures`: Pre-built test data for common scenarios
//! - `ToolTestHarness`: Helper for testing tool handlers
//! - Assertion helpers for `ToolResult`

mod assertions;
mod fixtures;
mod harness;
mod mock_client;

pub use assertions::*;
pub use fixtures::*;
pub use harness::*;
pub use mock_client::*;
