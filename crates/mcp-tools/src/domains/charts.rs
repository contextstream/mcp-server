//! Workspace analytics chart MCP tool.
//!
//! The preferred path is the MongoDB-free acceleration analytics
//! provider backed by allowlisted Postgres rollups. Atlas Charts stays
//! as a temporary compatibility fallback only when the acceleration
//! analytics provider is absent.
//!
//! Actions:
//! - `render` — mint an embed URL for one of the pre-built charts.
//! - `list` — enumerate which charts are configured for this
//!   deployment (the operator may have provisioned only a subset of
//!   the shipped chart set).

use async_trait::async_trait;
use mcp_session::SessionManager;
use mcp_types::acceleration_layer::{
    AccelerationAnalyticsRenderRequest, AccelerationAnalyticsScope, AccelerationLayer,
};
use mcp_types::atlas_layer::{AtlasChartId, AtlasChartScope, AtlasLayer};
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

/// Input to the `chart` / `atlas_chart` MCP tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AtlasChartInput {
    /// Action: `render` (default) or `list`.
    #[serde(default)]
    pub action: Option<String>,
    /// Chart identifier (snake-case). Required for `render`.
    #[serde(default)]
    pub chart: Option<String>,
    /// Back-compat time-range field. New acceleration analytics calls
    /// should use `range`.
    #[serde(default)]
    pub time_range: Option<String>,
    #[serde(default)]
    pub range: Option<String>,
    #[serde(default)]
    pub granularity: Option<String>,
    #[serde(default)]
    pub filters: Option<Value>,
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub project_id: Option<String>,
}

pub struct AtlasChartTool {
    atlas_layer: AtlasLayer,
    acceleration_layer: AccelerationLayer,
    session: Arc<SessionManager>,
    metadata: ToolMetadata,
}

impl AtlasChartTool {
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
            name: "chart".to_string(),
            title: "Workspace charts".to_string(),
            description: "Render an allowlisted ContextStream acceleration analytics chart \
                 scoped to the calling workspace, or list available charts. Charts include \
                 tool_latency_p95, acceleration_cache_hit_rate, provider_degraded_rate, \
                 archive_search_health, and signal_emit_health. Available on hosted/remote \
                 deployments only."
                .to_string(),
            category: ToolCategory::Utility,
            annotations: ToolAnnotations::read_only(),
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

