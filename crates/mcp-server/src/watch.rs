//! `contextstream-mcp watch` — editor-agnostic content-freshness watcher.
//!
//! A debounced filesystem watcher over the user's mapped project folders
//! (read from `~/.contextstream/mappings.json` and
//! `~/.contextstream/indexed-projects.json`). On change it pushes an exact delta
//! ([`ContextStreamClient::ingest_files_content`]) with a full-scan fallback
//! ([`ContextStreamClient::ingest_local`]), but editor-agnostic and continuous —
//! so editors *without* ContextStream hooks (Codex, Kilo,
//! Antigravity, ...) still get their local edits indexed automatically.
//!
//! Boundary note (see ticket P6): the bytes are always pushed from the
//! client. The remote gateway cannot read a user's local filesystem, so the
//! only way un-pushed edits reach the server is via something running on the
//! user's machine — this watcher (or the `dirty-files.json` drain). The
//! server can *orchestrate* a refresh but never read the files directly.
//!
//! Behavior:
//! - **Singleton per machine.** A lockfile at `~/.contextstream/watch.lock`
//!   ensures only one watcher runs even if several editors try to launch one.
//! - **Debounced + coalesced.** Rapid saves within the quiet window collapse
//!   into a single re-ingest per project.
//! - **Ignore-aware.** Event paths under build/VCS dirs are ignored up front;
//!   the actual ingest additionally honors `.gitignore` / `.contextignore` /
//!   git excludes and the file-size cap (via `read_local_files`).
//! - **Non-billed.** Ingests are marked `background: true` + `origin: "watch"`,
//!   landing in the system lane (mirrors the keep-warm daemon's
//!   `keep_warm_daemon` origin) so the watcher never charges user credits.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use mcp_client::{
    ContextStreamClient, IngestLocalParams, SyncBridgeCheckoutRegistration, SyncBridgeRefreshClaim,
    TargetedFileDecision,
};
use mcp_types::config::VERSION;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::hook_handlers::dirty_drain::{
    self, PendingSubmissionMode, PendingSubmissionReservation, WatchSubmissionRetry,
};
use crate::hook_handlers::post_tool_use::should_index;

/// Quiet window after the last change before a project is re-ingested.
const DEBOUNCE: Duration = Duration::from_millis(1500);

/// How often the debounce queue is checked for due projects.
const FLUSH_TICK: Duration = Duration::from_millis(500);

/// How often accepted watcher jobs are polled. The first reconciliation also
/// runs at startup so a process restart resumes durable receipts immediately.
const JOB_RECONCILE_INTERVAL: Duration = Duration::from_secs(5);

/// How often the watcher re-reads the registries to pick up newly mapped
/// projects (and drop folders that disappeared).
const REENUMERATE_INTERVAL: Duration = Duration::from_secs(120);

/// How often the singleton lock's heartbeat timestamp is refreshed.
const LOCK_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

/// Upper bound on the number of files the bulk fallback ingest will consider.
/// The ingest path's mtime/hash pre-filter means only *changed* files are
/// actually uploaded, but this caps the per-flush scan so a giant monorepo
/// can't turn one save into an unbounded walk.
const WATCH_MAX_FILES: usize = 20_000;

/// When a debounce window accumulates at most this many distinct changed paths,
/// the watcher pushes *only those files* (targeted POST) instead of re-scanning
/// the whole project tree. Above it (mass changes like `git checkout`/branch
/// switches) the watcher falls back to a full `ingest_local` scan, which
/// streams + mtime-filters + diffs deletions far more efficiently than reading
/// thousands of files into one request.
const WATCH_TARGETED_MAX: usize = 256;

/// Origin tag forwarded to the API (`x-contextstream-ingest-origin`) so the
/// backend classifies watcher ingests into the system/watch lane.
const WATCH_ORIGIN: &str = "watch";

/// Bounded channel between the (sync) notify callback and the async loop.
const EVENT_CHANNEL_CAP: usize = 4096;

const WATCH_LOCK_FILE: &str = "watch.lock";
const WATCH_HEARTBEAT_FILE: &str = "watch.heartbeat.json";
const WATCH_STOP_REQUEST_FILE: &str = "watch.stop.json";
const WATCH_RELOAD_REQUEST_FILE: &str = "watch.reload";
const BRIDGE_CONTROL_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(60);
const BRIDGE_CONTROL_CLAIM_INTERVAL: Duration = Duration::from_secs(30);
const BRIDGE_CONTROL_LEASE_SECONDS: u64 = 180;
const BRIDGE_CONTROL_RENEW_INTERVAL: Duration = Duration::from_secs(45);
const WATCH_HEARTBEAT_SCHEMA_VERSION: u16 = 1;
const WATCH_HEARTBEAT_ROLE: &str = "hosted_sync_bridge";
const MAX_HEARTBEAT_BYTES: u64 = 64 * 1024;
const MAX_HEARTBEAT_FUTURE_SKEW: chrono::Duration = chrono::Duration::minutes(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncBridgeHealthState {
    Running,
    Degraded,
    Stopped,
    Disabled,
}

/// Privacy-bounded health projection for doctor and local diagnostics.
///
/// Checkout roots and lock ownership tokens are deliberately omitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SyncBridgeHealth {
    pub state: SyncBridgeHealthState,
    pub enabled: bool,
    pub lock_held: Option<bool>,
    pub heartbeat_fresh: bool,
    pub pid: Option<u32>,
    pub target_count: usize,
    pub refreshed_at: Option<String>,
    pub role: &'static str,
    pub version: Option<String>,
    pub detail: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct WatchHeartbeat {
    schema_version: u16,
    role: String,
    version: String,
    pid: u32,
    owner_id: Uuid,
    target_count: usize,
    refreshed_at: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct WatchStopRequest {
    schema_version: u16,
    role: String,
    pid: u32,
    owner_id: Uuid,
    requested_at: String,
}

/// A single project folder the watcher keeps fresh.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WatchTarget {
    pub folder_path: String,
    pub project_id: Uuid,
    pub workspace_id: Option<Uuid>,
}

/// Whether the watcher is enabled. On by default; opt out with
/// `CONTEXTSTREAM_WATCH=0` (also accepts false/off/no).
pub fn watch_enabled() -> bool {
    !matches!(
        std::env::var("CONTEXTSTREAM_WATCH")
            .ok()
            .as_deref()
            .map(|s| s.trim().to_ascii_lowercase())
            .as_deref(),
        Some("0") | Some("false") | Some("off") | Some("no")
    )
}

fn contextstream_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".contextstream"))
}

fn read_json_file(path: &Path) -> Option<Value> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Pure target collection from the two registry JSON documents, deduplicated
/// by canonical checkout root, filtered to existing directories (per
/// `dir_exists`).
///
/// Split out from disk IO so it can be unit-tested without touching `$HOME`.
fn collect_watch_targets<F>(
    indexed: Option<&Value>,
    mappings: Option<&Value>,
    dir_exists: F,
) -> Vec<WatchTarget>
where
    F: Fn(&str) -> bool,
{
    let mut by_root: HashMap<PathBuf, WatchTarget> = HashMap::new();
    let mut ambiguous_roots: HashSet<PathBuf> = HashSet::new();

    let mut consider = |folder: &str, project_id: Option<Uuid>, workspace_id: Option<Uuid>| {
        let Some(pid) = project_id else {
            return;
        };
        // Existing directory only (drops dead /tmp scratch + file-scope
        // entries), and never a filesystem root.
        let p = Path::new(folder);
        if p.parent().is_none() || !dir_exists(folder) {
            return;
        }
        let root = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        if ambiguous_roots.contains(&root) {
            return;
        }
        let conflicts_with_existing = by_root.get(&root).is_some_and(|existing| {
            existing.project_id != pid
                || matches!(
                    (existing.workspace_id, workspace_id),
                    (Some(existing), Some(candidate)) if existing != candidate
                )
        });
        if conflicts_with_existing {
            by_root.remove(&root);
            ambiguous_roots.insert(root);
            return;
        }
        let entry = by_root.entry(root).or_insert_with(|| WatchTarget {
            folder_path: folder.to_string(),
            project_id: pid,
            workspace_id,
        });
        if entry.workspace_id.is_none() {
            entry.workspace_id = workspace_id;
        }
    };

    // 1) Local ingest registry (indexed-projects.json): { "projects": { path: { project_id } } }
    if let Some(projects) = indexed
        .and_then(|d| d.get("projects"))
        .and_then(|p| p.as_object())
    {
        for (path, info) in projects {
            let pid = info
                .get("project_id")
                .and_then(|v| v.as_str())
                .and_then(|s| Uuid::parse_str(s).ok());
            consider(path, pid, None);
        }
    }

    // 2) Global workspace mappings (mappings.json): richer — carries
    //    workspace_id, and covers folders mapped but not yet locally ingested.
    if let Some(data) = mappings {
        let arr = data
            .get("mappings")
            .and_then(|v| v.as_array())
            .cloned()
            .or_else(|| data.as_array().cloned())
            .unwrap_or_default();
        for m in arr {
            let path = m.get("path").and_then(|v| v.as_str());
            let pid = m
                .get("project_id")
                .and_then(|v| v.as_str())
                .and_then(|s| Uuid::parse_str(s).ok());
            let wid = m
                .get("workspace_id")
                .and_then(|v| v.as_str())
                .and_then(|s| Uuid::parse_str(s).ok());
            if let Some(path) = path {
                consider(path, pid, wid);
            }
        }
    }

    let mut targets: Vec<WatchTarget> = by_root.into_values().collect();
    // Deterministic order so longest-prefix resolution and watch setup are stable.
    targets.sort_by(|a, b| a.folder_path.cmp(&b.folder_path));
    targets
}

/// Read both registries from `~/.contextstream` and return watch targets.
fn enumerate_targets() -> Vec<WatchTarget> {
    let Some(dir) = contextstream_dir() else {
        return Vec::new();
    };
    let indexed = read_json_file(&dir.join("indexed-projects.json"));
    let mappings = read_json_file(&dir.join("mappings.json"));
    collect_watch_targets(indexed.as_ref(), mappings.as_ref(), |p| {
        Path::new(p).is_dir()
    })
    .into_iter()
    .filter_map(authorize_watch_target)
    .collect()
}

/// Require the machine-local, checkout-bound config to agree with a watcher
/// registry entry. Registries are caches and may drift independently; neither
/// is sufficient authority for uploading or deleting source bytes.
fn authorize_watch_target(mut target: WatchTarget) -> Option<WatchTarget> {
    let configured_workspace_id =
        mcp_session::auto_init::checkout_binding_workspace(&target.folder_path, target.project_id)?;
    if target
        .workspace_id
        .is_some_and(|target_workspace| target_workspace != configured_workspace_id)
    {
        return None;
    }
    target.workspace_id = Some(configured_workspace_id);
    Some(target)
}

fn watch_target_matches_checkout_config(target: &WatchTarget) -> bool {
    authorize_watch_target(target.clone()).is_some()
}

#[derive(Clone, Debug)]
struct AuthorizedBridgeTarget {
    target: WatchTarget,
    registration: SyncBridgeCheckoutRegistration,
}

