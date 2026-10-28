//! Which ContextAgents are on in a workspace, and choosing them: the `settings`, `enable`,
//! `disable` and `choose` actions.
//!
//! - `settings`: a checklist grouped by the kind of work, with how many are on, what each agent
//!   does, what it may access in plain words, which need review, who can change it, and why
//!   nothing can run when that is so.
//! - `enable` / `disable`: an agent id, a list (`agents`), or `all` (`none` too). Only the
//!   agents named change; the rest stay as they are.
//! - `choose`: a form the client shows the person (MCP elicitation) with every agent, those on
//!   checked: a multi-select where the client's protocol has one (2025-11-25 and later), one
//!   checkbox per agent before that. A client that cannot show forms gets the checklist and
//!   the call that sets it. With `agents` it sets exactly that set, without a form.
//!
//! The settings come from `GET /workspaces/{id}/tasks/agents/consent` (the agents with their
//! access in words, the caller's role, the owners and admins, and whether runs are open) and
//! change through `PUT` on the same path (`{"enable": "all" | "none" | [ids]}`: the exact set
//! to have on, in one transaction). Against an API without those routes (`404`), the catalog
//! (`GET /tasks/agents`), the workspace's members and one `PUT .../agents/{agent}/consent` per
//! agent do the same.
//!
//! Only a workspace owner or admin changes it; anyone else gets a read-only answer that names
//! who can. New access an agent asks for is never granted by turning agents on together: an
//! agent already on stays as it is, and only `enable` naming an agent that needs review grants
//! what it now asks for (`PUT .../agents/{agent}/consent {"enabled": true}`).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use futures::stream::{self, StreamExt};
use mcp_types::{tool::ToolResult, Error, Result};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::catalog::{agent_id_of, AccessItem, AccessKind, AgentEntry, Catalog};
use super::describe::shorten;
use super::gate::Availability;
use super::{is_agent_id, unwrap_data, SlipstreamInput, SlipstreamTool, TOOL_NAME};
use crate::peer::{self, Elicited};

/// How long the workspace's name and members may take in the fallback; past it they are left
/// out.
const WORKSPACE_INFO_BUDGET: Duration = Duration::from_secs(4);
/// How long the person has to answer the form.
const CHOOSE_TIMEOUT: Duration = Duration::from_secs(300);
/// Per-agent consent changes sent at once.
const PUT_CONCURRENCY: usize = 4;
/// The most people a read-only answer names.
const MAX_MANAGERS_NAMED: usize = 3;
/// Characters of an agent's job in the checklist.
const JOB_CHARS: usize = 90;
/// Characters of an agent's job in a form's option.
const FORM_JOB_CHARS: usize = 56;
/// The form's answer key, and its multi-select field.
const FORM_KEY: &str = "agents";
const FORM_FIELD: &str = "enabled";

fn settings_path(workspace_id: Uuid) -> String {
    format!("/workspaces/{workspace_id}/tasks/agents/consent")
}

// ---------------------------------------------------------------------------------------
// Groups and access, in the words the API uses (contextstream `consent_settings`)
// ---------------------------------------------------------------------------------------

/// The groups agents are shown in, in display order, when the API does not send them.
const CATEGORIES: [(&str, &str); 5] = [
    ("code", "Code and repositories"),
    ("files", "Files and data"),
    ("work", "Plans and memory"),
    ("research", "Mail and research"),
    ("other", "More agents"),
];

/// An agent's group id: the API's, else the known agents by id, any other by what it reads.
pub(crate) fn category_of(agent: &AgentEntry) -> String {
    if let Some(category) = &agent.category {
        return category.clone();
    }
    let id = match agent.id.as_str() {
        "review"
        | "ci-fix"
        | "dependency-upgrader"
        | "audit"
        | "docs-drift"
        | "guide"
        | "oracle"
        | "release-notes" => "code",
        "lens" | "pdf" | "analyst" => "files",
        "keeper" | "gardener" | "daily-briefing" => "work",
        "mail" | "research-brief" | "site-audit" => "research",
        _ => {
            let has = |prefix: &str| agent.scopes.iter().any(|scope| scope.starts_with(prefix));
            if has("vcs.") || has("search.") {
                "code"
            } else if has("mail.") {
                "research"
            } else if has("media.") || has("docs.") {
                "files"
            } else if has("items.") || has("memory.") {
                "work"
            } else {
                "other"
            }
        }
    };
    id.to_string()
}

/// A group's label and place: the API's list when it sent one, else [`CATEGORIES`].
fn category_label_rank(catalog: &Catalog, id: &str) -> (String, usize) {
    if !catalog.categories.is_empty() {
        if let Some(index) = catalog.categories.iter().position(|(known, _)| known == id) {
            return (catalog.categories[index].1.clone(), index);
        }
    }
    match CATEGORIES.iter().position(|(known, _)| *known == id) {
        Some(index) => (CATEGORIES[index].1.to_string(), 100 + index),
        None => (id.to_string(), 200),
    }
}

/// The agents by group (label first), groups in display order, agents by name within each.
pub(crate) fn grouped(catalog: &Catalog) -> Vec<(String, Vec<&AgentEntry>)> {
    let mut groups: Vec<(String, usize, Vec<&AgentEntry>)> = Vec::new();
    for agent in &catalog.agents {
        let (label, rank) = category_label_rank(catalog, &category_of(agent));
        match groups.iter_mut().find(|(known, _, _)| *known == label) {
            Some((_, _, agents)) => agents.push(agent),
            None => groups.push((label, rank, vec![agent])),
        }
    }
    groups.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    groups
        .into_iter()
        .map(|(label, _, mut agents)| {
            agents.sort_by(|a, b| {
                a.name
                    .to_lowercase()
                    .cmp(&b.name.to_lowercase())
                    .then_with(|| a.id.cmp(&b.id))
            });
            (label, agents)
        })
        .collect()
}

/// A scope's chip words and kind, as the API words them.
fn scope_words(scope: &str) -> Option<(&'static str, AccessKind)> {
    use AccessKind::{Models, Read, Write};
    Some(match scope {
        "media.read" => ("Reads images you share", Read),
        "search.read" => ("Searches code", Read),
        "memory.read" => ("Reads decisions and lessons", Read),
        "memory.write.note" => ("Saves one note", Write),
        "task.report" => ("Reports its progress", Read),
        "context.read" => ("Reads task context", Read),
        "context.feedback" => ("Marks context it used", Write),
        "model.invoke" => ("Uses ContextStream models", Models),
        "docs.read" => ("Reads a doc you name", Read),
        "media.write" => ("Saves one file", Write),
        "items.read" => ("Reads plans and tasks", Read),
        "memory.propose" => ("Proposes memory changes", Write),
        "vcs.read" => ("Reads repositories", Read),
        "docs.write" => ("Saves one doc", Write),
        "mail.read" => ("Reads your mail", Read),
        "mail.draft.write" => ("Drafts replies, never sends", Write),
        _ => return None,
    })
}

