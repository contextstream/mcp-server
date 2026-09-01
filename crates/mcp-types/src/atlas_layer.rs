//! Compatibility types for the legacy `atlas_remote_layer` wire contract.
//!
//! Public builds always use [`NoopAtlasLayer`]. The traits and serialized types
//! remain so existing clients can read older capability responses while the
//! MongoDB-free acceleration layer handles current hosted products.

use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Identifies a MongoDB Atlas product the remote MCP binary may expose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AtlasProductId {
    Search,
    Vector,
    Stream,
    Triggers,
    Charts,
    Archive,
    Federation,
    Functions,
}

impl AtlasProductId {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::Vector => "vector",
            Self::Stream => "stream",
            Self::Triggers => "triggers",
            Self::Charts => "charts",
            Self::Archive => "archive",
            Self::Federation => "federation",
            Self::Functions => "functions",
        }
    }

    pub const ALL: &'static [AtlasProductId] = &[
        Self::Search,
        Self::Vector,
        Self::Stream,
        Self::Triggers,
        Self::Charts,
        Self::Archive,
        Self::Federation,
        Self::Functions,
    ];
}

/// Per-product health snapshot exposed by the compatibility layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtlasProductHealth {
    pub product: AtlasProductId,
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

impl AtlasProductHealth {
    pub fn not_available(product: AtlasProductId, reason: impl Into<String>) -> Self {
        Self {
            product,
            available: false,
            last_error: Some(reason.into()),
        }
    }

    pub fn available(product: AtlasProductId) -> Self {
        Self {
            product,
            available: true,
            last_error: None,
        }
    }
}

/// Legacy product-layer contract. Public builds use [`NoopAtlasLayer`].
/// Provider accessors default to `None` so compatibility call sites fail closed.
pub trait AtlasProductLayer: Send + Sync {
    /// Whether the layer is **active for the current call**. Returns
    /// `false` for the no-op layer. For the real layer, returns true
    /// only when **both** a usable connection exists ([`Self::has_connection`])
    /// **and** either the pod-level master flag is on **or** the
    /// per-request override (`ConfigOverride.atlas_enabled = Some(true)`)
    /// flips it on for this call. Tool execution paths consult this
    /// to decide whether to run Atlas-backed code or fall through to
    /// degraded markers.
    fn is_enabled(&self) -> bool {
        false
    }

    /// Whether a compatibility provider is configured. Always `false` for the
    /// public no-op implementation.
    fn has_connection(&self) -> bool {
        false
    }

    /// Products this layer claims to support right now. Empty for the
    /// no-op layer; populated by the real layer once startup health
    /// checks have classified each product as reachable.
    fn available_products(&self) -> Vec<AtlasProductId> {
        Vec::new()
    }

    /// Cached health snapshot for a single product. Background refresh
    /// is owned by the implementation. Default returns an unavailable
    /// snapshot so callers can rely on `health(...).available` without
    /// branching on layer enablement.
    fn health(&self, product: AtlasProductId) -> AtlasProductHealth {
        AtlasProductHealth::not_available(product, "atlas product layer disabled")
    }

    /// Atlas Search (Lucene) provider. `Some` only when the Atlas Search
    /// product is wired up by the layer; `None` for the no-op layer and
    /// for tier configurations that gate the product off. Added by task
    /// A2.
    fn search(&self) -> Option<std::sync::Arc<dyn AtlasSearchProvider>> {
        None
    }

    /// Atlas Stream Processing provider. `Some` only when the
    /// Stream product is wired up. Added by the compatibility provider.
    fn stream(&self) -> Option<std::sync::Arc<dyn AtlasStreamProvider>> {
        None
    }

    /// Atlas Vector Search provider. `Some` only when the Vector
    /// product is wired up. Added by the compatibility provider.
    fn vector(&self) -> Option<std::sync::Arc<dyn AtlasVectorProvider>> {
        None
    }

    /// Atlas Triggers (App Services) provider. `Some` only when the
    /// Triggers product is wired up. Added by the compatibility provider.
    fn triggers(&self) -> Option<std::sync::Arc<dyn AtlasTriggersProvider>> {
        None
    }

    /// Atlas Online Archive provider. `Some` only when the Archive
    /// product is wired up. Added by the compatibility provider.
    fn archive(&self) -> Option<std::sync::Arc<dyn AtlasArchiveProvider>> {
        None
    }

    /// Atlas Data Federation / regional warm-cache provider. `Some`
    /// only when the Federation product is wired up. Added by the compatibility provider.
    fn federation(&self) -> Option<std::sync::Arc<dyn AtlasFederationProvider>> {
        None
    }

    /// Atlas Charts provider. `Some` only when the Charts product is
    /// wired up (a Charts dashboard exists in the Atlas org and the
    /// binary has the embed key + chart ID env vars set). Added by
    /// the compatibility provider.
    fn charts(&self) -> Option<std::sync::Arc<dyn AtlasChartsProvider>> {
        None
    }

    /// Atlas Functions (App Services) provider — serverless offload
    /// for expensive batch operations on Atlas-resident data. `Some`
    /// only when the Functions product is wired up. Added by the compatibility provider.
    fn functions(&self) -> Option<std::sync::Arc<dyn AtlasFunctionsProvider>> {
        None
    }
}

// ============================================================================
// Atlas Search (Lucene) — the compatibility provider.
// ============================================================================

/// Which mirrored MongoDB collection(s) to query.
///
/// String forms (via `as_str`) MUST stay in sync with the destination
/// collection names emitted by the server-side projections — see
/// the server-side projection contract and the
/// `collection_names_match_atlas_search_collection_strings` test there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AtlasSearchCollection {
    Transcripts,
    Decisions,
    Lessons,
    Docs,
    /// Q&A questions asked via the MCP qa(action="ask") surface.
    QaQuestions,
    /// GLM 5.1 answers persisted alongside their parent question.
    QaAnswers,
    /// User-stored knowledge base entries (guidance, guardrail, faq,
    /// runbook, caveat) that ground GLM 5.1 answers.
    QaKbItems,
}

impl AtlasSearchCollection {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Transcripts => "transcripts",
            Self::Decisions => "decisions",
            Self::Lessons => "lessons",
            Self::Docs => "docs",
            Self::QaQuestions => "qa_questions",
            Self::QaAnswers => "qa_answers",
            Self::QaKbItems => "qa_kb_items",
        }
    }

    pub const ALL: &'static [AtlasSearchCollection] = &[
        Self::Transcripts,
        Self::Decisions,
        Self::Lessons,
        Self::Docs,
        Self::QaQuestions,
        Self::QaAnswers,
        Self::QaKbItems,
    ];
}

/// Scope for a fuzzy text search call.
#[derive(Debug, Clone)]
pub struct AtlasSearchScope {
    /// Workspace the calling user is authorised for.
    pub workspace_id: uuid::Uuid,
    /// Optional project filter.
    pub project_id: Option<uuid::Uuid>,
    /// Per-caller scope token. When present, the search compound
    /// stage must add a `user_scope` $eq filter so personal-item
    /// rows belonging to other workspace members are never matched.
    /// `None` only for workspace-shared surfaces.
    pub user_scope: Option<String>,
    /// Subset of collections to search; empty = all.
    pub collections: Vec<AtlasSearchCollection>,
    /// Maximum edit distance for the fuzzy operator. Atlas Search caps
    /// this at 2; defaults to 2 here.
    pub max_edits: u8,
}

impl AtlasSearchScope {
    pub fn new(workspace_id: uuid::Uuid) -> Self {
        Self {
            workspace_id,
            project_id: None,
            user_scope: None,
            collections: Vec::new(),
            max_edits: 2,
        }
    }

