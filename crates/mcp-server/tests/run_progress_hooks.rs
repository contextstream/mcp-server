//! Drive the real hook binary against a fake API: what an ordinary session's
//! hooks send to live run progress, and when they stop.
//!
//! - A session that takes up a ticket and then says nothing for a while (one
//!   long tool call) stays live: its detached keep-alive signals the run.
//! - A session whose process dies ends its run within seconds, by a fixed
//!   sentence, and its keep-alive process leaves.
//! - Two sessions on one machine have a keep-alive each and never report for
//!   one another.
//! - A session that works and never reports is shown all the same: once it
//!   has worked a while with no run, the hooks open the session's own, with
//!   its agent and the project `init` resolved, and end it with the session.
//!   A session whose model reports keeps its own run and title.
//! - As shipped (`CONTEXTSTREAM_RUN_PROGRESS_AUTO` unset, or `off`), and with
//!   `CONTEXTSTREAM_RUN_PROGRESS=off`, nothing is sent and nothing is written.
//!
//! Every run gets its own HOME and a made-up key; the API is a listener on
//! localhost. The "session's process" is a `sleep` this test starts and names
//! to the hooks, because the test itself is not an editor.
#![cfg(unix)]

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const WORKSPACE: &str = "11111111-1111-4111-8111-111111111111";
const PROJECT: &str = "22222222-2222-4222-8222-222222222222";
const SESSION_A: &str = "3fa9c2d17e0b5a66aa11bb22cc33dd44";
const SESSION_B: &str = "77b2aa00112233445566778899aabbcc";
const TICKET_A: &str = "33333333-3333-4333-8333-333333333333";
/// A sandbox's word for "this variable is not in the hooks' environment".
const UNSET: &str = "<unset>";
const TICKET_B: &str = "55555555-5555-4555-8555-555555555555";
const TERMINAL: &[&str] = &["completed", "partial", "stopped", "failed", "cancelled"];
/// A project the made-up key may read and not write: the fake API answers a
/// report that names it with 403, as the API does.
const READ_ONLY_PROJECT: &str = "88888888-8888-4888-8888-888888888888";

/// The longest a keep-alive may outlive its session's process (what the docs
/// state), and the longest the tests wait for anything else.
const LEAVES_WITHIN: Duration = Duration::from_secs(30);
const SOON: Duration = Duration::from_secs(15);

#[derive(Clone)]
struct Report {
    at: Instant,
    body: Value,
}

/// A stand-in for the ContextStream API: it keeps every report it is sent and
/// answers the way the API does.
#[derive(Clone)]
struct FakeApi {
    port: u16,
    reports: Arc<Mutex<Vec<Report>>>,
}

impl FakeApi {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let api = Self {
            port: listener.local_addr().unwrap().port(),
            reports: Arc::new(Mutex::new(Vec::new())),
        };
        let reports = api.reports.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let reports = reports.clone();
                std::thread::spawn(move || serve(stream, &reports));
            }
        });
        api
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn reports(&self) -> Vec<Report> {
        self.reports.lock().unwrap().clone()
    }

    /// The first report that fits, waiting up to `within` for it.
    fn wait_for(&self, within: Duration, fits: impl Fn(&Value) -> bool) -> Option<Report> {
        let deadline = Instant::now() + within;
        loop {
            if let Some(found) = self.reports().into_iter().find(|report| fits(&report.body)) {
                return Some(found);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn serve(mut stream: TcpStream, reports: &Mutex<Vec<Report>>) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut request = Vec::new();
    let mut chunk = [0_u8; 4096];
    let (head_end, length) = loop {
        let Ok(read) = stream.read(&mut chunk) else {
            return;
        };
        if read == 0 {
            return;
        }
        request.extend_from_slice(&chunk[..read]);
        if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse::<usize>().ok())
                .unwrap_or(0);
            break (end + 4, length);
        }
    };
    while request.len() < head_end + length {
        match stream.read(&mut chunk) {
            Ok(read) if read > 0 => request.extend_from_slice(&chunk[..read]),
            _ => break,
        }
    }
    let first_line = String::from_utf8_lossy(&request[..head_end])
        .lines()
        .next()
        .unwrap_or_default()
        .to_string();
    let mut status = "200 OK";
    let answer = if first_line.contains("/api/v1/run-progress/report") {
        let body: Value = serde_json::from_slice(&request[head_end..]).unwrap_or(Value::Null);
        if body["project_id"] == READ_ONLY_PROJECT {
            status = "403 Forbidden";
        }
        let ended = body["status"]
            .as_str()
            .is_some_and(|status| TERMINAL.contains(&status));
        let answer = json!({
            "success": true,
            "data": {
                "run": {
                    "run_id": body["run_id"].as_str().unwrap_or("66666666-6666-4666-8666-666666666666"),
                    "workspace_id": body["workspace_id"],
                    "state": if ended { "stopped" } else { "working" },
                    "liveness": if ended { "ended" } else { "live" },
                    "agent": body["agent"],
                },
                "written": [],
                "cursor": "1",
            },
        });
        reports.lock().unwrap().push(Report {
            at: Instant::now(),
            body,
        });
        answer
    } else {
        json!({"success": true, "data": {}})
    };
    let text = answer.to_string();
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        text.len(),
        text
    );
}

/// One editor session: the host's id for it, what the server calls it, and
/// the process the hooks watch.
struct Session {
    host_session: &'static str,
    server_session: &'static str,
    process: Child,
}

impl Session {
    fn new(host_session: &'static str, server_session: &'static str) -> Self {
        Self {
            host_session,
            server_session,
            process: Command::new("sleep").arg("600").spawn().unwrap(),
        }
    }

    /// The agent the server reports this session as ("Claude Code 3fa9").
    fn agent(&self) -> Value {
        json!({
            "id": format!("claude-code:{}", &self.server_session[..8]),
            "name": format!("Claude Code {}", &self.server_session[..4]),
            "runtime": "mcp",
            "client": "claude-code",
        })
    }

    fn key(&self, ticket: &str) -> String {
        format!("{}:{ticket}", self.server_session)
    }

