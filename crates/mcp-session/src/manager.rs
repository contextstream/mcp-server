//! Session manager for tracking session state and context pressure.

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use mcp_client::{
    get_task_mcp_session_id, get_task_session_key, ContextStreamClient, SessionInitParams,
    SessionRefreshHook,
};
use mcp_types::{AccountContextSnapshot, AccountModePreference, ExecutionMode};
use mcp_types::{Config, SessionKey};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::warn;
use uuid::Uuid;

use crate::auto_init::WorkspaceMapping;

fn normalize_match_path(path: &std::path::Path) -> std::path::PathBuf {
    let mut current = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    let mut suffix = Vec::new();

    loop {
        if let Ok(canonical) = std::fs::canonicalize(&current) {
            let mut normalized = canonical;
            for component in suffix.iter().rev() {
                normalized.push(component);
            }
            return normalized;
        }

        let Some(parent) = current.parent() else {
            return path.to_path_buf();
        };
        if parent == current {
            return path.to_path_buf();
        }

        if let Some(name) = current.file_name() {
            suffix.push(name.to_os_string());
        }
        current = parent.to_path_buf();
    }
}

/// Describes a child project detected within a multi-project parent folder.
/// Used by multi-project workspace mode to route operations to the correct project.
#[derive(Debug, Clone)]
pub struct ChildProjectInfo {
    pub project_id: String,
    pub name: String,
    pub path: String,
}

/// Relation type between the active folder and another project in the workspace graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectRelationKind {
    Current,
    Parent,
    Child,
    Sibling,
}

impl ProjectRelationKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Current => "current",
            Self::Parent => "parent",
            Self::Child => "child",
            Self::Sibling => "sibling",
        }
    }
}

/// Unified relationship record for parent/child/sibling/current project routing.
#[derive(Debug, Clone)]
pub struct RelatedProjectInfo {
    pub project_id: String,
    pub name: String,
    pub path: String,
    pub relation: ProjectRelationKind,
}

/// Estimated tokens per conversation turn for pressure calculation.
const TOKENS_PER_TURN_ESTIMATE: i64 = 800;

/// Context pressure levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressureLevel {
    Low,
    Medium,
    High,
    Critical,
}

impl PressureLevel {
    pub fn from_tokens(tokens: i64, threshold: i64) -> Self {
        let ratio = tokens as f64 / threshold as f64;
        if ratio >= 0.9 {
            Self::Critical
        } else if ratio >= 0.75 {
            Self::High
        } else if ratio >= 0.5 {
            Self::Medium
        } else {
            Self::Low
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }
}

/// Session state.
#[derive(Debug, Clone)]
pub struct SessionState {
    /// Session ID
    pub session_id: Option<String>,

    /// Workspace ID for this session
    pub workspace_id: Option<Uuid>,

    /// Project ID for this session
    pub project_id: Option<Uuid>,

    /// Root folder path
    pub folder_path: Option<String>,

    /// Opaque server-issued handle for the immutable grounding base associated
    /// with the current user/workspace/project scope.  The handle is never
    /// persisted across scope changes and remains isolated by `SessionKey`.
    pub grounding_handle: Option<String>,

    /// Session start time
    pub started_at: DateTime<Utc>,

    /// Number of conversation turns
    pub conversation_turns: i64,

    /// Estimated token count
    pub session_tokens: i64,

    /// Whether session has been initialized
    pub initialized: bool,

    /// Last high pressure timestamp
    pub last_high_pressure_at: Option<DateTime<Utc>>,

    /// Token count at last high pressure
    pub last_high_pressure_tokens: Option<i64>,

    /// Whether context was restored post-compaction
    pub context_restored: bool,

    /// Optional per-session transcript capture preference.
    /// - `Some(true)`: persist exchanges by default for this session
    /// - `Some(false)`: do not persist exchanges for this session
    /// - `None`: fall back to process/default policy
    pub transcript_capture_enabled: Option<bool>,

    /// Default search mode setting from configuration
    pub default_search_mode: Option<String>,

    /// Related projects detected for the active folder.
    /// Key is usually folder name and value captures relation type and routing metadata.
    pub project_relations: HashMap<String, RelatedProjectInfo>,

    /// Legacy hosted-provider capabilities from the server's
    /// `SessionInitResponse.atlas_remote_layer` compatibility block. `None`
    /// means the block was omitted or could not be parsed.
    pub atlas_remote_capabilities: Option<mcp_types::atlas_layer::AtlasRemoteCapabilities>,

    /// Persisted team/personal/auto preference for this session bucket.
    pub account_mode_preference: AccountModePreference,

    /// Resolved execution mode after precedence rules.
    pub active_execution_mode: ExecutionMode,

    /// Latest account context from API or fallback.
    pub account_context: Option<AccountContextSnapshot>,

    /// When true, team-specific assumptions were disabled due to account mismatch.
    pub team_context_degraded: bool,

