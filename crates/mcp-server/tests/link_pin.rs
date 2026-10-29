//! Regression test for issue #268:
//! When a checkout is explicitly pinned to an accessible project with
//! `contextstream-mcp link --project-id <UUID> --pin`, subsequent `link` and
//! `doctor` runs honor the pin as healthy (`rc=0`), and neither `doctor --repair`
//! nor `link --fix` overwrites `.contextstream/config.json` with another
//! project tracking the same Git remote.
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
    path::Path,
    process::{Command, Stdio},
};

const WORKSPACE_ID: &str = "11111111-1111-4111-8111-111111111111";
const PROJECT_C1_ID: &str = "33333333-3333-4333-8333-333333333333";
const PROJECT_PINNED_ID: &str = "44444444-4444-4444-8444-444444444444";

async fn spawn_mock_pin_server(
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

    let projects = vec![
        json!({
            "id": PROJECT_C1_ID,
            "name": "c1",
            "workspace_id": WORKSPACE_ID,
            "repository_url": "https://github.com/example/shared-repo.git",
            "updated_at": chrono::Utc::now().to_rfc3339(),
            "created_at": "2026-01-01T00:00:00Z"
        }),
        json!({
            "id": PROJECT_PINNED_ID,
            "name": "qa-pinned-target",
            "workspace_id": WORKSPACE_ID,
            "updated_at": "2025-01-01T00:00:00Z",
            "created_at": "2025-01-01T00:00:00Z"
        }),
    ];
    let list_projects = projects.clone();
    let get_projects = projects.clone();

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
                            "email": "qa-pin@example.test",
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
                    let list_projects = list_projects.clone();
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
                        let items = if page == 1 { list_projects } else { Vec::new() };
                        (
                            StatusCode::OK,
                            Json(json!({
                                "data": {
                                    "items": items,
                                    "total": 2,
                                    "page": page,
                                    "page_size": 50,
                                    "total_pages": 1
                                }
                            })),
                        )
                    }
                },
            ),
        )
        .route(
            "/api/v1/projects/{id}",
            get(move |headers: HeaderMap, AxumPath(id): AxumPath<String>| {
                let get_projects = get_projects.clone();
                async move {
                    if !check_auth(&headers) {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({"error": "invalid api key"})),
                        );
                    }
                    if let Some(found) = get_projects
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

fn init_shared_remote_repo(path: &Path) {
    for args in [
        &["init", "-q"][..],
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/example/shared-repo.git",
        ][..],
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
    std::fs::write(path.join("README.md"), "# pin\n").unwrap();
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
async fn pinned_project_link_passes_link_and_doctor_and_survives_repairs() {
    const SECRET: &str = "cs_test_link_pin_268";
    let (api_url, server) = spawn_mock_pin_server(SECRET).await;
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let c1 = temp.path().join("c1");
    let c2 = temp.path().join("c2");
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::create_dir_all(&c1).unwrap();
    std::fs::create_dir_all(&c2).unwrap();
    init_shared_remote_repo(&c1);
    init_shared_remote_repo(&c2);

    // Configure credentials.
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

    // Run `setup --yes` in both c1 and c2.
    for checkout in [&c1, &c2] {
        let h = home.clone();
        let dir = checkout.clone();
        let url = api_url.clone();
        let out = tokio::task::spawn_blocking(move || {
            Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
                .args([
                    "setup",
                    "--yes",
                    "--editors=claude",
                    &format!("--workspace-id={WORKSPACE_ID}"),
                ])
                .env("HOME", &h)
                .env("USERPROFILE", &h)
                .env("CONTEXTSTREAM_API_URL", &url)
                .env("CONTEXTSTREAM_WATCH_DISABLED", "1")
                .env("NO_COLOR", "1")
                .current_dir(&dir)
                .output()
                .unwrap()
        })
        .await
        .unwrap();
        assert!(
            out.status.success(),
            "setup failed in {}:\nstdout: {}\nstderr: {}",
            checkout.display(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // Pin c2 to `qa-pinned-target` (`PROJECT_PINNED_ID`).
    let pin_home = home.clone();
    let pin_c2 = c2.clone();
    let pin_url = api_url.clone();
    let pin_out = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args([
                "link",
                &format!("--project-id={PROJECT_PINNED_ID}"),
                "--pin",
            ])
            .env("HOME", &pin_home)
            .env("USERPROFILE", &pin_home)
            .env("CONTEXTSTREAM_API_URL", &pin_url)
            .env("NO_COLOR", "1")
            .current_dir(&pin_c2)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        pin_out.status.success(),
        "link --pin failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&pin_out.stdout),
        String::from_utf8_lossy(&pin_out.stderr)
    );

    // 1. Subsequent `contextstream-mcp link` must exit 0 with `✓` and `pinned`.
    let check_home = home.clone();
    let check_c2 = c2.clone();
    let check_url = api_url.clone();
    let link_check = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args(["link"])
            .env("HOME", &check_home)
            .env("USERPROFILE", &check_home)
            .env("CONTEXTSTREAM_API_URL", &check_url)
            .env("NO_COLOR", "1")
            .current_dir(&check_c2)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let link_stdout = String::from_utf8_lossy(&link_check.stdout);
    assert!(
        link_check.status.success(),
        "subsequent `link` should honor pin (rc=0), got:\nstdout: {link_stdout}\nstderr: {}",
        String::from_utf8_lossy(&link_check.stderr)
    );
    assert!(link_stdout.contains("✓ "), "{link_stdout}");
    assert!(link_stdout.contains("pinned"), "{link_stdout}");
    assert!(
        !link_stdout.contains("Run again with --fix"),
        "pinned link must not prompt for --fix:\n{link_stdout}"
    );

    // 2. `contextstream-mcp doctor --json` must report `project_link.status == "pass"`.
    let doc_home = home.clone();
    let doc_c2 = c2.clone();
    let doc_url = api_url.clone();
    let doc_out = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args(["doctor", "--json"])
            .env("HOME", &doc_home)
            .env("USERPROFILE", &doc_home)
            .env("CONTEXTSTREAM_API_URL", &doc_url)
            .env("NO_COLOR", "1")
            .current_dir(&doc_c2)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let doc_json: Value = serde_json::from_slice(&doc_out.stdout).unwrap();
    assert_eq!(
        doc_json["project_link"]["status"].as_str(),
        Some("pass"),
        "doctor should treat pinned project link as pass: {}",
        doc_json["project_link"]
    );

    // 3. Neither `doctor --repair` nor `link --fix` may overwrite `.contextstream/config.json`.
    for args in [&["doctor", "--repair"][..], &["link", "--fix"][..]] {
        let rep_home = home.clone();
        let rep_c2 = c2.clone();
        let rep_url = api_url.clone();
        let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        let rep_out = tokio::task::spawn_blocking(move || {
            Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
                .args(&owned)
                .env("HOME", &rep_home)
                .env("USERPROFILE", &rep_home)
                .env("CONTEXTSTREAM_API_URL", &rep_url)
                .env("NO_COLOR", "1")
                .current_dir(&rep_c2)
                .output()
                .unwrap()
        })
        .await
        .unwrap();
        assert!(
            rep_out.status.success(),
            "{args:?} failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&rep_out.stdout),
            String::from_utf8_lossy(&rep_out.stderr)
        );
        let cfg: Value = serde_json::from_str(
            &std::fs::read_to_string(c2.join(".contextstream/config.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            cfg["project_id"].as_str(),
            Some(PROJECT_PINNED_ID),
            "{args:?} overwrote pinned project_id: {cfg}"
        );
    }

    stop_spawned_watch_helper_if_any(&home);
    server.abort();
}
