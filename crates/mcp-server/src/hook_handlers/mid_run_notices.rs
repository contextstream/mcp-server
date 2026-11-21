//! Mid-run delivery of coordination notices (plus new lessons and
//! preferences) to long-running sessions, headless ones included, between
//! prompts.
//!
//! Tool-call hooks (Claude Code and Codex `PreToolUse`, Antigravity
//! `PreInvocation`) call [`notice_for_hook`], which never touches the network:
//! - Each session has a small state file in `~/.contextstream/mid-run-notices/`.
//!   Most calls read it and return `None`.
//! - Notices a background poll fetched wait in that file's `pending` list; the
//!   next hook takes them under the file's lock and shows them, so each is
//!   shown exactly once.
//! - At most every 35 s per session (`CONTEXTSTREAM_NOTICE_POLL_SECS`,
//!   5..=300) the hook starts a detached `contextstream-mcp hook mid-run-poll`
//!   process and returns at once. That process calls `POST /api/v1/context/hook`
//!   with `hook_type = "mid_run_poll"` and `consume_notices = true` (the API
//!   acknowledges what it returns), keeps what is new, and adds it to `pending`.
//! - A session working from a scratch or temp folder is scoped to the
//!   repository its tool calls, prompt, or `brief.md` point at, never to a
//!   project made for the scratch folder.
//!
//! - The same detached process asks what a person answered or wrote to the
//!   runs the session reported with `progress`
//!   (`GET /api/v1/decision-requests/deliveries`, see [`check_deliveries`]),
//!   and leaves a pointer in `pending`: that something waits, for which run,
//!   and the `progress` call that reads it. The words themselves are shown
//!   where the delivery is claimed, by that call; the hooks claim nothing.
//!
//! `CONTEXTSTREAM_MID_RUN_NOTICES=0` turns delivery off.
//!
//! The same state file also holds what the prompt hook needs to show its
//! standing text once per window instead of with every prompt (see
//! [`throttle_prompt_standing_text`]), and a coordination notice in full once
//! and then as a count. That part does not depend on the switch above: its own
//! switches are `CONTEXTSTREAM_HOOK_STANDING_REPEAT_SECS` (`0` repeats
//! everything with every prompt) and `CONTEXTSTREAM_HOOK_NOTICE_REPEAT_SECS`
//! (`0` repeats every pending notice in full with every prompt).

use chrono::Utc;
use mcp_session::auto_init::checkout_binding_workspace;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

use super::common::{self, ApiConfig};
use super::durable_capture::{
    plain_session_id, session_api_session_id, session_primary_scope, sha256_hex, state_dir,
    with_locked_json, with_locked_json_within,
};

const DEFAULT_POLL_INTERVAL_SECS: u64 = 35;
const MIN_POLL_INTERVAL_SECS: u64 = 5;
const MAX_POLL_INTERVAL_SECS: u64 = 300;
/// Session state files untouched this long are removed by the next poll.
const MAX_SESSION_AGE: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_DELIVERED_IDS: usize = 256;
const MAX_SEEN_STATIC_LINES: usize = 256;
/// Notices waiting for the next hook; older ones give way to newer.
const MAX_PENDING_NOTICES: usize = 16;
/// How long the prompt hook leaves out standing lines it already showed.
const DEFAULT_STANDING_REPEAT_SECS: u64 = 600;
/// A session's state file is removed after `MAX_SESSION_AGE`, so no window
/// outlives it.
const MAX_STANDING_REPEAT_SECS: u64 = 24 * 60 * 60;
/// Standing lines remembered per window. Past this the oldest are forgotten,
/// and a forgotten line is simply shown again.
const MAX_STANDING_LINE_HASHES: usize = 256;
/// How long the prompt hook only counts a coordination notice it printed in
/// full: a notice still unacknowledged after this is printed in full again.
const DEFAULT_NOTICE_REPEAT_SECS: u64 = 30 * 60;
/// How long the prompt hook waits for another hook's lock on the session
/// state before it gives up and delivers the whole text: well inside the
/// hook's 5 s limit.
const STANDING_LOCK_WAIT: Duration = Duration::from_secs(1);
/// Notice ids the prompt hook's one-line count spells out; the rest are counted.
const MAX_LISTED_UNACKED_NOTICES: usize = 5;
/// The background poll is off the hook's path, so it can wait for a slow API.
const POLL_HTTP_TIMEOUT: Duration = Duration::from_secs(8);
const ACK_HTTP_TIMEOUT: Duration = Duration::from_secs(3);
/// Hook name of the detached poll process.
pub(crate) const BACKGROUND_POLL_HOOK: &str = "mid-run-poll";
/// Runs of a session the poll asks about; the ones reported longest ago
/// give way.
const MAX_REPORTED_RUNS: usize = 8;
/// How long an API that answered 404 for deliveries is not asked again.
const DELIVERIES_NOT_THERE_MS: u64 = 10 * 60 * 1000;
/// Once-per-session reminder tokens remembered per session. Past this the
/// oldest are forgotten, and a forgotten reminder may be shown once more.
const MAX_SHOWN_ONCE: usize = 64;
/// Titles of a session's own saves remembered; older ones give way.
const MAX_SELF_AUTHORED_TITLES: usize = 64;
/// A saved title shorter than this would match unrelated lines, so it is not
/// remembered.
const MIN_SELF_AUTHORED_TITLE_CHARS: usize = 6;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct MidRunSessionState {
    #[serde(default)]
    pub last_poll_unix_ms: u64,
    /// The first poll sends the session's task summary; later ones do not.
    #[serde(default)]
    pub checked_in_summary: bool,
    /// Lessons and preferences seen once are a baseline, not news.
    #[serde(default)]
    pub baselined_static: bool,
    #[serde(default)]
    pub delivered_notice_ids: Vec<String>,
    #[serde(default)]
    pub seen_lesson_lines: Vec<String>,
    #[serde(default)]
    pub seen_preference_lines: Vec<String>,
    /// Rendered notices fetched by a background poll, waiting for a hook.
    #[serde(default)]
    pub pending: Vec<String>,
    /// When the prompt hook last delivered its standing text in full. Zero:
    /// the next prompt is delivered in full (a new session, or one whose
    /// context was just compacted).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub standing_window_started_unix_ms: u64,
    /// Short hashes of the standing lines the prompt hook delivered in this
    /// window (at most `MAX_STANDING_LINE_HASHES`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub standing_line_hashes: Vec<String>,
    /// When the prompt hook last printed each `[COORDINATION]` notice in full,
    /// by notice id (`id:<id>`) and by its item together with its words
    /// (`item:<uuid>:<hash of the words without notice ids>`); at most
    /// `MAX_DELIVERED_IDS`. Not tied to the standing window.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub standing_notice_copies: Vec<NoticeCopy>,
    /// The folder the hooks ran in, which the MCP server compares with its
    /// own to tell whether hooks deliver notices for its session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_repo_root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_name: Option<String>,
    /// Runs the session reported with `progress` and has not finished: the
    /// poll asks what a person sent each (at most `MAX_REPORTED_RUNS`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reported_runs: Vec<ReportedRun>,
    /// Deliveries the session was pointed at, by id and how many claims on
    /// it had lapsed (`<id>#<n>`), and deliveries a `progress` answer showed
    /// (`<id>#tool`); at most `MAX_DELIVERED_IDS`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub noticed_deliveries: Vec<String>,
    /// The API said it has no delivery routes: not asked again before this.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub deliveries_not_there_until_unix_ms: u64,
    /// Reminders a hook shows at most once per session, by token (at most
    /// `MAX_SHOWN_ONCE`); see [`claim_once_per_session`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shown_once: Vec<String>,
    /// Titles of the lessons, preferences and decisions this session saved
    /// itself, lowercased with whitespace collapsed (at most
    /// `MAX_SELF_AUTHORED_TITLES`): a poll that returns one does not announce
    /// it as new.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub self_authored_titles: Vec<String>,
    #[serde(default)]
    pub updated_at: String,
}

/// A run the session reported with `progress`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ReportedRun {
    pub run_id: String,
    /// The workspace the report named; the session's own when it named none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
}

/// The last full copy of a coordination notice the prompt hook printed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct NoticeCopy {
    pub token: String,
    #[serde(default)]
    pub at_unix_ms: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedHookScope {
    pub config: ApiConfig,
    pub effective_folder_path: String,
    pub agent_name: Option<String>,
    pub coordination_session_id: Option<String>,
    pub host_session_id: Option<String>,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

fn sessions_dir() -> Option<PathBuf> {
    state_dir().map(|dir| dir.join("mid-run-notices"))
}

fn session_path(key: &str) -> Option<PathBuf> {
    // Keys are sha256 hex; anything else never names a file.
    (key.len() == 64 && key.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| sessions_dir().map(|dir| dir.join(format!("{key}.json"))))
        .flatten()
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub(crate) fn poll_interval_ms() -> u64 {
    let secs = std::env::var("CONTEXTSTREAM_NOTICE_POLL_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .map(|s| s.clamp(MIN_POLL_INTERVAL_SECS, MAX_POLL_INTERVAL_SECS))
        .unwrap_or(DEFAULT_POLL_INTERVAL_SECS);
    secs * 1_000
}

pub(crate) fn mid_run_notices_enabled() -> bool {
    !std::env::var("CONTEXTSTREAM_MID_RUN_NOTICES").is_ok_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        )
    }) && !std::env::var("CONTEXTSTREAM_REMINDER_ENABLED")
        .is_ok_and(|value| value.trim().eq_ignore_ascii_case("false"))
}

fn push_bounded(vec: &mut Vec<String>, item: String, max_len: usize) {
    if !vec.contains(&item) {
        if vec.len() >= max_len {
            let drain = vec.len() + 1 - max_len;
            vec.drain(0..drain);
        }
        vec.push(item);
    }
}

pub(crate) fn session_state_key(input: &Value, cwd: &str) -> String {
    if let Some(host_key) = common::host_session_key(input) {
        sha256_hex(format!("session:{host_key}").as_bytes())
    } else {
        sha256_hex(format!("cwd:{}", cwd.trim_end_matches('/')).as_bytes())
    }
}

fn read_session_entry(path: &Path) -> Option<MidRunSessionState> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn read_session_entry_fast(key: &str) -> Option<MidRunSessionState> {
    read_session_entry(&session_path(key)?)
}

fn update_session_entry<T>(
    key: &str,
    update: impl FnOnce(&mut MidRunSessionState) -> T,
) -> Option<T> {
    let path = session_path(key)?;
    with_locked_json(&path, |entry: &mut MidRunSessionState| {
        let result = update(entry);
        entry.updated_at = Utc::now().to_rfc3339();
        result
    })
}

/// True the first time this session claims `token`, false every time after,
/// so a reminder shown under it appears at most once per session. False too
/// when the session state cannot be written: without a record, the reminder
/// would otherwise come back with every call.
pub(crate) fn claim_once_per_session(input: &Value, cwd: &str, token: &str) -> bool {
    let key = session_state_key(input, cwd);
    if read_session_entry_fast(&key)
        .is_some_and(|entry| entry.shown_once.iter().any(|t| t == token))
    {
        return false;
    }
    update_session_entry(&key, |entry| claim_once(entry, token)).unwrap_or(false)
}

fn claim_once(entry: &mut MidRunSessionState, token: &str) -> bool {
    if entry.shown_once.iter().any(|shown| shown == token) {
        return false;
    }
    push_bounded(&mut entry.shown_once, token.to_string(), MAX_SHOWN_ONCE);
    true
}

/// Remember that this session saved a lesson, preference or decision under
/// `title`, so a later poll that returns it does not play it back as news.
pub(crate) fn record_self_authored_title(input: &Value, cwd: &str, title: &str) {
    if !mid_run_notices_enabled() {
        return;
    }
    let key = session_state_key(input, cwd);
    let _ = update_session_entry(&key, |entry| remember_self_authored_title(entry, title));
}

fn remember_self_authored_title(entry: &mut MidRunSessionState, title: &str) {
    let normalized = normalize_for_match(title);
    if normalized.chars().count() >= MIN_SELF_AUTHORED_TITLE_CHARS {
        push_bounded(
            &mut entry.self_authored_titles,
            normalized,
            MAX_SELF_AUTHORED_TITLES,
        );
    }
}

/// Lowercase, with every run of whitespace as one space.
fn normalize_for_match(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// True when a lesson or preference line names something this session saved
/// itself.
fn is_self_authored(entry: &MidRunSessionState, block: &str) -> bool {
    if entry.self_authored_titles.is_empty() {
        return false;
    }
    let line = normalize_for_match(block);
    entry
        .self_authored_titles
        .iter()
        .any(|title| line.contains(title.as_str()))
}

/// True when `path` is a scratch, temp, or agent-harness state directory that
/// must never auto-create or bind its own ContextStream project.
pub fn is_scratch_or_temp_path(path: &str) -> bool {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return false;
    }
    if mcp_session::project_link::is_generated_path(trimmed) {
        return true;
    }
    let normalized = trimmed.replace('\\', "/");
    let lower = normalized.trim_end_matches('/').to_ascii_lowercase();
    for prefix in [
        "/tmp/",
        "/private/tmp/",
        "/var/tmp/",
        "/private/var/",
        "/dev/shm/",
    ] {
        if lower == prefix.trim_end_matches('/') || lower.starts_with(prefix) {
            return true;
        }
    }
    for segment in lower.split('/').filter(|s| !s.is_empty()) {
        if matches!(
            segment,
            "tmp"
                | "temp"
                | "scratch"
                | "scratchpad"
                | ".tmp"
                | ".scratch"
                | "own-actions-runs"
                | ".claude"
                | ".codex"
                | ".gemini"
                | ".cursor"
                | ".contextstream"
        ) {
            return true;
        }
    }
    if let Some(base) = lower.rsplit('/').next() {
        if base.starts_with("tmp.") || base.starts_with("temp.") {
            return true;
        }
        if let Some((stem, suffix)) = base.rsplit_once('.') {
            if stem.starts_with("agent-")
                && (3..=10).contains(&suffix.len())
                && suffix.chars().all(|c| c.is_ascii_alphanumeric())
            {
                return true;
            }
        }
    }
    false
}

/// Normalize an agent handle such as `agent-cc-qa.Cz5v` -> `cc-qa`.
pub use mcp_client::agent_handle::normalize_agent_handle;

fn extract_heading_agent_name(text: &str) -> Option<String> {
    for line in text.lines().take(12) {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("# ") {
            let head = rest.split(':').next().unwrap_or(rest).trim();
            if !head.contains(char::is_whitespace) {
                if let Some(handle) = normalize_agent_handle(head) {
                    return Some(handle);
                }
            }
        }
    }
    None
}

fn read_brief_snippet(cwd: &str) -> Option<String> {
    if cwd.trim().is_empty() {
        return None;
    }
    let brief_path = Path::new(cwd).join("brief.md");
    let content = std::fs::read_to_string(brief_path).ok()?;
    (!content.trim().is_empty()).then_some(content)
}

pub(crate) fn derive_agent_name(input: &Value, cwd: &str) -> Option<String> {
    for env_key in ["CONTEXTSTREAM_AGENT_NAME", "AGENT_NAME"] {
        if let Ok(val) = std::env::var(env_key) {
            if let Some(handle) = normalize_agent_handle(&val) {
                return Some(handle);
            }
        }
    }
    for field in ["agent_name", "agentName"] {
        if let Some(raw) = input.get(field).and_then(Value::as_str) {
            if let Some(handle) = normalize_agent_handle(raw) {
                return Some(handle);
            }
        }
    }
    // Check scratch folder basename (e.g. `.../scratchpad/agent-cc-qa.Cz5v` -> `cc-qa`).
    if let Some(base) = Path::new(cwd.trim_end_matches('/'))
        .file_name()
        .and_then(|n| n.to_str())
    {
        if base.to_ascii_lowercase().starts_with("agent-") {
            if let Some(handle) = normalize_agent_handle(base) {
                return Some(handle);
            }
        }
    }
    for prompt_key in ["prompt", "lastUserInput"] {
        if let Some(prompt) = input.get(prompt_key).and_then(Value::as_str) {
            if let Some(handle) = extract_heading_agent_name(prompt) {
                return Some(handle);
            }
        }
    }
    if let Some(brief) = read_brief_snippet(cwd) {
        if let Some(handle) = extract_heading_agent_name(&brief) {
            return Some(handle);
        }
    }
    None
}

