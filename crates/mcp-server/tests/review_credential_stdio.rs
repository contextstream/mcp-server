//! Integration tests for `CONTEXTSTREAM_CAPSULE_CREDENTIAL` stdio isolation.
//!
//! Spawns the real `contextstream-mcp` binary and verifies:
//! 1. `CONTEXTSTREAM_CAPSULE_CREDENTIAL` overrides both `~/.contextstream/credentials.json`
//!    and ambient `CONTEXTSTREAM_API_KEY`/`CONTEXTSTREAM_JWT` env vars.
//! 2. `tools/list` exposes only `["capsule"]` (ignoring `CONTEXTSTREAM_ROUTER_MODE`,
//!    `CONTEXTSTREAM_TOOLSET`, etc.).
//! 3. Non-`capsule` tools and non-review actions fail locally with zero API requests.
//! 4. `capsule(action="open")` and `capsule(action="review_answer")` hit only
//!    `/api/v1/capsules/review/open*` with only `X-Capsule-Credential`.
//! 5. Blank/whitespace `CONTEXTSTREAM_CAPSULE_CREDENTIAL` without other credentials
//!    still enters limited mode cleanly.

use serde_json::json;
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

fn sha256_hex(text: &str) -> String {
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    hex::encode(h.finalize())
}

type CapturedRequests = Arc<Mutex<Vec<(String, String)>>>;

fn spawn_review_mock_server() -> (String, CapturedRequests) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
    let addr = listener.local_addr().expect("local_addr");
    let captured = Arc::new(Mutex::new(Vec::new()));
    let captured_bg = captured.clone();

    let md = "# Review capsule 0123456789abcdef  (part 1 of 1)\n\nStdio body".to_string();
    let sha = sha256_hex(&md);
    let digest = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_string();

    std::thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let captured_conn = captured_bg.clone();
            let md = md.clone();
            let sha = sha.clone();
            let digest = digest.clone();
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                let head = loop {
                    let n = match stream.read(&mut chunk) {
                        Ok(n) if n > 0 => n,
                        _ => return,
                    };
                    buf.extend_from_slice(&chunk[..n]);
                    let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    let head = String::from_utf8_lossy(&buf[..end]).to_string();
                    let len: usize = head
                        .lines()
                        .filter_map(|l| l.split_once(':'))
                        .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
                        .and_then(|(_, v)| v.trim().parse().ok())
                        .unwrap_or(0);
                    if buf.len() >= end + 4 + len {
                        break head;
                    }
                };
                let line = head.lines().next().unwrap_or_default().to_string();
                captured_conn.lock().unwrap().push((line.clone(), head));

                let (status, reason, ctype, extra, body) = if line
                    .starts_with("GET /api/v1/capsules/review/open ")
                {
                    (
                        200,
                        "OK",
                        "application/json",
                        String::new(),
                        json!({
                            "success": true,
                            "data": {
                                "review": {
                                    "label": "rev",
                                    "digest": digest,
                                    "repo": "org/repo",
                                    "base": "a",
                                    "head": "b",
                                    "files": 1,
                                    "files_shown": 1,
                                    "parts": 1
                                },
                                "manifest": {
                                    "contract_version": 1,
                                    "digest": digest,
                                    "label": "rev",
                                    "source": "client",
                                    "repo": "org/repo",
                                    "base": "a",
                                    "head": "b",
                                    "title": "T",
                                    "task": "Task",
                                    "part_chars": 4000,
                                    "parts": [{ "n": 1, "chars": md.len(), "sha256": sha }]
                                }
                            }
                        })
                        .to_string(),
                    )
                } else if line.starts_with("GET /api/v1/capsules/review/open/prompt.md") {
                    (
                        200,
                        "OK",
                        "text/markdown; charset=utf-8",
                        format!("X-Capsule-Digest: {digest}\r\nX-Capsule-Part-SHA256: {sha}\r\n"),
                        md,
                    )
                } else if line.starts_with("POST /api/v1/capsules/review/open/answers ") {
                    (
                        201,
                        "Created",
                        "application/json",
                        String::new(),
                        json!({
                            "success": true,
                            "data": {
                                "valid": true,
                                "verdict": "SAFE TO LAND",
                                "must": 0,
                                "should": 0,
                                "status": "approved"
                            }
                        })
                        .to_string(),
                    )
                } else {
                    (
                        500,
                        "Unexpected",
                        "application/json",
                        String::new(),
                        json!({"error": format!("unexpected request: {line}")}).to_string(),
                    )
                };

                let resp = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: {ctype}\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
            });
        }
    });

    (format!("http://{addr}"), captured)
}

fn recv_response_for_id(
    rx: &mpsc::Receiver<String>,
    expected_id: i64,
    timeout: Duration,
) -> serde_json::Value {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let Ok(line) = rx.recv_timeout(Duration::from_millis(200)) else {
            continue;
        };
        if let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) {
            if msg.get("id").and_then(serde_json::Value::as_i64) == Some(expected_id) {
                return msg;
            }
        }
    }
    panic!("timed out waiting for JSON-RPC response id={expected_id}");
}

