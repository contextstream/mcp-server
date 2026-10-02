//! Per-workspace prompt state tracking.
//!
//! Tracks whether a workspace requires an initial ContextStream `context(...)`
//! call for the current user prompt before other MCP tools execute.
//!
//! State file: `~/.contextstream/prompt-state.json`

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Initialization belongs to the editor's host session, rather than its cwd.
/// Keep this state separate from the legacy prompt file: older hook binaries
/// and concurrent sessions may still update that file using folder-only keys.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct SessionInitStateFile {
    sessions: HashMap<String, SessionInitStateEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SessionInitStateEntry {
    require_init: bool,
    updated_at: String,
}

const SESSION_INIT_RETENTION_DAYS: i64 = 7;
const MAX_SESSION_INIT_ENTRIES: usize = 8192;

fn session_init_state_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".contextstream").join("session-init-state.json"))
}

/// Tool arguments contain an independently chosen ContextStream session id;
/// only the top-level editor session identity can isolate the hook gate.
fn session_init_key(input: &Value) -> Option<String> {
    let host_session_id = ["session_id", "sessionId"].iter().find_map(|key| {
        input
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
    })?;
    Some(hex::encode(Sha256::digest(host_session_id.as_bytes())))
}

fn prune_session_init_state(state: &mut SessionInitStateFile) {
    let cutoff = chrono::Utc::now() - chrono::Duration::days(SESSION_INIT_RETENTION_DAYS);
    state.sessions.retain(|_, entry| {
        parse_rfc3339_utc(&entry.updated_at).is_some_and(|updated_at| updated_at >= cutoff)
    });
    if state.sessions.len() > MAX_SESSION_INIT_ENTRIES {
        let mut oldest: Vec<_> = state
            .sessions
            .iter()
            .map(|(key, entry)| (entry.updated_at.clone(), key.clone()))
            .collect();
        oldest.sort_unstable();
        let remove_count = oldest.len() - MAX_SESSION_INIT_ENTRIES;
        for (_, key) in oldest.into_iter().take(remove_count) {
            state.sessions.remove(&key);
        }
    }
}

fn write_session_init_state(path: &Path, state: &SessionInitStateFile) -> std::io::Result<()> {
    let json = serde_json::to_vec(state).map_err(std::io::Error::other)?;
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("session-init-state.json");
    let temporary_path = path.with_file_name(format!("{filename}.{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut temporary = options.open(&temporary_path)?;
        temporary.write_all(&json)?;
        temporary.sync_all()?;
        // The temporary file shares the destination directory, so replacement
        // is atomic and readers never observe a partially written JSON file.
        std::fs::rename(&temporary_path, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary_path);
    }
    result
}

fn with_session_init_state<T>(
    path: &Path,
    update: impl FnOnce(&mut SessionInitStateFile) -> T,
) -> std::io::Result<T> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Lock a stable sibling, not the state inode that atomic replacement
    // changes. File drop releases the advisory lock on every return path.
    let lock_path = path.with_extension("lock");
    let lock: File = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;
    fs2::FileExt::lock_exclusive(&lock)?;
    let mut state = match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(std::io::Error::other)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            SessionInitStateFile::default()
        }
        Err(error) => return Err(error),
    };
    prune_session_init_state(&mut state);
    let result = update(&mut state);
    prune_session_init_state(&mut state);
    write_session_init_state(path, &state)?;
    Ok(result)
}

fn mark_session_init_required_at(path: &Path, key: &str) -> std::io::Result<()> {
    with_session_init_state(path, |state| {
        let now = chrono::Utc::now().to_rfc3339();
        // Repeated startup/resume/compact events must not re-arm a session
        // that has already completed its initialization call.
        let entry = state
            .sessions
            .entry(key.to_owned())
            .or_insert(SessionInitStateEntry {
                require_init: true,
                updated_at: now.clone(),
            });
        entry.updated_at = now;
    })
}

