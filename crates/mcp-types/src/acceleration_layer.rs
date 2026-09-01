//! MCP acceleration layer — MongoDB Atlas replacement abstraction.
//!
//! This is the forward-looking provider surface for the remote MCP gateway.
//! The existing `atlas_layer` module remains as a compatibility shim while
//! call sites migrate provider-by-provider.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

pub type AccelerationLayer = Arc<dyn McpAccelerationLayer>;

pub trait McpAccelerationLayer: Send + Sync {
    fn is_enabled(&self) -> bool {
        false
    }

    fn has_connection(&self) -> bool {
        false
    }

    fn search(&self) -> Option<Arc<dyn SearchAccelerationProvider>> {
        None
    }

    fn vector(&self) -> Option<Arc<dyn VectorAccelerationProvider>> {
        None
    }

    fn signals(&self) -> Option<Arc<dyn SignalProvider>> {
        None
    }

    fn scheduled_jobs(&self) -> Option<Arc<dyn ScheduledJobProvider>> {
        None
    }

    fn archive(&self) -> Option<Arc<dyn ArchiveProvider>> {
        None
    }

    fn warm_cache(&self) -> Option<Arc<dyn WarmCacheProvider>> {
        None
    }

    fn analytics(&self) -> Option<Arc<dyn AnalyticsProvider>> {
        None
    }

    fn jobs(&self) -> Option<Arc<dyn JobProvider>> {
        None
    }
}

pub trait SearchAccelerationProvider: Send + Sync {
    fn provider_name(&self) -> &'static str {
        "search"
    }
}

pub trait VectorAccelerationProvider: Send + Sync {
    fn provider_name(&self) -> &'static str {
        "vector"
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccelerationSignalKind {
    FileChanged,
    ToolCall,
    ContextRequested,
    ContextPrewarmCandidate,
    MemoryEventSeen,
    CacheHitMiss,
    LatencySample,
    DegradedProvider,
}

impl AccelerationSignalKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::FileChanged => "file_changed",
            Self::ToolCall => "tool_call",
            Self::ContextRequested => "context_requested",
            Self::ContextPrewarmCandidate => "context_prewarm_candidate",
            Self::MemoryEventSeen => "memory_event_seen",
            Self::CacheHitMiss => "cache_hit_miss",
            Self::LatencySample => "latency_sample",
            Self::DegradedProvider => "degraded_provider",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AccelerationSignalEvent {
    pub kind: AccelerationSignalKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_hit: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub degraded: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default)]
    pub metadata: Value,
    pub emitted_at: DateTime<Utc>,
}

impl AccelerationSignalEvent {
    pub fn new(kind: AccelerationSignalKind) -> Self {
        Self {
            kind,
            tenant_id: None,
            workspace_id: None,
            project_id: None,
            tool: None,
            action: None,
            cache_hit: None,
            provider: None,
            latency_ms: None,
            degraded: None,
            generation: None,
            request_id: None,
            metadata: Value::Null,
            emitted_at: Utc::now(),
        }
    }

    pub fn with_scope(
        kind: AccelerationSignalKind,
        workspace_id: Uuid,
        project_id: Option<Uuid>,
        metadata: Value,
    ) -> Self {
        let mut event = Self::new(kind);
        event.workspace_id = Some(workspace_id);
        event.project_id = project_id;
        event.metadata = metadata;
        event
    }
}

#[derive(Debug, Error)]
pub enum AccelerationSignalError {
    #[error("signal provider is unavailable: {0}")]
    Unavailable(String),
    #[error("signal request failed: {0}")]
    Request(String),
    #[error("signal response decode failed: {0}")]
    Decode(String),
}

#[async_trait]
pub trait SignalProvider: Send + Sync {
    fn provider_name(&self) -> &'static str {
        "signals"
    }

    async fn emit(&self, event: AccelerationSignalEvent) -> Result<(), AccelerationSignalError>;

    async fn emit_payload(
        &self,
        kind: AccelerationSignalKind,
        workspace_id: Uuid,
        project_id: Option<Uuid>,
        metadata: Value,
    ) -> Result<(), AccelerationSignalError> {
        self.emit(AccelerationSignalEvent::with_scope(
            kind,
            workspace_id,
            project_id,
            metadata,
        ))
        .await
    }
}

