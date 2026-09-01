//! Session instruction cache tools (canonical: instruct; compatibility aliases: ram, mem).
//!
//! Actions: bootstrap, get, push, ack, clear, stats, checkpoint, verify.

use async_trait::async_trait;
use mcp_client::{
    get_task_session_key, ContextStreamClient, FlashAckParams, FlashBootstrapParams,
    FlashCheckpointParams, FlashClearParams, FlashGetParams, FlashPushEntry, FlashPushParams,
    FlashStatsParams, FlashVerifyParams,
};
use mcp_session::SessionManager;
use mcp_types::{
    tool::{ToolAnnotations, ToolCategory, ToolMetadata, ToolResult},
    Error, Result, SessionKey,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;

use super::workspace_drift::{is_recoverable_read_error, is_workspace_access_error};
use crate::registry::ToolHandler;
use crate::schema::SchemaBuilder;

/// Valid flash actions.
const VALID_ACTIONS: &[&str] = &[
    "bootstrap",
    "get",
    "push",
    "ack",
    "clear",
    "stats",
    "checkpoint",
    "verify",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlashEntryInput {
    pub text: String,
    pub id: Option<String>,
    pub source: Option<String>,
    pub critical: Option<bool>,
    pub surface: Option<bool>,
    pub metadata: Option<Value>,
}

/// Input for the unified flash/ram tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlashInput {
    pub action: String,
    pub workspace_id: Option<String>,
    pub project_id: Option<String>,
    pub session_id: Option<String>,
    pub limit: Option<i64>,
    pub entries: Option<Vec<FlashEntryInput>>,
    pub increment_turn: Option<bool>,
    pub force_version_bump: Option<bool>,
    pub ids: Option<Vec<String>>,
    pub expected_version: Option<u64>,
}

/// Unified flash/ram tool handler.
pub struct FlashTool {
    client: ContextStreamClient,
    session: Arc<SessionManager>,
}

/// Outcome of `resolve_or_bootstrap_scope`.
struct ResolvedFlashScope {
    workspace_id: Option<Uuid>,
    project_id: Option<Uuid>,
    /// True when we created/recovered a fallback workspace during this call.
    bootstrapped: bool,
    /// Human-readable workspace name when bootstrapping (used in hints/messages).
    workspace_name: Option<String>,
}

impl FlashTool {
    pub fn new(client: ContextStreamClient, session: Arc<SessionManager>) -> Self {
        Self { client, session }
    }

    fn parse_scope_id(input: &Option<String>, field: &str) -> Result<Option<Uuid>> {
        match input {
            Some(s) => {
                Ok(Some(Uuid::parse_str(s).map_err(|_| {
                    Error::Validation(format!("Invalid {field}"))
                })?))
            }
            None => Ok(None),
        }
    }

    fn require_session_id(input: Option<String>) -> Result<String> {
        input
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Validation("session_id is required".to_string()))
    }

    fn map_entries(entries: Vec<FlashEntryInput>) -> Vec<FlashPushEntry> {
        entries
            .into_iter()
            .map(|entry| FlashPushEntry {
                text: entry.text,
                id: entry.id,
                source: entry.source,
                critical: entry.critical,
                surface: entry.surface,
                metadata: entry.metadata,
            })
            .collect()
    }

    /// Resolve the instruction read scope. Explicit ids always win. Implicit
    /// state/config fallback is safe only for explicitly marked local stdio
    /// calls whose requested instruction session matches the active local
    /// session. Hosted HTTP requests can land on another process or use a
    /// request-unique state bucket, so inheriting its process-local project can
    /// leak instructions from a previously active project.
    async fn resolve_or_bootstrap_scope(
        &self,
        explicit_workspace_id: Option<Uuid>,
        explicit_project_id: Option<Uuid>,
        requested_session_id: &str,
    ) -> Result<ResolvedFlashScope> {
        let state = self.session.state().await;
        let config = self.client.config().await;
        let allow_implicit_scope = matches!(get_task_session_key(), Some(SessionKey::Local))
            && state.session_id.as_deref() == Some(requested_session_id);
        let workspace_id = explicit_workspace_id.or_else(|| {
            allow_implicit_scope
                .then(|| state.workspace_id.or(config.default_workspace_id))
                .flatten()
        });
        let project_id = explicit_project_id.or_else(|| {
            if !allow_implicit_scope {
                return None;
            }
            match explicit_workspace_id {
                Some(workspace_id) if state.workspace_id == Some(workspace_id) => state.project_id,
                Some(workspace_id)
                    if state.workspace_id.is_none()
                        && config.default_workspace_id == Some(workspace_id) =>
                {
                    config.default_project_id
                }
                Some(_) => None,
                None if state.workspace_id.is_some() || state.project_id.is_some() => {
                    state.project_id
                }
                None => config.default_project_id,
            }
        });

        Ok(ResolvedFlashScope {
            workspace_id,
            project_id,
            bootstrapped: false,
            workspace_name: None,
        })
    }
}

