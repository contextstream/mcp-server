//! Agent Q&A MCP tool — the user-facing surface for the Q&A plan.
//!
//! AI agents using the MCP call this tool when they get **stuck** on
//! workspace- or project-specific knowledge they can't derive from
//! code: conventions, prior decisions, why-we-chose-X-over-Y, runbooks
//! for recurring tasks, the team's guardrails. It's a **helper, not a
//! reflex** — the tool description below tells the calling agent
//! exactly when to use it and when to figure things out themselves.
//!
//! ## Actions
//!
//! - `ask` — submit a question, get an answer grounded in retrieved
//!   sources. The only action that calls upstream ContextCode.
//! - `search` — vector-similarity-free SQL listing of prior Q&A in
//!   scope. Useful as "have we asked this before?" before burning a
//!   fresh ask.
//! - `save_kb` — store a knowledge-base entry the agent draws from
//!   (kind: guidance / guardrail / faq / runbook / caveat).
//! - `list_kb` — browse stored KB items.
//! - `feedback` — rate an answer (-1 / 0 / +1).
//!
//! Every action returns a docs-style `✓ ...` headline as the lead
//! line of the text result, matching the v0.2.96 capsule tool family.
//! The `model_name` in any output is the public branding constant
//! ("ContextCode") — the upstream model identifier never leaks. Read
//! actions (`search`, `list_kb`) wrap drift handling via
//! `super::workspace_drift` so 403/401 mid-session never bubbles to
//! the agent as a raw "Forbidden" error.

use async_trait::async_trait;
use mcp_client::{
    ContextStreamClient, QaAskParams, QaAskResult, QaCreateKbItemParams, QaFeedbackParams,
    QaKbItem, QaListKbParams, QaListKbResult, QaSearchHit, QaSearchParams, QaSearchResult,
    QaUpdateKbItemParams,
};
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
    "ask",
    "search",
    "save_kb",
    "list_kb",
    "get_kb",
    "update_kb",
    "delete_kb",
    "feedback",
    "explain",
];

const VALID_KB_KINDS: &[&str] = &["guidance", "guardrail", "faq", "runbook", "caveat"];

/// Tool description steers AI callers to the "stuck-helper" pattern.
/// Reads identical-shape to capsule's `description` so the tools/list
/// surface stays uniform across the domain family.
const TOOL_DESCRIPTION: &str = "ContextStream agent Q&A — ask the workspace/project knowledge base when you get stuck.\n\nWhen to use:\n- You need workspace-specific knowledge you cannot derive from code: prior decisions (\"why was X chosen over Y?\"), conventions (\"what's the file naming pattern in this repo?\"), runbooks (\"how does the team handle this kind of incident?\"), guardrails (\"what's off-limits in this workspace?\").\n- You're about to make a non-trivial choice and the workspace probably has prior context that shapes it.\n- A teammate has likely answered this before and you'd rather reuse than re-derive.\n\nWhen NOT to use:\n- General programming questions you can answer yourself or via web search (\"how does Rust async work?\").\n- Things you can determine by reading the code right in front of you — read it first.\n- Trivial syntax or single-line questions.\n\nNot a reflex, not a last resort. If you're spending more than ~30 seconds stuck on something workspace-shaped, ask. If you can find the answer in 30 seconds yourself, do that.\n\nActions:\n- ask: submit a question, get a grounded answer with citations + confidence.\n- search: vector-similarity-free listing of prior Q&A — check before re-asking.\n- save_kb: store guidance/guardrail/faq/runbook/caveat for future asks to reference.\n- list_kb: browse stored knowledge.\n- get_kb / update_kb / delete_kb: manage individual KB items.\n- feedback: rate an answer (-1, 0, +1) so future retrievals weight it appropriately.\n\nAnswers come from ContextCode, ContextStream's grounded Q&A agent. Every claim cites the source (`[id=decision:abc]` / `[id=lesson:xyz]` / `[id=qa_kb_item:def]` etc.) so you can verify before acting on it.";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct QaInput {
    pub action: String,

    // ask
    pub question: Option<String>,
    pub max_tokens: Option<u32>,
    pub temperature: Option<f32>,
    pub tags: Option<Vec<String>>,
    pub scope_summary: Option<String>,

    // shared scope
    pub workspace_id: Option<String>,
    pub project_id: Option<String>,
    pub session_id: Option<String>,

    // search
    pub query: Option<String>,
    pub asked_by_user_id: Option<String>,
    pub tag: Option<String>,
    pub page: Option<i32>,
    pub per_page: Option<i32>,

    // kb
    pub id: Option<String>,
    pub title: Option<String>,
    pub content: Option<String>,
    pub kind: Option<String>,
    pub metadata: Option<Value>,
    pub created_by: Option<String>,

    // feedback
    pub answer_id: Option<String>,
    pub score: Option<i16>,
}

