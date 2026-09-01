//! File-backed per-session model cache for hook invocations.
//!
//! Each MCP hook is a separate `contextstream-mcp hook <name>` process, so
//! the in-process telemetry hint store doesn't survive between calls. To avoid
//! losing the model id between, say, `UserPromptSubmit` (which sees `model` in
//! the payload) and a follow-up `PostToolUse` (which does not), we persist
//! `(session_id -> canonical_model_id)` to disk. The same cache lets the
//! `context()` tool size context-pressure thresholds to the active model's
//! window, since it shares the session id used by the hook layer.
//!
//! Storage: `$XDG_STATE_HOME/contextstream-mcp/session-models.json`, falling
//! back to `~/.local/state/contextstream-mcp/session-models.json` and finally
//! `~/.contextstream/session-models.json` when XDG is unavailable.
//!
//! Entries older than [`DEFAULT_TTL_SECONDS`] are skipped on read. The cache
//! is also pruned opportunistically on every write.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Default TTL: 24 hours. A session lasts as long as the editor keeps it
/// open, but we don't want stale entries to accumulate forever.
pub const DEFAULT_TTL_SECONDS: u64 = 60 * 60 * 24;

/// Hard cap on entries to prevent the file from growing unbounded if the
/// editor recycles session ids constantly.
const MAX_ENTRIES: usize = 256;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedModel {
    canonical_model_id: String,
    captured_at_secs: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CacheFile {
    #[serde(default)]
    sessions: HashMap<String, CachedModel>,
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Resolve the cache file path. Returns `None` if no writable location is
/// available — callers treat that as "best-effort, skip the cache".
fn cache_path() -> Option<PathBuf> {
    if let Some(override_path) = std::env::var_os("CONTEXTSTREAM_SESSION_MODEL_CACHE") {
        return Some(PathBuf::from(override_path));
    }

    let base = if let Some(xdg) = std::env::var_os("XDG_STATE_HOME") {
        PathBuf::from(xdg).join("contextstream-mcp")
    } else if let Some(home) = std::env::var_os("HOME") {
        PathBuf::from(home)
            .join(".local")
            .join("state")
            .join("contextstream-mcp")
    } else if let Some(profile) = std::env::var_os("USERPROFILE") {
        PathBuf::from(profile).join(".contextstream")
    } else {
        return None;
    };

    Some(base.join("session-models.json"))
}

fn read_file(path: &Path) -> CacheFile {
    fs::read_to_string(path)
        .ok()
        .and_then(|content| serde_json::from_str::<CacheFile>(&content).ok())
        .unwrap_or_default()
}

fn write_file(path: &Path, file: &CacheFile) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    {
        let mut f = fs::File::create(&tmp)?;
        let serialized = serde_json::to_vec_pretty(file).unwrap_or_else(|_| b"{}".to_vec());
        f.write_all(&serialized)?;
        f.sync_data().ok();
    }
    fs::rename(tmp, path)
}

fn prune(file: &mut CacheFile, ttl_secs: u64) {
    let cutoff = now_secs().saturating_sub(ttl_secs);
    file.sessions
        .retain(|_, entry| entry.captured_at_secs >= cutoff);

    if file.sessions.len() > MAX_ENTRIES {
        let mut entries: Vec<(String, CachedModel)> = file.sessions.drain().collect();
        entries.sort_by_key(|(_, m)| std::cmp::Reverse(m.captured_at_secs));
        entries.truncate(MAX_ENTRIES);
        file.sessions = entries.into_iter().collect();
    }
}

/// Record a `(session_id, canonical_model_id)` pair. Best-effort: I/O errors
/// are ignored so a read-only cache directory never breaks hook execution.
pub fn record(session_id: &str, canonical_model_id: &str) {
    if session_id.trim().is_empty() || canonical_model_id.trim().is_empty() {
        return;
    }
    let Some(path) = cache_path() else {
        return;
    };

    let mut file = read_file(&path);
    file.sessions.insert(
        session_id.trim().to_string(),
        CachedModel {
            canonical_model_id: canonical_model_id.trim().to_string(),
            captured_at_secs: now_secs(),
        },
    );
    prune(&mut file, DEFAULT_TTL_SECONDS);
    let _ = write_file(&path, &file);
}

/// Look up a previously recorded model id for `session_id`. Returns `None`
/// when the session has no entry, the entry has expired, or the cache file
/// is unreadable.
pub fn lookup(session_id: &str) -> Option<String> {
    lookup_with_ttl(session_id, DEFAULT_TTL_SECONDS)
}

pub fn lookup_with_ttl(session_id: &str, ttl_secs: u64) -> Option<String> {
    if session_id.trim().is_empty() {
        return None;
    }
    let path = cache_path()?;
    let file = read_file(&path);
    let entry = file.sessions.get(session_id.trim())?;
    let cutoff = now_secs().saturating_sub(ttl_secs);
    if entry.captured_at_secs < cutoff {
        return None;
    }
    Some(entry.canonical_model_id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_temp_cache<F: FnOnce()>(f: F) {
        let _g = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("session-models.json");
        std::env::set_var("CONTEXTSTREAM_SESSION_MODEL_CACHE", &path);
        f();
        std::env::remove_var("CONTEXTSTREAM_SESSION_MODEL_CACHE");
    }

    #[test]
    fn record_and_lookup_round_trip() {
        with_temp_cache(|| {
            assert_eq!(lookup("session-1"), None);
            record("session-1", "claude-opus-4.7-thinking-high");
            assert_eq!(
                lookup("session-1").as_deref(),
                Some("claude-opus-4.7-thinking-high")
            );
        });
    }

    #[test]
    fn ttl_expires_old_entries() {
        with_temp_cache(|| {
            record("session-1", "gpt-5");
            // TTL of 0 seconds: even a freshly-written entry is expired
            // (since cutoff = now - 0 == now, and captured_at < cutoff is
            // false only at the exact same second; we add 1s margin to be
            // safe in CI).
            std::thread::sleep(std::time::Duration::from_secs(1));
            assert_eq!(lookup_with_ttl("session-1", 0), None);
        });
    }

    #[test]
    fn missing_session_returns_none() {
        with_temp_cache(|| {
            record("alpha", "gpt-5");
            assert_eq!(lookup("beta"), None);
        });
    }

    #[test]
    fn empty_inputs_are_ignored() {
        with_temp_cache(|| {
            record("", "gpt-5");
            record("session-1", "");
            assert_eq!(lookup(""), None);
            assert_eq!(lookup("session-1"), None);
        });
    }

    #[test]
    fn record_overwrites_previous_entry() {
        with_temp_cache(|| {
            record("session-1", "gpt-5");
            record("session-1", "claude-sonnet-4.5");
            assert_eq!(lookup("session-1").as_deref(), Some("claude-sonnet-4.5"));
        });
    }
}