/// Each scope an agent asks for, in words: the API's, or worded here from the catalog.
pub(crate) fn access_items(agent: &AgentEntry) -> Vec<AccessItem> {
    if !agent.access.is_empty() {
        return agent.access.clone();
    }
    let new_scopes = agent.new_scopes();
    agent
        .scopes
        .iter()
        .map(|scope| {
            let (label, kind) = scope_words(scope).unwrap_or((scope.as_str(), AccessKind::Write));
            // A catalog that does not say what runs hold: an agent on holds what it asks for.
            let held = agent.effective_scopes.contains(scope)
                || (agent.effective_scopes.is_empty() && !agent.needs_review);
            let new = new_scopes.contains(&scope.as_str());
            AccessItem {
                scope: scope.clone(),
                label: label.to_string(),
                description: agent
                    .scope_descriptions
                    .iter()
                    .find(|(name, _)| name == scope)
                    .map(|(_, description)| description.clone())
                    .unwrap_or_default(),
                kind,
                granted: agent.enabled && held,
                new,
            }
        })
        .collect()
}

fn lower_first(text: &str) -> String {
    let mut chars = text.trim().chars();
    match chars.next() {
        Some(first) => first.to_lowercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// What an agent's runs may read and change: for one that is on, what its runs hold now; for
/// one that is off, what it gets when turned on. Model use (every agent's) is left out.
pub(crate) fn access_parts(agent: &AgentEntry) -> (Vec<String>, Vec<String>) {
    let mut reads = Vec::new();
    let mut changes = Vec::new();
    for item in access_items(agent) {
        let shown = if agent.enabled {
            item.granted
        } else {
            !item.new
        };
        if !shown {
            continue;
        }
        match item.kind {
            AccessKind::Read => reads.push(item.label),
            AccessKind::Write => changes.push(item.label),
            AccessKind::Models => {}
        }
    }
    (reads, changes)
}

/// `searches code, reads task context; saves one note`, or `...; changes nothing`.
pub(crate) fn access_line(agent: &AgentEntry) -> String {
    let (reads, changes) = access_parts(agent);
    let words = |labels: &[String]| -> String {
        labels
            .iter()
            .map(|label| lower_first(label))
            .collect::<Vec<_>>()
            .join(", ")
    };
    match (reads.is_empty(), changes.is_empty()) {
        (false, false) => format!("{}; {}", words(&reads), words(&changes)),
        (false, true) => format!("{}; changes nothing", words(&reads)),
        (true, false) => words(&changes),
        (true, true) => "changes nothing".to_string(),
    }
}

/// The new access a needs-review agent asks for, as the consent sentences say it.
pub(crate) fn new_access(agent: &AgentEntry) -> Vec<String> {
    access_items(agent)
        .into_iter()
        .filter(|item| item.new && item.kind != AccessKind::Models)
        .map(|item| {
            let words = if item.description.is_empty() {
                item.label
            } else {
                item.description
            };
            lower_first(words.trim_end_matches('.'))
        })
        .collect()
}

fn price_label(agent: &AgentEntry) -> String {
    match agent.price_credits {
        Some(price) if price > 0 => format!("{price} credits"),
        Some(_) => "free".to_string(),
        None => "metered".to_string(),
    }
}

/// The agent's job in one line: the API's, else its description's first sentence.
fn job_of(agent: &AgentEntry) -> String {
    agent
        .job_line
        .clone()
        .unwrap_or_else(|| agent.job())
        .trim_end_matches('.')
        .to_string()
}

// ---------------------------------------------------------------------------------------
// The workspace, and who may change it
// ---------------------------------------------------------------------------------------

/// Whether the caller may change which agents are on, and who can.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Manage {
    /// `None` when it could not be read.
    pub can_change: Option<bool>,
    /// The caller's role here: owner, admin, member or viewer.
    pub role: Option<String>,
    /// Who can change it: `erik (owner)`, `Ana Lee (admin)`.
    pub managers: Vec<String>,
}

impl Manage {
    /// Who can change it, for a sentence: `erik (owner) or Ana Lee (admin)`.
    pub fn who(&self) -> String {
        let named: Vec<&str> = self
            .managers
            .iter()
            .take(MAX_MANAGERS_NAMED)
            .map(String::as_str)
            .collect();
        let more = self.managers.len().saturating_sub(MAX_MANAGERS_NAMED);
        match (named.as_slice(), more) {
            ([], _) => "the workspace owner or an admin".to_string(),
            ([one], 0) => one.to_string(),
            (names, 0) => format!(
                "{} or {}",
                names[..names.len() - 1].join(", "),
                names[names.len() - 1]
            ),
            (names, more) => format!("{} or {more} other admin(s)", names.join(", ")),
        }
    }

    fn you_are(&self) -> String {
        match self.role.as_deref() {
            Some(role @ ("admin" | "owner")) => format!(" (you're an {role} here)"),
            Some(role) => format!(" (you're a {role} here)"),
            None => String::new(),
        }
    }

    /// The line under the checklist's heading.
    pub fn summary_line(&self) -> String {
        match (self.can_change, self.role.as_deref()) {
            (Some(true), Some("owner")) => {
                "You can change them: you own this workspace.".to_string()
            }
            (Some(true), Some(role)) => format!("You can change them: you're an {role} here."),
            (Some(true), None) => "You can change them.".to_string(),
            (Some(false), _) => format!(
                "Read-only for you{}: {} can change them.",
                self.you_are(),
                self.who()
            ),
            (None, _) => "The workspace owner or an admin can change them.".to_string(),
        }
    }
}

/// The workspace's name, who manages it, and whether its agents can run, as far as they
/// could be read.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct WorkspaceInfo {
    pub name: Option<String>,
    pub manage: Manage,
    /// Whether the workspace may run agents at all (the rollout); `None` when unknown.
    pub runs_open: Option<bool>,
    /// Why nothing can run now, in the API's words.
    pub runs_blocked: Option<String>,
}

fn text_at<'a>(value: &'a Value, pointer: &str) -> Option<&'a str> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

