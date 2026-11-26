//! Regression tests for ticket e69fbc52: `contextstream-mcp update`, run with
//! another account's HOME inside a repository, rewrote that repository's
//! project `.mcp.json` with the HOME account's API key and installation id
//! and pointed its `headersHelper` at the HOME's own binary, without a word,
//! and created further project entries for that account. Every call from the
//! repository then ran as the wrong account, through another person's binary.
//!
//! `update` itself downloads its release manifest first, so these tests run
//! the config refresh it hands to the installed binary: `migrate-remote
//! --scope=all --only-configured` (with `--switch-account` handed on when
//! given). The world is all temporary:
//! - B's HOME holds B's key in its saved credentials (no key in the
//!   environment), names B's API (stub B), has set up Claude Code and Cursor,
//!   and runs the CLI from its own managed binary;
//! - A's repository holds A's hosted entry, which talks to A's API (stub A)
//!   and runs A's binary as its `headersHelper`;
//! - both stubs answer `/api/v1/auth/me` and record which key reached them.
#![cfg(unix)]

use axum::{
    http::{HeaderMap, StatusCode, Uri},
    Json, Router,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

const KEY_A: &str = "cs_test_owner_a_key_aaaa1111";
const KEY_B: &str = "cs_test_demo_b_key_bbbb2222";
const KEY_B_OLD: &str = "cs_test_demo_b_oldkey_cccc3333";
const A_INSTALLATION: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const B_INSTALLATION: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const A_WORKSPACE: &str = "11111111-1111-4111-8111-111111111111";
const A_PROJECT: &str = "22222222-2222-4222-8222-222222222222";
const API_KEY_HEADER: &str = "X-ContextStream-API-Key";
const INSTALLATION_HEADER: &str = "X-ContextStream-Installation-Id";

fn key_label(key: Option<&str>) -> &'static str {
    match key {
        Some(KEY_A) => "A",
        Some(KEY_B) => "B",
        Some(KEY_B_OLD) => "B_OLD",
        Some(_) => "other",
        None => "none",
    }
}

/// An API that answers `/api/v1/auth/me` for the three test keys and
/// records, per request, the path and which key reached it.
struct Stub {
    url: String,
    seen: Arc<Mutex<Vec<(String, &'static str)>>>,
    handle: tokio::task::JoinHandle<()>,
}

impl Stub {
    async fn spawn() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen: Arc<Mutex<Vec<(String, &'static str)>>> = Arc::default();
        let record = Arc::clone(&seen);
        let app = Router::new().fallback(move |uri: Uri, headers: HeaderMap| {
            let record = Arc::clone(&record);
            async move {
                let key = headers
                    .get("x-api-key")
                    .or_else(|| headers.get(API_KEY_HEADER))
                    .and_then(|value| value.to_str().ok())
                    .map(String::from)
                    .or_else(|| {
                        headers
                            .get("authorization")
                            .and_then(|value| value.to_str().ok())
                            .and_then(|value| value.strip_prefix("Bearer "))
                            .map(String::from)
                    });
                let label = key_label(key.as_deref());
                record.lock().unwrap().push((uri.path().to_string(), label));
                if uri.path() != "/api/v1/auth/me" {
                    return (StatusCode::NOT_FOUND, Json(json!({"error": "not found"})));
                }
                let email = match label {
                    "A" => "owner@example.test",
                    "B" | "B_OLD" => "demo@example.test",
                    _ => {
                        return (
                            StatusCode::UNAUTHORIZED,
                            Json(json!({"error": "invalid api key"})),
                        )
                    }
                };
                (StatusCode::OK, Json(json!({"data": {"email": email}})))
            }
        });
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { url, seen, handle }
    }

    /// The keys that reached this API, by label, on `path`.
    fn keys_on(&self, path: &str) -> Vec<&'static str> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|(seen_path, _)| seen_path == path)
            .map(|(_, label)| *label)
            .collect()
    }

    fn any_key(&self, label: &str) -> bool {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .any(|(_, seen)| *seen == label)
    }