/// Extract `(workspace_id, project_id, repo_root)` from a local non-scratch
/// directory if it (or an ancestor) has `.contextstream/config.json` or an
/// entry in `~/.contextstream/mappings.json`.
fn resolve_local_checkout_binding(candidate_path: &Path) -> Option<(String, String, String)> {
    let start = if candidate_path.is_file() {
        candidate_path.parent()?
    } else {
        candidate_path
    };
    let canonical = std::fs::canonicalize(start).unwrap_or_else(|_| start.to_path_buf());
    let canonical_str = canonical.to_string_lossy().to_string();
    if is_scratch_or_temp_path(&canonical_str)
        && !canonical.join(".contextstream/config.json").is_file()
    {
        return None;
    }

    // 1. Walk up looking for `.contextstream/config.json`.
    let mut dir = canonical.clone();
    for _ in 0..10 {
        let dir_str = dir.to_string_lossy().to_string();
        let cfg_path = dir.join(".contextstream").join("config.json");
        if let Ok(raw) = std::fs::read_to_string(&cfg_path) {
            if let Ok(cfg) = serde_json::from_str::<Value>(&raw) {
                let proj = cfg
                    .get("project_id")
                    .and_then(Value::as_str)
                    .and_then(|s| Uuid::parse_str(s.trim()).ok());
                let ws = cfg
                    .get("workspace_id")
                    .and_then(Value::as_str)
                    .and_then(|s| Uuid::parse_str(s.trim()).ok())
                    .or_else(|| proj.and_then(|pid| checkout_binding_workspace(&dir_str, pid)));
                if let (Some(ws_id), Some(proj_id)) = (ws, proj) {
                    return Some((ws_id.to_string(), proj_id.to_string(), dir_str));
                }
            }
        }
        if !dir.pop() {
            break;
        }
    }

    // 2. Match against `~/.contextstream/mappings.json`.
    if let Some(mappings_dir) = state_dir() {
        if let Ok(raw) = std::fs::read_to_string(mappings_dir.join("mappings.json")) {
            if let Ok(parsed) = serde_json::from_str::<Value>(&raw) {
                let entries = parsed
                    .get("mappings")
                    .and_then(Value::as_array)
                    .or_else(|| parsed.as_array());
                if let Some(arr) = entries {
                    let mut best: Option<(usize, String, String, String)> = None;
                    for item in arr {
                        let Some(mapped_path) = item.get("path").and_then(Value::as_str) else {
                            continue;
                        };
                        let mapped_norm = mapped_path.trim_end_matches('/');
                        if mapped_norm.is_empty() || is_scratch_or_temp_path(mapped_norm) {
                            continue;
                        }
                        if canonical_str == mapped_norm
                            || canonical_str.starts_with(&format!("{mapped_norm}/"))
                        {
                            let ws = item
                                .get("workspace_id")
                                .and_then(Value::as_str)
                                .and_then(|s| Uuid::parse_str(s.trim()).ok());
                            let proj = item
                                .get("project_id")
                                .and_then(Value::as_str)
                                .and_then(|s| Uuid::parse_str(s.trim()).ok());
                            if let (Some(ws_id), Some(proj_id)) = (ws, proj) {
                                if best.as_ref().is_none_or(|b| mapped_norm.len() > b.0) {
                                    best = Some((
                                        mapped_norm.len(),
                                        ws_id.to_string(),
                                        proj_id.to_string(),
                                        mapped_norm.to_string(),
                                    ));
                                }
                            }
                        }
                    }
                    if let Some((_, ws_id, proj_id, root)) = best {
                        return Some((ws_id, proj_id, root));
                    }
                }
            }
        }
    }

    None
}

fn expand_tilde(token: &str) -> PathBuf {
    if let Some(rest) = token.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(token)
}

/// Resolve a repo short name (from `--repo <name>` or `owner/<name>`) against
/// checkouts mapped in `~/.contextstream/mappings.json`, then common developer
/// directories (`~/dev/<name>`, `~/code/<name>`, `~/src/<name>`, `~/<name>`,
/// `/workspace/<name>`).
fn resolve_repo_name_locally(repo_name: &str) -> Option<(String, String, String)> {
    let target = repo_name.trim().trim_matches('/').to_ascii_lowercase();
    if target.is_empty() {
        return None;
    }
    let short_name = target.rsplit('/').next().unwrap_or(&target);

    if let Some(mappings_dir) = state_dir() {
        if let Ok(raw) = std::fs::read_to_string(mappings_dir.join("mappings.json")) {
            if let Ok(parsed) = serde_json::from_str::<Value>(&raw) {
                if let Some(arr) = parsed
                    .get("mappings")
                    .and_then(Value::as_array)
                    .or_else(|| parsed.as_array())
                {
                    let mut matches = Vec::new();
                    for item in arr {
                        let Some(mapped_path) = item.get("path").and_then(Value::as_str) else {
                            continue;
                        };
                        if is_scratch_or_temp_path(mapped_path) {
                            continue;
                        }
                        let base = mapped_path
                            .trim_end_matches('/')
                            .rsplit('/')
                            .next()
                            .unwrap_or("")
                            .to_ascii_lowercase();
                        let pname = item
                            .get("project_name")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_ascii_lowercase();
                        if base == short_name || pname == short_name {
                            if let Some(found) =
                                resolve_local_checkout_binding(Path::new(mapped_path))
                            {
                                matches.push(found);
                            }
                        }
                    }
                    matches.dedup_by(|a, b| a.1 == b.1);
                    if matches.len() == 1 {
                        return matches.into_iter().next();
                    }
                }
            }
        }
    }

    let mut candidate_dirs = vec![PathBuf::from("/workspace").join(short_name)];
    if let Some(home) = dirs::home_dir() {
        for dir in ["dev", "code", "src"] {
            candidate_dirs.push(home.join(dir).join(short_name));
        }
        candidate_dirs.push(home.join(short_name));
    }
    for dir in candidate_dirs {
        if dir.is_dir() {
            if let Some(found) = resolve_local_checkout_binding(&dir) {
                return Some(found);
            }
        }
    }
    None
}

/// Scan free-form text (a shell command, prompt, or `brief.md`) for filesystem
/// paths or `--repo <name>` / `owner/repo` hints that resolve to a local bound
/// repository checkout.
fn discover_repo_binding_from_text(text: &str) -> Option<(String, String, String)> {
    let tokens: Vec<&str> = text.split_whitespace().collect();
    for (idx, raw_tok) in tokens.iter().enumerate() {
        let tok = raw_tok.trim_matches(|c: char| {
            matches!(
                c,
                '"' | '\'' | '`' | '(' | ')' | '[' | ']' | ',' | ';' | ':'
            )
        });
        if tok.is_empty() {
            continue;
        }
        if tok == "--repo" {
            if let Some(next) = tokens.get(idx + 1) {
                let clean = next.trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | ',' | ';'));
                if let Some(found) = resolve_repo_name_locally(clean) {
                    return Some(found);
                }
            }
            continue;
        }
        if let Some(repo) = tok.strip_prefix("--repo=") {
            if let Some(found) = resolve_repo_name_locally(repo) {
                return Some(found);
            }
            continue;
        }
        if tok.starts_with('/') || tok.starts_with("~/") {
            let candidate = expand_tilde(tok);
            if candidate.exists() {
                if let Some(found) = resolve_local_checkout_binding(&candidate) {
                    return Some(found);
                }
            }
        } else if let Some((org, repo)) = tok.split_once('/') {
            if !org.is_empty()
                && !repo.is_empty()
                && !repo.contains('/')
                && org
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
                && repo
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            {
                if let Some(found) = resolve_repo_name_locally(repo) {
                    return Some(found);
                }
            }
        }
    }
    None
}

fn discover_repo_binding_from_tool_args(
    cwd: &str,
    tool_args: &Value,
) -> Option<(String, String, String)> {
    // 1. Direct path arguments across Claude Code, Codex, Cursor, and Antigravity.
    for key in [
        "file_path",
        "path",
        "Path",
        "TargetFile",
        "DirectoryPath",
        "SearchPath",
        "workdir",
        "cwd",
    ] {
        if let Some(raw) = tool_args.get(key).and_then(Value::as_str) {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                let candidate = if trimmed.starts_with('/') || trimmed.starts_with("~/") {
                    expand_tilde(trimmed)
                } else {
                    Path::new(cwd).join(trimmed)
                };
                if candidate.exists() {
                    if let Some(found) = resolve_local_checkout_binding(&candidate) {
                        return Some(found);
                    }
                }
            }
        }
    }
    if let Some(paths) = tool_args.get("paths").and_then(Value::as_array) {
        for item in paths.iter().filter_map(Value::as_str) {
            let candidate = expand_tilde(item.trim());
            if candidate.exists() {
                if let Some(found) = resolve_local_checkout_binding(&candidate) {
                    return Some(found);
                }
            }
        }
    }
    // 2. Shell command strings (`command` in Claude/Codex, `CommandLine` in Antigravity).
    for cmd_key in ["command", "CommandLine"] {
        if let Some(cmd_val) = tool_args.get(cmd_key) {
            let cmd_text = match cmd_val {
                Value::String(s) => Some(s.clone()),
                Value::Array(arr) => {
                    let parts: Vec<&str> = arr.iter().filter_map(Value::as_str).collect();
                    (!parts.is_empty()).then(|| parts.join(" "))
                }
                _ => None,
            };
            if let Some(text) = cmd_text {
                if let Some(found) = discover_repo_binding_from_text(&text) {
                    return Some(found);
                }
            }
        }
    }
    None
}

/// Record the repository a scratch-folder session works in, read from one
/// tool call's arguments. The first repository found sticks. Sessions in a
/// real folder return at once; scoped ones cost one small file read.
pub(crate) fn observe_tool_args(input: &Value, cwd: &str, tool_args: &Value) {
    if !mid_run_notices_enabled() || !is_scratch_or_temp_path(cwd) {
        return;
    }
    let key = session_state_key(input, cwd);
    if read_session_entry_fast(&key).is_some_and(|entry| entry.observed_project_id.is_some()) {
        return;
    }
    let Some((workspace_id, project_id, repo_root)) =
        discover_repo_binding_from_tool_args(cwd, tool_args)
    else {
        return;
    };
    let _ = update_session_entry(&key, |entry| {
        if entry.observed_project_id.is_none() {
            entry.observed_workspace_id = Some(workspace_id);
            entry.observed_project_id = Some(project_id);
            entry.observed_repo_root = Some(repo_root);
        }
    });
}

/// Record what a session start or prompt says about the session: the
/// repository it names and the agent's handle.
pub(crate) fn observe_session_context(input: &Value, cwd: &str) {
    if !mid_run_notices_enabled() {
        return;
    }
    let mut discovered = if !is_scratch_or_temp_path(cwd) {
        resolve_local_checkout_binding(Path::new(cwd))
    } else {
        None
    };
    if discovered.is_none() {
        for key in ["prompt", "lastUserInput"] {
            if let Some(text) = input.get(key).and_then(Value::as_str) {
                discovered = discover_repo_binding_from_text(text);
                if discovered.is_some() {
                    break;
                }
            }
        }
    }
    if discovered.is_none() {
        if let Some(brief) = read_brief_snippet(cwd) {
            discovered = discover_repo_binding_from_text(&brief);
        }
    }
    let derived_agent = derive_agent_name(input, cwd);
    if discovered.is_none() && derived_agent.is_none() {
        return;
    }
    let key = session_state_key(input, cwd);
    let _ = update_session_entry(&key, |entry| {
        if let Some((workspace_id, project_id, repo_root)) = discovered {
            entry.observed_workspace_id = Some(workspace_id);
            entry.observed_project_id = Some(project_id);
            entry.observed_repo_root = Some(repo_root);
        }
        if derived_agent.is_some() {
            entry.agent_name = derived_agent;
        }
    });
}

/// Resolve the best `(ApiConfig, effective_folder_path, agent_name)` for hook
/// calls, substituting a scratch directory with the observed repo checkout when
/// available.
pub(crate) fn resolve_hook_scope(input: &Value, cwd: &str) -> ResolvedHookScope {
    let host_session_id = common::host_session_key(input);
    let coordination_session_id = host_session_id.as_deref().and_then(session_api_session_id);
    let key = session_state_key(input, cwd);
    let cached = read_session_entry_fast(&key);

    let mut config = common::load_config(cwd);
    let mut effective_folder = cwd.to_string();

    // 1. If the session initialized via MCP (`init`/`context`), trust that scope.
    if config.workspace_id.is_none() || config.project_id.is_none() {
        if let Some((ws, proj)) = host_session_id.as_deref().and_then(session_primary_scope) {
            if config.workspace_id.is_none() {
                config.workspace_id = Some(ws.to_string());
            }
            if config.project_id.is_none() {
                config.project_id = Some(proj.to_string());
            }
        }
    }

    // 2. If `cwd` is a scratch/temp folder (or still has no project), check
    //    observed repo state, prompt/brief text, or non-scratch local bindings.
    if config.project_id.is_none() || is_scratch_or_temp_path(cwd) {
        let mut repo_match = cached.as_ref().and_then(|e| {
            Some((
                e.observed_workspace_id.clone()?,
                e.observed_project_id.clone()?,
                e.observed_repo_root.clone().unwrap_or_default(),
            ))
        });

        if repo_match.is_none() && !is_scratch_or_temp_path(cwd) {
            repo_match = resolve_local_checkout_binding(Path::new(cwd));
        }
        if repo_match.is_none() {
            for prompt_key in ["prompt", "lastUserInput"] {
                if let Some(text) = input.get(prompt_key).and_then(Value::as_str) {
                    repo_match = discover_repo_binding_from_text(text);
                    if repo_match.is_some() {
                        break;
                    }
                }
            }
        }
        if repo_match.is_none() {
            if let Some(brief) = read_brief_snippet(cwd) {
                repo_match = discover_repo_binding_from_text(&brief);
            }
        }

        if let Some((ws_id, proj_id, repo_root)) = repo_match {
            if std::env::var("CONTEXTSTREAM_WORKSPACE_ID").is_err() {
                config.workspace_id = Some(ws_id);
            }
            if std::env::var("CONTEXTSTREAM_PROJECT_ID").is_err() {
                config.project_id = Some(proj_id);
            }
            if !repo_root.is_empty() {
                effective_folder = repo_root;
            }
        } else if config.workspace_id.is_none() {
            // Even when only a single workspace is mapped on this machine, pick
            // up its workspace_id from ~/.contextstream/mappings.json so
            // workspace-targeted / agent-targeted shares reach scratch agents.
            if let Some(ws_id) = single_mapped_workspace_id() {
                config.workspace_id = Some(ws_id);
            }
        }
    }

    let agent_name = cached
        .and_then(|e| e.agent_name)
        .or_else(|| derive_agent_name(input, cwd));

    ResolvedHookScope {
        config,
        effective_folder_path: effective_folder,
        agent_name,
        coordination_session_id,
        host_session_id,
    }
}

fn single_mapped_workspace_id() -> Option<String> {
    let dir = state_dir()?;
    let raw = std::fs::read_to_string(dir.join("mappings.json")).ok()?;
    let parsed: Value = serde_json::from_str(&raw).ok()?;
    let arr = parsed
        .get("mappings")
        .and_then(Value::as_array)
        .or_else(|| parsed.as_array())?;
    let mut workspaces: Vec<String> = arr
        .iter()
        .filter_map(|item| item.get("workspace_id").and_then(Value::as_str))
        .filter_map(|s| Uuid::parse_str(s.trim()).ok().map(|u| u.to_string()))
        .collect();
    workspaces.sort();
    workspaces.dedup();
    (workspaces.len() == 1).then(|| workspaces.remove(0))
}

/// Parse `notice_id="<id>"` out of a `[COORDINATION]` line.
pub fn extract_notice_id_from_line(line: &str) -> Option<String> {
    let marker = "notice_id=\"";
    let start = line.find(marker)? + marker.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    plain_session_id(&rest[..end])
}

/// True when `line` opens a tagged block such as `[COORDINATION]`.
fn starts_with_tag(line: &str) -> bool {
    line.strip_prefix('[')
        .and_then(|rest| rest.split_once(']'))
        .is_some_and(|(tag, _)| {
            !tag.is_empty()
                && tag
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || matches!(c, '_' | ' '))
        })
}

