//! Search domain tools: semantic, hybrid, keyword, pattern, exhaustive, refactor, team.

use async_trait::async_trait;
use mcp_client::{
    client::{HotPathHintEntry, HotPathsHint},
    CheckoutRoutingScope, ContextStreamClient, GraphDependenciesParams, GraphTarget,
    IngestLocalParams, RequestOptions, SearchParams, TargetedFileDecision,
};
use mcp_session::{auto_init::resolve_workspace, SessionManager};
use mcp_types::{
    api::{
        CheckoutScopeStatus, Project, ProjectAgentMapResponse, ProjectAgentMapRouteHint,
        SearchIndexTrustEnvelope, SearchResponse, SearchResult,
    },
    tool::{ContentItem, ToolAnnotations, ToolCategory, ToolMetadata, ToolResult},
    Error, ErrorCode, Result,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Cap how long the hybrid endpoint can hold us before we fall back to a
/// faster lane. The server's hybrid p50 is ~350ms; outliers around 1-2s are
/// normal; anything past 12s is almost always a hung path on the LLM
/// normalization or graph-enrichment stage. Falling back to semantic keeps
/// the MCP tool well inside the client's tool-call budget (typically 30s)
/// instead of returning a hard "operation timed out" with no results.
const HYBRID_FAST_FALLBACK: Duration = Duration::from_secs(12);

/// Total MCP-side transport guard for Guided Search. The API owns a stricter
/// 3.5s end-to-end budget (retrieval + Navigator); this extra 1.5s is only
/// network/queueing margin and does not delay normal responses. Retries remain
/// disabled so a stalled request falls back to raw hybrid evidence once.
const GUIDED_SEARCH_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const GUIDED_SEARCH_FALLBACK_RESERVE_MAX: Duration = Duration::from_secs(1);
const GUIDED_SEARCH_FINALIZATION_RESERVE_MAX: Duration = Duration::from_millis(100);
const GUIDED_SEARCH_DEFAULT_LIMIT: usize = 12;
const GUIDED_SEARCH_MAX_LIMIT: usize = 30;

/// Whether CONTEXTSTREAM_DEBUG is set to a truthy value. Gates opt-in
/// diagnostic text (e.g. `[SCOPE_DIAGNOSTICS]`) that would otherwise leak
/// internal state to the agent/user on every call.
fn is_debug_enabled() -> bool {
    matches!(
        std::env::var("CONTEXTSTREAM_DEBUG")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Per-process result cache for `search()`. The short TTL preserves index
/// freshness. A per-caller quota prevents noisy-neighbor eviction, while the
/// larger global bound keeps enough callers warm on the shared HTTP gateway.
const SEARCH_CACHE_TTL: Duration = Duration::from_secs(30);
const SEARCH_CACHE_MAX_ENTRIES: usize = 128;
const SEARCH_CACHE_MAX_ENTRIES_PER_CALLER: usize = 16;

static SEARCH_RESULT_CACHE: OnceLock<crate::domains::result_cache::ResultCache<(String, Value)>> =
    OnceLock::new();
static INDEX_SCOPE_WARNINGS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn mark_checkout_scope_unconfirmed(mut result: ToolResult) -> ToolResult {
    const NOTE: &str = "[CHECKOUT_SCOPE] Search used canonical project evidence, but the MCP could not derive an exact active-checkout locator. Do not infer that uncommitted worktree changes were included.";
    match result.content.first_mut() {
        Some(ContentItem::Text { text }) => {
            text.push_str("\n\n");
            text.push_str(NOTE);
        }
        _ => result.content.push(ContentItem::text(NOTE)),
    }
    if let Some(object) = result
        .structured_content
        .as_mut()
        .and_then(Value::as_object_mut)
    {
        object.insert("checkout_scope_unconfirmed".to_string(), Value::Bool(true));
        object.insert(
            "checkout_scope_reason".to_string(),
            Value::String("checkout_routing_scope_unavailable".to_string()),
        );
    }
    result
}

#[derive(Clone)]
struct CachedGitOutput {
    stdout: Option<Vec<u8>>,
    refreshed_at: Instant,
    in_flight: bool,
}

static GIT_OUTPUT_CACHE: OnceLock<Mutex<HashMap<(String, String), CachedGitOutput>>> =
    OnceLock::new();

fn git_output_cache() -> &'static Mutex<HashMap<(String, String), CachedGitOutput>> {
    GIT_OUTPUT_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn run_git_output_bounded(folder: &str, args: &[&str]) -> Option<Vec<u8>> {
    use std::process::Stdio;

    let mut child = Command::new("git")
        .args(args)
        .current_dir(folder)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = child.wait_with_output().ok()?;
                return status.success().then_some(output.stdout);
            }
            Ok(None) if started.elapsed() < Duration::from_millis(300) => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Err(_) => return None,
        }
    }
}

/// Return a recent cached git probe and refresh it on a bounded background
/// thread. Git process startup/status can be arbitrarily slow on huge repos,
/// submodules, and network filesystems, so it must never sit on the retrieval
/// critical path.
fn cached_git_output(
    folder_path: &str,
    kind: &'static str,
    args: &'static [&'static str],
    allow_stale: bool,
) -> Option<Vec<u8>> {
    let folder = std::fs::canonicalize(folder_path)
        .unwrap_or_else(|_| PathBuf::from(folder_path))
        .to_string_lossy()
        .to_string();
    let key = (folder.clone(), kind.to_string());
    let mut cache = git_output_cache().lock().ok()?;
    let fresh = cache
        .get(&key)
        .is_some_and(|entry| entry.refreshed_at.elapsed() <= Duration::from_secs(2));
    if fresh {
        return cache.get(&key).and_then(|entry| entry.stdout.clone());
    }
    let stale = cache.get(&key).and_then(|entry| {
        (allow_stale && entry.refreshed_at.elapsed() <= Duration::from_secs(30))
            .then(|| entry.stdout.clone())
            .flatten()
    });
    let already_in_flight = cache.get(&key).is_some_and(|entry| entry.in_flight);
    if !already_in_flight {
        let prior_stdout = cache.get(&key).and_then(|entry| entry.stdout.clone());
        let prior_refreshed_at = cache
            .get(&key)
            .map(|entry| entry.refreshed_at)
            .unwrap_or_else(Instant::now);
        cache.insert(
            key.clone(),
            CachedGitOutput {
                stdout: prior_stdout,
                refreshed_at: prior_refreshed_at,
                in_flight: true,
            },
        );
        std::thread::spawn(move || {
            let stdout = run_git_output_bounded(&folder, args);
            if let Ok(mut cache) = git_output_cache().lock() {
                cache.insert(
                    key,
                    CachedGitOutput {
                        stdout,
                        refreshed_at: Instant::now(),
                        in_flight: false,
                    },
                );
            }
        });
    }
    stale
}

fn search_cache() -> &'static crate::domains::result_cache::ResultCache<(String, Value)> {
    SEARCH_RESULT_CACHE.get_or_init(|| {
        crate::domains::result_cache::ResultCache::new(SEARCH_CACHE_TTL, SEARCH_CACHE_MAX_ENTRIES)
    })
}

fn put_search_cache(cache_key: String, value: (String, Value)) {
    let caller_scope = super::atlas_warm_cache::current_caller_cache_scope();
    let Some(caller_identity) = caller_scope.cache_identity() else {
        return;
    };
    search_cache().put_partitioned(
        caller_identity,
        cache_key,
        value,
        SEARCH_CACHE_MAX_ENTRIES_PER_CALLER,
    );
}

fn should_emit_index_scope_warning(
    resolved_project_id: Option<Uuid>,
    indexed_root: &str,
    local_folder: &str,
) -> bool {
    let key = format!(
        "{}|{}|{}",
        resolved_project_id
            .map(|id| id.to_string())
            .unwrap_or_default(),
        indexed_root,
        local_folder
    );
    let warnings = INDEX_SCOPE_WARNINGS.get_or_init(|| Mutex::new(HashSet::new()));
    let mut guard = warnings.lock().unwrap();
    guard.insert(key)
}

/// Small, dependency-free SHA-256 used for cache identities. These digests are
/// not authentication primitives; SHA-256 is used here because it is stable
/// across processes and makes opaque cursors/handles safe to include without
/// copying their contents into logs or cache keys.
pub(super) fn sha256_hex_bytes(input: &[u8]) -> String {
    const INITIAL: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    const ROUND: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let bit_len = (input.len() as u64).wrapping_mul(8);
    let mut padded = input.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    let mut state = INITIAL;
    for chunk in padded.chunks_exact(64) {
        let mut schedule = [0u32; 64];
        for (index, word) in chunk.chunks_exact(4).enumerate() {
            schedule[index] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for index in 16..64 {
            let s0 = schedule[index - 15].rotate_right(7)
                ^ schedule[index - 15].rotate_right(18)
                ^ (schedule[index - 15] >> 3);
            let s1 = schedule[index - 2].rotate_right(17)
                ^ schedule[index - 2].rotate_right(19)
                ^ (schedule[index - 2] >> 10);
            schedule[index] = schedule[index - 16]
                .wrapping_add(s0)
                .wrapping_add(schedule[index - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for index in 0..64 {
            let sum1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choose = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(sum1)
                .wrapping_add(choose)
                .wrapping_add(ROUND[index])
                .wrapping_add(schedule[index]);
            let sum0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = sum0.wrapping_add(majority);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        for (slot, value) in state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }

    state.iter().map(|word| format!("{word:08x}")).collect()
}

fn append_cache_field(buffer: &mut Vec<u8>, name: &str, value: Option<&[u8]>) {
    buffer.extend_from_slice(&(name.len() as u32).to_be_bytes());
    buffer.extend_from_slice(name.as_bytes());
    match value {
        Some(value) => {
            buffer.push(1);
            buffer.extend_from_slice(&(value.len() as u64).to_be_bytes());
            buffer.extend_from_slice(value);
        }
        None => buffer.push(0),
    }
}

fn append_cache_text(buffer: &mut Vec<u8>, name: &str, value: Option<&str>) {
    append_cache_field(buffer, name, value.map(str::as_bytes));
}

#[derive(Debug, Clone)]
struct ResolvedSearchCacheShapers {
    limit: Option<i64>,
    offset: Option<i64>,
    file_types: Vec<String>,
    include_content: Option<bool>,
    include_memory: bool,
    include_vcs: bool,
    output_format: Option<String>,
    context_lines: Option<i64>,
    content_max_chars: i64,
    exact_match_boost: Option<f64>,
    hot_paths_identity: Option<String>,
}

fn hot_paths_cache_identity(hint: Option<&HotPathsHint>) -> Option<String> {
    let hint = hint?;
    let mut canonical = Vec::new();
    append_cache_field(
        &mut canonical,
        "profile_version",
        Some(&hint.profile_version.to_be_bytes()),
    );
    // The ordered path set is the response-shaping identity. Raw activity
    // scores and confidence can change continuously, but tiny magnitude
    // changes do not justify invalidating an otherwise identical warm search
    // result. Membership or rank-order changes still produce a new key.
    for entry in &hint.entries {
        append_cache_text(&mut canonical, "path", Some(&entry.path));
        append_cache_text(&mut canonical, "source", Some(&entry.source));
    }
    Some(sha256_hex_bytes(&canonical))
}

/// Build a canonical cache key for a `search()` call. Scope + every resolved
/// response/ranking-shaping input + index freshness uniquely identifies a
/// cacheable result within the warm window. Length framing prevents delimiter
/// ambiguity and the final SHA-256 digest keeps queries/cursors out of logs.
/// Side-effect-only `code_rerank_learning_opt_in` is intentionally excluded.
fn build_search_cache_key_with_tokenizer(
    workspace_id: Option<Uuid>,
    explicit_project_id: Option<Uuid>,
    input: &SearchInput,
    mode: SearchMode,
    resolved_guided_grounding_handle: Option<&str>,
    shapers: &ResolvedSearchCacheShapers,
    index_freshness: Option<chrono::DateTime<chrono::Utc>>,
    checkout_identity: Option<&str>,
    tokenizer_cache_namespace: &str,
) -> String {
    let workspace = workspace_id.map(|id| id.to_string());
    let project = explicit_project_id.map(|id| id.to_string());
    let limit = shapers.limit.map(|value| value.to_string());
    let offset = shapers.offset.map(|value| value.to_string());
    let context_lines = shapers.context_lines.map(|value| value.to_string());
    let content_max_chars = shapers.content_max_chars.to_string();
    let exact_match_boost = shapers
        .exact_match_boost
        .map(|value| format!("{:016x}", value.to_bits()));
    let index_freshness = index_freshness.map(|value| value.timestamp_millis().to_string());
    let cursor_digest = input
        .cursor
        .as_deref()
        .map(|cursor| sha256_hex_bytes(cursor.as_bytes()));
    let guided_grounding_handle_digest = (mode == SearchMode::Guided)
        .then(|| resolved_guided_grounding_handle.map(|handle| sha256_hex_bytes(handle.as_bytes())))
        .flatten();

    let mut file_types = shapers.file_types.clone();
    file_types.sort();
    file_types.dedup();
    let mut framed_file_types = Vec::new();
    for file_type in &file_types {
        append_cache_text(&mut framed_file_types, "item", Some(file_type));
    }

    let mut framed_vector = Vec::new();
    if let Some(vector) = input.query_vector.as_ref() {
        for value in vector {
            append_cache_field(
                &mut framed_vector,
                "f32",
                Some(&value.to_bits().to_be_bytes()),
            );
        }
    }

    let mut canonical = Vec::new();
    append_cache_text(&mut canonical, "version", Some("search-cache-v5"));
    append_cache_text(&mut canonical, "workspace_id", workspace.as_deref());
    append_cache_text(&mut canonical, "project_id", project.as_deref());
    append_cache_text(&mut canonical, "query", Some(&input.query));
    append_cache_text(&mut canonical, "intent", input.intent.as_deref());
    append_cache_text(&mut canonical, "mode", Some(mode.as_str()));
    append_cache_text(
        &mut canonical,
        "tokenizer_cache_namespace",
        Some(tokenizer_cache_namespace),
    );
    append_cache_text(&mut canonical, "limit", limit.as_deref());
    append_cache_text(&mut canonical, "offset", offset.as_deref());
    append_cache_text(&mut canonical, "cursor_sha256", cursor_digest.as_deref());
    append_cache_text(
        &mut canonical,
        "guided_grounding_handle_sha256",
        guided_grounding_handle_digest.as_deref(),
    );
    append_cache_field(
        &mut canonical,
        "file_types",
        (!framed_file_types.is_empty()).then_some(framed_file_types.as_slice()),
    );
    append_cache_text(
        &mut canonical,
        "include_content",
        shapers
            .include_content
            .map(|value| if value { "1" } else { "0" }),
    );
    append_cache_text(
        &mut canonical,
        "include_memory",
        Some(if shapers.include_memory { "1" } else { "0" }),
    );
    append_cache_text(
        &mut canonical,
        "include_vcs",
        Some(if shapers.include_vcs { "1" } else { "0" }),
    );
    append_cache_text(
        &mut canonical,
        "output_format",
        shapers.output_format.as_deref(),
    );
    append_cache_text(&mut canonical, "context_lines", context_lines.as_deref());
    append_cache_text(
        &mut canonical,
        "content_max_chars",
        Some(&content_max_chars),
    );
    append_cache_text(
        &mut canonical,
        "exact_match_boost_bits",
        exact_match_boost.as_deref(),
    );
    append_cache_text(
        &mut canonical,
        "hot_paths_identity",
        shapers.hot_paths_identity.as_deref(),
    );
    append_cache_field(
        &mut canonical,
        "query_vector_bits",
        input
            .query_vector
            .as_ref()
            .map(|_| framed_vector.as_slice()),
    );
    append_cache_text(
        &mut canonical,
        "index_freshness_ms",
        index_freshness.as_deref(),
    );
    append_cache_text(&mut canonical, "checkout_identity", checkout_identity);

    format!("search:v5:{}", sha256_hex_bytes(&canonical))
}

#[cfg(test)]
fn build_search_cache_key(
    workspace_id: Option<Uuid>,
    explicit_project_id: Option<Uuid>,
    input: &SearchInput,
    mode: SearchMode,
    resolved_guided_grounding_handle: Option<&str>,
    shapers: &ResolvedSearchCacheShapers,
    index_freshness: Option<chrono::DateTime<chrono::Utc>>,
    checkout_identity: Option<&str>,
) -> String {
    build_search_cache_key_with_tokenizer(
        workspace_id,
        explicit_project_id,
        input,
        mode,
        resolved_guided_grounding_handle,
        shapers,
        index_freshness,
        checkout_identity,
        "test-wire-tokenizer-namespace",
    )
}

fn caller_scoped_search_cache_key(base_key: &str, caller_identity: &str) -> String {
    let mut canonical = Vec::new();
    append_cache_text(&mut canonical, "base_key", Some(base_key));
    append_cache_text(&mut canonical, "caller_identity", Some(caller_identity));
    format!("search-caller:v2:{}", sha256_hex_bytes(&canonical))
}

fn local_checkout_cache_identity(
    folder_path: Option<&str>,
    repository: Option<&str>,
    branch: Option<&str>,
    commit_sha: Option<&str>,
) -> Option<String> {
    let folder_path = folder_path?;
    let mut canonical = Vec::new();
    append_cache_text(&mut canonical, "folder_path", Some(folder_path));
    append_cache_text(&mut canonical, "repository", repository);
    append_cache_text(&mut canonical, "branch", branch);
    append_cache_text(&mut canonical, "commit_sha", commit_sha);
    Some(sha256_hex_bytes(&canonical))
}

fn effective_search_cache_project_id(
    explicit_project_id: Option<Uuid>,
    resolved_folder_project_id: Option<Uuid>,
    local_index_project_id: Option<Uuid>,
    resolved_project_id: Option<Uuid>,
) -> Option<Uuid> {
    explicit_project_id
        .or(resolved_folder_project_id)
        .or(local_index_project_id)
        .or(resolved_project_id)
}

/// Resolve a project for the optional pre-search drift ingest.
///
/// Content writes require machine-local evidence (a checkout-bound folder
/// mapping or the local index ledger), and every other active scope must agree
/// with it. Search itself may still surface a routing warning/fallback, but an
/// ambiguous read must never trigger file uploads or deletion deltas.
fn drift_ingest_project_id(
    explicit_project_id: Option<Uuid>,
    resolved_folder_project_id: Option<Uuid>,
    local_index_project_id: Option<Uuid>,
    session_project_id: Option<Uuid>,
) -> Option<Uuid> {
    let anchored = match (resolved_folder_project_id, local_index_project_id) {
        (Some(folder), Some(indexed)) if folder != indexed => return None,
        (Some(folder), _) => folder,
        (_, Some(indexed)) => indexed,
        (None, None) => return None,
    };
    if [explicit_project_id, session_project_id]
        .into_iter()
        .flatten()
        .any(|candidate| candidate != anchored)
    {
        return None;
    }
    Some(anchored)
}

/// Return the exact workspace authorized for a local content write.
///
/// Every automatic search-side ingest must prove the checkout-local
/// root/project/workspace binding and agreement with the active workspace.
/// The ingest POST carries this exact workspace as a required expected-scope
/// assertion; the server validates it atomically with the content mutation,
/// avoiding a raceable preflight GET on the first-answer path.
async fn validated_checkout_content_workspace(
    _client: &ContextStreamClient,
    folder_path: &str,
    workspace_id: Option<Uuid>,
    project_id: Uuid,
    operation: &'static str,
) -> Option<Uuid> {
    let Some(bound_workspace_id) =
        mcp_session::auto_init::checkout_binding_workspace(folder_path, project_id)
    else {
        tracing::warn!(
            operation,
            path = %folder_path,
            project_id = %project_id,
            "automatic search ingest skipped because the checkout binding is missing or invalid"
        );
        return None;
    };
    if workspace_id.is_some_and(|hinted| hinted != bound_workspace_id) {
        tracing::warn!(
            operation,
            path = %folder_path,
            project_id = %project_id,
            workspace_id = %bound_workspace_id,
            "automatic search ingest skipped because workspace scopes conflict"
        );
        return None;
    }
    Some(bound_workspace_id)
}

/// Whether a `search()` result may be served from / written to the per-process
/// result cache for this call.
///
/// Workspace-scoped (no folder) searches are always cacheable. Folder-scoped
/// searches — the developer-in-a-checkout case — are cacheable only when the
/// working tree is in sync with the index (`!folder_has_drift`, i.e. no tracked
/// files newer than the indexed snapshot), so an identical repeat query never
/// replays stale snippets after a local edit. Combined with keying the cache on
/// the index `indexed_at` (see `build_search_cache_key`), a re-ingest also
/// invalidates older entries. Previously folder-scoped searches bypassed the
/// cache entirely and re-paid the network round-trip on every identical call.
fn should_use_search_cache(
    session_folder_path: Option<&str>,
    folder_has_drift: bool,
    code_rerank_learning_opt_in: bool,
) -> bool {
    if code_rerank_learning_opt_in {
        return false;
    }
    match session_folder_path {
        None => true,
        Some(_) => !folder_has_drift,
    }
}

/// Wrap a hybrid call with HYBRID_FAST_FALLBACK; on timeout or 5xx-shaped
/// failure, fall back to semantic with a fresh budget so the caller still
/// gets a meaningful response set.
async fn hybrid_with_fast_fallback(
    client: &ContextStreamClient,
    params: SearchParams,
) -> Result<(SearchResponse, Option<String>, Option<Uuid>)> {
    match tokio::time::timeout(
        HYBRID_FAST_FALLBACK,
        execute_api_search_attempt(client, SearchMode::Hybrid, params.clone()),
    )
    .await
    {
        Ok(Ok(attempt)) => Ok((attempt.response, None, attempt.learning_request_id)),
        Ok(Err(err)) => {
            // Server-side error: try semantic so the caller isn't empty-handed.
            tracing::warn!(
                error = %err,
                "Hybrid search failed; falling back to semantic"
            );
            let attempt = execute_api_search_attempt(client, SearchMode::Semantic, params).await?;
            Ok((
                attempt.response,
                Some(
                    "Hybrid search returned an error; semantic results shown instead.".to_string(),
                ),
                attempt.learning_request_id,
            ))
        }
        Err(_) => {
            tracing::warn!(
                timeout_secs = HYBRID_FAST_FALLBACK.as_secs(),
                "Hybrid search exceeded fast-fallback budget; falling back to semantic"
            );
            let attempt = execute_api_search_attempt(client, SearchMode::Semantic, params).await?;
            Ok((
                attempt.response,
                Some(format!(
                    "Hybrid search exceeded {}s; semantic results shown instead.",
                    HYBRID_FAST_FALLBACK.as_secs()
                )),
                attempt.learning_request_id,
            ))
        }
    }
}

use crate::atlas_flags::{gate_decision, AtlasProductGate};
use crate::domains::scope::{is_project_scope_error, resolve_read_scope};
use crate::registry::ToolHandler;
use crate::schema::SchemaBuilder;

/// Maximum number of workspace projects to include when expanding team/cross-project fallback.
const TEAM_PROJECT_FALLBACK_LIMIT: usize = 12;

/// Maximum age for local dirty-file hints before they are ignored.
const DIRTY_FILE_RETENTION_HOURS: i64 = 12;

/// Cap dirty-file hints surfaced to the model to keep context compact.
const DIRTY_FILE_DISPLAY_LIMIT: usize = 12;

/// Maximum number of locally-changed files for which a drift-triggered
/// re-index runs synchronously (blocking the current search) so results
/// reflect the edits. Larger drift falls through to the background path.
const DRIFT_SYNC_MAX_FILES: usize = 10;
/// Raw local bytes synchronously read for drift repair before retrieval. This
/// keeps first-answer latency and memory bounded even when the changed files
/// themselves are large.
const DRIFT_SYNC_MAX_BYTES: usize = 1024 * 1024;
const DRIFT_BACKGROUND_BATCH_MAX_FILES: usize = 64;
const DRIFT_BACKGROUND_BATCH_MAX_BYTES: usize = 8 * 1024 * 1024;
const DRIFT_BACKGROUND_TOTAL_MAX_FILES: usize = 512;

/// Upper bound on how long a synchronous drift re-index may block the current
/// search before falling back to the background re-index + advisory path.
/// Upper bound for active-project preflight repair. The search path should wait
/// briefly for hotness, then continue with local enrichment and background
/// maintenance if the repair is still running.
const ACTIVE_INDEX_PREFLIGHT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(750);
const ACTIVE_INDEX_PREFLIGHT_MAX_FILES: usize = 20_000;
const ACTIVE_INDEX_IN_PROGRESS_GRACE_SECS: i64 = 15;
const HOT_PATH_HINT_LIMIT: usize = 8;
// v2 intentionally stops treating unvalidated search results as activity.
// Only live editor/dirty-file evidence may shape this advisory hint.
const HOT_PATH_PROFILE_VERSION: u32 = 2;
/// Graph enrichment is supplemental context for search results. Keep it on a
/// strict budget so a slow graph backend never blocks the primary search tool.
// Graph context is optional enrichment, never a prerequisite for returning
// ranked code evidence. Regional production showed unavailable graph calls
// consistently consuming the old 1.5s ceiling and dominating search P95.
// Keep a small warm-cache window, then return the core answer immediately.
const GRAPH_ENRICHMENT_TIMEOUT: Duration = Duration::from_millis(250);

const VCS_QUERY_SIGNALS: &[&str] = &[
    "pull request",
    "merge request",
    "pr #",
    "mr #",
    "issue #",
    "commit",
    "branch",
    "tag",
    "diff",
    "review",
    "merge",
    "rebase",
    "checkout",
    "push",
    "remote",
    "upstream",
    "downstream",
    "fork",
    "clone",
    "github",
    "gitlab",
    "bitbucket",
    "repo",
    "repository",
];

fn query_has_vcs_signal(query: &str) -> bool {
    let lower = query.to_ascii_lowercase();
    VCS_QUERY_SIGNALS
        .iter()
        .any(|signal| lower.contains(signal))
}

fn extract_vcs_search_items(value: &serde_json::Value) -> Vec<serde_json::Value> {
    if let Some(arr) = value.as_array() {
        return arr.clone();
    }
    for key in &["data", "items", "results"] {
        if let Some(arr) = value.get(*key).and_then(|v| v.as_array()) {
            return arr.clone();
        }
    }
    Vec::new()
}

fn fetch_project_map_route_hint(
    client: &ContextStreamClient,
    project_id: Option<Uuid>,
    query: &str,
) -> Option<ProjectAgentMapRouteHint> {
    let project_id = project_id?;
    // Init prewarms this intelligence. A cold/missing map must never turn
    // best-effort routing into a synchronous search dependency.
    let response = client.cached_project_agent_map(project_id)?;

    if matches!(
        response.status.as_str(),
        "unavailable" | "building" | "failed"
    ) {
        return None;
    }

    response
        .route_hint
        .clone()
        .and_then(|route| filter_project_map_route_hint(route, query))
        .or_else(|| project_map_route_hint_from_structured(&response, query))
}

fn project_map_route_hint_from_structured(
    response: &ProjectAgentMapResponse,
    query: &str,
) -> Option<ProjectAgentMapRouteHint> {
    let routes = response
        .structured_json
        .get("search_routes")
        .and_then(Value::as_array)?;
    let query_tokens = project_map_route_query_tokens(query);
    if query_tokens.is_empty() {
        return None;
    }
    let mut best: Option<(usize, &Value)> = None;

    for route in routes {
        let mut route_text = Vec::new();
        if let Some(title) = route.get("title").and_then(Value::as_str) {
            route_text.push(title);
        }
        if let Some(paths) = route.get("paths").and_then(Value::as_array) {
            route_text.extend(paths.iter().filter_map(Value::as_str));
        }
        if let Some(suggested_queries) = route.get("suggested_queries").and_then(Value::as_array) {
            route_text.extend(suggested_queries.iter().filter_map(Value::as_str));
        }
        let weak_score = project_map_route_match_score(&query_tokens, route_text);
        let keyword_score = route
            .get("keywords")
            .and_then(Value::as_array)
            .map(|keywords| {
                keywords
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|keyword| {
                        let keyword_tokens = project_map_route_query_tokens(keyword);
                        !keyword_tokens.is_empty()
                            && keyword_tokens
                                .iter()
                                .all(|token| query_tokens.contains(token))
                    })
                    .count()
                    * 3
            })
            .unwrap_or_default();
        let score = weak_score + keyword_score;
        if score < project_map_route_min_match_score(query) {
            continue;
        }

        match best {
            Some((best_score, _)) if best_score >= score => {}
            _ => best = Some((score, route)),
        }
    }

    let (_score, route) = best?;
    let include_artifacts = query_explicitly_targets_artifacts(query);
    let paths = route
        .get("paths")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .filter(|path| include_artifacts || !is_artifact_like_path(path))
                .take(6)
                .map(str::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if paths.is_empty() {
        return None;
    }

    Some(ProjectAgentMapRouteHint {
        title: route
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("Project route")
            .to_string(),
        reason: "Matched query tokens against the prewarmed project map route index.".to_string(),
        paths,
        suggested_queries: route
            .get("suggested_queries")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .take(3)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default(),
        generated_at: response.generated_at.clone(),
        stale: response.stale,
    })
}

fn filter_project_map_route_hint(
    mut route: ProjectAgentMapRouteHint,
    query: &str,
) -> Option<ProjectAgentMapRouteHint> {
    if !query_explicitly_targets_artifacts(query) {
        route.paths.retain(|path| !is_artifact_like_path(path));
    }

    if route.paths.is_empty() {
        return None;
    }

    if !project_map_route_hint_matches_query(&route, query) {
        return None;
    }

    Some(route)
}

fn project_map_route_hint_matches_query(route: &ProjectAgentMapRouteHint, query: &str) -> bool {
    let tokens = project_map_route_query_tokens(query);
    if tokens.is_empty() {
        return false;
    }

    project_map_route_match_score(
        &tokens,
        std::iter::once(route.title.as_str())
            .chain(std::iter::once(route.reason.as_str()))
            .chain(route.paths.iter().map(String::as_str))
            .chain(route.suggested_queries.iter().map(String::as_str)),
    ) >= project_map_route_min_match_score(query)
}

/// Mirror the API project-map specificity floor. A single weak title/path
/// overlap is useful for a short lookup, but should not steer a multiword task
/// unless it has a real keyword match or several independent weak signals.
fn project_map_route_min_match_score(query: &str) -> usize {
    if query.split_whitespace().count() <= 2 {
        1
    } else {
        3
    }
}

fn project_map_route_match_score<'a, I>(tokens: &[String], texts: I) -> usize
where
    I: IntoIterator<Item = &'a str>,
{
    let mut haystack_tokens = texts
        .into_iter()
        .flat_map(local_snippet_query_tokens)
        .collect::<Vec<_>>();
    haystack_tokens.sort();
    haystack_tokens.dedup();

    tokens.iter().fold(0usize, |score, token| {
        if haystack_tokens.binary_search(token).is_ok() {
            score
                + if token.contains('_') || token.contains('/') || token.len() >= 5 {
                    2
                } else {
                    1
                }
        } else {
            score
        }
    })
}

fn project_map_route_query_tokens(query: &str) -> Vec<String> {
    const STOPWORDS: &[&str] = &[
        "and", "app", "code", "file", "files", "find", "for", "from", "how", "into", "lib",
        "project", "query", "route", "search", "src", "that", "the", "this", "what", "where",
        "why", "with",
    ];

    local_snippet_query_tokens(query)
        .into_iter()
        .filter(|token| {
            (token.len() >= 3 || token.contains('_') || token.contains('/'))
                && !STOPWORDS.contains(&token.as_str())
        })
        .collect()
}

fn should_fetch_project_map_route_hint(
    query: &str,
    output_format: Option<&str>,
    result: &SearchResponse,
) -> bool {
    if output_format.is_some_and(|format| format.eq_ignore_ascii_case("count"))
        || result.results.is_empty()
        || is_identifier_query(query)
        || looks_like_symbol_anchor_query(query)
    {
        return false;
    }

    // A map route is useful for a substantive task/concept query. Tiny
    // lookups almost never match a route and should not pay the map assembly
    // cost just to return `None`.
    project_map_route_query_tokens(query).len() >= 2
}

fn should_fetch_graph_enrichment(query: &str, count_only_output: bool, no_hits: bool) -> bool {
    !count_only_output
        && !no_hits
        && !is_identifier_query(query)
        && !looks_like_symbol_anchor_query(query)
}

fn format_project_map_route_hint(route: &ProjectAgentMapRouteHint) -> String {
    let mut text = format!(
        "[PROJECT_MAP_ROUTE] {}{}.\n",
        route.title,
        if route.stale && is_debug_enabled() {
            " (stale)"
        } else {
            ""
        }
    );
    if !route.paths.is_empty() {
        text.push_str(&format!(
            "Start with: {}.\n",
            route
                .paths
                .iter()
                .take(6)
                .map(|path| format!("`{}`", path))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !route.suggested_queries.is_empty() {
        text.push_str(&format!(
            "Suggested query: `{}`.\n",
            route.suggested_queries[0]
        ));
    }
    text
}

fn concise_tool_text_enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| match std::env::var("CONTEXTSTREAM_CONCISE_TOOL_TEXT") {
        Ok(value) => !matches!(
            value.to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off"
        ),
        // Default ON so users don't get flooded with tool chatter.
        Err(_) => true,
    })
}

fn build_hot_paths_hint(query: &str, active_paths: &[String]) -> Option<HotPathsHint> {
    // Search results are hypotheses, not user activity. Feeding them back into
    // the next request created a self-reinforcing ranking loop and changed the
    // cache key after every call. Use only current editor/dirty-file evidence.
    let mut merged: HashMap<String, (f64, String)> = HashMap::new();
    for path in active_paths {
        let normalized = path.trim().replace('\\', "/");
        if normalized.is_empty() {
            continue;
        }
        merged
            .entry(normalized)
            .or_insert((0.9, "active".to_string()));
    }

    let mut entries: Vec<HotPathHintEntry> = merged
        .into_iter()
        .map(|(path, (score, source))| HotPathHintEntry {
            path,
            score,
            source,
        })
        .collect();
    entries.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.path.cmp(&b.path))
            .then_with(|| a.source.cmp(&b.source))
    });
    entries.truncate(HOT_PATH_HINT_LIMIT);
    if entries.is_empty() {
        return None;
    }

    let broad_query = {
        let trimmed = query.trim();
        trimmed.len() <= 2 || trimmed.split_whitespace().count() > 14
    };
    let raw_confidence =
        entries.iter().map(|entry| entry.score).sum::<f64>() / (HOT_PATH_HINT_LIMIT as f64);
    let confidence = raw_confidence.clamp(0.0, 1.0) * if broad_query { 0.55 } else { 1.0 };

    Some(HotPathsHint {
        entries,
        confidence,
        generated_at: chrono::Utc::now().to_rfc3339(),
        profile_version: HOT_PATH_PROFILE_VERSION,
    })
}

/// Search mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchMode {
    #[default]
    Hybrid,
    Semantic,
    Keyword,
    Pattern,
    Exhaustive,
    Refactor,
    Team,
    Crawl,
    /// One-call agent navigation: bounded code/memory retrieval plus optional
    /// fast-model guidance, always returned alongside raw evidence.
    Guided,
    /// Legacy fuzzy / typo-tolerant mode. It is explicit-only and never
    /// auto-selected. When no compatibility provider is available, the request
    /// falls through to keyword search with a degradation note.
    Fuzzy,
    /// Legacy vector-search mode. It accepts a caller-supplied `query_vector`.
    /// Metadata
    /// filters are parsed out of the query string — `branch:main`,
    /// `lang:rust`, `recent:7d`, `project:<uuid>`, `path:src/…`.
    Vector,
}

impl SearchMode {
    pub fn from_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "semantic" => Self::Semantic,
            "keyword" | "text" => Self::Keyword,
            "pattern" | "regex" => Self::Pattern,
            "exhaustive" => Self::Exhaustive,
            "refactor" => Self::Refactor,
            "team" => Self::Team,
            "crawl" | "deep" => Self::Crawl,
            "guided" | "navigate" => Self::Guided,
            "fuzzy" | "typo" => Self::Fuzzy,
            "vector" | "knn" => Self::Vector,
            _ => Self::Hybrid,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Hybrid => "hybrid",
            Self::Semantic => "semantic",
            Self::Keyword => "keyword",
            Self::Pattern => "pattern",
            Self::Exhaustive => "exhaustive",
            Self::Refactor => "refactor",
            Self::Team => "team",
            Self::Crawl => "crawl",
            Self::Guided => "guided",
            Self::Fuzzy => "fuzzy",
            Self::Vector => "vector",
        }
    }
}

/// Prefixes that indicate a count query.
const COUNT_QUERY_PREFIXES: &[&str] = &["how many ", "count ", "count of ", "number of ", "total "];

/// Phrases that indicate all-occurrences intent.
const ALL_MATCH_KEYWORDS: &[&str] = &[
    "all occurrences",
    "all matches",
    "find all",
    "every usage",
    "every occurrence",
    "all usages",
];

/// Phrases that indicate team search intent.
const TEAM_QUERY_KEYWORDS: &[&str] = &[
    "team-wide",
    "teamwide",
    "cross-project",
    "cross project",
    "across projects",
    "all workspaces",
    "all projects",
];

/// Terms that usually indicate a docs lookup request (not codebase search).
const DOC_QUERY_KEYWORDS: &[&str] = &[
    "doc",
    "docs",
    "document",
    "documents",
    "spec",
    "specification",
    "plan",
    "roadmap",
];

/// Verbs that usually indicate listing/finding docs.
const DOC_LOOKUP_VERBS: &[&str] = &[
    "list", "show", "find", "open", "read", "lookup", "look up", "get",
];

/// Leading words that typically indicate semantic/natural-language search.
const QUESTION_WORDS: &[&str] = &[
    "how", "what", "where", "why", "when", "which", "who", "does", "is", "can", "should",
];

/// Common natural-language words that should not be treated as symbol anchors.
const SYMBOL_ANCHOR_STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "that", "this", "from", "into", "where", "which", "what", "when",
    "does", "how", "bug", "fixed", "tests", "test", "code", "file",
];

/// Path fragments that usually indicate generated/docs/build artifacts.
const SYMBOL_NOISE_PATH_TERMS: &[&str] = &[
    "/openapi/",
    "/essentials/",
    "/generated/",
    "/gen/",
    "/dist/",
    "/build/",
    "/target/",
    "/node_modules/",
    "/coverage/",
    "/vendor/",
    "/docs/",
    "/.next/",
    "/.next.bak",
];

/// Top-score threshold under which hybrid results are considered weak for NL queries.
const HYBRID_LOW_CONFIDENCE_SCORE: f64 = 0.55;

/// Minimum top-score improvement required to prefer semantic fallback over hybrid.
const SEMANTIC_SWITCH_MIN_IMPROVEMENT: f64 = 0.02;

/// Hard cap for mode-escalation retries performed after initial search.
const MAX_MODE_ESCALATION_ATTEMPTS: usize = 2;

/// Default hard character budget for rendered search text output.
///
/// High-volume modes (exhaustive can return up to 300 server-side rows) must
/// never blow past harness tool-result token caps — Claude Code, for example,
/// rejects oversized tool results, dumps them to a file, and derails the agent
/// into a subagent-read loop. Observed failure: one auto-escalated exhaustive
/// call rendered 271 rows / 61,739 chars.
const SEARCH_TEXT_OUTPUT_BUDGET_DEFAULT: usize = 24_000;
const SEARCH_STRUCTURED_OUTPUT_BUDGET_DEFAULT: usize = 24_000;
/// Final serialized `ToolResult` envelope for search. This is deliberately a
/// byte proxy, not a tokenizer claim: the exact model transport/tokenizer is
/// still enforced by the staged tokenizer work. The proxy nevertheless stops
/// text + duplicated structured content from jointly escaping their budgets.
const SEARCH_TOOL_RESULT_WIRE_BUDGET_DEFAULT: usize = 32_000;
/// The minimum must carry the largest backend-minted rf2 cursor (5,654 bytes)
/// plus mandatory trust/scope controls and a small actionable text envelope.
const SEARCH_TOOL_RESULT_WIRE_BUDGET_MIN: usize = 12_000;
const SEARCH_TOOL_RESULT_WIRE_BUDGET_MAX: usize = 96_000;
const SEARCH_STRUCTURED_OUTPUT_BUDGET_MIN: usize = 10_000;
/// Exact maximum emitted by the backend's compact rf2 producer: a 4,204-byte
/// binary payload (including a 4,096-byte keyset path) becomes 5,606 bytes of
/// unpadded base64url, plus `rf2.`, a separator, and a 43-byte HMAC signature.
/// Keep aligned with ContextStream API `REFACTOR_CURSOR_MAX_BYTES` and its
/// producer/request boundary test.
const MAX_VALID_SEARCH_CURSOR_BYTES: usize = 5_654;

/// Resolve the rendered-text budget, overridable via
/// `CONTEXTSTREAM_SEARCH_MAX_OUTPUT_CHARS` (clamped to 4_000..=200_000).
fn search_text_output_budget() -> usize {
    std::env::var("CONTEXTSTREAM_SEARCH_MAX_OUTPUT_CHARS")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .map(|value| value.clamp(4_000, 200_000))
        .unwrap_or(SEARCH_TEXT_OUTPUT_BUDGET_DEFAULT)
}

/// Structured content is billed to the same agent context window as text, so
/// it needs its own hard envelope. Keep the default aligned with rendered text
/// while capping the override below the historical 200k harness ceiling.
fn search_structured_output_budget() -> usize {
    std::env::var("CONTEXTSTREAM_SEARCH_MAX_STRUCTURED_BYTES")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .map(|value| value.clamp(SEARCH_STRUCTURED_OUTPUT_BUDGET_MIN, 64_000))
        .unwrap_or(SEARCH_STRUCTURED_OUTPUT_BUDGET_DEFAULT)
}

fn search_tool_result_wire_budget() -> usize {
    std::env::var("CONTEXTSTREAM_SEARCH_MAX_WIRE_BYTES")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .map(|value| {
            value.clamp(
                SEARCH_TOOL_RESULT_WIRE_BUDGET_MIN,
                SEARCH_TOOL_RESULT_WIRE_BUDGET_MAX,
            )
        })
        .unwrap_or(SEARCH_TOOL_RESULT_WIRE_BUDGET_DEFAULT)
}

/// Count the exact JSON bytes without materializing a second serialized copy.
/// When a limit is supplied, serialization stops as soon as the limit is
/// crossed; this keeps adversarial multi-megabyte diagnostics from being
/// traversed repeatedly merely to learn that they do not fit.
struct CappedCountingWriter {
    bytes: usize,
    limit: usize,
    exceeded: bool,
}

impl CappedCountingWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: 0,
            limit,
            exceeded: false,
        }
    }
}

impl Write for CappedCountingWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        if buffer.len() > self.limit.saturating_sub(self.bytes) {
            self.bytes = self.limit.saturating_add(1);
            self.exceeded = true;
            return Err(io::Error::other(
                "serialized search payload exceeded byte cap",
            ));
        }
        self.bytes = self.bytes.saturating_add(buffer.len());
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
struct SerializedSize {
    bytes: usize,
    is_lower_bound: bool,
}

#[derive(Debug, Default, Clone, Copy)]
struct SerializationBudgetStats {
    attempts: usize,
    early_aborts: usize,
}

fn serialized_size_up_to<T: Serialize>(value: &T, limit: usize) -> SerializedSize {
    let mut writer = CappedCountingWriter::new(limit);
    match serde_json::to_writer(&mut writer, value) {
        Ok(()) => SerializedSize {
            bytes: writer.bytes,
            is_lower_bound: false,
        },
        Err(_) if writer.exceeded => SerializedSize {
            bytes: limit.saturating_add(1),
            is_lower_bound: true,
        },
        Err(_) => SerializedSize {
            bytes: usize::MAX,
            is_lower_bound: false,
        },
    }
}

fn serialized_json_bytes(value: &Value) -> usize {
    serialized_size_up_to(value, usize::MAX).bytes
}

fn serialized_json_bytes_counted(value: &Value, stats: &mut SerializationBudgetStats) -> usize {
    stats.attempts = stats.attempts.saturating_add(1);
    serialized_json_bytes(value)
}

fn serialized_json_size_capped_counted(
    value: &Value,
    limit: usize,
    stats: &mut SerializationBudgetStats,
) -> SerializedSize {
    stats.attempts = stats.attempts.saturating_add(1);
    let size = serialized_size_up_to(value, limit);
    if size.is_lower_bound {
        stats.early_aborts = stats.early_aborts.saturating_add(1);
    }
    size
}

fn truncate_json_string(value: &str, max_chars: usize) -> String {
    let Some((byte_index, _)) = value.char_indices().nth(max_chars) else {
        return value.to_string();
    };
    let mut truncated = value[..byte_index].to_string();
    truncated.push('…');
    truncated
}

fn bounded_scalar(value: &Value, max_string_chars: usize) -> Option<Value> {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) => Some(value.clone()),
        Value::String(text) => Some(Value::String(truncate_json_string(text, max_string_chars))),
        Value::Array(_) | Value::Object(_) => None,
    }
}

fn compact_object_fields(
    value: &Value,
    fields: &[(&str, usize)],
) -> Option<serde_json::Map<String, Value>> {
    let source = value.as_object()?;
    let compact = fields
        .iter()
        .filter_map(|(field, max_chars)| {
            source
                .get(*field)
                .and_then(|value| bounded_scalar(value, *max_chars))
                .map(|value| ((*field).to_string(), value))
        })
        .collect::<serde_json::Map<_, _>>();
    (!compact.is_empty()).then_some(compact)
}

/// Trust and scope controls must survive hard compaction, but upstream prose
/// and arbitrary object keys must not be able to make those controls
/// unbounded. Keep only the fixed contract fields agents need to decide
/// whether the evidence is usable.
fn compact_fixed_control(field: &str, value: &Value) -> Value {
    match field {
        "index_trust" => {
            const ROOT: &[(&str, usize)] = &[
                ("project_id", 64),
                ("repository", 256),
                ("committed_generation", 32),
                ("indexed_at", 64),
                ("source_machine", 128),
                ("source_branch", 128),
                ("source_commit_sha", 128),
                ("result_generation_coverage_complete", 8),
                ("result_generation_consistent", 8),
            ];
            const LOCAL: &[(&str, usize)] = &[
                ("project_id", 64),
                ("repository", 256),
                ("branch", 128),
                ("commit_sha", 128),
                ("worktree_dirty", 8),
                ("drift", 8),
            ];
            const CHECKS: &[(&str, usize)] = &[
                ("resolved_project_match", 8),
                ("local_project_match", 8),
                ("repository_match", 8),
                ("branch_match", 8),
                ("commit_match", 8),
                ("generation_consistent", 8),
            ];
            let mut compact = compact_object_fields(value, ROOT).unwrap_or_default();
            if let Some(local) = value
                .get("local")
                .and_then(|value| compact_object_fields(value, LOCAL))
            {
                compact.insert("local".to_string(), Value::Object(local));
            }
            if let Some(checks) = value
                .get("checks")
                .and_then(|value| compact_object_fields(value, CHECKS))
            {
                compact.insert("checks".to_string(), Value::Object(checks));
            }
            Value::Object(compact)
        }
        "scope_reliability" => {
            const ROOT: &[(&str, usize)] = &[
                ("usable", 8),
                ("scope_match", 8),
                ("scope_invalid", 8),
                ("reason", 128),
            ];
            const REPAIR: &[(&str, usize)] = &[("attempted", 8), ("succeeded", 8), ("reason", 160)];
            let mut compact = compact_object_fields(value, ROOT).unwrap_or_default();
            if let Some(repair) = value
                .get("repair")
                .and_then(|value| compact_object_fields(value, REPAIR))
            {
                compact.insert("repair".to_string(), Value::Object(repair));
            }
            Value::Object(compact)
        }
        "scope_diagnostics" => {
            const FIELDS: &[(&str, usize)] = &[
                ("scope_valid", 8),
                ("scope_reason", 160),
                ("fallback_used", 8),
                ("fallback_reason", 160),
                ("project_index_state", 64),
                ("remediation_attempted", 8),
                ("remediation_note", 240),
            ];
            Value::Object(compact_object_fields(value, FIELDS).unwrap_or_default())
        }
        "index_health" => {
            const FIELDS: &[(&str, usize)] = &[
                ("freshness", 32),
                ("confidence", 32),
                ("age_hours", 32),
                ("scope_match", 8),
                ("drift_detected", 8),
                ("changed_file_count", 32),
                ("indexed_at", 64),
                ("recommendation", 240),
            ];
            Value::Object(compact_object_fields(value, FIELDS).unwrap_or_default())
        }
        _ => bounded_scalar(value, 512).unwrap_or(Value::Null),
    }
}

fn compact_essential_control(field: &str, value: &Value) -> Value {
    match field {
        "index_trust" => {
            const ROOT: &[(&str, usize)] = &[
                ("project_id", 64),
                ("committed_generation", 32),
                ("result_generation_coverage_complete", 8),
                ("result_generation_consistent", 8),
            ];
            const CHECKS: &[(&str, usize)] = &[
                ("resolved_project_match", 8),
                ("local_project_match", 8),
                ("repository_match", 8),
                ("branch_match", 8),
                ("commit_match", 8),
                ("generation_consistent", 8),
            ];
            let mut compact = compact_object_fields(value, ROOT).unwrap_or_default();
            if let Some(checks) = value
                .get("checks")
                .and_then(|value| compact_object_fields(value, CHECKS))
            {
                compact.insert("checks".to_string(), Value::Object(checks));
            }
            Value::Object(compact)
        }
        "scope_reliability" => compact_fixed_control(field, value),
        "scope_diagnostics" => {
            const FIELDS: &[(&str, usize)] = &[
                ("scope_valid", 8),
                ("fallback_used", 8),
                ("project_index_state", 64),
                ("remediation_attempted", 8),
            ];
            Value::Object(compact_object_fields(value, FIELDS).unwrap_or_default())
        }
        _ => compact_fixed_control(field, value),
    }
}

fn search_cursor_protocol_violation(cursor: &str) -> Option<&'static str> {
    if cursor.len() > MAX_VALID_SEARCH_CURSOR_BYTES {
        return Some("cursor_exceeds_max_bytes");
    }
    // Opaque cursors are transport tokens, not free-form prose. Reject JSON
    // escape-amplifying/control characters so the declared byte maximum also
    // bounds their serialized representation. Backend-minted signed `rf1` and
    // `rf2` base64url tokens remain valid.
    cursor
        .chars()
        .any(|character| character.is_control() || matches!(character, '"' | '\\'))
        .then_some("cursor_contains_invalid_transport_characters")
}

fn sanitize_search_continuation(value: &mut Value) {
    let Some(object) = value.as_object_mut() else {
        return;
    };
    let violation = match object.get("next_cursor") {
        Some(Value::String(cursor)) => {
            search_cursor_protocol_violation(cursor).map(|reason| (reason, cursor.len()))
        }
        Some(_) => Some(("cursor_must_be_a_string", 0)),
        None => None,
    };
    if let Some((reason, cursor_bytes)) = violation {
        object.remove("next_cursor");
        object.insert("continuation_unavailable".to_string(), Value::Bool(true));
        object.insert(
            "continuation_protocol_violation".to_string(),
            Value::String(reason.to_string()),
        );
        object.insert(
            "continuation_cursor_bytes".to_string(),
            serde_json::json!(cursor_bytes),
        );
        object.insert(
            "max_valid_cursor_bytes".to_string(),
            serde_json::json!(MAX_VALID_SEARCH_CURSOR_BYTES),
        );
    }
}

fn compact_json_diagnostic(value: &Value, depth: usize) -> Value {
    if depth >= 4 {
        return match value {
            Value::String(text) => Value::String(truncate_json_string(text, 240)),
            Value::Array(items) => serde_json::json!({ "item_count": items.len() }),
            Value::Object(object) => serde_json::json!({ "field_count": object.len() }),
            _ => value.clone(),
        };
    }
    match value {
        Value::String(text) => Value::String(truncate_json_string(text, 600)),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .take(20)
                .map(|item| compact_json_diagnostic(item, depth + 1))
                .collect(),
        ),
        Value::Object(object) => Value::Object(
            object
                .iter()
                .take(32)
                .map(|(key, value)| {
                    (
                        truncate_json_string(key, 128),
                        compact_json_diagnostic(value, depth + 1),
                    )
                })
                .collect(),
        ),
        _ => value.clone(),
    }
}

fn compact_search_result_value(value: &Value) -> Value {
    let Some(source) = value.as_object() else {
        return compact_json_diagnostic(value, 0);
    };
    const EVIDENCE_FIELDS: &[&str] = &[
        "id",
        "title",
        "file_path",
        "path",
        "start_line",
        "end_line",
        "language",
        "location",
        "breadcrumb",
        "score",
        "origin",
        "source_type",
        "content",
        "snippet",
        "text",
    ];
    const METADATA_FIELDS: &[&str] = &[
        "start_line",
        "end_line",
        "symbol",
        "snippet",
        "content_hash",
        "path",
    ];

    let mut compact = serde_json::Map::new();
    for field in EVIDENCE_FIELDS {
        if let Some(value) = source.get(*field) {
            let value = match (*field, value) {
                ("content" | "snippet" | "text", Value::String(text)) => {
                    Value::String(truncate_json_string(text, 700))
                }
                ("title" | "breadcrumb", Value::String(text)) => {
                    Value::String(truncate_json_string(text, 240))
                }
                (
                    "id" | "file_path" | "path" | "location" | "language" | "origin"
                    | "source_type",
                    Value::String(text),
                ) => Value::String(truncate_json_string(text, 512)),
                _ => bounded_scalar(value, 128).unwrap_or(Value::Null),
            };
            compact.insert((*field).to_string(), value);
        }
    }
    if let Some(metadata) = source.get("metadata").and_then(Value::as_object) {
        let selected: serde_json::Map<String, Value> = METADATA_FIELDS
            .iter()
            .filter_map(|field| {
                metadata.get(*field).map(|value| {
                    let max_chars = if matches!(*field, "snippet") {
                        700
                    } else {
                        512
                    };
                    let value = bounded_scalar(value, max_chars).unwrap_or(Value::Null);
                    ((*field).to_string(), value)
                })
            })
            .collect();
        if !selected.is_empty() {
            compact.insert("metadata".to_string(), Value::Object(selected));
        }
    }
    Value::Object(compact)
}

fn hard_search_result_value(value: &Value) -> Value {
    let Some(source) = value.as_object() else {
        return Value::Object(serde_json::Map::new());
    };
    const STRING_FIELDS: &[(&str, usize)] = &[
        ("id", 256),
        ("title", 240),
        ("file_path", 512),
        ("path", 512),
        ("language", 64),
        ("location", 512),
        ("breadcrumb", 240),
        ("origin", 64),
        ("source_type", 64),
    ];
    const SCALAR_FIELDS: &[&str] = &["start_line", "end_line", "score"];
    let mut compact = serde_json::Map::new();
    for (field, max_chars) in STRING_FIELDS {
        if let Some(Value::String(text)) = source.get(*field) {
            compact.insert(
                (*field).to_string(),
                Value::String(truncate_json_string(text, *max_chars)),
            );
        }
    }
    for field in SCALAR_FIELDS {
        if let Some(value) = source
            .get(*field)
            .and_then(|value| bounded_scalar(value, 0))
        {
            compact.insert((*field).to_string(), value);
        }
    }
    Value::Object(compact)
}

fn insert_structured_budget_report(
    value: &mut Value,
    byte_limit: usize,
    bytes_before: SerializedSize,
    original_results: usize,
) {
    let kept_results = value
        .get("results")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or_default();
    if let Some(object) = value.as_object_mut() {
        let mut report = serde_json::json!({
            "applied": true,
            "byte_limit": byte_limit,
            "bytes_before": bytes_before.bytes,
            "bytes_after": 0,
            "result_rows_omitted": original_results.saturating_sub(kept_results),
        });
        if bytes_before.is_lower_bound {
            report["bytes_before_is_lower_bound"] = Value::Bool(true);
        }
        object.insert("structured_budget".to_string(), report);
    }
}

fn refresh_structured_budget_report(
    value: &mut Value,
    stats: &mut SerializationBudgetStats,
) -> usize {
    // `bytes_after` is self-referential only through its decimal digit count;
    // two refreshes are sufficient to stabilize it without the prior 3-4
    // serializations on every shed row.
    for _ in 0..2 {
        let bytes_after = serialized_json_bytes_counted(value, stats);
        if let Some(report) = value
            .get_mut("structured_budget")
            .and_then(Value::as_object_mut)
        {
            report.insert("bytes_after".to_string(), serde_json::json!(bytes_after));
        }
    }
    serialized_json_bytes_counted(value, stats)
}

fn fit_search_result_rows(
    value: &mut Value,
    max_bytes: usize,
    bytes_before: SerializedSize,
    original_results: usize,
    stats: &mut SerializationBudgetStats,
) {
    let Some(all_results) = value
        .get_mut("results")
        .and_then(Value::as_array_mut)
        .map(std::mem::take)
    else {
        return;
    };
    if all_results.len() <= 1 {
        if let Some(results) = value.get_mut("results").and_then(Value::as_array_mut) {
            *results = all_results;
        }
        return;
    }

    // Keep a little headroom for the final `bytes_after` digit update. Each
    // probe serializes once, so 300 rows need at most ten probes instead of
    // hundreds of pop + report stabilization cycles.
    let target = max_bytes.saturating_sub(64);
    let mut low = 1usize;
    let mut high = all_results.len();
    let mut best = 1usize;
    while low <= high {
        let middle = low + (high - low) / 2;
        if let Some(results) = value.get_mut("results").and_then(Value::as_array_mut) {
            *results = all_results[..middle].to_vec();
        }
        insert_structured_budget_report(value, max_bytes, bytes_before, original_results);
        if serialized_json_size_capped_counted(value, target, stats).bytes <= target {
            best = middle;
            low = middle.saturating_add(1);
        } else {
            if middle == 0 {
                break;
            }
            high = middle - 1;
        }
    }
    if let Some(results) = value.get_mut("results").and_then(Value::as_array_mut) {
        *results = all_results[..best].to_vec();
    }
}

/// Bound the structured half of a search response while preserving the fields
/// agents need to act: file/path + line evidence, continuation controls, and
/// compact trust/scope diagnostics. Small payloads are returned byte-for-byte
/// unchanged for backwards compatibility.
fn budget_search_structured_value(value: Value, max_bytes: usize) -> Value {
    let mut stats = SerializationBudgetStats::default();
    budget_search_structured_value_impl(value, max_bytes, &mut stats)
}

#[cfg(test)]
fn budget_search_structured_value_counted(value: Value, max_bytes: usize) -> (Value, usize) {
    let mut stats = SerializationBudgetStats::default();
    let bounded = budget_search_structured_value_impl(value, max_bytes, &mut stats);
    (bounded, stats.attempts)
}

#[cfg(test)]
fn budget_search_structured_value_with_stats(
    value: Value,
    max_bytes: usize,
) -> (Value, SerializationBudgetStats) {
    let mut stats = SerializationBudgetStats::default();
    let bounded = budget_search_structured_value_impl(value, max_bytes, &mut stats);
    (bounded, stats)
}

fn budget_search_structured_value_impl(
    mut value: Value,
    max_bytes: usize,
    stats: &mut SerializationBudgetStats,
) -> Value {
    let max_bytes = max_bytes.max(SEARCH_STRUCTURED_OUTPUT_BUDGET_MIN);
    sanitize_search_continuation(&mut value);
    let bytes_before = serialized_json_size_capped_counted(&value, max_bytes, stats);
    if bytes_before.bytes <= max_bytes {
        return value;
    }

    let original_results = value
        .get("results")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or_default();
    let mut compact = value;
    if let Some(object) = compact.as_object_mut() {
        if let Some(results) = object.get_mut("results").and_then(Value::as_array_mut) {
            *results = results.iter().map(compact_search_result_value).collect();
        }
        for field in [
            "index_trust",
            "index_health",
            "scope_reliability",
            "scope_diagnostics",
            "active_index_repair",
            "search_explainability",
            "memory_inclusion",
            "skills_enrichment",
            "why_this_result",
            "guidance",
        ] {
            if let Some(value) = object.get_mut(field) {
                *value = if matches!(
                    field,
                    "index_trust" | "index_health" | "scope_reliability" | "scope_diagnostics"
                ) {
                    compact_fixed_control(field, value)
                } else {
                    compact_json_diagnostic(value, 0)
                };
            }
        }
        if let Some(fallback) = object
            .get_mut("memory_docs_fallback")
            .and_then(Value::as_object_mut)
        {
            fallback.remove("docs");
        }
        if let Some(vcs) = object
            .get_mut("vcs_search_results")
            .and_then(Value::as_object_mut)
        {
            if let Some(items) = vcs.get_mut("items").and_then(Value::as_array_mut) {
                items.truncate(5);
                for item in items {
                    *item = compact_json_diagnostic(item, 0);
                }
            }
        }
        if let Some(skills) = object
            .get_mut("matched_skills")
            .and_then(Value::as_array_mut)
        {
            skills.truncate(10);
            for skill in skills {
                *skill = compact_json_diagnostic(skill, 0);
            }
        }
        if let Some(files) = object
            .get_mut("session_dirty_files")
            .and_then(Value::as_object_mut)
            .and_then(|dirty| dirty.get_mut("files"))
            .and_then(Value::as_array_mut)
        {
            files.truncate(20);
        }
    }

    insert_structured_budget_report(&mut compact, max_bytes, bytes_before, original_results);
    if serialized_json_size_capped_counted(&compact, max_bytes, stats).bytes > max_bytes {
        fit_search_result_rows(
            &mut compact,
            max_bytes,
            bytes_before,
            original_results,
            stats,
        );
        insert_structured_budget_report(&mut compact, max_bytes, bytes_before, original_results);
    }
    if serialized_json_size_capped_counted(&compact, max_bytes, stats).bytes <= max_bytes
        && refresh_structured_budget_report(&mut compact, stats) <= max_bytes
    {
        return compact;
    }

    // Optional enrichments are useful but must never displace the first source
    // location or continuation/trust controls.
    if let Some(object) = compact.as_object_mut() {
        for field in [
            "vcs_search_results",
            "memory_docs_fallback",
            "matched_skills",
            "session_dirty_files",
            "skills_enrichment",
            "search_explainability",
            "hot_path_hint",
            "local_path_probe",
            "project_map_route",
        ] {
            object.remove(field);
        }
    }
    insert_structured_budget_report(&mut compact, max_bytes, bytes_before, original_results);
    if serialized_json_size_capped_counted(&compact, max_bytes, stats).bytes <= max_bytes
        && refresh_structured_budget_report(&mut compact, stats) <= max_bytes
    {
        return compact;
    }

    const CONTROL_FIELDS: &[&str] = &[
        "query",
        "mode",
        "workspace_id",
        "project_id",
        "total",
        "count",
        "result_count",
        "has_more",
        "next_offset",
        "next_cursor",
        "count_is_lower_bound",
        "query_time_ms",
        "scope_valid",
        "scope_reliability",
        "fallback_used",
        "fallback_reason",
        "index_state",
        "index_freshness",
        "index_trust",
        "index_health",
        "scope_diagnostics",
        "project_routing_warning",
        "index_origin_warning",
        "raw_evidence_first",
        "degraded",
        "degradation_reason",
        "degradation_message",
        "guidance_latency_ms",
        "retrieval_latency_ms",
        "navigator_latency_ms",
        "total_latency_ms",
        "code_evidence_count",
        "memory_evidence_count",
        "grounding_handle",
        "grounding_base_reused",
        "code_rerank_learning_request_id",
        "continuation_unavailable",
        "continuation_protocol_violation",
        "continuation_cursor_bytes",
        "max_valid_cursor_bytes",
        "wire_tokenizer",
    ];
    let mut minimal = serde_json::Map::new();
    if let Some(source) = compact.as_object() {
        for field in CONTROL_FIELDS {
            if let Some(value) = source.get(*field) {
                let value = match *field {
                    // Valid cursors are opaque protocol values: preserve them
                    // byte-for-byte. Invalid ones were removed above.
                    "next_cursor" => value.clone(),
                    "index_trust" | "index_health" | "scope_reliability" | "scope_diagnostics" => {
                        compact_fixed_control(field, value)
                    }
                    "grounding_handle" => bounded_scalar(value, 1_024).unwrap_or(Value::Null),
                    _ => bounded_scalar(value, 240).unwrap_or(Value::Null),
                };
                minimal.insert((*field).to_string(), value);
            }
        }
        if let Some(paths) = source.get("paths").and_then(Value::as_array) {
            minimal.insert(
                "paths".to_string(),
                Value::Array(
                    paths
                        .iter()
                        .filter_map(Value::as_str)
                        .take(20)
                        .map(|path| Value::String(truncate_json_string(path, 512)))
                        .collect(),
                ),
            );
        }
        if let Some(results) = source.get("results").and_then(Value::as_array) {
            minimal.insert(
                "results".to_string(),
                Value::Array(results.iter().take(1).cloned().collect()),
            );
        }
    }
    let mut minimal = Value::Object(minimal);
    insert_structured_budget_report(&mut minimal, max_bytes, bytes_before, original_results);
    if refresh_structured_budget_report(&mut minimal, stats) <= max_bytes {
        return minimal;
    }

    // Rebuild a fixed-schema evidence envelope. No arbitrary upstream object
    // keys or unbounded strings survive this point.
    let first_result = minimal
        .get("results")
        .and_then(Value::as_array)
        .and_then(|results| results.first())
        .map(hard_search_result_value);
    let first_path = minimal
        .get("paths")
        .and_then(Value::as_array)
        .and_then(|paths| paths.first())
        .and_then(Value::as_str)
        .map(|path| Value::String(truncate_json_string(path, 512)));
    let mut hard_object = serde_json::Map::new();
    for field in [
        "total",
        "count",
        "result_count",
        "has_more",
        "next_offset",
        "count_is_lower_bound",
        "continuation_unavailable",
        "continuation_protocol_violation",
        "continuation_cursor_bytes",
        "max_valid_cursor_bytes",
        "code_rerank_learning_request_id",
        "guidance_latency_ms",
        "retrieval_latency_ms",
        "navigator_latency_ms",
        "total_latency_ms",
        "code_evidence_count",
        "memory_evidence_count",
    ] {
        if let Some(value) = minimal
            .get(field)
            .and_then(|value| bounded_scalar(value, 128))
        {
            hard_object.insert(field.to_string(), value);
        }
    }
    if let Some(cursor) = minimal.get("next_cursor").and_then(Value::as_str) {
        hard_object.insert("next_cursor".to_string(), Value::String(cursor.to_string()));
    }
    for field in ["index_trust", "scope_reliability", "scope_diagnostics"] {
        if let Some(value) = minimal.get(field) {
            hard_object.insert(field.to_string(), compact_essential_control(field, value));
        }
    }
    hard_object.insert(
        "paths".to_string(),
        Value::Array(first_path.into_iter().collect()),
    );
    hard_object.insert(
        "results".to_string(),
        Value::Array(first_result.into_iter().collect()),
    );
    let mut hard = Value::Object(hard_object);
    insert_structured_budget_report(&mut hard, max_bytes, bytes_before, original_results);
    if let Some(report) = hard
        .get_mut("structured_budget")
        .and_then(Value::as_object_mut)
    {
        report.insert("hard_truncation".to_string(), Value::Bool(true));
    }
    if refresh_structured_budget_report(&mut hard, stats) > max_bytes {
        if let Some(result) = hard
            .get_mut("results")
            .and_then(Value::as_array_mut)
            .and_then(|results| results.first_mut())
            .and_then(Value::as_object_mut)
        {
            result.retain(|field, _| {
                matches!(
                    field.as_str(),
                    "id" | "file_path" | "path" | "start_line" | "end_line" | "location"
                )
            });
        }
    }
    if refresh_structured_budget_report(&mut hard, stats) <= max_bytes {
        return hard;
    }

    // Last-resort runtime envelope. Every carried value is now either a fixed
    // scalar, a <=5,654-byte valid cursor, or a <=128-character source
    // location. This branch is intentionally executable in release builds;
    // correctness does not depend on debug assertions.
    let cursor = hard.get("next_cursor").cloned();
    let continuation_unavailable = hard.get("continuation_unavailable").cloned();
    let continuation_protocol_violation = hard
        .get("continuation_protocol_violation")
        .and_then(Value::as_str)
        .map(|reason| Value::String(truncate_json_string(reason, 64)));
    let learning_request_id = hard.get("code_rerank_learning_request_id").cloned();
    let essential_result = hard
        .get("results")
        .and_then(Value::as_array)
        .and_then(|results| results.first())
        .map(|result| {
            let mut essential = serde_json::Map::new();
            if let Some(source) = result.as_object() {
                for field in ["file_path", "path", "location"] {
                    if let Some(Value::String(path)) = source.get(field) {
                        essential.insert(
                            field.to_string(),
                            Value::String(truncate_json_string(path, 128)),
                        );
                        break;
                    }
                }
                for field in ["start_line", "end_line"] {
                    if let Some(value) =
                        source.get(field).and_then(|value| bounded_scalar(value, 0))
                    {
                        essential.insert(field.to_string(), value);
                    }
                }
            }
            Value::Object(essential)
        });
    let essential_index_trust = hard
        .get("index_trust")
        .and_then(|value| {
            compact_object_fields(
                value,
                &[
                    ("project_id", 64),
                    ("committed_generation", 32),
                    ("result_generation_coverage_complete", 8),
                    ("result_generation_consistent", 8),
                ],
            )
        })
        .map(Value::Object);
    let essential_scope_reliability = hard
        .get("scope_reliability")
        .and_then(|value| {
            compact_object_fields(
                value,
                &[
                    ("usable", 8),
                    ("scope_match", 8),
                    ("scope_invalid", 8),
                    ("reason", 64),
                ],
            )
        })
        .map(Value::Object);
    let essential_scope_diagnostics = hard
        .get("scope_diagnostics")
        .and_then(|value| {
            compact_object_fields(
                value,
                &[
                    ("scope_valid", 8),
                    ("fallback_used", 8),
                    ("project_index_state", 64),
                    ("remediation_attempted", 8),
                ],
            )
        })
        .map(Value::Object);
    let mut absolute = serde_json::json!({
        "next_cursor": cursor.clone(),
        "continuation_unavailable": continuation_unavailable.clone(),
        "continuation_protocol_violation": continuation_protocol_violation.clone(),
        "code_rerank_learning_request_id": learning_request_id.clone(),
        "index_trust": essential_index_trust.clone(),
        "scope_reliability": essential_scope_reliability.clone(),
        "scope_diagnostics": essential_scope_diagnostics.clone(),
        "results": essential_result.clone().into_iter().collect::<Vec<_>>(),
        "structured_budget": {
            "applied": true,
            "hard_truncation": true,
            "absolute_envelope": true,
            "byte_limit": max_bytes,
            "bytes_before": bytes_before.bytes,
            "bytes_after": 0,
            "result_rows_omitted": original_results.saturating_sub(1),
        }
    });
    if refresh_structured_budget_report(&mut absolute, stats) <= max_bytes {
        return absolute;
    }

    let emergency = serde_json::json!({
        "next_cursor": cursor.clone(),
        "continuation_unavailable": continuation_unavailable.clone(),
        "continuation_protocol_violation": continuation_protocol_violation.clone(),
        "code_rerank_learning_request_id": learning_request_id.clone(),
        "index_trust": essential_index_trust.clone(),
        "scope_reliability": essential_scope_reliability.clone(),
        "scope_diagnostics": essential_scope_diagnostics.clone(),
        "results": essential_result.clone().into_iter().collect::<Vec<_>>(),
        "structured_budget": {
            "applied": true,
            "hard_truncation": true,
            "absolute_envelope": "emergency",
            "byte_limit": max_bytes,
        }
    });
    if serialized_json_size_capped_counted(&emergency, max_bytes, stats).bytes <= max_bytes {
        return emergency;
    }

    // A valid producer cursor is at most 5,654 bytes and cannot contain JSON
    // escape-amplifying characters, so this final protocol-only envelope is
    // strictly below the 10KiB structured minimum and preserves it exactly.
    let cursor_unavailable = cursor.is_none();
    let wire_violation = cursor_unavailable.then_some("wire_budget_invariant");
    serde_json::json!({
        "next_cursor": cursor,
        "continuation_unavailable": cursor_unavailable,
        "continuation_protocol_violation": wire_violation,
        "code_rerank_learning_request_id": learning_request_id,
        "index_trust": essential_index_trust,
        "scope_reliability": essential_scope_reliability,
        "scope_diagnostics": essential_scope_diagnostics,
        "results": essential_result.into_iter().collect::<Vec<_>>(),
    })
}

fn serialized_tool_result_bytes(result: &ToolResult) -> usize {
    let context = crate::wire_tokens::current_wire_response_context();
    let actual_wire = crate::wire_tokens::canonical_tool_result_bytes(result, &context)
        .map(|bytes| bytes.len())
        .unwrap_or(usize::MAX);
    // Retain the pre-existing ToolResult-schema invariant as a conservative
    // compatibility guard for callers/tests that serialize the domain object
    // directly, while actual tokenizer truth uses the transport bytes above.
    actual_wire.max(serialized_size_up_to(result, usize::MAX).bytes)
}

fn search_tool_result_fits(result: &ToolResult, max_wire_bytes: usize) -> bool {
    serialized_tool_result_bytes(result) <= max_wire_bytes
}

fn precap_search_text(mut text: String, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text;
    }

    let had_cache_marker = text.contains("[SEARCH_CACHED]");
    const OMISSION: &str =
        "\n[WIRE_BUDGET] Additional search prose was omitted before envelope fitting.";
    const CACHE: &str = "\n[SEARCH_CACHED] Cached response (wire-budgeted).\n";
    let suffix_bytes = OMISSION.len() + if had_cache_marker { CACHE.len() } else { 0 };
    let mut keep_bytes = max_bytes.saturating_sub(suffix_bytes);
    while keep_bytes > 0 && !text.is_char_boundary(keep_bytes) {
        keep_bytes -= 1;
    }
    text.truncate(keep_bytes);
    text.push_str(OMISSION);
    if had_cache_marker {
        text.push_str(CACHE);
    }
    text
}

fn wire_bounded_text_candidate(text: &str, keep_bytes: usize, had_cache_marker: bool) -> String {
    let mut candidate = text[..keep_bytes].to_string();
    if keep_bytes < text.len() {
        candidate.push_str(
            "\n[WIRE_BUDGET] Additional search prose was omitted; actionable evidence and controls remain in structured content.",
        );
    }
    if had_cache_marker && !candidate.contains("[SEARCH_CACHED]") {
        candidate.push_str("\n[SEARCH_CACHED] Cached response (wire-budgeted).\n");
    }
    candidate
}

fn fit_search_text_to_wire(text: &str, structured: &Value, max_wire_bytes: usize) -> String {
    let had_cache_marker = text.contains("[SEARCH_CACHED]");
    let mut boundaries = Vec::with_capacity(text.len().min(max_wire_bytes) + 1);
    boundaries.push(0);
    boundaries.extend(text.char_indices().skip(1).map(|(index, _)| index));
    if boundaries.last().copied() != Some(text.len()) {
        boundaries.push(text.len());
    }

    let mut low = 0usize;
    let mut high = boundaries.len().saturating_sub(1);
    let mut best = String::new();
    while low <= high {
        let middle = low + (high - low) / 2;
        let candidate_text =
            wire_bounded_text_candidate(text, boundaries[middle], had_cache_marker);
        let candidate = ToolResult::with_structured(candidate_text.clone(), structured.clone());
        if search_tool_result_fits(&candidate, max_wire_bytes) {
            best = candidate_text;
            low = middle.saturating_add(1);
        } else if middle == 0 {
            break;
        } else {
            high = middle - 1;
        }
    }
    best
}

/// Apply the final, combined search `ToolResult` envelope after every footer,
/// diagnostic, guidance target, and cache marker has been added. The limit is
/// exact serialized JSON bytes for `ToolResult`; it is intentionally described
/// as a transport proxy rather than tokenizer truth.
fn budget_search_tool_payload_at_limit(
    text: String,
    structured: Value,
    max_wire_bytes: usize,
) -> (String, Value) {
    let max_wire_bytes = max_wire_bytes.max(SEARCH_TOOL_RESULT_WIRE_BUDGET_MIN);
    let text = precap_search_text(text, max_wire_bytes.saturating_mul(2));
    let structured_limit = search_structured_output_budget()
        .min((max_wire_bytes / 2).max(SEARCH_STRUCTURED_OUTPUT_BUDGET_MIN));
    let mut structured = budget_search_structured_value(structured, structured_limit);

    let initial = ToolResult::with_structured(text.clone(), structured.clone());
    if search_tool_result_fits(&initial, max_wire_bytes) {
        return (text, structured);
    }

    let mut best = fit_search_text_to_wire(&text, &structured, max_wire_bytes);

    let bounded = ToolResult::with_structured(best.clone(), structured.clone());
    if search_tool_result_fits(&bounded, max_wire_bytes) {
        return (best, structured);
    }

    // Runtime fallback for future changes to ToolResult wrapper overhead.
    // Rebuild the structured side at the fixed minimum, then solve the text
    // allowance again. The 12KiB wire minimum carries the 10KiB fixed
    // actionable/control envelope plus wrapper and compact prose.
    structured = budget_search_structured_value(structured, SEARCH_STRUCTURED_OUTPUT_BUDGET_MIN);
    best = fit_search_text_to_wire(&text, &structured, max_wire_bytes);
    let bounded = ToolResult::with_structured(best.clone(), structured.clone());
    if search_tool_result_fits(&bounded, max_wire_bytes) {
        return (best, structured);
    }

    // Fully deterministic last resort. The structured limiter has already
    // reduced arbitrary input to a <=10KiB fixed envelope.
    (String::new(), structured)
}

fn budget_search_tool_payload(text: String, structured: Value) -> (String, Value) {
    budget_search_tool_payload_at_limit(text, structured, search_tool_result_wire_budget())
}

fn bounded_search_tool_result(text: impl Into<String>, structured: Value) -> ToolResult {
    let (text, structured) = budget_search_tool_payload(text.into(), structured);
    let result = ToolResult::with_structured(text, structured);
    if serialized_tool_result_bytes(&result) <= search_tool_result_wire_budget() {
        return result;
    }

    // Do not leave correctness to a debug assertion: if the ToolResult schema
    // grows unexpectedly, return a small explicit protocol envelope.
    ToolResult::with_structured(
        "[WIRE_BUDGET] Search response could not fit the configured transport envelope.",
        serde_json::json!({
            "results": [],
            "continuation_unavailable": true,
            "continuation_protocol_violation": "tool_result_wire_budget_invariant",
            "wire_budget": {
                "applied": true,
                "byte_limit": search_tool_result_wire_budget(),
                "proxy": "serialized_tool_result_json_bytes",
            }
        }),
    )
}

/// Enforce the same final envelope for search lanes that start from a complete
/// `ToolResult` (currently the plan-restriction result). This deliberately
/// preserves the original error bit while routing oversized text/structured
/// data through the exact same serialized-wire proxy as ordinary search hits.
fn bounded_existing_search_tool_result(result: ToolResult) -> ToolResult {
    bounded_existing_search_tool_result_at_limit(result, search_tool_result_wire_budget())
}

fn bounded_existing_search_tool_result_at_limit(
    mut result: ToolResult,
    max_wire_bytes: usize,
) -> ToolResult {
    let max_wire_bytes = max_wire_bytes.max(SEARCH_TOOL_RESULT_WIRE_BUDGET_MIN);
    if search_tool_result_fits(&result, max_wire_bytes) {
        return result;
    }

    let is_error = result.is_error;
    let text = result
        .content
        .iter()
        .filter_map(|item| match item {
            mcp_types::tool::ContentItem::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let structured = result
        .structured_content
        .take()
        .unwrap_or_else(|| serde_json::json!({ "results": [] }));
    let (text, structured) = budget_search_tool_payload_at_limit(text, structured, max_wire_bytes);
    let mut bounded = ToolResult::with_structured(text, structured);
    bounded.is_error = is_error;
    if search_tool_result_fits(&bounded, max_wire_bytes) {
        return bounded;
    }

    let mut fallback = ToolResult::with_structured(
        "[WIRE_BUDGET] Search response could not fit the configured transport envelope.",
        serde_json::json!({
            "results": [],
            "continuation_unavailable": true,
            "continuation_protocol_violation": "tool_result_wire_budget_invariant",
            "wire_budget": {
                "applied": true,
                "byte_limit": max_wire_bytes,
                "proxy": "serialized_tool_result_json_bytes",
            }
        }),
    );
    fallback.is_error = is_error;
    if search_tool_result_fits(&fallback, max_wire_bytes) {
        fallback
    } else {
        let mut text_only = ToolResult::text(
            "[WIRE_BUDGET] Search response was omitted because the transport envelope is too small.",
        );
        text_only.is_error = is_error;
        text_only
    }
}

#[derive(Clone)]
struct SearchWireTokenizerPolicy {
    decision: crate::wire_tokens::RolloutDecision,
    context: crate::wire_tokens::WireResponseContext,
}

fn search_tokenizer_canary_key(
    caller_identity: Option<&str>,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    session_id: Option<&str>,
    input: &SearchInput,
) -> String {
    crate::wire_tokens::stable_cohort_key(
        caller_identity,
        workspace_id,
        project_id,
        session_id,
        &input.query,
    )
}

fn search_result_text(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|item| match item {
            mcp_types::tool::ContentItem::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Apply exact counting only after cache markers, Guided diagnostics, cursor
/// controls, and all other final response shaping. Enforcement repeatedly
/// invokes the existing semantic byte compactor; serialized JSON/token vectors
/// are never sliced.
fn apply_search_wire_tokenizer(
    result: ToolResult,
    policy: &SearchWireTokenizerPolicy,
) -> ToolResult {
    apply_search_wire_tokenizer_with_budget(result, policy, None)
}

fn stamp_guided_total_latency(result: &mut ToolResult, budget: GuidedExecutionBudget) -> i64 {
    let elapsed_ms = budget.elapsed_ms();
    if let Some(object) = result
        .structured_content
        .as_mut()
        .and_then(Value::as_object_mut)
    {
        object.insert(
            "total_latency_ms".to_string(),
            serde_json::json!(elapsed_ms),
        );
    }
    elapsed_ms
}

fn apply_search_wire_tokenizer_with_budget(
    result: ToolResult,
    policy: &SearchWireTokenizerPolicy,
    guided_budget: Option<GuidedExecutionBudget>,
) -> ToolResult {
    apply_search_wire_tokenizer_at_limit(
        result,
        policy,
        guided_budget,
        search_tool_result_wire_budget(),
    )
}

fn search_exact_fail_honest_result(
    is_error: bool,
    context: &crate::wire_tokens::WireResponseContext,
    target_tokens: usize,
) -> ToolResult {
    let mut fallback = ToolResult::text(
        "[WIRE_BUDGET] Exact search response exceeded this token envelope; evidence was omitted. Retry with a narrower query/output_format or a larger wire budget.",
    );
    fallback.is_error = is_error;
    let outcome = match crate::wire_tokens::measure_tool_result(
        &fallback,
        context,
        "search_tool_result_fail_honest",
    ) {
        Some(measurement) if measurement.exact_tokens <= target_tokens => "fallback_within_target",
        Some(_) => "irreducible_transport_floor",
        None => "measurement_unavailable",
    };
    crate::wire_tokens::record_hard_floor_resolution("search", outcome);
    fallback
}

fn apply_search_wire_tokenizer_at_limit(
    result: ToolResult,
    policy: &SearchWireTokenizerPolicy,
    guided_budget: Option<GuidedExecutionBudget>,
    max_wire_bytes: usize,
) -> ToolResult {
    let max_wire_bytes = max_wire_bytes.max(SEARCH_TOOL_RESULT_WIRE_BUDGET_MIN);
    let mut proxy_result = bounded_existing_search_tool_result_at_limit(result, max_wire_bytes);
    if let Some(budget) = guided_budget {
        // Proxy shaping is part of the advertised latency. Stamp only after it
        // has completed, and before any final exact measurement consumes the
        // concrete whole-wire bytes.
        stamp_guided_total_latency(&mut proxy_result, budget);
    }
    if !policy.decision.measure_exact || !crate::wire_tokens::o200k_is_warm() {
        return proxy_result;
    }

    // Shadow and unselected enforcement are measured once at the final
    // transport boundary, on the concrete bytes the caller receives.
    if !policy.decision.enforce_exact {
        return proxy_result;
    }

    // `wire_tokenizer` is server-owned accounting metadata. Remove any stale
    // upstream value before exact enforcement so a report-skipping hard-floor
    // path cannot accidentally return an unverified enforcement claim.
    crate::wire_tokens::remove_fixed_point_report(&mut proxy_result);
    let target_tokens = max_wire_bytes.div_ceil(4);
    let Some(before) = crate::wire_tokens::measure_tool_result(
        &proxy_result,
        &policy.context,
        "search_tool_result",
    ) else {
        return search_exact_fail_honest_result(
            proxy_result.is_error,
            &policy.context,
            target_tokens,
        );
    };
    let mut result = proxy_result.clone();
    let report_reserve = if policy.context.include_structured && result.structured_content.is_some()
    {
        crate::wire_tokens::REPORT_TOKEN_RESERVE
    } else {
        0
    };
    let enforcement_target = target_tokens.saturating_sub(report_reserve).max(1);
    let base_text = search_result_text(&result);
    let base_structured = result
        .structured_content
        .clone()
        .unwrap_or_else(|| serde_json::json!({ "results": [] }));
    let is_error = result.is_error;
    let mut byte_limit = max_wire_bytes
        .saturating_sub(report_reserve.saturating_mul(4))
        .max(SEARCH_TOOL_RESULT_WIRE_BUDGET_MIN);
    let mut iterations = 0usize;

    if report_reserve > 0 {
        let (text, structured) = budget_search_tool_payload_at_limit(
            base_text.clone(),
            base_structured.clone(),
            byte_limit,
        );
        result = ToolResult::with_structured(text, structured);
        result.is_error = is_error;
    }

    for _ in 0..8 {
        let Some(measurement) = crate::wire_tokens::measure_tool_result(
            &result,
            &policy.context,
            "search_tool_result_enforce",
        ) else {
            return search_exact_fail_honest_result(
                result.is_error,
                &policy.context,
                target_tokens,
            );
        };
        if measurement.exact_tokens <= enforcement_target {
            break;
        }
        let scaled = byte_limit
            .saturating_mul(enforcement_target)
            .checked_div(measurement.exact_tokens.max(1))
            .unwrap_or(SEARCH_TOOL_RESULT_WIRE_BUDGET_MIN);
        let next = scaled
            .min(byte_limit.saturating_sub(1))
            .max(SEARCH_TOOL_RESULT_WIRE_BUDGET_MIN);
        if next >= byte_limit {
            break;
        }
        byte_limit = next;
        iterations += 1;
        let (text, structured) = budget_search_tool_payload_at_limit(
            base_text.clone(),
            base_structured.clone(),
            byte_limit,
        );
        result = ToolResult::with_structured(text, structured);
        result.is_error = is_error;
    }

    if let Some(budget) = guided_budget {
        // This is the latest mutable boundary: the timestamp itself is now
        // included in the final exact count and fixed-point report.
        stamp_guided_total_latency(&mut result, budget);
    }
    let Some(final_measurement) = crate::wire_tokens::measure_tool_result(
        &result,
        &policy.context,
        "search_tool_result_final",
    ) else {
        return search_exact_fail_honest_result(result.is_error, &policy.context, target_tokens);
    };
    if final_measurement.exact_tokens > enforcement_target {
        if final_measurement.exact_tokens <= target_tokens {
            crate::wire_tokens::record_hard_floor_resolution(
                "search",
                "report_omitted_within_target",
            );
            return result;
        }
        return search_exact_fail_honest_result(result.is_error, &policy.context, target_tokens);
    }

    let _ = crate::wire_tokens::attach_fixed_point_report(
        &mut result,
        policy.decision,
        &policy.context,
        "search_tool_result_reported",
        crate::wire_tokens::EnforcementReport {
            target_tokens,
            before: Some(before),
            iterations,
            hard_floor_exceeded: false,
        },
    );

    let Some(reported_measurement) = crate::wire_tokens::measure_tool_result(
        &result,
        &policy.context,
        "search_tool_result_post_report",
    ) else {
        crate::wire_tokens::remove_fixed_point_report(&mut result);
        return search_exact_fail_honest_result(result.is_error, &policy.context, target_tokens);
    };
    if reported_measurement.exact_tokens <= target_tokens
        && crate::wire_tokens::fixed_point_report_is_truthful(
            &result,
            policy.decision,
            target_tokens,
            reported_measurement,
        )
    {
        return result;
    }

    let report_removed = crate::wire_tokens::remove_fixed_point_report(&mut result);
    let Some(without_report) = crate::wire_tokens::measure_tool_result(
        &result,
        &policy.context,
        "search_tool_result_without_report",
    ) else {
        return search_exact_fail_honest_result(result.is_error, &policy.context, target_tokens);
    };
    if without_report.exact_tokens <= target_tokens {
        crate::wire_tokens::record_hard_floor_resolution(
            "search",
            if report_removed {
                "report_removed_within_target"
            } else {
                "report_omitted_within_target"
            },
        );
        return result;
    }

    search_exact_fail_honest_result(result.is_error, &policy.context, target_tokens)
}

/// Multi-word natural-language phrase detection for escalation guards.
///
/// Queries like "fable-5 effort high max slack" are keyword bags / NL
/// phrases, not literals: escalating them to exhaustive mode lets BM25
/// tokenization match on ANY single token and return hundreds of low-relevance
/// rows. Exhaustive escalation must be reserved for identifier/literal-shaped
/// queries where per-line completeness is meaningful.
fn is_natural_language_phrase_query(query: &str) -> bool {
    let trimmed = query.trim();
    if trimmed.split_whitespace().count() < 3 {
        return false;
    }
    extract_quoted_literal(trimmed).is_none()
        && !contains_code_identifiers(trimmed)
        && !contains_code_syntax_markers(trimmed)
        && !is_glob_like(trimmed)
        && !has_regex_characters(trimmed)
}

/// Compare a project's indexed root path (from API metadata) with the local
/// session folder, tolerating separator (`\` vs `/`), trailing-slash, and
/// case differences. Nested checkouts (one path is a prefix of the other)
/// count as matching. A Windows-rooted index (`C:\Users\me\dev\repo`)
/// checked against a Linux folder (`/home/me/dev/repo`) must NOT match —
/// that is exactly the cross-machine phantom-path case this guards against.
fn indexed_root_matches_local_folder(indexed_root: &str, local_folder: &str) -> bool {
    fn normalize(path: &str) -> String {
        let mut normalized = path.trim().replace('\\', "/");
        while normalized.ends_with('/') && normalized.len() > 1 {
            normalized.pop();
        }
        normalized.to_lowercase()
    }

    let indexed = normalize(indexed_root);
    let local = normalize(local_folder);
    if indexed.is_empty() || local.is_empty() {
        return true;
    }
    indexed == local
        || indexed.starts_with(&format!("{}/", local))
        || local.starts_with(&format!("{}/", indexed))
}

fn normalized_scope_label(value: &str) -> Option<String> {
    let normalized: String = value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(|ch| ch.to_lowercase())
        .collect();
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn normalized_path_leaf(path: &str) -> Option<String> {
    path.trim()
        .replace('\\', "/")
        .trim_matches('/')
        .rsplit('/')
        .find(|part| !part.trim().is_empty())
        .and_then(normalized_scope_label)
}

fn can_auto_repair_index_root_mismatch(
    indexed_root: &str,
    local_folder: &str,
    project_name: Option<&str>,
    resolved_project_id: Option<Uuid>,
    resolved_folder_project_id: Option<Uuid>,
    local_index_project_id: Option<Uuid>,
) -> bool {
    let Some(resolved_project_id) = resolved_project_id else {
        return false;
    };

    if resolved_folder_project_id == Some(resolved_project_id)
        || local_index_project_id == Some(resolved_project_id)
    {
        return true;
    }

    let local_leaf = normalized_path_leaf(local_folder);
    if local_leaf.is_none() {
        return false;
    }

    if normalized_path_leaf(indexed_root) == local_leaf {
        return true;
    }

    project_name
        .and_then(normalized_scope_label)
        .is_some_and(|name| Some(name) == local_leaf)
}

/// Check if a query appears to be an identifier/symbol.
fn is_identifier_query(query: &str) -> bool {
    let trimmed = query.trim();
    if trimmed.is_empty() || trimmed.contains(' ') || trimmed.len() < 2 {
        return false;
    }

    let is_valid = trimmed
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == ':');
    if !is_valid {
        return false;
    }

    let has_mixed_case =
        trimmed.chars().any(|c| c.is_uppercase()) && trimmed.chars().any(|c| c.is_lowercase());
    let has_underscore = trimmed.contains('_');
    let is_all_caps = trimmed.len() >= 3
        && trimmed
            .chars()
            .all(|c| c.is_uppercase() || c == '_' || c.is_numeric());

    has_mixed_case || has_underscore || is_all_caps
}

fn contains_code_syntax_markers(query: &str) -> bool {
    query.contains("::")
        || query.contains("->")
        || query.contains("=>")
        || query.contains('(')
        || query.contains(')')
        || query.contains('{')
        || query.contains('}')
        || query.contains('[')
        || query.contains(']')
        || query.contains('/')
        || query.contains('"')
        || query.contains('\'')
}

/// Detect whether a multi-word query contains code-like identifiers (camelCase,
/// PascalCase with internal caps, snake_case, UPPER_CASE, dotted paths, `::`)
/// that signal keyword/hybrid would outperform pure semantic search.
///
/// Title-case words like "Search" or "Two-Phase" are NOT counted — they are
/// common English, not code identifiers. We require internal uppercase
/// (e.g. "UserService" has U inside), underscores, or code punctuation.
fn contains_code_identifiers(query: &str) -> bool {
    query.split_whitespace().any(|token| {
        if token.len() < 3 {
            return false;
        }
        let stripped =
            token.trim_matches(|c: char| !c.is_alphanumeric() && c != '_' && c != ':' && c != '.');

        let has_snake =
            stripped.contains('_') && stripped.chars().all(|c| c.is_alphanumeric() || c == '_');

        // camelCase / PascalCase: must have an uppercase letter that isn't the
        // very first character (ruling out plain title-case English words).
        let has_internal_upper = stripped.len() >= 3
            && stripped.chars().skip(1).any(|c| c.is_uppercase())
            && stripped.chars().any(|c| c.is_lowercase())
            && stripped
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == ':' || c == '.');

        let has_code_punct = stripped.contains("::")
            || stripped.contains("->")
            || stripped.contains("=>")
            || (stripped.contains('.') && stripped.chars().filter(|&c| c == '.').count() >= 2);

        has_snake || has_internal_upper || has_code_punct
    })
}

const UI_COMPONENT_TERMS: &[&str] = &[
    "component",
    "modal",
    "dialog",
    "button",
    "page",
    "view",
    "panel",
    "sidebar",
    "navigation",
    "route",
    "palette",
    "menu",
    "dropdown",
    "tooltip",
    "popover",
    "drawer",
    "header",
    "footer",
    "layout",
    "widget",
    "card",
    "tab",
    "form",
    "input",
    "select",
    "checkbox",
    "toggle",
    "switch",
    "search",
    "filter",
    "table",
    "list",
    "grid",
    "chart",
    "dashboard",
    "toolbar",
    "breadcrumb",
    "stepper",
    "wizard",
    "carousel",
    "slider",
    "progress",
    "spinner",
    "skeleton",
    "avatar",
    "badge",
    "chip",
    "tag",
    "alert",
    "toast",
    "snackbar",
    "banner",
    "hook",
    "provider",
    "context",
    "store",
    "reducer",
    "action",
    "handler",
    "controller",
    "middleware",
    "interceptor",
    "guard",
    "resolver",
    "service",
    "factory",
    "adapter",
    "bridge",
    "proxy",
    "command",
    "shortcut",
    "keybinding",
    "scroll",
    "overflow",
];

fn contains_ui_component_terms(lower_query: &str) -> bool {
    UI_COMPONENT_TERMS
        .iter()
        .any(|term| lower_query.contains(term))
}

fn symbol_token_is_meaningful(token: &str) -> bool {
    if token.len() < 3 || token.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    if SYMBOL_ANCHOR_STOPWORDS.contains(&token) {
        return false;
    }

    token.contains('_')
        || token.contains("::")
        || token.ends_with("_id")
        || (token.chars().any(|c| c.is_uppercase()) && token.chars().any(|c| c.is_lowercase()))
}

fn extract_symbol_anchor_terms(query: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut current = String::new();

    let push_token = |token: &str, out: &mut Vec<String>| {
        for part in token.split("::") {
            let normalized = part.trim().to_lowercase();
            if symbol_token_is_meaningful(&normalized) && !out.contains(&normalized) {
                out.push(normalized);
            }
        }
    };

    for ch in query.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == ':' {
            current.push(ch);
            continue;
        }

        if !current.is_empty() {
            push_token(&current, &mut terms);
            current.clear();
        }
    }

    if !current.is_empty() {
        push_token(&current, &mut terms);
    }

    terms.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    terms
}

fn looks_like_symbol_anchor_query(query: &str) -> bool {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return false;
    }
    if is_glob_like(trimmed) || has_regex_characters(trimmed) {
        return false;
    }
    if !contains_code_syntax_markers(trimmed) {
        return false;
    }

    !extract_symbol_anchor_terms(trimmed).is_empty()
}

/// Check if a query appears to use regex metacharacters.
fn has_regex_characters(query: &str) -> bool {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return false;
    }

    if trimmed.contains("\\b")
        || trimmed.contains("\\s")
        || trimmed.contains("\\d")
        || trimmed.contains("\\w")
        || trimmed.contains("\\(")
        || trimmed.contains("\\)")
        || trimmed.contains("(?:")
        || trimmed.contains("(?=")
        || trimmed.contains("(?!")
        || trimmed.contains("(?<=")
        || trimmed.contains("(?<!")
        || trimmed.contains(".*")
        || trimmed.contains(".+")
    {
        return true;
    }

    if trimmed.starts_with('^') || trimmed.ends_with('$') {
        return true;
    }

    if trimmed.contains('|') && !trimmed.contains(" | ") {
        return true;
    }

    let open_brackets = trimmed.matches('[').count();
    let close_brackets = trimmed.matches(']').count();
    if open_brackets > 0
        && open_brackets == close_brackets
        && !trimmed.contains(char::is_whitespace)
    {
        return true;
    }

    if trimmed.contains('{') && trimmed.contains('}') && trimmed.chars().any(|c| c.is_ascii_digit())
    {
        return true;
    }

    // Parentheses alone are ambiguous (doc titles and function-call text often
    // contain them). Treat as regex only for balanced compact expressions.
    let open_count = trimmed.matches('(').count();
    let close_count = trimmed.matches(')').count();
    if open_count > 0 || close_count > 0 {
        let compact_group_like = !trimmed.chars().any(|c| c.is_whitespace()) && trimmed.len() <= 64;
        if open_count == close_count && compact_group_like {
            return true;
        }
    }

    if trimmed.contains('+')
        && !trimmed.chars().any(|c| c.is_whitespace())
        && trimmed.chars().any(|c| c.is_ascii_alphabetic())
    {
        return true;
    }

    // Treat '?' as regex only when it's not simple sentence punctuation.
    if trimmed.contains('?') {
        let trailing_only =
            trimmed.ends_with('?') && !trimmed[..trimmed.len().saturating_sub(1)].contains('?');
        let has_whitespace = trimmed.chars().any(|c| c.is_whitespace());
        return !trailing_only && !has_whitespace;
    }

    false
}

/// Check if a query looks like a glob pattern.
fn is_glob_like(query: &str) -> bool {
    let trimmed = query.trim();
    trimmed.contains('*') || (trimmed.contains('?') && !trimmed.ends_with('?'))
}

/// Check if the query asks for a count.
fn is_count_query(query_lower: &str) -> bool {
    COUNT_QUERY_PREFIXES
        .iter()
        .any(|prefix| query_lower.starts_with(prefix))
        || (query_lower.contains("how many") && query_lower.contains("are there"))
}

/// Check if the query asks for all occurrences.
fn is_all_matches_query(query_lower: &str) -> bool {
    ALL_MATCH_KEYWORDS.iter().any(|kw| query_lower.contains(kw))
}

/// Check if the query implies team/cross-project intent.
fn is_team_query(query_lower: &str) -> bool {
    TEAM_QUERY_KEYWORDS
        .iter()
        .any(|kw| query_lower.contains(kw))
}

fn should_allow_workspace_scope_fallback(
    requested_mode: SearchMode,
    query: &str,
    has_project_scope_candidates: bool,
) -> bool {
    if requested_mode == SearchMode::Team {
        return true;
    }

    if !has_project_scope_candidates {
        return true;
    }

    let query_lower = query.trim().to_lowercase();
    is_team_query(&query_lower)
}

fn push_unique_project_candidate(candidates: &mut Vec<Option<Uuid>>, candidate: Option<Uuid>) {
    if !candidates.contains(&candidate) {
        candidates.push(candidate);
    }
}

/// Check if the query likely asks for saved docs (memory/docs domain).
fn is_skill_query(query: &str) -> bool {
    let lower = query.trim().to_lowercase();
    if lower.is_empty() {
        return false;
    }

    let has_skill_keyword = [
        "skill",
        "skills",
        "checklist",
        "workflow",
        "standard operating procedure",
        "sop",
    ]
    .iter()
    .any(|term| lower.contains(term));

    let has_howto_workflow_intent = (lower.starts_with("how do i")
        || lower.starts_with("how to")
        || lower.contains("best way to")
        || lower.contains("recommended way to"))
        && [
            "deploy", "release", "rollback", "incident", "onboard", "triage", "migrate", "review",
            "test",
        ]
        .iter()
        .any(|term| lower.contains(term));

    has_skill_keyword || has_howto_workflow_intent
}

fn skill_score_threshold(query: &str) -> f64 {
    let lower = query.to_ascii_lowercase();
    if lower.contains("production")
        || lower.contains("incident")
        || lower.contains("rollback")
        || lower.contains("critical")
    {
        return 0.5;
    }
    if lower.contains("checklist") || lower.contains("workflow") || lower.contains("skill") {
        return 0.55;
    }
    0.65
}

fn score_confidence_band(score: Option<f64>) -> &'static str {
    match score {
        Some(s) if s >= 0.85 => "high",
        Some(s) if s >= 0.6 => "medium",
        Some(_) => "low",
        None => "unknown",
    }
}

fn is_doc_lookup_query(query: &str) -> bool {
    let lower = query.trim().to_lowercase();
    if lower.is_empty() {
        return false;
    }

    // If the query clearly targets source files/symbols, treat it as code search.
    if lower.contains(".rs")
        || lower.contains(".ts")
        || lower.contains(".js")
        || lower.contains("src/")
        || lower.contains("crates/")
        || lower.contains("function ")
        || lower.contains("class ")
    {
        return false;
    }

    let has_doc_term = DOC_QUERY_KEYWORDS.iter().any(|kw| lower.contains(kw));
    if !has_doc_term {
        return false;
    }

    let has_lookup_verb = DOC_LOOKUP_VERBS.iter().any(|kw| lower.contains(kw));
    has_lookup_verb || lower.starts_with("docs ") || lower.starts_with("doc ")
}

fn prefers_hybrid_for_code_location_query(query: &str) -> bool {
    let lower = query.trim().to_lowercase();
    if lower.is_empty() {
        return false;
    }

    let has_question_shape = lower.starts_with("where ")
        || lower.starts_with("which ")
        || lower.starts_with("what ")
        || lower.starts_with("how ")
        || lower.ends_with('?');
    if !has_question_shape {
        return false;
    }

    let has_location_intent = lower.contains("where")
        || lower.contains("which file")
        || lower.contains("what file")
        || lower.contains("implemented")
        || lower.contains("defined")
        || lower.contains("contains")
        || lower.contains("located");
    let has_code_terms = [
        "file",
        "path",
        "function",
        "handler",
        "component",
        "module",
        "css",
        "class",
        "symbol",
        "rust",
        "typescript",
        "tsx",
        " api",
    ]
    .iter()
    .any(|term| lower.contains(term));
    let has_bugfix_terms = ["bug", "fix", "fixed", "issue", "regression", "broken"]
        .iter()
        .any(|term| lower.contains(term));

    has_code_terms && (has_location_intent || has_bugfix_terms)
}

/// Recommend a mode from query heuristics (aligned with API behavior).
/// Note: `Crawl` mode is intentionally excluded — it is explicit-only and
/// never auto-selected by the recommender.
fn recommend_search_mode(query: &str, default_mode: Option<&str>) -> (SearchMode, &'static str) {
    let trimmed = query.trim();
    let resolved_default = default_mode
        .map(SearchMode::from_str)
        .unwrap_or(SearchMode::Hybrid);
    let crawl_is_default = matches!(resolved_default, SearchMode::Crawl);

    if trimmed.is_empty() {
        return (
            resolved_default,
            "Defaulted to fallback mode for broad discovery.",
        );
    }

    let lower = trimmed.to_lowercase();
    let _word_count = trimmed.split_whitespace().count();

    // ── Hard overrides: these modes have specific structural intent that
    // crawl cannot replace, so they always win regardless of default. ────

    if is_team_query(&lower) {
        return (SearchMode::Team, "Detected team/cross-project intent.");
    }

    if is_all_matches_query(&lower) {
        return (
            SearchMode::Exhaustive,
            "Detected all-occurrences intent; exhaustive mode is complete.",
        );
    }

    if is_glob_like(trimmed) || has_regex_characters(trimmed) {
        return (SearchMode::Pattern, "Detected glob/regex pattern.");
    }

    // ── When crawl is the configured default, it subsumes keyword, semantic,
    // hybrid, and refactor — its 3× candidate pool + graph enrichment covers
    // all of these with better recall. Only fall through to heuristics when
    // the default is NOT crawl. ──────────────────────────────────────────────

    if crawl_is_default {
        return (
            SearchMode::Crawl,
            "Workspace default_search_mode is crawl; using deep multi-modal search.",
        );
    }

    // ── Soft heuristics: only apply when crawl is NOT the default ───────

    let quoted = (trimmed.starts_with('"') && trimmed.ends_with('"'))
        || (trimmed.starts_with('\'') && trimmed.ends_with('\''));
    if quoted {
        return (SearchMode::Keyword, "Detected quoted exact-match query.");
    }

    if looks_like_symbol_anchor_query(trimmed) {
        return (
            SearchMode::Keyword,
            "Detected symbol-heavy code syntax query; using keyword mode for strict first-pass matching.",
        );
    }

    if is_identifier_query(trimmed) {
        return (
            SearchMode::Keyword,
            "Detected identifier-like query; using keyword mode for precise symbol matching.",
        );
    }

    if prefers_hybrid_for_code_location_query(trimmed) {
        return (
            SearchMode::Hybrid,
            "Detected code-location/bugfix question; hybrid balances semantic + exact code retrieval.",
        );
    }

    let starts_with_question = QUESTION_WORDS.iter().any(|w| lower.starts_with(w));
    if starts_with_question || trimmed.ends_with('?') {
        if contains_code_identifiers(trimmed) || contains_ui_component_terms(&lower) {
            return (
                SearchMode::Hybrid,
                "Detected natural-language query with code identifiers; hybrid balances semantic + keyword.",
            );
        }
        return (
            SearchMode::Semantic,
            "Detected natural-language query; semantic mode is a better fit.",
        );
    }

    if _word_count >= 4 {
        if contains_code_identifiers(trimmed) || contains_ui_component_terms(&lower) {
            return (
                SearchMode::Hybrid,
                "Detected multi-word query with code/UI terms; hybrid balances semantic + keyword.",
            );
        }
        return (
            SearchMode::Hybrid,
            "Detected multi-word query; hybrid mode balances semantic understanding with keyword matching.",
        );
    }

    (
        resolved_default,
        "No specific intent detected; used default fallback mode.",
    )
}

/// Suggest output format for token-efficient responses.
fn suggest_output_format(query: &str, mode: SearchMode) -> Option<&'static str> {
    let lower = query.trim().to_lowercase();
    if is_count_query(&lower) {
        return Some("count");
    }

    if is_identifier_query(query) {
        return match mode {
            SearchMode::Refactor | SearchMode::Exhaustive => Some("paths"),
            SearchMode::Keyword => Some("full"),
            _ => None,
        };
    }

    None
}

fn resolve_output_preferences(
    input: &SearchInput,
    requested_mode: SearchMode,
) -> (Option<String>, Option<bool>) {
    let explicit_output = input.output_format.clone();
    let explicit_full = explicit_output
        .as_deref()
        .map(|value| value.eq_ignore_ascii_case("full"))
        .unwrap_or(false);

    // Refactor and Exhaustive return one row per match-site across the entire
    // codebase — emitting full content per row blows past the MCP response
    // ceiling (200k chars) on any broadly-used symbol like `ApiError`. For
    // those modes default to paths-only and only honor an explicit caller opt-in.
    // Other modes (semantic, hybrid, keyword, pattern) keep the snippet default
    // because their result count is naturally bounded.
    let mode_defaults_to_content = !matches!(
        requested_mode,
        SearchMode::Refactor | SearchMode::Exhaustive
    );
    let include_content = match input.include_content {
        Some(value) => Some(value),
        // An explicit compact format is itself a content preference. Without
        // this branch, count/paths/minimal silently inherited the full-row
        // default for hybrid/semantic/keyword and were rewritten to `full`.
        None if explicit_output.is_some() => Some(explicit_full),
        None if mode_defaults_to_content => Some(true),
        None => Some(false),
    };

    let output_format = if include_content.unwrap_or(false) {
        Some("full".to_string())
    } else {
        explicit_output.or_else(|| {
            suggest_output_format(&input.query, requested_mode).map(|value| value.to_string())
        })
    };

    (output_format, include_content)
}

fn resolve_search_limit(input: &SearchInput, configured_limit: usize) -> Option<i64> {
    let limit = input.limit.unwrap_or(configured_limit as i64);
    Some(limit.clamp(1, 100))
}

fn resolve_search_content_max_chars(input: &SearchInput, configured_max_chars: usize) -> i64 {
    input
        .content_max_chars
        .unwrap_or(configured_max_chars as i64)
        .clamp(50, 10_000)
}

fn resolve_search_context_lines(input: &SearchInput) -> Option<i64> {
    input.context_lines.map(|value| value.clamp(0, 10))
}

fn resolve_exact_match_boost(input: &SearchInput) -> Option<f64> {
    input.exact_match_boost.map(|value| value.clamp(1.0, 10.0))
}

fn resolve_search_offset(input: &SearchInput) -> Option<i64> {
    input.offset.map(|value| value.max(0))
}

const PATH_QUERY_EXTENSIONS: &[&str] = &[
    ".rs", ".ts", ".tsx", ".js", ".jsx", ".py", ".go", ".java", ".kt", ".swift", ".c", ".h",
    ".cpp", ".hpp", ".json", ".yaml", ".yml", ".toml", ".md",
];
const INDEX_FRESH_HOURS: i64 = 1;
const INDEX_RECENT_HOURS: i64 = 12;
const INDEX_STALE_HOURS: i64 = 48;

#[derive(Debug, Clone)]
struct PathQueryHint {
    normalized_path: String,
    basename: String,
}

#[derive(Debug, Clone)]
struct LocalPathProbe {
    absolute_path: String,
    display_path: String,
    parent_dir: String,
    modified_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone)]
struct LocalIndexEntry {
    project_id: Option<Uuid>,
    indexed_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Git HEAD recorded at index time; drives out-of-session commit drift.
    indexed_commit: Option<String>,
}

#[derive(Debug, Clone)]
struct LocalProjectMapping {
    folder_path: String,
    project_id: Uuid,
    project_name: Option<String>,
    workspace_id: Option<Uuid>,
}

#[derive(Debug, Clone)]
struct LocalTwinRedirect {
    source_project_id: Uuid,
    target_project_id: Uuid,
    target_folder_path: String,
    indexed_root: String,
}

#[derive(Debug, Clone)]
struct ApiIndexHint {
    freshness: &'static str,
    confidence: &'static str,
    age_hours: Option<i64>,
    indexed_at: Option<chrono::DateTime<chrono::Utc>>,
    indicates_ready: bool,
    drift_detected: bool,
    recommendation: Option<String>,
}

#[derive(Debug, Clone)]
struct DirtyFileHint {
    absolute_path: String,
    display_path: String,
    modified_at: Option<chrono::DateTime<chrono::Utc>>,
    exists: bool,
}

#[derive(Debug, Clone, Default)]
struct DirtyFileSnapshot {
    /// Bounded copy for user-visible hints and hot-path shaping.
    hints: Vec<DirtyFileHint>,
    /// Complete known delta used by repair paths. This is never serialized.
    repair_hints: Vec<DirtyFileHint>,
    /// `Some` only when `git status` ran successfully for the checkout.
    git_worktree_dirty: Option<bool>,
}

#[derive(Debug, Clone)]
struct IndexHealth {
    freshness: &'static str,
    confidence: &'static str,
    age_hours: Option<i64>,
    scope_match: bool,
    drift_detected: bool,
    /// Number of locally-changed files (edited or deleted since the index
    /// snapshot) that drove `drift_detected`. Zero when drift came only from
    /// the query-derived probe or there is no local change signal.
    changed_file_count: usize,
    indexed_at: Option<String>,
    recommendation: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct LocalCheckoutTrust {
    #[serde(skip_serializing_if = "Option::is_none")]
    project_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    worktree_dirty: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    drift: Option<bool>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct IndexTrustChecks {
    #[serde(skip_serializing_if = "Option::is_none")]
    resolved_project_match: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    local_project_match: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    repository_match: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    branch_match: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    commit_match: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation_consistent: Option<bool>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct McpIndexTrustDiagnostics {
    #[serde(flatten)]
    server: SearchIndexTrustEnvelope,
    #[serde(skip_serializing_if = "Option::is_none")]
    local: Option<LocalCheckoutTrust>,
    checks: IndexTrustChecks,
}

#[derive(Debug, Clone, Default)]
struct ActiveIndexRepairStatus {
    attempted: bool,
    succeeded: bool,
    complete: bool,
    reason: Option<String>,
    age_secs_before: Option<i64>,
    changed_file_count: usize,
    files_indexed: Option<i64>,
    elapsed_ms: Option<u64>,
    timed_out: bool,
    error: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct TargetedLocalDelta {
    files: Vec<Value>,
    deleted_paths: Vec<String>,
    rejected_paths: usize,
    processed_hints: usize,
    truncated: bool,
}

impl TargetedLocalDelta {
    fn complete(&self) -> bool {
        !self.truncated && self.rejected_paths == 0
    }
}

fn targeted_payload_content_bytes(payload: &Value) -> usize {
    payload
        .get("content")
        .and_then(Value::as_str)
        .map(str::len)
        .unwrap_or(0)
}

fn targeted_local_delta(
    folder_path: &str,
    hints: &[&DirtyFileHint],
    max_files: usize,
    max_content_bytes: usize,
) -> TargetedLocalDelta {
    let mut delta = TargetedLocalDelta::default();
    let mut seen = HashSet::new();
    let mut content_bytes = 0usize;
    for hint in hints {
        if delta.processed_hints >= max_files {
            delta.truncated = true;
            break;
        }
        let relative = ContextStreamClient::safe_project_relative_path(
            folder_path,
            &hint.absolute_path,
            hint.exists,
        );
        let Some(relative) = relative else {
            delta.rejected_paths += 1;
            delta.processed_hints += 1;
            continue;
        };
        if !seen.insert(relative.clone()) {
            delta.processed_hints += 1;
            continue;
        }
        if hint.exists {
            // Files above the ordinary 5 MiB indexing limit are policy
            // deletions and require no read. Files that are indexable but do
            // not fit this repair lane's byte budget are deferred untouched.
            let metadata_len = std::fs::metadata(&hint.absolute_path)
                .ok()
                .filter(|metadata| metadata.is_file())
                .map(|metadata| metadata.len() as usize);
            if metadata_len.is_some_and(|len| {
                len <= 5 * 1024 * 1024 && content_bytes.saturating_add(len) > max_content_bytes
            }) {
                delta.truncated = true;
                break;
            }
            match ContextStreamClient::targeted_text_file_decision(folder_path, &hint.absolute_path)
            {
                TargetedFileDecision::Upload(payload) => {
                    let bytes = targeted_payload_content_bytes(&payload);
                    if content_bytes.saturating_add(bytes) > max_content_bytes {
                        delta.truncated = true;
                        break;
                    }
                    content_bytes += bytes;
                    delta.files.push(payload);
                }
                TargetedFileDecision::Delete(path) => delta.deleted_paths.push(path),
                TargetedFileDecision::Reject => delta.rejected_paths += 1,
            }
        } else {
            delta.deleted_paths.push(relative);
        }
        delta.processed_hints += 1;
    }
    if delta.processed_hints < hints.len() {
        delta.truncated = true;
    }
    delta.deleted_paths.sort();
    delta.deleted_paths.dedup();
    delta
}

/// Returns the dirty-file hints that represent uncommitted drift versus the
/// indexed snapshot: files modified after `indexed_at` (or all tracked dirty
/// files when the index timestamp is unknown), plus files deleted on disk.
fn dirty_hints_indicating_drift(
    hints: &[DirtyFileHint],
    indexed_at: Option<chrono::DateTime<chrono::Utc>>,
) -> Vec<&DirtyFileHint> {
    hints
        .iter()
        .filter(|hint| {
            // A deletion always drifts the index regardless of timestamps.
            if !hint.exists {
                return true;
            }
            match (hint.modified_at, indexed_at) {
                (Some(mtime), Some(indexed)) => mtime > indexed + chrono::Duration::seconds(2),
                // No known index time -> treat any tracked edit as drift.
                (Some(_), None) => true,
                (None, _) => false,
            }
        })
        .collect()
}

fn harmonize_project_index_state(response: &mut SearchResponse, health: Option<&IndexHealth>) {
    let Some(health) = health else {
        return;
    };
    if !health.scope_match || health.drift_detected {
        return;
    }

    let normalized = response
        .project_index_state
        .as_deref()
        .map(|state| state.trim().to_ascii_lowercase());
    let replacement = match (normalized.as_deref(), health.freshness) {
        (Some("partial" | "indexing" | "committing"), "fresh" | "recent") => Some("ready"),
        (Some("partial" | "indexing" | "committing"), "aging" | "stale") => Some("stale"),
        _ => None,
    };

    if let Some(state) = replacement {
        response.project_index_state = Some(state.to_string());
    }
}

impl IndexHealth {
    fn should_refresh(&self) -> bool {
        self.drift_detected || !self.scope_match || matches!(self.freshness, "stale" | "missing")
    }
}

fn should_surface_index_health_before_results(
    _health: &IndexHealth,
    _no_hits: bool,
    _scope_invalid: bool,
) -> bool {
    // Repair-first: never emit a pre-results index-health block (mirrors
    // `should_append_index_health_footer`). Honest signals — drift, scope
    // match/validity, and repair state — travel in the structured
    // `index_health` / `scope_reliability` payload instead of prose that agents
    // repeat to users or that steers them toward `git grep`.
    false
}

fn should_append_index_health_footer(
    health: &IndexHealth,
    _no_hits: bool,
    _scope_invalid: bool,
) -> bool {
    // Normal stale/drift handling is repair-first now. Keep index health in
    // structured telemetry for successful searches instead of emitting text
    // that agents repeat to users.
    let _ = health;
    false
}

/// Concise "files changed since indexing" line used by both the pre-results
/// block and the footer when drift is detected.
fn format_drift_change_note(health: &IndexHealth) -> Option<String> {
    if !health.drift_detected {
        return None;
    }
    if health.changed_file_count > 0 {
        Some(format!(
            "{} file(s) changed since last index; results may miss recent edits.",
            health.changed_file_count
        ))
    } else {
        Some("Local edits detected since last index; results may miss recent edits.".to_string())
    }
}

fn format_index_health_block(health: &IndexHealth, concise_text: bool) -> String {
    let mut text = String::new();
    let drift_note = format_drift_change_note(health);
    if concise_text {
        text.push_str(&format!(
            "Index health: freshness=`{}`.\n",
            health.freshness
        ));
        if let Some(note) = drift_note.as_deref() {
            text.push_str(&format!("{}\n", note));
        }
        if health.drift_detected || matches!(health.freshness, "stale" | "missing") {
            if let Some(recommendation) = health.recommendation.as_deref() {
                text.push_str(&format!("{}\n", recommendation));
            }
        }
    } else {
        let age_display = health
            .age_hours
            .map(|hours| format!("{}h old", hours))
            .unwrap_or_else(|| "age unknown".to_string());
        text.push_str(&format!(
            "Index health: freshness=`{}` ({}), confidence=`{}`.\n",
            health.freshness, age_display, health.confidence
        ));
        if let Some(note) = drift_note.as_deref() {
            text.push_str(&format!("{}\n", note));
        }
        if let Some(recommendation) = health.recommendation.as_deref() {
            text.push_str(&format!("{}\n", recommendation));
        }
    }
    text.push('\n');
    text
}

fn format_index_health_footer(health: &IndexHealth, concise_text: bool) -> String {
    let mut text = String::new();
    let drift_note = format_drift_change_note(health);
    if concise_text {
        text.push_str(&format!(
            "\n[Index advisory] freshness=`{}`. Results are usable, but they may miss recent edits.\n",
            health.freshness
        ));
        if let Some(note) = drift_note.as_deref() {
            text.push_str(&format!("{}\n", note));
        }
        if let Some(recommendation) = health.recommendation.as_deref() {
            text.push_str(&format!("{}\n", recommendation));
        }
    } else {
        let age_display = health
            .age_hours
            .map(|hours| format!("{}h old", hours))
            .unwrap_or_else(|| "age unknown".to_string());
        text.push_str(&format!(
            "\nIndex advisory: freshness=`{}` ({}), confidence=`{}`. Results are usable, but they may miss recent edits.\n",
            health.freshness, age_display, health.confidence
        ));
        if let Some(note) = drift_note.as_deref() {
            text.push_str(&format!("{}\n", note));
        }
        if let Some(recommendation) = health.recommendation.as_deref() {
            text.push_str(&format!("{}\n", recommendation));
        }
    }
    text
}

fn classify_index_freshness(age_hours: Option<i64>) -> &'static str {
    match age_hours {
        None => "unknown",
        Some(hours) if hours <= INDEX_FRESH_HOURS => "fresh",
        Some(hours) if hours <= INDEX_RECENT_HOURS => "recent",
        Some(hours) if hours <= INDEX_STALE_HOURS => "aging",
        Some(_) => "stale",
    }
}

fn index_freshness_severity(freshness: &str) -> i8 {
    match freshness {
        "fresh" => 0,
        "recent" => 1,
        "aging" => 2,
        "unknown" => 2,
        "stale" => 3,
        "missing" => 4,
        _ => 2,
    }
}

fn classify_confidence(score: i64) -> &'static str {
    if score >= 80 {
        "high"
    } else if score >= 55 {
        "medium"
    } else {
        "low"
    }
}

fn extract_api_index_hint(
    result: &SearchResponse,
    folder_path: Option<&str>,
    local_probe: Option<&LocalPathProbe>,
) -> Option<ApiIndexHint> {
    let indexed_at = result
        .ingested_at_max
        .as_deref()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc));
    let age_hours = indexed_at.map(|ts| chrono::Utc::now().signed_duration_since(ts).num_hours());
    let state = result
        .project_index_state
        .as_deref()
        .map(|value| value.to_ascii_lowercase());
    let generation_seen = result
        .index_generation
        .or(result.result_generation_max)
        .unwrap_or(0)
        > 0;
    let indicates_ready = generation_seen
        || matches!(
            state.as_deref(),
            Some("ready" | "indexing" | "partial" | "stale" | "committing")
        )
        || indexed_at.is_some();
    if !indicates_ready {
        return None;
    }

    let freshness = if let Some(age) = age_hours {
        classify_index_freshness(Some(age))
    } else {
        match state.as_deref() {
            Some("stale") => "stale",
            Some("partial") => "aging",
            // API reports healthy/active index state, but did not provide a timestamp.
            // Treat as recent rather than "missing" to avoid false-negative advisories.
            Some("ready" | "indexing" | "committing") => "recent",
            _ => "unknown",
        }
    };
    let confidence = if indexed_at.is_some() {
        "high"
    } else {
        "medium"
    };
    let recommendation = if freshness == "stale" {
        folder_path.map(|path| {
            format!(
                "Search reliability signals indicate stale index coverage. Results remain usable for existing indexed code; {}",
                hosted_index_refresh_instruction(path)
            )
        })
    } else {
        None
    };

    let drift_detected = match (local_probe, indexed_at) {
        (Some(probe), Some(indexed_time)) => probe
            .modified_at
            .map(|mtime| mtime > indexed_time + chrono::Duration::seconds(2))
            .unwrap_or(false),
        _ => false,
    };
    let recommendation = if drift_detected {
        let path = local_probe
            .map(|probe| probe.parent_dir.as_str())
            .or(folder_path)
            .unwrap_or("<folder>");
        Some(format!(
            "Detected local edits newer than indexed state. Results may miss those edits; {}",
            hosted_index_refresh_instruction(path)
        ))
    } else {
        recommendation
    };

    Some(ApiIndexHint {
        freshness,
        confidence,
        age_hours,
        indexed_at,
        indicates_ready,
        drift_detected,
        recommendation,
    })
}

fn extract_index_timestamp_value(result: &Value) -> Option<chrono::DateTime<chrono::Utc>> {
    let body = result.get("data").unwrap_or(result);
    for key in [
        "ingested_at_max",
        "indexed_at",
        "last_indexed",
        "index_timestamp",
    ] {
        if let Some(raw) = body.get(key).and_then(|v| v.as_str()) {
            if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(raw) {
                return Some(parsed.with_timezone(&chrono::Utc));
            }
        }
    }

    let state = body
        .get("project_index_state")
        .or_else(|| body.get("status"))
        .and_then(|v| v.as_str())
        .map(str::to_ascii_lowercase);
    if matches!(
        state.as_deref(),
        Some("indexing" | "partial" | "committing" | "processing" | "running" | "queued")
    ) {
        return None;
    }

    if let Some(raw) = body.get("last_updated").and_then(|v| v.as_str()) {
        if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(raw) {
            return Some(parsed.with_timezone(&chrono::Utc));
        }
    }
    None
}

fn index_status_reports_indexed(result: &Value) -> bool {
    let body = result.get("data").unwrap_or(result);
    if let Some(indexed) = body.get("indexed").and_then(|v| v.as_bool()) {
        return indexed;
    }
    body.get("indexed_file_count")
        .or_else(|| body.get("indexed_files"))
        .or_else(|| body.get("file_count"))
        .and_then(|v| v.as_i64())
        .map(|count| count > 0)
        .unwrap_or(false)
}

fn normalize_index_freshness_value(raw: Option<&str>) -> Option<&'static str> {
    match raw.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        Some("fresh") => Some("fresh"),
        Some("recent") => Some("recent"),
        Some("aging") => Some("aging"),
        Some("stale") => Some("stale"),
        Some("missing") => Some("missing"),
        Some("unknown") => Some("unknown"),
        _ => None,
    }
}

fn extract_project_status_index_hint(
    result: &Value,
    folder_path: Option<&str>,
    local_probe: Option<&LocalPathProbe>,
) -> Option<ApiIndexHint> {
    let body = result.get("data").unwrap_or(result);
    let indexed_at = extract_index_timestamp_value(result);
    let age_hours = indexed_at.map(|ts| chrono::Utc::now().signed_duration_since(ts).num_hours());
    let explicit_freshness = normalize_index_freshness_value(
        body.get("index_freshness")
            .and_then(|v| v.as_str())
            .or_else(|| body.get("freshness").and_then(|v| v.as_str())),
    );
    let state = body
        .get("project_index_state")
        .or_else(|| body.get("status"))
        .and_then(|v| v.as_str())
        .map(str::to_ascii_lowercase);
    let indexed = index_status_reports_indexed(result);
    let indicates_ready = indexed
        || indexed_at.is_some()
        || matches!(
            state.as_deref(),
            Some("ready" | "indexing" | "partial" | "stale" | "committing")
        )
        || explicit_freshness.is_some();
    if !indicates_ready {
        return None;
    }

    let freshness = if let Some(age) = age_hours {
        classify_index_freshness(Some(age))
    } else if let Some(freshness) = explicit_freshness {
        freshness
    } else {
        match state.as_deref() {
            Some("stale") => "stale",
            Some("partial") => "aging",
            Some("ready" | "indexing" | "committing") => "recent",
            _ => "unknown",
        }
    };
    let confidence = if indexed_at.is_some() {
        "high"
    } else {
        "medium"
    };
    let recommendation = if freshness == "stale" {
        folder_path.map(|path| {
            format!(
                "Search reliability signals indicate stale index coverage. Results remain usable for existing indexed code; {}",
                hosted_index_refresh_instruction(path)
            )
        })
    } else {
        None
    };

    let drift_detected = match (local_probe, indexed_at) {
        (Some(probe), Some(indexed_time)) => probe
            .modified_at
            .map(|mtime| mtime > indexed_time + chrono::Duration::seconds(2))
            .unwrap_or(false),
        _ => false,
    };
    let recommendation = if drift_detected {
        let path = local_probe
            .map(|probe| probe.parent_dir.as_str())
            .or(folder_path)
            .unwrap_or("<folder>");
        Some(format!(
            "Detected local edits newer than indexed state. Results may miss those edits; {}",
            hosted_index_refresh_instruction(path)
        ))
    } else {
        recommendation
    };

    Some(ApiIndexHint {
        freshness,
        confidence,
        age_hours,
        indexed_at,
        indicates_ready,
        drift_detected,
        recommendation,
    })
}

fn merge_api_index_hints(
    search_hint: Option<ApiIndexHint>,
    status_hint: Option<ApiIndexHint>,
) -> Option<ApiIndexHint> {
    match (search_hint, status_hint) {
        (Some(search), Some(status))
            if index_freshness_severity(status.freshness)
                > index_freshness_severity(search.freshness) =>
        {
            Some(status)
        }
        (Some(search), Some(status)) if status.drift_detected && !search.drift_detected => {
            Some(status)
        }
        (Some(search), _) => Some(search),
        (None, Some(status)) => Some(status),
        (None, None) => None,
    }
}

fn strip_wrapping_delimiters(value: &str) -> &str {
    let trimmed = value.trim();
    if trimmed.len() >= 2 {
        let first = trimmed.chars().next();
        let last = trimmed.chars().last();
        if matches!(
            (first, last),
            (Some('"'), Some('"')) | (Some('\''), Some('\'')) | (Some('`'), Some('`'))
        ) {
            return &trimmed[1..trimmed.len() - 1];
        }
    }
    trimmed
}

fn has_wrapping_delimiters(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.len() < 2 {
        return false;
    }
    matches!(
        (trimmed.chars().next(), trimmed.chars().last()),
        (Some('"'), Some('"')) | (Some('\''), Some('\'')) | (Some('`'), Some('`'))
    )
}

fn strip_trailing_line_suffix(value: &str) -> String {
    let mut parts: Vec<&str> = value.split(':').collect();
    if parts.len() >= 3
        && parts
            .last()
            .map(|p| p.chars().all(|c| c.is_ascii_digit()))
            .unwrap_or(false)
        && parts
            .get(parts.len() - 2)
            .map(|p| p.chars().all(|c| c.is_ascii_digit()))
            .unwrap_or(false)
    {
        parts.truncate(parts.len() - 2);
        return parts.join(":");
    }

    if parts.len() >= 2
        && parts
            .last()
            .map(|p| p.chars().all(|c| c.is_ascii_digit()))
            .unwrap_or(false)
    {
        parts.truncate(parts.len() - 1);
        return parts.join(":");
    }

    value.to_string()
}

fn normalize_path_like_query(query: &str) -> String {
    let mut normalized = strip_wrapping_delimiters(query).trim().replace('\\', "/");
    normalized = strip_trailing_line_suffix(&normalized);
    while normalized.starts_with("./") {
        normalized = normalized[2..].to_string();
    }
    normalized
}

fn looks_like_path_query(query: &str) -> bool {
    let trimmed = query.trim();
    let normalized = normalize_path_like_query(trimmed);
    if normalized.is_empty() || normalized.contains('\n') {
        return false;
    }

    // Require a path-dominant input. Mixed sentence/path fragments like
    // `foo(\"/a/b\" test` should not trigger path-aware retries.
    if normalized.split_whitespace().count() > 1 && !has_wrapping_delimiters(trimmed) {
        return false;
    }

    let normalized_lower = normalized.to_lowercase();
    normalized.contains('/')
        || normalized.starts_with("../")
        || PATH_QUERY_EXTENSIONS
            .iter()
            .any(|ext| normalized_lower.ends_with(ext))
}

fn path_query_hint(query: &str) -> Option<PathQueryHint> {
    if !looks_like_path_query(query) {
        return None;
    }

    let normalized_path = normalize_path_like_query(query);
    if normalized_path.is_empty() {
        return None;
    }

    let basename = normalized_path
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(normalized_path.as_str())
        .to_string();

    Some(PathQueryHint {
        normalized_path,
        basename,
    })
}

fn resolve_local_path_probe(query: &str, folder_path: Option<&str>) -> Option<LocalPathProbe> {
    let hint = path_query_hint(query)?;
    let mut candidates: Vec<PathBuf> = Vec::new();
    let hinted_path = PathBuf::from(&hint.normalized_path);
    if hinted_path.is_absolute() {
        candidates.push(hinted_path);
    }
    if let Some(root) = folder_path {
        candidates.push(Path::new(root).join(&hint.normalized_path));
    }
    candidates.push(PathBuf::from(&hint.normalized_path));

    let mut seen = HashSet::new();
    for candidate in candidates {
        let key = candidate.to_string_lossy().to_string();
        if !seen.insert(key) {
            continue;
        }
        if !candidate.is_file() {
            continue;
        }

        let display_path = if let Some(root) = folder_path {
            let root_path = Path::new(root);
            candidate
                .strip_prefix(root_path)
                .ok()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|| candidate.to_string_lossy().to_string())
        } else {
            candidate.to_string_lossy().to_string()
        };

        let parent_dir = candidate
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| ".".to_string());

        let modified_at = std::fs::metadata(&candidate)
            .ok()
            .and_then(|m| m.modified().ok())
            .map(chrono::DateTime::<chrono::Utc>::from);

        return Some(LocalPathProbe {
            absolute_path: candidate.to_string_lossy().to_string(),
            display_path,
            parent_dir,
            modified_at,
        });
    }

    None
}

fn index_scope_path_is_file(index_scope_path: &str) -> bool {
    std::fs::metadata(index_scope_path)
        .map(|meta| meta.is_file())
        .unwrap_or(false)
}

fn read_local_index_entry(folder_path: &str) -> Option<LocalIndexEntry> {
    let index_file = dirs::home_dir()?
        .join(".contextstream")
        .join("indexed-projects.json");
    let content = std::fs::read_to_string(index_file).ok()?;
    let data: Value = serde_json::from_str(&content).ok()?;
    let projects = data.get("projects")?.as_object()?;

    let folder = Path::new(folder_path);
    let mut best: Option<(usize, LocalIndexEntry)> = None;
    for (project_path, info) in projects {
        if !folder.starts_with(Path::new(project_path)) {
            continue;
        }
        if index_scope_path_is_file(project_path) {
            continue;
        }

        let project_id = info
            .get("project_id")
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok());
        let indexed_at = info
            .get("indexed_at")
            .and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc));
        let indexed_commit = info
            .get("indexed_commit")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let match_len = project_path.len();

        if best
            .as_ref()
            .map(|(best_len, _)| match_len > *best_len)
            .unwrap_or(true)
        {
            best = Some((
                match_len,
                LocalIndexEntry {
                    project_id,
                    indexed_at,
                    indexed_commit,
                },
            ));
        }
    }

    best.map(|(_, entry)| entry)
}

fn local_git_remote_url(folder_path: &str) -> Option<String> {
    // Remote identity authorizes automatic twin routing, so stale values are
    // not accepted. A miss schedules the bounded probe and fails closed now.
    let output = cached_git_output(
        folder_path,
        "remote",
        &["config", "--get", "remote.origin.url"],
        false,
    )?;
    let value = String::from_utf8_lossy(&output).trim().to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn repo_identity_component(value: &str) -> Option<String> {
    let component: String = value
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.')
        })
        .collect();
    (!component.is_empty() && !matches!(component.as_str(), "." | "..")).then_some(component)
}

fn repo_host_identity_component(value: &str) -> Option<String> {
    let component = value.trim().to_ascii_lowercase();
    if component.is_empty()
        || matches!(component.as_str(), "." | "..")
        || component.starts_with('.')
        || component.ends_with('.')
        || component.contains("..")
        || component.chars().any(|character| {
            !(character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | ':'))
        })
    {
        return None;
    }
    Some(component)
}

fn repo_identity_key(value: &str) -> Option<String> {
    let raw = value.trim().trim_end_matches('/');
    if raw.is_empty() {
        return None;
    }

    let (host, raw_path) = if raw.contains("://") {
        let url = reqwest::Url::parse(raw).ok()?;
        let host = url.host_str()?.to_ascii_lowercase();
        let default_port = match url.scheme() {
            "http" => Some(80),
            "https" => Some(443),
            "ssh" => Some(22),
            "git" => Some(9418),
            _ => None,
        };
        let host = match url.port() {
            Some(port) if Some(port) != default_port => {
                format!("{}:{}", host, port)
            }
            _ => host,
        };
        (host, url.path().to_string())
    } else if let Some((authority, path)) = raw.split_once(':') {
        // SCP-like Git syntax: git@host:group/subgroup/repo.git.
        if authority.contains('/') || path.is_empty() {
            return None;
        }
        let host = authority
            .rsplit_once('@')
            .map(|(_, host)| host)
            .unwrap_or(authority)
            .trim()
            .to_ascii_lowercase();
        (host, path.to_string())
    } else {
        // Scheme-less host/path form. Local filesystem remotes are not stable
        // cross-machine identities and therefore do not authorize twin reads.
        let (authority, path) = raw.split_once('/')?;
        if !authority.contains('.') && !authority.eq_ignore_ascii_case("localhost") {
            return None;
        }
        let host = authority
            .rsplit_once('@')
            .map(|(_, host)| host)
            .unwrap_or(authority)
            .trim()
            .to_ascii_lowercase();
        (host, path.to_string())
    };
    let host = repo_host_identity_component(host.trim_matches(['[', ']']))?;
    let mut parts = raw_path
        .trim_matches('/')
        .split('/')
        .filter(|part| !part.trim().is_empty())
        .map(repo_identity_component)
        .collect::<Option<Vec<_>>>()?;
    let last = parts.last_mut()?;
    if last.ends_with(".git") {
        last.truncate(last.len().saturating_sub(4));
    }
    if last.is_empty() {
        return None;
    }
    Some(format!("{}/{}", host, parts.join("/")))
}

fn enumerate_local_project_mappings() -> Vec<LocalProjectMapping> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let mut by_checkout: HashMap<(Uuid, String), LocalProjectMapping> = HashMap::new();
    let mut consider = |folder: &str,
                        project_id: Option<Uuid>,
                        project_name: Option<String>,
                        workspace_id: Option<Uuid>| {
        let Some(project_id) = project_id else {
            return;
        };
        let p = Path::new(folder);
        if !p.is_dir() || p.parent().is_none() {
            return;
        }
        let normalized_folder = std::fs::canonicalize(p)
            .unwrap_or_else(|_| p.to_path_buf())
            .to_string_lossy()
            .to_string();
        let entry = by_checkout
            .entry((project_id, normalized_folder.clone()))
            .or_insert_with(|| LocalProjectMapping {
                folder_path: normalized_folder,
                project_id,
                project_name: project_name.clone(),
                workspace_id,
            });
        if entry.project_name.is_none() {
            entry.project_name = project_name;
        }
        if entry.workspace_id.is_none() {
            entry.workspace_id = workspace_id;
        }
    };

    if let Ok(content) = std::fs::read_to_string(home.join(".contextstream").join("mappings.json"))
    {
        if let Ok(data) = serde_json::from_str::<Value>(&content) {
            let mappings = data
                .get("mappings")
                .and_then(|v| v.as_array())
                .cloned()
                .or_else(|| data.as_array().cloned())
                .unwrap_or_default();
            for mapping in mappings {
                let Some(path) = mapping.get("path").and_then(|v| v.as_str()) else {
                    continue;
                };
                let project_id = mapping
                    .get("project_id")
                    .and_then(|v| v.as_str())
                    .and_then(|s| Uuid::parse_str(s).ok());
                let project_name = mapping
                    .get("project_name")
                    .and_then(|v| v.as_str())
                    .map(ToString::to_string);
                let workspace_id = mapping
                    .get("workspace_id")
                    .and_then(|v| v.as_str())
                    .and_then(|s| Uuid::parse_str(s).ok());
                consider(path, project_id, project_name, workspace_id);
            }
        }
    }

    if let Ok(content) =
        std::fs::read_to_string(home.join(".contextstream").join("indexed-projects.json"))
    {
        if let Ok(data) = serde_json::from_str::<Value>(&content) {
            if let Some(projects) = data.get("projects").and_then(|v| v.as_object()) {
                for (path, info) in projects {
                    let project_id = info
                        .get("project_id")
                        .and_then(|v| v.as_str())
                        .and_then(|s| Uuid::parse_str(s).ok());
                    consider(path, project_id, None, None);
                }
            }
        }
    }

    by_checkout.into_values().collect()
}

/// Repo identity is authoritative for twin binding: two checkouts of the SAME
/// remote (on different machines) should bind, but two DIFFERENT repos that
/// merely share a name or path leaf must NOT — binding them is the cross-repo
/// provenance leak (e.g. an `mcp` folder pulling in `contextstream` chunks).
fn find_local_twin(project: &Project, workspace_id: Option<Uuid>) -> Option<LocalProjectMapping> {
    let requested_repo = project
        .repository_url
        .as_deref()
        .and_then(repo_identity_key)?;

    let mut matches = Vec::new();
    for mapping in enumerate_local_project_mappings() {
        let Some(bound_workspace_id) = mcp_session::auto_init::checkout_binding_workspace(
            &mapping.folder_path,
            mapping.project_id,
        ) else {
            continue;
        };
        if mapping.workspace_id != Some(bound_workspace_id)
            || workspace_id.is_some_and(|expected| expected != bound_workspace_id)
        {
            continue;
        }
        if project
            .path
            .as_deref()
            .is_some_and(|path| indexed_root_matches_local_folder(path, &mapping.folder_path))
        {
            continue;
        }

        let local_repo = local_git_remote_url(&mapping.folder_path)
            .as_deref()
            .and_then(repo_identity_key);
        if local_repo.as_deref() != Some(requested_repo.as_str()) {
            // Automatic cross-machine twin routing requires positive remote
            // repository identity on both sides. Names and path leaves are UX
            // hints only and can never authorize a local content read/write.
            continue;
        }
        matches.push(mapping);
    }

    (matches.len() == 1).then(|| matches.remove(0))
}

fn read_local_project_root_for_project(project_id: Option<Uuid>) -> Option<String> {
    let project_id = project_id?;
    if let Some(root) = read_current_config_root_for_project(Some(project_id)) {
        return Some(root);
    }
    let index_file = dirs::home_dir()?
        .join(".contextstream")
        .join("indexed-projects.json");
    let content = std::fs::read_to_string(index_file).ok()?;
    let data: Value = serde_json::from_str(&content).ok()?;
    let projects = data.get("projects")?.as_object()?;
    let mut candidates = Vec::new();
    for (project_path, info) in projects {
        if index_scope_path_is_file(project_path) {
            continue;
        }
        let Some(candidate_id) = info
            .get("project_id")
            .and_then(|v| v.as_str())
            .and_then(|s| Uuid::parse_str(s).ok())
        else {
            continue;
        };
        if candidate_id != project_id {
            continue;
        }

        let candidate = Path::new(project_path);
        if candidate.is_dir()
            && mcp_session::auto_init::checkout_binding_workspace(project_path, project_id)
                .is_some()
        {
            let normalized = std::fs::canonicalize(candidate)
                .unwrap_or_else(|_| candidate.to_path_buf())
                .to_string_lossy()
                .to_string();
            if !candidates.contains(&normalized) {
                candidates.push(normalized);
            }
        }
    }

    (candidates.len() == 1).then(|| candidates.remove(0))
}

fn read_current_config_root_for_project(project_id: Option<Uuid>) -> Option<String> {
    let project_id = project_id?;
    let mut current = std::env::current_dir().ok()?;

    loop {
        let config_path = current.join(".contextstream").join("config.json");
        if config_path.exists() {
            let content = std::fs::read_to_string(&config_path).ok()?;
            let config: Value = serde_json::from_str(&content).ok()?;
            let candidate_id = config
                .get("project_id")
                .and_then(|v| v.as_str())
                .and_then(|s| Uuid::parse_str(s).ok());
            if candidate_id == Some(project_id) {
                let root = current.to_string_lossy().to_string();
                if mcp_session::auto_init::checkout_binding_workspace(&root, project_id).is_some() {
                    return Some(root);
                }
                return None;
            }
        }

        if !current.pop() {
            break;
        }
        if current.as_os_str().is_empty() || current == Path::new("/") {
            break;
        }
        if let Some(home) = dirs::home_dir() {
            if current == home {
                break;
            }
        }
    }

    None
}

fn read_dirty_file_snapshot(folder_path: Option<&str>) -> DirtyFileSnapshot {
    let Some(folder_path) = folder_path else {
        return DirtyFileSnapshot::default();
    };
    let (git_dirty, git_worktree_dirty) = read_git_dirty_file_snapshot(folder_path);
    let repair_hints =
        merge_dirty_file_hints(read_recorded_dirty_file_hints(folder_path), git_dirty);
    let mut hints = repair_hints.clone();
    sort_and_limit_dirty_file_hints(&mut hints);
    DirtyFileSnapshot {
        hints,
        repair_hints,
        git_worktree_dirty,
    }
}

#[cfg(test)]
fn read_git_dirty_file_hints(folder_path: &str) -> Vec<DirtyFileHint> {
    read_git_dirty_file_snapshot(folder_path).0
}

fn read_recorded_dirty_file_hints(folder_path: &str) -> Vec<DirtyFileHint> {
    let Some(state_path) =
        dirs::home_dir().map(|h| h.join(".contextstream").join("dirty-files.json"))
    else {
        return Vec::new();
    };
    let Ok(content) = std::fs::read_to_string(state_path) else {
        return Vec::new();
    };
    let Ok(data) = serde_json::from_str::<Value>(&content) else {
        return Vec::new();
    };

    let Some(workspaces) = data.get("workspaces").and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    let folder = Path::new(folder_path);
    let mut selected_files: Option<&serde_json::Map<String, Value>> = None;
    let mut best_score = 0usize;

    for (tracked_root, entry) in workspaces {
        let tracked = Path::new(tracked_root);
        let score = if folder.starts_with(tracked) {
            10_000usize + tracked.components().count()
        } else if tracked.starts_with(folder) {
            tracked.components().count()
        } else {
            continue;
        };
        if score <= best_score {
            continue;
        }
        let Some(files) = entry.get("files").and_then(|v| v.as_object()) else {
            continue;
        };
        selected_files = Some(files);
        best_score = score;
    }

    let Some(files) = selected_files else {
        return Vec::new();
    };

    let cutoff = chrono::Utc::now() - chrono::Duration::hours(DIRTY_FILE_RETENTION_HOURS);
    let mut hints: Vec<DirtyFileHint> = files
        .iter()
        .filter_map(|(abs_path, ts_value)| {
            let modified_at = ts_value
                .as_str()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&chrono::Utc));
            if modified_at.map(|ts| ts < cutoff).unwrap_or(false) {
                return None;
            }

            let display_path = Path::new(abs_path)
                .strip_prefix(folder)
                .ok()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|| abs_path.to_string());

            Some(DirtyFileHint {
                absolute_path: abs_path.clone(),
                display_path,
                modified_at,
                exists: Path::new(abs_path).exists(),
            })
        })
        .collect();

    sort_dirty_file_hints(&mut hints);
    hints
}

fn read_git_dirty_file_snapshot(folder_path: &str) -> (Vec<DirtyFileHint>, Option<bool>) {
    let folder = Path::new(folder_path);
    if !folder.is_dir() {
        return (Vec::new(), None);
    }

    let Some(stdout) = cached_git_output(
        folder_path,
        "status",
        &["status", "--porcelain=v1", "-z", "--untracked-files=normal"],
        true,
    ) else {
        return (Vec::new(), None);
    };

    let hints = parse_git_status_dirty_hints(folder, &stdout);
    let dirty = !stdout.is_empty();
    (hints, Some(dirty))
}

fn parse_git_status_dirty_hints(folder: &Path, stdout: &[u8]) -> Vec<DirtyFileHint> {
    let mut hints = Vec::new();
    let mut entries = stdout
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty());

    while let Some(entry) = entries.next() {
        if entry.len() < 4 {
            continue;
        }

        let x = entry[0] as char;
        let y = entry[1] as char;
        if x == '!' && y == '!' {
            continue;
        }

        let path = String::from_utf8_lossy(&entry[3..])
            .trim()
            .replace('\\', "/");
        if path.is_empty() {
            continue;
        }

        let is_rename = matches!(x, 'R') || matches!(y, 'R');
        let is_rename_or_copy = is_rename || matches!(x, 'C') || matches!(y, 'C');
        let mut rename_source = None;
        if is_rename_or_copy {
            // In porcelain v1 -z, rename/copy records include the old path as
            // the next NUL-delimited field. We track the destination path and
            // consume the source path so it is not parsed as a separate entry.
            rename_source = entries.next().and_then(|source| {
                let source = String::from_utf8_lossy(source).trim().replace('\\', "/");
                (!source.is_empty()).then_some(source)
            });
        }

        let absolute_path = folder.join(Path::new(&path));
        let exists = absolute_path.exists();
        let modified_at = absolute_path
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .map(chrono::DateTime::<chrono::Utc>::from);

        hints.push(DirtyFileHint {
            absolute_path: absolute_path.to_string_lossy().to_string(),
            display_path: path,
            modified_at,
            exists,
        });
        if is_rename {
            if let Some(source) = rename_source {
                hints.push(DirtyFileHint {
                    absolute_path: folder.join(&source).to_string_lossy().to_string(),
                    display_path: source,
                    modified_at: None,
                    // A rename must remove the old indexed path even when a
                    // racy filesystem probe still sees something there.
                    exists: false,
                });
            }
        }
    }

    sort_dirty_file_hints(&mut hints);
    hints
}

fn merge_dirty_file_hints(
    recorded: Vec<DirtyFileHint>,
    git_dirty: Vec<DirtyFileHint>,
) -> Vec<DirtyFileHint> {
    let mut by_path: HashMap<String, DirtyFileHint> = HashMap::new();
    for hint in recorded.into_iter().chain(git_dirty) {
        let key = hint.absolute_path.replace('\\', "/");
        by_path
            .entry(key)
            .and_modify(|existing| {
                existing.exists = existing.exists || hint.exists;
                if hint.modified_at > existing.modified_at {
                    existing.modified_at = hint.modified_at;
                }
                if hint.display_path.len() < existing.display_path.len() {
                    existing.display_path = hint.display_path.clone();
                }
            })
            .or_insert(hint);
    }

    let mut hints: Vec<DirtyFileHint> = by_path.into_values().collect();
    sort_dirty_file_hints(&mut hints);
    hints
}

fn sort_dirty_file_hints(hints: &mut [DirtyFileHint]) {
    hints.sort_by(|a, b| {
        b.modified_at
            .cmp(&a.modified_at)
            .then_with(|| a.display_path.cmp(&b.display_path))
    });
}

fn sort_and_limit_dirty_file_hints(hints: &mut Vec<DirtyFileHint>) {
    sort_dirty_file_hints(hints);
    hints.truncate(DIRTY_FILE_DISPLAY_LIMIT.max(DRIFT_SYNC_MAX_FILES + 1));
}

/// Current git HEAD commit SHA for a folder, or None when it isn't a git repo
/// (or git is unavailable). Mirrors `local_git_remote_url`.
fn git_head_sha(folder_path: &str) -> Option<String> {
    let output = cached_git_output(folder_path, "head", &["rev-parse", "HEAD"], true)?;
    let value = String::from_utf8_lossy(&output).trim().to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn git_branch_name(folder_path: &str) -> Option<String> {
    let output = cached_git_output(
        folder_path,
        "branch",
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
        true,
    )?;
    let value = String::from_utf8_lossy(&output).trim().to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn response_generation_consistency(
    response: &SearchResponse,
    trust: &SearchIndexTrustEnvelope,
) -> Option<bool> {
    // A consistency verdict is meaningful only when every returned result had
    // generation metadata. Missing coverage (legacy/zero-result) and partial
    // coverage both remain explicitly unknown.
    if trust.result_generation_coverage_complete != Some(true) {
        return None;
    }

    let mut signals = Vec::with_capacity(3);
    if let Some(index_generation) = response.index_generation {
        signals.push(index_generation == trust.committed_generation);
    }
    if let Some(server_consistent) = trust.result_generation_consistent {
        signals.push(server_consistent);
    }
    if let Some(max_generation) = response.result_generation_max {
        let min_is_valid = response
            .result_generation_min
            .map(|min_generation| min_generation >= 0 && min_generation <= max_generation)
            .unwrap_or(true);
        signals.push(min_is_valid && max_generation <= trust.committed_generation);
    }

    (!signals.is_empty()).then(|| signals.into_iter().all(|consistent| consistent))
}

fn build_mcp_index_trust_diagnostics(
    response: &SearchResponse,
    resolved_project_id: Option<Uuid>,
    local_project_id: Option<Uuid>,
    local_repository: Option<String>,
    local_branch: Option<String>,
    local_commit_sha: Option<String>,
    local_indexed_at_known: bool,
    git_worktree_dirty: Option<bool>,
    index_health: Option<&IndexHealth>,
) -> Option<McpIndexTrustDiagnostics> {
    let server = response.index_trust.clone()?;
    let resolved_project_match =
        resolved_project_id.map(|project_id| project_id == server.project_id);
    let local_project_match = local_project_id.map(|project_id| project_id == server.project_id);
    let repository_match = match (server.repository.as_deref(), local_repository.as_deref()) {
        (Some(server), Some(local)) => Some(server == local),
        _ => None,
    };
    let branch_match = match (server.source_branch.as_deref(), local_branch.as_deref()) {
        (Some(server), Some(local)) => Some(server == local),
        _ => None,
    };
    let commit_match = match (
        server.source_commit_sha.as_deref(),
        local_commit_sha.as_deref(),
    ) {
        (Some(server), Some(local)) => Some(server == local),
        _ => None,
    };
    let checkout_comparable = !matches!(resolved_project_match, Some(false))
        && !matches!(local_project_match, Some(false))
        && !matches!(repository_match, Some(false))
        && !matches!(branch_match, Some(false))
        && !matches!(commit_match, Some(false));
    let local_drift = if index_health.is_some_and(|health| health.drift_detected) {
        Some(true)
    } else if checkout_comparable
        && git_worktree_dirty.is_some()
        && (local_indexed_at_known || server.indexed_at.is_some())
    {
        Some(false)
    } else {
        None
    };

    let local = LocalCheckoutTrust {
        project_id: local_project_id,
        repository: local_repository,
        branch: local_branch,
        commit_sha: local_commit_sha,
        worktree_dirty: git_worktree_dirty,
        drift: local_drift,
    };
    let has_local_metadata = local.project_id.is_some()
        || local.repository.is_some()
        || local.branch.is_some()
        || local.commit_sha.is_some()
        || local.worktree_dirty.is_some()
        || local.drift.is_some();

    Some(McpIndexTrustDiagnostics {
        checks: IndexTrustChecks {
            resolved_project_match,
            local_project_match,
            repository_match,
            branch_match,
            commit_match,
            generation_consistent: response_generation_consistency(response, &server),
        },
        server,
        local: has_local_metadata.then_some(local),
    })
}

fn format_index_trust_mismatch(diagnostics: &McpIndexTrustDiagnostics) -> Option<String> {
    let mut mismatches = Vec::new();
    if diagnostics.checks.resolved_project_match == Some(false)
        || diagnostics.checks.local_project_match == Some(false)
    {
        mismatches.push("project");
    }
    if diagnostics.checks.repository_match == Some(false) {
        mismatches.push("repository");
    }
    if diagnostics.checks.branch_match == Some(false) {
        mismatches.push("branch");
    }
    if diagnostics.checks.commit_match == Some(false) {
        mismatches.push("commit");
    }
    if diagnostics.checks.generation_consistent == Some(false) {
        mismatches.push("generation");
    }
    if mismatches.is_empty() {
        None
    } else {
        Some(format!(
            "[INDEX_TRUST] Search provenance does not match the active checkout ({}) — refresh this checkout's index before relying on missing or ghost paths.",
            mismatches.join(", ")
        ))
    }
}

/// Drift from a change committed OUTSIDE this session: true only when an indexed
/// commit was recorded and the current HEAD differs. Unknown HEAD (non-git
/// folder or git failure) is treated as no drift, so those folders fall back to
/// the mtime / dirty-file signals.
fn commit_indicates_drift(indexed_commit: Option<&str>, current_head: Option<&str>) -> bool {
    matches!((indexed_commit, current_head), (Some(i), Some(h)) if i != h)
}

fn build_index_health(
    folder_path: Option<&str>,
    resolved_project_id: Option<Uuid>,
    local_probe: Option<&LocalPathProbe>,
    local_entry: Option<LocalIndexEntry>,
    api_hint: Option<ApiIndexHint>,
    dirty_hints: &[DirtyFileHint],
) -> Option<IndexHealth> {
    let folder_path = folder_path?;

    let Some(local_entry) = local_entry else {
        if let Some(api) = api_hint {
            // No local metadata: reconcile the API-reported drift with the
            // local working-tree change signal (the API can't see uncommitted
            // edits, so dirty files are authoritative for drift here).
            let dirty_drift = dirty_hints_indicating_drift(dirty_hints, api.indexed_at);
            let drift_detected = api.drift_detected || !dirty_drift.is_empty();
            let recommendation = if !dirty_drift.is_empty() && api.recommendation.is_none() {
                Some(format!(
                    "Detected local edits newer than indexed state. {}",
                    hosted_index_refresh_instruction(folder_path)
                ))
            } else {
                api.recommendation
            };
            return Some(IndexHealth {
                freshness: api.freshness,
                confidence: api.confidence,
                age_hours: api.age_hours,
                scope_match: true,
                drift_detected,
                changed_file_count: dirty_drift.len(),
                indexed_at: api.indexed_at.map(|ts| ts.to_rfc3339()),
                recommendation,
            });
        }
        // No local metadata and no API hint: index is effectively missing.
        // Any tracked dirty file is drift relative to a non-existent index.
        let dirty_drift = dirty_hints_indicating_drift(dirty_hints, None);
        let recommendation = Some(format!(
            "No exact-checkout index metadata is available. {}",
            hosted_index_refresh_instruction(folder_path)
        ));
        return Some(IndexHealth {
            freshness: "missing",
            confidence: "low",
            age_hours: None,
            scope_match: false,
            drift_detected: !dirty_drift.is_empty(),
            changed_file_count: dirty_drift.len(),
            indexed_at: None,
            recommendation,
        });
    };

    let mut age_hours = local_entry
        .indexed_at
        .map(|ts| chrono::Utc::now().signed_duration_since(ts).num_hours());
    let mut freshness = classify_index_freshness(age_hours);
    let mut indexed_at = local_entry.indexed_at;
    let mut api_recommendation = None;
    if let Some(api) = api_hint.as_ref() {
        if index_freshness_severity(api.freshness) > index_freshness_severity(freshness) {
            freshness = api.freshness;
            age_hours = api.age_hours.or(age_hours);
            indexed_at = api.indexed_at.or(indexed_at);
            api_recommendation = api.recommendation.clone();
        }
    }
    let scope_match = match (resolved_project_id, local_entry.project_id) {
        (Some(resolved), Some(local)) => resolved == local,
        (None, _) => true,
        _ => false,
    };
    // Drift is detected from two independent signals: the query-derived probe
    // (the file the query named), and the local dirty-file tracker (any edited
    // or deleted file recorded by the PostToolUse hook). The latter is what
    // catches drift for ordinary keyword/semantic queries.
    let probe_drift = match (local_probe, local_entry.indexed_at) {
        (Some(probe), Some(indexed_at)) => probe
            .modified_at
            .map(|mtime| mtime > indexed_at + chrono::Duration::seconds(2))
            .unwrap_or(false),
        _ => false,
    };
    let dirty_drift = dirty_hints_indicating_drift(dirty_hints, local_entry.indexed_at);
    let changed_file_count = dirty_drift.len();
    let api_drift = api_hint
        .as_ref()
        .map(|hint| hint.drift_detected)
        .unwrap_or(false);
    // Drift from a commit made outside this session: the indexed commit was
    // recorded and HEAD has since moved. Only probes git when a commit is on
    // record, so non-git folders and pre-existing entries are unaffected.
    let current_head = if local_entry.indexed_commit.is_some() {
        git_head_sha(folder_path)
    } else {
        None
    };
    let commit_drift = commit_indicates_drift(
        local_entry.indexed_commit.as_deref(),
        current_head.as_deref(),
    );
    let drift_detected = probe_drift || !dirty_drift.is_empty() || api_drift || commit_drift;

    let mut score = 50i64;
    score += if scope_match { 20 } else { -25 };
    score += match freshness {
        "fresh" => 20,
        "recent" => 12,
        "aging" => -8,
        "stale" => -22,
        _ => -10,
    };
    if drift_detected {
        score -= 25;
    }
    let confidence = classify_confidence(score.clamp(0, 100));

    let recommendation_path = local_probe
        .map(|p| p.parent_dir.as_str())
        .unwrap_or(folder_path);
    let recommendation = if drift_detected {
        Some(format!(
            "Detected local edits newer than indexed state. {}",
            hosted_index_refresh_instruction(recommendation_path)
        ))
    } else if !scope_match {
        Some(format!(
            "Index metadata points at a different project scope. {}",
            hosted_index_refresh_instruction(recommendation_path)
        ))
    } else if freshness == "stale" {
        api_recommendation.or_else(|| {
            Some(format!(
                "Index metadata is older than {} hours. {}",
                INDEX_STALE_HOURS,
                hosted_index_refresh_instruction(recommendation_path)
            ))
        })
    } else {
        None
    };

    Some(IndexHealth {
        freshness,
        confidence,
        age_hours,
        scope_match,
        drift_detected,
        changed_file_count,
        indexed_at: indexed_at.map(|ts| ts.to_rfc3339()),
        recommendation,
    })
}

async fn maybe_repair_active_index_before_search(
    client: &ContextStreamClient,
    workspace_id: Option<Uuid>,
    explicit_project_id: Option<Uuid>,
    resolved_folder_project_id: Option<Uuid>,
    local_index_project_id: Option<Uuid>,
    session_project_id: Option<Uuid>,
    folder_path: Option<&str>,
    dirty_hints: &[DirtyFileHint],
) -> ActiveIndexRepairStatus {
    let mut status = ActiveIndexRepairStatus::default();
    let Some(project_id) = drift_ingest_project_id(
        explicit_project_id,
        resolved_folder_project_id,
        local_index_project_id,
        session_project_id,
    ) else {
        return status;
    };
    let Some(folder_path) = folder_path else {
        return status;
    };
    let folder = Path::new(folder_path);
    if !folder.is_dir() || folder.parent().is_none() {
        return status;
    }

    let indexed_at = ContextStreamClient::local_indexed_at(folder_path);
    let age_secs = ContextStreamClient::local_index_age_secs(folder_path);
    let dirty_drift = dirty_hints_indicating_drift(dirty_hints, indexed_at);
    let hot_threshold_secs = super::index_keeper::active_hot_threshold_secs();
    let inprogress_age_secs =
        ContextStreamClient::local_indexing_started_at(folder_path).map(|started| {
            chrono::Utc::now()
                .signed_duration_since(started)
                .num_seconds()
        });

    let reason = if !dirty_drift.is_empty() {
        "drift"
    } else if let Some(age) = age_secs {
        if age < hot_threshold_secs {
            return status;
        }
        if age >= INDEX_STALE_HOURS * 60 * 60 {
            "stale"
        } else {
            "not_hot"
        }
    } else {
        match inprogress_age_secs {
            Some(secs) if secs < ACTIVE_INDEX_IN_PROGRESS_GRACE_SECS => return status,
            Some(_) => "stranded",
            None => "missing",
        }
    };

    let Some(bound_workspace_id) = validated_checkout_content_workspace(
        client,
        folder_path,
        workspace_id,
        project_id,
        "search_preflight",
    )
    .await
    else {
        return status;
    };

    let targeted_all_known = dirty_drift.len() <= DRIFT_BACKGROUND_TOTAL_MAX_FILES;
    let targeted_hints = (reason == "drift").then(|| {
        dirty_drift
            .iter()
            .take(DRIFT_BACKGROUND_TOTAL_MAX_FILES)
            .map(|hint| (*hint).clone())
            .collect::<Vec<_>>()
    });
    let targeted_delta = targeted_hints.as_ref().map(|hints| {
        let refs = hints.iter().collect::<Vec<_>>();
        targeted_local_delta(
            folder_path,
            &refs,
            DRIFT_SYNC_MAX_FILES,
            DRIFT_SYNC_MAX_BYTES,
        )
    });
    if targeted_delta.as_ref().is_some_and(|delta| {
        delta.files.is_empty() && delta.deleted_paths.is_empty() && delta.rejected_paths > 0
    }) {
        tracing::warn!(
            path = %folder_path,
            project_id = %project_id,
            "search preflight skipped because every changed path failed canonical containment"
        );
        return status;
    }

    status.attempted = true;
    status.reason = Some(reason.to_string());
    status.age_secs_before = age_secs;
    status.changed_file_count = dirty_drift.len();

    ContextStreamClient::write_indexing_started(folder_path, project_id);

    // A full checkout walk is never allowed to sit in front of retrieval: an
    // async timeout cannot preempt synchronous filesystem traversal. Missing,
    // stale, and stranded indexes therefore repair strictly in a blocking-pool
    // background task. Only the bounded exact-file delta below may delay the
    // current search.
    if targeted_delta.is_none() {
        spawn_active_index_background_retry(
            client.clone(),
            bound_workspace_id,
            project_id,
            folder_path.to_string(),
            "search_preflight_background",
            None,
            false,
        );
        status.elapsed_ms = Some(0);
        return status;
    }

    let delta = targeted_delta.as_ref().expect("checked above");
    if delta.files.is_empty() && delta.deleted_paths.is_empty() {
        if delta.truncated {
            spawn_active_index_background_retry(
                client.clone(),
                bound_workspace_id,
                project_id,
                folder_path.to_string(),
                "search_preflight_deferred",
                targeted_hints,
                targeted_all_known,
            );
        }
        return status;
    }
    if mcp_session::auto_init::checkout_binding_workspace(folder_path, project_id)
        != Some(bound_workspace_id)
    {
        tracing::warn!(
            path = %folder_path,
            project_id = %project_id,
            "search preflight exact repair skipped because checkout identity changed while reading payloads"
        );
        return status;
    }

    let started = std::time::Instant::now();
    let ingest_result = tokio::time::timeout(
        ACTIVE_INDEX_PREFLIGHT_TIMEOUT,
        client.ingest_files_from_hook(
            project_id,
            bound_workspace_id,
            delta.files.clone(),
            delta.deleted_paths.clone(),
            false,
            Some("search_preflight"),
            Some(folder_path),
            false,
            true,
        ),
    )
    .await;
    match ingest_result {
        Ok(Ok(result)) => {
            status.succeeded = true;
            let committed = result.committed;
            status.complete = delta.complete() && targeted_all_known && committed;
            if status.complete {
                ContextStreamClient::write_index_status(folder_path, project_id);
            } else {
                spawn_active_index_background_retry(
                    client.clone(),
                    bound_workspace_id,
                    project_id,
                    folder_path.to_string(),
                    "search_preflight_remainder",
                    targeted_hints,
                    targeted_all_known,
                );
            }
            status.files_indexed = Some(result.files_indexed);
            status.elapsed_ms = Some(started.elapsed().as_millis() as u64);
        }
        Ok(Err(err)) => {
            status.elapsed_ms = Some(started.elapsed().as_millis() as u64);
            status.error = Some(err.to_string());
            spawn_active_index_background_retry(
                client.clone(),
                bound_workspace_id,
                project_id,
                folder_path.to_string(),
                "search_preflight_error",
                targeted_hints,
                targeted_all_known,
            );
        }
        Err(_) => {
            status.elapsed_ms = Some(started.elapsed().as_millis() as u64);
            status.timed_out = true;
            spawn_active_index_background_retry(
                client.clone(),
                bound_workspace_id,
                project_id,
                folder_path.to_string(),
                "search_preflight_timeout",
                targeted_hints,
                targeted_all_known,
            );
        }
    }

    status
}

fn spawn_active_index_background_retry(
    client: ContextStreamClient,
    workspace_id: Uuid,
    project_id: Uuid,
    folder_path: String,
    origin: &'static str,
    targeted_hints: Option<Vec<DirtyFileHint>>,
    targeted_all_known: bool,
) {
    tokio::spawn(async move {
        let Some(validated_workspace_id) = validated_checkout_content_workspace(
            &client,
            &folder_path,
            Some(workspace_id),
            project_id,
            origin,
        )
        .await
        else {
            return;
        };
        if targeted_hints.is_none() {
            let params = IngestLocalParams {
                path: folder_path.clone(),
                workspace_id: Some(validated_workspace_id),
                project_id: Some(project_id),
                force: Some(false),
                generate_editor_rules: None,
                include_media: None,
                max_files: Some(ACTIVE_INDEX_PREFLIGHT_MAX_FILES),
                background: Some(true),
                origin: Some(origin.to_string()),
                reroot: None,
            };
            let runtime = tokio::runtime::Handle::current();
            let blocking_result = tokio::task::spawn_blocking(move || {
                runtime.block_on(async move { client.ingest_local(params).await })
            })
            .await;
            match blocking_result {
                Ok(Ok(result))
                    if ContextStreamClient::ingest_scan_complete(&result)
                        && ContextStreamClient::ingest_result_committed(&result) =>
                {
                    ContextStreamClient::write_index_status(&folder_path, project_id);
                }
                Ok(Ok(_)) => {}
                Ok(Err(err)) => tracing::debug!(
                    error = %err,
                    path = %folder_path,
                    origin,
                    "active index background full repair failed"
                ),
                Err(err) => tracing::debug!(
                    error = %err,
                    path = %folder_path,
                    origin,
                    "active index background blocking task failed"
                ),
            }
            return;
        }

        let hints = targeted_hints.expect("checked above");
        let mut offset = 0usize;
        let mut complete = targeted_all_known;
        while offset < hints.len() {
            let refs = hints[offset..].iter().collect::<Vec<_>>();
            let delta = targeted_local_delta(
                &folder_path,
                &refs,
                DRIFT_BACKGROUND_BATCH_MAX_FILES,
                DRIFT_BACKGROUND_BATCH_MAX_BYTES,
            );
            if delta.processed_hints == 0 {
                complete = false;
                break;
            }
            offset += delta.processed_hints;
            complete &= delta.rejected_paths == 0;
            if delta.files.is_empty() && delta.deleted_paths.is_empty() {
                continue;
            }
            let Some(batch_workspace_id) = validated_checkout_content_workspace(
                &client,
                &folder_path,
                Some(validated_workspace_id),
                project_id,
                origin,
            )
            .await
            else {
                return;
            };
            match client
                .ingest_files_from_hook(
                    project_id,
                    batch_workspace_id,
                    delta.files,
                    delta.deleted_paths,
                    true,
                    Some(origin),
                    Some(&folder_path),
                    false,
                    false,
                )
                .await
            {
                Ok(result) => {
                    complete &= result.committed;
                }
                Err(err) => {
                    tracing::debug!(
                        error = %err,
                        path = %folder_path,
                        origin,
                        "active index background exact repair failed"
                    );
                    return;
                }
            }
        }
        complete &= offset == hints.len();
        if complete {
            ContextStreamClient::write_index_status(&folder_path, project_id);
        }
    });
}

fn normalize_search_root(path: Option<&str>) -> Option<String> {
    let path = path.map(str::trim).filter(|path| !path.is_empty())?;
    #[cfg(not(windows))]
    {
        let bytes = path.as_bytes();
        let is_windows_path = (bytes.len() >= 3
            && bytes[0].is_ascii_alphabetic()
            && bytes[1] == b':'
            && matches!(bytes[2], b'\\' | b'/'))
            || path.starts_with("\\\\");
        if is_windows_path {
            // On the hosted remote (Linux), Windows paths can't be used for local
            // filesystem operations, but we still return them so API-side scope
            // resolution (project_id, workspace_id) works via the mappings file.
            // Local enrichment will gracefully skip when the path doesn't exist.
            return Some(path.to_string());
        }
    }
    let candidate = Path::new(path);
    candidate.parent()?;
    Some(path.to_string())
}

fn current_dir_search_root() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let mut best: Option<(usize, usize, String)> = None;
    let repo_markers = [".git", "crates", "web", "migrations"];

    for ancestor in cwd.ancestors() {
        let Some(ancestor_str) = ancestor.to_str() else {
            continue;
        };
        let Some(normalized) = normalize_search_root(Some(ancestor_str)) else {
            continue;
        };
        let root = Path::new(&normalized);
        let marker_hits = repo_markers
            .iter()
            .filter(|marker| root.join(marker).exists())
            .count();
        if marker_hits < 2 {
            continue;
        }

        let depth = root.components().count();
        let score = (marker_hits, depth, normalized);
        if best
            .as_ref()
            .map(|(best_hits, best_depth, _)| {
                marker_hits > *best_hits || (marker_hits == *best_hits && depth > *best_depth)
            })
            .unwrap_or(true)
        {
            best = Some(score);
        }
    }

    best.map(|(_, _, normalized)| normalized)
}

fn scoped_session_folder_path<'a>(
    session_folder_path: Option<&'a str>,
    session_folder_project_id: Option<Uuid>,
    local_index_project_id: Option<Uuid>,
    target_project_id: Option<Uuid>,
    local_project_root: Option<&'a str>,
) -> Option<&'a str> {
    let session_path = session_folder_path?;
    let Some(target_project_id) = target_project_id else {
        return Some(session_path);
    };

    if session_folder_project_id == Some(target_project_id)
        || local_index_project_id == Some(target_project_id)
    {
        return Some(session_path);
    }

    if let Some(local_root) = local_project_root {
        if Path::new(session_path).starts_with(Path::new(local_root)) {
            return Some(session_path);
        }
    }

    None
}

fn resolve_effective_folder_path(
    session_folder_path: Option<&str>,
    project_path: Option<&str>,
    local_index_root: Option<&str>,
    allow_current_dir_fallback: bool,
) -> Option<String> {
    let local_index_root = normalize_search_root(local_index_root);
    if local_index_root.is_some() {
        return local_index_root;
    }

    let session_folder_path = normalize_search_root(session_folder_path);
    let project_path = normalize_search_root(project_path);

    match (session_folder_path, project_path) {
        (Some(session), Some(project)) if Path::new(&session).starts_with(Path::new(&project)) => {
            Some(project)
        }
        (Some(session), _) => Some(session),
        (None, Some(project)) => Some(project),
        (None, None) if allow_current_dir_fallback => current_dir_search_root(),
        (None, None) => None,
    }
}

fn extract_collection_array(value: &Value) -> Option<&Vec<Value>> {
    if let Some(arr) = value.as_array() {
        return Some(arr);
    }

    let obj = value.as_object()?;
    for key in ["items", "results", "docs", "data"] {
        if key == "data" {
            if let Some(data) = obj.get("data").and_then(extract_collection_array) {
                return Some(data);
            }
            continue;
        }
        if let Some(arr) = obj.get(key).and_then(|v| v.as_array()) {
            return Some(arr);
        }
    }

    None
}

async fn find_docs_fallback(
    client: &ContextStreamClient,
    workspace_id: Option<Uuid>,
    candidate_project_ids: &[Option<Uuid>],
    query: &str,
    limit: Option<i64>,
) -> Option<(Vec<Value>, Option<Uuid>)> {
    let mut candidates: Vec<Option<Uuid>> = Vec::new();
    for project_id in candidate_project_ids {
        if !candidates.contains(project_id) {
            candidates.push(*project_id);
        }
    }

    let max_docs = limit.unwrap_or(20).clamp(1, 50) as usize;
    for candidate in candidates {
        let response = client
            .list_docs(
                workspace_id,
                candidate,
                None,
                None,
                Some(query.trim().to_string()),
                Some(max_docs as i64),
            )
            .await;
        let Ok(response) = response else {
            continue;
        };

        let Some(items) = extract_collection_array(&response) else {
            continue;
        };
        if items.is_empty() {
            continue;
        }
        return Some((items.iter().take(max_docs).cloned().collect(), candidate));
    }

    None
}

async fn expand_team_project_candidates(
    client: &ContextStreamClient,
    workspace_id: Option<Uuid>,
    candidate_project_ids: &mut Vec<Option<Uuid>>,
) -> Option<String> {
    let workspace_id = workspace_id?;
    let Ok(projects) = client
        .list_projects(Some(workspace_id), Some(1), Some(100))
        .await
    else {
        return None;
    };

    let mut added = 0usize;
    for project in projects {
        if added >= TEAM_PROJECT_FALLBACK_LIMIT {
            break;
        }
        let candidate = Some(project.id);
        if candidate_project_ids.contains(&candidate) {
            continue;
        }
        candidate_project_ids.push(candidate);
        added += 1;
    }

    if added == 0 {
        None
    } else {
        Some(format!(
            "Team/cross-project query expanded fallback scope to {} additional workspace projects.",
            added
        ))
    }
}

/// Decide which mode to execute.
fn resolve_mode(
    input_mode: Option<&str>,
    query: &str,
    default_mode: Option<&str>,
) -> (SearchMode, bool, &'static str) {
    match input_mode.map(|m| m.trim().to_lowercase()) {
        Some(mode) if mode == "auto" => {
            let (resolved, reason) = recommend_search_mode(query, default_mode);
            (resolved, true, reason)
        }
        Some(mode) if !mode.is_empty() => {
            let resolved = SearchMode::from_str(&mode);
            (resolved, false, "Using explicit mode from input.")
        }
        _ => {
            let (resolved, reason) = recommend_search_mode(query, default_mode);
            (resolved, true, reason)
        }
    }
}

fn query_has_memory_intent(query: &str) -> bool {
    let lower = query.to_ascii_lowercase();
    if [
        "past session",
        "previous session",
        "prior context",
        "saved guidance",
    ]
    .iter()
    .any(|phrase| lower.contains(phrase))
    {
        return true;
    }

    let has_code_intent = query_has_code_intent(query);
    let strong_memory_terms = [
        "lesson",
        "lessons",
        "preference",
        "preferences",
        "memory",
        "memories",
        "remember",
        "remembered",
        "decision",
        "decisions",
        "runbook",
        "runbooks",
        "snapshot",
        "snapshots",
    ];
    let weak_memory_terms = ["doc", "docs", "spec", "adr", "rfc", "plan", "plans"];

    if strong_memory_terms
        .iter()
        .any(|term| contains_whole_word(&lower, term))
    {
        return true;
    }

    weak_memory_terms
        .iter()
        .any(|term| contains_whole_word(&lower, term))
        && !has_code_intent
}

fn contains_whole_word(haystack: &str, term: &str) -> bool {
    if term.is_empty() {
        return false;
    }
    if term.contains(' ') {
        return haystack.contains(term);
    }

    let bytes = haystack.as_bytes();
    let term_len = term.len();
    let mut start = 0usize;

    while let Some(idx) = haystack[start..].find(term) {
        let absolute = start + idx;
        let end = absolute + term_len;
        let prev_is_word = absolute > 0 && is_word_byte(bytes[absolute - 1]);
        let next_is_word = end < bytes.len() && is_word_byte(bytes[end]);
        if !prev_is_word && !next_is_word {
            return true;
        }
        start = absolute + 1;
    }

    false
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn query_has_code_intent(query: &str) -> bool {
    let lower = query.to_ascii_lowercase();
    if lower.contains("src/")
        || lower.contains("crates/")
        || lower.contains(".rs")
        || lower.contains(".ts")
        || lower.contains(".tsx")
        || lower.contains(".js")
        || lower.contains(".py")
        || lower.contains("::")
    {
        return true;
    }

    let tokens: std::collections::HashSet<&str> = lower
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|token| !token.is_empty())
        .collect();

    if [
        "function",
        "handler",
        "component",
        "module",
        "class",
        "symbol",
        "implementation",
        "implemented",
        "code",
        "file",
        "path",
        "ui",
        "dashboard",
        "tab",
        "tabs",
        "chip",
        "chips",
        "card",
        "cards",
        "header",
        "hero",
        "modal",
        "button",
        "buttons",
        "sidebar",
        "nav",
        "navigation",
        "page",
        "pages",
    ]
    .iter()
    .any(|term| tokens.contains(term))
    {
        return true;
    }

    // A query that is *shaped* like a code identifier — snake_case,
    // CamelCase/PascalCase, or a single bare token a human wouldn't type as
    // prose — is a code lookup even without an explicit keyword. This is the
    // dominant "find this symbol" case (e.g. `search_first_redirect_decision`,
    // `handleOAuth`). Without it, such queries fall through to memory-inclusive
    // ranking and surface docs/media noise instead of the code.
    query_is_identifier_shaped(query)
}

/// Heuristic: does the *whole* query read as a single code identifier rather
/// than natural language? True for snake_case (`fn_name`), CamelCase
/// (`SearchTool`), or a lone token with no prose markers. Deliberately
/// conservative — a multi-word phrase or a plain dictionary word (e.g.
/// `decisions`) returns false so memory-intent detection is not disturbed.
fn query_is_identifier_shaped(query: &str) -> bool {
    let trimmed = query.trim();
    // Single token only — any whitespace means it's a phrase, not an identifier.
    if trimmed.is_empty() || trimmed.split_whitespace().count() != 1 {
        return false;
    }
    // Must be made only of identifier characters.
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return false;
    }
    let has_underscore = trimmed.contains('_');
    let has_internal_camel = {
        let chars: Vec<char> = trimmed.chars().collect();
        chars.windows(2).any(|w| {
            (w[0].is_ascii_lowercase() && w[1].is_ascii_uppercase())
                || (w[0].is_ascii_uppercase() && w[1].is_ascii_lowercase())
        })
    };
    // snake_case or CamelCase → clearly an identifier.
    has_underscore || has_internal_camel
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MemoryInclusionDecision {
    enabled: bool,
    reason: &'static str,
}

fn resolve_include_memory_decision(
    requested_mode: SearchMode,
    include_memory_override: Option<bool>,
    prefers_project_scope: bool,
    query: &str,
) -> MemoryInclusionDecision {
    if let Some(include_memory) = include_memory_override {
        return MemoryInclusionDecision {
            enabled: include_memory,
            reason: "explicit include_memory override",
        };
    }

    if query_has_memory_intent(query) {
        return MemoryInclusionDecision {
            enabled: true,
            reason: "query explicitly requests memory/docs/history context",
        };
    }

    if prefers_project_scope {
        return MemoryInclusionDecision {
            enabled: false,
            reason: "project-scoped code search defaults to code-only results",
        };
    }

    if matches!(
        requested_mode,
        SearchMode::Keyword | SearchMode::Pattern | SearchMode::Exhaustive | SearchMode::Refactor
    ) {
        return MemoryInclusionDecision {
            enabled: false,
            reason: "exact/code-oriented search mode defaults to code-only results",
        };
    }

    if query_has_code_intent(query) {
        return MemoryInclusionDecision {
            enabled: false,
            reason: "code intent suppresses memory noise",
        };
    }

    if matches!(
        requested_mode,
        SearchMode::Semantic | SearchMode::Hybrid | SearchMode::Team
    ) {
        return MemoryInclusionDecision {
            enabled: false,
            reason: "code search defaults to code-only results",
        };
    }

    MemoryInclusionDecision {
        enabled: false,
        reason: "memory disabled by default for this search mode",
    }
}

#[cfg(test)]
fn resolve_include_memory(
    requested_mode: SearchMode,
    include_memory_override: Option<bool>,
    prefers_project_scope: bool,
    query: &str,
) -> bool {
    resolve_include_memory_decision(
        requested_mode,
        include_memory_override,
        prefers_project_scope,
        query,
    )
    .enabled
}

fn is_not_found_error(err: &Error) -> bool {
    is_project_scope_error(err)
        || matches!(
            err,
            Error::Http {
                code: ErrorCode::NotFound,
                ..
            }
        )
}

fn is_access_denied_error(err: &Error) -> bool {
    matches!(
        err,
        Error::Http {
            code: ErrorCode::Forbidden | ErrorCode::Unauthorized,
            ..
        }
    )
}

fn extract_quoted_literal(query: &str) -> Option<String> {
    let trimmed = query.trim();
    if trimmed.len() < 2 {
        return None;
    }

    let starts_double = trimmed.starts_with('"') && trimmed.ends_with('"');
    let starts_single = trimmed.starts_with('\'') && trimmed.ends_with('\'');
    if !starts_double && !starts_single {
        return None;
    }

    let literal = trimmed[1..trimmed.len() - 1].trim();
    if literal.is_empty() {
        None
    } else {
        Some(literal.to_string())
    }
}

fn normalized_symbol_retry_query(query: &str) -> String {
    extract_quoted_literal(query)
        .unwrap_or_else(|| strip_wrapping_delimiters(query).trim().to_string())
}

fn escape_regex_literal(input: &str) -> String {
    let mut escaped = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^' | '$' => {
                escaped.push('\\');
                escaped.push(ch);
            }
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn append_note(existing: Option<String>, extra: &str) -> Option<String> {
    if let Some(current) = existing {
        Some(format!("{} {}", current, extra))
    } else {
        Some(extra.to_string())
    }
}

fn local_mapping_mismatch_note(local_project_id: Uuid, resolved_scope: &str, path: &str) -> String {
    let path = serde_json::to_string(path).expect("serializing a string cannot fail");
    format!(
        "Local index mapping for this folder points to project_id {local_project_id}; search resolved {resolved_scope}. Re-establish the exact checkout with init(folder_path={path}), then run project(action=\"index_status\") and project(action=\"index\"). If the response says requires_sync_bridge, repair the bridge while keeping hosted MCP configured."
    )
}

fn hosted_index_refresh_instruction(path: &str) -> String {
    let path = serde_json::to_string(path).expect("serializing a string cannot fail");
    format!(
        "Re-establish the exact checkout with init(folder_path={path}), then run project(action=\"index\"). If the response says requires_sync_bridge, repair the bridge while keeping hosted MCP configured."
    )
}

fn push_fallback_stage(stages: &mut Vec<String>, stage: impl Into<String>) {
    let value = stage.into();
    if !stages.iter().any(|existing| existing == &value) {
        stages.push(value);
    }
}

/// Broad mode escalation is only appropriate for auto-selected natural-language
/// searches. Explicit modes, identifier queries, cursored searches, and symbol
/// anchors deliberately disable broad fallbacks so a healthy exact miss remains
/// authoritative instead of being replaced by unrelated BM25/semantic noise.
fn should_run_broad_mode_escalation(
    no_hits: bool,
    scope_invalid: bool,
    allow_broad_fallbacks: bool,
) -> bool {
    no_hits && !scope_invalid && allow_broad_fallbacks
}

#[derive(Debug, Clone, Serialize)]
struct LocalEnrichDiagnostic {
    kind: String,
    folder_path: String,
    detail: String,
}

impl LocalEnrichDiagnostic {
    fn new(kind: impl Into<String>, folder_path: &Path, detail: impl Into<String>) -> Self {
        Self {
            kind: kind.into(),
            folder_path: folder_path.display().to_string(),
            detail: detail.into(),
        }
    }

    fn from_io_error(folder_path: &Path, operation: &str, err: std::io::Error) -> Self {
        let kind = match err.kind() {
            std::io::ErrorKind::NotFound => "missing_root",
            std::io::ErrorKind::PermissionDenied => "permission_denied",
            _ => "unreadable_root",
        };
        Self::new(kind, folder_path, format!("{} failed: {}", operation, err))
    }
}

#[derive(Debug, Clone, Default)]
struct LocalEnrichOutcome {
    results: Vec<mcp_types::api::SearchResult>,
    diagnostic: Option<LocalEnrichDiagnostic>,
}

impl LocalEnrichOutcome {
    fn from_results(results: Vec<mcp_types::api::SearchResult>) -> Self {
        Self {
            results,
            diagnostic: None,
        }
    }

    fn diagnostic(diagnostic: LocalEnrichDiagnostic) -> Self {
        Self {
            results: Vec::new(),
            diagnostic: Some(diagnostic),
        }
    }
}

// ============================================================================
// Graph-boosted search enrichment
// ============================================================================

/// A single graph enrichment entry showing where a component/type is used.
struct GraphEnrichmentEntry {
    component_name: String,
    used_by: Vec<String>,
}

/// Graph enrichment context appended to search results.
struct GraphEnrichment {
    entries: Vec<GraphEnrichmentEntry>,
}

/// Attempt to enrich search results with graph reverse-dependency data.
/// Returns None if graph is unavailable or no enrichment is found.
/// This is non-blocking and best-effort — search results are never degraded.
async fn try_graph_enrichment(
    client: &ContextStreamClient,
    result: &SearchResponse,
    project_id: Option<Uuid>,
) -> Option<GraphEnrichment> {
    let project_id = project_id?;

    // Extract unique file paths from top 3 results
    let file_paths: Vec<&str> = result
        .results
        .iter()
        .take(3)
        .filter_map(|r| r.file_path.as_deref())
        .collect();

    if file_paths.is_empty() {
        return None;
    }

    // Extract names to query — components (PascalCase) AND modules (any file stem)
    struct EnrichmentTarget {
        name: String,
        target_type: &'static str,
    }
    let mut targets: Vec<EnrichmentTarget> = Vec::new();
    let mut seen_names = std::collections::HashSet::new();

    for path in &file_paths {
        let p = Path::new(path);
        if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
            if stem == "index" || stem == "mod" || stem == "lib" {
                continue;
            }
            if seen_names.contains(stem) {
                continue;
            }
            seen_names.insert(stem.to_string());

            if stem.starts_with(|c: char| c.is_uppercase()) {
                // PascalCase → likely component, also try as module
                targets.push(EnrichmentTarget {
                    name: stem.to_string(),
                    target_type: "component",
                });
            }
            // Always query as module for dependency info
            targets.push(EnrichmentTarget {
                name: path.to_string(),
                target_type: "module",
            });
        }
    }

    if targets.is_empty() {
        return None;
    }

    // Query graph targets concurrently. These are independent lookups, so
    // broad searches should pay the slowest call rather than the sum of four.
    let entry_futures = targets.into_iter().take(4).map(|target| async move {
        match target.target_type {
            "component" => {
                if let Ok(result) = client
                    .graph_usages(&target.name, "component", Some(project_id), Some(10))
                    .await
                {
                    if let Some(usages) = result.get("usages").and_then(|v| v.as_array()) {
                        if !usages.is_empty() {
                            let used_by: Vec<String> = usages
                                .iter()
                                .take(5)
                                .filter_map(|u| {
                                    u.get("file_path")
                                        .and_then(|v| v.as_str())
                                        .map(|s| format!("`{}`", s))
                                })
                                .collect();

                            if !used_by.is_empty() {
                                return Some(GraphEnrichmentEntry {
                                    component_name: target.name.clone(),
                                    used_by,
                                });
                            }
                        }
                    }
                }
            }
            "module" => {
                let dep_params = GraphDependenciesParams {
                    target: GraphTarget {
                        target_type: "module".to_string(),
                        id: None,
                        path: Some(target.name.clone()),
                    },
                    workspace_id: None,
                    project_id: Some(project_id),
                    max_depth: Some(1),
                    include_transitive: Some(false),
                };
                match client.graph_dependencies(dep_params).await {
                    Ok(result) => {
                        if let Some(deps) = result.get("dependencies").and_then(|v| v.as_array()) {
                            let dep_paths: Vec<String> = deps
                                .iter()
                                .take(5)
                                .filter_map(|d| {
                                    d.get("target")
                                        .and_then(|t| t.get("path").or_else(|| t.get("file_path")))
                                        .and_then(|v| v.as_str())
                                        .map(|s| format!("`{}`", s))
                                })
                                .collect();

                            if !dep_paths.is_empty() {
                                // Extract just the filename for the label
                                let label = Path::new(&target.name)
                                    .file_name()
                                    .and_then(|s| s.to_str())
                                    .unwrap_or(&target.name);
                                return Some(GraphEnrichmentEntry {
                                    component_name: format!("{} (deps)", label),
                                    used_by: dep_paths,
                                });
                            }
                        }
                    }
                    Err(_) => {
                        // Graph not available — silently skip
                    }
                }
            }
            _ => {}
        }
        None
    });
    let entries: Vec<GraphEnrichmentEntry> = futures::future::join_all(entry_futures)
        .await
        .into_iter()
        .flatten()
        .collect();

    if entries.is_empty() {
        None
    } else {
        Some(GraphEnrichment { entries })
    }
}

fn normalize_response(mut response: SearchResponse) -> SearchResponse {
    response.normalize_compact_formats();
    response
}

struct CorrelatedSearchResponse {
    response: SearchResponse,
    learning_request_id: Option<Uuid>,
}

/// Prepare one concrete backend search attempt.
///
/// A learning correlation identifies the exact candidate set returned by one
/// API call. Fallbacks and retries must therefore never reuse the UUID from a
/// different attempt: the backend stores the first observation for a scoped
/// UUID, while MCP may ultimately serve a later response.
fn prepare_code_rerank_learning_attempt(mut params: SearchParams) -> (SearchParams, Option<Uuid>) {
    let exact_learning_scope = params.workspace_id.is_some() && params.project_id.is_some();
    let learning_request_id = (params.code_rerank_learning_opt_in == Some(true)
        && exact_learning_scope)
        .then(Uuid::new_v4);
    if params.code_rerank_learning_opt_in == Some(true) && !exact_learning_scope {
        // Broader recovery attempts still serve their ordinary result, but
        // they cannot truthfully create a project-bound learning observation.
        params.code_rerank_learning_opt_in = None;
    }
    params.code_rerank_learning_request_id = learning_request_id;
    (params, learning_request_id)
}

async fn execute_api_search_attempt(
    client: &ContextStreamClient,
    mode: SearchMode,
    params: SearchParams,
) -> Result<CorrelatedSearchResponse> {
    let (params, learning_request_id) = prepare_code_rerank_learning_attempt(params);
    let response = match mode {
        SearchMode::Hybrid => client.search_hybrid(params).await?,
        SearchMode::Semantic => client.search_semantic(params).await?,
        SearchMode::Keyword => client.search_keyword(params).await?,
        SearchMode::Pattern => client.search_pattern(params).await?,
        SearchMode::Exhaustive => client.search_exhaustive(params).await?,
        SearchMode::Refactor => client.search_refactor(params).await?,
        SearchMode::Team => client.search_team(params).await?,
        SearchMode::Crawl => client.search_crawl(params).await?,
        SearchMode::Guided | SearchMode::Fuzzy | SearchMode::Vector => {
            return Err(Error::Validation(format!(
                "{} is not a direct API search attempt mode",
                mode.as_str()
            )));
        }
    };
    Ok(CorrelatedSearchResponse {
        response: normalize_response(response),
        learning_request_id,
    })
}

fn scope_remediation_note(response: &SearchResponse) -> Option<String> {
    if response.scope_is_valid() {
        return None;
    }

    if let Some(remediation) = response.scope_remediation.as_ref() {
        let requested = remediation
            .requested_scope
            .as_deref()
            .unwrap_or("unknown_scope");
        let resolved = remediation.resolved_scope.as_deref().unwrap_or("none");
        let reason = remediation
            .reason
            .as_deref()
            .or(response.scope_reason.as_deref())
            .unwrap_or("invalid_scope");
        return Some(format!(
            "Requested scope was invalid (reason: `{}`). Requested `{}`, resolved `{}`.",
            reason, requested, resolved
        ));
    }

    Some(format!(
        "Requested scope was invalid (reason: `{}`).",
        response.scope_reason.as_deref().unwrap_or("invalid_scope")
    ))
}

fn normalize_path_fallback_candidate(value: &str) -> String {
    let normalized = strip_trailing_line_suffix(&value.replace('\\', "/"));
    normalized.trim().to_lowercase()
}

fn normalize_paths_output_key(value: &str) -> String {
    let normalized = strip_trailing_line_suffix(&value.replace('\\', "/"));
    crate::domains::scope::repo_relative_suffix(&normalized)
        .map(|path| path.to_string_lossy().replace('\\', "/").to_lowercase())
        .unwrap_or_else(|| normalized.trim().to_lowercase())
}

fn path_fallback_result_matches_hint(
    item: &mcp_types::api::SearchResult,
    hint: &PathQueryHint,
) -> bool {
    let hint_path = hint.normalized_path.to_lowercase();
    let hint_basename = hint.basename.to_lowercase();

    [
        item.file_path.as_deref(),
        item.location.as_deref(),
        item.breadcrumb.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(normalize_path_fallback_candidate)
    .any(|candidate| {
        candidate == hint_path
            || candidate.contains(&hint_path)
            || candidate == hint_basename
            || candidate.ends_with(&format!("/{}", hint_basename))
    })
}

fn path_fallback_response_matches_hint(response: &SearchResponse, hint: &PathQueryHint) -> bool {
    response
        .results
        .iter()
        .any(|item| path_fallback_result_matches_hint(item, hint))
}

async fn try_path_aware_fallbacks(
    client: &ContextStreamClient,
    params: &SearchParams,
    original_query: &str,
) -> Option<(SearchResponse, SearchMode, &'static str, Option<Uuid>)> {
    let hint = path_query_hint(original_query)?;
    let mut attempts: Vec<(SearchMode, SearchParams, &'static str)> = Vec::new();
    let mut seen = HashSet::new();
    let mut push_attempt = |mode: SearchMode, query: String, note: &'static str| {
        if query.trim().is_empty() {
            return;
        }
        let key = format!("{}::{}", mode.as_str(), query);
        if !seen.insert(key) {
            return;
        }
        let mut next = params.clone();
        next.query = query;
        attempts.push((mode, next, note));
    };

    push_attempt(
        SearchMode::Keyword,
        format!("\"{}\"", hint.normalized_path),
        "Path-aware fallback: retried keyword search with an exact quoted path.",
    );
    push_attempt(
        SearchMode::Keyword,
        hint.normalized_path.clone(),
        "Path-aware fallback: retried keyword search using normalized file path.",
    );
    push_attempt(
        SearchMode::Pattern,
        escape_regex_literal(&hint.normalized_path),
        "Path-aware fallback: retried literal pattern search on normalized file path.",
    );
    if hint.basename != hint.normalized_path {
        push_attempt(
            SearchMode::Keyword,
            hint.basename.clone(),
            "Path-aware fallback: retried keyword search using file basename.",
        );
    }

    for (mode, next_params, note) in attempts {
        if let Ok(attempt) = execute_api_search_attempt(client, mode, next_params).await {
            if !attempt.response.results.is_empty()
                && path_fallback_response_matches_hint(&attempt.response, &hint)
            {
                return Some((attempt.response, mode, note, attempt.learning_request_id));
            }
        }
    }

    None
}

/// Format a single search result as a compact one-liner for display.
fn format_result_line(
    index: usize,
    item: &mcp_types::api::SearchResult,
    show_content: bool,
) -> String {
    let file_path = item.file_path.as_deref().unwrap_or("");
    let start_line = item.start_line.or_else(|| {
        item.metadata
            .as_ref()
            .and_then(|m| m.get("start_line"))
            .and_then(|v| v.as_i64())
    });
    let location = if !file_path.is_empty() {
        if let Some(line) = start_line {
            format!("{}:{}", file_path, line)
        } else {
            file_path.to_string()
        }
    } else {
        // A pathless hit must still render a usable reference — never a
        // degenerate ":0" from an empty path formatted with a line number.
        item.location
            .as_deref()
            .filter(|loc| !loc.starts_with(':') && !loc.trim().is_empty())
            .or(item.breadcrumb.as_deref())
            .or(item.title.as_deref())
            .unwrap_or("Result")
            .to_string()
    };

    // Label non-code hits (media/docs/memory) so they can't masquerade as
    // source files in agent output.
    let metadata_kind = item.metadata.as_ref().and_then(|m| {
        m.get("point_type")
            .or_else(|| m.get("content_type"))
            .or_else(|| m.get("chunk_type"))
            .and_then(|v| v.as_str())
    });
    let kind_tag = match metadata_kind {
        Some("image") | Some("video") | Some("audio") => "[media] ",
        Some("document") => "[doc] ",
        Some("memory_event") => "[memory] ",
        Some("knowledge_node") => "[node] ",
        _ => "",
    };

    let language = item.language.as_deref().or_else(|| {
        item.metadata
            .as_ref()
            .and_then(|m| m.get("language"))
            .and_then(|v| v.as_str())
    });
    let lang_tag = language.map(|l| format!(" [{}]", l)).unwrap_or_default();

    let score_str = item
        .score
        .map(|s| format!(" {}%", (s * 100.0).round() as i64))
        .unwrap_or_default();

    let mut line = format!(
        "{}. {}{}{}{}\n",
        index, kind_tag, location, lang_tag, score_str
    );

    if show_content {
        if let Some(content) = item.content.as_deref() {
            let trimmed = content.trim();
            if !trimmed.is_empty() && !trimmed.starts_with("File: ") {
                // Show a compact content preview (first ~3 lines, max 300 chars)
                let preview: String = trimmed.chars().take(300).collect();
                let preview_lines: Vec<&str> = preview.lines().take(4).collect();
                let preview_text = preview_lines.join("\n");
                line.push_str("   ");
                line.push_str(&preview_text.replace('\n', "\n   "));
                line.push('\n');
            }
        }
    }

    line
}

fn normalize_paths_output(result: &mut SearchResponse) {
    let mut seen = HashSet::new();
    let mut normalized_paths = Vec::new();

    for item in &result.results {
        if let Some(path) = item.file_path.as_deref() {
            let trimmed = path.trim();
            if trimmed.is_empty() {
                continue;
            }
            let key = normalize_paths_output_key(trimmed);
            if seen.insert(key) {
                normalized_paths.push(trimmed.to_string());
            }
        }
    }

    for path in &result.paths {
        let trimmed = path.trim();
        if trimmed.is_empty() {
            continue;
        }
        let key = normalize_paths_output_key(trimmed);
        if seen.insert(key) {
            normalized_paths.push(trimmed.to_string());
        }
    }

    if normalized_paths.is_empty() {
        return;
    }

    result.paths = normalized_paths.clone();
    result.results = normalized_paths
        .into_iter()
        .map(|path| SearchResult {
            id: path.clone(),
            file_path: Some(path.clone()),
            location: Some(path),
            ..SearchResult::default()
        })
        .collect();
    result.total = Some(result.paths.len() as i64);
    result.count = Some(result.paths.len() as i64);
}

fn is_memory_result(item: &mcp_types::api::SearchResult) -> bool {
    item.breadcrumb
        .as_deref()
        .map(|b| b.starts_with("memory_event") || b.starts_with("knowledge_node"))
        .unwrap_or(false)
}

fn is_symbol_noise_path(path: &str) -> bool {
    let normalized = path.to_lowercase().replace('\\', "/");
    SYMBOL_NOISE_PATH_TERMS
        .iter()
        .any(|term| normalized.contains(term))
}

fn is_artifact_like_path(path: &str) -> bool {
    let normalized = path.to_lowercase().replace('\\', "/");
    normalized.starts_with(".next/")
        || normalized.starts_with(".next.bak")
        || normalized.contains("/.next/")
        || normalized.contains("/.next.bak")
        || normalized.contains("/node_modules/")
        || normalized.contains("/dist/")
        || normalized.contains("/build/")
        || normalized.contains("/target/")
        || normalized.contains("/coverage/")
        || normalized.ends_with(".js.map")
        || normalized.ends_with(".css.map")
        || normalized.ends_with(".d.ts.map")
        || normalized.ends_with(".min.js")
        || normalized.starts_with("archives-ignore/")
        || normalized.contains("/archives-ignore/")
        || (normalized.starts_with("archives-") && normalized.contains("/tasks/"))
}

fn result_has_artifact_like_path(item: &mcp_types::api::SearchResult) -> bool {
    [
        item.file_path.as_deref(),
        item.location.as_deref(),
        item.breadcrumb.as_deref(),
        item.title.as_deref(),
        Some(item.id.as_str()),
    ]
    .into_iter()
    .flatten()
    .map(str::trim)
    .filter(|value| !value.is_empty())
    .any(is_artifact_like_path)
}

fn query_explicitly_targets_artifacts(query: &str) -> bool {
    let lower = query.to_lowercase();
    lower.contains(".map")
        || lower.contains("source map")
        || lower.contains("sourcemap")
        || lower.contains("node_modules")
        || lower.contains(".next")
        || lower.contains("dist/")
        || lower.contains("build/")
        || lower.contains("coverage")
}

fn should_filter_artifact_paths(_requested_mode: SearchMode, query: &str) -> bool {
    !query_explicitly_targets_artifacts(query)
}

fn is_compact_paths_response(response: &SearchResponse) -> bool {
    !response.paths.is_empty()
}

fn result_has_rerank_evidence(item: &mcp_types::api::SearchResult) -> bool {
    item.score.is_some()
        || item
            .content
            .as_deref()
            .is_some_and(|content| !content.trim().is_empty())
}

fn has_client_rerank_evidence(response: &SearchResponse) -> bool {
    !is_compact_paths_response(response) || response.results.iter().any(result_has_rerank_evidence)
}

fn apply_symbol_anchor_rerank(response: &mut SearchResponse, query: &str) -> Option<String> {
    // `paths` responses carry the server's authoritative file order but omit
    // snippets and scores. Re-ranking their synthetic result rows from path
    // tokens alone can promote a usage file over the exact definition. Fresh
    // local rows still carry content/score evidence and may override stale
    // server ordering.
    if !has_client_rerank_evidence(response) || response.results.len() < 2 {
        return None;
    }
    let compact_paths = is_compact_paths_response(response);
    let normalized_query = normalized_symbol_retry_query(query);
    if !looks_like_symbol_anchor_query(query) && !is_identifier_query(&normalized_query) {
        return None;
    }

    let anchors = extract_symbol_anchor_terms(&normalized_query);
    if anchors.is_empty() {
        return None;
    }

    let mut rows: Vec<(usize, bool, bool, mcp_types::api::SearchResult)> = response
        .results
        .drain(..)
        .enumerate()
        .map(|(idx, item)| {
            let row_has_evidence = !compact_paths || result_has_rerank_evidence(&item);
            let anchor_match = row_has_evidence && result_matches_symbol_anchors(&item, &anchors);
            let path_noise = !compact_paths
                && item
                    .file_path
                    .as_deref()
                    .map(is_symbol_noise_path)
                    .unwrap_or(false)
                && !anchor_match;
            (idx, anchor_match, path_noise, item)
        })
        .collect();

    let anchor_hits = rows.iter().filter(|(_, hit, _, _)| *hit).count();
    if anchor_hits == 0 {
        response.results = rows.into_iter().map(|(_, _, _, item)| item).collect();
        return None;
    }

    let noisy_rows = rows.iter().filter(|(_, _, noisy, _)| *noisy).count();
    rows.sort_by(|a, b| {
        b.1.cmp(&a.1) // anchor matches first
            .then_with(|| a.2.cmp(&b.2)) // then non-noisy paths
            .then_with(|| a.0.cmp(&b.0)) // stable original order
    });

    let reordered = rows
        .iter()
        .enumerate()
        .any(|(new_idx, (old_idx, _, _, _))| *old_idx != new_idx);
    response.results = rows.into_iter().map(|(_, _, _, item)| item).collect();

    if !reordered {
        return None;
    }

    Some(format!(
        "Symbol/identifier rerank prioritized {} exact anchor match(es) and demoted {} likely artifact/doc path result(s).",
        anchor_hits, noisy_rows
    ))
}

fn supports_token_fusion(mode: SearchMode) -> bool {
    matches!(
        mode,
        SearchMode::Keyword | SearchMode::Hybrid | SearchMode::Refactor | SearchMode::Exhaustive
    )
}

fn apply_post_rank_fusion(response: &mut SearchResponse, query: &str) -> Option<String> {
    // Compact path rows have no content evidence or calibrated scores. Keep
    // their server order instead of manufacturing a new one from partial path
    // token overlap. Evidence-bearing local rows may still outrank stale
    // server paths.
    if !has_client_rerank_evidence(response) || response.results.len() < 2 {
        return None;
    }
    let compact_paths = is_compact_paths_response(response);

    let tokens = local_snippet_query_tokens(query);
    if tokens.is_empty() {
        return None;
    }
    let normalized_query = normalized_symbol_retry_query(query).to_lowercase();

    let mut rows: Vec<(usize, f64, f64, mcp_types::api::SearchResult)> = response
        .results
        .drain(..)
        .enumerate()
        .map(|(idx, item)| {
            let row_has_evidence = !compact_paths || result_has_rerank_evidence(&item);
            let base_score = if row_has_evidence {
                item.score.unwrap_or(0.0)
            } else {
                0.0
            };
            let content = item.content.as_deref().unwrap_or_default().to_lowercase();
            let path = item.file_path.as_deref().unwrap_or_default().to_lowercase();
            let location = item.location.as_deref().unwrap_or_default().to_lowercase();
            let haystack = format!("{} {} {}", content, path, location);

            let token_hits = if row_has_evidence {
                tokens
                    .iter()
                    .filter(|token| haystack.contains(token.as_str()))
                    .count()
            } else {
                0
            };
            let token_boost = (token_hits.min(3) as f64) * 0.08;
            let exact_boost = if row_has_evidence
                && normalized_query.len() >= 3
                && haystack.contains(&normalized_query)
            {
                0.16
            } else {
                0.0
            };
            let path_boost =
                if row_has_evidence && tokens.iter().any(|token| path.contains(token.as_str())) {
                    0.06
                } else {
                    0.0
                };
            let fusion_score = base_score + token_boost + exact_boost + path_boost;
            (idx, fusion_score, base_score, item)
        })
        .collect();

    rows.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
            .then_with(|| a.0.cmp(&b.0))
    });

    let reordered = rows
        .iter()
        .enumerate()
        .any(|(new_idx, (old_idx, _, _, _))| *old_idx != new_idx);
    response.results = rows
        .into_iter()
        .map(|(_, _, _, item)| item)
        .collect::<Vec<_>>();

    if !reordered {
        return None;
    }

    Some(
        "Applied token-aware post-rank fusion to prioritize exact path/content matches."
            .to_string(),
    )
}

/// Demote keyword search results that do not actually contain any query token
/// in their content or file path. Returns the count of demoted results.
/// Results are heavily demoted (0.1x score, sorted to bottom) rather than removed,
/// so the caller can still see them if no better results exist.
fn demote_keyword_false_positives(response: &mut SearchResponse, query: &str) -> usize {
    // A compact paths response cannot prove a lexical false positive because
    // it intentionally omits the matching content. Its server ranking remains
    // authoritative until richer evidence is available.
    if is_compact_paths_response(response) || response.results.len() < 2 {
        return 0;
    }

    // Split on whitespace AND hyphens/underscores so compound identifiers like
    // "cs-static-page-shell" generate sub-tokens ["cs", "static", "page", "shell"]
    // in addition to the full token.
    let mut tokens: Vec<String> = Vec::new();
    for word in query.split_whitespace() {
        let lower = word.to_lowercase();
        if lower.len() >= 2 {
            tokens.push(lower.clone());
        }
        for sub in lower.split(['-', '_', '.']) {
            if sub.len() >= 2 && sub != lower {
                tokens.push(sub.to_string());
            }
        }
    }
    if tokens.is_empty() {
        return 0;
    }

    let mut demoted = 0usize;
    for item in response.results.iter_mut() {
        let content_lower = item.content.as_deref().unwrap_or_default().to_lowercase();
        let path_lower = item.file_path.as_deref().unwrap_or_default().to_lowercase();
        let location_lower = item.location.as_deref().unwrap_or_default().to_lowercase();

        let has_any_token = tokens.iter().any(|t| {
            content_lower.contains(t) || path_lower.contains(t) || location_lower.contains(t)
        });

        if !has_any_token {
            if let Some(ref mut score) = item.score {
                *score *= 0.1;
            }
            demoted += 1;
        }
    }

    if demoted > 0 {
        response.results.sort_by(|a, b| {
            let sa = a.score.unwrap_or(0.0);
            let sb = b.score.unwrap_or(0.0);
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        });
    }

    demoted
}

/// Maximum files returned by local glob enrichment.
const LOCAL_GLOB_MAX_RESULTS: usize = 200;
/// Maximum matched results returned by local keyword enrichment.
const LOCAL_KEYWORD_MAX_RESULTS: usize = 30;
/// Maximum file size (bytes) to read during local keyword enrichment.
const LOCAL_KEYWORD_MAX_FILE_SIZE: u64 = 1_048_576; // 1 MB
/// Timeout for local filesystem enrichment.
const LOCAL_ENRICH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

fn is_local_keyword_enrichment_query(query: &str) -> bool {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return false;
    }

    // Bound local fallback scans to concise query shapes.
    // Longer natural-language prompts are expensive and noisy for substring scans.
    trimmed.len() <= 160 && trimmed.split_whitespace().count() <= 10
}

fn should_apply_local_enrichment(
    executed_mode: SearchMode,
    query: &str,
    no_hits: bool,
    should_recover_compact_rows: bool,
    scope_invalid: bool,
    scope_fallback_applied: bool,
) -> bool {
    if scope_invalid {
        return false;
    }

    // When scope fallback was applied (workspace-wide retry after project miss),
    // still allow local enrichment on zero results — this is exactly when the
    // local filesystem should backstop the API.
    if scope_fallback_applied && !no_hits {
        return false;
    }

    match executed_mode {
        // Pattern: always try local enrichment on zero results, regardless of
        // whether the query contains glob characters.  For exact file paths or
        // partial path strings, `local_path_enrich` will handle the match via
        // substring comparison instead of glob matching.
        SearchMode::Pattern => no_hits,
        // Exhaustive: ALWAYS enrich with local ripgrep to catch files the
        // BM25 index missed (e.g., small re-export/import files). The API's
        // BM25 pool may not include every occurrence; ripgrep guarantees
        // complete coverage. Deduplication removes overlaps.
        SearchMode::Exhaustive => is_local_keyword_enrichment_query(query),
        SearchMode::Keyword => {
            (no_hits || should_recover_compact_rows) && is_local_keyword_enrichment_query(query)
        }
        // For all other modes (Hybrid, Semantic, Crawl, Refactor, Team),
        // enrich with local results when the API returned nothing and the
        // query shape is suitable for substring/ripgrep scanning.
        _ => no_hits && is_local_keyword_enrichment_query(query),
    }
}

/// Enrich pattern/glob search results with local filesystem matches.
/// Uses the `ignore` crate to respect .gitignore and walk efficiently.
fn local_glob_enrich(
    folder_path: &Path,
    pattern: &str,
    existing_paths: &HashSet<String>,
) -> Vec<mcp_types::api::SearchResult> {
    use globset::Glob;

    // Build glob matcher from the pattern.
    // Support both bare patterns like "*.toml" and qualified like "**/*.toml".
    let glob_pattern = if pattern.contains('/') || pattern.starts_with("**") {
        pattern.to_string()
    } else {
        format!("**/{}", pattern)
    };

    let glob = match Glob::new(&glob_pattern) {
        Ok(g) => g.compile_matcher(),
        Err(_) => return Vec::new(),
    };

    let walker = ignore::WalkBuilder::new(folder_path)
        .hidden(true) // skip hidden files
        .git_ignore(true) // respect .gitignore
        .git_global(true)
        .git_exclude(true)
        .build();

    let mut results = Vec::new();
    for entry in walker {
        if results.len() >= LOCAL_GLOB_MAX_RESULTS {
            break;
        }
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.path();
        let relative = path
            .strip_prefix(folder_path)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();

        if is_artifact_like_path(&relative) {
            continue;
        }
        if !glob.is_match(&relative) && !glob.is_match(path) {
            continue;
        }
        if existing_paths.contains(&relative) {
            continue;
        }
        let lang = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_string());

        results.push(mcp_types::api::SearchResult {
            id: relative.clone(),
            file_path: Some(relative.clone()),
            location: Some(relative),
            language: lang,
            metadata: Some(serde_json::json!({"source": "local_filesystem"})),
            ..Default::default()
        });
    }

    results
}

/// Enrich pattern search results with local filesystem path-substring matches.
/// Used when the pattern query is not a glob (no `*` or `?`) — e.g. an exact
/// file path or partial path fragment. Walks the tree and checks if the query
/// appears as a case-insensitive substring of each relative path.
fn local_path_enrich(
    folder_path: &Path,
    query: &str,
    existing_paths: &HashSet<String>,
) -> Vec<mcp_types::api::SearchResult> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }

    let walker = ignore::WalkBuilder::new(folder_path)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build();

    let mut results = Vec::new();
    for entry in walker {
        if results.len() >= LOCAL_GLOB_MAX_RESULTS {
            break;
        }
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        let path = entry.path();
        let relative = path
            .strip_prefix(folder_path)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();

        if is_artifact_like_path(&relative) {
            continue;
        }
        if existing_paths.contains(&relative) {
            continue;
        }

        // Exact match or case-insensitive substring match on relative path
        if relative == query || relative.to_lowercase().contains(&needle) {
            let lang = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|s| s.to_string());

            let score = if relative == query { 1.0 } else { 0.8 };

            results.push(mcp_types::api::SearchResult {
                id: relative.clone(),
                file_path: Some(relative.clone()),
                location: Some(relative),
                language: lang,
                score: Some(score),
                metadata: Some(serde_json::json!({"source": "local_filesystem", "match_type": "path_substring"})),
                ..Default::default()
            });
        }
    }

    results
}

fn normalize_file_type_filter(file_types: Option<&[String]>) -> Option<HashSet<String>> {
    let mut normalized = HashSet::new();
    for file_type in file_types.unwrap_or(&[]) {
        let trimmed = file_type.trim().trim_start_matches('.');
        if !trimmed.is_empty() {
            normalized.insert(trimmed.to_lowercase());
        }
    }
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn path_matches_file_type_filter(path: &Path, filter: &HashSet<String>) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    filter.contains(&ext.to_lowercase())
}

fn extract_local_match_snippet(
    content: &str,
    match_line_index: usize,
    context_lines: usize,
    max_chars: usize,
) -> String {
    let lines: Vec<&str> = content.lines().collect();
    if lines.is_empty() {
        return String::new();
    }

    let start = match_line_index.saturating_sub(context_lines);
    let end = (match_line_index + context_lines + 1).min(lines.len());
    let mut snippet = lines[start..end].join("\n");
    if snippet.chars().count() > max_chars {
        snippet = snippet.chars().take(max_chars).collect();
        snippet.push_str("...");
    }
    snippet
}

fn local_result_path(folder_path: &Path, file_path: &str) -> Option<PathBuf> {
    let normalized = strip_trailing_line_suffix(&file_path.replace('\\', "/"));
    let trimmed = normalized.trim();
    if trimmed.is_empty() {
        return None;
    }

    let candidate = Path::new(trimmed);
    if candidate.is_absolute() {
        return candidate
            .starts_with(folder_path)
            .then(|| candidate.to_path_buf());
    }

    if candidate
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return None;
    }

    Some(folder_path.join(candidate))
}

fn local_snippet_query_tokens(query: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    if let Some(literal) = extract_quoted_literal(query) {
        let literal = literal.trim().to_ascii_lowercase();
        if literal.len() >= 2 {
            tokens.push(literal);
        }
    }

    let normalized = normalized_symbol_retry_query(query);
    for word in normalized.split_whitespace() {
        let lower = word
            .trim_matches(|c: char| c == '"' || c == '\'' || c == '`')
            .to_ascii_lowercase();
        if lower.len() >= 2 {
            tokens.push(lower.clone());
        }
        for sub in lower.split(['-', '_', '/']) {
            if sub.len() >= 2 && sub != lower {
                tokens.push(sub.to_string());
            }
        }
    }

    tokens.sort();
    tokens.dedup();
    tokens
}

fn local_match_line_index(
    content: &str,
    query: &str,
    fallback_start_line: Option<i64>,
) -> Option<usize> {
    let tokens = local_snippet_query_tokens(query);
    if !tokens.is_empty() {
        for (idx, line) in content.lines().enumerate() {
            let line_lower = line.to_ascii_lowercase();
            if tokens.iter().any(|token| line_lower.contains(token)) {
                return Some(idx);
            }
        }
    }

    fallback_start_line
        .and_then(|line| (line > 0).then_some((line - 1) as usize))
        .or(Some(0))
}

fn mark_local_overlay(item: &mut mcp_types::api::SearchResult) {
    item.origin = Some("local_overlay_filesystem".to_string());

    let mut metadata = item
        .metadata
        .take()
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();
    metadata.insert(
        "snippet_source".to_string(),
        serde_json::json!("local_overlay_filesystem"),
    );
    metadata.insert(
        "source".to_string(),
        serde_json::json!("local_working_tree"),
    );
    item.metadata = Some(serde_json::Value::Object(metadata));
}

/// Drop indexed results whose backing file no longer exists in the local
/// working tree. Only files that resolve under `folder_path` are considered:
/// memory results, build artifacts, and paths outside the local tree (or that
/// can't be resolved locally, e.g. remote/hosted searches) are always kept so
/// this never prunes legitimately-remote hits. Returns the number removed and
/// refreshes `response.total`/`paths` to match.
fn prune_deleted_file_results(response: &mut SearchResponse, folder_path: &Path) -> usize {
    let before = response.results.len();
    let mut removed_paths: HashSet<String> = HashSet::new();

    response.results.retain(|item| {
        if is_memory_result(item) {
            return true;
        }
        let Some(file_path) = item.file_path.as_deref() else {
            return true;
        };
        if is_artifact_like_path(file_path) {
            return true;
        }
        // Only prune when the path resolves under the local folder; otherwise
        // we can't make a trustworthy existence judgement (keep it).
        let Some(local_path) = local_result_path(folder_path, file_path) else {
            return true;
        };
        if local_path.exists() {
            return true;
        }
        removed_paths.insert(file_path.to_string());
        false
    });

    let removed = before.saturating_sub(response.results.len());
    if removed > 0 {
        response.total = Some(response.results.len() as i64);
        if !response.paths.is_empty() {
            response
                .paths
                .retain(|p| !removed_paths.contains(p.as_str()));
        }
    }
    removed
}

fn refresh_indexed_result_snippets_from_local_files(
    response: &mut SearchResponse,
    folder_path: &Path,
    query: &str,
    context_lines: usize,
    content_max_chars: usize,
) -> usize {
    let mut refreshed = 0usize;

    for item in response.results.iter_mut() {
        if is_memory_result(item) {
            continue;
        }

        let Some(file_path) = item.file_path.as_deref() else {
            continue;
        };
        if is_artifact_like_path(file_path) {
            continue;
        }

        let Some(local_path) = local_result_path(folder_path, file_path) else {
            continue;
        };
        let Ok(metadata) = std::fs::metadata(&local_path) else {
            continue;
        };
        if !metadata.is_file() || metadata.len() > LOCAL_KEYWORD_MAX_FILE_SIZE {
            continue;
        }

        let Ok(content) = std::fs::read_to_string(&local_path) else {
            continue;
        };
        let Some(match_line_index) = local_match_line_index(&content, query, item.start_line)
        else {
            continue;
        };

        let snippet = extract_local_match_snippet(
            &content,
            match_line_index,
            context_lines,
            content_max_chars.max(50),
        );
        if snippet.is_empty() {
            continue;
        }

        if item.content.as_deref() != Some(snippet.as_str()) {
            item.content = Some(snippet);
            refreshed += 1;
        }
        if item.start_line.is_none() {
            item.start_line = Some((match_line_index + 1) as i64);
        }
        mark_local_overlay(item);
    }

    refreshed
}

/// Enrich keyword/exhaustive search results by invoking ripgrep on the local tree.
/// Falls back to the naive walker-based scan if `rg` is not available.
#[cfg(test)]
fn local_keyword_enrich(
    folder_path: &Path,
    query: &str,
    existing_paths: &HashSet<String>,
    file_type_filter: Option<&HashSet<String>>,
    context_lines: usize,
    content_max_chars: usize,
    include_existing_path_matches: bool,
) -> Vec<mcp_types::api::SearchResult> {
    local_keyword_enrich_checked(
        folder_path,
        query,
        existing_paths,
        file_type_filter,
        context_lines,
        content_max_chars,
        include_existing_path_matches,
    )
    .results
}

fn local_keyword_enrich_checked(
    folder_path: &Path,
    query: &str,
    existing_paths: &HashSet<String>,
    file_type_filter: Option<&HashSet<String>>,
    context_lines: usize,
    content_max_chars: usize,
    include_existing_path_matches: bool,
) -> LocalEnrichOutcome {
    let normalized_query = normalized_symbol_retry_query(query);
    if normalized_query.is_empty() {
        return LocalEnrichOutcome::default();
    }

    if let Some(diagnostic) = local_enrichment_root_diagnostic(folder_path) {
        return LocalEnrichOutcome::diagnostic(diagnostic);
    }

    match local_keyword_enrich_rg_checked(
        folder_path,
        &normalized_query,
        existing_paths,
        file_type_filter,
        context_lines,
        content_max_chars,
        include_existing_path_matches,
    ) {
        Some(results) => results,
        None => LocalEnrichOutcome::from_results(local_keyword_enrich_fallback(
            folder_path,
            &normalized_query,
            existing_paths,
            file_type_filter,
            context_lines,
            content_max_chars,
            include_existing_path_matches,
        )),
    }
}

fn local_enrichment_root_diagnostic(folder_path: &Path) -> Option<LocalEnrichDiagnostic> {
    let metadata = match std::fs::metadata(folder_path) {
        Ok(metadata) => metadata,
        Err(err) => {
            return Some(LocalEnrichDiagnostic::from_io_error(
                folder_path,
                "metadata",
                err,
            ));
        }
    };

    if !metadata.is_dir() {
        return Some(LocalEnrichDiagnostic::new(
            "invalid_root",
            folder_path,
            "local search root is not a directory",
        ));
    }

    if let Err(err) = std::fs::read_dir(folder_path) {
        return Some(LocalEnrichDiagnostic::from_io_error(
            folder_path,
            "read_dir",
            err,
        ));
    }

    None
}

/// Primary ripgrep-backed local search. Returns `None` if rg is not found.
fn local_keyword_enrich_rg_checked(
    folder_path: &Path,
    query: &str,
    existing_paths: &HashSet<String>,
    file_type_filter: Option<&HashSet<String>>,
    context_lines: usize,
    content_max_chars: usize,
    include_existing_path_matches: bool,
) -> Option<LocalEnrichOutcome> {
    let rg_path = which_rg()?;
    let mut cmd = Command::new(rg_path);
    cmd.arg("--json")
        .arg("--max-count")
        .arg("3")
        .arg("--max-filesize")
        .arg("1M")
        .arg("-i")
        .arg("--no-heading");

    if context_lines > 0 {
        cmd.arg("-C").arg(context_lines.min(6).to_string());
    }

    if let Some(filter) = file_type_filter {
        for ext in filter {
            cmd.arg("--glob").arg(format!("*.{}", ext));
        }
    }

    cmd.arg("--").arg(query).arg(folder_path);

    let output = match cmd.output() {
        Ok(output) => output,
        Err(err) => {
            return Some(LocalEnrichOutcome::diagnostic(
                LocalEnrichDiagnostic::from_io_error(folder_path, "ripgrep", err),
            ));
        }
    };
    if !output.status.success() && output.stdout.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        if output.status.code() == Some(1) && stderr.is_empty() {
            return Some(LocalEnrichOutcome::default());
        }

        let detail = if stderr.is_empty() {
            format!("ripgrep exited with status {} and no stdout", output.status)
        } else {
            format!("ripgrep exited with status {}: {}", output.status, stderr)
        };
        let kind = if stderr.to_ascii_lowercase().contains("permission denied") {
            "permission_denied"
        } else {
            "ripgrep_failed"
        };
        return Some(LocalEnrichOutcome::diagnostic(LocalEnrichDiagnostic::new(
            kind,
            folder_path,
            detail,
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut results: Vec<mcp_types::api::SearchResult> = Vec::new();
    let mut seen_paths: HashSet<String> = HashSet::new();

    for line in stdout.lines() {
        if results.len() >= LOCAL_KEYWORD_MAX_RESULTS {
            break;
        }
        let parsed: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if parsed.get("type").and_then(|v| v.as_str()) != Some("match") {
            continue;
        }
        let data = match parsed.get("data") {
            Some(d) => d,
            None => continue,
        };

        let abs_path_str = data
            .get("path")
            .and_then(|p| p.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("");
        let abs_path = Path::new(abs_path_str);
        let relative = abs_path
            .strip_prefix(folder_path)
            .unwrap_or(abs_path)
            .to_string_lossy()
            .to_string();

        if is_artifact_like_path(&relative) {
            continue;
        }
        if existing_paths.contains(&relative) && !include_existing_path_matches {
            continue;
        }
        if seen_paths.contains(&relative) {
            continue;
        }
        seen_paths.insert(relative.clone());

        let line_number = data.get("line_number").and_then(|n| n.as_i64());
        let match_text = data
            .get("lines")
            .and_then(|l| l.get("text"))
            .and_then(|t| t.as_str())
            .unwrap_or("");

        let snippet = if !match_text.is_empty() {
            let truncated: String = match_text.chars().take(content_max_chars.max(50)).collect();
            Some(truncated)
        } else {
            None
        };

        let lang = abs_path
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_string());

        results.push(mcp_types::api::SearchResult {
            id: relative.clone(),
            content: snippet,
            score: Some(1.0),
            file_path: Some(relative.clone()),
            location: Some(relative),
            start_line: line_number,
            language: lang,
            metadata: Some(
                serde_json::json!({"source": "local_ripgrep", "match": "content", "snippet_source": "rg"}),
            ),
            ..Default::default()
        });
    }

    let diagnostic = if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        (!stderr.is_empty()).then(|| {
            let kind = if stderr.to_ascii_lowercase().contains("permission denied") {
                "permission_denied"
            } else {
                "ripgrep_partial_failure"
            };
            LocalEnrichDiagnostic::new(
                kind,
                folder_path,
                format!(
                    "ripgrep returned partial results with status {}: {}",
                    output.status, stderr
                ),
            )
        })
    } else {
        None
    };

    Some(LocalEnrichOutcome {
        results,
        diagnostic,
    })
}

fn which_rg() -> Option<std::path::PathBuf> {
    static RG_PATH: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    RG_PATH
        .get_or_init(|| {
            for candidate in &["rg", "/usr/bin/rg", "/usr/local/bin/rg"] {
                if std::process::Command::new(candidate)
                    .arg("--version")
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false)
                {
                    return Some(std::path::PathBuf::from(candidate));
                }
            }
            None
        })
        .clone()
}

/// Fallback walker-based scan when ripgrep is not available.
fn local_keyword_enrich_fallback(
    folder_path: &Path,
    query: &str,
    existing_paths: &HashSet<String>,
    file_type_filter: Option<&HashSet<String>>,
    context_lines: usize,
    content_max_chars: usize,
    include_existing_path_matches: bool,
) -> Vec<mcp_types::api::SearchResult> {
    let query_lower = query.to_lowercase();
    let walker = ignore::WalkBuilder::new(folder_path)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .build();

    let mut results = Vec::new();
    for entry in walker {
        if results.len() >= LOCAL_KEYWORD_MAX_RESULTS {
            break;
        }
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            if meta.len() > LOCAL_KEYWORD_MAX_FILE_SIZE {
                continue;
            }
        }
        let path = entry.path();
        let relative = path
            .strip_prefix(folder_path)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();

        if is_artifact_like_path(&relative) {
            continue;
        }
        if existing_paths.contains(&relative) && !include_existing_path_matches {
            continue;
        }
        if let Some(filter) = file_type_filter {
            if !path_matches_file_type_filter(path, filter) {
                continue;
            }
        }

        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if file_name.to_lowercase().contains(&query_lower) {
            let lang = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|s| s.to_string());
            results.push(mcp_types::api::SearchResult {
                id: relative.clone(),
                score: Some(1.0),
                file_path: Some(relative.clone()),
                location: Some(relative),
                language: lang,
                metadata: Some(
                    serde_json::json!({"source": "local_filesystem", "match": "filename"}),
                ),
                ..Default::default()
            });
            continue;
        }

        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if content.to_lowercase().contains(&query_lower) {
            let match_line_index = content
                .lines()
                .enumerate()
                .find(|(_, line)| line.to_lowercase().contains(&query_lower))
                .map(|(i, _)| i);
            let start_line = match_line_index.map(|line| (line + 1) as i64);
            let snippet = match_line_index
                .map(|line| {
                    extract_local_match_snippet(
                        &content,
                        line,
                        context_lines,
                        content_max_chars.max(50),
                    )
                })
                .filter(|value| !value.is_empty());

            let lang = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|s| s.to_string());
            results.push(mcp_types::api::SearchResult {
                id: relative.clone(),
                content: snippet,
                score: Some(1.0),
                file_path: Some(relative.clone()),
                location: Some(relative),
                start_line,
                language: lang,
                metadata: Some(
                    serde_json::json!({"source": "local_filesystem", "match": "content", "snippet_source": "local"}),
                ),
                ..Default::default()
            });
        }
    }

    results
}

async fn run_search_for_mode(
    client: &ContextStreamClient,
    requested_mode: SearchMode,
    params: SearchParams,
    original_query: &str,
    allow_broad_fallbacks: bool,
) -> Result<(SearchResponse, SearchMode, Option<String>, Option<Uuid>)> {
    let normalized_retry_query = normalized_symbol_retry_query(original_query);
    // Surrounding quotes are caller syntax for an exact literal; they are not
    // part of the literal sent to the keyword index. Normalizing before the
    // primary request avoids paying for a guaranteed quoted miss followed by
    // the same keyword request without quotes.
    let quoted_keyword_literal = (requested_mode == SearchMode::Keyword)
        .then(|| extract_quoted_literal(original_query))
        .flatten();
    // A single unquoted identifier uses the server's bounded keyword lane.
    // That lane races sparse retrieval with exact definition/content lookup:
    // an entirely empty sparse result retains full exact recall, while fuzzy
    // sub-token noise gets a short absence budget and is suppressed. Treat an
    // empty response as terminal for this fast request instead of silently
    // serializing exhaustive + keyword + refactor calls. Callers that require
    // a complete occurrence or structural proof can request exhaustive or
    // refactor explicitly. Quoted literals keep their existing completeness
    // fallback, and memory/cursor requests retain their established routing.
    let bounded_identifier_keyword_path = requested_mode == SearchMode::Keyword
        && quoted_keyword_literal.is_none()
        && params.cursor.is_none()
        && params.include_memory != Some(true)
        && is_identifier_query(&normalized_retry_query);
    let symbol_anchor_terms = extract_symbol_anchor_terms(&normalized_retry_query);
    let strict_symbol_first_pass = looks_like_symbol_anchor_query(&normalized_retry_query);

    // Route to the appropriate search endpoint.
    // Auto-selected and explicit hybrid both use /search/hybrid (matching TypeScript).
    let semantic_prefers_hybrid = requested_mode == SearchMode::Semantic
        && prefers_hybrid_for_code_location_query(original_query);
    let (mut result, mut executed_mode, mut mode_fallback_note, mut selected_learning_request_id) =
        match requested_mode {
            SearchMode::Hybrid => {
                if strict_symbol_first_pass {
                    if let Ok(keyword_attempt) =
                        execute_api_search_attempt(client, SearchMode::Keyword, params.clone())
                            .await
                    {
                        if response_has_hits(&keyword_attempt.response)
                            && response_has_symbol_anchor_match(
                                &keyword_attempt.response,
                                &symbol_anchor_terms,
                            )
                        {
                            (
                            keyword_attempt.response,
                            SearchMode::Keyword,
                            Some(
                                "Hybrid query looked symbol-heavy; used strict keyword first-pass and kept anchor-aligned matches."
                                    .to_string(),
                            ),
                            keyword_attempt.learning_request_id,
                        )
                        } else if let Ok(refactor_attempt) =
                            execute_api_search_attempt(client, SearchMode::Refactor, params.clone())
                                .await
                        {
                            if response_has_hits(&refactor_attempt.response) {
                                (
                                refactor_attempt.response,
                                SearchMode::Refactor,
                                Some(
                                    "Hybrid query looked symbol-heavy; keyword first-pass was weak, so refactor search was used for symbol coverage."
                                        .to_string(),
                                ),
                                refactor_attempt.learning_request_id,
                            )
                            } else {
                                let hybrid_attempt = execute_api_search_attempt(
                                    client,
                                    SearchMode::Hybrid,
                                    params.clone(),
                                )
                                .await?;
                                (
                                hybrid_attempt.response,
                                SearchMode::Hybrid,
                                Some(
                                    "Hybrid query looked symbol-heavy; strict keyword/refactor pass had no anchor hits, so hybrid fallback was used."
                                        .to_string(),
                                ),
                                hybrid_attempt.learning_request_id,
                            )
                            }
                        } else {
                            let hybrid_attempt = execute_api_search_attempt(
                                client,
                                SearchMode::Hybrid,
                                params.clone(),
                            )
                            .await?;
                            (
                            hybrid_attempt.response,
                            SearchMode::Hybrid,
                            Some(
                                "Hybrid query looked symbol-heavy; strict first-pass fell back to hybrid."
                                    .to_string(),
                            ),
                            hybrid_attempt.learning_request_id,
                        )
                        }
                    } else {
                        let hybrid_attempt =
                            execute_api_search_attempt(client, SearchMode::Hybrid, params.clone())
                                .await?;
                        (
                        hybrid_attempt.response,
                        SearchMode::Hybrid,
                        Some(
                            "Hybrid query looked symbol-heavy; strict keyword first-pass failed and fell back to hybrid."
                                .to_string(),
                        ),
                        hybrid_attempt.learning_request_id,
                    )
                    }
                } else {
                    let (resp, note, learning_request_id) =
                        hybrid_with_fast_fallback(client, params.clone()).await?;
                    let executed = if note.is_some() {
                        SearchMode::Semantic
                    } else {
                        SearchMode::Hybrid
                    };
                    (
                        normalize_response(resp),
                        executed,
                        note,
                        learning_request_id,
                    )
                }
            }
            SearchMode::Semantic => {
                if semantic_prefers_hybrid {
                    let attempt =
                        execute_api_search_attempt(client, SearchMode::Hybrid, params.clone())
                            .await?;
                    (
                    attempt.response,
                    SearchMode::Hybrid,
                    Some(
                        "Semantic mode query looked like a code-location/bugfix question; used hybrid for faster and more precise code retrieval."
                            .to_string(),
                    ),
                    attempt.learning_request_id,
                )
                } else {
                    let attempt =
                        execute_api_search_attempt(client, SearchMode::Semantic, params.clone())
                            .await?;
                    (
                        attempt.response,
                        SearchMode::Semantic,
                        None,
                        attempt.learning_request_id,
                    )
                }
            }
            SearchMode::Keyword => {
                let mut keyword_params = params.clone();
                if let Some(literal) = quoted_keyword_literal.as_ref() {
                    keyword_params.query = literal.clone();
                }
                let attempt =
                    execute_api_search_attempt(client, SearchMode::Keyword, keyword_params).await?;
                let note = if quoted_keyword_literal.is_some() {
                    Some("Normalized surrounding quotes before exact keyword search.".to_string())
                } else if bounded_identifier_keyword_path {
                    Some(
                        "Bounded identifier lookup; use exhaustive for complete coverage."
                            .to_string(),
                    )
                } else {
                    None
                };
                (
                    attempt.response,
                    SearchMode::Keyword,
                    note,
                    attempt.learning_request_id,
                )
            }
            SearchMode::Pattern => {
                let attempt =
                    execute_api_search_attempt(client, SearchMode::Pattern, params.clone()).await?;
                (
                    attempt.response,
                    SearchMode::Pattern,
                    None,
                    attempt.learning_request_id,
                )
            }
            SearchMode::Exhaustive => {
                let attempt =
                    execute_api_search_attempt(client, SearchMode::Exhaustive, params.clone())
                        .await?;
                (
                    attempt.response,
                    SearchMode::Exhaustive,
                    None,
                    attempt.learning_request_id,
                )
            }
            SearchMode::Refactor => {
                let attempt =
                    execute_api_search_attempt(client, SearchMode::Refactor, params.clone())
                        .await?;
                (
                    attempt.response,
                    SearchMode::Refactor,
                    None,
                    attempt.learning_request_id,
                )
            }
            SearchMode::Team => {
                match execute_api_search_attempt(client, SearchMode::Team, params.clone()).await {
                    Ok(attempt) => (
                        attempt.response,
                        SearchMode::Team,
                        None,
                        attempt.learning_request_id,
                    ),
                    Err(err) if should_fallback_from_team_error(&err) => {
                        let attempt =
                            execute_api_search_attempt(client, SearchMode::Hybrid, params.clone())
                                .await?;
                        (
                    attempt.response,
                    SearchMode::Hybrid,
                    Some(
                        "Team mode is unavailable for this workspace; fell back to hybrid search."
                            .to_string(),
                    ),
                    attempt.learning_request_id,
                )
                    }
                    Err(err) => return Err(err),
                }
            }
            SearchMode::Crawl => {
                let attempt =
                    execute_api_search_attempt(client, SearchMode::Crawl, params.clone()).await?;
                (
                    attempt.response,
                    SearchMode::Crawl,
                    None,
                    attempt.learning_request_id,
                )
            }
            // Guided mode is handled by `SearchTool::execute_guided_search`
            // before this server-routing helper. Keep a safe raw-search fallback
            // for any future internal caller that bypasses that short-circuit.
            SearchMode::Guided => {
                let attempt =
                    execute_api_search_attempt(client, SearchMode::Hybrid, params.clone()).await?;
                (
                    attempt.response,
                    SearchMode::Hybrid,
                    Some(
                        "Guided Search was unavailable; served hybrid raw evidence instead."
                            .to_string(),
                    ),
                    attempt.learning_request_id,
                )
            }
            // Fuzzy + Vector modes are handled by
            // `SearchTool::execute_atlas_{fuzzy,vector}` before this
            // server-routing logic runs. If we ever reach here (e.g. some
            // future caller bypasses the short-circuit), fall back to
            // keyword search with a compatibility note.
            SearchMode::Fuzzy => {
                let attempt =
                    execute_api_search_attempt(client, SearchMode::Keyword, params.clone()).await?;
                (
                    attempt.response,
                    SearchMode::Keyword,
                    Some(
                        "fuzzy mode is only available on hosted/remote deployments; \
                     fell back to keyword search."
                            .to_string(),
                    ),
                    attempt.learning_request_id,
                )
            }
            SearchMode::Vector => {
                let attempt =
                    execute_api_search_attempt(client, SearchMode::Semantic, params.clone())
                        .await?;
                (
                    attempt.response,
                    SearchMode::Semantic,
                    Some(
                        "vector mode is only available on hosted/remote deployments; \
                     fell back to semantic search."
                            .to_string(),
                    ),
                    attempt.learning_request_id,
                )
            }
        };
    let allow_keyword_broad_fallbacks = allow_broad_fallbacks && !strict_symbol_first_pass;
    let mut quoted_exhaustive_attempted = false;

    // Retry with semantic if hybrid results are low-confidence for NL queries.
    if should_retry_semantic_fallback(original_query, requested_mode, &result) {
        if let Ok(semantic_attempt) =
            execute_api_search_attempt(client, SearchMode::Semantic, params.clone()).await
        {
            if should_prefer_semantic_results(original_query, &result, &semantic_attempt.response) {
                result = semantic_attempt.response;
                executed_mode = SearchMode::Semantic;
                selected_learning_request_id = semantic_attempt.learning_request_id;
                mode_fallback_note = append_note(
                    mode_fallback_note,
                    "Hybrid results looked low-confidence for this natural-language query; retried with semantic and used semantic results.",
                );
            }
        }
    }

    // A normalized exact keyword miss needs one complete literal fallback.
    // Exhaustive search already performs index retrieval plus literal live-file
    // coverage, so a separate pattern request only adds another serial round
    // trip without improving the final completeness guarantee.
    if requested_mode == SearchMode::Keyword && !response_has_hits(&result) {
        if let Some(literal) = quoted_keyword_literal.as_ref() {
            quoted_exhaustive_attempted = true;
            let mut exhaustive_params = params.clone();
            exhaustive_params.query = literal.clone();
            if let Ok(exhaustive_attempt) =
                execute_api_search_attempt(client, SearchMode::Exhaustive, exhaustive_params).await
            {
                if response_has_hits(&exhaustive_attempt.response) {
                    result = exhaustive_attempt.response;
                    executed_mode = SearchMode::Exhaustive;
                    selected_learning_request_id = exhaustive_attempt.learning_request_id;
                    mode_fallback_note = append_note(
                        mode_fallback_note,
                        "Exact keyword search returned no results; retried exhaustive search for complete literal coverage.",
                    );
                }
            }
        }
    }

    // Refactor/exhaustive can miss while keyword still matches on some indexes.
    // Retry keyword to reduce false-negative zero-result outcomes.
    if matches!(
        requested_mode,
        SearchMode::Refactor | SearchMode::Exhaustive
    ) && !response_has_hits(&result)
        && allow_broad_fallbacks
    {
        if let Ok(keyword_attempt) =
            execute_api_search_attempt(client, SearchMode::Keyword, params.clone()).await
        {
            if response_has_hits(&keyword_attempt.response) {
                result = keyword_attempt.response;
                executed_mode = SearchMode::Keyword;
                selected_learning_request_id = keyword_attempt.learning_request_id;
                mode_fallback_note = append_note(
                    mode_fallback_note,
                    "Requested mode returned no results; retried keyword search and found matches.",
                );
            }
        }
    }

    // For keyword misses, progressively retry symbol-aware and semantic-friendly
    // modes to reduce zero-result false negatives.
    if requested_mode == SearchMode::Keyword
        && !response_has_hits(&result)
        && !bounded_identifier_keyword_path
    {
        if should_retry_keyword_with_symbol_modes(&normalized_retry_query) {
            if let Ok(refactor_attempt) =
                execute_api_search_attempt(client, SearchMode::Refactor, params.clone()).await
            {
                if response_has_hits(&refactor_attempt.response) {
                    result = refactor_attempt.response;
                    executed_mode = SearchMode::Refactor;
                    selected_learning_request_id = refactor_attempt.learning_request_id;
                    mode_fallback_note = append_note(
                        mode_fallback_note,
                        "Keyword search returned no results; retried refactor search for identifier matching.",
                    );
                }
            }

            if !response_has_hits(&result)
                && !quoted_exhaustive_attempted
                && !is_natural_language_phrase_query(original_query)
            {
                if let Ok(exhaustive_attempt) =
                    execute_api_search_attempt(client, SearchMode::Exhaustive, params.clone()).await
                {
                    if response_has_hits(&exhaustive_attempt.response) {
                        result = exhaustive_attempt.response;
                        executed_mode = SearchMode::Exhaustive;
                        selected_learning_request_id = exhaustive_attempt.learning_request_id;
                        mode_fallback_note = append_note(
                            mode_fallback_note,
                            "Keyword search returned no results; retried exhaustive search for complete identifier coverage.",
                        );
                    }
                }
            }
        }

        if !response_has_hits(&result)
            && allow_keyword_broad_fallbacks
            && should_retry_keyword_with_semantic(&normalized_retry_query)
        {
            if let Ok(semantic_attempt) =
                execute_api_search_attempt(client, SearchMode::Semantic, params.clone()).await
            {
                if response_has_hits(&semantic_attempt.response) {
                    result = semantic_attempt.response;
                    executed_mode = SearchMode::Semantic;
                    selected_learning_request_id = semantic_attempt.learning_request_id;
                    mode_fallback_note = append_note(
                        mode_fallback_note,
                        "Keyword search returned no results; retried semantic search for natural-language intent.",
                    );
                }
            }
        }

        if !response_has_hits(&result) && allow_keyword_broad_fallbacks {
            if let Ok(hybrid_attempt) =
                execute_api_search_attempt(client, SearchMode::Hybrid, params.clone()).await
            {
                if response_has_hits(&hybrid_attempt.response) {
                    result = hybrid_attempt.response;
                    executed_mode = SearchMode::Hybrid;
                    selected_learning_request_id = hybrid_attempt.learning_request_id;
                    mode_fallback_note = append_note(
                        mode_fallback_note,
                        "Keyword search returned no results; retried hybrid search as a broad fallback.",
                    );
                }
            }
        }
    }

    if allow_broad_fallbacks && !response_has_hits(&result) {
        if let Some((path_result, path_mode, note, learning_request_id)) =
            try_path_aware_fallbacks(client, &params, original_query).await
        {
            result = path_result;
            executed_mode = path_mode;
            selected_learning_request_id = learning_request_id;
            mode_fallback_note = append_note(mode_fallback_note, note);
        }
    }

    if let Some(note) = scope_remediation_note(&result) {
        mode_fallback_note = append_note(mode_fallback_note, &note);
    }

    Ok((
        result,
        executed_mode,
        mode_fallback_note,
        selected_learning_request_id,
    ))
}

/// Check if team mode should gracefully fall back to hybrid.
fn should_fallback_from_team_error(err: &Error) -> bool {
    match err {
        Error::Http { status, .. } => matches!(status, 400 | 404 | 422),
        Error::Validation(msg) => msg.to_lowercase().contains("team"),
        _ => false,
    }
}

/// Highest score present in a search result set.
fn max_result_score(result: &SearchResponse) -> f64 {
    result
        .results
        .iter()
        .filter_map(|item| item.score)
        .fold(0.0, f64::max)
}

fn response_has_hits(result: &SearchResponse) -> bool {
    !result.results.is_empty() || result.total.unwrap_or(0) > 0
}

fn served_api_learning_receipt(
    result: &SearchResponse,
    learning_request_id: Option<Uuid>,
) -> Option<Uuid> {
    (!result.results.is_empty())
        .then_some(learning_request_id)
        .flatten()
}

fn search_response_structured_value(result: &SearchResponse) -> Value {
    serde_json::to_value(result).unwrap_or_default()
}

fn ensure_search_query_echo(result: &mut SearchResponse, query: &str) {
    // Compact/count API responses may omit the echoed query. The MCP
    // contract must still identify exactly what was executed so agents and
    // fail-closed benchmarks can bind the answer to the request.
    result.query = Some(query.to_string());
}

fn normalize_count_index_trust(result: &mut SearchResponse, output_format: Option<&str>) {
    if !output_format.is_some_and(|format| format.eq_ignore_ascii_case("count"))
        || !result.results.is_empty()
    {
        return;
    }

    // Count responses deliberately omit result rows. A backend `false`
    // coverage verdict in that shape describes the omitted candidate rows,
    // not the rows actually returned to the caller. Keep row-level trust
    // unknown instead of reporting a false incomplete-coverage alarm.
    if let Some(trust) = result.index_trust.as_mut() {
        trust.result_generation_coverage_complete = None;
        trust.result_generation_consistent = None;
    }
    result.result_generation_min = None;
    result.result_generation_max = None;
}

fn refactor_cursor_continuation_note(result: &SearchResponse) -> Option<&'static str> {
    result.next_cursor.as_ref().map(|_| {
        "[CONTINUATION] More refactor matches are available. Repeat the same mode=\"refactor\" search using the structured `next_cursor` value as `cursor`."
    })
}

fn result_symbol_text(item: &mcp_types::api::SearchResult) -> String {
    let mut segments = Vec::new();
    if let Some(path) = item.file_path.as_deref() {
        segments.push(path);
    }
    if let Some(location) = item.location.as_deref() {
        segments.push(location);
    }
    if let Some(breadcrumb) = item.breadcrumb.as_deref() {
        segments.push(breadcrumb);
    }
    if let Some(content) = item.content.as_deref() {
        segments.push(content);
    }
    segments.join(" ").to_lowercase()
}

fn result_matches_symbol_anchors(item: &mcp_types::api::SearchResult, anchors: &[String]) -> bool {
    if anchors.is_empty() {
        return false;
    }
    let haystack = result_symbol_text(item);
    anchors.iter().any(|anchor| haystack.contains(anchor))
}

fn response_has_symbol_anchor_match(result: &SearchResponse, anchors: &[String]) -> bool {
    result
        .results
        .iter()
        .any(|item| result_matches_symbol_anchors(item, anchors))
}

fn local_enrichment_unavailable_warning_for_response(
    query: &str,
    response: &SearchResponse,
    diagnostic: Option<&LocalEnrichDiagnostic>,
    readable_session_folder: Option<&str>,
) -> Option<String> {
    let diagnostic = diagnostic?;
    let normalized_query = normalized_symbol_retry_query(query);
    if !is_identifier_query(&normalized_query) && !looks_like_symbol_anchor_query(query) {
        return None;
    }

    let anchors = extract_symbol_anchor_terms(&normalized_query);
    if anchors.is_empty() || response_has_symbol_anchor_match(response, &anchors) {
        return None;
    }

    // Hosted remote gateway (or headless process): no local filesystem view of
    // the project exists at all, so enrichment can never run and the ingest
    // advice is unactionable — this fired on every identifier-shaped search.
    // The structured scope fields already convey index usability silently.
    let Some(session_folder) = readable_session_folder else {
        tracing::debug!(
            folder = %diagnostic.folder_path,
            kind = %diagnostic.kind,
            "suppressing LOCAL_ENRICHMENT_UNAVAILABLE: process has no local view of the project"
        );
        return None;
    };

    // If the scan root is missing on this machine (cross-machine index root)
    // the advice must point at the folder that IS readable here — advising an
    // ingest of the missing root re-creates the drift it complains about.
    let refresh_hint_path = if std::path::Path::new(&diagnostic.folder_path).is_dir() {
        diagnostic.folder_path.as_str()
    } else {
        session_folder
    };

    Some(format!(
        "[LOCAL_ENRICHMENT_UNAVAILABLE] Optional local filesystem enrichment could not scan `{}` ({}: {}); hosted indexed results are still usable. {}",
        diagnostic.folder_path,
        diagnostic.kind,
        diagnostic.detail,
        hosted_index_refresh_instruction(refresh_hint_path)
    ))
}

/// Whether hybrid results for this query should be retried semantically.
fn adaptive_hybrid_retry_threshold(query: &str) -> f64 {
    if looks_like_symbol_anchor_query(query) || is_identifier_query(query) {
        return 0.4;
    }
    if contains_code_identifiers(query) {
        return 0.48;
    }
    if QUESTION_WORDS
        .iter()
        .any(|word| query.trim_start().to_ascii_lowercase().starts_with(word))
    {
        return 0.6;
    }
    HYBRID_LOW_CONFIDENCE_SCORE
}

fn adaptive_semantic_switch_improvement(query: &str) -> f64 {
    if contains_code_identifiers(query) || looks_like_symbol_anchor_query(query) {
        0.04
    } else {
        SEMANTIC_SWITCH_MIN_IMPROVEMENT
    }
}

fn should_retry_semantic_fallback(query: &str, mode: SearchMode, result: &SearchResponse) -> bool {
    if mode != SearchMode::Hybrid {
        return false;
    }

    // Only retry for queries that have a natural-language component.
    // The old check `recommend_search_mode == Semantic` was too strict —
    // it excluded NL queries that happened to contain UI component terms
    // like "page", "layout", "route" (which route to Hybrid, not Semantic).
    // Instead, skip retry only for clearly structural/symbol queries.
    let recommended = recommend_search_mode(query, None).0;
    let is_structural = matches!(
        recommended,
        SearchMode::Pattern | SearchMode::Exhaustive | SearchMode::Refactor | SearchMode::Team
    );
    if is_structural {
        return false;
    }
    // Skip for single-token identifier queries — these are keyword territory.
    if is_identifier_query(query) || looks_like_symbol_anchor_query(query) {
        return false;
    }

    let has_hits = !result.results.is_empty() || result.total.unwrap_or(0) > 0;
    if !has_hits {
        return true;
    }

    // Count-only responses can legitimately have hits with no result rows.
    if result.results.is_empty() {
        return false;
    }

    max_result_score(result) < adaptive_hybrid_retry_threshold(query)
}

/// Whether semantic results are strong enough to replace hybrid output.
fn should_prefer_semantic_results(
    query: &str,
    hybrid_result: &SearchResponse,
    semantic_result: &SearchResponse,
) -> bool {
    if semantic_result.results.is_empty() {
        return false;
    }
    if hybrid_result.results.is_empty() {
        return true;
    }

    let hybrid_top = max_result_score(hybrid_result);
    let semantic_top = max_result_score(semantic_result);
    semantic_top > hybrid_top + adaptive_semantic_switch_improvement(query)
}

/// Whether keyword mode should retry semantic search on zero results.
fn should_retry_keyword_with_semantic(query: &str) -> bool {
    matches!(
        recommend_search_mode(query, None).0,
        SearchMode::Semantic | SearchMode::Hybrid
    )
}

/// Whether keyword mode should retry symbol-oriented modes on zero results.
fn should_retry_keyword_with_symbol_modes(query: &str) -> bool {
    is_identifier_query(query) || looks_like_symbol_anchor_query(query)
}

fn maybe_trigger_targeted_reingest(
    client: &ContextStreamClient,
    workspace_id: Option<Uuid>,
    resolved_project_id: Option<Uuid>,
    checkout_root: Option<&str>,
    local_probe: Option<&LocalPathProbe>,
    index_health: Option<&IndexHealth>,
) -> Option<String> {
    let local_probe = local_probe?;
    let index_health = index_health?;
    if !index_health.should_refresh() {
        return None;
    }
    let project_id = resolved_project_id?;
    let checkout_root = checkout_root?;
    let bound_workspace_id =
        mcp_session::auto_init::checkout_binding_workspace(checkout_root, project_id)?;
    if workspace_id.is_some_and(|hinted| hinted != bound_workspace_id) {
        return None;
    }
    match ContextStreamClient::targeted_text_file_decision(
        checkout_root,
        &local_probe.absolute_path,
    ) {
        TargetedFileDecision::Upload(_) | TargetedFileDecision::Delete(_) => {}
        TargetedFileDecision::Reject => return None,
    }

    let client = client.clone();
    let path_for_log = checkout_root.to_string();
    let target_path = local_probe.absolute_path.clone();
    tokio::spawn(async move {
        let Some(_) = validated_checkout_content_workspace(
            &client,
            &path_for_log,
            Some(bound_workspace_id),
            project_id,
            "targeted_reingest",
        )
        .await
        else {
            return;
        };
        let (files, deleted_paths) =
            match ContextStreamClient::targeted_text_file_decision(&path_for_log, &target_path) {
                TargetedFileDecision::Upload(payload) => (vec![payload], Vec::new()),
                TargetedFileDecision::Delete(path) => (Vec::new(), vec![path]),
                TargetedFileDecision::Reject => return,
            };
        if mcp_session::auto_init::checkout_binding_workspace(&path_for_log, project_id)
            != Some(bound_workspace_id)
        {
            tracing::warn!(
                path = %path_for_log,
                project_id = %project_id,
                "targeted search repair skipped because checkout identity changed while reading the payload"
            );
            return;
        }
        match client
            .ingest_files_from_hook(
                project_id,
                bound_workspace_id,
                files,
                deleted_paths,
                true,
                Some("targeted_reingest"),
                Some(&path_for_log),
                false,
                false,
            )
            .await
        {
            Ok(result) => {
                let files = result.files_indexed;
                tracing::debug!(
                    "targeted re-index completed: {} files indexed from {}",
                    files,
                    path_for_log
                );
            }
            Err(err) => {
                tracing::debug!("targeted re-index failed for {}: {}", path_for_log, err);
            }
        }
    });

    Some(format!(
        "Detected index drift for local path `{}`; scheduled a checkout-validated exact-path repair on `{}`. Retry this search shortly.",
        local_probe.display_path,
        checkout_root
    ))
}

/// Parse `branch:`, `lang:`, `language:`, `path:`, `recent:Nd`,
/// `project:<uuid>`, and `decision:<uuid>` tokens out of a vector
/// search query. Returns the residual query (with parsed tokens
/// stripped), the filled-in [`AtlasVectorFilter`], and an
/// optionally-parsed project UUID that overrides the caller-supplied
/// scope when both are present.
fn parse_vector_filters(
    query: &str,
) -> (
    String,
    mcp_types::atlas_layer::AtlasVectorFilter,
    Option<Uuid>,
) {
    use mcp_types::atlas_layer::AtlasVectorFilter;
    let mut filter = AtlasVectorFilter::default();
    let mut project_id: Option<Uuid> = None;
    let mut residual: Vec<&str> = Vec::new();

    for token in query.split_whitespace() {
        let Some((key, value)) = token.split_once(':') else {
            residual.push(token);
            continue;
        };
        let value_trim = value.trim_matches(|c: char| c == '\'' || c == '"');
        match key.to_ascii_lowercase().as_str() {
            "branch" if !value_trim.is_empty() => {
                filter.branch = Some(value_trim.to_string());
            }
            "lang" | "language" if !value_trim.is_empty() => {
                filter.language = Some(value_trim.to_ascii_lowercase());
            }
            "path" if !value_trim.is_empty() => {
                filter.path_prefix = Some(value_trim.to_string());
            }
            "recent" if !value_trim.is_empty() => {
                if let Some(d) = parse_recent_duration(value_trim) {
                    filter.updated_after = Some(chrono::Utc::now() - d);
                } else {
                    residual.push(token);
                }
            }
            "project" if !value_trim.is_empty() => {
                if let Ok(pid) = Uuid::parse_str(value_trim) {
                    project_id = Some(pid);
                } else {
                    residual.push(token);
                }
            }
            "decision" if !value_trim.is_empty() => {
                if let Ok(did) = Uuid::parse_str(value_trim) {
                    filter.decision_id = Some(did);
                } else {
                    residual.push(token);
                }
            }
            _ => residual.push(token),
        }
    }

    (residual.join(" "), filter, project_id)
}

/// Parse `7d`, `4h`, `30m` → [`chrono::Duration`]. Returns `None` on
/// anything else (`garbage`, `1y` — intentionally not supported).
fn parse_recent_duration(raw: &str) -> Option<chrono::Duration> {
    let raw = raw.trim();
    let (num_str, unit) = raw.split_at(raw.len().saturating_sub(1));
    let n: i64 = num_str.parse().ok()?;
    match unit {
        "d" | "D" => Some(chrono::Duration::days(n)),
        "h" | "H" => Some(chrono::Duration::hours(n)),
        "m" | "M" => Some(chrono::Duration::minutes(n)),
        _ => None,
    }
}

/// Render a human-friendly summary of an [`AtlasVectorFilter`].
fn summarize_filter(scope: &mcp_types::atlas_layer::AtlasVectorScope) -> String {
    let f = &scope.filter;
    let mut parts = Vec::new();
    if let Some(pid) = scope.project_id {
        parts.push(format!("project={}", pid));
    }
    if let Some(b) = &f.branch {
        parts.push(format!("branch={}", b));
    }
    if let Some(l) = &f.language {
        parts.push(format!("lang={}", l));
    }
    if let Some(p) = &f.path_prefix {
        parts.push(format!("path={}*", p));
    }
    if let Some(ua) = f.updated_after {
        parts.push(format!("updated>={}", ua.to_rfc3339()));
    }
    if let Some(d) = f.decision_id {
        parts.push(format!("decision={}", d));
    }
    if parts.is_empty() {
        "<none>".to_string()
    } else {
        format!("{{{}}}", parts.join(", "))
    }
}

/// Input for the unified search tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchInput {
    pub query: String,
    pub mode: Option<String>,
    /// Optional tokenizer encoding used only for whole-wire budget accounting.
    /// `encoding` is accepted as a compatibility alias.
    #[serde(default, alias = "encoding", skip_serializing_if = "Option::is_none")]
    pub tokenizer: Option<String>,
    /// The task behind a guided search. This lets the Navigator explain where
    /// to work rather than merely restating the literal query.
    pub intent: Option<String>,
    pub workspace_id: Option<String>,
    pub project_id: Option<String>,
    pub limit: Option<i64>,
    pub file_types: Option<Vec<String>>,
    pub include_content: Option<bool>,
    pub include_memory: Option<bool>,
    pub include_vcs: Option<bool>,
    /// Explicit caller consent for code-reranker learning telemetry. Defaults
    /// to false when omitted and never changes the served response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_rerank_learning_opt_in: Option<bool>,
    pub output_format: Option<String>,
    pub context_lines: Option<i64>,
    pub content_max_chars: Option<i64>,
    pub exact_match_boost: Option<f64>,
    pub offset: Option<i64>,
    /// Opaque keyset continuation returned by a prior refactor search. Pass
    /// back unchanged with the same query and scope.
    pub cursor: Option<String>,
    /// Pre-computed query embedding for `mode="vector"` calls. The
    /// remote binary does not yet run an embedder locally; callers
    /// supply the vector. Length must match the Atlas Vector index
    /// (`1024` for Voyage `voyage-3`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query_vector: Option<Vec<f32>>,
}

fn normalize_search_tokenizer_hint(value: Option<&str>) -> Result<Option<String>> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    if value.chars().count() > 64 {
        return Err(Error::Validation(
            "tokenizer must be at most 64 characters".to_string(),
        ));
    }
    Ok(Some(value.to_ascii_lowercase()))
}

fn resolve_search_tokenizer(explicit: Option<&str>, cached_model: Option<&str>) -> Option<String> {
    explicit.map(str::to_string).or_else(|| {
        cached_model
            .and_then(mcp_model_registry::tokenizer_encoding)
            .map(str::to_string)
    })
}

#[derive(Debug, Serialize)]
struct GuidedSearchApiRequest<'a> {
    query: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    intent: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    installation_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    checkout_locator: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    grounding_handle: Option<&'a str>,
    /// Explicit caller consent for reranker-learning telemetry. This is a
    /// side-effect flag and deliberately does not participate in response-cache
    /// identity because it cannot change the served result.
    #[serde(skip_serializing_if = "Option::is_none")]
    code_rerank_learning_opt_in: Option<bool>,
    /// Caller-minted correlation for authenticated, idempotent outcomes. It is
    /// never part of response-cache identity and is present only with consent.
    #[serde(skip_serializing_if = "Option::is_none")]
    code_rerank_learning_request_id: Option<Uuid>,
    limit: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct GuidedSearchTarget {
    path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lines: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    symbol: Option<String>,
    #[serde(default)]
    why: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct GuidedSearchGuidance {
    #[serde(default)]
    answer: String,
    #[serde(default)]
    targets: Vec<GuidedSearchTarget>,
    #[serde(default)]
    confidence: f32,
    #[serde(default)]
    followup_queries: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct GuidedSearchRawResult {
    #[serde(default)]
    file_path: String,
    #[serde(default)]
    start_line: i64,
    #[serde(default)]
    end_line: i64,
    #[serde(default)]
    language: String,
    #[serde(default)]
    snippet: String,
    #[serde(default)]
    source_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct GuidedSearchApiResponse {
    #[serde(default)]
    query: String,
    #[serde(default)]
    workspace_id: Option<Uuid>,
    #[serde(default)]
    project_id: Option<Uuid>,
    #[serde(default)]
    checkout_scope: Option<CheckoutScopeStatus>,
    #[serde(default)]
    guidance: Option<GuidedSearchGuidance>,
    #[serde(default)]
    degraded: bool,
    /// Stable, bounded API outcome code (for example `navigator_timeout` or
    /// `validation_rejected`). Older API deployments omit it.
    #[serde(default)]
    degradation_reason: Option<String>,
    /// Parse/validation failures may be repaired deterministically from the
    /// verified raw evidence. Preserve that bounded operator signal while the
    /// final response remains healthy and immediately actionable.
    #[serde(default)]
    guidance_recovery_reason: Option<String>,
    #[serde(default)]
    guidance_latency_ms: i64,
    #[serde(default)]
    retrieval_latency_ms: i64,
    #[serde(default)]
    navigator_latency_ms: i64,
    /// True API handler-entry-to-response-ready latency. Newer API
    /// deployments provide this in addition to the backward-compatible
    /// retrieval-plus-Navigator `guidance_latency_ms` field.
    #[serde(default)]
    total_latency_ms: Option<i64>,
    #[serde(default)]
    code_evidence_count: Option<usize>,
    #[serde(default)]
    memory_evidence_count: Option<usize>,
    #[serde(default)]
    grounding_handle: Option<String>,
    #[serde(default)]
    grounding_base_reused: bool,
    #[serde(default)]
    results: Vec<GuidedSearchRawResult>,
    #[serde(default)]
    knowledge: Vec<String>,
}

fn guided_search_limit(input: &SearchInput) -> usize {
    input
        .limit
        .unwrap_or(GUIDED_SEARCH_DEFAULT_LIMIT as i64)
        .clamp(1, GUIDED_SEARCH_MAX_LIMIT as i64) as usize
}

fn guided_degradation_message(reason: Option<&str>) -> &'static str {
    match reason {
        Some("zero_evidence") => {
            "No grounded evidence was available for Navigator synthesis; use the raw fallback evidence below."
        }
        Some("navigator_timeout") | Some("end_to_end_timeout") => {
            "Navigator synthesis exceeded its bounded latency budget; the raw evidence remains usable."
        }
        Some("provider_error") => {
            "Navigator synthesis encountered a model-provider error; the raw evidence remains usable."
        }
        Some("parse_error") => {
            "Navigator returned an unusable structured response; the raw evidence remains usable."
        }
        Some("validation_rejected") => {
            "Navigator guidance did not satisfy the evidence contract; the raw evidence remains usable."
        }
        Some("code_retrieval_timeout") => {
            "Code retrieval exceeded its bounded latency budget; any raw evidence returned remains usable."
        }
        Some("code_retrieval_error") => {
            "Code retrieval encountered a bounded backend error; any raw evidence returned remains usable."
        }
        Some("transport_fallback") => {
            "Guided Search transport was unavailable; bounded hybrid raw evidence was served instead."
        }
        Some("api_unspecified") | Some("api_unknown") => {
            "Guided Search returned a degraded response without a recognized machine reason; the raw evidence remains usable."
        }
        _ => "Navigator synthesis was unavailable; the raw evidence remains usable.",
    }
}

/// Normalize API-provided degradation codes before they reach structured
/// output or metric labels. Mixed-version deployments may omit the field;
/// unknown/human prose must never become an unbounded protocol value.
fn stable_guided_degradation_reason(reason: Option<&str>) -> &'static str {
    match reason.map(str::trim) {
        Some("zero_evidence") => "zero_evidence",
        Some("navigator_timeout") => "navigator_timeout",
        Some("end_to_end_timeout") => "end_to_end_timeout",
        Some("provider_error") => "provider_error",
        Some("parse_error") => "parse_error",
        Some("validation_rejected") => "validation_rejected",
        Some("code_retrieval_timeout") => "code_retrieval_timeout",
        Some("code_retrieval_error") => "code_retrieval_error",
        Some("transport_fallback") => "transport_fallback",
        Some("") | None => "api_unspecified",
        Some(_) => "api_unknown",
    }
}

fn guided_degradation_metric_reason(reason: Option<&str>) -> &'static str {
    match reason {
        Some("zero_evidence") => "zero_evidence",
        Some("navigator_timeout") => "navigator_timeout",
        Some("end_to_end_timeout") => "end_to_end_timeout",
        Some("provider_error") => "provider_error",
        Some("parse_error") => "parse_error",
        Some("validation_rejected") => "validation_rejected",
        Some("code_retrieval_timeout") => "code_retrieval_timeout",
        Some("code_retrieval_error") => "code_retrieval_error",
        Some("transport_fallback") => "transport_fallback",
        Some("api_unspecified") => "api_unspecified",
        Some("api_unknown") => "api_unknown",
        Some(_) => "api_unknown",
        None => "api_unspecified",
    }
}

fn stable_guided_recovery_reason(reason: Option<&str>) -> Option<&'static str> {
    match reason.map(str::trim) {
        Some("parse_error") => Some("parse_error"),
        Some("validation_rejected") => Some("validation_rejected"),
        Some("provider_error") => Some("provider_error"),
        Some("navigator_timeout") => Some("navigator_timeout"),
        Some("") | None => None,
        Some(_) => Some("api_unknown"),
    }
}

fn guided_primary_budget(total: Duration) -> Duration {
    let proportional_reserve = total / 5;
    let reserve = proportional_reserve.min(GUIDED_SEARCH_FALLBACK_RESERVE_MAX);
    total.saturating_sub(reserve)
}

fn guided_finalization_reserve(total: Duration) -> Duration {
    (total / 20).min(GUIDED_SEARCH_FINALIZATION_RESERVE_MAX)
}

/// One absolute wall-clock owned by the public `search` handler. The primary
/// Guided request may consume at most 80% (capped so hybrid retains <=1s), and
/// hybrid must stop before the small finalization reserve. No phase is allowed
/// to reset either deadline after handler prework has already elapsed.
#[derive(Debug, Clone, Copy)]
struct GuidedExecutionBudget {
    started: Instant,
    primary_deadline: Instant,
    fallback_deadline: Instant,
    deadline: Instant,
    total: Duration,
    #[cfg(test)]
    prework_delay: Duration,
    #[cfg(test)]
    finalization_delay: Duration,
}

impl GuidedExecutionBudget {
    fn new(started: Instant, total: Duration) -> Self {
        let primary_deadline = started + guided_primary_budget(total);
        let deadline = started + total;
        let fallback_deadline = deadline
            .checked_sub(guided_finalization_reserve(total))
            .unwrap_or(primary_deadline)
            .max(primary_deadline);
        Self {
            started,
            primary_deadline,
            fallback_deadline,
            deadline,
            total,
            #[cfg(test)]
            prework_delay: Duration::ZERO,
            #[cfg(test)]
            finalization_delay: Duration::ZERO,
        }
    }

    #[cfg(test)]
    fn with_test_delays(mut self, prework: Duration, finalization: Duration) -> Self {
        self.prework_delay = prework;
        self.finalization_delay = finalization;
        self
    }

    fn primary_remaining(self) -> Duration {
        self.primary_deadline
            .saturating_duration_since(Instant::now())
    }

    fn fallback_remaining(self) -> Duration {
        self.fallback_deadline
            .saturating_duration_since(Instant::now())
    }

    fn remaining(self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    fn elapsed_ms(self) -> i64 {
        self.started.elapsed().as_millis().min(i64::MAX as u128) as i64
    }
}

fn guided_output_format(input: &SearchInput) -> &str {
    input.output_format.as_deref().unwrap_or_else(|| {
        if input.include_content == Some(false) {
            "minimal"
        } else {
            "full"
        }
    })
}

fn guided_result_location(result: &GuidedSearchRawResult) -> String {
    match (result.start_line, result.end_line) {
        (start, end) if start > 0 && end > start => {
            format!("{}:{}-{}", result.file_path, start, end)
        }
        (start, _) if start > 0 => format!("{}:{}", result.file_path, start),
        _ => result.file_path.clone(),
    }
}

fn format_guided_evidence_line(
    index: usize,
    result: &GuidedSearchRawResult,
    show_content: bool,
) -> String {
    let language = if result.language.trim().is_empty() {
        String::new()
    } else {
        format!(" [{}]", result.language.trim())
    };
    let mut line = format!(
        "{}. {}{}\n",
        index,
        guided_result_location(result),
        language
    );
    if show_content {
        let preview: String = result.snippet.trim().chars().take(300).collect();
        if !preview.is_empty() {
            let preview = preview.lines().take(4).collect::<Vec<_>>().join("\n");
            line.push_str("   ");
            line.push_str(&preview.replace('\n', "\n   "));
            line.push('\n');
        }
    }
    line
}

/// Preserve the ordinary guided response shape when it is already small. For
/// oversized model prose/snippets, build a bounded equivalent directly instead
/// of first cloning multi-megabyte strings into a second JSON tree only to shed
/// them immediately afterward.
fn guided_search_structured_value(response: &GuidedSearchApiResponse) -> Value {
    let structured_budget = search_structured_output_budget();
    if serialized_size_up_to(response, structured_budget).bytes <= structured_budget {
        return serde_json::to_value(response).unwrap_or_else(|_| serde_json::json!({}));
    }

    let guidance = response.guidance.as_ref().map(|guidance| {
        let targets = guidance
            .targets
            .iter()
            .take(4)
            .map(|target| {
                serde_json::json!({
                    "path": truncate_json_string(&target.path, 512),
                    "lines": target.lines.as_deref().map(|value| truncate_json_string(value, 128)),
                    "symbol": target.symbol.as_deref().map(|value| truncate_json_string(value, 128)),
                    "why": truncate_json_string(&target.why, 512),
                })
            })
            .collect::<Vec<_>>();
        serde_json::json!({
            "answer": truncate_json_string(&guidance.answer, 4_000),
            "targets": targets,
            "confidence": guidance.confidence,
            "followup_queries": guidance
                .followup_queries
                .iter()
                .take(2)
                .map(|query| truncate_json_string(query, 512))
                .collect::<Vec<_>>(),
        })
    });
    let results = response
        .results
        .iter()
        .take(GUIDED_SEARCH_MAX_LIMIT)
        .map(|result| {
            serde_json::json!({
                "file_path": truncate_json_string(&result.file_path, 512),
                "start_line": result.start_line,
                "end_line": result.end_line,
                "language": truncate_json_string(&result.language, 64),
                "snippet": truncate_json_string(&result.snippet, 700),
                "source_type": truncate_json_string(&result.source_type, 64),
            })
        })
        .collect::<Vec<_>>();

    serde_json::json!({
        "query": truncate_json_string(&response.query, 512),
        "workspace_id": response.workspace_id,
        "project_id": response.project_id,
        "checkout_scope": response.checkout_scope,
        "guidance": guidance,
        "degraded": response.degraded,
        "degradation_reason": response.degradation_reason,
        "guidance_recovery_reason": response.guidance_recovery_reason,
        "guidance_latency_ms": response.guidance_latency_ms,
        "retrieval_latency_ms": response.retrieval_latency_ms,
        "navigator_latency_ms": response.navigator_latency_ms,
        "total_latency_ms": response.total_latency_ms,
        "code_evidence_count": response.code_evidence_count,
        "memory_evidence_count": response.memory_evidence_count,
        "grounding_handle": response
            .grounding_handle
            .as_deref()
            .map(|handle| truncate_json_string(handle, 1_024)),
        "grounding_base_reused": response.grounding_base_reused,
        "results": results,
        "knowledge": response
            .knowledge
            .iter()
            .take(10)
            .map(|item| truncate_json_string(item, 600))
            .collect::<Vec<_>>(),
        "guided_precompacted": true,
    })
}

/// Render raw, directly-actionable evidence before the optional Navigator
/// synthesis. This ordering is deliberate: agents can start reading/editing
/// even when guidance is degraded, slow, or absent.
fn render_guided_search_response(
    response: &GuidedSearchApiResponse,
    intent: Option<&str>,
    output_format: Option<&str>,
    degradation_message: Option<&str>,
) -> (String, Value) {
    let degradation_message = degradation_message.or_else(|| {
        response
            .degraded
            .then(|| guided_degradation_message(response.degradation_reason.as_deref()))
    });
    let format = output_format.unwrap_or("full").to_ascii_lowercase();
    let rendered_query = truncate_json_string(response.query.trim(), 512);
    let mut text = format!(
        "[GUIDED_EVIDENCE] {} raw result(s) for `{}`.\n",
        response.results.len(),
        rendered_query
    );

    if format != "count" {
        let show_content = format == "full";
        let output_budget = search_text_output_budget();
        let mut rendered = 0usize;
        let mut seen_paths = HashSet::new();
        for result in &response.results {
            let line = if format == "paths" {
                let location = guided_result_location(result);
                if !seen_paths.insert(location.clone()) {
                    continue;
                }
                format!("{}\n", location)
            } else {
                format_guided_evidence_line(rendered + 1, result, show_content)
            };
            if text.len() + line.len() > output_budget {
                break;
            }
            text.push_str(&line);
            rendered += 1;
        }
        if rendered < response.results.len() {
            text.push_str(&format!(
                "… +{} raw result(s) omitted to stay within the output budget.\n",
                response.results.len() - rendered
            ));
        }

        if response.degraded || degradation_message.is_some() {
            text.push_str("\n[GUIDED_DEGRADED] ");
            text.push_str(degradation_message.unwrap_or(
                "Navigator synthesis was unavailable; the raw evidence above remains usable.",
            ));
            text.push('\n');
        } else if let Some(guidance) = response.guidance.as_ref() {
            text.push_str(&format!(
                "\n[GUIDANCE] confidence={}%.\n",
                (guidance.confidence.clamp(0.0, 1.0) * 100.0).round() as i64
            ));
            if !guidance.answer.trim().is_empty() {
                text.push_str(&truncate_json_string(guidance.answer.trim(), 4_000));
                text.push('\n');
            }
            for target in guidance.targets.iter().take(4) {
                let lines = target
                    .lines
                    .as_deref()
                    .filter(|lines| !lines.trim().is_empty())
                    .map(|lines| format!(":{}", truncate_json_string(lines.trim(), 128)))
                    .unwrap_or_default();
                let symbol = target
                    .symbol
                    .as_deref()
                    .filter(|symbol| !symbol.trim().is_empty())
                    .map(|symbol| format!(" `{}`", truncate_json_string(symbol.trim(), 128)))
                    .unwrap_or_default();
                let why = if target.why.trim().is_empty() {
                    String::new()
                } else {
                    format!(" — {}", truncate_json_string(target.why.trim(), 512))
                };
                text.push_str(&format!(
                    "- `{}{}`{}{}\n",
                    truncate_json_string(&target.path, 512),
                    lines,
                    symbol,
                    why
                ));
            }
            if !guidance.followup_queries.is_empty() {
                text.push_str(&format!(
                    "Follow-up if needed: {}\n",
                    guidance
                        .followup_queries
                        .iter()
                        .take(2)
                        .map(|query| format!("`{}`", truncate_json_string(query, 512)))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
    }

    let mut structured = guided_search_structured_value(response);
    if let Some(object) = structured.as_object_mut() {
        object.insert("mode".to_string(), Value::String("guided".to_string()));
        object.insert("raw_evidence_first".to_string(), Value::Bool(true));
        object.insert(
            "result_count".to_string(),
            serde_json::json!(response.results.len()),
        );
        match format.as_str() {
            "count" => {
                object.insert("results".to_string(), Value::Array(Vec::new()));
                object.remove("guidance");
                object.remove("knowledge");
            }
            "paths" => {
                let mut seen = HashSet::new();
                let paths = response
                    .results
                    .iter()
                    .filter_map(|result| {
                        let path = truncate_json_string(&guided_result_location(result), 512);
                        seen.insert(path.clone())
                            .then(|| serde_json::json!({ "path": path }))
                    })
                    .collect();
                object.insert("results".to_string(), Value::Array(paths));
                object.remove("guidance");
                object.remove("knowledge");
            }
            "minimal" => {
                let results = response
                    .results
                    .iter()
                    .map(|result| {
                        serde_json::json!({
                            "file_path": truncate_json_string(&result.file_path, 512),
                            "start_line": result.start_line,
                            "end_line": result.end_line,
                            "language": truncate_json_string(&result.language, 64),
                            "source_type": truncate_json_string(&result.source_type, 64),
                        })
                    })
                    .collect();
                object.insert("results".to_string(), Value::Array(results));
            }
            _ => {}
        }
        if let Some(intent) = intent {
            object.insert("intent".to_string(), Value::String(intent.to_string()));
        }
        if let Some(message) = degradation_message {
            object.insert(
                "degradation_message".to_string(),
                Value::String(message.to_string()),
            );
            if response.degraded {
                object.insert(
                    "degradation_reason".to_string(),
                    Value::String(
                        stable_guided_degradation_reason(response.degradation_reason.as_deref())
                            .to_string(),
                    ),
                );
            }
        }
    }
    (text, structured)
}

/// Return the caller-owned learning correlation as one compact structured
/// scalar. Keeping this flat both minimizes tool tokens and lets the hard wire
/// envelope preserve it when larger optional diagnostics are shed.
fn attach_code_rerank_learning_request_id(structured: &mut Value, request_id: Option<Uuid>) {
    let Some(request_id) = request_id else {
        return;
    };
    if let Some(object) = structured.as_object_mut() {
        object.insert(
            "code_rerank_learning_request_id".to_string(),
            Value::String(request_id.to_string()),
        );
    }
}

fn guided_response_from_search_response(
    query: &str,
    workspace_id: Option<Uuid>,
    project_id: Option<Uuid>,
    response: SearchResponse,
) -> GuidedSearchApiResponse {
    let retrieval_latency_ms = response.query_time_ms.unwrap_or_default();
    let checkout_scope = response.checkout_scope.clone();
    let results: Vec<GuidedSearchRawResult> = response
        .results
        .into_iter()
        .filter_map(|result| {
            let metadata = result.metadata.as_ref();
            let file_path = result
                .file_path
                .or(result.location)
                .or(result.breadcrumb)
                .filter(|path| !path.trim().is_empty())?;
            let start_line = result
                .start_line
                .or_else(|| {
                    metadata
                        .and_then(|value| value.get("start_line"))
                        .and_then(Value::as_i64)
                })
                .unwrap_or_default();
            let end_line = metadata
                .and_then(|value| value.get("end_line"))
                .and_then(Value::as_i64)
                .unwrap_or(start_line);
            let snippet = result
                .content
                .or_else(|| {
                    metadata
                        .and_then(|value| value.get("snippet"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_default();
            Some(GuidedSearchRawResult {
                file_path,
                start_line,
                end_line,
                language: result.language.unwrap_or_default(),
                snippet,
                source_type: result
                    .origin
                    .unwrap_or_else(|| "hybrid_fallback".to_string()),
            })
        })
        .collect();

    GuidedSearchApiResponse {
        query: query.to_string(),
        workspace_id,
        project_id,
        checkout_scope,
        guidance: None,
        degraded: true,
        degradation_reason: Some("transport_fallback".to_string()),
        guidance_recovery_reason: None,
        guidance_latency_ms: retrieval_latency_ms,
        retrieval_latency_ms,
        navigator_latency_ms: 0,
        total_latency_ms: Some(retrieval_latency_ms),
        code_evidence_count: Some(results.len()),
        memory_evidence_count: Some(0),
        grounding_handle: None,
        grounding_base_reused: false,
        results,
        knowledge: Vec::new(),
    }
}

fn guided_deadline_exhausted_result(
    input: &SearchInput,
    workspace_id: Option<Uuid>,
    project_id: Option<Uuid>,
    budget: GuidedExecutionBudget,
) -> ToolResult {
    let response = GuidedSearchApiResponse {
        query: input.query.clone(),
        workspace_id,
        project_id,
        checkout_scope: None,
        guidance: None,
        degraded: true,
        degradation_reason: Some("end_to_end_timeout".to_string()),
        guidance_recovery_reason: None,
        guidance_latency_ms: 0,
        retrieval_latency_ms: 0,
        navigator_latency_ms: 0,
        total_latency_ms: Some(budget.elapsed_ms().max(budget.total.as_millis() as i64)),
        code_evidence_count: Some(0),
        memory_evidence_count: Some(0),
        grounding_handle: None,
        grounding_base_reused: false,
        results: Vec::new(),
        knowledge: Vec::new(),
    };
    let (text, structured) = render_guided_search_response(
        &response,
        input.intent.as_deref(),
        Some(guided_output_format(input)),
        Some(guided_degradation_message(Some("end_to_end_timeout"))),
    );
    bounded_search_tool_result(text, structured)
}

fn record_guided_total_latency(result: &ToolResult, budget: GuidedExecutionBudget) {
    let elapsed_ms = result
        .structured_content
        .as_ref()
        .and_then(|value| value.get("total_latency_ms"))
        .and_then(Value::as_i64)
        .unwrap_or_else(|| budget.elapsed_ms())
        .max(0);
    metrics::histogram!("mcp_guided_search_total_latency_ms").record(elapsed_ms as f64);
}

async fn finalize_guided_search_result(
    result: ToolResult,
    timeout_result: ToolResult,
    policy: SearchWireTokenizerPolicy,
    budget: GuidedExecutionBudget,
) -> ToolResult {
    #[cfg(test)]
    if !budget.finalization_delay.is_zero() {
        tokio::time::sleep(budget.finalization_delay).await;
    }

    let remaining = budget.remaining();
    if remaining.is_zero() {
        return apply_search_wire_tokenizer_with_budget(timeout_result, &policy, Some(budget));
    }

    let worker_policy = policy.clone();
    let worker = tokio::task::spawn_blocking(move || {
        apply_search_wire_tokenizer_with_budget(result, &worker_policy, Some(budget))
    });
    match tokio::time::timeout(remaining, worker).await {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            tracing::warn!(error = %error, "guided search finalization worker failed");
            apply_search_wire_tokenizer_with_budget(timeout_result, &policy, Some(budget))
        }
        Err(_) => {
            tracing::warn!(
                timeout_ms = budget.total.as_millis() as u64,
                "guided search finalization exhausted the single handler deadline"
            );
            apply_search_wire_tokenizer_with_budget(timeout_result, &policy, Some(budget))
        }
    }
}

/// Unified search tool handler.
pub struct SearchTool {
    client: ContextStreamClient,
    session: Arc<SessionManager>,
    index_keeper: Arc<super::index_keeper::IndexKeeper>,
    /// Atlas product layer for premium MongoDB Atlas-backed search
    /// modes (currently `fuzzy` via Atlas Search). No-op for the local
    /// stdio binary; populated by `register_search_tools` from the
    /// registry on the remote build.
    atlas_layer: mcp_types::atlas_layer::AtlasLayer,
    #[cfg(test)]
    guided_test_prework_delay: Duration,
    #[cfg(test)]
    guided_test_finalization_delay: Duration,
}

impl SearchTool {
    pub fn new(
        client: ContextStreamClient,
        session: Arc<SessionManager>,
        index_keeper: Arc<super::index_keeper::IndexKeeper>,
        atlas_layer: mcp_types::atlas_layer::AtlasLayer,
    ) -> Self {
        Self {
            client,
            session,
            index_keeper,
            atlas_layer,
            #[cfg(test)]
            guided_test_prework_delay: Duration::ZERO,
            #[cfg(test)]
            guided_test_finalization_delay: Duration::ZERO,
        }
    }

    #[cfg(test)]
    fn with_guided_test_delays(mut self, prework: Duration, finalization: Duration) -> Self {
        self.guided_test_prework_delay = prework;
        self.guided_test_finalization_delay = finalization;
        self
    }

    #[cfg(test)]
    async fn execute_guided_search(
        &self,
        input: &SearchInput,
        workspace_id: Option<Uuid>,
        project_id: Option<Uuid>,
        resolved_grounding_handle: Option<String>,
        code_rerank_learning_request_id: Option<Uuid>,
        checkout_scope: Option<CheckoutRoutingScope>,
        use_search_cache: bool,
        cache_key: String,
    ) -> Result<ToolResult> {
        self.execute_guided_search_with_timeout(
            input,
            workspace_id,
            project_id,
            resolved_grounding_handle,
            code_rerank_learning_request_id,
            checkout_scope,
            use_search_cache,
            cache_key,
            GUIDED_SEARCH_REQUEST_TIMEOUT,
        )
        .await
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    async fn execute_guided_search_with_timeout(
        &self,
        input: &SearchInput,
        workspace_id: Option<Uuid>,
        project_id: Option<Uuid>,
        resolved_grounding_handle: Option<String>,
        code_rerank_learning_request_id: Option<Uuid>,
        checkout_scope: Option<CheckoutRoutingScope>,
        use_search_cache: bool,
        cache_key: String,
        guided_request_timeout: Duration,
    ) -> Result<ToolResult> {
        self.execute_guided_search_with_budget(
            input,
            workspace_id,
            project_id,
            resolved_grounding_handle,
            code_rerank_learning_request_id,
            checkout_scope,
            use_search_cache,
            cache_key,
            GuidedExecutionBudget::new(Instant::now(), guided_request_timeout),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_guided_search_with_budget(
        &self,
        input: &SearchInput,
        workspace_id: Option<Uuid>,
        project_id: Option<Uuid>,
        resolved_grounding_handle: Option<String>,
        code_rerank_learning_request_id: Option<Uuid>,
        checkout_scope: Option<CheckoutRoutingScope>,
        use_search_cache: bool,
        cache_key: String,
        budget: GuidedExecutionBudget,
    ) -> Result<ToolResult> {
        let primary_timeout = budget.primary_remaining();
        let intent = input
            .intent
            .as_deref()
            .map(str::trim)
            .filter(|intent| !intent.is_empty());
        if intent.is_some_and(|intent| intent.chars().count() > 2_000) {
            return Err(Error::Validation(
                "intent must be at most 2000 characters for guided search".to_string(),
            ));
        }

        let request = GuidedSearchApiRequest {
            query: input.query.trim(),
            intent,
            workspace_id,
            project_id,
            installation_id: checkout_scope.as_ref().map(|scope| scope.installation_id),
            checkout_locator: checkout_scope
                .as_ref()
                .map(|scope| scope.checkout_locator.as_str()),
            grounding_handle: resolved_grounding_handle.as_deref(),
            code_rerank_learning_opt_in: input.code_rerank_learning_opt_in,
            code_rerank_learning_request_id,
            limit: guided_search_limit(input),
        };
        let options = RequestOptions {
            workspace_id,
            timeout: Some(primary_timeout),
            retries: Some(0),
            ..Default::default()
        };

        let guided_result = if primary_timeout.is_zero() {
            Err(Error::Timeout(1))
        } else {
            match tokio::time::timeout(
                primary_timeout,
                self.client.post_with_options::<GuidedSearchApiResponse, _>(
                    "/search/guided",
                    request,
                    options,
                ),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => Err(Error::Timeout(primary_timeout.as_secs().max(1))),
            }
        };

        match guided_result {
            Ok(mut response) => {
                if response.query.trim().is_empty() {
                    response.query = input.query.clone();
                }
                if response.degraded {
                    response.degradation_reason = Some(
                        stable_guided_degradation_reason(response.degradation_reason.as_deref())
                            .to_string(),
                    );
                }
                response.guidance_recovery_reason =
                    stable_guided_recovery_reason(response.guidance_recovery_reason.as_deref())
                        .map(str::to_string);
                let session_update_budget = budget.fallback_remaining();
                if !session_update_budget.is_zero()
                    && tokio::time::timeout(session_update_budget, async {
                        let session_state = self.session.state().await;
                        if response.grounding_handle.is_some()
                            && session_state.workspace_id == workspace_id
                            && session_state.project_id == project_id
                            && session_state.grounding_handle == resolved_grounding_handle
                        {
                            self.session
                                .set_grounding_handle(response.grounding_handle.clone())
                                .await;
                        }
                    })
                    .await
                    .is_err()
                {
                    // Session advancement is opportunistic. Never discard a
                    // successful raw-evidence response merely because this
                    // state update could not finish before finalization reserve.
                    tracing::warn!(
                        timeout_ms = budget.total.as_millis() as u64,
                        "guided session state update skipped at absolute deadline"
                    );
                }
                let degradation_reason = response
                    .degraded
                    .then(|| guided_degradation_message(response.degradation_reason.as_deref()));
                let (mut text, mut structured) = render_guided_search_response(
                    &response,
                    intent,
                    Some(guided_output_format(input)),
                    degradation_reason,
                );
                let checkout_scope_unconfirmed = checkout_scope.as_ref().is_some_and(|expected| {
                    !response.checkout_scope.as_ref().is_some_and(|actual| {
                        actual.matches(expected.installation_id, &expected.checkout_locator)
                    })
                });
                if checkout_scope_unconfirmed {
                    text.push_str(
                        "\n\n[CHECKOUT_SCOPE] Guided search served canonical project evidence, but the hosted service did not confirm the active checkout overlay.",
                    );
                    if let Some(object) = structured.as_object_mut() {
                        object.insert("checkout_scope_unconfirmed".to_string(), Value::Bool(true));
                    }
                }
                attach_code_rerank_learning_request_id(
                    &mut structured,
                    code_rerank_learning_request_id,
                );
                let (text, structured) = budget_search_tool_payload(text, structured);
                if use_search_cache && !response.degraded {
                    put_search_cache(cache_key, (text.clone(), structured.clone()));
                }
                metrics::counter!(
                    "mcp_search_calls_total",
                    "requested_mode" => "guided",
                    "executed_mode" => "guided",
                    "outcome" => if response.degraded { "degraded" } else { "hits" },
                )
                .increment(1);
                metrics::histogram!("mcp_guided_search_retrieval_latency_ms")
                    .record(response.retrieval_latency_ms as f64);
                metrics::histogram!("mcp_guided_search_navigator_latency_ms")
                    .record(response.navigator_latency_ms as f64);
                if response.degraded {
                    metrics::counter!(
                        "mcp_guided_search_degraded_total",
                        "reason" => guided_degradation_metric_reason(
                            response.degradation_reason.as_deref()
                        ),
                    )
                    .increment(1);
                }
                if let Some(reason) = response.guidance_recovery_reason.as_deref() {
                    metrics::counter!(
                        "mcp_guided_search_recovered_total",
                        "reason" => reason.to_string(),
                    )
                    .increment(1);
                }
                let mut result = bounded_existing_search_tool_result(ToolResult::with_structured(
                    text, structured,
                ));
                stamp_guided_total_latency(&mut result, budget);
                Ok(result)
            }
            Err(guided_error) => {
                // Older API deployments, transport failures, and a total
                // guided-request timeout must not strand the agent without
                // evidence. Fall back to the normal hybrid retrieval lane and
                // render it in the same raw-first contract.
                tracing::warn!(
                    error = %guided_error,
                    timeout_ms = budget.total.as_millis() as u64,
                    "guided search unavailable; falling back to hybrid evidence"
                );
                let fallback_params = SearchParams {
                    query: input.query.clone(),
                    cursor: None,
                    workspace_id,
                    project_id,
                    installation_id: checkout_scope.as_ref().map(|scope| scope.installation_id),
                    checkout_locator: checkout_scope
                        .as_ref()
                        .map(|scope| scope.checkout_locator.clone()),
                    limit: Some(guided_search_limit(input) as i64),
                    file_types: input.file_types.clone(),
                    include_content: Some(true),
                    output_format: Some("full".to_string()),
                    context_lines: input.context_lines,
                    content_max_chars: Some(
                        input.content_max_chars.unwrap_or(700).clamp(50, 10_000),
                    ),
                    exact_match_boost: input.exact_match_boost,
                    offset: input.offset,
                    include_memory: Some(false),
                    code_rerank_learning_opt_in: input.code_rerank_learning_opt_in,
                    code_rerank_learning_request_id,
                    hot_paths_hint: None,
                    session_id: mcp_client::get_task_mcp_session_id(),
                };
                // One absolute MCP budget owns both attempts. Guided consumes
                // at most its primary share; hybrid receives only the wall
                // time still remaining and can never start a fresh 12s clock.
                let remaining = budget.fallback_remaining();
                let fallback_result = if remaining.is_zero() {
                    None
                } else {
                    Some(
                        tokio::time::timeout(
                            remaining,
                            execute_api_search_attempt(
                                &self.client,
                                SearchMode::Hybrid,
                                fallback_params,
                            ),
                        )
                        .await,
                    )
                };
                let (response, learning_request_id, fallback_outcome, fallback_message) =
                    match fallback_result {
                        Some(Ok(Ok(attempt))) => (
                            guided_response_from_search_response(
                                &input.query,
                                workspace_id,
                                project_id,
                                attempt.response,
                            ),
                            attempt.learning_request_id,
                            "hybrid_evidence",
                            "Guided Search was unavailable; served hybrid raw evidence instead.",
                        ),
                        Some(Ok(Err(_))) => (
                            guided_response_from_search_response(
                                &input.query,
                                workspace_id,
                                project_id,
                                SearchResponse::default(),
                            ),
                            code_rerank_learning_request_id,
                            "hybrid_error",
                            "Guided Search and its bounded hybrid fallback were unavailable; retry the same guided search.",
                        ),
                        Some(Err(_)) => (
                            guided_response_from_search_response(
                                &input.query,
                                workspace_id,
                                project_id,
                                SearchResponse::default(),
                            ),
                            code_rerank_learning_request_id,
                            "hybrid_timeout",
                            "Guided Search and its bounded hybrid fallback exhausted the single MCP deadline; retry the same guided search.",
                        ),
                        None => (
                            guided_response_from_search_response(
                                &input.query,
                                workspace_id,
                                project_id,
                                SearchResponse::default(),
                            ),
                            code_rerank_learning_request_id,
                            "deadline_exhausted",
                            "Guided Search exhausted the single MCP deadline before hybrid fallback could run; retry the same guided search.",
                        ),
                    };
                let (text, mut structured) = render_guided_search_response(
                    &response,
                    intent,
                    Some(guided_output_format(input)),
                    Some(fallback_message),
                );
                if let Some(object) = structured.as_object_mut() {
                    object.insert("fallback_used".to_string(), Value::Bool(true));
                    object.insert(
                        "fallback_reason".to_string(),
                        Value::String(fallback_outcome.to_string()),
                    );
                }
                attach_code_rerank_learning_request_id(&mut structured, learning_request_id);
                metrics::counter!(
                    "mcp_search_calls_total",
                    "requested_mode" => "guided",
                    "executed_mode" => "hybrid",
                    "outcome" => "degraded",
                )
                .increment(1);
                metrics::counter!(
                    "mcp_guided_search_degraded_total",
                    "reason" => "transport_fallback",
                )
                .increment(1);
                metrics::counter!(
                    "mcp_guided_search_transport_fallback_total",
                    "outcome" => fallback_outcome,
                )
                .increment(1);
                metrics::histogram!("mcp_guided_search_retrieval_latency_ms")
                    .record(response.retrieval_latency_ms.max(0) as f64);
                metrics::histogram!("mcp_guided_search_navigator_latency_ms").record(0.0);
                let mut result = bounded_search_tool_result(text, structured);
                stamp_guided_total_latency(&mut result, budget);
                Ok(result)
            }
        }
    }

    async fn atlas_plan_nudge(
        &self,
        product: mcp_types::atlas_layer::AtlasProductId,
        feature: &str,
    ) -> Option<ToolResult> {
        let state = self.session.state().await;
        let gate = gate_decision(product, state.atlas_remote_capabilities.as_ref());
        match gate {
            AtlasProductGate::DeniedByTier { tier_required } => {
                let current_plan = state
                    .account_context
                    .as_ref()
                    .and_then(|ctx| ctx.effective_plan.as_deref());
                Some(bounded_existing_search_tool_result(
                    ToolResult::plan_restricted(
                        feature,
                        current_plan,
                        tier_required.as_deref().unwrap_or("pro"),
                        true,
                    ),
                ))
            }
            AtlasProductGate::Allowed
            | AtlasProductGate::DeniedByEnvFlag
            | AtlasProductGate::NoHandshake => None,
        }
    }

    /// Atlas Vector Search with metadata-filter pushdown.
    /// Short-circuit handler for `mode = "vector"`.
    ///
    /// # Scope guidance
    ///
    /// `mode = "vector"` here is a **gap-coverage** route, not the
    /// primary semantic path. Atlas Vector Search is appropriate for:
    /// - archived data (the compatibility provider's Online Archive — Qdrant doesn't
    ///   carry cold transcripts), and
    /// - MongoDB-resident docs the server's Qdrant pipeline doesn't
    ///   index, and
    /// - vector queries that need MongoDB-side `$match` joins with
    ///   non-Qdrant metadata.
    ///
    /// For primary semantic search over code or `memory_events` (the
    /// vast majority of callers), **route to `mode = "semantic"`** —
    /// that hits ContextStream's existing Voyage Large 4 + Qdrant
    /// pipeline which is faster and higher recall than rebuilding the
    /// same retrieval atop Atlas. The handler below tells the caller
    /// this when the layer isn't available; we keep the callable
    /// surface here for the gap cases.
    async fn execute_atlas_vector(
        &self,
        query: &str,
        query_vector: Option<&[f32]>,
        workspace_id: Option<Uuid>,
        project_id: Option<Uuid>,
        limit: usize,
    ) -> Result<ToolResult> {
        if let Some(nudge) = self
            .atlas_plan_nudge(
                mcp_types::atlas_layer::AtlasProductId::Vector,
                "Filtered vector search",
            )
            .await
        {
            return Ok(nudge);
        }

        use mcp_types::atlas_layer::{AtlasVectorHit, AtlasVectorScope};

        let provider = match self.atlas_layer.vector() {
            Some(p) => p,
            None => {
                let note = if self.atlas_layer.is_enabled() {
                    "[VECTOR] filtered vector search unavailable for this deployment. \
                     For primary semantic search use mode=\"semantic\" (ContextStream's \
                     canonical semantic pipeline). Filtered vector search is gap-coverage \
                     only (archived transcripts and warm-tier-only data)."
                } else {
                    "[VECTOR] this deployment does not include filtered vector search; \
                     only available on hosted/remote deployments. For primary semantic \
                     search use mode=\"semantic\" — the canonical semantic pipeline \
                     works on every binary."
                };
                return Ok(bounded_search_tool_result(
                    note,
                    serde_json::json!({
                        "stages_used": ["vector_filtered"],
                        "available": false,
                        "results": [],
                        "primary_path": "mode=semantic",
                    }),
                ));
            }
        };

        let workspace_id = match workspace_id {
            Some(id) => id,
            None => {
                return Err(Error::Validation(
                    "vector search requires a resolved workspace_id".to_string(),
                ));
            }
        };

        // Parse `branch:`, `lang:`, `path:`, `recent:Nd`, `project:<uuid>`
        // tokens out of the query and strip them so the remaining text
        // is the intent for future server-side embedding.
        let (residual_query, parsed_filter, parsed_project_id) = parse_vector_filters(query);
        let mut scope = AtlasVectorScope::new(workspace_id);
        scope.filter = parsed_filter;
        if let Some(pid) = project_id.or(parsed_project_id) {
            scope.project_id = Some(pid);
        }

        let query_vector = match query_vector {
            Some(v) if !v.is_empty() => v.to_vec(),
            _ => {
                return Ok(bounded_search_tool_result(
                    format!(
                        "[VECTOR] query_vector is required for this call \
                         (text → embedding integration is pending a follow-on task). \
                         Parsed filters: {} Residual intent: `{}`",
                        summarize_filter(&scope),
                        residual_query.trim(),
                    ),
                    serde_json::json!({
                        "stages_used": ["vector_filtered"],
                        "available": true,
                        "requires": "query_vector",
                        "parsed_filter": &scope.filter,
                        "residual_query": residual_query.trim(),
                        "results": [],
                    }),
                ));
            }
        };

        let started = std::time::Instant::now();
        let hits: Vec<AtlasVectorHit> =
            match provider.vector_search(&query_vector, &scope, limit).await {
                Ok(h) => h,
                Err(e) => {
                    tracing::warn!(error = %e, "atlas-vector: provider call failed");
                    return Ok(bounded_search_tool_result(
                        format!("[VECTOR] error: {}", e),
                        serde_json::json!({
                            "stages_used": ["vector_filtered"],
                            "error": e.to_string(),
                            "results": [],
                        }),
                    ));
                }
            };
        let elapsed_ms = started.elapsed().as_millis() as u64;

        let count = hits.len();
        let results: Vec<mcp_types::api::SearchResult> = hits
            .iter()
            .map(|h| mcp_types::api::SearchResult {
                id: h.id.clone(),
                title: h.title.clone(),
                content: None,
                score: Some(h.score),
                location: h.url.clone(),
                metadata: Some(serde_json::json!({
                    "collection": h.collection.as_str(),
                    "snippet": h.snippet,
                    "filter_metadata": h.metadata,
                })),
                origin: Some("vector_filtered".to_string()),
                ..Default::default()
            })
            .collect();

        let header = if count == 0 {
            format!(
                "[VECTOR] 0 hits for `{}` with filter {} ({}ms; index may be empty pending warm-tier embeddings)",
                residual_query.trim(),
                summarize_filter(&scope),
                elapsed_ms
            )
        } else {
            let lines: Vec<String> = hits
                .iter()
                .take(10)
                .enumerate()
                .map(|(i, h)| {
                    format!(
                        "  {}. [{}] {} (score {:.3})",
                        i + 1,
                        h.collection.as_str(),
                        h.title.as_deref().unwrap_or("(untitled)"),
                        h.score
                    )
                })
                .collect();
            format!(
                "[VECTOR] {} hit(s) for `{}` with filter {} ({}ms)\n{}",
                count,
                residual_query.trim(),
                summarize_filter(&scope),
                elapsed_ms,
                lines.join("\n")
            )
        };

        let structured = serde_json::json!({
            "stages_used": ["vector_filtered"],
            "available": true,
            "query": query,
            "residual_query": residual_query.trim(),
            "parsed_filter": &scope.filter,
            "elapsed_ms": elapsed_ms,
            "result_count": count,
            "results": results,
        });
        Ok(bounded_search_tool_result(header, structured))
    }

    /// Atlas Search (Lucene) fuzzy/typo-tolerant text search.
    /// Short-circuit handler for `mode = "fuzzy"`.
    async fn execute_atlas_fuzzy(
        &self,
        query: &str,
        workspace_id: Option<Uuid>,
        project_id: Option<Uuid>,
        limit: usize,
    ) -> Result<ToolResult> {
        if let Some(nudge) = self
            .atlas_plan_nudge(
                mcp_types::atlas_layer::AtlasProductId::Search,
                "Fuzzy search",
            )
            .await
        {
            return Ok(nudge);
        }

        use mcp_types::atlas_layer::{AtlasSearchHit, AtlasSearchScope};

        let provider = match self.atlas_layer.search() {
            Some(p) => p,
            None => {
                let note = if self.atlas_layer.is_enabled() {
                    "[FUZZY] disabled (fuzzy search provider not available for this deployment)"
                } else {
                    "[FUZZY] disabled (this deployment does not include fuzzy search; \
                     only available on hosted/remote deployments). Falling back to \
                     keyword search is recommended via mode=\"keyword\"."
                };
                tracing::debug!(
                    enabled = self.atlas_layer.is_enabled(),
                    "atlas-fuzzy: provider unavailable; returning empty result with note"
                );
                return Ok(bounded_search_tool_result(
                    note,
                    serde_json::json!({
                        "stages_used": ["fuzzy_search"],
                        "available": false,
                        "results": [],
                    }),
                ));
            }
        };

        let workspace_id = match workspace_id {
            Some(id) => id,
            None => {
                return Err(Error::Validation(
                    "fuzzy search requires a resolved workspace_id".to_string(),
                ));
            }
        };

        let mut scope = AtlasSearchScope::new(workspace_id);
        if let Some(pid) = project_id {
            scope = scope.with_project(pid);
        }

        let started = std::time::Instant::now();
        let hits: Vec<AtlasSearchHit> = match provider.fuzzy_text_search(query, &scope, limit).await
        {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(error = %e, "atlas-fuzzy: provider call failed");
                return Ok(bounded_search_tool_result(
                    format!("[FUZZY] error: {}", e),
                    serde_json::json!({
                        "stages_used": ["fuzzy_search"],
                        "error": e.to_string(),
                        "results": [],
                    }),
                ));
            }
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;

        let count = hits.len();
        let results: Vec<mcp_types::api::SearchResult> = hits
            .iter()
            .map(|h| mcp_types::api::SearchResult {
                id: h.id.clone(),
                title: h.title.clone(),
                content: h.content.clone(),
                score: Some(h.score),
                location: h.url.clone(),
                metadata: Some(serde_json::json!({
                    "collection": h.collection.as_str(),
                    "snippet": h.snippet,
                })),
                origin: Some("fuzzy_search".to_string()),
                ..Default::default()
            })
            .collect();

        let header = if count == 0 {
            format!(
                "[FUZZY] 0 fuzzy hits for `{}` ({}ms; index empty or no matches)",
                query, elapsed_ms
            )
        } else {
            let lines: Vec<String> = hits
                .iter()
                .take(10)
                .enumerate()
                .map(|(i, h)| {
                    format!(
                        "  {}. [{}] {} (score {:.3})",
                        i + 1,
                        h.collection.as_str(),
                        h.title.as_deref().unwrap_or("(untitled)"),
                        h.score
                    )
                })
                .collect();
            format!(
                "[FUZZY] {} fuzzy hit(s) for `{}` ({}ms)\n{}",
                count,
                query,
                elapsed_ms,
                lines.join("\n")
            )
        };

        let structured = serde_json::json!({
            "stages_used": ["fuzzy_search"],
            "available": true,
            "query": query,
            "elapsed_ms": elapsed_ms,
            "result_count": count,
            "results": results,
        });
        Ok(bounded_search_tool_result(header, structured))
    }
}

impl SearchTool {
    async fn execute_inner(
        &self,
        input: SearchInput,
        handler_started: Instant,
        guided_budget: Option<GuidedExecutionBudget>,
    ) -> Result<ToolResult> {
        // Validate the explicit protocol hint before index/network side effects.
        let explicit_tokenizer = normalize_search_tokenizer_hint(input.tokenizer.as_deref())?;

        #[cfg(test)]
        if let Some(budget) = guided_budget {
            if !budget.prework_delay.is_zero() {
                tokio::time::sleep(budget.prework_delay).await;
            }
        }

        // Fire-and-forget incremental index check (matches TypeScript behavior).
        self.index_keeper.tick();

        if input.query.trim().is_empty() {
            return Err(Error::Validation("query is required".to_string()));
        }
        if let Some(cursor) = input.cursor.as_deref() {
            if let Some(reason) = search_cursor_protocol_violation(cursor) {
                return Err(Error::Validation(format!(
                    "cursor protocol violation ({reason}): opaque cursor is {} bytes; maximum valid cursor size is {} bytes; request a fresh first page instead of truncating or modifying the cursor",
                    cursor.len(),
                    MAX_VALID_SEARCH_CURSOR_BYTES,
                )));
            }
        }

        // Fail fast when no auth is configured so callers don't wait on long HTTP timeouts.
        if !self.client.config().await.has_auth() {
            return Err(Error::MissingCredentials);
        }

        // Auto-resolve workspace/project from session if not provided
        let state = self.session.state().await;
        let search_session_id = mcp_client::get_task_mcp_session_id()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                state
                    .session_id
                    .clone()
                    .filter(|value| !value.trim().is_empty())
            });
        let cached_model = if explicit_tokenizer.is_none() {
            search_session_id
                .as_deref()
                .and_then(mcp_session::session_model_cache::lookup)
        } else {
            None
        };
        let effective_tokenizer =
            resolve_search_tokenizer(explicit_tokenizer.as_deref(), cached_model.as_deref());
        let tokenizer_caller_scope = super::atlas_warm_cache::current_caller_cache_scope();
        let cohort_workspace_id = input
            .workspace_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or_else(|| state.workspace_id.map(|id| id.to_string()));
        let cohort_project_id = input
            .project_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .or_else(|| state.project_id.map(|id| id.to_string()));
        let tokenizer_canary_key = search_tokenizer_canary_key(
            tokenizer_caller_scope.cache_identity(),
            cohort_workspace_id.as_deref(),
            cohort_project_id.as_deref(),
            search_session_id.as_deref(),
            &input,
        );
        let wire_response_context = crate::wire_tokens::current_wire_response_context();
        let tokenizer_decision = crate::wire_tokens::rollout_decision_for_context(
            effective_tokenizer.as_deref(),
            &tokenizer_canary_key,
            &wire_response_context,
        );
        wire_response_context.register_rollout_decision(tokenizer_decision);
        let tokenizer_cache_namespace =
            crate::wire_tokens::cache_namespace_for_decision(tokenizer_decision);
        let wire_tokenizer_policy = SearchWireTokenizerPolicy {
            decision: tokenizer_decision,
            context: wire_response_context,
        };
        let shared_scope = resolve_read_scope(
            &self.client,
            self.session.as_ref(),
            input.workspace_id.as_deref(),
            input.project_id.as_deref(),
        )
        .await?;

        let (requested_mode, mode_auto_selected, mode_reason) = resolve_mode(
            input.mode.as_deref(),
            &input.query,
            state.default_search_mode.as_deref(),
        );
        if input.cursor.is_some() && requested_mode != SearchMode::Refactor {
            return Err(Error::Validation(
                "cursor is only supported for mode=\"refactor\"; repeat the same refactor search and pass next_cursor back unchanged"
                    .to_string(),
            ));
        }

        // Fuzzy mode short-circuit: route directly to the atlas product
        // layer's fuzzy-search provider when available. Falls through to keyword
        // search with a degradation note when the compatibility layer is absent.
        if matches!(requested_mode, SearchMode::Fuzzy) {
            let workspace_id = input
                .workspace_id
                .as_ref()
                .and_then(|s| Uuid::parse_str(s).ok())
                .or(shared_scope.workspace_id)
                .or(state.workspace_id);
            let project_id = input
                .project_id
                .as_ref()
                .and_then(|s| Uuid::parse_str(s).ok());
            let limit = input.limit.unwrap_or(20).max(1).min(50) as usize;
            let result = self
                .execute_atlas_fuzzy(&input.query, workspace_id, project_id, limit)
                .await?;
            return Ok(apply_search_wire_tokenizer(result, &wire_tokenizer_policy));
        }

        // Vector mode short-circuit: route to the legacy vector provider with
        // metadata filters parsed out of the query.
        if matches!(requested_mode, SearchMode::Vector) {
            let workspace_id = input
                .workspace_id
                .as_ref()
                .and_then(|s| Uuid::parse_str(s).ok())
                .or(shared_scope.workspace_id)
                .or(state.workspace_id);
            let project_id = input
                .project_id
                .as_ref()
                .and_then(|s| Uuid::parse_str(s).ok());
            let limit = input.limit.unwrap_or(20).max(1).min(50) as usize;
            let result = self
                .execute_atlas_vector(
                    &input.query,
                    input.query_vector.as_deref(),
                    workspace_id,
                    project_id,
                    limit,
                )
                .await?;
            return Ok(apply_search_wire_tokenizer(result, &wire_tokenizer_policy));
        }

        let explicit_workspace_id = input
            .workspace_id
            .as_ref()
            .and_then(|s| Uuid::parse_str(s).ok());
        let mut workspace_id = explicit_workspace_id
            .or(shared_scope.workspace_id)
            .or(state.workspace_id);
        let requested_explicit_project_id = input
            .project_id
            .as_ref()
            .and_then(|s| Uuid::parse_str(s).ok());
        let mut explicit_project_id = requested_explicit_project_id;
        if shared_scope.note.is_some() {
            explicit_project_id = None;
        }
        let mut session_folder_path = normalize_search_root(state.folder_path.as_deref());
        let dirty_file_snapshot = read_dirty_file_snapshot(session_folder_path.as_deref());
        let mut local_git_worktree_dirty = dirty_file_snapshot.git_worktree_dirty;
        let mut activity_file_hints = dirty_file_snapshot.hints;
        let mut repair_file_hints = dirty_file_snapshot.repair_hints;
        let mut local_git_repository = session_folder_path
            .as_deref()
            .and_then(local_git_remote_url)
            .as_deref()
            .and_then(repo_identity_key);
        let mut local_git_branch = session_folder_path.as_deref().and_then(git_branch_name);
        let mut local_git_commit_sha = session_folder_path.as_deref().and_then(git_head_sha);
        let mut activity_paths: Vec<String> = activity_file_hints
            .iter()
            .map(|hint| hint.display_path.clone())
            .collect();

        // Folder-scoped result-cache freshness. We read the local index
        // timestamp and whether the working tree has drifted from it (any
        // tracked file newer than the indexed snapshot) up front, so the cache
        // gate below can safely serve a folder-scoped repeat query when the
        // tree is in sync — and skip the cache the moment a local edit lands.
        let mut folder_index_indexed_at = match session_folder_path.as_deref() {
            Some(p) if Path::new(p).is_dir() => {
                read_local_index_entry(p).and_then(|e| e.indexed_at)
            }
            _ => None,
        };
        let mut folder_has_drift = session_folder_path.is_some()
            && !dirty_hints_indicating_drift(&repair_file_hints, folder_index_indexed_at)
                .is_empty();

        let mut resolved_folder_project_id = None;
        if let Some(ref path) = session_folder_path {
            if let Some(mapping) = resolve_workspace(path).await {
                if workspace_id.is_none() {
                    workspace_id = Some(mapping.workspace_id);
                }
                resolved_folder_project_id = mapping.project_id;
            }
        }
        let local_index_project_id = session_folder_path
            .as_deref()
            .and_then(ContextStreamClient::indexed_project_id_for_folder);

        // `resolve_read_scope` already validates an explicit project against
        // the active workspace and records any correction in `note`. Do not
        // repeat the same ownership lookup here; the shared resolver remains
        // the single authorization/correction boundary for read tools.

        let mut local_twin_redirect: Option<LocalTwinRedirect> = None;
        let cross_machine_candidate_id = explicit_project_id
            .or(resolved_folder_project_id)
            .or(local_index_project_id)
            .or(shared_scope.project_id)
            .or(state.project_id);
        if let (Some(candidate_id), Some(active_folder)) =
            (cross_machine_candidate_id, session_folder_path.as_deref())
        {
            if let Ok(project) = self.client.get_project(candidate_id).await {
                if let Some(indexed_root) = project.path.as_deref() {
                    if !indexed_root_matches_local_folder(indexed_root, active_folder) {
                        if let Some(twin) =
                            find_local_twin(&project, workspace_id.or(project.workspace_id))
                        {
                            tracing::debug!(
                                source_project_id = %candidate_id,
                                target_project_id = %twin.project_id,
                                indexed_root = %indexed_root,
                                target_folder = %twin.folder_path,
                                "search scope redirected from cross-machine project to local twin"
                            );
                            self.index_keeper.maybe_trigger_duplicate_merge(
                                workspace_id.or(twin.workspace_id).or(project.workspace_id),
                                twin.project_id,
                                candidate_id,
                                &twin.folder_path,
                            );
                            local_twin_redirect = Some(LocalTwinRedirect {
                                source_project_id: candidate_id,
                                target_project_id: twin.project_id,
                                target_folder_path: twin.folder_path.clone(),
                                indexed_root: indexed_root.to_string(),
                            });
                            explicit_project_id = Some(twin.project_id);
                            if workspace_id.is_none() {
                                workspace_id = twin.workspace_id.or(project.workspace_id);
                            }
                            session_folder_path = Some(twin.folder_path.clone());
                            resolved_folder_project_id = Some(twin.project_id);
                            let dirty_file_snapshot =
                                read_dirty_file_snapshot(session_folder_path.as_deref());
                            local_git_worktree_dirty = dirty_file_snapshot.git_worktree_dirty;
                            activity_file_hints = dirty_file_snapshot.hints;
                            repair_file_hints = dirty_file_snapshot.repair_hints;
                            local_git_repository = session_folder_path
                                .as_deref()
                                .and_then(local_git_remote_url)
                                .as_deref()
                                .and_then(repo_identity_key);
                            local_git_branch =
                                session_folder_path.as_deref().and_then(git_branch_name);
                            local_git_commit_sha =
                                session_folder_path.as_deref().and_then(git_head_sha);
                            activity_paths = activity_file_hints
                                .iter()
                                .map(|hint| hint.display_path.clone())
                                .collect();
                            folder_index_indexed_at = match session_folder_path.as_deref() {
                                Some(p) if Path::new(p).is_dir() => {
                                    read_local_index_entry(p).and_then(|e| e.indexed_at)
                                }
                                _ => None,
                            };
                            folder_has_drift = session_folder_path.is_some()
                                && !dirty_hints_indicating_drift(
                                    &repair_file_hints,
                                    folder_index_indexed_at,
                                )
                                .is_empty();
                        }
                    }
                }
            }
        }

        let allow_fallback_project = input
            .workspace_id
            .as_deref()
            .is_none_or(|value| value.trim().is_empty())
            || explicit_workspace_id == shared_scope.workspace_id
            || explicit_workspace_id == state.workspace_id;
        let project_id = explicit_project_id.or_else(|| {
            allow_fallback_project.then(|| shared_scope.project_id.or(state.project_id))?
        });
        let active_index_repair = maybe_repair_active_index_before_search(
            &self.client,
            workspace_id,
            explicit_project_id,
            resolved_folder_project_id,
            local_index_project_id,
            project_id,
            session_folder_path.as_deref(),
            &repair_file_hints,
        )
        .await;
        if active_index_repair.complete {
            folder_index_indexed_at = match session_folder_path.as_deref() {
                Some(p) if Path::new(p).is_dir() => {
                    read_local_index_entry(p).and_then(|e| e.indexed_at)
                }
                _ => None,
            };
            folder_has_drift = false;
        }
        let checkout_routing_scope = session_folder_path
            .as_deref()
            .and_then(ContextStreamClient::checkout_routing_scope);
        let checkout_scope_requested = session_folder_path.is_some();
        let checkout_scope_unroutable =
            checkout_scope_requested && checkout_routing_scope.is_none();

        let prefers_project_scope = explicit_project_id.is_some()
            || resolved_folder_project_id.is_some()
            || local_index_project_id.is_some()
            || project_id.is_some();
        let memory_query_intent = query_has_memory_intent(&input.query);
        let code_query_intent = query_has_code_intent(&input.query);
        let memory_decision = resolve_include_memory_decision(
            requested_mode,
            input.include_memory,
            prefers_project_scope,
            &input.query,
        );
        let include_memory = Some(memory_decision.enabled);
        let config = self.client.config().await;
        let (resolved_output_format, resolved_include_content) =
            resolve_output_preferences(&input, requested_mode);
        let count_only_output = resolved_output_format
            .as_deref()
            .is_some_and(|format| format.eq_ignore_ascii_case("count"));
        let resolved_limit = if requested_mode == SearchMode::Guided {
            Some(guided_search_limit(&input) as i64)
        } else {
            resolve_search_limit(&input, config.search_limit)
        };
        let resolved_content_max_chars =
            resolve_search_content_max_chars(&input, config.search_max_chars);
        let resolved_context_lines = resolve_search_context_lines(&input);
        let resolved_exact_match_boost = resolve_exact_match_boost(&input);
        let resolved_offset = resolve_search_offset(&input);
        let resolved_include_vcs = input
            .include_vcs
            .unwrap_or_else(|| query_has_vcs_signal(&input.query));
        let hot_paths_hint = build_hot_paths_hint(&input.query, &activity_paths);
        let cache_shapers = ResolvedSearchCacheShapers {
            limit: resolved_limit,
            offset: resolved_offset,
            file_types: input.file_types.clone().unwrap_or_default(),
            include_content: resolved_include_content,
            include_memory: memory_decision.enabled,
            include_vcs: resolved_include_vcs,
            output_format: resolved_output_format.clone(),
            context_lines: resolved_context_lines,
            content_max_chars: resolved_content_max_chars,
            exact_match_boost: resolved_exact_match_boost,
            hot_paths_identity: hot_paths_cache_identity(hot_paths_hint.as_ref()),
        };

        // Cache check: same scope + query + mode + filters within the
        // warm window → return the prior rendered result with a compact
        // [SEARCH_CACHED] marker. This must run after stale explicit
        // project validation so a cached response cannot pin the search to
        // a project_id that has just been rejected or auto-corrected.
        //
        // For local folder-scoped searches, correctness beats the warm-cache
        // win: the user may have edited files since the previous identical
        // query, and a rendered response cache would replay stale snippets.
        let cache_project_id = effective_search_cache_project_id(
            explicit_project_id,
            resolved_folder_project_id,
            local_index_project_id,
            project_id,
        );
        let guided_learning_request_id = if input.code_rerank_learning_opt_in == Some(true) {
            if workspace_id.is_none() || cache_project_id.is_none() {
                return Err(Error::Validation(
                        "code_rerank_learning_opt_in requires an exact workspace_id and project_id scope"
                            .to_string(),
                    ));
            }
            Some(Uuid::new_v4())
        } else {
            None
        };
        let caller_cache_scope = super::atlas_warm_cache::current_caller_cache_scope();
        let caller_cache_identity = caller_cache_scope.cache_identity();
        // Learning is side-effect-only and therefore excluded from cache
        // identity, but an explicitly opted-in call must reach the API so its
        // observation exists. Ordinary searches retain the warm-cache path.
        let use_search_cache = caller_cache_identity.is_some()
            && !checkout_scope_unroutable
            && should_use_search_cache(
                session_folder_path.as_deref(),
                folder_has_drift,
                input.code_rerank_learning_opt_in == Some(true),
            );
        // Resolve once immediately before cache identity construction. The
        // exact same opaque handle is forwarded on a miss; only its SHA-256
        // digest enters the Guided cache key. Non-Guided modes intentionally
        // ignore this session state.
        let resolved_guided_grounding_handle = if requested_mode == SearchMode::Guided {
            self.session.state().await.grounding_handle
        } else {
            None
        };
        let local_checkout_identity = local_checkout_cache_identity(
            session_folder_path.as_deref(),
            local_git_repository.as_deref(),
            local_git_branch.as_deref(),
            local_git_commit_sha.as_deref(),
        );
        let checkout_cache_identity = checkout_routing_scope
            .as_ref()
            .map(|scope| scope.checkout_locator.as_str())
            .or(local_checkout_identity.as_deref());
        let base_cache_key = build_search_cache_key_with_tokenizer(
            workspace_id,
            cache_project_id,
            &input,
            requested_mode,
            resolved_guided_grounding_handle.as_deref(),
            &cache_shapers,
            folder_index_indexed_at,
            checkout_cache_identity,
            &tokenizer_cache_namespace,
        );
        let cache_key = caller_cache_identity
            .map(|identity| caller_scoped_search_cache_key(&base_cache_key, identity))
            .unwrap_or_default();
        if use_search_cache {
            if let Some((cached_text, cached_structured)) = search_cache().get(&cache_key) {
                tracing::debug!("search cache hit: key={}", cache_key);
                // A rendered-result cache hit is read-only with respect to
                // session grounding state. The key is already partitioned by
                // the current input handle; replaying the cached response's
                // output handle could roll a concurrently advanced session
                // backward.
                let marked = if requested_mode == SearchMode::Guided {
                    // Preserve Guided Search's evidence-first wire contract
                    // even on a rendered-result cache hit.
                    format!(
                        "{}\n[SEARCH_CACHED] Reused the previous identical guided result (<{}s old).",
                        cached_text,
                        SEARCH_CACHE_TTL.as_secs(),
                    )
                } else {
                    format!(
                        "[SEARCH_CACHED] Same `{}` search as the previous identical call (<{}s ago); \
                     returning cached result. Change query/mode/filters to refresh.\n\n{}",
                        requested_mode.as_str(),
                        SEARCH_CACHE_TTL.as_secs(),
                        cached_text
                    )
                };
                let cached_result = bounded_search_tool_result(marked, cached_structured);
                if requested_mode == SearchMode::Guided {
                    let budget = guided_budget.unwrap_or_else(|| {
                        GuidedExecutionBudget::new(handler_started, GUIDED_SEARCH_REQUEST_TIMEOUT)
                    });
                    let timeout_result = guided_deadline_exhausted_result(
                        &input,
                        workspace_id,
                        cache_project_id,
                        budget,
                    );
                    let result = finalize_guided_search_result(
                        cached_result,
                        timeout_result,
                        wire_tokenizer_policy.clone(),
                        budget,
                    )
                    .await;
                    record_guided_total_latency(&result, budget);
                    return Ok(result);
                }
                return Ok(apply_search_wire_tokenizer(
                    cached_result,
                    &wire_tokenizer_policy,
                ));
            }
        }
        if requested_mode == SearchMode::Guided {
            let budget = guided_budget.unwrap_or_else(|| {
                GuidedExecutionBudget::new(handler_started, GUIDED_SEARCH_REQUEST_TIMEOUT)
            });
            let mut result = self
                .execute_guided_search_with_budget(
                    &input,
                    workspace_id,
                    cache_project_id,
                    resolved_guided_grounding_handle,
                    guided_learning_request_id,
                    checkout_routing_scope.clone(),
                    use_search_cache,
                    cache_key,
                    budget,
                )
                .await?;
            if checkout_scope_unroutable {
                result = mark_checkout_scope_unconfirmed(result);
            }
            let timeout_result =
                guided_deadline_exhausted_result(&input, workspace_id, cache_project_id, budget);
            let result = finalize_guided_search_result(
                result,
                timeout_result,
                wire_tokenizer_policy.clone(),
                budget,
            )
            .await;
            record_guided_total_latency(&result, budget);
            return Ok(result);
        }
        // The single active-index preflight above owns bounded exact repair.
        // Keeping one entry point prevents duplicate uploads and guarantees
        // that retrieval waits at most ACTIVE_INDEX_PREFLIGHT_TIMEOUT once.
        let sync_drift_note = if active_index_repair.reason.as_deref() == Some("drift") {
            if active_index_repair.complete {
                Some(format!(
                    "Detected {} locally changed file(s); committed {} repaired file(s) before searching, so this search includes the completed delta.",
                    active_index_repair.changed_file_count,
                    active_index_repair.files_indexed.unwrap_or(0)
                ))
            } else {
                Some(format!(
                    "Detected {} locally changed file(s); the bounded repair is queued or still indexing, so current search results may briefly lag those edits.",
                    active_index_repair.changed_file_count
                ))
            }
        } else {
            None
        };

        let mut candidate_project_ids: Vec<Option<Uuid>> = Vec::new();

        if let Some(id) = explicit_project_id {
            push_unique_project_candidate(&mut candidate_project_ids, Some(id));
            // Do not let a stale session folder silently redirect a valid
            // explicit project-scoped search into another project.
            if local_index_project_id == Some(id) {
                push_unique_project_candidate(&mut candidate_project_ids, local_index_project_id);
            }
            if resolved_folder_project_id == Some(id) {
                push_unique_project_candidate(
                    &mut candidate_project_ids,
                    resolved_folder_project_id,
                );
            }
            push_unique_project_candidate(&mut candidate_project_ids, project_id);
        } else {
            // Folder pin wins: an explicit `.contextstream/config.json`
            // project pin is the user's declared intent for this checkout and
            // must be tried before the local index mapping or inherited
            // session scope. Reroutes past the pin are flagged loudly below
            // via [PROJECT_ROUTING].
            push_unique_project_candidate(&mut candidate_project_ids, resolved_folder_project_id);
            push_unique_project_candidate(&mut candidate_project_ids, local_index_project_id);
            push_unique_project_candidate(&mut candidate_project_ids, project_id);
            if candidate_project_ids.is_empty() {
                push_unique_project_candidate(&mut candidate_project_ids, None);
            }
        }

        let mut team_scope_note: Option<String> = None;
        if requested_mode == SearchMode::Team {
            team_scope_note = expand_team_project_candidates(
                &self.client,
                workspace_id,
                &mut candidate_project_ids,
            )
            .await;
        }
        if requested_mode != SearchMode::Team && explicit_project_id.is_none() {
            for related_id in &shared_scope.related_project_ids {
                push_unique_project_candidate(&mut candidate_project_ids, Some(*related_id));
            }
        }

        let has_project_scope_candidates = candidate_project_ids.iter().any(|id| id.is_some());
        if should_allow_workspace_scope_fallback(
            requested_mode,
            &input.query,
            has_project_scope_candidates,
        ) && !candidate_project_ids.contains(&None)
        {
            candidate_project_ids.push(None);
        }

        let hot_paths_hint_note = hot_paths_hint.as_ref().map(|hint| {
            format!(
                "Current editor activity supplied as a bounded search advisory ({} paths).",
                hint.entries.len()
            )
        });
        let hot_path_guardrail_note = hot_paths_hint.as_ref().map(|_| {
            "Search relevance remains authoritative; activity-path context cannot manufacture a match."
                .to_string()
        });
        let base_params = SearchParams {
            query: input.query.clone(),
            cursor: input.cursor.clone(),
            workspace_id,
            project_id: None,
            // Durable session identity so the backend can rehydrate the
            // session's init-resolved scope when no project id survives the
            // gateway's in-memory state (audit 2026-07-17).
            session_id: state
                .session_id
                .clone()
                .filter(|value| !value.is_empty())
                .or_else(mcp_client::get_task_mcp_session_id),
            installation_id: checkout_routing_scope
                .as_ref()
                .map(|scope| scope.installation_id),
            checkout_locator: checkout_routing_scope
                .as_ref()
                .map(|scope| scope.checkout_locator.clone()),
            limit: resolved_limit,
            file_types: input.file_types.clone(),
            include_content: resolved_include_content,
            output_format: resolved_output_format,
            context_lines: resolved_context_lines,
            content_max_chars: Some(resolved_content_max_chars),
            exact_match_boost: resolved_exact_match_boost,
            offset: resolved_offset,
            include_memory,
            code_rerank_learning_opt_in: input.code_rerank_learning_opt_in,
            // `execute_api_search_attempt` mints a fresh UUID for every
            // concrete backend attempt and returns it paired with the response.
            code_rerank_learning_request_id: None,
            hot_paths_hint: hot_paths_hint.clone(),
        };
        let allow_broad_fallbacks = input.cursor.is_none()
            && mode_auto_selected
            && !is_identifier_query(&input.query)
            && !looks_like_symbol_anchor_query(&input.query);
        let mut fallback_stages: Vec<String> = Vec::new();
        push_fallback_stage(
            &mut fallback_stages,
            format!("requested:{}", requested_mode.as_str()),
        );

        // Phase 3: For identifier queries, spawn local ripgrep in parallel with
        // the API call so we have local results ready without extra latency.
        let parallel_local_handle = if is_identifier_query(&input.query) {
            if let Some(ref sfp) = session_folder_path {
                let folder = PathBuf::from(sfp.clone());
                let query_clone = input.query.clone();
                let file_type_filter_clone =
                    normalize_file_type_filter(input.file_types.as_deref());
                let ctx_lines = resolved_context_lines.unwrap_or(0) as usize;
                let max_chars = resolved_content_max_chars.max(50) as usize;
                Some(tokio::task::spawn_blocking(move || {
                    local_keyword_enrich_checked(
                        &folder,
                        &query_clone,
                        &HashSet::new(),
                        file_type_filter_clone.as_ref(),
                        ctx_lines,
                        max_chars,
                        false,
                    )
                }))
            } else {
                None
            }
        } else {
            None
        };

        type SelectedSearchAttempt = (
            usize,
            Option<Uuid>,
            SearchResponse,
            SearchMode,
            Option<String>,
            Option<Uuid>,
        );
        let mut selected: Option<SelectedSearchAttempt> = None;
        let mut explicit_scope_had_no_results = false;
        for (idx, candidate_project_id) in candidate_project_ids.iter().enumerate() {
            let mut params = base_params.clone();
            params.project_id = *candidate_project_id;

            match run_search_for_mode(
                &self.client,
                requested_mode,
                params,
                &input.query,
                allow_broad_fallbacks,
            )
            .await
            {
                Ok((result, executed_mode, mode_fallback_note, learning_request_id)) => {
                    push_fallback_stage(
                        &mut fallback_stages,
                        format!("candidate:{}:{}", idx, executed_mode.as_str()),
                    );
                    // When the API reports scope invalid (e.g. project_access_denied),
                    // skip this candidate and try the next one rather than returning
                    // an empty result with an error message.
                    if !result.scope_is_valid() {
                        if selected.is_none() {
                            selected = Some((
                                idx,
                                *candidate_project_id,
                                result,
                                executed_mode,
                                mode_fallback_note,
                                learning_request_id,
                            ));
                        }
                        continue;
                    }

                    let has_hits = !result.results.is_empty() || result.total.unwrap_or(0) > 0;

                    if explicit_project_id.is_some() && idx == 0 && !has_hits {
                        explicit_scope_had_no_results = true;
                    }

                    if has_hits {
                        selected = Some((
                            idx,
                            *candidate_project_id,
                            result,
                            executed_mode,
                            mode_fallback_note,
                            learning_request_id,
                        ));
                        break;
                    }

                    if selected.is_none() {
                        selected = Some((
                            idx,
                            *candidate_project_id,
                            result,
                            executed_mode,
                            mode_fallback_note,
                            learning_request_id,
                        ));
                    }
                }
                Err(err) if is_not_found_error(&err) => continue,
                Err(err) if is_access_denied_error(&err) => continue,
                Err(err) => return Err(err),
            }
        }

        // Self-heal: when every project-scoped candidate returned NotFound /
        // Forbidden, the most common cause is a stale `project_id` from the
        // session (e.g. project was renamed, the agent switched workspaces,
        // or `init` was never called in this folder). Returning a hard
        // validation error there forces the agent to recover with a follow-up
        // `init` call — which most agents won't do mid-task. Falling back to
        // a workspace-scoped search instead lets the call succeed with a
        // soft note, matching the resilience the user asked for.
        let can_retry_broader_scope = explicit_project_id.is_none()
            && (!candidate_project_ids.contains(&None)
                || (explicit_workspace_id.is_none() && base_params.workspace_id.is_some()));
        if selected.is_none() && can_retry_broader_scope {
            let mut workspace_params = base_params.clone();
            workspace_params.project_id = None;
            let dropped_workspace_id = if explicit_workspace_id.is_none() {
                let dropped = workspace_params.workspace_id;
                workspace_params.workspace_id = None;
                dropped
            } else {
                None
            };
            if let Ok((
                fallback_result,
                fallback_mode,
                fallback_note,
                fallback_learning_request_id,
            )) = run_search_for_mode(
                &self.client,
                requested_mode,
                workspace_params,
                &input.query,
                allow_broad_fallbacks,
            )
            .await
            {
                push_fallback_stage(
                    &mut fallback_stages,
                    format!("workspace_scope_retry:{}", fallback_mode.as_str()),
                );
                let recovery_note =
                    "Project scope was unresolvable in this session — falling back to a broader search scope. \
                     Call init() in this folder (or pass project_id explicitly) to restore project-scoped results.";
                let recovery_note = if let Some(dropped) = dropped_workspace_id {
                    format!(
                        "{} Dropped inherited workspace_id {} for this retry because it may be stale or inaccessible.",
                        recovery_note, dropped
                    )
                } else {
                    recovery_note.to_string()
                };
                let combined_note = match fallback_note {
                    Some(existing) => Some(format!("{} {}", recovery_note, existing)),
                    None => Some(recovery_note),
                };
                selected = Some((
                    candidate_project_ids.len(),
                    None,
                    fallback_result,
                    fallback_mode,
                    combined_note,
                    fallback_learning_request_id,
                ));
            }
        }

        let (
            resolved_candidate_index,
            resolved_project_id,
            mut result,
            mut executed_mode,
            mut mode_fallback_note,
            mut served_learning_request_id,
        ) =
            selected.ok_or_else(|| {
                let base = "Project not found for current context. Call init(...) in this folder or pass a valid project_id explicitly.";
                if let Some(note) = shared_scope.note.as_deref() {
                    Error::Validation(format!("{} {}", note, base))
                } else {
                    Error::Validation(base.to_string())
                }
            })?;
        ensure_search_query_echo(&mut result, &input.query);
        normalize_count_index_trust(&mut result, base_params.output_format.as_deref());
        let checkout_scope_confirmed = if checkout_scope_requested {
            checkout_routing_scope.as_ref().is_some_and(|expected| {
                result.checkout_scope.as_ref().is_some_and(|actual| {
                    actual.matches(expected.installation_id, &expected.checkout_locator)
                })
            })
        } else {
            true
        };
        // A backend receipt can only describe a backend candidate set. If the
        // selected API response carried no rows, later local-only enrichment
        // must not surface a UUID that can never resolve to an observation.
        served_learning_request_id =
            served_api_learning_receipt(&result, served_learning_request_id);

        if let Some(note) = sync_drift_note {
            mode_fallback_note = append_note(mode_fallback_note, &note);
        }
        if !checkout_scope_confirmed {
            mode_fallback_note = append_note(
                mode_fallback_note,
                "Hosted search served canonical project results, but the service did not confirm the active checkout overlay; do not infer that uncommitted worktree changes are present from project-wide index state alone.",
            );
        }

        let target_project_id = resolved_project_id.or(project_id);
        let local_project_root = read_local_project_root_for_project(target_project_id)
            .or_else(|| read_current_config_root_for_project(target_project_id));
        let allow_current_dir_fallback = target_project_id.is_none();
        let mut resolved_project_meta_path: Option<String> = None;
        let mut resolved_project_name: Option<String> = None;
        let scoped_session_folder_path = scoped_session_folder_path(
            session_folder_path.as_deref(),
            resolved_folder_project_id,
            local_index_project_id,
            target_project_id,
            local_project_root.as_deref(),
        );
        let resolved_project_root = if let Some(project_id) = resolved_project_id {
            match self.client.get_project(project_id).await {
                Ok(project) => {
                    resolved_project_meta_path = project.path.clone();
                    resolved_project_name = Some(project.name.clone());
                    resolve_effective_folder_path(
                        scoped_session_folder_path,
                        project.path.as_deref(),
                        local_project_root.as_deref(),
                        allow_current_dir_fallback,
                    )
                }
                Err(err) => {
                    tracing::debug!(
                        project_id = %project_id,
                        error = %err,
                        "search: failed to load project root; falling back to session folder path"
                    );
                    resolve_effective_folder_path(
                        scoped_session_folder_path,
                        None,
                        local_project_root.as_deref(),
                        allow_current_dir_fallback,
                    )
                }
            }
        } else {
            resolve_effective_folder_path(
                scoped_session_folder_path,
                None,
                local_project_root.as_deref(),
                allow_current_dir_fallback,
            )
        };
        let mut folder_path = resolved_project_root;
        if folder_path.is_none() {
            folder_path = read_local_project_root_for_project(resolved_project_id.or(project_id));
            if folder_path.is_none() && target_project_id.is_none() {
                folder_path = current_dir_search_root();
            }
        }

        // Folder-pin reroute guard: when this checkout is pinned to a project
        // via `.contextstream/config.json` and scope fallback ended up serving
        // results from a different project, the reroute must be loud. A silent
        // reroute sends the agent chasing files that belong to another repo.
        let mut project_routing_warning: Option<String> = None;
        if explicit_project_id.is_none() {
            if let Some(pinned_project_id) = resolved_folder_project_id {
                if resolved_project_id != Some(pinned_project_id) {
                    let resolved_label = resolved_project_id
                        .map(|id| format!("project_id {}", id))
                        .unwrap_or_else(|| "workspace-wide scope".to_string());
                    project_routing_warning = Some(format!(
                        "[PROJECT_ROUTING] This folder is pinned to project_id {} via .contextstream/config.json, but these results come from {} (the pinned project returned no usable results for this query). Treat cross-project hits with caution — file paths may not exist in this checkout. To force the pinned scope, pass project_id=\"{}\" explicitly; if the pin is stale, re-run init(folder_path=\"...\").",
                        pinned_project_id, resolved_label, pinned_project_id
                    ));
                }
            }
        }

        // A canonical project may have several machine/worktree overlays.
        // When the server's legacy root hint differs from this session's
        // checkout, refresh this overlay without replacing or distrusting the
        // others.
        let mut index_origin_note: Option<String> = None;
        if let (Some(meta_path), Some(local_folder)) = (
            resolved_project_meta_path.as_deref(),
            session_folder_path.as_deref(),
        ) {
            if !indexed_root_matches_local_folder(meta_path, local_folder) {
                let already_redirected_to_local_twin =
                    local_twin_redirect.as_ref().is_some_and(|r| {
                        r.target_project_id == resolved_project_id.unwrap_or(r.target_project_id)
                            && indexed_root_matches_local_folder(
                                &r.target_folder_path,
                                local_folder,
                            )
                    });
                if already_redirected_to_local_twin {
                    if let Some(redirect) = local_twin_redirect.as_ref() {
                        tracing::debug!(
                            source_project_id = %redirect.source_project_id,
                            target_project_id = %redirect.target_project_id,
                            indexed_root = %redirect.indexed_root,
                            target_folder = %redirect.target_folder_path,
                            "suppressing INDEX_SCOPE warning after local-twin redirect"
                        );
                    }
                } else if !std::path::Path::new(local_folder).is_dir() {
                    // Hosted MCP cannot read the path, but it can request the
                    // exact registered checkout's managed sync bridge.
                    index_origin_note = self.index_keeper.maybe_trigger_scope_repair_reingest(
                        workspace_id,
                        resolved_project_id,
                        Some(local_folder),
                        meta_path,
                    );
                } else {
                    let repair_note = if can_auto_repair_index_root_mismatch(
                        meta_path,
                        local_folder,
                        resolved_project_name.as_deref(),
                        resolved_project_id,
                        resolved_folder_project_id,
                        local_index_project_id,
                    ) {
                        self.index_keeper.maybe_trigger_scope_repair_reingest(
                            workspace_id,
                            resolved_project_id,
                            Some(local_folder),
                            meta_path,
                        )
                    } else {
                        None
                    };

                    if let Some(note) = repair_note {
                        index_origin_note = Some(note);
                    } else if should_emit_index_scope_warning(
                        resolved_project_id,
                        meta_path,
                        local_folder,
                    ) {
                        let project_label = resolved_project_name
                            .as_deref()
                            .map(|name| format!(" (project `{}`)", name))
                            .unwrap_or_default();
                        index_origin_note = Some(format!(
                            "[INDEX_CHECKOUT] Canonical results currently include checkout data rooted at `{}`{}; the active checkout is `{}`. Refreshing this checkout overlay without replacing other machines or worktrees — current results remain usable.",
                            meta_path, project_label, local_folder
                        ));
                    } else {
                        tracing::debug!(
                            project_id = ?resolved_project_id,
                            indexed_root = %meta_path,
                            local_folder = %local_folder,
                            "suppressing repeated INDEX_SCOPE warning"
                        );
                    }
                }
            }
        }

        if let Some(note) = team_scope_note.take() {
            mode_fallback_note = append_note(mode_fallback_note, &note);
        }
        if let Some(note) = shared_scope.note.as_deref() {
            mode_fallback_note = append_note(mode_fallback_note, note);
        }
        let include_memory = base_params.include_memory.unwrap_or(false);
        if !include_memory && !result.results.is_empty() {
            let before = result.results.len();
            result.results.retain(|item| !is_memory_result(item));
            let removed = before.saturating_sub(result.results.len());
            if removed > 0 {
                result.total = Some(result.results.len() as i64);
                mode_fallback_note = append_note(
                    mode_fallback_note,
                    &format!(
                        "Suppressed {} memory-only result(s); set include_memory=true to include memory context in search output.",
                        removed
                    ),
                );
            }
        }
        if should_filter_artifact_paths(requested_mode, &input.query) && !result.results.is_empty()
        {
            let before = result.results.len();
            result
                .results
                .retain(|item| !result_has_artifact_like_path(item));
            let removed = before.saturating_sub(result.results.len());
            if removed > 0 {
                result.total = Some(result.results.len() as i64);
                mode_fallback_note = append_note(
                    mode_fallback_note,
                    &format!(
                        "Suppressed {} build/artifact path result(s) (e.g. `.next`, sourcemaps).",
                        removed
                    ),
                );
            }
        }
        // Duplicate suppression: collapse mirror-prefix duplicates (requirement #5)
        // First, canonicalize file paths so internal storage paths
        // (e.g. web/users/.../projects/.../apps/...) are resolved to
        // repo-relative paths for all output formats, not just paths/minimal.
        for item in &mut result.results {
            if let Some(ref fp) = item.file_path {
                let canonical = crate::domains::scope::canonicalize_repo_path(fp);
                if canonical != *fp {
                    item.file_path = Some(canonical);
                }
            }
        }
        let dedup_count = crate::domains::scope::deduplicate_results(&mut result);
        if dedup_count > 0 {
            mode_fallback_note = append_note(
                mode_fallback_note,
                &format!(
                    "Collapsed {} duplicate result(s) from mirror prefixes.",
                    dedup_count
                ),
            );
        }
        let path_dedup_count = crate::domains::scope::deduplicate_paths(&mut result);
        if path_dedup_count > 0 {
            mode_fallback_note = append_note(
                mode_fallback_note,
                &format!(
                    "Collapsed {} duplicate path(s) from mirror prefixes.",
                    path_dedup_count
                ),
            );
        }

        // Absolute path resolution for paths/minimal output (requirement #4)
        if let Some(ref fp) = folder_path {
            let output_fmt = base_params
                .output_format
                .as_deref()
                .unwrap_or("full")
                .to_lowercase();
            if output_fmt == "paths" || output_fmt == "minimal" || !result.paths.is_empty() {
                let dropped = crate::domains::scope::resolve_search_paths(&mut result, fp);
                if !dropped.is_empty() {
                    mode_fallback_note = append_note(
                        mode_fallback_note,
                        &format!(
                            "Dropped {} unresolvable path(s) from output.",
                            dropped.len()
                        ),
                    );
                }
            }
        }

        // Scope reliability diagnostics (requirement #6)
        let scope_diag = crate::domains::scope::extract_scope_diagnostics(&result);
        let scope_invalid = !result.scope_is_valid();

        if explicit_scope_had_no_results && resolved_candidate_index > 0 {
            mode_fallback_note = append_note(
                mode_fallback_note,
                "Explicit project_id returned no results; retried folder/local project mapping.",
            );
        }
        if resolved_project_id.is_none() && resolved_candidate_index > 0 {
            mode_fallback_note = append_note(
                mode_fallback_note,
                "Project-scoped search returned no results; retried workspace-wide scope.",
            );
        }
        if requested_mode == SearchMode::Team && resolved_candidate_index > 0 {
            if let Some(id) = resolved_project_id {
                mode_fallback_note = append_note(
                    mode_fallback_note,
                    &format!(
                        "Cross-project fallback matched in workspace project `{}` after checking the primary project scope.",
                        id
                    ),
                );
            } else {
                mode_fallback_note = append_note(
                    mode_fallback_note,
                    "Cross-project fallback matched only in workspace-wide scope after checking project scopes.",
                );
            }
        }

        if explicit_project_id.is_none() && folder_path.is_some() {
            let project_changed = resolved_project_id != project_id;
            let workspace_changed = workspace_id != state.workspace_id;
            if project_changed || workspace_changed {
                self.session
                    .update_scope(
                        workspace_id,
                        resolved_project_id.or(project_id),
                        folder_path.clone(),
                    )
                    .await;
            }
        }
        if let (Some(local_project_id), Some(path)) =
            (local_index_project_id, folder_path.as_deref())
        {
            if resolved_project_id != Some(local_project_id) {
                let resolved_scope = resolved_project_id
                    .map(|id| id.to_string())
                    .unwrap_or_else(|| "workspace-wide scope".to_string());
                mode_fallback_note = append_note(
                    mode_fallback_note,
                    &local_mapping_mismatch_note(local_project_id, &resolved_scope, path),
                );
            }
        }

        let local_path_probe = resolve_local_path_probe(&input.query, folder_path.as_deref());
        let local_index_entry = folder_path.as_deref().and_then(read_local_index_entry);
        let local_project_id_for_trust = local_index_entry
            .as_ref()
            .and_then(|entry| entry.project_id)
            .or(local_index_project_id);
        let local_indexed_at_known = local_index_entry
            .as_ref()
            .and_then(|entry| entry.indexed_at)
            .is_some();
        let search_api_index_hint = checkout_scope_confirmed
            .then(|| {
                extract_api_index_hint(&result, folder_path.as_deref(), local_path_probe.as_ref())
            })
            .flatten();
        let status_api_index_hint = if let Some(project_id) = resolved_project_id {
            if let Some(status) = self
                .client
                .cached_project_index_status_for_checkout(project_id, folder_path.as_deref())
            {
                if ContextStreamClient::project_index_status_is_checkout_scoped(&status)
                    && !ContextStreamClient::project_index_status_matches_checkout(&status)
                {
                    None
                } else {
                    extract_project_status_index_hint(
                        &status,
                        folder_path.as_deref(),
                        local_path_probe.as_ref(),
                    )
                }
            } else if search_api_index_hint.is_some() {
                // The search response already attests an active index state or
                // generation. Do not let an incidental freshness lookup gate
                // results: production status outliers have exceeded 10s while
                // the authoritative search itself completed in <100ms.
                None
            } else {
                self.client
                    .project_index_status_cached_for_checkout(project_id, folder_path.as_deref())
                    .await
                    .ok()
                    .and_then(|status| {
                        if ContextStreamClient::project_index_status_is_checkout_scoped(&status)
                            && !ContextStreamClient::project_index_status_matches_checkout(&status)
                        {
                            None
                        } else {
                            extract_project_status_index_hint(
                                &status,
                                folder_path.as_deref(),
                                local_path_probe.as_ref(),
                            )
                        }
                    })
            }
        } else {
            None
        };
        let api_index_hint = merge_api_index_hints(search_api_index_hint, status_api_index_hint);
        if local_index_entry.is_none() {
            if let (Some(path), Some(project_id), Some(api_hint)) = (
                folder_path.as_deref(),
                resolved_project_id,
                api_index_hint.as_ref(),
            ) {
                if api_hint.indicates_ready {
                    if let Some(indexed_at) = api_hint.indexed_at.as_ref() {
                        // Best-effort local self-heal to keep MCP freshness
                        // checks aligned with API state. Preserve the API's
                        // ingest timestamp; writing "now" would hide stale
                        // committed generations from future local checks.
                        ContextStreamClient::write_index_status_at(
                            path,
                            project_id,
                            indexed_at.to_owned(),
                        );
                    } else {
                        tracing::debug!(
                            "Skipping local index cache backfill: API reported ready without an ingest timestamp"
                        );
                    }
                }
            }
        }
        let index_health = build_index_health(
            folder_path.as_deref(),
            resolved_project_id,
            local_path_probe.as_ref(),
            local_index_entry.clone(),
            api_index_hint,
            &activity_file_hints,
        );
        harmonize_project_index_state(&mut result, index_health.as_ref());
        let dirty_file_hints = activity_file_hints.clone();

        // Prune results for files deleted from the local working tree before
        // computing no_hits, so a drifted index never silently serves hits for
        // files that no longer exist. Guarded to local directories only.
        if !scope_invalid {
            if let Some(ref fp) = folder_path {
                let folder = Path::new(fp);
                if folder.is_dir() {
                    let pruned_deleted = prune_deleted_file_results(&mut result, folder);
                    if pruned_deleted > 0 {
                        mode_fallback_note = append_note(
                            mode_fallback_note,
                            &format!(
                                "Removed {} result(s) for files no longer on disk; re-index to fully refresh the index.",
                                pruned_deleted
                            ),
                        );
                    }
                }
            }
        }

        let no_hits = result.total.unwrap_or(result.results.len() as i64) == 0;
        let stale_reingest_note = if !no_hits {
            if let Some(ref health) = index_health {
                self.index_keeper.maybe_trigger_stale_reingest(
                    workspace_id,
                    resolved_project_id,
                    folder_path.as_deref(),
                    health.freshness,
                    health.drift_detected,
                    health.scope_match,
                )
            } else {
                None
            }
        } else {
            None
        };
        let compact_without_rows = result.results.is_empty() && result.total.unwrap_or(0) > 0;
        let scope_fallback_applied = resolved_candidate_index > 0;
        let has_file_type_filter = input
            .file_types
            .as_ref()
            .map(|types| !types.is_empty())
            .unwrap_or(false);
        let wants_row_details = input.include_content.unwrap_or(false)
            || resolved_context_lines.unwrap_or(0) > 0
            || has_file_type_filter;
        let should_recover_compact_rows = compact_without_rows && wants_row_details;

        if !scope_invalid && no_hits {
            if let Some(note) = maybe_trigger_targeted_reingest(
                &self.client,
                workspace_id,
                resolved_project_id,
                folder_path.as_deref(),
                local_path_probe.as_ref(),
                index_health.as_ref(),
            ) {
                mode_fallback_note = append_note(mode_fallback_note, &note);
            }
        }

        let mut local_enrichment_count_total = 0usize;
        let mut local_enrichment_diagnostic: Option<LocalEnrichDiagnostic> = None;
        // Local filesystem enrichment: supplement API results with live filesystem scanning.
        // Apply only when scope is stable and the query shape is likely to benefit.
        if !scope_invalid {
            if let Some(ref fp) = folder_path {
                let folder = PathBuf::from(fp);
                let existing_paths: HashSet<String> = result
                    .results
                    .iter()
                    .filter_map(|r| r.file_path.clone())
                    .collect();
                let file_type_filter = normalize_file_type_filter(input.file_types.as_deref());
                let context_lines = resolved_context_lines.unwrap_or(0) as usize;
                let content_max_chars = resolved_content_max_chars.max(50) as usize;
                let mut local_enrichment_count = 0usize;

                let refreshed_count = refresh_indexed_result_snippets_from_local_files(
                    &mut result,
                    &folder,
                    &input.query,
                    context_lines,
                    content_max_chars,
                );
                if refreshed_count > 0 {
                    mode_fallback_note = append_note(
                        mode_fallback_note,
                        &format!(
                            "Refreshed {} indexed result snippet(s) from the local working tree.",
                            refreshed_count
                        ),
                    );
                }

                let normalized_enrich_query = normalized_symbol_retry_query(&input.query);
                let exact_identifier_or_symbol_query =
                    is_identifier_query(&normalized_enrich_query)
                        || looks_like_symbol_anchor_query(&input.query);
                let should_enrich = should_apply_local_enrichment(
                    executed_mode,
                    &input.query,
                    no_hits,
                    should_recover_compact_rows,
                    scope_invalid,
                    scope_fallback_applied,
                ) || exact_identifier_or_symbol_query;

                // When the parallel identifier pre-fetch is already in flight
                // and would cover the same local_keyword_enrich call, skip the
                // sequential enrichment to avoid duplicate work.
                //
                // Exact identifier/symbol queries intentionally still run the
                // resolved-folder enrichment here: the pre-fetch only knows
                // the initial session folder, while this path has the final
                // project root. The extra ripgrep call is bounded and keeps
                // fresh local symbols from being buried under stale API hits.
                let parallel_covers_enrich = parallel_local_handle.is_some()
                    && !exact_identifier_or_symbol_query
                    && !matches!(executed_mode, SearchMode::Pattern);

                if should_enrich && !parallel_covers_enrich {
                    let query_for_enrich = input.query.clone();
                    let mode_for_enrich = executed_mode;
                    let file_type_filter_for_enrich = file_type_filter.clone();
                    let enrich_result = tokio::time::timeout(
                        LOCAL_ENRICH_TIMEOUT,
                        tokio::task::spawn_blocking(move || {
                            if mode_for_enrich == SearchMode::Pattern {
                                if is_glob_like(&query_for_enrich) {
                                    LocalEnrichOutcome::from_results(local_glob_enrich(
                                        &folder,
                                        &query_for_enrich,
                                        &existing_paths,
                                    ))
                                } else {
                                    LocalEnrichOutcome::from_results(local_path_enrich(
                                        &folder,
                                        &query_for_enrich,
                                        &existing_paths,
                                    ))
                                }
                            } else {
                                local_keyword_enrich_checked(
                                    &folder,
                                    &query_for_enrich,
                                    &existing_paths,
                                    file_type_filter_for_enrich.as_ref(),
                                    context_lines,
                                    content_max_chars,
                                    exact_identifier_or_symbol_query,
                                )
                            }
                        }),
                    )
                    .await;

                    if let Ok(Ok(mut local_outcome)) = enrich_result {
                        if local_enrichment_diagnostic.is_none() {
                            local_enrichment_diagnostic = local_outcome.diagnostic.take();
                        }
                        let local_results = local_outcome.results;
                        if !local_results.is_empty() {
                            let count = local_results.len();
                            local_enrichment_count = count;
                            local_enrichment_count_total += count;
                            push_fallback_stage(
                                &mut fallback_stages,
                                "local_enrichment".to_string(),
                            );
                            result.results.extend(local_results);
                            result.total = Some(result.total.unwrap_or(0) + count as i64);
                            mode_fallback_note = append_note(
                                mode_fallback_note,
                                &format!("Enriched with {} local file(s) not yet in index.", count),
                            );
                        }
                    }
                }

                if should_recover_compact_rows && local_enrichment_count > 0 {
                    mode_fallback_note = append_note(
                        mode_fallback_note,
                        "Server returned compact rows for a detail-oriented query; supplemented with local filesystem snippets.",
                    );
                    if has_file_type_filter {
                        // Compact count responses can bypass server-side row filtering.
                        // When file type filters are requested, trust concrete rows.
                        result.total = Some(result.results.len() as i64);
                        result.has_more = None;
                        result.next_offset = None;
                    }
                }
            }
        }

        // Merge parallel local results from the identifier pre-fetch if available.
        if let Some(handle) = parallel_local_handle {
            if let Ok(Ok(parallel_outcome)) =
                tokio::time::timeout(LOCAL_ENRICH_TIMEOUT, handle).await
            {
                let LocalEnrichOutcome {
                    results: parallel_results,
                    diagnostic,
                } = parallel_outcome;
                if local_enrichment_diagnostic.is_none() {
                    local_enrichment_diagnostic = diagnostic;
                }
                if !parallel_results.is_empty() {
                    let existing: HashSet<String> = result
                        .results
                        .iter()
                        .filter_map(|r| r.file_path.clone())
                        .collect();
                    let mut added = 0usize;
                    for local_result in parallel_results {
                        if let Some(ref fp) = local_result.file_path {
                            if !existing.contains(fp) {
                                result.results.push(local_result);
                                added += 1;
                            }
                        }
                    }
                    if added > 0 {
                        local_enrichment_count_total += added;
                        result.total = Some(result.total.unwrap_or(0) + added as i64);
                        mode_fallback_note = append_note(
                            mode_fallback_note,
                            &format!(
                                "Merged {} parallel local result(s) for identifier query.",
                                added
                            ),
                        );
                    }
                }
            }
        }

        if let Some(note) = apply_symbol_anchor_rerank(&mut result, &input.query) {
            push_fallback_stage(&mut fallback_stages, "symbol_anchor_rerank".to_string());
            mode_fallback_note = append_note(mode_fallback_note, &note);
        }

        if supports_token_fusion(executed_mode) {
            if let Some(note) = apply_post_rank_fusion(&mut result, &input.query) {
                push_fallback_stage(&mut fallback_stages, "post_rank_fusion".to_string());
                mode_fallback_note = append_note(mode_fallback_note, &note);
            }
        }

        // Keyword term verification: demote results that don't actually contain
        // any of the query's tokens. BM25 sparse-vector search can produce false
        // positives when the index is stale or tokenization splits terms differently.
        let mut token_demotions = 0usize;
        if supports_token_fusion(executed_mode) && !result.results.is_empty() {
            let demoted = demote_keyword_false_positives(&mut result, &input.query);
            if demoted > 0 {
                token_demotions = demoted;
                push_fallback_stage(&mut fallback_stages, "token_demotion".to_string());
                mode_fallback_note = append_note(
                    mode_fallback_note,
                    &format!(
                        "Demoted {} result(s) that did not contain query terms after token verification.",
                        token_demotions
                    ),
                );
            }
        }

        // Recalculate no_hits after local enrichment
        let mut no_hits = result.total.unwrap_or(result.results.len() as i64) == 0;

        // Phase 5: Final ripgrep safety-net. When every API and enrichment path
        // yielded zero results and we have a folder path, run one last ripgrep
        // scan so we never return empty when Grep would have found something.
        if no_hits && !scope_invalid && is_local_keyword_enrichment_query(&input.query) {
            if let Some(ref fp) = folder_path {
                let folder = PathBuf::from(fp);
                let query_for_last = input.query.clone();
                let file_type_filter_last = normalize_file_type_filter(input.file_types.as_deref());
                let ctx_lines = resolved_context_lines.unwrap_or(0) as usize;
                let max_chars = resolved_content_max_chars.max(50) as usize;
                let last_resort = tokio::time::timeout(
                    LOCAL_ENRICH_TIMEOUT,
                    tokio::task::spawn_blocking(move || {
                        local_keyword_enrich_checked(
                            &folder,
                            &query_for_last,
                            &HashSet::new(),
                            file_type_filter_last.as_ref(),
                            ctx_lines,
                            max_chars,
                            false,
                        )
                    }),
                )
                .await;

                if let Ok(Ok(mut last_outcome)) = last_resort {
                    if local_enrichment_diagnostic.is_none() {
                        local_enrichment_diagnostic = last_outcome.diagnostic.take();
                    }
                    let last_results = last_outcome.results;
                    if !last_results.is_empty() {
                        let count = last_results.len();
                        push_fallback_stage(
                            &mut fallback_stages,
                            "last_resort_ripgrep".to_string(),
                        );
                        result.results.extend(last_results);
                        result.total = Some(count as i64);
                        no_hits = false;
                        mode_fallback_note = append_note(
                            mode_fallback_note,
                            &format!(
                                "All API searches returned empty; last-resort local ripgrep found {} result(s).",
                                count
                            ),
                        );
                    }
                }
            }
        }

        // Mode escalation: when the primary mode returned 0 results, automatically
        // retry with progressively broader modes before giving up. This prevents
        // the AI from seeing "0 results" and falling back to local tools.
        if should_run_broad_mode_escalation(no_hits, scope_invalid, allow_broad_fallbacks) {
            let escalation_modes: Vec<SearchMode> = match executed_mode {
                SearchMode::Semantic => vec![SearchMode::Hybrid, SearchMode::Keyword],
                SearchMode::Hybrid => vec![SearchMode::Keyword],
                SearchMode::Keyword => vec![SearchMode::Hybrid],
                SearchMode::Pattern => vec![SearchMode::Keyword, SearchMode::Hybrid],
                SearchMode::Exhaustive => vec![SearchMode::Keyword, SearchMode::Hybrid],
                _ => vec![],
            };

            for escalation_mode in escalation_modes
                .into_iter()
                .take(MAX_MODE_ESCALATION_ATTEMPTS)
            {
                let mut retry_params = base_params.clone();
                retry_params.project_id = resolved_project_id;

                match run_search_for_mode(
                    &self.client,
                    escalation_mode,
                    retry_params,
                    &input.query,
                    false,
                )
                .await
                {
                    Ok((retry_result, retry_executed_mode, _, retry_learning_request_id)) => {
                        push_fallback_stage(
                            &mut fallback_stages,
                            format!("escalation:{}", retry_executed_mode.as_str()),
                        );
                        let retry_has_hits =
                            !retry_result.results.is_empty() || retry_result.total.unwrap_or(0) > 0;
                        if retry_has_hits {
                            served_learning_request_id = served_api_learning_receipt(
                                &retry_result,
                                retry_learning_request_id,
                            );
                            result = retry_result;
                            executed_mode = retry_executed_mode;
                            no_hits = false;
                            mode_fallback_note = append_note(
                                mode_fallback_note,
                                &format!(
                                    "Primary {} search returned empty; escalated to {} which found results.",
                                    requested_mode.as_str(),
                                    retry_executed_mode.as_str(),
                                ),
                            );
                            break;
                        }
                    }
                    Err(_) => continue,
                }
            }
        }

        let docs_fallback = if !scope_invalid && no_hits && is_doc_lookup_query(&input.query) {
            find_docs_fallback(
                &self.client,
                workspace_id,
                &candidate_project_ids,
                &input.query,
                input.limit,
            )
            .await
        } else {
            None
        };

        // Skills enrichment: keep this explicit and high-signal. We only fetch
        // skills when the user query appears to ask for a reusable workflow.
        let skill_query_intent = is_skill_query(&input.query);
        let adaptive_skill_threshold = skill_score_threshold(&input.query);
        let skills_enrichment_reason = if skill_query_intent {
            "query asks for reusable workflow/skill guidance"
        } else {
            "query does not ask for reusable workflow/skill guidance"
        };
        let mut matched_skills_before_threshold = 0usize;
        let matched_skills = if skill_query_intent {
            match self
                .client
                .match_skills(workspace_id.unwrap_or_default(), &input.query, Some(3))
                .await
            {
                Ok(val) => {
                    let raw_matches = val.as_array().cloned().unwrap_or_default();
                    matched_skills_before_threshold = raw_matches.len();
                    raw_matches
                        .into_iter()
                        .filter(|skill| {
                            skill
                                .get("score")
                                .or_else(|| skill.get("confidence"))
                                .or_else(|| skill.get("relevance"))
                                .and_then(|value| value.as_f64())
                                .map(|score| score >= adaptive_skill_threshold)
                                .unwrap_or(true)
                        })
                        .collect()
                }
                Err(_) => vec![],
            }
        } else {
            vec![]
        };

        // Graph and prewarmed project-map intelligence are independent
        // enrichments. Run them concurrently so broad searches pay at most
        // the slower enrichment, not their sum. Exact/count paths skip the
        // map entirely because they cannot use a route hint.
        let graph_enrichment = async {
            if !should_fetch_graph_enrichment(&input.query, count_only_output, no_hits) {
                return None;
            }
            match tokio::time::timeout(
                GRAPH_ENRICHMENT_TIMEOUT,
                try_graph_enrichment(&self.client, &result, resolved_project_id),
            )
            .await
            {
                Ok(context) => context,
                Err(_) => {
                    tracing::debug!(
                        timeout_ms = GRAPH_ENRICHMENT_TIMEOUT.as_millis() as u64,
                        "search graph enrichment timed out; returning search results without graph context"
                    );
                    None
                }
            }
        };
        let project_map_enrichment = async {
            if !should_fetch_project_map_route_hint(
                &input.query,
                base_params.output_format.as_deref(),
                &result,
            ) {
                return None;
            }
            fetch_project_map_route_hint(
                &self.client,
                project_id.or(resolved_project_id),
                &input.query,
            )
        };
        let (graph_context, project_map_route) =
            tokio::join!(graph_enrichment, project_map_enrichment);
        let readable_session_folder = session_folder_path
            .as_deref()
            .filter(|folder| std::path::Path::new(folder).is_dir());
        let local_enrichment_warning = local_enrichment_unavailable_warning_for_response(
            &input.query,
            &result,
            local_enrichment_diagnostic.as_ref(),
            readable_session_folder,
        );
        if local_enrichment_warning.is_some() {
            push_fallback_stage(
                &mut fallback_stages,
                "local_enrichment_unavailable".to_string(),
            );
        }

        let requested_paths_output = input
            .output_format
            .as_deref()
            .map(|fmt| fmt.eq_ignore_ascii_case("paths"))
            .unwrap_or(false);
        if requested_paths_output {
            normalize_paths_output(&mut result);
        }

        // Hard row cap: never render more rows than the resolved limit,
        // regardless of mode. Exhaustive/refactor endpoints can return
        // hundreds of rows (server-bounded at 300); without this cap a single
        // search can blow harness tool-result token limits. Runs LAST — after
        // dedup, local ripgrep enrichment, and mode-escalation retries — so no
        // later stage can re-inflate the payload. The true pre-cap total is
        // preserved in `result.total`, and the note is stored separately from
        // `mode_fallback_note` because it must render even in concise mode.
        let mut output_budget_note: Option<String> = None;
        let display_row_cap = resolved_limit.unwrap_or(100).clamp(1, 100) as usize;
        let server_row_count = result.results.len();
        if server_row_count > display_row_cap {
            result.results.truncate(display_row_cap);
            if result.total.unwrap_or(0) < server_row_count as i64 {
                result.total = Some(server_row_count as i64);
            }
            let next_offset = resolved_offset.unwrap_or(0).max(0) as usize + display_row_cap;
            output_budget_note = Some(format!(
                "[OUTPUT_BUDGET] Server returned {} result(s); showing the first {}. Paginate with offset={} (limit={}), or narrow the query/file_types.",
                server_row_count, display_row_cap, next_offset, display_row_cap
            ));
        }
        let server_path_count = result.paths.len();
        if server_path_count > display_row_cap {
            result.paths.truncate(display_row_cap);
            let next_offset = resolved_offset.unwrap_or(0).max(0) as usize + display_row_cap;
            output_budget_note = Some(match output_budget_note.take() {
                Some(existing) => format!(
                    "{} Path list also truncated to {}.",
                    existing, display_row_cap
                ),
                None => format!(
                    "[OUTPUT_BUDGET] Server returned {} path(s); showing the first {}. Paginate with offset={} (limit={}), or narrow the query/file_types.",
                    server_path_count, display_row_cap, next_offset, display_row_cap
                ),
            });
        }

        // Format results matching TypeScript output format
        let mut text = String::new();
        let total = result.total.unwrap_or(result.results.len() as i64);
        let concise_text = concise_tool_text_enabled();
        let index_trust_diagnostics = build_mcp_index_trust_diagnostics(
            &result,
            resolved_project_id,
            local_project_id_for_trust,
            local_git_repository.clone(),
            local_git_branch.clone(),
            local_git_commit_sha.clone(),
            local_indexed_at_known,
            local_git_worktree_dirty,
            index_health.as_ref(),
        );
        let index_trust_warning = index_trust_diagnostics
            .as_ref()
            .and_then(format_index_trust_mismatch);

        // High-priority routing/scope/budget banners always render, even in
        // concise mode — suppressing them is how agents end up trusting
        // results from the wrong project, machine, or an unbounded dump.
        if let Some(warning) = project_routing_warning.as_deref() {
            text.push_str(warning);
            text.push('\n');
        }
        if let Some(note) = index_origin_note.as_deref() {
            text.push_str(note);
            text.push('\n');
        }
        if let Some(warning) = index_trust_warning.as_deref() {
            text.push_str(warning);
            text.push('\n');
        }
        // No-index / misresolved-scope is no longer surfaced as a distrust
        // banner. The honest signal now lives in the structured
        // `scope_reliability` field (usable / scope_invalid / reason), and scope
        // checkout refresh is triggered automatically — so we don't tell the
        // agent to distrust canonical results or switch away from hosted MCP.
        if let Some(note) = output_budget_note.as_deref() {
            text.push_str(note);
            text.push('\n');
        }
        if project_routing_warning.is_some()
            || index_origin_note.is_some()
            || index_trust_warning.is_some()
            || output_budget_note.is_some()
        {
            text.push('\n');
        }

        if mode_auto_selected && !concise_text {
            text.push_str(&format!(
                "Mode auto-selected: `{}`. {}\n",
                requested_mode.as_str(),
                mode_reason
            ));
        }
        if let Some(note) = mode_fallback_note.as_deref() {
            if !concise_text || no_hits {
                text.push_str(&format!("{}\n", note));
            }
        }
        if let Some(note) = local_enrichment_warning.as_deref() {
            text.push_str(&format!("{}\n", note));
        }
        if let Some(note) = hot_paths_hint_note.as_deref() {
            if !concise_text || no_hits {
                text.push_str(&format!("{}\n", note));
            }
        }
        if let Some(note) = hot_path_guardrail_note.as_deref() {
            if !concise_text || no_hits {
                text.push_str(&format!("{}\n", note));
            }
        }
        if let Some(route) = project_map_route.as_ref() {
            text.push_str(&format_project_map_route_hint(route));
        }
        if !concise_text && (mode_auto_selected || mode_fallback_note.is_some()) {
            text.push('\n');
        }

        let health_footer = if let Some(health) = index_health.as_ref() {
            if should_surface_index_health_before_results(health, no_hits, scope_invalid) {
                text.push_str(&format_index_health_block(health, concise_text));
                None
            } else if should_append_index_health_footer(health, no_hits, scope_invalid) {
                Some(format_index_health_footer(health, concise_text))
            } else {
                None
            }
        } else {
            None
        };
        if no_hits && !scope_invalid {
            if let Some(local_probe) = local_path_probe.as_ref() {
                text.push_str(&format!(
                    "Local file path exists: `{}`.\n\n",
                    local_probe.display_path
                ));
            }
        }

        if no_hits {
            if scope_invalid {
                let reason = result.scope_reason.as_deref().unwrap_or("invalid_scope");
                if concise_text {
                    text.push_str(&format!(
                        "Requested search scope is invalid (reason: `{}`).\n",
                        reason
                    ));
                    if let Some(path) = folder_path.as_deref() {
                        text.push_str(&format!("{}\n", hosted_index_refresh_instruction(path)));
                    }
                    text.push('\n');
                } else {
                    text.push_str(&format!(
                        "Requested search scope is invalid (reason: `{}`).\n",
                        reason
                    ));
                    if let Some(remediation) = result.scope_remediation.as_ref() {
                        let requested = remediation
                            .requested_scope
                            .as_deref()
                            .unwrap_or("unknown_scope");
                        let resolved = remediation.resolved_scope.as_deref().unwrap_or("none");
                        text.push_str(&format!(
                            "Requested scope: `{}`. Resolved scope: `{}`.\n",
                            requested, resolved
                        ));
                        if let Some(project_id) = remediation.requested_project_id.as_deref() {
                            text.push_str(&format!("Requested project_id: `{}`.\n", project_id));
                        }
                        if let Some(workspace_id) = remediation.requested_workspace_id.as_deref() {
                            text.push_str(&format!(
                                "Requested workspace_id: `{}`.\n",
                                workspace_id
                            ));
                        }
                    }
                    if let Some(path) = folder_path.as_deref() {
                        text.push_str(&format!(
                            "\nIf scope metadata may be stale, {}",
                            hosted_index_refresh_instruction(path)
                        ));
                    }
                }
            } else if let Some((docs, fallback_project_id)) = docs_fallback.as_ref() {
                let scope = fallback_project_id
                    .map(|id| format!(" (project_id: {})", id))
                    .unwrap_or_default();
                text.push_str(&format!(
                    "No codebase results found. This query looks like a docs lookup; found {} docs in ContextStream memory{}:\n\n",
                    docs.len(),
                    scope
                ));

                for (i, doc) in docs.iter().take(10).enumerate() {
                    let title = doc
                        .get("title")
                        .or_else(|| doc.get("name"))
                        .or_else(|| doc.get("summary"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("Untitled");
                    let id = doc.get("id").and_then(|v| v.as_str()).unwrap_or("unknown");
                    let doc_type = doc
                        .get("doc_type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("doc");
                    text.push_str(&format!(
                        "{}. **{}** (id: {}) [{}]\n",
                        i + 1,
                        title,
                        id,
                        doc_type
                    ));
                }

                text.push_str(
                    "\nUse memory(action=\"get_doc\", doc_id=\"...\") to open a specific doc or memory(action=\"list_docs\") to browse more.\n",
                );
            } else if !matched_skills.is_empty() {
                text.push_str(&format!(
                    "No codebase results, but found {} matching skill(s):\n\n",
                    matched_skills.len()
                ));
                for (i, skill) in matched_skills.iter().take(5).enumerate() {
                    let name = skill.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                    let title = skill.get("title").and_then(|v| v.as_str()).unwrap_or(name);
                    text.push_str(&format!("{}. **{}** ({})\n", i + 1, title, name));
                }
                text.push_str("\nUse skill(action=\"run\", name=\"...\") to execute or skill(action=\"list\") to browse all skills.\n");
            } else {
                // Positive framing: the search worked, it just confirmed no matches exist
                let mode_str = executed_mode.as_str();
                let timing = result
                    .query_time_ms
                    .map(|ms| format!(" in {}ms", ms))
                    .unwrap_or_default();

                text.push_str(&format!(
                    "Indexed codebase searched{}. Matches did not meet the relevance threshold for \"{}\" ({} search). Narrow or adjust your query:\n\n",
                    timing, input.query, mode_str
                ));

                // Suggest alternative modes the AI hasn't tried yet
                match executed_mode {
                    SearchMode::Keyword | SearchMode::Pattern => {
                        text.push_str("  - mode=\"semantic\" — conceptual match, useful when exact text isn't known\n");
                    }
                    SearchMode::Semantic => {
                        text.push_str("  - mode=\"keyword\" — exact text match for specific identifiers or strings\n");
                    }
                    SearchMode::Hybrid => {
                        text.push_str(
                            "  - mode=\"keyword\" — strict text match for exact identifiers\n",
                        );
                        text.push_str("  - mode=\"semantic\" — broader conceptual search\n");
                    }
                    _ => {}
                }
                if executed_mode != SearchMode::Exhaustive {
                    text.push_str("  - mode=\"exhaustive\" — full scan across all indexed files\n");
                }
                text.push_str("  - Rephrase with different terms or fewer words\n");

                if is_doc_lookup_query(&input.query) {
                    text.push_str(
                        "\nThis looks like a docs query. Try memory(action=\"list_docs\") then memory(action=\"get_doc\", doc_id=\"...\").\n",
                    );
                }
            }
        } else {
            let timing = result
                .query_time_ms
                .map(|ms| format!(" in {}ms", ms))
                .unwrap_or_default();
            text.push_str(&format!(
                "\u{1f50d} {} results for \"{}\" ({} search{}):\n\n",
                total,
                input.query,
                executed_mode.as_str(),
                timing
            ));

            // Honest provenance when the server's zero-result recovery found
            // these rows via fast-tier query rewrites instead of the original
            // query — agents can reuse the echoed rewrites directly.
            if result.recovered_via_rewrite == Some(true) {
                let rewrites = result
                    .rewritten_queries
                    .as_deref()
                    .unwrap_or_default()
                    .iter()
                    .map(|query| format!("\"{}\"", query))
                    .collect::<Vec<_>>()
                    .join(", ");
                if rewrites.is_empty() {
                    text.push_str(
                        "0 direct hits for the original query — results recovered via server-side query rewrite.\n\n",
                    );
                } else {
                    text.push_str(&format!(
                        "0 direct hits for the original query — results recovered via server-side rewrites: {}.\n\n",
                        rewrites
                    ));
                }
            }

            if requested_paths_output {
                for (idx, path) in result.paths.iter().enumerate() {
                    text.push_str(&format!("{}. {}\n", idx + 1, path));
                }
            } else if result.results.is_empty() {
                text.push_str(
                    "Server returned compact output (count/paths). Rerun with output_format=\"full\" for detailed result rows.\n\n",
                );
            } else {
                let (code_results, memory_results): (Vec<_>, Vec<_>) = result
                    .results
                    .iter()
                    .partition(|item| !is_memory_result(item));

                // Show content snippets by default so agents can act on results
                // directly without falling back to Grep/Read. Only suppress for
                // explicitly compact output or very large result sets.
                let explicitly_suppressed = input
                    .output_format
                    .as_deref()
                    .map(|f| {
                        f.eq_ignore_ascii_case("paths")
                            || f.eq_ignore_ascii_case("count")
                            || f.eq_ignore_ascii_case("minimal")
                    })
                    .unwrap_or(false);
                let show_content = !explicitly_suppressed
                    && (input.include_content.unwrap_or(true)
                        || input
                            .output_format
                            .as_deref()
                            .map(|f| f.eq_ignore_ascii_case("full"))
                            .unwrap_or(false));

                // Format code results within the hard character budget. The
                // row cap above bounds count; this bounds rendered bytes so
                // pathological rows (large snippets) still cannot blow the
                // harness tool-result limit.
                let output_budget = search_text_output_budget();
                let mut idx = 0;
                let mut omitted_rows = 0usize;
                for item in &code_results {
                    if text.len() >= output_budget {
                        omitted_rows = code_results.len().saturating_sub(idx);
                        break;
                    }
                    idx += 1;
                    text.push_str(&format_result_line(idx, item, show_content));
                }
                if omitted_rows > 0 {
                    let next_offset = resolved_offset.unwrap_or(0).max(0) as usize + idx;
                    text.push_str(&format!(
                        "… +{} more result(s) omitted to stay within the output budget. Paginate with offset={}, or use output_format=\"paths\"/\"minimal\".\n",
                        omitted_rows, next_offset
                    ));
                }

                // Format memory results in a separate section
                if include_memory && !memory_results.is_empty() && text.len() < output_budget {
                    text.push_str("\n--- Memory Context ---\n");
                    for item in &memory_results {
                        if text.len() >= output_budget {
                            break;
                        }
                        idx += 1;
                        text.push_str(&format_result_line(idx, item, show_content));
                    }
                }
            }

            // Verbose mode includes full JSON payload in text for debugging.
            // Concise mode keeps this in structured content only. The dump is
            // skipped when it would exceed the output budget — the same data
            // is always available in structured content.
            if !concise_text && !requested_paths_output {
                if let Ok(json) = serde_json::to_string(&result) {
                    if text.len() + json.len() <= search_text_output_budget() {
                        text.push_str("\n--- Full Results ---\n");
                        text.push_str(&json);
                    } else {
                        text.push_str(
                            "\n--- Full Results omitted (output budget); see structured content ---\n",
                        );
                    }
                }
            }
        }

        // Append graph enrichment if available
        if let Some(ref ctx) = graph_context {
            text.push_str("\n[GRAPH_CONTEXT] Code relationships for these results:\n");
            for entry in &ctx.entries {
                if entry.component_name.ends_with("(deps)") {
                    text.push_str(&format!(
                        "- `{}` depends on: {}\n",
                        entry.component_name,
                        entry.used_by.join(", ")
                    ));
                } else {
                    text.push_str(&format!(
                        "- `{}` is used in: {}\n",
                        entry.component_name,
                        entry.used_by.join(", ")
                    ));
                }
            }
            text.push_str(
                "Use graph(action=\"dependencies|impact|usages|call_path\") for deeper analysis.\n",
            );
        }
        if let Some(footer) = health_footer.as_deref() {
            text.push_str(footer);
        }
        if let Some(note) = refactor_cursor_continuation_note(&result) {
            text.push('\n');
            text.push_str(note);
            text.push('\n');
        }

        // Two-tier scope diagnostics:
        //   * Actionable issues (invalid scope, stale index, remediation
        //     notes) always surface — the user needs to know.
        //   * Routine signals (`fallback_used=true` with a ready index)
        //     only surface when CONTEXTSTREAM_DEBUG is on; otherwise they
        //     read as errors even though the search succeeded.
        // Structured diagnostics remain in the JSON payload for programmatic
        // consumers regardless of which text tier is used.
        if is_debug_enabled() {
            if let Some(diag_text) = scope_diag.to_diagnostic_text() {
                text.push_str(&format!("\n[SCOPE_DIAGNOSTICS] {}\n", diag_text));
            }
        } else if let Some(actionable) = scope_diag.to_actionable_text() {
            text.push_str(&format!("\n[SCOPE] {}\n", actionable));
        }

        let fallback_depth = fallback_stages.len().saturating_sub(1);
        let mut structured = search_response_structured_value(&result);
        if let Some(obj) = structured.as_object_mut() {
            if let Some(diagnostics) = index_trust_diagnostics.as_ref() {
                obj.insert(
                    "index_trust".to_string(),
                    serde_json::to_value(diagnostics).unwrap_or_default(),
                );
            }
            if let Some(warning) = project_routing_warning.as_deref() {
                obj.insert(
                    "project_routing_warning".to_string(),
                    serde_json::json!(warning),
                );
            }
            if let Some(note) = index_origin_note.as_deref() {
                obj.insert("index_origin_warning".to_string(), serde_json::json!(note));
            }
            if let Some(note) = output_budget_note.as_deref() {
                obj.insert("output_budget".to_string(), serde_json::json!(note));
            }
            if let Some(requested_id) = requested_explicit_project_id {
                obj.insert(
                    "requested_explicit_project_id".to_string(),
                    serde_json::json!(requested_id.to_string()),
                );
                obj.insert(
                    "effective_explicit_project_id".to_string(),
                    serde_json::json!(explicit_project_id.map(|id| id.to_string())),
                );
                obj.insert(
                    "explicit_project_autocorrected".to_string(),
                    serde_json::json!(Some(requested_id) != explicit_project_id),
                );
            }
            if resolved_project_id != project_id {
                obj.insert(
                    "original_project_id".to_string(),
                    serde_json::json!(project_id.map(|id| id.to_string())),
                );
            }
            obj.insert(
                "resolved_project_id".to_string(),
                serde_json::json!(resolved_project_id.map(|id| id.to_string())),
            );
            obj.insert(
                "resolution_rank".to_string(),
                serde_json::json!(resolved_candidate_index),
            );
            if let Some(health) = index_health.as_ref() {
                obj.insert(
                    "index_health".to_string(),
                    serde_json::json!({
                        "freshness": health.freshness,
                        "confidence": health.confidence,
                        "age_hours": health.age_hours,
                        "scope_match": health.scope_match,
                        "drift_detected": health.drift_detected,
                        "changed_file_count": health.changed_file_count,
                        "indexed_at": health.indexed_at,
                        "recommendation": health.recommendation,
                    }),
                );
            }
            // Calm, machine-readable scope/usability signal. This is where the
            // honest signals live now that the pre-results distrust banners are
            // suppressed (B1/B2): a structured consumer sees scope mismatch /
            // invalidity / active repair without the prose that steered agents
            // toward `git grep`.
            {
                let (scope_match, drift_detected) = index_health
                    .as_ref()
                    .map(|health| (health.scope_match, health.drift_detected))
                    .unwrap_or((true, false));
                let reason = if scope_invalid {
                    "scope_invalid"
                } else if !scope_match {
                    "scope_mismatch_reindexing"
                } else if drift_detected {
                    "drift"
                } else {
                    "ok"
                };
                obj.insert(
                    "scope_reliability".to_string(),
                    serde_json::json!({
                        // Results are usable unless the backend flagged that no
                        // matching index exists at all.
                        "usable": !scope_invalid,
                        "scope_match": scope_match,
                        "scope_invalid": scope_invalid,
                        "reason": reason,
                        "repair": {
                            "attempted": active_index_repair.attempted,
                            "succeeded": active_index_repair.succeeded,
                            "complete": active_index_repair.complete,
                            "reason": active_index_repair.reason,
                        },
                    }),
                );
            }
            obj.insert(
                "active_index_repair".to_string(),
                serde_json::json!({
                    "attempted": active_index_repair.attempted,
                    "succeeded": active_index_repair.succeeded,
                    "complete": active_index_repair.complete,
                    "reason": active_index_repair.reason,
                    "age_secs_before": active_index_repair.age_secs_before,
                    "changed_file_count": active_index_repair.changed_file_count,
                    "files_indexed": active_index_repair.files_indexed,
                    "elapsed_ms": active_index_repair.elapsed_ms,
                    "timed_out": active_index_repair.timed_out,
                    "error": active_index_repair.error,
                    "post_search_background_reingest": stale_reingest_note,
                }),
            );
            if result.recovered_via_rewrite == Some(true) {
                obj.insert(
                    "rewrite_recovery".to_string(),
                    serde_json::json!({
                        "recovered_via_rewrite": true,
                        "rewritten_queries": result.rewritten_queries.clone(),
                    }),
                );
            }
            if let Some(local_probe) = local_path_probe.as_ref() {
                obj.insert(
                    "local_path_probe".to_string(),
                    serde_json::json!({
                        "display_path": local_probe.display_path,
                        "parent_dir": local_probe.parent_dir,
                    }),
                );
            }
            if !dirty_file_hints.is_empty() {
                obj.insert(
                    "session_dirty_files".to_string(),
                    serde_json::json!({
                        "count": dirty_file_hints.len(),
                        "files": dirty_file_hints
                            .iter()
                            .map(|item| serde_json::json!({
                                "absolute_path": item.absolute_path,
                                "display_path": item.display_path,
                                "modified_at": item.modified_at.map(|ts| ts.to_rfc3339()),
                                "exists": item.exists,
                            }))
                            .collect::<Vec<_>>(),
                    }),
                );
            }
            if let Some(hint) = hot_paths_hint.as_ref() {
                obj.insert(
                    "hot_path_hint".to_string(),
                    serde_json::json!({
                        "active": true,
                        "path_count": hint.entries.len(),
                        "confidence": hint.confidence,
                        "guardrail": "activity_advisory_exact_relevance_authoritative",
                        "fallback_behavior": "if no affinity match, baseline ranking remains unchanged",
                    }),
                );
            } else {
                obj.insert(
                    "hot_path_hint".to_string(),
                    serde_json::json!({
                        "active": false,
                        "fallback_behavior": "baseline ranking only",
                    }),
                );
            }
            if let Some(route) = project_map_route.as_ref() {
                obj.insert("project_map_route".to_string(), serde_json::json!(route));
            }
            if let Some((docs, fallback_project_id)) = docs_fallback {
                let docs_count = docs.len();
                obj.insert(
                    "memory_docs_fallback".to_string(),
                    serde_json::json!({
                        "project_id": fallback_project_id.map(|id| id.to_string()),
                        "docs": docs,
                        "count": docs_count,
                    }),
                );
            }
            obj.insert(
                "memory_inclusion".to_string(),
                serde_json::json!({
                    "enabled": include_memory,
                    "requested_override": input.include_memory,
                    "memory_query_intent": memory_query_intent,
                    "code_query_intent": code_query_intent,
                    "project_scoped": prefers_project_scope,
                    "reason": memory_decision.reason,
                }),
            );
            obj.insert(
                "skills_enrichment".to_string(),
                serde_json::json!({
                    "enabled": skill_query_intent,
                    "query_intent": skill_query_intent,
                    "adaptive_threshold": adaptive_skill_threshold,
                    "matched_count_before_threshold": matched_skills_before_threshold,
                    "matched_count": matched_skills.len(),
                    "reason": skills_enrichment_reason,
                }),
            );
            obj.insert(
                "search_explainability".to_string(),
                serde_json::json!({
                    "requested_mode": requested_mode.as_str(),
                    "executed_mode": executed_mode.as_str(),
                    "mode_reason": mode_reason,
                    "fallback_note": mode_fallback_note,
                    "fallback_stages": fallback_stages.clone(),
                    "fallback_depth": fallback_depth,
                    "broad_fallbacks_enabled": allow_broad_fallbacks,
                    "top_result_confidence_band": score_confidence_band(result.results.first().and_then(|item| item.score)),
                    "top_result_score": result.results.first().and_then(|item| item.score),
                    "memory_reason_code": memory_decision.reason,
                    "skill_threshold": adaptive_skill_threshold,
                    "hybrid_retry_threshold": adaptive_hybrid_retry_threshold(&input.query),
                    "semantic_switch_min_improvement": adaptive_semantic_switch_improvement(&input.query),
                    "token_demotions": token_demotions,
                    "local_enrichment": {
                        "result_count": local_enrichment_count_total,
                        "diagnostic": local_enrichment_diagnostic,
                        "warning": local_enrichment_warning,
                    },
                }),
            );
            if let Some(top) = result.results.first() {
                obj.insert(
                    "why_this_result".to_string(),
                    serde_json::json!({
                        "file_path": top.file_path,
                        "start_line": top.start_line,
                        "score": top.score,
                        "confidence_band": score_confidence_band(top.score),
                        "origin": top.origin,
                    }),
                );
            }
            if !matched_skills.is_empty() {
                obj.insert(
                    "matched_skills".to_string(),
                    serde_json::json!(matched_skills),
                );
                if !no_hits {
                    text.push_str(&format!(
                        "\n\nAlso found {} related skill(s): ",
                        matched_skills.len()
                    ));
                    let names: Vec<&str> = matched_skills
                        .iter()
                        .filter_map(|s| s.get("name").and_then(|v| v.as_str()))
                        .collect();
                    text.push_str(&names.join(", "));
                    text.push_str(". Use skill(action=\"run\", name=\"...\") to execute.");
                }
            }

            // Inject scope diagnostics into structured output (requirement #6)
            if scope_diag.has_issues() {
                obj.insert(
                    "scope_diagnostics".to_string(),
                    serde_json::json!({
                        "scope_valid": scope_diag.scope_valid,
                        "scope_reason": scope_diag.scope_reason,
                        "fallback_used": scope_diag.fallback_used,
                        "fallback_reason": scope_diag.fallback_reason,
                        "project_index_state": scope_diag.project_index_state,
                        "remediation_attempted": scope_diag.remediation_attempted,
                        "remediation_note": scope_diag.remediation_note,
                    }),
                );
            }
        }

        // Cross-search VCS when query has repo/PR/issue signals and workspace is known
        if resolved_include_vcs {
            if let Some(ws_id) = workspace_id {
                let vcs_base = format!("/integrations/workspaces/{}/vcs", ws_id);
                let vcs_search_url = format!(
                    "{}/search?q={}&per_page=10",
                    vcs_base,
                    urlencoding::encode(&input.query)
                );
                if let Ok(Ok(vcs_data)) = tokio::time::timeout(
                    std::time::Duration::from_secs(3),
                    self.client.get::<serde_json::Value>(&vcs_search_url),
                )
                .await
                {
                    {
                        let vcs_items = extract_vcs_search_items(&vcs_data);
                        if !vcs_items.is_empty() {
                            text.push_str(&format!(
                                "\n[VCS_SEARCH] Also found {} result(s) in linked repositories.\n",
                                vcs_items.len()
                            ));
                            for item in vcs_items.iter().take(5) {
                                let title = item
                                    .get("title")
                                    .or_else(|| item.get("path"))
                                    .or_else(|| item.get("name"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("match");
                                let item_type = item
                                    .get("type")
                                    .or_else(|| item.get("object_type"))
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("result");
                                text.push_str(&format!("  [{}] {}\n", item_type, title));
                            }
                            text.push_str(
                                "Use vcs(action=\"search_vcs\"|\"search_code\") for full results.\n",
                            );
                            if let Some(obj) = structured.as_object_mut() {
                                obj.insert(
                                    "vcs_search_results".to_string(),
                                    serde_json::json!({
                                        "count": vcs_items.len(),
                                        "items": vcs_items,
                                    }),
                                );
                            }
                        }
                    }
                }
            }
        }

        // Rollout logging (requirement #11)
        crate::domains::scope::log_mcp_request(
            "search",
            &format!("search_{}", executed_mode.as_str()),
            workspace_id,
            resolved_project_id,
            &input.query,
        );
        crate::domains::scope::log_mcp_response_scope(
            "search",
            result.scope_valid,
            result.fallback_reason.as_deref(),
            result.results.len(),
        );
        let telemetry_outcome = if no_hits { "no_hits" } else { "hits" };
        metrics::counter!(
            "mcp_search_calls_total",
            "requested_mode" => requested_mode.as_str(),
            "executed_mode" => executed_mode.as_str(),
            "outcome" => telemetry_outcome,
        )
        .increment(1);
        metrics::histogram!(
            "mcp_search_fallback_depth",
            "requested_mode" => requested_mode.as_str(),
            "executed_mode" => executed_mode.as_str(),
        )
        .record(fallback_depth as f64);
        metrics::histogram!(
            "mcp_search_latency_ms",
            "requested_mode" => requested_mode.as_str(),
            "executed_mode" => executed_mode.as_str(),
        )
        .record(result.query_time_ms.unwrap_or(0) as f64);
        if local_enrichment_count_total > 0 {
            metrics::counter!(
                "mcp_search_local_enrich_hits_total",
                "requested_mode" => requested_mode.as_str(),
                "executed_mode" => executed_mode.as_str(),
            )
            .increment(local_enrichment_count_total as u64);
        }
        if token_demotions > 0 {
            metrics::counter!(
                "mcp_search_demotions_total",
                "requested_mode" => requested_mode.as_str(),
                "executed_mode" => executed_mode.as_str(),
            )
            .increment(token_demotions as u64);
        }
        if result.recovered_via_rewrite == Some(true) {
            metrics::counter!(
                "mcp_search_rewrite_recovery_total",
                "requested_mode" => requested_mode.as_str(),
                "executed_mode" => executed_mode.as_str(),
            )
            .increment(1);
        }

        attach_code_rerank_learning_request_id(&mut structured, served_learning_request_id);
        let (text, structured) = budget_search_tool_payload(text, structured);

        // Store in warm cache so the next identical non-local call can
        // short-circuit via the [SEARCH_CACHED] path. Cached value is the
        // rendered (text, structured) pair so the repeat call doesn't re-render.
        if use_search_cache && checkout_scope_confirmed {
            put_search_cache(cache_key.clone(), (text.clone(), structured.clone()));
        }

        Ok(apply_search_wire_tokenizer(
            bounded_existing_search_tool_result(ToolResult::with_structured(text, structured)),
            &wire_tokenizer_policy,
        ))
    }

    fn guided_emergency_wire_policy(&self, input: &SearchInput) -> SearchWireTokenizerPolicy {
        let explicit_tokenizer = normalize_search_tokenizer_hint(input.tokenizer.as_deref())
            .ok()
            .flatten();
        let session_id =
            mcp_client::get_task_mcp_session_id().filter(|value| !value.trim().is_empty());
        let cached_model = if explicit_tokenizer.is_none() {
            session_id
                .as_deref()
                .and_then(mcp_session::session_model_cache::lookup)
        } else {
            None
        };
        let effective_tokenizer =
            resolve_search_tokenizer(explicit_tokenizer.as_deref(), cached_model.as_deref());
        let caller_scope = super::atlas_warm_cache::current_caller_cache_scope();
        let canary_key = search_tokenizer_canary_key(
            caller_scope.cache_identity(),
            input.workspace_id.as_deref(),
            input.project_id.as_deref(),
            session_id.as_deref(),
            input,
        );
        let context = crate::wire_tokens::current_wire_response_context();
        let decision = crate::wire_tokens::rollout_decision_for_context(
            effective_tokenizer.as_deref(),
            &canary_key,
            &context,
        );
        context.register_rollout_decision(decision);
        SearchWireTokenizerPolicy { decision, context }
    }

    async fn execute_with_guided_timeout(
        &self,
        input: Value,
        guided_timeout: Duration,
    ) -> Result<ToolResult> {
        // This is deliberately the first operation at the public handler
        // boundary: JSON decoding and every subsequent prework await count.
        let handler_started = Instant::now();
        let input: SearchInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;
        let explicitly_guided = input
            .mode
            .as_deref()
            .map(str::trim)
            .filter(|mode| !mode.is_empty())
            .is_some_and(|mode| SearchMode::from_str(mode) == SearchMode::Guided);

        if !explicitly_guided {
            return self.execute_inner(input, handler_started, None).await;
        }

        let budget = GuidedExecutionBudget::new(handler_started, guided_timeout);
        #[cfg(test)]
        let budget = budget.with_test_delays(
            self.guided_test_prework_delay,
            self.guided_test_finalization_delay,
        );
        let timeout_input = input.clone();
        match tokio::time::timeout_at(
            tokio::time::Instant::from_std(budget.deadline),
            self.execute_inner(input, handler_started, Some(budget)),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => {
                let workspace_id = timeout_input
                    .workspace_id
                    .as_deref()
                    .and_then(|value| Uuid::parse_str(value).ok());
                let project_id = timeout_input
                    .project_id
                    .as_deref()
                    .and_then(|value| Uuid::parse_str(value).ok());
                let timeout_result = guided_deadline_exhausted_result(
                    &timeout_input,
                    workspace_id,
                    project_id,
                    budget,
                );
                let policy = self.guided_emergency_wire_policy(&timeout_input);
                let result =
                    apply_search_wire_tokenizer_with_budget(timeout_result, &policy, Some(budget));
                metrics::counter!(
                    "mcp_guided_search_degraded_total",
                    "reason" => "end_to_end_timeout",
                )
                .increment(1);
                record_guided_total_latency(&result, budget);
                Ok(result)
            }
        }
    }
}

#[async_trait]
impl ToolHandler for SearchTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        self.execute_with_guided_timeout(input, GUIDED_SEARCH_REQUEST_TIMEOUT)
            .await
    }

    fn metadata(&self) -> &ToolMetadata {
        static METADATA: std::sync::OnceLock<ToolMetadata> = std::sync::OnceLock::new();
        METADATA.get_or_init(|| ToolMetadata {
            name: "search".to_string(),
            title: "Search Codebase".to_string(),
            description: "Search the indexed CODEBASE for source code and files. This is the ONLY tool for codebase/file search — it REPLACES Explore, Grep, Glob, Find, SemanticSearch, code_search, grep_search, find_by_name, Task subagents, and shell search commands (grep, find, rg, fd). Do NOT fall back to local tools — this tool handles ALL code search needs with automatic mode escalation and local enrichment built in. FASTER than grep/ripgrep: pre-indexed BM25 returns results in 10-200ms with ranked source-code-first results, line-level precision, context lines, and noise filtering that grep cannot match.\n\n⚠️ NOT for finding docs / runbooks / specs / ADRs / RFCs / decisions / lessons — those live in `memory`, NOT in the code index. If the user says 'find the doc on X', 'our runbook for Y', 'the architecture note', 'why we decided Z' — call `memory(action=\"search\", query=\"…\")` or `memory(action=\"list_docs\", query=\"…\")`, not this tool. Do NOT use memory(search) or session(smart_search) for *code* lookup.\n\nModes: exact text (mode='keyword'), regex/glob patterns (mode='pattern'), semantic/conceptual queries (mode='semantic'), one-call raw-evidence-first navigation (mode='guided', optional intent), all occurrences (mode='exhaustive' — grep replacement with line-level output), symbol refactoring (mode='refactor'), cross-project (mode='team'), deep multi-modal crawl (mode='crawl'), and auto-detect (mode='auto').".to_string(),
            category: ToolCategory::Search,
            annotations: ToolAnnotations::read_only(),
            is_pro: false,
            required_tier: None,
        })
    }

    fn input_schema(&self) -> Value {
        SchemaBuilder::new()
            .description("Search the indexed codebase")
            .string(
                "query",
                "Search query string. Be specific for better results.",
                true,
            )
            .string_enum(
                "mode",
                "Search mode. Omit or use 'auto' for intelligent mode selection. Use 'guided' with optional intent for one-call raw evidence plus bounded navigation. Use 'crawl' for complex exploratory queries requiring parallel cross-index extraction.",
                &[
                    "auto",
                    "hybrid",
                    "semantic",
                    "keyword",
                    "pattern",
                    "exhaustive",
                    "refactor",
                    "team",
                    "crawl",
                    "guided",
                ],
                false,
            )
            .string(
                "intent",
                "Optional task intent for mode='guided' (max 2000 characters). Helps the Navigator explain where to work.",
                false,
            )
            .string(
                "tokenizer",
                "Optional tokenizer encoding for whole-wire budget accounting (for example o200k_base). Alias: encoding.",
                false,
            )
            .uuid("workspace_id", "Workspace ID", false)
            .uuid("project_id", "Project ID", false)
            .integer(
                "limit",
                "Maximum results (defaults to configured client/server search_limit)",
                false,
            )
            .array(
                "file_types",
                "Filter by file types (e.g., ['ts', 'js'])",
                "string",
                false,
            )
            .boolean("include_content", "Include file content in results", false)
            .boolean(
                "include_memory",
                "Include memory/doc matches in search results (defaults to false for project-scoped searches)",
                false,
            )
            .boolean(
                "include_vcs",
                "Also search linked VCS repositories (PRs, issues, code). Auto-detected when query mentions repos/PRs/issues.",
                false,
            )
            .property(
                "code_rerank_learning_opt_in",
                serde_json::json!({
                    "type": "boolean",
                    "description": "Optional reranker-learning consent; output unchanged.",
                    "default": false
                }),
                false,
            )
            .string_enum(
                "output_format",
                "Response format",
                &["full", "paths", "minimal", "count"],
                false,
            )
            .integer(
                "context_lines",
                "Lines of context around matches (like grep -C)",
                false,
            )
            .integer(
                "content_max_chars",
                "Max chars per result content (defaults to configured client/server search_max_chars)",
                false,
            )
            .number(
                "exact_match_boost",
                "Boost factor for exact matches (default: 2.0)",
                false,
            )
            .integer("offset", "Offset for pagination", false)
            .string(
                "cursor",
                "Opaque continuation returned as next_cursor by mode='refactor'. Pass it back unchanged with the same query and scope.",
                false,
            )
            .build()
    }
}

// ============================================================================
// Individual search mode tools (for backward compatibility)
// ============================================================================

/// Semantic search tool.
pub struct SemanticSearchTool {
    client: ContextStreamClient,
    session: Arc<SessionManager>,
    index_keeper: Arc<super::index_keeper::IndexKeeper>,
}

impl SemanticSearchTool {
    pub fn new(
        client: ContextStreamClient,
        session: Arc<SessionManager>,
        index_keeper: Arc<super::index_keeper::IndexKeeper>,
    ) -> Self {
        Self {
            client,
            session,
            index_keeper,
        }
    }
}

#[async_trait]
impl ToolHandler for SemanticSearchTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let mut input: SearchInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;
        input.mode = Some("semantic".to_string());

        // Atlas Search (Fuzzy) only fires from explicit mode="fuzzy"
        // on the unified `search` tool — these delegating tools fix
        // the mode to a server-routed kind, so a no-op atlas layer is
        // sufficient.
        let search_tool = SearchTool::new(
            self.client.clone(),
            self.session.clone(),
            self.index_keeper.clone(),
            mcp_types::atlas_layer::noop_layer(),
        );
        search_tool
            .execute(serde_json::to_value(&input).unwrap())
            .await
    }

    fn metadata(&self) -> &ToolMetadata {
        static METADATA: std::sync::OnceLock<ToolMetadata> = std::sync::OnceLock::new();
        METADATA.get_or_init(|| ToolMetadata {
            name: "search_semantic".to_string(),
            title: "Semantic Search".to_string(),
            description: "Search using semantic similarity. Good for conceptual queries."
                .to_string(),
            category: ToolCategory::Search,
            annotations: ToolAnnotations::read_only(),
            is_pro: false,
            required_tier: None,
        })
    }

    fn input_schema(&self) -> Value {
        SchemaBuilder::new()
            .string("query", "Search query", true)
            .uuid("workspace_id", "Workspace ID", false)
            .uuid("project_id", "Project ID", false)
            .integer("limit", "Maximum results", false)
            .property(
                "code_rerank_learning_opt_in",
                serde_json::json!({
                    "type": "boolean",
                    "description": "Optional reranker-learning consent; output unchanged.",
                    "default": false
                }),
                false,
            )
            .build()
    }
}

/// Hybrid search tool.
pub struct HybridSearchTool {
    client: ContextStreamClient,
    session: Arc<SessionManager>,
    index_keeper: Arc<super::index_keeper::IndexKeeper>,
}

impl HybridSearchTool {
    pub fn new(
        client: ContextStreamClient,
        session: Arc<SessionManager>,
        index_keeper: Arc<super::index_keeper::IndexKeeper>,
    ) -> Self {
        Self {
            client,
            session,
            index_keeper,
        }
    }
}

#[async_trait]
impl ToolHandler for HybridSearchTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let mut input: SearchInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;
        input.mode = Some("hybrid".to_string());

        // Atlas Search (Fuzzy) only fires from explicit mode="fuzzy"
        // on the unified `search` tool — these delegating tools fix
        // the mode to a server-routed kind, so a no-op atlas layer is
        // sufficient.
        let search_tool = SearchTool::new(
            self.client.clone(),
            self.session.clone(),
            self.index_keeper.clone(),
            mcp_types::atlas_layer::noop_layer(),
        );
        search_tool
            .execute(serde_json::to_value(&input).unwrap())
            .await
    }

    fn metadata(&self) -> &ToolMetadata {
        static METADATA: std::sync::OnceLock<ToolMetadata> = std::sync::OnceLock::new();
        METADATA.get_or_init(|| ToolMetadata {
            name: "search_hybrid".to_string(),
            title: "Hybrid Search".to_string(),
            description: "Search using both semantic and keyword matching. Best for most queries."
                .to_string(),
            category: ToolCategory::Search,
            annotations: ToolAnnotations::read_only(),
            is_pro: false,
            required_tier: None,
        })
    }

    fn input_schema(&self) -> Value {
        SchemaBuilder::new()
            .string("query", "Search query", true)
            .uuid("workspace_id", "Workspace ID", false)
            .uuid("project_id", "Project ID", false)
            .integer("limit", "Maximum results", false)
            .property(
                "code_rerank_learning_opt_in",
                serde_json::json!({
                    "type": "boolean",
                    "description": "Optional reranker-learning consent; output unchanged.",
                    "default": false
                }),
                false,
            )
            .build()
    }
}

/// Keyword search tool.
pub struct KeywordSearchTool {
    client: ContextStreamClient,
    session: Arc<SessionManager>,
    index_keeper: Arc<super::index_keeper::IndexKeeper>,
}

impl KeywordSearchTool {
    pub fn new(
        client: ContextStreamClient,
        session: Arc<SessionManager>,
        index_keeper: Arc<super::index_keeper::IndexKeeper>,
    ) -> Self {
        Self {
            client,
            session,
            index_keeper,
        }
    }
}

#[async_trait]
impl ToolHandler for KeywordSearchTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let mut input: SearchInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;
        input.mode = Some("keyword".to_string());

        // Atlas Search (Fuzzy) only fires from explicit mode="fuzzy"
        // on the unified `search` tool — these delegating tools fix
        // the mode to a server-routed kind, so a no-op atlas layer is
        // sufficient.
        let search_tool = SearchTool::new(
            self.client.clone(),
            self.session.clone(),
            self.index_keeper.clone(),
            mcp_types::atlas_layer::noop_layer(),
        );
        search_tool
            .execute(serde_json::to_value(&input).unwrap())
            .await
    }

    fn metadata(&self) -> &ToolMetadata {
        static METADATA: std::sync::OnceLock<ToolMetadata> = std::sync::OnceLock::new();
        METADATA.get_or_init(|| ToolMetadata {
            name: "search_keyword".to_string(),
            title: "Keyword Search".to_string(),
            description: "Search using exact keyword matching. Good for specific terms."
                .to_string(),
            category: ToolCategory::Search,
            annotations: ToolAnnotations::read_only(),
            is_pro: false,
            required_tier: None,
        })
    }

    fn input_schema(&self) -> Value {
        SchemaBuilder::new()
            .string("query", "Search query", true)
            .uuid("workspace_id", "Workspace ID", false)
            .uuid("project_id", "Project ID", false)
            .integer("limit", "Maximum results", false)
            .property(
                "code_rerank_learning_opt_in",
                serde_json::json!({
                    "type": "boolean",
                    "description": "Optional reranker-learning consent; output unchanged.",
                    "default": false
                }),
                false,
            )
            .build()
    }
}

/// Register all search tools.
pub fn register_search_tools(
    registry: &mut crate::registry::ToolRegistry,
    client: ContextStreamClient,
    session: Arc<SessionManager>,
    index_keeper: Arc<super::index_keeper::IndexKeeper>,
) {
    // Snapshot the atlas product layer set on the registry by
    // `mcp-server::server::build_registry`. Local stdio builds never
    // see anything but the no-op layer.
    let atlas_layer = registry.atlas_layer().clone();
    registry.register(
        "search",
        Arc::new(SearchTool::new(
            client.clone(),
            session.clone(),
            index_keeper.clone(),
            atlas_layer,
        )),
    );
    registry.register(
        "search_semantic",
        Arc::new(SemanticSearchTool::new(
            client.clone(),
            session.clone(),
            index_keeper.clone(),
        )),
    );
    registry.register(
        "search_hybrid",
        Arc::new(HybridSearchTool::new(
            client.clone(),
            session.clone(),
            index_keeper.clone(),
        )),
    );
    registry.register(
        "search_keyword",
        Arc::new(KeywordSearchTool::new(
            client.clone(),
            session.clone(),
            index_keeper,
        )),
    );
}

#[cfg(test)]
#[path = "search_tests.rs"]
mod tests;