#[test]
fn stdio_review_credential_ignores_disk_and_ambient_member_keys_and_isolates_tools() {
    let (mock_url, captured) = spawn_review_mock_server();
    let home = tempfile::tempdir().unwrap();
    let cs_dir = home.path().join(".contextstream");
    std::fs::create_dir_all(&cs_dir).unwrap();
    std::fs::write(
        cs_dir.join("credentials.json"),
        r#"{"api_url":"https://saved.example.com","api_key":"cbiq_disk_secret_key"}"#,
    )
    .unwrap();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"));
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("CONTEXTSTREAM_") {
            cmd.env_remove(k);
        }
    }
    let mut child = cmd
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        .env("NO_COLOR", "1")
        .env("CONTEXTSTREAM_WATCH", "0")
        .env("CONTEXTSTREAM_API_URL", &mock_url)
        .env("CONTEXTSTREAM_API_KEY", "cbiq_ambient_secret_key")
        .env("CONTEXTSTREAM_JWT", "ambient.jwt.secret")
        .env("CONTEXTSTREAM_CAPSULE_CREDENTIAL", "capsule_stdio_secret")
        .env("CONTEXTSTREAM_TOOLSET", "light")
        .env("CONTEXTSTREAM_ROUTER_MODE", "true")
        .env("CONTEXTSTREAM_TRANSCRIPTS_ENABLED", "true")
        .current_dir(home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn contextstream-mcp");

    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let mut stdin = child.stdin.take().unwrap();
    let send = |stdin: &mut std::process::ChildStdin, v: serde_json::Value| {
        writeln!(stdin, "{v}").unwrap();
        stdin.flush().unwrap();
    };

    // 1. initialize -> toolCount: 1, listChanged: false
    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "protocolVersion": "2025-03-26", "clientInfo": { "name": "chatgpt" } }
        }),
    );
    let init_resp = recv_response_for_id(&rx, 1, Duration::from_secs(15));
    assert_eq!(init_resp["result"]["toolCount"], 1, "{init_resp}");
    assert_eq!(
        init_resp["result"]["capabilities"]["tools"]["listChanged"], false,
        "{init_resp}"
    );

    // 2. tools/list -> ONLY ["capsule"]
    send(
        &mut stdin,
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
    );
    let list_resp = recv_response_for_id(&rx, 2, Duration::from_secs(15));
    let names: Vec<&str> = list_resp["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, vec!["capsule"], "{list_resp}");

    // 3. Blocked tools (including discovery meta-tools) + blocked capsule action fail locally with zero network requests.
    for (req_id, tool_name, args) in [
        (3, "memory", json!({ "action": "list_docs" })),
        (4, "tool_search", json!({ "query": "memory" })),
        (
            5,
            "execute_operation",
            json!({ "operation": "memory.list_docs", "arguments": {} }),
        ),
    ] {
        send(
            &mut stdin,
            json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "method": "tools/call",
                "params": { "name": tool_name, "arguments": args }
            }),
        );
        let blocked_tool = recv_response_for_id(&rx, req_id, Duration::from_secs(15));
        assert!(blocked_tool.get("error").is_some(), "{blocked_tool}");
    }

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "tools/call",
            "params": { "name": "capsule", "arguments": { "action": "create", "purpose": "review" } }
        }),
    );
    let blocked_act = recv_response_for_id(&rx, 6, Duration::from_secs(15));
    assert_eq!(blocked_act["result"]["isError"], true, "{blocked_act}");
    assert!(
        captured.lock().unwrap().is_empty(),
        "blocked calls must make zero HTTP requests: {:?}",
        captured.lock().unwrap()
    );

    // 4. open + review_answer work via X-Capsule-Credential ONLY.
    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": { "name": "capsule", "arguments": { "action": "open" } }
        }),
    );
    let open_resp = recv_response_for_id(&rx, 7, Duration::from_secs(15));
    assert_ne!(open_resp["result"]["isError"], true, "{open_resp}");

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "tools/call",
            "params": {
                "name": "capsule",
                "arguments": { "action": "review_answer", "answer": "CAPSULE: 0123456789abcdef\nVERDICT: SAFE TO LAND" }
            }
        }),
    );
    let ans_resp = recv_response_for_id(&rx, 8, Duration::from_secs(15));
    assert_ne!(ans_resp["result"]["isError"], true, "{ans_resp}");

    drop(stdin);
    let _ = child.kill();
    let _ = child.wait();

    let reqs = captured.lock().unwrap().clone();
    assert_eq!(reqs.len(), 3, "expected open + part=1 + answers: {reqs:?}");
    for (line, head) in &reqs {
        let lower = head.to_ascii_lowercase();
        assert!(
            lower.contains("x-capsule-credential: capsule_stdio_secret"),
            "missing X-Capsule-Credential on {line}: {head}"
        );
        assert!(
            !lower.contains("x-api-key:")
                && !lower.contains("authorization:")
                && !lower.contains("cbiq_disk_secret_key")
                && !lower.contains("cbiq_ambient_secret_key"),
            "member credentials leaked on {line}: {head}"
        );
    }
}

#[test]
fn stdio_blank_review_credential_falls_back_to_limited_mode() {
    let temp = tempfile::tempdir().unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"));
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("CONTEXTSTREAM_") {
            cmd.env_remove(k);
        }
    }
    let mut child = cmd
        .env("HOME", temp.path())
        .env("USERPROFILE", temp.path())
        .env("NO_COLOR", "1")
        .env("CONTEXTSTREAM_WATCH", "0")
        .env("CONTEXTSTREAM_API_URL", "http://127.0.0.1:9")
        .env("CONTEXTSTREAM_CAPSULE_CREDENTIAL", "   ")
        .current_dir(temp.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
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
