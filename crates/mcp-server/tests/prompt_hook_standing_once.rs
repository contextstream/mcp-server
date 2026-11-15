//! Drive the real prompt hook over several prompts of one session: the
//! standing text (workflow block, preferences, lessons, decisions, coaching)
//! goes out once per window, a coordination notice is printed once and then
//! counted, and what belongs to a single prompt is never left out.
//!
//! Every run gets its own HOME and a loopback API that answers with synthetic
//! hook context of the shape the real API sends, so nothing touches the
//! developer's configuration or the network.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

const COACHING_HEADER: &str =
    "[COACHING] You have corrected the agent on these before — do NOT repeat them:";
const ITEM_A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
const NOTICE_A1: &str = "10101010-1010-4010-8010-101010101010";
const NOTICE_A2: &str = "20202020-2020-4020-8020-202020202020";
const ITEM_B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
const NOTICE_B1: &str = "30303030-3030-4030-8030-303030303030";

/// The text `POST /api/v1/context/hook` answers with: synthetic, in the order
/// and with the tags of a real answer.
#[derive(Clone)]
struct HookText {
    preferences: Vec<String>,
    decisions: Vec<String>,
    coaching: Vec<String>,
    notices: Vec<String>,
    instruct: Vec<String>,
}

impl HookText {
    fn new() -> Self {
        Self {
            preferences: (1..=5)
                .map(|n| format!("[PREFERENCE] style: rule_{n}: {{\"value\":\"choice {n}\"}}"))
                .collect(),
            decisions: (1..=3)
                .map(|n| format!("[DECISION] (today old) Decision number {n}"))
                .collect(),
            coaching: (1..=5).map(|n| format!("- Coached rule {n}")).collect(),
            notices: Vec::new(),
            instruct: Vec::new(),
        }
    }

    fn render(&self) -> String {
        let mut lines: Vec<String> = vec![
            "[CONTEXTSTREAM] Default: call context(user_message=\"...\") first. For quick read-only turns, direct read-only calls are allowed.".to_string(),
        ];
        lines.extend(self.preferences.iter().cloned());
        lines.extend(
            (1..=5).map(|n| format!("[LESSONS_WARNING] Lesson {n} — check it before you ship")),
        );
        lines.extend(self.decisions.iter().cloned());
        lines.push(
            "[HOOK CONTEXT] Reuse the same session_id across turns so hook guidance stays coherent."
                .to_string(),
        );
        if !self.coaching.is_empty() {
            lines.push(COACHING_HEADER.to_string());
            lines.extend(self.coaching.iter().cloned());
        }
        lines.extend(self.notices.iter().cloned());
        lines.extend(self.instruct.iter().cloned());
        lines.join("\n")
    }
}

fn notice(item: &str, notice_id: &str, text: &str) -> String {
    format!(
        "[COORDINATION] (info) from Acme/widgets: item={item} {text} — ack via coordination(action=\"ack\", notice_id=\"{notice_id}\") or reply via coordination(action=\"reply\", notice_id=\"{notice_id}\", message=\"...\")"
    )
}

fn note(lines: usize, ago: &str) -> String {
    format!("[CONTEXTSTREAM] Standing guidance ({lines} lines) was shown {ago} and still applies.")
}

fn line_count(text: &str) -> usize {
    text.lines().filter(|line| !line.trim().is_empty()).count()
}

/// A loopback API: answers every request with the hook context it holds now,
/// and keeps the path and body of each request.
struct Api {
    url: String,
    context: Arc<Mutex<String>>,
    requests: Arc<Mutex<Vec<(String, String)>>>,
}

impl Api {
    fn start(text: &HookText) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let context = Arc::new(Mutex::new(text.render()));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (serving, seen) = (context.clone(), requests.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (serving, seen) = (serving.clone(), seen.clone());
                std::thread::spawn(move || answer(stream, &serving, &seen));
            }
        });
        Self {
            url,
            context,
            requests,
        }
    }

    fn serve(&self, text: &HookText) {
        *self.context.lock().unwrap() = text.render();
    }

    /// The bodies of the prompt hook's own requests, in order.
    fn hook_requests(&self) -> Vec<Value> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path == "/api/v1/context/hook")
            .map(|(_, body)| serde_json::from_str(body).unwrap())
            .collect()
    }
}

