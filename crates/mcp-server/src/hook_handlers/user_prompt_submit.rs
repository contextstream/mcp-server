//! UserPromptSubmit hook handler.
//!
//! Injects ContextStream context (preferences, lessons, rules) on every user message.
//!
//! PERFORMANCE: Fast path via /api/v1/context/hook (~20-50ms, Redis-cached).
//! Falls back to a static reminder if the API is unreachable.

use anyhow::Result;
use mcp_types::{build_harness_teaching, HarnessId, HarnessTeachingDelivery};
use serde_json::Value;
use std::path::{Path, PathBuf};

use super::prompt_state;
use super::save_intent;
use super::{write_stdout_json, HookOutput};

/// Static fallback reminder base (no sensitive text).
const FALLBACK_REMINDER_BASE: &str = r#"[CONTEXTSTREAM EXTENDED GUIDANCE]
AUTO-GROUNDING: when `context()` returns `[GROUNDING]`, those lines are pre-ranked prior work for this message — read them before code search; skipping search is often correct. One-shot outside `context()`: mcp__contextstream__session(action="ground", user_message="...").
PROJECT ROUTING: when `init()` or `context()` surfaces `[PROJECT_ROUTING]`, treat it as an active scope guardrail. If status is uncertain/ambiguous/needs_project_selection/needs_project_setup, do not do project-scoped search, memory, session, skill, indexing, or capture writes until you pass the selected `workspace_id`/`project_id`, rerun `init(folder_path="...")` or `context(folder_path="...")`, or ask the user to choose from surfaced candidates.
FRESHNESS: decisions, transcript continuity, snapshots, active plans, and tasks are time-sensitive. If grounding or hook context shows old/stale hits, refresh with session(action="ground"), memory(action="decisions", workspace_id="<current_workspace_id>", project_id="<current_project_id>") when ids are available, or memory(action="search_transcripts") before using them to plan or implement. Treat Gemini/LLM-derived [INSIGHT] items as advisory unless backed by a current captured decision/event/doc.
If ContextStream surfaces `[LESSONS_WARNING]`, treat those lessons as mandatory working instructions for the current task, not optional background context.
If ContextStream surfaces `[COORDINATION]`, read it before continuing, act on the shared context, then ack via coordination(action="ack", notice_id="..."). Coordination is shared awareness, not a handoff.
INSTRUCT ALIGNMENT: if available, call mcp__contextstream__instruct(action="get", session_id="...", workspace_id="<current_workspace_id>", project_id="<current_project_id>") before context each turn and ack consumed ids with the same session/workspace after use. Reuse the ids returned by init/context; if no current project is resolved, omit project_id intentionally for workspace-only instructions rather than inferring it.
TOOL DISAMBIGUATION: 'search' is the ONLY tool for codebase/file search. Do NOT use 'session'(smart_search) or 'memory'(search) for code search — those search conversation history and memory nodes respectively. For codebase search: use search(mode="auto") first when indexed. If search returns results with a stale-index advisory, those results are still usable for existing indexed code; refresh/retry before concluding a new symbol is absent. Use local tools only after the stale/not-indexed grace path or an explicit 0-result retry.
COMMON MEMORY CALLS: list docs via memory(action="list_docs"), list lessons via session(action="get_lessons"), list decisions via memory(action="decisions", workspace_id="<current_workspace_id>", project_id="<current_project_id>") when init/context surfaced ids (otherwise call init/ground first or omit ids only after scope is initialized), list plans via session(action="list_plans"), list tasks/todos via memory(action="list_tasks"|"list_todos").
MEDIA ASSETS: photos/images, videos, audio, and documents/PDFs live behind media, not code search or local file reads. List assets with media(action="list"), semantically search indexed assets/transcripts/OCR with media(action="search", query="...", content_types=["image"]) using image/video/audio/document as needed, index local/URL assets with media(action="index", file_path="...", content_type="image") or media(action="index", external_url="...", content_type="document") using the matching canonical type, then check media(action="status", content_id="..."). Friendly words map as photos/images -> image and docs/PDFs/slides -> document.
STRUCTURED ENTITIES: tickets/handoffs/incidents/releases/experiments/goals/key_results/sprints/reviews/risks/backlog_views all live behind one tool — entity(kind="<kind>", action="list|get|create|update|delete", body={...}, query={...}). Every request to prepare/create a handoff, hand work over, or continue in another agent/session must create entity(kind="handoff", action="create", body={"title":"...","summary":"...","scope":"...","next_steps":[...]}). Add to_user_id only when known. HANDOFF.md, scratch prompts, generic docs/events, and prose are not substitutes; add capsule only when a portable bundle/share link is requested. Other examples: entity(kind="ticket", action="create", body={"title": "Fix replication lag", "kind": "bug", "priority": "high"}); entity(kind="goal", action="list", query={"period": "2026-Q2"}); entity(kind="risk", action="list", query={"status": "open", "impact": "severe"}). Markdown-shaped artefacts (runbook, adr, rfc, postmortem, retro, release_notes, playbook, prd, user_story, persona, interview, design_spec, critique, glossary, oncall_schedule, slo, q_and_a, changelog, style_guide) are docs — use memory(action="create_doc", doc_type="<type>", title="...", content="..."). Distilled summary nodes for goal/risk/term — use memory(action="create_node", node_type="goal|risk|term", ...). Recurring signals — use memory(action="create_event", event_type="standup|status_update|question|approval|feedback|discovery|achievement", ...).
CODE HEALTH / DEPENDENCY RECOMMENDATIONS: when the user asks about code quality, dependency risk, circular dependencies, unused code, complexity, dashboard scans, or recommendations from prior dashboard analysis, call graph before guessing from source alone. Use graph(action="quality_freshness"), graph(action="quality_trends"), graph(action="quality_history"), graph(action="circular_dependencies"), graph(action="unused_code"), graph(action="complexity_metrics"), or graph(action="dependencies") with the current project_id. Use returned recommendations to propose small plans/tickets before edits; use graph(action="quality_snapshot") after scans/fixes when a saved baseline is useful.
KNOWLEDGE FIRST (not just code): when the user asks "how/why/what pattern/did we decide X?", the answer usually lives in docs/decisions/lessons/preferences/plans/tasks/skills — NOT in source. Pick by type: decisions → memory(action="decisions", workspace_id="<current_workspace_id>", project_id="<current_project_id>") when ids are available, docs → memory(action="list_docs"|"get_doc"), lessons → session(action="get_lessons"), preferences/constraints → memory(action="list_nodes", node_type="preference"|"constraint"), tasks/todos → memory(action="list_tasks"|"list_todos"), plans → session(action="list_plans"|"get_plan"), skills → skill(action="list"|"run"), unsure → memory(action="search") (hybrid memory + docs). Search code only after checking the right knowledge surface.
PAST SESSIONS: when the user references prior work ("last time", "yesterday", "pick up where we left off"), read any `[GROUNDING]` from your last `context()` first. Fresh, relevant, sufficient grounding completes the retrieval step; do not immediately duplicate it with session(action="recall"). Use recall only when grounding is absent, thin, stale, off-topic, or the user explicitly requests broader or session-specific history. For a bundled pack without waiting on context: session(action="ground", user_message="..."). If recall is thin, fall through to memory(action="search_transcripts", query="...") for full-text or memory(action="list_events", event_type="session_snapshot") for turning-point bookmarks. Save a session_snapshot at the end so the next session can pick up."#;

