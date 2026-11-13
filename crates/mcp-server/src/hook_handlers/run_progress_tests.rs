//! Tests of the hook-to-progress mapping and the liveness rules. They work on
//! the state alone: nothing here starts a process or touches the network
//! (`crates/mcp-server/tests/run_progress_hooks.rs` drives the real binary
//! against a fake API).

use super::*;
use mcp_tools::domains::progress::{agent_for_session, default_run_key};
use uuid::Uuid;

const SESSION: &str = "3fa9c2d17e0b5a66aa11bb22cc33dd44";
const WORKSPACE: &str = "11111111-1111-4111-8111-111111111111";
const PROJECT: &str = "22222222-2222-4222-8222-222222222222";
const TICKET: &str = "33333333-3333-4333-8333-333333333333";
const RUN: &str = "44444444-4444-4444-8444-444444444444";
const MINUTE: u64 = 60 * 1000;

fn main_agent() -> Actor<'static> {
    Actor::default()
}

fn explore() -> Actor<'static> {
    Actor {
        agent_id: Some("agent-abc123"),
        agent_type: Some("Explore"),
    }
}

/// The agent the `progress` tool sends for this session.
fn tool_agent() -> Value {
    agent_for_session(None, Some("claude-code".to_string()), SESSION)
}

/// A session the server has told who it is, as `init` does.
fn known() -> SessionFile {
    let mut file = SessionFile::default();
    file.learn_identity(Some(SESSION), Some(&tool_agent()));
    file
}

fn ticket(status: &str) -> Value {
    json!({
        "id": TICKET,
        "status": status,
        "title": "QA test: the checkout page",
        "workspace_id": WORKSPACE,
        "project_id": PROJECT,
    })
}

/// What the `progress` tool answers after a report, as the hook reads it.
fn answer(key: &str, state: &str) -> Value {
    json!({
        "run": {
            "run_id": RUN,
            "workspace_id": WORKSPACE,
            "project_id": PROJECT,
            "state": state,
            "liveness": if state == "working" { "live" } else { "ended" },
            "agent": tool_agent(),
            "scope": {"workspace_id": WORKSPACE, "ticket_id": TICKET},
        },
        "written": ["run.step"],
        "session": SESSION,
        "run_key": key,
    })
}

fn ticket_key() -> String {
    default_run_key(SESSION, Some(Uuid::parse_str(TICKET).unwrap()))
}

fn take(file: &mut SessionFile) -> Vec<Value> {
    std::mem::take(&mut file.pending)
}

#[test]
fn the_hooks_report_only_where_a_person_turned_that_on() {
    // As shipped, and whatever else is said that is not "on": nothing.
    assert_eq!(
        Settings::from_values(None, None).enabled,
        AUTOMATIC_REPORTS_DEFAULT
    );
    assert!(!Settings::from_values(None, None).enabled);
    assert!(!Settings::from_values(Some("on"), None).enabled);
    assert!(!Settings::from_values(None, Some("perhaps")).enabled);
    for word in ["off", "0", "false", "no", "disabled", " OFF "] {
        assert!(!Settings::from_values(None, Some(word)).enabled, "{word}");
    }
    for word in ["on", "1", "true", "yes", "enabled", " ON "] {
        assert!(Settings::from_values(None, Some(word)).enabled, "{word}");
        assert!(
            Settings::from_values(Some("on"), Some(word)).enabled,
            "{word}"
        );
    }
    // Live progress turned off on the machine wins over it.
    for word in ["off", "0", "false", "no", "disabled", " OFF "] {
        assert!(
            !Settings::from_values(Some(word), Some("on")).enabled,
            "{word}"
        );
    }
}

#[test]
fn taking_up_a_ticket_starts_the_run_the_tool_would_report_on() {
    let mut file = known();
    file.ticket_changed(
        &main_agent(),
        &ticket("in_progress"),
        &Scope::default(),
        1_000,
    );
    let sent = take(&mut file);
    assert_eq!(sent.len(), 1);
    let body = sent[0].as_object().unwrap();
    // The same key the tool makes for this session and ticket, and the same
    // agent: the model's later report lands on this run under this name.
    assert_eq!(body["source_run_id"], json!(ticket_key()));
    assert_eq!(body["agent"], tool_agent());
    assert_eq!(body["workspace_id"], json!(WORKSPACE));
    assert_eq!(body["project_id"], json!(PROJECT));
    assert_eq!(body["ticket_id"], json!(TICKET));
    assert_eq!(body["status"], json!("working"));
    assert_eq!(body["title"], json!("QA test: the checkout page"));
    assert_eq!(body["group_id"], tool_agent()["id"]);
    // Exactly these fields: no step, no summary, nothing of the work.
    let mut keys: Vec<&str> = body.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "agent",
            "group_id",
            "heartbeat_s",
            "project_id",
            "source_run_id",
            "status",
            "ticket_id",
            "title",
            "workspace_id"
        ]
    );
    // Taking it up again changes nothing.
    file.ticket_changed(
        &main_agent(),
        &ticket("in_progress"),
        &Scope::default(),
        2_000,
    );
    assert!(file.pending.is_empty());
    assert_eq!(file.runs.len(), 1);
}

