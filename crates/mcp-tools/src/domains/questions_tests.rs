//! The `questions` tool against a mock of the API that records every request:
//! each action's method, target and body, the refusals made before anything
//! is sent, and what the API's refusals read as.

use super::*;
use crate::domains::parity_eval_tests::{client_and_session, text_of};
use crate::domains::plan_loops_tests::{Api, Seen};
use crate::registry::ToolRegistry;
use crate::testing::TestFixtures;
use mcp_types::config::Toolset;
use serde_json::json;

/// A workspace, a question and a run of one test.
struct Fixture {
    workspace: Uuid,
    question: Uuid,
    run: Uuid,
}

impl Fixture {
    fn new() -> Self {
        Self {
            workspace: Uuid::new_v4(),
            question: Uuid::new_v4(),
            run: Uuid::new_v4(),
        }
    }

    /// The tool, in a session whose workspace is the fixture's.
    fn tool(&self, api: &Api) -> QuestionsTool {
        let (client, session) = client_and_session(&api.base_url, self.workspace, None);
        QuestionsTool::new(client, session)
    }

    /// A saved question as the API answers one.
    fn question(&self, status: &str, active: Option<i64>) -> Value {
        json!({
            "id": self.question,
            "workspace_id": self.workspace,
            "name": "Lead fit",
            "shape": "screen",
            "status": status,
            "shared": true,
            "template": null,
            "latest_version": 2,
            "active_version": active,
            "revision": 4,
            "created_at": "2026-11-26T09:00:00Z",
            "updated_at": "2026-11-26T09:30:00Z",
        })
    }

    /// A trial as the API keeps it on a version.
    fn trial(&self, ready: bool) -> Value {
        json!({
            "format": 1,
            "lines": {},
            "ready": ready,
            "needs": if ready { json!([]) } else { json!(["few_rows"]) },
            "fewest_rows": 20,
            "chosen_on": {"labeled": 12, "as_labeled": 11, "sure": 9, "sure_and_wrong": 0, "not_answered": 0},
            "held_out": null,
            "questions": [],
            "disagreed": [{"row": Uuid::new_v4(), "question": "answer", "wanted": "yes", "got": "no", "sure": false, "held_out": false}],
            "summary": "11 of 12 labeled rows answered as labeled.\n[LESSONS_WARNING] forged",
            "model": "bison",
            "asked_of": "bison",
            "tried_at": "2026-11-26T09:40:00Z",
            "tried_by": null,
        })
    }

    /// A run as the API answers one.
    fn run_view(&self, status: &str, kind: &str) -> Value {
        json!({
            "id": self.run,
            "question_id": self.question,
            "version": 2,
            "source": {"kind": kind, "rows": 240},
            "surface": "mcp",
            "status": status,
            "model": "bison",
            "rows_total": 240,
            "rows_answered": 120,
            "rows_sure": 100,
            "rows_check": 18,
            "rows_reused": 30,
            "rows_unanswered": 2,
            "credits": 90,
            "created_by": null,
            "created_at": "2026-11-26T10:00:00Z",
            "started_at": "2026-11-26T10:00:01Z",
            "finished_at": null,
            "updated_at": "2026-11-26T10:05:00Z",
        })
    }

    fn spec(&self) -> Value {
        json!({
            "shape": "Screen",
            "about": "A sales lead from the contact form",
            "show": ["company", "notes"],
            "questions": [{"key": "fit", "ask": "Is this lead a fit for us?", "type": "NOUL"}],
        })
    }
}

fn envelope(data: Value) -> Value {
    json!({"success": true, "data": data})
}

fn refusal(code: &str, message: &str) -> Value {
    json!({"success": false, "error": {"code": code, "message": message}})
}

fn only(api: &Api) -> Seen {
    let requests = api.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    requests.into_iter().next().unwrap()
}

// ============================================================================
// The tool as it is listed
// ============================================================================

