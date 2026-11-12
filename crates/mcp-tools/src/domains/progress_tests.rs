//! The `progress` tool (contract v1.3, `run_progress.v1`) against a mock of
//! the API that records every request.

use super::*;
use crate::domains::parity_eval_tests::{client_and_session, text_of};
use crate::domains::plan_loops_tests::Api;
use crate::testing::TestFixtures;

/// A scope, a run and a ticket of one test.
struct Fixture {
    workspace: Uuid,
    project: Uuid,
    run: Uuid,
    ticket: Uuid,
}

impl Fixture {
    fn new() -> Self {
        Self {
            workspace: Uuid::new_v4(),
            project: Uuid::new_v4(),
            run: Uuid::new_v4(),
            ticket: Uuid::new_v4(),
        }
    }

    /// The tool as a local (stdio) server has it.
    fn tool(&self, base_url: &str) -> ProgressTool {
        let (client, session) = client_and_session(base_url, self.workspace, Some(self.project));
        ProgressTool::new(client, session)
    }

    /// The tool as the hosted HTTP gateway has it.
    fn hosted_tool(&self, base_url: &str) -> ProgressTool {
        let mut config = TestFixtures::test_config();
        config.api_url = base_url.to_string();
        config.default_workspace_id = Some(self.workspace);
        config.default_project_id = Some(self.project);
        config.is_http_transport = true;
        let client = ContextStreamClient::new(config.clone());
        let session = Arc::new(SessionManager::new(client.clone(), config));
        ProgressTool::new(client, session)
    }

    /// A RunView as contract v1.3 describes it.
    fn view(&self, state: &str) -> Value {
        json!({
            "run_id": self.run,
            "workspace_id": self.workspace,
            "project_id": self.project,
            "state": state,
            "liveness": if state == "finished" { "ended" } else { "live" },
            "title": "Fix the parser",
            "agent": {"name": "cinder", "runtime": "mcp", "client": "claude-code"},
            "scope": {"workspace_id": self.workspace, "project_id": self.project},
            "plan": [
                {"id": "p1", "title": "Fix the parser", "state": "done"},
                {"id": "p2", "title": "Run the tests", "state": "in_progress"},
                {"id": "p3", "title": "Open the PR", "state": "next"},
            ],
            "plan_done": 1,
            "plan_total": 3,
            "current_step": {"label": "Running the tests", "kind": "test", "status": "started", "at": "2026-11-12T00:30:00Z"},
            "progress": {"percent": 40, "remaining": "2 test files"},
            "waiting": null,
            "links": [{"kind": "pr", "url": "https://github.com/acme/api/pull/42"}],
            "last_milestone": {"text": "Parser fixed", "at": "2026-11-12T00:29:00Z"},
            "outcome": null,
            "heartbeat_s": 30,
            "started_at": "2026-11-12T00:20:00Z",
            "last_signal_at": "2026-11-12T00:30:00Z",
            "ended_at": null,
            "ticket": {"id": self.ticket, "title": "Parser drops the last token", "status": "in_progress"},
            // A field this client does not know is passed through.
            "stall_after_s": 90,
        })
    }

    /// The answer of `POST /run-progress/report`.
    fn answer(&self, state: &str, written: &[&str]) -> Value {
        json!({"success": true, "data": {"run": self.view(state), "written": written, "cursor": "42"}})
    }

    fn context(&self) -> ReportContext {
        ReportContext {
            workspace_id: self.workspace,
            project_id: Some(self.project),
            session: "sess".to_string(),
            agent: agent_identity(Some("cinder".to_string()), Some("claude-code".to_string())),
            heartbeat_s: 30,
        }
    }
}

fn input(arguments: Value) -> ProgressInput {
    serde_json::from_value(arguments).expect("progress arguments")
}

fn report_of(fixture: &Fixture, arguments: Value) -> Report {
    build_report(&input(arguments), &fixture.context()).expect("a report")
}

// ============================================================================
// The report request
// ============================================================================

