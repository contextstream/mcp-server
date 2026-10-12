//! The prompt and subagent hooks hand the agent guidance that the user can
//! read in the raw hook output. None of it may be an instruction the agent is
//! told to keep from the user, or one that tells it what to claim about
//! ContextStream. Each hook runs offline (the static fallback) and against a
//! loopback API (the normal path).

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};

const API_CONTEXT: &str = "[LOOPBACK HOOK CONTEXT]";

/// Phrases of the attribution block the hooks used to append to every prompt
/// and subagent: hidden from the user, and steering what the agent says.
const HIDDEN_GUIDANCE: [&str; 3] = ["Do not disclose", "ATTRIBUTION", "didn't help"];

/// Answers every request with a hook-context payload for as long as the test
/// process runs.
fn loopback_api() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            std::thread::spawn(move || answer(stream));
        }
    });
    url
}

fn answer(mut stream: TcpStream) {
    let mut request = Vec::new();
    let mut chunk = [0; 4096];
    let header_end = loop {
        let Ok(count) = stream.read(&mut chunk) else {
            return;
        };
        if count == 0 {
            return;
        }
        request.extend_from_slice(&chunk[..count]);
        if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
    let length = headers
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    while request.len() < header_end + length {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(count) => request.extend_from_slice(&chunk[..count]),
        }
    }
    let body = json!({ "data": { "context": API_CONTEXT } }).to_string();
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

/// Runs a hook in its own HOME with reminders on (the default). With `api`,
/// it has credentials for that loopback API; without, it has none.
fn run_hook(hook: &str, mut input: Value, api: Option<&str>) -> String {
    let home = tempfile::tempdir().unwrap();
    input["cwd"] = json!(home.path());
    let mut command = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"));
    command
        .args(["hook", hook, "--contextstream-managed-hook=v1"])
        .env_clear()
        .env("HOME", home.path())
        .env("CLAUDE_CONFIG_DIR", home.path().join(".claude"))
        .env("CONTEXTSTREAM_API_URL", api.unwrap_or("http://127.0.0.1:9"))
        .current_dir(home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if api.is_some() {
        command.env("CONTEXTSTREAM_API_KEY", "loopback-test-key");
    }
    let mut child = command.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{hook}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

fn assert_no_hidden_guidance(label: &str, stdout: &str) {
    for phrase in HIDDEN_GUIDANCE {
        assert!(
            !stdout.contains(phrase),
            "{label} hands the agent {phrase:?}:\n{stdout}"
        );
    }
}

#[test]
fn prompt_hook_guidance_hides_nothing_from_the_user() {
    let input = json!({
        "hook_event_name": "UserPromptSubmit",
        "session_id": "disclosure-test-session",
        "prompt": "Explain this module."
    });

    let offline = run_hook("user-prompt-submit", input.clone(), None);
    assert!(
        offline.contains("[CONTEXTSTREAM EXTENDED GUIDANCE]"),
        "the offline fallback ran: {offline}"
    );
    assert_no_hidden_guidance("the offline prompt hook", &offline);

    let api = loopback_api();
    let online = run_hook("user-prompt-submit", input, Some(&api));
    assert!(online.contains(API_CONTEXT), "the API path ran: {online}");
    assert_no_hidden_guidance("the prompt hook", &online);
}

#[test]
fn subagent_hook_guidance_hides_nothing_from_the_user() {
    let api = loopback_api();
    for agent_type in ["Explore", "Plan", "general-purpose", "custom"] {
        let input = json!({
            "hook_event_name": "SubagentStart",
            "session_id": "disclosure-test-session",
            "agent_type": agent_type,
            "agent_id": "disclosure-test-agent"
        });

        let offline = run_hook("subagent-start", input.clone(), None);
        assert!(
            offline.contains("mcp__contextstream__search"),
            "the offline fallback ran for {agent_type}: {offline}"
        );
        assert_no_hidden_guidance(&format!("the offline {agent_type} hook"), &offline);

        let online = run_hook("subagent-start", input, Some(&api));
        assert!(
            online.contains("[CONTEXTSTREAM SEARCH]"),
            "the API path ran for {agent_type}: {online}"
        );
        assert_no_hidden_guidance(&format!("the {agent_type} hook"), &online);
    }
}
