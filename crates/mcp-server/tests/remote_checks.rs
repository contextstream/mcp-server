//! Exercise the editor-hook fallback across real one-shot processes.
#![cfg(unix)]

use axum::{
    http::StatusCode,
    routing::{get, post, put},
    Json, Router,
};
use serde_json::{json, Value};
use std::process::Command;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use std::time::{Duration, SystemTime};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_opt_in_updates_hints_and_credit_refusals_run_locally_across_processes() {
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let api = Router::new().route("/api/v1/mcp/hosted-tools/enabled", put(|Json(body): Json<Value>| async move {
        assert_eq!(body, json!({"enabled":true, "more_engines":true}));
        Json(json!({"tool":"check", "enabled":true, "available":true,"more_engines":true}))
    })).route("/api/v1/mcp/hosted-tools", get(|| async {
        Json(json!({"tools":[{"name":"check","description":"Remote checks","input_schema":{"type":"object"},
            "transport":{"kind":"stream"}, "attachments":["checkout_snapshot"],
            "command_hints":{"argument":"command","patterns":["^cargo check$"]}}]}))
    })).route("/api/v1/mcp/hosted-tools/check/call", post(move || async move {
        count.fetch_add(1, Ordering::SeqCst);
        (StatusCode::PAYMENT_REQUIRED, Json(json!({"error":{"code":"INSUFFICIENT_CREDITS","details":{"required":20,"available":3}}})))
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, api).await.unwrap() });
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        assert!(Command::new("git")
            .args(args)
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success())
    };
    git(&["init", "-q"]);
    std::fs::write(repo.path().join("Gemfile"), "").unwrap();
    git(&["add", "Gemfile"]);
    git(&[
        "-c",
        "user.name=test",
        "-c",
        "user.email=test@example.test",
        "commit",
        "-qm",
        "initial",
    ]);

    let cli = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args(args)
            .current_dir(repo.path())
            .env_clear()
            .env("HOME", home.path())
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("CONTEXTSTREAM_API_URL", &url)
            .env("CONTEXTSTREAM_API_KEY", "stub-remote-checks")
            .env(
                "CONTEXTSTREAM_WORKSPACE_ID",
                "11111111-1111-4111-8111-111111111111",
            )
            .output()
            .unwrap()
    };
    let setup = cli(&["remote-checks", "on", "--more-engines", "--json"]);
    assert!(setup.status.success());
    let report: Value = serde_json::from_slice(&setup.stdout).unwrap();
    assert_eq!(report["project_languages"]["more"], json!(["Ruby"]));
    for expected_calls in [1, 1, 2] {
        if expected_calls == 2 {
            let marker = std::fs::read_dir(home.path().join(".contextstream"))
                .unwrap()
                .flatten()
                .find(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with("remote-checks-credits-")
                })
                .unwrap();
            std::fs::File::open(marker.path())
                .unwrap()
                .set_modified(SystemTime::now() - Duration::from_secs(181))
                .unwrap();
        }
        let run = cli(&[
            "run",
            "--tool",
            "check",
            "--",
            "printf local-output && exit 7",
        ]);
        assert_eq!(run.status.code(), Some(7));
        assert_eq!(run.stdout, b"local-output");
        assert!(String::from_utf8(run.stderr).unwrap().contains(
            "Running the command locally. Add credits at https://contextstream.io/account/billing"
        ));
        assert_eq!(calls.load(Ordering::SeqCst), expected_calls);
    }
    server.abort();
}

