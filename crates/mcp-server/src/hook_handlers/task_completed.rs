//! TaskCompleted hook handler.
//!
//! Captures task completion events into ContextStream tasks and memory events.

use anyhow::Result;
use serde_json::Value;

use super::common::{create_task, extract_cwd, load_config, post_memory_event, update_task_status};
use super::{read_stdin_json, write_stdout_json, HookOutput};

/// Handle the TaskCompleted hook.
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
    .unwrap_or("Completed task")
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

    // Optional gate: block completion if task subject is missing.
    let require_subject = std::env::var("CONTEXTSTREAM_TASK_COMPLETED_REQUIRE_SUBJECT")
        .map(|v| v == "true")
        .unwrap_or(false);
    if require_subject && task_subject.is_empty() {
        eprintln!("TaskCompleted requires a non-empty task subject");
        std::process::exit(2);
    }

    if config.is_configured() {
        let mut updated = false;
        if !task_id.is_empty() {
            updated = update_task_status(
                &config,
                task_id,
                "completed",
                Some(&task_subject),
                task_description.as_deref(),
            )
            .await;
        }

        if !updated {
            let _ = create_task(
                &config,
                &task_subject,
                task_description.as_deref(),
                plan_id.as_deref(),
                Some("completed"),
            )
            .await;
        }

        let content = serde_json::json!({
            "task_id": if task_id.is_empty() { Value::Null } else { Value::String(task_id.to_string()) },
            "title": task_subject,
            "description": task_description,
            "plan_id": plan_id,
            "agent_id": first_str(&input, &["agent_id", "agentId"]),
            "teammate_name": first_str(&input, &["teammate_name", "teammateName"]),
            "team_name": first_str(&input, &["team_name", "teamName"]),
            "completed_at": chrono::Utc::now().to_rfc3339(),
            "source": "task_completed_hook"
        });

        post_memory_event(
            &config,
            "Task completed",
            content,
            &["hook", "task", "completed"],
        )
        .await;

        if looks_like_recovery_task(task_description.as_deref().unwrap_or("")) {
            let lesson = serde_json::json!({
                "task": task_subject,
                "description": task_description,
                "lesson": "Recovered from an execution issue; consider codifying this into tests/guards.",
            });
            post_memory_event(
                &config,
                "Lesson from task completion",
                lesson,
                &["hook", "lesson", "task_completed"],
            )
            .await;
        }
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

fn looks_like_recovery_task(description: &str) -> bool {
    if description.is_empty() {
        return false;
    }
    let lower = description.to_lowercase();
    ["error", "failure", "retry", "recover", "fix", "incident"]
        .iter()
        .any(|keyword| lower.contains(keyword))
}
