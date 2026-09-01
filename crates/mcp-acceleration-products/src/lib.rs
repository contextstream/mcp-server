//! MongoDB-free acceleration providers for the remote MCP gateway.
//!
//! This crate is compiled only behind `mcp-server`'s
//! `remote-acceleration` feature. It must never depend on `mongodb` or
//! `bson`; providers call ContextStream server APIs backed by
//! Postgres, Redis, R2, Qdrant, Neo4j, and Cloudflare.

pub mod analytics;
pub mod archive;
pub mod jobs;
pub mod layer;
pub mod signals;
pub mod warm_cache;

pub use analytics::ContextStreamAnalyticsProvider;
pub use archive::ContextStreamArchiveProvider;
pub use jobs::ContextStreamJobProvider;
pub use layer::{AccelerationConfig, McpRemoteAccelerationLayer};
pub use signals::ContextStreamSignalProvider;
pub use warm_cache::{normalize_acceleration_api_url, ContextStreamWarmCacheProvider};

pub use mcp_types::acceleration_layer::{
    AccelerationAnalyticsChart, AccelerationAnalyticsError, AccelerationAnalyticsPoint,
    AccelerationAnalyticsRender, AccelerationAnalyticsRenderRequest, AccelerationAnalyticsScope,
    AccelerationAnalyticsSeries, AccelerationArchiveCollection, AccelerationArchiveError,
    AccelerationArchiveHit, AccelerationArchiveScope, AccelerationJobError, AccelerationJobHandle,
    AccelerationJobKind, AccelerationJobResultPage, AccelerationJobSpec, AccelerationJobState,
    AccelerationJobStatus, AccelerationLayer, AccelerationReadModelScope, AccelerationSignalError,
    AccelerationSignalEvent, AccelerationSignalKind, AnalyticsProvider, ArchiveProvider,
    JobProvider, McpAccelerationLayer, SignalProvider, WarmCacheError, WarmCacheHit,
    WarmCacheLayer, WarmCacheLookup, WarmCacheProvider, WarmCachePut, WarmCacheRebuild,
};