// `is_recoverable_read_error` and `is_workspace_access_error` were
// originally defined inline here in v0.2.98. v0.3.0 hoists them into
// the shared `super::workspace_drift` module so other read-side tools
// (memory / session / capsule / skill / …) can reuse the same drift
// classifier without copy-paste. The local imports above re-bind the
// names so the existing match arms below keep compiling unchanged.

/// Attach the bootstrap hint into a structured payload (no-op when absent).
fn attach_bootstrap_note(payload: &mut Value, note: Option<&str>) {
    let Some(note) = note else { return };
    if let Some(obj) = payload.as_object_mut() {
        obj.entry("bootstrap_note")
            .or_insert(Value::String(note.to_string()));
    }
}

/// Mark a project-omitted read as intentionally workspace-only so callers can
/// distinguish safe fail-closed behavior from a relevance/search failure.
fn attach_workspace_only_note(payload: &mut Value, project_id: Option<Uuid>) {
    if project_id.is_some() {
        return;
    }
    if let Some(obj) = payload.as_object_mut() {
        obj.entry("scope_status")
            .or_insert(Value::String("workspace_only".to_string()));
        obj.entry("scope_hint").or_insert(Value::String(
            "Project-scoped entries were intentionally withheld because project_id was omitted. Pass the project_id returned by init/context; reconnect if the current client schema does not expose it."
                .to_string(),
        ));
    }
}

/// Append the bootstrap hint to the textual response (separator " — ").
fn compose_text(base: String, note: Option<&str>) -> String {
    match note {
        Some(note) => format!("{} {}", base, note),
        None => base,
    }
}

/// Build the empty-result fallback for `instruct(get)` when the API has no
/// state yet (404/422) or when no workspace scope could be resolved at all.
fn empty_get_result(
    session_id: &str,
    workspace_id: Option<Uuid>,
    project_id: Option<Uuid>,
    bootstrapped: bool,
    bootstrap_note: Option<&str>,
) -> ToolResult {
    let scope_status = if workspace_id.is_some() && project_id.is_none() {
        "workspace_only"
    } else if workspace_id.is_some() {
        "empty"
    } else {
        "missing"
    };
    let hint = if workspace_id.is_some() && project_id.is_none() {
        "No workspace/global instruction entries are available. Project-scoped entries were intentionally withheld because project_id was omitted. Pass the project_id returned by init/context; reconnect if the current client schema does not expose it."
    } else if workspace_id.is_some() {
        "Session has no instruction entries yet. They appear here after init/context populates the cache."
    } else {
        "No workspace scope is initialized. Run init(folder_path=\"...\") to enable instruction delivery for this session."
    };
    let text = compose_text(
        format!("Loaded 0 instruction entries. {}", hint),
        bootstrap_note,
    );
    let mut payload = serde_json::json!({
        "entries": [],
        "session_id": session_id,
        "workspace_id": workspace_id.map(|id| id.to_string()),
        "project_id": project_id.map(|id| id.to_string()),
        "scope_status": scope_status,
        "bootstrapped": bootstrapped,
        "hint": hint,
    });
    attach_bootstrap_note(&mut payload, bootstrap_note);
    ToolResult::with_structured(text, payload)
}

/// Build a drift-result fallback for `instruct(get)` when the API rejected
/// the read with 403/401. The session is bound to a workspace the current
/// credentials can't access — return zero entries with a hint that points
/// at the real fix (re-init), not at the agent.
fn workspace_drift_get_result(
    session_id: &str,
    workspace_id: Option<Uuid>,
    project_id: Option<Uuid>,
    bootstrapped: bool,
    bootstrap_note: Option<&str>,
) -> ToolResult {
    let workspace_phrase = workspace_id
        .map(|id| format!("workspace {} ", id))
        .unwrap_or_else(|| "the bound workspace ".to_string());
    let hint = format!(
        "Session is bound to {}which the current credentials can't access. \
         Re-run init(folder_path=\"...\") to rebind this session — instruct stays inert until then.",
        workspace_phrase
    );
    let text = compose_text(
        format!(
            "Loaded 0 instruction entries (workspace access drift). {}",
            hint
        ),
        bootstrap_note,
    );
    let mut payload = serde_json::json!({
        "entries": [],
        "session_id": session_id,
        "workspace_id": workspace_id.map(|id| id.to_string()),
        "project_id": project_id.map(|id| id.to_string()),
        "scope_status": "drift",
        "bootstrapped": bootstrapped,
        "hint": hint,
    });
    attach_bootstrap_note(&mut payload, bootstrap_note);
    ToolResult::with_structured(text, payload)
}

