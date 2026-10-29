//! Regression test for #276:
//! Running `contextstream-mcp setup --yes` inside a Git worktree of a local
//! repository that has no `origin` remote URL yet must reuse the sibling
//! checkout's canonical project (via the shared `git-common-dir-v1:`
//! repository fingerprint in `~/.contextstream/mappings.json`) instead of
//! minting a duplicate worktree-named project that causes `doctor` to fail
//! `project_link`.
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
    process::{Command, Output, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

const WORKSPACE_A_ID: &str = "11111111-1111-4111-8111-111111111111";
const WORKSPACE_B_ID: &str = "66666666-6666-4666-8666-666666666666";
const USER_ID: &str = "22222222-2222-4222-8222-222222222222";
const API_KEY: &str = "cs_mock_worktree_key_7777";

async fn spawn_mock_api_server() -> (String, Arc<Mutex<Vec<Value>>>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_url = format!("http://{}", listener.local_addr().unwrap());

    let projects: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let project_seq = Arc::new(AtomicUsize::new(1));

    let check_auth = |headers: &HeaderMap| -> bool {
        headers
            .get("x-api-key")
            .and_then(|value| value.to_str().ok())
            == Some(API_KEY)
    };

    let list_state = Arc::clone(&projects);
    let create_state = Arc::clone(&projects);
    let get_state = Arc::clone(&projects);
    let seq_state = Arc::clone(&project_seq);

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
                            "id": USER_ID,
                            "email": "wt@example.test",
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
                                    "id": WORKSPACE_A_ID,
                                    "name": "Testing",
                                    "created_at": "2026-01-01T00:00:00Z",
                                    "updated_at": "2026-01-01T00:00:00Z"
                                },
                                {
                                    "id": WORKSPACE_B_ID,
                                    "name": "Personal",
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
                let name = if id == WORKSPACE_A_ID {
                    "Testing"
                } else {
                    "Personal"
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
                        let ws_filter = params.get("workspace_id").cloned();
                        let stored: Vec<Value> = list_state
                            .lock()
                            .unwrap()
                            .iter()
                            .filter(|proj| match ws_filter.as_deref() {
                                Some(expected) => {
                                    proj.get("workspace_id").and_then(Value::as_str)
                                        == Some(expected)
                                }
                                None => true,
                            })
                            .cloned()
                            .collect();
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
                let seq_state = Arc::clone(&seq_state);
                async move {
                    if !check_auth(&headers) {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({"error": "invalid api key"})),
                        );
                    }
                    let idx = seq_state.fetch_add(1, Ordering::SeqCst);
                    let id = format!("33333333-3333-4333-8333-{idx:012}");
                    let name = body
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("myapp")
                        .to_string();
                    let ws_id = body
                        .get("workspace_id")
                        .and_then(Value::as_str)
                        .unwrap_or(WORKSPACE_A_ID)
                        .to_string();
                    let repo_url = body.get("repository_url").cloned().unwrap_or(Value::Null);
                    let created = json!({
                        "id": id,
                        "name": name,
                        "workspace_id": ws_id,
                        "repository_url": repo_url,
                        "updated_at": "2026-01-01T00:00:00Z"
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
            "/api/v1/telemetry/harness-readiness",
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
                            "highest_ready_stage": null,
                            "stages": []
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
                    let installation_id =
                        params.get("installation_id").cloned().unwrap_or_default();
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
    (api_url, projects, handle)
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

fn init_local_git_repo_no_remote(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    for args in [
        &["init", "-q"][..],
        &["config", "user.email", "dev@example.test"][..],
        &["config", "user.name", "Dev"][..],
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

fn run_cli(
    bin: &Path,
    home: &Path,
    cwd: &Path,
    api_url: &str,
    args: &[&str],
    stdin_payload: Option<&str>,
) -> Output {
    let mut cmd = Command::new(bin);
    cmd.args(args)
        .current_dir(cwd)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("CONTEXTSTREAM_API_URL", api_url)
        .env("CONTEXTSTREAM_NO_BROWSER", "1")
        .env("CONTEXTSTREAM_WATCH_DISABLED", "1")
        .env("CONTEXTSTREAM_WATCH", "0")
        .env("NO_COLOR", "1")
        .env_remove("CONTEXTSTREAM_API_KEY")
        .env_remove("CONTEXTSTREAM_WORKSPACE_ID")
        .env_remove("CONTEXTSTREAM_PROJECT_ID")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(payload) = stdin_payload {
        cmd.stdin(Stdio::piped());
        let mut child = cmd.spawn().expect("spawn cli");
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(payload.as_bytes())
            .expect("write stdin");
        child.wait_with_output().expect("wait cli")
    } else {
        cmd.stdin(Stdio::null());
        cmd.output().expect("run cli")
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_yes_in_git_worktree_without_remote_reuses_sibling_project_and_doctor_passes() {
    let (api_url, projects, server_handle) = spawn_mock_api_server().await;

    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    std::fs::create_dir_all(home.join(".claude")).expect("seed claude dir");
    let bin = install_release_like_binary(&home);

    // Save credentials via `configure --api-key-stdin`.
    let cfg = run_cli(
        &bin,
        &home,
        &home,
        &api_url,
        &["configure", "--api-key-stdin"],
        Some(&format!("{API_KEY}\n")),
    );
    assert!(
        cfg.status.success(),
        "configure --api-key-stdin failed: stderr={}",
        String::from_utf8_lossy(&cfg.stderr)
    );

    // 1. Initialize local Git repo `myapp` (no origin remote) and run `setup --yes`.
    let main_repo = temp.path().join("myapp");
    init_local_git_repo_no_remote(&main_repo);

    let ws_arg = format!("--workspace-id={WORKSPACE_A_ID}");
    let setup_main = run_cli(
        &bin,
        &home,
        &main_repo,
        &api_url,
        &["setup", "--yes", "--editors=claude", &ws_arg],
        None,
    );
    assert!(
        setup_main.status.success(),
        "setup --yes in main repo failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&setup_main.stdout),
        String::from_utf8_lossy(&setup_main.stderr)
    );
    assert_eq!(projects.lock().unwrap().len(), 1);
    let main_cfg: Value = serde_json::from_str(
        &std::fs::read_to_string(main_repo.join(".contextstream/config.json")).unwrap(),
    )
    .unwrap();
    let main_project_id = main_cfg["project_id"].as_str().unwrap().to_string();
    assert_eq!(main_cfg["project_name"].as_str(), Some("myapp"));

    // 2. Create Git worktree `myapp-worktree` and run `setup --yes` inside it.
    let worktree = temp.path().join("myapp-worktree");
    assert!(Command::new("git")
        .args([
            "worktree",
            "add",
            "-q",
            worktree.to_string_lossy().as_ref(),
            "-b",
            "feat/login",
        ])
        .current_dir(&main_repo)
        .status()
        .unwrap()
        .success());

    let setup_wt = run_cli(
        &bin,
        &home,
        &worktree,
        &api_url,
        &["setup", "--yes", "--editors=claude", &ws_arg],
        None,
    );
    assert!(
        setup_wt.status.success(),
        "setup --yes in worktree failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&setup_wt.stdout),
        String::from_utf8_lossy(&setup_wt.stderr)
    );

    // Must reuse `myapp` rather than creating a duplicate `myapp-worktree` project.
    assert_eq!(
        projects.lock().unwrap().len(),
        1,
        "setup --yes in worktree must not create a duplicate project: {:?}",
        *projects.lock().unwrap()
    );
    let wt_cfg: Value = serde_json::from_str(
        &std::fs::read_to_string(worktree.join(".contextstream/config.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        wt_cfg["project_id"].as_str(),
        Some(main_project_id.as_str())
    );
    assert_eq!(wt_cfg["project_name"].as_str(), Some("myapp"));

    // 3. `contextstream-mcp doctor` immediately inside `myapp-worktree` must exit 0 (`[HEALTHY]`).
    let doctor_wt = run_cli(&bin, &home, &worktree, &api_url, &["doctor"], None);
    let doctor_stdout = String::from_utf8_lossy(&doctor_wt.stdout);
    let doctor_stderr = String::from_utf8_lossy(&doctor_wt.stderr);
    assert!(
        doctor_wt.status.success(),
        "doctor in worktree failed:\nstdout={doctor_stdout}\nstderr={doctor_stderr}"
    );
    assert!(
        doctor_stdout.contains("[HEALTHY]"),
        "expected [HEALTHY] in worktree doctor:\n{doctor_stdout}"
    );

    // 4. A second worktree WITHOUT `--workspace-id` also resolves both the workspace (`Testing`)
    //    and canonical project (`myapp`) from the sibling checkout's `git-common-dir-v1:` link,
    //    even though the account has 2 workspaces (`Testing` and `Personal`).
    let worktree2 = temp.path().join("myapp-worktree-2");
    assert!(Command::new("git")
        .args([
            "worktree",
            "add",
            "-q",
            worktree2.to_string_lossy().as_ref(),
            "-b",
            "feat/billing",
        ])
        .current_dir(&main_repo)
        .status()
        .unwrap()
        .success());

    let setup_wt2 = run_cli(
        &bin,
        &home,
        &worktree2,
        &api_url,
        &["setup", "--yes", "--editors=claude"],
        None,
    );
    assert!(
        setup_wt2.status.success(),
        "setup --yes without --workspace-id in worktree2 failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&setup_wt2.stdout),
        String::from_utf8_lossy(&setup_wt2.stderr)
    );
    assert_eq!(projects.lock().unwrap().len(), 1);
    let wt2_cfg: Value = serde_json::from_str(
        &std::fs::read_to_string(worktree2.join(".contextstream/config.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(wt2_cfg["workspace_id"].as_str(), Some(WORKSPACE_A_ID));
    assert_eq!(
        wt2_cfg["project_id"].as_str(),
        Some(main_project_id.as_str())
    );

    server_handle.abort();
}
