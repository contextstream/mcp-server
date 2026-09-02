//! Natural-language Answer API MCP surface.
//!
//! The tool accepts only public, untrusted request hints. Effective identity,
//! scope, application grants, source selection, provider/model routing, and
//! one-use execution authority are resolved by the ContextStream API.

use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;
use mcp_client::{
    AnswerContextScopeV1, AnswerFeedbackRequestV1, AnswerFeedbackResponseV1,
    AnswerFeedbackSchemaVersionV1, AnswerFeedbackSignalV1, AnswerFeedbackTargetV1,
    AnswerLatencyBudgetRequestV1, AnswerPlanClientContextV1, AnswerRequestV1,
    AnswerResponseBudgetRequestV1, AnswerResponseModeV1, AnswerResponseV1, AnswerScopeModeV1,
    AnswerVisibilityV1, ContextStreamClient,
};
use mcp_types::{
    tool::{ToolAnnotations, ToolCategory, ToolMetadata, ToolResult},
    Error, Result,
};
use serde::Deserialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::{registry::ToolHandler, schema::SchemaBuilder};

const VALID_ACTIONS: &[&str] = &["query", "recent_changes", "receipt", "feedback"];
const VALID_SCOPE_MODES: &[&str] = &["auto", "explicit"];
const VALID_VISIBILITIES: &[&str] = &["personal", "project", "workspace", "account"];
const VALID_RESPONSE_MODES: &[&str] = &["concise", "detailed", "structured"];
const VALID_FEEDBACK_SIGNALS: &[&str] = &[
    "relevant",
    "useful_but_not_now",
    "wrong_project",
    "superseded",
    "never_for_topic",
    "promote_to_universal_rule",
];

