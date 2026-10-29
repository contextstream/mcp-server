//! Automatic ContextAgent runs from MCP clients: the workspace's `auto_run.mcp` setting.
//!
//! With the setting on, an agent working through ContextStream MCP starts a fitting
//! ContextAgent itself when a good moment comes up ([`super::moments`]): `context()` and the
//! hooks name the exact `agents(action="run", ..., automatic=true)` call, and the `agents`
//! tool starts such a run without the quote round trip, as `source: "auto_mcp"`, in the
//! background. The API enforces the limits (`auto_run_off`, `auto_run_agent_not_allowed`,
//! `auto_run_over_limit`, `auto_run_budget_reached`, the daily run limit). With the setting
//! off, the default, nothing here suggests or runs anything.
//!
//! Kept cheap and quiet:
//! - the setting (`GET /workspaces/{id}/tasks/agents/auto`) is read only once a moment fits,
//!   and kept 60 s per caller and workspace, in memory and, on one machine (stdio server and
//!   hooks), in `~/.contextstream/agents-auto/`; an API without the route counts as off;
//! - the [`Ledger`] suggests each agent at most once per task (30 minutes per project), never
//!   one started in the last 2 hours, and nothing for an hour after a refusal.

use mcp_client::ContextStreamClient;
use mcp_types::Error;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use uuid::Uuid;

use super::moments::{self, Moment};

/// The task source of a run an MCP client started on its own.
pub const AUTO_SOURCE: &str = "auto_mcp";
/// How long a read of the setting is kept.
pub const SETTING_TTL_SECS: i64 = 60;
/// How long an API without the route, or a caller it refuses, is believed.
const UNSUPPORTED_TTL_SECS: i64 = 600;
/// How soon an unreachable setting is read again.
const UNREACHABLE_RETRY_SECS: i64 = 30;
/// One suggestion of an agent per project in this window: one task.
pub const SUGGEST_WINDOW_SECS: i64 = 30 * 60;
/// An agent started automatically is not suggested again for this long.
pub const STARTED_WINDOW_SECS: i64 = 2 * 60 * 60;
/// After a refusal, nothing is suggested in the workspace for this long.
pub const REFUSAL_PAUSE_SECS: i64 = 60 * 60;
/// Unit tests keep the setting and the ledger in memory: parallel tests would race on one
/// file in the state folder.
const FILES_ALLOWED: bool = !cfg!(test);
/// Most agents one hint names.
const MAX_SUGGESTIONS: usize = 2;
/// Runs the ledger keeps waiting for, per caller.
const MAX_PENDING: usize = 20;

// ---------------------------------------------------------------------------------------
// The setting
// ---------------------------------------------------------------------------------------

/// One agent an automatic run may use, as `GET .../tasks/agents/auto` lists it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutoAgent {
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// What it does, in one line.
    #[serde(default)]
    pub job: String,
    #[serde(default)]
    pub enabled: bool,
    /// Its access is granted: it does not need review.
    #[serde(default)]
    pub scopes_ok: bool,
    #[serde(default)]
    pub inputs_required: Vec<String>,
}

impl AutoAgent {
    /// On, and nothing to review: an automatic run may use it.
    pub fn may_run(&self) -> bool {
        self.enabled && self.scopes_ok
    }

    fn display_name(&self) -> &str {
        if self.name.trim().is_empty() {
            &self.id
        } else {
            self.name.trim()
        }
    }
}

/// The workspace's automatic-run setting and the agents it may use.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AutoSetting {
    /// `auto_run.mcp`: MCP clients may start agents on their own.
    pub mcp: bool,
    /// `auto_run.contextcode`: `off`, `ask` or `auto`.
    #[serde(default)]
    pub contextcode: String,
    #[serde(default)]
    pub per_run_credit_limit: Option<i64>,
    /// Why the workspace's runs cannot start now, when they cannot.
    #[serde(default)]
    pub blocked: Option<String>,
    #[serde(default)]
    pub agents: Vec<AutoAgent>,
}

