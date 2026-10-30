//! Regression tests for issues #278 and #279:
//! - #278: `contextstream-mcp link --project-id <unknown-uuid>` must exit non-zero,
//!   render `✗ <path>` (not `✓` or `!`), report that re-linking failed while
//!   leaving any existing link unchanged, and never pin the old project when
//!   `--pin` was also passed.
//! - #279: `contextstream-mcp link --project-id <valid-uuid>` and `link --fix`
//!   must render `✓ <path>` and post-repair `status      linked correctly` (and
//!   updated `assessment`/`linked_project` in `--json`), omitting stale
//!   pre-repair `not linked` / `link it with --project-id <id>` output.
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
const PROJECT_ACME_ID: &str = "33333333-3333-4333-8333-333333333333";
const UNKNOWN_PROJECT_ID: &str = "99999999-9999-4999-8999-999999999999";

async fn spawn_mock_link_server(
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

    let projects = vec![json!({
        "id": PROJECT_ACME_ID,
        "name": "acme-service",
        "workspace_id": WORKSPACE_ID,
        "repository_url": "https://github.com/example-org/acme-service.git",
        "updated_at": chrono::Utc::now().to_rfc3339(),
        "created_at": "2026-01-01T00:00:00Z"
    })];
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
                            "email": "qa-link@example.test",
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
                                    "total": 1,
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

fn init_acme_remote_repo(path: &Path) {
    for args in [
        &["init", "-q"][..],
        &[
            "remote",
            "add",
            "origin",
            "https://github.com/example-org/acme-service.git",
        ][..],
        &["config", "user.email", "dev@example.test"][..],
        &["config", "user.name", "Dev"][..],
    ] {
        let out = Command::new("git")
            .args(args)
            .current_dir(path)
            .output()
            .expect("git init/remote");
        assert!(out.status.success(), "git {args:?} failed");
    }
}

fn save_credentials(home: &Path, api_url: &str, key: &str) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
        .args(["configure", "--api-key-stdin"])
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("CONTEXTSTREAM_API_URL", api_url)
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(format!("{key}\n").as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "configure --api-key-stdin failed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn link_unknown_project_id_fails_nonzero_and_preserves_existing_link() {
    const SECRET: &str = "cs_link_278_secret";
    let (api_url, server) = spawn_mock_link_server(SECRET).await;

    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let linked_repo = temp.path().join("acme-service");
    let unlinked_dir = temp.path().join("plain-unlinked");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&linked_repo).unwrap();
    std::fs::create_dir_all(&unlinked_dir).unwrap();
    init_acme_remote_repo(&linked_repo);

    let h = home.clone();
    let url = api_url.clone();
    tokio::task::spawn_blocking(move || save_credentials(&h, &url, SECRET))
        .await
        .unwrap();

    // Link `linked_repo` to `PROJECT_ACME_ID` first.
    let setup_home = home.clone();
    let setup_repo = linked_repo.clone();
    let setup_url = api_url.clone();
    let initial_link = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args(["link", &format!("--project-id={PROJECT_ACME_ID}")])
            .env("HOME", &setup_home)
            .env("USERPROFILE", &setup_home)
            .env("CONTEXTSTREAM_API_URL", &setup_url)
            .env("NO_COLOR", "1")
            .current_dir(&setup_repo)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(initial_link.status.success());

    // 1. In an already-linked checkout, `link --project-id <unknown> --pin` must fail (rc=1),
    // render `✗`, preserve `.contextstream/config.json`, and NOT write `link-pins.json`.
    let fail_home = home.clone();
    let fail_repo = linked_repo.clone();
    let fail_url = api_url.clone();
    let bad_linked = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args([
                "link",
                &format!("--project-id={UNKNOWN_PROJECT_ID}"),
                "--pin",
            ])
            .env("HOME", &fail_home)
            .env("USERPROFILE", &fail_home)
            .env("CONTEXTSTREAM_API_URL", &fail_url)
            .env("NO_COLOR", "1")
            .current_dir(&fail_repo)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let bad_linked_stdout = String::from_utf8_lossy(&bad_linked.stdout);
    assert!(
        !bad_linked.status.success(),
        "expected non-zero exit for unknown --project-id in linked repo:\nstdout:\n{bad_linked_stdout}"
    );
    assert!(
        bad_linked_stdout.starts_with("✗ "),
        "expected ✗ headline mark:\n{bad_linked_stdout}"
    );
    assert!(
        bad_linked_stdout.contains("status      re-link failed; existing link left unchanged"),
        "{bad_linked_stdout}"
    );
    assert!(
        bad_linked_stdout
            .contains("error       that project is not in any workspace this account can open"),
        "{bad_linked_stdout}"
    );
    assert!(
        !bad_linked_stdout.contains("status      linked correctly"),
        "{bad_linked_stdout}"
    );
    let cfg: Value = serde_json::from_str(
        &std::fs::read_to_string(linked_repo.join(".contextstream/config.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(cfg["project_id"].as_str(), Some(PROJECT_ACME_ID));
    assert!(
        !home.join(".contextstream/link-pins.json").exists(),
        "failed --project-id --pin must not pin the old project"
    );

    // 2. In an unlinked directory, `link --project-id <unknown>` must also render `✗` and exit non-zero.
    let unlinked_home = home.clone();
    let unlinked_path = unlinked_dir.clone();
    let unlinked_url = api_url.clone();
    let bad_unlinked = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args(["link", &format!("--project-id={UNKNOWN_PROJECT_ID}")])
            .env("HOME", &unlinked_home)
            .env("USERPROFILE", &unlinked_home)
            .env("CONTEXTSTREAM_API_URL", &unlinked_url)
            .env("NO_COLOR", "1")
            .current_dir(&unlinked_path)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let bad_unlinked_stdout = String::from_utf8_lossy(&bad_unlinked.stdout);
    assert!(!bad_unlinked.status.success());
    assert!(
        bad_unlinked_stdout.starts_with("✗ "),
        "expected ✗ mark on unlinked error:\n{bad_unlinked_stdout}"
    );
    assert!(
        bad_unlinked_stdout.contains("status      link failed; not linked"),
        "{bad_unlinked_stdout}"
    );

    // 3. Under -q the report is not on stdout, so a folder that is not linked
    // says why on stderr and the command still exits 1 (#354).
    let quiet_home = home.clone();
    let quiet_path = unlinked_dir.clone();
    let quiet_url = api_url.clone();
    let quiet = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args(["-q", "link"])
            .env("HOME", &quiet_home)
            .env("USERPROFILE", &quiet_home)
            .env("CONTEXTSTREAM_API_URL", &quiet_url)
            .env("NO_COLOR", "1")
            .current_dir(&quiet_path)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let quiet_stderr = String::from_utf8_lossy(&quiet.stderr);
    assert_eq!(quiet.status.code(), Some(1), "{quiet_stderr}");
    assert!(quiet.stdout.is_empty(), "-q link printed on stdout");
    assert!(
        quiet_stderr.contains("not linked") && quiet_stderr.contains("--project-id <id>"),
        "-q link says why on stderr:\n{quiet_stderr}"
    );

    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn link_project_id_and_fix_report_healthy_post_repair_status_in_text_and_json() {
    const SECRET: &str = "cs_link_279_secret";
    let (api_url, server) = spawn_mock_link_server(SECRET).await;

    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let plain_a = temp.path().join("unlinked-plain-a");
    let plain_b = temp.path().join("unlinked-plain-b");
    let clone2 = temp.path().join("clone2");
    for dir in [&home, &plain_a, &plain_b, &clone2] {
        std::fs::create_dir_all(dir).unwrap();
    }
    init_acme_remote_repo(&clone2);

    let h = home.clone();
    let url = api_url.clone();
    tokio::task::spawn_blocking(move || save_credentials(&h, &url, SECRET))
        .await
        .unwrap();

    // Case A (text): `link --project-id <PID>` on an unlinked plain folder.
    let a_home = home.clone();
    let a_dir = plain_a.clone();
    let a_url = api_url.clone();
    let out_a = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args(["link", &format!("--project-id={PROJECT_ACME_ID}")])
            .env("HOME", &a_home)
            .env("USERPROFILE", &a_home)
            .env("CONTEXTSTREAM_API_URL", &a_url)
            .env("NO_COLOR", "1")
            .current_dir(&a_dir)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let stdout_a = String::from_utf8_lossy(&out_a.stdout);
    assert!(out_a.status.success(), "{stdout_a}");
    assert!(stdout_a.starts_with("✓ "), "{stdout_a}");
    assert!(
        stdout_a.contains("status      linked correctly"),
        "{stdout_a}"
    );
    assert!(
        stdout_a.contains(&format!(
            "project     acme-service in Testing ({PROJECT_ACME_ID})"
        )),
        "{stdout_a}"
    );
    assert!(
        stdout_a.contains(&format!(
            "re-linked   acme-service in Testing ({PROJECT_ACME_ID})"
        )),
        "{stdout_a}"
    );
    assert!(!stdout_a.contains("not linked"), "{stdout_a}");
    assert!(
        !stdout_a.contains("link it with --project-id"),
        "{stdout_a}"
    );

    // Case A (`--json`): `link --project-id <PID> --json` on an unlinked plain folder.
    let b_home = home.clone();
    let b_dir = plain_b.clone();
    let b_url = api_url.clone();
    let out_b = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args(["link", &format!("--project-id={PROJECT_ACME_ID}"), "--json"])
            .env("HOME", &b_home)
            .env("USERPROFILE", &b_home)
            .env("CONTEXTSTREAM_API_URL", &b_url)
            .env("NO_COLOR", "1")
            .current_dir(&b_dir)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(out_b.status.success());
    let json_b: Value = serde_json::from_slice(&out_b.stdout).unwrap();
    assert_eq!(
        json_b[0]["assessment"]["status"]["status"], "correct",
        "{json_b:#}"
    );
    assert_eq!(
        json_b[0]["assessment"]["current"]["project_id"], PROJECT_ACME_ID,
        "{json_b:#}"
    );
    assert_eq!(
        json_b[0]["assessment"]["current"]["verified"], true,
        "{json_b:#}"
    );
    assert_eq!(
        json_b[0]["linked_project"]["project_id"], PROJECT_ACME_ID,
        "{json_b:#}"
    );
    assert_eq!(
        json_b[0]["relinked_to"]["project_id"], PROJECT_ACME_ID,
        "{json_b:#}"
    );

    // Case B: `link --fix` on an unlinked second clone with matching Git remote.
    let c_home = home.clone();
    let c_dir = clone2.clone();
    let c_url = api_url.clone();
    let out_c = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args(["link", "--fix"])
            .env("HOME", &c_home)
            .env("USERPROFILE", &c_home)
            .env("CONTEXTSTREAM_API_URL", &c_url)
            .env("NO_COLOR", "1")
            .current_dir(&c_dir)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let stdout_c = String::from_utf8_lossy(&out_c.stdout);
    assert!(out_c.status.success(), "{stdout_c}");
    assert!(stdout_c.starts_with("✓ "), "{stdout_c}");
    assert!(
        stdout_c.contains("status      linked correctly"),
        "{stdout_c}"
    );
    assert!(
        stdout_c.contains(&format!(
            "re-linked   acme-service in Testing ({PROJECT_ACME_ID})"
        )),
        "{stdout_c}"
    );
    assert!(!stdout_c.contains("not linked"), "{stdout_c}");
    assert!(!stdout_c.contains("best match"), "{stdout_c}");

    server.abort();
}
