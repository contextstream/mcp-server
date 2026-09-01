//! Async jobs MCP tool — formerly Atlas Functions backed.
//!
//! Exposes `async_job` plus the deprecated `atlas_job` compatibility
//! alias so agents can submit, poll, and page long-running memory
//! export / aggregate jobs. The preferred path is the remote
//! acceleration job provider backed by Postgres `mcp_jobs`; Atlas
//! Functions remain a temporary canary fallback when explicitly
//! configured. The local stdio binary's no-op layers skip registration.
//!
//! Actions:
//! - `submit_export` — submit a memory export job and return `{job_id}`
//!   immediately; worker execution is backed by the provider.
//! - `submit_aggregate` — submit a memory aggregate job (same pattern,
//!   returns counts bucketed by hour/day/week/month).
//! - `poll` — read the current status / progress of a submitted job.
//! - `result` — page through completed job's records.

use async_trait::async_trait;
use mcp_session::SessionManager;
use mcp_types::acceleration_layer::{
    AccelerationJobKind, AccelerationJobSpec, AccelerationJobStatus, AccelerationLayer,
};
use mcp_types::atlas_layer::{
    AtlasJobKind, AtlasJobSpec, AtlasJobStatus, AtlasLayer, AtlasSearchCollection,
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

/// Default page size for `result` action. Capped at MongoDB's 200/page
/// limit on the provider side.
const DEFAULT_PAGE_SIZE: usize = 50;

/// Input to the `atlas_job` MCP tool. Action-discriminated; not all
/// fields are meaningful for every action.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtlasJobInput {
    pub action: String,
    /// `submit_*`: source collection (transcripts/decisions/lessons/docs).
    #[serde(default)]
    pub collection: Option<String>,
    /// `submit_*`: optional MongoDB-shaped filter, e.g.
    /// `{"updated_at": {"$gte": "2026-01-01"}}`.
    #[serde(default)]
    pub filter: Option<Value>,
    /// `submit_export`: `{"include_content": true}` to ship full
    /// content. `submit_aggregate`: `{"bucket": "day"|"hour"|...}`.
    #[serde(default)]
    pub options: Option<Value>,
    /// `poll` / `result`: which job to inspect.
    #[serde(default)]
    pub job_id: Option<String>,
    /// `result`: pagination cursor (start `seq`).
    #[serde(default)]
    pub cursor: Option<u64>,
    /// `result`: max records per page.
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
}

pub struct AtlasJobTool {
    atlas_layer: AtlasLayer,
    acceleration_layer: AccelerationLayer,
    session: Arc<SessionManager>,
    metadata: ToolMetadata,
}

impl AtlasJobTool {
    pub fn new(atlas_layer: AtlasLayer, session: Arc<SessionManager>) -> Self {
        Self::with_acceleration(
            atlas_layer,
            mcp_types::acceleration_layer::noop_acceleration_layer(),
            session,
        )
    }

    pub fn with_acceleration(
        atlas_layer: AtlasLayer,
        acceleration_layer: AccelerationLayer,
        session: Arc<SessionManager>,
    ) -> Self {
        let metadata = ToolMetadata {
            name: "async_job".to_string(),
            title: "Async export jobs".to_string(),
            description: "Submit, poll, and page results from long-running async jobs over the \
                 caller's memory data (transcripts / decisions / lessons / docs). Use \
                 when a synchronous list / aggregate would time out or return more rows \
                 than the caller can stream. Actions: submit_export, submit_aggregate, \
                 poll, result. Submit returns a `job_id` immediately; poll reports \
                 status / progress; result pages records once status=completed. \
                 Available on hosted/remote deployments only."
                .to_string(),
            category: ToolCategory::Utility,
            // submit_export and submit_aggregate create durable async jobs.
            annotations: ToolAnnotations::write().long_running(),
            is_pro: true,
            required_tier: Some("pro".to_string()),
        };
        Self {
            atlas_layer,
            acceleration_layer,
            session,
            metadata,
        }
    }

