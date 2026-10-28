//! Tests for the studio tool against a stub of ContextStream Create: plan, the money gate
//! (elicitation, accepted price), status with progress, results, revise and refusals.

use super::*;
use crate::peer::{run_with_call_peer, CallPeer, PeerChannel};
use crate::testing::TestFixtures;
use mcp_types::Config;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const WORKSPACE: &str = "11111111-1111-4111-8111-111111111111";
const PROJECT: &str = "22222222-2222-4222-8222-222222222222";
const QUOTE: &str = "33333333-3333-4333-8333-333333333333";
const JOB: &str = "44444444-4444-4444-8444-444444444444";
const IMAGE_ID: &str = "55555555-5555-4555-8555-555555555555";
const VIDEO_ID: &str = "66666666-6666-4666-8666-666666666666";
const PNG: &[u8] = b"\x89PNG\r\n\x1a\nnot-really-an-image";

// ============================================================================
// A stub ContextStream API
// ============================================================================

#[derive(Debug, Clone)]
struct Recorded {
    method: String,
    /// Path below `/api/v1`, without the query.
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: Value,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone)]
struct Answer {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

impl Answer {
    fn ok(data: Value) -> Self {
        Self::status(200, data)
    }

    fn status(status: u16, data: Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: json!({"success": true, "data": data, "error": null})
                .to_string()
                .into_bytes(),
        }
    }

    fn raw(status: u16, body: Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: body.to_string().into_bytes(),
        }
    }

    fn error(status: u16, code: &str, message: &str) -> Self {
        Self::raw(
            status,
            json!({"success": false, "data": null, "error": {"code": code, "message": message}}),
        )
    }

    fn bytes(content_type: &'static str, body: &[u8]) -> Self {
        Self {
            status: 200,
            content_type,
            body: body.to_vec(),
        }
    }
}

type Responder = Arc<dyn Fn(&Recorded) -> Answer + Send + Sync>;

struct Stub {
    url: String,
    requests: Arc<Mutex<Vec<Recorded>>>,
}

impl Stub {
    async fn start(responder: Responder) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests: Arc<Mutex<Vec<Recorded>>> = Arc::default();
        let seen = requests.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let seen = seen.clone();
                let responder = responder.clone();
                tokio::spawn(async move {
                    let Some(recorded) = read_request(&mut socket).await else {
                        return;
                    };
                    seen.lock().unwrap().push(recorded.clone());
                    let answer = responder(&recorded);
                    let head = format!(
                        "HTTP/1.1 {} X\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        answer.status,
                        answer.content_type,
                        answer.body.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(&answer.body).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Self { url, requests }
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }

    fn posts_to(&self, path: &str) -> Vec<Recorded> {
        self.requests()
            .into_iter()
            .filter(|r| r.method == "POST" && r.path == path)
            .collect()
    }

    fn tool(&self) -> StudioTool {
        let mut config: Config = TestFixtures::test_config();
        config.api_url = self.url.clone();
        config.api_key = Some("studio-test-key".to_string());
        config.default_workspace_id = Some(Uuid::parse_str(WORKSPACE).unwrap());
        config.default_project_id = Some(Uuid::parse_str(PROJECT).unwrap());
        let client = ContextStreamClient::new(config.clone());
        let session = Arc::new(SessionManager::new(client.clone(), config));
        StudioTool::new(client, session, Arc::new(StudioGate::new()))
    }
}

async fn read_request(socket: &mut tokio::net::TcpStream) -> Option<Recorded> {
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
    let headers: Vec<(String, String)> = head
        .lines()
        .skip(1)
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_string(), value.trim().to_string()))
        })
        .collect();
    let length = headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .unwrap_or(0);
    while buffer.len() < header_end + length {
        let read = socket.read(&mut chunk).await.ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let request_line = head.lines().next()?.to_string();
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let (raw_path, query) = target.split_once('?').unwrap_or((&target, ""));
    Some(Recorded {
        method,
        path: raw_path
            .strip_prefix("/api/v1")
            .unwrap_or(raw_path)
            .to_string(),
        query: query.to_string(),
        headers,
        body: serde_json::from_slice(&buffer[header_end..]).unwrap_or(Value::Null),
    })
}

