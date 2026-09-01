//! Media asset tools: index, status, search, get_clip, list, delete.

use async_trait::async_trait;
use mcp_client::{ContextStreamClient, MediaGetClipParams, MediaIndexParams, MediaSearchParams};
use mcp_session::SessionManager;
use mcp_types::{
    tool::{ToolAnnotations, ToolCategory, ToolMetadata, ToolResult},
    Error, Result,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;
use std::sync::Arc;
use tracing::debug;
use uuid::Uuid;

use crate::registry::ToolHandler;
use crate::schema::SchemaBuilder;

// ============================================================================
// Valid Constants
// ============================================================================

/// Valid actions.
const VALID_ACTIONS: &[&str] = &["index", "status", "search", "get_clip", "list", "delete"];

/// Valid canonical content types accepted by the API.
const VALID_CONTENT_TYPES: &[&str] = &["video", "audio", "image", "document"];

/// Valid output formats.
const VALID_OUTPUT_FORMATS: &[&str] = &["remotion", "ffmpeg", "raw"];
const MEDIA_SUMMARY_LIMIT: usize = 8;
const MEDIA_DOWNLOAD_URL_LIMIT: usize = 3;

// ============================================================================
// Unified Media Tool
// ============================================================================

/// Input for the unified media tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaInput {
    pub action: String,
    // Common fields
    pub workspace_id: Option<String>,
    pub project_id: Option<String>,
    pub target_project: Option<String>,
    // Index fields
    pub file_path: Option<String>,
    pub external_url: Option<String>,
    pub content_type: Option<String>,
    pub tags: Option<Vec<String>>,
    /// Opt-in to index a `file_path` that lives outside the active project root
    /// (P0 ingestion containment). Also honored via the
    /// `CONTEXTSTREAM_ALLOW_BROAD_INGEST` env var. Does NOT bypass the
    /// secret/credential filter or the size cap. Defaults to false.
    pub allow_broad: Option<bool>,
    // Status/get_clip/delete fields
    pub content_id: Option<String>,
    // Search fields
    pub query: Option<String>,
    pub content_types: Option<Vec<String>>,
    pub limit: Option<i64>,
    // Clip fields
    pub start: Option<String>,
    pub end: Option<String>,
    pub output_format: Option<String>,
    pub fps: Option<i64>,
}

/// Unified media tool handler.
pub struct MediaTool {
    client: ContextStreamClient,
    session: Arc<SessionManager>,
}

impl MediaTool {
    pub fn new(client: ContextStreamClient, session: Arc<SessionManager>) -> Self {
        Self { client, session }
    }

    async fn resolve_workspace_id(&self, input: &Option<String>) -> Result<Option<Uuid>> {
        match input.as_deref().map(str::trim) {
            Some("") => Ok(self.session.state().await.workspace_id),
            Some(s) => {
                Ok(Some(Uuid::parse_str(s).map_err(|_| {
                    Error::Validation("Invalid workspace_id".to_string())
                })?))
            }
            None => Ok(self.session.state().await.workspace_id),
        }
    }

    async fn resolve_project_id(
        &self,
        project_id: &Option<String>,
        target_project: &Option<String>,
    ) -> Result<Option<Uuid>> {
        match project_id.as_deref().map(str::trim) {
            Some("") | None => {}
            Some(s) => {
                return Ok(Some(Uuid::parse_str(s).map_err(|_| {
                    Error::Validation("Invalid project_id".to_string())
                })?));
            }
        }

        if let Some(target_name) = target_project
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            if !self.session.has_child_projects().await {
                return Err(Error::Validation(format!(
                    "target_project '{}' requires init from a multi-project parent folder first",
                    target_name
                )));
            }
            if let Some(child) = self
                .session
                .resolve_child_project_by_name(target_name)
                .await
            {
                return Ok(Some(Uuid::parse_str(&child.project_id).map_err(|_| {
                    Error::Validation(format!(
                        "Resolved child project '{}' has an invalid project_id",
                        target_name
                    ))
                })?));
            }
            let mut available = self
                .session
                .get_child_projects()
                .await
                .into_keys()
                .collect::<Vec<_>>();
            available.sort();
            return Err(Error::Validation(format!(
                "Unknown target_project '{}'. Available child projects: {}",
                target_name,
                if available.is_empty() {
                    "none".to_string()
                } else {
                    available.join(", ")
                }
            )));
        }