fn clear_session_init_required_at(path: &Path, key: &str) -> std::io::Result<()> {
    with_session_init_state(path, |state| {
        // Also record success when an editor omitted SessionStart. A later
        // replayed SessionStart then recognizes this session as initialized.
        state.sessions.insert(
            key.to_owned(),
            SessionInitStateEntry {
                require_init: false,
                updated_at: chrono::Utc::now().to_rfc3339(),
            },
        );
    })
}

fn is_session_init_required_at(path: &Path, key: &str) -> std::io::Result<bool> {
    with_session_init_state(path, |state| {
        state.sessions.get_mut(key).is_some_and(|entry| {
            entry.updated_at = chrono::Utc::now().to_rfc3339();
            entry.require_init
        })
    })
}

pub fn mark_session_init_required(cwd: &str, input: &Value) {
    let Some(key) = session_init_key(input) else {
        mark_init_required(cwd);
        return;
    };
    if let Some(path) = session_init_state_path() {
        let _ = mark_session_init_required_at(&path, &key);
    }
}

pub fn clear_session_init_required(cwd: &str, input: &Value) {
    let Some(key) = session_init_key(input) else {
        clear_init_required(cwd);
        return;
    };
    if let Some(path) = session_init_state_path() {
        let _ = clear_session_init_required_at(&path, &key);
    }
}

pub fn is_session_init_required(cwd: &str, input: &Value) -> bool {
    let Some(key) = session_init_key(input) else {
        return is_init_required(cwd);
    };
    // Identified sessions never inherit another session's legacy cwd gate.
    session_init_state_path()
        .and_then(|path| is_session_init_required_at(&path, &key).ok())
        .unwrap_or(false)
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PromptStateFile {
    workspaces: HashMap<String, PromptStateEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PromptStateEntry {
    require_context: bool,
    #[serde(default)]
    require_init: bool,
    #[serde(default)]
    last_context_at: Option<String>,
    #[serde(default)]
    last_state_change_at: Option<String>,
    #[serde(default)]
    index_wait_started_at: Option<String>,
    #[serde(default)]
    index_wait_until: Option<String>,
    updated_at: String,
}

fn state_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".contextstream").join("prompt-state.json"))
}

fn read_state() -> PromptStateFile {
    let Some(path) = state_path() else {
        return PromptStateFile::default();
    };
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default()
}

fn write_state(state: &PromptStateFile) -> bool {
    let Some(path) = state_path() else {
        return false;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    serde_json::to_string_pretty(state)
        .ok()
        .and_then(|json| std::fs::write(&path, json).ok())
        .is_some()
}

fn workspace_paths_match(tracked_cwd: &str, cwd: &str) -> bool {
    cwd.starts_with(tracked_cwd) || tracked_cwd.starts_with(cwd)
}

fn parse_rfc3339_utc(value: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|ts| ts.with_timezone(&chrono::Utc))
}

fn entry_mut_for_cwd<'a>(
    workspaces: &'a mut HashMap<String, PromptStateEntry>,
    cwd: &str,
) -> Option<&'a mut PromptStateEntry> {
    if workspaces.contains_key(cwd) {
        return workspaces.get_mut(cwd);
    }
    let key = workspaces
        .keys()
        .find(|tracked_cwd| workspace_paths_match(tracked_cwd, cwd))
        .cloned()?;
    workspaces.get_mut(&key)
}

pub fn mark_context_required(cwd: &str) {
    if cwd.trim().is_empty() {
        return;
    }
    let mut state = read_state();
    let now = chrono::Utc::now().to_rfc3339();
    let entry = state
        .workspaces
        .entry(cwd.to_string())
        .or_insert(PromptStateEntry {
            require_context: false,
            require_init: false,
            last_context_at: None,
            last_state_change_at: None,
            index_wait_started_at: None,
            index_wait_until: None,
            updated_at: now.clone(),
        });
    entry.require_context = true;
    entry.updated_at = now;
    write_state(&state);
}

