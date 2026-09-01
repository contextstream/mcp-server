//! HTTP transport for the MCP server.
//!
//! Provides a REST/JSON-RPC gateway with:
//! - JWT authentication
//! - CORS support
//! - SSE for streaming responses

use crate::agentic_telemetry::{
    AgenticTelemetry, AgenticTelemetryInput, ManagedHarnessRuntimeIdentity,
};
use crate::server::{
    build_contextstream_stateless_discover_result, build_legacy_initialize_result, build_registry,
    contextstream_tools_list, validate_contextstream_meta_tool_arguments,
};
use axum::body::Body;
use axum::http::{Method, Request};
use axum::middleware::Next;
use axum::{
    extract::{Json, MatchedPath, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response, Sse},
    routing::{get, post},
    Router,
};
use base64::{engine::general_purpose::STANDARD as BASE64_STANDARD, Engine as _};
use futures::stream::{self, Stream};
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use mcp_client::{
    get_task_config_override, run_with_auth_override, run_with_caller_cache_identity,
    run_with_config_override, run_with_installation_id, run_with_session_key, ContextStreamClient,
};
use mcp_session::SessionManager;
use mcp_tools::ToolRegistry;
use mcp_types::config::{ConfigOverride, OutputFormat, ToolSurfaceProfile, Toolset};
use mcp_types::{
    decorate_stateless_cacheable_result, decorate_stateless_result,
    has_stateless_protocol_metadata, stateless_protocol_version,
    validate_stateless_jsonrpc_envelope, validate_stateless_method_params,
    validate_stateless_request, AuthOverride, Config, HarnessId, McpCacheScope, McpProtocolError,
    SessionKey, StatelessRequestMetadata, TrafficClass, MCP_PROTOCOL_2026_07_28,
    MCP_TOOLS_LIST_TTL_MS,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::{Duration, Instant};
use tower_http::compression::CompressionLayer;
use tower_http::cors::{Any, CorsLayer};
use tower_http::trace::TraceLayer;
use tracing::{debug, info, warn};
use uuid::Uuid;

// ============================================================================
// Types
// ============================================================================

/// HTTP server state.
#[derive(Clone)]
pub struct HttpState {
    pub registry: Arc<ToolRegistry>,
    pub client: ContextStreamClient,
    pub session: Arc<SessionManager>,
    pub jwt_secret: Option<String>,
    pub require_auth: bool,
    pub telemetry: AgenticTelemetry,
    pub tools_list_cache: Arc<RwLock<HashMap<String, Value>>>,
    pub concurrency_semaphore: Arc<tokio::sync::Semaphore>,
    /// Prometheus rendering handle. `Clone`-cheap (Arc inside).
    /// Installed once in `run_http_server`'s caller (`main.rs::run_http_server`)
    /// and rendered by the unauthenticated `/metrics` route. None when
    /// the binary is run via test fixtures that don't install a recorder.
    pub metrics_handle: Option<metrics_exporter_prometheus::PrometheusHandle>,
}

/// JSON-RPC request.
#[derive(Debug, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// JSON-RPC response.
#[derive(Debug, Serialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

/// JSON-RPC error.
#[derive(Debug, Serialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// JWT Claims.
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct Claims {
    sub: String,
    exp: usize,
}

/// Tool execution request.
#[derive(Debug, Deserialize)]
pub struct ToolRequest {
    pub tool: String,
    #[serde(default)]
    pub input: Value,
}

/// Query parameters for list endpoints.
#[derive(Debug, Deserialize)]
pub struct ListParams {
    pub category: Option<String>,
    pub format: Option<String>,
}

/// Query parameters for MCP streamable endpoint.
#[derive(Debug, Default, Deserialize)]
pub struct McpQueryParams {
    pub default_context_mode: Option<String>,
}

const DEFAULT_PUBLIC_MCP_ORIGIN: &str = "https://mcp.contextstream.io";
const DEFAULT_PUBLIC_API_ORIGIN: &str = "https://api.contextstream.io";
const MCP_PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";
const MCP_METHOD_HEADER: &str = "mcp-method";
const MCP_NAME_HEADER: &str = "mcp-name";
const LEGACY_HTTP_PROTOCOL_VERSIONS: &[&str] =
    &["2024-11-05", "2025-03-26", "2025-06-18", "2025-11-25"];
const DEFAULT_ALLOWED_MCP_CLIENT_ORIGINS: &[&str] = &[
    "https://contextstream.io",
    "https://www.contextstream.io",
    "https://chatgpt.com",
    "https://chat.openai.com",
    "https://claude.ai",
    "https://vscode.dev",
    "https://insiders.vscode.dev",
    "https://github.dev",
];

impl From<McpProtocolError> for JsonRpcError {
    fn from(error: McpProtocolError) -> Self {
        Self {
            code: error.code,
            message: error.message,
            data: error.data,
        }
    }
}

fn bounded_managed_version(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'+'))
    {
        return None;
    }
    Some(value.to_string())
}

fn managed_harness_runtime_identity(headers: &HeaderMap) -> Option<ManagedHarnessRuntimeIdentity> {
    let installation_id = headers
        .get("x-contextstream-installation-id")?
        .to_str()
        .ok()
        .and_then(|value| Uuid::parse_str(value.trim()).ok())
        .filter(|value| !value.is_nil())?;
    let harness_id = headers
        .get("x-contextstream-client")?
        .to_str()
        .ok()
        .and_then(HarnessId::from_alias)?;
    let managed_config_version = headers
        .get("x-contextstream-managed-config-version")?
        .to_str()
        .ok()
        .and_then(bounded_managed_version)?;
    let teaching_version = headers
        .get("x-contextstream-teaching-version")?
        .to_str()
        .ok()
        .and_then(bounded_managed_version)?;

    Some(ManagedHarnessRuntimeIdentity {
        installation_id,
        harness_id,
        managed_config_version,
        teaching_version,
    })
}

fn header_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
}

fn plain_routing_header_is_valid(value: &str) -> bool {
    let bytes = value.as_bytes();
    !bytes.is_empty()
        && !matches!(bytes.first(), Some(b' ' | b'\t'))
        && !matches!(bytes.last(), Some(b' ' | b'\t'))
        && bytes.iter().all(|byte| matches!(byte, b'\t' | 0x20..=0x7e))
}

fn protocol_header_uses_stateless_contract(headers: &HeaderMap) -> bool {
    header_value(headers, MCP_PROTOCOL_VERSION_HEADER)
        .is_some_and(|version| !LEGACY_HTTP_PROTOCOL_VERSIONS.contains(&version))
}

fn uses_stateless_http_contract(headers: &HeaderMap, request: &JsonRpcRequest) -> bool {
    request.method == "server/discover"
        || has_stateless_protocol_metadata(&request.params)
        || headers.contains_key(MCP_METHOD_HEADER)
        || headers.contains_key(MCP_NAME_HEADER)
        || protocol_header_uses_stateless_contract(headers)
}

fn decode_mcp_name_header(value: &str) -> Result<String, McpProtocolError> {
    const PREFIX: &str = "=?base64?";
    const SUFFIX: &str = "?=";

    if !(value.starts_with(PREFIX) && value.ends_with(SUFFIX)) {
        if !plain_routing_header_is_valid(value) {
            return Err(McpProtocolError::header_mismatch(
                "Mcp-Name contains an invalid plain header value",
            ));
        }
        return Ok(value.to_string());
    }
    let encoded = value
        .strip_prefix(PREFIX)
        .and_then(|value| value.strip_suffix(SUFFIX))
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            McpProtocolError::header_mismatch("Mcp-Name has an invalid base64 sentinel")
        })?;
    let decoded = BASE64_STANDARD
        .decode(encoded)
        .map_err(|_| McpProtocolError::header_mismatch("Mcp-Name has invalid base64 content"))?;
    String::from_utf8(decoded)
        .map_err(|_| McpProtocolError::header_mismatch("Mcp-Name is not valid UTF-8"))
}

fn request_routing_name(request: &JsonRpcRequest) -> Option<&str> {
    match request.method.as_str() {
        "tools/call" | "prompts/get" => request.params.get("name").and_then(Value::as_str),
        "resources/read" => request
            .params
            .get("name")
            .or_else(|| request.params.get("uri"))
            .and_then(Value::as_str),
        _ => None,
    }
}

fn method_requires_mcp_name(method: &str) -> bool {
    matches!(method, "tools/call" | "resources/read" | "prompts/get")
}

fn validate_stateless_http_request(
    headers: &HeaderMap,
    request: &JsonRpcRequest,
) -> Result<StatelessRequestMetadata, McpProtocolError> {
    let header_version = header_value(headers, MCP_PROTOCOL_VERSION_HEADER).ok_or_else(|| {
        McpProtocolError::header_mismatch("Missing required MCP-Protocol-Version header")
    })?;
    if !plain_routing_header_is_valid(header_version) {
        return Err(McpProtocolError::header_mismatch(
            "MCP-Protocol-Version has an invalid header value",
        ));
    }

    if let Some(body_version) = stateless_protocol_version(&request.params) {
        if body_version != header_version {
            return Err(McpProtocolError::header_mismatch(format!(
                "MCP-Protocol-Version header '{header_version}' does not match request metadata '{body_version}'"
            )));
        }
    }
    if header_version != MCP_PROTOCOL_2026_07_28 {
        return Err(McpProtocolError::unsupported_version(header_version));
    }

    let metadata = validate_stateless_request(&request.params)?;
    if metadata.protocol_version != header_version {
        return Err(McpProtocolError::header_mismatch(format!(
            "MCP-Protocol-Version header '{header_version}' does not match request metadata '{}'",
            metadata.protocol_version
        )));
    }

    let header_method = header_value(headers, MCP_METHOD_HEADER)
        .ok_or_else(|| McpProtocolError::header_mismatch("Missing required Mcp-Method header"))?;
    if !plain_routing_header_is_valid(header_method) {
        return Err(McpProtocolError::header_mismatch(
            "Mcp-Method has an invalid header value",
        ));
    }
    if header_method != request.method {
        return Err(McpProtocolError::header_mismatch(format!(
            "Mcp-Method header '{header_method}' does not match JSON-RPC method '{}'",
            request.method
        )));
    }

    if method_requires_mcp_name(&request.method) {
        let encoded_name = header_value(headers, MCP_NAME_HEADER).ok_or_else(|| {
            McpProtocolError::header_mismatch(format!(
                "Missing required Mcp-Name header for {}",
                request.method
            ))
        })?;
        let header_name = decode_mcp_name_header(encoded_name)?;
        let body_name = request_routing_name(request).ok_or_else(|| {
            McpProtocolError::header_mismatch(format!(
                "{} params do not contain the name routed by Mcp-Name",
                request.method
            ))
        })?;
        if header_name != body_name {
            return Err(McpProtocolError::header_mismatch(format!(
                "Mcp-Name header '{header_name}' does not match request name '{body_name}'"
            )));
        }
    }

    validate_stateless_method_params(&request.method, &request.params)?;

    Ok(metadata)
}

fn canonical_http_origin(value: &str) -> Option<(String, String)> {
    let parsed = reqwest::Url::parse(value.trim()).ok()?;
    if !matches!(parsed.scheme(), "http" | "https")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.path() != "/"
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return None;
    }
    let host = parsed.host_str()?.to_ascii_lowercase();
    Some((parsed.origin().ascii_serialization(), host))
}

fn mcp_origin_is_allowed(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else {
        return true;
    };
    let Ok(origin) = origin.to_str() else {
        return false;
    };
    let Some((origin, origin_host)) = canonical_http_origin(origin) else {
        return false;
    };

    if canonical_http_origin(&request_origin(headers))
        .is_some_and(|(request_origin, _)| request_origin == origin)
    {
        return true;
    }

    if DEFAULT_ALLOWED_MCP_CLIENT_ORIGINS
        .iter()
        .filter_map(|value| canonical_http_origin(value))
        .any(|(allowed, _)| allowed == origin)
    {
        return true;
    }

    if std::env::var("CONTEXTSTREAM_MCP_ALLOWED_ORIGINS")
        .ok()
        .into_iter()
        .flat_map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .filter_map(|value| canonical_http_origin(&value))
        .any(|(allowed, _)| allowed == origin)
    {
        return true;
    }

    let request_host = header_value(headers, "x-forwarded-host")
        .or_else(|| header_value(headers, "host"))
        .map(host_without_port);
    request_host.is_some_and(host_is_local_or_private) && host_is_local_or_private(&origin_host)
}

fn stateless_protocol_error_response(
    status: StatusCode,
    id: Option<Value>,
    error: impl Into<JsonRpcError>,
) -> Response {
    (
        status,
        Json(JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(error.into()),
        }),
    )
        .into_response()
}

fn is_streamable_mcp_path(path: &str) -> bool {
    matches!(path, "/mcp" | "/chatgpt" | "/claude" | "/client")
}

/// Read current process RSS in kilobytes from /proc/self/status (Linux only).
#[cfg(target_os = "linux")]
fn read_rss_kb() -> Option<u64> {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines().find(|l| l.starts_with("VmRSS:")).and_then(|l| {
                l.split_whitespace()
                    .nth(1)
                    .and_then(|v| v.parse::<u64>().ok())
            })
        })
}

#[cfg(not(target_os = "linux"))]
fn read_rss_kb() -> Option<u64> {
    None
}

// ============================================================================
// Server
// ============================================================================

/// Create the HTTP router.
pub fn create_router(state: HttpState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let api_routes = Router::new()
        // JSON-RPC endpoint
        .route("/rpc", post(handle_jsonrpc))
        // MCP protocol endpoints
        .route("/initialize", post(handle_initialize))
        .route("/tools/list", get(handle_list_tools))
        .route("/tools/call", post(handle_call_tool))
        .route("/tools/:name", post(handle_call_tool_by_name))
        // Health check
        .route("/health", get(handle_health))
        // SSE endpoint for streaming
        .route("/stream", get(handle_sse));

    Router::new()
        .nest("/api/v1", api_routes)
        .route("/health", get(handle_health))
        // Prometheus scrape endpoint — exposes every metrics::counter! /
        // histogram! emission from the dep tree. See `handle_metrics`
        // and `auth_middleware` for the unauthenticated-path allowlist.
        .route("/metrics", get(handle_metrics))
        .route(
            "/.well-known/oauth-protected-resource",
            get(handle_oauth_protected_resource),
        )
        // RFC 8414 + OIDC discovery. Some OAuth clients (notably
        // claude.ai's MCP custom-connector flow) probe the *resource*
        // host directly for the authorization-server metadata as a
        // pre-flight validation, before they follow the
        // resource→authorization_servers hop. Without these routes
        // they get a 401 from the auth middleware and bail with
        // `mcp_client_invalid`.
        //
        // The body we return mirrors what the actual authorization
        // server (api.contextstream.io) publishes, so clients that
        // skip the hop still get a usable answer.
        .route(
            "/.well-known/oauth-authorization-server",
            get(handle_oauth_authorization_server),
        )
        .route(
            "/.well-known/openid-configuration",
            get(handle_oauth_authorization_server),
        )
        // MCP Streamable HTTP endpoint (compatible with TS gateway /mcp path)
        .route("/mcp", post(handle_mcp_streamable))
        // Per-client MCP endpoints — same handler, distinct paths so each
        // integration (ChatGPT Apps SDK, Claude.ai Custom Connectors, generic
        // clients) can be registered/scoped/branded independently.
        .route("/chatgpt", post(handle_mcp_streamable))
        .route("/claude", post(handle_mcp_streamable))
        .route("/client", post(handle_mcp_streamable))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ))
        .layer(cors)
        .layer(TraceLayer::new_for_http())
        .layer(CompressionLayer::new())
        .with_state(state)
}

/// Run the HTTP server.
pub async fn run_http_server(
    registry: Arc<ToolRegistry>,
    client: ContextStreamClient,
    session: Arc<SessionManager>,
    host: &str,
    port: u16,
    jwt_secret: Option<String>,
    require_auth: bool,
    metrics_handle: metrics_exporter_prometheus::PrometheusHandle,
) -> anyhow::Result<()> {
    // Warm before router construction/listener binding so no request can pay
    // vocabulary initialization latency.
    mcp_tools::wire_tokens::warm_o200k();

    let state = HttpState {
        registry,
        client: client.clone(),
        session: session.clone(),
        jwt_secret,
        require_auth,
        telemetry: AgenticTelemetry::new(client, session),
        tools_list_cache: Arc::new(RwLock::new(HashMap::new())),
        concurrency_semaphore: Arc::new(tokio::sync::Semaphore::new(
            std::env::var("MCP_MAX_CONCURRENT_REQUESTS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8),
        )),
        metrics_handle: Some(metrics_handle),
    };

    let app = create_router(state);
    let addr = format!("{}:{}", host, port);

    info!("Starting HTTP server on {}", addr);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

// ============================================================================
// Handlers
// ============================================================================

/// Health check handler.
async fn handle_health() -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "ok",
        "server": "contextstream-mcp",
        "version": env!("CARGO_PKG_VERSION")
    }))
}

/// Prometheus metrics scrape endpoint. Returns the text-format
/// exposition of every counter/histogram emitted by `metrics::` macros
/// across the binary's dep tree. Unauthenticated by design — Prometheus
/// scrapers don't carry our JWT, and the surface only exposes operator-
/// level counters (no per-tenant data).
///
/// 503 when the recorder wasn't installed (test harness only — main.rs
/// always installs it).
async fn handle_metrics(State(state): State<HttpState>) -> impl IntoResponse {
    match &state.metrics_handle {
        Some(handle) => (
            StatusCode::OK,
            [(
                header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            )],
            handle.render(),
        )
            .into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            "metrics recorder not installed",
        )
            .into_response(),
    }
}

