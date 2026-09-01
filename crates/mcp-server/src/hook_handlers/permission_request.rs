//! PermissionRequest hook handler.
//!
//! Captures permission escalation requests and injects a safety reminder for
//! potentially destructive commands.

use anyhow::Result;

use super::common::{extract_cwd, load_config, post_memory_event};
use super::{read_stdin_json, write_stdout_json, HookOutput};

/// Handle the PermissionRequest hook.
pub async fn handle() -> Result<()> {
    let input = read_stdin_json()?;
    let cwd = extract_cwd(&input);
    let config = load_config(&cwd);

    let command = input
        .get("command")
        .or_else(|| input.get("cmd"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if config.is_configured() {
        let content = serde_json::json!({
            "request": input,
            "captured_at": chrono::Utc::now().to_rfc3339(),
        });

        post_memory_event(
            &config,
            "Permission request",
            content,
            &["hook", "permission_request"],
        )
        .await;
    }

    if is_high_risk_command(command) {
        write_stdout_json(&HookOutput::system_message(
            "High-risk command detected. Confirm scope and prefer least-privilege execution."
                .to_string(),
        ))?;
    } else {
        write_stdout_json(&HookOutput::empty())?;
    }

    Ok(())
}

fn is_high_risk_command(command: &str) -> bool {
    if command.is_empty() {
        return false;
    }

    let lower = command.to_lowercase();
    [
        "rm -rf",
        "git reset --hard",
        "mkfs",
        "dd if=",
        "shutdown",
        "reboot",
    ]
    .iter()
    .any(|pattern| lower.contains(pattern))
}
