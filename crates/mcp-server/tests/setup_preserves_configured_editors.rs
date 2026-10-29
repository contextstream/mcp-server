//! Regression test for issue #240:
//! Re-running `contextstream-mcp setup` without `--editors` preserves the
//! editors previously recorded in `~/.contextstream/installation.json` rather
//! than replacing them solely with fresh filesystem auto-detection.
#![cfg(unix)]

use axum::{
    extract::Query,
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

async fn spawn_mock_server(
    expected_api_key: &'static str,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_url = format!("http://{}", listener.local_addr().unwrap());

    let check_auth = move |headers: &HeaderMap| -> bool {
        headers
            .get("x-api-key")
            .and_then(|value| value.to_str().ok())
            == Some(expected_api_key)
    };

    let app = Router::new()
        .route(
            "/api/v1/auth/me",
            get(move |headers: HeaderMap| async move {
                if !check_auth(&headers) {
                    return (
                        StatusCode::UNAUTHORIZED,
                        Json(json!({"error": "invalid api key"})),
                    );
                }
                (
                    StatusCode::OK,
                    Json(json!({
                        "data": {
                            "id": "22222222-2222-4222-8222-222222222222",
                            "email": "qa-editors@example.test",
                            "created_at": "2026-01-01T00:00:00Z"
                        }
                    })),
                )
            }),
        )
        .route(
            "/api/v1/users/me/learning-consent",
            get(move |headers: HeaderMap| async move {
                if !check_auth(&headers) {
                    return (
                        StatusCode::UNAUTHORIZED,
                        Json(json!({"error": "invalid api key"})),
                    );
                }
                (StatusCode::OK, Json(json!({"data": {"enabled": false}})))
            }),
        )
        .route(
            "/api/v1/workspaces",
            get(move |headers: HeaderMap| async move {
                if !check_auth(&headers) {
                    return (
                        StatusCode::UNAUTHORIZED,
                        Json(json!({"error": "invalid api key"})),
                    );
                }
                (
                    StatusCode::OK,
                    Json(json!({
                        "data": {
                            "items": [
                                {
                                    "id": "11111111-1111-4111-8111-111111111111",
                                    "name": "Testing",
                                    "created_at": "2026-01-01T00:00:00Z",
                                    "updated_at": "2026-01-01T00:00:00Z"
                                }
                            ],
                            "total": 1,
                            "page": 1,
                            "page_size": 50,
                            "total_pages": 1
                        }
                    })),
                )
            }),
        )
        .route(
            "/api/v1/harness-readiness/status",
            get(
                move |headers: HeaderMap, Query(params): Query<HashMap<String, String>>| async move {
                    if !check_auth(&headers) {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({"error": "invalid api key"})),
                        );
                    }
                    let installation_id = params.get("installation_id").cloned().unwrap_or_default();
                    let harness_id = params.get("harness_id").cloned().unwrap_or_default();
                    (
                        StatusCode::OK,
                        Json(json!({
                            "success": true,
                            "data": {
                                "event_schema_version": 1,
                                "installation_id": installation_id,
                                "harness_id": harness_id,
                                "evidence": []
                            }
                        })),
                    )
                },
            ),
        )
        .route(
            "/api/v1/harness-readiness/events",
            post(|| async {
                (
                    StatusCode::OK,
                    Json(json!({"success": true, "data": {"accepted": true}})),
                )
            }),
        );

    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (api_url, handle)
}

fn stop_spawned_watch_helper_if_any(home: &Path) {
    let heartbeat = home.join(".contextstream/watch/heartbeat.json");
    if let Ok(raw) = std::fs::read_to_string(heartbeat) {
        if let Ok(value) = serde_json::from_str::<Value>(&raw) {
            if let Some(pid) = value.get("pid").and_then(Value::as_u64) {
                unsafe {
                    let _ = libc::kill(pid as i32, libc::SIGTERM);
                }
            }
        }
    }
}

#[tokio::test]
async fn rerunning_setup_without_editors_flag_preserves_configured_clients() {
    const SECRET: &str = "preserve-editors-secret";
    let (api_url, server) = spawn_mock_server(SECRET).await;
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path().to_path_buf();

    // Save credentials via stdin.
    let cfg_home = home_path.clone();
    let cfg_url = api_url.clone();
    tokio::task::spawn_blocking(move || {
        let mut child = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args(["configure", "--api-key-stdin"])
            .env_clear()
            .env("HOME", &cfg_home)
            .env("PATH", "/usr/bin:/bin")
            .env("CONTEXTSTREAM_API_URL", &cfg_url)
            .env("NO_COLOR", "1")
            .current_dir(&cfg_home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(format!("{SECRET}\n").as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
    })
    .await
    .unwrap();

    // 1. Initial setup explicitly configures both Claude Code and Codex CLI.
    let first_home = home_path.clone();
    let first = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args(["setup", "--yes", "--account-only", "--editors=claude,codex"])
            .env_clear()
            .env("HOME", &first_home)
            .env("PATH", "/usr/bin:/bin")
            .env("CONTEXTSTREAM_NO_BROWSER", "1")
            .env("NO_COLOR", "1")
            .current_dir(&first_home)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        first.status.success(),
        "initial setup failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );

    let installation_path = home.path().join(".contextstream/installation.json");
    let before: Value =
        serde_json::from_str(&std::fs::read_to_string(&installation_path).unwrap()).unwrap();
    assert_eq!(before["configured_clients"], json!(["claude", "codex"]));

    // Simulate Claude Code having ~/.claude on disk while Codex has no binary/dir
    // auto-detection markers yet.
    std::fs::create_dir_all(home.path().join(".claude")).unwrap();
    let _ = std::fs::remove_dir_all(home.path().join(".codex"));

    // 2. Re-run `setup --yes` without `--editors`.
    let second_home = home_path.clone();
    let second = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args(["setup", "--yes", "--account-only"])
            .env_clear()
            .env("HOME", &second_home)
            .env("PATH", "/usr/bin:/bin")
            .env("CONTEXTSTREAM_NO_BROWSER", "1")
            .env("NO_COLOR", "1")
            .current_dir(&second_home)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    })
    .await
    .unwrap();

    stop_spawned_watch_helper_if_any(home.path());
    server.abort();

    let second_stdout = String::from_utf8(second.stdout).unwrap();
    let second_stderr = String::from_utf8(second.stderr).unwrap();
    assert!(
        second.status.success(),
        "second setup failed:\nstdout:\n{second_stdout}\nstderr:\n{second_stderr}"
    );
    assert!(
        second_stdout.contains("configured by setup"),
        "expected provenance 'configured by setup' on re-run:\n{second_stdout}"
    );

    let after: Value =
        serde_json::from_str(&std::fs::read_to_string(&installation_path).unwrap()).unwrap();
    assert_eq!(
        after["configured_clients"],
        json!(["claude", "codex"]),
        "re-running setup without --editors must not drop codex: {after:#}"
    );
    assert!(
        home.path().join(".codex/config.toml").is_file(),
        "re-running setup must still configure saved editors"
    );
}
