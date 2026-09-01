//! Notification hook handler.
//!
//! Captures notable runtime notifications into ContextStream memory.

use anyhow::Result;

use super::common::{extract_cwd, load_config, post_memory_event};
use super::{read_stdin_json, write_stdout_json, HookOutput};

/// Handle the Notification hook.
pub async fn handle() -> Result<()> {
    let input = read_stdin_json()?;
    let cwd = extract_cwd(&input);
    let config = load_config(&cwd);

    if config.is_configured() {
        let title = input
            .get("title")
            .or_else(|| input.get("message"))
            .and_then(|v| v.as_str())
            .map(|s| format!("Notification: {}", s))
            .unwrap_or_else(|| "Notification event".to_string());

        let content = serde_json::json!({
            "notification": input,
            "captured_at": chrono::Utc::now().to_rfc3339(),
        });

        post_memory_event(&config, &title, content, &["hook", "notification"]).await;
    }

    let notification_type = input
        .get("notification_type")
        .and_then(|value| value.as_str())
        .unwrap_or("");

    let output = match notification_type {
        "permission_prompt" => HookOutput::context(
            "A permission prompt was triggered. Prefer least-privilege commands and explain why elevated access is needed before retrying."
                .to_string(),
        ),
        "idle_prompt" => HookOutput::context(
            "An idle prompt was triggered. Before stopping, summarize next steps or persist important state if work is still in progress."
                .to_string(),
        ),
        _ => HookOutput::empty(),
    };

    write_stdout_json(&output)?;
    Ok(())
}
