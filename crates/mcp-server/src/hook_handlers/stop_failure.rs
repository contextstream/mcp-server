//! StopFailure hook handler.
//!
//! Records Claude turn-ending API failures for later diagnosis.

use anyhow::Result;

use super::common::{extract_cwd, load_config, post_memory_event};
use super::{read_stdin_json, write_stdout_json, HookOutput};

/// Handle the StopFailure hook.
pub async fn handle() -> Result<()> {
    let input = read_stdin_json()?;
    let cwd = extract_cwd(&input);
    let config = load_config(&cwd);

    if config.is_configured() {
        let error = input
            .get("error")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown");

        let content = serde_json::json!({
            "error": error,
            "error_details": input.get("error_details").cloned(),
            "last_assistant_message": input.get("last_assistant_message").cloned(),
            "captured_at": chrono::Utc::now().to_rfc3339(),
        });

        post_memory_event(
            &config,
            &format!("Stop failure: {}", error),
            content,
            &["hook", "stop_failure", error],
        )
        .await;
    }

    write_stdout_json(&HookOutput::empty())?;
    Ok(())
}