    async fn submit_action(&self, input: AtlasJobInput, kind: AtlasJobKind) -> Result<ToolResult> {
        let acceleration_provider = self.acceleration_layer.jobs();
        let atlas_provider = self.atlas_layer.functions();
        if acceleration_provider.is_none() && atlas_provider.is_none() {
            return Ok(unavailable(&self.atlas_layer, &self.acceleration_layer));
        }

        let workspace_id = self
            .resolve_workspace_id(input.workspace_id.as_deref())
            .await
            .ok_or_else(|| {
                Error::Validation("async_job: submit requires a resolved workspace_id".to_string())
            })?;

        let collection = parse_collection(input.collection.as_deref(), kind)?;
        let filter = input.filter.unwrap_or(Value::Null);
        let options = input.options.unwrap_or(Value::Null);
        let project_id = parse_optional_project_id(input.project_id.as_deref())?;

        if let Some(provider) = acceleration_provider {
            let started = std::time::Instant::now();
            let handle = match provider
                .submit_job(AccelerationJobSpec {
                    kind: acceleration_job_kind(kind),
                    workspace_id,
                    project_id,
                    collection: collection.as_str().to_string(),
                    filter: filter.clone(),
                    options: options.clone(),
                })
                .await
            {
                Ok(handle) => handle,
                Err(error) => {
                    tracing::warn!(error = %error, "acceleration-jobs: submit_job failed");
                    return Ok(ToolResult::with_structured(
                        format!("[ACCELERATION_JOB] submit failed: {}", error),
                        serde_json::json!({
                            "stages_used": ["acceleration_jobs"],
                            "available": true,
                            "error": error.to_string(),
                        }),
                    ));
                }
            };
            let elapsed_ms = started.elapsed().as_millis() as u64;
            return Ok(ToolResult::with_structured(
                format!(
                    "[ACCELERATION_JOB] submitted {} job `{}` ({}ms). Poll with \
                     async_job(action=\"poll\", job_id=\"{}\").",
                    handle.kind.as_str(),
                    handle.job_id,
                    elapsed_ms,
                    handle.job_id
                ),
                serde_json::json!({
                    "stages_used": ["acceleration_jobs"],
                    "available": true,
                    "job_id": handle.job_id,
                    "kind": handle.kind.as_str(),
                    "submitted_at": handle.submitted_at.to_rfc3339(),
                    "estimated_total": handle.estimated_total,
                    "elapsed_ms": elapsed_ms,
                    "origin": "acceleration_jobs",
                    "marker": "[ACCELERATION_JOB]",
                }),
            ));
        }

        let provider = match atlas_provider {
            Some(provider) => provider,
            None => return Ok(unavailable(&self.atlas_layer, &self.acceleration_layer)),
        };

        let spec = AtlasJobSpec {
            kind,
            workspace_id,
            project_id,
            collection,
            filter,
            options,
        };

        let started = std::time::Instant::now();
        let handle = match provider.submit_job(spec).await {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(error = %e, "atlas-functions: submit_job failed");
                return Ok(ToolResult::with_structured(
                    format!("[JOB] submit failed: {}", e),
                    serde_json::json!({
                        "stages_used": ["async_jobs"],
                        "available": true,
                        "error": e.to_string(),
                    }),
                ));
            }
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;

        Ok(ToolResult::with_structured(
            format!(
                "[JOB] submitted {} job `{}` ({}ms). Poll with \
                 async_job(action=\"poll\", job_id=\"{}\").",
                handle.kind.as_str(),
                handle.job_id,
                elapsed_ms,
                handle.job_id
            ),
            serde_json::json!({
                "stages_used": ["async_jobs"],
                "available": true,
                "job_id": handle.job_id,
                "kind": handle.kind.as_str(),
                "submitted_at": handle.submitted_at.to_rfc3339(),
                "elapsed_ms": elapsed_ms,
                "origin": "async_jobs",
            }),
        ))
    }