        Ok(self.session.state().await.project_id)
    }

    fn parse_content_id(input: &Option<String>) -> Result<Uuid> {
        match input {
            Some(s) => {
                Uuid::parse_str(s).map_err(|_| Error::Validation("Invalid content_id".to_string()))
            }
            None => Err(Error::Validation("content_id is required".to_string())),
        }
    }

    fn normalize_content_type_label(value: &str) -> Result<String> {
        let normalized = value.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "video" | "videos" | "movie" | "movies" | "clip" | "clips" | "footage" => {
                Ok("video".to_string())
            }
            "audio" | "sound" | "voice" | "recording" | "recordings" | "podcast"
            | "podcasts" => Ok("audio".to_string()),
            "image" | "images" | "photo" | "photos" | "picture" | "pictures"
            | "screenshot" | "screenshots" | "png" | "jpg" | "jpeg" | "gif" | "webp" => {
                Ok("image".to_string())
            }
            "document" | "documents" | "doc" | "docs" | "pdf" | "pdfs" | "slide" | "slides"
            | "presentation" | "presentations" | "deck" | "decks" | "docx" | "pptx" => {
                Ok("document".to_string())
            }
            "" => Err(Error::Validation("content_type cannot be empty".to_string())),
            other => Err(Error::Validation(format!(
                "Invalid content_type '{}'. Use one of: {}. Friendly aliases are supported: photos/images -> image, docs/PDFs/slides -> document.",
                other,
                VALID_CONTENT_TYPES.join(", ")
            ))),
        }
    }

    fn normalize_optional_content_type(input: &Option<String>) -> Result<Option<String>> {
        input
            .as_deref()
            .map(Self::normalize_content_type_label)
            .transpose()
    }

    fn normalize_content_type_filters(input: &Option<Vec<String>>) -> Result<Option<Vec<String>>> {
        let Some(values) = input else {
            return Ok(None);
        };

        let mut seen = HashSet::new();
        let mut normalized = Vec::new();
        for value in values {
            let content_type = Self::normalize_content_type_label(value)?;
            if seen.insert(content_type.clone()) {
                normalized.push(content_type);
            }
        }

        if normalized.is_empty() {
            Ok(None)
        } else {
            Ok(Some(normalized))
        }
    }

    fn video_text_extraction_summary(result: &Value) -> Option<String> {
        let extraction = result.get("metadata")?.get("video_text_extraction")?;
        let provider = extraction
            .get("provider")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let status = extraction
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        let mut parts = vec![format!("provider={}, status={}", provider, status)];

        if let Some(segments) = extraction.get("segments_count").and_then(|v| v.as_u64()) {
            parts.push(format!("segments={}", segments));
        }

        if let Some(job_id) = extraction.get("job_id").and_then(|v| v.as_str()) {
            if !job_id.is_empty() {
                parts.push(format!("job_id={}", job_id));
            }
        }

        if let Some(doc_id) = extraction.get("doc_id").and_then(|v| v.as_str()) {
            if !doc_id.is_empty() {
                parts.push(format!("doc_id={}", doc_id));
            }
        }

        if let Some(profile) = extraction.get("aws_profile").and_then(|v| v.as_str()) {
            if !profile.is_empty() {
                parts.push(format!("aws_profile={}", profile));
            }
        }

        Some(parts.join(", "))
    }

    fn normalize_media_collection(result: Value) -> Value {
        if result.is_array() {
            return result;
        }

        if let Some(items) = result.get("items").and_then(|value| value.as_array()) {
            return Value::Array(items.clone());
        }

        if let Some(items) = result.get("results").and_then(|value| value.as_array()) {
            return Value::Array(items.clone());
        }

        result
    }

    fn string_field(item: &Value, keys: &[&str]) -> Option<String> {
        for key in keys {
            if let Some(value) = item.get(*key).and_then(|v| v.as_str()) {
                let trimmed = value.trim();
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
        }
        None
    }

    fn number_field(item: &Value, keys: &[&str]) -> Option<f64> {
        keys.iter()
            .find_map(|key| item.get(*key).and_then(|v| v.as_f64()))
    }

    fn content_id_from_item(item: &Value) -> Option<Uuid> {
        Self::string_field(item, &["content_id", "id"])
            .and_then(|value| Uuid::parse_str(&value).ok())
    }

    fn tags_summary(item: &Value) -> Option<String> {
        let tags = item.get("tags")?.as_array()?;
        let values = tags
            .iter()
            .filter_map(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .take(5)
            .collect::<Vec<_>>();
        if values.is_empty() {
            None
        } else {
            Some(values.join(","))
        }
    }

    fn truncate_for_summary(value: &str, max_chars: usize) -> String {
        let trimmed = value.trim();
        if trimmed.chars().count() <= max_chars {
            return trimmed.to_string();
        }
        let mut out = trimmed
            .chars()
            .take(max_chars.saturating_sub(3))
            .collect::<String>();
        out.push_str("...");
        out
    }

    fn existing_media_url(item: &Value) -> Option<String> {
        Self::string_field(
            item,
            &[
                "download_url",
                "external_url",
                "source_url",
                "url",
                "thumbnail_url",
                "preview_url",
            ],
        )
    }

    async fn download_url_for_item(
        &self,
        workspace_id: Option<Uuid>,
        item: &Value,
    ) -> Option<(String, Option<u64>)> {
        if let Some(url) = Self::existing_media_url(item) {
            return Some((url, None));
        }

        let content_id = Self::content_id_from_item(item)?;
        let result = self
            .client
            .media_download_url(workspace_id, content_id)
            .await
            .ok()?;
        let url = Self::string_field(&result, &["download_url"])?;
        let expires = result.get("expires_in_seconds").and_then(|v| v.as_u64());
        Some((url, expires))
    }

    fn media_item_summary_line(
        index: usize,
        item: &Value,
        url: Option<&str>,
        expires_in_seconds: Option<u64>,
    ) -> String {
        let content_id = Self::string_field(item, &["content_id", "id"])
            .unwrap_or_else(|| "unknown".to_string());
        let filename = Self::string_field(item, &["filename", "name"])
            .unwrap_or_else(|| "unnamed".to_string());
        let title = Self::string_field(item, &["title"]);
        let content_type = Self::string_field(item, &["content_type", "type"])
            .unwrap_or_else(|| "unknown".to_string());
        let status = Self::string_field(item, &["status"]);
        let project_id = Self::string_field(item, &["project_id"]);
        let score = Self::number_field(item, &["score"]);
        let match_text = Self::string_field(item, &["match_text", "text", "transcript"]);

        let mut line = format!(
            "{}. {} (content_id={}, type={}",
            index, filename, content_id, content_type
        );
        if let Some(status) = status {
            line.push_str(&format!(", status={}", status));
        }
        if let Some(score) = score {
            line.push_str(&format!(", score={:.3}", score));
        }
        if let Some(project_id) = project_id {
            line.push_str(&format!(", project_id={}", project_id));
        }
        if let Some(tags) = Self::tags_summary(item) {
            line.push_str(&format!(", tags={}", tags));
        }
        line.push(')');

        if let Some(title) = title.filter(|value| value != &filename) {
            line.push_str(&format!(
                "\n   title: {}",
                Self::truncate_for_summary(&title, 120)
            ));
        }
        if let Some(match_text) = match_text {
            line.push_str(&format!(
                "\n   match: {}",
                Self::truncate_for_summary(&match_text, 180)
            ));
        }
        if let Some(url) = url {
            line.push_str(&format!("\n   use_url: {}", url));
            if let Some(expires) = expires_in_seconds {
                line.push_str(&format!(" (expires_in_seconds={})", expires));
            }
        }
        if content_id != "unknown" {
            line.push_str(&format!(
                "\n   status_call: media(action=\"status\", content_id=\"{}\")",
                content_id
            ));
        }

        line
    }

    async fn media_collection_summary(
        &self,
        label: &str,
        result: &Value,
        workspace_id: Option<Uuid>,
    ) -> String {
        let Some(items) = result.as_array() else {
            return String::new();
        };
        if items.is_empty() {
            return String::new();
        }

        let shown = items.len().min(MEDIA_SUMMARY_LIMIT);
        let mut lines = vec![format!("Top {} {}:", shown, label)];
        for (idx, item) in items.iter().take(shown).enumerate() {
            let download = if idx < MEDIA_DOWNLOAD_URL_LIMIT {
                self.download_url_for_item(workspace_id, item).await
            } else {
                None
            };
            let (url, expires) = download
                .as_ref()
                .map(|(url, expires)| (Some(url.as_str()), *expires))
                .unwrap_or((None, None));
            lines.push(Self::media_item_summary_line(idx + 1, item, url, expires));
        }
        if items.len() > shown {
            lines.push(format!(
                "... {} more not shown; rerun with a narrower query or lower limit.",
                items.len() - shown
            ));
        }
        lines.push(
            "Use a `use_url` to download/copy the asset into the repo when the app needs a static image; use `content_id` for follow-up media status/details.".to_string(),
        );
        lines.join("\n")
    }
}

#[async_trait]
impl ToolHandler for MediaTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let input: MediaInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;

        let action = input.action.to_lowercase();
        let explicit_project_scope = input
            .project_id
            .as_deref()
            .map(str::trim)
            .is_some_and(|value| !value.is_empty())
            || input
                .target_project
                .as_deref()
                .map(str::trim)
                .is_some_and(|value| !value.is_empty());
        let workspace_id = self.resolve_workspace_id(&input.workspace_id).await?;
        let project_id = self
            .resolve_project_id(&input.project_id, &input.target_project)
            .await?;
        let content_type = Self::normalize_optional_content_type(&input.content_type)?;
        let content_type_filters = Self::normalize_content_type_filters(&input.content_types)?;
        let related_project_ids = self
            .session
            .get_project_relations()
            .await
            .values()
            .filter_map(|project| Uuid::parse_str(&project.project_id).ok())
            .collect::<Vec<_>>();

        match action.as_str() {
            "index" => {
                if input.file_path.is_none() && input.external_url.is_none() {
                    return Err(Error::Validation(
                        "Either file_path or external_url is required for index".to_string(),
                    ));
                }

                // P0 ingestion-containment: validate the source before any
                // upload. Local files must be a regular file within the size cap,
                // must not be a secret/credential, and must live inside the
                // active project root (unless explicitly opted in). External URLs
                // must use http(s) and must not point at private/loopback/
                // link-local/metadata addresses (SSRF guard).
                let opt_in = input.allow_broad.unwrap_or(false)
                    || mcp_client::broad_ingest_opt_in_from_env();
                if let Some(file_path) = input.file_path.as_deref() {
                    let project_root = self.session.state().await.folder_path;
                    let root_path: Option<&Path> = project_root.as_deref().map(Path::new);
                    validate_media_index_file(Path::new(file_path), root_path, opt_in)?;
                }
                if let Some(external_url) = input.external_url.as_deref() {
                    validate_external_media_url(external_url)?;
                }

                let params = MediaIndexParams {
                    file_path: input.file_path,
                    external_url: input.external_url,
                    content_type,
                    tags: input.tags,
                    workspace_id,
                    project_id,
                };
                let result = self.client.media_index(params).await?;
                let content_id = result
                    .get("content_id")
                    .or_else(|| result.get("id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let status = result
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let mut message = format!(
                    "Media indexed with content_id: {} (status: {}). Next: call media(action=\"status\", content_id=\"{}\") until indexed, then media(action=\"search\", query=\"...\") or media(action=\"list\").",
                    content_id, status, content_id
                );
                if let Some(summary) = Self::video_text_extraction_summary(&result) {
                    message.push_str(&format!(" | video_text_extraction: {}", summary));
                }
                Ok(ToolResult::with_structured(message, result))
            }

            "status" => {
                let content_id = Self::parse_content_id(&input.content_id)?;
                let result = self.client.media_status(workspace_id, content_id).await?;
                let status = result
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let mut message = format!("Media status: {}", status);
                if matches!(status, "uploaded" | "processing" | "pending") {
                    message.push_str(
                        " (still indexing; continue polling status, or use media(action=\"list\") to view recent uploads).",
                    );
                }
                if status == "indexed" {
                    message.push_str(" (ready for media(action=\"search\")).");
                }
                if let Some(summary) = Self::video_text_extraction_summary(&result) {
                    message.push_str(&format!(" | video_text_extraction: {}", summary));
                }
                let download = self.download_url_for_item(workspace_id, &result).await;
                let (url, expires) = download
                    .as_ref()
                    .map(|(url, expires)| (Some(url.as_str()), *expires))
                    .unwrap_or((None, None));
                message.push_str(&format!(
                    "\n{}",
                    Self::media_item_summary_line(1, &result, url, expires)
                ));
                Ok(ToolResult::with_structured(message, result))
            }

            "search" => {
                let query = input
                    .query
                    .ok_or_else(|| Error::Validation("query is required for search".to_string()))?;

                // Rollout logging (requirement #11)
                crate::domains::scope::log_mcp_request(
                    "media.search",
                    "workspaces/content/search",
                    workspace_id,
                    project_id,
                    &query,
                );

                let params = MediaSearchParams {
                    query: query.clone(),
                    content_types: content_type_filters.clone(),
                    limit: input.limit,
                    workspace_id,
                    project_id,
                };
                let mut result = Self::normalize_media_collection(
                    self.client.media_search(params.clone()).await?,
                );
                let mut note: Option<String> = None;
                let has_hits = result.as_array().map(|a| !a.is_empty()).unwrap_or(false);

                // Rollout logging for response
                crate::domains::scope::log_mcp_response_scope(
                    "media.search",
                    Some(true),
                    None,
                    result.as_array().map(|a| a.len()).unwrap_or(0),
                );

                if !has_hits && !explicit_project_scope {
                    // Try related projects first
                    for related_project_id in &related_project_ids {
                        if Some(*related_project_id) == project_id {
                            continue;
                        }
                        let mut retry = params.clone();
                        retry.project_id = Some(*related_project_id);
                        let candidate = Self::normalize_media_collection(
                            self.client.media_search(retry).await?,
                        );
                        if candidate.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
                            result = candidate;
                            note = Some(format!(
                                "Project-first scope returned no media hits; auto-expanded to related project `{}`.",
                                related_project_id
                            ));
                            break;
                        }
                    }
                }

                let count = result.as_array().map(|a| a.len()).unwrap_or(0);
                let mut message = format!("Found {} media results.", count);
                if count == 0 {
                    message.push_str(
                        " No indexed media matched yet. If this was recently uploaded, call media(action=\"list\") and media(action=\"status\", content_id=\"...\") while indexing completes.",
                    );
                } else {
                    let summary = self
                        .media_collection_summary("media result(s)", &result, workspace_id)
                        .await;
                    if !summary.is_empty() {
                        message.push_str(&format!("\n{}", summary));
                    }
                }
                let mut output = ToolResult::with_structured(message, result);
                if let Some(scope_note) = note {
                    output = output.with_prefix(format!("{}\n", scope_note));
                }
                Ok(output)
            }

            "get_clip" => {
                let content_id = Self::parse_content_id(&input.content_id)?;
                let start = input.start.ok_or_else(|| {
                    Error::Validation("start is required for get_clip".to_string())
                })?;
                let end = input
                    .end
                    .ok_or_else(|| Error::Validation("end is required for get_clip".to_string()))?;

                let params = MediaGetClipParams {
                    content_id,
                    start,
                    end,
                    output_format: input.output_format,
                    fps: input.fps,
                    workspace_id,
                };
                let result = self.client.media_get_clip(params).await?;
                Ok(ToolResult::with_structured(
                    "Clip details retrieved.".to_string(),
                    result,
                ))
            }

            "list" => {
                let content_types: Option<Vec<&str>> = content_type_filters
                    .as_ref()
                    .map(|types| types.iter().map(|s| s.as_str()).collect());

                // Rollout logging (requirement #11)
                crate::domains::scope::log_mcp_request(
                    "media.list",
                    "workspaces/content",
                    workspace_id,
                    project_id,
                    "",
                );

                let mut result = self
                    .client
                    .media_list(
                        workspace_id,
                        project_id,
                        content_types.as_deref(),
                        input.limit,
                    )
                    .await
                    .map(Self::normalize_media_collection)?;
                let mut note: Option<String> = None;
                let has_hits = result.as_array().map(|a| !a.is_empty()).unwrap_or(false);

                // Rollout logging for response
                crate::domains::scope::log_mcp_response_scope(
                    "media.list",
                    Some(true),
                    None,
                    result.as_array().map(|a| a.len()).unwrap_or(0),
                );

                if !has_hits && !explicit_project_scope {
                    for related_project_id in &related_project_ids {
                        if Some(*related_project_id) == project_id {
                            continue;
                        }
                        let candidate = self
                            .client
                            .media_list(
                                workspace_id,
                                Some(*related_project_id),
                                content_types.as_deref(),
                                input.limit,
                            )
                            .await
                            .map(Self::normalize_media_collection)?;
                        if candidate.as_array().map(|a| !a.is_empty()).unwrap_or(false) {
                            result = candidate;
                            note = Some(format!(
                                "Project-first scope returned no assets; auto-expanded to related project `{}`.",
                                related_project_id
                            ));
                            break;
                        }
                    }
                }

                // Log the exact response payload when list returns 0 despite assets being visible
                // elsewhere, to help diagnose the mismatch (requirement #8).
                if result.as_array().map(|a| a.is_empty()).unwrap_or(true) {
                    debug!(
                        "[media.list] workspace_id={:?} project_id={:?} returned 0 assets. Response payload: {}",
                        workspace_id,
                        project_id,
                        serde_json::to_string(&result).unwrap_or_else(|_| "serialize_error".into()),
                    );
                }

                let count = result.as_array().map(|a| a.len()).unwrap_or(0);
                let mut message = format!("Found {} media assets.", count);
                if count == 0 {
                    message.push_str(
                        " No media assets are currently visible in this scope. If upload was recent, re-check with media(action=\"status\", content_id=\"...\") once indexing completes.",
                    );
                } else {
                    let summary = self
                        .media_collection_summary("media asset(s)", &result, workspace_id)
                        .await;
                    if !summary.is_empty() {
                        message.push_str(&format!("\n{}", summary));
                    }
                }
                let mut output = ToolResult::with_structured(message, result);
                if let Some(scope_note) = note {
                    output = output.with_prefix(format!("{}\n", scope_note));
                }
                Ok(output)
            }

            "delete" => {
                let content_id = Self::parse_content_id(&input.content_id)?;
                let result = self.client.media_delete(workspace_id, content_id).await?;
                Ok(ToolResult::with_structured(
                    format!("Media {} deleted.", content_id),
                    result,
                ))
            }

            _ => Err(Error::Validation(format!(
                "Unknown action: {}. Available actions: {}",
                action,
                VALID_ACTIONS.join(", ")
            ))),
        }
    }

    fn metadata(&self) -> &ToolMetadata {
        static METADATA: std::sync::OnceLock<ToolMetadata> = std::sync::OnceLock::new();
        METADATA.get_or_init(|| ToolMetadata {
            name: "media".to_string(),
            title: "Media Operations".to_string(),
            description: "Media operations for indexed assets: photos/images, videos, audio, and documents/PDFs. Actions: index (upload URL/local assets and trigger ML processing), status (check processing), search (semantic search with Knowledge Stream fallback), get_clip (extract video/audio clip details with output_format: remotion/ffmpeg/raw), list, delete.".to_string(),
            category: ToolCategory::Ai,
            annotations: ToolAnnotations::destructive(),
            is_pro: true,
            required_tier: None,
        })
    }

    fn input_schema(&self) -> Value {
        SchemaBuilder::new()
            .description("Media asset operations for photos/images, videos, audio, and documents/PDFs")
            .string_enum("action", "Action to perform", VALID_ACTIONS, true)
            // Common fields
            .uuid("workspace_id", "Workspace ID", false)
            .uuid("project_id", "Project ID", false)
            .string(
                "target_project",
                "Target child project by folder name or project name (e.g. 'contextstream', 'mcp-server')",
                false,
            )
            // Index fields
            .string("file_path", "Local path to a media asset (photo/image, video, audio, document/PDF)", false)
            .string("external_url", "External URL to a media asset (http/https only; IP-literal private/loopback/link-local/metadata hosts are rejected as a best-effort pre-filter — authoritative SSRF checks run server-side at fetch time)", false)
            .boolean(
                "allow_broad",
                "Opt in to index a file_path outside the active project root (P0 ingestion containment). Does not bypass the filename/dirname secret filter or the size cap (those always apply). Also settable via CONTEXTSTREAM_ALLOW_BROAD_INGEST=1.",
                false,
            )
            .string_enum(
                "content_type",
                "Type of media content. Canonical values: video, audio, image, document. Friendly aliases are normalized: photos/images -> image; docs/PDFs/slides -> document.",
                VALID_CONTENT_TYPES,
                false,
            )
            .array("tags", "Tags to associate with media", "string", false)
            // Status/get_clip/delete fields
            .uuid("content_id", "Content ID from index/list/search results", false)
            // Search fields
            .string("query", "Semantic search query for indexed media content, transcripts, OCR/document text, titles, and tags", false)
            .array(
                "content_types",
                "Filter to specific media content types (video, audio, image, document). Friendly aliases are normalized: photos/images -> image; docs/PDFs/slides -> document.",
                "string",
                false,
            )
            .integer("limit", "Maximum results", false)
            // Clip fields
            .string("start", "Start time (e.g., '1:34', '94s')", false)
            .string("end", "End time (e.g., '2:15', '135s')", false)
            .string_enum(
                "output_format",
                "Output format for clip",
                VALID_OUTPUT_FORMATS,
                false,
            )
            .integer("fps", "Frames per second for remotion format", false)
            .build()
    }
}