fn quote(status: &str) -> Value {
    let ready = status == "ready";
    json!({
        "id": QUOTE,
        "status": status,
        "error": if status == "failed" { json!({"code": "make_unplanned", "message": "x"}) } else { Value::Null },
        "request": {"kind": "make", "model": "make", "prompt": "A sting for the release"},
        "plan": if ready { json!({
            "understanding": "a short sound mark for the release post",
            "pieces": [
                {"index": 0, "id": "release-sting", "title": "Release Sting", "kindOfWork": "sound.interface",
                 "medium": "sound", "seconds": 2, "maxSeconds": 2, "why": "a mark people hear once", "asked": true},
                {"index": 1, "id": "brand-board", "title": "Brand Board", "kindOfWork": "image.brand",
                 "medium": "image", "aspect": "16:9", "why": "the post's image", "asked": false},
            ]}) } else { Value::Null },
        "credits": if ready { json!(412) } else { Value::Null },
        "estimatedCredits": if ready { json!(260) } else { Value::Null },
        "read": {"brand": "ContextStream", "context": [
            {"what": "From your agent: The diff", "detail": "+ faster search"},
            {"what": "Doc: Brand voice", "detail": "Plain words."},
        ]},
        "retryAfterSec": if ready { Value::Null } else { json!(2) },
        "createdAt": "2026-10-27T18:00:00Z",
        "expiresAt": "2026-10-28T18:00:00Z",
    })
}

fn job(status: &str, made: bool) -> Value {
    let done = status == "succeeded";
    json!({
        "id": JOB,
        "workspaceId": WORKSPACE,
        "kind": "make",
        "status": status,
        "error": null,
        "progress": if done { 100 } else { 40 },
        "request": {"kind": "make", "model": "make", "prompt": "A sting for the release"},
        "outputs": if made { json!([
            {"index": 0, "contentId": VIDEO_ID, "mimeType": "audio/mp4", "durationMs": 2000,
             "url": format!("/api/v1/create/outputs/{VIDEO_ID}/content?workspaceId={WORKSPACE}")},
            {"index": 1, "contentId": IMAGE_ID, "mimeType": "image/png",
             "url": format!("/api/v1/create/outputs/{IMAGE_ID}/content?workspaceId={WORKSPACE}")},
        ]) } else { json!([]) },
        "quotedCredits": 412,
        "chargedCredits": if done { json!(250) } else { Value::Null },
        "make": {
            "quoteId": QUOTE,
            "plan": {"understanding": "u", "pieces": [
                {"index": 0, "id": "release-sting", "title": "Release Sting", "kindOfWork": "sound.interface",
                 "medium": "sound", "seconds": 2, "status": if made { "made" } else { "judging" }, "asked": true, "why": "w"},
                {"index": 1, "id": "brand-board", "title": "Brand Board", "kindOfWork": "image.brand",
                 "medium": "image", "aspect": "16:9", "status": if made { "made" } else { "making" }, "asked": false, "why": "w"},
            ]},
            "read": {"brand": "ContextStream", "context": []},
            "stage": if done { "done" } else { "judging release-sting" },
            "spentCredits": 120,
            "pieces": if made { json!([
                {"index": 0, "id": "release-sting", "title": "Release Sting", "medium": "sound",
                 "placard": {"asked": "A sting for the release", "idea": "two rising notes", "made": "Sound effects, three takes, judged"}},
                {"index": 1, "id": "brand-board", "title": "Brand Board", "medium": "image",
                 "placard": {"asked": "A sting for the release", "idea": "the wordmark on mist", "made": "Drawn by the engine"}},
            ]) } else { json!([]) },
            "failed": [],
        },
        "createdAt": "2026-10-27T18:01:00Z",
    })
}