pub struct QaTool {
    client: ContextStreamClient,
}

impl QaTool {
    pub fn new(client: ContextStreamClient) -> Self {
        Self { client }
    }

    fn parse_uuid_opt(value: &Option<String>, field: &str) -> Result<Option<Uuid>> {
        match value.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(raw) => Uuid::parse_str(raw)
                .map(Some)
                .map_err(|_| Error::Validation(format!("Invalid {}", field))),
            None => Ok(None),
        }
    }

    fn require_uuid(value: &Option<String>, field: &str) -> Result<Uuid> {
        Self::parse_uuid_opt(value, field)?
            .ok_or_else(|| Error::Validation(format!("{} is required", field)))
    }
}

#[async_trait]
impl ToolHandler for QaTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let input: QaInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;
        let action = input.action.to_lowercase();

        match action.as_str() {
            "ask" => self.handle_ask(input).await,
            "search" => self.handle_search(input).await,
            "save_kb" => self.handle_save_kb(input).await,
            "list_kb" => self.handle_list_kb(input).await,
            "get_kb" => self.handle_get_kb(input).await,
            "update_kb" => self.handle_update_kb(input).await,
            "delete_kb" => self.handle_delete_kb(input).await,
            "feedback" => self.handle_feedback(input).await,
            "explain" => Ok(ToolResult::text(
                "Agent Q&A — ask ContextCode questions about your workspace/project when stuck. \
                 Use action=ask for a grounded answer, action=search to check for prior Q&A, \
                 action=save_kb to store guidance/guardrails/runbooks the agent should consult \
                 in future answers."
                    .to_string(),
            )),
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
            name: "qa".to_string(),
            title: "Agent Q&A".to_string(),
            description: TOOL_DESCRIPTION.to_string(),
            category: ToolCategory::Session,
            annotations: ToolAnnotations::destructive(),
            is_pro: false,
            required_tier: None,
        })
    }

    fn input_schema(&self) -> Value {
        SchemaBuilder::new()
            .description("Agent Q&A — ask the workspace knowledge base when stuck.")
            .string_enum("action", "Action to perform", VALID_ACTIONS, true)
            .string("question", "Natural-language question (action=ask)", false)
            .uuid("workspace_id", "Workspace ID", false)
            .uuid("project_id", "Project ID", false)
            .string(
                "session_id",
                "Optional MCP session id — links the question to the AI session that asked.",
                false,
            )
            .integer("max_tokens", "Override max answer tokens (action=ask)", false)
            .property(
                "temperature",
                serde_json::json!({"type": "number", "description": "Override sampling temperature (action=ask, default 0.2)"}),
                false,
            )
            .array(
                "tags",
                "Optional tags to attach to the persisted question (action=ask)",
                "string",
                false,
            )
            .string(
                "scope_summary",
                "Optional human-readable scope label fed into the prompt (e.g. 'workspace=Engineering, project=mcp')",
                false,
            )
            .string(
                "query",
                "Filter prior Q&A by free-text against question_text (action=search) or KB title/content (action=list_kb)",
                false,
            )
            .uuid(
                "asked_by_user_id",
                "Filter prior Q&A by who asked (action=search)",
                false,
            )
            .string(
                "tag",
                "Filter prior Q&A or KB items by tag (action=search, list_kb)",
                false,
            )
            .integer("page", "Page number (1-based)", false)
            .integer("per_page", "Page size", false)
            .uuid(
                "id",
                "KB item id (action=get_kb / update_kb / delete_kb)",
                false,
            )
            .string("title", "KB item title (action=save_kb / update_kb)", false)
            .string("content", "KB item body (action=save_kb / update_kb)", false)
            .string_enum(
                "kind",
                "KB item kind (action=save_kb / update_kb)",
                VALID_KB_KINDS,
                false,
            )
            .property(
                "metadata",
                serde_json::json!({"type": "object", "description": "Optional metadata (action=save_kb / update_kb)", "additionalProperties": true}),
                false,
            )
            .uuid(
                "created_by",
                "Filter KB items by creator user id (action=list_kb)",
                false,
            )
            .uuid(
                "answer_id",
                "Answer id to rate (action=feedback)",
                false,
            )
            .integer(
                "score",
                "Feedback score: -1 (negative), 0 (clear), +1 (positive). action=feedback.",
                false,
            )
            .build()
    }
}

