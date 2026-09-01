//! Pinned whole-wire token accounting for MCP tool results.
//!
//! The byte proxy remains the default guard. Shadow mode measures the exact
//! serialized response, while enforce mode switches only a deterministic
//! canary of registry-verified `o200k_base` requests to exact accounting.
//! Vocabulary initialization is startup-only: request paths fail closed to the
//! proxy if the singleton was not warmed.

use mcp_types::tool::{as_structured_object, structured_content_enabled, ContentItem, ToolResult};
use metrics::{counter, histogram};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tiktoken_rs::{o200k_base_singleton, CoreBPE};
use tracing::{info, warn};

pub const O200K_ENCODING: &str = "o200k_base";
pub const O200K_IMPLEMENTATION: &str = "tiktoken-rs";
pub const O200K_IMPLEMENTATION_VERSION: &str = "0.12.0";
pub const BYTE_PROXY_ESTIMATOR: &str = "minified_json_utf8_bytes_div_4_ceil";
pub const TOKENIZER_CACHE_BASIS_VERSION: &str = "mcp-wire-tokenizer-v1";
pub const WIRE_REPORT_KEY: &str = "wire_tokenizer";
pub const REPORT_TOKEN_RESERVE: usize = 224;

const TOKENIZER_MODE_ENV: &str = "MCP_WIRE_TOKENIZER_MODE";
const TOKENIZER_CANARY_PERCENT_ENV: &str = "MCP_WIRE_TOKENIZER_CANARY_PERCENT";
const REPORT_FIXED_POINT_LIMIT: usize = 16;

static O200K: OnceLock<&'static CoreBPE> = OnceLock::new();
static ROLLOUT_CONFIG: OnceLock<RolloutConfig> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportView {
    /// The exact streamable-HTTP JSON-RPC response owned by the server.
    McpHttpJsonRpcV2,
    /// The exact legacy HTTP tool-result JSON body (no JSON-RPC envelope).
    McpHttpRestToolResultV1,
    /// The exact stdio JSON-RPC response, including its terminating LF.
    McpStdioJsonRpcV2,
    /// A handler-owned `ToolResult` when no outer MCP envelope is available.
    ToolResultBody,
}

impl TransportView {
    pub const fn label(self) -> &'static str {
        match self {
            Self::McpHttpJsonRpcV2 => "mcp_http_jsonrpc_v2",
            Self::McpHttpRestToolResultV1 => "mcp_http_rest_tool_result_v1",
            Self::McpStdioJsonRpcV2 => "mcp_stdio_jsonrpc_v2_newline",
            Self::ToolResultBody => "tool_result_body",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RolloutMode {
    Proxy,
    Shadow,
    Enforce,
}

impl RolloutMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::Proxy => "proxy",
            Self::Shadow => "shadow",
            Self::Enforce => "enforce",
        }
    }

    fn from_env_value(value: Option<String>) -> Self {
        match value
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("shadow") => Self::Shadow,
            Some("enforce") | Some("canary") => Self::Enforce,
            _ => Self::Proxy,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenizerCompatibility {
    VerifiedO200k,
    UnknownOrNonO200k,
}

impl TokenizerCompatibility {
    pub const fn label(self) -> &'static str {
        match self {
            Self::VerifiedO200k => "verified_o200k",
            Self::UnknownOrNonO200k => "unknown_or_non_o200k",
        }
    }

    pub const fn is_verified(self) -> bool {
        matches!(self, Self::VerifiedO200k)
    }
}

#[derive(Debug, Clone, Copy)]
struct RolloutConfig {
    mode: RolloutMode,
    canary_basis_points: u16,
}

#[derive(Debug, Clone, Copy)]
pub struct RolloutDecision {
    pub mode: RolloutMode,
    pub compatibility: TokenizerCompatibility,
    pub measure_exact: bool,
    pub enforce_exact: bool,
    pub canary_selected: bool,
    pub canary_basis_points: u16,
}

