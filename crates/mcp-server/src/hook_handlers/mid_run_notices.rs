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
//! `CONTEXTSTREAM_MID_RUN_NOTICES=0` turns delivery off.

use chrono::Utc;
use mcp_session::auto_init::checkout_binding_workspace;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

use super::common::{self, ApiConfig};
use super::durable_capture::{
    plain_session_id, session_api_session_id, session_primary_scope, sha256_hex, state_dir,
    with_locked_json,
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
/// The background poll is off the hook's path, so it can wait for a slow API.
const POLL_HTTP_TIMEOUT: Duration = Duration::from_secs(8);
const ACK_HTTP_TIMEOUT: Duration = Duration::from_secs(3);
/// Hook name of the detached poll process.
pub(crate) const BACKGROUND_POLL_HOOK: &str = "mid-run-poll";

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
    #[serde(default)]
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedHookScope {
    pub config: ApiConfig,
    pub effective_folder_path: String,
    pub agent_name: Option<String>,
    pub coordination_session_id: Option<String>,
    pub host_session_id: Option<String>,
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
pub async fn handle_notice_hook() -> anyhow::Result<()> {
    let input = super::read_stdin_json()?;
    let cwd = common::extract_cwd(&input);
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

/// Keep what a poll returned that this session has not seen: every new
/// `[COORDINATION]` notice, and lessons or preferences that appeared after
/// the first complete context. `complete` is false when the API answered
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
            if was_baselined && !already_seen {
                emit_lines.push(block);
            }
        } else if block.starts_with("[PREFERENCE]") {
            let already_seen = entry.seen_preference_lines.contains(&block);
            push_bounded(
                &mut entry.seen_preference_lines,
                block.clone(),
                MAX_SEEN_STATIC_LINES,
            );
            if was_baselined && !already_seen {
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
}