impl AutoSetting {
    /// Parse the API's answer, with or without its `{"success", "data"}` envelope. `None`
    /// for a body that is not this answer.
    pub fn from_value(value: &Value) -> Option<Self> {
        let value = value
            .get("data")
            .filter(|_| value.get("success").is_some())
            .unwrap_or(value);
        let auto_run = value.get("auto_run").filter(|v| v.is_object())?;
        let blocked = value
            .get("blocked")
            .filter(|b| !b.is_null())
            .map(|blocked| {
                blocked
                    .get("message")
                    .or_else(|| blocked.get("code"))
                    .and_then(Value::as_str)
                    .unwrap_or("blocked")
                    .to_string()
            });
        let agents = value
            .get("agents")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| serde_json::from_value::<AutoAgent>(item.clone()).ok())
                    .filter(|agent| !agent.id.trim().is_empty())
                    .collect()
            })
            .unwrap_or_default();
        Some(Self {
            mcp: auto_run.get("mcp").and_then(Value::as_bool) == Some(true),
            contextcode: auto_run
                .get("contextcode")
                .and_then(Value::as_str)
                .unwrap_or("off")
                .to_string(),
            per_run_credit_limit: auto_run.get("per_run_credit_limit").and_then(Value::as_i64),
            blocked,
            agents,
        })
    }

    /// MCP clients may start runs now.
    pub fn is_on(&self) -> bool {
        self.mcp && self.blocked.is_none()
    }

    /// The agent `id` (or `id@version`), when an automatic run may use it.
    pub fn runnable(&self, agent: &str) -> Option<&AutoAgent> {
        let id = super::catalog::agent_id_of(agent);
        self.agents.iter().find(|a| a.id == id && a.may_run())
    }
}

/// What one read of the setting found.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SettingRead {
    /// The API answered with the setting.
    Read { setting: AutoSetting },
    /// An API without the route (404/405), or one that refuses this caller (401/403):
    /// automatic runs are off.
    Unsupported,
    /// The read failed (network, timeout, 5xx); tried again soon.
    Unreachable,
}

impl SettingRead {
    /// The setting when automatic runs are on.
    pub fn on(&self) -> Option<&AutoSetting> {
        match self {
            Self::Read { setting } if setting.is_on() => Some(setting),
            _ => None,
        }
    }

    fn ttl_secs(&self) -> i64 {
        match self {
            Self::Read { .. } => SETTING_TTL_SECS,
            Self::Unsupported => UNSUPPORTED_TTL_SECS,
            Self::Unreachable => UNREACHABLE_RETRY_SECS,
        }
    }

