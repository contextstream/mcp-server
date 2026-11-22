use crate::domains::hosted_tools;
use crate::domains::hosted_tools_contract::{self, collect_contract_inputs};
use crate::peer::{run_with_call_peer, CallPeer, PeerChannel};
use crate::testing::TestFixtures;
use async_trait::async_trait;
use futures::{SinkExt, StreamExt};
use mcp_client::checkout_snapshot::CheckoutSnapshot;
use mcp_client::frames::{encode_frame, FrameBuffer};
use mcp_client::ContextStreamClient;
use mcp_session::SessionManager;
use mcp_types::tool::{ContentItem, ToolResult};
use serde_json::{json, Value};
use std::process::Command;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::accept_async;
use tokio_tungstenite::tungstenite::Message;

type ProgressCall = (f64, Option<f64>, Option<String>);

#[derive(Default)]
struct RecordingPeer {
    progress_calls: StdMutex<Vec<ProgressCall>>,
}

#[async_trait]
impl PeerChannel for RecordingPeer {
    async fn notify(&self, method: &str, params: Value) {
        if method == "notifications/progress" {
            let p = params
                .get("progress")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            let total = params.get("total").and_then(Value::as_f64);
            let msg = params
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string);
            self.progress_calls.lock().unwrap().push((p, total, msg));
        }
    }

    async fn request(
        &self,
        _method: &str,
        _params: Value,
        _timeout: Duration,
    ) -> std::result::Result<Value, String> {
        Err("unused".to_string())
    }
}

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

fn make_repo_a() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git_cmd(dir.path(), &["init", "-q"]);
    git_cmd(dir.path(), &["config", "user.email", "a@example.test"]);
    git_cmd(dir.path(), &["config", "user.name", "Tester"]);
    std::fs::create_dir_all(dir.path().join("openapi")).unwrap();
    std::fs::write(
        dir.path().join("openapi/billing.yaml"),
        "openapi: 3.0.0\ninfo:\n  title: Billing\n",
    )
    .unwrap();
    git_cmd(dir.path(), &["add", "-A"]);
    git_cmd(dir.path(), &["commit", "-qm", "init"]);
    dir
}

fn client_with_workspace(api_url: &str) -> ContextStreamClient {
    let mut cfg = TestFixtures::test_config();
    cfg.api_url = api_url.to_string();
    cfg.api_key = Some("test-key".to_string());
    cfg.default_workspace_id =
        Some(uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap());
    ContextStreamClient::new(cfg)
}

async fn read_http_req(socket: &mut tokio::net::TcpStream) -> Option<(String, String, Value)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = socket.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let len = head
        .lines()
        .skip(1)
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())?
        })
        .unwrap_or(0);
    while buf.len() < head_end + len {
        let n = socket.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let mut parts = head.lines().next()?.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.split('?').next().unwrap_or("").to_string();
    let body = serde_json::from_slice(&buf[head_end..]).unwrap_or(Value::Null);
    Some((method, path, body))
}

async fn spawn_ws_mock(
    output_text: &'static str,
    exit_code: i64,
) -> (String, tokio::task::JoinHandle<()>) {
    spawn_ws_mock_with(output_text, exit_code, true).await
}

/// A consumer worker. `acknowledge` false plays a worker from before contract checks: it runs
/// without saying it applied the contract overlay.
async fn spawn_ws_mock_with(
    output_text: &'static str,
    exit_code: i64,
    acknowledge: bool,
) -> (String, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
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
            let mut overlay_files = 0usize;
            while !(got_hello && got_pack) {
                if let Some(Ok(Message::Binary(bytes))) = ws.next().await {
                    buf.push(&bytes);
                    while let Some((kind, payload)) = buf.next_frame().unwrap() {
                        if kind == b'J' {
                            let hello: Value = serde_json::from_slice(&payload).unwrap();
                            overlay_files =
                                hello["contract_overlay"].as_array().map_or(0, Vec::len);
                            got_hello = true;
                        } else if kind == b'D' {
                            got_pack = true;
                        }
                    }
                }
            }
            ws.send(Message::binary(encode_frame(
                b'J',
                br#"{"type":"info","text":"running consumer check"}"#,
            )))
            .await
            .unwrap();
            if !output_text.is_empty() {
                ws.send(Message::binary(encode_frame(b'O', output_text.as_bytes())))
                    .await
                    .unwrap();
            }
            let mut res_frame = json!({
                "type": "result",
                "ran": true,
                "exit": exit_code,
            });
            if acknowledge {
                res_frame["contract_overlay_applied"] = json!(overlay_files);
            }
            ws.send(Message::binary(encode_frame(
                b'J',
                serde_json::to_vec(&res_frame).unwrap().as_slice(),
            )))
            .await
            .unwrap();
        }
    });
    (url, handle)
}

