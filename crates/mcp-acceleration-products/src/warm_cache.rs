use async_trait::async_trait;
use mcp_types::acceleration_layer::{
    WarmCacheError, WarmCacheHit, WarmCacheLookup, WarmCacheProvider, WarmCachePut,
    WarmCacheRebuild,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use tracing::debug;

const DEFAULT_LOOKUP_TIMEOUT: Duration = Duration::from_millis(75);
const DEFAULT_SIDE_EFFECT_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
pub struct ContextStreamWarmCacheProvider {
    lookup_client: reqwest::Client,
    side_effect_client: reqwest::Client,
    acceleration_api_url: String,
    job_api_token: String,
}

impl ContextStreamWarmCacheProvider {
    pub fn new(acceleration_api_url: String, job_api_token: String) -> Self {
        let lookup_client = build_timeout_client(DEFAULT_LOOKUP_TIMEOUT);
        let side_effect_client = build_timeout_client(DEFAULT_SIDE_EFFECT_TIMEOUT);
        Self {
            lookup_client,
            side_effect_client,
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

fn build_timeout_client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

#[derive(Debug, Serialize)]
struct ReadModelGetBody {
    tenant_id: Option<uuid::Uuid>,
    workspace_id: Option<uuid::Uuid>,
    project_id: Option<uuid::Uuid>,
    scope_type: String,
    scope_id: uuid::Uuid,
    model: String,
    cache_key: String,
    stale_ok: bool,
    redis_timeout_ms: u64,
}

impl From<WarmCacheLookup> for ReadModelGetBody {
    fn from(lookup: WarmCacheLookup) -> Self {
        Self {
            tenant_id: lookup.scope.tenant_id,
            workspace_id: lookup.scope.workspace_id,
            project_id: lookup.scope.project_id,
            scope_type: lookup.scope.scope_type,
            scope_id: lookup.scope.scope_id,
            model: lookup.model,
            cache_key: lookup.cache_key,
            stale_ok: lookup.stale_ok,
            redis_timeout_ms: 50,
        }
    }
}

#[derive(Debug, Serialize)]
struct EnqueueRebuildBody {
    tenant_id: Option<uuid::Uuid>,
    workspace_id: Option<uuid::Uuid>,
    project_id: Option<uuid::Uuid>,
    scope_type: String,
    scope_id: uuid::Uuid,
    model: String,
    cache_key: Option<String>,
    reason: String,
    target_generation: i64,
    job_kind: Option<String>,
}

impl From<WarmCacheRebuild> for EnqueueRebuildBody {
    fn from(rebuild: WarmCacheRebuild) -> Self {
        Self {
            tenant_id: rebuild.scope.tenant_id,
            workspace_id: rebuild.scope.workspace_id,
            project_id: rebuild.scope.project_id,
            scope_type: rebuild.scope.scope_type,
            scope_id: rebuild.scope.scope_id,
            model: rebuild.model,
            cache_key: rebuild.cache_key,
            reason: rebuild.reason,
            target_generation: rebuild.target_generation,
            job_kind: rebuild.job_kind,
        }
    }
}

#[derive(Debug, Serialize)]
struct PutReadModelJsonbBody {
    tenant_id: Option<uuid::Uuid>,
    workspace_id: Option<uuid::Uuid>,
    project_id: Option<uuid::Uuid>,
    scope_type: String,
    scope_id: uuid::Uuid,
    model: String,
    cache_key: String,
    generation: Option<i64>,
    source_generation: i64,
    payload: Value,
    etag: Option<String>,
    expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl From<WarmCachePut> for PutReadModelJsonbBody {
    fn from(put: WarmCachePut) -> Self {
        Self {
            tenant_id: put.scope.tenant_id,
            workspace_id: put.scope.workspace_id,
            project_id: put.scope.project_id,
            scope_type: put.scope.scope_type,
            scope_id: put.scope.scope_id,
            model: put.model,
            cache_key: put.cache_key,
            generation: put.generation,
            source_generation: put.source_generation,
            payload: put.payload,
            etag: put.etag,
            expires_at: put.expires_at,
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
struct ReadModelGetResponse {
    hit: bool,
    read_model: Option<WarmCacheHit>,
}

#[async_trait]
impl WarmCacheProvider for ContextStreamWarmCacheProvider {
    async fn get_read_model(
        &self,
        lookup: WarmCacheLookup,
    ) -> Result<Option<WarmCacheHit>, WarmCacheError> {
        let response = self
            .lookup_client
            .post(self.endpoint("read-model/get"))
            .bearer_auth(&self.job_api_token)
            .json(&ReadModelGetBody::from(lookup))
            .send()
            .await
            .map_err(|error| WarmCacheError::Request(error.to_string()))?;

        if !response.status().is_success() {
            return Err(WarmCacheError::Request(format!(
                "server returned {}",
                response.status()
            )));
        }

        let envelope = response
            .json::<ApiResponse<ReadModelGetResponse>>()
            .await
            .map_err(|error| WarmCacheError::Decode(error.to_string()))?;

        if !envelope.success {
            return Err(WarmCacheError::Request(format_api_error(envelope.error)));
        }

        let data = envelope
            .data
            .ok_or_else(|| WarmCacheError::Decode("missing read-model data".to_string()))?;
        if !data.hit {
            return Ok(None);
        }
        Ok(data.read_model)
    }

    async fn enqueue_rebuild(&self, rebuild: WarmCacheRebuild) -> Result<(), WarmCacheError> {
        let response = self
            .side_effect_client
            .post(self.endpoint("read-model/enqueue-rebuild"))
            .bearer_auth(&self.job_api_token)
            .json(&EnqueueRebuildBody::from(rebuild))
            .send()
            .await
            .map_err(|error| WarmCacheError::Request(error.to_string()))?;

        if !response.status().is_success() {
            debug!(
                status = response.status().as_u16(),
                "acceleration warm-cache rebuild enqueue failed"
            );
            return Err(WarmCacheError::Request(format!(
                "server returned {}",
                response.status()
            )));
        }

        Ok(())
    }

    async fn put_read_model(&self, put: WarmCachePut) -> Result<(), WarmCacheError> {
        let response = self
            .side_effect_client
            .post(self.endpoint("read-model/put-jsonb"))
            .bearer_auth(&self.job_api_token)
            .json(&PutReadModelJsonbBody::from(put))
            .send()
            .await
            .map_err(|error| WarmCacheError::Request(error.to_string()))?;

        if !response.status().is_success() {
            debug!(
                status = response.status().as_u16(),
                "acceleration warm-cache JSONB write-through failed"
            );
            return Err(WarmCacheError::Request(format!(
                "server returned {}",
                response.status()
            )));
        }

        let envelope = response
            .json::<ApiResponse<Value>>()
            .await
            .map_err(|error| WarmCacheError::Decode(error.to_string()))?;

        if !envelope.success {
            return Err(WarmCacheError::Request(format_api_error(envelope.error)));
        }

        Ok(())
    }
}

pub fn normalize_acceleration_api_url(input: &str) -> String {
    let trimmed = input.trim().trim_end_matches('/');
    if trimmed.ends_with("/v1/acceleration") || trimmed.ends_with("/acceleration") {
        trimmed.to_string()
    } else if trimmed.ends_with("/v1") {
        format!("{trimmed}/acceleration")
    } else {
        format!("{trimmed}/v1/acceleration")
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
    fn normalizes_api_url_variants() {
        assert_eq!(
            normalize_acceleration_api_url("https://api.contextstream.io"),
            "https://api.contextstream.io/v1/acceleration"
        );
        assert_eq!(
            normalize_acceleration_api_url("https://api.contextstream.io/v1"),
            "https://api.contextstream.io/v1/acceleration"
        );
        assert_eq!(
            normalize_acceleration_api_url("https://api.contextstream.io/v1/acceleration/"),
            "https://api.contextstream.io/v1/acceleration"
        );
    }

    #[test]
    fn warm_cache_provider_separates_lookup_and_write_budgets() {
        assert!(DEFAULT_LOOKUP_TIMEOUT <= Duration::from_millis(100));
        assert!(DEFAULT_SIDE_EFFECT_TIMEOUT >= Duration::from_secs(2));
        assert!(DEFAULT_SIDE_EFFECT_TIMEOUT > DEFAULT_LOOKUP_TIMEOUT);
    }

    #[test]
    fn put_body_preserves_scope_and_payload() {
        let tenant_id = uuid::Uuid::new_v4();
        let workspace_id = uuid::Uuid::new_v4();
        let project_id = uuid::Uuid::new_v4();
        let scope_id = project_id;
        let put = WarmCachePut {
            scope: mcp_types::acceleration_layer::AccelerationReadModelScope {
                tenant_id: Some(tenant_id),
                workspace_id: Some(workspace_id),
                project_id: Some(project_id),
                scope_type: "project".to_string(),
                scope_id,
            },
            model: "dependency_result".to_string(),
            cache_key: "cache-key".to_string(),
            generation: Some(2),
            source_generation: 2,
            payload: serde_json::json!({"dependencies": []}),
            etag: None,
            expires_at: None,
        };

        let body = PutReadModelJsonbBody::from(put);
        assert_eq!(body.tenant_id, Some(tenant_id));
        assert_eq!(body.workspace_id, Some(workspace_id));
        assert_eq!(body.project_id, Some(project_id));
        assert_eq!(body.scope_type, "project");
        assert_eq!(body.scope_id, scope_id);
        assert_eq!(body.model, "dependency_result");
        assert_eq!(body.cache_key, "cache-key");
        assert_eq!(body.source_generation, 2);
        assert_eq!(body.payload, serde_json::json!({"dependencies": []}));
    }
}