/// Build the empty-result fallback for `instruct(stats)` analogous to
/// `empty_get_result`.
fn empty_stats_result(
    session_id: &str,
    workspace_id: Option<Uuid>,
    bootstrapped: bool,
    bootstrap_note: Option<&str>,
) -> ToolResult {
    let scope_status = if workspace_id.is_some() {
        "empty"
    } else {
        "missing"
    };
    let hint = if workspace_id.is_some() {
        "Session has no instruction state yet."
    } else {
        "No workspace scope is initialized. Run init(folder_path=\"...\") to enable instruction delivery for this session."
    };
    let text = compose_text(
        format!("Session stats loaded (version 0, instructions 0). {}", hint),
        bootstrap_note,
    );
    let mut payload = serde_json::json!({
        "version": 0,
        "instruction_count": 0,
        "session_id": session_id,
        "workspace_id": workspace_id.map(|id| id.to_string()),
        "scope_status": scope_status,
        "bootstrapped": bootstrapped,
        "hint": hint,
    });
    attach_bootstrap_note(&mut payload, bootstrap_note);
    ToolResult::with_structured(text, payload)
}

/// Drift-result analogue of `empty_stats_result` for 403/401 on `instruct(stats)`.
fn workspace_drift_stats_result(
    session_id: &str,
    workspace_id: Option<Uuid>,
    bootstrapped: bool,
    bootstrap_note: Option<&str>,
) -> ToolResult {
    let workspace_phrase = workspace_id
        .map(|id| format!("workspace {} ", id))
        .unwrap_or_else(|| "the bound workspace ".to_string());
    let hint = format!(
        "Session is bound to {}which the current credentials can't access. \
         Re-run init(folder_path=\"...\") to rebind this session.",
        workspace_phrase
    );
    let text = compose_text(
        format!(
            "Session stats unavailable (workspace access drift). {}",
            hint
        ),
        bootstrap_note,
    );
    let mut payload = serde_json::json!({
        "version": 0,
        "instruction_count": 0,
        "session_id": session_id,
        "workspace_id": workspace_id.map(|id| id.to_string()),
        "scope_status": "drift",
        "bootstrapped": bootstrapped,
        "hint": hint,
    });
    attach_bootstrap_note(&mut payload, bootstrap_note);
    ToolResult::with_structured(text, payload)
}

