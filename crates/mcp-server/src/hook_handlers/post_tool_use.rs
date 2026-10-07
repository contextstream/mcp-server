//! PostToolUse hook handler.
//!
//! Indexes files after Edit/Write/NotebookEdit operations for real-time search updates.
//! Supports multiple editor formats: Claude Code, Cursor, Cline/Roo/Kilo.

use anyhow::Result;
use mcp_client::{ContextStreamClient, TargetedFileDecision};
use mcp_session::auto_init::{checkout_binding_workspace, persist_folder_mapping};
use serde_json::Value;
use std::path::Path;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use uuid::Uuid;

use super::{read_stdin_json, write_stdout_json, HookOutput};

/// Supported file extensions for indexing.
const INDEXABLE_EXTENSIONS: &[&str] = &[
    "ts", "tsx", "js", "jsx", "mjs", "cjs", "py", "pyi", "pyw", "rs", "go", "java", "kt", "kts",
    "scala", "rb", "cs", "fs", "vb", "cpp", "cc", "cxx", "c", "h", "hpp", "hxx", "swift", "m",
    "mm", "vue", "svelte", "astro", "php", "lua", "r", "jl", "zig", "ex", "exs", "clj", "cljs",
    "cljc", "hs", "ml", "mli", "tf", "hcl", "yaml", "yml", "toml", "json", "md", "mdx", "txt",
    "rst", "sql", "graphql", "gql", "proto", "prisma", "sh", "bash", "zsh", "fish", "html", "htm",
    "css", "scss", "sass", "less", "xml", "ini", "cfg",
];

/// Special filenames without extensions that should be indexed.
const SPECIAL_FILENAMES: &[&str] = &["dockerfile", "makefile", "rakefile", "gemfile", "procfile"];

/// Stale local index threshold used by hosted-remote init bridging.
const STALE_THRESHOLD_HOURS: i64 = 24 * 7;

/// Check if a file should be indexed based on extension and size.
pub(crate) fn should_index(path: &str) -> bool {
    let ext = path.rsplit('.').next().unwrap_or("");
    if INDEXABLE_EXTENSIONS.contains(&ext) {
        return true;
    }
    // Check special filenames without extensions
    if let Some(basename) = Path::new(path).file_name().and_then(|f| f.to_str()) {
        if SPECIAL_FILENAMES.contains(&basename.to_lowercase().as_str()) {
            return true;
        }
    }
    false
}

/// Extract tool name from any editor format.
fn extract_tool_name(input: &Value) -> String {
    let hook_event_name = input
        .get("hook_event_name")
        .or_else(|| input.get("hookEventName"))
        .or_else(|| input.get("agent_action_name"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if !hook_event_name.is_empty() {
        match hook_event_name.to_ascii_lowercase().as_str() {
            "post_write_code" => return "Write".to_string(),
            "post_mcp_tool_use" => {}
            _ => {}
        }
    }

    input
        .get("tool_name")
        .and_then(|v| v.as_str())
        .or_else(|| input.get("toolName").and_then(|v| v.as_str()))
        .or_else(|| input.get("mcp_tool_name").and_then(|v| v.as_str()))
        .or_else(|| {
            input
                .get("tool_info")
                .and_then(|v| v.get("mcp_tool_name"))
                .and_then(|v| v.as_str())
        })
        .or_else(|| input.get("tool").and_then(|v| v.as_str()))
        .or_else(|| {
            input
                .get("tool")
                .and_then(|v| v.get("name"))
                .and_then(|v| v.as_str())
        })
        .unwrap_or("")
        .to_string()
}

/// Extract tool input from any editor format.
fn extract_tool_input(input: &Value) -> Value {
    if let Some(tool_info) = input.get("tool_info") {
        let hook_event_name = input
            .get("hook_event_name")
            .or_else(|| input.get("hookEventName"))
            .or_else(|| input.get("agent_action_name"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if hook_event_name.to_ascii_lowercase().as_str() == "post_write_code" {
            return serde_json::json!({
                "path": tool_info.get("file_path").and_then(|v| v.as_str()).unwrap_or(""),
            });
        }
    }

    input
        .get("tool_input")
        .or_else(|| input.get("parameters"))
        .or_else(|| input.get("toolParameters"))
        .or_else(|| input.get("args"))
        .or_else(|| input.get("arguments"))
        .or_else(|| {
            input
                .get("tool_info")
                .and_then(|v| v.get("mcp_tool_arguments"))
        })
        .or_else(|| input.get("tool").and_then(|v| v.get("parameters")))
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}))
}

/// Extract tool response from any editor format.
fn extract_tool_response(input: &Value) -> Value {
    input
        .get("tool_response")
        .or_else(|| input.get("toolResponse"))
        .or_else(|| {
            input
                .get("tool_info")
                .and_then(|value| value.get("tool_response"))
        })
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}))
}

fn extract_mcp_server_name(input: &Value) -> String {
    input
        .get("mcp_server_name")
        .and_then(|v| v.as_str())
        .or_else(|| {
            input
                .get("tool_info")
                .and_then(|v| v.get("mcp_server_name"))
                .and_then(|v| v.as_str())
        })
        .unwrap_or("")
        .to_string()
}

fn is_contextstream_server_call(server_name: &str, tool_name: &str) -> bool {
    server_name.to_ascii_lowercase().contains("contextstream")
        || super::durable_paths::is_contextstream_mcp_tool_name(tool_name)
}

fn normalize_contextstream_tool_name(tool_name: &str) -> String {
    super::durable_paths::normalize_contextstream_tool_name(tool_name.trim())
}

