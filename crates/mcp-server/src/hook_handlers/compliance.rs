//! Best-effort compliance event emitter.
//!
//! Fires `POST /api/v1/analytics/compliance/events` from hook flows so the
//! ContextStream compliance dashboard gets real telemetry from the Rust MCP.
//! All calls are fire-and-forget: network or API errors are silently ignored
//! to avoid slowing down hook execution.
//!
//! # Model identification
//!
//! Each event carries a `model_id`. Resolution order is:
//! 1. Explicit `model_id` field on the [`ComplianceEvent`] (caller-supplied).
//! 2. The shared [`mcp_model_registry`] alias map applied to a hook payload via
//!    [`super::client_model_extractor::extract_model_from_hook`].
//! 3. The file-backed [`crate::session_model_cache`] keyed by `session_id`.
//!
//! When none of those produce a registry match, the field is omitted from the
//! payload. The API treats absent values as `unknown` rather than letting an
//! editor or client name leak into the model leaderboard.

use super::client_model_extractor::extract_model_from_hook;
use super::common::ApiConfig;
use crate::session_model_cache;
use mcp_types::{HarnessId, RUNTIME_CONTEXTSTREAM_MCP};
use serde_json::Value;
use std::time::Duration;

/// Current rules version tag (bump when rule logic changes).
const RULES_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Rule classes aligned with the API's `normalize_enum` whitelist.
#[derive(Debug, Clone, Copy)]
pub enum RuleClass {
    Hard,
    Procedural,
    Soft,
    /// Rules learned from a user's corrections (the coaching loop). A failure
    /// means the agent repeated a mistake the user already corrected.
    Learned,
}

impl RuleClass {
    fn as_str(self) -> &'static str {
        match self {
            Self::Hard => "hard",
            Self::Procedural => "procedural",
            Self::Soft => "soft",
            Self::Learned => "learned",
        }
    }
}

/// Check types aligned with the API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckType {
    Deterministic,
    Heuristic,
}

impl CheckType {
    fn as_str(self) -> &'static str {
        match self {
            Self::Deterministic => "deterministic",
            Self::Heuristic => "heuristic",
        }
    }
}

/// Result of a compliance check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckResult {
    Pass,
    Fail,
}

impl CheckResult {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
        }
    }
}

/// A compliance event to be emitted.
pub struct ComplianceEvent {
    pub rule_key: &'static str,
    pub rule_class: RuleClass,
    pub check_type: CheckType,
    pub result: CheckResult,
    pub severity: i16,
    pub metadata: Option<Value>,
    /// Optional registry-validated canonical model id. When `None`, callers
    /// should resolve via [`resolve_model_id`] or [`emit_for_hook`] which
    /// already handle hook payload extraction and the session cache.
    pub model_id: Option<String>,
    /// Optional explicit session id. When provided, overrides any value
    /// derived from the hook payload during resolution.
    pub session_id: Option<String>,
    /// Canonical coding harness that generated the hook event. This must be
    /// resolved from exact runtime/payload identity; the MCP binary itself is
    /// recorded separately as `metadata.runtime_id`.
    pub harness_id: Option<HarnessId>,
}

impl Default for ComplianceEvent {
    /// Blank scaffold so existing call sites can use struct-update syntax
    /// (`..ComplianceEvent::default()`) when adding `model_id` / `session_id`
    /// without specifying them. The five required fields below are mandatory
    /// — leave them set on the literal.
    fn default() -> Self {
        Self {
            rule_key: "",
            rule_class: RuleClass::Soft,
            check_type: CheckType::Deterministic,
            result: CheckResult::Pass,
            severity: 1,
            metadata: None,
            model_id: None,
            session_id: None,
            harness_id: None,
        }
    }
}

impl ComplianceEvent {
    /// Convenience constructor preserving the prior call shape (no explicit
    /// model / session). Resolution falls back to the session cache by
    /// session id if [`emit_for_hook`] is used.
    pub fn rule(
        rule_key: &'static str,
        rule_class: RuleClass,
        check_type: CheckType,
        result: CheckResult,
        severity: i16,
        metadata: Option<Value>,
    ) -> Self {
        Self {
            rule_key,
            rule_class,
            check_type,
            result,
            severity,
            metadata,
            model_id: None,
            session_id: None,
            harness_id: None,
        }
    }
}

