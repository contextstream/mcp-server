//! Drive the real hook binary for the durable-knowledge routing paths: Claude
//! auto memory, the plan-mode file, handoff files written by Codex
//! `apply_patch`, and the single prompt-time save guidance.
//!
//! Every run gets its own HOME and Claude config directory, no credentials
//! unless a test sets them, and an unroutable API URL, so nothing touches the
//! developer's real configuration or the network.

use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

struct Sandbox {
    home: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(home.path().join(".claude")).unwrap();
        Self { home }
    }

    fn claude_dir(&self) -> std::path::PathBuf {
        self.home.path().join(".claude")
    }

    fn run(&self, hook: &str, input: Value, extra_env: &[(&str, &str)]) -> Option<Value> {
        let mut command = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"));
        command
            .args(["hook", hook, "--contextstream-managed-hook=v1"])
            .env_clear()
            .env("HOME", self.home.path())
            .env("CLAUDE_CONFIG_DIR", self.claude_dir())
            .env("CONTEXTSTREAM_API_URL", "http://127.0.0.1:9")
            .env("CONTEXTSTREAM_REMINDER_ENABLED", "false")
            .current_dir(self.home.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in extra_env {
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
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stdout = stdout.trim();
        if stdout.is_empty() {
            return None;
        }
        Some(serde_json::from_str(stdout).expect("one JSON object on stdout"))
    }

    fn pre_tool_use(&self, tool_name: &str, tool_input: Value, extra: Value) -> Option<Value> {
        let mut input = json!({
            "hook_event_name": "PreToolUse",
            "session_id": "routing-test-session",
            "cwd": self.home.path(),
            "tool_name": tool_name,
            "tool_input": tool_input,
        });
        if let (Some(input), Some(extra)) = (input.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                input.insert(key.clone(), value.clone());
            }
        }
        self.run("pre-tool-use", input, &[])
    }
}

fn decision(output: &Option<Value>) -> Option<&str> {
    output
        .as_ref()?
        .get("hookSpecificOutput")?
        .get("permissionDecision")?
        .as_str()
}

fn agent_text(output: &Option<Value>) -> String {
    let Some(output) = output else {
        return String::new();
    };
    let specific = &output["hookSpecificOutput"];
    [
        specific.get("permissionDecisionReason"),
        specific.get("additionalContext"),
        output.get("additionalContext"),
        output.get("reason"),
    ]
    .into_iter()
    .flatten()
    .filter_map(Value::as_str)
    .collect::<Vec<_>>()
    .join("\n")
}

fn write_transcript(dir: &Path, prompt: &str) -> String {
    let path = dir.join("transcript.jsonl");
    let line = json!({"type": "user", "message": {"role": "user", "content": prompt}});
    std::fs::write(&path, format!("{line}\n")).unwrap();
    path.to_string_lossy().to_string()
}

#[test]
fn claude_auto_memory_writes_are_redirected_to_contextstream() {
    let sandbox = Sandbox::new();
    let memory_file = sandbox
        .claude_dir()
        .join("projects/-Users-dev-repo/memory/commit-directly-to-main.md");
    let output = sandbox.pre_tool_use(
        "Write",
        json!({
            "file_path": memory_file,
            "content": "---\nname: commit-directly-to-main\ndescription: x\nmetadata:\n  type: feedback\n---\nCommit to main.",
        }),
        json!({}),
    );
    assert_eq!(decision(&output), Some("deny"), "{output:?}");
    let text = agent_text(&output);
    assert!(text.contains("Save this preference with"), "{text}");
    assert!(text.contains("action=\"remember\""), "{text}");
    assert!(text.contains("action=\"capture_lesson\""), "{text}");

    let index = sandbox.pre_tool_use(
        "Edit",
        json!({
            "file_path": sandbox.claude_dir().join("projects/-Users-dev-repo/memory/MEMORY.md"),
            "old_string": "a",
            "new_string": "b",
        }),
        json!({}),
    );
    assert_eq!(decision(&index), Some("deny"));
    assert!(agent_text(&index).contains("MEMORY.md is not needed"));
}

#[test]
fn local_memory_escape_hatch_allows_the_write() {
    let sandbox = Sandbox::new();
    let mut input = json!({
        "hook_event_name": "PreToolUse",
        "session_id": "routing-test-session",
        "cwd": sandbox.home.path(),
        "tool_name": "Write",
        "tool_input": {
            "file_path": sandbox.claude_dir().join("projects/p/memory/a.md"),
            "content": "x",
        },
    });
    let output = sandbox.run(
        "pre-tool-use",
        input.take(),
        &[("CONTEXTSTREAM_ALLOW_LOCAL_MEMORY", "true")],
    );
    assert_ne!(decision(&output), Some("deny"), "{output:?}");
}

