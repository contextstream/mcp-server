//! Authentication context for per-request auth overrides.
//!
//! The primary per-request auth mechanism is the tokio task-local in
//! `mcp_client::run_with_auth_override`. This module provides helpers for
//! extracting auth from MCP JSON message headers (used by the stdio transport).

use mcp_types::{AuthOverride, TrafficClass};
use uuid::Uuid;

// Re-export the async-safe task-local helpers from mcp-client.
pub use mcp_client::{get_task_auth_override, run_with_auth_override};

/// Extract auth override from MCP protocol message headers (JSON value).
///
/// Used by the stdio/streamable-HTTP MCP transport where headers arrive as
/// JSON fields inside the MCP message envelope.
pub fn extract_auth_from_headers(headers: &serde_json::Value) -> Option<AuthOverride> {
    let api_key = headers
        .get("x-api-key")
        .or_else(|| headers.get("X-API-Key"))
        .and_then(|v| v.as_str())
        .map(String::from);

    let jwt = headers
        .get("authorization")
        .or_else(|| headers.get("Authorization"))
        .and_then(|v| v.as_str())
        .and_then(|s| s.strip_prefix("Bearer "))
        .map(String::from);

    let workspace_id = headers
        .get("x-contextstream-workspace-id")
        .or_else(|| headers.get("X-ContextStream-Workspace-Id"))
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok());

    let project_id = headers
        .get("x-contextstream-project-id")
        .or_else(|| headers.get("X-ContextStream-Project-Id"))
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok());

    let has_credential = api_key
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
        || jwt.as_deref().is_some_and(|value| !value.trim().is_empty());
    let traffic_class = if has_credential {
        headers
            .get("x-contextstream-traffic-class")
            .or_else(|| headers.get("X-ContextStream-Traffic-Class"))
            .and_then(|v| v.as_str())
            .and_then(TrafficClass::from_header_value)
    } else {
        None
    };

    if api_key.is_some()
        || jwt.is_some()
        || workspace_id.is_some()
        || project_id.is_some()
        || traffic_class.is_some()
    {
        Some(AuthOverride {
            api_key,
            jwt,
            workspace_id,
            project_id,
            traffic_class,
        })
    } else {
        None
    }
}
