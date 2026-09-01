//! Shared proactive index-maintenance service.
//!
//! `IndexKeeper` encapsulates all fire-and-forget index freshness logic
//! (incremental changed-file indexing, stale/aging re-ingestion) behind a
//! single `tick()` call that any tool handler can invoke.  Throttle state is
//! shared across all callers via `Arc<IndexKeeper>`, so concurrent tool
//! invocations never double-trigger expensive re-index operations.

use mcp_client::{ContextStreamClient, IngestLocalParams};
use mcp_session::SessionManager;
use mcp_types::{
    acceleration_layer::{AccelerationLayer, AccelerationSignalKind},
    atlas_layer::{AtlasLayer, AtlasStreamEventKind},
};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use uuid::Uuid;

/// Resolve the workspace authorized by the checkout-local binding and reject
/// any conflicting session/registry hint. Automatic maintenance is a content
/// write, so a project UUID by itself is never sufficient authority.
fn bound_workspace_for_content(
    folder_path: &str,
    hinted_workspace_id: Option<Uuid>,
    project_id: Uuid,
    operation: &str,
) -> Option<Uuid> {
    let bound_workspace_id =
        mcp_session::auto_init::checkout_binding_workspace(folder_path, project_id)?;
    if hinted_workspace_id.is_some_and(|hinted| hinted != bound_workspace_id) {
        tracing::warn!(
            operation,
            path = %folder_path,
            project_id = %project_id,
            hinted_workspace_id = ?hinted_workspace_id,
            bound_workspace_id = %bound_workspace_id,
            "index maintenance skipped because local scope sources disagree"
        );
        return None;
    }
    Some(bound_workspace_id)
}

/// Revalidate the bound project through the API immediately before an
/// automatic upload. Missing ownership is treated as unverifiable and fails
/// closed; network/5xx errors never fall back to stale local metadata.
async fn server_project_matches_bound_workspace(
    client: &ContextStreamClient,
    workspace_id: Uuid,
    project_id: Uuid,
    folder_path: &str,
    operation: &str,
) -> bool {
    match client.get_project_fresh(project_id).await {
        Ok(project) if project.workspace_id == Some(workspace_id) => true,
        Ok(project) => {
            tracing::warn!(
                operation,
                path = %folder_path,
                project_id = %project_id,
                expected_workspace_id = %workspace_id,
                actual_workspace_id = ?project.workspace_id,
                "index maintenance skipped because server project ownership is missing or mismatched"
            );
            false
        }
        Err(error) => {
            tracing::warn!(
                operation,
                path = %folder_path,
                project_id = %project_id,
                error = %error,
                "index maintenance skipped because server project validation failed"
            );
            false
        }
    }
}

/// Minimum interval between incremental changed-file checks.
const INCREMENTAL_INTERVAL_SECS: u64 = 10;

/// Maximum files per incremental index check.
const INCREMENTAL_MAX_FILES: usize = 500;

/// Minimum interval between stale re-ingest attempts.
const STALE_REINGEST_INTERVAL_SECS: u64 = 60;

/// Minimum interval between active-project hot refresh checks.
const ACTIVE_REFRESH_INTERVAL_SECS: u64 = 15;

/// Minimum interval between cross-machine root repair attempts.
const SCOPE_REPAIR_REINGEST_INTERVAL_SECS: u64 = 60;

/// Minimum interval between aging refresh attempts.
const AGING_REFRESH_INTERVAL_SECS: u64 = 300;

/// Minimum interval between local registry hygiene passes.
const REGISTRY_PRUNE_INTERVAL_SECS: u64 = 300;

/// Minimum interval between duplicate-project merge attempts.
const DUPLICATE_MERGE_INTERVAL_SECS: u64 = 300;

/// Seconds after which the active project is no longer considered hot.
///
/// Search preflight uses the same default and blocks briefly to repair before
/// rendering. The keep-warm daemon uses a slightly looser threshold for
/// inactive projects.
const ACTIVE_HOT_THRESHOLD_SECS_DEFAULT: i64 = 120;

/// Seconds after which an inactive mapped project is proactively refreshed.
const KEEP_WARM_AGING_THRESHOLD_SECS_DEFAULT: i64 = 300;

/// Hours after which the index is considered "stale". Aligned with the
/// search-side `INDEX_STALE_HOURS` so the keeper and search agree on the
/// stale/aging boundary used for logging and reason labels.
const STALE_THRESHOLD_HOURS: i64 = 48;

/// Maximum files for an aging-refresh ingest.
const AGING_REFRESH_MAX_FILES: usize = 20_000;

fn env_i64(name: &str, default: i64) -> i64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<i64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

/// P0 ingestion-containment guard for the background keep-warm / refresh /
/// reingest loops. Returns true only when `path` is a safe ingest *root*:
/// it rejects the filesystem root, `$HOME`, home ancestors, and sensitive
/// directories (`.ssh`/`.aws`/...). Honors the operator env opt-in
/// (`CONTEXTSTREAM_ALLOW_BROAD_INGEST=1`) like every other ingest entry point.
fn is_safe_background_ingest_root(path: &Path) -> bool {
    match mcp_client::validate_ingest_root(path, &mcp_client::IngestRootOptions::from_env()) {
        Ok(_) => true,
        Err(rejection) => {
            // Surface the first broad/sensitive-root skip once per process so a
            // previously-working root silently dropping out of keep-warm is
            // visible (and the opt-in is discoverable), without log spam.
            use std::sync::atomic::{AtomicBool, Ordering};
            static WARNED: AtomicBool = AtomicBool::new(false);
            if !WARNED.swap(true, Ordering::Relaxed) {
                tracing::warn!("keep-warm: {}", rejection);
            }
            false
        }
    }
}

