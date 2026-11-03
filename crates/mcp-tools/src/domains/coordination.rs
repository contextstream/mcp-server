//! Cross-workspace agent coordination.

use async_trait::async_trait;
use mcp_client::ContextStreamClient;
use mcp_types::{
    tool::{ToolAnnotations, ToolCategory, ToolMetadata, ToolResult},
    Error, Result,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;

use crate::registry::ToolHandler;
use crate::schema::SchemaBuilder;

const VALID_ACTIONS: &[&str] = &[
    "check_in", "inbox", "list", "get", "share", "ack", "reply", "dismiss", "settings",
];

/// Coordination item kinds accepted by `POST /coordinations`. Validated
/// client-side so a typo fails fast instead of round-tripping a 4xx. This
/// list must stay identical to the API's `coordination_kind_is_known`
/// (decision, constraint, api_contract, risk, status, knowledge): 1.0.1
/// shipped a divergent list and every `status`/`risk` share failed client-side
/// while the only client-accepted default (`note`) was refused by the API.
pub const VALID_KINDS: &[&str] = &[
    "decision",
    "constraint",
    "api_contract",
    "risk",
    "status",
    "knowledge",
];

/// Kind applied when `share` omits one; matches the API default.
pub const DEFAULT_KIND: &str = "knowledge";

/// Default number of `[COORDINATION]` lines rendered by `context()` /
/// `session(action="ground")` before the `… N more` trailer.
pub const NOTICE_RENDER_LIMIT: usize = 5;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoordinationInput {
    pub action: String,
    pub workspace_id: Option<String>,
    pub project_id: Option<String>,
    pub session_id: Option<String>,
    pub task_summary: Option<String>,
    pub id: Option<String>,
    pub notice_id: Option<String>,
    pub title: Option<String>,
    pub summary: Option<String>,
    pub why_it_matters: Option<String>,
    pub kind: Option<String>,
    pub target_workspace_ids: Option<Vec<String>>,
    pub target_project_ids: Option<Vec<String>>,
    pub enabled: Option<bool>,
    pub limit: Option<i64>,
    /// Reply text for action=reply.
    pub message: Option<String>,
    /// Presence metadata for check_in, e.g. {"git": {"branch", "commit"}}.
    pub metadata: Option<Value>,
    /// For action=share: the one agent (e.g. "cc-qa") or session id of the
    /// sender's own the share is for. Only that agent's sessions receive it.
    pub to: Option<String>,
}

// A notice action names its workspace so the API looks the notice up on that
// workspace's home primary (#309).
const SCOPE_DEFAULTED_ACTIONS: &[&str] = &[
    "check_in", "inbox", "list", "share", "settings", "get", "ack", "reply", "dismiss",
];

/// Fill the scope a call left out from the session's. Returns true when the
/// session's project was not borrowed because the call names another
/// workspace than the session's.
fn fill_scope_defaults(
    input: &mut CoordinationInput,
    (workspace, project): (Option<Uuid>, Option<Uuid>),
) -> bool {
    let explicit_ws = input
        .workspace_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let explicit_proj = input
        .project_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let names_another_workspace = explicit_ws.is_some_and(|raw| match Uuid::parse_str(raw) {
        Ok(named) => Some(named) != workspace,
        Err(_) => true,
    });
    let (gave_workspace, gave_project) = (explicit_ws.is_some(), explicit_proj.is_some());
    if !gave_workspace {
        input.workspace_id = workspace.map(|id| id.to_string());
    }
    if !gave_project && !names_another_workspace {
        input.project_id = project.map(|id| id.to_string());
    }
    !gave_project && names_another_workspace && project.is_some()
}

pub struct CoordinationTool {
    client: ContextStreamClient,
    session: Option<Arc<mcp_session::SessionManager>>,
}

impl CoordinationTool {
    pub fn new(client: ContextStreamClient) -> Self {
        Self {
            client,
            session: None,
        }
    }

    pub fn with_session(mut self, session: Arc<mcp_session::SessionManager>) -> Self {
        self.session = Some(session);
        self
    }

    fn parse_uuid(value: &Option<String>, field: &str) -> Result<Option<Uuid>> {
        match value.as_deref().map(str::trim) {
            Some("") | None => Ok(None),
            Some(s) => {
                Ok(Some(Uuid::parse_str(s).map_err(|_| {
                    Error::Validation(format!("Invalid {field}"))
                })?))
            }
        }
    }

    /// A coordination item or notice id: a whole UUID, or the start of one (8+
    /// characters, as reports quote them) that names exactly one item or
    /// notice in the caller's own inbox. Never another account's.
    async fn coordination_id(
        &self,
        raw: &str,
        param: &str,
        workspace_id: Option<Uuid>,
        project_id: Option<Uuid>,
    ) -> Result<Uuid> {
        let raw = raw.trim();
        if let Ok(id) = Uuid::parse_str(raw) {
            return Ok(id);
        }
        if crate::domains::lookup::id_prefix(raw).is_none() {
            return Err(Error::refused(
                mcp_types::ValidationReason::InvalidValue,
                format!(
                    "{param} \"{raw}\" is not an id: pass the whole 36-character id, or its first 8 or more characters, as coordination(action=\"inbox\") lists them."
                ),
            ));
        }
        let session_id = match self.session.as_deref() {
            Some(session) => session.state().await.api_session_id.clone(),
            None => None,
        };
        let inbox = self
            .client
            .coordination_inbox(workspace_id, project_id, session_id.as_deref(), Some(200))
            .await?;
        let ids = ["items", "notices"]
            .iter()
            .filter_map(|key| inbox.get(*key).and_then(Value::as_array))
            .flatten()
            .filter_map(|entry| entry.get("id").and_then(Value::as_str))
            .filter_map(|id| Uuid::parse_str(id).ok());
        let prefix = raw.to_ascii_lowercase();
        let mut matching = crate::domains::lookup::ids_starting_with(&prefix, ids);
        matching.sort();
        matching.dedup();
        match matching.as_slice() {
            [one] => Ok(*one),
            _ => Err(Error::refused(
                mcp_types::ValidationReason::ReferenceNotFound,
                format!(
                    "No single coordination item or notice in your inbox has {param} {}",
                    crate::domains::lookup::id_prefix_hint(raw, &matching, false)
                        .map(|hint| format!("starting \"{raw}\". {hint}"))
                        .unwrap_or_else(|| format!("\"{raw}\"."))
                ),
            )),
        }
    }

    fn parse_uuid_list(values: &Option<Vec<String>>) -> Result<Vec<Uuid>> {
        let Some(values) = values else {
            return Ok(Vec::new());
        };
        values
            .iter()
            .map(|s| {
                Uuid::parse_str(s).map_err(|_| Error::Validation(format!("Invalid uuid: {s}")))
            })
            .collect()
    }
}

#[async_trait]
impl ToolHandler for CoordinationTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let mut input: CoordinationInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;
        let action = input.action.to_lowercase();
        let mut project_not_borrowed = false;
        if SCOPE_DEFAULTED_ACTIONS.contains(&action.as_str()) {
            let defaults =
                super::scope::read_scope_defaults(&self.client, self.session.as_deref()).await;
            project_not_borrowed = fill_scope_defaults(&mut input, defaults);
        }
        let workspace_id = Self::parse_uuid(&input.workspace_id, "workspace_id")?;
        let project_id = Self::parse_uuid(&input.project_id, "project_id")?;

        match action.as_str() {
            "check_in" => {
                let session_id = input.session_id.ok_or_else(|| {
                    Error::Validation("session_id is required for check_in".into())
                })?;
                // The agent's handle lets a share that names it reach this
                // session (#310); one the caller sent wins.
                let mut metadata = input
                    .metadata
                    .clone()
                    .filter(Value::is_object)
                    .unwrap_or_else(|| serde_json::json!({}));
                if metadata.get("agent_name").is_none() {
                    let folder = match self.session.as_deref() {
                        Some(session) => session.state().await.folder_path,
                        None => None,
                    };
                    if let Some(handle) = mcp_client::agent_handle::agent_handle(folder.as_deref())
                    {
                        metadata["agent_name"] = serde_json::json!(handle);
                    }
                }
                let result = self
                    .client
                    .coordination_check_in(
                        workspace_id,
                        project_id,
                        &session_id,
                        input.task_summary.as_deref(),
                        Some(&metadata),
                    )
                    .await?;
                Ok(ToolResult::with_structured(
                    format!(
                        "Coordination check-in recorded for session {session_id}. context() already heartbeats presence."
                    ),
                    result,
                ))
            }
            "inbox" | "list" => {
                let result = self
                    .client
                    .coordination_inbox(
                        workspace_id,
                        project_id,
                        input.session_id.as_deref(),
                        input.limit,
                    )
                    .await?;
                Ok(ToolResult::with_structured(format_inbox(&result), result))
            }
            "get" => {
                let id = input
                    .id
                    .as_deref()
                    .or(input.notice_id.as_deref())
                    .ok_or_else(|| Error::Validation("id is required for get".into()))?;
                let uuid = self
                    .coordination_id(id, "id", workspace_id, project_id)
                    .await?;
                match self.client.get_coordination_item(uuid).await {
                    Ok(result) => Ok(ToolResult::with_structured(
                        format_item(&result, id),
                        result,
                    )),
                    Err(_) => {
                        let result = self
                            .client
                            .get_coordination_notice(uuid, workspace_id)
                            .await?;
                        Ok(ToolResult::with_structured(
                            format_notice(&result, id),
                            result,
                        ))
                    }
                }
            }
            "share" => {
                let title = input
                    .title
                    .ok_or_else(|| Error::Validation("title is required for share".into()))?;
                let kind = validate_kind(input.kind.as_deref())?;
                let workspace_id = workspace_id.ok_or_else(|| {
                    Error::Validation(
                        "workspace_id is required for action=\"share\" (pass workspace_id explicitly or call init(...) first)"
                            .into(),
                    )
                })?;
                // The session's project is used when the call names none; with
                // none to use, say why and what to pass. "Call init first"
                // misled agents whose init was in another session: hosted
                // connections keep scope per session, and every share they
                // sent afterwards failed (coordination/share missing
                // project_id, 2026-11-02).
                let project_id = project_id.ok_or_else(|| {
                    Error::Validation(if project_not_borrowed {
                        "project_id is required for action=\"share\": the workspace_id you passed is not this session's, so this session's project isn't used. Pass the project_id the share is about.".to_string()
                    } else {
                        "project_id is required for action=\"share\": this session has no project (scope lasts only within the session that called init). Pass the project_id the share is about, or call init with folder_path in this session.".to_string()
                    })
                })?;
                let mut body = serde_json::json!({
                    "title": title,
                    "summary": input.summary,
                    "why_it_matters": input.why_it_matters,
                    "kind": kind,
                    "workspace_id": workspace_id,
                    "project_id": project_id,
                    "created_by": "agent",
                });
                let targets = Self::parse_uuid_list(&input.target_workspace_ids)?;
                if !targets.is_empty() {
                    body["target_workspace_ids"] = serde_json::json!(targets);
                }
                let target_projects = Self::parse_uuid_list(&input.target_project_ids)?;
                if !target_projects.is_empty() {
                    body["target_project_ids"] = serde_json::json!(target_projects);
                }
                if let Some(to) = input
                    .to
                    .as_deref()
                    .map(str::trim)
                    .filter(|to| !to.is_empty())
                {
                    body["to"] = serde_json::json!(to);
                }
                // The share names its sending session: that session never receives
                // it back, and a reply to its notices reaches it. On the stateless
                // hosted gateway there is no session state, so the caller's
                // session_id stands in; without either, no reply can reach the
                // sender (0 of 708 agents' shares named one, 2026-11-01).
                let state_session = match self.session.as_deref() {
                    Some(session) => session.state().await.api_session_id.clone(),
                    None => None,
                };
                let sender = state_session.or_else(|| {
                    input
                        .session_id
                        .as_deref()
                        .map(str::trim)
                        .filter(|id| !id.is_empty())
                        .map(str::to_string)
                });
                if let Some(session_id) = sender {
                    body["session_id"] = serde_json::json!(session_id);
                }
                let result = self.client.coordination_share(body).await?;
                Ok(ToolResult::with_structured(
                    format_share(&result, &title),
                    result,
                ))
            }
            "ack" => {
                let notice_id = input
                    .notice_id
                    .or(input.id)
                    .ok_or_else(|| Error::Validation("notice_id is required for ack".into()))?;
                let uuid = self
                    .coordination_id(&notice_id, "notice_id", workspace_id, project_id)
                    .await?;
                let result = self
                    .client
                    .ack_coordination_notice(uuid, workspace_id)
                    .await
                    .map_err(|e| not_this_sessions_notice(&notice_id, e))?;
                Ok(ToolResult::with_structured(
                    format!("Acked coordination notice {notice_id}"),
                    result,
                ))
            }
            "reply" => {
                let notice_id = input
                    .notice_id
                    .or(input.id)
                    .ok_or_else(|| Error::Validation("notice_id is required for reply".into()))?;
                let uuid = self
                    .coordination_id(&notice_id, "notice_id", workspace_id, project_id)
                    .await?;
                let message = input
                    .message
                    .as_deref()
                    .map(str::trim)
                    .filter(|m| !m.is_empty())
                    .ok_or_else(|| Error::Validation("message is required for reply".into()))?;
                let length = message.chars().count();
                if length > REPLY_MAX_CHARS {
                    return Err(Error::refused(
                        mcp_types::ValidationReason::InvalidValue,
                        format!(
                            "`message` is {length} characters; a reply takes at most {REPLY_MAX_CHARS}, and the notice line it becomes shows the first 600. Shorten it, or put the details in a share: coordination(action=\"share\", to=..., title=..., summary=...)."
                        ),
                    ));
                }
                let result = self
                    .client
                    .reply_coordination_notice(uuid, message, workspace_id)
                    .await
                    .map_err(|e| match e {
                        // The share behind the notice named no sending session.
                        Error::Http { status: 400, ref message, .. }
                            if message.contains("no origin session") =>
                        {
                            Error::refused(
                                mcp_types::ValidationReason::InvalidValue,
                                format!(
                                    "`notice_id` {notice_id} can't take a reply: the share it came from didn't name its sender's session. Ack it with coordination(action=\"ack\", notice_id=\"{notice_id}\"), and answer the sender with coordination(action=\"share\", to=\"<their agent name>\", title=..., summary=...). Shares that pass session_id can be replied to."
                                ),
                            )
                        }
                        other => not_this_sessions_notice(&notice_id, other),
                    })?;
                Ok(ToolResult::with_structured(
                    format_reply(&result, &notice_id),
                    result,
                ))
            }
            "dismiss" => {
                let notice_id = input
                    .notice_id
                    .or(input.id)
                    .ok_or_else(|| Error::Validation("notice_id is required for dismiss".into()))?;
                let uuid = self
                    .coordination_id(&notice_id, "notice_id", workspace_id, project_id)
                    .await?;
                let result = self
                    .client
                    .dismiss_coordination_notice(uuid, workspace_id)
                    .await
                    .map_err(|e| not_this_sessions_notice(&notice_id, e))?;
                Ok(ToolResult::with_structured(
                    format!("Dismissed coordination notice {notice_id}"),
                    result,
                ))
            }
            "settings" => {
                let result = if let Some(enabled) = input.enabled {
                    self.client
                        .update_coordination_settings(workspace_id, project_id, enabled)
                        .await?
                } else {
                    self.client
                        .get_coordination_settings(workspace_id, project_id)
                        .await?
                };
                Ok(ToolResult::with_structured(
                    format_settings(&result),
                    result,
                ))
            }
            _ => Err(Error::Validation(format!(
                "Unknown action: {}. Available: {}",
                action,
                VALID_ACTIONS.join(", ")
            ))),
        }
    }

    fn metadata(&self) -> &ToolMetadata {
        static METADATA: std::sync::OnceLock<ToolMetadata> = std::sync::OnceLock::new();
        METADATA.get_or_init(|| ToolMetadata {
            name: "coordination".to_string(),
            title: "Cross-workspace Agent Coordination".to_string(),
            description: "Ongoing shared awareness between live agents, plus durable \
                coordination items when knowledge from one workspace/project is needed \
                in another. Distinct from handoffs (ownership transfer) and capsules \
                (portable bundles). Actions: check_in, inbox/list, get, share, ack, \
                reply, dismiss, settings. context() and init() already heartbeat presence. \
                When [COORDINATION] appears, read it before continuing and ack after use. \
                A (blocking) notice is a direct conflict with another session: read it \
                before continuing, then ack or reply (action=reply, notice_id, message) so \
                the other session sees your answer on its next turn."
                .to_string(),
            category: ToolCategory::Memory,
            annotations: ToolAnnotations::destructive(),
            is_pro: false,
            required_tier: None,
        })
    }

    fn input_schema(&self) -> Value {
        SchemaBuilder::new()
            .description("Coordinate live agents across workspaces/projects. Not a handoff.")
            .string_enum("action", "Action to perform", VALID_ACTIONS, true)
            .uuid(
                "workspace_id",
                "Workspace ID. Defaults to active scope.",
                false,
            )
            .uuid(
                "project_id",
                "Project ID. Defaults to this session's project (set by init in this session). action=\"share\" needs one: pass it when this session has none.",
                false,
            )
            .string(
                "session_id",
                "Your live session id (the one you pass to context/init): for check_in / inbox, and on share so replies to it reach you.",
                false,
            )
            .string("task_summary", "What this agent is working on.", false)
            .object(
                "metadata",
                "Presence metadata for check_in shown to the judge as evidence, e.g. {\"git\": {\"branch\": \"...\", \"commit\": \"...\"}}.",
                false,
            )
            .string(
                "id",
                "Coordination item id for get: the whole id, or its first 8+ characters as inbox shows it.",
                false,
            )
            .string(
                "notice_id",
                "Notice id for ack / reply / dismiss: the whole id, or its first 8+ characters.",
                false,
            )
            .integer("limit", "Most items and notices inbox / list returns.", false)
            .string(
                "message",
                "Reply text for action=reply; delivered to the session that raised the notice.",
                false,
            )
            .string("title", "Title when sharing an item.", false)
            .string(
                "to",
                "For share: one agent of yours (e.g. \"cc-qa\") or a session id. Only that agent receives the share, mid-run included. An agent's name is its CONTEXTSTREAM_AGENT_NAME or its agent-<name> folder; the reply says how many of its live sessions got the share.",
                false,
            )
            .string("summary", "Short summary of the shared knowledge.", false)
            .string(
                "why_it_matters",
                "Why another workspace/project needs this.",
                false,
            )
            .string_enum(
                "kind",
                "Kind of shared item: decision, constraint, api_contract, risk, status, or knowledge (defaults to knowledge; mirrors the API enum).",
                VALID_KINDS,
                false,
            )
            .array(
                "target_workspace_ids",
                "Workspaces this knowledge should reach.",
                "string",
                false,
            )
            .array(
                "target_project_ids",
                "Projects this knowledge should reach.",
                "string",
                false,
            )
            .boolean(
                "enabled",
                "When set with action=settings, updates enablement.",
                false,
            )
            .build()
    }
}

