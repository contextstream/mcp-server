use super::*;
use crate::testing::TestFixtures;
use serde_json::json;
use std::sync::Mutex as StdMutex;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[derive(Debug, Clone)]
struct Seen {
    method: String,
    path: String,
    query: String,
    body: Value,
}

type Respond = dyn Fn(&str, &str, &Value) -> (u16, String) + Send + Sync;

/// A stand-in server: each request is answered with `respond(method, path, body)`.
async fn stub(respond: Arc<Respond>) -> (String, Arc<StdMutex<Vec<Seen>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(StdMutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let respond = respond.clone();
            let log = log.clone();
            tokio::spawn(async move {
                let Some(request) = read_request(&mut socket).await else {
                    return;
                };
                let (status, body) = respond(&request.method, &request.path, &request.body);
                log.lock().unwrap().push(request);
                let response = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (url, seen)
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<Seen> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 8192];
    let header_end = loop {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let header = |name: &str| {
        head.lines().skip(1).find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_string())
        })
    };
    let length = header("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    while buffer.len() < header_end + length {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let mut parts = head.lines().next()?.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let path = target.split('?').next().unwrap_or("").to_string();
    let query = target
        .split_once('?')
        .map(|(_, q)| q.to_string())
        .unwrap_or_default();
    Some(Seen {
        method,
        path: path.strip_prefix("/api/v1").unwrap_or(&path).to_string(),
        query,
        body: serde_json::from_slice(&buffer[header_end..]).unwrap_or(Value::Null),
    })
}

fn client(url: &str) -> ContextStreamClient {
    let mut config = TestFixtures::test_config();
    config.api_url = url.to_string();
    config.api_key = Some("hosted-tools-test-key".to_string());
    ContextStreamClient::new(config)
}

fn session_for(client: &ContextStreamClient) -> SessionManager {
    SessionManager::new(client.clone(), TestFixtures::test_config())
}

const WORKSPACE: &str = "550e8400-e29b-41d4-a716-446655440000";

/// A client for `url` whose caller works in `WORKSPACE`.
fn client_in_workspace(url: &str) -> ContextStreamClient {
    let mut config = TestFixtures::test_config();
    config.api_url = url.to_string();
    config.api_key = Some("hosted-tools-test-key".to_string());
    config.default_workspace_id = Some(uuid::Uuid::parse_str(WORKSPACE).unwrap());
    ContextStreamClient::new(config)
}

fn git_here(dir: &std::path::Path, args: &[&str]) {
    let out = std::process::Command::new("git")
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

fn echo_definition(transport: &str) -> Value {
    json!({
        "name": "echo",
        "title": "Echo",
        "description": "Answer with the text it is given.",
        "input_schema": {
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"]
        },
        "transport": {"kind": transport}
    })
}

fn text_of(result: &ToolResult) -> Vec<String> {
    result
        .content
        .iter()
        .map(|item| match item {
            ContentItem::Text { text } => text.clone(),
            other => format!("{other:?}"),
        })
        .collect()
}

#[test]
fn the_catalog_keeps_only_usable_definitions() {
    let tools = parse_catalog(&json!({"tools": [
        echo_definition("api"),
        echo_definition("api"),
        {"name": "Bad-Name", "description": "x", "input_schema": {"type": "object"}},
        {"name": "no_description", "input_schema": {"type": "object"}},
        {"name": "no_schema", "description": "x"},
        {"name": "string_schema", "description": "x", "input_schema": {"type": "string"}},
        {"name": "other_transport", "description": "x", "input_schema": {"type": "object"}, "transport": {"kind": "carrier-pigeon"}},
        {"name": "streamed", "description": "x", "inputSchema": {"type": "object"}, "transport": {"kind": "stream"}},
    ]}));
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    assert_eq!(names, ["echo", "streamed"]);
    assert_eq!(tools[0].transport, Transport::Api);
    assert_eq!(tools[1].transport, Transport::Stream);
    assert_eq!(
        tools[0].list_entry()["inputSchema"]["required"],
        json!(["text"])
    );
    assert_eq!(tools[0].list_entry()["title"], "Echo");

    let many: Vec<Value> = (0..40)
        .map(|n| json!({"name": format!("tool_{n}"), "description": "x", "input_schema": {"type": "object"}}))
        .collect();
    assert_eq!(parse_catalog(&json!({"tools": many})).len(), MAX_TOOLS);
    assert!(parse_catalog(&json!({"something": "else"})).is_empty());
}

#[test]
fn a_blip_keeps_the_tools_known_before() {
    let gate = HostedToolsGate::new();
    let known = Read::Known(Arc::new(parse_catalog(
        &json!({"tools": [echo_definition("api")]}),
    )));
    assert_eq!(gate.record("me", known).len(), 1);
    assert_eq!(gate.record("me", Read::Unreachable).len(), 1);
    assert_eq!(gate.fresh("me").map(|tools| tools.len()), Some(1));
    assert!(gate.record("new", Read::Unreachable).is_empty());
    assert!(gate
        .record("me", Read::Known(Arc::new(Vec::new())))
        .is_empty());
}

/// Today: the API has no such route, so nothing is listed and nothing runs.
#[tokio::test]
async fn an_api_that_defines_no_hosted_tools_lists_none() {
    let (url, seen) = stub(Arc::new(|_, _, _| {
        (
            404,
            r#"{"success":false,"error":{"code":"NOT_FOUND","message":"Not found"}}"#.to_string(),
        )
    }))
    .await;
    let client = client(&url);
    let gate = HostedToolsGate::new();
    assert!(listing_entries(
        &gate,
        &client,
        &session_for(&client),
        LISTING_BUDGET,
        |_| false
    )
    .await
    .is_empty());
    assert!(find(&gate, &client, &session_for(&client), "echo")
        .await
        .is_none());
    assert!(!listing_changed(&gate, &client, &session_for(&client), |_| false).await);
    // One read, cached: the second lookup asked nothing.
    let reads: Vec<Seen> = seen.lock().unwrap().clone();
    assert_eq!(reads.len(), 1, "{reads:?}");
    assert_eq!(reads[0].method, "GET");
    assert_eq!(reads[0].path, "/mcp/hosted-tools");
}

#[tokio::test]
async fn a_hosted_echo_tool_is_listed_and_runs_through_the_api() {
    let (url, seen) = stub(Arc::new(|method, path, body| match (method, path) {
        ("GET", "/mcp/hosted-tools") => (
            200,
            json!({"success": true, "data": {"tools": [echo_definition("api")]}}).to_string(),
        ),
        ("POST", "/mcp/hosted-tools/echo/call") => (
            200,
            json!({"success": true, "data": {
                "content": [{"type": "text", "text": body["arguments"]["text"]}],
                "structuredContent": {"echoed": body["arguments"]["text"]}
            }})
            .to_string(),
        ),
        _ => (404, "{}".to_string()),
    }))
    .await;
    let client = client(&url);
    let gate = HostedToolsGate::new();

    let entries = listing_entries(
        &gate,
        &client,
        &session_for(&client),
        LISTING_BUDGET,
        |_| false,
    )
    .await;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["name"], "echo");
    assert_eq!(
        entries[0]["inputSchema"]["properties"]["text"]["type"],
        "string"
    );
    // A name the registry already has is never shadowed.
    assert!(listing_entries(
        &gate,
        &client,
        &session_for(&client),
        LISTING_BUDGET,
        |name| name == "echo"
    )
    .await
    .is_empty());

    let tool = find(&gate, &client, &session_for(&client), "echo")
        .await
        .expect("echo is defined");
    let result = run(
        &client,
        &session_for(&client),
        &tool,
        json!({"text": "hello"}),
    )
    .await;
    assert!(!result.is_error, "{result:?}");
    assert_eq!(text_of(&result), ["hello"]);
    assert_eq!(result.structured_content, Some(json!({"echoed": "hello"})));
    let calls: Vec<Seen> = seen
        .lock()
        .unwrap()
        .iter()
        .filter(|seen| seen.method == "POST")
        .cloned()
        .collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].body["arguments"], json!({"text": "hello"}));
}

