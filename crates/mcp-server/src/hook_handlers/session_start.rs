//! SessionStart hook handler.
//!
//! Injects initial context when a Claude Code session begins.
//! Fetches lessons, decisions, plans, and preferences from ContextStream.

use anyhow::Result;
use mcp_types::{build_harness_teaching, HarnessId, HarnessTeachingDelivery};
use serde_json::Value;
use std::path::Path;

use super::prompt_state;
use super::{read_stdin_json, write_stdout_json, HookOutput};

/// Handle the SessionStart hook.
pub async fn handle() -> Result<()> {
    if std::env::var("CONTEXTSTREAM_SESSION_INIT_ENABLED")
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
        .unwrap_or("startup");

    // Read cwd from input (not just current_dir)
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

    // Require `init(...)` before other MCP operations at the beginning of this
    // session. PreToolUse clears this once init is called.
    prompt_state::cleanup_stale(360);
    prompt_state::mark_init_required(&cwd);

    // Load config from env + .mcp.json + .contextstream/config.json
    let config = load_config(&cwd);

    // Check index status for the current project
    let index_info = check_index_status(&cwd);

    // Try to fetch compact restore state and startup context from API in
    // parallel so SessionStart can stay under the hook timeout budget.
    let context_fut = async {
        if config.api_key.is_empty() {
            (None, None)
        } else {
            tokio::join!(
                async {
                    if session_id.is_empty() {
                        None
                    } else {
                        fetch_restore_context(&config, session_id, trigger).await
                    }
                },
                fetch_initial_context(&config)
            )
        }
    };

    // Reconcile unflushed local edits (P6 content freshness): drain
    // `dirty-files.json` concurrently with the context fetch so it adds no
    // latency to the session-start budget. Best-effort + bounded + non-billed.
    let ((restored_context, context), _) = tokio::join!(
        context_fut,
        super::dirty_drain::drain_best_effort(std::time::Duration::from_secs(8)),
    );

    let mut output_text = String::from("\u{2b21} ContextStream \u{2014} Smart Context & Memory\n");
    let harness_id = if super::input_is_cursor(&input) {
        HarnessId::Cursor
    } else {
        HarnessId::ClaudeCode
    };
    let teaching = build_harness_teaching(Some(harness_id), HarnessTeachingDelivery::HookReminder);
    output_text.push('\n');
    output_text.push_str(&teaching.rendered_guidance);
    output_text.push('\n');

    // Add index status info
    match index_info {
        IndexInfo::Indexed => {
            output_text.push_str(
                "\n\u{2713} Project index is ready \u{2014} search works now and freshens automatically as you edit.\n",
            );
        }
        IndexInfo::Stale => {
            output_text.push_str("\n\u{2713} Search works now and freshens automatically as you edit. (Full index is older than 7 days; an auto-refresh starts on the first init() call.)\n");
        }
        IndexInfo::Indexing => {
            output_text.push_str("\n\u{2139} Project indexing is in progress; keyword search works now and semantic search comes online after the first committed generation. Use ContextStream search (not Explore/Grep) and retry as it fills in; do not fall back to local tools while it is building.\n");
            output_text.push_str(
                "Run `mcp__contextstream__project(action=\"index_status\")` to monitor progress.\n",
            );
        }
        IndexInfo::NotIndexed => {
            output_text.push_str("\n\u{2139} Project index not found yet \u{2014} use ContextStream search first (not Explore/Grep) to find code as it builds; fall back to local tools only if search itself returns nothing.\n");
            output_text.push_str("Keep hosted MCP configured, re-establish the intended checkout with `mcp__contextstream__init(folder_path=\"<folder>\")`, then run `mcp__contextstream__project(action=\"index\")`; the exact-checkout sync bridge supplies local bytes. Verify with `mcp__contextstream__project(action=\"index_status\")`.\n");
        }
    }

    if let Some(restored) = restored_context {
        output_text.push('\n');
        output_text.push_str(&restored);
    }

    if let Some(ctx) = context {
        output_text.push('\n');
        output_text.push_str(&ctx);
    } else {
        output_text.push_str(
            "\nNo saved context found yet. Follow the versioned workflow above: initialize once, then ground this turn with `mcp__contextstream__context(user_message=\"starting new session\")`.",
        );
    }

    // Past-session awareness — three-line escalation ladder. Every exchange
    // is saved as a transcript and key turning points are captured as
    // snapshots (manual + auto pre-compaction). When continuing prior work,
    // walk the ladder: recall → search_transcripts → list_events for
    // snapshots. Full guidance lives in CLAUDE.md.
    output_text.push_str(
        "\n\n📜 Past sessions are indexed (transcripts + snapshots at turning points). Continuing prior work?\n\
         1. `mcp__contextstream__session(action=\"recall\", query=\"...\")` — ranked fusion across transcripts/snapshots/docs/decisions.\n\
         2. If `recall` is thin: `mcp__contextstream__memory(action=\"search_transcripts\", query=\"...\")` for full-text, or `mcp__contextstream__memory(action=\"list_events\", event_type=\"session_snapshot\")` for turning-point bookmarks.\n\
         3. End of this session: save a snapshot so the next one picks up — `mcp__contextstream__session(action=\"capture\", event_type=\"session_snapshot\", title=\"...\", content=\"...\")`."
    );

    // Knowledge-first pathway for non-code lookups — the answer to "how do
    // we do X / why did we choose Y / did we already decide Q" usually
    // lives in docs, decisions, lessons, or skills, not in the source.
    output_text.push_str(
        "\n\n🧠 Not every answer lives in code. When you need decisions, docs, lessons, preferences, plans, tasks, or skills, use the matching ContextStream tool:\n\
         - decisions → `mcp__contextstream__memory(action=\"decisions\", query=\"...\", workspace_id=\"<current_workspace_id>\", project_id=\"<current_project_id>\")` when ids are available\n\
         - docs/specs → `mcp__contextstream__memory(action=\"list_docs\")` + `get_doc(doc_id=\"...\")`\n\
         - lessons → `mcp__contextstream__session(action=\"get_lessons\", query=\"...\")`\n\
         - tasks/todos/plans → `memory(action=\"list_tasks\"|\"list_todos\")`, `session(action=\"list_plans\")`\n\
         - skills → `mcp__contextstream__skill(action=\"list\")` + `skill(action=\"run\", name=\"...\")`\n\
         - unsure → `mcp__contextstream__memory(action=\"search\", query=\"...\")` (hybrid memory + docs)."
    );

    // Cursor loads `sessionStart` output via a top-level snake_case
    // `additional_context` field and uses raw (unprefixed) tool names. Strip the
    // Claude `mcp__contextstream__` prefix so the primer is callable there;
    // Claude/other editors keep the prefixed HookOutput schema.
    let output_text = if super::input_is_cursor(&input) {
        output_text.replace("mcp__contextstream__", "")
    } else {
        output_text
    };
    super::write_context_for_input(&input, output_text)?;

    Ok(())
}

