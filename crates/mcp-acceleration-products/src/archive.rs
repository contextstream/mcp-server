use async_trait::async_trait;
use mcp_types::acceleration_layer::{
    AccelerationArchiveCollection, AccelerationArchiveError, AccelerationArchiveHit,
    AccelerationArchiveScope, ArchiveProvider,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use tracing::debug;

use crate::warm_cache::normalize_acceleration_api_url;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct ContextStreamArchiveProvider {
    client: reqwest::Client,
    acceleration_api_url: String,
    job_api_token: String,
}

impl ContextStreamArchiveProvider {
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
struct ArchiveSearchBody {
    tenant_id: Option<uuid::Uuid>,
    workspace_id: uuid::Uuid,
    project_id: Option<uuid::Uuid>,
    collection: Option<String>,
    query: String,
    archived_after: Option<chrono::DateTime<chrono::Utc>>,
    limit: usize,
}

impl ArchiveSearchBody {
    fn from_scope(query: &str, scope: &AccelerationArchiveScope, limit: usize) -> Self {
        Self {
            tenant_id: scope.tenant_id,
            workspace_id: scope.workspace_id,
            project_id: scope.project_id,
            collection: scope
                .collection
                .as_ref()
                .map(|collection| collection.as_str().to_string()),
            query: query.to_string(),
            archived_after: scope.archived_after,
            limit: limit.clamp(1, 50),
        }
    }
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
struct ArchiveSearchResponse {
    hits: Vec<ArchiveSearchHitBody>,
    degraded: bool,
}

#[derive(Debug, Deserialize)]
struct ArchiveSearchHitBody {
    id: uuid::Uuid,
    subject_id: Option<uuid::Uuid>,
    collection: String,
    title: Option<String>,
    snippet: String,
    archived_at: chrono::DateTime<chrono::Utc>,
    score: Option<f64>,
    degraded: bool,
    note: Option<String>,
}

#[async_trait]
impl ArchiveProvider for ContextStreamArchiveProvider {
    async fn search_archive(
        &self,
        query: &str,
        scope: &AccelerationArchiveScope,
        limit: usize,
    ) -> Result<Vec<AccelerationArchiveHit>, AccelerationArchiveError> {
        let response = self
            .client
            .post(self.endpoint("archive/search"))
            .bearer_auth(&self.job_api_token)
            .json(&ArchiveSearchBody::from_scope(query, scope, limit))
            .send()
            .await
            .map_err(|error| AccelerationArchiveError::Request(error.to_string()))?;

        let envelope = decode_envelope::<ArchiveSearchResponse>(response).await?;
        let data = envelope
            .data
            .ok_or_else(|| AccelerationArchiveError::Decode("missing archive data".to_string()))?;
        if data.degraded {
            debug!("acceleration archive response included degraded hits");
        }

        data.hits
            .into_iter()
            .map(|hit| {
                Ok(AccelerationArchiveHit {
                    id: hit.id.to_string(),
                    subject_id: hit.subject_id.map(|id| id.to_string()),
                    collection: parse_archive_collection(&hit.collection)?,
                    title: hit.title,
                    snippet: hit.snippet,
                    archived_at: Some(hit.archived_at),
                    score: hit.score,
                    degraded: hit.degraded,
                    note: hit.note,
                })
            })
            .collect()
    }
}

async fn decode_envelope<T: for<'de> Deserialize<'de>>(
    response: reqwest::Response,
) -> Result<ApiResponse<T>, AccelerationArchiveError> {
    let status = response.status();
    if !status.is_success() {
        debug!(
            status = status.as_u16(),
            "acceleration archive API request failed"
        );
        return Err(AccelerationArchiveError::Request(format!(
            "server returned {status}"
        )));
    }

    let envelope = response
        .json::<ApiResponse<T>>()
        .await
        .map_err(|error| AccelerationArchiveError::Decode(error.to_string()))?;
    if !envelope.success {
        return Err(AccelerationArchiveError::Request(format_api_error(
            envelope.error,
        )));
    }
    Ok(envelope)
}

fn parse_archive_collection(
    value: &str,
) -> Result<AccelerationArchiveCollection, AccelerationArchiveError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "transcripts" => Ok(AccelerationArchiveCollection::Transcripts),
        "decisions" => Ok(AccelerationArchiveCollection::Decisions),
        "lessons" => Ok(AccelerationArchiveCollection::Lessons),
        "docs" => Ok(AccelerationArchiveCollection::Docs),
        "qa_questions" => Ok(AccelerationArchiveCollection::QaQuestions),
        "qa_answers" => Ok(AccelerationArchiveCollection::QaAnswers),
        "qa_kb_items" => Ok(AccelerationArchiveCollection::QaKbItems),
        other => Err(AccelerationArchiveError::Decode(format!(
            "unknown archive collection `{other}`"
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
    fn archive_body_maps_scope_to_server_shape() {
        let workspace_id = uuid::Uuid::new_v4();
        let project_id = uuid::Uuid::new_v4();
        let mut scope = AccelerationArchiveScope::new(workspace_id);
        scope.project_id = Some(project_id);
        scope.collection = Some(AccelerationArchiveCollection::Docs);

        let body = ArchiveSearchBody::from_scope("runbook", &scope, 500);
        assert_eq!(body.workspace_id, workspace_id);
        assert_eq!(body.project_id, Some(project_id));
        assert_eq!(body.collection.as_deref(), Some("docs"));
        assert_eq!(body.query, "runbook");
        assert_eq!(body.limit, 50);
    }

    #[test]
    fn parses_archive_collections() {
        assert_eq!(
            parse_archive_collection("qa_kb_items").unwrap(),
            AccelerationArchiveCollection::QaKbItems
        );
        assert!(parse_archive_collection("other").is_err());
    }
}
