//! Tool registry for managing tool registration and dispatch.

use async_trait::async_trait;
use mcp_client::ContextStreamClient;
use mcp_session::SessionManager;
use mcp_types::{
    acceleration_layer::{
        noop_acceleration_layer, AccelerationLayer, AccelerationSignalEvent, AccelerationSignalKind,
    },
    atlas_layer::{noop_layer, AtlasLayer},
    config::{ToolSurfaceProfile, Toolset},
    tool::{ToolCategory, ToolMetadata, ToolResult},
    Config, Error, Result,
};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Tool handler trait.
#[async_trait]
pub trait ToolHandler: Send + Sync {
    /// Execute the tool with the given input.
    async fn execute(&self, input: Value) -> Result<ToolResult>;

    /// Get the tool metadata.
    fn metadata(&self) -> &ToolMetadata;

    /// Get the JSON schema for the tool input.
    fn input_schema(&self) -> Value;
}

/// Registered tool with handler.
pub struct RegisteredTool {
    pub metadata: ToolMetadata,
    pub handler: Arc<dyn ToolHandler>,
    pub input_schema: Value,
}

/// Return true when a nominally successful tool result is actually an access
/// gate. Runtime readiness must not promote setup/auth/upgrade instructions as
/// successful grounding or practice.
pub fn tool_result_is_access_gate(result: &ToolResult) -> bool {
    result.content.iter().any(|item| {
        let mcp_types::tool::ContentItem::Text { text } = item else {
            return false;
        };
        let normalized = text.to_ascii_lowercase();
        [
            "[setup_required]",
            "[auth_required]",
            "[access_gate]",
            "[upgrade_required]",
            "authentication required",
            "run setup first",
            "session initialized without a resolved workspace_id",
            "no usable workspace_id was returned",
        ]
        .iter()
        .any(|marker| normalized.contains(marker))
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CacheObservation {
    hit: bool,
    layer: &'static str,
}

#[derive(Debug, Clone, Copy)]
struct AccelerationObservationRequest {
    tool: &'static str,
    action: &'static str,
    cache_layer: Option<&'static str>,
    workspace_id: Option<Uuid>,
    project_id: Option<Uuid>,
}

fn input_action(input: &Value) -> Option<&str> {
    input
        .get("action")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|action| !action.is_empty())
}

fn input_mode(input: &Value) -> Option<&str> {
    input
        .get("mode")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|mode| !mode.is_empty())
}

/// Return a bounded action label only for user-facing hot reads whose latency
/// is useful to the acceleration dashboard. Every returned value is selected
/// from a fixed allowlist: arbitrary user input can never become a metric or
/// rollup dimension.
fn acceleration_observation_action(name: &str, input: &Value) -> Option<&'static str> {
    match name {
        "context" => Some(match input_mode(input) {
            Some("fast" | "hook") => "fast",
            Some("pack") => "pack",
            Some("standard" | "smart") => "standard",
            _ => "adaptive",
        }),
        "search" => Some(match input_mode(input) {
            Some("semantic") => "semantic",
            Some("hybrid") => "hybrid",
            Some("keyword") => "keyword",
            Some("pattern") => "pattern",
            Some("exhaustive") => "exhaustive",
            Some("refactor") => "refactor",
            Some("team") => "team",
            Some("guided") => "guided",
            Some("crawl") => "crawl",
            _ => "auto",
        }),
        "search_semantic" => Some("semantic"),
        "search_hybrid" => Some("hybrid"),
        "search_keyword" => Some("keyword"),
        "session_recall" => Some("recall"),
        "memory_search" => Some("search"),
        "session" => match input_action(input) {
            Some("recall") => Some("recall"),
            Some("ground") => Some("ground"),
            _ => None,
        },
        "memory" => match input_action(input) {
            Some("search") => Some("search"),
            Some("search_transcripts") => Some("search_transcripts"),
            Some("decisions") => Some("decisions"),
            Some("list_docs") => Some("list_docs"),
            Some("list_events") => Some("list_events"),
            Some("list_tasks") => Some("list_tasks"),
            Some("list_todos") => Some("list_todos"),
            Some("list_transcripts") => Some("list_transcripts"),
            Some("list_nodes") => Some("list_nodes"),
            Some("list_diagrams") => Some("list_diagrams"),
            _ => None,
        },
        "project" => match input_action(input) {
            Some("list") => Some("list"),
            Some("get") => Some("get"),
            Some("index_status") => Some("index_status"),
            _ => None,
        },
        "workspace" => match input_action(input) {
            Some("list") => Some("list"),
            Some("get") => Some("get"),
            _ => None,
        },
        "help" => match input_action(input) {
            Some("version") => Some("version"),
            Some("tools") => Some("tools"),
            Some("auth") => Some("auth"),
            _ => None,
        },
        "reminder" => match input_action(input) {
            Some("list") => Some("list"),
            Some("active") => Some("active"),
            _ => None,
        },
        "coordination" => match input_action(input) {
            Some("inbox") | Some("list") | Some("get") | Some("settings") => Some("list"),
            _ => None,
        },
        _ => None,
    }
}

fn cache_layer_for_call(name: &str, input: &Value) -> Option<&'static str> {
    match name {
        "search" | "search_semantic" | "search_hybrid" | "search_keyword" => {
            // Learning-consented calls intentionally bypass the rendered
            // result cache so their observation reaches the API.
            (input
                .get("code_rerank_learning_opt_in")
                .and_then(Value::as_bool)
                != Some(true))
            .then_some("mcp_search_result_cache")
        }
        "session_recall" => Some("mcp_recall_result_cache"),
        "memory_search" => Some("mcp_memory_result_cache"),
        "session" if input_action(input) == Some("recall") => Some("mcp_recall_result_cache"),
        "memory" if input_action(input) == Some("search") => Some("mcp_memory_result_cache"),
        "memory" if input_action(input) == Some("search_transcripts") => {
            Some("mcp_transcript_result_cache")
        }
        _ => None,
    }
}

fn result_has_cache_hit_marker(result: &ToolResult) -> bool {
    const PREFIX_MARKERS: &[&str] = &[
        "[SEARCH_CACHED]",
        "[RECALL_CACHED]",
        "[MEMORY_CACHED]",
        "[TRANSCRIPTS_CACHED]",
        "[WARM_CACHE]",
    ];

    result.content.iter().any(|item| {
        let mcp_types::tool::ContentItem::Text { text } = item else {
            return false;
        };
        PREFIX_MARKERS.iter().any(|marker| text.starts_with(marker))
            || text.contains("\n[SEARCH_CACHED] Reused the previous identical guided result")
    })
}

fn normalize_structured_cache_layer(
    served_from: Option<&str>,
    fallback: Option<&'static str>,
) -> &'static str {
    match served_from {
        Some("regional_warm_cache") => "regional_warm_cache",
        Some("distributed_context_cache") => "distributed_context_cache",
        Some("process_context_cache" | "local_context_cache") => "process_context_cache",
        Some("redis" | "redis_cache") => "redis",
        Some("memory" | "memory_cache" | "l1") => "memory",
        Some("primary_server") => fallback.unwrap_or("regional_warm_cache"),
        _ => fallback.unwrap_or("mcp_warm_cache"),
    }
}

fn cache_observation(
    fallback: Option<&'static str>,
    outcome: std::result::Result<&ToolResult, &Error>,
) -> Option<CacheObservation> {
    let result = outcome.ok()?;
    if result.is_error {
        return None;
    }

    if let Some(structured) = result.structured_content.as_ref() {
        if let Some(hit) = structured.get("cache_hit").and_then(Value::as_bool) {
            let served_from = structured.get("served_from").and_then(Value::as_str);
            return Some(CacheObservation {
                hit,
                layer: normalize_structured_cache_layer(served_from, fallback),
            });
        }
    }

    if result_has_cache_hit_marker(result) {
        return Some(CacheObservation {
            hit: true,
            layer: fallback.unwrap_or("mcp_warm_cache"),
        });
    }

    fallback.map(|layer| CacheObservation { hit: false, layer })
}