    /// User id from last successful account context refresh (mismatch detection).
    pub last_account_user_id: Option<Uuid>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            session_id: None,
            workspace_id: None,
            project_id: None,
            folder_path: None,
            grounding_handle: None,
            started_at: Utc::now(),
            conversation_turns: 0,
            session_tokens: 0,
            initialized: false,
            last_high_pressure_at: None,
            last_high_pressure_tokens: None,
            context_restored: false,
            transcript_capture_enabled: None,
            default_search_mode: None,
            project_relations: HashMap::new(),
            atlas_remote_capabilities: None,
            account_mode_preference: AccountModePreference::Auto,
            active_execution_mode: ExecutionMode::Personal,
            account_context: None,
            team_context_degraded: false,
            last_account_user_id: None,
        }
    }
}

impl SessionState {
    /// Get total estimated tokens including conversation turns.
    pub fn total_tokens(&self) -> i64 {
        let turn_estimate = self.conversation_turns * TOKENS_PER_TURN_ESTIMATE;
        self.session_tokens + turn_estimate
    }

    /// Mark high context pressure.
    pub fn mark_high_pressure(&mut self) {
        self.mark_high_pressure_with_tokens(self.total_tokens());
    }

    /// Mark high context pressure using a caller-provided token estimate.
    pub fn mark_high_pressure_with_tokens(&mut self, tokens: i64) {
        self.last_high_pressure_at = Some(Utc::now());
        self.last_high_pressure_tokens = Some(tokens.max(0));
        self.context_restored = false;
    }

    /// Check if we should restore context post-compaction.
    pub fn should_restore_post_compact(&self) -> bool {
        self.should_restore_post_compact_for_tokens(self.total_tokens())
    }

    /// Check if we should restore context against a caller-provided
    /// post-compaction token estimate.
    pub fn should_restore_post_compact_for_tokens(&self, current_tokens: i64) -> bool {
        if self.context_restored {
            return false;
        }

        let Some(high_pressure_at) = self.last_high_pressure_at else {
            return false;
        };

        // Too old (more than 10 minutes)
        let elapsed = Utc::now().signed_duration_since(high_pressure_at);
        if elapsed.num_minutes() > 10 {
            return false;
        }

        let Some(high_tokens) = self.last_high_pressure_tokens else {
            return false;
        };

        let current_tokens = current_tokens.max(0);
        let token_drop = high_tokens - current_tokens;

        // Context drop detected: current tokens low and significant drop
        current_tokens < 10_000 && token_drop > (high_tokens / 2)
    }
}

/// Cached workspace mapping with TTL.
struct CachedMapping {
    mapping: WorkspaceMapping,
    folder_path: String,
    cached_at: DateTime<Utc>,
}

/// TTL for cached workspace mappings (5 minutes).
const WORKSPACE_CACHE_TTL_SECS: i64 = 300;

/// Session manager for tracking session state.
///
/// **Multi-tenant isolation contract.** Every mutable piece of session
/// state is partitioned by [`SessionKey`] — derived from the authenticated
/// subject by the HTTP transport and propagated through tokio task-local
/// storage. A caller reading `state()` / `update_scope()` / any of the
/// other methods below only ever sees its own bucket; two callers sharing
/// the same MCP server process can never observe each other's folder_path,
/// workspace_id, project_id, or cached workspace mapping.
///
/// The HTTP and stdio transports both install an explicit task-local key.
/// Legacy direct callers without one still use the local state bucket for API
/// compatibility, but caller-sensitive cache code independently fails closed
/// when the task-local marker is absent.
#[derive(Clone)]
pub struct SessionManager {
    /// Per-subject session state. The DashMap keeps per-key locks
    /// independent so contention stays local to a single caller.
    states: Arc<DashMap<SessionKey, Arc<RwLock<SessionState>>>>,
    /// Per-subject workspace-mapping cache, same isolation as `states`.
    cached_workspaces: Arc<DashMap<SessionKey, Arc<RwLock<Option<CachedMapping>>>>>,
    client: ContextStreamClient,
    #[allow(dead_code)]
    config: Arc<RwLock<Config>>,
}

impl SessionManager {
    /// Create a new session manager.
    pub fn new(client: ContextStreamClient, config: Config) -> Self {
        Self {
            states: Arc::new(DashMap::new()),
            cached_workspaces: Arc::new(DashMap::new()),
            client,
            config: Arc::new(RwLock::new(config)),
        }
    }

    /// Discard a request-scoped state bucket after a stateless transport call.
    ///
    /// Callers must only pass keys that are unique to one completed request;
    /// persistent stdio and initialize-era HTTP keys intentionally remain in
    /// the manager. Removing both maps prevents modern round-robin traffic
    /// from accumulating hidden session state or an unbounded set of buckets.
    pub fn discard_transient_state(&self, key: &SessionKey) -> bool {
        let removed_state = self.states.remove(key).is_some();
        let removed_workspace = self.cached_workspaces.remove(key).is_some();
        removed_state || removed_workspace
    }

    /// Resolve the session-state bucket for the current request, creating
    /// an empty one if the caller hasn't been seen before. The bucket key
    /// comes from the tokio task-local set by HTTP or stdio. Legacy direct
    /// library callers fall back to `SessionKey::Local`; cache eligibility
    /// never infers stdio from that absence.
    fn state_for_current(&self) -> Arc<RwLock<SessionState>> {
        let key = get_task_session_key().unwrap_or(SessionKey::Local);
        self.states
            .entry(key)
            .or_insert_with(|| Arc::new(RwLock::new(SessionState::default())))
            .clone()
    }

