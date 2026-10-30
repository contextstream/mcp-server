//! With ContextStream unreachable, `link` and `configure --account-learning
//! status` say where they tried and why it failed, and `link --json` still
//! prints JSON. They printed reqwest's bare text, or nothing (#343).

use std::path::Path;
use std::process::{Command, Output, Stdio};

/// Nothing listens on the discard port, as in quiet_flag.rs.
const REFUSED_URL: &str = "http://127.0.0.1:9";

fn run(home: &Path, api_url: &str, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("CONTEXTSTREAM_") {
            command.env_remove(key);
        }
    }
    command
        .args(args)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("CONTEXTSTREAM_API_KEY", "cs_test_unreachable")
        .env("CONTEXTSTREAM_API_URL", api_url)
        .env("CONTEXTSTREAM_WATCH", "0")
        .current_dir(home)
        .stdin(Stdio::null())
        .output()
        .expect("run contextstream-mcp")
}

#[test]
fn an_unreachable_server_is_named_with_its_cause() {
    let home = tempfile::tempdir().expect("home");
    let url = REFUSED_URL;
    let expected = format!("Could not reach ContextStream at {url}");

    let link = run(home.path(), url, &["link"]);
    assert_eq!(link.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&link.stderr);
    assert!(stderr.contains(&expected), "{stderr}");
    assert!(stderr.contains("CONTEXTSTREAM_API_URL"), "{stderr}");

    let json = run(home.path(), url, &["link", "--json"]);
    assert_eq!(json.status.code(), Some(1));
    let report: serde_json::Value =
        serde_json::from_slice(&json.stdout).expect("link --json prints JSON when it fails");
    assert!(
        report["error"]
            .as_str()
            .unwrap_or_default()
            .contains(&expected),
        "{report}"
    );

    let status = run(
        home.path(),
        url,
        &["configure", "--account-learning", "status"],
    );
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(
        stdout.contains("Deep project learning: unavailable"),
        "{stdout}"
    );
    assert!(stdout.contains(&expected), "{stdout}");
}