fn request_scope(input: &Value) -> (Option<Uuid>, Option<Uuid>) {
    let parse = |field: &str| {
        input
            .get(field)
            .and_then(Value::as_str)
            .and_then(|value| Uuid::parse_str(value.trim()).ok())
    };
    (parse("workspace_id"), parse("project_id"))
}

fn acceleration_observation_request(
    name: &str,
    input: &Value,
) -> Option<AccelerationObservationRequest> {
    let action = acceleration_observation_action(name, input)?;
    let tool = match name {
        "context" => "context",
        "search" => "search",
        "search_semantic" => "search_semantic",
        "search_hybrid" => "search_hybrid",
        "search_keyword" => "search_keyword",
        "session_recall" => "session_recall",
        "memory_search" => "memory_search",
        "session" => "session",
        "memory" => "memory",
        "project" => "project",
        "workspace" => "workspace",
        "help" => "help",
        "reminder" => "reminder",
        _ => return None,
    };
    let cache_layer = cache_layer_for_call(name, input);
    let (workspace_id, project_id) = request_scope(input);
    Some(AccelerationObservationRequest {
        tool,
        action,
        cache_layer,
        workspace_id,
        project_id,
    })
}

fn resolved_observation_scope(
    explicit_workspace_id: Option<Uuid>,
    explicit_project_id: Option<Uuid>,
    session_workspace_id: Option<Uuid>,
    session_project_id: Option<Uuid>,
) -> (Option<Uuid>, Option<Uuid>) {
    let workspace_id = explicit_workspace_id.or(session_workspace_id);
    let project_id = explicit_project_id.or_else(|| {
        if explicit_workspace_id.is_some() && explicit_workspace_id != session_workspace_id {
            None
        } else {
            session_project_id
        }
    });
    (workspace_id, project_id)
}

fn outcome_is_degraded(outcome: std::result::Result<&ToolResult, &Error>) -> bool {
    match outcome {
        Err(_) => true,
        Ok(result) => {
            result.is_error
                || result
                    .structured_content
                    .as_ref()
                    .and_then(|value| value.get("degraded"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
        }
    }
}

#[derive(Clone, Copy)]
struct DiscoveryHints {
    aliases: &'static [&'static str],
    tags: &'static [&'static str],
    when_to_use: &'static str,
    avoid_when: &'static str,
    examples: &'static [&'static str],
    latency_class: &'static str,
    parallel_safe: bool,
    batch_safe: bool,
}

/// Tool registry managing all registered tools.
pub struct ToolRegistry {
    tools: HashMap<String, RegisteredTool>,
    /// Operations registry for router mode (tools accessed via meta-tools)
    operations: HashMap<String, RegisteredTool>,
    toolsets: ToolsetConfig,
    enabled_bundles: HashSet<String>,
    progressive_mode: bool,
    router_mode: bool,
    consolidated_mode: bool,
    tool_surface_profile: RwLock<ToolSurfaceProfile>,
    /// The surface profile this registry was constructed with. `initialize`
    /// resets the live `tool_surface_profile` back to this when a request
    /// carries no explicit/auto-detected profile, so a prior client's
    /// auto-detected narrowing can never persist into the next client on a
    /// shared (multi-tenant) process. Immutable after construction.
    default_tool_surface_profile: ToolSurfaceProfile,
    /// Legacy provider compatibility layer. Public builds keep this as a no-op;
    /// current hosted products use `acceleration_layer` below.
    atlas_layer: AtlasLayer,
    /// MongoDB-free acceleration layer. This is separate from the
    /// Atlas compatibility surface while call sites migrate
    /// provider-by-provider.
    acceleration_layer: AccelerationLayer,
    session: Option<Arc<SessionManager>>,
    /// Local stdio client used only for detached, best-effort activation
    /// telemetry. Remote HTTP gateways intentionally leave this unset because
    /// their process installation is not the end user's editor installation.
    activation_client: Option<ContextStreamClient>,
}

impl ToolRegistry {
    /// Create a new tool registry.
    pub fn new(config: &Config) -> Self {
        Self {
            tools: HashMap::new(),
            operations: HashMap::new(),
            toolsets: ToolsetConfig::new(config.toolset),
            enabled_bundles: HashSet::new(),
            progressive_mode: config.progressive_mode,
            router_mode: config.router_mode,
            consolidated_mode: config.consolidated_mode,
            tool_surface_profile: RwLock::new(config.tool_surface_profile),
            default_tool_surface_profile: config.tool_surface_profile,
            atlas_layer: noop_layer(),
            acceleration_layer: noop_acceleration_layer(),
            session: None,
            activation_client: None,
        }
    }

    /// Attach the session manager so runtime tool calls can surface plan-aware
    /// upgrade nudges before sending clearly unavailable premium work to the API.
    pub fn set_session_manager(&mut self, session: Arc<SessionManager>) {
        self.session = Some(session);
    }

    pub fn set_activation_client(&mut self, client: ContextStreamClient) {
        self.activation_client = Some(client);
    }

    /// Replace the legacy compatibility layer. Public builds install a no-op.
    pub fn set_atlas_layer(&mut self, layer: AtlasLayer) {
        self.atlas_layer = layer;
    }

    /// Borrow the legacy compatibility layer.
    pub fn atlas_layer(&self) -> &AtlasLayer {
        &self.atlas_layer
    }

    /// Replace the acceleration layer. Called once at startup by
    /// `mcp-server`; defaults to no-op for local stdio builds.
    pub fn set_acceleration_layer(&mut self, layer: AccelerationLayer) {
        self.acceleration_layer = layer;
    }

    /// Borrow the active MongoDB-free acceleration layer.
    pub fn acceleration_layer(&self) -> &AccelerationLayer {
        &self.acceleration_layer
    }

    /// Register a tool.
    pub fn register(&mut self, name: &str, handler: Arc<dyn ToolHandler>) {
        let metadata = handler.metadata().clone();
        let input_schema = handler.input_schema();
        let operation_metadata = metadata.clone();
        let operation_input_schema = input_schema.clone();
        let operation_handler = handler.clone();

        // Check toolset filtering
        if !self.toolsets.is_allowed(name) {
            return;
        }

        // Check progressive mode
        if self.progressive_mode && !self.is_in_enabled_bundle(name) {
            // Store for later enabling
            return;
        }

        // Check router mode - store non-direct tools in operations registry
        if self.router_mode && !is_router_direct_tool(name) {
            self.operations.insert(
                name.to_string(),
                RegisteredTool {
                    metadata: metadata.clone(),
                    handler: handler.clone(),
                    input_schema: input_schema.clone(),
                },
            );
            return;
        }

        // Check consolidated mode
        if self.consolidated_mode && !is_consolidated_tool(name) {
            return;
        }

        self.tools.insert(
            name.to_string(),
            RegisteredTool {
                metadata,
                handler,
                input_schema,
            },
        );

        if !is_openai_agentic_core_tool(name) {
            self.operations.insert(
                name.to_string(),
                RegisteredTool {
                    metadata: operation_metadata,
                    handler: operation_handler,
                    input_schema: operation_input_schema,
                },
            );
        }
    }

    /// Get a tool by name.
    pub fn get(&self, name: &str) -> Option<&RegisteredTool> {
        if self.is_tool_visible(name) {
            self.tools.get(name)
        } else {
            None
        }
    }