    fn clear(&self) {
        self.seen.lock().unwrap().clear();
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// B's HOME, A's home and A's repository, in a temporary root.
struct World {
    _temp: tempfile::TempDir,
    b_home: PathBuf,
    a_binary: String,
    b_binary: PathBuf,
    repo: PathBuf,
}

impl World {
    fn new(b_api: &str) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let b_home = root.join("b-home");
        let repo = root.join("a-repo");
        let a_binary = root
            .join("a-home")
            .join(".contextstream")
            .join("bin")
            .join("contextstream-mcp");
        std::fs::create_dir_all(a_binary.parent().unwrap()).unwrap();
        std::fs::write(&a_binary, b"").unwrap();

        let b_dir = b_home.join(".contextstream");
        std::fs::create_dir_all(b_dir.join("bin")).unwrap();
        std::fs::create_dir_all(b_home.join(".claude")).unwrap();
        let b_binary = b_dir.join("bin").join("contextstream-mcp");
        let built = PathBuf::from(env!("CARGO_BIN_EXE_contextstream-mcp"));
        if std::fs::hard_link(&built, &b_binary).is_err() {
            // A copy made by a child process, so no descriptor writing the
            // binary is open here when another test forks (ETXTBSY).
            let status = Command::new("cp")
                .arg(&built)
                .arg(&b_binary)
                .status()
                .unwrap();
            assert!(status.success());
        }
        // B's key lives in B's saved credentials, with B's API.
        std::fs::write(
            b_dir.join("credentials.json"),
            json!({"api_key": KEY_B, "api_url": b_api}).to_string(),
        )
        .unwrap();
        std::fs::write(
            b_dir.join("installation.json"),
            json!({
                "schema_version": 1,
                "installation_id": B_INSTALLATION,
                "created_at": "2026-11-01T00:00:00Z",
                "configured_clients": ["claude", "cursor"],
                "last_install_completed_at": "2026-11-01T00:00:00Z",
            })
            .to_string(),
        )
        .unwrap();

        std::fs::create_dir_all(repo.join(".contextstream")).unwrap();
        assert!(Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap()
            .success());
        std::fs::write(
            repo.join(".contextstream").join("config.json"),
            json!({
                "workspace_id": A_WORKSPACE,
                "workspace_name": "Owner WS",
                "project_id": A_PROJECT,
                "project_name": "a-repo",
            })
            .to_string(),
        )
        .unwrap();
        Self {
            _temp: temp,
            b_home,
            a_binary: a_binary.display().to_string(),
            b_binary,
            repo,
        }
    }

    fn a_helper(&self) -> String {
        format!("{} hook mcp-headers", self.a_binary)
    }

    fn b_helper(&self) -> String {
        format!("{} hook mcp-headers", self.b_binary.display())
    }

    fn project_config(&self) -> PathBuf {
        self.repo.join(".mcp.json")
    }

    fn cursor_project_config(&self) -> PathBuf {
        self.repo.join(".cursor").join("mcp.json")
    }

    /// A's hosted entry in the repository: it talks to A's gateway (stub
    /// A), holds `key`, names A's installation and runs `helper`.
    fn seed_owner_entry(&self, a_api: &str, key: &str, helper: &str) -> Value {
        let entry = json!({"mcpServers": {"contextstream": {
            "type": "http",
            "url": format!("{a_api}/mcp?default_context_mode=fast"),
            "headers": {
                "X-ContextStream-Managed-Config-Version": "2",
                API_KEY_HEADER: key,
                INSTALLATION_HEADER: A_INSTALLATION,
                "X-ContextStream-Workspace-Id": A_WORKSPACE,
            },
            "headersHelper": helper,
        }}});
        std::fs::write(
            self.project_config(),
            serde_json::to_string_pretty(&entry).unwrap(),
        )
        .unwrap();
        entry
    }

    /// The CLI from B's managed binary, in B's HOME and A's repository, with
    /// no terminal and no key or API in the environment.
    async fn run(&self, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
        self.run_in(&self.repo, args, extra_env).await
    }

    async fn run_in(&self, cwd: &Path, args: &[&str], extra_env: &[(&str, &str)]) -> Output {
        let mut command = Command::new(&self.b_binary);
        command
            .args(args)
            .env_clear()
            .env("HOME", &self.b_home)
            .env("XDG_CONFIG_HOME", self.b_home.join(".config"))
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("CONTEXTSTREAM_WATCH", "0")
            .env("CONTEXTSTREAM_NO_BROWSER", "1")
            .env("NO_COLOR", "1")
            .current_dir(cwd)
            .stdin(Stdio::null());
        for (name, value) in extra_env {
            command.env(name, value);
        }
        tokio::task::spawn_blocking(move || command.output().unwrap())
            .await
            .unwrap()
    }

    /// The refresh `update` runs.
    async fn refresh(&self, extra: &[&str]) -> Output {
        let mut args = vec!["migrate-remote", "--scope=all", "--only-configured"];
        args.extend_from_slice(extra);
        self.run(&args, &[]).await
    }
}

fn entry_of(path: &Path) -> Value {
    let config: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    config["mcpServers"]["contextstream"].clone()
}

fn said(output: &Output) -> String {
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    for key in [KEY_A, KEY_B, KEY_B_OLD] {
        assert!(!text.contains(key), "a key was printed:\n{text}");
    }
    text
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_update_from_another_accounts_home_leaves_the_project_as_it_is() {
    let (a_api, b_api) = (Stub::spawn().await, Stub::spawn().await);
    let world = World::new(&b_api.url);
    let before = world.seed_owner_entry(&a_api.url, KEY_A, &world.a_helper());

    let output = world.refresh(&[]).await;
    let text = said(&output);
    assert!(output.status.success(), "{text}");

    // A's entry keeps its key, installation, helper and gateway byte for
    // byte; the rest of the refresh still applies.
    let entry = entry_of(&world.project_config());
    let held = &before["mcpServers"]["contextstream"];
    for field in ["headersHelper", "url", "type"] {
        assert_eq!(entry[field], held[field], "{field}: {text}");
    }
    for header in [
        API_KEY_HEADER,
        INSTALLATION_HEADER,
        "X-ContextStream-Workspace-Id",
    ] {
        assert_eq!(
            entry["headers"][header], held["headers"][header],
            "{header}: {text}"
        );
    }
    assert_eq!(
        entry["headers"]["X-ContextStream-Project-Id"],
        json!(A_PROJECT),
        "the rest of the refresh still applies: {entry}"
    );
    assert!(
        text.contains(&format!(
            "{} signs in as owner@example.test; this would switch it to demo@example.test.",
            world.project_config().display()
        )),
        "{text}"
    );
    assert!(
        text.contains("Kept its key, installation id, binary and API URL unchanged."),
        "{text}"
    );
    assert!(text.contains("--switch-account"), "{text}");

    // No new project entry for B: the project signs in as A.
    assert!(!world.cursor_project_config().exists(), "{text}");
    assert!(
        text.contains(&format!(
            "creating {} would sign this project in as",
            world.cursor_project_config().display()
        )) && text.contains(&format!(
            "Did not create {}.",
            world.cursor_project_config().display()
        )),
        "{text}"
    );

    // A's key went only to A's API; B's API never saw it.
    assert!(a_api.keys_on("/api/v1/auth/me").contains(&"A"), "{text}");
    assert!(!b_api.any_key("A"), "A's key reached B's API");
    assert!(b_api.keys_on("/api/v1/auth/me").is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agreed_switch_changes_the_key_and_installation_but_keeps_the_binary() {
    let (a_api, b_api) = (Stub::spawn().await, Stub::spawn().await);
    let world = World::new(&b_api.url);
    world.seed_owner_entry(&a_api.url, KEY_A, &world.a_helper());

    let output = world.refresh(&["--switch-account"]).await;
    let text = said(&output);
    assert!(output.status.success(), "{text}");
    let entry = entry_of(&world.project_config());
    assert_eq!(entry["headers"][API_KEY_HEADER], json!(KEY_B), "{text}");
    assert_eq!(
        entry["headers"][INSTALLATION_HEADER],
        json!(B_INSTALLATION),
        "{text}"
    );
    assert_eq!(entry["headersHelper"], json!(world.a_helper()), "{text}");
    assert!(
        text.contains("from owner@example.test to demo@example.test"),
        "{text}"
    );
    assert_eq!(
        text.matches("from another home than this one").count(),
        1,
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "{} runs {}, from another home than this one; it was not pointed at {}.",
            world.project_config().display(),
            world.a_binary,
            world.b_binary.display()
        )),
        "{text}"
    );
    // With the project now signed in as B, B's Cursor entry is created.
    assert_eq!(
        entry_of(&world.cursor_project_config())["headers"][API_KEY_HEADER],
        json!(KEY_B)
    );
    assert!(!b_api.any_key("A"), "A's key reached B's API");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_entry_holding_a_key_reference_keeps_it() {
    let (a_api, b_api) = (Stub::spawn().await, Stub::spawn().await);
    let world = World::new(&b_api.url);
    world.seed_owner_entry(&a_api.url, "${CONTEXTSTREAM_API_KEY}", &world.a_helper());

    let output = world.refresh(&[]).await;
    let text = said(&output);
    assert!(output.status.success(), "{text}");
    let entry = entry_of(&world.project_config());
    assert_eq!(
        entry["headers"][API_KEY_HEADER],
        json!("${CONTEXTSTREAM_API_KEY}"),
        "{text}"
    );
    assert_eq!(
        entry["headers"][INSTALLATION_HEADER],
        json!(A_INSTALLATION),
        "{text}"
    );
    assert_eq!(entry["headersHelper"], json!(world.a_helper()), "{text}");
    assert!(
        !std::fs::read_to_string(world.project_config())
            .unwrap()
            .contains(KEY_B),
        "a literal key was written over the reference"
    );
    assert!(
        text.contains("signs in as an unknown account (the key ${CONTEXTSTREAM_API_KEY})"),
        "{text}"
    );
    assert!(!world.cursor_project_config().exists(), "{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn doctor_repair_without_the_flag_keeps_the_entry() {
    let (a_api, b_api) = (Stub::spawn().await, Stub::spawn().await);
    let world = World::new(&b_api.url);
    // A's entry in B's own Claude Code config (B's HOME once signed in as A).
    let before = world.seed_owner_entry(&a_api.url, KEY_A, &world.a_helper());
    let claude = world.b_home.join(".claude.json");
    std::fs::rename(world.project_config(), &claude).unwrap();

    let output = world
        .run_in(&world.b_home, &["doctor", "--repair"], &[])
        .await;
    let text = said(&output);
    let entry = entry_of(&claude);
    let held = &before["mcpServers"]["contextstream"];
    assert_eq!(
        entry["headers"][API_KEY_HEADER], held["headers"][API_KEY_HEADER],
        "{text}"
    );
    assert_eq!(
        entry["headers"][INSTALLATION_HEADER], held["headers"][INSTALLATION_HEADER],
        "{text}"
    );
    assert_eq!(entry["headersHelper"], held["headersHelper"], "{text}");
    assert!(
        text.contains(&format!(
            "{} signs in as owner@example.test; this would switch it to demo@example.test.",
            claude.display()
        )),
        "{text}"
    );
    assert!(text.contains("--switch-account"), "{text}");
    assert!(a_api.keys_on("/api/v1/auth/me").contains(&"A"), "{text}");
    assert!(!b_api.any_key("A"), "A's key reached B's API");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_same_account_refresh_keeps_a_bare_helper() {
    let (a_api, b_api) = (Stub::spawn().await, Stub::spawn().await);
    let world = World::new(&b_api.url);
    world.seed_owner_entry(&a_api.url, KEY_B_OLD, "contextstream-mcp hook mcp-headers");

    let output = world.refresh(&[]).await;
    let text = said(&output);
    assert!(output.status.success(), "{text}");
    let entry = entry_of(&world.project_config());
    assert_eq!(entry["headers"][API_KEY_HEADER], json!(KEY_B), "{text}");
    assert_eq!(
        entry["headersHelper"],
        json!("contextstream-mcp hook mcp-headers"),
        "{text}"
    );
    assert!(!text.contains("signs in as"), "{text}");
    assert!(!text.contains("from another home"), "{text}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kept_stdio_entry_keeps_its_api_url_command_and_transport() {
    let (a_api, b_api) = (Stub::spawn().await, Stub::spawn().await);
    let world = World::new(&b_api.url);
    let stdio = json!({"mcpServers": {"contextstream": {
        "command": world.a_binary,
        "args": [],
        "env": {
            "CONTEXTSTREAM_MANAGED_CONFIG_VERSION": "2",
            "CONTEXTSTREAM_API_KEY": KEY_A,
            "CONTEXTSTREAM_API_URL": a_api.url,
            "CONTEXTSTREAM_INSTALLATION_ID": A_INSTALLATION,
        },
    }}});

    // The project's stdio entry is not turned into a hosted one: it stays
    // byte for byte, and its key reaches no API (its entry and the hosted
    // one it would become talk to different APIs).
    let cursor = world.cursor_project_config();
    std::fs::create_dir_all(cursor.parent().unwrap()).unwrap();
    std::fs::write(&cursor, serde_json::to_string_pretty(&stdio).unwrap()).unwrap();
    let output = world.refresh(&[]).await;
    let text = said(&output);
    assert!(output.status.success(), "{text}");
    let raw = std::fs::read_to_string(&cursor).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&raw).unwrap(),
        stdio,
        "{text}"
    );
    assert!(!a_api.any_key("A") && !b_api.any_key("A"), "{text}");
    assert!(
        text.contains(&format!("signs in as an unknown account on {}", a_api.url)),
        "{text}"
    );

    // B's own Cursor config holding A's stdio entry, refreshed in place with
    // B's API in the environment: its API URL, command, key and installation
    // stay; the refreshed setting applies.
    a_api.clear();
    let global = world.b_home.join(".cursor").join("mcp.json");
    std::fs::write(&global, serde_json::to_string_pretty(&stdio).unwrap()).unwrap();
    let output = world
        .run(
            &[
                "configure",
                "--transcripts=off",
                "--scope=global",
                "--editors=cursor",
            ],
            &[("CONTEXTSTREAM_API_URL", &b_api.url)],
        )
        .await;
    let text = said(&output);
    assert!(output.status.success(), "{text}");
    let entry = entry_of(&global);
    let held = &stdio["mcpServers"]["contextstream"];
    assert_eq!(entry["command"], held["command"], "{text}");
    assert_eq!(entry["args"], held["args"], "{text}");
    for name in [
        "CONTEXTSTREAM_API_KEY",
        "CONTEXTSTREAM_API_URL",
        "CONTEXTSTREAM_INSTALLATION_ID",
    ] {
        assert_eq!(entry["env"][name], held["env"][name], "{name}: {text}");
    }
    assert_eq!(
        entry["env"]["CONTEXTSTREAM_TRANSCRIPTS_ENABLED"],
        json!("false"),
        "{text}"
    );
    assert!(!b_api.any_key("A"), "A's key reached B's API");
}

/// Run `args` in a pseudo-terminal, answer the first question that contains
/// `question` with `answer`, and return what the terminal showed. `None`
/// when this system offers no pseudo-terminal.
fn run_in_terminal(world: &World, args: &[&str], question: &str, answer: &[u8]) -> Option<String> {
    use std::io::{Read, Write};
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;

    let (mut master, mut slave) = (0, 0);
    // SAFETY: openpty writes two descriptors it opened; null name, termios
    // and window size are allowed.
    let opened = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    if opened != 0 {
        return None;
    }
    // SAFETY: both descriptors were just opened by openpty and are owned here.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    let mut command = Command::new(&world.b_binary);
    command
        .args(args)
        .env_clear()
        .env("HOME", &world.b_home)
        .env("XDG_CONFIG_HOME", world.b_home.join(".config"))
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("CONTEXTSTREAM_WATCH", "0")
        .env("CONTEXTSTREAM_NO_BROWSER", "1")
        .env("NO_COLOR", "1")
        .env("TERM", "xterm")
        .current_dir(&world.repo)
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave.try_clone().unwrap()));
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            libc::ioctl(0, libc::TIOCSCTTY as _, 0);
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    // Close every copy of the terminal's child side here, so the reader ends
    // when the child does.
    drop(command);
    drop(slave);

    let shown = Arc::new(Mutex::new(Vec::new()));
    let reader = {
        let shown = Arc::clone(&shown);
        let mut master = std::fs::File::from(master.try_clone().unwrap());
        std::thread::spawn(move || {
            let mut buffer = [0u8; 4096];
            // EIO once the child side closes ends the loop.
            while let Ok(read) = master.read(&mut buffer) {
                if read == 0 {
                    break;
                }
                shown.lock().unwrap().extend_from_slice(&buffer[..read]);
            }
        })
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(240);
    let mut answered = false;
    let mut writer = std::fs::File::from(master);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "{status}");
            break;
        }
        if !answered && String::from_utf8_lossy(&shown.lock().unwrap()).contains(question) {
            std::thread::sleep(std::time::Duration::from_millis(300));
            writer.write_all(answer).unwrap();
            answered = true;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the terminal run did not end"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    drop(writer);
    let ended = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !reader.is_finished() && std::time::Instant::now() < ended {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(answered, "the question never came");
    let shown = shown.lock().unwrap().clone();
    Some(String::from_utf8_lossy(&shown).into_owned())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_terminal_is_asked_and_the_default_keeps_the_entry() {
    let (a_api, b_api) = (Stub::spawn().await, Stub::spawn().await);
    let world = World::new(&b_api.url);
    let args = ["migrate-remote", "--scope=project", "--editors=claude"];

    // Enter takes the default, No.
    world.seed_owner_entry(&a_api.url, KEY_A, &world.a_helper());
    let world = Arc::new(world);
    let run = {
        let world = Arc::clone(&world);
        move |answer: &'static [u8]| {
            let world = Arc::clone(&world);
            tokio::task::spawn_blocking(move || {
                run_in_terminal(&world, &args, "Switch it to demo@example.test?", answer)
            })
        }
    };
    let Some(shown) = run(b"\r").await.unwrap() else {
        eprintln!("no pseudo-terminal here; skipping the terminal check");
        return;
    };
    for key in [KEY_A, KEY_B] {
        assert!(!shown.contains(key), "a key was shown");
    }
    assert!(
        shown
            .contains("signs in as owner@example.test; this would switch it to demo@example.test."),
        "{shown}"
    );
    let entry = entry_of(&world.project_config());
    assert_eq!(entry["headers"][API_KEY_HEADER], json!(KEY_A), "{shown}");
    assert_eq!(entry["headersHelper"], json!(world.a_helper()), "{shown}");

    // Up then Enter answers Yes: the key and installation switch, the binary
    // stays A's.
    let Some(shown) = run(b"\x1b[A\r").await.unwrap() else {
        return;
    };
    let entry = entry_of(&world.project_config());
    assert_eq!(entry["headers"][API_KEY_HEADER], json!(KEY_B), "{shown}");
    assert_eq!(
        entry["headers"][INSTALLATION_HEADER],
        json!(B_INSTALLATION),
        "{shown}"
    );
    assert_eq!(entry["headersHelper"], json!(world.a_helper()), "{shown}");
    assert_ne!(entry["headersHelper"], json!(world.b_helper()));
    assert!(!b_api.any_key("A"), "A's key reached B's API");
}
