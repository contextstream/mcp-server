//! Every tool that takes "an id or a title" resolves it by the rule in
//! [`super::lookup`]. These tests drive the tools against a mock of the hosted
//! API and check what reaches it: an action that removes or retires a record
//! goes out only for an id or an exact title, and anything else comes back as
//! a candidate list with nothing written.

use super::entity::EntityTool;
use super::memory::MemoryTool;
use super::parity_eval_tests::{client_and_session, route, scope_routes, text_of, MockApi, Route};
use super::session::SessionTool;
use super::skill::SkillTool;
use crate::registry::ToolHandler;
use mcp_client::ContextStreamClient;
use mcp_session::SessionManager;
use mcp_types::tool::ToolResult;
use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;

struct Api {
    mock: MockApi,
    ws: Uuid,
    client: ContextStreamClient,
    session: Arc<SessionManager>,
}

impl Api {
    async fn start(routes: Vec<Route>) -> Self {
        Self::start_with(|_| routes).await
    }

    /// For routes that carry the workspace id in their path.
    async fn start_with(routes: impl FnOnce(Uuid) -> Vec<Route>) -> Self {
        let ws = Uuid::new_v4();
        let mut all = scope_routes(ws, None);
        all.extend(routes(ws));
        let mock = MockApi::start(all).await;
        let (client, session) = client_and_session(&mock.base_url, ws, None);
        session.initialize(Some(ws), None, None, None).await;
        Self {
            mock,
            ws,
            client,
            session,
        }
    }

    fn memory(&self) -> MemoryTool {
        MemoryTool::new(
            self.client.clone(),
            self.session.clone(),
            mcp_types::atlas_layer::noop_layer(),
        )
    }

    fn session_tool(&self) -> SessionTool {
        SessionTool::new(
            self.client.clone(),
            self.session.clone(),
            mcp_types::atlas_layer::noop_layer(),
        )
    }

    fn entity(&self) -> EntityTool {
        EntityTool::new(self.client.clone(), self.session.clone())
    }

    fn skill(&self) -> SkillTool {
        SkillTool::new(self.client.clone(), self.session.clone())
    }

    /// Request lines that would change data: everything but a GET.
    fn writes(&self) -> Vec<String> {
        self.mock
            .requests()
            .into_iter()
            .filter(|line| !line.starts_with("GET "))
            .collect()
    }

    fn saw(&self, needle: &str) -> bool {
        self.mock.saw(needle)
    }
}

fn no_writes() -> Vec<String> {
    Vec::new()
}

/// The call was refused: an error result that says why and lists exactly
/// `expected`, in the text and in the structured result.
fn assert_refused(result: &ToolResult, reason: &str, expected: &[Uuid]) {
    let text = text_of(result);
    assert!(
        result.is_error,
        "a refused action must not read as success: {text}"
    );
    assert!(text.starts_with("[CANDIDATES] "), "{text}");
    let structured = result
        .structured_content
        .as_ref()
        .expect("structured candidates");
    assert_eq!(structured["resolved"], false, "{text}");
    assert_eq!(structured["reason"], reason, "{text}");
    let mut listed: Vec<String> = structured["candidates"]
        .as_array()
        .expect("candidates array")
        .iter()
        .map(|candidate| candidate["id"].as_str().expect("candidate id").to_string())
        .collect();
    let mut wanted: Vec<String> = expected.iter().map(Uuid::to_string).collect();
    listed.sort();
    wanted.sort();
    assert_eq!(listed, wanted, "{text}");
    for id in &wanted {
        assert!(text.contains(id), "{text}");
    }
}

fn items(list: Value) -> Value {
    json!({ "items": list })
}

// ---------------------------------------------------------------------------
// Docs: the reported case
// ---------------------------------------------------------------------------

fn doc_routes(runbook: Uuid) -> Vec<Route> {
    vec![
        route(
            "GET",
            "/api/v1/docs?",
            200,
            items(json!([
                {"id": runbook, "title": "Deploy runbook", "doc_type": "runbook"},
                {"id": Uuid::new_v4(), "title": "Billing export", "doc_type": "general"}
            ])),
        ),
        route(
            "GET",
            "/api/v1/docs/",
            200,
            json!({"id": runbook, "title": "Deploy runbook", "content": "Steps."}),
        ),
        route(
            "PATCH",
            "/api/v1/docs/",
            200,
            json!({"id": runbook, "title": "Deploy runbook"}),
        ),
        route("DELETE", "/api/v1/docs/", 200, json!({"deleted": true})),
    ]
}

#[tokio::test]
async fn delete_doc_by_a_partial_title_deletes_nothing_and_returns_the_candidates() {
    // One doc mentions "deploy"; none is called "deploy notes". This call
    // used to delete "Deploy runbook".
    let runbook = Uuid::new_v4();
    let api = Api::start(doc_routes(runbook)).await;

    let result = api
        .memory()
        .execute(json!({"action": "delete_doc", "doc_id": "deploy notes", "workspace_id": api.ws}))
        .await
        .expect("a candidate answer");

    assert_refused(&result, "not_exact", &[runbook]);
    let text = text_of(&result);
    assert!(text.starts_with(
        "[CANDIDATES] \"deploy notes\" is not the id or the exact title of any doc; nothing was deleted."
    ));
    assert!(text.contains(&format!("1. **Deploy runbook** (id: {runbook})")));
    assert!(text.ends_with("Retry: memory(action=\"delete_doc\", doc_id=\"<id>\")"));
    assert_eq!(api.writes(), no_writes());
}