    /// List all registered tools.
    ///
    /// Back-compat aliases — registry keys that differ from the tool's
    /// canonical `metadata.name` (e.g. `atlas_chart` → `chart`,
    /// `atlas_job` → `async_job`) — are filtered out. Otherwise
    /// `tools/list` would surface the same canonical name multiple times,
    /// which strict MCP clients (Windsurf) treat as a fatal
    /// duplicate-tool registration error. Aliases stay callable via
    /// [`Self::get`] and [`Self::execute`].
    pub fn list(&self) -> Vec<&RegisteredTool> {
        self.tools
            .iter()
            .filter(|(name, tool)| {
                self.is_tool_visible(name) && name.as_str() == tool.metadata.name.as_str()
            })
            .map(|(_, tool)| tool)
            .collect()
    }

    /// List tool names. Like [`Self::list`], back-compat aliases are
    /// filtered out so each canonical name appears exactly once.
    pub fn names(&self) -> Vec<&str> {
        self.tools
            .iter()
            .filter(|(name, tool)| {
                self.is_tool_visible(name) && name.as_str() == tool.metadata.name.as_str()
            })
            .map(|(name, _)| name.as_str())
            .collect()
    }

    /// Execute a tool by name.
    pub async fn execute(&self, name: &str, input: Value) -> Result<ToolResult> {
        if !self.is_tool_visible(name) {
            return Err(Error::Tool(format!("Unknown tool: {}", name)));
        }
        let tool = self
            .tools
            .get(name)
            .ok_or_else(|| Error::Tool(format!("Unknown tool: {}", name)))?;

        if let Some(result) = self.plan_restriction_for_tool(&tool.metadata).await {
            return Ok(result);
        }

        // Attribute downstream server-side compliance events to the agent's
        // model: resolve it from the file-backed session model cache (warmed by
        // the hook layer) using the session id the tool carries, then scope it
        // so `ContextStreamClient` forwards `X-ContextStream-Model`.
        let model_id = input
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .and_then(mcp_session::session_model_cache::lookup);
        let observation =
            acceleration_observation_request(name, &input).map(|request| (request, Instant::now()));

        let outcome = match model_id {
            Some(model_id) => {
                mcp_client::run_with_model_id(model_id, || tool.handler.execute(input)).await
            }
            None => tool.handler.execute(input).await,
        };

        if let Some((request, started)) = observation {
            self.report_acceleration_observation(request, started.elapsed(), outcome.as_ref());
        }
        self.report_activation_outcome(name, tool.metadata.category.as_str(), outcome.as_ref())
            .await;
        outcome
    }

    fn report_acceleration_observation(
        &self,
        request: AccelerationObservationRequest,
        elapsed: Duration,
        outcome: std::result::Result<&ToolResult, &Error>,
    ) {
        let degraded = outcome_is_degraded(outcome);
        let outcome_label = if degraded { "degraded" } else { "ok" };
        let latency_ms = elapsed.as_millis().min(u128::from(u64::MAX)) as u64;
        let cache = cache_observation(request.cache_layer, outcome);

        metrics::histogram!(
            "mcp_tool_execution_latency_ms",
            "tool" => request.tool,
            "action" => request.action,
            "outcome" => outcome_label,
        )
        .record(latency_ms as f64);
        metrics::counter!(
            "mcp_tool_execution_total",
            "tool" => request.tool,
            "action" => request.action,
            "outcome" => outcome_label,
        )
        .increment(1);
        if let Some(cache) = cache {
            metrics::counter!(
                "mcp_tool_cache_total",
                "tool" => request.tool,
                "cache_layer" => cache.layer,
                "outcome" => if cache.hit { "hit" } else { "miss" },
            )
            .increment(1);
        }

        // Local/stdio builds use a no-op acceleration layer, so there is no
        // network or persistence cost. Remote gateways emit only the bounded
        // hot-read allowlist above, and the request is detached so telemetry
        // can never extend the caller's response latency.
        let Some(signals) = self.acceleration_layer.signals() else {
            return;
        };
        let session = self.session.clone();
        let explicit_workspace_id = request.workspace_id;
        let explicit_project_id = request.project_id;
        let tool = request.tool;
        let action = request.action;
        let cache_layer = cache.map(|observation| observation.layer);

        mcp_client::spawn_with_task_context(async move {
            let session_state = match session {
                Some(session) => Some(session.state().await),
                None => None,
            };
            let (workspace_id, project_id) = resolved_observation_scope(
                explicit_workspace_id,
                explicit_project_id,
                session_state.as_ref().and_then(|state| state.workspace_id),
                session_state.as_ref().and_then(|state| state.project_id),
            );
            let Some(workspace_id) = workspace_id else {
                return;
            };

            let kind = if cache.is_some() {
                AccelerationSignalKind::CacheHitMiss
            } else {
                AccelerationSignalKind::LatencySample
            };
            let mut event = AccelerationSignalEvent::with_scope(
                kind,
                workspace_id,
                project_id,
                serde_json::json!({
                    "source": "mcp_tool_registry",
                    "provider": "mcp_gateway",
                    "cache_layer": cache_layer,
                }),
            );
            event.tool = Some(tool.to_string());
            event.action = Some(action.to_string());
            event.cache_hit = cache.map(|observation| observation.hit);
            event.provider = Some("mcp_gateway".to_string());
            event.latency_ms = Some(latency_ms);
            event.degraded = Some(degraded);

            if let Err(error) = signals.emit(event).await {
                tracing::debug!(%error, "MCP acceleration observation skipped");
            }
        });
    }

    async fn report_activation_outcome(
        &self,
        name: &str,
        category: &str,
        outcome: std::result::Result<&ToolResult, &Error>,
    ) {
        let (Some(client), Some(session)) = (&self.activation_client, &self.session) else {
            return;
        };
        let state = session.state().await;
        match outcome {
            Ok(result) if !result.is_error && !tool_result_is_access_gate(result) => {
                client.spawn_first_mcp_action(
                    state.workspace_id,
                    state.project_id,
                    state.session_id.clone(),
                    name,
                    category,
                );
            }
            Err(error) => client.spawn_activation_failure(
                state.workspace_id,
                state.project_id,
                state.session_id.clone(),
                "tool_execution",
                error,
            ),
            _ => {}
        }
    }

    async fn plan_restriction_for_tool(&self, metadata: &ToolMetadata) -> Option<ToolResult> {
        let required = metadata
            .required_tier
            .as_deref()
            .or_else(|| metadata.is_pro.then_some("pro"))?;
        let session = self.session.as_ref()?;
        let state = session.state().await;
        let current_plan = state
            .account_context
            .as_ref()
            .and_then(|ctx| ctx.effective_plan.as_deref())?;

        if plan_allows(current_plan, required) {
            return None;
        }

        Some(ToolResult::plan_restricted(
            metadata.title.clone(),
            Some(current_plan),
            required,
            false,
        ))
    }

    /// Enable a bundle of tools (progressive mode).
    pub fn enable_bundle(&mut self, bundle: &str) {
        self.enabled_bundles.insert(bundle.to_string());
    }

    /// Check if a tool is in an enabled bundle.
    fn is_in_enabled_bundle(&self, name: &str) -> bool {
        // Core bundle is always enabled
        if CORE_BUNDLE.contains(&name) {
            return true;
        }

        for bundle in &self.enabled_bundles {
            if let Some(tools) = TOOL_BUNDLES.get(bundle.as_str()) {
                if tools.contains(&name) {
                    return true;
                }
            }
        }

        false
    }

    /// Get tool count.
    pub fn len(&self) -> usize {
        self.list().len()
    }