fn authorized_bridge_targets(targets: &[WatchTarget]) -> Vec<AuthorizedBridgeTarget> {
    let mut candidates = Vec::new();
    let mut counts: HashMap<String, usize> = HashMap::new();
    for target in targets {
        let Some(workspace_id) = target.workspace_id else {
            continue;
        };
        let Ok(registration) = ContextStreamClient::sync_bridge_checkout_registration(
            &target.folder_path,
            target.project_id,
            workspace_id,
        ) else {
            continue;
        };
        let checkout_id = registration.checkout_id.clone();
        *counts.entry(checkout_id.clone()).or_default() += 1;
        candidates.push(AuthorizedBridgeTarget {
            target: target.clone(),
            registration,
        });
    }
    // A checkout ID that resolves to multiple local roots is ambiguous. Do
    // not advertise or service it until setup repairs the cloned identity.
    candidates.retain(|candidate| {
        counts
            .get(&candidate.registration.checkout_id)
            .copied()
            .unwrap_or_default()
            == 1
    });
    candidates.sort_by(|left, right| {
        left.registration
            .checkout_id
            .cmp(&right.registration.checkout_id)
    });
    candidates
}

fn target_for_refresh_claim<'a>(
    claim: &SyncBridgeRefreshClaim,
    targets: &'a [AuthorizedBridgeTarget],
) -> Option<&'a WatchTarget> {
    if claim.request_id.trim().is_empty()
        || claim.lease_token.trim().is_empty()
        || claim.checkout_id.trim().is_empty()
    {
        return None;
    }
    targets
        .iter()
        .find(|candidate| {
            candidate.registration.checkout_id == claim.checkout_id
                && candidate.registration.project_id == claim.project_id
                && candidate.registration.workspace_id == claim.workspace_id
        })
        .map(|candidate| &candidate.target)
}

fn bridge_control_endpoint_unsupported(error: &mcp_types::Error) -> bool {
    matches!(
        error,
        mcp_types::Error::Http {
            status: 404 | 405 | 501,
            ..
        }
    )
}

fn bridge_ingest_summary(result: &Value) -> Value {
    let job_ids = result
        .get("job_ids")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(|raw| Uuid::parse_str(raw).ok())
        .map(|job_id| job_id.to_string())
        .collect::<Vec<_>>();
    let server_processing_status = result
        .get("server_processing_status")
        .and_then(Value::as_str)
        .filter(|status| matches!(*status, "not_required" | "in_progress" | "completed"))
        .unwrap_or("unknown");
    serde_json::json!({
        "schema_version": 1,
        "committed": ContextStreamClient::ingest_result_committed(result),
        "scan_complete": ContextStreamClient::ingest_scan_complete(result),
        "files_uploaded": result.get("files_uploaded").and_then(Value::as_u64).unwrap_or(0),
        "files_indexed": result.get("files_indexed").and_then(Value::as_u64).unwrap_or(0),
        "files_skipped": result.get("files_skipped").and_then(Value::as_u64).unwrap_or(0),
        "files_failed": result.get("files_failed").and_then(Value::as_u64).unwrap_or(0),
        "pending_jobs": result.get("pending_jobs").and_then(Value::as_u64).unwrap_or(0),
        "job_ids": job_ids,
        "server_processing_status": server_processing_status,
    })
}

fn hosted_refresh_ack_detail(result: &Value, summary: &Value) -> Option<&'static str> {
    let scan_complete = ContextStreamClient::ingest_scan_complete(result);
    let result_committed = ContextStreamClient::ingest_result_committed(result);
    if scan_complete && result_committed {
        None
    } else if !scan_complete {
        Some("scan_incomplete")
    } else if result
        .get("pending_jobs")
        .and_then(Value::as_u64)
        .is_some_and(|pending| pending > 0)
        || summary
            .get("job_ids")
            .and_then(Value::as_array)
            .is_some_and(|job_ids| !job_ids.is_empty())
    {
        Some("server_jobs_pending")
    } else {
        Some("commit_unconfirmed")
    }
}

fn hosted_refresh_ingest_params(target: &WatchTarget, force: bool) -> IngestLocalParams {
    IngestLocalParams {
        path: target.folder_path.clone(),
        workspace_id: target.workspace_id,
        project_id: Some(target.project_id),
        force: Some(force),
        generate_editor_rules: None,
        include_media: None,
        // A hosted project(index) request is an explicit refresh, not an
        // incidental filesystem-event flush. It must close the full checkout
        // scan epoch in one command so a bounded page cannot be mistaken for a
        // completed refresh. Ordinary watch fallbacks remain capped.
        max_files: None,
        background: Some(true),
        origin: Some("hosted_refresh".to_string()),
        reroot: None,
    }
}

async fn process_hosted_refresh_claim(
    client: ContextStreamClient,
    claim: SyncBridgeRefreshClaim,
    targets: &[AuthorizedBridgeTarget],
) {
    let Some(target) = target_for_refresh_claim(&claim, targets).cloned() else {
        if !claim.request_id.trim().is_empty()
            && !claim.checkout_id.trim().is_empty()
            && !claim.lease_token.trim().is_empty()
        {
            let _ = client
                .acknowledge_sync_bridge_refresh(
                    &claim.request_id,
                    &claim.checkout_id,
                    &claim.lease_token,
                    "rejected",
                    Some("scope_mismatch"),
                    None,
                )
                .await;
        }
        return;
    };
    if !watch_target_matches_checkout_config(&target) {
        let _ = client
            .acknowledge_sync_bridge_refresh(
                &claim.request_id,
                &claim.checkout_id,
                &claim.lease_token,
                "rejected",
                Some("checkout_binding_changed"),
                None,
            )
            .await;
        return;
    }

    let (done_tx, mut done_rx) = tokio::sync::watch::channel(false);
    let renewal_client = client.clone();
    let renewal_request_id = claim.request_id.clone();
    let renewal_checkout_id = claim.checkout_id.clone();
    let renewal_lease_token = claim.lease_token.clone();
    let renewal = tokio::spawn(async move {
        let mut tick = tokio::time::interval(BRIDGE_CONTROL_RENEW_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        tick.reset();
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if let Err(error) = renewal_client
                        .renew_sync_bridge_refresh(
                            &renewal_request_id,
                            &renewal_checkout_id,
                            &renewal_lease_token,
                            BRIDGE_CONTROL_LEASE_SECONDS,
                        )
                        .await
                    {
                        tracing::debug!("watch: could not renew hosted refresh lease: {}", error);
                    }
                }
                changed = done_rx.changed() => {
                    if changed.is_err() || *done_rx.borrow() {
                        break;
                    }
                }
            }
        }
    });

    let result = client
        .ingest_local(hosted_refresh_ingest_params(&target, claim.force))
        .await;
    let _ = done_tx.send(true);
    let _ = renewal.await;

    match result {
        Ok(result) => {
            let scan_complete = ContextStreamClient::ingest_scan_complete(&result);
            let result_committed = ContextStreamClient::ingest_result_committed(&result);
            let committed = scan_complete && result_committed;
            let summary = bridge_ingest_summary(&result);
            let status = if committed { "completed" } else { "pending" };
            let detail = hosted_refresh_ack_detail(&result, &summary);
            if let Err(error) = client
                .acknowledge_sync_bridge_refresh(
                    &claim.request_id,
                    &claim.checkout_id,
                    &claim.lease_token,
                    status,
                    detail,
                    Some(&summary),
                )
                .await
            {
                tracing::debug!("watch: could not acknowledge hosted refresh: {}", error);
            }
        }
        Err(error) => {
            tracing::warn!(
                project_id = %target.project_id,
                checkout_id = %claim.checkout_id,
                "watch: hosted refresh ingest failed: {}",
                error
            );
            let _ = client
                .acknowledge_sync_bridge_refresh(
                    &claim.request_id,
                    &claim.checkout_id,
                    &claim.lease_token,
                    "failed",
                    Some("ingest_failed"),
                    None,
                )
                .await;
        }
    }
}

async fn claim_and_process_hosted_refreshes(
    client: ContextStreamClient,
    bridge_instance_id: Uuid,
    targets: Vec<WatchTarget>,
    bridge_control_supported: Arc<AtomicBool>,
) {
    let authorized = authorized_bridge_targets(&targets);
    let registrations = authorized
        .iter()
        .map(|target| target.registration.clone())
        .collect::<Vec<_>>();
    match client
        .claim_sync_bridge_refreshes(
            bridge_instance_id,
            &registrations,
            1,
            BRIDGE_CONTROL_LEASE_SECONDS,
        )
        .await
    {
        Ok(claims) => {
            for claim in claims.into_iter().take(8) {
                process_hosted_refresh_claim(client.clone(), claim, &authorized).await;
            }
        }
        Err(error) if bridge_control_endpoint_unsupported(&error) => {
            bridge_control_supported.store(false, Ordering::Release);
            tracing::debug!(
                "watch: hosted refresh control endpoint is not available on this deployment"
            );
        }
        Err(error) => {
            tracing::debug!("watch: could not claim hosted refresh requests: {}", error);
        }
    }
}

async fn publish_bridge_control_heartbeat(
    client: ContextStreamClient,
    bridge_instance_id: Uuid,
    targets: Vec<WatchTarget>,
    bridge_control_supported: Arc<AtomicBool>,
) {
    let registrations = authorized_bridge_targets(&targets)
        .into_iter()
        .map(|target| target.registration)
        .collect::<Vec<_>>();
    if let Err(error) = client
        .sync_bridge_heartbeat(bridge_instance_id, &registrations, "running")
        .await
    {
        if bridge_control_endpoint_unsupported(&error) {
            bridge_control_supported.store(false, Ordering::Release);
            tracing::debug!(
                "watch: hosted bridge heartbeat endpoint is not available on this deployment"
            );
        } else {
            tracing::debug!(
                "watch: could not publish hosted bridge heartbeat: {}",
                error
            );
        }
    }
}

/// Resolve which target a changed path belongs to using longest-prefix match,
/// so nested mapped projects route to the most specific one.
fn resolve_target_for_path<'a>(path: &Path, targets: &'a [WatchTarget]) -> Option<&'a WatchTarget> {
    let mut best: Option<(usize, &WatchTarget)> = None;
    for target in targets {
        let root = Path::new(&target.folder_path);
        if path.starts_with(root) {
            let score = root.components().count();
            if best.map(|(b, _)| score > b).unwrap_or(true) {
                best = Some((score, target));
            }
        }
    }
    best.map(|(_, t)| t)
}

/// Coarse up-front filter: skip events whose path traverses a build/VCS dir.
fn path_has_ignored_component(path: &Path) -> bool {
    path.components().any(|c| {
        c.as_os_str()
            .to_str()
            .map(ContextStreamClient::should_skip_ingest_directory)
            .unwrap_or(false)
    })
}

// ===========================================================================
// Singleton lock
// ===========================================================================

/// Guards single-watcher-per-machine with a kernel-held advisory lock.
///
/// The open file descriptor is retained for this value's full lifetime. Human
/// diagnostic metadata lives in a separate heartbeat file: rewriting metadata
/// can therefore never replace the inode carrying the singleton lock.
struct SingletonLock {
    lock_file: File,
    heartbeat_path: PathBuf,
    pid: u32,
    owner_id: Uuid,
}