#[tokio::test]
async fn delete_doc_by_exact_title_or_id_deletes_that_doc() {
    let runbook = Uuid::new_v4();
    for reference in ["deploy RUNBOOK".to_string(), runbook.to_string()] {
        let api = Api::start(doc_routes(runbook)).await;
        let result = api
            .memory()
            .execute(json!({"action": "delete_doc", "doc_id": reference, "workspace_id": api.ws}))
            .await
            .expect("delete");
        assert!(!result.is_error);
        assert_eq!(text_of(&result), "Doc deleted successfully.");
        assert_eq!(
            api.writes(),
            vec![format!("DELETE /api/v1/docs/{runbook} HTTP/1.1")]
        );
    }
}

#[tokio::test]
async fn delete_doc_refuses_each_grade_below_an_exact_title() {
    let runbook = Uuid::new_v4();
    let api = Api::start(doc_routes(runbook)).await;
    // The same words with punctuation, the title wrapped in a request, and a
    // single word of it: none of them is the title.
    for reference in ["deploy-runbook", "the deploy runbook doc", "runbook"] {
        let result = api
            .memory()
            .execute(json!({"action": "delete_doc", "doc_id": reference, "workspace_id": api.ws}))
            .await
            .expect("a candidate answer");
        assert_refused(&result, "not_exact", &[runbook]);
    }
    assert_eq!(api.writes(), no_writes());
}

#[tokio::test]
async fn delete_doc_with_two_docs_of_that_title_returns_both() {
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let api = Api::start(vec![
        route(
            "GET",
            "/api/v1/docs?",
            200,
            items(json!([
                {"id": first, "title": "Deploy runbook", "doc_type": "runbook"},
                {"id": second, "title": "deploy runbook", "doc_type": "general"},
                {"id": Uuid::new_v4(), "title": "Deploy runbook for staging", "doc_type": "runbook"}
            ])),
        ),
        route("DELETE", "/api/v1/docs/", 200, json!({"deleted": true})),
    ])
    .await;

    let result = api
        .memory()
        .execute(
            json!({"action": "delete_doc", "doc_id": "Deploy runbook", "workspace_id": api.ws}),
        )
        .await
        .expect("a candidate answer");

    assert_refused(&result, "duplicate_title", &[first, second]);
    assert!(text_of(&result)
        .starts_with("[CANDIDATES] 2 docs are titled \"Deploy runbook\"; nothing was deleted."));
    assert_eq!(api.writes(), no_writes());
}

#[tokio::test]
async fn update_doc_resolves_one_clear_title_and_says_which_doc_it_took() {
    let runbook = Uuid::new_v4();
    let api = Api::start(doc_routes(runbook)).await;

    // Some words only: nothing is written.
    let result = api
        .memory()
        .execute(json!({"action": "update_doc", "doc_id": "deploy notes", "content": "x", "workspace_id": api.ws}))
        .await
        .expect("a candidate answer");
    assert_refused(&result, "partial_match", &[runbook]);
    assert_eq!(api.writes(), no_writes());

    // The title wrapped in a request: one doc, named in text and structure.
    let result = api
        .memory()
        .execute(json!({"action": "update_doc", "doc_id": "the deploy runbook doc", "content": "x", "workspace_id": api.ws}))
        .await
        .expect("update");
    assert!(!result.is_error);
    assert!(text_of(&result).starts_with(&format!(
        "Resolved \"the deploy runbook doc\" to doc **Deploy runbook** (id: {runbook}).\n\nDoc updated: {runbook}."
    )));
    assert_eq!(
        result.structured_content.as_ref().unwrap()["lookup_resolution"],
        json!({
            "lookup": "the deploy runbook doc",
            "resolved_id": runbook,
            "resolved_title": "Deploy runbook",
            "match": "same_words",
        })
    );
    assert_eq!(
        api.writes(),
        vec![format!("PATCH /api/v1/docs/{runbook} HTTP/1.1")]
    );
}

#[tokio::test]
async fn get_doc_opens_one_clear_title_and_lists_the_matches_otherwise() {
    let runbook = Uuid::new_v4();
    let api = Api::start(doc_routes(runbook)).await;
    let result = api
        .memory()
        .execute(json!({"action": "get_doc", "doc_id": "deploy", "workspace_id": api.ws}))
        .await
        .expect("get_doc");
    assert!(text_of(&result).starts_with(&format!(
        "Resolved doc query \"deploy\" to doc ID {runbook}."
    )));
    assert!(api.saw(&format!("GET /api/v1/docs/{runbook} ")));

    // Two titles hold the word: the match list, in the shape it always had.
    let checklist = Uuid::new_v4();
    let api = Api::start(vec![route(
        "GET",
        "/api/v1/docs?",
        200,
        items(json!([
            {"id": runbook, "title": "Deploy runbook", "doc_type": "runbook"},
            {"id": checklist, "title": "Deploy checklist", "doc_type": "general"}
        ])),
    )])
    .await;
    let result = api
        .memory()
        .execute(json!({"action": "get_doc", "doc_id": "deploy", "workspace_id": api.ws}))
        .await
        .expect("get_doc");
    assert!(!result.is_error);
    assert!(text_of(&result).starts_with("Found 2 docs matching \"deploy\":"));
    let structured = result.structured_content.as_ref().unwrap();
    assert_eq!(structured["query"], "deploy");
    assert_eq!(structured["matches"].as_array().map(Vec::len), Some(2));
    assert!(!api.saw("GET /api/v1/docs/"));
}

