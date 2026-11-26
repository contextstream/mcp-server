//! Regression test for ticket e69fbc52: `contextstream-mcp update`, run with
//! another account's HOME inside a repository, rewrote that repository's
//! project `.mcp.json` with the HOME account's API key and installation id,
//! and pointed its `headersHelper` at the HOME's own binary, without a word.
//! Every call from the repository then ran as the wrong account, through
//! another person's binary.
//!
//! `update` refreshes MCP configs by running `migrate-remote --scope=all
//! --only-configured` (with `--switch-account` handed on when given). These
//! tests run that refresh for the project entry from a HOME signed in as
//! Chase, against an `/auth/me` stub, with no terminal:
//! - an entry that signs in as Erik keeps Erik's key, installation id and
//!   `headersHelper` line byte for byte, the rest of the refresh still
//!   applies, and stderr names both accounts by email and says how to switch
//!   on purpose (never a key);
//! - `--switch-account` switches it, helper line included, and says so;
//! - a key `/auth/me` refuses is "an unknown account" and is kept too;
//! - a new key for the same account is written without an account question,
//!   but a helper from another home than Chase's stays, with a warning naming
//!   both binaries; an entry with no key is written as before.
//!
//! A local (stdio) entry is refreshed by `configure --transcripts`, which
//! keeps it local: its command is kept byte for byte without
//! `--switch-account` and repointed with it.
#![cfg(unix)]

use axum::{
    http::{HeaderMap, StatusCode},
    routing::get,
    Json, Router,
};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

const CHASE_KEY: &str = "cs_test_chase_home_key_1111";
const CHASE_OLD_KEY: &str = "cs_test_chase_older_key_2222";
const ERIK_KEY: &str = "cs_test_erik_project_key_3333";
const REVOKED_KEY: &str = "cs_test_revoked_project_key_4444";
const ERIK_INSTALLATION: &str = "eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee";
const API_KEY_HEADER: &str = "X-ContextStream-API-Key";
const INSTALLATION_HEADER: &str = "X-ContextStream-Installation-Id";

async fn spawn_auth_me_stub() -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_url = format!("http://{}", listener.local_addr().unwrap());
    let app = Router::new().route(
        "/api/v1/auth/me",
        get(|headers: HeaderMap| async move {
            let email = match headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
                Some(CHASE_KEY) | Some(CHASE_OLD_KEY) => "chase@example.test",
                Some(ERIK_KEY) => "erik@example.test",
                _ => {
                    return (
                        StatusCode::UNAUTHORIZED,
                        Json(json!({"error": "invalid or revoked api key"})),
                    )
                }
            };
            (
                StatusCode::OK,
                Json(json!({"data": {
                    "id": "22222222-2222-4222-8222-222222222222",
                    "email": email,
                    "created_at": "2026-01-01T00:00:00Z"
                }})),
            )
        }),
    );
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (api_url, handle)
}

/// Chase's managed binary (`<home>/.contextstream/bin/contextstream-mcp`):
/// the CLI runs from there, as an installed one does, so a refresh names it
/// and never needs to install it.
fn install_managed_binary(home: &Path) -> PathBuf {
    let bin_dir = home.join(".contextstream").join("bin");
    std::fs::create_dir_all(&bin_dir).unwrap();
    let dest = bin_dir.join("contextstream-mcp");
    let src = PathBuf::from(env!("CARGO_BIN_EXE_contextstream-mcp"));
    if std::fs::hard_link(&src, &dest).is_err() {
        // A copy made by a child process, so no descriptor writing the binary
        // is open here when another test forks (ETXTBSY).
        let status = Command::new("cp").arg(&src).arg(&dest).status().unwrap();
        assert!(status.success(), "cp {} {}", src.display(), dest.display());
    }
    dest
}

/// A project `.mcp.json` whose hosted ContextStream entry holds `api_key`
/// (none when `None`), names Erik's installation and, when given, runs
/// `helper` as its `headersHelper`.
fn seed_project_entry(project: &Path, api_key: Option<&str>, helper: Option<&str>) -> PathBuf {
    let path = project.join(".mcp.json");
    let mut headers = json!({
        "X-ContextStream-Managed-Config-Version": "2",
        INSTALLATION_HEADER: ERIK_INSTALLATION,
        "X-ContextStream-Workspace-Id": "33333333-3333-4333-8333-333333333333",
    });
    if let Some(api_key) = api_key {
        headers[API_KEY_HEADER] = json!(api_key);
    }
    let mut entry = json!({
        "type": "http",
        "url": "https://mcp.contextstream.io/mcp?default_context_mode=fast",
        "headers": headers,
    });
    if let Some(helper) = helper {
        entry["headersHelper"] = json!(helper);
    }
    let config = json!({"mcpServers": {"contextstream": entry}});
    std::fs::write(&path, serde_json::to_string_pretty(&config).unwrap()).unwrap();
    path
}

fn entry_of(path: &Path) -> Value {
    let config: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    config["mcpServers"]["contextstream"].clone()
}

