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
    assert_eq!(gate.record("me", known, None).len(), 1);
    assert_eq!(gate.record("me", Read::Unreachable, None).len(), 1);
    assert_eq!(gate.fresh("me", None).map(|tools| tools.len()), Some(1));
    assert!(gate.record("new", Read::Unreachable, None).is_empty());
    assert!(gate
        .record("me", Read::Known(Arc::new(Vec::new())), None)
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
        None,
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
    let (ws_url, _) = frames_service(vec![Step::Json(json!({
        "type": "result", "ran": false, "exit": 75, "host": "gateway"
    }))])
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
    assert_eq!(
        text_of(&result),
        ["check was not run remotely: the service declined to run it. Run it yourself: run the thing (from the folder pkg)"]
    );
    assert_eq!(result.structured_content, Some(json!({"ran": false})));
}

#[tokio::test]
async fn a_declined_run_includes_the_workers_reason_in_text_and_structured_content() {
    let reason = "the session runs Node 26 and this host runs Node 22 or 24 or 20, so it runs in the session";
    let (ws_url, _) = frames_service(vec![Step::Json(json!({
        "type": "result", "ran": false, "exit": 75, "refused": format!("\0{reason}\n"),
        "refused_code": "node_version_mismatch", "host": "gateway"
    }))])
    .await;
    let mut answer = stream_answer(&ws_url, "frames/1");
    answer["stream"]["request"]["command"] = json!("npx tsc --noEmit");
    answer["stream"]["request"]["cwd"] = json!("");
    let (url, _) = stream_api(answer).await;
    let client = client_in_workspace(&url);
    let (_repo, session) = changed_checkout(&client).await;
    let tool = check_tool(&client, &session).await;
    let result = run(
        &client,
        &session,
        &tool,
        json!({"command": "npx tsc --noEmit"}),
    )
    .await;
    assert!(result.is_error);
    assert_eq!(
        text_of(&result),
        ["check was not run remotely: the remote machine can't match this one (the session runs Node 26 and this host runs Node 22 or 24 or 20). Run it yourself: npx tsc --noEmit"]
    );
    assert_eq!(
        result.structured_content,
        Some(
            json!({"ran": false, "declined_reason": reason, "refused_code": "node_version_mismatch"})
        )
    );
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
    let frame = drive_stream(&call, &snapshot, Delivery::Streamed, events)
        .await
        .unwrap();
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
async fn the_hello_names_how_the_output_is_taken_and_describes_the_environment() {
    let (ws_url, sockets) = frames_service(vec![Step::Json(
        json!({"type": "result", "ran": true, "exit": 0}),
    )])
    .await;
    let mut answer = stream_answer(&ws_url, "frames/1");
    // Names the API lists are asked about; anything that is not a name is not.
    answer["stream"]["request"]["versions"] =
        json!(["no-such-tool-4f2c1", "Not A Name", "no-such-tool-4f2c1"]);
    let (url, _) = stream_api(answer).await;
    let client = client_in_workspace(&url);
    let (_repo, session) = changed_checkout(&client).await;
    let tool = check_tool(&client, &session).await;
    let pending = begin_snapshot(&session, &tool).await.unwrap().unwrap();
    let data = call_api(
        &client,
        &session,
        &tool,
        &json!({"command": "run the thing"}),
        Some(pending.identity.clone()),
    )
    .await
    .unwrap();
    let snapshot = pending.finish(&tool.name).await.unwrap();
    let call = stream_call(&data).unwrap();
    let (events, _inbox) = tokio::sync::mpsc::unbounded_channel();
    drive_stream(&call, &snapshot, Delivery::Buffered, events)
        .await
        .unwrap();

    let sockets = sockets.lock().unwrap().clone();
    assert_eq!(sockets.len(), 1);
    let hello = &sockets[0].hello;
    assert_eq!(hello["buffered"], true);
    assert!(hello.get("tool_result").is_none(), "{hello:?}");
    let environment = &hello["environment"];
    assert_eq!(environment["platform"]["os"], std::env::consts::OS);
    assert_eq!(environment["platform"]["arch"], std::env::consts::ARCH);
    assert!(environment["cpus"].as_u64().unwrap() >= 1);
    assert!(environment["root"].is_boolean());
    assert!(environment["tz"].is_string());
    assert_eq!(environment["versions"], json!({"no-such-tool-4f2c1": null}));
    // Machine facts and tool versions only.
    let mut keys: Vec<_> = environment.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    assert_eq!(keys, ["cpus", "platform", "root", "tz", "versions"]);
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
    let reason = drive_stream(&call, &snapshot, Delivery::Streamed, events)
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

#[tokio::test]
async fn remote_checks_setting_refreshes_the_callers_listing_and_hook_hints() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let enabled = AtomicBool::new(false);
    let (url, seen) = stub(Arc::new(move |method, path, body| {
        let value = match (method, path) {
            ("PUT", "/mcp/hosted-tools/enabled") => {
                let on = body["enabled"].as_bool().unwrap();
                enabled.store(on, Ordering::SeqCst);
                json!({"tool":"check", "enabled":on, "available":true, "more_engines":true})
            }
            ("GET", "/mcp/hosted-tools") if enabled.load(Ordering::SeqCst) => {
                json!({"tools":[{"name":"check","description":"Remote checks","input_schema":{"type":"object"},
                    "command_hints":{"argument":"command","patterns":["^cargo check$"]}}]})
            }
            ("GET", "/mcp/hosted-tools") => json!({"tools":[]}),
            _ => return (404, "{}".into())
        };
        (200, value.to_string())
    })).await;
    let config = mcp_types::Config {
        api_url: url.clone(),
        api_key: Some(uuid::Uuid::new_v4().to_string()),
        default_workspace_id: Some(WORKSPACE.parse().unwrap()),
        ..TestFixtures::test_config()
    };
    let client = ContextStreamClient::new(config.clone());
    let session = SessionManager::new(client.clone(), config);
    let gate = HostedToolsGate::global();
    let identity = crate::domains::slipstream::caller_identity(&client)
        .await
        .unwrap();
    let listing = || listing_entries(&gate, &client, &session, LISTING_BUDGET, |_| false);
    assert!(listing().await.is_empty());
    for on in [true, false] {
        let result = crate::domains::remote_checks::session_action(
            &client,
            &session,
            None,
            Some(on),
            Some(true),
        )
        .await
        .unwrap();
        assert!(!result.is_error);
        assert!(listing_changed(&gate, &client, &session, |_| false).await);
        assert_eq!(
            listing().await.iter().any(|tool| tool["name"] == "check"),
            on
        );
        assert!(!listing_changed(&gate, &client, &session, |_| false).await);
        let hints = command_hints::read(&identity, &url).unwrap();
        assert_eq!(hints.tool_for("cargo check").is_some(), on);
        let mut stale = hints;
        stale.settings_changed = None;
        assert!(!command_hints::write(&stale).unwrap());
    }
    let requests = seen.lock().unwrap();
    let changes: Vec<_> = requests
        .iter()
        .filter(|r| r.method == "PUT")
        .map(|r| &r.body)
        .collect();
    assert_eq!(
        changes,
        [
            &json!({"enabled":true,"more_engines":true}),
            &json!({"enabled":false,"more_engines":true})
        ]
    );
}

#[tokio::test]
async fn remote_checks_credit_refusal_is_cached_for_only_that_caller_and_endpoint() {
    let (url, seen) = stub(Arc::new(|_, _, _| (402, json!({"error":{"code":"INSUFFICIENT_CREDITS","details":{"available":2,"required":20}}}).to_string()))).await;
    let client = client_in_workspace(&url);
    let session = session_for(&client);
    let mut check = parse_catalog(&json!({"tools":[echo_definition("api")]})).remove(0);
    check.name = "check".into();
    for _ in 0..2 {
        let result = run(&client, &session, &check, json!({})).await;
        assert!(result.is_error);
        assert_eq!(text_of(&result), ["Your account is out of Context Credits for remote checks. Run the command locally. Add credits at https://contextstream.io/account/billing"]);
    }
    assert_eq!(seen.lock().unwrap().len(), 1);
    let mut config = client.config().await;
    config.api_key = Some(uuid::Uuid::new_v4().to_string());
    let other = ContextStreamClient::new(config);
    assert!(
        run(&other, &session_for(&other), &check, json!({}))
            .await
            .is_error
    );
    assert_eq!(seen.lock().unwrap().len(), 2);
    let (url2, others) = stub(Arc::new(|_, _, _| {
        (
            402,
            json!({"error":{"code":"PAYMENT_REQUIRED","message":"Payment required"}}).to_string(),
        )
    }))
    .await;
    let same_account = client_in_workspace(&url2);
    let result = run(
        &same_account,
        &session_for(&same_account),
        &check,
        json!({}),
    )
    .await;
    assert_eq!(text_of(&result), ["Payment required"]);
    assert_eq!(others.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn remote_checks_legacy_settings_use_the_separate_engine_route() {
    let (url, seen) = stub(Arc::new(|method, path, body| {
        let value = match (method, path) {
            ("GET" | "PUT", "/mcp/hosted-tools/enabled") => {
                json!({"tool":"check","enabled":true,"available":true})
            }
            ("GET", "/mcp/hosted-tools/more-engines") => json!({"enabled":false}),
            ("PUT", "/mcp/hosted-tools/more-engines") => json!({"enabled":body["enabled"]}),
            ("GET", "/mcp/hosted-tools") => json!({"tools":[]}),
            _ => return (404, "{}".into()),
        };
        (200, value.to_string())
    }))
    .await;
    let client = client_in_workspace(&url);
    let read = crate::domains::remote_checks::settings(&client, None, None, None)
        .await
        .unwrap();
    assert_eq!(read.more_engines, Some(false));
    let changed = crate::domains::remote_checks::settings(&client, None, Some(true), Some(true))
        .await
        .unwrap();
    assert_eq!(changed.more_engines, Some(true));
    let engine_only = crate::domains::remote_checks::settings(&client, None, None, Some(false))
        .await
        .unwrap();
    assert!(engine_only.enabled);
    assert_eq!(engine_only.more_engines, Some(false));
    let seen = seen.lock().unwrap();
    let puts: Vec<_> = seen
        .iter()
        .filter(|r| r.method == "PUT")
        .map(|r| (r.path.as_str(), &r.body))
        .collect();
    assert_eq!(
        puts,
        [
            (
                "/mcp/hosted-tools/enabled",
                &json!({"enabled":true,"more_engines":true})
            ),
            ("/mcp/hosted-tools/more-engines", &json!({"enabled":true})),
            ("/mcp/hosted-tools/more-engines", &json!({"enabled":false})),
        ]
    );
}

#[tokio::test]
async fn remote_checks_hosted_status_uses_the_projects_indexed_markers() {
    let id = uuid::Uuid::new_v4();
    let (url, _) = stub(Arc::new(move |_, path, _| {
        let data = if path == "/mcp/hosted-tools/enabled" {
            json!({"tool":"check","enabled":true,"available":true,"more_engines":false})
        } else if path == format!("/projects/{id}/files") {
            json!({"files":[{"path":"package.json"},{"path":"pkg/Pod.csproj"},{"path":"vendor/Gemfile"}],"total":3})
        } else { return (404,"{}".into()); };
        (200,data.to_string())
    })).await;
    let mut config = client_in_workspace(&url).config().await;
    config.is_http_transport = true;
    let client = ContextStreamClient::new(config);
    let result = crate::domains::remote_checks::session_action(
        &client,
        &session_for(&client),
        Some(id),
        None,
        None,
    )
    .await
    .unwrap();
    let text = text_of(&result).join("\n");
    assert!(
        text.contains(
            "This project: TypeScript and JavaScript (runs remotely) · .NET (runs remotely with extra engines)"
        ),
        "{text}"
    );
}

#[test]
fn diagnostic_header_orders_new_failures_first_cites_context_and_cuts_bytes_by_at_least_50_percent()
{
    let mut raw_lines = Vec::new();
    for i in 0..30 {
        raw_lines.push(format!(
            "   Compiling crate-{i} v0.1.0 (/work/repo/crates/c{i})"
        ));
    }
    for i in 0..20 {
        raw_lines.push(format!("test module::passing_case_{i} ... ok"));
    }
    for i in 0..8 {
        raw_lines.push(format!(
            "error[E0425]: cannot find value `old_var_{i}` in src/legacy_{i}.rs:10:5"
        ));
    }
    raw_lines
        .push("error[E0308]: mismatched types at src/auth.rs:42:9 in verify_token".to_string());
    raw_lines.push("[exit 101, 4.2 s]".to_string());
    let raw_stderr = raw_lines.join("\n");

    let old_failures: Vec<Value> = (0..8)
        .map(|i| {
            json!({
                "file": format!("src/legacy_{i}.rs"),
                "line": 10,
                "col": 5,
                "error_code": "E0425",
                "message": format!("cannot find value `old_var_{i}`"),
            })
        })
        .collect();
    let new_failure = json!({
        "file": "src/auth.rs",
        "line": 42,
        "col": 9,
        "symbol": "verify_token",
        "error_code": "E0308",
        "message": "mismatched types",
        "callers": ["routes::login"],
        "definitions": ["JwtClaims"],
    });
    let frame = json!({
        "type": "result",
        "ran": true,
        "exit": 101,
        "tool_result": {
            "content": [{"type": "text", "text": raw_stderr}],
            "structuredContent": {
                "exit": 101,
                "ran": true,
                "failures": [new_failure],
                "failing_on_base": {
                    "known": true,
                    "base_exit": 101,
                    "old": old_failures,
                    "new": [new_failure],
                },
                "diagnostics": {
                    "new_failures_count": 1,
                    "base_failures_suppressed": 8,
                    "new_failures": [new_failure],
                    "commits": [{"sha": "abc1234", "message": "refactor verify_token", "matched_by": "symbol"}],
                    "decisions": [{"id": "00000000-0000-0000-0000-000000000001", "summary": "Require typed claims in auth"}],
                    "lessons": [{"id": "00000000-0000-0000-0000-000000000002", "title": "Avoid E0308 in auth", "prevention": "Borrow &str"}],
                }
            },
            "isError": true
        }
    });

    let Streamed::Ran(result) = interpret("check", "cargo test", "", &frame) else {
        panic!("expected Streamed::Ran");
    };
    let text = text_of(&result).join("\n");
    assert!(
        text.starts_with("[diagnostics: 1 new failure(s), 8 base failure(s) suppressed]"),
        "{text}"
    );
    assert!(
        text.contains("commits: abc1234 refactor verify_token"),
        "{text}"
    );
    assert!(
        text.contains("decisions: Require typed claims in auth"),
        "{text}"
    );
    assert!(
        text.contains("lessons: Avoid E0308 in auth: Borrow &str"),
        "{text}"
    );
    assert!(
        text.contains("src/auth.rs:42:9 [E0308] (verify_token)"),
        "{text}"
    );
    assert!(
        !text.contains("old_var_0"),
        "base failures must be suppressed: {text}"
    );
    assert!(
        !text.contains("Compiling crate-0"),
        "passing build noise must be omitted: {text}"
    );
    assert!(
        text.len() * 2 <= raw_stderr.len(),
        "expected >=50% reduction: formatted={} bytes vs raw={} bytes\n{text}",
        text.len(),
        raw_stderr.len()
    );
}

#[test]
fn interpret_preserves_exit_75_cause_metadata_on_declined_frame() {
    let frame = json!({
        "type": "result",
        "ran": false,
        "exit": 75,
        "refused": "no such engine here",
        "refused_code": "engine_unavailable",
        "refused_class": "host",
        "detail": "no such engine here (ruby)",
    });
    let Streamed::NotRun(result) = interpret("check", "bundle exec rspec", "", &frame) else {
        panic!("expected Streamed::NotRun");
    };
    let sc = result.structured_content.expect("structured_content");
    assert_eq!(sc["refused_code"], "engine_unavailable");
    assert_eq!(sc["refused_class"], "host");
    assert_eq!(sc["detail"], "no such engine here (ruby)");
}

#[derive(Debug, Clone)]
struct StubReq {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Value,
}

async fn stub_with_headers(
    respond: Arc<
        dyn Fn(
                &StubReq,
            )
                -> std::pin::Pin<Box<dyn std::future::Future<Output = (u16, String)> + Send>>
            + Send
            + Sync,
    >,
) -> (String, Arc<StdMutex<Vec<StubReq>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(StdMutex::new(Vec::new()));
    let log = seen.clone();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let respond = respond.clone();
            let log = log.clone();
            tokio::spawn(async move {
                let mut buffer = Vec::new();
                let mut chunk = [0u8; 8192];
                let header_end = loop {
                    let read = socket.read(&mut chunk).await.unwrap_or(0);
                    if read == 0 {
                        return;
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                    if let Some(position) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                        break position + 4;
                    }
                };
                let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
                let headers: Vec<(String, String)> = head
                    .lines()
                    .skip(1)
                    .filter_map(|line| {
                        let (k, v) = line.split_once(':')?;
                        Some((k.trim().to_ascii_lowercase(), v.trim().to_string()))
                    })
                    .collect();
                let length = headers
                    .iter()
                    .find(|(k, _)| k == "content-length")
                    .and_then(|(_, v)| v.parse::<usize>().ok())
                    .unwrap_or(0);
                while buffer.len() < header_end + length {
                    let read = socket.read(&mut chunk).await.unwrap_or(0);
                    if read == 0 {
                        break;
                    }
                    buffer.extend_from_slice(&chunk[..read]);
                }
                let mut parts = head.lines().next().unwrap_or("").split_whitespace();
                let method = parts.next().unwrap_or("").to_string();
                let target = parts.next().unwrap_or("").to_string();
                let path = target.split('?').next().unwrap_or("").to_string();
                let req = StubReq {
                    method,
                    path: path.strip_prefix("/api/v1").unwrap_or(&path).to_string(),
                    headers,
                    body: serde_json::from_slice(&buffer[header_end..]).unwrap_or(Value::Null),
                };
                let (status, body) = respond(&req).await;
                log.lock().unwrap().push(req);
                let response = format!(
                    "HTTP/1.1 {status} OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.shutdown().await;
            });
        }
    });
    (url, seen)
}

#[tokio::test]
async fn run_enriches_failing_stream_from_diagnose_endpoint_without_prestuffed_frame_diagnostics() {
    let mut raw_lines = Vec::new();
    for i in 0..30 {
        raw_lines.push(format!(
            "   Compiling crate-{i} v0.1.0 (/work/repo/crates/c{i})"
        ));
    }
    for i in 0..8 {
        raw_lines.push(format!(
            "error[E0425]: cannot find value `old_var_{i}` in src/legacy_{i}.rs:10:5"
        ));
    }
    raw_lines
        .push("error[E0308]: mismatched types at src/auth.rs:42:9 in verify_token".to_string());
    raw_lines.push("[exit 101, 4.2 s]".to_string());
    let raw_stderr = raw_lines.join("\n");

    let old_failures: Vec<Value> = (0..8)
        .map(|i| {
            json!({
                "file": format!("src/legacy_{i}.rs"),
                "line": 10,
                "col": 5,
                "error_code": "E0425",
                "message": format!("cannot find value `old_var_{i}`"),
            })
        })
        .collect();
    let new_failure = json!({
        "file": "src/auth.rs",
        "line": 42,
        "col": 9,
        "symbol": "verify_token",
        "error_code": "E0308",
        "message": "mismatched types",
    });

    // J frame emitted by mcd.py/worker.ts has NO "diagnostics" key!
    let j_frame = json!({
        "type": "result",
        "ran": true,
        "exit": 101,
        "failures": [new_failure],
        "failing_on_base": {
            "known": true,
            "base_exit": 101,
            "old": old_failures,
            "new": [new_failure],
        },
        "tool_result": {
            "content": [{"type": "text", "text": raw_stderr}],
            "structuredContent": {
                "exit": 101,
                "ran": true,
                "failures": [new_failure],
                "failing_on_base": {
                    "known": true,
                    "base_exit": 101,
                    "old": old_failures,
                    "new": [new_failure],
                },
            },
            "isError": true,
        }
    });
    assert!(j_frame.get("diagnostics").is_none());
    assert!(j_frame
        .pointer("/tool_result/structuredContent/diagnostics")
        .is_none());

    let (ws_url, _) = frames_service(vec![Step::Json(j_frame)]).await;
    let mut def = echo_definition("stream");
    def["name"] = json!("check");
    def["attachments"] = json!(["checkout_snapshot"]);
    let mut answer = stream_answer(&ws_url, "frames/1");
    answer["call"]["id"] = json!("call-diag-1");

    let (url, seen) = stub_with_headers(Arc::new(move |req| {
        let def = def.clone();
        let answer = answer.clone();
        let method = req.method.clone();
        let path = req.path.clone();
        let headers = req.headers.clone();
        let body = req.body.clone();
        Box::pin(async move {
            match (method.as_str(), path.as_str()) {
                ("GET", "/mcp/hosted-tools") => {
                    (200, json!({"tools": [def]}).to_string())
                }
                ("POST", "/mcp/hosted-tools/check/call") => (200, answer.to_string()),
                ("POST", "/mcp/hosted-tools/calls/call-diag-1/diagnose") => {
                    let has_key = headers
                        .iter()
                        .any(|(k, v)| k == "x-api-key" && v == "hosted-tools-test-key");
                    assert!(has_key, "missing X-API-Key header: {headers:?}");
                    assert_eq!(body["workspace_id"], WORKSPACE);
                    assert_eq!(body["budget_ms"], 65);
                    assert!(body["failures"].is_array());
                    assert!(body["failing_on_base"].is_object());
                    (
                        200,
                        json!({
                            "success": true,
                            "data": {
                                "call_id": "00000000-0000-0000-0000-000000000001",
                                "new_failures_count": 1,
                                "base_failures_suppressed": 8,
                                "new_failures": [{
                                    "file": "src/auth.rs",
                                    "line": 42,
                                    "col": 9,
                                    "symbol": "verify_token",
                                    "error_code": "E0308",
                                    "message": "mismatched types",
                                    "callers": ["routes::login"],
                                    "definitions": ["JwtClaims"]
                                }],
                                "commits": [{"sha": "abc1234", "message": "refactor verify_token", "matched_by": "symbol"}],
                                "decisions": [{"id": "00000000-0000-0000-0000-000000000002", "summary": "Require typed claims in auth"}],
                                "lessons": [{"id": "00000000-0000-0000-0000-000000000003", "title": "Avoid E0308 in auth", "prevention": "Borrow &str"}],
                                "elapsed_ms": 14,
                                "timed_out": false
                            }
                        })
                        .to_string(),
                    )
                }
                _ => (404, "{}".to_string()),
            }
        })
    }))
    .await;

    let client = client_in_workspace(&url);
    let (_repo, session) = changed_checkout(&client).await;
    let tool = check_tool(&client, &session).await;
    let result = run(&client, &session, &tool, json!({"command": "cargo test"})).await;
    assert!(result.is_error);

    let text = text_of(&result).join("\n");
    assert!(
        text.starts_with("[diagnostics: 1 new failure(s), 8 base failure(s) suppressed]"),
        "{text}"
    );
    assert!(
        text.contains("commits: abc1234 refactor verify_token"),
        "{text}"
    );
    assert!(
        text.contains("decisions: Require typed claims in auth"),
        "{text}"
    );
    assert!(
        text.contains("lessons: Avoid E0308 in auth: Borrow &str"),
        "{text}"
    );
    assert!(text.contains("callers: routes::login"), "{text}");
    assert!(text.contains("defs: JwtClaims"), "{text}");
    assert!(!text.contains("old_var_0"), "{text}");
    assert!(!text.contains("Compiling crate-0"), "{text}");
    assert!(
        text.len() * 2 <= raw_stderr.len(),
        "expected >=50% reduction: {} vs {}",
        text.len(),
        raw_stderr.len()
    );

    let seen = seen.lock().unwrap();
    assert!(seen
        .iter()
        .any(|r| r.method == "POST" && r.path == "/mcp/hosted-tools/calls/call-diag-1/diagnose"));
}

#[tokio::test]
async fn run_fails_open_when_diagnose_endpoint_fails_or_times_out() {
    for mode in ["http_500", "timeout_250ms"] {
        let j_frame = json!({
            "type": "result",
            "ran": true,
            "exit": 101,
            "tool_result": {
                "content": [{"type": "text", "text": "error[E0308]: mismatched types at src/auth.rs:42:9\nerror[E0425]: cannot find `old` in src/old.rs:1:1"}],
                "structuredContent": {
                    "exit": 101,
                    "ran": true,
                    "failures": [{"file": "src/auth.rs", "line": 42, "error_code": "E0308"}],
                    "failing_on_base": {
                        "known": true,
                        "old": [{"file": "src/old.rs", "line": 1, "error_code": "E0425"}],
                        "new": [{"file": "src/auth.rs", "line": 42, "error_code": "E0308"}]
                    }
                },
                "isError": true,
            }
        });
        let (ws_url, _) = frames_service(vec![Step::Json(j_frame)]).await;
        let mut def = echo_definition("stream");
        def["name"] = json!("check");
        def["attachments"] = json!(["checkout_snapshot"]);
        let mut answer = stream_answer(&ws_url, "frames/1");
        answer["call"]["id"] = json!("call-failopen-1");

        let mode_str = mode.to_string();
        let (url, _) = stub_with_headers(Arc::new(move |req| {
            let def = def.clone();
            let answer = answer.clone();
            let method = req.method.clone();
            let path = req.path.clone();
            let mode_str = mode_str.clone();
            Box::pin(async move {
                match (method.as_str(), path.as_str()) {
                    ("GET", "/mcp/hosted-tools") => (200, json!({"tools": [def]}).to_string()),
                    ("POST", "/mcp/hosted-tools/check/call") => (200, answer.to_string()),
                    ("POST", "/mcp/hosted-tools/calls/call-failopen-1/diagnose") => {
                        if mode_str == "timeout_250ms" {
                            tokio::time::sleep(Duration::from_millis(250)).await;
                        }
                        (500, json!({"error": "boom"}).to_string())
                    }
                    _ => (404, "{}".to_string()),
                }
            })
        }))
        .await;

        let client = client_in_workspace(&url);
        let (_repo, session) = changed_checkout(&client).await;
        let tool = check_tool(&client, &session).await;

        let started = Instant::now();
        let result = run(&client, &session, &tool, json!({"command": "cargo test"})).await;
        let elapsed = started.elapsed();

        assert!(result.is_error, "{mode}: expected is_error");
        // Entire run (including checkout snapshot + WS + 95ms fail-open) must not block 250ms+ on /diagnose.
        assert!(
            elapsed < Duration::from_millis(240),
            "{mode}: took too long ({elapsed:?}), /diagnose did not time out within <100ms"
        );
        let text = text_of(&result).join("\n");
        assert!(
            text.contains("[diagnostics: 1 new failure(s), 1 base failure(s) suppressed]"),
            "{mode}: {text}"
        );
        assert!(text.contains("src/auth.rs:42 [E0308]"), "{mode}: {text}");
        assert!(!text.contains("src/old.rs"), "{mode}: {text}");
    }
}

#[test]
fn trim_diagnostic_slice_suppresses_shared_error_code_and_multiline_pytest_base_failures() {
    // Shared E0308 in both old and new (single-line and multi-line rustc)
    let new_items = vec![json!({
        "file": "src/new.rs",
        "line": 20,
        "error_code": "E0308",
    })];
    let old_items = vec![json!({
        "file": "src/old.rs",
        "line": 10,
        "error_code": "E0308",
    })];
    let rustc_raw = "\
error[E0308]: mismatched types at src/old.rs:10:5
error[E0308]: mismatched types
 --> src/old.rs:10:5
  |
10|     let x: i32 = \"old\";
  |                  ^^^^^ expected `i32`, found `&str`

error[E0308]: mismatched types at src/new.rs:20:5
";
    let sliced = trim_diagnostic_slice(rustc_raw, &new_items, &old_items, 4096);
    assert!(!sliced.contains("src/old.rs"), "{sliced}");
    assert!(!sliced.contains("let x: i32"), "{sliced}");
    assert_eq!(sliced.matches("error[E0308]").count(), 1, "{sliced}");
    assert!(sliced.contains("src/new.rs:20:5"), "{sliced}");

    // Multi-line pytest base failure trace
    let py_new = vec![json!({
        "file": "tests/new.py",
        "test": "test_new",
    })];
    let py_old = vec![json!({
        "file": "tests/old.py",
        "test": "test_old",
    })];
    let pytest_raw = "\
FAILED tests/old.py::test_old
E       AssertionError: base failure detail
E       assert 1 == 2

FAILED tests/new.py::test_new
E       AssertionError: new failure detail
";
    let py_sliced = trim_diagnostic_slice(pytest_raw, &py_new, &py_old, 4096);
    assert!(!py_sliced.contains("test_old"), "{py_sliced}");
    assert!(!py_sliced.contains("base failure detail"), "{py_sliced}");
    assert!(!py_sliced.contains("assert 1 == 2"), "{py_sliced}");
    assert!(py_sliced.contains("test_new"), "{py_sliced}");
    assert!(py_sliced.contains("new failure detail"), "{py_sliced}");
}

#[test]
fn trim_diagnostic_slice_suppresses_pytest_tb_auto_vitest_and_cargo_test_base_failures() {
    // (a) pytest --tb=auto parametrized blank-line trace
    let py_auto_new = vec![json!({
        "file": "tests/test_new.py",
        "test": "test_new_param[case2]",
    })];
    let py_auto_old = vec![json!({
        "file": "tests/test_old.py",
        "test": "test_old_param[case1]",
    })];
    let pytest_auto_raw = "\
____ test_old_param[case1] ____

x = 1

    def test_old_param(x):
>       assert x == 2
E       AssertionError: old_param_base_detail

tests/test_old.py:10: AssertionError
____ test_new_param[case2] ____

    def test_new_param(x):
>       assert x == 4
E       AssertionError: new_param_detail

tests/test_new.py:20: AssertionError
";
    let py_auto_sliced = trim_diagnostic_slice(pytest_auto_raw, &py_auto_new, &py_auto_old, 4096);
    assert!(
        !py_auto_sliced.contains("old_param_base_detail"),
        "{py_auto_sliced}"
    );
    assert!(
        !py_auto_sliced.contains("test_old_param"),
        "{py_auto_sliced}"
    );
    assert!(
        py_auto_sliced.contains("test_new_param[case2]"),
        "{py_auto_sliced}"
    );
    assert!(
        py_auto_sliced.contains("new_param_detail"),
        "{py_auto_sliced}"
    );

    // (b) vitest / jest column-0 AssertionError / Error / TypeError / ReferenceError
    let vitest_new = vec![json!({
        "file": "tests/new.test.ts",
        "test": "new case",
    })];
    let vitest_old = vec![json!({
        "file": "tests/old.test.ts",
        "test": "old case",
    })];
    let vitest_raw = "\
 FAIL  tests/old.test.ts > suite > old case
AssertionError: old_vitest_base_detail
Error: old_vitest_error_detail
TypeError: old_vitest_type_detail
ReferenceError: old_vitest_ref_detail
 ❯ tests/old.test.ts:10:5

 FAIL  tests/new.test.ts > suite > new case
AssertionError: new_vitest_detail
 ❯ tests/new.test.ts:25:5
";
    let vitest_sliced = trim_diagnostic_slice(vitest_raw, &vitest_new, &vitest_old, 4096);
    assert!(
        !vitest_sliced.contains("old_vitest_base_detail"),
        "{vitest_sliced}"
    );
    assert!(
        !vitest_sliced.contains("old_vitest_error_detail"),
        "{vitest_sliced}"
    );
    assert!(
        !vitest_sliced.contains("old_vitest_type_detail"),
        "{vitest_sliced}"
    );
    assert!(
        !vitest_sliced.contains("old_vitest_ref_detail"),
        "{vitest_sliced}"
    );
    assert!(
        vitest_sliced.contains("new_vitest_detail"),
        "{vitest_sliced}"
    );

    // (c) cargo test column-0 assertion failed panic trace (including shared file)
    let cargo_new = vec![json!({
        "file": "src/lib.rs",
        "test": "tests::test_new_panic",
    })];
    let cargo_old = vec![json!({
        "file": "src/lib.rs",
        "test": "tests::test_old_panic",
    })];
    let cargo_raw = "\
---- tests::test_old_panic stdout ----
thread 'tests::test_old_panic' panicked at src/lib.rs:10:5:
assertion `left == right` failed: old_panic_base_detail
  left: 1
 right: 2
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace
help: old_panic_help_detail

---- tests::test_new_panic stdout ----
thread 'tests::test_new_panic' panicked at src/lib.rs:25:5:
assertion `left == right` failed: new_panic_detail
  left: 3
 right: 4
";
    let cargo_sliced = trim_diagnostic_slice(cargo_raw, &cargo_new, &cargo_old, 4096);
    assert!(
        !cargo_sliced.contains("old_panic_base_detail"),
        "{cargo_sliced}"
    );
    assert!(!cargo_sliced.contains("left: 1"), "{cargo_sliced}");
    assert!(
        !cargo_sliced.contains("old_panic_help_detail"),
        "{cargo_sliced}"
    );
    assert!(
        cargo_sliced.contains("tests::test_new_panic"),
        "{cargo_sliced}"
    );
    assert!(cargo_sliced.contains("new_panic_detail"), "{cargo_sliced}");
    assert!(cargo_sliced.contains("left: 3"), "{cargo_sliced}");
}

#[test]
fn trim_diagnostic_slice_blocks_unshared_error_code_leak_and_pytest_captured_sections() {
    // Challenge 1: new_items has error_code "AssertionError", old_items omits error_code,
    // both on disjoint files and on shared file (tests/helpers.py).
    for (new_file, old_file) in [
        ("tests/test_new.py", "tests/helpers.py"),
        ("tests/helpers.py", "tests/helpers.py"),
    ] {
        let new_items = vec![json!({
            "file": new_file,
            "test": "test_new_param[case2]",
            "error_code": "AssertionError",
        })];
        let old_items = vec![json!({
            "file": old_file,
            "test": "test_old_param[case1]",
        })];
        let raw = format!(
            "\
____ test_old_param[case1] ____

x = 1

    def test_old_param(x):
>       assert x == 2
E       AssertionError: old_param_base_detail

{old_file}:10: AssertionError
____ test_new_param[case2] ____

    def test_new_param(x):
>       assert x == 4
E       AssertionError: new_param_detail

{new_file}:20: AssertionError
"
        );
        let sliced = trim_diagnostic_slice(&raw, &new_items, &old_items, 4096);
        assert!(
            !sliced.contains("old_param_base_detail"),
            "{new_file}/{old_file}: {sliced}"
        );
        assert!(
            !sliced.contains("test_old_param"),
            "{new_file}/{old_file}: {sliced}"
        );
        if new_file != old_file {
            assert!(
                !sliced.contains("tests/helpers.py"),
                "{new_file}/{old_file}: {sliced}"
            );
        }
        assert!(
            sliced.contains("test_new_param[case2]"),
            "{new_file}/{old_file}: {sliced}"
        );
        assert!(
            sliced.contains("new_param_detail"),
            "{new_file}/{old_file}: {sliced}"
        );
    }

    // Challenge 1b + Edge case: vitest col-0 AssertionError / Error: with unshared error_code on shared & disjoint files
    for (new_file, old_file) in [
        ("tests/new.test.ts", "tests/old.test.ts"),
        ("tests/shared.test.ts", "tests/shared.test.ts"),
    ] {
        let new_items = vec![json!({
            "file": new_file,
            "test": "new case",
            "error_code": "AssertionError",
        })];
        let old_items = vec![json!({
            "file": old_file,
            "test": "old case",
        })];
        let raw = format!(
            "\
 FAIL  {old_file} > suite > old case
AssertionError: old_vitest_base_detail
Error: old_vitest_error_detail
 ❯ {old_file}:10:5

 FAIL  {new_file} > suite > new case
AssertionError: new_vitest_detail
Error: new_vitest_error_detail
 ❯ {new_file}:25:5
"
        );
        let sliced = trim_diagnostic_slice(&raw, &new_items, &old_items, 4096);
        assert!(
            !sliced.contains("old_vitest_base_detail"),
            "{new_file}/{old_file}: {sliced}"
        );
        assert!(
            !sliced.contains("old_vitest_error_detail"),
            "{new_file}/{old_file}: {sliced}"
        );
        assert!(
            sliced.contains("new_vitest_detail"),
            "{new_file}/{old_file}: {sliced}"
        );
        assert!(
            sliced.contains("new_vitest_error_detail"),
            "{new_file}/{old_file}: {sliced}"
        );
    }

    // Challenge 2: pytest Captured stderr call / stdout setup / log teardown + transition to cargo test
    let mixed_new = vec![
        json!({
            "file": "tests/test_new.py",
            "test": "test_new_param[case2]",
            "error_code": "AssertionError",
        }),
        json!({
            "file": "src/lib.rs",
            "test": "tests::test_new",
        }),
    ];
    let mixed_old = vec![json!({
        "file": "tests/test_old.py",
        "test": "test_old_param[case1]",
    })];
    let mixed_raw = "\
____ test_old_param[case1] ____

    def test_old_param(x):
>       assert x == 2
E       AssertionError: old_param_base_detail

----------------------------- Captured stderr call -----------------------------
ERROR: old_test_captured_stderr_leak
----------------------------- Captured stdout setup ----------------------------
ERROR: old_test_captured_stdout_leak
------------------------------ Captured log teardown ---------------------------
ERROR: old_test_captured_log_leak

tests/test_old.py:10: AssertionError
____ test_new_param[case2] ____

    def test_new_param(x):
>       assert x == 4
E       AssertionError: new_param_detail

tests/test_new.py:20: AssertionError
---- tests::test_new stdout ----
thread 'tests::test_new' panicked at src/lib.rs:25:5:
assertion `left == right` failed: new_rust_after_pytest
";
    let mixed_sliced = trim_diagnostic_slice(mixed_raw, &mixed_new, &mixed_old, 4096);
    assert!(
        !mixed_sliced.contains("old_test_captured_stderr_leak"),
        "{mixed_sliced}"
    );
    assert!(
        !mixed_sliced.contains("old_test_captured_stdout_leak"),
        "{mixed_sliced}"
    );
    assert!(
        !mixed_sliced.contains("old_test_captured_log_leak"),
        "{mixed_sliced}"
    );
    assert!(
        !mixed_sliced.contains("old_param_base_detail"),
        "{mixed_sliced}"
    );
    assert!(mixed_sliced.contains("new_param_detail"), "{mixed_sliced}");
    assert!(
        mixed_sliced.contains("new_rust_after_pytest"),
        "{mixed_sliced}"
    );
}

#[test]
fn format_diagnostic_report_monotonic_budget_preserves_short_cargo_and_bounds_large() {
    // Challenge 3: ~494B unpadded cargo test with prefix-overlapping test name (test_old_panic vs test_old_panic_v2)
    let raw_494 = "\
---- tests::test_old_panic stdout ----
thread 'tests::test_old_panic' panicked at src/lib.rs:10:5:
assertion `left == right` failed: old_panic_base_detail
  left: 1
 right: 2
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace

---- tests::test_old_panic_v2 stdout ----
thread 'tests::test_old_panic_v2' panicked at src/lib.rs:25:5:
assertion `left == right` failed: new_panic_detail
  left: 3
 right: 4

failures:
    tests::test_old_panic
    tests::test_old_panic_v2

test result: FAILED. 0 passed; 2 failed; 0 ignored
";
    let structured = json!({
        "failures": [{
            "file": "src/lib.rs",
            "line": 25,
            "test": "tests::test_old_panic_v2",
            "message": "new_panic_detail",
        }],
        "failing_on_base": {
            "known": true,
            "old": [{
                "file": "src/lib.rs",
                "line": 10,
                "test": "tests::test_old_panic",
                "message": "old_panic_base_detail",
            }],
            "new": [{
                "file": "src/lib.rs",
                "line": 25,
                "test": "tests::test_old_panic_v2",
                "message": "new_panic_detail",
            }],
        }
    });
    let report = format_diagnostic_report(&structured, raw_494).expect("formatted report");
    assert!(
        !report.contains("old_panic_base_detail"),
        "base panic leaked:\n{report}"
    );
    assert!(!report.contains("left: 1"), "base left leaked:\n{report}");
    assert!(
        !report.contains("tests::test_old_panic\n"),
        "old summary leaked:\n{report}"
    );
    assert!(
        report.contains("tests::test_old_panic_v2"),
        "new test missing:\n{report}"
    );
    assert!(
        report.contains("new_panic_detail"),
        "new panic detail missing:\n{report}"
    );
    assert!(report.contains("left: 3"), "new left missing:\n{report}");

    // Monotonicity check across 300B..14.7KB + >=50% reduction on >=2048B and 14.7KB outputs
    let mut prev_len = 0usize;
    let short_raw = "\
---- tests::test_old_panic_v2 stdout ----
thread 'tests::test_old_panic_v2' panicked at src/lib.rs:25:5:
assertion `left == right` failed: new_panic_detail
  left: 3
 right: 4
";
    let short_out = format_diagnostic_report(&structured, short_raw).expect("short report");
    assert!(short_out.contains("new_panic_detail"), "{short_out}");
    assert!(short_out.contains("left: 3"), "{short_out}");

    let filler_line = "   Compiling padding-crate v0.1.0 (/work/repo/crates/pad)\n";
    let padded_new = "thread 'tests::test_old_panic_v2' panicked at src/lib.rs:25:5:\n";
    for target_len in [550usize, 700, 1200, 2048, 3072, 4096, 14700] {
        let mut raw = raw_494.to_string();
        while raw.len() < target_len {
            if raw.len() % 3 == 0 {
                raw.push_str(padded_new);
            } else {
                raw.push_str(filler_line);
            }
        }
        let out = format_diagnostic_report(&structured, &raw).expect("report");
        assert!(
            out.len() >= prev_len,
            "output shrank at {target_len} (raw {}): {} < {prev_len}",
            raw.len(),
            out.len()
        );
        prev_len = out.len();
        if target_len >= 2048 {
            assert!(
                out.len() * 2 <= raw.len(),
                "expected >=50% reduction at {target_len} (raw {}): {} vs {}",
                raw.len(),
                out.len(),
                raw.len()
            );
        }
    }

    // Large 14.7KB noisy multi-runner output check
    let mut big_lines = Vec::new();
    for i in 0..180 {
        big_lines.push(format!(
            "   Compiling noisy-crate-{i} v0.1.0 (/work/repo/crates/n{i})"
        ));
    }
    for i in 0..40 {
        big_lines.push(format!(
            "____ test_old_param_{i} ____\nE       AssertionError: old_base_{i}\ntests/old.py:{i}: AssertionError"
        ));
    }
    big_lines.push(raw_494.to_string());
    let big_raw = big_lines.join("\n");
    assert!(big_raw.len() >= 14_000, "got {}", big_raw.len());
    let big_report = format_diagnostic_report(&structured, &big_raw).expect("big report");
    assert!(
        big_report.len() * 2 <= big_raw.len(),
        "expected >=50% reduction on 14.7KB: {} vs {}",
        big_report.len(),
        big_raw.len()
    );
    assert!(big_report.contains("new_panic_detail"), "{big_report}");
}

#[test]
fn contains_token_handles_boundaries_and_utf8_safely() {
    assert!(contains_token("error[e0308]: mismatched", "e0308"));
    assert!(contains_token(
        "tests::test_old_panic stdout",
        "tests::test_old_panic"
    ));
    assert!(!contains_token(
        "---- tests::test_old_panic_v2 stdout ----",
        "tests::test_old_panic"
    ));
    assert!(!contains_token("assertionerror: x", "error"));
    assert!(!contains_token("old_vitest_error_detail", "error"));
    assert!(contains_token(
        "✓ ❯ tests/old.test.ts:10:5 ●",
        "tests/old.test.ts"
    ));
    assert!(contains_token("❯foo ❯ bar", "❯"));
    assert!(!contains_token("✓_foo", "✓"));
}

#[test]
fn trim_diagnostic_slice_and_report_remediate_r4_adversarial_leaks_and_parity() {
    // 1. vitest arbitrary <Name>Error: / <Name>Exception: on shared file with unshared error_code
    let vitest_new = vec![json!({
        "file": "tests/shared.test.ts",
        "test": "new case",
        "error_code": "AssertionError",
    })];
    let vitest_old = vec![json!({
        "file": "tests/shared.test.ts",
        "test": "old case",
    })];
    let vitest_raw = "\
 FAIL  tests/shared.test.ts > suite > old case
SyntaxError: old_vitest_syntax_leak
RangeError: old_vitest_range_leak
EvalError: old_vitest_eval_leak
URIError: old_vitest_uri_leak
AggregateError: old_vitest_agg_leak
ZodError: old_vitest_zod_leak
CustomException: old_vitest_custom_exception_leak
pkg.CustomError: old_vitest_dotted_error_leak

SyntaxError: old_vitest_after_blank_leak
 ❯ tests/shared.test.ts:10:5

 FAIL  tests/shared.test.ts > suite > new case
AssertionError: new_vitest_detail
 ❯ tests/shared.test.ts:25:5
";
    let vitest_sliced = trim_diagnostic_slice(vitest_raw, &vitest_new, &vitest_old, 4096);
    for leak in [
        "old_vitest_syntax_leak",
        "old_vitest_range_leak",
        "old_vitest_eval_leak",
        "old_vitest_uri_leak",
        "old_vitest_agg_leak",
        "old_vitest_zod_leak",
        "old_vitest_custom_exception_leak",
        "old_vitest_dotted_error_leak",
        "old_vitest_after_blank_leak",
    ] {
        assert!(!vitest_sliced.contains(leak), "{leak}: {vitest_sliced}");
    }
    assert!(
        vitest_sliced.contains("new_vitest_detail"),
        "{vitest_sliced}"
    );

    // 2. Go --- FAIL: blank line before goroutine N [running]:, stack frames, and panic:
    let go_new = vec![json!({
        "file": "pkg/a_test.go",
        "test": "TestNewPanic",
    })];
    let go_old = vec![json!({
        "file": "pkg/a_test.go",
        "test": "TestOldPanic",
    })];
    let go_raw = "\
--- FAIL: TestOldPanic (0.00s)
panic: old_go_panic_leak [recovered]
\tpanic: old_go_panic_leak

goroutine 19 [running]:
testing.tRunner.func1.2({0x100, 0x200})
\t/usr/local/go/src/testing/testing.go:1632 +0x230
panic: old_secondary_panic_leak

panic: old_tertiary_panic_after_blank_leak
--- FAIL: TestNewPanic (0.00s)
panic: new_go_panic_detail
";
    let go_sliced = trim_diagnostic_slice(go_raw, &go_new, &go_old, 4096);
    for leak in [
        "TestOldPanic",
        "old_go_panic_leak",
        "old_secondary_panic_leak",
        "old_tertiary_panic_after_blank_leak",
    ] {
        assert!(!go_sliced.contains(leak), "{leak}: {go_sliced}");
    }
    assert!(go_sliced.contains("TestNewPanic"), "{go_sliced}");
    assert!(go_sliced.contains("new_go_panic_detail"), "{go_sliced}");

    // 3. pytest Captured stderr/stdout/log containing FAILED / FAIL / --- FAIL: / error[
    let py_new = vec![json!({
        "file": "tests/test_new.py",
        "test": "test_new_param[case2]",
        "error_code": "AssertionError",
    })];
    let py_old = vec![json!({
        "file": "tests/test_old.py",
        "test": "test_old_param[case1]",
    })];
    let py_raw = "\
____ test_old_param[case1] ____
----------------------------- Captured stderr call -----------------------------
FAILED subprocess check: old_captured_subproc_leak
FAIL old_captured_fail_leak
--- FAIL: OldCapturedGoLeak
error[E0308]: old_captured_rustc_leak
____ test_new_param[case2] ____
E       AssertionError: new_param_detail
";
    let py_sliced = trim_diagnostic_slice(py_raw, &py_new, &py_old, 4096);
    for leak in [
        "old_captured_subproc_leak",
        "old_captured_fail_leak",
        "OldCapturedGoLeak",
        "old_captured_rustc_leak",
    ] {
        assert!(!py_sliced.contains(leak), "{leak}: {py_sliced}");
    }
    assert!(py_sliced.contains("test_new_param[case2]"), "{py_sliced}");
    assert!(py_sliced.contains("new_param_detail"), "{py_sliced}");

    // 4. CRLF interior line normalization & 5. explicit base_failures_suppressed: 0
    let mut crlf_lines = vec!["error[E0425]: old_crlf_leak at src/old.rs:1:1".to_string()];
    let mut diag_new = Vec::new();
    for i in 0..12 {
        crlf_lines.push(format!("error[E0308]: msg_{i} at src/f{i}.rs:1:1"));
        crlf_lines.push(format!(" --> src/f{i}.rs:1:1"));
        diag_new.push(json!({
            "file": format!("src/f{i}.rs"),
            "line": 1,
            "error_code": "E0308",
            "message": format!("msg_{i}"),
        }));
    }
    let crlf_raw = crlf_lines.join("\r\n") + "\r\n";
    let structured_zero = json!({
        "failing_on_base": {
            "known": true,
            "old": [{"file": "src/old.rs", "line": 1, "error_code": "E0425"}],
            "new": diag_new,
        },
        "diagnostics": {
            "new_failures_count": 12,
            "base_failures_suppressed": 0,
            "new_failures": diag_new,
            "commits": [{"sha": "abc1234", "message": "update f0"}],
            "decisions": [{"id": "d1", "summary": "keep strict"}],
            "lessons": [{"id": "l1", "title": "check E0308"}],
        }
    });
    let report = format_diagnostic_report(&structured_zero, &crlf_raw).expect("report");
    assert!(!report.contains('\r'), "CR leaked in report: {report:?}");
    assert!(!report.contains("old_crlf_leak"), "{report}");
    assert!(
        report.starts_with("[diagnostics: 12 new failure(s), 0 base failure(s) suppressed]"),
        "{report}"
    );
}

#[test]
fn trim_diagnostic_slice_and_report_remediate_r5_adversarial_leaks_and_parity() {
    // 1. a5_go_standalone_panic_goroutine: standalone panic + blank + goroutine N [running]: + stack + secondary panic
    let a5_new = vec![json!({
        "file": "pkg/new_test.go",
        "symbol": "NewFn",
    })];
    let a5_old = vec![json!({
        "file": "pkg/old_test.go",
        "symbol": "OldFn",
    })];
    let a5_raw = "\
panic: old_standalone_panic

goroutine 19 [running]:
pkg.OldFn()
\t/work/pkg/old_test.go:10 +0x20
panic: old_secondary_leak

panic: new_standalone_panic
goroutine 21 [running]:
pkg.NewFn()
\t/work/pkg/new_test.go:30 +0x10
";
    let a5_sliced = trim_diagnostic_slice(a5_raw, &a5_new, &a5_old, 4096);
    assert!(
        !a5_sliced.contains("old_standalone_panic"),
        "a5 old standalone panic leaked:\n{a5_sliced}"
    );
    assert!(
        !a5_sliced.contains("old_secondary_leak"),
        "a5 old secondary panic leaked:\n{a5_sliced}"
    );
    assert!(
        a5_sliced.contains("new_standalone_panic"),
        "a5 missing new standalone panic:\n{a5_sliced}"
    );
    assert!(
        a5_sliced.contains("pkg.NewFn()"),
        "a5 missing new goroutine stack:\n{a5_sliced}"
    );

    // 2. a1_standalone_named_errors_no_blank: consecutive standalone SyntaxError + TypeError with no blank line
    let a1_new = vec![json!({
        "file": "tests/new.ts",
        "error_code": "TypeError",
    })];
    let a1_old = vec![json!({
        "file": "tests/old.ts",
        "error_code": "SyntaxError",
    })];
    let a1_raw = "\
SyntaxError: old_syntax_err in tests/old.ts:5
TypeError: new_type_err in tests/new.ts:10
";
    let a1_sliced = trim_diagnostic_slice(a1_raw, &a1_new, &a1_old, 4096);
    assert!(
        !a1_sliced.contains("old_syntax_err"),
        "a1 old SyntaxError leaked:\n{a1_sliced}"
    );
    assert!(
        a1_sliced.contains("new_type_err"),
        "a1 new TypeError was suppressed:\n{a1_sliced}"
    );

    // 3. a6_vitest_caused_by_and_serialized_error: Caused by: and Serialized Error: inside FAIL block,
    //    and plain Caused by: / Serialized Error: outside a failure block do not swallow subsequent failures
    let a6_new = vec![json!({
        "file": "tests/shared.test.ts",
        "test": "new case",
        "error_code": "AssertionError",
    })];
    let a6_old = vec![json!({
        "file": "tests/shared.test.ts",
        "test": "old case",
    })];
    let a6_raw = "\
 FAIL  tests/shared.test.ts > suite > old case
SyntaxError: old_top_err
Caused by: TypeError: old_caused_by_leak
Serialized Error: { code: 'OLD_ERR' }

 FAIL  tests/shared.test.ts > suite > new case
AssertionError: new_vitest_ok
";
    let a6_sliced = trim_diagnostic_slice(a6_raw, &a6_new, &a6_old, 4096);
    assert!(
        !a6_sliced.contains("old_top_err"),
        "a6 old_top_err leaked:\n{a6_sliced}"
    );
    assert!(
        !a6_sliced.contains("old_caused_by_leak"),
        "a6 old_caused_by_leak leaked:\n{a6_sliced}"
    );
    assert!(
        !a6_sliced.contains("OLD_ERR"),
        "a6 OLD_ERR leaked:\n{a6_sliced}"
    );
    assert!(
        a6_sliced.contains("new_vitest_ok"),
        "a6 missing new_vitest_ok:\n{a6_sliced}"
    );

    let a6_outside_raw = "\
Caused by: note mentioning tests/shared.test.ts old case
Serialized Error: context mentioning tests/shared.test.ts old case
TypeError: new_standalone_after_caused_by in tests/other.ts:12
";
    let a6_outside_new = vec![json!({
        "file": "tests/other.ts",
        "error_code": "TypeError",
    })];
    let a6_outside_sliced = trim_diagnostic_slice(a6_outside_raw, &a6_outside_new, &a6_old, 4096);
    assert!(
        a6_outside_sliced.contains("new_standalone_after_caused_by"),
        "plain Caused by / Serialized Error outside failure block swallowed next line:\n{a6_outside_sliced}"
    );

    // 4. a9_fob_missing_new_key_with_failures: omitted `new` or `new: null` falls back to `failures`,
    //    while `new: []` reports 0 new failures
    for fob_val in [
        json!({"known": true, "old": [{"file": "src/old.rs"}]}),
        json!({"known": true, "old": [{"file": "src/old.rs"}], "new": null}),
    ] {
        let structured_a9 = json!({
            "failing_on_base": fob_val,
            "failures": [{
                "file": "src/new.rs",
                "line": 10,
                "error_code": "E0308",
                "message": "type mismatch"
            }]
        });
        let report_a9 = format_diagnostic_report(
            &structured_a9,
            "error[E0308]: type mismatch at src/new.rs:10:1\n",
        )
        .expect("a9 report");
        assert!(
            report_a9.starts_with("[diagnostics: 1 new failure(s), 1 base failure(s) suppressed]"),
            "a9 expected 1 new failure:\n{report_a9}"
        );
        assert!(
            report_a9.contains("src/new.rs:10 [E0308] type mismatch"),
            "a9 missing fallback failure label:\n{report_a9}"
        );
    }
    let structured_a9_empty = json!({
        "failing_on_base": {"known": true, "old": [{"file": "src/old.rs"}], "new": []},
        "failures": [{"file": "src/new.rs", "line": 10, "error_code": "E0308", "message": "type mismatch"}]
    });
    let report_a9_empty = format_diagnostic_report(
        &structured_a9_empty,
        "error[E0308]: type mismatch at src/new.rs:10:1\n",
    )
    .expect("a9 empty report");
    assert!(
        report_a9_empty
            .starts_with("[diagnostics: 0 new failure(s), 1 base failure(s) suppressed]"),
        "a9 empty new array should report 0 new:\n{report_a9_empty}"
    );

    // 5. fp_lowercase_ident_error: camelCase/lowercase final segment like `lastError:` or `pkg.lastError:`
    //    does NOT match is_named_error_or_exception_line and must not be swallowed into an old FAIL block
    assert!(!is_named_error_or_exception_line("lastError: x"));
    assert!(!is_named_error_or_exception_line("pkg.lastError: x"));
    assert!(!is_named_error_or_exception_line("_CustomError: x"));
    assert!(is_named_error_or_exception_line("SyntaxError: x"));
    assert!(is_named_error_or_exception_line("pkg.CustomError: x"));

    let fp_new = vec![json!({
        "file": "tests/new.test.ts",
        "test": "new case",
    })];
    let fp_old = vec![json!({
        "file": "tests/old.test.ts",
        "test": "old case",
    })];
    let fp_raw = "\
 FAIL  tests/old.test.ts > suite > old case
SyntaxError: old_leak
lastError: new_failure_in_tests/new.test.ts new case
";
    let fp_sliced = trim_diagnostic_slice(fp_raw, &fp_new, &fp_old, 4096);
    assert!(
        !fp_sliced.contains("old_leak"),
        "fp old_leak leaked:\n{fp_sliced}"
    );
    assert!(
        fp_sliced.contains("lastError: new_failure_in_tests/new.test.ts new case"),
        "fp lowercase ident error was swallowed:\n{fp_sliced}"
    );
}