fn add_setup_discovery_routes(router: Router) -> Router {
    const WS: &str = "11111111-1111-4111-8111-111111111111";
    const PROJ: &str = "22222222-2222-4222-8222-222222222222";
    router
        .route(
            "/api/v1/workspaces",
            get(|| async {
                Json(json!({
                    "data": {
                        "items": [{
                            "id": WS,
                            "name": "Testing",
                            "created_at": "2026-01-01T00:00:00Z",
                            "updated_at": "2026-01-01T00:00:00Z"
                        }]
                    }
                }))
            }),
        )
        .route(
            "/api/v1/workspaces/{id}",
            get(|| async {
                Json(json!({
                    "data": {
                        "id": WS,
                        "name": "Testing",
                        "created_at": "2026-01-01T00:00:00Z",
                        "updated_at": "2026-01-01T00:00:00Z"
                    }
                }))
            }),
        )
        .route(
            "/api/v1/projects",
            get(|| async {
                Json(json!({
                    "data": {
                        "items": [],
                        "total": 0,
                        "page": 1,
                        "page_size": 50,
                        "total_pages": 1
                    }
                }))
            })
            .post(|| async {
                Json(json!({
                    "data": {
                        "id": PROJ,
                        "name": "demo",
                        "workspace_id": WS
                    }
                }))
            }),
        )
        .route(
            "/api/v1/projects/{id}",
            get(|| async {
                Json(json!({
                    "data": {
                        "id": PROJ,
                        "name": "demo",
                        "workspace_id": WS
                    }
                }))
            }),
        )
        .fallback(
            |method: axum::http::Method, uri: axum::http::Uri| async move {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": format!("unmocked route: {method} {uri}")})),
                )
            },
        )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_remote_checks_is_opt_in_and_a_refusal_never_breaks_setup() {
    let reads = Arc::new(AtomicUsize::new(0));
    let writes = Arc::new(AtomicUsize::new(0));
    let gets = reads.clone();
    let puts = writes.clone();
    let api = add_setup_discovery_routes(
        Router::new()
            .route("/api/v1/auth/me", get(|| async {
                Json(json!({"data":{"id":"11111111-1111-4111-8111-111111111111","email":"tester@example.test","created_at":"2026-01-01T00:00:00Z"}}))
            }))
            .route("/api/v1/mcp/hosted-tools/enabled", get(move || async move {
                gets.fetch_add(1,Ordering::SeqCst);
                Json(json!({"enabled":false,"more_engines":false,"available":true,"tool":"check"}))
            }).put(move |Json(body): Json<Value>| async move {
                puts.fetch_add(1,Ordering::SeqCst);
                assert_eq!(body,json!({"enabled":true,"more_engines":true}));
                StatusCode::FORBIDDEN
            })),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, api).await.unwrap() });
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("Gemfile"), "").unwrap();
    for (extra, expected) in [
        (&[][..], (0, 0)),
        (&["--dry-run", "--remote-checks"], (1, 0)),
        (&["--remote-checks"], (2, 1)),
    ] {
        let home = tempfile::tempdir().unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args(["setup", "--yes"])
            .args(extra)
            .current_dir(repo.path())
            .env_clear()
            .env("HOME", home.path())
            .env("PATH", "")
            .env("NO_COLOR", "1")
            .env("CONTEXTSTREAM_API_URL", &url)
            .env("CONTEXTSTREAM_API_KEY", "stub-setup")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            (reads.load(Ordering::SeqCst), writes.load(Ordering::SeqCst)),
            expected
        );
        if expected.1 > 0 {
            assert!(String::from_utf8_lossy(&out.stdout)
                .contains("Remote checks are unavailable for this account."));
        }
    }
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_seeds_command_hints_when_remote_checks_already_enabled() {
    let puts = Arc::new(AtomicUsize::new(0));
    let puts_ref = puts.clone();
    let api = add_setup_discovery_routes(
        Router::new()
            .route("/api/v1/auth/me", get(|| async {
                Json(json!({"data":{"id":"11111111-1111-4111-8111-111111111111","email":"tester@example.test","created_at":"2026-01-01T00:00:00Z"}}))
            }))
            .route(
                "/api/v1/mcp/hosted-tools/enabled",
                get(|| async {
                    Json(json!({"enabled":true,"more_engines":false,"available":true,"tool":"check"}))
                })
                .put(move || async move {
                    puts_ref.fetch_add(1, Ordering::SeqCst);
                    StatusCode::INTERNAL_SERVER_ERROR
                }),
            )
            .route("/api/v1/mcp/hosted-tools", get(|| async {
                Json(json!({"tools":[{"name":"check","description":"Remote checks","input_schema":{"type":"object"},
                    "transport":{"kind":"stream"}, "attachments":["checkout_snapshot"],
                    "command_hints":{"argument":"command","patterns":["^cargo check$"]}}]}))
            })),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, api).await.unwrap() });
    let repo = tempfile::tempdir().unwrap();
    std::fs::write(repo.path().join("Cargo.toml"), "[package]\nname=\"demo\"\n").unwrap();

    // --dry-run must NOT write command_hints cache to disk.
    let dry_home = tempfile::tempdir().unwrap();
    let dry_out = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
        .args(["setup", "--yes", "--dry-run", "--remote-checks"])
        .current_dir(repo.path())
        .env_clear()
        .env("HOME", dry_home.path())
        .env("PATH", "")
        .env("NO_COLOR", "1")
        .env("CONTEXTSTREAM_API_URL", &url)
        .env("CONTEXTSTREAM_API_KEY", "stub-already-on")
        .output()
        .unwrap();
    assert!(
        dry_out.status.success(),
        "{}",
        String::from_utf8_lossy(&dry_out.stderr)
    );
    assert!(
        !dry_home.path().join(".contextstream").exists(),
        "dry-run should not write local cache files"
    );

    // Real setup --yes (both with and without --remote-checks) must seed command_hints without PUTting.
    for extra in [&[][..], &["--remote-checks"][..]] {
        let home = tempfile::tempdir().unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args(["setup", "--yes"])
            .args(extra)
            .current_dir(repo.path())
            .env_clear()
            .env("HOME", home.path())
            .env("PATH", "")
            .env("NO_COLOR", "1")
            .env("CONTEXTSTREAM_API_URL", &url)
            .env("CONTEXTSTREAM_API_KEY", "stub-already-on")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let hints_file = std::fs::read_dir(home.path().join(".contextstream"))
            .unwrap()
            .flatten()
            .find(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("hosted-command-hints-")
            });
        let hints_path = hints_file
            .unwrap_or_else(|| {
                panic!("expected hosted-command-hints cache file to be written for extra={extra:?}")
            })
            .path();
        let identity = mcp_client::command_hints::local_identity("stub-already-on");
        let cache = mcp_client::command_hints::read_from(&hints_path, &identity, &url)
            .expect("valid command_hints cache");
        assert!(
            cache.tool_for("cargo check").is_some(),
            "expected cached check tool to match cargo check for extra={extra:?}"
        );
    }
    assert_eq!(puts.load(Ordering::SeqCst), 0);
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_checks_off_on_policy_locked_account_explains_and_exits_nonzero() {
    let api = Router::new().route(
        "/api/v1/mcp/hosted-tools/enabled",
        put(|Json(body): Json<Value>| async move {
            assert_eq!(body, json!({"enabled": false}));
            Json(json!({
                "tool": "check",
                "enabled": true,
                "available": true,
                "policy_locked": true,
                "more_engines": false
            }))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, api).await.unwrap() });
    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();

    let run_cli = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args(args)
            .current_dir(repo.path())
            .env_clear()
            .env("HOME", home.path())
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("CONTEXTSTREAM_API_URL", &url)
            .env("CONTEXTSTREAM_API_KEY", "stub-policy-locked")
            .output()
            .unwrap()
    };

    let plain = run_cli(&["remote-checks", "off"]);
    assert!(
        !plain.status.success(),
        "policy-locked off must exit non-zero"
    );
    let stderr = String::from_utf8_lossy(&plain.stderr);
    assert!(
        stderr.contains("account policy") && stderr.contains("CONTEXTSTREAM_HOSTED_TOOLS=off"),
        "expected policy-locked explanation on stderr, got: {stderr}"
    );

    let as_json = run_cli(&["remote-checks", "off", "--json"]);
    assert!(
        !as_json.status.success(),
        "policy-locked off --json must exit non-zero"
    );
    let report: Value = serde_json::from_slice(&as_json.stdout).unwrap();
    assert_eq!(report["enabled"], json!(true));
    assert_eq!(report["policy_locked"], json!(true));
    assert!(report["notice"]
        .as_str()
        .unwrap_or("")
        .contains("CONTEXTSTREAM_HOSTED_TOOLS=off"));
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_piped_to_head_does_not_abort_with_sigpipe_and_preserves_pipestatus() {
    use futures::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    fn frame(kind: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(5 + payload.len());
        out.push(kind);
        out.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        out.extend_from_slice(payload);
        out
    }

    // Mock micro-compiler WebSocket server: reads J hello + D pack, then streams many O frames
    // (exceeding OS pipe buffer once `head -n 1` closes the pipe) and finishes with J result exit=101.
    let ws_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ws_url = format!("ws://{}/v1/run", ws_listener.local_addr().unwrap());
    let ws_server = tokio::spawn(async move {
        let Ok((stream, _)) = ws_listener.accept().await else {
            return;
        };
        let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
            return;
        };
        // Consume hello + initial pack frames (may arrive in one or two WS binary messages).
        let mut buf = mcp_client::frames::FrameBuffer::default();
        let mut got = 0usize;
        while got < 2 {
            match ws.next().await {
                Some(Ok(Message::Binary(bytes))) => {
                    buf.push(&bytes);
                    while buf.next_frame().ok().flatten().is_some() {
                        got += 1;
                    }
                }
                _ => return,
            }
        }
        // Stream 100 KB of output chunks so the closed pipe is guaranteed to hit EPIPE/SIGPIPE.
        let chunk = "warning: synthetic compiler diagnostic line\n".repeat(128);
        for _ in 0..20 {
            let _ = ws
                .send(Message::Binary(frame(b'O', chunk.as_bytes()).into()))
                .await;
        }
        let result = json!({
            "type": "result",
            "ran": true,
            "exit": 101,
            "seconds": 1.24,
            "cache": "miss"
        })
        .to_string();
        let _ = ws
            .send(Message::Binary(frame(b'J', result.as_bytes()).into()))
            .await;
        // Hold the socket open until the client finishes reading the result frame and closes.
        while ws.next().await.is_some() {}
    });

    let ws_url_for_api = ws_url.clone();
    let api = Router::new()
        .route(
            "/api/v1/mcp/hosted-tools",
            get(|| async {
                Json(json!({"tools":[{"name":"check","description":"Remote checks","input_schema":{"type":"object"},
                    "transport":{"kind":"stream"}, "attachments":["checkout_snapshot"],
                    "command_hints":{"argument":"command","patterns":["^cargo check$"]}}]}))
            }),
        )
        .route(
            "/api/v1/mcp/hosted-tools/check/call",
            post(move || {
                let ws_url = ws_url_for_api.clone();
                async move {
                    Json(json!({
                        "call": {"id": "call-pipe-1"},
                        "stream": {
                            "url": ws_url,
                            "token": "mct_stub",
                            "protocol": "frames/1",
                            "request": {
                                "type": "hello",
                                "v": 1,
                                "id": "mc-0123456789abcdef",
                                "repo_key": "demo-01234567",
                                "command": "cargo check",
                                "cwd": "",
                                "timeout_s": 30,
                                "wait_s": 10
                            }
                        }
                    }))
                }
            }),
        );
    let api_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_url = format!("http://{}", api_listener.local_addr().unwrap());
    let api_server = tokio::spawn(async move { axum::serve(api_listener, api).await.unwrap() });

    let home = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        assert!(Command::new("git")
            .args(args)
            .current_dir(repo.path())
            .status()
            .unwrap()
            .success())
    };
    git(&["init", "-q"]);
    std::fs::write(repo.path().join("Cargo.toml"), "[package]\nname=\"demo\"\n").unwrap();
    git(&["add", "Cargo.toml"]);
    git(&[
        "-c",
        "user.name=test",
        "-c",
        "user.email=test@example.test",
        "commit",
        "-qm",
        "initial",
    ]);

    let bin = env!("CARGO_BIN_EXE_contextstream-mcp");
    let script = format!(
        "'{bin}' run --tool 'check' -- 'cargo check' 2>&1 | head -n 1; echo \"PIPESTATUS=${{PIPESTATUS[0]}}\""
    );
    let out = tokio::process::Command::new("bash")
        .args(["-c", &script])
        .current_dir(repo.path())
        .env_clear()
        .env("HOME", home.path())
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("CONTEXTSTREAM_API_URL", &api_url)
        .env("CONTEXTSTREAM_API_KEY", "stub-pipe-test")
        .env(
            "CONTEXTSTREAM_WORKSPACE_ID",
            "11111111-1111-4111-8111-111111111111",
        )
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.lines().any(|l| l.trim() == "PIPESTATUS=101"),
        "expected remote exit 101 (not 141 SIGPIPE), got stdout={stdout:?} stderr={:?}",
        String::from_utf8_lossy(&out.stderr)
    );
    // The remote-run receipt must also have been written despite stdout/stderr closing early.
    let receipt_file = std::fs::read_dir(home.path().join(".contextstream"))
        .unwrap()
        .flatten()
        .find(|e| {
            e.file_name()
                .to_string_lossy()
                .starts_with("remote-run-receipt-")
        });
    assert!(
        receipt_file.is_some(),
        "expected remote-run receipt file to be written even when piped to head -n 1"
    );

    api_server.abort();
    ws_server.abort();
}