impl RolloutDecision {
    pub const fn budget_basis(self) -> &'static str {
        if self.enforce_exact {
            "o200k_exact"
        } else {
            "serialized_wire_proxy"
        }
    }

    pub const fn arm_label(self) -> &'static str {
        if self.enforce_exact {
            "exact"
        } else if matches!(self.mode, RolloutMode::Shadow) {
            "shadow"
        } else if matches!(self.mode, RolloutMode::Enforce) {
            "control"
        } else {
            "proxy"
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TokenMeasurement {
    pub proxy_tokens: usize,
    pub exact_tokens: usize,
    pub delta_tokens: i64,
    pub exact_to_proxy_ratio: f64,
    pub count_latency_us: u64,
}

/// Information the transport knows but a tool handler normally cannot see.
///
/// The task-local scope lets context/search measure the same result object,
/// real JSON-RPC id, decorations, field spelling, and framing the transport
/// will serialize after the handler returns.
#[derive(Debug, Clone, Default)]
pub struct WireTokenObservation {
    decision: Arc<OnceLock<RolloutDecision>>,
}

impl WireTokenObservation {
    pub fn register(&self, decision: RolloutDecision) {
        let _ = self.decision.set(decision);
    }

    pub fn decision(&self) -> Option<RolloutDecision> {
        self.decision.get().copied()
    }
}

#[derive(Debug, Clone)]
pub struct WireResponseContext {
    pub transport: TransportView,
    pub jsonrpc_id: Option<Value>,
    pub call_title: Option<String>,
    pub call_icon: Option<String>,
    /// HTTP forwards structured results by default. Stdio preserves its
    /// established text-only payload unless a bounded recovery response opts
    /// in explicitly.
    pub include_structured: bool,
    observation: WireTokenObservation,
}

impl WireResponseContext {
    pub fn http_jsonrpc(
        jsonrpc_id: Option<Value>,
        call_title: Option<String>,
        call_icon: Option<String>,
    ) -> Self {
        Self {
            transport: TransportView::McpHttpJsonRpcV2,
            jsonrpc_id,
            call_title,
            call_icon,
            include_structured: true,
            observation: WireTokenObservation::default(),
        }
    }

    pub fn stdio_jsonrpc(
        jsonrpc_id: Option<Value>,
        call_title: Option<String>,
        call_icon: Option<String>,
    ) -> Self {
        Self {
            transport: TransportView::McpStdioJsonRpcV2,
            jsonrpc_id,
            call_title,
            call_icon,
            include_structured: false,
            observation: WireTokenObservation::default(),
        }
    }

    pub fn http_rest(call_title: Option<String>, call_icon: Option<String>) -> Self {
        Self {
            transport: TransportView::McpHttpRestToolResultV1,
            jsonrpc_id: None,
            call_title,
            call_icon,
            include_structured: true,
            observation: WireTokenObservation::default(),
        }
    }

    pub fn tool_result_body() -> Self {
        Self {
            transport: TransportView::ToolResultBody,
            jsonrpc_id: None,
            call_title: None,
            call_icon: None,
            include_structured: true,
            observation: WireTokenObservation::default(),
        }
    }

    pub fn with_observation(mut self, observation: WireTokenObservation) -> Self {
        self.observation = observation;
        self
    }

    /// Opt this response into or out of standard MCP `structuredContent`.
    ///
    /// This is intentionally per-call. Enabling structured output globally on
    /// stdio would duplicate every large search/context result and regress
    /// clients that render the complete JSON envelope inline.
    pub fn with_structured_content(mut self, include_structured: bool) -> Self {
        self.include_structured = include_structured;
        self
    }

    pub fn register_rollout_decision(&self, decision: RolloutDecision) {
        self.observation.register(decision);
    }

    pub fn observation(&self) -> WireTokenObservation {
        self.observation.clone()
    }
}

tokio::task_local! {
    static WIRE_RESPONSE_CONTEXT: WireResponseContext;
}

pub async fn run_with_wire_response_context<F>(context: WireResponseContext, future: F) -> F::Output
where
    F: Future,
{
    WIRE_RESPONSE_CONTEXT.scope(context, future).await
}

pub fn current_wire_response_context() -> WireResponseContext {
    WIRE_RESPONSE_CONTEXT
        .try_with(Clone::clone)
        .unwrap_or_else(|_| WireResponseContext::tool_result_body())
}

/// Warm and retain the production tokenizer singleton before serving traffic.
pub fn warm_o200k() -> Duration {
    let started = Instant::now();
    let initialized_now = O200K.get().is_none();
    O200K.get_or_init(o200k_base_singleton);
    let elapsed = started.elapsed();
    histogram!("mcp_wire_tokenizer_warm_latency_ms", "encoding" => O200K_ENCODING)
        .record(elapsed.as_secs_f64() * 1_000.0);
    if initialized_now {
        info!(
            encoding = O200K_ENCODING,
            implementation = O200K_IMPLEMENTATION,
            implementation_version = O200K_IMPLEMENTATION_VERSION,
            elapsed_ms = elapsed.as_millis() as u64,
            "warmed MCP whole-wire tokenizer"
        );
    }
    elapsed
}

pub fn o200k_is_warm() -> bool {
    O200K.get().is_some()
}

pub fn byte_proxy_tokens(bytes: &[u8]) -> usize {
    bytes.len().div_ceil(4)
}

pub fn tokenizer_compatibility(hint: Option<&str>) -> TokenizerCompatibility {
    let normalized = hint
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase);
    match normalized.as_deref() {
        Some("o200k")
        | Some("o200k_base")
        | Some("openai:o200k_base")
        | Some("openai/o200k_base") => TokenizerCompatibility::VerifiedO200k,
        _ => TokenizerCompatibility::UnknownOrNonO200k,
    }
}

fn rollout_config() -> RolloutConfig {
    *ROLLOUT_CONFIG.get_or_init(|| {
        let mode = RolloutMode::from_env_value(std::env::var(TOKENIZER_MODE_ENV).ok());
        let canary_percent = std::env::var(TOKENIZER_CANARY_PERCENT_ENV)
            .ok()
            .and_then(|value| value.trim().parse::<f64>().ok())
            .filter(|value| value.is_finite())
            .unwrap_or(0.0)
            .clamp(0.0, 100.0);
        RolloutConfig {
            mode,
            canary_basis_points: (canary_percent * 100.0).round() as u16,
        }
    })
}

fn stable_canary_bucket(key: &str) -> u16 {
    let digest = Sha256::digest(key.as_bytes());
    let value = u16::from_be_bytes([digest[0], digest[1]]);
    ((u32::from(value) * 10_000) / 65_536) as u16
}

/// Build a deterministic rollout cohort from stable caller/scope identity.
///
/// Authenticated caller/workspace/project identity is hierarchical over the
/// caller-controlled session id. A session participates only when no stable
/// tenant/scope identity exists, preventing clients from hopping canary arms by
/// rotating or selecting session ids. Request text remains the final anonymous
/// fallback.
pub fn stable_cohort_key(
    caller_identity: Option<&str>,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    session_id: Option<&str>,
    anonymous_fallback: &str,
) -> String {
    fn update_field(hasher: &mut Sha256, label: &str, value: &str) {
        hasher.update((label.len() as u64).to_be_bytes());
        hasher.update(label.as_bytes());
        hasher.update((value.len() as u64).to_be_bytes());
        hasher.update(value.as_bytes());
    }

    let mut hasher = Sha256::new();
    update_field(&mut hasher, "version", TOKENIZER_CACHE_BASIS_VERSION);
    let mut stable_identity = false;
    for (label, value) in [
        ("caller_identity", caller_identity),
        ("workspace_id", workspace_id),
        ("project_id", project_id),
    ] {
        if let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) {
            update_field(&mut hasher, label, value);
            stable_identity = true;
        }
    }
    if !stable_identity {
        if let Some(value) = session_id.map(str::trim).filter(|value| !value.is_empty()) {
            update_field(&mut hasher, "session_id", value);
            stable_identity = true;
        }
    }
    if !stable_identity {
        update_field(&mut hasher, "anonymous_fallback", anonymous_fallback);
    }
    format!("{:x}", hasher.finalize())
}