// ---------------------------------------------------------------------------
// Nodes and events
// ---------------------------------------------------------------------------

fn node_routes(ws: Uuid, nodes: Value) -> Vec<Route> {
    vec![
        route(
            "GET",
            &format!("/api/v1/memory/nodes/workspace/{ws}"),
            200,
            items(nodes),
        ),
        route(
            "GET",
            "/api/v1/memory/nodes/",
            200,
            json!({"id": Uuid::new_v4(), "title": "Ledger database choice", "node_type": "Decision"}),
        ),
        route(
            "PUT",
            "/api/v1/memory/nodes/",
            200,
            json!({"updated": true}),
        ),
        route(
            "DELETE",
            "/api/v1/memory/nodes/",
            200,
            json!({"deleted": true}),
        ),
    ]
}

#[tokio::test]
async fn delete_node_needs_an_exact_title() {
    let ledger = Uuid::new_v4();
    let nodes = json!([
        {"id": ledger, "title": "Ledger database choice", "node_type": "Decision"},
        {"id": Uuid::new_v4(), "summary": "Unrelated caching note", "node_type": "Note"}
    ]);
    let api = Api::start_with(|ws| node_routes(ws, nodes)).await;

    let result = api
        .memory()
        .execute(
            json!({"action": "delete_node", "node_id": "ledger database", "workspace_id": api.ws}),
        )
        .await
        .expect("a candidate answer");
    assert_refused(&result, "not_exact", &[ledger]);
    assert!(
        text_of(&result).contains("delete_all=true deletes every record whose title is exactly")
    );
    assert_eq!(api.writes(), no_writes());

    let result = api
        .memory()
        .execute(json!({"action": "delete_node", "node_id": "LEDGER database choice", "workspace_id": api.ws}))
        .await
        .expect("delete");
    assert_eq!(text_of(&result), "Node deleted successfully.");
    assert_eq!(
        api.writes(),
        vec![format!("DELETE /api/v1/memory/nodes/{ledger} HTTP/1.1")]
    );
}

#[tokio::test]
async fn delete_all_removes_every_exact_title_and_nothing_else() {
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let related = Uuid::new_v4();
    let nodes = json!([
        {"id": first, "title": "Infrastructure is AWS only"},
        {"id": second, "title": "infrastructure is AWS ONLY"},
        {"id": related, "title": "Infrastructure is AWS only, for now"}
    ]);
    let api = Api::start_with(|ws| node_routes(ws, nodes)).await;

    // Without delete_all two exact titles are a tie.
    let result = api
        .memory()
        .execute(json!({"action": "delete_node", "node_id": "Infrastructure is AWS only", "workspace_id": api.ws}))
        .await
        .expect("a candidate answer");
    assert_refused(&result, "duplicate_title", &[first, second]);
    assert_eq!(api.writes(), no_writes());

    // A bulk delete never widens to a title that merely contains the text.
    let error = api
        .memory()
        .execute(json!({"action": "delete_node", "node_id": "Infrastructure", "delete_all": true, "workspace_id": api.ws}))
        .await
        .expect_err("no exact title");
    assert!(error.to_string().contains("No exact node matches"));
    assert_eq!(api.writes(), no_writes());

    let result = api
        .memory()
        .execute(json!({"action": "delete_node", "node_id": "Infrastructure is AWS only", "delete_all": true, "workspace_id": api.ws}))
        .await
        .expect("bulk delete");
    assert_eq!(result.structured_content.as_ref().unwrap()["deleted"], 2);
    let mut writes = api.writes();
    writes.sort();
    let mut expected = vec![
        format!("DELETE /api/v1/memory/nodes/{first} HTTP/1.1"),
        format!("DELETE /api/v1/memory/nodes/{second} HTTP/1.1"),
    ];
    expected.sort();
    assert_eq!(writes, expected);
}

#[tokio::test]
async fn get_node_and_update_node_take_one_clear_title_and_refuse_a_tie() {
    let choice = Uuid::new_v4();
    let rollout = Uuid::new_v4();
    let nodes = json!([
        {"id": choice, "title": "Ledger database choice"},
        {"id": rollout, "title": "Ledger database rollout"}
    ]);
    let api = Api::start_with(|ws| node_routes(ws, nodes)).await;

    for action in ["get_node", "update_node"] {
        let result = api
            .memory()
            .execute(json!({"action": action, "node_id": "ledger database", "content": "x", "workspace_id": api.ws}))
            .await
            .expect("a candidate answer");
        assert_refused(&result, "ambiguous", &[choice, rollout]);
    }
    assert_eq!(api.writes(), no_writes());

    let result = api
        .memory()
        .execute(json!({"action": "update_node", "node_id": "database rollout", "content": "x", "workspace_id": api.ws}))
        .await
        .expect("update");
    assert!(text_of(&result).starts_with(&format!(
        "Resolved \"database rollout\" to node **Ledger database rollout** (id: {rollout})."
    )));
    assert_eq!(
        result.structured_content.as_ref().unwrap()["lookup_resolution"]["resolved_id"],
        json!(rollout)
    );
    assert_eq!(
        api.writes(),
        vec![format!("PUT /api/v1/memory/nodes/{rollout} HTTP/1.1")]
    );
}

