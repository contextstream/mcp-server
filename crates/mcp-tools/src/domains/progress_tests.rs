//! The `progress` tool (contract v1.3, `run_progress.v1`) against a mock of
//! the API that records every request.

use super::*;
use crate::domains::parity_eval_tests::{client_and_session, text_of};
use crate::domains::plan_loops_tests::{Api, Seen};
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
            agent: agent_identity(
                Some("cinder".to_string()),
                Some("claude-code".to_string()),
                "sess",
            ),
            heartbeat_s: 30,
        }
    }
}

/// What the tool sent for the run's progress. After a recorded report (and a
/// `get` of the session's own run) it also reads what a person sent the run,
/// `GET /decision-requests/deliveries`, which this mock does not have and
/// answers with 404, as an API without the routes does. Those reads have
/// tests of their own (`progress_deliveries_tests.rs`); here they are left
/// out, so each test still counts the requests it is about.
fn progress_requests(api: &Api) -> Vec<Seen> {
    api.requests()
        .into_iter()
        .filter(|request| {
            const DELIVERIES: &str = "/api/v1/decision-requests/deliveries";
            let path = request.target.split('?').next().unwrap_or_default();
            if path == DELIVERIES || path.starts_with("/api/v1/decision-requests/deliveries/") {
                assert_eq!(request.method, "GET", "unexpected delivery write to {path}");
            }
            path != DELIVERIES
        })
        .collect()
}

// Prove the request checker cannot silently discard an unexpected mutation.
#[tokio::test]
#[should_panic(expected = "unexpected delivery write")]
async fn review10_progress_request_checks_reject_delivery_writes() {
    let api = Api::start(vec![]).await;
    mcp_client::http::client()
        .post(format!(
            "{}/api/v1/decision-requests/deliveries/{}/claim",
            api.base_url,
            Uuid::new_v4()
        ))
        .send()
        .await
        .unwrap();
    progress_requests(&api);
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

    let requests = progress_requests(&api);
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
    let requests = progress_requests(&api);
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
fn the_agent_is_named_by_its_handle_else_by_its_client_and_session() {
    let local = "3fa9c2d17e0b5a66aa11bb22cc33dd44";
    // A name the person configured is the agent, whatever the session.
    assert_eq!(
        agent_identity(
            Some("cc-qa".to_string()),
            Some("claude-code".to_string()),
            local
        ),
        json!({"name": "cc-qa", "runtime": "mcp", "client": "claude-code"})
    );
    assert_eq!(
        agent_identity(Some("cc-qa".to_string()), None, local),
        json!({"name": "cc-qa", "runtime": "mcp"})
    );
    // Otherwise it is this session of the client.
    assert_eq!(
        agent_identity(None, Some("claude-code".to_string()), local),
        json!({"id": "claude-code:3fa9c2d1", "name": "Claude Code 3fa9", "runtime": "mcp", "client": "claude-code"})
    );
    // The hosted gateway's session part carries a mark; the tag does not.
    assert_eq!(
        agent_identity(None, Some("codex-cli".to_string()), "s-77b2aa0011223344"),
        json!({"id": "codex-cli:77b2aa00", "name": "Codex 77b2", "runtime": "mcp", "client": "codex-cli"})
    );
    assert_eq!(
        agent_identity(None, None, "c-0123456789abcdef"),
        json!({"id": "mcp:01234567", "name": "MCP agent 0123", "runtime": "mcp"})
    );
    // Two sessions of one client are two agents; one session is always one.
    let other = agent_identity(None, Some("claude-code".to_string()), "77b2aa0011223344");
    let same = agent_identity(None, Some("claude-code".to_string()), local);
    assert_ne!(other["id"], same["id"]);
    assert_ne!(other["name"], same["name"]);
    assert_eq!(
        same,
        agent_identity(None, Some("claude-code".to_string()), local)
    );
    assert_eq!(
        same,
        agent_for_session(None, Some("claude-code".to_string()), local)
    );
    // Nothing stands for the session: the client's name alone.
    assert_eq!(
        agent_identity(None, Some("some-client".to_string()), "mcp"),
        json!({"name": "some-client", "runtime": "mcp", "client": "some-client"})
    );
    assert_eq!(
        agent_identity(None, None, "mcp"),
        json!({"name": "MCP agent", "runtime": "mcp"})
    );
    // A client is shown by its product name, or as it named itself.
    assert_eq!(shown_client_name("cursor-vscode"), "Cursor");
    assert_eq!(shown_client_name("Visual Studio Code"), "VS Code");
    assert_eq!(shown_client_name("my-own-client/1.0"), "my-own-client/1.0");
    // The default run key the hooks reuse is the tool's own.
    let ticket = Uuid::new_v4();
    assert_eq!(
        default_run_key(local, Some(ticket)),
        run_key_of(None, local, Some(ticket))
    );
    assert_eq!(default_run_key(local, None), format!("{local}:session"));
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
    assert!(
        progress_requests(&api).is_empty(),
        "{:?}",
        progress_requests(&api)
    );
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
    assert_eq!(
        progress_requests(&api).len(),
        1,
        "{:?}",
        progress_requests(&api)
    );

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
        assert_eq!(progress_requests(&api).len(), 1, "{status}");
    }
}

