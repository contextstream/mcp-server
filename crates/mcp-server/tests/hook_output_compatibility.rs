//! Exercise the public hook CLI, including event-name setup and stdout JSON.
//! Codex rejects unknown fields (notably Cursor's `user_message`):
//! https://learn.chatgpt.com/docs/hooks#userpromptsubmit

use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Stdio};

fn run_hook(hook: &str, input: Value) -> Option<Value> {
    let cwd = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
        .args(["hook", hook, "--contextstream-managed-hook=v1"])
        .env_clear()
        // The prompt-format probe needs no API calls or state writes. Save
        // intent remains enabled and exercises actual context output.
        .env("CONTEXTSTREAM_REMINDER_ENABLED", "false")
        .current_dir(cwd.path())
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
    assert!(
        output.status.success(),
        "{hook}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    if stdout.trim().is_empty() {
        return None;
    }
    Some(serde_json::from_str(&stdout).expect("one complete JSON output, without log noise"))
}

fn assert_codex_prompt_context(output: &Value, expected_text: &str) {
    // Validate the public contract instead of merely accepting any valid JSON.
    let allowed_fields = [
        "continue",
        "stopReason",
        "systemMessage",
        "suppressOutput",
        "decision",
        "reason",
        "hookSpecificOutput",
    ];
    for key in output.as_object().unwrap().keys() {
        assert!(
            allowed_fields.contains(&key.as_str()),
            "unsupported field {key}"
        );
    }
    let specific = &output["hookSpecificOutput"];
    assert_eq!(specific["hookEventName"], "UserPromptSubmit");
    assert!(specific["additionalContext"]
        .as_str()
        .unwrap()
        .contains(expected_text));
    assert_eq!(specific.as_object().unwrap().len(), 2);
}

#[test]
fn codex_and_claude_save_prompts_emit_supported_context_json() {
    for transcript_path in [Value::Null, json!("/tmp/imported-claude-session.jsonl")] {
        let input = json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": "synthetic-session",
            "turn_id": "synthetic-codex-turn",
            "transcript_path": transcript_path,
            "prompt": "Please save this decision for future reference."
        });
        let output = run_hook("on-save-intent", input).expect("save guidance");
        assert_codex_prompt_context(&output, "CONTEXTSTREAM DOCUMENT STORAGE");
    }
    let output = run_hook(
        "on-save-intent",
        json!({
            "hook_event_name": "UserPromptSubmit",
            "session_id": "claude-session",
            "prompt": "Prepare a handoff for the next agent."
        }),
    )
    .expect("handoff guidance");
    assert_codex_prompt_context(&output, "handoff");
}

#[test]
fn ordinary_prompt_does_not_trigger_save_guidance() {
    assert!(run_hook(
        "on-save-intent",
        json!({
            "hook_event_name": "UserPromptSubmit",
            "prompt": "Explain how this module works."
        })
    )
    .is_none());
}

#[test]
fn prompt_cli_distinguishes_canonical_and_cursor_events() {
    assert_eq!(
        run_hook(
            "user-prompt-submit",
            json!({
                "hook_event_name": "UserPromptSubmit", "prompt": "Explain this module."
            })
        ),
        Some(json!({}))
    );
    assert_eq!(
        run_hook(
            "user-prompt-submit",
            json!({
                "hook_event_name": "beforeSubmitPrompt", "prompt": "Explain this module."
            })
        ),
        Some(json!({"continue": true}))
    );
}

#[test]
fn legacy_cursor_and_cline_save_hooks_keep_their_formats() {
    let cursor = run_hook(
        "on-save-intent",
        json!({
            "hook_event_name": "beforeSubmitPrompt", "prompt": "Save this decision."
        }),
    )
    .unwrap();
    assert_eq!(cursor["continue"], true);
    assert!(cursor["user_message"]
        .as_str()
        .unwrap()
        .contains("CONTEXTSTREAM DOCUMENT STORAGE"));
    assert!(cursor.get("hookSpecificOutput").is_none());

    let cline = run_hook(
        "on-save-intent",
        json!({
            "hookName": "UserPromptSubmit", "prompt": "Save this decision."
        }),
    )
    .unwrap();
    assert_eq!(cline["cancel"], false);
    assert!(cline["contextModification"]
        .as_str()
        .unwrap()
        .contains("CONTEXTSTREAM DOCUMENT STORAGE"));
}

