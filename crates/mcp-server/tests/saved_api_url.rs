//! Regression test for #271: saved `api_url` in `~/.contextstream/credentials.json`
//! must be honored when `CONTEXTSTREAM_API_URL` is unset across `update-configs`,
//! `doctor --repair`, `update-rules`, `verify-key`, `setup`, and `configure`.
#![cfg(unix)]

use axum::{
    extract::{Path as AxumPath, Query},
    http::{HeaderMap, StatusCode},
    routing::get,
    Json, Router,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{Arc, Mutex},
};

const WORKSPACE_ID: &str = "11111111-1111-4111-8111-111111111111";
const USER_ID: &str = "22222222-2222-4222-8222-222222222222";
const PROJECT_ID: &str = "33333333-3333-4333-8333-333333333333";
const SAVED_KEY: &str = "cs_mock_saved_api_url_key_1234";

async fn spawn_mock_api_server() -> (String, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_url = format!("http://{}", listener.local_addr().unwrap());

    let routes: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let projects: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));

    let check_auth = |headers: &HeaderMap| -> bool {
        headers
            .get("x-api-key")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|key| key == SAVED_KEY)
    };

    let routes_me = Arc::clone(&routes);
    let routes_ws = Arc::clone(&routes);
    let routes_proj_get = Arc::clone(&routes);
    let routes_proj_post = Arc::clone(&routes);
    let routes_proj_id = Arc::clone(&routes);

    let list_state = Arc::clone(&projects);
    let create_state = Arc::clone(&projects);
    let get_state = Arc::clone(&projects);

    let app = Router::new()
        .route(
            "/api/v1/auth/me",
            get(move |headers: HeaderMap| {
                let routes = Arc::clone(&routes_me);
                async move {
                    routes
                        .lock()
                        .unwrap()
                        .push("GET /api/v1/auth/me".to_string());
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
                let routes = Arc::clone(&routes_ws);
                async move {
                    routes
                        .lock()
                        .unwrap()
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
                    let routes = Arc::clone(&routes_proj_get);
                    let list_state = Arc::clone(&list_state);
                    async move {
                        routes
                            .lock()
                            .unwrap()
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
                let routes = Arc::clone(&routes_proj_post);
                let create_state = Arc::clone(&create_state);
                async move {
                    routes
                        .lock()
                        .unwrap()
                        .push("POST /api/v1/projects".to_string());
                    if !check_auth(&headers) {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({"error": "invalid api key"})),
                        );
                    }
                    let created = json!({
                        "id": PROJECT_ID,
                        "name": body.get("name").and_then(Value::as_str).unwrap_or("demo"),
                        "workspace_id": body.get("workspace_id").and_then(Value::as_str).unwrap_or(WORKSPACE_ID),
                        "repository_url": body.get("repository_url").cloned().unwrap_or(Value::Null),
                        "created_at": "2026-01-01T00:00:00Z",
                        "updated_at": "2026-01-01T00:00:00Z"
                    });
                    create_state.lock().unwrap().push(created.clone());
                    (StatusCode::OK, Json(json!({"data": created})))
                }
            }),
        )
        .route(
            "/api/v1/projects/{id}",
            get(move |headers: HeaderMap, AxumPath(id): AxumPath<String>| {
                let routes = Arc::clone(&routes_proj_id);
                let get_state = Arc::clone(&get_state);
                async move {
                    routes
                        .lock()
                        .unwrap()
                        .push(format!("GET /api/v1/projects/{id}"));
                    if !check_auth(&headers) {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({"error": "invalid api key"})),
                        );
                    }
                    let found = get_state
                        .lock()
                        .unwrap()
                        .iter()
                        .find(|p| p.get("id").and_then(Value::as_str) == Some(id.as_str()))
                        .cloned();
                    match found {
                        Some(project) => (StatusCode::OK, Json(json!({"data": project}))),
                        None => (
                            StatusCode::NOT_FOUND,
                            Json(json!({"error": "project not found"})),
                        ),
                    }
                }
            }),
        );

    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    (api_url, routes, handle)
}

fn install_release_like_binary(home: &Path) -> PathBuf {
    let bin_dir = home.join(".contextstream").join("bin");
    std::fs::create_dir_all(&bin_dir).expect("create managed bin dir");
    let dest = bin_dir.join("contextstream-mcp");
    let src = PathBuf::from(env!("CARGO_BIN_EXE_contextstream-mcp"));
    if std::fs::hard_link(&src, &dest).is_err() {
        std::fs::copy(&src, &dest).expect("copy binary");
    }
    dest
}