    async fn poll_action(&self, input: AtlasJobInput) -> Result<ToolResult> {
        let job_id = input
            .job_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                Error::Validation("async_job(action=\"poll\") requires `job_id`".to_string())
            })?;

        if should_use_acceleration_job(job_id, &self.acceleration_layer, &self.atlas_layer) {
            return self.poll_acceleration_job(job_id).await;
        }

        let provider = match self.atlas_layer.functions() {
            Some(p) => p,
            None => return Ok(unavailable(&self.atlas_layer, &self.acceleration_layer)),
        };

        let started = std::time::Instant::now();
        let state = match provider.poll_job(job_id).await {
            Ok(s) => s,
            Err(e) => {
                return Ok(ToolResult::with_structured(
                    format!("[JOB] poll failed: {}", e),
                    serde_json::json!({
                        "stages_used": ["async_jobs"],
                        "available": true,
                        "error": e.to_string(),
                    }),
                ));
            }
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let progress_str = state
            .progress
            .map(|p| format!(" {:.1}%", (p * 100.0).clamp(0.0, 100.0)))
            .unwrap_or_default();
        let header = format!(
            "[JOB] job `{}` is {}{} ({}ms)",
            state.job_id,
            state.status.as_str(),
            progress_str,
            elapsed_ms
        );

        Ok(ToolResult::with_structured(
            header,
            serde_json::json!({
                "stages_used": ["async_jobs"],
                "available": true,
                "job_id": state.job_id,
                "kind": state.kind.as_str(),
                "status": state.status.as_str(),
                "is_terminal": state.status.is_terminal(),
                "progress": state.progress,
                "record_count": state.record_count,
                "submitted_at": state.submitted_at.to_rfc3339(),
                "started_at": state.started_at.map(|t| t.to_rfc3339()),
                "completed_at": state.completed_at.map(|t| t.to_rfc3339()),
                "error": state.error,
                "elapsed_ms": elapsed_ms,
                "origin": "async_jobs",
            }),
        ))
    }

    async fn result_action(&self, input: AtlasJobInput) -> Result<ToolResult> {
        let job_id = input
            .job_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                Error::Validation("async_job(action=\"result\") requires `job_id`".to_string())
            })?;

        if should_use_acceleration_job(job_id, &self.acceleration_layer, &self.atlas_layer) {
            return self
                .result_acceleration_job(job_id, input.cursor, input.limit)
                .await;
        }

        let provider = match self.atlas_layer.functions() {
            Some(p) => p,
            None => return Ok(unavailable(&self.atlas_layer, &self.acceleration_layer)),
        };

        // Surface a clear hint when the job isn't done yet — saves
        // agents a wasted page through empty results.
        let state = match provider.poll_job(job_id).await {
            Ok(s) => s,
            Err(e) => {
                return Ok(ToolResult::with_structured(
                    format!("[JOB] result failed: {}", e),
                    serde_json::json!({
                        "stages_used": ["async_jobs"],
                        "available": true,
                        "error": e.to_string(),
                    }),
                ));
            }
        };
        if !state.status.is_terminal() {
            return Ok(ToolResult::with_structured(
                format!(
                    "[JOB] job `{}` not yet complete (status: {}). \
                     Poll again in a few seconds.",
                    state.job_id,
                    state.status.as_str()
                ),
                serde_json::json!({
                    "stages_used": ["async_jobs"],
                    "available": true,
                    "job_id": state.job_id,
                    "status": state.status.as_str(),
                    "is_terminal": false,
                    "progress": state.progress,
                    "results": [],
                }),
            ));
        }
        if state.status == AtlasJobStatus::Failed {
            return Ok(ToolResult::with_structured(
                format!(
                    "[JOB] job `{}` failed: {}",
                    state.job_id,
                    state.error.as_deref().unwrap_or("(no error message)")
                ),
                serde_json::json!({
                    "stages_used": ["async_jobs"],
                    "available": true,
                    "job_id": state.job_id,
                    "status": "failed",
                    "is_terminal": true,
                    "error": state.error,
                    "results": [],
                }),
            ));
        }

        let limit = input.limit.unwrap_or(DEFAULT_PAGE_SIZE);
        let started = std::time::Instant::now();
        let page = match provider
            .fetch_result_page(job_id, input.cursor, limit)
            .await
        {
            Ok(p) => p,
            Err(e) => {
                return Ok(ToolResult::with_structured(
                    format!("[JOB] result fetch failed: {}", e),
                    serde_json::json!({
                        "stages_used": ["async_jobs"],
                        "available": true,
                        "error": e.to_string(),
                    }),
                ));
            }
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let next_cursor = if page.has_more {
            Some(page.seq_start + page.records.len() as u64)
        } else {
            None
        };

        Ok(ToolResult::with_structured(
            format!(
                "[JOB] page seq {}-{} ({} records, {}{}{}ms)",
                page.seq_start,
                page.seq_start + page.records.len() as u64,
                page.records.len(),
                if page.has_more { "more pages, " } else { "" },
                if let Some(n) = next_cursor {
                    format!("next cursor={}, ", n)
                } else {
                    String::new()
                },
                elapsed_ms
            ),
            serde_json::json!({
                "stages_used": ["async_jobs"],
                "available": true,
                "job_id": page.job_id,
                "status": "completed",
                "is_terminal": true,
                "seq_start": page.seq_start,
                "records": page.records,
                "has_more": page.has_more,
                "next_cursor": next_cursor,
                "elapsed_ms": elapsed_ms,
                "origin": "async_jobs",
            }),
        ))
    }

    async fn resolve_workspace_id(&self, raw: Option<&str>) -> Option<Uuid> {
        if let Some(s) = raw.map(str::trim).filter(|s| !s.is_empty()) {
            if let Ok(parsed) = Uuid::parse_str(s) {
                return Some(parsed);
            }
        }
        let state = self.session.state().await;
        state.workspace_id
    }

    async fn poll_acceleration_job(&self, job_id: &str) -> Result<ToolResult> {
        let provider = match self.acceleration_layer.jobs() {
            Some(provider) => provider,
            None => return Ok(unavailable(&self.atlas_layer, &self.acceleration_layer)),
        };

        let started = std::time::Instant::now();
        let state = match provider.poll_job(job_id).await {
            Ok(state) => state,
            Err(error) => {
                return Ok(ToolResult::with_structured(
                    format!("[ACCELERATION_JOB] poll failed: {}", error),
                    serde_json::json!({
                        "stages_used": ["acceleration_jobs"],
                        "available": true,
                        "error": error.to_string(),
                    }),
                ));
            }
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let progress_str = state
            .progress
            .map(|p| format!(" {:.1}%", (p * 100.0).clamp(0.0, 100.0)))
            .unwrap_or_default();

        Ok(ToolResult::with_structured(
            format!(
                "[ACCELERATION_JOB] job `{}` is {}{} ({}ms)",
                state.job_id,
                state.status.as_str(),
                progress_str,
                elapsed_ms
            ),
            serde_json::json!({
                "stages_used": ["acceleration_jobs"],
                "available": true,
                "job_id": state.job_id,
                "kind": state.kind.as_str(),
                "status": state.status.as_str(),
                "is_terminal": state.status.is_terminal(),
                "progress": state.progress,
                "record_count": state.record_count,
                "submitted_at": state.submitted_at.to_rfc3339(),
                "started_at": state.started_at.map(|t| t.to_rfc3339()),
                "completed_at": state.completed_at.map(|t| t.to_rfc3339()),
                "error": state.error,
                "elapsed_ms": elapsed_ms,
                "origin": "acceleration_jobs",
                "marker": "[ACCELERATION_JOB]",
            }),
        ))
    }

    async fn result_acceleration_job(
        &self,
        job_id: &str,
        cursor: Option<u64>,
        limit: Option<usize>,
    ) -> Result<ToolResult> {
        let provider = match self.acceleration_layer.jobs() {
            Some(provider) => provider,
            None => return Ok(unavailable(&self.atlas_layer, &self.acceleration_layer)),
        };

        let state = match provider.poll_job(job_id).await {
            Ok(state) => state,
            Err(error) => {
                return Ok(ToolResult::with_structured(
                    format!("[ACCELERATION_JOB] result failed: {}", error),
                    serde_json::json!({
                        "stages_used": ["acceleration_jobs"],
                        "available": true,
                        "error": error.to_string(),
                    }),
                ));
            }
        };
        if !state.status.is_terminal() {
            return Ok(ToolResult::with_structured(
                format!(
                    "[ACCELERATION_JOB] job `{}` not yet complete (status: {}). Poll again in a few seconds.",
                    state.job_id,
                    state.status.as_str()
                ),
                serde_json::json!({
                    "stages_used": ["acceleration_jobs"],
                    "available": true,
                    "job_id": state.job_id,
                    "status": state.status.as_str(),
                    "is_terminal": false,
                    "progress": state.progress,
                    "records": [],
                    "marker": "[ACCELERATION_JOB]",
                }),
            ));
        }
        if state.status == AccelerationJobStatus::Failed {
            return Ok(ToolResult::with_structured(
                format!(
                    "[ACCELERATION_JOB] job `{}` failed: {}",
                    state.job_id,
                    state
                        .error
                        .as_ref()
                        .map(|value| value.to_string())
                        .unwrap_or_else(|| "(no error message)".to_string())
                ),
                serde_json::json!({
                    "stages_used": ["acceleration_jobs"],
                    "available": true,
                    "job_id": state.job_id,
                    "status": "failed",
                    "is_terminal": true,
                    "error": state.error,
                    "records": [],
                    "marker": "[ACCELERATION_JOB]",
                }),
            ));
        }

        let limit = limit.unwrap_or(DEFAULT_PAGE_SIZE);
        let started = std::time::Instant::now();
        let page = match provider.fetch_result_page(job_id, cursor, limit).await {
            Ok(page) => page,
            Err(error) => {
                return Ok(ToolResult::with_structured(
                    format!("[ACCELERATION_JOB] result fetch failed: {}", error),
                    serde_json::json!({
                        "stages_used": ["acceleration_jobs"],
                        "available": true,
                        "error": error.to_string(),
                    }),
                ));
            }
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let next_cursor = if page.has_more {
            Some(page.seq_start + page.records.len() as u64)
        } else {
            None
        };

        Ok(ToolResult::with_structured(
            format!(
                "[ACCELERATION_JOB] page seq {}-{} ({} records, {}{}{}ms)",
                page.seq_start,
                page.seq_start + page.records.len() as u64,
                page.records.len(),
                if page.has_more { "more pages, " } else { "" },
                if let Some(n) = next_cursor {
                    format!("next cursor={}, ", n)
                } else {
                    String::new()
                },
                elapsed_ms
            ),
            serde_json::json!({
                "stages_used": ["acceleration_jobs"],
                "available": true,
                "job_id": page.job_id,
                "status": "completed",
                "is_terminal": true,
                "seq_start": page.seq_start,
                "records": page.records,
                "has_more": page.has_more,
                "next_cursor": next_cursor,
                "elapsed_ms": elapsed_ms,
                "origin": "acceleration_jobs",
                "marker": "[ACCELERATION_JOB]",
            }),
        ))
    }
}

fn parse_collection(raw: Option<&str>, kind: AtlasJobKind) -> Result<AtlasSearchCollection> {
    let raw = raw
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            Error::Validation(format!(
                "async_job({}) requires `collection` (one of: transcripts, decisions, \
                 lessons, docs)",
                match kind {
                    AtlasJobKind::MemoryExport => "submit_export",
                    AtlasJobKind::MemoryAggregate => "submit_aggregate",
                }
            ))
        })?;
    match raw.to_ascii_lowercase().as_str() {
        "transcripts" => Ok(AtlasSearchCollection::Transcripts),
        "decisions" => Ok(AtlasSearchCollection::Decisions),
        "lessons" => Ok(AtlasSearchCollection::Lessons),
        "docs" => Ok(AtlasSearchCollection::Docs),
        "qa_questions" => Ok(AtlasSearchCollection::QaQuestions),
        "qa_answers" => Ok(AtlasSearchCollection::QaAnswers),
        "qa_kb_items" => Ok(AtlasSearchCollection::QaKbItems),
        other => Err(Error::Validation(format!(
            "async_job: unknown collection `{}`. Valid: transcripts, decisions, \
             lessons, docs, qa_questions, qa_answers, qa_kb_items",
            other
        ))),
    }
}