fn answer(mut stream: TcpStream, context: &Mutex<String>, requests: &Mutex<Vec<(String, String)>>) {
    let mut request = Vec::new();
    let mut chunk = [0; 4096];
    let header_end = loop {
        let Ok(count) = stream.read(&mut chunk) else {
            return;
        };
        if count == 0 {
            return;
        }
        request.extend_from_slice(&chunk[..count]);
        if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let headers = String::from_utf8_lossy(&request[..header_end]).to_string();
    let length = headers
        .to_ascii_lowercase()
        .lines()
        .find_map(|line| {
            line.strip_prefix("content-length:")
                .and_then(|value| value.trim().parse::<usize>().ok())
        })
        .unwrap_or(0);
    while request.len() < header_end + length {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(count) => request.extend_from_slice(&chunk[..count]),
        }
    }
    let path = headers
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_string();
    let sent = String::from_utf8_lossy(&request[header_end..]).to_string();
    requests.lock().unwrap().push((path, sent));
    let body = json!({ "data": { "context": *context.lock().unwrap() } }).to_string();
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

struct Sandbox {
    home: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        Self {
            home: tempfile::tempdir().unwrap(),
        }
    }

    /// Runs one hook and returns its standard output. `api`: the loopback API
    /// and a key for it; without, no key and an address nothing listens on.
    fn hook(&self, hook: &str, input: Value, api: Option<&Api>, env: &[(&str, &str)]) -> String {
        let mut command = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"));
        command
            .args(["hook", hook, "--contextstream-managed-hook=v1"])
            .env_clear()
            .env("HOME", self.home.path())
            .env("CLAUDE_CONFIG_DIR", self.home.path().join(".claude"))
            .env(
                "CONTEXTSTREAM_API_URL",
                api.map_or("http://127.0.0.1:9", |api| api.url.as_str()),
            )
            .current_dir(self.home.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if api.is_some() {
            command.env("CONTEXTSTREAM_API_KEY", "loopback-test-key");
        }
        for (key, value) in env {
            command.env(key, value);
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
        String::from_utf8(output.stdout).unwrap()
    }

    /// One prompt the way Claude Code sends it; returns the `additionalContext`
    /// after checking the output is one line of the JSON both hosts accept.
    fn prompt_with(
        &self,
        api: Option<&Api>,
        session: Option<&str>,
        prompt: &str,
        env: &[(&str, &str)],
    ) -> String {
        let mut input = json!({
            "hook_event_name": "UserPromptSubmit",
            "cwd": self.home.path(),
            "prompt": prompt,
        });
        if let Some(session) = session {
            input["session_id"] = json!(session);
        }
        additional_context(&self.hook("user-prompt-submit", input, api, env))
    }

    fn prompt(&self, api: &Api, session: &str) -> String {
        self.prompt_with(Some(api), Some(session), "Explain this module.", &[])
    }

    fn state_dir(&self) -> PathBuf {
        self.home.path().join(".contextstream/mid-run-notices")
    }

    /// The state files of the sessions that ran in this HOME.
    fn state_files(&self) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(self.state_dir())
            .map(|entries| {
                entries
                    .flatten()
                    .map(|entry| entry.path())
                    .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
                    .collect()
            })
            .unwrap_or_default();
        files.sort();
        files
    }

    fn only_state(&self) -> (PathBuf, Value) {
        let files = self.state_files();
        assert_eq!(files.len(), 1, "one session, one state file: {files:?}");
        let state = serde_json::from_slice(&std::fs::read(&files[0]).unwrap()).unwrap();
        (files[0].clone(), state)
    }

    /// Moves the session's window start `secs` into the past.
    fn age_window(&self, secs: u64) {
        let (path, mut state) = self.only_state();
        let started = state["standing_window_started_unix_ms"].as_u64().unwrap();
        state["standing_window_started_unix_ms"] = json!(started - secs * 1_000);
        std::fs::write(path, state.to_string()).unwrap();
    }
}

/// The context of a Claude Code or Codex prompt hook output. The output is
/// one line of compact JSON with only the fields both hosts accept.
fn additional_context(stdout: &str) -> String {
    let line = stdout.strip_suffix('\n').expect("the output ends its line");
    assert!(!line.contains('\n'), "one line of JSON: {stdout}");
    let output: Value = serde_json::from_str(line).expect("one complete JSON object");
    assert_eq!(output.as_object().unwrap().len(), 1, "{output}");
    let specific = output["hookSpecificOutput"].as_object().unwrap();
    assert_eq!(specific.len(), 2);
    assert_eq!(specific["hookEventName"], "UserPromptSubmit");
    let context = specific["additionalContext"].as_str().unwrap();
    assert!(!context.trim().is_empty(), "never an empty context");
    context.to_string()
}

#[test]
fn the_first_prompt_is_full_and_the_second_carries_one_line() {
    let text = HookText::new();
    let api = Api::start(&text);
    let sandbox = Sandbox::new();

    let first = sandbox.prompt_with(Some(&api), Some("s1"), "First question.", &[]);
    let (workflow, rest) = first.split_once("\n\n").unwrap();
    assert!(workflow.starts_with("[CONTEXTSTREAM WORKFLOW "), "{first}");
    assert_eq!(line_count(workflow), 7, "header and six numbered steps");
    assert_eq!(rest, text.render());

    let second = sandbox.prompt_with(Some(&api), Some("s1"), "Second question.", &[]);
    assert_eq!(second, note(line_count(&first), "under a minute ago"));

    // What the hook does besides printing still happens with every prompt:
    // the API call that checks the session in for coordination, and the
    // record the mid-run poll reads. Nothing asks the API to acknowledge.
    let requests = api.hook_requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0]["task_summary"], "First question.");
    assert_eq!(requests[1]["task_summary"], "Second question.");
    for request in &requests {
        assert_eq!(request["hook_type"], "user_prompt_submit");
        assert_eq!(request["session_id"], "s1");
        assert!(request.get("consume_notices").is_none(), "{request}");
    }
    let (_, state) = sandbox.only_state();
    assert_eq!(state["seen_preference_lines"].as_array().unwrap().len(), 5);
    assert_eq!(state["seen_lesson_lines"].as_array().unwrap().len(), 5);
}