    pub fn with_collections(mut self, collections: Vec<AtlasSearchCollection>) -> Self {
        self.collections = collections;
        self
    }

    pub fn with_project(mut self, project_id: uuid::Uuid) -> Self {
        self.project_id = Some(project_id);
        self
    }

    pub fn with_user_scope(mut self, user_scope: impl Into<String>) -> Self {
        self.user_scope = Some(user_scope.into());
        self
    }
}

/// One result row from Atlas Search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtlasSearchHit {
    pub id: String,
    pub collection: AtlasSearchCollection,
    pub title: Option<String>,
    /// Highlighted snippet (Atlas Search highlights when configured).
    pub snippet: String,
    pub score: f64,
    /// Optional canonical URL (e.g. dashboard route for transcripts/decisions).
    pub url: Option<String>,
    /// Full-text content if requested (default omitted for response size).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

/// Errors a search provider can return.
#[derive(Debug, thiserror::Error)]
pub enum AtlasSearchError {
    #[error("fuzzy search is not configured for this deployment")]
    NotConfigured,
    #[error("fuzzy search connection failed: {0}")]
    Connection(String),
    #[error("fuzzy search query failed: {0}")]
    Query(String),
    #[error("fuzzy search timed out: {0}")]
    Timeout(String),
}

/// Atlas Search (Lucene) provider — fuzzy/typo-tolerant text search
/// over per-workspace mirrored MongoDB collections. Implementations
/// live in `hosted compatibility provider::search`.
#[async_trait::async_trait]
pub trait AtlasSearchProvider: Send + Sync {
    /// Issue an Atlas Search aggregation pipeline using the `text`
    /// operator with `fuzzy: { maxEdits }` against the in-scope
    /// collections. Returns up to `limit` hits ranked by Atlas score.
    async fn fuzzy_text_search(
        &self,
        query: &str,
        scope: &AtlasSearchScope,
        limit: usize,
    ) -> Result<Vec<AtlasSearchHit>, AtlasSearchError>;
}

// ============================================================================
// Atlas Stream Processing — the compatibility provider.
// ============================================================================

/// Kinds of real-time signals the binary feeds into Atlas Stream
/// Processing. New variants are added as later tasks (A5 triggers,
/// A8 federation) introduce new stream consumers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AtlasStreamEventKind {
    /// An editor / agent reported a local file change. Atlas Stream
    /// Processing pipelines update Atlas Search and Atlas Vector
    /// indices in response.
    FileChanged,
    /// An MCP tool call completed. Used for live "what just happened"
    /// signals and per-workspace tool-usage aggregation.
    ToolCall,
    /// The authoritative Neo4j graph changed. Atlas Stream Processing
    /// consumers use this as a cache invalidation/materialization signal
    /// for regional graph serving collections, not as the graph write log.
    GraphChanged,
}

impl AtlasStreamEventKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::FileChanged => "file_changed",
            Self::ToolCall => "tool_call",
            Self::GraphChanged => "graph_changed",
        }
    }
}

/// One real-time event payload. Inserted into the provider-side
/// `stream_events` collection where Atlas Stream Processing
/// pipelines consume it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtlasStreamEvent {
    pub kind: AtlasStreamEventKind,
    pub workspace_id: uuid::Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<uuid::Uuid>,
    /// Arbitrary structured payload — schema depends on `kind`.
    /// `file_changed` includes `file_path`, `content_hash`,
    /// `event_kind` (created/modified/deleted); `tool_call` includes
    /// `tool_name`, `args_summary`, `outcome`; `graph_changed` includes
    /// `graph_version`, `scope_hash`, `build_tier`, and graph counts.
    pub payload: serde_json::Value,
    /// Wall-clock timestamp when the binary observed the event.
    pub emitted_at: chrono::DateTime<chrono::Utc>,
}