    fn from_result(result: Result<Value, Error>) -> Self {
        match result {
            Ok(value) => match AutoSetting::from_value(&value) {
                Some(setting) => Self::Read { setting },
                None => Self::Unreachable,
            },
            Err(Error::Http {
                status: 401 | 403 | 404 | 405,
                ..
            }) => Self::Unsupported,
            Err(_) => Self::Unreachable,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedRead {
    read: SettingRead,
    /// Unix seconds.
    read_at: i64,
}

impl CachedRead {
    fn fresh_at(&self, now: i64) -> bool {
        now < self.read_at + self.read.ttl_secs()
    }
}

fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

/// The caller's secret-free identity: the hosted gateway's per-principal one, or a hash of
/// the credential a local process runs with.
pub async fn caller(client: &ContextStreamClient) -> Option<String> {
    super::caller_identity(client).await
}

/// The folder for this machine's shared state: `~/.contextstream/agents-auto`.
/// `CONTEXTSTREAM_AGENTS_AUTO_DIR` moves it (tests).
pub fn state_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CONTEXTSTREAM_AGENTS_AUTO_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    dirs::home_dir().map(|home| home.join(".contextstream").join("agents-auto"))
}

fn short_hash(text: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(text.as_bytes()))[..16].to_string()
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Write through a temporary file and a rename, owner-only.
fn write_json<T: Serialize>(path: &Path, value: &T) {
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let Ok(bytes) = serde_json::to_vec(value) else {
        return;
    };
    let temporary = path.with_extension(format!("{}.tmp", Uuid::new_v4().simple()));
    let written = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&bytes)?;
        std::fs::rename(&temporary, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
}

/// Reads of the setting, kept per caller and workspace.
#[derive(Default)]
pub struct SettingCache {
    entries: Mutex<HashMap<(String, Uuid), CachedRead>>,
}

impl SettingCache {
    /// The cache every registry shares.
    pub fn global() -> Arc<Self> {
        static CACHE: OnceLock<Arc<SettingCache>> = OnceLock::new();
        CACHE.get_or_init(|| Arc::new(Self::default())).clone()
    }

    fn file(identity: &str, workspace_id: Uuid) -> Option<PathBuf> {
        state_dir().map(|dir| {
            dir.join(format!(
                "setting-{}.json",
                short_hash(&format!("{identity}|{workspace_id}"))
            ))
        })
    }

    fn remember(&self, key: (String, Uuid), cached: CachedRead) {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        if entries.len() > 4096 {
            entries.clear();
        }
        entries.insert(key, cached);
    }

    /// The setting, read when the kept one is older than [`SETTING_TTL_SECS`]: at most
    /// `budget` is spent waiting. `shared` keeps it in the machine's state folder too, for
    /// the hooks and the stdio server of one person.
    pub async fn read(
        &self,
        client: &ContextStreamClient,
        identity: &str,
        workspace_id: Uuid,
        budget: Duration,
        shared: bool,
    ) -> SettingRead {
        let now = now_secs();
        let key = (identity.to_string(), workspace_id);
        let kept = self
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned();
        if let Some(kept) = kept.as_ref().filter(|k| k.fresh_at(now)) {
            return kept.read.clone();
        }
        let file = (shared && FILES_ALLOWED)
            .then(|| Self::file(identity, workspace_id))
            .flatten();
        if let Some(path) = &file {
            if let Some(cached) = read_json::<CachedRead>(path).filter(|c| c.fresh_at(now)) {
                self.remember(key, cached.clone());
                return cached.read;
            }
        }
        let path = format!("/workspaces/{workspace_id}/tasks/agents/auto");
        let fetched = match tokio::time::timeout(budget, client.get_once::<Value>(&path)).await {
            Ok(result) => SettingRead::from_result(result),
            Err(_) => SettingRead::Unreachable,
        };
        // A failed read keeps serving what was read before it, until the retry.
        let cached = match (fetched, kept) {
            (SettingRead::Unreachable, Some(previous))
                if !matches!(previous.read, SettingRead::Unreachable) =>
            {
                let ttl = previous.read.ttl_secs();
                CachedRead {
                    read: previous.read,
                    read_at: now - ttl + UNREACHABLE_RETRY_SECS,
                }
            }
            (fetched, _) => CachedRead {
                read: fetched,
                read_at: now,
            },
        };
        let read = cached.read.clone();
        if let Some(path) = &file {
            write_json(path, &cached);
        }
        self.remember(key, cached);
        read
    }

    /// The kept setting, without reading: what a listing shows between reads.
    pub fn peek(&self, identity: &str, workspace_id: Uuid) -> Option<SettingRead> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(identity.to_string(), workspace_id))
            .map(|cached| cached.read.clone())
    }

    /// Forget the kept setting (a refusal says it changed).
    pub fn forget(&self, identity: &str, workspace_id: Uuid) {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(identity.to_string(), workspace_id));
        if let Some(path) = Self::file(identity, workspace_id) {
            let _ = std::fs::remove_file(path);
        }
    }

    #[cfg(test)]
    pub(crate) fn set_for_test(&self, identity: &str, workspace_id: Uuid, read: SettingRead) {
        self.remember(
            (identity.to_string(), workspace_id),
            CachedRead {
                read,
                read_at: now_secs(),
            },
        );
    }
}

// ---------------------------------------------------------------------------------------
// The ledger: once per task, quiet after a refusal, runs to report
// ---------------------------------------------------------------------------------------

/// A run started automatically whose result the agent has not been told yet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingRun {
    pub caller: String,
    pub workspace_id: Uuid,
    pub task_id: Uuid,
    pub agent: String,
    pub started_at: i64,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct LedgerState {
    /// `caller|workspace|project|agent` → when it was last suggested.
    #[serde(default)]
    suggested: BTreeMap<String, i64>,
    /// `caller|workspace|project|agent` → when it was last started automatically.
    #[serde(default)]
    started: BTreeMap<String, i64>,
    /// `caller|workspace` → until when nothing is suggested.
    #[serde(default)]
    paused: BTreeMap<String, i64>,
    #[serde(default)]
    pending: Vec<PendingRun>,
}

