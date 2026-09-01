//! JSON parsing and serialization benchmarks.
//!
//! Measures the performance of JSON operations:
//! - JSON-RPC request parsing
//! - JSON-RPC response serialization
//! - Tool schema generation
//! - Large payload handling

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// JSON-RPC request structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: Value,
    method: String,
    #[serde(default)]
    params: Value,
}

/// JSON-RPC response structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct JsonRpcResponse {
    jsonrpc: String,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<JsonRpcError>,
}

/// JSON-RPC error structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct JsonRpcError {
    code: i32,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

/// Tool definition for schema benchmarking.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ToolDefinition {
    name: String,
    description: String,
    #[serde(rename = "inputSchema")]
    input_schema: Value,
}

/// Benchmark JSON-RPC request parsing.
fn bench_jsonrpc_request_parsing(c: &mut Criterion) {
    let mut group = c.benchmark_group("jsonrpc_request_parsing");

    // Initialize request
    let init_request =
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"capabilities":{}}}"#;
    group.throughput(Throughput::Bytes(init_request.len() as u64));
    group.bench_with_input(
        BenchmarkId::new("initialize", init_request.len()),
        &init_request,
        |b, input| {
            b.iter(|| {
                let _: JsonRpcRequest = serde_json::from_str(black_box(input)).unwrap();
            })
        },
    );

    // Tools list request
    let tools_request = r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#;
    group.bench_with_input(
        BenchmarkId::new("tools_list", tools_request.len()),
        &tools_request,
        |b, input| {
            b.iter(|| {
                let _: JsonRpcRequest = serde_json::from_str(black_box(input)).unwrap();
            })
        },
    );

    // Tool call request with parameters
    let call_request = r#"{
        "jsonrpc": "2.0",
        "id": 3,
        "method": "tools/call",
        "params": {
            "name": "mcp__contextstream__context",
            "arguments": {
                "user_message": "How do I implement authentication?",
                "workspace_id": "550e8400-e29b-41d4-a716-446655440000",
                "project_id": "6ba7b810-9dad-11d1-80b4-00c04fd430c8",
                "format": "minified",
                "max_tokens": 800
            }
        }
    }"#;
    group.bench_with_input(
        BenchmarkId::new("tool_call", call_request.len()),
        &call_request,
        |b, input| {
            b.iter(|| {
                let _: JsonRpcRequest = serde_json::from_str(black_box(input)).unwrap();
            })
        },
    );

    group.finish();
}

/// Benchmark JSON-RPC response serialization.
fn bench_jsonrpc_response_serialization(c: &mut Criterion) {
    let mut group = c.benchmark_group("jsonrpc_response_serialization");

    // Success response (small)
    let success_small = JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: json!(1),
        result: Some(json!({"success": true})),
        error: None,
    };
    group.bench_with_input(
        BenchmarkId::new("success_small", 50),
        &success_small,
        |b, response| b.iter(|| serde_json::to_string(black_box(response)).unwrap()),
    );

    // Success response (medium - context result)
    let success_medium = JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: json!(2),
        result: Some(json!({
            "content": [{
                "type": "text",
                "text": "Context retrieved successfully with relevant lessons and decisions."
            }],
            "isError": false
        })),
        error: None,
    };
    group.bench_with_input(
        BenchmarkId::new("success_medium", 200),
        &success_medium,
        |b, response| b.iter(|| serde_json::to_string(black_box(response)).unwrap()),
    );

    // Error response
    let error_response = JsonRpcResponse {
        jsonrpc: "2.0".to_string(),
        id: json!(3),
        result: None,
        error: Some(JsonRpcError {
            code: -32602,
            message: "Invalid params".to_string(),
            data: Some(json!({"field": "workspace_id", "reason": "Invalid UUID format"})),
        }),
    };
    group.bench_with_input(
        BenchmarkId::new("error", 150),
        &error_response,
        |b, response| b.iter(|| serde_json::to_string(black_box(response)).unwrap()),
    );

    group.finish();
}

