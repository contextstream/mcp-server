//! Placeholder ids and unmatched folders: the parts of scope resolution that do
//! not need a network call.
//!
//! Agents fill `workspace_id` / `project_id` from rules text, notes and
//! templates. When none of those held a real id they send the all-zero UUID or
//! text like `<current_workspace_id>`, and the API answers `404 Workspace
//! 0000… not found`. A placeholder is not a request for a workspace, so it is
//! treated as no id and the folder, repository and account decide the scope.

use mcp_client::ContextStreamClient;
use mcp_types::api::Project;
use mcp_types::tool::{ContentItem, ToolResult};
use serde_json::{json, Value};
use uuid::Uuid;

/// Tool arguments that carry a workspace or project scope.
const SCOPE_ID_ARGS: [&str; 2] = ["workspace_id", "project_id"];

/// Structured-result key listing the ids init set aside.
pub(crate) const SCOPE_REPAIRS_KEY: &str = "scope_repairs";

/// The most projects read when looking for near-name candidates.
const MAX_CATALOG_PROJECTS: usize = 1_000;

/// Words that say nothing about which project a folder belongs to.
const GENERIC_NAME_TOKENS: [&str; 11] = [
    "app", "apps", "web", "api", "src", "dev", "repo", "project", "code", "the", "and",
];

/// How many near-name projects an agent is shown.
pub(crate) const MAX_SCOPE_CANDIDATES: usize = 5;

/// True when `value` is not a real workspace or project id: empty, the nil
/// UUID in any spelling, a `<template>` or `{template}` placeholder, or a
/// null-like word.
pub(crate) fn is_placeholder_scope_id(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return true;
    }
    if let Ok(id) = Uuid::parse_str(trimmed) {
        return id.is_nil();
    }
    let templated = (trimmed.starts_with('<') && trimmed.ends_with('>'))
        || (trimmed.starts_with('{') && trimmed.ends_with('}'));
    templated
        || matches!(
            trimmed.to_ascii_lowercase().as_str(),
            "null" | "none" | "nil" | "undefined"
        )
}

/// What a caller sent for a workspace or project id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ScopeIdArg {
    /// Nothing was sent.
    Absent,
    /// A placeholder (see [`is_placeholder_scope_id`]); carries the text sent.
    Placeholder(String),
    /// A real, non-nil UUID.
    Valid(Uuid),
    /// Text that is neither a UUID nor a recognised placeholder.
    Malformed(String),
}

impl ScopeIdArg {
    pub(crate) fn valid(&self) -> Option<Uuid> {
        match self {
            Self::Valid(id) => Some(*id),
            _ => None,
        }
    }

    /// The caller named a scope on purpose, whether or not it parsed. A
    /// placeholder is the one thing that is not a choice.
    pub(crate) fn is_explicit(&self) -> bool {
        matches!(self, Self::Valid(_) | Self::Malformed(_))
    }

    pub(crate) fn placeholder(&self) -> Option<&str> {
        match self {
            Self::Placeholder(text) => Some(text),
            _ => None,
        }
    }
}

pub(crate) fn classify_scope_id_arg(value: Option<&str>) -> ScopeIdArg {
    let Some(raw) = value else {
        return ScopeIdArg::Absent;
    };
    if is_placeholder_scope_id(raw) {
        return ScopeIdArg::Placeholder(raw.trim().to_string());
    }
    match Uuid::parse_str(raw.trim()) {
        Ok(id) => ScopeIdArg::Valid(id),
        Err(_) => ScopeIdArg::Malformed(raw.trim().to_string()),
    }
}

/// Remove placeholder `workspace_id` / `project_id` arguments from a tool call
/// so a handler sees "no id" and resolves scope itself. Real ids, and text that
/// is wrong but not a placeholder, are left alone so the caller still gets a
/// validation error for them. Returns the names that were removed.
pub(crate) fn strip_placeholder_scope_ids(input: &mut Value) -> Vec<&'static str> {
    let Some(object) = input.as_object_mut() else {
        return Vec::new();
    };
    let mut removed = Vec::new();
    for name in SCOPE_ID_ARGS {
        let is_placeholder = object
            .get(name)
            .and_then(Value::as_str)
            .is_some_and(is_placeholder_scope_id);
        if is_placeholder {
            object.remove(name);
            removed.push(name);
        }
    }
    removed
}