const TOOL_DESCRIPTION: &str = "Ask ContextStream a natural-language question across the user's authorized context and receive one current, evidence-backed answer. Use receipt to recover an already-executed request without replay, and feedback to record an explicit receipt-bound signal; feedback acknowledgement means recorded_only. Use search instead for exact code/file discovery. Requested logical scope never grants authority: ContextStream resolves fresh effective authority server-side. Every Answer API request is sent exactly once with no transparent retry or replay.";

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerInput {
    pub action: Option<String>,
    pub question: Option<String>,
    pub scope_mode: Option<String>,
    pub account_id: Option<String>,
    pub workspace_ids: Option<Vec<String>>,
    pub project_ids: Option<Vec<String>>,
    pub visibility: Option<String>,
    pub timezone: Option<String>,
    pub current_workspace_id: Option<String>,
    pub current_project_id: Option<String>,
    pub response_mode: Option<String>,
    pub response_budget: Option<AnswerResponseBudgetInput>,
    pub latency_budget: Option<AnswerLatencyBudgetInput>,
    pub request_id: Option<String>,
    pub feedback_id: Option<String>,
    pub target: Option<AnswerFeedbackTargetInput>,
    pub signal: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case", deny_unknown_fields)]
pub enum AnswerFeedbackTargetInput {
    Answer {},
    Item { item_id: String },
    Citation { citation_id: String },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerResponseBudgetInput {
    pub max_items: u32,
    pub max_citations: u32,
    pub max_output_tokens: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerLatencyBudgetInput {
    pub total_ms: u32,
    pub retrieval_ms: u32,
    pub synthesis_ms: u32,
    pub authority_reserve_ms: u32,
}

pub struct AnswerTool {
    client: ContextStreamClient,
}

impl AnswerTool {
    pub fn new(client: ContextStreamClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl ToolHandler for AnswerTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let input: AnswerInput =
            serde_json::from_value(input).map_err(|error| Error::Validation(error.to_string()))?;
        match normalized_action(&input)?.as_str() {
            "receipt" => {
                let request_id = build_receipt_request_id(&input)?;
                let response = self.client.answer_receipt(request_id).await?;
                let text = format_response(&response);
                let structured = serde_json::to_value(&response).unwrap_or_default();
                Ok(ToolResult::with_structured(text, structured))
            }
            "feedback" => {
                let request = build_feedback_request(&input)?;
                let response = self.client.answer_feedback(request).await?;
                let text = format_feedback_response(&response);
                let structured = serde_json::to_value(&response).unwrap_or_default();
                Ok(ToolResult::with_structured(text, structured))
            }
            "query" | "recent_changes" => {
                let request = build_request(input)?;
                let response = self.client.answer_query(request).await?;
                let text = format_response(&response);
                let structured = serde_json::to_value(&response).unwrap_or_default();
                Ok(ToolResult::with_structured(text, structured))
            }
            _ => unreachable!("normalized_action validates the closed action set"),
        }
    }

    fn metadata(&self) -> &ToolMetadata {
        static METADATA: std::sync::OnceLock<ToolMetadata> = std::sync::OnceLock::new();
        METADATA.get_or_init(|| ToolMetadata {
            name: "answer".to_owned(),
            title: "Natural-Language Answer".to_owned(),
            description: TOOL_DESCRIPTION.to_owned(),
            category: ToolCategory::Ai,
            annotations: ToolAnnotations {
                read_only: false,
                destructive: false,
                requires_confirmation: false,
                idempotent: false,
                long_running: true,
                open_world: true,
            },
            is_pro: false,
            required_tier: None,
        })
    }

    fn input_schema(&self) -> Value {
        let uuid_array = |description: &str| {
            json!({
                "type": "array",
                "description": description,
                "items": { "type": "string", "format": "uuid" },
                "uniqueItems": true
            })
        };
        let mut schema = SchemaBuilder::new()
            .description(TOOL_DESCRIPTION)
            .string_enum(
                "action",
                "query requires question; recent_changes uses the canonical question; receipt requires request_id; feedback requires request_id, feedback_id, and signal. Defaults to query.",
                VALID_ACTIONS,
                false,
            )
            .string("question", "Natural-language context question", false)
            .string_enum(
                "scope_mode",
                "Requested scope mode. Defaults to auto; this never grants authority.",
                VALID_SCOPE_MODES,
                false,
            )
            .uuid("account_id", "Optional requested account ID", false)
            .property(
                "workspace_ids",
                uuid_array("Requested workspace IDs; empty means all currently authorized workspaces in account scope"),
                false,
            )
            .property(
                "project_ids",
                uuid_array("Requested project IDs; empty means all currently authorized projects in the requested workspaces"),
                false,
            )
            .string_enum(
                "visibility",
                "Requested visibility; inferred from supplied IDs and otherwise defaults to account",
                VALID_VISIBILITIES,
                false,
            )
            .string(
                "timezone",
                "IANA timezone used to interpret relative time. Defaults to UTC.",
                false,
            )
            .uuid(
                "current_workspace_id",
                "Optional current-workspace relevance hint; not authority",
                false,
            )
            .uuid(
                "current_project_id",
                "Optional current-project relevance hint; not authority",
                false,
            )
            .string_enum(
                "response_mode",
                "Requested response mode",
                VALID_RESPONSE_MODES,
                false,
            )
            .property(
                "response_budget",
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["max_items", "max_citations", "max_output_tokens"],
                    "properties": {
                        "max_items": { "type": "integer", "minimum": 1, "maximum": 100 },
                        "max_citations": { "type": "integer", "minimum": 1, "maximum": 250 },
                        "max_output_tokens": { "type": "integer", "minimum": 64, "maximum": 8192 }
                    }
                }),
                false,
            )
            .property(
                "latency_budget",
                json!({
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["total_ms", "retrieval_ms", "synthesis_ms", "authority_reserve_ms"],
                    "properties": {
                        "total_ms": { "type": "integer", "minimum": 100, "maximum": 30000 },
                        "retrieval_ms": { "type": "integer", "minimum": 1, "maximum": 30000 },
                        "synthesis_ms": { "type": "integer", "minimum": 1, "maximum": 30000 },
                        "authority_reserve_ms": { "type": "integer", "minimum": 1, "maximum": 30000 }
                    }
                }),
                false,
            )
            .uuid(
                "request_id",
                "Receipt request ID for action=receipt or action=feedback",
                false,
            )
            .uuid(
                "feedback_id",
                "Caller-generated idempotency ID required for action=feedback",
                false,
            )
            .property(
                "target",
                json!({
                    "description": "Feedback target. Defaults to the whole answer.",
                    "oneOf": [
                        {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["target"],
                            "properties": { "target": { "const": "answer" } }
                        },
                        {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["target", "item_id"],
                            "properties": {
                                "target": { "const": "item" },
                                "item_id": { "type": "string", "format": "uuid" }
                            }
                        },
                        {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["target", "citation_id"],
                            "properties": {
                                "target": { "const": "citation" },
                                "citation_id": { "type": "string", "format": "uuid" }
                            }
                        }
                    ]
                }),
                false,
            )
            .string_enum(
                "signal",
                "Receipt-bound feedback signal for action=feedback",
                VALID_FEEDBACK_SIGNALS,
                false,
            )
            .build();
        schema["additionalProperties"] = json!(false);
        schema["properties"]["question"]["minLength"] = json!(1);
        schema["properties"]["question"]["maxLength"] = json!(8192);
        schema["properties"]["timezone"]["maxLength"] = json!(64);
        schema["allOf"] = json!([
            {
                "if": {
                    "required": ["action"],
                    "properties": { "action": { "const": "receipt" } }
                },
                "then": { "required": ["request_id"] }
            },
            {
                "if": {
                    "required": ["action"],
                    "properties": { "action": { "const": "feedback" } }
                },
                "then": { "required": ["request_id", "feedback_id", "signal"] }
            }
        ]);
        schema
    }
}

fn parse_uuid(value: Option<String>, field: &str) -> Result<Option<Uuid>> {
    value
        .map(|raw| {
            Uuid::parse_str(raw.trim())
                .map_err(|_| Error::Validation(format!("{field} must be a UUID")))
        })
        .transpose()
}

fn parse_uuid_set(values: Option<Vec<String>>, field: &str) -> Result<BTreeSet<Uuid>> {
    values
        .unwrap_or_default()
        .into_iter()
        .map(|raw| {
            Uuid::parse_str(raw.trim())
                .map_err(|_| Error::Validation(format!("every {field} entry must be a UUID")))
        })
        .collect()
}

fn normalized_action(input: &AnswerInput) -> Result<String> {
    let action = input
        .action
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("query")
        .to_ascii_lowercase();
    if !VALID_ACTIONS.contains(&action.as_str()) {
        return Err(Error::Validation(format!(
            "action must be one of: {}",
            VALID_ACTIONS.join(", ")
        )));
    }
    Ok(action)
}

fn parse_required_uuid(value: Option<&String>, field: &str) -> Result<Uuid> {
    let raw = value
        .map(String::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::Validation(format!("{field} is required")))?;
    let id =
        Uuid::parse_str(raw).map_err(|_| Error::Validation(format!("{field} must be a UUID")))?;
    if id.is_nil() {
        return Err(Error::Validation(format!("{field} must not be nil")));
    }
    Ok(id)
}

fn has_query_fields(input: &AnswerInput) -> bool {
    input.question.is_some()
        || input.scope_mode.is_some()
        || input.account_id.is_some()
        || input.workspace_ids.is_some()
        || input.project_ids.is_some()
        || input.visibility.is_some()
        || input.timezone.is_some()
        || input.current_workspace_id.is_some()
        || input.current_project_id.is_some()
        || input.response_mode.is_some()
        || input.response_budget.is_some()
        || input.latency_budget.is_some()
}

fn build_receipt_request_id(input: &AnswerInput) -> Result<Uuid> {
    if has_query_fields(input)
        || input.feedback_id.is_some()
        || input.target.is_some()
        || input.signal.is_some()
    {
        return Err(Error::Validation(
            "action=receipt accepts only action and request_id".to_owned(),
        ));
    }
    parse_required_uuid(input.request_id.as_ref(), "request_id")
}

fn build_feedback_request(input: &AnswerInput) -> Result<AnswerFeedbackRequestV1> {
    if has_query_fields(input) {
        return Err(Error::Validation(
            "action=feedback does not accept query, scope, budget, or timezone fields".to_owned(),
        ));
    }
    let signal = match input.signal.as_deref().map(str::trim) {
        Some("relevant") => AnswerFeedbackSignalV1::Relevant,
        Some("useful_but_not_now") => AnswerFeedbackSignalV1::UsefulButNotNow,
        Some("wrong_project") => AnswerFeedbackSignalV1::WrongProject,
        Some("superseded") => AnswerFeedbackSignalV1::Superseded,
        Some("never_for_topic") => AnswerFeedbackSignalV1::NeverForTopic,
        Some("promote_to_universal_rule") => AnswerFeedbackSignalV1::PromoteToUniversalRule,
        Some(_) => {
            return Err(Error::Validation(format!(
                "signal must be one of: {}",
                VALID_FEEDBACK_SIGNALS.join(", ")
            )))
        }
        None => return Err(Error::Validation("signal is required".to_owned())),
    };
    let target = match input.target.as_ref() {
        None | Some(AnswerFeedbackTargetInput::Answer {}) => AnswerFeedbackTargetV1::Answer {},
        Some(AnswerFeedbackTargetInput::Item { item_id }) => AnswerFeedbackTargetV1::Item {
            item_id: parse_required_uuid(Some(item_id), "target.item_id")?,
        },
        Some(AnswerFeedbackTargetInput::Citation { citation_id }) => {
            AnswerFeedbackTargetV1::Citation {
                citation_id: parse_required_uuid(Some(citation_id), "target.citation_id")?,
            }
        }
    };
    Ok(AnswerFeedbackRequestV1 {
        schema_version: AnswerFeedbackSchemaVersionV1::V1,
        feedback_id: parse_required_uuid(input.feedback_id.as_ref(), "feedback_id")?,
        request_id: parse_required_uuid(input.request_id.as_ref(), "request_id")?,
        target,
        signal,
    })
}

fn build_request(input: AnswerInput) -> Result<AnswerRequestV1> {
    let action = normalized_action(&input)?;
    if !matches!(action.as_str(), "query" | "recent_changes") {
        return Err(Error::Validation(
            "build_request only accepts query or recent_changes".to_owned(),
        ));
    }
    if input.request_id.is_some()
        || input.feedback_id.is_some()
        || input.target.is_some()
        || input.signal.is_some()
    {
        return Err(Error::Validation(
            "query actions do not accept receipt or feedback fields".to_owned(),
        ));
    }
    let question = input
        .question
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .or_else(|| (action == "recent_changes").then(|| "What changed recently?".to_owned()))
        .ok_or_else(|| Error::Validation("question is required for action=query".to_owned()))?;

    let scope_mode = match input.scope_mode.as_deref().map(str::trim) {
        None | Some("") | Some("auto") => AnswerScopeModeV1::Auto,
        Some("explicit") => AnswerScopeModeV1::Explicit,
        Some(_) => {
            return Err(Error::Validation(format!(
                "scope_mode must be one of: {}",
                VALID_SCOPE_MODES.join(", ")
            )))
        }
    };
    let workspace_ids = parse_uuid_set(input.workspace_ids, "workspace_ids")?;
    let project_ids = parse_uuid_set(input.project_ids, "project_ids")?;
    let visibility = match input.visibility.as_deref().map(str::trim) {
        Some("personal") => AnswerVisibilityV1::Personal,
        Some("project") => AnswerVisibilityV1::Project,
        Some("workspace") => AnswerVisibilityV1::Workspace,
        Some("account") => AnswerVisibilityV1::Account,
        None | Some("") if !project_ids.is_empty() => AnswerVisibilityV1::Project,
        None | Some("") if !workspace_ids.is_empty() => AnswerVisibilityV1::Workspace,
        None | Some("") => AnswerVisibilityV1::Account,
        Some(_) => {
            return Err(Error::Validation(format!(
                "visibility must be one of: {}",
                VALID_VISIBILITIES.join(", ")
            )))
        }
    };
    let response_mode = match input.response_mode.as_deref().map(str::trim) {
        None | Some("") if action == "recent_changes" => Some(AnswerResponseModeV1::Structured),
        None | Some("") => None,
        Some("concise") => Some(AnswerResponseModeV1::Concise),
        Some("detailed") => Some(AnswerResponseModeV1::Detailed),
        Some("structured") => Some(AnswerResponseModeV1::Structured),
        Some(_) => {
            return Err(Error::Validation(format!(
                "response_mode must be one of: {}",
                VALID_RESPONSE_MODES.join(", ")
            )))
        }
    };
    let timezone = input
        .timezone
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "UTC".to_owned());

    let response_budget = input.response_budget.map(|budget| {
        if !(1..=100).contains(&budget.max_items)
            || !(1..=250).contains(&budget.max_citations)
            || !(64..=8_192).contains(&budget.max_output_tokens)
        {
            return Err(Error::Validation(
                "response_budget requires max_items 1..=100, max_citations 1..=250, and max_output_tokens 64..=8192"
                    .to_owned(),
            ));
        }
        Ok(AnswerResponseBudgetRequestV1 {
            max_items: budget.max_items,
            max_citations: budget.max_citations,
            max_output_tokens: budget.max_output_tokens,
        })
    }).transpose()?;
    let latency_budget = input.latency_budget.map(|budget| {
        let components = budget
            .retrieval_ms
            .checked_add(budget.synthesis_ms)
            .and_then(|value| value.checked_add(budget.authority_reserve_ms));
        if !(100..=30_000).contains(&budget.total_ms)
            || budget.retrieval_ms == 0
            || budget.synthesis_ms == 0
            || budget.authority_reserve_ms == 0
            || components != Some(budget.total_ms)
        {
            return Err(Error::Validation(
                "latency_budget requires total_ms 100..=30000, positive components, and retrieval_ms + synthesis_ms + authority_reserve_ms = total_ms"
                    .to_owned(),
            ));
        }
        Ok(AnswerLatencyBudgetRequestV1 {
            total_ms: budget.total_ms,
            retrieval_ms: budget.retrieval_ms,
            synthesis_ms: budget.synthesis_ms,
            authority_reserve_ms: budget.authority_reserve_ms,
        })
    }).transpose()?;

    Ok(AnswerRequestV1 {
        question,
        scope_mode,
        requested_scope: AnswerContextScopeV1 {
            account_id: parse_uuid(input.account_id, "account_id")?,
            workspace_ids,
            project_ids,
            visibility,
        },
        client_context: AnswerPlanClientContextV1 {
            timezone,
            current_workspace_id: parse_uuid(input.current_workspace_id, "current_workspace_id")?,
            current_project_id: parse_uuid(input.current_project_id, "current_project_id")?,
        },
        response_mode,
        response_budget,
        latency_budget,
    })
}