// ---------------------------------------------------------------------------
// Codex's per-event output contract (2026-11-04).
//
// Codex reads a hook's stdout into one type per event that refuses any field it
// does not list, and shows "Hook failed: hook returned invalid <Event> hook
// JSON output". `PostCompact` used to answer with a top-level `additionalContext`
// and failed that way on every compaction. The lists below are Codex 0.163.0's
// published output schemas (`codex-rs/hooks/schema/generated`), plus the
// refusals in its `engine/output_parser.rs`.
// ---------------------------------------------------------------------------

/// Top-level fields Codex lists for the event's output.
fn codex_top_level(event: &str) -> &'static [&'static str] {
    match event {
        "PostCompact" | "PreCompact" => {
            &["continue", "stopReason", "suppressOutput", "systemMessage"]
        }
        "PermissionRequest" | "SessionStart" | "SubagentStart" => &[
            "continue",
            "hookSpecificOutput",
            "stopReason",
            "suppressOutput",
            "systemMessage",
        ],
        "PostToolUse" | "PreToolUse" | "UserPromptSubmit" => &[
            "continue",
            "decision",
            "hookSpecificOutput",
            "reason",
            "stopReason",
            "suppressOutput",
            "systemMessage",
        ],
        "Stop" | "SubagentStop" => &[
            "continue",
            "decision",
            "reason",
            "stopReason",
            "suppressOutput",
            "systemMessage",
        ],
        other => panic!("no Codex output type for {other}"),
    }
}

/// `Err` with the reason when Codex would fail the hook for this stdout.
fn codex_refuses(event: &str, stdout: &str) -> Result<(), String> {
    if stdout.trim().is_empty() {
        return Ok(());
    }
    let output: Value = serde_json::from_str(stdout).map_err(|e| format!("not JSON: {e}"))?;
    let object = output.as_object().ok_or("not a JSON object")?;
    for key in object.keys() {
        if !codex_top_level(event).contains(&key.as_str()) {
            return Err(format!(
                "hook returned invalid {event} hook JSON output (unknown field `{key}`)"
            ));
        }
    }
    if let Some(specific) = object.get("hookSpecificOutput") {
        let allowed: &[&str] = match event {
            "PreToolUse" => &[
                "hookEventName",
                "additionalContext",
                "permissionDecision",
                "permissionDecisionReason",
                "updatedInput",
            ],
            "PermissionRequest" => &["hookEventName", "decision"],
            _ => &["hookEventName", "additionalContext"],
        };
        for key in specific
            .as_object()
            .ok_or("hookSpecificOutput is not an object")?
            .keys()
        {
            if !allowed.contains(&key.as_str()) {
                return Err(format!(
                    "hook returned invalid {event} hook JSON output (unknown hookSpecificOutput field `{key}`)"
                ));
            }
        }
        if specific["hookEventName"] != event {
            return Err("hookEventName is not the event's".to_string());
        }
        if let Some(decision) = specific.get("permissionDecision") {
            if decision != "deny" {
                return Err(format!(
                    "PreToolUse hook returned unsupported permissionDecision:{decision}"
                ));
            }
        }
        if specific.get("updatedInput").is_some() && specific.get("permissionDecision").is_none() {
            return Err("updatedInput without permissionDecision:allow".to_string());
        }
    }
    let stops = output.get("continue") == Some(&json!(false)) || output.get("stopReason").is_some();
    let suppresses = output.get("suppressOutput").is_some();
    if matches!(event, "PreToolUse" | "PermissionRequest") && (stops || suppresses) {
        return Err(format!(
            "{event} hook returned an unsupported universal field"
        ));
    }
    if event == "PostToolUse" && suppresses {
        return Err("PostToolUse hook returned unsupported suppressOutput".to_string());
    }
    Ok(())
}