/// A project in the workspace whose name resembles the folder's.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProjectCandidate {
    pub id: Uuid,
    pub name: String,
    pub score: f32,
}

fn name_tokens(value: &str) -> Vec<String> {
    let mut tokens: Vec<String> = value
        .to_ascii_lowercase()
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|token| token.len() >= 3 && !GENERIC_NAME_TOKENS.contains(token))
        .map(str::to_string)
        .collect();
    tokens.sort();
    tokens.dedup();
    tokens
}

fn squashed_name(value: &str) -> String {
    value
        .to_ascii_lowercase()
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect()
}

/// How alike two names are, from 0 (nothing shared) to 1 (same words). Shared
/// words count by overlap; one name containing the other (`mcp-server` inside
/// `mcp-server-init-scope`) counts at least half.
fn name_similarity(folder_name: &str, project_name: &str) -> f32 {
    let folder_tokens = name_tokens(folder_name);
    let project_tokens = name_tokens(project_name);
    let shared = folder_tokens
        .iter()
        .filter(|token| project_tokens.contains(token))
        .count();
    let total = folder_tokens.len() + project_tokens.len() - shared;
    let overlap = if total == 0 {
        0.0
    } else {
        shared as f32 / total as f32
    };

    let folder_key = squashed_name(folder_name);
    let project_key = squashed_name(project_name);
    let (short, long) = if folder_key.len() <= project_key.len() {
        (&folder_key, &project_key)
    } else {
        (&project_key, &folder_key)
    };
    let contained = short.len() >= 4 && long.contains(short.as_str());
    if contained {
        overlap.max(0.5)
    } else {
        overlap
    }
}

/// Projects whose names resemble `folder_name`, best first, at most `limit`.
/// `exclude` drops the project the session is already using.
pub(crate) fn similar_project_candidates(
    folder_name: &str,
    projects: &[Project],
    exclude: Option<Uuid>,
    limit: usize,
) -> Vec<ProjectCandidate> {
    let mut candidates: Vec<ProjectCandidate> = projects
        .iter()
        .filter(|project| Some(project.id) != exclude)
        .filter_map(|project| {
            let score = name_similarity(folder_name, &project.name);
            (score > 0.0).then(|| ProjectCandidate {
                id: project.id,
                name: project.name.clone(),
                score,
            })
        })
        .collect();
    candidates.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.id.cmp(&b.id))
    });
    candidates.truncate(limit);
    candidates
}

/// Projects in `workspace_id` that resemble the folder's name. Best effort: a
/// catalog that cannot be read means no suggestions, never a failed init.
pub(crate) async fn candidates_for_folder(
    client: &ContextStreamClient,
    workspace_id: Uuid,
    folder_path: &str,
    exclude: Option<Uuid>,
) -> Vec<ProjectCandidate> {
    let folder_name = folder_display_name(folder_path);
    match client
        .list_all_projects(Some(workspace_id), 200, MAX_CATALOG_PROJECTS)
        .await
    {
        Ok(projects) => {
            similar_project_candidates(&folder_name, &projects, exclude, MAX_SCOPE_CANDIDATES)
        }
        Err(error) => {
            tracing::debug!(error = %error, "project catalog unavailable for scope candidates");
            Vec::new()
        }
    }
}

pub(crate) fn folder_display_name(folder_path: &str) -> String {
    std::path::Path::new(folder_path)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(folder_path)
        .trim()
        .to_string()
}

/// The scope an init that found no match for its folder ended up using.
pub(crate) struct UnmatchedFolderScope<'a> {
    pub folder_path: &'a str,
    pub workspace_id: Uuid,
    pub workspace_name: Option<&'a str>,
    pub project_id: Uuid,
    pub project_name: Option<&'a str>,
    pub candidates: &'a [ProjectCandidate],
}

