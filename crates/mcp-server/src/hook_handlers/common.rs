//! Shared helpers for hook handlers.

use chrono::{DateTime, Utc};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Compact human-readable age (e.g. `3h`, `2d`, `6w`, `4mo`) used when
/// rendering captured_at/created_at timestamps in session-start output.
/// Mirrors the server-side watcher so both surfaces agree on shape.
pub fn format_age(captured_at: DateTime<Utc>) -> String {
    let secs = Utc::now()
        .signed_duration_since(captured_at)
        .num_seconds()
        .max(0);
    if secs < 3600 {
        let m = (secs / 60).max(1);
        format!("{}m", m)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else if secs < 86_400 * 14 {
        format!("{}d", secs / 86_400)
    } else if secs < 86_400 * 90 {
        format!("{}w", secs / (86_400 * 7))
    } else {
        format!("{}mo", secs / (86_400 * 30))
    }
}

/// Extract an RFC3339 timestamp from a JSON value by key. Returns None when
/// the field is missing, not a string, or unparseable.
pub fn extract_age_suffix(json: &Value, key: &str) -> String {
    json.get(key)
        .and_then(|v| v.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| format!(" ({} old)", format_age(dt.with_timezone(&Utc))))
        .unwrap_or_default()
}

#[derive(Debug, Clone)]
pub struct ApiConfig {
    pub api_key: String,
    pub api_url: String,
    pub workspace_id: Option<String>,
    pub project_id: Option<String>,
}

impl ApiConfig {
    pub fn is_configured(&self) -> bool {
        !self.api_key.is_empty()
    }
}

/// Extract working directory from a hook payload.
pub fn extract_cwd(input: &Value) -> String {
    input
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| {
            input
                .get("workspace_roots")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .or_else(|| {
            input
                .get("workspaceRoots")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .and_then(|p| p.to_str().map(String::from))
        })
        .unwrap_or_default()
}

/// Load API credentials and workspace/project IDs from env and local config files.
pub fn load_config(cwd: &str) -> ApiConfig {
    let mut api_key = std::env::var("CONTEXTSTREAM_API_KEY").unwrap_or_default();
    let mut api_url = std::env::var("CONTEXTSTREAM_API_URL")
        .unwrap_or_else(|_| "https://api.contextstream.io".to_string());
    let mut workspace_id: Option<String> = std::env::var("CONTEXTSTREAM_WORKSPACE_ID").ok();
    let mut project_id: Option<String> = std::env::var("CONTEXTSTREAM_PROJECT_ID").ok();

    let mut search_dir = PathBuf::from(cwd);
    for _ in 0..8 {
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
        if let Some(home) = home_dir() {
            let home_mcp = home.join(".mcp.json");
            if let Some((key, url)) = read_mcp_json_credentials(&home_mcp) {
                api_key = key;
                if let Some(u) = url {
                    api_url = u;
                }
            }
        }
    }

    if api_key.is_empty() {
        if let Some(home) = home_dir() {
            let creds_path = home.join(".contextstream").join("credentials.json");
            if let Ok(content) = std::fs::read_to_string(&creds_path) {
                if let Ok(creds) = serde_json::from_str::<Value>(&content) {
                    if let Some(key) = creds.get("api_key").and_then(|k| k.as_str()) {
                        api_key = key.to_string();
                    }
                    if let Some(url) = creds.get("api_url").and_then(|u| u.as_str()) {
                        api_url = url.to_string();
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

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn add_workspace_and_project(payload: &mut serde_json::Map<String, Value>, config: &ApiConfig) {
    if let Some(ref ws_id) = config.workspace_id {
        payload.insert("workspace_id".to_string(), Value::String(ws_id.clone()));
    }
    if let Some(ref project_id) = config.project_id {
        payload.insert("project_id".to_string(), Value::String(project_id.clone()));
    }
}

/// Extract an entity ID from common API response shapes.
pub fn extract_id(value: &Value) -> Option<String> {
    value
        .get("id")
        .and_then(|v| v.as_str())
        .or_else(|| {
            value
                .get("data")
                .and_then(|d| d.get("id"))
                .and_then(|v| v.as_str())
        })
        .or_else(|| {
            value
                .get("task")
                .and_then(|d| d.get("id"))
                .and_then(|v| v.as_str())
        })
        .or_else(|| {
            value
                .get("plan")
                .and_then(|d| d.get("id"))
                .and_then(|v| v.as_str())
        })
        .map(String::from)
}

/// Extract an array of items from common API response shapes.
pub fn extract_items(value: &Value) -> Vec<Value> {
    if let Some(items) = value.as_array() {
        return items.clone();
    }

    if let Some(items) = value.get("items").and_then(|v| v.as_array()) {
        return items.clone();
    }

    if let Some(items) = value
        .get("data")
        .and_then(|v| v.get("items"))
        .and_then(|v| v.as_array())
    {
        return items.clone();
    }

    if let Some(items) = value.get("data").and_then(|v| v.as_array()) {
        return items.clone();
    }

    if let Some(items) = value.get("tasks").and_then(|v| v.as_array()) {
        return items.clone();
    }

    Vec::new()
}

/// Send a low-importance memory event. Network/API errors are ignored.
pub async fn post_memory_event(config: &ApiConfig, title: &str, content: Value, tags: &[&str]) {
    if !config.is_configured() {
        return;
    }

    let mut payload = serde_json::Map::new();
    // Hook captures are operational telemetry, not knowledge: the explicit
    // `operation` type (plus source_type=hook below) lets ranking downweight
    // them and the recall/grounding surface exclude them, while list_events
    // keeps them fully queryable.
    payload.insert(
        "event_type".to_string(),
        Value::String("operation".to_string()),
    );
    payload.insert("title".to_string(), Value::String(title.to_string()));
    payload.insert("content".to_string(), Value::String(content.to_string()));
    payload.insert("importance".to_string(), Value::String("low".to_string()));
    payload.insert("source_type".to_string(), Value::String("hook".to_string()));
    payload.insert(
        "tags".to_string(),
        Value::Array(
            tags.iter()
                .map(|t| Value::String((*t).to_string()))
                .collect(),
        ),
    );
    add_workspace_and_project(&mut payload, config);

    let client = super::api_http_client();
    let _ = client
        .post(format!("{}/api/v1/memory/events", config.api_url))
        .header("X-API-Key", &config.api_key)
        .header("Content-Type", "application/json")
        .json(&payload)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await;
}

/// Create a plan. Returns the new plan ID when available.
pub async fn create_plan(config: &ApiConfig, title: &str, description: &str) -> Option<String> {
    if !config.is_configured() {
        return None;
    }

    let mut payload = serde_json::Map::new();
    payload.insert("title".to_string(), Value::String(title.to_string()));
    payload.insert(
        "description".to_string(),
        Value::String(description.to_string()),
    );
    payload.insert(
        "tags".to_string(),
        serde_json::json!(["hook", "subagent_plan"]),
    );
    add_workspace_and_project(&mut payload, config);

    let client = super::api_http_client();
    let response = client
        .post(format!("{}/api/v1/plans", config.api_url))
        .header("X-API-Key", &config.api_key)
        .header("Content-Type", "application/json")
        .json(&payload)
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await
        .ok()?;

    if !response.status().is_success() {
        return None;
    }

    let value: Value = response.json().await.ok()?;
    extract_id(&value)
}

/// Create a task and optionally attach it to a plan.
pub async fn create_task(
    config: &ApiConfig,
    title: &str,
    description: Option<&str>,
    plan_id: Option<&str>,
    status: Option<&str>,
) -> Option<String> {
    if !config.is_configured() {
        return None;
    }

    let mut payload = serde_json::Map::new();
    payload.insert("title".to_string(), Value::String(title.to_string()));
    if let Some(desc) = description {
        payload.insert("description".to_string(), Value::String(desc.to_string()));
    }
    if let Some(plan_id) = plan_id {
        payload.insert("plan_id".to_string(), Value::String(plan_id.to_string()));
    }
    if let Some(status) = status {
        payload.insert("status".to_string(), Value::String(status.to_string()));
    }
    payload.insert("tags".to_string(), serde_json::json!(["hook"]));
    add_workspace_and_project(&mut payload, config);

    let client = super::api_http_client();
    let response = client
        .post(format!("{}/api/v1/tasks", config.api_url))
        .header("X-API-Key", &config.api_key)
        .header("Content-Type", "application/json")
        .json(&payload)
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await
        .ok()?;

    if !response.status().is_success() {
        return None;
    }

    let value: Value = response.json().await.ok()?;
    extract_id(&value)
}

/// Update task status by ID. Returns true if the API call succeeded.
pub async fn update_task_status(
    config: &ApiConfig,
    task_id: &str,
    status: &str,
    title: Option<&str>,
    description: Option<&str>,
) -> bool {
    if !config.is_configured() || task_id.is_empty() {
        return false;
    }

    let mut payload = serde_json::Map::new();
    payload.insert("status".to_string(), Value::String(status.to_string()));
    if let Some(title) = title {
        payload.insert("title".to_string(), Value::String(title.to_string()));
    }
    if let Some(description) = description {
        payload.insert(
            "description".to_string(),
            Value::String(description.to_string()),
        );
    }

    let client = super::api_http_client();
    let response = client
        .put(format!("{}/api/v1/tasks/{}", config.api_url, task_id))
        .header("X-API-Key", &config.api_key)
        .header("Content-Type", "application/json")
        .json(&payload)
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await;

    match response {
        Ok(resp) => resp.status().is_success(),
        Err(_) => false,
    }
}

/// List pending tasks for the active workspace/project.
pub async fn list_pending_tasks(config: &ApiConfig, limit: usize) -> Vec<Value> {
    if !config.is_configured() {
        return Vec::new();
    }

    let mut params = vec![
        "status=pending".to_string(),
        format!("limit={}", limit.max(1)),
    ];
    if let Some(ref ws_id) = config.workspace_id {
        params.push(format!("workspace_id={}", ws_id));
    }
    if let Some(ref project_id) = config.project_id {
        params.push(format!("project_id={}", project_id));
    }

    let url = format!("{}/api/v1/tasks?{}", config.api_url, params.join("&"));

    let client = super::api_http_client();
    let response = match client
        .get(url)
        .header("X-API-Key", &config.api_key)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(_) => return Vec::new(),
    };

    if !response.status().is_success() {
        return Vec::new();
    }

    let value: Value = match response.json().await {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    extract_items(&value)
}
