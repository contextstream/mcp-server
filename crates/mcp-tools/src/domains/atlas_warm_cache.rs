//! Read-through wrapper for the Atlas regional warm-cache layer.
//!
//!  The provider + stream pipelines + cache
//! collections all land in A8; this module is the binary-side glue
//! that lets individual tool handlers consult the cache before
//! firing their slow primary call.
//!
//! # The contract every wrapper follows
//!
//! 1. Build a stable `scope_hash` from the call's inputs. Same call
//!    inputs MUST produce the same hash; different inputs MUST
//!    differ. The hash is the cache key.
//! 2. Call [`try_lookup`] with a hard latency budget (default 50ms).
//!    Anything slower returns `None` and the caller falls through.
//! 3. On hit (`Some((payload, age_ms))`), render the cached payload
//!    as the response body and stamp `served_from`, `cache_age_ms`,
//!    `cache_hit=true` on the structured envelope.
//! 4. On miss/None, run the primary call unchanged and stamp
//!    `served_from=primary_server`, `cache_hit=false`.
//!
//! # Why a tight latency budget
//!
//! Atlas reads on a regional replica via PrivateLink should be
//! <30ms p99. If a lookup is taking longer, something is wrong
//! upstream and the cache is no longer net-positive — we'd be
//! adding latency to the primary path. The 50ms cap lets us return
//! `None` and let the primary call serve the response. Lesson
//! `53be7d19`: never make the primary path slower in the name of
//! cache.

use mcp_types::{
    acceleration_layer::{
        AccelerationLayer, AccelerationReadModelScope, WarmCacheLayer, WarmCacheLookup,
        WarmCachePut,
    },
    atlas_layer::{AtlasFederationScope, AtlasLayer, AtlasWarmCacheKind, CachedBundle},
};
use std::sync::OnceLock;
use std::time::Duration;
use tracing::debug;
use uuid::Uuid;

/// Caller identity policy for caches that may contain user-specific search or
/// context material.
///
/// Both HTTP and stdio transports install an explicit task-local
/// [`mcp_types::SessionKey`]. Authenticated HTTP resolves to `Authenticated`,
/// anonymous HTTP resolves to `Bypass`, and the explicit stdio marker gets a
/// distinct local-only cache lane. Missing task-local identity fails closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallerCacheScope {
    Authenticated(String),
    LocalStdio,
    Bypass,
}

impl CallerCacheScope {
    /// Stable, non-secret identity suitable for in-process and distributed
    /// cache partitioning. `None` means the caller-sensitive cache must be
    /// bypassed entirely.
    pub fn cache_identity(&self) -> Option<&str> {
        match self {
            Self::Authenticated(identity) => Some(identity.as_str()),
            Self::LocalStdio => Some(local_stdio_cache_identity()),
            Self::Bypass => None,
        }
    }
}

/// Local stdio is single-user only within one process. A process nonce keeps
/// that fast lane warm for repeated calls without letting two independent
/// local MCP processes collide in a distributed cache.
fn local_stdio_cache_identity() -> &'static str {
    static IDENTITY: OnceLock<String> = OnceLock::new();
    IDENTITY
        .get_or_init(|| format!("l:stdio:{}", Uuid::new_v4().simple()))
        .as_str()
}

fn caller_cache_scope_for_session_key(
    session_key: Option<mcp_types::SessionKey>,
) -> CallerCacheScope {
    match session_key {
        None => CallerCacheScope::Bypass,
        Some(mcp_types::SessionKey::Local) => CallerCacheScope::LocalStdio,
        Some(mcp_types::SessionKey::AnonymousHttp(_)) => CallerCacheScope::Bypass,
        Some(key) => key
            .atlas_user_scope_token()
            .map(CallerCacheScope::Authenticated)
            .unwrap_or(CallerCacheScope::Bypass),
    }
}

/// Resolve the cache policy for the current tool invocation.
pub fn current_caller_cache_scope() -> CallerCacheScope {
    if let Some(identity) = mcp_client::get_task_caller_cache_identity() {
        return CallerCacheScope::Authenticated(identity);
    }
    caller_cache_scope_for_session_key(mcp_client::get_task_session_key())
}

/// Resolve the current task's per-caller scope token from the
/// task-local `SessionKey` set by the transport. Anonymous HTTP and missing
/// task-local state return `None` and must bypass caller-sensitive caches.
/// Explicit local stdio receives a process-isolated identity so it remains
/// cacheable without colliding with another process.
///
/// Tool handlers call this to derive the `user_scope` argument
/// they hand to `scope_hash_for_*` and to `AtlasFederationScope`
/// (and `AtlasSearchScope`). Cheap (~one task-local read), safe
/// to call on every cached request.
pub fn current_user_scope_token() -> Option<String> {
    current_caller_cache_scope()
        .cache_identity()
        .map(str::to_string)
}

/// Whether this invocation may touch any process-shared or distributed warm
/// cache. Anonymous HTTP and missing task-local identity fail closed even when
/// a caller accidentally supplies a workspace-only scope hash.
fn caller_cache_access_allowed() -> bool {
    current_caller_cache_scope().cache_identity().is_some()
}

/// Hard cap on the wrapper's lookup latency. Anything slower is
/// treated as a miss so the primary path remains unaffected.
pub const MAX_LOOKUP_WAIT: Duration = Duration::from_millis(50);

/// Try the MongoDB-free acceleration warm cache first, then fall back
/// to the legacy Atlas federation warm cache while the migration is
/// in compatibility mode.
pub async fn try_lookup_accelerated(
    acceleration_layer: &AccelerationLayer,
    atlas_layer: &AtlasLayer,
    kind: AtlasWarmCacheKind,
    scope: AtlasFederationScope,
    primary_baseline_ms: u64,
) -> Option<CachedBundle> {
    if !caller_cache_access_allowed() {
        return None;
    }

    if let Some(bundle) = try_acceleration_lookup(acceleration_layer, kind, &scope).await {
        return Some(bundle);
    }

    try_lookup(atlas_layer, kind, scope, primary_baseline_ms).await
}