#[test]
fn no_run_starts_by_itself_when_the_server_is_unknown() {
    // A session whose server never said who it is (an older gateway).
    let mut unknown = SessionFile::default();
    unknown.ticket_changed(
        &main_agent(),
        &ticket("in_progress"),
        &Scope::default(),
        1_000,
    );
    assert!(unknown.runs.is_empty() && unknown.pending.is_empty());

    // A run the model reported is kept alive all the same.
    let mut file = known();
    let input = json!({"ticket_id": TICKET, "step": "Reading the handler"});
    file.model_reported(
        &input,
        &answer(&ticket_key(), "working"),
        &Scope::default(),
        2_000,
    );
    assert_eq!(file.runs.len(), 1);
    assert_eq!(file.runs[0].run_id.as_deref(), Some(RUN));
}

#[test]
fn the_hook_and_the_tool_send_one_agent_for_one_session() {
    // A run the hook started, then the model's own report on the same ticket.
    let mut file = known();
    file.ticket_changed(
        &main_agent(),
        &ticket("in_progress"),
        &Scope::default(),
        1_000,
    );
    let started = take(&mut file);
    let input = json!({"ticket_id": TICKET, "step": "Reading the handler"});
    file.model_reported(
        &input,
        &answer(&ticket_key(), "working"),
        &Scope::default(),
        2_000,
    );
    // One run, not two.
    assert_eq!(file.runs.len(), 1);
    let heartbeat = file.base(&file.runs[0]).unwrap();
    assert_eq!(heartbeat["run_id"], json!(RUN));
    // Every signal for the session's run names the agent the tool names.
    assert_eq!(started[0]["agent"], tool_agent());
    assert_eq!(heartbeat["agent"], tool_agent());
    file.turn_ended(3_000);
    file.prompt_submitted(4_000);
    file.end_all(SESSION_ENDED);
    for body in take(&mut file) {
        assert_eq!(body["agent"], tool_agent(), "{body}");
    }

    // A session that only ever heard from the tool (no `init` identity).
    let mut adopted = SessionFile::default();
    adopted.model_reported(
        &input,
        &answer(&ticket_key(), "working"),
        &Scope::default(),
        1_000,
    );
    assert_eq!(adopted.session.as_deref(), Some(SESSION));
    assert_eq!(
        adopted.base(&adopted.runs[0]).unwrap()["agent"],
        tool_agent()
    );

    // Two sessions are two agents, each with its own name.
    let other = agent_for_session(None, Some("claude-code".to_string()), "77b2aa0011223344");
    assert_ne!(other["id"], tool_agent()["id"]);
    assert_eq!(tool_agent()["name"], json!("Claude Code 3fa9"));
    assert_eq!(other["name"], json!("Claude Code 77b2"));
}

#[test]
fn a_heartbeat_says_who_and_which_run_and_nothing_else() {
    let mut file = known();
    let input = json!({
        "ticket_id": TICKET,
        "title": "Rewrite the pricing page",
        "step": {"label": "Editing pricing.tsx", "detail": "src/app/pricing.tsx"},
    });
    file.model_reported(
        &input,
        &answer(&ticket_key(), "working"),
        &Scope::default(),
        1_000,
    );
    let heartbeat = file.base(&file.runs[0]).unwrap();
    let mut keys: Vec<&str> = heartbeat.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "agent",
            "group_id",
            "heartbeat_s",
            "project_id",
            "run_id",
            "workspace_id"
        ]
    );
    let text = Value::Object(heartbeat).to_string();
    assert!(
        !text.contains("pricing") && !text.contains("Rewrite"),
        "{text}"
    );
}