/// Benchmark tool list serialization (common operation).
fn bench_tool_list_serialization(c: &mut Criterion) {
    let mut group = c.benchmark_group("tool_list_serialization");

    // Generate tool definitions
    let generate_tools = |count: usize| -> Vec<ToolDefinition> {
        (0..count)
            .map(|i| ToolDefinition {
                name: format!("mcp__contextstream__tool_{}", i),
                description: format!("Tool {} for processing requests with various parameters", i),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "description": "Action to perform"},
                        "workspace_id": {"type": "string", "format": "uuid"},
                        "project_id": {"type": "string", "format": "uuid"},
                        "content": {"type": "string"},
                        "tags": {"type": "array", "items": {"type": "string"}}
                    },
                    "required": ["action"]
                }),
            })
            .collect()
    };

    // Small tool list (10 tools - light mode)
    let small_tools = generate_tools(10);
    group.bench_with_input(
        BenchmarkId::new("10_tools", 10),
        &small_tools,
        |b, tools| b.iter(|| serde_json::to_string(black_box(tools)).unwrap()),
    );

    // Medium tool list (50 tools - standard mode)
    let medium_tools = generate_tools(50);
    group.bench_with_input(
        BenchmarkId::new("50_tools", 50),
        &medium_tools,
        |b, tools| b.iter(|| serde_json::to_string(black_box(tools)).unwrap()),
    );

    // Large tool list (100+ tools - complete mode)
    let large_tools = generate_tools(120);
    group.bench_with_input(
        BenchmarkId::new("120_tools", 120),
        &large_tools,
        |b, tools| b.iter(|| serde_json::to_string(black_box(tools)).unwrap()),
    );

    group.finish();
}

/// Benchmark large payload handling.
fn bench_large_payload_handling(c: &mut Criterion) {
    let mut group = c.benchmark_group("large_payload_handling");

    // 1KB payload
    let payload_1kb = json!({
        "content": "x".repeat(1024),
        "metadata": {"size": "1KB"}
    });
    group.throughput(Throughput::Bytes(1024));
    group.bench_with_input(
        BenchmarkId::new("parse_1kb", 1024),
        &serde_json::to_string(&payload_1kb).unwrap(),
        |b, input| {
            b.iter(|| {
                let _: Value = serde_json::from_str(black_box(input)).unwrap();
            })
        },
    );

    // 10KB payload
    let payload_10kb = json!({
        "content": "x".repeat(10 * 1024),
        "metadata": {"size": "10KB"}
    });
    group.throughput(Throughput::Bytes(10 * 1024));
    group.bench_with_input(
        BenchmarkId::new("parse_10kb", 10 * 1024),
        &serde_json::to_string(&payload_10kb).unwrap(),
        |b, input| {
            b.iter(|| {
                let _: Value = serde_json::from_str(black_box(input)).unwrap();
            })
        },
    );

    // 100KB payload
    let payload_100kb = json!({
        "content": "x".repeat(100 * 1024),
        "metadata": {"size": "100KB"}
    });
    group.throughput(Throughput::Bytes(100 * 1024));
    group.bench_with_input(
        BenchmarkId::new("parse_100kb", 100 * 1024),
        &serde_json::to_string(&payload_100kb).unwrap(),
        |b, input| {
            b.iter(|| {
                let _: Value = serde_json::from_str(black_box(input)).unwrap();
            })
        },
    );

    group.finish();
}

/// Benchmark JSON value access patterns.
fn bench_json_value_access(c: &mut Criterion) {
    let mut group = c.benchmark_group("json_value_access");

    let complex_json = json!({
        "workspace": {
            "id": "550e8400-e29b-41d4-a716-446655440000",
            "name": "TestWorkspace",
            "projects": [
                {"id": "proj1", "name": "MCP Server"},
                {"id": "proj2", "name": "Client SDK"},
                {"id": "proj3", "name": "Documentation"}
            ]
        },
        "session": {
            "id": "session-123",
            "context": {
                "decisions": ["use-rust", "async-first"],
                "lessons": ["avoid-blocking", "prefer-streaming"]
            }
        }
    });

    // Pointer access
    group.bench_function("pointer_access", |b| {
        b.iter(|| {
            let _ = black_box(complex_json.pointer("/workspace/id"));
            let _ = black_box(complex_json.pointer("/workspace/projects/0/name"));
            let _ = black_box(complex_json.pointer("/session/context/decisions"));
        })
    });

    // Direct indexing
    group.bench_function("index_access", |b| {
        b.iter(|| {
            let _ = black_box(&complex_json["workspace"]["id"]);
            let _ = black_box(&complex_json["workspace"]["projects"][0]["name"]);
            let _ = black_box(&complex_json["session"]["context"]["decisions"]);
        })
    });

    // Get method
    group.bench_function("get_access", |b| {
        b.iter(|| {
            let _ = black_box(complex_json.get("workspace").and_then(|w| w.get("id")));
            let _ = black_box(
                complex_json
                    .get("workspace")
                    .and_then(|w| w.get("projects"))
                    .and_then(|p| p.get(0))
                    .and_then(|p| p.get("name")),
            );
        })
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_jsonrpc_request_parsing,
    bench_jsonrpc_response_serialization,
    bench_tool_list_serialization,
    bench_large_payload_handling,
    bench_json_value_access,
);

criterion_main!(benches);