/// Standard degraded response when the layer can't satisfy the call.
fn unavailable(layer: &AtlasLayer, acceleration_layer: &AccelerationLayer) -> ToolResult {
    let note = if acceleration_layer.is_enabled() {
        "[ACCELERATION_JOB] disabled (job provider unavailable for this deployment)"
    } else if layer.is_enabled() {
        "[JOB] disabled (async job provider unavailable for this deployment)"
    } else {
        "[ACCELERATION_JOB] disabled (this deployment does not include async export jobs; \
         only available on hosted/remote deployments)"
    };
    ToolResult::with_structured(
        note,
        serde_json::json!({
            "stages_used": ["acceleration_jobs"],
            "available": false,
            "results": [],
            "marker": "[ACCELERATION_JOB]",
        }),
    )
}

fn parse_optional_project_id(raw: Option<&str>) -> Result<Option<Uuid>> {
    match raw.map(str::trim).filter(|value| !value.is_empty()) {
        Some(project) => Uuid::parse_str(project)
            .map(Some)
            .map_err(|_| Error::Validation(format!("async_job: invalid project_id `{project}`"))),
        None => Ok(None),
    }
}

fn acceleration_job_kind(kind: AtlasJobKind) -> AccelerationJobKind {
    match kind {
        AtlasJobKind::MemoryExport => AccelerationJobKind::MemoryExport,
        AtlasJobKind::MemoryAggregate => AccelerationJobKind::MemoryAggregate,
    }
}