impl LedgerState {
    fn prune(&mut self, now: i64) {
        self.suggested
            .retain(|_, at| *at + SUGGEST_WINDOW_SECS > now);
        self.started.retain(|_, at| *at + STARTED_WINDOW_SECS > now);
        self.paused.retain(|_, until| *until > now);
        self.pending
            .retain(|run| run.started_at + 24 * 60 * 60 > now);
    }
}

/// Where suggestions and automatic runs are remembered: in memory (the hosted gateway, per
/// caller) or in the machine's state folder (the stdio server and the hooks share it).
pub struct Ledger {
    file: Option<PathBuf>,
    memory: Mutex<LedgerState>,
}

/// The key of one agent in one project for one caller.
fn agent_key(caller: &str, workspace_id: Uuid, project_id: Option<Uuid>, agent: &str) -> String {
    format!(
        "{caller}|{workspace_id}|{}|{}",
        project_id.map_or_else(|| "-".to_string(), |p| p.to_string()),
        super::catalog::agent_id_of(agent)
    )
}

impl Ledger {
    pub fn in_memory() -> Self {
        Self {
            file: None,
            memory: Mutex::default(),
        }
    }

    pub fn in_file(path: PathBuf) -> Self {
        Self {
            file: Some(path),
            memory: Mutex::default(),
        }
    }

    /// The process's ledger: shared on disk for a local (stdio or hook) process, in memory
    /// for the hosted gateway.
    pub fn for_process(shared: bool) -> Arc<Self> {
        static FILE: OnceLock<Arc<Ledger>> = OnceLock::new();
        static MEMORY: OnceLock<Arc<Ledger>> = OnceLock::new();
        if shared && FILES_ALLOWED {
            if let Some(dir) = state_dir() {
                return FILE
                    .get_or_init(|| Arc::new(Self::in_file(dir.join("ledger.json"))))
                    .clone();
            }
        }
        MEMORY.get_or_init(|| Arc::new(Self::in_memory())).clone()
    }

    fn with<T>(&self, update: impl FnOnce(&mut LedgerState) -> T) -> T {
        let now = now_secs();
        let mut memory = self.memory.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(path) = &self.file {
            // The lock above orders this process's writers; another process's write in
            // between is a lost suggestion record at worst.
            *memory = read_json(path).unwrap_or_default();
        }
        memory.prune(now);
        let result = update(&mut memory);
        if let Some(path) = &self.file {
            write_json(path, &*memory);
        }
        result
    }

    fn read<T>(&self, look: impl FnOnce(&LedgerState) -> T) -> T {
        let now = now_secs();
        let mut state = match &self.file {
            Some(path) => read_json(path).unwrap_or_default(),
            None => self
                .memory
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone(),
        };
        state.prune(now);
        look(&state)
    }

    /// Nothing is suggested in the workspace: a refusal came in the last hour.
    pub fn is_paused(&self, caller: &str, workspace_id: Uuid) -> bool {
        let key = format!("{caller}|{workspace_id}");
        self.read(|state| state.paused.contains_key(&key))
    }

    /// Whether `agent` may be suggested now: not paused, not suggested for this task, not
    /// started lately.
    pub fn may_suggest(
        &self,
        caller: &str,
        workspace_id: Uuid,
        project_id: Option<Uuid>,
        agent: &str,
    ) -> bool {
        let pause = format!("{caller}|{workspace_id}");
        let key = agent_key(caller, workspace_id, project_id, agent);
        self.read(|state| {
            !state.paused.contains_key(&pause)
                && !state.suggested.contains_key(&key)
                && !state.started.contains_key(&key)
        })
    }

    pub fn mark_suggested(
        &self,
        caller: &str,
        workspace_id: Uuid,
        project_id: Option<Uuid>,
        agents: &[&str],
    ) {
        let now = now_secs();
        self.with(|state| {
            for agent in agents {
                state
                    .suggested
                    .insert(agent_key(caller, workspace_id, project_id, agent), now);
            }
        });
    }

    pub fn mark_started(
        &self,
        caller: &str,
        workspace_id: Uuid,
        project_id: Option<Uuid>,
        agent: &str,
        task_id: Uuid,
    ) {
        let now = now_secs();
        self.with(|state| {
            state
                .started
                .insert(agent_key(caller, workspace_id, project_id, agent), now);
            state.pending.retain(|run| run.task_id != task_id);
            state.pending.push(PendingRun {
                caller: caller.to_string(),
                workspace_id,
                task_id,
                agent: super::catalog::agent_id_of(agent).to_string(),
                started_at: now,
            });
            let mine = state.pending.iter().filter(|r| r.caller == caller).count();
            if mine > MAX_PENDING {
                if let Some(oldest) = state.pending.iter().position(|r| r.caller == caller) {
                    state.pending.remove(oldest);
                }
            }
        });
    }

