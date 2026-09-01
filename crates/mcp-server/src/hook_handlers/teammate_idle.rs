//! TeammateIdle hook handler.
//!
//! Captures teammate idle events and can redirect teammates to pending tasks.

use anyhow::Result;
use serde_json::Value;

use super::common::{extract_cwd, list_pending_tasks, load_config, post_memory_event};
use super::{read_stdin_json, write_stdout_json, HookOutput};

/// Handle the TeammateIdle hook.
pub async fn handle() -> Result<()> {
    let input = read_stdin_json()?;
    let cwd = extract_cwd(&input);
    let config = load_config(&cwd);

    let teammate_name = first_str(&input, &["teammate_name", "teammateName", "agent_name"])
        .unwrap_or("teammate")
        .to_string();
    let team_name = first_str(&input, &["team_name", "teamName"]).unwrap_or("team");

    if !config.is_configured() {
        write_stdout_json(&HookOutput::empty())?;
        return Ok(());
    }

    let pending_tasks = list_pending_tasks(&config, 5).await;

    let summary = serde_json::json!({
        "teammate_name": teammate_name,
        "team_name": team_name,
        "pending_tasks": pending_tasks.iter().take(5).collect::<Vec<_>>(),
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });

    post_memory_event(
        &config,
        "Teammate idle",
        summary,
        &["hook", "teammate", "idle"],
    )
    .await;

    let should_redirect = std::env::var("CONTEXTSTREAM_TEAMMATE_IDLE_REDIRECT")
        .map(|v| v != "false")
        .unwrap_or(true);

    if should_redirect {
        if let Some(task) = pending_tasks.first() {
            let task_title = task
                .get("title")
                .or_else(|| task.get("subject"))
                .or_else(|| task.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("pending task");

            let message = format!(
                "Pending ContextStream task available: {}. Continue and complete this task before idling.",
                task_title
            );

            eprintln!("{}", message);
            std::process::exit(2);
        }
    }

    write_stdout_json(&HookOutput::empty())?;
    Ok(())
}

fn first_str<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| value.get(*k).and_then(|v| v.as_str()))
}