/// The usual API: a quote that plans once then is ready, jobs as `job_status` says, a
/// balance, signed links and the image's bytes.
fn api(job_status: Arc<Mutex<(String, bool)>>) -> Responder {
    // The first read after a plan is asked for is still planning.
    let planning_reads = Arc::new(AtomicUsize::new(0));
    Arc::new(move |request: &Recorded| {
        let path = request.path.as_str();
        match (request.method.as_str(), path) {
            ("GET", p) if p.starts_with("/projects/") => {
                Answer::ok(json!({"id": PROJECT, "name": "studio", "workspace_id": WORKSPACE}))
            }
            ("GET", "/create/catalog") => Answer::ok(json!({"make": {"status": "available"}})),
            ("POST", "/create/quotes") => {
                planning_reads.store(1, Ordering::SeqCst);
                Answer::status(202, quote("planning"))
            }
            ("GET", p) if p == format!("/create/quotes/{QUOTE}") => {
                if planning_reads.swap(0, Ordering::SeqCst) == 1 {
                    Answer::ok(quote("planning"))
                } else {
                    Answer::ok(quote("ready"))
                }
            }
            ("GET", "/credits/balance") => Answer::raw(
                200,
                json!({"credits": {"spendable": 5000, "total_available": 5000}}),
            ),
            ("POST", "/create/jobs") => Answer::status(202, job("queued", false)),
            ("GET", "/create/jobs") => Answer::ok(json!([job("succeeded", true)])),
            ("GET", p) if p == format!("/create/jobs/{JOB}") => {
                let (status, made) = job_status.lock().unwrap().clone();
                Answer::ok(job(&status, made))
            }
            ("POST", p) if p == format!("/create/jobs/{JOB}/cancel") => {
                Answer::ok(job("cancelled", false))
            }
            ("GET", p) if p.ends_with("/download-url") => Answer::ok(json!({
                "download_url": format!("https://store.example/{}?sig=1", p.split('/').nth(4).unwrap_or("")),
                "expires_in_seconds": 900
            })),
            ("GET", p) if p == format!("/create/outputs/{IMAGE_ID}/content") => {
                Answer::bytes("image/png", PNG)
            }
            _ => Answer::error(404, "NOT_FOUND", "Not found: nothing here"),
        }
    })
}