fn format_response(response: &AnswerResponseV1) -> String {
    match response {
        AnswerResponseV1::Completed {
            request_id,
            answer,
            items,
            citations,
            conflicts,
            freshness,
            coverage,
            degradation,
            latency,
            ..
        } => {
            let mut lines = vec![answer.trim().to_owned(), String::new()];
            lines.push(format!(
                "Evidence: {} item(s), {} citation(s), {} conflict(s); coverage {}/{} source slot(s).",
                items.len(),
                citations.len(),
                conflicts.len(),
                coverage.iter().filter(|entry| matches!(entry.status, mcp_types::AnswerCoverageStatusV1::Complete)).count(),
                coverage.len(),
            ));
            lines.push(format!(
                "As of: {} · Latency: {} ms · Request: {}",
                freshness.as_of.to_rfc3339(),
                latency.total_ms,
                request_id,
            ));
            if degradation.partial {
                lines.push("Partial result: inspect structured coverage and degradation metadata before acting.".to_owned());
            }
            lines.join("\n")
        }
        AnswerResponseV1::ClarificationRequired {
            request_id,
            clarification,
            ..
        } => format!(
            "Clarification required: {}\n\nCandidate scopes: {} · Request: {}",
            clarification.prompt,
            clarification.candidate_scopes.len(),
            request_id,
        ),
    }
}