pub fn inbox_payload(inbox: &Value) -> &Value {
    inbox.get("data").unwrap_or(inbox)
}

pub fn inbox_notices(inbox: &Value) -> &[Value] {
    inbox_payload(inbox)
        .get("notices")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn coordination_id(value: &Value) -> Option<&str> {
    let payload = value.get("data").unwrap_or(value);
    payload.get("id").and_then(Value::as_str)
}

/// Validate a `kind` for `share`; `None` defaults to [`DEFAULT_KIND`].
pub fn validate_kind(kind: Option<&str>) -> Result<String> {
    let kind = kind.map(str::trim).filter(|value| !value.is_empty());
    match kind {
        None => Ok(DEFAULT_KIND.to_string()),
        Some(value) => {
            let normalized = value.to_ascii_lowercase();
            if VALID_KINDS.contains(&normalized.as_str()) {
                Ok(normalized)
            } else {
                Err(Error::Validation(format!(
                    "Invalid coordination kind '{value}'. Use one of: {}",
                    VALID_KINDS.join(", ")
                )))
            }
        }
    }
}

fn notice_project_id(notice: &Value) -> Option<Uuid> {
    ["from_project_id", "project_id", "source_project_id"]
        .iter()
        .find_map(|field| notice.get(*field).and_then(Value::as_str))
        .and_then(|raw| Uuid::parse_str(raw).ok())
}

/// Render `[COORDINATION]` lines for an inbox payload.
///
/// * At most `limit` notices are rendered; the rest collapse into a
///   `… N more` trailer (using `total_pending` when the server reports it).
/// * Notices from another project are prefixed `[other project]`.
/// * The lines only *describe* the manual ack call. Nothing here (or in any
///   caller) acks a notice automatically.
pub fn format_coordination_notices(
    inbox: &Value,
    current_project_id: Option<Uuid>,
    limit: usize,
) -> String {
    let payload = inbox_payload(inbox);
    let notices = inbox_notices(inbox);
    if notices.is_empty() {
        return String::new();
    }
    let limit = limit.max(1);
    let mut text = String::new();
    for notice in notices.iter().take(limit) {
        let reason = notice
            .get("reason")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .or_else(|| notice.get("title").and_then(Value::as_str))
            .unwrap_or("shared context");
        let id = notice.get("id").and_then(Value::as_str).unwrap_or_default();
        let other_project = match (notice_project_id(notice), current_project_id) {
            (Some(from), Some(current)) => from != current,
            (Some(_), None) => false,
            (None, _) => false,
        };
        text.push_str(&crate::notices::coordination_notice_line(
            reason,
            id,
            other_project,
            notice.get("urgency").and_then(Value::as_str),
        ));
        text.push('\n');
    }
    let shown = notices.len().min(limit);
    let total_pending = payload
        .get("total_pending")
        .and_then(Value::as_u64)
        .map(|value| value as usize);
    let truncated = payload
        .get("truncated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let remaining = match total_pending {
        Some(total) if total > shown => total - shown,
        _ if notices.len() > shown => notices.len() - shown,
        _ if truncated => 1,
        _ => 0,
    };
    if remaining > 0 {
        text.push_str(&crate::notices::coordination_more_line(remaining));
        text.push('\n');
    }
    text
}

fn format_inbox(result: &Value) -> String {
    let data = inbox_payload(result);
    let notices = inbox_notices(result);
    let items = data
        .get("items")
        .and_then(Value::as_array)
        .map(|a| a.len())
        .unwrap_or(0);
    let mut text = format!(
        "Coordination inbox: {} notice(s), {items} item(s).",
        notices.len()
    );
    let rendered = format_coordination_notices(result, None, NOTICE_RENDER_LIMIT);
    if !rendered.is_empty() {
        text.push('\n');
        text.push_str(rendered.trim_end());
    }
    // The items themselves: over stdio the count was all an agent saw (#377).
    let item_lines = coordination_item_lines(data);
    if !item_lines.is_empty() {
        text.push_str("\nItems:\n");
        text.push_str(&item_lines);
    }
    text
}

/// One line per coordination item (the first 20): id, kind, title, summary
/// and who shared it (#377).
fn coordination_item_lines(data: &Value) -> String {
    const SHOWN: usize = 20;
    let Some(items) = data.get("items").and_then(Value::as_array) else {
        return String::new();
    };
    let field = |item: &Value, key: &str| {
        item.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let mut lines: Vec<String> = items
        .iter()
        .take(SHOWN)
        .map(|item| {
            let mut line = format!(
                "- {} [{}] {}",
                field(item, "id").unwrap_or_else(|| "?".to_string()),
                field(item, "kind").unwrap_or_else(|| "item".to_string()),
                one_line(&field(item, "title").unwrap_or_default(), 120)
            );
            if let Some(summary) = field(item, "summary") {
                line.push_str(&format!(" — {}", one_line(&summary, 200)));
            }
            if let Some(from) = field(item, "created_by") {
                line.push_str(&format!(" (from {from})"));
            }
            line
        })
        .collect();
    if items.len() > SHOWN {
        lines.push(format!("… and {} more.", items.len() - SHOWN));
    }
    lines.join("\n")
}

fn format_share(result: &Value, title: &str) -> String {
    let shared = match coordination_id(result) {
        Some(id) => format!("Shared coordination item {id} ({title})."),
        None => format!("Shared coordination item ({title})."),
    };
    // A share that names an agent says whether that agent got it (#310).
    let payload = result.get("data").unwrap_or(result);
    let delivery = payload.get("delivery").and_then(|delivery| {
        Some((
            delivery.get("to")?.as_str()?,
            delivery.get("live_sessions").and_then(Value::as_u64),
        ))
    });
    let addressed = match delivery {
        None => String::new(),
        Some((to, Some(0))) => format!(
            " No live agent named \"{to}\" right now; it reaches that agent if it checks in within 24 hours. An agent's name is its CONTEXTSTREAM_AGENT_NAME or its agent-<name> folder."
        ),
        Some((to, Some(live))) => format!(
            " Delivered to {live} live session(s) of {to}: shown at their next tool call (with hooks) or their next context() or inbox."
        ),
        Some((to, None)) => format!(" Addressed to {to}; delivering in the background."),
    };
    format!("{shared}{addressed} This is not a handoff.")
}

fn format_item(result: &Value, id: &str) -> String {
    let payload = result.get("data").unwrap_or(result);
    let title = payload.get("title").and_then(Value::as_str).unwrap_or(id);
    let kind = payload
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("item");
    format!("Coordination item {id} ({kind}): {title}")
}

/// The longest reply the API takes (`ReplyNoticeRequest`).
const REPLY_MAX_CHARS: usize = 2000;

/// The API's 404 for a notice action: the notice is not this session's. A
/// notice belongs to the session it was delivered to; after a session moves,
/// the same item reaches it as a new notice. ack said so; reply and dismiss
/// passed on the bare 404 (2026-11-03).
fn not_this_sessions_notice(notice_id: &str, error: Error) -> Error {
    match error {
        Error::Http { status: 404, .. } => Error::refused(
            mcp_types::ValidationReason::ReferenceNotFound,
            format!(
                "`notice_id` {notice_id} is not one of this session's notices. A notice belongs to the session it was delivered to; after a session moves, the same item comes back as a new notice: coordination(action=\"inbox\") lists this session's notices with their ids."
            ),
        ),
        other => other,
    }
}

fn format_reply(result: &Value, notice_id: &str) -> String {
    let payload = result.get("data").unwrap_or(result);
    let created = payload
        .get("created")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let to_session = payload
        .pointer("/notice/to_session_id")
        .and_then(Value::as_str)
        .unwrap_or("the origin session");
    if created {
        format!(
            "Replied to coordination notice {notice_id}; {to_session} sees it on its next turn. The original notice is still open — ack it when you are done."
        )
    } else {
        format!("An identical reply to coordination notice {notice_id} already exists; nothing new was sent.")
    }
}

fn format_notice(result: &Value, id: &str) -> String {
    let payload = result.get("data").unwrap_or(result);
    let reason = payload
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("shared context");
    format!("[COORDINATION] {reason} — ack via coordination(action=\"ack\", notice_id=\"{id}\")")
}

fn format_settings(result: &Value) -> String {
    let payload = result.get("data").unwrap_or(result);
    let enabled = payload.get("enabled").and_then(Value::as_bool);
    let workspace_enabled = payload.get("workspace_enabled").and_then(Value::as_bool);
    let project_enabled = payload.get("project_enabled").and_then(Value::as_bool);
    match (enabled, workspace_enabled, project_enabled) {
        (Some(enabled), Some(workspace_enabled), Some(project_enabled)) => format!(
            "Coordination settings: enabled={enabled} workspace_enabled={workspace_enabled} project_enabled={project_enabled}"
        ),
        (Some(enabled), _, _) => format!("Coordination settings: enabled={enabled}"),
        _ => "Coordination settings".to_string(),
    }
}

pub fn register_coordination_tools(
    registry: &mut crate::registry::ToolRegistry,
    client: ContextStreamClient,
    session: Arc<mcp_session::SessionManager>,
) {
    registry.register(
        "coordination",
        Arc::new(CoordinationTool::new(client).with_session(session)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn inbox_notices_read_unwrapped_client_payload() {
        let inbox = json!({
            "notices": [{"id": "n1", "reason": "API contract changed"}],
            "items": [{"id": "i1"}]
        });
        let notices = inbox_notices(&inbox);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0]["id"], "n1");
        let text = format_inbox(&inbox);
        assert!(text.contains("[COORDINATION] API contract changed"));
        assert!(text.contains("notice_id=\"n1\""));
    }

    #[test]
    fn inbox_notices_read_wrapped_api_payload() {
        let inbox = json!({
            "data": {
                "notices": [{"id": "n2", "reason": "Shared decision"}],
                "items": []
            }
        });
        assert_eq!(inbox_notices(&inbox).len(), 1);
        assert!(format_inbox(&inbox).contains("notice_id=\"n2\""));
    }

    #[test]
    fn other_project_notices_are_prefixed_and_never_auto_acked() {
        let current = Uuid::new_v4();
        let other = Uuid::new_v4();
        let inbox = json!({
            "notices": [
                {"id": "n1", "reason": "Shared decision", "from_project_id": current.to_string()},
                {"id": "n2", "reason": "Schema freeze", "from_project_id": other.to_string(), "urgency": "high"},
                {"id": "n3", "reason": "No project on notice"}
            ]
        });
        let text = format_coordination_notices(&inbox, Some(current), 5);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(
            lines[0],
            "[COORDINATION] Shared decision — ack via coordination(action=\"ack\", notice_id=\"n1\")"
        );
        assert!(lines[1].starts_with("[COORDINATION] [other project] Schema freeze (urgency=high)"));
        assert!(!lines[2].contains("[other project]"));
        // Only the manual ack call is described; no line claims an ack happened.
        assert!(!text.to_ascii_lowercase().contains("acked"));
    }

    #[test]
    fn truncation_trailer_counts_remaining_notices() {
        let notices: Vec<Value> = (0..7)
            .map(|index| json!({"id": format!("n{index}"), "reason": format!("reason {index}")}))
            .collect();
        let inbox = json!({"notices": notices, "truncated": true, "total_pending": 12});
        let text = format_coordination_notices(&inbox, None, 5);
        assert_eq!(text.matches("[COORDINATION]").count(), 6);
        assert!(text.contains("[COORDINATION] … 7 more"));

        let inbox = json!({"notices": [{"id": "a", "reason": "r"}], "truncated": true});
        let text = format_coordination_notices(&inbox, None, 5);
        assert!(text.contains("… 1 more"));

        let inbox = json!({"notices": [{"id": "a", "reason": "r"}]});
        assert!(!format_coordination_notices(&inbox, None, 5).contains("more"));
        assert!(format_coordination_notices(&json!({"notices": []}), None, 5).is_empty());
    }

    #[test]
    fn share_kind_is_validated_client_side() {
        assert_eq!(validate_kind(None).unwrap(), "knowledge");
        assert_eq!(validate_kind(Some("Decision")).unwrap(), "decision");
        assert_eq!(validate_kind(Some(" STATUS ")).unwrap(), "status");
        for kind in VALID_KINDS {
            assert_eq!(validate_kind(Some(kind)).unwrap(), *kind);
        }
        // The API enum (handlers/coordination.rs coordination_kind_is_known).
        assert_eq!(
            VALID_KINDS,
            &[
                "decision",
                "constraint",
                "api_contract",
                "risk",
                "status",
                "knowledge"
            ]
        );
        for legacy in [
            "note", "warning", "insight", "blocker", "request", "handoff",
        ] {
            let err = validate_kind(Some(legacy)).unwrap_err().to_string();
            assert!(err.contains("Invalid coordination kind"), "{legacy}");
            assert!(err.contains("knowledge"), "{legacy}");
        }
    }

    #[test]
    fn reply_text_names_the_origin_session_and_dedup_outcome() {
        let created = json!({"data": {"created": true, "notice": {"to_session_id": "origin-1"}, "reply_to": "n1"}});
        let text = format_reply(&created, "n1");
        assert!(text.contains("Replied to coordination notice n1"));
        assert!(text.contains("origin-1 sees it on its next turn"));
        let duplicate = json!({"created": false, "notice": {"to_session_id": "origin-1"}});
        assert!(format_reply(&duplicate, "n1").contains("already exists"));
        assert!(VALID_ACTIONS.contains(&"reply"));
    }

    #[test]
    fn share_and_settings_text_include_ids_and_flags() {
        let share = json!({"id": "item-1", "title": "Ranking"});
        assert!(format_share(&share, "Ranking").contains("item-1"));
        // #310: a share that names an agent says whether it reached anyone.
        let none_live = serde_json::json!({"data": {"id": "item-2", "delivery": {"to": "no-such-agent", "live_sessions": 0}}});
        assert!(format_share(&none_live, "probe").contains("No live agent named \"no-such-agent\""));
        let live = serde_json::json!({"data": {"id": "item-3", "delivery": {"to": "qa-b", "live_sessions": 1}}});
        assert!(format_share(&live, "probe").contains("Delivered to 1 live session(s) of qa-b"));
        let settings = json!({
            "enabled": true,
            "workspace_enabled": true,
            "project_enabled": false
        });
        assert_eq!(
            format_settings(&settings),
            "Coordination settings: enabled=true workspace_enabled=true project_enabled=false"
        );
    }

    #[tokio::test]
    async fn share_validates_missing_scope_without_leaking_backend_origin_field_names() {
        let config = mcp_types::Config {
            api_url: "http://127.0.0.1:9".to_string(),
            ..Default::default()
        };
        let client = ContextStreamClient::new(config.clone());
        let session = Arc::new(mcp_session::SessionManager::new(client.clone(), config));
        let tool = CoordinationTool::new(client).with_session(session.clone());
        let ws = Uuid::new_v4();

        // Missing workspace_id and project_id: fails fast naming workspace_id.
        let err = tool
            .execute(json!({
                "action": "share",
                "kind": "status",
                "title": "qa test",
                "summary": "test"
            }))
            .await
            .expect_err("missing workspace_id should fail client-side");
        let msg = err.to_string();
        assert!(
            msg.contains("workspace_id is required for action=\"share\""),
            "{msg}"
        );
        assert!(!msg.contains("origin_workspace_id"), "{msg}");

        // Explicit workspace_id without project_id and no session scope: fails fast naming project_id.
        let err = tool
            .execute(json!({
                "action": "share",
                "workspace_id": ws.to_string(),
                "kind": "status",
                "title": "qa test",
                "summary": "test"
            }))
            .await
            .expect_err("missing project_id should fail client-side");
        let msg = err.to_string();
        assert!(
            msg.contains("project_id is required for action=\"share\""),
            "{msg}"
        );
        assert!(!msg.contains("origin_project_id"), "{msg}");
        // It says why: this session has no project. "Call init first" misled
        // agents whose init was in another session (2026-11-02).
        assert!(msg.contains("this session has no project"), "{msg}");
        assert!(!msg.contains("call init(...) first"), "{msg}");

        // Session scope in Workspace A is not borrowed when caller names Workspace B without project_id.
        let other_ws = Uuid::new_v4();
        let session_proj = Uuid::new_v4();
        session
            .update_scope(Some(ws), Some(session_proj), None)
            .await;
        let err = tool
            .execute(json!({
                "action": "share",
                "workspace_id": other_ws.to_string(),
                "kind": "status",
                "title": "qa test",
                "summary": "test"
            }))
            .await
            .expect_err("cross-workspace share without project_id should fail client-side");
        let msg = err.to_string();
        assert!(
            msg.contains("project_id is required for action=\"share\""),
            "{msg}"
        );
        // And here why the session's project isn't used.
        assert!(msg.contains("is not this session's"), "{msg}");
    }

    #[tokio::test]
    async fn share_inherits_active_session_scope_when_omitted() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let captured = Arc::new(tokio::sync::Mutex::new(None::<Value>));
        let captured_srv = captured.clone();

        let srv = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = stream.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]);
            if let Some((_, body)) = req.split_once("\r\n\r\n") {
                if let Ok(parsed) = serde_json::from_str::<Value>(body) {
                    *captured_srv.lock().await = Some(parsed);
                }
            }
            let resp_body =
                r#"{"data":{"id":"11111111-1111-4111-8111-111111111111","title":"qa test"}}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                resp_body.len(),
                resp_body
            );
            stream.write_all(resp.as_bytes()).await.unwrap();
        });

        let config = mcp_types::Config {
            api_url: format!("http://{addr}/api/v1"),
            api_key: Some("test-key".to_string()),
            ..Default::default()
        };
        let client = ContextStreamClient::new(config.clone());
        let session = Arc::new(mcp_session::SessionManager::new(client.clone(), config));
        let ws = Uuid::new_v4();
        let proj = Uuid::new_v4();
        session.update_scope(Some(ws), Some(proj), None).await;

        let tool = CoordinationTool::new(client).with_session(session);
        let res = tool
            .execute(json!({
                "action": "share",
                "kind": "status",
                "title": "qa test",
                "summary": "test"
            }))
            .await
            .expect("share should inherit session scope");
        assert!(!res.is_error);
        srv.await.unwrap();

        let body = captured.lock().await.clone().expect("captured body");
        assert_eq!(body["workspace_id"], ws.to_string());
        assert_eq!(body["project_id"], proj.to_string());
        assert!(body.get("to").is_none());
    }

    #[tokio::test]
    async fn share_to_names_the_recipient_and_the_sending_session() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let captured = Arc::new(tokio::sync::Mutex::new(None::<Value>));
        let captured_srv = captured.clone();
        let srv = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 8192];
            let n = stream.read(&mut buf).await.unwrap();
            let req = String::from_utf8_lossy(&buf[..n]);
            if let Some((_, body)) = req.split_once("\r\n\r\n") {
                if let Ok(parsed) = serde_json::from_str::<Value>(body) {
                    *captured_srv.lock().await = Some(parsed);
                }
            }
            let resp_body = r#"{"data":{"id":"11111111-1111-4111-8111-111111111111"}}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                resp_body.len(),
                resp_body
            );
            stream.write_all(resp.as_bytes()).await.unwrap();
        });

        let config = mcp_types::Config {
            api_url: format!("http://{addr}/api/v1"),
            api_key: Some("test-key".to_string()),
            ..Default::default()
        };
        let client = ContextStreamClient::new(config.clone());
        let session = Arc::new(mcp_session::SessionManager::new(client.clone(), config));
        session
            .update_scope(Some(Uuid::new_v4()), Some(Uuid::new_v4()), None)
            .await;
        session
            .set_api_session_id(Some("sender-session".into()))
            .await;

        let tool = CoordinationTool::new(client).with_session(session);
        let res = tool
            .execute(json!({
                "action": "share",
                "kind": "status",
                "title": "Kimi key funded",
                "to": " cc-qa ",
            }))
            .await
            .expect("share with to");
        assert!(!res.is_error);
        srv.await.unwrap();

        let body = captured.lock().await.clone().expect("captured body");
        assert_eq!(body["to"], "cc-qa");
        assert_eq!(body["session_id"], "sender-session");
    }
}

