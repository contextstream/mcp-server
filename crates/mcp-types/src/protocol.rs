//! Shared MCP wire-contract helpers.
//!
//! The 2026-07-28 revision is a stateless protocol era: every request carries
//! its protocol version and client capabilities, and every successful result
//! carries a result type plus server identity.  Keeping those rules here makes
//! the stdio and Streamable HTTP adapters advance as one compatibility unit.

use serde_json::{json, Map, Value};

use crate::harness_teaching::MCP_PROTOCOL_2026_07_28;

pub const MCP_META_PROTOCOL_VERSION: &str = "io.modelcontextprotocol/protocolVersion";
pub const MCP_META_CLIENT_INFO: &str = "io.modelcontextprotocol/clientInfo";
pub const MCP_META_CLIENT_CAPABILITIES: &str = "io.modelcontextprotocol/clientCapabilities";
pub const MCP_META_SERVER_INFO: &str = "io.modelcontextprotocol/serverInfo";

pub const MCP_ERROR_HEADER_MISMATCH: i32 = -32020;
pub const MCP_ERROR_MISSING_REQUIRED_CLIENT_CAPABILITY: i32 = -32021;
pub const MCP_ERROR_UNSUPPORTED_PROTOCOL_VERSION: i32 = -32022;

pub const MCP_DISCOVERY_TTL_MS: u64 = 300_000;
pub const MCP_TOOLS_LIST_TTL_MS: u64 = 60_000;

pub const MCP_STATELESS_SUPPORTED_VERSIONS: &[&str] = &[MCP_PROTOCOL_2026_07_28];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpCacheScope {
    Public,
    Private,
}

impl McpCacheScope {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Private => "private",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct StatelessRequestMetadata {
    pub protocol_version: String,
    pub client_info: Option<Value>,
    pub client_capabilities: Value,
}

impl StatelessRequestMetadata {
    pub fn client_name(&self) -> Option<&str> {
        self.client_info
            .as_ref()
            .and_then(Value::as_object)
            .and_then(|value| value.get("name"))
            .and_then(Value::as_str)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct McpProtocolError {
    pub code: i32,
    pub message: String,
    pub data: Option<Value>,
}

impl McpProtocolError {
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self {
            code: -32600,
            message: message.into(),
            data: None,
        }
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: message.into(),
            data: None,
        }
    }

    pub fn header_mismatch(message: impl Into<String>) -> Self {
        Self {
            code: MCP_ERROR_HEADER_MISMATCH,
            message: message.into(),
            data: None,
        }
    }

    pub fn unsupported_version(requested: impl Into<String>) -> Self {
        let requested = requested.into();
        Self {
            code: MCP_ERROR_UNSUPPORTED_PROTOCOL_VERSION,
            message: "Unsupported protocol version".to_string(),
            data: Some(json!({
                "supported": MCP_STATELESS_SUPPORTED_VERSIONS,
                "requested": requested,
            })),
        }
    }
}

/// Validate the JSON-RPC envelope shared by all stateless-era requests.
pub fn validate_stateless_jsonrpc_envelope(
    jsonrpc: &str,
    id: Option<&Value>,
    method: &str,
) -> Result<(), McpProtocolError> {
    if jsonrpc != "2.0" {
        return Err(McpProtocolError::invalid_request(
            "jsonrpc must be exactly '2.0'",
        ));
    }
    if method.is_empty() {
        return Err(McpProtocolError::invalid_request(
            "method must be a non-empty string",
        ));
    }
    if !id.is_some_and(|id| id.is_string() || id.is_number()) {
        return Err(McpProtocolError::invalid_request(
            "stateless MCP requests require a string or number id",
        ));
    }
    Ok(())
}

/// Return the per-request protocol version when the stateless metadata key is
/// present and string-valued.
pub fn stateless_protocol_version(params: &Value) -> Option<&str> {
    params
        .get("_meta")
        .and_then(Value::as_object)
        .and_then(|meta| meta.get(MCP_META_PROTOCOL_VERSION))
        .and_then(Value::as_str)
}

/// Whether a request carries any stateless-era protocol-version signal.
///
/// This deliberately returns true for unsupported versions so callers can
/// return the spec-defined `UnsupportedProtocolVersion` response instead of
/// accidentally falling back to the initialize era.
pub fn has_stateless_protocol_metadata(params: &Value) -> bool {
    params
        .get("_meta")
        .and_then(Value::as_object)
        .is_some_and(|meta| meta.contains_key(MCP_META_PROTOCOL_VERSION))
}

/// Validate the required request metadata for MCP 2026-07-28.
pub fn validate_stateless_request(
    params: &Value,
) -> Result<StatelessRequestMetadata, McpProtocolError> {
    let meta = params
        .get("_meta")
        .and_then(Value::as_object)
        .ok_or_else(|| McpProtocolError::invalid_params("Missing required params._meta object"))?;

    let protocol_version = meta
        .get(MCP_META_PROTOCOL_VERSION)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            McpProtocolError::invalid_params(format!(
                "Missing required params._meta['{MCP_META_PROTOCOL_VERSION}'] string"
            ))
        })?;

    if !MCP_STATELESS_SUPPORTED_VERSIONS.contains(&protocol_version) {
        return Err(McpProtocolError::unsupported_version(protocol_version));
    }

    let client_capabilities = meta
        .get(MCP_META_CLIENT_CAPABILITIES)
        .filter(|value| value.is_object())
        .cloned()
        .ok_or_else(|| {
            McpProtocolError::invalid_params(format!(
                "Missing required params._meta['{MCP_META_CLIENT_CAPABILITIES}'] object"
            ))
        })?;

    let client_info = match meta.get(MCP_META_CLIENT_INFO) {
        None => None,
        Some(value)
            if value
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| !name.is_empty())
                && value
                    .get("version")
                    .and_then(Value::as_str)
                    .is_some_and(|version| !version.is_empty()) =>
        {
            Some(value.clone())
        }
        Some(_) => {
            return Err(McpProtocolError::invalid_params(format!(
                "params._meta['{MCP_META_CLIENT_INFO}'] must contain non-empty name and version strings"
            )))
        }
    };

    Ok(StatelessRequestMetadata {
        protocol_version: protocol_version.to_string(),
        client_info,
        client_capabilities,
    })
}

