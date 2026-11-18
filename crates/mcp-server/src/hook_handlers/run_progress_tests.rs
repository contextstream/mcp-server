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
    // The lock is a flock on the open file. A child process that another
    // test starts at this moment holds a copy of the descriptor from its
    // fork until its exec, so the lock can outlive the drop by that much.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while beater_running(&first) && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
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
    let Some(Finding::Identity { session, agent, .. }) =
        classify("init", &json!({}), &words(&init))
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

// ============================================================================
// Signs of life (CONTEXTSTREAM_RUN_PROGRESS_SIGNS)
// ============================================================================

#[test]
fn signs_of_life_are_off_as_shipped_and_on_only_where_turned_on() {
    let shipped = Settings::from_switches(None, None, None);
    assert_eq!(shipped.signs, SIGNS_DEFAULT);
    assert!(!shipped.signs && !shipped.enabled);
    for word in ["on", "1", "true", "yes", " ON "] {
        let settings = Settings::from_switches(None, None, Some(word));
        assert!(settings.signs && settings.signs_only(), "{word}");
        // Signs are not the automatic reports.
        assert!(!settings.enabled, "{word}");
    }
    for word in ["off", "0", "perhaps", ""] {
        assert!(
            !Settings::from_switches(None, None, Some(word)).signs,
            "{word:?}"
        );
    }
    // Live progress off wins.
    assert!(!Settings::from_switches(Some("off"), None, Some("on")).signs);
    // With the automatic reports on, they do the work (their own keep-alive).
    assert!(!Settings::from_switches(None, Some("on"), Some("on")).signs_only());
}

/// The session's run as a hosted report's answer names it, reported at `at`.
fn reported(file: &mut SessionFile, input: Value, view: Value, at: u64) {
    let answer = json!({
        "run": view,
        "written": ["run.step"],
        "session": SESSION,
        "run_key": "floor-live-runs",
    });
    file.model_reported(&input, &answer, &Scope::default(), at);
}

fn lane_view() -> Value {
    json!({
        "run_id": RUN,
        "workspace_id": WORKSPACE,
        "project_id": PROJECT,
        "state": "working",
        "liveness": "live",
        "agent": {"id": "floor-live-runs", "name": "floor-live-runs", "runtime": "mcp", "client": "claude-code"},
        "scope": {"workspace_id": WORKSPACE},
    })
}

#[test]
fn a_sign_of_life_follows_a_real_hook_event_and_says_only_who_and_which_run() {
    let every = MINUTE;
    let t0 = 1_000 * MINUTE;
    let mut file = known();
    reported(
        &mut file,
        json!({"run_key": "floor-live-runs", "step": "Building"}),
        lane_view(),
        t0,
    );
    // The report was a signal of its own, and nothing happened since.
    assert!(file.signs_due(every, t0 + 5 * MINUTE).is_empty());
    // A tool call half a minute on: due a minute after the report, not before.
    file.hook_event(t0 + MINUTE / 2);
    assert!(file.signs_due(every, t0 + MINUTE / 2).is_empty());
    let due = file.signs_due(every, t0 + MINUTE);
    assert_eq!(due.len(), 1, "{due:?}");
    // Who and which run, and nothing else: under the agent the run was
    // reported as (a heartbeat sets a run's agent), never the session's.
    assert_eq!(
        due[0].1,
        json!({"workspace_id": WORKSPACE, "run_id": RUN, "agent": lane_view()["agent"]})
    );
    file.runs[0].signed_ms = t0 + MINUTE;
    // Nothing more without another event, however long.
    assert!(file.signs_due(every, t0 + 30 * MINUTE).is_empty());
    // An event: due again, a minute after the last sign at the earliest.
    file.hook_event(t0 + MINUTE + 1_000);
    assert!(file.signs_due(every, t0 + MINUTE + 1_000).is_empty());
    assert_eq!(file.signs_due(every, t0 + 2 * MINUTE).len(), 1);
    // Nothing is queued: a sign is never a report of the hooks' own.
    assert!(file.pending.is_empty());
}

