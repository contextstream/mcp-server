//! Verify `setup --yes` persists validated `CONTEXTSTREAM_API_KEY` credentials
//! so subsequent `doctor` / `update` / `configure` invocations work in new shells,
//! while `--dry-run` previews without writing `~/.contextstream/credentials.json`.
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
                            "email": "qa-scripted@example.test",
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
    let heartbeat = home.join(".contextstream/watch.heartbeat.json");
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

#[tokio::test]
async fn setup_yes_with_env_api_key_persists_credentials_for_subsequent_doctor() {
    const SECRET: &str = "env-only-setup-yes-secret";
    let (api_url, server) = spawn_mock_setup_server(SECRET).await;
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path().to_path_buf();
    let setup_api_url = api_url.clone();

    let setup_output = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args([
                "setup",
                "--yes",
                "--account-only",
                "--editors=codex",
                &format!("--workspace-id={WORKSPACE_ID}"),
            ])
            .env_clear()
            .env("HOME", &home_path)
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("CONTEXTSTREAM_API_KEY", SECRET)
            .env("CONTEXTSTREAM_API_URL", &setup_api_url)
            .env("CONTEXTSTREAM_NO_BROWSER", "1")
            .env("NO_COLOR", "1")
            .current_dir(&home_path)
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
        "setup --yes failed:\nstdout:\n{setup_stdout}\nstderr:\n{setup_stderr}"
    );
    assert!(!setup_stdout.contains(SECRET), "{setup_stdout}");
    assert!(!setup_stderr.contains(SECRET), "{setup_stderr}");
    assert!(
        setup_stdout.contains("saved to ~/.contextstream/credentials.json"),
        "expected setup --yes output to confirm saved credentials:\n{setup_stdout}"
    );

    let credentials_path = home.path().join(".contextstream/credentials.json");
    assert!(
        credentials_path.is_file(),
        "setup --yes should write ~/.contextstream/credentials.json"
    );
    assert_eq!(
        std::fs::metadata(&credentials_path)
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    let saved: Value =
        serde_json::from_str(&std::fs::read_to_string(&credentials_path).unwrap()).unwrap();
    assert_eq!(saved["api_key"], SECRET);
    assert_eq!(saved["api_url"], api_url);
    assert!(saved["saved_at"].as_str().is_some_and(|ts| !ts.is_empty()));

    // Next shell session: neither CONTEXTSTREAM_API_KEY nor CONTEXTSTREAM_API_URL is set.
    let doctor_home = home.path().to_path_buf();
    let doctor_output = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args(["doctor", "--only-configured", "--json"])
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
        "subsequent doctor without CONTEXTSTREAM_API_KEY failed:\nstdout:\n{doctor_stdout}\nstderr:\n{doctor_stderr}"
    );
    let report: Value = serde_json::from_str(&doctor_stdout).unwrap();
    assert_eq!(report["credentials"]["status"], "pass", "{report:#}");
    assert_eq!(report["fail"], 0, "{report:#}");
}

#[tokio::test]
async fn setup_yes_dry_run_with_env_api_key_does_not_write_credentials_file() {
    const SECRET: &str = "dry-run-setup-yes-secret";
    let (api_url, server) = spawn_mock_setup_server(SECRET).await;
    let home = tempfile::tempdir().unwrap();
    let home_path = home.path().to_path_buf();

    let output = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args([
                "setup",
                "--yes",
                "--dry-run",
                "--account-only",
                "--editors=codex",
                &format!("--workspace-id={WORKSPACE_ID}"),
            ])
            .env_clear()
            .env("HOME", &home_path)
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("CONTEXTSTREAM_API_KEY", SECRET)
            .env("CONTEXTSTREAM_API_URL", &api_url)
            .env("CONTEXTSTREAM_NO_BROWSER", "1")
            .env("NO_COLOR", "1")
            .current_dir(&home_path)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    })
    .await
    .unwrap();

    stop_spawned_watch_helper_if_any(home.path());
    server.abort();

    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        output.status.success(),
        "setup --yes --dry-run failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(!stdout.contains(SECRET), "{stdout}");
    assert!(!stderr.contains(SECRET), "{stderr}");
    assert!(
        !home.path().join(".contextstream/credentials.json").exists(),
        "setup --yes --dry-run must not write ~/.contextstream/credentials.json"
    );
    assert!(
        stdout.contains("credentials.json"),
        "expected dry-run plan to list credentials.json:\n{stdout}"
    );
}