pub(crate) fn active_hot_threshold_secs() -> i64 {
    env_i64(
        "CONTEXTSTREAM_ACTIVE_INDEX_HOT_SECS",
        ACTIVE_HOT_THRESHOLD_SECS_DEFAULT,
    )
}

fn keep_warm_aging_threshold_secs() -> i64 {
    env_i64(
        "CONTEXTSTREAM_KEEP_WARM_AGING_SECS",
        KEEP_WARM_AGING_THRESHOLD_SECS_DEFAULT,
    )
}

/// Proactive index-maintenance service shared across tool handlers.
///
/// All methods are non-blocking: heavy work is `tokio::spawn`-ed and throttled
/// so that only one operation of each kind can be in-flight at a time.
pub struct IndexKeeper {
    client: ContextStreamClient,
    session: Arc<SessionManager>,
    last_incremental: Mutex<Option<Instant>>,
    last_active_refresh: Mutex<Option<Instant>>,
    last_stale_reingest: Mutex<Option<Instant>>,
    last_scope_repair_reingest: Mutex<Option<Instant>>,
    last_aging_refresh: Mutex<Option<Instant>>,
    last_registry_prune: Mutex<Option<Instant>>,
    last_duplicate_merge: Mutex<Option<Instant>>,
    /// MongoDB-free acceleration layer. The Signal provider emits
    /// non-canonical file-change hints to server telemetry/Cloudflare
    /// Pipelines; tool correctness never depends on these events.
    acceleration_layer: AccelerationLayer,
    /// Atlas product layer retained only as a compatibility fallback
    /// while the migration is canaried.
    atlas_layer: AtlasLayer,
}

impl IndexKeeper {
    pub fn new(
        client: ContextStreamClient,
        session: Arc<SessionManager>,
        atlas_layer: AtlasLayer,
        acceleration_layer: AccelerationLayer,
    ) -> Self {
        Self {
            client,
            session,
            last_incremental: Mutex::new(None),
            last_active_refresh: Mutex::new(None),
            last_stale_reingest: Mutex::new(None),
            last_scope_repair_reingest: Mutex::new(None),
            last_aging_refresh: Mutex::new(None),
            last_registry_prune: Mutex::new(None),
            last_duplicate_merge: Mutex::new(None),
            acceleration_layer,
            atlas_layer,
        }
    }

    /// Single entry-point for any tool handler.  Runs all applicable
    /// index-maintenance checks (each independently throttled).
    pub fn tick(&self) {
        self.check_registry_prune();
        self.check_active_hot_refresh();
        self.check_incremental();
        self.check_aging_refresh();
    }

    fn check_registry_prune(&self) {
        if !Self::should_fire(&self.last_registry_prune, REGISTRY_PRUNE_INTERVAL_SECS) {
            return;
        }
        tokio::spawn(async move {
            prune_dead_index_entries();
        });
    }

    // ------------------------------------------------------------------
    // Active hot refresh: keep the active checkout hot on every tool touch.
    // ------------------------------------------------------------------

    fn check_active_hot_refresh(&self) {
        if !Self::should_fire(&self.last_active_refresh, ACTIVE_REFRESH_INTERVAL_SECS) {
            return;
        }

        let client = self.client.clone();
        let session = self.session.clone();

        tokio::spawn(async move {
            let state = session.state().await;
            let Some(pid) = state.project_id else {
                return;
            };
            let Some(path) = state.folder_path.clone() else {
                return;
            };
            let p = Path::new(&path);
            if !p.is_dir() || !is_safe_background_ingest_root(p) {
                return;
            }
            let Some(workspace_id) =
                bound_workspace_for_content(&path, state.workspace_id, pid, "active_hot_refresh")
            else {
                return;
            };

            let threshold_secs = active_hot_threshold_secs();
            let age_secs = ContextStreamClient::local_index_age_secs(&path);
            let inprogress_secs =
                ContextStreamClient::local_indexing_started_at(&path).map(|started| {
                    chrono::Utc::now()
                        .signed_duration_since(started)
                        .num_seconds()
                });

            let reason = match age_secs {
                Some(secs) if secs < threshold_secs => return,
                Some(secs) if secs >= STALE_THRESHOLD_HOURS * 60 * 60 => "stale",
                Some(_) => "not_hot",
                None => match inprogress_secs {
                    Some(secs) if secs < ACTIVE_REFRESH_INTERVAL_SECS as i64 => return,
                    Some(_) => "stranded",
                    None => "seed",
                },
            };

            tracing::debug!(
                reason,
                age_secs,
                threshold_secs,
                path = %path,
                "index-keeper active hot refresh triggered"
            );

            if !server_project_matches_bound_workspace(
                &client,
                workspace_id,
                pid,
                &path,
                "active_hot_refresh",
            )
            .await
            {
                return;
            }

            let params = IngestLocalParams {
                path: path.clone(),
                workspace_id: Some(workspace_id),
                project_id: Some(pid),
                force: Some(false),
                generate_editor_rules: None,
                include_media: None,
                max_files: Some(AGING_REFRESH_MAX_FILES),
                background: Some(true),
                origin: Some("active_hot_tick".to_string()),
                reroot: None,
            };

            match client.ingest_local(params).await {
                Ok(result) => {
                    if ContextStreamClient::ingest_scan_complete(&result)
                        && ContextStreamClient::ingest_result_committed(&result)
                    {
                        ContextStreamClient::write_index_status(&path, pid);
                    }
                    let files = result
                        .get("files_indexed")
                        .or_else(|| result.get("files_changed"))
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    tracing::info!(
                        reason,
                        files,
                        path = %path,
                        "index-keeper active hot refresh completed"
                    );
                }
                Err(e) => {
                    tracing::debug!(
                        reason,
                        error = %e,
                        path = %path,
                        "index-keeper active hot refresh failed"
                    );
                }
            }
        });
    }

