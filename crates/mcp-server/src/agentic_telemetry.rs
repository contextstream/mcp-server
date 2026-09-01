//! Best-effort runtime telemetry for agentic MCP flows.
//!
//! This module records discovery, deferred execution, and batch usage into the
//! compliance analytics pipeline without blocking tool execution.

use chrono::Utc;
use mcp_client::ContextStreamClient;
use mcp_session::SessionManager;
use mcp_types::{
    agentic::{
        analytics_surface_profile, ComplianceEventRequest, META_ACTUAL_TOOL, META_BATCH_COUNT,
        META_CANDIDATE_COUNT, META_CASE_ID, META_EXPECTED_TOOL, META_FIRST_TOOL_CORRECT,
        META_HIDDEN_TOOL_REQUESTED, META_LATENCY_MS, META_OPERATION_NAME, META_REQUEST_ID,
        META_RESULT_COUNT, META_RETRY_COUNT, META_RUNTIME_ID, META_SELECTED_RANK, META_TOOL_NAME,
        META_TOOL_SURFACE_PROFILE, META_TOOL_YIELD_COUNT, META_TURN_ID,
        RULE_AGENTIC_BATCH_OPERATIONS_USED, RULE_AGENTIC_EXECUTE_OPERATION_USED,
        RULE_AGENTIC_HIDDEN_DIRECT_CALL_BLOCKED, RULE_AGENTIC_TOOL_RETRY,
        RULE_AGENTIC_TOOL_SEARCH_USED, RUNTIME_CONTEXTSTREAM_MCP,
    },
    config::{ToolSurfaceProfile, VERSION},
    tool::ToolResult,
    HarnessId, HarnessReadinessEvidence, HarnessReadinessStage, ReadinessEvidenceSource,
    ReadinessEvidenceStatus, SessionKey,
};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::debug;
use uuid::Uuid;

const TURN_IDLE_TIMEOUT: Duration = Duration::from_secs(45);
const HINT_IDLE_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const MAX_HINT_PARTITIONS: usize = 512;

