//! API response types for ContextStream.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Generic API response wrapper.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiResponse<T> {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiError>,
}

impl<T> ApiResponse<T> {
    pub fn unwrap_data(self) -> Option<T> {
        if self.success {
            self.data
        } else {
            None
        }
    }
}

/// API error structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

/// Workspace model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workspace {
    pub id: Uuid,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Project model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: Uuid,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_count: Option<i64>,
}

/// User/account model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    pub id: Uuid,
    pub email: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub created_at: String,
}

/// Credit balance/plan info.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreditBalance {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub credits: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<PlanInfo>,
}

/// Plan information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanInfo {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_tier: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub features: Option<serde_json::Value>,
}

/// Search result item.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SearchResult {
    #[serde(default)]
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_line: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breadcrumb: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    /// Provenance label for this result. It may be set by the server or by a
    /// client-side overlay. Known compatibility values include `server_index`,
    /// `local_overlay_filesystem`, and `atlas_search_lucene`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

/// Structured details about why a requested scope could not be used.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SearchScopeRemediation {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_workspace_id: Option<String>,
}

/// Compact project/index provenance emitted once per search response.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct SearchIndexTrustEnvelope {
    pub project_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    pub committed_generation: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_machine: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_commit_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_generation_coverage_complete: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_generation_consistent: Option<bool>,
}

/// Server acknowledgement of an opaque, installation-scoped checkout route.
///
/// `matched=false` is authoritative: the project may have a canonical index,
/// but the requested machine/worktree overlay was not resolved.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct CheckoutScopeStatus {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installation_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkout_locator: Option<String>,
    #[serde(default)]
    pub matched: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_generation: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlay_generation: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub indexed_at: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl CheckoutScopeStatus {
    pub fn matches(&self, installation_id: Uuid, checkout_locator: &str) -> bool {
        self.matched
            && self.installation_id == Some(installation_id)
            && self.checkout_locator.as_deref() == Some(checkout_locator)
    }
}

/// Search response.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SearchResponse {
    #[serde(default)]
    pub results: Vec<SearchResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<i64>,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_time_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub has_more: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<i64>,
    /// Opaque keyset continuation for refactor search. Pass this value back
    /// unchanged as the next request's `cursor`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// True when a compact count covers only the verified prefix and the
    /// remaining rows must be fetched with `next_cursor`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count_is_lower_bound: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_index_state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_generation: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_generation_min: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_generation_max: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ingested_at_max: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_trust: Option<SearchIndexTrustEnvelope>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkout_scope: Option<CheckoutScopeStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope_valid: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope_remediation: Option<SearchScopeRemediation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_used: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
    /// Present when the server rewrote a zero-result NL query via the fast
    /// generative tier and the results came from those rewrites.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovered_via_rewrite: Option<bool>,
    /// Rewrite candidates the server tried during zero-result recovery.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rewritten_queries: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_map_route: Option<ProjectAgentMapRouteHint>,
}

impl SearchResponse {
    /// Normalize compact output formats (`paths`, `count`) into a single shape.
    pub fn normalize_compact_formats(&mut self) {
        if self.results.is_empty() && !self.paths.is_empty() {
            self.results = self
                .paths
                .iter()
                .map(|path| SearchResult {
                    id: path.clone(),
                    file_path: Some(path.clone()),
                    location: Some(path.clone()),
                    ..SearchResult::default()
                })
                .collect();
        }

        if self.total.is_none() {
            self.total = self
                .count
                .or({
                    if !self.paths.is_empty() {
                        Some(self.paths.len() as i64)
                    } else {
                        None
                    }
                })
                .or(Some(self.results.len() as i64));
        }
    }

    pub fn scope_is_valid(&self) -> bool {
        self.scope_valid.unwrap_or(true)
    }
}

/// Route hint derived from a prewarmed project map.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectAgentMapRouteHint {
    pub title: String,
    pub reason: String,
    #[serde(default)]
    pub paths: Vec<String>,
    #[serde(default)]
    pub suggested_queries: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generated_at: Option<String>,
    #[serde(default)]
    pub stale: bool,
}

/// Prewarmed project map response.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ProjectAgentMapResponse {
    pub project_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<Uuid>,
    pub status: String,
    #[serde(default)]
    pub stale: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generated_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(default)]
    pub token_estimate: usize,
    #[serde(default)]
    pub summary_md: String,
    #[serde(default)]
    pub structured_json: serde_json::Value,
    #[serde(default)]
    pub source_versions: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub llm_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route_hint: Option<ProjectAgentMapRouteHint>,
}

/// ContextCapsule links to alternate representations.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ContextCapsuleLinks {
    /// Bootstrap prompt Markdown URL. Public-safe for bootstrap_link
    /// shares; team-auth required for other audiences.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_markdown: Option<String>,
    #[serde(rename = "self", skip_serializing_if = "Option::is_none")]
    pub self_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_markdown: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunks: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub share_url: Option<String>,
    /// Dashboard deep-link to the full project file explorer (auth required).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_explorer_url: Option<String>,
    /// Dashboard deep-link to the full knowledge graph (auth required).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub knowledge_graph_url: Option<String>,
    /// Dashboard deep-link to the full code dependency graph (auth required).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code_graph_url: Option<String>,
}