// ============================================================================
// P0 Ingestion Containment (media index)
// ============================================================================

/// Validate a local `file_path` for `media(action="index")`.
///
/// Enforces, in order:
///  1. the path is an existing regular file;
///  2. the file is within [`mcp_client::MEDIA_INDEX_MAX_BYTES`];
///  3. the file is not a secret/credential (never bypassable);
///  4. CONTAINMENT: the file lives inside `project_root` unless `opt_in`.
fn validate_media_index_file(path: &Path, project_root: Option<&Path>, opt_in: bool) -> Result<()> {
    validate_media_index_file_with_cap(
        path,
        project_root,
        opt_in,
        mcp_client::MEDIA_INDEX_MAX_BYTES,
    )
}

/// Same as [`validate_media_index_file`] but with an injectable size cap, so the
/// size-limit branch is testable without materializing a 512 MiB fixture.
fn validate_media_index_file_with_cap(
    path: &Path,
    project_root: Option<&Path>,
    opt_in: bool,
    max_bytes: u64,
) -> Result<()> {
    let display = path.display();

    let meta = std::fs::metadata(path).map_err(|e| {
        Error::Validation(format!(
            "media(action=\"index\") cannot access file_path '{display}': {e}"
        ))
    })?;
    if !meta.is_file() {
        return Err(Error::Validation(format!(
            "media(action=\"index\") requires file_path to be a regular file, not a directory or special file: '{display}'."
        )));
    }
    if meta.len() > max_bytes {
        return Err(Error::Validation(format!(
            "media(action=\"index\") refuses '{display}': {} bytes exceeds the {}-byte single-asset limit.",
            meta.len(),
            max_bytes
        )));
    }
    if let Some(reason) = media_secret_rejection_reason(path) {
        return Err(Error::Validation(format!(
            "media(action=\"index\") refuses '{display}' because {reason}. Secrets and credentials are never uploaded."
        )));
    }
    if !opt_in {
        match project_root {
            Some(root) if !root.as_os_str().is_empty() => {
                if !path_is_within_root(path, root) {
                    return Err(Error::Validation(format!(
                        "media(action=\"index\") refuses '{display}' because it is outside the active project root '{}'. \
                         Move the asset into the project, or opt in explicitly (pass allow_broad=true or set {}=1).",
                        root.display(),
                        mcp_client::ALLOW_BROAD_INGEST_ENV
                    )));
                }
            }
            _ => {
                return Err(Error::Validation(format!(
                    "media(action=\"index\") refuses '{display}' because there is no active project root to contain it. \
                     Initialize a project first, or opt in explicitly (pass allow_broad=true or set {}=1).",
                    mcp_client::ALLOW_BROAD_INGEST_ENV
                )));
            }
        }
    }
    Ok(())
}