impl WorkspaceInfo {
    /// From the settings view and `GET /workspaces/{id}` (for the name).
    pub fn from_view(view: &Value, workspace: Option<&Value>) -> Self {
        let role = text_at(view, "/role").map(str::to_lowercase);
        let can_change = view
            .get("can_manage")
            .and_then(Value::as_bool)
            .or_else(|| role.as_deref().map(|r| r == "owner" || r == "admin"));
        let managers = view
            .get("managers")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        let name = text_at(item, "/name")?;
                        Some(match text_at(item, "/role") {
                            Some(role) => format!("{name} ({})", role.to_lowercase()),
                            None => name.to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Self {
            name: workspace
                .and_then(|w| text_at(w, "/name"))
                .map(str::to_string),
            manage: Manage {
                can_change,
                role,
                managers,
            },
            runs_open: view.pointer("/runs/open").and_then(Value::as_bool),
            runs_blocked: text_at(view, "/runs/blocked/message").map(str::to_string),
        }
    }

    /// From `GET /workspaces/{id}`, `GET /auth/me` and `GET /workspaces/{id}/members`, any of
    /// which may be missing: an API without the settings view.
    pub fn from_reads(
        workspace: Option<&Value>,
        me: Option<&Value>,
        members: Option<&Value>,
    ) -> Self {
        let name = workspace
            .and_then(|w| text_at(w, "/name"))
            .map(str::to_string);
        let owner = workspace
            .and_then(|w| text_at(w, "/owner_username"))
            .map(str::to_string);
        let my_id = me
            .and_then(|m| text_at(m, "/id"))
            .and_then(|id| Uuid::parse_str(id).ok());
        let items = members
            .and_then(|m| m.get("items"))
            .and_then(Value::as_array);

        let role_of = |item: &Value| text_at(item, "/role").map(str::to_lowercase);
        let role = match (my_id, items) {
            (Some(me), Some(items)) => {
                let mine = items.iter().find(|item| {
                    text_at(item, "/user/id").and_then(|id| Uuid::parse_str(id).ok()) == Some(me)
                });
                let complete = members
                    .and_then(|m| m.get("total"))
                    .and_then(Value::as_u64)
                    .is_none_or(|total| total as usize <= items.len());
                match mine {
                    Some(item) => role_of(item),
                    // A person who reads the catalog but is not among the members owns the
                    // workspace (its owner is not a member row).
                    None if complete => Some("owner".to_string()),
                    None => None,
                }
            }
            _ => None,
        };

        let mut managers = Vec::new();
        if let Some(owner) = &owner {
            managers.push(format!("{owner} (owner)"));
        }
        for item in items.into_iter().flatten() {
            let Some(role) = role_of(item).filter(|r| r == "owner" || r == "admin") else {
                continue;
            };
            if text_at(item, "/status").is_some_and(|status| status != "active") {
                continue;
            }
            if role == "owner" && owner.as_deref() == text_at(item, "/user/username") {
                continue;
            }
            let Some(person) = text_at(item, "/user/full_name")
                .or_else(|| text_at(item, "/user/username"))
                .or_else(|| text_at(item, "/user/email"))
            else {
                continue;
            };
            let entry = format!("{person} ({role})");
            if !managers.contains(&entry) {
                managers.push(entry);
            }
        }
        Self {
            name,
            manage: Manage {
                can_change: role.as_deref().map(|r| r == "owner" || r == "admin"),
                role,
                managers,
            },
            runs_open: None,
            runs_blocked: None,
        }
    }

    /// `in Demos`, or `in this workspace`.
    pub fn where_label(&self) -> String {
        match &self.name {
            Some(name) => format!("in {name}"),
            None => "in this workspace".to_string(),
        }
    }
}

/// The settings as read: the agents, the workspace, and whether the settings routes (the
/// view and the bulk write) answered.
#[derive(Debug, Clone)]
pub(crate) struct Loaded {
    pub catalog: Arc<Catalog>,
    pub info: WorkspaceInfo,
    pub bulk: bool,
}

// ---------------------------------------------------------------------------------------
// The checklist
// ---------------------------------------------------------------------------------------

fn on_count(catalog: &Catalog) -> usize {
    catalog.agents.iter().filter(|agent| agent.enabled).count()
}

fn review_count(catalog: &Catalog) -> usize {
    catalog
        .agents
        .iter()
        .filter(|agent| agent.enabled && agent.needs_review)
        .count()
}

/// The way to change the settings, for someone who can.
fn how_to_change() -> String {
    format!(
        "Change: {TOOL_NAME}(action=\"choose\") asks the person in a form; or enable / disable with agent=\"<id>\" or \"all\", or agents=[\"<id>\", ...]."
    )
}

/// `settings`: every agent by group, with what it does and may access.
pub(crate) fn settings_text(catalog: &Catalog, info: &WorkspaceInfo) -> String {
    let place = info.where_label();
    if catalog.agents.is_empty() {
        return format!("No ContextAgents are available {place} yet.");
    }
    let total = catalog.agents.len();
    let review = review_count(catalog);
    let mut out = format!("ContextAgents {place}: {} of {total} on", on_count(catalog));
    if review > 0 {
        out.push_str(&format!(
            ", {review} need{} review",
            if review == 1 { "s" } else { "" }
        ));
    }
    out.push_str(".\n");
    out.push_str(&info.manage.summary_line());
    out.push('\n');
    if let Some(blocked) = &info.runs_blocked {
        out.push_str(blocked);
        out.push('\n');
    }
    for (group, agents) in grouped(catalog) {
        let on = agents.iter().filter(|agent| agent.enabled).count();
        out.push_str(&format!("\n{group} · {on} of {} on\n", agents.len()));
        for agent in agents {
            let mark = if agent.enabled { "[x]" } else { "[ ]" };
            out.push_str(&format!(
                "  {mark} {}  {} ({}): {}\n",
                agent.id,
                agent.name,
                price_label(agent),
                shorten(&job_of(agent), JOB_CHARS)
            ));
            out.push_str(&format!("      {}\n", access_line(agent)));
            if agent.enabled && agent.needs_review {
                let asks = new_access(agent);
                out.push_str(&format!(
                    "      needs review: it also asks to {}; enable {} to grant that\n",
                    if asks.is_empty() {
                        "use new scopes".to_string()
                    } else {
                        asks.join("; ")
                    },
                    agent.id
                ));
            }
        }
    }
    out.push('\n');
    match info.manage.can_change {
        Some(false) => out.push_str(&format!("To change them, ask {}.", info.manage.who())),
        _ => out.push_str(&how_to_change()),
    }
    out
}

/// `settings` for a program: the same, as data.
pub(crate) fn settings_json(workspace_id: Uuid, catalog: &Catalog, info: &WorkspaceInfo) -> Value {
    let agents: Vec<Value> = grouped(catalog)
        .into_iter()
        .flat_map(|(group, agents)| {
            agents.into_iter().map(move |agent| {
                let (reads, changes) = access_parts(agent);
                json!({
                    "id": agent.id,
                    "name": agent.name,
                    "group": group,
                    "enabled": agent.enabled,
                    "needs_review": agent.enabled && agent.needs_review,
                    "price_credits": agent.price_credits,
                    "job": job_of(agent),
                    "reads": reads,
                    "changes": changes,
                    "new_access": new_access(agent),
                })
            })
        })
        .collect();
    json!({
        "workspace_id": workspace_id,
        "workspace_name": info.name,
        "on": on_count(catalog),
        "total": catalog.agents.len(),
        "needs_review": review_count(catalog),
        "can_change": info.manage.can_change,
        "role": info.manage.role,
        "managers": info.manage.managers,
        "runs": {"open": info.runs_open, "blocked": info.runs_blocked},
        "agents": agents,
    })
}

/// A workspace whose agents were refused: not open to agents yet, or not the caller's.
pub(crate) fn closed_result(workspace_id: Uuid, info: &WorkspaceInfo, message: &str) -> ToolResult {
    let place = info.where_label();
    let not_open = message.to_lowercase().contains("not available");
    let text = if not_open {
        format!(
            "ContextAgents aren't open {place} yet, so there is nothing to choose and nothing was changed. Once they open, settings shows each agent and who can turn it on."
        )
    } else {
        format!("ContextAgents can't be read {place}: {message}. Nothing was changed.")
    };
    ToolResult::with_structured(
        text,
        json!({"workspace_id": workspace_id, "workspace_name": info.name, "open": false,
               "reason": if not_open { "not_open_yet" } else { "refused" }, "message": message}),
    )
}

/// What a person without the right gets when they try to change the settings.
pub(crate) fn read_only_result(
    workspace_id: Uuid,
    info: &WorkspaceInfo,
    wanted: &str,
) -> ToolResult {
    let manage = &info.manage;
    let text = format!(
        "Nothing changed: only {} can choose which ContextAgents are on {}{}. Ask them{wanted}. {TOOL_NAME}(action=\"settings\") shows what is on now.",
        manage.who(),
        info.where_label(),
        manage.you_are(),
    );
    ToolResult::with_structured(
        text,
        json!({"workspace_id": workspace_id, "changed": false, "can_change": false,
               "role": manage.role, "managers": manage.managers}),
    )
}

// ---------------------------------------------------------------------------------------
// What to change
// ---------------------------------------------------------------------------------------

/// The agents a call names.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Target {
    All,
    None,
    Some(Vec<String>),
}

/// Read `agent` and `agents`: an id, a name, `id@version`, a comma-separated list or an
/// array of them, or `all` / `none` on their own.
pub(crate) fn target_of(
    agent: Option<&str>,
    agents: Option<&Value>,
) -> std::result::Result<Target, String> {
    let mut words: Vec<String> = Vec::new();
    let mut add = |text: &str| {
        for part in text.split([',', '\n', ';']) {
            let part = part.trim();
            if !part.is_empty() {
                words.push(part.to_string());
            }
        }
    };
    if let Some(agent) = agent {
        add(agent);
    }
    match agents {
        None | Some(Value::Null) => {}
        Some(Value::String(text)) => add(text),
        Some(Value::Array(items)) => {
            for item in items {
                match item.as_str() {
                    Some(text) => add(text),
                    None => return Err("agents is a list of agent ids".to_string()),
                }
            }
        }
        Some(_) => return Err("agents is a list of agent ids".to_string()),
    }
    if words.is_empty() {
        return Err(
            "Name the agents: agent=\"<id>\", agents=[\"<id>\", ...], or \"all\"".to_string(),
        );
    }
    let special: Vec<&str> = words
        .iter()
        .map(|word| word.to_lowercase())
        .filter_map(|word| match word.as_str() {
            "all" | "*" | "every" | "everything" => Some("all"),
            "none" | "nothing" => Some("none"),
            _ => None,
        })
        .collect();
    match (special.as_slice(), words.len()) {
        ([], _) => {
            let mut seen = HashSet::new();
            words.retain(|word| seen.insert(word.to_lowercase()));
            Ok(Target::Some(words))
        }
        (["all"], 1) => Ok(Target::All),
        (["none"], 1) => Ok(Target::None),
        _ => Err("\"all\" and \"none\" stand on their own, not in a list".to_string()),
    }
}

/// The catalog entries `names` mean: by id, `id@version` or name, ignoring case.
pub(crate) fn resolve<'a>(
    catalog: &'a Catalog,
    names: &[String],
) -> std::result::Result<Vec<&'a AgentEntry>, String> {
    let mut found: Vec<&AgentEntry> = Vec::new();
    let mut unknown = Vec::new();
    for name in names {
        let lowered = name.to_lowercase();
        let id = agent_id_of(&lowered);
        let entry = catalog
            .agents
            .iter()
            .find(|agent| agent.id == id)
            .or_else(|| {
                catalog
                    .agents
                    .iter()
                    .find(|agent| agent.name.to_lowercase() == lowered)
            });
        match entry {
            Some(entry) if !found.iter().any(|f| f.id == entry.id) => found.push(entry),
            Some(_) => {}
            None => unknown.push(name.as_str()),
        }
    }
    if unknown.is_empty() {
        return Ok(found);
    }
    let ids: Vec<&str> = catalog
        .agents
        .iter()
        .map(|agent| agent.id.as_str())
        .collect();
    Err(format!(
        "No agent {} here. The agents: {}.",
        unknown.join(", "),
        ids.join(", ")
    ))
}