impl UnmatchedFolderScope<'_> {
    /// Structured form, for clients that show only `structuredContent`.
    pub(crate) fn to_value(&self) -> Value {
        let folder_name = folder_display_name(self.folder_path);
        json!({
            "status": "inherited_unmatched_folder",
            "folder_path": self.folder_path,
            "using": {
                "workspace_id": self.workspace_id.to_string(),
                "workspace_name": self.workspace_name,
                "project_id": self.project_id.to_string(),
                "project_name": self.project_name,
            },
            "candidates": self.candidates.iter().map(|candidate| json!({
                "project_id": candidate.id.to_string(),
                "name": candidate.name,
            })).collect::<Vec<_>>(),
            "next_steps": {
                "keep": "Carry on if this folder belongs to the project in `using`.",
                "switch": format!(
                    "init(folder_path=\"{}\", workspace_id=\"{}\", project_id=\"<candidate project_id>\")",
                    self.folder_path, self.workspace_id
                ),
                "create": format!(
                    "project(action=\"create\", workspace_id=\"{}\", name=\"{}\")",
                    self.workspace_id, folder_name
                ),
            },
        })
    }

    /// Text form for the init response. `compact` keeps it to the decision.
    pub(crate) fn notice(&self, compact: bool) -> String {
        let folder_name = folder_display_name(self.folder_path);
        let workspace = self.workspace_name.unwrap_or("(unnamed)");
        let project = self.project_name.unwrap_or("(unnamed)");

        let mut text = format!(
            "[SCOPE_RESOLUTION] No project matched folder `{folder_name}`, so this session kept its pinned scope: workspace {workspace} ({}), project {project} ({}).",
            self.workspace_id, self.project_id
        );
        if !compact {
            text.push_str(
                "\nCarry on if the folder belongs to that project. If it does not, pick one of these:",
            );
        }
        for (index, candidate) in self.candidates.iter().enumerate() {
            text.push_str(&format!(
                "\n  {}. {} ({}) -> init(folder_path=\"{}\", workspace_id=\"{}\", project_id=\"{}\")",
                index + 1,
                candidate.name,
                candidate.id,
                self.folder_path,
                self.workspace_id,
                candidate.id
            ));
        }
        text.push_str(&format!(
            "\n  Or create it: project(action=\"create\", workspace_id=\"{}\", name=\"{}\")",
            self.workspace_id, folder_name
        ));
        text
    }
}

/// An id the caller sent that init set aside, and why. One source for the text
/// note and the structured block so the two cannot disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScopeRepair {
    reason: &'static str,
    ignored: Vec<(&'static str, String)>,
}