pub fn rollout_decision(tokenizer_hint: Option<&str>, canary_key: &str) -> RolloutDecision {
    rollout_decision_with_config(tokenizer_hint, canary_key, rollout_config())
}

/// Resolve rollout once for the concrete request transport. Handler-only body
/// execution (tests or batch nesting) has no single final MCP envelope to
/// observe, so it remains pure proxy rather than claiming an exact wire basis
/// for the wrong bytes. Supported legacy REST calls install their own concrete
/// transport view.
pub fn rollout_decision_for_context(
    tokenizer_hint: Option<&str>,
    canary_key: &str,
    context: &WireResponseContext,
) -> RolloutDecision {
    constrain_decision_to_context(rollout_decision(tokenizer_hint, canary_key), context)
}

fn constrain_decision_to_context(
    mut decision: RolloutDecision,
    context: &WireResponseContext,
) -> RolloutDecision {
    if matches!(context.transport, TransportView::ToolResultBody) {
        decision.measure_exact = false;
        decision.enforce_exact = false;
        decision.canary_selected = false;
    }
    decision
}

fn rollout_decision_with_config(
    tokenizer_hint: Option<&str>,
    canary_key: &str,
    config: RolloutConfig,
) -> RolloutDecision {
    let compatibility = tokenizer_compatibility(tokenizer_hint);
    let measure_exact = !matches!(config.mode, RolloutMode::Proxy) && compatibility.is_verified();
    let canary_selected = matches!(config.mode, RolloutMode::Enforce)
        && compatibility.is_verified()
        && config.canary_basis_points > 0
        && stable_canary_bucket(canary_key) < config.canary_basis_points;
    RolloutDecision {
        mode: config.mode,
        compatibility,
        measure_exact,
        enforce_exact: canary_selected,
        canary_selected,
        canary_basis_points: config.canary_basis_points,
    }
}

/// Namespace every cache entry whose rendered output can vary by rollout arm.
pub fn cache_namespace(tokenizer_hint: Option<&str>, canary_key: &str) -> String {
    cache_namespace_for_decision(rollout_decision(tokenizer_hint, canary_key))
}

pub fn cache_namespace_for_decision(decision: RolloutDecision) -> String {
    if decision.enforce_exact && o200k_is_warm() {
        format!(
            "{}:{}:{}:{}:exact",
            TOKENIZER_CACHE_BASIS_VERSION,
            O200K_ENCODING,
            O200K_IMPLEMENTATION,
            O200K_IMPLEMENTATION_VERSION,
        )
    } else {
        // Shadow, incompatible hints, unselected canaries, cold startup, and
        // handler-only body execution serve proxy bytes and share its cache.
        format!("{}:proxy", TOKENIZER_CACHE_BASIS_VERSION)
    }
}

#[cfg(test)]
fn cache_namespace_with_config(
    tokenizer_hint: Option<&str>,
    canary_key: &str,
    config: RolloutConfig,
) -> String {
    cache_namespace_for_decision(rollout_decision_with_config(
        tokenizer_hint,
        canary_key,
        config,
    ))
}

fn stdio_content_value(item: &ContentItem) -> Value {
    match item {
        ContentItem::Text { text } => json!({"type": "text", "text": text}),
        ContentItem::Image { data, mime_type } => {
            json!({"type": "image", "data": data, "mimeType": mime_type})
        }
        ContentItem::Resource { uri, mime_type } => {
            json!({"type": "resource", "uri": uri, "mimeType": mime_type})
        }
    }
}

/// Build the exact result payload emitted by the selected MCP transport.
pub fn tool_result_payload(result: &ToolResult, context: &WireResponseContext) -> Value {
    let content = if matches!(context.transport, TransportView::McpStdioJsonRpcV2) {
        Value::Array(result.content.iter().map(stdio_content_value).collect())
    } else {
        serde_json::to_value(&result.content).unwrap_or_else(|_| Value::Array(Vec::new()))
    };
    let mut response = json!({
        "content": content,
        "isError": result.is_error,
    });
    if context.include_structured && structured_content_enabled() {
        if let Some(structured) = result
            .structured_content
            .as_ref()
            .and_then(as_structured_object)
        {
            let field = if matches!(context.transport, TransportView::McpHttpRestToolResultV1) {
                // The pre-standard REST endpoint historically exposed this
                // ContextStream-specific field. Preserve it there only.
                "structured"
            } else {
                "structuredContent"
            };
            response[field] = structured;
        }
    }
    if let Some(title) = context.call_title.as_ref() {
        response["title"] = Value::String(title.clone());
        response["_meta"] = json!({
            "contextstream": {
                "title": title,
                "icon": context.call_icon.as_deref().unwrap_or("⌬"),
            }
        });
    }
    response
}

#[derive(Serialize)]
struct JsonRpcSuccess<'a> {
    jsonrpc: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: &'a Option<Value>,
    result: &'a Value,
}

/// Serialize the bytes a caller receives, including the real JSON-RPC id and
/// the stdio line terminator. The body-only variant is explicitly labelled and
/// used only when execution did not occur inside an MCP transport scope.
pub fn canonical_tool_result_bytes(
    result: &ToolResult,
    context: &WireResponseContext,
) -> Result<Vec<u8>, serde_json::Error> {
    let payload = tool_result_payload(result, context);
    match context.transport {
        TransportView::ToolResultBody => serde_json::to_vec(&payload),
        TransportView::McpHttpRestToolResultV1 => serde_json::to_vec(&payload),
        TransportView::McpHttpJsonRpcV2 => {
            let envelope = JsonRpcSuccess {
                jsonrpc: "2.0",
                id: &context.jsonrpc_id,
                result: &payload,
            };
            let bytes = serde_json::to_vec(&envelope)?;
            Ok(bytes)
        }
        TransportView::McpStdioJsonRpcV2 => {
            // Match `server::json_rpc_result`: this is intentionally a Value
            // map rather than the HTTP response struct, so feature-dependent
            // serde_json map ordering remains identical to production.
            let envelope = json!({
                "jsonrpc": "2.0",
                "id": context.jsonrpc_id,
                "result": payload,
            });
            let mut bytes = serde_json::to_vec(&envelope)?;
            bytes.push(b'\n');
            Ok(bytes)
        }
    }
}