/// What a call asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Verb {
    Enable,
    Disable,
    Choose,
}

/// One agent turned on or off.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Change {
    pub id: String,
    pub name: String,
    pub on: bool,
    /// The new access turning it on grants (it needed review).
    pub grants: Vec<String>,
}

/// What to send, and what is already so.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Plan {
    pub changes: Vec<Change>,
    pub already_on: Vec<String>,
    pub already_off: Vec<String>,
    /// On and needing review, left as they are: only `enable` naming one grants its new
    /// access.
    pub still_review: Vec<String>,
    pub note: Option<String>,
    /// The whole set, when the call asks for it: `Some(true)` every agent on, `Some(false)`
    /// none. Otherwise only the changes are meant, and the rest stay as they are.
    pub whole: Option<bool>,
}

impl Plan {
    /// The bulk write's `enable`: `"all"`, `"none"`, or the exact set to have on (what is on
    /// now with this plan's changes made).
    pub fn bulk_enable(&self, catalog: &Catalog) -> Value {
        match self.whole {
            Some(true) => json!("all"),
            Some(false) => json!("none"),
            None => {
                let set: Vec<&str> = catalog
                    .agents
                    .iter()
                    .filter(|agent| {
                        match self.changes.iter().find(|change| change.id == agent.id) {
                            Some(change) => change.on,
                            None => agent.enabled,
                        }
                    })
                    .map(|agent| agent.id.as_str())
                    .collect();
                json!(set)
            }
        }
    }
}