    // ------------------------------------------------------------------
    // Incremental: index recently-changed files (throttle: 10s)
    // ------------------------------------------------------------------

    fn check_incremental(&self) {
        if !Self::should_fire(&self.last_incremental, INCREMENTAL_INTERVAL_SECS) {
            return;
        }

        let client = self.client.clone();
        let session = self.session.clone();
        let atlas_layer = self.atlas_layer.clone();
        let acceleration_layer = self.acceleration_layer.clone();

        tokio::spawn(async move {
            let state = session.state().await;
            let project_id = state.project_id;
            let folder_path = state.folder_path.clone();

            if let (Some(pid), Some(ref path)) = (project_id, &folder_path) {
                let root = Path::new(path);
                if !root.is_dir() || !is_safe_background_ingest_root(root) {
                    return;
                }
                if !ContextStreamClient::is_project_indexed(path) {
                    return;
                }
                let Some(workspace_id) = bound_workspace_for_content(
                    path,
                    state.workspace_id,
                    pid,
                    "incremental_refresh",
                ) else {
                    return;
                };
                if !server_project_matches_bound_workspace(
                    &client,
                    workspace_id,
                    pid,
                    path,
                    "incremental_refresh",
                )
                .await
                {
                    return;
                }
                let params = IngestLocalParams {
                    path: path.clone(),
                    workspace_id: Some(workspace_id),
                    project_id: Some(pid),
                    force: None,
                    generate_editor_rules: None,
                    include_media: None,
                    max_files: Some(INCREMENTAL_MAX_FILES),
                    background: Some(true),
                    origin: None,
                    reroot: None,
                };
                match client.ingest_local(params).await {
                    Ok(result) => {
                        let files = result
                            .get("files_indexed")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0);
                        if files > 0 {
                            tracing::debug!(
                                "index-keeper incremental: {} files indexed from {}",
                                files,
                                path
                            );
                            // Emit a single non-canonical file_changed
                            // signal. No-op when acceleration is not
                            // configured; Atlas stream is retained below as
                            // a temporary compatibility fallback only.
                            emit_incremental_stream_event(
                                &acceleration_layer,
                                &atlas_layer,
                                Some(workspace_id),
                                Some(pid),
                                path,
                                files,
                            )
                            .await;
                        }
                    }
                    Err(e) => {
                        tracing::debug!("index-keeper incremental failed: {}", e);
                    }
                }
            }
        });
    }

    // ------------------------------------------------------------------
    // Aging refresh: re-index when the local index is aging/stale, or seed
    // local metadata when it is missing entirely (throttle: 5 min).
    // ------------------------------------------------------------------

    fn check_aging_refresh(&self) {
        if !Self::should_fire(&self.last_aging_refresh, AGING_REFRESH_INTERVAL_SECS) {
            return;
        }

        let client = self.client.clone();
        let session = self.session.clone();

        tokio::spawn(async move {
            let state = session.state().await;
            let project_id = state.project_id;
            let folder_path = state.folder_path.clone();

            if let (Some(pid), Some(ref path)) = (project_id, &folder_path) {
                let p = Path::new(path);
                if !p.is_dir() || !is_safe_background_ingest_root(p) {
                    return;
                }
                let Some(workspace_id) =
                    bound_workspace_for_content(path, state.workspace_id, pid, "aging_refresh")
                else {
                    return;
                };

                let indexed_locally = ContextStreamClient::is_project_indexed(path);
                let age_secs = ContextStreamClient::local_index_age_secs(path);

                // Decide whether a refresh is warranted and label the reason.
                //
                // When local metadata is missing entirely (e.g. the index was
                // backfilled from the backend without a local hash manifest),
                // age-based checks return early and the index never refreshes.
                // Seed a background ingest instead so freshness checks gain
                // local ground truth and subsequent ticks use the age path.
                let reason = if !indexed_locally {
                    "seed"
                } else {
                    match age_secs {
                        Some(secs) if secs < keep_warm_aging_threshold_secs() => return,
                        Some(secs) if secs >= STALE_THRESHOLD_HOURS * 60 * 60 => "stale",
                        Some(_) => "aging",
                        // Metadata present but no/unparseable timestamp: treat
                        // as aging so a refresh re-establishes a known age.
                        None => "aging",
                    }
                };

                let age_display = age_secs
                    .map(|secs| format!("{}s", secs))
                    .unwrap_or_else(|| "unknown".to_string());
                tracing::debug!(
                    "index-keeper {}: index for {} (age {}), triggering background refresh",
                    reason,
                    path,
                    age_display
                );

                if !server_project_matches_bound_workspace(
                    &client,
                    workspace_id,
                    pid,
                    path,
                    "aging_refresh",
                )
                .await
                {
                    return;
                }

                let path_for_log = path.clone();
                let params = IngestLocalParams {
                    path: path.clone(),
                    workspace_id: Some(workspace_id),
                    project_id: Some(pid),
                    force: None,
                    generate_editor_rules: None,
                    include_media: None,
                    max_files: Some(AGING_REFRESH_MAX_FILES),
                    background: Some(true),
                    origin: None,
                    reroot: None,
                };
                match client.ingest_local(params).await {
                    Ok(result) => {
                        let files = result
                            .get("files_indexed")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0);
                        tracing::info!(
                            "index-keeper {} refresh completed: {} files indexed from {}",
                            reason,
                            files,
                            path_for_log
                        );
                    }
                    Err(e) => {
                        tracing::error!(
                            "index-keeper {} refresh failed for {}: {}",
                            reason,
                            path_for_log,
                            e
                        );
                    }
                }
            }
        });
    }

    // ------------------------------------------------------------------
    // Stale re-ingest: triggered post-search when health reports stale
    // ------------------------------------------------------------------

    /// Called after a search completes with stale or drifted health
    /// diagnostics. This is the urgent "we just got bad results" path, kept
    /// separate from the proactive `tick()` checks.
    ///
    /// Fires a background re-index when the index scope still matches and the
    /// index is either genuinely stale (>48h) or drift was detected (local
    /// edits newer than the indexed snapshot). Small drift is normally healed
    /// synchronously before the search; this covers the cases where the sync
    /// fast path did not run (too many changed files) or did not complete.
    pub fn maybe_trigger_stale_reingest(
        &self,
        workspace_id: Option<Uuid>,
        resolved_project_id: Option<Uuid>,
        folder_path: Option<&str>,
        freshness: &str,
        drift_detected: bool,
        scope_match: bool,
    ) -> Option<String> {
        if !scope_match {
            return None;
        }
        let reason = if drift_detected {
            "drift"
        } else if freshness == "stale" {
            "stale"
        } else {
            return None;
        };
        let project_id = resolved_project_id?;
        let path = folder_path?;
        let p = Path::new(path);
        if !p.exists() || !is_safe_background_ingest_root(p) {
            return None;
        }
        let workspace_id =
            bound_workspace_for_content(path, workspace_id, project_id, "stale_reingest")?;

        if !Self::should_fire(&self.last_stale_reingest, STALE_REINGEST_INTERVAL_SECS) {
            return None;
        }

        let client = self.client.clone();
        let path_for_log = path.to_string();
        let reason_for_log = reason;
        let params = IngestLocalParams {
            path: path.to_string(),
            workspace_id: Some(workspace_id),
            project_id: Some(project_id),
            force: Some(false),
            generate_editor_rules: None,
            include_media: None,
            max_files: None,
            background: Some(true),
            origin: None,
            reroot: None,
        };

        tokio::spawn(async move {
            if !server_project_matches_bound_workspace(
                &client,
                workspace_id,
                project_id,
                &path_for_log,
                "stale_reingest",
            )
            .await
            {
                return;
            }
            match client.ingest_local(params).await {
                Ok(result) => {
                    let files = result
                        .get("files_indexed")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    tracing::debug!(
                        "index-keeper {} re-index completed: {} files indexed from {}",
                        reason_for_log,
                        files,
                        path_for_log
                    );
                }
                Err(err) => {
                    tracing::debug!(
                        "index-keeper {} re-index failed for {}: {}",
                        reason_for_log,
                        path_for_log,
                        err
                    );
                }
            }
        });

        let message = if reason == "drift" {
            format!(
                "Started background re-index for `{}` because local edits are newer than the indexed snapshot.",
                path
            )
        } else {
            format!(
                "Started background re-index for `{}` because the current index freshness is stale.",
                path
            )
        };
        Some(message)
    }

    /// Called after search detects that the API's indexed project root belongs
    /// to another machine/checkout while the local folder still appears to be
    /// the same project. A process with disk access refreshes it directly; a
    /// hosted process asks the registered sync bridge, without changing the
    /// editor's hosted MCP transport.
    pub fn maybe_trigger_scope_repair_reingest(
        &self,
        workspace_id: Option<Uuid>,
        resolved_project_id: Option<Uuid>,
        folder_path: Option<&str>,
        indexed_root: &str,
    ) -> Option<String> {
        let project_id = resolved_project_id?;
        let path = folder_path?;
        if path.trim().is_empty() {
            return None;
        }
        let root = Path::new(path);
        let path_is_local = root.is_dir();
        if path_is_local && !is_safe_background_ingest_root(root) {
            return None;
        }
        let workspace_id = if path_is_local {
            bound_workspace_for_content(path, workspace_id, project_id, "scope_repair_reingest")?
        } else {
            // This process is the hosted MCP gateway and cannot inspect the
            // checkout binding itself. Requesting a bridge command is
            // non-content-bearing; the server and claiming bridge both verify
            // the exact tenant/project/checkout before any bytes are read.
            workspace_id?
        };
        let should_fire = Self::should_fire(
            &self.last_scope_repair_reingest,
            SCOPE_REPAIR_REINGEST_INTERVAL_SECS,
        );

        if !path_is_local {
            if should_fire {
                let client = self.client.clone();
                let path = path.to_string();
                tokio::spawn(async move {
                    if let Err(error) = client
                        .request_project_refresh(
                            project_id,
                            workspace_id,
                            Some(&path),
                            false,
                            "search.checkout_scope_refresh",
                        )
                        .await
                    {
                        tracing::debug!(
                            project_id = %project_id,
                            "hosted checkout refresh request failed: {}",
                            error
                        );
                    }
                });
                return Some(
                    "[INDEX_CHECKOUT_REFRESH] Hosted MCP asked the registered sync bridge to refresh this checkout overlay; current canonical results remain usable."
                        .to_string(),
                );
            }
            return Some(
                "[INDEX_CHECKOUT_REFRESH] This checkout overlay already has a hosted bridge refresh request in progress; current canonical results remain usable."
                    .to_string(),
            );
        }

        if should_fire {
            let client = self.client.clone();
            let path_for_log = path.to_string();
            let path_for_success = path.to_string();
            let path_for_rollback = path.to_string();
            let params = IngestLocalParams {
                path: path.to_string(),
                workspace_id: Some(workspace_id),
                project_id: Some(project_id),
                force: Some(false),
                generate_editor_rules: None,
                include_media: None,
                max_files: None,
                background: Some(true),
                origin: None,
                // Multiple machines/worktrees are first-class checkouts. A
                // scope repair may refresh this checkout but must not prune
                // another checkout's overlay.
                reroot: None,
            };

            tokio::spawn(async move {
                if !server_project_matches_bound_workspace(
                    &client,
                    workspace_id,
                    project_id,
                    &path_for_log,
                    "scope_repair_reingest",
                )
                .await
                {
                    return;
                }
                match client.ingest_local(params).await {
                    Ok(result) => {
                        if path_is_local
                            && ContextStreamClient::ingest_scan_complete(&result)
                            && ContextStreamClient::ingest_result_committed(&result)
                        {
                            ContextStreamClient::write_index_status(&path_for_success, project_id);
                        }
                        let files = result
                            .get("files_indexed")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0);
                        tracing::info!(
                            "index-keeper scope repair completed: {} files indexed from {}",
                            files,
                            path_for_log
                        );
                    }
                    Err(err) => {
                        if path_is_local {
                            ContextStreamClient::clear_index_status(&path_for_rollback);
                        }
                        tracing::debug!(
                            "index-keeper scope repair failed for {}: {}",
                            path_for_log,
                            err
                        );
                    }
                }
            });

            Some(format!(
                "[INDEX_CHECKOUT_REFRESH] Refreshing the active checkout overlay at `{}` without replacing the existing checkout rooted at `{}`; current canonical results remain usable.",
                path, indexed_root
            ))
        } else {
            Some(format!(
                "[INDEX_CHECKOUT_REFRESH] The active checkout overlay at `{}` is already queued for a non-destructive refresh; current canonical results remain usable.",
                path
            ))
        }
    }

    /// Re-ingest the validated local project twin without deleting any other
    /// machine or worktree overlay.
    /// Duplicate project deletion/merge is intentionally never automatic: it
    /// is destructive and requires an explicit server-side atomic precondition
    /// that this background path cannot currently supply.
    pub fn maybe_trigger_duplicate_merge(
        &self,
        workspace_id: Option<Uuid>,
        target_project_id: Uuid,
        source_project_id: Uuid,
        target_folder_path: &str,
    ) {
        if target_project_id == source_project_id {
            return;
        }
        if target_folder_path.trim().is_empty() || !Path::new(target_folder_path).is_dir() {
            return;
        }
        if !is_safe_background_ingest_root(Path::new(target_folder_path)) {
            return;
        }
        let Some(workspace_id) = bound_workspace_for_content(
            target_folder_path,
            workspace_id,
            target_project_id,
            "duplicate_merge",
        ) else {
            return;
        };
        if !Self::should_fire(&self.last_duplicate_merge, DUPLICATE_MERGE_INTERVAL_SECS) {
            return;
        }

        let client = self.client.clone();
        let path_for_log = target_folder_path.to_string();
        let path_for_status = target_folder_path.to_string();
        let params = IngestLocalParams {
            path: target_folder_path.to_string(),
            workspace_id: Some(workspace_id),
            project_id: Some(target_project_id),
            force: Some(false),
            generate_editor_rules: None,
            include_media: None,
            max_files: None,
            background: Some(true),
            origin: Some("scope_repair".to_string()),
            // Duplicate metadata repair is non-destructive; consolidation is
            // an explicit operation, never an automatic cross-checkout purge.
            reroot: None,
        };

        tokio::spawn(async move {
            if !server_project_matches_bound_workspace(
                &client,
                workspace_id,
                target_project_id,
                &path_for_log,
                "duplicate_checkout_refresh",
            )
            .await
            {
                return;
            }
            tracing::info!(
                target_project_id = %target_project_id,
                source_project_id = %source_project_id,
                "index-keeper left duplicate project unmerged; destructive consolidation requires an explicit operation"
            );

            match client.ingest_local(params).await {
                Ok(result) => {
                    if ContextStreamClient::ingest_scan_complete(&result)
                        && ContextStreamClient::ingest_result_committed(&result)
                    {
                        ContextStreamClient::write_index_status(
                            &path_for_status,
                            target_project_id,
                        );
                    }
                    let files = result
                        .get("files_indexed")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0);
                    tracing::info!(
                        target_project_id = %target_project_id,
                        source_project_id = %source_project_id,
                        files,
                        path = %path_for_log,
                        "index-keeper duplicate checkout refresh completed"
                    );
                }
                Err(err) => {
                    tracing::debug!(
                        target_project_id = %target_project_id,
                        source_project_id = %source_project_id,
                        path = %path_for_log,
                        error = %err,
                        "index-keeper duplicate checkout refresh failed"
                    );
                }
            }
        });
    }

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    /// Atomically check-and-update a throttle timestamp. Returns `true` if
    /// enough time has elapsed since the last firing.
    fn should_fire(last: &Mutex<Option<Instant>>, interval_secs: u64) -> bool {
        let mut guard = last.lock().unwrap();
        let now = Instant::now();
        if let Some(prev) = *guard {
            if now.duration_since(prev).as_secs() < interval_secs {
                return false;
            }
        }
        *guard = Some(now);
        true
    }
}