    /// Same shape as [`state_for_current`] for the workspace cache.
    fn cached_workspace_for_current(&self) -> Arc<RwLock<Option<CachedMapping>>> {
        let key = get_task_session_key().unwrap_or(SessionKey::Local);
        self.cached_workspaces
            .entry(key)
            .or_insert_with(|| Arc::new(RwLock::new(None)))
            .clone()
    }

    /// Register the session refresh hook on the client so that 403 workspace
    /// errors automatically trigger a session re-init and retry.
    /// Call this once after creating the SessionManager.
    pub async fn register_session_refresh_hook(self: &Arc<Self>) {
        let session_weak = Arc::downgrade(self);
        let hook: SessionRefreshHook = Arc::new(move || {
            let session_weak = session_weak.clone();
            Box::pin(async move {
                let Some(session) = session_weak.upgrade() else {
                    return false;
                };
                // The refresh hook fires on the same task as the original
                // tool call, so the TASK_SESSION_KEY task-local (and
                // TASK_AUTH_OVERRIDE) are already set — we'll end up in
                // the right per-subject bucket below. If we somehow fire
                // outside a request scope we land in the Local bucket,
                // which only contains CLI state and is safe to refresh.
                let state_handle = session.state_for_current();
                let state = state_handle.read().await;
                let workspace_id = state.workspace_id;
                let project_id = state.project_id;
                let folder_path = state.folder_path.clone();
                drop(state);

                if workspace_id.is_none() {
                    warn!("[session-refresh] no workspace_id in session state, cannot refresh");
                    return false;
                }

                let params = SessionInitParams {
                    workspace_id,
                    project_id,
                    repository_url: folder_path.as_deref().and_then(|folder_path| {
                        crate::current_repository_canonical_url(folder_path)
                            .ok()
                            .flatten()
                    }),
                    folder_path,
                    ..Default::default()
                };

                match session.client.session_init_quick(params).await {
                    Ok(result) => {
                        let grounding_handle = result
                            .get("grounding_handle")
                            .and_then(|value| value.as_str())
                            .map(str::to_string);
                        session.set_grounding_handle(grounding_handle).await;
                        // Re-set client defaults from the (possibly updated) state
                        session.client.set_defaults(workspace_id, project_id).await;
                        warn!("[session-refresh] re-init succeeded");
                        true
                    }
                    Err(e) => {
                        warn!("[session-refresh] re-init failed: {}", e);
                        false
                    }
                }
            })
        });

        self.client.set_session_refresh_hook(hook).await;
    }

    /// Get the current session state.
    pub async fn state(&self) -> SessionState {
        self.state_for_current().read().await.clone()
    }

    /// Update the legacy hosted-provider capability snapshot after a
    /// `session_init` response is parsed.
    pub async fn set_atlas_remote_capabilities(
        &self,
        capabilities: Option<mcp_types::atlas_layer::AtlasRemoteCapabilities>,
    ) {
        let handle = self.state_for_current();
        handle.write().await.atlas_remote_capabilities = capabilities;
    }

    /// Update account mode preference (persisted for session bucket).
    pub async fn set_account_mode_preference(&self, preference: AccountModePreference) {
        let handle = self.state_for_current();
        handle.write().await.account_mode_preference = preference;
    }

    /// Apply resolved execution mode + account context snapshot.
    pub async fn set_account_execution_state(
        &self,
        preference: AccountModePreference,
        execution_mode: ExecutionMode,
        account_context: Option<AccountContextSnapshot>,
        team_context_degraded: bool,
    ) {
        let handle = self.state_for_current();
        let mut state = handle.write().await;
        state.account_mode_preference = preference;
        state.active_execution_mode = execution_mode;
        if let Some(ref ctx) = account_context {
            state.last_account_user_id = ctx.user_id;
        }
        state.account_context = account_context;
        state.team_context_degraded = team_context_degraded;
    }

    /// Clear team-context degradation after account/context is healthy again.
    pub async fn clear_team_context_degradation(&self) {
        let handle = self.state_for_current();
        let mut state = handle.write().await;
        state.team_context_degraded = false;
    }

    /// Mark team context degraded (account switch safety).
    pub async fn degrade_team_context(&self, reason: &str) {
        let handle = self.state_for_current();
        let mut state = handle.write().await;
        state.team_context_degraded = true;
        state.active_execution_mode = ExecutionMode::Personal;
        if let Some(ref mut ctx) = state.account_context {
            ctx.has_team_membership = false;
            ctx.selected_context = "personal".to_string();
        }
        warn!("Team context degraded: {}", reason);
    }

    /// Current resolved execution mode.
    pub async fn active_execution_mode(&self) -> ExecutionMode {
        self.state_for_current().read().await.active_execution_mode
    }