#[tokio::test]
async fn an_api_that_cannot_be_reached_is_answered_calmly() {
    // A port nothing listens on: the discard port, which no test can bind. A port freed by dropping a listener
    // could be taken by another test's mock server before the call, which then answers 404 (release run
    // 20261118T233600Z-5ccb).
    let fixture = Fixture::new();
    let tool = fixture.tool("http://127.0.0.1:9");
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
    assert!(
        progress_requests(&api).is_empty(),
        "{:?}",
        progress_requests(&api)
    );
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

    let requests = progress_requests(&api);
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
    let sent = progress_requests(&api).len();
    let last = api
        .requests()
        .last()
        .expect("the last request")
        .body
        .clone();
    assert_eq!(last["status"], "completed");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        progress_requests(&api).len(),
        sent,
        "no heartbeat after the run was finished: {:?}",
        progress_requests(&api)
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
    assert_eq!(
        progress_requests(&api).len(),
        2,
        "{:?}",
        progress_requests(&api)
    );
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
    // The call named no run, so the session's open runs were read first (the
    // mock has none to show); then the report, and nothing after it.
    let all = progress_requests(&api);
    assert_eq!(all.len(), 2, "no background request: {all:?}");
    assert!(
        all[0].target.starts_with("/api/v1/run-progress/runs?"),
        "{all:?}"
    );
    let requests = &all[1..];
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
    assert_eq!(
        progress_requests(&api).len(),
        2,
        "{:?}",
        progress_requests(&api)
    );
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

    let requests = progress_requests(&api);
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
    let requests = progress_requests(&api);
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
    assert_eq!(progress_requests(&api).len(), 1);
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

    let requests = progress_requests(&api);
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
        json!(["report", "get", "list", "reply", "decline"])
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

// ============================================================================
// One session, one agent; a report that names no run
// ============================================================================

const TOKEN_A: &str = "0123456789abcdef0123456789abcdef";
const TOKEN_B: &str = "fedcba9876543210fedcba9876543210";

/// What the gateway's middleware holds of a request that carried the API
/// key `key`.
fn key_auth(key: &str) -> mcp_types::AuthOverride {
    mcp_types::AuthOverride {
        api_key: Some(key.to_string()),
        ..Default::default()
    }
}

/// The user the API names for a credential in these tests: one a
/// credential.
fn user_of_credential(credential: mcp_client::SentCredential<'_>) -> Uuid {
    use sha2::{Digest, Sha256};

    let text = match credential {
        mcp_client::SentCredential::ApiKey(key) => key,
        mcp_client::SentCredential::Bearer(token) => token,
    };
    Uuid::from_slice(&Sha256::digest(text.as_bytes())[..16]).expect("sixteen bytes")
}

/// What the client tells the gateway (which has no key of its own to send)
/// of the API's answers to requests sent under `auth`: with 200, that the
/// API was asked whose the credential is and named a user; with 401, that it
/// refused the credential. Of any other answer the client tells nothing.
fn api_answers(auth: &mcp_types::AuthOverride, status: u16) {
    let mut config = TestFixtures::test_config();
    config.api_key = None;
    let credential =
        mcp_client::sent_credential(auth, &config).expect("a credential of the caller's");
    let answer = match status {
        200 => mcp_client::ApiAnswer::Names(user_of_credential(credential)),
        401 => mcp_client::ApiAnswer::Refuses,
        _ => return,
    };
    api_answered(credential, answer);
}

/// Whose the API said the key `key` is, as the gateway remembers it now.
fn accepted_user_of_key(key: &str) -> Option<String> {
    accepted()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .user_of(
            &credential_digest(mcp_client::SentCredential::ApiKey(key)),
            Instant::now(),
        )
        .map(str::to_string)
}

/// The API's answer to `GET /auth/me` for the user `user`.
fn me_answer(user: Uuid) -> Value {
    json!({"success": true, "data": {
        "id": user,
        "email": "qa@example.com",
        "created_at": "2026-01-01T00:00:00Z",
    }})
}

/// Run `work` as the hosted gateway runs a request of `caller` that carried
/// `token` in the header that names a connection. `caller` is the API key
/// the request carried, and the API has accepted it.
async fn hosted<T, F, Fut>(caller: Option<&str>, token: Option<&str>, work: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let caller = caller.map(str::to_string);
    let as_caller = || async move {
        match caller {
            Some(caller) => {
                let auth = key_auth(&caller);
                api_answers(&auth, 200);
                mcp_client::run_with_auth_override(auth, || {
                    mcp_client::run_with_caller_cache_identity(caller, work)
                })
                .await
            }
            None => work().await,
        }
    };
    match token {
        Some(token) => mcp_client::client_session::run_with(token.to_string(), as_caller).await,
        None => as_caller().await,
    }
}

async fn hosted_part(caller: Option<&str>, token: Option<&str>) -> String {
    hosted(caller, token, || async { session_part(true) }).await
}

#[tokio::test]
async fn a_stateless_client_that_names_its_connection_is_one_session_per_connection() {
    let one = hosted_part(Some("key-1"), Some(TOKEN_A)).await;
    assert!(one.starts_with("s-"), "{one}");
    assert_eq!(one.len(), 18);
    assert!(!one.contains(&TOKEN_A[..8]), "{one}");
    // The same connection is the same session on every request.
    assert_eq!(one, hosted_part(Some("key-1"), Some(TOKEN_A)).await);
    // Two connections of one key are two sessions, and two agents.
    let two = hosted_part(Some("key-1"), Some(TOKEN_B)).await;
    assert_ne!(one, two);
    let agent = |part: &str| agent_identity(None, Some("claude-code".to_string()), part);
    assert_ne!(agent(&one)["id"], agent(&two)["id"]);
    assert_ne!(agent(&one)["name"], agent(&two)["name"]);
    // The token alone names nothing: under another key it is another session,
    // so no key can reach a session of another by choosing its token.
    assert_ne!(one, hosted_part(Some("key-2"), Some(TOKEN_A)).await);
    // Without a caller the token is not used at all.
    assert_eq!(hosted_part(None, Some(TOKEN_A)).await, "mcp");
    // A request without the header is named exactly as before.
    let by_key = hosted_part(Some("key-1"), None).await;
    assert!(by_key.starts_with("c-"), "{by_key}");
    assert_eq!(
        by_key,
        mcp_client::run_with_caller_cache_identity("key-1".to_string(), || async {
            session_part(true)
        })
        .await
    );
    assert_ne!(by_key[2..], one[2..]);
    // A transport that has a session id of its own is named by that id,
    // bound to the caller as the token is; the token is not used then.
    let by_transport = mcp_client::run_with_mcp_session_id("mcp-session-1".to_string(), || {
        hosted_part(Some("key-1"), Some(TOKEN_A))
    })
    .await;
    let transport_without_token =
        mcp_client::run_with_mcp_session_id("mcp-session-1".to_string(), || {
            hosted_part(Some("key-1"), None)
        })
        .await;
    assert_eq!(by_transport, transport_without_token);
    assert_ne!(by_transport, one);
    let other_key = mcp_client::run_with_mcp_session_id("mcp-session-1".to_string(), || {
        hosted_part(Some("key-2"), None)
    })
    .await;
    assert_ne!(by_transport, other_key);
}

#[test]
fn only_a_session_part_that_is_one_session_has_one_open_run() {
    assert!(is_one_session("3f2b0c9a4d5e4f6a8b7c6d5e4f3a2b1c", false));
    assert!(is_one_session("s-0123456789abcdef", true));
    assert!(!is_one_session("c-0123456789abcdef", true));
    assert!(!is_one_session("mcp", true));
}

#[test]
fn a_report_names_a_run_by_its_ticket_or_its_key() {
    assert!(names_no_run(&input(json!({"step": "Reading"}))));
    assert!(names_no_run(&input(
        json!({"step": "Reading", "run_key": "  ", "ticket_id": ""})
    )));
    assert!(!names_no_run(&input(json!({"run_key": "reviewer-2"}))));
    assert!(!names_no_run(&input(json!({"ticket_id": Uuid::new_v4()}))));
}

#[tokio::test]
async fn on_a_local_server_a_report_that_names_no_run_goes_to_the_one_open_run() {
    let fixture = Fixture::new();
    let api = Api::start(vec![(
        "POST /api/v1/run-progress/report",
        200,
        fixture.answer("working", &[]),
    )])
    .await;
    let tool = fixture.tool(&api.base_url).with_heartbeat(Heartbeat::None);
    let sent = |index: usize| progress_requests(&api)[index].body.clone();

    // The session takes up a ticket: one run is open.
    tool.execute(json!({"step": "Reading the parser", "ticket_id": fixture.ticket}))
        .await
        .expect("report");
    let on_ticket = sent(0)["source_run_id"]
        .as_str()
        .expect("a key")
        .to_string();
    assert!(
        on_ticket.ends_with(&format!(":{}", fixture.ticket)),
        "{on_ticket}"
    );

    // Its question names no run: it is that run's question, and the answer
    // says where it went.
    let result = tool
        .execute(json!({"waiting": {"on": "person", "question": "Go ahead?"}}))
        .await
        .expect("report");
    assert_eq!(sent(1)["source_run_id"], json!(on_ticket));
    assert!(sent(1).get("run_id").is_none(), "{}", sent(1));
    assert_eq!(sent(1)["status"], "waiting_input");
    assert!(text_of(&result).contains(SENT_ON), "{}", text_of(&result));
    assert_eq!(
        result.structured_content.expect("structured")["run_key"],
        json!(on_ticket)
    );

    // A report that names a run goes where it says, open run or not.
    let other = Uuid::new_v4();
    let result = tool
        .execute(json!({"step": "Another ticket", "ticket_id": other}))
        .await
        .expect("report");
    assert!(sent(2)["source_run_id"]
        .as_str()
        .unwrap()
        .ends_with(&format!(":{other}")));
    assert!(!text_of(&result).contains(SENT_ON));

    // Two runs are open now: a report that names none goes to the session's
    // default run, as it always did.
    let result = tool
        .execute(json!({"step": "Which one?"}))
        .await
        .expect("report");
    assert!(sent(3)["source_run_id"]
        .as_str()
        .unwrap()
        .ends_with(":session"));
    assert!(!text_of(&result).contains(SENT_ON));
}

#[tokio::test]
async fn on_a_local_server_no_open_run_means_the_default_run() {
    let fixture = Fixture::new();
    let api = Api::start(vec![(
        "POST /api/v1/run-progress/report",
        200,
        fixture.answer("working", &[]),
    )])
    .await;
    let tool = fixture.tool(&api.base_url).with_heartbeat(Heartbeat::None);
    let key = |index: usize| {
        progress_requests(&api)[index].body["source_run_id"]
            .as_str()
            .expect("a key")
            .to_string()
    };
    // Nothing reported yet.
    tool.execute(json!({"step": "Looking around"}))
        .await
        .expect("report");
    assert!(key(0).ends_with(":session"), "{}", key(0));
    // The default run is the one open run: the next goes there too, unsaid.
    let result = tool
        .execute(json!({"step": "Still looking"}))
        .await
        .expect("report");
    assert_eq!(key(1), key(0));
    assert!(!text_of(&result).contains(SENT_ON));
    tool.execute(json!({"status": "completed", "summary": "Looked."}))
        .await
        .expect("finish");
    assert_eq!(key(2), key(0));

    // A ticket's run, opened and finished: nothing is open afterwards.
    tool.execute(json!({"step": "Fixing", "ticket_id": fixture.ticket}))
        .await
        .expect("report");
    tool.execute(json!({"status": "completed", "summary": "Fixed.", "ticket_id": fixture.ticket}))
        .await
        .expect("finish");
    tool.execute(json!({"step": "Afterwards"}))
        .await
        .expect("report");
    assert!(key(5).ends_with(":session"), "{}", key(5));
}

/// The open runs a mock API lists, each under `agent_id`, the first on a ticket.
fn open_runs(fixture: &Fixture, agent_ids: &[&str]) -> Value {
    let runs: Vec<Value> = agent_ids
        .iter()
        .enumerate()
        .map(|(index, agent_id)| {
            let mut view = fixture.view("working");
            view["run_id"] = json!(Uuid::from_u128(index as u128 + 1));
            view["agent"] = json!({"id": agent_id, "name": "Claude Code", "runtime": "mcp"});
            view["scope"]["ticket_id"] = if index == 0 {
                json!(fixture.ticket)
            } else {
                Value::Null
            };
            view
        })
        .collect();
    json!({"success": true, "data": {"runs": runs}})
}

/// The agent ids of the workspace's open runs, given this session's own.
type Listed<'a> = &'a dyn Fn(&str) -> Vec<String>;

/// One report through the hosted gateway: the targets of the requests it
/// made, the report's body, the answer's text and its structured part.
struct HostedReport {
    targets: Vec<String>,
    report: Value,
    text: String,
    answer: Value,
}

/// What the hosted gateway sent for one report, when the API lists `listed`
/// as the workspace's open runs (`None`: it cannot say).
async fn hosted_report(
    fixture: &Fixture,
    token: Option<&str>,
    listed: Option<Listed<'_>>,
    arguments: Value,
) -> HostedReport {
    let me = hosted(Some("key-1"), token, || async {
        agent_identity(
            None,
            crate::client_identity::current_client_name(),
            &session_part(true),
        )
    })
    .await;
    let my_id = me["id"].as_str().expect("an agent id").to_string();
    let mut routes = vec![(
        "POST /api/v1/run-progress/report",
        200,
        fixture.answer("working", &["run.step"]),
    )];
    if let Some(listed) = listed {
        let ids = listed(&my_id);
        let ids: Vec<&str> = ids.iter().map(String::as_str).collect();
        routes.push((
            "GET /api/v1/run-progress/runs?",
            200,
            open_runs(fixture, &ids),
        ));
    }
    let api = Api::start(routes).await;
    let tool = fixture.hosted_tool(&api.base_url);
    let result = hosted(Some("key-1"), token, || async {
        tool.execute(arguments).await
    })
    .await
    .expect("report");
    let requests = progress_requests(&api);
    let targets = requests.iter().map(|seen| seen.target.clone()).collect();
    let report = requests.last().expect("the report").body.clone();
    HostedReport {
        targets,
        report,
        text: text_of(&result),
        answer: result.structured_content.unwrap_or(Value::Null),
    }
}

#[tokio::test]
async fn on_the_hosted_gateway_a_report_that_names_no_run_goes_to_the_sessions_one_open_run() {
    let fixture = Fixture::new();
    // One open run of this session, and one of somebody else.
    let listed = |me: &str| vec![me.to_string(), "claude-code:0badc0de".to_string()];
    let HostedReport {
        targets,
        report,
        text,
        answer,
    } = hosted_report(
        &fixture,
        Some(TOKEN_A),
        Some(&listed),
        json!({"waiting": {"on": "person", "question": "Go ahead?"}}),
    )
    .await;
    assert_eq!(targets.len(), 2, "{targets:?}");
    assert!(targets[0].contains("active=true"), "{targets:?}");
    assert!(
        targets[0].contains(&format!("workspace_id={}", fixture.workspace)),
        "{targets:?}"
    );
    // Sent by the run's id: the gateway cannot know the key it was opened under.
    assert_eq!(report["run_id"], json!(Uuid::from_u128(1)));
    assert!(report.get("source_run_id").is_none(), "{report}");
    assert_eq!(report["status"], "waiting_input");
    assert!(text.contains(SENT_ON), "{text}");
    assert_eq!(answer["notes"], json!([SENT_ON]), "{answer}");
    assert!(answer.get("run_key").is_none(), "{answer}");
    assert!(answer["session"].as_str().unwrap().starts_with("s-"));
}

#[tokio::test]
async fn on_the_hosted_gateway_none_or_several_open_runs_mean_the_default_run() {
    let fixture = Fixture::new();
    let step = || json!({"step": "Running the tests"});
    let default_run = |report: &Value, text: &str| {
        let key = report["source_run_id"].as_str().expect("a key");
        assert!(key.starts_with("s-") && key.ends_with(":session"), "{key}");
        assert!(report.get("run_id").is_none(), "{report}");
        assert!(!text.contains(SENT_ON), "{text}");
    };
    let no_notes = |answer: &Value| assert!(answer.get("notes").is_none(), "{answer}");
    // None of this session's.
    let nobody = |_: &str| vec!["claude-code:0badc0de".to_string()];
    let HostedReport {
        report,
        text,
        answer,
        ..
    } = hosted_report(&fixture, Some(TOKEN_A), Some(&nobody), step()).await;
    default_run(&report, &text);
    no_notes(&answer);
    assert_eq!(answer["run_key"], report["source_run_id"]);
    // Two of this session's.
    let two = |me: &str| vec![me.to_string(), me.to_string()];
    let HostedReport { report, text, .. } =
        hosted_report(&fixture, Some(TOKEN_A), Some(&two), step()).await;
    default_run(&report, &text);
    // The API cannot say (an older API, or it is away): as before.
    let HostedReport {
        targets,
        report,
        text,
        ..
    } = hosted_report(&fixture, Some(TOKEN_A), None, step()).await;
    assert_eq!(targets.len(), 2, "{targets:?}");
    default_run(&report, &text);
}

#[tokio::test]
async fn the_hosted_gateway_asks_for_open_runs_only_when_it_could_use_the_answer() {
    let fixture = Fixture::new();
    let mine = |me: &str| vec![me.to_string()];
    // A client that names no connection is every session of its key: its
    // one open run may be another session's. Nothing is read, nothing moves.
    let HostedReport {
        targets,
        report,
        text,
        ..
    } = hosted_report(
        &fixture,
        None,
        Some(&mine),
        json!({"step": "Running the tests"}),
    )
    .await;
    assert_eq!(targets, ["/api/v1/run-progress/report"]);
    let key = report["source_run_id"].as_str().expect("a key");
    assert!(key.starts_with("c-") && key.ends_with(":session"), "{key}");
    assert!(!text.contains(SENT_ON));
    // A report that names its run is sent there, unread.
    let HostedReport {
        targets, report, ..
    } = hosted_report(
        &fixture,
        Some(TOKEN_A),
        Some(&mine),
        json!({"step": "Running the tests", "ticket_id": fixture.ticket}),
    )
    .await;
    assert_eq!(targets, ["/api/v1/run-progress/report"]);
    assert!(report["source_run_id"]
        .as_str()
        .unwrap()
        .ends_with(&format!(":{}", fixture.ticket)));
    let HostedReport {
        targets, report, ..
    } = hosted_report(
        &fixture,
        Some(TOKEN_A),
        Some(&mine),
        json!({"step": "Running the tests", "run_key": "reviewer-2"}),
    )
    .await;
    assert_eq!(targets, ["/api/v1/run-progress/report"]);
    assert_eq!(report["source_run_id"], "reviewer-2");
}

#[test]
fn a_report_sent_on_says_so_only_when_it_went_somewhere_else() {
    let fixture = Fixture::new();
    let mut report = report_of(&fixture, json!({"step": "Reading"}));
    let default_key = report.run_key.clone();
    report.address(RunTarget::Key(default_key.clone()));
    assert_eq!(report.run_key, default_key);
    assert!(report.notes.is_empty(), "{:?}", report.notes);
    assert!(!report.by_run_id);

    report.address(RunTarget::Key("sess:another".to_string()));
    assert_eq!(report.run_key, "sess:another");
    assert_eq!(report.body["source_run_id"], "sess:another");
    assert_eq!(report.notes, [SENT_ON]);

    let mut report = report_of(&fixture, json!({"step": "Reading"}));
    report.address(RunTarget::Id {
        run_id: fixture.run.to_string(),
        on_a_ticket: false,
    });
    assert!(report.by_run_id);
    assert_eq!(report.body["run_id"], json!(fixture.run));
    assert!(report.body.get("source_run_id").is_none());
    assert!(report.notes.is_empty(), "{:?}", report.notes);
}

// ============================================================================
// The agent's name on the hosted gateway
// ============================================================================

/// Who calls the hosted gateway: the key, the token that names the
/// connection, and the agent name the client sent.
#[derive(Clone, Copy)]
struct Caller<'a> {
    key: Option<&'a str>,
    token: Option<&'a str>,
    name: Option<&'a str>,
}

