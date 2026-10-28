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
fn the_tool_is_named_agents_and_lists_its_actions() {
    let config = TestFixtures::test_config();
    let client = ContextStreamClient::new(config.clone());
    let session = Arc::new(SessionManager::new(client.clone(), config));
    let tool = SlipstreamTool::new(client, session, Arc::new(SlipstreamGate::new()));
    let metadata = tool.metadata();
    assert_eq!(metadata.name, "agents");
    assert_eq!(TOOL_NAME, "agents");
    assert_eq!(LEGACY_TOOL_NAME, "slipstream");
    assert!(is_tool_name("agents") && is_tool_name("slipstream") && !is_tool_name("agent"));
    assert_eq!(metadata.title, "ContextStream Agents");
    assert!(!metadata.description.to_lowercase().contains("slipstream"));
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
        "agents",
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
    assert!(description.contains("agents(action=\"run\", agent, input)"));
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
                    .contains("more: agents(action=\"find\")"),
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
    let mut entry = json!({"name": "agents", "description": "static", "inputSchema": {}});
    listed.apply(&mut entry);
    assert_eq!(entry["description"], description);

    // 200 with none on: listed, saying how to turn them on.
    let none_on = Stub::api(
        json!({"agents": [{"agent": {"id": "lens", "price_credits": 400}, "enabled": false}]}),
        |_| Answer::error(404, "no"),
    )
    .await;
    let none_listed = listing_for(&none_on.url, "key-none", Some(workspace())).await;
    assert!(none_listed.is_listed());
    assert!(none_listed
        .description()
        .unwrap()
        .contains("None is on in this workspace."));
    assert_ne!(none_listed.fingerprint(), listed.fingerprint());

    // 200 with no agents at all: hidden.
    let empty = Stub::api(json!({"agents": []}), |_| Answer::error(404, "no")).await;
    assert!(!listing_for(&empty.url, "key-empty", Some(workspace()))
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
        "agents(action=\"get_run\", task_id=\"{TASK}\", wait_ms=25000)"
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
    assert!(text.starts_with("8 agent(s), 8 turned on here."));
    assert!(text.contains("**PR Reviewer** (review@0.1.0) — 1200 credits; on"));
    assert!(
        text.contains("  - pull (integer, required): The pull request (or merge request) number.")
    );
    assert!(text.contains("  - project_id (string, required; defaults to the session's project)"));
    assert!(text.contains("Example input: {"));
    assert_eq!(
        format::catalog_text(&Catalog::default(), 0),
        "No agents are available in this workspace yet."
    );

    let list = format::task_list_text(
        &json!({"tasks": [task_view("succeeded", true), task_view("running", false)]}),
    );
    assert!(list.starts_with("2 run(s), newest first:"));
    assert!(list.contains("- review@0.1.0: succeeded, charged 1200 credits, asked"));
    assert!(list.contains(&format!("[task {TASK}]")));
    assert_eq!(
        format::task_list_text(&json!({"tasks": []})),
        "No agent runs in this workspace yet."
    );
}

#[test]
fn a_large_result_is_cut_for_text_but_kept_whole_in_structured_content() {
    let big = "x".repeat(format::MAX_RESULT_CHARS + 10);
    let rendered = format::render_data(&json!({"report": big}));
    assert!(rendered.chars().count() < format::MAX_RESULT_CHARS + 200);
    assert!(rendered.contains("the structured result has all of it"));
}

// ============================================================================
// Settings: which agents are on, and choosing them
// ============================================================================

use super::settings::{
    access_line, category_of, choose_form, chosen, form_state, outcome_text, plan, resolve,
    settings_text, target_of, Manage, Plan, Target, Verb, WorkspaceInfo,
};
use crate::peer::{run_with_call_peer, CallPeer, NoLiveChannel, PeerChannel};

const ME: &str = "44444444-4444-4444-8444-444444444444";
const ADMIN: &str = "55555555-5555-4555-8555-555555555555";

/// The fifteen agents of a production workspace from the catalog: fourteen on, `mail` never
/// chosen (from before agents were on by default).
fn settings_catalog() -> Value {
    serde_json::from_str(include_str!("testdata/settings_catalog.json")).expect("settings fixture")
}

/// The same workspace as the settings view answers it: every agent on, with its access in
/// words, its group, the caller's role and the owners and admins.
fn settings_view() -> Value {
    serde_json::from_str(include_str!("testdata/settings_view.json")).expect("view fixture")
}

/// The catalog with `lens` on but asking for a scope nobody consented to yet.
fn catalog_with_review() -> Value {
    let mut value = settings_catalog();
    for entry in value["data"]["agents"].as_array_mut().unwrap() {
        if entry["agent"]["id"] == "lens" {
            let effective: Vec<Value> = entry["effective_scopes"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|scope| *scope != "media.read")
                .cloned()
                .collect();
            entry["effective_scopes"] = json!(effective);
            entry["consented_scopes"] = json!(effective);
            entry["needs_review"] = json!(true);
        }
    }
    value
}

/// The view with `lens` needing review the same way.
fn view_with_review() -> Value {
    let mut value = settings_view();
    for entry in value["data"]["agents"].as_array_mut().unwrap() {
        if entry["id"] == "lens" {
            entry["needs_review"] = json!(true);
            entry["new_access"] = json!(["media.read"]);
            for item in entry["access"].as_array_mut().unwrap() {
                if item["scope"] == "media.read" {
                    item["granted"] = json!(false);
                    item["new"] = json!(true);
                }
            }
        }
    }
    value["data"]["summary"]["needs_review"] = json!(1);
    value
}

fn owner_info() -> WorkspaceInfo {
    WorkspaceInfo {
        name: Some("Demos".to_string()),
        manage: Manage {
            can_change: Some(true),
            role: Some("owner".to_string()),
            managers: vec!["sam (owner)".to_string()],
        },
        ..WorkspaceInfo::default()
    }
}

fn agent<'a>(catalog: &'a Catalog, id: &str) -> &'a super::catalog::AgentEntry {
    catalog.find(id).expect(id)
}