/// Build the full fallback reminder with decoded attribution.
fn fallback_reminder() -> String {
    format!(
        "{}\n{}\n[END]",
        FALLBACK_REMINDER_BASE,
        super::protected::prompt_attribution()
    )
}

/// Compensation guidance for editors without full lifecycle hook coverage.
const MISSING_LIFECYCLE_COMPENSATION: &str = r#"[CONTEXTSTREAM LIFECYCLE]
This editor has partial hook support (no SessionStart/PreCompact lifecycle hooks).
- If ContextStream surfaces `[LESSONS_WARNING]`, treat those lessons as active instructions for the task until it is finished.
- If ContextStream surfaces `[COORDINATION]`, read it before continuing and ack after using it via `coordination(action="ack", notice_id="...")`. Do not treat it as a handoff.
- Use a stable `session_id` and reuse it for all context calls in this conversation.
- If instruct is enabled, call `mcp__contextstream__instruct(action="get", session_id="...", workspace_id="<current_workspace_id>", project_id="<current_project_id>")` before each context call and ack consumed IDs with the same session/workspace after use. Reuse ids returned by init/context; if no project is resolved, omit project_id intentionally for workspace-only instructions rather than inferring it.
- Transcript capture is optional and OFF by default. Enable with `save_exchange=true` (with `session_id`) and disable with `save_exchange=false`.
- Capture checkpoints after major work: `mcp__contextstream__session(action="capture", event_type="session_snapshot", title="Session checkpoint", content="...")`.
- Auto-grounding: read `[GROUNDING]` from your last `context()` when present. If it is fresh, relevant, and sufficient, stop the continuation retrieval there; do not immediately duplicate it with `mcp__contextstream__session(action="recall")`. Use recall only when grounding is absent, thin, stale, off-topic, or the user explicitly requests broader or session-specific history. One-shot bundle: `mcp__contextstream__session(action="ground", user_message="...")`. If recall is thin, fall through to `mcp__contextstream__memory(action="search_transcripts", query="...")` or `mcp__contextstream__memory(action="list_events", event_type="session_snapshot")` for turning-point bookmarks.
- Project routing: when `[PROJECT_ROUTING]` appears, resolve it before project-scoped work. For uncertain/ambiguous/needs_project_selection/needs_project_setup statuses, pass the selected `workspace_id`/`project_id`, rerun `init(folder_path="...")` or `context(folder_path="...")`, or ask the user to choose from the surfaced candidates before writing/searching/indexing under a project.
- Freshness: inspect source age before relying on decisions, transcript continuity, snapshots, active plans, or tasks. Refresh stale hits with `mcp__contextstream__session(action="ground", user_message="...")`, `mcp__contextstream__memory(action="decisions", query="...", workspace_id="<current_workspace_id>", project_id="<current_project_id>")` when ids are available, or `mcp__contextstream__memory(action="search_transcripts", query="...")`. Gemini/LLM-derived insights guide investigation but are not durable decisions until captured as decisions/events/docs.
- Finding non-code info: pick the right ContextStream surface — decisions → `memory(action="decisions", workspace_id="<current_workspace_id>", project_id="<current_project_id>")` when ids are available, docs → `memory(action="list_docs"|"get_doc")`, lessons → `session(action="get_lessons")`, preferences/constraints → `memory(action="list_nodes", node_type="...")`, tasks/todos → `memory(action="list_tasks"|"list_todos")`, plans → `session(action="list_plans"|"get_plan")`, skills → `skill(action="list"|"run")`, unsure → `memory(action="search")`. Search code only after checking the right knowledge surface.
- Media assets: photos/images, videos, audio, and documents/PDFs use `mcp__contextstream__media(...)`. Use `media(action="search", query="...", content_types=["image"])` or `media(action="list")` to inspect indexed assets; use `media(action="index", file_path="...", content_type="image")` with the matching canonical type before expecting ContextStream to read a local/URL asset.
- Structured entities (tickets, handoffs, incidents, releases, experiments, goals, key_results, sprints, reviews, risks, backlog_views) all live behind `entity(kind="<kind>", action="list|get|create|update|delete", body={...}, query={...})`. A generic agent/session handoff always uses `entity(kind="handoff", action="create", body={"title":"...","summary":"...","scope":"...","next_steps":[...]})`; omit unknown `to_user_id`, never substitute HANDOFF.md/docs/events/prose, and additionally call capsule only for a requested portable bundle/share link. Other examples: `entity(kind="ticket", action="create", body={"title": "...", "kind": "bug"})`, `entity(kind="risk", action="list", query={"status": "open"})`. Markdown-shaped knowledge (runbook/adr/rfc/postmortem/retro/release_notes/playbook/prd/user_story/persona/interview/design_spec/critique/glossary/oncall_schedule/slo/q_and_a/changelog/style_guide) is still docs — use `memory(action="create_doc", doc_type="<type>")`.
- Code Health/dependency recommendations: when asked about code quality, dependency risk, circular dependencies, unused code, complexity, dashboard scans, or recommendations from prior dashboard analysis, call `graph(...)` with the current `project_id` before guessing. Useful actions: `quality_freshness`, `quality_trends`, `quality_history`, `circular_dependencies`, `unused_code`, `complexity_metrics`, `dependencies`, and `quality_snapshot`. Use the returned recommendations field to propose small tracked plans/tickets before edits.
- Persist plans/tasks so work doesn't disappear: `mcp__contextstream__session(action="capture_plan", title="...", steps=[...])` and `mcp__contextstream__memory(action="create_task", title="...", plan_id="...")`.
- For long-running/queued ContextStream writes (docs, plans, tasks, todos, events, remember/capture, indexing), post user-visible progress updates before and after the tool call so the user never thinks it is hanging.
[END]"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditorFormat {
    Claude,
    ClineLike,
    Cursor,
    Windsurf,
}

