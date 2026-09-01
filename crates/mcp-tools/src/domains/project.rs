//! Project domain tools: list, get, create, update, merge/combine, index, delete, purge, forget_local, remove_paths, overview, statistics, files, index_status, index_history, ingest_local, team_projects.

use super::session::is_not_found_error;
use async_trait::async_trait;
use mcp_client::{get_task_auth_override, ContextStreamClient, IngestLocalParams};
use mcp_session::{auto_init::resolve_workspace, SessionManager};
use mcp_types::{
    api::Project,
    tool::{ToolAnnotations, ToolCategory, ToolMetadata, ToolResult},
    Error, Result,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::process::Stdio;
use std::sync::Arc;
use tokio::process::Command;
use uuid::Uuid;

use crate::registry::ToolHandler;
use crate::schema::SchemaBuilder;

// ============================================================================
// Projects List Tool
// ============================================================================

/// Input for listing projects.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectsListInput {
    pub workspace_id: Option<String>,
    pub page: Option<i64>,
    pub page_size: Option<i64>,
}

/// Projects list tool handler.
pub struct ProjectsListTool {
    client: ContextStreamClient,
}

impl ProjectsListTool {
    pub fn new(client: ContextStreamClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl ToolHandler for ProjectsListTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let input: ProjectsListInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;

        let workspace_id = input
            .workspace_id
            .as_ref()
            .and_then(|s| Uuid::parse_str(s).ok());

        let projects = self
            .client
            .list_projects(workspace_id, input.page, input.page_size)
            .await?;

        let mut text = String::new();

        if projects.is_empty() {
            text.push_str("No projects found. Create one with projects_create(name=\"...\").");
        } else {
            text.push_str(&format!("Found {} projects.\n\n", projects.len()));

            for (i, proj) in projects.iter().enumerate() {
                text.push_str(&format!("{}. **{}** ({})\n", i + 1, proj.name, proj.id));

                if let Some(ref desc) = proj.description {
                    if !desc.is_empty() {
                        text.push_str(&format!("   {}\n", desc));
                    }
                }

                if let Some(count) = proj.file_count {
                    text.push_str(&format!("   Files: {}\n", count));
                }

                if let Some(ref indexed) = proj.indexed_at {
                    text.push_str(&format!("   Indexed: {}\n", indexed));
                }
            }
        }

        Ok(ToolResult::with_structured(
            text,
            serde_json::to_value(&projects).unwrap_or_default(),
        ))
    }

    fn metadata(&self) -> &ToolMetadata {
        static METADATA: std::sync::OnceLock<ToolMetadata> = std::sync::OnceLock::new();
        METADATA.get_or_init(|| ToolMetadata {
            name: "projects_list".to_string(),
            title: "List Projects".to_string(),
            description: "List all projects in a workspace.".to_string(),
            category: ToolCategory::Project,
            annotations: ToolAnnotations::read_only(),
            is_pro: false,
            required_tier: None,
        })
    }

    fn input_schema(&self) -> Value {
        SchemaBuilder::new()
            .description("List projects")
            .uuid(
                "workspace_id",
                "Workspace ID (uses default if omitted)",
                false,
            )
            .integer("page", "Page number", false)
            .integer("page_size", "Results per page", false)
            .build()
    }
}

// ============================================================================
// Projects Create Tool
// ============================================================================

/// Input for creating a project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectsCreateInput {
    pub name: String,
    pub description: Option<String>,
    pub workspace_id: Option<String>,
}

/// Projects create tool handler.
pub struct ProjectsCreateTool {
    client: ContextStreamClient,
}

impl ProjectsCreateTool {
    pub fn new(client: ContextStreamClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl ToolHandler for ProjectsCreateTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let input: ProjectsCreateInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;

        if input.name.trim().is_empty() {
            return Err(Error::Validation("name is required".to_string()));
        }

        let workspace_id = input
            .workspace_id
            .as_ref()
            .and_then(|s| Uuid::parse_str(s).ok());

        let project = self
            .client
            .create_project(&input.name, input.description.as_deref(), workspace_id)
            .await?;

        let ws_display = project
            .workspace_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "N/A".to_string());
        let text = format!(
            "Project created: {} ({})\nWorkspace: {}",
            project.name, project.id, ws_display
        );

        Ok(ToolResult::with_structured(
            text,
            serde_json::to_value(&project).unwrap_or_default(),
        ))
    }

    fn metadata(&self) -> &ToolMetadata {
        static METADATA: std::sync::OnceLock<ToolMetadata> = std::sync::OnceLock::new();
        METADATA.get_or_init(|| ToolMetadata {
            name: "projects_create".to_string(),
            title: "Create Project".to_string(),
            description: "Create a new project in a workspace.".to_string(),
            category: ToolCategory::Project,
            annotations: ToolAnnotations::write(),
            is_pro: false,
            required_tier: None,
        })
    }

    fn input_schema(&self) -> Value {
        SchemaBuilder::new()
            .description("Create a project")
            .string("name", "Project name", true)
            .string("description", "Project description", false)
            .uuid(
                "workspace_id",
                "Workspace ID (uses default if omitted)",
                false,
            )
            .build()
    }
}

// ============================================================================
// Projects Index Tool
// ============================================================================

/// Input for indexing a project.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectsIndexInput {
    pub project_id: String,
}

/// Projects index tool handler.
pub struct ProjectsIndexTool {
    client: ContextStreamClient,
}

impl ProjectsIndexTool {
    pub fn new(client: ContextStreamClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl ToolHandler for ProjectsIndexTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let input: ProjectsIndexInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;

        let project_id = Uuid::parse_str(&input.project_id).map_err(|_| {
            Error::Validation("Invalid project_id format. Provide a valid UUID.".to_string())
        })?;

        let result = self.client.index_project(project_id).await?;

        let status = result
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("started");

        let text = format!("Index job {} for project {}.", status, input.project_id);

        Ok(ToolResult::with_structured(text, result))
    }

    fn metadata(&self) -> &ToolMetadata {
        static METADATA: std::sync::OnceLock<ToolMetadata> = std::sync::OnceLock::new();
        METADATA.get_or_init(|| ToolMetadata {
            name: "projects_index".to_string(),
            title: "Index Project".to_string(),
            description: "Start indexing a project for search.".to_string(),
            category: ToolCategory::Project,
            annotations: ToolAnnotations::write(),
            is_pro: false,
            required_tier: None,
        })
    }

    fn input_schema(&self) -> Value {
        SchemaBuilder::new()
            .description("Index a project")
            .uuid("project_id", "Project ID to index", true)
            .build()
    }
}

// ============================================================================
// Unified Project Tool
// ============================================================================

/// Valid sort fields for files.
const VALID_SORT_BY: &[&str] = &["path", "indexed", "size"];

/// Valid sort orders.
const VALID_SORT_ORDER: &[&str] = &["asc", "desc"];
const INDEX_FRESH_HOURS: i64 = 1;
const INDEX_RECENT_HOURS: i64 = 24;
const INDEX_STALE_HOURS: i64 = 48;
const LOCAL_INDEXING_STARTED_VISIBLE_HOURS: i64 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ResolvedIngestProject {
    project_id: Uuid,
    source: &'static str,
}

struct ResolvedIngestContext {
    path: String,
    workspace_id: Option<Uuid>,
    project: ResolvedIngestProject,
    original_project_id: Option<Uuid>,
    auto_created: bool,
    workspace_display_name: Option<String>,
    resolved_project_name: Option<String>,
}

struct ResolvedReadProject {
    project_id: Uuid,
    project: Project,
}

/// Last path segment for both `/` and `\\` (handles Windows paths on non-Windows MCP hosts).
fn folder_name_from_path(path: &str) -> String {
    const SEPS: [char; 2] = ['/', '\\'];
    let trimmed = path.trim_end_matches(SEPS);
    let last = trimmed
        .rsplit_once(SEPS)
        .map(|(_, name)| name)
        .unwrap_or(trimmed)
        .trim();
    if last.is_empty() {
        return String::new();
    }
    // Drive-only roots: `C:` / `D:` from `C:\` style paths
    let bytes = last.as_bytes();
    if bytes.len() == 2 && bytes[1] == b':' {
        return String::new();
    }
    last.to_string()
}

fn project_repository_identity(project: &Project) -> Option<mcp_session::RepositoryRemoteIdentity> {
    let value = project.repository_url.as_deref()?.trim();
    if value.starts_with("git-remote-v1:") {
        mcp_session::RepositoryRemoteIdentity::parse(value).ok()
    } else {
        mcp_session::RepositoryRemoteIdentity::from_remote_url(value).ok()
    }
}

async fn resolve_repository_projects(
    client: &ContextStreamClient,
    workspace_id: Uuid,
    identity: &mcp_session::RepositoryRemoteIdentity,
    folder_name: &str,
) -> Result<(Vec<Project>, Vec<Project>)> {
    let mut exact_matches = Vec::new();
    let mut legacy_name_matches = Vec::new();
    let projects = client
        .list_all_projects(Some(workspace_id), 200, 100_000)
        .await?;
    for project in projects {
        match project_repository_identity(&project) {
            Some(project_identity) if &project_identity == identity => exact_matches.push(project),
            None if project.name.eq_ignore_ascii_case(folder_name) => {
                legacy_name_matches.push(project)
            }
            _ => {}
        }
    }
    Ok((exact_matches, legacy_name_matches))
}

fn requires_ingest_endpoint(error: &Error) -> bool {
    match error {
        Error::Http {
            status, message, ..
        } if *status == 400 => message
            .to_ascii_lowercase()
            .contains("requires using the ingest endpoint"),
        _ => false,
    }
}

fn validate_ingest_directory(path: &str, is_http_transport: bool) -> Result<()> {
    // P0 ingestion-containment: reject over-broad / sensitive ingest roots
    // ($HOME, home ancestors, `/`, `.ssh`/`.aws`/...). Applies in both
    // transports because the local filesystem is what gets walked regardless.
    // The explicit env opt-in (CONTEXTSTREAM_ALLOW_BROAD_INGEST=1) bypasses it.
    match mcp_client::validate_ingest_root(
        std::path::Path::new(path),
        &mcp_client::IngestRootOptions::from_env(),
    ) {
        Ok(assessment) => {
            for warning in assessment.warnings {
                tracing::warn!("ingest root warning: {}", warning);
            }
        }
        Err(rejection) => return Err(Error::Validation(rejection.message())),
    }

    if is_http_transport {
        let path_ref = std::path::Path::new(path);
        if !path_ref.exists() {
            return Ok(());
        }
        if !path_ref.is_dir() {
            return Err(Error::Validation(format!(
                "Path must be a directory for ingest_local: {}. Use the parent folder path.",
                path
            )));
        }
        return Ok(());
    }

    let path_ref = std::path::Path::new(path);
    if !path_ref.exists() {
        return Err(Error::Validation(format!(
            "Path not found: {}. Provide an existing directory path and try again.",
            path
        )));
    }
    if !path_ref.is_dir() {
        return Err(Error::Validation(format!(
            "Path must be a directory for ingest_local: {}. Use the parent folder path.",
            path
        )));
    }
    Ok(())
}