/// OAuth 2.0 protected resource metadata for hosted MCP clients.
async fn handle_oauth_protected_resource(headers: HeaderMap) -> impl IntoResponse {
    Json(oauth_protected_resource_metadata(&headers))
}

/// RFC 8414 / OIDC discovery served on the MCP host (mirrors what
/// the actual authorization server publishes). Lets clients that
/// probe the resource host directly find the right endpoints without
/// the auth-middleware 401 that broke claude.ai's custom connector
/// flow.
async fn handle_oauth_authorization_server() -> impl IntoResponse {
    let auth_server = authorization_server_origin();
    let public_url = std::env::var("CONTEXTSTREAM_PUBLIC_WEB_ORIGIN")
        .unwrap_or_else(|_| "https://contextstream.io".to_string());
    let public_url = public_url.trim_end_matches('/').to_string();
    Json(json!({
        "issuer": auth_server,
        // The authorize endpoint is on the web UI (contextstream.io)
        // by design — the auth-server origin (api.contextstream.io)
        // doesn't render the consent UI itself.
        "authorization_endpoint": format!("{}/oauth/authorize", public_url),
        "token_endpoint": format!("{}/api/v1/oauth/token", auth_server),
        "registration_endpoint": format!("{}/api/v1/oauth/register", auth_server),
        "revocation_endpoint": format!("{}/api/v1/oauth/revoke", auth_server),
        "token_endpoint_auth_methods_supported": [
            "client_secret_basic",
            "client_secret_post",
            "none"
        ],
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "code_challenge_methods_supported": ["S256", "plain"],
        "scopes_supported": [
            "read:workspaces",
            "read:projects",
            "read:search",
            "read:memory",
            "write:memory"
        ]
    }))
}

/// Handle JSON-RPC requests.
async fn handle_jsonrpc(
    State(state): State<HttpState>,
    Json(request): Json<JsonRpcRequest>,
) -> impl IntoResponse {
    debug!("JSON-RPC request: method={}", request.method);
    let wire_observation = mcp_tools::wire_tokens::WireTokenObservation::default();

    let result = match request.method.as_str() {
        "initialize" => {
            // Deterministic per-`initialize`: apply the detected profile, or
            // fall back to the registry default. Never leave a previous
            // client's auto-detected narrowing in place (global-state bleed).
            state.registry.apply_initialize_surface_profile(
                surface_profile_from_initialize_params(&request.params),
            );
            state
                .telemetry
                .update_initialize_hints(&request.params)
                .await;
            handle_initialize_method(&state, &request.params).await
        }
        "tools/list" => handle_list_tools_method(&state, None).await,
        "tools/call" => {
            handle_call_tool_method(
                &state,
                request.params,
                request.id.clone(),
                wire_observation.clone(),
                false,
                None,
            )
            .await
        }
        method if method.starts_with("notifications/") => {
            // Handle notifications (no response needed)
            return Json(JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: request.id,
                result: Some(serde_json::json!({})),
                error: None,
            });
        }
        _ => Err(JsonRpcError {
            code: -32601,
            message: format!("Method not found: {}", request.method),
            data: None,
        }),
    };

    let response = match result {
        Ok(value) => JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: request.id,
            result: Some(value),
            error: None,
        },
        Err(err) => JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: request.id,
            result: None,
            error: Some(err),
        },
    };

    if let Ok(bytes) = serde_json::to_vec(&response) {
        mcp_tools::wire_tokens::observe_final_wire_bytes(
            &bytes,
            &wire_observation,
            "mcp_http_jsonrpc_final_response",
        );
    }

    Json(response)
}

/// Handle MCP Streamable HTTP requests at /mcp.
///
/// Implements the Streamable HTTP transport per the MCP spec:
/// - POST with JSON-RPC body
/// - Requires `Accept: application/json, text/event-stream`
/// - Supports both initialize-era clients and the stateless 2026-07-28 contract
/// - Returns `Mcp-Session-Id` only for legacy initialize
/// - Notifications (no `id`) get 202 Accepted
async fn handle_mcp_streamable(
    State(state): State<HttpState>,
    Query(query): Query<McpQueryParams>,
    matched_path: Option<MatchedPath>,
    headers: HeaderMap,
    Json(mut request): Json<JsonRpcRequest>,
) -> Response {
    // Integration-specific endpoints (/chatgpt, /claude, /client) must expose
    // the full ContextStream tool surface regardless of client auto-detection,
    // so the app/plugin has complete parity with the local MCP tool.
    let force_full_surface = matched_path
        .as_ref()
        .map(|p| {
            let path = p.as_str();
            path == "/chatgpt" || path == "/claude" || path == "/client"
        })
        .unwrap_or(false);

    // MCP requires Origin validation to prevent DNS-rebinding attacks. Native
    // clients generally omit Origin; browser clients must be same-origin,
    // explicitly configured, or one of the hosted client origins we support.
    if !mcp_origin_is_allowed(&headers) {
        return stateless_protocol_error_response(
            StatusCode::FORBIDDEN,
            request.id.clone(),
            JsonRpcError {
                code: -32000,
                message: "Forbidden: Origin is not allowed".to_string(),
                data: None,
            },
        );
    }

    // Validate Accept header per MCP spec
    let accept = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !accept.contains("application/json") || !accept.contains("text/event-stream") {
        let err = JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: None,
            result: None,
            error: Some(JsonRpcError {
                code: -32000,
                message:
                    "Not Acceptable: Client must accept both application/json and text/event-stream"
                        .to_string(),
                data: None,
            }),
        };
        return (StatusCode::NOT_ACCEPTABLE, Json(err)).into_response();
    }

    apply_default_context_mode(&mut request, query.default_context_mode.as_deref());
    let stateless_request = uses_stateless_http_contract(&headers, &request);
    if stateless_request {
        if let Err(error) = validate_stateless_jsonrpc_envelope(
            &request.jsonrpc,
            request.id.as_ref(),
            &request.method,
        ) {
            return stateless_protocol_error_response(
                StatusCode::BAD_REQUEST,
                request.id.clone(),
                error,
            );
        }
        if let Err(error) = validate_stateless_http_request(&headers, &request) {
            return stateless_protocol_error_response(
                StatusCode::BAD_REQUEST,
                request.id.clone(),
                error,
            );
        }
    }

    // Handle notifications (requests without an id) — return 202
    if request.id.is_none() {
        return StatusCode::ACCEPTED.into_response();
    }

    // Acquire concurrency permit; reject with 503 if all slots are busy
    let _permit = match state.concurrency_semaphore.try_acquire() {
        Ok(permit) => permit,
        Err(_) => {
            warn!(
                "Concurrency limit reached ({} permits), rejecting request",
                state.concurrency_semaphore.available_permits()
            );
            let err = JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: request.id,
                result: None,
                error: Some(JsonRpcError {
                    code: -32000,
                    message: "Server busy: too many concurrent requests".to_string(),
                    data: None,
                }),
            };
            return (StatusCode::SERVICE_UNAVAILABLE, Json(err)).into_response();
        }
    };

    let rss_before = read_rss_kb();

    let is_initialize = request.method == "initialize";
    let response_session_id =
        (is_initialize && !stateless_request).then(|| Uuid::new_v4().to_string());

    let tool_name = if request.method == "tools/call" {
        request
            .params
            .get("name")
            .and_then(|n| n.as_str())
            .map(|s| s.to_string())
    } else {
        None
    };

    // Atlas Stream Processing wiring — capture the call's start so a
    // post-dispatch tool_call event can carry duration_ms. The actual
    // emit fires fire-and-forget after dispatch returns (see below).
    let tool_call_started = std::time::Instant::now();
    let workspace_id_for_stream = headers
        .get("x-contextstream-workspace-id")
        .or_else(|| headers.get("x-workspace-id"))
        .and_then(|h| h.to_str().ok())
        .and_then(|s| Uuid::parse_str(s).ok());
    let project_id_for_stream = headers
        .get("x-contextstream-project-id")
        .or_else(|| headers.get("x-project-id"))
        .and_then(|h| h.to_str().ok())
        .and_then(|s| Uuid::parse_str(s).ok());
    let managed_runtime_identity = managed_harness_runtime_identity(&headers);

    // Dispatch through the same JSON-RPC handler logic. The observation slot
    // is shared with the tool's task-local wire context and consumed exactly
    // once after the final response is serialized below.
    let wire_observation = mcp_tools::wire_tokens::WireTokenObservation::default();
    let request_registry =
        if stateless_request && matches!(request.method.as_str(), "tools/list" | "tools/call") {
            Some(stateless_request_registry(&state, force_full_surface).await)
        } else {
            None
        };
    if !stateless_request && force_full_surface {
        // Integration-specific legacy paths likewise select their complete
        // surface on every request rather than inheriting global profile state.
        state
            .registry
            .set_tool_surface_profile(ToolSurfaceProfile::Default);
    }
    let result = match request.method.as_str() {
        "server/discover" => Ok(build_contextstream_stateless_discover_result()),
        "initialize" if stateless_request => Err(JsonRpcError {
            code: -32601,
            message: format!(
                "Method not found: initialize is not part of MCP {MCP_PROTOCOL_2026_07_28}"
            ),
            data: None,
        }),
        "initialize" => {
            if !force_full_surface {
                // Deterministic per-`initialize`: apply the detected profile,
                // or fall back to the registry default so a prior client's
                // auto-detected narrowing can't persist (global-state bleed).
                state.registry.apply_initialize_surface_profile(
                    surface_profile_from_initialize_params(&request.params),
                );
            }
            state
                .telemetry
                .update_managed_initialize_hints_for_session(
                    &request.params,
                    response_session_id.as_deref(),
                    managed_runtime_identity,
                )
                .await;
            let result = handle_initialize_method(&state, &request.params).await;
            if result.is_ok() {
                state
                    .telemetry
                    .emit_managed_connected_readiness(
                        response_session_id.as_deref(),
                        workspace_id_for_stream,
                        project_id_for_stream,
                    )
                    .await;
            }
            result
        }
        "tools/list" => handle_list_tools_method(&state, request_registry.clone()).await,
        "tools/call" => {
            handle_call_tool_method(
                &state,
                request.params,
                request.id.clone(),
                wire_observation.clone(),
                stateless_request,
                request_registry,
            )
            .await
        }
        "ping" if !stateless_request => Ok(serde_json::json!({})),
        _ => Err(JsonRpcError {
            code: -32601,
            message: format!("Method not found: {}", request.method),
            data: None,
        }),
    };

    // Acceleration signal emit — fire-and-forget so the tool's response
    // latency stays untouched. These events are non-canonical telemetry
    // and prewarm hints only; no tool correctness depends on delivery.
    // Atlas stream remains a temporary compatibility fallback when no
    // acceleration signal provider is configured.
    if let (Some(name), Some(ws)) = (tool_name.as_deref(), workspace_id_for_stream) {
        let outcome = if result.is_ok() { "ok" } else { "error" };
        let elapsed_ms = tool_call_started.elapsed().as_millis() as u64;
        let payload = serde_json::json!({
            "tool_name": name,
            "outcome": outcome,
            "duration_ms": elapsed_ms,
        });
        if let Some(signals) = state.registry.acceleration_layer().signals() {
            let mut event = mcp_types::acceleration_layer::AccelerationSignalEvent::with_scope(
                mcp_types::acceleration_layer::AccelerationSignalKind::ToolCall,
                ws,
                project_id_for_stream,
                payload,
            );
            event.tool = Some(name.to_string());
            event.action = Some("tools/call".to_string());
            event.provider = Some("mcp_http".to_string());
            event.latency_ms = Some(elapsed_ms);
            event.degraded = Some(result.is_err());
            tokio::spawn(async move {
                if let Err(e) = signals.emit(event).await {
                    debug!(
                        error = %e,
                        "acceleration-signal: tool_call emit failed (best-effort; ignored)"
                    );
                }
            });
        } else if state.registry.acceleration_layer().is_enabled() {
            metrics::counter!(
                "acceleration_signal_disabled_total",
                "source" => "http_transport",
                "signal_type" => "tool_call",
            )
            .increment(1);
        } else if let Some(stream) = state.registry.atlas_layer().stream() {
            let project = project_id_for_stream;
            let kind = mcp_types::atlas_layer::AtlasStreamEventKind::ToolCall;
            metrics::counter!(
                "acceleration_signal_atlas_fallback_total",
                "source" => "http_transport",
                "signal_type" => "tool_call",
            )
            .increment(1);
            tokio::spawn(async move {
                if let Err(e) = stream.emit_payload(kind, ws, project, payload).await {
                    debug!(
                        kind = kind.as_str(),
                        error = %e,
                        "atlas-stream: tool_call emit failed (best-effort; ignored)"
                    );
                }
            });
        }
    }

    if let Some(before) = rss_before {
        if let Some(after) = read_rss_kb() {
            let delta_mb = (after as i64 - before as i64) as f64 / 1024.0;
            if delta_mb > 10.0 {
                warn!(
                    "Memory spike: RSS grew {:.1} MiB during {} (tool={}) [before={}KB after={}KB]",
                    delta_mb,
                    if is_initialize {
                        "initialize"
                    } else {
                        "tools/call"
                    },
                    tool_name.as_deref().unwrap_or("-"),
                    before,
                    after,
                );
            }
        }
    }

    let http_status =
        if stateless_request && result.as_ref().is_err_and(|error| error.code == -32601) {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::OK
        };
    let response = match result {
        Ok(value) => {
            let value = if stateless_request {
                match request.method.as_str() {
                    "tools/list" => decorate_stateless_cacheable_result(
                        value,
                        MCP_TOOLS_LIST_TTL_MS,
                        McpCacheScope::Private,
                    ),
                    _ => decorate_stateless_result(value),
                }
            } else {
                value
            };
            JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: request.id,
                result: Some(value),
                error: None,
            }
        }
        Err(err) => JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: request.id,
            result: None,
            error: Some(err),
        },
    };

    if let Ok(bytes) = serde_json::to_vec(&response) {
        mcp_tools::wire_tokens::observe_final_wire_bytes(
            &bytes,
            &wire_observation,
            "mcp_http_final_response",
        );
    }
    let mut http_response = (http_status, Json(response)).into_response();

    // Add Mcp-Session-Id on initialize (clients use this for subsequent requests)
    if let Some(session_id) = response_session_id {
        if let Ok(val) = session_id.parse() {
            http_response.headers_mut().insert("mcp-session-id", val);
        }
    }

    http_response
}

fn surface_profile_from_initialize_params(params: &Value) -> Option<ToolSurfaceProfile> {
    // Explicit opt-in always wins. Clients/integrations that genuinely want
    // the compact adaptive OpenAI surface either pass `tool_surface_profile`
    // in the initialize params (the agentic evals) or send the
    // `X-ContextStream-Tool-Surface-Profile` header (Copilot), which is
    // honored per-request via the config-override path in `effective_registry`.
    let explicit = params
        .get("tool_surface_profile")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<ToolSurfaceProfile>().ok());
    if explicit.is_some() {
        return explicit;
    }

    // Otherwise infer the surface ONLY from the client/provider *identity* —
    // never from the model name. A client's model (e.g. `gpt-5`, `gpt-5.5`,
    // `gpt-5-codex`) says nothing about whether its MCP transport can drive
    // the full tool surface: Codex/Fugu run gpt-5* models but are ordinary
    // full-surface clients. Previously matching the `model` field against
    // `"gpt-5"` silently narrowed Codex to the 13-tool adaptive surface,
    // hiding `search`, `memory`, `project`, etc. behind discovery meta-tools
    // so direct calls failed with "unsupported call: <tool>".
    let combined = [
        params.get("client_name").and_then(|v| v.as_str()),
        params.get("provider").and_then(|v| v.as_str()),
        params
            .get("clientInfo")
            .and_then(|v| v.get("name"))
            .and_then(|v| v.as_str()),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ")
    .to_ascii_lowercase();

    if combined.contains("chatgpt") || combined.contains("openai") || combined.contains("responses")
    {
        Some(ToolSurfaceProfile::OpenaiAgentic)
    } else {
        None
    }
}

fn apply_default_context_mode(request: &mut JsonRpcRequest, default_context_mode: Option<&str>) {
    let Some(mode) = default_context_mode
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return;
    };

    if request.method != "tools/call" {
        return;
    }

    let Some(params) = request.params.as_object_mut() else {
        return;
    };

    if params.get("name").and_then(|value| value.as_str()) != Some("context") {
        return;
    }

    if !params
        .get("arguments")
        .map(Value::is_object)
        .unwrap_or(false)
    {
        params.insert("arguments".to_string(), json!({}));
    }

    let Some(arguments) = params.get_mut("arguments").and_then(Value::as_object_mut) else {
        return;
    };

    arguments
        .entry("mode".to_string())
        .or_insert_with(|| json!(mode));
}

/// Handle initialize request.
async fn handle_initialize(State(state): State<HttpState>) -> impl IntoResponse {
    match handle_initialize_method(&state, &serde_json::json!({})).await {
        Ok(value) => (StatusCode::OK, Json(value)),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": err.message })),
        ),
    }
}

/// Handle list tools request.
async fn handle_list_tools(
    State(state): State<HttpState>,
    Query(params): Query<ListParams>,
) -> impl IntoResponse {
    let registry = effective_registry(&state).await;
    let tools = contextstream_tools_list(&registry, params.category.as_deref());

    Json(serde_json::json!({ "tools": tools }))
}