    /// The session's process dies without a word (a crash, a closed terminal).
    fn die(&mut self) -> Instant {
        self.process.kill().unwrap();
        self.process.wait().unwrap();
        Instant::now()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

struct Sandbox {
    home: tempfile::TempDir,
    repo: PathBuf,
    api: FakeApi,
    /// Extra environment for every hook of this sandbox; [`UNSET`] takes a
    /// variable out of it.
    env: Vec<(&'static str, &'static str)>,
}

impl Sandbox {
    fn new(api: &FakeApi, env: &[(&'static str, &'static str)]) -> Self {
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        Self {
            home,
            repo,
            api: api.clone(),
            env: env.to_vec(),
        }
    }

    fn hook(&self, session: &Session, hook: &str, input: Value) {
        self.hook_stdout(session, hook, input);
    }

    /// Run a hook and give back what it printed.
    fn hook_stdout(&self, session: &Session, hook: &str, mut input: Value) -> String {
        input["session_id"] = session.host_session.into();
        input["cwd"] = self.repo.to_string_lossy().into_owned().into();
        let mut command = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"));
        command
            .args(["hook", hook, "--contextstream-managed-hook=v1"])
            .env_clear()
            .env("HOME", self.home.path())
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("CONTEXTSTREAM_API_URL", self.api.url())
            .env("CONTEXTSTREAM_API_KEY", "a-made-up-key-for-this-test")
            .env("CONTEXTSTREAM_REMINDER_ENABLED", "false")
            // The hooks report only where a person turned that on; the tests
            // of the switches say otherwise.
            .env("CONTEXTSTREAM_RUN_PROGRESS_AUTO", "on")
            // One heartbeat a second, so a few seconds show the pattern.
            .env("CONTEXTSTREAM_RUN_PROGRESS_BEAT_SECS", "1")
            .env(
                "CONTEXTSTREAM_RUN_PROGRESS_HOST_PID",
                session.process.id().to_string(),
            )
            .current_dir(&self.repo)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (name, value) in &self.env {
            if *value == UNSET {
                command.env_remove(name);
            } else {
                command.env(name, value);
            }
        }
        let mut child = command.spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.to_string().as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{hook}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn tool_result(&self, session: &Session, tool: &str, tool_input: Value, structured: Value) {
        self.hook(
            session,
            "post-tool-use",
            json!({
                "hook_event_name": "PostToolUse",
                "tool_name": format!("mcp__contextstream__{tool}"),
                "tool_input": tool_input,
                "tool_response": {
                    "isError": false,
                    "content": [{"type": "text", "text": "ok"}],
                    "structuredContent": structured,
                },
            }),
        );
    }

    /// `init` answered, as a 1.0.77 server does: with who the session is.
    fn init(&self, session: &Session) {
        self.tool_result(
            session,
            "init",
            json!({"folder_path": self.repo}),
            json!({
                "session_id": "99999999-9999-4999-8999-999999999999",
                "workspace_id": WORKSPACE,
                "project_id": PROJECT,
                "run_progress": {"session": session.server_session, "agent": session.agent()},
            }),
        );
    }

    /// `init` answered as a current server does: who the session is, and the
    /// workspace and project it works in.
    fn init_in(&self, session: &Session, project_name: &str) {
        self.init_in_project(session, PROJECT, project_name);
    }

    fn init_in_project(&self, session: &Session, project: &str, project_name: &str) {
        self.tool_result(
            session,
            "init",
            json!({"folder_path": self.repo}),
            json!({
                "session_id": "99999999-9999-4999-8999-999999999999",
                "workspace_id": WORKSPACE,
                "project_id": project,
                "project": {"id": project, "name": project_name},
                "run_progress": {"session": session.server_session, "agent": session.agent()},
            }),
        );
    }

    /// A tool call of the session's own work starts.
    fn works(&self, session: &Session, tool: &str) {
        self.hook(
            session,
            "pre-tool-use",
            json!({"hook_event_name": "PreToolUse", "tool_name": tool, "tool_input": {"file_path": "/tmp/x"}}),
        );
    }

    /// The session moved a ticket to `status` with the `entity` tool.
    fn set_ticket(&self, session: &Session, ticket: &str, status: &str) {
        self.tool_result(
            session,
            "entity",
            json!({"kind": "ticket", "action": "update", "id": ticket, "body": {"status": status}}),
            json!({
                "id": ticket,
                "status": status,
                "title": "QA test: live progress",
                "workspace_id": WORKSPACE,
                "project_id": PROJECT,
            }),
        );
    }

    /// A tool result as a local server gives it: words, no structured part.
    fn tool_words(&self, session: &Session, tool: &str, tool_input: Value, text: &str) {
        self.hook(
            session,
            "post-tool-use",
            json!({
                "hook_event_name": "PostToolUse",
                "tool_name": format!("mcp__contextstream__{tool}"),
                "tool_input": tool_input,
                "tool_response": {"isError": false, "content": [{"type": "text", "text": text}]},
            }),
        );
    }

    fn state_dir(&self) -> PathBuf {
        self.home.path().join(".contextstream").join("run-progress")
    }

    /// The process id of the session's keep-alive, as it wrote it.
    fn keep_alive_pid(&self, session: &Session) -> Option<u32> {
        for entry in std::fs::read_dir(self.state_dir()).ok()?.flatten() {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            let state: Value = serde_json::from_slice(&std::fs::read(&path).ok()?).ok()?;
            if state["session"] == session.server_session {
                let pid = std::fs::read_to_string(path.with_extension("beater")).ok()?;
                return pid.trim().parse().ok();
            }
        }
        None
    }
}

/// Whether a process still runs (a zombie nobody reaped does not).
fn runs(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        let state = stat.rfind(')').and_then(|close| {
            stat[close + 1..]
                .split_whitespace()
                .next()
                .map(str::to_string)
        });
        state.as_deref() != Some("Z")
    }
    #[cfg(not(target_os = "linux"))]
    {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }
}

fn wait_until_gone(pid: u32, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if !runs(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    !runs(pid)
}

fn is_for(body: &Value, key: &str) -> bool {
    body["source_run_id"] == key
}

fn is_heartbeat(body: &Value) -> bool {
    body.get("status").is_none() && body.get("step").is_none()
}

/// The API's own rule: a run reads as stalled after `max(30 s, 3 x heartbeat)`
/// without a signal.
fn stalls_after(body: &Value) -> Duration {
    Duration::from_secs((3 * body["heartbeat_s"].as_u64().unwrap_or(30)).max(30))
}

#[test]
fn a_silent_working_session_stays_live_and_a_dead_one_ends() {
    let api = FakeApi::start();
    let sandbox = Sandbox::new(&api, &[]);
    let mut session = Session::new("host-session-a", SESSION_A);
    let key = session.key(TICKET_A);

    sandbox.init(&session);
    assert!(
        api.reports().is_empty(),
        "knowing who the session is sends nothing"
    );
    sandbox.set_ticket(&session, TICKET_A, "in_progress");

    // The run starts by itself: the tool's key, the tool's agent, the ticket.
    let started = api
        .wait_for(SOON, |body| {
            is_for(body, &key) && body["status"] == "working"
        })
        .expect("taking up the ticket starts a run");
    assert_eq!(started.body["agent"], session.agent());
    assert_eq!(started.body["ticket_id"], TICKET_A);
    assert_eq!(started.body["workspace_id"], WORKSPACE);
    assert_eq!(started.body["title"], "QA test: live progress");
    assert!(started.body.get("step").is_none());

    // The session now works without a word: no hook fires for eight seconds
    // (one long tool call). Its process is alive, so the run is signalled.
    std::thread::sleep(Duration::from_secs(8));
    let signals: Vec<Report> = api
        .reports()
        .into_iter()
        .filter(|report| is_for(&report.body, &key))
        .collect();
    let beats: Vec<&Report> = signals
        .iter()
        .filter(|report| is_heartbeat(&report.body))
        .collect();
    assert!(
        beats.len() >= 3,
        "{} heartbeats in eight seconds",
        beats.len()
    );
    for beat in &beats {
        // Who and which run; nothing about the work.
        let mut keys: Vec<&str> = beat
            .body
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "agent",
                "group_id",
                "heartbeat_s",
                "project_id",
                "source_run_id",
                "workspace_id"
            ]
        );
        assert_eq!(beat.body["agent"], session.agent());
    }
    // Never silent long enough to read as stalled, and still open.
    let stalls = stalls_after(&started.body);
    for pair in signals.windows(2) {
        assert!(
            pair[1].at - pair[0].at < stalls,
            "a gap of {:?}",
            pair[1].at - pair[0].at
        );
    }
    assert!(signals.last().unwrap().at.elapsed() < stalls);
    assert!(!signals
        .iter()
        .any(|report| report.body["status"] == "stopped"));
    let keep_alive = sandbox.keep_alive_pid(&session).expect("a keep-alive runs");
    assert!(runs(keep_alive));

    // The session's process dies without a word.
    let died = session.die();
    let ended = api
        .wait_for(LEAVES_WITHIN, |body| {
            is_for(body, &key) && body["status"] == "stopped"
        })
        .expect("a dead session's run ends");
    assert!(ended.at - died < LEAVES_WITHIN);
    assert_eq!(
        ended.body["summary"],
        "The session's process ended without a final report."
    );
    assert_eq!(ended.body["agent"], session.agent());
    // The keep-alive does not outlive its session.
    assert!(
        wait_until_gone(
            keep_alive,
            LEAVES_WITHIN - died.elapsed().min(LEAVES_WITHIN)
        ),
        "the keep-alive process still runs {:?} after its session died",
        died.elapsed()
    );
    let sent = api.reports().len();
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(api.reports().len(), sent, "nothing is sent after the end");
}

#[test]
fn two_sessions_have_a_keep_alive_each_and_report_only_for_themselves() {
    let api = FakeApi::start();
    let sandbox = Sandbox::new(&api, &[]);
    let first = Session::new("host-session-a", SESSION_A);
    let mut second = Session::new("host-session-b", SESSION_B);
    let (key_a, key_b) = (first.key(TICKET_A), second.key(TICKET_B));

    sandbox.init(&first);
    sandbox.init(&second);
    sandbox.set_ticket(&first, TICKET_A, "in_progress");
    sandbox.set_ticket(&second, TICKET_B, "in_progress");
    api.wait_for(SOON, |body| is_for(body, &key_a))
        .expect("the first run starts");
    api.wait_for(SOON, |body| is_for(body, &key_b))
        .expect("the second run starts");
    std::thread::sleep(Duration::from_secs(4));

    // Two processes, one a session.
    let pid_a = sandbox
        .keep_alive_pid(&first)
        .expect("the first session's keep-alive");
    let pid_b = sandbox
        .keep_alive_pid(&second)
        .expect("the second session's keep-alive");
    assert_ne!(pid_a, pid_b);
    assert!(runs(pid_a) && runs(pid_b));
    // Two robots: each run only ever under its own session's agent.
    assert_ne!(first.agent()["id"], second.agent()["id"]);
    for report in api.reports() {
        let (agent, ticket) = if is_for(&report.body, &key_a) {
            (first.agent(), TICKET_A)
        } else {
            assert!(is_for(&report.body, &key_b), "{}", report.body);
            (second.agent(), TICKET_B)
        };
        assert_eq!(report.body["agent"], agent, "{}", report.body);
        if let Some(named) = report.body.get("ticket_id") {
            assert_eq!(named, ticket);
        }
    }

    // The second session's turn ends, then its person answers.
    sandbox.hook(&second, "stop", json!({"hook_event_name": "Stop"}));
    let waits = api
        .wait_for(SOON, |body| {
            is_for(body, &key_b) && body["status"] == "waiting_input"
        })
        .expect("a turn that ended waits on a person");
    assert_eq!(waits.body["waiting"], json!({"on": "person"}));
    sandbox.hook(
        &second,
        "user-prompt-submit",
        json!({"hook_event_name": "UserPromptSubmit", "prompt": "carry on"}),
    );
    let resumed = api
        .wait_for(SOON, |body| {
            is_for(body, &key_b) && body["status"] == "working" && body.get("ticket_id").is_none()
        })
        .expect("a prompt says working again");
    assert!(resumed.at > waits.at);
    assert!(!resumed.body.to_string().contains("carry on"));

    // The first session ends: its run ends at once, by the hook itself.
    sandbox.hook(
        &first,
        "session-end",
        json!({"hook_event_name": "SessionEnd", "reason": "other"}),
    );
    let ended = api
        .wait_for(Duration::from_secs(2), |body| {
            is_for(body, &key_a) && body["status"] == "stopped"
        })
        .expect("a session that ends ends its run");
    assert_eq!(
        ended.body["summary"],
        "The session ended without a final report."
    );
    assert!(
        wait_until_gone(pid_a, SOON),
        "the ended session's keep-alive still runs"
    );
    // The other session is untouched and still signalled.
    assert!(runs(pid_b));
    let after = Instant::now();
    assert!(api
        .wait_for(SOON, |body| is_for(body, &key_b) && is_heartbeat(body))
        .is_some());
    std::thread::sleep(Duration::from_secs(3));
    assert!(api.reports().iter().any(|report| report.at > after
        && is_for(&report.body, &key_b)
        && is_heartbeat(&report.body)));
    assert!(!api
        .reports()
        .iter()
        .any(|report| report.at > ended.at && is_for(&report.body, &key_a)));

    let died = second.die();
    api.wait_for(LEAVES_WITHIN, |body| {
        is_for(body, &key_b) && body["status"] == "stopped"
    })
    .expect("the second session's run ends when its process dies");
    assert!(wait_until_gone(
        pid_b,
        LEAVES_WITHIN - died.elapsed().min(LEAVES_WITHIN)
    ));
}

#[test]
fn a_run_the_model_reported_through_an_older_server_is_kept_alive_until_it_is_finished() {
    let api = FakeApi::start();
    let sandbox = Sandbox::new(&api, &[]);
    let session = Session::new("host-session-a", SESSION_A);

    // The model reports through a server that says nothing of the session
    // (an older gateway): the run is known by the id the answer names.
    let run_id = "44444444-4444-4444-8444-444444444444";
    let view = json!({
        "run_id": run_id,
        "workspace_id": WORKSPACE,
        "project_id": PROJECT,
        "state": "working",
        "liveness": "live",
        "agent": {"id": "claude-code", "name": "claude-code", "runtime": "mcp", "client": "claude-code"},
        "scope": {"workspace_id": WORKSPACE, "ticket_id": TICKET_A},
    });
    sandbox.tool_result(
        &session,
        "progress",
        json!({"ticket_id": TICKET_A, "step": "Reading the handler in src/checkout.rs"}),
        json!({"run": view, "written": ["run.step"], "cursor": "7"}),
    );
    let beat = api
        .wait_for(SOON, |body| body["run_id"] == run_id)
        .expect("a run the model reported is kept alive");
    assert!(is_heartbeat(&beat.body));
    // Under the agent the tool itself reported, so the name never changes.
    assert_eq!(beat.body["agent"], view["agent"]);
    assert!(!beat.body.to_string().contains("checkout"));

    // The model finishes the run: the hooks leave it alone from then on.
    let mut finished = view.clone();
    finished["state"] = "finished".into();
    finished["liveness"] = "ended".into();
    sandbox.tool_result(
        &session,
        "progress",
        json!({"ticket_id": TICKET_A, "status": "completed", "summary": "Done."}),
        json!({"run": finished, "written": ["run.finished"], "cursor": "9"}),
    );
    std::thread::sleep(Duration::from_secs(2));
    let sent = api.reports().len();
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(api.reports().len(), sent);
    assert!(!api
        .reports()
        .iter()
        .any(|report| report.body.get("status").is_some()));
}

#[test]
fn nothing_is_sent_or_written_as_shipped() {
    a_whole_session_leaves_no_trace(&[("CONTEXTSTREAM_RUN_PROGRESS_AUTO", UNSET)]);
}

#[test]
fn nothing_is_sent_or_written_when_the_hooks_reports_are_said_off() {
    a_whole_session_leaves_no_trace(&[("CONTEXTSTREAM_RUN_PROGRESS_AUTO", "off")]);
}

#[test]
fn nothing_is_sent_or_written_when_live_progress_is_off() {
    // The hooks' own switch is on here (the sandbox says so): off still wins.
    a_whole_session_leaves_no_trace(&[("CONTEXTSTREAM_RUN_PROGRESS", "off")]);
}

/// Every hook of a session that takes up a ticket, reports through the tool,
/// works, waits on a person, loses a subagent and ends: with `env`, none of
/// them may send or write anything.
fn a_whole_session_leaves_no_trace(env: &[(&'static str, &'static str)]) {
    let api = FakeApi::start();
    let sandbox = Sandbox::new(&api, env);
    let session = Session::new("host-session-a", SESSION_A);

    sandbox.init(&session);
    sandbox.set_ticket(&session, TICKET_A, "in_progress");
    sandbox.tool_result(
        &session,
        "progress",
        json!({"ticket_id": TICKET_A, "step": "Reading"}),
        json!({
            "run": {
                "run_id": "44444444-4444-4444-8444-444444444444",
                "workspace_id": WORKSPACE,
                "state": "working",
                "agent": session.agent(),
            },
            "session": SESSION_A,
            "run_key": session.key(TICKET_A),
        }),
    );
    sandbox.hook(
        &session,
        "pre-tool-use",
        json!({"hook_event_name": "PreToolUse", "tool_name": "Read", "tool_input": {"file_path": "/tmp/x"}}),
    );
    sandbox.hook(&session, "stop", json!({"hook_event_name": "Stop"}));
    sandbox.hook(
        &session,
        "user-prompt-submit",
        json!({"hook_event_name": "UserPromptSubmit", "prompt": "carry on"}),
    );
    sandbox.hook(
        &session,
        "subagent-stop",
        json!({"hook_event_name": "SubagentStop", "agent_id": "agent-1", "agent_type": "Explore"}),
    );
    sandbox.hook(
        &session,
        "session-end",
        json!({"hook_event_name": "SessionEnd", "reason": "other"}),
    );
    std::thread::sleep(Duration::from_secs(4));

    assert!(
        api.reports().is_empty(),
        "{} reports were sent",
        api.reports().len()
    );
    assert!(!sandbox.state_dir().exists(), "state was written");
}

#[test]
fn a_local_servers_words_are_enough_to_start_keep_alive_and_end_a_run() {
    let api = FakeApi::start();
    let sandbox = Sandbox::new(&api, &[]);
    let session = Session::new("host-session-a", SESSION_A);
    let key = session.key(TICKET_A);

    // A local server answers in text only: `init` names the session on a line.
    let identity = json!({"session": SESSION_A, "agent": session.agent()});
    sandbox.tool_words(
        &session,
        "init",
        json!({"folder_path": sandbox.repo}),
        &format!(
            "Session ready for workspace: Testing\nResolved workspace_id: {WORKSPACE}\nLive progress identity (read by ContextStream's hooks; nothing for you to do): {identity}\n\n[ACCOUNT_CONTEXT]"
        ),
    );
    sandbox.tool_words(
        &session,
        "entity",
        json!({"kind": "ticket", "action": "update", "id": TICKET_A, "workspace_id": WORKSPACE,
            "body": {"status": "in_progress"}}),
        &format!("Updated ticket {TICKET_A}."),
    );
    let started = api
        .wait_for(SOON, |body| {
            is_for(body, &key) && body["status"] == "working"
        })
        .expect("a ticket taken up through a local server starts a run");
    assert_eq!(started.body["agent"], session.agent());
    assert_eq!(started.body["ticket_id"], TICKET_A);

    // The model's own report, answered in words, is the same run.
    sandbox.tool_words(
        &session,
        "progress",
        json!({"ticket_id": TICKET_A, "step": "Reading the handler"}),
        "[PROGRESS] Recorded: step. state=working run_id=66666666-6666-4666-8666-666666666666",
    );
    let beat = api
        .wait_for(SOON, |body| {
            body["run_id"] == "66666666-6666-4666-8666-666666666666"
        })
        .expect("the run is signalled by the id the answer named");
    assert!(is_heartbeat(&beat.body));
    assert_eq!(beat.body["agent"], session.agent());

    sandbox.hook(
        &session,
        "session-end",
        json!({"hook_event_name": "SessionEnd", "reason": "other"}),
    );
    let ended = api
        .wait_for(Duration::from_secs(2), |body| body["status"] == "stopped")
        .expect("the session's end ends the run");
    assert_eq!(
        ended.body["summary"],
        "The session ended without a final report."
    );
    assert_eq!(
        api.reports()
            .iter()
            .filter(|report| report.body["status"] == "stopped")
            .count(),
        1
    );
}

/// Signs of life (`CONTEXTSTREAM_RUN_PROGRESS_SIGNS=on`, the automatic
/// reports off): a run the model reported is signalled after real tool calls
/// only, at most once an interval (two beats: 2 s here), by who and which run.
/// Nothing is started, set waiting, stepped or ended by the hooks.
#[test]
fn signs_of_life_follow_real_tool_calls_and_nothing_else() {
    let api = FakeApi::start();
    let sandbox = Sandbox::new(
        &api,
        &[
            ("CONTEXTSTREAM_RUN_PROGRESS_AUTO", UNSET),
            ("CONTEXTSTREAM_RUN_PROGRESS_SIGNS", "on"),
        ],
    );
    let session = Session::new("host-session-a", SESSION_A);
    sandbox.init(&session);
    // A ticket taken up starts nothing in this mode.
    sandbox.set_ticket(&session, TICKET_A, "in_progress");
    let run_id = "44444444-4444-4444-8444-444444444444";
    let agent = json!({"id": "floor-live-runs", "name": "floor-live-runs", "runtime": "mcp", "client": "claude-code"});
    let view = json!({
        "run_id": run_id,
        "workspace_id": WORKSPACE,
        "project_id": PROJECT,
        "state": "working",
        "liveness": "live",
        "agent": agent,
        "scope": {"workspace_id": WORKSPACE},
    });
    sandbox.tool_result(
        &session,
        "progress",
        json!({"run_key": "floor-live-runs", "step": "Building the API"}),
        json!({"run": view, "written": ["run.step"], "session": SESSION_A, "run_key": "floor-live-runs"}),
    );
    let pre = json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": {"command": "cargo build"}});
    let post = json!({
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": "cargo build"},
        "tool_response": {"stdout": "", "stderr": "", "interrupted": false},
    });
    // The model works on, a tool call a second, and reports nothing.
    for _ in 0..5 {
        sandbox.hook(&session, "pre-tool-use", pre.clone());
        std::thread::sleep(Duration::from_secs(1));
        sandbox.hook(&session, "post-tool-use", post.clone());
    }
    let sign = api
        .wait_for(SOON, |body| body["run_id"] == run_id)
        .expect("a run at work is signalled");
    // Who and which run, nothing else, under the agent the run was reported
    // as: a signal sets a run's agent, so this renames nothing.
    assert_eq!(
        sign.body,
        json!({"workspace_id": WORKSPACE, "run_id": run_id, "agent": agent})
    );
    // The turn ends: nothing says the run waits, and without a tool call
    // nothing more is sent. No timer.
    sandbox.hook(&session, "stop", json!({"hook_event_name": "Stop"}));
    std::thread::sleep(Duration::from_secs(3));
    let quiet_from = api.reports().len();
    std::thread::sleep(Duration::from_secs(6));
    assert_eq!(api.reports().len(), quiet_from, "silence is silence");
    // A real tool call after the quiet spell is a sign again.
    sandbox.hook(&session, "pre-tool-use", pre.clone());
    std::thread::sleep(Duration::from_secs(3));
    sandbox.hook(&session, "post-tool-use", post);
    let deadline = Instant::now() + SOON;
    while api.reports().len() == quiet_from && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
    }
    assert!(
        api.reports().len() > quiet_from,
        "a tool call is a sign of life"
    );
    // The session ends: no end is invented.
    sandbox.hook(
        &session,
        "session-end",
        json!({"hook_event_name": "SessionEnd", "reason": "other"}),
    );
    std::thread::sleep(Duration::from_secs(3));
    let reports = api.reports();
    // Every report was a bare sign for the run the model reported, at least
    // an interval apart: no run by key (the ticket), no status, step or wait.
    for report in &reports {
        assert_eq!(
            report.body,
            json!({"workspace_id": WORKSPACE, "run_id": run_id, "agent": agent})
        );
    }
    for pair in reports.windows(2) {
        assert!(
            pair[1].at.duration_since(pair[0].at) >= Duration::from_millis(1_900),
            "at most one sign an interval"
        );
    }
}

