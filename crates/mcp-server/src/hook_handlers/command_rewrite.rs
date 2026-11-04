//! The editor hook's side of `contextstream-mcp run`: a shell command that a
//! tool the account has says it takes is handed to `run`, which sends it to the
//! tool and, where the tool does not take it, runs it right here.
//!
//! Only a Claude Code `Bash` call is changed, only when the hints the server
//! keeps (`mcp_client::command_hints`) say a tool takes the command as typed,
//! and only the command: the hook answers with the call's own input, its
//! `command` replaced by
//! `'<this binary>' run --tool <name> -- '<the command as typed>'`.
//! The hook makes no network call and gives no permission decision, so what
//! the editor would have asked about the original command it asks about the
//! changed one, which it shows.

use mcp_client::command_hints::{self, shell_quote};
use serde_json::{json, Value};

/// What the hook says to the editor about the model of the change.
const NOTE: &str = "This command was handed to `contextstream-mcp run`, which sends it to a tool your ContextStream account has for it when that tool takes it, and otherwise runs it here. Its output and exit status are the command's own; the last line says if it ran remotely.";

/// The hook's answer for a `Bash` call whose command a tool takes, or `None`
/// to leave the call as it is.
pub fn rewritten_bash(tool_input: &Value, cwd: &str) -> Option<Value> {
    if command_hints::switched_off(std::env::var(command_hints::ENV_SWITCH).ok().as_deref()) {
        return None;
    }
    let command = tool_input.get("command")?.as_str()?;
    let config = super::common::load_config(cwd);
    if !config.is_configured() {
        return None;
    }
    let cache = command_hints::read(
        &command_hints::local_identity(&config.api_key),
        &config.api_url,
    )?;
    let tool = cache.tool_for(command)?;
    let binary = std::env::current_exe().ok()?;
    wrap(tool_input, &binary.to_string_lossy(), &tool.name, command)
}

/// The hook's answer that replaces `command` in `tool_input` with `run` of it.
fn wrap(tool_input: &Value, binary: &str, tool: &str, command: &str) -> Option<Value> {
    let prefix = format!("{} run ", shell_quote(binary));
    // Already handed to `run`: not handed again.
    if command.trim_start().starts_with(&prefix) {
        return None;
    }
    let mut updated = tool_input.clone();
    updated["command"] = Value::String(format!(
        "{prefix}--tool {} -- {}",
        shell_quote(tool),
        shell_quote(command.trim())
    ));
    Some(json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "updatedInput": updated,
            "additionalContext": NOTE,
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_is_replaced_and_the_rest_of_the_call_is_kept() {
        let input = json!({
            "command": "cargo test -p thing 2>&1 | tail -5",
            "description": "Run the tests",
            "timeout": 120000,
            "run_in_background": false,
        });
        let output = wrap(
            &input,
            "/usr/local/bin/contextstream-mcp",
            "check",
            "cargo test -p thing 2>&1 | tail -5",
        )
        .unwrap();
        let specific = &output["hookSpecificOutput"];
        assert_eq!(specific["hookEventName"], "PreToolUse");
        // No permission decision: the editor decides about the changed command as it would any.
        assert!(specific.get("permissionDecision").is_none());
        let updated = &specific["updatedInput"];
        assert_eq!(
            updated["command"],
            "'/usr/local/bin/contextstream-mcp' run --tool 'check' -- 'cargo test -p thing 2>&1 | tail -5'"
        );
        assert_eq!(updated["description"], "Run the tests");
        assert_eq!(updated["timeout"], 120000);
        assert_eq!(updated["run_in_background"], false);
        assert!(specific["additionalContext"]
            .as_str()
            .unwrap()
            .contains("run"));
    }

    #[test]
    fn quotes_in_the_command_cannot_end_its_quoting() {
        let nasty = "cargo test 'a b' \"c\" ; echo $HOME `id` \\ # x";
        let output = wrap(&json!({"command": nasty}), "/bin/b", "check", nasty).unwrap();
        let line = output["hookSpecificOutput"]["updatedInput"]["command"]
            .as_str()
            .unwrap()
            .to_string();
        // What a POSIX shell would read back is the binary, then run, --tool, check, --, the command.
        assert_eq!(
            line,
            "'/bin/b' run --tool 'check' -- 'cargo test '\\''a b'\\'' \"c\" ; echo $HOME `id` \\ # x'"
        );
    }

    #[test]
    fn a_command_already_handed_to_run_is_left_alone() {
        let wrapped = "'/bin/b' run --tool 'check' -- 'cargo test'";
        assert!(wrap(&json!({"command": wrapped}), "/bin/b", "check", wrapped).is_none());
        assert!(wrap(
            &json!({"command": format!("  {wrapped}")}),
            "/bin/b",
            "check",
            &format!("  {wrapped}")
        )
        .is_none());
        // A different binary's `run` is not ours to skip.
        assert!(wrap(&json!({"command": wrapped}), "/other/b", "check", wrapped).is_some());
    }

    #[test]
    fn only_a_command_given_is_looked_at() {
        assert!(rewritten_bash(&json!({"description": "x"}), "/tmp").is_none());
        assert!(rewritten_bash(&json!({"command": 7}), "/tmp").is_none());
        assert!(rewritten_bash(&Value::Null, "/tmp").is_none());
    }
}