#[test]
fn no_sign_of_life_for_a_waiting_run_a_run_without_an_id_or_one_silent_for_hours() {
    let every = MINUTE;
    let t0 = 1_000 * MINUTE;
    // The model said the run waits.
    let mut file = known();
    let mut waiting = lane_view();
    waiting["state"] = json!("waiting_input");
    waiting["liveness"] = json!("waiting");
    reported(
        &mut file,
        json!({"run_key": "floor-live-runs", "waiting": {"on": "dependency", "question": "Review?"}}),
        waiting,
        t0,
    );
    assert_eq!(file.runs.len(), 1);
    file.hook_event(t0 + 5 * MINUTE);
    assert!(file.signs_due(every, t0 + 5 * MINUTE).is_empty());
    // A run known by key only: a heartbeat by key could start a run.
    let mut file = known();
    let mut keyed = lane_view();
    keyed["run_id"] = Value::Null;
    reported(
        &mut file,
        json!({"run_key": "floor-live-runs", "step": "Building"}),
        keyed,
        t0,
    );
    assert_eq!(file.runs.len(), 1);
    file.hook_event(t0 + 5 * MINUTE);
    assert!(file.signs_due(every, t0 + 5 * MINUTE).is_empty());
    // A run its agent has not reported on for four hours: silence is silence.
    let mut file = known();
    reported(
        &mut file,
        json!({"run_key": "floor-live-runs", "step": "Building"}),
        lane_view(),
        t0,
    );
    file.hook_event(t0 + 4 * 60 * MINUTE - MINUTE);
    assert_eq!(
        file.signs_due(every, t0 + 4 * 60 * MINUTE - MINUTE).len(),
        1
    );
    file.hook_event(t0 + 4 * 60 * MINUTE);
    assert!(file.signs_due(every, t0 + 4 * 60 * MINUTE).is_empty());
}

// ============================================================================
// The session's own run: a session that works without a report
// ============================================================================

const SPAN: u64 = 2 * MINUTE;
const PROJECT_NAME: &str = "mcp-server";

fn in_project() -> Scope {
    Scope {
        workspace_id: Some(WORKSPACE.to_string()),
        project_id: Some(PROJECT.to_string()),
    }
}

/// A session `init` answered at `at`: who it is, and where it works.
fn named_at(at: u64) -> SessionFile {
    let mut file = SessionFile::default();
    file.named(
        SESSION,
        &tool_agent(),
        &in_project(),
        Some(PROJECT_NAME),
        at,
    );
    file
}

fn session_key() -> String {
    default_run_key(SESSION, None)
}

/// What the `progress` tool answers for a report on the session's default run.
fn default_answer(state: &str) -> Value {
    json!({
        "run": {
            "run_id": RUN,
            "workspace_id": WORKSPACE,
            "project_id": PROJECT,
            "state": state,
            "liveness": if state == "working" { "live" } else { "ended" },
            "agent": tool_agent(),
            "scope": {"workspace_id": WORKSPACE},
        },
        "written": ["run.step"],
        "session": SESSION,
        "run_key": session_key(),
    })
}