async fn try_acceleration_lookup(
    acceleration_layer: &AccelerationLayer,
    kind: AtlasWarmCacheKind,
    scope: &AtlasFederationScope,
) -> Option<CachedBundle> {
    let provider = acceleration_layer.warm_cache()?;
    let (scope_type, scope_id) = read_model_scope_for_federation(scope)?;

    let lookup = WarmCacheLookup {
        scope: AccelerationReadModelScope {
            tenant_id: None,
            workspace_id: Some(scope.workspace_id),
            project_id: scope.project_id,
            scope_type,
            scope_id,
        },
        model: read_model_name_for_kind(kind).to_string(),
        cache_key: scope.scope_hash.clone(),
        stale_ok: false,
    };

    let started = std::time::Instant::now();
    match tokio::time::timeout(MAX_LOOKUP_WAIT, provider.get_read_model(lookup)).await {
        Ok(Ok(Some(hit))) => {
            metrics::counter!(
                "mcp_acceleration_warm_cache_lookup_total",
                "kind" => kind.as_str(),
                "outcome" => "hit",
                "layer" => warm_cache_layer_label(hit.served_from),
            )
            .increment(1);
            metrics::histogram!(
                "mcp_acceleration_warm_cache_lookup_ms",
                "kind" => kind.as_str(),
                "outcome" => "hit",
            )
            .record(started.elapsed().as_millis() as f64);
            Some(CachedBundle {
                kind,
                workspace_id: scope.workspace_id,
                scope_hash: scope.scope_hash.clone(),
                payload: hit.payload,
                warmed_at: chrono::Utc::now(),
                age_ms: None,
            })
        }
        Ok(Ok(None)) => {
            metrics::counter!(
                "mcp_acceleration_warm_cache_lookup_total",
                "kind" => kind.as_str(),
                "outcome" => "miss",
                "layer" => "none",
            )
            .increment(1);
            None
        }
        Ok(Err(error)) => {
            debug!(
                kind = kind.as_str(),
                error = %error,
                "acceleration warm-cache lookup error; falling through"
            );
            metrics::counter!(
                "mcp_acceleration_warm_cache_lookup_total",
                "kind" => kind.as_str(),
                "outcome" => "provider_error",
                "layer" => "none",
            )
            .increment(1);
            None
        }
        Err(_) => {
            debug!(
                kind = kind.as_str(),
                "acceleration warm-cache lookup exceeded {}ms; falling through",
                MAX_LOOKUP_WAIT.as_millis()
            );
            metrics::counter!(
                "mcp_acceleration_warm_cache_lookup_total",
                "kind" => kind.as_str(),
                "outcome" => "timeout",
                "layer" => "none",
            )
            .increment(1);
            None
        }
    }
}

fn read_model_scope_for_federation(scope: &AtlasFederationScope) -> Option<(String, Uuid)> {
    if let Some(project_id) = scope.project_id {
        Some(("project".to_string(), project_id))
    } else {
        Some(("workspace".to_string(), scope.workspace_id))
    }
}

fn read_model_name_for_kind(kind: AtlasWarmCacheKind) -> &'static str {
    match kind {
        AtlasWarmCacheKind::Context => "context_warm_bundle",
        AtlasWarmCacheKind::MemoryEventsHot => "memory_events_hot",
        AtlasWarmCacheKind::SubgraphSnapshot => "subgraph_snapshot",
        AtlasWarmCacheKind::DependencyResult => "dependency_result",
        AtlasWarmCacheKind::RelatedNodes => "related_nodes",
        AtlasWarmCacheKind::LessonsWarning => "lessons_warning",
        AtlasWarmCacheKind::Recall => "session_recall",
        AtlasWarmCacheKind::Ground => "session_ground",
        AtlasWarmCacheKind::DecisionsHot => "decisions_hot",
        AtlasWarmCacheKind::PreferencesHot => "preferences_hot",
        AtlasWarmCacheKind::MemoryTasksHot => "memory_tasks_hot",
        AtlasWarmCacheKind::MemoryTodosHot => "memory_todos_hot",
        AtlasWarmCacheKind::MemoryPlansHot => "memory_plans_hot",
        AtlasWarmCacheKind::SkillsHot => "skills_hot",
        AtlasWarmCacheKind::DocHot => "doc_hot",
        AtlasWarmCacheKind::CapsuleOpen => "capsule_open",
        AtlasWarmCacheKind::TicketsHot => "tickets_hot",
        AtlasWarmCacheKind::HandoffsHot => "handoffs_hot",
        AtlasWarmCacheKind::IncidentsHot => "incidents_hot",
    }
}

fn warm_cache_layer_label(layer: WarmCacheLayer) -> &'static str {
    match layer {
        WarmCacheLayer::Redis => "redis",
        WarmCacheLayer::Postgres => "postgres",
        WarmCacheLayer::R2 => "r2",
        WarmCacheLayer::StalePostgres => "stale_postgres",
    }
}

/// Try to fetch a cached bundle for the given scope. Returns `None`
/// in every degenerate case (provider absent, layer disabled,
/// timeout exceeded, lookup error, miss, stale, invalidated). The
/// caller MUST be prepared to fall through on `None`.
///
/// `primary_baseline_ms` is an optional hint about the primary
/// call's typical latency — used purely to record a savings
/// histogram on hits. Pass 0 if unknown.
pub async fn try_lookup(
    atlas_layer: &AtlasLayer,
    kind: AtlasWarmCacheKind,
    scope: AtlasFederationScope,
    primary_baseline_ms: u64,
) -> Option<CachedBundle> {
    if !caller_cache_access_allowed() {
        return None;
    }

    let provider = atlas_layer.federation()?;

    let started = std::time::Instant::now();
    let lookup_fut = provider.warm_cache_lookup(kind, &scope);
    // The federation provider already increments
    // `mcp_atlas_warm_cache_lookup_total` for hit/miss/stale/invalidated.
    // The wrapper only needs to distinguish wrapper-only outcomes here
    // (the 50ms cap firing, and provider errors surfaced as None) plus
    // record the wall-clock distribution per outcome for tuning.
    let bundle = match tokio::time::timeout(MAX_LOOKUP_WAIT, lookup_fut).await {
        Ok(Ok(Some(b))) => b,
        Ok(Ok(None)) => {
            // Provider returned None — federation already counted the
            // specific reason (miss / stale / invalidated). Just record
            // wrapper latency so we have a histogram for the no-hit path.
            metrics::histogram!(
                "mcp_atlas_warm_cache_lookup_ms",
                "kind" => kind.as_str(),
                "outcome" => "no_hit",
            )
            .record(started.elapsed().as_millis() as f64);
            return None;
        }
        Ok(Err(e)) => {
            debug!(
                kind = kind.as_str(),
                error = %e,
                "atlas-warm-cache: lookup error; falling through to primary"
            );
            metrics::counter!(
                "mcp_atlas_warm_cache_lookup_total",
                "kind" => kind.as_str(),
                "outcome" => "wrapper_provider_error",
            )
            .increment(1);
            metrics::histogram!(
                "mcp_atlas_warm_cache_lookup_ms",
                "kind" => kind.as_str(),
                "outcome" => "wrapper_provider_error",
            )
            .record(started.elapsed().as_millis() as f64);
            return None;
        }
        Err(_) => {
            debug!(
                kind = kind.as_str(),
                "atlas-warm-cache: lookup exceeded {}ms; falling through to primary",
                MAX_LOOKUP_WAIT.as_millis()
            );
            metrics::counter!(
                "mcp_atlas_warm_cache_lookup_total",
                "kind" => kind.as_str(),
                "outcome" => "wrapper_timeout",
            )
            .increment(1);
            metrics::histogram!(
                "mcp_atlas_warm_cache_lookup_ms",
                "kind" => kind.as_str(),
                "outcome" => "wrapper_timeout",
            )
            .record(started.elapsed().as_millis() as f64);
            return None;
        }
    };

    let lookup_ms = started.elapsed().as_millis() as u64;
    metrics::histogram!(
        "mcp_atlas_warm_cache_lookup_ms",
        "kind" => kind.as_str(),
        "outcome" => "served",
    )
    .record(lookup_ms as f64);

    // Record savings histogram only on hit. Savings = primary
    // baseline minus our lookup latency. Negative values clamped to
    // 0 (defensive — shouldn't happen with the timeout above).
    let savings_ms = primary_baseline_ms.saturating_sub(lookup_ms);
    if primary_baseline_ms > 0 {
        metrics::histogram!(
            "mcp_atlas_warm_cache_savings_ms",
            "kind" => kind.as_str(),
        )
        .record(savings_ms as f64);
    }

    Some(bundle)
}