fn text_of(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|item| match item {
            ContentItem::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn structured(result: &ToolResult) -> Value {
    result.structured_content.clone().unwrap_or(Value::Null)
}

/// A client that answers questions as told and records what it was sent.
#[derive(Default)]
struct FakeClient {
    sent: Mutex<Vec<(String, Value)>>,
    answer: Mutex<Option<Value>>,
}

#[async_trait]
impl PeerChannel for FakeClient {
    async fn notify(&self, method: &str, params: Value) {
        self.sent.lock().unwrap().push((method.to_string(), params));
    }

    async fn request(
        &self,
        method: &str,
        params: Value,
        _timeout: Duration,
    ) -> std::result::Result<Value, String> {
        self.sent.lock().unwrap().push((method.to_string(), params));
        self.answer
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| "no answer".to_string())
    }
}

// ============================================================================
// The tool
// ============================================================================

#[test]
fn the_tool_offers_a_producer_not_a_model_menu() {
    let stub_config = TestFixtures::test_config();
    let client = ContextStreamClient::new(stub_config.clone());
    let session = Arc::new(SessionManager::new(client.clone(), stub_config));
    let tool = StudioTool::new(client, session, Arc::new(StudioGate::new()));
    let metadata = tool.metadata();
    assert_eq!(metadata.name, "studio");
    assert!(metadata.description.chars().count() <= 1024);
    let schema = tool.input_schema();
    let properties = schema["properties"].as_object().unwrap();
    for field in [
        "action",
        "prompt",
        "context",
        "attachments",
        "plan_id",
        "make_id",
        "accepted_price_credits",
        "piece",
        "change",
        "wait_seconds",
    ] {
        assert!(properties.contains_key(field), "{field}");
    }
    // No model to choose anywhere: no parameter names one.
    assert!(!properties.keys().any(|k| k.contains("model")));
    for action in VALID_ACTIONS {
        assert!(schema["properties"]["action"]["description"]
            .as_str()
            .unwrap()
            .contains(&format!("- {action}:")));
    }
}

#[tokio::test]
async fn plan_sends_the_agents_context_and_returns_one_price() {
    let stub = Stub::start(api(Arc::new(Mutex::new(("queued".into(), false))))).await;
    let tool = stub.tool();
    let result = tool
        .execute(json!({
            "action": "plan",
            "prompt": "A sting for the release",
            "context": [{"title": "The diff", "text": "+ faster search\r\n"}],
            "wait_seconds": 20,
        }))
        .await
        .unwrap();
    assert!(!result.is_error, "{}", text_of(&result));
    let text = text_of(&result);
    assert!(
        text.contains("1. Release Sting (Sound · 2 seconds)"),
        "{text}"
    );
    assert!(
        text.contains("2. Brand Board (Image · Wide 16:9), suggested"),
        "{text}"
    );
    assert!(
        text.contains("Price: up to 412 credits, likely about 260."),
        "{text}"
    );
    assert!(text.contains("Balance: 5,000 credits."), "{text}");
    assert!(text.contains("From your agent: The diff"), "{text}");
    assert!(
        text.contains("Nothing is made until the person agrees."),
        "{text}"
    );
    let out = structured(&result);
    assert_eq!(out["plan_id"], QUOTE);
    assert_eq!(out["next"]["accepted_price_credits"], 412);

    let posted = stub.posts_to("/create/quotes");
    assert_eq!(posted.len(), 1);
    let body = &posted[0].body;
    assert_eq!(body["workspaceId"], WORKSPACE);
    assert_eq!(body["projectId"], PROJECT);
    assert_eq!(
        body["request"],
        json!({"kind": "make", "prompt": "A sting for the release"})
    );
    assert_eq!(
        body["context"],
        json!([{"title": "The diff", "text": "+ faster search"}])
    );
    // The plan is read back in its workspace.
    assert!(stub
        .requests()
        .iter()
        .any(|r| r.path == format!("/create/quotes/{QUOTE}")
            && r.query == format!("workspaceId={WORKSPACE}")));
    // Planning spends nothing: no job.
    assert!(stub.posts_to("/create/jobs").is_empty());
}

#[tokio::test]
async fn make_spends_nothing_without_the_persons_go_ahead() {
    let stub = Stub::start(api(Arc::new(Mutex::new(("queued".into(), false))))).await;
    let tool = stub.tool();
    let unasked = tool
        .execute(json!({"action": "make", "plan_id": QUOTE}))
        .await
        .unwrap();
    let text = text_of(&unasked);
    assert!(text.starts_with("Not made yet"), "{text}");
    assert!(text.contains("accepted_price_credits 412"), "{text}");
    assert_eq!(
        structured(&unasked)["accept_with"]["accepted_price_credits"],
        412
    );

    let too_low = tool
        .execute(json!({"action": "make", "plan_id": QUOTE, "accepted_price_credits": 300}))
        .await
        .unwrap();
    assert!(text_of(&too_low).contains("the accepted price was 300"));
    assert!(stub.posts_to("/create/jobs").is_empty());

    let accepted = tool
        .execute(json!({"action": "make", "plan_id": QUOTE, "accepted_price_credits": 412}))
        .await
        .unwrap();
    assert!(!accepted.is_error, "{}", text_of(&accepted));
    let jobs = stub.posts_to("/create/jobs");
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        jobs[0].header("idempotency-key"),
        Some(format!("make-{QUOTE}").as_str())
    );
    assert_eq!(jobs[0].body["quoteId"], QUOTE);
    assert_eq!(jobs[0].body["request"]["prompt"], "A sting for the release");
    let out = structured(&accepted);
    assert_eq!(out["make_id"], JOB);
    assert_eq!(out["follow_up"]["action"], "status");
}