impl SingletonLock {
    fn write_heartbeat(&self, target_count: usize) {
        let body = WatchHeartbeat {
            schema_version: WATCH_HEARTBEAT_SCHEMA_VERSION,
            role: WATCH_HEARTBEAT_ROLE.to_string(),
            version: VERSION.to_string(),
            pid: self.pid,
            owner_id: self.owner_id,
            target_count,
            refreshed_at: chrono::Utc::now().to_rfc3339(),
        };
        if let Ok(mut serialized) = serde_json::to_string(&body) {
            serialized.push('\n');
            if let Err(error) = crate::setup::safe_edit::write_owned_file_if_changed(
                &self.heartbeat_path,
                &serialized,
            ) {
                tracing::debug!("watch: could not update heartbeat: {}", error);
            }
        }
    }
}

impl Drop for SingletonLock {
    fn drop(&mut self) {
        if let Ok(raw) = std::fs::read_to_string(&self.heartbeat_path) {
            let heartbeat_is_ours = serde_json::from_str::<WatchHeartbeat>(&raw)
                .ok()
                .is_some_and(|heartbeat| {
                    heartbeat.pid == self.pid && heartbeat.owner_id == self.owner_id
                });
            if heartbeat_is_ours {
                let _ = crate::setup::safe_edit::remove_owned_file_if_unchanged(
                    &self.heartbeat_path,
                    &raw,
                );
            }
        }
        let _ = fs2::FileExt::unlock(&self.lock_file);
    }
}

fn open_singleton_lock_file(dir: &Path) -> Option<File> {
    std::fs::create_dir_all(dir).ok()?;
    OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join(WATCH_LOCK_FILE))
        .ok()
}

/// Try to become the singleton watcher. Returns `None` if another live watcher
/// already holds the lock.
fn acquire_singleton_lock_at(dir: &Path) -> Option<SingletonLock> {
    let lock_file = open_singleton_lock_file(dir)?;
    fs2::FileExt::try_lock_exclusive(&lock_file).ok()?;
    // Clear metadata written into watch.lock by pre-fs2 releases only after we
    // own this inode. The file itself remains in place forever to avoid split
    // lock races between an unlinked inode and a newly-created path.
    let _ = lock_file.set_len(0);
    let lock = SingletonLock {
        lock_file,
        heartbeat_path: dir.join(WATCH_HEARTBEAT_FILE),
        pid: std::process::id(),
        owner_id: Uuid::new_v4(),
    };
    lock.write_heartbeat(0);
    Some(lock)
}

fn acquire_singleton_lock() -> Option<SingletonLock> {
    acquire_singleton_lock_at(&contextstream_dir()?)
}

/// Best-effort launch probe. Failure to prove the lock is free fails closed;
/// the watcher process still repeats the authoritative acquisition itself.
fn singleton_lock_is_held_at(dir: &Path) -> bool {
    let Some(lock_file) = open_singleton_lock_file(dir) else {
        return true;
    };
    match fs2::FileExt::try_lock_exclusive(&lock_file) {
        Ok(()) => {
            let _ = fs2::FileExt::unlock(&lock_file);
            false
        }
        Err(_) => true,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockProbe {
    Held,
    Free,
    Unknown,
}

/// Read-only singleton probe used by diagnostics. Unlike the launch
/// optimization above, this never creates a directory or lock file.
fn probe_singleton_lock_at(dir: &Path) -> LockProbe {
    let lock_path = dir.join(WATCH_LOCK_FILE);
    let lock_file = match OpenOptions::new().read(true).write(true).open(lock_path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return LockProbe::Free,
        Err(_) => return LockProbe::Unknown,
    };
    match fs2::FileExt::try_lock_exclusive(&lock_file) {
        Ok(()) => {
            let _ = fs2::FileExt::unlock(&lock_file);
            LockProbe::Free
        }
        Err(_) => LockProbe::Held,
    }
}

fn read_watch_heartbeat(path: &Path) -> Result<Option<WatchHeartbeat>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_HEARTBEAT_BYTES
    {
        anyhow::bail!("invalid sync bridge heartbeat file");
    }
    let raw = std::fs::read_to_string(path)?;
    Ok(Some(serde_json::from_str(&raw)?))
}

fn read_watch_stop_request(path: &Path) -> Result<Option<(WatchStopRequest, String)>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_HEARTBEAT_BYTES
    {
        anyhow::bail!("invalid sync bridge stop request file");
    }
    let raw = std::fs::read_to_string(path)?;
    let request = serde_json::from_str(&raw)?;
    Ok(Some((request, raw)))
}

fn request_sync_bridge_stop_at(dir: &Path) -> Result<bool> {
    if probe_singleton_lock_at(dir) != LockProbe::Held {
        return Ok(false);
    }
    let heartbeat = read_watch_heartbeat(&dir.join(WATCH_HEARTBEAT_FILE))?
        .ok_or_else(|| anyhow::anyhow!("sync bridge lock is held but its heartbeat is missing"))?;
    if heartbeat.schema_version != WATCH_HEARTBEAT_SCHEMA_VERSION
        || heartbeat.role != WATCH_HEARTBEAT_ROLE
    {
        anyhow::bail!("sync bridge heartbeat identity is invalid");
    }
    let request = WatchStopRequest {
        schema_version: WATCH_HEARTBEAT_SCHEMA_VERSION,
        role: WATCH_HEARTBEAT_ROLE.to_string(),
        pid: heartbeat.pid,
        owner_id: heartbeat.owner_id,
        requested_at: chrono::Utc::now().to_rfc3339(),
    };
    let mut raw = serde_json::to_string(&request)?;
    raw.push('\n');
    crate::setup::safe_edit::write_owned_file_if_changed(&dir.join(WATCH_STOP_REQUEST_FILE), &raw)
}

/// Ask the exact currently-running managed singleton to exit gracefully.
///
/// The request is bound to the opaque lock owner token and PID from the
/// heartbeat. A racing replacement watcher therefore ignores a request meant
/// for its predecessor, and uninstall never sends a signal to a guessed PID.
pub fn request_sync_bridge_stop() -> Result<bool> {
    let Some(dir) = contextstream_dir() else {
        return Ok(false);
    };
    request_sync_bridge_stop_at(&dir)
}

/// Ask an already-running singleton to reload the machine mapping registry.
///
/// Enrollment writes a new exact-checkout mapping and then calls this helper;
/// without it, an existing watcher could take up to the normal two-minute
/// reconciliation interval to discover the target. The marker contains no
/// token, path, project id, or other sensitive state and is consumed once.
pub fn request_sync_bridge_reload() -> Result<bool> {
    let Some(dir) = contextstream_dir() else {
        return Ok(false);
    };
    if probe_singleton_lock_at(&dir) != LockProbe::Held {
        return Ok(false);
    }
    std::fs::create_dir_all(&dir)?;
    crate::setup::safe_edit::write_owned_file_if_changed(
        &dir.join(WATCH_RELOAD_REQUEST_FILE),
        "reload\n",
    )?;
    Ok(true)
}

fn consume_sync_bridge_reload_request_at(dir: &Path) -> bool {
    let path = dir.join(WATCH_RELOAD_REQUEST_FILE);
    match std::fs::remove_file(&path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            tracing::debug!("watch: could not consume reload request: {}", error);
            false
        }
    }
}

fn consume_sync_bridge_stop_request_at(dir: &Path, lock: &SingletonLock) -> bool {
    let path = dir.join(WATCH_STOP_REQUEST_FILE);
    let Ok(Some((request, raw))) = read_watch_stop_request(&path) else {
        return false;
    };
    if request.schema_version != WATCH_HEARTBEAT_SCHEMA_VERSION
        || request.role != WATCH_HEARTBEAT_ROLE
        || request.pid != lock.pid
        || request.owner_id != lock.owner_id
    {
        return false;
    }
    match crate::setup::safe_edit::remove_owned_file_if_unchanged(&path, &raw) {
        Ok(true) => true,
        Ok(false) => false,
        Err(error) => {
            tracing::debug!("watch: could not consume graceful stop request: {}", error);
            false
        }
    }
}

fn sync_bridge_health_at(dir: &Path, now: chrono::DateTime<chrono::Utc>) -> SyncBridgeHealth {
    if !watch_enabled() {
        return SyncBridgeHealth {
            state: SyncBridgeHealthState::Disabled,
            enabled: false,
            lock_held: None,
            heartbeat_fresh: false,
            pid: None,
            target_count: 0,
            refreshed_at: None,
            role: WATCH_HEARTBEAT_ROLE,
            version: None,
            detail: "the hosted sync bridge is disabled by CONTEXTSTREAM_WATCH".to_string(),
        };
    }

    let lock_probe = probe_singleton_lock_at(dir);
    let heartbeat = read_watch_heartbeat(&dir.join(WATCH_HEARTBEAT_FILE));
    let parsed = heartbeat
        .as_ref()
        .ok()
        .and_then(|heartbeat| heartbeat.as_ref());
    let timestamp = parsed.and_then(|heartbeat| {
        chrono::DateTime::parse_from_rfc3339(&heartbeat.refreshed_at)
            .ok()
            .map(|value| value.with_timezone(&chrono::Utc))
    });
    let age = timestamp.map(|timestamp| now.signed_duration_since(timestamp));
    let heartbeat_shape_valid = parsed.is_some_and(|heartbeat| {
        heartbeat.schema_version == WATCH_HEARTBEAT_SCHEMA_VERSION
            && heartbeat.role == WATCH_HEARTBEAT_ROLE
    });
    let heartbeat_fresh = heartbeat_shape_valid
        && age.is_some_and(|age| {
            age <= chrono::Duration::from_std(LOCK_REFRESH_INTERVAL * 3)
                .unwrap_or_else(|_| chrono::Duration::minutes(3))
                && age >= -MAX_HEARTBEAT_FUTURE_SKEW
        });
    let running = lock_probe == LockProbe::Held && heartbeat_fresh;
    let state = if running {
        SyncBridgeHealthState::Running
    } else if lock_probe == LockProbe::Free && parsed.is_none() {
        SyncBridgeHealthState::Stopped
    } else {
        SyncBridgeHealthState::Degraded
    };
    let detail = match state {
        SyncBridgeHealthState::Running => "managed hosted sync bridge is running",
        SyncBridgeHealthState::Stopped => "managed hosted sync bridge is not running",
        SyncBridgeHealthState::Degraded if heartbeat.is_err() => {
            "managed hosted sync bridge heartbeat is unreadable"
        }
        SyncBridgeHealthState::Degraded if lock_probe == LockProbe::Unknown => {
            "managed hosted sync bridge lock state is unreadable"
        }
        SyncBridgeHealthState::Degraded if !heartbeat_fresh => {
            "managed hosted sync bridge heartbeat is stale or invalid"
        }
        SyncBridgeHealthState::Degraded => "managed hosted sync bridge state is inconsistent",
        SyncBridgeHealthState::Disabled => unreachable!(),
    };

    SyncBridgeHealth {
        state,
        enabled: true,
        lock_held: match lock_probe {
            LockProbe::Held => Some(true),
            LockProbe::Free => Some(false),
            LockProbe::Unknown => None,
        },
        heartbeat_fresh,
        pid: parsed.map(|heartbeat| heartbeat.pid),
        target_count: parsed.map_or(0, |heartbeat| heartbeat.target_count),
        refreshed_at: parsed.map(|heartbeat| heartbeat.refreshed_at.clone()),
        role: WATCH_HEARTBEAT_ROLE,
        version: parsed.map(|heartbeat| heartbeat.version.clone()),
        detail: detail.to_string(),
    }
}