// ============================================================================
// Config Loading
// ============================================================================

struct ApiConfig {
    api_key: String,
    api_url: String,
    workspace_id: Option<String>,
    #[allow(dead_code)]
    project_id: Option<String>,
}

fn load_config(cwd: &str) -> ApiConfig {
    let mut api_key = std::env::var("CONTEXTSTREAM_API_KEY").unwrap_or_default();
    let mut api_url = std::env::var("CONTEXTSTREAM_API_URL")
        .unwrap_or_else(|_| "https://api.contextstream.io".to_string());
    let mut workspace_id = std::env::var("CONTEXTSTREAM_WORKSPACE_ID").ok();
    let mut project_id = std::env::var("CONTEXTSTREAM_PROJECT_ID").ok();

    // Walk up directories to find config files
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

    // Also check home .mcp.json
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
// Index Status
// ============================================================================

enum IndexInfo {
    Indexed,
    Stale,
    Indexing,
    NotIndexed,
}

fn check_index_status(folder_path: &str) -> IndexInfo {
    let index_file =
        dirs::home_dir().map(|h| h.join(".contextstream").join("indexed-projects.json"));

    if let Some(path) = index_file {
        if let Ok(content) = std::fs::read_to_string(&path) {
            if let Ok(data) = serde_json::from_str::<Value>(&content) {
                if let Some(projects) = data.get("projects").and_then(|p| p.as_object()) {
                    let folder = Path::new(folder_path);
                    for (project_path, info) in projects {
                        if !folder.starts_with(Path::new(project_path)) {
                            continue;
                        }
                        if std::fs::metadata(project_path)
                            .map(|meta| meta.is_file())
                            .unwrap_or(false)
                        {
                            continue;
                        }

                        if let Some(indexed_at) = info.get("indexed_at").and_then(|t| t.as_str()) {
                            if let Ok(indexed_time) =
                                chrono::DateTime::parse_from_rfc3339(indexed_at)
                            {
                                let now = chrono::Utc::now();
                                let diff = now.signed_duration_since(indexed_time);
                                let diff_days = diff.num_hours() as f64 / 24.0;
                                if diff_days > 7.0 {
                                    return IndexInfo::Stale;
                                }
                                return IndexInfo::Indexed;
                            }
                        }
                        if let Some(started_at) =
                            info.get("indexing_started_at").and_then(|t| t.as_str())
                        {
                            if let Ok(started_time) =
                                chrono::DateTime::parse_from_rfc3339(started_at)
                            {
                                let now = chrono::Utc::now();
                                let diff = now.signed_duration_since(started_time);
                                if diff.num_hours() <= 6 {
                                    return IndexInfo::Indexing;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    IndexInfo::NotIndexed
}

// ============================================================================
// Context Fetching
// ============================================================================

async fn fetch_initial_context(config: &ApiConfig) -> Option<String> {
    let mut url = format!(
        "{}/api/v1/context?include_rules=true&include_lessons=true&include_decisions=true&include_plans=true&limit=5",
        config.api_url
    );

    if let Some(ref ws_id) = config.workspace_id {
        url.push_str(&format!("&workspace_id={}", ws_id));
    }

    let client = super::api_http_client();
    let response = client
        .get(&url)
        .header("X-API-Key", &config.api_key)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .ok()?;

    if !response.status().is_success() {
        return None;
    }

    let data: Value = response.json().await.ok()?;

    let mut sections = Vec::new();

    // Lessons
    if let Some(lessons) = data.get("lessons").and_then(|l| l.as_array()) {
        let kept: Vec<&Value> = lessons
            .iter()
            .filter(|lesson| !is_noise_lesson(lesson))
            .take(3)
            .collect();
        if !kept.is_empty() {
            let mut text = String::from(
                "## \u{26a0}\u{fe0f} [LESSONS_WARNING] Lessons from Past Mistakes\nTreat these lessons as active instructions for the current task.\n",
            );
            for lesson in kept {
                let title = lesson
                    .get("title")
                    .and_then(|t| t.as_str())
                    .unwrap_or("Untitled");
                let prevention = lesson
                    .get("prevention")
                    .and_then(|p| p.as_str())
                    .unwrap_or("");
                let age = super::common::extract_age_suffix(lesson, "captured_at");
                if prevention.is_empty() {
                    text.push_str(&format!("- **{}**{}\n", title, age));
                } else {
                    text.push_str(&format!("- **{}**{}: {}\n", title, age, prevention));
                }
            }
            sections.push(text);
        }
    }

    // Active plans
    if let Some(plans) = data
        .get("active_plans")
        .or_else(|| data.get("plans"))
        .and_then(|p| p.as_array())
    {
        if !plans.is_empty() {
            let mut text = String::from("## \u{1f4cb} Active Plans\n");
            for plan in plans.iter().take(3) {
                let title = plan
                    .get("title")
                    .and_then(|t| t.as_str())
                    .unwrap_or("Untitled");
                let status = plan
                    .get("status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("active");
                let age = super::common::extract_age_suffix(plan, "captured_at");
                text.push_str(&format!("- {} ({}){}\n", title, status, age));
            }
            sections.push(text);
        }
    }

    // Pending tasks
    if let Some(tasks) = data.get("pending_tasks").and_then(|t| t.as_array()) {
        if !tasks.is_empty() {
            let mut text = String::from("## \u{2705} Pending Tasks\n");
            for task in tasks.iter().take(5) {
                let title = task
                    .get("title")
                    .and_then(|t| t.as_str())
                    .unwrap_or("Untitled");
                let age = super::common::extract_age_suffix(task, "captured_at");
                text.push_str(&format!("- {}{}\n", title, age));
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
            let mut text = String::from("## \u{1f4dd} Recent Decisions\n");
            for decision in decisions.iter().take(3) {
                let title = decision
                    .get("title")
                    .and_then(|t| t.as_str())
                    .unwrap_or("Untitled");
                let age = super::common::extract_age_suffix(decision, "captured_at");
                match mcp_tools::domains::grounding::decision_conflict_note(decision) {
                    Some(note) => text.push_str(&format!("- **{}**{} {}\n", title, age, note)),
                    None => text.push_str(&format!("- **{}**{}\n", title, age)),
                }
            }
            sections.push(text);
        }
    }

    if sections.is_empty() {
        return None;
    }

    sections.push("\n---\nOn the first message in a new session call `mcp__contextstream__init(...)` then `mcp__contextstream__context(user_message=\"...\")`. After that, call `mcp__contextstream__context(user_message=\"...\")` on every message.".to_string());

    Some(sections.join("\n"))
}

async fn fetch_restore_context(
    config: &ApiConfig,
    session_id: &str,
    trigger: &str,
) -> Option<String> {
    let mut payload = serde_json::json!({
        "session_id": session_id,
        "trigger": trigger,
        "include_durable_context": true
    });
    if let Some(ref ws_id) = config.workspace_id {
        payload["workspace_id"] = serde_json::Value::String(ws_id.clone());
    }
    if let Some(ref project_id) = config.project_id {
        payload["project_id"] = serde_json::Value::String(project_id.clone());
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
    let restored = data
        .get("restored")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if !restored {
        return None;
    }

    let summary = data.get("summary").and_then(|v| v.as_str()).unwrap_or("");
    let source = data
        .get("source")
        .and_then(|v| v.as_str())
        .unwrap_or("saved state");

    Some(format!(
        "## Recent Session Restore ({})\n{}\n",
        source, summary
    ))
}

/// Filter out obvious synthetic/test lessons that shouldn't appear in a
/// real session's [LESSONS_WARNING] block. Two signal tiers:
///
/// 1. Strong synthetic phrases — always filtered (`test lesson`,
///    `sample lesson`, `dummy ...`, etc.). These patterns only exist in
///    fixture data; real lessons don't use them.
///
/// 2. Weak markers (`Test …` / `Audit …` prefix) — filtered *only* when
///    severity is absent or "low". Real lessons like "Test coverage
///    requirements" or "Audit findings must be resolved" carry a real
///    severity and pass through.
///
/// No age decay: a 6-month-old lesson about a real mistake is still
/// worth surfacing. If it was worth capturing, it's worth remembering.
fn is_noise_lesson(lesson: &Value) -> bool {
    let title = lesson
        .get("title")
        .and_then(|t| t.as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();

    // Tier 1: clearly synthetic phrases.
    const SYNTHETIC_PHRASES: &[&str] = &[
        "test lesson",
        "audit lesson",
        "sample lesson",
        "example lesson",
        "dummy lesson",
        "fixture lesson",
        "placeholder lesson",
        "lorem ipsum",
    ];
    if SYNTHETIC_PHRASES.iter().any(|p| title.contains(p)) {
        return true;
    }

    // Tier 2: weak test/audit prefix combined with low-or-missing severity.
    let severity = lesson
        .get("severity")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let weak_severity = severity.is_empty() || severity == "low";
    let weak_prefix =
        title.starts_with("test ") || title.starts_with("audit ") || title.starts_with("testing ");
    if weak_prefix && weak_severity {
        return true;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn index_status_messaging_is_calm_and_drops_grep_steer() {
        // B4: SessionStart index status leads with search working + auto-fresh,
        // and never steers toward manual local tools or unavailable-search
        // framing (forbidden phrases are assembled below, not written here).
        let src = include_str!("session_start.rs");
        assert!(src.contains("Search works now"));
        assert!(src.contains("freshens automatically"));
        // Forbidden phrases are assembled at runtime so this guard's own source
        // doesn't trip the scan.
        let grep_steer = format!("{} are available", "Local tools");
        let disabled = format!("semantic search {}", "is disabled");
        assert!(
            !src.contains(&grep_steer),
            "must drop the local-tools grep-steer phrase"
        );
        assert!(
            !src.contains(&disabled),
            "must not mark semantic search unavailable"
        );
    }

    #[test]
    fn synthetic_test_lesson_phrase_is_noise() {
        let lesson = json!({ "title": "Test lesson from audit", "severity": "low" });
        assert!(is_noise_lesson(&lesson));
    }

    #[test]
    fn sample_lesson_is_noise_regardless_of_severity() {
        let lesson = json!({ "title": "Sample lesson for onboarding", "severity": "high" });
        assert!(is_noise_lesson(&lesson));
    }

    #[test]
    fn weak_prefix_with_low_severity_is_noise() {
        let lesson = json!({ "title": "Audit smoke check", "severity": "low" });
        assert!(is_noise_lesson(&lesson));
    }

    #[test]
    fn weak_prefix_with_real_severity_is_kept() {
        let lesson = json!({
            "title": "Test coverage requirements must be at 80 percent",
            "severity": "medium"
        });
        assert!(!is_noise_lesson(&lesson));
    }

    #[test]
    fn audit_findings_lesson_with_high_severity_is_kept() {
        let lesson = json!({
            "title": "Audit findings must be resolved before release",
            "severity": "high"
        });
        assert!(!is_noise_lesson(&lesson));
    }

    #[test]
    fn old_low_severity_real_lesson_is_kept() {
        let old = (chrono::Utc::now() - chrono::Duration::days(200)).to_rfc3339();
        let lesson = json!({
            "title": "Prefer tabs over spaces in generated files",
            "severity": "low",
            "captured_at": old,
        });
        assert!(!is_noise_lesson(&lesson));
    }
}