/// Resolve a model id from the available signals. Returns `None` when the
/// registry does not recognize any candidate — callers must NOT invent.
///
/// Order:
/// 1. Caller-supplied `explicit`.
/// 2. The hook payload, via [`extract_model_from_hook`].
/// 3. The file-backed [`session_model_cache`] keyed by `session_id`.
pub fn resolve_model_id(
    explicit: Option<&str>,
    hook_payload: Option<&Value>,
    hook_event: Option<&str>,
    session_id: Option<&str>,
) -> Option<String> {
    if let Some(value) = explicit.map(str::trim).filter(|v| !v.is_empty()) {
        if let Some(matched) = mcp_model_registry::match_model(value) {
            return Some(matched.canonical_id.to_string());
        }
    }

    if let Some(payload) = hook_payload {
        if let Some(model) = extract_model_from_hook(payload, hook_event.unwrap_or("")) {
            // Side effect: warm the session cache so later hooks (different
            // process, same session) inherit the captured model.
            if let Some(sid) = session_id.map(str::trim).filter(|v| !v.is_empty()) {
                session_model_cache::record(sid, &model);
            }
            return Some(model);
        }
    }

    if let Some(sid) = session_id.map(str::trim).filter(|v| !v.is_empty()) {
        if let Some(cached) = session_model_cache::lookup(sid) {
            // Defensive: only return if still a registry hit.
            if let Some(matched) = mcp_model_registry::match_model(&cached) {
                return Some(matched.canonical_id.to_string());
            }
        }
    }

    None
}

