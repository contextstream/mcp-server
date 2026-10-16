//! Cross-project scope for agents: the session working set (the primary
//! project plus related projects the same work touches) and loud alerts when
//! the project scope is wrong.
//!
//! Some clients, Claude Code among them, hand the model only the structured
//! part of a tool result, so every alert goes into the structured
//! `scope_alerts` array as well as the readable text. Alerts are rebuilt on
//! every call and never deduplicated: a wrong scope stays visible until it is
//! fixed.

use mcp_client::ContextStreamClient;
use mcp_types::api::{ProjectRoutingCandidate, ProjectRoutingContext, WorkingSetView};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Related-project cache for search
// ---------------------------------------------------------------------------

/// How long a working set seen in init/context (or fetched) is reused.
const RELATED_CACHE_TTL: Duration = Duration::from_secs(300);
const RELATED_CACHE_CAP: usize = 4_096;
const RELATED_FETCH_TIMEOUT: Duration = Duration::from_millis(800);

/// A related project as search needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RelatedProject {
    pub id: Uuid,
    pub name: String,
}

/// Related projects of a working set (the primary excluded).
pub(crate) fn related_from_view(view: Option<&WorkingSetView>) -> Vec<RelatedProject> {
    let Some(view) = view else {
        return Vec::new();
    };
    view.related
        .iter()
        .filter_map(|entry| {
            let id = entry.project_id?;
            (Some(id) != view.primary.project_id).then(|| RelatedProject {
                id,
                name: entry.project_name.clone(),
            })
        })
        .collect()
}

type RelatedCache = HashMap<(String, Uuid), (Instant, Vec<RelatedProject>)>;

fn related_cache() -> &'static Mutex<RelatedCache> {
    static CACHE: OnceLock<Mutex<RelatedCache>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Partitioned per caller: the hosted gateway serves many users from one
/// process, and a working set is personal.
fn caller_identity() -> Option<String> {
    super::atlas_warm_cache::current_caller_cache_scope()
        .cache_identity()
        .map(str::to_string)
}

/// Remember the related projects of `primary` (an empty list too, so search
/// does not ask again for a single-project session).
pub(crate) fn remember_related_projects(primary: Uuid, related: Vec<RelatedProject>) {
    let Some(identity) = caller_identity() else {
        return;
    };
    let mut cache = related_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if cache.len() >= RELATED_CACHE_CAP {
        cache.retain(|_, (seen, _)| seen.elapsed() < RELATED_CACHE_TTL);
        if cache.len() >= RELATED_CACHE_CAP {
            cache.clear();
        }
    }
    let related = related
        .into_iter()
        .filter(|project| project.id != primary)
        .collect();
    cache.insert((identity, primary), (Instant::now(), related));
}

fn cached_related_projects(primary: Uuid) -> Option<Vec<RelatedProject>> {
    let identity = caller_identity()?;
    let cache = related_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    cache
        .get(&(identity, primary))
        .filter(|(seen, _)| seen.elapsed() < RELATED_CACHE_TTL)
        .map(|(_, related)| related.clone())
}

/// Related projects of `primary` for the current caller: from the cache, else
/// one bounded API read. Errors and timeouts yield none (search must not fail
/// or slow down because of an advisory list).
pub(crate) async fn related_projects_for(
    client: &ContextStreamClient,
    primary: Uuid,
) -> Vec<RelatedProject> {
    if let Some(related) = cached_related_projects(primary) {
        return related;
    }
    let fetched = tokio::time::timeout(RELATED_FETCH_TIMEOUT, client.project_working_set(primary))
        .await
        .ok()
        .and_then(Result::ok)
        .and_then(|value| serde_json::from_value::<WorkingSetView>(value).ok());
    match fetched {
        Some(view) => {
            let related = related_from_view(Some(&view));
            remember_related_projects(primary, related.clone());
            related
        }
        None => Vec::new(),
    }
}