impl AtlasStreamEvent {
    pub fn new(
        kind: AtlasStreamEventKind,
        workspace_id: uuid::Uuid,
        project_id: Option<uuid::Uuid>,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            kind,
            workspace_id,
            project_id,
            payload,
            emitted_at: chrono::Utc::now(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AtlasStreamError {
    #[error("index stream is not configured for this deployment")]
    NotConfigured,
    #[error("index stream insert failed: {0}")]
    Insert(String),
}

/// Provider for emitting events into Atlas Stream Processing.
///
/// Calls are fire-and-forget by intent; concrete implementations may
/// still surface errors so callers can decide whether to log + drop
/// or back-pressure.
#[async_trait::async_trait]
pub trait AtlasStreamProvider: Send + Sync {
    /// Write a single event to the Atlas Stream input collection.
    async fn emit(&self, event: AtlasStreamEvent) -> Result<(), AtlasStreamError>;

    /// Convenience helper — most callers know the kind + payload at
    /// the call site.
    async fn emit_payload(
        &self,
        kind: AtlasStreamEventKind,
        workspace_id: uuid::Uuid,
        project_id: Option<uuid::Uuid>,
        payload: serde_json::Value,
    ) -> Result<(), AtlasStreamError> {
        self.emit(AtlasStreamEvent::new(
            kind,
            workspace_id,
            project_id,
            payload,
        ))
        .await
    }
}

// ============================================================================
// Atlas Vector Search — the compatibility provider.
// ============================================================================

/// Metadata-pre-filter pushed into the `$vectorSearch` stage's
/// `filter` clause. Each field is optional; the provider composes
/// only the non-empty ones so queries without filters return the
/// full k-NN neighbourhood.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AtlasVectorFilter {
    /// Restrict to this git branch (`main`, `develop`, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Path glob / prefix filter (single value — repeat the query to
    /// OR across several paths).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_prefix: Option<String>,
    /// Programming language (`rust`, `typescript`, `python`, …).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Freshness cutoff — return only documents whose `updated_at`
    /// is at or after this timestamp.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_after: Option<chrono::DateTime<chrono::Utc>>,
    /// Restrict to a specific decision (for "what supports decision
    /// X" queries).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision_id: Option<uuid::Uuid>,
}

impl AtlasVectorFilter {
    pub fn is_empty(&self) -> bool {
        self.branch.is_none()
            && self.path_prefix.is_none()
            && self.language.is_none()
            && self.updated_after.is_none()
            && self.decision_id.is_none()
    }
}

/// Scope + query configuration for a vector search call.
#[derive(Debug, Clone)]
pub struct AtlasVectorScope {
    pub workspace_id: uuid::Uuid,
    pub project_id: Option<uuid::Uuid>,
    /// Collection to query — defaults to the Atlas Vector index on
    /// `docs` when unset.
    pub collection: Option<AtlasSearchCollection>,
    pub filter: AtlasVectorFilter,
    /// Number of candidates the server pulls from the HNSW graph
    /// before scoring. Atlas docs recommend `~10 × limit`.
    pub num_candidates: Option<usize>,
}

impl AtlasVectorScope {
    pub fn new(workspace_id: uuid::Uuid) -> Self {
        Self {
            workspace_id,
            project_id: None,
            collection: None,
            filter: AtlasVectorFilter::default(),
            num_candidates: None,
        }
    }
}

/// One ranked hit from an Atlas Vector Search call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtlasVectorHit {
    pub id: String,
    pub collection: AtlasSearchCollection,
    pub title: Option<String>,
    pub snippet: String,
    /// Atlas vector score (cosine similarity, ~0.5–1.0 for relevant hits).
    pub score: f64,
    pub url: Option<String>,
    /// Metadata carried through from the mirrored document (branch,
    /// path, language, updated_at, …) — useful for agent reranking.
    pub metadata: serde_json::Value,
}

/// Embedding payload written into the Atlas Vector index (typically
/// by the Stream pipeline from the compatibility provider as new content arrives).
#[derive(Debug, Clone)]
pub struct AtlasVectorWrite {
    pub id: String,
    pub collection: AtlasSearchCollection,
    pub workspace_id: uuid::Uuid,
    pub project_id: Option<uuid::Uuid>,
    pub vector: Vec<f32>,
    /// Metadata columns used for `$vectorSearch.filter`.
    pub metadata: serde_json::Value,
}

#[derive(Debug, thiserror::Error)]
pub enum AtlasVectorError {
    #[error("vector search is not configured for this deployment")]
    NotConfigured,
    #[error("vector search connection failed: {0}")]
    Connection(String),
    #[error("vector search query failed: {0}")]
    Query(String),
    #[error("vector search timed out: {0}")]
    Timeout(String),
    #[error("query_vector is required but missing")]
    MissingQueryVector,
}

// ============================================================================
// Atlas Data Federation / regional warm-cache — the compatibility provider.
// ============================================================================
//
// Atlas in the remote MCP binary is a **read-through regional cache**
// in front of ContextStream's primary endpoints, not a federated
// retrieval replacement. Stream pipelines (the compatibility provider + compatible additions)
// pre-warm specific Atlas collections per workspace; the binary's
// tool handlers consult the regional warm cache first and fall
// through to the primary server endpoint on miss/stale.
//
// Targeted slow paths (verified against API latency table 2026-04-25):
// - `context()` coding-task: 1428ms p50, 1514ms p95 → context_warm_bundles
// - `memory(list_events)`: 125ms p50, 134ms p95 → memory_events_hot
// - `graph_impact`/`graph_call_path`/`graph_circular_dependencies`/
//   `graph_unused_code`: variable-length Neo4j → subgraph_snapshots
//   (already populated by A5's `refresh-subgraph-snapshot` trigger)

/// Which warm-cache collection a lookup targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AtlasWarmCacheKind {
    /// Pre-computed `context()` bundles, keyed by workspace + intent
    /// bucket + hot-paths hash. Refreshed by `pipeline-context-prewarm`.
    Context,
    /// Rolling 24h tail of memory events per workspace, denormalised
    /// for `memory(list_events)`. Refreshed by
    /// `pipeline-memory-events-hot`.
    MemoryEventsHot,
    /// Per-workspace subgraph snapshot for variable-length Neo4j
    /// queries (impact/call_path/circular_dependencies/unused_code).
    /// Already refreshed by A5's `refresh-subgraph-snapshot` trigger.
    SubgraphSnapshot,
    /// Per-target dependency-graph results for `graph_dependencies`.
    /// Keyed by workspace + project + (target_type, target_id, depth,
    /// transitive). Different cadence + key surface than
    /// SubgraphSnapshot — agents revisit the same target several times
    /// in a single tool-use loop, so a short, target-scoped TTL is
    /// the right shape. No refresh trigger; populated lazily by the
    /// binary's write-back on a primary-call success.
    DependencyResult,
    /// Per-node "related code" traversal results for `graph_related`.
    /// Keyed by workspace + project + node_id + max_depth +
    /// relation_types. Same agentic-burst access pattern as
    /// DependencyResult but with a different return shape (nodes +
    /// edges arrays vs dependencies + reverse_dependencies), so kept
    /// in its own collection for separate observability and to avoid
    /// any chance of shape collision on lookup.
    RelatedNodes,
    /// Lessons surfaced by `session(get_lessons)`, keyed by workspace,
    /// project, and optional query hash. The short TTL supports frequent
    /// lesson-warning reads without retaining stale guidance.
    LessonsWarning,
    /// `session(action="recall", query=…)` ranked-fusion output across
    /// transcripts/snapshots/docs/decisions/lessons. The most
    /// expensive single read in the agent loop (~1 s primary).
    /// P0 #2. Cache key = (workspace, project, hashed_query).
    /// 5 min TTL matches `Context` — recall surfaces transcripts +
    /// decisions, both of which are append-only, so 5 min is a
    /// generous freshness budget.
    Recall,
    /// `session(action="ground", user_message=…)` one-shot prior-work
    /// bundle. Composite of recall + docs + decisions + lessons +
    /// skills + git, all in a single call. P0 #3. Cache the entire
    /// ToolResult so subsequent identical-message calls (idempotent
    /// per turn for the same user_message) serve from cache. Same
    /// 5 min TTL as Recall.
    Ground,
    /// `memory(action="decisions", query=…)` — agent revisits the
    /// same decisions list repeatedly during refactor / planning
    /// loops. P0 #4. Cache key = (workspace, project, hashed_query,
    /// limit). 60 s TTL — decisions ARE captured during a session
    /// (manual / agent capture), so we want fresher reads than the
    /// 5 min Recall window. 60 s closes the latency gap without
    /// serving wildly stale results.
    DecisionsHot,
    /// `memory(action="list_nodes", node_type="preference"|"constraint")`
    /// — pulled to inject `[PREFERENCE]` blocks into the system
    /// prompt every turn. P0 #5. Cache key = (workspace, project,
    /// node_type). 5 min TTL — preferences/constraints are
    /// deliberately captured (not on every operation), so they're
    /// stable. 5 min hit rate is high, freshness is fine.
    PreferencesHot,
    /// `memory(action="list_tasks")` — agent polls open tasks
    /// frequently during planning loops. P1 #6. 30 s TTL — task
    /// state changes during a session (created/updated/completed),
    /// so freshness matters more than for preferences.
    MemoryTasksHot,
    /// `memory(action="list_todos")` — same agent polling pattern
    /// as tasks, slightly higher mutation rate (todos are more
    /// fine-grained). P1 #6. 30 s TTL.
    MemoryTodosHot,
    /// `memory(action="list_plans")` — plan listing, lower polling
    /// rate than tasks/todos. P1 #6. 60 s TTL.
    MemoryPlansHot,
    /// `skill(action="list")` — fetched at session start and during
    /// instruction-loading. P1 #7. 5 min TTL — skills are
    /// deliberately authored, low mutation rate.
    SkillsHot,
    /// `memory(action="get_doc", doc_id)` — agents fetch the same
    /// doc multiple times during a planning / refactor loop. P1 #8.
    /// 1 hr TTL — docs change occasionally but mostly stable; large
    /// payload makes the cache hit very valuable.
    DocHot,
    /// `capsule(action="open"|"get", capsule_id)` — capsules are
    /// IMMUTABLE by design (snapshot artifacts). Perfect cache fit:
    /// 24 hr TTL because the only invalidation is capsule deletion
    /// or expiry, which happens rarely. P1 #9. Repeat-open of the
    /// same capsule_id within 24 h serves from cache trivially.
    CapsuleOpen,
    /// `entity(kind="ticket", action="list"|"get")` — Phase 5' entity reads.
    TicketsHot,
    /// `entity(kind="handoff", action="list"|"get")`.
    HandoffsHot,
    /// `entity(kind="incident", action="list"|"get")`.
    IncidentsHot,
}