/// Return a human-readable reason when `path` looks like a secret/credential and
/// must never be uploaded. Combines mcp-client's filename filter (`.env`,
/// `id_rsa`, `*.pem`, `*.key`, `credentials.json`, archives/binaries) with a
/// parent-directory-aware check for sensitive credential directories
/// (`~/.ssh`, `~/.aws`, `~/.gnupg`, ...). The path is also canonicalized so a
/// symlink into a sensitive directory cannot dodge the check. Never bypassable.
fn media_secret_rejection_reason(path: &Path) -> Option<String> {
    let canonical = std::fs::canonicalize(path).ok();
    for candidate in std::iter::once(path).chain(canonical.as_deref()) {
        for component in candidate.components() {
            if let std::path::Component::Normal(os) = component {
                if let Some(name) = os.to_str() {
                    if mcp_client::SENSITIVE_DIR_NAMES.contains(&name) {
                        return Some(format!("it lives inside a sensitive directory ({name})"));
                    }
                }
            }
        }
    }

    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    if !file_name.is_empty() && mcp_client::ContextStreamClient::should_skip_file(file_name, true) {
        return Some(format!(
            "'{file_name}' matches the blocked-file filter (secrets, keys, archives, and binaries are never uploaded)"
        ));
    }

    None
}

/// True when `path` is the same as, or nested inside, `root` (component-wise,
/// after canonicalization so `..` and symlinks resolve).
///
/// The file `path` MUST canonicalize — it was just stat'd by the caller, so it
/// exists; if canonicalization nonetheless fails we refuse containment rather
/// than compare a raw, `..`-bearing literal (which could lexically "start with"
/// the root while actually escaping it). The `root` may legitimately not exist
/// yet, so it falls back to its literal form.
fn path_is_within_root(path: &Path, root: &Path) -> bool {
    let resolved_path = match std::fs::canonicalize(path) {
        Ok(p) => p,
        Err(_) => return false,
    };
    let resolved_root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    resolved_path.starts_with(&resolved_root)
}

