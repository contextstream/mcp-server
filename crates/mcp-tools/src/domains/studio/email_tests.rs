//! Tests for the studio's email actions against a stub of Create's email routes: the offer,
//! drafting, the person's go before a send (elicitation, confirmed recipient count), the
//! test-send recipients, reports and refusals.

use super::email::{
    email_refusal, is_resend_test_address, outline, percent, send_at_of, send_key, test_recipients,
    time_zone_of, when_words, Topic, EMAIL_ACTIONS, NOT_OFFERED,
};
use super::*;
use crate::peer::{run_with_call_peer, CallPeer, PeerChannel};
use crate::testing::TestFixtures;
use mcp_client::studio::StudioAnswer;
use mcp_types::Config;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const WORKSPACE: &str = "11111111-1111-4111-8111-111111111111";
const PROJECT: &str = "22222222-2222-4222-8222-222222222222";
const CAMPAIGN: &str = "77777777-7777-4777-8777-777777777777";
const AUDIENCE: &str = "88888888-8888-4888-8888-888888888888";
const SEGMENT: &str = "99999999-9999-4999-8999-999999999999";

// ============================================================================
// A stub of Create's email routes
// ============================================================================

#[derive(Debug, Clone)]
struct Recorded {
    method: String,
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

struct Answer {
    status: u16,
    body: Value,
}

impl Answer {
    fn ok(data: Value) -> Self {
        Self::status(200, data)
    }

    fn status(status: u16, data: Value) -> Self {
        Self {
            status,
            body: json!({"success": true, "data": data, "error": null}),
        }
    }