impl AtlasWarmCacheKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Context => "context",
            Self::MemoryEventsHot => "memory_events_hot",
            Self::SubgraphSnapshot => "subgraph_snapshot",
            Self::DependencyResult => "dependency_result",
            Self::RelatedNodes => "related_nodes",
            Self::LessonsWarning => "lessons_warning",
            Self::Recall => "recall",
            Self::Ground => "ground",
            Self::DecisionsHot => "decisions_hot",
            Self::PreferencesHot => "preferences_hot",
            Self::MemoryTasksHot => "memory_tasks_hot",
            Self::MemoryTodosHot => "memory_todos_hot",
            Self::MemoryPlansHot => "memory_plans_hot",
            Self::SkillsHot => "skills_hot",
            Self::DocHot => "doc_hot",
            Self::CapsuleOpen => "capsule_open",
            Self::TicketsHot => "tickets_hot",
            Self::HandoffsHot => "handoffs_hot",
            Self::IncidentsHot => "incidents_hot",
        }
    }

    /// MongoDB collection name in `mcp_remote` for this cache kind.
    pub fn collection_name(&self) -> &'static str {
        match self {
            Self::Context => "context_warm_bundles",
            Self::MemoryEventsHot => "memory_events_hot",
            Self::SubgraphSnapshot => "subgraph_snapshots",
            Self::DependencyResult => "dependency_results",
            Self::RelatedNodes => "related_nodes",
            Self::LessonsWarning => "lessons_warning_bundles",
            Self::Recall => "recall_bundles",
            Self::Ground => "ground_bundles",
            Self::DecisionsHot => "decisions_hot_bundles",
            Self::PreferencesHot => "preferences_hot_bundles",
            Self::MemoryTasksHot => "memory_tasks_hot_bundles",
            Self::MemoryTodosHot => "memory_todos_hot_bundles",
            Self::MemoryPlansHot => "memory_plans_hot_bundles",
            Self::SkillsHot => "skills_hot_bundles",
            Self::DocHot => "doc_hot_bundles",
            Self::CapsuleOpen => "capsule_open_bundles",
            Self::TicketsHot => "tickets_hot_bundles",
            Self::HandoffsHot => "handoffs_hot_bundles",
            Self::IncidentsHot => "incidents_hot_bundles",
        }
    }

    /// True for cache rows derived from the Neo4j project graph.
    ///
    /// These caches must also consult `graph_dirty_scopes` on read:
    /// their own TTL may still be valid, but a newer Neo4j graph
    /// build makes the cached payload stale immediately.
    pub fn is_graph_derived(&self) -> bool {
        matches!(
            self,
            Self::SubgraphSnapshot | Self::DependencyResult | Self::RelatedNodes
        )
    }

    /// Hard upper bound on cache age before a lookup is treated as
    /// `Stale` and the caller falls through to primary.
    pub fn max_age(&self) -> std::time::Duration {
        match self {
            // Context bundles change quickly when the agent is active;
            // 5 min is the operating point — beyond that, recompute
            // is cheaper than serving stale.
            Self::Context => std::time::Duration::from_secs(5 * 60),
            // memory/list_events fails the 100ms target by ~25-35ms
            // today; a 30s warm cache trivially closes that.
            Self::MemoryEventsHot => std::time::Duration::from_secs(30),
            // Subgraph snapshots are derived from a 15-min refresh
            // trigger — anything beyond 15 min is definitionally
            // stale.
            Self::SubgraphSnapshot => std::time::Duration::from_secs(15 * 60),
            // Dependency results: agentic-burst pattern. Same target
            // gets revisited multiple times within a single tool-use
            // loop (typically <30s of wall-clock). 2 min covers the
            // hot burst window without serving wildly stale data once
            // the agent moves on to a different target.
            Self::DependencyResult => std::time::Duration::from_secs(2 * 60),
            // Related-node traversals: same agentic-burst shape as
            // DependencyResult. Same TTL on purpose so behaviour is
            // uniform across "explore around X" queries.
            Self::RelatedNodes => std::time::Duration::from_secs(2 * 60),
            // Lessons surface every turn but mutation rate is low
            // (lessons captured deliberately, not on every operation).
            // 60 s gives a high hit rate while keeping freshness
            // bounded — newly captured lessons surface within 1 min.
            Self::LessonsWarning => std::time::Duration::from_secs(60),
            // Recall fans out across transcripts/snapshots/docs/
            // decisions; sources are append-only so 5 min is a
            // generous freshness budget.
            Self::Recall => std::time::Duration::from_secs(5 * 60),
            // Ground = recall + docs + decisions + lessons + skills +
            // git. Same source-append-only profile as Recall, same
            // 5 min budget. Idempotent per turn for the same
            // user_message.
            Self::Ground => std::time::Duration::from_secs(5 * 60),
            // Decisions ARE captured during a session (manual / agent
            // capture). 60 s closes latency without serving wildly
            // stale results.
            Self::DecisionsHot => std::time::Duration::from_secs(60),
            // Preferences/constraints captured deliberately; 5 min
            // is fine — newly set preferences surface within 5 min.
            Self::PreferencesHot => std::time::Duration::from_secs(5 * 60),
            // Tasks change during a session (created / status /
            // assignee). 30 s closes the latency gap on the polling
            // pattern without serving stale UI state.
            Self::MemoryTasksHot => std::time::Duration::from_secs(30),
            // Todos: same reasoning as tasks; slightly higher
            // mutation rate (todos are more granular). 30 s.
            Self::MemoryTodosHot => std::time::Duration::from_secs(30),
            // Plans: lower polling rate, lower mutation. 60 s is a
            // reasonable hit-rate / freshness compromise.
            Self::MemoryPlansHot => std::time::Duration::from_secs(60),
            // Skills are deliberately authored; low mutation makes
            // 5 min safe and keeps hit rate high on session-start
            // listing.
            Self::SkillsHot => std::time::Duration::from_secs(5 * 60),
            // Docs change occasionally but agents typically fetch
            // the same doc many times within a planning / refactor
            // loop. Larger payload = larger cache value when hit.
            Self::DocHot => std::time::Duration::from_secs(60 * 60),
            // Capsules are IMMUTABLE by design (snapshot artifacts).
            // Only invalidation is deletion or expiry, both rare.
            // 24 h TTL maximises hit rate on repeat-open.
            Self::CapsuleOpen => std::time::Duration::from_secs(24 * 60 * 60),
            // Entity list/get during active work — same 60s cadence as todos.
            Self::TicketsHot | Self::HandoffsHot | Self::IncidentsHot => {
                std::time::Duration::from_secs(60)
            }
        }
    }
}

/// Scope for a warm-cache lookup. The `scope_hash` is a caller-
/// supplied stable hash of the inputs that should produce the same
/// cached result (e.g. for `context()` it'd hash workspace + intent
/// kind + the hot-paths-hint). The cache pipeline writes documents
/// keyed by this same hash.
#[derive(Debug, Clone)]
pub struct AtlasFederationScope {
    pub workspace_id: uuid::Uuid,
    pub project_id: Option<uuid::Uuid>,
    /// Stable per-call key. Same call inputs MUST produce the same
    /// hash. Different inputs MUST produce a different hash.
    pub scope_hash: String,
    /// Per-caller scope token. When present, every cached row is
    /// keyed AND filtered by this string so a teammate (different
    /// `user_scope`) can never observe another caller's cached row.
    /// Required for any surface where the API response is filtered
    /// server-side by user_id (i.e. mixes workspace + personal
    /// items). `None` is reserved for surfaces that are
    /// workspace-shared by construction (e.g. subgraph snapshots).
    pub user_scope: Option<String>,
}

impl AtlasFederationScope {
    pub fn new(workspace_id: uuid::Uuid, scope_hash: impl Into<String>) -> Self {
        Self {
            workspace_id,
            project_id: None,
            scope_hash: scope_hash.into(),
            user_scope: None,
        }
    }

    pub fn with_project(mut self, project_id: uuid::Uuid) -> Self {
        self.project_id = Some(project_id);
        self
    }

    pub fn with_user_scope(mut self, user_scope: impl Into<String>) -> Self {
        self.user_scope = Some(user_scope.into());
        self
    }
}