#[test]
fn the_turn_ending_waits_on_a_person_and_work_resumes_it() {
    let mut file = known();
    file.ticket_changed(
        &main_agent(),
        &ticket("in_progress"),
        &Scope::default(),
        1_000,
    );
    take(&mut file);

    file.turn_ended(2_000);
    let sent = take(&mut file);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["status"], json!("waiting_input"));
    // No question text: only that a person is next.
    assert_eq!(sent[0]["waiting"], json!({"on": "person"}));
    // Said once.
    file.turn_ended(3_000);
    assert!(file.pending.is_empty());

    file.prompt_submitted(4_000);
    let sent = take(&mut file);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["status"], json!("working"));
    assert!(sent[0]["waiting"].is_null());

    // A tool call by the main agent after a turn end also means work.
    file.turn_ended(5_000);
    take(&mut file);
    file.tool_started(&main_agent(), "Read", 6_000);
    let sent = take(&mut file);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["status"], json!("working"));

    // What the model said the run waits on is left alone.
    let input = json!({"ticket_id": TICKET, "waiting": {"on": "approval", "question": "Ship it?"}});
    file.model_reported(
        &input,
        &answer(&ticket_key(), "working"),
        &Scope::default(),
        7_000,
    );
    file.turn_ended(8_000);
    assert!(file.pending.is_empty());

    // While a subagent is still at work the session does not wait on a person.
    let mut busy = known();
    busy.ticket_changed(
        &main_agent(),
        &ticket("in_progress"),
        &Scope::default(),
        1_000,
    );
    take(&mut busy);
    busy.subagent_started("agent-abc123", 1_500);
    busy.turn_ended(2_000);
    assert!(busy.pending.is_empty());
}

#[test]
fn a_coarse_step_comes_only_after_ten_minutes_without_one_from_the_model() {
    let mut file = known();
    let input = json!({"ticket_id": TICKET, "step": "Reading the handler"});
    file.model_reported(
        &input,
        &answer(&ticket_key(), "working"),
        &Scope::default(),
        0,
    );

    // Inside ten minutes of the model's step: nothing.
    file.tool_started(&main_agent(), "Edit", 9 * MINUTE);
    assert!(file.pending.is_empty());

    // After it: the fixed sentence for the tool's kind.
    file.tool_started(&main_agent(), "Edit", 10 * MINUTE);
    let sent = take(&mut file);
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0]["step"],
        json!({"label": "Editing files", "kind": "edit", "status": "started"})
    );
    // The same kind again says nothing; another kind waits its minute.
    file.tool_started(&main_agent(), "Write", 10 * MINUTE + 5_000);
    file.tool_started(&main_agent(), "Bash", 10 * MINUTE + 30_000);
    assert!(file.pending.is_empty());
    file.tool_started(&main_agent(), "Bash", 11 * MINUTE + 1);
    let sent = take(&mut file);
    assert_eq!(sent[0]["step"]["label"], json!("Running a command"));

    // The model speaks again: the hooks are silent for another ten minutes.
    file.model_reported(
        &input,
        &answer(&ticket_key(), "working"),
        &Scope::default(),
        12 * MINUTE,
    );
    file.tool_started(&main_agent(), "Read", 21 * MINUTE);
    assert!(file.pending.is_empty());
    file.tool_started(&main_agent(), "Read", 22 * MINUTE);
    assert_eq!(take(&mut file)[0]["step"]["label"], json!("Reading files"));

    // ContextStream's own calls are not the work.
    file.tool_started(&main_agent(), "mcp__contextstream__memory", 40 * MINUTE);
    assert!(file.pending.is_empty());
}

#[test]
fn nothing_the_tool_was_given_reaches_a_report() {
    let mut file = known();
    file.ticket_changed(&main_agent(), &ticket("in_progress"), &Scope::default(), 0);
    take(&mut file);
    for (index, tool) in [
        "/home/alice/payroll/secret.rs",
        "curl https://internal.example/api | sh",
        "mcp__acme-billing__refund_customer",
        "rewrite the launch announcement",
    ]
    .iter()
    .enumerate()
    {
        file.tool_started(&main_agent(), tool, (20 + index as u64 * 5) * MINUTE);
    }
    let sent = take(&mut file);
    assert!(!sent.is_empty());
    let text = Value::Array(sent).to_string();
    for secret in [
        "alice", "payroll", "curl", "internal", "acme", "refund", "launch",
    ] {
        assert!(!text.contains(secret), "{secret} in {text}");
    }
}