    /// Whether team features should be active for tool gating.
    pub async fn team_features_enabled(&self) -> bool {
        let handle = self.state_for_current();
        let state = handle.read().await;
        if state.team_context_degraded {
            return false;
        }
        state
            .account_context
            .as_ref()
            .map(|ctx| ctx.team_features_available())
            .unwrap_or(false)
            && matches!(state.active_execution_mode, ExecutionMode::Team)
    }

    /// Check persisted account metadata against freshly fetched context.
    pub async fn detect_account_mismatch(&self, fresh: &AccountContextSnapshot) -> bool {
        let handle = self.state_for_current();
        let state = handle.read().await;
        if let (Some(prev), Some(next)) = (state.last_account_user_id, fresh.user_id) {
            if prev != next {
                return true;
            }
        }
        if state.team_context_degraded {
            return false;
        }
        if let Some(ref prev_ctx) = state.account_context {
            if prev_ctx.team_features_available()
                && !fresh.team_features_available()
                && matches!(state.active_execution_mode, ExecutionMode::Team)
            {
                return true;
            }
        }
        false
    }

    /// Check if session is initialized.
    pub async fn is_initialized(&self) -> bool {
        self.state_for_current().read().await.initialized
    }

    /// Initialize the session.
    pub async fn initialize(
        &self,
        workspace_id: Option<Uuid>,
        project_id: Option<Uuid>,
        folder_path: Option<String>,
        default_search_mode: Option<String>,
    ) {
        self.initialize_with_session_id(
            workspace_id,
            project_id,
            folder_path,
            default_search_mode,
            None,
        )
        .await;
    }

    /// Initialize the session while preserving a caller- or transport-issued
    /// durable identity.
    ///
    /// Hosted MCP requests may land on different gateway processes. Keeping
    /// the same id that was sent to `/session/init` lets later context/search
    /// requests rehydrate the server-side scope instead of silently switching
    /// to a process-local random UUID. Local/direct callers retain the legacy
    /// random UUID fallback.
    pub async fn initialize_with_session_id(
        &self,
        workspace_id: Option<Uuid>,
        project_id: Option<Uuid>,
        folder_path: Option<String>,
        default_search_mode: Option<String>,
        session_id: Option<String>,
    ) {
        let session_id = session_id
            .and_then(|value| {
                let trimmed = value.trim();
                (!trimmed.is_empty()).then(|| trimmed.to_string())
            })
            .or_else(get_task_mcp_session_id)
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let handle = self.state_for_current();
        let mut state = handle.write().await;
        state.workspace_id = workspace_id;
        state.project_id = project_id;
        state.folder_path = folder_path;
        state.grounding_handle = None;
        state.initialized = true;
        state.started_at = Utc::now();
        state.session_id = Some(session_id);
        state.transcript_capture_enabled = None;
        state.default_search_mode = default_search_mode;

        // Update client defaults
        drop(state);
        self.client.set_defaults(workspace_id, project_id).await;
    }

    /// Update active workspace/project scope without resetting session identity.
    pub async fn update_scope(
        &self,
        workspace_id: Option<Uuid>,
        project_id: Option<Uuid>,
        folder_path: Option<String>,
    ) {
        let handle = self.state_for_current();
        let (effective_workspace_id, effective_project_id) = {
            let mut state = handle.write().await;
            let previous_scope = (
                state.workspace_id,
                state.project_id,
                state.folder_path.clone(),
            );
            if workspace_id.is_some() {
                state.workspace_id = workspace_id;
            }
            if project_id.is_some() {
                state.project_id = project_id;
            }
            if folder_path.is_some() {
                state.folder_path = folder_path;
            }
            if state.workspace_id.is_some() || state.project_id.is_some() {
                state.initialized = true;
            }
            if previous_scope
                != (
                    state.workspace_id,
                    state.project_id,
                    state.folder_path.clone(),
                )
            {
                state.grounding_handle = None;
            }
            (state.workspace_id, state.project_id)
        };

        self.client
            .set_defaults(effective_workspace_id, effective_project_id)
            .await;
    }

    /// Replace active workspace/project scope exactly, including clearing a
    /// stale folder path when the caller has proven it no longer matches.
    pub async fn replace_scope(
        &self,
        workspace_id: Option<Uuid>,
        project_id: Option<Uuid>,
        folder_path: Option<String>,
    ) {
        let handle = self.state_for_current();
        {
            let mut state = handle.write().await;
            let scope_changed = state.workspace_id != workspace_id
                || state.project_id != project_id
                || state.folder_path != folder_path;
            state.workspace_id = workspace_id;
            state.project_id = project_id;
            state.folder_path = folder_path;
            if scope_changed {
                state.grounding_handle = None;
            }
            if state.workspace_id.is_some() || state.project_id.is_some() {
                state.initialized = true;
            }
        }

        self.client.set_defaults(workspace_id, project_id).await;
    }

    /// Store the latest server-issued grounding handle for this caller's
    /// current scope. Empty or unreasonably large values are discarded so an
    /// untrusted upstream response cannot turn session state into an unbounded
    /// allocation sink.
    pub async fn set_grounding_handle(&self, grounding_handle: Option<String>) {
        let normalized = grounding_handle.and_then(|value| {
            let trimmed = value.trim();
            if trimmed.is_empty() || trimmed.len() > 1024 {
                None
            } else {
                Some(trimmed.to_string())
            }
        });
        let handle = self.state_for_current();
        handle.write().await.grounding_handle = normalized;
    }