#[tokio::test]
async fn delete_event_needs_an_exact_title_and_never_matches_type_or_content() {
    let retro = Uuid::new_v4();
    let untitled = Uuid::new_v4();
    let api = Api::start_with(|ws| {
        vec![
            route(
                "GET",
                &format!("/api/v1/memory/events/workspace/{ws}"),
                200,
                items(json!([
                    {"id": retro, "title": "Deploy retro", "event_type": "note"},
                    {"id": untitled, "event_type": "decision", "content": "deploy notes"}
                ])),
            ),
            route(
                "DELETE",
                "/api/v1/memory/events/",
                200,
                json!({"deleted": true}),
            ),
        ]
    })
    .await;

    let result = api
        .memory()
        .execute(json!({"action": "delete_event", "event_id": "deploy", "workspace_id": api.ws}))
        .await
        .expect("a candidate answer");
    assert_refused(&result, "not_exact", &[retro]);

    // An event without a title used to answer to its type and its content,
    // so "decision" named it, and delete_all removed every such event.
    for delete_all in [false, true] {
        for reference in ["decision", "deploy notes"] {
            let outcome = api
                .memory()
                .execute(json!({"action": "delete_event", "event_id": reference, "delete_all": delete_all, "workspace_id": api.ws}))
                .await;
            let refused = match outcome {
                Err(_) => true,
                Ok(result) => result.is_error,
            };
            assert!(refused, "{reference:?} with delete_all={delete_all}");
        }
    }
    assert_eq!(api.writes(), no_writes());

    api.memory()
        .execute(
            json!({"action": "delete_event", "event_id": "deploy RETRO", "workspace_id": api.ws}),
        )
        .await
        .expect("delete");
    assert_eq!(
        api.writes(),
        vec![format!("DELETE /api/v1/memory/events/{retro} HTTP/1.1")]
    );
}

// ---------------------------------------------------------------------------
// Tasks, todos, diagrams
// ---------------------------------------------------------------------------

#[tokio::test]
async fn deleting_or_closing_a_task_needs_an_exact_title() {
    let task = Uuid::new_v4();
    let api = Api::start(vec![
        route(
            "GET",
            "/api/v1/tasks?",
            200,
            items(json!([
                {"id": task, "title": "Wire the shared resolver", "status": "pending"},
                {"id": Uuid::new_v4(), "title": "Write release notes", "status": "pending"}
            ])),
        ),
        route(
            "GET",
            "/api/v1/tasks/",
            200,
            json!({"id": task, "title": "Wire the shared resolver", "status": "pending"}),
        ),
        route(
            "PATCH",
            "/api/v1/tasks/",
            200,
            json!({"id": task, "status": "in_progress"}),
        ),
        route("DELETE", "/api/v1/tasks/", 200, json!({"deleted": true})),
    ])
    .await;
    let call = |extra: Value| {
        let mut input = json!({"task_id": "resolver", "workspace_id": api.ws});
        input
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        input
    };

    let result = api
        .memory()
        .execute(call(json!({"action": "delete_task"})))
        .await
        .expect("a candidate answer");
    assert_refused(&result, "not_exact", &[task]);

    for status in ["completed", "cancelled"] {
        let result = api
            .memory()
            .execute(call(
                json!({"action": "update_task", "task_status": status}),
            ))
            .await
            .expect("a candidate answer");
        assert_refused(&result, "not_exact", &[task]);
        assert!(text_of(&result).contains("retires the record"));
    }
    assert_eq!(api.writes(), no_writes());

    // A status that keeps the task open is an ordinary edit.
    let result = api
        .memory()
        .execute(call(
            json!({"action": "update_task", "task_status": "in_progress"}),
        ))
        .await
        .expect("update");
    assert!(text_of(&result).starts_with(&format!(
        "Resolved \"resolver\" to task **Wire the shared resolver** (id: {task})."
    )));
    assert_eq!(
        api.writes(),
        vec![format!("PATCH /api/v1/tasks/{task} HTTP/1.1")]
    );

    // The exact title closes it.
    let result = api
        .memory()
        .execute(json!({"action": "update_task", "task_id": "wire the shared RESOLVER", "task_status": "completed", "workspace_id": api.ws}))
        .await
        .expect("update");
    assert!(!result.is_error);
    assert_eq!(api.writes().len(), 2);
}