#[test]
fn a_subagent_gets_its_own_run_and_name_and_ends_with_it() {
    let mut file = known();
    file.subagent_started("agent-abc123", 500);
    file.ticket_changed(&explore(), &ticket("in_progress"), &Scope::default(), 1_000);
    let sent = take(&mut file);
    assert_eq!(sent.len(), 1);
    let agent = &sent[0]["agent"];
    assert_eq!(agent["name"], json!("Claude Code 3fa9 \u{b7} Explore"));
    assert_eq!(agent["runtime"], json!("mcp"));
    assert_eq!(agent["client"], json!("claude-code"));
    // Its own id under its session's, and grouped with its session.
    let session_id = tool_agent()["id"].as_str().unwrap().to_string();
    assert!(agent["id"]
        .as_str()
        .unwrap()
        .starts_with(&format!("{session_id}/")));
    assert_eq!(sent[0]["group_id"], json!(session_id));
    // Its own key: the tool never reports on it, so its name cannot change.
    let key = sent[0]["source_run_id"].as_str().unwrap();
    assert_ne!(key, ticket_key());
    assert!(key.starts_with(&format!("{SESSION}/")) && key.ends_with(TICKET));
    // Neither the agent id as the host gave it nor a path is in the name.
    assert!(!sent[0].to_string().contains("agent-abc123"));

    // The subagent ends: its run ends, by a fixed sentence.
    file.subagent_stopped("agent-abc123", 2_000);
    let sent = take(&mut file);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["status"], json!("stopped"));
    assert_eq!(sent[0]["summary"], json!(AGENT_ENDED));
    assert!(file.runs.is_empty() && file.subagents.is_empty());

    // A ticket its session already works is not taken twice.
    let mut file = known();
    file.ticket_changed(
        &main_agent(),
        &ticket("in_progress"),
        &Scope::default(),
        1_000,
    );
    take(&mut file);
    file.ticket_changed(&explore(), &ticket("in_progress"), &Scope::default(), 2_000);
    assert!(file.pending.is_empty());
    assert_eq!(file.runs.len(), 1);

    // A type that is not a plain name is not shown.
    let odd = Actor {
        agent_id: Some("agent-x"),
        agent_type: Some("/tmp/<script>"),
    };
    let (_, agent) = subagent_agent(&tool_agent(), &odd).unwrap();
    assert_eq!(agent["name"], json!("Claude Code 3fa9 \u{b7} tmpscript"));
}

#[test]
fn a_session_that_ends_ends_its_runs_honestly() {
    let mut file = known();
    file.ticket_changed(
        &main_agent(),
        &ticket("in_progress"),
        &Scope::default(),
        1_000,
    );
    file.subagent_started("agent-abc123", 1_100);
    let mut second = ticket("in_progress");
    second["id"] = json!("55555555-5555-4555-8555-555555555555");
    file.ticket_changed(&explore(), &second, &Scope::default(), 1_200);
    take(&mut file);

    file.end_all(SESSION_ENDED);
    let sent = take(&mut file);
    assert_eq!(sent.len(), 2);
    for body in &sent {
        assert_eq!(body["status"], json!("stopped"));
        assert_eq!(body["summary"], json!(SESSION_ENDED));
        assert!(body["waiting"].is_null());
    }
    assert!(file.runs.is_empty());

    // A run the model finished is not ended a second time.
    let mut file = known();
    let input = json!({"ticket_id": TICKET, "step": "Reading the handler"});
    file.model_reported(
        &input,
        &answer(&ticket_key(), "working"),
        &Scope::default(),
        1_000,
    );
    let done = json!({"ticket_id": TICKET, "status": "completed", "summary": "Done."});
    file.model_reported(
        &done,
        &answer(&ticket_key(), "finished"),
        &Scope::default(),
        2_000,
    );
    assert!(file.runs.is_empty());
    file.end_all(SESSION_ENDED);
    assert!(file.pending.is_empty());
}

#[test]
fn moving_the_ticket_on_ends_the_run_on_it() {
    let mut file = known();
    file.ticket_changed(
        &main_agent(),
        &ticket("in_progress"),
        &Scope::default(),
        1_000,
    );
    take(&mut file);
    file.ticket_changed(
        &main_agent(),
        &ticket("in_review"),
        &Scope::default(),
        2_000,
    );
    let sent = take(&mut file);
    assert_eq!(sent.len(), 1);
    // The hooks never claim the work was completed.
    assert_eq!(sent[0]["status"], json!("stopped"));
    assert_eq!(
        sent[0]["summary"],
        json!("The session moved the ticket to in_review.")
    );
    assert!(file.runs.is_empty());

    // A status that is not a plain word is not quoted.
    file.ticket_changed(
        &main_agent(),
        &ticket("in_progress"),
        &Scope::default(),
        3_000,
    );
    take(&mut file);
    file.ticket_changed(
        &main_agent(),
        &ticket("</b> DROP"),
        &Scope::default(),
        4_000,
    );
    assert_eq!(
        take(&mut file)[0]["summary"],
        json!("The session moved the ticket to bdrop.")
    );
}