#[test]
fn what_is_new_inside_the_window_is_delivered_with_the_note() {
    let mut text = HookText::new();
    let api = Api::start(&text);
    let sandbox = Sandbox::new();
    let first = sandbox.prompt(&api, "s1");

    // A new decision takes the place of the oldest.
    text.decisions.remove(2);
    text.decisions
        .insert(0, "[DECISION] (today old) A decision made just now".into());
    api.serve(&text);
    assert_eq!(
        sandbox.prompt(&api, "s1"),
        format!(
            "{}\n[DECISION] (today old) A decision made just now",
            note(line_count(&first) - 1, "under a minute ago")
        )
    );

    // A preference set that changed: the preference that is new, alone.
    text.preferences[4] = "[PREFERENCE] style: rule_5: {\"value\":\"changed\"}".into();
    api.serve(&text);
    assert_eq!(
        sandbox.prompt(&api, "s1"),
        format!(
            "{}\n[PREFERENCE] style: rule_5: {{\"value\":\"changed\"}}",
            note(line_count(&first) - 1, "under a minute ago")
        )
    );
}

#[test]
fn a_coaching_block_comes_with_its_header_and_so_does_a_new_bullet() {
    let mut text = HookText::new();
    let bullets = std::mem::take(&mut text.coaching);
    let api = Api::start(&text);
    let sandbox = Sandbox::new();
    let first = sandbox.prompt(&api, "s1");
    assert!(!first.contains("[COACHING]"));

    text.coaching = bullets.clone();
    api.serve(&text);
    assert_eq!(
        sandbox.prompt(&api, "s1"),
        format!(
            "{}\n{COACHING_HEADER}\n{}",
            note(line_count(&first), "under a minute ago"),
            bullets.join("\n")
        )
    );

    text.coaching.push("- A rule learned a moment ago".into());
    api.serve(&text);
    assert_eq!(
        sandbox.prompt(&api, "s1"),
        format!(
            "{}\n{COACHING_HEADER}\n- A rule learned a moment ago",
            note(line_count(&first) + bullets.len(), "under a minute ago")
        )
    );
}