#[test]
fn the_catalog_and_the_settings_view_carry_access_and_review_state() {
    let catalog = Catalog::from_value(&catalog_with_review());
    assert_eq!(catalog.agents.len(), 15);
    let lens = agent(&catalog, "lens");
    assert!(lens.enabled && lens.needs_review);
    assert!(lens.scopes.contains(&"media.read".to_string()));
    assert_eq!(lens.new_scopes(), vec!["media.read"]);
    assert!(lens.access.is_empty() && lens.job_line.is_none());

    // The view: flat agents with words, groups and the one-line job.
    let view = Catalog::from_value(&view_with_review());
    assert_eq!(view.agents.len(), 15);
    assert_eq!(
        view.categories.first(),
        Some(&("code".to_string(), "Code and repositories".to_string()))
    );
    let lens = agent(&view, "lens");
    assert_eq!(lens.category.as_deref(), Some("files"));
    assert!(lens
        .job_line
        .as_deref()
        .unwrap()
        .starts_with("Reads an image you shared"));
    assert_eq!(lens.scopes.len(), lens.access.len());
    assert!(lens
        .access
        .iter()
        .any(|a| a.scope == "media.read" && a.new && !a.granted));
    assert!(lens.needs_review);
    assert!(view.find("mail").unwrap().enabled, "on by default");

    // Groups: the API's, else the same mapping here.
    let fallback = Catalog::from_value(&settings_catalog());
    for (id, group) in [
        ("review", "code"),
        ("audit", "code"),
        ("lens", "files"),
        ("keeper", "work"),
        ("mail", "research"),
        ("research-brief", "research"),
    ] {
        assert_eq!(category_of(agent(&fallback, id)), group, "{id}");
    }
    let unknown = Catalog::from_value(&json!({"agents": [
        {"agent": {"id": "triage", "scopes": ["vcs.read", "model.invoke"]}, "enabled": true},
        {"agent": {"id": "inbox", "scopes": ["mail.read"]}, "enabled": true},
        {"agent": {"id": "misc", "scopes": ["model.invoke"]}, "enabled": true}]}));
    let groups: Vec<String> = unknown.agents.iter().map(category_of).collect();
    assert_eq!(groups, vec!["code", "research", "other"]);
}

#[test]
fn access_is_said_in_the_apis_plain_words() {
    // Worded here from the catalog, and as the view words it: the same line.
    for value in [settings_catalog(), settings_view()] {
        let catalog = Catalog::from_value(&value);
        let line = |id: &str| access_line(agent(&catalog, id));
        assert_eq!(line("keeper"), "reads plans and tasks; changes nothing");
        assert_eq!(
            line("lens"),
            "reads task context, reads images you share, reads decisions and lessons, searches code; marks context it used, saves one note"
        );
        assert_eq!(line("mail"), "reads your mail; drafts replies, never sends");
        assert_eq!(
            line("gardener"),
            "reads plans and tasks, reads decisions and lessons; proposes memory changes"
        );
        assert_eq!(line("release-notes"), "reads repositories; saves one doc");
        assert_eq!(
            line("pdf"),
            "reads a doc you name, reads plans and tasks; saves one file"
        );
    }
    // A scope with no words is shown by its name, as something it changes.
    let odd = Catalog::from_value(&json!({"agents": [{"agent": {"id": "x",
        "scopes": ["calendar.write", "model.invoke"]}, "enabled": true}]}));
    assert_eq!(access_line(&odd.agents[0]), "calendar.write");
    // Needing review: what its runs hold now, and the new access apart.
    let review = Catalog::from_value(&view_with_review());
    assert_eq!(
        access_line(agent(&review, "lens")),
        "reads task context, reads decisions and lessons, searches code; marks context it used, saves one note"
    );
}

#[test]
fn settings_is_a_checklist_grouped_by_kind_of_work() {
    let catalog = Catalog::from_value(&settings_catalog());
    let text = settings_text(&catalog, &owner_info());
    eprintln!(
        "--- settings ({} chars) ---\n{text}\n---",
        text.chars().count()
    );
    assert!(text.starts_with(
        "ContextAgents in Demos: 14 of 15 on.\nYou can change them: you own this workspace.\n"
    ));
    for group in [
        "Code and repositories · 8 of 8 on",
        "Files and data · 3 of 3 on",
        "Plans and memory · 2 of 2 on",
        "Mail and research · 1 of 2 on",
    ] {
        assert!(text.contains(&format!("\n{group}\n")), "{group}");
    }
    let at = |needle: &str| text.find(needle).unwrap();
    assert!(at("\nCode and repositories ·") < at("\nFiles and data ·"));
    assert!(at("\nPlans and memory ·") < at("\nMail and research ·"));
    assert!(text.contains("  [ ] mail  Mail (500 credits): Reads your recent mail"));
    assert!(text.contains("\n      reads your mail; drafts replies, never sends\n"));
    assert!(text.contains("  [x] keeper  Plans & Tasks Keeper (free): "));
    assert!(text.contains("  [x] review  PR Reviewer (1200 credits): "));
    assert!(text.ends_with(
        "Change: agents(action=\"choose\") asks the person in a form; or enable / disable with agent=\"<id>\" or \"all\", or agents=[\"<id>\", ...]."
    ));
    assert!(!text.to_lowercase().contains("slipstream"));
    // Two lines an agent, inside a small budget.
    assert!(
        text.lines().count() <= 15 * 2 + 4 * 2 + 4,
        "{}",
        text.lines().count()
    );
    assert!(text.chars().count() < 4000, "{}", text.chars().count());

    // The view: every agent on by default, its own groups and jobs, and why runs are blocked.
    let view = settings_view();
    let mut info = WorkspaceInfo::from_view(&view["data"], Some(&json!({"name": "Demos"})));
    info.runs_blocked = Some(
        "ContextAgents can't run in this workspace yet. Your choices here are saved and apply as soon as they can."
            .to_string(),
    );
    let text = settings_text(&Catalog::from_value(&view), &info);
    assert!(text.starts_with(
        "ContextAgents in Demos: 15 of 15 on.\nYou can change them: you own this workspace.\nContextAgents can't run in this workspace yet."
    ));
    assert!(text.contains("\nMail and research · 2 of 2 on\n"));

    // An agent that needs review says what it asks for and how to grant it.
    let review = Catalog::from_value(&catalog_with_review());
    let text = settings_text(&review, &owner_info());
    assert!(text.starts_with("ContextAgents in Demos: 14 of 15 on, 1 needs review."));
    assert!(text.contains(
        "      needs review: it also asks to read a media file you name, its download link and its previews; enable lens to grant that\n"
    ));

    assert_eq!(
        settings_text(&Catalog::default(), &WorkspaceInfo::default()),
        "No ContextAgents are available in this workspace yet."
    );
}