/// Emit a `file_changed` signal. Prefer the MongoDB-free acceleration
/// signal provider and fall back to Atlas Stream Processing only during the
/// compatibility window. No-op when workspace_id is unresolved.
async fn emit_incremental_stream_event(
    acceleration_layer: &AccelerationLayer,
    atlas_layer: &AtlasLayer,
    workspace_id: Option<Uuid>,
    project_id: Option<Uuid>,
    folder_path: &str,
    files_indexed: i64,
) {
    let workspace_id = match workspace_id {
        Some(w) => w,
        None => return, // no scope -> skip silently
    };
    let payload = serde_json::json!({
        "folder_path": folder_path,
        "files_indexed": files_indexed,
        "source": "index_keeper.incremental",
    });

    if let Some(signals) = acceleration_layer.signals() {
        if let Err(e) = signals
            .emit_payload(
                AccelerationSignalKind::FileChanged,
                workspace_id,
                project_id,
                payload,
            )
            .await
        {
            tracing::debug!(
                error = %e,
                "index-keeper: acceleration signal emit failed (best-effort; ignoring)"
            );
        }
        return;
    }

    if acceleration_layer.is_enabled() {
        metrics::counter!(
            "acceleration_signal_disabled_total",
            "source" => "index_keeper",
            "signal_type" => "file_changed",
        )
        .increment(1);
        return;
    }

    let stream = match atlas_layer.stream() {
        Some(s) => s,
        None => return,
    };
    metrics::counter!(
        "acceleration_signal_atlas_fallback_total",
        "source" => "index_keeper",
        "signal_type" => "file_changed",
    )
    .increment(1);
    let payload = serde_json::json!({
        "folder_path": folder_path,
        "files_indexed": files_indexed,
        "source": "index_keeper.incremental",
    });
    if let Err(e) = stream
        .emit_payload(
            AtlasStreamEventKind::FileChanged,
            workspace_id,
            project_id,
            payload,
        )
        .await
    {
        tracing::debug!(
            error = %e,
            "index-keeper: atlas stream emit failed (best-effort; ignoring)"
        );
    }
}

