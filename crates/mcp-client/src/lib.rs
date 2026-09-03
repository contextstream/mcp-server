//! HTTP client for the ContextStream API.
//!
//! This crate provides a high-performance async HTTP client with:
//! - Automatic retries with exponential backoff
//! - Rate limit handling
//! - Connection pooling
//! - Auth override support

// Several request/ingest helpers legitimately take many parameters (workspace,
// project, paths, flags, origin, reroot, ...). Refactoring them into option
// structs is out of scope for the clippy cleanup, so allow the stylistic lint
// crate-wide rather than scattering per-fn attributes.
#![allow(clippy::too_many_arguments)]

pub mod activation;
pub mod auth;
pub mod cache;
pub mod client;
pub mod harness_readiness;
pub mod harness_remote;
pub mod ingest_guard;
pub mod json;
pub mod parity;
pub mod retry;
pub mod ticket;

pub use cache::{Cache, CacheKey};
pub use client::resolve_capsule_share_token_str;
pub use client::SessionRefreshHook;
pub use client::MEDIA_INDEX_MAX_BYTES;
pub use client::{
    entity_kind_to_path,
    feed_source_key,
    infer_memory_query_node_type,
    // Workspace params
    BootstrapWorkspaceParams,
    CallPathParams,
    CapsuleAckParams,
    CapsuleAuditParams,
    CapsuleChunkParams,
    CapsuleContextDocParams,
    CapsuleListSharesParams,
    CapsuleOpenParams,
    CapsulePrimerParams,
    CapsuleShareParams,
    CapsuleStreamParams,
    // Compliance types
    // Plan params
    CapturePlanParams,
    // VCS params
    CaptureVcsLocalEventParams,
    CheckoutRoutingScope,
    ContextParams,
    ContextStreamClient,
    // Doc params
    CreateCapsuleParams,
    // Diagram params
    CreateDiagramParams,
    CreateDocParams,
    CreateMemoryEventParams,
    // Memory params
    CreateMemoryNodeParams,
    CreateReminderParams,
    CreateRoadmapParams,
    // Skill params
    CreateSkillParams,
    // Task params
    CreateTaskParams,
    // Todo params
    CreateTodoParams,
    // Help params
    EditorRulesParams,
    ExportSkillParams,
    // Context Feed params
    FeedCreateParams,
    FeedFollowParams,
    FeedItemsParams,
    FeedListParams,
    FeedPostParams,
    FeedShareParams,
    FeedSourceParams,
    FeedUpdateParams,
    FlashAckParams,
    FlashBootstrapParams,
    FlashCheckpointParams,
    FlashClearParams,
    FlashGetParams,
    FlashPushEntry,
    FlashPushParams,
    FlashStatsParams,
    FlashVerifyParams,
    GraphDependenciesParams,
    GraphImpactParams,
    GraphRelatedParams,
    // Graph params
    GraphTarget,
    GraphTier,
    HookIngestOutcome,
    ImportMemoryEventsParams,
    ImportSkillParams,
    IndexSettingsParams,
    // Project params
    IngestLocalParams,
    IngestProgressEvent,
    IntegrationActivityParams,
    // Integration params
    IntegrationSearchParams,
    // Knowledge Stream params
    KnowledgeStreamSearchParams,
    ListCapsulesParams,
    // Reminder params
    ListRemindersParams,
    ListSuggestedRulesParams,
    ListTodosParams,
    MediaGetClipParams,
    // Media params
    MediaIndexParams,
    MediaSearchParams,
    MemorySearchParams,
    NotionCreateDatabaseParams,
    NotionCreatePageParams,
    NotionQueryDatabaseParams,
    NotionSearchPagesParams,
    NotionSort,
    NotionUpdatePageParams,
    PlanStep,
    // Q&A surface params + responses
    QaAnswerShape,
    QaAskParams,
    QaAskResult,
    QaCreateKbItemParams,
    QaFeedbackParams,
    QaFeedbackResult,
    QaKbItem,
    QaListKbParams,
    QaListKbResult,
    QaQuestionShape,
    QaSearchHit,
    QaSearchParams,
    QaSearchResult,
    QaUpdateKbItemParams,
    RequestOptions,
    RoadmapMilestone,
    RunSkillParams,
    SearchParams,
    // Transcript params
    SearchTranscriptsParams,
    SessionCaptureLessonParams,
    SessionCaptureParams,
    SessionCompressParams,
    SessionDecisionTraceParams,
    SessionDeltaParams,
    SessionGetLessonsParams,
    // Session params
    SessionInitParams,
    SessionRecallParams,
    SessionRememberParams,
    SessionRestoreContextParams,
    SessionSmartSearchParams,
    SessionSummaryParams,
    SessionUserContextParams,
    SuggestedRuleActionParams,
    SupersedeMemoryNodeParams,
    SyncBridgeCheckoutRegistration,
    SyncBridgeRefreshClaim,
    TargetedFileDecision,
    UpdateDiagramParams,
    UpdateDocParams,
    UpdateMemoryEventParams,
    UpdateMemoryNodeParams,
    UpdatePlanParams,
    UpdateSkillParams,
    UpdateTaskParams,
    UpdateTodoParams,
    FEED_CHANGES_MAX_PAGE_SIZE,
    FEED_GROUNDING_MAX_ITEMS,
    FEED_MAX_PAGE_SIZE,
    VALID_ENTITY_KINDS,
};
pub use client::{
    get_task_auth_override, get_task_caller_cache_identity, get_task_config_override,
    get_task_installation_id, get_task_mcp_session_id, get_task_model_id, get_task_session_key,
    run_with_auth_override, run_with_caller_cache_identity, run_with_config_override,
    run_with_installation_id, run_with_mcp_session_id, run_with_model_id, run_with_session_key,
    spawn_with_task_context, TaskContextSnapshot,
};
pub use ingest_guard::{
    broad_ingest_opt_in_from_env, validate_ingest_root, IngestRootAssessment, IngestRootOptions,
    IngestRootRejection, IngestRootRejectionReason, ALLOW_BROAD_INGEST_ENV, SENSITIVE_DIR_NAMES,
};
pub use mcp_types::agentic::{ComplianceEventRecorded, ComplianceEventRequest};
pub use mcp_types::answer::{
    AnswerContextScopeV1, AnswerFeedbackEffectV1, AnswerFeedbackRequestV1,
    AnswerFeedbackResponseV1, AnswerFeedbackSchemaVersionV1, AnswerFeedbackSignalV1,
    AnswerFeedbackStatusV1, AnswerFeedbackTargetV1, AnswerLatencyBudgetRequestV1,
    AnswerPlanClientContextV1, AnswerRequestV1, AnswerResponseBudgetRequestV1,
    AnswerResponseModeV1, AnswerResponseV1, AnswerScopeModeV1, AnswerVisibilityV1,
};
pub use parity::{
    normalize_decisions_envelope, CreateDecisionParams, CreateLessonParams, DecisionActionParams,
    ListDecisionsParams, ListLessonsParams, UpdateLessonParams, DECISION_ACTIONS,
};
pub use ticket::{
    append_ticket_extras, canonical_linked_item_kind, enrich_ticket_result_from_request,
    format_linked_summary, format_ticket_assignee_summary, format_ticket_linked_summary,
    normalize_assignees, normalize_linked_items_with_allowed_kinds, normalize_ticket_body,
    normalize_ticket_linked_items, summarize_ticket_linked_items, TicketLinkedSummary,
    ASSIGNEE_ENTITY_TYPES, LINKED_ITEM_KINDS, PLAN_LINKED_ITEM_KINDS,
};

/// Process-wide lock for tests that read or mutate environment variables
/// (notably `HOME`). Env is global to the test process, so any test that
/// reads `HOME` (e.g. `ingest_guard` home-rejection tests) must serialize
/// against tests that override it (e.g. `client` index-marker tests). All
/// such tests share this single mutex.
#[cfg(test)]
pub(crate) fn test_env_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}