/// Count an exact UTF-8 wire view without allocating a token vector or
/// emitting public telemetry. Semantic enforcement and fixed-point reporting
/// use this internal view; only the final transport boundary records the
/// public response distribution.
pub fn measure_utf8(bytes: &[u8], _surface: &'static str) -> Option<TokenMeasurement> {
    let proxy_tokens = byte_proxy_tokens(bytes);
    let text = std::str::from_utf8(bytes).ok()?;
    let tokenizer = O200K.get().copied()?;
    let started = Instant::now();
    let exact_tokens = tokenizer.count_ordinary(text);
    let count_latency_us = started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
    let delta_tokens = exact_tokens as i64 - proxy_tokens as i64;
    let exact_to_proxy_ratio = if proxy_tokens == 0 {
        if exact_tokens == 0 {
            1.0
        } else {
            exact_tokens as f64
        }
    } else {
        exact_tokens as f64 / proxy_tokens as f64
    };

    Some(TokenMeasurement {
        proxy_tokens,
        exact_tokens,
        delta_tokens,
        exact_to_proxy_ratio,
        count_latency_us,
    })
}

pub fn measure_tool_result(
    result: &ToolResult,
    context: &WireResponseContext,
    surface: &'static str,
) -> Option<TokenMeasurement> {
    let bytes = canonical_tool_result_bytes(result, context).ok()?;
    measure_utf8(&bytes, surface)
}

/// Transport-boundary reconciliation for bytes that are already serialized.
/// Exactly one public exact-count sample is emitted for an eligible returned
/// response. The request-scoped decision prevents unknown/non-o200k callers
/// and non-tokenized tool calls from doing any exact work.
pub fn observe_final_wire_bytes(
    bytes: &[u8],
    observation: &WireTokenObservation,
    surface: &'static str,
) -> Option<TokenMeasurement> {
    let decision = observation.decision()?;
    if !decision.measure_exact || !decision.compatibility.is_verified() {
        return None;
    }
    let Some(measurement) = measure_utf8(bytes, surface) else {
        counter!(
            "mcp_wire_tokenizer_final_count_total",
            "surface" => surface,
            "mode" => decision.mode.label(),
            "compatibility" => decision.compatibility.label(),
            "arm" => decision.arm_label(),
            "result" => if o200k_is_warm() { "invalid_utf8" } else { "not_warmed" },
        )
        .increment(1);
        warn!(
            surface,
            mode = decision.mode.label(),
            arm = decision.arm_label(),
            "MCP final whole-wire exact count failed; proxy behavior was retained"
        );
        return None;
    };

    counter!(
        "mcp_wire_tokenizer_final_count_total",
        "surface" => surface,
        "mode" => decision.mode.label(),
        "compatibility" => decision.compatibility.label(),
        "arm" => decision.arm_label(),
        "result" => "success",
    )
    .increment(1);
    histogram!(
        "mcp_wire_tokenizer_final_count_latency_us",
        "surface" => surface,
        "mode" => decision.mode.label(),
        "compatibility" => decision.compatibility.label(),
        "arm" => decision.arm_label(),
        "encoding" => O200K_ENCODING,
    )
    .record(measurement.count_latency_us as f64);
    histogram!(
        "mcp_wire_tokenizer_final_proxy_tokens",
        "surface" => surface,
        "mode" => decision.mode.label(),
        "compatibility" => decision.compatibility.label(),
        "arm" => decision.arm_label(),
    )
    .record(measurement.proxy_tokens as f64);
    histogram!(
        "mcp_wire_tokenizer_final_exact_tokens",
        "surface" => surface,
        "mode" => decision.mode.label(),
        "compatibility" => decision.compatibility.label(),
        "arm" => decision.arm_label(),
        "encoding" => O200K_ENCODING,
    )
    .record(measurement.exact_tokens as f64);
    histogram!(
        "mcp_wire_tokenizer_final_exact_minus_proxy_tokens",
        "surface" => surface,
        "mode" => decision.mode.label(),
        "compatibility" => decision.compatibility.label(),
        "arm" => decision.arm_label(),
    )
    .record(measurement.delta_tokens as f64);
    histogram!(
        "mcp_wire_tokenizer_final_exact_to_proxy_ratio",
        "surface" => surface,
        "mode" => decision.mode.label(),
        "compatibility" => decision.compatibility.label(),
        "arm" => decision.arm_label(),
    )
    .record(measurement.exact_to_proxy_ratio);
    Some(measurement)
}

#[derive(Debug, Clone, Copy, Default)]
pub struct EnforcementReport {
    pub target_tokens: usize,
    pub before: Option<TokenMeasurement>,
    pub iterations: usize,
    pub hard_floor_exceeded: bool,
}

fn report_value(
    decision: RolloutDecision,
    context: &WireResponseContext,
    report: EnforcementReport,
    final_measurement: TokenMeasurement,
) -> Value {
    // Keep scheduler timing out of the self-referential fixed-point loop. The
    // immutable pre-report measurement is still useful diagnostic context,
    // while the actual final count latency is emitted separately at the
    // transport boundary. Feeding each pass's wall-clock latency back into
    // the JSON can otherwise oscillate under CI/production CPU contention and
    // make an otherwise truthful report fail closed as non-convergent.
    let stable_count_latency_us = report
        .before
        .map(|measurement| measurement.count_latency_us)
        .unwrap_or_default();
    json!({
        "basis_version": TOKENIZER_CACHE_BASIS_VERSION,
        "encoding": O200K_ENCODING,
        "implementation": O200K_IMPLEMENTATION,
        "implementation_version": O200K_IMPLEMENTATION_VERSION,
        "transport": context.transport.label(),
        "mode": decision.mode.label(),
        "compatibility": decision.compatibility.label(),
        "canary_selected": decision.canary_selected,
        "enforced": decision.enforce_exact,
        "budget_basis": decision.budget_basis(),
        "target_tokens": report.target_tokens,
        "proxy_tokens_before": report.before.map(|m| m.proxy_tokens),
        "exact_tokens_before": report.before.map(|m| m.exact_tokens),
        "proxy_tokens_final": final_measurement.proxy_tokens,
        "exact_tokens_final": final_measurement.exact_tokens,
        "exact_minus_proxy_final": final_measurement.delta_tokens,
        "exact_to_proxy_ratio_final": final_measurement.exact_to_proxy_ratio,
        "count_latency_us": stable_count_latency_us,
        "enforcement_iterations": report.iterations,
        "hard_floor_exceeded": report.hard_floor_exceeded,
    })
}