#[test]
fn a_coordination_notice_is_printed_once_then_counted_until_it_is_gone() {
    let mut text = HookText::new();
    let notice_a = notice(
        ITEM_A,
        NOTICE_A1,
        "Migration 12 is claimed by another session.",
    );
    text.notices.push(notice_a.clone());
    let api = Api::start(&text);
    let sandbox = Sandbox::new();

    let first = sandbox.prompt(&api, "s1");
    assert!(first.ends_with(&notice_a), "{first}");
    let standing = line_count(&first) - 1;

    let counted_a1 = format!(
        "[COORDINATION] 1 notice shown earlier is still unacknowledged: ack via coordination(action=\"ack\", notice_id=\"{NOTICE_A1}\"), or read coordination(action=\"inbox\")."
    );
    assert_eq!(
        sandbox.prompt(&api, "s1"),
        format!("{}\n{counted_a1}", note(standing, "under a minute ago"))
    );

    // A second notice is printed in full while the first is counted.
    let notice_b = notice(ITEM_B, NOTICE_B1, "Both sessions edit the importer.");
    text.notices.push(notice_b.clone());
    api.serve(&text);
    assert_eq!(
        sandbox.prompt(&api, "s1"),
        format!(
            "{}\n{notice_b}\n{counted_a1}",
            note(standing, "under a minute ago")
        )
    );

    // The same item under a new notice id is not printed again.
    text.notices[0] = notice(
        ITEM_A,
        NOTICE_A2,
        "Migration 12 is claimed by another session.",
    );
    api.serve(&text);
    assert_eq!(
        sandbox.prompt(&api, "s1"),
        format!(
            "{}\n[COORDINATION] 2 notices shown earlier are still unacknowledged: ack via coordination(action=\"ack\", notice_id=\"{NOTICE_A2}\") / \"{NOTICE_B1}\", or read coordination(action=\"inbox\").",
            note(standing, "under a minute ago")
        )
    );

    // Acknowledged: the API sends them no more, and the count goes.
    text.notices.clear();
    api.serve(&text);
    assert_eq!(
        sandbox.prompt(&api, "s1"),
        note(standing, "under a minute ago")
    );

    // The prompt hook never asked the API to acknowledge a notice.
    for request in api.hook_requests() {
        assert!(request.get("consume_notices").is_none(), "{request}");
    }
}

#[test]
fn when_the_window_has_passed_the_prompt_is_full_again() {
    let mut text = HookText::new();
    text.notices
        .push(notice(ITEM_A, NOTICE_A1, "Migration 12 is claimed."));
    let api = Api::start(&text);
    let sandbox = Sandbox::new();
    let first = sandbox.prompt(&api, "s1");

    // Nine minutes in, the default window of 600 s still holds.
    sandbox.age_window(540);
    let inside = sandbox.prompt(&api, "s1");
    assert!(
        inside.starts_with(&note(line_count(&first) - 1, "9 min ago")),
        "{inside}"
    );

    sandbox.age_window(61);
    assert_eq!(sandbox.prompt(&api, "s1"), first, "full, notice included");
    // That delivery started a new window.
    let again = sandbox.prompt(&api, "s1");
    assert!(
        again.starts_with(&note(line_count(&first) - 1, "under a minute ago")),
        "{again}"
    );

    // A shorter window from the environment.
    let short = [("CONTEXTSTREAM_HOOK_STANDING_REPEAT_SECS", "120")];
    sandbox.age_window(121);
    assert_eq!(
        sandbox.prompt_with(Some(&api), Some("s1"), "Explain this module.", &short),
        first
    );
}