    fn error(status: u16, code: &str, message: &str) -> Self {
        Self {
            status,
            body: json!({"success": false, "data": null, "error": {"code": code, "message": message}}),
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
                    let body = answer.body.to_string();
                    let head = format!(
                        "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        answer.status,
                        body.len()
                    );
                    let _ = socket.write_all(head.as_bytes()).await;
                    let _ = socket.write_all(body.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Self { url, requests }
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }

    fn calls_to(&self, method: &str, path: &str) -> Vec<Recorded> {
        self.requests()
            .into_iter()
            .filter(|r| r.method == method && r.path == path)
            .collect()
    }

    fn email_calls(&self) -> Vec<Recorded> {
        self.requests()
            .into_iter()
            .filter(|r| r.path.starts_with("/create/email"))
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

fn campaign(status: &str) -> Value {
    json!({
        "id": CAMPAIGN,
        "workspaceId": WORKSPACE,
        "name": "Winter sale",
        "status": status,
        "subject": "Our winter sale starts today",
        "preheader": "Twenty percent off everything until Sunday.",
        "content": {"version": 1, "blocks": [
            {"id": "b1", "type": "heading", "text": "Winter is here"},
            {"id": "b2", "type": "text", "text": "Hi {{first_name|there}}, everything is twenty percent off until Sunday."},
            {"id": "b3", "type": "spacer", "size": "m"},
            {"id": "b4", "type": "button", "label": "Shop the sale", "url": "https://northwind.example/sale"},
            {"id": "b5", "type": "image", "contentId": AUDIENCE, "alt": "Beans in the snow"},
            {"id": "b6", "type": "divider"},
        ]},
        "brand": {"name": "Northwind Coffee"},
        "audienceId": AUDIENCE,
        "segmentId": null,
        "audience": {"id": AUDIENCE, "name": "Coffee club"},
        "segment": null,
        "sender": {"fromName": "Northwind", "fromEmail": "hello@news.northwind.example", "replyTo": null},
        "schedule": if status == "scheduled" { json!({"sendAt": "2026-11-02T09:00:00Z", "timeZone": "Europe/London"}) } else { Value::Null },
        "progress": {"recipients": 37, "queued": 0, "sent": if status == "sent" { 37 } else { 0 }, "failed": 0, "skipped": 3},
        "stats": {"delivered": 36, "opened": 20, "clicked": 6, "bounced": 1, "complained": 0, "unsubscribed": 1},
        "lastError": null,
        "revision": 4,
    })
}

/// Create's email routes as a connected, offered workspace answers them.
fn api(status: Arc<Mutex<String>>, recipients: i64) -> Responder {
    Arc::new(move |request: &Recorded| {
        let path = request.path.as_str();
        let campaign_path = format!("/create/email/campaigns/{CAMPAIGN}");
        match (request.method.as_str(), path) {
            ("GET", p) if p.starts_with("/projects/") => {
                Answer::ok(json!({"id": PROJECT, "name": "studio", "workspace_id": WORKSPACE}))
            }
            ("GET", "/create/catalog") => Answer::ok(json!({
                "make": {"status": "available"},
                "email": {"name": "Email", "status": "available"}
            })),
            ("GET", "/auth/me") => Answer::ok(json!({"id": "u", "email": "Ada@Northwind.example"})),
            ("GET", "/create/email/connection") => Answer::ok(json!({
                "connected": true, "key": {"last4": "Qa7x", "access": "full_access"},
                "sender": {"fromName": "Northwind", "fromEmail": "hello@news.northwind.example"},
                "ready": true, "missing": []
            })),
            ("GET", "/create/email/audiences") => Answer::ok(json!([
                {"id": AUDIENCE, "name": "Coffee club", "contacts": 40, "subscribed": 37}
            ])),
            ("GET", "/create/email/segments") => Answer::ok(json!([
                {"id": SEGMENT, "audienceId": AUDIENCE, "name": "Regulars", "contacts": 12}
            ])),
            ("POST", "/create/email/campaigns/draft") => Answer::status(
                201,
                json!({
                    "campaign": campaign("draft"),
                    "suggestions": {
                        "subjects": ["Our winter sale starts today", "Twenty percent off, this week only", "Warm up with 20% off"],
                        "preheaders": ["Twenty percent off everything until Sunday.", "Beans, brewers and gifts."]
                    }
                }),
            ),
            ("GET", "/create/email/campaigns") => Answer::ok(json!([
                campaign("sent"),
                campaign("scheduled"),
                campaign("draft")
            ])),
            ("GET", p) if p == campaign_path => Answer::ok(campaign(&status.lock().unwrap())),
            ("GET", p) if p == format!("{campaign_path}/preview") => Answer::ok(json!({
                "subject": "Our winter sale starts today",
                "preheader": "Twenty percent off everything until Sunday.",
                "from": "Northwind <hello@news.northwind.example>",
                "html": "<html></html>", "text": "",
                "warnings": ["The image “Beans in the snow” has no link."],
                "recipients": recipients
            })),
            ("POST", p) if p == format!("{campaign_path}/test") => {
                let to = request
                    .body
                    .get("to")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_else(|| vec![json!("ada@northwind.example")]);
                Answer::ok(
                    json!({"sent": to.iter().enumerate().map(|(i, e)| json!({"email": e, "id": format!("re_{i}")})).collect::<Vec<_>>()}),
                )
            }
            ("POST", p) if p == format!("{campaign_path}/schedule") => {
                let send_at = request.body["sendAt"].as_str().unwrap_or("now");
                let mut scheduled = campaign(if send_at == "now" {
                    "sending"
                } else {
                    "scheduled"
                });
                scheduled["schedule"] =
                    json!({"sendAt": "2026-11-02T09:00:00Z", "timeZone": request.body["timeZone"]});
                Answer::ok(scheduled)
            }
            ("POST", p) if p == format!("{campaign_path}/cancel") => {
                let now = status.lock().unwrap().clone();
                let mut cancelled = campaign(if now == "sending" {
                    "cancelled"
                } else {
                    "draft"
                });
                cancelled["progress"]["sent"] = json!(12);
                Answer::ok(cancelled)
            }
            ("GET", p) if p == format!("{campaign_path}/report") => Answer::ok(json!({
                "totals": {"recipients": 40, "sent": 37, "delivered": 36, "opened": 20, "clicked": 6,
                           "bounced": 1, "complained": 1, "unsubscribed": 1, "failed": 0, "skipped": 3},
                "rates": {"delivered": 0.973, "opened": 0.5556, "clicked": 0.1667, "bounced": 0.027,
                          "complained": 0.00027, "unsubscribed": 0.027},
                "timeline": [],
                "links": [{"url": "https://northwind.example/sale", "clicks": 9, "people": 6}],
                "bounces": [{"email": "bounced@resend.dev", "type": "Permanent", "detail": "550 5.1.1 Unknown user", "at": "2026-10-29T18:00:00Z"}],
                "complaints": [{"email": "complained@resend.dev", "at": "2026-10-29T18:01:00Z"}]
            })),
            ("GET", "/create/email/stats") => Answer::ok(json!({
                "days": [], "campaigns": 3, "contacts": 40, "suppressed": 3,
                "totals": {"sent": 74, "delivered": 72, "opened": 40, "clicked": 12, "bounced": 2, "complained": 1, "unsubscribed": 2},
                "rates": {"delivered": 0.973, "opened": 0.5556, "clicked": 0.1667, "bounced": 0.027, "complained": 0.0135, "unsubscribed": 0.027}
            })),
            _ => Answer::error(404, "NOT_FOUND", "Not found: nothing here"),
        }
    })
}

fn usual() -> Responder {
    api(Arc::new(Mutex::new("draft".to_string())), 37)
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
// The actions
// ============================================================================

#[test]
fn every_email_action_is_listed_described_and_in_the_schema() {
    for action in EMAIL_ACTIONS {
        assert!(VALID_ACTIONS.contains(action), "{action}");
        assert!(TOOL_REFERENCE.contains(&format!("- {action}:")), "{action}");
    }
    assert!(DESCRIPTION.chars().count() <= 1024);
    let schema = input_schema();
    let properties = schema["properties"].as_object().unwrap();
    for field in [
        "campaign_id",
        "audience_id",
        "segment_id",
        "send_at",
        "time_zone",
        "confirm_recipients",
        "to",
        "status",
        "days",
    ] {
        assert!(properties.contains_key(field), "{field}");
    }
    assert!(!properties.keys().any(|k| k.contains("model")));
}

#[tokio::test]
async fn email_isnt_offered_without_the_catalogs_email_block() {
    let stub = Stub::start(Arc::new(|request: &Recorded| match request.path.as_str() {
        "/create/catalog" => Answer::ok(json!({"make": {"status": "available"}})),
        _ => Answer::ok(json!([])),
    }))
    .await;
    let tool = stub.tool();
    for action in EMAIL_ACTIONS {
        let result = tool
            .execute(json!({"action": action, "campaign_id": CAMPAIGN, "prompt": "x", "confirm_recipients": 37}))
            .await
            .unwrap();
        assert!(result.is_error, "{action}");
        assert_eq!(text_of(&result), NOT_OFFERED, "{action}");
    }
    assert!(stub.email_calls().is_empty(), "nothing is asked of email");

    // Create closed to the account altogether: the same plain answer.
    let closed = Stub::start(Arc::new(|_: &Recorded| {
        Answer::error(
            403,
            "FORBIDDEN",
            "Forbidden: Create isn't available for this account yet.",
        )
    }))
    .await;
    let result = closed
        .tool()
        .execute(json!({"action": "email_audiences"}))
        .await
        .unwrap();
    assert_eq!(text_of(&result), NOT_OFFERED);
}

#[tokio::test]
async fn audiences_list_their_segments_and_the_connection() {
    let stub = Stub::start(usual()).await;
    let result = stub
        .tool()
        .execute(json!({"action": "email_audiences"}))
        .await
        .unwrap();
    let text = text_of(&result);
    assert!(!result.is_error, "{text}");
    assert!(text.contains("Resend is connected (key ending Qa7x), sending as Northwind <hello@news.northwind.example>."), "{text}");
    assert!(
        text.contains(&format!(
            "- Coffee club: 40 contacts, 37 subscribed (audience_id {AUDIENCE})"
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "segment Regulars: 12 contacts (segment_id {SEGMENT})"
        )),
        "{text}"
    );
    let out = structured(&result);
    assert_eq!(out["audiences"][0]["segments"][0]["segment_id"], SEGMENT);
    assert_eq!(out["connection"]["ready"], true);
}

#[tokio::test]
async fn a_draft_is_written_from_one_prompt_and_sends_nothing() {
    let stub = Stub::start(usual()).await;
    let result = stub
        .tool()
        .execute(json!({
            "action": "email_draft",
            "prompt": "announce our winter sale to customers",
            "audience_id": AUDIENCE,
        }))
        .await
        .unwrap();
    let text = text_of(&result);
    assert!(!result.is_error, "{text}");
    let drafts = stub.calls_to("POST", "/create/email/campaigns/draft");
    assert_eq!(drafts.len(), 1);
    assert_eq!(
        drafts[0].body["prompt"],
        "announce our winter sale to customers"
    );
    assert_eq!(drafts[0].body["audienceId"], AUDIENCE);
    assert_eq!(drafts[0].body["workspaceId"], WORKSPACE);
    assert!(text.starts_with(&format!("Drafted “Winter sale” in Northwind Coffee's brand (campaign_id {CAMPAIGN}). Nothing is sent.")), "{text}");
    assert!(
        text.contains("Subject: Our winter sale starts today"),
        "{text}"
    );
    assert!(
        text.contains(
            "Other subjects: “Twenty percent off, this week only”, “Warm up with 20% off”"
        ),
        "{text}"
    );
    assert!(text.contains("1. Heading: Winter is here"), "{text}");
    assert!(
        text.contains("3. Button: Shop the sale → https://northwind.example/sale"),
        "{text}"
    );
    assert!(text.contains("To: Coffee club (37 people)."), "{text}");
    assert!(
        text.contains("Check: The image “Beans in the snow” has no link."),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "https://ctxproduct.com/studio/email/campaigns/{CAMPAIGN}"
        )),
        "{text}"
    );
    let out = structured(&result);
    assert_eq!(out["campaign_id"], CAMPAIGN);
    assert_eq!(out["recipients"], 37);
    // Nothing went out.
    assert!(stub
        .requests()
        .iter()
        .all(|r| !r.path.ends_with("/schedule") && !r.path.ends_with("/test")));

    let empty = stub
        .tool()
        .execute(json!({"action": "email_draft", "prompt": "  "}))
        .await;
    assert!(matches!(empty, Err(Error::Validation(_))));
}

#[tokio::test]
async fn a_send_needs_the_persons_go_and_the_count_they_saw() {
    let stub = Stub::start(usual()).await;
    let tool = stub.tool();
    let schedule = format!("/create/email/campaigns/{CAMPAIGN}/schedule");

    let unasked = tool
        .execute(json!({"action": "email_send", "campaign_id": CAMPAIGN}))
        .await
        .unwrap();
    let text = text_of(&unasked);
    assert!(
        text.starts_with("Not sent yet: the person's go is needed first."),
        "{text}"
    );
    assert!(text.contains("“Winter sale” (subject “Our winter sale starts today”, from Northwind <hello@news.northwind.example>) to 37 people now"), "{text}");
    assert!(text.contains("confirm_recipients 37"), "{text}");
    assert_eq!(
        structured(&unasked)["confirm_with"]["confirm_recipients"],
        37
    );

    let wrong = tool
        .execute(json!({"action": "email_send", "campaign_id": CAMPAIGN, "confirm_recipients": 30}))
        .await
        .unwrap();
    assert!(
        text_of(&wrong)
            .starts_with("Not sent: it now goes to 37 people, not the 30 the person agreed to."),
        "{}",
        text_of(&wrong)
    );
    assert!(stub.calls_to("POST", &schedule).is_empty());

    let sent = tool
        .execute(json!({"action": "email_send", "campaign_id": CAMPAIGN, "confirm_recipients": 37}))
        .await
        .unwrap();
    assert!(!sent.is_error, "{}", text_of(&sent));
    assert!(
        text_of(&sent).starts_with("Sending “Winter sale” to 37 people now"),
        "{}",
        text_of(&sent)
    );
    let posts = stub.calls_to("POST", &schedule);
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].body["sendAt"], "now");
    assert_eq!(posts[0].body["timeZone"], "UTC");
    assert_eq!(posts[0].body["revision"], 4);
    assert_eq!(
        posts[0].header("idempotency-key"),
        Some(format!("email-send-{CAMPAIGN}-r4-now-UTC").as_str())
    );
    assert_eq!(structured(&sent)["follow_up"]["action"], "email_report");
}

#[tokio::test]
async fn a_send_asks_the_person_when_the_client_can() {
    let stub = Stub::start(usual()).await;
    let tool = stub.tool();
    let client = Arc::new(FakeClient::default());
    let schedule = format!("/create/email/campaigns/{CAMPAIGN}/schedule");
    let ask = json!({
        "action": "email_send", "campaign_id": CAMPAIGN,
        "send_at": "2026-11-02T09:00", "time_zone": "Europe/London",
    });

    *client.answer.lock().unwrap() = Some(json!({"action": "decline"}));
    let declined = run_with_call_peer(
        CallPeer::new(client.clone(), None, true),
        tool.execute(ask.clone()),
    )
    .await
    .unwrap();
    assert!(
        text_of(&declined).starts_with("Not sent: the person didn't say go"),
        "{}",
        text_of(&declined)
    );
    let asked = client.sent.lock().unwrap()[0].clone();
    assert_eq!(asked.0, "elicitation/create");
    let message = asked.1["message"].as_str().unwrap();
    assert!(message.contains("Send “Winter sale”"), "{message}");
    assert!(
        message.contains("to 37 people at 2026-11-02 09:00 (Europe/London)?"),
        "{message}"
    );

    // A count the agent passes doesn't outweigh the person's no.
    let still_no = run_with_call_peer(
        CallPeer::new(client.clone(), None, true),
        tool.execute(json!({
            "action": "email_send", "campaign_id": CAMPAIGN, "confirm_recipients": 37,
            "send_at": "2026-11-02T09:00", "time_zone": "Europe/London",
        })),
    )
    .await
    .unwrap();
    assert!(text_of(&still_no).starts_with("Not sent"));

    // Accepting without ticking "Send it" isn't a go.
    *client.answer.lock().unwrap() =
        Some(json!({"action": "accept", "content": {"confirm": false}}));
    let unticked = run_with_call_peer(
        CallPeer::new(client.clone(), None, true),
        tool.execute(ask.clone()),
    )
    .await
    .unwrap();
    assert!(text_of(&unticked).starts_with("Not sent"));
    assert!(stub.calls_to("POST", &schedule).is_empty());

    *client.answer.lock().unwrap() =
        Some(json!({"action": "accept", "content": {"confirm": true}}));
    let scheduled =
        run_with_call_peer(CallPeer::new(client.clone(), None, true), tool.execute(ask))
            .await
            .unwrap();
    let text = text_of(&scheduled);
    assert!(!scheduled.is_error, "{text}");
    assert!(text.starts_with("Scheduled “Winter sale” to 37 people at 2026-11-02 09:00 (Europe/London), that is 2026-11-02 09:00 UTC."), "{text}");
    assert!(
        text.contains("The person said go when the client asked them."),
        "{text}"
    );
    let posts = stub.calls_to("POST", &schedule);
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].body["sendAt"], "2026-11-02T09:00");
    assert_eq!(posts[0].body["timeZone"], "Europe/London");
}