impl ScopeRepair {
    /// The caller sent placeholder ids. `None` when it sent none.
    pub(crate) fn placeholders(workspace: &ScopeIdArg, project: &ScopeIdArg) -> Option<Self> {
        let ignored: Vec<(&'static str, String)> =
            [("workspace_id", workspace), ("project_id", project)]
                .into_iter()
                .filter_map(|(name, arg)| arg.placeholder().map(|text| (name, text.to_string())))
                .collect();
        (!ignored.is_empty()).then_some(Self {
            reason: "placeholder_ids",
            ignored,
        })
    }

    /// The caller named a workspace that does not exist.
    pub(crate) fn workspace_not_found(workspace_id: Uuid, project_id: Option<Uuid>) -> Self {
        let mut ignored = vec![("workspace_id", workspace_id.to_string())];
        if let Some(project_id) = project_id {
            ignored.push(("project_id", project_id.to_string()));
        }
        Self {
            reason: "workspace_not_found",
            ignored,
        }
    }

    /// The caller named a project that is not in the workspace.
    pub(crate) fn project_not_found(project_id: Uuid) -> Self {
        Self {
            reason: "project_not_found",
            ignored: vec![("project_id", project_id.to_string())],
        }
    }

    fn names(&self) -> String {
        self.ignored
            .iter()
            .map(|(name, value)| format!("{name} `{value}`"))
            .collect::<Vec<_>>()
            .join(" and ")
    }

    /// Text for the init response.
    pub(crate) fn note(&self) -> String {
        let names = self.names();
        match self.reason {
            "placeholder_ids" => format!(
                "Ignored {names}: not a real id, so scope was resolved from the folder and repository instead. Use the workspace_id and project_id returned by this call."
            ),
            "workspace_not_found" => format!(
                "Ignored {names}: that workspace does not exist or was deleted, so scope was resolved from the folder and repository instead. Use the workspace_id and project_id returned by this call."
            ),
            _ => format!(
                "Ignored {names}: not found in this workspace, so the project was resolved from the folder instead. Use the project_id returned by this call."
            ),
        }
    }

    /// Structured form, for clients that show only `structuredContent`.
    pub(crate) fn to_value(&self) -> Value {
        let ignored: serde_json::Map<String, Value> = self
            .ignored
            .iter()
            .map(|(name, value)| ((*name).to_string(), Value::String(value.clone())))
            .collect();
        json!({
            "reason": self.reason,
            "ignored": ignored,
            "note": self.note(),
        })
    }
}

/// Put `repair` on a finished init result: first in the text, and first in the
/// structured `scope_repairs` list when the result carries structured content.
pub(crate) fn with_scope_repair(mut result: ToolResult, repair: &ScopeRepair) -> ToolResult {
    let note = repair.note();
    let first_text = result
        .content
        .iter()
        .position(|item| matches!(item, ContentItem::Text { .. }));
    match first_text {
        Some(index) => {
            if let ContentItem::Text { text } = &mut result.content[index] {
                *text = format!("{note}\n\n{text}");
            }
        }
        None => result.content.insert(0, ContentItem::text(note)),
    }
    if let Some(object) = result
        .structured_content
        .as_mut()
        .and_then(Value::as_object_mut)
    {
        let repairs = object
            .entry(SCOPE_REPAIRS_KEY)
            .or_insert_with(|| Value::Array(Vec::new()));
        if let Value::Array(repairs) = repairs {
            repairs.insert(0, repair.to_value());
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(name: &str) -> Project {
        Project {
            id: Uuid::new_v4(),
            name: name.to_string(),
            description: None,
            repository_url: None,
            repository_type: None,
            workspace_id: None,
            path: None,
            created_at: None,
            updated_at: None,
            indexed_at: None,
            file_count: None,
        }
    }

    #[test]
    fn placeholders_are_recognised_in_every_spelling() {
        for value in [
            "",
            "   ",
            "00000000-0000-0000-0000-000000000000",
            "00000000000000000000000000000000",
            "{00000000-0000-0000-0000-000000000000}",
            "urn:uuid:00000000-0000-0000-0000-000000000000",
            " 00000000-0000-0000-0000-000000000000 ",
            "<current_workspace_id>",
            "<id>",
            "{workspace_id}",
            "null",
            "None",
            "undefined",
        ] {
            assert!(is_placeholder_scope_id(value), "{value:?} is a placeholder");
        }
    }

    #[test]
    fn real_and_wrong_ids_are_not_placeholders() {
        for value in [
            "11111111-2222-4333-8444-555555555555",
            "00000000-0000-0000-0000-000000000001",
            "engineering",
            "11111111",
        ] {
            assert!(
                !is_placeholder_scope_id(value),
                "{value:?} is not a placeholder"
            );
        }
    }

    #[test]
    fn classify_separates_absent_placeholder_valid_and_malformed() {
        let real = "11111111-2222-4333-8444-555555555555";
        assert_eq!(classify_scope_id_arg(None), ScopeIdArg::Absent);
        assert_eq!(
            classify_scope_id_arg(Some("<id>")),
            ScopeIdArg::Placeholder("<id>".to_string())
        );
        assert_eq!(
            classify_scope_id_arg(Some(" 00000000-0000-0000-0000-000000000000")),
            ScopeIdArg::Placeholder("00000000-0000-0000-0000-000000000000".to_string())
        );
        assert_eq!(
            classify_scope_id_arg(Some(real)),
            ScopeIdArg::Valid(Uuid::parse_str(real).unwrap())
        );
        assert_eq!(
            classify_scope_id_arg(Some("engineering")),
            ScopeIdArg::Malformed("engineering".to_string())
        );
    }

    #[test]
    fn only_a_real_or_malformed_id_counts_as_explicit() {
        assert!(!ScopeIdArg::Absent.is_explicit());
        assert!(!ScopeIdArg::Placeholder("<id>".into()).is_explicit());
        assert!(ScopeIdArg::Valid(Uuid::new_v4()).is_explicit());
        assert!(ScopeIdArg::Malformed("x".into()).is_explicit());
        assert_eq!(ScopeIdArg::Malformed("x".into()).valid(), None);
    }

    #[test]
    fn strip_removes_placeholders_and_keeps_everything_else() {
        let real = "11111111-2222-4333-8444-555555555555";
        let mut input = json!({
            "action": "list_docs",
            "workspace_id": "00000000-0000-0000-0000-000000000000",
            "project_id": real,
            "query": "<keep me>",
        });
        let removed = strip_placeholder_scope_ids(&mut input);
        assert_eq!(removed, vec!["workspace_id"]);
        assert_eq!(
            input,
            json!({"action": "list_docs", "project_id": real, "query": "<keep me>"})
        );

        let mut both = json!({"workspace_id": "<current_workspace_id>", "project_id": ""});
        assert_eq!(
            strip_placeholder_scope_ids(&mut both),
            vec!["workspace_id", "project_id"]
        );
        assert_eq!(both, json!({}));

        let mut wrong = json!({"workspace_id": "engineering"});
        assert!(strip_placeholder_scope_ids(&mut wrong).is_empty());
        assert_eq!(wrong, json!({"workspace_id": "engineering"}));

        let mut not_an_object = json!("workspace_id");
        assert!(strip_placeholder_scope_ids(&mut not_an_object).is_empty());
    }

    #[test]
    fn similar_names_are_found_best_first_and_unrelated_ones_are_not() {
        let projects = vec![
            project("coflow-web"),
            project("mcp-server"),
            project("contextcode"),
            project("mcp"),
            project("billing"),
        ];
        let found = similar_project_candidates("mcp-server-init-scope", &projects, None, 5);
        let names: Vec<&str> = found.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["mcp-server", "mcp"]);

        assert!(similar_project_candidates("zz-unmatched-probe", &projects, None, 5).is_empty());
    }

    #[test]
    fn generic_words_alone_do_not_make_a_match() {
        let projects = vec![project("web"), project("api-app"), project("src")];
        assert!(similar_project_candidates("my-web-app", &projects, None, 5).is_empty());
    }

    #[test]
    fn the_pinned_project_is_not_offered_and_the_list_is_capped() {
        let pinned = project("mcp-server");
        let mut projects = vec![pinned.clone()];
        for i in 0..8 {
            projects.push(project(&format!("mcp-server-{i}")));
        }
        let found = similar_project_candidates("mcp-server", &projects, Some(pinned.id), 5);
        assert_eq!(found.len(), 5);
        assert!(found.iter().all(|c| c.id != pinned.id));
    }

    #[test]
    fn ties_are_ordered_by_name_so_results_are_stable() {
        let projects = vec![project("alpha-core"), project("beta-core"), project("core")];
        let first = similar_project_candidates("core-tools", &projects, None, 5);
        let second = similar_project_candidates("core-tools", &projects, None, 5);
        assert_eq!(first, second);
    }

    #[test]
    fn the_notice_names_the_pinned_scope_the_candidates_and_the_calls() {
        let workspace_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        let candidate = ProjectCandidate {
            id: Uuid::new_v4(),
            name: "mcp-server".to_string(),
            score: 0.6,
        };
        let candidates = [candidate.clone()];
        let scope = UnmatchedFolderScope {
            folder_path: "/work/mcp-server-wt",
            workspace_id,
            workspace_name: Some("Engineering"),
            project_id,
            project_name: Some("contextcode"),
            candidates: &candidates,
        };

        let text = scope.notice(false);
        assert!(text.starts_with("[SCOPE_RESOLUTION]"), "{text}");
        assert!(text.contains("`mcp-server-wt`"), "{text}");
        assert!(text.contains("Engineering"), "{text}");
        assert!(text.contains("contextcode"), "{text}");
        assert!(text.contains(&candidate.id.to_string()), "{text}");
        assert!(
            text.contains(&format!(
                "init(folder_path=\"/work/mcp-server-wt\", workspace_id=\"{workspace_id}\", project_id=\"{}\")",
                candidate.id
            )),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "project(action=\"create\", workspace_id=\"{workspace_id}\", name=\"mcp-server-wt\")"
            )),
            "{text}"
        );

        let compact = scope.notice(true);
        assert!(compact.len() < text.len());
        assert!(!compact.contains("Carry on"), "{compact}");

        let value = scope.to_value();
        assert_eq!(value["status"], "inherited_unmatched_folder");
        assert_eq!(value["using"]["project_name"], "contextcode");
        assert_eq!(value["candidates"][0]["name"], "mcp-server");
        assert!(value["next_steps"]["create"]
            .as_str()
            .unwrap()
            .contains("mcp-server-wt"));
    }