/// Attach a bounded fixed-point report and remeasure the exact final bytes.
///
/// The report is emitted only on transports that actually serialize structured
/// content. If a pathological value does not converge within the bound, the
/// report is removed rather than publishing a knowingly false self-count.
pub fn attach_fixed_point_report(
    result: &mut ToolResult,
    decision: RolloutDecision,
    context: &WireResponseContext,
    surface: &'static str,
    report: EnforcementReport,
) -> Option<TokenMeasurement> {
    attach_fixed_point_report_with_measure(result, decision, context, surface, report, |result| {
        measure_tool_result(result, context, surface)
    })
}

fn attach_fixed_point_report_with_measure(
    result: &mut ToolResult,
    decision: RolloutDecision,
    context: &WireResponseContext,
    surface: &'static str,
    report: EnforcementReport,
    mut measure: impl FnMut(&ToolResult) -> Option<TokenMeasurement>,
) -> Option<TokenMeasurement> {
    if !decision.enforce_exact || !context.include_structured {
        return measure(result);
    }
    // An enforce-selected request may still arrive before startup warmup in a
    // test harness or an alternate embedding. Fail closed without inserting a
    // report that would falsely claim an exact basis.
    if !o200k_is_warm() {
        return None;
    }
    let Some(Value::Object(structured)) = result.structured_content.as_mut() else {
        return measure(result);
    };
    structured.insert(WIRE_REPORT_KEY.to_string(), json!({}));

    let mut previous: Option<(usize, usize)> = None;
    for _ in 0..REPORT_FIXED_POINT_LIMIT {
        let Some(measurement) = measure(result) else {
            if let Some(Value::Object(structured)) = result.structured_content.as_mut() {
                structured.remove(WIRE_REPORT_KEY);
            }
            counter!("mcp_wire_tokenizer_report_total", "surface" => surface, "result" => "count_failed")
                .increment(1);
            return None;
        };
        let pair = (measurement.proxy_tokens, measurement.exact_tokens);
        if previous == Some(pair) {
            return Some(measurement);
        }
        previous = Some(pair);
        let Some(Value::Object(structured)) = result.structured_content.as_mut() else {
            return Some(measurement);
        };
        structured.insert(
            WIRE_REPORT_KEY.to_string(),
            report_value(decision, context, report, measurement),
        );
    }

    if let Some(Value::Object(structured)) = result.structured_content.as_mut() {
        structured.remove(WIRE_REPORT_KEY);
    }
    counter!("mcp_wire_tokenizer_report_total", "surface" => surface, "result" => "non_convergent")
        .increment(1);
    measure(result)
}

pub fn fixed_point_report_is_truthful(
    result: &ToolResult,
    decision: RolloutDecision,
    target_tokens: usize,
    measurement: TokenMeasurement,
) -> bool {
    let Some(report) = result
        .structured_content
        .as_ref()
        .and_then(|value| value.get(WIRE_REPORT_KEY))
    else {
        return false;
    };
    report.get("enforced").and_then(Value::as_bool) == Some(decision.enforce_exact)
        && report.get("target_tokens").and_then(Value::as_u64) == Some(target_tokens as u64)
        && report.get("proxy_tokens_final").and_then(Value::as_u64)
            == Some(measurement.proxy_tokens as u64)
        && report.get("exact_tokens_final").and_then(Value::as_u64)
            == Some(measurement.exact_tokens as u64)
        && report.get("hard_floor_exceeded").and_then(Value::as_bool)
            == Some(measurement.exact_tokens > target_tokens)
}

pub fn remove_fixed_point_report(result: &mut ToolResult) -> bool {
    result
        .structured_content
        .as_mut()
        .and_then(Value::as_object_mut)
        .and_then(|structured| structured.remove(WIRE_REPORT_KEY))
        .is_some()
}