#[tokio::test]
async fn a_stateless_client_is_asked_through_the_calls_result() {
    let stub = Stub::start(usual()).await;
    let tool = stub.tool();
    let client = Arc::new(FakeClient::default());
    let first = CallPeer::new(client.clone(), None, false).with_inline_input(&json!({}));
    let asked = run_with_call_peer(
        first.clone(),
        tool.execute(
            json!({"action": "email_send", "campaign_id": CAMPAIGN, "confirm_recipients": 37}),
        ),
    )
    .await
    .unwrap();
    assert!(
        text_of(&asked).starts_with("Asking the person"),
        "{}",
        text_of(&asked)
    );
    let question = first.take_input_required().expect("input_required");
    let state = question["requestState"].as_str().unwrap().to_string();
    assert_eq!(state, format!("studio-email-send:{CAMPAIGN}:37:now:UTC"));
    assert!(stub
        .calls_to(
            "POST",
            &format!("/create/email/campaigns/{CAMPAIGN}/schedule")
        )
        .is_empty());

    let accepted = CallPeer::new(client, None, false).with_inline_input(&json!({
        "inputResponses": {"confirm-send": {"action": "accept", "content": {"confirm": true}}},
        "requestState": state,
    }));
    let sent = run_with_call_peer(
        accepted,
        tool.execute(json!({"action": "email_send", "campaign_id": CAMPAIGN})),
    )
    .await
    .unwrap();
    assert!(!sent.is_error, "{}", text_of(&sent));
    assert_eq!(
        stub.calls_to(
            "POST",
            &format!("/create/email/campaigns/{CAMPAIGN}/schedule")
        )
        .len(),
        1
    );
}

