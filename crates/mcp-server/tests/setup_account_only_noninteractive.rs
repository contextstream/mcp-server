//! Regression tests for issue #254:
//! 1. `setup --yes --account-only` succeeds on multi-workspace accounts without
//!    requiring `--workspace-id`, leaving global editor configuration healthy.
//! 2. When `setup --yes` fails during workspace/project preflight (e.g. a
//!    project setup on a multi-workspace account without `--workspace-id`), it
//!    must not mutate `~/.contextstream/installation.json` before aborting.
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

async fn spawn_multi_workspace_mock_server(
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
                            "email": "qa-multi-ws@example.test",
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
                                    "name": "Personal",
                                    "created_at": "2026-01-01T00:00:00Z",
                                    "updated_at": "2026-01-01T00:00:00Z"
                                },
                                {
                                    "id": "44444444-4444-4444-8444-444444444444",
                                    "name": "Testing",
                                    "created_at": "2026-01-01T00:00:00Z",
                                    "updated_at": "2026-01-01T00:00:00Z"
                                },
                                {
                                    "id": "33333333-3333-4333-8333-333333333333",
                                    "name": "Team",
                                    "created_at": "2026-01-01T00:00:00Z",
                                    "updated_at": "2026-01-01T00:00:00Z"
                                }
                            ],
                            "total": 3,
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

fn save_credentials_via_stdin(home: &Path, api_url: &str, secret: &str) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
        .args(["configure", "--api-key-stdin"])
        .env_clear()
        .env("HOME", home)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("CONTEXTSTREAM_API_URL", api_url)
        .env("NO_COLOR", "1")
        .current_dir(home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(format!("{secret}\n").as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "configure --api-key-stdin failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test]
async fn setup_yes_account_only_succeeds_on_multi_workspace_account_and_passes_global_doctor() {
    const SECRET: &str = "multi-ws-account-only-secret";
    let (api_url, server) = spawn_multi_workspace_mock_server(SECRET).await;
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path().to_path_buf();

    let cfg_home = home_path.clone();
    let cfg_url = api_url.clone();
    tokio::task::spawn_blocking(move || save_credentials_via_stdin(&cfg_home, &cfg_url, SECRET))
        .await
        .unwrap();

    let setup_home = home_path.clone();
    let setup_output = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args(["setup", "--yes", "--account-only", "--editors=claude,codex"])
            .env_clear()
            .env("HOME", &setup_home)
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("CONTEXTSTREAM_NO_BROWSER", "1")
            .env("NO_COLOR", "1")
            .current_dir(&setup_home)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    })
    .await
    .unwrap();

    let setup_stdout = String::from_utf8(setup_output.stdout).unwrap();
    let setup_stderr = String::from_utf8(setup_output.stderr).unwrap();
    assert!(
        setup_output.status.success(),
        "setup --yes --account-only failed on multi-workspace account:\nstdout:\n{setup_stdout}\nstderr:\n{setup_stderr}"
    );
    assert!(!setup_stdout.contains(SECRET), "{setup_stdout}");
    assert!(!setup_stderr.contains(SECRET), "{setup_stderr}");

    let installation: Value = serde_json::from_str(
        &std::fs::read_to_string(home.path().join(".contextstream/installation.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        installation["configured_clients"],
        json!(["claude", "codex"])
    );
    assert!(home.path().join(".claude.json").is_file());
    assert!(home.path().join(".codex/config.toml").is_file());

    let doctor_home = home_path.clone();
    let doctor_output = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args(["doctor", "--scope=global", "--json"])
            .env_clear()
            .env("HOME", &doctor_home)
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("NO_COLOR", "1")
            .current_dir(&doctor_home)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    })
    .await
    .unwrap();

    stop_spawned_watch_helper_if_any(home.path());
    server.abort();

    let doctor_stdout = String::from_utf8(doctor_output.stdout).unwrap();
    let doctor_stderr = String::from_utf8(doctor_output.stderr).unwrap();
    assert!(
        doctor_output.status.success(),
        "doctor --scope=global failed after setup --yes --account-only:\nstdout:\n{doctor_stdout}\nstderr:\n{doctor_stderr}"
    );
    let report: Value = serde_json::from_str(&doctor_stdout).unwrap();
    assert_eq!(report["credentials"]["status"], "pass", "{report:#}");
    assert_eq!(report["fail"], 0, "{report:#}");
}

#[tokio::test]
async fn setup_yes_preflight_failure_does_not_mutate_installation_json() {
    const SECRET: &str = "multi-ws-preflight-fail-secret";
    let (api_url, server) = spawn_multi_workspace_mock_server(SECRET).await;
    let home = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("README.md"), "# test\n").unwrap();

    let home_path = home.path().to_path_buf();
    let cfg_home = home_path.clone();
    let cfg_url = api_url.clone();
    tokio::task::spawn_blocking(move || save_credentials_via_stdin(&cfg_home, &cfg_url, SECRET))
        .await
        .unwrap();

    let setup_home = home_path.clone();
    let project_path = project.path().to_path_buf();
    let setup_output = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args(["setup", "--yes", "--editors=claude,codex"])
            .env_clear()
            .env("HOME", &setup_home)
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
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

    assert!(
        !setup_output.status.success(),
        "project setup --yes without --workspace-id on multi-workspace account must fail"
    );
    let stderr = String::from_utf8(setup_output.stderr).unwrap();
    assert!(
        stderr.contains("needs an unambiguous workspace"),
        "unexpected stderr:\n{stderr}"
    );

    let installation_path = home.path().join(".contextstream/installation.json");
    if installation_path.exists() {
        let installation: Value =
            serde_json::from_str(&std::fs::read_to_string(&installation_path).unwrap()).unwrap();
        assert_eq!(
            installation["configured_clients"],
            json!([]),
            "aborted setup must not persist configured_clients before preflight succeeds: {installation:#}"
        );
        assert!(
            installation.get("last_install_completed_at").is_none()
                || installation["last_install_completed_at"].is_null(),
            "aborted setup must not record last_install_completed_at: {installation:#}"
        );
    }
    assert!(
        !home
            .path()
            .join(".contextstream/setup-transport.json")
            .exists(),
        "aborted setup must not write setup-transport.json before preflight succeeds"
    );
}
