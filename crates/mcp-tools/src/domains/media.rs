//! Media asset tools: index, upload_url, complete_upload, status, search, get_clip, list, delete.

use async_trait::async_trait;
use mcp_client::{
    ContextStreamClient, MediaGetClipParams, MediaIndexParams, MediaSearchParams,
    MediaUploadInitParams,
};
use mcp_session::SessionManager;
use mcp_types::{
    tool::{ToolAnnotations, ToolCategory, ToolMetadata, ToolResult},
    Error, Result,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
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
const VALID_ACTIONS: &[&str] = &[
    "index",
    "upload_url",
    "complete_upload",
    "status",
    "search",
    "get_clip",
    "list",
    "delete",
];

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
    // Index / upload_url fields
    pub file_path: Option<String>,
    pub filename: Option<String>,
    pub external_url: Option<String>,
    pub content_type: Option<String>,
    pub mime_type: Option<String>,
    pub size_bytes: Option<u64>,
    pub title: Option<String>,
    pub tags: Option<Vec<String>>,
    /// Opt-in to index a `file_path` that lives outside the active project root
    /// (P0 ingestion containment). Also honored via the
    /// `CONTEXTSTREAM_ALLOW_BROAD_INGEST` env var. Does NOT bypass the
    /// secret/credential filter or the size cap. Defaults to false.
    pub allow_broad: Option<bool>,
    // Complete / Status / get_clip / delete fields
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
        let failure_reason = (status.as_deref() == Some("failed"))
            .then(|| {
                item.get("failure_reason")
                    .or_else(|| item.get("metadata").and_then(|m| m.get("failure_reason")))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|reason| !reason.is_empty())
                    .map(str::to_string)
            })
            .flatten();
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
        // A failed file says why: the status said only "failed" (#375).
        if let Some(reason) = failure_reason {
            line.push_str(&format!(
                "\n   failed: {}",
                Self::truncate_for_summary(&reason, 240)
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

    async fn start_client_upload(
        &self,
        input: &MediaInput,
        content_type: Option<String>,
        workspace_id: Option<Uuid>,
        project_id: Option<Uuid>,
        hosted_fallback: bool,
    ) -> Result<ToolResult> {
        let raw_path = input
            .file_path
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let explicit_filename = input
            .filename
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let filename = match (explicit_filename, raw_path) {
            (Some(name), _) => basename_from_path(name)
                .ok_or_else(|| Error::Validation(format!("Invalid filename '{}'", name)))?,
            (None, Some(path)) => basename_from_path(path).ok_or_else(|| {
                Error::Validation(format!(
                    "file_path '{}' must include a valid file name",
                    path
                ))
            })?,
            (None, None) => {
                return Err(Error::Validation(
                    "file_path or filename is required for upload_url".to_string(),
                ));
            }
        };

        // Purely lexical secret/credential filter (never touches gateway disk).
        if let Some(path) = raw_path {
            if let Some(reason) = lexical_secret_path_rejection_reason(path) {
                return Err(Error::Validation(format!(
                    "media(action=\"{}\") refuses '{path}' because {reason}. Secrets and credentials are never uploaded.",
                    input.action
                )));
            }
        } else if mcp_client::ContextStreamClient::should_skip_file(&filename, true) {
            return Err(Error::Validation(format!(
                "media(action=\"{}\") refuses '{filename}' because '{filename}' matches the blocked-file filter (secrets, keys, archives, and binaries are never uploaded). Secrets and credentials are never uploaded.",
                input.action
            )));
        }

        if let Some(size_bytes) = input.size_bytes {
            if size_bytes == 0 {
                return Err(Error::Validation(
                    "size_bytes must be at least 1 byte".to_string(),
                ));
            }
            if size_bytes > mcp_client::MEDIA_INDEX_MAX_BYTES {
                return Err(Error::Validation(format!(
                    "media upload refused: {} bytes exceeds the {}-byte single-asset limit",
                    size_bytes,
                    mcp_client::MEDIA_INDEX_MAX_BYTES
                )));
            }
        }

        let resolved_content_type = content_type
            .unwrap_or_else(|| detect_content_type_from_filename(&filename).to_string());
        let resolved_mime_type = input
            .mime_type
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| guess_mime_type_from_filename(&filename).map(str::to_string));
        let title = input
            .title
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        let upload_path_for_cmd = raw_path.unwrap_or(filename.as_str());

        let init_result = match self
            .client
            .media_init_upload(MediaUploadInitParams {
                filename: filename.clone(),
                content_type: resolved_content_type.clone(),
                mime_type: resolved_mime_type.clone(),
                size_bytes: input.size_bytes,
                title,
                tags: input.tags.clone(),
                workspace_id,
                project_id,
            })
            .await
        {
            Ok(res) => res,
            Err(err) if input.size_bytes.is_none() && is_missing_size_bytes_error(&err) => {
                let quoted_path = shell_single_quote(upload_path_for_cmd);
                return Err(Error::Validation(format!(
                    "Upload init requires size_bytes (file size in bytes). Measure the file locally with `stat -c %s {quoted_path}` (macOS: `stat -f %z {quoted_path}`) and retry with size_bytes=<bytes>."
                )));
            }
            Err(err) => return Err(err),
        };

        let content_id = init_result
            .get("content_id")
            .or_else(|| init_result.get("id"))
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Tool("No content_id in upload init response".to_string()))?;
        let upload_url = init_result
            .get("upload_url")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Tool("No upload_url in upload init response".to_string()))?;
        let headers = init_result
            .get("headers")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let expires_in_seconds = init_result
            .get("expires_in_seconds")
            .and_then(Value::as_u64);

        let upload_command = build_curl_upload_command(upload_path_for_cmd, &headers, upload_url);
        let complete_upload_call = format!(
            "media(action=\"complete_upload\", content_id=\"{}\")",
            content_id
        );

        let mut structured = Map::new();
        structured.insert("mode".to_string(), json!("client_upload"));
        structured.insert("status".to_string(), json!("awaiting_upload"));
        structured.insert("content_id".to_string(), json!(content_id));
        structured.insert("filename".to_string(), json!(filename));
        structured.insert("content_type".to_string(), json!(resolved_content_type));
        if let Some(mime_type) = &resolved_mime_type {
            structured.insert("mime_type".to_string(), json!(mime_type));
        }
        structured.insert("upload_url".to_string(), json!(upload_url));
        structured.insert("headers".to_string(), Value::Object(headers.clone()));
        if let Some(expires) = expires_in_seconds {
            structured.insert("expires_in_seconds".to_string(), json!(expires));
        }
        structured.insert("upload_command".to_string(), json!(upload_command));
        structured.insert(
            "complete_upload_call".to_string(),
            json!(complete_upload_call),
        );

        let expiry_note = expires_in_seconds
            .map(|secs| format!(", expires_in_seconds: {secs}"))
            .unwrap_or_default();
        let intro = if hosted_fallback {
            "The hosted MCP gateway cannot read your local disk directly. Upload the file from your machine, then finalize indexing:"
        } else {
            "Upload the file from your machine, then finalize indexing:"
        };
        let headers_json =
            serde_json::to_string(&Value::Object(headers)).unwrap_or_else(|_| "{}".to_string());
        let mut message = format!(
            "Upload initialized for {filename} (content_id: {content_id}{expiry_note}).\n{intro}\n\n1. Run:\n   {upload_command}\n\n2. Then call:\n   {complete_upload_call}\n\nUpload details:\n- content_id: {content_id}\n- upload_url: {upload_url}\n- headers: {headers_json}"
        );
        if let Some(expires) = expires_in_seconds {
            message.push_str(&format!("\n- expires_in_seconds: {expires}"));
        }

        // Never log the presigned upload_url; return it only in the tool result.
        Ok(ToolResult::with_structured(
            message,
            Value::Object(structured),
        ))
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

                // On the hosted MCP gateway (local_files_allowed == false), we never
                // read the gateway host's own disk. Instead of dead-ending on a local
                // file_path, initialize a presigned upload plan that the client runs
                // on its own machine and finishes with media(action="complete_upload").
                if input.file_path.is_some() && !self.client.config().await.local_files_allowed() {
                    return self
                        .start_client_upload(&input, content_type, workspace_id, project_id, true)
                        .await;
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
                    mime_type: input.mime_type,
                    title: input.title,
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

            "upload_url" => {
                self.start_client_upload(&input, content_type, workspace_id, project_id, false)
                    .await
            }

            "complete_upload" => {
                let content_id = Self::parse_content_id(&input.content_id)?;
                let result = self
                    .client
                    .media_complete_upload(workspace_id, content_id)
                    .await?;
                let status = result
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("processing");
                let mut message = format!(
                    "Media upload completed for content_id: {} (status: {}). Next: call media(action=\"status\", content_id=\"{}\") until indexed, then media(action=\"search\", query=\"...\") or media(action=\"list\").",
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
            description: "Media operations for indexed assets: photos/images, videos, audio, and documents/PDFs. Actions: index (upload URL/local assets and trigger ML processing; on the hosted MCP gateway, local file_path returns a presigned upload_url, headers, curl command, and follow-up complete_upload call so the client uploads directly), upload_url (start a client-side upload and return the presigned PUT URL, headers, and curl command without reading disk), complete_upload (finalize a client-side upload by content_id so indexing starts), status (check processing), search (semantic search with Knowledge Stream fallback), get_clip (extract video/audio clip details with output_format: remotion/ffmpeg/raw), list (to delete an asset: the `delete` tool, action=\"media.delete\").".to_string(),
            category: ToolCategory::Ai,
            annotations: ToolAnnotations::destructive(),
            is_pro: true,
            required_tier: None,
        })
    }

    fn input_schema(&self) -> Value {
        SchemaBuilder::new()
            .description("Media asset operations for photos/images, videos, audio, and documents/PDFs")
            .string_enum(
                "action",
                "Action to perform (index, upload_url, complete_upload, status, search, get_clip, list, delete)",
                VALID_ACTIONS,
                true,
            )
            // Common fields
            .uuid("workspace_id", "Workspace ID", false)
            .uuid("project_id", "Project ID", false)
            .string(
                "target_project",
                "Target child project by folder name or project name (e.g. 'contextstream', 'mcp-server')",
                false,
            )
            // Index / upload_url fields
            .string(
                "file_path",
                "Local path to a media asset (photo/image, video, audio, document/PDF). In local MCP mode the file is uploaded directly; on the hosted MCP gateway, index/upload_url returns a presigned upload_url, headers, curl upload command, and complete_upload follow-up without reading gateway disk.",
                false,
            )
            .string(
                "filename",
                "File name for upload_url when file_path is not given (defaults to the basename of file_path)",
                false,
            )
            .string("external_url", "External URL to a media asset (http/https only; IP-literal private/loopback/link-local/metadata hosts are rejected as a best-effort pre-filter — authoritative SSRF checks run server-side at fetch time)", false)
            .integer(
                "size_bytes",
                "File size in bytes for hosted index or upload_url (pass when known; if required by the API and omitted, the error returns the exact stat command to measure it)",
                false,
            )
            .string(
                "mime_type",
                "MIME type for hosted index or upload_url (guessed from file extension when omitted, e.g. application/pdf, image/png)",
                false,
            )
            .string("title", "Optional display title for the uploaded media asset", false)
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
            // Complete / Status / get_clip / delete fields
            .uuid(
                "content_id",
                "Content ID from index/upload_url/list/search results (required for complete_upload, status, get_clip, delete)",
                false,
            )
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
// Client-side Hosted Upload Helpers (never read gateway disk)
// ============================================================================

/// Extract the final path component from a Unix or Windows path string without
/// touching the local filesystem.
fn basename_from_path(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_end_matches(['/', '\\']);
    let name = trimmed.rsplit(['/', '\\']).next().unwrap_or("").trim();
    if name.is_empty() || name == "." || name == ".." {
        None
    } else {
        Some(name.to_string())
    }
}

fn extension_lower(filename: &str) -> Option<String> {
    let (_, ext) = filename.rsplit_once('.')?;
    let trimmed = ext.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_ascii_lowercase())
    }
}