/// Run the CLI from Chase's managed binary, in Chase's HOME, in `cwd`, with no
/// terminal.
async fn run_cli(
    binary: &Path,
    home: &Path,
    cwd: &Path,
    api_url: &str,
    args: Vec<String>,
) -> Output {
    let (binary, home, cwd, api_url) = (
        binary.to_path_buf(),
        home.to_path_buf(),
        cwd.to_path_buf(),
        api_url.to_string(),
    );
    tokio::task::spawn_blocking(move || {
        Command::new(&binary)
            .args(&args)
            .env_clear()
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("CONTEXTSTREAM_API_KEY", CHASE_KEY)
            .env("CONTEXTSTREAM_API_URL", &api_url)
            .env("CONTEXTSTREAM_WATCH_ENABLED", "false")
            .env("CONTEXTSTREAM_NO_BROWSER", "1")
            .env("NO_COLOR", "1")
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    })
    .await
    .unwrap()
}

/// The config refresh `update` runs, for the project entry only.
async fn refresh_project(
    binary: &Path,
    home: &Path,
    project: &Path,
    api_url: &str,
    extra: &[&str],
) -> Output {
    let mut args = vec![
        "migrate-remote".to_string(),
        "--scope=project".to_string(),
        "--editors=claude".to_string(),
    ];
    args.extend(extra.iter().map(|arg| arg.to_string()));
    run_cli(binary, home, project, api_url, args).await
}

