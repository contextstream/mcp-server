//! ContextStream MCP Server Library.
//!
//! This module provides the core functionality for the MCP server,
//! exposing transport, hooks, and configuration for testing.

#![recursion_limit = "512"]
// Stylistic clippy lints accepted crate-wide (see mcp-tools/mcp-client for rationale):
// many setup/transport helpers take wide parameter lists by design, and a few
// `from_str` helpers are domain parsers rather than std `FromStr` impls.
#![allow(clippy::too_many_arguments)]
#![allow(clippy::should_implement_trait)]

#[cfg(test)]
#[path = "../build_metadata.rs"]
mod build_metadata_tests;

pub mod acceleration;
pub mod agentic_telemetry;
pub mod atlas;
pub mod auth;
pub mod config;
pub mod connect;
pub mod hook_handlers;
pub mod hooks;
pub mod server;
// Relocated to `mcp-session` so the `context()` tool (in `mcp-tools`) can read
// the per-session model too. Re-exported here so existing
// `crate::session_model_cache::…` references keep working unchanged.
pub use mcp_session::session_model_cache;
pub mod setup;
pub mod transport;
pub mod watch;

pub use hooks::{HookContext, HookManager, HookResult, HookType, HooksConfig};

fn feature_gate_value(value: Option<&str>, default: bool) -> bool {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return default;
    };
    matches!(
        value.to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn feature_gate_enabled(name: &str, default: bool) -> bool {
    feature_gate_value(std::env::var(name).ok().as_deref(), default)
}

/// Unit tests exercise setup and hook code against synthetic paths. They must
/// never publish diagnostic readiness evidence into the developer's real
/// `~/.contextstream` state. Process-level sandbox tests use the normal binary
/// build and therefore keep the production behavior enabled.
pub(crate) const fn readiness_evidence_writes_enabled() -> bool {
    !cfg!(test)
}

/// Rollback gate for optional MCP lifecycle `instructions`.
///
/// The current 2024 protocol never emits this field. Newer, fully implemented
/// revisions remain opt-in during the teaching rollout so disabling this
/// surface cannot hide or disable any MCP tool.
pub(crate) fn protocol_harness_teaching_enabled() -> bool {
    feature_gate_enabled("CONTEXTSTREAM_HARNESS_TEACHING_PROTOCOL_ENABLED", false)
}

/// Rollback gate for evidence derived from managed editor hooks.
///
/// Static rules, MCP config, and every MCP tool keep working when disabled.
/// Local setup/runtime evidence remains available; only hook-derived
/// `loaded`/deterministic-practice observations are skipped.
pub(crate) fn hook_readiness_evidence_enabled() -> bool {
    readiness_evidence_writes_enabled()
        && feature_gate_enabled(
            "CONTEXTSTREAM_HARNESS_READINESS_HOOK_EVIDENCE_ENABLED",
            true,
        )
}

/// True only while the CLI is dispatching a hook command carrying the exact
/// ownership marker emitted by our installer. This is not an authentication
/// boundary (a local user can invoke the binary), but it prevents ad-hoc or
/// legacy unmarked hook calls from being mistaken for managed readiness
/// evidence.
pub(crate) fn managed_hook_invocation() -> bool {
    std::env::var("CONTEXTSTREAM_MANAGED_HOOK_INVOCATION").as_deref() == Ok("1")
}

/// Shared mutex for tests that modify environment variables.
///
/// Since `std::env::set_var` / `remove_var` affect the whole process,
/// all tests that touch env vars must hold this lock to avoid races.
#[cfg(test)]
pub fn env_test_mutex() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

#[cfg(test)]
mod rollout_gate_tests {
    use super::feature_gate_value;

    #[test]
    fn rollout_gates_are_explicit_and_fail_to_the_declared_default() {
        for enabled in ["1", "true", "TRUE", " yes ", "On"] {
            assert!(feature_gate_value(Some(enabled), false), "{enabled}");
        }
        for disabled in ["0", "false", "FALSE", " no ", "off", "unexpected"] {
            assert!(!feature_gate_value(Some(disabled), true), "{disabled}");
        }
        assert!(feature_gate_value(None, true));
        assert!(!feature_gate_value(None, false));
        assert!(feature_gate_value(Some("   "), true));
        assert!(!feature_gate_value(Some("   "), false));
    }
}