fn missing_project_scope_error(
    action: &str,
    workspace_id: Option<Uuid>,
    folder_path: Option<&str>,
) -> Error {
    let next_step = match (workspace_id, folder_path) {
        (Some(ws), Some(path)) => {
            let path = tool_string_literal(path);
            format!(
                "init(folder_path={path}, workspace_id=\"{ws}\"), then project(action=\"index\", workspace_id=\"{ws}\")"
            )
        }
        (Some(ws), None) => format!(
            "init(folder_path=\"<project_path>\", workspace_id=\"{}\") or pass project_id explicitly",
            ws
        ),
        (None, Some(path)) => format!("init(folder_path={})", tool_string_literal(path)),
        (None, None) => "init(folder_path=\"<your_project_path>\")".to_string(),
    };

    let requirement = if workspace_id.is_some() {
        "project_id is required"
    } else {
        "workspace_id is required before project_id is required"
    };
    let scope = if workspace_id.is_some() {
        "No active project scope — workspace-only fallback may be active."
    } else {
        "No active workspace scope is available, so a project cannot be created or resolved safely."
    };

    Error::Validation(format!(
        "{} for {}. {} Next step: {}",
        requirement, action, scope, next_step
    ))
}

fn tool_string_literal(value: &str) -> String {
    serde_json::to_string(value).expect("serializing a string cannot fail")
}

fn reconcile_ingest_project_id(
    action: &str,
    folder_path: &str,
    explicit_project_id: Option<Uuid>,
    session_project_id: Option<Uuid>,
    resolved_folder_project_id: Option<Uuid>,
    local_index_project_id: Option<Uuid>,
) -> Result<Option<ResolvedIngestProject>> {
    let candidates = [
        (explicit_project_id, "explicit_project_id"),
        (session_project_id, "session_project_id"),
        (resolved_folder_project_id, "folder_mapping"),
        (local_index_project_id, "local_index_metadata"),
    ];
    let Some((agreed_project_id, agreed_source)) = candidates
        .iter()
        .find_map(|(project_id, source)| project_id.map(|id| (id, *source)))
    else {
        return Ok(None);
    };

    if candidates
        .iter()
        .any(|(project_id, _)| project_id.is_some_and(|id| id != agreed_project_id))
    {
        let details = candidates
            .iter()
            .filter_map(|(project_id, source)| project_id.map(|id| format!("{source}={id}")))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(Error::Validation(format!(
            "Refusing to {action} local folder '{folder_path}': project scope sources disagree ({details}). No local ingest was started and no folder mapping or index metadata was changed. Re-run init(folder_path=\"{folder_path}\") and correct the stale folder mapping or local index metadata so every non-empty project_id agrees before retrying."
        )));
    }

    Ok(Some(ResolvedIngestProject {
        project_id: agreed_project_id,
        source: agreed_source,
    }))
}

fn push_project_candidate(
    candidates: &mut Vec<(Uuid, &'static str)>,
    project_id: Option<Uuid>,
    source: &'static str,
) {
    let Some(project_id) = project_id else {
        return;
    };
    if !candidates.iter().any(|(id, _)| *id == project_id) {
        candidates.push((project_id, source));
    }
}

async fn resolve_read_project(
    client: &ContextStreamClient,
    session: &Arc<SessionManager>,
    action: &str,
    workspace_id: Option<Uuid>,
    explicit_project_id: Option<Uuid>,
    session_project_id: Option<Uuid>,
    folder_path: Option<&str>,
) -> Result<ResolvedReadProject> {
    let mut resolved_workspace_id = workspace_id;
    let mut resolved_folder_project_id = None;

    if let Some(path) = folder_path {
        if let Some(mapping) = resolve_workspace(path).await {
            if resolved_workspace_id.is_none() {
                resolved_workspace_id = Some(mapping.workspace_id);
            }
            resolved_folder_project_id = mapping.project_id;
        }
    }

    let local_index_project_id =
        folder_path.and_then(ContextStreamClient::tracked_project_id_for_folder);
    let mut candidates = Vec::new();

    if explicit_project_id.is_some() {
        push_project_candidate(&mut candidates, explicit_project_id, "explicit_project_id");
        push_project_candidate(
            &mut candidates,
            resolved_folder_project_id,
            "folder_mapping",
        );
        push_project_candidate(
            &mut candidates,
            local_index_project_id,
            "local_index_metadata",
        );
    } else {
        push_project_candidate(
            &mut candidates,
            resolved_folder_project_id,
            "folder_mapping",
        );
        push_project_candidate(
            &mut candidates,
            local_index_project_id,
            "local_index_metadata",
        );
        push_project_candidate(&mut candidates, session_project_id, "session_project_id");
    }

    if candidates.is_empty() {
        return Err(missing_project_scope_error(
            action,
            resolved_workspace_id,
            folder_path,
        ));
    }

    for (candidate_id, source) in candidates {
        match client.get_project(candidate_id).await {
            Ok(project) => {
                if let (Some(expected), Some(actual)) =
                    (resolved_workspace_id, project.workspace_id)
                {
                    if expected != actual {
                        continue;
                    }
                }

                if resolved_workspace_id.is_none() {
                    resolved_workspace_id = project.workspace_id;
                }

                if session_project_id != Some(candidate_id) || workspace_id != resolved_workspace_id
                {
                    session
                        .update_scope(
                            resolved_workspace_id,
                            Some(candidate_id),
                            folder_path.map(str::to_string),
                        )
                        .await;
                }

                tracing::debug!(
                    project_id = %candidate_id,
                    source,
                    action,
                    "Resolved project scope for read-only project action"
                );

                return Ok(ResolvedReadProject {
                    project_id: candidate_id,
                    project,
                });
            }
            Err(err) if is_not_found_error(&err) => continue,
            Err(err) => return Err(err),
        }
    }

    Err(Error::Validation(
        "Project not found for current context. Call init(...) in this folder or pass a valid project_id explicitly.".to_string(),
    ))
}

fn parse_timestamp_field(result: &Value, key: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    result.get(key).and_then(|v| v.as_str()).and_then(|raw| {
        chrono::DateTime::parse_from_rfc3339(raw)
            .ok()
            .map(|parsed| parsed.with_timezone(&chrono::Utc))
    })
}

fn extract_index_timestamp(result: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    for key in [
        "ingested_at_max",
        "indexed_at",
        "last_indexed",
        "index_timestamp",
    ] {
        if let Some(parsed) = parse_timestamp_field(result, key) {
            return Some(parsed);
        }
    }

    // `last_updated` can mean "the index job/status row changed", not "a
    // searchable generation committed". While indexing is in progress, search
    // still uses the latest committed generation, so do not report a fresh
    // index from status-row churn.
    if api_result_is_indexing(result) {
        return None;
    }

    for key in ["last_updated"] {
        if let Some(raw) = result.get(key).and_then(|v| v.as_str()) {
            if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(raw) {
                return Some(parsed.with_timezone(&chrono::Utc));
            }
        }
    }
    None
}

fn api_result_reports_indexed(result: &Value) -> bool {
    if let Some(indexed) = result.get("indexed").and_then(|v| v.as_bool()) {
        return indexed;
    }

    let indexed_files = result
        .get("indexed_files")
        .and_then(|v| v.as_i64())
        .or_else(|| result.get("indexed_file_count").and_then(|v| v.as_i64()))
        .unwrap_or(0);
    if indexed_files > 0 {
        return true;
    }

    let total_files = result
        .get("total_files")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    if total_files > 0 {
        let status = result
            .get("status")
            .and_then(|v| v.as_str())
            .map(|s| s.to_ascii_lowercase());
        if matches!(status.as_deref(), Some("completed" | "ready")) {
            return true;
        }
    }

    false
}

fn api_result_reports_canonical_indexed(result: &Value, checkout_scope_unconfirmed: bool) -> bool {
    if api_result_reports_indexed(result) {
        return true;
    }
    if !checkout_scope_unconfirmed {
        return false;
    }

    ContextStreamClient::project_index_status_reports_canonical_ready(result)
}

fn api_result_is_indexing(result: &Value) -> bool {
    let project_index_state = result
        .get("project_index_state")
        .and_then(|v| v.as_str())
        .map(|s| s.to_ascii_lowercase());
    if matches!(
        project_index_state.as_deref(),
        Some("indexing" | "committing")
    ) {
        return true;
    }

    let status = result
        .get("status")
        .and_then(|v| v.as_str())
        .map(|s| s.to_ascii_lowercase());
    if matches!(status.as_deref(), Some("indexing" | "processing")) {
        return true;
    }

    result
        .get("pending_files")
        .and_then(|v| v.as_i64())
        .map(|pending| pending > 0)
        .unwrap_or(false)
}

fn index_history_entry_count(result: &Value) -> usize {
    if let Some(entries) = result.get("entries").and_then(|v| v.as_array()) {
        return entries.len();
    }
    if let Some(history) = result.get("history").and_then(|v| v.as_array()) {
        return history.len();
    }
    result.as_array().map(|a| a.len()).unwrap_or(0)
}

fn project_files_page_count(result: &Value) -> usize {
    result
        .get("files")
        .and_then(|f| f.as_array())
        .map(|a| a.len())
        .or_else(|| {
            result
                .get("paths")
                .and_then(|p| p.as_array())
                .map(|a| a.len())
        })
        .unwrap_or(0)
}

fn pending_files_from_value(value: Option<&Value>) -> Option<i64> {
    match value {
        Some(Value::Number(num)) => num.as_i64(),
        Some(Value::String(raw)) => raw.parse::<i64>().ok(),
        _ => None,
    }
}

fn extract_pending_files(result: &Value) -> Option<i64> {
    pending_files_from_value(result.get("pending_files")).or_else(|| {
        result
            .get("data")
            .and_then(|data| data.get("pending_files"))
            .and_then(|value| pending_files_from_value(Some(value)))
    })
}

fn pending_paths_from_object(obj: &serde_json::Map<String, Value>) -> Option<Vec<String>> {
    for key in ["pending_file_paths", "pending_paths", "pending_files_list"] {
        if let Some(paths) = obj.get(key).and_then(|value| value.as_array()) {
            let normalized: Vec<String> = paths
                .iter()
                .filter_map(|value| value.as_str().map(str::to_string))
                .collect();
            return Some(normalized);
        }
    }
    None
}

fn extract_pending_file_paths(result: &Value) -> Vec<String> {
    if let Some(obj) = result.as_object() {
        if let Some(paths) = pending_paths_from_object(obj) {
            return paths;
        }
        if let Some(data_obj) = obj.get("data").and_then(|value| value.as_object()) {
            if let Some(paths) = pending_paths_from_object(data_obj) {
                return paths;
            }
        }
    }
    vec![]
}

fn format_project_files_text(result: &Value) -> String {
    let page_count = project_files_page_count(result);
    let total_count = result.get("total").and_then(|v| v.as_i64());
    let mut text = if let Some(total) = total_count {
        if total as usize == page_count {
            format!("Found {} indexed files.", page_count)
        } else {
            format!(
                "Found {} indexed files on this page ({} total indexed).",
                page_count, total
            )
        }
    } else {
        format!("Found {} indexed files.", page_count)
    };
    if ContextStreamClient::project_index_status_is_checkout_scoped(result)
        && !ContextStreamClient::project_index_status_matches_checkout(result)
    {
        text.push_str(
            " The hosted service did not confirm this exact checkout; this may be the canonical project file list rather than the active worktree overlay.",
        );
    }
    text
}

fn classify_index_freshness(indexed: bool, age_hours: Option<i64>) -> &'static str {
    if !indexed {
        return "missing";
    }
    match age_hours {
        None => "unknown",
        Some(hours) if hours <= INDEX_FRESH_HOURS => "fresh",
        Some(hours) if hours <= INDEX_RECENT_HOURS => "recent",
        Some(hours) if hours <= INDEX_STALE_HOURS => "aging",
        Some(_) => "stale",
    }
}