/// A successful warm-cache lookup. The caller renders `payload` as
/// the response body and stamps `served_from=regional_warm_cache` +
/// `cache_age_ms` on the structured envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedBundle {
    pub kind: AtlasWarmCacheKind,
    pub workspace_id: uuid::Uuid,
    pub scope_hash: String,
    /// The cached response body — schema depends on `kind`.
    pub payload: serde_json::Value,
    pub warmed_at: chrono::DateTime<chrono::Utc>,
    /// `now() - warmed_at` at lookup time. Provided so callers don't
    /// have to re-read the clock.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub age_ms: Option<u64>,
}

/// One result from `federated_search`. Provenance-tagged.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FederatedHit {
    pub id: String,
    pub source: String, // "primary_semantic" | "atlas_search_lucene" | "atlas_online_archive"
    pub title: Option<String>,
    pub snippet: String,
    pub score: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum AtlasFederationError {
    #[error("regional warm-cache provider is not configured for this deployment")]
    NotConfigured,
    #[error("regional warm-cache connection failed: {0}")]
    Connection(String),
    #[error("regional warm-cache query failed: {0}")]
    Query(String),
    #[error("regional warm-cache timed out: {0}")]
    Timeout(String),
}

/// Atlas regional warm-cache + lightweight federation provider.
///
/// `warm_cache_lookup` + `warm_cache_put` are the read/write
/// primitives used by individual tool handlers to share their slow
/// primary-call results across pods in the region.
/// `federated_search` is a narrow fan-out helper for cross-source
/// queries; **not** a replacement for any retrieval surface.
///
/// # Write-back model
///
/// Handlers do `lookup → on miss, call primary → on success,
/// `warm_cache_put` for the next caller to find`. Pods in the same
/// region (Oregon / Ohio / Frankfurt) share these deposits, so the
/// second pod to ask for the same scope serves it from Atlas in
/// <30ms instead of repeating the primary call. Stream pipelines
/// (the compatibility provider manifests) provide invalidation + signal aggregation;
/// they don't populate the cache directly.
#[async_trait::async_trait]
pub trait AtlasFederationProvider: Send + Sync {
    /// Try to fetch a cached bundle for `(kind, scope)`. Returns
    /// `Ok(None)` on miss/stale/invalidated — caller falls through
    /// to primary. Errors are reserved for true failures (connection
    /// down, etc.) and the caller should also fall through.
    async fn warm_cache_lookup(
        &self,
        kind: AtlasWarmCacheKind,
        scope: &AtlasFederationScope,
    ) -> Result<Option<CachedBundle>, AtlasFederationError>;

    /// Write a primary-call response into the regional warm cache so
    /// subsequent same-scope reads (within the kind's `max_age`) hit
    /// instead of re-running primary. Best-effort — write failures
    /// are logged + counted but do NOT propagate to the caller. The
    /// caller has already returned a successful response; the cache
    /// is purely an optimisation for the next call.
    async fn warm_cache_put(
        &self,
        kind: AtlasWarmCacheKind,
        scope: &AtlasFederationScope,
        payload: serde_json::Value,
    ) -> Result<(), AtlasFederationError>;

    /// Lightweight fan-out across primary semantic + Atlas Search +
    /// Atlas Online Archive. Returns provenance-tagged hits in score
    /// order. Each source is best-effort: a failure on one doesn't
    /// poison the merged result.
    async fn federated_search(
        &self,
        query: &str,
        scope: &AtlasFederationScope,
        limit: usize,
    ) -> Result<Vec<FederatedHit>, AtlasFederationError>;
}

// ============================================================================
// Atlas Charts — the compatibility provider.
// ============================================================================
//
// Atlas Charts hosts dashboards over the same MongoDB Atlas cluster the
// rest of this crate writes to. The remote MCP binary's role is narrow:
// mint a short-lived signed embedding token scoped to the calling
// workspace (so the rendered chart only shows that workspace's data),
// and return both the embed URL (for visual rendering by the MCP
// client's UI surface) and a structured JSON metadata block (so the
// agent can reason about the chart contents without rendering it).
//
// The actual data aggregation, rendering, and Lucene/aggregation work
// happens on Atlas's side. This file declares only the trait surface +
// supporting types; `hosted compatibility provider::charts` mints the JWT and
// implements the trait.

/// Pre-built chart that the binary knows how to render. Each variant
/// maps to a chart UUID configured in MongoDB Atlas Charts; operators
/// supply the UUID via `ATLAS_CHART_ID_<VARIANT>` env vars.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AtlasChartId {
    /// Search-volume timeline by mode (keyword/semantic/hybrid).
    SearchVolumeTimeline,
    /// Credit spend split by Atlas product (search/vector/charts/etc).
    CreditSpendByProduct,
    /// Top-N hottest files (most-edited / most-searched).
    HotFiles,
    /// Decision + lesson density over time.
    DecisionLessonDensity,
    /// Dependency-graph snapshot for an indexed project.
    DependencyGraphSnapshot,
}

impl AtlasChartId {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SearchVolumeTimeline => "search_volume_timeline",
            Self::CreditSpendByProduct => "credit_spend_by_product",
            Self::HotFiles => "hot_files",
            Self::DecisionLessonDensity => "decision_lesson_density",
            Self::DependencyGraphSnapshot => "dependency_graph_snapshot",
        }
    }

    /// All shipped chart IDs in canonical order.
    pub const ALL: &'static [AtlasChartId] = &[
        Self::SearchVolumeTimeline,
        Self::CreditSpendByProduct,
        Self::HotFiles,
        Self::DecisionLessonDensity,
        Self::DependencyGraphSnapshot,
    ];

    /// Parse from the wire-format string returned by `as_str`.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|id| id.as_str().eq_ignore_ascii_case(s))
    }

    /// Short human description (used in tool list output).
    pub fn description(&self) -> &'static str {
        match self {
            Self::SearchVolumeTimeline => {
                "Search-volume timeline by mode (keyword/semantic/hybrid)."
            }
            Self::CreditSpendByProduct => "Credit spend split by Atlas product.",
            Self::HotFiles => "Top-N hottest files (most-edited / most-searched).",
            Self::DecisionLessonDensity => "Decision + lesson density over time.",
            Self::DependencyGraphSnapshot => "Dependency-graph snapshot for the active project.",
        }
    }
}

/// Caller-supplied filter scope for a chart render. The provider
/// always injects `workspace_id` into the signed token so a workspace
/// can only ever see its own data; the rest are passthrough filters
/// the chart's underlying aggregation can react to.
#[derive(Debug, Clone)]
pub struct AtlasChartScope {
    pub workspace_id: uuid::Uuid,
    pub project_id: Option<uuid::Uuid>,
    /// Free-form time-range hint (e.g. `"last_30d"`, `"last_24h"`).
    /// The chart's underlying aggregation interprets this; the binary
    /// just passes it through inside the signed token's filter.
    pub time_range: Option<String>,
}

impl AtlasChartScope {
    pub fn new(workspace_id: uuid::Uuid) -> Self {
        Self {
            workspace_id,
            project_id: None,
            time_range: None,
        }
    }

    pub fn with_project(mut self, project_id: uuid::Uuid) -> Self {
        self.project_id = Some(project_id);
        self
    }

    pub fn with_time_range(mut self, time_range: impl Into<String>) -> Self {
        self.time_range = Some(time_range.into());
        self
    }
}

