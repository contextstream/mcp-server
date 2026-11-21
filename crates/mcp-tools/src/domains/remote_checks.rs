//! Remote check settings shared by the session tool, CLI and setup.

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use mcp_client::remote_checks::{
    indexed_filename, project_filenames, ProjectLanguages, RemoteChecks,
};
use mcp_client::ContextStreamClient;
use mcp_session::SessionManager;
use mcp_types::{tool::ToolResult, Error, Result};
use serde_json::{json, Value};
use uuid::Uuid;

/// The new API applies both choices atomically. Older servers only return
/// `enabled`; use their dedicated engine route before reporting what changed.
pub async fn settings(
    client: &ContextStreamClient,
    workspace: Option<&str>,
    enabled: Option<bool>,
    more: Option<bool>,
) -> Result<RemoteChecks> {
    let result = match (enabled, more) {
        (Some(enabled), more) => {
            let mut state = client.set_remote_checks(enabled, more).await?;
            if state.more_engines.is_some() {
                Ok(state)
            } else if let Some(more) = more {
                match client.set_more_engines(more).await {
                    Ok(actual) => {
                        state.more_engines = Some(actual);
                        Ok(state)
                    }
                    Err(_) => Err(Error::Tool(format!(
                        "Remote checks: {}. Could not confirm the more engines setting.",
                        state.state()
                    ))),
                }
            } else {
                state.more_engines = client.more_engines().await.ok();
                Ok(state)
            }
        }
        (None, Some(more)) => {
            let actual = client.set_more_engines(more).await?;
            client.remote_checks().await.map(|mut state| {
                state.more_engines.get_or_insert(actual);
                state
            })
        }
        (None, None) => return client.remote_checks_status().await,
    };
    super::hosted_tools::refresh_settings(client, workspace).await;
    result
}

pub fn local_languages(folder: Option<&Path>, state: &RemoteChecks) -> Option<ProjectLanguages> {
    let names = project_filenames(folder?).ok()?;
    Some(ProjectLanguages::detect(&names, &state.languages))
}

/// The gateway cannot inspect its caller's filesystem. Use indexed filenames
/// for the selected project, with a deadline so an absent index or a slow API
/// never prevents the caller from reading or changing the account setting.
async fn indexed_languages(
    client: &ContextStreamClient,
    id: Uuid,
    folder: Option<&str>,
    state: &RemoteChecks,
) -> Option<ProjectLanguages> {
    let mut names = HashSet::new();
    let mut page = 1;
    let mut page_size = 100;
    let mut total = None;
    loop {
        let data = client
            .project_files_for_checkout(
                id,
                Some(page),
                Some(page_size),
                None,
                Some("path"),
                Some("asc"),
                folder,
            )
            .await
            .ok()?;
        let rows = data
            .get("files")
            .or_else(|| data.get("paths"))?
            .as_array()?;
        if page == 1 {
            total = data.get("total").and_then(Value::as_i64);
            if rows.is_empty() {
                return None;
            }
            page_size = rows.len() as i64; // Respect a lower server-side page cap.
        }
        names.extend(rows.iter().filter_map(|file| {
            let path = file
                .as_str()
                .or_else(|| file.get("path").or_else(|| file.get("file_path"))?.as_str())?;
            indexed_filename(path).map(str::to_string)
        }));
        if rows.len() < page_size as usize
            || total.is_some_and(|count| page * page_size >= count)
            || data.get("has_more").and_then(Value::as_bool) == Some(false)
        {
            break;
        }
        page += 1;
    }
    Some(ProjectLanguages::detect(&names, &state.languages))
}

pub fn report(state: &RemoteChecks, project: Option<&ProjectLanguages>) -> Value {
    let mut data = json!(state);
    data["project_languages"] = json!(project);
    data
}

pub(super) async fn session_action(
    client: &ContextStreamClient,
    session: &SessionManager,
    project: Option<Uuid>,
    enabled: Option<bool>,
    more: Option<bool>,
) -> Result<ToolResult> {
    let workspace = super::slipstream::listing_workspace(client, session)
        .await
        .map(|id| id.to_string());
    let state = match settings(client, workspace.as_deref(), enabled, more).await {
        Err(Error::Http { status: 403, .. }) => {
            return Ok(ToolResult::error(
                "Remote checks are unavailable for this account.",
            ))
        }
        other => other?,
    };
    let session = session.state().await;
    let config = client.config().await;
    let project = if config.is_http_transport {
        let id = project
            .or(session.project_id)
            .or_else(|| mcp_client::get_task_auth_override()?.project_id);
        match id {
            Some(id) => tokio::time::timeout(
                Duration::from_secs(3),
                indexed_languages(client, id, session.folder_path.as_deref(), &state),
            )
            .await
            .ok()
            .flatten(),
            None => None,
        }
    } else {
        let folder = session
            .folder_path
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::current_dir().ok());
        local_languages(folder.as_deref(), &state)
    };
    Ok(ToolResult::with_structured(
        state.status_text(project.as_ref()),
        report(&state, project.as_ref()),
    ))
}