#[tokio::test]
async fn make_asks_the_person_when_the_client_can() {
    let stub = Stub::start(api(Arc::new(Mutex::new(("queued".into(), false))))).await;
    let tool = stub.tool();
    let client = Arc::new(FakeClient::default());

    *client.answer.lock().unwrap() = Some(json!({"action": "decline"}));
    let declined = run_with_call_peer(
        CallPeer::new(client.clone(), None, true),
        tool.execute(json!({"action": "make", "plan_id": QUOTE})),
    )
    .await
    .unwrap();
    assert!(text_of(&declined).contains("the person said no"));
    assert!(stub.posts_to("/create/jobs").is_empty());
    let asked = client.sent.lock().unwrap()[0].clone();
    assert_eq!(asked.0, "elicitation/create");
    let message = asked.1["message"].as_str().unwrap();
    assert!(
        message.contains("“Release Sting” and “Brand Board”"),
        "{message}"
    );
    assert!(message.contains("up to 412 credits"), "{message}");

    // A decline wins over an accepted price the agent passes.
    let still_no = run_with_call_peer(
        CallPeer::new(client.clone(), None, true),
        tool.execute(json!({"action": "make", "plan_id": QUOTE, "accepted_price_credits": 412})),
    )
    .await
    .unwrap();
    assert!(text_of(&still_no).contains("the person said no"));
    assert!(stub.posts_to("/create/jobs").is_empty());

    *client.answer.lock().unwrap() =
        Some(json!({"action": "accept", "content": {"confirm": true}}));
    let made = run_with_call_peer(
        CallPeer::new(client.clone(), None, true),
        tool.execute(json!({"action": "make", "plan_id": QUOTE})),
    )
    .await
    .unwrap();
    assert!(!made.is_error, "{}", text_of(&made));
    assert!(text_of(&made).contains("The person accepted up to 412 credits"));
    assert_eq!(stub.posts_to("/create/jobs").len(), 1);
}

#[tokio::test]
async fn a_stateless_client_is_asked_through_the_calls_result() {
    let stub = Stub::start(api(Arc::new(Mutex::new(("queued".into(), false))))).await;
    let tool = stub.tool();
    let client = Arc::new(FakeClient::default());

    // First call: the question goes back as the result; nothing is made.
    let first = CallPeer::new(client.clone(), None, false).with_inline_input(&json!({}));
    let asked = run_with_call_peer(
        first.clone(),
        tool.execute(json!({"action": "make", "plan_id": QUOTE, "accepted_price_credits": 412})),
    )
    .await
    .unwrap();
    assert!(
        text_of(&asked).starts_with("Asking the person"),
        "{}",
        text_of(&asked)
    );
    let question = first.take_input_required().expect("input_required");
    assert_eq!(question["resultType"], "input_required");
    let state = question["requestState"].as_str().unwrap().to_string();
    assert_eq!(state, format!("studio-make:{QUOTE}:412"));
    let form = &question["inputRequests"]["confirm-price"]["params"];
    assert!(form["message"]
        .as_str()
        .unwrap()
        .contains("up to 412 credits"));
    assert_eq!(
        form["requestedSchema"]["properties"]["confirm"]["type"],
        "boolean"
    );
    assert!(stub.posts_to("/create/jobs").is_empty());

    // The client calls again with a decline: still nothing.
    let declined = CallPeer::new(client.clone(), None, false).with_inline_input(&json!({
        "inputResponses": {"confirm-price": {"action": "decline"}},
        "requestState": state,
    }));
    let no = run_with_call_peer(
        declined,
        tool.execute(json!({"action": "make", "plan_id": QUOTE})),
    )
    .await
    .unwrap();
    assert!(text_of(&no).contains("the person said no"));
    assert!(stub.posts_to("/create/jobs").is_empty());

    // And with an accept: made.
    let accepted = CallPeer::new(client, None, false).with_inline_input(&json!({
        "inputResponses": {"confirm-price": {"action": "accept", "content": {"confirm": true}}},
        "requestState": format!("studio-make:{QUOTE}:412"),
    }));
    let made = run_with_call_peer(
        accepted,
        tool.execute(json!({"action": "make", "plan_id": QUOTE})),
    )
    .await
    .unwrap();
    assert!(!made.is_error, "{}", text_of(&made));
    assert_eq!(stub.posts_to("/create/jobs").len(), 1);
}