/// Split hook context into tagged blocks; a line without a tag continues the
/// block before it (a notice whose reason spans lines stays one notice).
fn tagged_blocks(raw: &str) -> Vec<String> {
    let mut blocks: Vec<String> = Vec::new();
    for line in raw.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if starts_with_tag(line) {
            blocks.push(line.to_string());
        } else if let Some(block) = blocks.last_mut() {
            block.push(' ');
            block.push_str(line);
        }
    }
    blocks
}

/// Record the `[COORDINATION]`, `[LESSONS_WARNING]`, and `[PREFERENCE]` lines
/// a prompt hook already showed, so a mid-run poll never shows them again.
pub(crate) fn record_prompt_delivery(input: &Value, cwd: &str, context_text: &str) {
    if !mid_run_notices_enabled() {
        return;
    }
    let key = session_state_key(input, cwd);
    let now_ms = now_unix_ms();
    let agent = derive_agent_name(input, cwd);
    let cwd = cwd.to_string();
    let _ = update_session_entry(&key, |entry| {
        entry.last_poll_unix_ms = now_ms;
        entry.checked_in_summary = true;
        entry.cwd = Some(cwd);
        if entry.agent_name.is_none() {
            entry.agent_name = agent;
        }
        for block in tagged_blocks(context_text) {
            if block.starts_with("[COORDINATION]") {
                let id = extract_notice_id_from_line(&block)
                    .unwrap_or_else(|| sha256_hex(block.as_bytes()));
                push_bounded(&mut entry.delivered_notice_ids, id, MAX_DELIVERED_IDS);
            } else if block.starts_with("[LESSONS_WARNING]") {
                push_bounded(&mut entry.seen_lesson_lines, block, MAX_SEEN_STATIC_LINES);
            } else if block.starts_with("[PREFERENCE]") {
                push_bounded(
                    &mut entry.seen_preference_lines,
                    block,
                    MAX_SEEN_STATIC_LINES,
                );
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Prompt hook: standing text once per window
// ---------------------------------------------------------------------------

/// How long the prompt hook leaves out what it showed, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StandingRepeat {
    /// Standing lines: `CONTEXTSTREAM_HOOK_STANDING_REPEAT_SECS` (default
    /// 600). Zero turns the whole throttle off: every prompt gets everything,
    /// notices included.
    pub standing_ms: u64,
    /// The full copy of a coordination notice:
    /// `CONTEXTSTREAM_HOOK_NOTICE_REPEAT_SECS` (default 1800). Zero prints
    /// every pending notice in full with every prompt.
    pub notice_ms: u64,
}

impl StandingRepeat {
    pub(crate) fn from_env() -> Self {
        Self {
            standing_ms: repeat_ms_from_env(
                "CONTEXTSTREAM_HOOK_STANDING_REPEAT_SECS",
                DEFAULT_STANDING_REPEAT_SECS,
            ),
            notice_ms: repeat_ms_from_env(
                "CONTEXTSTREAM_HOOK_NOTICE_REPEAT_SECS",
                DEFAULT_NOTICE_REPEAT_SECS,
            ),
        }
    }
}

/// A number of seconds from the environment, as milliseconds; the default
/// when it is unset or no number.
fn repeat_ms_from_env(name: &str, default_secs: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .map(|secs| secs.min(MAX_STANDING_REPEAT_SECS))
        .unwrap_or(default_secs)
        * 1_000
}

/// Remember `at_unix_ms` as the last full copy of the notice these tokens name.
fn record_notice_copy(entry: &mut MidRunSessionState, tokens: Vec<String>, at_unix_ms: u64) {
    for token in tokens {
        if let Some(copy) = entry
            .standing_notice_copies
            .iter_mut()
            .find(|copy| copy.token == token)
        {
            copy.at_unix_ms = at_unix_ms;
            continue;
        }
        if entry.standing_notice_copies.len() >= MAX_DELIVERED_IDS {
            let excess = entry.standing_notice_copies.len() + 1 - MAX_DELIVERED_IDS;
            entry.standing_notice_copies.drain(0..excess);
        }
        entry
            .standing_notice_copies
            .push(NoticeCopy { token, at_unix_ms });
    }
}

/// True when `line` opens a block of hook text: `[PREFERENCE] ...`, and also
/// `[CONTEXTSTREAM WORKFLOW harness_teaching_v4]`, whose name `starts_with_tag`
/// does not take.
fn opens_standing_block(line: &str) -> bool {
    line.strip_prefix('[')
        .and_then(|rest| rest.split_once(']'))
        .is_some_and(|(tag, _)| {
            tag.len() <= 64
                && tag.starts_with(|c: char| c.is_ascii_uppercase())
                && tag
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | ' ' | '-' | '.'))
        })
}

/// Blocks the prompt hook never leaves out: they are about this prompt, or
/// the API hands them out once.
fn is_per_prompt_block(header: &str) -> bool {
    header.starts_with("[INSTRUCT]") || header.starts_with("[CONTEXTSTREAM_NOTICE]")
}

/// The item a `[COORDINATION]` notice is about, read only where the API
/// writes it: `[COORDINATION] (<urgency>) from <origin>: item=<uuid> ...`.
/// An `item=` anywhere else (quoted in a reason) is not this notice's item.
fn notice_item_from_header(header: &str) -> Option<String> {
    let rest = header
        .trim_start()
        .strip_prefix("[COORDINATION]")?
        .trim_start();
    let rest = match rest.strip_prefix('(') {
        Some(after) => after.split_once(')')?.1.trim_start(),
        None => rest,
    };
    let (origin, after) = rest.strip_prefix("from ")?.split_once(": item=")?;
    if origin.contains(": ") {
        return None;
    }
    let candidate = after.get(..36)?;
    Uuid::parse_str(candidate).ok().map(|item| item.to_string())
}

/// A short hash of a notice's words with every `notice_id="..."` value left
/// out: the same notice re-addressed under a new id reads the same, and a
/// reply or a new verdict on the same item reads new.
fn notice_words_hash(joined: &str) -> String {
    const MARKER: &str = "notice_id=\"";
    let mut words = String::with_capacity(joined.len());
    let mut rest = joined;
    while let Some(start) = rest.find(MARKER) {
        let value_start = start + MARKER.len();
        words.push_str(&rest[..value_start]);
        rest = &rest[value_start..];
        match rest.find('"') {
            Some(end) => rest = &rest[end..],
            None => break,
        }
    }
    words.push_str(rest);
    let mut hash = sha256_hex(words.as_bytes());
    hash.truncate(16);
    hash
}

/// What a standing line is remembered by: the trimmed line and, for a line
/// without a tag of its own, the header it stands under.
fn standing_line_hash(header: &str, line: &str) -> String {
    let mut hash = sha256_hex(format!("{}\n{}", header.trim(), line.trim()).as_bytes());
    hash.truncate(16);
    hash
}

/// A header line (or none, for text before any header) and the untagged lines
/// under it, up to the next header or blank line.
struct StandingBlock<'a> {
    header: Option<&'a str>,
    lines: Vec<&'a str>,
    /// A blank line separated this block from the one before it.
    after_blank: bool,
    /// Lines from `<name>` to `</name>` (the API wraps critical alerts in
    /// `<system-reminder>`): delivered whole or not at all, so a closing tag
    /// never goes out without its opening one.
    wrapped: bool,
}

/// The name in a line that is exactly `<name>`.
fn wrapper_open_name(line: &str) -> Option<&str> {
    let name = line.strip_prefix('<')?.strip_suffix('>')?;
    (!name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')))
    .then_some(name)
}

fn standing_blocks(text: &str) -> Vec<StandingBlock<'_>> {
    let lines: Vec<&str> = text.lines().collect();
    let mut blocks: Vec<StandingBlock<'_>> = Vec::new();
    let mut open = false;
    let mut after_blank = false;
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim();
        index += 1;
        if trimmed.is_empty() {
            open = false;
            after_blank = true;
            continue;
        }
        let wrapped_end = wrapper_open_name(trimmed).and_then(|name| {
            let close = format!("</{name}>");
            lines[index..]
                .iter()
                .position(|later| later.trim() == close)
                .map(|offset| index + offset)
        });
        if let Some(end) = wrapped_end {
            blocks.push(StandingBlock {
                header: None,
                lines: lines[index - 1..=end].to_vec(),
                after_blank,
                wrapped: true,
            });
            index = end + 1;
            open = false;
            after_blank = false;
        } else if opens_standing_block(trimmed) {
            blocks.push(StandingBlock {
                header: Some(line),
                lines: Vec::new(),
                after_blank,
                wrapped: false,
            });
            open = true;
            after_blank = false;
        } else if let Some(block) = blocks.last_mut().filter(|_| open) {
            block.lines.push(line);
        } else {
            blocks.push(StandingBlock {
                header: None,
                lines: vec![line],
                after_blank,
                wrapped: false,
            });
            open = true;
            after_blank = false;
        }
    }
    blocks
}

/// The tokens a `[COORDINATION]` block is remembered by, and its notice id.
fn coordination_tokens(block: &StandingBlock<'_>) -> (Vec<String>, Option<String>) {
    let mut joined = block.header.unwrap_or_default().trim().to_string();
    for line in &block.lines {
        joined.push(' ');
        joined.push_str(line.trim());
    }
    let notice_id = extract_notice_id_from_line(&joined);
    let mut tokens: Vec<String> = Vec::new();
    if let Some(id) = notice_id.as_deref() {
        tokens.push(format!("id:{id}"));
    }
    // The item alone would hide a reply, which keeps the item of the notice it
    // answers: an item stands for a notice only with the same words.
    if let Some(item) = block.header.and_then(notice_item_from_header) {
        tokens.push(format!("item:{item}:{}", notice_words_hash(&joined)));
    }
    if tokens.is_empty() {
        tokens.push(format!("text:{}", sha256_hex(joined.as_bytes())));
    }
    (tokens, notice_id)
}

fn push_standing_block<'a>(out: &mut Vec<&'a str>, after_blank: bool, lines: &[&'a str]) {
    if lines.is_empty() {
        return;
    }
    if after_blank && !out.is_empty() {
        out.push("");
    }
    out.extend_from_slice(lines);
}

/// One line for the notices left out: how many, and how to act on them.
fn unacknowledged_notices_line(count: usize, ids: &[String]) -> String {
    let (noun, verb) = if count == 1 {
        ("notice", "is")
    } else {
        ("notices", "are")
    };
    let mut line =
        format!("[COORDINATION] {count} {noun} shown earlier {verb} still unacknowledged: ");
    let listed: Vec<&String> = ids.iter().take(MAX_LISTED_UNACKED_NOTICES).collect();
    if let Some((first, rest)) = listed.split_first() {
        line.push_str(&format!(
            "ack via coordination(action=\"ack\", notice_id=\"{first}\")"
        ));
        for id in rest {
            line.push_str(&format!(" / \"{id}\""));
        }
        line.push_str(", or read coordination(action=\"inbox\").");
    } else {
        line.push_str("read coordination(action=\"inbox\").");
    }
    line
}

/// What the prompt hook shows of its standing text (workflow block, hook
/// context, lifecycle note) for one prompt, and what it remembers of it.
///
/// - No window open (first prompt of a session, a reset, or `window_ms` since
///   the last full delivery): `text` is returned as it is and a window starts.
/// - Inside a window: a line already delivered in it is left out, by equality
///   of the trimmed line (not by its tag). A line that is new is delivered; a
///   new line without a tag of its own (a numbered step, a `- ` bullet) comes
///   with its header, and a new header comes with its whole block. One line
///   says how much was left out and when it was shown.
/// - A `[COORDINATION]` notice is printed in full once, by notice id or by
///   the item it names with the same words (a reply on the item is new
///   words, so it is printed); after that one line counts the notices still coming
///   back and names their ids. Nothing is acknowledged here. This has its own
///   clock: a full delivery of the standing text does not bring a notice
///   back. A notice still pending `repeat.notice_ms` after its last full copy
///   is printed in full again.
/// - `[INSTRUCT]` and `[CONTEXTSTREAM_NOTICE]` blocks are always delivered.
///
/// The memory is bounded: `MAX_STANDING_LINE_HASHES` lines per window and
/// `MAX_DELIVERED_IDS` notice tokens, the oldest giving way (what is
/// forgotten is shown again), and the state file itself is removed after
/// `MAX_SESSION_AGE` without a hook call, after which the next prompt is full.
pub(crate) fn filter_standing_text(
    entry: &mut MidRunSessionState,
    text: &str,
    now_ms: u64,
    repeat: StandingRepeat,
) -> String {
    let started = entry.standing_window_started_unix_ms;
    let window_open = started != 0 && now_ms >= started && now_ms - started < repeat.standing_ms;
    let blocks = standing_blocks(text);
    if !window_open {
        entry.standing_window_started_unix_ms = now_ms;
        entry.standing_line_hashes.clear();
    }
    // A full copy older than the notice window stands in for nothing.
    entry
        .standing_notice_copies
        .retain(|copy| now_ms.saturating_sub(copy.at_unix_ms) < repeat.notice_ms);
    // What was delivered before this prompt: a line this text holds twice is
    // judged the same way both times.
    let seen_lines: HashSet<String> = entry.standing_line_hashes.iter().cloned().collect();
    let seen_copies: Vec<NoticeCopy> = entry.standing_notice_copies.clone();

    let mut out: Vec<&str> = Vec::new();
    let mut omitted_lines = 0usize;
    let mut unacked_count = 0usize;
    let mut unacked_ids: Vec<String> = Vec::new();

    for block in &blocks {
        let header = block.header.unwrap_or_default();
        let whole: Vec<&str> = block
            .header
            .iter()
            .copied()
            .chain(block.lines.iter().copied())
            .collect();

        if is_per_prompt_block(header.trim()) {
            push_standing_block(&mut out, block.after_blank, &whole);
            continue;
        }

        if header.trim().starts_with("[COORDINATION]") {
            if repeat.notice_ms == 0 {
                push_standing_block(&mut out, block.after_blank, &whole);
                continue;
            }
            let (tokens, notice_id) = coordination_tokens(block);
            // The last full copy of this notice, or of the same words on its item.
            let shown_at = seen_copies
                .iter()
                .filter(|copy| tokens.contains(&copy.token) && now_ms >= copy.at_unix_ms)
                .map(|copy| copy.at_unix_ms)
                .max();
            record_notice_copy(entry, tokens, shown_at.unwrap_or(now_ms));
            if shown_at.is_some() {
                unacked_count += 1;
                if let Some(id) = notice_id.filter(|id| !unacked_ids.contains(id)) {
                    unacked_ids.push(id);
                }
            } else {
                push_standing_block(&mut out, block.after_blank, &whole);
            }
            continue;
        }

        if block.wrapped {
            let region: Vec<&str> = block.lines.iter().map(|line| line.trim()).collect();
            let hash = standing_line_hash("", &region.join("\n"));
            if seen_lines.contains(&hash) {
                omitted_lines += whole.len();
            } else {
                push_standing_block(&mut out, block.after_blank, &whole);
            }
            push_bounded(
                &mut entry.standing_line_hashes,
                hash,
                MAX_STANDING_LINE_HASHES,
            );
            continue;
        }

        let header_hash = block.header.map(|line| standing_line_hash("", line));
        let header_is_new = header_hash
            .as_ref()
            .is_some_and(|hash| !seen_lines.contains(hash));
        let line_hashes: Vec<String> = block
            .lines
            .iter()
            .map(|line| standing_line_hash(header, line))
            .collect();
        let mut keep: Vec<&str> = Vec::new();
        if header_is_new {
            // A header not shown yet brings its whole block.
            keep = whole;
        } else {
            let new_lines: Vec<&str> = block
                .lines
                .iter()
                .zip(&line_hashes)
                .filter(|(_, hash)| !seen_lines.contains(*hash))
                .map(|(line, _)| *line)
                .collect();
            if !new_lines.is_empty() {
                keep.extend(block.header);
                keep.extend(new_lines);
            }
            omitted_lines += whole.len() - keep.len();
        }
        for hash in header_hash.into_iter().chain(line_hashes) {
            push_bounded(
                &mut entry.standing_line_hashes,
                hash,
                MAX_STANDING_LINE_HASHES,
            );
        }
        push_standing_block(&mut out, block.after_blank, &keep);
    }

    if omitted_lines == 0 && unacked_count == 0 {
        // Nothing was left out: the text goes out byte for byte.
        return text.to_string();
    }

    let mut rendered: Vec<String> = Vec::new();
    if omitted_lines > 0 {
        let lines = if omitted_lines == 1 { "line" } else { "lines" };
        let minutes = now_ms.saturating_sub(started) / 60_000;
        let ago = if minutes == 0 {
            "under a minute ago".to_string()
        } else {
            format!("{minutes} min ago")
        };
        rendered.push(format!(
            "[CONTEXTSTREAM] Standing guidance ({omitted_lines} {lines}) was shown {ago} and still applies."
        ));
    }
    rendered.extend(out.into_iter().map(str::to_string));
    if unacked_count > 0 {
        rendered.push(unacknowledged_notices_line(unacked_count, &unacked_ids));
    }
    rendered.join("\n")
}