#[tokio::test]
async fn a_send_is_refused_plainly_when_it_cant_go() {
    // Already sent: not again.
    let sent_stub = Stub::start(api(Arc::new(Mutex::new("sent".to_string())), 37)).await;
    let again = sent_stub
        .tool()
        .execute(json!({"action": "email_send", "campaign_id": CAMPAIGN, "confirm_recipients": 37}))
        .await
        .unwrap();
    assert!(text_of(&again).starts_with("Not sent again: “Winter sale” was sent."));

    // No one to send to.
    let empty = Stub::start(api(Arc::new(Mutex::new("draft".to_string())), 0)).await;
    let nobody = empty
        .tool()
        .execute(json!({"action": "email_send", "campaign_id": CAMPAIGN, "confirm_recipients": 0}))
        .await
        .unwrap();
    assert!(
        text_of(&nobody).contains("has no one to send to yet"),
        "{}",
        text_of(&nobody)
    );

    // A local time needs its zone; a malformed time is refused before anything is read.
    let stub = Stub::start(usual()).await;
    let no_zone = stub
        .tool()
        .execute(
            json!({"action": "email_send", "campaign_id": CAMPAIGN, "send_at": "2026-11-02T09:00"}),
        )
        .await;
    assert!(matches!(no_zone, Err(Error::Validation(m)) if m.contains("time_zone")));
    let malformed = stub
        .tool()
        .execute(
            json!({"action": "email_send", "campaign_id": CAMPAIGN, "send_at": "next tuesday"}),
        )
        .await;
    assert!(matches!(malformed, Err(Error::Validation(_))));
    assert!(stub
        .calls_to("GET", &format!("/create/email/campaigns/{CAMPAIGN}"))
        .is_empty());

    // Create's own reason when it refuses the schedule.
    let refusing = Stub::start(Arc::new(|request: &Recorded| {
        if request.path.ends_with("/schedule") {
            Answer::error(
                422,
                "UNPROCESSABLE_ENTITY",
                "Validation error: Add a postal address for the footer before sending.",
            )
        } else {
            (usual())(request)
        }
    }))
    .await;
    let refused = refusing
        .tool()
        .execute(json!({"action": "email_send", "campaign_id": CAMPAIGN, "confirm_recipients": 37}))
        .await
        .unwrap();
    assert!(refused.is_error);
    assert_eq!(
        text_of(&refused),
        "Not sent: Add a postal address for the footer before sending."
    );
}