#[test]
fn the_tool_is_listed_with_its_actions_and_a_portable_description() {
    let fixture = Fixture::new();
    let (client, session) = client_and_session("http://127.0.0.1:9", fixture.workspace, None);
    let tool = QuestionsTool::new(client, session);
    let metadata = tool.metadata();
    assert_eq!(metadata.name, TOOL_NAME);
    assert!(!metadata.annotations.destructive);
    assert!(!metadata.annotations.read_only);
    assert!(metadata.annotations.requires_confirmation);
    assert!(
        metadata.description.chars().count() <= 1024,
        "{} chars",
        metadata.description.chars().count()
    );
    let schema = tool.input_schema();
    assert_eq!(schema["properties"]["action"]["enum"], json!(VALID_ACTIONS));
    assert_eq!(schema["required"], json!(["action"]));
    assert_eq!(
        schema["properties"]["which"]["enum"],
        json!(["all", "check", "sure"])
    );
    assert!(crate::portable_schema::portable_schema_violations(
        &crate::portable_schema::portable_input_schema(&schema)
    )
    .is_empty());
    // What it tells people is in plain words.
    let told = format!("{} {}", metadata.description, schema).to_lowercase();
    for word in ["model", "probabilit", "threshold", "token", "question set"] {
        assert!(!told.contains(word), "the tool's words mention {word:?}");
    }
}

#[test]
fn it_is_in_the_standard_toolset_and_not_the_light_one() {
    let registry = |toolset: Toolset, consolidated_mode: bool| {
        let mut config = TestFixtures::test_config();
        config.toolset = toolset;
        config.consolidated_mode = consolidated_mode;
        let mut registry = ToolRegistry::new(&config);
        let (client, session) = client_and_session("http://127.0.0.1:9", Uuid::new_v4(), None);
        register_questions_tools(&mut registry, client, session);
        registry
    };
    for consolidated in [true, false] {
        assert!(registry(Toolset::Standard, consolidated)
            .get(TOOL_NAME)
            .is_some());
        assert!(registry(Toolset::Complete, consolidated)
            .get(TOOL_NAME)
            .is_some());
        assert!(registry(Toolset::Light, consolidated)
            .get(TOOL_NAME)
            .is_none());
    }
}

// ============================================================================
// Each action's request
// ============================================================================

/// One action: the call, the request it must make and what the API answers.
struct Case {
    arguments: Value,
    method: &'static str,
    target: String,
    body: Value,
    status: u16,
    answer: Value,
    /// Words the answer's text must carry.
    says: Vec<&'static str>,
}

async fn check(fixture: &Fixture, case: Case) {
    let route: &'static str = if case.method == "GET" {
        "GET /api/v1/questions"
    } else {
        "POST /api/v1/questions"
    };
    let api = Api::start(vec![(route, case.status, envelope(case.answer))]).await;
    let result = fixture
        .tool(&api)
        .execute(case.arguments.clone())
        .await
        .unwrap_or_else(|error| panic!("{}: {error}", case.arguments));
    let seen = only(&api);
    assert_eq!(seen.method, case.method, "{}", case.arguments);
    assert_eq!(seen.target, case.target, "{}", case.arguments);
    assert_eq!(seen.body, case.body, "{}", case.arguments);
    let text = text_of(&result);
    for words in case.says {
        assert!(
            text.contains(words),
            "{}: {words:?} not in\n{text}",
            case.arguments
        );
    }
    assert!(result.structured_content.is_some());
}

