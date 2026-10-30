//! `doctor` without `--repair` and `verify-key` only check. Building the API
//! client created ~/.contextstream/installation.json, so the first `doctor`
//! reported the identity missing (exit 1) and the next one passed (#341).

use std::path::Path;
use std::process::{Command, Output, Stdio};

fn run(home: &Path, args: &[&str]) -> Output {
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
        .env("CONTEXTSTREAM_API_KEY", "cs_test_check_commands")
        .env("CONTEXTSTREAM_API_URL", "http://127.0.0.1:9")
        .env("CONTEXTSTREAM_WATCH", "0")
        .current_dir(home)
        .stdin(Stdio::null())
        .output()
        .expect("run contextstream-mcp")
}

fn installation_state(home: &Path) -> Vec<String> {
    std::fs::read_dir(home.join(".contextstream"))
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.starts_with("installation"))
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn a_check_writes_no_installation_state_and_answers_the_same_twice() {
    for args in [
        &["doctor"][..],
        &["doctor", "--json"][..],
        &["verify-key"][..],
    ] {
        let home = tempfile::tempdir().expect("isolated home");
        let first = run(home.path(), args);
        assert_eq!(
            installation_state(home.path()),
            Vec::<String>::new(),
            "{args:?}"
        );
        let second = run(home.path(), args);
        assert_eq!(first.status.code(), second.status.code(), "{args:?}");
        assert_eq!(
            installation_state(home.path()),
            Vec::<String>::new(),
            "{args:?}"
        );
    }
}

#[test]
fn doctor_json_reports_the_identity_the_same_way_on_every_run() {
    let home = tempfile::tempdir().expect("isolated home");
    let installation = |output: Output| {
        let report: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("doctor --json prints JSON");
        report["installation"]["status"].clone()
    };
    let first = installation(run(home.path(), &["doctor", "--json"]));
    let second = installation(run(home.path(), &["doctor", "--json"]));
    assert_eq!(first, "fail");
    assert_eq!(first, second);
}