#[tokio::test]
async fn test_sends_go_only_to_the_person_or_resends_test_addresses() {
    let stub = Stub::start(usual()).await;
    let tool = stub.tool();
    let tests = format!("/create/email/campaigns/{CAMPAIGN}/test");

    let elsewhere = tool
        .execute(json!({"action": "email_test", "campaign_id": CAMPAIGN, "to": ["ada@northwind.example", "grace@elsewhere.example"]}))
        .await
        .unwrap();
    assert!(elsewhere.is_error);
    assert!(
        text_of(&elsewhere).ends_with("not grace@elsewhere.example."),
        "{}",
        text_of(&elsewhere)
    );
    assert!(stub.calls_to("POST", &tests).is_empty());

    let own_and_resend = tool
        .execute(json!({"action": "email_test", "campaign_id": CAMPAIGN, "to": "ADA@northwind.example, delivered+one@resend.dev"}))
        .await
        .unwrap();
    assert!(!own_and_resend.is_error, "{}", text_of(&own_and_resend));
    let posts = stub.calls_to("POST", &tests);
    assert_eq!(posts.len(), 1);
    assert_eq!(
        posts[0].body["to"],
        json!(["ada@northwind.example", "delivered+one@resend.dev"])
    );

    // By default the API sends it to the person; the tool sends no list.
    let default = tool
        .execute(json!({"action": "email_test", "campaign_id": CAMPAIGN}))
        .await
        .unwrap();
    assert!(
        text_of(&default).starts_with("Test sent to ada@northwind.example"),
        "{}",
        text_of(&default)
    );
    assert!(stub.calls_to("POST", &tests)[1].body.get("to").is_none());

    let too_many = tool
        .execute(json!({"action": "email_test", "campaign_id": CAMPAIGN,
            "to": ["delivered+1@resend.dev", "delivered+2@resend.dev", "delivered+3@resend.dev",
                   "delivered+4@resend.dev", "delivered+5@resend.dev", "delivered+6@resend.dev"]}))
        .await;
    assert!(matches!(too_many, Err(Error::Validation(_))));
}

