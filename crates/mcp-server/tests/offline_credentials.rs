//! An unreachable API is not an invalid key: `configure --api-key-stdin` and
//! `setup --yes` say ContextStream did not answer instead of blaming the key
//! (#248).

use std::io::Write;
use std::process::{Command, Output, Stdio};

fn run_offline(args: &[&str], stdin: &str, env: &[(&str, &str)]) -> (Output, tempfile::TempDir) {
    let home = tempfile::tempdir().expect("isolated home");
    let mut command = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("CONTEXTSTREAM_") {
            command.env_remove(key);
        }
    }
    command
        .args(args)
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        // Nothing listens on the discard port: the connection is refused.
        .env("CONTEXTSTREAM_API_URL", "http://127.0.0.1:9")
        .env("CONTEXTSTREAM_WATCH", "0")
        .current_dir(home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = command.spawn().expect("run contextstream-mcp");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin.as_bytes())
        .expect("write stdin");
    (child.wait_with_output().expect("wait"), home)
}

fn assert_blames_the_connection(what: &str, output: &Output) {
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{what} must fail offline");
    assert!(
        stderr.contains("did not answer") || stderr.contains("timed out"),
        "{what} must say ContextStream was unreachable: {stderr}"
    );
    assert!(
        !stderr.contains("not accepted") && !stderr.contains("are invalid"),
        "{what} must not blame the key when offline: {stderr}"
    );
}

#[test]
fn configure_offline_does_not_call_the_key_rejected() {
    let (output, home) = run_offline(
        &["configure", "--api-key-stdin"],
        "cs_test_offline_key\n",
        &[],
    );
    assert_blames_the_connection("configure --api-key-stdin", &output);
    assert!(
        !home.path().join(".contextstream/credentials.json").exists(),
        "nothing is saved"
    );
}

#[test]
fn setup_yes_offline_does_not_call_saved_credentials_invalid() {
    let (output, _home) = run_offline(
        &["setup", "--yes", "--editors=claude"],
        "",
        &[("CONTEXTSTREAM_API_KEY", "cs_test_offline_key")],
    );
    assert_blames_the_connection("setup --yes", &output);
}