#[test]
fn who_can_change_it_is_read_from_the_view_or_the_members() {
    // The view says it.
    let mut view = settings_view()["data"].clone();
    let info = WorkspaceInfo::from_view(&view, None);
    assert_eq!(info.manage.can_change, Some(true));
    assert_eq!(
        info.manage.managers,
        vec!["Sam Rivera (owner)", "Ana Lee (admin)"]
    );
    assert_eq!(info.runs_open, Some(true));
    assert_eq!(info.runs_blocked, None);
    view["role"] = json!("member");
    view["can_manage"] = json!(false);
    let info = WorkspaceInfo::from_view(&view, Some(&json!({"name": "Demos"})));
    assert_eq!(
        info.manage.summary_line(),
        "Read-only for you (you're a member here): Sam Rivera (owner) or Ana Lee (admin) can change them."
    );
    let text = settings_text(&Catalog::from_value(&view), &info);
    assert!(text.ends_with("To change them, ask Sam Rivera (owner) or Ana Lee (admin)."));
    assert!(!text.contains("Change: agents("));

    // An API without the view: the workspace and its members.
    let workspace = json!({"id": WORKSPACE, "name": "Demos", "owner_username": "sam"});
    let me = json!({"id": ME, "email": "me@example.com"});
    let members = json!({"items": [
        {"user": {"id": ME, "username": "me", "full_name": "Mia"}, "role": "member", "status": "active"},
        {"user": {"id": ADMIN, "username": "ana", "full_name": "Ana Lee"}, "role": "admin", "status": "active"},
        {"user": {"id": "66666666-6666-4666-8666-666666666666", "full_name": "Old Admin"}, "role": "admin", "status": "inactive"}
    ], "total": 3});
    let info = WorkspaceInfo::from_reads(Some(&workspace), Some(&me), Some(&members));
    assert_eq!(info.name.as_deref(), Some("Demos"));
    assert_eq!(info.manage.can_change, Some(false));
    assert_eq!(info.manage.role.as_deref(), Some("member"));
    assert_eq!(info.manage.managers, vec!["sam (owner)", "Ana Lee (admin)"]);
    let admin = WorkspaceInfo::from_reads(
        Some(&workspace),
        Some(&json!({"id": ADMIN})),
        Some(&members),
    );
    assert_eq!(
        admin.manage.summary_line(),
        "You can change them: you're an admin here."
    );
    // Someone who reads the catalog but is not a member row owns the workspace...
    let stranger = json!({"id": "77777777-7777-4777-8777-777777777777"});
    let owner = WorkspaceInfo::from_reads(Some(&workspace), Some(&stranger), Some(&members));
    assert_eq!(owner.manage.role.as_deref(), Some("owner"));
    // ... unless the member list was only a page of a longer one.
    let partial = json!({"items": members["items"], "total": 250});
    let unknown = WorkspaceInfo::from_reads(Some(&workspace), Some(&stranger), Some(&partial));
    assert_eq!(unknown.manage.can_change, None);
    assert_eq!(
        unknown.manage.summary_line(),
        "The workspace owner or an admin can change them."
    );
    let nothing = WorkspaceInfo::from_reads(None, None, None);
    assert_eq!(nothing.manage, Manage::default());
    assert_eq!(nothing.where_label(), "in this workspace");
    assert_eq!(nothing.manage.who(), "the workspace owner or an admin");
}

#[test]
fn a_call_names_agents_by_id_name_list_all_or_none() {
    let catalog = Catalog::from_value(&settings_catalog());
    let ids = |target: Target| match target {
        Target::Some(names) => resolve(&catalog, &names)
            .unwrap()
            .iter()
            .map(|entry| entry.id.clone())
            .collect::<Vec<_>>(),
        other => panic!("{other:?}"),
    };
    assert_eq!(ids(target_of(Some("lens"), None).unwrap()), vec!["lens"]);
    assert_eq!(
        ids(target_of(Some("lens, pdf@0.1.0"), None).unwrap()),
        vec!["lens", "pdf"]
    );
    assert_eq!(
        ids(target_of(None, Some(&json!(["Code Oracle", "LENS", "lens"]))).unwrap()),
        vec!["oracle", "lens"]
    );
    assert_eq!(target_of(Some("all"), None).unwrap(), Target::All);
    assert_eq!(target_of(None, Some(&json!(["ALL"]))).unwrap(), Target::All);
    assert_eq!(target_of(None, Some(&json!("none"))).unwrap(), Target::None);
    assert!(target_of(None, Some(&json!(["all", "lens"]))).is_err());
    assert!(target_of(None, None).is_err());
    assert!(target_of(Some("  "), Some(&json!([]))).is_err());
    assert!(target_of(None, Some(&json!([1]))).is_err());
    let unknown = resolve(&catalog, &["nope".to_string()]).unwrap_err();
    assert!(unknown.starts_with("No agent nope here. The agents: analyst, audit,"));
}

