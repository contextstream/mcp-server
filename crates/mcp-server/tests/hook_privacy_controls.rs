//! Regression tests for editor hook privacy controls (#272, #273).
#![cfg(unix)]

use axum::{body::Bytes, http::StatusCode, routing::any, Router};
use serde_json::{json, Value};
use std::{
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
    sync::{Arc, Mutex},
};

const WORKSPACE_ID: &str = "11111111-1111-4111-8111-111111111111";
const PROJECT_ID: &str = "33333333-3333-4333-8333-333333333333";

#[derive(Clone, Debug)]
struct CapturedRequest {
    method_and_path: String,
    body: String,
}

async fn spawn_capture_server() -> (
    String,
    Arc<Mutex<Vec<CapturedRequest>>>,
    tokio::task::JoinHandle<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_url = format!("http://{}", listener.local_addr().unwrap());
    let captured: Arc<Mutex<Vec<CapturedRequest>>> = Arc::new(Mutex::new(Vec::new()));
    let cap_route = Arc::clone(&captured);

    let app = Router::new().fallback(any(
        move |method: axum::http::Method, uri: axum::http::Uri, body: Bytes| {
            let cap = Arc::clone(&cap_route);
            async move {
                cap.lock().unwrap().push(CapturedRequest {
                    method_and_path: format!("{} {}", method, uri.path()),
                    body: String::from_utf8_lossy(&body).to_string(),
                });
                (StatusCode::OK, axum::Json(json!({"data": {"ok": true}})))
            }
        },
    ));

    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (api_url, captured, handle)
}

fn run_hook_with_env(
    home: &Path,
    cwd: &Path,
    hook_name: &str,
    stdin_payload: &Value,
    extra_env: &[(&str, &str)],
) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"));
    cmd.args(["hook", hook_name])
        .current_dir(cwd)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env_remove("CONTEXTSTREAM_API_KEY")
        .env_remove("CONTEXTSTREAM_API_URL")
        .env_remove("CONTEXTSTREAM_HOOK_ENABLED")
        .env_remove("CONTEXTSTREAM_TRANSCRIPTS_ENABLED")
        .env_remove("CONTEXTSTREAM_HOOK_TRANSCRIPTS_ENABLED")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("spawn hook");
    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(serde_json::to_string(stdin_payload).unwrap().as_bytes())
            .expect("write hook stdin");
    }
    child.wait_with_output().expect("wait hook")
}

#[tokio::test]
async fn contextstream_hook_enabled_false_noops_all_editor_lifecycle_hooks() {
    let (api_url, captured, server_handle) = spawn_capture_server().await;
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(home.join(".contextstream")).unwrap();
    std::fs::create_dir_all(repo.join(".contextstream")).unwrap();

    std::fs::write(
        home.join(".contextstream").join("credentials.json"),
        serde_json::to_string_pretty(&json!({
            "api_key": "cs_mock_hook_disabled_key",
            "api_url": api_url,
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        repo.join(".contextstream").join("config.json"),
        serde_json::to_string_pretty(&json!({
            "workspace_id": WORKSPACE_ID,
            "project_id": PROJECT_ID,
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(repo.join("a.txt"), "hello\n").unwrap();

    let memory_target = home
        .join(".claude")
        .join("projects")
        .join("foo")
        .join("memory")
        .join("MEMORY.md");

    for flag_val in ["false", "0", "off", "no"] {
        for (hook_name, payload) in [
            (
                "pre-tool-use",
                json!({
                    "session_id": "s1",
                    "cwd": repo,
                    "tool_name": "Write",
                    "tool_input": {
                        "file_path": memory_target,
                        "content": "note"
                    }
                }),
            ),
            (
                "post-tool-use",
                json!({
                    "session_id": "s1",
                    "cwd": repo,
                    "tool_name": "Edit",
                    "tool_input": { "file_path": "a.txt" }
                }),
            ),
            (
                "user-prompt-submit",
                json!({
                    "session_id": "s1",
                    "cwd": repo,
                    "prompt": "hello"
                }),
            ),
            (
                "session-start",
                json!({
                    "session_id": "s1",
                    "cwd": repo
                }),
            ),
            (
                "stop",
                json!({
                    "session_id": "s1",
                    "cwd": repo
                }),
            ),
            (
                "pre-compact",
                json!({
                    "session_id": "s1",
                    "cwd": repo
                }),
            ),
            (
                "session-end",
                json!({
                    "session_id": "s1",
                    "cwd": repo
                }),
            ),
            (
                "agy-pre-invocation",
                json!({
                    "conversationId": WORKSPACE_ID,
                    "workspacePaths": [repo]
                }),
            ),
        ] {
            let out = run_hook_with_env(
                &home,
                &repo,
                hook_name,
                &payload,
                &[
                    ("CONTEXTSTREAM_HOOK_ENABLED", flag_val),
                    ("CONTEXTSTREAM_API_URL", &api_url),
                ],
            );
            assert!(
                out.status.success(),
                "{hook_name} ({flag_val}) failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert_eq!(
                String::from_utf8_lossy(&out.stdout).trim(),
                "{}",
                "{hook_name} ({flag_val}) should emit empty JSON object"
            );
        }

        let codex_out = run_hook_with_env(
            &home,
            &repo,
            "codex-notify",
            &json!({"type": "agent-turn-complete", "cwd": repo}),
            &[
                ("CONTEXTSTREAM_HOOK_ENABLED", flag_val),
                ("CONTEXTSTREAM_API_URL", &api_url),
            ],
        );
        assert!(codex_out.status.success());
        assert!(String::from_utf8_lossy(&codex_out.stdout).trim().is_empty());
    }

    assert!(
        captured.lock().unwrap().is_empty(),
        "expected zero outbound API requests when CONTEXTSTREAM_HOOK_ENABLED=false, got {:?}",
        *captured.lock().unwrap()
    );
    server_handle.abort();
}