impl EditorFormat {
    fn as_api_str(self) -> &'static str {
        match self {
            EditorFormat::Claude => "claude",
            EditorFormat::ClineLike => "cline_like",
            EditorFormat::Cursor => "cursor",
            EditorFormat::Windsurf => "windsurf",
        }
    }

    fn harness_id(self) -> HarnessId {
        match self {
            EditorFormat::Claude => HarnessId::ClaudeCode,
            EditorFormat::ClineLike => HarnessId::Cline,
            EditorFormat::Cursor => HarnessId::Cursor,
            EditorFormat::Windsurf => HarnessId::Windsurf,
        }
    }
}

fn supports_hard_first_call_enforcement(editor: EditorFormat) -> bool {
    matches!(
        editor,
        EditorFormat::Claude
            | EditorFormat::Cursor
            | EditorFormat::Windsurf
            | EditorFormat::ClineLike
    )
}

fn detect_editor(input: &Value) -> EditorFormat {
    // 1. Payload-based detection (most specific — check all editors first)

    // Windsurf payload markers
    let windsurf_payload = input
        .get("hook_event_name")
        .and_then(|v| v.as_str())
        .map(|name| name.eq_ignore_ascii_case("pre_user_prompt"))
        .unwrap_or(false)
        || input
            .get("hookEventName")
            .and_then(|v| v.as_str())
            .map(|name| name.eq_ignore_ascii_case("pre_user_prompt"))
            .unwrap_or(false);

    if windsurf_payload {
        return EditorFormat::Windsurf;
    }

    // Cline/Roo/Kilo use camelCase
    if input.get("hookName").is_some()
        || input.get("toolName").is_some()
        || input.get("workspaceRoots").is_some()
    {
        return EditorFormat::ClineLike;
    }

    // Cursor uses hook_event_name without tool_name/toolName
    if input.get("hook_event_name").is_some()
        && input.get("tool_name").is_none()
        && input.get("toolName").is_none()
    {
        return EditorFormat::Cursor;
    }

    // Claude Code uses tool_name (snake_case)
    if input.get("tool_name").is_some() {
        return EditorFormat::Claude;
    }

    // 2. Environment-based fallback (when payload has no editor-specific markers)
    // Only use WINDSURF_CASCADE_TERMINAL_KIND — this env var is set exclusively
    // by the Windsurf editor at runtime. Do NOT check PATH for "windsurf" as
    // having Windsurf installed doesn't mean we're running inside it.
    if std::env::var("WINDSURF_CASCADE_TERMINAL_KIND").is_ok() {
        return EditorFormat::Windsurf;
    }

    EditorFormat::Claude
}