#[tokio::test]
async fn campaigns_cancel_and_reports_read_plainly() {
    let state = Arc::new(Mutex::new("sending".to_string()));
    let stub = Stub::start(api(state.clone(), 37)).await;
    let tool = stub.tool();

    let list = tool
        .execute(json!({"action": "email_campaigns", "limit": 2}))
        .await
        .unwrap();
    let text = text_of(&list);
    assert!(text.starts_with("The 2 newest of 3 campaigns:"), "{text}");
    assert!(
        text.contains("- Winter sale: sent to 37 people: 36 delivered, 20 opened, 6 clicked"),
        "{text}"
    );
    assert!(
        text.contains("- Winter sale: scheduled for 2026-11-02 09:00 UTC"),
        "{text}"
    );
    let bad_status = tool
        .execute(json!({"action": "email_campaigns", "status": "lost"}))
        .await;
    assert!(matches!(bad_status, Err(Error::Validation(_))));

    let stopped = tool
        .execute(json!({"action": "email_cancel", "campaign_id": CAMPAIGN}))
        .await
        .unwrap();
    assert_eq!(
        text_of(&stopped),
        "Stopped “Winter sale”: 12 sent before it stopped; no one else will get it."
    );
    *state.lock().unwrap() = "scheduled".to_string();
    let unscheduled = tool
        .execute(json!({"action": "email_cancel", "campaign_id": CAMPAIGN}))
        .await
        .unwrap();
    assert!(text_of(&unscheduled).starts_with("Unscheduled “Winter sale”"));

    *state.lock().unwrap() = "sent".to_string();
    let report = tool
        .execute(json!({"action": "email_report", "campaign_id": CAMPAIGN}))
        .await
        .unwrap();
    let text = text_of(&report);
    assert!(!report.is_error, "{text}");
    assert!(
        text.contains("Sent 37 · delivered 36 (97.3%) · opened 20 (55.6%) · clicked 6 (16.7%)"),
        "{text}"
    );
    assert!(text.contains("complaints 1 (0.03%)"), "{text}");
    assert!(
        text.contains("- https://northwind.example/sale: 9 clicks by 6 people"),
        "{text}"
    );
    assert!(
        text.contains("- bounced@resend.dev (Permanent): 550 5.1.1 Unknown user"),
        "{text}"
    );
    assert!(
        text.contains("Marked as spam by: complained@resend.dev"),
        "{text}"
    );

    let trends = tool
        .execute(json!({"action": "email_report"}))
        .await
        .unwrap();
    let text = text_of(&trends);
    assert!(
        text.starts_with("The last 30 days: 3 campaigns, sent 74."),
        "{text}"
    );
    assert!(
        text.contains("Contacts: 40; on the suppression list: 3."),
        "{text}"
    );
    let stats = stub.calls_to("GET", "/create/email/stats");
    assert!(stats[0].query.contains("days=30"), "{}", stats[0].query);
}