// ============================================================================
// Review of 89c632e: the reviewer's own break attempts (not part of the branch)
// ============================================================================

fn review_signs_sandbox(api: &FakeApi) -> Sandbox {
    Sandbox::new(
        api,
        &[
            ("CONTEXTSTREAM_RUN_PROGRESS_AUTO", UNSET),
            ("CONTEXTSTREAM_RUN_PROGRESS_SIGNS", "on"),
        ],
    )
}

fn review_view(run_id: &str) -> Value {
    json!({
        "run_id": run_id,
        "workspace_id": WORKSPACE,
        "project_id": PROJECT,
        "state": "working",
        "liveness": "live",
        "agent": {"id": "floor-live-runs", "name": "floor-live-runs", "runtime": "mcp", "client": "claude-code"},
        "scope": {"workspace_id": WORKSPACE},
    })
}

fn review_work(sandbox: &Sandbox, session: &Session, seconds: u64) {
    let pre = json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": {"command": "cargo build"}});
    let post = json!({
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": "cargo build"},
        "tool_response": {"stdout": "", "stderr": "", "interrupted": false},
    });
    for _ in 0..seconds {
        sandbox.hook(session, "pre-tool-use", pre.clone());
        std::thread::sleep(Duration::from_secs(1));
        sandbox.hook(session, "post-tool-use", post.clone());
    }
}