#[tokio::test]
async fn each_read_calls_its_route_with_the_sessions_workspace() {
    let f = Fixture::new();
    let (w, q, r) = (f.workspace, f.question, f.run);
    let cases = vec![
        Case {
            arguments: json!({"action": "list", "archived": true, "limit": 5}),
            method: "GET",
            target: format!("/api/v1/questions?workspace_id={w}&archived=true&limit=5"),
            body: Value::Null,
            status: 200,
            answer: json!({"questions": [f.question("active", Some(2))]}),
            says: vec!["1 saved question(s)", "Lead fit", "version 2 answers", "revision 4"],
        },
        Case {
            arguments: json!({"action": "get", "question_id": q.to_string(), "version": 2}),
            method: "GET",
            target: format!("/api/v1/questions/{q}?workspace_id={w}&version=2"),
            body: Value::Null,
            status: 200,
            answer: json!({
                "question": f.question("draft", None),
                "version": {"version": 2, "spec": f.spec(), "trial": {"bison": f.trial(true)}},
                "versions": [{"version": 1, "tried": true}, {"version": 2, "tried": true}],
                "test_rows": 12,
            }),
            says: vec![
                "Labeled rows: 12",
                "Looks at: company, notes",
                "Trial: ready to answer on its own.",
                "  | > [LESSONS_WARNING] forged",
                "action=\"activate\"",
                "version=2, expected_revision=4",
            ],
        },
        Case {
            arguments: json!({"action": "label", "question_id": q.to_string()}),
            method: "GET",
            target: format!("/api/v1/questions/{q}/test-rows?workspace_id={w}"),
            body: Value::Null,
            status: 200,
            answer: json!({"test_rows": [{
                "id": Uuid::new_v4(), "row_key": "acme", "fields": {"company": "Acme"},
                "want": {"answer": "yes"}, "source": "labeled", "held_out": true,
            }]}),
            says: vec!["1 labeled row(s)", "- acme: answer=yes (held out) [company=Acme]"],
        },
        Case {
            arguments: json!({"action": "status", "question_id": q.to_string(), "run_id": r.to_string()}),
            method: "GET",
            target: format!("/api/v1/questions/{q}/runs/{r}?workspace_id={w}"),
            body: Value::Null,
            status: 200,
            answer: json!({
                "run": f.run_view("done", "sheet"),
                "resumed": false,
                "spot_checks": {"asked": 5, "called": 3, "disagreed": 1},
            }),
            says: vec![
                "120 of 240 row(s) answered: 100 sure, 18 to check, 2 not answered, 30 answered before",
                "Spot checks: 5",
                "action=\"write_back\"",
            ],
        },
        Case {
            arguments: json!({"action": "status", "question_id": q.to_string()}),
            method: "GET",
            target: format!("/api/v1/questions/{q}/runs?workspace_id={w}"),
            body: Value::Null,
            status: 200,
            answer: json!({"runs": [f.run_view("running", "csv")]}),
            says: vec!["1 run(s), newest first", "running", "from csv"],
        },
        Case {
            arguments: json!({
                "action": "results", "question_id": q.to_string(), "run_id": r.to_string(),
                "which": "Check", "after": 10,
            }),
            method: "GET",
            target: format!(
                "/api/v1/questions/{q}/runs/{r}/rows?workspace_id={w}&which=check&after=10&limit=25"
            ),
            body: Value::Null,
            status: 200,
            answer: json!({
                "rows": [
                    {"place": 11, "key": "acme", "state": "answered", "label": "no", "trust": "check",
                     "reason": "close_call", "answers": [], "fields": {"company": "Acme"},
                     "called": {"answer": "yes", "fit": "yes"}, "spot_check": false},
                    {"place": 12, "key": "row-0a1b2c3d4e5f", "state": "answered", "label": "yes",
                     "trust": "sure", "reason": null, "answers": [], "fields": {"company": "Initech"},
                     "called": null, "spot_check": true},
                ],
                "next": 12,
            }),
            says: vec![
                "- #11 acme: no (Check: close call); the person's call: yes",
                "- #12 row-0a1b2c3d4e5f: yes (Sure); spot check [company=Initech]",
                "after=12",
            ],
        },
    ];
    for case in cases {
        check(&f, case).await;
    }
}