fn turn(agent: &AgentEntry, on: bool) -> Change {
    Change {
        id: agent.id.clone(),
        name: agent.name.clone(),
        on,
        grants: if on && agent.enabled {
            new_access(agent)
        } else {
            Vec::new()
        },
    }
}

/// What makes `verb` on `target` true. Agents already as asked are left alone.
pub(crate) fn plan(
    catalog: &Catalog,
    verb: Verb,
    target: &Target,
) -> std::result::Result<Plan, String> {
    let mut plan = Plan::default();
    let all: Vec<&AgentEntry> = catalog.agents.iter().collect();
    let all_on = |plan: &mut Plan| {
        for agent in &all {
            if !agent.enabled {
                plan.changes.push(turn(agent, true));
            } else if agent.needs_review {
                plan.still_review.push(agent.id.clone());
            }
        }
    };
    let all_off = |plan: &mut Plan| {
        for agent in &all {
            if agent.enabled {
                plan.changes.push(turn(agent, false));
            }
        }
    };
    match (verb, target) {
        (Verb::Enable | Verb::Choose, Target::All) => {
            all_on(&mut plan);
            plan.whole = Some(true);
        }
        (Verb::Choose, Target::None) | (Verb::Disable, Target::All) => {
            all_off(&mut plan);
            plan.whole = Some(false);
        }
        (Verb::Enable, Target::None) => {
            all_off(&mut plan);
            plan.whole = Some(false);
            plan.note = Some("enable with none turns every agent off.".to_string());
        }
        // None disabled: every agent on (as ContextCode's `csa disable none`).
        (Verb::Disable, Target::None) => {
            all_on(&mut plan);
            plan.whole = Some(true);
            plan.note = Some("disable with none turns every agent on.".to_string());
        }
        (Verb::Enable, Target::Some(names)) => {
            for agent in resolve(catalog, names)? {
                if !agent.enabled || agent.needs_review {
                    plan.changes.push(turn(agent, true));
                } else {
                    plan.already_on.push(agent.id.clone());
                }
            }
        }
        (Verb::Disable, Target::Some(names)) => {
            for agent in resolve(catalog, names)? {
                if agent.enabled {
                    plan.changes.push(turn(agent, false));
                } else {
                    plan.already_off.push(agent.id.clone());
                }
            }
        }
        (Verb::Choose, Target::Some(names)) => {
            let wanted: HashSet<&str> = resolve(catalog, names)?
                .into_iter()
                .map(|agent| agent.id.as_str())
                .collect();
            for agent in &all {
                match (wanted.contains(agent.id.as_str()), agent.enabled) {
                    (true, false) => plan.changes.push(turn(agent, true)),
                    (false, true) => plan.changes.push(turn(agent, false)),
                    (true, true) if agent.needs_review => plan.still_review.push(agent.id.clone()),
                    _ => {}
                }
            }
        }
    }
    Ok(plan)
}

// ---------------------------------------------------------------------------------------
// The form
// ---------------------------------------------------------------------------------------

/// The agents in the order the form and checklist show them.
fn ordered(catalog: &Catalog) -> Vec<&AgentEntry> {
    grouped(catalog)
        .into_iter()
        .flat_map(|(_, agents)| agents)
        .collect()
}

/// What the form asks.
pub(crate) fn form_message(catalog: &Catalog, info: &WorkspaceInfo) -> String {
    format!(
        "Choose the ContextAgents on {}. {} of {} are on now. An agent runs only when someone asks it, and a run is charged only when it succeeds.",
        info.where_label(),
        on_count(catalog),
        catalog.agents.len()
    )
}

/// The form: one multi-select of every agent with those on checked, or, for a client whose
/// protocol has no multi-select, one checkbox per agent.
pub(crate) fn choose_form(catalog: &Catalog, multi_select: bool) -> Value {
    let agents = ordered(catalog);
    if multi_select {
        let options: Vec<Value> = agents
            .iter()
            .map(|agent| {
                json!({
                    "const": agent.id,
                    "title": format!("{} ({}): {}", agent.name, price_label(agent),
                                     shorten(&job_of(agent), FORM_JOB_CHARS)),
                })
            })
            .collect();
        let on: Vec<&str> = agents
            .iter()
            .filter(|agent| agent.enabled)
            .map(|agent| agent.id.as_str())
            .collect();
        let mut properties = Map::new();
        properties.insert(
            FORM_FIELD.to_string(),
            json!({
                "type": "array",
                "title": "ContextAgents on",
                "description": "Checked agents can run in this workspace; unchecked ones can't until they are turned on again.",
                "items": {"anyOf": options},
                "default": on,
            }),
        );
        return json!({"type": "object", "properties": properties, "required": [FORM_FIELD]});
    }
    let mut properties = Map::new();
    for agent in agents {
        let (group, _) = category_label_rank(catalog, &category_of(agent));
        properties.insert(
            agent.id.clone(),
            json!({
                "type": "boolean",
                "title": format!("{} ({})", agent.name, price_label(agent)),
                "description": format!("{group}: {}. It {}.",
                                       shorten(&job_of(agent), 100),
                                       access_line(agent)),
                "default": agent.enabled,
            }),
        );
    }
    json!({"type": "object", "properties": properties})
}

/// What the form was asked about: an answer given for another state is not taken.
pub(crate) fn form_state(workspace_id: Uuid, catalog: &Catalog) -> String {
    let mut pairs: Vec<String> = catalog
        .agents
        .iter()
        .map(|agent| format!("{}={}", agent.id, agent.enabled))
        .collect();
    pairs.sort();
    let digest = Sha256::digest(pairs.join(",").as_bytes());
    format!(
        "agents-choose:{workspace_id}:{}",
        &hex::encode(digest)[..16]
    )
}

/// The ids the person left checked, from either form; `None` when the answer has neither.
pub(crate) fn chosen(catalog: &Catalog, content: &Map<String, Value>) -> Option<Vec<String>> {
    if let Some(items) = content.get(FORM_FIELD).and_then(Value::as_array) {
        let picked: HashSet<&str> = items.iter().filter_map(Value::as_str).collect();
        return Some(
            catalog
                .agents
                .iter()
                .filter(|agent| picked.contains(agent.id.as_str()))
                .map(|agent| agent.id.clone())
                .collect(),
        );
    }
    if !catalog
        .agents
        .iter()
        .any(|agent| content.get(&agent.id).is_some_and(Value::is_boolean))
    {
        return None;
    }
    Some(
        catalog
            .agents
            .iter()
            .filter(|agent| {
                content
                    .get(&agent.id)
                    .and_then(Value::as_bool)
                    .unwrap_or(agent.enabled)
            })
            .map(|agent| agent.id.clone())
            .collect(),
    )
}

