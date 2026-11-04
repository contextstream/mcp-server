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
    authorization: Option<String>,
    /// Every header name and value, names lowercased.
    headers: Vec<(String, String)>,
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
    let headers = head
        .lines()
        .skip(1)
        .filter_map(|line| {
            let (key, value) = line.split_once(":")?;
            Some((key.trim().to_lowercase(), value.trim().to_string()))
        })
        .collect();
    Some(Seen {
        method,
        path: path.strip_prefix("/api/v1").unwrap_or(&path).to_string(),
        query,
        authorization: header("authorization"),
        headers,
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
async fn a_stream_tool_reads_its_result_from_the_endpoint_the_api_names() {
    let (stream_url, stream_seen) = stub(Arc::new(|method, path, body| match (method, path) {
        ("POST", "/run") => (
            200,
            [
                json!({"type": "output", "text": "one "}).to_string(),
                json!({"type": "output", "text": "two"}).to_string(),
                json!({"type": "result", "content": [{"type": "text", "text": format!("done: {}", body["arguments"]["text"].as_str().unwrap_or_default())}]}).to_string(),
            ]
            .join("\n"),
        ),
        _ => (404, "{}".to_string()),
    }))
    .await;
    let endpoint = format!("{stream_url}/run");
    let (url, _) = stub(Arc::new(move |method, path, _| match (method, path) {
        ("GET", "/mcp/hosted-tools") => (
            200,
            json!({"tools": [echo_definition("stream")]}).to_string(),
        ),
        ("POST", "/mcp/hosted-tools/echo/call") => (
            200,
            json!({"stream": {"url": endpoint, "token": "call-token-1"}}).to_string(),
        ),
        _ => (404, "{}".to_string()),
    }))
    .await;
    let client = client(&url);
    let gate = HostedToolsGate::new();
    let tool = find(&gate, &client, &session_for(&client), "echo")
        .await
        .expect("echo is defined");
    assert_eq!(tool.transport, Transport::Stream);

    let result = run(&client, &session_for(&client), &tool, json!({"text": "hi"})).await;
    assert!(!result.is_error, "{result:?}");
    assert_eq!(text_of(&result), ["one two", "done: hi"]);
    let seen: Vec<Seen> = stream_seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    // The endpoint gets the call's token, never the API key.
    assert_eq!(
        seen[0].authorization.as_deref(),
        Some("Bearer call-token-1")
    );
    assert!(
        seen[0]
            .headers
            .iter()
            .all(|(name, value)| name != "x-api-key" && !value.contains("hosted-tools-test-key")),
        "{:?}",
        seen[0].headers
    );
    assert_eq!(seen[0].body, json!({"arguments": {"text": "hi"}}));
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

#[test]
fn a_stream_without_a_result_frame_is_an_error_that_keeps_its_output() {
    let result = result_from_frames("echo", "{\"type\":\"output\",\"text\":\"partial\"}\n");
    assert!(result.is_error);
    assert!(text_of(&result)[0].contains("partial"));
    assert!(result_from_frames("echo", "").is_error);
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