pub(crate) const SCOPE_ALERTS_FIELD: &str = "scope_alerts";
pub(crate) const WORKING_SET_FIELD: &str = "working_set";
const MISMATCH_MARKER: &str = "[SCOPE_MISMATCH]";
const WORKING_SET_MARKER: &str = "[WORKING_SET]";

/// Reasons that mean the checkout demonstrably belongs elsewhere (or to
/// several projects at once). Never muted.
fn reason_is_definitive_conflict(reason: &str) -> bool {
    matches!(
        reason,
        "folder_bound_to_different_project"
            | "folder_is_registered_root_of_different_project"
            | "folder_path_matches_different_project"
            | "repository_identity_matches_different_project"
            | "repository_identity_conflicts_with_active_project"
            | "repository_identity_matches_multiple_projects"
    )
}

/// Statuses under which no project-scoped work should happen yet.
fn status_blocks_project_work(status: &str) -> bool {
    matches!(
        status,
        "needs_project_selection"
            | "needs_project_setup"
            | "needs_workspace_selection"
            | "missing_project"
            | "project_missing"
            | "unresolved"
    )
}

fn project_label(name: Option<&str>, id: Option<Uuid>) -> Option<String> {
    let name = name.map(str::trim).filter(|name| !name.is_empty());
    match (name, id) {
        (Some(name), Some(id)) => Some(format!("{name} ({id})")),
        (Some(name), None) => Some(name.to_string()),
        (None, Some(id)) => Some(id.to_string()),
        (None, None) => None,
    }
}

fn candidate_label(candidate: &ProjectRoutingCandidate) -> String {
    project_label(candidate.project_name.as_deref(), candidate.project_id)
        .unwrap_or_else(|| "another project".to_string())
}

fn candidates_text(routing: &ProjectRoutingContext) -> String {
    routing
        .candidates
        .iter()
        .take(3)
        .map(candidate_label)
        .collect::<Vec<_>>()
        .join(", ")
}

fn is_related(working_set: Option<&WorkingSetView>, project_id: Option<Uuid>) -> bool {
    let (Some(view), Some(project_id)) = (working_set, project_id) else {
        return false;
    };
    view.related_project_ids().contains(&project_id)
}

/// A loud, actionable banner when the session's project scope is wrong or
/// unresolved. The first ~160 characters carry the fact, because tight
/// budgets truncate structured strings.
pub(crate) fn scope_mismatch_alert(
    routing: &ProjectRoutingContext,
    working_set: Option<&WorkingSetView>,
) -> Option<String> {
    let status = routing
        .status
        .as_deref()
        .map(|status| status.trim().to_ascii_lowercase())
        .unwrap_or_default();
    let reason = routing.reason.as_deref().map(str::trim).unwrap_or_default();
    let folder = routing
        .folder_path
        .as_deref()
        .map(str::trim)
        .filter(|folder| !folder.is_empty())
        .unwrap_or("this folder");
    let current = project_label(
        routing.current_project_name.as_deref(),
        routing.current_project_id,
    );
    let action = routing
        .suggested_action
        .as_deref()
        .map(str::trim)
        .filter(|action| !action.is_empty());

    if reason == "repository_identity_matches_multiple_projects" {
        return Some(format!(
            "{MISMATCH_MARKER} Several projects claim this checkout's repository ({}). Choose the canonical one and pass its project_id before project-scoped writes.",
            candidates_text(routing)
        ));
    }

    if reason_is_definitive_conflict(reason) {
        let candidate = routing.candidates.first();
        // The folder's project is already related work of this session: the
        // agent is doing cross-project work on purpose.
        if candidate.is_some_and(|candidate| is_related(working_set, candidate.project_id)) {
            return None;
        }
        let other = candidate
            .map(candidate_label)
            .unwrap_or_else(|| "a different project".to_string());
        let current = current.unwrap_or_else(|| "no project".to_string());
        let mut alert = format!(
            "{MISMATCH_MARKER} {folder} belongs to {other}, but this session is scoped to {current}. Fix the scope before project-scoped search or writes."
        );
        if let Some(action) = action {
            alert.push_str(&format!(" {action}"));
        }
        if let (Some(candidate), Some(current_id)) = (candidate, routing.current_project_id) {
            let name = candidate
                .project_name
                .as_deref()
                .filter(|name| !name.trim().is_empty())
                .map(str::to_string)
                .or_else(|| candidate.project_id.map(|id| id.to_string()));
            if let Some(name) = name {
                alert.push_str(&format!(
                    " If the session's own work is in the current project and this folder is only related work, keep project_id={current_id} and add it with project(action=\"working_set\", add=[\"{name}\"])."
                ));
            }
        }
        return Some(alert);
    }

    if status_blocks_project_work(&status) {
        let action = action.unwrap_or("Pass project_id or choose a project first.");
        return Some(if status == "needs_project_setup" {
            format!(
                "{MISMATCH_MARKER} No ContextStream project matches {folder}; project-scoped writes would go to {}. {action}",
                current.unwrap_or_else(|| "no project".to_string())
            )
        } else if routing.candidates.is_empty() {
            format!(
                "{MISMATCH_MARKER} Project scope for {folder} is unresolved ({status}). {action}"
            )
        } else {
            format!(
                "{MISMATCH_MARKER} Project scope for {folder} is unresolved ({status}); candidates: {}. {action}",
                candidates_text(routing)
            )
        });
    }
    None
}