    #[test]
    fn the_notice_without_candidates_still_offers_creation() {
        let scope = UnmatchedFolderScope {
            folder_path: "/tmp/zz",
            workspace_id: Uuid::new_v4(),
            workspace_name: None,
            project_id: Uuid::new_v4(),
            project_name: None,
            candidates: &[],
        };
        let text = scope.notice(false);
        assert!(text.contains("(unnamed)"), "{text}");
        assert!(text.contains("project(action=\"create\""), "{text}");
        assert_eq!(scope.to_value()["candidates"], json!([]));
    }

    #[test]
    fn placeholder_repairs_name_only_the_placeholders_that_were_sent() {
        let nil = "00000000-0000-0000-0000-000000000000";
        let real = ScopeIdArg::Valid(Uuid::new_v4());

        assert_eq!(
            ScopeRepair::placeholders(&ScopeIdArg::Absent, &ScopeIdArg::Absent),
            None
        );
        assert_eq!(ScopeRepair::placeholders(&real, &ScopeIdArg::Absent), None);

        let repair = ScopeRepair::placeholders(
            &ScopeIdArg::Placeholder(nil.to_string()),
            &ScopeIdArg::Placeholder("<id>".to_string()),
        )
        .expect("both were placeholders");
        let value = repair.to_value();
        assert_eq!(value["reason"], "placeholder_ids");
        assert_eq!(value["ignored"]["workspace_id"], nil);
        assert_eq!(value["ignored"]["project_id"], "<id>");
        assert!(repair.note().contains(&format!("workspace_id `{nil}`")));
        assert!(repair.note().contains("project_id `<id>`"));

        let only_workspace =
            ScopeRepair::placeholders(&ScopeIdArg::Placeholder(nil.to_string()), &real).unwrap();
        assert!(only_workspace.to_value()["ignored"]
            .get("project_id")
            .is_none());
    }