/// B. A subagent reported a run of its own and stopped without ending it.
/// Its run is no longer worked on; the session's later tool calls say nothing
/// about it.
#[test]
fn review_a_stopped_subagents_run_gets_no_sign_from_the_sessions_later_tool_calls() {
    let api = FakeApi::start();
    let sandbox = review_signs_sandbox(&api);
    let session = Session::new("host-session-review-sub", SESSION_A);
    sandbox.init(&session);
    let sub_run = "66666666-6666-4666-8666-666666666666";
    sandbox.hook(
        &session,
        "post-tool-use",
        json!({
            "hook_event_name": "PostToolUse",
            "agent_id": "agent-1",
            "agent_type": "Explore",
            "tool_name": "mcp__contextstream__progress",
            "tool_input": {"run_key": "explore-1", "step": "Reading the code"},
            "tool_response": {
                "isError": false,
                "content": [{"type": "text", "text": "ok"}],
                "structuredContent": {"run": review_view(sub_run), "written": ["run.step"], "session": SESSION_A, "run_key": "explore-1"},
            },
        }),
    );
    sandbox.hook(
        &session,
        "subagent-stop",
        json!({"hook_event_name": "SubagentStop", "agent_id": "agent-1", "agent_type": "Explore"}),
    );
    // The main agent works on.
    review_work(&sandbox, &session, 5);
    let sign = api.wait_for(Duration::from_secs(4), |body| body["run_id"] == sub_run);
    assert!(
        sign.is_none(),
        "the stopped subagent's run was signalled: {}",
        sign.map(|report| report.body.to_string())
            .unwrap_or_default()
    );
}