fn extract_cwd(input: &Value) -> String {
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
        .unwrap_or_else(|| ".".to_string())
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

fn write_editor_output(editor: EditorFormat, context: Option<String>) -> Result<()> {
    match editor {
        EditorFormat::Claude => {
            if let Some(text) = context {
                write_stdout_json(&HookOutput::context(text))?;
            } else {
                write_stdout_json(&HookOutput::empty())?;
            }
        }
        EditorFormat::ClineLike => {
            let output = if let Some(text) = context {
                serde_json::json!({
                    "cancel": false,
                    "contextModification": text,
                })
            } else {
                serde_json::json!({ "cancel": false })
            };
            println!("{}", serde_json::to_string(&output)?);
        }
        EditorFormat::Cursor => {
            // Cursor's `beforeSubmitPrompt` cannot inject context into the agent
            // — `user_message` is only surfaced when the prompt is *blocked*
            // (`continue: false`), and blocking here would drop the user's
            // message before the agent ever sees it. So this hook is
            // continue-only: its real job is the `mark_context_required`
            // side-effect in `handle()`, which the PreToolUse gate enforces via
            // `deny` + `agent_message`. Per-message guidance is delivered by the
            // always-on `.cursor/rules/*.mdc` and the `sessionStart` primer.
            let _ = context;
            println!(
                "{}",
                serde_json::to_string(&serde_json::json!({ "continue": true }))?
            );
        }
        EditorFormat::Windsurf => {
            // Windsurf pre_user_prompt hooks are exit-code based and do not
            // support the Cursor/Claude JSON response formats.
            let _ = context;
        }
    }

    Ok(())
}

fn build_context_message(editor: EditorFormat, base_context: String, input: &Value) -> String {
    let core = build_harness_teaching(
        Some(editor.harness_id()),
        HarnessTeachingDelivery::HookReminder,
    )
    .rendered_guidance;
    let mut context_parts = vec![core, base_context];

    if !matches!(editor, EditorFormat::Claude) {
        context_parts.push(MISSING_LIFECYCLE_COMPENSATION.to_string());
    }

    if let Some(save_guidance) = save_intent::guidance_for_input(input) {
        context_parts.push(save_guidance);
    }

    context_parts.join("\n\n")
}

fn compact_one_line(value: &str, max_chars: usize) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max_chars)
        .collect()
}

