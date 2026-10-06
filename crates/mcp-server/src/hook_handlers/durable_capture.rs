//! Save durable knowledge that was written as a local file into ContextStream.
//!
//! Two PostToolUse paths use this module:
//! - An approved Claude Code plan (ExitPlanMode) becomes a ContextStream plan
//!   with one linked task per numbered step.
//! - A repository runbook, ADR, RFC, postmortem or note is mirrored to a
//!   ContextStream doc, keyed by repository and path so later edits update the
//!   same doc.
//!
//! Both are idempotent: a small local state file maps the source to the
//! ContextStream id and the content hash last sent, and an unchanged source is
//! never sent twice.

use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

use super::common::{load_config, scrub_credential_tokens, ApiConfig};

/// Largest repository doc mirrored; bigger files are left alone.
const MAX_MIRRORED_DOC_BYTES: u64 = 256 * 1024;
const MAX_PLAN_STEPS: usize = 40;
const MAX_STEP_DESCRIPTION_CHARS: usize = 3000;
const MAX_PLAN_DESCRIPTION_CHARS: usize = 6000;
/// A same-titled plan this recent is assumed to be the agent's own capture.
const ADOPT_RECENT_PLAN_HOURS: i64 = 6;
const TASK_CREATE_CONCURRENCY: usize = 6;
/// PostToolUse hooks get 10 seconds; leave room for process start and output.
const CAPTURE_DEADLINE: Duration = Duration::from_secs(8);
const MAX_STATE_ENTRIES: usize = 2048;

// ============================================================================
// Plan parsing
// ============================================================================

/// A plan read from Claude Code's plan-mode markdown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedPlan {
    pub title: String,
    pub description: String,
    pub steps: Vec<ParsedStep>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedStep {
    pub title: String,
    pub description: String,
}

fn heading_level(line: &str) -> Option<(usize, &str)> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &line[hashes..];
    rest.starts_with(' ').then(|| (hashes, rest.trim()))
}

/// `N. text` at column 0, the plan's top-level numbered steps.
fn top_level_numbered_item(line: &str) -> Option<&str> {
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let rest = &line[digits..];
    rest.strip_prefix(". ")
        .or_else(|| rest.strip_prefix(") "))
        .map(str::trim)
}

fn strip_inline_markdown(text: &str) -> String {
    text.replace("**", "").replace('`', "").trim().to_string()
}