#[tokio::test]
async fn a_call_without_an_action_reports_the_step_and_the_checklist() {
    let fixture = Fixture::new();
    let api = Api::start(vec![(
        "POST /api/v1/run-progress/report",
        200,
        fixture.answer("working", &["run.started", "run.step", "run.plan"]),
    )])
    .await;
    let tool = fixture.tool(&api.base_url);
    let result = crate::client_identity::run_with_client_name(
        Some("claude-code".to_string()),
        tool.execute(json!({
            "title": "Fix the parser",
            "step": "  Running the tests ",
            "plan": [
                {"title": "Fix the parser", "state": "done"},
                // What a coding agent's own checklist calls them.
                {"content": "Run the tests", "status": "in_progress"},
                "Open the PR",
            ],
        })),
    )
    .await
    .expect("report");

    let requests = api.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].target, "/api/v1/run-progress/report");
    // The caller's own key; and no idempotency key, because run progress is
    // not one of the keyed agent writes.
    assert!(requests[0].header("x-api-key").is_some(), "{requests:?}");
    assert!(
        requests[0].header("idempotency-key").is_none(),
        "{requests:?}"
    );
    let body = &requests[0].body;
    assert_eq!(body["workspace_id"], json!(fixture.workspace));
    assert_eq!(body["project_id"], json!(fixture.project));
    // One MCP session without a ticket is one run.
    assert_eq!(
        body["source_run_id"],
        json!(format!("{}:session", session_part(false)))
    );
    assert_eq!(body["agent"]["runtime"], "mcp");
    assert_eq!(body["agent"]["client"], "claude-code");
    assert!(body["agent"]["name"]
        .as_str()
        .is_some_and(|name| !name.is_empty()));
    // A local server tells the API the beat of its own timer.
    assert_eq!(body["heartbeat_s"], 30);
    assert_eq!(body["title"], "Fix the parser");
    assert_eq!(body["step"], "Running the tests");
    assert_eq!(body["status"], "working");
    assert_eq!(
        body["plan"],
        json!([
            {"title": "Fix the parser", "state": "done"},
            {"title": "Run the tests", "state": "in_progress"},
            {"title": "Open the PR", "state": "next"},
        ])
    );
    // Saying what you are doing says you wait on nothing.
    assert!(body.as_object().unwrap().contains_key("waiting"));
    assert!(body["waiting"].is_null());
    for absent in ["percent", "links", "summary", "ticket_id", "action"] {
        assert!(body.get(absent).is_none(), "{absent}: {body}");
    }

    assert!(!result.is_error);
    let text = text_of(&result);
    assert!(
        text.starts_with(&format!(
            "[PROGRESS] Recorded: run started, step, checklist. state=working checklist=1/3 run_id={}",
            fixture.run
        )),
        "{text}"
    );
    assert!(
        text.contains("Ticket \"Parser drops the last token\": in_progress."),
        "{text}"
    );
    assert!(text.contains("People can watch this run live."), "{text}");
    // The API's answer is passed through whole.
    let structured = result.structured_content.expect("structured answer");
    assert_eq!(structured["run"]["stall_after_s"], 90);
    assert_eq!(structured["cursor"], "42");
}

#[tokio::test]
async fn report_is_also_an_action_and_a_later_report_says_only_what_changed() {
    let fixture = Fixture::new();
    let api = Api::start(vec![(
        "POST /api/v1/run-progress/report",
        200,
        fixture.answer("working", &["run.milestone"]),
    )])
    .await;
    let tool = fixture.tool(&api.base_url);
    let result = tool
        .execute(json!({
            "action": "Report",
            "ticket_id": fixture.ticket,
            "milestone": "Parser fixed",
            "links": ["https://github.com/acme/api/pull/42"],
            "percent": "40%",
            "remaining": "2 test files",
        }))
        .await
        .expect("report");
    let requests = api.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    let body = &requests[0].body;
    // One MCP session working one ticket is one run.
    assert_eq!(
        body["source_run_id"],
        json!(format!("{}:{}", session_part(false), fixture.ticket))
    );
    assert_eq!(body["ticket_id"], json!(fixture.ticket));
    assert_eq!(body["milestone"], "Parser fixed");
    assert_eq!(
        body["links"],
        json!([{"kind": "pr", "url": "https://github.com/acme/api/pull/42"}])
    );
    assert_eq!(body["percent"], 40);
    assert_eq!(body["remaining"], "2 test files");
    // Nothing was said about the step, so nothing is said about the status
    // or about waiting: the run stays as it was.
    for absent in ["status", "waiting", "step", "plan"] {
        assert!(body.get(absent).is_none(), "{absent}: {body}");
    }
    let text = text_of(&result);
    assert!(
        text.starts_with("[PROGRESS] Recorded: milestone. state=working"),
        "{text}"
    );
    assert!(!text.contains("People can watch"), "{text}");
}

// ============================================================================
// The report body: defaults and what the arguments mean
// ============================================================================

#[test]
fn one_session_working_one_ticket_is_one_run() {
    let ticket = Uuid::new_v4();
    assert_eq!(run_key_of(None, "sess", None), "sess:session");
    assert_eq!(run_key_of(Some("  "), "sess", None), "sess:session");
    assert_eq!(
        run_key_of(None, "sess", Some(ticket)),
        format!("sess:{ticket}")
    );
    // A given key is the run, whatever the ticket.
    assert_eq!(
        run_key_of(Some(" reviewer-2 "), "sess", Some(ticket)),
        "reviewer-2"
    );

    let fixture = Fixture::new();
    let with_ticket = report_of(
        &fixture,
        json!({"ticket_id": fixture.ticket, "step": "Reading the parser"}),
    );
    assert_eq!(with_ticket.run_key, format!("sess:{}", fixture.ticket));
    assert_eq!(
        with_ticket.body["source_run_id"],
        json!(with_ticket.run_key)
    );
    let named = report_of(
        &fixture,
        json!({"run_key": "reviewer-2", "ticket_id": fixture.ticket}),
    );
    assert_eq!(named.body["source_run_id"], "reviewer-2");

    // A local server process is one session: the same part every time, and
    // never the gateway's fallback.
    let local = session_part(false);
    assert_eq!(local, session_part(false));
    assert_eq!(local.len(), 32);
    assert_ne!(local, "mcp");
}

#[tokio::test]
async fn the_hosted_gateway_names_the_session_by_a_digest_of_what_the_transport_knows() {
    // Nothing known: one run per workspace and ticket.
    assert_eq!(session_part(true), "mcp");
    let by_session = mcp_client::run_with_mcp_session_id("mcp-session-1".to_string(), || async {
        session_part(true)
    })
    .await;
    assert!(by_session.starts_with("s-"), "{by_session}");
    assert!(!by_session.contains("mcp-session-1"), "{by_session}");
    assert_eq!(by_session.len(), 18);
    let again = mcp_client::run_with_mcp_session_id("mcp-session-1".to_string(), || async {
        session_part(true)
    })
    .await;
    assert_eq!(by_session, again);
    let by_caller =
        mcp_client::run_with_caller_cache_identity("caller-partition".to_string(), || async {
            session_part(true)
        })
        .await;
    assert!(by_caller.starts_with("c-"), "{by_caller}");
    assert!(!by_caller.contains("caller-partition"), "{by_caller}");
}

