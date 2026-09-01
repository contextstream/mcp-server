//! PreCompact hook handler.
//!
//! Saves session state before context compaction occurs.
//! This preserves conversation context that would otherwise be lost.
//!
//! Snapshot-first save strategy:
//! 1. Save a compact structured `session_snapshot` (primary restore artifact)
//! 2. Save full transcript as archival secondary support (optional)

use anyhow::Result;
use serde_json::Value;
use std::collections::HashSet;
use std::path::Path;

use super::{read_stdin_json, write_stdout_json, HookOutput};

/// Handle the PreCompact hook.
pub async fn handle() -> Result<()> {
    // Check if disabled
    if std::env::var("CONTEXTSTREAM_PRECOMPACT_ENABLED")
        .map(|v| v == "false")
        .unwrap_or(false)
    {
        write_stdout_json(&HookOutput::empty())?;
        return Ok(());
    }

    let input = read_stdin_json()?;

    let session_id = input
        .get("session_id")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    let trigger = input
        .get("trigger")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    let transcript_path = input
        .get("transcript_path")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let cwd = input
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .and_then(|p| p.to_str().map(String::from))
        })
        .unwrap_or_default();

    // Load config (API key, workspace ID) from env + local config files
    let config = load_config(&cwd);

    // Parse transcript for context
    let transcript_data = if !transcript_path.is_empty() && Path::new(transcript_path).exists() {
        parse_transcript(transcript_path)
    } else {
        TranscriptData::default()
    };

    // Auto-save if enabled and we have an API key (enabled by default).
    // Priority:
    // 1) Hook payload override (`transcripts_enabled` / `auto_save`)
    // 2) Environment / .mcp.json policy
    let auto_save = input
        .get("transcripts_enabled")
        .and_then(|v| v.as_bool())
        .or_else(|| input.get("auto_save").and_then(|v| v.as_bool()))
        .unwrap_or(config.auto_save_default);

    let mut save_status = String::new();
    if auto_save && !config.api_key.is_empty() {
        let snapshot_result = save_snapshot(&config, session_id, &transcript_data, trigger).await;
        let transcript_result =
            save_full_transcript(&config, session_id, &transcript_data, trigger).await;

        if snapshot_result.success {
            if transcript_result.success {
                save_status = format!(
                    "\n[ContextStream: {} | {}]",
                    snapshot_result.message, transcript_result.message
                );
            } else {
                save_status = format!(
                    "\n[ContextStream: {} | Transcript archival failed: {}]",
                    snapshot_result.message, transcript_result.message
                );
            }
        } else if transcript_result.success {
            save_status = format!(
                "\n[ContextStream: Transcript archived but snapshot save failed: {}]",
                snapshot_result.message
            );
        } else {
            save_status = format!(
                "\n[ContextStream: Auto-save failed - snapshot: {}; transcript: {}]",
                snapshot_result.message, transcript_result.message
            );
        }
    }

    let _ = save_status;

    // Claude Code's PreCompact hook schema does not accept hook-specific output.
    // Keep the snapshot/transcript side effects, but emit no JSON payload so the
    // hook validates cleanly across supported editors.
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
    auto_save_default: bool,
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

