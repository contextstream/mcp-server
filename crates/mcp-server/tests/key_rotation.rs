//! Regression test for #270:
//! 1. Rotating an API key via `configure --api-key-stdin` refreshes pre-authenticated
//!    keys in configured editor MCP configs (`~/.claude.json`, `~/.cursor/mcp.json`,
//!    `~/.codex/config.toml`, and project `.mcp.json`) without creating unsolicited
//!    configs when run before `setup`.
//! 2. `doctor` detects when an editor MCP config embeds a stale/revoked API key that
//!    differs from `~/.contextstream/credentials.json` without leaking secret values,
//!    and `doctor --repair` rewrites the config to match current credentials and
//!    reports the modified paths rather than `No managed changes were needed`.
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
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::{Arc, Mutex},
};

const WORKSPACE_ID: &str = "11111111-1111-4111-8111-111111111111";
const USER_ID: &str = "22222222-2222-4222-8222-222222222222";
const KEY_A: &str = "cs_mock_initial_key_a_1111";
const KEY_B: &str = "cs_mock_rotated_key_b_2222";
const REVOKED_DEAD_KEY: &str = "cs_revoked_dead_key_000";

async fn spawn_mock_api_server() -> (String, Arc<Mutex<String>>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_url = format!("http://{}", listener.local_addr().unwrap());
    let valid_key = Arc::new(Mutex::new(KEY_A.to_string()));

    let check_auth = {
        let valid_key = Arc::clone(&valid_key);
        move |headers: &HeaderMap| -> bool {
            let expected = valid_key.lock().unwrap().clone();
            headers
                .get("x-api-key")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|key| key == expected)
        }
    };

    let auth_me = check_auth.clone();
    let auth_consent = check_auth.clone();
    let auth_ws_list = check_auth.clone();
    let auth_ws_get = check_auth.clone();
    let auth_proj = check_auth.clone();
    let auth_readiness = check_auth.clone();

    let app = Router::new()
        .route(
            "/api/v1/auth/me",
            get(move |headers: HeaderMap| {
                let auth = auth_me.clone();
                async move {
                    if !auth(&headers) {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({"error": "invalid or revoked api key"})),
                        );
                    }
                    (
                        StatusCode::OK,
                        Json(json!({
                            "data": {
                                "id": USER_ID,
                                "email": "keyrot@example.test",
                                "created_at": "2026-01-01T00:00:00Z"
                            }
                        })),
                    )
                }
            }),
        )
        .route(
            "/api/v1/users/me/learning-consent",
            get(move |headers: HeaderMap| {
                let auth = auth_consent.clone();
                async move {
                    if !auth(&headers) {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({"error": "invalid or revoked api key"})),
                        );
                    }
                    (StatusCode::OK, Json(json!({"data": {"enabled": false}})))
                }
            }),
        )
        .route(
            "/api/v1/workspaces",
            get(move |headers: HeaderMap| {
                let auth = auth_ws_list.clone();
                async move {
                    if !auth(&headers) {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({"error": "invalid or revoked api key"})),
                        );
                    }
                    (
                        StatusCode::OK,
                        Json(json!({
                            "data": {
                                "items": [
                                    {
                                        "id": WORKSPACE_ID,
                                        "name": "RotationTest",
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
            get(move |headers: HeaderMap, AxumPath(id): AxumPath<String>| {
                let auth = auth_ws_get.clone();
                async move {
                    if !auth(&headers) {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({"error": "invalid or revoked api key"})),
                        );
                    }
                    (
                        StatusCode::OK,
                        Json(json!({
                            "data": {
                                "id": id,
                                "name": "RotationTest",
                                "created_at": "2026-01-01T00:00:00Z",
                                "updated_at": "2026-01-01T00:00:00Z"
                            }
                        })),
                    )
                }
            }),
        )
        .route(
            "/api/v1/projects",
            get(
                move |headers: HeaderMap, Query(params): Query<HashMap<String, String>>| {
                    let auth = auth_proj.clone();
                    async move {
                        if !auth(&headers) {
                            return (
                                StatusCode::UNAUTHORIZED,
                                Json(json!({"error": "invalid or revoked api key"})),
                            );
                        }
                        let page = params
                            .get("page")
                            .and_then(|raw| raw.parse::<u32>().ok())
                            .unwrap_or(1);
                        (
                            StatusCode::OK,
                            Json(json!({
                                "data": {
                                    "items": [],
                                    "total": 0,
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
            "/api/v1/telemetry/harness-readiness",
            get(move |headers: HeaderMap| {
                let auth = auth_readiness.clone();
                async move {
                    if !auth(&headers) {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({"error": "invalid or revoked api key"})),
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
                }
            }),
        );

    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    (api_url, valid_key, handle)
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

fn run_cli_with_stdin(
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
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("CONTEXTSTREAM_API_URL", api_url)
        .env("CONTEXTSTREAM_WATCH_ENABLED", "false")
        .env_remove("CONTEXTSTREAM_API_KEY")
        .env_remove("CONTEXTSTREAM_JWT")
        .env_remove("CONTEXTSTREAM_WORKSPACE_ID")
        .env_remove("CONTEXTSTREAM_PROJECT_ID")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(payload) = stdin_payload {
        let mut child = cmd.stdin(Stdio::piped()).spawn().expect("spawn cli");
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(payload.as_bytes())
            .expect("write stdin");
        child.wait_with_output().expect("wait cli")
    } else {
        cmd.stdin(Stdio::null()).output().expect("run cli")
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configure_api_key_stdin_rotates_editor_configs_and_doctor_repairs_stale_keys() {
    let (api_url, valid_key, server_handle) = spawn_mock_api_server().await;
    let temp = tempfile::tempdir().expect("tempdir");
    let home = temp.path().join("home");
    std::fs::create_dir_all(home.join(".claude")).expect("mkdir .claude");
    let bin = install_release_like_binary(&home);

    // 1. Initial `configure --api-key-stdin` BEFORE `setup`: saves KEY_A and does NOT create ~/.claude.json.
    let step1 = run_cli_with_stdin(
        &bin,
        &home,
        &home,
        &api_url,
        &["configure", "--api-key-stdin"],
        Some(&format!(
            "{KEY_A}
"
        )),
    );
    assert!(
        step1.status.success(),
        "initial configure failed: stdout={} stderr={}",
        String::from_utf8_lossy(&step1.stdout),
        String::from_utf8_lossy(&step1.stderr)
    );
    assert!(
        !home.join(".claude.json").exists(),
        "configure --api-key-stdin before setup must not create unsolicited editor configs"
    );

    // 2. Run `setup --yes --account-only --editors=claude,cursor,codex`: embeds KEY_A.
    let setup_arg = format!("--workspace-id={WORKSPACE_ID}");
    let setup_out = run_cli_with_stdin(
        &bin,
        &home,
        &home,
        &api_url,
        &[
            "setup",
            "--yes",
            "--account-only",
            "--editors=claude,cursor,codex",
            &setup_arg,
        ],
        None,
    );
    assert!(
        setup_out.status.success(),
        "setup failed: stdout={} stderr={}",
        String::from_utf8_lossy(&setup_out.stdout),
        String::from_utf8_lossy(&setup_out.stderr)
    );

    let claude_path = home.join(".claude.json");
    let cursor_path = home.join(".cursor").join("mcp.json");
    let codex_path = home.join(".codex").join("config.toml");
    for path in [&claude_path, &cursor_path, &codex_path] {
        let raw = std::fs::read_to_string(path).expect("read config after setup");
        assert!(
            raw.contains(KEY_A),
            "{} expected KEY_A:
{raw}",
            path.display()
        );
    }

    // 3. Revoke KEY_A on server, switch active valid key to KEY_B, and rotate via `configure --api-key-stdin`.
    *valid_key.lock().unwrap() = KEY_B.to_string();
    let rotate_out = run_cli_with_stdin(
        &bin,
        &home,
        &home,
        &api_url,
        &["configure", "--api-key-stdin"],
        Some(&format!(
            "{KEY_B}
"
        )),
    );
    assert!(
        rotate_out.status.success(),
        "rotate configure failed: stdout={} stderr={}",
        String::from_utf8_lossy(&rotate_out.stdout),
        String::from_utf8_lossy(&rotate_out.stderr)
    );
    let rotate_stderr = String::from_utf8_lossy(&rotate_out.stderr);
    assert!(
        rotate_stderr.contains("Refreshed API key in 3 editor MCP config(s)."),
        "expected refresh summary in stderr: {rotate_stderr}"
    );

    for path in [&claude_path, &cursor_path, &codex_path] {
        let raw = std::fs::read_to_string(path).expect("read config after rotation");
        assert!(
            !raw.contains(KEY_A),
            "OLD KEY_A still present in {}:
{raw}",
            path.display()
        );
        assert!(
            raw.contains(KEY_B),
            "NEW KEY_B missing in {}:
{raw}",
            path.display()
        );
    }

    // `doctor` passes cleanly after `configure --api-key-stdin` rotation.
    let doctor_healthy = run_cli_with_stdin(&bin, &home, &home, &api_url, &["doctor"], None);
    assert!(
        doctor_healthy.status.success(),
        "doctor after rotation failed: stdout={} stderr={}",
        String::from_utf8_lossy(&doctor_healthy.stdout),
        String::from_utf8_lossy(&doctor_healthy.stderr)
    );

    // 4. Plant a dead/revoked Bearer key in ~/.claude.json and stale X-ContextStream-API-Key in ~/.cursor/mcp.json.
    let mut claude_val: Value =
        serde_json::from_str(&std::fs::read_to_string(&claude_path).unwrap()).unwrap();
    claude_val["mcpServers"]["contextstream"]["headers"]
        .as_object_mut()
        .unwrap()
        .remove("X-ContextStream-API-Key");
    claude_val["mcpServers"]["contextstream"]["headers"]["Authorization"] =
        json!(format!("Bearer {REVOKED_DEAD_KEY}"));
    std::fs::write(
        &claude_path,
        serde_json::to_string_pretty(&claude_val).unwrap(),
    )
    .unwrap();

    let mut cursor_val: Value =
        serde_json::from_str(&std::fs::read_to_string(&cursor_path).unwrap()).unwrap();
    cursor_val["mcpServers"]["contextstream"]["headers"]["X-ContextStream-API-Key"] =
        json!(REVOKED_DEAD_KEY);
    std::fs::write(
        &cursor_path,
        serde_json::to_string_pretty(&cursor_val).unwrap(),
    )
    .unwrap();

    // `doctor` must exit non-zero (unhealthy) and NEVER leak either secret key in stdout/stderr.
    let doctor_stale = run_cli_with_stdin(&bin, &home, &home, &api_url, &["doctor"], None);
    assert!(
        !doctor_stale.status.success(),
        "doctor must fail when editor MCP configs embed a revoked/mismatched API key: stdout={}",
        String::from_utf8_lossy(&doctor_stale.stdout)
    );
    let stale_combined = format!(
        "{}
{}",
        String::from_utf8_lossy(&doctor_stale.stdout),
        String::from_utf8_lossy(&doctor_stale.stderr)
    );
    assert!(
        stale_combined.contains("embedded API key does not match saved credentials"),
        "expected mismatch diagnostic:
{stale_combined}"
    );
    assert!(!stale_combined.contains(REVOKED_DEAD_KEY));
    assert!(!stale_combined.contains(KEY_B));

    // 5. `doctor --repair` must replace the dead keys with KEY_B, report the modified files, and pass.
    let repair_out =
        run_cli_with_stdin(&bin, &home, &home, &api_url, &["doctor", "--repair"], None);
    let repair_stdout = String::from_utf8_lossy(&repair_out.stdout);
    let repair_stderr = String::from_utf8_lossy(&repair_out.stderr);
    assert!(
        repair_out.status.success(),
        "doctor --repair failed: stdout={repair_stdout}
stderr={repair_stderr}"
    );
    assert!(
        !repair_stdout.contains("No managed changes were needed"),
        "doctor --repair must report applied file modifications:
{repair_stdout}"
    );

    for path in [&claude_path, &cursor_path] {
        let raw = std::fs::read_to_string(path).expect("read config after repair");
        assert!(
            !raw.contains(REVOKED_DEAD_KEY),
            "REVOKED_DEAD_KEY still present after doctor --repair in {}:
{raw}",
            path.display()
        );
        assert!(
            raw.contains(KEY_B),
            "KEY_B missing after doctor --repair in {}:
{raw}",
            path.display()
        );
    }

    // 6. Re-running `doctor --repair` when everything is already current reports no changes needed.
    let noop_repair =
        run_cli_with_stdin(&bin, &home, &home, &api_url, &["doctor", "--repair"], None);
    let noop_stdout = String::from_utf8_lossy(&noop_repair.stdout);
    assert!(
        noop_repair.status.success(),
        "second doctor --repair failed: stdout={noop_stdout}"
    );
    assert!(
        noop_stdout.contains("No managed changes were needed"),
        "expected idempotent second doctor --repair:
{noop_stdout}"
    );

    server_handle.abort();
}
