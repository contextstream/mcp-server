use mcp_client::{get_task_auth_override, ContextStreamClient};
use mcp_session::{auto_init::resolve_workspace, SessionManager};
use mcp_types::{api::SearchResponse, Error, ErrorCode, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};
use tracing::debug;
use uuid::Uuid;

#[derive(Debug, Clone, Default)]
pub struct ResolvedReadScope {
    pub workspace_id: Option<Uuid>,
    pub project_id: Option<Uuid>,
    pub related_project_ids: Vec<Uuid>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ResolvedWriteScope {
    pub workspace_id: Option<Uuid>,
    pub project_id: Option<Uuid>,
    pub requested_project_id: Option<Uuid>,
    pub stale_project_id: Option<Uuid>,
    pub scope_recovered: bool,
    pub note: Option<String>,
}

fn parse_scope_uuid(
    raw: Option<&str>,
    fallback: Option<Uuid>,
    field_name: &str,
) -> Result<Option<Uuid>> {
    match raw.map(str::trim) {
        Some("") | None => Ok(fallback),
        Some(value) => Uuid::parse_str(value)
            .map(Some)
            .map_err(|_| Error::Validation(format!("Invalid {} UUID: {}", field_name, value))),
    }
}

/// Like `parse_scope_uuid` but, when the input isn't a UUID, falls back to
/// matching the value against project names in the resolved workspace —
/// agents commonly pass a project *name* like "vscode" instead of looking up
/// the UUID. We accept either form. If neither parses as UUID nor matches a
/// project name, returns a helpful error mentioning a few known names.
async fn parse_project_scope_id(
    client: &ContextStreamClient,
    workspace_id: Option<Uuid>,
    raw: Option<&str>,
    fallback: Option<Uuid>,
) -> Result<Option<Uuid>> {
    let value = match raw.map(str::trim) {
        Some("") | None => return Ok(fallback),
        Some(v) => v,
    };

    if let Ok(uuid) = Uuid::parse_str(value) {
        return Ok(Some(uuid));
    }

    let Some(ws) = workspace_id else {
        return Err(Error::Validation(format!(
            "Invalid project_id UUID: {}. workspace_id must be resolved before \
             a project name can be looked up.",
            value
        )));
    };

    let folder_key = value
        .trim()
        .to_ascii_lowercase()
        .chars()
        .filter(|ch| !matches!(ch, ' ' | '_' | '-'))
        .collect::<String>();

    let projects = client
        .list_projects(Some(ws), Some(1), Some(200))
        .await
        .ok();
    if let Some(projects) = projects {
        for project in &projects {
            let name = project.name.trim();
            let name_key: String = name
                .to_ascii_lowercase()
                .chars()
                .filter(|ch| !matches!(ch, ' ' | '_' | '-'))
                .collect();
            if name.eq_ignore_ascii_case(value) || name_key == folder_key {
                return Ok(Some(project.id));
            }
        }
        let mut sample: Vec<String> = projects.iter().take(5).map(|p| p.name.clone()).collect();
        if projects.len() > 5 {
            sample.push(format!("…+{} more", projects.len() - 5));
        }
        Err(Error::Validation(format!(
            "Invalid project_id UUID: {}. No project in this workspace matches \
             that name either. Known projects: {}.",
            value,
            sample.join(", ")
        )))
    } else {
        Err(Error::Validation(format!(
            "Invalid project_id UUID: {}. Could not list workspace projects to \
             try a name match. Pass a UUID instead.",
            value
        )))
    }
}

fn push_unique_project_candidate(candidates: &mut Vec<Uuid>, candidate: Option<Uuid>) {
    if let Some(candidate) = candidate {
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }
}

fn is_not_found_error(err: &Error) -> bool {
    matches!(
        err,
        Error::Http {
            code: ErrorCode::NotFound,
            ..
        }
    )
}

fn is_stale_folder_workspace_error(error: &Error) -> bool {
    is_not_found_error(error) || is_scope_access_error(error)
}

async fn folder_workspace_is_usable(
    client: &ContextStreamClient,
    workspace_id: Uuid,
) -> Result<bool> {
    match client.get_workspace(workspace_id).await {
        Ok(_) => Ok(true),
        Err(err) if is_stale_folder_workspace_error(&err) => Ok(false),
        Err(err) => Err(err),
    }
}

async fn clear_implicit_stale_folder_scope(
    client: &ContextStreamClient,
    session: &SessionManager,
    folder_path: &Option<String>,
) {
    session.replace_scope(None, None, folder_path.clone()).await;
    client.clear_defaults(true, true).await;
}

pub fn is_project_scope_error(error: &Error) -> bool {
    match error {
        Error::Http {
            status, message, ..
        } => {
            let lower = message.to_ascii_lowercase();
            let mentions_project = lower.contains("project") || lower.contains("project_id");
            (*status == 404 && mentions_project)
                || (*status == 400
                    && mentions_project
                    && (lower.contains("workspace")
                        || lower.contains("not found")
                        || lower.contains("does not belong")
                        || lower.contains("scope")
                        || lower.contains("invalid")))
                || (*status == 500
                    && (lower.contains("docs_project_id_fkey")
                        || lower.contains("project_id_fkey")
                        || lower.contains("violates foreign key constraint")))
        }
        _ => false,
    }
}

pub fn is_scope_access_error(error: &Error) -> bool {
    matches!(
        error,
        Error::Http {
            code: ErrorCode::Forbidden | ErrorCode::Unauthorized,
            ..
        }
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProjectCandidateSource {
    LocalIndex,
    FolderMapping,
    Fallback,
}

fn push_unique_write_candidate(
    candidates: &mut Vec<(Uuid, ProjectCandidateSource)>,
    candidate: Option<Uuid>,
    source: ProjectCandidateSource,
) {
    if let Some(candidate) = candidate {
        if !candidates.iter().any(|(id, _)| *id == candidate) {
            candidates.push((candidate, source));
        }
    }
}

async fn validate_project_candidate(
    client: &ContextStreamClient,
    candidate: Uuid,
    workspace_id: &mut Option<Uuid>,
) -> Result<Option<Uuid>> {
    match client.get_project(candidate).await {
        Ok(project) => {
            if let Some(project_ws) = project.workspace_id {
                match *workspace_id {
                    Some(active_ws) if active_ws != project_ws => return Ok(None),
                    None => *workspace_id = Some(project_ws),
                    _ => {}
                }
            }
            Ok(Some(candidate))
        }
        Err(err) if is_not_found_error(&err) => Ok(None),
        Err(err) if is_scope_access_error(&err) => Ok(None),
        Err(err) => Err(err),
    }
}

fn scope_field_was_provided(raw: Option<&str>) -> bool {
    raw.map(str::trim)
        .map(|value| !value.is_empty())
        .unwrap_or(false)
}

async fn resolve_write_scope_inner(
    client: &ContextStreamClient,
    session: &SessionManager,
    raw_workspace_id: Option<&str>,
    raw_project_id: Option<&str>,
    ignore_explicit_project_id: bool,
    ignore_stale_fallback_workspace: bool,
) -> Result<ResolvedWriteScope> {
    let state = session.state().await;
    let config = client.config().await;
    let task_auth = get_task_auth_override();
    let task_workspace_id = task_auth.as_ref().and_then(|auth| auth.workspace_id);
    let task_project_id = task_auth.as_ref().and_then(|auth| auth.project_id);
    let raw_workspace_provided = scope_field_was_provided(raw_workspace_id);

    let fallback_workspace_id = if ignore_stale_fallback_workspace {
        None
    } else {
        state
            .workspace_id
            .or(task_workspace_id)
            .or(config.default_workspace_id)
    };
    let mut workspace_id =
        parse_scope_uuid(raw_workspace_id, fallback_workspace_id, "workspace_id")?;
    let requested_project_id =
        parse_project_scope_id(client, workspace_id, raw_project_id, None).await?;
    let mut explicit_project_id = if ignore_explicit_project_id {
        None
    } else {
        requested_project_id
    };
    let fallback_project_id = state
        .project_id
        .or(task_project_id)
        .or(config.default_project_id);
    let folder_path = state.folder_path.clone();
    drop(config);

    let mut resolved_folder_project_id = None;
    let mut workspace_backed_by_folder = false;
    let mut note = None;
    if let Some(ref path) = folder_path {
        if let Some(mapping) = resolve_workspace(path).await {
            let can_use_folder_workspace = !raw_workspace_provided && task_workspace_id.is_none();
            if can_use_folder_workspace {
                if folder_workspace_is_usable(client, mapping.workspace_id).await? {
                    if workspace_id.is_none() || workspace_id != Some(mapping.workspace_id) {
                        if let Some(prev_ws) = workspace_id {
                            note = Some(format!(
                                "Ignored stale workspace_id {} because the current folder resolves to workspace_id {}.",
                                prev_ws, mapping.workspace_id
                            ));
                        }
                        workspace_id = Some(mapping.workspace_id);
                    }
                    if workspace_id == Some(mapping.workspace_id) {
                        resolved_folder_project_id = mapping.project_id;
                        workspace_backed_by_folder = true;
                    }
                } else {
                    if workspace_id == Some(mapping.workspace_id) {
                        workspace_id = None;
                        clear_implicit_stale_folder_scope(client, session, &folder_path).await;
                    }
                    note = Some(format!(
                        "Ignored stale folder workspace_id {} because it is not accessible. Run init(folder_path=\"...\") or pass workspace_id explicitly after checking workspace access.",
                        mapping.workspace_id
                    ));
                }
            } else if workspace_id == Some(mapping.workspace_id) {
                resolved_folder_project_id = mapping.project_id;
                workspace_backed_by_folder = true;
            }
        }
    }
    let local_index_project_id = folder_path
        .as_deref()
        .and_then(ContextStreamClient::tracked_project_id_for_folder);

    // A "soft" active workspace is one we inferred (session state, task-auth
    // fallback, or config default) rather than one the caller pinned this call
    // or that a live folder mapping backs. When a soft workspace disagrees with
    // an authoritative project it must yield to the project, instead of
    // discarding the project and emitting an inconsistent {workspace,
    // project: None} scope that the backend rejects for project-scoped writes.
    let workspace_is_adoptable =
        !raw_workspace_provided && task_workspace_id.is_none() && !workspace_backed_by_folder;

    let mut stale_project_id = if ignore_explicit_project_id {
        requested_project_id
    } else {
        None
    };
    if let Some(explicit_id) = explicit_project_id {
        match client.get_project(explicit_id).await {
            Ok(project) => {
                if let Some(project_ws) = project.workspace_id {
                    match workspace_id {
                        Some(active_ws) if active_ws != project_ws => {
                            if workspace_is_adoptable {
                                // The caller explicitly asked for this project, so a
                                // soft (drifted) active workspace must yield to the
                                // project's real workspace rather than drop it.
                                note = Some(format!(
                                    "Adopted workspace_id {} from explicitly requested project_id {} because the active workspace was soft/drifted.",
                                    project_ws, explicit_id
                                ));
                                workspace_id = Some(project_ws);
                            } else {
                                stale_project_id = Some(explicit_id);
                                note = Some(format!(
                                    "Ignored stale project_id {} because it belongs to a different workspace; reconnecting to the current folder project.",
                                    explicit_id
                                ));
                                explicit_project_id = None;
                            }
                        }
                        None => workspace_id = Some(project_ws),
                        _ => {}
                    }
                }
            }
            Err(err) if is_not_found_error(&err) || is_project_scope_error(&err) => {
                stale_project_id = Some(explicit_id);
                note = Some(format!(
                    "Ignored stale project_id {} because it is no longer valid; reconnecting to the current folder project.",
                    explicit_id
                ));
                explicit_project_id = None;
            }
            Err(err) => return Err(err),
        }
    }

    if let Some(project_id) = explicit_project_id {
        return Ok(ResolvedWriteScope {
            workspace_id,
            project_id: Some(project_id),
            requested_project_id,
            stale_project_id,
            scope_recovered: false,
            note,
        });
    }

    let mut candidates = Vec::new();
    push_unique_write_candidate(
        &mut candidates,
        local_index_project_id,
        ProjectCandidateSource::LocalIndex,
    );
    push_unique_write_candidate(
        &mut candidates,
        resolved_folder_project_id,
        ProjectCandidateSource::FolderMapping,
    );
    push_unique_write_candidate(
        &mut candidates,
        fallback_project_id,
        ProjectCandidateSource::Fallback,
    );

    // Pass 1 (strict): prefer a candidate that already belongs to the active
    // workspace (or that sets the workspace when none is resolved yet). This
    // keeps writes in the current workspace whenever any reachable project
    // matches it, avoiding a premature workspace switch from a stale candidate.
    let mut selected: Option<(Uuid, ProjectCandidateSource)> = None;
    for &(candidate, source) in &candidates {
        if let Some(valid) =
            validate_project_candidate(client, candidate, &mut workspace_id).await?
        {
            selected = Some((valid, source));
            break;
        }
    }

    // Pass 2 (soft-workspace recovery): every candidate was rejected only
    // because it lives in a different workspace than the active one. If that
    // workspace is soft (inferred, not caller/folder/task-auth pinned), adopt
    // the first reachable project together with its real workspace. Without
    // this the resolver emits {soft-workspace, project: None} — which the
    // backend accepts for some artifacts (e.g. diagrams) but rejects for others
    // (e.g. docs), the exact asymmetry behind repeated create_doc rejections
    // after the session workspace drifted away from its project.
    let mut adopted_workspace = false;
    if selected.is_none() && workspace_is_adoptable {
        for &(candidate, source) in &candidates {
            match client.get_project(candidate).await {
                Ok(project) => {
                    if let Some(project_ws) = project.workspace_id {
                        if workspace_id != Some(project_ws) {
                            note = Some(match note {
                                Some(existing) => format!(
                                    "{} Adopted workspace_id {} from project {} because the active workspace had no reachable project.",
                                    existing, project_ws, candidate
                                ),
                                None => format!(
                                    "Adopted workspace_id {} from project {} because the active workspace had no reachable project.",
                                    project_ws, candidate
                                ),
                            });
                            workspace_id = Some(project_ws);
                            adopted_workspace = true;
                        }
                    }
                    selected = Some((candidate, source));
                    break;
                }
                Err(err) if is_not_found_error(&err) || is_scope_access_error(&err) => continue,
                Err(err) => return Err(err),
            }
        }
    }

    let Some((project_id, source)) = selected else {
        if let Some(stale_id) = stale_project_id {
            return Err(Error::Validation(format!(
                "Project scope recovery failed: project_id {} is invalid and no current folder/session project could be validated. Re-run init(folder_path=\"...\") to reconnect this workspace.",
                stale_id
            )));
        }

        return Ok(ResolvedWriteScope {
            workspace_id,
            project_id: None,
            requested_project_id,
            stale_project_id,
            scope_recovered: false,
            note,
        });
    };

    let recovered_from_stale = stale_project_id.is_some();
    let recovered_from_scope_drift =
        !recovered_from_stale && state.project_id.is_some() && state.project_id != Some(project_id);
    // Adopting a project's workspace over a soft/drifted one must also be
    // persisted; otherwise the corrected scope is recomputed (and the backend
    // re-rejects the write) on every subsequent turn instead of self-healing.
    let scope_recovered = recovered_from_stale || recovered_from_scope_drift || adopted_workspace;

    if scope_recovered {
        let source_label = match source {
            ProjectCandidateSource::LocalIndex => "local index metadata",
            ProjectCandidateSource::FolderMapping => "current folder mapping",
            ProjectCandidateSource::Fallback => "session/default scope",
        };
        let recovery_note = match note {
            Some(existing) => format!(
                "{} Reconnected to project_id {} from {}.",
                existing, project_id, source_label
            ),
            None => format!(
                "Reconnected to project_id {} from {} because the prior project scope was stale.",
                project_id, source_label
            ),
        };
        note = Some(recovery_note);
        session
            .update_scope(workspace_id, Some(project_id), None)
            .await;
    }

    Ok(ResolvedWriteScope {
        workspace_id,
        project_id: Some(project_id),
        requested_project_id,
        stale_project_id,
        scope_recovered,
        note,
    })
}

pub async fn resolve_write_scope(
    client: &ContextStreamClient,
    session: &SessionManager,
    raw_workspace_id: Option<&str>,
    raw_project_id: Option<&str>,
) -> Result<ResolvedWriteScope> {
    resolve_write_scope_inner(
        client,
        session,
        raw_workspace_id,
        raw_project_id,
        false,
        false,
    )
    .await
}

pub async fn recover_write_scope_after_project_error(
    client: &ContextStreamClient,
    session: &SessionManager,
    raw_workspace_id: Option<&str>,
    raw_project_id: Option<&str>,
    err: Error,
) -> Result<ResolvedWriteScope> {
    if !is_project_scope_error(&err) {
        if is_scope_access_error(&err)
            && !scope_field_was_provided(raw_workspace_id)
            && !scope_field_was_provided(raw_project_id)
        {
            let mut scope = resolve_write_scope_inner(
                client,
                session,
                raw_workspace_id,
                raw_project_id,
                true,
                true,
            )
            .await?;
            scope.scope_recovered = true;
            if scope.note.is_none() {
                scope.note = Some(format!(
                    "Recovered from stale workspace scope after {}; retried without the forbidden fallback workspace.",
                    err
                ));
            }
            return Ok(scope);
        }
        return Err(err);
    }

    let mut scope = resolve_write_scope_inner(
        client,
        session,
        raw_workspace_id,
        raw_project_id,
        true,
        false,
    )
    .await?;
    if scope.project_id.is_none() {
        return Err(Error::Validation(format!(
            "Project scope recovery failed after {}. Re-run init(folder_path=\"...\") to reconnect this workspace.",
            err
        )));
    }
    scope.scope_recovered = true;
    if scope.note.is_none() {
        scope.note = Some(format!("Recovered from stale project scope after {}.", err));
    }
    Ok(scope)
}

pub fn attach_scope_recovery_metadata(value: &mut Value, scope: &ResolvedWriteScope) {
    if !scope.scope_recovered && scope.note.is_none() && scope.stale_project_id.is_none() {
        return;
    }

    if let Some(obj) = value.as_object_mut() {
        obj.insert(
            "scope_recovered".to_string(),
            serde_json::json!(scope.scope_recovered),
        );
        obj.insert(
            "stale_project_id".to_string(),
            scope
                .stale_project_id
                .map(|id| serde_json::json!(id.to_string()))
                .unwrap_or(Value::Null),
        );
        obj.insert(
            "requested_project_id".to_string(),
            scope
                .requested_project_id
                .map(|id| serde_json::json!(id.to_string()))
                .unwrap_or(Value::Null),
        );
        obj.insert(
            "resolved_workspace_id".to_string(),
            scope
                .workspace_id
                .map(|id| serde_json::json!(id.to_string()))
                .unwrap_or(Value::Null),
        );
        obj.insert(
            "resolved_project_id".to_string(),
            scope
                .project_id
                .map(|id| serde_json::json!(id.to_string()))
                .unwrap_or(Value::Null),
        );
        obj.insert(
            "scope_recovery_note".to_string(),
            scope
                .note
                .as_ref()
                .map(|note| serde_json::json!(note))
                .unwrap_or(Value::Null),
        );
    }
}

pub async fn resolve_read_scope(
    client: &ContextStreamClient,
    session: &SessionManager,
    raw_workspace_id: Option<&str>,
    raw_project_id: Option<&str>,
) -> Result<ResolvedReadScope> {
    let state = session.state().await;
    let config = client.config().await;
    let task_auth = get_task_auth_override();
    let task_workspace_id = task_auth.as_ref().and_then(|auth| auth.workspace_id);
    let task_project_id = task_auth.as_ref().and_then(|auth| auth.project_id);
    let raw_workspace_provided = scope_field_was_provided(raw_workspace_id);

    let fallback_workspace_id = state
        .workspace_id
        .or(task_workspace_id)
        .or(config.default_workspace_id);
    let mut workspace_id =
        parse_scope_uuid(raw_workspace_id, fallback_workspace_id, "workspace_id")?;
    let requested_explicit_project_id =
        parse_project_scope_id(client, workspace_id, raw_project_id, None).await?;
    let mut explicit_project_id = requested_explicit_project_id;
    let fallback_project_id = state
        .project_id
        .or(task_project_id)
        .or(config.default_project_id);
    let folder_path = state.folder_path.clone();
    drop(config);

    let mut resolved_folder_project_id = None;
    let mut note = None;
    if let Some(ref path) = folder_path {
        if let Some(mapping) = resolve_workspace(path).await {
            let can_use_folder_workspace = !raw_workspace_provided && task_workspace_id.is_none();
            if can_use_folder_workspace {
                if folder_workspace_is_usable(client, mapping.workspace_id).await? {
                    if workspace_id.is_none() || workspace_id != Some(mapping.workspace_id) {
                        if let Some(prev_ws) = workspace_id {
                            note = Some(format!(
                                "workspace_id {} was stale for the current folder and was ignored; using workspace_id {} from folder mapping.",
                                prev_ws, mapping.workspace_id
                            ));
                        }
                        workspace_id = Some(mapping.workspace_id);
                    }
                    if workspace_id == Some(mapping.workspace_id) {
                        resolved_folder_project_id = mapping.project_id;
                    }
                } else {
                    if workspace_id == Some(mapping.workspace_id) {
                        workspace_id = None;
                        clear_implicit_stale_folder_scope(client, session, &folder_path).await;
                    }
                    note = Some(format!(
                        "Ignored stale folder workspace_id {} because it is not accessible. Run init(folder_path=\"...\") or pass workspace_id explicitly after checking workspace access.",
                        mapping.workspace_id
                    ));
                }
            } else if workspace_id == Some(mapping.workspace_id) {
                resolved_folder_project_id = mapping.project_id;
            }
        }
    }
    let local_index_project_id = folder_path
        .as_deref()
        .and_then(ContextStreamClient::tracked_project_id_for_folder);

    if let Some(explicit_id) = explicit_project_id {
        match client.get_project(explicit_id).await {
            Ok(project) => {
                if let Some(project_ws) = project.workspace_id {
                    match workspace_id {
                        Some(active_ws) if active_ws != project_ws => {
                            note = Some(format!(
                                "project_id {} belongs to a different workspace and was ignored. Do NOT pass this project_id again — omit it and let the session resolve the correct project scope automatically.",
                                explicit_id
                            ));
                            explicit_project_id = None;
                        }
                        None => workspace_id = Some(project_ws),
                        _ => {}
                    }
                }
            }
            Err(err) if is_not_found_error(&err) || is_project_scope_error(&err) => {
                note = Some(format!(
                    "project_id {} no longer exists and was ignored. Do NOT pass this project_id again — omit it and let the session resolve the correct project scope automatically.",
                    explicit_id
                ));
                explicit_project_id = None;
            }
            Err(err) => return Err(err),
        }
    }

    let project_id = if let Some(explicit_id) = explicit_project_id {
        Some(explicit_id)
    } else {
        let mut candidates = Vec::new();
        push_unique_project_candidate(&mut candidates, local_index_project_id);
        push_unique_project_candidate(&mut candidates, resolved_folder_project_id);
        push_unique_project_candidate(&mut candidates, fallback_project_id);

        let mut selected = None;
        for candidate in candidates {
            if let Some(valid) =
                validate_project_candidate(client, candidate, &mut workspace_id).await?
            {
                selected = Some(valid);
                break;
            }
        }
        selected
    };

    let mut related_project_ids = Vec::new();
    if explicit_project_id.is_none() {
        let relations = session.get_project_relations().await;
        for related in relations.values() {
            let Ok(related_id) = Uuid::parse_str(&related.project_id) else {
                continue;
            };
            if Some(related_id) == project_id {
                continue;
            }
            if !related_project_ids.contains(&related_id) {
                related_project_ids.push(related_id);
            }
        }
    }

    Ok(ResolvedReadScope {
        workspace_id,
        project_id,
        related_project_ids,
        note,
    })
}

// ============================================================================
// Scope reliability diagnostics (requirement #6)
// ============================================================================

/// Diagnostics extracted from a search response's scope reliability metadata.
#[derive(Debug, Clone, Default)]
pub struct ScopeDiagnostics {
    pub scope_valid: bool,
    pub scope_reason: Option<String>,
    pub fallback_used: bool,
    pub fallback_reason: Option<String>,
    pub project_index_state: Option<String>,
    pub remediation_attempted: bool,
    pub remediation_note: Option<String>,
}

impl ScopeDiagnostics {
    pub fn has_issues(&self) -> bool {
        !self.scope_valid || self.fallback_used || self.remediation_attempted
    }

    /// Only true when the diagnostic represents something the caller should
    /// act on. Routine `fallback_used=true` with a ready index is not
    /// actionable — the fallback succeeded and results are valid. Real
    /// signals are invalid scope, a non-ready index, or a remediation with
    /// a user-facing note.
    pub fn is_actionable(&self) -> bool {
        if !self.scope_valid {
            return true;
        }
        if let Some(state) = self.project_index_state.as_deref() {
            if project_index_state_requires_action(state) {
                return true;
            }
        }
        if self.remediation_attempted && self.remediation_note.is_some() {
            return true;
        }
        false
    }

    /// Short, user-facing line describing the actionable issue. Returns
    /// None when `is_actionable()` is false.
    pub fn to_actionable_text(&self) -> Option<String> {
        if !self.is_actionable() {
            return None;
        }

        if !self.scope_valid {
            let reason = self
                .scope_reason
                .as_deref()
                .unwrap_or("scope resolution failed");
            return Some(format!(
                "scope resolution issue — {}. Re-run `init(folder_path=\"...\")` to refresh.",
                reason
            ));
        }

        if let Some(state) = self.project_index_state.as_deref() {
            if project_index_state_requires_action(state) {
                return Some(format!(
                    "project index state=`{}` — indexed coverage is still building for this scope. Use ContextStream search first; fall back to local tools only if it returns nothing. Re-establish the intended checkout with `init(folder_path=\"...\")`, then run `project(action=\"index\")`; keep hosted MCP configured.",
                    state
                ));
            }
        }

        if self.remediation_attempted {
            if let Some(note) = self.remediation_note.as_deref() {
                return Some(format!("remediation applied: {}", note));
            }
        }

        None
    }

    /// Full diagnostic dump including non-actionable fields like
    /// `fallback_used=true`. Used only under `CONTEXTSTREAM_DEBUG`.
    pub fn to_diagnostic_text(&self) -> Option<String> {
        if !self.has_issues() {
            return None;
        }

        let mut parts = Vec::new();
        if !self.scope_valid {
            parts.push(format!(
                "scope_valid=false (reason: `{}`)",
                self.scope_reason.as_deref().unwrap_or("unknown")
            ));
        }
        if self.fallback_used {
            parts.push(format!(
                "fallback_used=true (reason: `{}`)",
                self.fallback_reason.as_deref().unwrap_or("unknown")
            ));
        }
        if let Some(state) = self.project_index_state.as_deref() {
            parts.push(format!("project_index_state=`{}`", state));
        }
        if self.remediation_attempted {
            if let Some(ref note) = self.remediation_note {
                parts.push(format!("remediation: {}", note));
            }
        }

        let diag = parts.join("; ");
        Some(format!(
            "{} — These are internal quality signals. \
             The results above are REAL code from the project and should be used directly. \
             Do NOT fall back to local Grep/Glob/Find.",
            diag
        ))
    }
}

fn project_index_state_requires_action(state: &str) -> bool {
    let normalized = state.trim().to_ascii_lowercase();
    !matches!(
        normalized.as_str(),
        "ready" | "fresh" | "recent" | "aging" | "stale" | "partial" | "indexing" | "committing"
    )
}

/// Extract scope diagnostics from a search response.
pub fn extract_scope_diagnostics(response: &SearchResponse) -> ScopeDiagnostics {
    ScopeDiagnostics {
        scope_valid: response.scope_valid.unwrap_or(true),
        scope_reason: response.scope_reason.clone(),
        fallback_used: response.fallback_used.unwrap_or(false),
        fallback_reason: response.fallback_reason.clone(),
        project_index_state: response.project_index_state.clone(),
        remediation_attempted: false,
        remediation_note: None,
    }
}

// ============================================================================
// Absolute path resolution (requirement #4)
// ============================================================================

/// Known mirror prefixes that should be stripped to get canonical repo-relative paths.
const MIRROR_PREFIXES: &[&str] = &[
    "contextstream-ai-brain-export/",
    "contextstream/",
    "web/users/",
];

fn is_likely_repo_root_segment(segment: &str) -> bool {
    matches!(
        segment,
        "apps"
            | "config"
            | "crates"
            | "deploy"
            | "deploy-binaries"
            | "docs"
            | "essentials"
            | "examples"
            | "migrations"
            | "scripts"
            | "sdk"
            | "src"
            | "storage"
            | "tests"
            | "web"
    )
}

pub(crate) fn repo_relative_suffix(path: &str) -> Option<PathBuf> {
    let normalized = canonicalize_repo_path(path);
    let trimmed = normalized.trim_start_matches('/');
    let components: Vec<&str> = trimmed
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect();
    let start = components
        .iter()
        .position(|segment| is_likely_repo_root_segment(segment))?;

    let mut relative = PathBuf::new();
    for component in components.iter().skip(start) {
        relative.push(component);
    }
    (!relative.as_os_str().is_empty()).then_some(relative)
}

fn usable_folder_root(folder_path: &str) -> Option<&str> {
    let trimmed = folder_path.trim();
    if trimmed.is_empty() {
        return None;
    }
    let path = Path::new(trimmed);
    path.parent()?;
    Some(trimmed)
}

fn is_pseudo_absolute_repo_relative(path: &str) -> bool {
    let raw = Path::new(path);
    if !raw.is_absolute() {
        return false;
    }

    let mut components = raw.components();
    let _root = components.next();
    matches!(
        components.next(),
        Some(std::path::Component::Normal(segment))
            if is_likely_repo_root_segment(&segment.to_string_lossy())
    )
}

fn is_windows_absolute_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/')
}

/// Canonicalize a repo-relative path by stripping known mirror prefixes.
pub fn canonicalize_repo_path(path: &str) -> String {
    let mut normalized = path.replace('\\', "/");
    while normalized.starts_with("./") {
        normalized = normalized[2..].to_string();
    }

    for prefix in MIRROR_PREFIXES {
        if normalized.starts_with(prefix) {
            normalized = normalized[prefix.len()..].to_string();
            break;
        }
    }

    // .claude/worktrees/<worktree-name>/... -> strip prefix AND the worktree-name segment
    if normalized.starts_with(".claude/worktrees/") {
        let after_prefix = &normalized[".claude/worktrees/".len()..];
        if let Some(slash_idx) = after_prefix.find('/') {
            let remainder = &after_prefix[slash_idx + 1..];
            if !remainder.is_empty() {
                normalized = remainder.to_string();
            }
        } else {
            // No slash after worktree name — path is just the worktree dir itself, nothing to resolve
            normalized = after_prefix.to_string();
        }
    }

    // Handle web/users/.../workspaces/.../projects/... pattern
    if let Some(idx) = normalized.find("/projects/") {
        if let Some(after_projects) = normalized[idx..].strip_prefix("/projects/") {
            if let Some(slash_idx) = after_projects.find('/') {
                let remainder = &after_projects[slash_idx + 1..];
                if !remainder.is_empty() {
                    normalized = remainder.to_string();
                }
            }
        }
    }

    normalized
}

/// Resolve a repo-relative path to an absolute local path.
/// Always produces the joined path without checking `exists()` because
/// in hosted (HTTP transport) mode the folder_path refers to the client
/// machine, not the server filesystem.
pub fn resolve_to_absolute_path(relative_path: &str, folder_path: &str) -> Option<PathBuf> {
    let folder_path = usable_folder_root(folder_path)?;
    let root = Path::new(folder_path);
    let raw = Path::new(relative_path);

    if is_windows_absolute_path(relative_path) {
        if let Some(relative) = repo_relative_suffix(relative_path) {
            return Some(root.join(relative));
        }
        return None;
    }

    if raw.is_absolute() {
        if raw.starts_with(root) {
            return Some(raw.to_path_buf());
        }
        if let Some(relative) = repo_relative_suffix(relative_path) {
            return Some(root.join(relative));
        }
        return None;
    }

    let canonical = canonicalize_repo_path(relative_path);
    if canonical.is_empty() {
        return None;
    }

    let absolute = root.join(&canonical);
    Some(absolute)
}

/// Resolve all paths in a search response to absolute local paths.
/// Drops paths that cannot be resolved and returns diagnostics about dropped paths.
pub fn resolve_search_paths(response: &mut SearchResponse, folder_path: &str) -> Vec<String> {
    let mut dropped_paths = Vec::new();
    let usable_root = usable_folder_root(folder_path);

    // Filter results: resolve file_path to absolute, drop entries that can't resolve
    response.results.retain_mut(|result| {
        if let Some(ref file_path) = result.file_path {
            if usable_root.is_none()
                && Path::new(file_path).is_absolute()
                && !is_pseudo_absolute_repo_relative(file_path)
            {
                return true;
            }
            if usable_root.is_none() && is_pseudo_absolute_repo_relative(file_path) {
                dropped_paths.push(file_path.clone());
                return false;
            }
            if Path::new(file_path).is_absolute()
                && usable_root.is_some()
                && resolve_to_absolute_path(file_path, folder_path).is_none()
            {
                return true;
            }
            match resolve_to_absolute_path(file_path, folder_path) {
                Some(absolute) => {
                    let absolute_str = absolute.to_string_lossy().to_string();
                    result.file_path = Some(absolute_str.clone());
                    result.location = Some(match result.start_line {
                        Some(line) if line > 0 => format!("{}:{}", absolute_str, line),
                        _ => absolute_str,
                    });
                    true
                }
                None => {
                    dropped_paths.push(file_path.clone());
                    false
                }
            }
        } else {
            true
        }
    });

    let mut resolved_paths = Vec::new();
    for path in &response.paths {
        if usable_root.is_none()
            && Path::new(path).is_absolute()
            && !is_pseudo_absolute_repo_relative(path)
        {
            resolved_paths.push(path.clone());
            continue;
        }
        if usable_root.is_none() && is_pseudo_absolute_repo_relative(path) {
            dropped_paths.push(path.clone());
            continue;
        }
        if Path::new(path).is_absolute()
            && usable_root.is_some()
            && resolve_to_absolute_path(path, folder_path).is_none()
        {
            resolved_paths.push(path.clone());
            continue;
        }
        match resolve_to_absolute_path(path, folder_path) {
            Some(absolute) => {
                resolved_paths.push(absolute.to_string_lossy().to_string());
            }
            None => {
                dropped_paths.push(path.clone());
            }
        }
    }
    response.paths = resolved_paths;

    if !dropped_paths.is_empty() {
        debug!(
            "Dropped {} unresolvable path(s) from search results",
            dropped_paths.len()
        );
        response.total = Some(response.results.len() as i64);
    }

    dropped_paths
}

// ============================================================================
// Duplicate suppression (requirement #5)
// ============================================================================

/// Deduplicate search results by canonical path + line.
/// Keeps the first occurrence (which is the primary local repo variant from ranking).
pub fn deduplicate_results(response: &mut SearchResponse) -> usize {
    use std::collections::HashSet;

    let before = response.results.len();
    let mut seen = HashSet::new();

    response.results.retain(|item| {
        let canonical = item
            .file_path
            .as_deref()
            .map(canonicalize_repo_path)
            .unwrap_or_default();
        if canonical.is_empty() {
            return true;
        }
        let key = format!("{}:{}", canonical, item.start_line.unwrap_or(0));
        seen.insert(key)
    });

    let removed = before - response.results.len();
    if removed > 0 {
        response.total = Some(response.results.len() as i64);
        debug!("Deduplicated {} mirror-prefix duplicate(s)", removed);
    }
    removed
}

/// Deduplicate the paths list by canonical form.
pub fn deduplicate_paths(response: &mut SearchResponse) -> usize {
    use std::collections::HashSet;

    let before = response.paths.len();
    let mut seen = HashSet::new();
    response.paths.retain(|path| {
        let canonical = canonicalize_repo_path(path);
        seen.insert(canonical)
    });
    before - response.paths.len()
}

// ============================================================================
// Rollout logging (requirement #11)
// ============================================================================

/// Log an outgoing MCP search/media request for rollout diagnostics.
pub fn log_mcp_request(
    tool: &str,
    route: &str,
    workspace_id: Option<Uuid>,
    project_id: Option<Uuid>,
    query: &str,
) {
    debug!(
        "[MCP_REQUEST] tool={} route={} workspace_id={} project_id={} query=\"{}\"",
        tool,
        route,
        workspace_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "none".to_string()),
        project_id
            .map(|id| id.to_string())
            .unwrap_or_else(|| "none".to_string()),
        query,
    );
}

/// Log an incoming MCP response's scope metadata for rollout diagnostics.
pub fn log_mcp_response_scope(
    tool: &str,
    scope_valid: Option<bool>,
    fallback_reason: Option<&str>,
    result_count: usize,
) {
    debug!(
        "[MCP_RESPONSE] tool={} scope_valid={} fallback_reason={} result_count={}",
        tool,
        scope_valid
            .map(|v| v.to_string())
            .unwrap_or_else(|| "none".to_string()),
        fallback_reason.unwrap_or("none"),
        result_count,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_diag() -> ScopeDiagnostics {
        ScopeDiagnostics {
            scope_valid: true,
            scope_reason: None,
            fallback_used: false,
            fallback_reason: None,
            project_index_state: Some("ready".to_string()),
            remediation_attempted: false,
            remediation_note: None,
        }
    }

    #[test]
    fn routine_fallback_with_ready_index_is_not_actionable() {
        let mut d = base_diag();
        d.fallback_used = true;
        d.fallback_reason = Some("db_content_fallback".to_string());
        assert!(!d.is_actionable());
        assert!(d.to_actionable_text().is_none());
    }

    #[test]
    fn invalid_scope_is_actionable() {
        let mut d = base_diag();
        d.scope_valid = false;
        d.scope_reason = Some("workspace_mismatch".to_string());
        assert!(d.is_actionable());
        let text = d.to_actionable_text().unwrap();
        assert!(text.contains("workspace_mismatch"));
        assert!(text.contains("init("));
    }

    #[test]
    fn stale_index_is_not_actionable() {
        let mut d = base_diag();
        d.project_index_state = Some("stale".to_string());
        assert!(!d.is_actionable());
        assert!(d.to_actionable_text().is_none());
    }

    #[test]
    fn missing_index_is_actionable() {
        let mut d = base_diag();
        d.project_index_state = Some("not_indexed".to_string());
        assert!(d.is_actionable());
        let text = d.to_actionable_text().unwrap();
        assert!(text.contains("not_indexed"));
        assert!(text.contains("init(folder_path="));
        assert!(text.contains("project(action=\"index\""));
        assert!(text.contains("hosted MCP"));
        assert!(!text.contains("ingest_local"));
        assert!(!text.contains("narrow local inspection"));
    }

    #[test]
    fn ready_index_without_other_issues_is_not_actionable() {
        let d = base_diag();
        assert!(!d.is_actionable());
        assert!(d.to_actionable_text().is_none());
    }

    #[test]
    fn remediation_with_note_is_actionable() {
        let mut d = base_diag();
        d.remediation_attempted = true;
        d.remediation_note = Some("switched to default project".to_string());
        assert!(d.is_actionable());
    }

    #[test]
    fn remediation_without_note_is_not_actionable() {
        let mut d = base_diag();
        d.remediation_attempted = true;
        assert!(!d.is_actionable());
    }

    #[tokio::test]
    async fn read_scope_prefers_initialized_session_over_task_auth_workspace() {
        let config = mcp_types::config::Config::default();
        let client = ContextStreamClient::new(config.clone());
        let session = SessionManager::new(client.clone(), config);
        let session_workspace = Uuid::new_v4();
        let task_workspace = Uuid::new_v4();

        session
            .initialize(Some(session_workspace), None, None, None)
            .await;

        let scope = mcp_client::run_with_auth_override(
            mcp_types::AuthOverride {
                workspace_id: Some(task_workspace),
                project_id: None,
                ..Default::default()
            },
            || async { resolve_read_scope(&client, &session, None, None).await },
        )
        .await
        .unwrap();

        assert_eq!(scope.workspace_id, Some(session_workspace));
        assert_ne!(scope.workspace_id, Some(task_workspace));
    }

    #[tokio::test]
    async fn write_scope_prefers_initialized_session_over_task_auth_workspace() {
        let config = mcp_types::config::Config::default();
        let client = ContextStreamClient::new(config.clone());
        let session = SessionManager::new(client.clone(), config);
        let session_workspace = Uuid::new_v4();
        let task_workspace = Uuid::new_v4();

        session
            .initialize(Some(session_workspace), None, None, None)
            .await;

        let scope = mcp_client::run_with_auth_override(
            mcp_types::AuthOverride {
                workspace_id: Some(task_workspace),
                project_id: None,
                ..Default::default()
            },
            || async { resolve_write_scope(&client, &session, None, None).await },
        )
        .await
        .unwrap();

        assert_eq!(scope.workspace_id, Some(session_workspace));
        assert_ne!(scope.workspace_id, Some(task_workspace));
    }

    fn project_in_workspace(id: Uuid, workspace_id: Uuid) -> mcp_types::api::Project {
        mcp_types::api::Project {
            id,
            name: "mcp".to_string(),
            description: None,
            repository_url: None,
            repository_type: None,
            workspace_id: Some(workspace_id),
            path: None,
            created_at: None,
            updated_at: None,
            indexed_at: None,
            file_count: None,
        }
    }

    /// Regression: a drifted session where the active workspace was switched
    /// (e.g. to "Sales") without updating the project (still the "Engineering"
    /// project) must NOT resolve to {drifted-workspace, project: None}. The
    /// valid session project is authoritative for a project-scoped write, so it
    /// — and its real workspace — win, and the correction is persisted so the
    /// session self-heals. Before the fix this returned project_id: None, which
    /// the backend /docs endpoint rejected (while /diagrams silently accepted).
    #[tokio::test]
    async fn write_scope_adopts_project_workspace_when_session_workspace_is_soft_and_drifted() {
        let config = mcp_types::config::Config::default();
        let client = ContextStreamClient::new(config.clone());
        let session = SessionManager::new(client.clone(), config);

        let drifted_workspace = Uuid::new_v4(); // soft session workspace ("Sales")
        let real_workspace = Uuid::new_v4(); // where the project actually lives ("Engineering")
        let project_id = Uuid::new_v4();

        // Internally inconsistent pair: workspace points at "Sales" while the
        // project still belongs to "Engineering".
        let (scope, state) =
            mcp_client::run_with_session_key(mcp_types::SessionKey::Local, || async {
                session
                    .initialize(Some(drifted_workspace), Some(project_id), None, None)
                    .await;
                // Model the explicit stdio transport marker: caller-sensitive
                // client cache priming must fail closed outside this scope.
                client.prime_cached_project(project_in_workspace(project_id, real_workspace));

                let scope = resolve_write_scope(&client, &session, None, None)
                    .await
                    .unwrap();
                let state = session.state().await;
                (scope, state)
            })
            .await;

        assert_eq!(scope.project_id, Some(project_id));
        assert_eq!(scope.workspace_id, Some(real_workspace));
        assert!(scope.scope_recovered);

        // The corrected scope must be persisted back to the session.
        assert_eq!(state.workspace_id, Some(real_workspace));
        assert_eq!(state.project_id, Some(project_id));
    }

    /// An EXPLICITLY provided workspace_id stays authoritative: a session
    /// project in a different workspace must NOT drag the write into the
    /// project's workspace, and must be reported as stale instead of adopted.
    #[tokio::test]
    async fn write_scope_keeps_explicit_workspace_and_does_not_adopt_project_workspace() {
        let config = mcp_types::config::Config::default();
        let client = ContextStreamClient::new(config.clone());
        let session = SessionManager::new(client.clone(), config);

        let explicit_workspace = Uuid::new_v4();
        let other_workspace = Uuid::new_v4();
        let project_id = Uuid::new_v4();

        let scope = mcp_client::run_with_session_key(mcp_types::SessionKey::Local, || async {
            session
                .initialize(Some(other_workspace), Some(project_id), None, None)
                .await;
            client.prime_cached_project(project_in_workspace(project_id, other_workspace));

            // Caller pins the workspace explicitly; the project lives elsewhere.
            resolve_write_scope(
                &client,
                &session,
                Some(&explicit_workspace.to_string()),
                None,
            )
            .await
            .unwrap()
        })
        .await;

        assert_eq!(scope.workspace_id, Some(explicit_workspace));
        assert_eq!(scope.project_id, None);
    }

    #[test]
    fn stale_folder_workspace_error_detects_not_found_and_access_errors() {
        assert!(is_stale_folder_workspace_error(&Error::http(
            404,
            "Not found: Workspace missing"
        )));
        assert!(is_stale_folder_workspace_error(&Error::http(
            403,
            "Forbidden: Workspace inaccessible"
        )));
        assert!(is_stale_folder_workspace_error(&Error::http(
            401,
            "Unauthorized"
        )));
        assert!(!is_stale_folder_workspace_error(&Error::http(
            500,
            "temporary upstream failure"
        )));
    }

    #[test]
    fn project_scope_error_detects_project_mismatch_not_generic_validation() {
        assert!(is_project_scope_error(&Error::http(
            404,
            "Not found: Project not found"
        )));
        assert!(is_project_scope_error(&Error::http(
            400,
            "Project does not belong to workspace"
        )));
        assert!(is_project_scope_error(&Error::http(
            500,
            "insert or update violates foreign key constraint docs_project_id_fkey"
        )));
        assert!(!is_project_scope_error(&Error::http(
            400,
            "Validation error: invalid regex pattern"
        )));
    }

    #[test]
    fn scope_access_error_detects_forbidden_or_unauthorized_scope() {
        assert!(is_scope_access_error(&Error::http(
            403,
            "Forbidden: No access to this workspace"
        )));
        assert!(is_scope_access_error(&Error::http(401, "Unauthorized")));
        assert!(!is_scope_access_error(&Error::http(
            400,
            "Validation error: invalid regex pattern"
        )));
    }

    #[test]
    fn attach_scope_recovery_metadata_adds_structured_fields() {
        let stale = Uuid::new_v4();
        let resolved = Uuid::new_v4();
        let mut value = serde_json::json!({"id": "plan"});
        let scope = ResolvedWriteScope {
            workspace_id: Some(Uuid::nil()),
            project_id: Some(resolved),
            requested_project_id: Some(stale),
            stale_project_id: Some(stale),
            scope_recovered: true,
            note: Some("Recovered project scope".to_string()),
        };

        attach_scope_recovery_metadata(&mut value, &scope);

        assert_eq!(value["scope_recovered"], serde_json::json!(true));
        assert_eq!(
            value["stale_project_id"],
            serde_json::json!(stale.to_string())
        );
        assert_eq!(
            value["resolved_project_id"],
            serde_json::json!(resolved.to_string())
        );
        assert_eq!(
            value["scope_recovery_note"],
            serde_json::json!("Recovered project scope")
        );
    }
}
