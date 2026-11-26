//! Regression test for ticket e69fbc52: `contextstream-mcp update`, run with
//! another account's HOME inside a repository, rewrote that repository's
//! project `.mcp.json` with the HOME account's API key and installation id,
//! without a word, and every call from the repository then ran as the wrong
//! account.
//!
//! `update` refreshes MCP configs by running `migrate-remote --scope=all
//! --only-configured` (with `--switch-account` handed on when given). These
//! tests run that refresh for the project entry from a HOME signed in as
//! Chase, against an `/auth/me` stub, with no terminal:
//! - an entry that signs in as Erik keeps Erik's key and installation id, the
//!   rest of the refresh still applies, and stderr names both accounts by
//!   email and says how to switch on purpose (never a key);
//! - `--switch-account` switches it, and says so;
//! - a key `/auth/me` refuses is "an unknown account" and is kept too;
//! - a new key for the same account, and an entry with no key, are written
//!   without a word, as before.
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

/// A project `.mcp.json` whose hosted ContextStream entry holds `api_key`
/// (none when `None`) and names Erik's installation.
fn seed_project_entry(project: &Path, api_key: Option<&str>) -> PathBuf {
    let path = project.join(".mcp.json");
    let mut headers = json!({
        "X-ContextStream-Managed-Config-Version": "2",
        INSTALLATION_HEADER: ERIK_INSTALLATION,
        "X-ContextStream-Workspace-Id": "33333333-3333-4333-8333-333333333333",
    });
    if let Some(api_key) = api_key {
        headers[API_KEY_HEADER] = json!(api_key);
    }
    let config = json!({"mcpServers": {"contextstream": {
        "type": "http",
        "url": "https://mcp.contextstream.io/mcp?default_context_mode=fast",
        "headers": headers,
    }}});
    std::fs::write(&path, serde_json::to_string_pretty(&config).unwrap()).unwrap();
    path
}

fn entry_headers(path: &Path) -> Value {
    let config: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    config["mcpServers"]["contextstream"]["headers"].clone()
}

/// The config refresh `update` runs, for the project entry only, from
/// Chase's HOME, with no terminal.
async fn refresh_project(home: &Path, project: &Path, api_url: &str, extra: &[&str]) -> Output {
    let mut args = vec![
        "migrate-remote".to_string(),
        "--scope=project".to_string(),
        "--editors=claude".to_string(),
    ];
    args.extend(extra.iter().map(|arg| arg.to_string()));
    let (home, project, api_url) = (
        home.to_path_buf(),
        project.to_path_buf(),
        api_url.to_string(),
    );
    tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
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
            .current_dir(&project)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    })
    .await
    .unwrap()
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

    // 1. No terminal, no flag: Erik's key and installation stay, the rest of
    //    the refresh applies, and the warning names both accounts.
    let path = seed_project_entry(&project, Some(ERIK_KEY));
    let output = refresh_project(&home, &project, &api_url, &[]).await;
    let text = said(&output);
    assert!(output.status.success(), "{text}");
    let headers = entry_headers(&path);
    assert_eq!(headers[API_KEY_HEADER], json!(ERIK_KEY), "{text}");
    assert_eq!(
        headers[INSTALLATION_HEADER],
        json!(ERIK_INSTALLATION),
        "{text}"
    );
    assert!(
        headers.get("X-ContextStream-Teaching-Version").is_some(),
        "the rest of the refresh still applies: {headers}"
    );
    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(!raw.contains(CHASE_KEY), "{raw}");
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

    // 2. The explicit opt-in switches it, and says so.
    let output = refresh_project(&home, &project, &api_url, &["--switch-account"]).await;
    let text = said(&output);
    assert!(output.status.success(), "{text}");
    let headers = entry_headers(&path);
    assert_eq!(headers[API_KEY_HEADER], json!(CHASE_KEY), "{text}");
    assert_ne!(
        headers[INSTALLATION_HEADER],
        json!(ERIK_INSTALLATION),
        "{text}"
    );
    assert!(
        text.contains("from erik@example.test to chase@example.test"),
        "{text}"
    );
    assert_no_key_printed(&text);

    // 3. A key `/auth/me` refuses is an unknown account: still a switch, so
    //    it is kept.
    let path = seed_project_entry(&project, Some(REVOKED_KEY));
    let output = refresh_project(&home, &project, &api_url, &[]).await;
    let text = said(&output);
    assert!(output.status.success(), "{text}");
    assert_eq!(
        entry_headers(&path)[API_KEY_HEADER],
        json!(REVOKED_KEY),
        "{text}"
    );
    assert!(
        text.contains("signs in as an unknown account; this would switch it to chase@example.test"),
        "{text}"
    );
    assert_no_key_printed(&text);

    // 4. A new key for the same account is written without a word.
    let path = seed_project_entry(&project, Some(CHASE_OLD_KEY));
    let output = refresh_project(&home, &project, &api_url, &[]).await;
    let text = said(&output);
    assert!(output.status.success(), "{text}");
    assert_eq!(
        entry_headers(&path)[API_KEY_HEADER],
        json!(CHASE_KEY),
        "{text}"
    );
    assert!(!text.contains("signs in as"), "{text}");

    // 5. So is an entry with no key yet.
    let path = seed_project_entry(&project, None);
    let output = refresh_project(&home, &project, &api_url, &[]).await;
    let text = said(&output);
    assert!(output.status.success(), "{text}");
    assert_eq!(
        entry_headers(&path)[API_KEY_HEADER],
        json!(CHASE_KEY),
        "{text}"
    );
    assert!(!text.contains("signs in as"), "{text}");

    server.abort();
}