/// Helper that builds and emits a [`ComplianceEvent`] using model/session
/// resolution against the supplied hook payload.
pub fn emit_for_hook(
    config: &ApiConfig,
    hook_payload: Option<&Value>,
    hook_event: Option<&str>,
    mut event: ComplianceEvent,
) {
    let harness_id = exact_hook_harness(hook_payload);
    record_local_practice_if_proven(harness_id, &event);

    let session_id = event
        .session_id
        .clone()
        .or_else(|| extract_session_id(hook_payload));

    let resolved = resolve_model_id(
        event.model_id.as_deref(),
        hook_payload,
        hook_event,
        session_id.as_deref(),
    );

    event.model_id = resolved;
    event.session_id = session_id;
    event.harness_id = harness_id;
    emit(config, event);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HarnessResolution {
    Missing,
    Exact(HarnessId),
    Conflict,
}

fn exact_payload_harness(payload: Option<&Value>) -> HarnessResolution {
    let Some(payload) = payload else {
        return HarnessResolution::Missing;
    };
    let mut client_hint = None;
    for key in ["client_name", "clientName", "client", "editor", "source"] {
        let Some(candidate) = payload
            .get(key)
            .and_then(Value::as_str)
            .and_then(HarnessId::from_alias)
        else {
            continue;
        };
        if client_hint.is_some_and(|existing| existing != candidate) {
            return HarnessResolution::Conflict;
        }
        client_hint = Some(candidate);
    }

    // `hook_event_name` is a documented, host-emitted identity signal for
    // Claude and the Windsurf/Cursor integrations. Do not use the generic
    // `hookName` field: Cline-family products share that payload shape and
    // cannot be distinguished without an explicit client id.
    let event_harness = payload
        .get("hook_event_name")
        .or_else(|| payload.get("hookEventName"))
        .and_then(Value::as_str)
        .and_then(|event| mcp_model_registry::match_editor(None, Some(event)))
        .and_then(mcp_model_registry::KnownEditor::harness_id);

    match (client_hint, event_harness) {
        (Some(client), Some(event)) if client != event => HarnessResolution::Conflict,
        (Some(client), _) => HarnessResolution::Exact(client),
        (None, Some(event)) => HarnessResolution::Exact(event),
        (None, None) => HarnessResolution::Missing,
    }
}

fn resolve_exact_hook_harness(
    environment_harness: Option<HarnessId>,
    payload_harness: HarnessResolution,
) -> Option<HarnessId> {
    match (environment_harness, payload_harness) {
        (_, HarnessResolution::Conflict) => None,
        (Some(environment), HarnessResolution::Exact(payload)) if environment != payload => None,
        (Some(environment), _) => Some(environment),
        (None, HarnessResolution::Exact(payload)) => Some(payload),
        (None, HarnessResolution::Missing) => None,
    }
}

pub(super) fn exact_hook_harness(payload: Option<&Value>) -> Option<HarnessId> {
    let environment_harness = std::env::var("CONTEXTSTREAM_CLIENT")
        .ok()
        .and_then(|value| HarnessId::from_alias(&value));
    let payload_harness = exact_payload_harness(payload);

    resolve_exact_hook_harness(environment_harness, payload_harness)
}

fn event_proves_deterministic_practice(event: &ComplianceEvent) -> bool {
    event.check_type == CheckType::Deterministic
        && event.result == CheckResult::Pass
        && matches!(
            event.rule_key,
            RULE_INIT_REQUIRED
                | RULE_CONTEXT_REQUIRED
                | RULE_SCOPE_ALIGNMENT
                | RULE_SESSION_CONTINUITY
        )
}

fn record_local_practice_if_proven(harness_id: Option<HarnessId>, event: &ComplianceEvent) {
    if !crate::hook_readiness_evidence_enabled() || !crate::managed_hook_invocation() {
        return;
    }
    if !event_proves_deterministic_practice(event) {
        return;
    }
    let Some(harness_id) = harness_id else {
        return;
    };
    if let Err(error) = mcp_client::harness_readiness::record_deterministic_practice(harness_id) {
        tracing::warn!(
            harness = harness_id.as_str(),
            rule = event.rule_key,
            error = %error,
            "Deterministic harness practice could not be recorded"
        );
    }
}

fn extract_session_id(payload: Option<&Value>) -> Option<String> {
    let payload = payload?;
    for key in ["session_id", "sessionId", "session"] {
        if let Some(s) = payload.get(key).and_then(|v| v.as_str()) {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

/// Emit a compliance event to the API. Best-effort: errors are silently dropped.
///
/// This spawns a background task so it never blocks the hook response.
pub fn emit(config: &ApiConfig, event: ComplianceEvent) {
    let Some(payload) = build_payload(config, &event) else {
        return;
    };
    let api_url = config.api_url.clone();
    let api_key = config.api_key.clone();

    // Fire-and-forget: spawn a background task so the hook returns immediately.
    tokio::spawn(async move {
        let client = super::api_http_client();
        let _ = client
            .post(format!("{}/api/v1/analytics/compliance/events", api_url))
            .header("X-API-Key", &api_key)
            .header("Content-Type", "application/json")
            .json(&payload)
            .timeout(Duration::from_secs(5))
            .send()
            .await;
    });
}

fn bounded_metadata_token(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 96
        || !value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b':' | b'/')
        })
    {
        return None;
    }
    Some(value.to_string())
}

fn sanitized_metadata(metadata: Option<&Value>) -> Value {
    const ALLOWED_KEYS: &[&str] = &[
        "action",
        "blocked_tool",
        "kind",
        "note",
        "nudged_tool",
        "operation",
        "reason",
        "tool",
    ];

    let mut sanitized = serde_json::Map::new();
    sanitized.insert(
        "runtime_id".to_string(),
        Value::String(RUNTIME_CONTEXTSTREAM_MCP.to_string()),
    );
    sanitized.insert(
        "evidence_source".to_string(),
        Value::String("managed_hook".to_string()),
    );
    if let Some(object) = metadata.and_then(Value::as_object) {
        for key in ALLOWED_KEYS {
            let Some(value) = object.get(*key).and_then(Value::as_str) else {
                continue;
            };
            if let Some(value) = bounded_metadata_token(value) {
                sanitized.insert((*key).to_string(), Value::String(value));
            }
        }
    }
    Value::Object(sanitized)
}

fn build_payload(
    config: &ApiConfig,
    event: &ComplianceEvent,
) -> Option<serde_json::Map<String, Value>> {
    if !config.is_configured() {
        return None;
    }
    let workspace_id = config
        .workspace_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())?;
    let harness_id = event.harness_id?;

    let mut payload = serde_json::Map::new();
    payload.insert(
        "workspace_id".to_string(),
        Value::String(workspace_id.to_string()),
    );
    if let Some(project_id) = config
        .project_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        payload.insert(
            "project_id".to_string(),
            Value::String(project_id.to_string()),
        );
    }
    if let Some(model_id) = event
        .model_id
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        payload.insert("model_id".to_string(), Value::String(model_id.to_string()));
    } else {
        // Sentinel kept for back-compat with API payload validation that
        // requires a non-empty `model_id`. The server-side registry will tag
        // this row with `model_canonical_id = NULL` and `provider = 'unknown'`
        // so it filters out of public dashboards by default.
        payload.insert("model_id".to_string(), Value::String("unknown".to_string()));
    }
    if let Some(session_id) = event.session_id.as_deref().and_then(bounded_metadata_token) {
        payload.insert("session_id".to_string(), Value::String(session_id));
    }
    payload.insert(
        "harness_id".to_string(),
        Value::String(harness_id.as_str().to_string()),
    );
    payload.insert(
        "rules_version".to_string(),
        Value::String(RULES_VERSION.to_string()),
    );
    payload.insert(
        "rule_key".to_string(),
        Value::String(event.rule_key.to_string()),
    );
    payload.insert(
        "rule_class".to_string(),
        Value::String(event.rule_class.as_str().to_string()),
    );
    payload.insert(
        "check_type".to_string(),
        Value::String(event.check_type.as_str().to_string()),
    );
    payload.insert(
        "result".to_string(),
        Value::String(event.result.as_str().to_string()),
    );
    payload.insert("is_enforceable".to_string(), Value::Bool(true));
    payload.insert("severity".to_string(), Value::Number(event.severity.into()));
    payload.insert(
        "metadata".to_string(),
        sanitized_metadata(event.metadata.as_ref()),
    );
    Some(payload)
}