#[tokio::test]
async fn deleting_or_completing_a_todo_needs_an_exact_title() {
    let todo = Uuid::new_v4();
    let api = Api::start(vec![
        route(
            "GET",
            "/api/v1/todos?",
            200,
            items(json!([{"id": todo, "title": "Renew the TLS certificate", "status": "pending"}])),
        ),
        route(
            "PATCH",
            "/api/v1/todos/",
            200,
            json!({"id": todo, "status": "completed"}),
        ),
        route(
            "POST",
            "/api/v1/todos/",
            200,
            json!({"id": todo, "status": "completed"}),
        ),
        route("DELETE", "/api/v1/todos/", 200, json!({"deleted": true})),
    ])
    .await;

    for input in [
        json!({"action": "delete_todo"}),
        json!({"action": "complete_todo"}),
        json!({"action": "update_todo", "todo_status": "completed"}),
    ] {
        let mut input = input;
        input["todo_id"] = json!("tls certificate");
        input["workspace_id"] = json!(api.ws);
        let result = api
            .memory()
            .execute(input)
            .await
            .expect("a candidate answer");
        assert_refused(&result, "not_exact", &[todo]);
    }
    assert_eq!(api.writes(), no_writes());

    // update_todo used to look the todo up among todos that already had the
    // new status and priority, so a pending todo could not be completed by
    // title at all.
    api.memory()
        .execute(json!({"action": "update_todo", "todo_id": "renew the TLS certificate", "todo_status": "completed", "todo_priority": "high", "workspace_id": api.ws}))
        .await
        .expect("update");
    let lookups: Vec<String> = api
        .mock
        .requests()
        .into_iter()
        .filter(|line| line.starts_with("GET /api/v1/todos?"))
        .collect();
    let last = lookups.last().expect("the todo was looked up");
    assert!(!last.contains("status="), "{last}");
    assert!(!last.contains("priority="), "{last}");
    assert_eq!(
        api.writes(),
        vec![format!("PATCH /api/v1/todos/{todo} HTTP/1.1")]
    );

    api.memory()
        .execute(json!({"action": "complete_todo", "todo_id": "Renew the TLS certificate", "workspace_id": api.ws}))
        .await
        .expect("complete");
    assert!(api.saw(&format!("POST /api/v1/todos/{todo}/complete ")));
}

#[tokio::test]
async fn delete_diagram_needs_an_exact_title() {
    let diagram = Uuid::new_v4();
    let api = Api::start(vec![
        route(
            "GET",
            "/api/v1/diagrams?",
            200,
            items(json!([{"id": diagram, "title": "Auth handoff sequence", "diagram_type": "sequence"}])),
        ),
        route(
            "DELETE",
            "/api/v1/diagrams/",
            200,
            json!({"deleted": true}),
        ),
    ])
    .await;

    let result = api
        .memory()
        .execute(json!({"action": "delete_diagram", "diagram_id": "auth handoff", "workspace_id": api.ws}))
        .await
        .expect("a candidate answer");
    assert_refused(&result, "not_exact", &[diagram]);
    assert_eq!(api.writes(), no_writes());

    api.memory()
        .execute(json!({"action": "delete_diagram", "diagram_id": "auth handoff sequence", "workspace_id": api.ws}))
        .await
        .expect("delete");
    assert_eq!(
        api.writes(),
        vec![format!("DELETE /api/v1/diagrams/{diagram} HTTP/1.1")]
    );
}

// ---------------------------------------------------------------------------
// Decisions
// ---------------------------------------------------------------------------

fn decision_routes(old: Uuid, new: Uuid) -> Vec<Route> {
    vec![
        route(
            "GET",
            "/api/v1/memory/decisions?",
            200,
            json!({
                "items": [
                    {"id": old, "title": "Use Postgres for the ledger", "status": "active"},
                    {"id": new, "title": "Use Postgres 16 for the ledger", "status": "active"}
                ],
                "total": 2,
                "degraded": [],
                "schema_version": "decisions.v1"
            }),
        ),
        route(
            "POST",
            "/api/v1/memory/decisions",
            200,
            json!({"applied": true, "id": Uuid::new_v4(), "decision": {"id": new, "title": "Use Postgres 16 for the ledger", "status": "verified"}, "degraded": []}),
        ),
    ]
}

#[tokio::test]
async fn retiring_a_decision_needs_exact_references_for_it_and_its_successor() {
    let old = Uuid::new_v4();
    let new = Uuid::new_v4();
    let api = Api::start(decision_routes(old, new)).await;
    let act = |input: Value| {
        let mut input = input;
        input["action"] = json!("decision_action");
        input["workspace_id"] = json!(api.ws);
        input
    };

    // Each action that retires the decision refuses a title that is not exact.
    for action in ["supersede", "invalidate", "choose_successor"] {
        let result = api
            .memory()
            .execute(act(json!({"decision_id": "postgres ledger", "decision_action": action, "successor_id": new})))
            .await
            .expect("a candidate answer");
        assert_refused(&result, "not_exact", &[old, new]);
    }

    // The decision is exact, its successor is not: still nothing happens.
    let result = api
        .memory()
        .execute(act(json!({"decision_id": "use postgres for the ledger", "decision_action": "supersede", "successor_id": "postgres 16"})))
        .await
        .expect("a candidate answer");
    assert_refused(&result, "not_exact", &[old, new]);
    assert!(text_of(&result).contains("successor_id=\"<id>\""));
    assert_eq!(api.writes(), no_writes());

    // Both exact: the action is sent.
    api.memory()
        .execute(act(json!({"decision_id": "use postgres for the ledger", "decision_action": "supersede", "successor_id": "Use Postgres 16 for the ledger"})))
        .await
        .expect("supersede");
    assert_eq!(
        api.writes(),
        vec![format!(
            "POST /api/v1/memory/decisions/{old}/actions HTTP/1.1"
        )]
    );
}