fn run_cli(bin: &Path, home: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(bin)
        .args(args)
        .current_dir(cwd)
        .env("HOME", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env_remove("CONTEXTSTREAM_API_URL")
        .env_remove("CONTEXTSTREAM_API_KEY")
        .env_remove("CONTEXTSTREAM_JWT")
        .env_remove("CONTEXTSTREAM_WORKSPACE_ID")
        .env_remove("CONTEXTSTREAM_PROJECT_ID")
        .stdin(Stdio::null())
        .output()
        .expect("run contextstream-mcp")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_configs_and_doctor_repair_honor_saved_credentials_api_url() {
    let (api_url, routes, server_handle) = spawn_mock_api_server().await;
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    let repo = temp.path().join("demo");
    std::fs::create_dir_all(home.join(".claude").join("projects")).expect("claude evidence");
    std::fs::create_dir_all(home.join(".contextstream")).expect("cs dir");
    std::fs::create_dir_all(&repo).expect("repo dir");

    assert!(Command::new("git")
        .args(["init", "--quiet"])
        .arg(&repo)
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .args(["-C"])
        .arg(&repo)
        .args([
            "remote",
            "add",
            "origin",
            "https://github.com/example/demo-saved-url.git",
        ])
        .status()
        .unwrap()
        .success());

    let creds_path = home.join(".contextstream").join("credentials.json");
    std::fs::write(
        &creds_path,
        serde_json::to_string_pretty(&json!({
            "api_key": SAVED_KEY,
            "api_url": api_url,
        }))
        .unwrap(),
    )
    .expect("write credentials.json");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&creds_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    let bin = install_release_like_binary(&home);

    // 1. Baseline setup, verify-key, and configure --account-learning status with CONTEXTSTREAM_API_URL unset.
    for cmd in [
        &["setup", "--yes", "--editors=claude"][..],
        &["verify-key"][..],
        &["configure", "--account-learning", "status"][..],
    ] {
        let out = run_cli(&bin, &home, &repo, cmd);
        assert!(
            out.status.success(),
            "{cmd:?} failed:\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    routes.lock().unwrap().clear();

    // 2. `update-configs --scope=all --only-configured` must hit the saved `api_url` and succeed.
    let update_out = run_cli(
        &bin,
        &home,
        &repo,
        &["update-configs", "--scope=all", "--only-configured"],
    );
    assert!(
        update_out.status.success(),
        "update-configs failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&update_out.stdout),
        String::from_utf8_lossy(&update_out.stderr)
    );
    assert!(
        !routes.lock().unwrap().is_empty(),
        "expected update-configs to contact the saved api_url server"
    );

    // 3. Delete `.mcp.json` and run `doctor --repair` — config repair must also honor saved `api_url`.
    let project_mcp = repo.join(".mcp.json");
    std::fs::remove_file(&project_mcp).expect("remove .mcp.json");
    routes.lock().unwrap().clear();

    let repair_out = run_cli(&bin, &home, &repo, &["doctor", "--repair"]);
    assert!(
        repair_out.status.success(),
        "doctor --repair failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&repair_out.stdout),
        String::from_utf8_lossy(&repair_out.stderr)
    );
    assert!(
        project_mcp.exists(),
        "expected doctor --repair to recreate .mcp.json"
    );

    // 4. Remove `.contextstream/config.json` and verify `update-rules` API workspace lookup also uses saved `api_url`.
    let _ = std::fs::remove_file(repo.join(".contextstream").join("config.json"));
    routes.lock().unwrap().clear();
    let rules_out = run_cli(
        &bin,
        &home,
        &repo,
        &["update-rules", "--scope=project", "--only-configured"],
    );
    assert!(
        rules_out.status.success(),
        "update-rules failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&rules_out.stdout),
        String::from_utf8_lossy(&rules_out.stderr)
    );
    assert!(
        routes
            .lock()
            .unwrap()
            .iter()
            .any(|r| r == "GET /api/v1/workspaces"),
        "expected update-rules workspace lookup to hit saved api_url"
    );

    // Terminate any hosted sync bridge spawned under the temp HOME before cleanup.
    let _ = run_cli(&bin, &home, &repo, &["uninstall", "--scope=all"]);
    server_handle.abort();
}