fn format_feedback_response(response: &AnswerFeedbackResponseV1) -> String {
    format!(
        "Feedback recorded for Answer request {}. Effect: recorded_only. Feedback ID: {}.",
        response.request_id, response.feedback_id
    )
}

pub fn register_answer_tools(
    registry: &mut crate::registry::ToolRegistry,
    client: ContextStreamClient,
) {
    registry.register("answer", Arc::new(AnswerTool::new(client)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TestFixtures;

    fn tool() -> AnswerTool {
        AnswerTool::new(ContextStreamClient::new(TestFixtures::test_config()))
    }

    #[test]
    fn metadata_is_non_destructive_but_not_read_only_or_retry_safe() {
        let metadata = tool().metadata().clone();
        assert_eq!(metadata.name, "answer");
        assert_eq!(metadata.category, ToolCategory::Ai);
        assert!(!metadata.annotations.read_only);
        assert!(!metadata.annotations.destructive);
        assert!(!metadata.annotations.requires_confirmation);
        assert!(!metadata.annotations.idempotent);
        assert!(metadata.annotations.long_running);
        assert!(metadata.description.contains("exactly once"));
        assert!(metadata.description.contains("recorded_only"));
    }

    #[test]
    fn schema_is_closed_and_exposes_cross_workspace_scope_and_budgets() {
        let schema = tool().input_schema();
        assert_eq!(schema["additionalProperties"], false);
        for field in [
            "action",
            "question",
            "scope_mode",
            "account_id",
            "workspace_ids",
            "project_ids",
            "visibility",
            "timezone",
            "response_mode",
            "response_budget",
            "latency_budget",
            "request_id",
            "feedback_id",
            "target",
            "signal",
        ] {
            assert!(schema["properties"].get(field).is_some(), "missing {field}");
        }
        assert_eq!(schema["properties"]["workspace_ids"]["uniqueItems"], true);
        assert_eq!(schema["allOf"].as_array().map(Vec::len), Some(2));
    }

    #[test]
    fn recent_changes_defaults_to_authority_free_account_auto_scope() {
        let request =
            build_request(serde_json::from_value(json!({ "action": "recent_changes" })).unwrap())
                .unwrap();
        let wire = serde_json::to_value(request).unwrap();

        assert_eq!(wire["question"], "What changed recently?");
        assert_eq!(wire["scope_mode"], "auto");
        assert_eq!(wire["requested_scope"]["visibility"], "account");
        assert_eq!(wire["requested_scope"]["workspace_ids"], json!([]));
        assert_eq!(wire["requested_scope"]["project_ids"], json!([]));
        assert_eq!(wire["client_context"]["timezone"], "UTC");
        for forbidden in [
            "actor",
            "application_id",
            "authorization",
            "effective_scopes",
            "tenant_route",
            "model",
        ] {
            assert!(wire.get(forbidden).is_none());
        }
    }

    #[test]
    fn query_requires_nonempty_question_and_rejects_unknown_fields() {
        assert!(build_request(serde_json::from_value(json!({})).unwrap()).is_err());
        assert!(serde_json::from_value::<AnswerInput>(json!({
            "action": "query",
            "question": "current truth?",
            "tenant_route": "forged"
        }))
        .is_err());
        let query_with_receipt_field: AnswerInput = serde_json::from_value(json!({
            "action": "query",
            "question": "current truth?",
            "request_id": Uuid::from_u128(9)
        }))
        .unwrap();
        assert!(build_request(query_with_receipt_field).is_err());
    }

    #[test]
    fn receipt_requires_one_non_nil_request_id_and_rejects_query_fields() {
        let request_id = Uuid::from_u128(10);
        let valid: AnswerInput = serde_json::from_value(json!({
            "action": "receipt",
            "request_id": request_id
        }))
        .unwrap();
        assert_eq!(build_receipt_request_id(&valid).unwrap(), request_id);

        let mixed: AnswerInput = serde_json::from_value(json!({
            "action": "receipt",
            "request_id": request_id,
            "question": "run this again"
        }))
        .unwrap();
        assert!(build_receipt_request_id(&mixed).is_err());
        let nil: AnswerInput = serde_json::from_value(json!({
            "action": "receipt",
            "request_id": Uuid::nil()
        }))
        .unwrap();
        assert!(build_receipt_request_id(&nil).is_err());
    }

    #[test]
    fn feedback_requires_client_idempotency_and_preserves_recorded_only_semantics() {
        let request_id = Uuid::from_u128(11);
        let feedback_id = Uuid::from_u128(12);
        let input: AnswerInput = serde_json::from_value(json!({
            "action": "feedback",
            "request_id": request_id,
            "feedback_id": feedback_id,
            "target": { "target": "answer" },
            "signal": "relevant"
        }))
        .unwrap();
        let request = build_feedback_request(&input).unwrap();
        assert_eq!(request.request_id, request_id);
        assert_eq!(request.feedback_id, feedback_id);
        assert_eq!(request.target, AnswerFeedbackTargetV1::Answer {});
        assert_eq!(request.signal, AnswerFeedbackSignalV1::Relevant);

        let missing_feedback_id: AnswerInput = serde_json::from_value(json!({
            "action": "feedback",
            "request_id": request_id,
            "signal": "relevant"
        }))
        .unwrap();
        assert!(build_feedback_request(&missing_feedback_id).is_err());

        let mixed: AnswerInput = serde_json::from_value(json!({
            "action": "feedback",
            "request_id": request_id,
            "feedback_id": feedback_id,
            "signal": "relevant",
            "timezone": "UTC"
        }))
        .unwrap();
        assert!(build_feedback_request(&mixed).is_err());
    }

    #[test]
    fn request_budget_validation_matches_the_server_contract() {
        let valid = serde_json::from_value(json!({
            "action": "query",
            "question": "What is current?",
            "response_budget": {
                "max_items": 100,
                "max_citations": 250,
                "max_output_tokens": 8192
            },
            "latency_budget": {
                "total_ms": 1500,
                "retrieval_ms": 900,
                "synthesis_ms": 500,
                "authority_reserve_ms": 100
            }
        }))
        .unwrap();
        assert!(build_request(valid).is_ok());

        for invalid in [
            json!({
                "action": "query",
                "question": "What is current?",
                "response_budget": {
                    "max_items": 0,
                    "max_citations": 50,
                    "max_output_tokens": 1200
                }
            }),
            json!({
                "action": "query",
                "question": "What is current?",
                "latency_budget": {
                    "total_ms": 1500,
                    "retrieval_ms": 900,
                    "synthesis_ms": 500,
                    "authority_reserve_ms": 99
                }
            }),
        ] {
            let input = serde_json::from_value(invalid).unwrap();
            assert!(build_request(input).is_err());
        }
    }
}