/// ContextCapsule policy metadata.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ContextCapsulePolicy {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audience: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<String>,
    #[serde(default)]
    pub include_personal: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub include_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redaction_level: Option<String>,
    #[serde(default)]
    pub allowed_sections: Vec<String>,
    #[serde(default)]
    pub denied_sections: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_inline_tokens: Option<u32>,
}

/// One section of a ContextCapsule.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ContextCapsuleSection {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inline_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(default)]
    pub chunk_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    /// Timestamp of the youngest item in this section. Renders in the
    /// bootstrap prompt as "freshest 2d ago" (Phase 3 — plan-step-14).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_verified_at: Option<String>,
    /// Count of items past the kind-specific staleness threshold.
    #[serde(default)]
    pub stale_count: usize,
}

/// ContextCapsule top-level response.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ContextCapsuleResponse {
    #[serde(default)]
    pub schema_version: String,
    #[serde(default)]
    pub capsule_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generated_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness: Option<serde_json::Value>,
    #[serde(default)]
    pub policy: ContextCapsulePolicy,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bootstrap: Option<serde_json::Value>,
    #[serde(default)]
    pub sections: Vec<ContextCapsuleSection>,
    #[serde(default)]
    pub links: ContextCapsuleLinks,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redaction_summary: Option<serde_json::Value>,
    /// How the capsule's scope was resolved (e.g. from an explicit
    /// project_id, a folder_path lookup against the local index, or a
    /// fuzzy project_name match server-side). Populated by the MCP
    /// wrapper when it performs local resolution, and by the backend
    /// when it resolves a passed-in project_name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_scope: Option<ContextCapsuleResolvedScope>,
    /// Readiness assessment — how complete and shareable the capsule
    /// looks. Backend-computed at create/refresh time. Drives the
    /// readiness gate that blocks thin capsules from being shared
    /// externally (Phase 3 — plan-step-16).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub readiness: Option<ContextCapsuleReadiness>,
}

/// Readiness assessment for a capsule — surfaces whether it has enough
/// narrative content (decisions, docs, recent activity, LLM overview,
/// etc.) to be a useful handoff artifact.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ContextCapsuleReadiness {
    /// 0.0-1.0 weighted score across the readiness checklist.
    pub score: f32,
    /// `thin` (<0.4), `adequate` (0.4-0.7), `rich` (>=0.7).
    pub label: String,
    /// Section ids that the capsule's purpose expects but the manifest
    /// reported as empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_sections: Vec<String>,
    /// Sections present but with fewer items than the purpose-specific
    /// threshold.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thin_sections: Vec<ContextCapsuleThinSection>,
    /// Concrete next actions the agent can take to lift the score
    /// before sharing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggestions: Vec<String>,
    /// Individual check results — kept compact so a UI can render a
    /// checklist next to the score.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<ContextCapsuleReadinessCheck>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ContextCapsuleThinSection {
    pub id: String,
    pub items: usize,
    pub threshold: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ContextCapsuleReadinessCheck {
    pub id: String,
    pub label: String,
    pub passed: bool,
    pub weight: f32,
}

/// How the capsule's scope was resolved when the caller did not pass an
/// explicit `project_id`. Surfaced in the create response so the agent
/// can confirm at a glance that the right project was picked.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ContextCapsuleResolvedScope {
    /// Resolution path taken. One of:
    ///   - "explicit"      — caller passed project_id directly.
    ///   - "folder_path"   — MCP looked up indexed-projects.json.
    ///   - "project_name"  — backend fuzzy-matched by name.
    ///   - "workspace"     — no project resolution; workspace-scoped capsule.
    #[serde(default)]
    pub resolution_method: String,
    /// 0.0-1.0 score. 1.0 = exact id, ~0.95 = folder_path exact, ~0.8 =
    /// folder_path prefix, ~0.9 = unique name match, lower = fuzzy.
    #[serde(default)]
    pub confidence: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_name: Option<String>,
    /// The input that produced this resolution (folder_path string,
    /// project_name string, or None when explicit).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_from: Option<String>,
}

/// ContextCapsule single-chunk response.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ContextCapsuleChunkResponse {
    #[serde(default)]
    pub capsule_id: String,
    #[serde(default)]
    pub chunk_id: String,
    #[serde(default)]
    pub section_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub section_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redaction_summary: Option<serde_json::Value>,
}

/// Consumer-side acknowledgement of a capsule share: the recipient
/// agent acted on the share, optionally lists which sections they
/// read, and carries a free-text note (Phase 2 — plan-step-11).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ContextCapsuleAckSummary {
    pub acked_at: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sections_read: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<Uuid>,
}

