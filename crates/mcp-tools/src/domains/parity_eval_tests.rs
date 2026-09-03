//! Wave 3b / 4b parity eval corpus.
//!
//! Each case names the prompt an agent would send, the tool + action that
//! must answer it, and the exact text markers the tool must emit. The cases
//! run against a routing mock of the hosted API so both the typed contract
//! and the 404 fallbacks (`[PARTIAL]`) are exercised without network access.

use super::memory::MemoryTool;
use super::session::{
    ContextTool, SessionCaptureLessonTool, SessionDecisionTraceTool, SessionGetLessonsTool,
    SessionTool,
};
use crate::registry::ToolHandler;
use crate::testing::TestFixtures;
use mcp_client::ContextStreamClient;
use mcp_session::SessionManager;
use mcp_types::tool::{ContentItem, ToolResult};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

/// One eval case: what the agent asks, which tool/action answers, and the
/// markers the rendered text must contain.
struct EvalCase {
    id: &'static str,
    prompt: &'static str,
    expected_tool: &'static str,
    expected_action: &'static str,
    expected_markers: &'static [&'static str],
}

const PARITY_EVAL_CORPUS: &[EvalCase] = &[
    EvalCase {
        id: "decisions-latest-5",
        prompt: "show me the latest 5 decisions",
        expected_tool: "memory",
        expected_action: "decisions",
        expected_markers: &["[DECISIONS] 2 of 2 decision(s) (sort=recency, status=active)", "[DECISION]", "status=active", "freshness=fresh"],
    },
    EvalCase {
        id: "decisions-about-x",
        prompt: "what did we decide about the ledger database",
        expected_tool: "memory",
        expected_action: "decisions",
        expected_markers: &["[DECISION]", "status=unknown", "[PARTIAL] decisions_envelope:"],
    },
    EvalCase {
        id: "decision-why-trace",
        prompt: "why did we choose postgres for the ledger",
        expected_tool: "session",
        expected_action: "decision_trace",
        expected_markers: &["[DECISION_TRACE] Postgres was chosen for transactional guarantees.", "status=verified"],
    },
    EvalCase {
        id: "supersede-by-text",
        prompt: "supersede the ledger database node with the new content",
        expected_tool: "memory",
        expected_action: "supersede_node",
        expected_markers: &["Resolved node \"ledger database\"", "Node superseded:"],
    },
    EvalCase {
        id: "capture-lesson",
        prompt: "remember this lesson: always quote shell paths",
        expected_tool: "session",
        expected_action: "capture_lesson",
        expected_markers: &["Lesson captured: Quote shell paths (ID: "],
    },
    EvalCase {
        id: "high-severity-lessons",
        prompt: "list high severity lessons",
        expected_tool: "session",
        expected_action: "get_lessons",
        expected_markers: &["Found 1 of 1 lesson(s).", "[HIGH] Quote shell paths id=", "status=active"],
    },
    EvalCase {
        id: "ground-lessons-warning",
        prompt: "refactor the ledger service",
        expected_tool: "session",
        expected_action: "ground",
        expected_markers: &["[LESSONS_WARNING] severity=high relevance=0.90 Quote shell paths", "[COORDINATION] Ledger schema freeze"],
    },
    EvalCase {
        id: "context-coordination",
        prompt: "continue the ledger refactor",
        expected_tool: "context",
        expected_action: "",
        expected_markers: &["[COORDINATION] [other project] Schema freeze (urgency=high) — ack via coordination(action=\"ack\", notice_id=\"n-other\")", "[COORDINATION] Same-project note — ack via"],
    },
];

fn case(id: &str) -> &'static EvalCase {
    PARITY_EVAL_CORPUS
        .iter()
        .find(|case| case.id == id)
        .unwrap_or_else(|| panic!("eval case {id} is not in the corpus"))
}

fn assert_case(id: &str, text: &str) {
    let case = case(id);
    for marker in case.expected_markers {
        assert!(
            text.contains(marker),
            "case {id} ({}) expected marker {marker:?} in:\n{text}",
            case.prompt
        );
    }
}

// ---------------------------------------------------------------------------
// Routing mock of the hosted API
// ---------------------------------------------------------------------------

struct Route {
    method: &'static str,
    path_contains: String,
    status: u16,
    body: String,
}

fn route(method: &'static str, path_contains: &str, status: u16, body: Value) -> Route {
    Route {
        method,
        path_contains: path_contains.to_string(),
        status,
        body: body.to_string(),
    }
}