/// Handle tool call request.
async fn handle_call_tool(
    State(state): State<HttpState>,
    Json(request): Json<ToolRequest>,
) -> impl IntoResponse {
    execute_tool(&state, &request.tool, request.input).await
}

/// Handle tool call by name.
async fn handle_call_tool_by_name(
    State(state): State<HttpState>,
    Path(name): Path<String>,
    Json(input): Json<Value>,
) -> impl IntoResponse {
    execute_tool(&state, &name, input).await
}

fn observed_rest_tool_response(
    status: StatusCode,
    payload: Value,
    observation: &mcp_tools::wire_tokens::WireTokenObservation,
) -> Response {
    if let Ok(bytes) = serde_json::to_vec(&payload) {
        mcp_tools::wire_tokens::observe_final_wire_bytes(
            &bytes,
            observation,
            "mcp_http_rest_final_response",
        );
    }
    (status, Json(payload)).into_response()
}

/// Execute a tool and return the response.
async fn execute_tool(state: &HttpState, name: &str, input: Value) -> Response {
    let registry = effective_registry(state).await;
    let wire_observation = mcp_tools::wire_tokens::WireTokenObservation::default();

    if registry.is_router_mode() {
        match name {
            "operations" => {
                return (
                    StatusCode::OK,
                    Json(router_operations_response(registry.as_ref(), &input)),
                )
                    .into_response();
            }
            "execute_operation" => {
                let context = mcp_tools::wire_tokens::WireResponseContext::http_rest(None, None)
                    .with_observation(wire_observation.clone());
                let payload = mcp_tools::wire_tokens::run_with_wire_response_context(
                    context,
                    router_execute_response(
                        registry.as_ref(),
                        &input,
                        &state.telemetry,
                        registry.tool_surface_profile(),
                    ),
                )
                .await;
                return observed_rest_tool_response(StatusCode::OK, payload, &wire_observation);
            }
            "batch_operations" => {
                return (
                    StatusCode::OK,
                    Json(
                        batch_operations_response(
                            registry.as_ref(),
                            &input,
                            &state.telemetry,
                            registry.tool_surface_profile(),
                        )
                        .await,
                    ),
                )
                    .into_response();
            }
            _ => {}
        }
    }

    if registry.is_openai_agentic_surface() {
        match name {
            "tool_search" => {
                return (
                    StatusCode::OK,
                    Json(
                        tool_search_response(
                            registry.as_ref(),
                            &input,
                            &state.telemetry,
                            registry.tool_surface_profile(),
                        )
                        .await,
                    ),
                )
                    .into_response();
            }
            "execute_operation" => {
                let context = mcp_tools::wire_tokens::WireResponseContext::http_rest(None, None)
                    .with_observation(wire_observation.clone());
                let payload = mcp_tools::wire_tokens::run_with_wire_response_context(
                    context,
                    router_execute_response(
                        registry.as_ref(),
                        &input,
                        &state.telemetry,
                        registry.tool_surface_profile(),
                    ),
                )
                .await;
                return observed_rest_tool_response(StatusCode::OK, payload, &wire_observation);
            }
            "batch_operations" => {
                return (
                    StatusCode::OK,
                    Json(
                        batch_operations_response(
                            registry.as_ref(),
                            &input,
                            &state.telemetry,
                            registry.tool_surface_profile(),
                        )
                        .await,
                    ),
                )
                    .into_response();
            }
            _ => {}
        }

        if registry.get(name).is_none() && registry.get_operation(name).is_some() {
            state
                .telemetry
                .emit_hidden_direct_call_blocked(
                    registry.tool_surface_profile(),
                    name,
                    AgenticTelemetryInput::from_arguments(&input),
                )
                .await;
            return (
                StatusCode::BAD_REQUEST,
                Json(tool_error_response(format!(
                    "[ERROR] '{}' is hidden on the adaptive surface. Call tool_search(query=\"...\") first, then execute_operation(name=\"{}\", arguments={{...}}).",
                    name, name
                ))),
            )
                .into_response();
        }
    }

    let call_title = registry.get(name).map(|tool| {
        crate::server::contextstream_call_title(
            &tool.metadata.name,
            &tool.metadata.title,
            tool.metadata.annotations.read_only,
            &input,
        )
    });
    let call_icon = crate::server::contextstream_call_icon(name);

    let wire_context = mcp_tools::wire_tokens::WireResponseContext::http_rest(
        call_title.clone(),
        Some(call_icon.to_string()),
    )
    .with_observation(wire_observation.clone());
    let outcome = mcp_tools::wire_tokens::run_with_wire_response_context(
        wire_context,
        registry.execute(name, input),
    )
    .await;

    match outcome {
        Ok(result) => {
            let payload = rest_tool_result_response_with_title(
                result,
                call_title.as_deref(),
                Some(call_icon),
            );
            observed_rest_tool_response(StatusCode::OK, payload, &wire_observation)
        }
        Err(err) => {
            if err.is_non_blocking_parser_error() {
                debug!(
                    error = %err,
                    "suppressed non-blocking ParserError in HTTP tool response"
                );
            }
            let payload = tool_error_response_with_title(
                format!("Error: {}", err.user_facing_message()),
                call_title.as_deref(),
                Some(call_icon),
            );
            observed_rest_tool_response(StatusCode::BAD_REQUEST, payload, &wire_observation)
        }
    }
}

/// SSE handler for streaming responses.
async fn handle_sse(
    State(_state): State<HttpState>,
) -> Sse<impl Stream<Item = Result<axum::response::sse::Event, Infallible>>> {
    let stream = stream::unfold(0, |count| async move {
        if count >= 10 {
            return None;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        let event = axum::response::sse::Event::default()
            .data(format!("heartbeat {}", count))
            .event("ping");
        Some((Ok(event), count + 1))
    });

    Sse::new(stream).keep_alive(
        axum::response::sse::KeepAlive::new()
            .interval(Duration::from_secs(15))
            .text("keep-alive"),
    )
}

// ============================================================================
// Internal Methods
// ============================================================================

async fn effective_config(state: &HttpState) -> Config {
    state.client.config().await
}

fn toolset_cache_label(toolset: Toolset) -> &'static str {
    match toolset {
        Toolset::Light => "light",
        Toolset::Standard => "standard",
        Toolset::Complete => "complete",
    }
}

async fn tools_list_cache_key(state: &HttpState, registry: &ToolRegistry) -> String {
    let mut config = effective_config(state).await;
    config.tool_surface_profile = registry.tool_surface_profile();

    format!(
        "toolset={}|progressive={}|router={}|consolidated={}|surface={}",
        toolset_cache_label(config.toolset),
        config.progressive_mode,
        config.router_mode,
        config.consolidated_mode,
        config.tool_surface_profile.as_str()
    )
}

async fn effective_registry(state: &HttpState) -> Arc<ToolRegistry> {
    let Some(override_config) = get_task_config_override() else {
        return state.registry.clone();
    };

    if !override_config.affects_tool_registry() {
        return state.registry.clone();
    }

    let config = effective_config(state).await;
    Arc::new(build_registry(
        &config,
        state.client.clone(),
        state.session.clone(),
    ))
}

/// Build the tool surface from request-local configuration for the stateless
/// transport. This prevents initialize-era clients from racing modern
/// requests through the shared registry's mutable compatibility profile.
async fn stateless_request_registry(
    state: &HttpState,
    force_full_surface: bool,
) -> Arc<ToolRegistry> {
    let mut config = effective_config(state).await;
    if force_full_surface {
        config.tool_surface_profile = ToolSurfaceProfile::Default;
    }
    Arc::new(build_registry(
        &config,
        state.client.clone(),
        state.session.clone(),
    ))
}

fn tool_result_response(result: mcp_types::tool::ToolResult) -> Value {
    let context = mcp_tools::wire_tokens::current_wire_response_context();
    mcp_tools::wire_tokens::tool_result_payload(&result, &context)
}

fn tool_result_response_with_title(
    result: mcp_types::tool::ToolResult,
    call_title: Option<&str>,
    icon: Option<&str>,
) -> Value {
    let context = mcp_tools::wire_tokens::WireResponseContext::http_jsonrpc(
        None,
        call_title.map(str::to_string),
        icon.map(str::to_string),
    );
    mcp_tools::wire_tokens::tool_result_payload(&result, &context)
}

fn rest_tool_result_response_with_title(
    result: mcp_types::tool::ToolResult,
    call_title: Option<&str>,
    icon: Option<&str>,
) -> Value {
    let context = mcp_tools::wire_tokens::WireResponseContext::http_rest(
        call_title.map(str::to_string),
        icon.map(str::to_string),
    );
    mcp_tools::wire_tokens::tool_result_payload(&result, &context)
}

fn tool_error_response(message: String) -> Value {
    tool_error_response_with_title(message, None, None)
}

fn tool_error_response_with_title(
    message: String,
    call_title: Option<&str>,
    icon: Option<&str>,
) -> Value {
    let mut response = serde_json::json!({
        "content": [{
            "type": "text",
            "text": message
        }],
        "isError": true
    });
    inject_call_title(&mut response, call_title, icon);
    response
}

fn inject_call_title(response: &mut Value, call_title: Option<&str>, icon: Option<&str>) {
    let Some(title) = call_title else { return };
    if let Some(obj) = response.as_object_mut() {
        obj.insert("title".to_string(), Value::String(title.to_string()));
        let mut meta = serde_json::Map::new();
        meta.insert("title".to_string(), Value::String(title.to_string()));
        if let Some(icon) = icon {
            meta.insert("icon".to_string(), Value::String(icon.to_string()));
        }
        obj.insert(
            "_meta".to_string(),
            serde_json::json!({ "contextstream": Value::Object(meta) }),
        );
    }
}

fn router_operations_response(registry: &ToolRegistry, arguments: &Value) -> Value {
    let category = arguments.get("category").and_then(|c| c.as_str());
    let format = arguments
        .get("format")
        .and_then(|f| f.as_str())
        .unwrap_or("grouped");

    let operations = registry.list_operations();
    let filtered: Vec<_> = operations
        .iter()
        .filter(|op| {
            if let Some(cat) = category {
                op.metadata.category.as_str().to_lowercase() == cat.to_lowercase()
            } else {
                true
            }
        })
        .collect();

    let payload = match format {
        "minimal" => {
            let names: Vec<&str> = filtered
                .iter()
                .map(|op| op.metadata.name.as_str())
                .collect();
            serde_json::json!({
                "operations": names,
                "count": names.len()
            })
        }
        "full" => {
            let ops: Vec<Value> = filtered
                .iter()
                .map(|op| {
                    serde_json::json!({
                        "name": op.metadata.name,
                        "description": op.metadata.description,
                        "category": op.metadata.category.as_str(),
                        "inputSchema": op.input_schema
                    })
                })
                .collect();
            serde_json::json!({
                "operations": ops,
                "count": ops.len()
            })
        }
        _ => {
            let mut groups: std::collections::HashMap<String, Vec<Value>> =
                std::collections::HashMap::new();

            for op in &filtered {
                let cat = op.metadata.category.as_str().to_string();
                groups.entry(cat).or_default().push(serde_json::json!({
                    "name": op.metadata.name,
                    "description": op.metadata.description
                }));
            }

            serde_json::json!({
                "operations": groups,
                "count": filtered.len()
            })
        }
    };

    let text = serde_json::to_string_pretty(&payload).unwrap_or_default();
    let mut response = serde_json::json!({
        "content": [{
            "type": "text",
            "text": text
        }],
        "isError": false
    });

    if mcp_types::tool::structured_content_enabled() {
        if let Some(obj) = response.as_object_mut() {
            obj.insert("structured".to_string(), payload);
        }
    }

    response
}

async fn tool_search_response(
    registry: &ToolRegistry,
    arguments: &Value,
    telemetry: &AgenticTelemetry,
    surface_profile: ToolSurfaceProfile,
) -> Value {
    let started = Instant::now();
    let telemetry_input = AgenticTelemetryInput::from_arguments(arguments);
    let query = match arguments.get("query").and_then(|q| q.as_str()) {
        Some(query) if !query.trim().is_empty() => query.trim(),
        _ => {
            return tool_error_response("[ERROR] Missing 'query' parameter".to_string());
        }
    };

    let category = arguments.get("category").and_then(|c| c.as_str());
    let limit = arguments
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(8);

    let matches = registry.search_catalog(query, category, limit);
    telemetry
        .emit_tool_search(
            surface_profile,
            query,
            matches.len(),
            started.elapsed(),
            telemetry_input,
        )
        .await;
    tool_result_response(mcp_types::tool::ToolResult::with_structured(
        format!(
            "Found {} matching tools/operations for '{}'.",
            matches.len(),
            query
        ),
        serde_json::json!({
            "query": query,
            "count": matches.len(),
            "matches": matches
        }),
    ))
}

async fn batch_operations_response(
    registry: &ToolRegistry,
    arguments: &Value,
    telemetry: &AgenticTelemetry,
    surface_profile: ToolSurfaceProfile,
) -> Value {
    let started = Instant::now();
    let telemetry_input = AgenticTelemetryInput::from_arguments(arguments);
    let operations = match arguments.get("operations").and_then(|ops| ops.as_array()) {
        Some(ops) if !ops.is_empty() => ops,
        _ => return tool_error_response("[ERROR] Missing or empty 'operations' array".to_string()),
    };

    if let Some(name) = crate::server::batch_operation_requiring_direct_wire_accounting(arguments) {
        let metric_operation = match name {
            "context" => "context",
            _ => "search",
        };
        metrics::counter!(
            "mcp_wire_tokenizer_batch_rejected_total",
            "transport" => "http",
            "operation" => metric_operation,
        )
        .increment(1);
        return tool_error_response(crate::server::batch_wire_accounting_rejection_message(name));
    }

    let mut results = Vec::with_capacity(operations.len());
    let mut operation_names = Vec::with_capacity(operations.len());
    for op in operations {
        let Some(name) = op.get("name").and_then(|v| v.as_str()) else {
            return tool_error_response("[ERROR] Each operation requires a 'name'".to_string());
        };
        operation_names.push(name.to_string());
        let args = op
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));

        if let Some(tool) = registry.get(name) {
            if !tool.metadata.annotations.read_only || tool.metadata.annotations.destructive {
                return tool_error_response(format!(
                    "[ERROR] '{}' is not eligible for batch_operations because it is not read-only.",
                    name
                ));
            }
            match tool.handler.execute(args).await {
                Ok(result) => results.push(serde_json::json!({
                    "name": name,
                    "call_mode": "direct",
                    "result": result
                })),
                Err(err) => {
                    telemetry
                        .emit_batch_operations(
                            surface_profile,
                            &operation_names,
                            started.elapsed(),
                            results.len(),
                            telemetry_input.clone(),
                        )
                        .await;
                    if err.is_non_blocking_parser_error() {
                        debug!(
                            error = %err,
                            "suppressed non-blocking ParserError in HTTP batch response"
                        );
                    }
                    return tool_error_response(format!(
                        "[ERROR] {} failed: {}",
                        name,
                        err.user_facing_message()
                    ));
                }
            }
        } else if let Some(operation) = registry.get_operation(name) {
            if !operation.metadata.annotations.read_only
                || operation.metadata.annotations.destructive
            {
                return tool_error_response(format!(
                    "[ERROR] '{}' is not eligible for batch_operations because it is not read-only.",
                    name
                ));
            }
            match registry.execute_operation(name, args).await {
                Ok(result) => results.push(serde_json::json!({
                    "name": name,
                    "call_mode": "execute_operation",
                    "result": result
                })),
                Err(err) => {
                    telemetry
                        .emit_batch_operations(
                            surface_profile,
                            &operation_names,
                            started.elapsed(),
                            results.len(),
                            telemetry_input.clone(),
                        )
                        .await;
                    if err.is_non_blocking_parser_error() {
                        debug!(
                            error = %err,
                            "suppressed non-blocking ParserError in HTTP batch response"
                        );
                    }
                    return tool_error_response(format!(
                        "[ERROR] {} failed: {}",
                        name,
                        err.user_facing_message()
                    ));
                }
            }
        } else {
            return tool_error_response(format!("[ERROR] Unknown tool or operation: {}", name));
        }
    }

    telemetry
        .emit_batch_operations(
            surface_profile,
            &operation_names,
            started.elapsed(),
            results.len(),
            telemetry_input,
        )
        .await;
    tool_result_response(mcp_types::tool::ToolResult::with_structured(
        format!("Executed {} batched read-only operations.", results.len()),
        serde_json::json!({
            "count": results.len(),
            "results": results
        }),
    ))
}

async fn router_execute_response(
    registry: &ToolRegistry,
    arguments: &Value,
    telemetry: &AgenticTelemetry,
    surface_profile: ToolSurfaceProfile,
) -> Value {
    let started = Instant::now();
    let telemetry_input = AgenticTelemetryInput::from_arguments(arguments);
    let op_name = match arguments.get("name").and_then(|n| n.as_str()) {
        Some(name) => name,
        None => {
            return tool_error_response("[ERROR] Missing 'name' parameter".to_string());
        }
    };

    let op_args = arguments
        .get("arguments")
        .cloned()
        .unwrap_or(serde_json::json!({}));
    let query = op_args
        .get("query")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    match registry.execute_operation(op_name, op_args).await {
        Ok(result) => {
            telemetry
                .emit_managed_tool_readiness(op_name, &result)
                .await;
            telemetry
                .emit_execute_operation(
                    surface_profile,
                    op_name,
                    query.as_deref(),
                    started.elapsed(),
                    Some(&result),
                    telemetry_input,
                )
                .await;
            tool_result_response(result)
        }
        Err(err) => {
            telemetry
                .emit_execute_operation(
                    surface_profile,
                    op_name,
                    query.as_deref(),
                    started.elapsed(),
                    None,
                    telemetry_input,
                )
                .await;
            if err.is_non_blocking_parser_error() {
                debug!(
                    error = %err,
                    "suppressed non-blocking ParserError in HTTP execute_operation response"
                );
            }
            tool_error_response(format!("[ERROR] {}", err.user_facing_message()))
        }
    }
}