pub fn record_hard_floor_resolution(surface: &'static str, outcome: &'static str) {
    counter!(
        "mcp_wire_tokenizer_hard_floor_total",
        "surface" => surface,
        "outcome" => outcome,
    )
    .increment(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_transport_views_use_real_id_and_stdio_lf() {
        let result = ToolResult::with_structured("hello", json!({"answer": "world"}));
        let http = WireResponseContext::http_jsonrpc(
            Some(json!("request-with-variable-id")),
            Some("Searching ContextStream index".to_string()),
            Some("⌕".to_string()),
        );
        let stdio = WireResponseContext::stdio_jsonrpc(
            Some(json!("request-with-variable-id")),
            Some("Searching ContextStream index".to_string()),
            Some("⌕".to_string()),
        );
        let structured_stdio = stdio.clone().with_structured_content(true);
        let http_bytes = canonical_tool_result_bytes(&result, &http).unwrap();
        let stdio_bytes = canonical_tool_result_bytes(&result, &stdio).unwrap();
        let structured_stdio_bytes =
            canonical_tool_result_bytes(&result, &structured_stdio).unwrap();
        let rest = WireResponseContext::http_rest(
            Some("Searching ContextStream index".to_string()),
            Some("⌕".to_string()),
        );
        let rest_bytes = canonical_tool_result_bytes(&result, &rest).unwrap();
        assert_ne!(http_bytes, stdio_bytes);
        assert_ne!(http_bytes, rest_bytes);
        assert_ne!(http_bytes.last(), Some(&b'\n'));
        assert_ne!(rest_bytes.last(), Some(&b'\n'));
        assert_eq!(stdio_bytes.last(), Some(&b'\n'));
        let http_json: Value = serde_json::from_slice(&http_bytes).unwrap();
        assert_eq!(http_json["result"]["structuredContent"]["answer"], "world");
        assert!(http_json["result"].get("structured").is_none());
        let stdio_json: Value = serde_json::from_slice(&stdio_bytes).unwrap();
        assert!(stdio_json["result"].get("structuredContent").is_none());
        let structured_stdio_json: Value = serde_json::from_slice(&structured_stdio_bytes).unwrap();
        assert_eq!(
            structured_stdio_json["result"]["structuredContent"]["answer"],
            "world"
        );
        assert!(structured_stdio_json["result"].get("structured").is_none());
        assert!(String::from_utf8(http_bytes)
            .unwrap()
            .contains("request-with-variable-id"));
        let rest_json: Value = serde_json::from_slice(&rest_bytes).unwrap();
        assert!(rest_json.get("jsonrpc").is_none());
        assert_eq!(rest_json["content"][0]["text"], "hello");
        assert_eq!(rest_json["structured"]["answer"], "world");
        assert!(rest_json.get("structuredContent").is_none());
    }

    #[test]
    fn http_structured_content_coerces_arrays_to_objects() {
        let result = ToolResult {
            content: vec![ContentItem::text("Found 1")],
            structured_content: Some(json!([{"id": "n1"}])),
            is_error: false,
        };
        let http = WireResponseContext::http_jsonrpc(Some(json!(1)), None, None);
        let payload = tool_result_payload(&result, &http);
        assert!(payload["structuredContent"].is_object());
        assert_eq!(payload["structuredContent"]["items"][0]["id"], "n1");
    }

    #[test]
    fn rollout_is_fail_closed_for_unknown_tokenizers() {
        let config = RolloutConfig {
            mode: RolloutMode::Enforce,
            canary_basis_points: 10_000,
        };
        assert!(rollout_decision_with_config(Some("o200k_base"), "key", config).enforce_exact);
        for hint in [None, Some("claude"), Some("gemini"), Some("unknown")] {
            let decision = rollout_decision_with_config(hint, "key", config);
            assert!(!decision.measure_exact);
            assert!(!decision.enforce_exact);
        }
        let shadow = rollout_decision_with_config(
            Some("o200k_base"),
            "key",
            RolloutConfig {
                mode: RolloutMode::Shadow,
                canary_basis_points: 0,
            },
        );
        assert!(shadow.measure_exact);
        assert!(!shadow.enforce_exact);

        let exact = rollout_decision_with_config(
            Some("o200k_base"),
            "key",
            RolloutConfig {
                mode: RolloutMode::Enforce,
                canary_basis_points: 10_000,
            },
        );
        let body = constrain_decision_to_context(exact, &WireResponseContext::tool_result_body());
        assert!(!body.measure_exact);
        assert!(!body.enforce_exact);
        assert!(!body.canary_selected);
    }

    #[test]
    fn final_observation_requires_a_verified_request_decision() {
        warm_o200k();
        let bytes = br#"{"jsonrpc":"2.0","id":1,"result":{"content":[]}}"#;

        let missing = WireTokenObservation::default();
        assert!(observe_final_wire_bytes(bytes, &missing, "wire_observation_missing").is_none());

        let unknown = WireTokenObservation::default();
        unknown.register(rollout_decision_with_config(
            Some("claude"),
            "unknown",
            RolloutConfig {
                mode: RolloutMode::Shadow,
                canary_basis_points: 0,
            },
        ));
        assert!(observe_final_wire_bytes(bytes, &unknown, "wire_observation_unknown").is_none());

        let verified = WireTokenObservation::default();
        verified.register(rollout_decision_with_config(
            Some("o200k_base"),
            "verified",
            RolloutConfig {
                mode: RolloutMode::Shadow,
                canary_basis_points: 0,
            },
        ));
        assert!(observe_final_wire_bytes(bytes, &verified, "wire_observation_verified").is_some());
    }

    #[tokio::test]
    async fn task_local_decision_reaches_http_and_stdio_final_observers() {
        warm_o200k();
        let decision = rollout_decision_with_config(
            Some("o200k_base"),
            "stable-transport-cohort",
            RolloutConfig {
                mode: RolloutMode::Shadow,
                canary_basis_points: 0,
            },
        );
        let result = ToolResult::with_structured("grounded", json!({"answer": true}));

        for context in [
            WireResponseContext::http_jsonrpc(Some(json!(42)), None, None),
            WireResponseContext::stdio_jsonrpc(Some(json!(42)), None, None),
            WireResponseContext::http_rest(None, None),
        ] {
            let observation = context.observation();
            run_with_wire_response_context(context.clone(), async move {
                current_wire_response_context().register_rollout_decision(decision);
            })
            .await;
            let bytes = canonical_tool_result_bytes(&result, &context).unwrap();
            assert!(observe_final_wire_bytes(
                &bytes,
                &observation,
                "wire_observation_transport_test",
            )
            .is_some());
        }
    }

    #[test]
    fn cache_namespace_covers_rollout_compatibility_and_arm() {
        warm_o200k();
        let proxy = RolloutConfig {
            mode: RolloutMode::Proxy,
            canary_basis_points: 0,
        };
        let shadow = RolloutConfig {
            mode: RolloutMode::Shadow,
            canary_basis_points: 10_000,
        };
        let exact = RolloutConfig {
            mode: RolloutMode::Enforce,
            canary_basis_points: 10_000,
        };
        let a = cache_namespace_with_config(None, "key", proxy);
        let shadow_known = cache_namespace_with_config(Some("o200k_base"), "key", shadow);
        let b = cache_namespace_with_config(None, "key", exact);
        let c = cache_namespace_with_config(Some("o200k_base"), "key", exact);
        assert_eq!(a, shadow_known);
        assert_eq!(a, b);
        assert_ne!(b, c);
        assert!(c.contains(O200K_IMPLEMENTATION_VERSION));
        assert!(c.ends_with(":exact"));
    }

    #[test]
    fn canary_selection_is_deterministic_and_near_configured_percentage() {
        let config = RolloutConfig {
            mode: RolloutMode::Enforce,
            canary_basis_points: 1_000,
        };
        let selected = (0..10_000)
            .filter(|index| {
                rollout_decision_with_config(
                    Some("o200k_base"),
                    &format!("request-{index}"),
                    config,
                )
                .canary_selected
            })
            .count();
        assert!((850..=1_150).contains(&selected), "selected={selected}");
        assert_eq!(
            rollout_decision_with_config(Some("o200k_base"), "stable", config).canary_selected,
            rollout_decision_with_config(Some("o200k_base"), "stable", config).canary_selected,
        );
    }

    #[test]
    fn stable_identity_cohorts_distribute_common_prompts_and_repeat() {
        let common_prompt = "where is authentication implemented?";
        let workspace_id = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
        let selected = (0..10_000)
            .filter(|index| {
                let caller = format!("user-{index}");
                let key = stable_cohort_key(
                    Some(&caller),
                    Some(workspace_id),
                    None,
                    Some("shared-session-shape"),
                    common_prompt,
                );
                stable_canary_bucket(&key) < 1_000
            })
            .count();
        assert!((850..=1_150).contains(&selected), "selected={selected}");

        let first = stable_cohort_key(
            Some("stable-user"),
            Some(workspace_id),
            Some("project-a"),
            Some("session-a"),
            common_prompt,
        );
        let repeated = stable_cohort_key(
            Some("stable-user"),
            Some(workspace_id),
            Some("project-a"),
            Some("attacker-selected-session-b"),
            "a different prompt must not move an identified cohort",
        );
        assert_eq!(first, repeated);
        assert_ne!(
            first,
            stable_cohort_key(
                Some("different-user"),
                Some(workspace_id),
                Some("project-a"),
                Some("session-a"),
                common_prompt,
            )
        );

        // Workspace/project identity is also stable when caller identity is
        // unavailable; a caller-controlled session id cannot canary-hop.
        assert_eq!(
            stable_cohort_key(
                None,
                Some(workspace_id),
                Some("project-a"),
                Some("session-a"),
                common_prompt,
            ),
            stable_cohort_key(
                None,
                Some(workspace_id),
                Some("project-a"),
                Some("session-z"),
                "different prompt",
            )
        );

        // Session is a legitimate last-resort cohort only when no stable
        // tenant/scope identity is available.
        assert_ne!(
            stable_cohort_key(None, None, None, Some("session-a"), common_prompt),
            stable_cohort_key(None, None, None, Some("session-b"), common_prompt),
        );
    }

    #[test]
    fn pinned_o200k_matches_reference_corpus() {
        warm_o200k();
        let corpus = [
            ("ContextStream returns grounded evidence quickly, with less noise and fewer tokens.", 14),
            ("fn route<T: Send + Sync>(value: T) -> Result<T, Error> { Ok(value) }\nconst parseUserID = (raw: string): UserId => UserId.parse(raw);", 41),
            (r#"{"path":"C:\\repo\\src\\lib.rs","escaped":"\\n\\t\\u003c","items":[1,2,3],"ok":true}"#, 35),
            ("数据库查询如何工作？検索結果を高速に返します。", 13),
            ("👩‍💻 family 👨‍👩‍👧‍👦 e\u{301} flags 🇺🇸🇯🇵", 28),
            (
                "0123456789abcdef0123456789abcdef dGVzdC1wYXlsb2FkLTAxMjM0NTY3ODk= 550e8400-e29b-41d4-a716-446655440000 /repo/src/http/router.rs",
                58,
            ),
            ("before <|endoftext|> after", 9),
            (
                concat!(
                    "Evidence: auth middleware lives in crates/api/src/auth.rs and validates bearer tokens. ",
                    "Evidence: auth middleware lives in crates/api/src/auth.rs and validates bearer tokens. ",
                    "Evidence: auth middleware lives in crates/api/src/auth.rs and validates bearer tokens. ",
                    "Evidence: auth middleware lives in crates/api/src/auth.rs and validates bearer tokens. "
                ),
                65,
            ),
        ];
        let tokenizer = O200K.get().copied().unwrap();
        for (text, expected) in corpus {
            assert_eq!(tokenizer.count_ordinary(text), expected, "{text:?}");
        }
    }

    #[test]
    fn fixed_point_report_matches_final_http_wire_bytes() {
        warm_o200k();
        let context = WireResponseContext::http_jsonrpc(
            Some(json!(42)),
            Some("Loading ContextStream context".to_string()),
            Some("⌬".to_string()),
        );
        let decision = rollout_decision_with_config(
            Some("o200k_base"),
            "key",
            RolloutConfig {
                mode: RolloutMode::Enforce,
                canary_basis_points: 10_000,
            },
        );
        let mut result = ToolResult::with_structured(
            "数据库 👩‍💻 escaped \\\"json\\\"",
            json!({"answer": "数据库 👩‍💻", "valid": true}),
        );
        let final_measurement = attach_fixed_point_report(
            &mut result,
            decision,
            &context,
            "wire_tokens_test",
            EnforcementReport {
                target_tokens: 4_096,
                ..Default::default()
            },
        )
        .unwrap();
        let reported = &result.structured_content.as_ref().unwrap()[WIRE_REPORT_KEY];
        assert_eq!(
            reported["exact_tokens_final"],
            final_measurement.exact_tokens
        );
        assert_eq!(
            reported["proxy_tokens_final"],
            final_measurement.proxy_tokens
        );
        assert!(serde_json::from_slice::<Value>(
            &canonical_tool_result_bytes(&result, &context).unwrap()
        )
        .is_ok());
    }

    #[test]
    fn fixed_point_report_ignores_volatile_measurement_latency() {
        warm_o200k();
        let context = WireResponseContext::http_jsonrpc(
            Some(json!("latency-jitter")),
            Some("Loading ContextStream context".to_string()),
            Some("⌬".to_string()),
        );
        let decision = rollout_decision_with_config(
            Some("o200k_base"),
            "key",
            RolloutConfig {
                mode: RolloutMode::Enforce,
                canary_basis_points: 10_000,
            },
        );
        let mut result = ToolResult::with_structured(
            "database grounding",
            json!({"answer": "grounded", "valid": true}),
        );
        let before = measure_tool_result(&result, &context, "wire_tokens_latency_before")
            .expect("the warmed tokenizer measures the pre-report wire");
        let stable_latency = before.count_latency_us;
        let mut measurement_index = 0usize;

        let final_measurement = attach_fixed_point_report_with_measure(
            &mut result,
            decision,
            &context,
            "wire_tokens_latency_jitter_test",
            EnforcementReport {
                target_tokens: 4_096,
                before: Some(before),
                ..Default::default()
            },
            |result| {
                measurement_index += 1;
                let mut measurement =
                    measure_tool_result(result, &context, "wire_tokens_latency_jitter_measure")?;
                measurement.count_latency_us = if measurement_index.is_multiple_of(2) {
                    u64::MAX
                } else {
                    1
                };
                Some(measurement)
            },
        )
        .expect("latency jitter must not prevent token-count convergence");

        let report = &result.structured_content.as_ref().unwrap()[WIRE_REPORT_KEY];
        assert_eq!(report["count_latency_us"], stable_latency);
        assert_eq!(report["exact_tokens_final"], final_measurement.exact_tokens);
        assert_eq!(report["proxy_tokens_final"], final_measurement.proxy_tokens);
        assert!(fixed_point_report_is_truthful(
            &result,
            decision,
            4_096,
            final_measurement
        ));
    }

    #[test]
    fn fixed_point_report_fails_closed_for_a_non_convergent_wire_shape() {
        warm_o200k();
        let context = WireResponseContext::http_jsonrpc(
            Some(json!(987654321)),
            Some("Loading ContextStream context".to_string()),
            Some("⌬".to_string()),
        );
        let decision = rollout_decision_with_config(
            Some("o200k_base"),
            "key",
            RolloutConfig {
                mode: RolloutMode::Enforce,
                canary_basis_points: 10_000,
            },
        );
        let mut result = ToolResult::with_structured(
            "数据库 👩‍💻 escaped \\\"json\\\"",
            json!({"answer": "数据库 👩‍💻", "valid": true}),
        );

        let mut measurement_index = 0usize;
        let measurement = attach_fixed_point_report_with_measure(
            &mut result,
            decision,
            &context,
            "wire_tokens_non_convergent_test",
            EnforcementReport {
                target_tokens: 4_096,
                ..Default::default()
            },
            |_| {
                measurement_index += 1;
                let exact_tokens = 100 + (measurement_index % 2);
                Some(TokenMeasurement {
                    proxy_tokens: exact_tokens,
                    exact_tokens,
                    delta_tokens: 0,
                    exact_to_proxy_ratio: 1.0,
                    count_latency_us: 0,
                })
            },
        )
        .expect("the final report-free wire still has a valid exact measurement");

        assert!(result
            .structured_content
            .as_ref()
            .and_then(|structured| structured.get(WIRE_REPORT_KEY))
            .is_none());
        assert!(!fixed_point_report_is_truthful(
            &result,
            decision,
            4_096,
            measurement
        ));
        assert!(serde_json::from_slice::<Value>(
            &canonical_tool_result_bytes(&result, &context).unwrap()
        )
        .is_ok());
    }

    #[test]
    fn shadow_reporting_is_byte_for_byte_neutral() {
        warm_o200k();
        let context = WireResponseContext::http_jsonrpc(
            Some(json!("shadow-request")),
            Some("Searching ContextStream index".to_string()),
            Some("⌕".to_string()),
        );
        let decision = rollout_decision_with_config(
            Some("o200k_base"),
            "shadow-key",
            RolloutConfig {
                mode: RolloutMode::Shadow,
                canary_basis_points: 10_000,
            },
        );
        let mut result = ToolResult::with_structured(
            "数据库 👩‍💻 escaped \\\"json\\\"",
            json!({"answer": "unchanged", "results": [1, 2, 3]}),
        );
        let expected = canonical_tool_result_bytes(&result, &context).unwrap();

        let measurement = attach_fixed_point_report(
            &mut result,
            decision,
            &context,
            "wire_tokens_shadow_neutral_test",
            EnforcementReport {
                target_tokens: 4_096,
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(
            canonical_tool_result_bytes(&result, &context).unwrap(),
            expected
        );
        assert_eq!(
            measurement.exact_tokens,
            measure_utf8(&expected, "wire_tokens_shadow_expected")
                .unwrap()
                .exact_tokens
        );
        assert!(result
            .structured_content
            .as_ref()
            .is_some_and(|value| value.get(WIRE_REPORT_KEY).is_none()));
    }

    #[test]
    fn warm_counter_size_ladder_is_bounded() {
        warm_o200k();
        let line = "fn route<T: Send + Sync>(value: T) -> Result<T, Error> { Ok(value) }\n";
        let cases = [
            ("4k", line.repeat(58), 2_000_u64, 250_000_u64),
            ("24k", line.repeat(348), 5_000_u64, 500_000_u64),
            ("200k", line.repeat(2_899), 40_000_u64, 2_000_000_u64),
        ];
        for (label, payload, release_limit_us, debug_limit_us) in cases {
            let mut samples = Vec::with_capacity(20);
            for _ in 0..20 {
                samples.push(
                    measure_utf8(payload.as_bytes(), "wire_tokens_perf_test")
                        .unwrap()
                        .count_latency_us,
                );
            }
            samples.sort_unstable();
            let p95 = samples[(samples.len() * 95 / 100).min(samples.len() - 1)];
            let limit = if cfg!(debug_assertions) {
                debug_limit_us
            } else {
                release_limit_us
            };
            eprintln!(
                "wire_tokenizer_perf label={label} profile={} p95_us={p95} limit_us={limit}",
                if cfg!(debug_assertions) {
                    "debug"
                } else {
                    "release"
                }
            );
            assert!(p95 <= limit, "{label} warm count p95 {p95}us > {limit}us");
        }
    }
}