    /// Increment conversation turn count.
    pub async fn increment_turn(&self) {
        let handle = self.state_for_current();
        let mut state = handle.write().await;
        state.conversation_turns += 1;
    }

    /// Add to session token count.
    pub async fn add_tokens(&self, tokens: i64) {
        let handle = self.state_for_current();
        let mut state = handle.write().await;
        state.session_tokens += tokens;
    }

    /// Get current pressure level.
    pub async fn pressure_level(&self, threshold: i64) -> PressureLevel {
        let handle = self.state_for_current();
        let state = handle.read().await;
        PressureLevel::from_tokens(state.total_tokens(), threshold)
    }

    /// Mark high context pressure.
    pub async fn mark_high_pressure(&self) {
        let handle = self.state_for_current();
        let mut state = handle.write().await;
        state.mark_high_pressure();
    }

    /// Mark high context pressure using the token estimate returned by the
    /// context API.
    pub async fn mark_high_pressure_with_tokens(&self, tokens: i64) {
        let handle = self.state_for_current();
        let mut state = handle.write().await;
        state.mark_high_pressure_with_tokens(tokens);
    }

    /// Check if we should restore context.
    pub async fn should_restore_context(&self) -> bool {
        let handle = self.state_for_current();
        let state = handle.read().await;
        state.should_restore_post_compact()
    }

    /// Check if we should restore context using a caller-provided token count.
    pub async fn should_restore_context_for_tokens(&self, current_tokens: i64) -> bool {
        let handle = self.state_for_current();
        let state = handle.read().await;
        state.should_restore_post_compact_for_tokens(current_tokens)
    }

    /// Mark context as restored.
    pub async fn mark_context_restored(&self) {
        let handle = self.state_for_current();
        let mut state = handle.write().await;
        state.context_restored = true;
        state.last_high_pressure_at = None;
        state.last_high_pressure_tokens = None;
    }

    /// Reset session state.
    pub async fn reset(&self) {
        let handle = self.state_for_current();
        let mut state = handle.write().await;
        *state = SessionState::default();
    }

    /// Set per-session transcript capture preference.
    pub async fn set_transcript_capture_enabled(&self, enabled: Option<bool>) {
        let handle = self.state_for_current();
        let mut state = handle.write().await;
        state.transcript_capture_enabled = enabled;
    }

    /// Get per-session transcript capture preference.
    pub async fn transcript_capture_enabled(&self) -> Option<bool> {
        self.state_for_current()
            .read()
            .await
            .transcript_capture_enabled
    }

    /// Get a cached workspace mapping for the given folder path.
    /// Returns the cached mapping if it exists, matches the folder path, and is within TTL.
    pub async fn get_cached_workspace(&self, folder_path: &str) -> Option<WorkspaceMapping> {
        let handle = self.cached_workspace_for_current();
        let cache = handle.read().await;
        if let Some(ref cached) = *cache {
            if cached.folder_path == folder_path {
                let elapsed = Utc::now().signed_duration_since(cached.cached_at);
                if elapsed.num_seconds() < WORKSPACE_CACHE_TTL_SECS {
                    return Some(cached.mapping.clone());
                }
            }
        }
        None
    }

    /// Cache a workspace mapping for the given folder path.
    pub async fn set_cached_workspace(&self, folder_path: &str, mapping: WorkspaceMapping) {
        let handle = self.cached_workspace_for_current();
        let mut cache = handle.write().await;
        *cache = Some(CachedMapping {
            mapping,
            folder_path: folder_path.to_string(),
            cached_at: Utc::now(),
        });
    }

    /// Clear the cached workspace mapping (e.g., on force re-init).
    pub async fn clear_cached_workspace(&self) {
        let handle = self.cached_workspace_for_current();
        let mut cache = handle.write().await;
        *cache = None;
    }

    /// Get the client.
    pub fn client(&self) -> &ContextStreamClient {
        &self.client
    }

    // =========================================================================
    // Child Project Registry (Multi-Project Parent Folder)
    // =========================================================================

    /// Store the map of related projects detected during init.
    pub async fn set_project_relations(&self, projects: HashMap<String, RelatedProjectInfo>) {
        let handle = self.state_for_current();
        let mut state = handle.write().await;
        state.project_relations = projects;
    }

    /// Get all related projects for the current session.
    pub async fn get_project_relations(&self) -> HashMap<String, RelatedProjectInfo> {
        self.state_for_current()
            .read()
            .await
            .project_relations
            .clone()
    }

    /// Store the map of child projects detected during init.
    /// Backward-compatible shim that maps children into the relation graph.
    pub async fn set_child_projects(&self, projects: HashMap<String, ChildProjectInfo>) {
        let handle = self.state_for_current();
        let mut state = handle.write().await;
        let mut related = HashMap::new();
        for (key, child) in projects {
            related.insert(
                key,
                RelatedProjectInfo {
                    project_id: child.project_id,
                    name: child.name,
                    path: child.path,
                    relation: ProjectRelationKind::Child,
                },
            );
        }
        state.project_relations = related;
    }

