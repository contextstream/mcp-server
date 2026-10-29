//! Antigravity (`agy`) lifecycle hooks, installed as the `contextstream`
//! entry of `~/.gemini/config/hooks.json`.
//!
//! Antigravity runs hook commands with the hooks file's folder as the working
//! directory, so the session's folder comes from `workspacePaths[0]`, and its
//! session from `conversationId`.
//! - `SessionStart` (`agy-session-start`): records the repository and agent
//!   handle the first message names; outputs `{}`.
//! - `PreInvocation` (`agy-pre-invocation`): runs before each model call. Shows
//!   notices a background poll fetched as a system message
//!   (`{"injectSteps": [{"systemMessage": {"systemMessage": ...}}]}`), which
//!   stays in the conversation, and starts the next poll when one is due.
//! - `PostToolUse` (`agy-post-tool-use`, matcher `*`): reads tool arguments
//!   (`CommandLine`, `TargetFile`, ...) so a scratch-folder session is scoped
//!   to the repository it works in; outputs `{}`.

use anyhow::Result;
use serde_json::json;

use super::common;
use super::mid_run_notices;
use super::read_stdin_json;

fn write_agy_empty() -> Result<()> {
    println!("{{}}");
    Ok(())
}

fn write_agy_system_message(text: String) -> Result<()> {
    let out = json!({
        "injectSteps": [
            {
                "systemMessage": {
                    "systemMessage": text
                }
            }
        ]
    });
    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}

pub async fn handle_session_start() -> Result<()> {
    let input = read_stdin_json()?;
    let cwd = common::extract_cwd(&input);
    mid_run_notices::observe_session_context(&input, &cwd);
    write_agy_empty()
}

pub async fn handle_pre_invocation() -> Result<()> {
    let input = read_stdin_json()?;
    let cwd = common::extract_cwd(&input);
    match mid_run_notices::notice_for_hook(&input, &cwd, "antigravity") {
        Some(notice) => write_agy_system_message(notice),
        None => write_agy_empty(),
    }
}

pub async fn handle_post_tool_use() -> Result<()> {
    let input = read_stdin_json()?;
    let cwd = common::extract_cwd(&input);
    if let Some(args) = input.get("toolCall").and_then(|call| call.get("args")) {
        mid_run_notices::observe_tool_args(&input, &cwd, args);
    }
    write_agy_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn antigravity_payloads_resolve_folder_and_session() {
        let input = json!({
            "conversationId": "550e8400-e29b-41d4-a716-446655440000",
            "workspacePaths": ["/home/dev/work/agent-proof.a1b2"],
            "invocationNum": 3,
        });
        assert_eq!(
            common::extract_cwd(&input),
            "/home/dev/work/agent-proof.a1b2"
        );
        assert_eq!(
            common::host_session_key(&input).as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
    }
}