#[test]
fn a_session_that_works_without_a_report_gets_a_run_of_its_own() {
    let mut file = named_at(1_000);
    // Knowing who the session is sends nothing, and neither does its first
    // while of work: the model's own report, with its own title, comes first.
    assert!(file.pending.is_empty());
    assert!(!file.session_run_due(SPAN, 1_000 + SPAN - 1));
    assert!(!file.open_session_run(&Scope::default(), SPAN, 1_000 + SPAN - 1));
    assert!(file.pending.is_empty() && file.runs.is_empty());

    // It has worked the span with no run: the hooks open the session's own.
    assert!(file.session_run_due(SPAN, 1_000 + SPAN));
    assert!(file.open_session_run(&Scope::default(), SPAN, 1_000 + SPAN));
    let sent = take(&mut file);
    assert_eq!(sent.len(), 1);
    let body = sent[0].as_object().unwrap();
    // The key the tool makes for a report that names no run, and the tool's
    // agent: the model's later report lands on this run under this name.
    assert_eq!(body["source_run_id"], json!(session_key()));
    assert_eq!(body["agent"], tool_agent());
    assert_eq!(body["group_id"], tool_agent()["id"]);
    // Where the session works, as `init` resolved it.
    assert_eq!(body["workspace_id"], json!(WORKSPACE));
    assert_eq!(body["project_id"], json!(PROJECT));
    assert_eq!(body["status"], json!("working"));
    // Fixed words and the project's own name: nothing of the work.
    assert_eq!(body["title"], json!("Working in mcp-server"));
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
            "title",
            "workspace_id"
        ]
    );
    assert_eq!(file.runs.len(), 1);
    assert!(file.runs[0].unreported);

    // One run, once: nothing more is due while it is open, or after it ended.
    assert!(!file.session_run_due(SPAN, 1_000 + 10 * SPAN));
    assert!(!file.open_session_run(&Scope::default(), SPAN, 1_000 + 10 * SPAN));
    file.end_all(SESSION_ENDED);
    take(&mut file);
    assert!(!file.session_run_due(SPAN, 1_000 + 20 * SPAN));
}

#[test]
fn the_span_counts_from_the_turns_start() {
    // A session that sat idle after `init` and then got a prompt: the model
    // has the same span to report its new work under its own title.
    let mut file = named_at(1_000);
    file.prompt_submitted(1_000 + 30 * MINUTE);
    assert!(!file.session_run_due(SPAN, 1_000 + 30 * MINUTE + SPAN - 1));
    assert!(file.session_run_due(SPAN, 1_000 + 30 * MINUTE + SPAN));
}

#[test]
fn a_session_whose_model_reports_gets_no_run_from_the_hooks() {
    let mut file = named_at(1_000);
    file.model_reported(
        &json!({"title": "Fix the flaky login test", "step": "Reading the test"}),
        &default_answer("working"),
        &Scope::default(),
        5_000,
    );
    assert!(file.pending.is_empty());
    assert_eq!(file.runs.len(), 1);
    assert!(!file.runs[0].unreported);
    assert!(!file.session_run_due(SPAN, 1_000 + SPAN));
    assert!(!file.open_session_run(&Scope::default(), SPAN, 1_000 + SPAN));

    // The model's word stands after its run is over, too: the hooks do not
    // speak for a session whose model has.
    file.model_reported(
        &json!({"status": "completed", "summary": "Fixed."}),
        &default_answer("finished"),
        &Scope::default(),
        9_000,
    );
    assert!(file.runs.is_empty());
    assert!(!file.session_run_due(SPAN, 1_000 + 30 * SPAN));
    assert!(file.pending.is_empty());
}

#[test]
fn a_session_on_a_ticket_gets_no_second_run() {
    let mut file = named_at(1_000);
    file.ticket_changed(
        &main_agent(),
        &ticket("in_progress"),
        &Scope::default(),
        2_000,
    );
    take(&mut file);
    assert!(!file.session_run_due(SPAN, 1_000 + SPAN));
    assert!(!file.open_session_run(&Scope::default(), SPAN, 1_000 + SPAN));
    assert_eq!(file.runs.len(), 1);
    assert!(file.pending.is_empty());
}

#[test]
fn the_models_report_makes_the_session_run_its_own() {
    let mut file = named_at(0);
    assert!(file.open_session_run(&Scope::default(), SPAN, SPAN));
    take(&mut file);

    // A report that names no run lands on the session's run by its key.
    let index = file.model_reported(
        &json!({"step": "Reading the handler"}),
        &default_answer("working"),
        &Scope::default(),
        SPAN + 5_000,
    );
    assert_eq!(index, Some(0));
    assert_eq!(file.runs.len(), 1);
    assert!(!file.runs[0].unreported);
    assert_eq!(file.runs[0].run_id.as_deref(), Some(RUN));
    // Nothing is ended and nothing is said by the hooks.
    assert!(file.pending.is_empty());
    // The model gave a step: the hooks leave it alone for ten minutes.
    file.tool_started(&main_agent(), "Edit", SPAN + 5 * MINUTE);
    assert!(file.pending.is_empty());
}