fn contextstream_action(tool_input: &Value) -> String {
    tool_input
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

fn contextstream_kind(tool_input: &Value) -> String {
    tool_input
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

fn tool_call_succeeded(tool_response: &Value) -> bool {
    let has_response_evidence = match tool_response {
        Value::Object(object) => !object.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::String(value) => !value.trim().is_empty(),
        _ => false,
    };
    let is_error = tool_response
        .get("isError")
        .or_else(|| tool_response.get("is_error"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    has_response_evidence && !is_error && tool_response.get("error").is_none()
}

/// The host editor's session id (top-level `session_id`/`sessionId`).
fn host_session_id(input: &Value) -> Option<String> {
    ["session_id", "sessionId"].iter().find_map(|key| {
        input
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

/// The workspace/project an `init` or `context` result resolved. MCP results
/// reach hooks as structured content, a JSON string, or text content blocks.
/// The first text block of a content array.
fn first_text_block(blocks: &Value) -> Option<String> {
    blocks
        .as_array()?
        .iter()
        .find_map(|block| block.get("text").and_then(Value::as_str))
        .map(str::to_string)
}

/// The JSON object a tool result carries: structured content, a bare object,
/// or JSON in a text block (a content array, a JSON string, or the MCP
/// `{"content": [...]}` object).
fn response_json(tool_response: &Value) -> Option<Value> {
    let payload = tool_response_payload(tool_response);
    if payload.is_object() {
        let names_ids = ["session_id", "workspace_id", "resolved_scope"]
            .iter()
            .any(|key| payload.get(*key).is_some());
        if names_ids || payload.get("content").is_none() {
            return Some(payload.clone());
        }
    }
    let text = payload
        .as_str()
        .map(str::to_string)
        .or_else(|| first_text_block(payload))
        .or_else(|| payload.get("content").and_then(first_text_block))?;
    serde_json::from_str(&text).ok()
}

/// The ContextStream session id an `init` result names (its top-level
/// `session_id`), if it is a plain token.
fn session_id_from_init_response(tool_response: &Value) -> Option<String> {
    response_json(tool_response)?
        .get("session_id")
        .and_then(Value::as_str)
        .and_then(super::durable_capture::plain_session_id)
}

fn resolved_scope_from_response(tool_response: &Value) -> Option<(Uuid, Option<Uuid>)> {
    let object = response_json(tool_response)?;
    let pick = |key: &str| {
        object
            .get(key)
            .or_else(|| {
                object
                    .get("resolved_scope")
                    .and_then(|scope| scope.get(key))
            })
            .and_then(Value::as_str)
            .and_then(|id| Uuid::parse_str(id).ok())
    };
    Some((pick("workspace_id")?, pick("project_id")))
}

fn completes_session_init(normalized_tool_name: &str, tool_response: &Value) -> bool {
    matches!(normalized_tool_name, "init" | "context") && tool_call_succeeded(tool_response)
}

fn contextstream_post_tool_nudge(
    normalized_tool_name: &str,
    action: &str,
    kind: &str,
    tool_response: &Value,
) -> Option<String> {
    if !tool_call_succeeded(tool_response) {
        return None;
    }
    match normalized_tool_name {
        "session" if matches!(action, "capture_plan" | "update_plan" | "get_plan" | "list_plans") => {
            Some(
                "Plan refs reminder: linked_items use indexed refs. Preferred kinds: doc, diagram, runbook, handoff (kind+id required; snapshots optional). Team cues should be read from context/instructions fields first."
                    .to_string(),
            )
        }
        "entity" if kind == "ticket" && matches!(action, "create" | "update" | "get" | "list") => {
            Some(
                "Ticket refs reminder: linked_items support diagram. Keep links in indexed-ref form (kind+id; optional title/status/updated snapshots). Team cues should be read from context/instructions fields first."
                    .to_string(),
            )
        }
        "entity" if kind == "handoff" && matches!(action, "create" | "update") => Some(
            "Canonical handoff saved. Do not create HANDOFF.md or a paste-only prompt as a substitute. If the user also requested a portable bundle, capsule, or share link, create a session capsule now and return both the handoff and capsule results."
                .to_string(),
        ),
        "skill" if matches!(action, "share" | "list" | "get" | "run") => Some(
            "Team-skill reminder: share uses scope=team|public; prefer governance cues (scope/visibility/workspace/owner) when selecting reusable skills. Treat hooks as reinforcement, not the primary guidance source."
                .to_string(),
        ),
        _ => None,
    }
}

fn absolute_path_from(path: &str, cwd: &str) -> String {
    if Path::new(path).is_absolute() {
        path.to_string()
    } else {
        Path::new(cwd).join(path).to_string_lossy().to_string()
    }
}

fn first_uuid(value: &Value, keys: &[&str]) -> Option<Uuid> {
    keys.iter().find_map(|key| {
        value
            .get(*key)
            .and_then(|v| v.as_str())
            .and_then(|raw| Uuid::parse_str(raw.trim()).ok())
    })
}

fn nested_string(value: &Value, parent: &str, child: &str) -> Option<String> {
    value
        .get(parent)
        .and_then(|v| v.get(child))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn tool_response_payload(tool_response: &Value) -> &Value {
    tool_response
        .get("structuredContent")
        .or_else(|| tool_response.get("structured_content"))
        .filter(|value| !value.is_null())
        .unwrap_or(tool_response)
}

fn normalize_project_folder(path: &str, cwd: &str) -> Option<String> {
    let candidate = absolute_path_from(path, cwd);
    let candidate_path = Path::new(&candidate);
    if candidate_path.is_dir() {
        return std::fs::canonicalize(candidate_path)
            .ok()
            .map(|resolved| resolved.to_string_lossy().to_string())
            .or(Some(candidate));
    }

    candidate_path
        .parent()
        .filter(|parent| parent.is_dir())
        .and_then(|parent| {
            std::fs::canonicalize(parent)
                .ok()
                .map(|resolved| resolved.to_string_lossy().to_string())
                .or_else(|| Some(parent.to_string_lossy().to_string()))
        })
}

pub(crate) fn record_dirty_file(
    checkout_root: &str,
    absolute_path: &str,
) -> std::collections::BTreeMap<String, String> {
    if checkout_root.trim().is_empty() || absolute_path.trim().is_empty() {
        return std::collections::BTreeMap::new();
    }
    let root = std::fs::canonicalize(checkout_root)
        .unwrap_or_else(|_| PathBuf::from(checkout_root))
        .to_string_lossy()
        .to_string();
    super::dirty_drain::record_dirty_paths(&root, &[absolute_path.to_string()])
}

/// Build an authenticated client targeting the resolved project. Used by the
/// hook ingest paths so pushes go through [`ContextStreamClient::ingest_files_from_hook`]
/// (carrying validated checkout + machine provenance) instead of a hand-rolled
/// POST.
pub(crate) fn build_hook_client(cfg: &ProjectConfig) -> ContextStreamClient {
    let config = mcp_types::Config {
        api_url: cfg.api_url.clone(),
        api_key: Some(cfg.api_key.clone()),
        default_workspace_id: Some(cfg.workspace_id),
        default_project_id: Some(cfg.project_id),
        ..Default::default()
    };
    ContextStreamClient::new(config)
}

/// Automatic hook pushes must never request destructive project re-rooting.
///
/// One canonical project can legitimately have active checkouts on multiple
/// machines and in multiple linked worktrees. Checkout provenance lets the
/// backend reconcile those overlays; a local cache miss is not evidence that
/// another checkout's rows are stale or foreign.
pub(crate) fn should_reroot_push(_local_root: &str) -> bool {
    false
}

fn hosted_remote_transport_enabled() -> bool {
    let Some(marker_path) =
        dirs::home_dir().map(|h| h.join(".contextstream").join("setup-transport-mode"))
    else {
        return false;
    };

    std::fs::read_to_string(marker_path)
        .ok()
        .map(|value| value.trim().eq_ignore_ascii_case("remote"))
        .unwrap_or(false)
}

fn current_binary_path() -> Option<String> {
    let path = std::env::current_exe().ok()?;
    let raw = path.to_string_lossy();
    if raw.is_empty() {
        return None;
    }

    if let Some(stripped) = raw.strip_suffix(" (deleted)") {
        return Path::new(stripped).exists().then(|| stripped.to_string());
    }

    Some(raw.to_string())
}

#[derive(Debug, Clone)]
struct LocalIndexScope {
    folder_path: String,
    workspace_id: Option<Uuid>,
    workspace_name: Option<String>,
    project_id: Option<Uuid>,
    project_name: Option<String>,
}

fn consistent_uuid_source(
    value: &Value,
    keys: &[&str],
    nested_parent: &str,
) -> Option<Option<Uuid>> {
    let mut observed = Vec::new();
    for key in keys {
        if let Some(raw_value) = value.get(*key).filter(|value| !value.is_null()) {
            let raw = raw_value.as_str()?;
            observed.push(Uuid::parse_str(raw.trim()).ok()?);
        }
    }
    if let Some(raw_value) = value
        .get(nested_parent)
        .and_then(|nested| nested.get("id"))
        .filter(|value| !value.is_null())
    {
        let raw = raw_value.as_str()?;
        observed.push(Uuid::parse_str(raw.trim()).ok()?);
    }
    let first = observed.first().copied();
    observed
        .iter()
        .all(|candidate| Some(*candidate) == first)
        .then_some(first)
}

fn optional_source_agrees(authoritative: Uuid, candidate: Option<Uuid>) -> bool {
    candidate.is_none_or(|candidate| candidate == authoritative)
}

fn read_verified_local_scope(folder_path: &str) -> Option<LocalIndexScope> {
    let root = std::fs::canonicalize(folder_path).ok()?;
    let content = std::fs::read_to_string(root.join(".contextstream/config.json")).ok()?;
    let config: Value = serde_json::from_str(&content).ok()?;
    let project_id = first_uuid(&config, &["project_id", "projectId"])?;
    let workspace_id = first_uuid(&config, &["workspace_id", "workspaceId"])?;
    if checkout_binding_workspace(root.to_string_lossy().as_ref(), project_id) != Some(workspace_id)
    {
        return None;
    }
    Some(LocalIndexScope {
        folder_path: root.to_string_lossy().into_owned(),
        workspace_id: Some(workspace_id),
        workspace_name: config
            .get("workspace_name")
            .and_then(Value::as_str)
            .map(str::to_string),
        project_id: Some(project_id),
        project_name: config
            .get("project_name")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn resolve_local_index_scope(
    tool_input: &Value,
    tool_response: &Value,
    cwd: &str,
) -> Option<LocalIndexScope> {
    let path_hint = tool_input
        .get("path")
        .or_else(|| tool_input.get("folder_path"))
        .or_else(|| tool_input.get("folderPath"))
        .and_then(|v| v.as_str())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(cwd);
    let folder_path = normalize_project_folder(path_hint, cwd)?;
    let response = tool_response_payload(tool_response);
    for response_source in [tool_response, response] {
        if let Some(response_path) = response_source
            .get("path")
            .or_else(|| response_source.get("folder_path"))
            .or_else(|| response_source.get("folderPath"))
            .and_then(Value::as_str)
            .filter(|path| !path.trim().is_empty())
        {
            if normalize_project_folder(response_path, cwd).as_deref() != Some(folder_path.as_str())
            {
                return None;
            }
        }
    }

    // The checkout-bound local config is authoritative. Tool input may select
    // the folder, but IDs in input only confirm an existing binding. Response
    // IDs are also authoritative and every non-empty alias/source must agree.
    // This prevents a cross-project read or a stale editor environment from
    // rewriting the checkout mapping.
    let mut scope = read_verified_local_scope(&folder_path)?;
    let local_workspace_id = scope.workspace_id?;
    let local_project_id = scope.project_id?;
    let input_workspace_id = consistent_uuid_source(
        tool_input,
        &[
            "workspace_id",
            "workspaceId",
            "resolved_workspace_id",
            "resolvedWorkspaceId",
        ],
        "workspace",
    )?;
    let response_workspace_id = consistent_uuid_source(
        response,
        &[
            "workspace_id",
            "workspaceId",
            "resolved_workspace_id",
            "resolvedWorkspaceId",
        ],
        "workspace",
    )?;
    let response_envelope_workspace_id = consistent_uuid_source(
        tool_response,
        &[
            "workspace_id",
            "workspaceId",
            "resolved_workspace_id",
            "resolvedWorkspaceId",
        ],
        "workspace",
    )?;
    let input_project_id = consistent_uuid_source(
        tool_input,
        &[
            "project_id",
            "projectId",
            "resolved_project_id",
            "resolvedProjectId",
        ],
        "project",
    )?;
    let response_project_id = consistent_uuid_source(
        response,
        &[
            "project_id",
            "projectId",
            "resolved_project_id",
            "resolvedProjectId",
        ],
        "project",
    )?;
    let response_envelope_project_id = consistent_uuid_source(
        tool_response,
        &[
            "project_id",
            "projectId",
            "resolved_project_id",
            "resolvedProjectId",
        ],
        "project",
    )?;
    if !optional_source_agrees(local_workspace_id, input_workspace_id)
        || !optional_source_agrees(local_workspace_id, response_workspace_id)
        || !optional_source_agrees(local_workspace_id, response_envelope_workspace_id)
        || !optional_source_agrees(local_project_id, input_project_id)
        || !optional_source_agrees(local_project_id, response_project_id)
        || !optional_source_agrees(local_project_id, response_envelope_project_id)
    {
        return None;
    }

    // Names are display-only. Prefer names returned by the successful tool,
    // but never use them to infer IDs or scope.
    scope.workspace_name = response
        .get("workspace_name")
        .or_else(|| response.get("workspaceName"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| nested_string(response, "workspace", "name"))
        .or(scope.workspace_name);
    scope.project_name = response
        .get("project_name")
        .or_else(|| response.get("projectName"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| nested_string(response, "project", "name"))
        .or(scope.project_name);
    Some(scope)
}

fn observes_local_binding(normalized_tool_name: &str, action: &str) -> bool {
    normalized_tool_name == "init"
        || (normalized_tool_name == "project" && matches!(action, "index" | "ingest_local"))
}

async fn persist_local_scope(scope: &LocalIndexScope) {
    let Some(workspace_id) = scope.workspace_id else {
        return;
    };

    let workspace_name = scope
        .workspace_name
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("ContextStream");

    persist_folder_mapping(
        &scope.folder_path,
        workspace_id,
        workspace_name,
        scope.project_id,
        scope.project_name.as_deref(),
    )
    .await;
}

fn should_start_local_init_index(tool_input: &Value, scope: &LocalIndexScope) -> bool {
    if !Path::new(&scope.folder_path).is_dir() {
        return false;
    }

    let explicit_auto_index = tool_input
        .get("auto_index")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let indexed = ContextStreamClient::is_project_indexed(&scope.folder_path);
    let is_stale = ContextStreamClient::local_index_age_hours(&scope.folder_path)
        .map(|hours| hours >= STALE_THRESHOLD_HOURS)
        .unwrap_or(false);

    explicit_auto_index || !indexed || is_stale
}

fn start_local_background_index(scope: &LocalIndexScope, cwd: &str) {
    if !Path::new(&scope.folder_path).is_dir() {
        return;
    }
    // P0 ingestion-containment: never auto-index an over-broad / sensitive root
    // ($HOME, home ancestors, `/`, `.ssh`/`.aws`/...). The spawned `index`
    // subprocess re-checks this, but skip early to avoid the process churn.
    // The operator env opt-in (CONTEXTSTREAM_ALLOW_BROAD_INGEST=1) bypasses it.
    match mcp_client::validate_ingest_root(
        Path::new(&scope.folder_path),
        &mcp_client::IngestRootOptions::from_env(),
    ) {
        Ok(assessment) => {
            for warning in assessment.warnings {
                tracing::warn!(
                    "local background index root warning for {}: {}",
                    scope.folder_path,
                    warning
                );
            }
        }
        Err(rejection) => {
            tracing::debug!("skipping local background index: {}", rejection.message());
            return;
        }
    }
    if scope.project_id.is_none()
        && !Path::new(&scope.folder_path)
            .join(".contextstream")
            .join("config.json")
            .exists()
    {
        tracing::debug!(
            "skipping local background index for {} because no local project scope is available",
            scope.folder_path
        );
        return;
    }

    let binary = current_binary_path().unwrap_or_else(|| "contextstream-mcp".to_string());
    let config = super::common::load_config(cwd);

    let mut command = Command::new(binary);
    command
        .arg("index")
        .arg("--path")
        .arg(&scope.folder_path)
        .arg("--background")
        .current_dir(&scope.folder_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    if !config.api_key.is_empty() {
        command.env("CONTEXTSTREAM_API_KEY", config.api_key);
    }
    if !config.api_url.is_empty() {
        command.env("CONTEXTSTREAM_API_URL", config.api_url);
    }
    if let Some(workspace_id) = scope.workspace_id {
        command.env("CONTEXTSTREAM_WORKSPACE_ID", workspace_id.to_string());
    }
    if let Some(project_id) = scope.project_id {
        command.env("CONTEXTSTREAM_PROJECT_ID", project_id.to_string());
    }

    if let Err(error) = command.spawn() {
        tracing::debug!(
            "failed to start local background index bridge for {}: {}",
            scope.folder_path,
            error
        );
    }
}

/// Install/refresh the managed git hooks for `folder_path` via a detached local
/// process. This is the reliable local trigger for git capture: in hosted-remote
/// mode the `init` tool runs on the gateway, so the local install must be driven
/// from a local hook. The `git-hooks` subcommand resolves the git root, honors
/// the capture kill-switch / per-repo policy, and no-ops for non-git folders.
fn start_local_git_hooks_install(folder_path: &str) {
    if folder_path.trim().is_empty() {
        return;
    }
    let binary = current_binary_path().unwrap_or_else(|| "contextstream-mcp".to_string());
    let mut command = Command::new(binary);
    command
        .arg("git-hooks")
        .arg("--path")
        .arg(folder_path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    if let Err(error) = command.spawn() {
        tracing::debug!(
            "failed to start local git-hooks install for {}: {}",
            folder_path,
            error
        );
    }
}

/// Extract file path from hook input, handling different editor formats.
fn extract_file_path(input: &Value) -> Option<String> {
    // Claude Code: tool_input.file_path or tool_input.notebook_path
    if let Some(tool_input) = input.get("tool_input") {
        if let Some(fp) = tool_input.get("file_path").and_then(|v| v.as_str()) {
            return Some(fp.to_string());
        }
        if let Some(fp) = tool_input.get("notebook_path").and_then(|v| v.as_str()) {
            return Some(fp.to_string());
        }
        if let Some(fp) = tool_input.get("path").and_then(|v| v.as_str()) {
            return Some(fp.to_string());
        }
    }
    // Cursor: parameters.path or parameters.file_path
    if let Some(params) = input.get("parameters") {
        if let Some(fp) = params
            .get("path")
            .or_else(|| params.get("file_path"))
            .and_then(|v| v.as_str())
        {
            return Some(fp.to_string());
        }
    }
    // Cline/Roo/Kilo: toolParameters.path
    if let Some(tp) = input.get("toolParameters") {
        if let Some(fp) = tp.get("path").and_then(|v| v.as_str()) {
            return Some(fp.to_string());
        }
    }
    // Windsurf: direct file_path
    if let Some(fp) = input.get("file_path").and_then(|v| v.as_str()) {
        return Some(fp.to_string());
    }
    // Windsurf: nested tool_info.file_path
    if let Some(fp) = input
        .get("tool_info")
        .and_then(|v| v.get("file_path"))
        .and_then(|v| v.as_str())
    {
        return Some(fp.to_string());
    }
    None
}

/// Extract working directory from hook input.
fn extract_cwd(input: &Value) -> String {
    input
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            input
                .get("tool_info")
                .and_then(|v| v.get("cwd"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .or_else(|| {
            input
                .get("workspace_roots")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .or_else(|| {
            input
                .get("workspaceRoots")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .and_then(|p| p.to_str().map(|s| s.to_string()))
        })
        .unwrap_or_default()
}

fn project_root_from_path_hint(path_hint: &str) -> PathBuf {
    let candidate = Path::new(path_hint);
    if candidate.is_dir() {
        return candidate.to_path_buf();
    }
    candidate
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| candidate.to_path_buf())
}

fn parse_project_binding_from_config(
    config_path: &Path,
    project_root: &Path,
) -> Option<(Uuid, Uuid)> {
    let content = std::fs::read_to_string(config_path).ok()?;
    let config: Value = serde_json::from_str(&content).ok()?;
    let project_id = config
        .get("project_id")
        .and_then(|v| v.as_str())
        .and_then(|raw| Uuid::parse_str(raw.trim()).ok())?;
    let workspace_id =
        checkout_binding_workspace(project_root.to_string_lossy().as_ref(), project_id)?;
    Some((project_id, workspace_id))
}

/// Handle the PostToolUse hook.
pub async fn handle() -> Result<()> {
    let input = read_stdin_json()?;
    let tool_name = extract_tool_name(&input);
    let tool_input = extract_tool_input(&input);
    let tool_response = extract_tool_response(&input);
    let mcp_server_name = extract_mcp_server_name(&input);
    let normalized_tool_name = normalize_contextstream_tool_name(&tool_name);
    let cwd = extract_cwd(&input);

    if is_contextstream_server_call(&mcp_server_name, &tool_name) {
        // Record successful initialization before optional indexing/routing
        // work, whose early returns must not lose this session's receipt.
        if completes_session_init(&normalized_tool_name, &tool_response) {
            super::prompt_state::clear_session_init_required(&cwd, &input);
            // Remember the scope the server resolved for this session, so
            // plans and docs saved later land where the agent's own calls go,
            // and the session id init returned, which a resume prompt names.
            if let (Some(host_session), Some((workspace_id, project_id))) = (
                // Cursor names its conversation `conversation_id`; recording under
                // it lets a git hook there find the id `init` returned.
                super::common::host_session_key(&input),
                resolved_scope_from_response(&tool_response),
            ) {
                let api_session_id = (normalized_tool_name == "init")
                    .then(|| session_id_from_init_response(&tool_response))
                    .flatten();
                super::durable_capture::record_session_scope(
                    &host_session,
                    &cwd,
                    workspace_id,
                    project_id,
                    api_session_id.as_deref(),
                );
                // Docs written before this init/context were promised a mirror.
                if hook_feature_enabled("CONTEXTSTREAM_DOC_MIRROR_ENABLED") {
                    let _ = super::durable_capture::mirror_pending_docs(&host_session).await;
                }
            }
        }
        if tool_call_succeeded(&tool_response)
            && (normalized_tool_name == "context"
                || (normalized_tool_name == "session"
                    && contextstream_action(&tool_input) == "ground"))
        {
            super::prompt_state::clear_context_required(&cwd);
        }
    }

    // An approved plan-mode plan is saved to ContextStream. This is not a file
    // index push, so the post-write switch below does not apply to it.
    if tool_name == "ExitPlanMode" {
        return capture_exit_plan_mode(&input, &cwd, &tool_input, &tool_response).await;
    }

    // Disabling file indexing must not disable initialization receipts while
    // the corresponding PreToolUse gate remains enabled.
    if std::env::var("CONTEXTSTREAM_POSTWRITE_ENABLED")
        .map(|v| v == "false")
        .unwrap_or(false)
    {
        write_stdout_json(&HookOutput::empty())?;
        return Ok(());
    }

    if is_contextstream_server_call(&mcp_server_name, &tool_name) {
        let action = contextstream_action(&tool_input);
        // Observing a ContextStream read must never mutate local routing.
        // Likewise, failed calls are not evidence that an input scope is valid.
        // Only successful init/index operations may refresh an already verified
        // checkout binding (persist_folder_mapping itself is refresh-only).
        if tool_call_succeeded(&tool_response)
            && observes_local_binding(&normalized_tool_name, &action)
        {
            if let Some(scope) = resolve_local_index_scope(&tool_input, &tool_response, &cwd) {
                let validated_config =
                    find_project_config(&scope.folder_path, &cwd).filter(|cfg| {
                        Some(cfg.project_id) == scope.project_id
                            && Some(cfg.workspace_id) == scope.workspace_id
                            && cfg.project_root.as_deref() == Some(scope.folder_path.as_str())
                    });
                let api_scope_current = if let Some(config) = validated_config.as_ref() {
                    let client = build_hook_client(config);
                    hook_project_scope_is_current(&client, config).await
                } else {
                    false
                };
                if !api_scope_current {
                    write_stdout_json(&HookOutput::empty())?;
                    return Ok(());
                }

                persist_local_scope(&scope).await;

                // Local git capture: ensure the managed git hooks are installed
                // only after a successful init whose response/input/local scope
                // agree and whose current API ownership was freshly verified.
                if normalized_tool_name == "init" {
                    start_local_git_hooks_install(&scope.folder_path);
                }

                if hosted_remote_transport_enabled() {
                    if normalized_tool_name == "init"
                        && should_start_local_init_index(&tool_input, &scope)
                    {
                        start_local_background_index(&scope, &cwd);
                        write_stdout_json(&HookOutput::empty())?;
                        return Ok(());
                    }

                    if matches!(action.as_str(), "index" | "ingest_local") {
                        start_local_background_index(&scope, &cwd);
                        write_stdout_json(&HookOutput::empty())?;
                        return Ok(());
                    }
                }

                // Local ingest paths maintain their own committed, complete
                // freshness ledger. Merely observing a successful tool response
                // must not advance indexed_at for a pending/partial operation.
                if normalized_tool_name == "project"
                    && matches!(action.as_str(), "index" | "ingest_local")
                {
                    write_stdout_json(&HookOutput::empty())?;
                    return Ok(());
                }
            }
        }
    }

    if is_contextstream_server_call(&mcp_server_name, &tool_name) {
        let action = contextstream_action(&tool_input);
        let kind = contextstream_kind(&tool_input);
        if let Some(msg) =
            contextstream_post_tool_nudge(&normalized_tool_name, &action, &kind, &tool_response)
        {
            // Cursor injects post-tool guidance via `postToolUse.additional_context`
            // (snake_case, top-level); Claude/others use the HookOutput schema.
            super::write_context_for_input(&input, msg)?;
            return Ok(());
        }
    }

    // Only process write/edit-like tools (Codex edits arrive as `apply_patch`).
    let is_codex_patch = tool_name.eq_ignore_ascii_case("apply_patch");
    if !is_codex_patch
        && !matches!(
            tool_name.as_str(),
            "Edit" | "MultiEdit" | "Write" | "NotebookEdit"
        )
    {
        write_stdout_json(&HookOutput::empty())?;
        return Ok(());
    }

    // Extract file paths (multi-editor support; a patch can touch several).
    let file_paths = if is_codex_patch {
        super::durable_paths::write_target_paths("apply_patch", &tool_input)
    } else {
        extract_file_path(&input).into_iter().collect()
    };
    if file_paths.is_empty() {
        write_stdout_json(&HookOutput::empty())?;
        return Ok(());
    }
    let absolute_paths: Vec<String> = file_paths
        .iter()
        .map(|file_path| {
            if Path::new(file_path).is_absolute() {
                file_path.clone()
            } else {
                Path::new(&cwd)
                    .join(file_path)
                    .to_string_lossy()
                    .to_string()
            }
        })
        .collect();

    let index_files = async {
        let mut reached_drain = false;
        for absolute_path in &absolute_paths {
            reached_drain |= index_written_file(&cwd, absolute_path).await;
        }
        reached_drain
    };
    let host_session = host_session_id(&input);
    let (mirror_notes, reached_drain) = tokio::join!(
        mirror_repo_docs(&absolute_paths, host_session.as_deref()),
        index_files
    );

    // PostToolUse-tail drain: flush any edits recorded this turn that the
    // single-file push didn't commit (e.g. a MultiEdit fan-out, or a push that
    // returned 202). Cooldown-gated, so it's a cheap no-op on most calls and
    // never blocks the hook beyond the deadline.
    if reached_drain {
        super::dirty_drain::drain_best_effort(std::time::Duration::from_millis(1200)).await;
    }

    if mirror_notes.is_empty() {
        write_stdout_json(&HookOutput::empty())?;
    } else {
        super::write_context_for_input(&input, mirror_notes.join("\n"))?;
    }
    Ok(())
}

/// Whether a hook feature is enabled by an opt-out environment variable.
fn hook_feature_enabled(variable: &str) -> bool {
    !std::env::var(variable)
        .map(|value| matches!(value.trim().to_ascii_lowercase().as_str(), "false" | "0"))
        .unwrap_or(false)
}

/// Mirror written repository runbooks/ADRs/RFCs/notes to ContextStream docs
/// and return one agent-facing note per mirrored file.
async fn mirror_repo_docs(absolute_paths: &[String], host_session: Option<&str>) -> Vec<String> {
    if !hook_feature_enabled("CONTEXTSTREAM_DOC_MIRROR_ENABLED") {
        return Vec::new();
    }
    let mut notes = Vec::new();
    for absolute_path in absolute_paths {
        let Some(super::durable_paths::DurableWrite::RepoDoc { doc_type }) =
            super::durable_paths::classify(absolute_path, None)
        else {
            continue;
        };
        if !super::durable_capture::doc_mirror_available(Path::new(absolute_path), host_session)
            .await
        {
            // The agent wrote the doc before init/context resolved this
            // session's scope; mirror it as soon as one does.
            if let Some(host_session) = host_session {
                super::durable_capture::queue_pending_doc(host_session, Path::new(absolute_path));
            }
            continue;
        }
        if let Some(mirror) = super::durable_capture::mirror_repo_doc(
            Path::new(absolute_path),
            doc_type,
            host_session,
        )
        .await
        {
            notes.push(mirror.message());
        }
    }
    notes
}

/// The approved plan's markdown from a PostToolUse(ExitPlanMode) payload:
/// `tool_input.plan`, the response's `plan`, or the plan file itself.
fn approved_plan_markdown(
    tool_input: &Value,
    tool_response: &Value,
) -> Option<(String, Option<String>)> {
    let plan_path = ["planFilePath", "plan_file_path"]
        .iter()
        .find_map(|key| tool_input.get(*key).and_then(Value::as_str))
        .or_else(|| {
            ["filePath", "planFilePath"]
                .iter()
                .find_map(|key| tool_response.get(*key).and_then(Value::as_str))
        })
        .map(str::to_string);
    let inline = tool_input
        .get("plan")
        .or_else(|| tool_response.get("plan"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|plan| !plan.trim().is_empty());
    let markdown = inline.or_else(|| {
        plan_path
            .as_deref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .filter(|plan| !plan.trim().is_empty())
    })?;
    Some((markdown, plan_path))
}

/// Agent instruction when the approved plan could not be saved automatically.
const PLAN_CAPTURE_FAILED: &str = "ContextStream could not save the approved plan automatically (no credentials, the folder is not linked to a workspace, or the API did not answer). Save it now with mcp__contextstream__session(action=\"capture_plan\", title=\"...\", description=\"...\", steps=[...], create_tasks=true).";

/// Save the plan the user approved in Claude Code plan mode to ContextStream.
async fn capture_exit_plan_mode(
    input: &Value,
    cwd: &str,
    tool_input: &Value,
    tool_response: &Value,
) -> Result<()> {
    let from_subagent = tool_response
        .get("isAgent")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if from_subagent || !hook_feature_enabled("CONTEXTSTREAM_PLAN_CAPTURE_ENABLED") {
        write_stdout_json(&HookOutput::empty())?;
        return Ok(());
    }
    let Some((markdown, plan_path)) = approved_plan_markdown(tool_input, tool_response) else {
        write_stdout_json(&HookOutput::empty())?;
        return Ok(());
    };
    let message = match super::durable_capture::capture_approved_plan(
        cwd,
        &markdown,
        plan_path.as_deref(),
        host_session_id(input).as_deref(),
    )
    .await
    {
        Some(capture) => capture.message(),
        // PreToolUse told the agent the plan would be saved for it; say
        // plainly that it was not, so the plan is not lost.
        None => PLAN_CAPTURE_FAILED.to_string(),
    };
    super::write_context_for_input(input, message)?;
    Ok(())
}

/// Push one written file to the project index (the checkout-scoped,
/// ownership-verified targeted ingest). Every failure is silent. Returns
/// whether the push ran to the end, where the turn's dirty-file drain follows.
async fn index_written_file(cwd: &str, absolute_path: &str) -> bool {
    let cwd = cwd.to_string();
    let absolute_path = absolute_path.to_string();

    // Resolve the checkout before recording. Keying by raw cwd fragments one
    // checkout into unreconcilable subdirectory roots.
    let project_config = find_project_config(&absolute_path, &cwd);
    let ledger_root = project_config
        .as_ref()
        .and_then(|config| config.project_root.as_deref())
        .unwrap_or(cwd.as_str())
        .to_string();

    // Capture the exact opaque edit version before reading bytes or sending a
    // request. A newer edit during the upload receives a different version and
    // therefore cannot be cleared by this completion.
    let submitted_versions = record_dirty_file(&ledger_root, &absolute_path);

    if !should_index(&absolute_path) {
        return false;
    }

    if let Some(config) = project_config {
        // Only push file CONTENT when a project root was resolved from an ancestor
        // .contextstream/config.json. Without a root, the relative path below falls
        // back to the file's ABSOLUTE path, so a stray CONTEXTSTREAM_PROJECT_ID (no
        // ancestor config) would ingest files edited in ANY folder into that
        // project — a cross-folder leak. A real project checkout always resolves a
        // root here.
        if let Some(tracked_root) = config.project_root.clone() {
            if submitted_versions.is_empty() {
                return false;
            }
            if mcp_client::validate_ingest_root(
                Path::new(&tracked_root),
                &mcp_client::IngestRootOptions::from_env(),
            )
            .is_err()
            {
                return false;
            }
            let checkout_guard = match ContextStreamClient::checkout_guard_for_scope(
                &tracked_root,
                config.project_id,
                config.workspace_id,
            ) {
                Ok(guard) => guard,
                Err(_) => {
                    return false;
                }
            };
            if super::dirty_drain::has_pending_submission_for_scope(
                &ledger_root,
                config.project_id,
                config.workspace_id,
            ) {
                return false;
            }
            let Some(reservation) = super::dirty_drain::reserve_pending_submission(
                &ledger_root,
                config.project_id,
                config.workspace_id,
                &submitted_versions,
                super::dirty_drain::DRAIN_ORIGIN,
                super::dirty_drain::PendingSubmissionMode::Targeted,
                None,
                checkout_guard.as_deref(),
                true,
            ) else {
                return false;
            };

            let (files, deleted_paths) = match ContextStreamClient::targeted_text_file_decision(
                &tracked_root,
                &absolute_path,
            ) {
                TargetedFileDecision::Upload(payload) => (vec![payload], Vec::new()),
                TargetedFileDecision::Delete(relative) => (Vec::new(), vec![relative]),
                TargetedFileDecision::Reject => {
                    let _ =
                        super::dirty_drain::cancel_pending_submission(&ledger_root, &reservation);
                    return false;
                }
            };

            // Payload construction reads local bytes. Re-resolve the checkout
            // afterwards to close a config/symlink TOCTOU window. The client
            // then performs an uncached API ownership check immediately before
            // sending the prepared payload.
            let Some(fresh_config) = find_project_config(&absolute_path, &cwd).filter(|fresh| {
                fresh.project_id == config.project_id
                    && fresh.workspace_id == config.workspace_id
                    && fresh.project_root.as_deref() == Some(tracked_root.as_str())
            }) else {
                let _ = super::dirty_drain::cancel_pending_submission(&ledger_root, &reservation);
                return false;
            };
            let client = build_hook_client(&fresh_config);

            // Automatic edits are checkout-scoped and never replace another
            // machine/worktree overlay.
            let reroot = should_reroot_push(&tracked_root);
            if let Ok(outcome) = client
                .ingest_files_from_hook(
                    fresh_config.project_id,
                    fresh_config.workspace_id,
                    files,
                    deleted_paths,
                    false,
                    None,
                    fresh_config.project_root.as_deref(),
                    reroot,
                    false,
                )
                .await
            {
                if outcome.committed {
                    if super::dirty_drain::finalize_pending_submission(
                        &ledger_root,
                        &reservation,
                        &[],
                        true,
                        Some(true),
                    ) {
                        let _ = super::dirty_drain::reconcile_watch_submissions(
                            &client,
                            &ledger_root,
                            fresh_config.project_id,
                            fresh_config.workspace_id,
                        )
                        .await;
                    }
                } else if !outcome.job_ids.is_empty() {
                    let _ = super::dirty_drain::finalize_pending_submission(
                        &ledger_root,
                        &reservation,
                        &outcome.job_ids,
                        false,
                        Some(true),
                    );
                } else {
                    let _ =
                        super::dirty_drain::cancel_pending_submission(&ledger_root, &reservation);
                }
            } else {
                let _ = super::dirty_drain::cancel_pending_submission(&ledger_root, &reservation);
            }
        }
    }
    true
}

// ============================================================================
// Config Loading
// ============================================================================

pub(crate) struct ProjectConfig {
    pub(crate) api_key: String,
    pub(crate) api_url: String,
    pub(crate) project_id: Uuid,
    pub(crate) workspace_id: Uuid,
    pub(crate) project_root: Option<String>,
}

pub(crate) async fn hook_project_scope_is_current(
    client: &ContextStreamClient,
    config: &ProjectConfig,
) -> bool {
    match client.get_project_fresh(config.project_id).await {
        Ok(project) => project.workspace_id == Some(config.workspace_id),
        Err(error) => {
            tracing::warn!(
                project_id = %config.project_id,
                workspace_id = %config.workspace_id,
                error = %error,
                "hook ingest skipped because project validation failed"
            );
            false
        }
    }
}

/// Find project config by walking up from a file path.
/// Checks .contextstream/config.json and .mcp.json for credentials.
pub(crate) fn find_project_config(file_path: &str, cwd: &str) -> Option<ProjectConfig> {
    let mut api_key = std::env::var("CONTEXTSTREAM_API_KEY").unwrap_or_default();
    let mut api_url = std::env::var("CONTEXTSTREAM_API_URL")
        .unwrap_or_else(|_| "https://api.contextstream.io".to_string());
    let env_project_id = std::env::var("CONTEXTSTREAM_PROJECT_ID")
        .ok()
        .and_then(|raw| Uuid::parse_str(raw.trim()).ok());
    let env_workspace_id = std::env::var("CONTEXTSTREAM_WORKSPACE_ID")
        .ok()
        .and_then(|raw| Uuid::parse_str(raw.trim()).ok());
    let mut validated_binding: Option<(Uuid, Uuid, String)> = None;

    // Walk up from the supplied path looking for .contextstream/config.json.
    let mut dir = project_root_from_path_hint(file_path);
    loop {
        let config_path = dir.join(".contextstream").join("config.json");
        if config_path.exists() {
            // The nearest config is an explicit scope boundary. An invalid or
            // copied child config must not fall through to a valid ancestor
            // and upload nested-checkout files into the ancestor project.
            let (config_project_id, config_workspace_id) =
                parse_project_binding_from_config(&config_path, &dir)?;
            validated_binding = Some((
                config_project_id,
                config_workspace_id,
                dir.to_string_lossy().to_string(),
            ));
            break;
        }
        if !dir.pop() {
            break;
        }
    }

    let (project_id, workspace_id, project_root) = validated_binding?;
    // Environment scope may confirm the local binding, but must never
    // override it for a content upload. A stale editor/global environment
    // combined with a valid checkout config would otherwise route this
    // checkout's file bytes into another project's index.
    if env_project_id.is_some_and(|env_id| env_id != project_id) {
        return None;
    }
    if env_workspace_id.is_some_and(|env_id| env_id != workspace_id) {
        return None;
    }

    // If no API key from env, walk up looking for .mcp.json
    if api_key.is_empty() {
        let mut search_dir = std::path::PathBuf::from(cwd);
        for _ in 0..10 {
            let mcp_path = search_dir.join(".mcp.json");
            if let Some((key, url)) = read_mcp_json_credentials(&mcp_path) {
                api_key = key;
                if let Some(u) = url {
                    api_url = u;
                }
                break;
            }
            if !search_dir.pop() {
                break;
            }
        }
    }

    // Also check home directory
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

    if api_key.is_empty() {
        return None;
    }

    Some(ProjectConfig {
        api_key,
        api_url,
        project_id,
        workspace_id,
        project_root: Some(project_root),
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn write_repo_config(repo_root: &Path, project_id: Uuid) -> Uuid {
        std::fs::create_dir_all(repo_root.join(".git")).unwrap();
        let fingerprint =
            mcp_session::checkout_identity::ensure_repository_fingerprint(repo_root).unwrap();
        let config_dir = repo_root.join(".contextstream");
        std::fs::create_dir_all(&config_dir).unwrap();
        let checkout_root = std::fs::canonicalize(repo_root)
            .unwrap_or_else(|_| repo_root.to_path_buf())
            .to_string_lossy()
            .into_owned();
        let workspace_id = Uuid::new_v4();
        std::fs::write(
            config_dir.join("config.json"),
            serde_json::json!({
                "project_id": project_id.to_string(),
                "workspace_id": workspace_id.to_string(),
                "checkout_root": checkout_root,
                "repository_fingerprint": fingerprint.as_str(),
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            repo_root.join(".mcp.json"),
            serde_json::json!({
                "mcpServers": {
                    "contextstream": {
                        "env": {
                            "CONTEXTSTREAM_API_KEY": "test-api-key",
                            "CONTEXTSTREAM_API_URL": "https://api.contextstream.io"
                        }
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        workspace_id
    }

    #[test]
    fn automatic_hook_push_never_reroots_other_machine_or_worktree_rows() {
        assert!(!should_reroot_push("/home/u/proj"));
        assert!(!should_reroot_push("/home/u/proj-worktree"));
        assert!(!should_reroot_push("C:\\Users\\u\\proj"));
    }

    #[test]
    fn normalize_contextstream_tool_name_strips_mcp_prefix() {
        assert_eq!(
            normalize_contextstream_tool_name("mcp__contextstream__project"),
            "project"
        );
    }

    #[test]
    fn session_init_requires_a_successful_init_or_context_result() {
        let success =
            serde_json::json!({"content": [{"type": "text", "text": "ready"}], "isError": false});
        for tool in ["init", "context"] {
            assert!(completes_session_init(tool, &success));
            assert!(!completes_session_init(tool, &serde_json::json!({})));
            assert!(!completes_session_init(
                tool,
                &serde_json::json!({"isError": true, "content": ["failed"]})
            ));
            assert!(!completes_session_init(
                tool,
                &serde_json::json!({"error": "failed"})
            ));
        }
        for tool in ["search", "project", "session", "entity"] {
            assert!(!completes_session_init(tool, &success));
        }
    }

    #[test]
    fn successful_handoff_create_reinforces_capsule_pairing_without_local_files() {
        let message = contextstream_post_tool_nudge(
            "entity",
            "create",
            "handoff",
            &serde_json::json!({"id": "handoff-1"}),
        )
        .expect("handoff nudge");
        assert!(message.contains("Canonical handoff saved"));
        assert!(message.contains("HANDOFF.md"));
        assert!(message.contains("portable bundle"));
        assert!(message.contains("both the handoff and capsule"));
    }

    #[test]
    fn find_project_config_reads_directory_root() {
        let _guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous_project_id = std::env::var_os("CONTEXTSTREAM_PROJECT_ID");
        let previous_workspace_id = std::env::var_os("CONTEXTSTREAM_WORKSPACE_ID");
        std::env::remove_var("CONTEXTSTREAM_PROJECT_ID");
        std::env::remove_var("CONTEXTSTREAM_WORKSPACE_ID");
        let temp = tempdir().unwrap();
        let project_id = Uuid::new_v4();
        write_repo_config(temp.path(), project_id);

        let config =
            find_project_config(temp.path().to_str().unwrap(), temp.path().to_str().unwrap())
                .expect("project config");

        assert_eq!(config.project_id, project_id);
        assert_eq!(
            config.project_root.as_deref(),
            Some(temp.path().to_str().unwrap())
        );
        if let Some(value) = previous_project_id {
            std::env::set_var("CONTEXTSTREAM_PROJECT_ID", value);
        }
        if let Some(value) = previous_workspace_id {
            std::env::set_var("CONTEXTSTREAM_WORKSPACE_ID", value);
        }
    }

    #[test]
    fn find_project_config_rejects_copied_checkout_binding() {
        let source = tempdir().unwrap();
        let copied = tempdir().unwrap();
        let project_id = Uuid::new_v4();
        write_repo_config(copied.path(), project_id);

        let config_path = copied.path().join(".contextstream/config.json");
        let mut config: Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).expect("read config"))
                .expect("parse config");
        config["checkout_root"] = Value::String(
            std::fs::canonicalize(source.path())
                .expect("canonical source")
                .to_string_lossy()
                .into_owned(),
        );
        std::fs::write(&config_path, serde_json::to_string(&config).unwrap()).unwrap();

        assert!(find_project_config(
            copied.path().to_str().unwrap(),
            copied.path().to_str().unwrap()
        )
        .is_none());
    }

    #[test]
    fn find_project_config_does_not_inherit_valid_ancestor_after_invalid_child() {
        let parent = tempdir().unwrap();
        let child = parent.path().join("child");
        let nested = child.join("src");
        std::fs::create_dir_all(&nested).unwrap();
        write_repo_config(parent.path(), Uuid::new_v4());
        write_repo_config(&child, Uuid::new_v4());

        let child_config_path = child.join(".contextstream/config.json");
        let mut child_config: Value = serde_json::from_str(
            &std::fs::read_to_string(&child_config_path).expect("read child config"),
        )
        .expect("parse child config");
        child_config["checkout_root"] = Value::String("/copied/from/elsewhere".to_string());
        std::fs::write(
            &child_config_path,
            serde_json::to_string(&child_config).unwrap(),
        )
        .unwrap();

        assert!(find_project_config(nested.to_str().unwrap(), nested.to_str().unwrap()).is_none());
    }

    #[test]
    fn find_project_config_rejects_mismatched_environment_project() {
        let _guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous_project_id = std::env::var_os("CONTEXTSTREAM_PROJECT_ID");
        let temp = tempdir().unwrap();
        write_repo_config(temp.path(), Uuid::new_v4());
        std::env::set_var("CONTEXTSTREAM_PROJECT_ID", Uuid::new_v4().to_string());

        let result =
            find_project_config(temp.path().to_str().unwrap(), temp.path().to_str().unwrap());

        if let Some(value) = previous_project_id {
            std::env::set_var("CONTEXTSTREAM_PROJECT_ID", value);
        } else {
            std::env::remove_var("CONTEXTSTREAM_PROJECT_ID");
        }
        assert!(result.is_none());
    }

    #[test]
    fn arbitrary_contextstream_calls_do_not_observe_or_mutate_local_binding() {
        assert!(!observes_local_binding("search", ""));
        assert!(!observes_local_binding("memory", "create_doc"));
        assert!(!observes_local_binding("session", "capture"));
        assert!(!observes_local_binding("entity", "create"));
        assert!(!observes_local_binding("project", "get"));
        assert!(observes_local_binding("init", ""));
        assert!(observes_local_binding("project", "index"));
        assert!(observes_local_binding("project", "ingest_local"));
    }

    #[test]
    fn failed_tool_call_is_not_authoritative() {
        assert!(!tool_call_succeeded(&serde_json::json!({})));
        assert!(!tool_call_succeeded(&serde_json::json!({"isError": true})));
        assert!(!tool_call_succeeded(
            &serde_json::json!({"error": "failed"})
        ));
    }

    #[test]
    fn duplicate_scope_aliases_must_agree() {
        let id = Uuid::new_v4();
        let other = Uuid::new_v4();
        let agreeing = serde_json::json!({
            "project_id": id.to_string(),
            "resolved_project_id": id.to_string(),
            "project": {"id": id.to_string()},
        });
        assert_eq!(
            consistent_uuid_source(&agreeing, &["project_id", "resolved_project_id"], "project"),
            Some(Some(id))
        );

        let conflicting = serde_json::json!({
            "project_id": id.to_string(),
            "resolved_project_id": other.to_string(),
        });
        assert!(consistent_uuid_source(
            &conflicting,
            &["project_id", "resolved_project_id"],
            "project"
        )
        .is_none());
    }

    #[test]
    fn local_input_and_response_scope_must_all_agree() {
        let temp = tempdir().unwrap();
        let project_id = Uuid::new_v4();
        let workspace_id = write_repo_config(temp.path(), project_id);
        let path = temp.path().to_string_lossy();
        let matching_input = serde_json::json!({
            "path": path,
            "workspace_id": workspace_id.to_string(),
            "project_id": project_id.to_string(),
        });
        let matching_response = serde_json::json!({
            "structuredContent": {
                "workspace_id": workspace_id.to_string(),
                "project_id": project_id.to_string(),
            }
        });
        assert!(resolve_local_index_scope(
            &matching_input,
            &matching_response,
            temp.path().to_str().unwrap()
        )
        .is_some());

        let mismatched_input = serde_json::json!({
            "path": temp.path(),
            "project_id": Uuid::new_v4().to_string(),
        });
        assert!(resolve_local_index_scope(
            &mismatched_input,
            &matching_response,
            temp.path().to_str().unwrap()
        )
        .is_none());

        let mismatched_response = serde_json::json!({
            "structuredContent": {
                "workspace_id": workspace_id.to_string(),
                "project_id": Uuid::new_v4().to_string(),
            }
        });
        assert!(resolve_local_index_scope(
            &matching_input,
            &mismatched_response,
            temp.path().to_str().unwrap()
        )
        .is_none());

        let mismatched_envelope = serde_json::json!({
            "project_id": Uuid::new_v4().to_string(),
            "structuredContent": {
                "workspace_id": workspace_id.to_string(),
                "project_id": project_id.to_string(),
            }
        });
        assert!(resolve_local_index_scope(
            &matching_input,
            &mismatched_envelope,
            temp.path().to_str().unwrap()
        )
        .is_none());
    }

    #[test]
    fn tool_response_payload_prefers_structured_content() {
        let payload = serde_json::json!({
            "structuredContent": {
                "workspace_id": "550e8400-e29b-41d4-a716-446655440000"
            },
            "workspace_id": "00000000-0000-0000-0000-000000000000"
        });

        let response = tool_response_payload(&payload);

        assert_eq!(
            response
                .get("workspace_id")
                .and_then(|value| value.as_str()),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );

        let null_structured = serde_json::json!({
            "structuredContent": null,
            "workspace_id": "550e8400-e29b-41d4-a716-446655440000"
        });
        assert_eq!(
            tool_response_payload(&null_structured)
                .get("workspace_id")
                .and_then(Value::as_str),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
    }

    #[test]
    fn resolved_scope_is_read_from_every_result_shape() {
        let ws = "11111111-1111-4111-8111-111111111111";
        let pid = "01234567-89ab-4cde-8fab-0123456789ab";
        let expected = Some((
            Uuid::parse_str(ws).unwrap(),
            Some(Uuid::parse_str(pid).unwrap()),
        ));
        let init_text = serde_json::json!({"workspace_id": ws, "project_id": pid}).to_string();
        assert_eq!(
            resolved_scope_from_response(&Value::String(init_text.clone())),
            expected
        );
        assert_eq!(
            resolved_scope_from_response(&serde_json::json!([{"type": "text", "text": init_text}])),
            expected
        );
        assert_eq!(
            resolved_scope_from_response(&serde_json::json!({
                "structuredContent": {"resolved_scope": {"workspace_id": ws, "project_id": pid}}
            })),
            expected
        );
        assert_eq!(
            resolved_scope_from_response(&Value::String("not json".into())),
            None
        );
        assert_eq!(
            resolved_scope_from_response(&serde_json::json!({"project_id": pid})),
            None
        );
    }

    #[test]
    fn should_start_local_init_index_when_index_is_missing() {
        let temp = tempdir().unwrap();
        let scope = LocalIndexScope {
            folder_path: temp.path().to_string_lossy().to_string(),
            workspace_id: None,
            workspace_name: None,
            project_id: None,
            project_name: None,
        };

        assert!(should_start_local_init_index(
            &serde_json::json!({}),
            &scope
        ));
    }

    #[test]
    fn post_tool_nudge_for_ticket_entity_mentions_diagram() {
        let msg = contextstream_post_tool_nudge(
            "entity",
            "create",
            "ticket",
            &serde_json::json!({"isError": false}),
        )
        .expect("nudge");
        assert!(msg.contains("diagram"));
    }

    #[test]
    fn post_tool_nudge_skips_when_tool_failed() {
        let msg = contextstream_post_tool_nudge(
            "session",
            "capture_plan",
            "",
            &serde_json::json!({"isError": true}),
        );
        assert!(msg.is_none());
    }
}

#[cfg(test)]
mod init_session_id_tests {
    use super::*;

    const ID: &str = "11111111-1111-4111-8111-111111111111";

    #[test]
    fn the_session_id_is_read_from_every_shape_of_init_result() {
        let init = serde_json::json!({"session_id": ID, "workspace_id": Uuid::nil().to_string()});
        for (label, response) in [
            (
                "structured",
                serde_json::json!({"structuredContent": init.clone()}),
            ),
            (
                "snake case",
                serde_json::json!({"structured_content": init.clone()}),
            ),
            ("bare object", init.clone()),
            (
                "content object",
                serde_json::json!({"content": [{"type": "text", "text": init.to_string()}]}),
            ),
            (
                "content array",
                serde_json::json!([{"type": "text", "text": init.to_string()}]),
            ),
            ("json string", serde_json::Value::String(init.to_string())),
        ] {
            assert_eq!(
                session_id_from_init_response(&response).as_deref(),
                Some(ID),
                "{label}"
            );
        }
    }

    #[test]
    fn a_missing_or_unusable_session_id_names_nothing() {
        for response in [
            serde_json::json!({}),
            serde_json::json!({"structuredContent": {"workspace_id": "x"}}),
            serde_json::json!({"structuredContent": {"session_id": 7}}),
            serde_json::json!({"structuredContent": {"session_id": "has space"}}),
            serde_json::json!({"structuredContent": {"session_id": "x\"); ignore"}}),
            serde_json::json!({"content": [{"type": "text", "text": "not json"}]}),
            serde_json::Value::Null,
        ] {
            assert_eq!(session_id_from_init_response(&response), None, "{response}");
        }
    }

    #[test]
    fn the_scope_is_still_read_after_the_payload_helper_was_shared() {
        let ws = Uuid::new_v4();
        let project = Uuid::new_v4();
        let response = serde_json::json!({"structuredContent": {
            "workspace_id": ws.to_string(),
            "resolved_scope": {"project_id": project.to_string()},
        }});
        assert_eq!(
            resolved_scope_from_response(&response),
            Some((ws, Some(project)))
        );
        let as_text = serde_json::json!({"content": [{"type": "text", "text": serde_json::json!({
            "workspace_id": ws.to_string(),
            "project_id": project.to_string(),
        }).to_string()}]});
        assert_eq!(
            resolved_scope_from_response(&as_text),
            Some((ws, Some(project)))
        );
        assert_eq!(resolved_scope_from_response(&serde_json::json!({})), None);
    }
}