#[test]
fn only_what_changes_is_planned_and_new_access_is_never_granted_silently() {
    let catalog = Catalog::from_value(&catalog_with_review());
    let ids = |plan: &Plan, on: bool| -> Vec<String> {
        plan.changes
            .iter()
            .filter(|c| c.on == on)
            .map(|c| c.id.clone())
            .collect()
    };
    let on_now: Vec<&str> = catalog.enabled().iter().map(|a| a.id.as_str()).collect();

    // enable all: only what is off; lens keeps its consent; the bulk write says "all".
    let all = plan(&catalog, Verb::Enable, &Target::All).unwrap();
    assert_eq!(ids(&all, true), vec!["mail"]);
    assert_eq!(all.still_review, vec!["lens"]);
    assert_eq!(all.bulk_enable(&catalog), json!("all"));
    // disable all: the fourteen on, "none".
    let off = plan(&catalog, Verb::Disable, &Target::All).unwrap();
    assert_eq!(off.changes.len(), 14);
    assert_eq!(off.bulk_enable(&catalog), json!("none"));
    // enable none is everything off; disable none changes nothing.
    let none = plan(&catalog, Verb::Enable, &Target::None).unwrap();
    assert_eq!(none.changes.len(), 14);
    assert_eq!(none.bulk_enable(&catalog), json!("none"));
    // disable none: none off, so every agent on (as `csa disable none`).
    let none_off = plan(&catalog, Verb::Disable, &Target::None).unwrap();
    assert_eq!(ids(&none_off, true), vec!["mail"]);
    assert_eq!(none_off.bulk_enable(&catalog), json!("all"));
    assert!(none_off.note.as_deref().unwrap().contains("every agent on"));

    // Naming agents changes only those: the bulk set is what is on now plus or minus them.
    let named = plan(
        &catalog,
        Verb::Enable,
        &Target::Some(vec!["mail".into(), "oracle".into()]),
    )
    .unwrap();
    assert_eq!(ids(&named, true), vec!["mail"]);
    assert_eq!(named.already_on, vec!["oracle"]);
    let mut expected: Vec<&str> = on_now.clone();
    expected.push("mail");
    expected.sort();
    assert_eq!(named.bulk_enable(&catalog), json!(expected));
    let disable = plan(
        &catalog,
        Verb::Disable,
        &Target::Some(vec!["mail".into(), "keeper".into()]),
    )
    .unwrap();
    assert_eq!(ids(&disable, false), vec!["keeper"]);
    assert_eq!(disable.already_off, vec!["mail"]);
    let expected: Vec<&str> = on_now
        .iter()
        .copied()
        .filter(|id| *id != "keeper")
        .collect();
    assert_eq!(disable.bulk_enable(&catalog), json!(expected));

    // Naming lens in enable grants its new access, and says so; it is already in the set.
    let review = plan(&catalog, Verb::Enable, &Target::Some(vec!["lens".into()])).unwrap();
    assert_eq!(review.changes.len(), 1);
    assert_eq!(
        review.changes[0].grants,
        vec!["read a media file you name, its download link and its previews"]
    );

    // choose sets exactly the set.
    let chosen_set = plan(
        &catalog,
        Verb::Choose,
        &Target::Some(vec!["oracle".into(), "mail".into(), "lens".into()]),
    )
    .unwrap();
    assert_eq!(ids(&chosen_set, true), vec!["mail"]);
    assert_eq!(ids(&chosen_set, false).len(), 12);
    assert_eq!(chosen_set.still_review, vec!["lens"]);
    assert_eq!(
        chosen_set.bulk_enable(&catalog),
        json!(["lens", "mail", "oracle"])
    );
    assert!(plan(&catalog, Verb::Choose, &Target::Some(vec!["nope".into()])).is_err());
}

#[test]
fn the_form_lists_every_agent_with_those_on_checked() {
    let catalog = Catalog::from_value(&settings_catalog());
    let multi = choose_form(&catalog, true);
    eprintln!(
        "--- form ---\n{}",
        serde_json::to_string_pretty(&multi).unwrap()
    );
    let field = &multi["properties"]["enabled"];
    assert_eq!(field["type"], "array");
    let options = field["items"]["anyOf"].as_array().unwrap();
    assert_eq!(options.len(), 15);
    assert!(options
        .iter()
        .all(|o| o["const"].is_string() && o["title"].as_str().unwrap().chars().count() <= 100));
    assert_eq!(
        options[0]["const"], "ci-fix",
        "Code and repositories first, by name"
    );
    let default = field["default"].as_array().unwrap();
    assert_eq!(default.len(), 14);
    assert!(!default.contains(&json!("mail")));
    assert_eq!(multi["required"], json!(["enabled"]));

    // Before 2025-11-25: one checkbox per agent, flat and primitive.
    let boxes = choose_form(&catalog, false);
    let properties = boxes["properties"].as_object().unwrap();
    assert_eq!(properties.len(), 15);
    assert!(properties.values().all(|p| p["type"] == "boolean"));
    assert_eq!(properties["mail"]["default"], json!(false));
    assert_eq!(properties["oracle"]["default"], json!(true));
    assert_eq!(properties["mail"]["title"], "Mail (500 credits)");
    assert!(properties["mail"]["description"]
        .as_str()
        .unwrap()
        .starts_with("Mail and research: Reads your recent mail"));

    // Either answer reads back as the agents to have on.
    let picked = |content: Value| chosen(&catalog, content.as_object().unwrap());
    let answer = picked(json!({"enabled": ["mail", "oracle", "nope"]})).unwrap();
    assert_eq!(answer, vec!["mail", "oracle"]);
    assert_eq!(
        picked(json!({"enabled": []})).unwrap(),
        Vec::<String>::new()
    );
    let answer = picked(json!({"keeper": false, "mail": true})).unwrap();
    assert_eq!(answer.len(), 14);
    assert!(answer.contains(&"mail".to_string()) && !answer.contains(&"keeper".to_string()));
    assert!(picked(json!({})).is_none());

    // The question is about this state: another state asks again.
    let state = form_state(workspace(), &catalog);
    assert!(state.starts_with(&format!("agents-choose:{WORKSPACE}:")));
    let mut changed = settings_catalog();
    changed["data"]["agents"][9]["enabled"] = json!(true);
    assert_ne!(
        state,
        form_state(workspace(), &Catalog::from_value(&changed))
    );
}

#[test]
fn a_change_says_what_it_did_and_how_to_undo_it() {
    let before = Catalog::from_value(&settings_catalog());
    let plan_off = plan(&before, Verb::Disable, &Target::Some(vec!["keeper".into()])).unwrap();
    let mut after_value = settings_catalog();
    for entry in after_value["data"]["agents"].as_array_mut().unwrap() {
        if entry["agent"]["id"] == "keeper" {
            entry["enabled"] = json!(false);
        }
    }
    let after = Catalog::from_value(&after_value);
    let text = outcome_text(&owner_info(), &before, Some(&after), &plan_off, &[]);
    assert_eq!(
        text,
        "ContextAgents in Demos: 13 of 15 on (was 14).\nTurned off: keeper (Plans & Tasks Keeper).\nUndo: agents(action=\"enable\", agent=\"keeper\")"
    );
    let none = plan(&before, Verb::Enable, &Target::Some(vec!["oracle".into()])).unwrap();
    assert_eq!(
        outcome_text(&owner_info(), &before, Some(&before), &none, &[]),
        "Nothing changed: 14 of 15 ContextAgents are on in Demos.\nAlready on: oracle."
    );
    let two = plan(
        &before,
        Verb::Disable,
        &Target::Some(vec!["keeper".into(), "pdf".into()]),
    )
    .unwrap();
    let failed = vec![(two.changes[1].clone(), "boom".to_string())];
    let text = outcome_text(&owner_info(), &before, None, &two, &failed);
    assert!(text.starts_with("ContextAgents in Demos: 13 of 15 on (was 14)."));
    assert!(text.contains("Not changed: pdf stays on: boom"));
    let mixed = plan(
        &before,
        Verb::Choose,
        &Target::Some(vec!["oracle".into(), "mail".into()]),
    )
    .unwrap();
    let text = outcome_text(&owner_info(), &before, None, &mixed, &[]);
    assert!(text.contains("Undo: agents(action=\"choose\", agents=[\"analyst\", "));
}