// ---------------------------------------------------------------------------------------
// After a change
// ---------------------------------------------------------------------------------------

fn named(changes: &[&Change]) -> String {
    changes
        .iter()
        .map(|change| format!("{} ({})", change.id, change.name))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The call that puts back what `done` changed.
fn undo_call(done: &[&Change], before: &Catalog) -> Option<String> {
    let switched: Vec<&&Change> = done
        .iter()
        .filter(|change| change.grants.is_empty())
        .collect();
    if switched.is_empty() {
        return None;
    }
    let list = |changes: &[&&Change]| -> String {
        match changes {
            [one] => format!("agent=\"{}\"", one.id),
            many => format!(
                "agents=[{}]",
                many.iter()
                    .map(|change| format!("\"{}\"", change.id))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    };
    let ons: Vec<&&Change> = switched.iter().copied().filter(|c| c.on).collect();
    let offs: Vec<&&Change> = switched.iter().copied().filter(|c| !c.on).collect();
    Some(match (ons.is_empty(), offs.is_empty()) {
        (false, true) => format!("{TOOL_NAME}(action=\"disable\", {})", list(&ons)),
        (true, false) => format!("{TOOL_NAME}(action=\"enable\", {})", list(&offs)),
        _ => {
            let before_on: Vec<String> = before
                .agents
                .iter()
                .filter(|agent| agent.enabled)
                .map(|agent| format!("\"{}\"", agent.id))
                .collect();
            if before_on.is_empty() {
                format!("{TOOL_NAME}(action=\"choose\", agents=\"none\")")
            } else {
                format!(
                    "{TOOL_NAME}(action=\"choose\", agents=[{}])",
                    before_on.join(", ")
                )
            }
        }
    })
}

/// What a change did: the new count, what turned on and off, what was already so, what
/// failed, and how to undo it.
pub(crate) fn outcome_text(
    info: &WorkspaceInfo,
    before: &Catalog,
    after: Option<&Catalog>,
    plan: &Plan,
    failed: &[(Change, String)],
) -> String {
    let place = info.where_label();
    let failed_ids: HashSet<&str> = failed
        .iter()
        .map(|(change, _)| change.id.as_str())
        .collect();
    let done: Vec<&Change> = plan
        .changes
        .iter()
        .filter(|change| !failed_ids.contains(change.id.as_str()))
        .collect();
    let before_on = on_count(before);
    let total = after.unwrap_or(before).agents.len();
    let now_on = match after {
        Some(after) => on_count(after),
        None => {
            let delta: isize = done
                .iter()
                .filter(|change| change.grants.is_empty())
                .map(|change| if change.on { 1 } else { -1 })
                .sum();
            (before_on as isize + delta).max(0) as usize
        }
    };
    let mut lines = Vec::new();
    if done.is_empty() {
        lines.push(format!(
            "Nothing changed: {now_on} of {total} ContextAgents are on {place}."
        ));
    } else {
        lines.push(format!(
            "ContextAgents {place}: {now_on} of {total} on (was {before_on})."
        ));
    }
    let turned_on: Vec<&Change> = done
        .iter()
        .copied()
        .filter(|change| change.on && change.grants.is_empty())
        .collect();
    let turned_off: Vec<&Change> = done.iter().copied().filter(|change| !change.on).collect();
    if !turned_on.is_empty() {
        lines.push(format!("Turned on: {}.", named(&turned_on)));
    }
    if !turned_off.is_empty() {
        lines.push(format!("Turned off: {}.", named(&turned_off)));
    }
    for change in done.iter().filter(|change| !change.grants.is_empty()) {
        lines.push(format!(
            "Reviewed: {} may now also {}.",
            change.id,
            change.grants.join("; ")
        ));
    }
    if !plan.already_on.is_empty() {
        lines.push(format!("Already on: {}.", plan.already_on.join(", ")));
    }
    if !plan.already_off.is_empty() {
        lines.push(format!("Already off: {}.", plan.already_off.join(", ")));
    }
    if !plan.still_review.is_empty() {
        lines.push(format!(
            "Still needs review: {} (it stays on as it was; enable it by name to grant its new access).",
            plan.still_review.join(", ")
        ));
    }
    for (change, reason) in failed {
        lines.push(format!(
            "Not changed: {} stays {}: {reason}",
            change.id,
            if change.on { "off" } else { "on" }
        ));
    }
    if let Some(note) = &plan.note {
        let mut chars = note.chars();
        let note: String = chars
            .next()
            .map(|first| first.to_uppercase().chain(chars).collect())
            .unwrap_or_default();
        lines.push(note);
    }
    if let Some(undo) = undo_call(&done, before) {
        lines.push(format!("Undo: {undo}"));
    }
    lines.join("\n")
}

/// The same, as data.
pub(crate) fn outcome_json(
    workspace_id: Uuid,
    after: Option<&Catalog>,
    plan: &Plan,
    failed: &[(Change, String)],
) -> Value {
    let failed_ids: HashSet<&str> = failed
        .iter()
        .map(|(change, _)| change.id.as_str())
        .collect();
    let done = |on: bool| -> Vec<&str> {
        plan.changes
            .iter()
            .filter(|c| c.on == on && !failed_ids.contains(c.id.as_str()))
            .map(|c| c.id.as_str())
            .collect()
    };
    json!({
        "workspace_id": workspace_id,
        "changed": plan.changes.len() > failed.len(),
        "turned_on": done(true),
        "turned_off": done(false),
        "already_on": plan.already_on,
        "already_off": plan.already_off,
        "still_needs_review": plan.still_review,
        "failed": failed.iter().map(|(c, reason)| json!({"id": c.id, "reason": reason})).collect::<Vec<_>>(),
        "on": after.map(on_count),
        "total": after.map(|c| c.agents.len()),
        "enabled": after.map(|c| c.enabled().iter().map(|a| a.id.clone()).collect::<Vec<_>>()),
    })
}

// ---------------------------------------------------------------------------------------
// The actions
// ---------------------------------------------------------------------------------------

/// What a write did, before the answer is written.
struct Applied {
    failed: Vec<(Change, String)>,
    /// The caller turned out not to be allowed.
    refused: bool,
}

impl SlipstreamTool {
    /// The settings: the view where the API has it, else the catalog and the members. `Err`
    /// is the answer to give instead (a workspace whose agents were refused).
    async fn load(&self, workspace_id: Uuid) -> Result<std::result::Result<Loaded, ToolResult>> {
        let (view_path, workspace_path) = (
            settings_path(workspace_id),
            format!("/workspaces/{workspace_id}"),
        );
        let (view, workspace) = tokio::join!(
            self.client.get::<Value>(&view_path),
            self.client.get::<Value>(&workspace_path),
        );
        let workspace = workspace.ok().map(unwrap_data);
        match view.map(unwrap_data) {
            Ok(view) if view.get("agents").is_some_and(Value::is_array) => {
                return Ok(Ok(Loaded {
                    catalog: Arc::new(Catalog::from_value(&view)),
                    info: WorkspaceInfo::from_view(&view, workspace.as_ref()),
                    bulk: true,
                }));
            }
            Err(Error::Http {
                status: 401 | 403,
                message,
                ..
            }) => {
                let info = WorkspaceInfo::from_reads(workspace.as_ref(), None, None);
                return Ok(Err(closed_result(workspace_id, &info, &message)));
            }
            // An API from before the settings view (404), or an answer that is not one:
            // the catalog and the members say the same.
            _ => {}
        }
        let people = async {
            let members_path = format!("/workspaces/{workspace_id}/members?page=1&page_size=100");
            let (me, members) = tokio::join!(
                self.client.get::<Value>("/auth/me"),
                self.client.get::<Value>(&members_path),
            );
            (me.ok().map(unwrap_data), members.ok().map(unwrap_data))
        };
        let (availability, people) = tokio::join!(
            self.read_catalog(workspace_id),
            tokio::time::timeout(WORKSPACE_INFO_BUDGET, people)
        );
        let (me, members) = people.unwrap_or((None, None));
        let info = WorkspaceInfo::from_reads(workspace.as_ref(), me.as_ref(), members.as_ref());
        match availability {
            Availability::Available(catalog) | Availability::NoneEnabled(catalog) => {
                Ok(Ok(Loaded {
                    catalog,
                    info,
                    bulk: false,
                }))
            }
            Availability::Refused(message) => Ok(Err(closed_result(workspace_id, &info, &message))),
            Availability::Unreachable(message) => Err(Error::Network(format!(
                "The ContextAgents settings could not be read: {message}"
            ))),
        }
    }

    pub(super) async fn settings(&self, input: &SlipstreamInput) -> Result<ToolResult> {
        let (workspace_id, _) = self.scope(input).await?;
        let loaded = match self.load(workspace_id).await? {
            Ok(loaded) => loaded,
            Err(answer) => return Ok(answer),
        };
        Ok(ToolResult::with_structured(
            settings_text(&loaded.catalog, &loaded.info),
            settings_json(workspace_id, &loaded.catalog, &loaded.info),
        ))
    }

    /// `enable` and `disable`: only the agents named change.
    pub(super) async fn switch(&self, input: &SlipstreamInput, verb: Verb) -> Result<ToolResult> {
        let target =
            target_of(input.agent.as_deref(), input.agents.as_ref()).map_err(Error::Validation)?;
        let (workspace_id, _) = self.scope(input).await?;
        let loaded = match self.load(workspace_id).await? {
            Ok(loaded) => loaded,
            Err(answer) => return Ok(answer),
        };
        let plan = plan(&loaded.catalog, verb, &target).map_err(Error::Validation)?;
        self.apply(workspace_id, &loaded, plan).await
    }

    /// `choose`: the set named in the call, else the person's answer to a form.
    pub(super) async fn choose(&self, input: &SlipstreamInput) -> Result<ToolResult> {
        let named = input.agent.is_some()
            || input
                .agents
                .as_ref()
                .is_some_and(|agents| !agents.is_null());
        let target = if named {
            Some(
                target_of(input.agent.as_deref(), input.agents.as_ref())
                    .map_err(Error::Validation)?,
            )
        } else {
            None
        };
        let (workspace_id, _) = self.scope(input).await?;
        let loaded = match self.load(workspace_id).await? {
            Ok(loaded) => loaded,
            Err(answer) => return Ok(answer),
        };
        let (catalog, info) = (&loaded.catalog, &loaded.info);
        if catalog.agents.is_empty() {
            return Ok(ToolResult::text(format!(
                "No ContextAgents are available {} yet.",
                info.where_label()
            )));
        }
        if info.manage.can_change == Some(false) {
            return Ok(read_only_result(workspace_id, info, ""));
        }
        if let Some(target) = target {
            let plan = plan(catalog, Verb::Choose, &target).map_err(Error::Validation)?;
            return self.apply(workspace_id, &loaded, plan).await;
        }

        let Some(peer) = peer::current().filter(peer::CallPeer::can_elicit) else {
            return Ok(choose_in_text(workspace_id, catalog, info, None));
        };
        let message = form_message(catalog, info);
        let state = form_state(workspace_id, catalog);
        let multi_select = peer.can_multi_select();
        let mut answer = peer
            .ask(
                FORM_KEY,
                &message,
                choose_form(catalog, multi_select),
                &state,
                CHOOSE_TIMEOUT,
            )
            .await;
        // A client that turns the multi-select down at once gets one checkbox per agent.
        if multi_select
            && matches!(&answer, Elicited::Unanswered(reason)
                        if reason.starts_with("the client answered with an error"))
        {
            answer = peer
                .ask(
                    FORM_KEY,
                    &message,
                    choose_form(catalog, false),
                    &state,
                    CHOOSE_TIMEOUT,
                )
                .await;
        }
        match answer {
            Elicited::Pending => Ok(ToolResult::with_structured(
                format!(
                    "Asking the person which ContextAgents to keep on {}.",
                    info.where_label()
                ),
                json!({"workspace_id": workspace_id, "asking": true}),
            )),
            Elicited::Accepted(content) => match chosen(catalog, &content) {
                Some(wanted) => {
                    // The form's answer is the exact set to have on.
                    let plan = plan(catalog, Verb::Choose, &Target::Some(wanted))
                        .map_err(Error::Validation)?;
                    self.apply(workspace_id, &loaded, plan).await
                }
                None => Ok(ToolResult::with_structured(
                    "Nothing changed: the form came back without a choice.".to_string(),
                    json!({"workspace_id": workspace_id, "changed": false}),
                )),
            },
            Elicited::Declined | Elicited::Cancelled => Ok(ToolResult::with_structured(
                format!(
                    "Nothing changed: the person closed the form. {} of {} ContextAgents stay on {}.",
                    on_count(catalog),
                    catalog.agents.len(),
                    info.where_label()
                ),
                json!({"workspace_id": workspace_id, "changed": false}),
            )),
            Elicited::Unanswered(reason) => {
                tracing::debug!(%reason, "agents: the choose form got no answer");
                Ok(choose_in_text(workspace_id, catalog, info, Some(&reason)))
            }
        }
    }

    /// Turn one agent on or off. Turning on an agent that is on grants what it now asks for.
    async fn put_consent(&self, workspace_id: Uuid, change: &Change) -> Result<()> {
        if !is_agent_id(&change.id) {
            return Err(Error::Validation(format!(
                "{} is not an agent id",
                change.id
            )));
        }
        self.client
            .put::<Value, _>(
                &format!(
                    "/workspaces/{workspace_id}/tasks/agents/{}/consent",
                    change.id
                ),
                json!({"enabled": change.on}),
            )
            .await
            .map(|_| ())
    }

    /// One `PUT` per change, a few at a time. When it is not known whether the caller may
    /// change the settings, the first change finds out before the rest are sent.
    async fn put_each(&self, workspace_id: Uuid, changes: Vec<Change>, known: bool) -> Applied {
        let mut pending = changes;
        let mut results: Vec<(Change, Result<()>)> = Vec::new();
        if !known && !pending.is_empty() {
            let first = pending.remove(0);
            let result = self.put_consent(workspace_id, &first).await;
            if let Err(Error::Http { status: 403, .. }) = &result {
                return Applied {
                    failed: Vec::new(),
                    refused: true,
                };
            }
            results.push((first, result));
        }
        let rest: Vec<(Change, Result<()>)> = stream::iter(pending)
            .map(|change| async move {
                let result = self.put_consent(workspace_id, &change).await;
                (change, result)
            })
            .buffer_unordered(PUT_CONCURRENCY)
            .collect()
            .await;
        results.extend(rest);
        let refused = !results.is_empty()
            && results
                .iter()
                .all(|(_, result)| matches!(result, Err(Error::Http { status: 403, .. })));
        Applied {
            failed: results
                .into_iter()
                .filter_map(|(change, result)| match result {
                    Ok(()) => None,
                    Err(Error::Http { message, .. }) => Some((change, message)),
                    Err(error) => Some((change, error.to_string())),
                })
                .collect(),
            refused,
        }
    }

    /// Send a plan and say what happened: one bulk write for the agents turned on and off
    /// (per-agent writes on an API without it), one per-agent write for each review.
    async fn apply(&self, workspace_id: Uuid, loaded: &Loaded, plan: Plan) -> Result<ToolResult> {
        let (catalog, info) = (loaded.catalog.as_ref(), &loaded.info);
        if plan.changes.is_empty() {
            return Ok(ToolResult::with_structured(
                outcome_text(info, catalog, Some(catalog), &plan, &[]),
                outcome_json(workspace_id, Some(catalog), &plan, &[]),
            ));
        }
        let wanted = match plan.changes.as_slice() {
            [one] => format!(" to turn {} {}", one.id, if one.on { "on" } else { "off" }),
            _ => String::new(),
        };
        let read_only = |info: &WorkspaceInfo| {
            let mut info = info.clone();
            info.manage.can_change = Some(false);
            read_only_result(workspace_id, &info, &wanted)
        };
        if info.manage.can_change == Some(false) {
            return Ok(read_only(info));
        }
        let known = info.manage.can_change == Some(true);
        let (switches, reviews): (Vec<Change>, Vec<Change>) = plan
            .changes
            .iter()
            .cloned()
            .partition(|change| change.grants.is_empty());

        let mut failed: Vec<(Change, String)> = Vec::new();
        let mut per_agent = switches.clone();
        if loaded.bulk && !switches.is_empty() {
            let body = json!({"enable": plan.bulk_enable(catalog), "accept_new_access": false});
            match self
                .client
                .put::<Value, _>(&settings_path(workspace_id), body)
                .await
            {
                Ok(_) => per_agent.clear(),
                Err(Error::Http { status: 403, .. }) => return Ok(read_only(info)),
                // Not live yet: one write per agent.
                Err(Error::Http {
                    status: 404 | 405, ..
                }) => {}
                Err(Error::Http { message, .. }) => {
                    failed.extend(switches.iter().cloned().map(|c| (c, message.clone())));
                    per_agent.clear();
                }
                Err(other) => return Err(other),
            }
        }
        let mut sent_any = !switches.is_empty() && per_agent.is_empty() && failed.is_empty();
        if !per_agent.is_empty() {
            let applied = self.put_each(workspace_id, per_agent, known).await;
            if applied.refused {
                return Ok(read_only(info));
            }
            sent_any = true;
            failed.extend(applied.failed);
        }
        if !reviews.is_empty() {
            let applied = self
                .put_each(workspace_id, reviews, known || sent_any)
                .await;
            if applied.refused && !sent_any {
                return Ok(read_only(info));
            }
            failed.extend(applied.failed);
        }

        // Read again, so the answer shows what is true now and the tool listing learns it.
        let (view_after, availability) = tokio::join!(
            async {
                if !loaded.bulk {
                    return None;
                }
                self.client
                    .get::<Value>(&settings_path(workspace_id))
                    .await
                    .ok()
                    .map(unwrap_data)
                    .filter(|view| view.get("agents").is_some_and(Value::is_array))
                    .map(|view| Arc::new(Catalog::from_value(&view)))
            },
            self.read_catalog(workspace_id),
        );
        let after = if loaded.bulk {
            view_after
        } else {
            availability.catalog().cloned()
        };
        Ok(ToolResult::with_structured(
            outcome_text(info, catalog, after.as_deref(), &plan, &failed),
            outcome_json(workspace_id, after.as_deref(), &plan, &failed),
        ))
    }
}

/// `choose` for a client that cannot show the form: the checklist, and the call to make once
/// the person says which to keep on.
fn choose_in_text(
    workspace_id: Uuid,
    catalog: &Catalog,
    info: &WorkspaceInfo,
    reason: Option<&str>,
) -> ToolResult {
    let mut text = settings_text(catalog, info);
    let lead = match reason {
        Some(_) => "The form could not be shown here",
        None => "This client can't show a form",
    };
    text.push_str(&format!(
        "\n\n{lead}: show the person this checklist and ask which to keep on, then call {TOOL_NAME}(action=\"choose\", agents=[\"<id>\", ...]) with exactly those (or agents=\"all\" / \"none\"). Only what changes is sent."
    ));
    let mut structured = settings_json(workspace_id, catalog, info);
    structured["form"] = json!(false);
    ToolResult::with_structured(text, structured)
}