    /// Stop suggesting in the workspace for [`REFUSAL_PAUSE_SECS`].
    pub fn pause(&self, caller: &str, workspace_id: Uuid) {
        let until = now_secs() + REFUSAL_PAUSE_SECS;
        self.with(|state| {
            state
                .paused
                .insert(format!("{caller}|{workspace_id}"), until);
        });
    }

    /// The caller's runs in the workspace that the agent has not heard the end of.
    pub fn pending(&self, caller: &str, workspace_id: Uuid) -> Vec<PendingRun> {
        self.read(|state| {
            state
                .pending
                .iter()
                .filter(|run| run.caller == caller && run.workspace_id == workspace_id)
                .cloned()
                .collect()
        })
    }

    pub fn reported(&self, task_ids: &[Uuid]) {
        if task_ids.is_empty() {
            return;
        }
        self.with(|state| state.pending.retain(|run| !task_ids.contains(&run.task_id)));
    }
}

// ---------------------------------------------------------------------------------------
// The repository, for an agent's input
// ---------------------------------------------------------------------------------------

/// What the session knows about the repository it works in.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RepoFacts {
    /// The folder was read: a missing GitHub remote means the repository has none.
    pub inspected: bool,
    /// `owner/name` of the GitHub `origin`.
    pub github: Option<String>,
    /// The branch `origin/HEAD` names, else the checked-out one.
    pub default_branch: Option<String>,
    /// The newest tag reachable from HEAD, when known already.
    pub last_tag: Option<String>,
    /// The folder, to ask git for the newest tag when an input needs it.
    pub folder: Option<PathBuf>,
}

/// `owner/name` from a GitHub remote URL in any of its usual forms.
pub fn github_repo(url: &str) -> Option<String> {
    let url = url.trim();
    let rest = url
        .strip_prefix("git@github.com:")
        .or_else(|| {
            url.split_once("github.com/")
                .filter(|(scheme, _)| {
                    scheme.is_empty()
                        || scheme.ends_with("://")
                        || scheme.ends_with('@')
                        || scheme.ends_with("://www.")
                })
                .map(|(_, rest)| rest)
        })
        .or_else(|| url.split_once("github.com:").map(|(_, rest)| rest))?;
    let rest = rest.trim_end_matches('/');
    let rest = rest.strip_suffix(".git").unwrap_or(rest);
    let mut parts = rest.split('/');
    let owner = parts.next()?.trim();
    let name = parts.next()?.trim();
    let valid = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    (valid(owner) && valid(name)).then(|| format!("{owner}/{name}"))
}

