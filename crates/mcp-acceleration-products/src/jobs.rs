use async_trait::async_trait;
use mcp_types::acceleration_layer::{
    AccelerationJobError, AccelerationJobHandle, AccelerationJobKind, AccelerationJobResultPage,
    AccelerationJobSpec, AccelerationJobState, AccelerationJobStatus, JobProvider,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use tracing::debug;

use crate::warm_cache::normalize_acceleration_api_url;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct ContextStreamJobProvider {
    client: reqwest::Client,
    acceleration_api_url: String,
    job_api_token: String,
}

impl ContextStreamJobProvider {
    pub fn new(acceleration_api_url: String, job_api_token: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(DEFAULT_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            acceleration_api_url: normalize_acceleration_api_url(&acceleration_api_url),
            job_api_token,
        }
    }

    fn endpoint(&self, path: &str) -> String {
        format!(
            "{}/{}",
            self.acceleration_api_url,
            path.trim_start_matches('/')
        )
    }
}

#[derive(Debug, Serialize)]
struct SubmitJobBody {
    tenant_id: Option<uuid::Uuid>,
    workspace_id: uuid::Uuid,
    project_id: Option<uuid::Uuid>,
    kind: String,
    collection: String,
    filter: Value,
    options: Value,
}

impl From<AccelerationJobSpec> for SubmitJobBody {
    fn from(spec: AccelerationJobSpec) -> Self {
        Self {
            tenant_id: None,
            workspace_id: spec.workspace_id,
            project_id: spec.project_id,
            kind: spec.kind.as_str().to_string(),
            collection: spec.collection,
            filter: spec.filter,
            options: spec.options,
        }
    }
}

#[derive(Debug, Serialize)]
struct PollJobBody<'a> {
    job_id: &'a str,
}

#[derive(Debug, Serialize)]
struct ResultJobBody<'a> {
    job_id: &'a str,
    cursor: Option<u64>,
    limit: usize,
}

#[derive(Debug, Deserialize)]
struct ApiResponse<T> {
    success: bool,
    data: Option<T>,
    error: Option<ApiErrorBody>,
}