/// B. A session that dies right after a tool call: how many signs follow.
#[test]
fn review_signs_after_the_sessions_last_hook_event() {
    let api = FakeApi::start();
    let sandbox = review_signs_sandbox(&api);
    let session = Session::new("host-session-review-dead", SESSION_A);
    sandbox.init(&session);
    let run_id = "77777777-7777-4777-8777-777777777777";
    sandbox.tool_result(
        &session,
        "progress",
        json!({"run_key": "floor-live-runs", "step": "Building"}),
        json!({"run": review_view(run_id), "written": ["run.step"], "session": SESSION_A, "run_key": "floor-live-runs"}),
    );
    let reported = Instant::now();
    // One tool call starts 300 ms after the report, and then the session is gone.
    std::thread::sleep(Duration::from_millis(300));
    sandbox.hook(
        &session,
        "pre-tool-use",
        json!({"hook_event_name": "PreToolUse", "tool_name": "Bash", "tool_input": {"command": "sleep 600"}}),
    );
    let last_event = Instant::now();
    std::thread::sleep(Duration::from_secs(12));
    let signs = api.reports();
    println!(
        "REVIEW signs after the last hook event: {} at {:?} after the report (last hook event {:?} after it)",
        signs.len(),
        signs
            .iter()
            .map(|report| report.at.duration_since(reported))
            .collect::<Vec<_>>(),
        last_event.duration_since(reported),
    );
    assert!(
        signs.len() <= 1,
        "more than one sign with no hook event since"
    );
    for report in &signs {
        assert_eq!(report.body["run_id"], run_id);
        assert!(report.body.get("status").is_none() && report.body.get("step").is_none());
    }
}

/// A run reported under the session's default key belongs to that session
/// part: once the client reconnects (the server names a new part), the old
/// part's run gets no further sign; the new part's run does.
#[test]
fn a_default_run_of_a_session_part_that_ended_gets_no_further_sign() {
    let api = FakeApi::start();
    let sandbox = review_signs_sandbox(&api);
    let session = Session::new("host-session-part", SESSION_A);
    sandbox.init(&session);
    let old_run = "88888888-8888-4888-8888-888888888888";
    let old_key = format!("{SESSION_A}:session");
    sandbox.tool_result(
        &session,
        "progress",
        json!({"step": "Building"}),
        json!({"run": review_view(old_run), "written": ["run.step"], "session": SESSION_A, "run_key": old_key}),
    );
    // The client reconnects: the server names a new session part, and the
    // model's next report lands on a new run.
    let new_run = "99999999-9999-4999-8999-999999999999";
    let new_key = format!("{SESSION_B}:session");
    sandbox.tool_result(
        &session,
        "progress",
        json!({"step": "Building again"}),
        json!({"run": review_view(new_run), "written": ["run.step"], "session": SESSION_B, "run_key": new_key}),
    );
    review_work(&sandbox, &session, 5);
    assert!(
        api.wait_for(SOON, |body| body["run_id"] == new_run)
            .is_some(),
        "the new part's run is signalled"
    );
    let old = api
        .reports()
        .into_iter()
        .find(|report| report.body["run_id"] == old_run);
    assert!(
        old.is_none(),
        "the ended part's run was signalled: {}",
        old.map(|report| report.body.to_string())
            .unwrap_or_default()
    );
}

// ============================================================================
// The second reading of 60ecd07, MUST 2: signs of life know who the session is
// ============================================================================

/// The client reconnects and the server says so in `init`'s answer (a new
/// session part), and the model then works on without reporting: the old
/// part's default run is no longer this session's, and gets no further sign.
#[test]
fn a_reconnect_init_announces_ends_the_signs_for_the_old_parts_default_run() {
    let api = FakeApi::start();
    let sandbox = review_signs_sandbox(&api);
    let mut session = Session::new("host-session-reconnect", SESSION_A);
    sandbox.init(&session);
    let old_run = "88888888-8888-4888-8888-888888888888";
    let old_key = format!("{SESSION_A}:session");
    sandbox.tool_result(
        &session,
        "progress",
        json!({"step": "Building"}),
        json!({"run": review_view(old_run), "written": ["run.step"], "session": SESSION_A, "run_key": old_key}),
    );
    // The same editor session, a new server session: `init` names it.
    session.server_session = SESSION_B;
    sandbox.init(&session);
    // The model works on and reports nothing.
    review_work(&sandbox, &session, 5);
    std::thread::sleep(Duration::from_secs(3));
    let signs: Vec<String> = api
        .reports()
        .into_iter()
        .map(|report| report.body.to_string())
        .collect();
    assert!(
        signs.is_empty(),
        "the ended part's default run was signalled: {signs:?}"
    );
}