// --- Through the tool, against a stub API -------------------------------------

/// What a stub API holds and how it answers.
#[derive(Clone, Copy)]
struct StubShape {
    /// The settings view and the bulk write answer (an API with them).
    view: bool,
    /// The caller may change the settings.
    admin: bool,
    /// The catalog's status (`403`: outside the rollout).
    catalog_status: u16,
    /// The members can be read (an API without the view).
    members: bool,
}

const OWNER_WITH_VIEW: StubShape = StubShape {
    view: true,
    admin: true,
    catalog_status: 200,
    members: true,
};
const OWNER_WITHOUT_VIEW: StubShape = StubShape {
    view: false,
    ..OWNER_WITH_VIEW
};

fn set_enabled(view: &mut Value, id: &str, on: bool) {
    for entry in view["agents"].as_array_mut().unwrap() {
        if entry["id"] == id {
            entry["enabled"] = json!(on);
            for item in entry["access"].as_array_mut().unwrap() {
                item["granted"] = json!(on && item["new"] != json!(true));
            }
        }
    }
}

/// A stub that keeps what the tool sets. With the view, it answers the view and the bulk
/// write; without, the catalog (from `catalog`), the caller and the members. Both take the
/// per-agent write.
async fn settings_stub(catalog: Value, view: Value, shape: StubShape) -> Stub {
    let catalog = Arc::new(Mutex::new(catalog));
    let view = Arc::new(Mutex::new(view["data"].clone()));
    Stub::start(Arc::new(move |request: &Recorded| {
        let path = request.path.as_str();
        let settings = format!("/workspaces/{WORKSPACE}/tasks/agents/consent");
        let refused = || Answer::error(403, "Only an owner or admin sets which agents may run");
        if path == settings {
            if !shape.view {
                return Answer::error(404, "Not found");
            }
            if request.method == "PUT" {
                if !shape.admin {
                    return refused();
                }
                let mut view = view.lock().unwrap();
                let ids: Vec<String> = view["agents"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|a| a["id"].as_str().unwrap().to_string())
                    .collect();
                for id in ids {
                    let on = match &request.body["enable"] {
                        Value::String(word) => word == "all",
                        Value::Array(set) => set.contains(&json!(id)),
                        _ => return Answer::error(400, "enable is all, none or a list"),
                    };
                    set_enabled(&mut view, &id, on);
                }
                return Answer::ok(view.clone());
            }
            return Answer::ok(view.lock().unwrap().clone());
        }
        if path == "/tasks/agents" {
            if shape.catalog_status != 200 {
                return Answer::error(
                    shape.catalog_status,
                    "Tasks are not available in this workspace yet",
                );
            }
            return Answer::ok(catalog.lock().unwrap()["data"].clone());
        }
        if path == format!("/workspaces/{WORKSPACE}") {
            return Answer::ok(json!({"id": WORKSPACE, "name": "Demos", "owner_username": "sam"}));
        }
        if path == "/auth/me" {
            return Answer::ok(json!({"id": ME, "email": "me@example.com"}));
        }
        if path == format!("/workspaces/{WORKSPACE}/members") {
            if !shape.members {
                return Answer::error(404, "no");
            }
            let role = if shape.admin { "admin" } else { "member" };
            return Answer::ok(json!({"items": [
                {"user": {"id": ME, "username": "me"}, "role": role, "status": "active"},
                {"user": {"id": ADMIN, "username": "ana", "full_name": "Ana Lee"}, "role": "admin", "status": "active"}
            ], "total": 2}));
        }
        if path.starts_with("/projects/") {
            return Answer::ok(json!({"id": PROJECT, "name": "api", "workspace_id": WORKSPACE}));
        }
        if request.method == "PUT" && path.ends_with("/consent") {
            if !shape.admin {
                return refused();
            }
            let agent = path.split('/').nth_back(1).unwrap().to_string();
            let on = request.body["enabled"].as_bool().unwrap();
            for entry in catalog.lock().unwrap()["data"]["agents"].as_array_mut().unwrap() {
                if entry["agent"]["id"] == agent.as_str() {
                    entry["enabled"] = json!(on);
                    if on {
                        entry["needs_review"] = json!(false);
                    }
                }
            }
            let mut view = view.lock().unwrap();
            set_enabled(&mut view, &agent, on);
            for entry in view["agents"].as_array_mut().unwrap() {
                if entry["id"] == agent.as_str() && on {
                    entry["needs_review"] = json!(false);
                    for item in entry["access"].as_array_mut().unwrap() {
                        item["new"] = json!(false);
                        item["granted"] = json!(true);
                    }
                }
            }
            return Answer::ok(json!({"enabled": on}));
        }
        Answer::error(404, "no")
    }))
    .await
}

fn writes(stub: &Stub) -> Vec<(String, Value)> {
    stub.requests()
        .into_iter()
        .filter(|r| r.method == "PUT")
        .map(|r| (r.path, r.body))
        .collect()
}

fn settings_path_of() -> String {
    format!("/workspaces/{WORKSPACE}/tasks/agents/consent")
}