/// Render result. Contains everything an MCP client needs to render
/// the chart (`embed_url` + `embedding_token`) plus structured
/// metadata the agent can reason over without rendering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtlasChartEmbed {
    /// Logical chart identifier (snake-case, matches `AtlasChartId::as_str`).
    pub chart: String,
    /// provider-side chart UUID (operator-configured via env var).
    pub chart_id: String,
    /// Embed URL the MCP client points an `<iframe>` at. Already
    /// includes the signed token.
    pub embed_url: String,
    /// Bare signed JWT, in case the client wants to render via the
    /// Embedding SDK rather than a raw iframe.
    pub embedding_token: String,
    /// Token expiry as a Unix timestamp.
    pub expires_at: i64,
    /// Filter the token was scoped with (the same one Atlas applies
    /// server-side). Surfaced so the agent can describe what the chart
    /// actually shows.
    pub applied_filter: serde_json::Value,
    /// Short human description of the chart (mirrors `AtlasChartId::description`).
    pub description: String,
}

#[derive(Debug, thiserror::Error)]
pub enum AtlasChartError {
    #[error("workspace charts are not configured for this deployment")]
    NotConfigured,
    #[error("chart: `{0}` is not configured for this deployment")]
    UnknownChart(String, String),
    #[error("chart: failed to sign embedding token: {0}")]
    SignFailed(String),
}

/// Atlas Charts provider — mints a signed embedding token + assembles
/// the embed URL for a pre-built chart configured in the Atlas Charts
/// UI by the operator.
#[async_trait::async_trait]
pub trait AtlasChartsProvider: Send + Sync {
    /// Render the requested chart for the calling scope. Returns the
    /// embed URL + signed token + applied-filter metadata.
    ///
    /// The provider is responsible for injecting `workspace_id` (and
    /// `project_id` if set) into the token's filter so the chart only
    /// shows data for the calling workspace by construction — callers
    /// must not rely on caller-supplied filter manipulation for
    /// security.
    async fn render_chart(
        &self,
        chart: AtlasChartId,
        scope: &AtlasChartScope,
    ) -> Result<AtlasChartEmbed, AtlasChartError>;

    /// Return the subset of charts this deployment has configured. A
    /// chart appears here only when both the provider-side UUID env var
    /// is set and the embed key is configured. Used by the
    /// `atlas_chart(action="list")` action.
    fn configured_charts(&self) -> Vec<AtlasChartId>;
}

// ============================================================================
// Atlas Online Archive — the compatibility provider.
// ============================================================================
//
// Atlas Online Archive moves docs matching a configured rule to S3
// cold storage. The remote MCP binary's `nightly-archive-transcripts`
// trigger (the compatibility provider) flips `archive_status: "archived"` on transcripts
// older than 90 days; the provider-side archive policy then sweeps them
// into the Online Archive S3 tier. Querying archived data uses the
// same MongoDB connection — Atlas presents the archived docs through
// a federated virtual collection, so the binary just runs a normal
// `$search` against the archive-tagged subset.

#[derive(Debug, Clone)]
pub struct AtlasArchiveScope {
    pub workspace_id: uuid::Uuid,
    /// Restrict to a specific source collection (defaults to
    /// transcripts when None).
    pub collection: Option<AtlasSearchCollection>,
    /// Lower bound on `archived_at`. When set, only returns docs
    /// archived on or after this timestamp.
    pub archived_after: Option<chrono::DateTime<chrono::Utc>>,
}

impl AtlasArchiveScope {
    pub fn new(workspace_id: uuid::Uuid) -> Self {
        Self {
            workspace_id,
            collection: None,
            archived_after: None,
        }
    }
}

/// One archived doc surfaced by `search_archive`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtlasArchiveHit {
    pub id: String,
    pub collection: AtlasSearchCollection,
    pub title: Option<String>,
    pub snippet: String,
    /// When the doc was flipped into archived status (set by the
    /// nightly-archive trigger A5).
    pub archived_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Atlas Search score when a query was supplied; `None` for
    /// list-style "all archived" calls.
    pub score: Option<f64>,
}

#[derive(Debug, thiserror::Error)]
pub enum AtlasArchiveError {
    #[error("cold archive is not configured for this deployment")]
    NotConfigured,
    #[error("cold archive query failed: {0}")]
    Query(String),
    #[error("cold archive timed out: {0}")]
    Timeout(String),
}

/// Atlas Online Archive provider — surfaces docs that the Atlas
/// archive policy has moved to cold storage.
#[async_trait::async_trait]
pub trait AtlasArchiveProvider: Send + Sync {
    /// Search archived docs matching `query` text (Atlas Search
    /// `text` operator), restricted to the workspace + optional
    /// collection. Empty `query` returns the most recently archived
    /// docs in `archived_at` desc order.
    async fn search_archive(
        &self,
        query: &str,
        scope: &AtlasArchiveScope,
        limit: usize,
    ) -> Result<Vec<AtlasArchiveHit>, AtlasArchiveError>;
}

// ============================================================================
// Atlas Triggers (App Services) — the compatibility provider.
// ============================================================================

/// Trigger flavour — matches Atlas App Services trigger types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AtlasTriggerKind {
    /// Fires on insert/update/delete on a watched MongoDB collection.
    Database,
    /// Fires on a cron-style schedule.
    Scheduled,
    /// Fires when a registered Atlas authentication event happens.
    Auth,
}

impl AtlasTriggerKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Database => "database",
            Self::Scheduled => "scheduled",
            Self::Auth => "auth",
        }
    }
}

/// Compact descriptor for a trigger the binary expects to be
/// configured on the Atlas App Services side. Operators apply the
/// underlying manifest + JS function body via App Services CLI or the
/// Atlas UI; this descriptor is what the runtime carries so it can
/// surface "expected vs actual" diagnostics + telemetry labels.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtlasTriggerSpec {
    pub name: String,
    pub kind: AtlasTriggerKind,
    /// Short human-readable purpose (one line).
    pub purpose: String,
    /// Collection watched (for `Database` triggers) or empty string
    /// (for `Scheduled` / `Auth`).
    #[serde(default)]
    pub collection: String,
    /// Cron schedule (for `Scheduled` triggers) or empty string.
    #[serde(default)]
    pub schedule: String,
}

#[derive(Debug, thiserror::Error)]
pub enum AtlasTriggersError {
    #[error("scheduled triggers are not configured for this deployment")]
    NotConfigured,
    #[error("scheduled triggers admin call failed: {0}")]
    Admin(String),
}

/// Provider for Atlas Triggers (App Services). The data plane runs
/// entirely on Atlas's side — the binary's role is to declare which
/// triggers are expected, surface health, and emit telemetry when one
/// of those triggers writes back into a collection the binary serves.
///
#[async_trait::async_trait]
pub trait AtlasTriggersProvider: Send + Sync {
    /// Return the static list of triggers this binary expects to be
    /// configured. Used by health probes + the startup manifest log.
    fn expected_triggers(&self) -> Vec<AtlasTriggerSpec>;
}

// ============================================================================
// Atlas Functions (App Services) — the compatibility provider.
// ============================================================================
//
// Atlas Functions / App Services run JavaScript serverside on Atlas's
// infrastructure. The remote MCP binary uses them to offload expensive
// batch operations on **Atlas-resident** data (transcripts / decisions
// / lessons / docs mirrored via CDC, plus Online Archive). We
// **deliberately do not** route work that is already handled by the
// ContextStream server's Voyage Large 4 + Qdrant pipeline (semantic
// search, rerank) or its Neo4j graph (PageRank, impact analysis) —
// duplicating those would create the same drift A3 / A5 narrowed away
// from. Scope: Atlas-only data, Atlas-only work.
//
// Lifecycle:
//   1. Binary calls `submit_job(spec)` → inserts a doc in `jobs`
//      collection, returns `{job_id}` immediately (no MongoDB
//      round-trip beyond the insert).
//   2. App Services Database trigger fires on insert; runs the
//      aggregation server-side; streams results into `job_results`
//      and updates `progress` on the `jobs` doc as it goes.
//   3. Binary polls `poll_job(job_id)` → returns status / progress.
//   4. Once status = completed, binary pages results via
//      `fetch_result_page(job_id, cursor, limit)`.
//
// TTL indexes on `jobs` (7 days) + `job_results` (24h after job
// completion) are the operator's responsibility (see scripts/atlas/);
// the binary does not GC.

