//! Tool execution latency benchmarks.
//!
//! Measures the performance of key tool operations:
//! - Session init
//! - Context retrieval
//! - Search operations
//! - Memory operations

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use serde_json::{json, Value};

/// Simulate tool input parsing (common operation).
fn parse_tool_input(input: &str) -> Value {
    serde_json::from_str(input).unwrap_or(Value::Null)
}

/// Simulate tool result serialization (common operation).
fn serialize_tool_result(result: &Value) -> String {
    serde_json::to_string(result).unwrap_or_default()
}

/// Benchmark tool input parsing for various payload sizes.
fn bench_tool_input_parsing(c: &mut Criterion) {
    let mut group = c.benchmark_group("tool_input_parsing");

    // Small input (init params)
    let small_input = r#"{"folder_path": "/home/user/project", "auto_index": true}"#;
    group.bench_with_input(
        BenchmarkId::new("small", small_input.len()),
        &small_input,
        |b, input| b.iter(|| parse_tool_input(black_box(input))),
    );

    // Medium input (context params)
    let medium_input = r#"{
        "user_message": "How do I implement authentication in this project?",
        "workspace_id": "550e8400-e29b-41d4-a716-446655440000",
        "project_id": "6ba7b810-9dad-11d1-80b4-00c04fd430c8",
        "format": "minified",
        "max_tokens": 800
    }"#;
    group.bench_with_input(
        BenchmarkId::new("medium", medium_input.len()),
        &medium_input,
        |b, input| b.iter(|| parse_tool_input(black_box(input))),
    );

    // Large input (memory event with content)
    let large_content = "x".repeat(10000);
    let large_input = format!(
        r#"{{
            "action": "create_event",
            "event_type": "decision",
            "title": "Architecture Decision",
            "content": "{}",
            "tags": ["architecture", "rust", "performance"],
            "metadata": {{"priority": "high", "category": "technical"}}
        }}"#,
        large_content
    );
    group.bench_with_input(
        BenchmarkId::new("large", large_input.len()),
        &large_input,
        |b, input| b.iter(|| parse_tool_input(black_box(input))),
    );

    group.finish();
}

/// Benchmark tool result serialization for various payload sizes.
fn bench_tool_result_serialization(c: &mut Criterion) {
    let mut group = c.benchmark_group("tool_result_serialization");

    // Small result (init response)
    let small_result = json!({
        "success": true,
        "workspace_id": "550e8400-e29b-41d4-a716-446655440000",
        "project_id": "6ba7b810-9dad-11d1-80b4-00c04fd430c8"
    });
    group.bench_with_input(
        BenchmarkId::new("small", 100),
        &small_result,
        |b, result| b.iter(|| serialize_tool_result(black_box(result))),
    );

    // Medium result (context response)
    let medium_result = json!({
        "context": "W:TestWorkspace|P:test-project|D:Use Rust for performance|M:Testing phase",
        "token_estimate": 50,
        "format": "minified",
        "sources_used": 5,
        "workspace_id": "550e8400-e29b-41d4-a716-446655440000",
        "project_id": "6ba7b810-9dad-11d1-80b4-00c04fd430c8",
        "lessons": [],
        "reminders": []
    });
    group.bench_with_input(
        BenchmarkId::new("medium", 500),
        &medium_result,
        |b, result| b.iter(|| serialize_tool_result(black_box(result))),
    );

    // Large result (search results with content)
    let search_results: Vec<Value> = (0..20).map(|i| {
        json!({
            "id": format!("result-{}", i),
            "file_path": format!("src/module_{}/handler.rs", i),
            "content": format!("pub fn handle_request_{i}(req: Request) -> Response {{ /* implementation */ }}"),
            "score": 0.95 - (i as f64 * 0.02),
            "line_start": i * 10,
            "line_end": i * 10 + 20
        })
    }).collect();
    let large_result = json!({
        "success": true,
        "results": search_results,
        "total": 100,
        "query_time_ms": 45
    });
    group.bench_with_input(
        BenchmarkId::new("large", 5000),
        &large_result,
        |b, result| b.iter(|| serialize_tool_result(black_box(result))),
    );

    group.finish();
}

/// Benchmark tool routing/dispatch simulation.
fn bench_tool_dispatch(c: &mut Criterion) {
    let mut group = c.benchmark_group("tool_dispatch");

    // Simulate tool name matching
    let tool_names = vec![
        "mcp__contextstream__init",
        "mcp__contextstream__context",
        "mcp__contextstream__search",
        "mcp__contextstream__session",
        "mcp__contextstream__memory",
        "mcp__contextstream__graph",
        "mcp__contextstream__workspace",
        "mcp__contextstream__project",
        "mcp__contextstream__integration",
        "mcp__contextstream__reminder",
    ];

    group.bench_function("string_match", |b| {
        b.iter(|| {
            for name in &tool_names {
                let _ = black_box(match *name {
                    "mcp__contextstream__init" => 0,
                    "mcp__contextstream__context" => 1,
                    "mcp__contextstream__search" => 2,
                    "mcp__contextstream__session" => 3,
                    "mcp__contextstream__memory" => 4,
                    "mcp__contextstream__graph" => 5,
                    "mcp__contextstream__workspace" => 6,
                    "mcp__contextstream__project" => 7,
                    "mcp__contextstream__integration" => 8,
                    "mcp__contextstream__reminder" => 9,
                    _ => -1,
                });
            }
        })
    });

    // HashMap lookup simulation
    use std::collections::HashMap;
    let tool_map: HashMap<&str, i32> = tool_names
        .iter()
        .enumerate()
        .map(|(i, &name)| (name, i as i32))
        .collect();

    group.bench_function("hashmap_lookup", |b| {
        b.iter(|| {
            for name in &tool_names {
                let _ = black_box(tool_map.get(name));
            }
        })
    });

    group.finish();
}

/// Benchmark parameter validation patterns.
fn bench_parameter_validation(c: &mut Criterion) {
    let mut group = c.benchmark_group("parameter_validation");

    // UUID validation
    let valid_uuid = "550e8400-e29b-41d4-a716-446655440000";
    let invalid_uuid = "not-a-valid-uuid";

    group.bench_function("uuid_parse_valid", |b| {
        b.iter(|| {
            let _ = black_box(uuid::Uuid::parse_str(valid_uuid));
        })
    });

    group.bench_function("uuid_parse_invalid", |b| {
        b.iter(|| {
            let _ = black_box(uuid::Uuid::parse_str(invalid_uuid));
        })
    });

    // String validation (non-empty check)
    let long_string = "x".repeat(1000);
    let strings: Vec<&str> = vec!["", "short", "a medium length string", &long_string];
    group.bench_function("string_empty_check", |b| {
        b.iter(|| {
            for s in &strings {
                let _ = black_box(!s.is_empty());
            }
        })
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_tool_input_parsing,
    bench_tool_result_serialization,
    bench_tool_dispatch,
    bench_parameter_validation,
);

criterion_main!(benches);