/// The standing text of a prompt hook's answer, with what this session was
/// already shown in the current window left out (see [`filter_standing_text`]).
///
/// The text comes back unchanged when the throttle is off
/// (`CONTEXTSTREAM_HOOK_STANDING_REPEAT_SECS=0`, which also repeats every
/// notice; `CONTEXTSTREAM_HOOK_NOTICE_REPEAT_SECS=0` repeats only the notices),
/// when the payload names no
/// session (the state would be keyed by folder, which several sessions share),
/// and whenever the state cannot be locked, read or written: a failure never
/// costs the session its guidance. `CONTEXTSTREAM_MID_RUN_NOTICES=0` does not
/// turn this off.
pub(crate) fn throttle_prompt_standing_text(input: &Value, cwd: &str, text: String) -> String {
    let repeat = StandingRepeat::from_env();
    if repeat.standing_ms == 0 || common::host_session_key(input).is_none() {
        return text;
    }
    let key = session_state_key(input, cwd);
    let Some(path) = session_path(&key) else {
        return text;
    };
    // The clock is read under the lock: a prompt that waited for another
    // one's lock must not see that prompt's window as starting in its future.
    let filtered = with_locked_json_within(
        &path,
        STANDING_LOCK_WAIT,
        |entry: &mut MidRunSessionState| {
            let started = entry.standing_window_started_unix_ms;
            let shown = filter_standing_text(entry, &text, now_unix_ms(), repeat);
            entry.updated_at = Utc::now().to_rfc3339();
            (shown, entry.standing_window_started_unix_ms != started)
        },
    );
    match filtered {
        Some((shown, new_window)) => {
            // Old sessions' files are otherwise removed only by the mid-run
            // poll, which `CONTEXTSTREAM_MID_RUN_NOTICES=0` turns off.
            if new_window {
                prune_stale_sessions();
            }
            shown
        }
        None => text,
    }
}

/// Forget what the prompt hook showed this session, so its next prompt is
/// delivered in full, coordination notices included. Called when a session starts, resumes or comes back from
/// a compaction: what was shown earlier is no longer in the model's view.
pub(crate) fn reset_prompt_standing_window(input: &Value, cwd: &str) {
    if common::host_session_key(input).is_none() {
        return;
    }
    let key = session_state_key(input, cwd);
    // A session no prompt hook has throttled has nothing to forget.
    if read_session_entry_fast(&key).is_none_or(|entry| entry.standing_window_started_unix_ms == 0)
    {
        return;
    }
    let _ = update_session_entry(&key, |entry| {
        entry.standing_window_started_unix_ms = 0;
        entry.standing_line_hashes.clear();
        entry.standing_notice_copies.clear();
    });
}

fn initial_task_summary(input: &Value, cwd: &str) -> Option<String> {
    for key in ["prompt", "lastUserInput"] {
        if let Some(raw) = input.get(key).and_then(Value::as_str) {
            let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
            if !collapsed.is_empty() {
                return Some(collapsed.chars().take(500).collect());
            }
        }
    }
    if let Some(brief) = read_brief_snippet(cwd) {
        let collapsed = brief.split_whitespace().collect::<Vec<_>>().join(" ");
        if !collapsed.is_empty() {
            return Some(collapsed.chars().take(500).collect());
        }
    }
    None
}

/// `hook mid-run-notices`: the `PreToolUse` hook setup installs for Codex,
/// which keeps no ContextStream enforcement hooks. Shows waiting notices as
/// `additionalContext` (Codex accepts the Claude Code shape) and otherwise
/// prints an empty result.
///
/// It is the one hook of ContextStream's that Codex runs before a tool call,
/// so a run this session has open hears here that a tool call starts (no
/// network; nothing where live progress from the hooks is off).
pub async fn handle_notice_hook() -> anyhow::Result<()> {
    let input = super::read_stdin_json()?;
    let cwd = common::extract_cwd(&input);
    let tool = input
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    super::run_progress::tool_started(&input, &cwd, tool);
    if let Some(args) = input.get("tool_input") {
        observe_tool_args(&input, &cwd, args);
    }
    match notice_for_hook(&input, &cwd, "codex") {
        Some(notice) => super::write_context_for_input(&input, notice),
        None => super::write_stdout_json(&super::HookOutput::empty()),
    }
}

/// Every string inside a tool result, joined by lines.
fn tool_result_text(value: &Value, out: &mut String) {
    match value {
        Value::String(text) => {
            out.push_str(text);
            out.push('\n');
        }
        Value::Array(items) => items.iter().for_each(|item| tool_result_text(item, out)),
        Value::Object(object) => object.values().for_each(|item| tool_result_text(item, out)),
        _ => {}
    }
}

/// Record the `[COORDINATION]` notices a ContextStream tool result (such as
/// `context` or `init`) already showed, so a mid-run poll that later returns
/// the same notice does not show it a second time.
pub(crate) fn record_tool_result_delivery(input: &Value, cwd: &str, tool_response: &Value) {
    if !mid_run_notices_enabled() {
        return;
    }
    let mut text = String::new();
    tool_result_text(tool_response, &mut text);
    let ids: Vec<String> = tagged_blocks(&text)
        .iter()
        .filter(|block| block.starts_with("[COORDINATION]"))
        .filter_map(|block| extract_notice_id_from_line(block))
        .collect();
    if ids.is_empty() {
        return;
    }
    let key = session_state_key(input, cwd);
    let _ = update_session_entry(&key, |entry| {
        for id in ids {
            push_bounded(&mut entry.delivered_notice_ids, id, MAX_DELIVERED_IDS);
        }
    });
}

/// What a `progress` answer means for the deliveries the poll asks about.
#[derive(Debug, Default, PartialEq, Eq)]
struct ProgressFinding {
    /// A run the session reported, and the workspace the call named.
    reported: Option<ReportedRun>,
    /// That run is over.
    ended: bool,
    /// Deliveries the answer showed the agent.
    shown: Vec<String>,
}

/// The word after `mark` in a tool's text (`run_id=...`, `state=...`).
fn word_after(text: &str, mark: &str) -> Option<String> {
    let rest = text.get(text.find(mark)? + mark.len()..)?;
    let word: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .collect();
    (!word.is_empty()).then_some(word)
}

fn uuid_text(raw: Option<&str>) -> Option<String> {
    Uuid::parse_str(raw?.trim()).ok().map(|id| id.to_string())
}

/// Read a `progress` call and its answer: the hosted gateway answers with a
/// structured part, a local server in words.
fn progress_finding(
    tool_input: &Value,
    answer: &super::run_progress::Answer,
    all_text: &str,
) -> ProgressFinding {
    let mut finding = ProgressFinding::default();
    if !answer.succeeded {
        return finding;
    }
    // Every delivery the answer showed: the tool ends each one's block (an
    // answer's frame, or a message in the server's own wrapper) with one
    // line on how to confirm it, which names the delivery. A person's words
    // are quoted wherever they are shown, so no line of theirs begins so.
    for line in all_text
        .lines()
        .filter(|line| line.starts_with(mcp_tools::domains::run_deliveries::CONFIRM_PREFIX))
    {
        if let Some(id) = uuid_text(word_after(line, "delivery_id=\"").as_deref()) {
            if !finding.shown.contains(&id) {
                finding.shown.push(id);
            }
        }
    }
    let action = tool_input
        .get("action")
        .and_then(Value::as_str)
        .map(|action| action.trim().to_ascii_lowercase())
        .unwrap_or_default();
    if !(action.is_empty() || action == "report") {
        return finding;
    }
    let run = answer.json.and_then(|json| json.get("run"));
    // In words, the run is named on the answer's first line. What follows may
    // quote what a person wrote, which names no run and ends none.
    let text = answer
        .text
        .and_then(|text| text.lines().next())
        .filter(|line| line.starts_with("[PROGRESS] Recorded"));
    let run_id = uuid_text(
        run.and_then(|run| run.get("run_id"))
            .and_then(Value::as_str),
    )
    .or_else(|| {
        uuid_text(
            text.and_then(|text| word_after(text, " run_id="))
                .as_deref(),
        )
    });
    let Some(run_id) = run_id else {
        return finding;
    };
    let state = run
        .and_then(|run| run.get("state"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| text.and_then(|text| word_after(text, " state=")));
    let said_finished = tool_input
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|status| {
            matches!(
                status.trim().to_ascii_lowercase().as_str(),
                "completed" | "partial" | "stopped" | "failed" | "cancelled"
            )
        });
    finding.ended = said_finished
        || matches!(state.as_deref(), Some("finished" | "stopped" | "failed"))
        || run
            .and_then(|run| run.get("liveness"))
            .and_then(Value::as_str)
            == Some("ended");
    finding.reported = Some(ReportedRun {
        run_id,
        workspace_id: uuid_text(tool_input.get("workspace_id").and_then(Value::as_str)).or_else(
            || {
                uuid_text(
                    run.and_then(|run| run.get("workspace_id"))
                        .and_then(Value::as_str),
                )
            },
        ),
    });
    finding
}