#[test]
fn a_window_of_zero_repeats_everything_with_every_prompt() {
    let mut text = HookText::new();
    text.notices
        .push(notice(ITEM_A, NOTICE_A1, "Migration 12 is claimed."));
    let api = Api::start(&text);
    let sandbox = Sandbox::new();
    let off = [("CONTEXTSTREAM_HOOK_STANDING_REPEAT_SECS", "0")];

    let first = sandbox.prompt_with(Some(&api), Some("s1"), "Explain this module.", &off);
    assert!(first.ends_with(&text.render()));
    for _ in 0..2 {
        assert_eq!(
            sandbox.prompt_with(Some(&api), Some("s1"), "Explain this module.", &off),
            first
        );
    }
    // Nothing of the throttle is kept for a session that has it off.
    let (_, state) = sandbox.only_state();
    assert!(state.get("standing_window_started_unix_ms").is_none());
    assert!(state.get("standing_line_hashes").is_none());
}

#[test]
fn a_prompt_without_a_session_id_gets_everything_every_time() {
    let text = HookText::new();
    let api = Api::start(&text);
    let sandbox = Sandbox::new();

    let first = sandbox.prompt_with(Some(&api), None, "Explain this module.", &[]);
    assert!(first.ends_with(&text.render()));
    for _ in 0..2 {
        assert_eq!(
            sandbox.prompt_with(Some(&api), None, "Explain this module.", &[]),
            first
        );
    }
    let (_, state) = sandbox.only_state();
    assert!(state.get("standing_window_started_unix_ms").is_none());
}

#[test]
fn session_start_and_post_compact_make_the_next_prompt_full() {
    let mut text = HookText::new();
    text.notices
        .push(notice(ITEM_A, NOTICE_A1, "Migration 12 is claimed."));
    let api = Api::start(&text);
    let sandbox = Sandbox::new();
    let cwd = sandbox.home.path();

    let first = sandbox.prompt(&api, "s1");
    let resets = [
        (
            "session-start",
            json!({"hook_event_name": "SessionStart", "session_id": "s1", "cwd": cwd, "source": "startup"}),
        ),
        (
            "session-start",
            json!({"hook_event_name": "SessionStart", "session_id": "s1", "cwd": cwd, "source": "resume"}),
        ),
        (
            "session-start",
            json!({"hook_event_name": "SessionStart", "session_id": "s1", "cwd": cwd, "source": "compact"}),
        ),
        (
            "post-compact",
            json!({"hook_event_name": "PostCompact", "session_id": "s1", "cwd": cwd, "trigger": "auto"}),
        ),
        // Codex takes no context from PostCompact; the reset still happens.
        (
            "post-compact",
            json!({"hook_event_name": "PostCompact", "session_id": "s1", "cwd": cwd, "trigger": "auto", "turn_id": "t1", "model": "gpt-5"}),
        ),
    ];
    for (hook, input) in resets {
        let throttled = sandbox.prompt(&api, "s1");
        assert_ne!(throttled, first, "inside the window before {hook}");
        assert!(throttled.len() < first.len() / 4);

        // Another session's lifecycle event changes nothing for this one.
        let mut other = input.clone();
        other["session_id"] = json!("s2");
        sandbox.hook(hook, other, None, &[]);
        assert_eq!(sandbox.prompt(&api, "s1"), throttled);

        sandbox.hook(hook, input.clone(), None, &[]);
        assert_eq!(
            sandbox.prompt(&api, "s1"),
            first,
            "full after {hook}: {input}"
        );
    }
}