#[tokio::test]
async fn a_failed_call_is_a_tool_error_with_the_apis_message() {
    let (url, _) = stub(Arc::new(|method, path, _| match (method, path) {
        ("GET", "/mcp/hosted-tools") => (200, json!({"tools": [echo_definition("api")]}).to_string()),
        ("POST", "/mcp/hosted-tools/echo/call") => (
            403,
            json!({"success": false, "error": {"code": "FORBIDDEN", "message": "Not enabled for this workspace"}}).to_string(),
        ),
        _ => (404, "{}".to_string()),
    }))
    .await;
    let client = client(&url);
    let gate = HostedToolsGate::new();
    let tool = find(&gate, &client, &session_for(&client), "echo")
        .await
        .unwrap();
    let result = run(&client, &session_for(&client), &tool, json!({"text": "x"})).await;
    assert!(result.is_error);
    assert_eq!(text_of(&result), ["Not enabled for this workspace"]);
}

#[tokio::test]
async fn a_new_definition_after_a_listing_is_a_change() {
    let (url, _) = stub(Arc::new(|_, _, _| (404, "{}".to_string()))).await;
    let client = client(&url);
    let gate = HostedToolsGate::new();
    // Before any listing, nothing counts as a change.
    assert!(!listing_changed(&gate, &client, &session_for(&client), |_| false).await);
    assert!(listing_entries(
        &gate,
        &client,
        &session_for(&client),
        LISTING_BUDGET,
        |_| false
    )
    .await
    .is_empty());
    assert!(!listing_changed(&gate, &client, &session_for(&client), |_| false).await);
    let identity = crate::domains::slipstream::caller_identity(&client)
        .await
        .unwrap();
    gate.record(
        &identity,
        Read::Known(Arc::new(parse_catalog(
            &json!({"tools": [echo_definition("api")]}),
        ))),
    );
    assert!(listing_changed(&gate, &client, &session_for(&client), |_| false).await);
    assert_eq!(
        listing_entries(
            &gate,
            &client,
            &session_for(&client),
            LISTING_BUDGET,
            |_| false
        )
        .await
        .len(),
        1
    );
    assert!(!listing_changed(&gate, &client, &session_for(&client), |_| false).await);
}