#[test]
fn work_reported_as_another_run_ends_the_unreported_session_run() {
    // Taking up a ticket: the ticket's run is the session's work now.
    let mut file = named_at(0);
    assert!(file.open_session_run(&Scope::default(), SPAN, SPAN));
    take(&mut file);
    file.ticket_changed(
        &main_agent(),
        &ticket("in_progress"),
        &Scope::default(),
        SPAN + 1_000,
    );
    let sent = take(&mut file);
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0]["source_run_id"], json!(session_key()));
    assert_eq!(sent[0]["status"], json!("stopped"));
    assert_eq!(sent[0]["summary"], json!(MOVED_ON));
    assert_eq!(sent[1]["source_run_id"], json!(ticket_key()));
    assert_eq!(sent[1]["status"], json!("working"));
    assert_eq!(file.runs.len(), 1);
    assert_eq!(file.runs[0].key, Some(ticket_key()));

    // The model reporting on a ticket does the same.
    let mut file = named_at(0);
    assert!(file.open_session_run(&Scope::default(), SPAN, SPAN));
    take(&mut file);
    file.model_reported(
        &json!({"ticket_id": TICKET, "step": "Reading"}),
        &answer(&ticket_key(), "working"),
        &Scope::default(),
        SPAN + 1_000,
    );
    let sent = take(&mut file);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["source_run_id"], json!(session_key()));
    assert_eq!(sent[0]["summary"], json!(MOVED_ON));
    assert_eq!(file.runs.len(), 1);
    assert_eq!(file.runs[0].key, Some(ticket_key()));

    // A subagent's ticket is its own work: the session is still at its own.
    let mut file = named_at(0);
    assert!(file.open_session_run(&Scope::default(), SPAN, SPAN));
    take(&mut file);
    file.ticket_changed(
        &explore(),
        &ticket("in_progress"),
        &Scope::default(),
        SPAN + 1_000,
    );
    let sent = take(&mut file);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["status"], json!("working"));
    assert_eq!(file.runs.len(), 2);
    assert!(file.runs[0].unreported);
}

#[test]
fn an_unreported_session_run_shows_the_kind_of_work_at_once() {
    let mut file = named_at(0);
    assert!(file.open_session_run(&Scope::default(), SPAN, SPAN));
    take(&mut file);

    // No step of the model's stands to be written over: the fixed sentence
    // for the tool's kind is said with the first tool call.
    file.tool_started(&main_agent(), "Bash", SPAN + 1);
    let sent = take(&mut file);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["source_run_id"], json!(session_key()));
    assert_eq!(
        sent[0]["step"],
        json!({"label": "Running a command", "kind": "run", "status": "started"})
    );
    // The same kind again says nothing, and another kind waits its minute.
    file.tool_started(&main_agent(), "Bash", SPAN + 5_000);
    file.tool_started(&main_agent(), "Read", SPAN + 30_000);
    assert!(file.pending.is_empty());
    file.tool_started(&main_agent(), "Read", SPAN + MINUTE + 2);
    assert_eq!(take(&mut file)[0]["step"]["label"], json!("Reading files"));
    // ContextStream's own calls are not the work.
    file.tool_started(
        &main_agent(),
        "mcp__contextstream__search",
        SPAN + 5 * MINUTE,
    );
    assert!(file.pending.is_empty());
}

