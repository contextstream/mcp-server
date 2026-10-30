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

fn install_release_like_binary(home: &Path) -> std::path::PathBuf {
    let bin_dir = home.join(".contextstream").join("bin");
    std::fs::create_dir_all(&bin_dir).expect("create bin dir");
    let dst = bin_dir.join("contextstream-mcp");
    if !dst.exists() {
        let src = Path::new(env!("CARGO_BIN_EXE_contextstream-mcp"));
        if std::fs::hard_link(src, &dst).is_err() {
            std::fs::copy(src, &dst).expect("copy contextstream-mcp");
        }
    }
    dst
}

fn run_hook_with_env(
    home: &Path,
    cwd: &Path,
    hook_name: &str,
    stdin_payload: &Value,
    extra_env: &[(&str, &str)],
) -> Output {
    let bin = install_release_like_binary(home);
    let mut cmd = Command::new(bin);
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
        // A disabled hook exits without reading its input, and may do so
        // before this write: a closed pipe is then its answer, not a failure.
        match stdin.write_all(serde_json::to_string(stdin_payload).unwrap().as_bytes()) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {}
            Err(error) => panic!("write hook stdin: {error}"),
        }
    }
    child.wait_with_output().expect("wait hook")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pre_compact_and_session_end_honor_hosted_remote_transcript_opt_outs_and_saved_credentials()
{
    let (api_url, captured, server_handle) = spawn_capture_server().await;
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(home.join(".claude").join("projects")).unwrap();
    std::fs::create_dir_all(home.join(".contextstream")).unwrap();
    std::fs::create_dir_all(repo.join(".contextstream")).unwrap();

    // Seed hosted remote MCP configs (`.mcp.json` and `~/.claude.json`) + `credentials.json`
    // with a custom `api_url` and NO `env` block, matching `setup --yes --editors=claude`.
    let remote_server_entry = |project_id: Option<&str>| {
        let mut headers = serde_json::Map::from_iter([
            ("X-ContextStream-Client".to_string(), json!("claude-code")),
            (
                "X-ContextStream-Managed-Config-Version".to_string(),
                json!("2"),
            ),
            (
                "X-ContextStream-Installation-Id".to_string(),
                json!("11111111-1111-4111-8111-111111111111"),
            ),
            (
                "X-ContextStream-Teaching-Version".to_string(),
                json!("harness_teaching_v4"),
            ),
            (
                "X-ContextStream-Workspace-Id".to_string(),
                json!(WORKSPACE_ID),
            ),
            (
                "X-ContextStream-Transcripts-Enabled".to_string(),
                json!("true"),
            ),
            (
                "X-ContextStream-Hook-Transcripts-Enabled".to_string(),
                json!("true"),
            ),
        ]);
        if let Some(pid) = project_id {
            headers.insert("X-ContextStream-Project-Id".to_string(), json!(pid));
        }
        json!({
            "mcpServers": {
                "contextstream": {
                    "type": "http",
                    "url": "https://mcp.contextstream.io/mcp?default_context_mode=fast",
                    "headers": headers,
                }
            }
        })
    };

    std::fs::write(
        home.join(".claude.json"),
        serde_json::to_string_pretty(&remote_server_entry(None)).unwrap(),
    )
    .unwrap();
    std::fs::write(
        repo.join(".mcp.json"),
        serde_json::to_string_pretty(&remote_server_entry(Some(PROJECT_ID))).unwrap(),
    )
    .unwrap();
    std::fs::write(
        home.join(".contextstream").join("installation.json"),
        serde_json::to_string_pretty(&json!({
            "schema_version": 1,
            "installation_id": "11111111-1111-4111-8111-111111111111",
            "created_at": "2026-10-29T00:00:00Z",
            "configured_clients": ["claude"],
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        home.join(".contextstream").join("credentials.json"),
        serde_json::to_string_pretty(&json!({
            "api_key": "cs_mock_hosted_transcript_key",
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

    let transcript_path = temp.path().join("session.jsonl");
    std::fs::write(
        &transcript_path,
        concat!(
            r#"{"type":"user","content":"CONFIDENTIAL_TRANSCRIPT_BODY_999","timestamp":"2026-10-29T09:47:00Z"}"#,
            "\n",
            r#"{"type":"assistant","content":"CONFIDENTIAL_ASSISTANT_REPLY_999","timestamp":"2026-10-29T09:47:01Z"}"#,
            "\n",
        ),
    )
    .unwrap();

    let hook_payload = json!({
        "session_id": "sess-999",
        "cwd": repo,
        "transcript_path": transcript_path,
    });

    // 1. Default `on`: both `pre-compact` and `session-end` resolve saved `credentials.json`
    // (`api_key` + `api_url`) with `CONTEXTSTREAM_API_KEY` and `CONTEXTSTREAM_API_URL` unset,
    // and upload transcripts.
    for hook in ["pre-compact", "session-end"] {
        captured.lock().unwrap().clear();
        let out = run_hook_with_env(&home, &repo, hook, &hook_payload, &[]);
        assert!(
            out.status.success(),
            "{hook} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let hits = captured.lock().unwrap().clone();
        assert!(
            hits.iter()
                .any(|req| req.method_and_path == "POST /api/v1/transcripts"),
            "expected {hook} to POST /api/v1/transcripts when enabled on hosted setup, got {hits:?}"
        );
    }

    let bin = install_release_like_binary(&home);
    let run_configure = |args: &[&str]| {
        let out = Command::new(&bin)
            .args(args)
            .current_dir(&repo)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env_remove("CONTEXTSTREAM_API_KEY")
            .env_remove("CONTEXTSTREAM_API_URL")
            .stdin(Stdio::null())
            .output()
            .expect("run configure");
        assert!(
            out.status.success(),
            "configure {args:?} failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    };

    // 2. Turn off both policies via `configure` (`--scope=all`) — exact #273 reproduction.
    run_configure(&[
        "configure",
        "--transcripts",
        "off",
        "--scope=all",
        "--only-configured",
    ]);
    run_configure(&[
        "configure",
        "--hook-transcripts",
        "off",
        "--scope=all",
        "--only-configured",
    ]);

    for hook in ["pre-compact", "session-end"] {
        captured.lock().unwrap().clear();
        let out = run_hook_with_env(&home, &repo, hook, &hook_payload, &[]);
        assert!(out.status.success());
        let hits = captured.lock().unwrap().clone();
        assert!(
            hits.iter()
                .all(|req| req.method_and_path != "POST /api/v1/transcripts"),
            "{hook} must not upload POST /api/v1/transcripts when disabled: {hits:?}"
        );
        for req in &hits {
            assert!(
                !req.body.contains("CONFIDENTIAL_TRANSCRIPT_BODY_999")
                    && !req.body.contains("CONFIDENTIAL_ASSISTANT_REPLY_999"),
                "{hook} leaked confidential transcript text in {}: {}",
                req.method_and_path,
                req.body
            );
        }
    }

    // 3. Scope-partial opt-outs (`--scope=global` alone while `.mcp.json` has `"true"`,
    // and `--scope=project` alone while `~/.claude.json` has `"true"`) must also fail closed.
    for partial_scope in ["--scope=global", "--scope=project"] {
        run_configure(&[
            "configure",
            "--transcripts",
            "on",
            "--hook-transcripts",
            "on",
            "--scope=all",
            "--only-configured",
        ]);
        run_configure(&[
            "configure",
            "--hook-transcripts",
            "off",
            partial_scope,
            "--only-configured",
        ]);

        for hook in ["pre-compact", "session-end"] {
            captured.lock().unwrap().clear();
            let out = run_hook_with_env(&home, &repo, hook, &hook_payload, &[]);
            assert!(out.status.success());
            let hits = captured.lock().unwrap().clone();
            assert!(
                hits.iter()
                    .all(|req| req.method_and_path != "POST /api/v1/transcripts"),
                "{hook} must honor {partial_scope} opt-out: {hits:?}"
            );
            for req in &hits {
                assert!(
                    !req.body.contains("CONFIDENTIAL_TRANSCRIPT_BODY_999")
                        && !req.body.contains("CONFIDENTIAL_ASSISTANT_REPLY_999"),
                    "{hook} leaked transcript text under {partial_scope}: {}",
                    req.body
                );
            }
        }
    }

    // 4. Codex `.codex/config.toml` remote `http_headers` opt-out is also honored.
    run_configure(&[
        "configure",
        "--transcripts",
        "on",
        "--hook-transcripts",
        "on",
        "--scope=all",
        "--only-configured",
    ]);
    std::fs::create_dir_all(repo.join(".codex")).unwrap();
    std::fs::write(
        repo.join(".codex").join("config.toml"),
        r#"[mcp_servers.contextstream]
url = "https://mcp.contextstream.io/mcp?default_context_mode=fast"
http_headers = { "X-ContextStream-Transcripts-Enabled" = "false", "X-ContextStream-Hook-Transcripts-Enabled" = "false" }
"#,
    )
    .unwrap();
    captured.lock().unwrap().clear();
    let codex_pc = run_hook_with_env(&home, &repo, "pre-compact", &hook_payload, &[]);
    assert!(codex_pc.status.success());
    assert!(
        captured.lock().unwrap().is_empty(),
        "expected Codex TOML opt-out to prevent pre-compact uploads, got {:?}",
        *captured.lock().unwrap()
    );

    server_handle.abort();
}