/// A tool whose definition asks for a checkout snapshot, and a stand-in API for it.
async fn snapshot_tool_api() -> (String, Arc<StdMutex<Vec<Seen>>>) {
    let mut definition = echo_definition("api");
    definition["attachments"] = json!(["checkout_snapshot", "something_unknown"]);
    stub(Arc::new(move |method, path, _| match (method, path) {
        ("GET", "/mcp/hosted-tools") => (
            200,
            json!({"tools": [definition.clone(), echo_definition("api")]}).to_string(),
        ),
        ("POST", p) if p.ends_with("/call") => (
            200,
            json!({"content": [{"type": "text", "text": "ok"}]}).to_string(),
        ),
        _ => (404, "{}".to_string()),
    }))
    .await
}

#[test]
fn the_off_switch_reads_like_the_snapshots() {
    for off in ["off", "OFF", " 0 ", "false", "no", "disabled"] {
        assert!(off_by(Some(off)), "{off}");
    }
    for on in ["on", "1", "", "yes"] {
        assert!(!off_by(Some(on)), "{on}");
    }
    assert!(!off_by(None));
}

#[test]
fn a_definition_may_ask_for_a_checkout_snapshot() {
    let tools = parse_catalog(&json!({"tools": [
        {"name": "wants_one", "description": "x", "input_schema": {"type": "object"},
         "attachments": ["checkout_snapshot", "unknown", 7]},
        {"name": "wants_none", "description": "x", "input_schema": {"type": "object"}},
    ]}));
    assert_eq!(tools[0].attachments, vec![Attachment::CheckoutSnapshot]);
    assert!(tools[1].attachments.is_empty());
}

#[tokio::test]
async fn a_call_names_the_workspace_and_carries_a_snapshot_only_when_asked() {
    let (url, seen) = snapshot_tool_api().await;
    let client = client_in_workspace(&url);
    let session = session_for(&client);
    let repo = tempfile::tempdir().unwrap();
    git_here(repo.path(), &["init", "-q"]);
    std::fs::write(repo.path().join("a.txt"), "hello\n").unwrap();
    git_here(repo.path(), &["add", "-A"]);
    git_here(
        repo.path(),
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "-m",
            "first",
        ],
    );
    std::fs::write(repo.path().join("a.txt"), "hello, changed\n").unwrap();
    std::fs::write(repo.path().join(".env"), "TOKEN=abc\n").unwrap();
    session
        .initialize(
            Some(uuid::Uuid::parse_str(WORKSPACE).unwrap()),
            None,
            Some(repo.path().to_string_lossy().to_string()),
            None,
        )
        .await;

    let gate = HostedToolsGate::new();
    let entries = listing_entries(&gate, &client, &session, LISTING_BUDGET, |name| {
        name == "check"
    })
    .await;
    assert_eq!(
        entries.len(),
        1,
        "the repeated name is listed once: {entries:?}"
    );
    let mut tools = parse_catalog(&json!({"tools": [
        {"name": "wants_one", "description": "x", "input_schema": {"type": "object"},
         "attachments": ["checkout_snapshot"]},
        {"name": "wants_none", "description": "x", "input_schema": {"type": "object"}},
    ]}));
    let wants_none = tools.pop().unwrap();
    let wants_one = tools.pop().unwrap();

    let result = run(&client, &session, &wants_one, json!({"text": "x"})).await;
    assert!(!result.is_error, "{result:?}");
    let result = run(&client, &session, &wants_none, json!({"text": "y"})).await;
    assert!(!result.is_error, "{result:?}");

    let seen = seen.lock().unwrap().clone();
    let catalog = seen.iter().find(|s| s.method == "GET").unwrap();
    assert_eq!(catalog.query, format!("workspace_id={WORKSPACE}"));
    let calls: Vec<&Seen> = seen.iter().filter(|s| s.method == "POST").collect();
    assert_eq!(calls.len(), 2);
    for call in &calls {
        assert_eq!(call.body["workspace_id"], WORKSPACE);
    }
    let asked = &calls[0].body;
    assert_eq!(asked["arguments"], json!({"text": "x"}));
    let checkout = &asked["checkout"];
    assert_eq!(checkout["changed"], 1);
    assert_eq!(
        checkout["repo"]["origin_sha256"].as_str().unwrap().len(),
        64
    );
    assert_eq!(
        checkout["repo"]["name"],
        repo.path().file_name().unwrap().to_str().unwrap()
    );
    assert_eq!(checkout["withheld"][0]["path"], ".env");
    assert!(checkout["base_tree"].as_str().unwrap().len() == 40);
    assert_ne!(checkout["base_tree"], checkout["target_tree"]);
    // No content, no path of a change and no URL is in it.
    let text = checkout.to_string();
    assert!(!text.contains("hello, changed") && !text.contains("TOKEN=abc"));
    assert!(calls[1].body.get("checkout").is_none());
}

