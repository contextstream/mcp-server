//! Transport layer benchmarks.
//!
//! Measures the performance of transport operations:
//! - Stdio line parsing
//! - HTTP request/response handling
//! - Message routing

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use serde_json::{json, Value};
use std::collections::HashMap;

/// Benchmark stdio line parsing (newline-delimited JSON).
fn bench_stdio_line_parsing(c: &mut Criterion) {
    let mut group = c.benchmark_group("stdio_line_parsing");

    // Single line message
    let single_line = r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#;
    group.throughput(Throughput::Elements(1));
    group.bench_with_input(
        BenchmarkId::new("single_line", single_line.len()),
        &single_line,
        |b, input| {
            b.iter(|| {
                let _: Value = serde_json::from_str(black_box(input)).unwrap();
            })
        },
    );

    // Multiple messages (batch)
    let batch = vec![
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"test"}}"#,
    ];
    group.throughput(Throughput::Elements(3));
    group.bench_function("batch_3_messages", |b| {
        b.iter(|| {
            for line in &batch {
                let _: Value = serde_json::from_str(black_box(line)).unwrap();
            }
        })
    });

    group.finish();
}

/// Benchmark method routing/dispatch.
fn bench_method_routing(c: &mut Criterion) {
    let mut group = c.benchmark_group("method_routing");

    let methods = vec![
        "initialize",
        "initialized",
        "tools/list",
        "tools/call",
        "notifications/cancelled",
        "ping",
    ];

    // String match routing
    group.bench_function("match_routing", |b| {
        b.iter(|| {
            for method in &methods {
                let _ = black_box(match *method {
                    "initialize" => 0,
                    "initialized" => 1,
                    "tools/list" => 2,
                    "tools/call" => 3,
                    "notifications/cancelled" => 4,
                    "ping" => 5,
                    _ => -1,
                });
            }
        })
    });

    // HashMap routing
    let method_map: HashMap<&str, i32> = methods
        .iter()
        .enumerate()
        .map(|(i, &m)| (m, i as i32))
        .collect();

    group.bench_function("hashmap_routing", |b| {
        b.iter(|| {
            for method in &methods {
                let _ = black_box(method_map.get(method));
            }
        })
    });

    // Prefix-based routing
    group.bench_function("prefix_routing", |b| {
        b.iter(|| {
            for method in &methods {
                let _ = black_box(if method.starts_with("tools/") {
                    1
                } else if method.starts_with("notifications/") {
                    2
                } else {
                    0
                });
            }
        })
    });

    group.finish();
}

/// Benchmark request ID handling.
fn bench_request_id_handling(c: &mut Criterion) {
    let mut group = c.benchmark_group("request_id_handling");

    // Integer ID
    let int_id = json!(42);
    group.bench_function("integer_id", |b| {
        b.iter(|| {
            let id = black_box(&int_id);
            if let Some(n) = id.as_i64() {
                black_box(n);
            }
        })
    });

    // String ID
    let string_id = json!("request-12345-abcdef");
    group.bench_function("string_id", |b| {
        b.iter(|| {
            let id = black_box(&string_id);
            if let Some(s) = id.as_str() {
                black_box(s);
            }
        })
    });

    // ID comparison
    let id1 = json!(100);
    let id2 = json!(100);
    let id3 = json!("100");
    group.bench_function("id_comparison", |b| {
        b.iter(|| {
            let _ = black_box(id1 == id2);
            let _ = black_box(id1 == id3);
        })
    });

    group.finish();
}