impl QaTool {
    async fn handle_ask(&self, input: QaInput) -> Result<ToolResult> {
        let question = input
            .question
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Validation("question is required for action=ask".to_string()))?
            .to_string();
        let workspace_id = Self::parse_uuid_opt(&input.workspace_id, "workspace_id")?;
        let project_id = Self::parse_uuid_opt(&input.project_id, "project_id")?;

        let params = QaAskParams {
            question,
            workspace_id,
            project_id,
            session_id: input.session_id,
            max_tokens: input.max_tokens,
            temperature: input.temperature,
            tags: input.tags,
            scope_summary: input.scope_summary,
        };

        let response = self.client.qa_ask(params).await?;
        let text = format_ask_response(&response);
        Ok(ToolResult::with_structured(
            text,
            serde_json::to_value(&response).unwrap_or_default(),
        ))
    }

    async fn handle_search(&self, input: QaInput) -> Result<ToolResult> {
        let workspace_id = Self::parse_uuid_opt(&input.workspace_id, "workspace_id")?;
        let project_id = Self::parse_uuid_opt(&input.project_id, "project_id")?;
        let asked_by = Self::parse_uuid_opt(&input.asked_by_user_id, "asked_by_user_id")?;

        let params = QaSearchParams {
            q: input.query,
            workspace_id,
            project_id,
            session_id: input.session_id,
            asked_by_user_id: asked_by,
            tag: input.tag,
            page: input.page,
            per_page: input.per_page,
        };

        match self.client.qa_search(params).await {
            Ok(response) => {
                let text = format_search_response(&response);
                Ok(ToolResult::with_structured(
                    text,
                    serde_json::to_value(&response).unwrap_or_default(),
                ))
            }
            Err(err) if super::workspace_drift::is_workspace_access_error(&err) => Ok(
                super::workspace_drift::drift_collection_result("Q&A items", workspace_id, None),
            ),
            Err(err) => Err(err),
        }
    }

    async fn handle_save_kb(&self, input: QaInput) -> Result<ToolResult> {
        let title = input
            .title
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Validation("title is required for action=save_kb".to_string()))?
            .to_string();
        let content = input
            .content
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Validation("content is required for action=save_kb".to_string()))?
            .to_string();
        let kind = input
            .kind
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Validation("kind is required for action=save_kb".to_string()))?
            .to_lowercase();
        if !VALID_KB_KINDS.contains(&kind.as_str()) {
            return Err(Error::Validation(format!(
                "Invalid kind '{}'. Valid: {}.",
                kind,
                VALID_KB_KINDS.join(", ")
            )));
        }

        let workspace_id = Self::parse_uuid_opt(&input.workspace_id, "workspace_id")?;
        let project_id = Self::parse_uuid_opt(&input.project_id, "project_id")?;

        let params = QaCreateKbItemParams {
            title,
            content,
            kind,
            workspace_id,
            project_id,
            tags: input.tags,
            metadata: input.metadata,
        };

        let item = self.client.qa_create_kb_item(params).await?;
        let text = format_kb_create(&item);
        Ok(ToolResult::with_structured(
            text,
            serde_json::to_value(&item).unwrap_or_default(),
        ))
    }

    async fn handle_list_kb(&self, input: QaInput) -> Result<ToolResult> {
        let workspace_id = Self::parse_uuid_opt(&input.workspace_id, "workspace_id")?;
        let project_id = Self::parse_uuid_opt(&input.project_id, "project_id")?;
        let created_by = Self::parse_uuid_opt(&input.created_by, "created_by")?;
        let kind = input.kind.clone();
        if let Some(k) = kind.as_deref() {
            let lower = k.trim().to_lowercase();
            if !lower.is_empty() && !VALID_KB_KINDS.contains(&lower.as_str()) {
                return Err(Error::Validation(format!(
                    "Invalid kind '{}'. Valid: {}.",
                    k,
                    VALID_KB_KINDS.join(", ")
                )));
            }
        }

        let params = QaListKbParams {
            workspace_id,
            project_id,
            kind,
            tag: input.tag,
            q: input.query,
            created_by,
            page: input.page,
            per_page: input.per_page,
        };

        match self.client.qa_list_kb_items(params).await {
            Ok(response) => {
                let text = format_kb_list(&response);
                Ok(ToolResult::with_structured(
                    text,
                    serde_json::to_value(&response).unwrap_or_default(),
                ))
            }
            Err(err) if super::workspace_drift::is_workspace_access_error(&err) => Ok(
                super::workspace_drift::drift_collection_result("KB items", workspace_id, None),
            ),
            Err(err) => Err(err),
        }
    }

    async fn handle_get_kb(&self, input: QaInput) -> Result<ToolResult> {
        let id = Self::require_uuid(&input.id, "id")?;
        match self.client.qa_get_kb_item(id).await {
            Ok(item) => {
                let text = format_kb_item(&item);
                Ok(ToolResult::with_structured(
                    text,
                    serde_json::to_value(&item).unwrap_or_default(),
                ))
            }
            Err(err) if super::workspace_drift::is_workspace_access_error(&err) => Ok(
                super::workspace_drift::drift_single_result("KB item", None, None),
            ),
            Err(err) => Err(err),
        }
    }

    async fn handle_update_kb(&self, input: QaInput) -> Result<ToolResult> {
        let id = Self::require_uuid(&input.id, "id")?;
        let project_id = Self::parse_uuid_opt(&input.project_id, "project_id")?;
        let kind = input.kind.clone();
        if let Some(k) = kind.as_deref() {
            let lower = k.trim().to_lowercase();
            if !lower.is_empty() && !VALID_KB_KINDS.contains(&lower.as_str()) {
                return Err(Error::Validation(format!(
                    "Invalid kind '{}'. Valid: {}.",
                    k,
                    VALID_KB_KINDS.join(", ")
                )));
            }
        }
        let params = QaUpdateKbItemParams {
            title: input.title,
            content: input.content,
            kind,
            tags: input.tags,
            metadata: input.metadata,
            project_id,
        };
        let item = self.client.qa_update_kb_item(id, params).await?;
        let text = format!(
            "✓ updated · id={} · kind={} · scope={}",
            item.id,
            item.kind,
            scope_label(item.workspace_id, item.project_id)
        );
        Ok(ToolResult::with_structured(
            text,
            serde_json::to_value(&item).unwrap_or_default(),
        ))
    }

    async fn handle_delete_kb(&self, input: QaInput) -> Result<ToolResult> {
        let id = Self::require_uuid(&input.id, "id")?;
        let _ = self.client.qa_delete_kb_item(id).await?;
        Ok(ToolResult::text(format!(
            "✓ deleted · id={} · subsequent reads return 404",
            id
        )))
    }

    async fn handle_feedback(&self, input: QaInput) -> Result<ToolResult> {
        let answer_id = Self::require_uuid(&input.answer_id, "answer_id")?;
        let score = input
            .score
            .ok_or_else(|| Error::Validation("score is required (-1, 0, or +1)".to_string()))?;
        if !(-1..=1).contains(&score) {
            return Err(Error::Validation("score must be -1, 0, or +1".to_string()));
        }
        let response = self
            .client
            .qa_feedback(QaFeedbackParams { answer_id, score })
            .await?;
        let label = match response.feedback_score {
            v if v > 0 => "positive",
            v if v < 0 => "negative",
            _ => "cleared",
        };
        let text = format!(
            "✓ feedback {} · answer={} · score={}",
            label, response.answer_id, response.feedback_score
        );
        Ok(ToolResult::with_structured(
            text,
            serde_json::to_value(&response).unwrap_or_default(),
        ))
    }
}