#[tokio::test]
async fn a_tool_that_needs_a_snapshot_is_not_called_without_one() {
    let (url, seen) = snapshot_tool_api().await;
    let client = client_in_workspace(&url);
    let session = session_for(&client);
    let not_a_repo = tempfile::tempdir().unwrap();
    session
        .initialize(
            None,
            None,
            Some(not_a_repo.path().to_string_lossy().to_string()),
            None,
        )
        .await;
    let tools = parse_catalog(&json!({"tools": [
        {"name": "wants_one", "description": "x", "input_schema": {"type": "object"},
         "attachments": ["checkout_snapshot"]},
    ]}));
    let result = run(&client, &session, &tools[0], json!({})).await;
    assert!(result.is_error);
    let message = text_of(&result).join(" ");
    assert!(message.contains("not inside a git repository"), "{message}");
    assert!(message.contains("yourself"), "{message}");
    assert!(
        seen.lock().unwrap().iter().all(|s| s.method != "POST"),
        "the API was called without the snapshot it needs"
    );
}

// ---- Streams: a call answered over frames ------------------------------------------------------

use futures::{SinkExt, StreamExt};
use mcp_client::frames::{encode_frame, FrameBuffer};
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;

/// What a stand-in service saw of one socket.
#[derive(Debug, Clone)]
struct SocketSeen {
    path: String,
    authorization: Option<String>,
    headers: Vec<(String, String)>,
    hello: Value,
    pack_start: Vec<u8>,
}

/// What a stand-in service does after it has read the hello and the first pack.
#[derive(Clone)]
enum Step {
    Json(Value),
    Pause(u64),
    Output(&'static str),
    /// End the connection.
    Close,
}

/// A stand-in service on this machine: each socket gets `steps` after its hello and pack.
/// Returns its address and what the sockets showed.
#[allow(clippy::result_large_err)]
async fn frames_service(steps: Vec<Step>) -> (String, Arc<StdMutex<Vec<SocketSeen>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}/v1/run", listener.local_addr().unwrap());
    let seen = Arc::new(StdMutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let steps = steps.clone();
            let log = log.clone();
            tokio::spawn(async move {
                let head = Arc::new(StdMutex::new(None));
                let capture = head.clone();
                let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(
                    stream,
                    move |request: &Request, response: Response| {
                        let headers: Vec<(String, String)> = request
                            .headers()
                            .iter()
                            .map(|(name, value)| {
                                (
                                    name.as_str().to_lowercase(),
                                    value.to_str().unwrap_or("").to_string(),
                                )
                            })
                            .collect();
                        *capture.lock().unwrap() =
                            Some((request.uri().path().to_string(), headers));
                        Ok(response)
                    },
                )
                .await
                else {
                    return;
                };
                let (path, headers) = head.lock().unwrap().take().unwrap();
                let mut buffer = FrameBuffer::default();
                let mut frames: Vec<(u8, Vec<u8>)> = Vec::new();
                while frames.len() < 2 {
                    match ws.next().await {
                        Some(Ok(Message::Binary(bytes))) => {
                            buffer.push(&bytes);
                            while let Some(frame) = buffer.next_frame().unwrap() {
                                frames.push(frame);
                            }
                        }
                        _ => return,
                    }
                }
                let authorization = headers
                    .iter()
                    .find(|(name, _)| name == "authorization")
                    .map(|(_, value)| value.clone());
                log.lock().unwrap().push(SocketSeen {
                    path,
                    authorization,
                    headers,
                    hello: serde_json::from_slice(&frames[0].1).unwrap(),
                    pack_start: frames[1].1.iter().take(4).copied().collect(),
                });
                for step in steps {
                    match step {
                        Step::Json(value) => {
                            let frame = encode_frame(b'J', value.to_string().as_bytes());
                            if ws.send(Message::binary(frame)).await.is_err() {
                                return;
                            }
                        }
                        Step::Output(text) => {
                            let frame = encode_frame(b'O', text.as_bytes());
                            let _ = ws.send(Message::binary(frame)).await;
                        }
                        Step::Pause(ms) => tokio::time::sleep(Duration::from_millis(ms)).await,
                        Step::Close => return,
                    }
                }
                // Hold the socket until the client closes it.
                while ws.next().await.is_some() {}
            });
        }
    });
    (url, seen)
}

/// The API's answer to a call: a stream at `url`, described by `request`.
fn stream_answer(url: &str, protocol: &str) -> Value {
    json!({
        "stream": {
            "url": url, "token": "call-token-1", "protocol": protocol,
            "request": {
                "id": "call-1", "command": "run the thing", "cwd": "pkg", "env_names": ["API_TOKEN"],
                "timeout_s": 60, "wait_s": 30, "pipefail": true, "repo_key": "repo-key-1",
            },
        },
        "call": {"id": "c1"},
    })
}