/// ContextCapsule share response.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ContextCapsuleShareResponse {
    pub id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capsule_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audience: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permissions: Option<String>,
    #[serde(default)]
    pub token_prefix: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub share_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_url: Option<String>,
    /// Agent-readable URL: API endpoint that returns the raw JSON/markdown
    /// payload for the share token (e.g. `{api_base}/api/v1/capsules/shares/<token>`).
    /// Distinct from `share_url`, which is the React app shell URL meant for
    /// humans to open in a browser.
    ///
    /// The contextstream API may not yet return this field; the field is
    /// `Option<String>` so deserialization tolerates "missing today, present
    /// tomorrow." Callers should fall back to `api_url` when `agent_url` is
    /// absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_accessed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub single_use: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_uses: Option<i32>,
    #[serde(default)]
    pub use_count: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub consumed_at: Option<String>,
    /// Latest consumer-side ack of this share (plan-step-11). Surfaces
    /// in list_shares so the sender can see who actually opened the
    /// link and which sections they focused on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_ack: Option<ContextCapsuleAckSummary>,
    /// Plaintext unlock key — present ONCE in the mint response when the
    /// share is keyed. Stored only as a hash server-side; never returned
    /// again. Surface it to the user/agent immediately.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unlock_key: Option<String>,
    /// Tier-2 destinations the unlock key grants (when keyed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unlock_destinations: Option<Vec<String>>,
    /// True when this share currently has an active (non-revoked) key.
    #[serde(default)]
    pub unlock_key_active: bool,
}

/// ContextCapsule audit event response.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ContextCapsuleAuditEventResponse {
    pub id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capsule_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub export_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub share_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_id: Option<Uuid>,
    #[serde(default)]
    pub event_kind: String,
    #[serde(default)]
    pub access_scope: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_prefix: Option<String>,
    #[serde(default)]
    pub details: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

/// Memory node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryNode {
    pub id: Uuid,
    #[serde(default)]
    pub node_type: String,
    // API may return `summary` instead of `title`
    #[serde(default, alias = "summary")]
    pub title: String,
    // API may return `details` instead of `content`
    #[serde(skip_serializing_if = "Option::is_none", alias = "details")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Memory event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEvent {
    pub id: Uuid,
    pub event_type: String,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
}

/// Proactive VCS context surfaced when a project is linked to a repository.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct VcsContext {
    /// Linked repos for this workspace (owner/name, provider).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repos: Vec<serde_json::Value>,
    /// Open/recent pull requests across linked repos.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_pulls: Vec<serde_json::Value>,
    /// Recent activity events across linked repos.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_activity: Vec<serde_json::Value>,
    /// Unread notifications for linked repos.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notifications: Vec<serde_json::Value>,
    /// Open/recent issues across linked repos.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_issues: Vec<serde_json::Value>,
}

impl VcsContext {
    pub fn is_empty(&self) -> bool {
        self.repos.is_empty()
            && self.open_pulls.is_empty()
            && self.recent_activity.is_empty()
            && self.notifications.is_empty()
            && self.open_issues.is_empty()
    }
}

/// Candidate project suggested by the API's project-routing classifier.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRoutingCandidate {
    /// Candidate project ID. Optional for forward compatibility with
    /// partial/degraded API responses.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    /// Candidate workspace ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<Uuid>,
    /// Candidate workspace display name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_name: Option<String>,
    /// Candidate project display name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_name: Option<String>,
    /// Candidate local/project path when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Linked repository URL when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository_url: Option<String>,
    /// Match confidence score from the API.
    #[serde(default)]
    pub score: f32,
    /// Human-readable match reasons.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub match_reasons: Vec<String>,
}

/// Project-routing guidance returned by init/context APIs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRoutingContext {
    /// Routing status such as `confirmed`, `uncertain`,
    /// `needs_project_selection`, or `needs_project_setup`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Human-readable reason for the routing status.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Current workspace scope selected by the API.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_workspace_id: Option<Uuid>,
    /// Current project scope selected by the API.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_project_id: Option<Uuid>,
    /// Current project display name selected by the API.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_project_name: Option<String>,
    /// Folder path that informed routing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder_path: Option<String>,
    /// True when the classifier believes the active work moved projects.
    #[serde(default)]
    pub project_switch_signal: bool,
    /// Suggested next action for the agent/user.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_action: Option<String>,
    /// Candidate projects to choose from when routing is ambiguous.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<ProjectRoutingCandidate>,
}