// ============================================================================
// Predefined rule keys for hook enforcement points
// ============================================================================

/// PreToolUse blocked a tool because `init(...)` was required first.
pub const RULE_INIT_REQUIRED: &str = "init_before_tools";

/// PreToolUse blocked a tool because `context(...)` was required first.
pub const RULE_CONTEXT_REQUIRED: &str = "context_before_tools";

/// PreToolUse redirected a broad Glob/Grep to ContextStream search.
pub const RULE_SEARCH_FIRST: &str = "search_first";

/// PreToolUse nudged reading `[GROUNDING]` prior-work hits before local code search.
pub const RULE_GROUNDING_FIRST: &str = "grounding_first";

/// PreToolUse nudged plan saving to ContextStream.
pub const RULE_PLAN_PERSISTENCE: &str = "plan_persistence";

/// PreToolUse nudged doc/spec writes to ContextStream.
pub const RULE_DOC_PERSISTENCE: &str = "doc_persistence";

/// PreToolUse blocked a local-file substitute for a canonical handoff.
pub const RULE_HANDOFF_PERSISTENCE: &str = "handoff_persistence";

/// PreToolUse required user-visible progress for long-running writes.
pub const RULE_VISIBILITY: &str = "visibility_requirement";

/// PreToolUse detected missing/ambiguous workspace+project scope pairing.
pub const RULE_SCOPE_ALIGNMENT: &str = "scope_alignment";