// ======================================================================
// Keep-warm daemon
//
// The reactive `tick()` path above only ever refreshes the single project
// the active session is pointed at. That leaves every *other* locally-mapped
// project's index to rot until a session happens to open it — which is why a
// project can sit dozens of hours stale (and why a freshly-checked-out repo on
// a new machine has no local index at all). The keep-warm daemon closes that
// gap: a single background loop that scans every project this machine knows
// about and proactively re-ingests the ones that are stale, aging, never
// seeded, or stranded mid-ingest — so an agent's first search hits a fresh
// index instead of paying a cold re-ingest.
//
// Single-tenant only: spawn from the stdio `run_server` path, NEVER from the
// shared HTTP gateway (enumerating one machine's local folders is meaningless
// — and unsafe — across many tenants).
// ======================================================================

/// How often the keep-warm daemon scans all mapped projects.
const KEEP_WARM_TICK_SECS: u64 = 180;

/// Delay before the first scan, so the daemon doesn't compete with the
/// session's own startup init/index work on a cold process.
const KEEP_WARM_INITIAL_DELAY_SECS: u64 = 20;

/// Minimum time between refresh *attempts* for the same project. A successful
/// ingest resets the on-disk age (natural cooldown); this guards the failure
/// case, where `ingest_local` clears the registry entry so the project would
/// otherwise look like a fresh "seed" and be retried on every single tick.
const KEEP_WARM_PROJECT_COOLDOWN_SECS: u64 = 1800;