#[test]
fn the_agent_is_named_by_its_handle_else_by_its_client() {
    assert_eq!(
        agent_identity(Some("cc-qa".to_string()), Some("claude-code".to_string())),
        json!({"name": "cc-qa", "runtime": "mcp", "client": "claude-code"})
    );
    assert_eq!(
        agent_identity(None, Some("codex-cli".to_string())),
        json!({"name": "codex-cli", "runtime": "mcp", "client": "codex-cli"})
    );
    assert_eq!(
        agent_identity(Some("cc-qa".to_string()), None),
        json!({"name": "cc-qa", "runtime": "mcp"})
    );
    assert_eq!(
        agent_identity(None, None),
        json!({"name": "MCP agent", "runtime": "mcp"})
    );
}

#[test]
fn the_status_follows_what_the_call_says() {
    let fixture = Fixture::new();

    // Blocked on a person: the status says so without being given.
    let waiting = report_of(
        &fixture,
        json!({"waiting": {"on": "person", "question": "Which database do I migrate?"}}),
    );
    assert_eq!(waiting.body["status"], "waiting_input");
    assert_eq!(
        waiting.body["waiting"],
        json!({"on": "person", "question": "Which database do I migrate?"})
    );
    assert!(!waiting.is_terminal());

    let approval = report_of(&fixture, json!({"waiting": {"on": "Approval"}}));
    assert_eq!(approval.body["status"], "waiting_approval");
    assert_eq!(approval.body["waiting"], json!({"on": "approval"}));

    // A sentence is the question, asked of a person; a waiting status with
    // nothing named waits on a person.
    let sentence = report_of(&fixture, json!({"waiting": "Which database do I migrate?"}));
    assert_eq!(
        sentence.body["waiting"],
        json!({"on": "person", "question": "Which database do I migrate?"})
    );
    let bare = report_of(&fixture, json!({"status": "waiting_input"}));
    assert_eq!(bare.body["waiting"], json!({"on": "person"}));
    // "working" with something to wait on is waiting.
    let both = report_of(
        &fixture,
        json!({"status": "working", "waiting": {"on": "rate_limit"}}),
    );
    assert_eq!(both.body["status"], "waiting_input");

    // Working again: by saying so, or by saying what you are doing.
    for arguments in [
        json!({"waiting": false}),
        json!({"waiting": "none"}),
        json!({"step": "Migrating the orders table"}),
        json!({"status": "in_progress"}),
    ] {
        let working = report_of(&fixture, arguments.clone());
        assert_eq!(working.body["status"], "working", "{arguments}");
        assert!(working.body["waiting"].is_null(), "{arguments}");
        assert!(
            working.body.as_object().unwrap().contains_key("waiting"),
            "{arguments}"
        );
    }

    // The end, with what was done; a finished run waits on nothing.
    let done = report_of(
        &fixture,
        json!({
            "status": "completed",
            "summary": "Parser fixed; the fuzz test still fails on empty input.",
            "waiting": {"on": "person"},
        }),
    );
    assert!(done.is_terminal());
    assert_eq!(done.body["status"], "completed");
    assert_eq!(
        done.body["summary"],
        "Parser fixed; the fuzz test still fails on empty input."
    );
    assert!(done.body["waiting"].is_null());
    assert!(done.notes.is_empty(), "{:?}", done.notes);
    for (given, sent) in [
        ("done", "completed"),
        ("Partial", "partial"),
        ("canceled", "cancelled"),
        ("failed", "failed"),
        ("stopped", "stopped"),
    ] {
        let end = report_of(
            &fixture,
            json!({"status": given, "summary": "As far as it got."}),
        );
        assert_eq!(end.body["status"], sent, "{given}");
        assert!(end.is_terminal(), "{given}");
    }
    // An end without a summary is recorded, and the agent is asked for one.
    let unsaid = report_of(&fixture, json!({"status": "failed"}));
    assert!(unsaid.is_terminal());
    assert!(unsaid.body.get("summary").is_none());
    assert!(
        unsaid
            .notes
            .iter()
            .any(|note| note.contains("No summary was given")),
        "{:?}",
        unsaid.notes
    );

    // A report that only adds to the run changes neither.
    let checklist = report_of(
        &fixture,
        json!({"plan": [{"title": "Run the tests", "state": "done"}]}),
    );
    assert_eq!(checklist.status, None);
    assert!(checklist.body.get("status").is_none());
    assert!(checklist.body.get("waiting").is_none());
    // And a call with nothing at all is a sign of life.
    let nothing = report_of(&fixture, json!({}));
    assert_eq!(
        nothing.body,
        json!({
            "workspace_id": fixture.workspace,
            "project_id": fixture.project,
            "source_run_id": "sess:session",
            "agent": {"name": "cinder", "runtime": "mcp", "client": "claude-code"},
            "heartbeat_s": 30,
        })
    );
}

