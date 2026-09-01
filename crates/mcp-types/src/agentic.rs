//! Shared agentic telemetry contract.
//!
//! This module defines the rule keys, metadata keys, and request/response
//! payloads used by MCP runtime telemetry, eval harnesses, and downstream
//! compliance analytics.

use crate::config::ToolSurfaceProfile;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Runtime identifier for the Rust MCP process. This is not a coding-harness
/// identity and must not be used as `harness_id` in production telemetry.
pub const RUNTIME_CONTEXTSTREAM_MCP: &str = "contextstream-mcp";
/// Backward-compatible name used by existing eval fixtures. Production
/// telemetry should attribute the actual client harness and carry this value as
/// `metadata.runtime_id`.
pub const HARNESS_CONTEXTSTREAM_MCP: &str = RUNTIME_CONTEXTSTREAM_MCP;
/// Harness identifier for the local MCP eval harness.
pub const HARNESS_MCP_AGENTIC_EVAL: &str = "mcp-agentic-eval";
/// Harness identifier for OpenAI Responses validations.
pub const HARNESS_OPENAI_RESPONSES_E2E: &str = "openai-responses-e2e";
/// Harness identifier for ChatGPT gateway validations.
pub const HARNESS_CHATGPT_GATEWAY_E2E: &str = "chatgpt-gateway-e2e";

/// Rule key emitted when the compact surface uses discovery search.
pub const RULE_AGENTIC_TOOL_SEARCH_USED: &str = "agentic_tool_search_used";
/// Rule key emitted when a deferred operation is executed.
pub const RULE_AGENTIC_EXECUTE_OPERATION_USED: &str = "agentic_execute_operation_used";
/// Rule key emitted when batched read-only operations are executed.
pub const RULE_AGENTIC_BATCH_OPERATIONS_USED: &str = "agentic_batch_operations_used";
/// Rule key emitted when a hidden direct call is blocked or redirected.
pub const RULE_AGENTIC_HIDDEN_DIRECT_CALL_BLOCKED: &str = "agentic_hidden_direct_call_blocked";
/// Rule key emitted when the wrong tool or operation was selected.
pub const RULE_AGENTIC_WRONG_TOOL_SELECTED: &str = "agentic_wrong_tool_selected";
/// Rule key emitted when the same turn required another tool attempt.
pub const RULE_AGENTIC_TOOL_RETRY: &str = "agentic_tool_retry";

/// Metadata key: normalized surface identifier used in analytics breakdowns.
pub const META_TOOL_SURFACE_PROFILE: &str = "tool_surface_profile";
/// Metadata key: direct tool name or searched tool name.
pub const META_TOOL_NAME: &str = "tool_name";
/// Metadata key: deferred operation name.
pub const META_OPERATION_NAME: &str = "operation_name";
/// Metadata key: search query or task description.
pub const META_QUERY: &str = "query";
/// Metadata key: number of candidates returned by discovery.
pub const META_CANDIDATE_COUNT: &str = "candidate_count";
/// Metadata key: selected rank from a discovery result list.
pub const META_SELECTED_RANK: &str = "selected_rank";
/// Metadata key: number of operations supplied to a batch request.
pub const META_BATCH_COUNT: &str = "batch_count";
/// Metadata key: retries already observed for the current turn.
pub const META_RETRY_COUNT: &str = "retry_count";
/// Metadata key: number of agentic tool yields observed in the current turn.
pub const META_TOOL_YIELD_COUNT: &str = "tool_yield_count";
/// Metadata key: direct tool or operation the model attempted to access.
pub const META_HIDDEN_TOOL_REQUESTED: &str = "hidden_tool_requested";
/// Metadata key: end-to-end latency for the instrumented action.
pub const META_LATENCY_MS: &str = "latency_ms";
/// Metadata key: number of results or returned content items.
pub const META_RESULT_COUNT: &str = "result_count";
/// Metadata key: client identifier or user agent family.
pub const META_CLIENT_NAME: &str = "client_name";
/// Metadata key: correlation identifier for the incoming request.
pub const META_REQUEST_ID: &str = "request_id";
/// Metadata key: correlation identifier for the current turn.
pub const META_TURN_ID: &str = "turn_id";
/// Metadata key: whether the first tool matched the harness expectation.
pub const META_FIRST_TOOL_CORRECT: &str = "first_tool_correct";
/// Metadata key: free-form prompt or case identifier from an eval corpus.
pub const META_CASE_ID: &str = "case_id";
/// Metadata key: expected tool or operation from the eval corpus.
pub const META_EXPECTED_TOOL: &str = "expected_tool";
/// Metadata key: actual selected tool or operation from the harness.
pub const META_ACTUAL_TOOL: &str = "actual_tool";
/// Metadata key: runtime implementation separate from the coding harness.
pub const META_RUNTIME_ID: &str = "runtime_id";

/// Analytics label for the broad/default surface.
pub const SURFACE_BROAD_DEFAULT: &str = "broad_default_surface";
/// Analytics label for the compact OpenAI-oriented surface.
pub const SURFACE_ADAPTIVE_OPENAI: &str = "adaptive_openai_surface";

/// Normalize the configured tool surface to the analytics label.
pub fn analytics_surface_profile(profile: ToolSurfaceProfile) -> &'static str {
    match profile {
        ToolSurfaceProfile::Default => SURFACE_BROAD_DEFAULT,
        ToolSurfaceProfile::OpenaiAgentic => SURFACE_ADAPTIVE_OPENAI,
    }
}

/// Compliance event request payload for analytics ingest.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ComplianceEventRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub model_id: String,
    pub harness_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rules_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rule_id: Option<Uuid>,
    pub rule_key: String,
    pub rule_class: String,
    pub check_type: String,
    pub result: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_enforceable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity: Option<i16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_time: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

/// Compliance event ingest response body.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ComplianceEventRecorded {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub id: Option<Uuid>,
    #[serde(default)]
    pub created_at: Option<String>,
}