pub fn clear_context_required(cwd: &str) {
    if cwd.trim().is_empty() {
        return;
    }
    let mut state = read_state();
    if let Some(entry) = entry_mut_for_cwd(&mut state.workspaces, cwd) {
        entry.require_context = false;
        entry.last_context_at = Some(chrono::Utc::now().to_rfc3339());
        entry.index_wait_started_at = None;
        entry.index_wait_until = None;
        entry.updated_at = chrono::Utc::now().to_rfc3339();
        write_state(&state);
    }
}

pub fn mark_init_required(cwd: &str) {
    if cwd.trim().is_empty() {
        return;
    }
    let mut state = read_state();
    let now = chrono::Utc::now().to_rfc3339();
    let entry = state
        .workspaces
        .entry(cwd.to_string())
        .or_insert(PromptStateEntry {
            require_context: false,
            require_init: false,
            last_context_at: None,
            last_state_change_at: None,
            index_wait_started_at: None,
            index_wait_until: None,
            updated_at: now.clone(),
        });
    entry.require_init = true;
    entry.updated_at = now;
    write_state(&state);
}

pub fn clear_init_required(cwd: &str) {
    if cwd.trim().is_empty() {
        return;
    }
    let mut state = read_state();
    if let Some(entry) = entry_mut_for_cwd(&mut state.workspaces, cwd) {
        entry.require_init = false;
        entry.updated_at = chrono::Utc::now().to_rfc3339();
        write_state(&state);
    }
}

pub fn is_init_required(cwd: &str) -> bool {
    if cwd.trim().is_empty() {
        return false;
    }
    let state = read_state();

    if let Some(entry) = state.workspaces.get(cwd) {
        return entry.require_init;
    }

    for (tracked_cwd, entry) in &state.workspaces {
        if workspace_paths_match(tracked_cwd, cwd) {
            return entry.require_init;
        }
    }

    false
}

pub fn mark_state_changed(cwd: &str) {
    if cwd.trim().is_empty() {
        return;
    }
    let mut state = read_state();
    let now = chrono::Utc::now().to_rfc3339();
    let entry = state
        .workspaces
        .entry(cwd.to_string())
        .or_insert(PromptStateEntry {
            require_context: false,
            require_init: false,
            last_context_at: None,
            last_state_change_at: None,
            index_wait_started_at: None,
            index_wait_until: None,
            updated_at: now.clone(),
        });
    entry.last_state_change_at = Some(now.clone());
    entry.updated_at = now;
    write_state(&state);
}

#[allow(dead_code)]
pub fn is_context_fresh_and_clean(cwd: &str, max_age_seconds: u64) -> bool {
    if cwd.trim().is_empty() {
        return false;
    }
    let state = read_state();

    let entry = state.workspaces.get(cwd).or_else(|| {
        state.workspaces.iter().find_map(|(tracked_cwd, entry)| {
            workspace_paths_match(tracked_cwd, cwd).then_some(entry)
        })
    });

    let Some(entry) = entry else {
        return false;
    };

    let Some(last_context_at) = entry.last_context_at.as_deref() else {
        return false;
    };
    let Ok(last_context) = chrono::DateTime::parse_from_rfc3339(last_context_at) else {
        return false;
    };
    let age = chrono::Utc::now().signed_duration_since(last_context.with_timezone(&chrono::Utc));
    if age.num_seconds() < 0 || age.num_seconds() as u64 > max_age_seconds {
        return false;
    }

    if let Some(last_state_change_at) = entry.last_state_change_at.as_deref() {
        if let Ok(last_change) = chrono::DateTime::parse_from_rfc3339(last_state_change_at) {
            if last_change.with_timezone(&chrono::Utc) > last_context.with_timezone(&chrono::Utc) {
                return false;
            }
        }
    }

    true
}