#[test]
fn a_state_file_that_cannot_be_used_costs_no_guidance() {
    let text = HookText::new();
    let api = Api::start(&text);

    // Corrupt state: the prompt is full, and the state is rebuilt from it.
    let sandbox = Sandbox::new();
    let first = sandbox.prompt(&api, "s1");
    let throttled = sandbox.prompt(&api, "s1");
    assert_ne!(throttled, first);
    let (path, _) = sandbox.only_state();
    std::fs::write(
        &path,
        b"{\"standing_window_started_unix_ms\": \"not a number",
    )
    .unwrap();
    assert_eq!(sandbox.prompt(&api, "s1"), first);
    assert_eq!(sandbox.prompt(&api, "s1"), throttled);

    // Valid JSON of the wrong shape is no state either.
    std::fs::write(&path, b"[1, 2, 3]").unwrap();
    assert_eq!(sandbox.prompt(&api, "s1"), first);

    // No place to keep state at all: every prompt is full.
    let sandbox = Sandbox::new();
    std::fs::create_dir_all(sandbox.home.path().join(".contextstream")).unwrap();
    std::fs::write(sandbox.state_dir(), b"a file where the folder would be").unwrap();
    let first = sandbox.prompt(&api, "s1");
    assert!(first.ends_with(&text.render()));
    for _ in 0..2 {
        assert_eq!(sandbox.prompt(&api, "s1"), first);
    }
}

#[test]
fn guidance_chosen_from_the_prompt_is_delivered_with_every_prompt() {
    let text = HookText::new();
    let api = Api::start(&text);
    let sandbox = Sandbox::new();
    let first = sandbox.prompt(&api, "s1");
    let one_line = note(line_count(&first), "under a minute ago");

    for (prompt, marker) in [
        (
            "Please save this decision for future reference.",
            "CONTEXTSTREAM DOCUMENT STORAGE",
        ),
        (
            "Please save this decision for future reference.",
            "CONTEXTSTREAM DOCUMENT STORAGE",
        ),
        ("Prepare a handoff for the next agent.", "CANONICAL HANDOFF"),
        ("Pick up where we left off.", "[CONTEXTSTREAM RESUME]"),
        ("Pick up where we left off.", "[CONTEXTSTREAM RESUME]"),
    ] {
        let context = sandbox.prompt_with(Some(&api), Some("s1"), prompt, &[]);
        let (standing, guidance) = context.split_once("\n\n").expect("two parts");
        assert_eq!(standing, one_line, "{prompt}");
        assert!(guidance.contains(marker), "{prompt}: {context}");
    }
}

#[test]
fn instruct_entries_are_delivered_with_every_prompt() {
    let mut text = HookText::new();
    text.instruct
        .push("[INSTRUCT] Answer in French until told otherwise.".into());
    let api = Api::start(&text);
    let sandbox = Sandbox::new();
    let first = sandbox.prompt(&api, "s1");
    assert!(first.ends_with("[INSTRUCT] Answer in French until told otherwise."));
    for _ in 0..2 {
        assert_eq!(
            sandbox.prompt(&api, "s1"),
            format!(
                "{}\n[INSTRUCT] Answer in French until told otherwise.",
                note(line_count(&first) - 1, "under a minute ago")
            )
        );
    }
}

#[test]
fn two_sessions_do_not_affect_each_other() {
    let mut text = HookText::new();
    let api = Api::start(&text);
    let sandbox = Sandbox::new();

    let first = sandbox.prompt(&api, "s1");
    assert_eq!(
        sandbox.prompt(&api, "s2"),
        first,
        "the other session's first"
    );
    let one_line = note(line_count(&first), "under a minute ago");
    assert_eq!(sandbox.prompt(&api, "s1"), one_line);
    assert_eq!(sandbox.prompt(&api, "s2"), one_line);
    assert_eq!(sandbox.state_files().len(), 2);

    // What is new is new to each of them.
    text.decisions[0] = "[DECISION] (today old) A decision made just now".into();
    api.serve(&text);
    for session in ["s1", "s2", "s3"] {
        let context = sandbox.prompt(&api, session);
        assert!(
            context.contains("A decision made just now"),
            "{session}: {context}"
        );
    }
}