/// Benchmark response building.
fn bench_response_building(c: &mut Criterion) {
    let mut group = c.benchmark_group("response_building");

    // Success response building
    group.bench_function("build_success", |b| {
        b.iter(|| {
            let response = json!({
                "jsonrpc": "2.0",
                "id": black_box(42),
                "result": {
                    "content": [{
                        "type": "text",
                        "text": black_box("Operation completed successfully")
                    }],
                    "isError": false
                }
            });
            black_box(response);
        })
    });

    // Error response building
    group.bench_function("build_error", |b| {
        b.iter(|| {
            let response = json!({
                "jsonrpc": "2.0",
                "id": black_box(42),
                "error": {
                    "code": -32602,
                    "message": black_box("Invalid parameters"),
                    "data": {
                        "field": "workspace_id",
                        "reason": "UUID format required"
                    }
                }
            });
            black_box(response);
        })
    });

    // Tool result wrapping
    group.bench_function("wrap_tool_result", |b| {
        let tool_output = json!({
            "success": true,
            "data": {
                "context": "relevant context here",
                "sources": 5
            }
        });
        b.iter(|| {
            let wrapped = json!({
                "content": [{
                    "type": "text",
                    "text": serde_json::to_string(black_box(&tool_output)).unwrap()
                }],
                "isError": false
            });
            black_box(wrapped);
        })
    });

    group.finish();
}

/// Benchmark header parsing (HTTP transport).
fn bench_header_parsing(c: &mut Criterion) {
    let mut group = c.benchmark_group("header_parsing");

    // Authorization header extraction
    let auth_headers = vec![
        "Bearer benchmark-token",
        "bearer lowercase-benchmark-token",
        "Basic benchmark-basic-token",
    ];

    group.bench_function("bearer_token_extraction", |b| {
        b.iter(|| {
            for header in &auth_headers {
                if let Some(token) = header.strip_prefix("Bearer ") {
                    black_box(token);
                }
            }
        })
    });

    // Content-Type parsing
    let content_types = vec![
        "application/json",
        "application/json; charset=utf-8",
        "text/plain",
    ];

    group.bench_function("content_type_check", |b| {
        b.iter(|| {
            for ct in &content_types {
                let is_json = ct.starts_with("application/json");
                black_box(is_json);
            }
        })
    });

    group.finish();
}

/// Benchmark concurrent request tracking.
fn bench_request_tracking(c: &mut Criterion) {
    let mut group = c.benchmark_group("request_tracking");

    // HashMap-based tracking
    let mut tracking: HashMap<i64, String> = HashMap::new();
    for i in 0..100 {
        tracking.insert(i, format!("request-{}", i));
    }

    group.bench_function("hashmap_insert", |b| {
        let mut map: HashMap<i64, String> = HashMap::with_capacity(100);
        b.iter(|| {
            map.clear();
            for i in 0..100 {
                map.insert(black_box(i), format!("request-{}", i));
            }
        })
    });

    group.bench_function("hashmap_lookup", |b| {
        b.iter(|| {
            for i in 0..100 {
                let _ = black_box(tracking.get(&i));
            }
        })
    });

    group.bench_function("hashmap_remove", |b| {
        b.iter(|| {
            let mut map = tracking.clone();
            for i in 0..100 {
                let _ = black_box(map.remove(&i));
            }
        })
    });

    group.finish();
}

/// Benchmark message size estimation.
fn bench_message_size_estimation(c: &mut Criterion) {
    let mut group = c.benchmark_group("message_size_estimation");

    let messages = vec![
        json!({"small": true}),
        json!({"medium": "x".repeat(1000)}),
        json!({"large": "x".repeat(10000)}),
    ];

    group.bench_function("string_len", |b| {
        b.iter(|| {
            for msg in &messages {
                let s = serde_json::to_string(black_box(msg)).unwrap();
                black_box(s.len());
            }
        })
    });

    group.bench_function("to_vec_len", |b| {
        b.iter(|| {
            for msg in &messages {
                let v = serde_json::to_vec(black_box(msg)).unwrap();
                black_box(v.len());
            }
        })
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_stdio_line_parsing,
    bench_method_routing,
    bench_request_id_handling,
    bench_response_building,
    bench_header_parsing,
    bench_request_tracking,
    bench_message_size_estimation,
);

criterion_main!(benches);