/// An API that defines a streamed tool `check`, which wants a snapshot, and answers its calls
/// with `answer`.
async fn stream_api(answer: Value) -> (String, Arc<StdMutex<Vec<Seen>>>) {
    let mut definition = echo_definition("stream");
    definition["name"] = json!("check");
    definition["attachments"] = json!(["checkout_snapshot"]);
    stub(Arc::new(move |method, path, _| match (method, path) {
        ("GET", "/mcp/hosted-tools") => (200, json!({"tools": [definition.clone()]}).to_string()),
        ("POST", "/mcp/hosted-tools/check/call") => (200, answer.to_string()),
        _ => (404, "{}".to_string()),
    }))
    .await
}

/// A repository with one local change and a credentials file, and a session working in it.
async fn changed_checkout(client: &ContextStreamClient) -> (tempfile::TempDir, SessionManager) {
    let repo = tempfile::tempdir().unwrap();
    git_here(repo.path(), &["init", "-q"]);
    std::fs::write(repo.path().join("a.txt"), "hello\n").unwrap();
    git_here(repo.path(), &["add", "-A"]);
    git_here(
        repo.path(),
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "-m",
            "first",
        ],
    );
    std::fs::write(repo.path().join("a.txt"), "hello, changed\n").unwrap();
    std::fs::write(repo.path().join(".env"), "TOKEN=abc\n").unwrap();
    let session = session_for(client);
    session
        .initialize(
            Some(uuid::Uuid::parse_str(WORKSPACE).unwrap()),
            None,
            Some(repo.path().to_string_lossy().to_string()),
            None,
        )
        .await;
    (repo, session)
}

async fn check_tool(client: &ContextStreamClient, session: &SessionManager) -> HostedTool {
    find(&HostedToolsGate::new(), client, session, "check")
        .await
        .expect("check is defined")
}

fn made_result() -> Value {
    json!({
        "type": "result", "ran": true, "exit": 1, "seconds": 4.5, "host": "a-host-name",
        "tool_result": {
            "content": [{"type": "text", "text": "1 test failed"}],
            "structuredContent": {"exit": 1, "ran": true, "cache": "miss"},
            "isError": true,
        },
    })
}

#[tokio::test]
async fn a_stream_call_runs_over_frames_and_returns_what_the_service_made() {
    let (ws_url, sockets) = frames_service(vec![
        Step::Json(json!({"type": "info", "text": "accepted"})),
        // Output the service was not asked for is not part of the result.
        Step::Output("noise the result does not hold"),
        Step::Json(made_result()),
    ])
    .await;
    let (url, api_seen) = stream_api(stream_answer(&ws_url, "frames/1")).await;
    let client = client_in_workspace(&url);
    let (_repo, session) = changed_checkout(&client).await;
    let tool = check_tool(&client, &session).await;
    assert_eq!(tool.transport, Transport::Stream);

    let result = run(
        &client,
        &session,
        &tool,
        json!({"command": "run the thing"}),
    )
    .await;
    // The command's own result is returned as the service made it.
    assert!(result.is_error, "{result:?}");
    assert_eq!(text_of(&result), ["1 test failed"]);
    assert_eq!(
        result.structured_content,
        Some(json!({"exit": 1, "ran": true, "cache": "miss"}))
    );

    let call = api_seen
        .lock()
        .unwrap()
        .iter()
        .find(|s| s.method == "POST")
        .cloned()
        .unwrap();
    assert_eq!(call.body["arguments"], json!({"command": "run the thing"}));
    // The call to a stream tool carries who the repository is and nothing more of the snapshot.
    let checkout = call.body["checkout"].as_object().unwrap();
    assert_eq!(
        checkout.keys().collect::<Vec<_>>(),
        ["repo"],
        "{checkout:?}"
    );
    assert_eq!(checkout["repo"].as_object().unwrap().len(), 2);

    let sockets = sockets.lock().unwrap().clone();
    assert_eq!(sockets.len(), 1);
    let socket = &sockets[0];
    assert_eq!(socket.path, "/v1/run");
    // The socket gets the call's token, never the API key.
    assert_eq!(socket.authorization.as_deref(), Some("Bearer call-token-1"));
    assert!(
        socket
            .headers
            .iter()
            .all(|(_, value)| !value.contains("hosted-tools-test-key")),
        "{:?}",
        socket.headers
    );
    assert_eq!(socket.pack_start, b"PACK");
    // The hello: what the API decided, the checkout, nothing else.
    let hello = &socket.hello;
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["v"], 1);
    assert_eq!(hello["id"], "call-1");
    assert_eq!(hello["repo_key"], "repo-key-1");
    assert_eq!(hello["cwd"], "pkg");
    assert_eq!(hello["command"], "run the thing");
    assert_eq!(hello["timeout_s"], 60);
    assert_eq!(hello["wait_s"], 30);
    assert_eq!(hello["tool_result"], true);
    assert_eq!(hello["target_tree"].as_str().unwrap().len(), 40);
    assert_ne!(hello["target_tree"], hello["base_tree"]);
    assert_eq!(hello["overlay"].as_array().unwrap().len(), 1);
    assert_eq!(hello["overlay"][0][2], "a.txt");
    // A variable whose name looks like a credential is not forwarded, set or not.
    assert_eq!(hello["env"], json!({}));
    let text = hello.to_string();
    assert!(!text.contains("TOKEN=abc") && !text.contains("hello, changed"));
    assert!(hello["root"].as_str().unwrap().len() > 1);
}