/// Max concurrent background ingests the daemon runs at once.
const KEEP_WARM_MAX_CONCURRENT: usize = 2;

/// Max projects refreshed per tick (most-stale first), so one scan can't fan
/// out into an unbounded ingest storm on a machine with many projects.
const KEEP_WARM_MAX_PER_TICK: usize = 6;

/// An ingest that started but never committed is treated as "in progress" for
/// this long before the daemon assumes it was stranded (process crashed
/// mid-ingest) and retries it.
const KEEP_WARM_INPROGRESS_GRACE_MINS: i64 = 15;

fn keep_warm_tick_secs() -> u64 {
    env_u64("CONTEXTSTREAM_KEEP_WARM_TICK_SECS", KEEP_WARM_TICK_SECS)
}

fn keep_warm_project_cooldown_secs() -> u64 {
    env_u64(
        "CONTEXTSTREAM_KEEP_WARM_PROJECT_COOLDOWN_SECS",
        KEEP_WARM_PROJECT_COOLDOWN_SECS,
    )
}

fn keep_warm_max_concurrent() -> usize {
    env_usize(
        "CONTEXTSTREAM_KEEP_WARM_MAX_CONCURRENT",
        KEEP_WARM_MAX_CONCURRENT,
    )
}

fn keep_warm_max_per_tick() -> usize {
    env_usize(
        "CONTEXTSTREAM_KEEP_WARM_MAX_PER_TICK",
        KEEP_WARM_MAX_PER_TICK,
    )
}

/// A single project the keep-warm daemon should keep fresh.
#[derive(Clone, Debug)]
struct KeepWarmTarget {
    folder_path: String,
    project_id: Uuid,
    workspace_id: Option<Uuid>,
}

fn keep_warm_target_key(target: &KeepWarmTarget) -> std::path::PathBuf {
    std::fs::canonicalize(&target.folder_path)
        .unwrap_or_else(|_| std::path::PathBuf::from(&target.folder_path))
}

/// Whether the keep-warm daemon is enabled. On by default; opt out with
/// `CONTEXTSTREAM_KEEP_WARM=0` (also accepts false/off/no).
fn keep_warm_enabled() -> bool {
    !matches!(
        std::env::var("CONTEXTSTREAM_KEEP_WARM")
            .ok()
            .as_deref()
            .map(|s| s.trim().to_ascii_lowercase())
            .as_deref(),
        Some("0") | Some("false") | Some("off") | Some("no")
    )
}