pub fn sync_bridge_health() -> SyncBridgeHealth {
    let Some(dir) = contextstream_dir() else {
        return SyncBridgeHealth {
            state: if watch_enabled() {
                SyncBridgeHealthState::Stopped
            } else {
                SyncBridgeHealthState::Disabled
            },
            enabled: watch_enabled(),
            lock_held: None,
            heartbeat_fresh: false,
            pid: None,
            target_count: 0,
            refreshed_at: None,
            role: WATCH_HEARTBEAT_ROLE,
            version: None,
            detail: "the hosted sync bridge state directory is unavailable".to_string(),
        };
    };
    sync_bridge_health_at(&dir, chrono::Utc::now())
}

// ===========================================================================
// Run loop
// ===========================================================================

/// A project with a pending re-ingest: the target plus the exact set of paths
/// the OS reported changed during the current debounce window.
struct PendingProject {
    target: WatchTarget,
    changed: HashSet<PathBuf>,
    deadline: Instant,
    force_full: bool,
}

/// In-memory scheduling key for one mutable checkout.
///
/// The project UUID alone is deliberately insufficient: one project may have
/// several linked worktrees or clones on the same machine. The canonical root
/// keeps their debounce, in-flight, retry, and cooldown obligations isolated.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct WatchTargetKey {
    project_id: Uuid,
    checkout_root: PathBuf,
}

fn watch_target_key(target: &WatchTarget) -> WatchTargetKey {
    WatchTargetKey {
        project_id: target.project_id,
        checkout_root: std::fs::canonicalize(&target.folder_path)
            .unwrap_or_else(|_| PathBuf::from(&target.folder_path)),
    }
}

#[derive(Debug, Default)]
struct TargetedWatchDelta {
    files: Vec<Value>,
    deleted_paths: Vec<String>,
    completed_paths: Vec<String>,
    rejected: usize,
}

/// Build params for a non-billed, background *full-scan* watcher ingest of
/// `target` (the bulk fallback for mass changes).
fn watch_ingest_params(target: &WatchTarget) -> IngestLocalParams {
    IngestLocalParams {
        path: target.folder_path.clone(),
        workspace_id: target.workspace_id,
        project_id: Some(target.project_id),
        force: None,
        generate_editor_rules: None,
        include_media: None,
        max_files: Some(WATCH_MAX_FILES),
        background: Some(true),
        origin: Some(WATCH_ORIGIN.to_string()),
        reroot: None,
    }
}

fn validate_watch_root(root: &Path) -> bool {
    mcp_client::validate_ingest_root(root, &mcp_client::IngestRootOptions::from_env()).is_ok()
}

/// Build a bounded exact delta using the same canonical containment, ignore,
/// secret-name, size, UTF-8, and TOCTOU policy as every other automatic writer.
fn targeted_watch_delta(root: &Path, changed: &HashSet<PathBuf>) -> TargetedWatchDelta {
    let root_str = root.to_string_lossy();
    let mut delta = TargetedWatchDelta::default();

    for path in changed {
        let path_str = path.to_string_lossy();
        if !should_index(&path_str) {
            delta.rejected += 1;
            continue;
        }
        match ContextStreamClient::targeted_text_file_decision(&root_str, &path_str) {
            TargetedFileDecision::Upload(payload) => {
                delta.files.push(payload);
                delta.completed_paths.push(path_str.into_owned());
            }
            TargetedFileDecision::Delete(relative) => {
                delta.deleted_paths.push(relative);
                delta.completed_paths.push(path_str.into_owned());
            }
            TargetedFileDecision::Reject if !path.exists() => {
                if let Some(relative) =
                    ContextStreamClient::safe_project_relative_path(&root_str, &path_str, false)
                {
                    delta.deleted_paths.push(relative);
                    delta.completed_paths.push(path_str.into_owned());
                } else {
                    delta.rejected += 1;
                }
            }
            TargetedFileDecision::Reject => delta.rejected += 1,
        }
    }
    delta.completed_paths.sort();
    delta.completed_paths.dedup();
    delta.deleted_paths.sort();
    delta.deleted_paths.dedup();
    delta
}

/// Dispatch a flush: targeted POST for a modest change set, full scan for mass
/// changes (or when we somehow have no specific paths).
fn watch_submission_mode(
    force_full: bool,
    dirty_force_full: bool,
    changed_count: usize,
) -> PendingSubmissionMode {
    if force_full || dirty_force_full || changed_count == 0 || changed_count > WATCH_TARGETED_MAX {
        PendingSubmissionMode::Full
    } else {
        PendingSubmissionMode::Targeted
    }
}

async fn flush_project(
    client: ContextStreamClient,
    target: &WatchTarget,
    changed: HashSet<PathBuf>,
    force_full: bool,
) -> Option<WatchSubmissionRetry> {
    if !validate_watch_root(Path::new(&target.folder_path))
        || !watch_target_matches_checkout_config(target)
    {
        tracing::warn!(
            "watch: skipped {} because its root, checkout binding, or API ownership is not current",
            target.folder_path
        );
        return None;
    }
    let workspace_id = target.workspace_id?;
    let initial_snapshot = dirty_drain::snapshot_watch_dirty(&target.folder_path);
    let mode = watch_submission_mode(force_full, initial_snapshot.force_full, changed.len());
    let retry = || WatchSubmissionRetry {
        paths: changed
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
        mode,
    };

    // Never enqueue a second generation while an older watcher/hook receipt is
    // still unresolved for this project. New path versions remain durable and
    // are retried after the prior generation reaches a terminal state.
    if dirty_drain::has_pending_submission_for_scope(
        &target.folder_path,
        target.project_id,
        workspace_id,
    ) {
        return Some(retry());
    }

    // A full scan owns a durable epoch even when overflow/error recovery has
    // only a root obligation and no retained path versions. The epoch lets a
    // concurrent edit force another pass instead of being cleared by this one.
    let dirty_snapshot = if mode == PendingSubmissionMode::Full {
        dirty_drain::begin_watch_full_scan(&target.folder_path)
    } else {
        initial_snapshot
    };

    let changed_paths = changed
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let submitted_versions = if mode == PendingSubmissionMode::Full {
        dirty_snapshot.path_versions
    } else {
        changed_paths
            .iter()
            .filter_map(|path| {
                dirty_snapshot
                    .path_versions
                    .get(path)
                    .map(|version| (path.clone(), version.clone()))
            })
            .collect()
    };
    if submitted_versions.is_empty() && mode != PendingSubmissionMode::Full {
        tracing::warn!(
            project_id = %target.project_id,
            "watch: durable path-version snapshot unavailable; deferring submission"
        );
        return Some(retry());
    }

    let checkout_guard = match ContextStreamClient::checkout_guard_for_scope(
        &target.folder_path,
        target.project_id,
        workspace_id,
    ) {
        Ok(checkout_guard) => checkout_guard,
        Err(error) => {
            tracing::warn!(
                project_id = %target.project_id,
                %error,
                "watch: could not bind submission to the current checkout; deferring"
            );
            return Some(retry());
        }
    };
    let Some(reservation) = dirty_drain::reserve_pending_submission(
        &target.folder_path,
        target.project_id,
        workspace_id,
        &submitted_versions,
        WATCH_ORIGIN,
        mode,
        dirty_snapshot.root_version.as_deref(),
        checkout_guard.as_deref(),
        mode == PendingSubmissionMode::Targeted,
    ) else {
        return Some(retry());
    };

    if mode == PendingSubmissionMode::Full {
        flush_project_full(client, target, submitted_versions, reservation).await
    } else {
        flush_project_targeted(client, target, changed, submitted_versions, reservation).await
    }
}

fn ingest_result_job_ids(result: &Value) -> Vec<Uuid> {
    result
        .get("job_ids")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(|raw| Uuid::parse_str(raw).ok())
        .collect()
}

/// Convert a pre-request reservation into a durable async or attestation-only
/// receipt. Contradictory/rejected outcomes cancel the reservation but leave
/// its dirty versions (and full-scan obligation) available for retry.
fn finalize_reserved_watch_submission(
    root: &str,
    reservation: &PendingSubmissionReservation,
    committed: bool,
    job_ids: &[Uuid],
    scan_complete: Option<bool>,
) -> bool {
    let finalized = if committed && job_ids.is_empty() {
        dirty_drain::finalize_pending_submission(root, reservation, &[], true, scan_complete)
    } else if !committed && !job_ids.is_empty() {
        dirty_drain::finalize_pending_submission(root, reservation, job_ids, false, scan_complete)
    } else {
        false
    };
    if !finalized {
        let _ = dirty_drain::cancel_pending_submission(root, reservation);
    }
    finalized
}

/// Bulk fallback: full `ingest_local` scan (mtime/hash pre-filter + manifest
/// deletion diff). Used for mass changes like branch switches.
async fn flush_project_full(
    client: ContextStreamClient,
    target: &WatchTarget,
    submitted_versions: std::collections::BTreeMap<String, String>,
    reservation: PendingSubmissionReservation,
) -> Option<WatchSubmissionRetry> {
    let retry = || WatchSubmissionRetry {
        paths: submitted_versions.keys().cloned().collect(),
        mode: PendingSubmissionMode::Full,
    };
    match client.ingest_local(watch_ingest_params(target)).await {
        Ok(result) => {
            let files = result
                .get("files_indexed")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            tracing::info!(
                "watch: re-ingested {} via full scan ({} file(s) changed)",
                target.folder_path,
                files
            );
            let committed = result
                .get("committed")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let scan_complete = result
                .get("scan_complete")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let job_ids = ingest_result_job_ids(&result);
            if finalize_reserved_watch_submission(
                &target.folder_path,
                &reservation,
                committed,
                &job_ids,
                Some(scan_complete),
            ) {
                None
            } else {
                Some(retry())
            }
        }
        Err(e) => {
            tracing::debug!(
                "watch: full ingest failed for {}: {}",
                target.folder_path,
                e
            );
            let _ = dirty_drain::cancel_pending_submission(&target.folder_path, &reservation);
            Some(retry())
        }
    }
}