#[tokio::test]
async fn contract_blobs_respect_secret_filters_and_size_bounds() {
    let dir = make_repo_a();
    std::fs::write(dir.path().join(".contextignore"), "ignored/secret.yaml\n").unwrap();
    std::fs::create_dir_all(dir.path().join("ignored")).unwrap();
    std::fs::write(dir.path().join("ignored/secret.yaml"), "ignored: true\n").unwrap();
    std::fs::write(dir.path().join(".env"), "API_KEY=secret\n").unwrap();
    std::fs::write(dir.path().join("id_rsa"), "ssh-rsa AAAA\n").unwrap();
    std::fs::create_dir_all(dir.path().join("api")).unwrap();
    std::fs::write(
        dir.path().join("api/key.ts"),
        "export const K = `\n-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEAv1234567890=\n-----END RSA PRIVATE KEY-----\n`;\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("api/bin.proto"), [0xffu8, 0x00, 0xfe]).unwrap();
    std::fs::write(
        dir.path().join("openapi/billing.yaml"),
        "openapi: 3.0.0\npaths:\n  /v1/charge:\n    post:\n      summary: Charge\n",
    )
    .unwrap();

    let snap = CheckoutSnapshot::begin(dir.path())
        .await
        .unwrap()
        .complete()
        .await
        .unwrap();
    // A later edit must not replace the bytes the originating check observed.
    std::fs::write(
        dir.path().join("openapi/billing.yaml"),
        "different: true
",
    )
    .unwrap();
    let (_changed, files) = collect_contract_inputs(&snap).await;
    assert_eq!(
        files.len(),
        1,
        "only safe UTF-8 openapi/billing.yaml should pass"
    );
    assert_eq!(files[0].source_path, "openapi/billing.yaml");
    assert_eq!(
        files[0].content_b64,
        mcp_client::contract_check::encode_contract_b64(
            b"openapi: 3.0.0
paths:
  /v1/charge:
    post:
      summary: Charge
"
        )
    );
}

#[tokio::test]
async fn consumer_ts_rust_go_breakages_fail_repo_a_check_with_exact_compiler_errors() {
    let dir = make_repo_a();
    std::fs::write(
        dir.path().join("openapi/billing.yaml"),
        "openapi: 3.0.0\npaths:\n  /v2/charge: {}\n",
    )
    .unwrap();
    let snap = CheckoutSnapshot::begin(dir.path())
        .await
        .unwrap()
        .complete()
        .await
        .unwrap();

    let (ts_ws, h1) = spawn_ws_mock(
        "src/client.ts(14,22): error TS2339: Property 'userId' does not exist on type 'UserDto'.\n",
        2,
    )
    .await;
    let (rs_ws, h2) = spawn_ws_mock(
        "error[E0609]: no field `user_id` on type `UserResponse`\n --> src/main.rs:18:9\n",
        101,
    )
    .await;
    let (go_ws, h3) = spawn_ws_mock(
        "./client.go:24:15: resp.UserID undefined (type UserResponse has no field or method UserID)\n",
        1,
    )
    .await;

    let tree = "a".repeat(40);
    let resp_json = json!({
        "success": true,
        "data": {
            "source_project_id": "11111111-1111-4111-8111-111111111111",
            "matched_contract_paths": ["openapi/billing.yaml"],
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
                            "target_tree": tree
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
                            "target_tree": tree
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
                            "target_tree": tree
                        }
                    }
                }
            ],
            "elapsed_ms": 8
        }
    })
    .to_string();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_url = format!("http://{}", listener.local_addr().unwrap());
    let http_srv = tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let (method, path, _body) = read_http_req(&mut sock).await.unwrap();
            assert_eq!(method, "POST");
            assert_eq!(path, "/api/v1/mcp/hosted-tools/contract-check");
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{resp_json}",
                resp_json.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
        }
    });

    let client = client_with_workspace(&api_url);
    let session = SessionManager::new(client.clone(), TestFixtures::test_config());
    let recorder = Arc::new(RecordingPeer::default());
    let peer_handle = CallPeer::new(recorder.clone(), Some(json!("prog-1")), false);

    let mut result = ToolResult {
        content: vec![ContentItem::text("Repo A checks passed".to_string())],
        structured_content: Some(json!({"ran": true, "exit": 0})),
        is_error: false,
        meta: None,
    };

    run_with_call_peer(
        peer_handle,
        hosted_tools_contract::verify_consumer_contracts(
            &client,
            &session,
            &json!({"command": "cargo check"}),
            Some(&snap),
            &mut result,
        ),
    )
    .await;

    assert!(result.is_error);
    let sc = result.structured_content.as_ref().unwrap();
    assert_eq!(sc["exit"], 0);
    assert_eq!(sc["consumer_contracts_passed"], false);
    assert_eq!(sc["consumer_contracts_failed"], 3);
    assert_eq!(sc["consumer_contracts"].as_array().unwrap().len(), 3);

    let joined: String = result
        .content
        .iter()
        .filter_map(|c| match c {
            ContentItem::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        joined.contains("[consumer contracts: 3/3 failed]"),
        "{joined}"
    );
    assert!(joined.contains("TS2339"), "{joined}");
    assert!(joined.contains("E0609"), "{joined}");
    assert!(joined.contains("client.go:24:15"), "{joined}");

    let progress = recorder.progress_calls.lock().unwrap().clone();
    assert!(!progress.is_empty(), "expected notifications/progress");
    for w in progress.windows(2) {
        assert!(w[1].0 > w[0].0, "progress must be strictly monotonic");
    }

    let _ = (h1.await, h2.await, h3.await);
    http_srv.abort();
}

