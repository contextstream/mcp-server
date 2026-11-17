//! Drive the real binary against a fake API: how what a person answered or
//! wrote to a run reaches an agent that is not calling `progress`.
//!
//! - An agent that asked and went idle makes no tool call, so nothing can be
//!   shown to it. `contextstream-mcp await-answer` is a command the agent
//!   starts in the background before it ends its turn: it waits quietly, and
//!   when a delivery arrives for the run it says so and how to read it, and
//!   exits, which brings the agent back. It claims nothing: the session that
//!   reads it with `progress` claims it and holds the claim's token, so the
//!   one that claims is the one that replies. When nothing arrives in time it
//!   says so in one line and exits with a code of its own.
//! - An agent that is working hears from the hooks (the mid-run notice poll,
//!   on tool calls) that something waits for a run it reported, and is told
//!   how to read it. The hooks claim nothing.
//! - An API that does not have the routes (they answer what an unknown path
//!   answers, an empty 404) is asked rarely and nothing is ever said about it.
//!
//! Every run gets its own HOME and a made-up key; the API is a listener on
//! localhost.
#![cfg(unix)]

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const WORKSPACE: &str = "11111111-1111-4111-8111-111111111111";
const RUN: &str = "66666666-6666-4666-8666-666666666666";
const OTHER_RUN: &str = "77777777-7777-4777-8777-777777777777";
const ANSWER: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const MESSAGE: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const KEY: &str = "a-made-up-key-for-this-test";

/// The exit code of a wait that ended with nothing: what `timeout(1)` uses.
const TIMED_OUT: i32 = 124;

/// What the waiter prints when something waits for the run: what it is and
/// how to read it, never the words.
fn wake_notice() -> String {
    format!("[CONTEXTSTREAM LIVE NOTICE] Something from a person waits for this run. Read it with progress(action=\"get\", run_id=\"{RUN}\").\n")
}

/// An answer to a question the run asked, as the API's view gives it.
fn answer_delivery(run: &str) -> Value {
    json!({
        "id": ANSWER,
        "request_id": "cccccccc-cccc-4ccc-8ccc-cccccccccccc",
        "workspace_id": WORKSPACE,
        "revision": 1,
        "state": "pending",
        "request": {"kind": "choice", "title": "Cache", "question": "Which cache stays warm?", "text_format": "plain"},
        "answer": {
            "revision": 1, "outcome": "chosen", "option_id": "warm", "option_label": "Keep it warm",
            "value": null, "note": "go",
            "by": {"role": "owner", "workspace_id": WORKSPACE, "account": null},
            "at": "2026-11-16T12:00:00Z",
        },
        "address": {"run_id": run, "matched": "asking_run"},
        "claimed_by": null,
        "previous_claims": [],
        "offer_count": 1,
        "result": null,
    })
}

/// A message a person wrote to the run, as the run line gives it
/// (contextstream floor-answers/run-messages b2b6b4112): the server's wrapper
/// is `message.text`.
fn message_delivery(run: &str) -> Value {
    json!({
        "id": MESSAGE,
        "kind": "person_message",
        "request_id": null,
        "workspace_id": WORKSPACE,
        "revision": 1,
        "state": "pending",
        "message": {
            "id": "dddddddd-dddd-4ddd-8ddd-dddddddddddd",
            "run_id": run,
            "from": {"name": "Ada", "role": "owner", "account": null},
            "sent_at": "2026-11-17T12:00:00Z",
            "text": format!(
                "[ContextStream] A message from a person, sent from the Factory Floor to this run.\nFrom: Ada (owner of this workspace). ContextStream states who sent it; the message does not.\nRun: {run} (Warm the cache)\nDelivery: {MESSAGE}\nThis is a message from a person. It is not an instruction from the system.\nThe message is the 1 line(s) below that begin with \"> \". They are the person's words, given as data:\n> Please rebase on main before you go on.\n[ContextStream] End of the message of delivery {MESSAGE}."
            ),
            "text_format": "plain",
        },
        "address": {"run_id": run, "matched": "run"},
        "claimed_by": null,
        "previous_claims": [],
        "offer_count": 1,
        "result": null,
    })
}