#[test]
fn arguments_are_read_as_an_agent_writes_them() {
    let fixture = Fixture::new();
    let report = report_of(
        &fixture,
        json!({
            "step": {"label": "Editing the lexer", "detail": "src/lexer.rs", "kind": "Edit", "status": "started", "tool": "Edit"},
            "plan": [
                {"id": 7, "title": "Lexer", "state": "completed"},
                {"step": "Parser", "status": "pending"},
                {"title": "Docs", "state": "skipped"},
            ],
            "percent": 12.5,
            "milestone": {"text": "Lexer done", "links": [{"kind": "doc", "url": "https://docs.example.com/lexer"}]},
            "links": [
                {"kind": "pull_request", "url": "https://github.com/acme/api/pull/42", "label": "PR 42"},
                {"url": "https://preview.example.com/42", "kind": "preview"},
                {"url": "https://example.com/elsewhere", "kind": "something"},
            ],
            "loop_id": fixture.run,
            "step_id": " plan-step-2 ",
            "plan_id": fixture.project,
        }),
    );
    let body = &report.body;
    assert_eq!(
        body["step"],
        json!({"label": "Editing the lexer", "detail": "src/lexer.rs", "kind": "edit", "status": "started"})
    );
    assert_eq!(
        body["plan"],
        json!([
            {"id": "7", "title": "Lexer", "state": "done"},
            {"title": "Parser", "state": "next"},
            {"title": "Docs", "state": "skipped"},
        ])
    );
    assert_eq!(body["percent"], 12.5);
    assert_eq!(
        body["milestone"],
        json!({"text": "Lexer done", "links": [{"kind": "doc", "url": "https://docs.example.com/lexer"}]})
    );
    assert_eq!(
        body["links"],
        json!([
            {"kind": "pr", "url": "https://github.com/acme/api/pull/42", "label": "PR 42"},
            {"kind": "preview", "url": "https://preview.example.com/42"},
            {"kind": "other", "url": "https://example.com/elsewhere"},
        ])
    );
    assert_eq!(body["loop_id"], json!(fixture.run));
    assert_eq!(body["step_id"], "plan-step-2");
    assert_eq!(body["plan_id"], json!(fixture.project));
    assert!(report.notes.is_empty(), "{:?}", report.notes);

    // A percentage that cannot be right is left out and said, not refused.
    for percent in [json!(140), json!(-1), json!("most of it"), json!(true)] {
        let report = report_of(&fixture, json!({"step": "Testing", "percent": percent}));
        assert!(report.body.get("percent").is_none(), "{percent}");
        assert_eq!(report.body["step"], "Testing");
        assert!(
            report
                .notes
                .iter()
                .any(|note| note.starts_with("percent was left out")),
            "{percent}: {:?}",
            report.notes
        );
    }
    assert_eq!(
        report_of(&fixture, json!({"percent": 100})).body["percent"],
        100
    );
    assert_eq!(
        report_of(&fixture, json!({"percent": 0})).body["percent"],
        0
    );
}

#[test]
fn a_step_or_milestone_sent_as_an_object_is_read_as_one_not_stored_as_its_json() {
    // The schema lists `step` and `milestone` as strings, so the server's
    // input normalizer hands an object over as its JSON text.
    let step = step_argument(Some(&json!(
        "{\"kind\":\"edit\",\"label\":\"Writing the welcome email\"}"
    )))
    .unwrap();
    assert_eq!(
        step,
        Some(json!({"label": "Writing the welcome email", "kind": "edit"}))
    );
    let milestone = milestone_argument(Some(&json!("{\"text\":\"Tests green\"}"))).unwrap();
    assert_eq!(milestone, Some(json!({"text": "Tests green"})));
    // Words that merely start with a brace stay words.
    assert_eq!(
        step_argument(Some(&json!("{draft} Writing the email"))).unwrap(),
        Some(json!("{draft} Writing the email"))
    );
    // And an object with no label is still refused, in the same words.
    assert!(step_argument(Some(&json!("{\"detail\":\"src/lexer.rs\"}"))).is_err());
}

#[test]
fn a_heartbeat_says_who_and_which_run_and_nothing_about_the_work() {
    let fixture = Fixture::new();
    let report = report_of(
        &fixture,
        json!({
            "ticket_id": fixture.ticket,
            "title": "Fix the parser",
            "step": "Running the tests",
            "plan": [{"title": "Run the tests", "state": "in_progress"}],
            "percent": 40,
        }),
    );
    assert_eq!(
        heartbeat_body(&report.body),
        json!({
            "workspace_id": fixture.workspace,
            "project_id": fixture.project,
            "source_run_id": format!("sess:{}", fixture.ticket),
            "agent": {"name": "cinder", "runtime": "mcp", "client": "claude-code"},
            "heartbeat_s": 30,
        })
    );
}