/// Kind of asynchronous job to submit. The job runner trigger
/// dispatches based on this enum's wire-format string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AtlasJobKind {
    /// Bulk export of mirrored memory rows (transcripts / decisions /
    /// lessons / docs) for download. Replaces synchronous list calls
    /// when N is large enough that MongoDB cursor latency would
    /// dominate the response.
    MemoryExport,
    /// Aggregate stats over Atlas-resident memory (counts by type,
    /// timeline buckets). Replaces synchronous aggregations that
    /// can't be served from the warm cache (A8) because the scope is
    /// novel.
    MemoryAggregate,
}

impl AtlasJobKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::MemoryExport => "memory_export",
            Self::MemoryAggregate => "memory_aggregate",
        }
    }

    pub const ALL: &'static [AtlasJobKind] = &[Self::MemoryExport, Self::MemoryAggregate];

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|k| k.as_str().eq_ignore_ascii_case(s))
    }
}

/// Specification of one job to run. The binary builds this from the
/// caller's MCP tool input and persists it to the `jobs` collection.
#[derive(Debug, Clone)]
pub struct AtlasJobSpec {
    pub kind: AtlasJobKind,
    pub workspace_id: uuid::Uuid,
    pub project_id: Option<uuid::Uuid>,
    /// Source collection for the job (transcripts / decisions /
    /// lessons / docs). Must match an `AtlasSearchCollection`.
    pub collection: AtlasSearchCollection,
    /// Free-form MongoDB query filter applied before the job's main
    /// aggregation. Passed through verbatim to the server trigger;
    /// `workspace_id` is always added by the provider so caller
    /// filter cannot leak across workspaces.
    pub filter: serde_json::Value,
    /// Per-kind options (e.g. `{"format": "json", "include_content":
    /// true}` for `memory_export`, `{"bucket": "day"}` for
    /// `memory_aggregate`). Interpreted by the trigger function.
    pub options: serde_json::Value,
}

impl AtlasJobSpec {
    pub fn memory_export(workspace_id: uuid::Uuid, collection: AtlasSearchCollection) -> Self {
        Self {
            kind: AtlasJobKind::MemoryExport,
            workspace_id,
            project_id: None,
            collection,
            filter: serde_json::Value::Null,
            options: serde_json::Value::Null,
        }
    }

    pub fn with_project(mut self, project_id: uuid::Uuid) -> Self {
        self.project_id = Some(project_id);
        self
    }

    pub fn with_filter(mut self, filter: serde_json::Value) -> Self {
        self.filter = filter;
        self
    }

    pub fn with_options(mut self, options: serde_json::Value) -> Self {
        self.options = options;
        self
    }
}

/// Handle returned by `submit_job`. The `job_id` is the only piece a
/// caller needs to keep — `poll_job` and `fetch_result_page` look up
/// state from it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtlasJobHandle {
    pub job_id: String,
    pub kind: AtlasJobKind,
    pub submitted_at: chrono::DateTime<chrono::Utc>,
    /// Optional pre-flight estimate. The trigger overwrites this with
    /// the true count once it's computed; useful for progress UI.
    pub estimated_total: Option<u64>,
}

/// Lifecycle state of a submitted job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AtlasJobStatus {
    /// Inserted but the trigger has not yet picked it up.
    Pending,
    /// Trigger is actively running.
    Running,
    /// Trigger finished successfully; results are ready.
    Completed,
    /// Trigger failed; `error` field on `AtlasJobState` carries the
    /// reason. Results may be partial — caller decides whether to
    /// fetch what was written.
    Failed,
}

impl AtlasJobStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "pending" => Some(Self::Pending),
            "running" => Some(Self::Running),
            "completed" => Some(Self::Completed),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed)
    }
}

/// Snapshot of a job's state read from the `jobs` collection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtlasJobState {
    pub job_id: String,
    pub kind: AtlasJobKind,
    pub status: AtlasJobStatus,
    /// Fraction in `[0.0, 1.0]`, populated by the trigger as it
    /// streams results. `None` while pending.
    pub progress: Option<f64>,
    /// Total record count once known (set by the trigger once the
    /// underlying aggregation has been counted).
    pub record_count: Option<u64>,
    pub submitted_at: chrono::DateTime<chrono::Utc>,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    pub completed_at: Option<chrono::DateTime<chrono::Utc>>,
    /// Failure reason when `status == Failed`.
    pub error: Option<String>,
}

/// One page of results from a completed job. Pagination is by
/// per-result `seq` so callers can resume after a process restart.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtlasJobResultPage {
    pub job_id: String,
    /// `seq` of the first record in this page (inclusive).
    pub seq_start: u64,
    /// Records in `seq` order.
    pub records: Vec<serde_json::Value>,
    /// Whether more pages exist past this one.
    pub has_more: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum AtlasFunctionsError {
    #[error("async jobs are not configured for this deployment")]
    NotConfigured,
    #[error("async job: `{0}` not found (may have been GC'd or never existed)")]
    JobNotFound(String),
    #[error("async job: invalid job spec: {0}")]
    InvalidSpec(String),
    #[error("async job: submission failed: {0}")]
    Submission(String),
    #[error("async job: query failed: {0}")]
    Query(String),
    #[error("async job: timed out: {0}")]
    Timeout(String),
}

/// Atlas Functions / App Services provider — serverless offload of
/// expensive batch operations on Atlas-resident data.
///
/// The provider's job is narrow: insert into `jobs` (the trigger does
/// the actual work), then read `jobs` + `job_results` for status +
/// pagination. It deliberately does NOT route work the ContextStream
/// server's existing Voyage / Qdrant / Neo4j stack already handles.
#[async_trait::async_trait]
pub trait AtlasFunctionsProvider: Send + Sync {
    /// Submit a new job. Returns immediately once the doc is
    /// inserted; the App Services trigger picks it up async. The
    /// returned `job_id` is opaque to the caller — pass it back to
    /// `poll_job` / `fetch_result_page`.
    async fn submit_job(&self, spec: AtlasJobSpec) -> Result<AtlasJobHandle, AtlasFunctionsError>;

    /// Read the current state of a job. Cheap; intended for polling
    /// loops with a small backoff. Returns `JobNotFound` if the doc
    /// has been GC'd or never existed.
    async fn poll_job(&self, job_id: &str) -> Result<AtlasJobState, AtlasFunctionsError>;

    /// Page results for a job. `cursor` is `None` for the first page
    /// or the `seq_start + records.len()` from the previous page.
    /// `limit` caps records per page.
    async fn fetch_result_page(
        &self,
        job_id: &str,
        cursor: Option<u64>,
        limit: usize,
    ) -> Result<AtlasJobResultPage, AtlasFunctionsError>;
}

/// Atlas Vector Search provider.
#[async_trait::async_trait]
pub trait AtlasVectorProvider: Send + Sync {
    /// Run a `$vectorSearch` aggregation with the given query vector
    /// and metadata filters. `limit` caps returned hits;
    /// `scope.num_candidates` controls the HNSW fan-out.
    async fn vector_search(
        &self,
        query_vector: &[f32],
        scope: &AtlasVectorScope,
        limit: usize,
    ) -> Result<Vec<AtlasVectorHit>, AtlasVectorError>;