    /// Get the current child projects map.
    pub async fn get_child_projects(&self) -> HashMap<String, ChildProjectInfo> {
        self.state_for_current()
            .read()
            .await
            .project_relations
            .iter()
            .filter(|&(_key, related)| related.relation == ProjectRelationKind::Child)
            .map(|(key, related)| {
                (
                    key.clone(),
                    ChildProjectInfo {
                        project_id: related.project_id.clone(),
                        name: related.name.clone(),
                        path: related.path.clone(),
                    },
                )
            })
            .collect()
    }

    /// Check if the session has child projects.
    pub async fn has_child_projects(&self) -> bool {
        self.state_for_current()
            .read()
            .await
            .project_relations
            .values()
            .any(|related| related.relation == ProjectRelationKind::Child)
    }

    /// Resolve a child project by folder name or project name.
    /// Matches case-insensitively against both the folder key and the project name.
    pub async fn resolve_child_project_by_name(&self, name: &str) -> Option<ChildProjectInfo> {
        let handle = self.state_for_current();
        let state = handle.read().await;
        if state.project_relations.is_empty() {
            return None;
        }

        let lower = name.to_ascii_lowercase();
        if lower.is_empty() {
            return None;
        }

        // Exact folder-key match first
        for (child_key, child) in &state.project_relations {
            if child.relation != ProjectRelationKind::Child {
                continue;
            }
            if child_key.to_ascii_lowercase() == lower {
                return Some(ChildProjectInfo {
                    project_id: child.project_id.clone(),
                    name: child.name.clone(),
                    path: child.path.clone(),
                });
            }
        }

        // Then project-name match
        for child in state.project_relations.values() {
            if child.relation == ProjectRelationKind::Child
                && child.name.to_ascii_lowercase() == lower
            {
                return Some(ChildProjectInfo {
                    project_id: child.project_id.clone(),
                    name: child.name.clone(),
                    path: child.path.clone(),
                });
            }
        }

        // Partial/contains match as last resort
        for (child_key, child) in &state.project_relations {
            if child.relation != ProjectRelationKind::Child {
                continue;
            }
            let key_lower = child_key.to_ascii_lowercase();
            let name_lower = child.name.to_ascii_lowercase();
            if key_lower.contains(&lower)
                || lower.contains(&key_lower)
                || name_lower.contains(&lower)
                || lower.contains(&name_lower)
            {
                return Some(ChildProjectInfo {
                    project_id: child.project_id.clone(),
                    name: child.name.clone(),
                    path: child.path.clone(),
                });
            }
        }

        None
    }

    /// Resolve a child project from a file path.
    /// If the file path is inside one of the child project directories,
    /// returns that child project info.
    pub async fn resolve_child_project_by_path(
        &self,
        target_path: &str,
    ) -> Option<ChildProjectInfo> {
        let handle = self.state_for_current();
        let state = handle.read().await;
        if state.project_relations.is_empty() {
            return None;
        }

        let target = normalize_match_path(std::path::Path::new(target_path));
        for child in state.project_relations.values() {
            if child.relation != ProjectRelationKind::Child {
                continue;
            }
            let child_path = normalize_match_path(std::path::Path::new(&child.path));
            if target == child_path || target.starts_with(&child_path) {
                return Some(ChildProjectInfo {
                    project_id: child.project_id.clone(),
                    name: child.name.clone(),
                    path: child.path.clone(),
                });
            }
        }

        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_client::{run_with_mcp_session_id, run_with_session_key};

    fn test_session_manager() -> SessionManager {
        SessionManager::new(
            ContextStreamClient::new(Config::default()),
            Config::default(),
        )
    }

    #[tokio::test]
    async fn initialization_preserves_explicit_or_transport_session_identity() {
        let session = test_session_manager();

        run_with_session_key(SessionKey::Jwt("explicit-user".into()), || async {
            run_with_mcp_session_id("transport-session".to_string(), || async {
                session
                    .initialize_with_session_id(
                        None,
                        None,
                        Some("/repo/explicit".to_string()),
                        None,
                        Some("  explicit-session  ".to_string()),
                    )
                    .await;
                assert_eq!(
                    session.state().await.session_id.as_deref(),
                    Some("explicit-session")
                );
            })
            .await;
        })
        .await;

        run_with_session_key(SessionKey::Jwt("transport-user".into()), || async {
            run_with_mcp_session_id("transport-session".to_string(), || async {
                session
                    .initialize(None, None, Some("/repo/transport".to_string()), None)
                    .await;
                assert_eq!(
                    session.state().await.session_id.as_deref(),
                    Some("transport-session")
                );
            })
            .await;
        })
        .await;
    }

    #[tokio::test]
    async fn resolves_child_projects_by_folder_key_and_project_name() {
        let session = test_session_manager();
        let mut projects = HashMap::new();
        projects.insert(
            "contextstream".to_string(),
            ChildProjectInfo {
                project_id: "11111111-1111-4111-8111-111111111111".to_string(),
                name: "ContextStream".to_string(),
                path: "/tmp/contextstream".to_string(),
            },
        );
        projects.insert(
            "mcp-server".to_string(),
            ChildProjectInfo {
                project_id: "22222222-2222-4222-8222-222222222222".to_string(),
                name: "MCP Server".to_string(),
                path: "/tmp/mcp-server".to_string(),
            },
        );
        session.set_child_projects(projects).await;

        assert_eq!(
            session
                .resolve_child_project_by_name("contextstream")
                .await
                .expect("folder key match")
                .project_id,
            "11111111-1111-4111-8111-111111111111"
        );
        assert_eq!(
            session
                .resolve_child_project_by_name("mcp server")
                .await
                .expect("project name match")
                .project_id,
            "22222222-2222-4222-8222-222222222222"
        );
        assert_eq!(
            session
                .resolve_child_project_by_name("context")
                .await
                .expect("partial match")
                .project_id,
            "11111111-1111-4111-8111-111111111111"
        );
    }

    #[tokio::test]
    async fn resolves_child_projects_by_real_or_symlinked_paths(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let temp = std::env::temp_dir().join(format!("mcp-session-{}", Uuid::new_v4()));
        let real_root = temp.join("real-root");
        let linked_root = temp.join("linked-root");
        let child_dir = real_root.join("contextstream").join("src");
        std::fs::create_dir_all(&child_dir)?;
        let child_file = child_dir.join("lib.rs");
        std::fs::write(&child_file, "pub fn smoke() {}\n")?;

        #[cfg(unix)]
        std::os::unix::fs::symlink(&real_root, &linked_root)?;
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&real_root, &linked_root)?;

        let session = test_session_manager();
        let mut projects = HashMap::new();
        projects.insert(
            "contextstream".to_string(),
            ChildProjectInfo {
                project_id: "33333333-3333-4333-8333-333333333333".to_string(),
                name: "ContextStream".to_string(),
                path: linked_root
                    .join("contextstream")
                    .to_string_lossy()
                    .to_string(),
            },
        );
        session.set_child_projects(projects).await;

        let resolved = session
            .resolve_child_project_by_path(child_file.to_string_lossy().as_ref())
            .await
            .expect("real path should resolve through symlinked child root");
        assert_eq!(resolved.project_id, "33333333-3333-4333-8333-333333333333");

        std::fs::remove_dir_all(&temp)?;
        Ok(())
    }