const SESSION_A: Caller<'static> = Caller {
    key: Some("key-1"),
    token: Some(TOKEN_A),
    name: None,
};

/// Run `work` as the hosted gateway runs a tool call of `caller` from Claude
/// Code.
async fn as_caller<T, F, Fut>(caller: Caller<'_>, work: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let name = caller.name.map(str::to_string);
    hosted(caller.key, caller.token, || async move {
        crate::client_identity::run_with_client_name(Some("claude-code".to_string()), async move {
            match name {
                Some(name) => mcp_client::agent_handle::run_with_header_handle(name, work).await,
                None => work().await,
            }
        })
        .await
    })
    .await
}

/// The agent a hosted report of `caller` is sent under.
async fn reported_agent(caller: Caller<'_>) -> Value {
    let fixture = Fixture::new();
    let api = Api::start(vec![(
        "POST /api/v1/run-progress/report",
        200,
        fixture.answer("working", &["run.step"]),
    )])
    .await;
    let tool = fixture.hosted_tool(&api.base_url);
    as_caller(caller, || async {
        tool.execute(json!({"step": "Running the tests", "run_key": "floor-live-runs"}))
            .await
    })
    .await
    .expect("report");
    let requests = progress_requests(&api);
    assert_eq!(requests.len(), 1, "{requests:?}");
    requests[0].body["agent"].clone()
}

/// The hosted gateway's client and session, against a mock API.
fn hosted_client(fixture: &Fixture, base_url: &str) -> (ContextStreamClient, Arc<SessionManager>) {
    let mut config = TestFixtures::test_config();
    config.api_url = base_url.to_string();
    config.default_workspace_id = Some(fixture.workspace);
    config.default_project_id = Some(fixture.project);
    config.is_http_transport = true;
    let client = ContextStreamClient::new(config.clone());
    let session = Arc::new(SessionManager::new(client.clone(), config));
    (client, session)
}

#[tokio::test]
async fn a_client_that_sends_neither_header_keeps_todays_name_and_id() {
    // Today's rule, spelt out: the client's product name and four characters
    // of the session part; in the id, the client and eight.
    let today = |part: &str| {
        json!({
            "id": format!("claude-code:{}", &part[2..10]),
            "name": format!("Claude Code {}", &part[2..6]),
            "runtime": "mcp",
            "client": "claude-code",
        })
    };
    let by_key = Caller {
        key: Some("key-1"),
        token: None,
        name: None,
    };
    let part = hosted_part(Some("key-1"), None).await;
    assert!(part.starts_with("c-"), "{part}");
    assert_eq!(reported_agent(by_key).await, today(&part));
    let part = hosted_part(Some("key-1"), Some(TOKEN_A)).await;
    assert!(part.starts_with("s-"), "{part}");
    assert_eq!(reported_agent(SESSION_A).await, today(&part));
    // What init tells the hooks is the same agent.
    let fixture = Fixture::new();
    let (client, session) = hosted_client(&fixture, "http://127.0.0.1:9");
    let identity = as_caller(SESSION_A, || async {
        session_identity(&client, &session).await
    })
    .await
    .expect("an identity");
    assert_eq!(identity["agent"], today(&part));
}

#[tokio::test]
async fn a_hosted_client_that_names_its_agent_reports_under_that_name() {
    let lane = |name, token| Caller {
        key: Some("key-1"),
        token: Some(token),
        name: Some(name),
    };
    // The name is the agent, as a local server's CONTEXTSTREAM_AGENT_NAME is.
    assert_eq!(
        reported_agent(lane("floor-live-runs", TOKEN_A)).await,
        json!({"name": "floor-live-runs", "runtime": "mcp", "client": "claude-code"})
    );
    // Two lanes of one key are two names.
    assert_eq!(
        reported_agent(lane("floor-alive-4", TOKEN_B)).await["name"],
        "floor-alive-4"
    );
    // init tells the hooks the same name.
    let fixture = Fixture::new();
    let (client, session) = hosted_client(&fixture, "http://127.0.0.1:9");
    let identity = as_caller(lane("floor-live-runs", TOKEN_A), || async {
        session_identity(&client, &session).await
    })
    .await
    .expect("an identity");
    assert_eq!(identity["agent"]["name"], "floor-live-runs");
}

// ============================================================================
// A sign of life on the hosted gateway
// ============================================================================

/// A run the mock API lists: its agent, how it reads, and how many minutes
/// ago its agent last reported on it.
fn listed_run(
    fixture: &Fixture,
    index: u128,
    agent_id: &str,
    liveness: &str,
    minutes_ago: i64,
) -> Value {
    let mut view = fixture.view(match liveness {
        "waiting" => "waiting_input",
        "stalled" => "stalled",
        _ => "working",
    });
    view["run_id"] = json!(Uuid::from_u128(index));
    view["liveness"] = json!(liveness);
    view["agent"] = json!({"id": agent_id, "name": "Claude Code 3fa9", "runtime": "mcp", "client": "claude-code"});
    view["last_event_at"] =
        json!((chrono::Utc::now() - chrono::Duration::minutes(minutes_ago)).to_rfc3339());
    view
}

/// The agent id the runs of `caller` carry.
async fn agent_id_of(caller: Caller<'_>) -> String {
    as_caller(caller, || async {
        let agent = agent_identity(
            mcp_client::agent_handle::agent_handle_of_caller(None),
            crate::client_identity::current_client_name(),
            &session_part(true),
        );
        agent["id"]
            .as_str()
            .or_else(|| agent["name"].as_str())
            .expect("an agent")
            .to_string()
    })
    .await
}

/// One `tool` call of `caller` through the hosted gateway, with `listed` as
/// the workspace's open runs and `workspaces` as the gateway's setting:
/// whether a sign of life went out, and every request it made (target, body).
async fn sign_for(
    fixture: &Fixture,
    caller: Caller<'_>,
    workspaces: &[Uuid],
    tool: &str,
    listed: Vec<Value>,
) -> (bool, Vec<(String, Value)>) {
    let params = json!({"name": tool, "arguments": {"query": "anything"}});
    sign_for_call(fixture, caller, workspaces, params, listed).await
}

/// [`sign_for`] for a whole `tools/call` (`params`).
async fn sign_for_call(
    fixture: &Fixture,
    caller: Caller<'_>,
    workspaces: &[Uuid],
    params: Value,
    listed: Vec<Value>,
) -> (bool, Vec<(String, Value)>) {
    let api = Api::start(routes_knowing(fixture, &listed)).await;
    let (client, session) = hosted_client(fixture, &api.base_url);
    let sent = as_caller(caller, || async {
        sign_of_life_for(&client, &session, &params, workspaces).await
    })
    .await;
    let went = sent.is_some();
    if let Some(task) = sent {
        task.await.expect("the sign of life finishes");
    }
    let requests = api
        .requests()
        .into_iter()
        .map(|seen| (seen.target, seen.body))
        .collect();
    (went, requests)
}

#[tokio::test]
async fn a_tool_call_keeps_the_sessions_own_run_alive_where_the_gateway_is_told_to() {
    // The session's own run (its reports that name none), by how the API
    // answers it, and whether a tool call of the session signals it.
    let cases = [
        ("live", 5, true),
        ("stalled", 20, true),
        ("quiet", 4 * 60 - 5, true),
        ("waiting", 5, false),
        ("ended", 5, false),
        ("quiet", 4 * 60 + 5, false),
    ];
    for (liveness, minutes_ago, signalled) in cases {
        let fixture = Fixture::new();
        let mut own = reported_run(&fixture, SESSION_A, minutes_ago).await;
        own["liveness"] = json!(liveness);
        if liveness == "waiting" {
            own["waiting"] = json!({"on": "person"});
        }
        let others = listed_run(&fixture, 3, "claude-code:0badc0de", "live", 5);
        let (went, requests) = sign_for(
            &fixture,
            SESSION_A,
            &[fixture.workspace],
            "search",
            vec![own.clone(), others],
        )
        .await;
        assert!(went, "{liveness} {minutes_ago}");
        // One read: that run, by its id, in the call's workspace. Nothing
        // is listed, and no other run is read.
        assert_eq!(
            requests[0].0,
            format!(
                "/api/v1/run-progress/runs/{}?workspace_id={}",
                own["run_id"].as_str().expect("a run id"),
                fixture.workspace
            )
        );
        if signalled {
            // Who and which run and nothing about the work, under the run's
            // own agent, so nothing is renamed.
            assert_eq!(requests.len(), 2, "{liveness} {minutes_ago}: {requests:?}");
            assert_eq!(
                reports_in(&requests),
                vec![&json!({
                    "workspace_id": fixture.workspace,
                    "run_id": own["run_id"],
                    "agent": own["agent"],
                })]
            );
        } else {
            assert_eq!(requests.len(), 1, "{liveness} {minutes_ago}: {requests:?}");
        }
        // At most once a minute for one session: the next call sends nothing.
        let (went, requests) = sign_for(
            &fixture,
            SESSION_A,
            &[fixture.workspace],
            "memory",
            vec![own],
        )
        .await;
        assert!(!went && requests.is_empty(), "{requests:?}");
    }
}