/// Validate an `external_url` for `media(action="index")` (SSRF guard).
///
/// Only `http`/`https` are allowed, and the host may not be `localhost` or an
/// IP literal in a private/loopback/link-local/metadata/unspecified range
/// (e.g. `127.0.0.0/8`, `10/8`, `172.16/12`, `192.168/16`, `169.254.169.254`,
/// `0.0.0.0`, `::1`).
fn validate_external_media_url(raw: &str) -> Result<()> {
    let parsed = reqwest::Url::parse(raw.trim())
        .map_err(|e| Error::Validation(format!("external_url is not a valid URL: {e}")))?;

    let scheme = parsed.scheme().to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(Error::Validation(format!(
            "media(action=\"index\") refuses external_url scheme '{scheme}': only http and https are allowed."
        )));
    }

    let host = parsed
        .host_str()
        .ok_or_else(|| Error::Validation("external_url must include a host".to_string()))?;

    // `host_str()` may bracket IPv6 literals (`[::1]`); strip them for parsing.
    let bare_host = host
        .trim_matches(|c| c == '[' || c == ']')
        .to_ascii_lowercase();
    if bare_host == "localhost" || bare_host.ends_with(".localhost") {
        return Err(Error::Validation(
            "media(action=\"index\") refuses external_url pointing at localhost (SSRF guard)."
                .to_string(),
        ));
    }
    if let Ok(ip) = bare_host.parse::<IpAddr>() {
        if is_blocked_ip(&ip) {
            return Err(Error::Validation(format!(
                "media(action=\"index\") refuses external_url host '{host}' because it is a private/loopback/link-local/metadata address (SSRF guard)."
            )));
        }
    }

    Ok(())
}