#[test]
fn no_session_run_without_a_name_or_a_workspace() {
    // The server has not said who the session is.
    let mut file = SessionFile::default();
    assert!(!file.session_run_due(SPAN, 10 * SPAN));
    assert!(!file.open_session_run(&in_project(), SPAN, 10 * SPAN));
    assert!(file.pending.is_empty() && file.runs.is_empty());

    // `init` resolved no workspace and the folder is linked to none.
    let mut file = SessionFile::default();
    file.named(SESSION, &tool_agent(), &Scope::default(), None, 0);
    assert!(!file.open_session_run(&Scope::default(), SPAN, SPAN));
    assert!(file.pending.is_empty() && file.runs.is_empty());
    // Nothing was decided: the folder's own link still opens it.
    assert!(file.open_session_run(&in_project(), SPAN, SPAN));
    let sent = take(&mut file);
    assert_eq!(sent[0]["workspace_id"], json!(WORKSPACE));
    assert_eq!(sent[0]["project_id"], json!(PROJECT));
    // No project name was given: the fixed word alone.
    assert_eq!(sent[0]["title"], json!("Working"));

    // A refused session opens nothing.
    let mut file = named_at(0);
    file.refuse();
    assert!(!file.session_run_due(SPAN, SPAN));
    assert!(!file.open_session_run(&Scope::default(), SPAN, SPAN));
}

#[test]
fn a_session_whose_init_named_no_project_takes_the_folders_in_its_workspace() {
    let workspace_only = Scope {
        workspace_id: Some(WORKSPACE.to_string()),
        project_id: None,
    };
    let named = || {
        let mut file = SessionFile::default();
        file.named(SESSION, &tool_agent(), &workspace_only, None, 0);
        file
    };

    // The folder is linked to a project of that workspace (a session opened
    // from a parent folder, say): that is the project the session works in.
    let mut file = named();
    assert!(file.open_session_run(&in_project(), SPAN, SPAN));
    let sent = take(&mut file);
    assert_eq!(sent[0]["workspace_id"], json!(WORKSPACE));
    assert_eq!(sent[0]["project_id"], json!(PROJECT));
    assert_eq!(sent[0]["title"], json!("Working"));

    // A folder linked into another workspace says nothing about this one.
    let elsewhere = Scope {
        workspace_id: Some("99999999-9999-4999-8999-999999999999".to_string()),
        project_id: Some(PROJECT.to_string()),
    };
    let mut file = named();
    assert!(file.open_session_run(&elsewhere, SPAN, SPAN));
    let sent = take(&mut file);
    assert_eq!(sent[0]["workspace_id"], json!(WORKSPACE));
    assert!(sent[0].get("project_id").is_none());

    // No link at all: the session is still shown, in its workspace.
    let mut file = named();
    assert!(file.open_session_run(&Scope::default(), SPAN, SPAN));
    let sent = take(&mut file);
    assert_eq!(sent[0]["workspace_id"], json!(WORKSPACE));
    assert!(sent[0].get("project_id").is_none());

    // What `init` resolved is never replaced by the folder's link.
    let mut file = named_at(0);
    let other_project = Scope {
        workspace_id: Some(WORKSPACE.to_string()),
        project_id: Some("77777777-7777-4777-8777-777777777777".to_string()),
    };
    assert!(file.open_session_run(&other_project, SPAN, SPAN));
    assert_eq!(take(&mut file)[0]["project_id"], json!(PROJECT));
}

#[test]
fn a_project_name_is_one_short_line() {
    let mut file = SessionFile::default();
    let long = format!("  api\n{}  ", "x".repeat(300));
    file.named(SESSION, &tool_agent(), &in_project(), Some(&long), 0);
    assert!(file.open_session_run(&Scope::default(), SPAN, SPAN));
    let title = take(&mut file)[0]["title"].as_str().unwrap().to_string();
    assert!(title.starts_with("Working in api x"), "{title}");
    assert!(!title.contains('\n'));
    assert!(title.chars().count() <= 100, "{}", title.chars().count());
}

#[test]
fn a_reconnected_session_gets_a_run_under_its_new_name() {
    let mut file = named_at(0);
    assert!(file.open_session_run(&Scope::default(), SPAN, SPAN));
    take(&mut file);

    let other = "77b2aa00112233445566778899aabbcc";
    let agent = agent_for_session(None, Some("claude-code".to_string()), other);
    file.named(other, &agent, &in_project(), Some(PROJECT_NAME), 10 * SPAN);
    let sent = take(&mut file);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0]["source_run_id"], json!(session_key()));
    assert_eq!(sent[0]["summary"], json!(RECONNECTED));
    assert!(file.runs.is_empty());

    // The new part has its own span, and its own run after it.
    assert!(!file.session_run_due(SPAN, 11 * SPAN - 1));
    assert!(file.open_session_run(&Scope::default(), SPAN, 11 * SPAN));
    let sent = take(&mut file);
    assert_eq!(
        sent[0]["source_run_id"],
        json!(default_run_key(other, None))
    );
    assert_eq!(sent[0]["agent"], agent);
}