fn should_use_acceleration_job(
    job_id: &str,
    acceleration_layer: &AccelerationLayer,
    atlas_layer: &AtlasLayer,
) -> bool {
    if acceleration_layer.jobs().is_none() {
        return false;
    }
    Uuid::parse_str(job_id).is_ok() || atlas_layer.functions().is_none()
}

#[async_trait]
impl ToolHandler for AtlasJobTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let parsed: AtlasJobInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;

        match parsed.action.as_str() {
            "submit_export" => self.submit_action(parsed, AtlasJobKind::MemoryExport).await,
            "submit_aggregate" => {
                self.submit_action(parsed, AtlasJobKind::MemoryAggregate)
                    .await
            }
            "poll" => self.poll_action(parsed).await,
            "result" => self.result_action(parsed).await,
            other => Err(Error::Validation(format!(
                "async_job: unknown action `{}`. Valid: submit_export, \
                 submit_aggregate, poll, result",
                other
            ))),
        }
    }

    fn metadata(&self) -> &ToolMetadata {
        &self.metadata
    }

    fn input_schema(&self) -> Value {
        SchemaBuilder::new()
            .description(
                "Submit, poll, and page long-running async export / aggregate jobs over \
                 the caller's memory data.",
            )
            .string(
                "action",
                "Required. One of: submit_export, submit_aggregate, poll, result.",
                true,
            )
            .string(
                "collection",
                "submit_*: source collection (transcripts | decisions | lessons | docs).",
                false,
            )
            .object(
                "filter",
                "submit_*: optional record filter (JSON object with field/op/value \
                 conditions; e.g. {\"updated_at\": {\"$gte\": \"2026-01-01\"}}).",
                false,
            )
            .object(
                "options",
                "submit_export: {include_content?: bool}. submit_aggregate: \
                 {bucket: hour|day|week|month}.",
                false,
            )
            .string("job_id", "poll/result: job_id returned by submit.", false)
            .integer("cursor", "result: starting `seq` for pagination.", false)
            .integer("limit", "result: page size (default 50, max 200).", false)
            .uuid(
                "workspace_id",
                "Workspace ID (falls back to session state)",
                false,
            )
            .uuid("project_id", "Optional project filter (submit only)", false)
            .build()
    }
}