/// The repository's git folder: `.git`, or the folder a worktree's `.git` file names.
fn git_dir(folder: &Path) -> Option<PathBuf> {
    let mut dir = folder.to_path_buf();
    loop {
        let candidate = dir.join(".git");
        if candidate.is_dir() {
            return Some(candidate);
        }
        if candidate.is_file() {
            let text = std::fs::read_to_string(&candidate).ok()?;
            let target = text.trim().strip_prefix("gitdir:")?.trim();
            let target = PathBuf::from(target);
            return Some(if target.is_absolute() {
                target
            } else {
                dir.join(target)
            });
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// The `origin` URL from a git config's text.
fn origin_url(config: &str) -> Option<String> {
    let mut in_origin = false;
    for line in config.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_origin = line == "[remote \"origin\"]";
            continue;
        }
        if in_origin {
            if let Some((key, value)) = line.split_once('=') {
                if key.trim() == "url" {
                    return Some(value.trim().to_string());
                }
            }
        }
    }
    None
}

impl RepoFacts {
    /// Read the folder's git metadata (files only; no git process).
    pub fn from_folder(folder: &str) -> Self {
        let folder = Path::new(folder);
        let Some(git) = git_dir(folder) else {
            return Self {
                inspected: folder.is_dir(),
                ..Self::default()
            };
        };
        // A worktree's own folder holds HEAD; its config is in the common folder.
        let common = std::fs::read_to_string(git.join("commondir"))
            .ok()
            .map(|c| {
                let c = PathBuf::from(c.trim());
                if c.is_absolute() {
                    c
                } else {
                    git.join(c)
                }
            })
            .unwrap_or_else(|| git.clone());
        let github = std::fs::read_to_string(common.join("config"))
            .ok()
            .and_then(|config| origin_url(&config))
            .and_then(|url| github_repo(&url));
        let branch_of = |text: String, prefix: &str| {
            text.trim()
                .strip_prefix(prefix)
                .map(str::to_string)
                .filter(|b| !b.is_empty())
        };
        let default_branch = std::fs::read_to_string(common.join("refs/remotes/origin/HEAD"))
            .ok()
            .and_then(|text| branch_of(text, "ref: refs/remotes/origin/"))
            .or_else(|| {
                std::fs::read_to_string(git.join("HEAD"))
                    .ok()
                    .and_then(|text| branch_of(text, "ref: refs/heads/"))
            });
        Self {
            inspected: true,
            github,
            default_branch,
            last_tag: None,
            folder: Some(folder.to_path_buf()),
        }
    }

    /// The newest tag: the known one, else one short `git describe` in the folder.
    pub fn tag(&self) -> Option<String> {
        if self.last_tag.is_some() {
            return self.last_tag.clone();
        }
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(self.folder.as_ref()?)
            .args(["describe", "--tags", "--abbrev=0"])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()
            .filter(|out| out.status.success())?;
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string()).filter(|t| !t.is_empty())
    }
}

// ---------------------------------------------------------------------------------------
// Suggestions
// ---------------------------------------------------------------------------------------

/// What brought the moment up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// The person's request.
    Request,
    /// A step the agent is taking (a command or an edit).
    Step,
}

/// One agent to start, with its input.
#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
    pub agent: AutoAgent,
    pub why: String,
    pub input: Map<String, Value>,
    /// The input still has `<...>` for the agent to fill in.
    pub has_placeholders: bool,
}

/// The input `agent` needs, from what the session knows; `None` when a required field
/// cannot be filled or guessed (the run would fail).
pub fn fill_input(
    agent: &AutoAgent,
    prompt: Option<&str>,
    repo: &RepoFacts,
) -> Option<(Map<String, Value>, bool)> {
    let mut input = Map::new();
    let mut placeholders = false;
    let mut placeholder = |text: &str| {
        placeholders = true;
        Value::String(text.to_string())
    };
    for field in &agent.inputs_required {
        let value = match field.as_str() {
            // The tool fills it from the session.
            "project_id" => continue,
            "repo_url" => match (&repo.github, repo.inspected) {
                (Some(github), _) => json!(format!("https://github.com/{github}")),
                (None, true) => return None,
                (None, false) => placeholder("<this repository's GitHub URL>"),
            },
            "repo" => match (&repo.github, repo.inspected) {
                (Some(github), _) => json!(github),
                (None, true) => return None,
                (None, false) => placeholder("<owner/name of this repository on GitHub>"),
            },
            "question" => {
                let question: String = prompt?.split_whitespace().collect::<Vec<_>>().join(" ");
                if question.is_empty() {
                    return None;
                }
                json!(question.chars().take(500).collect::<String>())
            }
            "pull" => json!(prompt.and_then(moments::pull_number)?),
            "base" => match repo.tag() {
                Some(tag) => json!(tag),
                None => placeholder("<the last release tag>"),
            },
            "head" => match &repo.default_branch {
                Some(branch) => json!(branch),
                None => placeholder("<the branch to release, e.g. main>"),
            },
            _ => return None,
        };
        input.insert(field.clone(), value);
    }
    Some((input, placeholders))
}

/// The agents that take `moments`, at most [`MAX_SUGGESTIONS`], each once.
pub fn suggestions(
    moments: &[&Moment],
    setting: &AutoSetting,
    prompt: Option<&str>,
    repo: &RepoFacts,
) -> Vec<Suggestion> {
    let mut out: Vec<Suggestion> = Vec::new();
    for moment in moments {
        let Some(agent) = moments::agent_for(moment, &setting.agents) else {
            continue;
        };
        if out.iter().any(|s| s.agent.id == agent.id) {
            continue;
        }
        let Some((input, has_placeholders)) = fill_input(agent, prompt, repo) else {
            continue;
        };
        out.push(Suggestion {
            agent: agent.clone(),
            why: moment.why.clone(),
            input,
            has_placeholders,
        });
        if out.len() == MAX_SUGGESTIONS {
            break;
        }
    }
    out
}