/// Session context response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lessons: Option<Vec<Lesson>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remember_items: Option<Vec<RememberItem>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rules_notice: Option<RulesNotice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_notice: Option<VersionNotice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub semantic_intent: Option<SemanticIntent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pressure: Option<ContextPressure>,
    /// Dynamic instructions returned by context API for this turn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggested_rules: Vec<SuggestedRule>,
    /// Skills matched by the API for the current query/context.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matched_skills: Vec<serde_json::Value>,
    /// Recent decisions surfaced by the API.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recent_decisions: Vec<serde_json::Value>,
    /// Memory nodes (facts, preferences) surfaced by the API.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub memory_nodes: Vec<serde_json::Value>,
    /// LLM suggestions for what context should be surfaced.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub flash_suggestions: Vec<serde_json::Value>,
    /// Proactive VCS context (repos, PRs, activity) when project is linked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vcs_context: Option<VcsContext>,
    /// Project-routing guidance when the API detects ambiguous or changed scope.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_routing: Option<ProjectRoutingContext>,
    /// Active checkout selected for code-bearing context retrieval.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkout_scope: Option<CheckoutScopeStatus>,

    // --- Typed context items (smart surfacing) ---
    /// Typed context items from the TurnBrief assembly pipeline.
    /// When present, these carry relevance scoring and precedence.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<SmartContextItem>,
    /// Assembly manifest (budget accounting, included/dropped counts).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest: Option<ContextManifest>,
    /// Strongly-typed matched skill summaries from the API.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matched_skills_typed: Vec<MatchedSkillSummary>,
    /// Team surfacing envelope (optional, backward-compatible).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub team_context: Option<TeamContextSurface>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub team_recommendations: Vec<TeamRecommendation>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub team_governance: Vec<TeamGovernanceCue>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub team_priority_signals: Vec<TeamPrioritySignal>,
    /// Snapshot insights for transcript continuity.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub snapshot_insights: Vec<serde_json::Value>,
    /// Conversation audit suggestions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conversation_audit: Vec<serde_json::Value>,
    /// Degraded-mode action directive from the server.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action_required: Option<serde_json::Value>,

    /// Catch-all for any additional API fields not explicitly modeled.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl ContextResponse {
    /// Whether the API returned typed context items.
    pub fn has_typed_items(&self) -> bool {
        !self.items.is_empty()
    }

    /// Extract typed items matching a specific kind, sorted by score descending.
    pub fn items_by_kind(&self, kind: ContextItemKind) -> Vec<&SmartContextItem> {
        let mut matched: Vec<&SmartContextItem> =
            self.items.iter().filter(|i| i.kind() == kind).collect();
        matched.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        matched
    }

    /// Extract all typed items sorted by precedence (asc) then score (desc).
    pub fn items_sorted(&self) -> Vec<&SmartContextItem> {
        let mut items: Vec<&SmartContextItem> = self.items.iter().collect();
        items.sort_by(|a, b| {
            a.precedence().cmp(&b.precedence()).then_with(|| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        });
        items
    }

    /// Preference items (PR kind) from typed items, highest precedence first.
    pub fn preference_items(&self) -> Vec<&SmartContextItem> {
        self.items_by_kind(ContextItemKind::Preference)
    }

    /// VCS items (VC kind) from typed items.
    pub fn vcs_items(&self) -> Vec<&SmartContextItem> {
        self.items_by_kind(ContextItemKind::Vcs)
    }

    /// Skill items (SK kind) from typed items.
    pub fn skill_items(&self) -> Vec<&SmartContextItem> {
        self.items_by_kind(ContextItemKind::Skill)
    }

    /// Transcript snapshot items (TN kind) from typed items.
    pub fn transcript_snapshot_items(&self) -> Vec<&SmartContextItem> {
        self.items_by_kind(ContextItemKind::TranscriptSnapshot)
    }

    /// Lesson items (L kind) from typed items, sorted by score descending.
    pub fn lesson_items(&self) -> Vec<&SmartContextItem> {
        self.items_by_kind(ContextItemKind::Lesson)
    }
}

/// Lesson from past sessions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Lesson {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prevention: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
}

/// User preference/remember item.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RememberItem {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub importance: Option<String>,
}

/// AI-suggested rule from pattern detection (Blend Knob Architecture).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuggestedRule {
    pub id: Uuid,
    #[serde(default)]
    pub keywords: Vec<String>,
    pub instruction: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default)]
    pub confidence: f64,
    #[serde(default)]
    pub occurrence_count: i32,
}

/// Rules version notice.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RulesNotice {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    pub latest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub files_checked: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update_command: Option<String>,
}

/// Version update notice.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VersionNotice {
    #[serde(default)]
    pub behind: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upgrade_command: Option<String>,
}

/// Semantic intent classification.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticIntent {
    pub intent_type: String,
    pub risk_level: String,
    #[serde(default)]
    pub confidence: f64,
    #[serde(default)]
    pub decision_detected: bool,
    #[serde(default)]
    pub capture_worthy: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_capture_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggested_capture_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extracted_entities: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
}

/// Context pressure indicator.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextPressure {
    pub level: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold: Option<i64>,
}

// ===========================================================================
// Typed context items (smart surfacing)
// ===========================================================================

/// Precedence level for context items. Mirrors the server-side `Precedence`
/// enum from TurnBrief. Items are packed in this order; ties within a level
/// are broken by score (desc).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "snake_case")]
pub enum Precedence {
    Always = 0,
    Critical = 1,
    High = 2,
    #[default]
    Normal = 3,
    Low = 4,
}