fn said(output: &Output) -> String {
    format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn assert_no_key_printed(output: &str) {
    for key in [CHASE_KEY, CHASE_OLD_KEY, ERIK_KEY, REVOKED_KEY] {
        assert!(!output.contains(key), "a key was printed:\n{output}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refresh_from_another_accounts_home_asks_before_switching_a_project_entry() {
    let (api_url, server) = spawn_auth_me_stub().await;
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("chase-home");
    let project = temp.path().join("erik-repo");
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    // The CLI names the folder it runs in as the OS reports it.
    let project = project.canonicalize().unwrap();
    let chase_binary = install_managed_binary(&home);
    let chase_helper = format!("{} hook mcp-headers", chase_binary.display());
    let erik_binary = temp
        .path()
        .join("erik-home")
        .join(".contextstream")
        .join("bin")
        .join("contextstream-mcp")
        .display()
        .to_string();
    let erik_helper = format!("{erik_binary} hook mcp-headers");

    // 1. No terminal, no flag: Erik's key, installation and helper line stay
    //    byte for byte, the rest of the refresh applies, and the warning
    //    names both accounts.
    let path = seed_project_entry(&project, Some(ERIK_KEY), Some(&erik_helper));
    let output = refresh_project(&chase_binary, &home, &project, &api_url, &[]).await;
    let text = said(&output);
    assert!(output.status.success(), "{text}");
    let entry = entry_of(&path);
    assert_eq!(entry["headers"][API_KEY_HEADER], json!(ERIK_KEY), "{text}");
    assert_eq!(
        entry["headers"][INSTALLATION_HEADER],
        json!(ERIK_INSTALLATION),
        "{text}"
    );
    assert_eq!(entry["headersHelper"], json!(erik_helper), "{text}");
    assert!(
        entry["headers"]
            .get("X-ContextStream-Teaching-Version")
            .is_some(),
        "the rest of the refresh still applies: {entry}"
    );
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(!raw.contains(CHASE_KEY), "{raw}");
    assert!(!raw.contains(&chase_binary.display().to_string()), "{raw}");
    assert!(
        text.contains(&format!(
            "{} signs in as erik@example.test; this would switch it to chase@example.test.",
            path.display()
        )),
        "{text}"
    );
    assert!(
        text.contains("Kept its key and installation id unchanged."),
        "{text}"
    );
    assert!(text.contains("--switch-account"), "{text}");
    assert_no_key_printed(&text);

    // 2. The explicit opt-in switches it, helper line included, and says so.
    let output = refresh_project(
        &chase_binary,
        &home,
        &project,
        &api_url,
        &["--switch-account"],
    )
    .await;
    let text = said(&output);
    assert!(output.status.success(), "{text}");
    let entry = entry_of(&path);
    assert_eq!(entry["headers"][API_KEY_HEADER], json!(CHASE_KEY), "{text}");
    assert_ne!(
        entry["headers"][INSTALLATION_HEADER],
        json!(ERIK_INSTALLATION),
        "{text}"
    );
    assert_eq!(entry["headersHelper"], json!(chase_helper), "{text}");
    assert!(
        text.contains("from erik@example.test to chase@example.test"),
        "{text}"
    );
    assert_no_key_printed(&text);

    // 3. A key `/auth/me` refuses is an unknown account: still a switch, so
    //    the key and the helper line are kept.
    let path = seed_project_entry(&project, Some(REVOKED_KEY), Some(&erik_helper));
    let output = refresh_project(&chase_binary, &home, &project, &api_url, &[]).await;
    let text = said(&output);
    assert!(output.status.success(), "{text}");
    let entry = entry_of(&path);
    assert_eq!(
        entry["headers"][API_KEY_HEADER],
        json!(REVOKED_KEY),
        "{text}"
    );
    assert_eq!(entry["headersHelper"], json!(erik_helper), "{text}");
    assert!(
        text.contains("signs in as an unknown account; this would switch it to chase@example.test"),
        "{text}"
    );
    assert_no_key_printed(&text);

    // 4. A new key for the same account is written without an account
    //    question, but a helper from another home than Chase's stays, with
    //    one warning naming both binaries.
    let path = seed_project_entry(&project, Some(CHASE_OLD_KEY), Some(&erik_helper));
    let output = refresh_project(&chase_binary, &home, &project, &api_url, &[]).await;
    let text = said(&output);
    assert!(output.status.success(), "{text}");
    let entry = entry_of(&path);
    assert_eq!(entry["headers"][API_KEY_HEADER], json!(CHASE_KEY), "{text}");
    assert_eq!(entry["headersHelper"], json!(erik_helper), "{text}");
    assert!(!text.contains("signs in as"), "{text}");
    assert_eq!(
        text.matches("from another home than this one").count(),
        1,
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "{} runs {erik_binary}, from another home than this one; it was not pointed at {}.",
            path.display(),
            chase_binary.display()
        )),
        "{text}"
    );
    assert_no_key_printed(&text);

    // 5. An entry with no key (and no helper) is written as before.
    let path = seed_project_entry(&project, None, None);
    let output = refresh_project(&chase_binary, &home, &project, &api_url, &[]).await;
    let text = said(&output);
    assert!(output.status.success(), "{text}");
    let entry = entry_of(&path);
    assert_eq!(entry["headers"][API_KEY_HEADER], json!(CHASE_KEY), "{text}");
    assert_eq!(entry["headersHelper"], json!(chase_helper), "{text}");
    assert!(!text.contains("signs in as"), "{text}");
    assert!(!text.contains("from another home"), "{text}");

    // 6. A local (stdio) entry in Chase's Cursor config that runs Erik's binary
    //    with Erik's key: `configure --transcripts` keeps it local. Without
    //    the flag its command, arguments, key and installation stay byte for
    //    byte; with it, they move to Chase.
    let cursor = home.join(".cursor").join("mcp.json");
    let stdio_entry = json!({
        "command": erik_binary,
        "args": [],
        "env": {
            "CONTEXTSTREAM_MANAGED_CONFIG_VERSION": "2",
            "CONTEXTSTREAM_API_KEY": ERIK_KEY,
            "CONTEXTSTREAM_INSTALLATION_ID": ERIK_INSTALLATION,
        },
    });
    std::fs::create_dir_all(cursor.parent().unwrap()).unwrap();
    std::fs::write(
        &cursor,
        serde_json::to_string_pretty(&json!({"mcpServers": {"contextstream": stdio_entry}}))
            .unwrap(),
    )
    .unwrap();
    let configure = |extra: &[&str]| {
        let mut args: Vec<String> = [
            "configure",
            "--transcripts=on",
            "--scope=global",
            "--editors=cursor",
        ]
        .iter()
        .map(|arg| arg.to_string())
        .collect();
        args.extend(extra.iter().map(|arg| arg.to_string()));
        run_cli(&chase_binary, &home, &home, &api_url, args)
    };
    let output = configure(&[]).await;
    let text = said(&output);
    assert!(output.status.success(), "{text}");
    let entry = entry_of(&cursor);
    assert_eq!(entry["command"], stdio_entry["command"], "{text}");
    assert_eq!(entry["args"], stdio_entry["args"], "{text}");
    assert_eq!(
        entry["env"]["CONTEXTSTREAM_API_KEY"],
        json!(ERIK_KEY),
        "{text}"
    );
    assert_eq!(
        entry["env"]["CONTEXTSTREAM_INSTALLATION_ID"],
        json!(ERIK_INSTALLATION),
        "{text}"
    );
    assert_eq!(
        entry["env"]["CONTEXTSTREAM_TRANSCRIPTS_ENABLED"],
        json!("true"),
        "the rest of the refresh still applies: {entry}"
    );
    assert!(
        text.contains(&format!(
            "{} signs in as erik@example.test; this would switch it to chase@example.test.",
            cursor.display()
        )),
        "{text}"
    );
    assert_no_key_printed(&text);

    let output = configure(&["--switch-account"]).await;
    let text = said(&output);
    assert!(output.status.success(), "{text}");
    let entry = entry_of(&cursor);
    assert_eq!(
        entry["command"],
        json!(chase_binary.display().to_string()),
        "{text}"
    );
    assert_eq!(
        entry["env"]["CONTEXTSTREAM_API_KEY"],
        json!(CHASE_KEY),
        "{text}"
    );
    assert_no_key_printed(&text);

    server.abort();
}