    /// Check if empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get enabled bundles.
    pub fn enabled_bundles(&self) -> Vec<String> {
        self.enabled_bundles.iter().cloned().collect()
    }

    /// List available bundles.
    pub fn available_bundles() -> Vec<(&'static str, &'static [&'static str])> {
        TOOL_BUNDLES
            .entries()
            .map(|(name, tools)| (*name, *tools))
            .collect()
    }

    /// Check if in progressive mode.
    pub fn is_progressive_mode(&self) -> bool {
        self.progressive_mode
    }

    /// Check if in router mode.
    pub fn is_router_mode(&self) -> bool {
        self.router_mode
    }

    /// Check if in consolidated mode.
    pub fn is_consolidated_mode(&self) -> bool {
        self.consolidated_mode
    }

    /// Get the active tool surface profile.
    pub fn tool_surface_profile(&self) -> ToolSurfaceProfile {
        *self
            .tool_surface_profile
            .read()
            .unwrap_or_else(|err| err.into_inner())
    }

    /// Update the active tool surface profile.
    pub fn set_tool_surface_profile(&self, profile: ToolSurfaceProfile) {
        let mut guard = self
            .tool_surface_profile
            .write()
            .unwrap_or_else(|err| err.into_inner());
        *guard = profile;
    }

    /// The surface profile this registry was constructed with (its baseline).
    pub fn default_tool_surface_profile(&self) -> ToolSurfaceProfile {
        self.default_tool_surface_profile
    }

    /// Apply a per-`initialize` surface decision deterministically.
    ///
    /// `detected` is whatever a single `initialize` request resolved to
    /// (explicit param/header or client-identity auto-detection). When it is
    /// `Some`, that profile is applied; when it is `None`, the registry falls
    /// back to its construction-time default instead of leaving whatever a
    /// previous client set in place. This prevents global state bleed on a
    /// shared multi-tenant process, where the registry is a long-lived
    /// `Arc<ToolRegistry>` and `tool_surface_profile` was otherwise only ever
    /// set, never reset.
    pub fn apply_initialize_surface_profile(&self, detected: Option<ToolSurfaceProfile>) {
        self.set_tool_surface_profile(detected.unwrap_or(self.default_tool_surface_profile));
    }

    /// Whether the current surface profile is OpenAI agentic mode.
    pub fn is_openai_agentic_surface(&self) -> bool {
        self.tool_surface_profile() == ToolSurfaceProfile::OpenaiAgentic
    }

    /// Whether discovery meta-tools should be exposed.
    pub fn should_expose_discovery_tools(&self) -> bool {
        self.router_mode || self.is_openai_agentic_surface()
    }

    // =========================================================================
    // Router Mode Operations
    // =========================================================================

    /// List all operations (router mode).
    pub fn list_operations(&self) -> Vec<&RegisteredTool> {
        self.operations.values().collect()
    }

    /// Get operation names (router mode).
    pub fn operation_names(&self) -> Vec<&str> {
        self.operations.keys().map(|s| s.as_str()).collect()
    }

    /// Get an operation by name (router mode).
    pub fn get_operation(&self, name: &str) -> Option<&RegisteredTool> {
        self.operations.get(name)
    }

    /// Execute an operation by name (router mode).
    pub async fn execute_operation(&self, name: &str, input: Value) -> Result<ToolResult> {
        let operation = self
            .operations
            .get(name)
            .ok_or_else(|| Error::Tool(format!("Unknown operation: {}", name)))?;

        if let Some(result) = self.plan_restriction_for_tool(&operation.metadata).await {
            return Ok(result);
        }

        let observation =
            acceleration_observation_request(name, &input).map(|request| (request, Instant::now()));
        let outcome = operation.handler.execute(input).await;
        if let Some((request, started)) = observation {
            self.report_acceleration_observation(request, started.elapsed(), outcome.as_ref());
        }
        self.report_activation_outcome(
            name,
            operation.metadata.category.as_str(),
            outcome.as_ref(),
        )
        .await;
        outcome
    }

    /// Get operation count (router mode).
    pub fn operation_count(&self) -> usize {
        self.operations.len()
    }

    /// Search visible tools and hidden operations for discovery.
    pub fn search_catalog(&self, query: &str, category: Option<&str>, limit: usize) -> Vec<Value> {
        let query = query.trim().to_ascii_lowercase();
        if query.is_empty() {
            return Vec::new();
        }

        let limit = limit.max(1).min(20);
        let mut candidates: Vec<(i64, Value)> = Vec::new();

        for (name, tool) in &self.tools {
            let direct = self.is_tool_visible(name);
            if let Some(value) = self.search_candidate(name, tool, category, direct, &query) {
                candidates.push(value);
            }
        }

        for (name, tool) in &self.operations {
            if self.tools.contains_key(name) {
                continue;
            }
            if let Some(value) = self.search_candidate(name, tool, category, false, &query) {
                candidates.push(value);
            }
        }

        candidates.sort_by(|a, b| {
            b.0.cmp(&a.0).then_with(|| {
                let a_name = a.1.get("name").and_then(|v| v.as_str()).unwrap_or_default();
                let b_name = b.1.get("name").and_then(|v| v.as_str()).unwrap_or_default();
                a_name.cmp(b_name)
            })
        });
        candidates
            .into_iter()
            .take(limit)
            .map(|(_, value)| value)
            .collect()
    }

    fn search_candidate(
        &self,
        name: &str,
        tool: &RegisteredTool,
        category: Option<&str>,
        direct: bool,
        query: &str,
    ) -> Option<(i64, Value)> {
        if let Some(cat) = category {
            if !tool.metadata.category.as_str().eq_ignore_ascii_case(cat) {
                return None;
            }
        }

        let name_l = name.to_ascii_lowercase();
        let title_l = tool.metadata.title.to_ascii_lowercase();
        let desc_l = tool.metadata.description.to_ascii_lowercase();
        let hints = discovery_hints(name, &tool.metadata);
        let mut score = 0_i64;

        if name_l == query {
            score += 120;
        }
        if title_l == query {
            score += 100;
        }
        if name_l.contains(query) {
            score += 70;
        }
        if title_l.contains(query) {
            score += 50;
        }
        if desc_l.contains(query) {
            score += 25;
        }
        if hints
            .aliases
            .iter()
            .any(|alias| alias.eq_ignore_ascii_case(query))
        {
            score += 80;
        }
        if hints.tags.iter().any(|tag| tag.eq_ignore_ascii_case(query)) {
            score += 45;
        }

        for token in query.split_whitespace() {
            if token.is_empty() {
                continue;
            }
            if name_l.split('_').any(|part| part == token) {
                score += 18;
            } else if name_l.contains(token) {
                score += 10;
            }
            if title_l.contains(token) {
                score += 8;
            }
            if desc_l.contains(token) {
                score += 4;
            }
            if hints
                .aliases
                .iter()
                .any(|alias| alias.to_ascii_lowercase().contains(token))
            {
                score += 14;
            }
            if hints
                .tags
                .iter()
                .any(|tag| tag.to_ascii_lowercase().contains(token))
            {
                score += 10;
            }
            if hints.when_to_use.to_ascii_lowercase().contains(token) {
                score += 6;
            }
            if hints
                .examples
                .iter()
                .any(|example| example.to_ascii_lowercase().contains(token))
            {
                score += 6;
            }
        }

        if score <= 0 {
            return None;
        }

        Some((
            score,
            serde_json::json!({
                "name": tool.metadata.name,
                "title": tool.metadata.title,
                "description": tool.metadata.description,
                "category": tool.metadata.category.as_str(),
                "call_mode": if direct { "direct" } else { "execute_operation" },
                "aliases": hints.aliases,
                "tags": hints.tags,
                "when_to_use": hints.when_to_use,
                "avoid_when": hints.avoid_when,
                "examples": hints.examples,
                "latency_class": hints.latency_class,
                "parallel_safe": hints.parallel_safe,
                "batch_safe": hints.batch_safe,
                "annotations": {
                    "read_only": tool.metadata.annotations.read_only,
                    "destructive": tool.metadata.annotations.destructive,
                    "requires_confirmation": tool.metadata.annotations.requires_confirmation,
                    "idempotent": tool.metadata.annotations.idempotent,
                    "long_running": tool.metadata.annotations.long_running
                },
                "inputSchema": tool.input_schema,
                "score": score
            }),
        ))
    }

    /// Save bundle state to file.
    pub fn save_bundle_state(&self) -> std::result::Result<(), std::io::Error> {
        let state_path = bundle_state_path();
        if let Some(parent) = state_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let state = BundleState {
            enabled_bundles: self.enabled_bundles.iter().cloned().collect(),
        };
        let content = serde_json::to_string_pretty(&state).map_err(std::io::Error::other)?;
        std::fs::write(&state_path, content)
    }

    /// Load bundle state from file.
    pub fn load_bundle_state(&mut self) -> std::result::Result<(), std::io::Error> {
        let state_path = bundle_state_path();
        if !state_path.exists() {
            return Ok(());
        }
        let content = std::fs::read_to_string(&state_path)?;
        let state: BundleState = serde_json::from_str(&content).map_err(std::io::Error::other)?;
        for bundle in state.enabled_bundles {
            self.enabled_bundles.insert(bundle);
        }
        Ok(())
    }

    fn is_tool_visible(&self, name: &str) -> bool {
        if !self.tools.contains_key(name) {
            return false;
        }
        match self.tool_surface_profile() {
            ToolSurfaceProfile::Default => true,
            ToolSurfaceProfile::OpenaiAgentic => is_openai_agentic_core_tool(name),
        }
    }
}

#[cfg(test)]
mod acceleration_observation_tests {
    use super::{
        acceleration_observation_action, acceleration_observation_request, cache_observation,
        request_scope, resolved_observation_scope, AccelerationObservationRequest,
        CacheObservation, ToolRegistry,
    };
    use async_trait::async_trait;
    use mcp_types::{
        acceleration_layer::{
            AccelerationSignalError, AccelerationSignalEvent, McpAccelerationLayer, SignalProvider,
        },
        tool::ToolResult,
        Config, Error,
    };
    use serde_json::json;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::sync::mpsc;
    use uuid::Uuid;