/// Targeted fast path: push only the specific changed files (and deletions) the
/// OS reported, without walking the project tree. Mirrors the PostToolUse
/// hook's batched `/files/ingest` POST, but editor-agnostic and continuous.
async fn flush_project_targeted(
    client: ContextStreamClient,
    target: &WatchTarget,
    changed: HashSet<PathBuf>,
    observed_versions: std::collections::BTreeMap<String, String>,
    reservation: PendingSubmissionReservation,
) -> Option<WatchSubmissionRetry> {
    let root = Path::new(&target.folder_path);
    if !validate_watch_root(root) {
        let _ = dirty_drain::cancel_pending_submission(&target.folder_path, &reservation);
        return None;
    }
    let delta = targeted_watch_delta(root, &changed);

    let submitted_versions = delta
        .completed_paths
        .iter()
        .filter_map(|path| {
            observed_versions
                .get(path)
                .map(|version| (path.clone(), version.clone()))
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    if delta.rejected > 0
        || (delta.files.is_empty() && delta.deleted_paths.is_empty())
        || submitted_versions != observed_versions
    {
        let _ = dirty_drain::cancel_pending_submission(&target.folder_path, &reservation);
        let _ = dirty_drain::mark_watch_force_full(&target.folder_path);
        return Some(WatchSubmissionRetry {
            paths: changed
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect(),
            mode: PendingSubmissionMode::Full,
        });
    }
    let retry = || WatchSubmissionRetry {
        paths: delta.completed_paths.clone(),
        mode: PendingSubmissionMode::Targeted,
    };

    // File reads can race a checkout replacement. Repeat local identity after
    // payload construction and immediately before sending. The client performs
    // the uncached API ownership check after receiving this prepared payload.
    if !validate_watch_root(root) || !watch_target_matches_checkout_config(target) {
        tracing::warn!(
            "watch: discarded prepared delta for {} because scope changed during payload construction",
            target.folder_path
        );
        let _ = dirty_drain::cancel_pending_submission(&target.folder_path, &reservation);
        return None;
    }
    let Some(expected_workspace_id) = target.workspace_id else {
        let _ = dirty_drain::cancel_pending_submission(&target.folder_path, &reservation);
        return None;
    };

    let uploaded = delta.files.len();
    let deletions = delta.deleted_paths.len();
    match client
        .ingest_files_from_hook(
            target.project_id,
            expected_workspace_id,
            delta.files,
            delta.deleted_paths,
            true,
            Some(WATCH_ORIGIN),
            Some(&target.folder_path),
            false,
            false,
        )
        .await
    {
        Ok(outcome) => {
            let committed = outcome.committed;
            tracing::info!(
                "watch: pushed {} changed + {} deleted file(s) for {} ({} rejected, committed={})",
                uploaded,
                deletions,
                target.folder_path,
                delta.rejected,
                committed
            );
            if finalize_reserved_watch_submission(
                &target.folder_path,
                &reservation,
                committed,
                &outcome.job_ids,
                Some(true),
            ) {
                None
            } else {
                Some(retry())
            }
        }
        Err(e) => {
            tracing::debug!(
                "watch: targeted ingest failed for {}: {}",
                target.folder_path,
                e
            );
            let _ = dirty_drain::cancel_pending_submission(&target.folder_path, &reservation);
            Some(retry())
        }
    }
}

fn enqueue_watch_retry(
    pending: &Arc<Mutex<HashMap<WatchTargetKey, PendingProject>>>,
    target: &WatchTarget,
    retry: WatchSubmissionRetry,
) {
    if retry.paths.is_empty() && retry.mode != PendingSubmissionMode::Full {
        return;
    }
    let mut guard = pending.lock().unwrap_or_else(|error| error.into_inner());
    let entry = guard
        .entry(watch_target_key(target))
        .or_insert_with(|| PendingProject {
            target: target.clone(),
            changed: HashSet::new(),
            deadline: Instant::now() + DEBOUNCE,
            force_full: retry.mode == PendingSubmissionMode::Full,
        });
    entry.target = target.clone();
    entry.force_full |= retry.mode == PendingSubmissionMode::Full;
    entry.deadline = Instant::now() + DEBOUNCE;
    entry
        .changed
        .extend(retry.paths.into_iter().map(PathBuf::from));
}

fn enqueue_watch_snapshot(
    pending: &Arc<Mutex<HashMap<WatchTargetKey, PendingProject>>>,
    target: &WatchTarget,
    snapshot: dirty_drain::WatchDirtySnapshot,
) {
    if !snapshot.force_full && snapshot.path_versions.is_empty() {
        return;
    }
    let mode = if snapshot.force_full || snapshot.path_versions.len() > WATCH_TARGETED_MAX {
        PendingSubmissionMode::Full
    } else {
        PendingSubmissionMode::Targeted
    };
    enqueue_watch_retry(
        pending,
        target,
        WatchSubmissionRetry {
            paths: snapshot.path_versions.into_keys().collect(),
            mode,
        },
    );
}

fn take_due_watch_projects(
    pending: &Arc<Mutex<HashMap<WatchTargetKey, PendingProject>>>,
    inflight: &Arc<Mutex<HashSet<WatchTargetKey>>>,
    now: Instant,
) -> Vec<(WatchTarget, HashSet<PathBuf>, bool)> {
    let mut guard = pending.lock().unwrap_or_else(|error| error.into_inner());
    let ready: Vec<WatchTargetKey> = guard
        .iter()
        .filter(|(_, entry)| entry.deadline <= now)
        .map(|(key, _)| key.clone())
        .collect();
    let mut due = Vec::new();
    for key in ready {
        // Never run two generations for one checkout. Other roots for the same
        // project remain independent and may flush concurrently.
        let busy = inflight
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains(&key);
        if busy {
            if let Some(entry) = guard.get_mut(&key) {
                entry.deadline = now + DEBOUNCE;
            }
            continue;
        }
        if let Some(entry) = guard.remove(&key) {
            due.push((entry.target, entry.changed, entry.force_full));
        }
    }
    due
}

async fn reconcile_watch_targets(
    client: ContextStreamClient,
    targets: Vec<WatchTarget>,
    pending: Arc<Mutex<HashMap<WatchTargetKey, PendingProject>>>,
) {
    for target in targets {
        let Some(workspace_id) = target.workspace_id else {
            continue;
        };
        if !validate_watch_root(Path::new(&target.folder_path))
            || !watch_target_matches_checkout_config(&target)
        {
            continue;
        }
        for retry in dirty_drain::reconcile_watch_submissions(
            &client,
            &target.folder_path,
            target.project_id,
            workspace_id,
        )
        .await
        {
            enqueue_watch_retry(&pending, &target, retry);
        }

        // Dirty versions can outlive the in-memory debounce queue when the
        // process crashes before submission, or when a newer edit lands while
        // an older receipt is in flight. Once no receipt protects the scope,
        // seed those durable paths back into the watcher. Crossing the targeted
        // cap restores full-scan semantics after restart.
        if !dirty_drain::has_pending_submission_for_scope(
            &target.folder_path,
            target.project_id,
            workspace_id,
        ) {
            enqueue_watch_snapshot(
                &pending,
                &target,
                dirty_drain::snapshot_watch_dirty(&target.folder_path),
            );
        }
    }
}

fn watch_roots(targets: &[WatchTarget]) -> Vec<String> {
    targets
        .iter()
        .map(|target| target.folder_path.clone())
        .collect()
}

fn mark_full_rescan_obligations_with<F>(roots: &[String], mut mark: F) -> usize
where
    F: FnMut(&str) -> bool,
{
    roots.iter().filter(|root| mark(root)).count()
}

fn mark_full_rescan_obligations(roots: &[String], reason: &str) {
    let marked = mark_full_rescan_obligations_with(roots, dirty_drain::mark_watch_force_full);
    tracing::warn!(
        reason = %reason,
        targets = roots.len(),
        persisted = marked,
        "watch: filesystem notification fidelity lost; scheduled durable full rescans"
    );
}

/// Forward paths without blocking notify's callback thread. If even one path
/// cannot enter the bounded channel, exact event fidelity has been lost: stop
/// forwarding that event and durably require a covering scan for every active
/// root before returning.
fn forward_notify_paths_with<F>(
    paths: Vec<PathBuf>,
    tx: &tokio::sync::mpsc::Sender<PathBuf>,
    roots: &[String],
    mut mark: F,
) -> std::result::Result<(), usize>
where
    F: FnMut(&str) -> bool,
{
    for path in paths {
        if tx.try_send(path).is_err() {
            return Err(mark_full_rescan_obligations_with(roots, &mut mark));
        }
    }
    Ok(())
}

/// Run the long-lived watcher. Returns `Ok(())` when disabled, when another
/// watcher already holds the singleton lock, or on shutdown signal.
pub async fn run_watch() -> Result<()> {
    if !watch_enabled() {
        eprintln!("ContextStream watch is disabled (CONTEXTSTREAM_WATCH=0).");
        return Ok(());
    }

    let Some(lock) = acquire_singleton_lock() else {
        eprintln!("Another ContextStream watcher is already running on this machine.");
        return Ok(());
    };

    // Credentials: reuse the same resolution as the stdio server / hooks
    // (env -> saved `~/.contextstream/credentials.json`). `load_config`
    // already errors unless an API key, JWT, or header-auth is available; a
    // long-lived watcher should normally be backed by the saved API key.
    let config = match crate::config::load_config() {
        Ok(config) => config,
        Err(e) => {
            eprintln!(
                "ContextStream watch: no credentials ({e}). Run `contextstream-mcp setup` first."
            );
            return Ok(());
        }
    };
    let client = ContextStreamClient::new(config);

    let mut targets = enumerate_targets();
    lock.write_heartbeat(targets.len());
    eprintln!(
        "ContextStream watch started — monitoring {} mapped project(s). Edits re-ingest automatically (debounce {:?}).",
        targets.len(),
        DEBOUNCE
    );

    // Bridge the sync notify callback to the async loop. The callback keeps a
    // current root list so overflow, disconnect, and notify backend errors can
    // persist a repository-wide repair obligation even when no path fits in
    // the lossy in-memory channel.
    let (tx, mut rx) = tokio::sync::mpsc::channel::<PathBuf>(EVENT_CHANNEL_CAP);
    let notify_roots = Arc::new(Mutex::new(watch_roots(&targets)));
    let callback_roots = notify_roots.clone();
    let watcher_result = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let roots = callback_roots
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        match res {
            Ok(event) => {
                if let Err(marked) = forward_notify_paths_with(event.paths, &tx, &roots, |root| {
                    dirty_drain::mark_watch_force_full(root)
                }) {
                    tracing::warn!(
                        targets = roots.len(),
                        persisted = marked,
                        "watch: notify channel overflow/disconnect; durable full rescans required"
                    );
                }
            }
            Err(error) => {
                mark_full_rescan_obligations(&roots, &error.to_string());
            }
        }
    });
    let mut watcher = match watcher_result {
        Ok(watcher) => watcher,
        Err(error) => {
            let roots = notify_roots
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            mark_full_rescan_obligations(&roots, &error.to_string());
            return Err(error.into());
        }
    };

    let mut watched: HashSet<String> = HashSet::new();
    apply_watches(&mut watcher, &mut watched, &targets);

    let pending: Arc<Mutex<HashMap<WatchTargetKey, PendingProject>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let inflight: Arc<Mutex<HashSet<WatchTargetKey>>> = Arc::new(Mutex::new(HashSet::new()));
    let reconcile_running = Arc::new(AtomicBool::new(false));

    // Restore durable 202/attestation-only receipts before the normal debounce
    // loop starts. The timeout bounds startup; unresolved receipts remain on
    // disk and the periodic reconciler resumes them.
    let _ = tokio::time::timeout(
        Duration::from_secs(10),
        reconcile_watch_targets(client.clone(), targets.clone(), pending.clone()),
    )
    .await;

    let mut flush_tick = tokio::time::interval(FLUSH_TICK);
    flush_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut job_reconcile_tick = tokio::time::interval(JOB_RECONCILE_INTERVAL);
    job_reconcile_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    job_reconcile_tick.reset();
    let mut reenumerate_tick = tokio::time::interval(REENUMERATE_INTERVAL);
    reenumerate_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut lock_tick = tokio::time::interval(LOCK_REFRESH_INTERVAL);
    lock_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut bridge_heartbeat_tick = tokio::time::interval(BRIDGE_CONTROL_HEARTBEAT_INTERVAL);
    bridge_heartbeat_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut bridge_claim_tick = tokio::time::interval(BRIDGE_CONTROL_CLAIM_INTERVAL);
    bridge_claim_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let bridge_control_supported = Arc::new(AtomicBool::new(true));
    let bridge_heartbeat_running = Arc::new(AtomicBool::new(false));
    let bridge_claim_running = Arc::new(AtomicBool::new(false));
    let bridge_instance_id = lock.owner_id;

    loop {
        tokio::select! {
            maybe_path = rx.recv() => {
                let Some(path) = maybe_path else { break; };
                if path_has_ignored_component(&path) {
                    continue;
                }
                if let Some(target) = resolve_target_for_path(&path, &targets) {
                    let path_string = path.to_string_lossy().into_owned();
                    let requires_full_repair = dirty_drain::record_dirty_paths(
                        &target.folder_path,
                        std::slice::from_ref(&path_string),
                    )
                    .is_empty();
                    if requires_full_repair {
                        let persisted = dirty_drain::mark_watch_force_full(&target.folder_path);
                        tracing::warn!(
                            project_id = %target.project_id,
                            path = %path.display(),
                            persisted,
                            "watch: could not retain an exact dirty version; requiring a full rescan"
                        );
                    }
                    let deadline = Instant::now() + DEBOUNCE;
                    let mut guard = pending.lock().unwrap_or_else(|e| e.into_inner());
                    let entry = guard.entry(watch_target_key(target)).or_insert_with(|| PendingProject {
                        target: target.clone(),
                        changed: HashSet::new(),
                        deadline,
                        force_full: false,
                    });
                    entry.target = target.clone();
                    entry.deadline = deadline;
                    entry.force_full |= requires_full_repair;
                    // Bound memory if a single window goes wild; the bulk-scan
                    // fallback (changed.len() > WATCH_TARGETED_MAX) covers it.
                    if entry.changed.len() <= WATCH_TARGETED_MAX {
                        entry.changed.insert(path);
                        entry.force_full |= entry.changed.len() > WATCH_TARGETED_MAX;
                    } else {
                        entry.force_full = true;
                    }
                }
            }
            _ = flush_tick.tick() => {
                if let Some(dir) = contextstream_dir() {
                    if consume_sync_bridge_stop_request_at(&dir, &lock) {
                        tracing::info!("watch: received managed graceful stop request");
                        break;
                    }
                    if consume_sync_bridge_reload_request_at(&dir) {
                        targets = enumerate_targets();
                        *notify_roots
                            .lock()
                            .unwrap_or_else(|error| error.into_inner()) = watch_roots(&targets);
                        apply_watches(&mut watcher, &mut watched, &targets);
                        lock.write_heartbeat(targets.len());
                        tracing::info!(
                            targets = targets.len(),
                            "watch: reloaded managed checkout mappings"
                        );
                    }
                }
                let now = Instant::now();
                let due = take_due_watch_projects(&pending, &inflight, now);

                for (target, changed, force_full) in due {
                    let key = watch_target_key(&target);
                    inflight.lock().unwrap_or_else(|e| e.into_inner()).insert(key.clone());
                    let client = client.clone();
                    let inflight = inflight.clone();
                    let pending = pending.clone();
                    tokio::spawn(async move {
                        if let Some(retry) =
                            flush_project(client, &target, changed, force_full).await
                        {
                            enqueue_watch_retry(&pending, &target, retry);
                        }
                        inflight.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
                    });
                }
            }
            _ = job_reconcile_tick.tick() => {
                if reconcile_running
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    let client = client.clone();
                    let targets = targets.clone();
                    let pending = pending.clone();
                    let running = reconcile_running.clone();
                    tokio::spawn(async move {
                        reconcile_watch_targets(client, targets, pending).await;
                        running.store(false, Ordering::Release);
                    });
                }
            }
            _ = reenumerate_tick.tick() => {
                targets = enumerate_targets();
                *notify_roots
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()) = watch_roots(&targets);
                apply_watches(&mut watcher, &mut watched, &targets);
                lock.write_heartbeat(targets.len());
            }
            _ = lock_tick.tick() => {
                lock.write_heartbeat(targets.len());
            }
            _ = bridge_heartbeat_tick.tick() => {
                if bridge_control_supported.load(Ordering::Acquire)
                    && bridge_heartbeat_running
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                {
                    let client = client.clone();
                    let targets = targets.clone();
                    let supported = bridge_control_supported.clone();
                    let running = bridge_heartbeat_running.clone();
                    tokio::spawn(async move {
                        publish_bridge_control_heartbeat(
                            client,
                            bridge_instance_id,
                            targets,
                            supported,
                        )
                        .await;
                        running.store(false, Ordering::Release);
                    });
                }
            }
            _ = bridge_claim_tick.tick() => {
                if bridge_control_supported.load(Ordering::Acquire)
                    && bridge_claim_running
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok()
                {
                    let client = client.clone();
                    let targets = targets.clone();
                    let supported = bridge_control_supported.clone();
                    let running = bridge_claim_running.clone();
                    tokio::spawn(async move {
                        claim_and_process_hosted_refreshes(
                            client,
                            bridge_instance_id,
                            targets,
                            supported,
                        )
                        .await;
                        running.store(false, Ordering::Release);
                    });
                }
            }
        }
    }

    drop(lock);
    Ok(())
}