fn detect_content_type_from_filename(filename: &str) -> &'static str {
    match extension_lower(filename).as_deref() {
        Some("mp4" | "m4v" | "mov" | "avi" | "mkv" | "webm" | "mpeg" | "mpg") => "video",
        Some("mp3" | "wav" | "ogg" | "oga" | "m4a" | "flac" | "aac" | "opus" | "weba") => "audio",
        Some(
            "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "svg" | "tif" | "tiff" | "heic"
            | "heif" | "avif",
        ) => "image",
        _ => "document",
    }
}

fn guess_mime_type_from_filename(filename: &str) -> Option<&'static str> {
    match extension_lower(filename).as_deref()? {
        "pdf" => Some("application/pdf"),
        "doc" => Some("application/msword"),
        "docx" => Some("application/vnd.openxmlformats-officedocument.wordprocessingml.document"),
        "ppt" => Some("application/vnd.ms-powerpoint"),
        "pptx" => Some("application/vnd.openxmlformats-officedocument.presentationml.presentation"),
        "xls" => Some("application/vnd.ms-excel"),
        "xlsx" => Some("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"),
        "txt" => Some("text/plain"),
        "md" | "markdown" => Some("text/markdown"),
        "csv" => Some("text/csv"),
        "json" => Some("application/json"),
        "html" | "htm" => Some("text/html"),
        "rtf" => Some("application/rtf"),
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        "svg" => Some("image/svg+xml"),
        "tif" | "tiff" => Some("image/tiff"),
        "heic" => Some("image/heic"),
        "heif" => Some("image/heif"),
        "avif" => Some("image/avif"),
        "mp4" | "m4v" => Some("video/mp4"),
        "mov" => Some("video/quicktime"),
        "webm" => Some("video/webm"),
        "avi" => Some("video/x-msvideo"),
        "mkv" => Some("video/x-matroska"),
        "mpeg" | "mpg" => Some("video/mpeg"),
        "mp3" => Some("audio/mpeg"),
        "wav" => Some("audio/wav"),
        "ogg" | "oga" => Some("audio/ogg"),
        "m4a" => Some("audio/mp4"),
        "flac" => Some("audio/flac"),
        "aac" => Some("audio/aac"),
        "opus" => Some("audio/opus"),
        "weba" => Some("audio/webm"),
        _ => None,
    }
}