/// Discriminates the source/kind of a context item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextItemKind {
    Rule,
    Lesson,
    Decision,
    Memory,
    KnowledgeNode,
    Code,
    Flash,
    Graph,
    Instruction,
    ToolSuggestion,
    Warning,
    Synthesis,
    Pack,
    SuggestedRule,
    Vcs,
    Preference,
    Skill,
    TranscriptSnapshot,
}

impl ContextItemKind {
    /// Map a legacy single-letter wire code to a `ContextItemKind`.
    pub fn from_legacy_code(code: &str) -> Option<Self> {
        match code {
            "W" | "P" => Some(Self::Rule),
            "L" => Some(Self::Lesson),
            "D" => Some(Self::Decision),
            "M" | "T" => Some(Self::Memory),
            "N" => Some(Self::KnowledgeNode),
            "C" => Some(Self::Code),
            "F" => Some(Self::Flash),
            "G" => Some(Self::Graph),
            "I" => Some(Self::Instruction),
            "TS" => Some(Self::ToolSuggestion),
            "WN" => Some(Self::Warning),
            "SY" => Some(Self::Synthesis),
            "PK" => Some(Self::Pack),
            "SR" => Some(Self::SuggestedRule),
            "VC" => Some(Self::Vcs),
            "PR" => Some(Self::Preference),
            "SK" => Some(Self::Skill),
            "TN" => Some(Self::TranscriptSnapshot),
            _ => None,
        }
    }

    pub fn legacy_code(self) -> &'static str {
        match self {
            Self::Rule => "W",
            Self::Lesson => "L",
            Self::Decision => "D",
            Self::Memory => "M",
            Self::KnowledgeNode => "N",
            Self::Code => "C",
            Self::Flash => "F",
            Self::Graph => "G",
            Self::Instruction => "I",
            Self::ToolSuggestion => "TS",
            Self::Warning => "WN",
            Self::Synthesis => "SY",
            Self::Pack => "PK",
            Self::SuggestedRule => "SR",
            Self::Vcs => "VC",
            Self::Preference => "PR",
            Self::Skill => "SK",
            Self::TranscriptSnapshot => "TN",
        }
    }

    /// Default precedence for this kind when not provided by the server.
    pub fn default_precedence(self) -> Precedence {
        match self {
            Self::Rule => Precedence::Always,
            Self::Lesson => Precedence::Critical,
            Self::Decision | Self::Preference | Self::Skill => Precedence::High,
            Self::Graph => Precedence::Low,
            _ => Precedence::Normal,
        }
    }
}

/// A single typed context item from the API's `items` array.
/// This is the wire format returned by `/context/smart`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmartContextItem {
    /// Legacy single-letter type code (e.g. "VC", "PR", "SK", "TN", "L").
    #[serde(default)]
    pub typ: String,
    /// The rendered text payload.
    #[serde(default)]
    pub value: String,
    /// Relevance score (0.0–1.0).
    #[serde(default)]
    pub score: f32,
    /// Upstream item UUID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_id: Option<Uuid>,
    /// Item subtype label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_type: Option<String>,
}

impl SmartContextItem {
    /// Resolve the typed `ContextItemKind` from the legacy code.
    pub fn kind(&self) -> ContextItemKind {
        ContextItemKind::from_legacy_code(&self.typ).unwrap_or(ContextItemKind::Memory)
    }

    /// Resolve precedence: use the kind's default since the wire format
    /// doesn't carry an explicit precedence field.
    pub fn precedence(&self) -> Precedence {
        self.kind().default_precedence()
    }
}

