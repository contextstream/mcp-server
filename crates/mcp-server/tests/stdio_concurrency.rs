//! A tool call waiting on an API that does not answer must not hold up the
//! requests behind it that need no API: `ping` and `tools/list` were answered
//! only after the stalled call gave up, 253 s later (#478).
#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// An API that accepts connections, reads the request and never answers.
fn stalled_api() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = listener.local_addr().expect("address");
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming().flatten() {
            let mut reader = stream.try_clone().expect("clone");
            std::thread::spawn(move || {
                let mut buffer = [0u8; 4096];
                while matches!(reader.read(&mut buffer), Ok(count) if count > 0) {}
            });
            held.push(stream);
        }
    });
    format!("http://{address}")
}

#[test]
fn ping_and_tools_list_are_answered_while_a_tool_call_waits_on_the_api() {
    let home = tempfile::tempdir().expect("isolated home");
    let mut command = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"));
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("CONTEXTSTREAM_") {
            command.env_remove(key);
        }
    }
    let mut child = command
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        .env("CONTEXTSTREAM_API_URL", stalled_api())
        .env(
            "CONTEXTSTREAM_API_KEY",
            "cs_stalled_api_test_key_0000000000",
        )
        .env("CONTEXTSTREAM_WATCH", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("run contextstream-mcp");

    let stdout = child.stdout.take().expect("stdout");
    let (lines, answers) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if lines.send(line).is_err() {
                break;
            }
        }
    });

    let mut stdin = child.stdin.take().expect("stdin");
    let workspace = "11111111-1111-4111-8111-111111111111";
    for request in [
        format!(
            r#"{{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{{"name":"memory","arguments":{{"action":"list_docs","limit":2,"workspace_id":"{workspace}"}}}}}}"#
        ),
        r#"{"jsonrpc":"2.0","id":2,"method":"ping"}"#.to_string(),
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/list"}"#.to_string(),
    ] {
        writeln!(stdin, "{request}").expect("write request");
    }
    stdin.flush().expect("flush");

    let started = Instant::now();
    let mut answered = Vec::new();
    while answered.len() < 2 && started.elapsed() < Duration::from_secs(20) {
        let Ok(line) = answers.recv_timeout(Duration::from_millis(500)) else {
            continue;
        };
        let Ok(message) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if let Some(id) = message.get("id").and_then(serde_json::Value::as_i64) {
            answered.push((id, started.elapsed()));
        }
    }
    let _ = child.kill();
    let _ = child.wait();

    let ids: Vec<i64> = answered.iter().map(|(id, _)| *id).collect();
    assert!(
        ids.contains(&2),
        "ping must be answered while the call waits: {answered:?}"
    );
    assert!(
        ids.contains(&3),
        "tools/list must be answered while the call waits: {answered:?}"
    );
    assert!(
        !ids.contains(&1),
        "the call itself is still waiting on the API: {answered:?}"
    );
}