/// One-shot cache lookup + write-back wrapper. Reduces the per-
/// handler boilerplate from ~25 lines to ~5. Used by P1 #6-9
/// handlers (memory list_tasks/todos/plans, skill list, memory
/// get_doc, capsule open/get) which all follow the same pattern:
///
/// 1. Try `try_lookup`; if hit, return the cached `serde_json::Value`.
/// 2. Otherwise call the primary closure to produce the canonical value.
/// 3. Spawn a background `warm_cache_put` to populate the cache.
///
/// `workspace_id: None` skips the cache entirely (no scope).
///
/// The closure form keeps the primary-call code path lazy — we only
/// invoke it on cache miss / when there's no workspace scope. That
/// matters for tools whose primary call is itself expensive
/// (memory.get_doc fetches up to several KB; capsule.open can be
/// MBs); we don't want to evaluate it eagerly.
pub async fn fetch_or_cache<F, Fut>(
    atlas_layer: &AtlasLayer,
    kind: AtlasWarmCacheKind,
    workspace_id: Option<Uuid>,
    user_scope: Option<&str>,
    project_id: Option<Uuid>,
    scope_hash: String,
    primary_baseline_ms: u64,
    fetch: F,
) -> mcp_types::Result<serde_json::Value>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = mcp_types::Result<serde_json::Value>>,
{
    if !caller_cache_access_allowed() {
        return fetch().await;
    }

    if let Some(ws) = workspace_id {
        let mut scope = mcp_types::atlas_layer::AtlasFederationScope {
            workspace_id: ws,
            project_id,
            scope_hash: scope_hash.clone(),
            user_scope: user_scope.map(|s| s.to_string()),
        };
        if let Some(bundle) =
            try_lookup(atlas_layer, kind, scope.clone(), primary_baseline_ms).await
        {
            return Ok(bundle.payload);
        }
        // Reset for write-back path below.
        scope.scope_hash = scope_hash.clone();
    }
    let value = fetch().await?;
    if let Some(ws) = workspace_id {
        let scope = mcp_types::atlas_layer::AtlasFederationScope {
            workspace_id: ws,
            project_id,
            scope_hash,
            user_scope: user_scope.map(|s| s.to_string()),
        };
        put_in_background(atlas_layer.clone(), kind, scope, value.clone());
    }
    Ok(value)
}

/// Best-effort write-back: deposit a primary-call response into the
/// regional warm cache so the next caller in the same region with
/// the same scope hits instead of running primary. Spawned in the
/// background so the calling handler returns immediately. Failure
/// is logged + counted but never surfaced to the caller — the
/// primary response has already been served.
pub fn put_in_background(
    atlas_layer: AtlasLayer,
    kind: AtlasWarmCacheKind,
    scope: AtlasFederationScope,
    payload: serde_json::Value,
) {
    if !caller_cache_access_allowed() {
        return;
    }

    let provider = match atlas_layer.federation() {
        Some(p) => p,
        None => return,
    };
    tokio::spawn(async move {
        match tokio::time::timeout(
            std::time::Duration::from_secs(2),
            provider.warm_cache_put(kind, &scope, payload),
        )
        .await
        {
            Ok(Ok(())) => {
                debug!(
                    kind = kind.as_str(),
                    scope_hash = %scope.scope_hash,
                    "atlas-warm-cache: wrote response to regional cache"
                );
            }
            Ok(Err(e)) => {
                debug!(
                    kind = kind.as_str(),
                    scope_hash = %scope.scope_hash,
                    error = %e,
                    "atlas-warm-cache: write-back failed (best-effort; ignored)"
                );
            }
            Err(_) => {
                debug!(
                    kind = kind.as_str(),
                    scope_hash = %scope.scope_hash,
                    "atlas-warm-cache: write-back exceeded 2s timeout (best-effort; ignored)"
                );
            }
        }
    });
}

