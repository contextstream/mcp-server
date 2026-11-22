//! End-to-end stdio JSON-RPC test for Phase 3.3 cross-repo contract verification.
//! Exercises `contextstream-mcp` over real stdin/stdout JSON-RPC (`tools/call` for `"check"`),
//! streaming Repo A + 3 downstream consumers (TS/Rust/Go) and verifying `isError: true`,
//! `structuredContent`, `notifications/progress`, and exact compiler diagnostics.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    routing::{get, post},
    Json, Router,
};
use futures::{SinkExt, StreamExt};
use mcp_client::frames::{encode_frame, FrameBuffer};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

const WORKSPACE_ID: &str = "550e8400-e29b-41d4-a716-446655440000";

fn git_cmd(dir: &std::path::Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

async fn spawn_primary_ws_server() -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "ws://127.0.0.1:{}/v1/run",
        listener.local_addr().unwrap().port()
    );
    let handle = tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            let mut ws = accept_async(stream).await.unwrap();
            let mut buf = FrameBuffer::default();
            let mut got_hello = false;
            let mut got_pack = false;
            while !(got_hello && got_pack) {
                if let Some(Ok(Message::Binary(bytes))) = ws.next().await {
                    buf.push(&bytes);
                    while let Some((kind, _)) = buf.next_frame().unwrap() {
                        if kind == b'J' {
                            got_hello = true;
                        } else if kind == b'D' {
                            got_pack = true;
                        }
                    }
                }
            }
            let result_frame = json!({
                "type": "result",
                "ran": true,
                "exit": 0,
                "tool_result": {
                    "content": [{"type": "text", "text": "Repo A cargo check passed"}],
                    "structuredContent": {"ran": true, "exit": 0},
                    "isError": false
                }
            });
            ws.send(Message::binary(encode_frame(
                b'J',
                serde_json::to_vec(&result_frame).unwrap().as_slice(),
            )))
            .await
            .unwrap();
        }
    });
    (url, handle)
}

async fn spawn_consumer_ws_server(
    output_text: &'static str,
    exit_code: i64,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "ws://127.0.0.1:{}/v1/run",
        listener.local_addr().unwrap().port()
    );
    let handle = tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            let mut ws = accept_async(stream).await.unwrap();
            let mut buf = FrameBuffer::default();
            let mut got_hello = false;
            let mut got_pack = false;
            while !(got_hello && got_pack) {
                if let Some(Ok(Message::Binary(bytes))) = ws.next().await {
                    buf.push(&bytes);
                    while let Some((kind, payload)) = buf.next_frame().unwrap() {
                        if kind == b'J' {
                            let hello: Value = serde_json::from_slice(&payload).unwrap();
                            assert!(hello["contract_overlay"].is_array());
                            got_hello = true;
                        } else if kind == b'D' {
                            got_pack = true;
                        }
                    }
                }
            }
            ws.send(Message::binary(encode_frame(
                b'J',
                br#"{"type":"info","text":"verifying consumer contract"}"#,
            )))
            .await
            .unwrap();
            ws.send(Message::binary(encode_frame(b'O', output_text.as_bytes())))
                .await
                .unwrap();
            ws.send(Message::binary(encode_frame(
                b'J',
                serde_json::to_vec(&json!({
                    "type": "result",
                    "ran": true,
                    "exit": exit_code,
                }))
                .unwrap()
                .as_slice(),
            )))
            .await
            .unwrap();
        }
    });
    (url, handle)
}