/// A local server answers in words only. Signs of life learn who the session
/// is from `init`'s line and the run from `progress`'s, and signal that run
/// under that agent.
#[test]
fn a_local_servers_words_are_enough_for_signs_of_life() {
    let api = FakeApi::start();
    let sandbox = review_signs_sandbox(&api);
    let session = Session::new("host-session-words", SESSION_A);
    let identity = json!({"session": SESSION_A, "agent": session.agent()});
    sandbox.tool_words(
        &session,
        "init",
        json!({"folder_path": sandbox.repo}),
        &format!(
            "Session ready for workspace: Testing\nResolved workspace_id: {WORKSPACE}\nLive progress identity (read by ContextStream's hooks; nothing for you to do): {identity}\n\n[ACCOUNT_CONTEXT]"
        ),
    );
    let run_id = "66666666-6666-4666-8666-666666666666";
    sandbox.tool_words(
        &session,
        "progress",
        json!({"workspace_id": WORKSPACE, "step": "Reading the handler"}),
        &format!("[PROGRESS] Recorded: step. state=working run_id={run_id}"),
    );
    review_work(&sandbox, &session, 5);
    let sign = api
        .wait_for(SOON, |body| body["run_id"] == run_id)
        .expect("a run a local server reported is signalled");
    // Who and which run, under the agent `init` named, and nothing else.
    assert_eq!(
        sign.body,
        json!({"workspace_id": WORKSPACE, "run_id": run_id, "agent": session.agent()})
    );
    // Nothing but signs for that run was sent: the hooks start, step and end
    // nothing in this mode.
    for report in api.reports() {
        assert_eq!(
            report.body,
            json!({"workspace_id": WORKSPACE, "run_id": run_id, "agent": session.agent()})
        );
    }
}

/// How long a session works with no run before the hooks open its own; the
/// tests that watch it start set none.
const SESSION_AFTER: &str = "CONTEXTSTREAM_RUN_PROGRESS_SESSION_AFTER_SECS";

fn session_key(session: &Session) -> String {
    format!("{}:session", session.server_session)
}

#[test]
fn a_session_that_never_reports_is_shown_with_its_agent_and_project() {
    let api = FakeApi::start();
    let sandbox = Sandbox::new(&api, &[(SESSION_AFTER, "0")]);
    let session = Session::new("host-session-a", SESSION_A);
    let key = session_key(&session);

    sandbox.init_in(&session, "platform-api");
    // ContextStream's own calls are bookkeeping, not the work.
    sandbox.works(&session, "mcp__contextstream__context");
    std::thread::sleep(Duration::from_secs(2));
    assert!(
        api.reports().is_empty(),
        "knowing who the session is sends nothing"
    );

    // The session works, and its model says nothing: the session's own run
    // starts, under the key and the agent the `progress` tool would use.
    sandbox.works(&session, "Read");
    let started = api
        .wait_for(SOON, |body| {
            is_for(body, &key) && body["status"] == "working"
        })
        .expect("a session at work with no run gets its own");
    assert_eq!(started.body["agent"], session.agent());
    assert_eq!(started.body["group_id"], session.agent()["id"]);
    assert_eq!(started.body["workspace_id"], WORKSPACE);
    assert_eq!(started.body["project_id"], PROJECT);
    assert_eq!(started.body["title"], "Working in platform-api");
    assert!(started.body.get("ticket_id").is_none());
    assert!(started.body.get("step").is_none());

    // The kind of work, in fixed words, from the first tool call on.
    let step = api
        .wait_for(SOON, |body| {
            is_for(body, &key) && body.get("step").is_some()
        })
        .expect("the kind of work is said");
    assert_eq!(
        step.body["step"],
        json!({"label": "Reading files", "kind": "read", "status": "started"})
    );

    // It works on without a word (one long tool call): the run is kept alive.
    std::thread::sleep(Duration::from_secs(6));
    let beats = api
        .reports()
        .into_iter()
        .filter(|report| is_for(&report.body, &key) && is_heartbeat(&report.body))
        .count();
    assert!(beats >= 2, "{beats} heartbeats in six seconds");
    assert_eq!(
        api.reports()
            .iter()
            .filter(|report| report.body["status"] == "working")
            .count(),
        1,
        "the run starts once"
    );

    // The turn ends: the run waits on a person. The session ends: so does it.
    sandbox.hook(&session, "stop", json!({"hook_event_name": "Stop"}));
    api.wait_for(SOON, |body| {
        is_for(body, &key) && body["status"] == "waiting_input"
    })
    .expect("the session's run waits when the turn ends");
    sandbox.hook(
        &session,
        "session-end",
        json!({"hook_event_name": "SessionEnd", "reason": "other"}),
    );
    let ended = api
        .wait_for(SOON, |body| {
            is_for(body, &key) && body["status"] == "stopped"
        })
        .expect("the session's run ends with the session");
    assert_eq!(
        ended.body["summary"],
        "The session ended without a final report."
    );
}

#[test]
fn a_dead_session_that_never_reported_does_not_linger() {
    let api = FakeApi::start();
    let sandbox = Sandbox::new(&api, &[(SESSION_AFTER, "0")]);
    let mut session = Session::new("host-session-a", SESSION_A);
    let key = session_key(&session);

    sandbox.init_in(&session, "platform-api");
    sandbox.works(&session, "Bash");
    api.wait_for(SOON, |body| {
        is_for(body, &key) && body["status"] == "working"
    })
    .expect("the session's own run starts");
    let keep_alive = sandbox
        .keep_alive_pid(&session)
        .expect("the session has a keep-alive");

    // The process dies without a word: the run ends by a fixed sentence.
    let died = session.die();
    let ended = api
        .wait_for(LEAVES_WITHIN, |body| {
            is_for(body, &key) && body["status"] == "stopped"
        })
        .expect("a dead session's run ends");
    assert_eq!(
        ended.body["summary"],
        "The session's process ended without a final report."
    );
    assert!(ended.at.duration_since(died) < LEAVES_WITHIN);
    assert!(wait_until_gone(keep_alive, LEAVES_WITHIN));
}

#[test]
fn a_session_at_work_for_a_moment_is_left_to_report_itself() {
    // As installed: two minutes of work before the hooks speak for a session.
    let api = FakeApi::start();
    let sandbox = Sandbox::new(&api, &[]);
    let session = Session::new("host-session-a", SESSION_A);

    sandbox.init_in(&session, "platform-api");
    sandbox.works(&session, "Read");
    sandbox.works(&session, "Bash");
    std::thread::sleep(Duration::from_secs(4));
    assert!(
        api.reports().is_empty(),
        "{} reports were sent",
        api.reports().len()
    );
}