    #[test]
    fn a_missing_workspace_is_reported_with_the_project_that_went_with_it() {
        let workspace_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        let repair = ScopeRepair::workspace_not_found(workspace_id, Some(project_id));
        let value = repair.to_value();
        assert_eq!(value["reason"], "workspace_not_found");
        assert_eq!(value["ignored"]["workspace_id"], workspace_id.to_string());
        assert_eq!(value["ignored"]["project_id"], project_id.to_string());
        assert!(repair.note().contains("does not exist"));
        assert!(repair.note().contains("returned by this call"));
        assert_eq!(value["note"], repair.note());

        let without_project = ScopeRepair::workspace_not_found(workspace_id, None);
        assert!(without_project.to_value()["ignored"]
            .get("project_id")
            .is_none());
    }

    #[test]
    fn a_repair_goes_first_in_the_text_and_the_structured_list() {
        let repair = ScopeRepair::workspace_not_found(Uuid::new_v4(), None);

        let mut result = ToolResult::text("Session ready for workspace: Engineering");
        result.structured_content =
            Some(json!({"workspace_id": "w", "scope_repairs": [{"reason": "placeholder_ids"}]}));
        let result = with_scope_repair(result, &repair);
        let ContentItem::Text { text } = &result.content[0] else {
            panic!("text first");
        };
        assert!(text.starts_with("Ignored workspace_id"), "{text}");
        assert!(
            text.ends_with("Session ready for workspace: Engineering"),
            "{text}"
        );
        let repairs = result.structured_content.as_ref().unwrap()["scope_repairs"]
            .as_array()
            .unwrap();
        assert_eq!(repairs.len(), 2);
        assert_eq!(repairs[0]["reason"], "workspace_not_found");
        assert_eq!(repairs[1]["reason"], "placeholder_ids");

        // No structured content (disabled for the client): the text still says it.
        let plain = with_scope_repair(ToolResult::text("ok"), &repair);
        assert!(plain.structured_content.is_none());
        let ContentItem::Text { text } = &plain.content[0] else {
            panic!("text first");
        };
        assert!(text.contains("does not exist"), "{text}");

        // A result with no text item gets one rather than losing the note.
        let mut empty = ToolResult::text("x");
        empty.content.clear();
        assert!(matches!(
            with_scope_repair(empty, &repair).content[0],
            ContentItem::Text { .. }
        ));
    }

    #[test]
    fn a_missing_project_is_reported_on_its_own() {
        let project_id = Uuid::new_v4();
        let repair = ScopeRepair::project_not_found(project_id);
        assert_eq!(repair.to_value()["reason"], "project_not_found");
        assert!(repair.note().contains(&project_id.to_string()));
        assert!(repair.note().contains("not found in this workspace"));
    }
}
