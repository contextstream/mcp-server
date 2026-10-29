//! An empty credential variable is no credential (#311). An editor config
//! that names an unset variable (`"CONTEXTSTREAM_API_KEY": "${CONTEXTSTREAM_API_KEY}"`)
//! sends an empty one; the server must start in limited mode and point at
//! setup, not run as if a key were configured and fail every call with a 401.

use std::{
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[test]
fn an_empty_api_key_starts_the_stdio_server_in_limited_mode() {
    let temp = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
        .env("HOME", temp.path())
        .env("USERPROFILE", temp.path())
        .env("NO_COLOR", "1")
        .env("CONTEXTSTREAM_WATCH", "0")
        .env("CONTEXTSTREAM_API_URL", "http://127.0.0.1:9")
        .env("CONTEXTSTREAM_API_KEY", "")
        .env("CONTEXTSTREAM_JWT", "  ")
        .current_dir(temp.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // No requests: closing stdin ends the server once it has started.
    drop(child.stdin.take());
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            child.kill().unwrap();
            panic!("the stdio server did not exit after stdin closed");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("(limited mode)"), "{stderr}");
}