#[tokio::test]
async fn with_the_settings_routes_one_bulk_write_sets_exactly_what_is_meant() {
    let stub = settings_stub(settings_catalog(), view_with_review(), OWNER_WITH_VIEW).await;
    let (tool, _) = stub.tool(stub.config("key-view"));

    let settings = tool.execute(json!({"action": "settings"})).await.unwrap();
    let text = text_of(&settings);
    assert!(text.starts_with(
        "ContextAgents in Demos: 15 of 15 on, 1 needs review.\nYou can change them: you own this workspace.\n\nCode and repositories · 8 of 8 on\n"
    ));
    let data = settings.structured_content.clone().unwrap();
    assert_eq!(data["runs"]["open"], true);
    assert_eq!(data["agents"][0]["id"], "ci-fix");
    // Only the view and the workspace were read: no members, no catalog.
    assert!(stub
        .requests()
        .iter()
        .all(|r| r.path != "/auth/me" && r.path != "/tasks/agents"));

    // disable names one: the set is everything on but keeper.
    let off = tool
        .execute(json!({"action": "disable", "agent": "keeper"}))
        .await
        .unwrap();
    assert_eq!(
        text_of(&off),
        "ContextAgents in Demos: 14 of 15 on (was 15).\nTurned off: keeper (Plans & Tasks Keeper).\nUndo: agents(action=\"enable\", agent=\"keeper\")"
    );
    let sent = writes(&stub);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].0, settings_path_of());
    let set = sent[0].1["enable"].as_array().unwrap();
    assert_eq!(set.len(), 14);
    assert!(!set.contains(&json!("keeper")) && set.contains(&json!("lens")));
    assert_eq!(sent[0].1["accept_new_access"], false);

    // enable names one: everything on plus it.
    let on = tool
        .execute(json!({"action": "enable", "agents": ["keeper"]}))
        .await
        .unwrap();
    assert!(text_of(&on).starts_with("ContextAgents in Demos: 15 of 15 on (was 14)."));
    assert_eq!(writes(&stub)[1].1["enable"].as_array().unwrap().len(), 15);

    // none, then all: the words themselves.
    let none = tool
        .execute(json!({"action": "choose", "agents": "none"}))
        .await
        .unwrap();
    assert!(text_of(&none).starts_with("ContextAgents in Demos: 0 of 15 on (was 15)."));
    assert_eq!(writes(&stub)[2].1["enable"], "none");
    let all = tool
        .execute(json!({"action": "enable", "agent": "all"}))
        .await
        .unwrap();
    assert!(text_of(&all).starts_with("ContextAgents in Demos: 15 of 15 on (was 0)."));
    assert_eq!(writes(&stub)[3].1["enable"], "all");

    // Naming the agent that needs review grants its new access, one agent at a time.
    let reviewed = tool
        .execute(json!({"action": "enable", "agent": "lens"}))
        .await
        .unwrap();
    assert!(text_of(&reviewed).contains(
        "Reviewed: lens may now also read a media file you name, its download link and its previews."
    ));
    let last = writes(&stub).pop().unwrap();
    assert_eq!(
        last,
        (
            format!("/workspaces/{WORKSPACE}/tasks/agents/lens/consent"),
            json!({"enabled": true})
        )
    );
    assert_eq!(writes(&stub).len(), 5);
}

#[tokio::test]
async fn without_the_settings_routes_each_agent_is_written_alone() {
    let stub = settings_stub(settings_catalog(), settings_view(), OWNER_WITHOUT_VIEW).await;
    let (tool, gate) = stub.tool(stub.config("key-settings"));

    let settings = tool.execute(json!({"action": "settings"})).await.unwrap();
    assert!(!settings.is_error);
    let text = text_of(&settings);
    assert!(text.starts_with(
        "ContextAgents in Demos: 14 of 15 on.\nYou can change them: you're an admin here."
    ));
    let data = settings.structured_content.clone().unwrap();
    assert_eq!(
        (data["on"].clone(), data["total"].clone()),
        (json!(14), json!(15))
    );

    let off = tool
        .execute(json!({"action": "disable", "agent": "keeper"}))
        .await
        .unwrap();
    assert_eq!(
        text_of(&off),
        "ContextAgents in Demos: 13 of 15 on (was 14).\nTurned off: keeper (Plans & Tasks Keeper).\nUndo: agents(action=\"enable\", agent=\"keeper\")"
    );
    let per_agent: Vec<(String, Value)> = writes(&stub)
        .into_iter()
        .filter(|(path, _)| *path != settings_path_of())
        .collect();
    assert_eq!(
        per_agent,
        vec![(
            format!("/workspaces/{WORKSPACE}/tasks/agents/keeper/consent"),
            json!({"enabled": false})
        )]
    );
    // The listing learned it from the read after the change.
    let key = GateKey {
        identity: caller_identity(&tool.client).await.unwrap(),
        workspace_id: workspace(),
    };
    let cached = gate.entry(&key).unwrap();
    assert!(
        !cached
            .availability
            .catalog()
            .unwrap()
            .find("keeper")
            .unwrap()
            .enabled
    );

    // enable all turns on only what is off.
    let all = tool
        .execute(json!({"action": "enable", "agents": ["all"]}))
        .await
        .unwrap();
    assert!(text_of(&all).starts_with("ContextAgents in Demos: 15 of 15 on (was 13)."));
    assert_eq!(writes(&stub).len(), 3);

    // Already so: nothing is sent.
    let again = tool
        .execute(json!({"action": "enable", "agent": "mail"}))
        .await
        .unwrap();
    assert!(text_of(&again).starts_with("Nothing changed: 15 of 15 ContextAgents are on in Demos."));
    assert_eq!(writes(&stub).len(), 3);

    // A name the catalog lacks is refused before anything is sent.
    let unknown = tool
        .execute(json!({"action": "disable", "agent": "nope"}))
        .await
        .unwrap_err();
    assert!(unknown.to_string().contains("No agent nope here."));
    let empty = tool.execute(json!({"action": "enable"})).await.unwrap_err();
    assert!(empty.to_string().contains("Name the agents"));
    assert_eq!(writes(&stub).len(), 3);
}