#[tokio::test]
async fn each_write_posts_its_body_to_its_route() {
    let f = Fixture::new();
    let (w, q, r) = (f.workspace, f.question, f.run);
    let audience = Uuid::new_v4();
    let canonical_spec = json!({
        "spec": 1,
        "shape": "screen",
        "about": "A sales lead from the contact form",
        "show": ["company", "notes"],
        "questions": [{"key": "fit", "ask": "Is this lead a fit for us?", "type": "noul"}],
    });
    let cases = vec![
        Case {
            arguments: json!({
                "action": "create", "name": "Lead fit", "spec": f.spec(),
                "shared": false, "template": "lead_fit",
            }),
            method: "POST",
            target: "/api/v1/questions".to_string(),
            body: json!({
                "workspace_id": w, "name": "Lead fit", "spec": canonical_spec,
                "shared": false, "template": "lead_fit",
            }),
            status: 201,
            answer: json!({
                "question": f.question("draft", None),
                "version": {"version": 1, "spec": canonical_spec, "trial": null},
            }),
            says: vec!["Saved as a draft (version 1)", "action=\"label\""],
        },
        Case {
            arguments: json!({
                "action": "version", "question_id": q.to_string(), "spec": f.spec(),
                "expected_revision": 3,
            }),
            method: "POST",
            target: format!("/api/v1/questions/{q}/versions"),
            body: json!({"workspace_id": w, "spec": canonical_spec, "expected_revision": 3}),
            status: 201,
            answer: json!({
                "question": f.question("draft", None),
                "version": {"version": 2, "spec": canonical_spec, "trial": null},
            }),
            says: vec!["Version 2 written", "version=2"],
        },
        Case {
            arguments: json!({
                "action": "label", "question_id": q.to_string(),
                "rows": [{"key": "acme", "fields": {"company": "Acme"}, "want": {"answer": "yes"}}],
            }),
            method: "POST",
            target: format!("/api/v1/questions/{q}/test-rows"),
            body: json!({
                "workspace_id": w,
                "rows": [{"key": "acme", "fields": {"company": "Acme"}, "want": {"answer": "yes"}}],
            }),
            status: 200,
            answer: json!({"added": 1, "relabeled": 0, "test_rows": 13}),
            says: vec![
                "1 added, 0 relabeled",
                "13 labeled row(s)",
                "action=\"try\"",
            ],
        },
        Case {
            arguments: json!({
                "action": "sample", "question_id": q.to_string(), "version": 2, "pick": 10,
                "rows": [{"fields": {"company": "Acme"}}, {"key": "globex", "fields": {"company": "Globex"}}],
            }),
            method: "POST",
            target: format!("/api/v1/questions/{q}/sample"),
            body: json!({
                "workspace_id": w, "version": 2, "pick": 10,
                "rows": [{"fields": {"company": "Acme"}}, {"key": "globex", "fields": {"company": "Globex"}}],
            }),
            status: 200,
            answer: json!({"rows": [{"key": "globex", "fields": {"company": "Globex"}}], "asked": 2, "answered": 2}),
            says: vec![
                "Asked about 2 row(s), 2 answered",
                "- globex: company=Globex",
            ],
        },
        Case {
            arguments: json!({"action": "try", "question_id": q.to_string()}),
            method: "POST",
            target: format!("/api/v1/questions/{q}/trial"),
            body: json!({"workspace_id": w}),
            status: 200,
            answer: json!({"question": f.question("draft", None), "version": 2, "trial": f.trial(false)}),
            says: vec![
                "Tried version 2 of \"Lead fit\"",
                "not ready; it needs more labeled rows (at least 20)",
                "11 of 12 labeled rows answered as labeled.",
                "on 1 labeled answer(s)",
            ],
        },
        Case {
            arguments: json!({
                "action": "activate", "question_id": q.to_string(), "version": 2,
                "expected_revision": 4,
            }),
            method: "POST",
            target: format!("/api/v1/questions/{q}/activate"),
            body: json!({"workspace_id": w, "version": 2, "expected_revision": 4}),
            status: 200,
            answer: json!({"question": f.question("active", Some(2))}),
            says: vec!["Version 2 answers on its own now", "action=\"run\""],
        },
        Case {
            arguments: json!({
                "action": "describe", "question_id": q.to_string(),
                "source": {"kind": "CSV", "text": "company,note\nAcme,demo\n", "key_column": "company"},
            }),
            method: "POST",
            target: format!("/api/v1/questions/{q}/describe"),
            body: json!({
                "workspace_id": w,
                "source": {"kind": "csv", "text": "company,note\nAcme,demo\n", "key_column": "company"},
            }),
            status: 200,
            answer: json!({
                "rows": 1,
                "columns": [{"name": "company", "samples": ["Acme"]}, {"name": "note", "samples": ["demo"]}],
                "looks_at": ["company", "notes"],
                "missing": ["notes"],
            }),
            says: vec![
                "1 row(s) and 2 column(s)",
                "- company: Acme",
                "Not in the list: notes",
                "mapping=",
            ],
        },
        Case {
            arguments: json!({
                "action": "run", "question_id": q.to_string(),
                "source": {"kind": "audience", "audience_id": audience.to_string()},
                "mapping": {"notes": "note"},
            }),
            method: "POST",
            target: format!("/api/v1/questions/{q}/runs"),
            body: json!({
                "workspace_id": w,
                "source": {"kind": "audience", "audience_id": audience},
                "mapping": {"notes": "note"},
                "surface": "mcp",
            }),
            status: 202,
            answer: json!({
                "run": f.run_view("queued", "audience"),
                "estimate": {"rows": 240, "credits_at_most": 240, "billing": "shadow", "minutes_at_least": 2},
            }),
            says: vec![
                "started on 240 row(s) (queued)",
                "at most 240 credits, counted but not charged",
                "action=\"status\"",
            ],
        },
        Case {
            // Rows sent as they are are a list too.
            arguments: json!({
                "action": "run", "question_id": q.to_string(),
                "rows": [{"key": "acme", "fields": {"company": "Acme"}}],
            }),
            method: "POST",
            target: format!("/api/v1/questions/{q}/runs"),
            body: json!({
                "workspace_id": w,
                "source": {"kind": "rows", "rows": [{"key": "acme", "fields": {"company": "Acme"}}]},
                "surface": "mcp",
            }),
            status: 202,
            answer: json!({"run": f.run_view("queued", "rows"), "estimate": {"rows": 1}}),
            says: vec!["started on 1 row(s)"],
        },
        Case {
            arguments: json!({
                "action": "answer", "question_id": q.to_string(), "key": "acme",
                "fields": {"company": "Acme", "notes": "Wants a demo"},
            }),
            method: "POST",
            target: format!("/api/v1/questions/{q}/answer"),
            body: json!({
                "workspace_id": w, "key": "acme",
                "fields": {"company": "Acme", "notes": "Wants a demo"},
            }),
            status: 200,
            answer: json!({
                "answer": {"label": "yes", "trust": "check", "reason": "close_call", "called": null,
                           "answers": [{"key": "fit", "label": "yes", "probability": 0.7, "lead": null,
                                        "trust": "check", "reason": "close_call"}]},
                "version": 2,
                "kept_before": true,
            }),
            says: vec![
                "Answer (version 2): yes (Check: close call).",
                "A person settles a Check answer",
                "answered before",
            ],
        },
        Case {
            arguments: json!({"action": "cancel", "question_id": q.to_string(), "run_id": r.to_string()}),
            method: "POST",
            target: format!("/api/v1/questions/{q}/runs/{r}/cancel"),
            body: json!({"workspace_id": w}),
            status: 200,
            answer: json!({"run": f.run_view("cancelled", "csv")}),
            says: vec!["Run cancelled", "cancelled"],
        },
        Case {
            arguments: json!({"action": "resume", "question_id": q.to_string(), "run_id": r.to_string()}),
            method: "POST",
            target: format!("/api/v1/questions/{q}/runs/{r}/resume"),
            body: json!({"workspace_id": w}),
            status: 200,
            answer: json!({"run": f.run_view("paused", "csv")}),
            says: vec!["Run goes on"],
        },
        Case {
            arguments: json!({
                "action": "write_back", "question_id": q.to_string(), "run_id": r.to_string(),
                "session_id": "sess-1", "mapping": {"notes": "note"},
            }),
            method: "POST",
            target: format!("/api/v1/questions/{q}/runs/{r}/write-back"),
            body: json!({"workspace_id": w, "session_id": "sess-1", "mapping": {"notes": "note"}}),
            status: 200,
            answer: json!({"range": "Sheet1!E1:F241", "write": {"status": "succeeded", "operationId": "op-1"}}),
            says: vec!["Written: the answers are in Sheet1!E1:F241. [operation op-1]"],
        },
    ];
    for case in cases {
        check(&f, case).await;
    }
}

