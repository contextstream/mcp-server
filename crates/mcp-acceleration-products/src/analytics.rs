use async_trait::async_trait;
use mcp_types::acceleration_layer::{
    AccelerationAnalyticsChart, AccelerationAnalyticsError, AccelerationAnalyticsRender,
    AccelerationAnalyticsRenderRequest, AccelerationAnalyticsSeries, AnalyticsProvider,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;
use tracing::debug;

use crate::warm_cache::normalize_acceleration_api_url;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct ContextStreamAnalyticsProvider {
    client: reqwest::Client,
    acceleration_api_url: String,
    job_api_token: String,
}

impl ContextStreamAnalyticsProvider {
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
struct AnalyticsRenderBody {
    tenant_id: Option<uuid::Uuid>,
    workspace_id: uuid::Uuid,
    project_id: Option<uuid::Uuid>,
    chart_key: String,
    range: Option<String>,
    granularity: Option<String>,
    filters: Option<Value>,
}

impl From<AccelerationAnalyticsRenderRequest> for AnalyticsRenderBody {
    fn from(request: AccelerationAnalyticsRenderRequest) -> Self {
        Self {
            tenant_id: request.scope.tenant_id,
            workspace_id: request.scope.workspace_id,
            project_id: request.scope.project_id,
            chart_key: request.chart_key,
            range: request.range,
            granularity: request.granularity,
            filters: request.filters,
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
struct AnalyticsChartsResponse {
    charts: Vec<AccelerationAnalyticsChart>,
}

#[derive(Debug, Deserialize)]
struct AnalyticsRenderResponse {
    chart_key: String,
    title: String,
    range: String,
    granularity: String,
    source: String,
    series: Vec<AccelerationAnalyticsSeries>,
    generated_at: chrono::DateTime<chrono::Utc>,
    degraded: bool,
    note: Option<String>,
}

#[async_trait]
impl AnalyticsProvider for ContextStreamAnalyticsProvider {
    async fn list_charts(
        &self,
    ) -> Result<Vec<AccelerationAnalyticsChart>, AccelerationAnalyticsError> {
        let response = self
            .client
            .get(self.endpoint("analytics/charts"))
            .bearer_auth(&self.job_api_token)
            .send()
            .await
            .map_err(|error| AccelerationAnalyticsError::Request(error.to_string()))?;

        let envelope = decode_envelope::<AnalyticsChartsResponse>(response).await?;
        let data = envelope
            .data
            .ok_or_else(|| AccelerationAnalyticsError::Decode("missing chart data".to_string()))?;
        Ok(data.charts)
    }

    async fn render_chart(
        &self,
        request: AccelerationAnalyticsRenderRequest,
    ) -> Result<AccelerationAnalyticsRender, AccelerationAnalyticsError> {
        let response = self
            .client
            .post(self.endpoint("analytics/render"))
            .bearer_auth(&self.job_api_token)
            .json(&AnalyticsRenderBody::from(request))
            .send()
            .await
            .map_err(|error| AccelerationAnalyticsError::Request(error.to_string()))?;

        let envelope = decode_envelope::<AnalyticsRenderResponse>(response).await?;
        let data = envelope
            .data
            .ok_or_else(|| AccelerationAnalyticsError::Decode("missing render data".to_string()))?;
        Ok(AccelerationAnalyticsRender {
            chart_key: data.chart_key,
            title: data.title,
            range: data.range,
            granularity: data.granularity,
            source: data.source,
            series: data.series,
            generated_at: data.generated_at,
            degraded: data.degraded,
            note: data.note,
        })
    }
}

async fn decode_envelope<T: for<'de> Deserialize<'de>>(
    response: reqwest::Response,
) -> Result<ApiResponse<T>, AccelerationAnalyticsError> {
    let status = response.status();
    if !status.is_success() {
        debug!(
            status = status.as_u16(),
            "acceleration analytics API request failed"
        );
        return Err(AccelerationAnalyticsError::Request(format!(
            "server returned {status}"
        )));
    }

    let envelope = response
        .json::<ApiResponse<T>>()
        .await
        .map_err(|error| AccelerationAnalyticsError::Decode(error.to_string()))?;
    if !envelope.success {
        return Err(AccelerationAnalyticsError::Request(format_api_error(
            envelope.error,
        )));
    }
    Ok(envelope)
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
    use mcp_types::acceleration_layer::{AccelerationAnalyticsPoint, AccelerationAnalyticsScope};

    #[test]
    fn render_body_maps_scope_to_server_shape() {
        let workspace_id = uuid::Uuid::new_v4();
        let project_id = uuid::Uuid::new_v4();
        let mut scope = AccelerationAnalyticsScope::new(workspace_id);
        scope.project_id = Some(project_id);

        let body = AnalyticsRenderBody::from(AccelerationAnalyticsRenderRequest {
            scope,
            chart_key: "tool_latency_p95".to_string(),
            range: Some("24h".to_string()),
            granularity: Some("hour".to_string()),
            filters: Some(serde_json::json!({"tool_name": "context"})),
        });

        assert_eq!(body.workspace_id, workspace_id);
        assert_eq!(body.project_id, Some(project_id));
        assert_eq!(body.chart_key, "tool_latency_p95");
        assert_eq!(body.range.as_deref(), Some("24h"));
        assert_eq!(body.granularity.as_deref(), Some("hour"));
        assert_eq!(
            body.filters.as_ref().and_then(|v| v.get("tool_name")),
            Some(&serde_json::json!("context"))
        );
    }

    #[test]
    fn analytics_point_is_deserializable() {
        let point: AccelerationAnalyticsPoint = serde_json::from_value(serde_json::json!({
            "t": "2026-06-02T08:00:00Z",
            "value": 42.0
        }))
        .unwrap();
        assert_eq!(point.value, 42.0);
    }
}