/// Matched skill summary returned by the API alongside typed items.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchedSkillSummary {
    pub id: Uuid,
    pub name: String,
    pub title: String,
    #[serde(default)]
    pub has_actions: bool,
    #[serde(default)]
    pub priority: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamContextSurface {
    pub mode: String,
    pub workspace_id: Uuid,
    pub workspace_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    pub confidence: f32,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamRecommendation {
    pub title: String,
    pub action: String,
    pub rationale: String,
    pub priority: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamGovernanceCue {
    pub kind: String,
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub visibility: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_user_id: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TeamPrioritySignal {
    pub kind: String,
    pub id: String,
    pub signal: String,
    pub score: f32,
}

/// Assembly manifest describing budget allocation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextManifest {
    #[serde(default)]
    pub budget: usize,
    #[serde(default)]
    pub used: usize,
    #[serde(default)]
    pub included_count: usize,
    #[serde(default)]
    pub dropped_count: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub degraded_subsystems: Vec<String>,
    /// Catch-all for additional manifest fields.
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Rate limit information from headers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RateLimitInfo {
    pub limit: i64,
    pub remaining: i64,
    pub reset: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after: Option<i64>,
}

/// Paginated response wrapper.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaginatedResponse<T> {
    pub items: Vec<T>,
    pub total: i64,
    pub page: i64,
    pub page_size: i64,
    pub total_pages: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn search_response_normalizes_count_format() {
        let mut response: SearchResponse = serde_json::from_value(json!({
            "count": 7,
            "query_time_ms": 3,
            "has_more": true,
            "next_cursor": "refactor:v1:count-page-two",
            "count_is_lower_bound": true
        }))
        .unwrap();

        response.normalize_compact_formats();

        assert_eq!(response.total, Some(7));
        assert!(response.results.is_empty());
        assert_eq!(
            response.next_cursor.as_deref(),
            Some("refactor:v1:count-page-two")
        );
        assert_eq!(response.has_more, Some(true));
        assert_eq!(response.count_is_lower_bound, Some(true));

        let serialized = serde_json::to_value(&response).unwrap();
        assert_eq!(serialized["has_more"], true);
        assert_eq!(serialized["next_cursor"], "refactor:v1:count-page-two");
        assert_eq!(serialized["count_is_lower_bound"], true);

        let legacy: SearchResponse =
            serde_json::from_value(json!({ "count": 7, "query_time_ms": 3 })).unwrap();
        assert!(legacy.next_cursor.is_none());
        assert!(legacy.count_is_lower_bound.is_none());
        let legacy = serde_json::to_value(legacy).unwrap();
        assert!(legacy.get("next_cursor").is_none());
        assert!(legacy.get("count_is_lower_bound").is_none());
    }

    #[test]
    fn search_response_normalizes_paths_format() {
        let mut response: SearchResponse = serde_json::from_value(json!({
            "paths": ["src/main.rs", "src/lib.rs"],
            "query_time_ms": 5,
            "next_cursor": "refactor:v1:paths-page-two"
        }))
        .unwrap();

        response.normalize_compact_formats();

        assert_eq!(response.total, Some(2));
        assert_eq!(response.results.len(), 2);
        assert_eq!(
            response.results[0].file_path.as_deref(),
            Some("src/main.rs")
        );
        assert_eq!(response.results[1].file_path.as_deref(), Some("src/lib.rs"));
        assert_eq!(
            response.next_cursor.as_deref(),
            Some("refactor:v1:paths-page-two")
        );
    }

    #[test]
    fn search_response_parses_scope_reliability_fields() {
        let response: SearchResponse = serde_json::from_value(json!({
            "results": [],
            "scope_valid": false,
            "scope_reason": "project_not_found",
            "scope_remediation": {
                "requested_scope": "project_scope",
                "resolved_scope": "none",
                "reason": "project_not_found"
            }
        }))
        .unwrap();

        assert!(!response.scope_is_valid());
        assert_eq!(response.scope_reason.as_deref(), Some("project_not_found"));
    }

    #[test]
    fn search_response_parses_rewrite_recovery_fields() {
        let response: SearchResponse = serde_json::from_value(json!({
            "results": [],
            "recovered_via_rewrite": true,
            "rewritten_queries": ["upload retry", "retry_upload"]
        }))
        .unwrap();

        assert_eq!(response.recovered_via_rewrite, Some(true));
        assert_eq!(
            response.rewritten_queries.as_deref(),
            Some(["upload retry".to_string(), "retry_upload".to_string()].as_slice())
        );

        // Old backends omit the fields entirely — behavior must match today's.
        let legacy: SearchResponse = serde_json::from_value(json!({
            "results": []
        }))
        .unwrap();
        assert_eq!(legacy.recovered_via_rewrite, None);
        assert_eq!(legacy.rewritten_queries, None);
    }

    #[test]
    fn search_response_round_trips_refactor_cursor_and_accepts_legacy_absence() {
        let cursor = "refactor:v1:opaque-page-two";
        let response: SearchResponse = serde_json::from_value(json!({
            "results": [],
            "next_cursor": cursor
        }))
        .unwrap();
        assert_eq!(response.next_cursor.as_deref(), Some(cursor));

        let serialized = serde_json::to_value(&response).unwrap();
        assert_eq!(serialized["next_cursor"], cursor);

        let legacy: SearchResponse = serde_json::from_value(json!({ "results": [] })).unwrap();
        assert!(legacy.next_cursor.is_none());
        assert!(serde_json::to_value(legacy)
            .unwrap()
            .get("next_cursor")
            .is_none());
    }

    #[test]
    fn search_response_parses_compact_index_trust_envelope() {
        let project_id = Uuid::new_v4();
        let response: SearchResponse = serde_json::from_value(json!({
            "results": [],
            "index_generation": 17,
            "index_trust": {
                "project_id": project_id,
                "repository": "contextstream/mcp",
                "committed_generation": 17,
                "indexed_at": "2026-07-20T12:34:56Z",
                "source_machine": "desktop-a",
                "source_branch": "main",
                "source_commit_sha": "0123456789abcdef",
                "result_generation_coverage_complete": true,
                "result_generation_consistent": true
            }
        }))
        .unwrap();

        let trust = response.index_trust.expect("trust envelope");
        assert_eq!(trust.project_id, project_id);
        assert_eq!(trust.repository.as_deref(), Some("contextstream/mcp"));
        assert_eq!(trust.committed_generation, 17);
        assert_eq!(trust.result_generation_coverage_complete, Some(true));
        assert_eq!(trust.result_generation_consistent, Some(true));
    }

    #[test]
    fn search_response_accepts_minimal_index_trust_without_placeholders() {
        let project_id = Uuid::new_v4();
        let response: SearchResponse = serde_json::from_value(json!({
            "results": [],
            "index_trust": {
                "project_id": project_id,
                "committed_generation": 0
            }
        }))
        .unwrap();

        let trust = response.index_trust.expect("minimal trust envelope");
        assert_eq!(trust.project_id, project_id);
        assert!(trust.repository.is_none());
        assert!(trust.indexed_at.is_none());
        assert!(trust.source_machine.is_none());
        assert!(trust.source_branch.is_none());
        assert!(trust.source_commit_sha.is_none());
        assert!(trust.result_generation_coverage_complete.is_none());
        assert!(trust.result_generation_consistent.is_none());
    }

    #[test]
    fn checkout_scope_requires_exact_installation_locator_and_match() {
        let installation_id = Uuid::new_v4();
        let response: SearchResponse = serde_json::from_value(json!({
            "results": [],
            "checkout_scope": {
                "installation_id": installation_id,
                "checkout_locator": "checkout-locator-v1:opaque",
                "matched": true,
                "canonical_generation": 12,
                "overlay_generation": 4
            }
        }))
        .unwrap();
        let scope = response.checkout_scope.expect("checkout scope");

        assert!(scope.matches(installation_id, "checkout-locator-v1:opaque"));
        assert!(!scope.matches(Uuid::new_v4(), "checkout-locator-v1:opaque"));
        assert!(!scope.matches(installation_id, "checkout-locator-v1:other"));
    }

    // ========================================================================
    // Typed context item tests
    // ========================================================================

    #[test]
    fn smart_context_item_resolves_kind_from_legacy_code() {
        let item = SmartContextItem {
            typ: "VC".to_string(),
            value: "PR #42 open".to_string(),
            score: 0.85,
            item_id: None,
            item_type: None,
        };
        assert_eq!(item.kind(), ContextItemKind::Vcs);
        assert_eq!(item.precedence(), Precedence::Normal);
    }

    #[test]
    fn smart_context_item_preference_has_high_precedence() {
        let item = SmartContextItem {
            typ: "PR".to_string(),
            value: "Use tabs not spaces".to_string(),
            score: 0.95,
            item_id: None,
            item_type: None,
        };
        assert_eq!(item.kind(), ContextItemKind::Preference);
        assert_eq!(item.precedence(), Precedence::High);
    }

    #[test]
    fn smart_context_item_lesson_has_critical_precedence() {
        let item = SmartContextItem {
            typ: "L".to_string(),
            value: "Always run tests before commit".to_string(),
            score: 0.7,
            item_id: None,
            item_type: None,
        };
        assert_eq!(item.kind(), ContextItemKind::Lesson);
        assert_eq!(item.precedence(), Precedence::Critical);
    }

    #[test]
    fn smart_context_item_unknown_type_defaults_to_memory() {
        let item = SmartContextItem {
            typ: "ZZ".to_string(),
            value: "unknown".to_string(),
            score: 0.5,
            item_id: None,
            item_type: None,
        };
        assert_eq!(item.kind(), ContextItemKind::Memory);
        assert_eq!(item.precedence(), Precedence::Normal);
    }

    #[test]
    fn context_item_kind_roundtrips_legacy_code() {
        let kinds = [
            ContextItemKind::Rule,
            ContextItemKind::Lesson,
            ContextItemKind::Decision,
            ContextItemKind::Memory,
            ContextItemKind::Vcs,
            ContextItemKind::Preference,
            ContextItemKind::Skill,
            ContextItemKind::TranscriptSnapshot,
        ];
        for kind in &kinds {
            let code = kind.legacy_code();
            let resolved = ContextItemKind::from_legacy_code(code).unwrap();
            assert_eq!(*kind, resolved, "roundtrip failed for {:?}", kind);
        }
    }

    #[test]
    fn context_response_parses_items_array() {
        let response: ContextResponse = serde_json::from_value(json!({
            "context": "Hello",
            "items": [
                { "typ": "PR", "value": "Use tabs", "score": 0.9 },
                { "typ": "VC", "value": "PR #42", "score": 0.8 },
                { "typ": "L", "value": "Run tests", "score": 0.7 },
                { "typ": "SK", "value": "deploy-skill", "score": 0.6 },
                { "typ": "TN", "value": "Prior session snapshot", "score": 0.5 }
            ]
        }))
        .unwrap();

        assert!(response.has_typed_items());
        assert_eq!(response.items.len(), 5);
        assert_eq!(response.preference_items().len(), 1);
        assert_eq!(response.vcs_items().len(), 1);
        assert_eq!(response.lesson_items().len(), 1);
        assert_eq!(response.skill_items().len(), 1);
        assert_eq!(response.transcript_snapshot_items().len(), 1);
    }

    #[test]
    fn context_response_items_sorted_by_precedence_then_score() {
        let response: ContextResponse = serde_json::from_value(json!({
            "context": "test",
            "items": [
                { "typ": "VC", "value": "low-score vcs", "score": 0.3 },
                { "typ": "PR", "value": "pref high", "score": 0.9 },
                { "typ": "L", "value": "lesson critical", "score": 0.8 },
                { "typ": "PR", "value": "pref low", "score": 0.4 },
            ]
        }))
        .unwrap();

        let sorted = response.items_sorted();
        // Critical (L) comes before High (PR), which comes before Normal (VC)
        assert_eq!(sorted[0].kind(), ContextItemKind::Lesson);
        assert_eq!(sorted[1].kind(), ContextItemKind::Preference);
        assert_eq!(sorted[1].value, "pref high");
        assert_eq!(sorted[2].kind(), ContextItemKind::Preference);
        assert_eq!(sorted[2].value, "pref low");
        assert_eq!(sorted[3].kind(), ContextItemKind::Vcs);
    }

    #[test]
    fn context_response_backward_compat_without_items() {
        let response: ContextResponse = serde_json::from_value(json!({
            "context": "Hello world",
            "lessons": [{ "title": "Test lesson", "severity": "high" }],
            "remember_items": [{ "content": "Use tabs" }]
        }))
        .unwrap();

        assert!(!response.has_typed_items());
        assert_eq!(response.items.len(), 0);
        assert!(response.lessons.is_some());
        assert!(response.remember_items.is_some());
    }

    #[test]
    fn context_response_parses_manifest() {
        let response: ContextResponse = serde_json::from_value(json!({
            "context": "test",
            "manifest": {
                "budget": 4096,
                "used": 2048,
                "included_count": 10,
                "dropped_count": 3,
                "degraded_subsystems": ["mem"]
            }
        }))
        .unwrap();

        let manifest = response.manifest.unwrap();
        assert_eq!(manifest.budget, 4096);
        assert_eq!(manifest.used, 2048);
        assert_eq!(manifest.included_count, 10);
        assert_eq!(manifest.dropped_count, 3);
        assert_eq!(manifest.degraded_subsystems, vec!["mem"]);
    }

    #[test]
    fn context_response_parses_team_surfacing_fields() {
        let response: ContextResponse = serde_json::from_value(json!({
            "context": "team context",
            "instructions": "Run team skills first.",
            "team_context": {
                "mode": "team",
                "workspace_id": "11111111-1111-4111-8111-111111111111",
                "workspace_name": "Engineering",
                "confidence": 0.9,
                "reason": "team scoped matches"
            },
            "team_recommendations": [{
                "title": "Run deploy skill",
                "action": "skill(action=\"run\", name=\"deploy\")",
                "rationale": "high priority",
                "priority": 90
            }],
            "team_governance": [{
                "kind": "skill",
                "id": "abc",
                "scope": "team",
                "visibility": "workspace"
            }],
            "team_priority_signals": [{
                "kind": "skill",
                "id": "abc",
                "signal": "matched_skill_priority",
                "score": 0.9
            }]
        }))
        .unwrap();

        assert_eq!(
            response.instructions.as_deref(),
            Some("Run team skills first.")
        );
        assert_eq!(
            response.team_context.as_ref().map(|ctx| ctx.mode.as_str()),
            Some("team")
        );
        assert_eq!(response.team_recommendations.len(), 1);
        assert_eq!(response.team_governance.len(), 1);
        assert_eq!(response.team_priority_signals.len(), 1);
    }

    #[test]
    fn context_response_parses_project_routing() {
        let response: ContextResponse = serde_json::from_value(json!({
            "context": "[PROJECT_ROUTING] status=uncertain",
            "project_routing": {
                "status": "needs_project_selection",
                "reason": "Folder matched multiple projects",
                "current_workspace_id": "11111111-1111-4111-8111-111111111111",
                "folder_path": "/tmp/workspace/app",
                "project_switch_signal": true,
                "suggested_action": "Choose a candidate before writing memory",
                "candidates": [{
                    "project_id": "22222222-2222-4222-8222-222222222222",
                    "workspace_id": "11111111-1111-4111-8111-111111111111",
                    "workspace_name": "Engineering",
                    "project_name": "app",
                    "path": "/tmp/workspace/app",
                    "score": 0.93,
                    "match_reasons": ["folder path"]
                }]
            }
        }))
        .unwrap();

        let routing = response.project_routing.expect("project routing");
        assert_eq!(routing.status.as_deref(), Some("needs_project_selection"));
        assert!(routing.project_switch_signal);
        assert_eq!(routing.candidates.len(), 1);
        assert_eq!(routing.candidates[0].project_name.as_deref(), Some("app"));
    }

    #[test]
    fn precedence_ordering() {
        assert!(Precedence::Always < Precedence::Critical);
        assert!(Precedence::Critical < Precedence::High);
        assert!(Precedence::High < Precedence::Normal);
        assert!(Precedence::Normal < Precedence::Low);
    }
}
