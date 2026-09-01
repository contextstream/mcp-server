//! SessionEnd (Stop) hook handler.
//!
//! Saves the full transcript when a session ends.
//! This captures the final exchange that the lagging UserPromptSubmit hook misses.

use anyhow::Result;
use serde_json::Value;
use std::collections::HashSet;
use std::path::Path;

use super::{read_stdin_json, write_stdout_json, HookOutput};

/// Handle the Stop/SessionEnd hook.
pub async fn handle() -> Result<()> {
    if std::env::var("CONTEXTSTREAM_SESSION_END_ENABLED")
        .map(|v| v == "false")
        .unwrap_or(false)
    {
        // Silent exit - no output needed for Stop hooks
        write_stdout_json(&HookOutput::empty())?;
        return Ok(());
    }

    let input = read_stdin_json()?;

    let session_id = input
        .get("session_id")
        .and_then(|v| v.as_str())
        .or_else(|| input.get("trajectory_id").and_then(|v| v.as_str()))
        .or_else(|| input.get("execution_id").and_then(|v| v.as_str()))
        .unwrap_or("unknown");
    let transcript_path = input
        .get("transcript_path")
        .and_then(|v| v.as_str())
        .or_else(|| {
            input
                .get("tool_info")
                .and_then(|v| v.get("transcript_path"))
                .and_then(|v| v.as_str())
        })
        .unwrap_or("");
    let reason = input
        .get("reason")
        .and_then(|v| v.as_str())
        .or_else(|| input.get("agent_action_name").and_then(|v| v.as_str()))
        .unwrap_or("user_exit");
    let last_assistant_message = input
        .get("last_assistant_message")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(String::from);
    let cwd = input
        .get("cwd")
        .and_then(|v| v.as_str())
        .or_else(|| {
            input
                .get("tool_info")
                .and_then(|v| v.get("cwd"))
                .and_then(|v| v.as_str())
        })
        .map(String::from)
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .and_then(|p| p.to_str().map(String::from))
        })
        .unwrap_or_default();

    let config = load_config(&cwd);

    if config.api_key.is_empty() {
        write_stdout_json(&HookOutput::empty())?;
        return Ok(());
    }

    // Parse transcript
    let mut stats = if !transcript_path.is_empty() && Path::new(transcript_path).exists() {
        parse_transcript_stats(transcript_path)
    } else {
        TranscriptStats::default()
    };

    if stats.messages.is_empty() {
        if let Some(message) = last_assistant_message {
            stats.messages.push(serde_json::json!({
                "role": "assistant",
                "content": message,
                "timestamp": chrono::Utc::now().to_rfc3339(),
            }));
        }
    }

    // Save transcript if enabled (disabled by default).
    // Priority:
    // 1) Hook payload override (`transcripts_enabled` / `save_transcript`)
    // 2) Environment / .mcp.json policy
    let save_transcript = input
        .get("transcripts_enabled")
        .and_then(|v| v.as_bool())
        .or_else(|| input.get("save_transcript").and_then(|v| v.as_bool()))
        .unwrap_or(config.save_transcript_default);

    if save_transcript && !stats.messages.is_empty() {
        let _ = save_full_transcript(&config, session_id, &stats, reason).await;
    }

    // Save summary event
    let _ = save_summary_event(&config, session_id, &stats, reason).await;

    // Silent exit - no output for Stop hooks
    write_stdout_json(&HookOutput::empty())?;
    Ok(())
}

// ============================================================================
// Config Loading
// ============================================================================