#[tokio::test]
async fn all_consumers_green_preserves_repo_a_success_and_404_fails_open() {
    let dir = make_repo_a();
    std::fs::write(dir.path().join("openapi/billing.yaml"), "openapi: 3.0.1\n").unwrap();
    let snap = CheckoutSnapshot::begin(dir.path())
        .await
        .unwrap()
        .complete()
        .await
        .unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_url = format!("http://{}", listener.local_addr().unwrap());
    let http_srv = tokio::spawn(async move {
        for idx in 0..2 {
            let (mut sock, _) = listener.accept().await.unwrap();
            let _ = read_http_req(&mut sock).await.unwrap();
            let (status, body) = if idx == 0 {
                (
                    "200 OK",
                    json!({
                        "success": true,
                        "data": {
                            "consumers": [{
                                "project_id": "22222222-2222-4222-8222-222222222222",
                                "project_name": "checkout-web",
                                "repo_key": "checkout-web-11111111",
                                "language": "typescript",
                                "command": "npx tsc --noEmit",
                                "result": { "ran": true, "exit_code": 0, "output": "" }
                            }],
                            "elapsed_ms": 2
                        }
                    })
                    .to_string(),
                )
            } else {
                ("404 Not Found", "{}".to_string())
            };
            let resp = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
        }
    });

    let client = client_with_workspace(&api_url);
    let session = SessionManager::new(client.clone(), TestFixtures::test_config());

    let mut green = ToolResult {
        content: vec![ContentItem::text("ok".to_string())],
        structured_content: Some(json!({"ran": true, "exit": 0})),
        is_error: false,
        meta: None,
    };
    hosted_tools_contract::verify_consumer_contracts(
        &client,
        &session,
        &json!({"command": "cargo check"}),
        Some(&snap),
        &mut green,
    )
    .await;
    assert!(!green.is_error);
    assert_eq!(
        green.structured_content.as_ref().unwrap()["consumer_contracts_passed"],
        true
    );

    let mut fail_open = ToolResult {
        content: vec![ContentItem::text("ok".to_string())],
        structured_content: Some(json!({"ran": true, "exit": 0})),
        is_error: false,
        meta: None,
    };
    hosted_tools_contract::verify_consumer_contracts(
        &client,
        &session,
        &json!({"command": "cargo check"}),
        Some(&snap),
        &mut fail_open,
    )
    .await;
    assert!(!fail_open.is_error);
    assert!(fail_open
        .structured_content
        .as_ref()
        .unwrap()
        .get("consumer_contracts")
        .is_none());

    // Unused helper sanity check so hosted_tools import stays live
    assert_eq!(hosted_tools::MAX_TOOLS, 16);
    http_srv.abort();
}