/// Load API config from environment variables and local config files.
/// Walks up directories (like TypeScript) to find .mcp.json and .contextstream/config.json.
fn load_config(cwd: &str) -> ApiConfig {
    let mut api_key = std::env::var("CONTEXTSTREAM_API_KEY").unwrap_or_default();
    let mut api_url = std::env::var("CONTEXTSTREAM_API_URL")
        .unwrap_or_else(|_| "https://api.contextstream.io".to_string());
    let mut workspace_id = std::env::var("CONTEXTSTREAM_WORKSPACE_ID").ok();
    let mut project_id = std::env::var("CONTEXTSTREAM_PROJECT_ID").ok();
    let mut auto_save_default = std::env::var("CONTEXTSTREAM_PRECOMPACT_AUTO_SAVE")
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

    // Walk up directories to find config files
    let mut search_dir = std::path::PathBuf::from(cwd);
    for _ in 0..5 {
        let mcp_path = search_dir.join(".mcp.json");
        // Load API key from .mcp.json
        if api_key.is_empty() {
            if let Some((key, url)) = read_mcp_json_credentials(&mcp_path) {
                api_key = key;
                if let Some(u) = url {
                    api_url = u;
                }
            }
        }
        if auto_save_default.is_none() {
            auto_save_default =
                read_mcp_json_env_bool(&mcp_path, "CONTEXTSTREAM_PRECOMPACT_AUTO_SAVE")
                    .or_else(|| {
                        read_mcp_json_env_bool(&mcp_path, "CONTEXTSTREAM_HOOK_TRANSCRIPTS_ENABLED")
                    })
                    .or_else(|| {
                        read_mcp_json_env_bool(&mcp_path, "CONTEXTSTREAM_TRANSCRIPTS_ENABLED")
                    });
        }

        // Load workspace/project IDs from .contextstream/config.json
        if workspace_id.is_none() || project_id.is_none() {
            let cs_config = search_dir.join(".contextstream").join("config.json");
            if let Some((ws_id, pid)) = read_contextstream_ids(&cs_config) {
                if workspace_id.is_none() {
                    workspace_id = ws_id;
                }
                if project_id.is_none() {
                    project_id = pid;
                }
            }
        }

        if !search_dir.pop() {
            break;
        }
    }

    // Also check home directory for .mcp.json
    if api_key.is_empty() {
        if let Some(home) = dirs::home_dir() {
            let home_mcp = home.join(".mcp.json");
            if let Some((key, url)) = read_mcp_json_credentials(&home_mcp) {
                api_key = key;
                if let Some(u) = url {
                    api_url = u;
                }
            }
            if auto_save_default.is_none() {
                auto_save_default =
                    read_mcp_json_env_bool(&home_mcp, "CONTEXTSTREAM_PRECOMPACT_AUTO_SAVE")
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

    // Also check saved credentials
    if api_key.is_empty() {
        if let Some(home) = dirs::home_dir() {
            let creds_path = home.join(".contextstream").join("credentials.json");
            if let Ok(content) = std::fs::read_to_string(&creds_path) {
                if let Ok(creds) = serde_json::from_str::<Value>(&content) {
                    if let Some(key) = creds.get("api_key").and_then(|k| k.as_str()) {
                        api_key = key.to_string();
                    }
                }
            }
        }
    }

    ApiConfig {
        api_key,
        api_url,
        workspace_id,
        project_id,
        auto_save_default: auto_save_default.unwrap_or(true),
    }
}

/// Read API credentials from a .mcp.json file.
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

/// Read workspace/project IDs from a .contextstream/config.json file.
fn read_contextstream_ids(path: &Path) -> Option<(Option<String>, Option<String>)> {
    let content = std::fs::read_to_string(path).ok()?;
    let config: Value = serde_json::from_str(&content).ok()?;
    let workspace_id = config
        .get("workspace_id")
        .and_then(|w| w.as_str())
        .map(String::from);
    let project_id = config
        .get("project_id")
        .and_then(|p| p.as_str())
        .map(String::from);

    (workspace_id.is_some() || project_id.is_some()).then_some((workspace_id, project_id))
}

// ============================================================================
// Transcript Parsing
// ============================================================================

#[derive(Default)]
struct TranscriptData {
    active_files: Vec<String>,
    tool_call_count: usize,
    #[allow(dead_code)]
    message_count: usize,
    last_tools: Vec<String>,
    messages: Vec<Value>,
    started_at: String,
}

/// Parse a JSONL transcript file to extract session data.
fn parse_transcript(transcript_path: &str) -> TranscriptData {
    let content = match std::fs::read_to_string(transcript_path) {
        Ok(c) => c,
        Err(_) => return TranscriptData::default(),
    };

    let mut active_files = HashSet::new();
    let mut tool_calls: Vec<String> = Vec::new();
    let mut messages: Vec<Value> = Vec::new();
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

        // Track first timestamp as started_at
        if started_at.is_empty() && !timestamp.is_empty() {
            started_at = timestamp.clone();
        }

        match msg_type {
            "tool_use" => {
                let tool_name = entry
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string();
                let tool_input = entry.get("input").cloned().unwrap_or(serde_json::json!({}));

                // Extract file paths from common tools
                match tool_name.as_str() {
                    "Read" | "Write" | "Edit" | "NotebookEdit" => {
                        if let Some(path) = tool_input
                            .get("file_path")
                            .or_else(|| tool_input.get("notebook_path"))
                            .and_then(|p| p.as_str())
                        {
                            active_files.insert(path.to_string());
                        }
                    }
                    "Glob" => {
                        if let Some(pattern) = tool_input.get("pattern").and_then(|p| p.as_str()) {
                            active_files.insert(format!("[glob:{}]", pattern));
                        }
                    }
                    _ => {}
                }

                tool_calls.push(tool_name.clone());

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
            "user" => {
                if let Some(content) = entry.get("content").and_then(|c| c.as_str()) {
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
                if let Some(content) = entry.get("content").and_then(|c| c.as_str()) {
                    if !content.is_empty() {
                        messages.push(serde_json::json!({
                            "role": "assistant",
                            "content": content,
                            "timestamp": timestamp,
                        }));
                    }
                }
            }
            _ => {
                // Also check role field for messages without type
                let role = entry.get("role").and_then(|r| r.as_str()).unwrap_or("");
                if role == "user" || role == "assistant" {
                    if let Some(content) = entry.get("content").and_then(|c| c.as_str()) {
                        if !content.is_empty() {
                            messages.push(serde_json::json!({
                                "role": role,
                                "content": content,
                                "timestamp": timestamp,
                            }));
                        }
                    }
                }
            }
        }
    }

    if started_at.is_empty() {
        started_at = chrono::Utc::now().to_rfc3339();
    }

    let mut sorted_files: Vec<String> = active_files.into_iter().collect();
    sorted_files.sort();
    // Keep last 20 files
    if sorted_files.len() > 20 {
        sorted_files = sorted_files.split_off(sorted_files.len() - 20);
    }

    let last_tools: Vec<String> = tool_calls
        .iter()
        .rev()
        .take(10)
        .cloned()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    TranscriptData {
        active_files: sorted_files,
        tool_call_count: tool_calls.len(),
        message_count: messages.len(),
        last_tools,
        messages,
        started_at,
    }
}

// ============================================================================
// API Saves
// ============================================================================

struct SaveResult {
    success: bool,
    message: String,
}

/// Save full transcript to the /transcripts API (primary).
async fn save_full_transcript(
    config: &ApiConfig,
    session_id: &str,
    data: &TranscriptData,
    trigger: &str,
) -> SaveResult {
    if config.api_key.is_empty() {
        return SaveResult {
            success: false,
            message: "No API key configured".to_string(),
        };
    }

    if data.messages.is_empty() {
        return SaveResult {
            success: false,
            message: "No messages to save".to_string(),
        };
    }

    let mut payload = serde_json::json!({
        "session_id": session_id,
        "messages": data.messages,
        "started_at": data.started_at,
        "source_type": "pre_compact",
        "title": format!("Pre-compaction save ({})", trigger),
        "metadata": {
            "trigger": trigger,
            "active_files": data.active_files,
            "tool_call_count": data.tool_call_count,
        },
        "tags": ["pre_compaction", trigger],
    });

    if let Some(ref ws_id) = config.workspace_id {
        payload
            .as_object_mut()
            .unwrap()
            .insert("workspace_id".to_string(), Value::String(ws_id.clone()));
    }
    if let Some(ref project_id) = config.project_id {
        payload
            .as_object_mut()
            .unwrap()
            .insert("project_id".to_string(), Value::String(project_id.clone()));
    }

    let client = super::api_http_client();
    let result = client
        .post(format!("{}/api/v1/transcripts", config.api_url))
        .header("X-API-Key", &config.api_key)
        .header("Content-Type", "application/json")
        .json(&payload)
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await;

    match result {
        Ok(r) if r.status().is_success() => SaveResult {
            success: true,
            message: format!("Transcript saved ({} messages)", data.messages.len()),
        },
        Ok(r) => SaveResult {
            success: false,
            message: format!("API error: {}", r.status()),
        },
        Err(e) => SaveResult {
            success: false,
            message: e.to_string(),
        },
    }
}

/// Save session snapshot to /memory/events (fallback).
async fn save_snapshot(
    config: &ApiConfig,
    session_id: &str,
    data: &TranscriptData,
    trigger: &str,
) -> SaveResult {
    if config.api_key.is_empty() {
        return SaveResult {
            success: false,
            message: "No API key configured".to_string(),
        };
    }

    let captured_at = chrono::Utc::now().to_rfc3339();
    let recent_requests = recent_user_requests(data);
    let assistant_summary = last_assistant_summary(data);
    let summary = if !assistant_summary.is_empty() {
        assistant_summary.clone()
    } else {
        recent_requests
            .last()
            .map(|request| format!("Working on: {}", request))
            .unwrap_or_else(|| format!("Recent session snapshot captured ({})", trigger))
    };
    let snapshot_content = serde_json::json!({
        "version": "v1",
        "session_id": session_id,
        "trigger": trigger,
        "captured_at": captured_at,
        "summary": summary,
        "active_files": data.active_files,
        "recent_user_requests": recent_requests,
        "last_assistant_summary": assistant_summary,
        "tool_call_count": data.tool_call_count,
        "message_count": data.message_count,
        "last_tools": data.last_tools,
        "started_at": data.started_at,
        "auto_captured": true,
    });

    let mut payload = serde_json::json!({
        "event_type": "session_snapshot",
        "title": format!("Auto Pre-compaction Snapshot ({})", trigger),
        "content": snapshot_content.to_string(),
        "metadata": {
            "session_id": session_id,
            "trigger": trigger,
            "captured_at": captured_at,
            "auto_captured": true,
            "source_hook": "pre_compact",
            "summary": summary,
            "project_id": config.project_id.as_deref(),
            "active_files": data.active_files,
            "last_tools": data.last_tools,
            "tool_call_count": data.tool_call_count,
            "message_count": data.message_count
        },
        "importance": "high",
        "tags": ["session_snapshot", "pre_compaction", "auto_captured"],
        "source_type": "hook",
    });

    if let Some(ref ws_id) = config.workspace_id {
        payload
            .as_object_mut()
            .unwrap()
            .insert("workspace_id".to_string(), Value::String(ws_id.clone()));
    }
    if let Some(ref project_id) = config.project_id {
        payload
            .as_object_mut()
            .unwrap()
            .insert("project_id".to_string(), Value::String(project_id.clone()));
    }

    let client = super::api_http_client();
    let result = client
        .post(format!("{}/api/v1/memory/events", config.api_url))
        .header("X-API-Key", &config.api_key)
        .header("Content-Type", "application/json")
        .json(&payload)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await;

    match result {
        Ok(r) if r.status().is_success() => SaveResult {
            success: true,
            message: format!(
                "Snapshot saved ({} files, {} recent requests)",
                data.active_files.len(),
                recent_user_requests(data).len()
            ),
        },
        Ok(r) => SaveResult {
            success: false,
            message: format!("API error: {}", r.status()),
        },
        Err(e) => SaveResult {
            success: false,
            message: e.to_string(),
        },
    }
}

fn recent_user_requests(data: &TranscriptData) -> Vec<String> {
    data.messages
        .iter()
        .rev()
        .filter(|msg| msg.get("role").and_then(|r| r.as_str()) == Some("user"))
        .filter_map(|msg| msg.get("content").and_then(|c| c.as_str()))
        .take(3)
        .map(|content| truncate_text(content, 160))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

fn last_assistant_summary(data: &TranscriptData) -> String {
    data.messages
        .iter()
        .rev()
        .find(|msg| {
            msg.get("role").and_then(|r| r.as_str()) == Some("assistant")
                && !msg
                    .get("content")
                    .and_then(|c| c.as_str())
                    .unwrap_or_default()
                    .starts_with("[Tool:")
        })
        .and_then(|msg| msg.get("content").and_then(|c| c.as_str()))
        .map(|content| truncate_text(content, 400))
        .unwrap_or_default()
}

fn truncate_text(value: &str, max_chars: usize) -> String {
    let mut out = value.trim().chars().take(max_chars).collect::<String>();
    if value.chars().count() > max_chars {
        out.push_str("...");
    }
    out
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_transcript_empty() {
        let data = parse_transcript("/nonexistent/path");
        assert_eq!(data.active_files.len(), 0);
        assert_eq!(data.tool_call_count, 0);
        assert_eq!(data.message_count, 0);
    }

    #[test]
    fn test_parse_transcript_with_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");
        let content = r#"{"type":"user","content":"Hello","timestamp":"2026-01-01T00:00:00Z"}
{"type":"tool_use","name":"Read","input":{"file_path":"/home/user/foo.rs"},"timestamp":"2026-01-01T00:00:01Z"}
{"type":"tool_result","name":"Read","content":"file content here","timestamp":"2026-01-01T00:00:02Z"}
{"type":"tool_use","name":"Edit","input":{"file_path":"/home/user/bar.rs"},"timestamp":"2026-01-01T00:00:03Z"}
{"type":"assistant","content":"Done editing","timestamp":"2026-01-01T00:00:04Z"}
"#;
        std::fs::write(&path, content).unwrap();

        let data = parse_transcript(path.to_str().unwrap());

        assert_eq!(data.tool_call_count, 2);
        assert!(data.active_files.contains(&"/home/user/bar.rs".to_string()));
        assert!(data.active_files.contains(&"/home/user/foo.rs".to_string()));
        assert_eq!(data.started_at, "2026-01-01T00:00:00Z");
        assert!(data.message_count >= 4); // user + 2 tool_use + tool_result + assistant
        assert_eq!(data.last_tools, vec!["Read", "Edit"]);
    }

    #[test]
    fn test_parse_transcript_with_glob() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("transcript.jsonl");
        let content = r#"{"type":"tool_use","name":"Glob","input":{"pattern":"**/*.rs"},"timestamp":"2026-01-01T00:00:00Z"}
"#;
        std::fs::write(&path, content).unwrap();

        let data = parse_transcript(path.to_str().unwrap());
        assert!(data.active_files.contains(&"[glob:**/*.rs]".to_string()));
    }

    #[test]
    fn test_read_mcp_json_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".mcp.json");
        let content = r#"{
            "mcpServers": {
                "contextstream": {
                    "command": "contextstream-mcp",
                    "env": {
                        "CONTEXTSTREAM_API_KEY": "test-key-123",
                        "CONTEXTSTREAM_API_URL": "https://test.example.com"
                    }
                }
            }
        }"#;
        std::fs::write(&path, content).unwrap();

        let result = read_mcp_json_credentials(&path);
        assert!(result.is_some());
        let (key, url) = result.unwrap();
        assert_eq!(key, "test-key-123");
        assert_eq!(url, Some("https://test.example.com".to_string()));
    }

    #[test]
    fn test_read_contextstream_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.json");
        let content = r#"{
            "workspace_id": "550e8400-e29b-41d4-a716-446655440000",
            "project_id": "660e8400-e29b-41d4-a716-446655440000"
        }"#;
        std::fs::write(&path, content).unwrap();

        let result = read_contextstream_ids(&path).unwrap();
        assert_eq!(
            result.0.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
        assert_eq!(
            result.1.as_deref(),
            Some("660e8400-e29b-41d4-a716-446655440000")
        );
    }

    #[test]
    fn test_read_contextstream_ids_missing_file() {
        let result = read_contextstream_ids(Path::new("/nonexistent/config.json"));
        assert!(result.is_none());
    }

    #[test]
    fn test_transcript_data_default() {
        let data = TranscriptData::default();
        assert!(data.active_files.is_empty());
        assert_eq!(data.tool_call_count, 0);
        assert_eq!(data.message_count, 0);
        assert!(data.last_tools.is_empty());
        assert!(data.messages.is_empty());
    }

    #[test]
    fn test_recent_user_requests_prefers_latest_three_in_original_order() {
        let data = TranscriptData {
            messages: vec![
                serde_json::json!({"role":"user","content":"first"}),
                serde_json::json!({"role":"assistant","content":"done"}),
                serde_json::json!({"role":"user","content":"second"}),
                serde_json::json!({"role":"user","content":"third"}),
                serde_json::json!({"role":"user","content":"fourth"}),
            ],
            ..Default::default()
        };

        assert_eq!(
            recent_user_requests(&data),
            vec![
                "second".to_string(),
                "third".to_string(),
                "fourth".to_string()
            ]
        );
    }

    #[test]
    fn test_last_assistant_summary_ignores_tool_markers() {
        let data = TranscriptData {
            messages: vec![
                serde_json::json!({"role":"assistant","content":"[Tool: Read]"}),
                serde_json::json!({"role":"assistant","content":"Implemented the migration guard and updated tests."}),
            ],
            ..Default::default()
        };

        assert_eq!(
            last_assistant_summary(&data),
            "Implemented the migration guard and updated tests.".to_string()
        );
    }

    #[test]
    fn test_truncate_text_appends_ellipsis_when_needed() {
        let value = "a".repeat(10);
        assert_eq!(truncate_text(&value, 5), "aaaaa...".to_string());
        assert_eq!(truncate_text("short", 10), "short".to_string());
    }
}
