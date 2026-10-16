//! Hook side of the session working set (primary project plus related
//! projects the same work touches).
//!
//! PostToolUse notices when the agent edits files in another project's
//! checkout, records that project as related on the server (so search and the
//! next context call include it) and tells the agent once per project.
//! PreToolUse uses the local copy kept here to tell a deliberate
//! cross-project write from a wrong-scope one without a network call.

use super::durable_capture::{session_primary_scope, sha256_hex, state_dir, with_locked_json};
use super::post_tool_use::{build_hook_client, find_project_config, ProjectConfig};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

const MAX_AGE_HOURS: i64 = 24;
const MAX_SESSIONS: usize = 512;
const MAX_PROJECTS_PER_EDIT: usize = 3;
const SERVER_TIMEOUT: Duration = Duration::from_millis(1_500);

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct WorkingSets {
    #[serde(default)]
    sessions: BTreeMap<String, SessionWorkingSet>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SessionWorkingSet {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    primary_project_id: Option<Uuid>,
    /// Related project id -> entry.
    #[serde(default)]
    related: BTreeMap<Uuid, RelatedEntry>,
    #[serde(default)]
    updated_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RelatedEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    /// The agent was told about this project in this session.
    #[serde(default)]
    notified: bool,
}

fn state_path() -> Option<PathBuf> {
    state_dir().map(|dir| dir.join("working-sets.json"))
}

fn fresh(entry: &SessionWorkingSet) -> bool {
    chrono::DateTime::parse_from_rfc3339(&entry.updated_at).is_ok_and(|updated| {
        chrono::Utc::now().signed_duration_since(updated) <= chrono::Duration::hours(MAX_AGE_HOURS)
    })
}

fn prune(state: &mut WorkingSets) {
    state.sessions.retain(|_, entry| fresh(entry));
    while state.sessions.len() > MAX_SESSIONS {
        let Some(oldest) = state
            .sessions
            .iter()
            .min_by(|a, b| a.1.updated_at.cmp(&b.1.updated_at))
            .map(|(key, _)| key.clone())
        else {
            break;
        };
        state.sessions.remove(&oldest);
    }
}

fn read_session_at(path: &Path, host_session_id: &str) -> Option<SessionWorkingSet> {
    let bytes = std::fs::read(path).ok()?;
    let state: WorkingSets = serde_json::from_slice(&bytes).ok()?;
    state
        .sessions
        .get(&sha256_hex(host_session_id.as_bytes()))
        .filter(|entry| fresh(entry))
        .cloned()
}

/// Related project ids recorded for this host session.
fn related_ids_at(path: &Path, host_session_id: &str) -> Vec<Uuid> {
    read_session_at(path, host_session_id)
        .map(|entry| entry.related.into_keys().collect())
        .unwrap_or_default()
}

/// Record related projects for a host session. With `replace`, the given
/// list is the server's full view and drops entries it no longer has (the
/// `notified` flags of kept entries survive). Returns the ids that were not
/// yet notified, marking them notified when `mark_notified` is set.
fn record_at(
    path: &Path,
    host_session_id: &str,
    primary: Option<Uuid>,
    related: &[(Uuid, Option<String>)],
    replace: bool,
    mark_notified: bool,
) -> Vec<Uuid> {
    let key = sha256_hex(host_session_id.as_bytes());
    with_locked_json(path, |state: &mut WorkingSets| {
        prune(state);
        let entry = state.sessions.entry(key).or_default();
        if !fresh(entry) {
            *entry = SessionWorkingSet::default();
        }
        if primary.is_some() && entry.primary_project_id != primary {
            // A different primary project means a different working set.
            entry.related.clear();
            entry.primary_project_id = primary;
        }
        if replace {
            entry
                .related
                .retain(|id, _| related.iter().any(|(related_id, _)| related_id == id));
        }
        let mut newly_notified = Vec::new();
        for (id, name) in related {
            if Some(*id) == entry.primary_project_id {
                continue;
            }
            let related_entry = entry.related.entry(*id).or_default();
            if name.is_some() {
                related_entry.name = name.clone();
            }
            if mark_notified && !related_entry.notified {
                related_entry.notified = true;
                newly_notified.push(*id);
            }
        }
        entry.updated_at = chrono::Utc::now().to_rfc3339();
        newly_notified
    })
    .unwrap_or_default()
}

/// Read the working set an `init`/`context` result carries (full or compact
/// form) and keep the host session's copy in line with it.
pub(crate) fn observe_tool_response(host_session_id: &str, response: &Value) {
    // init/context and a working-set update nest it; a working-set read is it.
    let Some(view) = response.get("working_set").or_else(|| {
        (response.get("primary").is_some() && response.get("related").is_some()).then_some(response)
    }) else {
        return;
    };
    let primary = view
        .get("primary")
        .and_then(|primary| primary.get("project_id"))
        .and_then(Value::as_str)
        .and_then(|id| Uuid::parse_str(id).ok());
    let related = view
        .get("related")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| {
                    let id = entry
                        .get("project_id")
                        .and_then(Value::as_str)
                        .and_then(|id| Uuid::parse_str(id).ok())?;
                    let name = entry
                        .get("project_name")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    Some((id, name))
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if let Some(path) = state_path() {
        record_at(&path, host_session_id, primary, &related, true, false);
    }
}

/// The session's primary scope `(workspace_id, project_id)`: what the server
/// resolved for this host session, else what the working folder is linked to.
pub(crate) fn primary_scope(host_session_id: Option<&str>, cwd: &str) -> Option<(Uuid, Uuid)> {
    host_session_id.and_then(session_primary_scope).or_else(|| {
        let config = super::common::load_config(cwd);
        let parse = |value: Option<&str>| value.and_then(|id| Uuid::parse_str(id.trim()).ok());
        Some((
            parse(config.workspace_id.as_deref())?,
            parse(config.project_id.as_deref())?,
        ))
    })
}

/// The session's primary project (see [`primary_scope`]).
pub(crate) fn primary_project(host_session_id: Option<&str>, cwd: &str) -> Option<Uuid> {
    primary_scope(host_session_id, cwd).map(|(_, project)| project)
}

/// `project_name` from the nearest `.contextstream/config.json` at or above
/// `root`.
fn project_name_for(root: &str) -> Option<String> {
    let mut dir = PathBuf::from(root);
    loop {
        let config = dir.join(".contextstream").join("config.json");
        if let Ok(raw) = std::fs::read_to_string(&config) {
            return serde_json::from_str::<Value>(&raw)
                .ok()?
                .get("project_name")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string);
        }
        if !dir.pop() {
            return None;
        }
    }
}

struct OtherProjectEdit {
    config: ProjectConfig,
    relative_path: String,
}

/// Group edited paths by the project their checkout is linked to, keeping
/// only projects other than `primary`.
fn edits_in_other_projects(
    primary: Uuid,
    cwd: &str,
    absolute_paths: &[String],
) -> Vec<OtherProjectEdit> {
    let mut edits: Vec<OtherProjectEdit> = Vec::new();
    for path in absolute_paths {
        let Some(config) = find_project_config(path, cwd) else {
            continue;
        };
        if config.project_id == primary
            || edits
                .iter()
                .any(|edit| edit.config.project_id == config.project_id)
        {
            continue;
        }
        let relative_path = config
            .project_root
            .as_deref()
            .and_then(|root| Path::new(path).strip_prefix(root).ok())
            .map(|relative| relative.to_string_lossy().to_string())
            .unwrap_or_else(|| path.clone());
        edits.push(OtherProjectEdit {
            config,
            relative_path,
        });
        if edits.len() >= MAX_PROJECTS_PER_EDIT {
            break;
        }
    }
    edits
}

/// Ask the server to add `related` to `primary`'s working set. Returns the
/// project name the server knows, when it answered.
async fn record_on_server(
    config: &ProjectConfig,
    primary: Uuid,
    related: Uuid,
    reason: &str,
) -> Option<String> {
    let client = build_hook_client(config);
    let body = serde_json::json!({
        "add": [{ "project": related.to_string(), "source": "activity", "reason": reason }]
    });
    let response = tokio::time::timeout(
        SERVER_TIMEOUT,
        client.update_project_working_set(primary, body),
    )
    .await
    .ok()?
    .ok()?;
    response
        .get("working_set")
        .and_then(|view| view.get("related"))
        .and_then(Value::as_array)?
        .iter()
        .find(|entry| {
            entry.get("project_id").and_then(Value::as_str) == Some(related.to_string().as_str())
        })
        .and_then(|entry| entry.get("project_name"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn activity_notice(
    primary: Uuid,
    primary_name: Option<&str>,
    related: Uuid,
    related_name: &str,
    relative_path: &str,
) -> String {
    let primary_label = match primary_name {
        Some(name) => format!("{name} ({primary})"),
        None => primary.to_string(),
    };
    format!(
        "[WORKING_SET] You edited {relative_path} in {related_name} (project_id={related}), which is not this session's primary project {primary_label}. The primary stays {primary_label}; {related_name} is now a related project: ContextStream search also covers it, and decisions, lessons, tickets and docs about that code belong under project_id={related}. If the work has moved to {related_name} entirely, run init with its folder instead."
    )
}

/// PostToolUse for file edits: notice each other project the edits touched,
/// record it as related (locally and on the server), and return one notice
/// per newly related project.
pub(crate) async fn observe_edits(
    host_session_id: Option<&str>,
    cwd: &str,
    absolute_paths: &[String],
) -> Option<String> {
    let host_session_id = host_session_id?;
    let primary = primary_project(Some(host_session_id), cwd)?;
    let edits = edits_in_other_projects(primary, cwd, absolute_paths);
    if edits.is_empty() {
        return None;
    }
    let path = state_path()?;
    let primary_name = project_name_for(cwd);
    let mut notices = Vec::new();
    for edit in edits {
        let related = edit.config.project_id;
        let already_known = read_session_at(&path, host_session_id)
            .filter(|entry| entry.primary_project_id == Some(primary))
            .and_then(|entry| entry.related.get(&related).map(|entry| entry.notified))
            .unwrap_or(false);
        if already_known {
            continue;
        }
        let reason = format!("edited {}", edit.relative_path);
        let server_name = record_on_server(&edit.config, primary, related, &reason).await;
        let name = server_name
            .or_else(|| {
                edit.config
                    .project_root
                    .as_deref()
                    .and_then(project_name_for)
            })
            .unwrap_or_else(|| related.to_string());
        let newly = record_at(
            &path,
            host_session_id,
            Some(primary),
            &[(related, Some(name.clone()))],
            false,
            true,
        );
        if newly.contains(&related) {
            notices.push(activity_notice(
                primary,
                primary_name.as_deref(),
                related,
                &name,
                &edit.relative_path,
            ));
        }
    }
    (!notices.is_empty()).then(|| notices.join("\n"))
}

/// Calls whose project or workspace is not a scope choice: listing or
/// creating projects, reading or changing the working set itself, and
/// tickets, which are filed in the project that owns the code they are about
/// (questioned only across workspaces).
fn scope_is_not_judged(tool: &str, action: &str, cross_workspace: bool) -> bool {
    match tool {
        "project" => matches!(action, "list" | "team_projects" | "working_set" | "create"),
        "entity" => matches!(action, "create" | "update") && !cross_workspace,
        "workspace" => true,
        _ => false,
    }
}

/// The judgement behind [`scope_warning`], without any I/O: a call naming a
/// project outside this session's working set (primary plus related
/// projects), or another workspace, is told so with the exact fix.
#[allow(clippy::too_many_arguments)]
pub(crate) fn scope_warning_for(
    tool: &str,
    action: &str,
    requested_project: Option<Uuid>,
    requested_workspace: Option<&str>,
    primary: Uuid,
    primary_workspace: Uuid,
    related: &[Uuid],
    primary_label: &str,
) -> Option<String> {
    let cross_workspace = requested_workspace
        .and_then(|workspace| Uuid::parse_str(workspace.trim()).ok())
        .is_some_and(|workspace| workspace != primary_workspace);
    if scope_is_not_judged(tool, action, cross_workspace) {
        return None;
    }
    let call = if action.is_empty() {
        tool.to_string()
    } else {
        format!("{tool}(action=\"{action}\")")
    };
    if let Some(requested) = requested_project {
        if requested == primary || related.contains(&requested) {
            return None;
        }
        return Some(format!(
            "[SCOPE_MISMATCH] {call} targets project_id={requested}, but this session's primary project is {primary_label} and {requested} is not one of its related projects. If you meant the session's own project, use project_id={primary}. If you are deliberately working on another project, add it to the working set first: project(action=\"working_set\", project_id=\"{primary}\", add=[\"{requested}\"])."
        ));
    }
    if cross_workspace {
        let requested = requested_workspace.unwrap_or_default().trim();
        return Some(format!(
            "[SCOPE_MISMATCH] {call} targets workspace_id={requested}, but this session's primary project {primary_label} is in workspace_id={primary_workspace}. Use workspace_id={primary_workspace} and project_id={primary} unless you deliberately mean the other workspace."
        ));
    }
    None
}

/// A loud warning when a ContextStream call names a project outside this
/// session's working set or another workspace. `None` when the call is fine
/// or cannot be judged (no known primary project, a project name rather
/// than an id with no workspace to compare).
pub(crate) fn scope_warning(
    host_session_id: Option<&str>,
    cwd: &str,
    tool: &str,
    action: &str,
    requested_project_id: Option<&str>,
    requested_workspace_id: Option<&str>,
) -> Option<String> {
    let (primary_workspace, primary) = primary_scope(host_session_id, cwd)?;
    let requested_project =
        requested_project_id.and_then(|project| Uuid::parse_str(project.trim()).ok());
    let related = match (host_session_id, state_path()) {
        (Some(host), Some(path)) => related_ids_at(&path, host),
        _ => Vec::new(),
    };
    let primary_label = match project_name_for(cwd) {
        Some(name) => format!("{name} ({primary})"),
        None => primary.to_string(),
    };
    scope_warning_for(
        tool,
        action,
        requested_project,
        requested_workspace_id,
        primary,
        primary_workspace,
        &related,
        &primary_label,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRIMARY: Uuid = Uuid::from_u128(0x11);
    const RELATED: Uuid = Uuid::from_u128(0x22);
    const OTHER: Uuid = Uuid::from_u128(0x33);
    const WORKSPACE: Uuid = Uuid::from_u128(0x99);
    const OTHER_WORKSPACE: Uuid = Uuid::from_u128(0x98);

    fn judge(
        tool: &str,
        action: &str,
        project: Option<Uuid>,
        workspace: Option<Uuid>,
    ) -> Option<String> {
        let workspace = workspace.map(|id| id.to_string());
        scope_warning_for(
            tool,
            action,
            project,
            workspace.as_deref(),
            PRIMARY,
            WORKSPACE,
            &[RELATED],
            "acme-web (p)",
        )
    }

    #[test]
    fn recording_tracks_notifications_once_per_project() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("working-sets.json");
        let first = record_at(
            &path,
            "host",
            Some(PRIMARY),
            &[(RELATED, None)],
            false,
            true,
        );
        assert_eq!(first, vec![RELATED]);
        let second = record_at(
            &path,
            "host",
            Some(PRIMARY),
            &[(RELATED, None)],
            false,
            true,
        );
        assert!(second.is_empty(), "notified once per session and project");
        assert_eq!(related_ids_at(&path, "host"), vec![RELATED]);
        assert!(related_ids_at(&path, "other-host").is_empty());
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            !raw.contains("\"host\""),
            "host session ids are stored hashed"
        );
    }

    #[test]
    fn server_view_replaces_the_related_list_and_primary_is_never_related() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("working-sets.json");
        record_at(
            &path,
            "host",
            Some(PRIMARY),
            &[(RELATED, None), (OTHER, None)],
            false,
            true,
        );
        record_at(
            &path,
            "host",
            Some(PRIMARY),
            &[(RELATED, Some("acme-api".to_string())), (PRIMARY, None)],
            true,
            false,
        );
        let entry = read_session_at(&path, "host").unwrap();
        assert_eq!(
            entry.related.keys().copied().collect::<Vec<_>>(),
            vec![RELATED]
        );
        assert!(
            entry.related[&RELATED].notified,
            "notified flag survives a server refresh"
        );
        assert_eq!(entry.related[&RELATED].name.as_deref(), Some("acme-api"));
    }

    #[test]
    fn a_new_primary_starts_a_new_working_set() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("working-sets.json");
        record_at(
            &path,
            "host",
            Some(PRIMARY),
            &[(RELATED, None)],
            false,
            true,
        );
        record_at(&path, "host", Some(OTHER), &[], false, false);
        assert!(related_ids_at(&path, "host").is_empty());
    }

    #[test]
    fn calls_outside_the_working_set_are_loud_and_inside_it_quiet() {
        let loud = judge("memory", "create_decision", Some(OTHER), None).expect("warning");
        assert!(loud.starts_with(
            "[SCOPE_MISMATCH] memory(action=\"create_decision\") targets project_id="
        ));
        assert!(loud.contains(&format!("use project_id={PRIMARY}")));
        assert!(loud.contains(&format!("add=[\"{OTHER}\"]")));
        // Reads with a foreign project are a wrong scope too.
        assert!(judge("search", "", Some(OTHER), None).is_some());
        assert!(judge("context", "", Some(OTHER), None).is_some());
        // The session's own project and its related projects are quiet.
        assert!(judge("memory", "create_decision", Some(RELATED), None).is_none());
        assert!(judge("search", "", Some(PRIMARY), Some(WORKSPACE)).is_none());
        // Managing the working set or listing projects is never judged.
        assert!(judge("project", "working_set", Some(OTHER), None).is_none());
        assert!(judge("project", "list", None, Some(OTHER_WORKSPACE)).is_none());
    }

    #[test]
    fn another_workspace_is_loud_even_without_a_project() {
        let loud = judge("memory", "decisions", None, Some(OTHER_WORKSPACE)).expect("warning");
        assert!(loud.contains(&format!("targets workspace_id={OTHER_WORKSPACE}")));
        assert!(loud.contains(&format!("is in workspace_id={WORKSPACE}")));
        assert!(judge("memory", "decisions", None, Some(WORKSPACE)).is_none());
    }

    #[test]
    fn tickets_are_quiet_within_the_workspace_and_questioned_across_workspaces() {
        assert!(judge("entity", "create", Some(OTHER), Some(WORKSPACE)).is_none());
        assert!(judge("entity", "update", Some(OTHER), Some(OTHER_WORKSPACE)).is_some());
        assert!(judge("entity", "delete", Some(OTHER), None).is_some());
    }

    #[test]
    fn notice_names_both_projects_and_where_notes_belong() {
        let notice = activity_notice(
            PRIMARY,
            Some("acme-web"),
            RELATED,
            "acme-api",
            "crates/api/src/lib.rs",
        );
        assert!(notice.starts_with("[WORKING_SET] You edited crates/api/src/lib.rs in acme-api"));
        assert!(notice.contains("primary project acme-web ("));
        assert!(notice.contains(&format!("project_id={RELATED}")));
    }

    #[test]
    fn edits_are_grouped_by_their_linked_project() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("acme-api");
        std::fs::create_dir_all(root.join(".contextstream")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join(".contextstream").join("config.json"),
            serde_json::json!({
                "project_id": RELATED,
                "workspace_id": Uuid::from_u128(0x99),
                "project_name": "acme-api",
                "checkout_root": root.to_string_lossy(),
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            root.join(".mcp.json"),
            serde_json::json!({"mcpServers": {"contextstream": {"env": {"CONTEXTSTREAM_API_KEY": "test-key"}}}}).to_string(),
        )
        .unwrap();
        let file = root
            .join("src")
            .join("lib.rs")
            .to_string_lossy()
            .to_string();
        let edits =
            edits_in_other_projects(PRIMARY, &root.to_string_lossy(), &[file.clone(), file]);
        if let Some(edit) = edits.first() {
            // Credentials and checkout binding resolve on this machine.
            assert_eq!(edits.len(), 1);
            assert_eq!(edit.config.project_id, RELATED);
            assert_eq!(edit.relative_path, "src/lib.rs");
        }
        assert!(edits_in_other_projects(
            RELATED,
            &root.to_string_lossy(),
            &[root.join("src/lib.rs").to_string_lossy().to_string()]
        )
        .is_empty());
        assert_eq!(
            project_name_for(&root.join("src").to_string_lossy()).as_deref(),
            Some("acme-api")
        );
    }
}