// ============================================================================
// Headline formatters — match the v0.2.96 capsule docs-style family.
// ============================================================================

fn format_ask_response(response: &QaAskResult) -> String {
    let lead = if response.cached {
        "✓ cached"
    } else {
        "✓ answered"
    };
    let mut parts = vec![lead.to_string()];
    if let Some(c) = response.confidence {
        parts.push(format!("confidence={:.2}", c));
    }
    parts.push(format!(
        "{} sources",
        source_count_from_value(&response.source_refs)
    ));
    parts.push(format!("model={}", response.model_name));
    if !response.cached {
        parts.push(format!("{}ms", response.total_latency_ms));
    }
    let headline = parts.join(" · ");

    let mut lines = vec![headline, String::new(), response.answer_text.clone()];
    if !response.cached {
        lines.push(String::new());
        lines.push(format!(
            "Tier 1 (workspace+project): {} · Tier 2 (workspace peers): {} · embed {}ms · search {}ms · model {}ms",
            response.tier1_count,
            response.tier2_count,
            response.embed_latency_ms,
            response.search_latency_ms,
            response.friendli_latency_ms
        ));
    }
    if let (Some(p), Some(c)) = (response.prompt_token_count, response.completion_token_count) {
        lines.push(format!("Tokens: prompt={} · completion={}", p, c));
    }
    lines.push(format!(
        "answer_id: {}  ·  question_id: {}",
        response.answer_id, response.question_id
    ));
    lines.join("\n")
}