/// One line describing the working set, or `None` for a single-project
/// session with nothing to suggest.
pub(crate) fn working_set_line(view: &WorkingSetView) -> Option<String> {
    if view.related.is_empty() && view.suggested.is_empty() {
        return None;
    }
    let primary = project_label(Some(&view.primary.project_name), view.primary.project_id)
        .unwrap_or_else(|| "the current project".to_string());
    let mut line = format!("{WORKING_SET_MARKER} primary={primary}");
    if !view.related.is_empty() {
        let related = view
            .related
            .iter()
            .map(|entry| {
                let mut detail = Vec::new();
                if let Some(id) = entry.project_id {
                    detail.push(id.to_string());
                }
                if !entry.sources.is_empty() {
                    detail.push(entry.sources.join("+"));
                }
                if entry.added_now {
                    detail.push("added now".to_string());
                }
                if detail.is_empty() {
                    entry.project_name.clone()
                } else {
                    format!("{} ({})", entry.project_name, detail.join("; "))
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        line.push_str(&format!(
            "; related={related}. Keep the primary project_id for this session's own work. Search also covers related projects (labelled). Save decisions, lessons, tickets and docs about a related project's code under that project's project_id."
        ));
    } else {
        line.push('.');
    }
    if let Some(first) = view.suggested.first() {
        let names = view
            .suggested
            .iter()
            .map(|suggestion| {
                project_label(Some(&suggestion.project_name), suggestion.project_id)
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>()
            .join(", ");
        line.push_str(&format!(
            " The message also names {names}; if this task changes that code, add it: project(action=\"working_set\", add=[\"{}\"]).",
            first.project_name
        ));
    }
    Some(line)
}

/// The working set without timestamps or long reasons, for structured output.
pub(crate) fn compact_working_set(view: &WorkingSetView) -> Value {
    serde_json::json!({
        "primary": {
            "project_id": view.primary.project_id,
            "project_name": view.primary.project_name,
        },
        "related": view.related.iter().map(|entry| serde_json::json!({
            "project_id": entry.project_id,
            "project_name": entry.project_name,
            "sources": entry.sources,
            "path": entry.path,
        })).collect::<Vec<_>>(),
        "suggested": view.suggested.iter().map(|suggestion| serde_json::json!({
            "project_id": suggestion.project_id,
            "project_name": suggestion.project_name,
        })).collect::<Vec<_>>(),
    })
}

pub(crate) fn parse_working_set(payload: &Value) -> Option<WorkingSetView> {
    let value = payload.get(WORKING_SET_FIELD)?;
    serde_json::from_value::<WorkingSetView>(value.clone()).ok()
}

/// All alerts for one init/context result, mismatch first.
pub(crate) fn scope_alerts(
    routing: Option<&ProjectRoutingContext>,
    working_set: Option<&WorkingSetView>,
) -> Vec<String> {
    let mut alerts = Vec::new();
    if let Some(alert) = routing.and_then(|routing| scope_mismatch_alert(routing, working_set)) {
        alerts.push(alert);
    }
    if let Some(line) = working_set.and_then(working_set_line) {
        alerts.push(line);
    }
    alerts
}

/// Put the alerts into the structured result and the text: a mismatch banner
/// at the top of the text, the working-set line at its end (unless the
/// server's own context text already carries one).
pub(crate) fn apply_scope_alerts(
    text: &mut String,
    structured: &mut Value,
    routing: Option<&ProjectRoutingContext>,
    working_set: Option<&WorkingSetView>,
) {
    let alerts = scope_alerts(routing, working_set);
    if let Some(object) = structured.as_object_mut() {
        match working_set {
            Some(view) if !(view.related.is_empty() && view.suggested.is_empty()) => {
                object.insert(WORKING_SET_FIELD.to_string(), compact_working_set(view));
            }
            _ => {
                object.remove(WORKING_SET_FIELD);
            }
        }
        if alerts.is_empty() {
            object.remove(SCOPE_ALERTS_FIELD);
        } else {
            object.insert(
                SCOPE_ALERTS_FIELD.to_string(),
                Value::Array(alerts.iter().cloned().map(Value::String).collect()),
            );
        }
    }
    for alert in &alerts {
        if alert.starts_with(MISMATCH_MARKER) {
            if !text.contains(MISMATCH_MARKER) {
                *text = if text.is_empty() {
                    alert.clone()
                } else {
                    format!("{alert}\n\n{text}")
                };
            }
        } else if !text.contains(WORKING_SET_MARKER) && !text.contains("WORKING_SET:") {
            if !text.is_empty() {
                text.push_str("\n\n");
            }
            text.push_str(alert);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_types::api::{WorkingSetEntry, WorkingSetProject, WorkingSetSuggestion};

    const CURRENT: Uuid = Uuid::from_u128(0x11);
    const OTHER: Uuid = Uuid::from_u128(0x22);

    fn routing(status: &str, reason: &str) -> ProjectRoutingContext {
        serde_json::from_value(serde_json::json!({
            "status": status,
            "reason": reason,
            "current_project_id": CURRENT,
            "current_project_name": "acme-web",
            "folder_path": "/work/acme-api",
            "suggested_action": "Switch project scope to acme-api or pass the intended project_id explicitly.",
            "candidates": [{"project_id": OTHER, "project_name": "acme-api", "score": 1.0}]
        }))
        .unwrap()
    }

    fn view(related: bool, suggested: bool) -> WorkingSetView {
        WorkingSetView {
            primary: WorkingSetProject {
                project_id: Some(CURRENT),
                project_name: "acme-web".to_string(),
                workspace_id: None,
            },
            related: if related {
                vec![WorkingSetEntry {
                    project_id: Some(OTHER),
                    project_name: "acme-api".to_string(),
                    sources: vec!["activity".to_string()],
                    added_now: true,
                    ..Default::default()
                }]
            } else {
                Vec::new()
            },
            suggested: if suggested {
                vec![WorkingSetSuggestion {
                    project_id: Some(Uuid::from_u128(0x33)),
                    project_name: "acme-desktop".to_string(),
                }]
            } else {
                Vec::new()
            },
            guidance: String::new(),
        }
    }

    #[test]
    fn definitive_folder_conflict_is_loud_and_names_both_projects() {
        let alert = scope_mismatch_alert(
            &routing("uncertain", "folder_bound_to_different_project"),
            None,
        )
        .expect("alert");
        assert!(alert.starts_with("[SCOPE_MISMATCH] /work/acme-api belongs to acme-api ("));
        assert!(alert.contains("scoped to acme-web ("));
        assert!(alert.contains("add=[\"acme-api\"]"));
    }

    #[test]
    fn conflict_with_a_related_project_is_cross_project_work_not_a_mismatch() {
        let related = view(true, false);
        assert!(scope_mismatch_alert(
            &routing(
                "uncertain",
                "folder_is_registered_root_of_different_project"
            ),
            Some(&related),
        )
        .is_none());
    }

    #[test]
    fn soft_statuses_stay_quiet_and_blocking_statuses_alert() {
        assert!(scope_mismatch_alert(&routing("confirmed", "folder_binding"), None).is_none());
        assert!(scope_mismatch_alert(
            &routing("uncertain", "message_mentions_different_project"),
            None
        )
        .is_none());
        let setup = scope_mismatch_alert(
            &routing("needs_project_setup", "folder_path_has_no_matching_project"),
            None,
        )
        .expect("setup alert");
        assert!(
            setup.starts_with("[SCOPE_MISMATCH] No ContextStream project matches /work/acme-api")
        );
        let selection = scope_mismatch_alert(
            &routing("needs_project_selection", "no_active_project"),
            None,
        )
        .expect("selection alert");
        assert!(selection.contains("candidates: acme-api ("));
    }

    #[test]
    fn duplicate_repository_projects_alert() {
        let alert = scope_mismatch_alert(
            &routing("uncertain", "repository_identity_matches_multiple_projects"),
            None,
        )
        .expect("alert");
        assert!(alert.contains("Several projects claim"));
    }

    #[test]
    fn working_set_line_lists_related_projects_and_suggestions() {
        assert!(working_set_line(&view(false, false)).is_none());
        let line = working_set_line(&view(true, true)).expect("line");
        assert!(
            line.starts_with("[WORKING_SET] primary=acme-web ("),
            "{line}"
        );
        assert!(line.contains("related=acme-api ("), "{line}");
        assert!(line.contains("activity; added now"), "{line}");
        assert!(line.contains("add=[\"acme-desktop\"]"), "{line}");
    }

    #[test]
    fn alerts_reach_both_text_and_structured_output_every_time() {
        let routing = routing("uncertain", "folder_bound_to_different_project");
        let ws = view(false, true);
        for _ in 0..2 {
            let mut text = "body".to_string();
            let mut structured = serde_json::json!({"working_set": {"verbose": true}});
            apply_scope_alerts(&mut text, &mut structured, Some(&routing), Some(&ws));
            assert!(text.starts_with("[SCOPE_MISMATCH]"), "{text}");
            assert!(
                text.ends_with("]).") || text.contains("[WORKING_SET]"),
                "{text}"
            );
            let alerts = structured[SCOPE_ALERTS_FIELD].as_array().unwrap();
            assert_eq!(alerts.len(), 2);
            assert!(
                structured[WORKING_SET_FIELD]["suggested"][0]["project_name"] == "acme-desktop"
            );
        }
    }

    #[test]
    fn server_text_that_already_has_the_line_is_not_duplicated() {
        let ws = view(true, false);
        let mut text = "[WORKING_SET] primary=acme-web (x); related=acme-api\nbody".to_string();
        let mut structured = serde_json::json!({});
        apply_scope_alerts(&mut text, &mut structured, None, Some(&ws));
        assert_eq!(text.matches("[WORKING_SET]").count(), 1);
        assert_eq!(structured[SCOPE_ALERTS_FIELD].as_array().unwrap().len(), 1);
    }

    #[test]
    fn empty_alerts_remove_stale_fields() {
        let mut text = String::new();
        let mut structured = serde_json::json!({"scope_alerts": ["old"], "working_set": {}});
        apply_scope_alerts(&mut text, &mut structured, None, Some(&view(false, false)));
        assert!(structured.get(SCOPE_ALERTS_FIELD).is_none());
        assert!(structured.get(WORKING_SET_FIELD).is_none());
        assert!(text.is_empty());
    }
}
