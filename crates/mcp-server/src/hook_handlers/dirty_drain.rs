//! `dirty-files.json` drain — reconcile-on-session content freshness.
//!
//! The PostToolUse hook records every edited path to
//! `~/.contextstream/dirty-files.json` (see [`super::post_tool_use`]). In
//! **remote-gateway** mode nothing on the server consumes that ledger — the
//! only readers (the search drift check and the local stdio keep-warm daemon)
//! don't run on the hosted gateway, so the file just accumulates. See ticket
//! P6.
//!
//! This module gives the ledger a consumer that runs on the session lifecycle
//! hooks (`SessionStart` / `UserPromptSubmit`), on the PostToolUse tail, and —
//! via [`drain_now_sync`] — synchronously right before each ContextStream
//! search/context call. Every durable edit receipt is authoritative until its
//! exact opaque version is proven committed; folder-wide `indexed_at` is never
//! allowed to suppress a targeted obligation. The drain pushes content through
//! [`mcp_client::ContextStreamClient::ingest_files_from_hook`], dropping a
//! drained entry only once the server commits it (a 202 enqueue is retained for
//! a later drain).
//!
//! Boundary (P6): the bytes are pushed from the client. This is a reconcile
//! pass for edits made between explicit ingests, and a backstop for editors
//! that fire prompt hooks but not a per-write `PostToolUse` hook.
//!
//! Safety properties:
//! - **Best-effort.** Never returns an error and never blocks the hook beyond
//!   the caller-supplied deadline (callers wrap it in a timeout).
//! - **Bounded.** At most [`MAX_DRAIN_FILES_PER_TURN`] files per turn, one
//!   batched POST per project, size-capped per file.
//! - **Cooldown.** A per-machine timestamp file rate-limits drains to at most
//!   one per [`MIN_DRAIN_INTERVAL_SECS`], so calling it on every prompt is
//!   cheap.
//! - **Non-billed.** POSTs carry `x-contextstream-background: true` +
//!   `x-contextstream-ingest-origin: dirty_drain`, landing in the system lane
//!   (consistent with the watcher's `watch` origin and keep-warm's
//!   `keep_warm_daemon`).
//! - **Commit-searchable.** Small / in-turn batches additionally send
//!   `x-contextstream-ingest-wait: committed`, so the non-billed push runs the
//!   server's synchronous lane and a 2xx means the edit is searchable now.
//!   Large idle batches stay on the 202 fast path and are reconciled later.

use std::collections::{BTreeMap, HashSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use mcp_client::{ContextStreamClient, TargetedFileDecision};
use serde_json::Value;
use uuid::Uuid;

/// Hard cap on files re-ingested in a single drain so a large dirty set never
/// stalls a turn.
const MAX_DRAIN_FILES_PER_TURN: usize = 40;

/// Minimum spacing between drains (seconds). Lets every hook call the drain
/// while almost all calls return immediately.
const MIN_DRAIN_INTERVAL_SECS: i64 = 15;

/// Slack applied to the mtime-vs-indexed comparison, matching the search-side
/// `dirty_hints_indicating_drift` rule so the two stay consistent.
const DRIFT_SLACK_SECS: i64 = 2;

/// At or below this many files per root, drains take the synchronous commit
/// lane (the `x-contextstream-ingest-wait: committed` header) so the edits are
/// searchable the instant the drain returns. Larger sets stay on the 202 fast
/// path so a big sync index never blocks the turn.
const SYNC_COMMIT_MAX_FILES: usize = 8;

/// Origin tag forwarded as `x-contextstream-ingest-origin` so the backend
/// classifies these ingests into the system/drain lane (non-billed).
pub(crate) const DRAIN_ORIGIN: &str = "dirty_drain";

/// Watcher submissions share the durable dirty ledger but retain their origin
/// so the long-lived watcher can restore full-scan retry semantics after a
/// restart. Hook/drain submissions continue to use [`DRAIN_ORIGIN`].
#[cfg(test)]
const WATCH_ORIGIN: &str = "watch";

/// Retain exact local dirty-file receipts for this many hours. Any eviction
/// first persists a covering full-scan obligation; pending paths are protected
/// until their jobs are reconciled.
const DIRTY_FILE_RETENTION_HOURS: i64 = 12;

/// Bound ordinary dirty hints per root. Pending submission paths are preserved
/// above this limit because dropping them would sever a durable job receipt.
const MAX_DIRTY_FILES_PER_WORKSPACE: usize = 256;

/// Crossing the watcher's exact-delta budget requires a repository-wide scan.
/// Keep this aligned with `watch::WATCH_TARGETED_MAX`; the durable flag makes
/// that decision survive a crash before the debounce window fires.
const WATCH_FULL_SCAN_THRESHOLD: usize = MAX_DIRTY_FILES_PER_WORKSPACE;

/// A receipt that never reaches a terminal job state cannot pin a checkout
/// forever. Expiry removes only the job reference: the exact dirty versions
/// stay present and a full repair is required before they can be cleared.
const PENDING_SUBMISSION_RETENTION_HOURS: i64 = 24;
const PENDING_RESERVATION_RETENTION_MINUTES: i64 = 5;

fn dirty_file_version(value: &Value) -> Option<&str> {
    value
        .get("version")
        .and_then(Value::as_str)
        .or_else(|| value.as_str())
}

fn dirty_file_modified_at(value: &Value) -> Option<DateTime<Utc>> {
    value
        .get("modified_at")
        .and_then(Value::as_str)
        .or_else(|| value.as_str())
        .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

fn new_dirty_file_value(now: DateTime<Utc>) -> Value {
    serde_json::json!({
        "version": Uuid::new_v4().to_string(),
        "modified_at": now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
    })
}

/// Whether a drained batch should take the synchronous commit lane (searchable
/// on return) rather than the 202 fast path. In-turn drains force it; otherwise
/// only small batches commit synchronously so a big set never blocks the turn.
fn use_sync_commit(force_sync: bool, file_count: usize) -> bool {
    force_sync || file_count <= SYNC_COMMIT_MAX_FILES
}

/// Outcome of pushing one drained batch.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PushOutcome {
    /// Server committed synchronously (HTTP 200, no async job): the files are
    /// searchable now, so the ledger entries can be cleared.
    Committed,
    /// Server enqueued an async job (HTTP 202): the edit is still pending, so
    /// the ledger entries are retained for a later drain.
    Pending(Vec<Uuid>),
    /// Network/transport failure: retain and retry on the next drain.
    Failed,
}

/// Run the drain, bounded by `deadline`. Best-effort: swallows all errors and
/// never blocks the hook longer than `deadline`. Safe to call concurrently
/// with the hook's context fetch (e.g. inside `tokio::join!`).
pub async fn drain_best_effort(deadline: Duration) {
    let _ = tokio::time::timeout(deadline, run_drain()).await;
}

fn contextstream_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".contextstream"))
}

fn dirty_ledger_path() -> Option<PathBuf> {
    contextstream_dir().map(|dir| dir.join("dirty-files.json"))
}

fn cooldown_path() -> Option<PathBuf> {
    contextstream_dir().map(|d| d.join("dirty-drain.last"))
}

fn persist_dirty_ledger(path: &Path, ledger: &Value) -> bool {
    let Ok(serialized) = serde_json::to_string_pretty(ledger) else {
        return false;
    };
    let Some(parent) = path.parent() else {
        return false;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return false;
    }
    let temporary = parent.join(format!(
        ".dirty-files.{}.{}.tmp",
        std::process::id(),
        Uuid::new_v4()
    ));
    let mut file = match OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
    {
        Ok(file) => file,
        Err(_) => return false,
    };
    if file.write_all(serialized.as_bytes()).is_err() || file.sync_all().is_err() {
        let _ = std::fs::remove_file(&temporary);
        return false;
    }
    drop(file);
    if std::fs::rename(&temporary, path).is_ok() {
        // Best-effort directory fsync makes the rename durable across a machine
        // crash on platforms that permit opening directories as files.
        if let Ok(parent_file) = std::fs::File::open(parent) {
            let _ = parent_file.sync_all();
        }
        true
    } else {
        let _ = std::fs::remove_file(&temporary);
        false
    }
}

/// Serialize every dirty-ledger read/modify/write across MCP processes. The
/// operation receives the latest on-disk value and reports whether it mutated
/// it. A successful return means the mutation (if any) reached an atomic,
/// fsynced rename before the lock was released.
fn with_locked_dirty_ledger_at_path<T>(
    state_path: &Path,
    operation: impl FnOnce(&mut Value) -> (T, bool),
) -> Option<T> {
    let parent = state_path.parent()?;
    std::fs::create_dir_all(parent).ok()?;
    let lock_path = parent.join("dirty-files.lock");
    let lock_file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)
        .ok()?;
    fs2::FileExt::lock_exclusive(&lock_file).ok()?;

    let mut ledger = std::fs::read_to_string(state_path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .unwrap_or_else(|| serde_json::json!({ "workspaces": {} }));
    if ledger
        .get("workspaces")
        .and_then(Value::as_object)
        .is_none()
    {
        ledger["workspaces"] = serde_json::json!({});
    }

    let (output, mutated) = operation(&mut ledger);
    let persisted = !mutated || persist_dirty_ledger(state_path, &ledger);
    let _ = fs2::FileExt::unlock(&lock_file);
    persisted.then_some(output)
}

fn with_locked_dirty_ledger<T>(operation: impl FnOnce(&mut Value) -> (T, bool)) -> Option<T> {
    let state_path = dirty_ledger_path()?;
    with_locked_dirty_ledger_at_path(&state_path, operation)
}

fn read_dirty_ledger() -> Option<Value> {
    with_locked_dirty_ledger(|ledger| (ledger.clone(), false))
}