    /// Upsert a pre-computed embedding. Called by the Stream
    /// pipeline's consumer (A4) as new content arrives, or by
    /// operator-driven backfill tools.
    async fn upsert_embedding(&self, write: AtlasVectorWrite) -> Result<(), AtlasVectorError>;
}

/// Default no-op implementation used by all public builds.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoopAtlasLayer;

impl AtlasProductLayer for NoopAtlasLayer {}

// ============================================================================
// Client-side compatibility handshake mirror.
// ============================================================================
//
// Shape of the legacy `atlas_remote_layer` block in `SessionInitResponse`.
// It remains deserializable so older hosted responses continue to work.

/// Full capability handshake block.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtlasRemoteCapabilities {
    /// Whether the Atlas remote layer is entitled at the caller's plan
    /// tier. Free/starter plans always get `false`; Pro+ get `true`.
    pub enabled: bool,
    /// Per-product availability + minimum-tier hint, in canonical order.
    pub products: Vec<AtlasRemoteProductInfo>,
}

/// Per-product capability entry. Matches the server's
/// `AtlasRemoteProductInfo` shape exactly — keep these in sync.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtlasRemoteProductInfo {
    /// One of the product-id strings (`search`, `vector`, `stream`,
    /// `triggers`, `charts`, `archive`, `federation`, `functions`).
    pub name: String,
    /// Whether the calling workspace's plan entitles this product.
    pub available: bool,
    /// Lowest plan name that includes this product (`"pro"`, `"elite"`).
    pub tier_required: String,
}

impl AtlasRemoteCapabilities {
    /// Look up whether a specific product is declared available by the
    /// server's handshake. Returns `None` when the product isn't
    /// present in the handshake at all (older server, older plan).
    pub fn product_available(&self, product: AtlasProductId) -> Option<bool> {
        if !self.enabled {
            return Some(false);
        }
        self.products
            .iter()
            .find(|p| p.name == product.as_str())
            .map(|p| p.available)
    }

    /// Parse the handshake block out of an arbitrary `serde_json::Value`
    /// (the shape of `session_init`'s response). Returns `None` when
    /// the field is absent or malformed — caller decides how to degrade.
    pub fn from_session_init_value(v: &serde_json::Value) -> Option<Self> {
        v.get("acceleration_layer")
            .or_else(|| v.get("atlas_remote_layer"))
            .and_then(|inner| serde_json::from_value::<Self>(inner.clone()).ok())
    }
}

#[cfg(test)]
mod acceleration_handshake_tests {
    use super::*;

    #[test]
    fn parses_preferred_acceleration_layer_field() {
        let value = serde_json::json!({
            "acceleration_layer": {
                "enabled": true,
                "products": [
                    {"name": "search", "available": true, "tier_required": "pro"}
                ]
            },
            "atlas_remote_layer": {
                "enabled": false,
                "products": []
            }
        });

        let caps = AtlasRemoteCapabilities::from_session_init_value(&value).unwrap();
        assert!(caps.enabled);
        assert_eq!(caps.product_available(AtlasProductId::Search), Some(true));
    }

    #[test]
    fn falls_back_to_deprecated_atlas_remote_layer_field() {
        let value = serde_json::json!({
            "atlas_remote_layer": {
                "enabled": true,
                "products": [
                    {"name": "archive", "available": false, "tier_required": "elite"}
                ]
            }
        });

        let caps = AtlasRemoteCapabilities::from_session_init_value(&value).unwrap();
        assert!(caps.enabled);
        assert_eq!(caps.product_available(AtlasProductId::Archive), Some(false));
    }
}

/// Convenience type alias for the trait object most callers actually hold.
pub type AtlasLayer = Arc<dyn AtlasProductLayer>;

/// Construct the default no-op layer wrapped in an [`Arc`].
pub fn noop_layer() -> AtlasLayer {
    Arc::new(NoopAtlasLayer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_layer_is_disabled_with_no_products() {
        let layer = noop_layer();
        assert!(!layer.is_enabled());
        assert!(layer.available_products().is_empty());
    }

    #[test]
    fn noop_health_is_unavailable_with_reason() {
        let layer = noop_layer();
        for product in AtlasProductId::ALL {
            let h = layer.health(*product);
            assert_eq!(h.product, *product);
            assert!(!h.available);
            assert!(h.last_error.is_some());
        }
    }

    #[test]
    fn product_id_serializes_snake_case() {
        let s = serde_json::to_string(&AtlasProductId::Vector).unwrap();
        assert_eq!(s, "\"vector\"");
        assert_eq!(AtlasProductId::Search.as_str(), "search");
    }

    #[test]
    fn all_includes_every_product_variant() {
        // Catch silent forgets when adding a new variant.
        let count = AtlasProductId::ALL.len();
        assert_eq!(count, 8);
    }

    #[test]
    fn chart_id_round_trips_via_string() {
        for chart in AtlasChartId::ALL {
            let s = chart.as_str();
            assert_eq!(AtlasChartId::parse(s), Some(*chart));
            assert_eq!(AtlasChartId::parse(&s.to_ascii_uppercase()), Some(*chart));
        }
        assert!(AtlasChartId::parse("not_a_chart").is_none());
    }

    #[test]
    fn chart_scope_builders() {
        let ws = uuid::Uuid::new_v4();
        let pj = uuid::Uuid::new_v4();
        let s = AtlasChartScope::new(ws)
            .with_project(pj)
            .with_time_range("last_7d");
        assert_eq!(s.workspace_id, ws);
        assert_eq!(s.project_id, Some(pj));
        assert_eq!(s.time_range.as_deref(), Some("last_7d"));
    }

    #[test]
    fn job_kind_round_trips() {
        for kind in AtlasJobKind::ALL {
            assert_eq!(AtlasJobKind::parse(kind.as_str()), Some(*kind));
            assert_eq!(
                AtlasJobKind::parse(&kind.as_str().to_ascii_uppercase()),
                Some(*kind)
            );
        }
        assert!(AtlasJobKind::parse("does_not_exist").is_none());
    }

    #[test]
    fn job_status_terminal_classification() {
        assert!(!AtlasJobStatus::Pending.is_terminal());
        assert!(!AtlasJobStatus::Running.is_terminal());
        assert!(AtlasJobStatus::Completed.is_terminal());
        assert!(AtlasJobStatus::Failed.is_terminal());
        assert_eq!(
            AtlasJobStatus::parse("running"),
            Some(AtlasJobStatus::Running)
        );
        assert_eq!(
            AtlasJobStatus::parse("FAILED"),
            Some(AtlasJobStatus::Failed)
        );
        assert!(AtlasJobStatus::parse("???").is_none());
    }

    #[test]
    fn job_spec_builders_pin_workspace() {
        let ws = uuid::Uuid::new_v4();
        let pj = uuid::Uuid::new_v4();
        let spec = AtlasJobSpec::memory_export(ws, AtlasSearchCollection::Decisions)
            .with_project(pj)
            .with_filter(serde_json::json!({"updated_at": {"$gte": "2026-01-01"}}))
            .with_options(serde_json::json!({"format": "json"}));
        assert_eq!(spec.workspace_id, ws);
        assert_eq!(spec.project_id, Some(pj));
        assert_eq!(spec.collection, AtlasSearchCollection::Decisions);
        assert_eq!(spec.kind, AtlasJobKind::MemoryExport);
        assert!(spec.filter.is_object());
        assert!(spec.options.is_object());
    }
}