#[test]
fn a_session_whose_model_reports_keeps_its_own_run_and_title() {
    let api = FakeApi::start();
    let sandbox = Sandbox::new(&api, &[(SESSION_AFTER, "0")]);
    let session = Session::new("host-session-a", SESSION_A);
    let key = session_key(&session);
    let run_id = "44444444-4444-4444-8444-444444444444";

    sandbox.init_in(&session, "platform-api");
    // The model reports first, as the rules ask: a title and a step of its own.
    sandbox.tool_result(
        &session,
        "progress",
        json!({"title": "Fix the flaky login test", "step": "Reading the test"}),
        json!({
            "run": {
                "run_id": run_id,
                "workspace_id": WORKSPACE,
                "project_id": PROJECT,
                "state": "working",
                "agent": session.agent(),
            },
            "session": SESSION_A,
            "run_key": key,
        }),
    );
    sandbox.works(&session, "Read");
    sandbox.works(&session, "Bash");
    std::thread::sleep(Duration::from_secs(5));

    // The hooks keep that run alive and say nothing of their own about it.
    let reports = api.reports();
    assert!(!reports.is_empty(), "the model's run is kept alive");
    for report in &reports {
        assert_eq!(report.body["run_id"], run_id);
        assert!(is_heartbeat(&report.body), "{}", report.body);
        assert!(report.body.get("title").is_none());
    }
}

#[test]
fn no_session_run_as_shipped_or_with_signs_of_life_alone() {
    for env in [
        vec![
            ("CONTEXTSTREAM_RUN_PROGRESS_AUTO", UNSET),
            (SESSION_AFTER, "0"),
        ],
        vec![
            ("CONTEXTSTREAM_RUN_PROGRESS_AUTO", UNSET),
            ("CONTEXTSTREAM_RUN_PROGRESS_SIGNS", "on"),
            (SESSION_AFTER, "0"),
        ],
        vec![("CONTEXTSTREAM_RUN_PROGRESS", "off"), (SESSION_AFTER, "0")],
    ] {
        let api = FakeApi::start();
        let sandbox = Sandbox::new(&api, &env);
        let session = Session::new("host-session-a", SESSION_A);

        sandbox.init_in(&session, "platform-api");
        sandbox.works(&session, "Read");
        sandbox.works(&session, "Bash");
        sandbox.hook(&session, "stop", json!({"hook_event_name": "Stop"}));
        std::thread::sleep(Duration::from_secs(3));
        assert!(
            api.reports().is_empty(),
            "{env:?}: {} reports were sent",
            api.reports().len()
        );
    }
}

#[test]
fn a_run_reported_without_a_project_is_shown_in_the_sessions_project() {
    let api = FakeApi::start();
    let sandbox = Sandbox::new(&api, &[]);
    let session = Session::new("host-session-a", SESSION_A);
    let run_id = "44444444-4444-4444-8444-444444444444";

    sandbox.init_in(&session, "platform-api");
    // The server had no project for the call (a stateless report, or a
    // session it resolved none for): the run the model reported has none.
    sandbox.tool_result(
        &session,
        "progress",
        json!({"title": "Fix the flaky login test", "step": "Reading the test"}),
        json!({
            "run": {
                "run_id": run_id,
                "workspace_id": WORKSPACE,
                "project_id": null,
                "state": "working",
                "agent": session.agent(),
            },
            "session": SESSION_A,
            "run_key": session_key(&session),
        }),
    );

    // The run's first signal says where the session works, as `init`
    // resolved it: the API gives the run its project from it.
    let signal = api
        .wait_for(SOON, |body| body["run_id"] == run_id)
        .expect("the model's run is kept alive");
    assert_eq!(signal.body["workspace_id"], WORKSPACE);
    assert_eq!(signal.body["project_id"], PROJECT);
    assert!(is_heartbeat(&signal.body), "{}", signal.body);
    assert!(signal.body.get("title").is_none());
}

#[test]
fn a_project_the_key_may_not_write_is_offered_once_and_the_run_stays_alive() {
    let api = FakeApi::start();
    let sandbox = Sandbox::new(&api, &[]);
    let session = Session::new("host-session-a", SESSION_A);
    let run_id = "44444444-4444-4444-8444-444444444444";

    // The session works in a project its key may read and not write, and
    // the run its model reported has no project: the API took that report.
    sandbox.init_in_project(&session, READ_ONLY_PROJECT, "read-only");
    sandbox.tool_result(
        &session,
        "progress",
        json!({"title": "Fix the flaky login test", "step": "Reading the test"}),
        json!({
            "run": {
                "run_id": run_id,
                "workspace_id": WORKSPACE,
                "project_id": null,
                "state": "working",
                "agent": session.agent(),
            },
            "session": SESSION_A,
            "run_key": session_key(&session),
        }),
    );

    // The hooks offer the run the session's project, and the API refuses it.
    let offered = api
        .wait_for(SOON, |body| {
            body["run_id"] == run_id && body["project_id"] == READ_ONLY_PROJECT
        })
        .expect("the session's project is offered");

    // The refusal is about the project, not about the run: it is offered
    // once, and the run is kept alive as the model reported it.
    std::thread::sleep(Duration::from_secs(6));
    let reports = api.reports();
    assert_eq!(
        reports
            .iter()
            .filter(|report| report.body["project_id"] == READ_ONLY_PROJECT)
            .count(),
        1,
        "the project is offered once"
    );
    let after: Vec<&Report> = reports
        .iter()
        .filter(|report| report.at > offered.at && report.body["run_id"] == run_id)
        .collect();
    assert!(
        after.len() >= 2,
        "{} signals after the refusal",
        after.len()
    );
    for report in after {
        assert!(report.body.get("project_id").is_none(), "{}", report.body);
        assert!(is_heartbeat(&report.body), "{}", report.body);
    }
}

// ============================================================================
// Codex: the hooks setup installs for it are enough for live progress
// ============================================================================

/// The hook setup installs for Codex's PostToolUse and Stop events.
const CODEX_EVENT_HOOK: &str = "run-progress";
/// The hook Codex already runs before every tool call.
const CODEX_NOTICE_HOOK: &str = "mid-run-notices";

/// A hook input as Codex 0.169 sends it: Claude Code's fields, and the pair
/// of `turn_id` and `model` that tells Codex from Claude Code. The shapes in
/// this section are the ones a real `codex exec` session handed these hooks
/// (2026-11-18, Codex 0.169.0 against the hosted server): an MCP tool as
/// `mcp__contextstream__<tool>` with `_meta`, `content`, `isError` and
/// `structuredContent` in its result, a shell command as `Bash` with its
/// output as plain text, and `Stop` with `stop_hook_active` and
/// `last_assistant_message`.
fn codex(event: &str, mut input: Value) -> Value {
    input["hook_event_name"] = event.into();
    input["turn_id"] = "019a8f3e-turn-1".into();
    input["model"] = "gpt-5.4".into();
    input["permission_mode"] = "bypassPermissions".into();
    input["transcript_path"] = Value::Null;
    input
}

/// The agent the hosted server reports a Codex session as ("Codex 3fa9").
fn codex_agent(session: &Session) -> Value {
    json!({
        "id": format!("codex-mcp-client:{}", &session.server_session[..8]),
        "name": format!("Codex {}", &session.server_session[..4]),
        "runtime": "mcp",
        "client": "codex-mcp-client",
    })
}

impl Sandbox {
    /// Codex's PostToolUse for one of ContextStream's tools: the tool's
    /// arguments, and the MCP result as Codex hands it on. Whatever the hook
    /// prints, Codex must accept: it prints nothing.
    fn codex_result(&self, session: &Session, tool: &str, tool_input: Value, structured: Value) {
        let printed = self.hook_stdout(
            session,
            CODEX_EVENT_HOOK,
            codex(
                "PostToolUse",
                json!({
                    "tool_name": format!("mcp__contextstream__{tool}"),
                    "tool_use_id": "call_1",
                    "tool_input": tool_input,
                    "tool_response": {
                        "_meta": {},
                        "content": [{"type": "text", "text": "ok"}],
                        "isError": false,
                        "structuredContent": structured,
                    },
                }),
            ),
        );
        assert_eq!(printed.trim(), "", "Codex PostToolUse output");
    }

