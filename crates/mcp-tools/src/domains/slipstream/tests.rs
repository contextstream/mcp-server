//! Tests for the Slipstream tool: its catalog, its gated listing, its description, the
//! quote-then-accept flow, the auto-accept cap, declines, idempotency and long runs.

use super::catalog::{prepare_input, Catalog};
use super::describe::{describe, shorten, DESCRIPTION_BUDGET, INPUT_REFERENCE_BUDGET};
use super::gate::{
    Availability, GateKey, SlipstreamGate, AVAILABLE_TTL, MAX_ENTRIES, REFUSED_TTL,
    UNREACHABLE_RETRY,
};
use super::*;
use crate::testing::TestFixtures;
use mcp_types::Config;
use std::sync::Mutex;
use std::time::Instant;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const WORKSPACE: &str = "11111111-1111-4111-8111-111111111111";
const PROJECT: &str = "22222222-2222-4222-8222-222222222222";
const TASK: &str = "33333333-3333-4333-8333-333333333333";

/// The catalog an eight-agent workspace answers (every agent turned on).
fn live_catalog() -> Value {
    serde_json::from_str(include_str!("testdata/catalog.json")).expect("catalog fixture")
}

fn workspace() -> Uuid {
    Uuid::parse_str(WORKSPACE).unwrap()
}

fn project() -> Uuid {
    Uuid::parse_str(PROJECT).unwrap()
}

// ============================================================================
// A stub ContextStream API
// ============================================================================

#[derive(Debug, Clone)]
struct Recorded {
    method: String,
    /// Path below `/api/v1`, without the query.
    path: String,
    query: String,
    body: Value,
}

#[derive(Debug, Clone)]
struct Answer {
    status: u16,
    body: Value,
}

impl Answer {
    fn ok(data: Value) -> Self {
        Self {
            status: 200,
            body: json!({"success": true, "data": data, "error": null, "metadata": {}}),
        }
    }

    fn error(status: u16, message: &str) -> Self {
        Self {
            status,
            body: json!({"success": false, "data": null,
                         "error": {"code": "ERROR", "message": message}}),
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
                    let response = format!(
                        "HTTP/1.1 {} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        answer.status,
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Self { url, requests }
    }

    /// The usual API: the catalog, one project of the workspace, and tasks as `task`
    /// answers them.
    async fn api(
        catalog: Value,
        task: impl Fn(&Recorded) -> Answer + Send + Sync + 'static,
    ) -> Self {
        Self::start(Arc::new(move |request: &Recorded| {
            if request.path == "/tasks/agents" {
                Answer::ok(catalog.clone())
            } else if request.path.starts_with("/projects/") {
                Answer::ok(json!({"id": PROJECT, "name": "api", "workspace_id": WORKSPACE}))
            } else {
                task(request)
            }
        }))
        .await
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }

    fn task_posts(&self) -> Vec<Recorded> {
        self.requests()
            .into_iter()
            .filter(|r| r.method == "POST" && r.path.ends_with("/tasks"))
            .collect()
    }

    fn config(&self, api_key: &str) -> Config {
        let mut config = TestFixtures::test_config();
        config.api_url = self.url.clone();
        config.api_key = Some(api_key.to_string());
        config.default_workspace_id = Some(workspace());
        config.default_project_id = Some(project());
        config
    }

    fn tool(&self, config: Config) -> (SlipstreamTool, Arc<SlipstreamGate>) {
        let client = ContextStreamClient::new(config.clone());
        let session = Arc::new(SessionManager::new(client.clone(), config));
        let gate = Arc::new(SlipstreamGate::new());
        (SlipstreamTool::new(client, session, gate.clone()), gate)
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
    let length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })
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
        body: serde_json::from_slice(&buffer[header_end..]).unwrap_or(Value::Null),
    })
}

fn task_view(status: &str, done: bool) -> Value {
    json!({
        "id": TASK, "workspace_id": WORKSPACE, "project_id": PROJECT,
        "agent": "review@0.1.0", "status": status, "done": done,
        "stage": if done { Value::Null } else { json!("reading_diff") },
        "price_credits": 1200, "charged_credits": if status == "succeeded" { json!(1200) } else { Value::Null },
        "created_at": chrono::Utc::now().to_rfc3339(),
        "output": if done {
            json!({"status": status, "summary": "Two findings in the retry loop.",
                   "data": {"findings": [{"line": 12, "constraint": "decision: retries are capped"}],
                            "report": "## Findings\n1. The retry loop has no cap."}})
        } else { Value::Null },
    })
}

