//! SubagentStop hook handler.
//!
//! Captures subagent outcomes. For Plan agents, extracts and persists plans/tasks.

use anyhow::Result;
use serde_json::Value;
use std::collections::HashSet;
use std::path::Path;

use super::common::{create_plan, create_task, extract_cwd, load_config, post_memory_event};
use super::{read_stdin_json, write_stdout_json, HookOutput};

const MAX_PLAN_DESCRIPTION_LEN: usize = 12_000;

/// Handle the SubagentStop hook.
pub async fn handle() -> Result<()> {
    let input = read_stdin_json()?;
    let cwd = extract_cwd(&input);
    let config = load_config(&cwd);

    if !config.is_configured() {
        write_stdout_json(&HookOutput::empty())?;
        return Ok(());
    }

    let agent_type = input
        .get("agent_type")
        .or_else(|| input.get("subagent_type"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let agent_id = input
        .get("agent_id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let transcript_path = input
        .get("agent_transcript_path")
        .or_else(|| input.get("transcript_path"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let transcript = if !transcript_path.is_empty() && Path::new(transcript_path).exists() {
        parse_transcript(transcript_path)
    } else {
        ParsedTranscript::default()
    };

    let summary_text = if transcript.assistant_messages.is_empty() {
        extract_summary_from_input(&input)
            .or_else(|| {
                input
                    .get("last_assistant_message")
                    .and_then(|value| value.as_str())
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(String::from)
            })
            .unwrap_or_else(|| "(No assistant output found in subagent transcript.)".to_string())
    } else {
        transcript.assistant_messages.join("\n\n")
    };

    if agent_type.eq_ignore_ascii_case("plan") {
        handle_plan_agent(&config, agent_id, &summary_text, &transcript, &input).await;
    } else {
        let content = serde_json::json!({
            "agent_type": agent_type,
            "agent_id": agent_id,
            "summary": truncate(&summary_text, 4000),
            "assistant_message_count": transcript.assistant_messages.len(),
            "tool_call_count": transcript.tool_call_count,
            "timestamp": chrono::Utc::now().to_rfc3339(),
        });

        post_memory_event(
            &config,
            &format!("Subagent finished: {}", agent_type),
            content,
            &["hook", "subagent_stop", agent_type],
        )
        .await;
    }

    // Clean up active subagent state (tracked by SubagentStart)
    super::subagent_state::remove_active_subagent(&cwd);

    write_stdout_json(&HookOutput::empty())?;
    Ok(())
}

async fn handle_plan_agent(
    config: &super::common::ApiConfig,
    agent_id: &str,
    summary_text: &str,
    transcript: &ParsedTranscript,
    input: &Value,
) {
    let plan_title = derive_plan_title(summary_text, agent_id);
    let plan_description = truncate(summary_text, MAX_PLAN_DESCRIPTION_LEN);
    let extracted_tasks = merge_tasks(
        extract_plan_tasks(summary_text),
        extract_tasks_from_input(input),
    );

    let supplied_plan_id = first_str(input, &["plan_id", "planId"])
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(String::from);

    let plan_id = if supplied_plan_id.is_some() {
        supplied_plan_id.clone()
    } else {
        create_plan(config, &plan_title, &plan_description).await
    };

    let mut created_task_count = 0usize;
    if let Some(ref plan_id) = plan_id {
        for task in extracted_tasks.iter().take(20) {
            if create_task(config, task, None, Some(plan_id), Some("pending"))
                .await
                .is_some()
            {
                created_task_count += 1;
            }
        }
    }

    let content = serde_json::json!({
        "agent_type": "Plan",
        "agent_id": agent_id,
        "plan_title": plan_title,
        "plan_id": plan_id,
        "supplied_plan_id": supplied_plan_id,
        "extracted_task_count": extracted_tasks.len(),
        "created_task_count": created_task_count,
        "assistant_message_count": transcript.assistant_messages.len(),
        "tool_call_count": transcript.tool_call_count,
        "summary_preview": truncate(summary_text, 2000),
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });

    post_memory_event(
        config,
        "Plan subagent captured",
        content,
        &["hook", "subagent_stop", "plan"],
    )
    .await;
}

#[derive(Default)]
struct ParsedTranscript {
    assistant_messages: Vec<String>,
    tool_call_count: usize,
}

fn parse_transcript(path: &str) -> ParsedTranscript {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return ParsedTranscript::default(),
    };

    let mut assistant_messages = Vec::new();
    let mut tool_call_count = 0usize;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let entry: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let msg_type = entry.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if msg_type == "tool_use" {
            tool_call_count += 1;
        }

        let is_assistant = msg_type == "assistant"
            || entry
                .get("role")
                .and_then(|v| v.as_str())
                .map(|r| r == "assistant")
                .unwrap_or(false);

        if is_assistant {
            if let Some(text) = extract_text(entry.get("content")) {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    assistant_messages.push(trimmed.to_string());
                }
            }
        }
    }

    ParsedTranscript {
        assistant_messages,
        tool_call_count,
    }
}

fn extract_text(content: Option<&Value>) -> Option<String> {
    let value = content?;

    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }

    if let Some(array) = value.as_array() {
        let mut parts = Vec::new();
        for item in array {
            if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                parts.push(text.to_string());
            } else if let Some(text) = item.as_str() {
                parts.push(text.to_string());
            }
        }
        if !parts.is_empty() {
            return Some(parts.join("\n"));
        }
    }

    if let Some(object) = value.as_object() {
        if let Some(text) = object.get("text").and_then(|entry| entry.as_str()) {
            return Some(text.to_string());
        }
    }

    None
}

