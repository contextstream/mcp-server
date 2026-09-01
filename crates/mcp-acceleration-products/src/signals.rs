use async_trait::async_trait;
use mcp_types::acceleration_layer::{
    AccelerationSignalError, AccelerationSignalEvent, SignalProvider,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use tracing::debug;

use crate::warm_cache::normalize_acceleration_api_url;

const DEFAULT_TIMEOUT: Duration = Duration::from_millis(100);
const MAX_SIGNAL_METADATA_BYTES: usize = 4096;

#[derive(Debug, Clone)]
pub struct ContextStreamSignalProvider {
    client: reqwest::Client,
    acceleration_api_url: String,
    job_api_token: String,
}

impl ContextStreamSignalProvider {
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
struct TelemetryBody {
    tenant_id: Option<uuid::Uuid>,
    workspace_id: Option<uuid::Uuid>,
    project_id: Option<uuid::Uuid>,
    tool: Option<String>,
    action: Option<String>,
    signal_type: String,
    cache_hit: Option<bool>,
    provider: String,
    latency_ms: Option<u64>,
    degraded: Option<bool>,
    generation: Option<i64>,
    timestamp: chrono::DateTime<chrono::Utc>,
    request_id: Option<String>,
    metadata: Value,
}

impl From<AccelerationSignalEvent> for TelemetryBody {
    fn from(event: AccelerationSignalEvent) -> Self {
        let signal_type = event.kind.as_str().to_string();
        let metadata = bounded_metadata(&signal_type, event.metadata);
        Self {
            tenant_id: event.tenant_id,
            workspace_id: event.workspace_id,
            project_id: event.project_id,
            tool: event.tool,
            action: event.action.or_else(|| Some(signal_type.clone())),
            signal_type,
            cache_hit: event.cache_hit,
            provider: event.provider.unwrap_or_else(|| "signals".to_string()),
            latency_ms: event.latency_ms,
            degraded: event.degraded,
            generation: event.generation,
            timestamp: event.emitted_at,
            request_id: event.request_id,
            metadata,
        }
    }
}

#[derive(Debug, Deserialize)]
struct ApiResponse {
    success: bool,
    error: Option<ApiErrorBody>,
}

#[derive(Debug, Deserialize)]
struct ApiErrorBody {
    code: String,
    message: String,
    details: Option<Value>,
}

#[async_trait]
impl SignalProvider for ContextStreamSignalProvider {
    async fn emit(&self, event: AccelerationSignalEvent) -> Result<(), AccelerationSignalError> {
        let body = TelemetryBody::from(event);
        let signal_type = body.signal_type.clone();
        let provider = body.provider.clone();
        metrics::counter!(
            "acceleration_signal_emit_total",
            "signal_type" => signal_type.clone(),
            "provider" => provider.clone(),
        )
        .increment(1);

        let response = self
            .client
            .post(self.endpoint("telemetry"))
            .bearer_auth(&self.job_api_token)
            .json(&body)
            .send()
            .await
            .map_err(|error| {
                if error.is_timeout() {
                    metrics::counter!(
                        "acceleration_signal_emit_timeout_total",
                        "signal_type" => signal_type.clone(),
                        "provider" => provider.clone(),
                    )
                    .increment(1);
                }
                metrics::counter!(
                    "acceleration_signal_emit_failure_total",
                    "signal_type" => signal_type.clone(),
                    "provider" => provider.clone(),
                )
                .increment(1);
                AccelerationSignalError::Request(error.to_string())
            })?;

        let status = response.status();
        if !status.is_success() {
            debug!(
                status = status.as_u16(),
                "acceleration signal telemetry request failed"
            );
            metrics::counter!(
                "acceleration_signal_emit_failure_total",
                "signal_type" => signal_type.clone(),
                "provider" => provider.clone(),
            )
            .increment(1);
            return Err(AccelerationSignalError::Request(format!(
                "server returned {status}"
            )));
        }

        let envelope = response.json::<ApiResponse>().await.map_err(|error| {
            metrics::counter!(
                "acceleration_signal_emit_failure_total",
                "signal_type" => signal_type.clone(),
                "provider" => provider.clone(),
            )
            .increment(1);
            AccelerationSignalError::Decode(error.to_string())
        })?;

        if !envelope.success {
            metrics::counter!(
                "acceleration_signal_emit_failure_total",
                "signal_type" => signal_type.clone(),
                "provider" => provider.clone(),
            )
            .increment(1);
            return Err(AccelerationSignalError::Request(format_api_error(
                envelope.error,
            )));
        }

        metrics::counter!(
            "acceleration_signal_emit_success_total",
            "signal_type" => signal_type,
            "provider" => provider,
        )
        .increment(1);
        Ok(())
    }
}

fn bounded_metadata(signal_type: &str, metadata: Value) -> Value {
    let Ok(bytes) = serde_json::to_vec(&metadata) else {
        metrics::counter!(
            "acceleration_signal_metadata_truncated_total",
            "signal_type" => signal_type.to_string(),
            "reason" => "encode_failed",
        )
        .increment(1);
        return serde_json::json!({
            "truncated": true,
            "reason": "metadata_encode_failed"
        });
    };

    if bytes.len() <= MAX_SIGNAL_METADATA_BYTES {
        return metadata;
    }

    metrics::counter!(
        "acceleration_signal_metadata_truncated_total",
        "signal_type" => signal_type.to_string(),
        "reason" => "too_large",
    )
    .increment(1);
    serde_json::json!({
        "truncated": true,
        "original_size_bytes": bytes.len(),
        "max_size_bytes": MAX_SIGNAL_METADATA_BYTES,
    })
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
    use mcp_types::acceleration_layer::AccelerationSignalKind;

    #[test]
    fn telemetry_body_maps_signal_shape() {
        let workspace_id = uuid::Uuid::new_v4();
        let project_id = uuid::Uuid::new_v4();
        let mut event = AccelerationSignalEvent::with_scope(
            AccelerationSignalKind::FileChanged,
            workspace_id,
            Some(project_id),
            serde_json::json!({"files_indexed": 3}),
        );
        event.tool = Some("index_keeper".to_string());
        event.provider = Some("server_telemetry".to_string());
        event.latency_ms = Some(12);

        let body = TelemetryBody::from(event);
        assert_eq!(body.workspace_id, Some(workspace_id));
        assert_eq!(body.project_id, Some(project_id));
        assert_eq!(body.signal_type, "file_changed");
        assert_eq!(body.action.as_deref(), Some("file_changed"));
        assert_eq!(body.tool.as_deref(), Some("index_keeper"));
        assert_eq!(body.provider, "server_telemetry");
        assert_eq!(body.latency_ms, Some(12));
        assert_eq!(body.metadata["files_indexed"], 3);
    }

    #[test]
    fn telemetry_body_truncates_large_metadata() {
        let event = AccelerationSignalEvent::with_scope(
            AccelerationSignalKind::ToolCall,
            uuid::Uuid::new_v4(),
            None,
            serde_json::json!({"large": "x".repeat(MAX_SIGNAL_METADATA_BYTES + 1)}),
        );

        let body = TelemetryBody::from(event);
        assert_eq!(body.metadata["truncated"], true);
        assert_eq!(body.metadata["max_size_bytes"], MAX_SIGNAL_METADATA_BYTES);
    }
}