struct ApiConfig {
    api_key: String,
    api_url: String,
    workspace_id: Option<String>,
    project_id: Option<String>,
    save_transcript_default: bool,
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn read_mcp_json_env_bool(path: &Path, key: &str) -> Option<bool> {
    let content = std::fs::read_to_string(path).ok()?;
    let config: Value = serde_json::from_str(&content).ok()?;
    let value = config
        .get("mcpServers")?
        .get("contextstream")?
        .get("env")?
        .get(key)?
        .as_str()?;
    parse_bool(value)
}

fn load_config(cwd: &str) -> ApiConfig {
    let mut api_key = std::env::var("CONTEXTSTREAM_API_KEY").unwrap_or_default();
    let mut api_url = std::env::var("CONTEXTSTREAM_API_URL")
        .unwrap_or_else(|_| "https://api.contextstream.io".to_string());
    let mut workspace_id = std::env::var("CONTEXTSTREAM_WORKSPACE_ID").ok();
    let mut project_id: Option<String> = None;
    let mut save_transcript_default = std::env::var("CONTEXTSTREAM_SESSION_END_SAVE_TRANSCRIPT")
        .ok()
        .and_then(|v| parse_bool(&v))
        .or_else(|| {
            std::env::var("CONTEXTSTREAM_HOOK_TRANSCRIPTS_ENABLED")
                .ok()
                .and_then(|v| parse_bool(&v))
        })
        .or_else(|| {
            std::env::var("CONTEXTSTREAM_TRANSCRIPTS_ENABLED")
                .ok()
                .and_then(|v| parse_bool(&v))
        });

    let mut search_dir = std::path::PathBuf::from(cwd);
    for _ in 0..5 {
        let mcp_path = search_dir.join(".mcp.json");
        if api_key.is_empty() {
            if let Some((key, url)) = read_mcp_json_credentials(&mcp_path) {
                api_key = key;
                if let Some(u) = url {
                    api_url = u;
                }
            }
        }
        if save_transcript_default.is_none() {
            save_transcript_default =
                read_mcp_json_env_bool(&mcp_path, "CONTEXTSTREAM_SESSION_END_SAVE_TRANSCRIPT")
                    .or_else(|| {
                        read_mcp_json_env_bool(&mcp_path, "CONTEXTSTREAM_HOOK_TRANSCRIPTS_ENABLED")
                    })
                    .or_else(|| {
                        read_mcp_json_env_bool(&mcp_path, "CONTEXTSTREAM_TRANSCRIPTS_ENABLED")
                    });
        }
        if workspace_id.is_none() || project_id.is_none() {
            let cs_config = search_dir.join(".contextstream").join("config.json");
            if let Ok(content) = std::fs::read_to_string(&cs_config) {
                if let Ok(cfg) = serde_json::from_str::<Value>(&content) {
                    if workspace_id.is_none() {
                        workspace_id = cfg
                            .get("workspace_id")
                            .and_then(|w| w.as_str())
                            .map(String::from);
                    }
                    if project_id.is_none() {
                        project_id = cfg
                            .get("project_id")
                            .and_then(|p| p.as_str())
                            .map(String::from);
                    }
                }
            }
        }
        if !search_dir.pop() {
            break;
        }
    }

    if api_key.is_empty() {
        if let Some(home) = dirs::home_dir() {
            let home_mcp = home.join(".mcp.json");
            if let Some((key, url)) = read_mcp_json_credentials(&home_mcp) {
                api_key = key;
                if let Some(u) = url {
                    api_url = u;
                }
            }
            if save_transcript_default.is_none() {
                save_transcript_default =
                    read_mcp_json_env_bool(&home_mcp, "CONTEXTSTREAM_SESSION_END_SAVE_TRANSCRIPT")
                        .or_else(|| {
                            read_mcp_json_env_bool(
                                &home_mcp,
                                "CONTEXTSTREAM_HOOK_TRANSCRIPTS_ENABLED",
                            )
                        })
                        .or_else(|| {
                            read_mcp_json_env_bool(&home_mcp, "CONTEXTSTREAM_TRANSCRIPTS_ENABLED")
                        });
            }
        }
    }

    ApiConfig {
        api_key,
        api_url,
        workspace_id,
        project_id,
        save_transcript_default: save_transcript_default.unwrap_or(false),
    }
}

fn read_mcp_json_credentials(path: &Path) -> Option<(String, Option<String>)> {
    let content = std::fs::read_to_string(path).ok()?;
    let config: Value = serde_json::from_str(&content).ok()?;
    let env = config.get("mcpServers")?.get("contextstream")?.get("env")?;
    let key = env
        .get("CONTEXTSTREAM_API_KEY")
        .and_then(|k| k.as_str())?
        .to_string();
    let url = env
        .get("CONTEXTSTREAM_API_URL")
        .and_then(|u| u.as_str())
        .map(String::from);
    Some((key, url))
}

// ============================================================================
// Transcript Parsing
// ============================================================================

#[derive(Default)]
struct TranscriptStats {
    #[allow(dead_code)]
    message_count: usize,
    tool_call_count: usize,
    duration_seconds: i64,
    files_modified: Vec<String>,
    messages: Vec<Value>,
    started_at: String,
}

fn parse_transcript_stats(transcript_path: &str) -> TranscriptStats {
    let content = match std::fs::read_to_string(transcript_path) {
        Ok(c) => c,
        Err(_) => return TranscriptStats::default(),
    };

    let mut message_count = 0usize;
    let mut tool_call_count = 0usize;
    let mut modified_files = HashSet::new();
    let mut messages: Vec<Value> = Vec::new();
    let mut first_timestamp: Option<chrono::DateTime<chrono::FixedOffset>> = None;
    let mut last_timestamp: Option<chrono::DateTime<chrono::FixedOffset>> = None;
    let mut started_at = String::new();

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        let entry: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        let msg_type = entry.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let timestamp = entry
            .get("timestamp")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string();

        // Track timestamps
        if let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&timestamp) {
            if first_timestamp.is_none() || ts < first_timestamp.unwrap() {
                first_timestamp = Some(ts);
                started_at = timestamp.clone();
            }
            if last_timestamp.is_none() || ts > last_timestamp.unwrap() {
                last_timestamp = Some(ts);
            }
        }

