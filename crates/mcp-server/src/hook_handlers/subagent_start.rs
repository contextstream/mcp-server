//! SubagentStart hook handler.
//!
//! Injects ContextStream context when Claude Code spawns a subagent (Explore, Plan, etc.).
//! Tailors context based on agent type:
//!   - Explore: lightweight (lessons + search protocol)
//!   - Plan: full (lessons + decisions + active plans + search protocol)
//!   - general-purpose/other: medium (lessons + search protocol)
//!
//! PERFORMANCE: Explore <100ms, Plan <200ms via /api/v1/context/hook (Redis-cached).

use anyhow::Result;
use serde_json::Value;
use std::path::{Path, PathBuf};

use super::{write_stdout_json, HookOutput};

/// Search protocol base (tool guidance only — no sensitive text).
const SEARCH_PROTOCOL_BASE: &str = r#"[CONTEXTSTREAM SEARCH]
When searching code, prefer ContextStream search tools if the project is indexed:
- mcp__contextstream__search(mode="auto", query="...") for smart search
- mcp__contextstream__search(mode="semantic", query="...") for concept search
- mcp__contextstream__search(mode="keyword", query="...") for exact matches
- mcp__contextstream__search(mode="pattern", query="...") for regex
- mcp__contextstream__graph(action="related", query="...") for dependency analysis
Fall back to Glob/Grep/Read only if ContextStream search is unavailable or returns no results."#;

/// Fallback base when API is unreachable.
const FALLBACK_CONTEXT_BASE: &str = r#"[CONTEXTSTREAM] Use mcp__contextstream__search(mode="auto", query="...") for codebase search when available. Call mcp__contextstream__context(user_message="...") for task-specific context; when it returns [GROUNDING], read those prior-work hits before searching code (one-shot bundle: mcp__contextstream__session(action="ground", user_message="...")). If ContextStream surfaces [LESSONS_WARNING], treat those lessons as active instructions for the task. If this task is a continuation or references prior work, treat fresh, relevant, sufficient [GROUNDING] as the completed retrieval step and do not immediately duplicate it with mcp__contextstream__session(action="recall"). Use recall only when [GROUNDING] is absent, thin, stale, off-topic, or the user explicitly requests broader or session-specific history; if recall is thin, fall through to mcp__contextstream__memory(action="search_transcripts", query="...") for full-text across transcripts or mcp__contextstream__memory(action="list_events", event_type="session_snapshot") for turning-point bookmarks. FRESHNESS: decisions, transcript continuity, snapshots, active plans, and tasks are time-sensitive; refresh stale hits with session(action="ground"), memory(action="decisions", workspace_id="<current_workspace_id>", project_id="<current_project_id>") when ids are available, or memory(action="search_transcripts") before planning or implementation. Treat Gemini/LLM-derived [INSIGHT] items as advisory unless backed by a current captured decision/event/doc. KNOWLEDGE-FIRST (not just code): when the question is "how/why/what pattern/did we decide X?", the answer usually lives in docs/decisions/lessons/preferences/plans/tasks/skills. Pick by type: memory(action="decisions", workspace_id="<current_workspace_id>", project_id="<current_project_id>") when ids are available; memory(action="list_docs"|"get_doc"|"list_nodes"|"list_tasks"|"list_todos"|"search"); session(action="get_lessons"|"list_plans"|"get_plan"); skill(action="list"|"run"). Check the right knowledge surface BEFORE code search. If hook context is unavailable and the work is risky or unfamiliar, call mcp__contextstream__session(action="get_lessons") before improvising."#;

/// Build search protocol with decoded attribution.
fn search_protocol() -> String {
    format!(
        "{}\n\n{}",
        SEARCH_PROTOCOL_BASE,
        super::protected::subagent_attribution()
    )
}

