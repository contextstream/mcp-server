//! Regression test for issue #256:
//! `setup --keep-claude-auto-memory` persists its opt-in in
//! `~/.contextstream/installation.json` so `doctor` treats Claude Code auto
//! memory as intentionally enabled, `doctor --repair` never clobbers
//! `~/.claude/settings.json` with `"autoMemoryEnabled": false`, and `PreToolUse`
//! allows Claude's local memory writes.
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
    sync::{Arc, Mutex},
};

const WORKSPACE_ID: &str = "11111111-1111-4111-8111-111111111111";
const PROJECT_ID: &str = "33333333-3333-4333-8333-333333333333";

async fn spawn_mock_keepmem_server(
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
                            "email": "qa-keepmem@example.test",
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
    std::fs::write(path.join("a.rs"), "x\n").unwrap();
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
async fn setup_keep_claude_auto_memory_persists_passes_doctor_and_survives_repair() {
    const SECRET: &str = "cs_test_keep_claude_auto_memory_256";
    let (api_url, server) = spawn_mock_keepmem_server(SECRET).await;
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("qa-r2-keepmem");
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    init_git_repo(&project);

    // Seed `~/.claude/settings.json` with auto memory explicitly enabled.
    let claude_settings_path = home.join(".claude/settings.json");
    std::fs::write(
        &claude_settings_path,
        serde_json::to_string_pretty(&json!({
            "model": "opus",
            "autoMemoryEnabled": true
        }))
        .unwrap(),
    )
    .unwrap();

    // Configure credentials via `configure --api-key-stdin`.
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

    // 1. Run `setup --yes --editors=claude --keep-claude-auto-memory --workspace-id=...`.
    let setup_home = home.clone();
    let setup_proj = project.clone();
    let setup_url = api_url.clone();
    let setup_out = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args([
                "setup",
                "--yes",
                "--editors=claude",
                "--keep-claude-auto-memory",
                &format!("--workspace-id={WORKSPACE_ID}"),
            ])
            .env("HOME", &setup_home)
            .env("USERPROFILE", &setup_home)
            .env("CONTEXTSTREAM_API_URL", &setup_url)
            .env("NO_COLOR", "1")
            .current_dir(&setup_proj)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        setup_out.status.success(),
        "setup failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&setup_out.stdout),
        String::from_utf8_lossy(&setup_out.stderr)
    );

    let installation: Value = serde_json::from_str(
        &std::fs::read_to_string(home.join(".contextstream/installation.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        installation.get("keep_claude_auto_memory"),
        Some(&Value::Bool(true)),
        "installation.json must persist keep_claude_auto_memory: {installation}"
    );

    let claude_after_setup: Value =
        serde_json::from_str(&std::fs::read_to_string(&claude_settings_path).unwrap()).unwrap();
    assert_eq!(
        claude_after_setup.get("autoMemoryEnabled"),
        Some(&Value::Bool(true)),
        "setup --keep-claude-auto-memory must preserve autoMemoryEnabled=true: {claude_after_setup}"
    );

    // 2. Run `doctor --json` and verify `claude_auto_memory` passes without a repair prompt.
    let doc_home = home.clone();
    let doc_proj = project.clone();
    let doc_url = api_url.clone();
    let doc_out = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args(["doctor", "--json"])
            .env("HOME", &doc_home)
            .env("USERPROFILE", &doc_home)
            .env("CONTEXTSTREAM_API_URL", &doc_url)
            .env("NO_COLOR", "1")
            .current_dir(&doc_proj)
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let doc_json: Value = serde_json::from_slice(&doc_out.stdout).unwrap_or_else(|e| {
        panic!(
            "doctor --json parse failed ({e}):\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&doc_out.stdout),
            String::from_utf8_lossy(&doc_out.stderr)
        )
    });
    let claude_editor = doc_json["editors"]
        .as_array()
        .unwrap()
        .iter()
        .find(|editor| editor.get("editor").and_then(Value::as_str) == Some("claude"))
        .expect("claude editor in doctor report");
    let auto_memory_check = claude_editor["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|check| check.get("surface").and_then(Value::as_str) == Some("claude_auto_memory"))
        .expect("claude_auto_memory check in doctor report");
    assert_eq!(
        auto_memory_check.get("status").and_then(Value::as_str),
        Some("pass"),
        "doctor should treat kept-on auto memory as pass: {auto_memory_check}"
    );
    assert!(
        auto_memory_check.get("fix").is_none_or(Value::is_null),
        "doctor should not recommend repairing kept-on auto memory: {auto_memory_check}"
    );

    // 3. Run `doctor --repair` and `doctor --repair --scope global --editors claude`:
    // neither may clobber `autoMemoryEnabled: true`.
    for repair_args in [
        &["doctor", "--repair"][..],
        &[
            "doctor",
            "--repair",
            "--scope",
            "global",
            "--editors",
            "claude",
        ][..],
    ] {
        let rep_home = home.clone();
        let rep_proj = project.clone();
        let rep_url = api_url.clone();
        let args: Vec<String> = repair_args.iter().map(|s| (*s).to_string()).collect();
        let rep_out = tokio::task::spawn_blocking(move || {
            Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
                .args(&args)
                .env("HOME", &rep_home)
                .env("USERPROFILE", &rep_home)
                .env("CONTEXTSTREAM_API_URL", &rep_url)
                .env("NO_COLOR", "1")
                .current_dir(&rep_proj)
                .output()
                .unwrap()
        })
        .await
        .unwrap();
        assert!(
            rep_out.status.success(),
            "doctor --repair ({repair_args:?}) failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&rep_out.stdout),
            String::from_utf8_lossy(&rep_out.stderr)
        );
        let settings_after_repair: Value =
            serde_json::from_str(&std::fs::read_to_string(&claude_settings_path).unwrap()).unwrap();
        assert_eq!(
            settings_after_repair.get("autoMemoryEnabled"),
            Some(&Value::Bool(true)),
            "doctor --repair ({repair_args:?}) must not clobber autoMemoryEnabled: {settings_after_repair}"
        );
    }

    // 4. Verify PreToolUse hook allows writing to `~/.claude/projects/*/memory/*.md`.
    let hook_home = home.clone();
    let hook_out = tokio::task::spawn_blocking(move || {
        let memory_file = hook_home.join(".claude/projects/demo/memory/pref.md");
        let payload = json!({
            "hook_event_name": "PreToolUse",
            "session_id": "keep-mem-session",
            "cwd": hook_home,
            "tool_name": "Write",
            "tool_input": {
                "file_path": memory_file,
                "content": "---\nname: pref\ndescription: x\nmetadata:\n  type: feedback\n---\nKeep local.",
            },
        });
        let mut child = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .args(["hook", "pre-tool-use", "--contextstream-managed-hook=v1"])
            .env_clear()
            .env("HOME", &hook_home)
            .env("USERPROFILE", &hook_home)
            .env("CLAUDE_CONFIG_DIR", hook_home.join(".claude"))
            .env("CONTEXTSTREAM_API_URL", "http://127.0.0.1:9")
            .env("CONTEXTSTREAM_REMINDER_ENABLED", "false")
            .current_dir(&hook_home)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(payload.to_string().as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    })
    .await
    .unwrap();
    assert!(hook_out.status.success());
    let hook_stdout = String::from_utf8_lossy(&hook_out.stdout);
    assert!(
        !hook_stdout.contains("\"permissionDecision\":\"deny\""),
        "PreToolUse should allow Claude memory writes when keep_claude_auto_memory is persisted: {hook_stdout}"
    );

    stop_spawned_watch_helper_if_any(&home);
    server.abort();
}