#[tokio::test]
async fn a_run_the_service_declines_is_handed_back_to_the_caller() {
    let (ws_url, _) = frames_service(vec![Step::Json(
        json!({"type": "result", "ran": false, "exit": 2, "refused": "something internal", "host": "gateway"}),
    )])
    .await;
    let (url, _) = stream_api(stream_answer(&ws_url, "frames/1")).await;
    let client = client_in_workspace(&url);
    let (_repo, session) = changed_checkout(&client).await;
    let tool = check_tool(&client, &session).await;
    let result = run(
        &client,
        &session,
        &tool,
        json!({"command": "run the thing"}),
    )
    .await;
    assert!(result.is_error);
    let message = text_of(&result).join(" ");
    assert!(
        message.contains("Run it yourself: run the thing (from the folder pkg)"),
        "{message}"
    );
    // What the service said of why is not passed on.
    assert!(
        !message.contains("something internal") && !message.contains("gateway"),
        "{message}"
    );
    assert_eq!(result.structured_content, Some(json!({"ran": false})));
}

#[tokio::test]
async fn a_service_that_cannot_be_reached_or_ends_early_is_handed_back_the_same_way() {
    // Nothing listens at the first address; the second service ends the call without a result.
    let closed = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        format!("ws://{}/v1/run", listener.local_addr().unwrap())
    };
    let (ends, _) = frames_service(vec![Step::Json(json!({"type": "info"})), Step::Close]).await;
    for (address, expect) in [
        (closed, "could not be reached"),
        (ends, "ended before a result"),
    ] {
        let answer = stream_answer(&address, "frames/1");
        let (url, _) = stream_api(answer).await;
        let client = client_in_workspace(&url);
        let (_repo, session) = changed_checkout(&client).await;
        let tool = check_tool(&client, &session).await;
        let result = tokio::time::timeout(
            Duration::from_secs(30),
            run(
                &client,
                &session,
                &tool,
                json!({"command": "run the thing"}),
            ),
        )
        .await
        .expect("the call ends");
        assert!(result.is_error);
        let message = text_of(&result).join(" ");
        assert!(message.contains(expect), "{message}");
        assert!(
            message.contains("Run it yourself: run the thing"),
            "{message}"
        );
    }
}

#[tokio::test]
async fn a_stream_of_a_kind_this_client_does_not_know_is_not_connected_to() {
    let (ws_url, sockets) = frames_service(vec![Step::Json(made_result())]).await;
    let (url, _) = stream_api(stream_answer(&ws_url, "ndjson/9")).await;
    let client = client_in_workspace(&url);
    let (_repo, session) = changed_checkout(&client).await;
    let tool = check_tool(&client, &session).await;
    let result = run(
        &client,
        &session,
        &tool,
        json!({"command": "run the thing"}),
    )
    .await;
    assert!(result.is_error);
    assert!(
        text_of(&result)
            .join(" ")
            .contains("Run it yourself: run the thing"),
        "{result:?}"
    );
    assert!(sockets.lock().unwrap().is_empty());
    // An answer with no stream at all is an error the caller reads.
    let (url, _) = stream_api(json!({"call": {"id": "c1"}})).await;
    let client = client_in_workspace(&url);
    let (_repo, session) = changed_checkout(&client).await;
    let tool = check_tool(&client, &session).await;
    let result = run(&client, &session, &tool, json!({})).await;
    assert!(result.is_error);
    assert!(text_of(&result)[0].contains("named no stream"));
}

/// A client that records what is sent to it.
#[derive(Default)]
struct Recording {
    sent: StdMutex<Vec<(String, Value)>>,
}

#[async_trait::async_trait]
impl crate::peer::PeerChannel for Recording {
    async fn notify(&self, method: &str, params: Value) {
        self.sent.lock().unwrap().push((method.to_string(), params));
    }

    async fn request(
        &self,
        _method: &str,
        _params: Value,
        _timeout: Duration,
    ) -> std::result::Result<Value, String> {
        Err("not asked".to_string())
    }
}