#[test]
fn a_reconnected_session_ends_the_runs_started_under_its_old_name() {
    let mut file = known();
    file.ticket_changed(
        &main_agent(),
        &ticket("in_progress"),
        &Scope::default(),
        1_000,
    );
    take(&mut file);
    let renamed = agent_for_session(None, Some("claude-code".to_string()), "77b2aa0011223344");
    file.learn_identity(Some("77b2aa0011223344"), Some(&renamed));
    let sent = take(&mut file);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["status"], json!("stopped"));
    assert_eq!(sent[0]["summary"], json!(RECONNECTED));
    assert!(file.runs.is_empty());
    assert_eq!(file.agent, Some(renamed));
    // Hearing the same name again ends nothing.
    file.ticket_changed(
        &main_agent(),
        &ticket("in_progress"),
        &Scope::default(),
        2_000,
    );
    take(&mut file);
    file.learn_identity(Some("77b2aa0011223344"), None);
    assert!(file.pending.is_empty());
    assert_eq!(file.runs.len(), 1);
}

#[test]
fn a_refusal_ends_reporting_for_the_session() {
    let mut file = known();
    file.ticket_changed(
        &main_agent(),
        &ticket("in_progress"),
        &Scope::default(),
        1_000,
    );
    for reason in ["off", "not_available", "not_allowed"] {
        let mut refused = file.clone();
        let no = json!({"recorded": false, "reason": reason});
        refused.model_reported(&json!({"step": "x"}), &no, &Scope::default(), 2_000);
        assert!(refused.refused, "{reason}");
        assert!(refused.runs.is_empty() && refused.pending.is_empty());
        refused.ticket_changed(
            &main_agent(),
            &ticket("in_progress"),
            &Scope::default(),
            3_000,
        );
        assert!(refused.runs.is_empty() && refused.pending.is_empty());
    }
    // An API that was away for one call refuses nothing.
    let away = json!({"recorded": false, "reason": "unreachable"});
    file.model_reported(&json!({"step": "x"}), &away, &Scope::default(), 2_000);
    assert!(!file.refused);
    assert_eq!(file.runs.len(), 1);
}

#[test]
fn a_full_queue_keeps_the_last_word_on_a_run() {
    let mut file = known();
    file.ticket_changed(&main_agent(), &ticket("in_progress"), &Scope::default(), 0);
    file.end_all(SESSION_ENDED);
    for turn in 0..(MAX_PENDING as u64 * 2) {
        let mut body = Map::new();
        body.insert("step".into(), json!(turn));
        file.queue(body);
    }
    assert_eq!(file.pending.len(), MAX_PENDING);
    assert!(file.pending.iter().any(ends_a_run));
}

#[test]
fn a_process_that_is_gone_is_seen_as_gone() {
    // This test's own process is alive.
    let own = std::process::id();
    assert!(process_alive(own, process_start(own)));
    let mut file = SessionFile {
        host_pid: Some(own),
        host_start: process_start(own),
        ..SessionFile::default()
    };
    assert_eq!(host_state(&file), Host::Alive);
    file.host_pid = None;
    assert_eq!(host_state(&file), Host::Unknown);

    #[cfg(unix)]
    {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let pid = child.id();
        let start = process_start(pid);
        assert!(process_alive(pid, start));
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!process_alive(pid, start));
        file.host_pid = Some(pid);
        file.host_start = start;
        assert_eq!(host_state(&file), Host::Gone);
    }
    // A process id reused by another process is not the session's.
    #[cfg(target_os = "linux")]
    assert!(!process_alive(
        own,
        process_start(own).map(|start| start + 1)
    ));
}