    async fn render_action(&self, input: AtlasChartInput) -> Result<ToolResult> {
        if self.acceleration_layer.is_enabled() {
            if let Some(provider) = self.acceleration_layer.analytics() {
                return self.render_acceleration_action(provider, input).await;
            }
        }

        let provider = match self.atlas_layer.charts() {
            Some(p) => p,
            None => {
                return Ok(charts_unavailable(
                    &self.atlas_layer,
                    &self.acceleration_layer,
                ))
            }
        };

        let chart_str = input
            .chart
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                Error::Validation(
                    "chart(action=\"render\") requires `chart` (one of: \
                     search_volume_timeline, credit_spend_by_product, hot_files, \
                     decision_lesson_density, dependency_graph_snapshot)"
                        .to_string(),
                )
            })?;
        let chart = AtlasChartId::parse(chart_str).ok_or_else(|| {
            Error::Validation(format!(
                "chart: unknown chart `{}`. Valid: {}",
                chart_str,
                AtlasChartId::ALL
                    .iter()
                    .map(|c| c.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))
        })?;

        let workspace_id = self
            .resolve_workspace_id(input.workspace_id.as_deref())
            .await
            .ok_or_else(|| {
                Error::Validation(
                    "chart(action=\"render\") requires a resolved workspace_id".to_string(),
                )
            })?;

        let mut scope = AtlasChartScope::new(workspace_id);
        if let Some(project) = input
            .project_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            let parsed = Uuid::parse_str(project).map_err(|_| {
                Error::Validation(format!("chart: invalid project_id `{}`", project))
            })?;
            scope = scope.with_project(parsed);
        }
        if let Some(time_range) = input
            .time_range
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            scope = scope.with_time_range(time_range);
        }

        let embed = match provider.render_chart(chart, &scope).await {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(error = %e, chart = chart.as_str(), "atlas-charts: render failed");
                return Ok(ToolResult::with_structured(
                    format!("[CHART] render failed: {}", e),
                    serde_json::json!({
                        "stages_used": ["charts"],
                        "available": true,
                        "error": e.to_string(),
                    }),
                ));
            }
        };

        let header = format!(
            "[CHART] {} — {} (token expires {})\n  embed_url: {}",
            embed.chart, embed.description, embed.expires_at, embed.embed_url
        );

        Ok(ToolResult::with_structured(
            header,
            serde_json::json!({
                "stages_used": ["charts"],
                "available": true,
                "chart": embed.chart,
                "chart_id": embed.chart_id,
                "embed_url": embed.embed_url,
                "embedding_token": embed.embedding_token,
                "expires_at": embed.expires_at,
                "applied_filter": embed.applied_filter,
                "description": embed.description,
                "origin": "charts",
            }),
        ))
    }

    async fn list_action(&self) -> Result<ToolResult> {
        if self.acceleration_layer.is_enabled() {
            if let Some(provider) = self.acceleration_layer.analytics() {
                return self.list_acceleration_action(provider).await;
            }
        }

        let provider = match self.atlas_layer.charts() {
            Some(p) => p,
            None => {
                return Ok(charts_unavailable(
                    &self.atlas_layer,
                    &self.acceleration_layer,
                ))
            }
        };

        let configured = provider.configured_charts();
        let configured_set: std::collections::HashSet<_> = configured.iter().copied().collect();

        let entries: Vec<serde_json::Value> = AtlasChartId::ALL
            .iter()
            .map(|c| {
                serde_json::json!({
                    "chart": c.as_str(),
                    "description": c.description(),
                    "configured": configured_set.contains(c),
                })
            })
            .collect();

        let mut header = format!(
            "[CHART] {} of {} charts configured.",
            configured.len(),
            AtlasChartId::ALL.len()
        );
        if !configured.is_empty() {
            header.push('\n');
            for c in &configured {
                header.push_str(&format!("  - {} — {}\n", c.as_str(), c.description()));
            }
        }
        let unconfigured: Vec<&str> = AtlasChartId::ALL
            .iter()
            .filter(|c| !configured_set.contains(c))
            .map(|c| c.as_str())
            .collect();
        if !unconfigured.is_empty() {
            header.push_str(&format!(
                "  (operator can enable: {})",
                unconfigured.join(", ")
            ));
        }

        Ok(ToolResult::with_structured(
            header,
            serde_json::json!({
                "stages_used": ["charts"],
                "available": true,
                "configured_count": configured.len(),
                "total_count": AtlasChartId::ALL.len(),
                "charts": entries,
                "origin": "charts",
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

    async fn render_acceleration_action(
        &self,
        provider: Arc<dyn mcp_types::acceleration_layer::AnalyticsProvider>,
        input: AtlasChartInput,
    ) -> Result<ToolResult> {
        let chart_key = input
            .chart
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                Error::Validation(
                    "chart(action=\"render\") requires `chart` (one of: tool_latency_p95, \
                     acceleration_cache_hit_rate, provider_degraded_rate, \
                     archive_search_health, signal_emit_health)"
                        .to_string(),
                )
            })?
            .to_string();

        let workspace_id = self
            .resolve_workspace_id(input.workspace_id.as_deref())
            .await
            .ok_or_else(|| {
                Error::Validation(
                    "chart(action=\"render\") requires a resolved workspace_id".to_string(),
                )
            })?;

        let mut scope = AccelerationAnalyticsScope::new(workspace_id);
        scope.project_id = parse_optional_project_id(input.project_id.as_deref())?;

        let range = input
            .range
            .or(input.time_range)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());

        let started = std::time::Instant::now();
        let render = match provider
            .render_chart(AccelerationAnalyticsRenderRequest {
                scope,
                chart_key,
                range,
                granularity: input
                    .granularity
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty()),
                filters: input.filters,
            })
            .await
        {
            Ok(render) => render,
            Err(error) => {
                tracing::warn!(error = %error, "acceleration-analytics: render failed");
                return Ok(ToolResult::with_structured(
                    format!("[ACCELERATION_DEGRADED] analytics render failed: {}", error),
                    serde_json::json!({
                        "stages_used": ["acceleration_analytics"],
                        "available": true,
                        "degraded": true,
                        "error": error.to_string(),
                        "origin": "acceleration_analytics",
                        "marker": "[ACCELERATION_DEGRADED]",
                    }),
                ));
            }
        };
        let elapsed_ms = started.elapsed().as_millis() as u64;
        let point_count: usize = render.series.iter().map(|series| series.points.len()).sum();

        Ok(ToolResult::with_structured(
            format!(
                "[ACCELERATION_ANALYTICS] {} ({}, {}, {} point{}, {}ms). \
                 Served by ContextStream acceleration analytics, not MongoDB Atlas Charts.",
                render.title,
                render.range,
                render.granularity,
                point_count,
                if point_count == 1 { "" } else { "s" },
                elapsed_ms
            ),
            serde_json::json!({
                "stages_used": ["acceleration_analytics"],
                "available": true,
                "chart_key": render.chart_key,
                "title": render.title,
                "range": render.range,
                "granularity": render.granularity,
                "source": render.source,
                "series": render.series,
                "generated_at": render.generated_at.to_rfc3339(),
                "degraded": render.degraded,
                "note": render.note,
                "elapsed_ms": elapsed_ms,
                "origin": "acceleration_analytics",
                "marker": "[ACCELERATION_ANALYTICS]",
            }),
        ))
    }

    async fn list_acceleration_action(
        &self,
        provider: Arc<dyn mcp_types::acceleration_layer::AnalyticsProvider>,
    ) -> Result<ToolResult> {
        let charts = match provider.list_charts().await {
            Ok(charts) => charts,
            Err(error) => {
                tracing::warn!(error = %error, "acceleration-analytics: list failed");
                return Ok(ToolResult::with_structured(
                    format!(
                        "[ACCELERATION_DEGRADED] analytics chart list failed: {}",
                        error
                    ),
                    serde_json::json!({
                        "stages_used": ["acceleration_analytics"],
                        "available": true,
                        "degraded": true,
                        "error": error.to_string(),
                        "origin": "acceleration_analytics",
                        "marker": "[ACCELERATION_DEGRADED]",
                    }),
                ));
            }
        };

        let mut header = format!(
            "[ACCELERATION_ANALYTICS] {} chart{} configured.",
            charts.len(),
            if charts.len() == 1 { "" } else { "s" }
        );
        if !charts.is_empty() {
            header.push('\n');
            for chart in &charts {
                header.push_str(&format!("  - {} — {}\n", chart.chart_key, chart.title));
            }
        }

        Ok(ToolResult::with_structured(
            header,
            serde_json::json!({
                "stages_used": ["acceleration_analytics"],
                "available": true,
                "configured_count": charts.len(),
                "charts": charts,
                "origin": "acceleration_analytics",
                "marker": "[ACCELERATION_ANALYTICS]",
            }),
        ))
    }
}