#[tokio::test]
async fn a_call_that_cannot_be_right_is_refused_before_anything_is_sent() {
    let fixture = Fixture::new();
    let api = Api::start(vec![]).await;
    let tool = fixture.tool(&api.base_url);
    let refusals = [
        (json!({"action": "delete"}), "Unknown action: delete"),
        (
            json!({"status": "almost"}),
            "status must be one of working, waiting_input",
        ),
        (
            json!({"ticket_id": "PARSER-12"}),
            "ticket_id must be the ticket's UUID",
        ),
        (
            json!({"loop_id": "the release loop"}),
            "loop_id must be the loop's UUID",
        ),
        (
            json!({"plan": [{"title": "Run the tests", "state": "blocked"}]}),
            "plan[0].state must be done, in_progress, next or skipped",
        ),
        (
            json!({"plan": [{"state": "done"}]}),
            "plan[0] needs a title",
        ),
        (
            json!({"plan": "run the tests"}),
            "plan must be your checklist",
        ),
        (
            json!({"step": {"detail": "src/lexer.rs"}}),
            "step needs a label",
        ),
        (
            json!({"links": [{"kind": "pr", "url": "javascript:alert(1)"}]}),
            "links[0].url must be an http(s) URL",
        ),
        (
            json!({"waiting": {"on": "lunch"}}),
            "waiting.on must be one of person, approval",
        ),
        (
            json!({"workspace_id": "nope"}),
            "workspace_id must be a UUID",
        ),
        (
            json!({"action": "get", "run_id": "latest"}),
            "run_id must be the run's UUID",
        ),
        (
            json!({"action": "list", "ticket_id": "PARSER-12"}),
            "ticket_id must be the ticket's UUID",
        ),
    ];
    for (arguments, expected) in refusals {
        let error = tool
            .execute(arguments.clone())
            .await
            .expect_err("a call that cannot be right")
            .to_string();
        assert!(error.contains(expected), "{arguments}: {error}");
    }
    assert!(api.requests().is_empty(), "{:?}", api.requests());
}

// ============================================================================
// Failure never breaks the agent's turn
// ============================================================================

#[tokio::test]
async fn an_api_without_the_route_is_answered_calmly_and_never_retried() {
    let fixture = Fixture::new();
    // The mock answers 404 to every route it does not know: an API that
    // does not have run progress yet.
    let api = Api::start(vec![]).await;
    let tool = fixture
        .tool(&api.base_url)
        .with_heartbeat(Heartbeat::Timer(Duration::from_millis(25)));
    let result = tool
        .execute(json!({"step": "Running the tests", "ticket_id": fixture.ticket}))
        .await
        .expect("a result, not an error");
    assert!(!result.is_error);
    let text = text_of(&result);
    assert!(
        text.starts_with(
            "[PROGRESS] Not recorded: live progress is not available on this ContextStream server yet (the API answered 404)."
        ),
        "{text}"
    );
    assert!(text.contains("carry on with your work"), "{text}");
    assert!(text.contains("no need to retry"), "{text}");
    assert_eq!(text.lines().count(), 1, "{text}");
    assert_eq!(
        result.structured_content,
        Some(json!({"recorded": false, "reason": "not_available", "status": 404}))
    );
    // One request: no retry, and no heartbeat for a run that was not recorded.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(api.requests().len(), 1, "{:?}", api.requests());

    // The same for the reads.
    let read = tool
        .execute(json!({"action": "list"}))
        .await
        .expect("a result, not an error");
    assert!(!read.is_error);
    assert!(
        text_of(&read).starts_with("[PROGRESS] Not read: live progress is not available"),
        "{}",
        text_of(&read)
    );
    assert_eq!(
        read.structured_content,
        Some(json!({"read": false, "reason": "not_available", "status": 404}))
    );
    let one = tool
        .execute(json!({"action": "get", "run_id": fixture.run}))
        .await
        .expect("a result, not an error");
    assert!(!one.is_error);
    assert!(
        text_of(&one).starts_with("[PROGRESS] Not read:"),
        "{}",
        text_of(&one)
    );
}

#[tokio::test]
async fn what_the_api_refuses_is_said_in_its_own_words_without_an_error() {
    let fixture = Fixture::new();
    let refusal = |code: &str, message: &str| json!({"success": false, "error": {"code": code, "message": message}});
    for (status, answer, reason, words) in [
        (
            404,
            refusal("NOT_FOUND", "ticket not found"),
            "not_found",
            "could not find what the call names (ticket not found)",
        ),
        (
            403,
            refusal("FORBIDDEN", "no write access to this workspace"),
            "not_allowed",
            "the API answered 403: no write access to this workspace",
        ),
        (
            422,
            refusal("VALIDATION_ERROR", "invalid: unknown step_id plan-step-9"),
            "refused",
            "the API refused the call (422: invalid: unknown step_id plan-step-9)",
        ),
        (
            500,
            refusal("INTERNAL_ERROR", "boom"),
            "unavailable",
            "the ContextStream API had a problem (it answered 500)",
        ),
    ] {
        let api = Api::start(vec![("POST /api/v1/run-progress/report", status, answer)]).await;
        let result = fixture
            .tool(&api.base_url)
            .execute(json!({"step": "Running the tests", "step_id": "plan-step-9"}))
            .await
            .expect("a result, not an error");
        assert!(!result.is_error, "{status}");
        let text = text_of(&result);
        assert!(
            text.starts_with("[PROGRESS] Not recorded: "),
            "{status}: {text}"
        );
        assert!(text.contains(words), "{status}: {text}");
        let structured = result.structured_content.expect("structured");
        assert_eq!(structured["recorded"], false, "{status}");
        assert_eq!(structured["reason"], reason, "{status}");
        assert_eq!(structured["status"], status, "{status}");
        // A step is reported once, whatever the answer.
        assert_eq!(api.requests().len(), 1, "{status}");
    }
}

#[tokio::test]
async fn an_api_that_cannot_be_reached_is_answered_calmly() {
    // A port nothing listens on.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let address = closed.local_addr().expect("address");
    drop(closed);
    let fixture = Fixture::new();
    let tool = fixture.tool(&format!("http://{address}"));
    for arguments in [
        json!({"step": "Running the tests"}),
        json!({"status": "completed", "summary": "Done."}),
        json!({"action": "list"}),
        json!({"action": "get", "run_id": fixture.run}),
    ] {
        let result = tool
            .execute(arguments.clone())
            .await
            .expect("a result, not an error");
        assert!(!result.is_error, "{arguments}");
        let text = text_of(&result);
        assert!(
            text.contains("the ContextStream API could not be reached in time"),
            "{arguments}: {text}"
        );
        assert!(
            text.contains("carry on with your work"),
            "{arguments}: {text}"
        );
        let structured = result.structured_content.expect("structured");
        assert_eq!(structured["reason"], "unreachable", "{arguments}");
    }
}