fn apply_progress_finding(entry: &mut MidRunSessionState, finding: ProgressFinding) {
    if let Some(run) = finding.reported {
        entry
            .reported_runs
            .retain(|known| known.run_id != run.run_id);
        if !finding.ended {
            entry.reported_runs.push(run);
            let excess = entry.reported_runs.len().saturating_sub(MAX_REPORTED_RUNS);
            entry.reported_runs.drain(0..excess);
        }
    }
    for id in finding.shown {
        // The agent has read it: a pointer at it would only send it back.
        let named = format!("(delivery_id={id})");
        // Old state files can contain several pointers in a single entry.
        entry.pending.retain_mut(|notice| {
            if notice.lines().any(|line| {
                line.starts_with("[CONTEXTSTREAM LIVE NOTICE]") && line.contains(&named)
            }) {
                *notice = notice
                    .lines()
                    .filter(|line| {
                        !(line.starts_with("[CONTEXTSTREAM LIVE NOTICE]") && line.contains(&named))
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
            }
            !notice.is_empty()
        });
        push_bounded(
            &mut entry.noticed_deliveries,
            format!("{id}#tool"),
            MAX_DELIVERED_IDS,
        );
    }
}

/// PostToolUse of ContextStream's `progress` tool: remember the run the
/// session reported, so the poll can ask what a person sent it; forget a run
/// the session finished; and note the deliveries the answer itself showed.
/// Never touches the network.
pub(crate) fn record_progress_answer(
    input: &Value,
    cwd: &str,
    tool_input: &Value,
    tool_response: &Value,
    answer: &super::run_progress::Answer,
) {
    if !mid_run_notices_enabled() {
        return;
    }
    let mut all_text = String::new();
    tool_result_text(tool_response, &mut all_text);
    let finding = progress_finding(tool_input, answer, &all_text);
    if finding == ProgressFinding::default() {
        return;
    }
    let key = session_state_key(input, cwd);
    let _ = update_session_entry(&key, |entry| apply_progress_finding(entry, finding));
}

/// One delivery the poll found waiting.
struct Waiting {
    id: String,
    previous_claims: usize,
    pointer: String,
}

/// Queue a pointer for each delivery the session has not been pointed at.
fn queue_delivery_pointers(entry: &mut MidRunSessionState, found: Vec<Waiting>) {
    for waiting in found {
        let token = format!("{}#{}", waiting.id, waiting.previous_claims);
        if entry.noticed_deliveries.contains(&token) {
            continue;
        }
        push_bounded(&mut entry.noticed_deliveries, token, MAX_DELIVERED_IDS);
        // A `progress` answer showed it while this read was on its way.
        if waiting.previous_claims == 0
            && entry
                .noticed_deliveries
                .contains(&format!("{}#tool", waiting.id))
        {
            continue;
        }
        entry.pending.push(waiting.pointer);
    }
    let excess = entry.pending.len().saturating_sub(MAX_PENDING_NOTICES);
    entry.pending.drain(0..excess);
}

/// Ask what a person answered or wrote to the runs the session reported, and
/// leave a pointer for each delivery the session has not been pointed at.
/// The read counts an offer and changes nothing else: the claim is made by
/// the `progress` call the pointer names, where the words are shown and the
/// claim's token is kept for the reply. An API without the routes (404), or
/// whose database cannot hold deliveries yet (503 `deliveries_not_ready`),
/// is not asked again for ten minutes; any failure says nothing.
async fn check_deliveries(key: &str, editor: &str, config: &ApiConfig) {
    use mcp_client::deliveries::{is_no_way_back, is_not_ready, is_not_there, DeliveryReader};
    use mcp_tools::domains::run_deliveries::{pointer, CLAUDE_TOOL, TOOL};

    let Some(entry) = read_session_entry_fast(key) else {
        return;
    };
    if entry.reported_runs.is_empty() || now_unix_ms() < entry.deliveries_not_there_until_unix_ms {
        return;
    }
    let client = mcp_client::ContextStreamClient::new(mcp_types::Config {
        api_url: config.api_url.clone(),
        api_key: Some(config.api_key.clone()),
        ..Default::default()
    });
    let tool = if editor == "claude" {
        CLAUDE_TOOL
    } else {
        TOOL
    };
    let mut found = Vec::new();
    let mut not_there = false;
    for run in &entry.reported_runs {
        let ids = (
            Uuid::parse_str(&run.run_id).ok(),
            run.workspace_id
                .as_deref()
                .or(config.workspace_id.as_deref())
                .and_then(|id| Uuid::parse_str(id.trim()).ok()),
        );
        let (Some(run_id), Some(workspace_id)) = ids else {
            continue;
        };
        let reader = DeliveryReader {
            workspace_id,
            run_id: Some(run_id),
        };
        match client.read_deliveries(&reader).await {
            Ok(deliveries) => found.extend(deliveries.iter().map(|delivery| Waiting {
                id: delivery.id.to_string(),
                previous_claims: delivery.previous_claims,
                pointer: pointer(delivery, run_id, tool),
            })),
            // No such routes, or not yet; or this folder's key is not one
            // anything comes back to (the client asks nothing under it).
            Err(error)
                if is_not_there(&error) || is_not_ready(&error) || is_no_way_back(&error) =>
            {
                not_there = true;
                break;
            }
            Err(_) => {}
        }
    }
    if found.is_empty() && !not_there {
        return;
    }
    let _ = update_session_entry(key, |entry| {
        if not_there {
            entry.deliveries_not_there_until_unix_ms = now_unix_ms() + DELIVERIES_NOT_THERE_MS;
        }
        queue_delivery_pointers(entry, found);
    });
}

/// Keep what a poll returned that this session has not seen: every new
/// `[COORDINATION]` notice, and lessons or preferences that appeared after
/// the first complete context, except the ones this session saved itself
/// (recorded, never announced). `complete` is false when the API answered
/// from its cache-miss fallback, which leaves lessons and preferences out:
/// those are then neither recorded nor announced. Returns the rendered
/// message (if any) and the notice ids to acknowledge.
pub(crate) fn filter_and_record_polled_context(
    entry: &mut MidRunSessionState,
    raw_context: &str,
    complete: bool,
) -> (Option<String>, Vec<String>) {
    let mut emit_lines: Vec<String> = Vec::new();
    let mut ack_ids: Vec<String> = Vec::new();
    let was_baselined = entry.baselined_static;

    for block in tagged_blocks(raw_context) {
        if block.starts_with("[COORDINATION]") {
            let notice_id = extract_notice_id_from_line(&block);
            let dedup_token = notice_id
                .clone()
                .unwrap_or_else(|| sha256_hex(block.as_bytes()));
            if entry.delivered_notice_ids.contains(&dedup_token) {
                continue;
            }
            push_bounded(
                &mut entry.delivered_notice_ids,
                dedup_token,
                MAX_DELIVERED_IDS,
            );
            if let Some(id) = notice_id {
                ack_ids.push(id);
            }
            emit_lines.push(block);
        } else if !complete {
            continue;
        } else if block.starts_with("[LESSONS_WARNING]") {
            let already_seen = entry.seen_lesson_lines.contains(&block);
            push_bounded(
                &mut entry.seen_lesson_lines,
                block.clone(),
                MAX_SEEN_STATIC_LINES,
            );
            if was_baselined && !already_seen && !is_self_authored(entry, &block) {
                emit_lines.push(block);
            }
        } else if block.starts_with("[PREFERENCE]") {
            let already_seen = entry.seen_preference_lines.contains(&block);
            push_bounded(
                &mut entry.seen_preference_lines,
                block.clone(),
                MAX_SEEN_STATIC_LINES,
            );
            if was_baselined && !already_seen && !is_self_authored(entry, &block) {
                emit_lines.push(block);
            }
        }
    }
    if complete {
        entry.baselined_static = true;
    }

    if emit_lines.is_empty() {
        return (None, ack_ids);
    }
    let mut rendered = String::from(
        "[CONTEXTSTREAM LIVE NOTICE] New while you were working (shown once, already acknowledged):\n",
    );
    rendered.push_str(&emit_lines.join("\n"));
    rendered.push_str("\nRead it now and act on anything above that applies to your current work.");
    (Some(rendered), ack_ids)
}

/// What one hook call does with the session state: take the waiting notices
/// and, when a poll is due, claim it. Returns `(notices, start_poll, first_poll)`.
fn claim_for_hook(
    entry: &mut MidRunSessionState,
    now_ms: u64,
    interval_ms: u64,
    cwd: &str,
) -> (Vec<String>, bool, bool) {
    let notices = std::mem::take(&mut entry.pending);
    let start_poll = poll_is_due(entry, now_ms, interval_ms);
    let first_poll = start_poll && !entry.checked_in_summary;
    if start_poll {
        entry.last_poll_unix_ms = now_ms;
        entry.checked_in_summary = true;
    }
    entry.cwd = Some(cwd.to_string());
    (notices, start_poll, first_poll)
}

fn poll_is_due(entry: &MidRunSessionState, now_ms: u64, interval_ms: u64) -> bool {
    now_ms.saturating_sub(entry.last_poll_unix_ms) >= interval_ms
}

/// Notices for this hook to show, if a background poll fetched any, and the
/// start of the next background poll when one is due. Never waits on the
/// network: the common case is one small file read.
pub(crate) fn notice_for_hook(input: &Value, cwd: &str, editor: &str) -> Option<String> {
    if !mid_run_notices_enabled() {
        return None;
    }
    let key = session_state_key(input, cwd);
    let now_ms = now_unix_ms();
    let interval_ms = poll_interval_ms();
    if let Some(entry) = read_session_entry_fast(&key) {
        if entry.pending.is_empty() && !poll_is_due(&entry, now_ms, interval_ms) {
            return None;
        }
    }
    let (notices, start_poll, first_poll) = update_session_entry(&key, |entry| {
        claim_for_hook(entry, now_ms, interval_ms, cwd)
    })?;
    if start_poll {
        start_background_poll(input, cwd, &key, editor, first_poll);
    }
    (!notices.is_empty()).then(|| notices.join("\n\n"))
}

fn poll_request_body(
    input: &Value,
    cwd: &str,
    key: &str,
    editor: &str,
    resolved: &ResolvedHookScope,
    first_poll: bool,
) -> Value {
    let mut body = serde_json::Map::new();
    if let Some(ref workspace_id) = resolved.config.workspace_id {
        body.insert("workspace_id".into(), Value::String(workspace_id.clone()));
    }
    if let Some(ref project_id) = resolved.config.project_id {
        body.insert("project_id".into(), Value::String(project_id.clone()));
    }
    let session_id = resolved
        .host_session_id
        .clone()
        .unwrap_or_else(|| format!("cwd-{}", &key[..16]));
    body.insert("session_id".into(), Value::String(session_id));
    if let Some(ref coordination_session_id) = resolved.coordination_session_id {
        body.insert(
            "coordination_session_id".into(),
            Value::String(coordination_session_id.clone()),
        );
    }
    body.insert("hook_type".into(), Value::String("mid_run_poll".into()));
    if !resolved.effective_folder_path.trim().is_empty() {
        body.insert(
            "folder_path".into(),
            Value::String(resolved.effective_folder_path.clone()),
        );
    }
    body.insert("editor".into(), Value::String(editor.to_string()));
    if let Some(ref agent_name) = resolved.agent_name {
        body.insert("agent_name".into(), Value::String(agent_name.clone()));
    }
    if first_poll {
        if let Some(summary) = initial_task_summary(input, cwd) {
            body.insert("task_summary".into(), Value::String(summary));
        }
    }
    body.insert("consume_notices".into(), Value::Bool(true));
    Value::Object(body)
}

/// Hook payload fields the background poll needs to resolve the session's
/// scope, identity, and first task summary.
const POLL_JOB_INPUT_KEYS: &[&str] = &[
    "session_id",
    "sessionId",
    "conversation_id",
    "conversationId",
    "prompt",
    "lastUserInput",
    "agent_name",
    "agentName",
    "cwd",
    "workspacePaths",
];
const POLL_JOB_MAX_FIELD_CHARS: usize = 8_000;

/// Hand the poll to a detached process. Everything that reads files (scope,
/// credentials, agent name) happens there, not on the hook's path.
fn start_background_poll(input: &Value, cwd: &str, key: &str, editor: &str, first_poll: bool) {
    let mut fields = serde_json::Map::new();
    for name in POLL_JOB_INPUT_KEYS {
        let Some(value) = input.get(*name) else {
            continue;
        };
        let value = match value.as_str() {
            Some(text) if text.chars().count() > POLL_JOB_MAX_FIELD_CHARS => {
                Value::String(text.chars().take(POLL_JOB_MAX_FIELD_CHARS).collect())
            }
            _ => value.clone(),
        };
        fields.insert((*name).to_string(), value);
    }
    let job = serde_json::json!({
        "cwd": cwd,
        "state_key": key,
        "editor": editor,
        "first_poll": first_poll,
        "input": Value::Object(fields),
    });
    spawn_poll_process(&job);
}

/// The workspace setup chose for an editor's ContextStream MCP server (its
/// `X-ContextStream-Workspace-Id` header, or `CONTEXTSTREAM_WORKSPACE_ID` in
/// its env), for a session with no linked folder that has not called `init`.
fn configured_mcp_workspace(editor: &str) -> Option<String> {
    let home = dirs::home_dir()?;
    let claude = home.join(".claude.json");
    let antigravity = home.join(".gemini").join("config").join("mcp_config.json");
    let codex = home.join(".codex").join("config.toml");
    let order: [PathBuf; 3] = match editor {
        "antigravity" => [antigravity, claude, codex],
        "codex" => [codex, claude, antigravity],
        _ => [claude, codex, antigravity],
    };
    order.into_iter().find_map(|path| {
        let text = std::fs::read_to_string(&path).ok()?;
        if path.extension().is_some_and(|ext| ext == "toml") {
            workspace_from_codex_toml(&text)
        } else {
            workspace_from_mcp_json(&text)
        }
    })
}

fn workspace_from_mcp_json(text: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(text).ok()?;
    let server = parsed.get("mcpServers")?.get("contextstream")?;
    server
        .get("headers")
        .and_then(|headers| headers.get("X-ContextStream-Workspace-Id"))
        .or_else(|| {
            server
                .get("env")
                .and_then(|env| env.get("CONTEXTSTREAM_WORKSPACE_ID"))
        })
        .and_then(Value::as_str)
        .and_then(|raw| Uuid::parse_str(raw.trim()).ok())
        .map(|id| id.to_string())
}

/// Codex keeps the header inside `[mcp_servers.contextstream]`.
fn workspace_from_codex_toml(text: &str) -> Option<String> {
    let mut in_contextstream = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_contextstream = line
                .trim_matches(|c| c == '[' || c == ']')
                .starts_with("mcp_servers.contextstream");
            continue;
        }
        if !in_contextstream {
            continue;
        }
        for marker in [
            "\"X-ContextStream-Workspace-Id\"",
            "CONTEXTSTREAM_WORKSPACE_ID",
        ] {
            if let Some(at) = line.find(marker) {
                let rest = &line[at + marker.len()..];
                let value = rest.split('"').nth(1)?;
                if let Ok(id) = Uuid::parse_str(value.trim()) {
                    return Some(id.to_string());
                }
            }
        }
    }
    None
}

/// Start `contextstream-mcp hook mid-run-poll` detached, hand it the job on
/// stdin, and return without waiting. Its own process group keeps it alive
/// when the host reaps the hook's group.
fn spawn_poll_process(job: &Value) {
    if cfg!(test) {
        return;
    }
    use std::process::{Command, Stdio};
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut command = Command::new(exe);
    command
        .args(["hook", BACKGROUND_POLL_HOOK])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    let Ok(mut child) = command.spawn() else {
        return;
    };
    if let Some(mut stdin) = child.stdin.take() {
        // A few KB: fits the pipe buffer, so this never waits on the child.
        let _ = serde_json::to_writer(&mut stdin, job);
    }
}

/// The detached poll: fetch, keep what is new for the session, acknowledge.
pub async fn run_background_poll() -> anyhow::Result<()> {
    let job = super::read_stdin_json()?;
    let Some(key) = job.get("state_key").and_then(Value::as_str) else {
        return Ok(());
    };
    let cwd = job.get("cwd").and_then(Value::as_str).unwrap_or_default();
    let editor = job
        .get("editor")
        .and_then(Value::as_str)
        .unwrap_or("claude");
    let first_poll = job
        .get("first_poll")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let input = job
        .get("input")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let mut resolved = resolve_hook_scope(&input, cwd);
    if resolved.config.workspace_id.is_none() {
        resolved.config.workspace_id = configured_mcp_workspace(editor);
    }
    let config = resolved.config.clone();
    if config.is_configured() {
        let body = poll_request_body(&input, cwd, key, editor, &resolved, first_poll);
        if let Some((raw_context, complete)) = fetch_poll_context(&config, &body).await {
            let ack_ids = update_session_entry(key, |entry| {
                let (rendered, ack_ids) =
                    filter_and_record_polled_context(entry, &raw_context, complete);
                if let Some(rendered) = rendered {
                    entry.pending.push(rendered);
                    let excess = entry.pending.len().saturating_sub(MAX_PENDING_NOTICES);
                    entry.pending.drain(0..excess);
                }
                ack_ids
            })
            .unwrap_or_default();
            ack_notices(&config, &ack_ids).await;
        }
        check_deliveries(key, editor, &config).await;
    }
    prune_stale_sessions();
    Ok(())
}

/// The hook context text, and whether it is complete (served from the API's
/// context cache rather than its cache-miss fallback).
async fn fetch_poll_context(config: &ApiConfig, body: &Value) -> Option<(String, bool)> {
    let response = super::api_http_client()
        .post(format!("{}/api/v1/context/hook", config.api_url))
        .header("X-API-Key", &config.api_key)
        .header("Content-Type", "application/json")
        .json(body)
        .timeout(POLL_HTTP_TIMEOUT)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let payload: Value = response.json().await.ok()?;
    hook_context_of(&payload)
}

fn hook_context_of(payload: &Value) -> Option<(String, bool)> {
    let data = payload.get("data")?;
    let context = data.get("context").and_then(Value::as_str)?.to_string();
    let complete = data.get("cached").and_then(Value::as_bool).unwrap_or(false);
    Some((context, complete))
}

/// The API already acknowledged what a `consume_notices` poll returned; this
/// covers an API from before that flag.
async fn ack_notices(config: &ApiConfig, notice_ids: &[String]) {
    for id in notice_ids {
        if Uuid::parse_str(id).is_err() {
            continue;
        }
        let _ = super::api_http_client()
            .post(format!(
                "{}/api/v1/coordination/notices/{id}/ack",
                config.api_url
            ))
            .header("X-API-Key", &config.api_key)
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({}))
            .timeout(ACK_HTTP_TIMEOUT)
            .send()
            .await;
    }
}

/// Remove session state no hook has touched for a day.
fn prune_stale_sessions() {
    let Some(dir) = sessions_dir() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        // Every hook call rewrites the session's JSON, so its age is the
        // session's; a lock file's own age says nothing.
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > MAX_SESSION_AGE);
        if stale {
            let _ = std::fs::remove_file(&path);
            let _ = std::fs::remove_file(path.with_extension("lock"));
        }
    }
}

/// True when hooks running in `cwd` handled mid-run notices within `within`,
/// so an MCP server started there leaves mid-run delivery to them.
pub fn hooks_deliver_notices_in(cwd: &Path, within: Duration) -> bool {
    let Some(dir) = sessions_dir() else {
        return false;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    let wanted = cwd.to_string_lossy();
    let wanted = wanted.trim_end_matches(['/', '\\']);
    let now = SystemTime::now();
    entries.flatten().any(|entry| {
        let path = entry.path();
        let recent = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age <= within);
        recent
            && path.extension().is_some_and(|ext| ext == "json")
            && read_session_entry(&path)
                .and_then(|state| state.cwd)
                .is_some_and(|hook_cwd| hook_cwd.trim_end_matches(['/', '\\']) == wanted)
    })
}

// ---------------------------------------------------------------------------
// MCP server fallback for clients without hooks
// ---------------------------------------------------------------------------

/// Hooks that touched their session state this recently count as live.
const HOOKS_LIVE_WINDOW: Duration = Duration::from_secs(180);
/// ContextStream tools whose results carry coordination notices themselves.
const NOTICE_CARRYING_TOOLS: &[&str] = &["context", "init", "coordination"];

#[derive(Default)]
struct ServerFallbackState {
    session: MidRunSessionState,
    polling: bool,
}