#[async_trait]
impl ToolHandler for FlashTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let input: FlashInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;

        let action = input.action.to_lowercase();
        let explicit_workspace_id = Self::parse_scope_id(&input.workspace_id, "workspace_id")?;
        let explicit_project_id = Self::parse_scope_id(&input.project_id, "project_id")?;
        let session_id = Self::require_session_id(input.session_id)?;

        let scope = self
            .resolve_or_bootstrap_scope(explicit_workspace_id, explicit_project_id, &session_id)
            .await?;
        let workspace_id = scope.workspace_id;
        let project_id = scope.project_id;
        let bootstrap_note = if scope.bootstrapped {
            let ws_name = scope.workspace_name.as_deref().unwrap_or("workspace");
            Some(format!(
                "Auto-bootstrapped fallback workspace '{}'. Run init(folder_path=\"...\") to attach this session to a real project.",
                ws_name
            ))
        } else {
            None
        };

        match action.as_str() {
            "bootstrap" => {
                let params = FlashBootstrapParams {
                    workspace_id,
                    session_id,
                };
                let mut result = self.client.flash_bootstrap(params).await?;
                let version = result.get("version").and_then(|v| v.as_u64()).unwrap_or(0);
                attach_bootstrap_note(&mut result, bootstrap_note.as_deref());
                let text = compose_text(
                    format!("Session bootstrapped (version {}).", version),
                    bootstrap_note.as_deref(),
                );
                Ok(ToolResult::with_structured(text, result))
            }
            "get" => {
                // Do not let `FlashGetParams::with_defaults` resurrect a stale
                // process-wide workspace/project when a hosted caller omitted
                // scope. Frozen clients degrade to a safe empty result.
                if workspace_id.is_none() {
                    return Ok(empty_get_result(
                        &session_id,
                        None,
                        None,
                        scope.bootstrapped,
                        bootstrap_note.as_deref(),
                    ));
                }
                let params = FlashGetParams {
                    workspace_id,
                    project_id,
                    session_id: session_id.clone(),
                    limit: input.limit,
                };
                match self.client.flash_get(params).await {
                    Ok(mut result) => {
                        let count = result
                            .get("entries")
                            .and_then(|v| v.as_array())
                            .map(|v| v.len())
                            .unwrap_or(0);
                        attach_bootstrap_note(&mut result, bootstrap_note.as_deref());
                        attach_workspace_only_note(&mut result, project_id);
                        let text = compose_text(
                            if project_id.is_none() {
                                format!(
                                    "Loaded {} workspace/global instruction entries. Project-scoped entries were intentionally withheld because project_id was omitted.",
                                    count
                                )
                            } else {
                                format!("Loaded {} instruction entries.", count)
                            },
                            bootstrap_note.as_deref(),
                        );
                        Ok(ToolResult::with_structured(text, result))
                    }
                    Err(err) if is_recoverable_read_error(&err) => Ok(empty_get_result(
                        &session_id,
                        workspace_id,
                        project_id,
                        scope.bootstrapped,
                        bootstrap_note.as_deref(),
                    )),
                    Err(err) if is_workspace_access_error(&err) => Ok(workspace_drift_get_result(
                        &session_id,
                        workspace_id,
                        project_id,
                        scope.bootstrapped,
                        bootstrap_note.as_deref(),
                    )),
                    Err(err) => Err(err),
                }
            }
            "push" => {
                let entries = input.entries.unwrap_or_default();
                let params = FlashPushParams {
                    workspace_id,
                    session_id,
                    entries: Self::map_entries(entries),
                    increment_turn: input.increment_turn,
                    force_version_bump: input.force_version_bump,
                };
                let mut result = self.client.flash_push(params).await?;
                let accepted = result.get("accepted").and_then(|v| v.as_u64()).unwrap_or(0);
                let suppressed = result
                    .get("suppressed")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                attach_bootstrap_note(&mut result, bootstrap_note.as_deref());
                let text = compose_text(
                    format!(
                        "Pushed instruction entries (accepted {}, suppressed {}).",
                        accepted, suppressed
                    ),
                    bootstrap_note.as_deref(),
                );
                Ok(ToolResult::with_structured(text, result))
            }
            "ack" => {
                let params = FlashAckParams {
                    workspace_id,
                    session_id,
                    ids: input.ids.unwrap_or_default(),
                };
                let mut result = self.client.flash_ack(params).await?;
                let acked = result.get("acked").and_then(|v| v.as_u64()).unwrap_or(0);
                attach_bootstrap_note(&mut result, bootstrap_note.as_deref());
                let text = compose_text(
                    format!("Acknowledged {} instruction entries.", acked),
                    bootstrap_note.as_deref(),
                );
                Ok(ToolResult::with_structured(text, result))
            }
            "clear" => {
                let params = FlashClearParams {
                    workspace_id,
                    session_id,
                };
                let mut result = self.client.flash_clear(params).await?;
                attach_bootstrap_note(&mut result, bootstrap_note.as_deref());
                let text = compose_text(
                    "Cleared session instruction state.".to_string(),
                    bootstrap_note.as_deref(),
                );
                Ok(ToolResult::with_structured(text, result))
            }
            "stats" => {
                let params = FlashStatsParams {
                    workspace_id,
                    session_id: session_id.clone(),
                };
                match self.client.flash_stats(params).await {
                    Ok(mut result) => {
                        let version = result.get("version").and_then(|v| v.as_u64()).unwrap_or(0);
                        let instructions = result
                            .get("instruction_count")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        attach_bootstrap_note(&mut result, bootstrap_note.as_deref());
                        let text = compose_text(
                            format!(
                                "Session stats loaded (version {}, instructions {}).",
                                version, instructions
                            ),
                            bootstrap_note.as_deref(),
                        );
                        Ok(ToolResult::with_structured(text, result))
                    }
                    Err(err) if is_recoverable_read_error(&err) => Ok(empty_stats_result(
                        &session_id,
                        workspace_id,
                        scope.bootstrapped,
                        bootstrap_note.as_deref(),
                    )),
                    Err(err) if is_workspace_access_error(&err) => {
                        Ok(workspace_drift_stats_result(
                            &session_id,
                            workspace_id,
                            scope.bootstrapped,
                            bootstrap_note.as_deref(),
                        ))
                    }
                    Err(err) => Err(err),
                }
            }
            "checkpoint" => {
                let params = FlashCheckpointParams {
                    workspace_id,
                    session_id,
                };
                let mut result = self.client.flash_checkpoint(params).await?;
                let version = result.get("version").and_then(|v| v.as_u64()).unwrap_or(0);
                attach_bootstrap_note(&mut result, bootstrap_note.as_deref());
                let text = compose_text(
                    format!("Checkpoint saved (version {}).", version),
                    bootstrap_note.as_deref(),
                );
                Ok(ToolResult::with_structured(text, result))
            }
            "verify" => {
                let params = FlashVerifyParams {
                    workspace_id,
                    session_id,
                    expected_version: input.expected_version,
                };
                let mut result = self.client.flash_verify(params).await?;
                let verified = result
                    .get("verified")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let source = result
                    .get("source")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                attach_bootstrap_note(&mut result, bootstrap_note.as_deref());
                let text = compose_text(
                    format!(
                        "Checkpoint verification: {} (source: {}).",
                        if verified { "passed" } else { "failed" },
                        source
                    ),
                    bootstrap_note.as_deref(),
                );
                Ok(ToolResult::with_structured(text, result))
            }
            _ => Err(Error::Validation(format!(
                "Unknown action: {}. Available actions: {}",
                action,
                VALID_ACTIONS.join(", ")
            ))),
        }
    }

    fn metadata(&self) -> &ToolMetadata {
        static METADATA: std::sync::OnceLock<ToolMetadata> = std::sync::OnceLock::new();
        METADATA.get_or_init(|| ToolMetadata {
            name: "flash".to_string(),
            title: "Session Instructions (flash alias)".to_string(),
            description: "Alias of instruct. Session-scoped instruction cache operations. Actions: bootstrap, get, push, ack, clear, stats, checkpoint, verify.".to_string(),
            category: ToolCategory::Session,
            annotations: ToolAnnotations::destructive(),
            is_pro: false,
            required_tier: None,
        })
    }

    fn input_schema(&self) -> Value {
        SchemaBuilder::new()
            .description("Session-scoped instruction cache operations")
            .string_enum("action", "Action to perform", VALID_ACTIONS, true)
            .uuid(
                "workspace_id",
                "Workspace ID. Pass the current workspace_id returned by init/context. Hosted instruct(get) fails closed when omitted so client defaults cannot resurrect stale scope.",
                false,
            )
            .uuid(
                "project_id",
                "Project ID (for get). Pass the current project_id returned by init/context. Omission is workspace-only on hosted HTTP; local stdio may inherit an exact matching active session.",
                false,
            )
            .string("session_id", "Session identifier", true)
            .integer("limit", "Maximum entries (for get)", false)
            .property(
                "entries",
                serde_json::json!({
                    "type": "array",
                    "description": "Entries to push (for push)",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": { "type": "string", "description": "Optional entry id" },
                            "text": { "type": "string", "description": "Instruction text" },
                            "source": { "type": "string", "description": "Optional source label" },
                            "critical": { "type": "boolean", "description": "Mark as critical" },
                            "surface": { "type": "boolean", "description": "Whether to surface in consume/get" },
                            "metadata": { "type": "object", "description": "Optional metadata", "additionalProperties": true }
                        },
                        "required": ["text"],
                        "additionalProperties": false
                    }
                }),
                false,
            )
            .boolean("increment_turn", "Increment turn counter (for push)", false)
            .boolean(
                "force_version_bump",
                "Force version bump even with no new entries (for push)",
                false,
            )
            .array("ids", "Entry IDs to acknowledge (for ack)", "string", false)
            .integer("expected_version", "Expected version for checkpoint verify", false)
            .build()
    }
}

