//! TaskCreated hook handler.
//!
//! Captures task creation events into ContextStream tasks and memory events.

use anyhow::Result;
use serde_json::Value;

use super::common::{create_task, extract_cwd, load_config, post_memory_event};
use super::{read_stdin_json, write_stdout_json, HookOutput};

/// Handle the TaskCreated hook.
pub async fn handle() -> Result<()> {
    let input = read_stdin_json()?;
    let cwd = extract_cwd(&input);
    let config = load_config(&cwd);

    let task_id = first_str(&input, &["task_id", "taskId"]).unwrap_or("");
    let task_subject = first_str(
        &input,
        &[
            "task_subject",
            "task_title",
            "title",
            "subject",
            "description",
        ],
    )
    .unwrap_or("New task")
    .trim()
    .to_string();
    let task_description = first_str(&input, &["task_description", "details", "content"])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    let plan_id = first_str(&input, &["plan_id", "planId"])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);

    let require_subject = std::env::var("CONTEXTSTREAM_TASK_CREATED_REQUIRE_SUBJECT")
        .map(|v| v == "true")
        .unwrap_or(false);
    if require_subject && task_subject.is_empty() {
        eprintln!("TaskCreated requires a non-empty task subject");
        std::process::exit(2);
    }

    if config.is_configured() {
        let _ = create_task(
            &config,
            &task_subject,
            task_description.as_deref(),
            plan_id.as_deref(),
            Some("pending"),
        )
        .await;

        let content = serde_json::json!({
            "task_id": if task_id.is_empty() { Value::Null } else { Value::String(task_id.to_string()) },
            "title": task_subject,
            "description": task_description,
            "plan_id": plan_id,
            "teammate_name": first_str(&input, &["teammate_name", "teammateName"]),
            "team_name": first_str(&input, &["team_name", "teamName"]),
            "created_at": chrono::Utc::now().to_rfc3339(),
            "source": "task_created_hook"
        });

        post_memory_event(
            &config,
            "Task created",
            content,
            &["hook", "task", "created"],
        )
        .await;
    }

    write_stdout_json(&HookOutput::empty())?;
    Ok(())
}

fn first_str<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| value.get(*k).and_then(|v| v.as_str()))
        .or_else(|| {
            value.get("task").and_then(|task| {
                keys.iter()
                    .find_map(|k| task.get(*k).and_then(|v| v.as_str()))
            })
        })
}