/// POSIX single-quote `value`: wrap in `'` and escape embedded `'` as `'\''`.
fn shell_single_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Build a copy-pasteable `curl` command that PUTs `file_path` to `upload_url`
/// with every signed header returned by the API.
fn build_curl_upload_command(
    file_path: &str,
    headers: &Map<String, Value>,
    upload_url: &str,
) -> String {
    let mut parts = vec![
        "curl".to_string(),
        "-fsS".to_string(),
        "-X".to_string(),
        "PUT".to_string(),
        "--upload-file".to_string(),
        shell_single_quote(file_path),
    ];

    let mut header_entries: Vec<(&str, &str)> = headers
        .iter()
        .filter_map(|(name, value)| value.as_str().map(|v| (name.as_str(), v)))
        .collect();
    header_entries.sort_by_key(|(a, _)| *a);

    for (name, value) in header_entries {
        parts.push("-H".to_string());
        parts.push(shell_single_quote(&format!("{name}: {value}")));
    }

    parts.push(shell_single_quote(upload_url));
    parts.join(" ")
}

/// Purely lexical secret/credential path check for hosted upload init: never
/// invokes `std::fs::canonicalize` or any filesystem syscall on the gateway host.
fn lexical_secret_path_rejection_reason(raw: &str) -> Option<String> {
    for segment in raw.split(['/', '\\']) {
        if mcp_client::SENSITIVE_DIR_NAMES.contains(&segment) {
            return Some(format!("it lives inside a sensitive directory ({segment})"));
        }
    }
    if let Some(filename) = basename_from_path(raw) {
        if mcp_client::ContextStreamClient::should_skip_file(&filename, true) {
            return Some(format!(
                "'{filename}' matches the blocked-file filter (secrets, keys, archives, and binaries are never uploaded)"
            ));
        }
    }
    None
}

fn is_missing_size_bytes_error(err: &Error) -> bool {
    match err {
        Error::Http {
            status, message, ..
        } if *status == 400 || *status == 422 => {
            message.to_ascii_lowercase().contains("size_bytes")
        }
        _ => false,
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
    mcp_client::secret_path_rejection_reason(path, &mcp_client::SENSITIVE_DIR_NAMES)
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