#[derive(Debug, Clone)]
struct RuntimeHints {
    harness_id: Option<HarnessId>,
    managed_identity: Option<ManagedHarnessRuntimeIdentity>,
    model_id: Option<String>,
    last_activity: Instant,
    readiness_observed_at: HashMap<RuntimeReadinessKind, chrono::DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedHarnessRuntimeIdentity {
    pub installation_id: Uuid,
    pub harness_id: HarnessId,
    pub managed_config_version: String,
    pub teaching_version: String,
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
enum RuntimeReadinessKind {
    Connected,
    GroundedInit,
    GroundedContext,
    PracticingSearch,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct TelemetryPartition {
    caller: Option<SessionKey>,
    mcp_session: Option<SessionKey>,
}

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct TurnStorageKey {
    partition: TelemetryPartition,
    turn_scope: String,
}

#[derive(Debug, Clone)]
struct ActiveTurn {
    turn_id: String,
    request_id: Uuid,
    last_activity: Instant,
    tool_yield_count: u32,
    attempts_by_action: HashMap<String, u32>,
    total_retry_count: u32,
    last_emitted_retry_count: u32,
    first_tool: Option<String>,
}

#[derive(Debug, Default)]
struct RuntimeState {
    hints: HashMap<TelemetryPartition, RuntimeHints>,
    turns: HashMap<TurnStorageKey, ActiveTurn>,
    turn_counters: HashMap<TelemetryPartition, u64>,
}

#[derive(Debug, Clone)]
struct TurnSnapshot {
    workspace_id: Option<Uuid>,
    project_id: Option<Uuid>,
    request_id: Uuid,
    turn_id: String,
    tool_yield_count: u32,
    retry_count: u32,
    emit_retry_event: bool,
    first_tool: Option<String>,
    harness_id: HarnessId,
    model_id: String,
}

#[derive(Debug, Clone, Default)]
pub struct AgenticTelemetryInput {
    pub turn_id: Option<String>,
    pub request_id: Option<String>,
    pub selected_rank: Option<i64>,
    pub retry_count: Option<u32>,
    pub case_id: Option<String>,
    pub expected_tool: Option<String>,
}

impl AgenticTelemetryInput {
    pub fn from_arguments(arguments: &Value) -> Self {
        Self {
            turn_id: get_bounded_token(arguments, "turn_id"),
            request_id: get_bounded_token(arguments, "request_id"),
            selected_rank: get_i64(arguments, "selected_rank"),
            retry_count: get_u32(arguments, "retry_count"),
            case_id: get_bounded_token(arguments, "case_id"),
            expected_tool: get_bounded_token(arguments, "expected_tool"),
        }
    }
}

pub(crate) fn exact_initialize_harness(
    params: &Value,
    managed_harness: Option<HarnessId>,
) -> Option<HarnessId> {
    let mut resolved = managed_harness;
    let claims = [
        params.get("client_name"),
        params
            .get("clientInfo")
            .or_else(|| params.get("client_info"))
            .and_then(|client| client.get("name")),
    ];
    let mut saw_claim = false;
    for claim in claims.into_iter().flatten() {
        let raw = claim.as_str()?.trim();
        if raw.is_empty() {
            return None;
        }
        let candidate = HarnessId::from_client_hint(raw)?;
        saw_claim = true;
        if resolved.is_some_and(|existing| existing != candidate) {
            return None;
        }
        resolved = Some(candidate);
    }
    // A managed header records installer intent, not proof of the process
    // actually speaking MCP. Require at least one exact initialize claim so a
    // copied header set cannot relabel an unknown client as a selected harness.
    if managed_harness.is_some() && !saw_claim {
        return None;
    }
    resolved
}

fn caller_partition_base() -> Option<SessionKey> {
    if let Some(auth) = mcp_client::get_task_auth_override() {
        if let Some(jwt) = auth.jwt.as_deref().filter(|value| !value.is_empty()) {
            return Some(SessionKey::for_http_jwt(jwt, None));
        }
        if let Some(api_key) = auth.api_key.as_deref().filter(|value| !value.is_empty()) {
            return Some(SessionKey::for_http_api_key(api_key, None));
        }
    }
    mcp_client::get_task_session_key()
}

fn telemetry_partition(explicit_mcp_session_id: Option<&str>) -> Option<TelemetryPartition> {
    let task_mcp_session_id = mcp_client::get_task_mcp_session_id();
    let raw_mcp_session_id = explicit_mcp_session_id
        .or(task_mcp_session_id.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.len() <= 128);
    let mut caller = caller_partition_base();
    let mcp_session = raw_mcp_session_id.map(SessionKey::for_anonymous_http);

    // Anonymous HTTP has no durable principal. Once the server-minted MCP
    // session exists, that unguessable session digest is the complete
    // partition; retaining the initialize request nonce would make the next
    // request unable to find its own hints.
    if mcp_session.is_some() && matches!(caller, Some(SessionKey::AnonymousHttp(_))) {
        caller = None;
    }
    if caller.is_none() && mcp_session.is_none() {
        return None;
    }
    Some(TelemetryPartition {
        caller,
        mcp_session,
    })
}

fn clean_runtime_state(runtime: &mut RuntimeState, now: Instant) {
    runtime
        .turns
        .retain(|_, turn| now.duration_since(turn.last_activity) <= TURN_IDLE_TIMEOUT);
    runtime
        .hints
        .retain(|_, hints| now.duration_since(hints.last_activity) <= HINT_IDLE_TIMEOUT);
    runtime
        .turn_counters
        .retain(|partition, _| runtime.hints.contains_key(partition));

    // Insertion may transiently take the map to MAX+1; the next operation
    // trims it before use. Do not evict on every read when exactly at capacity.
    while runtime.hints.len() > MAX_HINT_PARTITIONS {
        let Some(oldest) = runtime
            .hints
            .iter()
            .min_by_key(|(_, hints)| hints.last_activity)
            .map(|(partition, _)| partition.clone())
        else {
            break;
        };
        runtime.hints.remove(&oldest);
        runtime.turn_counters.remove(&oldest);
        runtime.turns.retain(|key, _| key.partition != oldest);
    }
}

fn build_runtime_readiness_evidence(
    identity: &ManagedHarnessRuntimeIdentity,
    kind: RuntimeReadinessKind,
    observed_at: chrono::DateTime<Utc>,
) -> HarnessReadinessEvidence {
    let (stage, status, source) = match kind {
        RuntimeReadinessKind::Connected => (
            HarnessReadinessStage::Connected,
            ReadinessEvidenceStatus::Verified,
            ReadinessEvidenceSource::McpProtocolRequest,
        ),
        RuntimeReadinessKind::GroundedInit => (
            HarnessReadinessStage::Grounded,
            ReadinessEvidenceStatus::Verified,
            ReadinessEvidenceSource::InitTool,
        ),
        RuntimeReadinessKind::GroundedContext => (
            HarnessReadinessStage::Grounded,
            ReadinessEvidenceStatus::Verified,
            ReadinessEvidenceSource::ContextTool,
        ),
        RuntimeReadinessKind::PracticingSearch => (
            HarnessReadinessStage::Practicing,
            ReadinessEvidenceStatus::Inferred,
            ReadinessEvidenceSource::RuntimeBehavior,
        ),
    };
    let mut evidence =
        HarnessReadinessEvidence::new(identity.harness_id, stage, status, source, observed_at);
    evidence.teaching_version = Some(identity.teaching_version.clone());
    if kind == RuntimeReadinessKind::Connected {
        evidence.managed_config_version = Some(identity.managed_config_version.clone());
    }
    evidence
}

fn successful_managed_readiness_kind(
    tool_name: &str,
    result: &ToolResult,
    resolved_workspace_id: Option<Uuid>,
) -> Option<RuntimeReadinessKind> {
    if result.is_error || mcp_tools::registry::tool_result_is_access_gate(result) {
        return None;
    }
    match tool_name {
        "init" if resolved_workspace_id.is_some_and(|workspace_id| !workspace_id.is_nil()) => {
            Some(RuntimeReadinessKind::GroundedInit)
        }
        "context" if resolved_workspace_id.is_some_and(|workspace_id| !workspace_id.is_nil()) => {
            Some(RuntimeReadinessKind::GroundedContext)
        }
        "search" => Some(RuntimeReadinessKind::PracticingSearch),
        _ => None,
    }
}

#[derive(Clone)]
pub struct AgenticTelemetry {
    client: ContextStreamClient,
    session: Arc<SessionManager>,
    runtime: Arc<Mutex<RuntimeState>>,
}

impl AgenticTelemetry {
    pub fn new(client: ContextStreamClient, session: Arc<SessionManager>) -> Self {
        Self {
            client,
            session,
            runtime: Arc::new(Mutex::new(RuntimeState::default())),
        }
    }

    pub async fn update_initialize_hints(&self, params: &Value) {
        self.update_initialize_hints_for_session(params, None).await;
    }

    /// Return the exact harness identity established by the current caller's
    /// initialize request. Managed environment/header identity alone is never
    /// enough: unknown, malformed, missing, or conflicting client claims
    /// leave this unset.
    pub async fn current_harness_id(&self) -> Option<HarnessId> {
        let partition = telemetry_partition(None)?;
        let now = Instant::now();
        let mut runtime = self.runtime.lock().await;
        clean_runtime_state(&mut runtime, now);
        let hints = runtime.hints.get_mut(&partition)?;
        hints.last_activity = now;
        hints.harness_id
    }

    /// Store initialize hints in the exact caller/session partition that will
    /// own subsequent requests. HTTP initialize passes its server-minted
    /// response session id; stdio uses the explicit `SessionKey::Local` scope.
    pub async fn update_initialize_hints_for_session(
        &self,
        params: &Value,
        mcp_session_id: Option<&str>,
    ) {
        self.update_initialize_hints_inner(params, mcp_session_id, None)
            .await;
    }

    pub async fn update_managed_initialize_hints_for_session(
        &self,
        params: &Value,
        mcp_session_id: Option<&str>,
        managed_identity: Option<ManagedHarnessRuntimeIdentity>,
    ) {
        self.update_initialize_hints_inner(params, mcp_session_id, managed_identity)
            .await;
    }

    async fn update_initialize_hints_inner(
        &self,
        params: &Value,
        mcp_session_id: Option<&str>,
        managed_identity: Option<ManagedHarnessRuntimeIdentity>,
    ) {
        let Some(partition) = telemetry_partition(mcp_session_id) else {
            return;
        };
        let now = Instant::now();
        let mut runtime = self.runtime.lock().await;
        clean_runtime_state(&mut runtime, now);
        runtime.turns.retain(|key, _| key.partition != partition);
        runtime.turn_counters.remove(&partition);

        let harness_id = exact_initialize_harness(
            params,
            managed_identity
                .as_ref()
                .map(|identity| identity.harness_id),
        );
        let managed_identity =
            managed_identity.filter(|identity| harness_id == Some(identity.harness_id));
        let hints = RuntimeHints {
            harness_id,
            managed_identity: managed_identity.clone(),
            model_id: None,
            last_activity: now,
            readiness_observed_at: HashMap::new(),
        };
        let hints = runtime.hints.entry(partition).or_insert(hints);
        hints.harness_id = harness_id;
        hints.managed_identity = managed_identity;
        hints.readiness_observed_at.clear();
        hints.last_activity = now;

        // Strict model matching: only store a `model_id` hint if the registry
        // recognizes it. Editor-supplied raw values like `gpt-4o` (unsupported
        // here) or vendor-prefixed forms get canonicalized; truly unknown
        // strings are dropped so they don't pollute analytics.
        hints.model_id = params
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .and_then(mcp_model_registry::match_model)
            .map(|known| known.canonical_id.to_string());
    }

    pub async fn emit_managed_connected_readiness(
        &self,
        mcp_session_id: Option<&str>,
        workspace_id: Option<Uuid>,
        project_id: Option<Uuid>,
    ) {
        self.emit_managed_readiness(
            RuntimeReadinessKind::Connected,
            mcp_session_id,
            workspace_id,
            project_id,
        )
        .await;
    }

    pub async fn emit_managed_tool_readiness(&self, tool_name: &str, result: &ToolResult) {
        let state = self.session.state().await;
        let Some(kind) = successful_managed_readiness_kind(tool_name, result, state.workspace_id)
        else {
            return;
        };
        self.emit_managed_readiness(kind, None, state.workspace_id, state.project_id)
            .await;
    }

    async fn emit_managed_readiness(
        &self,
        kind: RuntimeReadinessKind,
        mcp_session_id: Option<&str>,
        workspace_id: Option<Uuid>,
        project_id: Option<Uuid>,
    ) {
        let Some(partition) = telemetry_partition(mcp_session_id) else {
            return;
        };
        let now = Instant::now();
        let (identity, observed_at) = {
            let mut runtime = self.runtime.lock().await;
            clean_runtime_state(&mut runtime, now);
            let Some(hints) = runtime.hints.get_mut(&partition) else {
                return;
            };
            hints.last_activity = now;
            let Some(identity) = hints.managed_identity.clone() else {
                return;
            };
            let observed_at = hints
                .readiness_observed_at
                .entry(kind)
                .or_insert_with(Utc::now)
                .to_owned();
            (identity, observed_at)
        };

        let evidence = build_runtime_readiness_evidence(&identity, kind, observed_at);
        self.client.spawn_runtime_harness_readiness(
            identity.installation_id,
            evidence,
            workspace_id,
            project_id,
        );
    }

    pub async fn emit_tool_search(
        &self,
        surface_profile: ToolSurfaceProfile,
        _query: &str,
        candidate_count: usize,
        elapsed: Duration,
        input: AgenticTelemetryInput,
    ) {
        let Some(snapshot) = self
            .record_turn_attempt("tool_search", "tool_search", &input)
            .await
        else {
            return;
        };

        let mut metadata = self.base_metadata(&snapshot, surface_profile);
        metadata.insert(
            META_TOOL_NAME.to_string(),
            Value::String("search".to_string()),
        );
        metadata.insert(
            META_ACTUAL_TOOL.to_string(),
            Value::String("tool_search".to_string()),
        );
        metadata.insert(
            META_CANDIDATE_COUNT.to_string(),
            Value::from(candidate_count as u64),
        );
        metadata.insert(
            META_LATENCY_MS.to_string(),
            Value::from(elapsed.as_millis() as u64),
        );
        metadata.insert(
            META_RESULT_COUNT.to_string(),
            Value::from(candidate_count as u64),
        );
        if let Some(rank) = input.selected_rank {
            metadata.insert(META_SELECTED_RANK.to_string(), Value::from(rank));
        }
        if let Some(case_id) = input.case_id.as_ref() {
            metadata.insert(META_CASE_ID.to_string(), Value::String(case_id.clone()));
        }
        if let Some(expected_tool) = input.expected_tool.as_ref() {
            metadata.insert(
                META_EXPECTED_TOOL.to_string(),
                Value::String(expected_tool.clone()),
            );
        }
        self.emit_rule(
            &snapshot,
            RULE_AGENTIC_TOOL_SEARCH_USED,
            "pass",
            2,
            metadata,
        );
        self.emit_retry_if_needed(&snapshot, surface_profile, "tool_search", elapsed, &input)
            .await;
    }

    pub async fn emit_execute_operation(
        &self,
        surface_profile: ToolSurfaceProfile,
        operation_name: &str,
        _query: Option<&str>,
        elapsed: Duration,
        result: Option<&ToolResult>,
        input: AgenticTelemetryInput,
    ) {
        let Some(snapshot) = self
            .record_turn_attempt(
                &format!("execute_operation:{operation_name}"),
                operation_name,
                &input,
            )
            .await
        else {
            return;
        };

        let mut metadata = self.base_metadata(&snapshot, surface_profile);
        metadata.insert(
            META_OPERATION_NAME.to_string(),
            Value::String(operation_name.to_string()),
        );
        metadata.insert(
            META_ACTUAL_TOOL.to_string(),
            Value::String(operation_name.to_string()),
        );
        metadata.insert(
            META_LATENCY_MS.to_string(),
            Value::from(elapsed.as_millis() as u64),
        );
        metadata.insert(
            META_RESULT_COUNT.to_string(),
            Value::from(result_count(result) as u64),
        );
        if let Some(rank) = input.selected_rank {
            metadata.insert(META_SELECTED_RANK.to_string(), Value::from(rank));
        }
        if let Some(case_id) = input.case_id.as_ref() {
            metadata.insert(META_CASE_ID.to_string(), Value::String(case_id.clone()));
        }
        if let Some(expected_tool) = input.expected_tool.as_ref() {
            metadata.insert(
                META_EXPECTED_TOOL.to_string(),
                Value::String(expected_tool.clone()),
            );
        }
        self.emit_rule(
            &snapshot,
            RULE_AGENTIC_EXECUTE_OPERATION_USED,
            "pass",
            2,
            metadata,
        );
        self.emit_retry_if_needed(&snapshot, surface_profile, operation_name, elapsed, &input)
            .await;
    }

    pub async fn emit_batch_operations(
        &self,
        surface_profile: ToolSurfaceProfile,
        operation_names: &[String],
        elapsed: Duration,
        result_count_hint: usize,
        input: AgenticTelemetryInput,
    ) {
        let Some(snapshot) = self
            .record_turn_attempt("batch_operations", "batch_operations", &input)
            .await
        else {
            return;
        };

        let mut metadata = self.base_metadata(&snapshot, surface_profile);
        metadata.insert(
            META_OPERATION_NAME.to_string(),
            Value::String("batch_operations".to_string()),
        );
        metadata.insert(
            META_ACTUAL_TOOL.to_string(),
            Value::String("batch_operations".to_string()),
        );
        metadata.insert(
            META_BATCH_COUNT.to_string(),
            Value::from(operation_names.len() as u64),
        );
        metadata.insert(
            META_LATENCY_MS.to_string(),
            Value::from(elapsed.as_millis() as u64),
        );
        metadata.insert(
            META_RESULT_COUNT.to_string(),
            Value::from(result_count_hint as u64),
        );
        if !operation_names.is_empty() {
            metadata.insert(
                "operation_names".to_string(),
                Value::Array(operation_names.iter().cloned().map(Value::String).collect()),
            );
        }
        if let Some(case_id) = input.case_id.as_ref() {
            metadata.insert(META_CASE_ID.to_string(), Value::String(case_id.clone()));
        }
        if let Some(expected_tool) = input.expected_tool.as_ref() {
            metadata.insert(
                META_EXPECTED_TOOL.to_string(),
                Value::String(expected_tool.clone()),
            );
        }
        self.emit_rule(
            &snapshot,
            RULE_AGENTIC_BATCH_OPERATIONS_USED,
            "pass",
            2,
            metadata,
        );
        self.emit_retry_if_needed(
            &snapshot,
            surface_profile,
            "batch_operations",
            elapsed,
            &input,
        )
        .await;
    }

    pub async fn emit_hidden_direct_call_blocked(
        &self,
        surface_profile: ToolSurfaceProfile,
        requested_name: &str,
        input: AgenticTelemetryInput,
    ) {
        let Some(snapshot) = self
            .record_turn_attempt(
                &format!("hidden_direct:{requested_name}"),
                requested_name,
                &input,
            )
            .await
        else {
            return;
        };

        let mut metadata = self.base_metadata(&snapshot, surface_profile);
        metadata.insert(
            META_HIDDEN_TOOL_REQUESTED.to_string(),
            Value::String(requested_name.to_string()),
        );
        metadata.insert(
            META_ACTUAL_TOOL.to_string(),
            Value::String(requested_name.to_string()),
        );
        if let Some(case_id) = input.case_id.as_ref() {
            metadata.insert(META_CASE_ID.to_string(), Value::String(case_id.clone()));
        }
        if let Some(expected_tool) = input.expected_tool.as_ref() {
            metadata.insert(
                META_EXPECTED_TOOL.to_string(),
                Value::String(expected_tool.clone()),
            );
        }
        self.emit_rule(
            &snapshot,
            RULE_AGENTIC_HIDDEN_DIRECT_CALL_BLOCKED,
            "fail",
            6,
            metadata,
        );
    }

    async fn emit_retry_if_needed(
        &self,
        snapshot: &TurnSnapshot,
        surface_profile: ToolSurfaceProfile,
        actual_tool: &str,
        elapsed: Duration,
        input: &AgenticTelemetryInput,
    ) {
        if !snapshot.emit_retry_event || snapshot.retry_count == 0 {
            return;
        }

        let mut metadata = self.base_metadata(snapshot, surface_profile);
        metadata.insert(
            META_ACTUAL_TOOL.to_string(),
            Value::String(actual_tool.to_string()),
        );
        metadata.insert(
            META_LATENCY_MS.to_string(),
            Value::from(elapsed.as_millis() as u64),
        );
        if let Some(case_id) = input.case_id.as_ref() {
            metadata.insert(META_CASE_ID.to_string(), Value::String(case_id.clone()));
        }
        if let Some(expected_tool) = input.expected_tool.as_ref() {
            metadata.insert(
                META_EXPECTED_TOOL.to_string(),
                Value::String(expected_tool.clone()),
            );
        }
        self.emit_rule(snapshot, RULE_AGENTIC_TOOL_RETRY, "fail", 4, metadata);
        debug!(
            retry_count = snapshot.retry_count,
            turn_id = %snapshot.turn_id,
            request_id = %snapshot.request_id,
            elapsed_ms = elapsed.as_millis() as u64,
            expected_tool = input.expected_tool.as_deref().unwrap_or(""),
            "agentic retry telemetry emitted"
        );
    }

    async fn record_turn_attempt(
        &self,
        action_key: &str,
        action_label: &str,
        input: &AgenticTelemetryInput,
    ) -> Option<TurnSnapshot> {
        let partition = telemetry_partition(None)?;
        let state = self.session.state().await;
        let turn_scope = input.turn_id.clone().unwrap_or_else(|| {
            state
                .session_id
                .clone()
                .and_then(|value| bounded_telemetry_token(&value))
                .unwrap_or_else(|| "stateless".to_string())
        });
        let now = Instant::now();
        let mut runtime = self.runtime.lock().await;
        clean_runtime_state(&mut runtime, now);

        let (harness_id, resolved_model_id) = {
            let hints = runtime.hints.get_mut(&partition)?;
            hints.last_activity = now;
            (
                hints.harness_id?,
                hints
                    .model_id
                    .clone()
                    .or_else(|| {
                        state
                            .session_id
                            .as_deref()
                            .and_then(crate::session_model_cache::lookup)
                    })
                    .unwrap_or_else(|| "unknown".to_string()),
            )
        };
        let storage_key = TurnStorageKey {
            partition: partition.clone(),
            turn_scope,
        };

        let new_turn = runtime
            .turns
            .get(&storage_key)
            .map(|turn| now.duration_since(turn.last_activity) > TURN_IDLE_TIMEOUT)
            .unwrap_or(true);

        let explicit_request_id = input
            .request_id
            .as_deref()
            .and_then(|value| Uuid::parse_str(value).ok());

        if new_turn {
            let sequence = runtime.turn_counters.entry(partition).or_default();
            *sequence += 1;
            let turn_id = input
                .turn_id
                .clone()
                .unwrap_or_else(|| format!("turn-{}", *sequence));
            runtime.turns.insert(
                storage_key.clone(),
                ActiveTurn {
                    turn_id,
                    request_id: explicit_request_id.unwrap_or_else(Uuid::new_v4),
                    last_activity: now,
                    tool_yield_count: 0,
                    attempts_by_action: HashMap::new(),
                    total_retry_count: 0,
                    last_emitted_retry_count: 0,
                    first_tool: None,
                },
            );
        }

        let turn = runtime.turns.get_mut(&storage_key)?;
        turn.last_activity = now;
        if let Some(request_id) = explicit_request_id {
            turn.request_id = request_id;
        }
        turn.tool_yield_count += 1;
        let attempt = turn
            .attempts_by_action
            .entry(action_key.to_string())
            .or_insert(0);
        *attempt += 1;
        if *attempt > 1 {
            turn.total_retry_count += 1;
        }

        let retry_count = input
            .retry_count
            .map(|value| value.max(turn.total_retry_count))
            .unwrap_or(turn.total_retry_count);
        let emit_retry_event = retry_count > turn.last_emitted_retry_count;
        if emit_retry_event {
            turn.last_emitted_retry_count = retry_count;
        }

        if turn.first_tool.is_none() {
            turn.first_tool = Some(action_label.to_string());
        }

        Some(TurnSnapshot {
            workspace_id: state.workspace_id,
            project_id: state.project_id,
            request_id: turn.request_id,
            turn_id: turn.turn_id.clone(),
            tool_yield_count: turn.tool_yield_count,
            retry_count,
            emit_retry_event,
            first_tool: turn.first_tool.clone(),
            harness_id,
            model_id: resolved_model_id,
        })
    }

    fn base_metadata(
        &self,
        snapshot: &TurnSnapshot,
        surface_profile: ToolSurfaceProfile,
    ) -> Map<String, Value> {
        let mut metadata = Map::new();
        metadata.insert(
            META_TOOL_SURFACE_PROFILE.to_string(),
            Value::String(analytics_surface_profile(surface_profile).to_string()),
        );
        metadata.insert(
            META_RETRY_COUNT.to_string(),
            Value::from(snapshot.retry_count as u64),
        );
        metadata.insert(
            META_TOOL_YIELD_COUNT.to_string(),
            Value::from(snapshot.tool_yield_count as u64),
        );
        metadata.insert(
            META_REQUEST_ID.to_string(),
            Value::String(snapshot.request_id.to_string()),
        );
        metadata.insert(
            META_TURN_ID.to_string(),
            Value::String(snapshot.turn_id.clone()),
        );
        metadata.insert(
            META_RUNTIME_ID.to_string(),
            Value::String(RUNTIME_CONTEXTSTREAM_MCP.to_string()),
        );
        metadata
    }

    fn emit_rule(
        &self,
        snapshot: &TurnSnapshot,
        rule_key: &str,
        result: &str,
        severity: i16,
        metadata: Map<String, Value>,
    ) {
        let request =
            build_runtime_compliance_request(snapshot, rule_key, result, severity, metadata);

        let client = self.client.clone();
        mcp_client::spawn_with_task_context(async move {
            if let Err(error) = client.track_compliance_event(request).await {
                debug!("agentic telemetry emit skipped: {}", error);
            }
        });
    }

    /// Emit a per-tool-call "tool-use quality" event for direct (stdio) tool
    /// calls. Records the call against the current turn so tool_yield_count /
    /// retry are tracked for direct tool use too, then emits an
    /// `agentic_tool_call` event carrying the tool name, latency, and result.
    /// Best-effort; call once per executed tool from the stdio dispatch.
    pub async fn emit_tool_call(
        &self,
        tool_name: &str,
        latency_ms: u64,
        is_error: bool,
        input: &AgenticTelemetryInput,
    ) {
        let Some(snapshot) = self
            .record_turn_attempt("tool_call", tool_name, input)
            .await
        else {
            return;
        };
        let mut metadata = Map::new();
        metadata.insert(
            "tool_name".to_string(),
            Value::String(tool_name.to_string()),
        );
        metadata.insert("latency_ms".to_string(), Value::from(latency_ms));
        metadata.insert(
            META_RETRY_COUNT.to_string(),
            Value::from(snapshot.retry_count as u64),
        );
        metadata.insert(
            META_TOOL_YIELD_COUNT.to_string(),
            Value::from(snapshot.tool_yield_count as u64),
        );
        let result = if is_error { "fail" } else { "pass" };
        self.emit_rule(&snapshot, "agentic_tool_call", result, 1, metadata);
    }
}

fn build_runtime_compliance_request(
    snapshot: &TurnSnapshot,
    rule_key: &str,
    result: &str,
    severity: i16,
    mut metadata: Map<String, Value>,
) -> ComplianceEventRequest {
    if let Some(expected_tool) = metadata
        .get(META_EXPECTED_TOOL)
        .and_then(Value::as_str)
        .map(str::to_string)
    {
        metadata.insert(
            META_FIRST_TOOL_CORRECT.to_string(),
            Value::Bool(
                snapshot
                    .first_tool
                    .as_deref()
                    .map(|first| first == expected_tool)
                    .unwrap_or(false),
            ),
        );
    }

    ComplianceEventRequest {
        workspace_id: snapshot.workspace_id,
        project_id: snapshot.project_id,
        tenant_id: Some("default".to_string()),
        // User-chosen MCP/session labels can contain paths, names, or prompt
        // fragments. Request/turn UUIDs provide correlation without
        // forwarding that free-form identifier.
        session_id: None,
        request_id: Some(snapshot.request_id),
        turn_id: Some(snapshot.turn_id.clone()),
        model_id: snapshot.model_id.clone(),
        harness_id: snapshot.harness_id.as_str().to_string(),
        rules_version: Some(VERSION.to_string()),
        rule_id: None,
        rule_key: rule_key.to_string(),
        rule_class: "procedural".to_string(),
        check_type: "deterministic".to_string(),
        result: result.to_string(),
        is_enforceable: Some(true),
        failure_source: (result == "fail").then_some("harness".to_string()),
        confidence: Some(1.0),
        severity: Some(severity),
        event_time: None,
        metadata: Some(Value::Object(sanitize_runtime_metadata(metadata))),
    }
}

pub fn result_count(result: Option<&ToolResult>) -> usize {
    let Some(result) = result else {
        return 0;
    };

    if let Some(structured) = result.structured_content.as_ref() {
        if let Some(count) = structured.get("count").and_then(|value| value.as_u64()) {
            return count as usize;
        }
        if let Some(items) = structured.get("results").and_then(|value| value.as_array()) {
            return items.len();
        }
        if let Some(items) = structured.get("items").and_then(|value| value.as_array()) {
            return items.len();
        }
        if structured.get("result").is_some() {
            return 1;
        }
    }

    result.content.len()
}

fn bounded_telemetry_token(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':'))
    {
        return None;
    }
    Some(value.to_string())
}

fn get_bounded_token(arguments: &Value, key: &str) -> Option<String> {
    arguments
        .get(key)
        .and_then(|value| value.as_str())
        .and_then(bounded_telemetry_token)
}

fn sanitize_runtime_metadata(metadata: Map<String, Value>) -> Map<String, Value> {
    const STRING_KEYS: &[&str] = &[
        META_ACTUAL_TOOL,
        META_CASE_ID,
        META_EXPECTED_TOOL,
        META_HIDDEN_TOOL_REQUESTED,
        META_OPERATION_NAME,
        META_REQUEST_ID,
        META_RUNTIME_ID,
        META_TOOL_NAME,
        META_TOOL_SURFACE_PROFILE,
        META_TURN_ID,
    ];
    const NUMBER_KEYS: &[&str] = &[
        META_BATCH_COUNT,
        META_CANDIDATE_COUNT,
        META_LATENCY_MS,
        META_RESULT_COUNT,
        META_RETRY_COUNT,
        META_SELECTED_RANK,
        META_TOOL_YIELD_COUNT,
    ];

    let mut sanitized = Map::new();
    for (key, value) in metadata {
        if STRING_KEYS.contains(&key.as_str()) {
            if let Some(value) = value
                .as_str()
                .and_then(bounded_telemetry_token)
                .map(Value::String)
            {
                sanitized.insert(key, value);
            }
        } else if (NUMBER_KEYS.contains(&key.as_str()) && value.is_number())
            || (key == META_FIRST_TOOL_CORRECT && value.is_boolean())
        {
            sanitized.insert(key, value);
        } else if key == "operation_names" {
            let values: Vec<Value> = value
                .as_array()
                .into_iter()
                .flatten()
                .take(32)
                .filter_map(Value::as_str)
                .filter_map(bounded_telemetry_token)
                .map(Value::String)
                .collect();
            if !values.is_empty() {
                sanitized.insert(key, Value::Array(values));
            }
        }
    }
    sanitized.insert(
        META_RUNTIME_ID.to_string(),
        Value::String(RUNTIME_CONTEXTSTREAM_MCP.to_string()),
    );
    sanitized
}

fn get_i64(arguments: &Value, key: &str) -> Option<i64> {
    arguments.get(key).and_then(|value| value.as_i64())
}

fn get_u32(arguments: &Value, key: &str) -> Option<u32> {
    arguments
        .get(key)
        .and_then(|value| value.as_u64())
        .and_then(|value| u32::try_from(value).ok())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_client::{run_with_auth_override, run_with_mcp_session_id, run_with_session_key};
    use mcp_types::tool::ToolResult;

    #[test]
    fn result_count_prefers_structured_count() {
        let result = ToolResult::with_structured(
            "ok",
            serde_json::json!({
                "count": 4,
                "results": [1, 2, 3],
            }),
        );
        assert_eq!(super::result_count(Some(&result)), 4);
    }

    #[test]
    fn telemetry_input_reads_optional_fields() {
        let input = AgenticTelemetryInput::from_arguments(&serde_json::json!({
            "turn_id": "turn-7",
            "request_id": "abc123",
            "selected_rank": 2,
            "retry_count": 3,
            "case_id": "case-a",
            "expected_tool": "tool_search",
        }));

        assert_eq!(input.turn_id.as_deref(), Some("turn-7"));
        assert_eq!(input.request_id.as_deref(), Some("abc123"));
        assert_eq!(input.selected_rank, Some(2));
        assert_eq!(input.retry_count, Some(3));
        assert_eq!(input.case_id.as_deref(), Some("case-a"));
        assert_eq!(input.expected_tool.as_deref(), Some("tool_search"));
    }

    #[test]
    fn initialize_identity_requires_one_nonconflicting_known_harness() {
        assert_eq!(
            exact_initialize_harness(&serde_json::json!({"clientInfo": {"name": "codex"}}), None),
            Some(HarnessId::Codex)
        );
        assert_eq!(
            exact_initialize_harness(
                &serde_json::json!({
                    "client_name": "codex",
                    "clientInfo": {"name": "claude-code"}
                }),
                None
            ),
            None
        );
        assert_eq!(
            exact_initialize_harness(
                &serde_json::json!({"clientInfo": {"name": "claude-code"}}),
                Some(HarnessId::Codex)
            ),
            None,
            "a managed header must not override conflicting initialize identity"
        );
        assert_eq!(
            exact_initialize_harness(
                &serde_json::json!({"clientInfo": {"name": "unknown-harness"}}),
                Some(HarnessId::Codex)
            ),
            None,
            "a managed header must not relabel an explicit unknown client"
        );
        assert_eq!(
            exact_initialize_harness(&serde_json::json!({}), Some(HarnessId::Codex)),
            None,
            "managed installer identity alone is not runtime evidence"
        );
    }

    #[tokio::test]
    async fn telemetry_partitions_same_session_id_by_authenticated_caller() {
        let auth_a = mcp_types::AuthOverride {
            api_key: Some("caller-a-key".to_string()),
            ..Default::default()
        };
        let auth_b = mcp_types::AuthOverride {
            api_key: Some("caller-b-key".to_string()),
            ..Default::default()
        };
        let a = run_with_auth_override(auth_a.clone(), || async {
            telemetry_partition(Some("same-session"))
        })
        .await;
        let a_again = run_with_auth_override(auth_a, || async {
            telemetry_partition(Some("same-session"))
        })
        .await;
        let b = run_with_auth_override(auth_b, || async {
            telemetry_partition(Some("same-session"))
        })
        .await;
        assert_eq!(a, a_again);
        assert_ne!(a, b);

        let anonymous_initialize =
            run_with_session_key(SessionKey::for_anonymous_http("request-nonce"), || async {
                telemetry_partition(Some("server-session"))
            })
            .await;
        let anonymous_follow_up = run_with_session_key(
            SessionKey::for_anonymous_http("different-request-bucket"),
            || async {
                run_with_mcp_session_id("server-session".to_string(), || async {
                    telemetry_partition(None)
                })
                .await
            },
        )
        .await;
        assert_eq!(anonymous_initialize, anonymous_follow_up);
    }

    #[tokio::test]
    async fn initialize_hints_do_not_bleed_between_shared_process_clients() {
        let config = mcp_types::Config {
            api_key: Some("process-key".to_string()),
            ..Default::default()
        };
        let client = ContextStreamClient::new(config.clone());
        let session = Arc::new(SessionManager::new(client.clone(), config));
        let telemetry = AgenticTelemetry::new(client, session);
        let identity = |harness_id| ManagedHarnessRuntimeIdentity {
            installation_id: Uuid::new_v4(),
            harness_id,
            managed_config_version: "2".to_string(),
            teaching_version: "harness_teaching_v4".to_string(),
        };

        for (api_key, session_id, harness_id) in [
            ("caller-a", "session-a", HarnessId::Codex),
            ("caller-b", "session-b", HarnessId::ClaudeCode),
        ] {
            run_with_auth_override(
                mcp_types::AuthOverride {
                    api_key: Some(api_key.to_string()),
                    ..Default::default()
                },
                || async {
                    telemetry
                        .update_managed_initialize_hints_for_session(
                            &serde_json::json!({
                                "clientInfo": {"name": harness_id.as_str()}
                            }),
                            Some(session_id),
                            Some(identity(harness_id)),
                        )
                        .await;
                },
            )
            .await;
        }

        run_with_auth_override(
            mcp_types::AuthOverride {
                api_key: Some("caller-c".to_string()),
                ..Default::default()
            },
            || async {
                telemetry
                    .update_managed_initialize_hints_for_session(
                        &serde_json::json!({"clientInfo": {"name": "claude-code"}}),
                        Some("session-c"),
                        Some(identity(HarnessId::Codex)),
                    )
                    .await;
            },
        )
        .await;

        let runtime = telemetry.runtime.lock().await;
        assert_eq!(runtime.hints.len(), 3);
        assert_eq!(
            runtime
                .hints
                .values()
                .filter_map(|hints| hints.harness_id)
                .collect::<std::collections::HashSet<_>>(),
            std::collections::HashSet::from([HarnessId::Codex, HarnessId::ClaudeCode])
        );
        assert_eq!(
            runtime
                .hints
                .values()
                .filter(|hints| hints.harness_id.is_none())
                .count(),
            1
        );
        assert_eq!(
            runtime
                .hints
                .values()
                .filter(|hints| hints.managed_identity.is_none())
                .count(),
            1,
            "the conflicting managed identity must be discarded rather than relabeled"
        );
    }

    #[test]
    fn runtime_metadata_is_a_closed_privacy_allowlist() {
        let sanitized = sanitize_runtime_metadata(Map::from_iter([
            (
                META_TOOL_NAME.to_string(),
                Value::String("search".to_string()),
            ),
            (
                META_OPERATION_NAME.to_string(),
                Value::String("memory_create_doc".to_string()),
            ),
            (
                "query".to_string(),
                Value::String("secret customer".to_string()),
            ),
            (
                "path".to_string(),
                Value::String("/home/alice/private".to_string()),
            ),
            (
                META_TURN_ID.to_string(),
                Value::String("/home/alice/private".to_string()),
            ),
            (
                "prompt".to_string(),
                Value::String("private prompt".to_string()),
            ),
            (
                META_CASE_ID.to_string(),
                Value::String("case-with spaces".to_string()),
            ),
            (META_RESULT_COUNT.to_string(), Value::from(4_u64)),
        ]));
        assert_eq!(sanitized[META_TOOL_NAME], "search");
        assert_eq!(sanitized[META_OPERATION_NAME], "memory_create_doc");
        assert_eq!(sanitized[META_RESULT_COUNT], 4);
        assert_eq!(sanitized[META_RUNTIME_ID], RUNTIME_CONTEXTSTREAM_MCP);
        assert!(!sanitized.contains_key("query"));
        assert!(!sanitized.contains_key("path"));
        assert!(!sanitized.contains_key("prompt"));
        assert!(!sanitized.contains_key(META_TURN_ID));
        assert!(!sanitized.contains_key(META_CASE_ID));
    }

    #[test]
    fn compliance_payload_uses_client_harness_and_valid_failure_source() {
        let snapshot = TurnSnapshot {
            workspace_id: Some(Uuid::new_v4()),
            project_id: Some(Uuid::new_v4()),
            request_id: Uuid::new_v4(),
            turn_id: "turn-1".to_string(),
            tool_yield_count: 1,
            retry_count: 0,
            emit_retry_event: false,
            first_tool: Some("search".to_string()),
            harness_id: HarnessId::Codex,
            model_id: "unknown".to_string(),
        };
        let request = build_runtime_compliance_request(
            &snapshot,
            RULE_AGENTIC_TOOL_RETRY,
            "fail",
            4,
            Map::new(),
        );
        assert_eq!(request.harness_id, HarnessId::Codex.as_str());
        assert_eq!(request.failure_source.as_deref(), Some("harness"));
        assert!(request.session_id.is_none());
        assert_eq!(
            request
                .metadata
                .as_ref()
                .and_then(Value::as_object)
                .and_then(|metadata| metadata.get(META_RUNTIME_ID))
                .and_then(Value::as_str),
            Some(RUNTIME_CONTEXTSTREAM_MCP)
        );
    }

    #[test]
    fn runtime_readiness_distinguishes_verified_grounding_from_inferred_practice() {
        let identity = ManagedHarnessRuntimeIdentity {
            installation_id: Uuid::new_v4(),
            harness_id: HarnessId::Codex,
            managed_config_version: "2".to_string(),
            teaching_version: "harness_teaching_v4".to_string(),
        };
        let observed_at = Utc::now();
        let connected = build_runtime_readiness_evidence(
            &identity,
            RuntimeReadinessKind::Connected,
            observed_at,
        );
        assert_eq!(connected.stage, HarnessReadinessStage::Connected);
        assert_eq!(connected.status, ReadinessEvidenceStatus::Verified);
        assert_eq!(
            connected.source,
            ReadinessEvidenceSource::McpProtocolRequest
        );
        assert_eq!(connected.managed_config_version.as_deref(), Some("2"));

        let search = build_runtime_readiness_evidence(
            &identity,
            RuntimeReadinessKind::PracticingSearch,
            observed_at,
        );
        assert_eq!(search.stage, HarnessReadinessStage::Practicing);
        assert_eq!(search.status, ReadinessEvidenceStatus::Inferred);
        assert_eq!(search.source, ReadinessEvidenceSource::RuntimeBehavior);
        assert!(search.managed_config_version.is_none());

        assert_eq!(
            successful_managed_readiness_kind(
                "context",
                &ToolResult::text("[AUTH_REQUIRED] Authentication required"),
                Some(Uuid::new_v4())
            ),
            None
        );
        assert_eq!(
            successful_managed_readiness_kind(
                "search",
                &ToolResult::text("[SETUP_REQUIRED] Run setup first"),
                Some(Uuid::new_v4())
            ),
            None
        );
        assert_eq!(
            successful_managed_readiness_kind(
                "search",
                &ToolResult::error("failed"),
                Some(Uuid::new_v4())
            ),
            None
        );
        assert_eq!(
            successful_managed_readiness_kind("search", &ToolResult::text("3 results"), None),
            Some(RuntimeReadinessKind::PracticingSearch)
        );
        assert_eq!(
            successful_managed_readiness_kind(
                "context",
                &ToolResult::text("grounded"),
                Some(Uuid::new_v4())
            ),
            Some(RuntimeReadinessKind::GroundedContext)
        );
        assert_eq!(
            successful_managed_readiness_kind("init", &ToolResult::text("initialized"), None),
            None,
            "hosted verified grounding requires a resolved caller-partitioned session scope"
        );
        assert_eq!(
            successful_managed_readiness_kind(
                "context",
                &ToolResult::text("grounded"),
                Some(Uuid::nil())
            ),
            None
        );
    }
}