#[test]
fn the_session_run_waits_at_the_turns_end_and_ends_with_the_session() {
    let mut file = named_at(0);
    assert!(file.open_session_run(&Scope::default(), SPAN, SPAN));
    take(&mut file);

    file.turn_ended(SPAN + 1_000);
    let sent = take(&mut file);
    assert_eq!(sent[0]["status"], json!("waiting_input"));
    assert_eq!(sent[0]["waiting"], json!({"on": "person"}));
    file.prompt_submitted(SPAN + 2_000);
    assert_eq!(take(&mut file)[0]["status"], json!("working"));

    file.end_all(SESSION_ENDED);
    let sent = take(&mut file);
    assert_eq!(sent[0]["source_run_id"], json!(session_key()));
    assert_eq!(sent[0]["status"], json!("stopped"));
    assert_eq!(sent[0]["summary"], json!(SESSION_ENDED));
}

#[test]
fn init_says_where_the_session_works() {
    let identity = json!({"session": SESSION, "agent": tool_agent()});
    // The hosted gateway: the structured answer.
    let json = json!({
        "workspace_id": WORKSPACE,
        "project_id": PROJECT,
        "project": {"id": PROJECT, "name": PROJECT_NAME},
        "run_progress": identity,
    });
    match classify("init", &json!({}), &structured(&json)) {
        Some(Finding::Identity {
            session,
            scope,
            project_name,
            ..
        }) => {
            assert_eq!(session, SESSION);
            assert_eq!(scope.workspace_id.as_deref(), Some(WORKSPACE));
            assert_eq!(scope.project_id.as_deref(), Some(PROJECT));
            assert_eq!(project_name.as_deref(), Some(PROJECT_NAME));
        }
        _ => panic!("init names the session"),
    }
    // A local server: the same in words.
    let said = format!(
        "Session ready for workspace: Engineering, project: {PROJECT_NAME}\nResolved workspace_id: {WORKSPACE}\nResolved project_id: {PROJECT}\n{}: {identity}",
        mcp_tools::domains::progress::IDENTITY_LINE
    );
    match classify("init", &json!({}), &words(&said)) {
        Some(Finding::Identity {
            scope,
            project_name,
            ..
        }) => {
            assert_eq!(scope.workspace_id.as_deref(), Some(WORKSPACE));
            assert_eq!(scope.project_id.as_deref(), Some(PROJECT));
            assert_eq!(project_name.as_deref(), Some(PROJECT_NAME));
        }
        _ => panic!("init names the session in words"),
    }
    // No workspace resolved: who the session is, and no place.
    let said = format!(
        "Session initialized without a resolved workspace_id.\n{}: {identity}",
        mcp_tools::domains::progress::IDENTITY_LINE
    );
    match classify("init", &json!({}), &words(&said)) {
        Some(Finding::Identity {
            scope,
            project_name,
            ..
        }) => {
            assert!(scope.workspace_id.is_none() && scope.project_id.is_none());
            assert!(project_name.is_none());
        }
        _ => panic!("init names the session"),
    }
}

#[test]
fn the_span_before_the_session_run_is_two_minutes_unless_set() {
    assert_eq!(session_run_after_ms_of(None), 2 * MINUTE);
    assert_eq!(session_run_after_ms_of(Some("0")), 0);
    assert_eq!(session_run_after_ms_of(Some(" 45 ")), 45_000);
    // Anything else, or more than an hour, is the default.
    assert_eq!(session_run_after_ms_of(Some("soon")), 2 * MINUTE);
    assert_eq!(session_run_after_ms_of(Some("-1")), 2 * MINUTE);
    assert_eq!(session_run_after_ms_of(Some("3601")), 2 * MINUTE);
}