#[test]
fn one_keep_alive_a_session_and_never_one_for_two() {
    let dir = tempfile::tempdir().unwrap();
    let first = dir.path().join("aaaa.json");
    let second = dir.path().join("bbbb.json");
    assert!(!beater_running(&first));
    let held = BeaterLock::take(&first).expect("the first keep-alive of a session starts");
    assert!(beater_running(&first));
    // A second one for the same session does not start.
    assert!(BeaterLock::take(&first).is_none());
    // Another session has its own.
    assert!(!beater_running(&second));
    let other = BeaterLock::take(&second).expect("another session has its own keep-alive");
    assert_ne!(beater_lock_path(&first), beater_lock_path(&second));
    drop(held);
    assert!(!beater_running(&first));
    assert!(beater_running(&second));
    drop(other);

    // Two host sessions never share a state file.
    let a = state_path("host-session-a").unwrap();
    let b = state_path("host-session-b").unwrap();
    assert_ne!(a, b);
    assert_eq!(a, state_path("host-session-a").unwrap());
    // The host's own session id is not in the file's name.
    assert!(!a.to_string_lossy().contains("host-session-a"));
}

fn structured(json: &Value) -> Answer<'_> {
    Answer {
        succeeded: true,
        json: Some(json),
        text: None,
    }
}

fn words(text: &str) -> Answer<'_> {
    Answer {
        succeeded: true,
        json: None,
        text: Some(text),
    }
}

#[test]
fn what_a_tool_call_means_is_read_from_the_tool_and_its_answer() {
    let identity = json!({"run_progress": {"session": SESSION, "agent": tool_agent()}});
    assert!(matches!(
        classify("init", &json!({}), &structured(&identity)),
        Some(Finding::Identity { .. })
    ));
    assert!(classify("init", &json!({}), &structured(&json!({"session_id": "x"}))).is_none());
    let recorded = json!({"run": {"run_id": RUN}});
    assert!(matches!(
        classify("progress", &json!({"step": "x"}), &structured(&recorded)),
        Some(Finding::Report(_))
    ));
    assert!(classify(
        "progress",
        &json!({"action": "list"}),
        &structured(&recorded)
    )
    .is_none());
    // A failed call means nothing.
    let failed = Answer {
        succeeded: false,
        json: Some(&recorded),
        text: Some("[PROGRESS] Recorded: step."),
    };
    assert!(classify("progress", &json!({"step": "x"}), &failed).is_none());

    let update = json!({"kind": "ticket", "action": "update", "id": TICKET, "body": {"status": "in_progress"}});
    assert!(matches!(
        classify("entity", &update, &structured(&ticket("in_progress"))),
        Some(Finding::Ticket(_))
    ));
    let nested = json!({"ticket": ticket("in_progress")});
    assert!(matches!(
        classify("entity", &update, &structured(&nested)),
        Some(Finding::Ticket(_))
    ));
    // An edit that sets no status, another kind, or a list is not a take-up.
    let retitle =
        json!({"kind": "ticket", "action": "update", "id": TICKET, "body": {"title": "x"}});
    assert!(classify("entity", &retitle, &structured(&ticket("in_progress"))).is_none());
    let handoff = json!({"kind": "handoff", "action": "update", "body": {"status": "in_progress"}});
    assert!(classify("entity", &handoff, &structured(&ticket("in_progress"))).is_none());
    let list = json!({"kind": "ticket", "action": "list"});
    assert!(classify(
        "entity",
        &list,
        &structured(&json!([ticket("in_progress")]))
    )
    .is_none());
    assert!(classify("memory", &update, &structured(&ticket("in_progress"))).is_none());
}