pub fn is_context_required(cwd: &str) -> bool {
    if cwd.trim().is_empty() {
        return false;
    }
    let state = read_state();

    if let Some(entry) = state.workspaces.get(cwd) {
        return entry.require_context;
    }

    for (tracked_cwd, entry) in &state.workspaces {
        if workspace_paths_match(tracked_cwd, cwd) {
            return entry.require_context;
        }
    }

    false
}

pub fn start_index_wait_window(cwd: &str, wait_seconds: u64) {
    if cwd.trim().is_empty() || wait_seconds == 0 {
        return;
    }
    let mut state = read_state();
    let now = chrono::Utc::now();
    let now_iso = now.to_rfc3339();
    let wait_until_iso = (now + chrono::Duration::seconds(wait_seconds as i64)).to_rfc3339();
    let entry = state
        .workspaces
        .entry(cwd.to_string())
        .or_insert(PromptStateEntry {
            require_context: false,
            require_init: false,
            last_context_at: None,
            last_state_change_at: None,
            index_wait_started_at: None,
            index_wait_until: None,
            updated_at: now_iso.clone(),
        });

    if let Some(existing_until) = entry
        .index_wait_until
        .as_deref()
        .and_then(parse_rfc3339_utc)
    {
        if existing_until > now {
            entry.updated_at = now_iso;
            write_state(&state);
            return;
        }
    }

    entry.index_wait_started_at = Some(now_iso.clone());
    entry.index_wait_until = Some(wait_until_iso);
    entry.updated_at = now_iso;
    write_state(&state);
}

pub fn clear_index_wait_window(cwd: &str) {
    if cwd.trim().is_empty() {
        return;
    }
    let mut state = read_state();
    if let Some(entry) = entry_mut_for_cwd(&mut state.workspaces, cwd) {
        entry.index_wait_started_at = None;
        entry.index_wait_until = None;
        entry.updated_at = chrono::Utc::now().to_rfc3339();
        write_state(&state);
    }
}

pub fn index_wait_remaining_seconds(cwd: &str) -> Option<u64> {
    if cwd.trim().is_empty() {
        return None;
    }
    let state = read_state();
    let entry = state.workspaces.get(cwd).or_else(|| {
        state.workspaces.iter().find_map(|(tracked_cwd, entry)| {
            workspace_paths_match(tracked_cwd, cwd).then_some(entry)
        })
    })?;
    let until = parse_rfc3339_utc(entry.index_wait_until.as_deref()?)?;
    let now = chrono::Utc::now();
    let remaining = until.signed_duration_since(now).num_seconds();
    (remaining > 0).then_some(remaining as u64)
}

