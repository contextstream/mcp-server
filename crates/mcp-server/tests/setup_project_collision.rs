//! Regression test for issue #251:
//! Unattended `setup --yes` surfaces actionable `--project-id` / `--project-name`
//! recovery options instead of failing with HTTP 400 when a workspace already has
//! a project with the local folder's basename, and supports linking (`--project-id`)
//! or creating under an explicit name (`--project-name`).

use axum::{
    extract::{Path as AxumPath, Query},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::Path,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

const WORKSPACE_ID: &str = "11111111-1111-4111-8111-111111111111";
const OTHER_WORKSPACE_ID: &str = "55555555-5555-4555-8555-555555555555";
const EXISTING_PROJECT_ID: &str = "33333333-3333-4333-8333-333333333333";
const CREATED_PROJECT_ID: &str = "44444444-4444-4444-8444-444444444444";

struct MockServerHandle {
    api_url: String,
    create_project_hits: Arc<AtomicUsize>,
    join: tokio::task::JoinHandle<()>,
}

async fn spawn_mock_collision_server(expected_api_key: &'static str) -> MockServerHandle {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_url = format!("http://{}", listener.local_addr().unwrap());

    let projects: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(vec![json!({
        "id": EXISTING_PROJECT_ID,
        "name": "qa-shared-name",
        "workspace_id": WORKSPACE_ID
    })]));
    let create_project_hits = Arc::new(AtomicUsize::new(0));

    let check_auth = move |headers: &HeaderMap| -> bool {
        headers
            .get("x-api-key")
            .and_then(|value| value.to_str().ok())
            == Some(expected_api_key)
    };

    let list_projects_state = Arc::clone(&projects);
    let create_projects_state = Arc::clone(&projects);
    let get_project_state = Arc::clone(&projects);
    let create_hits_state = Arc::clone(&create_project_hits);

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
                            "email": "qa-collision@example.test",
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
                                    "id": WORKSPACE_ID,
                                    "name": "Testing",
                                    "created_at": "2026-01-01T00:00:00Z",
                                    "updated_at": "2026-01-01T00:00:00Z"
                                },
                                {
                                    "id": OTHER_WORKSPACE_ID,
                                    "name": "Other",
                                    "created_at": "2026-01-01T00:00:00Z",
                                    "updated_at": "2026-01-01T00:00:00Z"
                                }
                            ]
                        }
                    })),
                )
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
                let name = if id == WORKSPACE_ID {
                    "Testing"
                } else {
                    "Other"
                };
                (
                    StatusCode::OK,
                    Json(json!({
                        "data": {
                            "id": id,
                            "name": name,
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
                    let list_projects_state = Arc::clone(&list_projects_state);
                    async move {
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
                        let stored = list_projects_state.lock().unwrap().clone();
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
                let create_projects_state = Arc::clone(&create_projects_state);
                let create_hits_state = Arc::clone(&create_hits_state);
                async move {
                    if !check_auth(&headers) {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({"error": "invalid api key"})),
                        );
                    }
                    create_hits_state.fetch_add(1, Ordering::SeqCst);
                    let name = body
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("demo")
                        .to_string();
                    let mut stored = create_projects_state.lock().unwrap();
                    if stored.iter().any(|proj| {
                        proj.get("name")
                            .and_then(Value::as_str)
                            .is_some_and(|existing| existing.eq_ignore_ascii_case(&name))
                    }) {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(json!({
                                "error": "Bad request: That project name is already in use in this workspace. Choose another name."
                            })),
                        );
                    }
                    let created = json!({
                        "id": CREATED_PROJECT_ID,
                        "name": name,
                        "workspace_id": WORKSPACE_ID
                    });
                    stored.push(created.clone());
                    (StatusCode::OK, Json(json!({ "data": created })))
                }
            }),
        )
        .route(
            "/api/v1/projects/{id}",
            get(move |headers: HeaderMap, AxumPath(id): AxumPath<String>| {
                let get_project_state = Arc::clone(&get_project_state);
                async move {
                    if !check_auth(&headers) {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({"error": "invalid api key"})),
                        );
                    }
                    let stored = get_project_state.lock().unwrap().clone();
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

    MockServerHandle {
        api_url,
        create_project_hits,
        join,
    }
}

fn init_git_repo(path: &Path) {
    for args in [
        &["init", "-q"][..],
        &["config", "user.email", "qa@example.test"][..],
        &["config", "user.name", "QA"][..],
    ] {
        let status = Command::new("git")
            .args(args)
            .current_dir(path)
            .status()
            .expect("git available");
        assert!(status.success(), "git {args:?} failed");
    }
    std::fs::write(path.join(".gitkeep"), "").unwrap();
    for args in [&["add", ".gitkeep"][..], &["commit", "-qm", "init"][..]] {
        let status = Command::new("git")
            .args(args)
            .current_dir(path)
            .status()
            .expect("git available");
        assert!(status.success(), "git {args:?} failed");
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

async fn run_setup(
    home: &Path,
    cwd: &Path,
    api_url: &str,
    api_key: &str,
    extra_args: &[&str],
) -> std::process::Output {
    let home = home.to_path_buf();
    let cwd = cwd.to_path_buf();
    let api_url = api_url.to_string();
    let api_key = api_key.to_string();
    let extra: Vec<String> = extra_args.iter().map(|s| (*s).to_string()).collect();
    tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args(["setup", "--yes", "--editors", "claude"])
            .args(&extra)
            .current_dir(&cwd)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("CONTEXTSTREAM_API_URL", &api_url)
            .env("CONTEXTSTREAM_API_KEY", &api_key)
            .env("CONTEXTSTREAM_NO_BROWSER", "1")
            .env("NO_COLOR", "1")
            .env_remove("CONTEXTSTREAM_WORKSPACE_ID")
            .stdin(Stdio::null())
            .output()
            .expect("run contextstream-mcp setup")
    })
    .await
    .expect("join setup subprocess")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_yes_guides_and_resolves_project_basename_collision() {
    let api_key = "cs_test_setup_project_collision_251";
    let server = spawn_mock_collision_server(api_key).await;

    let temp = tempfile::tempdir().unwrap();

    // 1. Machine 2 has a local checkout named `qa-shared-name` with no git remote.
    // Plain `setup --yes` must fail cleanly before POST /api/v1/projects (no HTTP 400)
    // and suggest both `--project-id <EXISTING_PROJECT_ID>` and `--project-name`.
    let m2_home = temp.path().join("home-m2");
    let m2_project = temp.path().join("m2").join("qa-shared-name");
    std::fs::create_dir_all(&m2_home).unwrap();
    std::fs::create_dir_all(&m2_project).unwrap();
    init_git_repo(&m2_project);

    let collision_out = run_setup(
        &m2_home,
        &m2_project,
        &server.api_url,
        api_key,
        &["--workspace-id", WORKSPACE_ID],
    )
    .await;
    assert!(
        !collision_out.status.success(),
        "colliding setup --yes without project override should fail cleanly"
    );
    let stderr = String::from_utf8_lossy(&collision_out.stderr);
    assert!(
        !stderr.contains("400"),
        "collision must fail before hitting server HTTP 400, got:\n{stderr}"
    );
    assert_eq!(
        server.create_project_hits.load(Ordering::SeqCst),
        0,
        "POST /api/v1/projects must not be called when workspace already has a same-name project"
    );
    assert!(
        stderr.contains(&format!("--project-id {EXISTING_PROJECT_ID}")),
        "error should name the existing project's --project-id recovery flag, got:\n{stderr}"
    );
    assert!(
        stderr.contains("--project-name"),
        "error should name the --project-name recovery flag, got:\n{stderr}"
    );
    assert!(
        !m2_project.join(".contextstream/config.json").exists(),
        "aborted setup must not write a partial .contextstream/config.json"
    );

    // 2. Re-running with `--project-id <EXISTING_PROJECT_ID>` links Machine 2 to the
    // existing project (and even infers the workspace on a multi-workspace account).
    let link_out = run_setup(
        &m2_home,
        &m2_project,
        &server.api_url,
        api_key,
        &["--project-id", EXISTING_PROJECT_ID],
    )
    .await;
    assert!(
        link_out.status.success(),
        "setup --yes --project-id failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&link_out.stdout),
        String::from_utf8_lossy(&link_out.stderr)
    );
    let m2_cfg: Value = serde_json::from_str(
        &std::fs::read_to_string(m2_project.join(".contextstream/config.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        m2_cfg.get("project_id").and_then(Value::as_str),
        Some(EXISTING_PROJECT_ID)
    );
    assert_eq!(
        m2_cfg.get("project_name").and_then(Value::as_str),
        Some("qa-shared-name")
    );
    assert_eq!(server.create_project_hits.load(Ordering::SeqCst), 0);

    // 3. Machine 3 has an unrelated local repo that also happens to be named `qa-shared-name`.
    // Passing `--project-name qa-shared-name-m3` creates a separate project cleanly, and
    // re-running the same command idempotently reuses that binding without creating a third.
    let m3_home = temp.path().join("home-m3");
    let m3_project = temp.path().join("m3").join("qa-shared-name");
    std::fs::create_dir_all(&m3_home).unwrap();
    std::fs::create_dir_all(&m3_project).unwrap();
    init_git_repo(&m3_project);

    let create_out = run_setup(
        &m3_home,
        &m3_project,
        &server.api_url,
        api_key,
        &[
            "--workspace-id",
            WORKSPACE_ID,
            "--project-name",
            "qa-shared-name-m3",
        ],
    )
    .await;
    assert!(
        create_out.status.success(),
        "setup --yes --project-name failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&create_out.stdout),
        String::from_utf8_lossy(&create_out.stderr)
    );
    assert_eq!(server.create_project_hits.load(Ordering::SeqCst), 1);
    let m3_cfg: Value = serde_json::from_str(
        &std::fs::read_to_string(m3_project.join(".contextstream/config.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        m3_cfg.get("project_id").and_then(Value::as_str),
        Some(CREATED_PROJECT_ID)
    );
    assert_eq!(
        m3_cfg.get("project_name").and_then(Value::as_str),
        Some("qa-shared-name-m3")
    );

    let rerun_out = run_setup(
        &m3_home,
        &m3_project,
        &server.api_url,
        api_key,
        &[
            "--workspace-id",
            WORKSPACE_ID,
            "--project-name",
            "qa-shared-name-m3",
        ],
    )
    .await;
    assert!(
        rerun_out.status.success(),
        "idempotent re-run of setup --yes --project-name failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&rerun_out.stdout),
        String::from_utf8_lossy(&rerun_out.stderr)
    );
    assert_eq!(
        server.create_project_hits.load(Ordering::SeqCst),
        1,
        "idempotent re-run must reuse linked project rather than calling POST /api/v1/projects again"
    );

    stop_spawned_watch_helper_if_any(&m2_home);
    stop_spawned_watch_helper_if_any(&m3_home);
    server.join.abort();
}