    struct RecordingSignalProvider {
        sender: mpsc::UnboundedSender<AccelerationSignalEvent>,
    }

    #[async_trait]
    impl SignalProvider for RecordingSignalProvider {
        async fn emit(
            &self,
            event: AccelerationSignalEvent,
        ) -> Result<(), AccelerationSignalError> {
            self.sender
                .send(event)
                .map_err(|error| AccelerationSignalError::Unavailable(error.to_string()))
        }
    }

    struct RecordingAccelerationLayer {
        provider: Arc<dyn SignalProvider>,
    }

    impl McpAccelerationLayer for RecordingAccelerationLayer {
        fn is_enabled(&self) -> bool {
            true
        }

        fn has_connection(&self) -> bool {
            true
        }

        fn signals(&self) -> Option<Arc<dyn SignalProvider>> {
            Some(self.provider.clone())
        }
    }

    #[test]
    fn action_labels_are_allowlisted_and_mutations_are_not_observed() {
        assert_eq!(
            acceleration_observation_action("search", &json!({"mode": "guided"})),
            Some("guided")
        );
        assert_eq!(
            acceleration_observation_action("search", &json!({"mode": "attacker-label"})),
            Some("auto")
        );
        assert_eq!(
            acceleration_observation_action("memory", &json!({"action": "list_tasks"})),
            Some("list_tasks")
        );
        assert_eq!(
            acceleration_observation_action("memory", &json!({"action": "list_transcripts"})),
            Some("list_transcripts")
        );
        assert_eq!(
            acceleration_observation_action("memory", &json!({"action": "list_nodes"})),
            Some("list_nodes")
        );
        assert_eq!(
            acceleration_observation_action("memory", &json!({"action": "list_diagrams"})),
            Some("list_diagrams")
        );
        assert_eq!(
            acceleration_observation_action("memory", &json!({"action": "create_task"})),
            None
        );
        assert_eq!(
            acceleration_observation_action("project", &json!({"action": "index"})),
            None
        );
        assert_eq!(
            acceleration_observation_request(
                "search",
                &json!({"code_rerank_learning_opt_in": true})
            )
            .and_then(|request| request.cache_layer),
            None
        );
    }

    #[test]
    fn cache_markers_report_hits_without_exposing_cache_keys() {
        let result = ToolResult::with_structured(
            "[SEARCH_CACHED] returning cached result",
            json!({"results": []}),
        );
        assert_eq!(
            cache_observation(Some("mcp_search_result_cache"), Ok(&result)),
            Some(CacheObservation {
                hit: true,
                layer: "mcp_search_result_cache",
            })
        );

        let result = ToolResult::with_structured(
            "[RECALL_CACHED] returning cached result",
            json!({"items": []}),
        );
        assert_eq!(
            cache_observation(Some("mcp_recall_result_cache"), Ok(&result)),
            Some(CacheObservation {
                hit: true,
                layer: "mcp_recall_result_cache",
            })
        );

        let code_result = ToolResult::with_structured(
            "Found source containing `[SEARCH_CACHED]`, but this is a fresh result.",
            json!({"results": []}),
        );
        assert_eq!(
            cache_observation(Some("mcp_search_result_cache"), Ok(&code_result)),
            Some(CacheObservation {
                hit: false,
                layer: "mcp_search_result_cache",
            })
        );

        let guided = ToolResult::with_structured(
            "Evidence first.\n[SEARCH_CACHED] Reused the previous identical guided result (<30s old).",
            json!({"results": []}),
        );
        assert_eq!(
            cache_observation(Some("mcp_search_result_cache"), Ok(&guided)),
            Some(CacheObservation {
                hit: true,
                layer: "mcp_search_result_cache",
            })
        );
    }

    #[test]
    fn structured_cache_provenance_wins_over_marker_inference() {
        let hit = ToolResult::with_structured(
            "cached",
            json!({
                "cache_hit": true,
                "served_from": "regional_warm_cache"
            }),
        );
        assert_eq!(
            cache_observation(None, Ok(&hit)),
            Some(CacheObservation {
                hit: true,
                layer: "regional_warm_cache",
            })
        );

        let miss = ToolResult::with_structured(
            "primary",
            json!({
                "cache_hit": false,
                "served_from": "primary_server"
            }),
        );
        assert_eq!(
            cache_observation(None, Ok(&miss)),
            Some(CacheObservation {
                hit: false,
                layer: "regional_warm_cache",
            })
        );
    }

    #[test]
    fn misses_are_counted_only_for_successful_cacheable_calls() {
        let success = ToolResult::with_structured("fresh", json!({"results": []}));
        assert_eq!(
            cache_observation(Some("mcp_memory_result_cache"), Ok(&success)),
            Some(CacheObservation {
                hit: false,
                layer: "mcp_memory_result_cache",
            })
        );
        assert_eq!(cache_observation(None, Ok(&success)), None);

        let error = Error::Tool("transport failed".to_string());
        assert_eq!(
            cache_observation(Some("mcp_search_result_cache"), Err(&error)),
            None
        );
    }

