//! Tests for the studio's email actions against a stub of Create's email routes: the offer,
//! drafting, the person's go before a send (elicitation, confirmed recipient count), the
//! test-send recipients, reports and refusals.

use super::email::{
    address_of, contacts_of, email_refusal, holds_resend_key, is_resend_test_address,
    looks_like_resend_key, operation_of, outline, percent, send_at_of, send_key, send_state,
    suppression_words, test_recipients, time_zone_of, variant_of, when_words, Topic, CONNECT_URL,
    EMAIL_ACTIONS, KEY_REFUSED, NOT_OFFERED, SIGN_IN,
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
const COPY: &str = "66666666-6666-4666-8666-666666666666";
const CONTACT: &str = "55555555-5555-4555-8555-555555555555";
const LINK: &str = "44444444-4444-4444-8444-444444444444";

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
                "subject": if request.query.contains("variant=1") { "Twenty percent off, this week only" } else { "Our winter sale starts today" },
                "subjects": ["Our winter sale starts today", "Twenty percent off, this week only"],
                "variant": if request.query.contains("variant=1") { 1 } else { 0 },
                "preheader": "Twenty percent off everything until Sunday.",
                "from": "Northwind <hello@news.northwind.example>",
                "fromName": "Northwind", "fromEmail": "hello@news.northwind.example",
                "html": "<html></html>", "text": "",
                "warnings": ["The image “Beans in the snow” has no link."],
                "recipients": recipients
            })),
            ("GET", p) if p == format!("{campaign_path}/preview-links") => Answer::ok(json!({
                "links": [link(LINK, 72)]
            })),
            ("POST", p) if p == format!("{campaign_path}/preview-links") => {
                Answer::status(201, link("33333333-3333-4333-8333-333333333333", 168))
            }
            ("POST", p) if p == format!("{campaign_path}/revise") => {
                let mut revised = campaign("draft");
                revised["revision"] = json!(5);
                revised["subject"] = json!("Winter sale: 20% off");
                Answer::ok(json!({
                    "campaign": revised,
                    "outline": [{"type": "heading", "text": "Winter is here"}],
                    "changes": ["Shortened the subject to 20 characters."]
                }))
            }
            ("PATCH", p) if p == campaign_path => {
                let mut changed = campaign("draft");
                changed["revision"] = json!(5);
                if let Some(subject) = request.body.get("subject") {
                    changed["subject"] = subject.clone();
                }
                if let Some(preheader) = request.body.get("preheader") {
                    changed["preheader"] = preheader.clone();
                }
                Answer::ok(changed)
            }
            ("POST", p) if p == format!("{campaign_path}/duplicate") => {
                let mut copy = campaign("draft");
                copy["id"] = json!(COPY);
                copy["name"] = json!("Copy of Winter sale");
                Answer::status(201, copy)
            }
            ("GET", "/create/email/contacts") => Answer::ok(json!({
                "contacts": [
                    {"id": CONTACT, "email": "ada@northwind.example", "firstName": "Ada", "lastName": "Lovelace",
                     "tags": ["vip"], "attributes": {}, "consent": {"source": "manual", "note": "Met at the Spring market", "at": "2026-10-01T10:00:00Z"},
                     "audienceIds": [AUDIENCE], "suppressed": null},
                    {"id": "40404040-4040-4040-8040-404040404040", "email": "grace@northwind.example", "firstName": null, "lastName": null,
                     "tags": [], "attributes": {}, "consent": {"source": "import", "note": null, "at": "2026-10-01T10:00:00Z"},
                     "audienceIds": [AUDIENCE], "suppressed": {"reason": "unsubscribed", "at": "2026-10-20T08:00:00Z"}}
                ],
                "nextCursor": null, "total": 2
            })),
            ("POST", "/create/email/contacts") => {
                let n = request.body["contacts"].as_array().map_or(0, Vec::len);
                Answer::ok(
                    json!({"created": n, "updated": 0, "added": n, "invalid": [], "suppressed": 0}),
                )
            }
            ("DELETE", p) if p == format!("/create/email/contacts/{CONTACT}") => {
                Answer::ok(json!({"deleted": true}))
            }
            ("GET", "/create/email/suppressions") => Answer::ok(json!({
                "suppressions": [
                    {"email": "grace@northwind.example", "reason": "unsubscribed", "source": "one_click",
                     "campaignId": CAMPAIGN, "campaign": {"id": CAMPAIGN, "name": "Winter sale"},
                     "feedback": {"reason": "too_many", "comment": "Weekly is plenty", "at": "2026-10-20T08:01:00Z"},
                     "detail": null, "createdAt": "2026-10-20T08:00:00Z"},
                    {"email": "bounced@resend.dev", "reason": "bounced", "source": "webhook",
                     "campaignId": CAMPAIGN, "campaign": {"id": CAMPAIGN, "name": "Winter sale"},
                     "feedback": null, "detail": "550 5.1.1 Unknown user", "createdAt": "2026-10-19T08:00:00Z"}
                ],
                "nextCursor": null, "total": 2
            })),
            ("GET", "/create/email/suppressions/summary") => Answer::ok(json!({
                "days": 30, "total": 2,
                "byReason": {"unsubscribed": 1, "bounced": 1, "complained": 0, "manual": 0},
                "bySource": {"one_click": 1, "page": 0, "webhook": 1, "manual": 0, "api": 0},
                "feedback": {"too_many": 1, "not_relevant": 0, "never_signed_up": 0, "other": 0, "none": 1},
                "series": [], "campaigns": [], "resubscribed": 1
            })),
            ("POST", "/create/email/suppressions") => {
                let n = request.body["emails"].as_array().map_or(0, Vec::len);
                Answer::ok(json!({"added": n, "already": 0}))
            }
            ("POST", "/create/email/suppressions/remove") => Answer::ok(json!({"removed": true})),
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