fn hook_project_routing_needs_attention(routing: &Value) -> bool {
    let status = routing
        .get("status")
        .and_then(|value| value.as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    // Quiet, resolved statuses never warrant a reminder — resolved_by_folder
    // intentionally ships its candidate, so it must not fall through to the
    // missing-current-project fallback below.
    if matches!(status.as_str(), "confirmed" | "resolved_by_folder") {
        return false;
    }
    let status_needs_attention = matches!(
        status.as_str(),
        "ambiguous"
            | "uncertain"
            | "missing_project"
            | "needs_project_selection"
            | "needs_project_setup"
            | "needs_workspace_selection"
            | "project_missing"
            | "switch_suggested"
            | "unresolved"
    );
    let project_switch_signal = routing
        .get("project_switch_signal")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let has_candidates = routing
        .get("candidates")
        .and_then(|value| value.as_array())
        .map(|values| !values.is_empty())
        .unwrap_or(false);
    let missing_current_project = routing
        .get("current_project_id")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_none();

    status_needs_attention || project_switch_signal || (missing_current_project && has_candidates)
}

fn hook_project_routing_notice(data: &Value) -> Option<String> {
    let routing = data.get("project_routing")?;
    if !hook_project_routing_needs_attention(routing) {
        return None;
    }

    let status = routing
        .get("status")
        .and_then(|value| value.as_str())
        .unwrap_or("unresolved");
    let action = routing
        .get("suggested_action")
        .and_then(|value| value.as_str())
        .unwrap_or(
            "Resolve project scope before project-scoped search, memory, session, skill, indexing, or capture calls.",
        );

    let mut parts = vec![format!("[PROJECT_ROUTING] status={status}")];
    if let Some(reason) = routing.get("reason").and_then(|value| value.as_str()) {
        if !reason.trim().is_empty() {
            parts.push(format!("reason={}", compact_one_line(reason, 180)));
        }
    }
    if let Some(project_name) = routing
        .get("current_project_name")
        .and_then(|value| value.as_str())
    {
        parts.push(format!(
            "current_project={}",
            compact_one_line(project_name, 80)
        ));
    }
    if let Some(project_id) = routing
        .get("current_project_id")
        .and_then(|value| value.as_str())
    {
        parts.push(format!("current_project_id={project_id}"));
    }
    if let Some(folder_path) = routing.get("folder_path").and_then(|value| value.as_str()) {
        parts.push(format!("folder={}", compact_one_line(folder_path, 120)));
    }
    parts.push(format!("action={}", compact_one_line(action, 220)));

    if let Some(candidates) = routing.get("candidates").and_then(|value| value.as_array()) {
        let rendered = candidates
            .iter()
            .take(3)
            .map(|candidate| {
                let name = candidate
                    .get("project_name")
                    .and_then(|value| value.as_str())
                    .unwrap_or("Unnamed project");
                let id = candidate
                    .get("project_id")
                    .and_then(|value| value.as_str())
                    .map(|value| format!(" id={value}"))
                    .unwrap_or_default();
                let score = candidate
                    .get("score")
                    .and_then(|value| value.as_f64())
                    .map(|value| format!(" score={value:.2}"))
                    .unwrap_or_default();
                format!("{}{}{}", compact_one_line(name, 80), id, score)
            })
            .collect::<Vec<_>>()
            .join(" | ");
        if !rendered.is_empty() {
            parts.push(format!("candidates={rendered}"));
        }
    }

    Some(parts.join(" "))
}

struct ApiConfig {
    api_key: String,
    api_url: String,
    workspace_id: Option<String>,
    project_id: Option<String>,
    session_id: Option<String>,
}

/// Handle the UserPromptSubmit hook.
///
/// Fast path: call /api/v1/context/hook (Redis-cached, ~20-50ms).
/// Fallback: output static reminder if API unavailable.
pub async fn handle() -> Result<()> {
    let input: Value =
        serde_json::from_reader(std::io::stdin().lock()).unwrap_or_else(|_| serde_json::json!({}));
    let editor = detect_editor(&input);

    if std::env::var("CONTEXTSTREAM_REMINDER_ENABLED")
        .map(|v| v == "false")
        .unwrap_or(false)
    {
        write_editor_output(editor, None)?;
        return Ok(());
    }

    let cwd = extract_cwd(&input);

    // Mark this prompt as requiring a first `context(...)` tool call so
    // PreToolUse can enforce it before non-simple MCP operations.
    if supports_hard_first_call_enforcement(editor) {
        prompt_state::cleanup_stale(180);
        prompt_state::mark_context_required(&cwd);
    }

    let mut config = load_config(&cwd);
    config.session_id = extract_session_id(&input);

    // Try fast API call if we have credentials. Reconcile unflushed local
    // edits (P6 content freshness) concurrently with the context fetch, with a
    // tight deadline so the per-prompt fast path stays fast. The drain's own
    // cooldown makes the vast majority of prompts a cheap no-op.
    let context_fut = async {
        if !config.api_key.is_empty() {
            match fetch_hook_context(&config, editor, &cwd).await {
                Some(ctx) => ctx,
                None => fallback_reminder(),
            }
        } else {
            fallback_reminder()
        }
    };
    let (base_context, _) = tokio::join!(
        context_fut,
        super::dirty_drain::drain_best_effort(std::time::Duration::from_secs(2)),
    );

    // Append decoded attribution to every response
    let context = format!(
        "{}\n{}",
        base_context,
        super::protected::prompt_attribution()
    );

    let combined = build_context_message(editor, context, &input);
    write_editor_output(editor, Some(combined))?;
    Ok(())
}

/// Fetch hook context from the fast /api/v1/context/hook endpoint.
/// Returns None if the API is unreachable or returns an error.
async fn fetch_hook_context(config: &ApiConfig, editor: EditorFormat, cwd: &str) -> Option<String> {
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
    body.insert(
        "hook_type".to_string(),
        Value::String("user_prompt_submit".to_string()),
    );
    if !cwd.trim().is_empty() {
        body.insert("folder_path".to_string(), Value::String(cwd.to_string()));
    }
    body.insert(
        "editor".to_string(),
        Value::String(editor.as_api_str().to_string()),
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
    let payload = data.get("data")?;
    let mut context = payload
        .get("context")
        .and_then(|c| c.as_str())
        .map(String::from)?;
    if !context.contains("[PROJECT_ROUTING]") {
        if let Some(notice) = hook_project_routing_notice(payload) {
            context.push('\n');
            context.push_str(&notice);
        }
    }
    Some(context)
}

// ============================================================================
// Config Loading
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

    // Check ~/.contextstream/credentials.json for saved API key
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_cline_like_editor_format() {
        let input = serde_json::json!({
            "hookName": "UserPromptSubmit",
            "workspaceRoots": ["/tmp/project"]
        });
        assert!(matches!(detect_editor(&input), EditorFormat::ClineLike));
    }

    #[test]
    fn detects_cursor_editor_format() {
        let input = serde_json::json!({
            "hook_event_name": "beforeSubmitPrompt"
        });
        assert!(matches!(detect_editor(&input), EditorFormat::Cursor));
    }

    #[test]
    fn non_claude_context_includes_lifecycle_compensation() {
        let input = serde_json::json!({});
        let message = build_context_message(EditorFormat::Cursor, "base".to_string(), &input);
        assert!(message.contains("partial hook support"));
        assert!(message.contains("[LESSONS_WARNING]"));
    }

    #[test]
    fn fallback_reminder_treats_lessons_as_mandatory() {
        let reminder = fallback_reminder();
        assert!(reminder.contains("[LESSONS_WARNING]"));
        assert!(reminder.contains("mandatory working instructions"));
    }

    #[test]
    fn instruction_alignment_requires_explicit_current_scope() {
        let reminder = fallback_reminder();
        for guidance in [reminder.as_str(), MISSING_LIFECYCLE_COMPENSATION] {
            assert!(guidance.contains(
                "workspace_id=\"<current_workspace_id>\", project_id=\"<current_project_id>\""
            ));
            assert!(guidance.contains("omit project_id intentionally for workspace-only"));
            assert!(!guidance.contains("instruct(action=\"get\", session_id=\"...\")"));
        }
    }

    #[test]
    fn continuation_guidance_stops_after_sufficient_auto_grounding() {
        let reminder = fallback_reminder();
        assert!(reminder.contains("do not immediately duplicate"));
        assert!(reminder.contains("absent, thin, stale, off-topic"));
        assert!(!reminder.contains("first, then call session(action=\"recall\""));

        assert!(MISSING_LIFECYCLE_COMPENSATION.contains("stop the continuation retrieval there"));
        assert!(MISSING_LIFECYCLE_COMPENSATION.contains("absent, thin, stale, off-topic"));
        assert!(!MISSING_LIFECYCLE_COMPENSATION.contains("when present; then"));
    }

    #[test]
    fn fallback_reminder_surfaces_project_routing_guardrail() {
        let reminder = fallback_reminder();
        assert!(reminder.contains("PROJECT ROUTING"));
        assert!(reminder.contains("[PROJECT_ROUTING]"));
        assert!(reminder.contains("needs_project_selection"));
    }

    #[test]
    fn fallback_reminder_surfaces_media_assets() {
        let reminder = fallback_reminder();
        assert!(reminder.contains("MEDIA ASSETS"));
        assert!(reminder.contains("photos/images"));
        assert!(reminder.contains("documents/PDFs"));
        assert!(reminder.contains("media(action=\"search\""));
        assert!(reminder.contains("media(action=\"index\""));
    }

    #[test]
    fn fallback_reminder_surfaces_code_health_graph_actions() {
        let reminder = fallback_reminder();
        assert!(reminder.contains("CODE HEALTH / DEPENDENCY RECOMMENDATIONS"));
        assert!(reminder.contains("graph(action=\"quality_trends\""));
        assert!(reminder.contains("graph(action=\"complexity_metrics\""));
        assert!(reminder.contains("graph(action=\"quality_snapshot\""));
    }

    #[test]
    fn lifecycle_compensation_surfaces_media_assets() {
        let input = serde_json::json!({});
        let message = build_context_message(EditorFormat::Cursor, "base".to_string(), &input);
        assert!(message.contains("Media assets"));
        assert!(message.contains("mcp__contextstream__media"));
        assert!(message.contains("media(action=\"list\""));
    }

    #[test]
    fn lifecycle_compensation_surfaces_code_health_graph_actions() {
        let input = serde_json::json!({});
        let message = build_context_message(EditorFormat::Cursor, "base".to_string(), &input);
        assert!(message.contains("Code Health/dependency recommendations"));
        assert!(message.contains("quality_freshness"));
        assert!(message.contains("complexity_metrics"));
    }

    #[test]
    fn lifecycle_compensation_surfaces_project_routing_guardrail() {
        let input = serde_json::json!({});
        let message = build_context_message(EditorFormat::Cursor, "base".to_string(), &input);
        assert!(message.contains("Project routing"));
        assert!(message.contains("[PROJECT_ROUTING]"));
        assert!(message.contains("needs_project_setup"));
    }

    #[test]
    fn hook_project_routing_notice_formats_structured_payload() {
        let data = serde_json::json!({
            "project_routing": {
                "status": "uncertain",
                "reason": "Folder matched multiple projects",
                "folder_path": "/tmp/workspace/app",
                "suggested_action": "Choose a candidate",
                "candidates": [{
                    "project_id": "22222222-2222-4222-8222-222222222222",
                    "project_name": "app",
                    "score": 0.91
                }]
            }
        });

        let notice = hook_project_routing_notice(&data).expect("routing notice");
        assert!(notice.contains("[PROJECT_ROUTING]"));
        assert!(notice.contains("status=uncertain"));
        assert!(notice.contains("Choose a candidate"));
        assert!(notice.contains("app"));
        assert!(notice.contains("score=0.91"));
    }

    #[test]
    fn hook_project_routing_quiet_statuses_stay_quiet() {
        // confirmed and resolved_by_folder must not surface a reminder even
        // with candidates present and no current project id — the fallback
        // clause below the status match must not catch them.
        for status in ["confirmed", "resolved_by_folder"] {
            let data = serde_json::json!({
                "project_routing": {
                    "status": status,
                    "reason": "folder_binding",
                    "suggested_action": "Continue with the current project scope.",
                    "candidates": [{
                        "project_id": "22222222-2222-4222-8222-222222222222",
                        "project_name": "app",
                        "score": 1.0
                    }]
                }
            });
            assert!(
                hook_project_routing_notice(&data).is_none(),
                "status {status} should stay quiet"
            );
        }
    }

    #[test]
    fn save_intent_guidance_is_appended_when_detected() {
        let input = serde_json::json!({
            "prompt": "Please save this decision for future reference."
        });
        let message = build_context_message(EditorFormat::Claude, "base".to_string(), &input);
        assert!(message.contains("[CONTEXTSTREAM DOCUMENT STORAGE]"));
    }

    #[test]
    fn handoff_intent_gets_specific_entity_first_guidance() {
        let input = serde_json::json!({
            "prompt": "Please prepare a handoff so the next agent can continue."
        });
        let message = build_context_message(EditorFormat::Claude, "base".to_string(), &input);
        assert!(message.contains("[CONTEXTSTREAM CANONICAL HANDOFF]"));
        assert!(message.contains("mcp__contextstream__entity"));
        assert!(message.contains("kind=\"handoff\""));
        assert!(message.contains("HANDOFF.md"));
        assert!(message.contains("NOT a substitute"));
    }

    #[test]
    fn hook_messages_use_the_shared_versioned_workflow() {
        for editor in [
            EditorFormat::Claude,
            EditorFormat::Cursor,
            EditorFormat::Windsurf,
            EditorFormat::ClineLike,
        ] {
            let message = build_context_message(
                editor,
                "dynamic context".to_string(),
                &serde_json::json!({}),
            );
            assert!(message.contains(mcp_types::HARNESS_TEACHING_VERSION));
            let init = message.find("init(").expect("init instruction");
            let context = message.find("context(").expect("context instruction");
            assert!(init < context, "init must precede context for {editor:?}");
            assert!(!message.contains("before or after context"));
        }
    }

    #[test]
    fn fallback_extended_guidance_does_not_redefine_core_ordering() {
        let reminder = fallback_reminder();
        assert!(reminder.contains("[CONTEXTSTREAM EXTENDED GUIDANCE]"));
        assert!(!reminder.contains("before or after"));
        assert!(!reminder.contains("Quick-start on a new session"));
    }

    #[test]
    fn hard_first_call_enforcement_supported_for_all_tier_a_formats() {
        assert!(supports_hard_first_call_enforcement(EditorFormat::Claude));
        assert!(supports_hard_first_call_enforcement(EditorFormat::Cursor));
        assert!(supports_hard_first_call_enforcement(EditorFormat::Windsurf));
        assert!(supports_hard_first_call_enforcement(
            EditorFormat::ClineLike
        ));
    }
}