    /// Regression: before per-subject state, the `folder_path` written by
    /// one caller's `update_scope(...)` would leak into another caller's
    /// `state().await` because all requests shared a single
    /// `Arc<RwLock<SessionState>>` inside `SessionManager`. This test
    /// isolates two simulated subjects via `run_with_session_key` and
    /// confirms each only sees its own value.
    #[tokio::test]
    async fn session_state_is_isolated_by_session_key() {
        let session = test_session_manager();

        // Subject A writes its folder_path, reads it back: should see its own.
        run_with_session_key(SessionKey::Jwt("user-a".into()), || async {
            session
                .update_scope(
                    None,
                    None,
                    Some("/Users/alice/projects/contextstream".to_string()),
                )
                .await;
            let state = session.state().await;
            assert_eq!(
                state.folder_path.as_deref(),
                Some("/Users/alice/projects/contextstream")
            );
        })
        .await;

        // Subject B reads state WITHOUT having written anything. Must see
        // empty state, NOT Subject A's folder_path.
        run_with_session_key(SessionKey::Jwt("user-b".into()), || async {
            let state = session.state().await;
            assert_eq!(
                state.folder_path, None,
                "subject B must not observe subject A's folder_path"
            );

            // Subject B writes its own and reads back — still isolated.
            session
                .update_scope(None, None, Some("/home/bob/code/other".to_string()))
                .await;
            let state = session.state().await;
            assert_eq!(state.folder_path.as_deref(), Some("/home/bob/code/other"));
        })
        .await;

        // Subject A reads again: still its own, unchanged by Subject B.
        run_with_session_key(SessionKey::Jwt("user-a".into()), || async {
            let state = session.state().await;
            assert_eq!(
                state.folder_path.as_deref(),
                Some("/Users/alice/projects/contextstream"),
                "subject A's folder_path must not have been overwritten by subject B"
            );
        })
        .await;

        // Local (unscoped) callers — e.g. CLI — share one bucket distinct
        // from both JWT-keyed subjects.
        {
            let state = session.state().await;
            assert_eq!(
                state.folder_path, None,
                "local/unscoped bucket must not inherit from any subject"
            );
        }
    }