/// Build the standard [`ToolResult`] returned when no supported chart
/// provider can satisfy a request.
fn charts_unavailable(layer: &AtlasLayer, acceleration_layer: &AccelerationLayer) -> ToolResult {
    let note = if acceleration_layer.is_enabled() {
        "[ACCELERATION_DEGRADED] Analytics provider is not configured. Chart rendering is unavailable in this deployment."
    } else if layer.is_enabled() {
        "[CHART] disabled (legacy chart provider unavailable for this deployment)"
    } else {
        "[ACCELERATION_DEGRADED] Analytics provider is not configured. Chart rendering is unavailable in this deployment."
    };
    ToolResult::with_structured(
        note,
        serde_json::json!({
            "stages_used": ["acceleration_analytics"],
            "available": false,
            "degraded": true,
            "results": [],
            "marker": "[ACCELERATION_DEGRADED]",
        }),
    )
}

fn parse_optional_project_id(raw: Option<&str>) -> Result<Option<Uuid>> {
    match raw.map(str::trim).filter(|value| !value.is_empty()) {
        Some(project) => Uuid::parse_str(project)
            .map(Some)
            .map_err(|_| Error::Validation(format!("chart: invalid project_id `{project}`"))),
        None => Ok(None),
    }
}

#[async_trait]
impl ToolHandler for AtlasChartTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let parsed: AtlasChartInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;

        let action = parsed
            .action
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("render");

        match action {
            "render" => self.render_action(parsed).await,
            "list" => self.list_action().await,
            other => Err(Error::Validation(format!(
                "chart: unknown action `{}`. Valid: render, list",
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
                "Render an allowlisted ContextStream acceleration analytics chart scoped \
                 to the calling workspace, or list available charts.",
            )
            .string(
                "action",
                "`render` (default) returns rollup series data; `list` enumerates \
                 configured analytics charts.",
                false,
            )
            .string(
                "chart",
                "Required for action=render. One of: tool_latency_p95, \
                 acceleration_cache_hit_rate, provider_degraded_rate, \
                 archive_search_health, signal_emit_health.",
                false,
            )
            .string(
                "time_range",
                "Deprecated alias for range (e.g. 24h, 7d, 30d).",
                false,
            )
            .string(
                "range",
                "Optional range (e.g. 24h, 7d, 30d). Defaults to the chart definition.",
                false,
            )
            .string(
                "granularity",
                "Optional rollup granularity: minute, hour, or day.",
                false,
            )
            .object(
                "filters",
                "Optional allowlisted dimension filters, e.g. {\"tool_name\":\"context\"}.",
                false,
            )
            .uuid(
                "workspace_id",
                "Workspace ID (falls back to session state)",
                false,
            )
            .uuid(
                "project_id",
                "Project ID (passed through to chart filter)",
                false,
            )
            .build()
    }
}

/// Register the `chart` tool when either the acceleration analytics
/// API or the temporary Atlas Charts fallback is configured. The
/// canonical tool name is `chart`; `atlas_chart` remains callable as a
/// canary-window alias for supported remote clients.
pub fn register_charts_tools(
    registry: &mut crate::registry::ToolRegistry,
    session: Arc<SessionManager>,
) {
    let atlas_layer = registry.atlas_layer().clone();
    let acceleration_layer = registry.acceleration_layer().clone();
    if !atlas_layer.has_connection() && !acceleration_layer.has_connection() {
        tracing::debug!(
            "chart: skipping registration (no acceleration analytics or legacy charts connection)"
        );
        return;
    }
    let tool = Arc::new(AtlasChartTool::with_acceleration(
        atlas_layer,
        acceleration_layer,
        session,
    ));
    registry.register("chart", tool.clone());
    registry.register("atlas_chart", tool);
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_types::acceleration_layer::{
        AccelerationAnalyticsChart, AccelerationAnalyticsError, AccelerationAnalyticsPoint,
        AccelerationAnalyticsRender, AccelerationAnalyticsRenderRequest,
        AccelerationAnalyticsSeries, AnalyticsProvider, McpAccelerationLayer,
    };
    use mcp_types::atlas_layer::{
        noop_layer, AtlasChartEmbed, AtlasChartError, AtlasChartId, AtlasChartScope,
        AtlasChartsProvider, AtlasProductHealth, AtlasProductId, AtlasProductLayer,
    };
    use std::sync::Arc;

    /// Minimal in-memory layer that exposes a Charts provider and
    /// reports `is_enabled()` so we can exercise the tool handler in
    /// isolation without bringing in MongoDB.
    struct FakeLayer {
        provider: Arc<dyn AtlasChartsProvider>,
    }

    impl AtlasProductLayer for FakeLayer {
        fn has_connection(&self) -> bool {
            true
        }

        fn is_enabled(&self) -> bool {
            true
        }

        fn available_products(&self) -> Vec<AtlasProductId> {
            vec![AtlasProductId::Charts]
        }

        fn health(&self, product: AtlasProductId) -> AtlasProductHealth {
            match product {
                AtlasProductId::Charts => AtlasProductHealth::available(product),
                other => AtlasProductHealth::not_available(other, "fake layer"),
            }
        }

        fn charts(&self) -> Option<Arc<dyn AtlasChartsProvider>> {
            Some(self.provider.clone())
        }
    }

    struct FakeProvider {
        configured: Vec<AtlasChartId>,
    }

    #[async_trait]
    impl AtlasChartsProvider for FakeProvider {
        async fn render_chart(
            &self,
            chart: AtlasChartId,
            scope: &AtlasChartScope,
        ) -> std::result::Result<AtlasChartEmbed, AtlasChartError> {
            if !self.configured.contains(&chart) {
                return Err(AtlasChartError::UnknownChart(
                    chart.as_str().to_string(),
                    chart.as_str().to_ascii_uppercase(),
                ));
            }
            Ok(AtlasChartEmbed {
                chart: chart.as_str().to_string(),
                chart_id: format!("uuid-for-{}", chart.as_str()),
                embed_url: format!(
                    "https://charts.test/embed/charts?id=uuid-for-{}&signedToken=tok",
                    chart.as_str()
                ),
                embedding_token: "tok".to_string(),
                expires_at: 1_700_000_000,
                applied_filter: serde_json::json!({
                    "workspace_id": scope.workspace_id.to_string(),
                }),
                description: chart.description().to_string(),
            })
        }

        fn configured_charts(&self) -> Vec<AtlasChartId> {
            self.configured.clone()
        }
    }

    fn fake_session() -> Arc<SessionManager> {
        let cfg = mcp_types::config::Config::default();
        let client = mcp_client::ContextStreamClient::new(cfg.clone());
        Arc::new(SessionManager::new(client, cfg))
    }

    fn build_tool(configured: Vec<AtlasChartId>) -> AtlasChartTool {
        let provider: Arc<dyn AtlasChartsProvider> = Arc::new(FakeProvider { configured });
        let layer: AtlasLayer = Arc::new(FakeLayer { provider });
        AtlasChartTool::new(layer, fake_session())
    }

    struct FakeAccelerationLayer {
        provider: Arc<dyn AnalyticsProvider>,
    }

    impl McpAccelerationLayer for FakeAccelerationLayer {
        fn has_connection(&self) -> bool {
            true
        }

        fn is_enabled(&self) -> bool {
            true
        }

        fn analytics(&self) -> Option<Arc<dyn AnalyticsProvider>> {
            Some(self.provider.clone())
        }
    }

    struct FakeAnalyticsProvider {
        fail_render: bool,
    }

    #[async_trait]
    impl AnalyticsProvider for FakeAnalyticsProvider {
        async fn list_charts(
            &self,
        ) -> std::result::Result<Vec<AccelerationAnalyticsChart>, AccelerationAnalyticsError>
        {
            Ok(vec![AccelerationAnalyticsChart {
                chart_key: "tool_latency_p95".to_string(),
                title: "Tool latency p95".to_string(),
                description: Some("Latency by tool".to_string()),
                metric: "tool_latency_ms".to_string(),
                allowed_dimensions: serde_json::json!(["tool_name"]),
                default_range: "24h".to_string(),
                default_granularity: "hour".to_string(),
            }])
        }

        async fn render_chart(
            &self,
            request: AccelerationAnalyticsRenderRequest,
        ) -> std::result::Result<AccelerationAnalyticsRender, AccelerationAnalyticsError> {
            if self.fail_render {
                return Err(AccelerationAnalyticsError::Request(
                    "rollup query failed".to_string(),
                ));
            }
            Ok(AccelerationAnalyticsRender {
                chart_key: request.chart_key,
                title: "Tool latency p95".to_string(),
                range: request.range.unwrap_or_else(|| "24h".to_string()),
                granularity: request.granularity.unwrap_or_else(|| "hour".to_string()),
                source: "postgres_rollup".to_string(),
                series: vec![AccelerationAnalyticsSeries {
                    name: "context".to_string(),
                    points: vec![AccelerationAnalyticsPoint {
                        t: chrono::Utc::now(),
                        value: 42.0,
                    }],
                }],
                generated_at: chrono::Utc::now(),
                degraded: false,
                note: None,
            })
        }
    }

    fn acceleration_layer(fail_render: bool) -> AccelerationLayer {
        Arc::new(FakeAccelerationLayer {
            provider: Arc::new(FakeAnalyticsProvider { fail_render }),
        })
    }

    #[tokio::test]
    async fn render_returns_embed_url_for_configured_chart() {
        let tool = build_tool(vec![AtlasChartId::SearchVolumeTimeline]);
        let ws = Uuid::new_v4();
        let input = serde_json::json!({
            "action": "render",
            "chart": "search_volume_timeline",
            "workspace_id": ws.to_string(),
            "time_range": "last_30d",
        });
        let result = tool.execute(input).await.unwrap();
        assert!(!result.is_error);
        let structured = result.structured_content.expect("structured payload");
        assert_eq!(structured["chart"].as_str(), Some("search_volume_timeline"));
        assert!(structured["embed_url"]
            .as_str()
            .unwrap()
            .contains("signedToken="));
        assert_eq!(structured["available"], serde_json::json!(true));
        assert_eq!(structured["origin"], serde_json::json!("charts"));
    }

    #[tokio::test]
    async fn render_errors_for_unknown_chart_string() {
        let tool = build_tool(vec![AtlasChartId::SearchVolumeTimeline]);
        let ws = Uuid::new_v4();
        let input = serde_json::json!({
            "action": "render",
            "chart": "totally_made_up",
            "workspace_id": ws.to_string(),
        });
        let err = tool.execute(input).await.unwrap_err();
        match err {
            Error::Validation(msg) => {
                assert!(msg.contains("unknown chart"));
                assert!(msg.contains("search_volume_timeline"));
            }
            other => panic!("expected Validation, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn render_errors_when_workspace_id_unresolvable() {
        let tool = build_tool(vec![AtlasChartId::SearchVolumeTimeline]);
        let input = serde_json::json!({
            "action": "render",
            "chart": "search_volume_timeline",
        });
        let err = tool.execute(input).await.unwrap_err();
        match err {
            Error::Validation(msg) => assert!(msg.contains("workspace_id")),
            other => panic!("expected Validation, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn render_surfaces_unknown_chart_provider_error_as_degraded_response() {
        // Only HotFiles is provisioned operator-side; the request asks
        // for SearchVolumeTimeline, so the provider returns
        // `UnknownChart`. The tool should NOT raise an error — it
        // should report the failure in-band so agents can ask for a
        // different chart.
        let tool = build_tool(vec![AtlasChartId::HotFiles]);
        let ws = Uuid::new_v4();
        let input = serde_json::json!({
            "action": "render",
            "chart": "search_volume_timeline",
            "workspace_id": ws.to_string(),
        });
        let result = tool.execute(input).await.unwrap();
        assert!(!result.is_error);
        assert!(result.content.iter().any(|c| matches!(
            c,
            mcp_types::tool::ContentItem::Text { text } if text.contains("[CHART] render failed")
        )));
    }

    #[tokio::test]
    async fn list_reports_configured_and_unconfigured_charts() {
        let tool = build_tool(vec![
            AtlasChartId::HotFiles,
            AtlasChartId::DecisionLessonDensity,
        ]);
        let result = tool
            .execute(serde_json::json!({"action": "list"}))
            .await
            .unwrap();
        assert!(!result.is_error);
        let structured = result.structured_content.expect("structured payload");
        assert_eq!(structured["configured_count"], serde_json::json!(2));
        assert_eq!(structured["total_count"], serde_json::json!(5));
        let charts = structured["charts"].as_array().unwrap();
        let hot = charts
            .iter()
            .find(|c| c["chart"] == serde_json::json!("hot_files"))
            .unwrap();
        assert_eq!(hot["configured"], serde_json::json!(true));
        let archived = charts
            .iter()
            .find(|c| c["chart"] == serde_json::json!("dependency_graph_snapshot"))
            .unwrap();
        assert_eq!(archived["configured"], serde_json::json!(false));
    }

    #[tokio::test]
    async fn render_prefers_acceleration_analytics() {
        let tool = AtlasChartTool::with_acceleration(
            noop_layer(),
            acceleration_layer(false),
            fake_session(),
        );
        let ws = Uuid::new_v4();
        let result = tool
            .execute(serde_json::json!({
                "action": "render",
                "chart": "tool_latency_p95",
                "workspace_id": ws.to_string(),
                "range": "24h",
                "filters": {"tool_name": "context"},
            }))
            .await
            .unwrap();

        assert!(!result.is_error);
        let structured = result.structured_content.expect("structured payload");
        assert_eq!(
            structured["origin"],
            serde_json::json!("acceleration_analytics")
        );
        assert_eq!(
            structured["marker"],
            serde_json::json!("[ACCELERATION_ANALYTICS]")
        );
        assert_eq!(
            structured["chart_key"],
            serde_json::json!("tool_latency_p95")
        );
        assert!(structured["embed_url"].is_null());
    }

    #[tokio::test]
    async fn acceleration_render_failure_does_not_fallback_to_atlas() {
        let atlas_provider: Arc<dyn AtlasChartsProvider> = Arc::new(FakeProvider {
            configured: vec![AtlasChartId::SearchVolumeTimeline],
        });
        let atlas_layer: AtlasLayer = Arc::new(FakeLayer {
            provider: atlas_provider,
        });
        let tool = AtlasChartTool::with_acceleration(
            atlas_layer,
            acceleration_layer(true),
            fake_session(),
        );
        let ws = Uuid::new_v4();
        let result = tool
            .execute(serde_json::json!({
                "action": "render",
                "chart": "tool_latency_p95",
                "workspace_id": ws.to_string(),
            }))
            .await
            .unwrap();

        assert!(!result.is_error);
        let structured = result.structured_content.expect("structured payload");
        assert_eq!(
            structured["origin"],
            serde_json::json!("acceleration_analytics")
        );
        assert_eq!(
            structured["marker"],
            serde_json::json!("[ACCELERATION_DEGRADED]")
        );
        assert!(structured["embed_url"].is_null());
    }

    #[tokio::test]
    async fn list_uses_acceleration_analytics() {
        let tool = AtlasChartTool::with_acceleration(
            noop_layer(),
            acceleration_layer(false),
            fake_session(),
        );
        let result = tool
            .execute(serde_json::json!({"action": "list"}))
            .await
            .unwrap();
        let structured = result.structured_content.expect("structured payload");
        assert_eq!(
            structured["origin"],
            serde_json::json!("acceleration_analytics")
        );
        assert_eq!(structured["configured_count"], serde_json::json!(1));
        assert_eq!(
            structured["charts"][0]["chart_key"],
            serde_json::json!("tool_latency_p95")
        );
    }

    #[tokio::test]
    async fn unknown_action_returns_validation_error() {
        let tool = build_tool(vec![]);
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
    fn charts_unavailable_when_layer_disabled_skips_registration() {
        // Build a registry with the default no-op layer and ensure
        // register_charts_tools doesn't add the tool.
        let cfg = mcp_types::config::Config::default();
        let mut registry = crate::registry::ToolRegistry::new(&cfg);
        // Default layer is no-op; do NOT call set_atlas_layer.
        register_charts_tools(&mut registry, fake_session());
        assert!(
            registry.get("atlas_chart").is_none(),
            "atlas_chart tool should not be registered when atlas layer is the no-op layer"
        );
    }

    #[test]
    fn charts_registered_when_layer_enabled() {
        let provider: Arc<dyn AtlasChartsProvider> = Arc::new(FakeProvider {
            configured: vec![AtlasChartId::HotFiles],
        });
        let layer: AtlasLayer = Arc::new(FakeLayer { provider });
        let cfg = mcp_types::config::Config::default();
        let mut registry = crate::registry::ToolRegistry::new(&cfg);
        registry.set_atlas_layer(layer);
        register_charts_tools(&mut registry, fake_session());
        assert!(registry.get("atlas_chart").is_some());
    }

    #[test]
    fn charts_registered_when_acceleration_connection_exists() {
        let cfg = mcp_types::config::Config::default();
        let mut registry = crate::registry::ToolRegistry::new(&cfg);
        registry.set_acceleration_layer(acceleration_layer(false));
        register_charts_tools(&mut registry, fake_session());
        assert!(registry.get("chart").is_some());
        assert!(registry.get("atlas_chart").is_some());
    }
}