        match msg_type {
            "user" => {
                message_count += 1;
                if let Some(content) = extract_text(entry.get("content")) {
                    if !content.is_empty() {
                        messages.push(serde_json::json!({
                            "role": "user",
                            "content": content,
                            "timestamp": timestamp,
                        }));
                    }
                }
            }
            "assistant" => {
                message_count += 1;
                if let Some(content) = extract_text(entry.get("content")) {
                    if !content.is_empty() {
                        messages.push(serde_json::json!({
                            "role": "assistant",
                            "content": content,
                            "timestamp": timestamp,
                        }));
                    }
                }
            }
            "tool_use" => {
                tool_call_count += 1;
                let tool_name = entry
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string();
                let tool_input = entry.get("input").cloned().unwrap_or(serde_json::json!({}));

                // Track file modifications
                if matches!(tool_name.as_str(), "Write" | "Edit" | "NotebookEdit") {
                    if let Some(fp) = tool_input
                        .get("file_path")
                        .or_else(|| tool_input.get("notebook_path"))
                        .and_then(|p| p.as_str())
                    {
                        modified_files.insert(fp.to_string());
                    }
                }

                messages.push(serde_json::json!({
                    "role": "assistant",
                    "content": format!("[Tool: {}]", tool_name),
                    "timestamp": timestamp,
                    "tool_calls": { "name": tool_name, "input": tool_input },
                }));
            }
            "tool_result" => {
                let result_content = entry
                    .get("content")
                    .map(|c| {
                        if let Some(s) = c.as_str() {
                            s[..s.len().min(2000)].to_string()
                        } else {
                            let s = c.to_string();
                            s[..s.len().min(2000)].to_string()
                        }
                    })
                    .unwrap_or_default();
                messages.push(serde_json::json!({
                    "role": "tool",
                    "content": result_content,
                    "timestamp": timestamp,
                    "tool_results": { "name": entry.get("name").and_then(|n| n.as_str()).unwrap_or("") },
                }));
            }
            _ => {}
        }
    }

    let duration_seconds = match (first_timestamp, last_timestamp) {
        (Some(first), Some(last)) => (last - first).num_seconds(),
        _ => 0,
    };

    if started_at.is_empty() {
        started_at = chrono::Utc::now().to_rfc3339();
    }

    TranscriptStats {
        message_count,
        tool_call_count,
        duration_seconds,
        files_modified: modified_files.into_iter().collect(),
        messages,
        started_at,
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
            if let Some(text) = item.get("text").and_then(|entry| entry.as_str()) {
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

// ============================================================================
// API Saves
// ============================================================================

async fn save_full_transcript(
    config: &ApiConfig,
    session_id: &str,
    stats: &TranscriptStats,
    reason: &str,
) -> Result<()> {
    let mut payload = serde_json::json!({
        "session_id": session_id,
        "messages": stats.messages,
        "started_at": stats.started_at,
        "source_type": "session_end",
        "title": format!("Session transcript ({})", reason),
        "metadata": {
            "reason": reason,
            "tool_call_count": stats.tool_call_count,
            "files_modified": &stats.files_modified[..stats.files_modified.len().min(20)],
            "duration_seconds": stats.duration_seconds,
        },
        "tags": ["session_end", reason],
    });

    if let Some(ref ws_id) = config.workspace_id {
        payload
            .as_object_mut()
            .unwrap()
            .insert("workspace_id".to_string(), Value::String(ws_id.clone()));
    }
    if let Some(ref proj_id) = config.project_id {
        payload
            .as_object_mut()
            .unwrap()
            .insert("project_id".to_string(), Value::String(proj_id.clone()));
    }

    let client = super::api_http_client();
    let _ = client
        .post(format!("{}/api/v1/transcripts", config.api_url))
        .header("X-API-Key", &config.api_key)
        .header("Content-Type", "application/json")
        .json(&payload)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await;

    Ok(())
}

async fn save_summary_event(
    config: &ApiConfig,
    session_id: &str,
    stats: &TranscriptStats,
    reason: &str,
) -> Result<()> {
    let summary_content = serde_json::json!({
        "session_id": session_id,
        "reason": reason,
        "stats": {
            "messages": stats.messages.len(),
            "tool_calls": stats.tool_call_count,
            "duration_seconds": stats.duration_seconds,
            "files_modified": stats.files_modified.len(),
        },
        "files_modified": &stats.files_modified[..stats.files_modified.len().min(20)],
        "ended_at": chrono::Utc::now().to_rfc3339(),
    });

    let mut payload = serde_json::json!({
        "event_type": "uncategorized",
        "title": format!("Session Ended: {}", reason),
        "content": summary_content.to_string(),
        "importance": "low",
        "tags": ["session", "end", reason],
        "source_type": "hook",
    });

    if let Some(ref ws_id) = config.workspace_id {
        payload
            .as_object_mut()
            .unwrap()
            .insert("workspace_id".to_string(), Value::String(ws_id.clone()));
    }

    let client = super::api_http_client();
    let _ = client
        .post(format!("{}/api/v1/memory/events", config.api_url))
        .header("X-API-Key", &config.api_key)
        .header("Content-Type", "application/json")
        .json(&payload)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await;

    Ok(())
}