    #[test]
    fn explicit_scope_parser_accepts_only_uuid_strings() {
        let workspace_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        assert_eq!(
            request_scope(&json!({
                "workspace_id": workspace_id,
                "project_id": project_id,
            })),
            (Some(workspace_id), Some(project_id))
        );
        assert_eq!(
            request_scope(&json!({
                "workspace_id": "not-a-uuid",
                "project_id": 42,
            })),
            (None, None)
        );
    }

    #[test]
    fn explicit_workspace_never_inherits_a_different_sessions_project() {
        let explicit_workspace_id = Uuid::new_v4();
        let session_workspace_id = Uuid::new_v4();
        let session_project_id = Uuid::new_v4();

        assert_eq!(
            resolved_observation_scope(
                Some(explicit_workspace_id),
                None,
                Some(session_workspace_id),
                Some(session_project_id),
            ),
            (Some(explicit_workspace_id), None)
        );
        assert_eq!(
            resolved_observation_scope(
                None,
                None,
                Some(session_workspace_id),
                Some(session_project_id),
            ),
            (Some(session_workspace_id), Some(session_project_id))
        );
    }

    #[tokio::test]
    async fn observed_hot_read_emits_detached_scoped_rollup_event() {
        let (sender, mut receiver) = mpsc::unbounded_channel();
        let layer = Arc::new(RecordingAccelerationLayer {
            provider: Arc::new(RecordingSignalProvider { sender }),
        });
        let mut registry = ToolRegistry::new(&Config::default());
        registry.set_acceleration_layer(layer);

        let workspace_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        let request = AccelerationObservationRequest {
            tool: "search",
            action: "hybrid",
            cache_layer: Some("mcp_search_result_cache"),
            workspace_id: Some(workspace_id),
            project_id: Some(project_id),
        };
        let result = ToolResult::with_structured(
            "[SEARCH_CACHED] returning cached result",
            json!({"results": []}),
        );

        registry.report_acceleration_observation(request, Duration::from_millis(17), Ok(&result));

        let event = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .expect("detached telemetry should complete")
            .expect("recording provider should receive an event");
        assert_eq!(event.workspace_id, Some(workspace_id));
        assert_eq!(event.project_id, Some(project_id));
        assert_eq!(event.tool.as_deref(), Some("search"));
        assert_eq!(event.action.as_deref(), Some("hybrid"));
        assert_eq!(event.cache_hit, Some(true));
        assert_eq!(event.latency_ms, Some(17));
        assert_eq!(event.provider.as_deref(), Some("mcp_gateway"));
        assert_eq!(event.metadata["source"], "mcp_tool_registry");
        assert_eq!(event.metadata["cache_layer"], "mcp_search_result_cache");
    }
}

#[cfg(test)]
mod activation_tests {
    use super::tool_result_is_access_gate;
    use mcp_types::tool::ToolResult;

    #[test]
    fn access_gate_detection_excludes_false_successes() {
        assert!(tool_result_is_access_gate(&ToolResult::text(
            "[SETUP_REQUIRED] Run setup first"
        )));
        assert!(tool_result_is_access_gate(&ToolResult::text(
            "Authentication required before this action"
        )));
        assert!(tool_result_is_access_gate(&ToolResult::text(
            "Session initialized without a resolved workspace_id.\n\
             No usable workspace_id was returned."
        )));
        assert!(!tool_result_is_access_gate(&ToolResult::text(
            "Found 12 relevant code results"
        )));
    }
}

fn plan_rank(plan: &str) -> i32 {
    match plan.to_ascii_lowercase().as_str() {
        "free" => 0,
        "starter" => 1,
        "pro" | "lite" => 2,
        "elite" | "full" | "semantic" => 3,
        "team" => 4,
        "enterprise" => 5,
        _ => -1,
    }
}

fn plan_allows(current_plan: &str, required_tier: &str) -> bool {
    plan_rank(current_plan) >= plan_rank(required_tier)
}

/// Bundle state for persistence.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct BundleState {
    enabled_bundles: Vec<String>,
}

/// Get the path for bundle state file.
fn bundle_state_path() -> std::path::PathBuf {
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        std::path::PathBuf::from(home)
            .join(".claude")
            .join("bundle_state.json")
    } else {
        std::path::PathBuf::from(".claude").join("bundle_state.json")
    }
}

/// Toolset configuration for filtering tools.
struct ToolsetConfig {
    toolset: Toolset,
    allowlist: Option<HashSet<String>>,
}

impl ToolsetConfig {
    fn new(toolset: Toolset) -> Self {
        Self {
            toolset,
            allowlist: None,
        }
    }

    fn is_allowed(&self, name: &str) -> bool {
        // Check explicit allowlist first
        if let Some(ref list) = self.allowlist {
            return list.contains(name);
        }

        // Check toolset
        match self.toolset {
            Toolset::Complete => true,
            Toolset::Standard => LIGHT_TOOLS.contains(&name) || STANDARD_TOOLS.contains(&name),
            Toolset::Light => LIGHT_TOOLS.contains(&name),
        }
    }
}

/// Check if tool is a router direct tool (exposed in router mode).
fn is_router_direct_tool(name: &str) -> bool {
    ROUTER_DIRECT_TOOLS.contains(&name)
}

/// Check if tool is a consolidated domain tool.
fn is_consolidated_tool(name: &str) -> bool {
    CONSOLIDATED_TOOLS.contains(&name)
}

/// Check if tool is exposed directly in the OpenAI agentic surface.
fn is_openai_agentic_core_tool(name: &str) -> bool {
    OPENAI_AGENTIC_CORE_TOOLS.contains(&name)
}

