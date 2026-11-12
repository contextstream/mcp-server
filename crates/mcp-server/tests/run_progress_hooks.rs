//! Drive the real hook binary against a fake API: what an ordinary session's
//! hooks send to live run progress, and when they stop.
//!
//! - A session that takes up a ticket and then says nothing for a while (one
//!   long tool call) stays live: its detached keep-alive signals the run.
//! - A session whose process dies ends its run within seconds, by a fixed
//!   sentence, and its keep-alive process leaves.
//! - Two sessions on one machine have a keep-alive each and never report for
//!   one another.
//! - With `CONTEXTSTREAM_RUN_PROGRESS=off` nothing is sent and nothing is
//!   written.
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
const TICKET_B: &str = "55555555-5555-4555-8555-555555555555";
const TERMINAL: &[&str] = &["completed", "partial", "stopped", "failed", "cancelled"];

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
    let answer = if first_line.contains("/api/v1/run-progress/report") {
        let body: Value = serde_json::from_slice(&request[head_end..]).unwrap_or(Value::Null);
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
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
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
    /// Extra environment for every hook of this sandbox.
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

    fn hook(&self, session: &Session, hook: &str, mut input: Value) {
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
            // Said here, so these tests hold whichever way the default for
            // runs that start by themselves is set.
            .env("CONTEXTSTREAM_RUN_PROGRESS_AUTO", "on")
            // One heartbeat a second, so a few seconds show the pattern.
            .env("CONTEXTSTREAM_RUN_PROGRESS_BEAT_SECS", "1")
            .env(
                "CONTEXTSTREAM_RUN_PROGRESS_HOST_PID",
                session.process.id().to_string(),
            )
            .envs(self.env.iter().copied())
            .current_dir(&self.repo)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
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
fn a_run_the_model_reported_is_kept_alive_and_none_starts_by_itself_when_that_is_off() {
    let api = FakeApi::start();
    let sandbox = Sandbox::new(&api, &[("CONTEXTSTREAM_RUN_PROGRESS_AUTO", "off")]);
    let session = Session::new("host-session-a", SESSION_A);

    // Taking up a ticket starts nothing here.
    sandbox.init(&session);
    sandbox.set_ticket(&session, TICKET_A, "in_progress");
    std::thread::sleep(Duration::from_secs(3));
    assert!(api.reports().is_empty(), "{:?}", api.reports().len());

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
fn nothing_is_sent_or_written_when_live_progress_is_off() {
    let api = FakeApi::start();
    let sandbox = Sandbox::new(&api, &[("CONTEXTSTREAM_RUN_PROGRESS", "off")]);
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