/// Whether an IP literal is in a range that must be refused for `external_url`.
fn is_blocked_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_ipv4(v4),
        IpAddr::V6(v6) => is_blocked_ipv6(v6),
    }
}

fn is_blocked_ipv4(ip: &Ipv4Addr) -> bool {
    // 10/8, 172.16/12, 192.168/16; 127/8; 169.254/16 (incl. 169.254.169.254
    // metadata); 0.0.0.0 and 0/8; 255.255.255.255.
    ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_unspecified()
        || ip.is_broadcast()
        || ip.octets()[0] == 0
}

fn is_blocked_ipv6(ip: &Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_unspecified() {
        return true; // ::1, ::
    }
    // IPv4-mapped (`::ffff:a.b.c.d`) — classify the embedded v4 address.
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_blocked_ipv4(&v4);
    }
    let seg = ip.segments();
    // fe80::/10 link-local, fc00::/7 unique-local.
    (seg[0] & 0xffc0) == 0xfe80 || (seg[0] & 0xfe00) == 0xfc00
}

/// Register all media tools.
pub fn register_media_tools(
    registry: &mut crate::registry::ToolRegistry,
    client: ContextStreamClient,
    session: Arc<SessionManager>,
) {
    registry.register("media", Arc::new(MediaTool::new(client, session)));
}

#[cfg(test)]
#[path = "media_tests.rs"]
mod tests;