#[tokio::test]
async fn a_lone_partial_match_does_not_invalidate_a_decision() {
    // With nothing to tie with, part of a title used to be enough to act on.
    let decision = Uuid::new_v4();
    let api = Api::start(vec![
        route(
            "GET",
            "/api/v1/memory/decisions?",
            200,
            json!({
                "items": [{"id": decision, "title": "Use Postgres for the ledger", "status": "active"}],
                "total": 1,
                "degraded": [],
                "schema_version": "decisions.v1"
            }),
        ),
        route(
            "POST",
            "/api/v1/memory/decisions",
            200,
            json!({"applied": true, "decision": {"id": decision, "status": "superseded"}, "degraded": []}),
        ),
    ])
    .await;

    let result = api
        .memory()
        .execute(json!({"action": "decision_action", "decision_id": "postgres", "decision_action": "invalidate", "workspace_id": api.ws}))
        .await
        .expect("a candidate answer");

    assert_refused(&result, "not_exact", &[decision]);
    assert!(text_of(&result).contains("nothing was invalidated."));
    assert_eq!(api.writes(), no_writes());
}

#[tokio::test]
async fn reviewing_a_decision_takes_one_clear_title() {
    let old = Uuid::new_v4();
    let new = Uuid::new_v4();
    let api = Api::start(decision_routes(old, new)).await;

    // verify and dispute stamp a review status; they do not retire.
    let result = api
        .memory()
        .execute(json!({"action": "decision_action", "decision_id": "postgres 16", "decision_action": "verify", "workspace_id": api.ws}))
        .await
        .expect("verify");
    assert!(text_of(&result).starts_with(&format!(
        "Resolved \"postgres 16\" to decision **Use Postgres 16 for the ledger** (id: {new})."
    )));
    assert_eq!(
        api.writes(),
        vec![format!(
            "POST /api/v1/memory/decisions/{new}/actions HTTP/1.1"
        )]
    );

    // Two decisions hold both words: a tie, even for a review.
    let result = api
        .memory()
        .execute(json!({"action": "decision_action", "decision_id": "postgres ledger", "decision_action": "dispute", "workspace_id": api.ws}))
        .await
        .expect("a candidate answer");
    assert_refused(&result, "ambiguous", &[old, new]);
    assert_eq!(api.writes().len(), 1);
}

#[tokio::test]
async fn create_decision_does_not_supersede_a_guess() {
    let old = Uuid::new_v4();
    let new = Uuid::new_v4();
    let api = Api::start(decision_routes(old, new)).await;

    let result = api
        .memory()
        .execute(json!({
            "action": "create_decision",
            "title": "Move the ledger to Postgres 17",
            "content": "Postgres 17 for the ledger.",
            "supersedes": "postgres 16",
            "workspace_id": api.ws
        }))
        .await
        .expect("a candidate answer");

    assert_refused(&result, "not_exact", &[old, new]);
    assert!(text_of(&result).contains("nothing was superseded. The new decision was not recorded."));
    assert_eq!(api.writes(), no_writes());
}

// ---------------------------------------------------------------------------
// Lessons
// ---------------------------------------------------------------------------

#[tokio::test]
async fn deleting_or_superseding_a_lesson_needs_an_exact_title() {
    let lesson = Uuid::new_v4();
    let api = Api::start(vec![
        route(
            "GET",
            "/api/v1/lessons?",
            200,
            items(json!([{"id": lesson, "title": "Quote shell paths", "severity": "high"}])),
        ),
        route(
            "POST",
            "/api/v1/lessons/",
            200,
            json!({"successor_id": Uuid::new_v4()}),
        ),
        route("DELETE", "/api/v1/lessons/", 200, json!({"deleted": true})),
    ])
    .await;

    let result = api
        .session_tool()
        .execute(
            json!({"action": "delete_lesson", "lesson_id": "shell paths", "workspace_id": api.ws}),
        )
        .await
        .expect("a candidate answer");
    assert_refused(&result, "not_exact", &[lesson]);

    let result = api
        .session_tool()
        .execute(json!({"action": "supersede_lesson", "lesson_id": "shell paths", "title": "Quote every shell path", "workspace_id": api.ws}))
        .await
        .expect("a candidate answer");
    assert_refused(&result, "not_exact", &[lesson]);

    // The lesson is exact, its successor is not.
    let result = api
        .session_tool()
        .execute(json!({"action": "supersede_lesson", "lesson_id": "quote shell paths", "successor_id": "shell", "workspace_id": api.ws}))
        .await
        .expect("a candidate answer");
    assert_refused(&result, "not_exact", &[lesson]);
    assert!(text_of(&result).contains("successor_id=\"<id>\""));
    assert_eq!(api.writes(), no_writes());

    let result = api
        .session_tool()
        .execute(json!({"action": "delete_lesson", "lesson_id": "quote shell PATHS", "workspace_id": api.ws}))
        .await
        .expect("delete");
    assert_eq!(text_of(&result), format!("Lesson deleted: {lesson}."));
    assert_eq!(
        api.writes(),
        vec![format!("DELETE /api/v1/lessons/{lesson} HTTP/1.1")]
    );
}