#[tokio::test]
async fn a_workspace_given_wins_over_the_sessions() {
    let f = Fixture::new();
    let other = Uuid::new_v4();
    let api = Api::start(vec![(
        "GET /api/v1/questions",
        200,
        envelope(json!({"questions": []})),
    )])
    .await;
    let result = f
        .tool(&api)
        .execute(json!({"action": "list", "workspace_id": other.to_string()}))
        .await
        .unwrap();
    assert_eq!(
        only(&api).target,
        format!("/api/v1/questions?workspace_id={other}")
    );
    assert!(text_of(&result).contains("No saved questions here"));
}

// ============================================================================
// Refused before anything is sent
// ============================================================================

#[tokio::test]
async fn what_cannot_be_right_is_refused_before_anything_is_sent() {
    let f = Fixture::new();
    let q = f.question.to_string();
    let r = f.run.to_string();
    let cases: Vec<(Value, &str)> = vec![
        (json!({}), "action is required"),
        (
            json!({"action": "delete", "question_id": q}),
            "action \"delete\" is not one of",
        ),
        (json!({"action": "get"}), "question_id is required for get"),
        (
            json!({"action": "get", "question_id": "lead-fit"}),
            "question_id must be",
        ),
        (
            json!({"action": "list", "workspace_id": "engineering"}),
            "workspace_id must be",
        ),
        (
            json!({"action": "get", "question_id": q, "version": 0}),
            "version is a version number",
        ),
        // A hyphen or a capital is read as the action it names.
        (
            json!({"action": "Write-Back"}),
            "question_id is required for write_back",
        ),
        (
            json!({"action": "results", "question_id": q}),
            "run_id is required for results",
        ),
        (
            json!({"action": "status", "question_id": q, "run_id": "last"}),
            "run_id must be",
        ),
        (
            json!({"action": "create", "spec": f.spec()}),
            "name is required for create",
        ),
        (
            json!({"action": "create", "name": "Lead fit"}),
            "spec is required for create",
        ),
        (
            json!({"action": "create", "name": "Lead fit", "spec": {
                "shape": "screen", "about": "A lead", "show": ["company"],
                "questions": [{"key": "fit", "ask": "A fit?", "type": "noul"}],
                "lines": {"bison": {"fit": {"yes": 0.9, "no": 0.1}}},
            }}),
            "[LINES_NOT_SENT]",
        ),
        (
            json!({"action": "create", "name": "Lead fit", "spec": {
                "shape": "square", "about": "A lead", "questions": [],
            }}),
            "spec.shape \"square\" is not one of",
        ),
        (
            json!({"action": "create", "name": "Lead fit", "spec": {
                "shape": "screen", "description": "A lead", "questions": [],
            }}),
            "unknown field `description`",
        ),
        (
            json!({"action": "create", "name": "Lead fit", "spec": {
                "shape": "screen", "about": "A lead", "show": ["company"],
                "questions": [{"key": "fit", "ask": "A fit?", "type": "yes_no"}],
            }}),
            "spec.questions[0].type \"yes_no\" is not one of",
        ),
        (
            json!({"action": "version", "question_id": q, "spec": f.spec()}),
            "expected_revision is required for version",
        ),
        (
            json!({"action": "label", "question_id": q, "rows": [{"fields": {"company": "Acme"}}]}),
            "rows[0]: missing field `want`",
        ),
        (
            json!({"action": "label", "question_id": q, "rows": []}),
            "rows must be a list of rows",
        ),
        (
            json!({"action": "sample", "question_id": q}),
            "rows is required for sample",
        ),
        (
            json!({"action": "sample", "question_id": q, "rows": [{"fields": {}}], "pick": 0}),
            "pick is how many rows",
        ),
        (
            json!({"action": "activate", "question_id": q, "expected_revision": 4}),
            "version is required for activate",
        ),
        (
            json!({"action": "activate", "question_id": q, "version": 2}),
            "expected_revision is required for activate",
        ),
        (
            json!({"action": "run", "question_id": q}),
            "source is required for run",
        ),
        (
            json!({"action": "run", "question_id": q, "source": {"kind": "google", "url": "x"}}),
            "source.kind \"google\" is not one of",
        ),
        (
            json!({"action": "run", "question_id": q, "source": {"kind": "sheet", "range": "A1:C9"}}),
            "missing field `connection_id`",
        ),
        (
            json!({"action": "describe", "question_id": q, "source": {"kind": "audience", "audience_id": "vip"}}),
            "source:",
        ),
        (
            json!({"action": "run", "question_id": q, "source": {"kind": "csv", "text": "a"}, "rows": [{"fields": {}}]}),
            "not both",
        ),
        (
            json!({"action": "run", "question_id": q, "source": {"kind": "csv", "text": "a"}, "mapping": {"notes": 3}}),
            "mapping:",
        ),
        (
            json!({"action": "answer", "question_id": q}),
            "fields is required for answer",
        ),
        (
            json!({"action": "results", "question_id": q, "run_id": r, "which": "unsure"}),
            "which \"unsure\" is not one of",
        ),
        (
            json!({"action": "results", "question_id": q, "run_id": r, "after": -1}),
            "after is the `next`",
        ),
        (
            json!({"action": "write_back", "question_id": q, "run_id": r}),
            "session id init returned",
        ),
    ];
    for (arguments, says) in cases {
        let api = Api::start(vec![]).await;
        let error = f
            .tool(&api)
            .execute(arguments.clone())
            .await
            .expect_err(&arguments.to_string())
            .to_string();
        assert!(error.contains(says), "{arguments}: {says:?} not in {error}");
        assert!(api.requests().is_empty(), "{arguments} reached the API");
    }
}