/// The hint: what fits, what the agent does, and the exact call to start it, first. `tool`
/// is the agents tool as the client names it (`agents`, or `mcp__contextstream__agents`).
pub fn render_hint(suggestions: &[Suggestion], trigger: Trigger, tool: &str) -> String {
    let what = match trigger {
        Trigger::Request => "this task",
        Trigger::Step => "this step",
    };
    let mut text = format!(
        "[CONTEXTAGENTS] Do this first: this workspace turned on automatic ContextAgent runs, and {what} fits {}. Start it now with the call below, without asking. It works in ContextStream's own sandbox, in the background, so it costs you no time:\n",
        if suggestions.len() == 1 { "one" } else { "these" }
    );
    for suggestion in suggestions {
        let input = Value::Object(suggestion.input.clone()).to_string();
        let job: String = suggestion
            .agent
            .job
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        let job = if job.chars().count() > 160 {
            format!("{}…", job.chars().take(159).collect::<String>())
        } else {
            job
        };
        text.push_str(&format!(
            "- {} ({}): {job}\n  {tool}(action=\"run\", agent=\"{}\", input={input}, automatic=true)\n",
            suggestion.agent.display_name(),
            suggestion.why,
            suggestion.agent.id,
        ));
    }
    if suggestions.iter().any(|s| s.has_placeholders) {
        text.push_str("Fill in each <...> first. ");
    }
    text.push_str(&format!(
        "Then carry on with the task yourself. When your part is done, check the run once with {tool}(action=\"get_run\", task_id=\"<the run's task_id>\") and fold what it found into your work and your answer; if it is still running, say so and give its task_id. Skip it only if the person said not to use agents."
    ));
    text
}

/// Everything one suggestion needs from its caller.
pub struct SuggestScope<'a> {
    pub client: &'a ContextStreamClient,
    /// A secret-free identity of the caller (see [`super::caller_identity`]).
    pub caller: &'a str,
    pub workspace_id: Uuid,
    pub project_id: Option<Uuid>,
    /// Local process (stdio server or hook): the setting and ledger live on disk.
    pub shared: bool,
    /// How long the setting may be waited for when it is not kept.
    pub budget: Duration,
}

/// The hint for `moments`, or `None`: nothing fits, the setting is off (or unknown), or every
/// fitting agent was suggested for this task already. Reads the setting only when a moment
/// fits; records what it suggests.
pub async fn suggest(
    scope: &SuggestScope<'_>,
    moments: &[&Moment],
    prompt: Option<&str>,
    repo: impl FnOnce() -> RepoFacts,
    trigger: Trigger,
    tool: &str,
) -> Option<String> {
    if moments.is_empty() {
        return None;
    }
    let ledger = Ledger::for_process(scope.shared);
    if ledger.is_paused(scope.caller, scope.workspace_id) {
        return None;
    }
    let read = SettingCache::global()
        .read(
            scope.client,
            scope.caller,
            scope.workspace_id,
            scope.budget,
            scope.shared,
        )
        .await;
    let setting = read.on()?;
    let repo = repo();
    let fresh: Vec<Suggestion> = suggestions(moments, setting, prompt, &repo)
        .into_iter()
        .filter(|s| {
            ledger.may_suggest(
                scope.caller,
                scope.workspace_id,
                scope.project_id,
                &s.agent.id,
            )
        })
        .collect();
    if fresh.is_empty() {
        return None;
    }
    let ids: Vec<&str> = fresh.iter().map(|s| s.agent.id.as_str()).collect();
    ledger.mark_suggested(scope.caller, scope.workspace_id, scope.project_id, &ids);
    Some(render_hint(&fresh, trigger, tool))
}