// ---------------------------------------------------------------------------
// Plans: the path that accepted any positive score
// ---------------------------------------------------------------------------

fn plan_routes(plans: Value, opened: Uuid) -> Vec<Route> {
    vec![
        route("GET", "/api/v1/plans?", 200, plans),
        route(
            "GET",
            "/api/v1/plans/",
            200,
            json!({"id": opened, "title": "Quality and dependency workflow redesign", "status": "active", "tasks": []}),
        ),
        route(
            "PATCH",
            "/api/v1/plans/",
            200,
            json!({"id": opened, "title": "Quality and dependency workflow redesign"}),
        ),
    ]
}

#[tokio::test]
async fn a_plan_is_not_updated_or_opened_on_a_partial_overlap() {
    // One plan shares two of the three words. Any positive score used to be
    // enough to open it, and to update it.
    let plan = Uuid::new_v4();
    let plans = json!([{
        "id": plan,
        "title": "Quality and dependency workflow redesign",
        "content": "Plan for the Code Health dashboard",
        "status": "active"
    }]);
    let api = Api::start(plan_routes(plans, plan)).await;

    let result = api
        .session_tool()
        .execute(json!({"action": "update_plan", "plan_id": "dashboard quality work", "description": "x", "workspace_id": api.ws}))
        .await
        .expect("a candidate answer");
    assert_refused(&result, "partial_match", &[plan]);
    assert!(text_of(&result).contains(&format!("(id: {plan}) [active] (0.0%)")));
    assert_eq!(api.writes(), no_writes());

    let result = api
        .session_tool()
        .execute(json!({"action": "get_plan", "query": "dashboard quality work", "workspace_id": api.ws}))
        .await
        .expect("get_plan");
    assert!(!result.is_error);
    let structured = result.structured_content.as_ref().unwrap();
    assert_eq!(structured["plan_resolution"]["mode"], "no_match_candidates");
    assert_eq!(
        structured["plan_resolution"]["candidates"][0]["id"],
        json!(plan)
    );
    assert!(!api.saw(&format!("GET /api/v1/plans/{plan}")));
}

#[tokio::test]
async fn a_plan_is_updated_by_one_clear_title_and_retired_only_by_an_exact_one() {
    let plan = Uuid::new_v4();
    let plans = json!([{
        "id": plan,
        "title": "Quality and dependency workflow redesign",
        "status": "active"
    }]);
    let api = Api::start(plan_routes(plans, plan)).await;

    // Every word is in the title, but archiving retires the plan.
    for status in ["archived", "completed", "abandoned"] {
        let result = api
            .session_tool()
            .execute(json!({"action": "update_plan", "plan_id": "quality workflow", "status": status, "workspace_id": api.ws}))
            .await
            .expect("a candidate answer");
        assert_refused(&result, "not_exact", &[plan]);
        assert!(text_of(&result).contains("retires the record"));
    }
    assert_eq!(api.writes(), no_writes());

    // The same reference is enough for an edit, and the plan it took is named.
    let result = api
        .session_tool()
        .execute(json!({"action": "update_plan", "plan_id": "quality workflow", "description": "x", "workspace_id": api.ws}))
        .await
        .expect("update");
    assert!(text_of(&result).starts_with(&format!(
        "Resolved \"quality workflow\" to plan **Quality and dependency workflow redesign** (id: {plan})."
    )));

    // The exact title archives it.
    let result = api
        .session_tool()
        .execute(json!({"action": "update_plan", "plan_id": "quality and dependency workflow REDESIGN", "status": "archived", "workspace_id": api.ws}))
        .await
        .expect("update");
    assert!(!result.is_error);
    assert_eq!(
        api.writes(),
        vec![format!("PATCH /api/v1/plans/{plan} HTTP/1.1"); 2]
    );
}

#[tokio::test]
async fn two_plans_with_the_same_title_are_both_returned() {
    let finished = Uuid::new_v4();
    let current = Uuid::new_v4();
    let plans = json!([
        {"id": finished, "title": "Release cutover", "status": "completed", "updated_at": "2026-05-13T12:00:00Z"},
        {"id": current, "title": "Release cutover", "status": "active", "updated_at": "2026-05-14T12:00:00Z"}
    ]);
    let api = Api::start(plan_routes(plans, current)).await;

    let result = api
        .session_tool()
        .execute(json!({"action": "update_plan", "plan_id": "Release cutover", "description": "x", "workspace_id": api.ws}))
        .await
        .expect("a candidate answer");
    assert_refused(&result, "duplicate_title", &[finished, current]);
    assert_eq!(api.writes(), no_writes());

    // The newest active plan used to win this tie.
    let result = api
        .session_tool()
        .execute(json!({"action": "get_plan", "query": "Release cutover", "workspace_id": api.ws}))
        .await
        .expect("get_plan");
    let resolution = &result.structured_content.as_ref().unwrap()["plan_resolution"];
    assert_eq!(resolution["mode"], "ambiguous_candidates");
    assert_eq!(resolution["reason"], "duplicate_title");
    assert_eq!(resolution["candidate_count"], 2);
    assert!(text_of(&result).starts_with("2 plans are titled 'Release cutover'; none was opened."));
    assert!(!api.saw("GET /api/v1/plans/"));
}