fn keep_warm_read_json(path: &Path) -> Option<serde_json::Value> {
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Hygiene: drop local index-registry entries whose folder no longer exists on
/// disk (e.g. abandoned `/tmp` scratch projects) and stale "started-only"
/// entries whose ingest never committed. Best-effort; entries are recreated by
/// a future successful ingest if the folder reappears.
fn prune_dead_index_entries() {
    let Some(home) = dirs::home_dir() else {
        return;
    };
    let index_file = home.join(".contextstream").join("indexed-projects.json");
    let Some(data) = keep_warm_read_json(&index_file) else {
        return;
    };
    let Some(projects) = data.get("projects").and_then(|p| p.as_object()) else {
        return;
    };
    let now = chrono::Utc::now();
    let dead: Vec<(String, &'static str)> = projects
        .iter()
        .filter_map(|(path, info)| {
            if !Path::new(path.as_str()).exists() {
                return Some((path.clone(), "missing_path"));
            }
            if info.get("indexed_at").is_some() {
                return None;
            }
            let started_at = info
                .get("indexing_started_at")
                .and_then(|v| v.as_str())
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&chrono::Utc))?;
            let age_mins = now.signed_duration_since(started_at).num_minutes();
            if age_mins >= KEEP_WARM_INPROGRESS_GRACE_MINS {
                Some((path.clone(), "stranded_started_only"))
            } else {
                None
            }
        })
        .collect();
    for (path, reason) in dead {
        tracing::debug!("keep-warm: pruning local index entry {} ({})", path, reason);
        ContextStreamClient::clear_index_status(&path);
    }
}

/// Enumerate every project this machine knows about — the union of the local
/// ingest registry (`indexed-projects.json`) and the global workspace mappings
/// (`mappings.json`, which also carries `workspace_id`) — filtered to existing
/// directories and deduplicated by canonical checkout root. Multiple roots for
/// one project are intentional: each worktree/clone has independent freshness.
fn enumerate_keep_warm_targets() -> Vec<KeepWarmTarget> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let mut by_root: std::collections::HashMap<std::path::PathBuf, KeepWarmTarget> =
        std::collections::HashMap::new();
    let mut ambiguous_roots = std::collections::HashSet::new();

    let mut consider = |folder: &str, project_id: Option<Uuid>, workspace_id: Option<Uuid>| {
        let Some(pid) = project_id else {
            return;
        };
        let p = Path::new(folder);
        // Existing directory only (drops dead /tmp scratch + file-scope entries),
        // and never a filesystem root or other over-broad / sensitive root.
        if !p.is_dir() || !is_safe_background_ingest_root(p) {
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
            tracing::warn!(
                project_id = %pid,
                path = %folder,
                "keep-warm skipped one root because its project/workspace registries conflict"
            );
            return;
        }
        let entry = by_root.entry(root).or_insert_with(|| KeepWarmTarget {
            folder_path: folder.to_string(),
            project_id: pid,
            workspace_id,
        });
        if entry.workspace_id.is_none() {
            entry.workspace_id = workspace_id;
        }
    };

    // 1) Local ingest registry.
    if let Some(data) =
        keep_warm_read_json(&home.join(".contextstream").join("indexed-projects.json"))
    {
        if let Some(projects) = data.get("projects").and_then(|p| p.as_object()) {
            for (path, info) in projects {
                let pid = info
                    .get("project_id")
                    .and_then(|v| v.as_str())
                    .and_then(|s| Uuid::parse_str(s).ok());
                consider(path, pid, None);
            }
        }
    }

    // 2) Global workspace mappings (richer: carries workspace_id, covers folders
    //    this machine has mapped but not yet locally ingested).
    if let Some(data) = keep_warm_read_json(&home.join(".contextstream").join("mappings.json")) {
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

    by_root
        .into_values()
        .filter_map(|mut target| {
            let workspace_id = bound_workspace_for_content(
                &target.folder_path,
                target.workspace_id,
                target.project_id,
                "keep_warm_enumeration",
            )?;
            target.workspace_id = Some(workspace_id);
            Some(target)
        })
        .collect()
}

/// Decide whether (and why) a project needs a keep-warm refresh. Returns
/// `None` when the index is fresh enough to skip. Pure function — unit-tested.
fn classify_refresh(
    age_secs: Option<i64>,
    inprogress_age_mins: Option<i64>,
) -> Option<&'static str> {
    match age_secs {
        Some(secs) if secs < keep_warm_aging_threshold_secs() => None, // hot — leave it
        Some(secs) if secs >= STALE_THRESHOLD_HOURS * 60 * 60 => Some("stale"),
        Some(_) => Some("aging"),
        // No committed local index timestamp.
        None => match inprogress_age_mins {
            // An ingest is recently in flight; let it finish.
            Some(m) if m < KEEP_WARM_INPROGRESS_GRACE_MINS => None,
            // Started long ago but never committed → process died mid-ingest.
            Some(_) => Some("stranded"),
            // Never indexed on this machine.
            None => Some("seed"),
        },
    }
}

