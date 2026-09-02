//! Shared types for the ContextStream MCP server.
//!
//! This crate provides common types used across all MCP server crates:
//! - Configuration types
//! - Error types
//! - Tool-related types
//! - API response types

pub mod acceleration_layer;
pub mod account_mode;
pub mod agentic;
pub mod answer;
pub mod api;
pub mod atlas_layer;
pub mod config;
pub mod error;
pub mod harness;
pub mod harness_teaching;
pub mod protocol;
pub mod rules_hash;
pub mod tool;

pub use acceleration_layer::{
    noop_acceleration_layer, AccelerationLayer, AnalyticsProvider, ArchiveProvider, JobProvider,
    McpAccelerationLayer, NoopAccelerationLayer, ScheduledJobProvider, SearchAccelerationProvider,
    SignalProvider, VectorAccelerationProvider, WarmCacheProvider,
};
pub use account_mode::{
    AccountContextSnapshot, AccountContextSource, AccountModePreference, ExecutionMode,
    TeamDiscussion, TeamPriorityItem, TranscriptTopicSignal,
};
pub use agentic::{ComplianceEventRecorded, ComplianceEventRequest, RUNTIME_CONTEXTSTREAM_MCP};
pub use answer::*;
pub use atlas_layer::{
    noop_layer, AtlasArchiveError, AtlasArchiveHit, AtlasArchiveProvider, AtlasArchiveScope,
    AtlasFederationError, AtlasFederationProvider, AtlasFederationScope, AtlasLayer,
    AtlasProductHealth, AtlasProductId, AtlasProductLayer, AtlasRemoteCapabilities,
    AtlasRemoteProductInfo, AtlasSearchCollection, AtlasSearchError, AtlasSearchHit,
    AtlasSearchProvider, AtlasSearchScope, AtlasStreamError, AtlasStreamEvent,
    AtlasStreamEventKind, AtlasStreamProvider, AtlasTriggerKind, AtlasTriggerSpec,
    AtlasTriggersError, AtlasTriggersProvider, AtlasVectorError, AtlasVectorFilter, AtlasVectorHit,
    AtlasVectorProvider, AtlasVectorScope, AtlasVectorWrite, AtlasWarmCacheKind, CachedBundle,
    FederatedHit, NoopAtlasLayer,
};
pub use config::{AuthOverride, Config, ConfigOverride, SessionKey, TrafficClass};
pub use error::{is_non_blocking_parser_error_message, Error, ErrorCode, Result};
pub use harness::{
    HarnessId, HarnessProfile, HarnessReadinessEvidence, HarnessReadinessStage, HookCapabilities,
    McpConfigFormat, McpTransportSupport, ReadinessEvidenceSource, ReadinessEvidenceStatus,
    RulesFormat, TeachingLoadEvidence, HARNESS_PROFILE_SCHEMA_VERSION,
    HARNESS_READINESS_SCHEMA_VERSION,
};
pub use harness_teaching::{
    build_harness_teaching, legacy_initialize_instructions, legacy_protocol_supports_instructions,
    stateless_discovery_teaching, HarnessTeachingBudget, HarnessTeachingCapabilities,
    HarnessTeachingContract, HarnessTeachingDelivery, HarnessTeachingStep, HarnessTeachingStepId,
    StatelessMcpConformance, HARNESS_TEACHING_SCHEMA_VERSION, HARNESS_TEACHING_VERSION,
    MCP_PROTOCOL_2024_11_05, MCP_PROTOCOL_2025_03_26, MCP_PROTOCOL_2025_06_18,
    MCP_PROTOCOL_2026_07_28,
};
pub use protocol::{
    build_stateless_discover_result, decorate_stateless_cacheable_result,
    decorate_stateless_result, has_stateless_protocol_metadata, stateless_protocol_version,
    validate_stateless_jsonrpc_envelope, validate_stateless_method_params,
    validate_stateless_request, McpCacheScope, McpProtocolError, StatelessRequestMetadata,
    MCP_DISCOVERY_TTL_MS, MCP_ERROR_HEADER_MISMATCH, MCP_ERROR_MISSING_REQUIRED_CLIENT_CAPABILITY,
    MCP_ERROR_UNSUPPORTED_PROTOCOL_VERSION, MCP_META_CLIENT_CAPABILITIES, MCP_META_CLIENT_INFO,
    MCP_META_PROTOCOL_VERSION, MCP_META_SERVER_INFO, MCP_STATELESS_SUPPORTED_VERSIONS,
    MCP_TOOLS_LIST_TTL_MS,
};