#[test]
fn a_local_server_answers_in_words_and_the_hooks_still_follow() {
    // `init` names the session on one line of its text.
    let identity = json!({"session": SESSION, "agent": tool_agent()});
    let init = format!(
        "Session ready for workspace: Testing\nResolved workspace_id: {WORKSPACE}\n{}: {identity}\n\n[ACCOUNT_CONTEXT]\nactive_mode=team",
        mcp_tools::domains::progress::IDENTITY_LINE
    );
    let Some(Finding::Identity { session, agent }) = classify("init", &json!({}), &words(&init))
    else {
        panic!("the identity line is read");
    };
    assert_eq!(session, SESSION);
    assert_eq!(agent, tool_agent());
    assert!(classify(
        "init",
        &json!({}),
        &words("Session ready for workspace: Testing")
    )
    .is_none());
    let mut file = SessionFile::default();
    file.learn_identity(Some(&session), Some(&agent));

    // A ticket update answered in words: the call names the ticket.
    let update = json!({"kind": "ticket", "action": "update", "id": TICKET, "workspace_id": WORKSPACE,
        "body": {"status": "in_progress"}});
    let said = format!("Updated ticket {TICKET}.");
    let Some(Finding::Ticket(taken)) = classify("entity", &update, &words(&said)) else {
        panic!("the ticket is read from the call");
    };
    file.ticket_changed(&main_agent(), &taken, &Scope::default(), 1_000);
    let started = take(&mut file);
    assert_eq!(started.len(), 1);
    assert_eq!(started[0]["source_run_id"], json!(ticket_key()));
    assert_eq!(started[0]["agent"], tool_agent());
    assert!(started[0].get("title").is_none());
    // A ticket named by its title, not its id, cannot be told in words.
    let by_title = json!({"kind": "ticket", "action": "update", "id": "the checkout ticket",
        "body": {"status": "in_progress"}});
    assert!(classify("entity", &by_title, &words("Updated ticket.")).is_none());

    // The model's report answered in words lands on the same run.
    let report = json!({"ticket_id": TICKET, "step": "Reading the handler"});
    let said = format!(
        "[PROGRESS] Recorded: step. state=working run_id={RUN}\nTicket \"x\": in_progress."
    );
    let Some(Finding::ReportInText { run_id, state }) =
        classify("progress", &report, &words(&said))
    else {
        panic!("a recorded report is read from its words");
    };
    assert_eq!(run_id.as_deref(), Some(RUN));
    assert_eq!(state.as_deref(), Some("working"));
    file.model_reported_in_text(
        &report,
        run_id.as_deref(),
        state.as_deref(),
        &Scope::default(),
        2_000,
    );
    assert!(file.pending.is_empty());
    assert_eq!(file.runs.len(), 1);
    assert_eq!(file.runs[0].run_id.as_deref(), Some(RUN));
    assert_eq!(file.base(&file.runs[0]).unwrap()["agent"], tool_agent());
    // A report with a key of its own is a run of its own.
    let keyed = json!({"run_key": "reviewer-2", "step": "Reviewing", "workspace_id": WORKSPACE});
    file.model_reported_in_text(&keyed, None, Some("working"), &Scope::default(), 2_500);
    assert_eq!(file.runs.len(), 2);
    assert_eq!(file.runs[1].key.as_deref(), Some("reviewer-2"));

    // A report that was not recorded changes nothing.
    assert!(classify(
        "progress",
        &report,
        &words("[PROGRESS] Not recorded: live progress is turned off on this machine.")
    )
    .is_none());
    // The model finishes the run, in words.
    let done = json!({"ticket_id": TICKET, "status": "completed", "summary": "Done."});
    file.model_reported_in_text(&done, Some(RUN), Some("finished"), &Scope::default(), 3_000);
    assert_eq!(file.runs.len(), 1);
    assert_eq!(file.runs[0].key.as_deref(), Some("reviewer-2"));

    // Without the server's word on who the session is, words are not enough.
    let mut unknown = SessionFile::default();
    unknown.model_reported_in_text(
        &report,
        Some(RUN),
        Some("working"),
        &Scope::default(),
        1_000,
    );
    assert!(unknown.runs.is_empty());
}

/// `classify` reads the answers of the tools in `ANSWERS_READ` and of no
/// other: that list is what setup's PostToolUse matcher is built from, so a
/// tool handled here and missing there would never reach this code in an
/// editor (1.0.77: `progress` and `entity`).
#[test]
fn classify_reads_the_listed_tools_and_no_other() {
    let report = json!({"run": {"run_id": "11111111-1111-4111-8111-111111111111"}});
    let answer = Answer {
        succeeded: true,
        json: Some(&report),
        text: None,
    };
    assert!(matches!(
        classify("progress", &json!({}), &answer),
        Some(Finding::Report(_))
    ));
    assert!(
        classify("progress_elsewhere", &json!({}), &answer).is_none(),
        "a tool outside the list is not read, whatever it answers"
    );
    let identity = json!({"run_progress": {"session": "s-1", "agent": {"id": "claude-code:1"}}});
    let answer = Answer {
        succeeded: true,
        json: Some(&identity),
        text: None,
    };
    assert!(matches!(
        classify("init", &json!({}), &answer),
        Some(Finding::Identity { .. })
    ));
    let ticket = json!({"id": "11111111-1111-4111-8111-111111111111", "status": "in_progress"});
    let answer = Answer {
        succeeded: true,
        json: Some(&ticket),
        text: None,
    };
    let call = json!({"kind": "ticket", "action": "update", "body": {"status": "in_progress"}});
    assert!(matches!(
        classify("entity", &call, &answer),
        Some(Finding::Ticket(_))
    ));
    assert_eq!(ANSWERS_READ, &["init", "progress", "entity"]);
}