#[tokio::test]
async fn a_member_is_told_who_can_change_it_and_nothing_is_sent() {
    let mut member_view = settings_view();
    member_view["data"]["role"] = json!("member");
    member_view["data"]["can_manage"] = json!(false);
    let member = StubShape {
        admin: false,
        ..OWNER_WITH_VIEW
    };
    let stub = settings_stub(settings_catalog(), member_view, member).await;
    let (tool, _) = stub.tool(stub.config("key-member"));
    let settings = tool.execute(json!({"action": "settings"})).await.unwrap();
    assert!(text_of(&settings).contains(
        "Read-only for you (you're a member here): Sam Rivera (owner) or Ana Lee (admin) can change them."
    ));
    let refused = tool
        .execute(json!({"action": "disable", "agent": "mail"}))
        .await
        .unwrap();
    assert!(!refused.is_error);
    assert_eq!(
        text_of(&refused),
        "Nothing changed: only Sam Rivera (owner) or Ana Lee (admin) can choose which ContextAgents are on in Demos (you're a member here). Ask them to turn mail off. agents(action=\"settings\") shows what is on now."
    );
    let choose = tool.execute(json!({"action": "choose"})).await.unwrap();
    assert!(
        text_of(&choose).starts_with("Nothing changed: only Sam Rivera (owner) or Ana Lee (admin)")
    );
    assert!(writes(&stub).is_empty());

    // Without the view, and the members unreadable: the first write finds out, then stops.
    let unknown = StubShape {
        view: false,
        admin: false,
        members: false,
        catalog_status: 200,
    };
    let stub = settings_stub(settings_catalog(), settings_view(), unknown).await;
    let (tool, _) = stub.tool(stub.config("key-unknown"));
    let refused = tool
        .execute(json!({"action": "disable", "agents": ["keeper", "pdf", "lens"]}))
        .await
        .unwrap();
    assert!(text_of(&refused).starts_with(
        "Nothing changed: only sam (owner) can choose which ContextAgents are on in Demos."
    ));
    assert_eq!(writes(&stub).len(), 1, "one try, then stop");
}

#[tokio::test]
async fn runs_not_open_yet_are_said_plainly() {
    // With the view: the settings still read and change; why runs can't start is said.
    let mut closed_view = settings_view();
    closed_view["data"]["runs"] = json!({"open": false, "blocked": {"code": "not_open",
        "message": "ContextAgents can't run in this workspace yet. Your choices here are saved and apply as soon as they can."},
        "daily": {"used": 0, "limit": 200, "reached": false, "resets_at": "2026-10-29T00:00:00Z"}});
    let closed = StubShape {
        catalog_status: 403,
        ..OWNER_WITH_VIEW
    };
    let stub = settings_stub(settings_catalog(), closed_view, closed).await;
    let (tool, gate) = stub.tool(stub.config("key-not-open"));
    let settings = tool.execute(json!({"action": "settings"})).await.unwrap();
    assert!(text_of(&settings).contains(
        "\nContextAgents can't run in this workspace yet. Your choices here are saved and apply as soon as they can.\n"
    ));
    assert_eq!(
        settings.structured_content.as_ref().unwrap()["runs"]["open"],
        false
    );
    let off = tool
        .execute(json!({"action": "disable", "agent": "mail"}))
        .await
        .unwrap();
    assert!(text_of(&off).starts_with("ContextAgents in Demos: 14 of 15 on (was 15)."));
    // The tool stays unlisted there: the catalog is still closed.
    let key = GateKey {
        identity: caller_identity(&tool.client).await.unwrap(),
        workspace_id: workspace(),
    };
    assert!(!gate.entry(&key).unwrap().availability.is_listed());

    // Without the view: a closed catalog means nothing to choose.
    let old_closed = StubShape {
        view: false,
        catalog_status: 403,
        ..OWNER_WITH_VIEW
    };
    let stub = settings_stub(settings_catalog(), settings_view(), old_closed).await;
    let (tool, _) = stub.tool(stub.config("key-closed-settings"));
    for call in [
        json!({"action": "settings"}),
        json!({"action": "enable", "agent": "all"}),
        json!({"action": "choose"}),
    ] {
        let answer = tool.execute(call).await.unwrap();
        assert!(!answer.is_error);
        assert!(text_of(&answer).starts_with(
            "ContextAgents aren't open in Demos yet, so there is nothing to choose and nothing was changed."
        ));
        assert_eq!(answer.structured_content.as_ref().unwrap()["open"], false);
    }
    assert!(writes(&stub).is_empty());
}

/// A client that answers elicitations with `answers` in turn, recording what it was asked.
#[derive(Default)]
struct FormClient {
    asked: Mutex<Vec<Value>>,
    answers: Mutex<Vec<std::result::Result<Value, String>>>,
}

#[async_trait]
impl PeerChannel for FormClient {
    async fn notify(&self, _method: &str, _params: Value) {}

    async fn request(
        &self,
        method: &str,
        params: Value,
        _timeout: Duration,
    ) -> std::result::Result<Value, String> {
        assert_eq!(method, "elicitation/create");
        self.asked.lock().unwrap().push(params);
        let mut answers = self.answers.lock().unwrap();
        if answers.is_empty() {
            Err("no answer".to_string())
        } else {
            answers.remove(0)
        }
    }
}

fn form_client(answers: Vec<std::result::Result<Value, String>>) -> Arc<FormClient> {
    Arc::new(FormClient {
        asked: Mutex::new(Vec::new()),
        answers: Mutex::new(answers),
    })
}

