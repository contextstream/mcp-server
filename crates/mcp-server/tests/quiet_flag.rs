//! `-q` / `--quiet` is documented as "Quiet mode (errors only)": one-shot
//! commands print nothing on stdout, and keep their errors and exit status
//! (#255).

use std::io::Write;
use std::process::{Command, Output, Stdio};

fn run(args: &[&str], stdin: &str) -> Output {
    let home = tempfile::tempdir().expect("isolated home");
    let mut command = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("CONTEXTSTREAM_") {
            command.env_remove(key);
        }
    }
    let mut child = command
        .args(args)
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        .env("CONTEXTSTREAM_API_URL", "http://127.0.0.1:9")
        .env("CONTEXTSTREAM_WATCH", "0")
        .current_dir(home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run contextstream-mcp");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(stdin.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("wait")
}

#[test]
fn quiet_silences_stdout_and_keeps_the_exit_status() {
    let loud = run(&["about"], "");
    assert!(loud.status.success());
    assert!(!loud.stdout.is_empty(), "about prints without -q");

    for flag in ["-q", "--quiet"] {
        let quiet = run(&[flag, "about"], "");
        assert!(quiet.status.success(), "{flag}: {:?}", quiet.status);
        assert!(
            quiet.stdout.is_empty(),
            "{flag} about printed: {}",
            String::from_utf8_lossy(&quiet.stdout)
        );
    }

    let loud = run(&["setup", "--yes", "--dry-run", "--editors=claude"], "");
    let quiet = run(
        &["-q", "setup", "--yes", "--dry-run", "--editors=claude"],
        "",
    );
    assert!(!loud.stdout.is_empty());
    assert!(
        quiet.stdout.is_empty(),
        "-q setup printed: {}",
        String::from_utf8_lossy(&quiet.stdout)
    );
    assert_eq!(quiet.status.code(), loud.status.code());
}

#[test]
fn quiet_silences_progress_on_stderr_and_keeps_requested_data_on_stdout() {
    // update-rules reports each editor on stderr; -q drops it.
    let loud = run(
        &["update-rules", "--scope", "global", "--editors", "claude"],
        "",
    );
    let quiet = run(
        &[
            "-q",
            "update-rules",
            "--scope",
            "global",
            "--editors",
            "claude",
        ],
        "",
    );
    assert_eq!(quiet.status.code(), loud.status.code());
    assert!(!loud.stderr.is_empty(), "update-rules reports progress");
    assert!(
        quiet.stderr.is_empty(),
        "-q update-rules printed: {}",
        String::from_utf8_lossy(&quiet.stderr)
    );

    // Output the command was asked for is not discarded.
    let clients = run(&["-q", "clients"], "");
    assert!(clients.status.success());
    let parsed: serde_json::Value =
        serde_json::from_slice(&clients.stdout).expect("clients JSON on stdout under -q");
    assert!(parsed.is_object() || parsed.is_array());
}

#[test]
fn quiet_keeps_errors_on_stderr() {
    let quiet = run(&["-q", "configure", "--api-key-stdin"], "\n");
    assert!(!quiet.status.success(), "an empty key must fail");
    assert!(quiet.stdout.is_empty());
    assert!(
        !quiet.stderr.is_empty(),
        "the error stays visible on stderr under -q"
    );
}