#[tokio::test]
async fn heartbeats_become_progress_and_pass_on_nothing_the_service_said() {
    let (ws_url, _) = frames_service(vec![
        Step::Json(json!({"type": "info", "text": "accepted by host-7"})),
        Step::Pause(30),
        Step::Json(json!({"type": "info", "text": "still going on host-7"})),
        Step::Pause(30),
        Step::Json(json!({"type": "info", "text": "again"})),
        Step::Json(made_result()),
    ])
    .await;
    let (url, _) = stream_api(stream_answer(&ws_url, "frames/1")).await;
    let client = client_in_workspace(&url);
    let (_repo, session) = changed_checkout(&client).await;
    let tool = check_tool(&client, &session).await;
    let channel = Arc::new(Recording::default());
    let peer = crate::peer::CallPeer::new(channel.clone(), Some(json!("progress-1")), false);
    let result = crate::peer::run_with_call_peer(
        peer,
        run(
            &client,
            &session,
            &tool,
            json!({"command": "run the thing"}),
        ),
    )
    .await;
    assert_eq!(text_of(&result), ["1 test failed"]);
    let sent = channel.sent.lock().unwrap().clone();
    assert_eq!(sent.len(), 3, "{sent:?}");
    let mut last = 0.0;
    for (method, params) in &sent {
        assert_eq!(method, "notifications/progress");
        assert_eq!(params["progressToken"], "progress-1");
        let progress = params["progress"].as_f64().unwrap();
        assert!(progress > last, "{sent:?}");
        last = progress;
        assert_eq!(params["message"], "still running");
        assert!(!params.to_string().contains("host-7"));
    }
}

#[tokio::test]
async fn a_call_given_up_on_is_picked_up_by_the_same_call_asked_again() {
    let (ws_url, sockets) = frames_service(vec![Step::Json(made_result())]).await;
    let (url, api_seen) = stream_api(stream_answer(&ws_url, "frames/1")).await;
    let client = client_in_workspace(&url);
    let (_repo, session) = changed_checkout(&client).await;
    let tool = check_tool(&client, &session).await;
    let arguments = json!({"command": "run the thing", "note": "given-up"});

    // The client cancelled request 4101 while the run went on; its result is kept.
    crate::peer::note_cancelled(&json!(4101));
    let channel = Arc::new(Recording::default());
    let peer = crate::peer::CallPeer::new(channel, None, false).with_request_id(Some(&json!(4101)));
    let first =
        crate::peer::run_with_call_peer(peer, run(&client, &session, &tool, arguments.clone()))
            .await;
    assert_eq!(text_of(&first), ["1 test failed"]);

    // Asked again, it is the same result at once: no second run, no second call to the API.
    let again = run(&client, &session, &tool, arguments.clone()).await;
    assert_eq!(text_of(&again), ["1 test failed"]);
    assert_eq!(again.structured_content, first.structured_content);
    assert_eq!(sockets.lock().unwrap().len(), 1);
    let posts = |seen: &Arc<StdMutex<Vec<Seen>>>| {
        seen.lock()
            .unwrap()
            .iter()
            .filter(|s| s.method == "POST")
            .count()
    };
    assert_eq!(posts(&api_seen), 1);

    // It is handed out once: a third ask runs again.
    let third = run(&client, &session, &tool, arguments).await;
    assert_eq!(text_of(&third), ["1 test failed"]);
    assert_eq!(sockets.lock().unwrap().len(), 2);
    assert_eq!(posts(&api_seen), 2);
}

#[tokio::test]
async fn a_call_nobody_gave_up_on_is_run_each_time_and_a_changed_checkout_is_a_new_call() {
    let (ws_url, sockets) = frames_service(vec![Step::Json(made_result())]).await;
    let (url, _) = stream_api(stream_answer(&ws_url, "frames/1")).await;
    let client = client_in_workspace(&url);
    let (repo, session) = changed_checkout(&client).await;
    let tool = check_tool(&client, &session).await;
    let arguments = json!({"command": "run the thing", "note": "not-given-up"});

    run(&client, &session, &tool, arguments.clone()).await;
    run(&client, &session, &tool, arguments.clone()).await;
    assert_eq!(sockets.lock().unwrap().len(), 2, "each ask is its own run");

    // Given up on, kept: but only for the checkout it was run on.
    crate::peer::note_cancelled(&json!(4102));
    let peer = crate::peer::CallPeer::new(Arc::new(Recording::default()), None, false)
        .with_request_id(Some(&json!(4102)));
    crate::peer::run_with_call_peer(peer, run(&client, &session, &tool, arguments.clone())).await;
    assert_eq!(sockets.lock().unwrap().len(), 3);
    std::fs::write(repo.path().join("a.txt"), "changed once more\n").unwrap();
    run(&client, &session, &tool, arguments).await;
    assert_eq!(sockets.lock().unwrap().len(), 4);
}

