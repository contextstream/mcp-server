//! Stop hook handler.
//!
//! Fires whenever Claude finishes responding. Captures a lightweight checkpoint
//! so recent progress is not lost between full SessionEnd events.

use anyhow::Result;

use super::common::{extract_cwd, load_config, post_memory_event};
use super::{read_stdin_json, write_stdout_json, HookOutput};

/// Handle the Stop hook.
pub async fn handle() -> Result<()> {
    if std::env::var("CONTEXTSTREAM_STOP_ENABLED")
        .map(|v| v == "false")
        .unwrap_or(false)
    {
        write_stdout_json(&HookOutput::empty())?;
        return Ok(());
    }

    let input = read_stdin_json()?;
    let cwd = extract_cwd(&input);
    let config = load_config(&cwd);

    if config.is_configured() {
        let session_id = input
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let reason = input
            .get("reason")
            .or_else(|| input.get("stop_reason"))
            .and_then(|v| v.as_str())
            .unwrap_or("response_complete");
        let last_assistant_message = input
            .get("last_assistant_message")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty());

        let content = serde_json::json!({
            "session_id": session_id,
            "reason": reason,
            "hook": "stop",
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "tool_name": input.get("tool_name").and_then(|v| v.as_str()),
            "model": input.get("model").and_then(|v| v.as_str()),
            "stop_hook_active": input.get("stop_hook_active").and_then(|v| v.as_bool()),
            "last_assistant_message": last_assistant_message,
        });

        post_memory_event(
            &config,
            "Stop checkpoint",
            content,
            &["hook", "stop", "checkpoint"],
        )
        .await;
    }

    // Stop hooks are side-effect only.
    write_stdout_json(&HookOutput::empty())?;
    Ok(())
}