#[test]
fn the_throttle_does_not_depend_on_mid_run_notices() {
    let text = HookText::new();
    let api = Api::start(&text);
    let sandbox = Sandbox::new();
    let off = [("CONTEXTSTREAM_MID_RUN_NOTICES", "0")];

    let first = sandbox.prompt_with(Some(&api), Some("s1"), "Explain this module.", &off);
    assert!(first.ends_with(&text.render()));
    assert_eq!(
        sandbox.prompt_with(Some(&api), Some("s1"), "Explain this module.", &off),
        note(line_count(&first), "under a minute ago")
    );
    // The mid-run record itself stays off.
    let (_, state) = sandbox.only_state();
    assert!(state["seen_preference_lines"]
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn the_static_reminder_is_whole_the_first_time_and_then_left_out() {
    // No credentials: the static reminder from the first prompt on.
    let sandbox = Sandbox::new();
    let first = sandbox.prompt_with(None, Some("s1"), "Explain this module.", &[]);
    assert!(first.contains("[CONTEXTSTREAM EXTENDED GUIDANCE]"));
    assert!(first.ends_with("[END]"));
    assert_eq!(
        sandbox.prompt_with(None, Some("s1"), "Explain this module.", &[]),
        note(line_count(&first), "under a minute ago")
    );

    // The API answers the first prompt and is unreachable for the second:
    // the reminder is new to the session and goes out whole, once.
    let text = HookText::new();
    let api = Api::start(&text);
    let sandbox = Sandbox::new();
    let online = sandbox.prompt(&api, "s1");
    let key = [("CONTEXTSTREAM_API_KEY", "loopback-test-key")];
    let offline = sandbox.prompt_with(None, Some("s1"), "Explain this module.", &key);
    let reminder = &first[first.find("[CONTEXTSTREAM EXTENDED GUIDANCE]").unwrap()..];
    assert_eq!(
        offline,
        format!("{}\n{reminder}", note(7, "under a minute ago"))
    );
    assert_eq!(
        sandbox.prompt_with(None, Some("s1"), "Explain this module.", &key),
        note(7 + line_count(reminder), "under a minute ago")
    );
    // Back online inside the window: the hook context was shown already.
    assert_eq!(
        sandbox.prompt(&api, "s1"),
        note(line_count(&online), "under a minute ago")
    );
}

#[test]
fn codex_is_throttled_like_claude_code_and_other_formats_are_not() {
    let text = HookText::new();
    let api = Api::start(&text);

    // Codex sends the same event with a turn id and a model.
    let sandbox = Sandbox::new();
    let codex = |prompt: &str| {
        additional_context(&sandbox.hook(
            "user-prompt-submit",
            json!({
                "hook_event_name": "UserPromptSubmit",
                "session_id": "codex-1",
                "turn_id": "turn-1",
                "model": "gpt-5",
                "cwd": sandbox.home.path(),
                "prompt": prompt,
            }),
            Some(&api),
            &[],
        ))
    };
    let first = codex("Explain this module.");
    assert!(first.ends_with(&text.render()));
    assert_eq!(
        codex("And this one."),
        note(line_count(&first), "under a minute ago")
    );

    // A Cline-format editor has no session-start hook to make the next prompt
    // full after a compaction, so it keeps getting everything.
    let sandbox = Sandbox::new();
    let cline = || {
        let stdout = sandbox.hook(
            "user-prompt-submit",
            json!({
                "hookName": "UserPromptSubmit",
                "sessionId": "cline-1",
                "workspaceRoots": [sandbox.home.path()],
                "prompt": "Explain this module.",
            }),
            Some(&api),
            &[],
        );
        let output: Value = serde_json::from_str(stdout.trim()).unwrap();
        assert_eq!(output["cancel"], false);
        output["contextModification"].as_str().unwrap().to_string()
    };
    let first = cline();
    assert!(first.contains(&text.render()));
    assert_eq!(cline(), first);
    assert_eq!(cline(), first);
}