/// Best-effort write-through for the MongoDB-free acceleration cache.
///
/// The new Postgres/Redis read-model write runs first when the acceleration
/// provider is configured. The legacy Atlas write still runs afterward during
/// the migration compatibility window, so canary/shadow comparisons keep
/// working. Both paths are silent best-effort and never affect the primary
/// response.
pub fn put_accelerated_in_background(
    acceleration_layer: AccelerationLayer,
    atlas_layer: AtlasLayer,
    kind: AtlasWarmCacheKind,
    scope: AtlasFederationScope,
    payload: serde_json::Value,
) {
    if !caller_cache_access_allowed() {
        return;
    }

    if let Some(provider) = acceleration_layer.warm_cache() {
        if let Some((scope_type, scope_id)) = read_model_scope_for_federation(&scope) {
            let put = WarmCachePut {
                scope: AccelerationReadModelScope {
                    tenant_id: None,
                    workspace_id: Some(scope.workspace_id),
                    project_id: scope.project_id,
                    scope_type,
                    scope_id,
                },
                model: read_model_name_for_kind(kind).to_string(),
                cache_key: scope.scope_hash.clone(),
                generation: None,
                source_generation: 1,
                payload: payload.clone(),
                etag: None,
                expires_at: chrono::Duration::from_std(kind.max_age())
                    .ok()
                    .map(|ttl| chrono::Utc::now() + ttl),
            };
            let scope_hash = scope.scope_hash.clone();
            tokio::spawn(async move {
                match tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    provider.put_read_model(put),
                )
                .await
                {
                    Ok(Ok(())) => {
                        debug!(
                            kind = kind.as_str(),
                            scope_hash = %scope_hash,
                            "acceleration-warm-cache: wrote JSONB read model"
                        );
                        metrics::counter!(
                            "mcp_acceleration_warm_cache_write_total",
                            "kind" => kind.as_str(),
                            "outcome" => "ok",
                        )
                        .increment(1);
                    }
                    Ok(Err(error)) => {
                        debug!(
                            kind = kind.as_str(),
                            scope_hash = %scope_hash,
                            error = %error,
                            "acceleration-warm-cache: write-through failed"
                        );
                        metrics::counter!(
                            "mcp_acceleration_warm_cache_write_total",
                            "kind" => kind.as_str(),
                            "outcome" => "provider_error",
                        )
                        .increment(1);
                    }
                    Err(_) => {
                        debug!(
                            kind = kind.as_str(),
                            scope_hash = %scope_hash,
                            "acceleration-warm-cache: write-through exceeded 2s timeout"
                        );
                        metrics::counter!(
                            "mcp_acceleration_warm_cache_write_total",
                            "kind" => kind.as_str(),
                            "outcome" => "timeout",
                        )
                        .increment(1);
                    }
                }
            });
        }
    }

    put_in_background(atlas_layer, kind, scope, payload);
}

/// Build a stable scope hash for `context()` cache lookups. Inputs
/// must mirror the stream pipeline's keying so a hit corresponds
/// exactly to the same call shape.
///
/// Pipeline `pipeline-context-prewarm` keys on:
///   `<workspace_id>:<project_id|"no_project">:coding_task`
///
/// We mirror that here — `intent` is fixed to `coding_task` for now
/// (the only intent we pre-warm); future intent flavours can extend
/// this enum without changing the helper signature.
pub fn scope_hash_for_context(
    workspace_id: Uuid,
    project_id: Option<Uuid>,
    intent: &str,
) -> String {
    format!(
        "{}:{}:{}",
        workspace_id,
        project_id
            .map(|p| p.to_string())
            .unwrap_or_else(|| "no_project".to_string()),
        intent
    )
}

/// Caller-isolated variant for live `context()` responses. Stream-prewarmed
/// bundles use the shared helper above, while live responses can contain
/// personal lessons, memory, and opaque grounding handles and therefore must
/// never reuse a cross-user cache key.
pub fn scope_hash_for_context_scoped(
    workspace_id: Uuid,
    project_id: Option<Uuid>,
    intent: &str,
    user_scope: Option<&str>,
) -> String {
    let shared = scope_hash_for_context(workspace_id, project_id, intent);
    match user_scope {
        Some(scope) => format!("{shared}:user:{scope}"),
        None => shared,
    }
}

/// Stream pipeline `pipeline-memory-events-hot` keys on:
///   `<workspace_id>:<project_id|"no_project">:hot_24h`
pub fn scope_hash_for_memory_events_hot(workspace_id: Uuid, project_id: Option<Uuid>) -> String {
    format!(
        "{}:{}:hot_24h",
        workspace_id,
        project_id
            .map(|p| p.to_string())
            .unwrap_or_else(|| "no_project".to_string())
    )
}

/// A5's `refresh-subgraph-snapshot` trigger keys per-workspace
/// activity snapshots on `workspace_id` directly (one snapshot per
/// workspace).
pub fn scope_hash_for_subgraph(workspace_id: Uuid) -> String {
    workspace_id.to_string()
}

/// `session(get_lessons)` cache key. Lessons surface every turn for
/// `[LESSONS_WARNING]` auto-injection; same workspace + project +
/// query combination produces the same lesson set. Optional `query`
/// is folded in so query-filtered lookups don't collide with the
/// no-query default. Severity / category / limit filters are NOT in
/// the key — they're applied client-side after the cached payload
/// is fetched, so the same primary call can serve multiple filter
/// shapes. Key shape:
///   `<workspace_id>:<project_id|"no_project">:lessons:<query_hash|"all">`
pub fn scope_hash_for_lessons_warning(
    workspace_id: Uuid,
    user_scope: Option<&str>,
    project_id: Option<Uuid>,
    query: Option<&str>,
) -> String {
    let query_part = hash_query_for_scope(query);
    format!(
        "{}:{}:{}:lessons:{}",
        workspace_id,
        user_scope.unwrap_or("shared"),
        project_id
            .map(|p| p.to_string())
            .unwrap_or_else(|| "no_project".to_string()),
        query_part
    )
}

/// `session(recall)` cache key. P0 #2. Recall fans out across
/// transcripts/snapshots/docs/decisions/lessons; the same query in
/// the same workspace/project context produces the same ranking.
/// `limit` is NOT folded in — recall results are ordered, so a
/// `limit=5` slice is a prefix of `limit=20`; cache the larger and
/// truncate client-side.
pub fn scope_hash_for_recall(
    workspace_id: Uuid,
    user_scope: Option<&str>,
    project_id: Option<Uuid>,
    query: &str,
) -> String {
    format!(
        "{}:{}:{}:recall:{}",
        workspace_id,
        user_scope.unwrap_or("shared"),
        project_id
            .map(|p| p.to_string())
            .unwrap_or_else(|| "no_project".to_string()),
        hash_query_for_scope(Some(query))
    )
}

/// `session(ground)` cache key. P0 #3. Ground is a composite of
/// recall + docs + decisions + lessons + skills + git for a given
/// user_message. Within a single turn, the same user_message
/// produces the same bundle (ground is idempotent per turn).
pub fn scope_hash_for_ground(
    workspace_id: Uuid,
    user_scope: Option<&str>,
    project_id: Option<Uuid>,
    user_message: &str,
) -> String {
    format!(
        "{}:{}:{}:ground:{}",
        workspace_id,
        user_scope.unwrap_or("shared"),
        project_id
            .map(|p| p.to_string())
            .unwrap_or_else(|| "no_project".to_string()),
        hash_query_for_scope(Some(user_message))
    )
}