async fn handle_initialize_method(
    state: &HttpState,
    params: &Value,
) -> Result<Value, JsonRpcError> {
    let registry = effective_registry(state).await;
    Ok(build_legacy_initialize_result(
        &registry,
        mcp_types::MCP_PROTOCOL_2024_11_05,
        params,
    ))
}

async fn handle_list_tools_method(
    state: &HttpState,
    registry_override: Option<Arc<ToolRegistry>>,
) -> Result<Value, JsonRpcError> {
    let registry = match registry_override {
        Some(registry) => registry,
        None => effective_registry(state).await,
    };
    let cache_key = tools_list_cache_key(state, registry.as_ref()).await;
    if let Some(cached) = state
        .tools_list_cache
        .read()
        .unwrap_or_else(|err| err.into_inner())
        .get(&cache_key)
        .cloned()
    {
        return Ok(cached);
    }

    let tools = contextstream_tools_list(&registry, None);

    let response = serde_json::json!({ "tools": tools });
    state
        .tools_list_cache
        .write()
        .unwrap_or_else(|err| err.into_inner())
        .insert(cache_key, response.clone());

    Ok(response)
}

async fn handle_call_tool_method(
    state: &HttpState,
    params: Value,
    jsonrpc_id: Option<Value>,
    wire_observation: mcp_tools::wire_tokens::WireTokenObservation,
    strict_protocol_errors: bool,
    registry_override: Option<Arc<ToolRegistry>>,
) -> Result<Value, JsonRpcError> {
    let registry = match registry_override {
        Some(registry) => registry,
        None => effective_registry(state).await,
    };
    let name = params
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or_else(|| JsonRpcError {
            code: -32602,
            message: "Missing 'name' parameter".to_string(),
            data: None,
        })?;

    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or(serde_json::json!({}));
    if strict_protocol_errors {
        validate_contextstream_meta_tool_arguments(name, &arguments).map_err(JsonRpcError::from)?;
    }

    if registry.is_router_mode() {
        match name {
            "operations" => {
                return Ok(router_operations_response(registry.as_ref(), &arguments));
            }
            "execute_operation" => {
                let context = mcp_tools::wire_tokens::WireResponseContext::http_jsonrpc(
                    jsonrpc_id.clone(),
                    None,
                    None,
                )
                .with_observation(wire_observation.clone());
                return Ok(mcp_tools::wire_tokens::run_with_wire_response_context(
                    context,
                    router_execute_response(
                        registry.as_ref(),
                        &arguments,
                        &state.telemetry,
                        registry.tool_surface_profile(),
                    ),
                )
                .await);
            }
            "batch_operations" => {
                return Ok(batch_operations_response(
                    registry.as_ref(),
                    &arguments,
                    &state.telemetry,
                    registry.tool_surface_profile(),
                )
                .await);
            }
            _ => {}
        }
    }

    if registry.is_openai_agentic_surface() {
        match name {
            "tool_search" => {
                return Ok(tool_search_response(
                    registry.as_ref(),
                    &arguments,
                    &state.telemetry,
                    registry.tool_surface_profile(),
                )
                .await);
            }
            "execute_operation" => {
                let context = mcp_tools::wire_tokens::WireResponseContext::http_jsonrpc(
                    jsonrpc_id.clone(),
                    None,
                    None,
                )
                .with_observation(wire_observation.clone());
                return Ok(mcp_tools::wire_tokens::run_with_wire_response_context(
                    context,
                    router_execute_response(
                        registry.as_ref(),
                        &arguments,
                        &state.telemetry,
                        registry.tool_surface_profile(),
                    ),
                )
                .await);
            }
            "batch_operations" => {
                return Ok(batch_operations_response(
                    registry.as_ref(),
                    &arguments,
                    &state.telemetry,
                    registry.tool_surface_profile(),
                )
                .await);
            }
            _ => {}
        }

        if registry.get(name).is_none() && registry.get_operation(name).is_some() {
            state
                .telemetry
                .emit_hidden_direct_call_blocked(
                    registry.tool_surface_profile(),
                    name,
                    AgenticTelemetryInput::from_arguments(&arguments),
                )
                .await;
            return Ok(tool_error_response(format!(
                "[ERROR] '{}' is hidden on the adaptive surface. Call tool_search(query=\"...\") first, then execute_operation(name=\"{}\", arguments={{...}}).",
                name, name
            )));
        }
    }

    if strict_protocol_errors && registry.get(name).is_none() {
        return Err(JsonRpcError {
            code: -32602,
            message: format!("Unknown tool: {name}"),
            data: None,
        });
    }

    let call_title = registry.get(name).map(|tool| {
        crate::server::contextstream_call_title(
            &tool.metadata.name,
            &tool.metadata.title,
            tool.metadata.annotations.read_only,
            &arguments,
        )
    });
    let call_icon = crate::server::contextstream_call_icon(name);

    let wire_context = mcp_tools::wire_tokens::WireResponseContext::http_jsonrpc(
        jsonrpc_id,
        call_title.clone(),
        Some(call_icon.to_string()),
    )
    .with_observation(wire_observation);
    let outcome = mcp_tools::wire_tokens::run_with_wire_response_context(
        wire_context,
        registry.execute(name, arguments),
    )
    .await;

    match outcome {
        Ok(result) => {
            state
                .telemetry
                .emit_managed_tool_readiness(name, &result)
                .await;
            Ok(tool_result_response_with_title(
                result,
                call_title.as_deref(),
                Some(call_icon),
            ))
        }
        Err(err) => {
            if err.is_non_blocking_parser_error() {
                debug!(
                    error = %err,
                    "suppressed non-blocking ParserError in JSON-RPC tool response"
                );
            }
            Err(JsonRpcError {
                code: if strict_protocol_errors
                    && matches!(
                        &err,
                        mcp_types::Error::Validation(_)
                            | mcp_types::Error::InvalidUuid(_)
                            | mcp_types::Error::Serialization(_)
                    ) {
                    -32602
                } else {
                    -32603
                },
                message: err.user_facing_message(),
                data: None,
            })
        }
    }
}

// ============================================================================
// Auth Middleware
// ============================================================================

/// Extract an [`AuthOverride`] from HTTP request headers.
///
/// Supports:
/// - `Authorization: Bearer <api_key_or_jwt>`
/// - `X-API-Key` / `X-ContextStream-API-Key`
/// - `X-ContextStream-JWT`
/// - `X-ContextStream-Workspace-Id` / `X-Workspace-Id`
/// - `X-ContextStream-Project-Id` / `X-Project-Id`
/// - exact `X-ContextStream-Traffic-Class: synthetic-probe` on an
///   authenticated request (all other values are discarded)
fn extract_auth_override(headers: &HeaderMap) -> Option<AuthOverride> {
    let mut api_key: Option<String> = None;
    let mut jwt: Option<String> = None;

    // Explicit API-key headers
    if let Some(v) = headers
        .get("x-api-key")
        .or_else(|| headers.get("x-contextstream-api-key"))
        .and_then(|h| h.to_str().ok())
    {
        api_key = Some(v.to_string());
    }

    // Explicit JWT header
    if let Some(v) = headers
        .get("x-contextstream-jwt")
        .and_then(|h| h.to_str().ok())
    {
        jwt = Some(v.to_string());
    }

    // Authorization: Bearer – could be an API key or a JWT
    if api_key.is_none() && jwt.is_none() {
        if let Some(bearer) = headers
            .get(header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
        {
            let token = bearer.trim().to_string();
            // Heuristic: JWTs have three dot-separated base64 segments
            let parts: Vec<&str> = token.split('.').collect();
            if parts.len() == 3 && parts.iter().all(|p| !p.is_empty()) {
                jwt = Some(token);
            } else {
                api_key = Some(token);
            }
        }
    }

    let workspace_id = headers
        .get("x-contextstream-workspace-id")
        .or_else(|| headers.get("x-workspace-id"))
        .and_then(|h| h.to_str().ok())
        .and_then(|s| Uuid::parse_str(s).ok());

    let project_id = headers
        .get("x-contextstream-project-id")
        .or_else(|| headers.get("x-project-id"))
        .and_then(|h| h.to_str().ok())
        .and_then(|s| Uuid::parse_str(s).ok());

    // Never carry arbitrary caller-controlled traffic classes into backend
    // telemetry. Authentication is also required here; the API performs the
    // final probe-principal binding before accepting the classification.
    let has_credential = api_key
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
        || jwt.as_deref().is_some_and(|value| !value.trim().is_empty());
    let traffic_class = if has_credential {
        headers
            .get(TrafficClass::HEADER_NAME)
            .and_then(|h| h.to_str().ok())
            .and_then(TrafficClass::from_header_value)
    } else {
        None
    };

    let auth = AuthOverride {
        api_key,
        jwt,
        workspace_id,
        project_id,
        traffic_class,
    };

    if auth.is_empty() {
        None
    } else {
        Some(auth)
    }
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|value| value.to_str().ok())
}

fn parse_header_bool(headers: &HeaderMap, name: &str) -> Option<bool> {
    header_str(headers, name).and_then(|value| match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    })
}

fn parse_header_usize(headers: &HeaderMap, name: &str) -> Option<usize> {
    header_str(headers, name)
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
}

fn extract_config_override(headers: &HeaderMap) -> Option<ConfigOverride> {
    let acceleration_enabled = parse_header_bool(headers, "x-contextstream-acceleration-enabled");
    let atlas_enabled = parse_header_bool(headers, "x-contextstream-atlas-enabled");
    let override_config = ConfigOverride {
        context_pack_enabled: parse_header_bool(headers, "x-contextstream-context-pack-enabled"),
        toolset: header_str(headers, "x-contextstream-toolset")
            .and_then(|value| value.parse::<Toolset>().ok()),
        output_format: header_str(headers, "x-contextstream-output-format")
            .and_then(|value| value.parse::<OutputFormat>().ok()),
        progressive_mode: parse_header_bool(headers, "x-contextstream-progressive-mode"),
        router_mode: parse_header_bool(headers, "x-contextstream-router-mode"),
        consolidated_mode: parse_header_bool(headers, "x-contextstream-consolidated"),
        auto_hide_integrations: parse_header_bool(
            headers,
            "x-contextstream-auto-hide-integrations",
        ),
        search_limit: parse_header_usize(headers, "x-contextstream-search-limit"),
        search_max_chars: parse_header_usize(headers, "x-contextstream-search-max-chars"),
        transcripts_enabled: parse_header_bool(headers, "x-contextstream-transcripts-enabled"),
        hook_transcripts_enabled: parse_header_bool(
            headers,
            "x-contextstream-hook-transcripts-enabled",
        ),
        tool_surface_profile: header_str(headers, "x-contextstream-tool-surface-profile")
            .and_then(|value| value.parse::<ToolSurfaceProfile>().ok()),
        // Per-request override for the acceleration layer. The new
        // header takes precedence; the old Atlas header is accepted as
        // a deprecated alias during migration.
        acceleration_enabled,
        atlas_enabled,
    };

    if override_config.is_empty() {
        None
    } else {
        Some(override_config)
    }
}

fn host_without_port(host: &str) -> &str {
    let trimmed = host.trim().trim_start_matches('[').trim_end_matches(']');
    if let Some((value, _port)) = trimmed.rsplit_once(':') {
        if !value.contains(':') {
            return value;
        }
    }
    trimmed
}

fn host_is_local_or_private(host: &str) -> bool {
    let host = host_without_port(host);

    if host.eq_ignore_ascii_case("localhost")
        || host.eq_ignore_ascii_case("127.0.0.1")
        || host.eq_ignore_ascii_case("::1")
    {
        return true;
    }

    if let Ok(addr) = host.parse::<std::net::Ipv4Addr>() {
        let octets = addr.octets();
        return octets[0] == 10
            || (octets[0] == 172 && (16..=31).contains(&octets[1]))
            || (octets[0] == 192 && octets[1] == 168)
            || octets[0] == 127;
    }

    false
}

fn normalized_public_origin(url: &str, fallback: &str) -> String {
    let trimmed = url.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return fallback.to_string();
    }

    if let Ok(parsed) = reqwest::Url::parse(trimmed) {
        if let Some(host) = parsed.host_str() {
            if host_is_local_or_private(host) {
                return fallback.to_string();
            }
        }
        return format!(
            "{}://{}",
            parsed.scheme(),
            parsed
                .host_str()
                .map(|host| {
                    if let Some(port) = parsed.port() {
                        format!("{host}:{port}")
                    } else {
                        host.to_string()
                    }
                })
                .unwrap_or_else(|| fallback.trim_end_matches('/').to_string())
        );
    }

    fallback.to_string()
}

fn authorization_server_origin() -> String {
    if let Ok(public_api_url) = std::env::var("CONTEXTSTREAM_PUBLIC_API_URL") {
        let origin = normalized_public_origin(&public_api_url, DEFAULT_PUBLIC_API_ORIGIN);
        if !origin.is_empty() {
            return origin;
        }
    }

    let api_url = std::env::var("CONTEXTSTREAM_API_URL")
        .unwrap_or_else(|_| DEFAULT_PUBLIC_API_ORIGIN.to_string());
    normalized_public_origin(&api_url, DEFAULT_PUBLIC_API_ORIGIN)
}

fn request_origin(headers: &HeaderMap) -> String {
    if let Ok(public_mcp_origin) = std::env::var("CONTEXTSTREAM_PUBLIC_MCP_ORIGIN") {
        let origin = normalized_public_origin(&public_mcp_origin, DEFAULT_PUBLIC_MCP_ORIGIN);
        if !origin.is_empty() {
            return origin;
        }
    }

    let host = header_str(headers, "x-forwarded-host")
        .or_else(|| header_str(headers, "host"))
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let mut scheme = header_str(headers, "x-forwarded-proto")
        .map(str::trim)
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| match host {
            Some(value) if host_is_local_or_private(value) => "http",
            _ => "https",
        });

    if let Some(value) = host {
        if !host_is_local_or_private(value) {
            scheme = "https";
        }
    }

    match host {
        Some(value) => format!("{scheme}://{}", value.trim_end_matches('/')),
        None => DEFAULT_PUBLIC_MCP_ORIGIN.to_string(),
    }
}

fn oauth_resource_metadata_url(headers: &HeaderMap) -> String {
    format!(
        "{}/.well-known/oauth-protected-resource",
        request_origin(headers).trim_end_matches('/')
    )
}

fn oauth_protected_resource_metadata(headers: &HeaderMap) -> Value {
    json!({
        "resource": request_origin(headers),
        "authorization_servers": [authorization_server_origin()]
    })
}

fn oauth_unauthorized_response(
    headers: &HeaderMap,
    error_description: &str,
    body_description: &str,
) -> Response {
    let mut response = (
        StatusCode::UNAUTHORIZED,
        Json(json!({
            "error": "unauthorized",
            "error_description": body_description,
            "_meta": {
                "mcp/www_authenticate": {
                    "error": "invalid_token",
                    "error_description": error_description,
                },
            },
        })),
    )
        .into_response();

    let www_authenticate = format!(
        r#"Bearer resource_metadata="{}""#,
        oauth_resource_metadata_url(headers)
    );
    if let Ok(value) = header::HeaderValue::from_str(&www_authenticate) {
        response
            .headers_mut()
            .insert(header::WWW_AUTHENTICATE, value);
    }

    response
}

