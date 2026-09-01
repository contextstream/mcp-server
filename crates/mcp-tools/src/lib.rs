//! Tool definitions for the ContextStream MCP server.
//!
//! This crate provides:
//! - Tool registry with JSON Schema generation
//! - Tool handlers for all domain tools
//! - Tool filtering by toolset (light/standard/complete)
//! - Atlas remote-layer gating helpers ([`atlas_flags`]) —
//! - Testing utilities (behind `cfg(test)`)

// Stylistic clippy lints we accept crate-wide rather than mass-rewriting:
// - too_many_arguments: many domain handlers take wide parameter lists by design.
// - manual_clamp: the explicit `.max().min()` form avoids clamp's panic-on-inverted-bounds.
// - should_implement_trait: several `from_str` helpers are domain parsers, not std FromStr.
#![allow(clippy::too_many_arguments)]
#![allow(clippy::manual_clamp)]
#![allow(clippy::should_implement_trait)]

pub mod atlas_flags;
pub mod domains;
pub mod registry;
pub mod schema;
pub mod wire_tokens;

#[cfg(test)]
pub mod testing;

pub use atlas_flags::{gate_decision, AtlasProductGate};
pub use registry::{RegisteredTool, ToolRegistry};

/// Shared mutex for tests that mutate process-global state (env vars or the
/// current working directory). `std::env::set_var` / `std::env::set_current_dir`
/// affect the whole process, so every test that touches them must hold this lock
/// to avoid races under parallel execution. Mirrors `mcp_server::env_test_mutex`.
#[cfg(test)]
pub fn env_test_mutex() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}
