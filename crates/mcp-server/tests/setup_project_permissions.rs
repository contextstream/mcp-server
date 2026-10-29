//! Regression test for issue #246:
//! Non-secret project rules and config files created by `setup` respect the
//! caller's `umask` (e.g. `0644` under `umask 0022`), while secret-bearing MCP
//! configs and `credentials.json` remain `0600`.
#![cfg(unix)]

use axum::{
    extract::{Path as AxumPath, Query},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Stdio},
};

const WORKSPACE_ID: &str = "11111111-1111-4111-8111-111111111111";
const PROJECT_ID: &str = "33333333-3333-4333-8333-333333333333";

async fn spawn_mock_setup_server(
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
                            "email": "qa-perms@example.test",
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
            "/api/v1/workspaces/{id}",
            get(move |headers: HeaderMap, AxumPath(id): AxumPath<String>| async move {
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
                            "id": id,
                            "name": "Testing",
                            "created_at": "2026-01-01T00:00:00Z",
                            "updated_at": "2026-01-01T00:00:00Z"
                        }
                    })),
                )
            }),
        )
        .route(
            "/api/v1/projects",
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
                            "items": [],
                            "total": 0,
                            "page": 1,
                            "page_size": 50,
                            "total_pages": 1
                        }
                    })),
                )
            })
            .post(move |headers: HeaderMap, Json(body): Json<Value>| async move {
                if !check_auth(&headers) {
                    return (
                        StatusCode::UNAUTHORIZED,
                        Json(json!({"error": "invalid api key"})),
                    );
                }
                let name = body
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("demo")
                    .to_string();
                (
                    StatusCode::OK,
                    Json(json!({
                        "data": {
                            "id": PROJECT_ID,
                            "name": name,
                            "workspace_id": WORKSPACE_ID
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

fn init_git_repo(dir: &Path) {
    assert!(Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir)
        .status()
        .unwrap()
        .success());
    std::fs::write(dir.join("README.md"), "# demo\n").unwrap();
    assert!(Command::new("git")
        .args(["add", "."])
        .current_dir(dir)
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .args([
            "-c",
            "user.name=qa",
            "-c",
            "user.email=qa@example.test",
            "commit",
            "-qm",
            "init",
        ])
        .current_dir(dir)
        .status()
        .unwrap()
        .success());
}

fn stop_spawned_watch_helper_if_any(home: &Path) {
    for rel in [
        ".contextstream/watch.heartbeat.json",
        ".contextstream/watch/heartbeat.json",
    ] {
        let heartbeat = home.join(rel);
        if let Ok(raw) = std::fs::read_to_string(heartbeat) {
            if let Ok(parsed) = serde_json::from_str::<Value>(&raw) {
                if let Some(pid) = parsed.get("pid").and_then(Value::as_u64) {
                    let _ = Command::new("kill")
                        .args(["-TERM", &pid.to_string()])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status();
                }
            }
        }
    }
}

#[tokio::test]
async fn setup_respects_umask_for_project_rules_and_keeps_secret_configs_private() {
    const SECRET: &str = "umask-test-secret-key";
    let (api_url, server) = spawn_mock_setup_server(SECRET).await;
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    init_git_repo(project.path());

    let home_path = home.path().to_path_buf();
    let project_path = project.path().to_path_buf();
    let bin = env!("CARGO_BIN_EXE_contextstream-mcp").to_string();

    let output = tokio::task::spawn_blocking(move || {
        Command::new("sh")
            .args([
                "-c",
                "umask 0022 && exec \"$1\" setup --yes --no-index --editors=claude,cursor,windsurf,codex,gemini,qwen,cline,aider,opencode --workspace-id=\"$2\"",
                "sh",
                &bin,
                WORKSPACE_ID,
            ])
            .env_clear()
            .env("HOME", &home_path)
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("CONTEXTSTREAM_API_KEY", SECRET)
            .env("CONTEXTSTREAM_API_URL", &api_url)
            .env("CONTEXTSTREAM_NO_BROWSER", "1")
            .env("NO_COLOR", "1")
            .current_dir(&project_path)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    })
    .await
    .unwrap();

    stop_spawned_watch_helper_if_any(home.path());
    server.abort();

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "setup failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    for rel in [
        "AGENTS.md",
        "CLAUDE.md",
        "GEMINI.md",
        "QWEN.md",
        ".clinerules",
        ".aider.conf.yml",
        "opencode.json",
        ".contextstream/config.json",
        ".contextstream/rules.md",
        ".cursor/rules/contextstream.mdc",
        ".windsurf/rules/contextstream.md",
    ] {
        let file = project.path().join(rel);
        let mode = std::fs::metadata(&file)
            .unwrap_or_else(|e| panic!("expected {rel} to exist: {e}"))
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o644,
            "expected {rel} to respect umask 0022 (0644), got {mode:04o}"
        );
    }

    for secret_rel in [".mcp.json", ".cursor/mcp.json"] {
        let file = project.path().join(secret_rel);
        let mode = std::fs::metadata(&file)
            .unwrap_or_else(|e| panic!("expected {secret_rel} to exist: {e}"))
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(
            mode, 0o600,
            "expected secret-bearing {secret_rel} to stay 0600, got {mode:04o}"
        );
    }

    let creds_mode = std::fs::metadata(home.path().join(".contextstream/credentials.json"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(creds_mode, 0o600);
}
