//! Regression tests for `setup --profile` / `setup --profile-file` (#266, #265).
#![cfg(unix)]

use axum::{
    extract::{Path as AxumPath, Query},
    http::{HeaderMap, StatusCode},
    routing::{delete, get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::Path,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
};

const WORKSPACE_ID: &str = "11111111-1111-4111-8111-111111111111";
const USER_ID: &str = "22222222-2222-4222-8222-222222222222";
const PROJECT_ID: &str = "33333333-3333-4333-8333-333333333333";
const DEVICE_ID: &str = "44444444-4444-4444-8444-444444444444";

#[derive(Default)]
struct MockHits {
    routes: Vec<String>,
    completion_bodies: Vec<Value>,
}

async fn spawn_mock_profile_server(
    expected_keys: &'static [&'static str],
) -> (String, Arc<Mutex<MockHits>>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_url = format!("http://{}", listener.local_addr().unwrap());

    let hits = Arc::new(Mutex::new(MockHits::default()));
    let projects: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));

    let check_auth = move |headers: &HeaderMap| -> bool {
        headers
            .get("x-api-key")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|key| expected_keys.contains(&key))
    };

    let hits_me = Arc::clone(&hits);
    let hits_ws = Arc::clone(&hits);
    let hits_proj_get = Arc::clone(&hits);
    let hits_proj_post = Arc::clone(&hits);
    let hits_proj_id = Arc::clone(&hits);
    let hits_complete = Arc::clone(&hits);
    let hits_revoke = Arc::clone(&hits);

    let list_state = Arc::clone(&projects);
    let create_state = Arc::clone(&projects);
    let get_state = Arc::clone(&projects);

    let app = Router::new()
        .route(
            "/api/v1/auth/me",
            get(move |headers: HeaderMap| {
                let hits = Arc::clone(&hits_me);
                async move {
                    hits.lock().unwrap().routes.push("GET /api/v1/auth/me".to_string());
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
                                "id": USER_ID,
                                "email": "selfhosted@example.test",
                                "created_at": "2026-01-01T00:00:00Z"
                            }
                        })),
                    )
                }
            }),
        )
        .route(
            "/api/v1/auth/api-keys/{id}",
            delete(move |headers: HeaderMap, AxumPath(id): AxumPath<String>| {
                let hits = Arc::clone(&hits_revoke);
                async move {
                    hits.lock()
                        .unwrap()
                        .routes
                        .push(format!("DELETE /api/v1/auth/api-keys/{id}"));
                    if !check_auth(&headers) {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({"error": "invalid api key"})),
                        );
                    }
                    (StatusCode::OK, Json(json!({"data": {"deleted": true}})))
                }
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
            get(move |headers: HeaderMap| {
                let hits = Arc::clone(&hits_ws);
                async move {
                    hits.lock()
                        .unwrap()
                        .routes
                        .push("GET /api/v1/workspaces".to_string());
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
                                        "id": WORKSPACE_ID,
                                        "name": "Testing",
                                        "created_at": "2026-01-01T00:00:00Z",
                                        "updated_at": "2026-01-01T00:00:00Z"
                                    }
                                ]
                            }
                        })),
                    )
                }
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
            get(
                move |headers: HeaderMap, Query(params): Query<HashMap<String, String>>| {
                    let hits = Arc::clone(&hits_proj_get);
                    let list_state = Arc::clone(&list_state);
                    async move {
                        hits.lock()
                            .unwrap()
                            .routes
                            .push("GET /api/v1/projects".to_string());
                        if !check_auth(&headers) {
                            return (
                                StatusCode::UNAUTHORIZED,
                                Json(json!({"error": "invalid api key"})),
                            );
                        }
                        let page = params
                            .get("page")
                            .and_then(|raw| raw.parse::<u32>().ok())
                            .unwrap_or(1);
                        let stored = list_state.lock().unwrap().clone();
                        let total = stored.len();
                        let items = if page == 1 { stored } else { Vec::new() };
                        (
                            StatusCode::OK,
                            Json(json!({
                                "data": {
                                    "items": items,
                                    "total": total,
                                    "page": page,
                                    "page_size": 50,
                                    "total_pages": 1
                                }
                            })),
                        )
                    }
                },
            )
            .post(move |headers: HeaderMap, Json(body): Json<Value>| {
                let hits = Arc::clone(&hits_proj_post);
                let create_state = Arc::clone(&create_state);
                async move {
                    hits.lock()
                        .unwrap()
                        .routes
                        .push("POST /api/v1/projects".to_string());
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
                    let created = json!({
                        "id": PROJECT_ID,
                        "name": name,
                        "workspace_id": WORKSPACE_ID
                    });
                    create_state.lock().unwrap().push(created.clone());
                    (StatusCode::OK, Json(json!({ "data": created })))
                }
            }),
        )
        .route(
            "/api/v1/projects/{id}",
            get(move |headers: HeaderMap, AxumPath(id): AxumPath<String>| {
                let hits = Arc::clone(&hits_proj_id);
                let get_state = Arc::clone(&get_state);
                async move {
                    hits.lock()
                        .unwrap()
                        .routes
                        .push(format!("GET /api/v1/projects/{id}"));
                    if !check_auth(&headers) {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({"error": "invalid api key"})),
                        );
                    }
                    let stored = get_state.lock().unwrap().clone();
                    if let Some(found) = stored
                        .into_iter()
                        .find(|proj| proj.get("id").and_then(Value::as_str) == Some(id.as_str()))
                    {
                        (StatusCode::OK, Json(json!({ "data": found })))
                    } else {
                        (
                            StatusCode::NOT_FOUND,
                            Json(json!({ "error": "project not found" })),
                        )
                    }
                }
            }),
        )
        .route(
            "/api/v1/setup/devices/{id}/complete",
            post(
                move |headers: HeaderMap, AxumPath(id): AxumPath<String>, Json(body): Json<Value>| {
                    let hits = Arc::clone(&hits_complete);
                    async move {
                        if !check_auth(&headers) {
                            return (
                                StatusCode::UNAUTHORIZED,
                                Json(json!({"error": "invalid api key"})),
                            );
                        }
                        let mut guard = hits.lock().unwrap();
                        guard
                            .routes
                            .push(format!("POST /api/v1/setup/devices/{id}/complete"));
                        guard.completion_bodies.push(body);
                        (StatusCode::OK, Json(json!({"data": {"ok": true}})))
                    }
                },
            ),
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

    let join = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (api_url, hits, join)
}

