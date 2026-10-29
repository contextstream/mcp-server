//! Verify `setup --yes --dry-run` in a brand-new repository previews all
//! project-scoped files (`.mcp.json`, rules, `.contextstream/config.json`, and
//! managed Git hooks) without creating a server-side project or mutating disk
//! (including `.git/info/exclude`).
#![cfg(unix)]

use axum::{
    extract::{Path as AxumPath, Query},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
    Json, Router,
};
use serde_json::json;
use std::{
    collections::HashMap,
    path::Path,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

const WORKSPACE_ID: &str = "11111111-1111-4111-8111-111111111111";

fn init_git_repo(path: &Path) {
    for args in [
        &["init", "-q"][..],
        &["config", "user.email", "dev@example.test"][..],
        &["config", "user.name", "Dev"][..],
    ] {
        let out = Command::new("git")
            .args(args)
            .current_dir(path)
            .output()
            .expect("git init/config");
        assert!(out.status.success(), "git {args:?} failed");
    }
}

async fn spawn_mock_setup_server(
    expected_api_key: &'static str,
) -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_url = format!("http://{}", listener.local_addr().unwrap());
    let create_project_calls = Arc::new(AtomicUsize::new(0));
    let create_calls_route = Arc::clone(&create_project_calls);

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
                            "email": "dryrun-preview@example.test",
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
                (StatusCode::OK, Json(json!({"data": {"items": []}})))
            })
            .post(move |_headers: HeaderMap| {
                let create_calls_route = Arc::clone(&create_calls_route);
                async move {
                    create_calls_route.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(json!({"error": "dry-run must never create a server project"})),
                    )
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

    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (api_url, create_project_calls, handle)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_yes_dry_run_in_new_repo_previews_project_files_and_hooks_without_writing() {
    const SECRET: &str = "dry-run-new-repo-preview-secret";
    let (api_url, create_project_calls, server) = spawn_mock_setup_server(SECRET).await;
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let repo = temp.path().join("fresh-repo");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    init_git_repo(&repo);
    std::fs::write(repo.join("src/lib.rs"), "pub fn answer() -> u32 { 42 }\n").unwrap();

    let run_home = home.clone();
    let run_repo = repo.clone();
    let run_api_url = api_url.clone();
    let output = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args([
                "setup",
                "--yes",
                "--dry-run",
                "--editors=claude,codex",
                &format!("--workspace-id={WORKSPACE_ID}"),
                "--project-name=custom-preview-name",
            ])
            .env_clear()
            .env("HOME", &run_home)
            .env("USERPROFILE", &run_home)
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("CONTEXTSTREAM_API_KEY", SECRET)
            .env("CONTEXTSTREAM_API_URL", &run_api_url)
            .env("CONTEXTSTREAM_NO_BROWSER", "1")
            .env("CONTEXTSTREAM_WATCH_DISABLED", "1")
            .env("NO_COLOR", "1")
            .current_dir(&run_repo)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    })
    .await
    .unwrap();

    server.abort();

    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        output.status.success(),
        "setup --yes --dry-run failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(
        create_project_calls.load(Ordering::SeqCst),
        0,
        "dry-run must never POST /api/v1/projects"
    );
    assert!(!stdout.contains(SECRET), "{stdout}");
    assert!(!stderr.contains(SECRET), "{stderr}");

    // Every project-scoped artifact that a real first-time setup writes must
    // appear in the dry-run preview.
    for expected_path in [
        repo.join(".mcp.json"),
        repo.join("CLAUDE.md"),
        repo.join("AGENTS.md"),
        repo.join(".contextstream/config.json"),
        repo.join(".git/hooks/post-commit"),
        repo.join(".git/hooks/post-checkout"),
        repo.join(".git/hooks/post-merge"),
        repo.join(".git/hooks/pre-push"),
    ] {
        let display = expected_path.display().to_string();
        assert!(
            stdout.contains(&display),
            "expected dry-run preview to include {display}:\n{stdout}"
        );
        assert!(
            !expected_path.exists(),
            "dry-run must not write {display} to disk"
        );
    }

    // Shared `.contextstream/config.json` must be deduplicated across multiple editors.
    let ws_cfg_display = repo
        .join(".contextstream/config.json")
        .display()
        .to_string();
    let ws_cfg_creates = stdout
        .lines()
        .filter(|line| line.trim_start().starts_with("+ create") && line.contains(&ws_cfg_display))
        .count();
    assert_eq!(
        ws_cfg_creates, 1,
        "expected exactly one + create line for {ws_cfg_display}, got {ws_cfg_creates}:\n{stdout}"
    );

    for expected_phrase in [
        "Would create project custom-preview-name",
        "Would index in the background",
        "Preview finished · nothing was changed",
        "would create, link & index",
    ] {
        assert!(
            stdout.contains(expected_phrase),
            "expected {expected_phrase:?} in dry-run output:\n{stdout}"
        );
    }
    for forbidden in [
        "Not linked",
        "✓ Project ready",
        "· indexing",
        "previewing editor files without a new project binding",
    ] {
        assert!(
            !stdout.contains(forbidden),
            "dry-run output must not contain {forbidden:?}:\n{stdout}"
        );
    }
    assert!(
        !home.join(".contextstream").exists(),
        "dry-run must not create ~/.contextstream"
    );
    assert!(
        !repo.join(".contextstream").exists(),
        "dry-run must not create <repo>/.contextstream"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn setup_yes_dry_run_with_existing_mcp_json_does_not_mutate_git_info_exclude() {
    const SECRET: &str = "dry-run-existing-mcp-secret";
    let (api_url, create_project_calls, server) = spawn_mock_setup_server(SECRET).await;
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let repo = temp.path().join("existing-mcp-repo");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    init_git_repo(&repo);

    let pre_existing_mcp =
        "{\n  \"mcpServers\": {\n    \"other\": {\n      \"command\": \"echo\"\n    }\n  }\n}\n";
    std::fs::write(repo.join(".mcp.json"), pre_existing_mcp).unwrap();
    let exclude_path = repo.join(".git/info/exclude");
    let exclude_before = std::fs::read_to_string(&exclude_path).unwrap_or_default();

    let run_home = home.clone();
    let run_repo = repo.clone();
    let output = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args([
                "setup",
                "--yes",
                "--dry-run",
                "--editors=claude",
                &format!("--workspace-id={WORKSPACE_ID}"),
            ])
            .env_clear()
            .env("HOME", &run_home)
            .env("USERPROFILE", &run_home)
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("CONTEXTSTREAM_API_KEY", SECRET)
            .env("CONTEXTSTREAM_API_URL", &api_url)
            .env("CONTEXTSTREAM_NO_BROWSER", "1")
            .env("CONTEXTSTREAM_WATCH_DISABLED", "1")
            .env("NO_COLOR", "1")
            .current_dir(&run_repo)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    })
    .await
    .unwrap();

    server.abort();

    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        output.status.success(),
        "setup --yes --dry-run failed:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert_eq!(create_project_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        std::fs::read_to_string(repo.join(".mcp.json")).unwrap(),
        pre_existing_mcp,
        "dry-run must not modify existing .mcp.json"
    );
    let exclude_after = std::fs::read_to_string(&exclude_path).unwrap_or_default();
    assert_eq!(
        exclude_after, exclude_before,
        "dry-run must not modify .git/info/exclude"
    );
}