/// Reconcile the set of actively-watched folders with the current targets.
fn apply_watches<W: notify::Watcher>(
    watcher: &mut W,
    watched: &mut HashSet<String>,
    targets: &[WatchTarget],
) {
    use notify::RecursiveMode;

    let desired: HashSet<String> = targets.iter().map(|t| t.folder_path.clone()).collect();

    // Add new folders.
    for folder in &desired {
        if watched.contains(folder) {
            continue;
        }
        match watcher.watch(Path::new(folder), RecursiveMode::Recursive) {
            Ok(()) => {
                watched.insert(folder.clone());
                tracing::debug!("watch: now watching {}", folder);
            }
            Err(e) => {
                let _ = dirty_drain::mark_watch_force_full(folder);
                tracing::debug!("watch: failed to watch {}: {}", folder, e);
            }
        }
    }

    // Drop folders that are no longer mapped or have disappeared.
    let stale: Vec<String> = watched.difference(&desired).cloned().collect();
    for folder in stale {
        let _ = watcher.unwatch(Path::new(&folder));
        watched.remove(&folder);
        tracing::debug!("watch: stopped watching {}", folder);
    }
}

// ===========================================================================
// Launch helper
// ===========================================================================

/// Best-effort: launch a detached `contextstream-mcp watch` singleton.
///
/// Called from the setup flow so that editors *without* lifecycle hooks
/// (Codex, Kilo, Antigravity, ...) still get continuous content freshness.
/// Safe to call repeatedly — the singleton lock dedupes, and a watcher
/// already holding the lock means we do nothing.
pub fn spawn_watch_helper() {
    if !watch_enabled() {
        return;
    }

    // Avoid a redundant helper when the kernel says a watcher owns the lock.
    // This is only an optimization: run_watch repeats the authoritative
    // lifetime-held acquisition, closing the probe/spawn TOCTOU window.
    if let Some(dir) = contextstream_dir() {
        if singleton_lock_is_held_at(&dir) {
            return;
        }
    }

    let binary = watch_helper_binary();
    let mut command = std::process::Command::new(binary);
    command
        .arg("watch")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());

    if let Err(e) = command.spawn() {
        tracing::debug!("watch: failed to launch background watcher: {}", e);
    }
}