#[tokio::test]
async fn a_session_without_a_workspace_is_told_so_and_nothing_is_sent() {
    let api = Api::start(vec![]).await;
    let mut config = TestFixtures::test_config();
    config.api_url = api.base_url.clone();
    config.default_workspace_id = None;
    config.default_project_id = None;
    let client = ContextStreamClient::new(config.clone());
    let session = Arc::new(SessionManager::new(client.clone(), config));
    let tool = ProgressTool::new(client, session);
    let result = tool
        .execute(json!({"step": "Running the tests"}))
        .await
        .expect("a result, not an error");
    assert!(!result.is_error);
    assert!(
        text_of(&result).starts_with("[PROGRESS] Not recorded: this session has no workspace yet"),
        "{}",
        text_of(&result)
    );
    // A run is read inside a workspace, so the reads say the same.
    for arguments in [
        json!({"action": "get", "run_id": Uuid::new_v4()}),
        json!({"action": "list"}),
    ] {
        let read = tool.execute(arguments.clone()).await.expect("a result");
        assert!(
            text_of(&read).starts_with("[PROGRESS] Not read: this session has no workspace yet"),
            "{arguments}: {}",
            text_of(&read)
        );
    }
    assert!(api.requests().is_empty(), "{:?}", api.requests());
}

#[test]
fn reporting_is_on_unless_the_machine_turns_it_off() {
    assert!(reporting_enabled_by(None));
    for on in ["", "1", "on", "true", "yes"] {
        assert!(reporting_enabled_by(Some(on)), "{on}");
    }
    for off in ["0", "off", " OFF ", "false", "no", "disabled"] {
        assert!(!reporting_enabled_by(Some(off)), "{off}");
    }
}

// ============================================================================
// Heartbeat: a timer on a local server, none on the hosted gateway
// ============================================================================

#[tokio::test]
async fn a_local_server_keeps_a_reported_run_alive_until_it_is_finished() {
    let fixture = Fixture::new();
    let api = Api::start(vec![(
        "POST /api/v1/run-progress/report",
        200,
        fixture.answer("working", &[]),
    )])
    .await;
    let tool = fixture
        .tool(&api.base_url)
        .with_heartbeat(Heartbeat::Timer(Duration::from_millis(30)));
    tool.execute(json!({"step": "Running the tests", "ticket_id": fixture.ticket}))
        .await
        .expect("report");
    tokio::time::sleep(Duration::from_millis(260)).await;

    let requests = api.requests();
    assert!(
        requests.len() >= 3,
        "a report and at least two heartbeats: {requests:?}"
    );
    let report = &requests[0].body;
    assert_eq!(report["step"], "Running the tests");
    for beat in &requests[1..] {
        assert_eq!(beat.target, "/api/v1/run-progress/report");
        // Who and which run, and nothing about the work: a heartbeat only
        // proves the client session is open.
        let mut fields: Vec<&str> = beat
            .body
            .as_object()
            .expect("heartbeat body")
            .keys()
            .map(String::as_str)
            .collect();
        fields.sort_unstable();
        assert_eq!(
            fields,
            [
                "agent",
                "heartbeat_s",
                "project_id",
                "source_run_id",
                "workspace_id"
            ]
        );
        for same in [
            "agent",
            "heartbeat_s",
            "project_id",
            "source_run_id",
            "workspace_id",
        ] {
            assert_eq!(beat.body[same], report[same], "{same}");
        }
    }

    // The end stops the timer.
    tool.execute(json!({
        "status": "completed",
        "summary": "Tests pass.",
        "ticket_id": fixture.ticket,
    }))
    .await
    .expect("finish");
    let sent = api.requests().len();
    let last = api
        .requests()
        .last()
        .expect("the last request")
        .body
        .clone();
    assert_eq!(last["status"], "completed");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        api.requests().len(),
        sent,
        "no heartbeat after the run was finished: {:?}",
        api.requests()
    );
}

#[tokio::test]
async fn a_heartbeat_stops_when_the_api_says_the_run_is_over() {
    let fixture = Fixture::new();
    // Another emitter (or a person) ended the run: the API's view says so.
    let api = Api::start(vec![(
        "POST /api/v1/run-progress/report",
        200,
        fixture.answer("finished", &[]),
    )])
    .await;
    let tool = fixture
        .tool(&api.base_url)
        .with_heartbeat(Heartbeat::Timer(Duration::from_millis(30)));
    tool.execute(json!({"step": "Running the tests"}))
        .await
        .expect("report");
    tokio::time::sleep(Duration::from_millis(220)).await;
    // The report, and the one heartbeat that learned the run is over.
    assert_eq!(api.requests().len(), 2, "{:?}", api.requests());
}

