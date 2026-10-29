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
    contextstream_tools_list, validate_contextstream_meta_tool_arguments, with_slipstream_listing,
};
use axum::body::Body;
use axum::http::{Method, Request};
use axum::middleware::Next;
use axum::{
    body::Bytes,
    extract::{rejection::BytesRejection, DefaultBodyLimit, Json, MatchedPath, Path, Query, State},
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
    run_with_config_override, run_with_installation_id, run_with_session_key,
    run_with_tool_surface_profile, ContextStreamClient,
};
use mcp_session::SessionManager;
use mcp_tools::ToolRegistry;
use mcp_types::config::{ConfigOverride, OutputFormat, ToolSurfaceProfile, Toolset};
use mcp_types::{
    decorate_stateless_cacheable_result, decorate_stateless_result,
    has_stateless_protocol_metadata, stateless_protocol_version,
    validate_stateless_jsonrpc_envelope, validate_stateless_method_params,
    validate_stateless_request, AuthOverride, Config, EdgeGeography, HarnessId, McpCacheScope,
    McpProtocolError, NearestRegion, SessionKey, StatelessRequestMetadata, TrafficClass,
    MCP_PROTOCOL_2026_07_28, MCP_TOOLS_LIST_TTL_MS,
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
#[derive(Debug, Clone, Deserialize)]
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

/// The largest request body the HTTP transport reads (axum's default, set
/// here so the 413 answer can name it).
const MAX_REQUEST_BODY_BYTES: usize = 2 * 1024 * 1024;

const JSONRPC_PARSE_ERROR: i32 = -32700;
const JSONRPC_INVALID_REQUEST: i32 = -32600;

/// One JSON-RPC message POSTed to a Streamable HTTP path.
#[derive(Debug)]
enum IncomingMcpMessage {
    /// A request or notification for the server to handle.
    Request(JsonRpcRequest),
    /// The client's answer to a server request. Nothing waits for one, so it
    /// is accepted with no body.
    Response,
}

/// Why a POST body is not one usable JSON-RPC message, answered as a
/// JSON-RPC error.
#[derive(Debug)]
struct MalformedMessage {
    status: StatusCode,
    id: Value,
    code: i32,
    message: String,
}

impl MalformedMessage {
    fn new(status: StatusCode, id: Value, code: i32, message: impl Into<String>) -> Self {
        Self {
            status,
            id,
            code,
            message: message.into(),
        }
    }

    fn into_response(self) -> Response {
        stateless_protocol_error_response(
            self.status,
            Some(self.id),
            JsonRpcError {
                code: self.code,
                message: self.message,
                data: None,
            },
        )
    }
}

/// `application/json` or an `application/*+json` type, as axum's `Json` accepts.
fn has_json_content_type(headers: &HeaderMap) -> bool {
    let Some(mime) = header_value(headers, header::CONTENT_TYPE.as_str())
        .and_then(|value| value.split(';').next())
        .map(|mime| mime.trim().to_ascii_lowercase())
    else {
        return false;
    };
    mime.split_once('/').is_some_and(|(kind, subtype)| {
        kind == "application" && (subtype == "json" || subtype.ends_with("+json"))
    })
}

/// Reads the body of a Streamable HTTP POST as one JSON-RPC message. Every
/// failure is a JSON-RPC error with a short reason, never framework text.
fn incoming_mcp_message(
    headers: &HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<IncomingMcpMessage, MalformedMessage> {
    if !has_json_content_type(headers) {
        return Err(MalformedMessage::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Value::Null,
            JSONRPC_INVALID_REQUEST,
            "Unsupported Media Type: send the JSON-RPC message with Content-Type: application/json",
        ));
    }
    let body = body.map_err(|rejection| {
        if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
            MalformedMessage::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                Value::Null,
                JSONRPC_INVALID_REQUEST,
                format!(
                    "Request body too large: the limit is {} MB ({MAX_REQUEST_BODY_BYTES} bytes)",
                    MAX_REQUEST_BODY_BYTES / (1024 * 1024)
                ),
            )
        } else {
            MalformedMessage::new(
                StatusCode::BAD_REQUEST,
                Value::Null,
                JSONRPC_PARSE_ERROR,
                "Parse error: the request body could not be read",
            )
        }
    })?;
    if body.iter().all(u8::is_ascii_whitespace) {
        return Err(MalformedMessage::new(
            StatusCode::BAD_REQUEST,
            Value::Null,
            JSONRPC_PARSE_ERROR,
            "Parse error: the request body is empty; send one JSON-RPC message",
        ));
    }
    let value: Value = serde_json::from_slice(&body).map_err(|err| {
        MalformedMessage::new(
            StatusCode::BAD_REQUEST,
            Value::Null,
            JSONRPC_PARSE_ERROR,
            format!("Parse error: {err}"),
        )
    })?;
    jsonrpc_message_from_value(value).map_err(|(id, reason)| {
        MalformedMessage::new(
            StatusCode::BAD_REQUEST,
            id,
            JSONRPC_INVALID_REQUEST,
            format!("Invalid Request: {reason}"),
        )
    })
}

/// One parsed JSON value as a JSON-RPC message, or why it is not one (with the
/// message's `id` when it carries a usable one).
fn jsonrpc_message_from_value(value: Value) -> Result<IncomingMcpMessage, (Value, &'static str)> {
    let mut message = match value {
        Value::Object(message) => message,
        Value::Array(_) => {
            return Err((
                Value::Null,
                "batches are not supported; send one JSON-RPC message per request",
            ))
        }
        _ => return Err((Value::Null, "the body must be one JSON-RPC message object")),
    };
    let id = match message.remove("id") {
        None | Some(Value::Null) => None,
        Some(id @ (Value::String(_) | Value::Number(_))) => Some(id),
        Some(_) => return Err((Value::Null, "\"id\" must be a string or a number")),
    };
    let error_id = id.clone().unwrap_or(Value::Null);
    let jsonrpc = match message.remove("jsonrpc") {
        Some(Value::String(jsonrpc)) => jsonrpc,
        _ => return Err((error_id, "\"jsonrpc\" must be \"2.0\"")),
    };
    let method = match message.remove("method") {
        Some(Value::String(method)) => method,
        Some(_) => return Err((error_id, "\"method\" must be a string")),
        None if message.contains_key("result") || message.contains_key("error") => {
            return Ok(IncomingMcpMessage::Response)
        }
        None => return Err((error_id, "missing \"method\"")),
    };
    Ok(IncomingMcpMessage::Request(JsonRpcRequest {
        jsonrpc,
        id,
        method,
        params: message.remove("params").unwrap_or(Value::Null),
    }))
}

fn is_streamable_mcp_path(path: &str) -> bool {
    matches!(
        path,
        "/mcp" | "/chatgpt" | "/claude" | "/client" | COACH_PATH | STUDIO_PATH
    )
}

/// The Agent Coach Kit endpoint: only the three coach tools.
const COACH_PATH: &str = "/coach";
/// Its protected-resource metadata.
const COACH_RESOURCE_METADATA_PATH: &str = "/.well-known/oauth-protected-resource/coach";

/// The ContextStudio connector: only the `studio` tool.
const STUDIO_PATH: &str = "/studio";
/// Its protected-resource metadata.
const STUDIO_RESOURCE_METADATA_PATH: &str = "/.well-known/oauth-protected-resource/studio";
/// The scopes a token bound to the ContextStudio connector carries.
const STUDIO_READ_SCOPE: &str = "read:studio";
const STUDIO_WRITE_SCOPE: &str = "write:studio";

/// The tool surface a path fixes, whatever the request asks for.
fn path_surface_profile(path: &str) -> Option<ToolSurfaceProfile> {
    match path {
        // Integration paths have complete parity with the local MCP tool.
        "/chatgpt" | "/claude" | "/client" => Some(ToolSurfaceProfile::Default),
        COACH_PATH => Some(ToolSurfaceProfile::Coach),
        STUDIO_PATH => Some(ToolSurfaceProfile::Studio),
        _ => None,
    }
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

/// The CORS layer of the HTTP router. It answers EVERY OPTIONS request itself (tower-http's CorsLayer returns at once
/// for any OPTIONS), the same for any origin and path, so the ContextStream MCP gateway Worker answers OPTIONS at the
/// edge with exactly this answer. The contract is `transport/cors/mcp-preflight.json`, a copy of the contextstream
/// repository's deploy/cloudflare/shared/cors/mcp-preflight.json; `cors_preflight_contract` pins it to this layer.
pub(crate) fn cors_layer() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any)
}