#[tokio::test]
async fn a_worker_that_does_not_acknowledge_the_contract_overlay_leaves_the_consumer_unverified() {
    let dir = make_repo_a();
    std::fs::write(
        dir.path().join("openapi/billing.yaml"),
        "openapi: 3.0.0\npaths:\n  /v2/charge: {}\n",
    )
    .unwrap();
    let snap = CheckoutSnapshot::begin(dir.path())
        .await
        .unwrap()
        .complete()
        .await
        .unwrap();

    // Both consumers compile green. The updated worker says it applied the overlay; the older one
    // ran the plain default branch, which proves nothing about the changed contract.
    let (updated_ws, h1) = spawn_ws_mock_with("", 0, true).await;
    let (older_ws, h2) = spawn_ws_mock_with("", 0, false).await;

    let tree = "a".repeat(40);
    let overlay =
        json!([{"path": "openapi/billing.yaml", "mode": "100644", "content_b64": "e30="}]);
    let consumer = |id: &str, name: &str, key: &str, url: &str, call: &str| {
        json!({
            "project_id": id,
            "project_name": name,
            "repo_key": key,
            "default_branch": "main",
            "language": "typescript",
            "command": "npx tsc --noEmit",
            "stream": {
                "url": url,
                "token": "mct_x",
                "protocol": "frames/1",
                "request": {
                    "id": call,
                    "repo_key": key,
                    "command": "npx tsc --noEmit",
                    "base_tree": tree,
                    "target_tree": tree,
                    "contract_overlay": overlay
                }
            }
        })
    };
    let resp_json = json!({
        "success": true,
        "data": {
            "source_project_id": "11111111-1111-4111-8111-111111111111",
            "matched_contract_paths": ["openapi/billing.yaml"],
            "consumers": [
                consumer("22222222-2222-4222-8222-222222222222", "checkout-web", "checkout-web-11111111", &updated_ws, "mc-0000000000000011"),
                consumer("33333333-3333-4333-8333-333333333333", "billing-admin", "billing-admin-22222222", &older_ws, "mc-0000000000000012"),
            ],
            "elapsed_ms": 3
        }
    })
    .to_string();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_url = format!("http://{}", listener.local_addr().unwrap());
    let http_srv = tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let _ = read_http_req(&mut sock).await.unwrap();
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{resp_json}",
                resp_json.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
        }
    });

    let client = client_with_workspace(&api_url);
    let session = SessionManager::new(client.clone(), TestFixtures::test_config());
    let mut result = ToolResult {
        content: vec![ContentItem::text("Repo A checks passed".to_string())],
        structured_content: Some(json!({"ran": true, "exit": 0})),
        is_error: false,
        meta: None,
    };
    hosted_tools_contract::verify_consumer_contracts(
        &client,
        &session,
        &json!({"command": "cargo check"}),
        Some(&snap),
        &mut result,
    )
    .await;

    // The source check stays green, but the contract is not reported as passed.
    assert!(!result.is_error);
    let sc = result.structured_content.as_ref().unwrap();
    assert_eq!(sc["consumer_contracts_passed"], false);
    assert_eq!(sc["consumer_contracts_failed"], 0);
    let outcomes = sc["consumer_contracts"].as_array().unwrap();
    let by_name = |name: &str| {
        outcomes
            .iter()
            .find(|o| o["project_name"] == name)
            .unwrap_or_else(|| panic!("no outcome for {name}: {outcomes:?}"))
    };
    assert_eq!(by_name("checkout-web")["ran"], true);
    assert_eq!(by_name("checkout-web")["passed"], true);
    assert_eq!(by_name("billing-admin")["ran"], false);
    assert_eq!(by_name("billing-admin")["passed"], false);

    let joined: String = result
        .content
        .iter()
        .filter_map(|c| match c {
            ContentItem::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(joined.contains("1/2 unverified"), "{joined}");
    assert!(
        joined
            .contains("billing-admin: the check host has not been updated for contract checks yet"),
        "{joined}"
    );

    let _ = (h1.await, h2.await);
    http_srv.abort();
}