#[tokio::test]
async fn a_workspace_not_on_the_list_sees_exactly_todays_behaviour() {
    let fixture = Fixture::new();
    let me = agent_id_of(SESSION_A).await;
    // The setting unset (as shipped), or naming other workspaces only: not a
    // single request, whatever the session's runs.
    for workspaces in [vec![], vec![Uuid::new_v4(), Uuid::new_v4()]] {
        let (went, requests) = sign_for(
            &fixture,
            SESSION_A,
            &workspaces,
            "search",
            vec![listed_run(&fixture, 1, &me, "live", 5)],
        )
        .await;
        assert!(!went && requests.is_empty(), "{workspaces:?}: {requests:?}");
    }
    // The setting: a list of ids, split by commas or white space.
    assert_eq!(parse_workspace_list(None), Vec::<Uuid>::new());
    assert_eq!(parse_workspace_list(Some("  ")), Vec::<Uuid>::new());
    let other = Uuid::new_v4();
    let here = fixture.workspace;
    for list in [
        format!("{other},{here}"),
        format!(" {other} ,\n{here},, "),
        format!("{other} {here}"),
    ] {
        assert_eq!(
            parse_workspace_list(Some(&list)),
            vec![other, here],
            "{list:?}"
        );
    }
    // Anything in it that is not a workspace id, and the whole list names
    // nothing: a mistyped setting turns the signs off, never half on.
    for list in [
        format!(" {other}, nope ,{here}"),
        format!("{here},bad"),
        format!("{here},{}", Uuid::nil()),
        format!("{here};{other}"),
        format!("[\"{here}\"]"),
        format!("{here},*"),
        format!("{here},all"),
        format!("{here}x"),
    ] {
        assert_eq!(
            parse_workspace_list(Some(&list)),
            Vec::<Uuid>::new(),
            "{list:?}"
        );
    }
    // A local server never signs: it has its own heartbeat.
    let api = Api::start(vec![]).await;
    let (client, session) =
        client_and_session(&api.base_url, fixture.workspace, Some(fixture.project));
    let params = json!({"name": "search", "arguments": {}});
    let local = as_caller(SESSION_A, || async {
        sign_of_life_for(&client, &session, &params, &[fixture.workspace]).await
    })
    .await;
    assert!(local.is_none());
    assert!(progress_requests(&api).is_empty());
}

#[tokio::test]
async fn a_session_the_gateway_cannot_tell_apart_gives_no_sign_of_life() {
    let fixture = Fixture::new();
    let here = [fixture.workspace];
    // Every session of a key that names no connection looks alike: a call
    // may be another session's.
    let by_key = Caller {
        key: Some("key-1"),
        token: None,
        name: None,
    };
    let me = agent_id_of(by_key).await;
    let (went, requests) = sign_for(
        &fixture,
        by_key,
        &here,
        "search",
        vec![listed_run(&fixture, 1, &me, "live", 5)],
    )
    .await;
    assert!(!went && requests.is_empty(), "{requests:?}");
    // No caller (no credential): nothing.
    let nobody = Caller {
        key: None,
        token: Some(TOKEN_A),
        name: None,
    };
    let (went, requests) = sign_for(&fixture, nobody, &here, "search", vec![]).await;
    assert!(!went && requests.is_empty(), "{requests:?}");
    // A report is a signal of its own.
    let (went, requests) = sign_for(&fixture, SESSION_A, &here, "progress", vec![]).await;
    assert!(!went && requests.is_empty(), "{requests:?}");
    // A name its client sent says nothing of the session: any session of
    // any key may send it.
    let named = Caller {
        key: Some("key-1"),
        token: None,
        name: Some("floor-live-runs"),
    };
    let (went, requests) = sign_for(
        &fixture,
        named,
        &here,
        "search",
        vec![
            listed_run(&fixture, 1, "floor-live-runs", "live", 5),
            listed_run(&fixture, 2, &me, "live", 5),
        ],
    )
    .await;
    assert!(!went && requests.is_empty(), "{requests:?}");
}

// ============================================================================
// A sign only for this session's own run (review of 89c632e: MUST 1, SHOULD 2
// and 3; the reviewer's two failing tests are the first two)
// ============================================================================

/// A hosted report of `caller` that names no run: the body the API received.
async fn review_report_body(fixture: &Fixture, caller: Caller<'_>) -> Value {
    let api = Api::start(vec![
        (
            "GET /api/v1/run-progress/runs?",
            200,
            json!({"success": true, "data": {"runs": []}}),
        ),
        (
            "POST /api/v1/run-progress/report",
            200,
            fixture.answer("working", &["run.step"]),
        ),
    ])
    .await;
    let tool = fixture.hosted_tool(&api.base_url);
    as_caller(caller, || async {
        tool.execute(json!({"step": "Running the tests"})).await
    })
    .await
    .expect("report");
    progress_requests(&api)
        .into_iter()
        .find(|seen| seen.target.ends_with("/run-progress/report"))
        .expect("a report")
        .body
}

/// The run a report of `caller` that names no run made, as the API keeps it:
/// the id the API gives it (`uuid5(workspace, "mcp:" + source_run_id)`,
/// contextstream `progress/report.rs`) and the agent as sent (an agent with
/// no id is listed under its name).
async fn reported_run(fixture: &Fixture, caller: Caller<'_>, minutes_ago: i64) -> Value {
    let report = review_report_body(fixture, caller).await;
    let mut agent = report["agent"].clone();
    if agent.get("id").is_none() {
        agent["id"] = agent["name"].clone();
    }
    let id = agent["id"].as_str().expect("an agent id").to_string();
    let mut run = listed_run(fixture, 0, &id, "live", minutes_ago);
    let key = report["source_run_id"].as_str().expect("a run key");
    run["run_id"] = json!(run_id_of_key(fixture.workspace, key));
    run["agent"] = agent;
    run
}

fn reports_in(requests: &[(String, Value)]) -> Vec<&Value> {
    requests
        .iter()
        .filter(|(target, _)| target.ends_with("/run-progress/report"))
        .map(|(_, body)| body)
        .collect()
}

/// The routes of a mock API that knows the runs `known`: each by its id,
/// all of them in a list, and a report route.
fn routes_knowing(fixture: &Fixture, known: &[Value]) -> Vec<(&'static str, u16, Value)> {
    let mut routes = vec![
        (
            "GET /api/v1/run-progress/runs?",
            200,
            json!({"success": true, "data": {"runs": known}}),
        ),
        (
            "POST /api/v1/run-progress/report",
            200,
            fixture.answer("working", &[]),
        ),
    ];
    for view in known {
        if let Some(id) = view["run_id"].as_str() {
            let route: &'static str =
                Box::leak(format!("GET /api/v1/run-progress/runs/{id}").into_boxed_str());
            routes.push((route, 200, json!({"success": true, "data": view})));
        }
    }
    routes
}