fn server_fallback() -> std::sync::MutexGuard<'static, ServerFallbackState> {
    static STATE: std::sync::OnceLock<std::sync::Mutex<ServerFallbackState>> =
        std::sync::OnceLock::new();
    STATE
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn payload_text(payload: &Value) -> String {
    payload
        .get("content")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// For a client without hooks, the stdio MCP server shows mid-run notices on
/// the next ContextStream tool result instead: it polls in the background at
/// most every 35 s and appends what arrived as one more text block. It stands
/// aside while hooks in this folder deliver notices, and only records what
/// `context`, `init`, and `coordination` results already showed.
pub async fn attach_to_tool_result(
    payload: &mut Value,
    tool_name: &str,
    client: &mcp_client::ContextStreamClient,
    session: &mcp_session::SessionManager,
) {
    if !mid_run_notices_enabled() {
        return;
    }
    let Ok(cwd) = std::env::current_dir() else {
        return;
    };
    if hooks_deliver_notices_in(&cwd, HOOKS_LIVE_WINDOW) {
        return;
    }
    if NOTICE_CARRYING_TOOLS.contains(&tool_name) {
        let shown = payload_text(payload);
        let mut state = server_fallback();
        for block in tagged_blocks(&shown) {
            if block.starts_with("[COORDINATION]") {
                if let Some(id) = extract_notice_id_from_line(&block) {
                    push_bounded(
                        &mut state.session.delivered_notice_ids,
                        id,
                        MAX_DELIVERED_IDS,
                    );
                }
            }
        }
        return;
    }
    let session_state = session.state().await;
    let Some(session_id) = session_state
        .api_session_id
        .clone()
        .or(session_state.session_id.clone())
    else {
        return;
    };
    let now_ms = now_unix_ms();
    let (notices, start_poll) = {
        let mut state = server_fallback();
        let start_poll = !state.polling && poll_is_due(&state.session, now_ms, poll_interval_ms());
        if start_poll {
            state.polling = true;
            state.session.last_poll_unix_ms = now_ms;
        }
        (std::mem::take(&mut state.session.pending), start_poll)
    };
    if start_poll {
        let folder = cwd.to_string_lossy().to_string();
        let mut body = serde_json::json!({
            "session_id": session_id,
            "coordination_session_id": session_id,
            "hook_type": "mid_run_poll",
            "editor": "mcp",
            "folder_path": folder,
            "consume_notices": true,
        });
        if let Some(workspace_id) = session_state.workspace_id {
            body["workspace_id"] = Value::String(workspace_id.to_string());
        }
        if let Some(project_id) = session_state.project_id {
            body["project_id"] = Value::String(project_id.to_string());
        }
        if let Some(agent_name) = derive_agent_name(&serde_json::json!({}), &folder) {
            body["agent_name"] = Value::String(agent_name);
        }
        let client = client.clone();
        tokio::spawn(async move {
            let fetched = client
                .post::<Value, _>("/context/hook", body)
                .await
                .ok()
                .and_then(|payload| hook_context_of(&payload));
            let mut state = server_fallback();
            state.polling = false;
            if let Some((raw_context, complete)) = fetched {
                let (rendered, _) =
                    filter_and_record_polled_context(&mut state.session, &raw_context, complete);
                if let Some(rendered) = rendered {
                    state.session.pending.push(rendered);
                    let excess = state
                        .session
                        .pending
                        .len()
                        .saturating_sub(MAX_PENDING_NOTICES);
                    state.session.pending.drain(0..excess);
                }
            }
        });
    }
    if notices.is_empty() {
        return;
    }
    match payload.get_mut("content").and_then(Value::as_array_mut) {
        Some(content) => content.push(serde_json::json!({
            "type": "text",
            "text": notices.join("\n\n"),
        })),
        // Nowhere to show them: keep them for the next result.
        None => server_fallback().session.pending.extend(notices),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratch_and_temp_paths_are_detected_without_false_positives() {
        assert!(is_scratch_or_temp_path(
            "/private/tmp/claude-501/scratchpad/agent-cc-eng.7sYW"
        ));
        assert!(is_scratch_or_temp_path(
            "/tmp/scratchpad/agent-proof-claude"
        ));
        assert!(is_scratch_or_temp_path("/home/dev/.gemini/config"));
        assert!(!is_scratch_or_temp_path("/home/alice/code/mcp-server"));
        assert!(!is_scratch_or_temp_path("/Users/alice/code/contextstream"));
    }

    #[test]
    fn agent_handle_normalizes_scratch_suffixes_and_headings() {
        assert_eq!(
            normalize_agent_handle("agent-cc-qa.Cz5v"),
            Some("cc-qa".to_string())
        );
        assert_eq!(
            normalize_agent_handle("agent-mcp-eng2"),
            Some("mcp-eng2".to_string())
        );
        assert_eq!(
            extract_heading_agent_name("# cc-eng: engineer for contextcode\nMore details"),
            Some("cc-eng".to_string())
        );
    }

    #[test]
    fn filter_delivers_each_coordination_notice_once_and_only_new_lessons_after_baseline() {
        let mut state = MidRunSessionState::default();
        let poll_1 = "[LESSONS_WARNING] Existing lesson\n[PREFERENCE] Prefer concise diffs\n[COORDINATION] (important) from Engineering/contextcode: item=x For cc-qa: Kimi key funded. — ack via coordination(action=\"ack\", notice_id=\"11111111-1111-1111-1111-111111111111\")";
        let (msg_1, ack_1) = filter_and_record_polled_context(&mut state, poll_1, true);
        let msg_1 = msg_1.expect("coordination notice should emit on first poll");
        assert!(msg_1.contains("For cc-qa: Kimi key funded."));
        assert!(
            !msg_1.contains("Existing lesson"),
            "initial static lessons are baselined, not re-injected"
        );
        assert_eq!(ack_1, vec!["11111111-1111-1111-1111-111111111111"]);

        let (msg_2, ack_2) = filter_and_record_polled_context(&mut state, poll_1, true);
        assert!(msg_2.is_none());
        assert!(ack_2.is_empty());

        let poll_3 =
            "[LESSONS_WARNING] Existing lesson\n[LESSONS_WARNING] Brand new mid-run lesson";
        let (msg_3, ack_3) = filter_and_record_polled_context(&mut state, poll_3, true);
        let msg_3 = msg_3.expect("new lesson should emit");
        assert!(msg_3.contains("Brand new mid-run lesson"));
        assert!(!msg_3.contains("Existing lesson"));
        assert!(ack_3.is_empty());
    }

    #[test]
    fn a_cache_miss_context_delivers_notices_but_does_not_baseline_lessons() {
        let mut state = MidRunSessionState::default();
        // The API's cache-miss fallback has no lessons or preferences yet.
        let fallback = "[CONTEXTSTREAM] reminder\n[COORDINATION] (info) from W: x — ack via coordination(action=\"ack\", notice_id=\"44444444-4444-4444-8444-444444444444\")";
        let (msg, ack) = filter_and_record_polled_context(&mut state, fallback, false);
        assert!(msg.expect("notice emits").contains("[COORDINATION]"));
        assert_eq!(ack.len(), 1);
        assert!(!state.baselined_static);

        // The first complete context is the baseline, not news.
        let full = "[PREFERENCE] Prefer concise diffs\n[LESSONS_WARNING] Old lesson";
        let (msg, _) = filter_and_record_polled_context(&mut state, full, true);
        assert!(msg.is_none());
        assert!(state.baselined_static);

        // A cache miss later neither announces nor forgets anything.
        let (msg, _) = filter_and_record_polled_context(&mut state, "[PREFERENCE] Other", false);
        assert!(msg.is_none());
        let (msg, _) = filter_and_record_polled_context(
            &mut state,
            "[PREFERENCE] Prefer concise diffs\n[PREFERENCE] New preference",
            true,
        );
        let msg = msg.expect("new preference emits");
        assert!(msg.contains("New preference") && !msg.contains("concise diffs"));
    }

    #[test]
    fn a_notice_whose_reason_spans_lines_stays_one_notice_with_its_id() {
        let mut state = MidRunSessionState::default();
        let raw = "[COORDINATION] (info) from Engineering/contextcode: Resume Kimi\nlive tests [now]. — ack via coordination(action=\"ack\", notice_id=\"22222222-2222-2222-2222-222222222222\")\n[CONTEXTSTREAM] other";
        let (msg, ack) = filter_and_record_polled_context(&mut state, raw, true);
        let msg = msg.expect("notice emits");
        assert!(msg.contains("Resume Kimi live tests [now]."));
        assert_eq!(ack, vec!["22222222-2222-2222-2222-222222222222"]);
        assert!(!msg.contains("[CONTEXTSTREAM] other"));
    }

    #[test]
    fn hooks_take_waiting_notices_once_and_poll_at_most_once_per_interval() {
        let mut entry = MidRunSessionState::default();
        // First hook of a session: nothing waiting, first poll starts.
        let (notices, start, first) = claim_for_hook(&mut entry, 100_000, 35_000, "/w");
        assert!(notices.is_empty() && start && first);
        assert_eq!(entry.cwd.as_deref(), Some("/w"));

        // The poll left a notice; the next hook shows it and polls no sooner.
        entry.pending.push("[CONTEXTSTREAM LIVE NOTICE] x".into());
        let (notices, start, _) = claim_for_hook(&mut entry, 110_000, 35_000, "/w");
        assert_eq!(notices, vec!["[CONTEXTSTREAM LIVE NOTICE] x".to_string()]);
        assert!(!start);
        let (notices, start, _) = claim_for_hook(&mut entry, 120_000, 35_000, "/w");
        assert!(notices.is_empty() && !start);

        // After the interval the next poll starts, without the task summary.
        let (_, start, first) = claim_for_hook(&mut entry, 135_000, 35_000, "/w");
        assert!(start && !first);
    }

    #[test]
    fn notices_a_tool_result_showed_are_found_anywhere_in_it() {
        let response = serde_json::json!([{
            "type": "text",
            "text": "ok\n[COORDINATION] (important) from W/P: Kimi funded — ack via coordination(action=\"ack\", notice_id=\"33333333-3333-3333-3333-333333333333\")"
        }]);
        let mut text = String::new();
        tool_result_text(&response, &mut text);
        let ids: Vec<String> = tagged_blocks(&text)
            .iter()
            .filter_map(|block| extract_notice_id_from_line(block))
            .collect();
        assert_eq!(
            ids,
            vec!["33333333-3333-3333-3333-333333333333".to_string()]
        );
    }

    #[test]
    fn the_configured_workspace_is_read_from_each_editor_mcp_config() {
        let ws = "550e8400-e29b-41d4-a716-446655440000";
        let claude = serde_json::json!({
            "projects": {"/x": {"mcpServers": {"contextstream": {"headers": {"X-ContextStream-Workspace-Id": "not-this"}}}}},
            "mcpServers": {"contextstream": {"type": "http", "headers": {"X-ContextStream-Workspace-Id": ws}}}
        });
        assert_eq!(
            workspace_from_mcp_json(&claude.to_string()).as_deref(),
            Some(ws)
        );
        let local = serde_json::json!({"mcpServers": {"contextstream": {"env": {"CONTEXTSTREAM_WORKSPACE_ID": ws}}}});
        assert_eq!(
            workspace_from_mcp_json(&local.to_string()).as_deref(),
            Some(ws)
        );
        assert_eq!(workspace_from_mcp_json("{}"), None);
        let toml = format!(
            "[mcp_servers.other]\nhttp_headers = {{ \"X-ContextStream-Workspace-Id\" = \"00000000-0000-4000-8000-000000000042\" }}\n[mcp_servers.contextstream]\nurl = \"https://mcp.example\"\nhttp_headers = {{ \"X-ContextStream-Client\" = \"codex\", \"X-ContextStream-Workspace-Id\" = \"{ws}\" }}\n"
        );
        assert_eq!(workspace_from_codex_toml(&toml).as_deref(), Some(ws));
        assert_eq!(
            workspace_from_codex_toml("[mcp_servers.contextstream]\nurl = \"x\"\n"),
            None
        );
    }

    #[test]
    fn session_files_are_named_only_by_hex_keys() {
        assert!(session_path(&"a".repeat(64)).is_some());
        assert!(session_path("../../etc/passwd").is_none());
        assert!(session_path(&"g".repeat(64)).is_none());
    }

    // --- Prompt hook: standing text once per window ---

    const REPEAT: StandingRepeat = StandingRepeat {
        standing_ms: 600_000,
        notice_ms: 1_800_000,
    };
    const T0: u64 = 1_800_000_000_000;

    /// A prompt hook answer of the shape the API sends, with synthetic text:
    /// workflow header and six numbered steps, a blank line, then the tagged
    /// lines, a coaching block and the coordination notices.
    struct HookText {
        preferences: Vec<String>,
        decisions: Vec<String>,
        coaching: Vec<String>,
        notices: Vec<String>,
    }

    impl HookText {
        fn new() -> Self {
            Self {
                preferences: (1..=5)
                    .map(|n| format!("[PREFERENCE] style: rule_{n}: {{\"value\":\"choice {n}\"}}"))
                    .collect(),
                decisions: (1..=3)
                    .map(|n| format!("[DECISION] (today old) Decision number {n}"))
                    .collect(),
                coaching: (1..=5).map(|n| format!("- Coached rule {n}")).collect(),
                notices: Vec::new(),
            }
        }

        fn render(&self) -> String {
            let mut lines: Vec<String> =
                vec!["[CONTEXTSTREAM WORKFLOW harness_teaching_v4]".to_string()];
            lines.extend((1..=6).map(|n| format!("{n}) Step {n} — `tool_{n}(...)`")));
            lines.push(String::new());
            lines.push("[CONTEXTSTREAM] Default: call context(user_message=\"...\") first.".into());
            lines.extend(self.preferences.iter().cloned());
            lines.extend((1..=5).map(|n| format!("[LESSONS_WARNING] Lesson {n} — what to do")));
            lines.extend(self.decisions.iter().cloned());
            lines.push("[HOOK CONTEXT] Reuse the same session_id across turns.".into());
            if !self.coaching.is_empty() {
                lines.push(COACHING_HEADER.to_string());
                lines.extend(self.coaching.iter().cloned());
            }
            lines.extend(self.notices.iter().cloned());
            lines.join("\n")
        }
    }

    const COACHING_HEADER: &str =
        "[COACHING] You have corrected the agent on these before — do NOT repeat them:";
    const ITEM_A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";
    const NOTICE_A1: &str = "10101010-1010-4010-8010-101010101010";
    const NOTICE_A2: &str = "20202020-2020-4020-8020-202020202020";
    const ITEM_B: &str = "bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb";
    const NOTICE_B1: &str = "30303030-3030-4030-8030-303030303030";

    fn notice(item: &str, notice_id: &str, text: &str) -> String {
        format!(
            "[COORDINATION] (info) from Acme/widgets: item={item} {text} — ack via coordination(action=\"ack\", notice_id=\"{notice_id}\") or reply via coordination(action=\"reply\", notice_id=\"{notice_id}\", message=\"...\")"
        )
    }

    fn note(lines: usize, ago: &str) -> String {
        format!(
            "[CONTEXTSTREAM] Standing guidance ({lines} lines) was shown {ago} and still applies."
        )
    }

    fn line_count(text: &str) -> usize {
        text.lines().filter(|line| !line.trim().is_empty()).count()
    }

    #[test]
    fn the_first_prompt_is_full_and_the_next_in_the_window_is_one_line() {
        let mut state = MidRunSessionState::default();
        let text = HookText::new().render();
        assert_eq!(filter_standing_text(&mut state, &text, T0, REPEAT), text);
        assert_eq!(state.standing_window_started_unix_ms, T0);

        let second = filter_standing_text(&mut state, &text, T0 + 20_000, REPEAT);
        assert_eq!(second, note(line_count(&text), "under a minute ago"));
        let later = filter_standing_text(&mut state, &text, T0 + 599_999, REPEAT);
        assert_eq!(later, note(line_count(&text), "9 min ago"));
        // The window counts from the full delivery, not from the last prompt.
        assert_eq!(state.standing_window_started_unix_ms, T0);
    }

    #[test]
    fn after_the_window_the_prompt_is_full_again_and_a_new_window_starts() {
        let mut state = MidRunSessionState::default();
        let text = HookText::new().render();
        assert_eq!(filter_standing_text(&mut state, &text, T0, REPEAT), text);
        assert_ne!(
            filter_standing_text(&mut state, &text, T0 + 1, REPEAT),
            text
        );

        let expired = T0 + REPEAT.standing_ms;
        assert_eq!(
            filter_standing_text(&mut state, &text, expired, REPEAT),
            text
        );
        assert_eq!(state.standing_window_started_unix_ms, expired);
        assert_ne!(
            filter_standing_text(&mut state, &text, expired + 1, REPEAT),
            text
        );

        // A clock that went backwards never keeps guidance from a session.
        assert_eq!(
            filter_standing_text(&mut state, &text, T0 - 5, REPEAT),
            text
        );
    }

    #[test]
    fn a_new_line_in_the_window_is_delivered_alone_with_the_note() {
        let mut state = MidRunSessionState::default();
        let mut text = HookText::new();
        let first = text.render();
        filter_standing_text(&mut state, &first, T0, REPEAT);

        // A new decision pushes the oldest of the three out.
        text.decisions.remove(2);
        text.decisions
            .insert(0, "[DECISION] (today old) A decision made just now".into());
        let second = filter_standing_text(&mut state, &text.render(), T0 + 120_000, REPEAT);
        assert_eq!(
            second,
            format!(
                "{}\n[DECISION] (today old) A decision made just now",
                note(line_count(&first) - 1, "2 min ago")
            )
        );

        // A preference set that changed: only the preference that is new.
        text.preferences[1] = "[PREFERENCE] style: rule_2: {\"value\":\"changed\"}".into();
        let third = filter_standing_text(&mut state, &text.render(), T0 + 180_000, REPEAT);
        assert_eq!(
            third,
            format!(
                "{}\n[PREFERENCE] style: rule_2: {{\"value\":\"changed\"}}",
                note(line_count(&first) - 1, "3 min ago")
            )
        );

        // The decision that was pushed out and comes back was shown already.
        let fourth = filter_standing_text(&mut state, &first, T0 + 240_000, REPEAT);
        assert_eq!(fourth, note(line_count(&first), "4 min ago"));
    }

    #[test]
    fn a_line_without_a_tag_of_its_own_comes_with_its_header() {
        let mut state = MidRunSessionState::default();
        let mut text = HookText::new();
        let bullets = std::mem::take(&mut text.coaching);
        let without_coaching = text.render();
        filter_standing_text(&mut state, &without_coaching, T0, REPEAT);

        // The coaching block appears inside the window: header and bullets.
        text.coaching = bullets.clone();
        let appeared = filter_standing_text(&mut state, &text.render(), T0 + 60_000, REPEAT);
        assert_eq!(
            appeared,
            format!(
                "{}\n{COACHING_HEADER}\n{}",
                note(line_count(&without_coaching), "1 min ago"),
                bullets.join("\n")
            )
        );

        // A new bullet under the header that was shown brings the header.
        text.coaching.push("- A rule learned a moment ago".into());
        let grown = filter_standing_text(&mut state, &text.render(), T0 + 61_000, REPEAT);
        assert_eq!(
            grown,
            format!(
                "{}\n{COACHING_HEADER}\n- A rule learned a moment ago",
                note(line_count(&without_coaching) + bullets.len(), "1 min ago")
            )
        );

        // The same words under another header are another line.
        let moved = format!("{}\n[OTHER RULES] Also:\n- Coached rule 1", text.render());
        let moved = filter_standing_text(&mut state, &moved, T0 + 62_000, REPEAT);
        assert!(moved.ends_with("\n[OTHER RULES] Also:\n- Coached rule 1"));
    }

    #[test]
    fn a_coordination_notice_is_printed_once_then_counted_with_its_id() {
        let mut state = MidRunSessionState::default();
        let mut text = HookText::new();
        let standing = line_count(&text.render());
        let notice_a = notice(ITEM_A, NOTICE_A1, "Migration 12 is claimed.");
        text.notices.push(notice_a.clone());
        let first = filter_standing_text(&mut state, &text.render(), T0, REPEAT);
        assert!(first.ends_with(&notice_a));

        // Still unacknowledged on the next prompt: counted, with the id to ack.
        let second = filter_standing_text(&mut state, &text.render(), T0 + 1_000, REPEAT);
        assert_eq!(
            second,
            format!(
                "{}\n[COORDINATION] 1 notice shown earlier is still unacknowledged: ack via coordination(action=\"ack\", notice_id=\"{NOTICE_A1}\"), or read coordination(action=\"inbox\").",
                note(standing, "under a minute ago")
            )
        );
        assert!(!second.contains("Migration 12"));

        // A second notice arrives: printed in full, the first one counted.
        let notice_b = notice(ITEM_B, NOTICE_B1, "Both sessions edit the importer.");
        text.notices.push(notice_b.clone());
        let third = filter_standing_text(&mut state, &text.render(), T0 + 2_000, REPEAT);
        assert_eq!(
            third,
            format!(
                "{}\n{notice_b}\n[COORDINATION] 1 notice shown earlier is still unacknowledged: ack via coordination(action=\"ack\", notice_id=\"{NOTICE_A1}\"), or read coordination(action=\"inbox\").",
                note(standing, "under a minute ago")
            )
        );

        // The same item under a new notice id is not printed again, and the
        // count names the id that acknowledges it now.
        text.notices[0] = notice(ITEM_A, NOTICE_A2, "Migration 12 is claimed.");
        let fourth = filter_standing_text(&mut state, &text.render(), T0 + 3_000, REPEAT);
        assert_eq!(
            fourth,
            format!(
                "{}\n[COORDINATION] 2 notices shown earlier are still unacknowledged: ack via coordination(action=\"ack\", notice_id=\"{NOTICE_A2}\") / \"{NOTICE_B1}\", or read coordination(action=\"inbox\").",
                note(standing, "under a minute ago")
            )
        );

        // Acknowledged: the API stops sending them and the count goes.
        text.notices.clear();
        let fifth = filter_standing_text(&mut state, &text.render(), T0 + 4_000, REPEAT);
        assert_eq!(fifth, note(standing, "under a minute ago"));
    }

    fn counted(ids: &[&str]) -> String {
        let (noun, verb) = if ids.len() == 1 {
            ("notice", "is")
        } else {
            ("notices", "are")
        };
        let rest: String = ids[1..].iter().map(|id| format!(" / \"{id}\"")).collect();
        format!(
            "[COORDINATION] {} {noun} shown earlier {verb} still unacknowledged: ack via coordination(action=\"ack\", notice_id=\"{}\"){rest}, or read coordination(action=\"inbox\").",
            ids.len(),
            ids[0]
        )
    }

    #[test]
    fn a_notice_is_not_brought_back_by_a_full_delivery_of_the_standing_text() {
        const MINUTE: u64 = 60_000;
        let mut state = MidRunSessionState::default();
        let mut text = HookText::new();
        let standing = text.render();
        let notice_a = notice(ITEM_A, NOTICE_A1, "Migration 12 is claimed.");
        text.notices.push(notice_a.clone());
        let first = filter_standing_text(&mut state, &text.render(), T0, REPEAT);
        assert_eq!(first, format!("{standing}\n{notice_a}"));

        // Ten and twenty minutes later the standing text is due in full. The
        // notice was shown: it is counted, not printed.
        for minutes in [10, 20] {
            let full =
                filter_standing_text(&mut state, &text.render(), T0 + minutes * MINUTE, REPEAT);
            assert_eq!(
                full,
                format!("{standing}\n{}", counted(&[NOTICE_A1])),
                "{minutes} min"
            );
        }

        // A different notice arrives meanwhile: in full, once.
        let notice_b = notice(ITEM_B, NOTICE_B1, "Both sessions edit the importer.");
        text.notices.push(notice_b.clone());
        let arrived = filter_standing_text(&mut state, &text.render(), T0 + 25 * MINUTE, REPEAT);
        assert_eq!(
            arrived,
            format!(
                "{}\n{notice_b}\n{}",
                note(line_count(&standing), "5 min ago"),
                counted(&[NOTICE_A1])
            )
        );
        let both = filter_standing_text(&mut state, &text.render(), T0 + 26 * MINUTE, REPEAT);
        assert!(both.ends_with(&counted(&[NOTICE_A1, NOTICE_B1])), "{both}");
        assert!(!both.contains("Migration 12") && !both.contains("importer"));

        // Just short of thirty minutes after its full copy: counted.
        let short = filter_standing_text(&mut state, &text.render(), T0 + 30 * MINUTE - 1, REPEAT);
        assert!(
            short.ends_with(&counted(&[NOTICE_A1, NOTICE_B1])),
            "{short}"
        );

        // Still pending thirty minutes after its full copy: in full again.
        // The other notice, five minutes younger, stays counted.
        let again = filter_standing_text(&mut state, &text.render(), T0 + 30 * MINUTE, REPEAT);
        assert_eq!(
            again,
            format!("{standing}\n{notice_a}\n{}", counted(&[NOTICE_B1]))
        );
        let after = filter_standing_text(&mut state, &text.render(), T0 + 31 * MINUTE, REPEAT);
        assert!(
            after.ends_with(&counted(&[NOTICE_A1, NOTICE_B1])),
            "{after}"
        );

        // The item under a new notice id keeps the item's clock.
        text.notices[0] = notice(ITEM_A, NOTICE_A2, "Migration 12 is claimed.");
        let renamed = filter_standing_text(&mut state, &text.render(), T0 + 32 * MINUTE, REPEAT);
        assert!(
            renamed.ends_with(&counted(&[NOTICE_A2, NOTICE_B1])),
            "{renamed}"
        );
        let due = filter_standing_text(&mut state, &text.render(), T0 + 60 * MINUTE, REPEAT);
        assert!(
            due.contains("Migration 12") && due.contains("importer"),
            "{due}"
        );
        assert!(!due.contains("still unacknowledged"));
    }

    #[test]
    fn a_notice_window_of_zero_prints_every_pending_notice_with_every_prompt() {
        let every_prompt = StandingRepeat {
            notice_ms: 0,
            ..REPEAT
        };
        let mut state = MidRunSessionState::default();
        let mut text = HookText::new();
        let standing = line_count(&text.render());
        let notice_a = notice(ITEM_A, NOTICE_A1, "Migration 12 is claimed.");
        text.notices.push(notice_a.clone());
        let text = text.render();
        assert_eq!(
            filter_standing_text(&mut state, &text, T0, every_prompt),
            text
        );
        for second in 1..=2 {
            assert_eq!(
                filter_standing_text(&mut state, &text, T0 + second * 1_000, every_prompt),
                format!("{}\n{notice_a}", note(standing, "under a minute ago"))
            );
        }
        assert!(state.standing_notice_copies.is_empty());
    }

    #[test]
    fn a_notice_without_an_id_or_an_item_is_printed_once_by_its_text() {
        let mut state = MidRunSessionState::default();
        let text = "[CONTEXTSTREAM] Default.\n[COORDINATION] (info) from Acme: the staging box is\nbeing rebuilt until noon.";
        assert_eq!(filter_standing_text(&mut state, text, T0, REPEAT), text);
        assert_eq!(
            filter_standing_text(&mut state, text, T0 + 1, REPEAT),
            "[CONTEXTSTREAM] Standing guidance (1 line) was shown under a minute ago and still applies.\n[COORDINATION] 1 notice shown earlier is still unacknowledged: read coordination(action=\"inbox\")."
        );
    }

    #[test]
    fn instruct_entries_and_the_credit_notice_are_delivered_with_every_prompt() {
        let mut state = MidRunSessionState::default();
        let text = format!(
            "{}\n[INSTRUCT] Answer in French\nuntil told otherwise.\n[CONTEXTSTREAM_NOTICE] Indexing is paused.",
            HookText::new().render()
        );
        filter_standing_text(&mut state, &text, T0, REPEAT);
        let second = filter_standing_text(&mut state, &text, T0 + 1_000, REPEAT);
        assert_eq!(
            second,
            format!(
                "{}\n[INSTRUCT] Answer in French\nuntil told otherwise.\n[CONTEXTSTREAM_NOTICE] Indexing is paused.",
                note(line_count(&text) - 3, "under a minute ago")
            )
        );
        assert!(state.standing_line_hashes.len() == line_count(&text) - 3);
    }

    #[test]
    fn a_block_that_is_new_in_the_window_is_delivered_whole_then_left_out() {
        let mut state = MidRunSessionState::default();
        let api_text = HookText::new().render();
        filter_standing_text(&mut state, &api_text, T0, REPEAT);

        // The API stops answering: the static reminder takes the hook
        // context's place. Its first delivery is whole.
        let workflow: String = api_text.lines().take(7).collect::<Vec<_>>().join("\n");
        let reminder = "[CONTEXTSTREAM EXTENDED GUIDANCE]\nAUTO-GROUNDING: read `[GROUNDING]` first.\nRESUME: call session(action=\"resume\").\n[END]";
        let offline = format!("{workflow}\n\n{reminder}");
        let first = filter_standing_text(&mut state, &offline, T0 + 1_000, REPEAT);
        assert_eq!(
            first,
            format!("{}\n{reminder}", note(7, "under a minute ago"))
        );
        let second = filter_standing_text(&mut state, &offline, T0 + 2_000, REPEAT);
        assert_eq!(second, note(11, "under a minute ago"));
    }

    #[test]
    fn a_wrapped_alert_block_goes_out_whole_or_not_at_all() {
        let mut state = MidRunSessionState::default();
        let wrap = |alerts: &str| {
            format!("[CONTEXTSTREAM] Default.\n<system-reminder>\n{alerts}\n</system-reminder>\n[PREFERENCE] tabs")
        };
        filter_standing_text(
            &mut state,
            "[CONTEXTSTREAM] Default.\n[PREFERENCE] tabs",
            T0,
            REPEAT,
        );

        // The alerts appear inside the window: the whole wrapped block.
        let one = wrap("[GRAPH_ALERT] cycle in module a");
        assert_eq!(
            filter_standing_text(&mut state, &one, T0 + 1, REPEAT),
            format!(
                "{}\n<system-reminder>\n[GRAPH_ALERT] cycle in module a\n</system-reminder>",
                note(2, "under a minute ago")
            )
        );
        assert_eq!(
            filter_standing_text(&mut state, &one, T0 + 2, REPEAT),
            note(5, "under a minute ago")
        );

        // A second alert: the block again, both tags with it.
        let two = wrap("[GRAPH_ALERT] cycle in module a\n[GRAPH_ALERT] cycle in module b");
        assert_eq!(
            filter_standing_text(&mut state, &two, T0 + 3, REPEAT),
            format!(
                "{}\n<system-reminder>\n[GRAPH_ALERT] cycle in module a\n[GRAPH_ALERT] cycle in module b\n</system-reminder>",
                note(2, "under a minute ago")
            )
        );
    }

    #[test]
    fn the_standing_memory_is_bounded_and_absent_from_a_state_that_never_used_it() {
        let untouched = serde_json::to_string(&MidRunSessionState::default()).unwrap();
        assert!(!untouched.contains("standing_"));

        let mut state = MidRunSessionState::default();
        let long: String = (0..400)
            .map(|n| format!("[RULE] number {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(filter_standing_text(&mut state, &long, T0, REPEAT), long);
        assert_eq!(state.standing_line_hashes.len(), MAX_STANDING_LINE_HASHES);
        // The lines that fell out of the memory are shown again.
        let again = filter_standing_text(&mut state, &long, T0 + 1, REPEAT);
        assert_eq!(line_count(&again), 1 + 400 - MAX_STANDING_LINE_HASHES);

        // A state file written before these fields existed reads as no window.
        let old: MidRunSessionState =
            serde_json::from_str(r#"{"last_poll_unix_ms":5,"delivered_notice_ids":["x"]}"#)
                .unwrap();
        assert_eq!(old.standing_window_started_unix_ms, 0);
    }

    // The two tests below are the reader's (review-prompt-hook-standing-1);
    // both failed on 5cb65cda.
    #[test]
    fn a_reply_on_an_item_already_shown_is_printed_in_full() {
        let mut state = MidRunSessionState::default();
        let mut text = HookText::new();
        text.notices
            .push(notice(ITEM_A, NOTICE_A1, "Migration 12 is claimed."));
        filter_standing_text(&mut state, &text.render(), T0, REPEAT);

        // The API's reply notice keeps the item of the notice it answers and
        // carries words this session has never seen.
        let reply = notice(
            ITEM_A,
            NOTICE_A2,
            "Reply from session lane-y: STOP, I am rewriting migration 12 now; do not push it",
        );
        text.notices.push(reply.clone());
        let second = filter_standing_text(&mut state, &text.render(), T0 + 60_000, REPEAT);
        assert!(
            second.lines().any(|line| line == reply),
            "the reply was never shown, yet it is counted as 'shown earlier':\n{second}"
        );
        assert!(second.ends_with(&format!(
            "[COORDINATION] 1 notice shown earlier is still unacknowledged: ack via coordination(action=\"ack\", notice_id=\"{NOTICE_A1}\"), or read coordination(action=\"inbox\")."
        )), "{second}");
    }

    #[test]
    fn a_notice_that_quotes_an_item_in_its_words_is_printed_in_full() {
        let mut state = MidRunSessionState::default();
        let mut text = HookText::new();
        text.notices
            .push(notice(ITEM_A, NOTICE_A1, "Migration 12 is claimed."));
        filter_standing_text(&mut state, &text.render(), T0, REPEAT);

        // A notice with no item of its own whose reason mentions an item.
        let lease = format!(
            "[COORDINATION] (important) from Acme/widgets: deploy is held by lane z (it follows item={ITEM_A}); wait for it — ack via coordination(action=\"ack\", notice_id=\"{NOTICE_B1}\")"
        );
        text.notices.push(lease.clone());
        let second = filter_standing_text(&mut state, &text.render(), T0 + 60_000, REPEAT);
        assert!(
            second.lines().any(|line| line == lease),
            "a notice never shown is counted as 'shown earlier':\n{second}"
        );
    }

    #[test]
    fn a_notice_item_is_read_only_where_the_api_writes_it() {
        let api = notice(ITEM_A, NOTICE_A1, "Migration 12 is claimed.");
        assert_eq!(notice_item_from_header(&api).as_deref(), Some(ITEM_A));
        let blocking = format!(
            "[COORDINATION] (blocking) from Acme/widgets: item={ITEM_B} READ BEFORE CONTINUING — x"
        );
        assert_eq!(notice_item_from_header(&blocking).as_deref(), Some(ITEM_B));
        let no_urgency = format!("[COORDINATION] from another scope: item={ITEM_B} x");
        assert_eq!(
            notice_item_from_header(&no_urgency).as_deref(),
            Some(ITEM_B)
        );
        for quoted in [
            format!("[COORDINATION] (info) from Acme/widgets: see item={ITEM_A} first"),
            format!("[COORDINATION] (info) from Acme/widgets: lease: item={ITEM_A}"),
            format!("[COORDINATION] (info) item={ITEM_A}"),
            format!("[PREFERENCE] from Acme/widgets: item={ITEM_A}"),
            "[COORDINATION] (info) from Acme/widgets: item=not-a-uuid".to_string(),
        ] {
            assert_eq!(notice_item_from_header(&quoted), None, "{quoted}");
        }

        // The words are compared without the notice ids, and only without them.
        let again = notice(ITEM_A, NOTICE_A2, "Migration 12 is claimed.");
        let other = notice(ITEM_A, NOTICE_A1, "Migration 12 is free.");
        assert_eq!(notice_words_hash(&api), notice_words_hash(&again));
        assert_ne!(notice_words_hash(&api), notice_words_hash(&other));
        assert_eq!(
            notice_words_hash("a notice_id=\"x"),
            notice_words_hash("a notice_id=\"x")
        );
    }

    #[test]
    fn a_held_session_lock_costs_at_most_the_wait_and_changes_nothing() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("session.json");
        let held = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path.with_extension("lock"))
            .expect("lock file");
        fs2::FileExt::lock_exclusive(&held).expect("hold the lock");
        let started = std::time::Instant::now();
        let result = with_locked_json_within(
            &path,
            Duration::from_millis(200),
            |entry: &mut MidRunSessionState| entry.standing_window_started_unix_ms = 1,
        );
        assert!(result.is_none());
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(!path.exists(), "nothing is written without the lock");

        fs2::FileExt::unlock(&held).expect("release the lock");
        let result = with_locked_json_within(
            &path,
            Duration::from_millis(200),
            |entry: &mut MidRunSessionState| entry.standing_window_started_unix_ms = 1,
        );
        assert!(result.is_some());
        assert!(read_session_entry(&path).is_some_and(|e| e.standing_window_started_unix_ms == 1));
    }

    #[test]
    fn block_headers_are_told_from_ordinary_lines() {
        for header in [
            "[CONTEXTSTREAM WORKFLOW harness_teaching_v4]",
            "[PREFERENCE] style: tabs",
            "[DECISION] (today old) Ship it",
            "[END]",
        ] {
            assert!(opens_standing_block(header), "{header}");
        }
        for line in [
            "1) Initialize once — `init(...)`",
            "- A coached rule",
            "[] empty",
            "[lowercase] words",
            "If ContextStream surfaces `[LESSONS_WARNING]`, follow it.",
            "<system-reminder>",
        ] {
            assert!(!opens_standing_block(line), "{line}");
        }
        assert_eq!(
            notice_item_from_header(&notice(ITEM_A, NOTICE_A1, "x")).as_deref(),
            Some(ITEM_A)
        );
        assert_eq!(
            notice_item_from_header(
                "[COORDINATION] (info) from a/b: item=not-a-uuid and more text here, long enough"
            ),
            None
        );
        assert_eq!(
            notice_item_from_header("[COORDINATION] (info) from a/b: item=é"),
            None
        );
    }

    // -- What a person sends a run: the poll's pointers ------------------------

    fn answered<'a>(
        json: Option<&'a Value>,
        text: Option<&'a str>,
    ) -> super::super::run_progress::Answer<'a> {
        super::super::run_progress::Answer {
            succeeded: true,
            json,
            text,
        }
    }

    const RUN_A: &str = "66666666-6666-4666-8666-666666666666";
    const RUN_B: &str = "77777777-7777-4777-8777-777777777777";
    const WORKSPACE_A: &str = "11111111-1111-4111-8111-111111111111";
    const DELIVERY_A: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

    #[test]
    fn a_progress_report_names_the_run_the_poll_asks_about() {
        // A local server answers in words.
        let words = format!("[PROGRESS] Recorded: step. state=working run_id={RUN_A}");
        let finding = progress_finding(
            &serde_json::json!({"step": "Running the tests"}),
            &answered(None, Some(&words)),
            &words,
        );
        assert_eq!(
            finding.reported,
            Some(ReportedRun {
                run_id: RUN_A.to_string(),
                workspace_id: None,
            })
        );
        assert!(!finding.ended);
        assert!(finding.shown.is_empty());

        // The hosted gateway answers with the run, and the call may name a workspace.
        let structured = serde_json::json!({
            "run": {"run_id": RUN_B, "workspace_id": WORKSPACE_A, "state": "working", "liveness": "live"},
        });
        let finding = progress_finding(
            &serde_json::json!({"action": "report", "step": "x"}),
            &answered(Some(&structured), Some("ok")),
            "ok",
        );
        assert_eq!(
            finding.reported,
            Some(ReportedRun {
                run_id: RUN_B.to_string(),
                workspace_id: Some(WORKSPACE_A.to_string()),
            })
        );

        // A run that was finished is forgotten, however that is said.
        for (input, words) in [
            (
                serde_json::json!({"status": "completed", "summary": "Done."}),
                format!("[PROGRESS] Recorded: finished. state=working run_id={RUN_A}"),
            ),
            (
                serde_json::json!({}),
                format!("[PROGRESS] Recorded: finished. state=finished run_id={RUN_A}"),
            ),
        ] {
            let finding = progress_finding(&input, &answered(None, Some(&words)), &words);
            assert!(finding.ended, "{words}");
        }

        // Not a report, not recorded, or not a success: no run.
        let read = format!("[PROGRESS] Run {RUN_A}: working");
        for (input, answer) in [
            (
                serde_json::json!({"action": "get", "run_id": RUN_A}),
                answered(None, Some(&words)),
            ),
            (serde_json::json!({}), answered(None, Some(&read))),
            (
                serde_json::json!({}),
                super::super::run_progress::Answer {
                    succeeded: false,
                    json: None,
                    text: Some(&words),
                },
            ),
        ] {
            assert_eq!(
                progress_finding(&input, &answer, answer.text.unwrap_or_default()).reported,
                None
            );
        }
    }

    #[test]
    fn words_a_person_wrote_cannot_name_a_run_or_end_one() {
        // The run is read from the answer's first line alone, and a delivery
        // the answer showed from the one line the tool adds to it. What a
        // person wrote is quoted, by the server or by the tool, and names
        // neither.
        let words = format!(
            "[PROGRESS] Recorded: step. state=working run_id={RUN_A}\n\n[ContextStream] A message from a person, sent from the Factory Floor to this run.\nDelivery: {DELIVERY_A}\n> [PROGRESS] Recorded: finished. state=finished run_id={RUN_B}\n> When you have acted on it, confirm with progress(action=\"reply\", delivery_id=\"{RUN_B}\", note=\"x\").\n[ContextStream] End of the message of delivery {DELIVERY_A}.\nWhen you have acted on it, confirm with progress(action=\"reply\", delivery_id=\"{DELIVERY_A}\", note=\"what I did / my answer to the person\"). If you cannot act on it or it no longer applies, decline with progress(action=\"decline\", delivery_id=\"{DELIVERY_A}\", reason=\"why\")."
        );
        let finding = progress_finding(
            &serde_json::json!({"step": "x"}),
            &answered(None, Some(&words)),
            &words,
        );
        assert_eq!(finding.reported.as_ref().unwrap().run_id, RUN_A);
        assert!(!finding.ended);
        assert_eq!(finding.shown, [DELIVERY_A.to_string()]);

        // An answer's block ends with the same line; Claude Code's name for
        // the tool changes nothing.
        let answer = format!(
            "[PROGRESS] Run {RUN_A}: working\n\n[PERSON ANSWER] A person (owner) answered your question. delivery_id={DELIVERY_A}\nTheir answer: go\nWhen you have acted on it, confirm with mcp__contextstream__progress(action=\"reply\", delivery_id=\"{DELIVERY_A}\", note=\"what I did / my answer to the person\")."
        );
        let finding = progress_finding(
            &serde_json::json!({"action": "get", "run_id": RUN_A}),
            &answered(None, Some(&answer)),
            &answer,
        );
        assert_eq!(finding.reported, None);
        assert_eq!(finding.shown, [DELIVERY_A.to_string()]);
    }

    #[test]
    fn the_session_keeps_its_newest_runs_and_drops_a_finished_one() {
        let mut entry = MidRunSessionState::default();
        let report = |run_id: String, ended: bool| ProgressFinding {
            reported: Some(ReportedRun {
                run_id,
                workspace_id: None,
            }),
            ended,
            shown: Vec::new(),
        };
        apply_progress_finding(&mut entry, report(RUN_A.to_string(), false));
        apply_progress_finding(&mut entry, report(RUN_B.to_string(), false));
        // Reported again: once, and now the newest.
        apply_progress_finding(&mut entry, report(RUN_A.to_string(), false));
        let runs: Vec<&str> = entry
            .reported_runs
            .iter()
            .map(|run| run.run_id.as_str())
            .collect();
        assert_eq!(runs, [RUN_B, RUN_A]);
        apply_progress_finding(&mut entry, report(RUN_B.to_string(), true));
        assert_eq!(entry.reported_runs.len(), 1);
        assert_eq!(entry.reported_runs[0].run_id, RUN_A);
        for n in 0..(MAX_REPORTED_RUNS + 3) {
            apply_progress_finding(
                &mut entry,
                report(Uuid::from_u128(n as u128 + 1).to_string(), false),
            );
        }
        assert_eq!(entry.reported_runs.len(), MAX_REPORTED_RUNS);
        assert!(entry.reported_runs.iter().all(|run| run.run_id != RUN_A));
    }

    fn waiting(id: &str, previous_claims: usize) -> Waiting {
        Waiting {
            id: id.to_string(),
            previous_claims,
            pointer: format!("[CONTEXTSTREAM LIVE NOTICE] something waits (delivery_id={id})."),
        }
    }

    #[test]
    fn a_delivery_is_pointed_at_once_per_offer_and_not_after_the_tool_showed_it() {
        let mut entry = MidRunSessionState::default();
        queue_delivery_pointers(&mut entry, vec![waiting(DELIVERY_A, 0), waiting(RUN_B, 0)]);
        assert_eq!(
            entry.pending,
            [waiting(DELIVERY_A, 0).pointer, waiting(RUN_B, 0).pointer]
        );
        // Still waiting at the next poll: not said again.
        let before = entry.pending.clone();
        queue_delivery_pointers(&mut entry, vec![waiting(DELIVERY_A, 0)]);
        assert_eq!(entry.pending, before);
        // The agent read one of them with `progress`: its pointer is taken back.
        entry.pending.push("unrelated notice".into());
        apply_progress_finding(
            &mut entry,
            ProgressFinding {
                reported: None,
                ended: false,
                shown: vec![DELIVERY_A.to_string()],
            },
        );
        assert_eq!(
            entry.pending,
            [waiting(RUN_B, 0).pointer, "unrelated notice".to_string()]
        );
        // Its claim ran out and the API offers it again: pointed at once more.
        entry.pending.clear();
        queue_delivery_pointers(&mut entry, vec![waiting(DELIVERY_A, 1)]);
        assert_eq!(entry.pending.len(), 1);
        queue_delivery_pointers(&mut entry, vec![waiting(DELIVERY_A, 1)]);
        assert_eq!(entry.pending.len(), 1);

        // A read that crossed the tool's own claim: the tool showed it first.
        let mut entry = MidRunSessionState::default();
        apply_progress_finding(
            &mut entry,
            ProgressFinding {
                reported: None,
                ended: false,
                shown: vec![DELIVERY_A.to_string()],
            },
        );
        queue_delivery_pointers(&mut entry, vec![waiting(DELIVERY_A, 0)]);
        assert!(entry.pending.is_empty());
    }

    #[test]
    fn a_state_that_never_heard_of_deliveries_reads_and_writes_as_before() {
        let old = r#"{"last_poll_unix_ms":5,"pending":["x"],"updated_at":"t"}"#;
        let entry: MidRunSessionState = serde_json::from_str(old).unwrap();
        assert!(entry.reported_runs.is_empty());
        assert!(entry.noticed_deliveries.is_empty());
        assert_eq!(entry.deliveries_not_there_until_unix_ms, 0);
        let written = serde_json::to_string(&entry).unwrap();
        for field in [
            "reported_runs",
            "noticed_deliveries",
            "deliveries_not_there",
        ] {
            assert!(!written.contains(field), "{written}");
        }
    }

    /// Run `body` with `HOME` (where session state lives) in a fresh folder.
    fn with_temp_home(body: impl FnOnce()) {
        let _guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().expect("temp home");
        let previous = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());
        body();
        match previous {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn a_once_per_session_reminder_is_claimed_once_per_session_and_token() {
        with_temp_home(|| {
            let input = serde_json::json!({"session_id": "once-session-a"});
            let token = "post-tool-nudge:entity:ticket";
            assert!(claim_once_per_session(&input, "/repo", token));
            assert!(!claim_once_per_session(&input, "/repo", token));
            assert!(claim_once_per_session(
                &input,
                "/repo",
                "post-tool-nudge:entity:other"
            ));
            // Another session has its own record.
            let other = serde_json::json!({"session_id": "once-session-b"});
            assert!(claim_once_per_session(&other, "/repo", token));
        });
    }

    #[test]
    fn once_per_session_tokens_are_bounded_and_survive_older_state_files() {
        let mut entry = MidRunSessionState::default();
        for n in 0..(MAX_SHOWN_ONCE + 5) {
            assert!(claim_once(&mut entry, &format!("token-{n}")));
        }
        assert_eq!(entry.shown_once.len(), MAX_SHOWN_ONCE);
        assert!(!claim_once(
            &mut entry,
            &format!("token-{}", MAX_SHOWN_ONCE + 4)
        ));

        let old = r#"{"last_poll_unix_ms":5,"updated_at":"t"}"#;
        let entry: MidRunSessionState = serde_json::from_str(old).unwrap();
        assert!(entry.shown_once.is_empty());
        assert!(!serde_json::to_string(&entry)
            .unwrap()
            .contains("shown_once"));
    }

    #[test]
    fn what_the_session_saved_itself_is_not_played_back_as_news() {
        let mut state = MidRunSessionState::default();
        let baseline = "[PREFERENCE] Prefer concise diffs\n[LESSONS_WARNING] Old lesson";
        assert!(filter_and_record_polled_context(&mut state, baseline, true)
            .0
            .is_none());

        remember_self_authored_title(&mut state, "Prefer X");
        remember_self_authored_title(&mut state, "Check  the LOCK first");
        let echo = "[PREFERENCE] Prefer concise diffs\n[PREFERENCE] Prefer X: use X over Y\n[LESSONS_WARNING] Check the lock first — before writing";
        let (msg, _) = filter_and_record_polled_context(&mut state, echo, true);
        assert!(msg.is_none(), "own saves are not news: {msg:?}");
        assert!(state
            .seen_preference_lines
            .iter()
            .any(|line| line.contains("Prefer X")));

        let (msg, _) = filter_and_record_polled_context(
            &mut state,
            "[PREFERENCE] Prefer X: use X over Y\n[PREFERENCE] Prefer small commits",
            true,
        );
        let msg = msg.expect("an unrelated new preference still emits");
        assert!(msg.contains("Prefer small commits"));
        assert!(!msg.contains("Prefer X"));
    }

    #[test]
    fn self_authored_titles_are_bounded_and_skip_ones_too_short_to_match() {
        let mut state = MidRunSessionState::default();
        remember_self_authored_title(&mut state, "  X ");
        assert!(state.self_authored_titles.is_empty());
        for n in 0..(MAX_SELF_AUTHORED_TITLES + 3) {
            remember_self_authored_title(&mut state, &format!("Saved title {n}"));
        }
        assert_eq!(state.self_authored_titles.len(), MAX_SELF_AUTHORED_TITLES);
        assert_eq!(state.self_authored_titles[0], "saved title 3");

        let old = r#"{"last_poll_unix_ms":5,"updated_at":"t"}"#;
        let entry: MidRunSessionState = serde_json::from_str(old).unwrap();
        assert!(entry.self_authored_titles.is_empty());
        assert!(!serde_json::to_string(&entry)
            .unwrap()
            .contains("self_authored_titles"));
    }

    #[test]
    fn a_recorded_title_reaches_the_session_state_the_poll_reads() {
        with_temp_home(|| {
            let input = serde_json::json!({"session_id": "self-authored-session"});
            record_self_authored_title(&input, "/repo", "Prefer X");
            let key = session_state_key(&input, "/repo");
            let entry = read_session_entry_fast(&key).expect("state written");
            assert_eq!(entry.self_authored_titles, vec!["prefer x".to_string()]);
        });
    }
}
