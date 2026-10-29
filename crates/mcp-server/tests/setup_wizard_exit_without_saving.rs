//! Regression test for issue #263:
//! Interactive `contextstream-mcp setup` promises "Nothing is saved until you
//! choose Save." Selecting "Exit without saving" must leave no `credentials.json`
//! or `installation.json` on a fresh `HOME`, and must preserve pre-existing
//! credentials byte-for-byte after "Sign in with a different account".
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
    path::Path,
    process::{Command, Stdio},
};

const WORKSPACE_ID: &str = "11111111-1111-4111-8111-111111111111";
const PROJECT_ID: &str = "33333333-3333-4333-8333-333333333333";
const KEY_A: &str = "cs_test_wizard_exit_account_a";
const KEY_B: &str = "cs_test_wizard_exit_account_b";

async fn spawn_mock_wizard_server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_url = format!("http://{}", listener.local_addr().unwrap());

    let account_for = |headers: &HeaderMap| -> Option<(&'static str, &'static str)> {
        match headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
            Some(KEY_A) => Some((
                "22222222-2222-4222-8222-222222222222",
                "account-a@example.test",
            )),
            Some(KEY_B) => Some((
                "44444444-4444-4444-8444-444444444444",
                "account-b@example.test",
            )),
            _ => None,
        }
    };

    let app = Router::new()
        .route(
            "/api/v1/auth/me",
            get(move |headers: HeaderMap| async move {
                let Some((id, email)) = account_for(&headers) else {
                    return (
                        StatusCode::UNAUTHORIZED,
                        Json(json!({"error": "invalid api key"})),
                    );
                };
                (
                    StatusCode::OK,
                    Json(json!({
                        "data": {
                            "id": id,
                            "email": email,
                            "created_at": "2026-01-01T00:00:00Z"
                        }
                    })),
                )
            }),
        )
        .route(
            "/api/v1/users/me/learning-consent",
            get(move |headers: HeaderMap| async move {
                if account_for(&headers).is_none() {
                    return (
                        StatusCode::UNAUTHORIZED,
                        Json(json!({"error": "invalid api key"})),
                    );
                }
                (StatusCode::OK, Json(json!({"data": {"enabled": true}})))
            }),
        )
        .route(
            "/api/v1/workspaces",
            get(move |headers: HeaderMap| async move {
                if account_for(&headers).is_none() {
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
                if account_for(&headers).is_none() {
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
                if account_for(&headers).is_none() {
                    return (
                        StatusCode::UNAUTHORIZED,
                        Json(json!({"error": "invalid api key"})),
                    );
                }
                (
                    StatusCode::OK,
                    Json(json!({
                        "data": {
                            "items": [],
                            "total": 0,
                            "page": 1,
                            "page_size": 50,
                            "total_pages": 1
                        }
                    })),
                )
            })
            .post(move |headers: HeaderMap, Json(body): Json<Value>| async move {
                if account_for(&headers).is_none() {
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
                (
                    StatusCode::OK,
                    Json(json!({
                        "data": {
                            "id": PROJECT_ID,
                            "name": name,
                            "workspace_id": WORKSPACE_ID
                        }
                    })),
                )
            }),
        )
        .route(
            "/api/v1/projects/{id}",
            get(move |headers: HeaderMap, AxumPath(id): AxumPath<String>| async move {
                if account_for(&headers).is_none() {
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
                            "name": "demo",
                            "workspace_id": WORKSPACE_ID
                        }
                    })),
                )
            }),
        )
        .route(
            "/api/v1/harness-readiness/status",
            get(
                move |headers: HeaderMap, Query(params): Query<HashMap<String, String>>| async move {
                    if account_for(&headers).is_none() {
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

/// Drive the interactive setup wizard inside a real Unix PTY.
/// `steps` is JSON array of `["select", "Prompt substring", "Item substring"]`
/// or `["password", "Prompt substring", "secret"]`.
async fn run_wizard_in_pty(
    home: &Path,
    cwd: &Path,
    api_url: &str,
    env_api_key: Option<&str>,
    steps: Value,
) -> (i32, String) {
    let bin = env!("CARGO_BIN_EXE_contextstream-mcp").to_string();
    let home = home.to_path_buf();
    let cwd = cwd.to_path_buf();
    let api_url = api_url.to_string();
    let env_api_key = env_api_key.map(str::to_string);
    let steps_json = serde_json::to_string(&steps).unwrap();

    let driver = r#"
import json, os, pty, re, select, sys, time

ANSI_RE = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]")
def strip_ansi(s):
    return ANSI_RE.sub("", s)

bin_path = sys.argv[1]
cwd = sys.argv[2]
steps = json.loads(sys.argv[3])

pid, fd = pty.fork()
if pid == 0:
    os.chdir(cwd)
    os.execv(bin_path, [bin_path, "setup", "--editors=claude"])

transcript = ""
cursor = 0

ITEM_RE = re.compile(r"[›>]\s+([^\r\n]+)")

def drain_quiet(quiet_secs=0.05, max_secs=1.0):
    global transcript
    end = time.monotonic() + max_secs
    while time.monotonic() < end:
        r, _, _ = select.select([fd], [], [], quiet_secs)
        if not r:
            return
        try:
            chunk = os.read(fd, 4096)
        except OSError:
            return
        if not chunk:
            return
        transcript += chunk.decode("utf-8", "replace")

def active_item(raw_slice):
    matches = ITEM_RE.findall(strip_ansi(raw_slice))
    return matches[-1].strip() if matches else None

def read_until(substr, require_menu=False, timeout=15.0):
    global transcript
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        clean = strip_ansi(transcript[cursor:])
        if substr in clean:
            tail = clean[clean.index(substr):]
            if not require_menu or ITEM_RE.search(tail):
                drain_quiet(0.05, 0.5)
                return strip_ansi(transcript[cursor:])
        rem = max(0.05, min(0.25, deadline - time.monotonic()))
        r, _, _ = select.select([fd], [], [], rem)
        if r:
            try:
                chunk = os.read(fd, 4096)
            except OSError:
                break
            if not chunk:
                break
            transcript += chunk.decode("utf-8", "replace")
    raise RuntimeError(f"Timed out waiting for {substr!r} in:\n{strip_ansi(transcript)}")

for kind, prompt_sub, target in steps:
    if kind == "password":
        read_until(prompt_sub, require_menu=False)
        cursor = len(transcript)
        os.write(fd, (target + "\r").encode("utf-8"))
    elif kind == "select":
        read_until(prompt_sub, require_menu=True)
        for _ in range(12):
            current = active_item(transcript[cursor:])
            if current and target in current:
                break
            step_pos = len(transcript)
            os.write(fd, b"\x1b[B")
            deadline = time.monotonic() + 2.0
            while time.monotonic() < deadline:
                r, _, _ = select.select([fd], [], [], 0.05)
                if r:
                    try:
                        chunk = os.read(fd, 4096)
                    except OSError:
                        break
                    if chunk:
                        transcript += chunk.decode("utf-8", "replace")
                        if active_item(transcript[step_pos:]) is not None:
                            drain_quiet(0.04, 0.3)
                            break
        else:
            raise RuntimeError(f"Could not highlight {target!r} in:\n{strip_ansi(transcript)}")
        cursor = len(transcript)
        os.write(fd, b"\r")

deadline = time.monotonic() + 20.0
pty_open = True
while time.monotonic() < deadline:
    if pty_open:
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try:
                chunk = os.read(fd, 4096)
                if chunk:
                    transcript += chunk.decode("utf-8", "replace")
                else:
                    pty_open = False
            except OSError:
                pty_open = False
    else:
        time.sleep(0.02)
    wpid, status = os.waitpid(pid, os.WNOHANG)
    if wpid == pid:
        if pty_open:
            while True:
                r, _, _ = select.select([fd], [], [], 0.01)
                if not r:
                    break
                try:
                    chunk = os.read(fd, 4096)
                    if not chunk:
                        break
                    transcript += chunk.decode("utf-8", "replace")
                except OSError:
                    break
        os.close(fd)
        code = os.WEXITSTATUS(status) if os.WIFEXITED(status) else 128
        sys.stdout.write(strip_ansi(transcript))
        sys.exit(code)

os.kill(pid, 9)
sys.stdout.write(strip_ansi(transcript))
sys.exit(124)
"#;

    tokio::task::spawn_blocking(move || {
        let mut cmd = Command::new("python3");
        cmd.args(["-c", driver, &bin, cwd.to_str().unwrap(), &steps_json])
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("CONTEXTSTREAM_API_URL", &api_url)
            .env("CONTEXTSTREAM_NO_BROWSER", "1")
            .env("CONTEXTSTREAM_WATCH_DISABLED", "1")
            .env("TERM", "xterm-256color")
            .env_remove("NO_COLOR")
            .env_remove("CONTEXTSTREAM_WORKSPACE_ID");
        if let Some(key) = env_api_key {
            cmd.env("CONTEXTSTREAM_API_KEY", key);
        } else {
            cmd.env_remove("CONTEXTSTREAM_API_KEY");
        }
        let out = cmd.output().expect("python3 available for PTY test");
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.code().unwrap_or(1), combined)
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wizard_exit_without_saving_leaves_fresh_and_existing_credentials_untouched() {
    let (api_url, server) = spawn_mock_wizard_server().await;
    let temp = tempfile::tempdir().unwrap();

    // 1. Fresh HOME + Paste an API key + Exit without saving -> zero ~/.contextstream files.
    let fresh_home = temp.path().join("fresh-home");
    let project_1 = temp.path().join("proj-1");
    std::fs::create_dir_all(&fresh_home).unwrap();
    std::fs::create_dir_all(&project_1).unwrap();
    init_git_repo(&project_1);

    let (code_1, transcript_1) = run_wizard_in_pty(
        &fresh_home,
        &project_1,
        &api_url,
        None,
        json!([
            ["select", "How do you want to sign in?", "Paste an API key"],
            ["password", "Paste your API key", KEY_A],
            [
                "select",
                "Ready to set up ContextStream?",
                "Exit without saving"
            ]
        ]),
    )
    .await;
    assert_eq!(code_1, 0, "fresh exit failed:\n{transcript_1}");
    assert!(
        transcript_1.contains("Setup cancelled"),
        "expected cancellation notice:\n{transcript_1}"
    );
    assert!(
        !transcript_1.contains("saved to"),
        "Step 1 must not claim credentials were saved before Review:\n{transcript_1}"
    );
    assert!(
        !fresh_home.join(".contextstream").exists(),
        "Exit without saving on fresh HOME left ~/.contextstream entries: {:?}",
        std::fs::read_dir(fresh_home.join(".contextstream"))
            .map(|it| it.flatten().map(|e| e.file_name()).collect::<Vec<_>>())
    );

    // 2. Existing Account A credentials.json + Sign in with a different account (Account B)
    // + Exit without saving -> Account A credentials.json preserved byte-for-byte.
    let existing_home = temp.path().join("existing-home");
    let cs_dir = existing_home.join(".contextstream");
    std::fs::create_dir_all(&cs_dir).unwrap();
    let creds_path = cs_dir.join("credentials.json");
    let seeded_creds = format!(
        "{{\n  \"api_key\": \"{KEY_A}\",\n  \"api_url\": \"{api_url}\",\n  \"saved_at\": \"2026-01-01T00:00:00Z\"\n}}\n"
    );
    std::fs::write(&creds_path, &seeded_creds).unwrap();
    let before_bytes = std::fs::read(&creds_path).unwrap();

    let (code_2, transcript_2) = run_wizard_in_pty(
        &existing_home,
        &project_1,
        &api_url,
        None,
        json!([
            [
                "select",
                "Ready to set up ContextStream?",
                "Sign in with a different account"
            ],
            ["select", "How do you want to sign in?", "Paste an API key"],
            ["password", "Paste your API key", KEY_B],
            [
                "select",
                "Ready to set up ContextStream?",
                "Exit without saving"
            ]
        ]),
    )
    .await;
    assert_eq!(code_2, 0, "account-switch exit failed:\n{transcript_2}");
    assert!(
        transcript_2.contains("account-b@example.test"),
        "expected switched account on Review screen:\n{transcript_2}"
    );
    assert_eq!(
        std::fs::read(&creds_path).unwrap(),
        before_bytes,
        "Exit without saving after switching accounts must preserve prior credentials.json"
    );
    assert!(
        !cs_dir.join("installation.json").exists(),
        "Exit without saving must not create installation.json"
    );

    // 3. Control: Save and finish persists both credentials.json and installation.json.
    let save_home = temp.path().join("save-home");
    std::fs::create_dir_all(&save_home).unwrap();
    let (code_3, transcript_3) = run_wizard_in_pty(
        &save_home,
        &project_1,
        &api_url,
        Some(KEY_A),
        json!([[
            "select",
            "Ready to set up ContextStream?",
            "Save and finish"
        ]]),
    )
    .await;
    stop_spawned_watch_helper_if_any(&save_home);
    server.abort();

    assert_eq!(code_3, 0, "Save and finish failed:\n{transcript_3}");
    let saved_creds: Value = serde_json::from_str(
        &std::fs::read_to_string(save_home.join(".contextstream/credentials.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(saved_creds["api_key"], KEY_A);
    let saved_install: Value = serde_json::from_str(
        &std::fs::read_to_string(save_home.join(".contextstream/installation.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(saved_install["configured_clients"], json!(["claude"]));
}