/// Short title for a step: its bold lead if any, else its first sentence.
fn step_title(first_line: &str) -> String {
    let first_line = first_line.trim();
    if let Some(rest) = first_line.strip_prefix("**") {
        if let Some(end) = rest.find("**") {
            let bold = rest[..end].trim().trim_end_matches(['.', ':']);
            if !bold.is_empty() {
                return truncate_chars(&strip_inline_markdown(bold), 160);
            }
        }
    }
    let plain = strip_inline_markdown(first_line);
    let sentence = plain
        .split_once(". ")
        .map(|(sentence, _)| sentence)
        .unwrap_or(&plain);
    truncate_chars(sentence.trim_end_matches('.'), 160)
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(max_chars.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// Parse plan-mode markdown: H1 title, the Context section as description,
/// and top-level numbered items (or, failing that, `##` sections) as steps.
pub fn parse_plan_markdown(markdown: &str) -> ParsedPlan {
    let lines: Vec<&str> = markdown.lines().collect();
    let title = lines
        .iter()
        .find_map(|line| match heading_level(line) {
            Some((1, text)) => Some(strip_inline_markdown(text)),
            _ => None,
        })
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| "Approved plan".to_string());

    // Sections: (heading, body lines) for every `##` heading.
    let mut sections: Vec<(String, Vec<&str>)> = Vec::new();
    let mut preamble: Vec<&str> = Vec::new();
    for line in &lines {
        match heading_level(line) {
            Some((1, _)) => {}
            Some((2, text)) => sections.push((strip_inline_markdown(text), Vec::new())),
            _ => match sections.last_mut() {
                Some((_, body)) => body.push(line),
                None => preamble.push(line),
            },
        }
    }

    let context_body = sections
        .iter()
        .find(|(heading, _)| heading.to_ascii_lowercase().contains("context"))
        .map(|(_, body)| body.join("\n"))
        .unwrap_or_else(|| preamble.join("\n"));
    let description = truncate_chars(context_body.trim(), MAX_PLAN_DESCRIPTION_CHARS);

    // Top-level numbered items, each tagged with the section it sits in.
    let mut steps: Vec<ParsedStep> = Vec::new();
    let mut current: Option<(String, Vec<String>)> = None;
    let mut section = String::new();
    let flush = |current: &mut Option<(String, Vec<String>)>, steps: &mut Vec<ParsedStep>| {
        if let Some((section, body)) = current.take() {
            let first = body.first().cloned().unwrap_or_default();
            let text = body.join("\n");
            let description = if section.is_empty() {
                text
            } else {
                format!("[{section}] {text}")
            };
            steps.push(ParsedStep {
                title: step_title(&first),
                description: truncate_chars(description.trim(), MAX_STEP_DESCRIPTION_CHARS),
            });
        }
    };
    for line in &lines {
        if let Some((_, text)) = heading_level(line) {
            flush(&mut current, &mut steps);
            section = strip_inline_markdown(text);
            continue;
        }
        if let Some(item) = top_level_numbered_item(line) {
            flush(&mut current, &mut steps);
            current = Some((section.clone(), vec![item.to_string()]));
            continue;
        }
        if current.is_some() {
            // Indented or blank lines continue the item; a new top-level
            // bullet or paragraph ends it.
            let continues =
                line.trim().is_empty() || line.starts_with(' ') || line.starts_with('\t');
            if !continues {
                flush(&mut current, &mut steps);
            } else if let Some((_, body)) = current.as_mut() {
                body.push(line.trim_end().to_string());
            }
        }
    }
    flush(&mut current, &mut steps);

    if steps.len() < 2 {
        steps = sections
            .iter()
            .filter(|(heading, body)| {
                !heading.to_ascii_lowercase().contains("context")
                    && body.iter().any(|line| !line.trim().is_empty())
            })
            .map(|(heading, body)| ParsedStep {
                title: truncate_chars(heading, 160),
                description: truncate_chars(body.join("\n").trim(), MAX_STEP_DESCRIPTION_CHARS),
            })
            .collect();
    }
    if steps.is_empty() {
        steps.push(ParsedStep {
            title: truncate_chars(&title, 160),
            description: truncate_chars(markdown.trim(), MAX_STEP_DESCRIPTION_CHARS),
        });
    }
    steps.truncate(MAX_PLAN_STEPS);

    ParsedPlan {
        title,
        description,
        steps,
    }
}

// ============================================================================
// Local mirror state
// ============================================================================

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct MirrorEntry {
    id: String,
    sha256: String,
    updated_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct MirrorState {
    #[serde(default)]
    entries: BTreeMap<String, MirrorEntry>,
}

fn state_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".contextstream"))
}

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn write_state_atomically<S: Serialize>(path: &Path, state: &S) -> std::io::Result<()> {
    let json = serde_json::to_vec(state).map_err(std::io::Error::other)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("mirror-state.json");
    let temporary = path.with_file_name(format!("{file_name}.{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&json)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// Read-modify-write a JSON state file under an advisory lock on a stable
/// sibling, so parallel hooks never lose each other's entries.
fn with_locked_json<S, T>(path: &Path, update: impl FnOnce(&mut S) -> T) -> Option<T>
where
    S: Default + Serialize + serde::de::DeserializeOwned,
{
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    let lock: File = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("lock"))
        .ok()?;
    fs2::FileExt::lock_exclusive(&lock).ok()?;
    let mut state: S = std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default();
    let result = update(&mut state);
    write_state_atomically(path, &state).ok()?;
    Some(result)
}

fn with_mirror_state<T>(path: &Path, update: impl FnOnce(&mut MirrorState) -> T) -> Option<T> {
    with_locked_json(path, |state: &mut MirrorState| {
        let result = update(state);
        prune_mirror_state(state);
        result
    })
}

fn prune_mirror_state(state: &mut MirrorState) {
    if state.entries.len() > MAX_STATE_ENTRIES {
        let mut oldest: Vec<(String, String)> = state
            .entries
            .iter()
            .map(|(key, entry)| (entry.updated_at.clone(), key.clone()))
            .collect();
        oldest.sort_unstable();
        let excess = oldest.len() - MAX_STATE_ENTRIES;
        for (_, key) in oldest.into_iter().take(excess) {
            state.entries.remove(&key);
        }
    }
}

fn read_entry(path: &Path, key: &str) -> Option<MirrorEntry> {
    let bytes = std::fs::read(path).ok()?;
    let state: MirrorState = serde_json::from_slice(&bytes).ok()?;
    state.entries.get(key).cloned()
}

fn record_entry(path: &Path, key: &str, id: &str, sha256: &str) {
    let entry = MirrorEntry {
        id: id.to_string(),
        sha256: sha256.to_string(),
        updated_at: chrono::Utc::now().to_rfc3339(),
    };
    let _ = with_mirror_state(path, |state| {
        state.entries.insert(key.to_string(), entry);
    });
}

/// The id recorded for `key` in a receipts file, if any.
pub(crate) fn read_receipt(path: &Path, key: &str) -> Option<String> {
    read_entry(path, key).map(|entry| entry.id)
}

/// Record that `key` was saved as `id` with content hash `sha256`.
pub(crate) fn record_receipt(path: &Path, key: &str, id: &str, sha256: &str) {
    record_entry(path, key, id, sha256);
}

// ============================================================================
// Scope and client
// ============================================================================

struct CaptureScope {
    config: ApiConfig,
    workspace_id: Option<Uuid>,
    project_id: Option<Uuid>,
}

impl CaptureScope {
    fn client(&self) -> mcp_client::ContextStreamClient {
        mcp_client::ContextStreamClient::new(mcp_types::Config {
            api_url: self.config.api_url.clone(),
            api_key: Some(self.config.api_key.clone()),
            default_workspace_id: self.workspace_id,
            default_project_id: self.project_id,
            ..Default::default()
        })
    }
}

/// The scope the server resolved for a host session's working folder, as
/// observed by PostToolUse on a successful `init` or `context` call.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionScope {
    cwd: String,
    workspace_id: Uuid,
    project_id: Option<Uuid>,
    updated_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SessionScopes {
    #[serde(default)]
    sessions: BTreeMap<String, SessionScope>,
}

const SESSION_SCOPE_MAX_AGE_HOURS: i64 = 24;
const MAX_SESSION_SCOPES: usize = 512;

fn session_scopes_path() -> Option<PathBuf> {
    state_dir().map(|dir| dir.join("session-scopes.json"))
}

/// Record the workspace/project the server resolved for this host session.
/// Plans and docs saved later in the session use it when the checkout has no
/// verified local link (the same scope the agent's own calls would use).
pub(crate) fn record_session_scope(
    host_session_id: &str,
    cwd: &str,
    workspace_id: Uuid,
    project_id: Option<Uuid>,
) {
    let Some(path) = session_scopes_path() else {
        return;
    };
    record_session_scope_at(&path, host_session_id, cwd, workspace_id, project_id);
}

fn record_session_scope_at(
    path: &Path,
    host_session_id: &str,
    cwd: &str,
    workspace_id: Uuid,
    project_id: Option<Uuid>,
) {
    let key = sha256_hex(host_session_id.as_bytes());
    let scope = SessionScope {
        cwd: cwd.trim_end_matches('/').to_string(),
        workspace_id,
        project_id,
        updated_at: chrono::Utc::now().to_rfc3339(),
    };
    let _ = with_locked_json(path, |state: &mut SessionScopes| {
        state.sessions.insert(key, scope);
        let cutoff = chrono::Utc::now() - chrono::Duration::hours(SESSION_SCOPE_MAX_AGE_HOURS);
        state.sessions.retain(|_, entry| {
            chrono::DateTime::parse_from_rfc3339(&entry.updated_at)
                .is_ok_and(|updated| updated >= cutoff)
        });
        while state.sessions.len() > MAX_SESSION_SCOPES {
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
    });
}

/// Whether one folder contains the other (or they are the same folder).
fn folders_related(a: &str, b: &str) -> bool {
    let a = a.trim_end_matches('/');
    let b = b.trim_end_matches('/');
    !a.is_empty()
        && !b.is_empty()
        && (a == b || a.starts_with(&format!("{b}/")) || b.starts_with(&format!("{a}/")))
}

fn session_scope_at(
    path: &Path,
    host_session_id: &str,
    folder: &str,
) -> Option<(Uuid, Option<Uuid>)> {
    let bytes = std::fs::read(path).ok()?;
    let state: SessionScopes = serde_json::from_slice(&bytes).ok()?;
    let entry = state
        .sessions
        .get(&sha256_hex(host_session_id.as_bytes()))?;
    let fresh = chrono::DateTime::parse_from_rfc3339(&entry.updated_at).is_ok_and(|updated| {
        chrono::Utc::now().signed_duration_since(updated)
            <= chrono::Duration::hours(SESSION_SCOPE_MAX_AGE_HOURS)
    });
    (fresh && folders_related(&entry.cwd, folder)).then_some((entry.workspace_id, entry.project_id))
}

/// Repository docs written before the session had a resolved scope (the
/// agent wrote them before calling init/context). Mirrored once it does.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PendingDocs {
    #[serde(default)]
    sessions: BTreeMap<String, Vec<PendingDoc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PendingDoc {
    path: String,
    queued_at: String,
}

const MAX_PENDING_DOCS_PER_SESSION: usize = 50;

fn pending_docs_path() -> Option<PathBuf> {
    state_dir().map(|dir| dir.join("pending-doc-mirrors.json"))
}

fn queue_pending_doc_at(path: &Path, host_session_id: &str, doc_path: &Path) {
    let key = sha256_hex(host_session_id.as_bytes());
    let doc = doc_path.to_string_lossy().to_string();
    let _ = with_locked_json(path, |state: &mut PendingDocs| {
        let cutoff = chrono::Utc::now() - chrono::Duration::hours(SESSION_SCOPE_MAX_AGE_HOURS);
        for docs in state.sessions.values_mut() {
            docs.retain(|pending| {
                chrono::DateTime::parse_from_rfc3339(&pending.queued_at)
                    .is_ok_and(|queued| queued >= cutoff)
            });
        }
        state.sessions.retain(|_, docs| !docs.is_empty());
        let docs = state.sessions.entry(key).or_default();
        if !docs.iter().any(|pending| pending.path == doc)
            && docs.len() < MAX_PENDING_DOCS_PER_SESSION
        {
            docs.push(PendingDoc {
                path: doc,
                queued_at: chrono::Utc::now().to_rfc3339(),
            });
        }
    });
}

fn take_pending_docs_at(path: &Path, host_session_id: &str) -> Vec<PathBuf> {
    let key = sha256_hex(host_session_id.as_bytes());
    if !path.exists() {
        return Vec::new();
    }
    with_locked_json(path, |state: &mut PendingDocs| {
        state
            .sessions
            .remove(&key)
            .unwrap_or_default()
            .into_iter()
            .map(|pending| PathBuf::from(pending.path))
            .collect()
    })
    .unwrap_or_default()
}

/// Remember a repository doc written before this session had a scope.
pub(crate) fn queue_pending_doc(host_session_id: &str, doc_path: &Path) {
    if let Some(path) = pending_docs_path() {
        queue_pending_doc_at(&path, host_session_id, doc_path);
    }
}

/// Mirror the docs this session wrote before it had a scope. Called after a
/// successful init/context recorded one. Returns one note per mirrored doc.
pub(crate) async fn mirror_pending_docs(host_session_id: &str) -> Vec<String> {
    let Some(path) = pending_docs_path() else {
        return Vec::new();
    };
    let mut notes = Vec::new();
    for doc in take_pending_docs_at(&path, host_session_id) {
        let Some(doc_type) = super::durable_paths::repo_doc_type(&doc.to_string_lossy()) else {
            continue;
        };
        if let Some(mirror) = mirror_repo_doc(&doc, doc_type, Some(host_session_id)).await {
            notes.push(mirror.message());
        }
    }
    notes
}

/// Credentials from the usual hook sources and the workspace/project for
/// `folder`: its verified checkout link or folder mapping first, then the
/// scope the server resolved for this host session's folder. `None` without
/// credentials or a workspace: content is never sent to a default workspace.
async fn resolve_scope(folder: &str, host_session_id: Option<&str>) -> Option<CaptureScope> {
    let config = load_config(folder);
    if !config.is_configured() {
        return None;
    }
    let mapping = mcp_session::auto_init::resolve_workspace(folder).await;
    let parse = |value: &Option<String>| value.as_deref().and_then(|id| Uuid::parse_str(id).ok());
    let local = mapping
        .as_ref()
        .map(|mapping| (mapping.workspace_id, mapping.project_id))
        .or_else(|| parse(&config.workspace_id).map(|ws| (ws, parse(&config.project_id))));
    let (workspace_id, project_id) = local.or_else(|| {
        let path = session_scopes_path()?;
        session_scope_at(&path, host_session_id?, folder)
    })?;
    Some(CaptureScope {
        config,
        workspace_id: Some(workspace_id),
        project_id,
    })
}

/// Whether a repository doc at `path` can be mirrored: credentials exist and
/// its checkout is linked to a ContextStream workspace (locally, or through
/// this session's resolved scope). Reads local files only.
pub(crate) async fn doc_mirror_available(path: &Path, host_session_id: Option<&str>) -> bool {
    let Some(root) = repository_root(path).or_else(|| path.parent().map(Path::to_path_buf)) else {
        return false;
    };
    resolve_scope(&root.to_string_lossy(), host_session_id)
        .await
        .is_some()
}

fn response_id(value: &Value) -> Option<Uuid> {
    [
        value.get("id"),
        value.get("data").and_then(|data| data.get("id")),
        value.get("plan").and_then(|plan| plan.get("id")),
        value.get("doc").and_then(|doc| doc.get("id")),
    ]
    .into_iter()
    .flatten()
    .find_map(|id| id.as_str().and_then(|id| Uuid::parse_str(id).ok()))
}

fn list_items(value: &Value) -> Vec<Value> {
    for key in ["items", "plans", "docs", "data", "results"] {
        if let Some(items) = value.get(key).and_then(Value::as_array) {
            return items.clone();
        }
        if let Some(items) = value
            .get("data")
            .and_then(|data| data.get(key))
            .and_then(Value::as_array)
        {
            return items.clone();
        }
    }
    value.as_array().cloned().unwrap_or_default()
}

fn same_title(item: &Value, title: &str) -> bool {
    item.get("title")
        .and_then(Value::as_str)
        .is_some_and(|candidate| candidate.trim().eq_ignore_ascii_case(title.trim()))
}

fn created_within_hours(item: &Value, hours: i64) -> bool {
    item.get("created_at")
        .and_then(Value::as_str)
        .and_then(|raw| chrono::DateTime::parse_from_rfc3339(raw).ok())
        .is_some_and(|created| {
            chrono::Utc::now().signed_duration_since(created) <= chrono::Duration::hours(hours)
        })
}

// ============================================================================
// Approved plans
// ============================================================================

fn plan_steps(parsed: &ParsedPlan) -> Vec<mcp_client::PlanStep> {
    parsed
        .steps
        .iter()
        .enumerate()
        .map(|(index, step)| mcp_client::PlanStep {
            id: format!("plan-step-{}", index + 1),
            title: step.title.clone(),
            order: index as i32 + 1,
            description: Some(step.description.clone()),
            estimated_effort: None,
        })
        .collect()
}

async fn create_plan_tasks(
    client: &mcp_client::ContextStreamClient,
    scope: &CaptureScope,
    plan_id: Uuid,
    steps: &[mcp_client::PlanStep],
) -> usize {
    stream::iter(steps.iter().cloned())
        .map(|step| async move {
            client
                .create_task(mcp_client::CreateTaskParams {
                    title: step.title,
                    description: step.description,
                    content: None,
                    priority: Some("medium".to_string()),
                    status: Some("pending".to_string()),
                    plan_id: Some(plan_id),
                    plan_step_id: Some(step.id),
                    tags: Some(vec!["plan_mode".to_string()]),
                    order: Some(i64::from(step.order)),
                    is_personal: None,
                    workspace_id: scope.workspace_id,
                    project_id: scope.project_id,
                })
                .await
                .is_ok()
        })
        .buffer_unordered(TASK_CREATE_CONCURRENCY)
        .filter(|created| std::future::ready(*created))
        .count()
        .await
}

/// Agent-facing result of saving an approved plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanCapture {
    pub plan_id: String,
    pub step_count: usize,
    pub task_count: usize,
    pub outcome: &'static str,
}

impl PlanCapture {
    pub fn message(&self) -> String {
        let what = match self.outcome {
            "created" => format!(
                "saved the approved plan as plan {} ({} steps, {} linked tasks)",
                self.plan_id, self.step_count, self.task_count
            ),
            "updated" => format!("updated plan {} from the approved plan", self.plan_id),
            "adopted" => format!(
                "found this plan already saved as plan {} and linked it to the plan file",
                self.plan_id
            ),
            _ => format!("already has this plan as plan {}", self.plan_id),
        };
        format!(
            "ContextStream {what}. Do not call capture_plan for it again. Change it with session(action=\"update_plan\", plan_id=\"{id}\") and track progress with memory(action=\"update_task\", task_status=\"in_progress|completed\").",
            id = self.plan_id
        )
    }
}

async fn capture_plan_inner(
    cwd: &str,
    plan_markdown: &str,
    plan_file_path: Option<&str>,
    host_session_id: Option<&str>,
) -> Option<PlanCapture> {
    let parsed = parse_plan_markdown(plan_markdown);
    let sha = sha256_hex(plan_markdown.as_bytes());
    let key = sha256_hex(plan_file_path.unwrap_or(&parsed.title).as_bytes());
    let state_path = state_dir()?.join("plan-mirrors.json");

    if let Some(entry) = read_entry(&state_path, &key) {
        if entry.sha256 == sha {
            return Some(PlanCapture {
                plan_id: entry.id,
                step_count: parsed.steps.len(),
                task_count: 0,
                outcome: "unchanged",
            });
        }
    }

    let scope = resolve_scope(cwd, host_session_id).await?;
    let client = scope.client();
    let steps = plan_steps(&parsed);

    if let Some(entry) = read_entry(&state_path, &key) {
        if let Ok(plan_id) = Uuid::parse_str(&entry.id) {
            let updated = client
                .update_plan(
                    plan_id,
                    mcp_client::UpdatePlanParams {
                        title: Some(parsed.title.clone()),
                        description: Some(parsed.description.clone()),
                        status: None,
                        goals: None,
                        // Steps keep their ids, so existing linked tasks stay attached.
                        steps: Some(steps.clone()),
                        linked_items: None,
                    },
                )
                .await;
            if updated.is_ok() {
                record_entry(&state_path, &key, &entry.id, &sha);
                return Some(PlanCapture {
                    plan_id: entry.id,
                    step_count: steps.len(),
                    task_count: 0,
                    outcome: "updated",
                });
            }
        }
    }

    // The agent may already have saved this plan itself before exiting plan
    // mode; adopt that plan instead of creating a duplicate.
    if let Ok(listed) = client
        .list_plans_filtered(
            scope.workspace_id,
            scope.project_id,
            Some(&parsed.title),
            None,
            Some(5),
        )
        .await
    {
        if let Some(existing) = list_items(&listed).into_iter().find(|item| {
            same_title(item, &parsed.title) && created_within_hours(item, ADOPT_RECENT_PLAN_HOURS)
        }) {
            if let Some(plan_id) = response_id(&existing) {
                let plan_id = plan_id.to_string();
                record_entry(&state_path, &key, &plan_id, &sha);
                return Some(PlanCapture {
                    plan_id,
                    step_count: steps.len(),
                    task_count: 0,
                    outcome: "adopted",
                });
            }
        }
    }

    let created = client
        .capture_plan(mcp_client::CapturePlanParams {
            title: parsed.title.clone(),
            description: Some(parsed.description.clone()),
            goals: None,
            steps: Some(steps.clone()),
            tags: Some(vec!["plan_mode".to_string(), "auto_capture".to_string()]),
            linked_items: None,
            workspace_id: scope.workspace_id,
            project_id: scope.project_id,
            is_personal: None,
        })
        .await
        .ok()?;
    let plan_id = response_id(&created)?;
    let task_count = create_plan_tasks(&client, &scope, plan_id, &steps).await;
    record_entry(&state_path, &key, &plan_id.to_string(), &sha);
    Some(PlanCapture {
        plan_id: plan_id.to_string(),
        step_count: steps.len(),
        task_count,
        outcome: "created",
    })
}

/// Save an approved plan-mode plan to ContextStream. Returns `None` when the
/// plan could not be saved (no credentials, network failure, deadline).
pub async fn capture_approved_plan(
    cwd: &str,
    plan_markdown: &str,
    plan_file_path: Option<&str>,
    host_session_id: Option<&str>,
) -> Option<PlanCapture> {
    if plan_markdown.trim().is_empty() {
        return None;
    }
    tokio::time::timeout(
        CAPTURE_DEADLINE,
        capture_plan_inner(cwd, plan_markdown, plan_file_path, host_session_id),
    )
    .await
    .ok()
    .flatten()
}

// ============================================================================
// Repository docs
// ============================================================================

fn repository_root(path: &Path) -> Option<PathBuf> {
    let mut dir = path.parent()?.to_path_buf();
    loop {
        if dir.join(".git").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// `owner/name` for the repository, or the checkout folder name.
fn repository_label(root: &Path) -> String {
    mcp_session::checkout_identity::current_repository_canonical_url(root)
        .ok()
        .flatten()
        .and_then(|url| {
            let trimmed = url.trim_end_matches(".git");
            let mut parts = trimmed.rsplit('/');
            let name = parts.next()?;
            let owner = parts.next()?;
            Some(format!("{owner}/{name}"))
        })
        .or_else(|| {
            root.file_name()
                .and_then(|name| name.to_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "repository".to_string())
}

fn doc_title(content: &str, file_stem: &str, repository: &str, relative: &str) -> String {
    let heading = content
        .lines()
        .find_map(|line| match heading_level(line) {
            Some((1, text)) => Some(strip_inline_markdown(text)),
            _ => None,
        })
        .filter(|heading| !heading.is_empty())
        .unwrap_or_else(|| file_stem.to_string());
    format!(
        "{} ({repository}/{relative})",
        truncate_chars(&heading, 160)
    )
}

/// Agent-facing result of mirroring a repository doc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocMirror {
    pub doc_id: String,
    pub doc_type: &'static str,
    pub outcome: &'static str,
}

impl DocMirror {
    pub fn message(&self) -> String {
        format!(
            "ContextStream {} doc {} (doc_type=\"{}\") from this file. Later edits to the file update the same doc, so do not call create_doc for it.",
            if self.outcome == "created" {
                "created"
            } else {
                "updated"
            },
            self.doc_id,
            self.doc_type
        )
    }
}

async fn mirror_doc_inner(
    absolute_path: &Path,
    doc_type: &'static str,
    host_session_id: Option<&str>,
) -> Option<DocMirror> {
    let metadata = std::fs::metadata(absolute_path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_MIRRORED_DOC_BYTES {
        return None;
    }
    let raw = std::fs::read_to_string(absolute_path).ok()?;
    if raw.trim().is_empty() {
        return None;
    }
    let content = scrub_credential_tokens(&raw);
    let root =
        repository_root(absolute_path).or_else(|| absolute_path.parent().map(Path::to_path_buf))?;
    let relative = absolute_path
        .strip_prefix(&root)
        .ok()?
        .to_string_lossy()
        .replace('\\', "/");
    let repository = repository_label(&root);
    let file_stem = absolute_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("doc");
    let title = doc_title(&content, file_stem, &repository, &relative);
    let key = sha256_hex(format!("{repository}:{relative}").as_bytes());
    let sha = sha256_hex(content.as_bytes());
    let state_path = state_dir()?.join("doc-mirrors.json");

    let previous = read_entry(&state_path, &key);
    if previous.as_ref().is_some_and(|entry| entry.sha256 == sha) {
        return None;
    }

    let scope = resolve_scope(&root.to_string_lossy(), host_session_id).await?;
    let client = scope.client();
    let update = |doc_id: Uuid| {
        let client = client.clone();
        let title = title.clone();
        let content = content.clone();
        async move {
            client
                .update_doc(
                    doc_id,
                    mcp_client::UpdateDocParams {
                        title: Some(title),
                        content: Some(content),
                        doc_type: Some(doc_type.to_string()),
                    },
                )
                .await
                .is_ok()
        }
    };

    if let Some(doc_id) = previous
        .as_ref()
        .and_then(|entry| Uuid::parse_str(&entry.id).ok())
    {
        if update(doc_id).await {
            record_entry(&state_path, &key, &doc_id.to_string(), &sha);
            return Some(DocMirror {
                doc_id: doc_id.to_string(),
                doc_type,
                outcome: "updated",
            });
        }
    }

    // Another machine may already mirror this file under the same title.
    if let Ok(listed) = client
        .list_docs(
            scope.workspace_id,
            scope.project_id,
            Some(doc_type.to_string()),
            None,
            Some(title.clone()),
            Some(5),
        )
        .await
    {
        if let Some(doc_id) = list_items(&listed)
            .iter()
            .find(|item| same_title(item, &title))
            .and_then(response_id)
        {
            if update(doc_id).await {
                record_entry(&state_path, &key, &doc_id.to_string(), &sha);
                return Some(DocMirror {
                    doc_id: doc_id.to_string(),
                    doc_type,
                    outcome: "updated",
                });
            }
        }
    }

    let created = client
        .create_doc(mcp_client::CreateDocParams {
            title,
            content,
            doc_type: Some(doc_type.to_string()),
            metadata: Some(serde_json::json!({
                "source": "repository_mirror",
                "repository": repository,
                "path": relative,
            })),
            workspace_id: scope.workspace_id,
            project_id: scope.project_id,
            is_personal: None,
        })
        .await
        .ok()?;
    let doc_id = response_id(&created)?.to_string();
    record_entry(&state_path, &key, &doc_id, &sha);
    Some(DocMirror {
        doc_id,
        doc_type,
        outcome: "created",
    })
}

/// Mirror a repository doc to ContextStream after it was written. Returns
/// `None` when nothing was sent (unchanged, too large, no credentials, error).
pub async fn mirror_repo_doc(
    absolute_path: &Path,
    doc_type: &'static str,
    host_session_id: Option<&str>,
) -> Option<DocMirror> {
    tokio::time::timeout(
        CAPTURE_DEADLINE,
        mirror_doc_inner(absolute_path, doc_type, host_session_id),
    )
    .await
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAN: &str = "# MCP speed + quality pass\n\n## Context\n\nTwo asks: faster MCP and ContextStream-first storage.\n\n## Phase 1 — Persistence routing\n\n1. **Claude auto memory off, plus redirect.**\n   - Setup sets autoMemoryEnabled.\n   - PreToolUse denies memory writes.\n2. **Plan mode becomes deterministic.** Capture on ExitPlanMode.\n\n## Phase 2 — Tokens\n\n3. Compact rules for hook clients. Keep Tier B/C.\n\n## Verification\n\n- cargo test\n- real session\n";

    #[test]
    fn plan_markdown_yields_title_context_and_numbered_steps() {
        let plan = parse_plan_markdown(PLAN);
        assert_eq!(plan.title, "MCP speed + quality pass");
        assert_eq!(
            plan.description,
            "Two asks: faster MCP and ContextStream-first storage."
        );
        let titles: Vec<&str> = plan.steps.iter().map(|step| step.title.as_str()).collect();
        assert_eq!(
            titles,
            vec![
                "Claude auto memory off, plus redirect",
                "Plan mode becomes deterministic",
                "Compact rules for hook clients",
            ]
        );
        assert!(plan.steps[0]
            .description
            .starts_with("[Phase 1 — Persistence routing]"));
        assert!(plan.steps[0]
            .description
            .contains("PreToolUse denies memory writes."));
        assert!(!plan.steps[1].description.contains("Compact rules"));
        assert!(!plan.steps[2].description.contains("cargo test"));
    }

    #[test]
    fn plans_without_numbered_steps_fall_back_to_sections() {
        let plan = parse_plan_markdown(
            "# Fix login\n\n## Context\nBroken redirect.\n\n## Change\nEdit auth.rs.\n\n## Verify\nRun tests.\n",
        );
        let titles: Vec<&str> = plan.steps.iter().map(|step| step.title.as_str()).collect();
        assert_eq!(titles, vec!["Change", "Verify"]);
        assert_eq!(plan.steps[0].description, "Edit auth.rs.");

        let bare = parse_plan_markdown("Just do the thing.");
        assert_eq!(bare.title, "Approved plan");
        assert_eq!(bare.steps.len(), 1);
    }

    #[test]
    fn mirror_state_is_locked_bounded_and_atomic() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("doc-mirrors.json");
        record_entry(&path, "a", "id-a", "sha-a");
        record_entry(&path, "b", "id-b", "sha-b");
        record_entry(&path, "a", "id-a", "sha-a2");
        assert_eq!(
            read_entry(&path, "a").map(|entry| entry.sha256).as_deref(),
            Some("sha-a2")
        );
        assert_eq!(
            read_entry(&path, "b").map(|entry| entry.id).as_deref(),
            Some("id-b")
        );
        let names: Vec<String> = std::fs::read_dir(temp.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
            .collect();
        assert!(
            names.iter().all(|name| !name.ends_with(".tmp")),
            "{names:?}"
        );
    }

    #[test]
    fn session_scopes_apply_only_to_related_fresh_folders() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("session-scopes.json");
        let workspace = Uuid::new_v4();
        let project = Uuid::new_v4();
        record_session_scope_at(&path, "host-1", "/repo/app/", workspace, Some(project));
        assert_eq!(
            session_scope_at(&path, "host-1", "/repo/app"),
            Some((workspace, Some(project)))
        );
        assert_eq!(
            session_scope_at(&path, "host-1", "/repo/app/docs"),
            Some((workspace, Some(project)))
        );
        assert_eq!(
            session_scope_at(&path, "host-1", "/repo"),
            Some((workspace, Some(project)))
        );
        assert_eq!(session_scope_at(&path, "host-1", "/repo/application"), None);
        assert_eq!(session_scope_at(&path, "host-1", "/other"), None);
        assert_eq!(session_scope_at(&path, "host-2", "/repo/app"), None);
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            !raw.contains("host-1"),
            "host session ids are stored hashed"
        );
    }

    #[test]
    fn pending_docs_are_queued_once_per_session_and_taken_once() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("pending-doc-mirrors.json");
        queue_pending_doc_at(&path, "host-1", Path::new("/repo/docs/runbooks/a.md"));
        queue_pending_doc_at(&path, "host-1", Path::new("/repo/docs/runbooks/a.md"));
        queue_pending_doc_at(&path, "host-2", Path::new("/repo/notes/b.md"));
        assert_eq!(
            take_pending_docs_at(&path, "host-1"),
            vec![PathBuf::from("/repo/docs/runbooks/a.md")]
        );
        assert!(take_pending_docs_at(&path, "host-1").is_empty());
        assert_eq!(take_pending_docs_at(&path, "host-2").len(), 1);
    }

    #[test]
    fn doc_titles_name_the_repository_and_path() {
        assert_eq!(
            doc_title(
                "# Ops VM runbook\n\nSteps",
                "ops-vm",
                "acme/ops",
                "docs/runbooks/ops-vm.md"
            ),
            "Ops VM runbook (acme/ops/docs/runbooks/ops-vm.md)"
        );
        assert_eq!(
            doc_title("no heading", "ops-vm", "repo", "notes/ops-vm.md"),
            "ops-vm (repo/notes/ops-vm.md)"
        );
    }

    #[test]
    fn list_items_reads_the_common_envelopes() {
        let id = Uuid::new_v4().to_string();
        for value in [
            serde_json::json!({"items": [{"id": id}]}),
            serde_json::json!({"plans": [{"id": id}]}),
            serde_json::json!({"data": {"docs": [{"id": id}]}}),
            serde_json::json!([{"id": id}]),
        ] {
            let items = list_items(&value);
            assert_eq!(
                response_id(&items[0]).map(|id| id.to_string()),
                Some(id.clone())
            );
        }
    }
}