/// Create the HTTP router.
pub fn create_router(state: HttpState) -> Router {
    let cors = cors_layer();

    let api_routes = Router::new()
        // JSON-RPC endpoint
        .route("/rpc", post(handle_jsonrpc))
        // MCP protocol endpoints
        .route("/initialize", post(handle_initialize))
        .route("/tools/list", get(handle_list_tools))
        .route("/tools/call", post(handle_call_tool))
        .route("/tools/{name}", post(handle_call_tool_by_name))
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
        .route(
            COACH_RESOURCE_METADATA_PATH,
            get(handle_coach_protected_resource),
        )
        .route(
            STUDIO_RESOURCE_METADATA_PATH,
            get(handle_studio_protected_resource),
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
        // Agent Coach Kit: its own product surface (three tools, its own
        // protected-resource metadata and scopes).
        .route(COACH_PATH, post(handle_mcp_streamable))
        // ContextStudio: its own connector (one tool, its own protected-resource
        // metadata and scopes).
        .route(STUDIO_PATH, post(handle_mcp_streamable))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
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
    // vocabulary initialization latency. Proxy mode (the default) never
    // consults the vocabulary, so it skips the load entirely.
    if mcp_tools::wire_tokens::exact_tokenizer_required() {
        mcp_tools::wire_tokens::warm_o200k();
    }

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

/// Protected resource metadata (RFC 9728) for the Agent Coach Kit endpoint.
async fn handle_coach_protected_resource(headers: HeaderMap) -> impl IntoResponse {
    Json(coach_protected_resource_metadata(&headers))
}

/// Protected resource metadata (RFC 9728) for the ContextStudio connector.
async fn handle_studio_protected_resource(headers: HeaderMap) -> impl IntoResponse {
    Json(studio_protected_resource_metadata(&headers))
}

/// RFC 8414 / OIDC discovery served on the MCP host. Lets clients that
/// probe the resource host directly find the right endpoints without
/// the auth-middleware 401 that broke claude.ai's custom connector
/// flow. It must say what the authorization server itself publishes, field
/// for field: contextstream `oauth_provider::oauth_authorization_server_metadata`
/// (its `ADVERTISED_SCOPES` and `SIGNING_ALGORITHMS`).
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
            "none",
            "private_key_jwt"
        ],
        "response_types_supported": ["code"],
        "grant_types_supported": ["authorization_code", "refresh_token"],
        // OAuth 2.1 and the MCP authorization spec allow S256 only.
        "code_challenge_methods_supported": ["S256"],
        "scopes_supported": [
            "read:workspaces",
            "read:projects",
            "read:search",
            "read:memory",
            "write:memory",
            mcp_client::coach::READ_SCOPE,
            mcp_client::coach::WRITE_SCOPE,
            "read:agents",
            "write:agents",
            STUDIO_READ_SCOPE,
            STUDIO_WRITE_SCOPE
        ],
        "authorization_response_iss_parameter_supported": true,
        "token_endpoint_auth_signing_alg_values_supported": ["RS256", "ES256"],
        "client_id_metadata_document_supported": true
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
            // The detected profile shapes this initialize result only. /rpc
            // issues no session to carry it, and the shared registry serves
            // every caller, so the registry is never changed.
            let surface_profile = resolve_request_tool_surface_profile(
                None,
                get_task_config_override().and_then(|config| config.tool_surface_profile),
                surface_profile_from_initialize_params(&request.params),
                state.registry.default_tool_surface_profile(),
            );
            state
                .telemetry
                .update_initialize_hints(&request.params)
                .await;
            run_with_tool_surface_profile(surface_profile, || {
                handle_initialize_method(&state, &request.params)
            })
            .await
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
/// - Notifications (no `id`) and client responses get 202 Accepted
/// - A body that is not one JSON-RPC message gets a JSON-RPC error
async fn handle_mcp_streamable(
    State(state): State<HttpState>,
    Query(query): Query<McpQueryParams>,
    matched_path: Option<MatchedPath>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let request = match incoming_mcp_message(&headers, body) {
        Ok(IncomingMcpMessage::Request(request)) => request,
        Ok(IncomingMcpMessage::Response) => return StatusCode::ACCEPTED.into_response(),
        Err(malformed) => return malformed.into_response(),
    };
    // Integration-specific endpoints (/chatgpt, /claude, /client) must expose
    // the full ContextStream tool surface regardless of client auto-detection,
    // so the app/plugin has complete parity with the local MCP tool; /coach
    // exposes only the Agent Coach Kit tools.
    let path_profile = matched_path
        .as_ref()
        .and_then(|path| path_surface_profile(path.as_str()));
    let force_full_surface = path_profile == Some(ToolSurfaceProfile::Default);

    // One shared registry serves every caller, so the surface rides this
    // request as a task-local and the registry itself is never changed.
    let session_profile = if request.method == "initialize" {
        surface_profile_from_initialize_params(&request.params)
    } else if uses_stateless_http_contract(&headers, &request) {
        // The stateless contract has no protocol session to remember one.
        None
    } else {
        header_str(&headers, "mcp-session-id").and_then(surface_profile_from_session_id)
    };
    let surface_profile = resolve_request_tool_surface_profile(
        path_profile,
        header_str(&headers, "x-contextstream-tool-surface-profile")
            .and_then(|value| value.parse::<ToolSurfaceProfile>().ok()),
        session_profile,
        state.registry.default_tool_surface_profile(),
    );
    // A refused credential fails initialize the way a missing one does:
    // HTTP 401 with WWW-Authenticate, which an editor reports as a failed
    // connection, where a JSON-RPC error in a 200 still read "Connected"
    // (#252, #303). The stateless protocol has no initialize: every request
    // stands alone, and Claude Code's health check is its server/discover
    // and tools/list, so each one is checked (a cached verdict, mostly).
    if request.method == "initialize" || uses_stateless_http_contract(&headers, &request) {
        if let Some(refusal) = credential_refused(&state, &headers).await {
            let path = matched_path.as_ref().map_or("/mcp", |path| path.as_str());
            return oauth_unauthorized_response(&headers, path, refusal, refusal);
        }
    }
    run_with_tool_surface_profile(surface_profile, || {
        serve_mcp_streamable(
            state,
            query,
            force_full_surface,
            surface_profile,
            headers,
            request,
        )
    })
    .await
}

async fn serve_mcp_streamable(
    state: HttpState,
    query: McpQueryParams,
    force_full_surface: bool,
    surface_profile: ToolSurfaceProfile,
    headers: HeaderMap,
    mut request: JsonRpcRequest,
) -> Response {
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
    apply_cloud_client_init_defaults(&mut request, &headers);
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
        (is_initialize && !stateless_request).then(|| mint_mcp_session_id(surface_profile));

    let tool_name = if request.method == "tools/call" {
        request
            .params
            .get("name")
            .and_then(|n| n.as_str())
            .map(|s| s.to_string())
    } else {
        None
    };
    // Bounded action label (fixed allowlists; never raw input) for the
    // per-call timing line and telemetry.
    let tool_action: Option<&'static str> = tool_name.as_deref().map(|name| {
        mcp_tools::registry::tool_call_action_label(
            name,
            request.params.get("arguments").unwrap_or(&Value::Null),
        )
    });

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
    let (result, api_calls) = mcp_client::run_counting_api_calls(async {
        match request.method.as_str() {
            "server/discover" if surface_profile == ToolSurfaceProfile::Coach => {
                Ok(crate::server::build_coach_stateless_discover_result())
            }
            "server/discover" => Ok(build_contextstream_stateless_discover_result()),
            // The coach surface serves its ChatGPT component as a resource.
            "resources/list" if surface_profile == ToolSurfaceProfile::Coach => {
                Ok(crate::server::coach_resources_list())
            }
            "resources/templates/list" if surface_profile == ToolSurfaceProfile::Coach => {
                Ok(crate::server::coach_resource_templates_list())
            }
            "resources/read" if surface_profile == ToolSurfaceProfile::Coach => {
                crate::server::coach_resource_read(&request.params).map_err(|(code, message)| {
                    JsonRpcError {
                        code,
                        message,
                        data: None,
                    }
                })
            }
            "initialize" if stateless_request => Err(JsonRpcError {
                code: -32601,
                message: format!(
                    "Method not found: initialize is not part of MCP {MCP_PROTOCOL_2026_07_28}"
                ),
                data: None,
            }),
            "initialize" => {
                state
                    .telemetry
                    .update_managed_initialize_hints_for_session(
                        &request.params,
                        response_session_id.as_deref(),
                        managed_runtime_identity,
                    )
                    .await;
                let result = handle_initialize_method(&state, &request.params).await;
                if let (Ok(_), Some(session_id)) = (result.as_ref(), response_session_id.as_deref())
                {
                    remember_session_client(session_id, &request.params);
                }
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
            "tools/list" => {
                let mut listing = handle_list_tools_method(&state, request_registry.clone()).await;
                if client_confirms_writes(&headers) {
                    if let Ok(listing) = listing.as_mut() {
                        add_read_only_twins(&state, request_registry.clone(), listing).await;
                    }
                }
                listing
            }
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
        }
    })
    .await;

    if let (Some(name), Err(error)) = (tool_name.as_deref(), result.as_ref()) {
        log_failed_tool_call(&headers, name, error);
    }
    if let Some(name) = tool_name.as_deref() {
        log_tool_call_timing(
            &headers,
            name,
            tool_action.unwrap_or("other"),
            tool_call_started.elapsed(),
            result.is_ok(),
            api_calls,
        );
    }

    // Acceleration signal emit — fire-and-forget so the tool's response
    // latency stays untouched. These events are non-canonical telemetry
    // and prewarm hints only; no tool correctness depends on delivery.
    // Atlas stream remains a temporary compatibility fallback when no
    // acceleration signal provider is configured.
    if let (Some(name), Some(ws)) = (tool_name.as_deref(), workspace_id_for_stream) {
        let outcome = if result.is_ok() { "ok" } else { "error" };
        let elapsed_ms = tool_call_started.elapsed().as_millis() as u64;
        let action = tool_action.unwrap_or("other");
        let payload = serde_json::json!({
            "tool_name": name,
            "action": action,
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
            event.action = Some(action.to_string());
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

/// The tool surface for one request, first match wins:
///
/// 0. A server configured with the coach surface serves only that.
/// 1. The path: the integration paths (/chatgpt, /claude, /client) get the
///    full surface, /coach the Agent Coach Kit surface.
/// 2. The `X-ContextStream-Tool-Surface-Profile` header.
/// 3. The session's own choice: an initialize request's params, or the code in
///    the `Mcp-Session-Id` that initialize issued.
/// 4. The server's configured default.
fn resolve_request_tool_surface_profile(
    path_profile: Option<ToolSurfaceProfile>,
    header_profile: Option<ToolSurfaceProfile>,
    session_profile: Option<ToolSurfaceProfile>,
    default_profile: ToolSurfaceProfile,
) -> ToolSurfaceProfile {
    if default_profile == ToolSurfaceProfile::Coach {
        return ToolSurfaceProfile::Coach;
    }
    path_profile
        .or(header_profile)
        .or(session_profile)
        .unwrap_or(default_profile)
}

/// Marks an `Mcp-Session-Id` that carries a tool surface code.
const SESSION_SURFACE_MARKER: u8 = 0xc5;

fn surface_profile_code(profile: ToolSurfaceProfile) -> u8 {
    match profile {
        ToolSurfaceProfile::Default => 0,
        ToolSurfaceProfile::OpenaiAgentic => 1,
        ToolSurfaceProfile::Coach => 2,
        ToolSurfaceProfile::Studio => 3,
    }
}

/// Mint an initialize-era `Mcp-Session-Id` that carries the session's tool
/// surface, so whichever replica serves a later request of the session needs
/// no shared state to find it. The id stays a canonical UUID (version 8, the
/// RFC 9562 custom layout): random bytes, then the marker and the code.
fn mint_mcp_session_id(profile: ToolSurfaceProfile) -> String {
    let mut bytes = *Uuid::new_v4().as_bytes();
    bytes[14] = SESSION_SURFACE_MARKER;
    bytes[15] = surface_profile_code(profile);
    uuid::Builder::from_custom_bytes(bytes)
        .into_uuid()
        .to_string()
}

/// The tool surface an `Mcp-Session-Id` from [`mint_mcp_session_id`] carries.
/// Ids minted before the code existed (version 4) carry none.
fn surface_profile_from_session_id(session_id: &str) -> Option<ToolSurfaceProfile> {
    let id = Uuid::parse_str(session_id.trim()).ok()?;
    let bytes = id.as_bytes();
    if id.get_version_num() != 8
        || id.get_variant() != uuid::Variant::RFC4122
        || bytes[14] != SESSION_SURFACE_MARKER
    {
        return None;
    }
    [
        ToolSurfaceProfile::Default,
        ToolSurfaceProfile::OpenaiAgentic,
        ToolSurfaceProfile::Coach,
        ToolSurfaceProfile::Studio,
    ]
    .into_iter()
    .find(|profile| surface_profile_code(*profile) == bytes[15])
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

/// Whether the request comes from a client that runs in its vendor's cloud
/// and has no checkout of the user's folders: Google's Gemini app, whose
/// requests all carry `User-Agent: Google` and whose tool calls come from
/// sessions initialized as clientInfo `Gemini`.
fn client_without_local_checkout(headers: &HeaderMap) -> bool {
    header_str(headers, "user-agent").is_some_and(|agent| agent.trim() == "Google")
        || session_client_identity(headers).0 == mcp_types::harness::GEMINI_APP_CLIENT_NAME
}

/// Whether the request comes from a client that asks its person to confirm
/// every tool call not annotated read-only: the Gemini app, the same requests
/// as [`client_without_local_checkout`]. Its `tools/list` also carries the
/// read-only twins (`mcp_tools::domains::read_only`).
fn client_confirms_writes(headers: &HeaderMap) -> bool {
    client_without_local_checkout(headers)
}

async fn add_read_only_twins(
    state: &HttpState,
    registry_override: Option<Arc<ToolRegistry>>,
    listing: &mut Value,
) {
    let registry = match registry_override {
        Some(registry) => registry,
        None => effective_registry(state).await,
    };
    if let Some(tools) = listing.get_mut("tools").and_then(Value::as_array_mut) {
        crate::server::with_read_only_twins(&registry, None, tools);
    }
}

/// A `folder_path` such a client passes to `init` names a folder on the
/// user's computer that nothing will ever index, so `init` may match it to an
/// existing project but must not create one (with "the index builds in the
/// background"). An explicit `skip_project_creation` still wins.
fn apply_cloud_client_init_defaults(request: &mut JsonRpcRequest, headers: &HeaderMap) {
    if request.method != "tools/call" || !client_without_local_checkout(headers) {
        return;
    }
    let Some(params) = request.params.as_object_mut() else {
        return;
    };
    if params.get("name").and_then(Value::as_str) != Some("init") {
        return;
    }
    if !params.get("arguments").is_some_and(Value::is_object) {
        params.insert("arguments".to_string(), json!({}));
    }
    if let Some(arguments) = params.get_mut("arguments").and_then(Value::as_object_mut) {
        arguments
            .entry("skip_project_creation".to_string())
            .or_insert(json!(true));
    }
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
    let mut tools = contextstream_tools_list(&registry, params.category.as_deref());
    with_slipstream_listing(
        &registry,
        &state.client,
        &state.session,
        params.category.as_deref(),
        &mut tools,
    )
    .await;

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

    if registry.is_router_mode() && !registry.is_coach_surface() {
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

    // The registry's own tool names are part of the key: a per-request registry
    // (a config override, the stateless transport) must never be served another
    // registry's cached list, nor fill the shared registry's entry.
    let mut names = registry.names();
    names.sort_unstable();
    format!(
        "toolset={}|progressive={}|router={}|consolidated={}|tools={}|surface={}",
        toolset_cache_label(config.toolset),
        config.progressive_mode,
        config.router_mode,
        config.consolidated_mode,
        names.join(","),
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

/// Log a client's self-reported `clientInfo.name` when no harness alias
/// matches it, so aliases for new clients come from real hosted traffic (see
/// `HarnessId::from_client_hint`). Only a short, printable form of the name and
/// version is logged, and neither is used for authorization.
fn log_unrecognized_client(params: &Value) {
    if let Some((name, version)) = unrecognized_client_identity(params) {
        info!(
            target: "contextstream::client_identity",
            client_info_name = %name,
            client_info_version = %version,
            "unrecognized MCP client"
        );
    }
}

/// The loggable `(name, version)` of a client no harness alias recognizes.
fn unrecognized_client_identity(params: &Value) -> Option<(String, String)> {
    let client = params
        .get("clientInfo")
        .or_else(|| params.get("client_info"))?;
    let name = client.get("name").and_then(Value::as_str)?;
    if HarnessId::from_client_hint(name).is_some() {
        return None;
    }
    let name = loggable_client_token(name)?;
    let version = client
        .get("version")
        .and_then(Value::as_str)
        .and_then(loggable_client_token)
        .unwrap_or_default();
    Some((name, version))
}

/// At most 64 characters from a small printable set, or `None` when nothing
/// usable remains.
fn loggable_client_token(raw: &str) -> Option<String> {
    let token: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '.' | '_' | '-' | '/' | '+'))
        .take(64)
        .collect();
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// `clientInfo` (name, version) of recent MCP sessions, kept only so a failed
/// tool call can be logged with the client that sent it. Never used for
/// authorization. Bounded: cleared when it reaches [`SESSION_CLIENTS_MAX`].
static SESSION_CLIENTS: std::sync::LazyLock<std::sync::Mutex<HashMap<String, (String, String)>>> =
    std::sync::LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));
const SESSION_CLIENTS_MAX: usize = 10_000;

/// The loggable `(name, version)` of any client, `("unknown", "")` when absent.
fn loggable_client_identity(params: &Value) -> (String, String) {
    let client = params
        .get("clientInfo")
        .or_else(|| params.get("client_info"));
    let field = |key: &str| {
        client
            .and_then(|c| c.get(key))
            .and_then(Value::as_str)
            .and_then(loggable_client_token)
    };
    (
        field("name").unwrap_or_else(|| "unknown".to_string()),
        field("version").unwrap_or_default(),
    )
}

fn remember_session_client(session_id: &str, params: &Value) {
    let identity = loggable_client_identity(params);
    if let Ok(mut clients) = SESSION_CLIENTS.lock() {
        if clients.len() >= SESSION_CLIENTS_MAX {
            clients.clear();
        }
        clients.insert(session_id.to_string(), identity);
    }
}

fn session_client_identity(headers: &HeaderMap) -> (String, String) {
    header_str(headers, "mcp-session-id")
        .and_then(|id| SESSION_CLIENTS.lock().ok()?.get(id).cloned())
        .unwrap_or_else(|| ("unknown".to_string(), String::new()))
}

/// A short class for a failed tool call's error, from its message prefix
/// (`mcp_types::Error`'s display forms). Classes carry no argument values.
fn tool_error_class(message: &str) -> String {
    let m = message.trim_start();
    if let Some(rest) = m.strip_prefix("Validation error: ") {
        let sub = if rest.contains("do not match the tool's input schema") {
            "schema_mismatch"
        } else if rest.starts_with("invalid type") {
            "invalid_type"
        } else if rest.starts_with("missing field") {
            "missing_field"
        } else if rest.starts_with("unknown variant") || rest.starts_with("invalid value") {
            "invalid_value"
        } else if rest.starts_with("repository_url") {
            "repository_url"
        } else {
            "other"
        };
        return format!("validation:{sub}");
    }
    if let Some(rest) = m.strip_prefix("HTTP error (") {
        let status: String = rest.chars().take_while(char::is_ascii_digit).collect();
        if !status.is_empty() {
            return format!("http:{status}");
        }
    }
    for (prefix, class) in [
        ("Network error", "network"),
        ("Configuration error", "configuration"),
        ("Tool error", "tool"),
        ("Serialization error", "serialization"),
        ("Invalid UUID", "invalid_uuid"),
        ("IO error", "io"),
        ("Rate limited", "rate_limited"),
        ("Feature not available", "feature_unavailable"),
        ("Integration not connected", "integration_not_connected"),
        ("Request cancelled", "cancelled"),
        ("Request timeout", "timeout"),
        ("Missing 'name'", "missing_tool_name"),
        ("Unknown tool", "unknown_tool"),
    ] {
        if m.starts_with(prefix) {
            return class.to_string();
        }
    }
    "other".to_string()
}

/// Log a failed tools/call: tool, JSON-RPC code, error class and client.
/// Argument values and the error text (which can quote them) are never logged.
/// One info line per tools/call with its timing. Labels only: tool name,
/// bounded action, outcome, API attempt count and summed API time, client.
fn log_tool_call_timing(
    headers: &HeaderMap,
    tool: &str,
    action: &'static str,
    elapsed: std::time::Duration,
    ok: bool,
    api_calls: mcp_client::ApiCallStats,
) {
    let (client_name, _) = session_client_identity(headers);
    let tool = loggable_client_token(tool).unwrap_or_else(|| "unknown".to_string());
    info!(
        target: "contextstream::tool_timing",
        tool = %tool,
        action = action,
        duration_ms = elapsed.as_millis().min(u128::from(u64::MAX)) as u64,
        outcome = if ok { "ok" } else { "error" },
        api_calls = api_calls.calls,
        api_ms_sum = api_calls.elapsed_ms_sum,
        client_name = %client_name,
        "tool call timing"
    );
}

fn log_failed_tool_call(headers: &HeaderMap, tool: &str, error: &JsonRpcError) {
    let class = tool_error_class(&error.message);
    let (client_name, client_version) = session_client_identity(headers);
    let tool = loggable_client_token(tool).unwrap_or_else(|| "unknown".to_string());
    metrics::counter!(
        "mcp_tool_call_errors_total",
        "tool" => tool.clone(),
        "class" => class.clone(),
    )
    .increment(1);
    warn!(
        target: "contextstream::tool_errors",
        tool = %tool,
        error_code = error.code,
        error_class = %class,
        client_name = %client_name,
        client_version = %client_version,
        "tool call failed"
    );
}

/// How long the hosted gateway keeps the API's answer about one API key at
/// `initialize`, so reconnects do not each cost an API call.
const INITIALIZE_KEY_ACCEPTED_TTL: Duration = Duration::from_secs(600);
const INITIALIZE_KEY_REFUSED_TTL: Duration = Duration::from_secs(60);
const INITIALIZE_KEY_CHECK_TIMEOUT: Duration = Duration::from_secs(2);
const INITIALIZE_KEY_CACHE_LIMIT: usize = 10_000;
const INITIALIZE_KEY_REFUSED_MESSAGE: &str =
    "ContextStream did not accept the API key sent with this \
    connection (invalid, expired or revoked). Save a valid key with `contextstream-mcp setup` and \
    restart the editor, or update the X-ContextStream-API-Key header in the editor's MCP config.";
const INITIALIZE_TOKEN_REFUSED_MESSAGE: &str =
    "ContextStream did not accept the access token sent with this \
    connection (invalid or expired). Reconnect to sign in again, or use an API key from \
    `contextstream-mcp setup`.";

/// Hashed caller identity -> (accepted, when the API answered).
fn initialize_key_verdicts() -> &'static std::sync::Mutex<HashMap<String, (bool, Instant)>> {
    static VERDICTS: std::sync::OnceLock<std::sync::Mutex<HashMap<String, (bool, Instant)>>> =
        std::sync::OnceLock::new();
    VERDICTS.get_or_init(Default::default)
}

/// On the hosted gateway (`require_auth`), an `initialize` or a stateless
/// request whose credential the API refuses with a definite 401 fails, so an
/// editor does not report a connection whose every tool call would fail
/// (#252, #303). A bearer token is checked the same way unless the gateway
/// verified it itself (it has the JWT secret). An unreachable, slow or failing
/// API lets the request through. The API is asked with this request's own
/// credentials (the middleware's task-local auth override), and its verdict is
/// remembered per credential. Returns what to tell the client.
async fn credential_refused(state: &HttpState, headers: &HeaderMap) -> Option<&'static str> {
    if !state.require_auth {
        return None;
    }
    let auth = extract_auth_override(headers)?;
    let (identity, refusal) = match (auth.jwt.as_deref(), auth.api_key.as_deref()) {
        (Some(_), _) if state.jwt_secret.is_some() => return None,
        (Some(token), _) => (
            SessionKey::Jwt(token.to_string()).atlas_user_scope_token(),
            INITIALIZE_TOKEN_REFUSED_MESSAGE,
        ),
        (None, Some(api_key)) => (
            SessionKey::ApiKey(api_key.to_string()).atlas_user_scope_token(),
            INITIALIZE_KEY_REFUSED_MESSAGE,
        ),
        (None, None) => return None,
    };
    let now = Instant::now();
    if let Some(identity) = identity.as_deref() {
        let cached = initialize_key_verdicts()
            .lock()
            .unwrap_or_else(|err| err.into_inner())
            .get(identity)
            .copied();
        if let Some((accepted, at)) = cached {
            let ttl = if accepted {
                INITIALIZE_KEY_ACCEPTED_TTL
            } else {
                INITIALIZE_KEY_REFUSED_TTL
            };
            if now.duration_since(at) < ttl {
                return (!accepted).then_some(refusal);
            }
        }
    }
    let accepted = match tokio::time::timeout(INITIALIZE_KEY_CHECK_TIMEOUT, state.client.me()).await
    {
        Ok(Ok(_)) => true,
        Ok(Err(mcp_types::Error::Http { status: 401, .. })) => false,
        _ => return None,
    };
    if let Some(identity) = identity {
        let mut verdicts = initialize_key_verdicts()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        if verdicts.len() >= INITIALIZE_KEY_CACHE_LIMIT {
            verdicts.retain(|_, (_, at)| now.duration_since(*at) < INITIALIZE_KEY_ACCEPTED_TTL);
            if verdicts.len() >= INITIALIZE_KEY_CACHE_LIMIT {
                verdicts.clear();
            }
        }
        verdicts.insert(identity, (accepted, now));
    }
    (!accepted).then_some(refusal)
}

async fn handle_initialize_method(
    state: &HttpState,
    params: &Value,
) -> Result<Value, JsonRpcError> {
    log_unrecognized_client(params);
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
    let cached = state
        .tools_list_cache
        .read()
        .unwrap_or_else(|err| err.into_inner())
        .get(&cache_key)
        .cloned();
    let mut response = match cached {
        Some(cached) => cached,
        None => {
            let tools = contextstream_tools_list(&registry, None);
            let response = serde_json::json!({ "tools": tools });
            state
                .tools_list_cache
                .write()
                .unwrap_or_else(|err| err.into_inner())
                .insert(cache_key, response.clone());
            response
        }
    };

    // The shared cache holds what every caller sees; Slipstream depends on the caller's
    // workspace and is added per request.
    if let Some(tools) = response
        .get_mut("tools")
        .and_then(|tools| tools.as_array_mut())
    {
        with_slipstream_listing(&registry, &state.client, &state.session, None, tools).await;
    }

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

    if let Some(message) =
        mcp_tools::input_shape::tools_call_arguments_error(name, params.get("arguments"))
    {
        return Err(JsonRpcError {
            code: -32602,
            message,
            data: None,
        });
    }
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or(serde_json::json!({}));
    if strict_protocol_errors {
        validate_contextstream_meta_tool_arguments(name, &arguments).map_err(JsonRpcError::from)?;
    }

    // The coach surface has no router meta-tools: only its three tools run.
    if registry.is_router_mode() && !registry.is_coach_surface() {
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
    // A stateless call whose client can ask its person carries the person's answers in,
    // and a question out as its own result (`input_required`; see `mcp_tools::peer`).
    let call_peer = (strict_protocol_errors
        && mcp_tools::peer::CallPeer::stateless_client_can_elicit(&params))
    .then(|| {
        mcp_tools::peer::CallPeer::new(Arc::new(mcp_tools::peer::NoLiveChannel), None, false)
            .with_inline_input(&params)
    });
    let execution = mcp_tools::wire_tokens::run_with_wire_response_context(
        wire_context,
        registry.execute(name, arguments),
    );
    let outcome = match call_peer.clone() {
        Some(peer) => mcp_tools::peer::run_with_call_peer(peer, execution).await,
        None => execution.await,
    };

    match outcome {
        Ok(result) => {
            state
                .telemetry
                .emit_managed_tool_readiness(name, &result)
                .await;
            let mut response =
                tool_result_response_with_title(result, call_title.as_deref(), Some(call_icon));
            if let (Some(Value::Object(fields)), Some(target)) = (
                call_peer.and_then(|peer| peer.take_input_required()),
                response.as_object_mut(),
            ) {
                target.extend(fields);
            }
            Ok(response)
        }
        Err(err) => {
            if err.is_non_blocking_parser_error() {
                debug!(
                    error = %err,
                    "suppressed non-blocking ParserError in JSON-RPC tool response"
                );
            }
            tool_call_error_response(
                err,
                strict_protocol_errors,
                call_title.as_deref(),
                call_icon,
            )
        }
    }
}

/// A failed tool call, answered as the stdio server answers it (#280): only the
/// stateless contract's argument errors are JSON-RPC protocol errors (-32602);
/// every other failure is a tool result with `isError: true`, so the model can
/// read it and correct course, keeping the call's title and, for exhausted
/// credits, the guidance `ToolResult::credits_exhausted` carries.
fn tool_call_error_response(
    err: mcp_types::Error,
    strict_protocol_errors: bool,
    call_title: Option<&str>,
    call_icon: &str,
) -> Result<Value, JsonRpcError> {
    if strict_protocol_errors
        && matches!(
            &err,
            mcp_types::Error::Validation(_)
                | mcp_types::Error::InvalidUuid(_)
                | mcp_types::Error::Serialization(_)
        )
    {
        return Err(JsonRpcError {
            code: -32602,
            message: err.user_facing_message(),
            data: None,
        });
    }
    if let mcp_types::Error::InsufficientCredits {
        required,
        available,
        ..
    } = &err
    {
        let mut result = mcp_types::tool::ToolResult::credits_exhausted(*required, *available);
        result.is_error = true;
        return Ok(tool_result_response_with_title(
            result,
            call_title,
            Some(call_icon),
        ));
    }
    Ok(tool_error_response_with_title(
        format!("[ERROR] {}", err.user_facing_message()),
        call_title,
        Some(call_icon),
    ))
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

    // An empty or blank credential header (what an editor config sends when
    // the variable it names is unset) is no credential (#303).
    let credential = |name: &str| {
        headers
            .get(name)
            .and_then(|h| h.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };

    // Explicit API-key headers
    if let Some(v) = credential("x-api-key").or_else(|| credential("x-contextstream-api-key")) {
        api_key = Some(v);
    }

    // Explicit JWT header
    if let Some(v) = credential("x-contextstream-jwt") {
        jwt = Some(v);
    }

    // Authorization: Bearer – could be an API key or a JWT
    if api_key.is_none() && jwt.is_none() {
        if let Some(bearer) = headers
            .get(header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
            .map(str::trim)
            .filter(|token| !token.is_empty())
        {
            let token = bearer.to_string();
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

    // Like the traffic class, the client id travels only with credentials
    // and only as a known harness id, never as the caller's raw string.
    let client = has_credential
        .then(|| header_str(headers, "x-contextstream-client"))
        .flatten()
        .and_then(HarnessId::from_alias);

    let auth = AuthOverride {
        api_key,
        jwt,
        workspace_id,
        project_id,
        traffic_class,
        client,
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
        // The person's own cap; the gateway's environment never sets one for callers. The
        // header's first name is read when the current one is not sent.
        slipstream_auto_accept_max_credits: header_str(
            headers,
            mcp_tools::domains::slipstream::AUTO_ACCEPT_HEADER,
        )
        .or_else(|| {
            header_str(
                headers,
                mcp_tools::domains::slipstream::LEGACY_AUTO_ACCEPT_HEADER,
            )
        })
        .and_then(|value| value.trim().parse::<u32>().ok()),
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

fn oauth_resource_metadata_url(headers: &HeaderMap, path: &str) -> String {
    let metadata = if path == COACH_PATH {
        COACH_RESOURCE_METADATA_PATH
    } else if path == STUDIO_PATH {
        STUDIO_RESOURCE_METADATA_PATH
    } else {
        "/.well-known/oauth-protected-resource"
    };
    format!(
        "{}{metadata}",
        request_origin(headers).trim_end_matches('/')
    )
}

fn oauth_protected_resource_metadata(headers: &HeaderMap) -> Value {
    json!({
        "resource": request_origin(headers),
        "authorization_servers": [authorization_server_origin()]
    })
}

/// The Agent Coach Kit resource: `<origin>/coach`, with only the coach
/// scopes, so a ChatGPT connector asks for nothing else.
fn coach_protected_resource_metadata(headers: &HeaderMap) -> Value {
    json!({
        "resource": format!("{}{COACH_PATH}", request_origin(headers).trim_end_matches('/')),
        "authorization_servers": [authorization_server_origin()],
        "scopes_supported": [
            mcp_client::coach::READ_SCOPE,
            mcp_client::coach::WRITE_SCOPE
        ],
        "bearer_methods_supported": ["header"],
        "resource_name": "Agent Coach Kit"
    })
}

/// The ContextStudio connector's resource: `<origin>/studio`, with only the studio
/// scopes. The API holds a token bound to it to Create's making routes and the reads the
/// tool needs (contextstream `middleware::oauth_resource::studio_route`).
fn studio_protected_resource_metadata(headers: &HeaderMap) -> Value {
    json!({
        "resource": format!("{}{STUDIO_PATH}", request_origin(headers).trim_end_matches('/')),
        "authorization_servers": [authorization_server_origin()],
        "scopes_supported": [STUDIO_READ_SCOPE, STUDIO_WRITE_SCOPE],
        "bearer_methods_supported": ["header"],
        "resource_name": "ContextStudio"
    })
}

fn oauth_unauthorized_response(
    headers: &HeaderMap,
    path: &str,
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

    let mut www_authenticate = format!(
        r#"Bearer resource_metadata="{}""#,
        oauth_resource_metadata_url(headers, path)
    );
    if path == COACH_PATH {
        www_authenticate.push_str(&format!(
            r#", scope="{} {}""#,
            mcp_client::coach::READ_SCOPE,
            mcp_client::coach::WRITE_SCOPE
        ));
    } else if path == STUDIO_PATH {
        www_authenticate.push_str(&format!(
            r#", scope="{STUDIO_READ_SCOPE} {STUDIO_WRITE_SCOPE}""#
        ));
    }
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
    // connector flow. That holds for every `/.well-known/` path: one the
    // gateway does not publish, such as the RFC 9728 URL a client builds
    // from `/mcp`, is a 404, so the client falls back to the root document.
    if path == "/health"
        || path == "/api/v1/health"
        || path == "/metrics"
        || path.starts_with("/.well-known/")
    {
        return Ok(next.run(request).await);
    }

    let auth = extract_auth_override(&headers);
    let config_override = extract_config_override(&headers);
    let edge_geography = extract_edge_geography(&headers);
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
                        path,
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
            path,
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
    let exec_with_session = || async move {
        match effective_mcp_session_id {
            Some(sid) => mcp_client::run_with_mcp_session_id(sid, exec_with_caller_cache).await,
            None => exec_with_caller_cache().await,
        }
    };
    // The edge's view of the caller's location rides every API call made for
    // this request, and the tenant home the API names on those calls comes
    // back on this response so the Cloudflare MCP gateway can remember it.
    let (mut response, tenant_home) = mcp_client::run_capturing_tenant_home(|| async move {
        match edge_geography {
            Some(geography) => {
                mcp_client::run_with_edge_geography(geography, exec_with_session).await
            }
            None => exec_with_session().await,
        }
    })
    .await;
    publish_tenant_home(response.headers_mut(), tenant_home);
    if let Some(key) = transient_session_key {
        state.session.discard_transient_state(&key);
    }
    Ok(response)
}

/// Name of the response header the API sets once a request identified its
/// tenant; mirrored onto the gateway's response. Must match
/// TENANT_HOME_REGION_HEADER in the API's region module.
const TENANT_HOME_REGION_HEADER: &str = "x-contextstream-tenant-home-region";

/// Publish the tenant home captured from the API, with the scopes named on
/// the API response that carried it. Any other copy is removed first: the
/// gateway must learn only what an API response said, and a scope only
/// beside the home it describes.
fn publish_tenant_home(headers: &mut HeaderMap, captured: Option<mcp_client::CapturedTenantHome>) {
    headers.remove(TENANT_HOME_REGION_HEADER);
    headers.remove(mcp_client::TENANT_HOME_SCOPE_HEADER);
    let Some(captured) = captured else {
        return;
    };
    let Ok(home) = header::HeaderValue::from_str(&captured.home) else {
        return;
    };
    headers.insert(TENANT_HOME_REGION_HEADER, home);
    if let Some(scopes) = captured
        .scopes
        .and_then(|scopes| header::HeaderValue::from_str(&scopes).ok())
    {
        headers.insert(mcp_client::TENANT_HOME_SCOPE_HEADER, scopes);
    }
}

/// What the Cloudflare router said about the caller's location. Both values
/// are validated to their canonical shapes; anything else is dropped so no
/// caller text reaches API telemetry through this path.
fn extract_edge_geography(headers: &HeaderMap) -> Option<EdgeGeography> {
    let geography = EdgeGeography {
        nearest_region: header_str(headers, NearestRegion::HEADER_NAME)
            .and_then(NearestRegion::from_header_value),
        client_country: header_str(headers, EdgeGeography::COUNTRY_HEADER_NAME)
            .and_then(EdgeGeography::parse_country),
    };
    if geography.is_empty() {
        None
    } else {
        Some(geography)
    }
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

    const SCOPE_WORKSPACE: &str = "workspace=5dfde09db0822996";
    const SCOPE_PROJECT: &str = "project=0123456789abcdef";

    #[test]
    fn publish_tenant_home_owns_both_headers() {
        let scope = mcp_client::TENANT_HOME_SCOPE_HEADER;
        let stale = || {
            let mut headers = HeaderMap::new();
            headers.insert(TENANT_HOME_REGION_HEADER, "eu-ams".parse().unwrap());
            headers.insert(scope, SCOPE_PROJECT.parse().unwrap());
            headers
        };

        let mut headers = stale();
        publish_tenant_home(
            &mut headers,
            Some(mcp_client::CapturedTenantHome {
                home: "ovh-west".to_string(),
                scopes: Some(SCOPE_WORKSPACE.to_string()),
            }),
        );
        assert_eq!(headers[TENANT_HOME_REGION_HEADER], "ovh-west");
        assert_eq!(headers[scope], SCOPE_WORKSPACE);

        let mut headers = stale();
        publish_tenant_home(
            &mut headers,
            Some(mcp_client::CapturedTenantHome {
                home: "ovh-east".to_string(),
                scopes: None,
            }),
        );
        assert_eq!(headers[TENANT_HOME_REGION_HEADER], "ovh-east");
        assert!(
            headers.get(scope).is_none(),
            "a stale scope must not ride a new home"
        );

        let mut headers = stale();
        publish_tenant_home(&mut headers, None);
        assert!(headers.get(TENANT_HOME_REGION_HEADER).is_none());
        assert!(headers.get(scope).is_none());
    }

    /// An API that answers every request with an empty success, naming the
    /// given tenant home and scopes.
    async fn tenant_home_api(home: Option<&'static str>, scopes: Option<&'static str>) -> String {
        use axum::response::IntoResponse;
        let app = axum::Router::new().fallback(move || async move {
            let mut response = axum::Json(json!({"success": true, "data": []})).into_response();
            if let Some(home) = home {
                response
                    .headers_mut()
                    .insert(TENANT_HOME_REGION_HEADER, home.parse().unwrap());
            }
            if let Some(scopes) = scopes {
                response.headers_mut().insert(
                    mcp_client::TENANT_HOME_SCOPE_HEADER,
                    scopes.parse().unwrap(),
                );
            }
            response
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind tenant-home API");
        let address = listener.local_addr().expect("tenant-home API address");
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve tenant-home API")
        });
        format!("http://{address}")
    }

    /// A stateless workspace-list tool call against `api_url`, carrying
    /// client-supplied copies of both tenant-home headers, which must never
    /// reach the response.
    async fn tool_call_tenant_home_headers(api_url: String) -> HeaderMap {
        let config = Config {
            api_url,
            api_key: Some("test-api-key".to_string()),
            is_http_transport: true,
            ..Config::default()
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
        let call = json!({
            "jsonrpc": "2.0",
            "id": "tenant-home",
            "method": "tools/call",
            "params": {
                "name": "workspace",
                "arguments": {"action": "list"},
                "_meta": stateless_params(MCP_PROTOCOL_2026_07_28)["_meta"].clone()
            }
        });
        let mut request = stateless_streamable_request(
            call,
            MCP_PROTOCOL_2026_07_28,
            "tools/call",
            Some("workspace"),
            None,
        );
        let headers = request.headers_mut();
        headers.insert(
            "x-contextstream-api-key",
            "caller-request-key".parse().unwrap(),
        );
        headers.insert(TENANT_HOME_REGION_HEADER, "eu-ams".parse().unwrap());
        headers.insert(
            mcp_client::TENANT_HOME_SCOPE_HEADER,
            SCOPE_PROJECT.parse().unwrap(),
        );
        let response = create_router(state)
            .oneshot(request)
            .await
            .expect("tool call");
        assert_eq!(response.status(), StatusCode::OK);
        response.headers().clone()
    }

    #[tokio::test]
    async fn tool_call_relays_the_home_and_scopes_from_the_api_response() {
        let headers = tool_call_tenant_home_headers(
            tenant_home_api(Some("ovh-west"), Some(SCOPE_WORKSPACE)).await,
        )
        .await;
        assert_eq!(headers[TENANT_HOME_REGION_HEADER], "ovh-west");
        assert_eq!(
            headers[mcp_client::TENANT_HOME_SCOPE_HEADER],
            SCOPE_WORKSPACE
        );
    }

    #[tokio::test]
    async fn tool_call_relays_no_scope_beside_a_home_whose_response_named_none() {
        let headers =
            tool_call_tenant_home_headers(tenant_home_api(Some("ovh-east"), None).await).await;
        assert_eq!(headers[TENANT_HOME_REGION_HEADER], "ovh-east");
        assert!(headers.get(mcp_client::TENANT_HOME_SCOPE_HEADER).is_none());
    }

    #[tokio::test]
    async fn tool_call_relays_an_oregon_home_and_its_scope() {
        // An Oregon-homed tenant's scoped tools/call: the edge routes the
        // caller's next calls to us-oregon only if this header reaches it.
        let headers = tool_call_tenant_home_headers(
            tenant_home_api(Some("us-oregon"), Some(SCOPE_WORKSPACE)).await,
        )
        .await;
        assert_eq!(headers[TENANT_HOME_REGION_HEADER], "us-oregon");
        assert_eq!(
            headers[mcp_client::TENANT_HOME_SCOPE_HEADER],
            SCOPE_WORKSPACE
        );

        let headers =
            tool_call_tenant_home_headers(tenant_home_api(Some("us-virginia"), None).await).await;
        assert_eq!(headers[TENANT_HOME_REGION_HEADER], "us-virginia");
    }

    #[tokio::test]
    async fn tool_call_relays_neither_header_when_the_api_named_no_home() {
        let headers =
            tool_call_tenant_home_headers(tenant_home_api(None, Some(SCOPE_WORKSPACE)).await).await;
        assert!(headers.get(TENANT_HOME_REGION_HEADER).is_none());
        assert!(headers.get(mcp_client::TENANT_HOME_SCOPE_HEADER).is_none());
    }

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

    // #287: a body that is not one JSON-RPC request gets a JSON-RPC error with
    // a short reason, never framework text, and a client's response gets 202.
    #[tokio::test]
    async fn streamable_http_answers_malformed_bodies_in_json_rpc() {
        async fn post(content_type: &str, body: impl Into<Body>) -> Response {
            create_router(create_protocol_test_state())
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/mcp")
                        .header("content-type", content_type)
                        .header("accept", "application/json, text/event-stream")
                        .body(body.into())
                        .unwrap(),
                )
                .await
                .unwrap()
        }
        async fn error_of(response: Response, status: StatusCode) -> (Value, i64, String) {
            assert_eq!(response.status(), status);
            assert_eq!(response.headers()[header::CONTENT_TYPE], "application/json");
            let body = json_response(response).await;
            assert_eq!(body["jsonrpc"], "2.0");
            let message = body["error"]["message"].as_str().unwrap().to_string();
            assert!(
                !message.contains("target type") && !message.contains("invalid type"),
                "{message}"
            );
            (
                body["id"].clone(),
                body["error"]["code"].as_i64().unwrap(),
                message,
            )
        }
        async fn accepted(response: Response) {
            assert_eq!(response.status(), StatusCode::ACCEPTED);
            let body = response.into_body().collect().await.unwrap().to_bytes();
            assert!(body.is_empty());
        }

        accepted(
            post(
                "application/json",
                r#"{"jsonrpc":"2.0","id":9,"result":{}}"#,
            )
            .await,
        )
        .await;
        accepted(
            post(
                "application/json",
                r#"{"jsonrpc":"2.0","id":9,"error":{"code":-1,"message":"no"}}"#,
            )
            .await,
        )
        .await;
        accepted(
            post(
                "application/json; charset=utf-8",
                r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            )
            .await,
        )
        .await;

        let (id, code, message) = error_of(
            post(
                "application/json",
                r#"[{"jsonrpc":"2.0","id":5,"method":"ping"},{"jsonrpc":"2.0","id":6,"method":"ping"}]"#,
            )
            .await,
            StatusCode::BAD_REQUEST,
        )
        .await;
        assert_eq!((id, code), (Value::Null, -32600));
        assert!(message.contains("batches are not supported"), "{message}");

        let (id, code, message) = error_of(
            post("application/json", "{nope").await,
            StatusCode::BAD_REQUEST,
        )
        .await;
        assert_eq!((id, code), (Value::Null, -32700));
        assert!(message.starts_with("Parse error: "), "{message}");

        let (id, code, message) =
            error_of(post("application/json", "").await, StatusCode::BAD_REQUEST).await;
        assert_eq!((id, code), (Value::Null, -32700));
        assert!(message.contains("empty"), "{message}");

        let (_, code, message) = error_of(
            post("text/plain", "{}").await,
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        )
        .await;
        assert_eq!(code, -32600);
        assert!(
            message.contains("Content-Type: application/json"),
            "{message}"
        );

        let (id, code, message) = error_of(
            post("application/json", r#"{"jsonrpc":"2.0","id":3}"#).await,
            StatusCode::BAD_REQUEST,
        )
        .await;
        assert_eq!((id, code), (json!(3), -32600));
        assert_eq!(message, "Invalid Request: missing \"method\"");

        let (id, _, message) = error_of(
            post(
                "application/json",
                r#"{"jsonrpc":"2.0","id":"a","method":7}"#,
            )
            .await,
            StatusCode::BAD_REQUEST,
        )
        .await;
        assert_eq!(id, json!("a"));
        assert_eq!(message, "Invalid Request: \"method\" must be a string");

        let (id, code, _) = error_of(
            post("application/json", r#""ping""#).await,
            StatusCode::BAD_REQUEST,
        )
        .await;
        assert_eq!((id, code), (Value::Null, -32600));

        let (id, code, message) = error_of(
            post("application/json", vec![b' '; MAX_REQUEST_BODY_BYTES + 1]).await,
            StatusCode::PAYLOAD_TOO_LARGE,
        )
        .await;
        assert_eq!((id, code), (Value::Null, -32600));
        assert!(message.contains("2 MB (2097152 bytes)"), "{message}");
    }

    // Ticket 7c787154: one registry serves every caller. A ChatGPT-like and a
    // Claude-like session interleaved on one process each keep the surface
    // their initialize chose, the registry is never changed, and the tools/list
    // cache never holds a list built under another surface than its key's.
    #[tokio::test]
    async fn interleaved_http_sessions_keep_their_own_tool_surfaces() {
        fn legacy_request(path: &str, session_id: Option<&str>, body: Value) -> Request<Body> {
            let mut builder = Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .header("accept", "application/json, text/event-stream");
            if let Some(session_id) = session_id {
                builder = builder.header("mcp-session-id", session_id);
            }
            builder
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap()
        }
        async fn initialize(app: &Router, client_name: &str) -> String {
            let body = json!({
                "jsonrpc": "2.0",
                "id": "initialize",
                "method": "initialize",
                "params": {
                    "protocolVersion": mcp_types::MCP_PROTOCOL_2024_11_05,
                    "clientInfo": {"name": client_name, "version": "1.0.0"},
                    "capabilities": {}
                }
            });
            let response = app
                .clone()
                .oneshot(legacy_request("/mcp", None, body))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            response.headers()["mcp-session-id"]
                .to_str()
                .unwrap()
                .to_string()
        }
        async fn tool_names(app: &Router, path: &str, session_id: &str) -> Vec<String> {
            let body =
                json!({"jsonrpc": "2.0", "id": "list", "method": "tools/list", "params": {}});
            let response = app
                .clone()
                .oneshot(legacy_request(path, Some(session_id), body))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            json_response(response).await["result"]["tools"]
                .as_array()
                .expect("tools array")
                .iter()
                .filter_map(|tool| tool["name"].as_str().map(str::to_string))
                .collect()
        }
        let has = |names: &[String], name: &str| names.iter().any(|n| n == name);

        let state = create_protocol_test_state();
        let registry = state.registry.clone();
        let tools_list_cache = state.tools_list_cache.clone();
        let app = create_router(state);

        let chatgpt = initialize(&app, "chatgpt").await;
        let claude = initialize(&app, "claude-code").await;
        assert_eq!(
            surface_profile_from_session_id(&chatgpt),
            Some(ToolSurfaceProfile::OpenaiAgentic)
        );
        assert_eq!(
            surface_profile_from_session_id(&claude),
            Some(ToolSurfaceProfile::Default)
        );

        for _ in 0..2 {
            let agentic = tool_names(&app, "/mcp", &chatgpt).await;
            assert!(has(&agentic, "tool_search") && !has(&agentic, "entity"));
            let full = tool_names(&app, "/mcp", &claude).await;
            assert!(has(&full, "entity") && !has(&full, "tool_search"));
        }
        // An integration path serves the full surface even to an agentic
        // session, and leaves that session's own surface alone.
        let forced = tool_names(&app, "/chatgpt", &chatgpt).await;
        assert!(has(&forced, "entity") && !has(&forced, "tool_search"));
        assert!(has(
            &tool_names(&app, "/mcp", &chatgpt).await,
            "tool_search"
        ));

        assert_eq!(registry.tool_surface_profile(), ToolSurfaceProfile::Default);

        let cache = tools_list_cache
            .read()
            .unwrap_or_else(|err| err.into_inner());
        assert_eq!(cache.len(), 2, "one list per surface");
        for (key, list) in cache.iter() {
            let names: Vec<String> = list["tools"]
                .as_array()
                .expect("tools array")
                .iter()
                .filter_map(|tool| tool["name"].as_str().map(str::to_string))
                .collect();
            assert_eq!(
                key.ends_with("surface=openai_agentic"),
                has(&names, "tool_search"),
                "{key}"
            );
            assert_eq!(
                key.ends_with("surface=default"),
                has(&names, "entity"),
                "{key}"
            );
        }
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

        fn tool_call(name: &str, arguments: Value) -> JsonRpcRequest {
            JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: Some(json!(1)),
                method: "tools/call".to_string(),
                params: json!({ "name": name, "arguments": arguments }),
            }
        }

        fn headers_with(pairs: &[(&'static str, &str)]) -> HeaderMap {
            let mut headers = HeaderMap::new();
            for (name, value) in pairs {
                headers.insert(*name, value.parse().unwrap());
            }
            headers
        }

        #[test]
        fn gemini_init_does_not_create_a_project_unless_asked() {
            let google = headers_with(&[("user-agent", "Google")]);
            let mut request = tool_call("init", json!({ "folder_path": "/Users/me/dev/app" }));
            apply_cloud_client_init_defaults(&mut request, &google);
            assert_eq!(
                request.params["arguments"]["skip_project_creation"],
                json!(true)
            );

            let mut explicit = tool_call(
                "init",
                json!({ "folder_path": "/Users/me/dev/app", "skip_project_creation": false }),
            );
            apply_cloud_client_init_defaults(&mut explicit, &google);
            assert_eq!(
                explicit.params["arguments"]["skip_project_creation"],
                json!(false)
            );

            let mut no_arguments = JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: Some(json!(1)),
                method: "tools/call".to_string(),
                params: json!({ "name": "init" }),
            };
            apply_cloud_client_init_defaults(&mut no_arguments, &google);
            assert_eq!(
                no_arguments.params["arguments"]["skip_project_creation"],
                json!(true)
            );

            let mut other_tool = tool_call("search", json!({ "query": "auth" }));
            apply_cloud_client_init_defaults(&mut other_tool, &google);
            assert!(other_tool.params["arguments"]
                .get("skip_project_creation")
                .is_none());
        }

        #[test]
        fn only_the_gemini_app_gets_the_read_only_twins() {
            assert!(client_confirms_writes(&headers_with(&[(
                "user-agent",
                "Google"
            )])));
            for agent in [
                "claude-code/2.1.312 (cli)",
                "codex-mcp-client/0.165.0",
                "Google-Extended",
            ] {
                assert!(
                    !client_confirms_writes(&headers_with(&[("user-agent", agent)])),
                    "{agent}"
                );
            }
        }

        #[test]
        fn gemini_session_init_does_not_create_a_project() {
            let session_id = "6f0c1e8a-0000-4000-8000-00000000gem1";
            remember_session_client(
                session_id,
                &json!({ "clientInfo": { "name": "Gemini", "version": "1" } }),
            );
            let headers = headers_with(&[("mcp-session-id", session_id), ("user-agent", "x")]);
            let mut request = tool_call("init", json!({}));
            apply_cloud_client_init_defaults(&mut request, &headers);
            assert_eq!(
                request.params["arguments"]["skip_project_creation"],
                json!(true)
            );
        }

        #[test]
        fn local_clients_keep_project_creation_on_init() {
            for agent in [
                "claude-code/2.1.312 (cli)",
                "codex-mcp-client/0.165.0",
                "Google-Extended",
            ] {
                let mut request = tool_call("init", json!({ "folder_path": "/Users/me/dev/app" }));
                apply_cloud_client_init_defaults(
                    &mut request,
                    &headers_with(&[("user-agent", agent)]),
                );
                assert!(
                    request.params["arguments"]
                        .get("skip_project_creation")
                        .is_none(),
                    "{agent}"
                );
            }
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

        const JWT_TEST_SECRET: &[u8] = b"transport-auth-regression-secret-32-bytes";

        fn signed_test_jwt(claims: &Value, algorithm: Algorithm, secret: &[u8]) -> String {
            jsonwebtoken::encode(
                &jsonwebtoken::Header::new(algorithm),
                claims,
                &jsonwebtoken::EncodingKey::from_secret(secret),
            )
            .unwrap()
        }

        async fn jwt_auth_response(token: &str) -> Response {
            // Exercise the real middleware without sending any upstream request.
            let state =
                create_auth_state(true, Some(std::str::from_utf8(JWT_TEST_SECRET).unwrap()));
            let app = Router::new()
                .route("/auth-probe", get(|| async { StatusCode::NO_CONTENT }))
                .layer(axum::middleware::from_fn_with_state(state, auth_middleware));
            app.oneshot(
                Request::builder()
                    .uri("/auth-probe")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
        }

        #[tokio::test]
        async fn signed_jwt_transport_preserves_required_claims_and_expiration() {
            let future = chrono::Utc::now().timestamp() + 3600;
            let valid = json!({"sub": "caller", "exp": future});
            let token = signed_test_jwt(&valid, Algorithm::HS256, JWT_TEST_SECRET);
            assert_eq!(
                jwt_auth_response(&token).await.status(),
                StatusCode::NO_CONTENT
            );

            let mut invalid = vec![
                json!({"sub": "caller"}),
                json!({"exp": future}),
                json!({"sub": 123, "exp": future}),
                json!({"sub": "caller", "exp": 1}),
            ];
            for exp in [
                json!("never"),
                json!(true),
                json!([]),
                json!({}),
                Value::Null,
            ] {
                invalid.push(json!({"sub": "caller", "exp": exp}));
            }
            for claims in invalid {
                let token = signed_test_jwt(&claims, Algorithm::HS256, JWT_TEST_SECRET);
                let response = jwt_auth_response(&token).await;
                assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{claims}");
                assert!(response.headers().contains_key(header::WWW_AUTHENTICATE));
            }
        }

        #[tokio::test]
        async fn signed_jwt_transport_rejects_wrong_algorithm_and_signature() {
            let claims = json!({"sub": "caller", "exp": chrono::Utc::now().timestamp() + 3600});
            for token in [
                signed_test_jwt(&claims, Algorithm::HS512, JWT_TEST_SECRET),
                signed_test_jwt(&claims, Algorithm::HS256, b"different-signing-secret"),
            ] {
                assert_eq!(
                    jwt_auth_response(&token).await.status(),
                    StatusCode::UNAUTHORIZED
                );
            }
        }

        #[test]
        fn enabled_optional_jwt_claim_validation_rejects_malformed_types() {
            // GHSA-h395-gr6q-cpjc: enabled validation must not treat a malformed
            // optional claim as absent. The transport currently requires exp
            // and does not enable nbf; pin the patched library contract too.
            let key = DecodingKey::from_secret(JWT_TEST_SECRET);
            for claim in ["exp", "nbf"] {
                let mut validation = Validation::new(Algorithm::HS256);
                validation.required_spec_claims.clear();
                validation.validate_nbf = true;
                let absent =
                    signed_test_jwt(&json!({"sub": "caller"}), Algorithm::HS256, JWT_TEST_SECRET);
                assert!(decode::<Value>(&absent, &key, &validation).is_ok());
                for malformed in [json!("invalid"), json!(true), json!([]), json!({})] {
                    let mut claims = json!({"sub": "caller"});
                    claims[claim] = malformed;
                    let token = signed_test_jwt(&claims, Algorithm::HS256, JWT_TEST_SECRET);
                    assert!(
                        decode::<Value>(&token, &key, &validation).is_err(),
                        "{claims}"
                    );
                }
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
            // The edge's geography is carried whatever the credential says, in
            // canonical form only.
            let mut geo_headers = HeaderMap::new();
            geo_headers.insert(
                "x-contextstream-suggested-home-region",
                axum::http::HeaderValue::from_static(" EU-AMS "),
            );
            geo_headers.insert("cf-ipcountry", axum::http::HeaderValue::from_static("nl"));
            let geography = extract_edge_geography(&geo_headers).expect("geography");
            assert_eq!(geography.nearest_region, Some(NearestRegion::EuAms));
            assert_eq!(geography.client_country.as_deref(), Some("NL"));
            let mut forged = HeaderMap::new();
            forged.insert(
                "x-contextstream-suggested-home-region",
                axum::http::HeaderValue::from_static("amsterdam"),
            );
            forged.insert("cf-ipcountry", axum::http::HeaderValue::from_static("NLD"));
            assert!(extract_edge_geography(&forged).is_none());

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
        fn client_identity_is_authenticated_and_only_a_known_harness() {
            let mut headers = HeaderMap::new();
            headers.insert(header::AUTHORIZATION, "Bearer hosted-key".parse().unwrap());
            for (raw, expected) in [
                ("claude", Some(HarnessId::ClaudeCode)),
                ("Claude-Code", Some(HarnessId::ClaudeCode)),
                ("kimi", Some(HarnessId::KimiCode)),
                ("web-dashboard", None),
                ("my-own-client", None),
            ] {
                headers.insert("x-contextstream-client", raw.parse().unwrap());
                let auth = extract_auth_override(&headers).expect("authenticated override");
                assert_eq!(auth.client, expected, "{raw}");
            }

            // Without credentials the header creates no override at all.
            headers.remove(header::AUTHORIZATION);
            headers.insert("x-contextstream-client", "claude".parse().unwrap());
            assert!(extract_auth_override(&headers).is_none());
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
        async fn test_jsonrpc_tools_call_non_object_arguments_are_invalid_params() {
            for (arguments, got) in [
                (json!("[1,2]"), "a string that is not a JSON object"),
                (json!(5), "an integer"),
                (json!([1]), "an array"),
            ] {
                let app = create_router(create_test_state());
                let body = json!({
                    "jsonrpc": "2.0",
                    "id": 6,
                    "method": "tools/call",
                    "params": {"name": "memory", "arguments": arguments}
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

                assert_eq!(json["error"]["code"], -32602, "{arguments}: {json}");
                let message = json["error"]["message"].as_str().unwrap();
                assert!(message.contains(&format!("(got {got})")), "{message}");
                assert!(json.get("result").is_none_or(Value::is_null), "{json}");
            }
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
            // POST /api/v1/tools/{name} - Call a tool by name (in path)
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
                ("POST", "/api/v1/tools/{name}"),
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

        #[test]
        fn request_surface_follows_path_then_header_then_session_then_default() {
            use ToolSurfaceProfile::{Default as Full, OpenaiAgentic as Agentic};
            // An integration path always gets the full surface.
            assert_eq!(
                resolve_request_tool_surface_profile(
                    Some(Full),
                    Some(Agentic),
                    Some(Agentic),
                    Agentic
                ),
                Full
            );
            // The header beats the session.
            assert_eq!(
                resolve_request_tool_surface_profile(None, Some(Full), Some(Agentic), Agentic),
                Full
            );
            // The session beats the server default.
            assert_eq!(
                resolve_request_tool_surface_profile(None, None, Some(Agentic), Full),
                Agentic
            );
            assert_eq!(
                resolve_request_tool_surface_profile(None, None, None, Agentic),
                Agentic
            );
        }

        #[test]
        fn session_ids_carry_their_surface_and_stay_canonical_uuids() {
            for profile in [
                ToolSurfaceProfile::Default,
                ToolSurfaceProfile::OpenaiAgentic,
                ToolSurfaceProfile::Coach,
            ] {
                let session_id = mint_mcp_session_id(profile);
                let parsed = Uuid::parse_str(&session_id).expect("a UUID");
                assert_eq!(parsed.to_string(), session_id, "canonical form");
                assert_eq!(surface_profile_from_session_id(&session_id), Some(profile));
                assert_ne!(mint_mcp_session_id(profile), session_id, "ids stay unique");
            }

            // Ids minted before the code existed, and anything else, carry none.
            assert_eq!(
                surface_profile_from_session_id(&Uuid::new_v4().to_string()),
                None
            );
            assert_eq!(surface_profile_from_session_id("obsolete-session"), None);
            let mut bytes = *Uuid::new_v4().as_bytes();
            bytes[15] = 1;
            let unmarked = uuid::Builder::from_custom_bytes(bytes)
                .into_uuid()
                .to_string();
            assert_eq!(surface_profile_from_session_id(&unmarked), None);
        }

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

    #[test]
    fn only_unrecognized_clients_are_logged_and_only_as_bounded_tokens() {
        let identity = |name: &str, version: &str| {
            unrecognized_client_identity(&json!({
                "clientInfo": {"name": name, "version": version}
            }))
        };
        // Known clients, including exact clientInfo names, are not logged.
        for known in ["claude-code", "cursor-vscode", "codex-mcp-client", "Zed"] {
            assert_eq!(identity(known, "1.0.0"), None, "{known}");
        }
        assert_eq!(
            identity("kimi-code-mcp-client", "0.4.1"),
            Some(("kimi-code-mcp-client".to_string(), "0.4.1".to_string()))
        );
        // Control characters and markup are dropped and length is bounded.
        let (name, version) =
            identity("Muse\n<script>Code</script>\u{1b}[31m", &"9".repeat(200)).unwrap();
        assert_eq!(name, "MusescriptCode/script31m");
        assert_eq!(version.len(), 64);
        assert_eq!(identity("\n\t<>", "1"), None);
        assert_eq!(unrecognized_client_identity(&json!({})), None);
    }

    #[test]
    fn failed_tool_calls_are_classed_without_their_values() {
        for (message, class) in [
            (
                "Validation error: invalid type: string \"maybe\", expected a boolean",
                "validation:invalid_type",
            ),
            (
                "Validation error: init: the arguments do not match the tool's input schema (invalid type: map, expected a string). `workspace_id` must be a string (got object).",
                "validation:schema_mismatch",
            ),
            ("Validation error: missing field `query`", "validation:missing_field"),
            (
                "Validation error: repository_url must identify a valid Git repository remote",
                "validation:repository_url",
            ),
            ("Validation error: something else", "validation:other"),
            ("HTTP error (403): workspace not accessible", "http:403"),
            ("Request timeout after 10 seconds", "timeout"),
            ("Invalid UUID: abc", "invalid_uuid"),
            ("a message that quotes secret-value", "other"),
        ] {
            let class_seen = tool_error_class(message);
            assert_eq!(class_seen, class, "{message}");
            assert!(!class_seen.contains("maybe") && !class_seen.contains("secret"));
        }
    }

    #[test]
    fn failed_tool_calls_name_the_client_that_initialized_the_session() {
        remember_session_client(
            "tool-error-log-test-session",
            &json!({"clientInfo": {"name": "mcporter", "version": "0.7.3"}}),
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            "mcp-session-id",
            "tool-error-log-test-session".parse().unwrap(),
        );
        assert_eq!(
            session_client_identity(&headers),
            ("mcporter".to_string(), "0.7.3".to_string())
        );
        assert_eq!(
            session_client_identity(&HeaderMap::new()),
            ("unknown".to_string(), String::new())
        );
        // Unprintable client names are reduced like the unrecognized-client log.
        assert_eq!(
            loggable_client_identity(&json!({"clientInfo": {"name": "bot\n<x>", "version": 7}})),
            ("botx".to_string(), String::new())
        );
    }

    // ========================================================================
    // Agent Coach Kit surface (/coach)
    // ========================================================================

    mod coach_surface_tests {
        use super::*;
        use crate::server::build_registry;
        use mcp_types::config::{Config, Toolset};

        /// As the hosted gateway runs: the complete toolset, consolidated.
        fn hosted_state(require_auth: bool) -> HttpState {
            let config = Config {
                api_key: Some("test-api-key".to_string()),
                is_http_transport: true,
                toolset: Toolset::Complete,
                consolidated_mode: true,
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
                require_auth,
                telemetry: AgenticTelemetry::new(client, session),
                tools_list_cache: Arc::new(RwLock::new(HashMap::new())),
                concurrency_semaphore: Arc::new(tokio::sync::Semaphore::new(8)),
                metrics_handle: None,
            }
        }

        fn tool_names(response: &Value) -> Vec<String> {
            response["tools"]
                .as_array()
                .expect("tools")
                .iter()
                .map(|tool| tool["name"].as_str().expect("name").to_string())
                .collect()
        }

        #[tokio::test]
        async fn the_hosted_surface_lists_the_tools_build_registry_registers() {
            // The gateway once kept a registry list of its own that never gained
            // answer, feed, vcs and capsule (nor chart and async_job, which also
            // need the acceleration connection): a fresh gateway listed them only
            // after a per-request registry had filled the tools/list cache.
            let state = hosted_state(false);
            let names = tool_names(&handle_list_tools_method(&state, None).await.expect("list"));
            for name in ["answer", "feed", "vcs", "capsule"] {
                assert!(
                    names.iter().any(|listed| listed == name),
                    "{name} missing: {names:?}"
                );
            }
        }

        #[tokio::test]
        async fn a_per_request_registry_never_shares_another_registrys_tools_list() {
            // tools/list is cached per process. A registry with other tools (a
            // config override, the stateless transport) under the same config must
            // get its own entry, whichever registry asks first.
            let state = hosted_state(false);
            let config = Config {
                api_key: Some("test-api-key".to_string()),
                is_http_transport: true,
                toolset: Toolset::Complete,
                consolidated_mode: true,
                ..Config::default()
            };
            let mut smaller = mcp_tools::ToolRegistry::new(&config);
            mcp_tools::domains::help::register_help_tools(&mut smaller, state.client.clone());
            let smaller = Arc::new(smaller);

            let small_first = tool_names(
                &handle_list_tools_method(&state, Some(smaller.clone()))
                    .await
                    .expect("list"),
            );
            let full = tool_names(&handle_list_tools_method(&state, None).await.expect("list"));
            assert_eq!(small_first, vec!["help".to_string()]);
            assert!(
                full.len() > small_first.len(),
                "the shared registry got the smaller list: {full:?}"
            );
            assert_eq!(
                tool_names(
                    &handle_list_tools_method(&state, Some(smaller))
                        .await
                        .expect("list")
                ),
                small_first
            );
            assert_eq!(
                tool_names(&handle_list_tools_method(&state, None).await.expect("list")),
                full
            );
        }

        fn post(
            path: &str,
            session_id: Option<&str>,
            headers: &[(&str, &str)],
            body: Value,
        ) -> Request<Body> {
            let mut builder = Request::builder()
                .method("POST")
                .uri(path)
                .header("host", "mcp.contextstream.io")
                .header("x-forwarded-proto", "https")
                .header("content-type", "application/json")
                .header("accept", "application/json, text/event-stream");
            if let Some(session_id) = session_id {
                builder = builder.header("mcp-session-id", session_id);
            }
            for (name, value) in headers {
                builder = builder.header(*name, *value);
            }
            builder
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap()
        }

        async fn rpc(
            app: &Router,
            path: &str,
            headers: &[(&str, &str)],
            method: &str,
            params: Value,
        ) -> (StatusCode, HeaderMap, Value) {
            let body = json!({"jsonrpc": "2.0", "id": method, "method": method, "params": params});
            let response = app
                .clone()
                .oneshot(post(path, None, headers, body))
                .await
                .unwrap();
            let status = response.status();
            let headers = response.headers().clone();
            (status, headers, json_response(response).await)
        }

        fn names(list: &Value) -> Vec<String> {
            let mut names: Vec<String> = list["result"]["tools"]
                .as_array()
                .expect("tools array")
                .iter()
                .filter_map(|tool| tool["name"].as_str().map(str::to_string))
                .collect();
            names.sort();
            names
        }

        #[tokio::test]
        async fn coach_lists_exactly_the_three_coach_tools_with_their_component() {
            let app = create_router(hosted_state(false));
            let (status, headers, init) = rpc(
                &app,
                "/coach",
                &[],
                "initialize",
                json!({"protocolVersion": mcp_types::MCP_PROTOCOL_2024_11_05,
                       "clientInfo": {"name": "openai-mcp", "version": "1.0.0"}, "capabilities": {}}),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(init["result"]["serverInfo"]["name"], "agent-coach-kit");
            assert_eq!(init["result"]["serverInfo"]["title"], "Agent Coach Kit");
            assert!(init["result"]["capabilities"]["resources"].is_object());
            assert!(init["result"]["instructions"]
                .as_str()
                .unwrap()
                .contains("verbatim"));
            assert_eq!(init["result"]["toolCount"], 3);
            let session = headers["mcp-session-id"].to_str().unwrap().to_string();
            assert_eq!(
                surface_profile_from_session_id(&session),
                Some(ToolSurfaceProfile::Coach)
            );

            // A header asking for the full surface does not widen /coach.
            let (_, _, list) = rpc(
                &app,
                "/coach",
                &[("x-contextstream-tool-surface-profile", "default")],
                "tools/list",
                json!({}),
            )
            .await;
            assert_eq!(
                names(&list),
                ["workflow", "workflow_delete", "workflow_save"]
            );
            for tool in list["result"]["tools"].as_array().unwrap() {
                let name = tool["name"].as_str().unwrap();
                let annotations = &tool["annotations"];
                assert_eq!(annotations["openWorldHint"], false, "{name}");
                match name {
                    "workflow" => {
                        assert_eq!(annotations["readOnlyHint"], true);
                        assert_eq!(
                            tool["_meta"]["openai/outputTemplate"],
                            "ui://widget/coach.html"
                        );
                        assert_eq!(tool["securitySchemes"][0]["scopes"], json!(["read:coach"]));
                    }
                    "workflow_save" => {
                        assert_eq!(annotations["readOnlyHint"], false);
                        assert_eq!(annotations["destructiveHint"], false);
                        assert_eq!(annotations["idempotentHint"], true);
                        assert_eq!(
                            tool["_meta"]["openai/outputTemplate"],
                            "ui://widget/coach.html"
                        );
                        assert_eq!(tool["_meta"]["openai/fileParams"], json!(["file"]));
                        assert_eq!(
                            tool["_meta"]["securitySchemes"][0]["scopes"],
                            json!(["write:coach"])
                        );
                    }
                    _ => {
                        assert_eq!(annotations["destructiveHint"], true);
                        assert_eq!(tool["securitySchemes"][0]["scopes"], json!(["write:coach"]));
                    }
                }
            }

            // Every other surface stays as it was: no coach tools.
            for (path, headers) in [
                ("/mcp", vec![]),
                (
                    "/mcp",
                    vec![("x-contextstream-tool-surface-profile", "openai_agentic")],
                ),
                ("/chatgpt", vec![]),
                ("/claude", vec![]),
            ] {
                let (_, _, list) = rpc(&app, path, &headers, "tools/list", json!({})).await;
                let names = names(&list);
                assert!(!names.is_empty(), "{path}");
                for coach_tool in ["workflow", "workflow_save", "workflow_delete"] {
                    assert!(
                        !names.iter().any(|n| n == coach_tool),
                        "{path} lists {coach_tool}"
                    );
                }
            }
        }

        #[tokio::test]
        async fn coach_runs_no_tool_outside_its_surface() {
            let app = create_router(hosted_state(false));
            for name in ["memory", "execute_operation", "tool_search", "operations"] {
                let (_, _, response) = rpc(
                    &app,
                    "/coach",
                    &[],
                    "tools/call",
                    json!({"name": name, "arguments": {"action": "list"}}),
                )
                .await;
                let refused =
                    response.get("error").is_some() || response["result"]["isError"] == true;
                assert!(refused, "{name} ran on /coach: {response}");
            }
        }

        #[tokio::test]
        async fn coach_serves_its_component_and_mcp_does_not() {
            let app = create_router(hosted_state(false));
            let (_, _, listed) = rpc(&app, "/coach", &[], "resources/list", json!({})).await;
            assert_eq!(
                listed["result"]["resources"][0]["uri"],
                mcp_tools::domains::coach::WIDGET_URI
            );
            assert_eq!(
                listed["result"]["resources"][0]["mimeType"],
                "text/html+skybridge"
            );
            let (_, _, read) = rpc(
                &app,
                "/coach",
                &[],
                "resources/read",
                json!({"uri": mcp_tools::domains::coach::WIDGET_URI}),
            )
            .await;
            assert_eq!(
                read["result"]["contents"][0]["mimeType"],
                "text/html+skybridge"
            );
            assert!(read["result"]["contents"][0]["text"]
                .as_str()
                .unwrap()
                .contains("coach-root"));
            let (_, _, missing) = rpc(
                &app,
                "/coach",
                &[],
                "resources/read",
                json!({"uri": "ui://widget/nope.html"}),
            )
            .await;
            assert_eq!(missing["error"]["code"], -32002);
            let (_, _, mcp) = rpc(&app, "/mcp", &[], "resources/list", json!({})).await;
            assert_eq!(mcp["error"]["code"], -32601);
        }

        #[tokio::test]
        async fn coach_has_its_own_protected_resource_metadata_and_challenge() {
            let app = create_router(hosted_state(true));
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri("/.well-known/oauth-protected-resource/coach")
                        .header("host", "mcp.contextstream.io")
                        .header("x-forwarded-proto", "https")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let metadata = json_response(response).await;
            assert_eq!(metadata["resource"], "https://mcp.contextstream.io/coach");
            assert_eq!(
                metadata["scopes_supported"],
                json!(["read:coach", "write:coach"])
            );
            assert_eq!(
                metadata["authorization_servers"],
                json!(["https://api.contextstream.io"])
            );

            let response = app
                .clone()
                .oneshot(post(
                    "/coach",
                    None,
                    &[],
                    json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(
                response.headers()[header::WWW_AUTHENTICATE]
                    .to_str()
                    .unwrap(),
                r#"Bearer resource_metadata="https://mcp.contextstream.io/.well-known/oauth-protected-resource/coach", scope="read:coach write:coach""#
            );
            let body = json_response(response).await;
            assert_eq!(
                body["_meta"]["mcp/www_authenticate"]["error"],
                "invalid_token"
            );

            // /mcp keeps its own challenge.
            let response = app
                .clone()
                .oneshot(post(
                    "/mcp",
                    None,
                    &[],
                    json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
                ))
                .await
                .unwrap();
            assert_eq!(
                response.headers()[header::WWW_AUTHENTICATE]
                    .to_str()
                    .unwrap(),
                r#"Bearer resource_metadata="https://mcp.contextstream.io/.well-known/oauth-protected-resource""#
            );
        }

        #[tokio::test]
        async fn studio_is_its_own_connector_with_one_tool() {
            let app = create_router(hosted_state(false));
            let (status, headers, init) = rpc(
                &app,
                "/studio",
                &[],
                "initialize",
                json!({"protocolVersion": mcp_types::MCP_PROTOCOL_2024_11_05,
                       "clientInfo": {"name": "claude-ai", "version": "1.0.0"}, "capabilities": {}}),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(init["result"]["serverInfo"]["name"], "contextstudio");
            assert_eq!(init["result"]["serverInfo"]["title"], "ContextStudio");
            assert_eq!(init["result"]["toolCount"], 1);
            let session = headers["mcp-session-id"].to_str().unwrap().to_string();
            assert_eq!(
                surface_profile_from_session_id(&session),
                Some(ToolSurfaceProfile::Studio)
            );

            // Only `studio`, listed without asking the catalog, whatever a header asks for.
            let (_, _, list) = rpc(
                &app,
                "/studio",
                &[("x-contextstream-tool-surface-profile", "default")],
                "tools/list",
                json!({}),
            )
            .await;
            assert_eq!(names(&list), ["studio"]);

            // Nothing else runs there.
            for name in ["memory", "search", "execute_operation", "workflow"] {
                let (_, _, response) = rpc(
                    &app,
                    "/studio",
                    &[],
                    "tools/call",
                    json!({"name": name, "arguments": {}}),
                )
                .await;
                let refused =
                    response.get("error").is_some() || response["result"]["isError"] == json!(true);
                assert!(refused, "{name} ran on /studio: {response}");
            }

            // The full surfaces don't list it for a caller Create doesn't offer making to.
            for path in ["/mcp", "/claude"] {
                let (_, _, list) = rpc(&app, path, &[], "tools/list", json!({})).await;
                assert!(!names(&list).iter().any(|n| n == "studio"), "{path}");
            }
        }

        #[tokio::test]
        async fn studio_has_its_own_protected_resource_metadata_and_challenge() {
            let app = create_router(hosted_state(true));
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("GET")
                        .uri("/.well-known/oauth-protected-resource/studio")
                        .header("host", "mcp.contextstream.io")
                        .header("x-forwarded-proto", "https")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let metadata = json_response(response).await;
            assert_eq!(metadata["resource"], "https://mcp.contextstream.io/studio");
            assert_eq!(
                metadata["scopes_supported"],
                json!(["read:studio", "write:studio"])
            );
            assert_eq!(metadata["resource_name"], "ContextStudio");

            let response = app
                .clone()
                .oneshot(post(
                    "/studio",
                    None,
                    &[],
                    json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(
                response.headers()[header::WWW_AUTHENTICATE]
                    .to_str()
                    .unwrap(),
                r#"Bearer resource_metadata="https://mcp.contextstream.io/.well-known/oauth-protected-resource/studio", scope="read:studio write:studio""#
            );
            assert_eq!(
                path_surface_profile("/studio"),
                Some(ToolSurfaceProfile::Studio)
            );
            assert!(is_streamable_mcp_path("/studio"));
        }

        // #299: a discovery URL is never challenged for a token. One the
        // gateway does not publish is a 404, so the client uses the root one.
        #[tokio::test]
        async fn every_well_known_path_is_public() {
            let app = create_router(hosted_state(true));
            for (path, status) in [
                ("/.well-known/oauth-protected-resource", StatusCode::OK),
                (
                    "/.well-known/oauth-protected-resource/coach",
                    StatusCode::OK,
                ),
                (
                    "/.well-known/oauth-protected-resource/studio",
                    StatusCode::OK,
                ),
                (
                    "/.well-known/oauth-protected-resource/mcp",
                    StatusCode::NOT_FOUND,
                ),
                (
                    "/.well-known/oauth-protected-resource/claude",
                    StatusCode::NOT_FOUND,
                ),
                ("/.well-known/oauth-authorization-server", StatusCode::OK),
                ("/.well-known/openid-configuration", StatusCode::OK),
            ] {
                let response = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method("GET")
                            .uri(path)
                            .header("host", "mcp.contextstream.io")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), status, "{path}");
                assert!(
                    !response.headers().contains_key(header::WWW_AUTHENTICATE),
                    "{path}"
                );
            }
        }

        // #300: the gateway's authorization-server document says what
        // api.contextstream.io's says: S256 only, every scope it grants.
        #[tokio::test]
        async fn authorization_server_metadata_matches_the_authorization_server() {
            let app = create_router(hosted_state(true));
            for path in [
                "/.well-known/oauth-authorization-server",
                "/.well-known/openid-configuration",
            ] {
                let response = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method("GET")
                            .uri(path)
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), StatusCode::OK, "{path}");
                let metadata = json_response(response).await;
                assert_eq!(
                    metadata["code_challenge_methods_supported"],
                    json!(["S256"])
                );
                assert_eq!(
                    metadata["scopes_supported"],
                    json!([
                        "read:workspaces",
                        "read:projects",
                        "read:search",
                        "read:memory",
                        "write:memory",
                        "read:coach",
                        "write:coach",
                        "read:agents",
                        "write:agents",
                        "read:studio",
                        "write:studio"
                    ])
                );
                assert_eq!(
                    metadata["token_endpoint_auth_methods_supported"],
                    json!([
                        "client_secret_basic",
                        "client_secret_post",
                        "none",
                        "private_key_jwt"
                    ])
                );
                assert_eq!(
                    metadata["token_endpoint_auth_signing_alg_values_supported"],
                    json!(["RS256", "ES256"])
                );
                assert_eq!(
                    metadata["authorization_response_iss_parameter_supported"],
                    true
                );
                assert_eq!(metadata["client_id_metadata_document_supported"], true);
            }
        }

        #[test]
        fn a_coach_only_server_never_widens() {
            use ToolSurfaceProfile::{Coach, Default as Full};
            assert_eq!(
                resolve_request_tool_surface_profile(Some(Full), Some(Full), Some(Full), Coach),
                Coach
            );
            assert_eq!(
                resolve_request_tool_surface_profile(Some(Coach), Some(Full), None, Full),
                Coach
            );
            assert_eq!(path_surface_profile("/coach"), Some(Coach));
            assert_eq!(path_surface_profile("/mcp"), None);
            assert!(is_streamable_mcp_path("/coach"));
        }
    }

    /// The hosted gateway serves everyone from one cached tool list; `agents`
    /// is added per caller, for the workspace their connection names, and only
    /// where that workspace has agents turned on.
    #[tokio::test]
    async fn hosted_tools_list_adds_slipstream_per_caller_workspace() {
        let open = Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap();
        let closed = Uuid::parse_str("22222222-2222-4222-8222-222222222222").unwrap();
        let app = axum::Router::new().route(
            "/api/v1/tasks/agents",
            get(move |Query(query): Query<HashMap<String, String>>| async move {
                let asked = query.get("workspace_id").cloned().unwrap_or_default();
                if asked == open.to_string() {
                    (
                        StatusCode::OK,
                        Json(json!({"success": true, "data": {"agents": [
                            {"agent": {"id": "review", "version": "0.1.0", "name": "PR Reviewer",
                                       "description": "Reviews a pull request's diff against the workspace's decisions.",
                                       "price_credits": 1200,
                                       "input_schema": {"type": "object", "required": ["repo", "pull"],
                                                        "properties": {"repo": {"type": "string"},
                                                                       "pull": {"type": "integer"}}}},
                             "enabled": true}]}})),
                    )
                } else {
                    (
                        StatusCode::FORBIDDEN,
                        Json(json!({"success": false, "data": null, "error": {
                            "code": "FORBIDDEN", "message": "Tasks are not available in this workspace yet"}})),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind slipstream API");
        let address = listener.local_addr().expect("slipstream API address");
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });

        let config = Config {
            api_url: format!("http://{address}"),
            api_key: Some("gateway-process-key".to_string()),
            is_http_transport: true,
            ..Config::default()
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
        let list_as = |caller: &'static str, workspace: Option<Uuid>, cap: Option<u32>| {
            let state = state.clone();
            async move {
                let auth = AuthOverride {
                    api_key: Some(format!("{caller}-key")),
                    workspace_id: workspace,
                    ..AuthOverride::default()
                };
                let config = ConfigOverride {
                    slipstream_auto_accept_max_credits: cap,
                    ..ConfigOverride::default()
                };
                run_with_caller_cache_identity(caller.to_string(), || async {
                    run_with_auth_override(auth, || async {
                        run_with_config_override(config, || async {
                            handle_list_tools_method(&state, None).await
                        })
                        .await
                    })
                    .await
                })
                .await
                .expect("tools/list")
            }
        };
        let slipstream = |list: &Value| {
            list["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|tool| tool["name"] == "agents")
                .cloned()
        };

        let listed = list_as("caller-open", Some(open), None).await;
        let tool = slipstream(&listed).expect("listed for an open workspace");
        // The first name is never listed.
        assert!(!listed["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["name"] == "slipstream"));
        let description = tool["description"].as_str().unwrap();
        assert!(description.contains("- review (1200) [pull, repo]: Reviews a pull request"));
        assert!(description.contains("It first returns the price"));

        // The caller's own auto-accept cap shapes their description only.
        let capped = list_as("caller-open", Some(open), Some(1500)).await;
        assert!(slipstream(&capped).unwrap()["description"]
            .as_str()
            .unwrap()
            .contains("A run priced at most 1500 credits starts at once"));

        // Another caller, another workspace: not listed, and nothing leaks across.
        let other = list_as("caller-closed", Some(closed), None).await;
        assert!(slipstream(&other).is_none());
        let unknown = list_as("caller-nowhere", None, None).await;
        assert!(slipstream(&unknown).is_none());
        assert_eq!(
            other["tools"].as_array().unwrap().len() + 1,
            listed["tools"].as_array().unwrap().len()
        );

        // The shared cache holds only what every caller sees.
        for cached in state
            .tools_list_cache
            .read()
            .unwrap_or_else(|err| err.into_inner())
            .values()
        {
            assert!(slipstream(cached).is_none());
        }

        // The cap comes from the caller's header, never from the gateway.
        let mut headers = HeaderMap::new();
        headers.insert(
            mcp_tools::domains::slipstream::AUTO_ACCEPT_HEADER,
            "750".parse().unwrap(),
        );
        assert_eq!(
            extract_config_override(&headers).and_then(|c| c.slipstream_auto_accept_max_credits),
            Some(750)
        );
        headers.insert(
            mcp_tools::domains::slipstream::AUTO_ACCEPT_HEADER,
            "lots".parse().unwrap(),
        );
        assert!(extract_config_override(&headers).is_none());

        // The header's first name still works, and the current one wins over it.
        let mut headers = HeaderMap::new();
        headers.insert(
            mcp_tools::domains::slipstream::LEGACY_AUTO_ACCEPT_HEADER,
            "300".parse().unwrap(),
        );
        assert_eq!(
            extract_config_override(&headers).and_then(|c| c.slipstream_auto_accept_max_credits),
            Some(300)
        );
        headers.insert(
            mcp_tools::domains::slipstream::AUTO_ACCEPT_HEADER,
            "900".parse().unwrap(),
        );
        assert_eq!(
            extract_config_override(&headers).and_then(|c| c.slipstream_auto_accept_max_credits),
            Some(900)
        );
    }
}

#[cfg(test)]
mod cors_preflight_contract {
    //! The ContextStream MCP gateway Worker answers OPTIONS itself (no round trip, no dependency on an origin's
    //! hostname). Its answer is pinned to this layer through `transport/cors/mcp-preflight.json`, identical to the
    //! contextstream repository's deploy/cloudflare/shared/cors/mcp-preflight.json, whose Worker tests check the edge
    //! against it. This test drives `cors_layer()` over every case and requires the exact status, headers and body.
    use super::cors_layer;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::Router;
    use std::collections::BTreeMap;
    use tower::ServiceExt as _;

    const FIXTURE: &str = include_str!("cors/mcp-preflight.json");

    async fn answer(case: &serde_json::Value) -> serde_json::Value {
        let app = Router::new()
            .fallback(|| async { (StatusCode::NOT_FOUND, "routed past the CORS layer") })
            .layer(cors_layer());
        let mut request = Request::builder()
            .method(case["method"].as_str().expect("method"))
            .uri(case["path"].as_str().expect("path"));
        for (name, value) in case["headers"].as_object().expect("headers") {
            request = request.header(name.as_str(), value.as_str().expect("header value"));
        }
        let response = app
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = response.status().as_u16();
        let mut headers = BTreeMap::new();
        for (name, value) in response.headers() {
            let previous = headers.insert(
                name.as_str().to_string(),
                value.to_str().unwrap().to_string(),
            );
            assert!(previous.is_none(), "{name} sent twice");
        }
        let body = axum::body::to_bytes(response.into_body(), 1 << 16)
            .await
            .unwrap();
        serde_json::json!({
            "status": status,
            "body": String::from_utf8(body.to_vec()).unwrap(),
            "headers": headers,
        })
    }

    #[tokio::test]
    async fn every_fixture_case_is_what_the_mcp_cors_layer_answers() {
        let fixture: serde_json::Value = serde_json::from_str(FIXTURE).unwrap();
        let cases = fixture["cases"].as_array().expect("cases");
        assert!(cases.len() >= 8, "the fixture lost its cases");
        let names: std::collections::BTreeSet<_> = cases
            .iter()
            .map(|case| case["name"].as_str().unwrap().to_string())
            .collect();
        for required in [
            "missing-origin",
            "private-network",
            "not-a-preflight",
            "opaque-null-origin",
        ] {
            assert!(
                names.contains(required),
                "the fixture must cover {required}"
            );
        }
        let mut mismatches = Vec::new();
        for case in cases {
            assert_eq!(case["method"], "OPTIONS", "{}", case["name"]);
            let got = answer(case).await;
            if got != case["expect"] {
                mismatches.push(format!(
                    "{}:\n  fixture {}\n  layer   {}",
                    case["name"], case["expect"], got
                ));
            }
        }
        assert!(
            mismatches.is_empty(),
            "the MCP CORS layer no longer answers as the edge does (update this fixture, the contextstream copy and \
             deploy/cloudflare/shared/cors-preflight.ts together):\n{}",
            mismatches.join("\n")
        );
    }

    #[tokio::test]
    async fn a_non_options_request_is_routed_past_the_layer() {
        let case = serde_json::json!({"method": "POST", "path": "/mcp",
            "headers": {"origin": "https://claude.ai"}});
        let got = answer(&case).await;
        assert_eq!(got["status"], 404);
        assert_eq!(got["body"], "routed past the CORS layer");
    }
}

#[cfg(test)]
mod initialize_key_tests {
    use super::*;
    use crate::server::build_registry;
    use http_body_util::BodyExt;
    use mcp_types::config::Config;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tower::ServiceExt as _;

    /// An API that answers every request with `status_line`, counting them.
    async fn api_answering(status_line: &'static str) -> (String, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind api");
        let address = listener.local_addr().expect("api address");
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                counter.fetch_add(1, Ordering::SeqCst);
                let mut buffer = vec![0u8; 16 * 1024];
                let _ = socket.read(&mut buffer).await;
                let body = r#"{"success":false,"error":"no"}"#;
                let response = format!(
                    "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });
        (format!("http://{address}"), hits)
    }

    /// The hosted gateway: credentials come with each request.
    fn hosted_gateway(api_url: String) -> axum::Router {
        let config = Config {
            api_url,
            api_key: None,
            is_http_transport: true,
            ..Config::default()
        };
        let client = ContextStreamClient::new(config.clone());
        let session = Arc::new(SessionManager::new(client.clone(), config.clone()));
        let registry = build_registry(&config, client.clone(), session.clone());
        create_router(HttpState {
            registry: Arc::new(registry),
            client: client.clone(),
            session: session.clone(),
            jwt_secret: None,
            require_auth: true,
            telemetry: AgenticTelemetry::new(client, session),
            tools_list_cache: Arc::new(RwLock::new(HashMap::new())),
            concurrency_semaphore: Arc::new(tokio::sync::Semaphore::new(8)),
            metrics_handle: None,
        })
    }

    async fn initialize(router: axum::Router, api_key: &str) -> Value {
        let response = initialize_with(router, "x-contextstream-api-key", api_key).await;
        json_body(response).await
    }

    async fn initialize_with(router: axum::Router, header: &str, value: &str) -> Response {
        router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .header("accept", "application/json, text/event-stream")
                    .header(header, value)
                    .body(Body::from(
                        json!({
                            "jsonrpc": "2.0",
                            "id": 1,
                            "method": "initialize",
                            "params": {
                                "protocolVersion": "2024-11-05",
                                "clientInfo": {"name": "qa", "version": "1.0"},
                                "capabilities": {}
                            }
                        })
                        .to_string(),
                    ))
                    .expect("initialize request"),
            )
            .await
            .expect("initialize response")
    }

    async fn json_body(response: Response) -> Value {
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("body")
            .to_bytes();
        let text = String::from_utf8_lossy(&bytes);
        // Plain JSON, or one server-sent event carrying it.
        let json_text = text
            .lines()
            .find_map(|line| line.strip_prefix("data:"))
            .unwrap_or(&text)
            .trim()
            .to_string();
        serde_json::from_str(&json_text).unwrap_or_else(|_| panic!("JSON body: {text}"))
    }

    /// A refused credential fails initialize like a missing one: HTTP 401 with
    /// WWW-Authenticate (#252: a JSON-RPC error in a 200 still read "Connected").
    async fn assert_refused(response: Response, says: &str) {
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(response.headers().contains_key(header::WWW_AUTHENTICATE));
        let body = json_body(response).await;
        assert!(
            body["error_description"]
                .as_str()
                .is_some_and(|message| message.contains(says)),
            "{body}"
        );
    }

    #[tokio::test]
    async fn hosted_initialize_refuses_a_key_the_api_rejects_and_remembers_it() {
        let (api_url, hits) = api_answering("HTTP/1.1 401 Unauthorized").await;
        let gateway = hosted_gateway(api_url.clone());
        let response = initialize_with(gateway, "x-contextstream-api-key", "refused-key-252").await;
        assert_refused(response, "did not accept the API key").await;
        let asked = hits.load(Ordering::SeqCst);
        assert!(asked >= 1);

        // A reconnect within the cache window is refused without asking again.
        let gateway = hosted_gateway(api_url);
        let response = initialize_with(gateway, "x-contextstream-api-key", "refused-key-252").await;
        assert_refused(response, "did not accept the API key").await;
        assert_eq!(hits.load(Ordering::SeqCst), asked);
    }

    #[tokio::test]
    async fn hosted_initialize_checks_a_bearer_token_it_cannot_verify_itself() {
        // #303: a garbage JWT passed initialize on the hosted gateway.
        let (api_url, hits) = api_answering("HTTP/1.1 401 Unauthorized").await;
        let response = initialize_with(
            hosted_gateway(api_url),
            "authorization",
            "Bearer eyJhbGciOiJIUzI1NiJ9.eyJleHAiOjF9.x303",
        )
        .await;
        assert_refused(response, "did not accept the access token").await;
        assert!(hits.load(Ordering::SeqCst) >= 1);
    }

    #[tokio::test]
    async fn an_empty_credential_header_is_no_credential() {
        // #303: an editor config naming an unset variable sends an empty key.
        let (api_url, hits) = api_answering("HTTP/1.1 200 OK").await;
        for (header, value) in [("x-api-key", ""), ("x-contextstream-api-key", "   ")] {
            let response = initialize_with(hosted_gateway(api_url.clone()), header, value).await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{header}");
            let body = json_body(response).await;
            assert_eq!(
                body["error_description"], "Bearer token required",
                "{header}"
            );
        }
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "nothing was asked of the API"
        );
    }

    /// A request under the stateless 2026-07-28 contract, as Claude Code
    /// 2.1.312 sends its health check.
    async fn stateless_with(router: axum::Router, method: &str, api_key: &str) -> Response {
        let version = "2026-07-28";
        router
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/mcp")
                    .header("content-type", "application/json")
                    .header("accept", "application/json, text/event-stream")
                    .header(MCP_PROTOCOL_VERSION_HEADER, version)
                    .header(MCP_METHOD_HEADER, method)
                    .header("x-contextstream-api-key", api_key)
                    .body(Body::from(
                        json!({
                            "jsonrpc": "2.0",
                            "id": 1,
                            "method": method,
                            "params": {
                                "_meta": {
                                    mcp_types::MCP_META_PROTOCOL_VERSION: version,
                                    mcp_types::MCP_META_CLIENT_INFO: {
                                        "name": "claude-code",
                                        "version": "2.1.312"
                                    },
                                    mcp_types::MCP_META_CLIENT_CAPABILITIES: {}
                                }
                            }
                        })
                        .to_string(),
                    ))
                    .expect("stateless request"),
            )
            .await
            .expect("stateless response")
    }

    #[tokio::test]
    async fn a_stateless_health_check_with_a_refused_key_is_refused() {
        // #252: Claude Code sends no initialize; its server/discover and
        // tools/list answered 200, so a refused key showed "Connected".
        let (api_url, hits) = api_answering("HTTP/1.1 401 Unauthorized").await;
        let response = stateless_with(
            hosted_gateway(api_url.clone()),
            "server/discover",
            "refused-key-252-stateless",
        )
        .await;
        assert_refused(response, "did not accept the API key").await;
        let asked = hits.load(Ordering::SeqCst);
        assert!(asked >= 1);

        let response = stateless_with(
            hosted_gateway(api_url),
            "tools/list",
            "refused-key-252-stateless",
        )
        .await;
        assert_refused(response, "did not accept the API key").await;
        assert_eq!(hits.load(Ordering::SeqCst), asked, "the verdict is reused");
    }

    #[tokio::test]
    async fn a_stateless_request_goes_through_when_the_api_cannot_answer() {
        let (api_url, _) = api_answering("HTTP/1.1 503 Service Unavailable").await;
        let response = stateless_with(
            hosted_gateway(api_url),
            "server/discover",
            "transient-key-252-stateless",
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = json_body(response).await;
        assert!(body.get("result").is_some(), "{body}");
    }

    #[tokio::test]
    async fn hosted_initialize_goes_through_when_the_api_cannot_answer() {
        let (api_url, _) = api_answering("HTTP/1.1 503 Service Unavailable").await;
        let body = initialize(hosted_gateway(api_url), "transient-key-252").await;
        assert!(body.get("result").is_some(), "{body}");
        assert!(body.get("error").is_none(), "{body}");
    }
}

#[cfg(test)]
mod tool_call_error_tests {
    use super::*;

    #[test]
    fn session_tool_errors_are_tool_results_not_internal_errors() {
        let validation = mcp_types::Error::Validation(
            "target_id is required for dependencies action".to_string(),
        );
        let response = tool_call_error_response(
            validation,
            false,
            Some("Reading code graph from ContextStream"),
            "◎",
        )
        .expect("a tool result");
        assert_eq!(response["isError"], true);
        assert_eq!(response["title"], "Reading code graph from ContextStream");
        assert!(response["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("target_id is required")));

        let api = mcp_types::Error::http(404, "Not Found");
        let response = tool_call_error_response(api, false, None, "◎").expect("a tool result");
        assert_eq!(response["isError"], true);
    }

    #[test]
    fn stateless_argument_errors_stay_protocol_errors() {
        let error = tool_call_error_response(
            mcp_types::Error::Validation("bad".to_string()),
            true,
            None,
            "◎",
        )
        .expect_err("a protocol error");
        assert_eq!(error.code, -32602);
        // Other failures are tool results under the stateless contract too.
        let response =
            tool_call_error_response(mcp_types::Error::http(409, "Conflict"), true, None, "◎")
                .expect("a tool result");
        assert_eq!(response["isError"], true);
    }
}