fn discovery_hints(name: &str, metadata: &ToolMetadata) -> DiscoveryHints {
    match name {
        "init" => DiscoveryHints {
            aliases: &["bootstrap", "setup session", "initialize contextstream"],
            tags: &["startup", "session", "workspace", "indexing"],
            when_to_use: "First call in a new session to resolve workspace/project state and bootstrap indexing context.",
            avoid_when: "Do not use for ordinary turn-by-turn retrieval after the session is already initialized.",
            examples: &[
                "initialize context for this repo",
                "start a new ContextStream session for this folder",
            ],
            latency_class: "medium",
            parallel_safe: false,
            batch_safe: false,
        },
        "context" => DiscoveryHints {
            aliases: &["context pack", "load context", "refresh context"],
            tags: &["guidance", "lessons", "preferences", "routing"],
            when_to_use: "Primary pre-work retrieval for instructions, lessons, preferences, and task-specific context.",
            avoid_when: "Do not use as a generic search substitute when you need exact code or file matches.",
            examples: &[
                "get context before editing auth code",
                "refresh context for this user request",
            ],
            latency_class: "medium",
            parallel_safe: false,
            batch_safe: false,
        },
        "instruct" => DiscoveryHints {
            aliases: &["instructions cache", "session instructions", "flash memory"],
            tags: &["cache", "session", "instruction", "hot state"],
            when_to_use: "Read or acknowledge session-scoped instruction entries between turns.",
            avoid_when: "Do not use for durable notes or docs; use memory/session capture instead.",
            examples: &[
                "load session instructions",
                "acknowledge consumed instruction entries",
            ],
            latency_class: "fast",
            parallel_safe: true,
            batch_safe: false,
        },
        "search" => DiscoveryHints {
            aliases: &["code search", "find code", "lookup symbol"],
            tags: &["code", "files", "semantic", "keyword", "refactor"],
            when_to_use: "Primary tool for code/file discovery, symbol lookup, semantic code search, and exhaustive usage scans.",
            avoid_when: "Do not use for memory-only queries, plans, or docs retrieval unless you explicitly need indexed code results.",
            examples: &[
                "find where UserService is used",
                "search code for route definition",
            ],
            latency_class: "fast",
            parallel_safe: true,
            batch_safe: true,
        },
        "memory" => DiscoveryHints {
            aliases: &[
                "notes",
                "docs",
                "decisions",
                "memory search",
                "AI memory",
                "my memories",
                "recent memories",
                "preferences",
                "saved preferences",
                "lessons",
                "saved lessons",
            ],
            tags: &[
                "knowledge",
                "docs",
                "decisions",
                "preferences",
                "tasks",
                "context",
                "memories",
            ],
            when_to_use: "Query or mutate durable knowledge such as docs, decisions, notes, todos, and tasks. Also use when user asks about memories, context, or past decisions.",
            avoid_when: "Do not use for codebase search; use search for code and files.",
            examples: &[
                "list docs about deployment",
                "search memory for decisions about auth",
                "search memory for saved preferences",
                "find lessons from prior mistakes",
                "show me my recent memories",
                "what decisions have we made",
                "list my AI memories",
            ],
            latency_class: "fast",
            parallel_safe: true,
            batch_safe: true,
        },
        "capture_plan" => DiscoveryHints {
            aliases: &["save plan", "capture plan", "store plan", "persist plan"],
            tags: &["session", "plan", "write", "save", "tasks"],
            when_to_use: "Save the canonical implementation plan to ContextStream and create linked tasks by default.",
            avoid_when: "Do not use for generic notes, docs, or tickets.",
            examples: &[
                "save this implementation plan",
                "capture plan and create linked tasks",
                "persist the plan to ContextStream",
            ],
            latency_class: "medium",
            parallel_safe: false,
            batch_safe: false,
        },
        "session_capture_lesson" => DiscoveryHints {
            aliases: &["save lesson", "capture lesson", "lesson learned", "remember lesson"],
            tags: &["session", "lessons", "write", "save", "memory"],
            when_to_use: "Persist a lesson learned from a correction, mistake, or workflow insight so it resurfaces later.",
            avoid_when: "Do not use for general code search or ordinary notes without a lesson/prevention pattern.",
            examples: &[
                "save lesson learned about test verification",
                "capture this mistake so it does not happen again",
            ],
            latency_class: "fast",
            parallel_safe: false,
            batch_safe: false,
        },
        "session_remember" => DiscoveryHints {
            aliases: &["remember", "save memory", "save preference", "remember preference"],
            tags: &["session", "memory", "preferences", "remember", "write", "save"],
            when_to_use: "Quick-save important context, user preferences, or durable memory for future sessions.",
            avoid_when: "Do not use for structured lessons; use session_capture_lesson when prevention/impact matters.",
            examples: &[
                "remember this preference",
                "save this note for next time",
                "remember that the user prefers concise output",
            ],
            latency_class: "fast",
            parallel_safe: false,
            batch_safe: false,
        },
        "session" => DiscoveryHints {
            aliases: &[
                "lessons",
                "recall",
                "capture",
                "plan management",
                "remember",
                "save memory",
                "save plan",
                "save lesson",
                "save preference",
                "show context",
            ],
            tags: &[
                "session",
                "lessons",
                "recall",
                "capture",
                "plan",
                "remember",
                "context",
                "memory",
            ],
            when_to_use: "Capture lessons/decisions, recall prior context, remember things for future sessions, or manage session-level plans.",
            avoid_when: "Do not use for exact code/file discovery or workspace administration.",
            examples: &[
                "get lessons for migrations",
                "capture a decision from this conversation",
                "remember this for next time",
                "show me recent context",
                "what do you remember about this project",
            ],
            latency_class: "fast",
            parallel_safe: true,
            batch_safe: false,
        },
        "project" => DiscoveryHints {
            aliases: &["index project", "ingest repo", "project status"],
            tags: &["project", "indexing", "repo", "status"],
            when_to_use: "Inspect project scope, refresh indexing, or resolve local repo/project associations.",
            avoid_when: "Do not use for workspace-level membership/admin work.",
            examples: &[
                "check whether this repo is indexed",
                "refresh the project index for this folder",
            ],
            latency_class: "medium",
            parallel_safe: true,
            batch_safe: false,
        },
        "workspace" => DiscoveryHints {
            aliases: &["workspace admin", "workspace info", "team context"],
            tags: &["workspace", "membership", "association", "bootstrap"],
            when_to_use: "List or manage workspaces and associate folders with workspace scope.",
            avoid_when: "Do not use when a project-scoped tool already answers the question.",
            examples: &[
                "list available workspaces",
                "associate this folder with a workspace",
            ],
            latency_class: "fast",
            parallel_safe: true,
            batch_safe: true,
        },
        "help" => DiscoveryHints {
            aliases: &["tools help", "version", "auth status"],
            tags: &["help", "metadata", "version", "bundles"],
            when_to_use: "Inspect available tools, auth state, version info, bundles, and generated rules.",
            avoid_when: "Do not use to execute business operations directly.",
            examples: &[
                "show available tools",
                "check MCP version and auth status",
            ],
            latency_class: "fast",
            parallel_safe: true,
            batch_safe: true,
        },
        "integration" => DiscoveryHints {
            aliases: &["integrations", "github", "slack", "notion", "team activity"],
            tags: &["integration", "external", "github", "slack", "notion"],
            when_to_use: "Query connected integrations, team activity, provider-specific resources, or external knowledge synced into ContextStream.",
            avoid_when: "Do not use for core codebase search or local project indexing operations.",
            examples: &[
                "check connected integrations for this workspace",
                "search GitHub issues through the integration tool",
            ],
            latency_class: "medium",
            parallel_safe: true,
            batch_safe: true,
        },
        "skill" => DiscoveryHints {
            aliases: &["skills", "skill locker", "reusable instructions", "run skill"],
            tags: &["skill", "instructions", "actions", "portable", "import"],
            when_to_use: "Manage and execute reusable skills (instruction + action bundles). Import skills from other tools, create custom skills, execute skill actions.",
            avoid_when: "Do not use for one-off instructions or session-scoped memory — use session capture or flash for those.",
            examples: &[
                "list available skills",
                "run the deploy-checker skill",
                "import skills from CLAUDE.md",
            ],
            latency_class: "fast",
            parallel_safe: true,
            batch_safe: true,
        },
        "tool_search" => DiscoveryHints {
            aliases: &["find tool", "discover operation", "tool lookup"],
            tags: &["discovery", "routing", "tooling"],
            when_to_use: "Discover the best direct tool or hidden operation when the capability is unclear.",
            avoid_when: "Do not use when you already know the exact tool to call.",
            examples: &[
                "find the best tool for integration sync status",
                "search for a tool to list decisions",
            ],
            latency_class: "fast",
            parallel_safe: true,
            batch_safe: true,
        },
        "graph" => DiscoveryHints {
            aliases: &[
                "code health",
                "dependency analysis",
                "quality dashboard",
                "circular dependencies",
                "unused code",
                "complexity metrics",
                "scan history",
            ],
            tags: &[
                "graph",
                "dependencies",
                "code-health",
                "quality",
                "dashboard",
                "recommendations",
            ],
            when_to_use: "Use for structural code graph analysis, dependency blast radius, Code Health dashboard data, saved scan history, trends, freshness, and recommendation-oriented quality summaries.",
            avoid_when: "Do not use for keyword/content search in files; use search for code text and memory/session for durable knowledge.",
            examples: &[
                "show code health trends for this project",
                "find circular dependencies and recommend what to fix first",
                "get dependency blast radius for this module",
                "summarize saved quality scan history",
            ],
            latency_class: "variable",
            parallel_safe: true,
            batch_safe: true,
        },
        "execute_operation" => DiscoveryHints {
            aliases: &["run operation", "deferred tool call", "hidden operation"],
            tags: &["operations", "deferred", "router"],
            when_to_use: "Execute a hidden or deferred capability returned by tool_search or operations listing.",
            avoid_when: "Do not use blindly; pick a named operation first via tool_search or operations.",
            examples: &[
                "execute an operation returned by tool_search",
                "run a hidden integration operation by name",
            ],
            latency_class: "variable",
            parallel_safe: false,
            batch_safe: false,
        },
        _ => DiscoveryHints {
            aliases: &[],
            tags: default_category_tags(metadata.category),
            when_to_use: "Use when this tool best matches the requested capability.",
            avoid_when: "Avoid when a more specific tool or code search path exists.",
            examples: &[],
            latency_class: if metadata.annotations.long_running {
                "slow"
            } else {
                "fast"
            },
            parallel_safe: metadata.annotations.read_only && !metadata.annotations.long_running,
            batch_safe: metadata.annotations.read_only && metadata.annotations.idempotent,
        },
    }
}