fn source_count_from_value(value: &Value) -> usize {
    value.as_array().map(|a| a.len()).unwrap_or(0)
}

fn format_search_response(response: &QaSearchResult) -> String {
    let headline = format!(
        "✓ {} prior Q&A · page {} of {} · per_page {}",
        response.total,
        response.page,
        ((response.total as f64) / (response.per_page.max(1) as f64)).ceil() as i64,
        response.per_page
    );
    if response.items.is_empty() {
        return format!("{}\n\nNo prior Q&A in scope.", headline);
    }
    let mut lines = vec![headline, String::new()];
    for hit in &response.items {
        lines.push(format_search_hit(hit));
    }
    lines.join("\n")
}

fn format_search_hit(hit: &QaSearchHit) -> String {
    let q = &hit.question;
    let answer_count = hit.answers.len();
    let mut out = format!(
        "Q ({}): {}",
        truncate_inline(&q.question_text, 200),
        scope_label(q.workspace_id, q.project_id)
    );
    out.push('\n');
    out.push_str(&format!(
        "   asked_by={} · session={} · {} answer(s) · created={}",
        q.asked_by_user_id
            .map(|u| u.to_string())
            .unwrap_or_else(|| "—".to_string()),
        q.session_id.as_deref().unwrap_or("—"),
        answer_count,
        q.created_at.to_rfc3339()
    ));
    if let Some(latest) = hit.answers.first() {
        out.push('\n');
        out.push_str(&format!(
            "   latest: feedback={} · cached={} · {}",
            latest.feedback_score,
            latest.cached,
            truncate_inline(&latest.answer_text, 160)
        ));
    }
    out
}

