//! A reader that closes stdout early (`contextstream-mcp about | head -2`)
//! must not make a one-shot command panic and abort with exit 134 (#159).
#![cfg(unix)]

use std::os::unix::process::ExitStatusExt;
use std::process::{Command, ExitStatus, Stdio};

/// Runs the binary with a stdout whose reader is already gone, so its first
/// write meets a broken pipe: deterministic, and before any network call.
fn run_with_closed_stdout(args: &[&str]) -> (ExitStatus, String) {
    let home = tempfile::tempdir().expect("isolated home");
    let (reader, writer) = std::io::pipe().expect("pipe");
    drop(reader);
    let mut command = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("CONTEXTSTREAM_") {
            command.env_remove(key);
        }
    }
    let output = command
        .args(args)
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        .env("CONTEXTSTREAM_API_URL", "http://127.0.0.1:9")
        .env("CONTEXTSTREAM_WATCH", "0")
        .stdin(Stdio::null())
        .stdout(writer)
        .stderr(Stdio::piped())
        .output()
        .expect("run contextstream-mcp");
    (
        output.status,
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

fn assert_no_panic(args: &[&str], status: ExitStatus, stderr: &str) {
    assert!(
        !stderr.contains("panicked"),
        "{args:?} must not panic on a closed stdout: {status:?}\n{stderr}"
    );
    assert_ne!(
        status.signal(),
        Some(libc::SIGABRT),
        "{args:?} aborted: {stderr}"
    );
}

#[test]
fn about_ends_on_sigpipe_instead_of_panicking() {
    let args = ["about"];
    let (status, stderr) = run_with_closed_stdout(&args);
    assert_no_panic(&args, status, &stderr);
    assert_eq!(
        status.signal(),
        Some(libc::SIGPIPE),
        "a closed stdout ends the command the way it ends other CLIs: {status:?}\n{stderr}"
    );
}

#[test]
fn setup_and_doctor_do_not_panic_on_a_closed_stdout() {
    for args in [
        &["setup", "--yes", "--dry-run", "--editors=claude"][..],
        &["doctor", "--editors=claude"][..],
        &["clients", "--format=markdown"][..],
    ] {
        let (status, stderr) = run_with_closed_stdout(args);
        assert_no_panic(args, status, &stderr);
    }
}