/// Authentication middleware that supports API keys, JWTs, and header-based auth.
pub async fn auth_middleware(
    State(state): State<HttpState>,
    headers: HeaderMap,
    request: Request<Body>,
    next: Next,
) -> Result<Response, Response> {
    let path = request.uri().path();
    if is_streamable_mcp_path(path) && !mcp_origin_is_allowed(&headers) {
        return Err(stateless_protocol_error_response(
            StatusCode::FORBIDDEN,
            None,
            JsonRpcError {
                code: -32000,
                message: "Forbidden: Origin is not allowed".to_string(),
                data: None,
            },
        ));
    }
    if is_streamable_mcp_path(path) && matches!(request.method(), &Method::GET | &Method::DELETE) {
        return Ok(StatusCode::METHOD_NOT_ALLOWED.into_response());
    }

    // Skip auth for health + Prometheus scrape endpoints + every
    // OAuth discovery endpoint. Discovery is by definition
    // unauthenticated — challenging it for a Bearer token (which is
    // what an OAuth client is trying to *obtain*) is the bug that
    // surfaced as `mcp_client_invalid` in claude.ai's custom
    // connector flow.
    if path == "/health"
        || path == "/api/v1/health"
        || path == "/metrics"
        || path == "/.well-known/oauth-protected-resource"
        || path == "/.well-known/oauth-authorization-server"
        || path == "/.well-known/openid-configuration"
    {
        return Ok(next.run(request).await);
    }

    let auth = extract_auth_override(&headers);
    let config_override = extract_config_override(&headers);
    let request_installation_id = headers
        .get("x-contextstream-installation-id")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| Uuid::parse_str(value.trim()).ok())
        .filter(|value| !value.is_nil());

    // Decode the JWT (if one was presented and we have a secret to verify it
    // with) so we can both (a) reject invalid tokens and (b) extract the
    // authenticated subject for per-caller session isolation further down.
    let mut jwt_subject: Option<String> = None;
    if let (Some(secret), Some(ref a)) = (&state.jwt_secret, &auth) {
        if let Some(ref token) = a.jwt {
            let validation = Validation::new(Algorithm::HS256);
            match decode::<Claims>(
                token,
                &DecodingKey::from_secret(secret.as_bytes()),
                &validation,
            ) {
                Ok(data) => {
                    jwt_subject = Some(data.claims.sub);
                }
                Err(_) => {
                    warn!("JWT validation failed");
                    return Err(oauth_unauthorized_response(
                        &headers,
                        "Invalid or expired access token",
                        "Bearer token invalid or expired",
                    ));
                }
            }
        }
    }

    // If auth is required, reject requests without credentials
    if state.require_auth
        && auth
            .as_ref()
            .is_none_or(|a| a.api_key.is_none() && a.jwt.is_none())
    {
        return Err(oauth_unauthorized_response(
            &headers,
            "No access token provided",
            "Bearer token required",
        ));
    }

    // Derive a SessionKey that partitions in-memory SessionState across
    // callers. Precedence:
    //   1. JWT sub (verified above)               — hosted multi-tenant
    //   2. Domain-separated SHA-256 of API key    — api-key auth
    //   3. AnonymousHttp(MCP session/request id)  — unauthenticated HTTP
    // Without this, SessionManager's state would be a process-wide
    // singleton and one caller's folder_path / workspace_id / project_id
    // would bleed into another caller's session.
    // For initialize-era requests, fold the MCP session id (assigned at
    // initialize and echoed by compliant clients) into the bucket key so
    // CONCURRENT MCP sessions of the same account get isolated state. The
    // stateless 2026 contract ignores this obsolete header and uses a
    // request-unique bucket that is discarded after the response. Without
    // legacy partitioning, two sessions of one user (an editor session and a
    // background agent) share one bucket and silently rescope each other's
    // folder/workspace/project (audit 2026-07-17). The auth identity stays in
    // the key, so one tenant can never reach another tenant's bucket even on
    // session-id collision.
    let stateless_transport = protocol_header_uses_stateless_contract(&headers);
    let mcp_session_id: Option<String> = (!stateless_transport)
        .then(|| {
            headers
                .get("mcp-session-id")
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|s| !s.is_empty() && s.len() <= 128)
                .map(str::to_string)
        })
        .flatten();
    let request_nonce = Uuid::new_v4().simple().to_string();
    let (session_key, effective_mcp_session_id) = if stateless_transport {
        (
            derive_stateless_http_request_key(
                jwt_subject.as_deref(),
                auth.as_ref().and_then(|a| a.api_key.as_deref()),
                &request_nonce,
            ),
            None,
        )
    } else {
        derive_http_session_context(
            jwt_subject.as_deref(),
            auth.as_ref().and_then(|a| a.api_key.as_deref()),
            mcp_session_id.as_deref(),
            &request_nonce,
        )
    };
    let transient_session_key = stateless_transport.then(|| session_key.clone());
    let caller_cache_identity = derive_http_caller_cache_identity(
        jwt_subject.as_deref(),
        auth.as_ref().and_then(|a| a.api_key.as_deref()),
    );

    // Wrap handler execution with per-request auth, config, and session-
    // identity context. SessionManager reads the session key via
    // get_task_session_key() to pick the right in-memory bucket for this
    // caller; ContextStreamClient reads the auth override.
    let exec = || async move {
        match (auth, config_override) {
            (Some(a), Some(config)) => {
                run_with_session_key(session_key, || async {
                    run_with_auth_override(a, || async {
                        run_with_config_override(config, || next.run(request)).await
                    })
                    .await
                })
                .await
            }
            (Some(a), None) => {
                run_with_session_key(session_key, || async {
                    run_with_auth_override(a, || next.run(request)).await
                })
                .await
            }
            (None, Some(config)) => {
                run_with_session_key(session_key, || async {
                    run_with_config_override(config, || next.run(request)).await
                })
                .await
            }
            (None, None) => run_with_session_key(session_key, || next.run(request)).await,
        }
    };
    let exec_with_installation = || async move {
        match request_installation_id {
            Some(installation_id) => run_with_installation_id(installation_id, exec).await,
            None => exec().await,
        }
    };
    let exec_with_caller_cache = || async move {
        match caller_cache_identity {
            Some(identity) => {
                run_with_caller_cache_identity(identity, exec_with_installation).await
            }
            None => exec_with_installation().await,
        }
    };
    // Expose the transport-level MCP session id to tools so backend calls can
    // carry a durable session identity (scope persistence + rehydration).
    let response = match effective_mcp_session_id {
        Some(sid) => mcp_client::run_with_mcp_session_id(sid, exec_with_caller_cache).await,
        None => exec_with_caller_cache().await,
    };
    if let Some(key) = transient_session_key {
        state.session.discard_transient_state(&key);
    }
    Ok(response)
}

/// Build a stable, secret-free cache identity for an authenticated principal.
///
/// Initialize-era session state is partitioned by
/// `derive_http_session_context`, while modern request state is transient.
/// Read caches use this separate identity so the same authenticated caller can
/// reuse safe entries while distinct JWT subjects and API keys remain
/// cryptographically isolated.
fn derive_http_caller_cache_identity(
    jwt_subject: Option<&str>,
    api_key: Option<&str>,
) -> Option<String> {
    if let Some(subject) = jwt_subject {
        return SessionKey::Jwt(subject.to_string()).atlas_user_scope_token();
    }
    api_key.and_then(|key| SessionKey::ApiKey(key.to_string()).atlas_user_scope_token())
}

/// Build a request-unique state bucket for the stateless transport contract.
/// The authenticated caller still has a stable, separate cache identity, but
/// mutable SessionManager fields can never become a hidden cross-request
/// protocol session.
fn derive_stateless_http_request_key(
    jwt_subject: Option<&str>,
    api_key: Option<&str>,
    request_nonce: &str,
) -> SessionKey {
    let partition = format!("stateless-request:{request_nonce}");
    if let Some(subject) = jwt_subject {
        return SessionKey::for_http_jwt(subject, Some(&partition));
    }
    if let Some(api_key) = api_key {
        return SessionKey::for_http_api_key(api_key, Some(&partition));
    }
    SessionKey::for_anonymous_http(&partition)
}

