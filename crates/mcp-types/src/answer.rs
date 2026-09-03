//! Public ContextStream Answer API v1 wire types used by MCP clients.
//!
//! These types intentionally contain requested logical scope only. Actor
//! identity, effective authorization, tenant routes, provider/model choices,
//! and durable execution receipts remain server-owned.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerScopeModeV1 {
    Explicit,
    Auto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerVisibilityV1 {
    Personal,
    Project,
    Workspace,
    Account,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerContextScopeV1 {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<Uuid>,
    pub workspace_ids: BTreeSet<Uuid>,
    pub project_ids: BTreeSet<Uuid>,
    pub visibility: AnswerVisibilityV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerPlanClientContextV1 {
    pub timezone: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_workspace_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_project_id: Option<Uuid>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerResponseModeV1 {
    Concise,
    Detailed,
    Structured,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerResponseBudgetRequestV1 {
    pub max_items: u32,
    pub max_citations: u32,
    pub max_output_tokens: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerLatencyBudgetRequestV1 {
    pub total_ms: u32,
    pub retrieval_ms: u32,
    pub synthesis_ms: u32,
    pub authority_reserve_ms: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerRequestV1 {
    pub question: String,
    pub scope_mode: AnswerScopeModeV1,
    pub requested_scope: AnswerContextScopeV1,
    pub client_context: AnswerPlanClientContextV1,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_mode: Option<AnswerResponseModeV1>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_budget: Option<AnswerResponseBudgetRequestV1>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_budget: Option<AnswerLatencyBudgetRequestV1>,
}

impl AnswerRequestV1 {
    /// Build the account-wide auto-scope request used by CoFlow's
    /// "What changed recently?" journey. Empty ID sets request all currently
    /// authorized scopes; they never grant additional access.
    pub fn coflow_recent_changes(
        question: Option<String>,
        timezone: impl Into<String>,
        account_id: Option<Uuid>,
        current_workspace_id: Option<Uuid>,
        current_project_id: Option<Uuid>,
    ) -> Self {
        Self {
            question: question.unwrap_or_else(|| "What changed recently?".to_owned()),
            scope_mode: AnswerScopeModeV1::Auto,
            requested_scope: AnswerContextScopeV1 {
                account_id,
                workspace_ids: BTreeSet::new(),
                project_ids: BTreeSet::new(),
                visibility: AnswerVisibilityV1::Account,
            },
            client_context: AnswerPlanClientContextV1 {
                timezone: timezone.into(),
                current_workspace_id,
                current_project_id,
            },
            response_mode: Some(AnswerResponseModeV1::Structured),
            response_budget: None,
            latency_budget: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AnswerSchemaVersionV1 {
    #[serde(rename = "answer.v1")]
    V1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerIntentV1 {
    CurrentTruth,
    RecentChanges,
    DecisionTrace,
    ActiveWork,
    StatusRisk,
    OpenCoordination,
    SemanticRetrieval,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerSourceClassV1 {
    Assertions,
    Documents,
    Decisions,
    Events,
    Tasks,
    Transcripts,
    Code,
    Entities,
    Coordination,
    LiveState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerTimeRuleV1 {
    Recently,
    LastHours,
    LastDays,
    LastWeeks,
    Today,
    Yesterday,
    ThisWeek,
    PointInTimeNow,
    HistoryThroughNow,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerTimeInterpretationV1 {
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub anchor: DateTime<Utc>,
    pub timezone: String,
    pub natural_language: String,
    pub rule: AnswerTimeRuleV1,
    pub quantity: Option<u32>,
    pub confidence_basis_points: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerPlanInterpretationV1 {
    pub plan_id: Uuid,
    pub intent: AnswerIntentV1,
    pub intent_confidence_basis_points: u16,
    pub requested_scope: AnswerContextScopeV1,
    pub time: AnswerTimeInterpretationV1,
    pub response_mode: AnswerResponseModeV1,
    pub source_classes: BTreeSet<AnswerSourceClassV1>,
    pub omitted_source_classes: BTreeSet<AnswerSourceClassV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerResolvedScopeV1 {
    pub scope_ref: u16,
    pub scope: AnswerContextScopeV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerTruthStatusV1 {
    Current,
    Stale,
    Disputed,
    Invalid,
    Historical,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerItemV1 {
    pub item_id: Uuid,
    pub statement: String,
    pub truth_status: AnswerTruthStatusV1,
    pub source_class: AnswerSourceClassV1,
    pub scope_ref: u16,
    pub occurred_at: DateTime<Utc>,
    pub observed_at: DateTime<Utc>,
    pub score_basis_points: u16,
    pub citation_ids: BTreeSet<Uuid>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerCitationV1 {
    pub citation_id: Uuid,
    pub source_class: AnswerSourceClassV1,
    pub source_ref: String,
    pub label: String,
    pub observed_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerConflictV1 {
    pub subject: String,
    pub competing_item_ids: BTreeSet<Uuid>,
    pub resolution: AnswerTruthStatusV1,
    pub explanation: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerFreshnessV1 {
    pub as_of: DateTime<Utc>,
    pub oldest_observed_at: Option<DateTime<Utc>>,
    pub newest_observed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerWatermarkV1 {
    pub source_class: AnswerSourceClassV1,
    pub scope_ref: u16,
    pub captured_at: DateTime<Utc>,
    pub complete_through: Option<DateTime<Utc>>,
    pub token: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerCoverageStatusV1 {
    Complete,
    Partial,
    Unavailable,
    TimedOut,
    BudgetExceeded,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerCoverageV1 {
    pub source_class: AnswerSourceClassV1,
    pub scope_ref: u16,
    pub status: AnswerCoverageStatusV1,
    pub matched_count: u32,
    pub returned_count: u32,
    pub truncated_count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerDegradationReasonV1 {
    SourceUnavailable,
    SourceTimedOut,
    SourceBudgetExceeded,
    IncompleteHorizon,
    OutputTruncated,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerDegradationV1 {
    pub partial: bool,
    pub reasons: BTreeSet<AnswerDegradationReasonV1>,
    pub omitted_source_classes: BTreeSet<AnswerSourceClassV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerTruncationV1 {
    pub eligible_item_count: u32,
    pub emitted_item_count: u32,
    pub item_truncated_count: u32,
    pub available_citation_count: u32,
    pub emitted_citation_count: u32,
    pub citation_truncated_count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AnswerTokenizerV1 {
    #[serde(rename = "o200k_base")]
    O200kBase,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerOutputBudgetV1 {
    pub tokenizer: AnswerTokenizerV1,
    pub max_output_tokens: u32,
    pub exact_output_tokens: u32,
    pub max_serialized_bytes: u32,
    pub serialized_bytes: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerLatencyV1 {
    pub total_ms: u32,
    pub authority_ms: u32,
    pub retrieval_ms: u32,
    pub synthesis_ms: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerCacheStatusV1 {
    Hit,
    Miss,
    Bypass,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerCacheV1 {
    pub status: AnswerCacheStatusV1,
    pub age_ms: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerClarificationKindV1 {
    AmbiguousIntent,
    ScopeRequired,
    ConflictingScopeLanguage,
    ScopeSelectionMismatch,
    ConflictingTimeLanguage,
    UnsupportedTimeExpression,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerClarificationV1 {
    pub clarification_id: Uuid,
    pub kind: AnswerClarificationKindV1,
    pub prompt: String,
    pub candidate_scopes: Vec<AnswerContextScopeV1>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum AnswerResponseV1 {
    Completed {
        schema_version: AnswerSchemaVersionV1,
        request_id: Uuid,
        answer_id: Uuid,
        application_id: Option<Uuid>,
        plan: AnswerPlanInterpretationV1,
        resolved_scopes: Vec<AnswerResolvedScopeV1>,
        answer: String,
        items: Vec<AnswerItemV1>,
        citations: Vec<AnswerCitationV1>,
        conflicts: Vec<AnswerConflictV1>,
        freshness: AnswerFreshnessV1,
        watermarks: Vec<AnswerWatermarkV1>,
        coverage: Vec<AnswerCoverageV1>,
        degradation: AnswerDegradationV1,
        truncation: AnswerTruncationV1,
        output_budget: AnswerOutputBudgetV1,
        latency: AnswerLatencyV1,
        cache: AnswerCacheV1,
        next_cursor: Option<String>,
        generated_at: DateTime<Utc>,
    },
    ClarificationRequired {
        schema_version: AnswerSchemaVersionV1,
        request_id: Uuid,
        application_id: Option<Uuid>,
        clarification: AnswerClarificationV1,
    },
}

impl AnswerResponseV1 {
    pub fn request_id(&self) -> Uuid {
        match self {
            Self::Completed { request_id, .. } | Self::ClarificationRequired { request_id, .. } => {
                *request_id
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AnswerFeedbackSchemaVersionV1 {
    #[serde(rename = "answer.feedback.v1")]
    V1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "target", rename_all = "snake_case", deny_unknown_fields)]
pub enum AnswerFeedbackTargetV1 {
    Answer {},
    Item { item_id: Uuid },
    Citation { citation_id: Uuid },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerFeedbackSignalV1 {
    Relevant,
    UsefulButNotNow,
    WrongProject,
    Superseded,
    NeverForTopic,
    PromoteToUniversalRule,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerFeedbackRequestV1 {
    pub schema_version: AnswerFeedbackSchemaVersionV1,
    pub feedback_id: Uuid,
    pub request_id: Uuid,
    pub target: AnswerFeedbackTargetV1,
    pub signal: AnswerFeedbackSignalV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerFeedbackStatusV1 {
    Accepted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerFeedbackEffectV1 {
    RecordedOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnswerFeedbackResponseV1 {
    pub schema_version: AnswerFeedbackSchemaVersionV1,
    pub feedback_id: Uuid,
    pub request_id: Uuid,
    pub target: AnswerFeedbackTargetV1,
    pub signal: AnswerFeedbackSignalV1,
    pub status: AnswerFeedbackStatusV1,
    pub effect: AnswerFeedbackEffectV1,
    pub recorded_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coflow_request_is_authority_free_account_auto_scope() {
        let request = AnswerRequestV1::coflow_recent_changes(
            None,
            "America/Los_Angeles",
            Some(Uuid::from_u128(1)),
            Some(Uuid::from_u128(2)),
            None,
        );
        let wire = serde_json::to_value(request).unwrap();

        assert_eq!(wire["question"], "What changed recently?");
        assert_eq!(wire["scope_mode"], "auto");
        assert_eq!(wire["requested_scope"]["visibility"], "account");
        assert_eq!(
            wire["requested_scope"]["workspace_ids"],
            serde_json::json!([])
        );
        assert_eq!(
            wire["requested_scope"]["project_ids"],
            serde_json::json!([])
        );
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
    fn response_discriminator_is_closed() {
        let valid = serde_json::json!({
            "status": "clarification_required",
            "schema_version": "answer.v1",
            "request_id": Uuid::from_u128(1),
            "application_id": null,
            "clarification": {
                "clarification_id": Uuid::from_u128(2),
                "kind": "scope_required",
                "prompt": "Which workspace?",
                "candidate_scopes": [],
                "created_at": "2026-09-02T00:00:00Z"
            }
        });
        assert!(serde_json::from_value::<AnswerResponseV1>(valid.clone()).is_ok());

        let mut forged = valid;
        forged["tenant_route"] = serde_json::json!("private");
        assert!(serde_json::from_value::<AnswerResponseV1>(forged).is_err());
    }

    #[test]
    fn feedback_answer_target_rejects_extra_fields() {
        let valid = serde_json::json!({"target": "answer"});
        assert!(serde_json::from_value::<AnswerFeedbackTargetV1>(valid.clone()).is_ok());

        let mut forged = valid;
        forged["item_id"] = serde_json::json!(Uuid::from_u128(3));
        assert!(serde_json::from_value::<AnswerFeedbackTargetV1>(forged).is_err());
    }
}