/// Resolve the binary used to launch the watcher: prefer the stable managed
/// helper path, fall back to the currently running executable.
fn watch_helper_binary() -> PathBuf {
    let managed = crate::setup::managed_binary_path();
    if managed.exists() {
        return managed;
    }
    std::env::current_exe().unwrap_or(managed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    fn ws() -> Uuid {
        Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap()
    }
    fn pid_a() -> Uuid {
        Uuid::parse_str("22222222-2222-4222-8222-222222222222").unwrap()
    }
    fn pid_b() -> Uuid {
        Uuid::parse_str("33333333-3333-4333-8333-333333333333").unwrap()
    }

    #[test]
    fn singleton_lock_is_exclusive_for_guard_lifetime_and_heartbeat_is_separate() {
        let directory = tempdir().expect("tempdir");
        let first = acquire_singleton_lock_at(directory.path()).expect("first lock");

        assert!(singleton_lock_is_held_at(directory.path()));
        assert!(
            acquire_singleton_lock_at(directory.path()).is_none(),
            "a second watcher must not acquire the same machine lock"
        );
        assert_eq!(
            std::fs::read_to_string(directory.path().join(WATCH_LOCK_FILE)).unwrap(),
            "",
            "heartbeat metadata must never be written into the locked inode"
        );
        let heartbeat = read_json_file(&directory.path().join(WATCH_HEARTBEAT_FILE))
            .expect("heartbeat metadata");
        assert_eq!(
            heartbeat.get("pid").and_then(Value::as_u64),
            Some(std::process::id() as u64)
        );
        assert_eq!(
            heartbeat
                .get("owner_id")
                .and_then(Value::as_str)
                .and_then(|raw| Uuid::parse_str(raw).ok()),
            Some(first.owner_id)
        );
        assert_eq!(
            heartbeat.get("role").and_then(Value::as_str),
            Some(WATCH_HEARTBEAT_ROLE)
        );
        assert_eq!(
            heartbeat.get("target_count").and_then(Value::as_u64),
            Some(0)
        );

        drop(first);
        assert!(!directory.path().join(WATCH_HEARTBEAT_FILE).exists());
        assert!(!singleton_lock_is_held_at(directory.path()));
        assert!(acquire_singleton_lock_at(directory.path()).is_some());
    }

    #[test]
    fn graceful_stop_is_bound_to_the_exact_lock_owner() {
        let directory = tempdir().expect("tempdir");
        let first = acquire_singleton_lock_at(directory.path()).expect("first lock");
        assert!(request_sync_bridge_stop_at(directory.path()).expect("request stop"));
        assert!(consume_sync_bridge_stop_request_at(
            directory.path(),
            &first
        ));
        assert!(!directory.path().join(WATCH_STOP_REQUEST_FILE).exists());

        let stale = WatchStopRequest {
            schema_version: WATCH_HEARTBEAT_SCHEMA_VERSION,
            role: WATCH_HEARTBEAT_ROLE.to_string(),
            pid: first.pid,
            owner_id: Uuid::new_v4(),
            requested_at: chrono::Utc::now().to_rfc3339(),
        };
        let mut raw = serde_json::to_string(&stale).unwrap();
        raw.push('\n');
        crate::setup::safe_edit::write_owned_file_if_changed(
            &directory.path().join(WATCH_STOP_REQUEST_FILE),
            &raw,
        )
        .unwrap();
        assert!(!consume_sync_bridge_stop_request_at(
            directory.path(),
            &first
        ));
        assert!(directory.path().join(WATCH_STOP_REQUEST_FILE).exists());
    }

    #[test]
    fn reload_request_is_one_shot_and_contains_no_scope_data() {
        let directory = tempdir().expect("tempdir");
        let path = directory.path().join(WATCH_RELOAD_REQUEST_FILE);
        crate::setup::safe_edit::write_owned_file_if_changed(&path, "reload\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "reload\n");
        assert!(consume_sync_bridge_reload_request_at(directory.path()));
        assert!(!consume_sync_bridge_reload_request_at(directory.path()));
    }

    #[test]
    fn bridge_health_is_fresh_and_privacy_bounded_while_the_lock_is_held() {
        let directory = tempdir().expect("tempdir");
        let lock = acquire_singleton_lock_at(directory.path()).expect("lock");
        lock.write_heartbeat(3);

        let health = sync_bridge_health_at(directory.path(), chrono::Utc::now());
        assert_eq!(health.state, SyncBridgeHealthState::Running);
        assert_eq!(health.lock_held, Some(true));
        assert!(health.heartbeat_fresh);
        assert_eq!(health.target_count, 3);
        assert_eq!(health.role, WATCH_HEARTBEAT_ROLE);
        assert_eq!(health.version.as_deref(), Some(VERSION));
        let serialized = serde_json::to_string(&health).unwrap();
        assert!(!serialized.contains(directory.path().to_string_lossy().as_ref()));
        assert!(!serialized.contains(&lock.owner_id.to_string()));
    }

    #[test]
    fn stale_heartbeat_is_degraded_and_health_probe_never_creates_state() {
        let directory = tempdir().expect("tempdir");
        let state_dir = directory.path().join("not-created");
        let stopped = sync_bridge_health_at(&state_dir, chrono::Utc::now());
        assert_eq!(stopped.state, SyncBridgeHealthState::Stopped);
        assert!(
            !state_dir.exists(),
            "doctor-style health probes are read-only"
        );

        let lock = acquire_singleton_lock_at(directory.path()).expect("lock");
        let heartbeat = WatchHeartbeat {
            schema_version: WATCH_HEARTBEAT_SCHEMA_VERSION,
            role: WATCH_HEARTBEAT_ROLE.to_string(),
            version: VERSION.to_string(),
            pid: lock.pid,
            owner_id: lock.owner_id,
            target_count: 2,
            refreshed_at: (chrono::Utc::now() - chrono::Duration::minutes(10)).to_rfc3339(),
        };
        std::fs::write(
            directory.path().join(WATCH_HEARTBEAT_FILE),
            serde_json::to_string(&heartbeat).unwrap(),
        )
        .unwrap();

        let degraded = sync_bridge_health_at(directory.path(), chrono::Utc::now());
        assert_eq!(degraded.state, SyncBridgeHealthState::Degraded);
        assert_eq!(degraded.lock_held, Some(true));
        assert!(!degraded.heartbeat_fresh);
        assert_eq!(degraded.target_count, 2);
    }

    #[test]
    fn hosted_refresh_claim_requires_exact_checkout_project_and_workspace() {
        let target = WatchTarget {
            folder_path: "/repo/worktree-a".to_string(),
            project_id: pid_a(),
            workspace_id: Some(ws()),
        };
        let authorized = vec![AuthorizedBridgeTarget {
            target: target.clone(),
            registration: SyncBridgeCheckoutRegistration {
                checkout_id: "checkout-v1:44444444-4444-4444-8444-444444444444".to_string(),
                checkout_locator:
                    "checkout-locator-v1:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                        .to_string(),
                project_id: pid_a(),
                workspace_id: ws(),
                repository_fingerprint: "git-common-dir-v1:55555555-5555-4555-8555-555555555555"
                    .to_string(),
                repository_identity: Some("git-remote-v1:github.com/acme/mcp".to_string()),
                git_ref_kind: "branch".to_string(),
                git_branch: Some("main".to_string()),
                git_commit_sha: Some("0123456789abcdef".to_string()),
                git_commit_timestamp: Some("2026-07-29T00:00:00Z".to_string()),
                is_default_branch: Some(true),
                dirty: Some(false),
                captured_at: "2026-07-29T00:00:01Z".to_string(),
            },
        }];
        let claim = SyncBridgeRefreshClaim {
            request_id: "request-1".to_string(),
            checkout_id: authorized[0].registration.checkout_id.clone(),
            project_id: pid_a(),
            workspace_id: ws(),
            lease_token: "lease-1".to_string(),
            force: true,
            reason: Some("project.index".to_string()),
            expires_at: None,
        };
        assert_eq!(target_for_refresh_claim(&claim, &authorized), Some(&target));

        let mut wrong_project = claim.clone();
        wrong_project.project_id = pid_b();
        assert!(target_for_refresh_claim(&wrong_project, &authorized).is_none());
        let mut wrong_workspace = claim.clone();
        wrong_workspace.workspace_id = Uuid::new_v4();
        assert!(target_for_refresh_claim(&wrong_workspace, &authorized).is_none());
        let mut empty_lease = claim;
        empty_lease.lease_token.clear();
        assert!(target_for_refresh_claim(&empty_lease, &authorized).is_none());
    }

    #[test]
    fn hosted_refresh_ack_summary_is_path_free_and_bounded() {
        let pending_job = Uuid::new_v4();
        let result = serde_json::json!({
            "committed": false,
            "scan_complete": true,
            "files_uploaded": 4,
            "files_indexed": 3,
            "files_skipped": 1,
            "files_failed": 0,
            "pending_jobs": 1,
            "job_ids": [
                pending_job,
                "not-a-job-uuid",
                "/home/alice/private/repo/job-id"
            ],
            "server_processing_status": "in_progress",
            "progress_urls": ["/api/v1/projects/private/ingest/jobs/secret"],
            "local_root": "/home/alice/private/repo",
            "errors": [{"path": "/home/alice/private/repo/secret.rs"}],
        });
        let summary = bridge_ingest_summary(&result);
        assert_eq!(summary["schema_version"], 1);
        assert_eq!(summary["committed"], false);
        assert_eq!(summary["files_indexed"], 3);
        assert_eq!(summary["job_ids"], serde_json::json!([pending_job]));
        assert_eq!(summary["server_processing_status"], "in_progress");
        let serialized = summary.to_string();
        assert!(!serialized.contains("/home/alice"));
        assert!(!serialized.contains("progress_urls"));
        assert!(summary.get("errors").is_none());
    }

    #[test]
    fn hosted_refresh_ack_detail_distinguishes_each_nonterminal_state() {
        let completed = serde_json::json!({
            "committed": true,
            "scan_complete": true,
            "pending_jobs": 0,
            "job_ids": [Uuid::new_v4()],
        });
        let completed_summary = bridge_ingest_summary(&completed);
        assert_eq!(
            hosted_refresh_ack_detail(&completed, &completed_summary),
            None
        );

        let incomplete = serde_json::json!({
            "committed": true,
            "scan_complete": false,
            "pending_jobs": 0,
        });
        let incomplete_summary = bridge_ingest_summary(&incomplete);
        assert_eq!(
            hosted_refresh_ack_detail(&incomplete, &incomplete_summary),
            Some("scan_incomplete")
        );

        let pending_job = serde_json::json!({
            "committed": false,
            "scan_complete": true,
            "pending_jobs": 1,
            "job_ids": [Uuid::new_v4()],
        });
        let pending_summary = bridge_ingest_summary(&pending_job);
        assert_eq!(
            hosted_refresh_ack_detail(&pending_job, &pending_summary),
            Some("server_jobs_pending")
        );

        let unconfirmed = serde_json::json!({
            "committed": false,
            "scan_complete": true,
            "pending_jobs": 0,
            "job_ids": [],
        });
        let unconfirmed_summary = bridge_ingest_summary(&unconfirmed);
        assert_eq!(
            hosted_refresh_ack_detail(&unconfirmed, &unconfirmed_summary),
            Some("commit_unconfirmed")
        );
    }

    #[test]
    fn hosted_refresh_closes_the_full_checkout_scan_epoch() {
        let target = WatchTarget {
            folder_path: "/repo/worktree-a".to_string(),
            project_id: pid_a(),
            workspace_id: Some(ws()),
        };
        let params = hosted_refresh_ingest_params(&target, true);

        assert_eq!(params.path, target.folder_path);
        assert_eq!(params.project_id, Some(target.project_id));
        assert_eq!(params.workspace_id, target.workspace_id);
        assert_eq!(params.force, Some(true));
        assert_eq!(params.max_files, None);
        assert_eq!(params.background, Some(true));
        assert_eq!(params.origin.as_deref(), Some("hosted_refresh"));
        assert_eq!(params.reroot, None);
    }

    #[test]
    fn notify_channel_overflow_marks_every_active_root_for_full_rescan() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        tx.try_send(PathBuf::from("already-queued.rs")).unwrap();
        let roots = vec!["/repo/a".to_string(), "/repo/b".to_string()];
        let mut marked_roots = Vec::new();

        let persisted =
            forward_notify_paths_with(vec![PathBuf::from("overflowed.rs")], &tx, &roots, |root| {
                marked_roots.push(root.to_string());
                true
            })
            .expect_err("a saturated channel must require durable full rescans");

        assert_eq!(persisted, roots.len());
        assert_eq!(marked_roots, roots);
    }

    #[test]
    fn root_only_startup_obligation_reaches_full_flush_queue_without_paths() {
        let target = WatchTarget {
            folder_path: "/repo/root-only".to_string(),
            project_id: pid_a(),
            workspace_id: Some(ws()),
        };
        let pending = Arc::new(Mutex::new(HashMap::new()));
        enqueue_watch_snapshot(
            &pending,
            &target,
            dirty_drain::WatchDirtySnapshot {
                path_versions: Default::default(),
                root_version: Some(Uuid::new_v4().to_string()),
                force_full: true,
            },
        );
        pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get_mut(&watch_target_key(&target))
            .expect("root-only pending project")
            .deadline = Instant::now();

        let due = take_due_watch_projects(
            &pending,
            &Arc::new(Mutex::new(HashSet::new())),
            Instant::now(),
        );
        assert_eq!(due.len(), 1);
        let (due_target, changed, force_full) = &due[0];
        assert_eq!(due_target, &target);
        assert!(changed.is_empty());
        assert!(*force_full);
        assert_eq!(
            watch_submission_mode(*force_full, false, changed.len()),
            PendingSubmissionMode::Full
        );
    }

    #[test]
    fn debounce_and_inflight_state_are_isolated_per_checkout_root() {
        let first = WatchTarget {
            folder_path: "/repo/worktree-a".to_string(),
            project_id: pid_a(),
            workspace_id: Some(ws()),
        };
        let second = WatchTarget {
            folder_path: "/repo/worktree-b".to_string(),
            project_id: pid_a(),
            workspace_id: Some(ws()),
        };
        let pending = Arc::new(Mutex::new(HashMap::new()));
        for target in [&first, &second] {
            enqueue_watch_retry(
                &pending,
                target,
                WatchSubmissionRetry {
                    paths: vec![format!("{}/src/lib.rs", target.folder_path)],
                    mode: PendingSubmissionMode::Targeted,
                },
            );
            pending
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get_mut(&watch_target_key(target))
                .expect("pending checkout")
                .deadline = Instant::now();
        }

        let inflight = Arc::new(Mutex::new(HashSet::from([watch_target_key(&first)])));
        let due = take_due_watch_projects(&pending, &inflight, Instant::now());
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].0, second);
        assert!(
            pending
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .contains_key(&watch_target_key(&first)),
            "an in-flight worktree must retain only its own obligation"
        );
    }

    #[test]
    fn collect_targets_unions_registries_and_dedupes_by_checkout_root() {
        let indexed = json!({
            "projects": {
                "/home/u/proj-a": { "project_id": pid_a().to_string() },
                "/home/u/proj-b": { "project_id": pid_b().to_string() },
            }
        });
        // mappings repeats proj-a (carrying workspace_id) and proj-b.
        let mappings = json!({
            "mappings": [
                { "path": "/home/u/proj-a", "project_id": pid_a().to_string(), "workspace_id": ws().to_string() },
                { "path": "/home/u/proj-b", "project_id": pid_b().to_string() },
            ]
        });

        let targets = collect_watch_targets(Some(&indexed), Some(&mappings), |_| true);
        assert_eq!(targets.len(), 2, "deduped by checkout root");
        let a = targets.iter().find(|t| t.project_id == pid_a()).unwrap();
        // workspace_id backfilled from mappings even though indexed lacked it.
        assert_eq!(a.workspace_id, Some(ws()));
    }

    #[test]
    fn collect_targets_skips_missing_dirs_and_entries_without_project_id() {
        let indexed = json!({
            "projects": {
                "/home/u/gone": { "project_id": pid_a().to_string() },
                "/home/u/keep": { "project_id": pid_b().to_string() },
                "/home/u/no-id": { "indexed_at": "2026-01-01T00:00:00Z" },
            }
        });
        let targets = collect_watch_targets(Some(&indexed), None, |p| p == "/home/u/keep");
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].folder_path, "/home/u/keep");
    }

    #[test]
    fn collect_targets_keeps_every_root_for_one_project() {
        let indexed = json!({
            "projects": {
                "/home/u/clone-a": { "project_id": pid_a().to_string() },
                "/home/u/clone-b": { "project_id": pid_a().to_string() },
            }
        });
        let targets = collect_watch_targets(Some(&indexed), None, |_| true);
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].project_id, pid_a());
        assert_eq!(targets[1].project_id, pid_a());
        assert_ne!(watch_target_key(&targets[0]), watch_target_key(&targets[1]));
    }

    #[test]
    fn watcher_requires_checkout_bound_config_that_agrees_with_registry() {
        let temp = tempdir().expect("tempdir");
        std::fs::create_dir_all(temp.path().join(".git")).unwrap();
        let fingerprint =
            mcp_session::checkout_identity::ensure_repository_fingerprint(temp.path()).unwrap();
        let config_dir = temp.path().join(".contextstream");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        let target = WatchTarget {
            folder_path: temp.path().to_string_lossy().into_owned(),
            project_id: pid_a(),
            workspace_id: Some(ws()),
        };
        let write_config = |project_id: Uuid, checkout_root: Option<String>| {
            std::fs::write(
                config_dir.join("config.json"),
                serde_json::to_string(&json!({
                    "project_id": project_id.to_string(),
                    "workspace_id": ws().to_string(),
                    "checkout_root": checkout_root,
                    "repository_fingerprint": fingerprint.as_str(),
                }))
                .unwrap(),
            )
            .unwrap();
        };

        write_config(
            pid_a(),
            Some(crate::setup::canonical_checkout_root(temp.path())),
        );
        assert!(watch_target_matches_checkout_config(&target));

        write_config(
            pid_b(),
            Some(crate::setup::canonical_checkout_root(temp.path())),
        );
        assert!(!watch_target_matches_checkout_config(&target));

        write_config(pid_a(), None);
        assert!(!watch_target_matches_checkout_config(&target));

        std::fs::write(
            config_dir.join("config.json"),
            serde_json::to_string(&json!({
                "project_id": pid_a().to_string(),
                "checkout_root": crate::setup::canonical_checkout_root(temp.path()),
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(
            !watch_target_matches_checkout_config(&target),
            "missing workspace ownership must fail closed"
        );

        write_config(
            pid_a(),
            Some(crate::setup::canonical_checkout_root(temp.path())),
        );
        let config_path = config_dir.join("config.json");
        let mut legacy: Value =
            serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        legacy
            .as_object_mut()
            .unwrap()
            .remove("repository_fingerprint");
        std::fs::write(&config_path, serde_json::to_string(&legacy).unwrap()).unwrap();
        assert!(
            !watch_target_matches_checkout_config(&target),
            "legacy configs without repository identity must fail closed"
        );

        write_config(
            pid_a(),
            Some(crate::setup::canonical_checkout_root(temp.path())),
        );
        std::fs::remove_file(temp.path().join(".git/contextstream/repository-id")).unwrap();
        let replacement =
            mcp_session::checkout_identity::ensure_repository_fingerprint(temp.path()).unwrap();
        assert_ne!(replacement, fingerprint);
        assert!(
            !watch_target_matches_checkout_config(&target),
            "same-path repository replacement must fail closed"
        );
    }

    #[test]
    fn resolve_target_prefers_longest_prefix_for_nested_projects() {
        let targets = vec![
            WatchTarget {
                folder_path: "/home/u/proj".to_string(),
                project_id: pid_a(),
                workspace_id: None,
            },
            WatchTarget {
                folder_path: "/home/u/proj/sub".to_string(),
                project_id: pid_b(),
                workspace_id: None,
            },
        ];

        let outer = resolve_target_for_path(Path::new("/home/u/proj/src/main.rs"), &targets);
        assert_eq!(outer.map(|t| t.project_id), Some(pid_a()));

        let inner = resolve_target_for_path(Path::new("/home/u/proj/sub/lib.rs"), &targets);
        assert_eq!(inner.map(|t| t.project_id), Some(pid_b()));

        let unmapped = resolve_target_for_path(Path::new("/somewhere/else.rs"), &targets);
        assert!(unmapped.is_none());
    }

    #[test]
    fn ignored_components_are_filtered() {
        for path in [
            "/home/u/proj/target/debug/x.rs",
            "/home/u/proj/node_modules/pkg/index.js",
            "/home/u/proj/.git/HEAD",
            "/home/u/proj/.dev-data/droid/session.json",
            "/home/u/proj/.caches/npm/package.json",
            "/home/u/proj/.claude-worktrees/task/src/lib.rs",
        ] {
            assert!(
                path_has_ignored_component(Path::new(path)),
                "{path} should not wake the watcher"
            );
        }
        for path in [
            "/home/u/proj/src/main.rs",
            "/home/u/proj/src/bin/cli.rs",
            "/home/u/proj/.github/workflows/ci.yml",
            "/home/u/proj/.agents/reviewer.md",
        ] {
            assert!(
                !path_has_ignored_component(Path::new(path)),
                "{path} should wake the watcher"
            );
        }
    }

    #[test]
    fn watch_ingest_params_use_non_billed_watch_lane() {
        let target = WatchTarget {
            folder_path: "/home/u/proj".to_string(),
            project_id: pid_a(),
            workspace_id: Some(ws()),
        };
        let params = watch_ingest_params(&target);
        assert_eq!(params.background, Some(true));
        assert_eq!(params.origin.as_deref(), Some("watch"));
        assert_eq!(params.project_id, Some(pid_a()));
        assert_eq!(params.workspace_id, Some(ws()));
        assert_eq!(params.max_files, Some(WATCH_MAX_FILES));
    }

    #[test]
    fn watch_enabled_respects_opt_out() {
        let _guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("CONTEXTSTREAM_WATCH").ok();

        std::env::set_var("CONTEXTSTREAM_WATCH", "0");
        assert!(!watch_enabled());
        std::env::set_var("CONTEXTSTREAM_WATCH", "off");
        assert!(!watch_enabled());
        std::env::set_var("CONTEXTSTREAM_WATCH", "1");
        assert!(watch_enabled());
        std::env::remove_var("CONTEXTSTREAM_WATCH");
        assert!(watch_enabled());

        match prev {
            Some(v) => std::env::set_var("CONTEXTSTREAM_WATCH", v),
            None => std::env::remove_var("CONTEXTSTREAM_WATCH"),
        }
    }

    #[test]
    fn targeted_delta_uses_shared_upload_ignore_and_reject_policy() {
        let dir = tempfile::tempdir().unwrap();
        let keep = dir.path().join("keep.rs");
        let ignored = dir.path().join("ignored.rs");
        let secret = dir.path().join(".mcp.json");
        let invalid = dir.path().join("invalid.rs");
        std::fs::write(&keep, "fn keep() {}\n").unwrap();
        std::fs::write(&ignored, "fn secret() {}\n").unwrap();
        std::fs::write(&secret, r#"{"headers":{"Authorization":"secret"}}"#).unwrap();
        std::fs::write(&invalid, [0xff, 0xfe]).unwrap();
        std::fs::write(dir.path().join(".gitignore"), "ignored.rs\n").unwrap();

        let changed = HashSet::from([
            keep.clone(),
            ignored.clone(),
            secret.clone(),
            invalid.clone(),
        ]);
        let mut delta = targeted_watch_delta(dir.path(), &changed);

        assert_eq!(delta.files.len(), 1);
        assert_eq!(delta.files[0]["path"], "keep.rs");
        delta.deleted_paths.sort();
        assert_eq!(
            delta.deleted_paths,
            vec![
                ".mcp.json".to_string(),
                "ignored.rs".to_string(),
                "invalid.rs".to_string(),
            ]
        );
        let mut expected_completed = vec![
            ignored.to_string_lossy().into_owned(),
            invalid.to_string_lossy().into_owned(),
            keep.to_string_lossy().into_owned(),
            secret.to_string_lossy().into_owned(),
        ];
        expected_completed.sort();
        assert_eq!(delta.completed_paths, expected_completed);
        assert_eq!(
            delta.rejected, 0,
            "safe contained invalid UTF-8 is tombstoned to avoid a watcher hot loop"
        );
    }

    #[test]
    fn targeted_delta_sends_safe_missing_file_as_deletion() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("removed.rs");
        let changed = HashSet::from([missing.clone()]);

        let delta = targeted_watch_delta(dir.path(), &changed);

        assert!(delta.files.is_empty());
        assert_eq!(delta.deleted_paths, vec!["removed.rs".to_string()]);
        assert_eq!(
            delta.completed_paths,
            vec![missing.to_string_lossy().into_owned()]
        );
        assert_eq!(delta.rejected, 0);
    }

    #[cfg(unix)]
    #[test]
    fn targeted_delta_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let outside_file = outside.path().join("outside.rs");
        std::fs::write(&outside_file, "fn outside() {}\n").unwrap();
        let escaped = dir.path().join("escaped.rs");
        symlink(&outside_file, &escaped).unwrap();

        let delta = targeted_watch_delta(dir.path(), &HashSet::from([escaped]));

        assert!(delta.files.is_empty());
        assert!(delta.deleted_paths.is_empty());
        assert!(delta.completed_paths.is_empty());
        assert_eq!(delta.rejected, 1);
    }
}