/// A preview link that is live for `hours` more.
fn link(id: &str, hours: i64) -> Value {
    let expires = chrono::Utc::now() + chrono::Duration::hours(hours);
    json!({
        "id": id, "url": format!("https://ctxproduct.com/email-preview#t=token-{id}"),
        "createdAt": "2026-10-30T01:00:00Z", "expiresAt": expires.to_rfc3339(),
        "revokedAt": null, "live": true, "opens": 0, "lastOpenedAt": null, "createdBy": "u"
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
        "variant",
        "subject",
        "preheader",
        "operation",
        "email",
        "contacts",
        "consent_source",
        "consent_note",
        "query",
        "reason",
        "note",
        "confirm_email",
    ] {
        assert!(properties.contains_key(field), "{field}");
    }
    assert!(!properties.keys().any(|k| k.contains("model")));
    // No parameter takes a Resend key, and the reference says where it goes instead.
    assert!(!properties.keys().any(|k| k.contains("key")));
    assert!(TOOL_REFERENCE.contains(CONNECT_URL));
    assert!(TOOL_REFERENCE.contains("never ask for or pass an API key"));
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
    assert_eq!(state, format!("studio-email-send:{CAMPAIGN}:4:37:now:UTC"));
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

// ============================================================================
// Round 2: #332's follow-ups
// ============================================================================

#[tokio::test]
async fn a_go_given_for_an_older_revision_isnt_taken() {
    let revision = Arc::new(Mutex::new(4i64));
    let current = revision.clone();
    let stub = Stub::start(Arc::new(move |request: &Recorded| {
        let answer = (usual())(request);
        if request.method == "GET" && request.path == format!("/create/email/campaigns/{CAMPAIGN}")
        {
            let mut campaign = campaign("draft");
            campaign["revision"] = json!(*current.lock().unwrap());
            return Answer::ok(campaign);
        }
        answer
    }))
    .await;
    let tool = stub.tool();
    let client = Arc::new(FakeClient::default());
    let first = CallPeer::new(client.clone(), None, false).with_inline_input(&json!({}));
    run_with_call_peer(
        first.clone(),
        tool.execute(json!({"action": "email_send", "campaign_id": CAMPAIGN})),
    )
    .await
    .unwrap();
    let state = first.take_input_required().unwrap()["requestState"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        state,
        send_state(Uuid::parse_str(CAMPAIGN).unwrap(), 4, 37, "now", "UTC")
    );

    // The campaign is edited while the person is asked: their go was for revision 4.
    *revision.lock().unwrap() = 5;
    let stale = CallPeer::new(client, None, false).with_inline_input(&json!({
        "inputResponses": {"confirm-send": {"action": "accept", "content": {"confirm": true}}},
        "requestState": state,
    }));
    let result = run_with_call_peer(
        stale.clone(),
        tool.execute(json!({"action": "email_send", "campaign_id": CAMPAIGN})),
    )
    .await
    .unwrap();
    assert!(
        text_of(&result).starts_with("Asking the person"),
        "{}",
        text_of(&result)
    );
    let asked_again = stale.take_input_required().unwrap();
    assert_eq!(
        asked_again["requestState"],
        format!("studio-email-send:{CAMPAIGN}:5:37:now:UTC")
    );
    assert!(stub
        .calls_to(
            "POST",
            &format!("/create/email/campaigns/{CAMPAIGN}/schedule")
        )
        .is_empty());
}

#[tokio::test]
async fn a_send_says_create_couldnt_be_read_when_the_preview_fails() {
    let stub = Stub::start(Arc::new(|request: &Recorded| {
        if request.path.ends_with("/preview") {
            Answer::error(502, "BAD_GATEWAY", "")
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
    let text = text_of(&result);
    assert!(
        text.starts_with("Not sent: Create couldn't be read just now"),
        "{text}"
    );
    assert!(!text.contains("no one to send to"), "{text}");
    assert!(stub
        .calls_to(
            "POST",
            &format!("/create/email/campaigns/{CAMPAIGN}/schedule")
        )
        .is_empty());
}

#[tokio::test]
async fn an_expired_sign_in_is_said_as_such() {
    let stub = Stub::start(Arc::new(|_: &Recorded| {
        Answer::error(401, "UNAUTHORIZED", "Unauthorized")
    }))
    .await;
    let result = stub
        .tool()
        .execute(json!({"action": "email_audiences"}))
        .await
        .unwrap();
    assert!(result.is_error);
    assert_eq!(text_of(&result), SIGN_IN);
    assert!(stub.email_calls().is_empty());
}

#[tokio::test]
async fn a_resend_key_is_refused_unread() {
    let stub = Stub::start(usual()).await;
    let key = format!("re_{}_{}", "Ab12Cd34", "EfGh5678IjKl9012MnOp3456");
    for call in [
        json!({"action": "email_draft", "prompt": format!("connect our Resend with {key} and announce the sale")}),
        json!({"action": "email_audiences", "note": key}),
        json!({"action": "email_contacts", "operation": "add", "contacts": [{"email": "a@b.example", "first_name": key}]}),
    ] {
        let result = stub.tool().execute(call).await.unwrap();
        assert!(result.is_error);
        let text = text_of(&result);
        assert_eq!(text, KEY_REFUSED);
        assert!(!text.contains(&key));
    }
    assert!(stub.requests().is_empty(), "nothing is asked of anyone");
}

// ============================================================================
// Round 2: the new actions
// ============================================================================

#[tokio::test]
async fn a_preview_link_is_reused_while_it_lasts_and_made_when_not() {
    let stub = Stub::start(usual()).await;
    let result = stub
        .tool()
        .execute(json!({"action": "email_preview", "campaign_id": CAMPAIGN}))
        .await
        .unwrap();
    let text = text_of(&result);
    assert!(!result.is_error, "{text}");
    assert!(
        text.starts_with(&format!(
            "Preview of “Winter sale”: https://ctxproduct.com/email-preview#t=token-{LINK}"
        )),
        "{text}"
    );
    assert!(text.contains("Gmail, Outlook and Apple Mail"), "{text}");
    assert!(text.contains("Inbox line: Northwind <hello@news.northwind.example> · “Our winter sale starts today” · Twenty percent off everything until Sunday."), "{text}");
    assert!(
        text.contains("A/B subjects: A “Our winter sale starts today”, B “Twenty percent off, this week only”"),
        "{text}"
    );
    assert!(text.contains("To: Coffee club (37 people)."), "{text}");
    assert!(
        text.contains("Check: The image “Beans in the snow” has no link."),
        "{text}"
    );
    assert!(stub
        .calls_to(
            "POST",
            &format!("/create/email/campaigns/{CAMPAIGN}/preview-links")
        )
        .is_empty());
    assert_eq!(structured(&result)["link_id"], LINK);

    // A link with only hours left isn't handed out: a new one is made.
    let short = Stub::start(Arc::new(|request: &Recorded| {
        if request.method == "GET" && request.path.ends_with("/preview-links") {
            Answer::ok(json!({"links": [link(LINK, 2)]}))
        } else {
            (usual())(request)
        }
    }))
    .await;
    let made = short
        .tool()
        .execute(json!({"action": "email_preview", "campaign_id": CAMPAIGN, "variant": 1}))
        .await
        .unwrap();
    let text = text_of(&made);
    assert!(
        text.contains("token-33333333-3333-4333-8333-333333333333"),
        "{text}"
    );
    assert!(
        text.contains("“Twenty percent off, this week only”"),
        "{text}"
    );
    assert!(text.contains("(this is B)"), "{text}");
    let posts = short.calls_to(
        "POST",
        &format!("/create/email/campaigns/{CAMPAIGN}/preview-links"),
    );
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].body["workspaceId"], WORKSPACE);
    let previews = short.calls_to(
        "GET",
        &format!("/create/email/campaigns/{CAMPAIGN}/preview"),
    );
    assert!(
        previews[0].query.contains("variant=1"),
        "{}",
        previews[0].query
    );

    let bad = short
        .tool()
        .execute(json!({"action": "email_preview", "campaign_id": CAMPAIGN, "variant": 3}))
        .await;
    assert!(matches!(bad, Err(Error::Validation(_))));
}

#[tokio::test]
async fn a_draft_is_revised_from_plain_words_or_given_lines() {
    let stub = Stub::start(usual()).await;
    let tool = stub.tool();
    let result = tool
        .execute(
            json!({"action": "email_revise", "campaign_id": CAMPAIGN, "change": "shorter subject"}),
        )
        .await
        .unwrap();
    let text = text_of(&result);
    assert!(!result.is_error, "{text}");
    assert!(
        text.starts_with("Changed “Winter sale” (revision 5). Nothing is sent."),
        "{text}"
    );
    assert!(
        text.contains("- Shortened the subject to 20 characters."),
        "{text}"
    );
    assert!(text.contains("Subject: Winter sale: 20% off"), "{text}");
    assert!(text.contains("1. Heading: Winter is here"), "{text}");
    let revises = stub.calls_to(
        "POST",
        &format!("/create/email/campaigns/{CAMPAIGN}/revise"),
    );
    assert_eq!(revises.len(), 1);
    assert_eq!(revises[0].body["instruction"], "shorter subject");
    assert_eq!(revises[0].body["revision"], 4);
    assert!(revises[0]
        .header("idempotency-key")
        .is_some_and(|k| k.starts_with(&format!("email-revise-{CAMPAIGN}-r4-"))));
    assert_eq!(structured(&result)["revision"], 5);

    let set = tool
        .execute(json!({"action": "email_revise", "campaign_id": CAMPAIGN, "subject": "Warm up with 20% off", "preheader": ""}))
        .await
        .unwrap();
    assert!(!set.is_error, "{}", text_of(&set));
    let patches = stub.calls_to("PATCH", &format!("/create/email/campaigns/{CAMPAIGN}"));
    assert_eq!(patches.len(), 1);
    assert_eq!(patches[0].body["subject"], "Warm up with 20% off");
    assert_eq!(patches[0].body["preheader"], "");
    assert_eq!(patches[0].body["revision"], 4);
    assert!(text_of(&set).contains("Subject: Warm up with 20% off"));

    for wrong in [
        json!({"action": "email_revise", "campaign_id": CAMPAIGN}),
        json!({"action": "email_revise", "campaign_id": CAMPAIGN, "change": "warmer", "subject": "x"}),
        json!({"action": "email_revise", "campaign_id": CAMPAIGN, "subject": " "}),
        json!({"action": "email_revise", "campaign_id": CAMPAIGN, "change": "x".repeat(1001)}),
    ] {
        assert!(matches!(
            tool.execute(wrong).await,
            Err(Error::Validation(_))
        ));
    }

    // Only a draft changes.
    let scheduled = Stub::start(api(Arc::new(Mutex::new("scheduled".to_string())), 37)).await;
    let refused = scheduled
        .tool()
        .execute(json!({"action": "email_revise", "campaign_id": CAMPAIGN, "change": "warmer"}))
        .await
        .unwrap();
    assert!(refused.is_error);
    assert!(
        text_of(&refused).contains("only a draft can be changed, and “Winter sale” is scheduled. email_cancel unschedules it"),
        "{}",
        text_of(&refused)
    );
    assert!(scheduled
        .calls_to(
            "POST",
            &format!("/create/email/campaigns/{CAMPAIGN}/revise")
        )
        .is_empty());
}

#[tokio::test]
async fn a_campaign_is_duplicated_as_a_draft() {
    let stub = Stub::start(usual()).await;
    let result = stub
        .tool()
        .execute(json!({"action": "email_duplicate", "campaign_id": CAMPAIGN}))
        .await
        .unwrap();
    let text = text_of(&result);
    assert!(!result.is_error, "{text}");
    assert!(
        text.starts_with(&format!(
            "Made “Copy of Winter sale”, a draft copy (campaign_id {COPY}). Nothing is sent."
        )),
        "{text}"
    );
    assert_eq!(structured(&result)["campaign_id"], COPY);
    assert_eq!(
        stub.calls_to(
            "POST",
            &format!("/create/email/campaigns/{CAMPAIGN}/duplicate")
        )
        .len(),
        1
    );
}

#[tokio::test]
async fn contacts_are_listed_added_erased_and_unsubscribed() {
    let stub = Stub::start(usual()).await;
    let tool = stub.tool();

    let list = tool
        .execute(json!({"action": "email_contacts", "query": "northwind", "status": "subscribed"}))
        .await
        .unwrap();
    let text = text_of(&list);
    assert!(!list.is_error, "{text}");
    assert!(
        text.starts_with("2 contacts matching “northwind”:"),
        "{text}"
    );
    assert!(text.contains("- ada@northwind.example (Ada Lovelace) · subscribed · in Coffee club · tags vip · consent manual: Met at the Spring market"), "{text}");
    assert!(
        text.contains("- grace@northwind.example · not sent to: unsubscribed 2026-10-20 08:00 UTC"),
        "{text}"
    );
    let gets = stub.calls_to("GET", "/create/email/contacts");
    assert!(gets[0].query.contains("q=northwind"), "{}", gets[0].query);
    assert!(
        gets[0].query.contains("status=subscribed"),
        "{}",
        gets[0].query
    );

    // Adding needs where they agreed to hear from the team.
    let no_consent = tool
        .execute(json!({"action": "email_contacts", "operation": "add", "contacts": ["new@northwind.example"]}))
        .await;
    assert!(matches!(no_consent, Err(Error::Validation(m)) if m.contains("consent_source")));
    let no_note = tool
        .execute(json!({"action": "email_contacts", "operation": "add", "contacts": ["new@northwind.example"], "consent_source": "form"}))
        .await;
    assert!(matches!(no_note, Err(Error::Validation(m)) if m.contains("consent_note")));
    let added = tool
        .execute(json!({
            "action": "email_contacts", "operation": "add", "audience_id": AUDIENCE,
            "consent_source": "form", "consent_note": "Signed up at the Spring market",
            "contacts": ["New@Northwind.example", {"email": "lin@northwind.example", "first_name": "Lin", "tags": ["vip"]}, "new@northwind.example", "not-an-address"],
        }))
        .await
        .unwrap();
    let text = text_of(&added);
    assert!(!added.is_error, "{text}");
    assert!(text.starts_with("Added 2 people (2 new, 0 updated), 2 put in the audience, consent form: “Signed up at the Spring market”."), "{text}");
    assert!(
        text.contains("Not added: not-an-address (isn't an email address)"),
        "{text}"
    );
    let posts = stub.calls_to("POST", "/create/email/contacts");
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].body["consentSource"], "form");
    assert_eq!(posts[0].body["audienceId"], AUDIENCE);
    assert_eq!(
        posts[0].body["contacts"][0]["email"],
        "new@northwind.example"
    );
    assert_eq!(
        posts[0].body["contacts"][0]["consentNote"],
        "Signed up at the Spring market"
    );
    assert_eq!(posts[0].body["contacts"][1]["firstName"], "Lin");
    assert!(posts[0]
        .header("idempotency-key")
        .is_some_and(|k| k.starts_with("email-contacts-")));

    let erased = tool
        .execute(json!({"action": "email_contacts", "operation": "remove", "email": "ADA@northwind.example"}))
        .await
        .unwrap();
    assert!(!erased.is_error, "{}", text_of(&erased));
    assert!(text_of(&erased).starts_with("Erased ada@northwind.example"));
    assert_eq!(
        stub.calls_to("DELETE", &format!("/create/email/contacts/{CONTACT}"))
            .len(),
        1
    );
    let unknown = tool
        .execute(json!({"action": "email_contacts", "operation": "remove", "email": "nobody@northwind.example"}))
        .await
        .unwrap();
    assert!(unknown.is_error);
    assert!(text_of(&unknown).contains("there is no contact nobody@northwind.example"));

    let unsubscribed = tool
        .execute(json!({"action": "email_contacts", "operation": "unsubscribe", "email": "lin@northwind.example", "note": "Asked on the phone"}))
        .await
        .unwrap();
    assert_eq!(
        text_of(&unsubscribed),
        "lin@northwind.example is on the suppression list now: no campaign will reach them."
    );
    let suppressions = stub.calls_to("POST", "/create/email/suppressions");
    assert_eq!(
        suppressions[0].body["emails"],
        json!(["lin@northwind.example"])
    );
    assert_eq!(
        suppressions[0].body["note"],
        "Added through the studio tool: Asked on the phone"
    );

    let bad = tool
        .execute(json!({"action": "email_contacts", "operation": "export"}))
        .await;
    assert!(matches!(bad, Err(Error::Validation(_))));
}

#[tokio::test]
async fn unsubscribes_are_listed_with_why_and_a_remove_needs_the_persons_go() {
    let stub = Stub::start(usual()).await;
    let tool = stub.tool();
    let list = tool
        .execute(json!({"action": "email_unsubscribes"}))
        .await
        .unwrap();
    let text = text_of(&list);
    assert!(!list.is_error, "{text}");
    assert!(
        text.starts_with("The suppression list: 2 addresses, never sent to."),
        "{text}"
    );
    assert!(text.contains("Last 30 days: 1 unsubscribed, 1 bounced, 0 spam complaints, 0 added by hand; 1 resubscribed."), "{text}");
    assert!(text.contains("Why they left: too many emails 1."), "{text}");
    assert!(text.contains("- grace@northwind.example: unsubscribed with their mail client's one-click button on 2026-10-20 08:00 UTC, from “Winter sale”; said: too many emails (“Weekly is plenty”)"), "{text}");
    assert!(text.contains("- bounced@resend.dev: hard-bounced on 2026-10-19 08:00 UTC, from “Winter sale”: 550 5.1.1 Unknown user"), "{text}");

    let remove = "/create/email/suppressions/remove".to_string();
    // No go: nothing comes off, and the agent is told what to show.
    let unasked = tool
        .execute(json!({"action": "email_unsubscribes", "operation": "remove", "email": "grace@northwind.example"}))
        .await
        .unwrap();
    let text = text_of(&unasked);
    assert!(
        text.starts_with("Not removed yet: the person's go is needed first."),
        "{text}"
    );
    assert!(
        text.contains("Only if they asked to hear from the team again"),
        "{text}"
    );
    assert!(
        text.contains("confirm_email grace@northwind.example"),
        "{text}"
    );
    let wrong = tool
        .execute(json!({"action": "email_unsubscribes", "operation": "remove", "email": "grace@northwind.example", "confirm_email": "ada@northwind.example"}))
        .await
        .unwrap();
    assert!(text_of(&wrong).starts_with("Not removed: confirm_email is ada@northwind.example"));
    assert!(stub.calls_to("POST", &remove).is_empty());

    // Through the client: a no keeps them on the list, a yes takes them off.
    let client = Arc::new(FakeClient::default());
    *client.answer.lock().unwrap() = Some(json!({"action": "decline"}));
    let declined = run_with_call_peer(
        CallPeer::new(client.clone(), None, true),
        tool.execute(json!({"action": "email_unsubscribes", "operation": "remove", "email": "grace@northwind.example", "confirm_email": "grace@northwind.example"})),
    )
    .await
    .unwrap();
    assert!(text_of(&declined).starts_with("Not removed: the person didn't say go."));
    assert!(stub.calls_to("POST", &remove).is_empty());
    let message = client.sent.lock().unwrap()[0].1["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        message.starts_with("Take grace@northwind.example (unsubscribed"),
        "{message}"
    );

    *client.answer.lock().unwrap() =
        Some(json!({"action": "accept", "content": {"confirm": true}}));
    let removed = run_with_call_peer(
        CallPeer::new(client, None, true),
        tool.execute(json!({"action": "email_unsubscribes", "operation": "remove", "email": "grace@northwind.example", "note": "She wrote to ask"})),
    )
    .await
    .unwrap();
    assert!(!removed.is_error, "{}", text_of(&removed));
    let posts = stub.calls_to("POST", &remove);
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].body["email"], "grace@northwind.example");
    assert_eq!(posts[0].body["confirm"], true);
    assert_eq!(
        posts[0].body["note"],
        "Taken off through the studio tool, with the person's go: She wrote to ask"
    );

    // Without a client to ask, the confirmed address is the go; a bounce needs no confirm flag.
    let bounced = tool
        .execute(json!({"action": "email_unsubscribes", "operation": "remove", "email": "bounced@resend.dev", "confirm_email": "Bounced@resend.dev"}))
        .await
        .unwrap();
    assert!(!bounced.is_error, "{}", text_of(&bounced));
    let posts = stub.calls_to("POST", &remove);
    assert_eq!(posts.len(), 2);
    assert!(posts[1].body.get("confirm").is_none());

    let absent = tool
        .execute(json!({"action": "email_unsubscribes", "operation": "remove", "email": "ada@northwind.example", "confirm_email": "ada@northwind.example"}))
        .await
        .unwrap();
    assert!(text_of(&absent).contains("isn't on the suppression list"));

    let added = tool
        .execute(json!({"action": "email_unsubscribes", "operation": "add", "contacts": "a@northwind.example, b@northwind.example"}))
        .await
        .unwrap();
    assert_eq!(
        text_of(&added),
        "Added 2 to the suppression list: no campaign will reach them."
    );
    let bad_reason = tool
        .execute(json!({"action": "email_unsubscribes", "reason": "bored"}))
        .await;
    assert!(matches!(bad_reason, Err(Error::Validation(_))));
}

#[tokio::test]
async fn the_connection_is_made_in_the_studio_never_here() {
    let stub = Stub::start(Arc::new(|request: &Recorded| {
        if request.path == "/create/email/connection" {
            Answer::ok(json!({"connected": false}))
        } else {
            (usual())(request)
        }
    }))
    .await;
    let result = stub
        .tool()
        .execute(json!({"action": "email_audiences"}))
        .await
        .unwrap();
    let text = text_of(&result);
    assert!(text.contains(&format!("Resend isn't connected yet: the team connects its own Resend key in the studio ({CONNECT_URL}). An agent never takes the key")), "{text}");
    assert_eq!(
        structured(&result)["connection"]["connect_url"],
        CONNECT_URL
    );
}

// ============================================================================
// Round 2: words and parsing
// ============================================================================

#[test]
fn resend_keys_are_recognised_without_false_alarms() {
    assert!(looks_like_resend_key(
        "re_Ab12Cd34_EfGh5678IjKl9012MnOp3456"
    ));
    assert!(looks_like_resend_key(
        "the key is re_Ab12Cd34EfGh5678IjKl9012 ok"
    ));
    for fine in [
        "re_short",
        "pre_existing_condition_values_that_run_long",
        "store_everything_in_one_place_for_now",
        "Re: our winter sale",
        "",
    ] {
        assert!(!looks_like_resend_key(fine), "{fine}");
    }
    assert!(holds_resend_key(
        &json!({"a": [{"b": "re_Ab12Cd34_EfGh5678IjKl9012MnOp3456"}]})
    ));
    assert!(!holds_resend_key(&json!({"a": [1, null, "fine"]})));
}

#[test]
fn addresses_contacts_and_operations_read_plainly() {
    assert_eq!(
        address_of(" Ada@Example.COM ").as_deref(),
        Some("ada@example.com")
    );
    for no in [
        "ada",
        "ada@",
        "@example.com",
        "ada@example",
        "ada@.com",
        "a da@example.com",
    ] {
        assert!(address_of(no).is_none(), "{no}");
    }
    let input = StudioInput {
        contacts: Some(
            json!(["a@x.example", {"email": "B@x.example", "firstName": "Bea"}, "a@x.example", "nope"]),
        ),
        ..StudioInput::default()
    };
    let (contacts, invalid) = contacts_of(&input).unwrap();
    assert_eq!(contacts.len(), 2);
    assert_eq!(contacts[1].email, "b@x.example");
    assert_eq!(contacts[1].first_name.as_deref(), Some("Bea"));
    assert_eq!(invalid, vec!["nope"]);
    let many = StudioInput {
        contacts: Some(json!((0..101)
            .map(|i| format!("p{i}@x.example"))
            .collect::<Vec<_>>())),
        ..StudioInput::default()
    };
    assert!(contacts_of(&many).is_err());
    assert_eq!(operation_of(None, &["list", "add"], "x").unwrap(), "list");
    assert_eq!(
        operation_of(Some(" ADD "), &["list", "add"], "x").unwrap(),
        "add"
    );
    assert!(operation_of(Some("drop"), &["list", "add"], "x").is_err());
    assert_eq!(variant_of(Some(2)).unwrap(), Some(2));
    assert!(variant_of(Some(-1)).is_err());
    assert_eq!(
        suppression_words(
            &json!({"email": "x@y.example", "reason": "manual", "source": "api", "createdAt": "2026-10-20T08:00:00Z"})
        ),
        "added through the API on 2026-10-20 08:00 UTC"
    );
}