#[derive(Debug, Deserialize)]
struct ApiErrorBody {
    code: String,
    message: String,
    details: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct SubmitJobResponse {
    job_id: uuid::Uuid,
    kind: String,
    submitted_at: chrono::DateTime<chrono::Utc>,
    estimated_total: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct PollJobResponse {
    job_id: uuid::Uuid,
    kind: String,
    status: String,
    progress: Option<f64>,
    record_count: Option<u64>,
    submitted_at: chrono::DateTime<chrono::Utc>,
    started_at: Option<chrono::DateTime<chrono::Utc>>,
    completed_at: Option<chrono::DateTime<chrono::Utc>>,
    error: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct ResultJobResponse {
    job_id: uuid::Uuid,
    seq_start: u64,
    records: Vec<Value>,
    has_more: bool,
    error: Option<Value>,
}

#[async_trait]
impl JobProvider for ContextStreamJobProvider {
    async fn submit_job(
        &self,
        spec: AccelerationJobSpec,
    ) -> Result<AccelerationJobHandle, AccelerationJobError> {
        let response = self
            .client
            .post(self.endpoint("jobs/submit"))
            .bearer_auth(&self.job_api_token)
            .json(&SubmitJobBody::from(spec))
            .send()
            .await
            .map_err(|error| AccelerationJobError::Request(error.to_string()))?;

        let envelope = decode_envelope::<SubmitJobResponse>(response).await?;
        let data = envelope
            .data
            .ok_or_else(|| AccelerationJobError::Decode("missing submit data".to_string()))?;
        Ok(AccelerationJobHandle {
            job_id: data.job_id.to_string(),
            kind: parse_job_kind(&data.kind)?,
            submitted_at: data.submitted_at,
            estimated_total: data.estimated_total,
        })
    }

    async fn poll_job(&self, job_id: &str) -> Result<AccelerationJobState, AccelerationJobError> {
        let response = self
            .client
            .post(self.endpoint("jobs/poll"))
            .bearer_auth(&self.job_api_token)
            .json(&PollJobBody { job_id })
            .send()
            .await
            .map_err(|error| AccelerationJobError::Request(error.to_string()))?;

        let envelope = decode_envelope::<PollJobResponse>(response).await?;
        let data = envelope
            .data
            .ok_or_else(|| AccelerationJobError::Decode("missing poll data".to_string()))?;
        Ok(AccelerationJobState {
            job_id: data.job_id.to_string(),
            kind: parse_job_kind(&data.kind)?,
            status: AccelerationJobStatus::parse(&data.status).ok_or_else(|| {
                AccelerationJobError::Decode(format!("unknown job status `{}`", data.status))
            })?,
            progress: data.progress,
            record_count: data.record_count,
            submitted_at: data.submitted_at,
            started_at: data.started_at,
            completed_at: data.completed_at,
            error: data.error,
        })
    }

    async fn fetch_result_page(
        &self,
        job_id: &str,
        cursor: Option<u64>,
        limit: usize,
    ) -> Result<AccelerationJobResultPage, AccelerationJobError> {
        let response = self
            .client
            .post(self.endpoint("jobs/result"))
            .bearer_auth(&self.job_api_token)
            .json(&ResultJobBody {
                job_id,
                cursor,
                limit: limit.clamp(1, 200),
            })
            .send()
            .await
            .map_err(|error| AccelerationJobError::Request(error.to_string()))?;

        let envelope = decode_envelope::<ResultJobResponse>(response).await?;
        let data = envelope
            .data
            .ok_or_else(|| AccelerationJobError::Decode("missing result data".to_string()))?;
        if let Some(error) = data.error {
            return Err(AccelerationJobError::Request(format!(
                "server returned job result error: {error}"
            )));
        }
        Ok(AccelerationJobResultPage {
            job_id: data.job_id.to_string(),
            seq_start: data.seq_start,
            records: data.records,
            has_more: data.has_more,
        })
    }
}

async fn decode_envelope<T: for<'de> Deserialize<'de>>(
    response: reqwest::Response,
) -> Result<ApiResponse<T>, AccelerationJobError> {
    let status = response.status();
    if !status.is_success() {
        debug!(
            status = status.as_u16(),
            "acceleration job API request failed"
        );
        return Err(AccelerationJobError::Request(format!(
            "server returned {status}"
        )));
    }

    let envelope = response
        .json::<ApiResponse<T>>()
        .await
        .map_err(|error| AccelerationJobError::Decode(error.to_string()))?;

    if !envelope.success {
        return Err(AccelerationJobError::Request(format_api_error(
            envelope.error,
        )));
    }

    Ok(envelope)
}

fn parse_job_kind(value: &str) -> Result<AccelerationJobKind, AccelerationJobError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "memory_export" => Ok(AccelerationJobKind::MemoryExport),
        "memory_aggregate" => Ok(AccelerationJobKind::MemoryAggregate),
        other => Err(AccelerationJobError::Decode(format!(
            "unknown job kind `{other}`"
        ))),
    }
}

fn format_api_error(error: Option<ApiErrorBody>) -> String {
    match error {
        Some(error) => match error.details {
            Some(details) => format!("{}: {} ({details})", error.code, error.message),
            None => format!("{}: {}", error.code, error.message),
        },
        None => "server returned an error without details".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submit_body_maps_spec_to_server_shape() {
        let workspace_id = uuid::Uuid::new_v4();
        let project_id = uuid::Uuid::new_v4();
        let body = SubmitJobBody::from(AccelerationJobSpec {
            kind: AccelerationJobKind::MemoryExport,
            workspace_id,
            project_id: Some(project_id),
            collection: "decisions".to_string(),
            filter: serde_json::json!({"updated_at": {"$gte": "2026-01-01"}}),
            options: serde_json::json!({"include_content": true}),
        });

        assert_eq!(body.tenant_id, None);
        assert_eq!(body.workspace_id, workspace_id);
        assert_eq!(body.project_id, Some(project_id));
        assert_eq!(body.kind, "memory_export");
        assert_eq!(body.collection, "decisions");
    }

    #[test]
    fn parses_job_kinds() {
        assert_eq!(
            parse_job_kind("memory_export").unwrap(),
            AccelerationJobKind::MemoryExport
        );
        assert_eq!(
            parse_job_kind("memory_aggregate").unwrap(),
            AccelerationJobKind::MemoryAggregate
        );
        assert!(parse_job_kind("other").is_err());
    }
}