/// Validate the standard parameter shapes for the stateless methods this
/// server implements. Tool handlers still perform their domain-specific
/// validation; this layer keeps malformed MCP envelopes transport-neutral.
pub fn validate_stateless_method_params(
    method: &str,
    params: &Value,
) -> Result<(), McpProtocolError> {
    if method == "tools/list" && params.get("cursor").is_some_and(|value| !value.is_string()) {
        return Err(McpProtocolError::invalid_params(
            "tools/list params.cursor must be a string",
        ));
    }
    if method != "tools/call" {
        return Ok(());
    }
    if params
        .get("name")
        .and_then(Value::as_str)
        .is_none_or(|name| name.is_empty())
    {
        return Err(McpProtocolError::invalid_params(
            "tools/call params.name must be a non-empty string",
        ));
    }
    if params
        .get("arguments")
        .is_some_and(|value| !value.is_object())
    {
        return Err(McpProtocolError::invalid_params(
            "tools/call params.arguments must be an object",
        ));
    }
    if params
        .get("inputResponses")
        .is_some_and(|value| !value.is_object())
    {
        return Err(McpProtocolError::invalid_params(
            "tools/call params.inputResponses must be an object",
        ));
    }
    if params
        .get("requestState")
        .is_some_and(|value| !value.is_string())
    {
        return Err(McpProtocolError::invalid_params(
            "tools/call params.requestState must be a string",
        ));
    }
    Ok(())
}

fn result_object(result: Value) -> Map<String, Value> {
    match result {
        Value::Object(object) => object,
        value => Map::from_iter([("value".to_string(), value)]),
    }
}

/// Decorate a successful result with the fields required/recommended by the
/// 2026-07-28 schema while preserving product-specific `_meta` entries.
pub fn decorate_stateless_result(result: Value) -> Value {
    let mut result = result_object(result);
    result
        .entry("resultType".to_string())
        .or_insert_with(|| Value::String("complete".to_string()));

    let meta = result
        .entry("_meta".to_string())
        .or_insert_with(|| Value::Object(Map::new()));
    if !meta.is_object() {
        *meta = Value::Object(Map::new());
    }
    meta.as_object_mut()
        .expect("result _meta was normalized to an object")
        .insert(
            MCP_META_SERVER_INFO.to_string(),
            json!({
                "name": "contextstream-mcp",
                "version": crate::config::VERSION,
            }),
        );

    Value::Object(result)
}

pub fn decorate_stateless_cacheable_result(
    result: Value,
    ttl_ms: u64,
    cache_scope: McpCacheScope,
) -> Value {
    let mut result = decorate_stateless_result(result);
    let object = result
        .as_object_mut()
        .expect("decorate_stateless_result always returns an object");
    object.insert("ttlMs".to_string(), json!(ttl_ms));
    object.insert(
        "cacheScope".to_string(),
        Value::String(cache_scope.as_str().to_string()),
    );
    result
}