#[tokio::test]
async fn a_run_without_a_made_result_streams_its_output_and_ends_with_its_status() {
    let (ws_url, sockets) = frames_service(vec![
        Step::Output("line one\n"),
        Step::Pause(20),
        Step::Output("line two\n"),
        Step::Json(json!({"type": "result", "ran": true, "exit": 3, "seconds": 1.5})),
    ])
    .await;
    let (url, _) = stream_api(stream_answer(&ws_url, "frames/1")).await;
    let client = client_in_workspace(&url);
    let (_repo, session) = changed_checkout(&client).await;
    let tool = check_tool(&client, &session).await;
    let pending = begin_snapshot(&session, &tool).await.unwrap().unwrap();
    let arguments = json!({"command": "run the thing"});
    let data = call_api(
        &client,
        &session,
        &tool,
        &arguments,
        Some(pending.identity.clone()),
    )
    .await
    .unwrap();
    let snapshot = pending.finish(&tool.name).await.unwrap();
    let call = stream_call(&data).unwrap();
    assert_eq!((call.command, call.cwd), ("run the thing", "pkg"));

    let (events, mut inbox) = tokio::sync::mpsc::unbounded_channel();
    let frame = drive_stream(&call, &snapshot, false, events).await.unwrap();
    assert_eq!(
        (frame["ran"].clone(), frame["exit"].clone()),
        (json!(true), json!(3))
    );
    let mut output = Vec::new();
    while let Ok(event) = inbox.try_recv() {
        if let Event::Output(bytes) = event {
            output.extend(bytes);
        }
    }
    assert_eq!(String::from_utf8(output).unwrap(), "line one\nline two\n");
    // Asked for no made result: the hello does not say `tool_result`.
    let sockets = sockets.lock().unwrap().clone();
    assert_eq!(sockets.len(), 1);
    assert!(
        sockets[0].hello.get("tool_result").is_none(),
        "{:?}",
        sockets[0].hello
    );
}

#[tokio::test]
async fn a_stream_that_cannot_be_used_says_why_before_any_socket_is_opened() {
    assert!(stream_call(&json!({})).unwrap_err().contains("no stream"));
    assert!(
        stream_call(&json!({"stream": {"url": "wss://x/y", "token": "t"}}))
            .unwrap_err()
            .contains("described no run")
    );
    let (ws_url, sockets) = frames_service(vec![Step::Json(made_result())]).await;
    let (url, _) = stream_api(stream_answer(&ws_url, "ndjson/9")).await;
    let client = client_in_workspace(&url);
    let (_repo, session) = changed_checkout(&client).await;
    let tool = check_tool(&client, &session).await;
    let pending = begin_snapshot(&session, &tool).await.unwrap().unwrap();
    let data = call_api(
        &client,
        &session,
        &tool,
        &json!({"command": "x"}),
        Some(pending.identity.clone()),
    )
    .await
    .unwrap();
    let snapshot = pending.finish(&tool.name).await.unwrap();
    let call = stream_call(&data).unwrap();
    let (events, _inbox) = tokio::sync::mpsc::unbounded_channel();
    let reason = drive_stream(&call, &snapshot, false, events)
        .await
        .unwrap_err();
    assert!(
        reason.contains("kind this client does not know"),
        "{reason}"
    );
    assert!(sockets.lock().unwrap().is_empty());
    // A tool that asks for no snapshot takes none.
    let plain = parse_catalog(&json!({"tools": [
        {"name": "plain", "description": "x", "input_schema": {"type": "object"}},
    ]}));
    assert!(begin_snapshot(&session, &plain[0]).await.unwrap().is_none());
}

#[test]
fn a_definition_carries_its_command_hints() {
    let tools = parse_catalog(&json!({"tools": [
        {"name": "takes", "description": "x", "input_schema": {"type": "object"},
         "command_hints": {"argument": "command", "patterns": ["^cargo test$"]}},
        {"name": "takes_none", "description": "x", "input_schema": {"type": "object"}},
        {"name": "too_broad", "description": "x", "input_schema": {"type": "object"},
         "command_hints": {"argument": "command", "patterns": [".*"]}},
    ]}));
    assert!(tools[0]
        .command_hints
        .as_ref()
        .unwrap()
        .matches("cargo test"));
    assert!(tools[1].command_hints.is_none());
    assert!(tools[2].command_hints.is_none());
}

#[tokio::test]
async fn a_snapshot_that_cannot_be_finished_ends_the_call_though_it_may_already_have_been_made() {
    let (ws_url, sockets) = frames_service(vec![Step::Json(made_result())]).await;
    let (url, api_seen) = stream_api(stream_answer(&ws_url, "frames/1")).await;
    let client = client_in_workspace(&url);
    let (repo, session) = changed_checkout(&client).await;
    // A change over the per-file limit: the snapshot begins, then cannot be finished.
    std::fs::write(repo.path().join("huge.bin"), vec![0u8; 17 << 20]).unwrap();
    let tool = check_tool(&client, &session).await;
    let result = run(
        &client,
        &session,
        &tool,
        json!({"command": "run the thing"}),
    )
    .await;
    assert!(result.is_error);
    let message = text_of(&result).join(" ");
    assert!(
        message.contains("huge.bin") && message.contains("limit"),
        "{message}"
    );
    assert!(message.contains("yourself"), "{message}");
    // Nothing was sent over a socket, and the API was asked at most once.
    assert!(sockets.lock().unwrap().is_empty());
    let posts = api_seen
        .lock()
        .unwrap()
        .iter()
        .filter(|s| s.method == "POST")
        .count();
    assert!(posts <= 1, "{posts}");
}