    /// Codex's PostToolUse for a shell command: its output, as plain text.
    fn codex_ran(&self, session: &Session, command: &str, output: &str) {
        let printed = self.hook_stdout(
            session,
            CODEX_EVENT_HOOK,
            codex(
                "PostToolUse",
                json!({
                    "tool_name": "Bash",
                    "tool_use_id": "call_2",
                    "tool_input": {"command": command},
                    "tool_response": output,
                }),
            ),
        );
        assert_eq!(printed.trim(), "", "Codex PostToolUse output");
    }

    fn codex_init(&self, session: &Session) {
        self.codex_result(
            session,
            "init",
            json!({"folder_path": self.repo}),
            json!({
                "session_id": "99999999-9999-4999-8999-999999999999",
                "workspace_id": WORKSPACE,
                "project_id": PROJECT,
                "project": {"id": PROJECT, "name": "platform-api"},
                "run_progress": {"session": session.server_session, "agent": codex_agent(session)},
            }),
        );
    }

    /// Codex's PreToolUse for a shell command, through the hook it already
    /// runs before every tool call.
    fn codex_runs(&self, session: &Session, command: &str) {
        self.hook(
            session,
            CODEX_NOTICE_HOOK,
            codex(
                "PreToolUse",
                json!({"tool_name": "Bash", "tool_use_id": "call_2", "tool_input": {"command": command}}),
            ),
        );
    }
}

#[test]
fn a_codex_session_stays_live_through_local_work_and_ends_with_its_process() {
    let api = FakeApi::start();
    let sandbox = Sandbox::new(&api, &[]);
    let mut session = Session::new("019a8f3e-codex-thread-a", SESSION_A);
    let run_id = "44444444-4444-4444-8444-444444444444";

    // The lane starts as its rules say: `init`, then one report of its own.
    sandbox.codex_init(&session);
    assert!(
        api.reports().is_empty(),
        "knowing who the session is sends nothing"
    );
    sandbox.codex_result(
        &session,
        "progress",
        json!({"title": "Round 7 delivery pins", "step": "Verify the checkout"}),
        json!({
            "run": {
                "run_id": run_id,
                "workspace_id": WORKSPACE,
                "project_id": PROJECT,
                "state": "working",
                "agent": codex_agent(&session),
            },
            "session": SESSION_A,
            "run_key": session_key(&session),
        }),
    );

    // It then builds and tests on its own machine: one long command, and no
    // call to ContextStream for the whole of it. The run is kept alive.
    sandbox.codex_runs(&session, "cargo test --workspace");
    std::thread::sleep(Duration::from_secs(7));
    let signals: Vec<Report> = api
        .reports()
        .into_iter()
        .filter(|report| report.body["run_id"] == run_id)
        .collect();
    assert!(
        signals.len() >= 3,
        "{} signals in seven seconds of local work",
        signals.len()
    );
    for signal in &signals {
        assert!(is_heartbeat(&signal.body), "{}", signal.body);
        assert_eq!(signal.body["agent"], codex_agent(&session));
        assert_eq!(signal.body["project_id"], PROJECT);
        assert!(signal.body.get("title").is_none());
    }
    // No two of them are further apart than the API allows before it calls
    // the run stalled.
    for pair in signals.windows(2) {
        assert!(pair[1].at.duration_since(pair[0].at) < stalls_after(&pair[0].body));
    }
    // The command comes back. Neither the command nor a word of its output
    // is in anything the hooks sent.
    sandbox.codex_ran(
        &session,
        "cargo test --workspace",
        "test result: ok. 12 passed\n",
    );
    for report in api.reports() {
        let sent = report.body.to_string();
        assert!(!sent.contains("cargo test"), "{sent}");
        assert!(!sent.contains("12 passed"), "{sent}");
    }

    // The turn ends (an interactive session): the run waits on a person.
    let printed = sandbox.hook_stdout(
        &session,
        CODEX_EVENT_HOOK,
        codex(
            "Stop",
            json!({"stop_hook_active": false, "last_assistant_message": "Done."}),
        ),
    );
    assert_eq!(printed.trim(), "", "Codex Stop output");
    api.wait_for(SOON, |body| {
        body["run_id"] == run_id && body["status"] == "waiting_input"
    })
    .expect("the run waits when the turn ends");

    // The lane's process goes (it finished, or it was killed): the run ends
    // by a fixed sentence within seconds, and is never left to stall.
    let keep_alive = sandbox
        .keep_alive_pid(&session)
        .expect("the session has a keep-alive");
    let died = session.die();
    let ended = api
        .wait_for(LEAVES_WITHIN, |body| {
            body["run_id"] == run_id && body["status"] == "stopped"
        })
        .expect("a Codex session that is gone ends its run");
    assert_eq!(
        ended.body["summary"],
        "The session's process ended without a final report."
    );
    assert!(ended.at.duration_since(died) < LEAVES_WITHIN);
    assert!(wait_until_gone(keep_alive, LEAVES_WITHIN));
}

#[test]
fn a_codex_session_that_never_reports_is_shown_with_its_agent_and_project() {
    let api = FakeApi::start();
    let sandbox = Sandbox::new(&api, &[(SESSION_AFTER, "0")]);
    let session = Session::new("019a8f3e-codex-thread-a", SESSION_A);
    let key = session_key(&session);

    sandbox.codex_init(&session);
    sandbox.codex_runs(&session, "cargo build");
    let started = api
        .wait_for(SOON, |body| {
            is_for(body, &key) && body["status"] == "working"
        })
        .expect("a Codex session at work with no run gets its own");
    assert_eq!(started.body["agent"], codex_agent(&session));
    assert_eq!(started.body["workspace_id"], WORKSPACE);
    assert_eq!(started.body["project_id"], PROJECT);
    assert_eq!(started.body["title"], "Working in platform-api");
    let step = api
        .wait_for(SOON, |body| {
            is_for(body, &key) && body.get("step").is_some()
        })
        .expect("the kind of work is said");
    assert_eq!(step.body["step"]["label"], "Running a command");
}

#[test]
fn the_codex_hooks_send_and_write_nothing_as_shipped() {
    for env in [
        vec![
            ("CONTEXTSTREAM_RUN_PROGRESS_AUTO", UNSET),
            (SESSION_AFTER, "0"),
        ],
        vec![("CONTEXTSTREAM_RUN_PROGRESS", "off"), (SESSION_AFTER, "0")],
    ] {
        let api = FakeApi::start();
        let sandbox = Sandbox::new(&api, &env);
        let session = Session::new("019a8f3e-codex-thread-a", SESSION_A);

        sandbox.codex_init(&session);
        sandbox.codex_result(
            &session,
            "progress",
            json!({"title": "Round 7 delivery pins", "step": "Verify the checkout"}),
            json!({
                "run": {
                    "run_id": "44444444-4444-4444-8444-444444444444",
                    "workspace_id": WORKSPACE,
                    "state": "working",
                    "agent": codex_agent(&session),
                },
                "session": SESSION_A,
                "run_key": session_key(&session),
            }),
        );
        sandbox.codex_runs(&session, "cargo test");
        sandbox.hook(
            &session,
            CODEX_EVENT_HOOK,
            codex("Stop", json!({"stop_hook_active": false})),
        );
        std::thread::sleep(Duration::from_secs(3));
        assert!(
            api.reports().is_empty(),
            "{env:?}: {} reports were sent",
            api.reports().len()
        );
        assert!(!sandbox.state_dir().exists(), "{env:?}: state was written");
    }
}