/// One tool call each of `callers`, in turn, through one hosted gateway
/// client against one mock API that knows the runs `known`: how many signs
/// went out, and every request they made (target, body).
async fn signs_for_calls(
    fixture: &Fixture,
    callers: &[Caller<'_>],
    known: Vec<Value>,
) -> (usize, Vec<(String, Value)>) {
    let api = Api::start(routes_knowing(fixture, &known)).await;
    let (client, session) = hosted_client(fixture, &api.base_url);
    let params = json!({"name": "search", "arguments": {"query": "anything"}});
    let mut went = 0;
    for caller in callers {
        let sent = as_caller(*caller, || async {
            sign_of_life_for(&client, &session, &params, &[fixture.workspace]).await
        })
        .await;
        if let Some(task) = sent {
            went += 1;
            task.await.expect("the sign of life finishes");
        }
    }
    let requests = api
        .requests()
        .into_iter()
        .map(|seen| (seen.target, seen.body))
        .collect();
    (went, requests)
}

/// B1/A2. A tool call is a sign of life for THAT session's run, and a dead
/// session's run goes quiet at the old time, whatever name both sessions
/// send.
#[tokio::test]
async fn review_a_dead_named_sessions_run_gets_no_sign_from_another_session_of_that_name() {
    let fixture = Fixture::new();
    let here = [fixture.workspace];
    let dead = Caller {
        key: Some("key-1"),
        token: Some(TOKEN_A),
        name: Some("floor-live-runs"),
    };
    let alive = Caller {
        key: Some("key-1"),
        token: Some(TOKEN_B),
        name: Some("floor-live-runs"),
    };
    // The first session reported, then died five minutes ago; its run is
    // still open.
    let dead_run = reported_run(&fixture, dead, 5).await;
    let (went, requests) = sign_for(&fixture, alive, &here, "search", vec![dead_run.clone()]).await;
    let reports = reports_in(&requests);
    assert!(
        reports.is_empty(),
        "went={went}: the live session's tool call kept the dead session's run alive: {reports:?}"
    );
    // The run is the dead session's own: its call would have signalled it.
    let (went, requests) = sign_for(&fixture, dead, &here, "search", vec![dead_run.clone()]).await;
    let reports = reports_in(&requests);
    assert!(went, "{requests:?}");
    assert_eq!(reports.len(), 1, "{requests:?}");
    assert_eq!(reports[0]["run_id"], dead_run["run_id"]);
}

/// A2. The same across keys: another key that sends the same name.
#[tokio::test]
async fn review_another_keys_tool_call_gives_no_sign_for_a_named_agents_run() {
    let fixture = Fixture::new();
    let here = [fixture.workspace];
    let owner = Caller {
        key: Some("key-1"),
        token: Some(TOKEN_A),
        name: Some("floor-live-runs"),
    };
    let other_key = Caller {
        key: Some("key-2"),
        token: None,
        name: Some("floor-live-runs"),
    };
    let owners_run = reported_run(&fixture, owner, 5).await;
    let (went, requests) = sign_for(&fixture, other_key, &here, "search", vec![owners_run]).await;
    let reports = reports_in(&requests);
    assert!(
        reports.is_empty(),
        "went={went}: key-2 kept key-1's run alive by sending its name: {reports:?}"
    );
}

/// A named caller the gateway cannot tell from the key's other sessions (no
/// `s-` session part) gets no sign: a name alone is never enough.
#[tokio::test]
async fn a_named_caller_with_no_session_part_gets_no_sign() {
    let fixture = Fixture::new();
    let here = [fixture.workspace];
    let lane = Caller {
        key: Some("key-1"),
        token: Some(TOKEN_A),
        name: Some("floor-live-runs"),
    };
    let named_only = Caller {
        key: Some("key-1"),
        token: None,
        name: Some("floor-live-runs"),
    };
    let known = vec![
        reported_run(&fixture, lane, 5).await,
        reported_run(&fixture, named_only, 5).await,
        listed_run(&fixture, 7, "floor-live-runs", "live", 5),
    ];
    let (went, requests) = sign_for(&fixture, named_only, &here, "search", known).await;
    assert!(
        !went && requests.is_empty(),
        "went={went}: a name alone was taken for a session: {requests:?}"
    );
}

/// The session part a legacy (stateful) request of `key` that presents
/// `session_id` gets.
async fn legacy_part(key: &str, session_id: &str) -> String {
    mcp_client::run_with_mcp_session_id(session_id.to_string(), || hosted_part(Some(key), None))
        .await
}

/// Run `work` as a legacy request that presents the session id
/// `mcp-session-1`.
async fn in_session<T, F, Fut>(work: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    mcp_client::run_with_mcp_session_id("mcp-session-1".to_string(), work).await
}

/// On the legacy contract the session id is the client's to send: the part
/// is bound to the caller, so another key that presents the same id is
/// another session, and its tool calls give the first key's run no sign.
#[tokio::test]
async fn the_legacy_session_part_is_bound_to_the_caller() {
    let one = legacy_part("key-1", "mcp-session-1").await;
    assert!(one.starts_with("s-") && one.len() == 18, "{one}");
    assert_eq!(one, legacy_part("key-1", "mcp-session-1").await);
    let other = legacy_part("key-2", "mcp-session-1").await;
    assert_ne!(
        one, other,
        "another key with the same session id is another session"
    );

    let fixture = Fixture::new();
    let here = [fixture.workspace];
    let key_1 = Caller {
        key: Some("key-1"),
        token: None,
        name: None,
    };
    let key_2 = Caller {
        key: Some("key-2"),
        token: None,
        name: None,
    };
    let run = in_session(|| reported_run(&fixture, key_1, 5)).await;
    let (went, requests) =
        in_session(|| sign_for(&fixture, key_2, &here, "search", vec![run.clone()])).await;
    let reports = reports_in(&requests);
    assert!(
        reports.is_empty(),
        "went={went}: key-2 kept key-1's run alive with key-1's session id: {reports:?}"
    );
    // Key-1's own call in its session signals it.
    let (went, requests) =
        in_session(|| sign_for(&fixture, key_1, &here, "search", vec![run.clone()])).await;
    let reports = reports_in(&requests);
    assert!(went, "{requests:?}");
    assert_eq!(reports.len(), 1, "{requests:?}");
    assert_eq!(reports[0]["run_id"], run["run_id"]);
}

/// SHOULD 2. A client that sends a new connection token on every call is a
/// new session each time; one key's calls still make at most the allowed
/// signs a minute, and a run is signalled at most once a minute.
#[tokio::test]
async fn three_hundred_calls_with_three_hundred_tokens_make_at_most_the_allowed_signs() {
    const ALLOWED: usize = 60;
    let fixture = Fixture::new();
    let tokens: Vec<String> = (0..300_u32).map(|index| format!("{index:032x}")).collect();
    let callers: Vec<Caller> = tokens
        .iter()
        .map(|token| Caller {
            key: Some("key-1"),
            token: Some(token),
            name: None,
        })
        .collect();
    let first_run = reported_run(&fixture, callers[0], 5).await;
    let (went, requests) = signs_for_calls(&fixture, &callers, vec![first_run.clone()]).await;
    let reads = requests.len() - reports_in(&requests).len();
    let reports = reports_in(&requests);
    println!(
        "SIGNS 300 calls, 300 tokens, one key: signs {went}, reads {reads}, reports {}",
        reports.len()
    );
    assert!(
        went <= ALLOWED && reads <= ALLOWED,
        "went={went} reads={reads}"
    );
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert_eq!(reports[0]["run_id"], first_run["run_id"]);

    // One named agent, 50 tokens: the run its first session reported gets
    // one sign.
    let fixture = Fixture::new();
    let named: Vec<Caller> = tokens[..50]
        .iter()
        .map(|token| Caller {
            key: Some("key-1"),
            token: Some(token),
            name: Some("floor-live-runs"),
        })
        .collect();
    let lanes_run = reported_run(&fixture, named[0], 5).await;
    let by_name = listed_run(&fixture, 8, "floor-live-runs", "live", 5);
    let (went, requests) = signs_for_calls(&fixture, &named, vec![lanes_run, by_name]).await;
    let reports = reports_in(&requests);
    println!(
        "SIGNS 50 calls, one named agent, 50 tokens: signs {went}, requests {}, reports {}",
        requests.len(),
        reports.len()
    );
    assert!(reports.len() <= 1, "{reports:?}");

    // One session, 300 calls: one sign.
    let fixture = Fixture::new();
    let run = reported_run(&fixture, callers[0], 5).await;
    let (went, requests) = signs_for_calls(&fixture, &[callers[0]; 300], vec![run]).await;
    println!(
        "SIGNS 300 calls, one session: signs {went}, requests {}",
        requests.len()
    );
    assert_eq!((went, requests.len()), (1, 2), "{requests:?}");
}

/// SHOULD 3. A sign the API refuses is one request: no session refresh, no
/// retry, and nothing more for that session that minute.
#[tokio::test]
async fn a_refused_sign_is_one_request_and_never_refreshes_the_session() {
    let fixture = Fixture::new();
    let api = Api::start(vec![
        (
            "GET /api/v1/run-progress/runs",
            403,
            json!({"success": false, "error": {"code": "forbidden", "message": "not a member"}}),
        ),
        (
            "POST /api/v1/run-progress/report",
            403,
            json!({"success": false, "error": {"code": "forbidden", "message": "not a member"}}),
        ),
    ])
    .await;
    let (client, session) = hosted_client(&fixture, &api.base_url);
    let refreshes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let refreshes_for_hook = Arc::clone(&refreshes);
    client
        .set_session_refresh_hook(Arc::new(move || {
            let refreshes = Arc::clone(&refreshes_for_hook);
            Box::pin(async move {
                refreshes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                true
            })
        }))
        .await;
    let params = json!({"name": "search", "arguments": {}});
    for _ in 0..3 {
        let sent = as_caller(SESSION_A, || async {
            sign_of_life_for(&client, &session, &params, &[fixture.workspace]).await
        })
        .await;
        if let Some(task) = sent {
            task.await.expect("the sign of life finishes");
        }
    }
    let requests = progress_requests(&api);
    let refreshed = refreshes.load(std::sync::atomic::Ordering::SeqCst);
    println!(
        "SIGNS refused: requests {} {:?}, session refreshes {refreshed}",
        requests.len(),
        requests
            .iter()
            .map(|seen| seen.target.as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(refreshed, 0, "a sign never refreshes the session");
    assert_eq!(requests.len(), 1, "one request, then quiet for the minute");
}

/// The run id the API gives a report's `source_run_id`, the same as
/// Python's `uuid.uuid5(workspace, "mcp:" + key)` (here for the synthetic
/// workspace 1111..., written without hyphens).
#[test]
fn the_default_run_id_is_the_apis_id_for_the_default_key() {
    let workspace = Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap();
    assert_eq!(
        run_id_of_key(workspace, "s-0123456789abcdef:session")
            .simple()
            .to_string(),
        "cd442ae9fd695186a39dd2da46cbc5c7"
    );
}

/// SHOULD 4. Signs out at once are capped: above the cap a sign is dropped,
/// not queued, and its place is free again when a sign ends.
#[test]
fn signs_out_at_once_are_capped_and_dropped_above_the_cap() {
    let in_flight: &'static InFlight = Box::leak(Box::new(InFlight::new(3)));
    let slots: Vec<SignSlot> = (0..3)
        .map(|_| in_flight.take().expect("a free place"))
        .collect();
    assert!(in_flight.take().is_none(), "the fourth is dropped");
    drop(slots);
    assert!(in_flight.take().is_some(), "a place is free again");
}

/// SHOULD 2. What the gateway remembers of its signs: one a minute per
/// session, at most so many a minute per key, and bounded, the oldest
/// forgotten first.
#[test]
fn the_memory_of_signs_is_bounded_and_forgets_the_old_first() {
    let workspace = Uuid::new_v4();
    let start = Instant::now();
    let mut due = SignsDue::default();
    assert!(due.due("s-1", "key-1", workspace, start));
    assert!(!due.due("s-1", "key-1", workspace, start + Duration::from_secs(59)));
    assert!(due.due("s-1", "key-1", workspace, start + SIGN_OF_LIFE_EVERY));
    // One key, a new session every call: at most so many signs a minute.
    let mut due = SignsDue::default();
    let sent = (0..300)
        .filter(|index| due.due(&format!("s-{index}"), "key-1", workspace, start))
        .count();
    assert_eq!(sent, SIGNS_PER_CALLER_EVERY_MINUTE as usize);
    assert!(
        due.due("s-x", "key-2", workspace, start),
        "another key has its own"
    );
    assert!(due.due("s-x", "key-1", workspace, start + SIGN_OF_LIFE_EVERY));
    // Full of signs a minute old: they are forgotten for a new one.
    let mut due = SignsDue::default();
    for index in 0..MAX_SIGNED_SESSIONS {
        assert!(due.due(
            &format!("s-{index}"),
            &format!("key-{index}"),
            workspace,
            start
        ));
    }
    assert_eq!(due.remembered(), (MAX_SIGNED_SESSIONS, MAX_SIGNED_SESSIONS));
    assert!(due.due("s-new", "key-new", workspace, start + SIGN_OF_LIFE_EVERY));
    assert_eq!(due.remembered(), (1, 1));
    // Full of signs of this minute: there is no room for a new one, and
    // none is forgotten for it (see the test of a full memory below).
    let mut due = SignsDue::default();
    for index in 0..MAX_SIGNED_SESSIONS {
        due.due(
            &format!("s-{index}"),
            &format!("key-{index}"),
            workspace,
            start,
        );
    }
    assert!(!due.due(
        "s-new",
        "key-new",
        workspace,
        start + Duration::from_secs(1)
    ));
    assert_eq!(due.remembered(), (MAX_SIGNED_SESSIONS, MAX_SIGNED_SESSIONS));
}

// ============================================================================
// The second reading of 60ecd07: MUST 1 (a caller the gateway cannot name
// itself is still a caller) and MUST 3 (a full memory of signs)
// ============================================================================

/// A bearer token shaped as the API's are: `claims` and a made-up signature.
fn bearer_token(claims: Value, signature: &str) -> String {
    use base64::Engine;

    let encode = |bytes: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    format!(
        "{}.{}.{signature}",
        encode(br#"{"alg":"RS256","typ":"JWT"}"#),
        encode(claims.to_string().as_bytes())
    )
}

/// Run `work` as a request that presented the bearer token `token` to a
/// gateway that holds no secret to verify it with: it has the token, and no
/// verified subject and no key.
async fn as_bearer<T, F, Fut>(token: &str, work: F) -> T
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let auth = mcp_types::AuthOverride {
        jwt: Some(token.to_string()),
        ..Default::default()
    };
    mcp_client::run_with_auth_override(auth, work).await
}

/// The session part of a request of `token`: in the legacy session
/// `session_id`, from the connection `connection`, or with neither.
async fn bearer_part(token: &str, session_id: Option<&str>, connection: Option<&str>) -> String {
    as_bearer(token, || async {
        let part = || async { session_part(true) };
        let in_connection = || async {
            match connection {
                Some(connection) => {
                    mcp_client::client_session::run_with(connection.to_string(), part).await
                }
                None => part().await,
            }
        };
        match session_id {
            Some(id) => mcp_client::run_with_mcp_session_id(id.to_string(), in_connection).await,
            None => in_connection().await,
        }
    })
    .await
}

/// MUST 1. Two people signed in with tokens the gateway cannot verify are two
/// callers: the session id, or the connection token, one of them presents
/// never names a session of the other, and neither is everyone.
#[tokio::test]
async fn a_caller_the_gateway_cannot_name_itself_is_still_a_caller() {
    let ann = bearer_token(json!({"sub": "user-ann"}), "signature-a");
    let bob = bearer_token(json!({"sub": "user-bob"}), "signature-b");
    // The legacy contract: one session id, two callers, two sessions.
    let ann_legacy = bearer_part(&ann, Some("sid-42"), None).await;
    let bob_legacy = bearer_part(&bob, Some("sid-42"), None).await;
    assert!(
        ann_legacy.starts_with("s-") && ann_legacy.len() == 18,
        "{ann_legacy}"
    );
    assert_eq!(ann_legacy, bearer_part(&ann, Some("sid-42"), None).await);
    assert_ne!(
        ann_legacy, bob_legacy,
        "two callers with one session id are one session"
    );
    // The stateless contract: one connection token, two callers, two
    // sessions; and a caller that names no connection is its own, not
    // everyone.
    let ann_stateless = bearer_part(&ann, None, Some(TOKEN_A)).await;
    let bob_stateless = bearer_part(&bob, None, Some(TOKEN_A)).await;
    assert!(ann_stateless.starts_with("s-"), "{ann_stateless}");
    assert_ne!(ann_stateless, bob_stateless);
    let ann_alone = bearer_part(&ann, None, None).await;
    let bob_alone = bearer_part(&bob, None, None).await;
    assert!(
        ann_alone.starts_with("c-") && bob_alone.starts_with("c-"),
        "{ann_alone} {bob_alone}"
    );
    assert_ne!(ann_alone, bob_alone);
    // Nothing of a token or of the subject it names is in a part.
    for part in [&ann_legacy, &ann_stateless, &ann_alone] {
        assert!(!part.contains("user") && !part.contains("signature"));
    }
}

/// MUST 3. A memory full of this minute's signs has no room for a new
/// session or key: that sign is dropped and what is remembered stays, so a
/// flood of new callers cannot make the gateway forget whom it throttles.
#[test]
fn a_full_memory_of_signs_drops_the_new_sign_and_keeps_its_throttles() {
    let workspace = Uuid::new_v4();
    let start = Instant::now();
    let mut due = SignsDue::default();
    // One key uses up its minute.
    for index in 0..SIGNS_PER_CALLER_EVERY_MINUTE {
        assert!(due.due(&format!("s-busy-{index}"), "key-busy", workspace, start));
    }
    assert!(!due.due("s-busy-more", "key-busy", workspace, start));
    // Other callers fill the memory within the same minute.
    let soon = start + Duration::from_secs(1);
    let mut index = 0;
    while due.remembered().0 < MAX_SIGNED_SESSIONS {
        assert!(due.due(
            &format!("s-{index}"),
            &format!("key-{index}"),
            workspace,
            soon
        ));
        index += 1;
    }
    // One more caller: no room, so no sign, and nothing is forgotten.
    let full = due.remembered();
    assert!(
        !due.due("s-new", "key-new", workspace, soon),
        "a full memory let a new sign through"
    );
    assert_eq!(due.remembered(), full, "a full memory forgot its signs");
    // The busy key is as throttled as it was: no new session of it, and no
    // second sign for one that had its own.
    assert!(!due.due("s-busy-more", "key-busy", workspace, soon));
    assert!(!due.due("s-busy-0", "key-busy", workspace, soon));
    // A minute on the old signs are forgotten, and there is room again.
    assert!(due.due("s-new", "key-new", workspace, soon + SIGN_OF_LIFE_EVERY));
    // The memory of sessions was full and is searched; the memory of keys
    // was not, and forgets its old entries when it is.
    let (sessions, keys) = due.remembered();
    assert_eq!(sessions, 1);
    assert!(keys <= MAX_SIGNED_SESSIONS);
}

/// A full memory is searched for entries to forget at most once a second,
/// however many new callers knock: between two searches it has no room.
#[test]
fn a_full_memory_is_searched_for_room_at_most_once_a_second() {
    let start = Instant::now();
    let keep = Duration::from_secs(60);
    let mut memory: HashMap<String, Instant> = (0..MAX_SIGNED_SESSIONS)
        .map(|index| (format!("old-{index}"), start))
        .collect();
    let mut swept = None;
    let room = |memory: &mut HashMap<String, Instant>, swept: &mut Option<Instant>, at: Instant| {
        has_room(memory, swept, "new", at, |kept| {
            at.saturating_duration_since(*kept) < keep
        })
    };
    // Full of entries that still count: searched once, no room.
    let soon = start + Duration::from_secs(59);
    assert!(!room(&mut memory, &mut swept, soon));
    assert_eq!(swept, Some(soon));
    // The entries are old enough to go a moment later, but the memory was
    // just searched: not again within the second.
    let old_enough = start + keep;
    assert!(old_enough.saturating_duration_since(soon) >= SWEEP_FULL_MEMORY_EVERY);
    let within = soon + Duration::from_millis(500);
    assert!(!room(&mut memory, &mut swept, within));
    assert_eq!((memory.len(), swept), (MAX_SIGNED_SESSIONS, Some(soon)));
    // A second after the search it is searched again, and has room.
    assert!(room(&mut memory, &mut swept, old_enough));
    assert_eq!((memory.len(), swept), (0, Some(old_enough)));
    // One it holds always has room, searched or not.
    let mut memory: HashMap<String, Instant> = (0..MAX_SIGNED_SESSIONS)
        .map(|index| (format!("old-{index}"), start))
        .collect();
    let mut swept = Some(start);
    assert!(has_room(&mut memory, &mut swept, "old-7", start, |_| true));
    assert_eq!(swept, Some(start));
}

/// MUST 1. Who a bearer token the gateway cannot verify stands for: the
/// subject it names, so a replaced token is the same caller; with no subject
/// to read, the token itself. Never the text of either.
#[tokio::test]
async fn who_a_bearer_token_stands_for() {
    async fn caller(token: &str) -> String {
        as_bearer(token, || async { caller_of_request() })
            .await
            .expect("a request with a bearer token has a caller")
    }
    let first = bearer_token(json!({"sub": "user-ann", "jti": "1", "exp": 1}), "s1");
    let second = bearer_token(json!({"sub": "user-ann", "jti": "2", "exp": 2}), "s2");
    let ann = caller(&first).await;
    assert!(ann.starts_with("csrp:v1:s:"), "{ann}");
    assert_eq!(
        ann,
        caller(&second).await,
        "a replaced token is its subject"
    );
    assert_ne!(
        ann,
        caller(&bearer_token(json!({"sub": "user-bob"}), "s1")).await
    );
    assert!(!ann.contains("user-ann") && !ann.contains("s1"), "{ann}");
    // Nothing that reads as a subject: the whole token is the caller, and
    // another token is another caller.
    for claims in [
        json!({}),
        json!({"sub": ""}),
        json!({"sub": "  "}),
        json!({"sub": 7}),
        json!({"sub": ["user-ann"]}),
        json!({"sub": "user\nann"}),
        json!({"sub": "x".repeat(mcp_client::run_progress::MAX_SUBJECT + 1)}),
        json!(["user-ann"]),
    ] {
        let one = caller(&bearer_token(claims.clone(), "s1")).await;
        let two = caller(&bearer_token(claims.clone(), "s2")).await;
        assert!(one.starts_with("csrp:v1:t:"), "{claims}: {one}");
        assert_ne!(one, two, "{claims}");
        assert_eq!(one, caller(&bearer_token(claims, "s1")).await);
    }
    for token in ["opaque.token.one", "a.b.c.d", "no-dots", "..", "a.!!!.c"] {
        let one = caller(token).await;
        assert!(one.starts_with("csrp:v1:t:"), "{token}: {one}");
        assert!(!one.contains(token), "{one}");
    }
    assert_ne!(
        caller("opaque.token.one").await,
        caller("opaque.token.two").await
    );
    // A token whose text is a subject's is not that subject.
    assert_ne!(caller("user-ann").await, ann);
    // The identity the gateway derived itself (a verified subject, a key)
    // is the caller as it is.
    let derived = hosted(Some("key-1"), None, || async { caller_of_request() }).await;
    assert_eq!(derived.as_deref(), Some("key-1"));
    // No credential: no caller.
    assert_eq!(caller_of_request(), None);
    let empty = mcp_client::run_with_auth_override(mcp_types::AuthOverride::default(), || async {
        caller_of_request()
    })
    .await;
    assert_eq!(empty, None);
    let blank = mcp_types::AuthOverride {
        jwt: Some("  ".to_string()),
        ..Default::default()
    };
    let blank = mcp_client::run_with_auth_override(blank, || async { caller_of_request() }).await;
    assert_eq!(blank, None);
}

/// The hosted gateway's client and session: no key of its own, so a
/// caller's own credential is what its requests carry.
fn hosted_client_without_a_key(
    fixture: &Fixture,
    base_url: &str,
) -> (ContextStreamClient, Arc<SessionManager>) {
    let mut config = TestFixtures::test_config();
    config.api_url = base_url.to_string();
    config.api_key = None;
    config.default_workspace_id = Some(fixture.workspace);
    config.default_project_id = Some(fixture.project);
    config.is_http_transport = true;
    let client = ContextStreamClient::new(config.clone());
    let session = Arc::new(SessionManager::new(client.clone(), config));
    (client, session)
}

/// One `search` call that carried the credential `auth`, in the legacy
/// session `session_id`, through a hosted gateway whose setting names the
/// fixture's workspace and whose API knows the runs `listed`. `identity` is
/// what the middleware derived for the caller (for a key), if anything.
/// Nothing here says the API accepted the credential. Whether a sign went
/// out, and every request made (target, body).
async fn sign_under(
    fixture: &Fixture,
    auth: &mcp_types::AuthOverride,
    identity: Option<&str>,
    session_id: &str,
    listed: Vec<Value>,
) -> (bool, Vec<(String, Value)>) {
    let api = Api::start(routes_knowing(fixture, &listed)).await;
    let (client, session) = hosted_client_without_a_key(fixture, &api.base_url);
    let params = json!({"name": "search", "arguments": {"query": "anything"}});
    let here = [fixture.workspace];
    let call = || async {
        mcp_client::run_with_mcp_session_id(session_id.to_string(), || async {
            crate::client_identity::run_with_client_name(
                Some("claude-code".to_string()),
                sign_of_life_for(&client, &session, &params, &here),
            )
            .await
        })
        .await
    };
    let sent = mcp_client::run_with_auth_override(auth.clone(), || async {
        match identity {
            Some(identity) => {
                mcp_client::run_with_caller_cache_identity(identity.to_string(), call).await
            }
            None => call().await,
        }
    })
    .await;
    let went = sent.is_some();
    if let Some(task) = sent {
        task.await.expect("the sign of life finishes");
    }
    let requests = api
        .requests()
        .into_iter()
        .map(|seen| (seen.target, seen.body))
        .collect();
    (went, requests)
}

fn bearer_auth(token: &str) -> mcp_types::AuthOverride {
    mcp_types::AuthOverride {
        jwt: Some(token.to_string()),
        ..Default::default()
    }
}

/// A run at work in the fixture's workspace, with the id of the default run
/// of the session part `part`.
fn default_run_of(fixture: &Fixture, part: &str) -> Value {
    let mut run = listed_run(fixture, 0, "claude-code:0badc0de", "live", 5);
    run["run_id"] = json!(run_id_of_key(
        fixture.workspace,
        &run_key_of(None, part, None)
    ));
    run
}

/// MUST 1. A signed-in caller's tool call signals its own session's run and
/// never another caller's, though both present one session id.
#[tokio::test]
async fn a_signed_in_callers_tool_call_signals_its_own_run_and_never_anothers() {
    let fixture = Fixture::new();
    let ann = bearer_token(json!({"sub": Uuid::new_v4()}), "signature-a");
    let bob = bearer_token(json!({"sub": Uuid::new_v4()}), "signature-b");
    let ann_run = default_run_of(&fixture, &bearer_part(&ann, Some("sid-42"), None).await);
    let ann_target = format!(
        "/api/v1/run-progress/runs/{}?workspace_id={}",
        ann_run["run_id"].as_str().expect("a run id"),
        fixture.workspace
    );
    api_answers(&bearer_auth(&ann), 200);
    api_answers(&bearer_auth(&bob), 200);
    // Bob presents Ann's session id: his call reads a run of his own (there
    // is none), and Ann's run is neither read nor signalled.
    let (went, requests) = sign_under(
        &fixture,
        &bearer_auth(&bob),
        None,
        "sid-42",
        vec![ann_run.clone()],
    )
    .await;
    assert!(went, "{requests:?}");
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_ne!(requests[0].0, ann_target);
    assert!(requests[0].0.starts_with("/api/v1/run-progress/runs/"));
    // Ann's own call signals it.
    let (went, requests) = sign_under(
        &fixture,
        &bearer_auth(&ann),
        None,
        "sid-42",
        vec![ann_run.clone()],
    )
    .await;
    assert!(went, "{requests:?}");
    assert_eq!(requests[0].0, ann_target);
    assert_eq!(
        reports_in(&requests),
        vec![&json!({
            "workspace_id": fixture.workspace,
            "run_id": ann_run["run_id"],
            "agent": ann_run["agent"],
        })]
    );
    // A request with no credential at all, in that session: no caller, so
    // no run is derived and nothing is asked.
    let (went, requests) = sign_under(
        &fixture,
        &mcp_types::AuthOverride::default(),
        None,
        "sid-42",
        vec![ann_run],
    )
    .await;
    assert!(!went && requests.is_empty(), "{requests:?}");
}

/// MUST 3. No sign goes out under a credential the API was not seen to
/// accept: nothing is asked, and nothing is remembered of the call. Only a
/// success counts, and a 401 takes it back.
#[tokio::test]
async fn a_sign_waits_until_the_api_was_seen_to_accept_the_credential() {
    let fixture = Fixture::new();
    let key = format!("key-{}", Uuid::new_v4());
    let auth = key_auth(&key);
    let run = |session_id: &'static str| {
        let part = mcp_client::run_with_mcp_session_id(session_id.to_string(), || {
            mcp_client::run_with_caller_cache_identity(key.clone(), || async { session_part(true) })
        });
        async { default_run_of(&fixture, &part.await) }
    };
    let first = run("sid-1").await;
    // Never seen: nothing.
    let (went, requests) =
        sign_under(&fixture, &auth, Some(&key), "sid-1", vec![first.clone()]).await;
    assert!(!went && requests.is_empty(), "{requests:?}");
    // An answer that is not a success says nothing of the credential.
    for status in [403, 404, 429, 500, 503] {
        api_answers(&auth, status);
        let (went, requests) =
            sign_under(&fixture, &auth, Some(&key), "sid-1", vec![first.clone()]).await;
        assert!(!went && requests.is_empty(), "{status}: {requests:?}");
    }
    // The calls above were not counted against the session's minute: once
    // the API accepted the key, the session's next call signals its run.
    api_answers(&auth, 200);
    let (went, requests) =
        sign_under(&fixture, &auth, Some(&key), "sid-1", vec![first.clone()]).await;
    assert!(went, "{requests:?}");
    assert_eq!(reports_in(&requests).len(), 1, "{requests:?}");
    // Another key was not accepted by that.
    let other = format!("key-{}", Uuid::new_v4());
    let (went, requests) =
        sign_under(&fixture, &key_auth(&other), Some(&other), "sid-1", vec![]).await;
    assert!(!went && requests.is_empty(), "{requests:?}");
    // The API refuses the key (401): no further sign, in any session.
    api_answers(&auth, 401);
    let second = run("sid-2").await;
    let (went, requests) = sign_under(&fixture, &auth, Some(&key), "sid-2", vec![second]).await;
    assert!(!went && requests.is_empty(), "{requests:?}");
}

/// The delta reading's MUST 1. The gateway learns whose a credential is from
/// the one answer in which the API says so (`GET /auth/me`). A success of
/// any other request says nothing of the credential (the API answers what is
/// public with success whatever came with it), and a 401 takes the word
/// back.
#[tokio::test]
async fn only_the_apis_word_on_whose_a_credential_is_accepts_it() {
    mcp_client::observe_api_answers(api_answered);
    let fixture = Fixture::new();
    let key = format!("key-{}", Uuid::new_v4());
    let user = Uuid::new_v4();
    assert_eq!(accepted_user_of_key(&key), None);
    let body = json!({"workspace_id": fixture.workspace, "source_run_id": "a-run"});
    let api = Api::start(vec![
        (
            "POST /api/v1/run-progress/report",
            200,
            fixture.answer("working", &[]),
        ),
        (
            "GET /api/v1/mcp/version",
            200,
            json!({"success": true, "data": {"latest_version": "1.0.80"}}),
        ),
        ("GET /api/v1/auth/me", 200, me_answer(user)),
    ])
    .await;
    let (client, _) = hosted_client_without_a_key(&fixture, &api.base_url);
    // Two successes under the key: a report the API recorded, and a route
    // that needs no credential. Neither says whose the key is.
    mcp_client::run_with_auth_override(key_auth(&key), || async {
        client
            .report_run_progress(&body, 0)
            .await
            .expect("the report is recorded");
        let _: Value = client
            .get("/mcp/version")
            .await
            .expect("the version is public");
    })
    .await;
    assert_eq!(progress_requests(&api).len(), 2);
    assert_eq!(
        accepted_user_of_key(&key),
        None,
        "a success that names nobody accepted the key"
    );
    // The API says whose the key is.
    mcp_client::run_with_auth_override(key_auth(&key), || async { client.me().await })
        .await
        .expect("the API names the caller");
    assert_eq!(accepted_user_of_key(&key), Some(user_digest(user)));
    // Nothing of the user's id is kept.
    assert!(!user_digest(user).contains(&user.simple().to_string()[..8]));
    // The side channel's question is the same question.
    let other = format!("key-{}", Uuid::new_v4());
    mcp_client::run_with_auth_override(key_auth(&other), || async {
        client.name_caller_quietly().await
    })
    .await
    .expect("the API names the caller");
    assert_eq!(accepted_user_of_key(&other), Some(user_digest(user)));
    // A request the gateway sends under no caller's credential says nothing
    // of any caller.
    client.me().await.expect("the API answers");
    assert_eq!(accepted_user_of_key("test-api-key"), None);
    // The API refuses the key: it is no longer accepted.
    let refusing = Api::start(vec![(
        "POST /api/v1/run-progress/report",
        401,
        json!({"success": false, "error": {"message": "Invalid API key"}}),
    )])
    .await;
    let (client, _) = hosted_client_without_a_key(&fixture, &refusing.base_url);
    let refused = mcp_client::run_with_auth_override(key_auth(&key), || async {
        client.report_run_progress(&body, 0).await
    })
    .await;
    assert!(refused.is_err());
    assert_eq!(
        accepted_user_of_key(&key),
        None,
        "a 401 under the key was not noted"
    );
    assert_eq!(accepted_user_of_key(&other), Some(user_digest(user)));
}

/// The memory of accepted credentials: only the API's word puts one in, a
/// refusal takes it out, the word counts for [`ACCEPTED_FOR`], and a full
/// memory keeps what it has and leaves the new one out.
#[test]
fn the_memory_of_accepted_credentials_is_bounded_and_keeps_what_it_has() {
    let start = Instant::now();
    let second = Duration::from_secs(1);
    let mut seen = Accepted::default();
    seen.named("a".into(), "ann", start);
    assert_eq!(seen.user_of("a", start), Some("ann"));
    assert_eq!(
        seen.user_of("a", start + ACCEPTED_FOR - second),
        Some("ann")
    );
    assert_eq!(seen.user_of("a", start + ACCEPTED_FOR), None);
    assert_eq!(seen.user_of("b", start), None);
    // The gateway's door lets a credential through on the API's answer for
    // ten minutes: the word counts a little longer than that.
    assert!(ACCEPTED_FOR > Duration::from_secs(600));
    seen.refused("a");
    assert_eq!(seen.user_of("a", start), None);
    assert_eq!(seen.remembered(), 0);

    let mut seen = Accepted::default();
    for index in 0..MAX_SIGNED_SESSIONS {
        seen.named(format!("c-{index}"), &format!("user-{index}"), start);
    }
    let soon = start + second;
    seen.named("new".into(), "user-new", soon);
    assert_eq!(
        seen.user_of("new", soon),
        None,
        "a full memory took one more"
    );
    assert_eq!(seen.remembered(), MAX_SIGNED_SESSIONS);
    assert!(
        seen.user_of("c-0", soon).is_some(),
        "a full memory forgot one"
    );
    // One it has is still refreshed.
    seen.named("c-0".into(), "user-0", soon);
    // Once the others are too old to count they are forgotten for a new one.
    let later = start + ACCEPTED_FOR;
    seen.named("new".into(), "user-new", later);
    assert_eq!(seen.user_of("new", later), Some("user-new"));
    assert_eq!(seen.user_of("c-0", later), Some("user-0"));
    assert_eq!(seen.remembered(), 2);
}

/// The delta reading's MUST 1, third part. One user's credentials take a few
/// places in the memory and no more, so no one account fills it: one more,
/// and the one the API named longest ago makes way.
#[test]
fn one_users_credentials_take_a_few_places_and_no_more() {
    let start = Instant::now();
    let at = |index: usize| start + Duration::from_secs(index as u64);
    let mut seen = Accepted::default();
    seen.named("bob-0".into(), "bob", start);
    let more = 5;
    for index in 0..MAX_CREDENTIALS_OF_ONE_USER + more {
        seen.named(format!("ann-{index}"), "ann", at(index));
    }
    assert_eq!(seen.remembered(), MAX_CREDENTIALS_OF_ONE_USER + 1);
    let now = at(MAX_CREDENTIALS_OF_ONE_USER + more);
    for index in 0..MAX_CREDENTIALS_OF_ONE_USER + more {
        assert_eq!(
            seen.user_of(&format!("ann-{index}"), now).is_some(),
            index >= more,
            "ann-{index}"
        );
    }
    // Another user's credential is not what makes way.
    assert_eq!(seen.user_of("bob-0", now), Some("bob"));
    // A credential it has is refreshed in its place.
    seen.named(format!("ann-{more}"), "ann", now);
    assert_eq!(seen.remembered(), MAX_CREDENTIALS_OF_ONE_USER + 1);
}

/// A credential a run was reported under, while the API's word on it was
/// fresh, stays accepted for as long as a tool call can signal that run; a
/// report under a credential with no fresh word accepts nothing, and has the
/// API asked, at most once a while.
#[test]
fn a_credential_with_a_run_stays_accepted_as_long_as_the_run_can_be_signalled() {
    let start = Instant::now();
    let second = Duration::from_secs(1);
    assert_eq!(ACCEPTED_WITH_A_RUN_FOR, HEARTBEAT_IDLE_LIMIT + ACCEPTED_FOR);
    let mut seen = Accepted::default();
    seen.named("a".into(), "ann", start);
    // The last moment the word is fresh: the report needs no question.
    let reported = start + ACCEPTED_FOR - second;
    assert!(!seen.reported("a", reported));
    // The run can be signalled for HEARTBEAT_IDLE_LIMIT after the report.
    let last_sign = reported + HEARTBEAT_IDLE_LIMIT - second;
    assert_eq!(seen.user_of("a", last_sign), Some("ann"));
    assert_eq!(seen.user_of("a", start + ACCEPTED_WITH_A_RUN_FOR), None);

    // No fresh word: the report accepts nothing, and the API is to be asked.
    let mut seen = Accepted::default();
    seen.named("b".into(), "bob", start);
    let stale = start + ACCEPTED_FOR;
    assert!(seen.reported("b", stale));
    assert_eq!(seen.user_of("b", stale), None, "a report alone accepted it");
    assert!(seen.reported("never-named", stale));
    assert_eq!(seen.user_of("never-named", stale), None);
    // Not again for a while.
    assert!(!seen.reported("b", stale + second));
    assert!(!seen.reported("never-named", stale + ASK_WHOSE_EVERY - second));
    assert!(seen.reported("b", stale + ASK_WHOSE_EVERY));
    // The API says whose it is: the report it was asked for gives it its run.
    let named = stale + ASK_WHOSE_EVERY + second;
    seen.named("b".into(), "bob", named);
    assert!(!seen.reported("b", named));
    assert_eq!(seen.user_of("b", named + HEARTBEAT_IDLE_LIMIT), Some("bob"));
    // A word that stopped counting takes its run with it.
    let much_later = named + ACCEPTED_WITH_A_RUN_FOR;
    assert_eq!(seen.user_of("b", much_later), None);
    seen.named("b".into(), "bob", much_later);
    assert_eq!(seen.user_of("b", much_later + ACCEPTED_FOR), None);
    // A refusal ends it at once, run or no run.
    seen.named("b".into(), "bob", much_later);
    assert!(!seen.reported("b", much_later));
    seen.refused("b");
    assert_eq!(seen.user_of("b", much_later), None);
}

/// The delta reading's MUST 1, second part. Signs are counted against the
/// user the API named for the credential, and a session's minute is that
/// user's: nothing another user's request says of itself (a session part)
/// spends them.
#[test]
fn signs_are_counted_against_the_user_the_api_named() {
    let workspace = Uuid::new_v4();
    let start = Instant::now();
    let mut due = SignsDue::default();
    assert!(due.due("s-shared", "ann", workspace, start));
    // Bob's request says it is the same session: it is his own, and Ann's
    // minute stands as it was.
    assert!(due.due("s-shared", "bob", workspace, start));
    assert!(!due.due("s-shared", "ann", workspace, start + Duration::from_secs(1)));
    assert!(!due.due("s-shared", "bob", workspace, start + Duration::from_secs(1)));
    // Bob spends all of his signs of the minute: Ann still has hers.
    for index in 1..SIGNS_PER_CALLER_EVERY_MINUTE {
        assert!(due.due(&format!("s-bob-{index}"), "bob", workspace, start));
    }
    assert!(!due.due("s-bob-more", "bob", workspace, start));
    assert!(due.due("s-ann-2", "ann", workspace, start));
}

/// One report under `key` that the API recorded in `workspace`, on a
/// gateway that keeps `workspaces` alive: whether the API was asked whose
/// the key is.
async fn reported_under(
    client: &ContextStreamClient,
    key: &str,
    workspace: Uuid,
    workspaces: &[Uuid],
) -> bool {
    let task = mcp_client::run_with_auth_override(key_auth(key), || async {
        caller_reported_for(client, workspace, workspaces).await
    })
    .await;
    let asked = task.is_some();
    if let Some(task) = task {
        task.await.expect("the question finishes");
    }
    asked
}

/// On the session-id contract the gateway's door sees a credential only at
/// `initialize`. A report the API recorded under a credential it has no
/// fresh word on has the API asked whose it is: once, only for a workspace
/// the gateway keeps alive, and a credential the API then refuses is not
/// accepted and not asked about again for a while.
#[tokio::test]
async fn a_recorded_report_has_the_api_asked_whose_the_credential_is() {
    mcp_client::observe_api_answers(api_answered);
    let fixture = Fixture::new();
    let here = [fixture.workspace];
    let key = format!("key-{}", Uuid::new_v4());
    let user = Uuid::new_v4();
    let api = Api::start(vec![("GET /api/v1/auth/me", 200, me_answer(user))]).await;
    let (client, _) = hosted_client_without_a_key(&fixture, &api.base_url);

    // Another workspace: nothing is asked and nothing is remembered.
    assert!(!reported_under(&client, &key, Uuid::new_v4(), &here).await);
    assert!(!reported_under(&client, &key, fixture.workspace, &[]).await);
    assert!(progress_requests(&api).is_empty());

    assert!(reported_under(&client, &key, fixture.workspace, &here).await);
    let asked = progress_requests(&api);
    assert_eq!(asked.len(), 1, "{asked:?}");
    assert_eq!(
        (asked[0].method.as_str(), asked[0].target.as_str()),
        ("GET", "/api/v1/auth/me")
    );
    assert_eq!(accepted_user_of_key(&key), Some(user_digest(user)));
    // The word is fresh: the next reports ask nothing.
    assert!(!reported_under(&client, &key, fixture.workspace, &here).await);
    assert_eq!(progress_requests(&api).len(), 1);
    // The credential has a run: it counts beyond the fresh word.
    let digest = credential_digest(mcp_client::SentCredential::ApiKey(&key));
    let beyond = Instant::now() + ACCEPTED_FOR + Duration::from_secs(1);
    assert!(accepted()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .user_of(&digest, beyond)
        .is_some());

    // A credential the API refuses when asked: not accepted, and not asked
    // about again at once.
    let refusing = Api::start(vec![(
        "GET /api/v1/auth/me",
        401,
        json!({"success": false, "error": {"message": "Invalid API key"}}),
    )])
    .await;
    let (client, _) = hosted_client_without_a_key(&fixture, &refusing.base_url);
    let refused = format!("key-{}", Uuid::new_v4());
    assert!(reported_under(&client, &refused, fixture.workspace, &here).await);
    assert_eq!(accepted_user_of_key(&refused), None);
    assert!(!reported_under(&client, &refused, fixture.workspace, &here).await);
    assert_eq!(refusing.requests().len(), 1);

    // No credential of the caller's: nothing.
    assert!(caller_reported_for(&client, fixture.workspace, &here)
        .await
        .is_none());
}

/// The delta reading's MUST 3. The folder a hosted session named names the
/// agent only of a caller the gateway derived an identity for itself. A
/// signed-in caller it cannot verify has the session state of its session
/// id, which another caller may present: a folder found there names nobody.
#[tokio::test]
async fn a_session_folder_names_the_agent_only_of_a_caller_the_gateway_derived() {
    let folder = || Some("/work/agent-hijacked".to_string());
    assert_eq!(session_folder_of_caller(folder()), None);
    let token = bearer_token(json!({"sub": Uuid::new_v4()}), "signature");
    let signed_in = mcp_client::run_with_auth_override(bearer_auth(&token), || async {
        session_folder_of_caller(folder())
    })
    .await;
    assert_eq!(signed_in, None);
    let with_a_key =
        mcp_client::run_with_caller_cache_identity("key-partition".to_string(), || async {
            session_folder_of_caller(folder())
        })
        .await;
    assert_eq!(with_a_key, folder());
    assert_eq!(
        mcp_client::run_with_caller_cache_identity("key-partition".to_string(), || async {
            session_folder_of_caller(None)
        })
        .await,
        None
    );
}

/// SHOULD 1. A tool run through the router (`execute_operation`) counts as
/// that tool: `progress` is a report and gives no sign, and any other tool's
/// workspace is the one its own arguments name.
#[tokio::test]
async fn a_tool_run_through_the_router_counts_as_that_tool() {
    let fixture = Fixture::new();
    let through_router = |tool: &str, arguments: Value| json!({"name": "execute_operation", "arguments": {"name": tool, "arguments": arguments}});
    let (went, requests) = sign_for_call(
        &fixture,
        SESSION_A,
        &[fixture.workspace],
        through_router("progress", json!({"step": "Building"})),
        vec![],
    )
    .await;
    assert!(!went && requests.is_empty(), "{requests:?}");
    // The setting names another workspace than the session's: a tool that
    // names it in its own arguments is signalled there.
    let other = Uuid::new_v4();
    let (went, requests) = sign_for_call(
        &fixture,
        SESSION_A,
        &[other],
        through_router("search", json!({"query": "x", "workspace_id": other})),
        vec![],
    )
    .await;
    assert!(went, "{requests:?}");
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert!(
        requests[0].0.ends_with(&format!("?workspace_id={other}")),
        "{requests:?}"
    );
    // A tool that names none is in the session's workspace, which the
    // setting does not name here.
    let session_b = Caller {
        key: Some("key-1"),
        token: Some(TOKEN_B),
        name: None,
    };
    let (went, requests) = sign_for_call(
        &fixture,
        session_b,
        &[other],
        through_router("search", json!({"query": "x"})),
        vec![],
    )
    .await;
    assert!(!went && requests.is_empty(), "{requests:?}");
    // The router asked for nothing it can name: the call is the router's.
    let (went, requests) = sign_for_call(
        &fixture,
        session_b,
        &[fixture.workspace],
        json!({"name": "execute_operation", "arguments": {}}),
        vec![],
    )
    .await;
    assert!(went, "{requests:?}");
}