struct MockApi {
    base_url: String,
    requests: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MockApi {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl MockApi {
    async fn start(routes: Vec<Route>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock api");
        let base_url = format!("http://{}", listener.local_addr().expect("mock addr"));
        let requests: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let log = requests.clone();
        let routes = Arc::new(routes);
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let routes = routes.clone();
                let log = log.clone();
                tokio::spawn(async move {
                    let mut buffer = vec![0_u8; 64 * 1024];
                    let count = socket.read(&mut buffer).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buffer[..count]).to_string();
                    let request_line = request.lines().next().unwrap_or_default().to_string();
                    log.lock().unwrap().push(request_line.clone());
                    let (status, body) = routes
                        .iter()
                        .find(|route| {
                            request_line.starts_with(&format!("{} ", route.method))
                                && request_line.contains(&route.path_contains)
                        })
                        .map(|route| (route.status, route.body.clone()))
                        .unwrap_or((404, json!({"error": "not found"}).to_string()));
                    let reason = match status {
                        200 => "OK",
                        201 => "Created",
                        404 => "Not Found",
                        _ => "OK",
                    };
                    let response = format!(
                        "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Self {
            base_url,
            requests,
            task,
        }
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }

    fn saw(&self, needle: &str) -> bool {
        self.requests().iter().any(|line| line.contains(needle))
    }

    async fn wait_for(&self, needle: &str) -> bool {
        for _ in 0..40 {
            if self.saw(needle) {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        false
    }
}

fn client_and_session(
    base_url: &str,
    workspace_id: Uuid,
    project_id: Option<Uuid>,
) -> (ContextStreamClient, Arc<SessionManager>) {
    let mut config = TestFixtures::test_config();
    config.api_url = base_url.to_string();
    config.default_workspace_id = Some(workspace_id);
    config.default_project_id = project_id;
    let client = ContextStreamClient::new(config.clone());
    let session = Arc::new(SessionManager::new(client.clone(), config));
    (client, session)
}

fn text_of(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .find_map(|item| match item {
            ContentItem::Text { text } => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn scope_routes(workspace_id: Uuid, project_id: Option<Uuid>) -> Vec<Route> {
    let mut routes = vec![route(
        "GET",
        &format!("/api/v1/workspaces/{workspace_id}"),
        200,
        json!({"id": workspace_id, "name": "Parity", "slug": "parity"}),
    )];
    if let Some(project_id) = project_id {
        routes.push(route(
            "GET",
            &format!("/api/v1/projects/{project_id}"),
            200,
            json!({"id": project_id, "workspace_id": workspace_id, "name": "ledger", "slug": "ledger"}),
        ));
    }
    routes
}

const D1: &str = "11111111-1111-4111-8111-111111111111";
const D2: &str = "22222222-2222-4222-8222-222222222222";

// ---------------------------------------------------------------------------
// Cases
// ---------------------------------------------------------------------------

#[test]
fn corpus_ids_are_unique_and_name_real_tools() {
    let mut ids = HashSet::new();
    for case in PARITY_EVAL_CORPUS {
        assert!(ids.insert(case.id), "duplicate eval case id {}", case.id);
        assert!(
            matches!(
                case.expected_tool,
                "memory" | "session" | "context" | "init"
            ),
            "unknown tool {}",
            case.expected_tool
        );
        assert!(!case.expected_markers.is_empty());
        // `context` is a plain tool; every other case names a real action.
        assert_eq!(
            case.expected_action.is_empty(),
            case.expected_tool == "context",
            "case {} action/tool mismatch",
            case.id
        );
    }
    assert_eq!(PARITY_EVAL_CORPUS.len(), 8);
}

#[tokio::test]
async fn decisions_latest_5_requests_envelope_and_renders_typed_lines() {
    let ws = Uuid::new_v4();
    let mut routes = scope_routes(ws, None);
    routes.push(route(
        "GET",
        "/api/v1/memory/decisions?",
        200,
        json!({
            "items": [
                {"id": D1, "title": "Use Postgres for the ledger", "status": "active", "freshness": "fresh",
                 "category": "architecture", "created_at": "2026-09-01T10:00:00Z",
                 "structured": {"rationale": "Transactional guarantees"}},
                {"id": D2, "title": "Batch writes nightly", "status": "active", "freshness": "fresh",
                 "category": "operations", "created_at": "2026-08-30T10:00:00Z"}
            ],
            "total": 2, "next_offset": null, "sort": "recency", "scope": {"workspace_id": ws},
            "degraded": [], "schema_version": "decisions.v1"
        }),
    ));
    let api = MockApi::start(routes).await;
    let (client, session) = client_and_session(&api.base_url, ws, None);
    session.initialize(Some(ws), None, None, None).await;
    let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());
    let result = tool
        .execute(json!({"action": "decisions", "sort": "recency", "limit": 5, "workspace_id": ws}))
        .await
        .expect("decisions");
    let text = text_of(&result);
    assert_case("decisions-latest-5", &text);
    assert!(text.contains(&format!("1. [DECISION] Use Postgres for the ledger — status=active freshness=fresh category=architecture id={D1}")));
    assert!(text.contains("rationale: Transactional guarantees"));
    assert!(!text.contains("[PARTIAL]"));
    let request = api
        .requests()
        .into_iter()
        .find(|line| line.contains("/api/v1/memory/decisions?"))
        .expect("decisions request");
    for needle in ["sort=recency", "limit=5", "format=envelope"] {
        assert!(request.contains(needle), "{request}");
    }
    let structured = result.structured_content.as_ref().expect("structured");
    assert_eq!(structured["schema_version"], "decisions.v1");
}

#[tokio::test]
async fn decisions_about_topic_falls_back_to_legacy_array_with_partial() {
    let ws = Uuid::new_v4();
    let mut routes = scope_routes(ws, None);
    routes.push(route(
        "GET",
        "/api/v1/memory/decisions?",
        200,
        json!([{"id": D1, "summary": "Use Postgres for the ledger", "details": "chosen over Redis"}]),
    ));
    let api = MockApi::start(routes).await;
    let (client, session) = client_and_session(&api.base_url, ws, None);
    session.initialize(Some(ws), None, None, None).await;
    let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());
    let result = tool
        .execute(json!({"action": "decisions", "query": "ledger database", "status": "all", "workspace_id": ws}))
        .await
        .expect("decisions");
    let text = text_of(&result);
    assert_case("decisions-about-x", &text);
    assert!(text.contains("freshness=unknown"));
    assert!(api.saw("query=ledger%20database"));
    assert!(api.saw("status=all"));
    let err = tool
        .execute(json!({"action": "decisions", "status": "bogus"}))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("Invalid status"));
}

#[tokio::test]
async fn decision_trace_renders_answer_from_typed_trace() {
    let ws = Uuid::new_v4();
    let mut routes = scope_routes(ws, None);
    routes.push(route(
        "POST",
        "/api/v1/memory/search/decisions/trace",
        200,
        json!({"decisions": [{"id": D1, "title": "Use Postgres for the ledger", "status": "verified", "created_at": "2026-09-01"}]}),
    ));
    routes.push(route(
        "GET",
        &format!("/api/v1/memory/decisions/{D1}/trace"),
        200,
        json!({"answer": "Postgres was chosen for transactional guarantees.", "markers": ["[DECISION_TRACE]"], "decision": {"id": D1}}),
    ));
    let api = MockApi::start(routes).await;
    let (client, _session) = client_and_session(&api.base_url, ws, None);
    let tool = SessionDecisionTraceTool::new(client);
    let result = tool
        .execute(json!({"query": "why postgres for the ledger", "workspace_id": ws}))
        .await
        .expect("trace");
    let text = text_of(&result);
    assert_case("decision-why-trace", &text);
    assert!(text.starts_with("[DECISION_TRACE] Postgres was chosen"));
    assert!(text.contains(&format!(
        "1. Use Postgres for the ledger — status=verified (2026-09-01) id={D1}"
    )));
    assert!(text.contains("memory(action=\"decision_action\""));
    let structured = result.structured_content.as_ref().expect("structured");
    assert_eq!(structured["markers"][0], "[DECISION_TRACE]");

    // A UUID query goes straight to the typed trace.
    let result = tool
        .execute(json!({"query": D1, "workspace_id": ws}))
        .await
        .expect("trace by id");
    assert!(text_of(&result).starts_with("[DECISION_TRACE] Postgres was chosen"));
}

#[tokio::test]
async fn decision_trace_without_typed_endpoint_says_no_answer() {
    let ws = Uuid::new_v4();
    let mut routes = scope_routes(ws, None);
    routes.push(route(
        "POST",
        "/api/v1/memory/search/decisions/trace",
        200,
        json!({"decisions": [{"id": D1, "title": "Use Postgres for the ledger"}]}),
    ));
    let api = MockApi::start(routes).await;
    let (client, _session) = client_and_session(&api.base_url, ws, None);
    let text = text_of(
        &SessionDecisionTraceTool::new(client)
            .execute(json!({"query": "postgres ledger", "workspace_id": ws}))
            .await
            .expect("trace"),
    );
    assert!(text.starts_with("[DECISION_TRACE] No synthesized answer from the server"));
    assert!(text.contains("status=unknown"));
}

#[tokio::test]
async fn supersede_node_resolves_lookup_text_and_lists_candidates_when_ambiguous() {
    let ws = Uuid::new_v4();
    let node_id = Uuid::new_v4();
    let new_id = Uuid::new_v4();
    let mut routes = scope_routes(ws, None);
    routes.push(route(
        "GET",
        &format!("/api/v1/memory/nodes/workspace/{ws}"),
        200,
        json!({"items": [
            {"id": node_id, "title": "Ledger database choice", "node_type": "Decision"},
            {"id": Uuid::new_v4(), "title": "Unrelated caching note", "node_type": "Note"}
        ]}),
    ));
    routes.push(route(
        "GET",
        &format!("/api/v1/memory/nodes/{node_id}"),
        200,
        json!({"id": node_id, "workspace_id": ws, "node_type": "Decision", "summary": "Ledger database choice"}),
    ));
    routes.push(route(
        "POST",
        "/api/v1/memory/nodes",
        200,
        json!({"id": new_id}),
    ));
    let api = MockApi::start(routes).await;
    let (client, session) = client_and_session(&api.base_url, ws, None);
    session.initialize(Some(ws), None, None, None).await;
    let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());
    let result = tool
        .execute(json!({"action": "supersede_node", "node_id": "ledger database", "new_content": "Use Postgres 16", "workspace_id": ws}))
        .await
        .expect("supersede");
    let text = text_of(&result);
    assert_case("supersede-by-text", &text);
    assert!(text.contains(&format!("Node superseded: {node_id} → {new_id}.")));
    assert!(api.saw(&format!("POST /api/v1/memory/nodes/{node_id}/supersede")));

    // Ambiguous lookup: candidate list, nothing superseded.
    let ambiguous = MockApi::start({
        let mut routes = scope_routes(ws, None);
        routes.push(route(
            "GET",
            &format!("/api/v1/memory/nodes/workspace/{ws}"),
            200,
            json!({"items": [
                {"id": Uuid::new_v4(), "title": "Ledger database choice"},
                {"id": Uuid::new_v4(), "title": "Ledger database rollout"}
            ]}),
        ));
        routes
    })
    .await;
    let (client, session) = client_and_session(&ambiguous.base_url, ws, None);
    session.initialize(Some(ws), None, None, None).await;
    let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());
    let result = tool
        .execute(json!({"action": "supersede_node", "node_id": "ledger database", "new_content": "x", "workspace_id": ws}))
        .await
        .expect("candidates");
    let text = text_of(&result);
    assert!(text.starts_with(
        "[CANDIDATES] Multiple nodes match \"ledger database\"; nothing was superseded."
    ));
    assert_eq!(
        result.structured_content.as_ref().unwrap()["candidates"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert!(!ambiguous.saw("/supersede"));
}

#[tokio::test]
async fn capture_lesson_uses_typed_endpoint_then_events_on_404() {
    let ws = Uuid::new_v4();
    let lesson_id = Uuid::new_v4();
    let mut routes = scope_routes(ws, None);
    routes.push(route(
        "POST",
        "/api/v1/lessons",
        201,
        json!({"id": lesson_id, "title": "Quote shell paths"}),
    ));
    let api = MockApi::start(routes).await;
    let (client, session) = client_and_session(&api.base_url, ws, None);
    session.initialize(Some(ws), None, None, None).await;
    let tool = SessionCaptureLessonTool::new(client, session);
    let text = text_of(
        &tool
            .execute(json!({"title": "Quote shell paths", "trigger": "paths with spaces", "impact": "command failed",
                           "prevention": "Always quote paths.", "severity": "high", "workspace_id": ws}))
            .await
            .expect("capture"),
    );
    assert_case("capture-lesson", &text);
    assert!(text.contains(&lesson_id.to_string()));
    assert!(!text.contains("[PARTIAL]"));
    assert!(!api.saw("/api/v1/memory/events"));

    let event_id = Uuid::new_v4();
    let fallback = MockApi::start({
        let mut routes = scope_routes(ws, None);
        routes.push(route(
            "POST",
            "/api/v1/lessons",
            404,
            json!({"error": "not found"}),
        ));
        routes.push(route(
            "POST",
            "/api/v1/memory/events",
            200,
            json!({"id": event_id}),
        ));
        routes
    })
    .await;
    let (client, session) = client_and_session(&fallback.base_url, ws, None);
    session.initialize(Some(ws), None, None, None).await;
    let tool = SessionCaptureLessonTool::new(client, session);
    let result = tool
        .execute(json!({"title": "Quote shell paths (events)", "trigger": "paths with spaces", "impact": "command failed",
                       "prevention": "Always quote paths.", "severity": "high", "workspace_id": ws}))
        .await
        .expect("fallback capture");
    let text = text_of(&result);
    assert!(text.contains(&format!(
        "Lesson captured: Quote shell paths (events) (ID: {event_id})."
    )));
    assert!(text.contains("[PARTIAL] /lessons endpoint unavailable (404); stored the lesson as a memory event via /memory/events."));
    assert_eq!(
        result.structured_content.as_ref().unwrap()["fallback"],
        "memory_events"
    );
    assert!(fallback.saw("POST /api/v1/lessons"));
    assert!(fallback.saw("POST /api/v1/memory/events"));
}

#[tokio::test]
async fn high_severity_lessons_use_typed_listing_with_min_severity() {
    let ws = Uuid::new_v4();
    let mut routes = scope_routes(ws, None);
    routes.push(route(
        "GET",
        "/api/v1/lessons?",
        200,
        json!({"items": [{"id": D1, "title": "Quote shell paths", "severity": "high", "status": "active",
                          "category": "workflow", "prevention": "Always quote paths."}],
               "total": 1, "next_offset": null, "degraded": [], "schema_version": "lessons.v1"}),
    ));
    let api = MockApi::start(routes).await;
    let (client, session) = client_and_session(&api.base_url, ws, None);
    session.initialize(Some(ws), None, None, None).await;
    let tool = SessionGetLessonsTool::new(client, session, mcp_types::atlas_layer::noop_layer());
    let result = tool
        .execute(json!({"severity": "high", "workspace_id": ws}))
        .await
        .expect("lessons");
    let text = text_of(&result);
    assert_case("high-severity-lessons", &text);
    assert!(text.contains("category=workflow"));
    let request = api
        .requests()
        .into_iter()
        .find(|line| line.contains("/api/v1/lessons?"))
        .expect("lessons request");
    assert!(request.contains("min_severity=high"), "{request}");
    assert!(request.contains("format=envelope"), "{request}");
    assert_eq!(
        result.structured_content.as_ref().unwrap()["source"],
        "lessons_api"
    );
}

#[tokio::test]
async fn get_lessons_falls_back_to_events_on_404_and_says_so() {
    let ws = Uuid::new_v4();
    let mut routes = scope_routes(ws, None);
    routes.push(route(
        "GET",
        "/api/v1/lessons?",
        404,
        json!({"error": "not found"}),
    ));
    routes.push(route(
        "GET",
        &format!("/api/v1/memory/events/workspace/{ws}"),
        200,
        json!({"items": [{"id": D1, "type": "lesson", "event_type": "lesson",
                          "title": "Quote shell paths",
                          "metadata": {"original_type": "lesson", "severity": "high"},
                          "content": "## Quote shell paths\n**Severity:** high\n### Prevention\nAlways quote paths."}]}),
    ));
    let api = MockApi::start(routes).await;
    let (client, session) = client_and_session(&api.base_url, ws, None);
    session.initialize(Some(ws), None, None, None).await;
    let tool = SessionGetLessonsTool::new(client, session, mcp_types::atlas_layer::noop_layer());
    let result = tool
        .execute(json!({"workspace_id": ws}))
        .await
        .expect("lessons");
    let text = text_of(&result);
    assert!(text.contains("[HIGH] Quote shell paths"));
    assert!(text.contains(
        "[PARTIAL] /lessons endpoint unavailable (404); listed lessons from memory events."
    ));
    assert_eq!(
        result.structured_content.as_ref().unwrap()["source"],
        "memory_events"
    );
}

#[tokio::test]
async fn ground_emits_lessons_warning_and_coordination_lines() {
    let ws = Uuid::new_v4();
    let mut routes = scope_routes(ws, None);
    routes.push(route(
        "GET",
        "/api/v1/lessons/warnings?",
        200,
        json!({"items": [{"lesson": {"id": D1, "title": "Quote shell paths", "severity": "high", "prevention": "Always quote paths."},
                          "relevance": 0.9, "reason": "matched shell"}],
               "rule": "Apply before shell edits", "degraded": []}),
    ));
    routes.push(route(
        "POST",
        "/api/v1/session/recall",
        200,
        json!({"results": []}),
    ));
    routes.push(route("GET", "/api/v1/memory/decisions?", 200, json!([])));
    routes.push(route(
        "GET",
        "/api/v1/coordination/inbox",
        200,
        json!({"notices": [{"id": "n1", "reason": "Ledger schema freeze"}], "items": []}),
    ));
    let api = MockApi::start(routes).await;
    let (client, session) = client_and_session(&api.base_url, ws, None);
    session.initialize(Some(ws), None, None, None).await;
    let tool = SessionTool::new(client, session, mcp_types::atlas_layer::noop_layer());
    let result = tool
        .execute(json!({"action": "ground", "user_message": "refactor the ledger service", "workspace_id": ws}))
        .await
        .expect("ground");
    let text = text_of(&result);
    assert_case("ground-lessons-warning", &text);
    assert!(text.contains("Always quote paths."));
    assert!(text.contains("notice_id=\"n1\""));
    assert!(!api.saw("/ack"));
    let request = api
        .requests()
        .into_iter()
        .find(|line| line.contains("/api/v1/lessons/warnings?"))
        .expect("warnings request");
    assert!(
        request.contains("user_message=refactor%20the%20ledger%20service"),
        "{request}"
    );
    assert_eq!(
        result.structured_content.as_ref().unwrap()["lessons"]["source"],
        "lessons_warnings"
    );
}

#[tokio::test]
async fn ground_lessons_fall_back_to_events_on_404() {
    let ws = Uuid::new_v4();
    let mut routes = scope_routes(ws, None);
    routes.push(route(
        "GET",
        "/api/v1/lessons/warnings?",
        404,
        json!({"error": "not found"}),
    ));
    routes.push(route(
        "POST",
        "/api/v1/memory/search",
        200,
        json!({"results": [{"id": D1, "type": "lesson", "title": "Quote shell paths", "score": 0.7,
                             "content": "## Quote shell paths\n**Severity:** critical\n### Prevention\nAlways quote paths."}]}),
    ));
    routes.push(route(
        "POST",
        "/api/v1/session/recall",
        200,
        json!({"results": []}),
    ));
    let api = MockApi::start(routes).await;
    let (client, session) = client_and_session(&api.base_url, ws, None);
    session.initialize(Some(ws), None, None, None).await;
    let tool = SessionTool::new(client, session, mcp_types::atlas_layer::noop_layer());
    let text = text_of(
        &tool
            .execute(json!({"action": "ground", "user_message": "shell edits", "workspace_id": ws}))
            .await
            .expect("ground"),
    );
    assert!(text.contains(
        "[LESSONS_WARNING] severity=critical relevance=0.70 Quote shell paths: Always quote paths."
    ));
    assert!(text.contains("[PARTIAL] lessons_warnings: GET /lessons/warnings returned 404"));
}

#[tokio::test]
async fn context_emits_coordination_lines_and_checks_in_without_acking() {
    let ws = Uuid::new_v4();
    let project = Uuid::new_v4();
    let other = Uuid::new_v4();
    let mut routes = scope_routes(ws, Some(project));
    routes.push(route(
        "POST",
        "/api/v1/context/smart",
        200,
        json!({"context": "[CTX] W:Parity P:ledger", "summary": "ledger refactor"}),
    ));
    routes.push(route(
        "GET",
        "/api/v1/coordination/inbox",
        200,
        json!({"notices": [
            {"id": "n-other", "reason": "Schema freeze", "from_project_id": other, "urgency": "high"},
            {"id": "n-same", "reason": "Same-project note", "from_project_id": project}
        ], "items": []}),
    ));
    routes.push(route(
        "POST",
        "/api/v1/coordination/check-in",
        200,
        json!({"ok": true}),
    ));
    let api = MockApi::start(routes).await;
    let (client, session) = client_and_session(&api.base_url, ws, Some(project));
    session
        .initialize(Some(ws), Some(project), None, None)
        .await;
    let index_keeper = Arc::new(super::index_keeper::IndexKeeper::new(
        client.clone(),
        session.clone(),
        mcp_types::atlas_layer::noop_layer(),
        mcp_types::acceleration_layer::noop_acceleration_layer(),
    ));
    let tool = ContextTool::new(
        client,
        session,
        index_keeper,
        mcp_types::atlas_layer::noop_layer(),
    );
    let result = tool
        .execute(
            json!({"user_message": "continue the ledger refactor", "mode": "standard",
                       "session_id": "parity-session", "workspace_id": ws, "project_id": project}),
        )
        .await
        .expect("context");
    let text = text_of(&result);
    assert_case("context-coordination", &text);
    assert!(!text.contains("[COORDINATION] [other project] Same-project note"));
    assert!(api.wait_for("POST /api/v1/coordination/check-in").await);
    assert!(!api.saw("/ack"));
    assert!(!api.saw("/dismiss"));
    let structured = result.structured_content.as_ref().expect("structured");
    assert_eq!(
        structured["coordination_inbox"]["notices"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn context_fast_route_skips_coordination_inbox() {
    let ws = Uuid::new_v4();
    let mut routes = scope_routes(ws, None);
    routes.push(route(
        "POST",
        "/api/v1/context/hook",
        200,
        json!({"context": "[CTX] fast"}),
    ));
    routes.push(route(
        "GET",
        "/api/v1/coordination/inbox",
        200,
        json!({"notices": [{"id": "n1", "reason": "should not render"}]}),
    ));
    let api = MockApi::start(routes).await;
    let (client, session) = client_and_session(&api.base_url, ws, None);
    session.initialize(Some(ws), None, None, None).await;
    let index_keeper = Arc::new(super::index_keeper::IndexKeeper::new(
        client.clone(),
        session.clone(),
        mcp_types::atlas_layer::noop_layer(),
        mcp_types::acceleration_layer::noop_acceleration_layer(),
    ));
    let tool = ContextTool::new(
        client,
        session,
        index_keeper,
        mcp_types::atlas_layer::noop_layer(),
    );
    let result = tool
        .execute(json!({"user_message": "quick lookup", "mode": "fast", "workspace_id": ws}))
        .await
        .expect("fast context");
    assert!(!text_of(&result).contains("[COORDINATION]"));
    assert!(!api.saw("/api/v1/coordination/inbox"));
}

#[tokio::test]
async fn capture_decision_with_structured_fields_routes_to_typed_create() {
    let ws = Uuid::new_v4();
    let mut routes = scope_routes(ws, None);
    routes.push(route(
        "POST",
        "/api/v1/memory/decisions",
        201,
        json!({"id": D1, "node_id": D2, "event_id": D1, "deduplicated": false}),
    ));
    let api = MockApi::start(routes).await;
    let (client, session) = client_and_session(&api.base_url, ws, None);
    session.initialize(Some(ws), None, None, None).await;
    let tool = SessionTool::new(client, session, mcp_types::atlas_layer::noop_layer());
    let text = text_of(
        &tool
            .execute(json!({"action": "capture", "event_type": "decision", "title": "Use Postgres for the ledger",
                           "content": "Postgres over Redis", "rationale": "transactions",
                           "alternatives": ["Redis"], "confidence": 0.8, "workspace_id": ws}))
            .await
            .expect("capture decision"),
    );
    assert!(text.contains(&format!(
        "Decision recorded: Use Postgres for the ledger (id: {D1}, node_id: {D2}, event_id: {D1})."
    )));
    assert!(api.saw("POST /api/v1/memory/decisions"));
    assert!(!api.saw("POST /api/v1/memory/events"));

    // Without the typed endpoint the decision is stored as an event and says so.
    let fallback = MockApi::start({
        let mut routes = scope_routes(ws, None);
        routes.push(route(
            "POST",
            "/api/v1/memory/decisions",
            404,
            json!({"error": "not found"}),
        ));
        routes.push(route(
            "POST",
            "/api/v1/memory/events",
            200,
            json!({"id": D2}),
        ));
        routes
    })
    .await;
    let (client, session) = client_and_session(&fallback.base_url, ws, None);
    session.initialize(Some(ws), None, None, None).await;
    let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());
    let result = tool
        .execute(json!({"action": "create_decision", "title": "Use Postgres for the ledger", "rationale": "transactions", "workspace_id": ws}))
        .await
        .expect("fallback create");
    let text = text_of(&result);
    assert!(text.contains(&format!(
        "Decision recorded as a memory event: Use Postgres for the ledger (ID: {D2})."
    )));
    assert!(text
        .contains("[PARTIAL] typed decision endpoint unavailable (404 on POST /memory/decisions)"));
    assert_eq!(
        result.structured_content.as_ref().unwrap()["fallback"],
        "memory_events"
    );
}

#[tokio::test]
async fn decision_action_resolves_lookup_text_and_reports_applied() {
    let ws = Uuid::new_v4();
    let mut routes = scope_routes(ws, None);
    routes.push(route(
        "GET",
        "/api/v1/memory/decisions?",
        200,
        json!({"items": [{"id": D1, "title": "Use Postgres for the ledger", "status": "active"}],
               "total": 1, "degraded": [], "schema_version": "decisions.v1"}),
    ));
    routes.push(route(
        "POST",
        &format!("/api/v1/memory/decisions/{D1}/actions"),
        200,
        json!({"applied": true, "decision": {"id": D1, "title": "Use Postgres for the ledger", "status": "verified"}, "degraded": []}),
    ));
    let api = MockApi::start(routes).await;
    let (client, session) = client_and_session(&api.base_url, ws, None);
    session.initialize(Some(ws), None, None, None).await;
    let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());
    let text = text_of(
        &tool
            .execute(json!({"action": "decision_action", "decision_id": "postgres ledger", "decision_action": "verify", "workspace_id": ws}))
            .await
            .expect("verify"),
    );
    assert!(text.contains(&format!("[DECISION_ACTION] verify applied=true — Use Postgres for the ledger (id={D1}) status=verified")));
    let err = tool
        .execute(
            json!({"action": "decision_action", "decision_id": D1, "decision_action": "explode"}),
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("Invalid decision_action"));
}

#[tokio::test]
async fn decision_action_without_typed_endpoint_is_honest() {
    let ws = Uuid::new_v4();
    let mut routes = scope_routes(ws, None);
    routes.push(route(
        "POST",
        &format!("/api/v1/memory/decisions/{D1}/actions"),
        404,
        json!({"error": "not found"}),
    ));
    routes.push(route(
        "POST",
        &format!("/api/v1/memory/nodes/{D1}/supersede"),
        200,
        json!({"ok": true}),
    ));
    let api = MockApi::start(routes).await;
    let (client, session) = client_and_session(&api.base_url, ws, None);
    session.initialize(Some(ws), None, None, None).await;
    let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());
    let err = tool
        .execute(json!({"action": "decision_action", "decision_id": D1, "decision_action": "verify", "workspace_id": ws}))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains(
        "does not expose (404). No fallback exists for this action; nothing was changed."
    ));
    let text = text_of(
        &tool
            .execute(json!({"action": "decision_action", "decision_id": D1, "decision_action": "supersede", "successor_id": D2, "workspace_id": ws}))
            .await
            .expect("supersede fallback"),
    );
    assert!(text.contains(&format!(
        "[DECISION_ACTION] supersede applied via /memory/nodes/{D1}/supersede — {D1} → {D2}"
    )));
    assert!(text.contains("[PARTIAL] typed decision actions unavailable"));
    assert!(api.saw(&format!("POST /api/v1/memory/nodes/{D1}/supersede")));
}

#[tokio::test]
async fn suggested_rules_render_typed_lines_with_sources_and_native_guidance() {
    let ws = Uuid::new_v4();
    let rule_id = Uuid::new_v4();
    let mut routes = scope_routes(ws, None);
    routes.push(route(
        "GET",
        "/api/v1/suggested-rules?",
        200,
        json!({"rules": [{"id": rule_id, "instruction": "Quote shell paths", "category": "workflow", "confidence": 0.92,
                          "occurrence_count": 4, "source_lesson_ids": [D1, D2]}],
               "native_guidance": {"heading": "Shell safety", "agents_md_snippet": "- Always quote shell paths"}}),
    ));
    routes.push(route(
        "POST",
        &format!("/api/v1/suggested-rules/{rule_id}/action"),
        200,
        json!({"success": true, "status": "accepted"}),
    ));
    routes.push(route(
        "GET",
        "/api/v1/suggested-rules/stats",
        200,
        json!({"total_suggested": 3, "accepted": 1, "rejected": 1, "pending": 1}),
    ));
    let api = MockApi::start(routes).await;
    let (client, session) = client_and_session(&api.base_url, ws, None);
    session.initialize(Some(ws), None, None, None).await;
    let tool = SessionTool::new(client, session, mcp_types::atlas_layer::noop_layer());
    let text = text_of(
        &tool
            .execute(json!({"action": "list_suggested_rules", "workspace_id": ws}))
            .await
            .expect("list"),
    );
    assert!(text.starts_with(crate::notices::SUGGESTED_RULES_HEADER));
    assert!(text.contains(&format!("[SUGGESTED_RULES] [workflow] Quote shell paths (confidence: 92%, seen 4x) id={rule_id} source_lesson_ids={D1},{D2}")));
    assert!(text.contains("[SUGGESTED_RULES] native_guidance heading=\"Shell safety\" — paste into the rules file:\n- Always quote shell paths"));
    assert!(api.saw("format=envelope"));
    let text = text_of(
        &tool
            .execute(json!({"action": "suggested_rule_action", "rule_id": rule_id, "rule_action": "accept"}))
            .await
            .expect("action"),
    );
    assert_eq!(
        text,
        format!("[SUGGESTED_RULES] action=accept rule_id={rule_id} success=true status=accepted")
    );
    let text = text_of(
        &tool
            .execute(json!({"action": "suggested_rules_stats", "workspace_id": ws}))
            .await
            .expect("stats"),
    );
    assert_eq!(
        text,
        "[SUGGESTED_RULES] stats total=3 accepted=1 rejected=1 pending=1"
    );
}