fn derive_plan_title(summary: &str, agent_id: &str) -> String {
    for line in summary.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            let title = line.trim_start_matches('#').trim();
            if !title.is_empty() {
                return truncate(title, 120);
            }
        }
    }

    for line in summary.lines() {
        let line = line.trim();
        if !line.is_empty() {
            return truncate(line, 120);
        }
    }

    format!("Plan generated by {}", agent_id)
}

fn extract_plan_tasks(summary: &str) -> Vec<String> {
    let mut tasks = Vec::new();
    let mut seen = HashSet::new();

    for line in summary.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let normalized = normalize_task_line(line);
        if normalized.is_empty() {
            continue;
        }

        let key = normalized.to_lowercase();
        if seen.insert(key) {
            tasks.push(normalized);
        }
    }

    tasks
}

fn extract_tasks_from_input(input: &Value) -> Vec<String> {
    let mut tasks = Vec::new();

    if let Some(array) = input.get("tasks").and_then(|value| value.as_array()) {
        for item in array {
            if let Some(text) = item.as_str() {
                let normalized = normalize_task_line(text);
                if !normalized.is_empty() {
                    tasks.push(normalized);
                }
                continue;
            }

            if let Some(text) = item
                .get("title")
                .or_else(|| item.get("task"))
                .or_else(|| item.get("name"))
                .and_then(|value| value.as_str())
            {
                let normalized = truncate(text.trim(), 220);
                if !normalized.is_empty() {
                    tasks.push(normalized);
                }
            }
        }
    }

    if let Some(array) = input
        .get("plan")
        .and_then(|plan| plan.get("steps"))
        .and_then(|value| value.as_array())
    {
        for item in array {
            if let Some(text) = item.as_str() {
                let normalized = normalize_task_line(text);
                if !normalized.is_empty() {
                    tasks.push(normalized);
                }
            }
        }
    }

    tasks
}

fn merge_tasks(primary: Vec<String>, secondary: Vec<String>) -> Vec<String> {
    let mut merged = Vec::new();
    let mut seen = HashSet::new();

    for task in primary.into_iter().chain(secondary) {
        let key = task.to_lowercase();
        if seen.insert(key) {
            merged.push(task);
        }
    }

    merged
}

fn extract_summary_from_input(input: &Value) -> Option<String> {
    first_str(
        input,
        &[
            "summary",
            "result",
            "output",
            "final_response",
            "assistant_output",
        ],
    )
    .map(str::trim)
    .filter(|value| !value.is_empty())
    .map(String::from)
}

fn first_str<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(|entry| entry.as_str()))
}

fn normalize_task_line(line: &str) -> String {
    let trimmed = line.trim();

    let prefixes = ["- [ ]", "- [x]", "-", "*", "•"];
    for prefix in prefixes {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return truncate(rest.trim(), 220);
        }
    }

    if let Some((num, rest)) = trimmed.split_once('.') {
        if num.chars().all(|c| c.is_ascii_digit()) {
            return truncate(rest.trim(), 220);
        }
    }

    String::new()
}

fn truncate(value: &str, max_len: usize) -> String {
    if value.len() <= max_len {
        return value.to_string();
    }
    format!("{}...", &value[..max_len])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_summary_from_input_payload() {
        let input = serde_json::json!({
            "summary": "Plan summary from payload"
        });

        assert_eq!(
            extract_summary_from_input(&input).as_deref(),
            Some("Plan summary from payload")
        );
    }

    #[test]
    fn merges_tasks_without_duplicates() {
        let merged = merge_tasks(
            vec!["Set up hooks".to_string(), "Add tests".to_string()],
            vec!["add tests".to_string(), "Update docs".to_string()],
        );

        assert_eq!(merged.len(), 3);
        assert!(merged.iter().any(|task| task == "Set up hooks"));
        assert!(merged.iter().any(|task| task == "Add tests"));
        assert!(merged.iter().any(|task| task == "Update docs"));
    }
}
