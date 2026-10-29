//! Regression test for issue #241:
//! Re-running plain `contextstream-mcp setup --yes` on an already configured,
//! linked, indexed, and healthy checkout is quiet and mutates zero state on
//! disk, while still running full setup when drift is present.
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
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::SystemTime,
};

const WORKSPACE_ID: &str = "11111111-1111-4111-8111-111111111111";
const PROJECT_ID: &str = "33333333-3333-4333-8333-333333333333";

async fn spawn_mock_rerun_server(
    expected_api_key: &'static str,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_url = format!("http://{}", listener.local_addr().unwrap());

    let projects: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let check_auth = move |headers: &HeaderMap| -> bool {
        headers
            .get("x-api-key")
            .and_then(|value| value.to_str().ok())
            == Some(expected_api_key)
    };

    let list_state = Arc::clone(&projects);
    let create_state = Arc::clone(&projects);
    let get_state = Arc::clone(&projects);

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
                            "email": "qa-rerun@example.test",
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
                    let list_state = Arc::clone(&list_state);
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
                let create_state = Arc::clone(&create_state);
                async move {
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
                let get_state = Arc::clone(&get_state);
                async move {
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
    (api_url, join)
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
    std::fs::write(path.join(".gitkeep"), "").unwrap();
    for args in [&["add", ".gitkeep"][..], &["commit", "-qm", "init"][..]] {
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

fn snapshot_files(paths: &[PathBuf]) -> Vec<(PathBuf, SystemTime, Vec<u8>)> {
    paths
        .iter()
        .map(|path| {
            let meta = std::fs::metadata(path)
                .unwrap_or_else(|e| panic!("expected {} to exist: {e}", path.display()));
            let mtime = meta.modified().unwrap();
            let bytes = std::fs::read(path).unwrap();
            (path.clone(), mtime, bytes)
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_yes_rerun_on_healthy_indexed_project_is_quiet_and_mutates_nothing() {
    const SECRET: &str = "cs_test_setup_idempotent_rerun_241";
    let (api_url, server) = spawn_mock_rerun_server(SECRET).await;
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("qa-proj-c1");
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    init_git_repo(&project);

    // Save credentials via `configure --api-key-stdin`, matching #241 reproduction.
    let cfg_home = home.clone();
    let cfg_url = api_url.clone();
    tokio::task::spawn_blocking(move || {
        let mut child = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args(["configure", "--api-key-stdin"])
            .env("HOME", &cfg_home)
            .env("USERPROFILE", &cfg_home)
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
        assert!(child.wait_with_output().unwrap().status.success());
    })
    .await
    .unwrap();

    // 1. Initial `setup --yes --editors=claude --workspace-id=...`.
    let first_home = home.clone();
    let first_proj = project.clone();
    let first_out = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args([
                "setup",
                "--yes",
                "--editors=claude",
                &format!("--workspace-id={WORKSPACE_ID}"),
            ])
            .env("HOME", &first_home)
            .env("USERPROFILE", &first_home)
            .env("CONTEXTSTREAM_NO_BROWSER", "1")
            .env("CONTEXTSTREAM_WATCH_DISABLED", "1")
            .env("NO_COLOR", "1")
            .env_remove("CONTEXTSTREAM_API_KEY")
            .env_remove("CONTEXTSTREAM_API_URL")
            .current_dir(&first_proj)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        first_out.status.success(),
        "initial setup failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&first_out.stdout),
        String::from_utf8_lossy(&first_out.stderr)
    );

    // Add a tracked content file (`README.md`) and record a completed local index
    // at current HEAD in `~/.contextstream/indexed-projects.json`.
    std::fs::write(project.join("README.md"), "# demo\n").unwrap();
    assert!(Command::new("git")
        .args(["add", "README.md"])
        .current_dir(&project)
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .args(["commit", "-qm", "add readme"])
        .current_dir(&project)
        .status()
        .unwrap()
        .success());
    let head_sha = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&project)
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap()
    .trim()
    .to_string();
    let canonical_project = std::fs::canonicalize(&project).unwrap();
    let indexed_projects_path = home.join(".contextstream/indexed-projects.json");
    std::fs::write(
        &indexed_projects_path,
        serde_json::to_vec_pretty(&json!({
            "version": 1,
            "projects": {
                canonical_project.to_string_lossy().to_string(): {
                    "project_id": PROJECT_ID,
                    "indexed_at": chrono::Utc::now().to_rfc3339(),
                    "indexed_commit": head_sha
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();

    let watched_paths = vec![
        home.join(".contextstream/credentials.json"),
        home.join(".contextstream/installation.json"),
        home.join(".contextstream/setup-transport-mode"),
        home.join(".claude.json"),
        home.join(".claude/CLAUDE.md"),
        home.join(".claude/settings.json"),
        indexed_projects_path.clone(),
        project.join(".contextstream/config.json"),
        project.join(".mcp.json"),
        project.join("CLAUDE.md"),
    ];
    let before = snapshot_files(&watched_paths);
    tokio::time::sleep(std::time::Duration::from_millis(25)).await;

    // 2. Re-run plain `contextstream-mcp setup --yes`: must be quiet and mutate nothing.
    let rerun_home = home.clone();
    let rerun_proj = project.clone();
    let rerun_out = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args(["setup", "--yes"])
            .env("HOME", &rerun_home)
            .env("USERPROFILE", &rerun_home)
            .env("CONTEXTSTREAM_NO_BROWSER", "1")
            .env("CONTEXTSTREAM_WATCH_DISABLED", "1")
            .env("NO_COLOR", "1")
            .env_remove("CONTEXTSTREAM_API_KEY")
            .env_remove("CONTEXTSTREAM_API_URL")
            .current_dir(&rerun_proj)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let rerun_stdout = String::from_utf8_lossy(&rerun_out.stdout);
    let rerun_stderr = String::from_utf8_lossy(&rerun_out.stderr);
    assert!(
        rerun_out.status.success(),
        "rerun setup --yes failed:\nstdout:\n{rerun_stdout}\nstderr:\n{rerun_stderr}"
    );
    assert!(
        rerun_stdout.contains("already set up and healthy"),
        "expected concise healthy confirmation, got:\n{rerun_stdout}"
    );
    for noisy in [
        "CONTEXTSTREAM SETUP",
        "DATA AND PRIVACY",
        "Indexing in the background",
        "Restart Claude Code",
    ] {
        assert!(
            !rerun_stdout.contains(noisy),
            "healthy re-run should omit onboarding noise {noisy:?}, got:\n{rerun_stdout}"
        );
    }

    let after = snapshot_files(&watched_paths);
    for ((path, before_mtime, before_bytes), (_, after_mtime, after_bytes)) in
        before.iter().zip(after.iter())
    {
        assert_eq!(
            before_bytes,
            after_bytes,
            "healthy re-run mutated contents of {}",
            path.display()
        );
        assert_eq!(
            before_mtime,
            after_mtime,
            "healthy re-run touched mtime of {}",
            path.display()
        );
    }

    // 3. Introducing drift (deleting CLAUDE.md) disables the short-circuit and self-heals.
    std::fs::remove_file(project.join("README.md")).unwrap();
    std::fs::remove_file(project.join("CLAUDE.md")).unwrap();
    let heal_home = home.clone();
    let heal_proj = project.clone();
    let heal_out = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args(["setup", "--yes"])
            .env("HOME", &heal_home)
            .env("USERPROFILE", &heal_home)
            .env("CONTEXTSTREAM_NO_BROWSER", "1")
            .env("CONTEXTSTREAM_WATCH_DISABLED", "1")
            .env("NO_COLOR", "1")
            .env_remove("CONTEXTSTREAM_API_KEY")
            .env_remove("CONTEXTSTREAM_API_URL")
            .current_dir(&heal_proj)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    stop_spawned_watch_helper_if_any(&home);
    server.abort();

    assert!(
        heal_out.status.success(),
        "self-healing setup --yes failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&heal_out.stdout),
        String::from_utf8_lossy(&heal_out.stderr)
    );
    assert!(
        project.join("CLAUDE.md").is_file(),
        "drifted project must run full setup and recreate CLAUDE.md"
    );
}