/// One line about a finished automatic run, from its task view.
pub fn finished_line(run: &PendingRun, task: &Value, tool: &str) -> Option<String> {
    if task.get("done").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let status = task
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("ended");
    let output = task.get("output").filter(|o| !o.is_null());
    let one_line = |text: &str| -> String {
        let text: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.chars().count() > 220 {
            format!("{}…", text.chars().take(219).collect::<String>())
        } else {
            text
        }
    };
    let detail = output
        .and_then(|o| o.get("summary"))
        .and_then(Value::as_str)
        .or_else(|| {
            output
                .and_then(|o| o.pointer("/declined/reason"))
                .and_then(Value::as_str)
        })
        .or_else(|| task.get("error").and_then(Value::as_str))
        .map(one_line)
        .filter(|d| !d.is_empty())
        .map(|d| format!(": {d}"))
        .unwrap_or_default();
    Some(format!(
        "- {} {status}{detail} Full result: {tool}(action=\"get_run\", task_id=\"{}\").",
        run.agent, run.task_id
    ))
}

/// The block reporting finished automatic runs.
pub fn render_finished(lines: &[String]) -> Option<String> {
    (!lines.is_empty()).then(|| {
        format!(
            "[CONTEXTAGENTS] Automatic runs that finished since you started them; fold what they found into your work:\n{}",
            lines.join("\n")
        )
    })
}

/// Read the caller's automatic runs that are still to report, at most `budget` in all, and
/// say which finished. Reported runs leave the ledger.
pub async fn finished_runs(
    client: &ContextStreamClient,
    caller: &str,
    workspace_id: Uuid,
    shared: bool,
    budget: Duration,
    tool: &str,
) -> Option<String> {
    let ledger = Ledger::for_process(shared);
    let pending = ledger.pending(caller, workspace_id);
    if pending.is_empty() {
        return None;
    }
    let reads = pending.iter().map(|run| {
        let path = format!("/workspaces/{}/tasks/{}", run.workspace_id, run.task_id);
        async move {
            let task = client
                .get_once::<Value>(&path)
                .await
                .ok()
                .map(super::unwrap_data);
            (run, task)
        }
    });
    let results = tokio::time::timeout(budget, futures::future::join_all(reads))
        .await
        .ok()?;
    let mut lines = Vec::new();
    let mut done = Vec::new();
    for (run, task) in results {
        if let Some(line) = task.and_then(|task| finished_line(run, &task, tool)) {
            lines.push(line);
            done.push(run.task_id);
        }
    }
    ledger.reported(&done);
    render_finished(&lines)
}

/// How long `context()` waits for its ContextAgents block: the setting (when not kept) and
/// the automatic runs to report. It runs alongside the main context read.
pub const CONTEXT_BUDGET: Duration = Duration::from_millis(1500);

/// What `context()` adds: the hint when the request fits an agent, and the automatic runs
/// that finished. Reads nothing when no moment fits and no run is waiting. A local process
/// (`shared`) reads the folder's git metadata for the agent's input; the hosted gateway
/// leaves `<...>` for the agent to fill.
pub async fn context_block(
    client: &ContextStreamClient,
    workspace_id: Uuid,
    project_id: Option<Uuid>,
    folder: Option<&str>,
    prompt: &str,
) -> Option<String> {
    let caller = super::caller_identity(client).await?;
    let shared = !client.config().await.is_http_transport;
    let fitting = moments::prompt_moments(prompt);
    let scope = SuggestScope {
        client,
        caller: &caller,
        workspace_id,
        project_id,
        shared,
        budget: CONTEXT_BUDGET,
    };
    let repo = || match folder {
        Some(folder) if shared => RepoFacts::from_folder(folder),
        _ => RepoFacts::default(),
    };
    let (hint, finished) = tokio::join!(
        suggest(
            &scope,
            &fitting,
            Some(prompt),
            repo,
            Trigger::Request,
            "agents"
        ),
        finished_runs(
            client,
            &caller,
            workspace_id,
            shared,
            CONTEXT_BUDGET,
            "agents"
        ),
    );
    match (hint, finished) {
        (None, None) => None,
        (Some(one), None) | (None, Some(one)) => Some(one),
        (Some(hint), Some(finished)) => Some(format!("{finished}\n{hint}")),
    }
}

/// Whether a refused automatic run's error is the API refusing it (the setting, the agent,
/// a limit): nothing ran, and suggestions stop for a while.
pub fn is_refusal(error: &Error) -> bool {
    matches!(error, Error::Http { status, .. } if (400..500).contains(status))
        || matches!(error, Error::InsufficientCredits { .. })
}

#[cfg(test)]
#[path = "auto_tests.rs"]
mod tests;