/// Register the job tools when the binary has either the new
/// acceleration job provider or the temporary Atlas Functions fallback.
/// Per-call behavior prefers acceleration jobs and degrades in-band
/// when no provider is available.
pub fn register_atlas_job_tools(
    registry: &mut crate::registry::ToolRegistry,
    session: Arc<SessionManager>,
) {
    let atlas_layer = registry.atlas_layer().clone();
    let acceleration_layer = registry.acceleration_layer().clone();
    if !atlas_layer.has_connection() && !acceleration_layer.has_connection() {
        tracing::debug!(
            "async_job: skipping registration (no acceleration or legacy premium connection)"
        );
        return;
    }
    // v0.3.2 renamed the public-facing tool name from `atlas_job` to
    // `async_job`. The implementation's metadata uses the new name;
    // the old name stays registered as a back-compat alias for one
    // minor cycle so any in-flight callers don't break.
    let tool = Arc::new(AtlasJobTool::with_acceleration(
        atlas_layer,
        acceleration_layer,
        session,
    ));
    registry.register("async_job", tool.clone());
    registry.register("atlas_job", tool);
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_types::atlas_layer::{
        AtlasFunctionsError, AtlasFunctionsProvider, AtlasJobHandle, AtlasJobResultPage,
        AtlasJobState, AtlasProductHealth, AtlasProductId, AtlasProductLayer,
    };
    use std::sync::Mutex;

    #[test]
    fn metadata_marks_job_submission_as_a_long_running_write() {
        let config = mcp_types::Config::default();
        let client = mcp_client::ContextStreamClient::new(config.clone());
        let session = Arc::new(SessionManager::new(client, config));
        let tool = AtlasJobTool::new(mcp_types::atlas_layer::noop_layer(), session);
        let metadata = tool.metadata();

        assert!(!metadata.annotations.read_only);
        assert!(!metadata.annotations.destructive);
        assert!(metadata.annotations.requires_confirmation);
        assert!(metadata.annotations.long_running);
    }

    /// Layer that exposes a Functions provider and reports
    /// `is_enabled()` so the conditional tool registration path
    /// activates.
    struct FakeLayer {
        provider: Arc<dyn AtlasFunctionsProvider>,
    }

    impl AtlasProductLayer for FakeLayer {
        fn has_connection(&self) -> bool {
            true
        }

        fn is_enabled(&self) -> bool {
            true
        }

        fn available_products(&self) -> Vec<AtlasProductId> {
            vec![AtlasProductId::Functions]
        }

        fn health(&self, product: AtlasProductId) -> AtlasProductHealth {
            match product {
                AtlasProductId::Functions => AtlasProductHealth::available(product),
                other => AtlasProductHealth::not_available(other, "fake layer"),
            }
        }

        fn functions(&self) -> Option<Arc<dyn AtlasFunctionsProvider>> {
            Some(self.provider.clone())
        }
    }

    /// Same shape as the unit test mock in hosted compatibility provider. We
    /// duplicate it here because cross-crate test re-export would
    /// require a `pub` mock module on a non-test boundary.
    struct MockFunctions {
        jobs: Mutex<std::collections::HashMap<String, AtlasJobState>>,
        results: Mutex<std::collections::HashMap<String, Vec<serde_json::Value>>>,
        force_submit_error: Mutex<Option<String>>,
    }

    impl MockFunctions {
        fn new() -> Self {
            Self {
                jobs: Mutex::new(Default::default()),
                results: Mutex::new(Default::default()),
                force_submit_error: Mutex::new(None),
            }
        }

        fn complete_with(&self, job_id: &str, records: Vec<serde_json::Value>) {
            let mut jobs = self.jobs.lock().unwrap();
            if let Some(state) = jobs.get_mut(job_id) {
                state.status = AtlasJobStatus::Completed;
                state.record_count = Some(records.len() as u64);
                state.completed_at = Some(chrono::Utc::now());
                state.progress = Some(1.0);
            }
            self.results
                .lock()
                .unwrap()
                .insert(job_id.to_string(), records);
        }

        fn fail_with(&self, job_id: &str, error: &str) {
            let mut jobs = self.jobs.lock().unwrap();
            if let Some(state) = jobs.get_mut(job_id) {
                state.status = AtlasJobStatus::Failed;
                state.error = Some(error.to_string());
                state.completed_at = Some(chrono::Utc::now());
            }
        }
    }

    #[async_trait]
    impl AtlasFunctionsProvider for MockFunctions {
        async fn submit_job(
            &self,
            spec: AtlasJobSpec,
        ) -> std::result::Result<AtlasJobHandle, AtlasFunctionsError> {
            if let Some(err) = self.force_submit_error.lock().unwrap().clone() {
                return Err(AtlasFunctionsError::Submission(err));
            }
            let job_id = format!("mock-job-{}", Uuid::new_v4());
            let now = chrono::Utc::now();
            self.jobs.lock().unwrap().insert(
                job_id.clone(),
                AtlasJobState {
                    job_id: job_id.clone(),
                    kind: spec.kind,
                    status: AtlasJobStatus::Pending,
                    progress: None,
                    record_count: None,
                    submitted_at: now,
                    started_at: None,
                    completed_at: None,
                    error: None,
                },
            );
            Ok(AtlasJobHandle {
                job_id,
                kind: spec.kind,
                submitted_at: now,
                estimated_total: None,
            })
        }

        async fn poll_job(
            &self,
            job_id: &str,
        ) -> std::result::Result<AtlasJobState, AtlasFunctionsError> {
            self.jobs
                .lock()
                .unwrap()
                .get(job_id)
                .cloned()
                .ok_or_else(|| AtlasFunctionsError::JobNotFound(job_id.to_string()))
        }

        async fn fetch_result_page(
            &self,
            job_id: &str,
            cursor: Option<u64>,
            limit: usize,
        ) -> std::result::Result<AtlasJobResultPage, AtlasFunctionsError> {
            let results = self.results.lock().unwrap();
            let all = results
                .get(job_id)
                .cloned()
                .ok_or_else(|| AtlasFunctionsError::JobNotFound(job_id.to_string()))?;
            let start = cursor.unwrap_or(0) as usize;
            let limit = limit.min(200).max(1);
            let end = (start + limit).min(all.len());
            let page = all[start.min(all.len())..end].to_vec();
            Ok(AtlasJobResultPage {
                job_id: job_id.to_string(),
                seq_start: start as u64,
                records: page,
                has_more: end < all.len(),
            })
        }
    }

    fn fake_session() -> Arc<SessionManager> {
        let cfg = mcp_types::config::Config::default();
        let client = mcp_client::ContextStreamClient::new(cfg.clone());
        Arc::new(SessionManager::new(client, cfg))
    }

    fn build_tool() -> (AtlasJobTool, Arc<MockFunctions>) {
        let mock = Arc::new(MockFunctions::new());
        let layer: AtlasLayer = Arc::new(FakeLayer {
            provider: mock.clone(),
        });
        (AtlasJobTool::new(layer, fake_session()), mock)
    }

    #[tokio::test]
    async fn submit_export_returns_job_id_quickly() {
        let (tool, _mock) = build_tool();
        let ws = Uuid::new_v4();
        let result = tool
            .execute(serde_json::json!({
                "action": "submit_export",
                "collection": "decisions",
                "workspace_id": ws.to_string(),
            }))
            .await
            .unwrap();
        assert!(!result.is_error);
        let s = result.structured_content.expect("structured");
        assert!(s["job_id"].as_str().unwrap().starts_with("mock-job-"));
        assert_eq!(s["kind"], serde_json::json!("memory_export"));
    }

    #[tokio::test]
    async fn submit_export_requires_collection() {
        let (tool, _mock) = build_tool();
        let ws = Uuid::new_v4();
        let err = tool
            .execute(serde_json::json!({
                "action": "submit_export",
                "workspace_id": ws.to_string(),
            }))
            .await
            .unwrap_err();
        match err {
            Error::Validation(msg) => assert!(msg.contains("collection")),
            other => panic!("expected Validation, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn submit_export_rejects_unknown_collection() {
        let (tool, _mock) = build_tool();
        let ws = Uuid::new_v4();
        let err = tool
            .execute(serde_json::json!({
                "action": "submit_export",
                "collection": "secrets",
                "workspace_id": ws.to_string(),
            }))
            .await
            .unwrap_err();
        match err {
            Error::Validation(msg) => assert!(msg.contains("unknown collection")),
            other => panic!("expected Validation, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn poll_reports_pending_then_completed() {
        let (tool, mock) = build_tool();
        let ws = Uuid::new_v4();
        let submit = tool
            .execute(serde_json::json!({
                "action": "submit_export",
                "collection": "lessons",
                "workspace_id": ws.to_string(),
            }))
            .await
            .unwrap();
        let job_id = submit.structured_content.unwrap()["job_id"]
            .as_str()
            .unwrap()
            .to_string();

        let pending = tool
            .execute(serde_json::json!({"action": "poll", "job_id": job_id.clone()}))
            .await
            .unwrap();
        let s = pending.structured_content.unwrap();
        assert_eq!(s["status"], serde_json::json!("pending"));
        assert_eq!(s["is_terminal"], serde_json::json!(false));

        mock.complete_with(
            &job_id,
            vec![
                serde_json::json!({"id": "a"}),
                serde_json::json!({"id": "b"}),
            ],
        );

        let done = tool
            .execute(serde_json::json!({"action": "poll", "job_id": job_id}))
            .await
            .unwrap();
        let s = done.structured_content.unwrap();
        assert_eq!(s["status"], serde_json::json!("completed"));
        assert_eq!(s["is_terminal"], serde_json::json!(true));
        assert_eq!(s["record_count"], serde_json::json!(2));
    }

    #[tokio::test]
    async fn result_paginates_records_after_completion() {
        let (tool, mock) = build_tool();
        let ws = Uuid::new_v4();
        let submit = tool
            .execute(serde_json::json!({
                "action": "submit_export",
                "collection": "transcripts",
                "workspace_id": ws.to_string(),
            }))
            .await
            .unwrap();
        let job_id = submit.structured_content.unwrap()["job_id"]
            .as_str()
            .unwrap()
            .to_string();

        let records: Vec<_> = (0..5).map(|i| serde_json::json!({"seq": i})).collect();
        mock.complete_with(&job_id, records);

        let page1 = tool
            .execute(serde_json::json!({
                "action": "result",
                "job_id": job_id.clone(),
                "limit": 2,
            }))
            .await
            .unwrap();
        let s = page1.structured_content.unwrap();
        assert_eq!(s["records"].as_array().unwrap().len(), 2);
        assert_eq!(s["has_more"], serde_json::json!(true));
        assert_eq!(s["next_cursor"], serde_json::json!(2));

        let page2 = tool
            .execute(serde_json::json!({
                "action": "result",
                "job_id": job_id.clone(),
                "cursor": 2,
                "limit": 2,
            }))
            .await
            .unwrap();
        let s = page2.structured_content.unwrap();
        assert_eq!(s["records"].as_array().unwrap().len(), 2);
        assert_eq!(s["has_more"], serde_json::json!(true));
        assert_eq!(s["next_cursor"], serde_json::json!(4));

        let page3 = tool
            .execute(serde_json::json!({
                "action": "result",
                "job_id": job_id,
                "cursor": 4,
                "limit": 2,
            }))
            .await
            .unwrap();
        let s = page3.structured_content.unwrap();
        assert_eq!(s["records"].as_array().unwrap().len(), 1);
        assert_eq!(s["has_more"], serde_json::json!(false));
        assert!(s["next_cursor"].is_null());
    }

    #[tokio::test]
    async fn result_warns_when_job_not_yet_terminal() {
        let (tool, _mock) = build_tool();
        let ws = Uuid::new_v4();
        let submit = tool
            .execute(serde_json::json!({
                "action": "submit_export",
                "collection": "decisions",
                "workspace_id": ws.to_string(),
            }))
            .await
            .unwrap();
        let job_id = submit.structured_content.unwrap()["job_id"]
            .as_str()
            .unwrap()
            .to_string();

        let still_pending = tool
            .execute(serde_json::json!({
                "action": "result",
                "job_id": job_id,
            }))
            .await
            .unwrap();
        let s = still_pending.structured_content.unwrap();
        assert_eq!(s["status"], serde_json::json!("pending"));
        assert_eq!(s["is_terminal"], serde_json::json!(false));
        assert!(still_pending
            .content
            .iter()
            .any(|c| matches!(c, mcp_types::tool::ContentItem::Text { text } if text.contains("not yet complete"))));
    }

    #[tokio::test]
    async fn result_surfaces_failed_job_error() {
        let (tool, mock) = build_tool();
        let ws = Uuid::new_v4();
        let submit = tool
            .execute(serde_json::json!({
                "action": "submit_export",
                "collection": "lessons",
                "workspace_id": ws.to_string(),
            }))
            .await
            .unwrap();
        let job_id = submit.structured_content.unwrap()["job_id"]
            .as_str()
            .unwrap()
            .to_string();
        mock.fail_with(&job_id, "boom");

        let result = tool
            .execute(serde_json::json!({"action": "result", "job_id": job_id}))
            .await
            .unwrap();
        let s = result.structured_content.unwrap();
        assert_eq!(s["status"], serde_json::json!("failed"));
        assert_eq!(s["error"], serde_json::json!("boom"));
    }

    #[tokio::test]
    async fn unknown_action_errors() {
        let (tool, _mock) = build_tool();
        let err = tool
            .execute(serde_json::json!({"action": "explode"}))
            .await
            .unwrap_err();
        match err {
            Error::Validation(msg) => assert!(msg.contains("unknown action")),
            other => panic!("expected Validation, got {:?}", other),
        }
    }

    #[test]
    fn skips_registration_when_layer_disabled() {
        let cfg = mcp_types::config::Config::default();
        let mut registry = crate::registry::ToolRegistry::new(&cfg);
        // Default layer is no-op.
        register_atlas_job_tools(&mut registry, fake_session());
        assert!(registry.get("atlas_job").is_none());
    }

    #[test]
    fn registers_when_layer_enabled() {
        let provider: Arc<dyn AtlasFunctionsProvider> = Arc::new(MockFunctions::new());
        let layer: AtlasLayer = Arc::new(FakeLayer { provider });
        let cfg = mcp_types::config::Config::default();
        let mut registry = crate::registry::ToolRegistry::new(&cfg);
        registry.set_atlas_layer(layer);
        register_atlas_job_tools(&mut registry, fake_session());
        assert!(registry.get("atlas_job").is_some());
    }

    #[tokio::test]
    async fn submit_aggregate_routes_to_aggregate_kind() {
        let (tool, _mock) = build_tool();
        let ws = Uuid::new_v4();
        let result = tool
            .execute(serde_json::json!({
                "action": "submit_aggregate",
                "collection": "transcripts",
                "options": {"bucket": "day"},
                "workspace_id": ws.to_string(),
            }))
            .await
            .unwrap();
        let s = result.structured_content.unwrap();
        assert_eq!(s["kind"], serde_json::json!("memory_aggregate"));
    }
}