fn record_dirty_paths_in_ledger(
    ledger: &mut Value,
    root: &str,
    paths: &[String],
    now: DateTime<Utc>,
) -> BTreeMap<String, String> {
    if root.trim().is_empty() || paths.is_empty() {
        return BTreeMap::new();
    }
    let now_str = now.to_rfc3339_opts(chrono::SecondsFormat::Nanos, true);
    let Some(workspaces) = ledger.get_mut("workspaces").and_then(Value::as_object_mut) else {
        return BTreeMap::new();
    };
    let entry = workspaces
        .entry(root.to_string())
        .or_insert_with(|| serde_json::json!({ "updated_at": now_str, "files": {} }));
    if !entry.is_object() {
        *entry = serde_json::json!({ "updated_at": now_str, "files": {} });
    }
    let Some(entry) = entry.as_object_mut() else {
        return BTreeMap::new();
    };
    entry.insert("updated_at".to_string(), Value::String(now_str.clone()));
    entry.insert(
        "generation".to_string(),
        Value::String(Uuid::new_v4().to_string()),
    );
    if entry.get("files").and_then(Value::as_object).is_none() {
        entry.insert("files".to_string(), serde_json::json!({}));
    }

    let full_submission_pending = entry
        .get("pending_submissions")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|submissions| submissions.values())
        .any(|submission| submission.get("mode").and_then(Value::as_str) == Some("full"));
    let protected_paths = entry
        .get("pending_submissions")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|submissions| submissions.values())
        .filter_map(|submission| submission.get("path_versions"))
        .filter_map(Value::as_object)
        .flat_map(|versions| versions.keys().cloned())
        .collect::<HashSet<_>>();
    let (versions, requires_full_scan) = {
        let Some(files) = entry.get_mut("files").and_then(Value::as_object_mut) else {
            return BTreeMap::new();
        };
        for path in paths.iter().filter(|path| !path.trim().is_empty()) {
            // Version identity is deliberately independent from wall time. A
            // completion for write N must never clear write N+1 merely because
            // the filesystem clock has coarse resolution.
            files.insert(path.clone(), new_dirty_file_value(now));
        }

        let cutoff = now - ChronoDuration::hours(DIRTY_FILE_RETENTION_HOURS);
        let before_retention = files.len();
        files.retain(|path, version| {
            protected_paths.contains(path)
                || dirty_file_modified_at(version)
                    .map(|timestamp| timestamp >= cutoff)
                    .unwrap_or(false)
        });
        let retention_evicted = files.len() != before_retention;

        let mut ordinary = files
            .iter()
            .filter(|(path, _)| !protected_paths.contains(*path))
            .filter_map(|(path, version)| {
                dirty_file_modified_at(version).map(|timestamp| (path.clone(), timestamp))
            })
            .collect::<Vec<_>>();
        let capacity_evicted = ordinary.len() > MAX_DIRTY_FILES_PER_WORKSPACE;
        let requires_full_scan = full_submission_pending
            || ordinary.len() > WATCH_FULL_SCAN_THRESHOLD
            || retention_evicted
            || capacity_evicted;
        if ordinary.len() > MAX_DIRTY_FILES_PER_WORKSPACE {
            ordinary.sort_by_key(|(_, timestamp)| std::cmp::Reverse(*timestamp));
            let keep = ordinary
                .into_iter()
                .take(MAX_DIRTY_FILES_PER_WORKSPACE)
                .map(|(path, _)| path)
                .collect::<HashSet<_>>();
            files.retain(|path, _| protected_paths.contains(path) || keep.contains(path));
        }

        let versions = paths
            .iter()
            .filter_map(|path| {
                files
                    .get(path)
                    .and_then(dirty_file_version)
                    .map(|version| (path.clone(), version.to_string()))
            })
            .collect();
        (versions, requires_full_scan)
    };
    if requires_full_scan {
        entry.insert("force_full".to_string(), Value::Bool(true));
    }
    versions
}

/// Persist watcher/hook paths before any async submission can outlive the
/// process. The returned versions are immutable receipts for exactly these
/// edits and must be used when clearing or attaching job IDs.
pub(crate) fn record_dirty_paths(root: &str, paths: &[String]) -> BTreeMap<String, String> {
    with_locked_dirty_ledger(|ledger| {
        let versions = record_dirty_paths_in_ledger(ledger, root, paths, Utc::now());
        prune_all_empty_roots(ledger);
        // The root generation/full-scan obligation can change even when the
        // bounded path map immediately evicts the newly observed path. Always
        // persist a non-empty recording attempt; callers still get an empty
        // receipt and defer if the exact version could not be retained.
        (versions, !root.trim().is_empty() && !paths.is_empty())
    })
    .unwrap_or_default()
}

pub(crate) fn snapshot_dirty_path_versions(
    root: &str,
    paths: &[String],
) -> BTreeMap<String, String> {
    with_locked_dirty_ledger(|ledger| (path_versions_for_root(ledger, root, paths), false))
        .unwrap_or_default()
}

/// Snapshot every durable dirty version for one checkout. Full watcher scans
/// cover the complete repository, so their crash-safe receipt must include
/// paths that were recorded before an overflow, debounce restart, or process
/// crash—not only the bounded in-memory event set that happened to survive.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct WatchDirtySnapshot {
    pub(crate) path_versions: BTreeMap<String, String>,
    pub(crate) root_version: Option<String>,
    pub(crate) force_full: bool,
}

pub(crate) fn snapshot_watch_dirty(root: &str) -> WatchDirtySnapshot {
    with_locked_dirty_ledger(|ledger| {
        let mutated = ensure_watch_root_generation(ledger, root);
        (watch_dirty_snapshot_for_root(ledger, root), mutated)
    })
    .unwrap_or_default()
}

/// Start (or resume after restart) one repository-wide cursor epoch. The
/// generation is captured exactly once and reused across every capped page.
/// A concurrent edit changes `generation`, but cannot replace this epoch, so
/// the final page detects the mismatch and schedules a new pass from page 1.
pub(crate) fn begin_watch_full_scan(root: &str) -> WatchDirtySnapshot {
    with_locked_dirty_ledger(|ledger| {
        let _ = ensure_watch_root_entry(ledger, root);
        let _ = ensure_watch_root_generation(ledger, root);
        let current_generation = ledger
            .get("workspaces")
            .and_then(|workspaces| workspaces.get(root))
            .and_then(watch_root_version)
            .map(ToString::to_string);
        if let Some(entry) = ledger
            .get_mut("workspaces")
            .and_then(Value::as_object_mut)
            .and_then(|workspaces| workspaces.get_mut(root))
            .and_then(Value::as_object_mut)
        {
            if entry
                .get("full_scan_epoch_version")
                .and_then(Value::as_str)
                .is_none()
            {
                if let Some(generation) = current_generation.as_deref() {
                    entry.insert(
                        "full_scan_epoch_version".to_string(),
                        Value::String(generation.to_string()),
                    );
                }
            }
            entry.insert("force_full".to_string(), Value::Bool(true));
        }
        let mut snapshot = watch_dirty_snapshot_for_root(ledger, root);
        snapshot.root_version = ledger
            .get("workspaces")
            .and_then(|workspaces| workspaces.get(root))
            .and_then(|entry| entry.get("full_scan_epoch_version"))
            .and_then(Value::as_str)
            .map(ToString::to_string);
        (snapshot, true)
    })
    .unwrap_or_default()
}

/// Persist a root-only covering obligation. This is used when an exact path
/// cannot be retained (notify overflow, retention/cap eviction, or repair).
pub(crate) fn mark_watch_force_full(root: &str) -> bool {
    if root.trim().is_empty() {
        return false;
    }
    with_locked_dirty_ledger(|ledger| {
        let _ = ensure_watch_root_entry(ledger, root);
        let _ = ensure_watch_root_generation(ledger, root);
        set_watch_force_full(ledger, root, true);
        (true, true)
    })
    .unwrap_or(false)
}

/// Collapse legacy cwd-keyed fragments into the canonical checkout root. Any
/// ambiguity is repaired by a covering full scan; exact receipts and job IDs
/// are preserved so accepted work remains reconcilable.
fn migrate_dirty_root(from: &str, to: &str) -> bool {
    if from.trim().is_empty() || to.trim().is_empty() || from == to {
        return false;
    }
    with_locked_dirty_ledger(|ledger| {
        let Some(source) = ledger
            .get_mut("workspaces")
            .and_then(Value::as_object_mut)
            .and_then(|workspaces| workspaces.remove(from))
        else {
            return (false, false);
        };
        let _ = ensure_watch_root_entry(ledger, to);
        let Some(target) = ledger
            .get_mut("workspaces")
            .and_then(Value::as_object_mut)
            .and_then(|workspaces| workspaces.get_mut(to))
            .and_then(Value::as_object_mut)
        else {
            return (false, false);
        };

        if let Some(source_files) = source.get("files").and_then(Value::as_object) {
            if target.get("files").and_then(Value::as_object).is_none() {
                target.insert("files".to_string(), serde_json::json!({}));
            }
            if let Some(target_files) = target.get_mut("files").and_then(Value::as_object_mut) {
                for (path, receipt) in source_files {
                    target_files
                        .entry(path.clone())
                        .or_insert_with(|| receipt.clone());
                }
            }
        }
        if let Some(source_pending) = source.get("pending_submissions").and_then(Value::as_object) {
            if target
                .get("pending_submissions")
                .and_then(Value::as_object)
                .is_none()
            {
                target.insert("pending_submissions".to_string(), serde_json::json!({}));
            }
            if let Some(target_pending) = target
                .get_mut("pending_submissions")
                .and_then(Value::as_object_mut)
            {
                for (key, receipt) in source_pending {
                    let key = if target_pending.contains_key(key) {
                        Uuid::new_v4().to_string()
                    } else {
                        key.clone()
                    };
                    target_pending.insert(key, receipt.clone());
                }
            }
        }
        target.insert("force_full".to_string(), Value::Bool(true));
        target.insert(
            "generation".to_string(),
            Value::String(Uuid::new_v4().to_string()),
        );
        target.insert(
            "updated_at".to_string(),
            Value::String(Utc::now().to_rfc3339()),
        );
        target.remove("full_scan_epoch_version");
        (true, true)
    })
    .unwrap_or(false)
}

/// Whether a drain is due given the last-run timestamp. Pure for testing.
fn due_for_drain(last_run: Option<DateTime<Utc>>, now: DateTime<Utc>) -> bool {
    match last_run {
        Some(last) => now.signed_duration_since(last).num_seconds() >= MIN_DRAIN_INTERVAL_SECS,
        None => true,
    }
}

/// Read + check the cooldown, stamping it when due. Returns true if the caller
/// should proceed with a drain.
fn claim_drain_slot() -> bool {
    let Some(path) = cooldown_path() else {
        return false;
    };
    let last_run = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| DateTime::parse_from_rfc3339(s.trim()).ok())
        .map(|dt| dt.with_timezone(&Utc));
    if !due_for_drain(last_run, Utc::now()) {
        return false;
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&path, Utc::now().to_rfc3339());
    true
}

/// One dirty entry under consideration: an absolute path plus the time the
/// edit was recorded in `dirty-files.json` (effectively its mtime).
#[derive(Debug, Clone)]
struct DirtyEntry {
    abs_path: String,
    modified_at: Option<DateTime<Utc>>,
}