#[tokio::test]
async fn status_reports_progress_while_it_waits_and_result_shows_the_pieces() {
    let state = Arc::new(Mutex::new(("generating".to_string(), false)));
    let stub = Stub::start(api(state.clone())).await;
    let tool = stub.tool();
    let client = Arc::new(FakeClient::default());

    let running = run_with_call_peer(
        CallPeer::new(client.clone(), Some(json!("tok")), false),
        tool.execute(json!({"action": "status", "make_id": JOB, "wait_seconds": 0})),
    )
    .await
    .unwrap();
    let text = text_of(&running);
    assert!(
        text.starts_with("Judging the takes: Release Sting"),
        "{text}"
    );
    assert!(text.contains("Still going."), "{text}");
    let progress = client.sent.lock().unwrap().clone();
    assert_eq!(progress[0].0, "notifications/progress");
    assert_eq!(progress[0].1["progressToken"], "tok");
    assert_eq!(progress[0].1["total"], 100.0);

    *state.lock().unwrap() = ("succeeded".to_string(), true);
    let done = tool
        .execute(json!({"action": "status", "make_id": JOB}))
        .await
        .unwrap();
    let text = text_of(&done);
    assert!(text.starts_with("All 2 pieces made"), "{text}");
    assert!(text.contains("Charged: 250 credits."), "{text}");
    assert_eq!(structured(&done)["follow_up"]["action"], "result");

    let result = tool
        .execute(json!({"action": "result", "make_id": JOB}))
        .await
        .unwrap();
    let text = text_of(&result);
    assert!(text.contains("Idea: Two rising notes."), "{text}");
    assert!(
        text.contains("How it was made: Drawn by the engine."),
        "{text}"
    );
    let image = result.content.iter().find_map(|item| match item {
        ContentItem::Image { data, mime_type } => Some((data.clone(), mime_type.clone())),
        _ => None,
    });
    let (data, mime) = image.expect("the image inline");
    assert_eq!(mime, "image/png");
    use base64::Engine as _;
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(data)
            .unwrap(),
        PNG
    );
    let link = result.content.iter().find_map(|item| match item {
        ContentItem::ResourceLink {
            uri,
            name,
            mime_type,
            ..
        } => Some((uri.clone(), name.clone(), mime_type.clone())),
        _ => None,
    });
    let (uri, name, mime) = link.expect("the sound as a link");
    assert!(
        uri.starts_with(&format!("https://store.example/{VIDEO_ID}")),
        "{uri}"
    );
    assert_eq!(name, "Release Sting");
    assert_eq!(mime.as_deref(), Some("audio/mp4"));
    // The link serializes as MCP's resource_link.
    let link = result
        .content
        .iter()
        .find(|item| matches!(item, ContentItem::ResourceLink { .. }))
        .unwrap();
    let wire = serde_json::to_value(link).unwrap();
    assert_eq!(wire["type"], "resource_link");
    assert_eq!(wire["mimeType"], "audio/mp4");

    // Without a make_id, the latest making.
    let latest = tool.execute(json!({"action": "result"})).await.unwrap();
    assert_eq!(structured(&latest)["make_id"], JOB);
}

#[tokio::test]
async fn revise_plans_another_like_a_made_piece() {
    let stub = Stub::start(api(Arc::new(Mutex::new(("succeeded".into(), true))))).await;
    let tool = stub.tool();
    let result = tool
        .execute(json!({"action": "revise", "make_id": JOB, "piece": "brand-board", "change": "darker ground", "wait_seconds": 20}))
        .await
        .unwrap();
    assert!(!result.is_error, "{}", text_of(&result));
    let posted = stub.posts_to("/create/quotes");
    let request = &posted[0].body["request"];
    let prompt = request["prompt"].as_str().unwrap();
    assert!(prompt.starts_with("A sting for the release\n\nNow make one more piece: another “Brand Board” (image · wide 16:9), like the attached image. This time: darker ground."), "{prompt}");
    assert_eq!(
        request["references"],
        json!([{"contentId": IMAGE_ID, "role": "style"}])
    );
    assert!(stub.posts_to("/create/jobs").is_empty());
}

#[tokio::test]
async fn revise_plans_a_piece_that_was_not_made_again() {
    let stub = Stub::start(api(Arc::new(Mutex::new(("generating".into(), false))))).await;
    let tool = stub.tool();
    let result = tool
        .execute(json!({"action": "revise", "make_id": JOB, "piece": 1, "change": "a darker ground", "wait_seconds": 20}))
        .await
        .unwrap();
    assert!(!result.is_error, "{}", text_of(&result));
    let posted = stub.posts_to("/create/quotes");
    let request = &posted[0].body["request"];
    assert_eq!(
        request["prompt"],
        "A sting for the release\n\nNow make only this piece, which wasn't made before: “Brand Board” (image · wide 16:9). This time: a darker ground."
    );
    assert!(request.get("references").is_none());
}