fn format_kb_create(item: &QaKbItem) -> String {
    format!(
        "✓ {} saved · id={} · kind={} · scope={}",
        item.kind,
        item.id,
        item.kind,
        scope_label(item.workspace_id, item.project_id)
    )
}

fn format_kb_list(response: &QaListKbResult) -> String {
    let headline = format!(
        "✓ {} KB items · page {} of {} · per_page {}",
        response.total,
        response.page,
        ((response.total as f64) / (response.per_page.max(1) as f64)).ceil() as i64,
        response.per_page
    );
    if response.items.is_empty() {
        return format!("{}\n\nNo KB items in scope.", headline);
    }
    let mut lines = vec![headline, String::new()];
    for item in &response.items {
        lines.push(format!(
            "- [{}] {} · {} · scope={}",
            item.kind,
            truncate_inline(&item.title, 100),
            item.id,
            scope_label(item.workspace_id, item.project_id)
        ));
    }
    lines.join("\n")
}

fn format_kb_item(item: &QaKbItem) -> String {
    let mut lines = vec![
        format!(
            "✓ {} · {} · {} · scope={}",
            item.kind,
            truncate_inline(&item.title, 100),
            item.id,
            scope_label(item.workspace_id, item.project_id)
        ),
        String::new(),
        item.content.clone(),
    ];
    if !item.tags.is_empty() {
        lines.push(String::new());
        lines.push(format!("Tags: {}", item.tags.join(", ")));
    }
    lines.join("\n")
}

fn scope_label(workspace_id: Uuid, project_id: Option<Uuid>) -> String {
    match project_id {
        Some(p) => format!("workspace={} project={}", workspace_id, p),
        None => format!("workspace={}", workspace_id),
    }
}

fn truncate_inline(text: &str, max_chars: usize) -> String {
    let single: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if single.chars().count() <= max_chars {
        return single;
    }
    let cap = max_chars.saturating_sub(1);
    let truncated: String = single.chars().take(cap).collect();
    format!("{}…", truncated)
}