// ---------------------------------------------------------------------------
// Entities
// ---------------------------------------------------------------------------

#[tokio::test]
async fn deleting_an_entity_or_changing_its_status_needs_an_exact_title() {
    let ticket = Uuid::new_v4();
    let api = Api::start(vec![
        route(
            "GET",
            "/api/v1/tickets?",
            200,
            items(json!([{"id": ticket, "title": "Fix replication lag", "kind": "bug", "status": "open"}])),
        ),
        route(
            "PATCH",
            "/api/v1/tickets/",
            200,
            json!({"id": ticket, "title": "Fix replication lag", "kind": "bug", "status": "open"}),
        ),
        route(
            "DELETE",
            "/api/v1/tickets/",
            200,
            json!({"deleted": true}),
        ),
    ])
    .await;

    let result = api
        .entity()
        .execute(json!({"kind": "ticket", "action": "delete", "id": "replication lag"}))
        .await
        .expect("a candidate answer");
    assert_refused(&result, "not_exact", &[ticket]);
    assert!(text_of(&result)
        .ends_with("Retry: entity(kind=\"ticket\", action=\"delete\", id=\"<id>\")"));

    let result = api
        .entity()
        .execute(json!({"kind": "ticket", "action": "update", "id": "replication lag", "body": {"status": "closed"}}))
        .await
        .expect("a candidate answer");
    assert_refused(&result, "not_exact", &[ticket]);
    assert!(
        text_of(&result).contains("An update that sets a status needs an id or the exact title")
    );
    assert_eq!(api.writes(), no_writes());

    // An edit that leaves the status alone takes one clear title.
    let result = api
        .entity()
        .execute(json!({"kind": "ticket", "action": "update", "id": "replication lag", "body": {"description": "x"}}))
        .await
        .expect("update");
    assert!(text_of(&result).starts_with(&format!(
        "Resolved \"replication lag\" to ticket **Fix replication lag** (id: {ticket})."
    )));
    assert_eq!(
        api.writes(),
        vec![format!("PATCH /api/v1/tickets/{ticket} HTTP/1.1")]
    );

    api.entity()
        .execute(json!({"kind": "ticket", "action": "delete", "id": "fix replication LAG"}))
        .await
        .expect("delete");
    assert!(api.saw(&format!("DELETE /api/v1/tickets/{ticket} ")));
}

// ---------------------------------------------------------------------------
// Skills
// ---------------------------------------------------------------------------

#[tokio::test]
async fn deleting_or_archiving_a_skill_needs_its_exact_name_or_title() {
    let skill = Uuid::new_v4();
    let listed = json!({"id": skill, "name": "deploy-checker", "title": "Deploy checker", "scope": "personal", "status": "active"});
    let routes = || {
        vec![
            route(
                "GET",
                "/api/v1/skills?",
                200,
                items(json!([listed.clone()])),
            ),
            // A lone semantic match used to be enough to act on.
            route("POST", "/api/v1/skills/match", 200, json!([listed.clone()])),
            route("PATCH", "/api/v1/skills/", 200, json!({"version": 2})),
            route("DELETE", "/api/v1/skills/", 200, json!({"deleted": true})),
        ]
    };
    let api = Api::start(routes()).await;

    for input in [
        json!({"action": "delete", "name": "deploy"}),
        json!({"action": "supersede", "name": "deploy"}),
        json!({"action": "update", "name": "deploy", "status": "archived"}),
    ] {
        let result = api
            .skill()
            .execute(input)
            .await
            .expect("a candidate answer");
        assert_refused(&result, "not_exact", &[skill]);
        assert!(text_of(&result).contains("(deploy-checker) [personal|active]"));
    }
    assert_eq!(api.writes(), no_writes());

    // The name and the title are both the skill's own.
    for (reference, expected) in [
        (
            "deploy-checker",
            format!("DELETE /api/v1/skills/{skill} HTTP/1.1"),
        ),
        (
            "Deploy Checker",
            format!("DELETE /api/v1/skills/{skill} HTTP/1.1"),
        ),
    ] {
        let api = Api::start(routes()).await;
        let result = api
            .skill()
            .execute(json!({"action": "delete", "name": reference}))
            .await
            .expect("delete");
        assert!(!result.is_error);
        assert_eq!(api.writes(), vec![expected]);
    }

    // A skill listed without a name is shown as "?", which names nothing.
    let nameless = Api::start(vec![
        route(
            "GET",
            "/api/v1/skills?",
            200,
            items(json!([{"id": skill, "scope": "personal", "status": "active"}])),
        ),
        route("DELETE", "/api/v1/skills/", 200, json!({"deleted": true})),
    ])
    .await;
    let outcome = nameless
        .skill()
        .execute(json!({"action": "delete", "name": "?"}))
        .await;
    assert!(outcome.is_err(), "a placeholder must not resolve");
    assert_eq!(nameless.writes(), no_writes());

    let api = Api::start(routes()).await;
    api.skill()
        .execute(json!({"action": "supersede", "name": "deploy-checker", "superseded_by": "release-checker"}))
        .await
        .expect("supersede");
    assert_eq!(
        api.writes(),
        vec![format!("PATCH /api/v1/skills/{skill} HTTP/1.1")]
    );
}