fn text_of(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|item| match item {
            mcp_types::tool::ContentItem::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ============================================================================
// The tool
// ============================================================================

#[test]
fn the_tool_is_named_slipstream_and_lists_its_actions() {
    let config = TestFixtures::test_config();
    let client = ContextStreamClient::new(config.clone());
    let session = Arc::new(SessionManager::new(client.clone(), config));
    let tool = SlipstreamTool::new(client, session, Arc::new(SlipstreamGate::new()));
    let metadata = tool.metadata();
    assert_eq!(metadata.name, "slipstream");
    assert_eq!(TOOL_NAME, "slipstream");
    for action in VALID_ACTIONS {
        assert!(metadata.description.contains(action), "{action}");
    }
    assert!(metadata.description.chars().count() <= 1024);
    let schema = tool.input_schema();
    let props = schema["properties"].as_object().unwrap();
    for field in [
        "action",
        "workspace_id",
        "project_id",
        "agent",
        "input",
        "accepted_price_credits",
        "wait_ms",
        "task_id",
        "limit",
        "idempotency_key",
        "session_id",
    ] {
        assert!(props.contains_key(field), "{field}");
    }
    assert_eq!(schema["required"], json!(["action"]));
}

#[test]
fn the_catalog_parses_every_agent_with_its_price_input_and_job() {
    let catalog = Catalog::from_value(&live_catalog());
    let ids: Vec<&str> = catalog.enabled().iter().map(|a| a.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "docs-drift",
            "gardener",
            "guide",
            "lens",
            "mail",
            "pdf",
            "release-notes",
            "review"
        ]
    );
    let review = catalog.find("review@0.1.0").unwrap();
    assert_eq!(review.price_credits, Some(1200));
    assert_eq!(review.quoted_price(), Some(1200));
    assert_eq!(review.agent_ref(), "review@0.1.0");
    assert!(review.job().starts_with("Reviews a pull request's diff"));
    assert!(!review.job().ends_with('.'));
    let fields: Vec<String> = review
        .input_fields()
        .iter()
        .map(|f| format!("{}:{}:{}", f.name, f.kind, f.required))
        .collect();
    assert_eq!(
        fields,
        [
            "pull:integer:true",
            "repo:string:true",
            "focus:string:false",
            "project_id:string:false",
            "save_note:boolean:false"
        ]
    );
    assert!(catalog.find("guide").unwrap().requires_project_id());
    assert!(!review.requires_project_id());
    assert!(catalog.find("ci-fix").is_none());
    // The envelope is optional, and an entry without an id is skipped.
    let bare = Catalog::from_value(
        &json!({"agents": [{"agent": {"name": "x"}}, {"agent": {"id": "lens"}, "enabled": true}]}),
    );
    assert_eq!(bare.agents.len(), 1);
    assert_eq!(bare.agents[0].name, "lens");
}

#[test]
fn an_input_is_completed_and_checked_against_the_agents_schema() {
    let catalog = Catalog::from_value(&live_catalog());
    let guide = catalog.find("guide").unwrap();
    // A required project_id is the session's.
    let prepared = prepare_input(guide, Map::new(), Some(project())).unwrap();
    assert_eq!(prepared.input["project_id"], PROJECT);
    assert_eq!(prepared.notes.len(), 1);

    let review = catalog.find("review").unwrap();
    // Missing fields are named, with the agent's input and an example.
    let missing = prepare_input(review, Map::new(), Some(project())).unwrap_err();
    assert!(missing.contains("missing repo, pull") || missing.contains("missing pull, repo"));
    assert!(missing.contains("pull (integer, required)"));
    assert!(missing.contains("Example input:"));
    // Unknown fields are refused when the schema is closed.
    let unknown = prepare_input(
        review,
        json!({"repo": "acme/api", "pull": 1, "branch": "x"})
            .as_object()
            .unwrap()
            .clone(),
        None,
    )
    .unwrap_err();
    assert!(unknown.contains("review does not take branch"));
    // A string that spells a number is taken as one; a wrong type is refused.
    let coerced = prepare_input(
        review,
        json!({"repo": "acme/api", "pull": "42"})
            .as_object()
            .unwrap()
            .clone(),
        None,
    )
    .unwrap();
    assert_eq!(coerced.input["pull"], 42);
    assert!(coerced.notes.is_empty());
    let wrong = prepare_input(
        review,
        json!({"repo": "acme/api", "pull": "forty-two"})
            .as_object()
            .unwrap()
            .clone(),
        None,
    )
    .unwrap_err();
    assert!(wrong.contains("pull must be integer"));
    let release = catalog.find("release-notes").unwrap();
    let bad_enum = prepare_input(
        release,
        json!({"repo": "a/b", "base": "v1", "head": "v2", "audience": "everyone"})
            .as_object()
            .unwrap()
            .clone(),
        None,
    )
    .unwrap_err();
    assert!(bad_enum.contains("audience is one of \"users\", \"developers\""));
    // An agent synced before inputs takes anything.
    let legacy =
        Catalog::from_value(&json!({"agents": [{"agent": {"id": "old"}, "enabled": true}]}));
    let passed = prepare_input(
        legacy.find("old").unwrap(),
        json!({"anything": 1}).as_object().unwrap().clone(),
        Some(project()),
    )
    .unwrap();
    assert_eq!(passed.input, json!({"anything": 1}));
}

// ============================================================================
// The description
// ============================================================================

#[test]
fn the_description_names_every_enabled_agent_its_price_and_input_within_budget() {
    let catalog = Catalog::from_value(&live_catalog());
    let described = describe(&catalog, 0);
    let description = &described.description;
    eprintln!(
        "--- slipstream description ({} chars) ---\n{description}\n--- agent ---\n{}\n--- input ({} chars) ---\n{}\n---",
        description.chars().count(),
        described.agent_parameter,
        described.input_parameter.chars().count(),
        described.input_parameter
    );
    assert!(description.chars().count() <= DESCRIPTION_BUDGET);
    for (id, price) in [
        ("docs-drift", 800),
        ("gardener", 400),
        ("guide", 500),
        ("lens", 400),
        ("mail", 500),
        ("pdf", 600),
        ("release-notes", 900),
        ("review", 1200),
    ] {
        assert!(
            description.contains(&format!("- {id} ({price})")),
            "{id} in {description}"
        );
        assert!(described.agent_parameter.contains(id));
        assert!(described.input_parameter.contains(&format!("- {id}:")));
    }
    // Required input with its names; a required project_id is filled, so not asked for.
    assert!(description.contains("- review (1200) [pull, repo]"));
    assert!(description.contains("- lens (400) [media_id]"));
    assert!(description.contains("- release-notes (900) [base, head, repo]"));
    assert!(!description.contains("[project_id]"));
    // How to run, with the quote step.
    assert!(description.contains("slipstream(action=\"run\", agent, input)"));
    assert!(description.contains("accepted_price_credits"));
    assert!(description.contains("get_run"));
    // The input parameter carries types, required marks and an example.
    let input = &described.input_parameter;
    assert!(input.chars().count() <= INPUT_REFERENCE_BUDGET);
    assert!(input.contains("- review: pull* (integer), repo* (string)"));
    assert!(input.contains("project_id* (string, defaults to the session's)"));
    assert!(input.contains("e.g. {"));
    // An example's placeholder project_id is left out where the session's fills it.
    assert!(input.contains(
        "- docs-drift: project_id* (string, defaults to the session's), focus (string), paths (string[]), save_note (boolean). e.g. {}"
    ));
    assert!(input.contains(r#"e.g. {"pull":42,"repo":"acme/api"}"#));
}

#[test]
fn a_large_catalog_is_shortened_gracefully_and_never_over_budget() {
    let long_job = "Does a very particular and rather long-winded job that needs many words to explain what it does and why it matters to the person";
    for count in [1usize, 8, 15, 40, 120] {
        let agents: Vec<Value> = (0..count)
            .map(|i| {
                json!({"agent": {"id": format!("agent-{i:03}"), "version": "1.0.0", "name": format!("Agent {i}"),
                       "description": format!("{long_job}. Second sentence."), "price_credits": 100 + i,
                       "input_schema": {"type": "object", "required": ["repo", "pull"],
                                        "properties": {"repo": {"type": "string"}, "pull": {"type": "integer"},
                                                       "focus": {"type": "string"}}},
                       "examples": [{"input": {"repo": "a/b", "pull": 1}, "note": ""}]},
                       "enabled": true})
            })
            .collect();
        let catalog = Catalog::from_value(&json!({"agents": agents}));
        let described = describe(&catalog, 0);
        let chars = described.description.chars().count();
        assert!(chars <= DESCRIPTION_BUDGET, "{count} agents: {chars} chars");
        assert!(described.input_parameter.chars().count() <= INPUT_REFERENCE_BUDGET);
        assert!(described.description.contains("- agent-000 (100)"));
        if count == 1 {
            assert!(described.description.contains(long_job));
        }
        if count == 8 {
            // Every agent stays, with a shortened job.
            assert!(described.description.contains("- agent-007 (107)"));
            assert!(described.description.contains('…'));
        }
        if count >= 40 {
            assert!(
                described
                    .description
                    .contains("more: slipstream(action=\"find\")"),
                "{count}: {}",
                described.description
            );
        }
    }
    assert_eq!(shorten("short", 10), "short");
    assert_eq!(shorten("one two three four five", 12), "one two…");
}

#[test]
fn the_description_shows_the_auto_accept_cap_and_leaves_out_agents_turned_off() {
    let mut value = live_catalog();
    for entry in value["data"]["agents"].as_array_mut().unwrap() {
        if entry["agent"]["id"] == "mail" {
            entry["enabled"] = json!(false);
        }
    }
    let catalog = Catalog::from_value(&value);
    let described = describe(&catalog, 500);
    assert!(!described.description.contains("- mail"));
    assert!(!described.agent_parameter.contains("mail"));
    assert!(described
        .description
        .contains("A run priced at most 500 credits starts at once"));
    assert!(described.description.chars().count() <= DESCRIPTION_BUDGET);
}

// ============================================================================
// The gate
// ============================================================================

#[test]
fn the_catalog_answer_decides_whether_the_tool_is_listed() {
    let available = Availability::from_read(Ok(live_catalog()));
    assert!(available.is_listed());
    let none_on = Availability::from_read(Ok(json!({"agents": [
        {"agent": {"id": "lens", "price_credits": 400}, "enabled": false}]})));
    assert!(matches!(none_on, Availability::NoneEnabled(_)));
    assert!(!none_on.is_listed());
    for status in [401, 403, 404] {
        let refused = Availability::from_read(Err(Error::http(status, "not yet")));
        assert!(matches!(refused, Availability::Refused(_)), "{status}");
    }
    for error in [
        Error::http(500, "boom"),
        Error::Network("connection refused".to_string()),
        Error::Timeout(10),
    ] {
        assert!(matches!(
            Availability::from_read(Err(error)),
            Availability::Unreachable(_)
        ));
    }
    assert!(matches!(
        Availability::from_read(Ok(json!({"unexpected": true}))),
        Availability::Unreachable(_)
    ));
}

#[test]
fn answers_are_cached_for_their_ttl_and_a_failure_never_hides_a_known_catalog() {
    let gate = SlipstreamGate::new();
    let key = GateKey {
        identity: "caller".to_string(),
        workspace_id: workspace(),
    };
    let t0 = Instant::now();
    let catalog = Arc::new(Catalog::from_value(&live_catalog()));
    let entry = gate.record_at(&key, Availability::Available(catalog), t0);
    assert!(entry.is_fresh_at(t0 + AVAILABLE_TTL - Duration::from_secs(1)));
    assert!(!entry.is_fresh_at(t0 + AVAILABLE_TTL));

    // An unreachable catalog keeps the known one, to be read again after 30 s.
    let t1 = t0 + AVAILABLE_TTL + Duration::from_secs(5);
    let kept = gate.record_at(&key, Availability::Unreachable("down".into()), t1);
    assert!(kept.availability.is_listed());
    assert!(kept.is_fresh_at(t1 + UNREACHABLE_RETRY - Duration::from_secs(1)));
    assert!(!kept.is_fresh_at(t1 + UNREACHABLE_RETRY));

    // A refusal replaces it, for 10 minutes.
    let refused = gate.record_at(&key, Availability::Refused("closed".into()), t1);
    assert!(!refused.availability.is_listed());
    assert!(refused.is_fresh_at(t1 + REFUSED_TTL - Duration::from_secs(1)));

    // Unreachable with nothing known: hidden, retried after 30 s.
    let other = GateKey {
        identity: "someone-else".to_string(),
        workspace_id: workspace(),
    };
    let unknown = gate.record_at(&other, Availability::Unreachable("down".into()), t1);
    assert!(!unknown.availability.is_listed());
    assert!(!unknown.is_fresh_at(t1 + UNREACHABLE_RETRY));
    // Another caller's answer is never served for this one.
    assert!(gate.entry(&key).unwrap().availability.catalog().is_none());
}

#[test]
fn the_cache_stays_bounded() {
    let gate = SlipstreamGate::new();
    let now = Instant::now();
    for i in 0..(MAX_ENTRIES + 25) {
        gate.record_at(
            &GateKey {
                identity: format!("caller-{i}"),
                workspace_id: workspace(),
            },
            Availability::Refused("closed".into()),
            now + Duration::from_millis(i as u64),
        );
    }
    assert!(gate.cached_count() <= MAX_ENTRIES);
    // The newest answer is kept.
    assert!(gate
        .entry(&GateKey {
            identity: format!("caller-{}", MAX_ENTRIES + 24),
            workspace_id: workspace(),
        })
        .is_some());
}

async fn listing_for(
    stub_url: &str,
    api_key: &str,
    workspace_id: Option<Uuid>,
) -> SlipstreamListing {
    let mut config = TestFixtures::test_config();
    config.api_url = stub_url.to_string();
    config.api_key = Some(api_key.to_string());
    config.default_workspace_id = workspace_id;
    let client = ContextStreamClient::new(config.clone());
    let session = SessionManager::new(client.clone(), config);
    let gate = SlipstreamGate::new();
    listing(&gate, &client, &session, &input_schema(), LISTING_BUDGET).await
}

#[tokio::test]
async fn the_tool_is_listed_only_for_a_workspace_with_agents_turned_on() {
    // 200 with agents on: listed, with a description and input written from them.
    let stub = Stub::api(live_catalog(), |_| Answer::error(404, "no")).await;
    let listed = listing_for(&stub.url, "key-on", Some(workspace())).await;
    assert!(listed.is_listed());
    let description = listed.description().unwrap();
    assert!(description.contains("- review (1200) [pull, repo]"));
    assert!(description.chars().count() <= 1024);
    let schema = listed.input_schema().unwrap();
    assert!(schema["properties"]["input"]["description"]
        .as_str()
        .unwrap()
        .contains("- lens: media_id* (string)"));
    assert_eq!(schema["required"], json!(["action"]));
    let request = &stub.requests()[0];
    assert_eq!(request.path, "/tasks/agents");
    assert_eq!(request.query, format!("workspace_id={WORKSPACE}"));
    let mut entry = json!({"name": "slipstream", "description": "static", "inputSchema": {}});
    listed.apply(&mut entry);
    assert_eq!(entry["description"], description);

    // 200 with none on: hidden.
    let none_on = Stub::api(
        json!({"agents": [{"agent": {"id": "lens", "price_credits": 400}, "enabled": false}]}),
        |_| Answer::error(404, "no"),
    )
    .await;
    assert!(!listing_for(&none_on.url, "key-none", Some(workspace()))
        .await
        .is_listed());

    // 403 (the workspace is outside the rollout): hidden.
    let closed = Stub::start(Arc::new(|_: &Recorded| {
        Answer::error(403, "Tasks are not available in this workspace yet")
    }))
    .await;
    let hidden = listing_for(&closed.url, "key-403", Some(workspace())).await;
    assert!(!hidden.is_listed());
    assert_eq!(closed.requests().len(), 1, "one read, no retry or replay");

    // Network error: hidden.
    assert!(
        !listing_for("http://127.0.0.1:9", "key-down", Some(workspace()))
            .await
            .is_listed()
    );

    // No workspace known: hidden without asking anyone.
    let unasked = Stub::api(live_catalog(), |_| Answer::error(404, "no")).await;
    let no_workspace = listing_for(&unasked.url, "key-nows", None).await;
    if !no_workspace.is_listed() {
        // (A developer machine may map its working folder to a workspace.)
        assert!(unasked.requests().is_empty());
    }

    // Fingerprints tell listed from hidden, and the same listing twice apart from nothing.
    assert_ne!(listed.fingerprint(), hidden.fingerprint());
    assert_eq!(
        listed.fingerprint(),
        listing_for(&stub.url, "key-on", Some(workspace()))
            .await
            .fingerprint()
    );
}

#[tokio::test]
async fn a_fresh_answer_is_served_from_the_cache() {
    let stub = Stub::api(live_catalog(), |_| Answer::error(404, "no")).await;
    let config = stub.config("key-cache");
    let client = ContextStreamClient::new(config.clone());
    let session = SessionManager::new(client.clone(), config);
    let gate = SlipstreamGate::new();
    for _ in 0..3 {
        assert!(
            listing(&gate, &client, &session, &input_schema(), LISTING_BUDGET)
                .await
                .is_listed()
        );
    }
    assert_eq!(stub.requests().len(), 1);
}

#[tokio::test]
async fn a_change_after_the_listing_is_noticed() {
    let enabled = Arc::new(Mutex::new(true));
    let switch = enabled.clone();
    let stub = Stub::start(Arc::new(move |request: &Recorded| {
        if request.path.starts_with("/projects/") {
            return Answer::ok(json!({"id": PROJECT, "name": "api", "workspace_id": WORKSPACE}));
        }
        let on = *switch.lock().unwrap();
        Answer::ok(json!({"agents": [{"agent": {"id": "lens", "version": "0.2.0", "name": "Lens",
                                                  "description": "Reads an image.", "price_credits": 400},
                                       "enabled": on}]}))
    }))
    .await;
    let config = stub.config("key-change");
    let client = ContextStreamClient::new(config.clone());
    let session = SessionManager::new(client.clone(), config);
    let gate = Arc::new(SlipstreamGate::new());
    let schema = input_schema();

    // Nothing shown yet: nothing to announce.
    assert!(!listing_changed(&gate, &client, &session, &schema).await);
    let shown = listing(&gate, &client, &session, &schema, LISTING_BUDGET).await;
    assert!(shown.is_listed());
    shown.mark_advertised(&gate);
    assert!(!listing_changed(&gate, &client, &session, &schema).await);

    // The agent is turned off and a call reads the catalog (as find and run do).
    *enabled.lock().unwrap() = false;
    let tool = SlipstreamTool::new(client.clone(), Arc::new(session.clone()), gate.clone());
    let found = tool
        .execute(json!({"action": "find", "workspace_id": WORKSPACE}))
        .await
        .unwrap();
    assert!(text_of(&found).contains("0 turned on here"));
    assert!(listing_changed(&gate, &client, &session, &schema).await);
    // Announced once.
    assert!(!listing_changed(&gate, &client, &session, &schema).await);
}

// ============================================================================
// Running an agent
// ============================================================================

fn run_input(extra: Value) -> Value {
    let mut input = json!({"action": "run", "agent": "review", "workspace_id": WORKSPACE,
                           "project_id": PROJECT,
                           "input": {"repo": "acme/api", "pull": 42}});
    if let (Some(map), Some(extra)) = (input.as_object_mut(), extra.as_object()) {
        for (key, value) in extra {
            map.insert(key.clone(), value.clone());
        }
    }
    input
}

#[tokio::test]
async fn a_priced_agent_quotes_first_and_runs_once_the_price_is_accepted() {
    let stub = Stub::api(live_catalog(), |request| {
        if request.method == "POST" {
            Answer::ok(task_view("succeeded", true))
        } else {
            Answer::error(404, "no")
        }
    })
    .await;
    let (tool, _) = stub.tool(stub.config("key-quote"));

    // No accepted price: the quote, and nothing runs.
    let quote = tool.execute(run_input(json!({}))).await.unwrap();
    let text = text_of(&quote);
    assert!(text.starts_with("Not run yet: review (PR Reviewer) costs 1200 credits a run."));
    assert!(text.contains("charged only if the run succeeds"));
    assert!(text.contains("accepted_price_credits=1200"));
    assert!(text.contains(AUTO_ACCEPT_ENV));
    assert_eq!(quote.structured_content.as_ref().unwrap()["ran"], false);
    assert!(stub.task_posts().is_empty());

    // Less than the price: still a quote.
    let low = tool
        .execute(run_input(json!({"accepted_price_credits": 1000})))
        .await
        .unwrap();
    assert!(text_of(&low).contains("now costs 1200 credits a run, more than the 1000 accepted"));
    assert!(stub.task_posts().is_empty());

    // Accepted: one task, asked as the API expects.
    let ran = tool
        .execute(run_input(json!({"accepted_price_credits": 1200})))
        .await
        .unwrap();
    let posts = stub.task_posts();
    assert_eq!(posts.len(), 1);
    let body = &posts[0].body;
    assert_eq!(posts[0].path, format!("/workspaces/{WORKSPACE}/tasks"));
    assert_eq!(body["agent"], "review");
    assert_eq!(body["source"], "mcp");
    assert_eq!(body["accepted_price_credits"], 1200);
    assert_eq!(body["context"], json!({"project_id": PROJECT}));
    assert_eq!(body["input"], json!({"repo": "acme/api", "pull": 42}));
    assert_eq!(body["wait_ms"], DEFAULT_RUN_WAIT_MS);
    assert!(body["idempotency_key"]
        .as_str()
        .unwrap()
        .starts_with("mcp-"));

    // The answer: summary first, the task id on every line, then the cost; then the result.
    let text = text_of(&ran);
    let lines: Vec<&str> = text.lines().take_while(|l| !l.is_empty()).collect();
    assert!(lines[0].starts_with("Summary: Two findings in the retry loop."));
    for line in &lines {
        assert!(line.ends_with(&format!("[task {TASK}]")), "{line}");
    }
    assert!(lines
        .iter()
        .any(|l| l.starts_with("Cost: charged 1200 credits")));
    assert!(text.contains(&format!("Result [task {TASK}]:")));
    assert!(text.contains("report:\n## Findings\n1. The retry loop has no cap."));
    assert_eq!(ran.structured_content.as_ref().unwrap()["task_id"], TASK);
}

#[tokio::test]
async fn the_auto_accept_cap_runs_without_a_quote_up_to_its_price() {
    let stub = Stub::api(live_catalog(), |request| {
        if request.method == "POST" {
            Answer::ok(task_view("succeeded", true))
        } else {
            Answer::error(404, "no")
        }
    })
    .await;
    let mut config = stub.config("key-auto");
    config.slipstream_auto_accept_max_credits = 1200;
    let (tool, _) = stub.tool(config);
    let ran = tool.execute(run_input(json!({}))).await.unwrap();
    let posts = stub.task_posts();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].body["accepted_price_credits"], 1200);
    assert!(text_of(&ran).contains("accepted by the person's auto-accept cap of 1200 credits"));

    // A price above the cap still quotes.
    let mut config = stub.config("key-auto-low");
    config.slipstream_auto_accept_max_credits = 1000;
    let (tool, _) = stub.tool(config);
    let quote = tool.execute(run_input(json!({}))).await.unwrap();
    assert!(text_of(&quote).contains("above the person's auto-accept cap of 1000 credits"));
    assert_eq!(stub.task_posts().len(), 1);

    // The default, 0, always quotes.
    let (tool, _) = stub.tool(stub.config("key-auto-zero"));
    let quote = tool.execute(run_input(json!({}))).await.unwrap();
    assert!(text_of(&quote).starts_with("Not run yet"));
    assert_eq!(stub.task_posts().len(), 1);
}

#[tokio::test]
async fn a_retried_run_carries_the_same_idempotency_key() {
    let stub = Stub::api(live_catalog(), |request| {
        if request.method == "POST" {
            Answer::ok(task_view("succeeded", true))
        } else {
            Answer::error(404, "no")
        }
    })
    .await;
    let (tool, _) = stub.tool(stub.config("key-idem"));
    let accepted = json!({"accepted_price_credits": 1200});
    tool.execute(run_input(accepted.clone())).await.unwrap();
    // The same ask, its input keys in another order.
    let mut again = run_input(accepted.clone());
    again["input"] = json!({"pull": 42, "repo": "acme/api"});
    tool.execute(again).await.unwrap();
    // Another input is another task.
    let mut other = run_input(accepted.clone());
    other["input"] = json!({"repo": "acme/api", "pull": 43});
    tool.execute(other).await.unwrap();
    // A key the caller names is passed through.
    tool.execute(run_input(
        json!({"accepted_price_credits": 1200, "idempotency_key": "mine-1"}),
    ))
    .await
    .unwrap();

    let keys: Vec<String> = stub
        .task_posts()
        .iter()
        .map(|p| p.body["idempotency_key"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(keys.len(), 4);
    assert_eq!(keys[0], keys[1]);
    assert_ne!(keys[0], keys[2]);
    assert_eq!(keys[3], "mine-1");
    assert!(keys[0].len() <= 200);

    // The key covers the window too: the next 10 minutes are another task.
    let input = json!({"repo": "acme/api", "pull": 42});
    let at =
        |secs| derived_idempotency_key("me", workspace(), project(), "review", &input, None, secs);
    assert_eq!(at(1_800_000_000), at(1_800_000_000 + 30));
    assert_ne!(at(1_800_000_000), at(1_800_000_000 + 600));
    assert_eq!(
        derived_idempotency_key(
            "me",
            workspace(),
            project(),
            "review@0.1.0",
            &input,
            None,
            0
        ),
        derived_idempotency_key("me", workspace(), project(), "review", &input, None, 0)
    );
    assert_ne!(
        at(0),
        derived_idempotency_key("you", workspace(), project(), "review", &input, None, 0)
    );
}

#[tokio::test]
async fn an_earlier_identical_ask_is_said_to_be_the_same_task() {
    let stub = Stub::api(live_catalog(), |request| {
        if request.method == "POST" {
            let mut task = task_view("succeeded", true);
            task["created_at"] = json!("2026-01-01T00:00:00Z");
            Answer::ok(task)
        } else {
            Answer::error(404, "no")
        }
    })
    .await;
    let (tool, _) = stub.tool(stub.config("key-replay"));
    let ran = tool
        .execute(run_input(json!({"accepted_price_credits": 1200})))
        .await
        .unwrap();
    assert!(text_of(&ran).contains("This is the task an identical earlier ask started"));
}

#[tokio::test]
async fn a_long_run_returns_its_task_id_and_how_to_follow_it() {
    let stub = Stub::api(live_catalog(), |request| {
        if request.method == "POST" || request.path.contains("/tasks/") {
            Answer::ok(task_view("running", false))
        } else {
            Answer::error(404, "no")
        }
    })
    .await;
    let (tool, _) = stub.tool(stub.config("key-long"));
    let ran = tool
        .execute(run_input(json!({"accepted_price_credits": 1200})))
        .await
        .unwrap();
    let text = text_of(&ran);
    assert!(text.starts_with(&format!(
        "Status: running (review@0.1.0, at reading_diff) [task {TASK}]"
    )));
    assert!(text.contains("Cost: 1200 credits if it succeeds, nothing otherwise"));
    assert!(text.contains(&format!(
        "slipstream(action=\"get_run\", task_id=\"{TASK}\", wait_ms=25000)"
    )));
    let structured = ran.structured_content.unwrap();
    assert_eq!(structured["follow_up"]["action"], "get_run");
    assert_eq!(structured["follow_up"]["wait_ms"], 25000);

    // get_run waits 20 s by default and at most 25 s.
    tool.execute(json!({"action": "get_run", "workspace_id": WORKSPACE, "task_id": TASK}))
        .await
        .unwrap();
    tool.execute(
        json!({"action": "get_run", "workspace_id": WORKSPACE, "task_id": TASK, "wait_ms": 90000}),
    )
    .await
    .unwrap();
    let reads: Vec<String> = stub
        .requests()
        .into_iter()
        .filter(|r| r.method == "GET" && r.path.starts_with("/workspaces/"))
        .map(|r| r.query)
        .collect();
    assert_eq!(reads, ["wait_ms=20000", "wait_ms=25000"]);
}

#[tokio::test]
async fn a_decline_suggests_only_an_agent_the_catalog_has() {
    let decline = |suggested: &str| {
        let mut task = task_view("declined", true);
        task["charged_credits"] = Value::Null;
        task["output"] = json!({"status": "declined", "summary": "This is not a review.",
                                "declined": {"reason": "Fixing tests is not a review", "suggested_agent": suggested}});
        task
    };
    let catalog = Catalog::from_value(&live_catalog());
    let unknown = format::task_text(&decline("ci-fix"), Some(&catalog), &[]);
    assert!(unknown.contains("Declined: Fixing tests is not a review"));
    assert!(!unknown.contains("ci-fix"));
    assert!(!unknown.contains("Try instead"));
    assert!(unknown
        .contains("Cost: not charged (the 1200-credit price applies only to a successful run)"));

    let known = format::task_text(&decline("guide"), Some(&catalog), &[]);
    assert!(known.contains(&format!(
        "Try instead: guide (Repo Guide, 500 credits) [task {TASK}]"
    )));
    // Without a catalog nothing is suggested.
    assert!(!format::task_text(&decline("guide"), None, &[]).contains("Try instead"));

    // Through get_run, against the workspace's own catalog.
    let stub = Stub::api(live_catalog(), move |request| {
        if request.path.contains("/tasks/") {
            Answer::ok(decline("ci-fix"))
        } else {
            Answer::error(404, "no")
        }
    })
    .await;
    let (tool, _) = stub.tool(stub.config("key-decline"));
    let read = tool
        .execute(
            json!({"action": "get_run", "workspace_id": WORKSPACE, "task_id": TASK, "wait_ms": 0}),
        )
        .await
        .unwrap();
    let text = text_of(&read);
    assert!(text.starts_with("Summary: This is not a review."));
    assert!(!text.contains("ci-fix"));
}

#[tokio::test]
async fn refusals_say_what_happened_and_never_run() {
    // A price that rose between the catalog read and the ask: a new quote.
    let stub = Stub::api(live_catalog(), |request| {
        if request.method == "POST" {
            Answer::error(
                409,
                "review now costs 1500 credits a run, more than the 1200 accepted: show the new price and ask again",
            )
        } else {
            Answer::error(404, "no")
        }
    })
    .await;
    let (tool, _) = stub.tool(stub.config("key-409"));
    let raised = tool
        .execute(run_input(json!({"accepted_price_credits": 1200})))
        .await
        .unwrap();
    assert!(text_of(&raised).starts_with("The price changed."));
    assert!(!raised.is_error);

    // A workspace outside the rollout: refused, and the listing learns it.
    let closed = Stub::start(Arc::new(|request: &Recorded| {
        if request.path.starts_with("/projects/") {
            Answer::ok(json!({"id": PROJECT, "name": "api", "workspace_id": WORKSPACE}))
        } else {
            Answer::error(403, "Tasks are not available in this workspace yet")
        }
    }))
    .await;
    let (tool, gate) = closed.tool(closed.config("key-closed"));
    let refused = tool.execute(run_input(json!({}))).await.unwrap();
    assert!(refused.is_error);
    assert!(text_of(&refused).contains("Tasks are not available in this workspace yet"));
    assert!(closed.task_posts().is_empty());
    let found = tool
        .execute(json!({"action": "find", "workspace_id": WORKSPACE}))
        .await
        .unwrap();
    assert!(found.is_error);
    assert!(gate.cached_count() >= 1);

    // An agent the catalog has not, or one turned off.
    let mut value = live_catalog();
    for entry in value["data"]["agents"].as_array_mut().unwrap() {
        if entry["agent"]["id"] == "review" {
            entry["enabled"] = json!(false);
        }
    }
    let off = Stub::api(value, |_| Answer::error(404, "no")).await;
    let (tool, _) = off.tool(off.config("key-off"));
    let missing = tool
        .execute(run_input(json!({"agent": "ci-fix"})))
        .await
        .unwrap_err();
    assert!(missing
        .to_string()
        .contains("No agent ci-fix in this workspace's catalog"));
    let disabled = tool.execute(run_input(json!({}))).await.unwrap();
    assert!(disabled.is_error);
    assert!(text_of(&disabled).contains("is not turned on in this workspace"));
    assert!(off.task_posts().is_empty());

    // Bad input is caught before anything is sent.
    let (tool, _) = stub.tool(stub.config("key-bad"));
    let bad = tool
        .execute(run_input(
            json!({"input": {"repo": "acme/api"}, "accepted_price_credits": 1200}),
        ))
        .await
        .unwrap_err();
    assert!(bad.to_string().contains("missing pull"));
}

#[tokio::test]
async fn malformed_calls_are_refused_before_any_request() {
    let stub = Stub::api(live_catalog(), |_| Answer::error(404, "no")).await;
    let (tool, _) = stub.tool(stub.config("key-malformed"));
    let missing = tool.execute(json!({"action": "run"})).await.unwrap_err();
    assert!(missing.to_string().contains("agent is required"));
    let not_object = tool
        .execute(json!({"action": "run", "agent": "lens", "input": ["x"]}))
        .await
        .unwrap_err();
    assert!(not_object.to_string().contains("input must be"));
    let bad_agent = tool
        .execute(json!({"action": "run", "agent": "Lens&x=1"}))
        .await
        .unwrap_err();
    assert!(bad_agent.to_string().contains("is not an agent id"));
    let unknown = tool.execute(json!({"action": "deploy"})).await.unwrap_err();
    assert!(unknown.to_string().contains("Unknown action"));
    let bad_task = tool
        .execute(json!({"action": "get_run", "workspace_id": WORKSPACE, "task_id": "nope"}))
        .await
        .unwrap_err();
    assert!(bad_task.to_string().contains("Invalid task_id"));
    let bad_filter = tool
        .execute(json!({"action": "list_runs", "workspace_id": WORKSPACE, "agent": "Lens&x=1"}))
        .await
        .unwrap_err();
    assert!(bad_filter.to_string().contains("is not an agent id"));
    assert!(stub.requests().is_empty());
}

#[test]
fn agent_ids_are_checked_before_they_reach_a_query_string() {
    for ok in ["lens", "release-notes", "ci_fix", "a1"] {
        assert!(is_agent_id(ok), "{ok}");
    }
    for bad in [
        "",
        "Lens",
        "lens&x=1",
        "lens/../x",
        "-lens",
        &"a".repeat(65),
    ] {
        assert!(!is_agent_id(bad), "{bad:?}");
    }
}

#[test]
fn an_api_envelope_is_unwrapped_once() {
    assert_eq!(
        unwrap_data(json!({"success": true, "data": {"agents": []}})),
        json!({"agents": []})
    );
    assert_eq!(unwrap_data(json!({"agents": []})), json!({"agents": []}));
    assert_eq!(unwrap_data(json!({"data": 1})), json!({"data": 1}));
}

#[test]
fn find_and_list_runs_read_for_a_model() {
    let catalog = Catalog::from_value(&live_catalog());
    let text = format::catalog_text(&catalog, 0);
    assert!(text.starts_with("8 Slipstream agent(s), 8 turned on here."));
    assert!(text.contains("**PR Reviewer** (review@0.1.0) — 1200 credits; on"));
    assert!(
        text.contains("  - pull (integer, required): The pull request (or merge request) number.")
    );
    assert!(text.contains("  - project_id (string, required; defaults to the session's project)"));
    assert!(text.contains("Example input: {"));
    assert_eq!(
        format::catalog_text(&Catalog::default(), 0),
        "No Slipstream agents are available in this workspace yet."
    );

    let list = format::task_list_text(
        &json!({"tasks": [task_view("succeeded", true), task_view("running", false)]}),
    );
    assert!(list.starts_with("2 run(s), newest first:"));
    assert!(list.contains("- review@0.1.0: succeeded, charged 1200 credits, asked"));
    assert!(list.contains(&format!("[task {TASK}]")));
    assert_eq!(
        format::task_list_text(&json!({"tasks": []})),
        "No Slipstream runs in this workspace yet."
    );
}

#[test]
fn a_large_result_is_cut_for_text_but_kept_whole_in_structured_content() {
    let big = "x".repeat(format::MAX_RESULT_CHARS + 10);
    let rendered = format::render_data(&json!({"report": big}));
    assert!(rendered.chars().count() < format::MAX_RESULT_CHARS + 200);
    assert!(rendered.contains("the structured result has all of it"));
}