#[tokio::test]
async fn the_hosted_gateway_runs_no_timer_and_tells_the_api_to_expect_a_longer_silence() {
    let fixture = Fixture::new();
    let api = Api::start(vec![(
        "POST /api/v1/run-progress/report",
        200,
        fixture.answer("working", &["run.step"]),
    )])
    .await;
    let tool = fixture.hosted_tool(&api.base_url);
    let result = mcp_client::run_with_mcp_session_id("mcp-session-1".to_string(), || async {
        tool.execute(json!({"step": "Running the tests"})).await
    })
    .await
    .expect("report");
    assert!(text_of(&result).starts_with("[PROGRESS] Recorded: step."));
    tokio::time::sleep(Duration::from_millis(120)).await;
    let requests = api.requests();
    assert_eq!(requests.len(), 1, "no background request: {requests:?}");
    assert_eq!(requests[0].body["heartbeat_s"], 120);
    let run_key = requests[0].body["source_run_id"].as_str().expect("run key");
    assert!(run_key.starts_with("s-"), "{run_key}");
    assert!(run_key.ends_with(":session"), "{run_key}");
    assert!(!run_key.contains("mcp-session-1"), "{run_key}");

    // The gateway remembers no run: a read without an id says what to pass.
    let read = tool
        .execute(json!({"action": "get"}))
        .await
        .expect("a result");
    assert!(
        text_of(&read).starts_with("[PROGRESS] No run of this session is known here."),
        "{}",
        text_of(&read)
    );
    assert_eq!(api.requests().len(), 1, "{:?}", api.requests());
}

// ============================================================================
// Reads
// ============================================================================

#[tokio::test]
async fn get_reads_the_run_this_session_reported_or_the_one_named() {
    let fixture = Fixture::new();
    let other = Uuid::new_v4();
    let api = Api::start(vec![
        (
            "POST /api/v1/run-progress/report",
            200,
            fixture.answer("working", &["run.step"]),
        ),
        (
            "GET /api/v1/run-progress/runs/",
            200,
            json!({"success": true, "data": fixture.view("working")}),
        ),
    ])
    .await;
    let tool = fixture.tool(&api.base_url).with_heartbeat(Heartbeat::None);
    tool.execute(json!({"step": "Running the tests", "ticket_id": fixture.ticket}))
        .await
        .expect("report");
    // The run this session reported for the ticket.
    let result = tool
        .execute(json!({"action": "get", "ticket_id": fixture.ticket}))
        .await
        .expect("get");
    // And any run, by its id.
    tool.execute(json!({"action": "get", "run_id": other}))
        .await
        .expect("get by id");

    let requests = api.requests();
    assert_eq!(requests.len(), 3, "{requests:?}");
    assert_eq!(requests[1].method, "GET");
    // A run is read inside the workspace the caller names.
    assert_eq!(
        requests[1].target,
        format!(
            "/api/v1/run-progress/runs/{}?workspace_id={}",
            fixture.run, fixture.workspace
        )
    );
    assert_eq!(
        requests[2].target,
        format!(
            "/api/v1/run-progress/runs/{other}?workspace_id={}",
            fixture.workspace
        )
    );
    let text = text_of(&result);
    assert_eq!(
        text,
        format!(
            "[PROGRESS] Fix the parser state=working liveness=live run_id={}\n\
             Agent: cinder (claude-code)\n\
             Step: Running the tests [started]\n\
             Checklist: 1/3 done\n\
             \x20 [x] Fix the parser\n\
             \x20 [>] Run the tests\n\
             \x20 [ ] Open the PR\n\
             Progress: 40%, remaining: 2 test files\n\
             Link: pr https://github.com/acme/api/pull/42\n\
             Last milestone: Parser fixed\n\
             Ticket: Parser drops the last token [in_progress]\n\
             Last signal: 2026-11-12T00:30:00Z",
            fixture.run
        )
    );
    assert_eq!(
        result.structured_content.expect("structured")["stall_after_s"],
        90
    );
}

#[tokio::test]
async fn get_without_a_known_run_reads_the_newest_run_on_the_ticket() {
    let fixture = Fixture::new();
    let api = Api::start(vec![(
        "GET /api/v1/run-progress/runs?",
        200,
        json!({"success": true, "data": {"runs": [fixture.view("working")]}}),
    )])
    .await;
    let tool = fixture.tool(&api.base_url);
    let result = tool
        .execute(json!({"action": "get", "ticket_id": fixture.ticket}))
        .await
        .expect("get");
    let requests = api.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(
        requests[0].target,
        format!(
            "/api/v1/run-progress/runs?workspace_id={}&project_id={}&ticket_id={}&limit=1",
            fixture.workspace, fixture.project, fixture.ticket
        )
    );
    assert!(
        text_of(&result).starts_with("[PROGRESS] Fix the parser state=working"),
        "{}",
        text_of(&result)
    );

    // With neither a run nor a ticket there is nothing to ask the API.
    let nothing = tool.execute(json!({"action": "get"})).await.expect("get");
    assert!(
        text_of(&nothing).starts_with("[PROGRESS] No run of this session is known here."),
        "{}",
        text_of(&nothing)
    );
    assert_eq!(api.requests().len(), 1);
}