pub fn build_stateless_discover_result(instructions: Option<String>) -> Value {
    let mut result = decorate_stateless_cacheable_result(
        json!({
            "supportedVersions": MCP_STATELESS_SUPPORTED_VERSIONS,
            "capabilities": {
                "tools": {
                    "listChanged": false,
                },
            },
        }),
        MCP_DISCOVERY_TTL_MS,
        // ContextStream's discovered tool surface and instructions can vary by
        // authenticated/configured caller, so shared caches would be unsafe.
        McpCacheScope::Private,
    );

    if let Some(instructions) = instructions.filter(|value| !value.is_empty()) {
        result
            .as_object_mut()
            .expect("discover result is an object")
            .insert("instructions".to_string(), Value::String(instructions));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(version: &str) -> Value {
        json!({
            "_meta": {
                MCP_META_PROTOCOL_VERSION: version,
                MCP_META_CLIENT_INFO: {
                    "name": "test-client",
                    "version": "1.0.0",
                },
                MCP_META_CLIENT_CAPABILITIES: {},
            },
        })
    }

    #[test]
    fn stateless_metadata_is_required_and_exact_versioned() {
        let metadata = validate_stateless_request(&params(MCP_PROTOCOL_2026_07_28))
            .expect("valid stateless request metadata");
        assert_eq!(metadata.protocol_version, MCP_PROTOCOL_2026_07_28);
        assert_eq!(metadata.client_name(), Some("test-client"));

        let unsupported = validate_stateless_request(&params("2099-01-01"))
            .expect_err("future versions must not be assumed compatible");
        assert_eq!(unsupported.code, MCP_ERROR_UNSUPPORTED_PROTOCOL_VERSION);
        assert_eq!(unsupported.data.unwrap()["requested"], "2099-01-01");

        let missing =
            validate_stateless_request(&json!({})).expect_err("request metadata is required");
        assert_eq!(missing.code, -32602);

        let missing_capabilities = validate_stateless_request(&json!({
            "_meta": { MCP_META_PROTOCOL_VERSION: MCP_PROTOCOL_2026_07_28 }
        }))
        .expect_err("client capabilities are required per request");
        assert_eq!(missing_capabilities.code, -32602);
    }

    #[test]
    fn stateless_result_preserves_existing_meta_and_adds_identity() {
        let result = decorate_stateless_result(json!({
            "content": [],
            "_meta": {"contextstream": {"icon": "x"}},
        }));
        assert_eq!(result["resultType"], "complete");
        assert_eq!(result["_meta"]["contextstream"]["icon"], "x");
        assert_eq!(
            result["_meta"][MCP_META_SERVER_INFO]["name"],
            "contextstream-mcp"
        );
    }

    #[test]
    fn stateless_tool_method_params_reject_malformed_shapes() {
        assert!(validate_stateless_method_params("tools/list", &json!({"cursor": "next"})).is_ok());
        assert_eq!(
            validate_stateless_method_params("tools/list", &json!({"cursor": 7}))
                .expect_err("numeric cursor must be rejected")
                .code,
            -32602
        );

        assert!(validate_stateless_method_params(
            "tools/call",
            &json!({"name": "search", "arguments": {}})
        )
        .is_ok());
        for malformed in [
            json!({"arguments": {}}),
            json!({"name": "search", "arguments": []}),
            json!({"name": "search", "inputResponses": []}),
            json!({"name": "search", "requestState": {}}),
        ] {
            assert_eq!(
                validate_stateless_method_params("tools/call", &malformed)
                    .expect_err("malformed tools/call params must be rejected")
                    .code,
                -32602
            );
        }
    }

    #[test]
    fn stateless_jsonrpc_envelope_requires_version_method_and_id() {
        let id = json!(1);
        assert!(validate_stateless_jsonrpc_envelope("2.0", Some(&id), "tools/list").is_ok());
        for (jsonrpc, id, method) in [
            ("1.0", Some(json!(1)), "tools/list"),
            ("2.0", None, "tools/list"),
            ("2.0", Some(json!({"bad": "id"})), "tools/list"),
            ("2.0", Some(json!(1)), ""),
        ] {
            assert_eq!(
                validate_stateless_jsonrpc_envelope(jsonrpc, id.as_ref(), method)
                    .expect_err("malformed JSON-RPC envelope must be rejected")
                    .code,
                -32600
            );
        }
    }

    #[test]
    fn discover_result_is_complete_private_and_cacheable() {
        let result = build_stateless_discover_result(Some("Use ContextStream".to_string()));
        assert_eq!(result["resultType"], "complete");
        assert_eq!(
            result["supportedVersions"],
            json!([MCP_PROTOCOL_2026_07_28])
        );
        assert_eq!(result["capabilities"]["tools"]["listChanged"], false);
        assert_eq!(result["cacheScope"], "private");
        assert_eq!(result["ttlMs"], MCP_DISCOVERY_TTL_MS);
        assert_eq!(result["instructions"], "Use ContextStream");
    }
}