/// `text` on one line, cut to `max` characters.
fn one_line(text: &str, max: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= max {
        return flat;
    }
    let cut: String = flat.chars().take(max.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}

#[cfg(test)]
mod coordination_item_line_tests {
    use super::*;

    #[test]
    fn inbox_items_are_listed_not_only_counted() {
        let result = serde_json::json!({"items": [
            {"id": "247b1040", "kind": "decision", "title": "To mcp-eng: #365 is yours",
             "summary": "The first tool call re-stamps associated_at.", "created_by": "coordinator"},
        ], "notices": []});
        let text = format_inbox(&result);
        assert!(
            text.contains("Coordination inbox: 0 notice(s), 1 item(s)."),
            "{text}"
        );
        assert!(
            text.contains("- 247b1040 [decision] To mcp-eng: #365 is yours — The first tool call re-stamps associated_at. (from coordinator)"),
            "{text}"
        );
    }
}

#[cfg(test)]
mod short_id_tests {
    use super::*;
    use crate::testing::TestFixtures;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const ITEM: &str = "550e8400-e29b-41d4-a716-446655440000";
    const NOTICE: &str = "660e8400-e29b-41d4-a716-446655440000";

    /// Answers the inbox with one item and one notice, and an item read with
    /// the item; returns the request lines it saw.
    fn serve(listener: tokio::net::TcpListener) -> tokio::task::JoinHandle<Vec<String>> {
        tokio::spawn(async move {
            let mut lines = Vec::new();
            while let Ok(Ok((mut socket, _))) =
                tokio::time::timeout(std::time::Duration::from_secs(2), listener.accept()).await
            {
                let mut buffer = vec![0u8; 16 * 1024];
                let read = socket.read(&mut buffer).await.expect("read request");
                let request = String::from_utf8_lossy(&buffer[..read]);
                let line = request.lines().next().unwrap_or_default().to_string();
                let body = if line.contains("/coordination/inbox") {
                    serde_json::json!({"items": [{"id": ITEM, "title": "Coordinator: go"}],
                                       "notices": [{"id": NOTICE, "item_id": ITEM}]})
                } else if line.contains(&format!("/coordinations/{ITEM}")) {
                    serde_json::json!({"id": ITEM, "title": "Coordinator: go", "summary": "Go."})
                } else {
                    serde_json::json!({})
                }
                .to_string();
                lines.push(line);
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response");
            }
            lines
        })
    }

    async fn tool() -> (CoordinationTool, tokio::net::TcpListener) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let mut config = TestFixtures::test_config();
        config.api_url = format!("http://{}", listener.local_addr().expect("addr"));
        config.api_key = Some("test-key".to_string());
        (
            CoordinationTool::new(ContextStreamClient::new(config)),
            listener,
        )
    }

    #[tokio::test]
    async fn an_item_is_read_by_the_start_of_its_id() {
        // Reports quote items by 8 characters ("item=550e8400"); get answered
        // "Invalid coordination id" (10 times in a day, 2026-11-01).
        let (tool, listener) = tool().await;
        let seen = serve(listener);
        let result = tool
            .execute(serde_json::json!({"action": "get", "id": "550e8400",
                                        "workspace_id": TestFixtures::workspace_id()}))
            .await
            .expect("the item is read");
        assert!(!result.is_error, "{result:?}");
        let lines = seen.await.unwrap();
        assert!(
            lines
                .iter()
                .any(|l| l.contains(&format!("/coordinations/{ITEM}"))),
            "{lines:?}"
        );
    }

    #[tokio::test]
    async fn an_id_that_starts_nothing_says_how_to_pass_it() {
        let (tool, listener) = tool().await;
        let _seen = serve(listener);
        let err = tool
            .execute(serde_json::json!({"action": "ack", "notice_id": "abcdef12",
                                        "workspace_id": TestFixtures::workspace_id()}))
            .await
            .expect_err("no notice starts with abcdef12");
        assert!(
            err.to_string().contains("pass the whole 36-character id"),
            "{err}"
        );
        let err = tool
            .execute(
                serde_json::json!({"action": "ack", "notice_id": "not-an-id",
                                        "workspace_id": TestFixtures::workspace_id()}),
            )
            .await
            .expect_err("not an id");
        assert!(
            err.to_string()
                .contains("coordination(action=\"inbox\") lists them"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_share_names_the_callers_session_so_replies_reach_it() {
        // 0 of 708 agents' shares named a sending session (2026-11-01): on the
        // stateless gateway there is no session state, so every reply to them
        // was refused with "no origin session".
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let mut config = TestFixtures::test_config();
        config.api_url = format!("http://{}", listener.local_addr().expect("addr"));
        config.api_key = Some("test-key".to_string());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buffer = vec![0u8; 16 * 1024];
            let read = socket.read(&mut buffer).await.expect("read");
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            let body = r#"{"id": "550e8400-e29b-41d4-a716-446655440000", "title": "t"}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.expect("write");
            request
        });
        let tool = CoordinationTool::new(ContextStreamClient::new(config));
        tool.execute(
            serde_json::json!({"action": "share", "title": "t", "summary": "s",
                                        "workspace_id": TestFixtures::workspace_id(),
                                        "project_id": TestFixtures::project_id(),
                                        "session_id": "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee"}),
        )
        .await
        .expect("shared");
        let request = server.await.unwrap();
        assert!(
            request.contains(r#""session_id":"aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee""#),
            "{request}"
        );
    }

    #[test]
    fn inbox_limit_sent_as_a_string_is_read_as_a_number() {
        // `limit: "6"` failed the schema ("invalid type: string \"6\", expected
        // i64"): limit wasn't declared, so it wasn't coerced like other integers.
        let tool = CoordinationTool::new(ContextStreamClient::new(TestFixtures::test_config()));
        let mut input = serde_json::json!({"action": "inbox", "limit": "6"});
        crate::input_shape::normalize_arguments(&tool.input_schema(), &mut input);
        assert_eq!(input["limit"], serde_json::json!(6));
        let parsed: CoordinationInput = serde_json::from_value(input).expect("parses");
        assert_eq!(parsed.limit, Some(6));
    }
}

#[cfg(test)]
mod notice_refusal_tests {
    //! Agents' coordination calls that the API refused (telemetry,
    //! 2026-11-03): replies and acks to notices that are not this session's
    //! (404), replies to notices whose share named no sending session, and a
    //! reply longer than the API takes (400). Each refusal names the argument
    //! it is about, so an agent knows what to change and the gateway's
    //! telemetry records it.
    use super::*;
    use crate::testing::TestFixtures;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const NOTICE: &str = "660e8400-e29b-41d4-a716-446655440000";

    /// Answers every request with `status` and `body`; returns the request
    /// lines it saw.
    fn answer_all(
        listener: tokio::net::TcpListener,
        status: u16,
        body: &'static str,
    ) -> tokio::task::JoinHandle<Vec<String>> {
        tokio::spawn(async move {
            let mut lines = Vec::new();
            while let Ok(Ok((mut socket, _))) =
                tokio::time::timeout(std::time::Duration::from_secs(2), listener.accept()).await
            {
                let mut buffer = vec![0u8; 16 * 1024];
                let read = socket.read(&mut buffer).await.expect("read request");
                let request = String::from_utf8_lossy(&buffer[..read]);
                lines.push(request.lines().next().unwrap_or_default().to_string());
                let response = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response");
            }
            lines
        })
    }

    async fn tool() -> (CoordinationTool, tokio::net::TcpListener) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let mut config = TestFixtures::test_config();
        config.api_url = format!("http://{}", listener.local_addr().expect("addr"));
        config.api_key = Some("test-key".to_string());
        (
            CoordinationTool::new(ContextStreamClient::new(config)),
            listener,
        )
    }

    fn named(tool: &CoordinationTool, error: &Error) -> Option<String> {
        crate::registry::refused_argument(&tool.input_schema(), &error.to_string())
    }

    #[tokio::test]
    async fn a_notice_that_is_not_this_sessions_is_said_so_for_ack_reply_and_dismiss() {
        for input in [
            serde_json::json!({"action": "ack"}),
            serde_json::json!({"action": "reply", "message": "Thanks."}),
            serde_json::json!({"action": "dismiss"}),
        ] {
            let (tool, listener) = tool().await;
            let _seen = answer_all(
                listener,
                404,
                r#"{"error":{"code":"not_found","message":"Coordination notice not found"}}"#,
            );
            let mut input = input;
            input["notice_id"] = serde_json::json!(NOTICE);
            input["workspace_id"] = serde_json::json!(TestFixtures::workspace_id());
            let error = tool.execute(input.clone()).await.expect_err("refused");
            assert!(
                matches!(
                    error,
                    Error::Refused {
                        reason: mcp_types::ValidationReason::ReferenceNotFound,
                        ..
                    }
                ),
                "{input}: {error}"
            );
            let text = error.to_string();
            assert!(
                text.contains("is not one of this session's notices"),
                "{input}: {text}"
            );
            assert!(
                text.contains("coordination(action=\"inbox\")"),
                "{input}: {text}"
            );
            assert_eq!(
                named(&tool, &error).as_deref(),
                Some("notice_id"),
                "{input}: {text}"
            );
        }
    }

    #[tokio::test]
    async fn a_notice_with_no_sending_session_names_the_notice_not_the_action() {
        let (tool, listener) = tool().await;
        let _seen = answer_all(
            listener,
            400,
            r#"{"error":{"code":"bad_request","message":"This notice has no origin session to reply to; ack or dismiss it instead"}}"#,
        );
        let error = tool
            .execute(
                serde_json::json!({"action": "reply", "notice_id": NOTICE, "message": "Thanks.",
                                        "workspace_id": TestFixtures::workspace_id()}),
            )
            .await
            .expect_err("refused");
        let text = error.to_string();
        assert!(text.contains("can't take a reply"), "{text}");
        // It used to be counted as a bad `action`, the first declared word in it.
        assert_eq!(named(&tool, &error).as_deref(), Some("notice_id"), "{text}");
    }

    #[tokio::test]
    async fn a_reply_longer_than_the_api_takes_is_refused_before_it_is_sent() {
        let (tool, listener) = tool().await;
        let seen = answer_all(listener, 200, "{}");
        let error = tool
            .execute(serde_json::json!({"action": "reply", "notice_id": NOTICE,
                                        "message": "x".repeat(2001),
                                        "workspace_id": TestFixtures::workspace_id()}))
            .await
            .expect_err("too long");
        let text = error.to_string();
        assert!(text.contains("2000"), "{text}");
        assert!(text.contains("2001"), "{text}");
        assert_eq!(named(&tool, &error).as_deref(), Some("message"), "{text}");
        assert_eq!(seen.await.unwrap(), Vec::<String>::new(), "nothing is sent");
    }

    #[test]
    fn the_schema_says_a_share_needs_a_project() {
        // "Defaults to active scope." left agents with no project in this
        // session to find out from the refusal (share missing_argument
        // project_id, 3 times on 2026-11-03).
        let tool = CoordinationTool::new(ContextStreamClient::new(TestFixtures::test_config()));
        let schema = tool.input_schema();
        let description = schema["properties"]["project_id"]["description"]
            .as_str()
            .expect("project_id is described");
        assert!(description.contains("share"), "{description}");
    }
}