fn classify_index_confidence(
    indexed: bool,
    api_indexed: bool,
    locally_indexed: bool,
    freshness: &str,
) -> (&'static str, &'static str) {
    if !indexed {
        return (
            "low",
            "Neither API status nor local index metadata currently indicates a usable index.",
        );
    }

    if api_indexed && locally_indexed {
        let reason = if freshness == "stale" {
            "API and local metadata agree, but index age indicates stale coverage."
        } else {
            "API and local metadata agree for this project scope."
        };
        return ("high", reason);
    }

    if api_indexed {
        let reason = if freshness == "stale" {
            "API reports index readiness, but index age indicates stale coverage."
        } else {
            "API reports index readiness; local metadata is only an optional cache."
        };
        return ("high", reason);
    }

    if locally_indexed {
        return (
            "medium",
            "Local metadata reports index readiness, but API status does not currently confirm it.",
        );
    }

    (
        "low",
        "Index state is inferred but lacks corroborating API/local metadata.",
    )
}

fn format_index_freshness_text(freshness: &str, age_hours: Option<i64>, indexed: bool) -> String {
    let age_display = age_hours
        .map(|h| format!("{}h", h))
        .unwrap_or_else(|| "unknown".to_string());

    if !indexed {
        return format!(" Freshness: {} ({}).", freshness, age_display);
    }

    match freshness {
        "aging" | "stale" => format!(
            " Search is ready from the existing index; last confirmed ingest was {} ago. Background refresh can update coverage.",
            age_display
        ),
        "unknown" => " Index is ready; freshness timestamp is unavailable.".to_string(),
        _ => format!(" Freshness: {} ({}).", freshness, age_display),
    }
}

fn folder_scope_mismatches_explicit_project(
    explicit_project_id: Option<Uuid>,
    resolved_folder_project_id: Option<Uuid>,
    local_index_project_id: Option<Uuid>,
) -> bool {
    let Some(explicit_id) = explicit_project_id else {
        return false;
    };

    let indicators = [resolved_folder_project_id, local_index_project_id];
    let has_folder_indicator = indicators.iter().any(Option::is_some);
    let has_matching_indicator = indicators.contains(&Some(explicit_id));

    has_folder_indicator && !has_matching_indicator
}

fn resolve_project_tool_folder_path(
    explicit_folder_path: Option<String>,
    explicit_path: Option<String>,
    session_folder_path: Option<String>,
    _has_explicit_project_id: bool,
) -> Option<String> {
    explicit_folder_path
        .or(explicit_path)
        .or(session_folder_path)
}

/// Whether two folder paths refer to the same directory. Exact-string match
/// first (cheap, and the only thing that works for paths that don't exist on
/// disk), then canonicalized equality so a trailing slash, symlink, or
/// relative-vs-absolute form still matches. Non-existent, non-equal paths do
/// NOT match (canonicalize fails on both → no false positive).
fn folder_paths_equivalent(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

/// Input for the unified project tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectInput {
    pub action: String,
    // Common fields
    pub name: Option<String>,
    pub description: Option<String>,
    pub workspace_id: Option<String>,
    pub project_id: Option<String>,
    /// Source project to merge into project_id when action is merge/combine.
    pub source_project_id: Option<String>,
    pub page: Option<i64>,
    pub page_size: Option<i64>,
    // Files fields
    pub path_pattern: Option<String>,
    /// Exact indexed file paths (relative to the project root) to de-index, for
    /// action="remove_paths".
    pub paths: Option<Vec<String>>,
    pub sort_by: Option<String>,
    pub sort_order: Option<String>,
    // Index history fields
    pub since: Option<String>,
    pub until: Option<String>,
    pub machine_id: Option<String>,
    pub branch: Option<String>,
    // Recent changes fields
    pub limit: Option<i64>,
    // Ingest local fields
    pub path: Option<String>,
    pub folder_path: Option<String>,
    pub force: Option<bool>,
    pub generate_editor_rules: Option<bool>,
    /// When true, `ingest_local` / folder `index` do not auto-create a project if scope is missing.
    pub skip_project_creation: Option<bool>,
}

/// Unified project tool handler.
pub struct ProjectTool {
    client: ContextStreamClient,
    session: Arc<SessionManager>,
}

/// How an index/ingest request whose folder is NOT readable from this process
/// should be answered, based on the server-side index_status payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteIndexDisposition {
    /// The server-side index has committed files — nothing to ingest from here.
    Ready,
    /// A server-side index run is currently in flight.
    Indexing,
    /// No server-side content exists yet — the installed sync bridge, an editor
    /// hook, or Desktop must upload it.
    RequiresSyncBridge,
}

fn classify_remote_index_disposition(status: &Value) -> RemoteIndexDisposition {
    if api_result_reports_indexed(status) {
        RemoteIndexDisposition::Ready
    } else if api_result_is_indexing(status) {
        RemoteIndexDisposition::Indexing
    } else {
        RemoteIndexDisposition::RequiresSyncBridge
    }
}

/// The API acks `POST /projects/{id}/index` for folder-synced ("local")
/// projects with `status: "skipped"` instead of running a server-side job.
/// Relaying that ack ("use the CLI or ingest API") sends remote agents in
/// circles — those requests are answered with real index state instead.
fn api_index_result_is_skipped(result: &Value) -> bool {
    result
        .get("status")
        .and_then(|v| v.as_str())
        .is_some_and(|status| status.eq_ignore_ascii_case("skipped"))
}

fn remote_index_file_count(status: &Value) -> Option<i64> {
    status
        .get("indexed_files")
        .and_then(|v| v.as_i64())
        .or_else(|| status.get("indexed_file_count").and_then(|v| v.as_i64()))
        .or_else(|| status.get("total_files").and_then(|v| v.as_i64()))
        .filter(|count| *count > 0)
}

fn remote_path_note(path: Option<&str>) -> String {
    match path {
        Some(p) => format!(
            "Hosted MCP intentionally delegates workstation access for '{p}' to the exact-checkout ContextStream sync bridge, editor hooks, or ContextStream Desktop; this server request used managed index state rather than walking the local filesystem."
        ),
        None => "Hosted MCP intentionally delegates workstation access to the exact-checkout ContextStream sync bridge, editor hooks, or ContextStream Desktop; this server request used managed index state rather than walking the local filesystem.".to_string(),
    }
}

fn server_index_ready_message(project_id: Uuid, files: Option<i64>, path: Option<&str>) -> String {
    let files_note = files
        .map(|count| format!(" ({count} files)"))
        .unwrap_or_default();
    format!(
        "Server-side index is ready for project {project_id}{files_note}. {} Local changes are kept current by the installed ContextStream sync bridge, editor hooks, or ContextStream Desktop; verify freshness with project(action=\"index_status\").",
        remote_path_note(path)
    )
}

fn server_indexing_in_progress_message(project_id: Uuid, path: Option<&str>) -> String {
    format!(
        "A server-side index run is already in progress for project {project_id}. {} Monitor with project(action=\"index_status\").",
        remote_path_note(path)
    )
}

fn requires_sync_bridge_message(path: Option<&str>) -> String {
    let target = path
        .map(|p| format!("'{p}'"))
        .unwrap_or_else(|| "the project folder".to_string());
    format!(
        "No exact-checkout content upload has completed yet. Hosted MCP intentionally receives {target} through the installed ContextStream sync bridge, editor hooks, or ContextStream Desktop instead of reading workstation paths in the remote process. Keep hosted MCP configured. Run `contextstream-mcp doctor --repair --scope global --only-configured` to repair the bridge, or open ContextStream Desktop and choose 'Add local repository'. Then run project(action=\"index\") and verify with project(action=\"index_status\")."
    )
}