#[test]
fn plan_mode_file_write_is_allowed_without_a_contradicting_nudge() {
    let sandbox = Sandbox::new();
    let output = sandbox.pre_tool_use(
        "Write",
        json!({
            "file_path": sandbox.claude_dir().join("plans/need-to-look-for-sequential-zebra.md"),
            "content": "# Plan\n",
        }),
        json!({}),
    );
    assert_ne!(decision(&output), Some("deny"));
    assert!(
        !agent_text(&output).contains("Instead of writing a plan to a local file"),
        "{output:?}"
    );
}

#[test]
fn exit_plan_mode_promises_auto_capture_only_when_installed() {
    let sandbox = Sandbox::new();
    let old = sandbox.pre_tool_use("ExitPlanMode", json!({"plan": "# P\n"}), json!({}));
    assert!(agent_text(&old).contains("capture_plan"), "{old:?}");
    assert!(!agent_text(&old).contains("saves it automatically"));

    std::fs::write(
        sandbox.claude_dir().join("settings.json"),
        json!({"hooks": {"PostToolUse": [{
            "matcher": "Edit|Write|ExitPlanMode",
            "hooks": [{"type": "command", "command": "contextstream-mcp hook post-tool-use --contextstream-managed-hook=v1"}]
        }]}})
        .to_string(),
    )
    .unwrap();
    let installed = sandbox.pre_tool_use("ExitPlanMode", json!({"plan": "# P\n"}), json!({}));
    let text = agent_text(&installed);
    assert!(text.contains("saves it automatically"), "{text}");
    assert!(text.contains("do not call capture_plan"), "{text}");
}

#[test]
fn codex_apply_patch_handoff_file_follows_the_handoff_guard() {
    let sandbox = Sandbox::new();
    let patch =
        "*** Begin Patch\n*** Add File: HANDOFF.md\n+# Handoff\n+Next steps\n*** End Patch\n";

    let generic = sandbox.pre_tool_use("apply_patch", json!({ "command": patch }), json!({}));
    assert_ne!(decision(&generic), Some("deny"));
    assert!(
        agent_text(&generic).contains("not a canonical ContextStream handoff"),
        "{generic:?}"
    );

    let transcript = write_transcript(
        sandbox.home.path(),
        "Please prepare a handoff for the next agent.",
    );
    let requested = sandbox.pre_tool_use(
        "apply_patch",
        json!({ "command": patch }),
        json!({ "transcript_path": transcript }),
    );
    assert_eq!(decision(&requested), Some("deny"), "{requested:?}");
    assert!(agent_text(&requested).contains("kind=\"handoff\""));
}

#[test]
fn unconfigured_runbook_write_keeps_the_save_suggestion() {
    let sandbox = Sandbox::new();
    let output = sandbox.pre_tool_use(
        "Write",
        json!({ "file_path": "/repo/docs/runbooks/ops.md", "content": "# Ops\n" }),
        json!({}),
    );
    assert_ne!(decision(&output), Some("deny"));
    // Without credentials nothing can be mirrored, so the agent is told to save.
    assert!(agent_text(&output).contains("create_doc"), "{output:?}");
    assert!(!agent_text(&output).contains("mirrors this"));
}

#[test]
fn prompt_save_guidance_is_emitted_once_per_prompt() {
    let sandbox = Sandbox::new();
    let input = json!({
        "hook_event_name": "UserPromptSubmit",
        "session_id": "routing-test-session",
        "prompt": "Remember that we always commit straight to main."
    });

    // With reminders on, `user-prompt-submit` carries the guidance and the
    // older `on-save-intent` entry stays silent.
    let mut command = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"));
    let mut child = command
        .args(["hook", "on-save-intent", "--contextstream-managed-hook=v1"])
        .env_clear()
        .env("HOME", sandbox.home.path())
        .env("CLAUDE_CONFIG_DIR", sandbox.claude_dir())
        .current_dir(sandbox.home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).trim().is_empty());

    // With reminders off it is the only source, so it still speaks.
    let fallback = sandbox
        .run("on-save-intent", input, &[])
        .expect("save guidance when reminders are disabled");
    let text = agent_text(&Some(fallback));
    assert!(text.contains("action=\"remember\""), "{text}");
}