pub trait ScheduledJobProvider: Send + Sync {
    fn provider_name(&self) -> &'static str {
        "scheduled_jobs"
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccelerationArchiveCollection {
    Transcripts,
    Decisions,
    Lessons,
    Docs,
    QaQuestions,
    QaAnswers,
    QaKbItems,
}

impl AccelerationArchiveCollection {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Transcripts => "transcripts",
            Self::Decisions => "decisions",
            Self::Lessons => "lessons",
            Self::Docs => "docs",
            Self::QaQuestions => "qa_questions",
            Self::QaAnswers => "qa_answers",
            Self::QaKbItems => "qa_kb_items",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccelerationArchiveScope {
    pub tenant_id: Option<Uuid>,
    pub workspace_id: Uuid,
    pub project_id: Option<Uuid>,
    pub collection: Option<AccelerationArchiveCollection>,
    pub archived_after: Option<DateTime<Utc>>,
}

impl AccelerationArchiveScope {
    pub fn new(workspace_id: Uuid) -> Self {
        Self {
            tenant_id: None,
            workspace_id,
            project_id: None,
            collection: None,
            archived_after: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AccelerationArchiveHit {
    pub id: String,
    pub subject_id: Option<String>,
    pub collection: AccelerationArchiveCollection,
    pub title: Option<String>,
    pub snippet: String,
    pub archived_at: Option<DateTime<Utc>>,
    pub score: Option<f64>,
    pub degraded: bool,
    pub note: Option<String>,
}

#[derive(Debug, Error)]
pub enum AccelerationArchiveError {
    #[error("archive provider is unavailable: {0}")]
    Unavailable(String),
    #[error("archive request failed: {0}")]
    Request(String),
    #[error("archive response decode failed: {0}")]
    Decode(String),
}

#[async_trait]
pub trait ArchiveProvider: Send + Sync {
    fn provider_name(&self) -> &'static str {
        "archive"
    }

    async fn search_archive(
        &self,
        query: &str,
        scope: &AccelerationArchiveScope,
        limit: usize,
    ) -> Result<Vec<AccelerationArchiveHit>, AccelerationArchiveError>;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AccelerationReadModelScope {
    pub tenant_id: Option<Uuid>,
    pub workspace_id: Option<Uuid>,
    pub project_id: Option<Uuid>,
    pub scope_type: String,
    pub scope_id: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WarmCacheLookup {
    pub scope: AccelerationReadModelScope,
    pub model: String,
    pub cache_key: String,
    pub stale_ok: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WarmCacheLayer {
    Redis,
    Postgres,
    R2,
    StalePostgres,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WarmCacheHit {
    pub payload: Value,
    pub served_from: WarmCacheLayer,
    pub cache_hit: bool,
    pub stale: bool,
    pub generation: Option<i64>,
    pub source_generation: Option<i64>,
    pub etag: Option<String>,
    pub freshness: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WarmCacheRebuild {
    pub scope: AccelerationReadModelScope,
    pub model: String,
    pub cache_key: Option<String>,
    pub reason: String,
    pub target_generation: i64,
    pub job_kind: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WarmCachePut {
    pub scope: AccelerationReadModelScope,
    pub model: String,
    pub cache_key: String,
    pub generation: Option<i64>,
    pub source_generation: i64,
    pub payload: Value,
    pub etag: Option<String>,
    pub expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Error)]
pub enum WarmCacheError {
    #[error("warm cache provider is unavailable: {0}")]
    Unavailable(String),
    #[error("warm cache request failed: {0}")]
    Request(String),
    #[error("warm cache response decode failed: {0}")]
    Decode(String),
}

#[async_trait]
pub trait WarmCacheProvider: Send + Sync {
    fn provider_name(&self) -> &'static str {
        "warm_cache"
    }

    async fn get_read_model(
        &self,
        lookup: WarmCacheLookup,
    ) -> Result<Option<WarmCacheHit>, WarmCacheError>;

    async fn enqueue_rebuild(&self, rebuild: WarmCacheRebuild) -> Result<(), WarmCacheError> {
        let _ = rebuild;
        Ok(())
    }

    async fn put_read_model(&self, put: WarmCachePut) -> Result<(), WarmCacheError> {
        let _ = put;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AccelerationAnalyticsChart {
    pub chart_key: String,
    pub title: String,
    pub description: Option<String>,
    pub metric: String,
    pub allowed_dimensions: Value,
    pub default_range: String,
    pub default_granularity: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AccelerationAnalyticsScope {
    pub tenant_id: Option<Uuid>,
    pub workspace_id: Uuid,
    pub project_id: Option<Uuid>,
}

impl AccelerationAnalyticsScope {
    pub fn new(workspace_id: Uuid) -> Self {
        Self {
            tenant_id: None,
            workspace_id,
            project_id: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AccelerationAnalyticsRenderRequest {
    pub scope: AccelerationAnalyticsScope,
    pub chart_key: String,
    pub range: Option<String>,
    pub granularity: Option<String>,
    pub filters: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AccelerationAnalyticsPoint {
    pub t: DateTime<Utc>,
    pub value: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AccelerationAnalyticsSeries {
    pub name: String,
    pub points: Vec<AccelerationAnalyticsPoint>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AccelerationAnalyticsRender {
    pub chart_key: String,
    pub title: String,
    pub range: String,
    pub granularity: String,
    pub source: String,
    pub series: Vec<AccelerationAnalyticsSeries>,
    pub generated_at: DateTime<Utc>,
    pub degraded: bool,
    pub note: Option<String>,
}

#[derive(Debug, Error)]
pub enum AccelerationAnalyticsError {
    #[error("analytics provider is unavailable: {0}")]
    Unavailable(String),
    #[error("analytics request failed: {0}")]
    Request(String),
    #[error("analytics response decode failed: {0}")]
    Decode(String),
}

#[async_trait]
pub trait AnalyticsProvider: Send + Sync {
    fn provider_name(&self) -> &'static str {
        "analytics"
    }

    async fn list_charts(
        &self,
    ) -> Result<Vec<AccelerationAnalyticsChart>, AccelerationAnalyticsError>;

    async fn render_chart(
        &self,
        request: AccelerationAnalyticsRenderRequest,
    ) -> Result<AccelerationAnalyticsRender, AccelerationAnalyticsError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccelerationJobKind {
    MemoryExport,
    MemoryAggregate,
}

impl AccelerationJobKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::MemoryExport => "memory_export",
            Self::MemoryAggregate => "memory_aggregate",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AccelerationJobSpec {
    pub kind: AccelerationJobKind,
    pub workspace_id: Uuid,
    pub project_id: Option<Uuid>,
    pub collection: String,
    pub filter: Value,
    pub options: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AccelerationJobHandle {
    pub job_id: String,
    pub kind: AccelerationJobKind,
    pub submitted_at: DateTime<Utc>,
    pub estimated_total: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccelerationJobStatus {
    Pending,
    Running,
    Completed,
    Failed,
}

impl AccelerationJobStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "pending" | "queued" => Some(Self::Pending),
            "running" => Some(Self::Running),
            "completed" | "complete" => Some(Self::Completed),
            "failed" | "cancelled" | "canceled" => Some(Self::Failed),
            _ => None,
        }
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AccelerationJobState {
    pub job_id: String,
    pub kind: AccelerationJobKind,
    pub status: AccelerationJobStatus,
    pub progress: Option<f64>,
    pub record_count: Option<u64>,
    pub submitted_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub error: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AccelerationJobResultPage {
    pub job_id: String,
    pub seq_start: u64,
    pub records: Vec<Value>,
    pub has_more: bool,
}

#[derive(Debug, Error)]
pub enum AccelerationJobError {
    #[error("job provider is unavailable: {0}")]
    Unavailable(String),
    #[error("job request failed: {0}")]
    Request(String),
    #[error("job response decode failed: {0}")]
    Decode(String),
    #[error("job not found: {0}")]
    JobNotFound(String),
}

#[async_trait]
pub trait JobProvider: Send + Sync {
    fn provider_name(&self) -> &'static str {
        "jobs"
    }

    async fn submit_job(
        &self,
        spec: AccelerationJobSpec,
    ) -> Result<AccelerationJobHandle, AccelerationJobError>;

    async fn poll_job(&self, job_id: &str) -> Result<AccelerationJobState, AccelerationJobError>;

    async fn fetch_result_page(
        &self,
        job_id: &str,
        cursor: Option<u64>,
        limit: usize,
    ) -> Result<AccelerationJobResultPage, AccelerationJobError>;
}

#[derive(Debug, Default)]
pub struct NoopAccelerationLayer;

impl McpAccelerationLayer for NoopAccelerationLayer {}

pub fn noop_acceleration_layer() -> AccelerationLayer {
    Arc::new(NoopAccelerationLayer)
}