/// One durable async submission. Paths are paired with the exact dirty-ledger
/// timestamp that was submitted so a later completion can never clear a newer
/// edit of the same file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingSubmission {
    key: String,
    project_id: Uuid,
    workspace_id: Uuid,
    job_ids: Vec<Uuid>,
    path_versions: BTreeMap<String, String>,
    origin: String,
    mode: PendingSubmissionMode,
    root_version: Option<String>,
    checkout_guard: Option<String>,
    scan_complete: bool,
    jobs_completed: bool,
    reserved: bool,
    submitted_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PendingSubmissionMode {
    Targeted,
    Full,
}

impl PendingSubmissionMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Targeted => "targeted",
            Self::Full => "full",
        }
    }

    fn parse(value: Option<&str>) -> Self {
        match value {
            Some("full") => Self::Full,
            _ => Self::Targeted,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct WatchSubmissionRetry {
    pub(crate) paths: Vec<String>,
    pub(crate) mode: PendingSubmissionMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobTerminalState {
    Completed,
    Failed,
    Pending,
    Unknown,
}

fn pending_submissions_for_root(data: &Value, root: &str) -> Vec<PendingSubmission> {
    let Some(submissions) = data
        .get("workspaces")
        .and_then(|workspaces| workspaces.get(root))
        .and_then(|entry| entry.get("pending_submissions"))
        .and_then(Value::as_object)
    else {
        return Vec::new();
    };

    submissions
        .iter()
        .filter_map(|(key, value)| {
            let project_id = value
                .get("project_id")
                .and_then(Value::as_str)
                .and_then(|raw| Uuid::parse_str(raw).ok())?;
            let workspace_id = value
                .get("workspace_id")
                .and_then(Value::as_str)
                .and_then(|raw| Uuid::parse_str(raw).ok())?;
            let job_ids = value
                .get("job_ids")
                .and_then(Value::as_array)?
                .iter()
                .filter_map(Value::as_str)
                .filter_map(|raw| Uuid::parse_str(raw).ok())
                .collect::<Vec<_>>();
            let jobs_completed = value
                .get("jobs_completed")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let reserved = value
                .get("reserved")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if job_ids.is_empty() && !jobs_completed && !reserved {
                return None;
            }
            let mode = PendingSubmissionMode::parse(value.get("mode").and_then(Value::as_str));
            let path_versions = value
                .get("path_versions")
                .and_then(Value::as_object)?
                .iter()
                .filter_map(|(path, version)| {
                    version
                        .as_str()
                        .map(|version| (path.clone(), version.to_string()))
                })
                .collect::<BTreeMap<_, _>>();
            if path_versions.is_empty() && mode != PendingSubmissionMode::Full {
                return None;
            }
            Some(PendingSubmission {
                key: key.clone(),
                project_id,
                workspace_id,
                job_ids,
                path_versions,
                origin: value
                    .get("origin")
                    .and_then(Value::as_str)
                    .unwrap_or(DRAIN_ORIGIN)
                    .to_string(),
                mode,
                root_version: value
                    .get("root_version")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                checkout_guard: value
                    .get("checkout_guard")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
                scan_complete: value
                    .get("scan_complete")
                    .and_then(Value::as_bool)
                    .unwrap_or(mode == PendingSubmissionMode::Targeted),
                jobs_completed,
                reserved,
                submitted_at: value
                    .get("submitted_at")
                    .and_then(Value::as_str)
                    .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
                    .map(|timestamp| timestamp.with_timezone(&Utc)),
            })
        })
        .collect()
}

fn path_versions_for_root(data: &Value, root: &str, paths: &[String]) -> BTreeMap<String, String> {
    let Some(files) = data
        .get("workspaces")
        .and_then(|workspaces| workspaces.get(root))
        .and_then(|entry| entry.get("files"))
        .and_then(Value::as_object)
    else {
        return BTreeMap::new();
    };
    paths
        .iter()
        .filter_map(|path| {
            files
                .get(path)
                .and_then(dirty_file_version)
                .map(|version| (path.clone(), version.to_string()))
        })
        .collect()
}

fn all_path_versions_for_root(data: &Value, root: &str) -> BTreeMap<String, String> {
    data.get("workspaces")
        .and_then(|workspaces| workspaces.get(root))
        .and_then(|entry| entry.get("files"))
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|files| files.iter())
        .filter_map(|(path, version)| {
            dirty_file_version(version).map(|version| (path.clone(), version.to_string()))
        })
        .collect()
}

fn watch_dirty_snapshot_for_root(data: &Value, root: &str) -> WatchDirtySnapshot {
    let entry = data
        .get("workspaces")
        .and_then(|workspaces| workspaces.get(root));
    WatchDirtySnapshot {
        path_versions: all_path_versions_for_root(data, root),
        root_version: entry.and_then(watch_root_version).map(ToString::to_string),
        force_full: entry
            .and_then(|entry| entry.get("force_full"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

fn watch_root_version(entry: &Value) -> Option<&str> {
    entry
        .get("generation")
        .and_then(Value::as_str)
        .or_else(|| entry.get("updated_at").and_then(Value::as_str))
}

fn ensure_watch_root_entry(data: &mut Value, root: &str) -> bool {
    if root.trim().is_empty() {
        return false;
    }
    if data.get("workspaces").and_then(Value::as_object).is_none() {
        data["workspaces"] = serde_json::json!({});
    }
    let Some(workspaces) = data.get_mut("workspaces").and_then(Value::as_object_mut) else {
        return false;
    };
    if workspaces.get(root).and_then(Value::as_object).is_some() {
        return false;
    }
    workspaces.insert(
        root.to_string(),
        serde_json::json!({
            "updated_at": Utc::now().to_rfc3339(),
            "generation": Uuid::new_v4().to_string(),
            "force_full": true,
            "files": {},
            "pending_submissions": {},
        }),
    );
    true
}

/// Upgrade legacy dirty-ledger roots to an opaque generation before a watcher
/// snapshots them. Wall-clock timestamps remain useful for drift selection,
/// but an opaque generation cannot collide or move backwards when two edits
/// happen within the same clock tick.
fn ensure_watch_root_generation(data: &mut Value, root: &str) -> bool {
    let Some(entry) = data
        .get_mut("workspaces")
        .and_then(Value::as_object_mut)
        .and_then(|workspaces| workspaces.get_mut(root))
        .and_then(Value::as_object_mut)
    else {
        return false;
    };
    if entry.get("generation").and_then(Value::as_str).is_some() {
        return false;
    }
    entry.insert(
        "generation".to_string(),
        Value::String(Uuid::new_v4().to_string()),
    );
    true
}

#[cfg(test)]
fn add_pending_submission(
    data: &mut Value,
    root: &str,
    project_id: Uuid,
    workspace_id: Uuid,
    job_ids: &[Uuid],
    paths: &[String],
) -> bool {
    let path_versions = path_versions_for_root(data, root, paths);
    add_pending_submission_with_versions(
        data,
        root,
        project_id,
        workspace_id,
        job_ids,
        &path_versions,
        DRAIN_ORIGIN,
        PendingSubmissionMode::Targeted,
        None,
        None,
        true,
        false,
    )
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
fn add_pending_submission_with_versions(
    data: &mut Value,
    root: &str,
    project_id: Uuid,
    workspace_id: Uuid,
    job_ids: &[Uuid],
    path_versions: &BTreeMap<String, String>,
    origin: &str,
    mode: PendingSubmissionMode,
    root_version: Option<&str>,
    checkout_guard: Option<&str>,
    scan_complete: bool,
    jobs_completed: bool,
) -> bool {
    if job_ids.is_empty() && !jobs_completed {
        return false;
    }
    if path_versions.is_empty() && mode != PendingSubmissionMode::Full {
        return false;
    }
    if pending_submissions_for_root(data, root)
        .into_iter()
        .any(|submission| {
            submission.project_id == project_id && submission.workspace_id == workspace_id
        })
    {
        return false;
    }
    let Some(entry) = data
        .get_mut("workspaces")
        .and_then(Value::as_object_mut)
        .and_then(|workspaces| workspaces.get_mut(root))
        .and_then(Value::as_object_mut)
    else {
        return false;
    };
    if entry
        .get("pending_submissions")
        .and_then(Value::as_object)
        .is_none()
    {
        entry.insert("pending_submissions".to_string(), serde_json::json!({}));
    }
    if mode == PendingSubmissionMode::Full {
        entry.insert("force_full".to_string(), Value::Bool(true));
    }
    let Some(submissions) = entry
        .get_mut("pending_submissions")
        .and_then(Value::as_object_mut)
    else {
        return false;
    };
    let key = Uuid::new_v4().to_string();
    submissions.insert(
        key,
        serde_json::json!({
            "project_id": project_id,
            "workspace_id": workspace_id,
            "job_ids": job_ids,
            "path_versions": path_versions,
            "origin": origin,
            "mode": mode.as_str(),
            "root_version": root_version,
            "checkout_guard": checkout_guard,
            "scan_complete": scan_complete,
            "jobs_completed": jobs_completed,
            "reserved": false,
            "submitted_at": Utc::now().to_rfc3339(),
        }),
    );
    true
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingSubmissionReservation {
    key: String,
}

/// Atomically reserve the single in-flight slot for one checkout scope before
/// local bytes are read or a request is sent. A process crash leaves a bounded
/// reservation that protects its exact paths; expiry converts it into retry
/// work without clearing evidence.
#[allow(clippy::too_many_arguments)]
pub(crate) fn reserve_pending_submission(
    root: &str,
    project_id: Uuid,
    workspace_id: Uuid,
    path_versions: &BTreeMap<String, String>,
    origin: &str,
    mode: PendingSubmissionMode,
    root_version: Option<&str>,
    checkout_guard: Option<&str>,
    scan_complete: bool,
) -> Option<PendingSubmissionReservation> {
    if root.trim().is_empty() || (path_versions.is_empty() && mode != PendingSubmissionMode::Full) {
        return None;
    }
    with_locked_dirty_ledger(|ledger| {
        let _ = ensure_watch_root_entry(ledger, root);
        if pending_submissions_for_root(ledger, root)
            .into_iter()
            .any(|submission| {
                submission.project_id == project_id && submission.workspace_id == workspace_id
            })
        {
            return (None, false);
        }
        let Some(entry) = ledger
            .get_mut("workspaces")
            .and_then(Value::as_object_mut)
            .and_then(|workspaces| workspaces.get_mut(root))
            .and_then(Value::as_object_mut)
        else {
            return (None, false);
        };
        if entry
            .get("pending_submissions")
            .and_then(Value::as_object)
            .is_none()
        {
            entry.insert("pending_submissions".to_string(), serde_json::json!({}));
        }
        if mode == PendingSubmissionMode::Full {
            entry.insert("force_full".to_string(), Value::Bool(true));
        }
        let Some(submissions) = entry
            .get_mut("pending_submissions")
            .and_then(Value::as_object_mut)
        else {
            return (None, false);
        };
        let key = Uuid::new_v4().to_string();
        submissions.insert(
            key.clone(),
            serde_json::json!({
                "project_id": project_id,
                "workspace_id": workspace_id,
                "job_ids": [],
                "path_versions": path_versions,
                "origin": origin,
                "mode": mode.as_str(),
                "root_version": root_version,
                "checkout_guard": checkout_guard,
                "scan_complete": scan_complete,
                "jobs_completed": false,
                "reserved": true,
                "submitted_at": Utc::now().to_rfc3339(),
            }),
        );
        (Some(PendingSubmissionReservation { key }), true)
    })
    .flatten()
}

pub(crate) fn finalize_pending_submission(
    root: &str,
    reservation: &PendingSubmissionReservation,
    job_ids: &[Uuid],
    jobs_completed: bool,
    scan_complete: Option<bool>,
) -> bool {
    if job_ids.is_empty() && !jobs_completed {
        return false;
    }
    with_locked_dirty_ledger(|ledger| {
        let Some(receipt) = ledger
            .get_mut("workspaces")
            .and_then(Value::as_object_mut)
            .and_then(|workspaces| workspaces.get_mut(root))
            .and_then(|entry| entry.get_mut("pending_submissions"))
            .and_then(Value::as_object_mut)
            .and_then(|submissions| submissions.get_mut(&reservation.key))
            .and_then(Value::as_object_mut)
        else {
            return (false, false);
        };
        if !receipt
            .get("reserved")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            return (false, false);
        }
        receipt.insert("job_ids".to_string(), serde_json::json!(job_ids));
        receipt.insert("jobs_completed".to_string(), Value::Bool(jobs_completed));
        if let Some(scan_complete) = scan_complete {
            receipt.insert("scan_complete".to_string(), Value::Bool(scan_complete));
        }
        receipt.insert("reserved".to_string(), Value::Bool(false));
        receipt.insert(
            "submitted_at".to_string(),
            Value::String(Utc::now().to_rfc3339()),
        );
        (true, true)
    })
    .unwrap_or(false)
}

pub(crate) fn cancel_pending_submission(
    root: &str,
    reservation: &PendingSubmissionReservation,
) -> bool {
    with_locked_dirty_ledger(|ledger| {
        let submission = pending_submissions_for_root(ledger, root)
            .into_iter()
            .find(|submission| submission.key == reservation.key);
        let Some(submission) = submission else {
            return (false, false);
        };
        let removed = remove_pending_submission(ledger, root, &reservation.key);
        if submission.mode == PendingSubmissionMode::Full {
            set_watch_force_full(ledger, root, true);
            reset_full_scan_epoch(ledger, root);
        }
        if removed {
            prune_empty_root(ledger, root);
        }
        (removed, removed)
    })
    .unwrap_or(false)
}

fn remove_pending_submission(data: &mut Value, root: &str, key: &str) -> bool {
    data.get_mut("workspaces")
        .and_then(Value::as_object_mut)
        .and_then(|workspaces| workspaces.get_mut(root))
        .and_then(|entry| entry.get_mut("pending_submissions"))
        .and_then(Value::as_object_mut)
        .and_then(|submissions| submissions.remove(key))
        .is_some()
}

fn prune_empty_root(data: &mut Value, root: &str) {
    let should_prune = data
        .get("workspaces")
        .and_then(|workspaces| workspaces.get(root))
        .is_some_and(|entry| {
            entry
                .get("files")
                .and_then(Value::as_object)
                .is_none_or(serde_json::Map::is_empty)
                && entry
                    .get("pending_submissions")
                    .and_then(Value::as_object)
                    .is_none_or(serde_json::Map::is_empty)
                && !entry
                    .get("force_full")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                && entry.get("full_scan_epoch_version").is_none()
        });
    if should_prune {
        if let Some(workspaces) = data.get_mut("workspaces").and_then(Value::as_object_mut) {
            workspaces.remove(root);
        }
    }
}

fn prune_all_empty_roots(data: &mut Value) {
    let roots = data
        .get("workspaces")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|workspaces| workspaces.keys().cloned())
        .collect::<Vec<_>>();
    for root in roots {
        prune_empty_root(data, &root);
    }
}

fn remove_drained_path_versions(
    data: &mut Value,
    root: &str,
    submitted: &BTreeMap<String, String>,
) {
    let Some(files) = data
        .get_mut("workspaces")
        .and_then(Value::as_object_mut)
        .and_then(|workspaces| workspaces.get_mut(root))
        .and_then(|entry| entry.get_mut("files"))
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    files.retain(|path, current_version| {
        submitted.get(path).is_none_or(|submitted_version| {
            dirty_file_version(current_version) != Some(submitted_version)
        })
    });
}

fn set_watch_force_full(data: &mut Value, root: &str, force_full: bool) {
    if let Some(entry) = data
        .get_mut("workspaces")
        .and_then(Value::as_object_mut)
        .and_then(|workspaces| workspaces.get_mut(root))
        .and_then(Value::as_object_mut)
    {
        entry.insert("force_full".to_string(), Value::Bool(force_full));
    }
}

fn reset_full_scan_epoch(data: &mut Value, root: &str) {
    if let Some(entry) = data
        .get_mut("workspaces")
        .and_then(Value::as_object_mut)
        .and_then(|workspaces| workspaces.get_mut(root))
        .and_then(Value::as_object_mut)
    {
        entry.remove("full_scan_epoch_version");
    }
}

/// Apply the dirty-version side of a proven watcher commit. Returns whether a
/// further full scan is required. A complete full scan may clear the whole
/// bounded ledger only when no edit changed the opaque root generation while
/// the scan/request/attestation was in flight. Partial cursor pages and
/// generation mismatches retain a durable full-scan obligation.
fn apply_committed_watch_versions(
    data: &mut Value,
    root: &str,
    submitted: &BTreeMap<String, String>,
    mode: PendingSubmissionMode,
    root_version: Option<&str>,
    scan_complete: bool,
) -> bool {
    if mode == PendingSubmissionMode::Targeted {
        remove_drained_path_versions(data, root, submitted);
        return false;
    }

    if !scan_complete {
        set_watch_force_full(data, root, true);
        return true;
    }

    let current_root_version = data
        .get("workspaces")
        .and_then(|workspaces| workspaces.get(root))
        .and_then(watch_root_version);
    if root_version.is_some() && current_root_version == root_version {
        if let Some(entry) = data
            .get_mut("workspaces")
            .and_then(Value::as_object_mut)
            .and_then(|workspaces| workspaces.get_mut(root))
            .and_then(Value::as_object_mut)
        {
            entry.insert("files".to_string(), serde_json::json!({}));
            entry.insert("force_full".to_string(), Value::Bool(false));
            entry.remove("full_scan_epoch_version");
        }
        false
    } else {
        remove_drained_path_versions(data, root, submitted);
        set_watch_force_full(data, root, true);
        reset_full_scan_epoch(data, root);
        true
    }
}

fn pending_submission_is_current(data: &Value, root: &str, expected: &PendingSubmission) -> bool {
    pending_submissions_for_root(data, root)
        .into_iter()
        .any(|current| current == *expected)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingResolution {
    Completed,
    Retry,
}

/// Apply a terminal resolution only when the durable receipt still exactly
/// matches the snapshot that was polled. This makes concurrent watcher/drain
/// reconcilers idempotent and prevents a stale completion from clearing a
/// replacement submission or a newer path version.
fn apply_pending_resolution(
    data: &mut Value,
    root: &str,
    submission: &PendingSubmission,
    resolution: PendingResolution,
) -> (bool, bool) {
    if !pending_submission_is_current(data, root, submission) {
        return (false, false);
    }
    let retry_required = match resolution {
        PendingResolution::Completed => apply_committed_watch_versions(
            data,
            root,
            &submission.path_versions,
            submission.mode,
            submission.root_version.as_deref(),
            submission.scan_complete,
        ),
        PendingResolution::Retry => {
            if submission.mode == PendingSubmissionMode::Full {
                set_watch_force_full(data, root, true);
                reset_full_scan_epoch(data, root);
            }
            true
        }
    };
    let applied = remove_pending_submission(data, root, &submission.key);
    if applied {
        prune_empty_root(data, root);
    }
    (applied, applied && retry_required)
}

pub(crate) fn has_pending_submission_for_scope(
    root: &str,
    project_id: Uuid,
    workspace_id: Uuid,
) -> bool {
    read_dirty_ledger().is_some_and(|ledger| {
        pending_submissions_for_root(&ledger, root)
            .into_iter()
            .any(|submission| {
                submission.project_id == project_id && submission.workspace_id == workspace_id
            })
    })
}

fn ingest_job_terminal_state(progress: &Value) -> JobTerminalState {
    let status = progress
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .trim()
        .to_ascii_lowercase();
    let has_failure_count = progress
        .get("files_failed")
        .and_then(Value::as_i64)
        .is_some_and(|count| count > 0);
    let has_error = [
        "error",
        "errors",
        "error_message",
        "failed_paths",
        "file_errors",
    ]
    .into_iter()
    .any(|key| {
        progress.get(key).is_some_and(|value| match value {
            Value::Null => false,
            Value::Bool(value) => *value,
            Value::Number(value) => value.as_i64().unwrap_or(1) > 0,
            Value::String(value) => !value.trim().is_empty(),
            Value::Array(value) => !value.is_empty(),
            Value::Object(value) => !value.is_empty(),
        })
    });
    if has_failure_count || has_error {
        return JobTerminalState::Failed;
    }
    match status.as_str() {
        "completed" => JobTerminalState::Completed,
        "failed" | "canceled" | "cancelled" | "rejected" | "dead_letter" => {
            JobTerminalState::Failed
        }
        "pending" | "queued" | "claimed" | "processing" | "running" | "committing" => {
            JobTerminalState::Pending
        }
        _ => JobTerminalState::Unknown,
    }
}

/// Drift rule mirroring search-side `dirty_hints_indicating_drift`: an edit
/// drifts when its recorded time is newer than `indexed_at` (with slack), or
/// when the index time is unknown. Pure for testing.
fn file_drifted(modified_at: Option<DateTime<Utc>>, indexed_at: Option<DateTime<Utc>>) -> bool {
    match (modified_at, indexed_at) {
        (Some(m), Some(indexed)) => m > indexed + ChronoDuration::seconds(DRIFT_SLACK_SECS),
        // No known index time -> treat any tracked edit as drift.
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// Select which dirty entries to re-ingest: drifted + indexable, capped at
/// `budget`. Pure (existence/size are validated later at read time).
fn select_files_to_ingest(
    entries: &[DirtyEntry],
    indexed_at: Option<DateTime<Utc>>,
    indexable: impl Fn(&str) -> bool,
    budget: usize,
) -> Vec<String> {
    let mut selected = Vec::new();
    for entry in entries {
        if selected.len() >= budget {
            break;
        }
        if !indexable(&entry.abs_path) {
            continue;
        }
        if file_drifted(entry.modified_at, indexed_at) {
            selected.push(entry.abs_path.clone());
        }
    }
    selected
}

/// Parse the `{ workspaces: { root: { files: { abs_path: receipt } } } }` ledger
/// into per-root dirty entries. Pure for testing.
fn parse_dirty_ledger(data: &Value) -> Vec<(String, Vec<DirtyEntry>)> {
    let Some(workspaces) = data.get("workspaces").and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (root, entry) in workspaces {
        let Some(files) = entry.get("files").and_then(|v| v.as_object()) else {
            continue;
        };
        let entries: Vec<DirtyEntry> = files
            .iter()
            .map(|(abs_path, receipt)| DirtyEntry {
                abs_path: abs_path.clone(),
                modified_at: dirty_file_modified_at(receipt),
            })
            .collect();
        out.push((root.clone(), entries));
    }
    out
}

/// Exact delta prepared from dirty paths. `completed` contains only ledger
/// entries represented by an upload or safe deletion; rejected paths remain
/// dirty for a later attempt.
#[derive(Debug, Default)]
struct BuiltIngestBatch {
    files: Vec<Value>,
    deleted_paths: Vec<String>,
    completed: Vec<String>,
}

/// Build an exact delta using the shared canonical containment, ignore,
/// secret-name, size, UTF-8, and TOCTOU policy.
fn build_ingest_batch(selected: &[String], project_root: &str) -> BuiltIngestBatch {
    let mut batch = BuiltIngestBatch::default();
    for abs in selected {
        match ContextStreamClient::targeted_text_file_decision(project_root, abs) {
            TargetedFileDecision::Upload(payload) => {
                batch.files.push(payload);
                batch.completed.push(abs.clone());
            }
            TargetedFileDecision::Delete(relative) => {
                batch.deleted_paths.push(relative);
                batch.completed.push(abs.clone());
            }
            TargetedFileDecision::Reject if !Path::new(abs).exists() => {
                if let Some(relative) =
                    ContextStreamClient::safe_project_relative_path(project_root, abs, false)
                {
                    batch.deleted_paths.push(relative);
                    batch.completed.push(abs.clone());
                }
            }
            TargetedFileDecision::Reject => {}
        }
    }
    batch
}

#[derive(Debug, Default)]
struct ReconcileSummary {
    protected_paths: HashSet<String>,
    watch_retries: Vec<WatchSubmissionRetry>,
}

fn poll_error_is_terminal_unknown(error: &mcp_types::Error) -> bool {
    matches!(
        error,
        mcp_types::Error::Http {
            status: 404 | 410,
            ..
        }
    )
}

/// Poll durable accepted jobs for one checkout scope. Pending jobs and
/// transient poll failures protect their paths from resubmission. Failed,
/// canceled, missing, or unknown jobs remove only the durable receipt and leave
/// the exact dirty versions available for retry. Completed jobs clear those
/// versions only after a fresh primary attestation succeeds.
async fn reconcile_pending_submissions(
    client: &ContextStreamClient,
    project_id: Uuid,
    workspace_id: Uuid,
    ledger_root: &str,
    tracked_root: &str,
) -> ReconcileSummary {
    let submissions = read_dirty_ledger()
        .map(|ledger| pending_submissions_for_root(&ledger, ledger_root))
        .unwrap_or_default();
    let mut summary = ReconcileSummary::default();

    for submission in submissions {
        let checkout_guard =
            ContextStreamClient::checkout_guard_for_scope(tracked_root, project_id, workspace_id);
        let receipt_expired = submission.submitted_at.is_none_or(|submitted_at| {
            let age = Utc::now().signed_duration_since(submitted_at);
            if submission.reserved {
                age > ChronoDuration::minutes(PENDING_RESERVATION_RETENTION_MINUTES)
            } else {
                age > ChronoDuration::hours(PENDING_SUBMISSION_RETENTION_HOURS)
            }
        });
        let resolution =
            if submission.project_id != project_id || submission.workspace_id != workspace_id {
                // A checkout was rebound. Never poll a job from the old scope with
                // the new credentials; discard only the stale job reference and
                // leave all dirty paths available for a scoped retry.
                Some(PendingResolution::Retry)
            } else if receipt_expired
                || checkout_guard.as_ref().ok() != Some(&submission.checkout_guard)
            {
                // A receipt belongs to one physical checkout identity. Legacy
                // receipts without a guard are deliberately retried rather
                // than trusted after an upgrade.
                Some(PendingResolution::Retry)
            } else {
                let mut all_completed = submission.jobs_completed;
                let mut terminal_failure = false;
                let mut transient_poll_failure = false;
                if !submission.jobs_completed {
                    all_completed = true;
                    for job_id in &submission.job_ids {
                        match client.ingest_job_progress(project_id, *job_id).await {
                            Ok(progress) => match ingest_job_terminal_state(&progress) {
                                JobTerminalState::Completed => {}
                                JobTerminalState::Failed | JobTerminalState::Unknown => {
                                    terminal_failure = true;
                                }
                                JobTerminalState::Pending => all_completed = false,
                            },
                            Err(error) if poll_error_is_terminal_unknown(&error) => {
                                terminal_failure = true;
                            }
                            Err(_) => {
                                all_completed = false;
                                transient_poll_failure = true;
                            }
                        }
                    }
                }

                if terminal_failure {
                    Some(PendingResolution::Retry)
                } else if all_completed
                    && !transient_poll_failure
                    && client
                        .refresh_verified_index_attestation(tracked_root, project_id, workspace_id)
                        .await
                        .unwrap_or(false)
                {
                    Some(PendingResolution::Completed)
                } else {
                    None
                }
            };

        let Some(resolution) = resolution else {
            summary
                .protected_paths
                .extend(submission.path_versions.keys().cloned());
            continue;
        };
        let (applied, retry_required) = with_locked_dirty_ledger(|ledger| {
            let outcome = apply_pending_resolution(ledger, ledger_root, &submission, resolution);
            (outcome, outcome.0)
        })
        .unwrap_or((false, false));
        if applied && retry_required {
            summary.watch_retries.push(WatchSubmissionRetry {
                paths: submission.path_versions.keys().cloned().collect(),
                mode: submission.mode,
            });
        }
    }

    summary
}

pub(crate) async fn reconcile_watch_submissions(
    client: &ContextStreamClient,
    root: &str,
    project_id: Uuid,
    workspace_id: Uuid,
) -> Vec<WatchSubmissionRetry> {
    reconcile_pending_submissions(client, project_id, workspace_id, root, root)
        .await
        .watch_retries
}

async fn run_drain() {
    if !claim_drain_slot() {
        return;
    }
    drain_tracked_roots(None, false).await;
}

/// Cooldown-bypassed, root-scoped synchronous drain for the pre-search hook.
///
/// Flushes pending edits for tracked roots under `root` on the synchronous
/// commit lane so a search issued immediately afterwards sees them. Bounded by
/// `budget` and fails open (just returns) on timeout, so it never blocks the
/// tool it runs ahead of.
pub async fn drain_now_sync(root: &str, budget: Duration) {
    let root = root.trim();
    if root.is_empty() {
        return;
    }
    let filter = PathBuf::from(root);
    let _ = tokio::time::timeout(budget, drain_tracked_roots(Some(&filter), true)).await;
}

/// Drain pending edits for tracked roots, optionally limited to roots related
/// to `root_filter` (ancestor-or-descendant of it). `force_sync` forces the
/// synchronous commit lane for every batch regardless of size (used by the
/// in-turn pre-search drain); otherwise only small batches commit synchronously
/// and larger sets ride the 202 fast path.
///
/// Best-effort: per-root failures are skipped, and the ledger is only rewritten
/// when entries were actually committed.
async fn drain_tracked_roots(root_filter: Option<&Path>, force_sync: bool) {
    let Some(ledger) = read_dirty_ledger() else {
        return; // no writable home/ledger scope
    };

    let roots = parse_dirty_ledger(&ledger);
    if roots.is_empty() {
        return;
    }

    let mut budget = MAX_DRAIN_FILES_PER_TURN;

    for (root, _) in roots {
        if budget == 0 {
            break;
        }

        // Scope filter: only roots that overlap the requested filter (the edit
        // root may be the project root, or a subdirectory of it).
        if let Some(filter) = root_filter {
            let root_path = Path::new(&root);
            if !root_path.starts_with(filter) && !filter.starts_with(root_path) {
                continue;
            }
        }

        // Resolve credentials + project for this tracked root (env -> config
        // files), exactly as the PostToolUse ingest path does.
        let Some(cfg) = super::post_tool_use::find_project_config(&root, &root) else {
            continue;
        };
        if cfg.api_key.trim().is_empty() {
            continue;
        }
        // Require a checkout-bound project root. The shared exact-file decision
        // accepts only canonical project-relative paths and never falls back to
        // an absolute index key.
        let Some(tracked_root) = cfg.project_root.clone() else {
            continue;
        };
        let canonical_tracked_root = std::fs::canonicalize(&tracked_root)
            .unwrap_or_else(|_| PathBuf::from(&tracked_root))
            .to_string_lossy()
            .to_string();
        if root != canonical_tracked_root {
            let _ = migrate_dirty_root(&root, &canonical_tracked_root);
            continue;
        }

        if mcp_client::validate_ingest_root(
            Path::new(&tracked_root),
            &mcp_client::IngestRootOptions::from_env(),
        )
        .is_err()
        {
            continue;
        }

        // Re-resolve the binding before polling persisted jobs. A stale job
        // reference from an old checkout/project scope is discarded without
        // ever being queried under the new scope.
        let Some(fresh_cfg) = super::post_tool_use::find_project_config(&tracked_root, &root)
            .filter(|fresh| {
                fresh.project_id == cfg.project_id
                    && fresh.workspace_id == cfg.workspace_id
                    && fresh.project_root.as_deref() == Some(tracked_root.as_str())
            })
        else {
            continue;
        };
        let client = super::post_tool_use::build_hook_client(&fresh_cfg);
        let _ = reconcile_pending_submissions(
            &client,
            fresh_cfg.project_id,
            fresh_cfg.workspace_id,
            &root,
            &tracked_root,
        )
        .await;
        if has_pending_submission_for_scope(&root, fresh_cfg.project_id, fresh_cfg.workspace_id) {
            continue;
        }

        // Reload after reconciliation so concurrent watcher/hook writes and
        // terminal job resolutions are reflected in selection. Every later
        // mutation is an exact locked delta against the latest disk state.
        let Some(latest_ledger) = read_dirty_ledger() else {
            continue;
        };
        let entries = parse_dirty_ledger(&latest_ledger)
            .into_iter()
            .find_map(|(candidate, entries)| (candidate == root).then_some(entries))
            .unwrap_or_default();
        let protected_paths = pending_submissions_for_root(&latest_ledger, &root)
            .into_iter()
            .filter(|submission| {
                submission.project_id == fresh_cfg.project_id
                    && submission.workspace_id == fresh_cfg.workspace_id
            })
            .flat_map(|submission| submission.path_versions.into_keys())
            .collect::<HashSet<_>>();

        let selected = select_files_to_ingest(
            &entries,
            None,
            |path| !protected_paths.contains(path) && super::post_tool_use::should_index(path),
            budget,
        );
        if selected.is_empty() {
            continue;
        }

        // Capture immutable edit identities before reading a single byte. If a
        // newer edit lands during payload construction or upload, exact-version
        // clearing leaves that newer obligation intact.
        let captured_versions = snapshot_dirty_path_versions(&root, &selected);
        let captured_paths = selected
            .into_iter()
            .filter(|path| captured_versions.contains_key(path))
            .collect::<Vec<_>>();
        if captured_paths.is_empty() {
            continue;
        }
        let Ok(checkout_guard) = ContextStreamClient::checkout_guard_for_scope(
            &tracked_root,
            fresh_cfg.project_id,
            fresh_cfg.workspace_id,
        ) else {
            continue;
        };
        let batch = build_ingest_batch(&captured_paths, &tracked_root);
        if batch.files.is_empty() && batch.deleted_paths.is_empty() {
            continue;
        }

        // Re-resolve local binding after reading bytes. The client performs an
        // uncached API ownership check immediately before sending the prepared
        // payload.
        let Some(fresh_cfg) = super::post_tool_use::find_project_config(&tracked_root, &root)
            .filter(|fresh| {
                fresh.project_id == cfg.project_id
                    && fresh.workspace_id == cfg.workspace_id
                    && fresh.project_root.as_deref() == Some(tracked_root.as_str())
            })
        else {
            continue;
        };
        let reroot = super::post_tool_use::should_reroot_push(&tracked_root);
        let wait_committed = use_sync_commit(force_sync, batch.completed.len());
        let submitted_versions = batch
            .completed
            .iter()
            .filter_map(|path| {
                captured_versions
                    .get(path)
                    .map(|version| (path.clone(), version.clone()))
            })
            .collect::<BTreeMap<_, _>>();
        let Some(reservation) = reserve_pending_submission(
            &root,
            fresh_cfg.project_id,
            fresh_cfg.workspace_id,
            &submitted_versions,
            DRAIN_ORIGIN,
            PendingSubmissionMode::Targeted,
            None,
            checkout_guard.as_deref(),
            true,
        ) else {
            continue;
        };

        match post_ingest_batch(
            &fresh_cfg,
            batch.files,
            batch.deleted_paths,
            reroot,
            wait_committed,
        )
        .await
        {
            PushOutcome::Committed => {
                // Searchable now. Targeted writes update server attestation but
                // never claim folder-wide coverage/indexed_at.
                budget = budget.saturating_sub(batch.completed.len());
                if finalize_pending_submission(&root, &reservation, &[], true, Some(true)) {
                    let _ = reconcile_pending_submissions(
                        &client,
                        fresh_cfg.project_id,
                        fresh_cfg.workspace_id,
                        &root,
                        &tracked_root,
                    )
                    .await;
                }
            }
            PushOutcome::Pending(job_ids) => {
                // Accepted but not yet committed (202): persist the exact job
                // IDs + path versions so later drains poll rather than resend.
                // The files remain dirty until terminal success is attested.
                budget = budget.saturating_sub(batch.completed.len());
                let _ =
                    finalize_pending_submission(&root, &reservation, &job_ids, false, Some(true));
            }
            PushOutcome::Failed => {
                // Retain entries and let the next drain retry.
                let _ = cancel_pending_submission(&root, &reservation);
            }
        }
    }
}

/// Push a drained batch through the client on the non-billed system lane
/// (`background=true` + `origin=dirty_drain`), carrying validated checkout
/// provenance and per-file machine identity. When `wait_committed`, the server runs the
/// synchronous commit lane so a 2xx means the files are searchable now.
///
/// Returns [`PushOutcome::Committed`] only on a synchronous 200 (no async job),
/// [`PushOutcome::Pending`] on a 202 enqueue, and [`PushOutcome::Failed`] on a
/// transport error.
async fn post_ingest_batch(
    cfg: &super::post_tool_use::ProjectConfig,
    files: Vec<Value>,
    deleted_paths: Vec<String>,
    reroot: bool,
    wait_committed: bool,
) -> PushOutcome {
    let Some(root) = cfg.project_root.as_deref() else {
        return PushOutcome::Failed;
    };
    if mcp_client::validate_ingest_root(Path::new(root), &mcp_client::IngestRootOptions::from_env())
        .is_err()
    {
        return PushOutcome::Failed;
    }
    let Some(fresh_cfg) = super::post_tool_use::find_project_config(root, root) else {
        return PushOutcome::Failed;
    };
    if fresh_cfg.project_id != cfg.project_id
        || fresh_cfg.workspace_id != cfg.workspace_id
        || fresh_cfg.project_root != cfg.project_root
    {
        return PushOutcome::Failed;
    }
    let client = super::post_tool_use::build_hook_client(&fresh_cfg);
    match client
        .ingest_files_from_hook(
            cfg.project_id,
            cfg.workspace_id,
            files,
            deleted_paths,
            true,
            Some(DRAIN_ORIGIN),
            cfg.project_root.as_deref(),
            reroot,
            wait_committed,
        )
        .await
    {
        Ok(outcome) if outcome.committed => PushOutcome::Committed,
        Ok(outcome) if !outcome.job_ids.is_empty() => PushOutcome::Pending(outcome.job_ids),
        Ok(_) => PushOutcome::Failed,
        Err(_) => PushOutcome::Failed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ts(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn drift_requires_mtime_after_indexed_plus_slack() {
        let indexed = Some(ts("2026-06-09T00:00:00Z"));
        // 1s after index — within slack, not drift.
        assert!(!file_drifted(Some(ts("2026-06-09T00:00:01Z")), indexed));
        // 5s after index — drift.
        assert!(file_drifted(Some(ts("2026-06-09T00:00:05Z")), indexed));
        // Before index — not drift.
        assert!(!file_drifted(Some(ts("2026-06-08T23:00:00Z")), indexed));
        // Unknown index time — any tracked edit drifts.
        assert!(file_drifted(Some(ts("2026-06-09T00:00:00Z")), None));
        // Unknown mtime — never drift.
        assert!(!file_drifted(None, indexed));
        assert!(!file_drifted(None, None));
    }

    #[test]
    fn select_caps_at_budget_and_filters_non_indexable() {
        let indexed = Some(ts("2026-06-09T00:00:00Z"));
        let entries = vec![
            DirtyEntry {
                abs_path: "/p/a.rs".into(),
                modified_at: Some(ts("2026-06-09T01:00:00Z")),
            },
            DirtyEntry {
                abs_path: "/p/b.bin".into(), // not indexable
                modified_at: Some(ts("2026-06-09T01:00:00Z")),
            },
            DirtyEntry {
                abs_path: "/p/c.ts".into(),
                modified_at: Some(ts("2026-06-09T01:00:00Z")),
            },
            DirtyEntry {
                abs_path: "/p/d.py".into(),
                modified_at: Some(ts("2026-06-09T01:00:00Z")),
            },
        ];
        let indexable = |p: &str| p.ends_with(".rs") || p.ends_with(".ts") || p.ends_with(".py");

        // Budget caps the result.
        let two = select_files_to_ingest(&entries, indexed, indexable, 2);
        assert_eq!(two, vec!["/p/a.rs".to_string(), "/p/c.ts".to_string()]);

        // Non-indexable .bin is always filtered out.
        let all = select_files_to_ingest(&entries, indexed, indexable, 10);
        assert_eq!(
            all,
            vec![
                "/p/a.rs".to_string(),
                "/p/c.ts".to_string(),
                "/p/d.py".to_string()
            ]
        );
    }

    #[test]
    fn select_drops_unchanged_files() {
        let indexed = Some(ts("2026-06-09T02:00:00Z"));
        let entries = vec![
            DirtyEntry {
                abs_path: "/p/old.rs".into(),
                modified_at: Some(ts("2026-06-09T01:00:00Z")), // before index
            },
            DirtyEntry {
                abs_path: "/p/new.rs".into(),
                modified_at: Some(ts("2026-06-09T03:00:00Z")), // after index
            },
        ];
        let selected = select_files_to_ingest(&entries, indexed, |_| true, 10);
        assert_eq!(selected, vec!["/p/new.rs".to_string()]);
    }

    #[test]
    fn sync_commit_policy_forces_small_and_in_turn_batches() {
        // In-turn (pre-search) drains always commit synchronously.
        assert!(use_sync_commit(true, 1));
        assert!(use_sync_commit(true, SYNC_COMMIT_MAX_FILES + 50));
        // Idle drains commit synchronously only for small batches; large sets
        // ride the 202 fast path so they never block the turn.
        assert!(use_sync_commit(false, 1));
        assert!(use_sync_commit(false, SYNC_COMMIT_MAX_FILES));
        assert!(!use_sync_commit(false, SYNC_COMMIT_MAX_FILES + 1));
    }

    #[test]
    fn cooldown_blocks_recent_runs() {
        let now = ts("2026-06-09T00:00:30Z");
        // 10s ago — too soon.
        assert!(!due_for_drain(Some(ts("2026-06-09T00:00:20Z")), now));
        // 20s ago — due.
        assert!(due_for_drain(Some(ts("2026-06-09T00:00:10Z")), now));
        // Never run — due.
        assert!(due_for_drain(None, now));
    }

    #[test]
    fn parse_ledger_extracts_entries_per_root() {
        let data = json!({
            "workspaces": {
                "/home/u/proj": {
                    "updated_at": "2026-06-09T00:00:00Z",
                    "files": {
                        "/home/u/proj/a.rs": "2026-06-09T00:00:01Z",
                        "/home/u/proj/b.rs": "2026-06-09T00:00:02Z"
                    }
                }
            }
        });
        let roots = parse_dirty_ledger(&data);
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].0, "/home/u/proj");
        assert_eq!(roots[0].1.len(), 2);
    }

    #[test]
    fn remove_drained_versions_clears_only_the_submitted_edit() {
        let mut data = json!({
            "workspaces": {
                "/home/u/proj": {
                    "files": {
                        "/home/u/proj/a.rs": "2026-06-09T00:00:01Z",
                        "/home/u/proj/b.rs": "2026-06-09T00:00:02Z"
                    }
                }
            }
        });
        remove_drained_path_versions(
            &mut data,
            "/home/u/proj",
            &BTreeMap::from([(
                "/home/u/proj/a.rs".to_string(),
                "2026-06-09T00:00:01Z".to_string(),
            )]),
        );
        let files = data["workspaces"]["/home/u/proj"]["files"]
            .as_object()
            .unwrap();
        assert!(!files.contains_key("/home/u/proj/a.rs"));
        assert!(files.contains_key("/home/u/proj/b.rs"));
    }

    #[test]
    fn completed_old_submission_never_clears_a_newer_edit() {
        let mut data = json!({
            "workspaces": {
                "/home/u/proj": {
                    "files": {
                        "/home/u/proj/a.rs": "2026-06-09T00:00:05Z"
                    }
                }
            }
        });
        remove_drained_path_versions(
            &mut data,
            "/home/u/proj",
            &BTreeMap::from([(
                "/home/u/proj/a.rs".to_string(),
                "2026-06-09T00:00:01Z".to_string(),
            )]),
        );
        assert_eq!(
            data["workspaces"]["/home/u/proj"]["files"]["/home/u/proj/a.rs"],
            "2026-06-09T00:00:05Z"
        );
    }

    #[test]
    fn pending_submission_round_trips_across_restart() {
        let project_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let job_id = Uuid::new_v4();
        let mut data = json!({
            "workspaces": {
                "/home/u/proj": {
                    "files": {
                        "/home/u/proj/a.rs": "2026-06-09T00:00:01Z"
                    }
                }
            }
        });
        assert!(add_pending_submission(
            &mut data,
            "/home/u/proj",
            project_id,
            workspace_id,
            &[job_id],
            &["/home/u/proj/a.rs".to_string()],
        ));

        let serialized = serde_json::to_string(&data).unwrap();
        let restored: Value = serde_json::from_str(&serialized).unwrap();
        let pending = pending_submissions_for_root(&restored, "/home/u/proj");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].project_id, project_id);
        assert_eq!(pending[0].workspace_id, workspace_id);
        assert_eq!(pending[0].job_ids, vec![job_id]);
        assert_eq!(pending[0].origin, DRAIN_ORIGIN);
        assert_eq!(pending[0].mode, PendingSubmissionMode::Targeted);
        assert!(!pending[0].jobs_completed);
        assert_eq!(
            pending[0].path_versions.get("/home/u/proj/a.rs"),
            Some(&"2026-06-09T00:00:01Z".to_string())
        );
    }

    #[test]
    fn full_watch_receipt_survives_disk_restart() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("dirty-files.json");
        let root = "/home/u/proj";
        let path = "/home/u/proj/a.rs".to_string();
        let project_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let job_id = Uuid::new_v4();

        let versions = with_locked_dirty_ledger_at_path(&state_path, |ledger| {
            let versions = record_dirty_paths_in_ledger(
                ledger,
                root,
                std::slice::from_ref(&path),
                ts("2026-06-09T00:00:01Z"),
            );
            (versions, true)
        })
        .unwrap();
        assert!(versions
            .get(&path)
            .is_some_and(|version| Uuid::parse_str(version).is_ok()));

        assert!(with_locked_dirty_ledger_at_path(&state_path, |ledger| {
            let root_version = watch_dirty_snapshot_for_root(ledger, root).root_version;
            let added = add_pending_submission_with_versions(
                ledger,
                root,
                project_id,
                workspace_id,
                &[job_id],
                &versions,
                WATCH_ORIGIN,
                PendingSubmissionMode::Full,
                root_version.as_deref(),
                None,
                true,
                false,
            );
            (added, added)
        })
        .unwrap());

        // A new process sees the durable origin, retry mode, exact versions,
        // and job IDs without needing any in-memory watcher state.
        let restored: Value =
            serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
        let pending = pending_submissions_for_root(&restored, root);
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].project_id, project_id);
        assert_eq!(pending[0].workspace_id, workspace_id);
        assert_eq!(pending[0].job_ids, vec![job_id]);
        assert_eq!(pending[0].path_versions, versions);
        assert_eq!(pending[0].origin, WATCH_ORIGIN);
        assert_eq!(pending[0].mode, PendingSubmissionMode::Full);
        assert!(pending[0].root_version.is_some());
        assert!(pending[0].scan_complete);
        assert!(!pending[0].jobs_completed);
    }

    #[test]
    fn dirty_root_generation_and_full_obligation_survive_pre_debounce_restart() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("dirty-files.json");
        let root = "/home/u/large";
        let paths = (0..=WATCH_FULL_SCAN_THRESHOLD)
            .map(|index| format!("{root}/src/file-{index}.rs"))
            .collect::<Vec<_>>();

        let first = with_locked_dirty_ledger_at_path(&state_path, |ledger| {
            let versions =
                record_dirty_paths_in_ledger(ledger, root, &paths, ts("2026-06-09T00:00:01Z"));
            let snapshot = watch_dirty_snapshot_for_root(ledger, root);
            ((versions, snapshot), true)
        })
        .unwrap();
        assert_eq!(first.0.len(), MAX_DIRTY_FILES_PER_WORKSPACE);
        assert!(first.1.force_full);
        assert!(first.1.root_version.is_some());

        let restored: Value =
            serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
        let restored = watch_dirty_snapshot_for_root(&restored, root);
        assert_eq!(restored, first.1);
    }

    #[test]
    fn same_timestamp_edits_receive_distinct_opaque_versions() {
        let root = "/home/u/proj";
        let path = format!("{root}/a.rs");
        let now = ts("2026-06-09T00:00:01Z");
        let mut data = json!({"workspaces": {}});
        let first = record_dirty_paths_in_ledger(&mut data, root, std::slice::from_ref(&path), now);
        let second =
            record_dirty_paths_in_ledger(&mut data, root, std::slice::from_ref(&path), now);
        assert_ne!(first.get(&path), second.get(&path));
        assert!(second
            .get(&path)
            .is_some_and(|version| Uuid::parse_str(version).is_ok()));
    }

    #[test]
    fn retention_eviction_persists_covering_full_scan_obligation() {
        let root = "/home/u/proj";
        let old_path = format!("{root}/old.rs");
        let new_path = format!("{root}/new.rs");
        let mut data = json!({
            "workspaces": {
                root: {
                    "files": {
                        (old_path.clone()): "2026-06-08T00:00:00Z"
                    }
                }
            }
        });
        let versions = record_dirty_paths_in_ledger(
            &mut data,
            root,
            std::slice::from_ref(&new_path),
            ts("2026-06-09T00:00:01Z"),
        );
        assert!(versions.contains_key(&new_path));
        assert!(data["workspaces"][root]["files"].get(&old_path).is_none());
        assert_eq!(data["workspaces"][root]["force_full"], true);
    }

    #[test]
    fn full_scan_epoch_is_reused_across_pages_and_reset_on_generation_mismatch() {
        let root = "/home/u/proj";
        let first_path = format!("{root}/a.rs");
        let second_path = format!("{root}/z.rs");
        let mut data = json!({"workspaces": {}});
        let submitted = record_dirty_paths_in_ledger(
            &mut data,
            root,
            std::slice::from_ref(&first_path),
            ts("2026-06-09T00:00:01Z"),
        );
        let _ = ensure_watch_root_generation(&mut data, root);
        let current = watch_dirty_snapshot_for_root(&data, root)
            .root_version
            .unwrap();
        data["workspaces"][root]["full_scan_epoch_version"] = json!(current);
        let epoch = data["workspaces"][root]["full_scan_epoch_version"]
            .as_str()
            .unwrap()
            .to_string();

        let _ = record_dirty_paths_in_ledger(
            &mut data,
            root,
            std::slice::from_ref(&second_path),
            ts("2026-06-09T00:00:02Z"),
        );
        assert_eq!(
            data["workspaces"][root]["full_scan_epoch_version"],
            json!(epoch.clone())
        );
        assert!(apply_committed_watch_versions(
            &mut data,
            root,
            &submitted,
            PendingSubmissionMode::Full,
            Some(&epoch),
            true,
        ));
        assert!(data["workspaces"][root]
            .get("full_scan_epoch_version")
            .is_none());
        assert_eq!(data["workspaces"][root]["force_full"], true);
        assert!(data["workspaces"][root]["files"]
            .get(&second_path)
            .is_some());
    }

    #[test]
    fn root_only_full_receipt_and_single_scope_reservation_survive_restart() {
        let root = "/home/u/proj";
        let project_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let mut data = json!({"workspaces": {}});
        assert!(ensure_watch_root_entry(&mut data, root));
        let root_version = watch_dirty_snapshot_for_root(&data, root).root_version;
        assert!(add_pending_submission_with_versions(
            &mut data,
            root,
            project_id,
            workspace_id,
            &[Uuid::new_v4()],
            &BTreeMap::new(),
            WATCH_ORIGIN,
            PendingSubmissionMode::Full,
            root_version.as_deref(),
            None,
            true,
            false,
        ));
        assert_eq!(pending_submissions_for_root(&data, root).len(), 1);
        assert!(!add_pending_submission_with_versions(
            &mut data,
            root,
            project_id,
            workspace_id,
            &[Uuid::new_v4()],
            &BTreeMap::new(),
            WATCH_ORIGIN,
            PendingSubmissionMode::Full,
            root_version.as_deref(),
            None,
            true,
            false,
        ));
        let restored: Value = serde_json::from_str(&serde_json::to_string(&data).unwrap()).unwrap();
        assert!(pending_submissions_for_root(&restored, root)[0]
            .path_versions
            .is_empty());
    }

    #[test]
    fn full_completion_clears_every_dirty_path_only_for_unchanged_root_generation() {
        let root = "/home/u/proj";
        let project_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let job_id = Uuid::new_v4();
        let submitted =
            BTreeMap::from([(format!("{root}/a.rs"), "2026-06-09T00:00:01Z".to_string())]);
        let mut data = json!({
            "workspaces": {
                "/home/u/proj": {
                    "generation": "generation-1",
                    "force_full": true,
                    "files": {
                        "/home/u/proj/a.rs": "2026-06-09T00:00:01Z",
                        "/home/u/proj/b.rs": "2026-06-09T00:00:01Z"
                    }
                }
            }
        });
        assert!(add_pending_submission_with_versions(
            &mut data,
            root,
            project_id,
            workspace_id,
            &[job_id],
            &submitted,
            WATCH_ORIGIN,
            PendingSubmissionMode::Full,
            Some("generation-1"),
            None,
            true,
            false,
        ));
        let receipt = pending_submissions_for_root(&data, root)
            .into_iter()
            .next()
            .unwrap();

        assert_eq!(
            apply_pending_resolution(&mut data, root, &receipt, PendingResolution::Completed,),
            (true, false)
        );
        assert!(data["workspaces"].get(root).is_none());
    }

    #[test]
    fn full_completion_preserves_new_edit_and_retries_when_generation_changed() {
        let root = "/home/u/proj";
        let old_path = format!("{root}/a.rs");
        let new_path = format!("{root}/new.rs");
        let project_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let job_id = Uuid::new_v4();
        let submitted = BTreeMap::from([(old_path.clone(), "2026-06-09T00:00:01Z".to_string())]);
        let mut data = json!({
            "workspaces": {
                "/home/u/proj": {
                    "generation": "generation-1",
                    "files": { "/home/u/proj/a.rs": "2026-06-09T00:00:01Z" }
                }
            }
        });
        assert!(add_pending_submission_with_versions(
            &mut data,
            root,
            project_id,
            workspace_id,
            &[job_id],
            &submitted,
            WATCH_ORIGIN,
            PendingSubmissionMode::Full,
            Some("generation-1"),
            None,
            true,
            false,
        ));
        let receipt = pending_submissions_for_root(&data, root)
            .into_iter()
            .next()
            .unwrap();
        data["workspaces"][root]["generation"] = json!("generation-2");
        data["workspaces"][root]["files"][new_path.clone()] = json!("2026-06-09T00:00:02Z");

        assert_eq!(
            apply_pending_resolution(&mut data, root, &receipt, PendingResolution::Completed,),
            (true, true)
        );
        let files = data["workspaces"][root]["files"].as_object().unwrap();
        assert!(!files.contains_key(&old_path));
        assert!(files.contains_key(&new_path));
        assert_eq!(data["workspaces"][root]["force_full"], true);
    }

    #[test]
    fn partial_full_completion_keeps_evidence_and_requires_next_cursor_page() {
        let root = "/home/u/proj";
        let path = format!("{root}/a.rs");
        let project_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let job_id = Uuid::new_v4();
        let submitted = BTreeMap::from([(path.clone(), "2026-06-09T00:00:01Z".to_string())]);
        let mut data = json!({
            "workspaces": {
                "/home/u/proj": {
                    "generation": "generation-1",
                    "files": { "/home/u/proj/a.rs": "2026-06-09T00:00:01Z" }
                }
            }
        });
        assert!(add_pending_submission_with_versions(
            &mut data,
            root,
            project_id,
            workspace_id,
            &[job_id],
            &submitted,
            WATCH_ORIGIN,
            PendingSubmissionMode::Full,
            Some("generation-1"),
            None,
            false,
            false,
        ));
        let receipt = pending_submissions_for_root(&data, root)
            .into_iter()
            .next()
            .unwrap();

        assert_eq!(
            apply_pending_resolution(&mut data, root, &receipt, PendingResolution::Completed,),
            (true, true)
        );
        assert_eq!(
            data["workspaces"][root]["files"][path],
            "2026-06-09T00:00:01Z"
        );
        assert_eq!(data["workspaces"][root]["force_full"], true);
    }

    #[test]
    fn completed_watch_receipt_preserves_newer_interleaved_edit() {
        let root = "/home/u/proj";
        let path = "/home/u/proj/a.rs";
        let project_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let job_id = Uuid::new_v4();
        let submitted_version = "2026-06-09T00:00:01Z";
        let newer_version = "2026-06-09T00:00:05Z";
        let submitted = BTreeMap::from([(path.to_string(), submitted_version.to_string())]);
        let mut data = json!({
            "workspaces": {
                root: {
                    "files": { path: submitted_version }
                }
            }
        });
        assert!(add_pending_submission_with_versions(
            &mut data,
            root,
            project_id,
            workspace_id,
            &[job_id],
            &submitted,
            WATCH_ORIGIN,
            PendingSubmissionMode::Targeted,
            None,
            None,
            true,
            false,
        ));
        let receipt = pending_submissions_for_root(&data, root)
            .into_iter()
            .next()
            .unwrap();

        // The same path changes again while the original job is in flight.
        data["workspaces"][root]["files"][path] = Value::String(newer_version.to_string());
        assert_eq!(
            apply_pending_resolution(&mut data, root, &receipt, PendingResolution::Completed,),
            (true, false)
        );

        assert_eq!(data["workspaces"][root]["files"][path], newer_version);
        assert!(pending_submissions_for_root(&data, root).is_empty());
    }

    #[test]
    fn failed_watch_receipt_is_removed_but_exact_dirty_version_remains_for_retry() {
        let root = "/home/u/proj";
        let path = "/home/u/proj/a.rs";
        let project_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let job_id = Uuid::new_v4();
        let version = "2026-06-09T00:00:01Z";
        let submitted = BTreeMap::from([(path.to_string(), version.to_string())]);
        let mut data = json!({
            "workspaces": {
                root: {
                    "files": { path: version }
                }
            }
        });
        assert!(add_pending_submission_with_versions(
            &mut data,
            root,
            project_id,
            workspace_id,
            &[job_id],
            &submitted,
            WATCH_ORIGIN,
            PendingSubmissionMode::Full,
            None,
            None,
            true,
            false,
        ));
        let receipt = pending_submissions_for_root(&data, root)
            .into_iter()
            .next()
            .unwrap();

        assert_eq!(
            apply_pending_resolution(&mut data, root, &receipt, PendingResolution::Retry,),
            (true, true)
        );

        assert_eq!(data["workspaces"][root]["files"][path], version);
        assert!(pending_submissions_for_root(&data, root).is_empty());
        assert_eq!(receipt.mode, PendingSubmissionMode::Full);
    }

    #[test]
    fn attestation_only_watch_receipt_round_trips_without_job_ids() {
        let root = "/home/u/proj";
        let path = "/home/u/proj/a.rs";
        let project_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let submitted = BTreeMap::from([(path.to_string(), "2026-06-09T00:00:01Z".to_string())]);
        let mut data = json!({
            "workspaces": {
                root: {
                    "files": { path: "2026-06-09T00:00:01Z" }
                }
            }
        });

        assert!(add_pending_submission_with_versions(
            &mut data,
            root,
            project_id,
            workspace_id,
            &[],
            &submitted,
            WATCH_ORIGIN,
            PendingSubmissionMode::Targeted,
            None,
            None,
            true,
            true,
        ));
        let restored: Value = serde_json::from_str(&serde_json::to_string(&data).unwrap()).unwrap();
        let receipt = pending_submissions_for_root(&restored, root)
            .into_iter()
            .next()
            .unwrap();

        assert!(receipt.job_ids.is_empty());
        assert!(receipt.jobs_completed);
        assert_eq!(receipt.path_versions, submitted);
        assert_eq!(receipt.origin, WATCH_ORIGIN);
    }

    #[test]
    fn terminal_job_status_is_fail_closed() {
        assert_eq!(
            ingest_job_terminal_state(&json!({"status": "completed", "files_failed": 0})),
            JobTerminalState::Completed
        );
        assert_eq!(
            ingest_job_terminal_state(&json!({"status": "completed", "files_failed": 1})),
            JobTerminalState::Failed
        );
        assert_eq!(
            ingest_job_terminal_state(&json!({"status": "canceled"})),
            JobTerminalState::Failed
        );
        assert_eq!(
            ingest_job_terminal_state(&json!({"status": "cancelled"})),
            JobTerminalState::Failed
        );
        assert_eq!(
            ingest_job_terminal_state(&json!({"status": "running"})),
            JobTerminalState::Pending
        );
        assert_eq!(
            ingest_job_terminal_state(&json!({"status": "new_backend_state"})),
            JobTerminalState::Unknown
        );
        assert_eq!(
            ingest_job_terminal_state(&json!({})),
            JobTerminalState::Unknown
        );
    }

    #[test]
    fn batch_uses_shared_ignore_and_read_failure_policy() {
        let dir = tempfile::tempdir().unwrap();
        let keep = dir.path().join("keep.rs");
        let ignored = dir.path().join("ignored.rs");
        let secret = dir.path().join("opencode.json");
        let invalid = dir.path().join("invalid.rs");
        std::fs::write(&keep, "fn keep() {}\n").unwrap();
        std::fs::write(&ignored, "fn secret() {}\n").unwrap();
        std::fs::write(&secret, r#"{"env":{"API_KEY":"secret"}}"#).unwrap();
        std::fs::write(&invalid, [0xff, 0xfe]).unwrap();
        std::fs::write(dir.path().join(".contextignore"), "ignored.rs\n").unwrap();
        let selected = vec![
            keep.to_string_lossy().into_owned(),
            ignored.to_string_lossy().into_owned(),
            secret.to_string_lossy().into_owned(),
            invalid.to_string_lossy().into_owned(),
        ];

        let batch = build_ingest_batch(&selected, dir.path().to_str().unwrap());

        assert_eq!(batch.files.len(), 1);
        assert_eq!(batch.files[0]["path"], "keep.rs");
        let mut deleted = batch.deleted_paths.clone();
        deleted.sort();
        assert_eq!(
            deleted,
            vec![
                "ignored.rs".to_string(),
                "invalid.rs".to_string(),
                "opencode.json".to_string(),
            ]
        );
        assert_eq!(batch.completed.len(), 4);
        assert!(batch
            .completed
            .contains(&invalid.to_string_lossy().into_owned()));
    }

    #[cfg(unix)]
    #[test]
    fn batch_rejects_symlink_escape_without_clearing_ledger() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("outside.rs");
        std::fs::write(&outside_file, "fn outside() {}\n").unwrap();
        let escaped = dir.path().join("escaped.rs");
        symlink(&outside_file, &escaped).unwrap();

        let batch = build_ingest_batch(
            &[escaped.to_string_lossy().into_owned()],
            dir.path().to_str().unwrap(),
        );

        assert!(batch.files.is_empty());
        assert!(batch.deleted_paths.is_empty());
        assert!(batch.completed.is_empty());
    }
}