#[tokio::test]
async fn refusals_are_plain_and_nothing_is_spent() {
    let stub = Stub::start(Arc::new(|request: &Recorded| match request.path.as_str() {
        "/create/quotes" => Answer::error(
            403,
            "FORBIDDEN",
            "Forbidden: Create isn't available for this account yet.",
        ),
        _ => Answer::error(404, "NOT_FOUND", "Not found"),
    }))
    .await;
    let tool = stub.tool();
    let result = tool
        .execute(json!({"action": "plan", "prompt": "a film"}))
        .await
        .unwrap();
    assert!(result.is_error);
    assert_eq!(text_of(&result), format!("Not planned: {}", api::NOT_OPEN));

    let credits_short = Stub::start(Arc::new(|request: &Recorded| {
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", p) if p.starts_with("/projects/") => {
                Answer::ok(json!({"id": PROJECT, "name": "studio", "workspace_id": WORKSPACE}))
            }
            ("GET", p) if p.starts_with("/create/quotes/") => Answer::ok(quote("ready")),
            ("POST", "/create/jobs") => Answer::raw(
                402,
                json!({"success": false, "error": {
            "code": "INSUFFICIENT_CREDITS", "message": "You've run out of credits.",
            "details": {"required": 412, "available": 100}}}),
            ),
            _ => Answer::error(404, "NOT_FOUND", "Not found"),
        }
    }))
    .await;
    let made = credits_short
        .tool()
        .execute(json!({"action": "make", "plan_id": QUOTE, "accepted_price_credits": 412}))
        .await
        .unwrap();
    assert!(made.is_error);
    assert_eq!(
        text_of(&made),
        "Not made: Not enough credits: this needs up to 412 and the account has 100. Add credits, then make it again. Nothing was charged."
    );
}

#[test]
fn spend_past_the_hold_says_the_charge_stops_there() {
    let stub_config = TestFixtures::test_config();
    let client = ContextStreamClient::new(stub_config.clone());
    let session = Arc::new(SessionManager::new(client.clone(), stub_config));
    let tool = StudioTool::new(client, session, Arc::new(StudioGate::new()));
    let mut running = job("generating", false);
    running["make"]["spentCredits"] = json!(1193);
    running["quotedCredits"] = json!(919);
    let text = text_of(&tool.status_result(Uuid::nil(), &running, &[]));
    assert!(
        text.contains("The studio has spent 1,193 so far; the charge stops at the 919 held."),
        "{text}"
    );
    let mut done = job("succeeded", true);
    done["chargedCredits"] = json!(919);
    done["make"]["spentCredits"] = json!(1926);
    let text = text_of(&tool.status_result(Uuid::nil(), &done, &[]));
    assert!(text.contains("Charged: 919 credits."), "{text}");
}

#[test]
fn context_is_cut_to_fit_and_said_so() {
    let (notes, said) = notes_from(Some(&json!("one plain note"))).unwrap();
    assert_eq!(notes, vec![json!({"title": "", "text": "one plain note"})]);
    assert!(said.is_empty());

    let long = "x".repeat(MAX_NOTE_CHARS + 10);
    let many: Vec<Value> = (0..6)
        .map(|i| json!({"title": format!("note {i}"), "text": long.clone()}))
        .collect();
    let (notes, said) = notes_from(Some(&Value::Array(many))).unwrap();
    assert_eq!(notes.len(), 2);
    let total: usize = notes
        .iter()
        .map(|n| n["text"].as_str().unwrap().chars().count())
        .sum();
    assert_eq!(total, MAX_NOTES_CHARS);
    assert!(said.iter().any(|s| s.contains("first 4 notes")));
    assert!(said.iter().any(|s| s.contains("cut to")));
    assert!(notes_from(Some(&json!(7))).is_err());
}

#[test]
fn pieces_are_named_by_index_or_id() {
    assert_eq!(piece_selector(&json!(1)), Some(PieceSelector::Index(1)));
    assert_eq!(piece_selector(&json!("0")), Some(PieceSelector::Index(0)));
    assert_eq!(
        piece_selector(&json!("brand-board")),
        Some(PieceSelector::Id("brand-board".into()))
    );
    assert_eq!(piece_selector(&json!(" ")), None);
}