fn init_git_repo(path: &Path) {
    for args in [
        &["init", "-q"][..],
        &["config", "user.email", "qa@example.test"][..],
        &["config", "user.name", "QA"][..],
    ] {
        assert!(Command::new("git")
            .args(args)
            .current_dir(path)
            .status()
            .unwrap()
            .success());
    }
    std::fs::write(path.join("README.md"), "# profile\n").unwrap();
    for args in [&["add", "."][..], &["commit", "-qm", "init"][..]] {
        assert!(Command::new("git")
            .args(args)
            .current_dir(path)
            .status()
            .unwrap()
            .success());
    }
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_profile_file_uses_payload_api_url_for_all_requests() {
    const EXISTING_KEY: &str = "cs_selfhosted_existing_key_266";
    const MINTED_KEY: &str = "cs_selfhosted_minted_key_266";
    static KEYS: &[&str] = &[EXISTING_KEY, MINTED_KEY];

    let (api_url, hits, server) = spawn_mock_profile_server(KEYS).await;
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let proj = temp.path().join("proj");
    std::fs::create_dir_all(home.join(".claude/projects")).unwrap();
    std::fs::create_dir_all(&proj).unwrap();
    init_git_repo(&proj);

    let profile_path = temp.path().join("selfhosted-profile.json");
    std::fs::write(
        &profile_path,
        serde_json::to_string_pretty(&json!({
            "device_id": DEVICE_ID,
            "user_id": USER_ID,
            "email": "selfhosted@example.test",
            "api_key": {"id": "55555555-5555-4555-8555-555555555555", "secret": MINTED_KEY},
            "api_url": api_url,
            "profile": {
                "editors": ["claude"],
                "workspace_id": WORKSPACE_ID,
                "workspace_name": "Testing"
            }
        }))
        .unwrap(),
    )
    .unwrap();

    // 1. Fresh HOME with CONTEXTSTREAM_API_URL unset: all workspace, project, and
    // device-completion calls must hit `payload.api_url`.
    let run_home = home.clone();
    let run_proj = proj.clone();
    let run_pf = profile_path.clone();
    let out = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args(["setup", "--profile-file", run_pf.to_str().unwrap()])
            .env("HOME", &run_home)
            .env("USERPROFILE", &run_home)
            .env("CONTEXTSTREAM_WATCH_DISABLED", "1")
            .env("NO_COLOR", "1")
            .env_remove("CONTEXTSTREAM_API_URL")
            .env_remove("CONTEXTSTREAM_API_KEY")
            .current_dir(&run_proj)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        out.status.success(),
        "setup --profile-file failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    // 2. Re-run with pre-existing credentials on the same self-hosted server:
    // `resolve_credentials` must validate `client.me()` against `payload.api_url`
    // and retire the duplicate minted key on `payload.api_url`.
    std::fs::write(
        home.join(".contextstream/credentials.json"),
        serde_json::to_string_pretty(&json!({
            "api_key": EXISTING_KEY,
            "api_url": api_url,
            "saved_at": "2026-01-01T00:00:00Z"
        }))
        .unwrap(),
    )
    .unwrap();
    let rerun_home = home.clone();
    let rerun_proj = proj.clone();
    let rerun_pf = profile_path.clone();
    let rerun_out = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args([
                "setup",
                "--profile-file",
                rerun_pf.to_str().unwrap(),
                "--account-only",
            ])
            .env("HOME", &rerun_home)
            .env("USERPROFILE", &rerun_home)
            .env("CONTEXTSTREAM_WATCH_DISABLED", "1")
            .env("NO_COLOR", "1")
            .env_remove("CONTEXTSTREAM_API_URL")
            .env_remove("CONTEXTSTREAM_API_KEY")
            .current_dir(&rerun_proj)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        rerun_out.status.success(),
        "rerun setup --profile-file --account-only failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&rerun_out.stdout),
        String::from_utf8_lossy(&rerun_out.stderr)
    );

    let recorded = hits.lock().unwrap();
    for expected_route in [
        "GET /api/v1/workspaces",
        "POST /api/v1/projects",
        &format!("POST /api/v1/setup/devices/{DEVICE_ID}/complete"),
        "GET /api/v1/auth/me",
        "DELETE /api/v1/auth/api-keys/55555555-5555-4555-8555-555555555555",
    ] {
        assert!(
            recorded.routes.iter().any(|r| r == expected_route),
            "expected {expected_route} to reach payload.api_url, got: {:?}",
            recorded.routes
        );
    }
    drop(recorded);

    stop_spawned_watch_helper_if_any(&home);
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_profile_file_dry_run_previews_cleanly_without_false_doctor_failures() {
    const MINTED_KEY: &str = "cs_dry_run_profile_key_265";
    static KEYS: &[&str] = &[MINTED_KEY];

    let (api_url, hits, server) = spawn_mock_profile_server(KEYS).await;
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("dry-home");
    let proj = temp.path().join("dry-proj");
    std::fs::create_dir_all(home.join(".claude/projects")).unwrap();
    std::fs::create_dir_all(&proj).unwrap();
    init_git_repo(&proj);

    let profile_path = temp.path().join("dry-profile.json");
    std::fs::write(
        &profile_path,
        serde_json::to_string_pretty(&json!({
            "device_id": DEVICE_ID,
            "user_id": USER_ID,
            "email": "dryrun@example.test",
            "api_key": {"id": "", "secret": MINTED_KEY},
            "api_url": api_url,
            "profile": {
                "editors": ["claude"],
                "workspace_id": WORKSPACE_ID,
                "workspace_name": "Testing"
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let run_home = home.clone();
    let run_proj = proj.clone();
    let run_pf = profile_path.clone();
    let out = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args([
                "setup",
                "--profile-file",
                run_pf.to_str().unwrap(),
                "--dry-run",
            ])
            .env("HOME", &run_home)
            .env("USERPROFILE", &run_home)
            .env("CONTEXTSTREAM_WATCH_DISABLED", "1")
            .env("NO_COLOR", "1")
            .env_remove("CONTEXTSTREAM_API_URL")
            .env_remove("CONTEXTSTREAM_API_KEY")
            .current_dir(&run_proj)
            .output()
            .unwrap()
    })
    .await
    .unwrap();

    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "setup --profile-file --dry-run failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        stdout.contains("Preview finished · nothing was changed"),
        "expected preview banner:\n{stdout}"
    );
    for forbidden in [
        "still need attention",
        "some checks failed",
        "Credentials installed for this machine",
        "✓ Claude auto memory off",
        "managed installation identity is missing",
        "credentials are missing or unreadable",
    ] {
        assert!(
            !stdout.contains(forbidden),
            "dry-run output must not contain {forbidden:?}:\n{stdout}"
        );
    }

    let settings_path_str = home.join(".claude/settings.json").display().to_string();
    let settings_occurrences = stdout.matches(&settings_path_str).count();
    assert_eq!(
        settings_occurrences, 1,
        "expected exactly one planned operation for {settings_path_str}, got {settings_occurrences}:\n{stdout}"
    );

    assert!(
        !home.join(".contextstream").exists(),
        "dry-run must not create ~/.contextstream"
    );
    assert!(
        !home.join(".claude/settings.json").exists(),
        "dry-run must not write ~/.claude/settings.json"
    );
    assert!(
        hits.lock().unwrap().completion_bodies.is_empty(),
        "dry-run must not POST device completion"
    );

    stop_spawned_watch_helper_if_any(&home);
    server.abort();
}