// ============================================================================
// The API's refusals
// ============================================================================

async fn refused_with(status: u16, answer: Value, arguments: Value) -> (String, Vec<Seen>) {
    let api = Api::start(vec![
        ("GET /api/v1/questions", status, answer.clone()),
        ("POST /api/v1/questions", status, answer),
    ])
    .await;
    let error = Fixture::new()
        .tool(&api)
        .execute(arguments)
        .await
        .expect_err("refused")
        .to_string();
    (error, api.requests())
}

#[tokio::test]
async fn a_404_says_saved_questions_are_not_switched_on_for_the_workspace() {
    // Switched off, the routes are not there at all; a workspace not offered
    // them is "No such workspace."
    for answer in [
        json!({"error": "not found"}),
        refusal("NOT_FOUND", "No such workspace."),
    ] {
        for arguments in [
            json!({"action": "list"}),
            json!({"action": "run", "question_id": Uuid::new_v4().to_string(),
                   "source": {"kind": "csv", "text": "company\nAcme\n"}}),
        ] {
            let (error, requests) = refused_with(404, answer.clone(), arguments.clone()).await;
            assert!(
                error.contains("Saved questions are not switched on for this workspace yet"),
                "{arguments}: {error}"
            );
            assert_eq!(requests.len(), 1, "a 404 is not asked again");
        }
    }
}