fn default_category_tags(category: ToolCategory) -> &'static [&'static str] {
    match category {
        ToolCategory::Session => &["session"],
        ToolCategory::Search => &["search", "code"],
        ToolCategory::Memory => &["memory", "knowledge"],
        ToolCategory::Graph => &["graph", "dependencies"],
        ToolCategory::Workspace => &["workspace"],
        ToolCategory::Project => &["project", "indexing"],
        ToolCategory::Ai => &["ai"],
        ToolCategory::Reminders => &["reminders"],
        ToolCategory::Integrations => &["integrations"],
        ToolCategory::Utility => &["utility"],
    }
}

// Tool sets

/// Light toolset (core tools only)
const LIGHT_TOOLS: &[&str] = &[
    // Session
    "init",
    "context",
    "session",
    "session_capture",
    "session_recall",
    // Search
    "search",
    // Memory
    "memory",
    "memory_search",
    "memory_decisions",
    // Graph
    "graph",
    "graph_related",
    "graph_decisions",
    // Project
    "project",
    "projects_list",
    "projects_create",
    "projects_ingest_local",
    // Workspace
    "workspace",
    "workspaces_list",
    // Utility
    "help",
    "generate_rules",
    "generate_editor_rules",
    // Media
    "media",
];

/// Standard toolset additions
const STANDARD_TOOLS: &[&str] = &[
    // Workspace management
    "workspaces_create",
    "workspace_bootstrap",
    "workspace_associate",
    // Reminders
    "reminder",
    "reminders_create",
    "reminders_list",
    "reminders_active",
    "coordination",
    // Integrations
    "integration",
    // Media
    "media",
    // Instruction cache (canonical name; `ram` and `mem` aliases were
    // dropped in v0.3.2)
    "instruct",
    // Skills
    "skill",
    // Consolidated advanced assistants
    "ingest",
    "ai",
    "capsule",
    // Agent Q&A surface (added v0.3.5 — was registered but absent
    // from any toolset filter, so tools/list never surfaced it).
    "qa",
    // Structured entities — tickets, handoffs, incidents, releases,
    // experiments, goals, sprints, reviews, risks (added v0.3.5).
    "entity",
    // VCS bridge — pulls, issues, activity, notifications across
    // linked repositories (added v0.3.5).
    "vcs",
    // Workspace dashboard charts (premium / hosted deployments only —
    // only registered when the premium layer is connected). The
    // `atlas_chart` alias is kept for one minor cycle for back-compat.
    "chart",
    "atlas_chart",
    // Async export jobs (premium / hosted deployments only). The
    // `atlas_job` alias is kept for one minor cycle.
    "async_job",
    "atlas_job",
    // Per-action write surfaces. Surface them in the Standard
    // toolset so MCP clients that display only the tool name
    // (opencode, Cursor, Codex, Claude Code) can render
    // `contextstream_memory_update_doc` instead of the generic
    // `contextstream_memory`. They dispatch to the same handlers as
    // the corresponding `session(action=...)` / `memory(action=...)`
    // calls, so model prompts can still go either route.
    "capture_plan",
    "session_capture_lesson",
    "session_remember",
    "memory_create_doc",
    "memory_update_doc",
    "memory_delete_doc",
    "memory_create_task",
    "memory_update_task",
    "memory_create_todo",
    "memory_complete_todo",
    "memory_create_event",
];

/// Compact default tools for OpenAI/GPT agentic tool calling.
const OPENAI_AGENTIC_CORE_TOOLS: &[&str] = &[
    "init",
    "context",
    "session",
    "instruct",
    "search",
    "memory",
    "media",
    // Capsule creation must remain directly reachable on compact OpenAI/Codex
    // surfaces. Hiding it behind tool_search caused agents to invent a
    // "design-spec wrapped in a handoff" substitute instead of returning the
    // capsule link the user explicitly requested.
    "capsule",
    "project",
    "workspace",
    "help",
];

/// Tools exposed in router mode
const ROUTER_DIRECT_TOOLS: &[&str] = &["operations", "execute_operation"];

/// Consolidated domain tools (v0.4.x default)
const CONSOLIDATED_TOOLS: &[&str] = &[
    "init",
    "context",
    "session",
    "instruct",
    "search",
    "memory",
    "graph",
    "workspace",
    "project",
    "ingest",
    "ai",
    "integration",
    "reminder",
    "coordination",
    "media",
    "skill",
    "capsule",
    // Agent Q&A surface (added v0.3.5).
    "qa",
    // Structured entities (added v0.3.5).
    "entity",
    // VCS bridge (added v0.3.5).
    "vcs",
    "help",
    "generate_rules",
    "generate_editor_rules",
    // Workspace dashboard charts (premium / hosted only).
    "chart",
    "atlas_chart",
    // Async export jobs (premium / hosted only).
    "async_job",
    "atlas_job",
    // Per-action write surfaces. Surface them so MCP clients that
    // display only the tool name (opencode, Cursor, Codex, Claude Code)
    // can render `contextstream_capture_plan` instead of the generic
    // `contextstream_session`. They dispatch to the same handlers as
    // the corresponding `session(action=...)` / `memory(action=...)`
    // calls, so model prompts can still go either route.
    "capture_plan",
    "session_capture",
    "session_capture_lesson",
    "session_remember",
    "memory_create_doc",
    "memory_update_doc",
    "memory_delete_doc",
    "memory_create_task",
    "memory_update_task",
    "memory_create_todo",
    "memory_complete_todo",
    "memory_create_event",
];

/// Core bundle (always enabled in progressive mode)
const CORE_BUNDLE: &[&str] = &[
    "init",
    "context",
    "session",
    "instruct",
    "search",
    "help",
    "generate_rules",
    "memory",
    "graph",
    "media",
];

/// Tool bundles for progressive disclosure
static TOOL_BUNDLES: phf::Map<&'static str, &'static [&'static str]> = phf::phf_map! {
    "core" => &["init", "context", "session", "search", "help", "generate_rules", "media", "coordination"],
    "session" => &["session_capture", "session_recall", "session_compress", "instruct"],
    "memory" => &["memory", "memory_search", "memory_decisions", "memory_timeline", "memory_create_node"],
    "search" => &["search", "search_semantic", "search_hybrid", "search_keyword"],
    "graph" => &["graph", "graph_dependencies", "graph_impact", "graph_related"],
    "workspace" => &["workspace", "workspaces_list", "workspaces_create", "workspace_bootstrap"],
    "project" => &["project", "projects_list", "projects_create", "projects_ingest_local"],
    "media" => &["media"],
    "reminders" => &["reminders", "reminders_create", "reminders_list", "reminders_active"],
    "integrations" => &["github", "slack", "notion", "linear", "jira", "figma"],
};

#[cfg(test)]
#[path = "registry_tests.rs"]
mod tests;