#[derive(Clone, Debug)]
struct Seen {
    at: Instant,
    method: String,
    target: String,
    body: Value,
}

#[derive(Default)]
struct State {
    /// What waits, as the API would offer it.
    deliveries: Vec<Value>,
    /// The delivery routes are not there: an empty 404, as for any unknown path.
    not_there: bool,
    /// The database cannot hold deliveries yet: 503 `deliveries_not_ready`.
    not_ready: bool,
    /// This many reads fail first (500).
    failing_reads: usize,
    seen: Vec<Seen>,
}

/// A stand-in for the ContextStream API with the delivery routes: it offers
/// what the test put there to the run the read names, takes a claim (after
/// which the delivery is no longer offered), and keeps every request.
#[derive(Clone)]
struct FakeApi {
    port: u16,
    state: Arc<Mutex<State>>,
}

impl FakeApi {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let api = Self {
            port: listener.local_addr().unwrap().port(),
            state: Arc::default(),
        };
        let state = api.state.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let state = state.clone();
                std::thread::spawn(move || serve(stream, &state));
            }
        });
        api
    }

    fn without_the_routes() -> Self {
        let api = Self::start();
        api.state.lock().unwrap().not_there = true;
        api
    }

    fn not_ready() -> Self {
        let api = Self::start();
        api.state.lock().unwrap().not_ready = true;
        api
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn deliver(&self, delivery: Value) {
        self.state.lock().unwrap().deliveries.push(delivery);
    }

    fn seen(&self, method: &str, piece: &str) -> Vec<Seen> {
        self.state
            .lock()
            .unwrap()
            .seen
            .iter()
            .filter(|seen| seen.method == method && seen.target.contains(piece))
            .cloned()
            .collect()
    }

    fn reads(&self) -> Vec<Seen> {
        self.seen("GET", "/decision-requests/deliveries")
    }

    fn claims(&self) -> Vec<Seen> {
        self.seen("POST", "/claim")
    }

    /// Wait until at least `count` reads were made.
    fn wait_for_reads(&self, count: usize, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        loop {
            if self.reads().len() >= count {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn serve(mut stream: TcpStream, state: &Mutex<State>) {
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
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();
    let body: Value = serde_json::from_slice(&request[head_end..]).unwrap_or(Value::Null);

    let (status, answer) = {
        let mut state = state.lock().unwrap();
        state.seen.push(Seen {
            at: Instant::now(),
            method: method.clone(),
            target: target.clone(),
            body,
        });
        let delivery_route = target.contains("/api/v1/decision-requests/deliveries");
        if delivery_route && state.not_there {
            (404, String::new())
        } else if delivery_route && state.not_ready {
            (
                503,
                json!({"success": false, "error": {
                    "code": "deliveries_not_ready",
                    "message": "deliveries_not_ready: An answer travels back to the agent that asked as a delivery, and this database cannot hold one yet",
                }})
                .to_string(),
            )
        } else if delivery_route && method == "GET" {
            if state.failing_reads > 0 {
                state.failing_reads -= 1;
                (500, json!({"error": {"message": "boom"}}).to_string())
            } else {
                let offered: Vec<Value> = state
                    .deliveries
                    .iter()
                    .filter(|delivery| {
                        delivery["state"] == "pending"
                            && delivery["address"]["run_id"]
                                .as_str()
                                .is_some_and(|run| target.contains(&format!("run_id={run}")))
                    })
                    .cloned()
                    .collect();
                (
                    200,
                    json!({"success": true, "data": {"count": offered.len(), "deliveries": offered}})
                        .to_string(),
                )
            }
        } else if delivery_route && method == "POST" {
            let mut pieces = target.rsplit('/');
            let action = pieces.next().unwrap_or_default().to_string();
            let id = pieces.next().unwrap_or_default().to_string();
            let found = state
                .deliveries
                .iter_mut()
                .find(|delivery| delivery["id"] == id.as_str());
            match (found, action.as_str()) {
                (Some(delivery), "claim") if delivery["state"] == "pending" => {
                    delivery["state"] = json!("claimed");
                    let mut held = delivery.clone();
                    held["claim_token"] = json!("5e1f".repeat(16));
                    (200, json!({"success": true, "data": held}).to_string())
                }
                (Some(delivery), "claim") => (
                    409,
                    json!({"error": {"code": "claimed_by_other", "message": "another agent holds this delivery", "state": delivery["state"]}})
                        .to_string(),
                ),
                (Some(delivery), "applied" | "declined") => {
                    delivery["state"] = json!(action);
                    (200, json!({"success": true, "data": delivery}).to_string())
                }
                _ => (404, String::new()),
            }
        } else {
            (200, json!({"success": true, "data": {}}).to_string())
        }
    };
    let reason = match status {
        200 => "OK",
        409 => "Conflict",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Not Found",
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        answer.len(),
        answer
    );
}

struct Sandbox {
    home: tempfile::TempDir,
    repo: PathBuf,
    api: FakeApi,
    /// The key the session's commands find in their environment.
    key: &'static str,
}

impl Sandbox {
    fn new(api: &FakeApi) -> Self {
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        Self {
            home,
            repo,
            api: api.clone(),
            key: KEY,
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"));
        command
            .env_clear()
            .env("HOME", self.home.path())
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("CONTEXTSTREAM_API_URL", self.api.url())
            .env("CONTEXTSTREAM_API_KEY", self.key)
            .current_dir(&self.repo)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    /// `contextstream-mcp await-answer` with a quick poll, run to its end.
    fn await_answer(&self, arguments: &[&str]) -> Output {
        let mut command = self.command();
        command.arg("await-answer").args(arguments);
        finish(command, None, Duration::from_secs(60))
    }

    /// One hook of the session, with the hooks' mid-run notices on.
    fn hook(&self, hook: &str, mut input: Value) -> String {
        input["session_id"] = "3fa9c2d17e0b5a66aa11bb22cc33dd44".into();
        input["cwd"] = self.repo.to_string_lossy().into_owned().into();
        let mut command = self.command();
        command
            .args(["hook", hook, "--contextstream-managed-hook=v1"])
            .env("CONTEXTSTREAM_WORKSPACE_ID", WORKSPACE)
            // The shortest the poll goes; the tests wait it out.
            .env("CONTEXTSTREAM_NOTICE_POLL_SECS", "5");
        let output = finish(command, Some(input.to_string()), Duration::from_secs(30));
        assert!(
            output.status.success(),
            "{hook}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// A tool call of the session starts: what the hook shows the agent.
    fn tool_call_starts(&self) -> String {
        let shown = self.hook(
            "pre-tool-use",
            json!({
                "hook_event_name": "PreToolUse",
                "tool_name": "Read",
                "tool_input": {"file_path": self.repo.join("notes.txt")},
            }),
        );
        strings_of(&serde_json::from_str::<Value>(shown.trim()).unwrap_or(Value::Null))
    }

    /// The session reported a run with `progress`, answered in words as a
    /// local server does.
    fn reported(&self, run: &str) {
        self.hook(
            "post-tool-use",
            json!({
                "hook_event_name": "PostToolUse",
                "tool_name": "mcp__contextstream__progress",
                "tool_input": {"step": "Running the tests"},
                "tool_response": {"isError": false, "content": [{
                    "type": "text",
                    "text": format!("[PROGRESS] Recorded: step. state=working run_id={run}"),
                }]},
            }),
        );
    }

    /// Call a tool until the hook shows something, for at most `within`.
    fn shown_within(&self, within: Duration) -> String {
        let deadline = Instant::now() + within;
        loop {
            let shown = self.tool_call_starts();
            if !shown.is_empty() || Instant::now() >= deadline {
                return shown;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

/// Every string in a hook's output, a line each.
fn strings_of(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Array(items) => items
            .iter()
            .map(strings_of)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        Value::Object(fields) => fields
            .values()
            .map(strings_of)
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Run a command to its end, or fail the test when it outlives `within`.
fn finish(mut command: Command, stdin: Option<String>, within: Duration) -> Output {
    let mut child = command.spawn().unwrap();
    let mut pipe = child.stdin.take().unwrap();
    if let Some(input) = stdin {
        pipe.write_all(input.as_bytes()).unwrap();
    }
    drop(pipe);
    let deadline = Instant::now() + within;
    loop {
        if child.try_wait().unwrap().is_some() {
            return child.wait_with_output().unwrap();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output().unwrap();
            panic!(
                "the command did not end within {within:?}; it printed {:?} and {:?}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn out(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn err(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

// ============================================================================
// The waiter
// ============================================================================

#[test]
fn the_waiter_wakes_the_agent_when_an_answer_arrives_and_claims_nothing() {
    let api = FakeApi::start();
    let sandbox = Sandbox::new(&api);
    // The person answers a moment after the agent went idle.
    let later = api.clone();
    let answering = std::thread::spawn(move || {
        assert!(later.wait_for_reads(2, Duration::from_secs(20)));
        later.deliver(answer_delivery(RUN));
    });
    let started = Instant::now();
    let output = sandbox.await_answer(&[
        "--run-id",
        RUN,
        "--workspace-id",
        WORKSPACE,
        "--poll-secs",
        "0.2",
    ]);
    answering.join().unwrap();

    assert_eq!(output.status.code(), Some(0), "{}", err(&output));
    assert_eq!(out(&output), wake_notice());
    assert_eq!(err(&output), "");
    assert!(started.elapsed() < Duration::from_secs(20));

    // It asked for this run in this workspace, with the session's own key,
    // and waited between asks.
    let reads = api.reads();
    assert!(reads.len() >= 3, "{reads:?}");
    for read in &reads {
        assert_eq!(
            read.target,
            format!("/api/v1/decision-requests/deliveries?workspace_id={WORKSPACE}&run_id={RUN}")
        );
    }
    assert!(reads[1].at.duration_since(reads[0].at) >= Duration::from_millis(150));
    assert!(reads.iter().all(|read| read.body.is_null()), "{reads:?}");
    // It claimed nothing and confirmed nothing: the session that reads it
    // claims it, holds the claim's token, and replies with it.
    assert!(api.claims().is_empty(), "{:?}", api.claims());
    assert!(api.seen("POST", "/applied").is_empty());
    assert!(api.seen("POST", "/declined").is_empty());
    assert_eq!(api.state.lock().unwrap().deliveries[0]["state"], "pending");
    // Neither the words nor any secret are printed.
    assert!(!out(&output).contains(KEY));
    assert!(!out(&output).contains("Keep it warm"));
}

#[test]
fn the_waiter_prints_a_persons_message_under_its_own_label() {
    let api = FakeApi::start();
    let sandbox = Sandbox::new(&api);
    api.deliver(message_delivery(RUN));
    // What waits for another run is not this run's.
    api.deliver(answer_delivery(OTHER_RUN));
    let output = sandbox.await_answer(&["--run-id", RUN, "--workspace-id", WORKSPACE]);
    assert_eq!(output.status.code(), Some(0), "{}", err(&output));
    assert_eq!(out(&output), wake_notice());
    assert!(!out(&output).contains("Please rebase"));
    assert!(api.claims().is_empty(), "{:?}", api.claims());
}

#[test]
fn review8_one_notice_for_everything_that_waits_for_the_run() {
    let api = FakeApi::start();
    let sandbox = Sandbox::new(&api);
    api.deliver(answer_delivery(RUN));
    api.deliver(message_delivery(RUN));
    let output = sandbox.await_answer(&["--run-id", RUN, "--workspace-id", WORKSPACE]);
    assert_eq!(output.status.code(), Some(0), "{}", err(&output));
    assert_eq!(out(&output), wake_notice());
    assert!(api.claims().is_empty());
}

#[test]
fn the_waiter_takes_its_workspace_from_the_folder_when_none_is_given() {
    let api = FakeApi::start();
    let sandbox = Sandbox::new(&api);
    std::fs::create_dir_all(sandbox.repo.join(".contextstream")).unwrap();
    std::fs::write(
        sandbox.repo.join(".contextstream").join("config.json"),
        json!({"workspace_id": WORKSPACE}).to_string(),
    )
    .unwrap();
    api.deliver(answer_delivery(RUN));
    let output = sandbox.await_answer(&["--run-id", RUN]);
    assert_eq!(output.status.code(), Some(0), "{}", err(&output));
    assert_eq!(out(&output), wake_notice());
}

#[test]
fn the_waiter_says_in_one_line_that_nothing_arrived_and_exits_with_its_own_code() {
    let api = FakeApi::start();
    let sandbox = Sandbox::new(&api);
    let started = Instant::now();
    let output = sandbox.await_answer(&[
        "--run-id",
        RUN,
        "--workspace-id",
        WORKSPACE,
        "--timeout-min",
        "0.02",
        "--poll-secs",
        "0.2",
    ]);
    assert_eq!(output.status.code(), Some(TIMED_OUT), "{}", err(&output));
    assert_eq!(
        out(&output),
        format!(
            "contextstream: nothing arrived for run {RUN} within 0.02 minutes. Run the same command again to keep waiting.\n"
        )
    );
    assert_eq!(err(&output), "");
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(1100) && waited < Duration::from_secs(15),
        "{waited:?}"
    );
    // It kept asking until then, and claimed nothing.
    assert!(api.reads().len() <= 2, "{:?}", api.reads());
    assert!(api.claims().is_empty());
}

#[test]
fn the_waiter_waits_quietly_and_slowly_on_a_server_without_the_routes() {
    for api in [FakeApi::without_the_routes(), FakeApi::not_ready()] {
        quietly_and_slowly(&api);
    }
}

fn quietly_and_slowly(api: &FakeApi) {
    let sandbox = Sandbox::new(api);
    let output = sandbox.await_answer(&[
        "--run-id",
        RUN,
        "--workspace-id",
        WORKSPACE,
        "--timeout-min",
        "0.04",
        "--poll-secs",
        "0.2",
    ]);
    // Nothing about the 404: only the one line every wait ends with.
    assert_eq!(output.status.code(), Some(TIMED_OUT), "{}", err(&output));
    assert_eq!(
        out(&output),
        format!(
            "contextstream: nothing arrived for run {RUN} within 0.04 minutes. Run the same command again to keep waiting.\n"
        )
    );
    assert_eq!(err(&output), "");
    // 2.4 s at a poll of 0.2 s would be a dozen asks; a server that says it
    // has no such thing is asked at a sixth of that pace.
    let reads = api.reads().len();
    assert!((1..=4).contains(&reads), "{reads} reads");
}

#[test]
fn the_waiter_keeps_waiting_through_failures() {
    let api = FakeApi::start();
    api.state.lock().unwrap().failing_reads = 2;
    api.deliver(answer_delivery(RUN));
    let sandbox = Sandbox::new(&api);
    let output = sandbox.await_answer(&[
        "--run-id",
        RUN,
        "--workspace-id",
        WORKSPACE,
        "--poll-secs",
        "0.1",
    ]);
    assert_eq!(output.status.code(), Some(0), "{}", err(&output));
    assert_eq!(out(&output), wake_notice());
    assert_eq!(err(&output), "");
    let reads = api.reads();
    assert_eq!(reads.len(), 3, "{reads:?}");
    // Each failure doubles the wait before the next ask.
    let first = reads[1].at.duration_since(reads[0].at);
    let second = reads[2].at.duration_since(reads[1].at);
    assert!(first >= Duration::from_millis(180), "{first:?}");
    assert!(second >= Duration::from_millis(380), "{second:?}");
}

#[test]
fn the_waiter_does_not_start_without_credentials_or_a_workspace() {
    let api = FakeApi::start();
    let sandbox = Sandbox::new(&api);
    // No key anywhere: not in the environment, not in this HOME.
    let mut command = sandbox.command();
    command.env_remove("CONTEXTSTREAM_API_KEY").args([
        "await-answer",
        "--run-id",
        RUN,
        "--workspace-id",
        WORKSPACE,
    ]);
    let output = finish(command, None, Duration::from_secs(30));
    assert_eq!(output.status.code(), Some(1), "{}", err(&output));
    assert_eq!(
        out(&output),
        "contextstream: no ContextStream credentials were found here, so there is nothing to wait on. Run `contextstream-mcp setup` first.\n"
    );
    // A key, and no workspace given or linked to the folder.
    let output = sandbox.await_answer(&["--run-id", RUN]);
    assert_eq!(output.status.code(), Some(1), "{}", err(&output));
    assert_eq!(
        out(&output),
        "contextstream: this folder is linked to no workspace. Pass --workspace-id with the run's workspace.\n"
    );
    // A run id that is no id is refused before anything is asked.
    let output = sandbox.await_answer(&["--run-id", "the-run", "--workspace-id", WORKSPACE]);
    assert_eq!(output.status.code(), Some(2), "{}", out(&output));
    assert!(api.reads().is_empty());
}

#[test]
fn the_waiter_does_not_start_under_a_credential_nothing_comes_back_to() {
    let api = FakeApi::start();
    api.deliver(answer_delivery(RUN));
    let sandbox = Sandbox::new(&api);
    let line = "contextstream: only a person's full API key receives what a person sends a run, and the credentials found here are not one, so there is nothing to wait on.\n";
    // A key limited to one workspace.
    let mut command = sandbox.command();
    command
        .env("CONTEXTSTREAM_API_KEY", "cbiq_ws_0123456789abcdef")
        .args(["await-answer", "--run-id", RUN, "--workspace-id", WORKSPACE]);
    let output = finish(command, None, Duration::from_secs(30));
    assert_eq!(output.status.code(), Some(1), "{}", err(&output));
    assert_eq!(out(&output), line);
    // A sign-in token and no key.
    let mut command = sandbox.command();
    command
        .env_remove("CONTEXTSTREAM_API_KEY")
        .env("CONTEXTSTREAM_JWT", "header.payload.signature")
        .args(["await-answer", "--run-id", RUN, "--workspace-id", WORKSPACE]);
    let output = finish(command, None, Duration::from_secs(30));
    assert_eq!(output.status.code(), Some(1), "{}", err(&output));
    assert_eq!(out(&output), line);
    assert!(api.reads().is_empty(), "{:?}", api.reads());
    assert!(api.claims().is_empty());
}

// ============================================================================
// The hooks, mid-run
// ============================================================================

#[test]
fn the_hooks_ask_nothing_under_a_key_limited_to_one_workspace() {
    let api = FakeApi::start();
    api.deliver(answer_delivery(RUN));
    let mut sandbox = Sandbox::new(&api);
    sandbox.key = "cbiq_ws_0123456789abcdef";
    sandbox.reported(RUN);
    assert_eq!(sandbox.tool_call_starts(), "");
    std::thread::sleep(Duration::from_millis(3000));
    assert_eq!(sandbox.tool_call_starts(), "");
    assert!(api.reads().is_empty(), "{:?}", api.reads());
}

#[test]
fn the_hooks_tell_a_working_session_once_that_a_person_answered_and_claim_nothing() {
    let api = FakeApi::start();
    let sandbox = Sandbox::new(&api);
    // Nothing reported yet: the hooks know no run, and ask about none.
    assert_eq!(sandbox.tool_call_starts(), "");
    std::thread::sleep(Duration::from_millis(1500));
    assert!(api.reads().is_empty(), "{:?}", api.reads());

    sandbox.reported(RUN);
    api.deliver(answer_delivery(RUN));
    api.deliver(message_delivery(RUN));

    // The next poll is due five seconds after the first; a tool call then
    // starts it, and a later tool call shows what it found.
    std::thread::sleep(Duration::from_millis(5200));
    let shown = sandbox.shown_within(Duration::from_secs(20));
    let expected = format!(
        "[CONTEXTSTREAM LIVE NOTICE] A person answered a question this run asked (delivery_id={ANSWER}). Read it now with mcp__contextstream__progress(action=\"get\", run_id=\"{RUN}\"): it comes with how to confirm it.\n[CONTEXTSTREAM LIVE NOTICE] A person sent this run a message (delivery_id={MESSAGE}). Read it now with mcp__contextstream__progress(action=\"get\", run_id=\"{RUN}\"): it comes with how to confirm it."
    );
    assert!(shown.contains(&expected), "{shown}");
    // The words a person wrote are not the hooks' to show.
    assert!(!shown.contains("Keep it warm"), "{shown}");
    assert!(!shown.contains("Please rebase"), "{shown}");

    let reads = api.reads();
    assert_eq!(reads.len(), 1, "{reads:?}");
    assert_eq!(
        reads[0].target,
        format!("/api/v1/decision-requests/deliveries?workspace_id={WORKSPACE}&run_id={RUN}")
    );

    // Still waiting at the next poll (the agent has not read it): asked
    // again, and not said again.
    std::thread::sleep(Duration::from_millis(5200));
    assert_eq!(sandbox.tool_call_starts(), "");
    assert!(api.wait_for_reads(2, Duration::from_secs(20)));
    std::thread::sleep(Duration::from_millis(1000));
    assert_eq!(sandbox.tool_call_starts(), "");
    // The hooks only point: the claim is made where the words are shown.
    assert!(api.claims().is_empty());
    assert!(api.seen("POST", "/applied").is_empty());
}

#[test]
fn the_hooks_ask_a_server_without_the_routes_once_and_say_nothing() {
    for api in [FakeApi::without_the_routes(), FakeApi::not_ready()] {
        hooks_say_nothing(&api);
    }
}

fn hooks_say_nothing(api: &FakeApi) {
    let sandbox = Sandbox::new(api);
    sandbox.reported(RUN);
    assert_eq!(sandbox.tool_call_starts(), "");
    assert!(api.wait_for_reads(1, Duration::from_secs(20)));
    std::thread::sleep(Duration::from_millis(1000));
    assert_eq!(sandbox.tool_call_starts(), "");
    // The next poll does not ask a server that said it has no such thing.
    std::thread::sleep(Duration::from_millis(5200));
    assert_eq!(sandbox.tool_call_starts(), "");
    std::thread::sleep(Duration::from_millis(2500));
    assert_eq!(sandbox.tool_call_starts(), "");
    assert_eq!(api.reads().len(), 1, "{:?}", api.reads());
}

#[test]
fn a_run_the_session_finished_is_no_longer_asked_about() {
    let api = FakeApi::start();
    let sandbox = Sandbox::new(&api);
    sandbox.reported(RUN);
    sandbox.hook(
        "post-tool-use",
        json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "mcp__contextstream__progress",
            "tool_input": {"status": "completed", "summary": "Done."},
            "tool_response": {"isError": false, "content": [{
                "type": "text",
                "text": format!("[PROGRESS] Recorded: finished. state=finished run_id={RUN}\nThe run is finished; a later report on it opens it again."),
            }]},
        }),
    );
    api.deliver(answer_delivery(RUN));
    assert_eq!(sandbox.tool_call_starts(), "");
    std::thread::sleep(Duration::from_millis(2500));
    assert_eq!(sandbox.tool_call_starts(), "");
    assert!(api.reads().is_empty(), "{:?}", api.reads());
}

#[test]
fn review8_second_waiter_exits_without_a_read_and_no_read_starts_at_the_deadline() {
    let api = FakeApi::start();
    let sandbox = Sandbox::new(&api);
    let args = [
        "--run-id",
        RUN,
        "--workspace-id",
        WORKSPACE,
        "--timeout-min",
        "0.025",
        "--poll-secs",
        "30",
    ];
    let mut first = sandbox
        .command()
        .arg("await-answer")
        .args(args)
        .spawn()
        .unwrap();
    assert!(api.wait_for_reads(1, Duration::from_secs(5)));
    let second = sandbox.await_answer(&args);
    assert_eq!(second.status.code(), Some(1), "{}", out(&second));
    assert!(out(&second).contains("already waiting"));
    assert_eq!(first.wait().unwrap().code(), Some(TIMED_OUT));
    assert_eq!(api.reads().len(), 1);
}