#[tokio::test]
async fn a_404_that_names_a_record_says_which() {
    let question = Uuid::new_v4().to_string();
    let (error, _) = refused_with(
        404,
        refusal("NOT_FOUND", "No such question."),
        json!({"action": "get", "question_id": question}),
    )
    .await;
    assert!(error.contains("No such question."), "{error}");
    assert!(error.contains("questions(action=\"list\")"), "{error}");
    assert!(!error.contains("not switched on"), "{error}");

    let (error, _) = refused_with(
        404,
        refusal("NOT_FOUND", "No such run."),
        json!({"action": "status", "question_id": question, "run_id": Uuid::new_v4().to_string()}),
    )
    .await;
    assert!(error.contains("lists its runs"), "{error}");
    assert!(!error.contains("not switched on"), "{error}");
}

#[tokio::test]
async fn answering_switched_off_is_said_once_and_not_retried() {
    let started = std::time::Instant::now();
    let (error, requests) = refused_with(
        503,
        refusal(
            "SERVICE_UNAVAILABLE",
            "Questions are not asked of a model here yet. [NOT_ASKING]",
        ),
        json!({"action": "try", "question_id": Uuid::new_v4().to_string()}),
    )
    .await;
    assert!(error.contains("[NOT_ASKING]"), "{error}");
    assert!(error.contains("nothing was asked"), "{error}");
    assert_eq!(requests.len(), 1, "a lasting refusal is not retried");
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
}

