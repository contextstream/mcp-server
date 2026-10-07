//! Drive the real hook binary for git session tagging: what a git-mutating shell
//! command leaves for the managed git hooks to read, for Claude Code, Cursor and
//! Codex.
//!
//! The session hint must name the session id `init` returned (what a transcript
//! and a resume card use), never the host's own session id, and a host session
//! that never ran `init` must tag nothing. Every run gets its own HOME, no
//! credentials and an unroutable API URL, so nothing touches the developer's
//! configuration or the network.
//!
//! The Cursor and Codex payloads are the shapes those hosts are documented to
//! send; they have not been captured from a running Cursor or Codex here.

use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const INIT_SESSION: &str = "11111111-1111-4111-8111-111111111111";

struct Sandbox {
    home: tempfile::TempDir,
    repo: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .output()
            .expect("git is installed");
        assert!(git.status.success());
        Self { home, repo }
    }

    fn run(&self, hook: &str, input: Value) {
        let mut command = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"));
        command
            .args(["hook", hook, "--contextstream-managed-hook=v1"])
            .env_clear()
            .env("HOME", self.home.path())
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("CONTEXTSTREAM_API_URL", "http://127.0.0.1:9")
            .env("CONTEXTSTREAM_REMINDER_ENABLED", "false")
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

    /// `init` returned `INIT_SESSION`, as a host's PostToolUse hook receives it.
    /// `host_key` names the host session the way that host does.
    fn record_init(&self, host_key: (&str, &str), event: &str) {
        let mut input = json!({
            "hook_event_name": event,
            "cwd": self.repo,
            "tool_name": "mcp__contextstream__init",
            "tool_input": {"folder_path": self.repo},
            "tool_response": {
                "isError": false,
                "content": [{"type": "text", "text": "Session ready"}],
                "structuredContent": {
                    "session_id": INIT_SESSION,
                    "workspace_id": "33333333-3333-4333-8333-333333333333",
                    "project_id": "44444444-4444-4444-8444-444444444444",
                },
            },
        });
        input[host_key.0] = host_key.1.into();
        self.run("post-tool-use", input);
    }

    /// The hint the managed git hooks would read, if a hint was written.
    fn hint(&self) -> Option<Value> {
        let dir = self
            .home
            .path()
            .join(".contextstream")
            .join("git-session-hints");
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .ok()?
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .collect();
        files.sort();
        let file: &Path = files.first()?;
        Some(serde_json::from_str(&std::fs::read_to_string(file).unwrap()).unwrap())
    }
}

#[test]
fn claude_code_tags_with_the_id_init_returned_not_its_own() {
    let sandbox = Sandbox::new();
    sandbox.record_init(("session_id", "host-claude-1"), "PostToolUse");
    sandbox.run(
        "git-bash-observed",
        json!({
            "hook_event_name": "PostToolUse",
            "session_id": "host-claude-1",
            "cwd": sandbox.repo,
            "tool_name": "Bash",
            "tool_input": {"command": "git commit -m x"},
        }),
    );
    let hint = sandbox.hint().expect("a hint was written");
    assert_eq!(hint["session_id"], INIT_SESSION);
    assert_ne!(hint["session_id"], "host-claude-1");
    assert_eq!(hint["agent"], "claude_code");
}

#[test]
fn a_host_session_that_never_ran_init_tags_nothing() {
    let sandbox = Sandbox::new();
    sandbox.run(
        "git-bash-observed",
        json!({
            "hook_event_name": "PostToolUse",
            "session_id": "host-never-initialised",
            "cwd": sandbox.repo,
            "tool_name": "Bash",
            "tool_input": {"command": "git push origin main"},
        }),
    );
    let hint = sandbox.hint().expect("the agent is still recorded");
    assert!(
        hint["session_id"].is_null(),
        "the host's own id must never be sent: {hint}"
    );
    assert_eq!(hint["agent"], "claude_code");
}

#[test]
fn cursor_tags_with_the_id_init_returned() {
    let sandbox = Sandbox::new();
    sandbox.record_init(("conversation_id", "host-cursor-1"), "afterMCPExecution");
    sandbox.run(
        "git-bash-observed",
        json!({
            "hook_event_name": "afterShellExecution",
            "conversation_id": "host-cursor-1",
            "generation_id": "gen-1",
            "workspace_roots": [sandbox.repo],
            "command": "git commit -m x",
            "output": "",
            "duration": 120,
        }),
    );
    let hint = sandbox.hint().expect("a hint was written");
    assert_eq!(hint["session_id"], INIT_SESSION);
    assert_eq!(hint["agent"], "cursor");
}

#[test]
fn codex_tags_with_the_id_init_returned_from_an_argument_list() {
    let sandbox = Sandbox::new();
    sandbox.record_init(("session_id", "host-codex-1"), "PostToolUse");
    sandbox.run(
        "git-bash-observed",
        json!({
            "hook_event_name": "PostToolUse",
            "session_id": "host-codex-1",
            "turn_id": "turn-1",
            "cwd": sandbox.repo,
            "tool_name": "Bash",
            "tool_input": {"command": ["bash", "-lc", "git commit -m x"]},
        }),
    );
    let hint = sandbox.hint().expect("a hint was written");
    assert_eq!(hint["session_id"], INIT_SESSION);
    assert_eq!(hint["agent"], "codex");
}

#[test]
fn a_read_only_command_leaves_no_hint() {
    let sandbox = Sandbox::new();
    sandbox.record_init(("session_id", "host-claude-2"), "PostToolUse");
    sandbox.run(
        "git-bash-observed",
        json!({
            "hook_event_name": "PostToolUse",
            "session_id": "host-claude-2",
            "cwd": sandbox.repo,
            "tool_name": "Bash",
            "tool_input": {"command": "git status && git log --oneline"},
        }),
    );
    assert!(sandbox.hint().is_none());
}