/// What the hosted gateway answers when it sent a report that named no run on
/// to the session's one open run: by the run's id, so without a key.
fn sent_on_by_id(state: &str, liveness: &str) -> Value {
    let mut answer = answer(&ticket_key(), state);
    answer.as_object_mut().unwrap().remove("run_key");
    answer["run"]["liveness"] = json!(liveness);
    answer
}

#[test]
fn a_report_the_gateway_sent_on_lands_on_the_run_the_hooks_started_and_its_end_stands() {
    let mut file = known();
    file.ticket_changed(
        &main_agent(),
        &ticket("in_progress"),
        &Scope::default(),
        1_000,
    );
    take(&mut file);
    // The model asks a person, naming no run: no second run is kept.
    file.model_reported(
        &json!({"waiting": {"on": "person", "question": "Close it?"}}),
        &sent_on_by_id("waiting_input", "waiting"),
        &Scope::default(),
        2_000,
    );
    assert_eq!(file.runs.len(), 1);
    assert_eq!(file.runs[0].run_id.as_deref(), Some(RUN));
    assert_eq!(file.runs[0].key, Some(ticket_key()));
    // The model said what the run waits on: the turn's end adds nothing.
    file.turn_ended(3_000);
    assert!(take(&mut file).is_empty());
    // The model's own last word ends the run: moving the ticket on, a new
    // prompt and the turn's end send nothing over it.
    file.model_reported(
        &json!({"status": "completed", "summary": "Done."}),
        &sent_on_by_id("finished", "ended"),
        &Scope::default(),
        4_000,
    );
    assert!(file.runs.is_empty());
    file.ticket_changed(&main_agent(), &ticket("closed"), &Scope::default(), 5_000);
    file.prompt_submitted(6_000);
    file.turn_ended(7_000);
    assert!(take(&mut file).is_empty());
}

#[test]
fn a_report_that_named_no_run_is_never_guessed_onto_another_run() {
    let other_ticket = "55555555-5555-4555-8555-555555555555";
    // The run the answer names is on another ticket.
    let mut file = known();
    file.ticket_changed(
        &main_agent(),
        &ticket("in_progress"),
        &Scope::default(),
        1_000,
    );
    let mut elsewhere = sent_on_by_id("working", "live");
    elsewhere["run"]["scope"]["ticket_id"] = json!(other_ticket);
    file.model_reported(
        &json!({"step": "Reading"}),
        &elsewhere,
        &Scope::default(),
        2_000,
    );
    assert_eq!(file.runs.len(), 2);
    assert!(file.runs[0].run_id.is_none());
    // The report named its run: it is matched by that, never by this rule.
    let mut file = known();
    file.ticket_changed(
        &main_agent(),
        &ticket("in_progress"),
        &Scope::default(),
        1_000,
    );
    file.model_reported(
        &json!({"step": "Reading", "run_key": "my-own-run"}),
        &sent_on_by_id("working", "live"),
        &Scope::default(),
        2_000,
    );
    assert_eq!(file.runs.len(), 2);
    assert!(file.runs[0].run_id.is_none());
    // Two runs of the session's own: which one is not guessed.
    let mut file = known();
    file.ticket_changed(
        &main_agent(),
        &ticket("in_progress"),
        &Scope::default(),
        1_000,
    );
    let mut second = ticket("in_progress");
    second["id"] = json!(other_ticket);
    file.ticket_changed(&main_agent(), &second, &Scope::default(), 1_500);
    file.model_reported(
        &json!({"step": "Reading"}),
        &sent_on_by_id("working", "live"),
        &Scope::default(),
        2_000,
    );
    assert_eq!(file.runs.len(), 3);
    assert!(file.runs[..2].iter().all(|run| run.run_id.is_none()));
    // No run of its own: the hooks follow the run the server named, by its
    // id, as for any report of the model's, and claim no key for it.
    let mut file = known();
    file.model_reported(
        &json!({"step": "Reading"}),
        &sent_on_by_id("working", "live"),
        &Scope::default(),
        2_000,
    );
    assert_eq!(file.runs.len(), 1);
    assert_eq!(file.runs[0].run_id.as_deref(), Some(RUN));
    assert!(file.runs[0].key.is_none());
}