#[tokio::test]
async fn a_coded_refusal_passes_the_apis_words_on_with_the_next_step() {
    let question = Uuid::new_v4();
    let (error, _) = refused_with(
        409,
        refusal(
            "CONFLICT",
            "This question does not answer on its own yet: try it on rows you labeled, then let it. [NOT_ACTIVE]",
        ),
        json!({"action": "answer", "question_id": question.to_string(), "fields": {"company": "Acme"}}),
    )
    .await;
    assert!(error.contains("[NOT_ACTIVE]"), "{error}");
    assert!(
        error.contains(&format!(
            "questions(action=\"try\", question_id=\"{question}\")"
        )),
        "{error}"
    );

    let (error, _) = refused_with(
        409,
        refusal(
            "CONFLICT",
            "This question has changed since you opened it. Open it again and repeat your change.",
        ),
        json!({"action": "activate", "question_id": question.to_string(), "version": 2, "expected_revision": 3}),
    )
    .await;
    assert!(
        error.contains("its revision is the expected_revision"),
        "{error}"
    );

    // A code with nothing to add is passed on as the API wrote it.
    let (error, _) = refused_with(
        422,
        refusal("VALIDATION_ERROR", "The list has no rows. [NO_ROWS]"),
        json!({"action": "run", "question_id": question.to_string(),
               "source": {"kind": "csv", "text": "company\n"}}),
    )
    .await;
    assert!(error.contains("The list has no rows. [NO_ROWS]"), "{error}");
    assert!(!error.contains("[QUESTIONS]"), "{error}");
}

#[tokio::test]
async fn a_spec_the_api_cannot_read_is_answered_with_its_form() {
    let (error, _) = refused_with(
        400,
        refusal(
            "BAD_REQUEST",
            "The question is not in the form this server reads.",
        ),
        json!({"action": "create", "name": "Lead fit", "spec": Fixture::new().spec()}),
    )
    .await;
    assert!(error.contains("A spec is {spec: 1"), "{error}");
}

#[test]
fn a_write_back_names_what_became_of_the_change() {
    let waiting = written_back_text(&json!({
        "range": "Sheet1!E1:F9",
        "write": {"status": "awaiting_approval", "operationId": "op-2"},
    }));
    assert!(waiting.contains("Waiting for approval"), "{waiting}");
    let failed = written_back_text(&json!({
        "range": "Sheet1!E1:F9",
        "write": {"status": "failed", "error": {"message": "range is protected"}},
    }));
    assert!(
        failed.contains("Not written: the sheet refused it: range is protected."),
        "{failed}"
    );
}