/// Spawn the background keep-warm daemon. Best-effort; never blocks tools.
///
/// **Single-tenant only.** Call from the stdio `run_server` path; do NOT call
/// from the shared HTTP gateway.
pub fn spawn_keep_warm_daemon(client: ContextStreamClient) {
    if !keep_warm_enabled() {
        tracing::debug!("keep-warm daemon disabled via CONTEXTSTREAM_KEEP_WARM");
        return;
    }

    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(KEEP_WARM_INITIAL_DELAY_SECS)).await;

        // Per-checkout attempt cooldown, so one persistently-failing worktree
        // cannot suppress or accelerate another root for the same project.
        let mut last_refresh: std::collections::HashMap<std::path::PathBuf, Instant> =
            std::collections::HashMap::new();
        let tick_secs = keep_warm_tick_secs();
        let cooldown_secs = keep_warm_project_cooldown_secs();
        let max_per_tick = keep_warm_max_per_tick();
        let max_concurrent = keep_warm_max_concurrent();

        let mut interval = tokio::time::interval(std::time::Duration::from_secs(tick_secs));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        tracing::info!(
            "keep-warm daemon started (tick {}s, cooldown {}s, max {}/tick, {} concurrent)",
            tick_secs,
            cooldown_secs,
            max_per_tick,
            max_concurrent,
        );

        loop {
            interval.tick().await;
            prune_dead_index_entries();

            let now = Instant::now();
            // Bound the cooldown map.
            last_refresh.retain(|_, t| now.duration_since(*t).as_secs() < cooldown_secs);

            // Collect everything due this tick: (target, reason, age_secs).
            let mut due: Vec<(KeepWarmTarget, &'static str, Option<i64>)> = Vec::new();
            for target in enumerate_keep_warm_targets() {
                let target_key = keep_warm_target_key(&target);
                if let Some(t) = last_refresh.get(&target_key) {
                    if now.duration_since(*t).as_secs() < cooldown_secs {
                        continue;
                    }
                }
                let age = ContextStreamClient::local_index_age_secs(&target.folder_path);
                let inprogress_mins =
                    ContextStreamClient::local_indexing_started_at(&target.folder_path)
                        .map(|t| chrono::Utc::now().signed_duration_since(t).num_minutes());
                if let Some(reason) = classify_refresh(age, inprogress_mins) {
                    due.push((target, reason, age));
                }
            }

            if due.is_empty() {
                continue;
            }

            // Most-stale first: `None` age (seed/stranded) is most urgent, then
            // by descending age in hours.
            due.sort_by(|a, b| {
                let ka = a.2.unwrap_or(i64::MAX);
                let kb = b.2.unwrap_or(i64::MAX);
                kb.cmp(&ka)
            });
            due.truncate(max_per_tick);

            tracing::info!("keep-warm: refreshing {} project(s) this tick", due.len());

            let sem = Arc::new(tokio::sync::Semaphore::new(max_concurrent));
            let mut handles = Vec::new();
            for (target, reason, age) in due {
                // Record the attempt up front, regardless of outcome.
                last_refresh.insert(keep_warm_target_key(&target), Instant::now());

                let permit = match sem.clone().acquire_owned().await {
                    Ok(p) => p,
                    Err(_) => break,
                };
                let client = client.clone();
                handles.push(tokio::spawn(async move {
                    let _permit = permit;
                    let Some(workspace_id) = target.workspace_id else {
                        return;
                    };
                    if !server_project_matches_bound_workspace(
                        &client,
                        workspace_id,
                        target.project_id,
                        &target.folder_path,
                        "keep_warm_daemon",
                    )
                    .await
                    {
                        return;
                    }
                    let params = IngestLocalParams {
                        path: target.folder_path.clone(),
                        workspace_id: Some(workspace_id),
                        project_id: Some(target.project_id),
                        force: None,
                        generate_editor_rules: None,
                        include_media: None,
                        max_files: Some(AGING_REFRESH_MAX_FILES),
                        background: Some(true),
                        origin: Some("keep_warm_daemon".to_string()),
                        reroot: None,
                    };
                    let age_display = age
                        .map(|secs| format!("{}s", secs))
                        .unwrap_or_else(|| "none".to_string());
                    match client.ingest_local(params).await {
                        Ok(result) => {
                            let files = result
                                .get("files_indexed")
                                .and_then(|v| v.as_i64())
                                .unwrap_or(0);
                            tracing::info!(
                                "keep-warm {} refresh: {} files indexed from {} (age {})",
                                reason,
                                files,
                                target.folder_path,
                                age_display
                            );
                        }
                        Err(e) => {
                            tracing::debug!(
                                "keep-warm {} refresh failed for {}: {}",
                                reason,
                                target.folder_path,
                                e
                            );
                        }
                    }
                }));
            }
            for h in handles {
                let _ = h.await;
            }
        }
    });
}

#[cfg(test)]
mod keep_warm_tests {
    use super::*;

    #[test]
    fn fresh_index_is_skipped() {
        assert_eq!(classify_refresh(Some(0), None), None);
        assert_eq!(
            classify_refresh(Some(keep_warm_aging_threshold_secs() - 1), None),
            None
        );
    }

    #[test]
    fn aging_and_stale_are_classified() {
        assert_eq!(
            classify_refresh(Some(keep_warm_aging_threshold_secs()), None),
            Some("aging")
        );
        assert_eq!(
            classify_refresh(Some(keep_warm_aging_threshold_secs() + 1), None),
            Some("aging")
        );
        assert_eq!(
            classify_refresh(Some(STALE_THRESHOLD_HOURS * 60 * 60), None),
            Some("stale")
        );
        assert_eq!(
            classify_refresh(Some((STALE_THRESHOLD_HOURS + 100) * 60 * 60), None),
            Some("stale")
        );
    }

    #[test]
    fn missing_index_seeds_or_recovers() {
        // Never indexed at all.
        assert_eq!(classify_refresh(None, None), Some("seed"));
        // Ingest recently in flight — let it finish.
        assert_eq!(classify_refresh(None, Some(0)), None);
        assert_eq!(
            classify_refresh(None, Some(KEEP_WARM_INPROGRESS_GRACE_MINS - 1)),
            None
        );
        // Started long ago, never committed — stranded, retry.
        assert_eq!(
            classify_refresh(None, Some(KEEP_WARM_INPROGRESS_GRACE_MINS)),
            Some("stranded")
        );
        assert_eq!(classify_refresh(None, Some(600)), Some("stranded"));
    }
}