/// `memory(decisions)` cache key. P0 #4. Decisions list is filtered
/// by optional query; decisions get captured during a session so
/// freshness matters more than for other surfaces — 60 s TTL is
/// tighter (see `AtlasWarmCacheKind::DecisionsHot::max_age`).
pub fn scope_hash_for_decisions_hot(
    workspace_id: Uuid,
    user_scope: Option<&str>,
    project_id: Option<Uuid>,
    query: Option<&str>,
) -> String {
    format!(
        "{}:{}:{}:decisions:{}",
        workspace_id,
        user_scope.unwrap_or("shared"),
        project_id
            .map(|p| p.to_string())
            .unwrap_or_else(|| "no_project".to_string()),
        hash_query_for_scope(query)
    )
}

/// `memory(list_nodes, node_type=preference|constraint)` cache key.
/// P0 #5. node_type is part of the key so preference + constraint
/// reads don't collide. Lowercased to normalize stylistic variation.
pub fn scope_hash_for_preferences_hot(
    workspace_id: Uuid,
    user_scope: Option<&str>,
    project_id: Option<Uuid>,
    node_type: &str,
) -> String {
    format!(
        "{}:{}:{}:prefs:{}",
        workspace_id,
        user_scope.unwrap_or("shared"),
        project_id
            .map(|p| p.to_string())
            .unwrap_or_else(|| "no_project".to_string()),
        node_type.trim().to_ascii_lowercase()
    )
}

/// Generic list-shaped cache key. Used by `memory(list_tasks)` /
/// `(list_todos)` / `(list_plans)` / `skill(list)` and similar
/// `(workspace, project, list_kind, optional filter)` shaped
/// caches. P1 #6, #7, etc. `list_kind` distinguishes which list
/// surface (e.g. "tasks_pending", "skills_personal"); `filter` is
/// an optional already-canonicalised string (sorted enum, lowercased)
/// so two calls with the same filters in different order collapse.
pub fn scope_hash_for_list(
    workspace_id: Uuid,
    user_scope: Option<&str>,
    project_id: Option<Uuid>,
    list_kind: &str,
    filter: Option<&str>,
) -> String {
    let filter_part = filter
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(|s| {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            std::hash::Hasher::write(&mut hasher, s.to_ascii_lowercase().as_bytes());
            format!("{:x}", std::hash::Hasher::finish(&hasher))
        })
        .unwrap_or_else(|| "all".to_string());
    format!(
        "{}:{}:{}:list_{}:{}",
        workspace_id,
        user_scope.unwrap_or("shared"),
        project_id
            .map(|p| p.to_string())
            .unwrap_or_else(|| "no_project".to_string()),
        list_kind.trim().to_ascii_lowercase(),
        filter_part
    )
}

/// `memory(get_doc, doc_id)` cache key. P1 #8. Doc ID is the
/// stable identifier; workspace/project guard against the
/// theoretical case of an ID colliding across workspaces (which
/// the API doesn't currently allow but which the cache must
/// enforce by construction).
pub fn scope_hash_for_doc_hot(
    workspace_id: Uuid,
    user_scope: Option<&str>,
    project_id: Option<Uuid>,
    doc_id: &str,
) -> String {
    format!(
        "{}:{}:{}:doc:{}",
        workspace_id,
        user_scope.unwrap_or("shared"),
        project_id
            .map(|p| p.to_string())
            .unwrap_or_else(|| "no_project".to_string()),
        doc_id.trim()
    )
}

/// `capsule(open|get, capsule_id)` cache key. P1 #9. Capsules are
/// immutable; capsule_id is the stable + sufficient key. workspace
/// is folded in defensively so cross-workspace capsule sharing
/// (which capsule shares enable via `share_token`) doesn't put two
/// workspaces' identical capsule_id into the same cache row.
pub fn scope_hash_for_capsule_open(
    workspace_id: Uuid,
    user_scope: Option<&str>,
    capsule_id: &str,
) -> String {
    format!(
        "{}:{}:capsule:{}",
        workspace_id,
        user_scope.unwrap_or("shared"),
        capsule_id.trim()
    )
}

/// Internal: hash a query string deterministically + bound key
/// length. Used by lessons_warning, recall, ground, decisions_hot.
fn hash_query_for_scope(query: Option<&str>) -> String {
    match query {
        Some(q) if !q.trim().is_empty() => {
            let normalised = q.trim().to_ascii_lowercase();
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            std::hash::Hasher::write(&mut hasher, normalised.as_bytes());
            format!("{:x}", std::hash::Hasher::finish(&hasher))
        }
        _ => "all".to_string(),
    }
}

/// Variable-length Neo4j graph queries (impact / call_path /
/// circular_dependencies / unused_code) cache PER TARGET — each
/// query about a different code element is a different question. A
/// per-workspace snapshot would conflate them. Key shape:
///   `<workspace_id>:<project_id|"no_project">:graph_<kind>:<target>`
///
/// `query_kind` should be one of: `impact`, `call_path`,
/// `circular_dependencies`, `unused_code`. `target` is the symbol /
/// path the caller asked about; for queries with no specific target
/// (circular_dependencies, unused_code on the whole project), pass
/// `"_workspace"` as the target so the key is still stable.
pub fn scope_hash_for_graph_query(
    workspace_id: Uuid,
    project_id: Option<Uuid>,
    query_kind: &str,
    target: &str,
) -> String {
    format!(
        "{}:{}:graph_{}:{}",
        workspace_id,
        project_id
            .map(|p| p.to_string())
            .unwrap_or_else(|| "no_project".to_string()),
        query_kind,
        target
    )
}

/// `graph_related` cache key folds the `node_id` (already a UUID,
/// stable identifier for a graph node), depth, and the
/// relation-types filter. Two calls that asked about the same node
/// with the same depth and the same set of relation types — in any
/// order — get the same hash. Different depth, different relations,
/// or a different node ⇒ different hash. Key shape:
///   `<workspace_id>:<project_id|"no_project">:rel:<node_id>:d<max_depth>:r<sorted_relations|"any">`
pub fn scope_hash_for_related_query(
    workspace_id: Uuid,
    project_id: Option<Uuid>,
    node_id: Uuid,
    max_depth: Option<i64>,
    relation_types: Option<&[String]>,
) -> String {
    let depth = max_depth
        .map(|d| d.to_string())
        .unwrap_or_else(|| "any".to_string());
    // Sort + lowercase relation types so caller order doesn't matter
    // and "Calls" / "calls" collapse to the same hash.
    let relations = match relation_types {
        Some(list) if !list.is_empty() => {
            let mut sorted: Vec<String> =
                list.iter().map(|s| s.trim().to_ascii_lowercase()).collect();
            sorted.sort();
            sorted.dedup();
            sorted.join(",")
        }
        _ => "any".to_string(),
    };
    format!(
        "{}:{}:rel:{}:d{}:r{}",
        workspace_id,
        project_id
            .map(|p| p.to_string())
            .unwrap_or_else(|| "no_project".to_string()),
        node_id,
        depth,
        relations
    )
}