/// Build fallback context with decoded attribution.
fn fallback_context() -> String {
    format!(
        "{} {}",
        FALLBACK_CONTEXT_BASE,
        super::protected::fallback_attribution()
    )
}

struct ApiConfig {
    api_key: String,
    api_url: String,
    workspace_id: Option<String>,
    project_id: Option<String>,
    session_id: Option<String>,
}

fn extract_session_id(input: &Value) -> Option<String> {
    input
        .get("session_id")
        .and_then(|v| v.as_str())
        .or_else(|| input.get("sessionId").and_then(|v| v.as_str()))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// Handle the SubagentStart hook.
///
/// Input JSON from Claude Code:
/// ```json
/// {
///   "session_id": "abc123",
///   "cwd": "/path/to/project",
///   "hook_event_name": "SubagentStart",
///   "agent_id": "agent-abc123",
///   "agent_type": "Explore"
/// }
/// ```
pub async fn handle() -> Result<()> {
    // Check disable flag
    if std::env::var("CONTEXTSTREAM_SUBAGENT_CONTEXT_ENABLED")
        .map(|v| v == "false")
        .unwrap_or(false)
    {
        write_stdout_json(&HookOutput::empty())?;
        return Ok(());
    }

    // Read stdin JSON
    let input: Value =
        serde_json::from_reader(std::io::stdin().lock()).unwrap_or_else(|_| serde_json::json!({}));

    let agent_type = input
        .get("agent_type")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let agent_id = input.get("agent_id").and_then(|v| v.as_str());
    let cwd = input.get("cwd").and_then(|v| v.as_str()).unwrap_or(".");

    // Track active subagent for PreToolUse coordination
    super::subagent_state::write_active_subagent(cwd, agent_type, agent_id);

    let mut config = load_config(cwd);
    config.session_id = extract_session_id(&input);

    if config.api_key.is_empty() {
        write_stdout_json(&HookOutput::context(fallback_context()))?;
        return Ok(());
    }

    let output = if agent_type.eq_ignore_ascii_case("plan") {
        build_plan_context(&config, agent_type).await
    } else if agent_type.eq_ignore_ascii_case("explore") {
        build_explore_context(&config, agent_type).await
    } else if agent_type.eq_ignore_ascii_case("general-purpose")
        || agent_type.eq_ignore_ascii_case("general_purpose")
        || agent_type.eq_ignore_ascii_case("general")
    {
        build_default_context(&config, agent_type).await
    } else if agent_type.eq_ignore_ascii_case("custom") {
        build_custom_context(&config, agent_type).await
    } else {
        build_default_context(&config, agent_type).await
    };

    write_stdout_json(&HookOutput::context(output))?;
    Ok(())
}

// ============================================================================
// Context Builders (per agent type)
// ============================================================================

/// Explore agents: search-first context with strong enforcement.
/// Drives Explore to use ContextStream search before reading files.
async fn build_explore_context(config: &ApiConfig, agent_type: &str) -> String {
    let mut parts: Vec<String> = Vec::new();

    // Fetch fast cached context (lessons + preferences)
    if let Some(hook_ctx) = fetch_hook_context(config, agent_type).await {
        parts.push(hook_ctx);
    }

    // Search-first enforcement (stronger than generic search protocol)
    parts.push(EXPLORE_SEARCH_FIRST.to_string());

    // Also include base search protocol for tool reference
    parts.push(search_protocol());

    if parts.is_empty() {
        return fallback_context();
    }

    parts.join("\n\n")
}

/// Strong search-first protocol for Explore agents.
const EXPLORE_SEARCH_FIRST: &str = r#"[CRITICAL: SEARCH-FIRST PROTOCOL]
You MUST call mcp__contextstream__search(mode="auto", query="...") BEFORE reading any source file.
Search returns relevant code snippets with file paths and line numbers — this is 10x faster than reading whole files.

RULES:
1. Start with search to find relevant files and code sections
2. Only Read a file AFTER search identifies it as relevant
3. When Reading, request only the needed line range (use offset/limit)
4. Do NOT use Glob with broad patterns — search replaces file discovery
5. If search returns good results, do NOT redundantly Read the same content
6. Do NOT iterate through files one-by-one (file-by-file) for repository discovery

This project has a fresh ContextStream index. Search will find what you need."#;

/// Plan agents: full context (lessons + decisions + active plans + search protocol).
/// Plan agents need architectural context to make informed design decisions.
const PLAN_SEARCH_FIRST: &str = r#"[PLAN MODE: SEARCH-FIRST]
Plan mode does NOT justify file-by-file repository scans.
Do NOT launch Explore subagents for broad code discovery.

WORKFLOW:
1. Start with mcp__contextstream__search(mode="auto", query="...")
2. Use mcp__contextstream__search(mode="keyword", query="...", include_content=true) to inspect snippets before opening files
3. Read only the small set of files and line ranges that search identified
4. If search returns results with a stale-index advisory, use those results for existing indexed code and refresh/retry before concluding a new symbol is absent
5. Fall back to local discovery tools only if ContextStream search returns 0 results after the refresh/retry path.
6. Before choosing an implementation path from prior work, inspect decision/transcript/plan age and refresh stale hits with mcp__contextstream__session(action="ground", user_message="...") or mcp__contextstream__memory(action="decisions", query="...", workspace_id="<current_workspace_id>", project_id="<current_project_id>") when ids are available."#;

async fn build_plan_context(config: &ApiConfig, agent_type: &str) -> String {
    let mut parts: Vec<String> = Vec::new();

    // Fetch fast cached context (lessons + preferences)
    if let Some(hook_ctx) = fetch_hook_context(config, agent_type).await {
        parts.push(hook_ctx);
    }

    // Fetch full context with decisions and plans
    if let Some(full_ctx) = fetch_plan_context(config).await {
        parts.push(full_ctx);
    }

    // Keep planning search-first to prevent token-heavy file-by-file scans.
    parts.push(PLAN_SEARCH_FIRST.to_string());

    // Always include search protocol
    parts.push(search_protocol());

    // Add plan-specific guidance
    parts.push(
        "[CONTEXTSTREAM PLAN] When creating plans, save them to ContextStream: \
         mcp__contextstream__session(action=\"capture_plan\", title=\"...\", steps=[...]). \
         Create tasks: mcp__contextstream__memory(action=\"create_task\", title=\"...\", plan_id=\"...\"). \
         Check existing plans above to avoid duplicating work."
            .to_string(),
    );

    if parts.is_empty() {
        return fallback_context();
    }

    parts.join("\n\n")
}

/// Default context for general-purpose and custom agents.
async fn build_default_context(config: &ApiConfig, agent_type: &str) -> String {
    let mut parts: Vec<String> = Vec::new();

    // Fetch fast cached context (lessons + preferences)
    if let Some(hook_ctx) = fetch_hook_context(config, agent_type).await {
        parts.push(hook_ctx);
    }

    // Always include search protocol
    parts.push(search_protocol());

    if parts.is_empty() {
        return fallback_context();
    }

    parts.join("\n\n")
}

/// Custom agents: keep context concise but include preferences + lessons.
async fn build_custom_context(config: &ApiConfig, agent_type: &str) -> String {
    let mut parts: Vec<String> = Vec::new();

    if let Some(hook_ctx) = fetch_hook_context(config, agent_type).await {
        parts.push(hook_ctx);
    }

    parts.push(search_protocol());
    parts.push(
        "[CONTEXTSTREAM CUSTOM AGENT] Prefer ContextStream memory, search, and graph tools before local-only exploration."
            .to_string(),
    );

    if parts.is_empty() {
        return fallback_context();
    }

    parts.join("\n\n")
}

// ============================================================================
// API Fetching
// ============================================================================

/// Fetch fast cached context from /api/v1/context/hook (~20-50ms).
/// Returns lessons, high-importance preferences, and core rules.
async fn fetch_hook_context(config: &ApiConfig, agent_type: &str) -> Option<String> {
    let url = format!("{}/api/v1/context/hook", config.api_url);

    let mut body = serde_json::Map::new();
    if let Some(ref ws_id) = config.workspace_id {
        body.insert("workspace_id".to_string(), Value::String(ws_id.clone()));
    }
    if let Some(ref proj_id) = config.project_id {
        body.insert("project_id".to_string(), Value::String(proj_id.clone()));
    }
    if let Some(ref session_id) = config.session_id {
        body.insert("session_id".to_string(), Value::String(session_id.clone()));
    }
    // Include agent_type for future API-side tailoring
    body.insert(
        "hook_type".to_string(),
        Value::String("subagent_start".to_string()),
    );
    body.insert("editor".to_string(), Value::String("claude".to_string()));
    body.insert(
        "source".to_string(),
        Value::String("subagent_start".to_string()),
    );
    body.insert(
        "agent_type".to_string(),
        Value::String(agent_type.to_string()),
    );

    let client = super::api_http_client();
    let response = client
        .post(&url)
        .header("X-API-Key", &config.api_key)
        .header("Content-Type", "application/json")
        .json(&body)
        .timeout(std::time::Duration::from_secs(2))
        .send()
        .await
        .ok()?;

    if !response.status().is_success() {
        return None;
    }

    let data: Value = response.json().await.ok()?;
    data.get("data")
        .and_then(|d| d.get("context"))
        .and_then(|c| c.as_str())
        .map(String::from)
}

/// Fetch full context for Plan agents: decisions + active plans.
/// Uses /api/v1/context with include flags.
async fn fetch_plan_context(config: &ApiConfig) -> Option<String> {
    let mut url = format!(
        "{}/api/v1/context?include_decisions=true&include_plans=true&limit=3",
        config.api_url
    );

    if let Some(ref ws_id) = config.workspace_id {
        url.push_str(&format!("&workspace_id={}", ws_id));
    }

    let client = super::api_http_client();
    let response = client
        .get(&url)
        .header("X-API-Key", &config.api_key)
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .ok()?;

    if !response.status().is_success() {
        return None;
    }

    let data: Value = response.json().await.ok()?;
    let mut sections = Vec::new();

    // Active plans
    if let Some(plans) = data
        .get("active_plans")
        .or_else(|| data.get("plans"))
        .and_then(|p| p.as_array())
    {
        if !plans.is_empty() {
            let mut text = String::from("## Active Plans\n");
            for plan in plans.iter().take(3) {
                let title = plan
                    .get("title")
                    .and_then(|t| t.as_str())
                    .unwrap_or("Untitled");
                let status = plan
                    .get("status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("active");
                text.push_str(&format!("- {} ({})\n", title, status));
            }
            sections.push(text);
        }
    }

    // Recent decisions
    if let Some(decisions) = data
        .get("recent_decisions")
        .or_else(|| data.get("decisions"))
        .and_then(|d| d.as_array())
    {
        if !decisions.is_empty() {
            let mut text = String::from("## Recent Decisions\n");
            for decision in decisions.iter().take(3) {
                let title = decision
                    .get("title")
                    .and_then(|t| t.as_str())
                    .unwrap_or("Untitled");
                let content = decision
                    .get("content")
                    .and_then(|c| c.as_str())
                    .unwrap_or("");
                if content.is_empty() {
                    text.push_str(&format!("- **{}**\n", title));
                } else {
                    // Truncate long decision content for context efficiency
                    let truncated = if content.len() > 200 {
                        format!("{}...", &content[..200])
                    } else {
                        content.to_string()
                    };
                    text.push_str(&format!("- **{}**: {}\n", title, truncated));
                }
            }
            sections.push(text);
        }
    }

    if sections.is_empty() {
        return None;
    }

    Some(sections.join("\n"))
}

// ============================================================================
// Config Loading (same pattern as user_prompt_submit.rs)
// ============================================================================

fn load_config(cwd: &str) -> ApiConfig {
    let mut api_key = std::env::var("CONTEXTSTREAM_API_KEY").unwrap_or_default();
    let mut api_url = std::env::var("CONTEXTSTREAM_API_URL")
        .unwrap_or_else(|_| "https://api.contextstream.io".to_string());
    let mut workspace_id: Option<String> = std::env::var("CONTEXTSTREAM_WORKSPACE_ID").ok();
    let mut project_id: Option<String> = std::env::var("CONTEXTSTREAM_PROJECT_ID").ok();

    let mut search_dir = PathBuf::from(cwd);
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

    // Check ~/.contextstream/credentials.json
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

    // Check home .mcp.json
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

    ApiConfig {
        api_key,
        api_url,
        workspace_id,
        project_id,
        session_id: None,
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

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_search_protocol_contains_key_tools() {
        let proto = search_protocol();
        assert!(proto.contains("mcp__contextstream__search"));
        assert!(proto.contains("mcp__contextstream__graph"));
        assert!(proto.contains("mode=\"auto\""));
        assert!(proto.contains("mode=\"semantic\""));
    }

    #[test]
    fn test_fallback_context_is_useful() {
        let fb = fallback_context();
        assert!(fb.contains("search"));
        assert!(fb.contains("context"));
        assert!(fb.contains("[LESSONS_WARNING]"));
        assert!(fb.contains("get_lessons"));
        assert!(fb.contains("do not immediately duplicate"));
        assert!(fb.contains("absent, thin, stale, off-topic"));
        assert!(!fb.contains("[GROUNDING] if present, then"));
    }

    #[test]
    fn test_explore_search_first_blocks_file_by_file_discovery() {
        assert!(EXPLORE_SEARCH_FIRST.contains("file-by-file"));
        assert!(EXPLORE_SEARCH_FIRST.contains("one-by-one"));
    }

    #[test]
    fn test_plan_search_first_discourages_explore_subagents() {
        assert!(PLAN_SEARCH_FIRST.contains("Do NOT launch Explore subagents"));
        assert!(PLAN_SEARCH_FIRST.contains("file-by-file"));
        assert!(PLAN_SEARCH_FIRST.contains("mcp__contextstream__search(mode=\"auto\""));
    }

    #[test]
    fn test_load_config_from_env() {
        let _guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Set env vars for test
        std::env::set_var("CONTEXTSTREAM_API_KEY", "test-key-123");
        std::env::set_var("CONTEXTSTREAM_API_URL", "https://test.api.com");
        std::env::set_var("CONTEXTSTREAM_WORKSPACE_ID", "ws-123");

        let config = load_config("/tmp");

        assert_eq!(config.api_key, "test-key-123");
        assert_eq!(config.api_url, "https://test.api.com");
        assert_eq!(config.workspace_id.as_deref(), Some("ws-123"));

        // Clean up
        std::env::remove_var("CONTEXTSTREAM_API_KEY");
        std::env::remove_var("CONTEXTSTREAM_API_URL");
        std::env::remove_var("CONTEXTSTREAM_WORKSPACE_ID");
    }

    #[test]
    fn test_disabled_via_env_var() {
        // This tests the env var check logic (not the full async handler)
        let _guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var("CONTEXTSTREAM_SUBAGENT_CONTEXT_ENABLED", "false");
        let disabled = std::env::var("CONTEXTSTREAM_SUBAGENT_CONTEXT_ENABLED")
            .map(|v| v == "false")
            .unwrap_or(false);
        assert!(disabled);
        std::env::remove_var("CONTEXTSTREAM_SUBAGENT_CONTEXT_ENABLED");
    }
}