/// PreToolUse nudged stable session_id usage for context continuity.
pub const RULE_SESSION_CONTINUITY: &str = "session_continuity";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rule_class_as_str() {
        assert_eq!(RuleClass::Hard.as_str(), "hard");
        assert_eq!(RuleClass::Procedural.as_str(), "procedural");
        assert_eq!(RuleClass::Soft.as_str(), "soft");
    }

    #[test]
    fn check_type_as_str() {
        assert_eq!(CheckType::Deterministic.as_str(), "deterministic");
        assert_eq!(CheckType::Heuristic.as_str(), "heuristic");
    }

    #[test]
    fn check_result_as_str() {
        assert_eq!(CheckResult::Pass.as_str(), "pass");
        assert_eq!(CheckResult::Fail.as_str(), "fail");
    }

    #[test]
    fn exact_payload_harness_never_guesses_from_ambiguous_or_substring_hints() {
        assert_eq!(
            exact_payload_harness(Some(&serde_json::json!({
                "hook_event_name": "PreToolUse",
                "tool_name": "Read"
            }))),
            HarnessResolution::Exact(HarnessId::ClaudeCode)
        );
        assert_eq!(
            exact_payload_harness(Some(&serde_json::json!({
                "hook_event_name": "pre_mcp_tool_use"
            }))),
            HarnessResolution::Exact(HarnessId::Windsurf)
        );
        assert_eq!(
            exact_payload_harness(Some(&serde_json::json!({
                "client_name": "roo-code"
            }))),
            HarnessResolution::Exact(HarnessId::RooCode)
        );
        assert_eq!(
            exact_payload_harness(Some(&serde_json::json!({
                "hookName": "PreToolUse",
                "toolName": "read_file"
            }))),
            HarnessResolution::Missing,
            "shared Cline-family payloads are ambiguous without a client id"
        );
        assert_eq!(
            exact_payload_harness(Some(&serde_json::json!({
                "client_name": "my-contextstream-cursor-proxy"
            }))),
            HarnessResolution::Missing
        );
        assert_eq!(
            exact_payload_harness(Some(&serde_json::json!({
                "client_name": "cursor",
                "hook_event_name": "PreToolUse"
            }))),
            HarnessResolution::Conflict,
            "conflicting exact identity signals must fail closed"
        );
        assert_eq!(
            exact_payload_harness(Some(&serde_json::json!({
                "client_name": "claude",
                "editor": "cursor"
            }))),
            HarnessResolution::Conflict,
            "multiple explicit payload identities must agree"
        );
        assert_eq!(
            resolve_exact_hook_harness(Some(HarnessId::ClaudeCode), HarnessResolution::Conflict,),
            None,
            "an environment identity must not mask conflicting payload signals"
        );
    }

    #[test]
    fn only_behavioral_deterministic_passes_prove_practice() {
        let eligible = || ComplianceEvent {
            rule_key: RULE_SESSION_CONTINUITY,
            rule_class: RuleClass::Procedural,
            check_type: CheckType::Deterministic,
            result: CheckResult::Pass,
            severity: 1,
            ..Default::default()
        };
        assert!(event_proves_deterministic_practice(&eligible()));

        for event in [
            ComplianceEvent {
                result: CheckResult::Fail,
                ..eligible()
            },
            ComplianceEvent {
                check_type: CheckType::Heuristic,
                ..eligible()
            },
            ComplianceEvent {
                rule_key: RULE_PLAN_PERSISTENCE,
                ..eligible()
            },
            ComplianceEvent {
                rule_key: RULE_VISIBILITY,
                ..eligible()
            },
            ComplianceEvent {
                rule_key: RULE_DOC_PERSISTENCE,
                ..eligible()
            },
        ] {
            assert!(!event_proves_deterministic_practice(&event));
        }
    }

    #[test]
    fn emit_skips_when_not_configured() {
        let config = ApiConfig {
            api_key: String::new(),
            api_url: "https://api.contextstream.io".to_string(),
            workspace_id: None,
            project_id: None,
        };
        // Should not panic — just returns early.
        emit(
            &config,
            ComplianceEvent {
                rule_key: RULE_INIT_REQUIRED,
                rule_class: RuleClass::Hard,
                check_type: CheckType::Deterministic,
                result: CheckResult::Fail,
                severity: 5,
                metadata: None,
                ..Default::default()
            },
        );
    }

    #[test]
    fn emit_skips_when_no_workspace_id() {
        let config = ApiConfig {
            api_key: "test-key".to_string(),
            api_url: "https://api.contextstream.io".to_string(),
            workspace_id: None,
            project_id: None,
        };
        // Should not panic — workspace_id is required.
        emit(
            &config,
            ComplianceEvent {
                rule_key: RULE_CONTEXT_REQUIRED,
                rule_class: RuleClass::Hard,
                check_type: CheckType::Deterministic,
                result: CheckResult::Fail,
                severity: 5,
                metadata: None,
                ..Default::default()
            },
        );
    }

    #[test]
    fn payload_attributes_actual_harness_and_allows_only_bounded_metadata() {
        let config = ApiConfig {
            api_key: "test-key".to_string(),
            api_url: "https://api.contextstream.io".to_string(),
            workspace_id: Some("80808080-8080-4080-8080-808080808080".to_string()),
            project_id: Some("90909090-9090-4090-8090-909090909090".to_string()),
        };
        let event = ComplianceEvent {
            rule_key: RULE_SEARCH_FIRST,
            rule_class: RuleClass::Procedural,
            check_type: CheckType::Heuristic,
            result: CheckResult::Pass,
            severity: 1,
            metadata: Some(serde_json::json!({
                "tool": "contextstream_search",
                "reason": "search_first_satisfied",
                "file_path": "/home/alice/private/repo",
                "query": "secret customer name",
                "requested_project_id": "private-project",
                "note": "contains whitespace and must be dropped"
            })),
            harness_id: Some(HarnessId::Codex),
            session_id: Some("customer name must not leave the hook".to_string()),
            ..Default::default()
        };

        let payload = build_payload(&config, &event).expect("eligible payload");
        assert_eq!(payload["harness_id"], HarnessId::Codex.as_str());
        assert!(!payload.contains_key("session_id"));
        let metadata = payload["metadata"].as_object().expect("metadata object");
        assert_eq!(
            metadata["runtime_id"],
            Value::String(RUNTIME_CONTEXTSTREAM_MCP.to_string())
        );
        assert_eq!(metadata["evidence_source"], "managed_hook");
        assert_eq!(metadata["tool"], "contextstream_search");
        assert_eq!(metadata["reason"], "search_first_satisfied");
        assert!(!metadata.contains_key("file_path"));
        assert!(!metadata.contains_key("query"));
        assert!(!metadata.contains_key("requested_project_id"));
        assert!(!metadata.contains_key("note"));
    }

    #[test]
    fn payload_without_exact_harness_identity_is_dropped() {
        let config = ApiConfig {
            api_key: "test-key".to_string(),
            api_url: "https://api.contextstream.io".to_string(),
            workspace_id: Some("80808080-8080-4080-8080-808080808080".to_string()),
            project_id: None,
        };
        assert!(build_payload(
            &config,
            &ComplianceEvent {
                rule_key: RULE_INIT_REQUIRED,
                rule_class: RuleClass::Hard,
                check_type: CheckType::Deterministic,
                result: CheckResult::Pass,
                severity: 1,
                ..Default::default()
            }
        )
        .is_none());
    }

    #[test]
    fn resolve_model_id_uses_explicit_when_known() {
        assert_eq!(
            resolve_model_id(Some("claude-opus-4-7-thinking-high"), None, None, None).as_deref(),
            Some("claude-opus-4.7-thinking-high")
        );
    }

    #[test]
    fn resolve_model_id_drops_unknown_explicit_value() {
        // Strict matching: an unknown explicit string falls through, then
        // returns None when no other source is available.
        assert_eq!(
            resolve_model_id(Some("totally-fake-model"), None, None, None),
            None
        );
    }

    #[test]
    fn resolve_model_id_extracts_from_hook_payload() {
        let payload = serde_json::json!({ "model": "anthropic/claude-sonnet-4.5" });
        assert_eq!(
            resolve_model_id(None, Some(&payload), Some("PreToolUse"), None).as_deref(),
            Some("claude-sonnet-4.5")
        );
    }

    #[test]
    fn resolve_model_id_returns_none_when_nothing_resolvable() {
        let payload = serde_json::json!({ "client_name": "claude-code" });
        assert_eq!(
            resolve_model_id(None, Some(&payload), Some("PreToolUse"), None),
            None
        );
    }
}