/// `graph_dependencies` has the widest cache-key surface of any
/// graph tool — same target asked at different depths or with
/// different transitive flags is genuinely a different result. Fold
/// every dimension into a canonical string so cache hits are
/// deterministic across calls but cache misses are honest. Key shape:
///   `<workspace_id>:<project_id|"no_project">:dep:<target_type>:<target>:d<max_depth>:t<include_transitive>`
///
/// `target` is the symbol name (function/type/variable) or file path
/// (module). Depth and transitive flag are normalised to canonical
/// strings (`"any"` / `"true"` / `"false"`) so absent / explicit-but-
/// equivalent inputs collapse to the same hash.
pub fn scope_hash_for_dependency_query(
    workspace_id: Uuid,
    project_id: Option<Uuid>,
    target_type: &str,
    target: &str,
    max_depth: Option<i64>,
    include_transitive: Option<bool>,
) -> String {
    let depth = max_depth
        .map(|d| d.to_string())
        .unwrap_or_else(|| "any".to_string());
    let transitive = match include_transitive {
        Some(true) => "true",
        Some(false) => "false",
        None => "any",
    };
    format!(
        "{}:{}:dep:{}:{}:d{}:t{}",
        workspace_id,
        project_id
            .map(|p| p.to_string())
            .unwrap_or_else(|| "no_project".to_string()),
        target_type.to_ascii_lowercase(),
        target,
        depth,
        transitive
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_hash_includes_workspace_project_intent() {
        let ws = Uuid::from_u128(1);
        let pid = Uuid::from_u128(2);
        let h1 = scope_hash_for_context(ws, Some(pid), "coding_task");
        let h2 = scope_hash_for_context(ws, Some(pid), "coding_task");
        assert_eq!(h1, h2, "same inputs must produce same hash");

        let h3 = scope_hash_for_context(ws, Some(pid), "explainer");
        assert_ne!(h1, h3, "different intent must produce different hash");

        let h4 = scope_hash_for_context(ws, None, "coding_task");
        assert_ne!(h1, h4, "no project must differ from project");
        assert!(h4.contains("no_project"));
    }

    #[test]
    fn memory_events_hot_hash_is_deterministic() {
        let ws = Uuid::from_u128(42);
        let h1 = scope_hash_for_memory_events_hot(ws, None);
        let h2 = scope_hash_for_memory_events_hot(ws, None);
        assert_eq!(h1, h2);
        assert!(h1.ends_with(":no_project:hot_24h"));
    }

    #[test]
    fn subgraph_hash_is_just_workspace() {
        let ws = Uuid::from_u128(7);
        assert_eq!(scope_hash_for_subgraph(ws), ws.to_string());
    }

    #[test]
    fn graph_query_hash_includes_kind_and_target() {
        let ws = Uuid::from_u128(11);
        let pid = Uuid::from_u128(12);
        let h_impact = scope_hash_for_graph_query(ws, Some(pid), "impact", "fn_foo");
        let h_call_path = scope_hash_for_graph_query(ws, Some(pid), "call_path", "fn_foo");
        let h_other_target = scope_hash_for_graph_query(ws, Some(pid), "impact", "fn_bar");
        assert_ne!(h_impact, h_call_path, "different kinds must differ");
        assert_ne!(h_impact, h_other_target, "different targets must differ");
        // Same inputs deterministic
        assert_eq!(
            scope_hash_for_graph_query(ws, Some(pid), "impact", "fn_foo"),
            h_impact
        );
        // Project-less form is still well-formed
        let h_no_project = scope_hash_for_graph_query(ws, None, "impact", "fn_foo");
        assert!(h_no_project.contains("no_project"));
        // Workspace-wide queries (no specific target) get the
        // sentinel `_workspace`
        let h_workspace_wide =
            scope_hash_for_graph_query(ws, Some(pid), "circular_dependencies", "_workspace");
        assert!(h_workspace_wide.ends_with(":graph_circular_dependencies:_workspace"));
    }

    #[test]
    fn context_hash_differs_per_workspace() {
        let h1 = scope_hash_for_context(Uuid::from_u128(1), None, "coding_task");
        let h2 = scope_hash_for_context(Uuid::from_u128(2), None, "coding_task");
        assert_ne!(h1, h2);
    }

    #[test]
    fn related_hash_normalises_relation_types() {
        let ws = Uuid::from_u128(41);
        let pid = Uuid::from_u128(42);
        let node = Uuid::from_u128(99);

        // Same inputs deterministic.
        let rels_a: Vec<String> = vec!["Calls".into(), "Imports".into()];
        let h1 = scope_hash_for_related_query(ws, Some(pid), node, Some(2), Some(&rels_a));
        let h2 = scope_hash_for_related_query(ws, Some(pid), node, Some(2), Some(&rels_a));
        assert_eq!(h1, h2);

        // Order doesn't matter — caller-supplied order is normalised.
        let rels_reversed: Vec<String> = vec!["Imports".into(), "Calls".into()];
        let h_rev =
            scope_hash_for_related_query(ws, Some(pid), node, Some(2), Some(&rels_reversed));
        assert_eq!(h1, h_rev);

        // Case doesn't matter.
        let rels_lower: Vec<String> = vec!["calls".into(), "imports".into()];
        let h_lower = scope_hash_for_related_query(ws, Some(pid), node, Some(2), Some(&rels_lower));
        assert_eq!(h1, h_lower);

        // Duplicates collapse.
        let rels_dup: Vec<String> = vec!["calls".into(), "calls".into(), "imports".into()];
        let h_dup = scope_hash_for_related_query(ws, Some(pid), node, Some(2), Some(&rels_dup));
        assert_eq!(h1, h_dup);

        // Different relation set → different hash.
        let rels_b: Vec<String> = vec!["calls".into(), "implements".into()];
        let h_diff = scope_hash_for_related_query(ws, Some(pid), node, Some(2), Some(&rels_b));
        assert_ne!(h1, h_diff);

        // Different depth → different hash.
        let h_d3 = scope_hash_for_related_query(ws, Some(pid), node, Some(3), Some(&rels_a));
        assert_ne!(h1, h_d3);

        // Different node → different hash.
        let other_node = Uuid::from_u128(100);
        let h_other_node =
            scope_hash_for_related_query(ws, Some(pid), other_node, Some(2), Some(&rels_a));
        assert_ne!(h1, h_other_node);

        // Absent depth + relations collapse to canonical "any".
        let h_any = scope_hash_for_related_query(ws, Some(pid), node, None, None);
        assert!(h_any.contains(":dany:rany"));
        // Empty relation list is treated as "any" (same as None).
        let empty: Vec<String> = vec![];
        let h_empty = scope_hash_for_related_query(ws, Some(pid), node, None, Some(&empty));
        assert_eq!(h_any, h_empty);
    }

    #[test]
    fn dependency_hash_folds_every_dimension() {
        let ws = Uuid::from_u128(31);
        let pid = Uuid::from_u128(32);

        // Same inputs deterministic.
        let h_a = scope_hash_for_dependency_query(
            ws,
            Some(pid),
            "function",
            "fn_foo",
            Some(3),
            Some(true),
        );
        let h_a_again = scope_hash_for_dependency_query(
            ws,
            Some(pid),
            "function",
            "fn_foo",
            Some(3),
            Some(true),
        );
        assert_eq!(h_a, h_a_again);

        // Different target_type → different hash (e.g. function fn_foo
        // vs type fn_foo are different questions).
        let h_type =
            scope_hash_for_dependency_query(ws, Some(pid), "type", "fn_foo", Some(3), Some(true));
        assert_ne!(h_a, h_type);

        // Different target → different hash.
        let h_bar = scope_hash_for_dependency_query(
            ws,
            Some(pid),
            "function",
            "fn_bar",
            Some(3),
            Some(true),
        );
        assert_ne!(h_a, h_bar);

        // Different depth → different hash.
        let h_d5 = scope_hash_for_dependency_query(
            ws,
            Some(pid),
            "function",
            "fn_foo",
            Some(5),
            Some(true),
        );
        assert_ne!(h_a, h_d5);

        // Different transitive → different hash.
        let h_no_trans = scope_hash_for_dependency_query(
            ws,
            Some(pid),
            "function",
            "fn_foo",
            Some(3),
            Some(false),
        );
        assert_ne!(h_a, h_no_trans);

        // Absent depth/transitive collapse to canonical "any" so two
        // call paths that left both unset hash the same.
        let h_any_a =
            scope_hash_for_dependency_query(ws, Some(pid), "function", "fn_foo", None, None);
        let h_any_b =
            scope_hash_for_dependency_query(ws, Some(pid), "function", "fn_foo", None, None);
        assert_eq!(h_any_a, h_any_b);
        assert!(h_any_a.contains(":dany:tany"));

        // target_type is normalised to lowercase so callers passing
        // "Function" vs "function" don't double-cache the same result.
        let h_lower = scope_hash_for_dependency_query(
            ws,
            Some(pid),
            "function",
            "fn_foo",
            Some(3),
            Some(true),
        );
        let h_upper = scope_hash_for_dependency_query(
            ws,
            Some(pid),
            "FUNCTION",
            "fn_foo",
            Some(3),
            Some(true),
        );
        assert_eq!(h_lower, h_upper);

        // No-project form well-formed.
        let h_no_project =
            scope_hash_for_dependency_query(ws, None, "function", "fn_foo", Some(3), Some(true));
        assert!(h_no_project.contains(":no_project:"));
    }

    #[tokio::test]
    async fn put_in_background_is_silent_noop_for_noop_layer() {
        // The default no-op layer has no `federation()` — `put_in_
        // background` must spawn nothing and not panic.
        let layer = mcp_types::atlas_layer::noop_layer();
        let scope = AtlasFederationScope::new(Uuid::new_v4(), "any-hash");
        put_in_background(
            layer,
            AtlasWarmCacheKind::Context,
            scope,
            serde_json::json!({"test": true}),
        );
        // Yield to give any spawned task a chance to (not) run.
        tokio::task::yield_now().await;
        // No assertion needed — survival without panic is the test.
    }

    #[tokio::test]
    async fn try_lookup_returns_none_on_noop_layer() {
        // The default no-op layer has `federation()` = None, so
        // `try_lookup` must return None immediately without doing
        // any I/O.
        let layer = mcp_types::atlas_layer::noop_layer();
        let scope = AtlasFederationScope::new(Uuid::new_v4(), "any-hash");
        let started = std::time::Instant::now();
        let got = try_lookup(&layer, AtlasWarmCacheKind::Context, scope, 1500).await;
        let elapsed = started.elapsed();
        assert!(got.is_none());
        // No-op should be instant — well under 5ms even on a slow box.
        assert!(
            elapsed.as_millis() < 50,
            "noop layer should not introduce measurable latency"
        );
    }

    #[test]
    fn max_lookup_wait_is_under_primary_baseline() {
        // Sanity: the wrapper budget must be small enough that a
        // miss doesn't double the slowest primary call's latency.
        // memory(list_events) at 134ms p95 is the tightest target.
        // 50ms wrapper + 134ms primary = 184ms — acceptable.
        assert!(MAX_LOOKUP_WAIT.as_millis() <= 50);
    }

    /// Per-user scope isolation: two callers in the same workspace
    /// with different `user_scope` tokens must produce distinct
    /// scope hashes for every user-scoped surface. This is the
    /// invariant that prevents personal items leaking across
    /// teammates.
    #[test]
    fn user_scope_isolates_every_user_scoped_helper() {
        let ws = Uuid::from_u128(7);
        let pid = Uuid::from_u128(8);
        let alice = "j:alice123";
        let bob = "j:bob456";

        // lessons_warning
        let l_a = scope_hash_for_lessons_warning(ws, Some(alice), Some(pid), None);
        let l_b = scope_hash_for_lessons_warning(ws, Some(bob), Some(pid), None);
        assert_ne!(l_a, l_b);

        // recall
        let r_a = scope_hash_for_recall(ws, Some(alice), Some(pid), "deploy");
        let r_b = scope_hash_for_recall(ws, Some(bob), Some(pid), "deploy");
        assert_ne!(r_a, r_b);

        // ground
        let g_a = scope_hash_for_ground(ws, Some(alice), Some(pid), "ship it");
        let g_b = scope_hash_for_ground(ws, Some(bob), Some(pid), "ship it");
        assert_ne!(g_a, g_b);

        // decisions_hot
        let d_a = scope_hash_for_decisions_hot(ws, Some(alice), Some(pid), None);
        let d_b = scope_hash_for_decisions_hot(ws, Some(bob), Some(pid), None);
        assert_ne!(d_a, d_b);

        // preferences_hot
        let p_a = scope_hash_for_preferences_hot(ws, Some(alice), Some(pid), "preference");
        let p_b = scope_hash_for_preferences_hot(ws, Some(bob), Some(pid), "preference");
        assert_ne!(p_a, p_b);

        // list (used by tasks/todos/plans/skills)
        let lt_a = scope_hash_for_list(ws, Some(alice), Some(pid), "tasks", None);
        let lt_b = scope_hash_for_list(ws, Some(bob), Some(pid), "tasks", None);
        assert_ne!(lt_a, lt_b);

        // doc_hot
        let dh_a = scope_hash_for_doc_hot(ws, Some(alice), Some(pid), "doc-uuid");
        let dh_b = scope_hash_for_doc_hot(ws, Some(bob), Some(pid), "doc-uuid");
        assert_ne!(dh_a, dh_b);

        // capsule_open
        let c_a = scope_hash_for_capsule_open(ws, Some(alice), "capsule-uuid");
        let c_b = scope_hash_for_capsule_open(ws, Some(bob), "capsule-uuid");
        assert_ne!(c_a, c_b);
    }

    /// `None` user_scope must be stable but ALSO must differ from
    /// any populated user_scope. Without this, a workspace-shared
    /// row could collide with a user-scoped row that hashed to the
    /// "shared" sentinel coincidentally.
    #[test]
    fn user_scope_none_is_stable_and_not_collide() {
        let ws = Uuid::from_u128(11);

        let none1 = scope_hash_for_recall(ws, None, None, "q");
        let none2 = scope_hash_for_recall(ws, None, None, "q");
        assert_eq!(none1, none2);

        let scoped = scope_hash_for_recall(ws, Some("j:user"), None, "q");
        assert_ne!(none1, scoped);
    }

    /// A user_scope token literally equal to the "shared" sentinel
    /// could in principle collide with the None case. The helpers
    /// don't sanitise, so we rely on `SessionKey::atlas_user_scope_token`
    /// always producing a versioned variant prefix. Verify that contract
    /// holds (defence-in-depth — the SessionKey unit tests cover the
    /// generation side; this confirms the helpers' sentinel choice
    /// is safe given that contract).
    #[test]
    fn shared_sentinel_does_not_collide_with_real_token_format() {
        // No real token ever equals "shared" because tokens carry the
        // versioned caller-scope namespace and variant tag.
        let token = mcp_types::SessionKey::Jwt("anything".into()).atlas_user_scope_token();
        assert!(token.is_some());
        let t = token.unwrap();
        assert_ne!(t.as_str(), "shared");
        assert!(t.starts_with("csuc:v2:j:") || t.starts_with("csuc:v2:k:"));
    }

    #[tokio::test]
    async fn cache_scope_distinguishes_explicit_stdio_from_anonymous_http() {
        assert!(
            !caller_cache_access_allowed(),
            "missing task-local identity must fail closed"
        );

        let (stdio, stdio_allowed) =
            mcp_client::run_with_session_key(mcp_types::SessionKey::Local, || async {
                (current_caller_cache_scope(), caller_cache_access_allowed())
            })
            .await;
        assert_eq!(stdio, CallerCacheScope::LocalStdio);
        assert!(stdio_allowed);
        let identity = stdio.cache_identity().expect("stdio cache identity");
        assert!(identity.starts_with("l:stdio:"));
        assert_eq!(stdio.cache_identity(), Some(identity));

        let (anonymous_http, anonymous_allowed) = mcp_client::run_with_session_key(
            mcp_types::SessionKey::for_anonymous_http("mcp-session-a"),
            || async { (current_caller_cache_scope(), caller_cache_access_allowed()) },
        )
        .await;
        assert_eq!(anonymous_http, CallerCacheScope::Bypass);
        assert_eq!(anonymous_http.cache_identity(), None);
        assert!(!anonymous_allowed);

        assert_eq!(
            caller_cache_scope_for_session_key(None),
            CallerCacheScope::Bypass,
            "missing task-local identity must not be mistaken for stdio"
        );
    }

    #[tokio::test]
    async fn spawned_task_without_propagated_identity_bypasses_cache() {
        let parent = mcp_types::SessionKey::Jwt("caller-a".to_string());
        mcp_client::run_with_session_key(parent, || async {
            assert!(matches!(
                current_caller_cache_scope(),
                CallerCacheScope::Authenticated(_)
            ));
            assert!(caller_cache_access_allowed());
            let (child_scope, child_allowed) = tokio::spawn(async {
                (current_caller_cache_scope(), caller_cache_access_allowed())
            })
            .await
            .expect("spawned task");
            assert_eq!(child_scope, CallerCacheScope::Bypass);
            assert!(!child_allowed);
        })
        .await;
    }

    #[tokio::test]
    async fn transport_cache_identity_is_stable_and_separate_from_session_partition() {
        let session_partition = mcp_types::SessionKey::Jwt("session-specific".to_string());
        let scope = mcp_client::run_with_session_key(session_partition, || async {
            mcp_client::run_with_caller_cache_identity(
                "csuc:v2:j:stable-caller".to_string(),
                || async { current_caller_cache_scope() },
            )
            .await
        })
        .await;

        assert_eq!(scope.cache_identity(), Some("csuc:v2:j:stable-caller"));
    }

    #[test]
    fn cache_scope_partitions_authenticated_callers_without_raw_secrets() {
        let jwt_secret = "jwt-secret-that-must-not-enter-cache-keys";
        let api_secret = "api-secret-that-must-not-enter-cache-keys";
        let jwt = caller_cache_scope_for_session_key(Some(mcp_types::SessionKey::Jwt(
            jwt_secret.to_string(),
        )));
        let api_key = caller_cache_scope_for_session_key(Some(mcp_types::SessionKey::ApiKey(
            api_secret.to_string(),
        )));

        let jwt_identity = jwt.cache_identity().expect("JWT cache identity");
        let api_identity = api_key.cache_identity().expect("API-key cache identity");
        assert_ne!(jwt_identity, api_identity);
        assert!(jwt_identity.starts_with("csuc:v2:j:"));
        assert!(api_identity.starts_with("csuc:v2:k:"));
        assert!(!jwt_identity.contains(jwt_secret));
        assert!(!api_identity.contains(api_secret));
    }
}