async fn spawn_mcd_consumer(
    fixture: &std::ffi::OsStr,
) -> (String, String, tokio::task::JoinHandle<()>) {
    let mut child = tokio::process::Command::new("python3")
        .arg(fixture)
        .arg("--stdio-bridge")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("start local mcd fixture");
    let mut output = tokio::io::BufReader::new(child.stdout.take().unwrap());
    let mut ready = String::new();
    tokio::time::timeout(Duration::from_secs(60), output.read_line(&mut ready))
        .await
        .expect("warm mcd fixture")
        .unwrap();
    let ready: Value = serde_json::from_str(&ready).expect("fixture's green default branch");
    assert_eq!(ready["branch"], "main");
    assert_eq!(ready["baseline_exit"], 0);
    let tree = ready["tree"].as_str().unwrap().to_string();
    let mut input = child.stdin.take().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/v1/run", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut ws = accept_async(socket).await.unwrap();
        let mut incoming = FrameBuffer::default();
        let mut pack_received = false;
        while !pack_received {
            let Some(Ok(Message::Binary(bytes))) = ws.next().await else {
                panic!("MCP ended before sending contract hello and pack");
            };
            incoming.push(&bytes);
            while let Some((kind, payload)) = incoming.next_frame().unwrap() {
                input
                    .write_all(&encode_frame(kind, &payload))
                    .await
                    .unwrap();
                pack_received |= kind == b'D';
            }
        }
        input.flush().await.unwrap();
        loop {
            let kind = output.read_u8().await.unwrap();
            let size = output.read_u32().await.unwrap();
            let mut payload = vec![0; size as usize];
            output.read_exact(&mut payload).await.unwrap();
            ws.send(Message::binary(encode_frame(kind, &payload)))
                .await
                .unwrap();
            if kind == b'J' {
                let frame: Value = serde_json::from_slice(&payload).unwrap();
                assert_ne!(
                    frame["type"], "need",
                    "consumer's default-branch tree must be warm"
                );
                if frame["type"] == "result" {
                    assert_eq!(frame["warm"], true);
                    assert_eq!(frame["exit"], 101);
                    assert_eq!(frame["ran"], true);
                    break;
                }
            }
        }
        drop(input);
        assert!(
            child.wait().await.unwrap().success(),
            "mcd fixture or workspace assertion failed"
        );
    });
    (url, tree, task)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stdio_check_surfaces_downstream_ts_rust_go_contract_breakages() {
    let repo_a = tempfile::tempdir().unwrap();
    git_cmd(repo_a.path(), &["init", "-q"]);
    git_cmd(repo_a.path(), &["config", "user.email", "a@example.test"]);
    git_cmd(repo_a.path(), &["config", "user.name", "Tester"]);
    let real_fixture = std::env::var_os("P33_REAL_MCD");
    let (path, original, broken) = if real_fixture.is_some() {
        (
            "schema/contract.rs",
            "pub struct ChargeRequest { pub amount_cents: u64 }
",
            "pub struct ChargeRequest { pub total_cents: u64 }
",
        )
    } else {
        (
            "schema/user.ts",
            "export interface UserDto { userId: string; }
",
            "export interface UserDto { accountId: string; }
",
        )
    };
    std::fs::create_dir_all(repo_a.path().join("schema")).unwrap();
    std::fs::write(repo_a.path().join(path), original).unwrap();
    git_cmd(repo_a.path(), &["add", "-A"]);
    git_cmd(repo_a.path(), &["commit", "-qm", "init"]);
    std::fs::write(repo_a.path().join(path), broken).unwrap();

    let (primary_ws, h_primary) = spawn_primary_ws_server().await;
    let (ts_ws, h_ts) = spawn_consumer_ws_server(
        "src/client.ts(14,22): error TS2339: Property 'userId' does not exist on type 'UserDto'.\n",
        2,
    )
    .await;
    let (rs_ws, tree, h_rs) = if let Some(fixture) = &real_fixture {
        spawn_mcd_consumer(fixture).await
    } else {
        let (url, handle) = spawn_consumer_ws_server(
            "error[E0609]: no field `user_id` on type `UserResponse`
 --> src/main.rs:18:9
",
            101,
        )
        .await;
        (url, "b".repeat(40), handle)
    };
    let (go_ws, h_go) = spawn_consumer_ws_server(
        "./client.go:24:15: resp.UserID undefined (type UserResponse has no field or method UserID)\n",
        1,
    )
    .await;

    let seen_contract_reqs: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
    let seen_clone = seen_contract_reqs.clone();

    let api = Router::new()
        .route(
            "/api/v1/mcp/hosted-tools",
            get(|| async {
                Json(json!({
                    "tools": [{
                        "name": "check",
                        "title": "Check",
                        "description": "Remote check",
                        "input_schema": {
                            "type": "object",
                            "properties": { "command": { "type": "string" } },
                            "required": ["command"]
                        },
                        "transport": { "kind": "stream" },
                        "attachments": ["checkout_snapshot"]
                    }]
                }))
            }),
        )
        .route(
            "/api/v1/mcp/hosted-tools/check/call",
            post({
                let primary_ws = primary_ws.clone();
                move || {
                    let primary_ws = primary_ws.clone();
                    async move {
                        Json(json!({
                            "success": true,
                            "data": {
                                "stream": {
                                    "url": primary_ws,
                                    "token": "mct_primary",
                                    "protocol": "frames/1",
                                    "request": {
                                        "id": "mc-0000000000000000",
                                        "repo_key": "repo-a-00000000",
                                        "command": "cargo check",
                                        "cwd": "",
                                        "timeout_s": 60,
                                        "wait_s": 10,
                                        "tool_result": true
                                    }
                                },
                                "call": { "id": "00000000-0000-4000-8000-000000000001", "expires_at": "2030-01-01T00:00:00Z" }
                            }
                        }))
                    }
                }
            }),
        )
        .route(
            "/api/v1/mcp/hosted-tools/contract-check",
            post(move |Json(body): Json<Value>| {
                let payload = body["contract_files"][0]["content_b64"].clone();
                seen_clone.lock().unwrap().push(body);
                let ts_ws = ts_ws.clone();
                let rs_ws = rs_ws.clone();
                let go_ws = go_ws.clone();
                let tree = tree.clone();
                async move {
                    let overlay = json!([{
                        "path": "src/contract.rs",
                        "mode": "100644",
                        "content_b64": payload
                    }]);
                    Json(json!({
                        "success": true,
                        "data": {
                            "source_project_id": "11111111-1111-4111-8111-111111111111",
                            "matched_contract_paths": ["schema/user.ts"],
                            "consumers": [
                                {
                                    "project_id": "22222222-2222-4222-8222-222222222222",
                                    "project_name": "checkout-web",
                                    "repo_key": "checkout-web-11111111",
                                    "default_branch": "main",
                                    "language": "typescript",
                                    "command": "npx tsc --noEmit",
                                    "stream": {
                                        "url": ts_ws,
                                        "token": "mct_ts",
                                        "protocol": "frames/1",
                                        "request": {
                                            "id": "mc-0000000000000001",
                                            "repo_key": "checkout-web-11111111",
                                            "command": "npx tsc --noEmit",
                                            "base_tree": tree,
                                            "target_tree": tree,
                                            "contract_overlay": overlay
                                        }
                                    }
                                },
                                {
                                    "project_id": "33333333-3333-4333-8333-333333333333",
                                    "project_name": "ledger-rust",
                                    "repo_key": "ledger-rust-22222222",
                                    "default_branch": "main",
                                    "language": "rust",
                                    "command": "cargo check",
                                    "stream": {
                                        "url": rs_ws,
                                        "token": "mct_rs",
                                        "protocol": "frames/1",
                                        "request": {
                                            "id": "mc-0000000000000002",
                                            "repo_key": "ledger-rust-22222222",
                                            "command": "cargo check",
                                            "base_tree": tree,
                                            "target_tree": tree,
                                            "contract_overlay": overlay
                                        }
                                    }
                                },
                                {
                                    "project_id": "44444444-4444-4444-8444-444444444444",
                                    "project_name": "gateway-go",
                                    "repo_key": "gateway-go-33333333",
                                    "default_branch": "main",
                                    "language": "go",
                                    "command": "go vet ./...",
                                    "stream": {
                                        "url": go_ws,
                                        "token": "mct_go",
                                        "protocol": "frames/1",
                                        "request": {
                                            "id": "mc-0000000000000003",
                                            "repo_key": "gateway-go-33333333",
                                            "command": "go vet ./...",
                                            "base_tree": tree,
                                            "target_tree": tree,
                                            "contract_overlay": overlay
                                        }
                                    }
                                }
                            ],
                            "elapsed_ms": 6
                        }
                    }))
                }
            }),
        );

    let api_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_url = format!("http://{}", api_listener.local_addr().unwrap());
    let api_srv = tokio::spawn(async move { axum::serve(api_listener, api).await.unwrap() });

    let home = tempfile::tempdir().unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"));
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("CONTEXTSTREAM_") {
            cmd.env_remove(k);
        }
    }
    let mut child = cmd
        .current_dir(repo_a.path())
        .env("HOME", home.path())
        .env("XDG_CONFIG_HOME", home.path().join(".config"))
        .env("CONTEXTSTREAM_API_URL", &api_url)
        .env("CONTEXTSTREAM_API_KEY", "cs_contract_test_key")
        .env("CONTEXTSTREAM_WORKSPACE_ID", WORKSPACE_ID)
        .env("CONTEXTSTREAM_WATCH", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn contextstream-mcp");

    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<Value>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(val) = serde_json::from_str::<Value>(&line) {
                let _ = tx.send(val);
            }
        }
    });

    let mut stdin = child.stdin.take().unwrap();
    let init_req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": { "name": "contract-test", "version": "1.0" },
            "workspaceFolders": [{
                "uri": format!("file://{}", repo_a.path().display()),
                "name": "repo_a"
            }]
        }
    });
    writeln!(stdin, "{init_req}").unwrap();
    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
    )
    .unwrap();
    let call_req = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": {
            "name": "check",
            "arguments": { "command": "cargo check" },
            "_meta": { "progressToken": "contract-prog-1" }
        }
    });
    writeln!(stdin, "{call_req}").unwrap();
    stdin.flush().unwrap();

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut progress_msgs = Vec::new();
    let mut call_response: Option<Value> = None;
    while Instant::now() < deadline {
        if let Ok(msg) = rx.recv_timeout(Duration::from_millis(200)) {
            if msg.get("method").and_then(Value::as_str) == Some("notifications/progress") {
                progress_msgs.push(msg);
            } else if msg.get("id").and_then(Value::as_i64) == Some(2) {
                call_response = Some(msg);
                break;
            }
        }
    }

    let _ = child.kill();
    let _ = child.wait();
    for task in [h_primary, h_ts, h_rs, h_go] {
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("worker finished")
            .expect("worker assertions passed");
    }
    api_srv.abort();

    let resp = call_response.expect("expected stdio JSON-RPC response for id=2");
    let res = &resp["result"];
    assert_eq!(res["isError"], true, "expected isError=true: {resp}");
    assert_eq!(res["structuredContent"]["exit"], 0);
    assert_eq!(res["structuredContent"]["consumer_contracts_passed"], false);
    assert_eq!(res["structuredContent"]["consumer_contracts_failed"], 3);
    assert_eq!(
        res["structuredContent"]["consumer_contracts"]
            .as_array()
            .unwrap()
            .len(),
        3
    );

    let all_text: String = res["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(all_text.contains("Repo A cargo check passed"), "{all_text}");
    assert!(all_text.contains("TS2339"), "{all_text}");
    if real_fixture.is_some() {
        let rust = &res["structuredContent"]["consumer_contracts"][1];
        assert_eq!(rust["exit_code"], 101);
        let errors = rust["errors"].as_array().unwrap();
        assert!(
            errors.iter().any(|line| line.as_str()
                == Some("error[E0609]: no field `amount_cents` on type `&ChargeRequest`")),
            "{all_text}"
        );
        assert!(all_text.contains("src/lib.rs:2:59"), "{all_text}");
        println!("Real Rust result (default branch main, warm=true, baseline exit=0, consumer exit=101)\n{all_text}");
    } else {
        assert!(all_text.contains("E0609"), "{all_text}");
    }
    assert!(all_text.contains("client.go:24:15"), "{all_text}");
    assert!(
        !progress_msgs.is_empty(),
        "expected stdio notifications/progress messages"
    );

    let reqs = seen_contract_reqs.lock().unwrap();
    assert_eq!(reqs.len(), 1);
    assert_eq!(reqs[0]["changed_paths"], json!([path]));
    assert_eq!(reqs[0]["contract_files"][0]["source_path"], path);
}