/// The input Codex sends for `event`, built from its published input schemas.
fn codex_input(event: &str, cwd: &std::path::Path) -> Value {
    let mut input = json!({
        "hook_event_name": event,
        "session_id": "codex-session-1",
        "transcript_path": null,
        "cwd": cwd.to_str().unwrap(),
        "model": "gpt-test",
    });
    let extra = match event {
        "PostCompact" | "PreCompact" => json!({"turn_id": "turn-1", "trigger": "auto"}),
        "PermissionRequest" | "PreToolUse" => json!({
            "turn_id": "turn-1", "permission_mode": "default", "tool_name": "Bash",
            "tool_input": {"command": "echo hi"}, "tool_use_id": "call-1",
        }),
        "PostToolUse" => json!({
            "turn_id": "turn-1", "permission_mode": "default", "tool_name": "Bash",
            "tool_input": {"command": "echo hi"}, "tool_response": "hi\n", "tool_use_id": "call-1",
        }),
        "SessionStart" => json!({"permission_mode": "default", "source": "compact"}),
        "Stop" => json!({
            "turn_id": "turn-1", "permission_mode": "default",
            "last_assistant_message": "done", "stop_hook_active": false,
        }),
        "SubagentStart" => json!({
            "turn_id": "turn-1", "permission_mode": "default",
            "agent_id": "agent-1", "agent_type": "default",
        }),
        "SubagentStop" => json!({
            "turn_id": "turn-1", "permission_mode": "default", "agent_id": "agent-1",
            "agent_type": "default", "agent_transcript_path": null,
            "last_assistant_message": "done", "stop_hook_active": false,
        }),
        "UserPromptSubmit" => json!({
            "turn_id": "turn-1", "permission_mode": "default", "prompt": "hello",
        }),
        other => panic!("no Codex input for {other}"),
    };
    input
        .as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    input
}

/// Run `hook` with `input` and no way to reach an account or a network.
fn run_hook_offline(hook: &str, input: &Value) -> String {
    let scratch = tempfile::tempdir().unwrap();
    let home = scratch.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
        .args(["hook", hook, "--contextstream-managed-hook=v1"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("CONTEXTSTREAM_API_URL", "http://127.0.0.1:9")
        .env("CONTEXTSTREAM_WATCH", "0")
        .env("CONTEXTSTREAM_REMINDER_ENABLED", "false")
        .current_dir(scratch.path())
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
    assert!(
        output.status.success(),
        "{hook}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

#[test]
fn codex_postcompact_hook_prints_nothing_codex_refuses() {
    let cwd = tempfile::tempdir().unwrap();
    let stdout = run_hook_offline("post-compact", &codex_input("PostCompact", cwd.path()));
    assert_eq!(stdout.trim(), "{}");
    assert!(codex_refuses("PostCompact", &stdout).is_ok());
}

#[test]
fn claude_postcompact_hook_keeps_its_output() {
    // Claude Code's PostCompact input: a trigger and the summary, no turn or model.
    let cwd = tempfile::tempdir().unwrap();
    let stdout = run_hook_offline(
        "post-compact",
        &json!({
            "hook_event_name": "PostCompact", "session_id": "claude-session",
            "transcript_path": "/tmp/t.jsonl", "cwd": cwd.path().to_str().unwrap(),
            "trigger": "manual", "compact_summary": "Summary of the conversation",
        }),
    );
    let output: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(output.as_object().unwrap().len(), 1, "{stdout}");
    assert!(output["additionalContext"]
        .as_str()
        .unwrap()
        .starts_with("[POST-COMPACTION - Context Restored]"));
}

/// Every hook command a ContextStream installer has written to Codex's
/// `hooks.json`, with the event it is registered for.
#[test]
fn every_hook_an_installer_wrote_for_codex_answers_in_a_shape_codex_accepts() {
    let hooks = [
        ("PermissionRequest", "permission-request"),
        ("PostCompact", "post-compact"),
        ("PostToolUse", "post-tool-use"),
        ("PostToolUse", "git-bash-observed"),
        ("PreCompact", "pre-compact"),
        ("PreToolUse", "pre-tool-use"),
        ("PreToolUse", "mid-run-notices"),
        ("PreToolUse", "package-bouncer"),
        ("SessionStart", "session-start"),
        ("Stop", "stop"),
        ("SubagentStart", "subagent-start"),
        ("SubagentStop", "subagent-stop"),
        ("UserPromptSubmit", "user-prompt-submit"),
        ("UserPromptSubmit", "on-save-intent"),
    ];
    let cwd = tempfile::tempdir().unwrap();
    let mut refused = Vec::new();
    for (event, hook) in hooks {
        let stdout = run_hook_offline(hook, &codex_input(event, cwd.path()));
        if let Err(why) = codex_refuses(event, &stdout) {
            refused.push(format!("{event} / {hook}: {why}: {}", stdout.trim()));
        }
    }
    assert!(refused.is_empty(), "{}", refused.join("\n"));
}