#[tokio::test]
async fn a_schedule_answered_as_a_draft_is_never_called_sent() {
    // A repeated idempotency key answers the campaign as it is.
    let stub = Stub::start(Arc::new(|request: &Recorded| {
        if request.path.ends_with("/schedule") {
            Answer::ok(campaign("draft"))
        } else {
            (usual())(request)
        }
    }))
    .await;
    let result = stub
        .tool()
        .execute(json!({"action": "email_send", "campaign_id": CAMPAIGN, "confirm_recipients": 37}))
        .await
        .unwrap();
    assert!(result.is_error);
    assert!(
        text_of(&result).starts_with("Not sent: Create kept “Winter sale” as draft."),
        "{}",
        text_of(&result)
    );
}

// ============================================================================
// Words
// ============================================================================

fn answer(status: u16, body: Value) -> StudioAnswer {
    let data = body.get("data").cloned().unwrap_or(Value::Null);
    StudioAnswer {
        status,
        data,
        body,
        retry_after: None,
    }
}

#[test]
fn refusals_keep_creates_sentence_and_name_what_is_missing() {
    let not_offered = answer(
        404,
        json!({"success": false, "error": {"message": "Not found: Email isn't offered to this account yet."}}),
    );
    assert_eq!(email_refusal(&not_offered, Topic::Campaign), NOT_OFFERED);
    let missing = answer(
        404,
        json!({"success": false, "error": {"message": "Not found: No such campaign."}}),
    );
    assert!(email_refusal(&missing, Topic::Campaign).starts_with("There is no such campaign"));
    let busy = answer(
        429,
        json!({"success": false, "error": {"message": "Too many requests: Twenty drafts an hour is the most; try again later."}}),
    );
    assert_eq!(
        email_refusal(&busy, Topic::Other),
        "Twenty drafts an hour is the most; try again later."
    );
    let person = answer(
        403,
        json!({"success": false, "error": {"message": "Forbidden: A person sends campaigns."}}),
    );
    assert_eq!(
        email_refusal(&person, Topic::Campaign),
        "A person sends campaigns."
    );
    let down = answer(502, Value::String("<html>".into()));
    assert!(email_refusal(&down, Topic::Other).contains("isn't answering"));
    let signed_out = answer(401, json!({}));
    assert!(email_refusal(&signed_out, Topic::Other).starts_with("ContextStream didn't accept"));
}

