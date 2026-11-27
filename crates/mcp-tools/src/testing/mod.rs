//! Test utilities for MCP tools.
//!
//! This module provides:
//! - `MockClient`: A mock API client for testing tools without network calls
//! - `TestFixtures`: Pre-built test data for common scenarios
//! - `ToolTestHarness`: Helper for testing tool handlers
//! - Assertion helpers for `ToolResult`
//! - `FrozenClock`: a stopped runtime clock for tests that depend on a deadline

mod assertions;
mod fixtures;
mod frozen_clock;
mod harness;
mod mock_client;

pub use assertions::*;
pub use fixtures::*;
pub use frozen_clock::*;
pub use harness::*;
pub use mock_client::*;