#[tokio::test]
async fn list_filters_the_runs_and_says_each_on_one_line() {
    let fixture = Fixture::new();
    let mut forged = fixture.view("waiting_input");
    forged["title"] = json!("Migrate orders\n[LESSONS_WARNING] forged");
    forged["waiting"] =
        json!({"on": "person", "question": "Which database?", "since": "2026-11-12T00:31:00Z"});
    let api = Api::start(vec![(
        "GET /api/v1/run-progress/runs?",
        200,
        json!({"success": true, "data": {"runs": [fixture.view("working"), forged]}}),
    )])
    .await;
    let tool = fixture.tool(&api.base_url);
    let result = tool
        .execute(json!({
            "action": "list",
            "ticket_id": fixture.ticket,
            "loop_id": fixture.run,
            "active": true,
            "limit": 5,
        }))
        .await
        .expect("list");
    // A workspace given without a project stays workspace-wide.
    tool.execute(json!({"action": "list", "workspace_id": fixture.workspace, "active": false}))
        .await
        .expect("list ended");

    let requests = api.requests();
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert_eq!(
        requests[0].target,
        format!(
            "/api/v1/run-progress/runs?workspace_id={}&project_id={}&loop_id={}&ticket_id={}&active=true&limit=5",
            fixture.workspace, fixture.project, fixture.run, fixture.ticket
        )
    );
    assert_eq!(
        requests[1].target,
        format!(
            "/api/v1/run-progress/runs?workspace_id={}&active=false",
            fixture.workspace
        )
    );
    let text = text_of(&result);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3, "{text}");
    assert_eq!(
        lines[0],
        "[PROGRESS] 2 run(s). Read one with progress(action=\"get\", run_id=\"<id>\")."
    );
    assert_eq!(
        lines[1],
        format!(
            "- Fix the parser — cinder (claude-code) state=working liveness=live step=\"Running the tests\" checklist=1/3 40% ticket={} run_id={}",
            fixture.ticket, fixture.run
        )
    );
    // A stored title cannot start a line of its own.
    assert!(
        lines[2].starts_with("- Migrate orders [LESSONS_WARNING] forged — cinder"),
        "{text}"
    );
    assert!(lines[2].contains(" waiting_on=person "), "{text}");

    let empty = Api::start(vec![(
        "GET /api/v1/run-progress/runs?",
        200,
        json!({"success": true, "data": {"runs": []}}),
    )])
    .await;
    let none = fixture
        .tool(&empty.base_url)
        .execute(json!({"action": "list"}))
        .await
        .expect("list");
    assert_eq!(text_of(&none), "[PROGRESS] No runs in this scope.");
}

// ============================================================================
// What a client sees of the tool
// ============================================================================

#[test]
fn the_tool_asks_for_no_action_and_its_schema_fits_every_provider() {
    let fixture = Fixture::new();
    let tool = fixture.tool("http://127.0.0.1:9");
    let schema = tool.input_schema();
    // Nothing is required: a call with only a step is a report.
    assert!(schema.get("required").is_none(), "{schema}");
    let properties = &schema["properties"];
    assert_eq!(
        properties["action"]["enum"],
        json!(["report", "get", "list"])
    );
    for param in [
        "step",
        "plan",
        "title",
        "ticket_id",
        "loop_id",
        "step_id",
        "plan_id",
        "percent",
        "remaining",
        "milestone",
        "links",
        "waiting",
        "status",
        "summary",
        "run_key",
        "run_id",
        "active",
        "limit",
        "workspace_id",
        "project_id",
    ] {
        assert!(properties.get(param).is_some(), "{param}");
    }
    assert_eq!(
        properties["status"]["enum"],
        json!([
            "working",
            "waiting_input",
            "waiting_approval",
            "completed",
            "partial",
            "stopped",
            "failed",
            "cancelled"
        ])
    );
    assert_eq!(
        properties["plan"]["items"]["properties"]["state"]["enum"],
        json!(["next", "in_progress", "done", "skipped"])
    );
    assert_eq!(
        properties["waiting"]["properties"]["on"]["enum"],
        json!([
            "person",
            "approval",
            "budget",
            "rate_limit",
            "sign_in",
            "dependency",
            "error"
        ])
    );
    let advertised = crate::portable_schema::portable_input_schema(&schema);
    assert!(
        crate::portable_schema::portable_schema_violations(&advertised).is_empty(),
        "{:?}",
        crate::portable_schema::portable_schema_violations(&advertised)
    );

    let metadata = tool.metadata();
    assert_eq!(metadata.name, "progress");
    // OpenAI-compatible endpoints refuse a longer description.
    assert!(
        metadata.description.chars().count() <= 1024,
        "{} characters",
        metadata.description.chars().count()
    );
    // The description is what makes an agent call it: it says that people
    // watch, and each moment to report.
    for said in [
        "People watch agents' runs",
        "when you start work on a ticket, a plan step or any multi-step task",
        "whenever your step or checklist changes",
        "when you open a PR, a preview or a deploy (links)",
        "when you are blocked or need a person (waiting)",
        "when you finish (status, with an honest summary",
        "Give percent only when it is real",
        "do not retry it",
    ] {
        assert!(metadata.description.contains(said), "{said}");
    }
    assert!(!metadata.annotations.read_only);
    assert!(!metadata.annotations.destructive);
    assert!(metadata.annotations.idempotent);
    assert!(!metadata.annotations.requires_confirmation);
    // The full reference rides on the action parameter.
    assert!(properties["action"]["description"]
        .as_str()
        .is_some_and(|text| text.contains("Full tool reference:") && text.contains("run_key")));
}

#[test]
fn a_report_is_labelled_for_the_write_latency_histogram_and_a_read_is_not() {
    for arguments in [json!({}), json!({"action": "report", "step": "x"})] {
        assert_eq!(
            crate::registry::tool_call_action_label("progress", &arguments),
            "report",
            "{arguments}"
        );
    }
    for arguments in [json!({"action": "get"}), json!({"action": "list"})] {
        assert_eq!(
            crate::registry::tool_call_action_label("progress", &arguments),
            "other",
            "{arguments}"
        );
    }
}