pub fn register_qa_tools(
    registry: &mut crate::registry::ToolRegistry,
    client: ContextStreamClient,
) {
    registry.register("qa", Arc::new(QaTool::new(client)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ToolHandler;
    use crate::testing::TestFixtures;
    use mcp_types::tool::ToolCategory;

    fn create_mock_client() -> ContextStreamClient {
        ContextStreamClient::new(TestFixtures::test_config())
    }

    #[test]
    fn metadata_uses_branding_and_session_category() {
        let tool = QaTool::new(create_mock_client());
        let metadata = tool.metadata();
        assert_eq!(metadata.name, "qa");
        assert_eq!(metadata.title, "Agent Q&A");
        assert_eq!(metadata.category, ToolCategory::Session);
        // Stuck-helper guidance must be in the description so callers
        // see it in tools/list and self-regulate.
        assert!(metadata.description.contains("stuck"));
        assert!(metadata.description.contains("Not a reflex"));
        assert!(metadata.description.contains("ContextCode"));
        // The upstream model name must NEVER appear in the description.
        assert!(!metadata.description.to_lowercase().contains("glm"));
        assert!(!metadata.description.to_lowercase().contains("zai-org"));
    }

    #[test]
    fn schema_lists_every_action_and_kb_kind() {
        let tool = QaTool::new(create_mock_client());
        let schema = tool.input_schema();
        let action_enum = schema["properties"]["action"]["enum"].as_array().unwrap();
        for action in [
            "ask",
            "search",
            "save_kb",
            "list_kb",
            "get_kb",
            "update_kb",
            "delete_kb",
            "feedback",
            "explain",
        ] {
            assert!(
                action_enum.iter().any(|v| v == action),
                "action {action} missing from schema"
            );
        }
        let kind_enum = schema["properties"]["kind"]["enum"].as_array().unwrap();
        for kind in ["guidance", "guardrail", "faq", "runbook", "caveat"] {
            assert!(
                kind_enum.iter().any(|v| v == kind),
                "kind {kind} missing from schema"
            );
        }
    }

    #[tokio::test]
    async fn unknown_action_errors_with_valid_action_list() {
        let tool = QaTool::new(create_mock_client());
        let result = tool
            .execute(serde_json::json!({"action": "frobnicate"}))
            .await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Unknown action"));
        assert!(msg.contains("ask"));
        assert!(msg.contains("save_kb"));
    }

    #[tokio::test]
    async fn ask_requires_question() {
        let tool = QaTool::new(create_mock_client());
        let result = tool.execute(serde_json::json!({"action": "ask"})).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("question"));
    }

    #[tokio::test]
    async fn save_kb_rejects_invalid_kind() {
        let tool = QaTool::new(create_mock_client());
        let result = tool
            .execute(serde_json::json!({
                "action": "save_kb",
                "title": "T",
                "content": "C",
                "kind": "frobnicate",
            }))
            .await;
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("frobnicate"));
        assert!(msg.contains("guidance"));
    }

    #[tokio::test]
    async fn feedback_rejects_out_of_range_score() {
        let tool = QaTool::new(create_mock_client());
        let result = tool
            .execute(serde_json::json!({
                "action": "feedback",
                "answer_id": "00000000-0000-0000-0000-000000000000",
                "score": 5,
            }))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("-1, 0, or +1"));
    }

    #[test]
    fn format_ask_response_leads_with_docs_style_headline() {
        let response = QaAskResult {
            question_id: Uuid::nil(),
            answer_id: Uuid::nil(),
            answer_text: "Per [id=decision:abc] the team picks AWS.".to_string(),
            confidence: Some(0.85),
            source_refs: serde_json::json!([{"kind": "decision", "id": "abc"}]),
            model_name: "ContextCode".to_string(),
            friendli_request_id: Some("chatcmpl-x".to_string()),
            prompt_token_count: Some(123),
            completion_token_count: Some(456),
            total_token_count: Some(579),
            embed_latency_ms: 50,
            search_latency_ms: 80,
            friendli_latency_ms: 600,
            total_latency_ms: 750,
            tier1_count: 3,
            tier2_count: 2,
            cached: false,
        };
        let text = format_ask_response(&response);
        assert!(text.starts_with("✓ answered"));
        assert!(text.contains("confidence=0.85"));
        assert!(text.contains("1 sources"));
        assert!(text.contains("model=ContextCode"));
        assert!(text.contains("750ms"));
        assert!(text.contains("Per [id=decision:abc]"));
        assert!(text.contains("Tier 1"));
    }

    #[test]
    fn format_ask_response_marks_cached_in_headline() {
        let response = QaAskResult {
            question_id: Uuid::nil(),
            answer_id: Uuid::nil(),
            answer_text: "Cached answer".to_string(),
            confidence: Some(0.9),
            source_refs: serde_json::json!([]),
            model_name: "ContextCode".to_string(),
            friendli_request_id: None,
            prompt_token_count: None,
            completion_token_count: None,
            total_token_count: None,
            embed_latency_ms: 0,
            search_latency_ms: 0,
            friendli_latency_ms: 0,
            total_latency_ms: 5,
            tier1_count: 0,
            tier2_count: 0,
            cached: true,
        };
        let text = format_ask_response(&response);
        assert!(text.starts_with("✓ cached"));
        assert!(!text.contains("Tier 1"));
    }

    #[test]
    fn truncate_inline_collapses_whitespace_and_caps_chars() {
        let s = "  hello   world\nthis is a long string  ";
        assert_eq!(truncate_inline(s, 80), "hello world this is a long string");
        let long = "x".repeat(200);
        let out = truncate_inline(&long, 50);
        assert_eq!(out.chars().count(), 50);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn truncate_inline_is_unicode_safe() {
        let multi = "あいうえおかきくけこ";
        let out = truncate_inline(multi, 5);
        assert_eq!(out.chars().count(), 5);
        assert!(out.ends_with('…'));
    }
}