/// Resolve the HTTP task-local session context without ever representing an
/// anonymous request as `SessionKey::Local`. Anonymous requests with a valid
/// MCP session header are stable only within that session; when the header is
/// absent the transport-provided nonce creates a one-request partition.
fn derive_http_session_context(
    jwt_subject: Option<&str>,
    api_key: Option<&str>,
    mcp_session_id: Option<&str>,
    anonymous_request_nonce: &str,
) -> (SessionKey, Option<String>) {
    if let Some(subject) = jwt_subject {
        return (
            SessionKey::for_http_jwt(subject, mcp_session_id),
            mcp_session_id.map(str::to_string),
        );
    }
    if let Some(api_key) = api_key {
        return (
            SessionKey::for_http_api_key(api_key, mcp_session_id),
            mcp_session_id.map(str::to_string),
        );
    }

    let effective_session_id = mcp_session_id
        .map(str::to_string)
        .unwrap_or_else(|| format!("anonymous-request:{anonymous_request_nonce}"));
    (
        SessionKey::for_anonymous_http(&effective_session_id),
        Some(effective_session_id),
    )
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    #![allow(clippy::field_reassign_with_default)]
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tower::ServiceExt;

    fn create_protocol_test_state() -> HttpState {
        let config = Config {
            api_key: Some("test-api-key".to_string()),
            is_http_transport: true,
            ..Config::default()
        };
        let client = ContextStreamClient::new(config.clone());
        let session = Arc::new(SessionManager::new(client.clone(), config.clone()));
        let registry = build_registry(&config, client.clone(), session.clone());
        HttpState {
            registry: Arc::new(registry),
            client: client.clone(),
            session: session.clone(),
            jwt_secret: None,
            require_auth: false,
            telemetry: AgenticTelemetry::new(client, session),
            tools_list_cache: Arc::new(RwLock::new(HashMap::new())),
            concurrency_semaphore: Arc::new(tokio::sync::Semaphore::new(8)),
            metrics_handle: None,
        }
    }

    fn stateless_params(version: &str) -> Value {
        json!({
            "_meta": {
                mcp_types::MCP_META_PROTOCOL_VERSION: version,
                mcp_types::MCP_META_CLIENT_INFO: {
                    "name": "codex-cli",
                    "version": "1.2.3"
                },
                mcp_types::MCP_META_CLIENT_CAPABILITIES: {}
            }
        })
    }

    fn stateless_streamable_request(
        body: Value,
        header_version: &str,
        header_method: &str,
        header_name: Option<&str>,
        origin: Option<&str>,
    ) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("host", "mcp.contextstream.io")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header(MCP_PROTOCOL_VERSION_HEADER, header_version)
            .header(MCP_METHOD_HEADER, header_method);
        if let Some(name) = header_name {
            builder = builder.header(MCP_NAME_HEADER, name);
        }
        if let Some(origin) = origin {
            builder = builder.header(header::ORIGIN, origin);
        }
        builder
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    }

    async fn json_response(response: Response) -> Value {
        let body = response.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&body).unwrap()
    }

    #[test]
    fn managed_runtime_identity_requires_complete_bounded_exact_headers() {
        let installation_id = Uuid::new_v4();
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-contextstream-installation-id",
            installation_id.to_string().parse().unwrap(),
        );
        headers.insert("x-contextstream-client", "codex".parse().unwrap());
        headers.insert(
            "x-contextstream-managed-config-version",
            "2".parse().unwrap(),
        );
        headers.insert(
            "x-contextstream-teaching-version",
            "harness_teaching_v4".parse().unwrap(),
        );

        assert_eq!(
            managed_harness_runtime_identity(&headers),
            Some(ManagedHarnessRuntimeIdentity {
                installation_id,
                harness_id: HarnessId::Codex,
                managed_config_version: "2".to_string(),
                teaching_version: "harness_teaching_v4".to_string(),
            })
        );

        let mut incomplete = headers.clone();
        incomplete.remove("x-contextstream-teaching-version");
        assert!(managed_harness_runtime_identity(&incomplete).is_none());

        let mut invalid = headers;
        invalid.insert(
            "x-contextstream-installation-id",
            Uuid::nil().to_string().parse().unwrap(),
        );
        assert!(managed_harness_runtime_identity(&invalid).is_none());
    }

    #[test]
    fn stateless_http_routing_decodes_names_and_validates_origins() {
        let unicode_name = "搜尋/工具";
        let encoded = format!("=?base64?{}?=", BASE64_STANDARD.encode(unicode_name));
        assert_eq!(
            decode_mcp_name_header(&encoded).expect("valid base64 sentinel"),
            unicode_name
        );
        assert_eq!(
            decode_mcp_name_header("search").expect("plain ASCII name"),
            "search"
        );
        assert_eq!(
            decode_mcp_name_header("=?base64?literal")
                .expect("a non-sentinel prefix remains plain text"),
            "=?base64?literal"
        );
        assert_eq!(
            decode_mcp_name_header("=?base64?not-base64!?=")
                .expect_err("invalid sentinel must fail closed")
                .code,
            mcp_types::MCP_ERROR_HEADER_MISMATCH
        );
        assert_eq!(
            decode_mcp_name_header(" search ")
                .expect_err("plain leading or trailing whitespace must be encoded")
                .code,
            mcp_types::MCP_ERROR_HEADER_MISMATCH
        );

        let mut headers = HeaderMap::new();
        headers.insert("host", "mcp.contextstream.io".parse().unwrap());
        headers.insert(
            header::ORIGIN,
            "https://mcp.contextstream.io".parse().unwrap(),
        );
        assert!(mcp_origin_is_allowed(&headers));

        headers.insert(header::ORIGIN, "https://claude.ai".parse().unwrap());
        assert!(mcp_origin_is_allowed(&headers));

        headers.insert(header::ORIGIN, "https://attacker.example".parse().unwrap());
        assert!(!mcp_origin_is_allowed(&headers));
    }

    #[test]
    fn stateless_http_state_is_request_unique_but_cache_identity_is_stable() {
        let subject = "sensitive-user-subject";
        let api_key = "sensitive-api-key";
        let first =
            derive_stateless_http_request_key(Some(subject), None, "sensitive-request-nonce-a");
        let second =
            derive_stateless_http_request_key(Some(subject), None, "sensitive-request-nonce-b");
        assert_ne!(first, second);
        let rendered = format!("{first:?}");
        assert!(!rendered.contains(subject));
        assert!(!rendered.contains("sensitive-request-nonce-a"));

        let api_first =
            derive_stateless_http_request_key(None, Some(api_key), "api-request-nonce-a");
        let api_second =
            derive_stateless_http_request_key(None, Some(api_key), "api-request-nonce-b");
        assert_ne!(api_first, api_second);
        assert!(!format!("{api_first:?}").contains(api_key));

        let anonymous_first =
            derive_stateless_http_request_key(None, None, "anonymous-request-nonce-a");
        let anonymous_second =
            derive_stateless_http_request_key(None, None, "anonymous-request-nonce-b");
        assert_ne!(anonymous_first, anonymous_second);

        let cache_identity_a = derive_http_caller_cache_identity(Some(subject), None);
        let cache_identity_b = derive_http_caller_cache_identity(Some(subject), None);
        assert_eq!(cache_identity_a, cache_identity_b);
        assert_ne!(
            cache_identity_a,
            derive_http_caller_cache_identity(Some("different-subject"), None)
        );
    }

    #[tokio::test]
    async fn stateless_http_discovery_and_tools_list_expose_the_2026_contract() {
        let state = create_protocol_test_state();
        state
            .registry
            .set_tool_surface_profile(ToolSurfaceProfile::OpenaiAgentic);
        let registry = state.registry.clone();
        let app = create_router(state);
        let discover = json!({
            "jsonrpc": "2.0",
            "id": "discover-http-2026",
            "method": "server/discover",
            "params": stateless_params(MCP_PROTOCOL_2026_07_28)
        });
        let mut request = stateless_streamable_request(
            discover,
            MCP_PROTOCOL_2026_07_28,
            "server/discover",
            None,
            Some("https://mcp.contextstream.io"),
        );
        request
            .headers_mut()
            .insert("mcp-session-id", "obsolete-session".parse().unwrap());
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            !response.headers().contains_key("mcp-session-id"),
            "the stateless contract must never return a protocol session"
        );
        let discover = json_response(response).await;
        assert_eq!(
            discover["result"]["supportedVersions"],
            json!([MCP_PROTOCOL_2026_07_28])
        );
        assert_eq!(discover["result"]["resultType"], "complete");
        assert_eq!(discover["result"]["ttlMs"], mcp_types::MCP_DISCOVERY_TTL_MS);
        assert_eq!(discover["result"]["cacheScope"], "private");
        assert_eq!(
            discover["result"]["_meta"][mcp_types::MCP_META_SERVER_INFO]["name"],
            "contextstream-mcp"
        );
        assert_eq!(
            registry.tool_surface_profile(),
            ToolSurfaceProfile::OpenaiAgentic,
            "modern discovery must not mutate a legacy client's shared surface profile"
        );

        let tools_list = json!({
            "jsonrpc": "2.0",
            "id": "tools-http-2026",
            "method": "tools/list",
            "params": stateless_params(MCP_PROTOCOL_2026_07_28)
        });
        let response = app
            .oneshot(stateless_streamable_request(
                tools_list,
                MCP_PROTOCOL_2026_07_28,
                "tools/list",
                None,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key("mcp-session-id"));
        let tools = json_response(response).await;
        assert!(tools["result"]["tools"].is_array());
        assert!(tools["result"]["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .any(|tool| tool["name"] == "search"));
        assert_eq!(tools["result"]["resultType"], "complete");
        assert_eq!(tools["result"]["ttlMs"], MCP_TOOLS_LIST_TTL_MS);
        assert_eq!(tools["result"]["cacheScope"], "private");
        assert_eq!(
            registry.tool_surface_profile(),
            ToolSurfaceProfile::OpenaiAgentic,
            "modern tool discovery must use an isolated request registry"
        );
    }

    #[tokio::test]
    async fn stateless_http_rejects_header_drift_future_versions_and_missing_names() {
        let app = create_router(create_protocol_test_state());
        let discovery = json!({
            "jsonrpc": "2.0",
            "id": "method-mismatch",
            "method": "server/discover",
            "params": stateless_params(MCP_PROTOCOL_2026_07_28)
        });
        let response = app
            .clone()
            .oneshot(stateless_streamable_request(
                discovery,
                MCP_PROTOCOL_2026_07_28,
                "tools/list",
                None,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let mismatch = json_response(response).await;
        assert_eq!(
            mismatch["error"]["code"],
            mcp_types::MCP_ERROR_HEADER_MISMATCH
        );

        let future = json!({
            "jsonrpc": "2.0",
            "id": "future-version",
            "method": "tools/list",
            "params": stateless_params("2099-01-01")
        });
        let response = app
            .clone()
            .oneshot(stateless_streamable_request(
                future,
                "2099-01-01",
                "tools/list",
                None,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let future = json_response(response).await;
        assert_eq!(
            future["error"]["code"],
            mcp_types::MCP_ERROR_UNSUPPORTED_PROTOCOL_VERSION
        );
        assert_eq!(future["error"]["data"]["requested"], "2099-01-01");

        let call = json!({
            "jsonrpc": "2.0",
            "id": "missing-name",
            "method": "tools/call",
            "params": {
                "name": "search",
                "arguments": {},
                "_meta": stateless_params(MCP_PROTOCOL_2026_07_28)["_meta"].clone()
            }
        });
        let response = app
            .clone()
            .oneshot(stateless_streamable_request(
                call,
                MCP_PROTOCOL_2026_07_28,
                "tools/call",
                None,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let missing_name = json_response(response).await;
        assert_eq!(
            missing_name["error"]["code"],
            mcp_types::MCP_ERROR_HEADER_MISMATCH
        );

        let missing_id = json!({
            "jsonrpc": "2.0",
            "method": "tools/list",
            "params": stateless_params(MCP_PROTOCOL_2026_07_28)
        });
        let response = app
            .clone()
            .oneshot(stateless_streamable_request(
                missing_id,
                MCP_PROTOCOL_2026_07_28,
                "tools/list",
                None,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let missing_id = json_response(response).await;
        assert_eq!(missing_id["error"]["code"], -32600);

        let invalid_cursor = json!({
            "jsonrpc": "2.0",
            "id": "invalid-cursor",
            "method": "tools/list",
            "params": {
                "cursor": 7,
                "_meta": stateless_params(MCP_PROTOCOL_2026_07_28)["_meta"].clone()
            }
        });
        let response = app
            .oneshot(stateless_streamable_request(
                invalid_cursor,
                MCP_PROTOCOL_2026_07_28,
                "tools/list",
                None,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let invalid_cursor = json_response(response).await;
        assert_eq!(invalid_cursor["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn stateless_http_uses_404_for_unimplemented_methods_and_403_for_bad_origin() {
        let app = create_router(create_protocol_test_state());
        let unimplemented = json!({
            "jsonrpc": "2.0",
            "id": "resources-list",
            "method": "resources/list",
            "params": stateless_params(MCP_PROTOCOL_2026_07_28)
        });
        let response = app
            .clone()
            .oneshot(stateless_streamable_request(
                unimplemented,
                MCP_PROTOCOL_2026_07_28,
                "resources/list",
                None,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let unimplemented = json_response(response).await;
        assert_eq!(unimplemented["error"]["code"], -32601);

        let ping = json!({
            "jsonrpc": "2.0",
            "id": "removed-ping",
            "method": "ping",
            "params": stateless_params(MCP_PROTOCOL_2026_07_28)
        });
        let response = app
            .clone()
            .oneshot(stateless_streamable_request(
                ping,
                MCP_PROTOCOL_2026_07_28,
                "ping",
                None,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let ping = json_response(response).await;
        assert_eq!(ping["error"]["code"], -32601);

        let unknown_tool = json!({
            "jsonrpc": "2.0",
            "id": "unknown-tool",
            "method": "tools/call",
            "params": {
                "name": "does_not_exist",
                "arguments": {},
                "_meta": stateless_params(MCP_PROTOCOL_2026_07_28)["_meta"].clone()
            }
        });
        let response = app
            .clone()
            .oneshot(stateless_streamable_request(
                unknown_tool,
                MCP_PROTOCOL_2026_07_28,
                "tools/call",
                Some("does_not_exist"),
                None,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let unknown_tool = json_response(response).await;
        assert_eq!(unknown_tool["error"]["code"], -32602);

        let discovery = json!({
            "jsonrpc": "2.0",
            "id": "bad-origin",
            "method": "server/discover",
            "params": stateless_params(MCP_PROTOCOL_2026_07_28)
        });
        let response = app
            .oneshot(stateless_streamable_request(
                discovery,
                MCP_PROTOCOL_2026_07_28,
                "server/discover",
                None,
                Some("https://attacker.example"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let forbidden = json_response(response).await;
        assert_eq!(forbidden["error"]["code"], -32000);
    }

    #[tokio::test]
    async fn legacy_http_initialize_keeps_its_negotiated_session_contract() {
        let app = create_router(create_protocol_test_state());
        let body = json!({
            "jsonrpc": "2.0",
            "id": "legacy-initialize",
            "method": "initialize",
            "params": {
                "protocolVersion": mcp_types::MCP_PROTOCOL_2024_11_05,
                "clientInfo": {"name": "legacy-client", "version": "1.0.0"},
                "capabilities": {}
            }
        });
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .header("accept", "application/json, text/event-stream")
                    .body(Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().contains_key("mcp-session-id"));
        let response = json_response(response).await;
        assert_eq!(
            response["result"]["protocolVersion"],
            mcp_types::MCP_PROTOCOL_2024_11_05
        );
        assert!(response["result"].get("resultType").is_none());
    }

    #[tokio::test]
    async fn stateless_http_endpoint_rejects_get_and_delete() {
        let mut state = create_protocol_test_state();
        state.require_auth = true;
        let app = create_router(state);
        for method in ["GET", "DELETE"] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri("/mcp")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        }
    }

    #[tokio::test]
    async fn managed_stream_initialize_respects_disabled_remote_readiness() {
        async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
            let mut request = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                let read = socket.read(&mut chunk).await.expect("read request");
                assert!(read > 0);
                request.extend_from_slice(&chunk[..read]);
                let Some(header_end) = request
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .map(|index| index + 4)
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if request.len() >= header_end + content_length {
                    return String::from_utf8(request).expect("UTF-8 request");
                }
            }
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind readiness backend");
        let address = listener.local_addr().expect("readiness backend address");
        let mut backend = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept readiness event");
            let request = read_request(&mut socket).await;
            let body = serde_json::json!({
                "success": true,
                "data": {"inserted": true, "current_updated": true}
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write readiness response");
            request
        });

        let config = Config {
            api_url: format!("http://{address}"),
            api_key: Some("process-key-must-not-be-used".to_string()),
            is_http_transport: true,
            ..Default::default()
        };
        let client = ContextStreamClient::new(config.clone());
        let session = Arc::new(SessionManager::new(client.clone(), config.clone()));
        let registry = build_registry(&config, client.clone(), session.clone());
        let state = HttpState {
            registry: Arc::new(registry),
            client: client.clone(),
            session: session.clone(),
            jwt_secret: None,
            require_auth: false,
            telemetry: AgenticTelemetry::new(client, session),
            tools_list_cache: Arc::new(RwLock::new(HashMap::new())),
            concurrency_semaphore: Arc::new(tokio::sync::Semaphore::new(8)),
            metrics_handle: None,
        };
        let installation_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        let response = create_router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .header("accept", "application/json, text/event-stream")
                    .header("x-contextstream-api-key", "caller-request-key")
                    .header("x-contextstream-client", "codex")
                    .header(
                        "x-contextstream-installation-id",
                        installation_id.to_string(),
                    )
                    .header("x-contextstream-managed-config-version", "2")
                    .header("x-contextstream-teaching-version", "harness_teaching_v4")
                    .header("x-contextstream-workspace-id", workspace_id.to_string())
                    .header("x-contextstream-project-id", project_id.to_string())
                    .body(Body::from(
                        serde_json::json!({
                            "jsonrpc": "2.0",
                            "id": 1,
                            "method": "initialize",
                            "params": {
                                "protocolVersion": "2024-11-05",
                                "clientInfo": {"name": "codex", "version": "e2e"},
                                "capabilities": {}
                            }
                        })
                        .to_string(),
                    ))
                    .expect("initialize request"),
            )
            .await
            .expect("initialize response");
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().contains_key("mcp-session-id"));

        // Runtime readiness upload is a beta rollout and must remain disabled
        // unless the process explicitly opts in. The client crate separately
        // exercises enabled delivery, caller-auth propagation, and the exact
        // privacy-bounded wire payload without mutating process-global env.
        assert!(
            tokio::time::timeout(Duration::from_millis(250), &mut backend)
                .await
                .is_err(),
            "default-off remote readiness unexpectedly contacted the backend"
        );
        backend.abort();
        let cancellation = backend
            .await
            .expect_err("backend listener should be cancelled");
        assert!(cancellation.is_cancelled());
    }

    #[test]
    fn http_tool_result_counter_matches_actual_jsonrpc_serialization() {
        let result = mcp_types::tool::ToolResult::with_structured(
            "数据库 👩‍💻 \\\"json\\\"",
            serde_json::json!({"answer": "grounded"}),
        );
        for id in [serde_json::json!(42), serde_json::json!("request-long-id")] {
            let title = "Loading ContextStream context";
            let icon = "⌬";
            let payload = tool_result_response_with_title(result.clone(), Some(title), Some(icon));
            let actual = serde_json::to_vec(&JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: Some(id.clone()),
                result: Some(payload),
                error: None,
            })
            .unwrap();
            let context = mcp_tools::wire_tokens::WireResponseContext::http_jsonrpc(
                Some(id),
                Some(title.to_string()),
                Some(icon.to_string()),
            );
            let canonical =
                mcp_tools::wire_tokens::canonical_tool_result_bytes(&result, &context).unwrap();
            assert_eq!(actual, canonical);
            assert_ne!(actual.last(), Some(&b'\n'));
        }
    }

    #[test]
    fn legacy_http_rest_counter_matches_actual_tool_result_serialization() {
        let result = mcp_types::tool::ToolResult::with_structured(
            "数据库 👩‍💻 \\\"json\\\"",
            serde_json::json!({"answer": "grounded"}),
        );
        let title = "Loading ContextStream context";
        let icon = "⌬";
        let payload = rest_tool_result_response_with_title(result.clone(), Some(title), Some(icon));
        let actual = serde_json::to_vec(&payload).unwrap();
        let context = mcp_tools::wire_tokens::WireResponseContext::http_rest(
            Some(title.to_string()),
            Some(icon.to_string()),
        );
        let canonical =
            mcp_tools::wire_tokens::canonical_tool_result_bytes(&result, &context).unwrap();
        assert_eq!(actual, canonical);
        assert_ne!(actual.last(), Some(&b'\n'));
        assert!(serde_json::from_slice::<Value>(&actual)
            .unwrap()
            .get("jsonrpc")
            .is_none());
    }

    // ========================================================================
    // Type Serialization Tests
    // ========================================================================

    mod type_tests {
        use super::*;

        #[test]
        fn test_jsonrpc_request_deserialization() {
            let json = r#"{
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {}
            }"#;

            let request: JsonRpcRequest = serde_json::from_str(json).unwrap();
            assert_eq!(request.jsonrpc, "2.0");
            assert_eq!(request.id, Some(serde_json::json!(1)));
            assert_eq!(request.method, "initialize");
            assert_eq!(request.params, serde_json::json!({}));
        }

        #[test]
        fn test_jsonrpc_request_without_id() {
            // Notifications have no id
            let json = r#"{
                "jsonrpc": "2.0",
                "method": "notifications/progress",
                "params": {"progress": 50}
            }"#;

            let request: JsonRpcRequest = serde_json::from_str(json).unwrap();
            assert_eq!(request.jsonrpc, "2.0");
            assert!(request.id.is_none());
            assert_eq!(request.method, "notifications/progress");
        }

        #[test]
        fn test_jsonrpc_request_without_params() {
            // Params defaults to null when not provided
            let json = r#"{
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/list"
            }"#;

            let request: JsonRpcRequest = serde_json::from_str(json).unwrap();
            assert_eq!(request.params, serde_json::Value::Null);
        }

        #[test]
        fn test_jsonrpc_response_serialization() {
            let response = JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: Some(serde_json::json!(1)),
                result: Some(serde_json::json!({"status": "ok"})),
                error: None,
            };

            let json = serde_json::to_string(&response).unwrap();
            assert!(json.contains("\"jsonrpc\":\"2.0\""));
            assert!(json.contains("\"id\":1"));
            assert!(json.contains("\"result\""));
            assert!(!json.contains("\"error\"")); // Should skip None
        }

        #[test]
        fn test_jsonrpc_response_with_error() {
            let response = JsonRpcResponse {
                jsonrpc: "2.0".to_string(),
                id: Some(serde_json::json!(1)),
                result: None,
                error: Some(JsonRpcError {
                    code: -32601,
                    message: "Method not found".to_string(),
                    data: None,
                }),
            };

            let json = serde_json::to_string(&response).unwrap();
            assert!(json.contains("\"error\""));
            assert!(json.contains("-32601"));
            assert!(json.contains("Method not found"));
            assert!(!json.contains("\"result\"")); // Should skip None
        }

        #[test]
        fn test_jsonrpc_error_serialization() {
            let error = JsonRpcError {
                code: -32602,
                message: "Invalid params".to_string(),
                data: Some(serde_json::json!({"field": "name"})),
            };

            let json = serde_json::to_string(&error).unwrap();
            assert!(json.contains("-32602"));
            assert!(json.contains("Invalid params"));
            assert!(json.contains("\"data\""));
        }

        #[test]
        fn test_jsonrpc_error_without_data() {
            let error = JsonRpcError {
                code: -32603,
                message: "Internal error".to_string(),
                data: None,
            };

            let json = serde_json::to_string(&error).unwrap();
            assert!(!json.contains("\"data\"")); // Should skip None
        }

        #[test]
        fn test_tool_request_deserialization() {
            let json = r#"{
                "tool": "session",
                "input": {"action": "init"}
            }"#;

            let request: ToolRequest = serde_json::from_str(json).unwrap();
            assert_eq!(request.tool, "session");
            assert_eq!(request.input, serde_json::json!({"action": "init"}));
        }

        #[test]
        fn test_tool_request_without_input() {
            let json = r#"{"tool": "help"}"#;

            let request: ToolRequest = serde_json::from_str(json).unwrap();
            assert_eq!(request.tool, "help");
            // #[serde(default)] defaults to Value::Null, not {}
            assert_eq!(request.input, serde_json::Value::Null);
        }

        #[test]
        fn test_list_params_deserialization() {
            let params: ListParams = serde_json::from_str(
                r#"{
                "category": "session",
                "format": "full"
            }"#,
            )
            .unwrap();

            assert_eq!(params.category, Some("session".to_string()));
            assert_eq!(params.format, Some("full".to_string()));
        }

        #[test]
        fn test_list_params_empty() {
            let params: ListParams = serde_json::from_str("{}").unwrap();

            assert!(params.category.is_none());
            assert!(params.format.is_none());
        }

        #[test]
        fn test_apply_default_context_mode_sets_mode_for_context_calls() {
            let mut request = JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: Some(json!(1)),
                method: "tools/call".to_string(),
                params: json!({
                    "name": "context",
                    "arguments": {
                        "user_message": "hello"
                    }
                }),
            };

            apply_default_context_mode(&mut request, Some("fast"));

            assert_eq!(request.params["arguments"]["mode"], json!("fast"));
        }

        #[test]
        fn test_apply_default_context_mode_preserves_explicit_mode() {
            let mut request = JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: Some(json!(1)),
                method: "tools/call".to_string(),
                params: json!({
                    "name": "context",
                    "arguments": {
                        "user_message": "hello",
                        "mode": "deep"
                    }
                }),
            };

            apply_default_context_mode(&mut request, Some("fast"));

            assert_eq!(request.params["arguments"]["mode"], json!("deep"));
        }

        #[test]
        fn test_apply_default_context_mode_ignores_non_context_calls() {
            let mut request = JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: Some(json!(1)),
                method: "tools/call".to_string(),
                params: json!({
                    "name": "search",
                    "arguments": {
                        "query": "auth"
                    }
                }),
            };

            apply_default_context_mode(&mut request, Some("fast"));

            assert!(request.params["arguments"].get("mode").is_none());
        }
    }

    // ========================================================================
    // JSON-RPC Error Code Tests
    // ========================================================================

    mod error_code_tests {
        #[test]
        fn test_standard_error_codes() {
            // Document standard JSON-RPC error codes used
            // -32600: Invalid Request
            // -32601: Method not found
            // -32602: Invalid params
            // -32603: Internal error

            assert_eq!(-32601, -32601); // Method not found
            assert_eq!(-32602, -32602); // Invalid params
            assert_eq!(-32603, -32603); // Internal error
        }
    }

    // ========================================================================
    // Auth Tests
    // ========================================================================

    mod auth_tests {
        use super::*;
        use crate::server::build_registry;
        use mcp_client::ContextStreamClient;
        use mcp_session::SessionManager;
        use mcp_types::config::Config;

        fn create_auth_state(require_auth: bool, jwt_secret: Option<&str>) -> HttpState {
            let config = Config::default();
            let client = ContextStreamClient::new(config.clone());
            let session = Arc::new(SessionManager::new(client.clone(), config.clone()));
            let registry = build_registry(&config, client.clone(), session.clone());

            HttpState {
                registry: Arc::new(registry),
                client: client.clone(),
                session: session.clone(),
                jwt_secret: jwt_secret.map(str::to_string),
                require_auth,
                telemetry: AgenticTelemetry::new(client, session),
                tools_list_cache: Arc::new(RwLock::new(HashMap::new())),
                concurrency_semaphore: Arc::new(tokio::sync::Semaphore::new(8)),
                metrics_handle: None,
            }
        }

        #[test]
        fn traffic_class_extraction_is_authenticated_and_fail_closed() {
            let mut headers = HeaderMap::new();
            headers.insert(header::AUTHORIZATION, "Bearer probe-key".parse().unwrap());
            headers.insert(
                TrafficClass::HEADER_NAME,
                TrafficClass::SYNTHETIC_PROBE_VALUE.parse().unwrap(),
            );
            let auth = extract_auth_override(&headers).expect("authenticated override");
            assert_eq!(auth.traffic_class, Some(TrafficClass::SyntheticProbe));

            for rejected in ["customer", "Synthetic-Probe", "synthetic-probe,customer"] {
                headers.insert(TrafficClass::HEADER_NAME, rejected.parse().unwrap());
                let auth = extract_auth_override(&headers).expect("authenticated override");
                assert_eq!(auth.traffic_class, None);
            }

            headers.remove(header::AUTHORIZATION);
            headers.insert(
                TrafficClass::HEADER_NAME,
                TrafficClass::SYNTHETIC_PROBE_VALUE.parse().unwrap(),
            );
            assert!(
                extract_auth_override(&headers).is_none(),
                "an unauthenticated classification header must not create an override"
            );
        }

        #[test]
        fn anonymous_http_sessions_are_partitioned_and_non_cacheable() {
            let (session_a, effective_a) =
                derive_http_session_context(None, None, Some("mcp-session-a"), "unused-a");
            let (session_a_repeat, effective_a_repeat) =
                derive_http_session_context(None, None, Some("mcp-session-a"), "unused-b");
            let (session_b, effective_b) =
                derive_http_session_context(None, None, Some("mcp-session-b"), "unused-c");

            assert_eq!(session_a, session_a_repeat);
            assert_eq!(effective_a.as_deref(), Some("mcp-session-a"));
            assert_eq!(effective_a, effective_a_repeat);
            assert_ne!(session_a, session_b);
            assert_eq!(effective_b.as_deref(), Some("mcp-session-b"));
            assert!(matches!(session_a, SessionKey::AnonymousHttp(_)));
            assert_eq!(session_a.atlas_user_scope_token(), None);
        }

        #[test]
        fn authenticated_cache_identity_is_stable_across_mcp_sessions() {
            let api_key = "api-key-that-must-not-enter-cache-identities";
            let (session_a, _) =
                derive_http_session_context(None, Some(api_key), Some("session-a"), "unused-a");
            let (session_b, _) =
                derive_http_session_context(None, Some(api_key), Some("session-b"), "unused-b");
            assert_ne!(
                session_a, session_b,
                "mutable session state must remain isolated"
            );

            let caller_a = derive_http_caller_cache_identity(None, Some(api_key))
                .expect("authenticated cache identity");
            let caller_b = derive_http_caller_cache_identity(None, Some(api_key))
                .expect("authenticated cache identity");
            assert_eq!(caller_a, caller_b);
            assert!(!caller_a.contains(api_key));

            let other = derive_http_caller_cache_identity(None, Some("different-api-key"))
                .expect("other authenticated cache identity");
            assert_ne!(caller_a, other);

            let jwt_subject = "jwt-subject-that-must-not-enter-cache-identities";
            let jwt = derive_http_caller_cache_identity(Some(jwt_subject), Some(api_key))
                .expect("JWT caller identity");
            assert!(!jwt.contains(jwt_subject));
            assert_ne!(
                jwt, caller_a,
                "credential kinds must remain domain-separated"
            );
            assert!(derive_http_caller_cache_identity(None, None).is_none());
        }

        #[test]
        fn anonymous_http_without_session_header_is_unique_per_request() {
            let (first, first_effective) =
                derive_http_session_context(None, None, None, "request-nonce-a");
            let (second, second_effective) =
                derive_http_session_context(None, None, None, "request-nonce-b");

            assert_ne!(first, second);
            assert_ne!(first_effective, second_effective);
            assert_eq!(
                first_effective.as_deref(),
                Some("anonymous-request:request-nonce-a")
            );
            assert_eq!(
                second_effective.as_deref(),
                Some("anonymous-request:request-nonce-b")
            );
            assert!(matches!(first, SessionKey::AnonymousHttp(_)));
            assert!(matches!(second, SessionKey::AnonymousHttp(_)));
        }

        #[test]
        fn authenticated_http_partition_does_not_retain_raw_identity() {
            let subject = "jwt-subject-sensitive";
            let api_key = "api-key-sensitive";
            let session_id = "mcp-session-sensitive";
            let (jwt, _) =
                derive_http_session_context(Some(subject), None, Some(session_id), "unused");
            let (api, _) =
                derive_http_session_context(None, Some(api_key), Some(session_id), "unused");

            let jwt_debug = format!("{jwt:?}");
            let api_debug = format!("{api:?}");
            assert!(!jwt_debug.contains(subject));
            assert!(!jwt_debug.contains(session_id));
            assert!(!api_debug.contains(api_key));
            assert!(!api_debug.contains(session_id));
        }

        #[test]
        fn test_extract_config_override_from_headers() {
            let mut headers = HeaderMap::new();
            headers.insert(
                "x-contextstream-context-pack-enabled",
                "false".parse().unwrap(),
            );
            headers.insert(
                "x-contextstream-transcripts-enabled",
                "false".parse().unwrap(),
            );
            headers.insert(
                "x-contextstream-auto-hide-integrations",
                "false".parse().unwrap(),
            );
            headers.insert("x-contextstream-search-limit", "25".parse().unwrap());
            headers.insert(
                "x-contextstream-tool-surface-profile",
                "openai_agentic".parse().unwrap(),
            );
            headers.insert(
                "x-contextstream-acceleration-enabled",
                "false".parse().unwrap(),
            );
            headers.insert("x-contextstream-atlas-enabled", "true".parse().unwrap());

            let override_config = extract_config_override(&headers).expect("config override");
            assert_eq!(override_config.context_pack_enabled, Some(false));
            assert_eq!(override_config.transcripts_enabled, Some(false));
            assert_eq!(override_config.auto_hide_integrations, Some(false));
            assert_eq!(override_config.search_limit, Some(25));
            assert_eq!(
                override_config.tool_surface_profile,
                Some(ToolSurfaceProfile::OpenaiAgentic)
            );
            assert_eq!(override_config.acceleration_enabled, Some(false));
            assert_eq!(override_config.atlas_enabled, Some(true));
            assert_eq!(
                override_config.effective_acceleration_enabled(),
                Some(false),
                "new acceleration header must take precedence over deprecated Atlas alias"
            );
        }

        #[test]
        fn test_extract_config_override_accepts_atlas_header_alias() {
            let mut headers = HeaderMap::new();
            headers.insert("x-contextstream-atlas-enabled", "false".parse().unwrap());

            let override_config = extract_config_override(&headers).expect("config override");
            assert_eq!(override_config.acceleration_enabled, None);
            assert_eq!(override_config.atlas_enabled, Some(false));
            assert_eq!(
                override_config.effective_acceleration_enabled(),
                Some(false)
            );
        }

        #[test]
        fn test_request_origin_prefers_https_for_public_hosts() {
            let mut headers = HeaderMap::new();
            headers.insert("host", "mcp.contextstream.io".parse().unwrap());
            headers.insert("x-forwarded-proto", "http".parse().unwrap());

            assert_eq!(request_origin(&headers), "https://mcp.contextstream.io");
        }

        #[test]
        fn test_authorization_server_origin_uses_public_fallback_for_private_api_url() {
            let _env_guard = crate::env_test_mutex()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let previous_api_url = std::env::var_os("CONTEXTSTREAM_API_URL");
            let previous_public_api_url = std::env::var_os("CONTEXTSTREAM_PUBLIC_API_URL");

            std::env::set_var("CONTEXTSTREAM_API_URL", "http://127.0.0.1:8080");
            std::env::remove_var("CONTEXTSTREAM_PUBLIC_API_URL");

            assert_eq!(
                authorization_server_origin(),
                "https://api.contextstream.io"
            );

            if let Some(value) = previous_api_url {
                std::env::set_var("CONTEXTSTREAM_API_URL", value);
            } else {
                std::env::remove_var("CONTEXTSTREAM_API_URL");
            }
            if let Some(value) = previous_public_api_url {
                std::env::set_var("CONTEXTSTREAM_PUBLIC_API_URL", value);
            } else {
                std::env::remove_var("CONTEXTSTREAM_PUBLIC_API_URL");
            }
        }

        #[test]
        fn test_request_origin_prefers_configured_public_origin() {
            let _env_guard = crate::env_test_mutex()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let previous_public_mcp_origin = std::env::var_os("CONTEXTSTREAM_PUBLIC_MCP_ORIGIN");
            std::env::set_var(
                "CONTEXTSTREAM_PUBLIC_MCP_ORIGIN",
                "https://mcp.contextstream.io",
            );

            let mut headers = HeaderMap::new();
            headers.insert("host", "internal.example".parse().unwrap());
            headers.insert("x-forwarded-proto", "http".parse().unwrap());

            assert_eq!(request_origin(&headers), "https://mcp.contextstream.io");

            if let Some(value) = previous_public_mcp_origin {
                std::env::set_var("CONTEXTSTREAM_PUBLIC_MCP_ORIGIN", value);
            } else {
                std::env::remove_var("CONTEXTSTREAM_PUBLIC_MCP_ORIGIN");
            }
        }

        #[tokio::test]
        async fn test_missing_auth_returns_oauth_challenge() {
            let app = create_router(create_auth_state(true, None));

            let request = Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("content-type", "application/json")
                .header("accept", "application/json, text/event-stream")
                .body(Body::from("{}"))
                .unwrap();

            let response = app.oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(
                response
                    .headers()
                    .get(header::WWW_AUTHENTICATE)
                    .and_then(|value| value.to_str().ok()),
                Some(
                    r#"Bearer resource_metadata="https://mcp.contextstream.io/.well-known/oauth-protected-resource""#
                )
            );

            let body = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(json["error"], "unauthorized");
            assert_eq!(json["error_description"], "Bearer token required");
            assert_eq!(
                json["_meta"]["mcp/www_authenticate"]["error_description"],
                "No access token provided"
            );
        }

        #[tokio::test]
        async fn test_invalid_jwt_returns_oauth_challenge() {
            let app = create_router(create_auth_state(false, Some("test-secret")));

            let request = Request::builder()
                .method("POST")
                .uri("/mcp")
                .header("content-type", "application/json")
                .header("accept", "application/json, text/event-stream")
                .header("authorization", "Bearer invalid.jwt.token")
                .body(Body::from("{}"))
                .unwrap();

            let response = app.oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(
                response
                    .headers()
                    .get(header::WWW_AUTHENTICATE)
                    .and_then(|value| value.to_str().ok()),
                Some(
                    r#"Bearer resource_metadata="https://mcp.contextstream.io/.well-known/oauth-protected-resource""#
                )
            );

            let body = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(json["error"], "unauthorized");
            assert_eq!(json["error_description"], "Bearer token invalid or expired");
            assert_eq!(
                json["_meta"]["mcp/www_authenticate"]["error_description"],
                "Invalid or expired access token"
            );
        }

        #[tokio::test]
        async fn test_oauth_protected_resource_metadata_matches_mcp_origin() {
            let app = create_router(create_auth_state(true, None));

            let request = Request::builder()
                .method("GET")
                .uri("/.well-known/oauth-protected-resource")
                .header("host", "mcp.contextstream.io")
                .header("x-forwarded-proto", "https")
                .body(Body::empty())
                .unwrap();

            let response = app.oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);

            let body = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(json["resource"], "https://mcp.contextstream.io");
            assert_eq!(
                json["authorization_servers"],
                json!(["https://api.contextstream.io"])
            );
        }
    }

    // ========================================================================
    // Router Tests
    // ========================================================================

    mod router_tests {
        use super::*;
        use crate::server::build_registry;
        use mcp_client::{run_with_config_override, ContextStreamClient};
        use mcp_types::config::{Config, ConfigOverride, ToolSurfaceProfile};

        fn create_test_state() -> HttpState {
            let config = Config::default();
            let client = ContextStreamClient::new(config.clone());
            let session = Arc::new(mcp_session::SessionManager::new(
                client.clone(),
                config.clone(),
            ));
            let registry = build_registry(&config, client.clone(), session.clone());
            HttpState {
                registry: Arc::new(registry),
                client: client.clone(),
                session: session.clone(),
                jwt_secret: None,
                require_auth: false,
                telemetry: AgenticTelemetry::new(client, session),
                tools_list_cache: Arc::new(RwLock::new(HashMap::new())),
                concurrency_semaphore: Arc::new(tokio::sync::Semaphore::new(8)),
                metrics_handle: None,
            }
        }

        fn create_router_mode_state() -> HttpState {
            let mut config = Config::default();
            config.router_mode = true;
            config.api_key = Some("test-api-key".to_string());

            let client = ContextStreamClient::new(config.clone());
            let session = Arc::new(mcp_session::SessionManager::new(
                client.clone(),
                config.clone(),
            ));
            let registry = build_registry(&config, client.clone(), session.clone());

            HttpState {
                registry: Arc::new(registry),
                client: client.clone(),
                session: session.clone(),
                jwt_secret: None,
                require_auth: false,
                telemetry: AgenticTelemetry::new(client, session),
                tools_list_cache: Arc::new(RwLock::new(HashMap::new())),
                concurrency_semaphore: Arc::new(tokio::sync::Semaphore::new(8)),
                metrics_handle: None,
            }
        }

        fn create_openai_agentic_state() -> HttpState {
            let mut config = Config::default();
            config.tool_surface_profile = mcp_types::config::ToolSurfaceProfile::OpenaiAgentic;
            config.api_key = Some("test-api-key".to_string());

            let client = ContextStreamClient::new(config.clone());
            let session = Arc::new(mcp_session::SessionManager::new(
                client.clone(),
                config.clone(),
            ));
            let registry = build_registry(&config, client.clone(), session.clone());

            HttpState {
                registry: Arc::new(registry),
                client: client.clone(),
                session: session.clone(),
                jwt_secret: None,
                require_auth: false,
                telemetry: AgenticTelemetry::new(client, session),
                tools_list_cache: Arc::new(RwLock::new(HashMap::new())),
                concurrency_semaphore: Arc::new(tokio::sync::Semaphore::new(8)),
                metrics_handle: None,
            }
        }

        async fn assert_tools_list_transport_parity(state: HttpState) {
            let expected = contextstream_tools_list(&state.registry, None);
            let app = create_router(state);

            let rest_response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/api/v1/tools/list")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(rest_response.status(), StatusCode::OK);
            let rest_bytes = rest_response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes();
            let rest_json: Value = serde_json::from_slice(&rest_bytes).unwrap();

            let rpc_body = json!({
                "jsonrpc": "2.0",
                "id": "tools-list-parity",
                "method": "tools/list",
                "params": {}
            });
            let rpc_response = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/v1/rpc")
                        .header("content-type", "application/json")
                        .body(Body::from(serde_json::to_vec(&rpc_body).unwrap()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(rpc_response.status(), StatusCode::OK);
            let rpc_bytes = rpc_response.into_body().collect().await.unwrap().to_bytes();
            let rpc_json: Value = serde_json::from_slice(&rpc_bytes).unwrap();

            assert_eq!(rest_json["tools"], rpc_json["result"]["tools"]);
            assert_eq!(rest_json["tools"], Value::Array(expected));
        }

        #[tokio::test]
        async fn broad_router_and_openai_tool_lists_are_transport_identical() {
            for state in [
                create_test_state(),
                create_router_mode_state(),
                create_openai_agentic_state(),
            ] {
                assert_tools_list_transport_parity(state).await;
            }
        }

        #[test]
        fn test_create_router() {
            let state = create_test_state();
            let _router = create_router(state);
            // Router created successfully
        }

        #[tokio::test]
        async fn test_health_endpoint() {
            let state = create_test_state();
            let app = create_router(state);

            let request = Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap();

            let response = app.oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);

            let body = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

            assert_eq!(json["status"], "ok");
            assert_eq!(json["server"], "contextstream-mcp");
            assert!(json["version"].is_string());
        }

        #[tokio::test]
        async fn test_api_health_endpoint() {
            let state = create_test_state();
            let app = create_router(state);

            let request = Request::builder()
                .uri("/api/v1/health")
                .body(Body::empty())
                .unwrap();

            let response = app.oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }

        #[tokio::test]
        async fn test_initialize_endpoint() {
            let state = create_test_state();
            let app = create_router(state);

            let request = Request::builder()
                .method("POST")
                .uri("/api/v1/initialize")
                .header("content-type", "application/json")
                .body(Body::empty())
                .unwrap();

            let response = app.oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);

            let body = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

            assert_eq!(json["protocolVersion"], "2024-11-05");
            assert!(json["capabilities"].is_object());
            assert!(json["serverInfo"].is_object());
            assert!(json.get("instructions").is_none());
        }

        #[tokio::test]
        async fn test_list_tools_endpoint() {
            let state = create_test_state();
            let app = create_router(state);

            let request = Request::builder()
                .uri("/api/v1/tools/list")
                .body(Body::empty())
                .unwrap();

            let response = app.oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);

            let body = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

            assert!(json["tools"].is_array());
        }

        #[tokio::test]
        async fn test_router_mode_list_tools_includes_meta_tools() {
            let state = create_router_mode_state();
            let app = create_router(state);

            let request = Request::builder()
                .uri("/api/v1/tools/list")
                .body(Body::empty())
                .unwrap();

            let response = app.oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);

            let body = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let names: Vec<&str> = json["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|tool| tool.get("name").and_then(|name| name.as_str()))
                .collect();

            assert!(names.contains(&"operations"));
            assert!(names.contains(&"execute_operation"));
        }

        #[tokio::test]
        async fn test_openai_agentic_list_tools_includes_discovery_and_hides_long_tail() {
            let state = create_openai_agentic_state();
            let app = create_router(state);

            let request = Request::builder()
                .uri("/api/v1/tools/list")
                .body(Body::empty())
                .unwrap();

            let response = app.oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);

            let body = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let names: Vec<&str> = json["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|tool| tool.get("name").and_then(|name| name.as_str()))
                .collect();

            assert!(names.contains(&"help"));
            assert!(names.contains(&"search"));
            assert!(names.contains(&"tool_search"));
            assert!(names.contains(&"execute_operation"));
            assert!(names.contains(&"batch_operations"));
            assert!(!names.contains(&"integration"));
        }

        #[tokio::test]
        async fn test_list_tools_cache_keys_by_effective_surface() {
            let state = create_test_state();

            assert_eq!(
                state
                    .tools_list_cache
                    .read()
                    .unwrap_or_else(|err| err.into_inner())
                    .len(),
                0
            );

            let default_response = handle_list_tools_method(&state, None)
                .await
                .expect("default tools list");
            let cache_len_after_default = state
                .tools_list_cache
                .read()
                .unwrap_or_else(|err| err.into_inner())
                .len();
            assert_eq!(cache_len_after_default, 1);

            let default_response_cached = handle_list_tools_method(&state, None)
                .await
                .expect("cached default tools list");
            assert_eq!(default_response, default_response_cached);
            assert_eq!(
                state
                    .tools_list_cache
                    .read()
                    .unwrap_or_else(|err| err.into_inner())
                    .len(),
                1
            );

            let agentic_response = run_with_config_override(
                ConfigOverride {
                    tool_surface_profile: Some(ToolSurfaceProfile::OpenaiAgentic),
                    ..ConfigOverride::default()
                },
                || async { handle_list_tools_method(&state, None).await },
            )
            .await
            .expect("agentic tools list");
            assert_ne!(default_response, agentic_response);
            assert_eq!(
                state
                    .tools_list_cache
                    .read()
                    .unwrap_or_else(|err| err.into_inner())
                    .len(),
                2
            );
        }

        #[tokio::test]
        async fn test_remote_tool_surface_override_exposes_agentic_meta_tools() {
            let state = create_test_state();
            let app = create_router(state);

            let request = Request::builder()
                .uri("/api/v1/tools/list")
                .header("x-contextstream-tool-surface-profile", "openai_agentic")
                .body(Body::empty())
                .unwrap();

            let response = app.oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);

            let body = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let names: Vec<&str> = json["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|tool| tool.get("name").and_then(|name| name.as_str()))
                .collect();

            assert!(names.contains(&"tool_search"));
            assert!(names.contains(&"execute_operation"));
            assert!(names.contains(&"batch_operations"));
        }

        #[tokio::test]
        async fn test_jsonrpc_initialize() {
            let state = create_test_state();
            let app = create_router(state);

            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {}
            });

            let request = Request::builder()
                .method("POST")
                .uri("/api/v1/rpc")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap();

            let response = app.oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);

            let body = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

            assert_eq!(json["jsonrpc"], "2.0");
            assert_eq!(json["id"], 1);
            assert!(json["result"].is_object());
            assert!(json["error"].is_null());
        }

        #[tokio::test]
        async fn test_jsonrpc_tools_list() {
            let state = create_test_state();
            let app = create_router(state);

            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/list",
                "params": {}
            });

            let request = Request::builder()
                .method("POST")
                .uri("/api/v1/rpc")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap();

            let response = app.oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);

            let body = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

            assert_eq!(json["jsonrpc"], "2.0");
            assert!(json["result"]["tools"].is_array());
        }

        #[tokio::test]
        async fn test_router_mode_jsonrpc_tools_list_includes_meta_tools() {
            let state = create_router_mode_state();
            let app = create_router(state);

            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 22,
                "method": "tools/list",
                "params": {}
            });

            let request = Request::builder()
                .method("POST")
                .uri("/api/v1/rpc")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap();

            let response = app.oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);

            let body = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            let names: Vec<&str> = json["result"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|tool| tool.get("name").and_then(|name| name.as_str()))
                .collect();

            assert!(names.contains(&"operations"));
            assert!(names.contains(&"execute_operation"));
        }

        #[tokio::test]
        async fn http_jsonrpc_and_legacy_rest_batch_reject_wire_budgeted_operations() {
            let batch_arguments = serde_json::json!({
                "operations": [{
                    "name": "search",
                    "arguments": {
                        "query": "auth middleware",
                        "tokenizer": "o200k_base"
                    }
                }]
            });

            let rpc_app = create_router(create_openai_agentic_state());
            let rpc_body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": "batch-wire-budget",
                "method": "tools/call",
                "params": {
                    "name": "batch_operations",
                    "arguments": batch_arguments.clone()
                }
            });
            let rpc_response = rpc_app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/v1/rpc")
                        .header("content-type", "application/json")
                        .body(Body::from(serde_json::to_vec(&rpc_body).unwrap()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(rpc_response.status(), StatusCode::OK);
            let rpc_bytes = rpc_response.into_body().collect().await.unwrap().to_bytes();
            let rpc_json: Value = serde_json::from_slice(&rpc_bytes).unwrap();
            assert_eq!(rpc_json["result"]["isError"], true);
            assert!(rpc_json["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("direct tool call"));

            let rest_app = create_router(create_openai_agentic_state());
            let rest_response = rest_app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/api/v1/tools/batch_operations")
                        .header("content-type", "application/json")
                        .body(Body::from(serde_json::to_vec(&batch_arguments).unwrap()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(rest_response.status(), StatusCode::OK);
            let rest_bytes = rest_response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes();
            let rest_json: Value = serde_json::from_slice(&rest_bytes).unwrap();
            assert_eq!(rest_json["isError"], true);
            assert!(rest_json["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("whole-wire token budget"));
        }

        #[tokio::test]
        async fn test_jsonrpc_method_not_found() {
            let state = create_test_state();
            let app = create_router(state);

            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": "unknown/method",
                "params": {}
            });

            let request = Request::builder()
                .method("POST")
                .uri("/api/v1/rpc")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap();

            let response = app.oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);

            let body = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

            assert_eq!(json["jsonrpc"], "2.0");
            assert!(json["result"].is_null());
            assert_eq!(json["error"]["code"], -32601);
            assert!(json["error"]["message"]
                .as_str()
                .unwrap()
                .contains("Method not found"));
        }

        #[tokio::test]
        async fn test_jsonrpc_notification() {
            let state = create_test_state();
            let app = create_router(state);

            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 4,
                "method": "notifications/progress",
                "params": {"progress": 50}
            });

            let request = Request::builder()
                .method("POST")
                .uri("/api/v1/rpc")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap();

            let response = app.oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);

            let body = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

            // Notifications return empty result
            assert_eq!(json["result"], serde_json::json!({}));
        }

        #[tokio::test]
        async fn test_jsonrpc_tools_call_missing_name() {
            let state = create_test_state();
            let app = create_router(state);

            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 5,
                "method": "tools/call",
                "params": {
                    "arguments": {}
                }
            });

            let request = Request::builder()
                .method("POST")
                .uri("/api/v1/rpc")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap();

            let response = app.oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);

            let body = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

            assert_eq!(json["error"]["code"], -32602);
            assert!(json["error"]["message"].as_str().unwrap().contains("name"));
        }

        #[tokio::test]
        async fn test_call_tool_not_found() {
            let state = create_test_state();
            let app = create_router(state);

            let body = serde_json::json!({
                "tool": "nonexistent_tool",
                "input": {}
            });

            let request = Request::builder()
                .method("POST")
                .uri("/api/v1/tools/call")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap();

            let response = app.oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);

            let body = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

            assert_eq!(json["isError"], true);
        }

        #[tokio::test]
        async fn test_router_mode_execute_operation_rest_dispatches() {
            let state = create_router_mode_state();
            let app = create_router(state);

            let body = serde_json::json!({
                "tool": "execute_operation",
                "input": {
                    "name": "help",
                    "arguments": {
                        "action": "workflow",
                        "client_name": "codex-cli/1.2.3"
                    }
                }
            });

            let request = Request::builder()
                .method("POST")
                .uri("/api/v1/tools/call")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap();

            let response = app.oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);

            let body = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(json["isError"], false);
            assert!(json["content"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["type"] == "text"));
            assert_eq!(
                json["structured"]["teaching_version"],
                mcp_types::HARNESS_TEACHING_VERSION
            );
            assert!(
                json.get("structuredContent").is_none(),
                "legacy REST keeps its compatibility field"
            );
        }

        #[tokio::test]
        async fn test_router_mode_execute_operation_jsonrpc_dispatches() {
            let state = create_router_mode_state();
            let app = create_router(state);

            let body = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 33,
                "method": "tools/call",
                "params": {
                    "name": "execute_operation",
                    "arguments": {
                        "name": "help",
                        "arguments": {
                            "action": "workflow",
                            "client_name": "codex-cli/1.2.3"
                        }
                    }
                }
            });

            let request = Request::builder()
                .method("POST")
                .uri("/api/v1/rpc")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap();

            let response = app.oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);

            let body = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(json["error"], serde_json::Value::Null);
            assert_eq!(json["result"]["isError"], false);
            assert_eq!(
                json["result"]["structuredContent"]["teaching_version"],
                mcp_types::HARNESS_TEACHING_VERSION
            );
            assert!(
                json["result"].get("structured").is_none(),
                "MCP JSON-RPC uses the standard structuredContent field"
            );
        }

        #[tokio::test]
        async fn test_call_tool_by_name_not_found() {
            let state = create_test_state();
            let app = create_router(state);

            let request = Request::builder()
                .method("POST")
                .uri("/api/v1/tools/nonexistent")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap();

            let response = app.oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }

        #[tokio::test]
        async fn test_not_found_route() {
            let state = create_test_state();
            let app = create_router(state);

            let request = Request::builder()
                .uri("/nonexistent/path")
                .body(Body::empty())
                .unwrap();

            let response = app.oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
    }

    // ========================================================================
    // State Tests
    // ========================================================================

    mod state_tests {
        use super::*;
        use crate::server::build_registry;
        use mcp_client::ContextStreamClient;
        use mcp_session::SessionManager;
        use mcp_types::config::Config;

        #[test]
        fn test_http_state_clone() {
            let config = Config::default();
            let client = ContextStreamClient::new(config.clone());
            let session = Arc::new(SessionManager::new(client.clone(), config.clone()));
            let registry = build_registry(&config, client.clone(), session.clone());

            let state = HttpState {
                registry: Arc::new(registry),
                client: client.clone(),
                session: session.clone(),
                jwt_secret: Some("secret".to_string()),
                require_auth: false,
                telemetry: AgenticTelemetry::new(client, session),
                tools_list_cache: Arc::new(RwLock::new(HashMap::new())),
                concurrency_semaphore: Arc::new(tokio::sync::Semaphore::new(8)),
                metrics_handle: None,
            };

            let cloned = state.clone();
            assert!(cloned.jwt_secret.is_some());
            assert_eq!(cloned.jwt_secret, Some("secret".to_string()));
        }

        #[test]
        fn test_http_state_without_jwt() {
            let config = Config::default();
            let client = ContextStreamClient::new(config.clone());
            let session = Arc::new(SessionManager::new(client.clone(), config.clone()));
            let registry = build_registry(&config, client.clone(), session.clone());

            let state = HttpState {
                registry: Arc::new(registry),
                client: client.clone(),
                session: session.clone(),
                jwt_secret: None,
                require_auth: false,
                telemetry: AgenticTelemetry::new(client, session),
                tools_list_cache: Arc::new(RwLock::new(HashMap::new())),
                concurrency_semaphore: Arc::new(tokio::sync::Semaphore::new(8)),
                metrics_handle: None,
            };

            assert!(state.jwt_secret.is_none());
        }
    }

    // ========================================================================
    // Route Coverage Tests
    // ========================================================================

    mod coverage_tests {
        #[test]
        fn test_api_routes_coverage() {
            // Document all API routes:
            //
            // POST /api/v1/rpc - JSON-RPC endpoint (handles all MCP methods)
            // POST /api/v1/initialize - MCP initialize
            // GET  /api/v1/tools/list - List available tools
            // POST /api/v1/tools/call - Call a tool by name (in body)
            // POST /api/v1/tools/:name - Call a tool by name (in path)
            // GET  /api/v1/health - Health check
            // GET  /api/v1/stream - SSE streaming endpoint
            //
            // Also available at root:
            // GET  /health - Health check (alternative path)

            let routes = [
                ("POST", "/api/v1/rpc"),
                ("POST", "/api/v1/initialize"),
                ("GET", "/api/v1/tools/list"),
                ("POST", "/api/v1/tools/call"),
                ("POST", "/api/v1/tools/:name"),
                ("GET", "/api/v1/health"),
                ("GET", "/api/v1/stream"),
                ("GET", "/health"),
            ];
            assert_eq!(routes.len(), 8);
        }

        #[test]
        fn test_jsonrpc_methods_coverage() {
            // Document JSON-RPC methods handled at /api/v1/rpc:
            //
            // initialize - Server initialization
            // tools/list - List available tools
            // tools/call - Call a specific tool
            // notifications/* - Handled as no-op (returns empty result)
            // * (other) - Returns -32601 Method not found

            let methods = ["initialize", "tools/list", "tools/call", "notifications/*"];
            assert_eq!(methods.len(), 4);
        }

        #[test]
        fn test_middleware_layers() {
            // Document middleware layers applied:
            //
            // 1. CORS - Allow any origin, method, headers
            // 2. TraceLayer - HTTP request tracing
            // 3. CompressionLayer - Response compression
            // 4. (Optional) JWT auth middleware

            let layers = ["CorsLayer", "TraceLayer", "CompressionLayer"];
            assert_eq!(layers.len(), 3);
        }
    }

    mod surface_detection_tests {
        use super::*;
        use crate::server::build_registry;
        use mcp_types::config::{Config, ToolSurfaceProfile};

        // Regression coverage for the codex-fugu "unsupported call: <tool>"
        // bug. Codex/Fugu connect with a gpt-5* model but are ordinary
        // full-surface clients; the model name must NOT narrow them to the
        // compact adaptive OpenAI surface (which hides search/memory/project/
        // etc behind discovery meta-tools).

        #[test]
        fn model_gpt5_does_not_trigger_agentic_surface() {
            for model in ["gpt-5", "gpt-5.5", "gpt-5-codex", "gpt-5-codex-high"] {
                let params = serde_json::json!({
                    "clientInfo": { "name": "codex" },
                    "model": model,
                });
                assert_eq!(
                    surface_profile_from_initialize_params(&params),
                    None,
                    "model={model} must not auto-select the agentic surface",
                );
            }
        }

        #[test]
        fn plain_codex_client_does_not_trigger_agentic_surface() {
            let params = serde_json::json!({ "clientInfo": { "name": "codex" } });
            assert_eq!(surface_profile_from_initialize_params(&params), None);
        }

        #[test]
        fn explicit_profile_param_still_wins() {
            let params = serde_json::json!({
                "clientInfo": { "name": "codex" },
                "model": "gpt-5.5",
                "tool_surface_profile": "openai_agentic",
            });
            assert_eq!(
                surface_profile_from_initialize_params(&params),
                Some(ToolSurfaceProfile::OpenaiAgentic),
            );
        }

        #[test]
        fn chatgpt_and_openai_client_names_still_trigger_agentic_surface() {
            for name in [
                "chatgpt",
                "ChatGPT",
                "chatgpt-gateway-e2e",
                "openai-responses",
            ] {
                let params = serde_json::json!({ "clientInfo": { "name": name } });
                assert_eq!(
                    surface_profile_from_initialize_params(&params),
                    Some(ToolSurfaceProfile::OpenaiAgentic),
                    "client name={name} should still select the agentic surface",
                );
            }
        }

        // Regression coverage for global tool-surface state bleed: the hosted
        // gateway shares one long-lived Arc<ToolRegistry> across tenants. A
        // prior client's auto-detected narrowing must not persist into the
        // next client that carries no profile signal.
        #[test]
        fn apply_initialize_surface_profile_resets_to_default_when_undetected() {
            let config = Config::default();
            let client = ContextStreamClient::new(config.clone());
            let session = Arc::new(mcp_session::SessionManager::new(
                client.clone(),
                config.clone(),
            ));
            let registry = build_registry(&config, client, session);
            assert_eq!(registry.tool_surface_profile(), ToolSurfaceProfile::Default,);

            // A gpt-5/chatgpt client connects and narrows the shared registry.
            registry.apply_initialize_surface_profile(Some(ToolSurfaceProfile::OpenaiAgentic));
            assert_eq!(
                registry.tool_surface_profile(),
                ToolSurfaceProfile::OpenaiAgentic,
            );

            // A subsequent plain client (no detected profile) must reset to the
            // construction-time default rather than inherit the narrowing.
            registry.apply_initialize_surface_profile(None);
            assert_eq!(registry.tool_surface_profile(), ToolSurfaceProfile::Default,);
        }

        #[test]
        fn apply_initialize_surface_profile_preserves_configured_default() {
            // Copilot (and other env/header opt-ins) construct the registry
            // with the agentic surface as its baseline. An undetected
            // initialize must fall back to THAT default, not force Default.
            let mut config = Config::default();
            config.tool_surface_profile = ToolSurfaceProfile::OpenaiAgentic;
            let client = ContextStreamClient::new(config.clone());
            let session = Arc::new(mcp_session::SessionManager::new(
                client.clone(),
                config.clone(),
            ));
            let registry = build_registry(&config, client, session);

            registry.apply_initialize_surface_profile(None);
            assert_eq!(
                registry.tool_surface_profile(),
                ToolSurfaceProfile::OpenaiAgentic,
            );
        }
    }
}