fn hosted_index_refresh_instruction(path: &str) -> String {
    let path = tool_string_literal(path);
    format!(
        "Re-establish the exact checkout with init(folder_path={path}), then run project(action=\"index\"). If the response says requires_sync_bridge, repair the bridge while keeping hosted MCP configured."
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoteRefreshDisposition {
    Requested,
    Pending,
    Completed,
    BridgeOffline,
    NoCheckout,
    Unknown,
}

fn classify_remote_refresh_disposition(receipt: &Value) -> RemoteRefreshDisposition {
    match receipt
        .get("status")
        .or_else(|| receipt.get("state"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "requested" | "accepted" | "queued" => RemoteRefreshDisposition::Requested,
        "pending" | "claimed" | "running" | "in_progress" => RemoteRefreshDisposition::Pending,
        "completed" | "committed" | "ready" => RemoteRefreshDisposition::Completed,
        "bridge_offline" | "offline" => RemoteRefreshDisposition::BridgeOffline,
        "no_checkout" | "checkout_not_found" | "unregistered" => {
            RemoteRefreshDisposition::NoCheckout
        }
        _ => RemoteRefreshDisposition::Unknown,
    }
}

fn refresh_endpoint_is_unsupported(error: &Error) -> bool {
    matches!(
        error,
        Error::Http {
            status: 404 | 405 | 501,
            ..
        }
    )
}

impl ProjectTool {
    pub fn new(client: ContextStreamClient, session: Arc<SessionManager>) -> Self {
        Self { client, session }
    }

    async fn resolve_ingest_context(
        &self,
        action: &str,
        folder_path: Option<String>,
        workspace_id: Option<Uuid>,
        explicit_project_id: Option<Uuid>,
        session_project_id: Option<Uuid>,
        skip_project_creation: bool,
    ) -> Result<ResolvedIngestContext> {
        let path = folder_path.ok_or_else(|| {
            Error::Validation(format!(
                "path is required for {}. Call init(...) first or pass path explicitly.",
                action
            ))
        })?;
        validate_ingest_directory(&path, self.client.config().await.is_http_transport)?;

        let mut resolved_workspace_id = workspace_id;
        let mut mapping_workspace_name: Option<String> = None;
        let mut resolved_folder_project_id = None;
        if let Some(mapping) = resolve_workspace(&path).await {
            if let Some(expected) = resolved_workspace_id {
                if expected != mapping.workspace_id {
                    return Err(Error::Validation(format!(
                        "Refusing to {action} local folder '{path}': active workspace {expected} conflicts with the checkout binding workspace {}. No local ingest was started.",
                        mapping.workspace_id
                    )));
                }
            } else {
                resolved_workspace_id = Some(mapping.workspace_id);
            }
            mapping_workspace_name = Some(mapping.workspace_name);
            resolved_folder_project_id = mapping.project_id;
        }
        let local_index_project_id = ContextStreamClient::tracked_project_id_for_folder(&path);
        let mut resolved_project = reconcile_ingest_project_id(
            action,
            &path,
            explicit_project_id,
            session_project_id,
            resolved_folder_project_id,
            local_index_project_id,
        )?;
        let mut auto_created = false;
        let mut workspace_display_name: Option<String> = None;
        let mut resolved_project_name: Option<String> = None;
        let repository_identity = if std::path::Path::new(&path).is_dir() {
            match mcp_session::current_repository_remote_identity(std::path::Path::new(&path)) {
                Ok(identity) => identity,
                Err(mcp_session::CheckoutIdentityError::NotGitCheckout(_)) => None,
                Err(error) => {
                    return Err(Error::Validation(format!(
                        "Refusing to {action} local folder '{path}': repository identity is ambiguous or unreadable: {error}. No local ingest was started."
                    )))
                }
            }
        } else {
            None
        };

        if resolved_project.is_none() {
            if let (Some(workspace_id), Some(identity)) =
                (resolved_workspace_id, repository_identity.as_ref())
            {
                let folder_name = folder_name_from_path(&path);
                let (matches, legacy_name_matches) =
                    resolve_repository_projects(&self.client, workspace_id, identity, &folder_name)
                        .await?;
                match matches.as_slice() {
                    [project] => {
                        resolved_project = Some(ResolvedIngestProject {
                            project_id: project.id,
                            source: "repository_identity",
                        });
                        resolved_project_name = Some(project.name.clone());
                    }
                    [] => {}
                    duplicates => {
                        let ids = duplicates
                            .iter()
                            .map(|project| project.id.to_string())
                            .collect::<Vec<_>>()
                            .join(", ");
                        return Err(Error::Validation(format!(
                            "Refusing to {action} local folder '{path}': multiple projects in workspace {workspace_id} claim repository {identity} ({ids}). Select or merge the canonical project explicitly; no duplicate was created and no local ingest was started."
                        )));
                    }
                }
                if resolved_project.is_none() && !legacy_name_matches.is_empty() {
                    let ids = legacy_name_matches
                        .iter()
                        .map(|project| project.id.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Err(Error::Validation(format!(
                        "Refusing to {action} local folder '{path}': same-name legacy project(s) {ids} have no trustworthy repository identity. Select the canonical project explicitly once so it can be bound and backfilled; no duplicate was created and no local ingest was started."
                    )));
                }
            }
        }

        if resolved_project.is_none() {
            // Never auto-create a project for a folder this process cannot
            // read (hosted/remote gateway): the ingest that would justify the
            // new project cannot run from here, and an empty orphan project
            // would be left behind.
            let path_is_readable_dir = std::path::Path::new(&path).is_dir();
            if skip_project_creation || !path_is_readable_dir {
                return Err(missing_project_scope_error(
                    action,
                    resolved_workspace_id,
                    Some(&path),
                ));
            }
            let ws_id = resolved_workspace_id;
            let mut ws_name = mapping_workspace_name.clone();

            if ws_name.is_none() {
                if let Some(id) = ws_id {
                    ws_name = self.client.get_workspace(id).await.ok().map(|w| w.name);
                }
            }

            let Some(workspace_id_final) = ws_id else {
                return Err(missing_project_scope_error(
                    action,
                    resolved_workspace_id,
                    Some(&path),
                ));
            };

            let workspace_name_str = ws_name.unwrap_or_else(|| "Unknown".to_string());
            workspace_display_name = Some(workspace_name_str.clone());
            resolved_workspace_id = Some(workspace_id_final);

            // A folder/project name is not repository identity. A unique
            // credential-free repository match was already reused above; if
            // none exists, create a new canonical project and persist that
            // repository URL for another machine/worktree to resolve later.
            let folder_name = folder_name_from_path(&path);
            let create_name = if folder_name.trim().is_empty() {
                "default-project"
            } else {
                folder_name.as_str()
            };
            let repository_url = repository_identity
                .as_ref()
                .map(mcp_session::RepositoryRemoteIdentity::canonical_https_url);
            let project = self
                .client
                .create_project_with_repository(
                    create_name,
                    None,
                    Some(workspace_id_final),
                    repository_url.as_deref(),
                )
                .await?;
            resolved_project = Some(ResolvedIngestProject {
                project_id: project.id,
                source: "auto_created",
            });
            resolved_project_name = Some(project.name.clone());
            auto_created = true;
        }

        let resolved_project = resolved_project.ok_or_else(|| {
            Error::Validation(
                "Internal error: ingest project resolution returned no project.".to_string(),
            )
        })?;

        // A local ingest can apply checkout-scoped deletions, so
        // never trust an ID solely because local/session metadata agreed. The
        // API is the final authority for both project existence and workspace
        // ownership. Network and server errors intentionally propagate: an
        // unavailable authority is not permission to mutate an unverified
        // project.
        let validated_project = match self
            .client
            .get_project_fresh(resolved_project.project_id)
            .await
        {
            Ok(project) => project,
            Err(err) if is_not_found_error(&err) => {
                return Err(Error::Validation(format!(
                    "Refusing to {action} local folder '{path}': resolved project {} ({}) no longer exists. No local ingest was started. Re-run init(folder_path=\"{path}\") to repair the project mapping before retrying.",
                    resolved_project.project_id, resolved_project.source
                )));
            }
            Err(err) => return Err(err),
        };
        match (resolved_workspace_id, validated_project.workspace_id) {
            (Some(expected), Some(actual)) if expected != actual => {
                return Err(Error::Validation(format!(
                    "Refusing to {action} local folder '{path}': resolved project {} belongs to workspace {actual}, but the active/folder scope requires workspace {expected}. No local ingest was started and no folder mapping or index metadata was changed. Re-run init(folder_path=\"{path}\", workspace_id=\"{actual}\") or select the project that belongs to workspace {expected}.",
                    resolved_project.project_id
                )));
            }
            (Some(expected), None) => {
                return Err(Error::Validation(format!(
                    "Refusing to {action} local folder '{path}': resolved project {} did not report a workspace_id, so ownership by the required workspace {expected} could not be verified. No local ingest was started; retry after project metadata is available.",
                    resolved_project.project_id
                )));
            }
            (None, Some(actual)) => resolved_workspace_id = Some(actual),
            _ => {}
        }

        // An explicit local ingest is the recovery path for legacy or replaced
        // folder bindings. Establish identity only after the uncached API
        // ownership proof above. Non-Git folders receive a durable local
        // marker so later managed-bridge refreshes remain exact and safe.
        if std::path::Path::new(&path).is_dir() {
            if let Some(ws) = resolved_workspace_id {
                let ws_name = workspace_display_name
                    .as_deref()
                    .or(mapping_workspace_name.as_deref())
                    .unwrap_or("Workspace");
                mcp_session::auto_init::establish_folder_binding(
                    &path,
                    ws,
                    ws_name,
                    Some(resolved_project.project_id),
                    resolved_project_name.as_deref(),
                )
                .await
                .map_err(|error| {
                    Error::Validation(format!(
                        "Refusing to {action} local folder '{path}': trusted folder identity could not be established after ownership validation: {error}. No local ingest was started."
                    ))
                })?;
            }
        }

        if session_project_id != Some(resolved_project.project_id) {
            self.session
                .update_scope(
                    resolved_workspace_id,
                    Some(resolved_project.project_id),
                    Some(path.clone()),
                )
                .await;
        }

        Ok(ResolvedIngestContext {
            path,
            workspace_id: resolved_workspace_id,
            project: resolved_project,
            original_project_id: explicit_project_id.or(session_project_id),
            auto_created,
            workspace_display_name,
            resolved_project_name,
        })
    }

    async fn start_background_ingest(
        &self,
        action: &str,
        context: ResolvedIngestContext,
        force: Option<bool>,
        generate_editor_rules: Option<bool>,
    ) -> Result<ToolResult> {
        let path = context.path.clone();
        let project_id = context.project.project_id;
        let ingest_params = IngestLocalParams {
            path: path.clone(),
            workspace_id: context.workspace_id,
            project_id: Some(project_id),
            force,
            generate_editor_rules,
            include_media: None,
            max_files: None,
            background: None, // User-initiated: charge credits
            origin: None,
            reroot: None,
        };

        // Only write local index status when the path exists on this machine.
        // In HTTP mode the MCP runs remotely and the path is on the user's machine.
        //
        // Do not mark the folder indexed before the background ingest commits:
        // search/index_status would otherwise report a fresh local index while
        // the server is still serving the previous committed generation.
        let path_is_local = std::path::Path::new(&path).is_dir();
        if !path_is_local {
            // This process cannot read the folder (hosted/remote gateway).
            // Never delegate the filesystem walk to the API — the API host
            // cannot read the user's workstation either — and never turn an
            // unreadable-but-valid workstation path into a protocol error.
            // Answer with the committed server-side index state instead.
            return self
                .remote_index_state_result(
                    action,
                    project_id,
                    Some(path.as_str()),
                    context.workspace_id,
                    force.unwrap_or(false),
                )
                .await;
        }

        ContextStreamClient::write_indexing_started(&path, project_id);

        let client = self.client.clone();
        let path_for_log = path.clone();
        let path_for_success = path.clone();
        let path_for_rollback = path.clone();
        let expected_workspace_id = context.workspace_id;
        tokio::spawn(async move {
            let Some(expected_workspace_id) = expected_workspace_id else {
                tracing::error!(
                    "ingest_local skipped for {} because no workspace ownership was resolved",
                    path_for_log
                );
                ContextStreamClient::clear_index_status(&path_for_rollback);
                return;
            };
            if mcp_session::auto_init::checkout_binding_workspace(&path_for_log, project_id)
                != Some(expected_workspace_id)
            {
                tracing::error!(
                    "ingest_local skipped for {} because its checkout binding changed before execution",
                    path_for_log
                );
                ContextStreamClient::clear_index_status(&path_for_rollback);
                return;
            }
            match client.ingest_local(ingest_params).await {
                Ok(result) => {
                    if ContextStreamClient::ingest_scan_complete(&result)
                        && ContextStreamClient::ingest_result_committed(&result)
                    {
                        ContextStreamClient::write_index_status(&path_for_success, project_id);
                    }
                    let files_indexed = result
                        .get("files_indexed")
                        .or_else(|| result.get("files_changed"))
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    tracing::info!(
                        "ingest_local completed: {} files indexed from {}",
                        files_indexed,
                        path_for_log
                    );
                }
                Err(e) => {
                    tracing::error!("ingest_local failed for {}: {}", path_for_log, e);
                    ContextStreamClient::clear_index_status(&path_for_rollback);
                }
            }
        });

        let force_note = if force.unwrap_or(false) {
            " (force mode - version checks bypassed)"
        } else {
            ""
        };

        let main_msg = if context.auto_created {
            let pname = context
                .resolved_project_name
                .as_deref()
                .unwrap_or("project");
            let wname = context
                .workspace_display_name
                .as_deref()
                .unwrap_or("workspace");
            format!(
                "Created project '{}' in workspace '{}' and updating index in background{} for directory: {}.",
                pname, wname, force_note, context.path
            )
        } else {
            format!(
                "Updating index in background{} for directory: {}.",
                force_note, context.path
            )
        };
        let note = "Use 'project' with action 'index_status' to monitor progress.";
        let text = format!("{} {}", main_msg, note);

        let mut result = serde_json::json!({
            "status": "started",
            "message": text.clone(),
            "project_id": project_id.to_string(),
            "resolved_project_id": project_id.to_string(),
            "project_resolution_source": context.project.source,
            "path": path,
            "invoked_action": action,
            "auto_created": context.auto_created,
            "workspace_id": context.workspace_id.map(|id| id.to_string()),
            "note": note,
        });

        if let Some(original_project_id) = context.original_project_id {
            if original_project_id != project_id {
                if let Some(obj) = result.as_object_mut() {
                    obj.insert(
                        "original_project_id".to_string(),
                        serde_json::json!(original_project_id.to_string()),
                    );
                }
            }
        }

        Ok(ToolResult::with_structured(text, result))
    }

    /// Answer an index/ingest request whose folder is not readable from this
    /// process WITHOUT a protocol error: report the committed server-side
    /// index state, or actionable local-ingest guidance when no server-side
    /// index exists yet. Nothing is ingested by this path.
    async fn remote_index_state_result(
        &self,
        action: &str,
        project_uuid: Uuid,
        path: Option<&str>,
        workspace_id: Option<Uuid>,
        force: bool,
    ) -> Result<ToolResult> {
        let refresh_attempt = match workspace_id {
            Some(workspace_id) => Some(
                self.client
                    .request_project_refresh(
                        project_uuid,
                        workspace_id,
                        path,
                        force,
                        &format!("project.{action}"),
                    )
                    .await,
            ),
            None => None,
        };
        let (refresh_receipt, refresh_error, refresh_unsupported) = match refresh_attempt {
            Some(Ok(receipt)) => (Some(receipt), None, false),
            Some(Err(error)) if refresh_endpoint_is_unsupported(&error) => {
                (None, Some(error.user_facing_message()), true)
            }
            Some(Err(error)) => (None, Some(error.user_facing_message()), false),
            None => (None, None, false),
        };

        let (status, status_error) = match self
            .client
            .project_index_status_for_checkout(project_uuid, path)
            .await
        {
            Ok(status) => (status, None),
            Err(err) if is_not_found_error(&err) => (Value::Null, None),
            Err(err) => (Value::Null, Some(err.user_facing_message())),
        };

        let checkout_status_unconfirmed =
            ContextStreamClient::project_index_status_is_checkout_scoped(&status)
                && !ContextStreamClient::project_index_status_matches_checkout(&status);
        let (index_status_label, index_message) = if checkout_status_unconfirmed {
            (
                "checkout_index_unconfirmed",
                format!(
                    "The hosted service returned project-wide index state for project {project_uuid}, but did not confirm this exact checkout. Existing canonical project results remain searchable; active-checkout freshness remains unconfirmed while the managed sync bridge refresh is resolved."
                ),
            )
        } else {
            match classify_remote_index_disposition(&status) {
                RemoteIndexDisposition::Ready => (
                    "server_index_ready",
                    server_index_ready_message(
                        project_uuid,
                        remote_index_file_count(&status),
                        path,
                    ),
                ),
                RemoteIndexDisposition::Indexing => (
                    "server_indexing",
                    server_indexing_in_progress_message(project_uuid, path),
                ),
                RemoteIndexDisposition::RequiresSyncBridge => {
                    ("requires_sync_bridge", requires_sync_bridge_message(path))
                }
            }
        };
        let (status_label, message, refresh_requested) = match refresh_receipt.as_ref() {
            Some(receipt) => match classify_remote_refresh_disposition(receipt) {
                RemoteRefreshDisposition::Requested => (
                    "refresh_requested",
                    format!(
                        "Hosted refresh requested for project {project_uuid}. The installed ContextStream sync bridge will upload this checkout's current changes; the existing index remains searchable while that runs. Monitor with project(action=\"index_status\")."
                    ),
                    true,
                ),
                RemoteRefreshDisposition::Pending => (
                    "refresh_pending",
                    format!(
                        "Hosted refresh is already pending for project {project_uuid}. The installed ContextStream sync bridge owns the local filesystem transfer; the editor remains connected to hosted MCP. Monitor with project(action=\"index_status\")."
                    ),
                    true,
                ),
                RemoteRefreshDisposition::Completed => (
                    "refresh_completed",
                    format!(
                        "The installed ContextStream sync bridge completed the hosted refresh request for project {project_uuid}. Verify the committed generation with project(action=\"index_status\")."
                    ),
                    true,
                ),
                RemoteRefreshDisposition::BridgeOffline => (
                    "bridge_offline",
                    format!(
                        "The hosted refresh request found the registered ContextStream sync bridge offline for project {project_uuid}. Run `contextstream-mcp setup` once to repair its managed startup registration, or open ContextStream Desktop; keep this editor on hosted MCP. {index_message}"
                    ),
                    false,
                ),
                RemoteRefreshDisposition::NoCheckout => (
                    "checkout_not_registered",
                    format!(
                        "No checkout registered to this installation matches project {project_uuid}. Run `contextstream-mcp setup` once from the checkout, or add it in ContextStream Desktop; keep this editor on hosted MCP. {index_message}"
                    ),
                    false,
                ),
                RemoteRefreshDisposition::Unknown => (
                    "refresh_state_unknown",
                    format!(
                        "The hosted service returned an unrecognized refresh state for project {project_uuid}; the editor should remain on hosted MCP. {index_message}"
                    ),
                    false,
                ),
            },
            None if refresh_unsupported => (
                index_status_label,
                format!(
                    "{index_message} This hosted service does not yet expose active bridge wake-up; the installed sync bridge, editor hooks, or ContextStream Desktop will continue publishing changes automatically."
                ),
                false,
            ),
            None if refresh_error.is_some() => (
                "refresh_unconfirmed",
                format!(
                    "The hosted refresh request could not be confirmed; the editor should remain on hosted MCP. {index_message}"
                ),
                false,
            ),
            None => (
                index_status_label,
                format!(
                    "{index_message} A refresh was not requested because workspace ownership was unresolved."
                ),
                false,
            ),
        };

        let structured = serde_json::json!({
            "status": status_label,
            "message": message.clone(),
            "project_id": project_uuid.to_string(),
            "resolved_project_id": project_uuid.to_string(),
            "workspace_id": workspace_id.map(|id| id.to_string()),
            "path": path,
            "invoked_action": action,
            "ingest_performed": false,
            "refresh_requested": refresh_requested,
            "refresh_receipt": refresh_receipt,
            "refresh_error": refresh_error,
            "refresh_endpoint_unsupported": refresh_unsupported,
            "index_status": status,
            "index_status_error": status_error,
            "next_steps": [
                "project(action=\"index_status\") verifies the committed server-side index",
                "The installed ContextStream sync bridge, editor hooks, or ContextStream Desktop publish local changes while the editor stays on hosted MCP",
                "contextstream-mcp setup repairs the managed sync bridge when it is missing or offline",
            ],
        });
        Ok(ToolResult::with_structured(message, structured))
    }

    /// Trigger a server-side index job for `id`. Folder-synced projects cannot
    /// be re-walked by the server, so those answer with the committed
    /// server-side index state / local-ingest guidance instead of an error.
    async fn trigger_remote_index(
        &self,
        id: Uuid,
        path: Option<&str>,
        workspace_id: Option<Uuid>,
        force: bool,
    ) -> Result<ToolResult> {
        if path.is_some() && workspace_id.is_some() {
            return self
                .remote_index_state_result("index", id, path, workspace_id, force)
                .await;
        }
        match self.client.index_project(id).await {
            Ok(result) if api_index_result_is_skipped(&result) => {
                self.remote_index_state_result("index", id, path, workspace_id, force)
                    .await
            }
            Ok(result) => {
                let status = result
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("started");
                let text = format!("Index job {} for project {}.", status, id);
                Ok(ToolResult::with_structured(text, result))
            }
            Err(err) if requires_ingest_endpoint(&err) => {
                self.remote_index_state_result("index", id, path, workspace_id, force)
                    .await
            }
            Err(err) => Err(err),
        }
    }
}

#[async_trait]
impl ToolHandler for ProjectTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let input: ProjectInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;

        // Auto-resolve workspace/project from session if not provided
        let state = self.session.state().await;
        let config = self.client.config().await;
        let task_auth = get_task_auth_override();
        let task_workspace_id = task_auth.as_ref().and_then(|auth| auth.workspace_id);
        let task_project_id = task_auth.as_ref().and_then(|auth| auth.project_id);
        let explicit_workspace_id = input
            .workspace_id
            .as_ref()
            .and_then(|s| Uuid::parse_str(s).ok());
        let fallback_workspace_id = state
            .workspace_id
            .or(task_workspace_id)
            .or(config.default_workspace_id);
        let mut workspace_id = explicit_workspace_id.or(fallback_workspace_id);
        let explicit_project_id = input
            .project_id
            .as_ref()
            .and_then(|s| Uuid::parse_str(s).ok());
        let fallback_project_id = state.project_id.or(task_project_id);
        drop(config);
        let mut project_id = explicit_project_id.or_else(|| {
            if input
                .workspace_id
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
                && explicit_workspace_id != fallback_workspace_id
            {
                None
            } else {
                fallback_project_id
            }
        });
        let folder_path = resolve_project_tool_folder_path(
            input.folder_path.clone(),
            input.path.clone(),
            state.folder_path.clone(),
            explicit_project_id.is_some(),
        );
        let folder = folder_path.as_deref();

        match input.action.to_lowercase().as_str() {
            "list" => {
                let list_input = ProjectsListInput {
                    workspace_id: input.workspace_id,
                    page: input.page,
                    page_size: input.page_size,
                };
                let tool = ProjectsListTool::new(self.client.clone());
                tool.execute(serde_json::to_value(&list_input).unwrap()).await
            }
            "get" => {
                let resolved = resolve_read_project(
                    &self.client,
                    &self.session,
                    "get",
                    workspace_id,
                    explicit_project_id,
                    project_id,
                    folder,
                )
                .await?;
                let project = resolved.project;
                let text = format!("Project loaded: {} ({})", project.name, project.id);
                Ok(ToolResult::with_structured(text, serde_json::to_value(&project).unwrap_or_default()))
            }
            "create" => {
                let name = input.name.ok_or_else(|| Error::Validation("name is required for create".to_string()))?;
                let create_input = ProjectsCreateInput {
                    name,
                    description: input.description,
                    workspace_id: workspace_id.map(|id| id.to_string()),
                };
                let tool = ProjectsCreateTool::new(self.client.clone());
                tool.execute(serde_json::to_value(&create_input).unwrap()).await
            }
            "update" => {
                let id = project_id.ok_or_else(|| {
                    missing_project_scope_error("update", workspace_id, folder)
                })?;
                let project = self.client.update_project(
                    id,
                    input.name.as_deref(),
                    input.description.as_deref(),
                ).await?;
                let text = format!("Project updated: {} ({})", project.name, project.id);
                Ok(ToolResult::with_structured(text, serde_json::to_value(&project).unwrap_or_default()))
            }
            "index" => {
                // Local ingest only when THIS process can actually read the
                // folder. On a hosted/remote gateway the session-injected
                // folder path lives on the user's machine, not here — those
                // requests are answered from committed server-side index
                // state instead of attempting a doomed local walk or
                // delegating the walk to the API.
                let folder_is_readable_dir = folder_path
                    .as_deref()
                    .map(|p| std::path::Path::new(p).is_dir())
                    .unwrap_or(false);

                if folder_is_readable_dir {
                    let context = self
                        .resolve_ingest_context(
                            "index",
                            folder_path.clone(),
                            workspace_id,
                            explicit_project_id,
                            fallback_project_id,
                            input.skip_project_creation.unwrap_or(false),
                        )
                        .await?;

                    return self.start_background_ingest(
                        "index",
                        context,
                        input.force,
                        input.generate_editor_rules,
                    ).await;
                }

                if let Some(path) = folder_path.as_deref() {
                    // Folder present but not readable here. stdio keeps its
                    // Path-not-found / not-a-directory errors (typo
                    // protection); HTTP transport passes validation and is
                    // answered from server-side state below.
                    validate_ingest_directory(path, self.client.config().await.is_http_transport)?;
                }

                let id = if let Some(project_id_str) = input.project_id.as_deref() {
                    Uuid::parse_str(project_id_str).map_err(|_| {
                        Error::Validation(
                            "Invalid project_id format. Provide a valid UUID.".to_string(),
                        )
                    })?
                } else {
                    project_id.ok_or_else(|| {
                        missing_project_scope_error("index", workspace_id, folder)
                    })?
                };

                self.trigger_remote_index(
                    id,
                    folder_path.as_deref(),
                    workspace_id,
                    input.force.unwrap_or(false),
                )
                    .await
            }
            "delete" => {
                let id = project_id.ok_or_else(|| {
                    missing_project_scope_error("delete", workspace_id, folder)
                })?;
                let result = self.client.delete_project(id).await?;
                Ok(ToolResult::with_structured(
                    format!("Project deleted: {}.", id),
                    result
                ))
            }
            "purge" => {
                let id = project_id.ok_or_else(|| {
                    missing_project_scope_error("purge", workspace_id, folder)
                })?;
                let result = self.client.purge_project_index(id).await?;
                Ok(ToolResult::with_structured(
                    format!(
                        "Project index purged: {} — file_indices, code chunks, search vectors, and stored file objects removed. The project record itself is preserved (use action=\"delete\" to remove the project too, or action=\"forget_local\" to stop this machine from re-indexing the folder).",
                        id
                    ),
                    result,
                ))
            }
            "forget_local" => {
                // Stop this machine from re-indexing a folder: remove its global
                // mapping + local index registry entry so auto-init/keep-warm
                // won't re-resolve or re-seed it, and drop the active session's
                // project scope so the in-session aging tick won't immediately
                // re-ingest. Purely LOCAL — server-side index/content is
                // untouched (pair with action="purge"/"delete" to remove that).
                let target = folder_path
                    .clone()
                    .or_else(|| state.folder_path.clone())
                    .ok_or_else(|| {
                        Error::Validation(
                            "forget_local needs a folder. Pass folder_path=\"<dir>\", or run init(folder_path=\"...\") first.".to_string(),
                        )
                    })?;

                let mapping_removed = mcp_session::auto_init::remove_global_mapping(&target).await;
                ContextStreamClient::clear_index_status(&target);
                // Also clear the per-folder .contextstream/config.json project
                // scope — it's a fallback the PostToolUse persist path uses to
                // re-resolve scope, so leaving it would let the mapping be
                // re-seeded right after we removed it (re-opening the keep-warm
                // re-seed this action is meant to close).
                let local_config_cleared =
                    mcp_session::auto_init::clear_local_config_project(&target).await;

                // If the active session points at this folder, clear its project
                // scope so the in-session aging tick stops treating it as a
                // project to re-ingest. replace_scope assigns unconditionally —
                // update_scope(None) would be a no-op for project_id.
                let session_scope_cleared = state
                    .folder_path
                    .as_deref()
                    .is_some_and(|f| folder_paths_equivalent(f, &target))
                    && state.project_id.is_some();
                if session_scope_cleared {
                    self.session
                        .replace_scope(state.workspace_id, None, Some(target.clone()))
                        .await;
                }

                let text = format!(
                    "Forgot local indexing state for {target}: global mapping {}, local index registry cleared{}{}. Server-side index/content is unchanged — run project(action=\"purge\") to remove the indexed content too.",
                    if mapping_removed { "removed" } else { "not found" },
                    if local_config_cleared {
                        ", per-folder config project scope cleared"
                    } else {
                        ""
                    },
                    if session_scope_cleared {
                        ", and cleared the active session's project scope (re-init to use this folder again)"
                    } else {
                        ""
                    }
                );
                Ok(ToolResult::with_structured(
                    text,
                    serde_json::json!({
                        "folder_path": target,
                        "mapping_removed": mapping_removed,
                        "index_status_cleared": true,
                        "local_config_cleared": local_config_cleared,
                        "session_scope_cleared": session_scope_cleared,
                    }),
                ))
            }
            "remove_paths" => {
                let id = project_id.ok_or_else(|| {
                    missing_project_scope_error("remove_paths", workspace_id, folder)
                })?;
                let paths = input
                    .paths
                    .clone()
                    .filter(|p| !p.is_empty())
                    .ok_or_else(|| {
                        Error::Validation(
                            "remove_paths requires a non-empty \"paths\" array of indexed file paths (relative to the project root) to de-index.".to_string(),
                        )
                    })?;
                let result = self.client.remove_project_files(id, paths).await?;
                let removed = result
                    .get("removed_count")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let not_found = result
                    .get("not_found")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                Ok(ToolResult::with_structured(
                    format!(
                        "Removed {removed} file(s) from project {id}'s index — vectors, indexed rows, and stored files deleted ({not_found} path(s) not found). The project record is preserved.",
                    ),
                    result,
                ))
            }
            "merge" | "combine" => {
                let target_id = project_id.ok_or_else(|| {
                    missing_project_scope_error("merge", workspace_id, folder)
                })?;
                let source_project_id = input.source_project_id.as_deref().ok_or_else(|| {
                    Error::Validation(
                        "source_project_id is required for project merge/combine.".to_string(),
                    )
                })?;
                let source_id = Uuid::parse_str(source_project_id).map_err(|_| {
                    Error::Validation(
                        "Invalid source_project_id format. Provide a valid UUID.".to_string(),
                    )
                })?;
                if source_id == target_id {
                    return Err(Error::Validation(
                        "source_project_id must be different from project_id.".to_string(),
                    ));
                }

                let result = self.client.merge_project(target_id, source_id).await?;
                self.session
                    .update_scope(workspace_id, Some(target_id), folder_path.clone())
                    .await;

                let target_name = result
                    .get("target_project")
                    .and_then(|project| project.get("name"))
                    .and_then(|name| name.as_str())
                    .unwrap_or("target project");
                let reindex_note = if result
                    .get("reindex_recommended")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false)
                {
                    " Re-index the target project to refresh search vectors."
                } else {
                    ""
                };
                Ok(ToolResult::with_structured(
                    format!(
                        "Projects combined: source project {} merged into {} ({}).{}",
                        source_id, target_name, target_id, reindex_note
                    ),
                    result,
                ))
            }
            "overview" => {
                let id = resolve_read_project(
                    &self.client,
                    &self.session,
                    "overview",
                    workspace_id,
                    explicit_project_id,
                    project_id,
                    folder,
                )
                .await?
                .project_id;
                let result = self.client.project_overview(id).await?;
                Ok(ToolResult::with_structured("Project overview loaded.".to_string(), result))
            }
            "statistics" => {
                let id = resolve_read_project(
                    &self.client,
                    &self.session,
                    "statistics",
                    workspace_id,
                    explicit_project_id,
                    project_id,
                    folder,
                )
                .await?
                .project_id;
                let result = self.client.project_statistics(id).await?;
                Ok(ToolResult::with_structured("Project statistics loaded.".to_string(), result))
            }
            "files" => {
                let id = resolve_read_project(
                    &self.client,
                    &self.session,
                    "files",
                    workspace_id,
                    explicit_project_id,
                    project_id,
                    folder,
                )
                .await?
                .project_id;
                let result = self
                    .client
                    .project_files_for_checkout(
                        id,
                        input.page,
                        input.page_size,
                        input.path_pattern.as_deref(),
                        input.sort_by.as_deref(),
                        input.sort_order.as_deref(),
                        folder,
                    )
                    .await?;
                let text = format_project_files_text(&result);
                Ok(ToolResult::with_structured(text, result))
            }
            "index_status" => {
                // Resolve project from folder context when session/default IDs are stale.
                let mut resolved_folder_project_id = None;
                if let Some(path) = folder {
                    if let Some(mapping) = resolve_workspace(path).await {
                        if workspace_id.is_none() {
                            workspace_id = Some(mapping.workspace_id);
                        }
                        resolved_folder_project_id = mapping.project_id;
                    }
                }
                let local_index_project_id =
                    folder.and_then(ContextStreamClient::tracked_project_id_for_folder);
                let local_ready_project_id =
                    folder.and_then(ContextStreamClient::indexed_project_id_for_folder);
                let folder_scope_mismatch = folder_scope_mismatches_explicit_project(
                    explicit_project_id,
                    resolved_folder_project_id,
                    local_index_project_id,
                );
                let status_folder = if folder_scope_mismatch { None } else { folder };

                let mut candidate_ids: Vec<Uuid> = Vec::new();
                if let Some(id) = explicit_project_id {
                    candidate_ids.push(id);
                }
                if explicit_project_id.is_none() {
                    if let Some(id) = resolved_folder_project_id {
                        if !candidate_ids.contains(&id) {
                            candidate_ids.push(id);
                        }
                    }
                    if let Some(id) = local_index_project_id {
                        if !candidate_ids.contains(&id) {
                            candidate_ids.push(id);
                        }
                    }
                    if let Some(id) = project_id {
                        if !candidate_ids.contains(&id) {
                            candidate_ids.push(id);
                        }
                    }
                } else if !folder_scope_mismatch {
                    // Keep explicit ID first, but allow folder/local fallback candidates.
                    if let Some(id) = resolved_folder_project_id {
                        if !candidate_ids.contains(&id) {
                            candidate_ids.push(id);
                        }
                    }
                    if let Some(id) = local_index_project_id {
                        if !candidate_ids.contains(&id) {
                            candidate_ids.push(id);
                        }
                    }
                }

                if candidate_ids.is_empty() {
                    return Err(missing_project_scope_error(
                        "index_status",
                        workspace_id,
                        folder,
                    ));
                }

                let mut selected: Option<(usize, Uuid, serde_json::Value, bool, bool)> = None;
                for (idx, candidate_id) in candidate_ids.iter().enumerate() {
                    match self
                        .client
                        .project_index_status_for_checkout(*candidate_id, status_folder)
                        .await
                    {
                        Ok(api_result) => {
                            let checkout_status_unconfirmed =
                                ContextStreamClient::project_index_status_is_checkout_scoped(
                                    &api_result,
                                ) && !ContextStreamClient::project_index_status_matches_checkout(
                                    &api_result,
                                );
                            let canonical_api_indexed = api_result_reports_canonical_indexed(
                                &api_result,
                                checkout_status_unconfirmed,
                            );

                            if canonical_api_indexed
                                && (!checkout_status_unconfirmed
                                    || explicit_project_id == Some(*candidate_id))
                            {
                                selected = Some((
                                    idx,
                                    *candidate_id,
                                    api_result,
                                    canonical_api_indexed,
                                    checkout_status_unconfirmed,
                                ));
                                break;
                            }

                            let selected_is_canonically_indexed = selected
                                .as_ref()
                                .is_some_and(|(_, _, _, indexed, _)| *indexed);
                            if selected.is_none()
                                || (canonical_api_indexed && !selected_is_canonically_indexed)
                            {
                                selected = Some((
                                    idx,
                                    *candidate_id,
                                    api_result,
                                    canonical_api_indexed,
                                    checkout_status_unconfirmed,
                                ));
                            }
                        }
                        Err(err) if is_not_found_error(&err) => continue,
                        Err(err) => return Err(err),
                    }
                }

                let (
                    resolved_candidate_index,
                    resolved_project_id,
                    mut result,
                    api_indexed,
                    checkout_status_unconfirmed,
                ) = selected.ok_or_else(|| {
                        Error::Validation(
                            "Project not found for current context. Call init(...) in this folder or pass a valid project_id explicitly."
                                .to_string(),
                        )
                    })?;

                let original_project_id = project_id;
                if project_id != Some(resolved_project_id) && folder_path.is_some() {
                    project_id = Some(resolved_project_id);
                    self.session
                        .update_scope(workspace_id, project_id, folder_path.clone())
                        .await;
                }

                // Only treat local index metadata as valid when it maps to the resolved project.
                let locally_indexed =
                    !folder_scope_mismatch && local_ready_project_id == Some(resolved_project_id);

                let api_indexing = api_result_is_indexing(&result);
                let local_indexing_started_at = status_folder
                    .and_then(ContextStreamClient::local_indexing_started_at)
                    .filter(|started_at| {
                        chrono::Utc::now()
                            .signed_duration_since(*started_at)
                            .num_hours()
                            <= LOCAL_INDEXING_STARTED_VISIBLE_HOURS
                    });
                let local_indexing = local_indexing_started_at.is_some();
                let index_in_progress = api_indexing || local_indexing;
                let indexed = api_indexed || locally_indexed;
                let indexed_at = extract_index_timestamp(&result);
                let age_hours = indexed_at
                    .map(|ts| chrono::Utc::now().signed_duration_since(ts).num_hours());
                let freshness = classify_index_freshness(indexed, age_hours);
                let (confidence, confidence_reason) =
                    classify_index_confidence(indexed, api_indexed, locally_indexed, freshness);
                let pending_paths = extract_pending_file_paths(&result);
                let pending_files = extract_pending_files(&result).unwrap_or({
                    if pending_paths.is_empty() {
                        0
                    } else {
                        pending_paths.len() as i64
                    }
                });
                let checkout_scope_requested =
                    ContextStreamClient::project_index_status_is_checkout_scoped(&result);
                if let Some(obj) = result.as_object_mut() {
                    obj.insert("indexed".to_string(), serde_json::json!(indexed));
                    if locally_indexed && !api_indexed {
                        obj.insert("indexed_source".to_string(), serde_json::json!("local"));
                    }
                    obj.insert("index_freshness".to_string(), serde_json::json!(freshness));
                    obj.insert("index_age_hours".to_string(), serde_json::json!(age_hours));
                    obj.insert("index_confidence".to_string(), serde_json::json!(confidence));
                    obj.insert(
                        "index_confidence_reason".to_string(),
                        serde_json::json!(confidence_reason),
                    );
                    obj.insert(
                        "index_timestamp".to_string(),
                        serde_json::json!(indexed_at.map(|ts| ts.to_rfc3339())),
                    );
                    obj.insert(
                        "canonical_index_ready".to_string(),
                        serde_json::json!(api_indexed),
                    );
                    obj.insert(
                        "checkout_scope_confirmed".to_string(),
                        if checkout_scope_requested {
                            serde_json::json!(!checkout_status_unconfirmed)
                        } else {
                            serde_json::Value::Null
                        },
                    );
                    obj.insert(
                        "checkout_scope_status".to_string(),
                        serde_json::json!(
                            if checkout_status_unconfirmed {
                                "unconfirmed"
                            } else if checkout_scope_requested {
                                "confirmed"
                            } else {
                                "not_requested"
                            }
                        ),
                    );
                    if original_project_id != Some(resolved_project_id) {
                        obj.insert(
                            "original_project_id".to_string(),
                            serde_json::json!(original_project_id.map(|id| id.to_string())),
                        );
                    }
                    obj.insert(
                        "resolved_project_id".to_string(),
                        serde_json::json!(resolved_project_id.to_string()),
                    );
                    obj.insert(
                        "resolution_rank".to_string(),
                        serde_json::json!(resolved_candidate_index),
                    );
                    obj.insert(
                        "index_in_progress".to_string(),
                        serde_json::json!(index_in_progress),
                    );
                    if let Some(started_at) = local_indexing_started_at {
                        obj.insert(
                            "local_indexing_started_at".to_string(),
                            serde_json::json!(started_at.to_rfc3339()),
                        );
                    }
                    if obj.get("pending_files").is_none() {
                        obj.insert("pending_files".to_string(), serde_json::json!(pending_files));
                    }
                    if !pending_paths.is_empty() && obj.get("pending_file_paths").is_none() {
                        obj.insert(
                            "pending_file_paths".to_string(),
                            serde_json::json!(pending_paths.clone()),
                        );
                    }
                    if index_in_progress && pending_files > 0 && pending_paths.is_empty() {
                        obj.insert(
                            "pending_files_diagnostic".to_string(),
                            serde_json::json!(
                                "API reports pending_files but did not provide pending file paths."
                            ),
                        );
                    }
                }
                let mut text = if index_in_progress {
                    if indexed {
                        "Project indexing is in progress. Search is using the latest committed generation."
                    } else {
                        "Project indexing is in progress. Keyword search works now; semantic search comes online after the first commit. Use search and retry as it fills in; do not fall back to local tools while it is building."
                    }
                } else if indexed {
                    if checkout_status_unconfirmed {
                        "Project index is ready (canonical state). Semantic search is available."
                    } else if locally_indexed && !api_indexed {
                        "Project index is ready (local state). Semantic search is available."
                    } else {
                        "Project index is ready. Semantic search is available."
                    }
                } else {
                    "Project index not found. Keep hosted MCP configured. Re-establish the intended checkout with init(folder_path=\"<folder>\"), then run project(action=\"index\"); the exact-checkout sync bridge will provide local bytes. Until then use ContextStream search first; fall back to local tools only if search itself returns nothing."
                }
                .to_string();
                if checkout_status_unconfirmed {
                    if indexed {
                        text.push_str(
                            " The hosted service did not confirm this exact checkout overlay, so uncommitted or very recent worktree changes may not yet be included. This is not a missing-index condition; keep hosted MCP configured and run project(action=\"index\") to request a managed sync-bridge refresh.",
                        );
                    } else {
                        text.push_str(
                            " The hosted service also did not confirm this exact checkout overlay; keep hosted MCP configured and repair or re-register the managed sync bridge before relying on active-worktree freshness.",
                        );
                    }
                }
                text.push_str(&format_index_freshness_text(freshness, age_hours, indexed));
                if confidence != "high" {
                    text.push_str(&format!(
                        " Confidence: {}. {}",
                        confidence, confidence_reason
                    ));
                } else if api_indexed && !locally_indexed {
                    text.push_str(" API confirms index readiness.");
                }
                if api_indexing && pending_files > 0 && pending_paths.is_empty() {
                    text.push_str(" Pending file paths are unavailable in this API response.");
                }
                if local_indexing {
                    text.push_str(" Local refresh has started; freshness will update after the committed generation completes.");
                } else if matches!(freshness, "stale" | "missing") {
                    let refresh_path = status_folder.unwrap_or("<folder>");
                    text.push_str(&format!(
                        " {}",
                        hosted_index_refresh_instruction(refresh_path)
                    ));
                }
                if folder_scope_mismatch {
                    text.push_str(
                        " Current folder metadata points at a different project; re-run init(folder_path=\"...\") in the intended checkout or pass the correct path explicitly.",
                    );
                }
                Ok(ToolResult::with_structured(text, result))
            }
            "index_history" => {
                let id = resolve_read_project(
                    &self.client,
                    &self.session,
                    "index_history",
                    workspace_id,
                    explicit_project_id,
                    project_id,
                    folder,
                )
                .await?
                .project_id;
                let mut result = self.client.project_index_history(
                    id,
                    input.page,
                    input.page_size,
                    input.since.as_deref(),
                    input.until.as_deref(),
                    input.machine_id.as_deref(),
                    input.branch.as_deref(),
                ).await?;
                let count = index_history_entry_count(&result);
                if let Some(obj) = result.as_object_mut() {
                    obj.insert("entries_count".to_string(), serde_json::json!(count));
                }
                Ok(ToolResult::with_structured(format!("Found {} index history entries.", count), result))
            }
            "ingest_local" => {
                let context = self
                    .resolve_ingest_context(
                        "ingest_local",
                        folder_path.clone(),
                        workspace_id,
                        explicit_project_id,
                        fallback_project_id,
                        input.skip_project_creation.unwrap_or(false),
                    )
                    .await?;

                self.start_background_ingest(
                    "ingest_local",
                    context,
                    input.force,
                    input.generate_editor_rules,
                ).await
            }
            "team_projects" => {
                let result = self.client.team_projects(workspace_id, input.page, input.page_size).await?;
                let count = result.as_array().map(|a| a.len()).unwrap_or(0);
                Ok(ToolResult::with_structured(format!("Found {} team projects.", count), result))
            }
            "recent_changes" => {
                let repo_path = folder_path.clone()
                    .or_else(|| std::env::current_dir().ok().map(|p| p.to_string_lossy().to_string()))
                    .ok_or_else(|| {
                    Error::Validation(
                        "folder_path (or path) is required for recent_changes. Call init(...) first or pass path explicitly."
                            .to_string(),
                    )
                })?;

                // Verify git is available and path is a git repo
                let git_root = find_git_root(&repo_path).ok_or_else(|| {
                    Error::Validation(format!(
                        "No git repository found at or above: {}",
                        repo_path
                    ))
                })?;

                let commit_limit = input.limit.unwrap_or(10).clamp(1, 50) as usize;
                let since_arg = input.since.as_deref();

                // Run git log and git diff --stat in parallel
                let log_fut = run_git_log(&git_root, commit_limit, since_arg);
                let diff_fut = run_git_diff_stat(&git_root);
                let (log_result, diff_result) = tokio::join!(log_fut, diff_fut);

                let commits = log_result.unwrap_or_default();
                let diff_stat = diff_result.unwrap_or_default();

                // Build structured response
                let commits_json: Vec<Value> = commits
                    .iter()
                    .map(|c| {
                        serde_json::json!({
                            "hash": c.hash,
                            "message": c.message,
                            "author": c.author,
                            "date": c.date,
                            "files_changed": c.files_changed,
                        })
                    })
                    .collect();

                let result = serde_json::json!({
                    "commits": commits_json,
                    "commit_count": commits.len(),
                    "diff_stat": {
                        "files_modified": diff_stat.files_modified,
                        "insertions": diff_stat.insertions,
                        "deletions": diff_stat.deletions,
                        "summary": diff_stat.summary,
                        "files": diff_stat.files,
                    },
                    "git_root": git_root,
                });

                // Format human-readable text
                let mut text = String::new();
                if commits.is_empty() {
                    text.push_str("No recent commits found.");
                } else {
                    text.push_str(&format!(
                        "Recent {} commits in {}:\n\n",
                        commits.len(),
                        git_root
                    ));
                    for c in &commits {
                        text.push_str(&format!(
                            "  {} {} — {} ({})\n",
                            &c.hash[..7.min(c.hash.len())],
                            c.message,
                            c.author,
                            c.date
                        ));
                        if !c.files_changed.is_empty() {
                            for f in &c.files_changed {
                                text.push_str(&format!("    {}\n", f));
                            }
                        }
                    }
                }
                if !diff_stat.summary.is_empty() {
                    text.push_str(&format!(
                        "\nUncommitted changes: {}\n",
                        diff_stat.summary
                    ));
                }

                Ok(ToolResult::with_structured(text, result))
            }
            _ => Err(Error::Validation(format!(
                "Unknown action: {}. Available actions: list, get, create, update, merge, combine, index, delete, purge, forget_local, remove_paths, overview, statistics, files, index_status, index_history, ingest_local, team_projects, recent_changes.",
                input.action
            ))),
        }
    }

    fn metadata(&self) -> &ToolMetadata {
        static METADATA: std::sync::OnceLock<ToolMetadata> = std::sync::OnceLock::new();
        METADATA.get_or_init(|| ToolMetadata {
            name: "project".to_string(),
            title: "Project Operations".to_string(),
            description: "Project management. Actions: list, get, create, update, merge/combine duplicate projects, index (preferred hosted workflow: requests the registered managed sync bridge for the exact checkout, or ingests directly only when this process already has disk access; requires_sync_bridge means repair bridge/hooks/Desktop while keeping hosted MCP), delete (remove the project), purge (completely de-index a project — removes file_indices, code chunks, search vectors, and stored files but keeps the project record), forget_local (stop this machine from re-indexing a folder: removes its local mapping/registry entry and drops the active session's project scope; server data untouched), remove_paths (de-index specific files by exact path — deletes their vectors, indexed rows, and stored files server-side, but keeps the project; pass paths=[...]), overview, statistics, files, index_status, index_history (audit trail of indexed files), ingest_local (optional direct indexing only when this process can read the folder; reuses a unique credential-free Git repository match across machines/worktrees, otherwise creates a project and records that identity — pass skip_project_creation=true to disable creation), team_projects (list all team projects - team plans only), recent_changes (git log/diff for recent file changes).".to_string(),
            category: ToolCategory::Project,
            annotations: ToolAnnotations::destructive(),
            is_pro: false,
            required_tier: None,
        })
    }

    fn input_schema(&self) -> Value {
        let all_actions = &[
            "list",
            "get",
            "create",
            "update",
            "merge",
            "combine",
            "index",
            "delete",
            "purge",
            "forget_local",
            "remove_paths",
            "overview",
            "statistics",
            "files",
            "index_status",
            "index_history",
            "ingest_local",
            "team_projects",
            "recent_changes",
        ];

        SchemaBuilder::new()
            .description("Project operations")
            .string_enum("action", "Operation to perform", all_actions, true)
            // Common fields
            .string("name", "Project name (for create/update)", false)
            .string(
                "description",
                "Project description (for create/update)",
                false,
            )
            .uuid("workspace_id", "Workspace ID", false)
            .uuid("project_id", "Project ID", false)
            .uuid(
                "source_project_id",
                "Source project ID to merge into project_id (for merge/combine)",
                false,
            )
            .integer(
                "page",
                "Page number (for list/files/index_history/team_projects)",
                false,
            )
            .integer("page_size", "Results per page", false)
            // Files fields
            .string(
                "path_pattern",
                "Filter by file path pattern (for files)",
                false,
            )
            .string_enum("sort_by", "Sort field (for files)", VALID_SORT_BY, false)
            .string_enum(
                "sort_order",
                "Sort order (for files)",
                VALID_SORT_ORDER,
                false,
            )
            // Index history fields
            .string(
                "since",
                "ISO timestamp - filter after this time (for index_history)",
                false,
            )
            .string(
                "until",
                "ISO timestamp - filter before this time (for index_history)",
                false,
            )
            .string(
                "machine_id",
                "Filter by machine ID (for index_history)",
                false,
            )
            .string("branch", "Filter by git branch (for index_history)", false)
            // Index / ingest-local fields
            .string(
                "path",
                "Checkout path for index/ingest_local; index uses it to resolve exact checkout routing",
                false,
            )
            .string(
                "folder_path",
                "Alias for path (for index/ingest_local)",
                false,
            )
            .array(
                "paths",
                "Exact indexed file paths (relative to the project root) to de-index (for remove_paths)",
                "string",
                false,
            )
            .boolean(
                "force",
                "Force re-index all files (for ingest_local)",
                false,
            )
            .boolean(
                "generate_editor_rules",
                "Generate editor rules (for ingest_local)",
                false,
            )
            .boolean(
                "skip_project_creation",
                "When true, ingest_local / folder index do not auto-create a project if scope is missing",
                false,
            )
            // Recent changes fields
            .integer(
                "limit",
                "Maximum commits to return (for recent_changes, default: 10, max: 50)",
                false,
            )
            .build()
    }
}

// ============================================================================
// Git helpers for recent_changes
// ============================================================================

#[derive(Debug, Clone, Default)]
struct GitCommit {
    hash: String,
    message: String,
    author: String,
    date: String,
    files_changed: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct GitDiffStat {
    files_modified: usize,
    insertions: usize,
    deletions: usize,
    summary: String,
    files: Vec<String>,
}

/// Walk up from `path` to find the nearest `.git` directory and return the repo root.
fn find_git_root(path: &str) -> Option<String> {
    let mut current = std::path::Path::new(path);
    loop {
        if current.join(".git").exists() {
            return Some(current.to_string_lossy().to_string());
        }
        current = current.parent()?;
    }
}

/// Run `git log` and parse structured commit info including per-commit changed files.
async fn run_git_log(
    repo_path: &str,
    limit: usize,
    since: Option<&str>,
) -> std::result::Result<Vec<GitCommit>, String> {
    const RECORD_SEP: char = '\u{1e}';
    const FIELD_SEP: char = '\u{1f}';
    let format_str = format!(
        "{record}%H{field}%s{field}%an{field}%ar",
        record = RECORD_SEP,
        field = FIELD_SEP
    );

    let mut args = vec![
        "log".to_string(),
        format!("--format={}", format_str),
        "--name-only".to_string(),
        format!("-n{}", limit),
    ];
    if let Some(since_val) = since {
        args.push(format!("--since={}", since_val));
    }

    let output = Command::new("git")
        .args(&args)
        .current_dir(repo_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("Failed to run git: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git log failed: {}", stderr.trim()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut commits: Vec<GitCommit> = Vec::new();

    for chunk in stdout.split(RECORD_SEP).skip(1) {
        let parts: Vec<&str> = chunk.splitn(4, FIELD_SEP).collect();
        if parts.len() < 4 {
            continue;
        }

        let hash = parts[0].trim().to_string();
        let message = parts[1].trim().to_string();
        let author = parts[2].trim().to_string();

        // The date part may have trailing file names separated by newlines
        let date_and_files = parts[3];
        let mut lines = date_and_files.lines();
        let date = lines.next().unwrap_or("").trim().to_string();

        let files_changed: Vec<String> = lines
            .map(|l| l.trim())
            .filter(|l| !l.is_empty())
            .map(|l| l.to_string())
            .collect();

        if !hash.is_empty() {
            commits.push(GitCommit {
                hash,
                message,
                author,
                date,
                files_changed,
            });
        }
    }

    Ok(commits)
}

/// Run `git diff --stat` for uncommitted changes.
async fn run_git_diff_stat(repo_path: &str) -> std::result::Result<GitDiffStat, String> {
    let output = Command::new("git")
        .args(["diff", "--stat", "HEAD"])
        .current_dir(repo_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("Failed to run git diff: {}", e))?;

    if !output.status.success() {
        // Could be an empty repo or detached HEAD — return empty rather than error
        return Ok(GitDiffStat::default());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();

    if lines.is_empty() {
        return Ok(GitDiffStat::default());
    }

    // Last line is the summary: " N files changed, X insertions(+), Y deletions(-)"
    let summary_line = lines.last().copied().unwrap_or("").trim().to_string();

    // All lines except the last are individual file stats
    let files: Vec<String> = lines
        .iter()
        .take(lines.len().saturating_sub(1))
        .map(|l| {
            // Extract just the file path (before the " |" separator)
            l.split('|').next().unwrap_or(l).trim().to_string()
        })
        .filter(|f| !f.is_empty())
        .collect();

    // Parse summary numbers
    let mut files_modified = files.len();
    let mut insertions = 0usize;
    let mut deletions = 0usize;

    let summary_lower = summary_line.to_lowercase();
    // Parse "N files changed"
    if let Some(pos) = summary_lower.find("file") {
        if let Ok(n) = summary_lower[..pos].trim().parse::<usize>() {
            files_modified = n;
        }
    }
    // Parse "X insertions(+)"
    if let Some(pos) = summary_lower.find("insertion") {
        let before = &summary_lower[..pos];
        if let Some(num_str) = before.rsplit(", ").next().or(before.rsplit(' ').next()) {
            if let Ok(n) = num_str.trim().parse::<usize>() {
                insertions = n;
            }
        }
    }
    // Parse "Y deletions(-)"
    if let Some(pos) = summary_lower.find("deletion") {
        let before = &summary_lower[..pos];
        if let Some(num_str) = before.rsplit(", ").next().or(before.rsplit(' ').next()) {
            if let Ok(n) = num_str.trim().parse::<usize>() {
                deletions = n;
            }
        }
    }

    Ok(GitDiffStat {
        files_modified,
        insertions,
        deletions,
        summary: summary_line,
        files,
    })
}

/// Register all project tools.
pub fn register_project_tools(
    registry: &mut crate::registry::ToolRegistry,
    client: ContextStreamClient,
    session: Arc<SessionManager>,
) {
    registry.register(
        "project",
        Arc::new(ProjectTool::new(client.clone(), session)),
    );
    registry.register(
        "projects_list",
        Arc::new(ProjectsListTool::new(client.clone())),
    );
    registry.register(
        "projects_create",
        Arc::new(ProjectsCreateTool::new(client.clone())),
    );
    registry.register(
        "projects_index",
        Arc::new(ProjectsIndexTool::new(client.clone())),
    );
}

#[cfg(test)]
#[path = "project_tests.rs"]
mod tests;