#[test]
fn only_resends_test_addresses_count_as_test_addresses() {
    for ok in [
        "delivered@resend.dev",
        "bounced@resend.dev",
        "complained@resend.dev",
        "suppressed@resend.dev",
        "delivered+launch-1@resend.dev",
    ] {
        assert!(is_resend_test_address(ok), "{ok}");
    }
    for no in [
        "someone@resend.dev",
        "delivered@resend.dev.evil.example",
        "delivered@evil.example",
        "delivered+@resend.dev",
        "delivered+a b@resend.dev",
        "resend.dev",
    ] {
        assert!(!is_resend_test_address(no), "{no}");
    }
    assert_eq!(
        test_recipients(Some(&json!("A@x.example; a@x.example, b@x.example"))).unwrap(),
        vec!["a@x.example", "b@x.example"]
    );
    assert!(test_recipients(Some(&json!(7))).is_err());
}

#[test]
fn send_times_and_zones_are_read_as_create_reads_them() {
    assert_eq!(send_at_of(None).unwrap(), "now");
    assert_eq!(send_at_of(Some("NOW")).unwrap(), "now");
    assert_eq!(
        send_at_of(Some("2026-11-02 09:00")).unwrap(),
        "2026-11-02T09:00"
    );
    assert_eq!(
        send_at_of(Some("2026-11-02T09:00:30")).unwrap(),
        "2026-11-02T09:00"
    );
    assert!(send_at_of(Some("2026-11-02T25:00")).is_err());
    assert!(send_at_of(Some("tomorrow")).is_err());
    assert_eq!(time_zone_of(None, "now").unwrap(), "UTC");
    assert!(time_zone_of(None, "2026-11-02T09:00").is_err());
    assert_eq!(
        time_zone_of(Some("America/New_York"), "2026-11-02T09:00").unwrap(),
        "America/New_York"
    );
    assert!(time_zone_of(Some("Europe/London; DROP"), "now").is_err());
    assert_eq!(when_words("now", "UTC"), "now");
    assert_eq!(
        when_words("2026-11-02T09:00", "Europe/London"),
        "at 2026-11-02 09:00 (Europe/London)"
    );
    let campaign = Uuid::parse_str(CAMPAIGN).unwrap();
    let key = send_key(campaign, 4, "2026-11-02T09:00", "Europe/London");
    assert_eq!(
        key,
        format!("email-send-{CAMPAIGN}-r4-2026-11-02T09-00-Europe-London")
    );
    assert!(key.len() <= 200);
    assert_ne!(
        key,
        send_key(campaign, 5, "2026-11-02T09:00", "Europe/London")
    );
}

#[test]
fn rates_and_outlines_read_at_a_glance() {
    assert_eq!(percent(0.5556), "55.6%");
    assert_eq!(percent(0.0003), "0.03%");
    assert_eq!(percent(0.0), "0.0%");
    let lines = outline(&campaign("draft")["content"]);
    assert_eq!(
        lines,
        vec![
            "1. Heading: Winter is here",
            "2. Text: Hi {{first_name|there}}, everything is twenty percent off until Sunday.",
            "3. Button: Shop the sale → https://northwind.example/sale",
            "4. Image: Beans in the snow",
            "5. Divider",
        ]
    );
}