pub fn cleanup_stale(max_age_minutes: u64) {
    let mut state = read_state();
    let now = chrono::Utc::now();
    let original_count = state.workspaces.len();

    state.workspaces.retain(|_, entry| {
        if let Ok(updated) = chrono::DateTime::parse_from_rfc3339(&entry.updated_at) {
            let age = now.signed_duration_since(updated);
            age.num_minutes() < max_age_minutes as i64
        } else {
            false
        }
    });

    if state.workspaces.len() != original_count {
        let _ = write_state(&state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_init_isolated_for_parallel_sessions_in_same_directory() {
        let directory = tempfile::tempdir().expect("create isolated hook state");
        let path = directory.path().join("session-init-state.json");
        let a = session_init_key(&serde_json::json!({
            "session_id": "host-a", "cwd": "/same/project"
        }))
        .expect("host A identity");
        let b = session_init_key(&serde_json::json!({
            "session_id": "host-b", "cwd": "/same/project"
        }))
        .expect("host B identity");

        mark_session_init_required_at(&path, &a).expect("start A");
        assert!(is_session_init_required_at(&path, &a).expect("read fresh A"));
        clear_session_init_required_at(&path, &a).expect("initialize A");
        mark_session_init_required_at(&path, &b).expect("start B");
        assert!(!is_session_init_required_at(&path, &a).expect("A stays initialized"));
        assert!(is_session_init_required_at(&path, &b).expect("B still requires init"));
    }

    #[test]
    fn session_init_replayed_start_preserves_initialization_across_directories() {
        let directory = tempfile::tempdir().expect("create isolated hook state");
        let path = directory.path().join("session-init-state.json");
        let original = session_init_key(&serde_json::json!({
            "session_id": "same-host", "cwd": "/original/project"
        }))
        .expect("original identity");
        let resumed = session_init_key(&serde_json::json!({
            "session_id": "same-host", "cwd": "/other/project", "source": "resume"
        }))
        .expect("resumed identity");
        assert_eq!(original, resumed);
        mark_session_init_required_at(&path, &original).expect("first start");
        clear_session_init_required_at(&path, &original).expect("initialize");
        mark_session_init_required_at(&path, &resumed).expect("replayed start");
        assert!(!is_session_init_required_at(&path, &resumed).expect("read resumed session"));
    }

    #[test]
    fn session_init_success_before_start_is_preserved_and_unknown_session_is_open() {
        let directory = tempfile::tempdir().expect("create isolated hook state");
        let path = directory.path().join("session-init-state.json");
        let key = session_init_key(&serde_json::json!({ "session_id": "quick-start" }))
            .expect("host identity");
        assert!(!is_session_init_required_at(&path, &key).expect("unknown session"));
        clear_session_init_required_at(&path, &key).expect("context success without start");
        mark_session_init_required_at(&path, &key).expect("late session start");
        assert!(!is_session_init_required_at(&path, &key).expect("success remains recorded"));
    }

    #[test]
    fn session_init_identity_uses_trimmed_host_aliases_and_ignores_tool_arguments() {
        let snake = serde_json::json!({ "session_id": " host-session " });
        let camel = serde_json::json!({ "sessionId": "host-session" });
        assert_eq!(session_init_key(&snake), session_init_key(&camel));
        assert_eq!(
            session_init_key(&snake),
            session_init_key(&serde_json::json!({
                "session_id": "  ", "sessionId": "host-session"
            }))
        );
        let key = session_init_key(&snake).expect("hashed host identity");
        assert_eq!(key.len(), 64);
        assert!(!key.contains("host-session"));
        assert!(session_init_key(&serde_json::json!({
            "session_id": " ", "sessionId": "",
            "tool_input": { "session_id": "tool-session" }
        }))
        .is_none());
        assert_eq!(
            session_init_key(&snake),
            session_init_key(&serde_json::json!({
                "session_id": "host-session",
                "tool_input": { "session_id": "different-contextstream-session" }
            }))
        );
    }

    #[test]
    fn session_init_concurrent_writers_preserve_every_session() {
        let directory = tempfile::tempdir().expect("create isolated hook state");
        let path = directory.path().join("session-init-state.json");
        let workers: Vec<_> = (0..24)
            .map(|index| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let key = session_init_key(&serde_json::json!({
                        "session_id": format!("host-{index}")
                    }))
                    .expect("worker identity");
                    mark_session_init_required_at(&path, &key).expect("start worker");
                    assert!(is_session_init_required_at(&path, &key).expect("fresh worker"));
                    clear_session_init_required_at(&path, &key).expect("initialize worker");
                    mark_session_init_required_at(&path, &key).expect("replay worker start");
                    key
                })
            })
            .collect();
        let keys: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().expect("worker finished"))
            .collect();
        let state: SessionInitStateFile =
            serde_json::from_slice(&std::fs::read(&path).expect("read complete state"))
                .expect("atomic state remains valid JSON");
        assert_eq!(state.sessions.len(), keys.len());
        for key in keys {
            assert!(!state.sessions[&key].require_init);
        }
        assert!(path.with_extension("lock").exists());
        assert_eq!(
            std::fs::read_dir(directory.path())
                .expect("read isolated state directory")
                .count(),
            2,
            "atomic replacement should leave only state and the stable lock"
        );
    }

    #[test]
    fn session_init_retention_discards_only_expired_sessions() {
        let directory = tempfile::tempdir().expect("create isolated hook state");
        let path = directory.path().join("session-init-state.json");
        let mut state = SessionInitStateFile::default();
        state.sessions.insert(
            "expired".to_string(),
            SessionInitStateEntry {
                require_init: true,
                updated_at: (chrono::Utc::now() - chrono::Duration::days(8)).to_rfc3339(),
            },
        );
        state.sessions.insert(
            "recent".to_string(),
            SessionInitStateEntry {
                require_init: false,
                updated_at: chrono::Utc::now().to_rfc3339(),
            },
        );
        write_session_init_state(&path, &state).expect("write retention fixture");
        mark_session_init_required_at(&path, "fresh").expect("start fresh session");
        let saved: SessionInitStateFile =
            serde_json::from_slice(&std::fs::read(path).expect("read pruned state"))
                .expect("valid pruned state");
        assert!(!saved.sessions.contains_key("expired"));
        assert!(!saved.sessions["recent"].require_init);
        assert!(saved.sessions["fresh"].require_init);
    }

    #[test]
    fn prompt_state_roundtrip_shape_is_stable() {
        let mut state = PromptStateFile::default();
        state.workspaces.insert(
            "/tmp/project".to_string(),
            PromptStateEntry {
                require_context: true,
                require_init: false,
                last_context_at: Some(chrono::Utc::now().to_rfc3339()),
                last_state_change_at: None,
                index_wait_started_at: None,
                index_wait_until: None,
                updated_at: chrono::Utc::now().to_rfc3339(),
            },
        );
        let json = serde_json::to_string_pretty(&state).expect("serialize prompt state");
        let parsed: PromptStateFile = serde_json::from_str(&json).expect("parse prompt state");
        assert_eq!(parsed.workspaces.len(), 1);
        assert!(parsed.workspaces["/tmp/project"].require_context);
        assert!(!parsed.workspaces["/tmp/project"].require_init);
        assert!(parsed.workspaces["/tmp/project"].last_context_at.is_some());
        assert!(parsed.workspaces["/tmp/project"]
            .last_state_change_at
            .is_none());
        assert!(parsed.workspaces["/tmp/project"]
            .index_wait_started_at
            .is_none());
        assert!(parsed.workspaces["/tmp/project"].index_wait_until.is_none());
    }

    #[test]
    fn legacy_prompt_state_defaults_require_init_false() {
        let legacy = serde_json::json!({
            "workspaces": {
                "/tmp/project": {
                    "require_context": true,
                    "updated_at": chrono::Utc::now().to_rfc3339()
                }
            }
        });

        let parsed: PromptStateFile =
            serde_json::from_value(legacy).expect("parse legacy prompt state");
        assert!(parsed.workspaces["/tmp/project"].require_context);
        assert!(!parsed.workspaces["/tmp/project"].require_init);
        assert!(parsed.workspaces["/tmp/project"].last_context_at.is_none());
        assert!(parsed.workspaces["/tmp/project"]
            .last_state_change_at
            .is_none());
        assert!(parsed.workspaces["/tmp/project"]
            .index_wait_started_at
            .is_none());
        assert!(parsed.workspaces["/tmp/project"].index_wait_until.is_none());
    }

    #[test]
    fn workspace_paths_match_handles_parent_and_child_paths() {
        assert!(workspace_paths_match("/tmp/project", "/tmp/project"));
        assert!(workspace_paths_match(
            "/tmp/project",
            "/tmp/project/subdir/module"
        ));
        assert!(workspace_paths_match("/tmp/project/subdir", "/tmp/project"));
        assert!(!workspace_paths_match("/tmp/project-a", "/tmp/project-b"));
    }
}
