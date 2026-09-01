//! PostCompact hook handler.
//!
//! Runs AFTER conversation context is compacted. Fetches a compact restored session
//! payload from ContextStream and injects the essential recent state.

use anyhow::Result;
use serde_json::Value;
use std::path::Path;

use super::{read_stdin_json, write_stdout_json, HookOutput};

/// Handle the PostCompact hook.
pub async fn handle() -> Result<()> {
    if std::env::var("CONTEXTSTREAM_POSTCOMPACT_ENABLED")
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
        .unwrap_or("");
    let trigger = input
        .get("trigger")
        .and_then(|v| v.as_str())
        .unwrap_or("compact");
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

    let config = load_config(&cwd);

    let restored = if !session_id.is_empty() && !config.api_key.is_empty() {
        fetch_restore_payload(&config, session_id, trigger).await
    } else {
        None
    };

    let context = match restored {
        Some(payload) if payload.restored => format!(
            "[POST-COMPACTION - Context Restored]\n\n\
             {}\n\n\
             ContextStream restored the essential recent session state from {}. Call `mcp__contextstream__context(user_message=\"resuming after compaction\")` only if you need broader context beyond this compact restore.",
            payload.summary,
            payload.source.as_deref().unwrap_or("saved state"),
        ),
        _ => "[POST-COMPACTION - Context Restored]\n\nNo saved state found. Starting fresh.\n\nCall `mcp__contextstream__context(user_message=\"resuming after compaction\")` if you need a broader context refresh.".to_string(),
    };

    write_stdout_json(&HookOutput::context(context))?;

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
}

fn load_config(cwd: &str) -> ApiConfig {
    let mut api_key = std::env::var("CONTEXTSTREAM_API_KEY").unwrap_or_default();
    let mut api_url = std::env::var("CONTEXTSTREAM_API_URL")
        .unwrap_or_else(|_| "https://api.contextstream.io".to_string());
    let mut workspace_id = std::env::var("CONTEXTSTREAM_WORKSPACE_ID").ok();
    let mut project_id = std::env::var("CONTEXTSTREAM_PROJECT_ID").ok();

    let mut search_dir = std::path::PathBuf::from(cwd);
    for _ in 0..5 {
        if api_key.is_empty() {
            let mcp_path = search_dir.join(".mcp.json");
            if let Some((key, url)) = read_mcp_json_credentials(&mcp_path) {
                api_key = key;
                if let Some(u) = url {
                    api_url = u;
                }
            }
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
        }
    }

    ApiConfig {
        api_key,
        api_url,
        workspace_id,
        project_id,
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
// Restore Fetching
// ============================================================================

#[derive(Default, serde::Deserialize)]
struct RestorePayload {
    restored: bool,
    source: Option<String>,
    summary: String,
}

async fn fetch_restore_payload(
    config: &ApiConfig,
    session_id: &str,
    trigger: &str,
) -> Option<RestorePayload> {
    let mut payload = serde_json::json!({
        "session_id": session_id,
        "trigger": trigger,
        "include_durable_context": true,
    });
    if let Some(ref ws_id) = config.workspace_id {
        payload["workspace_id"] = Value::String(ws_id.clone());
    }
    if let Some(ref project_id) = config.project_id {
        payload["project_id"] = Value::String(project_id.clone());
    }

    let client = super::api_http_client();
    let response = client
        .post(format!("{}/api/v1/session/restore", config.api_url))
        .header("X-API-Key", &config.api_key)
        .header("Content-Type", "application/json")
        .json(&payload)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .ok()?;

    if !response.status().is_success() {
        return None;
    }

    let raw: Value = response.json().await.ok()?;
    let data = raw.get("data").cloned().unwrap_or(raw);
    serde_json::from_value(data).ok()
}