/// Register instruction cache tools — canonical name `instruct`, plus
/// the compatibility alias `flash` for callers that came in via the
/// historical name. The shorter `ram` and `mem` aliases were dropped
/// in v0.3.2 because nothing in the wild was calling them.
pub fn register_flash_tools(
    registry: &mut crate::registry::ToolRegistry,
    client: ContextStreamClient,
    session: Arc<SessionManager>,
) {
    registry.register(
        "instruct",
        Arc::new(InstructTool::new(client.clone(), session)),
    );
}

/// Instruct — canonical name for session-scoped instruction cache.
pub struct InstructTool {
    inner: FlashTool,
}

impl InstructTool {
    pub fn new(client: ContextStreamClient, session: Arc<SessionManager>) -> Self {
        Self {
            inner: FlashTool::new(client, session),
        }
    }
}

#[async_trait]
impl ToolHandler for InstructTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        self.inner.execute(input).await
    }

    fn metadata(&self) -> &ToolMetadata {
        static METADATA: std::sync::OnceLock<ToolMetadata> = std::sync::OnceLock::new();
        METADATA.get_or_init(|| ToolMetadata {
            name: "instruct".to_string(),
            title: "Session Instructions".to_string(),
            description:
                "Session-scoped instruction cache operations. Actions: bootstrap, get, push, ack, clear, stats, checkpoint, verify."
                    .to_string(),
            category: ToolCategory::Session,
            annotations: ToolAnnotations::destructive(),
            is_pro: false,
            required_tier: None,
        })
    }

    fn input_schema(&self) -> Value {
        self.inner.input_schema()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TestFixtures;
    use mcp_client::run_with_session_key;
    use mcp_types::tool::ContentItem;

    fn make_tool() -> FlashTool {
        let client = ContextStreamClient::new(TestFixtures::test_config());
        let session = Arc::new(SessionManager::new(
            client.clone(),
            TestFixtures::test_config(),
        ));
        FlashTool::new(client, session)
    }

    #[test]
    fn recoverable_read_error_matches_404_and_422() {
        assert!(is_recoverable_read_error(&Error::http(404, "missing")));
        assert!(is_recoverable_read_error(&Error::http(422, "validation")));
    }

    #[test]
    fn recoverable_read_error_does_not_match_other_codes() {
        assert!(!is_recoverable_read_error(&Error::http(500, "boom")));
        assert!(!is_recoverable_read_error(&Error::http(429, "slow down")));
        assert!(!is_recoverable_read_error(&Error::Validation(
            "bad input".into()
        )));
    }

    #[test]
    fn workspace_access_error_matches_403_and_401() {
        assert!(is_workspace_access_error(&Error::http(
            403,
            "Forbidden: You do not have access to this workspace"
        )));
        assert!(is_workspace_access_error(&Error::http(401, "Unauthorized")));
    }

    #[test]
    fn workspace_access_error_does_not_swallow_other_codes() {
        // Reads still bubble for 404/422 (handled by is_recoverable_read_error
        // which produces an "empty" result, not "drift") and for everything
        // else (5xx, 429, validation).
        assert!(!is_workspace_access_error(&Error::http(404, "missing")));
        assert!(!is_workspace_access_error(&Error::http(422, "validation")));
        assert!(!is_workspace_access_error(&Error::http(500, "boom")));
        assert!(!is_workspace_access_error(&Error::http(429, "slow down")));
        assert!(!is_workspace_access_error(&Error::Validation("bad".into())));
    }

    #[test]
    fn workspace_drift_get_result_signals_drift_with_workspace_id() {
        // Deterministic id with no "403"/"Forbidden" substring: the assertion below
        // checks the drift text doesn't leak raw HTTP wording, and a random v4 UUID
        // can itself contain "403" (valid hex), which made this test flaky.
        let ws = Uuid::from_u128(0x1234_5678_9abc_4def_8123_4567_89ab_cdef);
        let result = workspace_drift_get_result("session-1", Some(ws), None, false, None);
        assert!(!result.is_error);
        let structured = result.structured_content.expect("structured");
        assert_eq!(
            structured.get("scope_status"),
            Some(&serde_json::json!("drift"))
        );
        assert_eq!(structured.get("entries"), Some(&serde_json::json!([])));
        assert_eq!(
            structured.get("workspace_id"),
            Some(&serde_json::json!(ws.to_string()))
        );
        let hint = structured
            .get("hint")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        assert!(hint.contains("Re-run init"));
        assert!(hint.contains(&ws.to_string()));
        match result.content.first() {
            Some(ContentItem::Text { text }) => {
                assert!(
                    text.contains("workspace access drift"),
                    "expected drift marker, got: {text}"
                );
                assert!(
                    !text.contains("403") && !text.contains("Forbidden"),
                    "raw HTTP code/wording must not leak to agents: {text}"
                );
            }
            other => panic!("expected text content, got {:?}", other),
        }
    }

    #[test]
    fn workspace_drift_get_result_handles_missing_workspace_id() {
        let result = workspace_drift_get_result("session-1", None, None, false, None);
        let structured = result.structured_content.expect("structured");
        assert_eq!(
            structured.get("workspace_id"),
            Some(&serde_json::Value::Null)
        );
        let hint = structured
            .get("hint")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        assert!(hint.contains("the bound workspace"));
    }

    #[test]
    fn workspace_drift_stats_result_signals_drift() {
        let ws = Uuid::new_v4();
        let result = workspace_drift_stats_result("session-1", Some(ws), false, None);
        let structured = result.structured_content.expect("structured");
        assert_eq!(
            structured.get("scope_status"),
            Some(&serde_json::json!("drift"))
        );
        assert_eq!(
            structured.get("instruction_count"),
            Some(&serde_json::json!(0))
        );
        match result.content.first() {
            Some(ContentItem::Text { text }) => {
                assert!(text.contains("workspace access drift"));
                assert!(text.contains("Re-run init"));
            }
            other => panic!("expected text content, got {:?}", other),
        }
    }

    #[test]
    fn empty_get_result_signals_missing_scope_when_no_workspace() {
        let result = empty_get_result("session-1", None, None, false, None);
        assert!(!result.is_error);
        let structured = result.structured_content.expect("structured");
        assert_eq!(
            structured.get("scope_status"),
            Some(&serde_json::json!("missing"))
        );
        assert_eq!(structured.get("entries"), Some(&serde_json::json!([])));
        match result.content.first() {
            Some(ContentItem::Text { text }) => {
                assert!(text.contains("Loaded 0 instruction entries"));
                assert!(text.contains("Run init"));
            }
            other => panic!("expected text content, got {:?}", other),
        }
    }

    #[test]
    fn empty_get_result_signals_empty_when_workspace_present() {
        let ws = Uuid::new_v4();
        let result = empty_get_result("session-1", Some(ws), None, false, None);
        let structured = result.structured_content.expect("structured");
        assert_eq!(
            structured.get("scope_status"),
            Some(&serde_json::json!("workspace_only"))
        );
        assert_eq!(
            structured.get("workspace_id"),
            Some(&serde_json::json!(ws.to_string()))
        );
    }

    #[test]
    fn empty_get_result_includes_bootstrap_note_when_provided() {
        let result = empty_get_result(
            "session-1",
            Some(Uuid::new_v4()),
            None,
            true,
            Some("Auto-bootstrapped fallback workspace 'foo'."),
        );
        let structured = result.structured_content.expect("structured");
        assert_eq!(
            structured.get("bootstrapped"),
            Some(&serde_json::json!(true))
        );
        assert!(structured
            .get("bootstrap_note")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .contains("Auto-bootstrapped"));
    }

    #[test]
    fn compose_text_appends_note_when_present() {
        assert_eq!(
            compose_text("base.".to_string(), Some("hint")),
            "base. hint"
        );
        assert_eq!(compose_text("base.".to_string(), None), "base.");
    }

    #[test]
    fn attach_bootstrap_note_inserts_only_once() {
        let mut payload = serde_json::json!({});
        attach_bootstrap_note(&mut payload, Some("first"));
        attach_bootstrap_note(&mut payload, Some("second"));
        assert_eq!(
            payload.get("bootstrap_note"),
            Some(&serde_json::json!("first"))
        );
    }

    #[tokio::test]
    async fn flash_tool_requires_session_id() {
        let tool = make_tool();
        let result = tool.execute(serde_json::json!({ "action": "get" })).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("session_id"));
    }

    #[test]
    fn flash_tool_schema_exposes_optional_project_id() {
        let schema = make_tool().input_schema();
        assert_eq!(
            schema.pointer("/properties/project_id/type"),
            Some(&serde_json::json!("string"))
        );
        assert!(!schema
            .get("required")
            .and_then(Value::as_array)
            .is_some_and(|required| required.iter().any(|field| field == "project_id")));
    }

    #[tokio::test]
    async fn flash_get_scope_tracks_same_session_project_switch() {
        let config = mcp_types::Config::default();
        let client = ContextStreamClient::new(config.clone());
        let session = Arc::new(SessionManager::new(client.clone(), config));
        let workspace_id = Uuid::new_v4();
        let project_a = Uuid::new_v4();
        let project_b = Uuid::new_v4();
        session
            .initialize_with_session_id(
                Some(workspace_id),
                Some(project_a),
                None,
                None,
                Some("same-mcp-session".to_string()),
            )
            .await;
        let tool = FlashTool::new(client, session.clone());

        let scope_a = run_with_session_key(SessionKey::Local, || async {
            tool.resolve_or_bootstrap_scope(Some(workspace_id), None, "same-mcp-session")
                .await
        })
        .await
        .expect("resolve project A");
        assert_eq!(scope_a.project_id, Some(project_a));

        session
            .update_scope(Some(workspace_id), Some(project_b), None)
            .await;
        let scope_b = run_with_session_key(SessionKey::Local, || async {
            tool.resolve_or_bootstrap_scope(Some(workspace_id), None, "same-mcp-session")
                .await
        })
        .await
        .expect("resolve project B");
        assert_eq!(scope_b.project_id, Some(project_b));
        assert_ne!(scope_b.project_id, scope_a.project_id);
        assert_eq!(
            session.state().await.session_id.as_deref(),
            Some("same-mcp-session")
        );
    }

    #[tokio::test]
    async fn flash_get_scope_does_not_inherit_project_across_workspaces() {
        let config = mcp_types::Config::default();
        let client = ContextStreamClient::new(config.clone());
        let session = Arc::new(SessionManager::new(client.clone(), config));
        let active_workspace = Uuid::new_v4();
        let other_workspace = Uuid::new_v4();
        session
            .initialize(Some(active_workspace), Some(Uuid::new_v4()), None, None)
            .await;
        let tool = FlashTool::new(client, session);

        let scope = run_with_session_key(SessionKey::Local, || async {
            tool.resolve_or_bootstrap_scope(Some(other_workspace), None, "local-session")
                .await
        })
        .await
        .expect("resolve explicit workspace");
        assert_eq!(scope.workspace_id, Some(other_workspace));
        assert_eq!(scope.project_id, None);
    }

    #[tokio::test]
    async fn hosted_flash_get_scope_never_infers_stale_project() {
        let workspace_id = Uuid::new_v4();
        let stale_project = Uuid::new_v4();
        let config = mcp_types::Config {
            default_workspace_id: Some(workspace_id),
            default_project_id: Some(stale_project),
            ..Default::default()
        };
        let client = ContextStreamClient::new(config.clone());
        let session = Arc::new(SessionManager::new(client.clone(), config));
        let tool = FlashTool::new(client, session.clone());

        for key in [
            SessionKey::Jwt("jwt-caller".to_string()),
            SessionKey::ApiKey("api-key-caller".to_string()),
            SessionKey::AnonymousHttp("anonymous-request".to_string()),
        ] {
            let scope = run_with_session_key(key, || async {
                session
                    .initialize_with_session_id(
                        Some(workspace_id),
                        Some(stale_project),
                        None,
                        None,
                        Some("same-hosted-session".to_string()),
                    )
                    .await;
                tool.resolve_or_bootstrap_scope(Some(workspace_id), None, "same-hosted-session")
                    .await
            })
            .await
            .expect("resolve hosted scope");

            assert_eq!(scope.workspace_id, Some(workspace_id));
            assert_eq!(scope.project_id, None);
        }
    }

    #[tokio::test]
    async fn missing_transport_identity_never_infers_local_project() {
        let workspace_id = Uuid::new_v4();
        let stale_project = Uuid::new_v4();
        let config = mcp_types::Config {
            default_workspace_id: Some(workspace_id),
            default_project_id: Some(stale_project),
            ..Default::default()
        };
        let client = ContextStreamClient::new(config.clone());
        let session = Arc::new(SessionManager::new(client.clone(), config));
        session
            .initialize_with_session_id(
                Some(workspace_id),
                Some(stale_project),
                None,
                None,
                Some("same-session".to_string()),
            )
            .await;
        let tool = FlashTool::new(client, session);

        let scope = tool
            .resolve_or_bootstrap_scope(Some(workspace_id), None, "same-session")
            .await
            .expect("resolve unmarked scope");
        assert_eq!(scope.workspace_id, Some(workspace_id));
        assert_eq!(scope.project_id, None);
    }

    #[tokio::test]
    async fn local_flash_get_scope_requires_exact_instruction_session() {
        let workspace_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        let config = mcp_types::Config::default();
        let client = ContextStreamClient::new(config.clone());
        let session = Arc::new(SessionManager::new(client.clone(), config));
        session
            .initialize_with_session_id(
                Some(workspace_id),
                Some(project_id),
                None,
                None,
                Some("active-session".to_string()),
            )
            .await;
        let tool = FlashTool::new(client, session);

        let scope = run_with_session_key(SessionKey::Local, || async {
            tool.resolve_or_bootstrap_scope(Some(workspace_id), None, "different-session")
                .await
        })
        .await
        .expect("resolve mismatched local session");
        assert_eq!(scope.workspace_id, Some(workspace_id));
        assert_eq!(scope.project_id, None);
    }

    #[tokio::test]
    async fn explicit_project_always_wins_for_hosted_flash_get() {
        let workspace_id = Uuid::new_v4();
        let stale_project = Uuid::new_v4();
        let requested_project = Uuid::new_v4();
        let config = mcp_types::Config::default();
        let client = ContextStreamClient::new(config.clone());
        let session = Arc::new(SessionManager::new(client.clone(), config));
        let tool = FlashTool::new(client, session);

        let scope = run_with_session_key(SessionKey::Jwt("jwt-caller".to_string()), || async {
            tool.resolve_or_bootstrap_scope(
                Some(workspace_id),
                Some(requested_project),
                "hosted-session",
            )
            .await
        })
        .await
        .expect("resolve explicit hosted project");
        assert_eq!(scope.workspace_id, Some(workspace_id));
        assert_eq!(scope.project_id, Some(requested_project));
        assert_ne!(scope.project_id, Some(stale_project));
    }

    #[tokio::test]
    async fn hosted_flash_get_without_explicit_workspace_returns_before_http() {
        let tool = make_tool();
        let result = run_with_session_key(SessionKey::Jwt("jwt-caller".to_string()), || async {
            tool.execute(serde_json::json!({
                "action": "get",
                "session_id": "hosted-session"
            }))
            .await
        })
        .await
        .expect("safe empty result");
        let structured = result.structured_content.expect("structured");
        assert_eq!(
            structured.get("scope_status"),
            Some(&serde_json::json!("missing"))
        );
        assert_eq!(structured.get("entries"), Some(&serde_json::json!([])));
    }

    #[tokio::test]
    async fn flash_tool_rejects_invalid_project_id() {
        let tool = make_tool();
        let result = tool
            .execute(serde_json::json!({
                "action": "get",
                "session_id": "codex-test",
                "project_id": "not-a-uuid"
            }))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("project_id"));
    }

    #[tokio::test]
    async fn flash_tool_rejects_unknown_action_after_session_resolution() {
        let tool = make_tool();
        let result = tool
            .execute(serde_json::json!({
                "action": "frobnicate",
                "session_id": "codex-test"
            }))
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Unknown action"), "{}", err);
    }
}
