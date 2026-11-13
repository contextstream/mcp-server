//! `contextstream-mcp hook mcp-headers` as Claude Code runs it: the built
//! binary, started by a shell line, with nothing on its standard input that
//! ever ends.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const HEADER: &str = "X-ContextStream-Client-Session";

struct Ran {
    stdout: String,
    stderr: String,
    success: bool,
    took: Duration,
}

/// Run the command with `env` in a clean environment whose home is `home`.
/// Its standard input is a pipe that stays open, as a host may leave it.
fn run(home: &std::path::Path, env: &[(&str, &str)]) -> Ran {
    let mut command = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"));
    command
        .args(["hook", "mcp-headers"])
        .env_clear()
        .env("HOME", home)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in env {
        command.env(name, value);
    }
    let started = Instant::now();
    let mut child = command.spawn().expect("the binary starts");
    let held_open = child.stdin.take();
    let mut stdout = String::new();
    child
        .stdout
        .take()
        .expect("stdout")
        .read_to_string(&mut stdout)
        .expect("read stdout");
    let status = child.wait().expect("the command ends");
    let took = started.elapsed();
    let mut stderr = String::new();
    child
        .stderr
        .take()
        .expect("stderr")
        .read_to_string(&mut stderr)
        .expect("read stderr");
    drop(held_open);
    Ran {
        stdout,
        stderr,
        success: status.success(),
        took,
    }
}

fn token_of(ran: &Ran) -> String {
    let body: serde_json::Value =
        serde_json::from_str(ran.stdout.trim()).unwrap_or_else(|_| panic!("{:?}", ran.stdout));
    let fields = body.as_object().expect("one JSON object");
    assert_eq!(fields.len(), 1, "{body}");
    let token = fields[HEADER].as_str().expect("a string").to_string();
    assert_eq!(token.len(), 32, "{token}");
    assert!(
        token
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
        "{token}"
    );
    token
}

#[test]
fn it_prints_one_header_with_a_random_id_and_does_not_wait_on_its_input() {
    let home = tempfile::tempdir().expect("a home");
    let first = run(home.path(), &[]);
    assert!(first.success, "{}", first.stderr);
    assert_eq!(first.stderr, "");
    assert!(first.took < Duration::from_millis(1900), "{:?}", first.took);
    let second = run(home.path(), &[]);
    // Nothing names the session's process: every connect has its own id.
    assert_ne!(token_of(&first), token_of(&second));
    // And it kept nothing on disk for it.
    assert!(!home.path().join(".contextstream").exists());
}

#[test]
fn it_sends_no_id_where_live_progress_is_turned_off() {
    let home = tempfile::tempdir().expect("a home");
    let ran = run(home.path(), &[("CONTEXTSTREAM_RUN_PROGRESS", "off")]);
    assert!(ran.success, "{}", ran.stderr);
    assert_eq!(ran.stdout.trim(), "{}");
    // The hooks' own switch is another matter: the id is still sent.
    let ran = run(home.path(), &[("CONTEXTSTREAM_HOOK_ENABLED", "off")]);
    token_of(&ran);
}

#[test]
fn held_back_it_ends_itself_at_its_deadline_without_a_word() {
    let home = tempfile::tempdir().expect("a home");
    let ran = run(
        home.path(),
        &[("CONTEXTSTREAM_MCP_HEADERS_DELAY_MS", "30000")],
    );
    assert!(ran.success);
    assert_eq!(ran.stdout, "");
    assert_eq!(ran.stderr, "");
    assert!(ran.took >= Duration::from_millis(1900), "{:?}", ran.took);
    assert!(ran.took < Duration::from_millis(6000), "{:?}", ran.took);
}

#[cfg(unix)]
#[test]
fn a_reconnect_of_the_same_session_process_sends_the_same_id() {
    let home = tempfile::tempdir().expect("a home");
    let sockets = tempfile::tempdir().expect("a folder");
    let one = sockets.path().join("4242.sock");
    let other = sockets.path().join("4243.sock");
    let _one = std::os::unix::net::UnixListener::bind(&one).expect("a socket");
    let _other = std::os::unix::net::UnixListener::bind(&other).expect("a socket");
    let socket = |path: &std::path::Path| path.to_string_lossy().into_owned();

    let first = token_of(&run(
        home.path(),
        &[("CLAUDE_CODE_MESSAGING_SOCKET", &socket(&one))],
    ));
    let again = token_of(&run(
        home.path(),
        &[("CLAUDE_CODE_MESSAGING_SOCKET", &socket(&one))],
    ));
    assert_eq!(first, again);
    let another_session = token_of(&run(
        home.path(),
        &[("CLAUDE_CODE_MESSAGING_SOCKET", &socket(&other))],
    ));
    assert_ne!(first, another_session);
    // A socket that is not there names nothing: a new id, and no error.
    let gone = run(
        home.path(),
        &[(
            "CLAUDE_CODE_MESSAGING_SOCKET",
            &socket(&sockets.path().join("none.sock")),
        )],
    );
    assert_eq!(gone.stderr, "");
    assert_ne!(token_of(&gone), first);

    // What is kept: one small file per session process, holding the id only.
    let kept = home.path().join(".contextstream").join("mcp-connections");
    let files: Vec<_> = std::fs::read_dir(&kept)
        .expect("the folder")
        .flatten()
        .collect();
    assert_eq!(files.len(), 2);
    for file in files {
        let name = file.file_name().to_string_lossy().into_owned();
        assert!(!name.contains("4242") && !name.contains("sock"), "{name}");
        let text = std::fs::read_to_string(file.path()).expect("read");
        assert_eq!(text.len(), 32, "{text}");
    }
}