#[tokio::test]
async fn choose_asks_the_person_in_a_form_and_sets_what_they_ticked() {
    let stub = settings_stub(settings_catalog(), settings_view(), OWNER_WITH_VIEW).await;
    let (tool, _) = stub.tool(stub.config("key-form"));

    // A 2025-11-25 client: one multi-select, everything on checked; keeper unticked.
    let keep: Vec<String> = Catalog::from_value(&settings_view())
        .agents
        .iter()
        .map(|a| a.id.clone())
        .filter(|id| id != "keeper")
        .collect();
    let client = form_client(vec![Ok(
        json!({"action": "accept", "content": {"enabled": keep}}),
    )]);
    let peer = CallPeer::new(client.clone(), None, true).with_client_protocol(Some("2025-11-25"));
    let answer = run_with_call_peer(peer, tool.execute(json!({"action": "choose"})))
        .await
        .unwrap();
    assert!(text_of(&answer)
        .starts_with("ContextAgents in Demos: 14 of 15 on (was 15).\nTurned off: keeper"));
    let asked = client.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 1);
    assert!(asked[0]["message"]
        .as_str()
        .unwrap()
        .starts_with("Choose the ContextAgents on in Demos. 15 of 15 are on now."));
    let field = &asked[0]["requestedSchema"]["properties"]["enabled"];
    assert_eq!(field["type"], "array");
    assert_eq!(field["default"].as_array().unwrap().len(), 15);
    // The form's answer is the exact set.
    assert_eq!(writes(&stub)[0].1["enable"], json!(keep));

    // A 2025-06-18 client: one checkbox per agent; keeper ticked again.
    let client = form_client(vec![Ok(
        json!({"action": "accept", "content": {"keeper": true}}),
    )]);
    let peer = CallPeer::new(client.clone(), None, true).with_client_protocol(Some("2025-06-18"));
    let answer = run_with_call_peer(peer, tool.execute(json!({"action": "choose"})))
        .await
        .unwrap();
    assert!(text_of(&answer).contains("Turned on: keeper (Plans & Tasks Keeper)."));
    let asked = client.asked.lock().unwrap().clone();
    let boxes = &asked[0]["requestedSchema"]["properties"];
    assert_eq!(boxes["mail"]["type"], "boolean");
    assert_eq!(boxes["keeper"]["default"], false);
    assert_eq!(writes(&stub).len(), 2);

    // A client that refuses the multi-select at once is asked again with checkboxes.
    let client = form_client(vec![
        Err("the client answered with an error: unsupported schema".to_string()),
        Ok(json!({"action": "decline"})),
    ]);
    let peer = CallPeer::new(client.clone(), None, true).with_client_protocol(Some("2025-11-25"));
    let answer = run_with_call_peer(peer, tool.execute(json!({"action": "choose"})))
        .await
        .unwrap();
    assert_eq!(
        text_of(&answer),
        "Nothing changed: the person closed the form. 15 of 15 ContextAgents stay on in Demos."
    );
    let asked = client.asked.lock().unwrap().clone();
    assert_eq!(asked.len(), 2);
    assert_eq!(
        asked[1]["requestedSchema"]["properties"]["mail"]["type"],
        "boolean"
    );

    // No answer at all: the checklist and the call to make.
    let peer =
        CallPeer::new(form_client(vec![]), None, true).with_client_protocol(Some("2025-11-25"));
    let answer = run_with_call_peer(peer, tool.execute(json!({"action": "choose"})))
        .await
        .unwrap();
    assert!(text_of(&answer)
        .contains("The form could not be shown here: show the person this checklist"));
    assert_eq!(writes(&stub).len(), 2);
}

#[tokio::test]
async fn choose_without_a_form_gives_the_checklist_and_with_agents_sets_exactly_them() {
    let stub = settings_stub(settings_catalog(), settings_view(), OWNER_WITHOUT_VIEW).await;
    let (tool, _) = stub.tool(stub.config("key-text"));
    // No peer (the hosted gateway), or a client without elicitation.
    for peer in [None, Some(CallPeer::new(form_client(vec![]), None, false))] {
        let call = tool.execute(json!({"action": "choose"}));
        let answer = match peer {
            Some(peer) => run_with_call_peer(peer, call).await,
            None => call.await,
        }
        .unwrap();
        let text = text_of(&answer);
        assert!(text.starts_with("ContextAgents in Demos: 14 of 15 on."));
        assert!(text.ends_with("This client can't show a form: show the person this checklist and ask which to keep on, then call agents(action=\"choose\", agents=[\"<id>\", ...]) with exactly those (or agents=\"all\" / \"none\"). Only what changes is sent."));
        assert_eq!(answer.structured_content.as_ref().unwrap()["form"], false);
    }
    assert!(writes(&stub).is_empty());

    let exact = tool
        .execute(json!({"action": "choose", "agents": ["oracle", "review", "mail"]}))
        .await
        .unwrap();
    let text = text_of(&exact);
    assert!(text.starts_with("ContextAgents in Demos: 3 of 15 on (was 14)."));
    assert!(text.contains("Turned on: mail (Mail)."));
    assert_eq!(writes(&stub).len(), 1 + 12);
}

#[tokio::test]
async fn a_stateless_client_answers_the_form_in_the_next_call() {
    let stub = settings_stub(settings_catalog(), settings_view(), OWNER_WITH_VIEW).await;
    let (tool, _) = stub.tool(stub.config("key-stateless"));
    let first = CallPeer::new(Arc::new(NoLiveChannel), None, false).with_inline_input(&json!({}));
    let asking = run_with_call_peer(first.clone(), tool.execute(json!({"action": "choose"})))
        .await
        .unwrap();
    assert_eq!(
        text_of(&asking),
        "Asking the person which ContextAgents to keep on in Demos."
    );
    let question = first.take_input_required().expect("the question");
    assert_eq!(question["resultType"], "input_required");
    let request = &question["inputRequests"]["agents"];
    assert_eq!(request["method"], "elicitation/create");
    assert_eq!(
        request["params"]["requestedSchema"]["properties"]["enabled"]["type"], "array",
        "a stateless (2026-07-28) client has multi-select"
    );
    let state = question["requestState"].as_str().unwrap().to_string();
    assert!(writes(&stub).is_empty());

    let answered = CallPeer::new(Arc::new(NoLiveChannel), None, false).with_inline_input(&json!({
        "inputResponses": {"agents": {"action": "accept", "content": {"enabled": ["oracle"]}}},
        "requestState": state,
    }));
    let done = run_with_call_peer(answered, tool.execute(json!({"action": "choose"})))
        .await
        .unwrap();
    assert!(text_of(&done).starts_with("ContextAgents in Demos: 1 of 15 on (was 15)."));
    assert_eq!(
        writes(&stub),
        vec![(
            settings_path_of(),
            json!({"enable": ["oracle"], "accept_new_access": false})
        )]
    );
}

#[tokio::test]
async fn with_no_agent_on_the_tool_is_listed_with_how_to_turn_them_on() {
    let mut value = settings_catalog();
    for entry in value["data"]["agents"].as_array_mut().unwrap() {
        entry["enabled"] = json!(false);
    }
    let stub = Stub::api(value["data"].clone(), |_| Answer::error(404, "no")).await;
    let listed = listing_for(&stub.url, "key-none-on", Some(workspace())).await;
    assert!(listed.is_listed());
    let description = listed.description().unwrap();
    assert!(description.starts_with("ContextAgents: specialist agents"));
    assert!(description.contains("None is on in this workspace."));
    assert!(description.contains("choose") && description.contains("enable"));
    assert!(description.chars().count() <= DESCRIPTION_BUDGET);
    let schema = listed.input_schema().unwrap();
    let agent_param = schema["properties"]["agent"]["description"]
        .as_str()
        .unwrap();
    assert!(agent_param.contains("oracle") && agent_param.contains("mail"));
    assert_eq!(schema["properties"]["agents"]["items"]["type"], "string");
}