    #[tokio::test]
    async fn transient_request_state_can_be_discarded_without_residue() {
        let session = test_session_manager();
        let key =
            SessionKey::for_http_jwt("stateless-user", Some("stateless-request:request-nonce"));
        let mapping = WorkspaceMapping {
            workspace_id: Uuid::new_v4(),
            workspace_name: "transient-workspace".to_string(),
            project_id: Some(Uuid::new_v4()),
            project_name: Some("transient-project".to_string()),
        };

        run_with_session_key(key.clone(), || async {
            session
                .update_scope(None, None, Some("/repo/transient".to_string()))
                .await;
            session
                .set_cached_workspace("/repo/transient", mapping)
                .await;
            assert_eq!(
                session.state().await.folder_path.as_deref(),
                Some("/repo/transient")
            );
            assert!(session
                .get_cached_workspace("/repo/transient")
                .await
                .is_some());
        })
        .await;

        assert!(session.discard_transient_state(&key));
        assert!(!session.discard_transient_state(&key));

        run_with_session_key(key, || async {
            assert_eq!(session.state().await.folder_path, None);
            assert!(session
                .get_cached_workspace("/repo/transient")
                .await
                .is_none());
        })
        .await;
    }

    #[tokio::test]
    async fn anonymous_http_sessions_never_share_scope_or_grounding_handle() {
        let session = test_session_manager();
        let anonymous_a = SessionKey::for_anonymous_http("mcp-session-a");
        let anonymous_b = SessionKey::for_anonymous_http("mcp-session-b");

        run_with_session_key(anonymous_a, || async {
            session
                .update_scope(None, None, Some("/private/anonymous-a".to_string()))
                .await;
            session
                .set_grounding_handle(Some("grounding-a".to_string()))
                .await;
        })
        .await;

        run_with_session_key(anonymous_b, || async {
            let state = session.state().await;
            assert_eq!(state.folder_path, None);
            assert_eq!(state.grounding_handle, None);
        })
        .await;

        // Missing-header requests receive request-unique partition ids from
        // the HTTP transport and therefore cannot observe one another either.
        let missing_header_a = SessionKey::for_anonymous_http("anonymous-request:nonce-a");
        let missing_header_b = SessionKey::for_anonymous_http("anonymous-request:nonce-b");
        run_with_session_key(missing_header_a, || async {
            session
                .set_grounding_handle(Some("missing-header-a".to_string()))
                .await;
        })
        .await;
        run_with_session_key(missing_header_b, || async {
            assert_eq!(session.state().await.grounding_handle, None);
        })
        .await;
    }

    #[tokio::test]
    async fn grounding_handle_is_tenant_isolated_and_cleared_on_scope_change() {
        let session = test_session_manager();
        let workspace_a = Uuid::from_u128(1);
        let project_a = Uuid::from_u128(2);
        let project_b = Uuid::from_u128(3);

        run_with_session_key(SessionKey::Jwt("user-a".into()), || async {
            session
                .initialize(
                    Some(workspace_a),
                    Some(project_a),
                    Some("/repo/a".to_string()),
                    None,
                )
                .await;
            session
                .set_grounding_handle(Some(" grounding-a ".to_string()))
                .await;
            assert_eq!(
                session.state().await.grounding_handle.as_deref(),
                Some("grounding-a")
            );

            // Re-applying the exact scope keeps the reusable handle.
            session
                .update_scope(
                    Some(workspace_a),
                    Some(project_a),
                    Some("/repo/a".to_string()),
                )
                .await;
            assert_eq!(
                session.state().await.grounding_handle.as_deref(),
                Some("grounding-a")
            );

            // A project switch invalidates the prior scope-bound handle.
            session
                .update_scope(Some(workspace_a), Some(project_b), None)
                .await;
            assert_eq!(session.state().await.grounding_handle, None);
        })
        .await;

        run_with_session_key(SessionKey::Jwt("user-b".into()), || async {
            assert_eq!(
                session.state().await.grounding_handle,
                None,
                "subject B must not observe subject A's grounding handle"
            );
        })
        .await;
    }

    /// Regression: the workspace-mapping cache is also per-subject.
    #[tokio::test]
    async fn workspace_cache_is_isolated_by_session_key() {
        let session = test_session_manager();

        let mapping = WorkspaceMapping {
            workspace_id: Uuid::new_v4(),
            workspace_name: "ws-a".into(),
            project_id: Some(Uuid::new_v4()),
            project_name: Some("proj-a".into()),
        };

        run_with_session_key(SessionKey::ApiKey("aaaa".into()), || async {
            session
                .set_cached_workspace("/path/a", mapping.clone())
                .await;
            let got = session.get_cached_workspace("/path/a").await;
            assert!(got.is_some());
        })
        .await;

        run_with_session_key(SessionKey::ApiKey("bbbb".into()), || async {
            let got = session.get_cached_workspace("/path/a").await;
            assert!(
                got.is_none(),
                "subject B must not see subject A's cached workspace mapping"
            );
        })
        .await;
    }

    #[test]
    fn detects_restore_need_after_large_token_drop() {
        let mut state = SessionState::default();
        state.mark_high_pressure_with_tokens(80_000);

        assert!(state.should_restore_post_compact_for_tokens(8_000));
        assert!(!state.should_restore_post_compact_for_tokens(50_000));
    }

    #[test]
    fn restore_detection_stops_after_mark_restored() {
        let mut state = SessionState::default();
        state.mark_high_pressure_with_tokens(80_000);
        state.context_restored = true;

        assert!(!state.should_restore_post_compact_for_tokens(8_000));

        state.mark_high_pressure_with_tokens(90_000);
        assert!(state.should_restore_post_compact_for_tokens(7_000));
    }
}