// ============================================================================
// Project details: a run reported without a project
// ============================================================================

const OTHER_WORKSPACE: &str = "99999999-9999-4999-8999-999999999999";
const OTHER_PROJECT: &str = "77777777-7777-4777-8777-777777777777";

/// A `progress` answer for a run the server had no project for (a session
/// opened from a parent folder, or a stateless call that named none).
fn answer_without_a_project() -> Value {
    json!({
        "run": {
            "run_id": RUN,
            "workspace_id": WORKSPACE,
            "project_id": null,
            "state": "working",
            "liveness": "live",
            "agent": tool_agent(),
            "scope": {"workspace_id": WORKSPACE},
        },
        "written": ["run.step"],
        "session": SESSION,
        "run_key": session_key(),
    })
}

fn signal_of(file: &SessionFile, index: usize) -> Map<String, Value> {
    file.base(&file.runs[index]).expect("the run has a signal")
}

#[test]
fn a_run_reported_without_a_project_is_kept_in_the_sessions_project() {
    let report = json!({"title": "Fix the flaky login test", "step": "Reading the test"});

    // `init` resolved the session's project: the run's signals carry it, and
    // the API gives the run its project from the first of them.
    let mut file = named_at(0);
    let index = file
        .model_reported(
            &report,
            &answer_without_a_project(),
            &Scope::default(),
            1_000,
        )
        .unwrap();
    assert_eq!(signal_of(&file, index)["workspace_id"], json!(WORKSPACE));
    assert_eq!(signal_of(&file, index)["project_id"], json!(PROJECT));

    // `init` named no project: the folder's, in the run's workspace.
    let mut file = known();
    let index = file
        .model_reported(&report, &answer_without_a_project(), &in_project(), 1_000)
        .unwrap();
    assert_eq!(signal_of(&file, index)["project_id"], json!(PROJECT));

    // A project of another workspace is not this run's.
    let elsewhere = Scope {
        workspace_id: Some(OTHER_WORKSPACE.to_string()),
        project_id: Some(OTHER_PROJECT.to_string()),
    };
    let mut file = known();
    let index = file
        .model_reported(&report, &answer_without_a_project(), &elsewhere, 1_000)
        .unwrap();
    assert!(!signal_of(&file, index).contains_key("project_id"));
    let mut file = SessionFile::default();
    let scope = Scope {
        workspace_id: Some(OTHER_WORKSPACE.to_string()),
        project_id: Some(OTHER_PROJECT.to_string()),
    };
    file.named(SESSION, &tool_agent(), &scope, None, 0);
    let index = file
        .model_reported(
            &report,
            &answer_without_a_project(),
            &Scope::default(),
            1_000,
        )
        .unwrap();
    assert!(!signal_of(&file, index).contains_key("project_id"));

    // Nothing known: nothing is made up.
    let mut file = known();
    let index = file
        .model_reported(
            &report,
            &answer_without_a_project(),
            &Scope::default(),
            1_000,
        )
        .unwrap();
    assert!(!signal_of(&file, index).contains_key("project_id"));
}

#[test]
fn the_project_a_report_named_stands() {
    // The run is in a project of its own: neither what `init` resolved nor
    // the folder's link replaces it.
    let mut answer = answer_without_a_project();
    answer["run"]["project_id"] = json!(OTHER_PROJECT);
    let mut file = named_at(0);
    let index = file
        .model_reported(&json!({"step": "Reading"}), &answer, &in_project(), 1_000)
        .unwrap();
    assert_eq!(signal_of(&file, index)["project_id"], json!(OTHER_PROJECT));
    // A later answer that names none takes nothing away.
    let index = file
        .model_reported(
            &json!({"step": "Editing"}),
            &answer_without_a_project(),
            &in_project(),
            2_000,
        )
        .unwrap();
    assert_eq!(signal_of(&file, index)["project_id"], json!(OTHER_PROJECT));
}
