//! Memory domain tools: events, nodes, tasks, todos, diagrams, docs, transcripts.

use async_trait::async_trait;
use mcp_client::{
    infer_memory_query_node_type, ContextStreamClient, CreateDiagramParams, CreateDocParams,
    CreateMemoryNodeParams, CreateRoadmapParams, CreateTaskParams, CreateTodoParams,
    ImportMemoryEventsParams, ListTodosParams, MemorySearchParams, RoadmapMilestone,
    SearchTranscriptsParams, SupersedeMemoryNodeParams, UpdateDiagramParams, UpdateDocParams,
    UpdateMemoryEventParams, UpdateMemoryNodeParams, UpdateTaskParams, UpdateTodoParams,
};
use mcp_types::{
    tool::{ToolAnnotations, ToolCategory, ToolMetadata, ToolResult},
    Error, Result,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tracing;
use uuid::Uuid;

use crate::domains::result_cache::{rendered_entry_fits, ResultCache};
use crate::domains::scope::{
    attach_scope_recovery_metadata, recover_write_scope_after_project_error, resolve_read_scope,
    resolve_write_scope, ResolvedReadScope, ResolvedWriteScope,
};
use crate::domains::session::{deserialize_string_or_vec, is_not_found_error};
use crate::registry::ToolHandler;
use crate::schema::SchemaBuilder;
use mcp_client::SessionCaptureParams;

/// Per-process warm cache for `memory(action="search")`. The short TTL
/// preserves freshness; per-caller and per-entry bounds keep a shared gateway
/// warm without letting one caller or one oversized response consume the pool.
const MEMORY_SEARCH_CACHE_TTL: Duration = Duration::from_secs(30);
const MEMORY_SEARCH_CACHE_MAX_ENTRIES: usize = 64;
const MEMORY_SEARCH_CACHE_MAX_ENTRIES_PER_CALLER: usize = 8;
const MEMORY_SEARCH_CACHE_MAX_ENTRY_BYTES: usize = 128 * 1024;
const MEMORY_SEARCH_TOOL_TIMEOUT: Duration = Duration::from_secs(6);
const MEMORY_SEARCH_DOCS_TIMEOUT: Duration = Duration::from_millis(1_500);
const MEMORY_SEARCH_DOC_DETAIL_TIMEOUT: Duration = Duration::from_millis(750);

static MEMORY_SEARCH_RESULT_CACHE: OnceLock<ResultCache<(String, Value)>> = OnceLock::new();

fn memory_search_cache() -> &'static ResultCache<(String, Value)> {
    MEMORY_SEARCH_RESULT_CACHE
        .get_or_init(|| ResultCache::new(MEMORY_SEARCH_CACHE_TTL, MEMORY_SEARCH_CACHE_MAX_ENTRIES))
}

fn put_memory_search_cache(
    caller_identity: Option<&str>,
    cache_key: String,
    value: (String, Value),
) {
    let Some(caller_identity) = caller_identity else {
        return;
    };
    if !rendered_entry_fits(&value.0, &value.1, MEMORY_SEARCH_CACHE_MAX_ENTRY_BYTES) {
        tracing::debug!("memory search result exceeded local cache entry budget");
        return;
    }
    memory_search_cache().put_partitioned(
        caller_identity,
        cache_key,
        value,
        MEMORY_SEARCH_CACHE_MAX_ENTRIES_PER_CALLER,
    );
}

fn current_memory_cache_identity() -> Option<String> {
    super::atlas_warm_cache::current_caller_cache_scope()
        .cache_identity()
        .map(str::to_string)
}

fn append_memory_cache_key_field(buffer: &mut Vec<u8>, name: &str, value: Option<&str>) {
    buffer.extend_from_slice(&(name.len() as u32).to_be_bytes());
    buffer.extend_from_slice(name.as_bytes());
    match value {
        Some(value) => {
            buffer.push(1);
            buffer.extend_from_slice(&(value.len() as u64).to_be_bytes());
            buffer.extend_from_slice(value.as_bytes());
        }
        None => buffer.push(0),
    }
}

async fn consume_grounding_memory_tool(session: &Arc<mcp_session::SessionManager>) {
    if let Some(fp) = session.state().await.folder_path.as_deref() {
        mcp_session::grounding_state::clear_grounding_consumed(fp);
    }
}

fn build_memory_search_cache_key(
    caller_identity: &str,
    workspace_id: Option<Uuid>,
    project_id: Option<Uuid>,
    query: &str,
    node_type: Option<&str>,
    limit: Option<i64>,
) -> String {
    let workspace_id = workspace_id.map(|id| id.to_string());
    let project_id = project_id.map(|id| id.to_string());
    let limit = limit.map(|value| value.to_string());
    let mut canonical = Vec::new();
    append_memory_cache_key_field(&mut canonical, "version", Some("memory-search-local:v2"));
    append_memory_cache_key_field(&mut canonical, "caller_identity", Some(caller_identity));
    append_memory_cache_key_field(&mut canonical, "workspace_id", workspace_id.as_deref());
    append_memory_cache_key_field(&mut canonical, "project_id", project_id.as_deref());
    append_memory_cache_key_field(&mut canonical, "query", Some(query));
    append_memory_cache_key_field(&mut canonical, "node_type", node_type);
    append_memory_cache_key_field(&mut canonical, "limit", limit.as_deref());
    format!(
        "memory-search-local:v2:{}",
        super::search::sha256_hex_bytes(&canonical)
    )
}

fn memory_search_cache_key_for_caller(
    caller_identity: Option<&str>,
    workspace_id: Option<Uuid>,
    project_id: Option<Uuid>,
    query: &str,
    node_type: Option<&str>,
    limit: Option<i64>,
) -> Option<String> {
    let caller_identity = caller_identity?;
    workspace_id?;
    Some(build_memory_search_cache_key(
        caller_identity,
        workspace_id,
        project_id,
        query,
        node_type,
        limit,
    ))
}

/// Per-process warm cache for `memory(action="search_transcripts")`.
const TRANSCRIPTS_SEARCH_CACHE_TTL: Duration = Duration::from_secs(30);
const TRANSCRIPTS_SEARCH_CACHE_MAX_ENTRIES: usize = 64;
const TRANSCRIPTS_SEARCH_CACHE_MAX_ENTRIES_PER_CALLER: usize = 8;
const TRANSCRIPTS_SEARCH_CACHE_MAX_ENTRY_BYTES: usize = 128 * 1024;

static TRANSCRIPTS_SEARCH_RESULT_CACHE: OnceLock<ResultCache<(String, Value)>> = OnceLock::new();

fn transcripts_search_cache() -> &'static ResultCache<(String, Value)> {
    TRANSCRIPTS_SEARCH_RESULT_CACHE.get_or_init(|| {
        ResultCache::new(
            TRANSCRIPTS_SEARCH_CACHE_TTL,
            TRANSCRIPTS_SEARCH_CACHE_MAX_ENTRIES,
        )
    })
}

fn put_transcripts_search_cache(
    caller_identity: Option<&str>,
    cache_key: String,
    value: (String, Value),
) {
    let Some(caller_identity) = caller_identity else {
        return;
    };
    if !rendered_entry_fits(&value.0, &value.1, TRANSCRIPTS_SEARCH_CACHE_MAX_ENTRY_BYTES) {
        tracing::debug!("transcripts search result exceeded local cache entry budget");
        return;
    }
    transcripts_search_cache().put_partitioned(
        caller_identity,
        cache_key,
        value,
        TRANSCRIPTS_SEARCH_CACHE_MAX_ENTRIES_PER_CALLER,
    );
}

fn build_transcripts_search_cache_key(
    caller_identity: &str,
    workspace_id: Option<Uuid>,
    project_id: Option<Uuid>,
    query: &str,
    limit: Option<i64>,
    atlas_search_available: bool,
) -> String {
    let workspace_id = workspace_id.map(|id| id.to_string());
    let project_id = project_id.map(|id| id.to_string());
    let limit = limit.map(|value| value.to_string());
    let atlas_search_available = if atlas_search_available { "1" } else { "0" };
    let mut canonical = Vec::new();
    append_memory_cache_key_field(
        &mut canonical,
        "version",
        Some("transcripts-search-local:v2"),
    );
    append_memory_cache_key_field(&mut canonical, "caller_identity", Some(caller_identity));
    append_memory_cache_key_field(&mut canonical, "workspace_id", workspace_id.as_deref());
    append_memory_cache_key_field(&mut canonical, "project_id", project_id.as_deref());
    append_memory_cache_key_field(&mut canonical, "query", Some(query));
    append_memory_cache_key_field(&mut canonical, "limit", limit.as_deref());
    append_memory_cache_key_field(
        &mut canonical,
        "atlas_search_available",
        Some(atlas_search_available),
    );
    format!(
        "transcripts-search-local:v2:{}",
        super::search::sha256_hex_bytes(&canonical)
    )
}

fn transcripts_search_cache_key_for_caller(
    caller_identity: Option<&str>,
    workspace_id: Option<Uuid>,
    project_id: Option<Uuid>,
    query: &str,
    limit: Option<i64>,
    atlas_search_available: bool,
) -> Option<String> {
    let caller_identity = caller_identity?;
    workspace_id?;
    Some(build_transcripts_search_cache_key(
        caller_identity,
        workspace_id,
        project_id,
        query,
        limit,
        atlas_search_available,
    ))
}

async fn enrich_transcript_search_with_atlas(
    provider: Option<Arc<dyn mcp_types::atlas_layer::AtlasSearchProvider>>,
    caller_identity: Option<&str>,
    workspace_id: Option<Uuid>,
    project_id: Option<Uuid>,
    query: &str,
    limit: Option<i64>,
    result: &mut Value,
) {
    let (Some(provider), Some(caller_identity), Some(workspace_id)) =
        (provider, caller_identity, workspace_id)
    else {
        return;
    };

    use mcp_types::atlas_layer::{AtlasSearchCollection, AtlasSearchScope};
    let mut atlas_scope = AtlasSearchScope::new(workspace_id)
        .with_user_scope(caller_identity)
        .with_collections(vec![AtlasSearchCollection::Transcripts]);
    if let Some(project_id) = project_id {
        atlas_scope = atlas_scope.with_project(project_id);
    }

    let limit = limit.unwrap_or(10).max(1) as usize;
    let atlas_lookup = tokio::time::timeout(
        Duration::from_millis(250),
        provider.fuzzy_text_search(query, &atlas_scope, limit.min(20)),
    )
    .await;
    if let Ok(Ok(hits)) = atlas_lookup {
        // Keep a defensive collection check even though the provider request
        // is already narrowed to transcripts.
        let transcript_hits: Vec<_> = hits
            .into_iter()
            .filter(|hit| hit.collection == AtlasSearchCollection::Transcripts)
            .collect();
        if !transcript_hits.is_empty() {
            if let Some(object) = result.as_object_mut() {
                if let Ok(hits_value) = serde_json::to_value(&transcript_hits) {
                    object.insert("atlas_search_hits".to_string(), hits_value);
                    object.insert(
                        "atlas_search_origin".to_string(),
                        serde_json::json!("atlas_search_lucene"),
                    );
                }
            }
        }
    }
}

/// Node types for memory. Extended in the Phase 3 + 4 taxonomy expansion to
/// include `goal`, `risk`, `term` — distilled OKR / risk register / glossary
/// summaries. Full structured records live in dedicated tables (`goals`,
/// `risks`) or as docs (`doc_type='glossary'`); these node types are the
/// short-form distilled signals.
const VALID_NODE_TYPES: &[&str] = &[
    "fact",
    "decision",
    "preference",
    "constraint",
    "habit",
    "lesson",
    "goal",
    "risk",
    "term",
];

const DOC_AUTO_OPEN_SCORE: i64 = 700;
const DOC_AUTO_OPEN_SECONDARY_MAX: i64 = 500;
const DOC_LOCAL_SWEEP_SCORE: i64 = 650;

async fn resolve_target_project_input(
    session: &mcp_session::SessionManager,
    target_project: Option<&str>,
) -> Result<Option<String>> {
    let Some(target_name) = target_project
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };

    if !session.has_child_projects().await {
        return Err(Error::Validation(format!(
            "target_project '{}' requires init from a multi-project parent folder first",
            target_name
        )));
    }

    if let Some(child) = session.resolve_child_project_by_name(target_name).await {
        return Ok(Some(child.project_id));
    }

    let mut available = session
        .get_child_projects()
        .await
        .into_keys()
        .collect::<Vec<_>>();
    available.sort();

    Err(Error::Validation(format!(
        "Unknown target_project '{}'. Available child projects: {}",
        target_name,
        if available.is_empty() {
            "none".to_string()
        } else {
            available.join(", ")
        }
    )))
}

fn normalize_node_type(input: &str) -> Result<String> {
    let normalized = input.trim().to_lowercase();
    match normalized.as_str() {
        "fact" | "insight" | "note" => Ok("Fact".to_string()),
        "decision" => Ok("Decision".to_string()),
        "preference" => Ok("Preference".to_string()),
        "constraint" => Ok("Constraint".to_string()),
        "habit" => Ok("Habit".to_string()),
        "lesson" => Ok("Lesson".to_string()),
        _ => Err(Error::Validation(format!(
            "Invalid node_type: {} (expected one of: {})",
            input,
            VALID_NODE_TYPES.join(", ")
        ))),
    }
}

fn extract_collection_array(value: &Value) -> Option<&Vec<Value>> {
    if let Some(arr) = value.as_array() {
        return Some(arr);
    }

    let obj = value.as_object()?;

    for key in [
        "items",
        "results",
        "todos",
        "tasks",
        "events",
        "nodes",
        "diagrams",
        "docs",
        "transcripts",
    ] {
        if let Some(arr) = obj.get(key).and_then(|v| v.as_array()) {
            return Some(arr);
        }
    }

    obj.get("data").and_then(extract_collection_array)
}

fn collection_count(value: &Value) -> usize {
    if let Some(arr) = extract_collection_array(value) {
        return arr.len();
    }

    value
        .get("total")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(0)
}

fn extract_metadata_str<'a>(item: &'a Value, field: &str) -> Option<&'a str> {
    item.get("metadata")
        .and_then(|value| value.get(field))
        .and_then(|value| value.as_str())
}

fn extract_display_title(node: &Value) -> String {
    crate::domains::display_title::extract_display_title(node)
}

fn extract_memory_result_type(item: &Value) -> String {
    for field in ["node_type", "type", "event_type"] {
        if let Some(value) = item.get(field).and_then(|v| v.as_str()) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }

    for field in ["node_type", "original_type", "event_type", "type"] {
        if let Some(value) = extract_metadata_str(item, field) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }

    if let Some(value) = item.get("result_type").and_then(|v| v.as_str()) {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }

    "unknown".to_string()
}

fn truncate_preview(raw: &str, max_chars: usize) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let mut preview = trimmed.replace('\n', " ");
    if preview.chars().count() > max_chars {
        preview = format!("{}...", preview.chars().take(max_chars).collect::<String>());
    }
    preview
}

fn extract_content_preview(item: &Value) -> Option<String> {
    for field in ["content", "details", "description"] {
        if let Some(raw) = item.get(field).and_then(|v| v.as_str()) {
            let preview = truncate_preview(raw, 150);
            if !preview.is_empty() {
                return Some(preview);
            }
        }
    }

    for field in ["summary", "content", "details", "description"] {
        if let Some(raw) = extract_metadata_str(item, field) {
            let preview = truncate_preview(raw, 150);
            if !preview.is_empty() {
                return Some(preview);
            }
        }
    }

    item.get("highlights")
        .and_then(|v| v.as_array())
        .and_then(|values| values.first())
        .and_then(|value| value.as_str())
        .map(|value| truncate_preview(value, 150))
        .filter(|value| !value.is_empty())
}

fn extract_memory_result_id(item: &Value) -> String {
    item.get("id")
        .or_else(|| item.get("node_id"))
        .and_then(|v| v.as_str())
        .or_else(|| extract_metadata_str(item, "node_id"))
        .unwrap_or("unknown")
        .to_string()
}

fn extract_memory_result_score(item: &Value) -> i64 {
    item.get("score")
        .and_then(|v| v.as_f64())
        .map(|score| (score * 300.0).round() as i64)
        .unwrap_or(0)
}

/// Format a collection of items into a human-readable list with IDs and titles.
/// This allows the AI to see item details (especially IDs) so it can act on them.
fn format_collection(entity_name: &str, value: &Value) -> String {
    let items = extract_collection_array(value);
    let count = items.map(|a| a.len()).unwrap_or_else(|| {
        value
            .get("total")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(0)
    });

    if count == 0 {
        let hint = match entity_name {
            "tasks" => " Create one with the create_task action.",
            "todos" => " Create one with the create_todo action.",
            "diagrams" => " Create one with the create_diagram action.",
            "docs" => " Create one with the create_doc action.",
            "transcripts" => " Transcripts are created automatically during sessions.",
            _ => "",
        };
        return format!("No {} found.{}", entity_name, hint);
    }

    let mut text = format!("Found {} {}:\n\n", count, entity_name);

    if let Some(arr) = items {
        for (i, item) in arr.iter().enumerate() {
            let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("—");
            let title = extract_display_title(item);
            let item_type = item
                .get("doc_type")
                .or_else(|| item.get("node_type"))
                .or_else(|| item.get("type"))
                .or_else(|| item.get("event_type"))
                .or_else(|| item.get("priority"))
                .and_then(|v| v.as_str());
            let status = item.get("status").and_then(|v| v.as_str());

            let mut line = format!("{}. **{}** (id: {})", i + 1, title, id);
            if let Some(t) = item_type {
                line.push_str(&format!(" [{}]", t));
            }
            if let Some(s) = status {
                line.push_str(&format!(" — {}", s));
            }
            text.push_str(&line);
            text.push('\n');
        }
    }

    text
}

fn extract_primary_object<'a>(value: &'a Value, keys: &[&str]) -> &'a Value {
    let mut current = value;
    // Some API responses may still be wrapped as { data: { ... } }.
    for _ in 0..3 {
        let has_key = keys.iter().any(|key| current.get(key).is_some());
        if has_key {
            return current;
        }

        if let Some(next) = current.get("data") {
            current = next;
        } else {
            break;
        }
    }
    current
}

fn format_doc_detail(value: &Value) -> String {
    let doc = extract_primary_object(value, &["id", "title", "content"]);
    let id = doc.get("id").and_then(|v| v.as_str()).unwrap_or("—");
    let title = doc
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("Untitled");
    let doc_type = doc.get("doc_type").and_then(|v| v.as_str()).unwrap_or("—");
    let created_at = doc
        .get("created_at")
        .and_then(|v| v.as_str())
        .unwrap_or("—");
    let updated_at = doc
        .get("updated_at")
        .and_then(|v| v.as_str())
        .unwrap_or("—");
    let content = doc
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();

    let mut text = format!("**{}**\nID: {}\nType: {}", title, id, doc_type);
    if created_at != "—" {
        text.push_str(&format!("\nCreated: {}", created_at));
    }
    if updated_at != "—" {
        text.push_str(&format!("\nUpdated: {}", updated_at));
    }

    text.push_str("\n\n");
    if content.is_empty() {
        text.push_str("Content is empty.");
    } else {
        text.push_str(content);
    }

    text
}

fn format_event_detail(value: &Value) -> String {
    let event = extract_primary_object(value, &["id", "title", "content"]);
    let id = event.get("id").and_then(|v| v.as_str()).unwrap_or("—");
    let title = event
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("(no title)");
    let event_type = event
        .get("event_type")
        .or_else(|| event.get("type"))
        .and_then(|v| v.as_str())
        .unwrap_or("—");
    let occurred_at = event
        .get("occurred_at")
        .or_else(|| event.get("created_at"))
        .and_then(|v| v.as_str());
    let updated_at = event.get("updated_at").and_then(|v| v.as_str());
    let tags = event
        .get("tags")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .filter(|s| !s.is_empty());
    let content = event
        .get("content")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();

    let mut text = format!("**{}**\nID: {}\nType: {}", title, id, event_type);
    if let Some(ts) = occurred_at {
        text.push_str(&format!("\nOccurred: {}", ts));
    }
    if let Some(ts) = updated_at {
        text.push_str(&format!("\nUpdated: {}", ts));
    }
    if let Some(t) = tags {
        text.push_str(&format!("\nTags: {}", t));
    }
    text.push_str("\n\n");
    if content.is_empty() {
        text.push_str("Content is empty.");
    } else {
        text.push_str(content);
    }
    text
}

fn format_task_detail(value: &Value) -> String {
    let task = extract_primary_object(value, &["id", "title", "description", "content"]);
    let id = task.get("id").and_then(|v| v.as_str()).unwrap_or("—");
    let title = task
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("(no title)");
    let metadata = task.get("metadata");
    let pick = |key: &str| -> Option<String> {
        task.get(key)
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| {
                metadata
                    .and_then(|m| m.get(key))
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
    };
    let task_status = pick("task_status")
        .or_else(|| pick("status"))
        .unwrap_or_else(|| "—".to_string());
    let priority = pick("priority").unwrap_or_else(|| "—".to_string());
    let plan_id = task
        .get("plan_id")
        .or_else(|| task.get("parent_event_id"))
        .and_then(|v| v.as_str());
    let plan_step_id = pick("plan_step_id");
    let created_at = task.get("created_at").and_then(|v| v.as_str());
    let started_at = pick("started_at");
    let completed_at = pick("completed_at");
    let description = task
        .get("description")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            metadata
                .and_then(|m| m.get("description"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .or_else(|| {
            task.get("content")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default();
    let description = description.trim();

    let mut text = format!(
        "**{}**\nID: {}\nStatus: {}\nPriority: {}",
        title, id, task_status, priority
    );
    if let Some(pid) = plan_id {
        text.push_str(&format!("\nPlan: {}", pid));
    }
    if let Some(step) = plan_step_id {
        text.push_str(&format!("\nPlan step: {}", step));
    }
    if let Some(ts) = created_at {
        text.push_str(&format!("\nCreated: {}", ts));
    }
    if let Some(ts) = started_at {
        text.push_str(&format!("\nStarted: {}", ts));
    }
    if let Some(ts) = completed_at {
        text.push_str(&format!("\nCompleted: {}", ts));
    }
    text.push_str("\n\n");
    if description.is_empty() {
        text.push_str("Description is empty.");
    } else {
        text.push_str(description);
    }
    text
}

fn format_todo_detail(value: &Value) -> String {
    let todo = extract_primary_object(value, &["id", "title", "content"]);
    let id = todo.get("id").and_then(|v| v.as_str()).unwrap_or("—");
    let title = todo
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("(no title)");
    let status = todo
        .get("status")
        .or_else(|| todo.get("todo_status"))
        .and_then(|v| v.as_str())
        .unwrap_or("—");
    let priority = todo
        .get("priority")
        .or_else(|| todo.get("todo_priority"))
        .and_then(|v| v.as_str())
        .unwrap_or("—");
    let due_at = todo.get("due_at").and_then(|v| v.as_str());
    let created_at = todo.get("created_at").and_then(|v| v.as_str());
    let completed_at = todo.get("completed_at").and_then(|v| v.as_str());
    let content = todo
        .get("description")
        .or_else(|| todo.get("content"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();

    let mut text = format!(
        "**{}**\nID: {}\nStatus: {}\nPriority: {}",
        title, id, status, priority
    );
    if let Some(ts) = due_at {
        text.push_str(&format!("\nDue: {}", ts));
    }
    if let Some(ts) = created_at {
        text.push_str(&format!("\nCreated: {}", ts));
    }
    if let Some(ts) = completed_at {
        text.push_str(&format!("\nCompleted: {}", ts));
    }
    text.push_str("\n\n");
    if content.is_empty() {
        text.push_str("Content is empty.");
    } else {
        text.push_str(content);
    }
    text
}

fn format_transcript_detail(value: &Value) -> String {
    const MAX_MESSAGES: usize = 40;
    const MAX_MSG_CHARS: usize = 800;

    let transcript = extract_primary_object(value, &["id", "messages", "session_id"]);
    let id = transcript.get("id").and_then(|v| v.as_str()).unwrap_or("—");
    let session_id = transcript.get("session_id").and_then(|v| v.as_str());
    let title = transcript
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("(no title)");
    let started_at = transcript.get("started_at").and_then(|v| v.as_str());
    let ended_at = transcript.get("ended_at").and_then(|v| v.as_str());
    let client_name = transcript.get("client_name").and_then(|v| v.as_str());
    let summary = transcript.get("summary").and_then(|v| v.as_str());
    let messages = transcript.get("messages").and_then(|v| v.as_array());
    let message_count = messages.map(|m| m.len()).unwrap_or(0);

    let mut text = format!("**{}**\nID: {}", title, id);
    if let Some(sid) = session_id {
        text.push_str(&format!("\nSession: {}", sid));
    }
    if let Some(c) = client_name {
        text.push_str(&format!("\nClient: {}", c));
    }
    if let Some(ts) = started_at {
        text.push_str(&format!("\nStarted: {}", ts));
    }
    if let Some(ts) = ended_at {
        text.push_str(&format!("\nEnded: {}", ts));
    }
    text.push_str(&format!("\nMessages: {}", message_count));
    if let Some(s) = summary {
        text.push_str(&format!("\n\nSummary: {}", s));
    }
    if let Some(arr) = messages {
        text.push_str("\n\n--- Messages ---");
        let shown = arr.len().min(MAX_MESSAGES);
        for (i, msg) in arr.iter().take(shown).enumerate() {
            let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("?");
            let body = msg
                .get("content")
                .and_then(|v| v.as_str())
                .unwrap_or("(empty)");
            let trimmed = if body.chars().count() > MAX_MSG_CHARS {
                let truncated: String = body.chars().take(MAX_MSG_CHARS).collect();
                format!("{}… [truncated]", truncated)
            } else {
                body.to_string()
            };
            text.push_str(&format!("\n\n[{}] {}: {}", i + 1, role, trimmed));
        }
        if arr.len() > shown {
            text.push_str(&format!(
                "\n\n… {} more message(s) elided. Full payload in structured response.",
                arr.len() - shown
            ));
        }
    }
    text
}

fn normalize_lookup_text(input: &str) -> String {
    let cleaned = input
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>();
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[derive(Debug, Clone)]
struct LookupMatch {
    id: Uuid,
    title: String,
    score: i64,
    exact: bool,
}

fn extract_first_string<'a>(item: &'a Value, fields: &[&str]) -> Option<&'a str> {
    fields.iter().find_map(|field| {
        item.get(field)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
    })
}

fn extract_item_uuid(item: &Value) -> Option<Uuid> {
    item.get("id")
        .and_then(|v| v.as_str())
        .and_then(|raw| Uuid::parse_str(raw).ok())
}

fn score_lookup_match(
    item: &Value,
    lookup_raw: &str,
    lookup_norm: &str,
    fields: &[&str],
) -> Option<LookupMatch> {
    let id = extract_item_uuid(item)?;
    let title = extract_first_string(item, fields).unwrap_or("(untitled)");
    let title_norm = normalize_lookup_text(title);
    let mut score = 0i64;
    let mut exact = false;

    if id.to_string().eq_ignore_ascii_case(lookup_raw) {
        score = 10_000;
        exact = true;
    } else if !lookup_norm.is_empty() && title_norm == lookup_norm {
        score = 9_000;
        exact = true;
    } else if !lookup_norm.is_empty() && title_norm.contains(lookup_norm) {
        score = 7_200;
    } else if !lookup_norm.is_empty() && lookup_norm.contains(&title_norm) && title_norm.len() >= 8
    {
        score = 6_600;
    } else if !lookup_norm.is_empty() {
        let terms: Vec<&str> = lookup_norm.split_whitespace().collect();
        let matched = terms
            .iter()
            .filter(|term| title_norm.contains(**term))
            .count();
        if matched == 0 {
            return None;
        }
        score = 2_500 + (matched as i64 * 150);
        if matched == terms.len() && !terms.is_empty() {
            score += 600;
        }
    }

    if score <= 0 {
        return None;
    }

    Some(LookupMatch {
        id,
        title: title.to_string(),
        score,
        exact,
    })
}

fn rank_lookup_matches(
    items: &[Value],
    lookup: &str,
    fields: &[&str],
    limit: usize,
) -> Vec<LookupMatch> {
    let lookup_raw = lookup.trim();
    let lookup_norm = normalize_lookup_text(lookup_raw);
    if lookup_raw.is_empty() {
        return Vec::new();
    }

    let mut matches: Vec<LookupMatch> = items
        .iter()
        .filter_map(|item| score_lookup_match(item, lookup_raw, &lookup_norm, fields))
        .collect();

    matches.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.title.cmp(&b.title)));
    matches.truncate(limit);
    matches
}

/// Exact (id or exact-title) matches for a `delete_all` bulk delete. Restricting
/// to exact matches keeps bulk delete safe: it removes duplicate rows sharing the
/// looked-up id/title, never loosely-related items.
fn collect_bulk_delete_matches(
    collection: &Value,
    lookup: &str,
    fields: &[&str],
) -> Vec<LookupMatch> {
    let items = extract_collection_array(collection)
        .cloned()
        .unwrap_or_default();
    rank_lookup_matches(&items, lookup, fields, 100)
        .into_iter()
        .filter(|m| m.exact)
        .collect()
}

enum LookupResolution<'a> {
    None,
    Single(&'a LookupMatch),
    Ambiguous,
}

/// One clear winner (exact id/title, or a score gap above the ambiguity
/// window) resolves; several close matches are ambiguous.
fn classify_lookup_resolution(ranked: &[LookupMatch]) -> LookupResolution<'_> {
    let Some(best) = ranked.first() else {
        return LookupResolution::None;
    };
    let second_score = ranked.get(1).map(|item| item.score).unwrap_or_default();
    if !best.exact && second_score > 0 && best.score <= second_score + 200 {
        LookupResolution::Ambiguous
    } else {
        LookupResolution::Single(best)
    }
}

/// `[CANDIDATES]` result for an ambiguous `supersede_node` lookup. Nothing
/// is superseded; the agent retries with one of the listed ids.
fn supersede_candidates_result(lookup: &str, matches: &[LookupMatch]) -> ToolResult {
    let mut text = format!(
        "[CANDIDATES] Multiple nodes match \"{lookup}\"; nothing was superseded. Retry supersede_node with node_id set to one of:\n"
    );
    let candidates: Vec<Value> = matches
        .iter()
        .take(5)
        .enumerate()
        .map(|(index, item)| {
            text.push_str(&format!(
                "{}. **{}** (id: {})\n",
                index + 1,
                item.title,
                item.id
            ));
            serde_json::json!({"id": item.id, "title": item.title, "score": item.score})
        })
        .collect();
    ToolResult::with_structured(
        text.trim_end().to_string(),
        serde_json::json!({"resolved": false, "lookup": lookup, "candidates": candidates}),
    )
}

fn format_lookup_ambiguity(entity_name: &str, lookup: &str, matches: &[LookupMatch]) -> String {
    let mut text = format!(
        "Multiple {} match \"{}\". Retry with an explicit id from the list below, \
         or pass delete_all=true to remove ALL exact-title matches in one call:\n\n",
        entity_name, lookup
    );
    for (idx, item) in matches.iter().take(5).enumerate() {
        text.push_str(&format!(
            "{}. **{}** (id: {})\n",
            idx + 1,
            item.title,
            item.id
        ));
    }
    text
}

fn resolve_uuid_from_matches(
    entity_name: &str,
    lookup: &str,
    matches: &[LookupMatch],
) -> Result<(Uuid, Option<String>)> {
    let best = matches.first().ok_or_else(|| {
        Error::Validation(format!(
            "No {} found matching \"{}\". Use list_{} to inspect available IDs.",
            entity_name, lookup, entity_name
        ))
    })?;

    let second_score = matches.get(1).map(|m| m.score).unwrap_or_default();
    if !best.exact && second_score > 0 && best.score <= second_score + 200 {
        return Err(Error::Validation(format_lookup_ambiguity(
            entity_name,
            lookup,
            matches,
        )));
    }

    let note = if best.exact {
        None
    } else {
        Some(format!(
            "Resolved \"{}\" to {} (id: {}).",
            lookup, best.title, best.id
        ))
    };

    Ok((best.id, note))
}

fn is_doc_query_stopword(term: &str) -> bool {
    matches!(
        term,
        "a" | "an"
            | "and"
            | "for"
            | "from"
            | "get"
            | "in"
            | "inside"
            | "look"
            | "lookup"
            | "mcp"
            | "me"
            | "of"
            | "on"
            | "open"
            | "page"
            | "pages"
            | "please"
            | "project"
            | "pull"
            | "read"
            | "repo"
            | "see"
            | "show"
            | "the"
            | "to"
            | "up"
            | "view"
            | "within"
            | "contextstream"
            | "doc"
            | "docs"
            | "document"
            | "documents"
            | "file"
            | "files"
    )
}

fn is_doc_intent_term(term: &str) -> bool {
    matches!(
        term,
        "doc"
            | "docs"
            | "document"
            | "documents"
            | "guide"
            | "manual"
            | "playbook"
            | "readme"
            | "reference"
            | "runbook"
            | "spec"
            | "specs"
    )
}

fn extract_quoted_phrases(query: &str) -> Vec<String> {
    let mut phrases = Vec::new();
    let mut in_quotes = false;
    let mut current = String::new();

    for ch in query.chars() {
        if ch == '"' {
            if in_quotes {
                let normalized = normalize_lookup_text(&current);
                if !normalized.is_empty() {
                    phrases.push(normalized);
                }
                current.clear();
            }
            in_quotes = !in_quotes;
            continue;
        }

        if in_quotes {
            current.push(ch);
        }
    }

    phrases
}

fn query_terms(query: &str) -> Vec<String> {
    normalize_lookup_text(query)
        .split_whitespace()
        .filter(|t| t.len() > 1 && !is_doc_query_stopword(t))
        .map(|t| t.to_string())
        .collect()
}

#[derive(Debug, Clone)]
struct PreparedDocQuery {
    raw: String,
    normalized: String,
    stripped: String,
    quoted_phrases: Vec<String>,
    terms: Vec<String>,
    has_doc_intent: bool,
}

impl PreparedDocQuery {
    fn new(query: &str) -> Self {
        let raw = query.trim().to_string();
        let normalized = normalize_lookup_text(&raw);
        let terms = query_terms(&raw);
        let stripped = terms.join(" ");
        let has_doc_intent = normalized.split_whitespace().any(is_doc_intent_term);
        let quoted_phrases = extract_quoted_phrases(&raw);

        Self {
            raw,
            normalized,
            stripped,
            quoted_phrases,
            terms,
            has_doc_intent,
        }
    }
}

#[derive(Debug, Clone)]
struct DocCandidateScore {
    score: i64,
    match_source: &'static str,
    exact: bool,
}

impl DocCandidateScore {
    fn zero() -> Self {
        Self {
            score: 0,
            match_source: "none",
            exact: false,
        }
    }
}

fn score_doc_candidate(doc: &Value, prepared: &PreparedDocQuery) -> DocCandidateScore {
    let title = doc
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if title.is_empty() {
        return DocCandidateScore::zero();
    }

    let title_norm = normalize_lookup_text(title);
    let id = doc.get("id").and_then(|v| v.as_str()).unwrap_or("").trim();

    let mut best_score = 0i64;
    let mut match_source = "term_overlap";
    let mut exact = false;

    let mut promote = |score: i64, source: &'static str, is_exact: bool| {
        if score > best_score {
            best_score = score;
            match_source = source;
            exact = is_exact;
        }
    };

    if !prepared.raw.is_empty() && !id.is_empty() && id.eq_ignore_ascii_case(&prepared.raw) {
        promote(2_500, "doc_id", true);
    }

    if !prepared.normalized.is_empty() {
        if title_norm == prepared.normalized {
            promote(2_200, "exact_title", true);
        } else if title_norm.contains(&prepared.normalized)
            && prepared.normalized.split_whitespace().count() >= 3
        {
            promote(1_400, "query_substring", false);
        } else if prepared.normalized.contains(&title_norm) && title_norm.len() >= 12 {
            promote(800, "title_in_query", false);
        }
    }

    if !prepared.stripped.is_empty() {
        if title_norm == prepared.stripped {
            promote(2_000, "normalized_title", true);
        } else if title_norm.contains(&prepared.stripped) {
            promote(1_550, "normalized_substring", false);
        } else if prepared.stripped.contains(&title_norm) && title_norm.len() >= 12 {
            promote(850, "title_in_stripped_query", false);
        }
    }

    for phrase in &prepared.quoted_phrases {
        if title_norm == *phrase {
            promote(2_100, "quoted_exact", true);
        } else if title_norm.contains(phrase) {
            promote(1_700, "quoted_phrase", false);
        }
    }

    let matched_terms = prepared
        .terms
        .iter()
        .filter(|term| title_norm.contains(term.as_str()))
        .count();

    if best_score == 0 && matched_terms == 0 {
        return DocCandidateScore::zero();
    }

    let mut score = best_score + matched_terms as i64 * 120;
    if !prepared.terms.is_empty() && matched_terms == prepared.terms.len() {
        score += 360;
        if best_score == 0 {
            match_source = "all_terms";
        }
    } else if matched_terms >= 3 {
        score += 180;
        if best_score == 0 {
            match_source = "strong_overlap";
        }
    } else if matched_terms >= 2 {
        score += 80;
    }

    if prepared.has_doc_intent {
        score += 120;
        if best_score == 0 {
            match_source = "doc_intent_overlap";
        }
    }

    if best_score == 0 && matched_terms == 1 && prepared.terms.len() > 2 {
        return DocCandidateScore::zero();
    }

    DocCandidateScore {
        score,
        match_source,
        exact,
    }
}

fn annotate_doc_match(mut doc: Value, score: &DocCandidateScore) -> Value {
    if let Some(obj) = doc.as_object_mut() {
        obj.insert("match_score".to_string(), serde_json::json!(score.score));
        obj.insert(
            "match_source".to_string(),
            serde_json::json!(score.match_source),
        );
        obj.insert("exact_match".to_string(), serde_json::json!(score.exact));
    }
    doc
}

fn doc_match_score(doc: &Value) -> i64 {
    doc.get("match_score")
        .and_then(|v| v.as_i64())
        .unwrap_or_default()
}

fn doc_match_source(doc: &Value) -> &str {
    doc.get("match_source")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
}

fn doc_exact_match(doc: &Value) -> bool {
    doc.get("exact_match")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

fn rank_docs_for_query(docs: Vec<Value>, query: &str, limit: usize) -> Vec<Value> {
    let prepared = PreparedDocQuery::new(query);
    let mut scored: Vec<(i64, usize, Value)> = docs
        .into_iter()
        .enumerate()
        .map(|doc| {
            let score = score_doc_candidate(&doc.1, &prepared);
            (score.score, doc.0, annotate_doc_match(doc.1, &score))
        })
        .collect();

    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));

    scored
        .into_iter()
        .filter_map(|(score, _, doc)| if score > 0 { Some(doc) } else { None })
        .take(limit)
        .collect()
}

fn dedupe_docs(docs: Vec<Value>) -> Vec<Value> {
    let mut seen = HashSet::new();
    let mut deduped = Vec::new();

    for doc in docs {
        let key = doc
            .get("id")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .or_else(|| {
                doc.get("title")
                    .and_then(|v| v.as_str())
                    .map(|value| value.to_string())
            });

        if let Some(key) = key {
            if seen.insert(key) {
                deduped.push(doc);
            }
        } else {
            deduped.push(doc);
        }
    }

    deduped
}

async fn search_docs_by_title(
    client: &ContextStreamClient,
    workspace_id: Option<Uuid>,
    project_id: Option<Uuid>,
    doc_type: Option<&str>,
    is_personal: Option<bool>,
    query: &str,
    limit: Option<i64>,
) -> Result<Vec<Value>> {
    let fetch_limit = limit.unwrap_or(50).clamp(5, 100);
    let return_limit = limit.unwrap_or(10).clamp(1, 25) as usize;
    let query = query.trim();
    let server_docs = client
        .list_docs(
            workspace_id,
            project_id,
            doc_type.map(str::to_string),
            is_personal,
            Some(query.to_string()),
            Some(fetch_limit),
        )
        .await?;
    let mut docs = extract_collection_array(&server_docs)
        .cloned()
        .unwrap_or_default();

    let ranked_server_docs = rank_docs_for_query(docs.clone(), query, return_limit);
    let best_server_score = ranked_server_docs
        .first()
        .map(doc_match_score)
        .unwrap_or_default();

    if ranked_server_docs.is_empty() || best_server_score < DOC_LOCAL_SWEEP_SCORE {
        let all_docs = client
            .list_docs(
                workspace_id,
                project_id,
                doc_type.map(str::to_string),
                is_personal,
                None,
                Some(fetch_limit),
            )
            .await?;
        docs.extend(
            extract_collection_array(&all_docs)
                .cloned()
                .unwrap_or_default(),
        );
    }

    Ok(rank_docs_for_query(dedupe_docs(docs), query, return_limit))
}

fn format_doc_matches(query: &str, docs: &[Value]) -> String {
    let mut text = format!(
        "Found {} docs matching \"{}\":\n\n",
        docs.len(),
        query.trim()
    );

    for (i, doc) in docs.iter().enumerate() {
        let id = doc.get("id").and_then(|v| v.as_str()).unwrap_or("—");
        let title = doc
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("Untitled");
        let doc_type = doc
            .get("doc_type")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        text.push_str(&format!(
            "{}. **{}** (id: {}) [{}]\n",
            i + 1,
            title,
            id,
            doc_type
        ));
    }

    text.push_str(
        "\nUse memory(action=\"get_doc\", doc_id=\"...\") with one of the IDs, or pass the exact title as doc_id.",
    );
    text
}

fn find_exact_doc_match<'a>(docs: &'a [Value], query: &str) -> Option<&'a Value> {
    let normalized_query = normalize_lookup_text(query.trim());
    if normalized_query.is_empty() {
        return None;
    }

    docs.iter().find(|doc| {
        if doc_exact_match(doc) {
            return true;
        }
        let id = doc.get("id").and_then(|v| v.as_str()).unwrap_or("").trim();
        if !id.is_empty() && id.eq_ignore_ascii_case(query.trim()) {
            return true;
        }
        let title = doc
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        !title.is_empty() && normalize_lookup_text(title) == normalized_query
    })
}

fn select_resolved_doc_match<'a>(docs: &'a [Value], query: &str) -> Option<&'a Value> {
    if let Some(exact) = find_exact_doc_match(docs, query) {
        return Some(exact);
    }

    let best = docs.first()?;
    let second_score = docs.get(1).map(doc_match_score).unwrap_or_default();
    if doc_match_score(best) >= DOC_AUTO_OPEN_SCORE && second_score < DOC_AUTO_OPEN_SECONDARY_MAX {
        Some(best)
    } else {
        None
    }
}

fn resolve_uuid_lookup_input(raw: &str) -> Option<Uuid> {
    Uuid::parse_str(raw.trim()).ok()
}

async fn resolve_doc_uuid_for_action(
    client: &ContextStreamClient,
    workspace_id: Option<Uuid>,
    project_id: Option<Uuid>,
    lookup: &str,
    doc_type: Option<&str>,
    is_personal: Option<bool>,
    limit: Option<i64>,
) -> Result<(Uuid, Option<String>)> {
    if let Some(uuid) = resolve_uuid_lookup_input(lookup) {
        return Ok((uuid, None));
    }

    let matches = search_docs_by_title(
        client,
        workspace_id,
        project_id,
        doc_type,
        is_personal,
        lookup,
        limit,
    )
    .await?;

    if matches.is_empty() {
        return Err(Error::Validation(format!(
            "No docs found matching \"{}\". Use memory(action=\"list_docs\", query=\"{}\") to inspect candidates.",
            lookup, lookup
        )));
    }

    if let Some(resolved) = select_resolved_doc_match(&matches, lookup) {
        if let Some(id_str) = resolved.get("id").and_then(|v| v.as_str()) {
            if let Ok(uuid) = Uuid::parse_str(id_str) {
                let note = if id_str.eq_ignore_ascii_case(lookup.trim()) {
                    None
                } else {
                    Some(format!(
                        "Resolved \"{}\" to doc **{}** (id: {}).",
                        lookup,
                        resolved
                            .get("title")
                            .and_then(|v| v.as_str())
                            .unwrap_or("Untitled"),
                        id_str
                    ))
                };
                return Ok((uuid, note));
            }
        }
    }

    let ranked = rank_lookup_matches(&matches, lookup, &["title"], 5);
    resolve_uuid_from_matches("docs", lookup, &ranked)
}

fn resolve_from_collection_lookup(
    entity_name: &str,
    lookup: &str,
    collection: &Value,
    title_fields: &[&str],
) -> Result<(Uuid, Option<String>)> {
    if let Some(uuid) = resolve_uuid_lookup_input(lookup) {
        return Ok((uuid, None));
    }

    let items = extract_collection_array(collection)
        .cloned()
        .unwrap_or_default();
    let ranked = rank_lookup_matches(&items, lookup, title_fields, 8);
    resolve_uuid_from_matches(entity_name, lookup, &ranked)
}

fn extract_memory_search_results(value: &Value) -> Vec<Value> {
    extract_primary_object(value, &["results"])
        .get("results")
        .and_then(|v| v.as_array())
        .cloned()
        .or_else(|| extract_collection_array(value).cloned())
        .unwrap_or_default()
}

fn memory_search_degraded_reason(value: &Value) -> Option<&'static str> {
    let payload = extract_primary_object(value, &["results", "degraded", "degraded_reason"]);
    if !payload
        .get("degraded")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }

    Some(
        match payload.get("degraded_reason").and_then(Value::as_str) {
            Some("vector_timeout") => "vector_timeout",
            Some("vector_unavailable") => "vector_unavailable",
            Some("memory_search_unavailable") => "memory_search_unavailable",
            // Do not promote arbitrary backend text into the MCP envelope.
            _ => "memory_search_degraded",
        },
    )
}

fn memory_search_error_allows_docs_fallback(error: &Error) -> bool {
    match error {
        Error::Network(_) | Error::Timeout(_) => true,
        Error::Http { status, .. } => matches!(status, 408 | 500 | 502 | 503 | 504),
        // Authentication, authorization, validation, billing, and rate-limit
        // errors remain actionable and must not be disguised as an empty hit.
        _ => false,
    }
}

fn normalize_doc_search_result(doc: &Value) -> Value {
    serde_json::json!({
        "entity_type": "doc",
        "id": doc.get("id").and_then(|v| v.as_str()).unwrap_or("unknown"),
        "title": doc.get("title").and_then(|v| v.as_str()).unwrap_or("Untitled"),
        "preview": doc.get("content").and_then(|v| v.as_str()).map(|value| truncate_preview(value, 150)),
        "score": doc_match_score(doc),
        "match_source": doc_match_source(doc),
        "doc_type": doc.get("doc_type").and_then(|v| v.as_str()).unwrap_or("unknown"),
    })
}

fn normalize_memory_search_result(item: &Value) -> Value {
    serde_json::json!({
        "entity_type": "memory",
        "id": extract_memory_result_id(item),
        "title": extract_display_title(item),
        "preview": extract_content_preview(item),
        "score": extract_memory_result_score(item),
        "match_source": "memory_search",
        "node_type": extract_memory_result_type(item),
    })
}

fn result_score(item: &Value) -> i64 {
    item.get("score")
        .and_then(|v| v.as_i64())
        .unwrap_or_default()
}

fn result_entity_rank(item: &Value) -> i64 {
    match item.get("entity_type").and_then(|v| v.as_str()) {
        Some("doc") => 1,
        _ => 0,
    }
}

fn build_hybrid_search_results(
    memory_results: &[Value],
    docs: &[Value],
    limit: usize,
) -> Vec<Value> {
    let mut results = Vec::new();
    results.extend(docs.iter().map(normalize_doc_search_result));
    results.extend(memory_results.iter().map(normalize_memory_search_result));
    results.sort_by(|a, b| {
        result_score(b)
            .cmp(&result_score(a))
            .then_with(|| result_entity_rank(b).cmp(&result_entity_rank(a)))
    });
    results.truncate(limit);
    results
}

fn format_hybrid_search_results(query: &str, results: &[Value]) -> String {
    if results.is_empty() {
        return format!("No memory or doc matches found for \"{}\".", query.trim());
    }

    let mut text = format!(
        "Found {} memory/doc matches for \"{}\":\n\n",
        results.len(),
        query.trim()
    );

    for (i, item) in results.iter().enumerate() {
        let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("unknown");
        let title = item
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("Untitled");
        let label = match item.get("entity_type").and_then(|v| v.as_str()) {
            Some("doc") => format!(
                "doc/{}",
                item.get("doc_type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
            ),
            _ => item
                .get("node_type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string(),
        };

        text.push_str(&format!(
            "{}. [{}] **{}** (id: {})\n",
            i + 1,
            label,
            title,
            id
        ));

        if let Some(preview) = item.get("preview").and_then(|v| v.as_str()) {
            if !preview.trim().is_empty() {
                text.push_str(&format!("   {}\n", preview));
            }
        }

        text.push('\n');
    }

    text
}

fn scope_uuid_to_string(scope: Option<Uuid>) -> Option<String> {
    scope.map(|value| value.to_string())
}

async fn execute_write_with_scope_recovery<P, R, F, Fut, S>(
    client: &ContextStreamClient,
    session: &Arc<mcp_session::SessionManager>,
    raw_workspace_id: Option<&str>,
    raw_project_id: Option<&str>,
    mut params: P,
    set_scope: S,
    operation: F,
) -> Result<(R, ResolvedWriteScope)>
where
    P: Clone,
    F: Fn(ContextStreamClient, P) -> Fut,
    Fut: std::future::Future<Output = Result<R>>,
    S: Fn(&mut P, Option<Uuid>, Option<Uuid>) + Copy,
{
    let mut scope =
        resolve_write_scope(client, session.as_ref(), raw_workspace_id, raw_project_id).await?;
    set_scope(&mut params, scope.workspace_id, scope.project_id);

    match operation(client.clone(), params.clone()).await {
        Ok(result) => Ok((result, scope)),
        Err(err) => {
            scope = recover_write_scope_after_project_error(
                client,
                session.as_ref(),
                raw_workspace_id,
                raw_project_id,
                err,
            )
            .await?;
            set_scope(&mut params, scope.workspace_id, scope.project_id);
            let result = operation(client.clone(), params).await?;
            Ok((result, scope))
        }
    }
}

// ============================================================================
// Memory Search Tool
// ============================================================================

/// Input for memory search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySearchInput {
    pub query: String,
    pub workspace_id: Option<String>,
    pub project_id: Option<String>,
    pub node_type: Option<String>,
    pub limit: Option<i64>,
}

fn build_memory_search_input(
    query: String,
    workspace_id: Option<String>,
    project_id: Option<String>,
    resolved_workspace_id: Option<Uuid>,
    resolved_project_id: Option<Uuid>,
    node_type: Option<String>,
    limit: Option<i64>,
) -> MemorySearchInput {
    MemorySearchInput {
        query,
        workspace_id: workspace_id.or_else(|| scope_uuid_to_string(resolved_workspace_id)),
        project_id: project_id.or_else(|| scope_uuid_to_string(resolved_project_id)),
        node_type,
        limit,
    }
}

/// Memory search tool handler.
pub struct MemorySearchTool {
    client: ContextStreamClient,
}

impl MemorySearchTool {
    pub fn new(client: ContextStreamClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl ToolHandler for MemorySearchTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let input: MemorySearchInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;

        if input.query.trim().is_empty() {
            return Err(Error::Validation("query is required".to_string()));
        }

        let explicit_workspace_id = input
            .workspace_id
            .as_ref()
            .and_then(|s| Uuid::parse_str(s).ok());
        let explicit_project_id = input
            .project_id
            .as_ref()
            .and_then(|s| Uuid::parse_str(s).ok());
        let task_auth = mcp_client::get_task_auth_override();
        let task_workspace_id = task_auth.as_ref().and_then(|auth| auth.workspace_id);
        let workspace_id = explicit_workspace_id.or(task_workspace_id);
        let project_id = explicit_project_id.or_else(|| {
            if explicit_workspace_id.is_some() && explicit_workspace_id != task_workspace_id {
                None
            } else {
                task_auth.as_ref().and_then(|auth| auth.project_id)
            }
        });

        let query = input.query.clone();
        let limit = input.limit;
        let effective_node_type = input
            .node_type
            .clone()
            .or_else(|| infer_memory_query_node_type(&input.query));
        let caller_cache_identity = current_memory_cache_identity();

        // Cache check: identical scope + query + node_type + limit in the
        // warm window → return prior rendered result with [MEMORY_CACHED]
        // marker. Skips the parallel memory-search + docs-search round-trips.
        let cache_key = memory_search_cache_key_for_caller(
            caller_cache_identity.as_deref(),
            workspace_id,
            project_id,
            &query,
            effective_node_type.as_deref(),
            limit,
        );
        if let Some(cache_key) = cache_key.as_deref() {
            if let Some((cached_text, cached_structured)) = memory_search_cache().get(cache_key) {
                tracing::debug!("memory search cache hit: key={}", cache_key);
                let marked = format!(
                    "[MEMORY_CACHED] Same memory search as the previous identical call (<{}s ago); \
                     returning cached result. Change query/node_type/limit to refresh.\n\n{}",
                    MEMORY_SEARCH_CACHE_TTL.as_secs(),
                    cached_text
                );
                return Ok(ToolResult::with_structured(marked, cached_structured));
            }
        }
        let params = MemorySearchParams {
            query: query.clone(),
            workspace_id,
            project_id,
            node_type: effective_node_type,
            limit,
            ..Default::default()
        };

        let tool_deadline = tokio::time::Instant::now() + MEMORY_SEARCH_TOOL_TIMEOUT;
        let memory_client = self.client.clone();
        let docs_client = self.client.clone();
        let (response, docs) = tokio::join!(
            async {
                tokio::time::timeout_at(tool_deadline, memory_client.search_memory(params))
                    .await
                    .map_err(|_| Error::Timeout(MEMORY_SEARCH_TOOL_TIMEOUT.as_secs()))?
            },
            async {
                tokio::time::timeout(
                    MEMORY_SEARCH_DOCS_TIMEOUT,
                    search_docs_by_title(
                        &docs_client,
                        workspace_id,
                        project_id,
                        None,
                        None,
                        &query,
                        limit,
                    ),
                )
                .await
                .map_err(|_| Error::Timeout(MEMORY_SEARCH_DOCS_TIMEOUT.as_secs()))?
            }
        );
        let (response, docs, memory_transport_degraded, docs_degraded) = match (response, docs) {
            (Ok(response), Ok(docs)) => (response, docs, false, false),
            (Err(memory_error), Ok(docs))
                if memory_search_error_allows_docs_fallback(&memory_error) =>
            {
                // The docs branch is independently scope-bound. Preserve its
                // useful evidence when the memory API is unavailable instead
                // of discarding the whole tool response (and tempting callers
                // into another identical request). Keep the marker generic so
                // backend topology and failure details never cross the wire.
                tracing::warn!(error = %memory_error, "memory search API unavailable; returning scoped document fallback");
                (
                    serde_json::json!({
                        "results": [],
                        "total": 0,
                        "degraded": true,
                        "degraded_reason": "memory_search_unavailable",
                    }),
                    docs,
                    true,
                    false,
                )
            }
            (Err(memory_error), Ok(_docs)) => return Err(memory_error),
            (Ok(response), Err(docs_error)) => {
                tracing::warn!(error = %docs_error, "document lookup unavailable; preserving memory search results");
                (response, Vec::new(), false, true)
            }
            (Err(memory_error), Err(_docs_error)) => return Err(memory_error),
        };

        let api_memory_degraded_reason = memory_search_degraded_reason(&response);
        let memory_degraded = memory_transport_degraded || api_memory_degraded_reason.is_some();

        let memory_results = extract_memory_search_results(&response);
        let hybrid_results = build_hybrid_search_results(
            &memory_results,
            &docs,
            limit.unwrap_or(10).clamp(1, 25) as usize,
        );

        let mut doc_detail_degraded = false;
        let resolved_doc = match select_resolved_doc_match(&docs, &query) {
            Some(doc) => {
                let doc_id = doc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                if let Ok(doc_uuid) = Uuid::parse_str(doc_id) {
                    let now = tokio::time::Instant::now();
                    let detail_deadline =
                        std::cmp::min(tool_deadline, now + MEMORY_SEARCH_DOC_DETAIL_TIMEOUT);
                    let detail = if detail_deadline <= now {
                        Err(Error::Timeout(1))
                    } else {
                        match tokio::time::timeout_at(
                            detail_deadline,
                            self.client.get_doc(doc_uuid),
                        )
                        .await
                        {
                            Ok(result) => result,
                            Err(_) => Err(Error::Timeout(
                                MEMORY_SEARCH_DOC_DETAIL_TIMEOUT.as_secs().max(1),
                            )),
                        }
                    };
                    match detail {
                        Ok(doc) => Some(doc),
                        Err(error) => {
                            doc_detail_degraded = true;
                            tracing::warn!(error = %error, "document detail lookup unavailable; preserving ranked matches");
                            None
                        }
                    }
                } else {
                    None
                }
            }
            None => None,
        };

        let text = if let Some(doc) = resolved_doc.as_ref() {
            let related_results = format_hybrid_search_results(&query, &hybrid_results);
            format!(
                "Resolved doc query \"{}\".\n\n{}\n\nRelated matches:\n\n{}",
                query,
                format_doc_detail(doc),
                related_results
            )
        } else {
            format_hybrid_search_results(&query, &hybrid_results)
        };
        let mut notices = Vec::new();
        let mut degraded_sources = Vec::new();
        if memory_degraded {
            degraded_sources.push("memory");
            notices.push("[MEMORY_DEGRADED] Some memory retrieval stages were unavailable; returning scoped fallback evidence without replaying failed requests.");
        }
        if docs_degraded {
            degraded_sources.push("docs");
        }
        if doc_detail_degraded {
            degraded_sources.push("doc_detail");
        }
        if docs_degraded || doc_detail_degraded {
            notices.push(
                "[DOCS_DEGRADED] Document lookup was unavailable; returning scoped memory matches.",
            );
        }
        let degraded = !degraded_sources.is_empty();
        let text = if notices.is_empty() {
            text
        } else {
            format!("{}\n\n{text}", notices.join("\n"))
        };

        let structured = serde_json::json!({
            "query": query,
            "results": hybrid_results,
            "doc_matches": docs,
            "resolved_doc": resolved_doc,
            "raw_memory_search": response,
            "memory_results": memory_results,
            "degraded": degraded,
            "degraded_sources": degraded_sources,
            "memory_degraded_reason": api_memory_degraded_reason
                .or(memory_transport_degraded.then_some("memory_search_unavailable")),
        });

        if !degraded {
            if let Some(cache_key) = cache_key {
                put_memory_search_cache(
                    caller_cache_identity.as_deref(),
                    cache_key,
                    (text.clone(), structured.clone()),
                );
            }
        }

        Ok(ToolResult::with_structured(text, structured))
    }

    fn metadata(&self) -> &ToolMetadata {
        static METADATA: std::sync::OnceLock<ToolMetadata> = std::sync::OnceLock::new();
        METADATA.get_or_init(|| ToolMetadata {
            name: "memory_search".to_string(),
            title: "Search Memory".to_string(),
            description:
                "Search the project memory for facts, decisions, preferences, and lessons, and return relevant docs alongside memory nodes when they match the query. Reuse the current project_id from init/context for project-scoped lookups."
                    .to_string(),
            category: ToolCategory::Memory,
            annotations: ToolAnnotations::read_only(),
            is_pro: false,
            required_tier: None,
        })
    }

    fn input_schema(&self) -> Value {
        SchemaBuilder::new()
            .description("Search project memory")
            .string("query", "Search query", true)
            .uuid(
                "workspace_id",
                "Workspace ID. Reuse the current workspace_id from init/context when overriding session scope.",
                false,
            )
            .uuid(
                "project_id",
                "Project ID. Reuse the current project_id returned by init/context for project-scoped memory lookups.",
                false,
            )
            .string_enum("node_type", "Filter by node type", VALID_NODE_TYPES, false)
            .integer("limit", "Maximum results", false)
            .build()
    }
}

// ============================================================================
// Memory Create Node Tool
// ============================================================================

/// Input for creating a memory node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateMemoryNodeInput {
    pub node_type: String,
    pub title: String,
    pub content: Option<String>,
    pub workspace_id: Option<String>,
    pub project_id: Option<String>,
    pub metadata: Option<Value>,
}

/// Create memory node tool handler.
pub struct CreateMemoryNodeTool {
    client: ContextStreamClient,
    session: Arc<mcp_session::SessionManager>,
}

impl CreateMemoryNodeTool {
    pub fn new(client: ContextStreamClient, session: Arc<mcp_session::SessionManager>) -> Self {
        Self { client, session }
    }
}

#[async_trait]
impl ToolHandler for CreateMemoryNodeTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let input: CreateMemoryNodeInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;

        let node_type = normalize_node_type(&input.node_type)?;

        if input.title.trim().is_empty() {
            return Err(Error::Validation("title is required".to_string()));
        }

        let params = CreateMemoryNodeParams {
            node_type,
            title: input.title.clone(),
            content: input.content,
            workspace_id: None,
            project_id: None,
            metadata: input.metadata,
        };

        let (mut result, scope) = execute_write_with_scope_recovery(
            &self.client,
            &self.session,
            input.workspace_id.as_deref(),
            input.project_id.as_deref(),
            params,
            |params, workspace_id, project_id| {
                params.workspace_id = workspace_id;
                params.project_id = project_id;
            },
            |client, params| async move { client.create_memory_node(params).await },
        )
        .await?;
        attach_scope_recovery_metadata(&mut result, &scope);

        let node_type = result
            .get("node_type")
            .or_else(|| result.get("type"))
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let title = result
            .get("title")
            .or_else(|| result.get("summary"))
            .and_then(|v| v.as_str())
            .unwrap_or("Untitled");
        let id = result
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");

        let text = format!("Created {} node: {}\nID: {}", node_type, title, id);

        let mut output = ToolResult::with_structured(text, result);
        if let Some(note) = scope.note {
            output = output.with_prefix(format!("{}\n", note));
        }
        Ok(output)
    }

    fn metadata(&self) -> &ToolMetadata {
        static METADATA: std::sync::OnceLock<ToolMetadata> = std::sync::OnceLock::new();
        METADATA.get_or_init(|| ToolMetadata {
            name: "memory_create_node".to_string(),
            title: "Create Memory Node".to_string(),
            description: "Create a new memory node (fact, decision, preference, constraint, habit, or lesson).".to_string(),
            category: ToolCategory::Memory,
            annotations: ToolAnnotations::write(),
            is_pro: false,
            required_tier: None,
        })
    }

    fn input_schema(&self) -> Value {
        SchemaBuilder::new()
            .description("Create a memory node")
            .string_enum(
                "node_type",
                "Type of node to create",
                VALID_NODE_TYPES,
                true,
            )
            .string("title", "Short descriptive title", true)
            .string("content", "Full content/details", false)
            .uuid("workspace_id", "Workspace ID", false)
            .uuid("project_id", "Project ID", false)
            .build()
    }
}

// ============================================================================
// Memory Decisions Tool
// ============================================================================

/// Input for listing decisions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryDecisionsInput {
    pub workspace_id: Option<String>,
    pub project_id: Option<String>,
    pub query: Option<String>,
    /// TypeScript uses `category` for filtering decisions; accept both.
    pub category: Option<String>,
    pub limit: Option<i64>,
    /// `recency` | `relevance` (server default: relevance).
    #[serde(default)]
    pub sort: Option<String>,
    /// `active` | `superseded` | `disputed` | `verified` | `all` (server default: active).
    #[serde(default)]
    pub status: Option<String>,
    /// ISO-8601 lower bound on the decision timestamp.
    #[serde(default)]
    pub since: Option<String>,
    #[serde(default)]
    pub offset: Option<i64>,
    #[serde(default)]
    pub source: Option<String>,
}

/// Valid `sort` values for the typed decisions listing.
pub const DECISION_SORTS: &[&str] = &["recency", "relevance"];
/// Valid `status` filters for the typed decisions listing.
pub const DECISION_STATUSES: &[&str] = &["active", "superseded", "disputed", "verified", "all"];

fn normalize_decision_sort(raw: Option<&str>) -> Result<Option<String>> {
    match raw.map(str::trim).filter(|value| !value.is_empty()) {
        None => Ok(None),
        Some(value) => {
            let lower = value.to_ascii_lowercase();
            if DECISION_SORTS.contains(&lower.as_str()) {
                Ok(Some(lower))
            } else {
                Err(Error::Validation(format!(
                    "Invalid sort '{value}'. Use one of: {}",
                    DECISION_SORTS.join(", ")
                )))
            }
        }
    }
}

fn normalize_decision_status(raw: Option<&str>) -> Result<Option<String>> {
    match raw.map(str::trim).filter(|value| !value.is_empty()) {
        None => Ok(None),
        Some(value) => {
            let lower = value.to_ascii_lowercase();
            if DECISION_STATUSES.contains(&lower.as_str()) {
                Ok(Some(lower))
            } else {
                Err(Error::Validation(format!(
                    "Invalid status '{value}'. Use one of: {}",
                    DECISION_STATUSES.join(", ")
                )))
            }
        }
    }
}

fn decision_str<'a>(node: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|key| {
        node.get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .or_else(|| {
                node.get("metadata")
                    .and_then(|meta| meta.get(*key))
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
            })
    })
}

/// Render one `[PARTIAL]` line per `degraded` entry of a typed envelope.
pub(crate) fn render_degraded_lines(envelope: &Value) -> String {
    let mut text = String::new();
    if let Some(entries) = envelope.get("degraded").and_then(Value::as_array) {
        for entry in entries {
            let source = entry
                .get("source")
                .and_then(Value::as_str)
                .unwrap_or("server");
            let detail = entry
                .get("detail")
                .and_then(Value::as_str)
                .or_else(|| entry.get("reason").and_then(Value::as_str))
                .unwrap_or("degraded");
            text.push_str(&format!("[PARTIAL] {source}: {detail}\n"));
        }
    }
    text
}

/// Render the `decisions.v1` envelope as typed `[DECISION]` lines.
///
/// Missing typed fields render as `unknown` / `none`; nothing is inferred.
pub(crate) fn render_decisions_envelope(
    envelope: &Value,
    requested_sort: Option<&str>,
    requested_status: Option<&str>,
) -> String {
    let items = envelope
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let total = envelope
        .get("total")
        .and_then(Value::as_u64)
        .map(|value| value as usize)
        .unwrap_or(items.len());
    let sort = envelope
        .get("sort")
        .and_then(Value::as_str)
        .or(requested_sort)
        .unwrap_or("relevance");
    let status_filter = requested_status.unwrap_or("active");
    let mut text = String::new();

    if items.is_empty() {
        text.push_str(&format!(
            "No decisions recorded yet (status={status_filter}).\n"
        ));
    } else {
        text.push_str(&format!(
            "[DECISIONS] {} of {} decision(s) (sort={sort}, status={status_filter}):\n\n",
            items.len(),
            total
        ));
        for (index, node) in items.iter().enumerate() {
            let title = extract_display_title(node);
            let status = decision_str(node, &["status"]).unwrap_or("unknown");
            let freshness = decision_str(node, &["freshness"]).unwrap_or("unknown");
            let category = decision_str(node, &["category"]).unwrap_or("none");
            let id = decision_str(node, &["id", "node_id", "decision_id"]).unwrap_or("unknown");
            text.push_str(&format!(
                "{}. [DECISION] {title} — status={status} freshness={freshness} category={category} id={id}\n",
                index + 1
            ));
            if let Some(content) = node
                .get("content")
                .or_else(|| node.get("details"))
                .or_else(|| node.get("description"))
                .and_then(Value::as_str)
            {
                let preview: String = content.chars().take(200).collect();
                text.push_str(&format!("   {}\n", preview.replace('\n', " ")));
            }
            if let Some(rationale) = node
                .get("structured")
                .and_then(|value| value.get("rationale"))
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
            {
                let preview: String = rationale.chars().take(160).collect();
                text.push_str(&format!("   rationale: {}\n", preview.replace('\n', " ")));
            }
            let mut links = Vec::new();
            if let Some(value) = decision_str(node, &["superseded_by"]) {
                links.push(format!("superseded_by={value}"));
            }
            if let Some(value) = decision_str(node, &["supersedes"]) {
                links.push(format!("supersedes={value}"));
            }
            if let Some(value) = decision_str(node, &["source"]) {
                links.push(format!("source={value}"));
            }
            if let Some(score) = node.get("rank_score").and_then(Value::as_f64) {
                links.push(format!("rank_score={score:.2}"));
            }
            if !links.is_empty() {
                text.push_str(&format!("   {}\n", links.join(" ")));
            }
            let created_at = node
                .get("created_at")
                .or_else(|| node.get("createdAt"))
                .or_else(|| node.get("timestamp"))
                .or_else(|| node.get("date"))
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            text.push_str(&format!("   Created: {created_at}\n\n"));
        }
        if let Some(next_offset) = envelope.get("next_offset").and_then(Value::as_u64) {
            text.push_str(&format!(
                "Next offset: {next_offset} (pass offset={next_offset} to continue).\n"
            ));
        }
    }
    text.push_str(&render_degraded_lines(envelope));
    text
}

// ============================================================================
// Typed decision create / actions (shared by memory() and session(capture))
// ============================================================================

/// Input for `memory(action="create_decision")` and the structured route of
/// `session(action="capture", event_type="decision")`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreateDecisionInput {
    pub workspace_id: Option<String>,
    pub project_id: Option<String>,
    pub title: String,
    pub content: Option<String>,
    pub rationale: Option<String>,
    /// Strings or `{option, rejected_reason}` objects.
    pub alternatives: Option<Vec<Value>>,
    pub scope: Option<String>,
    pub confidence: Option<f64>,
    /// Decision id or lookup text of the decision this one replaces.
    pub supersedes: Option<String>,
    pub category: Option<String>,
    pub tags: Option<Vec<String>>,
    pub session_id: Option<String>,
}

/// Input for `memory(action="decision_action")`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DecisionActionInput {
    pub workspace_id: Option<String>,
    pub project_id: Option<String>,
    /// Decision id or lookup text.
    pub decision_id: String,
    /// `supersede` | `dispute` | `verify` | `invalidate` | `choose_successor`
    pub decision_action: String,
    /// Successor decision id or lookup text (supersede / choose_successor).
    pub successor_id: Option<String>,
    pub reason: Option<String>,
    /// Title for a successor created inline by the server.
    pub title: Option<String>,
}

/// True when any typed decision field is present, which routes a plain
/// `capture(event_type="decision")` to the typed create endpoint.
pub fn has_structured_decision_fields(
    rationale: Option<&str>,
    alternatives: Option<&[Value]>,
    scope: Option<&str>,
    confidence: Option<f64>,
) -> bool {
    rationale
        .map(str::trim)
        .is_some_and(|value| !value.is_empty())
        || alternatives.is_some_and(|value| !value.is_empty())
        || scope.map(str::trim).is_some_and(|value| !value.is_empty())
        || confidence.is_some()
}

fn normalize_alternatives(raw: &[Value]) -> Vec<Value> {
    raw.iter()
        .filter_map(|entry| match entry {
            Value::String(option) => {
                let option = option.trim();
                (!option.is_empty()).then(|| serde_json::json!({ "option": option }))
            }
            Value::Object(_) => Some(entry.clone()),
            _ => None,
        })
        .collect()
}

fn compose_decision_content(
    content: &str,
    rationale: Option<&str>,
    alternatives: &[Value],
    scope: Option<&str>,
    confidence: Option<f64>,
) -> String {
    let mut text = content.trim().to_string();
    if let Some(rationale) = rationale.map(str::trim).filter(|value| !value.is_empty()) {
        text.push_str(&format!("\n\n### Rationale\n{rationale}"));
    }
    if !alternatives.is_empty() {
        text.push_str("\n\n### Alternatives considered");
        for alternative in alternatives {
            let option = alternative
                .get("option")
                .and_then(Value::as_str)
                .unwrap_or("(unnamed)");
            match alternative.get("rejected_reason").and_then(Value::as_str) {
                Some(reason) if !reason.trim().is_empty() => {
                    text.push_str(&format!("\n- {option} — rejected: {reason}"))
                }
                _ => text.push_str(&format!("\n- {option}")),
            }
        }
    }
    if let Some(scope) = scope.map(str::trim).filter(|value| !value.is_empty()) {
        text.push_str(&format!("\n\n**Scope:** {scope}"));
    }
    if let Some(confidence) = confidence {
        text.push_str(&format!("\n**Confidence:** {confidence}"));
    }
    text
}

fn format_decision_candidates(lookup: &str, matches: &[LookupMatch]) -> String {
    let mut text = format!(
        "Multiple decisions match \"{}\". Retry with an explicit decision id:\n\n",
        lookup
    );
    for (index, item) in matches.iter().take(5).enumerate() {
        text.push_str(&format!(
            "{}. **{}** (id: {})\n",
            index + 1,
            item.title,
            item.id
        ));
    }
    text
}

/// Resolve a decision id from a UUID or lookup text against the typed
/// decisions listing (all statuses). A single high-confidence match wins;
/// several close matches return the candidate list as a validation error.
pub(crate) async fn resolve_decision_id(
    client: &ContextStreamClient,
    workspace_id: Option<Uuid>,
    project_id: Option<Uuid>,
    lookup: &str,
) -> Result<(Uuid, Option<String>)> {
    let lookup = lookup.trim();
    if let Ok(id) = Uuid::parse_str(lookup) {
        return Ok((id, None));
    }
    if lookup.is_empty() {
        return Err(Error::Validation("decision_id is required".to_string()));
    }
    let envelope = client
        .list_decisions_envelope(mcp_client::ListDecisionsParams {
            workspace_id,
            project_id,
            query: Some(lookup.to_string()),
            status: Some("all".to_string()),
            limit: Some(25),
            ..Default::default()
        })
        .await?;
    let items = envelope
        .get("items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let ranked = rank_lookup_matches(&items, lookup, &["title", "summary", "name"], 8);
    let best = ranked.first().ok_or_else(|| {
        Error::Validation(format!(
            "No decisions match \"{lookup}\". Use memory(action=\"decisions\", query=\"{lookup}\", status=\"all\") to inspect candidates."
        ))
    })?;
    let second_score = ranked.get(1).map(|item| item.score).unwrap_or_default();
    if !best.exact && second_score > 0 && best.score <= second_score + 200 {
        return Err(Error::Validation(format_decision_candidates(
            lookup, &ranked,
        )));
    }
    let note = (!best.exact).then(|| {
        format!(
            "Resolved decision \"{}\" to **{}** (id: {}).",
            lookup, best.title, best.id
        )
    });
    Ok((best.id, note))
}

/// Create a typed decision (`POST /memory/decisions`). When the server
/// answers 404 the decision is stored as a decision event through
/// `/memory/events` with the structured fields kept in `provenance`, and the
/// tool text says so with a `[PARTIAL]` line.
pub async fn execute_create_decision(
    client: &ContextStreamClient,
    session: &mcp_session::SessionManager,
    input: CreateDecisionInput,
) -> Result<ToolResult> {
    let title = input.title.trim().to_string();
    if title.is_empty() {
        return Err(Error::Validation("title is required".to_string()));
    }
    let content = input
        .content
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or_else(|| {
            input
                .rationale
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
        .ok_or_else(|| Error::Validation("content (or rationale) is required".to_string()))?
        .to_string();

    let scope = resolve_write_scope(
        client,
        session,
        input.workspace_id.as_deref(),
        input.project_id.as_deref(),
    )
    .await?;

    let alternatives = input
        .alternatives
        .as_deref()
        .map(normalize_alternatives)
        .filter(|list| !list.is_empty());
    let supersedes = match input
        .supersedes
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(lookup) => Some(
            resolve_decision_id(client, scope.workspace_id, scope.project_id, lookup)
                .await?
                .0,
        ),
        None => None,
    };
    let decision_scope = input
        .scope
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    let params = mcp_client::CreateDecisionParams {
        workspace_id: scope.workspace_id,
        project_id: scope.project_id,
        title: title.clone(),
        content: content.clone(),
        rationale: input.rationale.clone(),
        alternatives: alternatives.clone(),
        scope: decision_scope.clone(),
        confidence: input.confidence,
        supersedes,
        category: input.category.clone(),
        tags: input.tags.clone(),
        session_id: input.session_id.clone(),
    };

    match client.create_decision(params).await {
        Ok(mut result) => {
            let id = result
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let mut extras = Vec::new();
            if let Some(node_id) = result.get("node_id").and_then(Value::as_str) {
                extras.push(format!("node_id: {node_id}"));
            }
            if let Some(event_id) = result.get("event_id").and_then(Value::as_str) {
                extras.push(format!("event_id: {event_id}"));
            }
            let deduplicated = result
                .get("deduplicated")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let mut text = format!("Decision recorded: {title} (id: {id}");
            if !extras.is_empty() {
                text.push_str(&format!(", {}", extras.join(", ")));
            }
            text.push(')');
            if deduplicated {
                text.push_str(" — deduplicated against an existing decision");
            }
            text.push_str(".\nProgress: completed.");
            if let Some(obj) = result.as_object_mut() {
                obj.insert(
                    "operation_status".to_string(),
                    serde_json::json!({"operation": "create_decision", "state": "completed"}),
                );
            }
            let mut output = ToolResult::with_structured(text, result);
            if let Some(note) = scope.note {
                output = output.with_prefix(format!("{note}\n"));
            }
            Ok(output)
        }
        Err(err) if is_not_found_error(&err) => {
            let alternatives_list = alternatives.clone().unwrap_or_default();
            let composed = compose_decision_content(
                &content,
                input.rationale.as_deref(),
                &alternatives_list,
                decision_scope.as_deref(),
                input.confidence,
            );
            let structured = serde_json::json!({
                "rationale": input.rationale,
                "alternatives": alternatives,
                "scope": decision_scope,
                "confidence": input.confidence,
                "supersedes": supersedes,
                "category": input.category,
            });
            let capture = client
                .session_capture(SessionCaptureParams {
                    workspace_id: scope.workspace_id,
                    project_id: scope.project_id,
                    event_type: Some("decision".to_string()),
                    title: title.clone(),
                    content: composed,
                    tags: input.tags.clone(),
                    session_id: input.session_id.clone(),
                    provenance: Some(serde_json::json!({
                        "source": "create_decision_fallback",
                        "structured": structured,
                    })),
                    ..Default::default()
                })
                .await?;
            let event_id = capture
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_string();
            let text = format!(
                "Decision recorded as a memory event: {title} (ID: {event_id}).\n[PARTIAL] typed decision endpoint unavailable (404 on POST /memory/decisions); stored via /memory/events with the structured fields in provenance.\nProgress: completed."
            );
            let structured = serde_json::json!({
                "id": event_id,
                "event_id": event_id,
                "node_id": Value::Null,
                "deduplicated": false,
                "fallback": "memory_events",
                "degraded": [{
                    "source": "decisions_create",
                    "detail": "POST /memory/decisions returned 404; decision stored as a memory event"
                }],
                "raw": capture,
                "operation_status": {"operation": "create_decision", "state": "completed"},
            });
            let mut output = ToolResult::with_structured(text, structured);
            if let Some(note) = scope.note {
                output = output.with_prefix(format!("{note}\n"));
            }
            Ok(output)
        }
        Err(err) => Err(err),
    }
}

/// Apply a lifecycle action to a decision (`POST /memory/decisions/:id/actions`).
///
/// Only `supersede` with a successor has an events-era fallback
/// (`/memory/nodes/:id/supersede`); every other action fails honestly when
/// the typed endpoint is absent.
pub async fn execute_decision_action(
    client: &ContextStreamClient,
    session: &mcp_session::SessionManager,
    input: DecisionActionInput,
) -> Result<ToolResult> {
    let action = input.decision_action.trim().to_ascii_lowercase();
    if !mcp_client::DECISION_ACTIONS.contains(&action.as_str()) {
        return Err(Error::Validation(format!(
            "Invalid decision_action '{}'. Use one of: {}",
            input.decision_action,
            mcp_client::DECISION_ACTIONS.join(", ")
        )));
    }
    let scope = resolve_read_scope(
        client,
        session,
        input.workspace_id.as_deref(),
        input.project_id.as_deref(),
    )
    .await?;
    let (decision_id, resolution_note) = resolve_decision_id(
        client,
        scope.workspace_id,
        scope.project_id,
        &input.decision_id,
    )
    .await?;
    let successor = match input
        .successor_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(lookup) => Some(
            resolve_decision_id(client, scope.workspace_id, scope.project_id, lookup)
                .await?
                .0,
        ),
        None => None,
    };
    if matches!(action.as_str(), "supersede" | "choose_successor")
        && successor.is_none()
        && input
            .title
            .as_deref()
            .map(str::trim)
            .is_none_or(str::is_empty)
    {
        return Err(Error::Validation(format!(
            "successor_id (or title for a new successor) is required for decision_action=\"{action}\""
        )));
    }

    let mut text = String::new();
    if let Some(note) = resolution_note {
        text.push_str(&note);
        text.push('\n');
    }
    let params = mcp_client::DecisionActionParams {
        action: action.clone(),
        successor_id: successor,
        reason: input.reason.clone(),
        title: input.title.clone(),
    };
    match client.decision_action(decision_id, params).await {
        Ok(result) => {
            let applied = result
                .get("applied")
                .and_then(Value::as_bool)
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let decision = result.get("decision").cloned().unwrap_or(Value::Null);
            let title = if decision.is_object() {
                extract_display_title(&decision)
            } else {
                decision_id.to_string()
            };
            let status = decision_str(&decision, &["status"]).unwrap_or("unknown");
            text.push_str(&format!(
                "[DECISION_ACTION] {action} applied={applied} — {title} (id={decision_id}) status={status}\n"
            ));
            if let Some(successor) = successor {
                text.push_str(&format!("   successor={successor}\n"));
            }
            text.push_str(&render_degraded_lines(&result));
            let mut output = ToolResult::with_structured(text.trim_end().to_string(), result);
            if let Some(note) = scope.note {
                output = output.with_prefix(format!("{note}\n"));
            }
            Ok(output)
        }
        Err(err) if is_not_found_error(&err) => {
            if action == "supersede" {
                if let Some(successor) = successor {
                    let linked = client
                        .link_node_superseded_by(decision_id, successor)
                        .await?;
                    text.push_str(&format!(
                        "[DECISION_ACTION] supersede applied via /memory/nodes/{decision_id}/supersede — {decision_id} → {successor}\n[PARTIAL] typed decision actions unavailable (404 on POST /memory/decisions/{decision_id}/actions); only the supersede link was written, status/freshness were not updated."
                    ));
                    let structured = serde_json::json!({
                        "applied": true,
                        "action": action,
                        "decision_id": decision_id,
                        "successor_id": successor,
                        "fallback": "memory_nodes_supersede",
                        "degraded": [{
                            "source": "decision_actions",
                            "detail": "POST /memory/decisions/:id/actions returned 404; used /memory/nodes/:id/supersede"
                        }],
                        "raw": linked,
                    });
                    return Ok(ToolResult::with_structured(text, structured));
                }
            }
            Err(Error::Validation(format!(
                "decision_action \"{action}\" needs the typed decisions endpoint (POST /memory/decisions/{decision_id}/actions), which this server does not expose (404). No fallback exists for this action; nothing was changed."
            )))
        }
        Err(err) => Err(err),
    }
}

/// Memory decisions tool handler.
pub struct MemoryDecisionsTool {
    client: ContextStreamClient,
    session: Arc<mcp_session::SessionManager>,
    atlas_layer: mcp_types::atlas_layer::AtlasLayer,
}

impl MemoryDecisionsTool {
    pub fn new(client: ContextStreamClient) -> Self {
        Self::with_session(
            client.clone(),
            Arc::new(mcp_session::SessionManager::new(
                client,
                mcp_types::config::Config::default(),
            )),
        )
    }

    pub fn with_session(
        client: ContextStreamClient,
        session: Arc<mcp_session::SessionManager>,
    ) -> Self {
        Self::with_session_and_atlas(client, session, mcp_types::atlas_layer::noop_layer())
    }

    pub fn with_session_and_atlas(
        client: ContextStreamClient,
        session: Arc<mcp_session::SessionManager>,
        atlas_layer: mcp_types::atlas_layer::AtlasLayer,
    ) -> Self {
        Self {
            client,
            session,
            atlas_layer,
        }
    }
}

#[async_trait]
impl ToolHandler for MemoryDecisionsTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let input: MemoryDecisionsInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;

        let scope = resolve_read_scope(
            &self.client,
            self.session.as_ref(),
            input.workspace_id.as_deref(),
            input.project_id.as_deref(),
        )
        .await?;

        if scope.workspace_id.is_none() {
            let mut text = String::from(
                "⚠️ [SETUP_REQUIRED] Decisions cannot be loaded because ContextStream has no resolved workspace_id.",
            );
            text.push_str(
                "\nRun `init(folder_path=\"/absolute/path/to/project\")` and reuse the returned `workspace_id`; if init reports a stale or inaccessible workspace, check workspace access or pass `workspace_id` explicitly.",
            );
            if let Some(note) = scope.note.as_deref() {
                text.push('\n');
                text.push_str(note);
            }

            let structured = serde_json::json!({
                "setup_required": true,
                "workspace_id": null,
                "project_id": scope.project_id,
                "query": input.query,
                "category": input.category,
                "limit": input.limit.or(Some(50)),
                "summary": "Decisions setup required",
                "scope_recovery_note": scope.note,
            });
            return Ok(ToolResult::with_structured(text, structured));
        }

        let sort = normalize_decision_sort(input.sort.as_deref())?;
        let status = normalize_decision_status(input.status.as_deref())?;
        let offset = input.offset.filter(|value| *value > 0);
        let limit = input.limit.or(Some(50));
        // The DecisionsHot warm cache folds (workspace, project, query) only;
        // any typed filter bypasses it so a filtered call never serves a
        // differently-filtered payload.
        let cacheable = input.category.is_none()
            && input.since.is_none()
            && input.source.is_none()
            && offset.is_none()
            && sort.is_none()
            && status.is_none();

        let query_clone = input.query.clone();
        let user_scope_token = super::atlas_warm_cache::current_user_scope_token();
        let cached_response = match (cacheable, scope.workspace_id) {
            (true, Some(ws)) => {
                let scope_obj = mcp_types::atlas_layer::AtlasFederationScope {
                    workspace_id: ws,
                    project_id: scope.project_id,
                    scope_hash: super::atlas_warm_cache::scope_hash_for_decisions_hot(
                        ws,
                        user_scope_token.as_deref(),
                        scope.project_id,
                        query_clone.as_deref(),
                    ),
                    user_scope: user_scope_token.clone(),
                };
                super::atlas_warm_cache::try_lookup(
                    &self.atlas_layer,
                    mcp_types::atlas_layer::AtlasWarmCacheKind::DecisionsHot,
                    scope_obj,
                    150, // primary baseline ms
                )
                .await
            }
            _ => None,
        };
        let envelope = if let Some(bundle) = cached_response {
            mcp_client::normalize_decisions_envelope(bundle.payload, sort.as_deref())
        } else {
            let params = mcp_client::ListDecisionsParams {
                workspace_id: scope.workspace_id,
                project_id: scope.project_id,
                query: input.query.clone(),
                category: input.category.clone(),
                sort: sort.clone(),
                status: status.clone(),
                since: input.since.clone(),
                offset,
                limit,
                source: input.source.clone(),
            };
            let envelope = self.client.list_decisions_envelope(params).await?;
            if let (true, Some(ws)) = (cacheable, scope.workspace_id) {
                let scope_obj = mcp_types::atlas_layer::AtlasFederationScope {
                    workspace_id: ws,
                    project_id: scope.project_id,
                    scope_hash: super::atlas_warm_cache::scope_hash_for_decisions_hot(
                        ws,
                        user_scope_token.as_deref(),
                        scope.project_id,
                        query_clone.as_deref(),
                    ),
                    user_scope: user_scope_token.clone(),
                };
                super::atlas_warm_cache::put_in_background(
                    self.atlas_layer.clone(),
                    mcp_types::atlas_layer::AtlasWarmCacheKind::DecisionsHot,
                    scope_obj,
                    envelope.clone(),
                );
            }
            envelope
        };

        let text = render_decisions_envelope(&envelope, sort.as_deref(), status.as_deref());
        let mut output = ToolResult::with_structured(text, envelope);
        if let Some(note) = scope.note.as_deref() {
            output = output.with_prefix(format!("{}\n", note));
        }
        Ok(output)
    }

    fn metadata(&self) -> &ToolMetadata {
        static METADATA: std::sync::OnceLock<ToolMetadata> = std::sync::OnceLock::new();
        METADATA.get_or_init(|| ToolMetadata {
            name: "memory_decisions".to_string(),
            title: "List Decisions".to_string(),
            description: "List recorded architectural and design decisions.".to_string(),
            category: ToolCategory::Memory,
            annotations: ToolAnnotations::read_only(),
            is_pro: false,
            required_tier: None,
        })
    }

    fn input_schema(&self) -> Value {
        SchemaBuilder::new()
            .description("List decisions (typed decisions.v1 envelope)")
            .uuid("workspace_id", "Workspace ID", false)
            .uuid("project_id", "Project ID", false)
            .string("query", "Optional search query to filter decisions", false)
            .string("category", "Optional category filter", false)
            .string_enum("sort", "recency or relevance", DECISION_SORTS, false)
            .string_enum(
                "status",
                "active (default), superseded, disputed, verified, or all",
                DECISION_STATUSES,
                false,
            )
            .string("since", "ISO-8601 lower bound on decision time", false)
            .integer("offset", "Pagination offset", false)
            .integer("limit", "Maximum results", false)
            .build()
    }
}

// ============================================================================
// Unified Memory Tool
// ============================================================================

/// Valid event types for memory. Extended in the Phase 3 taxonomy expansion
/// with recurring-signal + product/design event types: `standup`,
/// `status_update`, `question`, `approval`, `feedback` (customer-facing —
/// distinct from internal `frustration`), `discovery`, `achievement`.
const VALID_EVENT_TYPES: &[&str] = &[
    "decision",
    "preference",
    "insight",
    "uncategorized",
    "note",
    "general",
    "manual_note",
    "implementation",
    "operation",
    "command_execution",
    "file_operation",
    "task",
    "bug",
    "feature",
    "correction",
    "lesson",
    "warning",
    "frustration",
    "conversation",
    "session_snapshot",
    "standup",
    "status_update",
    "question",
    "approval",
    "feedback",
    "discovery",
    "achievement",
];

fn is_reserved_plan_event_type(event_type: &str) -> bool {
    event_type.trim().eq_ignore_ascii_case("plan")
}

fn reserved_plan_event_error() -> Error {
    Error::Validation(
        "event_type=\"plan\" is reserved for session(action=\"capture_plan\"). Do not save plans as memory events; use capture_plan with detailed steps and linked tasks."
            .to_string(),
    )
}

/// Valid task statuses.
const VALID_TASK_STATUSES: &[&str] = &[
    "pending",
    "in_progress",
    "completed",
    "blocked",
    "cancelled",
];

fn require_explicit_task_workspace(raw_workspace_id: Option<&str>) -> Result<()> {
    if raw_workspace_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .is_some()
    {
        return Ok(());
    }

    Err(Error::Validation(
        "workspace_id is required for every memory task action. Reuse the exact workspace_id returned by init/context and pass it explicitly; do not rely on implicit session scope."
            .to_string(),
    ))
}

/// Valid todo priorities.
const VALID_TODO_PRIORITIES: &[&str] = &["low", "medium", "high", "urgent"];

/// Valid diagram types.
const VALID_DIAGRAM_TYPES: &[&str] = &[
    "flowchart",
    "sequence",
    "class",
    "er",
    "gantt",
    "mindmap",
    "pie",
    "other",
];

fn diagram_types_help_suffix() -> &'static str {
    "Diagram types: flowchart (process or decision flow), sequence (service/API interactions), class (object model), er (database entities and relationships), gantt (timeline), mindmap (ideas and taxonomy), pie (distribution), other. Examples: flowchart for ingest pipelines, sequence for MCP/API request handoffs, class for domain models, er for Postgres schemas, gantt for release plans, mindmap for discovery notes, pie for usage mix."
}

/// Valid doc types. Extended across Phase 1-4 of the taxonomy expansion:
/// - Phase 1: `runbook`
/// - Phase 2 (eng/SRE): `adr`, `rfc`, `postmortem`, `retro`, `release_notes`,
///   `playbook`
/// - Phase 3 (product/design): `prd`, `user_story`, `persona`, `interview`,
///   `design_spec`, `critique`, `glossary`
/// - Phase 4 (long tail): `oncall_schedule`, `slo`, `q_and_a`, `changelog`,
///   `style_guide`
const VALID_DOC_TYPES: &[&str] = &[
    "roadmap",
    "spec",
    "runbook",
    "adr",
    "rfc",
    "postmortem",
    "retro",
    "release_notes",
    "playbook",
    "prd",
    "user_story",
    "persona",
    "interview",
    "design_spec",
    "critique",
    "glossary",
    "oncall_schedule",
    "slo",
    "q_and_a",
    "changelog",
    "style_guide",
    "general",
];

/// Input for the unified memory tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryInput {
    pub action: String,
    // Common fields
    pub query: Option<String>,
    pub scope: Option<String>,
    pub workspace_id: Option<String>,
    pub project_id: Option<String>,
    pub target_project: Option<String>,
    pub limit: Option<i64>,
    // Node fields
    pub node_type: Option<String>,
    pub node_id: Option<String>,
    pub title: Option<String>,
    pub content: Option<String>,
    // Event fields
    pub event_type: Option<String>,
    pub event_id: Option<String>,
    /// Bulk delete: for delete_node/delete_event, remove ALL exact (id or
    /// exact-title) matches of node_id/event_id in one call.
    #[serde(default)]
    pub delete_all: Option<bool>,
    pub metadata: Option<Value>,
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    pub events: Option<Vec<Value>>,
    // Node supersede fields
    pub new_content: Option<String>,
    pub reason: Option<String>,
    // Decision fields (decisions / create_decision / decision_action)
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub sort: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub since: Option<String>,
    #[serde(default)]
    pub offset: Option<i64>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub rationale: Option<String>,
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    pub alternatives: Option<Vec<Value>>,
    #[serde(default)]
    pub confidence: Option<f64>,
    #[serde(default)]
    pub supersedes: Option<String>,
    #[serde(default)]
    pub decision_id: Option<String>,
    #[serde(default)]
    pub decision_action: Option<String>,
    #[serde(default)]
    pub successor_id: Option<String>,
    // Task fields
    pub task_id: Option<String>,
    pub description: Option<String>,
    pub priority: Option<String>,
    pub task_status: Option<String>,
    pub plan_id: Option<String>,
    pub plan_step_id: Option<String>,
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    pub tags: Option<Vec<String>>,
    pub blocked_reason: Option<String>,
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    pub code_refs: Option<Vec<serde_json::Value>>,
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    pub task_ids: Option<Vec<String>>,
    pub order: Option<i64>,
    // Todo fields
    pub todo_id: Option<String>,
    pub todo_priority: Option<String>,
    pub todo_status: Option<String>,
    pub due_at: Option<String>,
    pub clear_due_at: Option<bool>,
    pub created_after: Option<String>,
    pub created_before: Option<String>,
    pub updated_after: Option<String>,
    pub updated_before: Option<String>,
    pub due_after: Option<String>,
    pub due_before: Option<String>,
    pub completed_after: Option<String>,
    pub completed_before: Option<String>,
    // Diagram fields
    pub diagram_id: Option<String>,
    pub diagram_type: Option<String>,
    // Doc fields
    pub doc_id: Option<String>,
    pub doc_type: Option<String>,
    #[serde(default, deserialize_with = "deserialize_string_or_vec")]
    pub milestones: Option<Vec<Value>>,
    // Transcript fields
    pub transcript_id: Option<String>,
    pub session_id: Option<String>,
    pub client_name: Option<String>,
    pub started_after: Option<String>,
    pub started_before: Option<String>,
    // Personal flag
    pub is_personal: Option<bool>,
}

/// Unified memory tool handler.
pub struct MemoryTool {
    client: ContextStreamClient,
    session: Arc<mcp_session::SessionManager>,
    /// Legacy no-op compatibility layer for `action="search_archive"`.
    atlas_layer: mcp_types::atlas_layer::AtlasLayer,
    acceleration_layer: mcp_types::acceleration_layer::AccelerationLayer,
}

fn status_only_task_update(params: &UpdateTaskParams) -> Option<&str> {
    params.status.as_deref().filter(|_| {
        params.title.is_none()
            && params.description.is_none()
            && params.content.is_none()
            && params.priority.is_none()
            && params.plan_id.is_none()
            && params.order.is_none()
            && params.code_refs.is_none()
            && params.tags.is_none()
            && params.blocked_reason.is_none()
    })
}

fn task_status_from_result(result: &Value) -> Option<&str> {
    result
        .get("status")
        .or_else(|| result.get("task").and_then(|task| task.get("status")))
        .or_else(|| result.get("data").and_then(|data| data.get("status")))
        .or_else(|| {
            result
                .get("data")
                .and_then(|data| data.get("task"))
                .and_then(|task| task.get("status"))
        })
        .and_then(Value::as_str)
}

impl MemoryTool {
    pub fn new(
        client: ContextStreamClient,
        session: Arc<mcp_session::SessionManager>,
        atlas_layer: mcp_types::atlas_layer::AtlasLayer,
    ) -> Self {
        Self::with_acceleration(
            client,
            session,
            atlas_layer,
            mcp_types::acceleration_layer::noop_acceleration_layer(),
        )
    }

    pub fn with_acceleration(
        client: ContextStreamClient,
        session: Arc<mcp_session::SessionManager>,
        atlas_layer: mcp_types::atlas_layer::AtlasLayer,
        acceleration_layer: mcp_types::acceleration_layer::AccelerationLayer,
    ) -> Self {
        Self {
            client,
            session,
            atlas_layer,
            acceleration_layer,
        }
    }

    async fn resolve_scope_for_input(&self, input: &MemoryInput) -> Result<ResolvedReadScope> {
        resolve_read_scope(
            &self.client,
            self.session.as_ref(),
            input.workspace_id.as_deref(),
            input.project_id.as_deref(),
        )
        .await
    }

    /// Fetch an entity type across all team workspaces (up to 10), merge and sort results.
    async fn fetch_team_entity<F>(&self, fetcher: F) -> Result<serde_json::Value>
    where
        F: Fn(
            ContextStreamClient,
            Uuid,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = mcp_types::Result<serde_json::Value>> + Send>,
        >,
    {
        if !self.session.team_features_enabled().await {
            return Ok(serde_json::json!({
                "items": [],
                "message": "Team features require an active team execution mode and team membership."
            }));
        }

        let workspaces = self
            .client
            .list_workspaces(None, Some(100))
            .await
            .unwrap_or_default();

        let mut all_items: Vec<serde_json::Value> = Vec::new();
        for ws in workspaces.iter().take(10) {
            if let Ok(result) = fetcher(self.client.clone(), ws.id).await {
                if let Some(arr) = extract_collection_array(&result) {
                    for item in arr {
                        let mut item = item.clone();
                        if let Some(obj) = item.as_object_mut() {
                            obj.insert(
                                "workspace_name".to_string(),
                                serde_json::Value::String(ws.name.clone()),
                            );
                        }
                        all_items.push(item);
                    }
                }
            }
        }

        // Sort by updated_at/created_at descending
        all_items.sort_by(|a, b| {
            let date_a = a
                .get("updated_at")
                .or_else(|| a.get("created_at"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let date_b = b
                .get("updated_at")
                .or_else(|| b.get("created_at"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            date_b.cmp(date_a)
        });

        let total = all_items.len();
        Ok(serde_json::json!({
            "items": all_items,
            "total": total,
            "workspaces_searched": workspaces.len().min(10),
        }))
    }
}

#[async_trait]
impl ToolHandler for MemoryTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let mut input: MemoryInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;

        if input
            .project_id
            .as_deref()
            .map(str::trim)
            .map(|value| value.is_empty())
            .unwrap_or(true)
        {
            input.project_id = resolve_target_project_input(
                self.session.as_ref(),
                input.target_project.as_deref(),
            )
            .await?;
        }

        let state = self.session.state().await;
        let raw_workspace_id = input.workspace_id.clone();
        let raw_project_id = input.project_id.clone();
        let normalized_action = input.action.to_ascii_lowercase();
        let explicit_workspace_id = input
            .workspace_id
            .as_ref()
            .and_then(|s| Uuid::parse_str(s).ok());
        let explicit_project_id = input
            .project_id
            .as_ref()
            .and_then(|s| Uuid::parse_str(s).ok());
        let workspace_id = explicit_workspace_id.or(state.workspace_id);
        let project_id = explicit_project_id.or_else(|| {
            if input
                .workspace_id
                .as_deref()
                .is_some_and(|value| !value.trim().is_empty())
                && explicit_workspace_id != state.workspace_id
            {
                None
            } else {
                state.project_id
            }
        });

        input.is_personal = crate::domains::account_mode::resolve_is_personal(
            state.active_execution_mode,
            input.is_personal,
            state.team_context_degraded,
        );

        match normalized_action.as_str() {
            // === Node Actions ===
            "search" => {
                let query = input
                    .query
                    .ok_or_else(|| Error::Validation("query is required for search".to_string()))?;
                let search_input = build_memory_search_input(
                    query,
                    input.workspace_id,
                    input.project_id,
                    workspace_id,
                    project_id,
                    input.node_type,
                    input.limit,
                );
                let tool = MemorySearchTool::new(self.client.clone());
                tool.execute(serde_json::to_value(&search_input).unwrap())
                    .await
            }
            "create_node" => {
                let node_type = input.node_type.ok_or_else(|| {
                    Error::Validation("node_type is required for create_node".to_string())
                })?;
                let title = input.title.ok_or_else(|| {
                    Error::Validation("title is required for create_node".to_string())
                })?;
                let node_type = normalize_node_type(&node_type)?;
                if title.trim().is_empty() {
                    return Err(Error::Validation("title is required".to_string()));
                }
                let params = CreateMemoryNodeParams {
                    node_type,
                    title,
                    content: input.content,
                    workspace_id: None,
                    project_id: None,
                    metadata: input.metadata,
                };
                let (mut result, scope) = execute_write_with_scope_recovery(
                    &self.client,
                    &self.session,
                    raw_workspace_id.as_deref(),
                    raw_project_id.as_deref(),
                    params,
                    |params, workspace_id, project_id| {
                        params.workspace_id = workspace_id;
                        params.project_id = project_id;
                    },
                    |client, params| async move { client.create_memory_node(params).await },
                )
                .await?;
                attach_scope_recovery_metadata(&mut result, &scope);
                let node_id = result
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                if let Some(obj) = result.as_object_mut() {
                    obj.insert(
                        "operation_status".to_string(),
                        serde_json::json!({
                            "operation": "create_node",
                            "state": "completed"
                        }),
                    );
                    obj.insert(
                        "user_visibility_hint".to_string(),
                        serde_json::json!({
                            "announce_now": "Node saved successfully.",
                            "note": "Node creation is synchronous and complete when this response is returned."
                        }),
                    );
                }
                let text = format!("Node created (ID: {}).\nProgress: completed.", node_id);
                let mut output = ToolResult::with_structured(text, result);
                if let Some(note) = scope.note {
                    output = output.with_prefix(format!("{}\n", note));
                }
                Ok(output)
            }
            "get_node" => {
                let node_id = input
                    .node_id
                    .ok_or_else(|| Error::Validation("node_id is required".to_string()))?;
                let node_lookup = node_id.trim().to_string();
                let node_listing = self
                    .client
                    .list_memory_nodes(workspace_id, project_id, input.node_type.clone(), Some(100))
                    .await?;
                let (id, resolution_note) = resolve_from_collection_lookup(
                    "nodes",
                    &node_lookup,
                    &node_listing,
                    &["title", "summary", "name"],
                )?;
                let result = self.client.get_memory_node(id).await?;
                let node_type = result
                    .get("node_type")
                    .or_else(|| result.get("type"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let title = result
                    .get("title")
                    .or_else(|| result.get("summary"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("Untitled");
                let node_id_str = result
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let mut text = String::new();
                if let Some(note) = resolution_note {
                    text.push_str(&format!("{}\n\n", note));
                }
                text.push_str(&format!("[{}] {}\nID: {}", node_type, title, node_id_str));
                Ok(ToolResult::with_structured(text, result))
            }
            "update_node" => {
                let node_id = input
                    .node_id
                    .ok_or_else(|| Error::Validation("node_id is required".to_string()))?;
                let node_lookup = node_id.trim().to_string();
                let node_listing = self
                    .client
                    .list_memory_nodes(workspace_id, project_id, input.node_type.clone(), Some(100))
                    .await?;
                let (id, resolution_note) = resolve_from_collection_lookup(
                    "nodes",
                    &node_lookup,
                    &node_listing,
                    &["title", "summary", "name"],
                )?;
                // Accept `new_content` as the node body too: agents reach for
                // it by analogy with update_doc/supersede_node, and the API
                // used to silently drop everything (no-op "success") when the
                // expected fields were absent.
                let content = input.content.or(input.new_content);
                if input.title.is_none() && content.is_none() && input.metadata.is_none() {
                    return Err(Error::Validation(
                        "update_node requires at least one of: title, content (or new_content), metadata"
                            .to_string(),
                    ));
                }
                let params = UpdateMemoryNodeParams {
                    title: input.title,
                    content,
                    metadata: input.metadata,
                };
                let mut result = self.client.update_memory_node(id, params).await?;
                if let Some(obj) = result.as_object_mut() {
                    obj.insert(
                        "operation_status".to_string(),
                        serde_json::json!({
                            "operation": "update_node",
                            "state": "completed"
                        }),
                    );
                    obj.insert(
                        "user_visibility_hint".to_string(),
                        serde_json::json!({
                            "announce_now": "Node update completed.",
                            "note": "Node update is synchronous and complete when this response is returned."
                        }),
                    );
                }
                let mut text = String::new();
                if let Some(note) = resolution_note {
                    text.push_str(&format!("{}\n", note));
                }
                text.push_str(&format!("Node updated: {}.\nProgress: completed.", id));
                Ok(ToolResult::with_structured(text, result))
            }
            "delete_node" => {
                let node_id = input
                    .node_id
                    .ok_or_else(|| Error::Validation("node_id is required".to_string()))?;
                let node_lookup = node_id.trim().to_string();
                let node_listing = self
                    .client
                    .list_memory_nodes(workspace_id, project_id, input.node_type.clone(), Some(100))
                    .await?;
                if input.delete_all.unwrap_or(false) {
                    let matches = collect_bulk_delete_matches(
                        &node_listing,
                        &node_lookup,
                        &["title", "summary", "name"],
                    );
                    if matches.is_empty() {
                        return Err(Error::Validation(format!(
                            "No exact node matches for \"{}\" to bulk-delete. Use list_nodes to inspect IDs.",
                            node_lookup
                        )));
                    }
                    let mut deleted = 0usize;
                    let mut errors = 0usize;
                    let mut lines = String::new();
                    for m in &matches {
                        match self.client.delete_memory_node(m.id).await {
                            Ok(_) => {
                                deleted += 1;
                                lines.push_str(&format!("- deleted \"{}\" ({})\n", m.title, m.id));
                            }
                            Err(e) => {
                                errors += 1;
                                lines.push_str(&format!(
                                    "- FAILED \"{}\" ({}): {}\n",
                                    m.title, m.id, e
                                ));
                            }
                        }
                    }
                    let text = format!(
                        "Bulk delete for \"{}\": {} node(s) deleted, {} error(s).\n{}",
                        node_lookup, deleted, errors, lines
                    );
                    return Ok(ToolResult::with_structured(
                        text,
                        serde_json::json!({
                            "deleted": deleted,
                            "errors": errors,
                            "matched": matches.len(),
                        }),
                    ));
                }
                let (id, resolution_note) = resolve_from_collection_lookup(
                    "nodes",
                    &node_lookup,
                    &node_listing,
                    &["title", "summary", "name"],
                )?;
                let result = self.client.delete_memory_node(id).await?;
                let mut text = String::new();
                if let Some(note) = resolution_note {
                    text.push_str(&format!("{}\n", note));
                }
                text.push_str("Node deleted successfully.");
                Ok(ToolResult::with_structured(text, result))
            }
            "list_nodes" => {
                // P0 #5 — PreferencesHot warm cache, scoped to
                // node_type=preference or =constraint (the every-turn
                // [PREFERENCE] injection sources). Other node_types
                // (fact, lesson, habit, decision) bypass the cache —
                // they're either lower-frequency or covered by
                // dedicated caches (e.g. lessons via P0 #1).
                let node_type_normalised = input
                    .node_type
                    .as_ref()
                    .map(|s| s.trim().to_ascii_lowercase());
                let preferences_cache_eligible = matches!(
                    node_type_normalised.as_deref(),
                    Some("preference") | Some("constraint")
                );
                let user_scope_token = super::atlas_warm_cache::current_user_scope_token();
                let cached_nodes = if preferences_cache_eligible {
                    if let (Some(ws), Some(nt)) = (workspace_id, node_type_normalised.as_deref()) {
                        let cache_scope = mcp_types::atlas_layer::AtlasFederationScope {
                            workspace_id: ws,
                            project_id,
                            scope_hash:
                                super::atlas_warm_cache::scope_hash_for_preferences_hot(
                                    ws,
                                    user_scope_token.as_deref(),
                                    project_id,
                                    nt,
                                ),
                            user_scope: user_scope_token.clone(),
                        };
                        super::atlas_warm_cache::try_lookup(
                            &self.atlas_layer,
                            mcp_types::atlas_layer::AtlasWarmCacheKind::PreferencesHot,
                            cache_scope,
                            120, // primary baseline ms
                        )
                        .await
                    } else {
                        None
                    }
                } else {
                    None
                };
                let result = if let Some(bundle) = cached_nodes {
                    bundle.payload
                } else {
                    let r = self
                        .client
                        .list_memory_nodes(
                            workspace_id,
                            project_id,
                            input.node_type.clone(),
                            input.limit,
                        )
                        .await?;
                    if preferences_cache_eligible {
                        if let (Some(ws), Some(nt)) =
                            (workspace_id, node_type_normalised.as_deref())
                        {
                            let cache_scope = mcp_types::atlas_layer::AtlasFederationScope {
                                workspace_id: ws,
                                project_id,
                                scope_hash:
                                    super::atlas_warm_cache::scope_hash_for_preferences_hot(
                                        ws,
                                        user_scope_token.as_deref(),
                                        project_id,
                                        nt,
                                    ),
                                user_scope: user_scope_token.clone(),
                            };
                            super::atlas_warm_cache::put_in_background(
                                self.atlas_layer.clone(),
                                mcp_types::atlas_layer::AtlasWarmCacheKind::PreferencesHot,
                                cache_scope,
                                r.clone(),
                            );
                        }
                    }
                    r
                };
                let text = format_collection("memory nodes", &result);
                Ok(ToolResult::with_structured(text, result))
            }
            "supersede_node" => {
                let node_id = input
                    .node_id
                    .ok_or_else(|| Error::Validation("node_id is required".to_string()))?;
                let new_content = input
                    .new_content
                    .ok_or_else(|| Error::Validation("new_content is required".to_string()))?;
                let node_lookup = node_id.trim().to_string();
                let (id, resolution_note) = match Uuid::parse_str(&node_lookup) {
                    Ok(id) => (id, None),
                    Err(_) => {
                        let listing = self
                            .client
                            .list_memory_nodes(
                                workspace_id,
                                project_id,
                                input.node_type.clone(),
                                Some(100),
                            )
                            .await?;
                        let items = extract_collection_array(&listing)
                            .cloned()
                            .unwrap_or_default();
                        let ranked = rank_lookup_matches(
                            &items,
                            &node_lookup,
                            &["title", "summary", "name"],
                            8,
                        );
                        match classify_lookup_resolution(&ranked) {
                            LookupResolution::None => {
                                return Err(Error::Validation(format!(
                                    "No nodes match \"{node_lookup}\". Use memory(action=\"list_nodes\") or memory(action=\"search\", query=\"{node_lookup}\") to find the node id."
                                )));
                            }
                            LookupResolution::Single(best) => {
                                let note = (!best.exact).then(|| {
                                    format!(
                                        "Resolved node \"{}\" to **{}** (id: {}).",
                                        node_lookup, best.title, best.id
                                    )
                                });
                                (best.id, note)
                            }
                            LookupResolution::Ambiguous => {
                                return Ok(supersede_candidates_result(&node_lookup, &ranked));
                            }
                        }
                    }
                };
                let params = SupersedeMemoryNodeParams {
                    new_content,
                    reason: input.reason,
                };
                let result = self.client.supersede_memory_node(id, params).await?;
                let mut text = String::new();
                if let Some(note) = resolution_note {
                    text.push_str(&note);
                    text.push('\n');
                }
                let new_id = result
                    .get("new_node_id")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                text.push_str(&format!("Node superseded: {id} → {new_id}."));
                Ok(ToolResult::with_structured(text, result))
            }
            "create_decision" => {
                let title = input
                    .title
                    .ok_or_else(|| Error::Validation("title is required".to_string()))?;
                let decision_input = CreateDecisionInput {
                    workspace_id: input.workspace_id,
                    project_id: input.project_id,
                    title,
                    content: input.content,
                    rationale: input.rationale,
                    alternatives: input.alternatives,
                    scope: input.scope,
                    confidence: input.confidence,
                    supersedes: input.supersedes,
                    category: input.category,
                    tags: input.tags,
                    session_id: input.session_id,
                };
                execute_create_decision(&self.client, self.session.as_ref(), decision_input).await
            }
            "decision_action" => {
                let decision_id = input
                    .decision_id
                    .or(input.node_id)
                    .ok_or_else(|| Error::Validation("decision_id is required".to_string()))?;
                let decision_action = input.decision_action.ok_or_else(|| {
                    Error::Validation(format!(
                        "decision_action is required (one of: {})",
                        mcp_client::DECISION_ACTIONS.join(", ")
                    ))
                })?;
                let action_input = DecisionActionInput {
                    workspace_id: input.workspace_id,
                    project_id: input.project_id,
                    decision_id,
                    decision_action,
                    successor_id: input.successor_id,
                    reason: input.reason,
                    title: input.title,
                };
                execute_decision_action(&self.client, self.session.as_ref(), action_input).await
            }
            "decisions" => {
                let decisions_input = MemoryDecisionsInput {
                    workspace_id: input.workspace_id,
                    project_id: input.project_id,
                    query: input.query,
                    category: input.category,
                    limit: input.limit,
                    sort: input.sort,
                    status: input.status,
                    since: input.since,
                    offset: input.offset,
                    source: input.source,
                };
                let tool =
                    MemoryDecisionsTool::with_session_and_atlas(
                        self.client.clone(),
                        self.session.clone(),
                        self.atlas_layer.clone(),
                    );
                let res = tool
                    .execute(serde_json::to_value(&decisions_input).unwrap())
                    .await;
                match res {
                    Ok(r) => {
                        consume_grounding_memory_tool(&self.session).await;
                        Ok(r)
                    }
                    Err(err) if super::workspace_drift::is_workspace_access_error(&err) => {
                        Ok(super::workspace_drift::drift_collection_result(
                            "decisions",
                            workspace_id,
                            None,
                        ))
                    }
                    Err(err) => Err(err),
                }
            }

            // === Event Actions ===
            "create_event" => {
                let event_type = input
                    .event_type
                    .ok_or_else(|| Error::Validation("event_type is required".to_string()))?;
                if is_reserved_plan_event_type(&event_type) {
                    return Err(reserved_plan_event_error());
                }
                let title = input
                    .title
                    .ok_or_else(|| Error::Validation("title is required".to_string()))?;
                let title_for_user = title.clone();
                let params = mcp_client::CreateMemoryEventParams {
                    event_type,
                    title,
                    content: input.content,
                    workspace_id: None,
                    project_id: None,
                    metadata: input.metadata,
                    is_personal: input.is_personal,
                };
                let (created, scope) = execute_write_with_scope_recovery(
                    &self.client,
                    &self.session,
                    raw_workspace_id.as_deref(),
                    raw_project_id.as_deref(),
                    params,
                    |params, workspace_id, project_id| {
                        params.workspace_id = workspace_id;
                        params.project_id = project_id;
                    },
                    |client, params| async move { client.create_memory_event(params).await },
                )
                .await?;
                let event_id = created.id.to_string();
                let mut result = serde_json::to_value(&created)
                    .unwrap_or_else(|_| serde_json::json!({ "id": event_id.clone() }));
                attach_scope_recovery_metadata(&mut result, &scope);
                if let Some(obj) = result.as_object_mut() {
                    obj.insert(
                        "operation_status".to_string(),
                        serde_json::json!({
                            "operation": "create_event",
                            "state": "completed"
                        }),
                    );
                    obj.insert(
                        "user_visibility_hint".to_string(),
                        serde_json::json!({
                            "announce_now": "Event saved successfully.",
                            "note": "Event creation is synchronous and complete when this response is returned."
                        }),
                    );
                }
                let text = format!(
                    "Event created: {} (ID: {}).\nProgress: completed.",
                    title_for_user, event_id
                );
                let mut output = ToolResult::with_structured(text, result);
                if let Some(note) = scope.note {
                    output = output.with_prefix(format!("{}\n", note));
                }
                Ok(output)
            }
            "get_event" => {
                let event_id = input
                    .event_id
                    .as_ref()
                    .ok_or_else(|| Error::Validation("event_id is required".to_string()))?;
                let read_scope = self.resolve_scope_for_input(&input).await?;
                let workspace_id = read_scope.workspace_id;
                let project_id = read_scope.project_id;
                let event_lookup = event_id.trim().to_string();
                let events_listing = self
                    .client
                    .list_memory_events(
                        workspace_id,
                        project_id,
                        input.event_type.clone(),
                        Some(100),
                    )
                    .await?;
                let (id, resolution_note) = resolve_from_collection_lookup(
                    "events",
                    &event_lookup,
                    &events_listing,
                    &["title", "summary", "event_type", "content"],
                )?;
                match self.client.get_memory_event(id).await {
                    Ok(result) => {
                        consume_grounding_memory_tool(&self.session).await;
                        let mut text = String::new();
                        if let Some(note) = resolution_note {
                            text.push_str(&format!("{}\n\n", note));
                        }
                        text.push_str(&format_event_detail(&result));
                        Ok(ToolResult::with_structured(text, result))
                    }
                    Err(err) if is_not_found_error(&err) => {
                        consume_grounding_memory_tool(&self.session).await;
                        Ok(ToolResult::with_structured(
                            format!(
                                "Memory event {} was not found. The ID may be stale or may refer to another recall result type; use session(action=\"recall\", query=\"...\") or memory(action=\"list_events\") to locate the current item.",
                                event_id
                            ),
                            serde_json::json!({
                                "found": false,
                                "event_id": event_id,
                                "reason": "not_found",
                                "suggested_actions": [
                                    "session(action=\"recall\", query=\"<keywords>\")",
                                    "memory(action=\"list_events\")"
                                ]
                            }),
                        ))
                    }
                    Err(err) => Err(err),
                }
            }
            "update_event" => {
                let event_id = input
                    .event_id
                    .as_ref()
                    .ok_or_else(|| Error::Validation("event_id is required".to_string()))?;
                let read_scope = self.resolve_scope_for_input(&input).await?;
                let workspace_id = read_scope.workspace_id;
                let project_id = read_scope.project_id;
                let event_lookup = event_id.trim().to_string();
                let events_listing = self
                    .client
                    .list_memory_events(
                        workspace_id,
                        project_id,
                        input.event_type.clone(),
                        Some(100),
                    )
                    .await?;
                let (id, resolution_note) = resolve_from_collection_lookup(
                    "events",
                    &event_lookup,
                    &events_listing,
                    &["title", "summary", "event_type", "content"],
                )?;
                let params = UpdateMemoryEventParams {
                    title: input.title,
                    content: input.content,
                    metadata: input.metadata,
                };
                let mut result = self.client.update_memory_event(id, params).await?;
                if let Some(obj) = result.as_object_mut() {
                    obj.insert(
                        "operation_status".to_string(),
                        serde_json::json!({
                            "operation": "update_event",
                            "state": "completed"
                        }),
                    );
                    obj.insert(
                        "user_visibility_hint".to_string(),
                        serde_json::json!({
                            "announce_now": "Event update completed.",
                            "note": "Event update is synchronous and complete when this response is returned."
                        }),
                    );
                }
                let mut text = String::new();
                if let Some(note) = resolution_note {
                    text.push_str(&format!("{}\n", note));
                }
                text.push_str(&format!("Event updated: {}.\nProgress: completed.", id));
                Ok(ToolResult::with_structured(text, result))
            }
            "delete_event" => {
                let event_id = input
                    .event_id
                    .as_ref()
                    .ok_or_else(|| Error::Validation("event_id is required".to_string()))?;
                let read_scope = self.resolve_scope_for_input(&input).await?;
                let workspace_id = read_scope.workspace_id;
                let project_id = read_scope.project_id;
                let event_lookup = event_id.trim().to_string();
                let events_listing = self
                    .client
                    .list_memory_events(
                        workspace_id,
                        project_id,
                        input.event_type.clone(),
                        Some(100),
                    )
                    .await?;
                if input.delete_all.unwrap_or(false) {
                    let matches = collect_bulk_delete_matches(
                        &events_listing,
                        &event_lookup,
                        &["title", "summary", "event_type", "content"],
                    );
                    if matches.is_empty() {
                        return Err(Error::Validation(format!(
                            "No exact event matches for \"{}\" to bulk-delete. Use list_events to inspect IDs.",
                            event_lookup
                        )));
                    }
                    let mut deleted = 0usize;
                    let mut errors = 0usize;
                    let mut lines = String::new();
                    for m in &matches {
                        match self.client.delete_memory_event(m.id).await {
                            Ok(_) => {
                                deleted += 1;
                                lines.push_str(&format!("- deleted \"{}\" ({})\n", m.title, m.id));
                            }
                            Err(e) => {
                                errors += 1;
                                lines.push_str(&format!(
                                    "- FAILED \"{}\" ({}): {}\n",
                                    m.title, m.id, e
                                ));
                            }
                        }
                    }
                    let text = format!(
                        "Bulk delete for \"{}\": {} event(s) deleted, {} error(s).\n{}",
                        event_lookup, deleted, errors, lines
                    );
                    return Ok(ToolResult::with_structured(
                        text,
                        serde_json::json!({
                            "deleted": deleted,
                            "errors": errors,
                            "matched": matches.len(),
                        }),
                    ));
                }
                let (id, resolution_note) = resolve_from_collection_lookup(
                    "events",
                    &event_lookup,
                    &events_listing,
                    &["title", "summary", "event_type", "content"],
                )?;
                let result = self.client.delete_memory_event(id).await?;
                let mut text = String::new();
                if let Some(note) = resolution_note {
                    text.push_str(&format!("{}\n", note));
                }
                text.push_str("Event deleted successfully.");
                Ok(ToolResult::with_structured(text, result))
            }
            "distill_event" => {
                let event_id = input
                    .event_id
                    .ok_or_else(|| Error::Validation("event_id is required".to_string()))?;
                let id = Uuid::parse_str(&event_id)
                    .map_err(|_| Error::Validation("Invalid event_id UUID".to_string()))?;
                let result = self.client.distill_memory_event(id).await?;
                Ok(ToolResult::with_structured(
                    "Event distilled successfully.".to_string(),
                    result,
                ))
            }
            "list_events" => {
                let read_scope = self.resolve_scope_for_input(&input).await?;
                let workspace_id = read_scope.workspace_id;
                let project_id = read_scope.project_id;
                // A8b: try the regional warm cache first (~30ms p95
                // when populated by another pod in this region).
                // Lookup respects a 50ms hard cap; on miss/error we
                // fall through to the primary call unchanged.
                //
                // Cache scope: workspace + project (no event_type or
                // limit slicing — the warm cache always holds the
                // newest 200 events per workspace, and the handler
                // applies caller filters in memory below).
                let warm_workspace = workspace_id;
                let mut served_from_cache = false;
                let mut cache_age_ms: Option<u64> = None;
                let cached_result = if let Some(ws) = warm_workspace {
                    let scope = mcp_types::atlas_layer::AtlasFederationScope {
                        workspace_id: ws,
                        project_id,
                        scope_hash: super::atlas_warm_cache::scope_hash_for_memory_events_hot(
                            ws,
                            project_id,
                        ),
                        // memory_events_hot is the workspace-shared
                        // 24h hot bundle written by the
                        // pipeline-memory-events-hot stream
                        // pipeline; key matches the pipeline so
                        // user_scope is intentionally absent here.
                        user_scope: None,
                    };
                    super::atlas_warm_cache::try_lookup_accelerated(
                        &self.acceleration_layer,
                        &self.atlas_layer,
                        mcp_types::atlas_layer::AtlasWarmCacheKind::MemoryEventsHot,
                        scope,
                        130, // primary baseline ms — list_events 134ms p95
                    )
                    .await
                } else {
                    None
                };

                let raw_result = if let Some(bundle) = cached_result {
                    served_from_cache = true;
                    cache_age_ms = bundle.age_ms;
                    bundle.payload
                } else {
                    let primary = match self
                        .client
                        .list_memory_events(
                            workspace_id,
                            project_id,
                            input.event_type.clone(),
                            input.limit,
                        )
                        .await
                    {
                        Ok(p) => p,
                        Err(err)
                            if super::workspace_drift::is_workspace_access_error(&err) =>
                        {
                            return Ok(super::workspace_drift::drift_collection_result(
                                "events",
                                workspace_id,
                                None,
                            ));
                        }
                        Err(err) => return Err(err),
                    };
                    // Best-effort write-back so the next pod in this
                    // region serves it from cache.
                    if let Some(ws) = warm_workspace {
                        let scope = mcp_types::atlas_layer::AtlasFederationScope {
                            workspace_id: ws,
                            project_id,
                            scope_hash:
                                super::atlas_warm_cache::scope_hash_for_memory_events_hot(
                                    ws, project_id,
                                ),
                            user_scope: None,
                        };
                        super::atlas_warm_cache::put_accelerated_in_background(
                            self.acceleration_layer.clone(),
                            self.atlas_layer.clone(),
                            mcp_types::atlas_layer::AtlasWarmCacheKind::MemoryEventsHot,
                            scope,
                            primary.clone(),
                        );
                    }
                    primary
                };

                // Stamp provenance markers on the structured envelope
                // (cache hit / served_from / age) without changing
                // the response shape clients already consume.
                let mut result = raw_result;
                if let Some(obj) = result.as_object_mut() {
                    obj.insert(
                        "served_from".to_string(),
                        serde_json::Value::String(if served_from_cache {
                            "regional_warm_cache".to_string()
                        } else {
                            "primary_server".to_string()
                        }),
                    );
                    obj.insert(
                        "cache_hit".to_string(),
                        serde_json::Value::Bool(served_from_cache),
                    );
                    if let Some(age) = cache_age_ms {
                        obj.insert(
                            "cache_age_ms".to_string(),
                            serde_json::Value::Number(age.into()),
                        );
                    }
                }
                let text = if served_from_cache {
                    format!(
                        "[WARM_CACHE] {} (age {}ms)\n{}",
                        format_collection("events", &result),
                        cache_age_ms.unwrap_or(0),
                        ""
                    )
                } else {
                    format_collection("events", &result)
                };
                Ok(ToolResult::with_structured(text, result))
            }
            "import_batch" => {
                let events = input
                    .events
                    .ok_or_else(|| Error::Validation("events array is required".to_string()))?;
                let event_count = events.len();
                let params = ImportMemoryEventsParams {
                    events,
                    workspace_id: None,
                    project_id: None,
                };
                let (mut result, scope) = execute_write_with_scope_recovery(
                    &self.client,
                    &self.session,
                    raw_workspace_id.as_deref(),
                    raw_project_id.as_deref(),
                    params,
                    |params, workspace_id, project_id| {
                        params.workspace_id = workspace_id;
                        params.project_id = project_id;
                    },
                    |client, params| async move { client.import_memory_events(params).await },
                )
                .await?;
                attach_scope_recovery_metadata(&mut result, &scope);
                if let Some(obj) = result.as_object_mut() {
                    obj.insert(
                        "operation_status".to_string(),
                        serde_json::json!({
                            "operation": "import_batch",
                            "state": "completed"
                        }),
                    );
                    obj.insert(
                        "user_visibility_hint".to_string(),
                        serde_json::json!({
                            "announce_now": "Event import completed.",
                            "note": "Import is synchronous and complete when this response is returned."
                        }),
                    );
                }
                let text = format!("Imported {} events.\nProgress: completed.", event_count);
                let mut output = ToolResult::with_structured(text, result);
                if let Some(note) = scope.note {
                    output = output.with_prefix(format!("{}\n", note));
                }
                Ok(output)
            }

            // === Task Actions ===
            "create_task" => {
                let title = input
                    .title
                    .ok_or_else(|| Error::Validation("title is required".to_string()))?;
                require_explicit_task_workspace(raw_workspace_id.as_deref())?;
                let plan_id = input.plan_id.as_ref().and_then(|s| Uuid::parse_str(s).ok());
                let title_for_user = title.clone();
                let params = CreateTaskParams {
                    title,
                    description: input.description,
                    content: input.content,
                    priority: input.priority,
                    status: input.task_status,
                    plan_id,
                    plan_step_id: input.plan_step_id,
                    tags: input.tags,
                    order: input.order,
                    is_personal: input.is_personal,
                    workspace_id: None,
                    project_id: None,
                };
                let (mut result, scope) = execute_write_with_scope_recovery(
                    &self.client,
                    &self.session,
                    raw_workspace_id.as_deref(),
                    raw_project_id.as_deref(),
                    params,
                    |params, workspace_id, project_id| {
                        params.workspace_id = workspace_id;
                        params.project_id = project_id;
                    },
                    |client, params| async move { client.create_task(params).await },
                )
                .await?;
                attach_scope_recovery_metadata(&mut result, &scope);
                let task_id = result
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                if let Some(obj) = result.as_object_mut() {
                    obj.insert(
                        "operation_status".to_string(),
                        serde_json::json!({
                            "operation": "create_task",
                            "state": "completed"
                        }),
                    );
                    obj.insert(
                        "user_visibility_hint".to_string(),
                        serde_json::json!({
                            "announce_now": "Task saved successfully.",
                            "note": "Task creation is synchronous and complete when this response is returned."
                        }),
                    );
                }
                let text = format!(
                    "Task created: {} (ID: {}).\nProgress: completed.",
                    title_for_user, task_id
                );
                let mut output = ToolResult::with_structured(text, result);
                if let Some(note) = scope.note {
                    output = output.with_prefix(format!("{}\n", note));
                }
                Ok(output)
            }
            "get_task" => {
                let task_id = input
                    .task_id
                    .ok_or_else(|| Error::Validation("task_id is required".to_string()))?;
                require_explicit_task_workspace(raw_workspace_id.as_deref())?;
                let task_lookup = task_id.trim().to_string();
                let plan_id = input.plan_id.as_ref().and_then(|s| Uuid::parse_str(s).ok());
                let task_listing = self
                    .client
                    .list_tasks(
                        workspace_id,
                        project_id,
                        plan_id,
                        None,
                        Some(100),
                    )
                    .await?;
                let (id, resolution_note) = resolve_from_collection_lookup(
                    "tasks",
                    &task_lookup,
                    &task_listing,
                    &["title", "description", "content"],
                )?;
                let result = self.client.get_task(id).await?;
                consume_grounding_memory_tool(&self.session).await;
                let mut text = String::new();
                if let Some(note) = resolution_note {
                    text.push_str(&format!("{}\n\n", note));
                }
                text.push_str(&format_task_detail(&result));
                Ok(ToolResult::with_structured(text, result))
            }
            "update_task" => {
                let task_id = input
                    .task_id
                    .ok_or_else(|| Error::Validation("task_id is required".to_string()))?;
                require_explicit_task_workspace(raw_workspace_id.as_deref())?;
                let task_lookup = task_id.trim().to_string();
                let plan_id = input.plan_id.as_ref().and_then(|s| Uuid::parse_str(s).ok());
                let task_listing = self
                    .client
                    .list_tasks(
                        workspace_id,
                        project_id,
                        plan_id,
                        None,
                        Some(100),
                    )
                    .await?;
                let (id, resolution_note) = resolve_from_collection_lookup(
                    "tasks",
                    &task_lookup,
                    &task_listing,
                    &["title", "description", "content"],
                )?;
                let params = UpdateTaskParams {
                    title: input.title,
                    description: input.description,
                    content: input.content,
                    status: input.task_status,
                    priority: input.priority,
                    plan_id,
                    order: input.order,
                    code_refs: input.code_refs,
                    tags: input.tags,
                    blocked_reason: input.blocked_reason,
                };

                if let Some(target_status) = status_only_task_update(&params) {
                    let mut current = self.client.get_task(id).await?;
                    if task_status_from_result(&current)
                        .is_some_and(|current_status| current_status.eq_ignore_ascii_case(target_status))
                    {
                        if let Some(obj) = current.as_object_mut() {
                            obj.insert(
                                "operation_status".to_string(),
                                serde_json::json!({
                                    "operation": "update_task",
                                    "state": "completed",
                                    "changed": false,
                                    "reason": "already_in_target_status",
                                    "status": target_status,
                                }),
                            );
                            obj.insert(
                                "user_visibility_hint".to_string(),
                                serde_json::json!({
                                    "announce_now": format!("Task is already {target_status}. No changes were needed."),
                                    "note": "A repeated status-only update is a successful no-op."
                                }),
                            );
                        }
                        let mut text = String::new();
                        if let Some(note) = resolution_note {
                            text.push_str(&format!("{}\n", note));
                        }
                        text.push_str(&format!(
                            "Task already has status '{}'; no changes were needed.\nProgress: completed.",
                            target_status
                        ));
                        return Ok(ToolResult::with_structured(text, current));
                    }
                }
                let mut result = self.client.update_task(id, params).await?;
                if let Some(obj) = result.as_object_mut() {
                    obj.insert(
                        "operation_status".to_string(),
                        serde_json::json!({
                            "operation": "update_task",
                            "state": "completed"
                        }),
                    );
                    obj.insert(
                        "user_visibility_hint".to_string(),
                        serde_json::json!({
                            "announce_now": "Task update completed.",
                            "note": "Task update is synchronous and complete when this response is returned."
                        }),
                    );
                }
                let mut text = String::new();
                if let Some(note) = resolution_note {
                    text.push_str(&format!("{}\n", note));
                }
                text.push_str(&format!("Task updated: {}.\nProgress: completed.", id));
                Ok(ToolResult::with_structured(text, result))
            }
            "delete_task" => {
                let task_id = input
                    .task_id
                    .ok_or_else(|| Error::Validation("task_id is required".to_string()))?;
                require_explicit_task_workspace(raw_workspace_id.as_deref())?;
                let task_lookup = task_id.trim().to_string();
                let plan_id = input.plan_id.as_ref().and_then(|s| Uuid::parse_str(s).ok());
                let task_listing = self
                    .client
                    .list_tasks(
                        workspace_id,
                        project_id,
                        plan_id,
                        input.task_status.clone(),
                        Some(100),
                    )
                    .await?;
                let (id, resolution_note) = resolve_from_collection_lookup(
                    "tasks",
                    &task_lookup,
                    &task_listing,
                    &["title", "description", "content"],
                )?;
                let result = self.client.delete_task(id).await?;
                let mut text = String::new();
                if let Some(note) = resolution_note {
                    text.push_str(&format!("{}\n", note));
                }
                text.push_str("Task deleted successfully.");
                Ok(ToolResult::with_structured(text, result))
            }
            "list_tasks" => {
                require_explicit_task_workspace(raw_workspace_id.as_deref())?;
                // P1 #6 — MemoryTasksHot warm cache. 30 s TTL.
                // Filter folds plan_id + status + limit so distinct
                // queries don't collide on cache key.
                let plan_id = input.plan_id.as_ref().and_then(|s| Uuid::parse_str(s).ok());
                let task_status_clone = input.task_status.clone();
                let limit_clone = input.limit;
                let filter_str = format!(
                    "plan={};status={};limit={}",
                    plan_id.map(|p| p.to_string()).unwrap_or_default(),
                    task_status_clone.as_deref().unwrap_or(""),
                    limit_clone.unwrap_or(0)
                );
                let user_scope_token =
                    super::atlas_warm_cache::current_user_scope_token();
                let scope_hash = if let Some(ws) = workspace_id {
                    super::atlas_warm_cache::scope_hash_for_list(
                        ws,
                        user_scope_token.as_deref(),
                        project_id,
                        "tasks",
                        Some(&filter_str),
                    )
                } else {
                    String::new()
                };
                let client = self.client.clone();
                let user_scope_for_fetch = user_scope_token.clone();
                let fetch_result = super::atlas_warm_cache::fetch_or_cache(
                    &self.atlas_layer,
                    mcp_types::atlas_layer::AtlasWarmCacheKind::MemoryTasksHot,
                    workspace_id,
                    user_scope_for_fetch.as_deref(),
                    project_id,
                    scope_hash,
                    150,
                    || async move {
                        client
                            .list_tasks(workspace_id, project_id, plan_id, task_status_clone, limit_clone)
                            .await
                    },
                )
                .await;
                let result = match fetch_result {
                    Ok(r) => r,
                    Err(err) if super::workspace_drift::is_workspace_access_error(&err) => {
                        return Ok(super::workspace_drift::drift_collection_result(
                            "tasks",
                            workspace_id,
                            None,
                        ));
                    }
                    Err(err) => return Err(err),
                };
                let text = format_collection("tasks", &result);
                Ok(ToolResult::with_structured(text, result))
            }
            "reorder_tasks" => {
                let task_ids = input
                    .task_ids
                    .ok_or_else(|| Error::Validation("task_ids array is required".to_string()))?;
                let uuids: Vec<Uuid> = task_ids
                    .iter()
                    .filter_map(|s| Uuid::parse_str(s).ok())
                    .collect();
                if uuids.is_empty() {
                    return Err(Error::Validation("No valid UUIDs in task_ids".to_string()));
                }
                require_explicit_task_workspace(raw_workspace_id.as_deref())?;
                let plan_id = input.plan_id.as_ref().and_then(|s| Uuid::parse_str(s).ok());
                let result = self.client.reorder_tasks(uuids, plan_id).await?;
                Ok(ToolResult::with_structured(
                    "Tasks reordered successfully.".to_string(),
                    result,
                ))
            }

            // === Todo Actions ===
            "create_todo" => {
                let title = input
                    .title
                    .ok_or_else(|| Error::Validation("title is required".to_string()))?;
                let title_for_user = title.clone();
                let params = CreateTodoParams {
                    title,
                    content: input.content,
                    priority: input.todo_priority,
                    due_at: input.due_at,
                    is_personal: input.is_personal,
                    workspace_id: None,
                    project_id: None,
                };
                let (mut result, scope) = execute_write_with_scope_recovery(
                    &self.client,
                    &self.session,
                    raw_workspace_id.as_deref(),
                    raw_project_id.as_deref(),
                    params,
                    |params, workspace_id, project_id| {
                        params.workspace_id = workspace_id;
                        params.project_id = project_id;
                    },
                    |client, params| async move { client.create_todo(params).await },
                )
                .await?;
                attach_scope_recovery_metadata(&mut result, &scope);
                let todo_id = result
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                if let Some(obj) = result.as_object_mut() {
                    obj.insert(
                        "operation_status".to_string(),
                        serde_json::json!({
                            "operation": "create_todo",
                            "state": "completed"
                        }),
                    );
                    obj.insert(
                        "user_visibility_hint".to_string(),
                        serde_json::json!({
                            "announce_now": "Todo saved successfully.",
                            "note": "Todo creation is synchronous and complete when this response is returned."
                        }),
                    );
                }
                let text = format!(
                    "Todo created: {} (ID: {}).\nProgress: completed.",
                    title_for_user, todo_id
                );
                let mut output = ToolResult::with_structured(text, result);
                if let Some(note) = scope.note {
                    output = output.with_prefix(format!("{}\n", note));
                }
                Ok(output)
            }
            "get_todo" => {
                let todo_id = input
                    .todo_id
                    .ok_or_else(|| Error::Validation("todo_id is required".to_string()))?;
                let todo_lookup = todo_id.trim().to_string();
                let todo_listing = self
                    .client
                    .list_todos(ListTodosParams {
                        workspace_id,
                        project_id,
                        status: input.todo_status.clone(),
                        priority: input.todo_priority.clone(),
                        is_personal: input.is_personal,
                        scope: input.scope.clone(),
                        query: Some(todo_lookup.clone()),
                        created_after: None,
                        created_before: None,
                        updated_after: None,
                        updated_before: None,
                        due_after: None,
                        due_before: None,
                        completed_after: None,
                        completed_before: None,
                        limit: Some(100),
                        page: None,
                    })
                    .await?;
                let (id, resolution_note) = resolve_from_collection_lookup(
                    "todos",
                    &todo_lookup,
                    &todo_listing,
                    &["title", "content", "description"],
                )?;
                let result = self.client.get_todo(id).await?;
                consume_grounding_memory_tool(&self.session).await;
                let mut text = String::new();
                if let Some(note) = resolution_note {
                    text.push_str(&format!("{}\n\n", note));
                }
                text.push_str(&format_todo_detail(&result));
                Ok(ToolResult::with_structured(text, result))
            }
            "update_todo" => {
                let todo_id = input
                    .todo_id
                    .ok_or_else(|| Error::Validation("todo_id is required".to_string()))?;
                let todo_lookup = todo_id.trim().to_string();
                let todo_listing = self
                    .client
                    .list_todos(ListTodosParams {
                        workspace_id,
                        project_id,
                        status: input.todo_status.clone(),
                        priority: input.todo_priority.clone(),
                        is_personal: input.is_personal,
                        scope: input.scope.clone(),
                        query: Some(todo_lookup.clone()),
                        created_after: None,
                        created_before: None,
                        updated_after: None,
                        updated_before: None,
                        due_after: None,
                        due_before: None,
                        completed_after: None,
                        completed_before: None,
                        limit: Some(100),
                        page: None,
                    })
                    .await?;
                let (id, resolution_note) = resolve_from_collection_lookup(
                    "todos",
                    &todo_lookup,
                    &todo_listing,
                    &["title", "content", "description"],
                )?;
                let params = UpdateTodoParams {
                    title: input.title,
                    content: input.content,
                    priority: input.todo_priority,
                    due_at: input.due_at,
                    clear_due_at: input.clear_due_at,
                    status: input.todo_status,
                };
                let mut result = self.client.update_todo(id, params).await?;
                if let Some(obj) = result.as_object_mut() {
                    obj.insert(
                        "operation_status".to_string(),
                        serde_json::json!({
                            "operation": "update_todo",
                            "state": "completed"
                        }),
                    );
                    obj.insert(
                        "user_visibility_hint".to_string(),
                        serde_json::json!({
                            "announce_now": "Todo update completed.",
                            "note": "Todo update is synchronous and complete when this response is returned."
                        }),
                    );
                }
                let mut text = String::new();
                if let Some(note) = resolution_note {
                    text.push_str(&format!("{}\n", note));
                }
                text.push_str(&format!("Todo updated: {}.\nProgress: completed.", id));
                Ok(ToolResult::with_structured(text, result))
            }
            "delete_todo" => {
                let todo_id = input
                    .todo_id
                    .ok_or_else(|| Error::Validation("todo_id is required".to_string()))?;
                let todo_lookup = todo_id.trim().to_string();
                let todo_listing = self
                    .client
                    .list_todos(ListTodosParams {
                        workspace_id,
                        project_id,
                        status: input.todo_status.clone(),
                        priority: input.todo_priority.clone(),
                        is_personal: input.is_personal,
                        scope: input.scope.clone(),
                        query: Some(todo_lookup.clone()),
                        created_after: None,
                        created_before: None,
                        updated_after: None,
                        updated_before: None,
                        due_after: None,
                        due_before: None,
                        completed_after: None,
                        completed_before: None,
                        limit: Some(100),
                        page: None,
                    })
                    .await?;
                let (id, resolution_note) = resolve_from_collection_lookup(
                    "todos",
                    &todo_lookup,
                    &todo_listing,
                    &["title", "content", "description"],
                )?;
                let result = self.client.delete_todo(id).await?;
                let mut text = String::new();
                if let Some(note) = resolution_note {
                    text.push_str(&format!("{}\n", note));
                }
                text.push_str("Todo deleted successfully.");
                Ok(ToolResult::with_structured(text, result))
            }
            "complete_todo" => {
                let todo_id = input
                    .todo_id
                    .ok_or_else(|| Error::Validation("todo_id is required".to_string()))?;
                let todo_lookup = todo_id.trim().to_string();
                let todo_listing = self
                    .client
                    .list_todos(ListTodosParams {
                        workspace_id,
                        project_id,
                        status: input.todo_status.clone(),
                        priority: input.todo_priority.clone(),
                        is_personal: input.is_personal,
                        scope: input.scope.clone(),
                        query: Some(todo_lookup.clone()),
                        created_after: None,
                        created_before: None,
                        updated_after: None,
                        updated_before: None,
                        due_after: None,
                        due_before: None,
                        completed_after: None,
                        completed_before: None,
                        limit: Some(100),
                        page: None,
                    })
                    .await?;
                let (id, resolution_note) = resolve_from_collection_lookup(
                    "todos",
                    &todo_lookup,
                    &todo_listing,
                    &["title", "content", "description"],
                )?;
                let result = self.client.complete_todo(id).await?;
                let mut text = String::new();
                if let Some(note) = resolution_note {
                    text.push_str(&format!("{}\n", note));
                }
                text.push_str("Todo completed successfully.");
                Ok(ToolResult::with_structured(text, result))
            }
            "list_todos" => {
                // P1 #6 — MemoryTodosHot warm cache. 30 s TTL.
                // Filter folds the most-distinguishing inputs
                // (status, priority, is_personal, scope, query) so
                // different filter shapes don't collide.
                let params = ListTodosParams {
                    workspace_id,
                    project_id,
                    status: input.todo_status,
                    priority: input.todo_priority,
                    is_personal: input.is_personal,
                    scope: input.scope,
                    query: input.query,
                    created_after: input.created_after,
                    created_before: input.created_before,
                    updated_after: input.updated_after,
                    updated_before: input.updated_before,
                    due_after: input.due_after,
                    due_before: input.due_before,
                    completed_after: input.completed_after,
                    completed_before: input.completed_before,
                    limit: input.limit,
                    page: None,
                };
                let filter_str = format!(
                    "status={};priority={};personal={};scope={};query={};limit={}",
                    params.status.as_deref().unwrap_or(""),
                    params.priority.as_deref().unwrap_or(""),
                    params.is_personal.map(|b| b.to_string()).unwrap_or_default(),
                    params.scope.as_deref().unwrap_or(""),
                    params.query.as_deref().unwrap_or(""),
                    params.limit.unwrap_or(0)
                );
                let user_scope_token =
                    super::atlas_warm_cache::current_user_scope_token();
                let scope_hash = if let Some(ws) = workspace_id {
                    super::atlas_warm_cache::scope_hash_for_list(
                        ws,
                        user_scope_token.as_deref(),
                        project_id,
                        "todos",
                        Some(&filter_str),
                    )
                } else {
                    String::new()
                };
                let client = self.client.clone();
                let user_scope_for_fetch = user_scope_token.clone();
                let fetch_result = super::atlas_warm_cache::fetch_or_cache(
                    &self.atlas_layer,
                    mcp_types::atlas_layer::AtlasWarmCacheKind::MemoryTodosHot,
                    workspace_id,
                    user_scope_for_fetch.as_deref(),
                    project_id,
                    scope_hash,
                    150,
                    || async move { client.list_todos(params).await },
                )
                .await;
                let result = match fetch_result {
                    Ok(r) => r,
                    Err(err) if super::workspace_drift::is_workspace_access_error(&err) => {
                        return Ok(super::workspace_drift::drift_collection_result(
                            "todos",
                            workspace_id,
                            None,
                        ));
                    }
                    Err(err) => return Err(err),
                };
                let text = format_collection("todos", &result);
                Ok(ToolResult::with_structured(text, result))
            }

            // === Diagram Actions ===
            "create_diagram" => {
                let title = input
                    .title
                    .ok_or_else(|| Error::Validation("title is required".to_string()))?;
                let content = input.content.ok_or_else(|| {
                    Error::Validation("content (mermaid code) is required".to_string())
                })?;
                let title_for_user = title.clone();
                let params = CreateDiagramParams {
                    title,
                    content,
                    diagram_type: input.diagram_type,
                    metadata: input.metadata,
                    workspace_id: None,
                    project_id: None,
                is_personal: input.is_personal,
                };
                let (mut result, scope) = execute_write_with_scope_recovery(
                    &self.client,
                    &self.session,
                    raw_workspace_id.as_deref(),
                    raw_project_id.as_deref(),
                    params,
                    |params, workspace_id, project_id| {
                        params.workspace_id = workspace_id;
                        params.project_id = project_id;
                    },
                    |client, params| async move { client.create_diagram(params).await },
                )
                .await?;
                attach_scope_recovery_metadata(&mut result, &scope);
                let diagram_id = result
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                if let Some(obj) = result.as_object_mut() {
                    obj.insert(
                        "operation_status".to_string(),
                        serde_json::json!({
                            "operation": "create_diagram",
                            "state": "completed"
                        }),
                    );
                    obj.insert(
                        "user_visibility_hint".to_string(),
                        serde_json::json!({
                            "announce_now": "Diagram saved successfully.",
                            "note": "Diagram creation is synchronous and complete when this response is returned."
                        }),
                    );
                }
                let text = format!(
                    "Diagram created: {} (ID: {}).\nProgress: completed.",
                    title_for_user, diagram_id
                );
                let mut output = ToolResult::with_structured(text, result);
                if let Some(note) = scope.note {
                    output = output.with_prefix(format!("{}\n", note));
                }
                Ok(output)
            }
            "get_diagram" => {
                let diagram_id = input
                    .diagram_id
                    .ok_or_else(|| Error::Validation("diagram_id is required".to_string()))?;
                let diagram_lookup = diagram_id.trim().to_string();
                let diagrams_listing = self
                    .client
                    .list_diagrams(
                        workspace_id,
                        project_id,
                        input.is_personal,
                        Some(100),
                        input.diagram_type.clone(),
                    )
                    .await?;
                let (id, resolution_note) = resolve_from_collection_lookup(
                    "diagrams",
                    &diagram_lookup,
                    &diagrams_listing,
                    &["title", "content", "description"],
                )?;
                let result = self.client.get_diagram(id).await?;
                let mut text = String::new();
                if let Some(note) = resolution_note {
                    text.push_str(&format!("{}\n\n", note));
                }
                text.push_str("Diagram retrieved.");
                Ok(ToolResult::with_structured(text, result))
            }
            "update_diagram" => {
                let diagram_id = input
                    .diagram_id
                    .ok_or_else(|| Error::Validation("diagram_id is required".to_string()))?;
                let diagram_lookup = diagram_id.trim().to_string();
                let diagrams_listing = self
                    .client
                    .list_diagrams(
                        workspace_id,
                        project_id,
                        input.is_personal,
                        Some(100),
                        input.diagram_type.clone(),
                    )
                    .await?;
                let (id, resolution_note) = resolve_from_collection_lookup(
                    "diagrams",
                    &diagram_lookup,
                    &diagrams_listing,
                    &["title", "content", "description"],
                )?;
                let params = UpdateDiagramParams {
                    title: input.title,
                    content: input.content,
                    diagram_type: input.diagram_type,
                    metadata: input.metadata,
                };
                let mut result = self.client.update_diagram(id, params).await?;
                if let Some(obj) = result.as_object_mut() {
                    obj.insert(
                        "operation_status".to_string(),
                        serde_json::json!({
                            "operation": "update_diagram",
                            "state": "completed"
                        }),
                    );
                    obj.insert(
                        "user_visibility_hint".to_string(),
                        serde_json::json!({
                            "announce_now": "Diagram update completed.",
                            "note": "Diagram update is synchronous and complete when this response is returned."
                        }),
                    );
                }
                let mut text = String::new();
                if let Some(note) = resolution_note {
                    text.push_str(&format!("{}\n", note));
                }
                text.push_str(&format!("Diagram updated: {}.\nProgress: completed.", id));
                Ok(ToolResult::with_structured(text, result))
            }
            "delete_diagram" => {
                let diagram_id = input
                    .diagram_id
                    .ok_or_else(|| Error::Validation("diagram_id is required".to_string()))?;
                let diagram_lookup = diagram_id.trim().to_string();
                let diagrams_listing = self
                    .client
                    .list_diagrams(
                        workspace_id,
                        project_id,
                        input.is_personal,
                        Some(100),
                        input.diagram_type.clone(),
                    )
                    .await?;
                let (id, resolution_note) = resolve_from_collection_lookup(
                    "diagrams",
                    &diagram_lookup,
                    &diagrams_listing,
                    &["title", "content", "description"],
                )?;
                let result = self.client.delete_diagram(id).await?;
                let mut text = String::new();
                if let Some(note) = resolution_note {
                    text.push_str(&format!("{}\n", note));
                }
                text.push_str("Diagram deleted successfully.");
                Ok(ToolResult::with_structured(text, result))
            }
            "list_diagrams" => {
                let result = self
                    .client
                    .list_diagrams(
                        workspace_id,
                        project_id,
                        input.is_personal,
                        input.limit,
                        input.diagram_type,
                    )
                    .await?;
                let text = format!(
                    "{}\n{}\nExamples: memory(action=\"create_diagram\", diagram_type=\"sequence\", title=\"Auth handoff\", content=\"...\") | memory(action=\"create_diagram\", diagram_type=\"er\", title=\"Billing schema\", content=\"...\")",
                    format_collection("diagrams", &result),
                    diagram_types_help_suffix()
                );
                Ok(ToolResult::with_structured(text, result))
            }

            // === Doc Actions ===
            "create_doc" => {
                let title = input
                    .title
                    .ok_or_else(|| Error::Validation("title is required".to_string()))?;
                let content = input
                    .content
                    .ok_or_else(|| Error::Validation("content is required".to_string()))?;
                let title_for_user = title.clone();
                let params = CreateDocParams {
                    title,
                    content,
                    doc_type: input.doc_type,
                    metadata: input.metadata,
                    workspace_id: None,
                    project_id: None,
                is_personal: input.is_personal,
                };
                let (mut result, scope) = execute_write_with_scope_recovery(
                    &self.client,
                    &self.session,
                    raw_workspace_id.as_deref(),
                    raw_project_id.as_deref(),
                    params,
                    |params, workspace_id, project_id| {
                        params.workspace_id = workspace_id;
                        params.project_id = project_id;
                    },
                    |client, params| async move { client.create_doc(params).await },
                )
                .await?;
                attach_scope_recovery_metadata(&mut result, &scope);
                let doc_id = result
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                if let Some(obj) = result.as_object_mut() {
                    obj.insert(
                        "operation_status".to_string(),
                        serde_json::json!({
                            "operation": "create_doc",
                            "state": "completed"
                        }),
                    );
                    obj.insert(
                        "user_visibility_hint".to_string(),
                        serde_json::json!({
                            "announce_now": "Doc saved successfully.",
                            "note": "Doc creation is synchronous and complete when this response is returned."
                        }),
                    );
                }
                let text = format!(
                    "Doc created: {} (ID: {}).\nProgress: completed.",
                    title_for_user, doc_id
                );
                let mut output = ToolResult::with_structured(text, result);
                if let Some(note) = scope.note {
                    output = output.with_prefix(format!("{}\n", note));
                }
                Ok(output)
            }
            "get_doc" => {
                let doc_id = input
                    .doc_id
                    .ok_or_else(|| Error::Validation("doc_id is required".to_string()))?;
                if let Ok(id) = Uuid::parse_str(&doc_id) {
                    // P1 #8 — DocHot warm cache. 1 hr TTL. Doc-id is
                    // the stable key. Title-search path (below) is
                    // not cached — those queries vary too much for
                    // the cache to help.
                    let id_str = id.to_string();
                    let user_scope_token =
                        super::atlas_warm_cache::current_user_scope_token();
                    let scope_hash = if let Some(ws) = workspace_id {
                        super::atlas_warm_cache::scope_hash_for_doc_hot(
                            ws,
                            user_scope_token.as_deref(),
                            project_id,
                            &id_str,
                        )
                    } else {
                        String::new()
                    };
                    let client = self.client.clone();
                    let user_scope_for_fetch = user_scope_token.clone();
                    let result = super::atlas_warm_cache::fetch_or_cache(
                        &self.atlas_layer,
                        mcp_types::atlas_layer::AtlasWarmCacheKind::DocHot,
                        workspace_id,
                        user_scope_for_fetch.as_deref(),
                        project_id,
                        scope_hash,
                        300,
                        || async move { client.get_doc(id).await },
                    )
                    .await?;
                    let text = format_doc_detail(&result);
                    consume_grounding_memory_tool(&self.session).await;
                    return Ok(ToolResult::with_structured(text, result));
                }

                let query = doc_id.trim();
                if query.is_empty() {
                    return Err(Error::Validation("doc_id is required".to_string()));
                }

                let matches = search_docs_by_title(
                    &self.client,
                    workspace_id,
                    project_id,
                    None,
                    None,
                    query,
                    input.limit,
                )
                .await?;

                if matches.is_empty() {
                    return Err(Error::Validation(format!(
                        "No doc found matching \"{}\". Use list_docs to browse available docs.",
                        query
                    )));
                }

                if let Some(doc) = select_resolved_doc_match(&matches, query) {
                    if let Some(id_str) = doc.get("id").and_then(|v| v.as_str()) {
                        if let Ok(doc_uuid) = Uuid::parse_str(id_str) {
                            let result = self.client.get_doc(doc_uuid).await?;
                            let text = format!(
                                "Resolved doc query \"{}\" to doc ID {}.\n\n{}",
                                query,
                                id_str,
                                format_doc_detail(&result)
                            );
                            consume_grounding_memory_tool(&self.session).await;
                            return Ok(ToolResult::with_structured(text, result));
                        }
                    }
                }

                let text = format_doc_matches(query, &matches);
                let structured = serde_json::json!({
                    "query": query,
                    "matches": matches,
                });
                consume_grounding_memory_tool(&self.session).await;
                Ok(ToolResult::with_structured(text, structured))
            }
            "update_doc" => {
                let doc_id = input
                    .doc_id
                    .ok_or_else(|| Error::Validation("doc_id is required".to_string()))?;
                let (id, resolution_note) = resolve_doc_uuid_for_action(
                    &self.client,
                    workspace_id,
                    project_id,
                    doc_id.trim(),
                    input.doc_type.as_deref(),
                    input.is_personal,
                    input.limit,
                )
                .await?;
                let params = UpdateDocParams {
                    title: input.title,
                    content: input.content,
                    doc_type: input.doc_type,
                };
                let mut result = self.client.update_doc(id, params).await?;
                if let Some(obj) = result.as_object_mut() {
                    obj.insert(
                        "operation_status".to_string(),
                        serde_json::json!({
                            "operation": "update_doc",
                            "state": "completed"
                        }),
                    );
                    obj.insert(
                        "user_visibility_hint".to_string(),
                        serde_json::json!({
                            "announce_now": "Doc update completed.",
                            "note": "Doc update is synchronous and complete when this response is returned."
                        }),
                    );
                }
                let mut text = String::new();
                if let Some(note) = resolution_note {
                    text.push_str(&format!("{}\n", note));
                }
                text.push_str(&format!("Doc updated: {}.\nProgress: completed.", id));
                Ok(ToolResult::with_structured(text, result))
            }
            "delete_doc" => {
                let doc_id = input
                    .doc_id
                    .ok_or_else(|| Error::Validation("doc_id is required".to_string()))?;
                let (id, resolution_note) = resolve_doc_uuid_for_action(
                    &self.client,
                    workspace_id,
                    project_id,
                    doc_id.trim(),
                    input.doc_type.as_deref(),
                    input.is_personal,
                    input.limit,
                )
                .await?;
                let result = self.client.delete_doc(id).await?;
                let mut text = String::new();
                if let Some(note) = resolution_note {
                    text.push_str(&format!("{}\n", note));
                }
                text.push_str("Doc deleted successfully.");
                Ok(ToolResult::with_structured(text, result))
            }
            "list_docs" => {
                if let Some(query) = input.query.as_deref() {
                    let trimmed = query.trim();
                    if !trimmed.is_empty() {
                        let matches = search_docs_by_title(
                            &self.client,
                            workspace_id,
                            project_id,
                            input.doc_type.as_deref(),
                            input.is_personal,
                            trimmed,
                            input.limit,
                        )
                        .await?;
                        if matches.is_empty() {
                            let text = format!(
                                "No docs found matching \"{}\". Use list_docs without query to browse all.",
                                trimmed
                            );
                            let structured = serde_json::json!({
                                "query": trimmed,
                                "matches": []
                            });
                            return Ok(ToolResult::with_structured(text, structured));
                        }

                        let text = format_doc_matches(trimmed, &matches);
                        let structured = serde_json::json!({
                            "query": trimmed,
                            "matches": matches
                        });
                        return Ok(ToolResult::with_structured(text, structured));
                    }
                }

                let result = match self
                    .client
                    .list_docs(
                        workspace_id,
                        project_id,
                        input.doc_type,
                        input.is_personal,
                        None,
                        input.limit,
                    )
                    .await
                {
                    Ok(r) => r,
                    Err(err) if super::workspace_drift::is_workspace_access_error(&err) => {
                        return Ok(super::workspace_drift::drift_collection_result(
                            "docs",
                            workspace_id,
                            None,
                        ));
                    }
                    Err(err) => return Err(err),
                };
                let text = format_collection("docs", &result);
                Ok(ToolResult::with_structured(text, result))
            }
            "create_roadmap" => {
                let title = input
                    .title
                    .ok_or_else(|| Error::Validation("title is required".to_string()))?;
                let roadmap_title = title.clone();
                let milestones = input.milestones.map(|ms| {
                    ms.into_iter()
                        .filter_map(|v| serde_json::from_value::<RoadmapMilestone>(v).ok())
                        .collect()
                });
                let params = CreateRoadmapParams {
                    title,
                    milestones,
                    workspace_id: None,
                    project_id: None,
                is_personal: input.is_personal,
                };
                let (mut result, scope) = execute_write_with_scope_recovery(
                    &self.client,
                    &self.session,
                    raw_workspace_id.as_deref(),
                    raw_project_id.as_deref(),
                    params,
                    |params, workspace_id, project_id| {
                        params.workspace_id = workspace_id;
                        params.project_id = project_id;
                    },
                    |client, params| async move { client.create_roadmap(params).await },
                )
                .await?;
                attach_scope_recovery_metadata(&mut result, &scope);
                let roadmap_id = result
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                if let Some(obj) = result.as_object_mut() {
                    obj.insert(
                        "operation_status".to_string(),
                        serde_json::json!({
                            "operation": "create_roadmap",
                            "state": "completed"
                        }),
                    );
                    obj.insert(
                        "user_visibility_hint".to_string(),
                        serde_json::json!({
                            "announce_now": "Roadmap saved successfully.",
                            "note": "If this operation takes time, provide an in-progress update before running it."
                        }),
                    );
                }
                let text = format!(
                    "Roadmap created: {} (ID: {}).\nProgress: completed.",
                    roadmap_title, roadmap_id
                );
                let mut output = ToolResult::with_structured(text, result);
                if let Some(note) = scope.note {
                    output = output.with_prefix(format!("{}\n", note));
                }
                Ok(output)
            }

            // === Transcript Actions ===
            "list_transcripts" => {
                let result = self
                    .client
                    .list_transcripts(
                        workspace_id,
                        project_id,
                        input.session_id,
                        input.client_name,
                        input.started_after,
                        input.started_before,
                        input.limit,
                    )
                    .await?;
                let text = format_collection("transcripts", &result);
                Ok(ToolResult::with_structured(text, result))
            }
            "get_transcript" => {
                let transcript_id = input
                    .transcript_id
                    .ok_or_else(|| Error::Validation("transcript_id is required".to_string()))?;
                let id = Uuid::parse_str(&transcript_id)
                    .map_err(|_| Error::Validation("Invalid transcript_id UUID".to_string()))?;
                let result = self.client.get_transcript(id).await?;
                consume_grounding_memory_tool(&self.session).await;
                let text = format_transcript_detail(&result);
                Ok(ToolResult::with_structured(text, result))
            }
            "search_transcripts" => {
                let query = input
                    .query
                    .ok_or_else(|| Error::Validation("query is required".to_string()))?;
                let caller_cache_identity = current_memory_cache_identity();
                let atlas_search_provider = caller_cache_identity
                    .as_ref()
                    .and_then(|_| self.atlas_layer.search());
                let cache_key = transcripts_search_cache_key_for_caller(
                    caller_cache_identity.as_deref(),
                    workspace_id,
                    project_id,
                    &query,
                    input.limit,
                    atlas_search_provider.is_some(),
                );
                if let Some(cache_key) = cache_key.as_deref() {
                    if let Some((cached_text, cached_structured)) =
                        transcripts_search_cache().get(cache_key)
                    {
                        tracing::debug!("transcripts search cache hit: key={}", cache_key);
                        let marked = format!(
                            "[TRANSCRIPTS_CACHED] Same transcripts search as the previous \
                             identical call (<{}s ago); returning cached result. \
                             Change query/limit to refresh.\n\n{}",
                            TRANSCRIPTS_SEARCH_CACHE_TTL.as_secs(),
                            cached_text
                        );
                        return Ok(ToolResult::with_structured(marked, cached_structured));
                    }
                }
                let params = SearchTranscriptsParams {
                    query: query.clone(),
                    limit: input.limit,
                    workspace_id,
                    project_id,
                };
                let mut result = self.client.search_transcripts(params).await?;

                // P1 #10 — Atlas Search Lucene enrichment. PG full-text
                // doesn't fuzzy-match (typos miss); Atlas Search adds
                // typo-tolerance + faceted filtering. Best-effort:
                // when the search provider is available we issue a
                // parallel fuzzy_text_search over the same scope and
                // attach the hits as `atlas_search_hits` on the
                // structured response. The PG result remains the
                // primary user-facing list — Atlas hits are
                // supplementary so a degraded Atlas layer never
                // worsens the response. 250 ms hard wall-clock cap on
                // the Atlas call so a slow Atlas Search never delays
                // the response (analogous to atlas_warm_cache's
                // 50 ms try_lookup cap; lesson 53be7d19).
                enrich_transcript_search_with_atlas(
                    atlas_search_provider,
                    caller_cache_identity.as_deref(),
                    workspace_id,
                    project_id,
                    &query,
                    input.limit,
                    &mut result,
                )
                .await;

                let pg_count = collection_count(&result);
                let atlas_count = result
                    .get("atlas_search_hits")
                    .and_then(|v| v.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                let text = if atlas_count > 0 {
                    format!(
                        "Found {} matching transcripts (+{} fuzzy hit(s)).",
                        pg_count, atlas_count
                    )
                } else {
                    format!("Found {} matching transcripts.", pg_count)
                };
                if let Some(cache_key) = cache_key {
                    put_transcripts_search_cache(
                        caller_cache_identity.as_deref(),
                        cache_key,
                        (text.clone(), result.clone()),
                    );
                }
                Ok(ToolResult::with_structured(text, result))
            }
            "delete_transcript" => {
                let transcript_id = input
                    .transcript_id
                    .ok_or_else(|| Error::Validation("transcript_id is required".to_string()))?;
                let id = Uuid::parse_str(&transcript_id)
                    .map_err(|_| Error::Validation("Invalid transcript_id UUID".to_string()))?;
                let result = self.client.delete_transcript(id).await?;
                Ok(ToolResult::with_structured(
                    "Transcript deleted successfully.".to_string(),
                    result,
                ))
            }
            "search_archive" => {
                let workspace_id = workspace_id.ok_or_else(|| {
                    Error::Validation(
                        "search_archive requires a resolved workspace_id".to_string(),
                    )
                })?;
                let limit = input.limit.unwrap_or(20).max(1).min(50) as usize;
                let query_text = input.query.unwrap_or_default();

                if let Some(provider) = self.acceleration_layer.archive() {
                    use mcp_types::acceleration_layer::{
                        AccelerationArchiveCollection, AccelerationArchiveScope,
                    };
                    let mut scope = AccelerationArchiveScope::new(workspace_id);
                    scope.project_id = project_id;
                    if let Some(scope_str) = input.scope.as_deref() {
                        if let Some(collection) = match scope_str.to_ascii_lowercase().as_str() {
                            "transcripts" => Some(AccelerationArchiveCollection::Transcripts),
                            "decisions" => Some(AccelerationArchiveCollection::Decisions),
                            "lessons" => Some(AccelerationArchiveCollection::Lessons),
                            "docs" => Some(AccelerationArchiveCollection::Docs),
                            "qa_questions" => Some(AccelerationArchiveCollection::QaQuestions),
                            "qa_answers" => Some(AccelerationArchiveCollection::QaAnswers),
                            "qa_kb_items" => Some(AccelerationArchiveCollection::QaKbItems),
                            _ => None,
                        } {
                            scope.collection = Some(collection);
                        }
                    }

                    let started = std::time::Instant::now();
                    let hits = match provider.search_archive(&query_text, &scope, limit).await {
                        Ok(hits) => hits,
                        Err(error) => {
                            tracing::warn!(error = %error, "acceleration-archive: provider call failed");
                            return Ok(ToolResult::with_structured(
                                format!("[ARCHIVE] error: {}", error),
                                serde_json::json!({
                                    "stages_used": ["r2_archive"],
                                    "error": error.to_string(),
                                    "results": [],
                                }),
                            ));
                        }
                    };
                    let elapsed_ms = started.elapsed().as_millis() as u64;
                    let count = hits.len();
                    let degraded_count = hits.iter().filter(|hit| hit.degraded).count();
                    let header = if count == 0 {
                        format!(
                            "[ARCHIVE] 0 archived hits for `{}` ({}ms; archive_manifest may be empty)",
                            query_text.trim(),
                            elapsed_ms
                        )
                    } else {
                        let lines: Vec<String> = hits
                            .iter()
                            .take(10)
                            .enumerate()
                            .map(|(i, hit)| {
                                let archived = hit
                                    .archived_at
                                    .map(|time| time.to_rfc3339())
                                    .unwrap_or_else(|| "<unknown>".to_string());
                                let score = hit
                                    .score
                                    .map(|score| format!(" score {:.3}", score))
                                    .unwrap_or_default();
                                let degraded = if hit.degraded { " degraded" } else { "" };
                                format!(
                                    "  {}. [{}] {} (archived {}{}{})",
                                    i + 1,
                                    hit.collection.as_str(),
                                    hit.title.as_deref().unwrap_or("(untitled)"),
                                    archived,
                                    score,
                                    degraded
                                )
                            })
                            .collect();
                        format!(
                            "[ARCHIVE] {} hit(s) for `{}` ({}ms{})\n{}",
                            count,
                            query_text.trim(),
                            elapsed_ms,
                            if degraded_count > 0 {
                                format!("; {} degraded", degraded_count)
                            } else {
                                String::new()
                            },
                            lines.join("\n")
                        )
                    };
                    let results: Vec<serde_json::Value> = hits
                        .iter()
                        .map(|hit| {
                            serde_json::json!({
                                "id": hit.id,
                                "subject_id": hit.subject_id,
                                "collection": hit.collection.as_str(),
                                "title": hit.title,
                                "snippet": hit.snippet,
                                "archived_at": hit.archived_at.map(|time| time.to_rfc3339()),
                                "score": hit.score,
                                "degraded": hit.degraded,
                                "note": hit.note,
                                "origin": "r2_archive",
                            })
                        })
                        .collect();

                    return Ok(ToolResult::with_structured(
                        header,
                        serde_json::json!({
                            "stages_used": ["r2_archive"],
                            "available": true,
                            "query": query_text,
                            "elapsed_ms": elapsed_ms,
                            "result_count": count,
                            "degraded_count": degraded_count,
                            "results": results,
                            "marker": "[ARCHIVE]",
                        }),
                    ));
                }

                // Temporary compatibility fallback: Atlas Online
                // Archive remains available only during migration
                // canaries. The preferred provider above reads
                // Postgres archive_manifest + R2.
                use mcp_types::atlas_layer::AtlasArchiveScope;
                let provider = match self.atlas_layer.archive() {
                    Some(p) => p,
                    None => {
                        let note = if self.acceleration_layer.is_enabled()
                            || self.atlas_layer.is_enabled()
                        {
                            "[ARCHIVE] disabled (archive provider unavailable for this deployment)"
                        } else {
                            "[ARCHIVE] disabled (this deployment does not include archive search; \
                             only available on hosted/remote deployments)"
                        };
                        return Ok(ToolResult::with_structured(
                            note,
                            serde_json::json!({
                                "stages_used": ["r2_archive"],
                                "available": false,
                                "results": [],
                                "marker": "[ARCHIVE]",
                            }),
                        ));
                    }
                };
                metrics::counter!(
                    "archive_search_atlas_fallback_total",
                    "source" => "memory_search_archive",
                )
                .increment(1);
                let mut scope = AtlasArchiveScope::new(workspace_id);
                if let Some(scope_str) = input.scope.as_deref() {
                    if let Some(coll) = match scope_str.to_ascii_lowercase().as_str() {
                        "transcripts" => Some(mcp_types::atlas_layer::AtlasSearchCollection::Transcripts),
                        "decisions" => Some(mcp_types::atlas_layer::AtlasSearchCollection::Decisions),
                        "lessons" => Some(mcp_types::atlas_layer::AtlasSearchCollection::Lessons),
                        "docs" => Some(mcp_types::atlas_layer::AtlasSearchCollection::Docs),
                        "qa_questions" => Some(mcp_types::atlas_layer::AtlasSearchCollection::QaQuestions),
                        "qa_answers" => Some(mcp_types::atlas_layer::AtlasSearchCollection::QaAnswers),
                        "qa_kb_items" => Some(mcp_types::atlas_layer::AtlasSearchCollection::QaKbItems),
                        _ => None,
                    } {
                        scope.collection = Some(coll);
                    }
                }

                let started = std::time::Instant::now();
                let hits = match provider.search_archive(&query_text, &scope, limit).await {
                    Ok(h) => h,
                    Err(e) => {
                        tracing::warn!(error = %e, "atlas-archive: provider call failed");
                        return Ok(ToolResult::with_structured(
                            format!("[ARCHIVE] error: {}", e),
                            serde_json::json!({
                                "stages_used": ["atlas_online_archive"],
                                "error": e.to_string(),
                                "results": [],
                            }),
                        ));
                    }
                };
                let elapsed_ms = started.elapsed().as_millis() as u64;
                let count = hits.len();

                let header = if count == 0 {
                    format!(
                        "[ARCHIVE] 0 archived hits for `{}` ({}ms; archive may be empty until A5 nightly trigger has run)",
                        query_text.trim(),
                        elapsed_ms
                    )
                } else {
                    let lines: Vec<String> = hits
                        .iter()
                        .take(10)
                        .enumerate()
                        .map(|(i, h)| {
                            let archived = h
                                .archived_at
                                .map(|t| t.to_rfc3339())
                                .unwrap_or_else(|| "<unknown>".to_string());
                            let score = h
                                .score
                                .map(|s| format!(" score {:.3}", s))
                                .unwrap_or_default();
                            format!(
                                "  {}. [{}] {} (archived {}{})",
                                i + 1,
                                h.collection.as_str(),
                                h.title.as_deref().unwrap_or("(untitled)"),
                                archived,
                                score
                            )
                        })
                        .collect();
                    format!(
                        "[ARCHIVE] {} hit(s) for `{}` ({}ms)\n{}",
                        count,
                        query_text.trim(),
                        elapsed_ms,
                        lines.join("\n")
                    )
                };

                let results: Vec<serde_json::Value> = hits
                    .iter()
                    .map(|h| {
                        serde_json::json!({
                            "id": h.id,
                            "collection": h.collection.as_str(),
                            "title": h.title,
                            "snippet": h.snippet,
                            "archived_at": h.archived_at.map(|t| t.to_rfc3339()),
                            "score": h.score,
                            "origin": "atlas_online_archive",
                        })
                    })
                    .collect();

                Ok(ToolResult::with_structured(
                    header,
                    serde_json::json!({
                        "stages_used": ["atlas_online_archive"],
                        "available": true,
                        "query": query_text,
                        "elapsed_ms": elapsed_ms,
                        "result_count": count,
                        "results": results,
                    }),
                ))
            }

            // === Timeline & Summary ===
            "timeline" => {
                let result = self.client.memory_timeline(workspace_id).await?;
                Ok(ToolResult::with_structured(
                    "Timeline retrieved.".to_string(),
                    result,
                ))
            }
            "summary" => {
                let result = self.client.memory_summary(workspace_id).await?;
                Ok(ToolResult::with_structured(
                    "Summary retrieved.".to_string(),
                    result,
                ))
            }

            // === Team Actions ===
            "team_tasks" => {
                let items = self
                    .fetch_team_entity(|client, ws_id| {
                        let status = input.task_status.clone();
                        let limit = input.limit;
                        Box::pin(async move {
                            client
                                .list_tasks(Some(ws_id), None, None, status, limit)
                                .await
                        })
                    })
                    .await?;
                let text = format_collection("team tasks", &items);
                Ok(ToolResult::with_structured(text, items))
            }
            "team_todos" => {
                let items = self
                    .fetch_team_entity(|client, ws_id| {
                        let status = input.todo_status.clone();
                        let priority = input.todo_priority.clone();
                        let query = input.query.clone();
                        let created_after = input.created_after.clone();
                        let created_before = input.created_before.clone();
                        let updated_after = input.updated_after.clone();
                        let updated_before = input.updated_before.clone();
                        let due_after = input.due_after.clone();
                        let due_before = input.due_before.clone();
                        let completed_after = input.completed_after.clone();
                        let completed_before = input.completed_before.clone();
                        let limit = input.limit;
                        Box::pin(async move {
                            client
                                .list_todos(ListTodosParams {
                                    workspace_id: Some(ws_id),
                                    project_id: None,
                                    status,
                                    priority,
                                    is_personal: None,
                                    scope: Some("team".to_string()),
                                    query,
                                    created_after,
                                    created_before,
                                    updated_after,
                                    updated_before,
                                    due_after,
                                    due_before,
                                    completed_after,
                                    completed_before,
                                    limit,
                                    page: None,
                                })
                                .await
                        })
                    })
                    .await?;
                let text = format_collection("team todos", &items);
                Ok(ToolResult::with_structured(text, items))
            }
            "team_diagrams" => {
                let items = self
                    .fetch_team_entity(|client, ws_id| {
                        let limit = input.limit;
                        Box::pin(async move {
                            client
                                .list_diagrams(Some(ws_id), None, None, limit, None)
                                .await
                        })
                    })
                    .await?;
                let text = format_collection("team diagrams", &items);
                Ok(ToolResult::with_structured(text, items))
            }
            "team_docs" => {
                let items = self
                    .fetch_team_entity(|client, ws_id| {
                        let doc_type = input.doc_type.clone();
                        let limit = input.limit;
                        Box::pin(async move {
                            client
                                .list_docs(Some(ws_id), None, doc_type, None, None, limit)
                                .await
                        })
                    })
                    .await?;
                let text = format_collection("team docs", &items);
                Ok(ToolResult::with_structured(text, items))
            }
            "team_discussions" => {
                if !self.session.team_features_enabled().await {
                    return Ok(ToolResult::with_structured(
                        "Team discussions require active team mode and team membership.".to_string(),
                        serde_json::json!({ "items": [] }),
                    ));
                }
                let query = input.query.clone();
                let limit = input.limit;
                let discussions = if let Some(q) = query.filter(|s| !s.trim().is_empty()) {
                    self.client.search_team_discussions(&q, limit).await?
                } else {
                    self.client
                        .list_team_discussions(workspace_id, project_id, limit)
                        .await?
                };
                let payload = serde_json::json!({ "items": discussions });
                let text = format_collection("team discussions", &payload);
                Ok(ToolResult::with_structured(text, payload))
            }
            "team_transcript_topics" => {
                let query = input.query.ok_or_else(|| {
                    Error::Validation(
                        "query is required for team_transcript_topics (metadata-only; transcript content is never shared)".to_string(),
                    )
                })?;
                if !self.session.team_features_enabled().await {
                    return Ok(ToolResult::with_structured(
                        "Team transcript topic signals require active team mode.".to_string(),
                        serde_json::json!({ "items": [], "content_shared": false }),
                    ));
                }
                let signals = self
                    .client
                    .search_transcript_topic_signals(&query, input.limit.or(Some(10)))
                    .await?;
                for signal in &signals {
                    debug_assert!(
                        !signal.content_shared,
                        "transcript topic signals must never include shared content"
                    );
                }
                let payload = serde_json::json!({
                    "items": signals,
                    "content_shared": false,
                    "privacy": "metadata_only",
                });
                let text = if signals.is_empty() {
                    format!(
                        "No team-visible topic signals found for '{}'. Transcript bodies remain private.",
                        query
                    )
                } else {
                    format!(
                        "Found {} metadata-only transcript topic signal(s) for '{}'. Transcript content is never shared.",
                        signals.len(),
                        query
                    )
                };
                Ok(ToolResult::with_structured(text, payload))
            }

            // === Legacy alias ===
            "create" => {
                // Redirect to create_node for backwards compatibility
                let node_type = input.node_type.ok_or_else(|| {
                    Error::Validation("node_type is required for create".to_string())
                })?;
                let title = input
                    .title
                    .ok_or_else(|| Error::Validation("title is required for create".to_string()))?;
                let create_input = CreateMemoryNodeInput {
                    node_type,
                    title,
                    content: input.content,
                    workspace_id: input.workspace_id,
                    project_id: input.project_id,
                    metadata: input.metadata,
                };
                let tool = CreateMemoryNodeTool::new(self.client.clone(), self.session.clone());
                tool.execute(serde_json::to_value(&create_input).unwrap())
                    .await
            }

            _ => Err(Error::Validation(format!(
                "Unknown action: {}. Available actions: search, create_node, get_node, update_node, delete_node, list_nodes, supersede_node, decisions, create_decision, decision_action, timeline, summary, create_event, get_event, update_event, delete_event, distill_event, list_events, import_batch, create_task, get_task, update_task, delete_task, list_tasks, reorder_tasks, create_todo, get_todo, update_todo, delete_todo, complete_todo, list_todos, create_diagram, get_diagram, update_diagram, delete_diagram, list_diagrams, create_doc, get_doc, update_doc, delete_doc, list_docs, create_roadmap, list_transcripts, get_transcript, search_transcripts, search_archive, delete_transcript, team_tasks, team_todos, team_diagrams, team_docs, team_discussions, team_transcript_topics.",
                input.action
            ))),
        }
    }

    fn metadata(&self) -> &ToolMetadata {
        static METADATA: std::sync::OnceLock<ToolMetadata> = std::sync::OnceLock::new();
        METADATA.get_or_init(|| ToolMetadata {
            name: "memory".to_string(),
            title: "Memory Operations".to_string(),
            description: "Persistent memory storage — docs, runbooks, specs, ADRs, RFCs, decisions, lessons, preferences, tasks, todos, knowledge nodes, transcripts. NOT for codebase/file search.\n\n⚠️ FINDING A DOC, RUNBOOK, SPEC, OR ARCHITECTURE NOTE? USE THIS TOOL — NOT `find`, `ls`, `grep`, or filesystem searches. ContextStream docs/runbooks/specs/decisions/lessons live ONLY in this tool's storage (Postgres + indexes), NEVER on disk under ~/.claude, /tmp, or the project tree. If the user mentions 'the doc on X', 'our runbook for Y', 'the design spec', 'the ADR/RFC', 'a postmortem', 'the architecture note', 'why we decided Z' — go through:\n  · memory(action=\"search\", query=\"…\") — hybrid across docs + nodes (try this first when unsure)\n  · memory(action=\"list_docs\", query=\"…\") then memory(action=\"get_doc\", doc_id=\"<id-or-title>\")\n  · memory(action=\"decisions\", query=\"…\") for past architectural decisions\n  · session(action=\"recall\", query=\"…\") if it might be in past-session transcripts\nFalling back to filesystem tools to find a ContextStream doc is wrong — the doc is not on disk.\n\nCodebase / source / files? Use the `search` tool, not memory.\n\nPlans? Use session(action=\"capture_plan\") instead of memory(action=\"create_event\", event_type=\"plan\"). Plan tasks should be created with plan_id, plan_step_id, priority/status, and detailed descriptions.\n\nDISTINCT FROM (don't use memory for these):\n· entity(kind=ticket|handoff|incident|release|experiment|goal|key_result|sprint|review|risk|backlog_view) — structured taxonomy entities with their own status timelines and per-kind fields. When the user says 'create a ticket', 'file a bug', 'create a handoff', 'log an incident', 'track this release' — that's `entity`, not memory(create_task).\n· session(action=capture_lesson|capture|recall|capture_plan) — lessons / decisions / snapshots / plans tied to the current session.\n· capsule(...) — portable context bundles for cross-agent handoffs.\n\nThis tool's `create_task` is a lightweight project-tracking todo with priority/status — NOT a 'ticket'. This tool's `create_task` should include plan_id and plan_step_id when the task belongs to a plan. This tool's `create_doc(doc_type=runbook)` is a versioned markdown doc — NOT a 'handoff'.\n\nNode actions: create_node, get_node, update_node, delete_node, list_nodes, supersede_node (node_id accepts an id or lookup text; ambiguous text returns a [CANDIDATES] list). Query actions: search (searches memory nodes and relevant docs together, not code), decisions (typed envelope: query, category, sort=recency|relevance, status=active|superseded|disputed|verified|all, since, offset, limit), timeline, summary. Decision actions: create_decision (title, content, rationale, alternatives, scope, confidence, supersedes, category, tags), decision_action (decision_id or lookup text + decision_action=supersede|dispute|verify|invalidate|choose_successor, successor_id, reason). Event actions: create_event, get_event, update_event, delete_event, list_events, distill_event, import_batch. Task actions: create_task, get_task, update_task, delete_task, list_tasks, reorder_tasks. Todo actions: create_todo, list_todos, get_todo, update_todo, delete_todo, complete_todo. Diagram actions: create_diagram, list_diagrams, get_diagram, update_diagram, delete_diagram (diagram_type values: flowchart, sequence, class, er, gantt, mindmap, pie, other — use sequence for API/request flows and er for data models). Doc actions: create_doc, list_docs, get_doc, update_doc, delete_doc, create_roadmap (doc_type values: roadmap, spec, runbook, adr, rfc, postmortem, retro, release_notes, playbook, prd, user_story, persona, interview, design_spec, critique, glossary, oncall_schedule, slo, q_and_a, changelog, style_guide, general — `get_doc` accepts ID or natural-language title query). Transcript actions: list_transcripts, get_transcript, search_transcripts, search_archive, delete_transcript. `search_archive` queries the cold storage tier for transcripts past the hot-retention window (remote/hosted deployments only). Team actions: team_tasks, team_todos, team_diagrams, team_docs.".to_string(),
            category: ToolCategory::Memory,
            annotations: ToolAnnotations::destructive(),
            is_pro: false,
            required_tier: None,
        })
    }

    fn input_schema(&self) -> Value {
        let all_actions = &[
            // Node actions
            "search",
            "create_node",
            "get_node",
            "update_node",
            "delete_node",
            "list_nodes",
            "supersede_node",
            "decisions",
            "create_decision",
            "decision_action",
            "timeline",
            "summary",
            // Event actions
            "create_event",
            "get_event",
            "update_event",
            "delete_event",
            "distill_event",
            "list_events",
            "import_batch",
            // Task actions
            "create_task",
            "get_task",
            "update_task",
            "delete_task",
            "list_tasks",
            "reorder_tasks",
            // Todo actions
            "create_todo",
            "list_todos",
            "get_todo",
            "update_todo",
            "delete_todo",
            "complete_todo",
            // Diagram actions
            "create_diagram",
            "list_diagrams",
            "get_diagram",
            "update_diagram",
            "delete_diagram",
            // Doc actions
            "create_doc",
            "list_docs",
            "get_doc",
            "update_doc",
            "delete_doc",
            "create_roadmap",
            // Transcript actions
            "list_transcripts",
            "get_transcript",
            "search_transcripts",
            "search_archive",
            "delete_transcript",
            // Team actions
            "team_tasks",
            "team_todos",
            "team_diagrams",
            "team_docs",
            "team_discussions",
            "team_transcript_topics",
        ];

        SchemaBuilder::new()
            .description("Memory operations")
            .string_enum("action", "Operation to perform", all_actions, true)
            // Common fields
            .string(
                "query",
                "Search query (for search/decisions/list_docs/search_transcripts)",
                false,
            )
            .string(
                "scope",
                "Todo scope for list_todos (all, personal, team) or the decision scope for create_decision (free text)",
                false,
            )
            .uuid(
                "workspace_id",
                "Workspace ID. REQUIRED for every task action; reuse the exact workspace_id returned by init/context. For other actions, pass it whenever workspace scope is available.",
                false,
            )
            .uuid(
                "project_id",
                "Project ID. For project-scoped memory writes and lookups, pass the current project_id returned by init/context instead of guessing.",
                false,
            )
            .string(
                "target_project",
                "Target child project by folder name or project name (e.g. 'contextstream', 'mcp-server'). Use this only after init from a multi-project parent folder.",
                false,
            )
            .integer("limit", "Maximum results", false)
            // Node fields
            .string_enum("node_type", "Node type", VALID_NODE_TYPES, false)
            .string(
                "node_id",
                "Node ID or lookup text (title/summary) for node operations",
                false,
            )
            .boolean(
                "delete_all",
                "For delete_node/delete_event: delete ALL exact-title matches of node_id/event_id in one call (bulk-remove duplicates) instead of erroring on ambiguity.",
                false,
            )
            .string("title", "Title (for create/update operations)", false)
            .string("content", "Content (for create/update operations)", false)
            .string(
                "new_content",
                "New content (for supersede_node; also accepted as the body for update_node)",
                false,
            )
            .string("reason", "Reason (for supersede_node, decision_action)", false)
            // Decision fields
            .string(
                "category",
                "Decision category filter (decisions) or category to store (create_decision)",
                false,
            )
            .string_enum(
                "sort",
                "Decision ordering (decisions): recency or relevance",
                DECISION_SORTS,
                false,
            )
            .string_enum(
                "status",
                "Decision status filter (decisions): active (default), superseded, disputed, verified, or all",
                DECISION_STATUSES,
                false,
            )
            .string(
                "since",
                "ISO-8601 lower bound on decision time (decisions)",
                false,
            )
            .integer("offset", "Pagination offset (decisions)", false)
            .string("source", "Decision source filter (decisions)", false)
            .string(
                "rationale",
                "Why this decision was made (create_decision)",
                false,
            )
            .property(
                "alternatives",
                serde_json::json!({
                    "type": "array",
                    "description": "Alternatives considered (create_decision): strings or {option, rejected_reason} objects",
                    "items": {"anyOf": [{"type": "string"}, {"type": "object"}]}
                }),
                false,
            )
            .number(
                "confidence",
                "Confidence in the decision, 0.0-1.0 (create_decision)",
                false,
            )
            .string(
                "supersedes",
                "Decision id or lookup text this decision replaces (create_decision)",
                false,
            )
            .string(
                "decision_id",
                "Decision id or lookup text (decision_action)",
                false,
            )
            .string_enum(
                "decision_action",
                "Lifecycle action to apply (decision_action)",
                mcp_client::DECISION_ACTIONS,
                false,
            )
            .string(
                "successor_id",
                "Successor decision id or lookup text (decision_action=supersede|choose_successor)",
                false,
            )
            // Event fields
            .string_enum(
                "event_type",
                "Event type. Do not use this for plans; use session(action=\"capture_plan\") instead.",
                VALID_EVENT_TYPES,
                false,
            )
            .string(
                "event_id",
                "Event ID or lookup text (title/content) for event operations",
                false,
            )
            .array(
                "events",
                "Array of events (for import_batch)",
                "object",
                false,
            )
            // Task fields
            .string(
                "task_id",
                "Task ID or lookup text (title/description) for task operations",
                false,
            )
            .string(
                "description",
                "Description (for create_task/update_task only). For plan tasks, include concrete work, acceptance criteria, and verification. For todos, use 'content' instead.",
                false,
            )
            .string("priority", "Priority (for task operations)", false)
            .string_enum("task_status", "Task status", VALID_TASK_STATUSES, false)
            .uuid(
                "plan_id",
                "Plan ID (required when creating a task that belongs to a plan)",
                false,
            )
            .string(
                "plan_step_id",
                "Plan step ID (required when the task implements a specific plan step)",
                false,
            )
            .array("tags", "Tags (for create_task)", "string", false)
            .string("blocked_reason", "Blocked reason (for update_task)", false)
            .array(
                "code_refs",
                "Code references (for update_task): [{file_path, symbol_name?, line_range?}]",
                "object",
                false,
            )
            .integer("order", "Sort order (for create_task/update_task)", false)
            .array(
                "task_ids",
                "Task IDs array (for reorder_tasks)",
                "string",
                false,
            )
            // Todo fields
            .string(
                "todo_id",
                "Todo ID or lookup text (title/content) for todo operations",
                false,
            )
            .string_enum(
                "todo_priority",
                "Todo priority",
                VALID_TODO_PRIORITIES,
                false,
            )
            .string("todo_status", "Todo status: pending or completed", false)
            .string("due_at", "Due date (ISO 8601)", false)
            .boolean("clear_due_at", "Clear the todo due date on update_todo", false)
            .string("created_after", "Created-at lower bound (ISO 8601)", false)
            .string("created_before", "Created-at upper bound (ISO 8601)", false)
            .string("updated_after", "Updated-at lower bound (ISO 8601)", false)
            .string("updated_before", "Updated-at upper bound (ISO 8601)", false)
            .string("due_after", "Due-at lower bound (ISO 8601)", false)
            .string("due_before", "Due-at upper bound (ISO 8601)", false)
            .string("completed_after", "Completed-at lower bound (ISO 8601)", false)
            .string("completed_before", "Completed-at upper bound (ISO 8601)", false)
            // Diagram fields
            .string(
                "diagram_id",
                "Diagram ID or lookup text (title/content) for diagram operations",
                false,
            )
            .string_enum(
                "diagram_type",
                "Diagram type (flowchart, sequence, class, er, gantt, mindmap, pie, other)",
                VALID_DIAGRAM_TYPES,
                false,
            )
            // Doc fields
            .string("doc_id", "Doc ID or title/query (for get_doc)", false)
            .string_enum("doc_type", "Doc type", VALID_DOC_TYPES, false)
            .array(
                "milestones",
                "Milestones (for create_roadmap)",
                "object",
                false,
            )
            // Transcript fields
            .uuid(
                "transcript_id",
                "Transcript ID (for transcript operations)",
                false,
            )
            .string("session_id", "Session ID (for list_transcripts)", false)
            .string("client_name", "Client name (for list_transcripts)", false)
            .string(
                "started_after",
                "ISO timestamp (for list_transcripts)",
                false,
            )
            .string(
                "started_before",
                "ISO timestamp (for list_transcripts)",
                false,
            )
            // Personal flag
            .boolean(
                "is_personal",
                "Mark as personal (for create/list on todos, diagrams, docs)",
                false,
            )
            .build()
    }
}

/// Thin alias over `MemoryTool` that pins a single `action` and
/// presents itself to MCP clients under a descriptive top-level
/// name. Lets opencode/Cursor/Codex/Claude Code (which render only
/// the tool name) show e.g. `contextstream_memory_update_doc`
/// instead of the generic `contextstream_memory`. All routing,
/// validation, and side effects still go through `MemoryTool`.
pub struct MemoryActionAlias {
    inner: Arc<MemoryTool>,
    metadata: ToolMetadata,
    schema: Value,
    action: &'static str,
}

impl MemoryActionAlias {
    pub fn new(
        inner: Arc<MemoryTool>,
        name: &'static str,
        title: &'static str,
        description: &'static str,
        action: &'static str,
        annotations: ToolAnnotations,
        schema: Value,
    ) -> Self {
        Self {
            inner,
            metadata: ToolMetadata {
                name: name.to_string(),
                title: title.to_string(),
                description: description.to_string(),
                category: ToolCategory::Memory,
                annotations,
                is_pro: false,
                required_tier: None,
            },
            schema,
            action,
        }
    }
}

fn memory_update_task_schema() -> Value {
    SchemaBuilder::new()
        .description(
            "Update an existing task in ContextStream memory. workspace_id is required; reuse the exact value returned by init/context.",
        )
        .uuid("task_id", "Task ID", true)
        .uuid(
            "workspace_id",
            "REQUIRED. Exact workspace ID returned by init/context for the task's workspace.",
            true,
        )
        .uuid(
            "project_id",
            "Project ID returned by init/context when project scope is available",
            false,
        )
        .string("title", "New title (optional)", false)
        .string("description", "New description (optional)", false)
        .string("priority", "Priority (low|medium|high|urgent)", false)
        .string_enum(
            "task_status",
            "New status",
            &[
                "pending",
                "in_progress",
                "completed",
                "blocked",
                "cancelled",
            ],
            false,
        )
        .string("blocked_reason", "Reason (when status=blocked)", false)
        .build()
}

#[async_trait]
impl ToolHandler for MemoryActionAlias {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let mut input = match input {
            Value::Object(map) => Value::Object(map),
            Value::Null => Value::Object(serde_json::Map::new()),
            other => {
                return Err(Error::Validation(format!(
                    "memory alias `{}` expects an object input, got {}",
                    self.action,
                    match &other {
                        Value::Array(_) => "array",
                        Value::String(_) => "string",
                        Value::Number(_) => "number",
                        Value::Bool(_) => "boolean",
                        _ => "unknown",
                    }
                )))
            }
        };
        if let Some(obj) = input.as_object_mut() {
            obj.insert("action".to_string(), Value::String(self.action.to_string()));
        }
        self.inner.execute(input).await
    }

    fn metadata(&self) -> &ToolMetadata {
        &self.metadata
    }

    fn input_schema(&self) -> Value {
        self.schema.clone()
    }
}

/// Register all memory tools.
pub fn register_memory_tools(
    registry: &mut crate::registry::ToolRegistry,
    client: ContextStreamClient,
    session: Arc<mcp_session::SessionManager>,
) {
    // Snapshot the legacy layer for compatibility routing.
    let atlas_layer = registry.atlas_layer().clone();
    let acceleration_layer = registry.acceleration_layer().clone();
    let memory = Arc::new(MemoryTool::with_acceleration(
        client.clone(),
        session.clone(),
        atlas_layer.clone(),
        acceleration_layer,
    ));
    registry.register("memory", memory.clone());
    registry.register(
        "memory_search",
        Arc::new(MemorySearchTool::new(client.clone())),
    );
    registry.register(
        "memory_create_node",
        Arc::new(CreateMemoryNodeTool::new(client.clone(), session.clone())),
    );
    registry.register(
        "memory_decisions",
        Arc::new(MemoryDecisionsTool::with_session_and_atlas(
            client.clone(),
            session,
            atlas_layer.clone(),
        )),
    );

    // Per-action aliases for the most common write operations. Each
    // shares the same `MemoryTool` and is rendered by clients with
    // its own self-describing name.
    registry.register(
        "memory_create_doc",
        Arc::new(MemoryActionAlias::new(
            memory.clone(),
            "memory_create_doc",
            "Save doc to ContextStream",
            "Save a new doc (runbook, ADR, RFC, postmortem, spec, etc.) to ContextStream memory. \
             Same as memory(action=\"create_doc\"). Use this when the user asks to save/create a \
             doc, runbook, ADR, RFC, postmortem, retro, release notes, playbook, PRD, design spec, \
             glossary entry, etc.",
            "create_doc",
            ToolAnnotations::write(),
            SchemaBuilder::new()
                .description("Save a new doc to ContextStream memory")
                .string("title", "Doc title", true)
                .string("content", "Doc content (markdown)", true)
                .string_enum(
                    "doc_type",
                    "Doc type",
                    &[
                        "roadmap",
                        "spec",
                        "runbook",
                        "adr",
                        "rfc",
                        "postmortem",
                        "retro",
                        "release_notes",
                        "playbook",
                        "prd",
                        "user_story",
                        "persona",
                        "interview",
                        "design_spec",
                        "critique",
                        "glossary",
                        "oncall_schedule",
                        "slo",
                        "q_and_a",
                        "changelog",
                        "style_guide",
                        "general",
                    ],
                    false,
                )
                .uuid("workspace_id", "Workspace ID. Reuse current.", false)
                .uuid("project_id", "Project ID. Reuse current.", false)
                .boolean("is_personal", "Mark as personal", false)
                .build(),
        )),
    );
    registry.register(
        "memory_update_doc",
        Arc::new(MemoryActionAlias::new(
            memory.clone(),
            "memory_update_doc",
            "Update doc in ContextStream",
            "Update an existing doc in ContextStream memory. Same as \
             memory(action=\"update_doc\"). Use this when the user asks to update/edit a doc, \
             runbook, ADR, RFC, postmortem, retro, etc.",
            "update_doc",
            ToolAnnotations::destructive(),
            SchemaBuilder::new()
                .description("Update an existing doc in ContextStream memory")
                .string("doc_id", "Doc ID", true)
                .string("title", "New title (optional)", false)
                .string("content", "New content (optional)", false)
                .string_enum(
                    "doc_type",
                    "New doc type (optional)",
                    &[
                        "roadmap",
                        "spec",
                        "runbook",
                        "adr",
                        "rfc",
                        "postmortem",
                        "retro",
                        "release_notes",
                        "playbook",
                        "prd",
                        "user_story",
                        "persona",
                        "interview",
                        "design_spec",
                        "critique",
                        "glossary",
                        "oncall_schedule",
                        "slo",
                        "q_and_a",
                        "changelog",
                        "style_guide",
                        "general",
                    ],
                    false,
                )
                .build(),
        )),
    );
    registry.register(
        "memory_delete_doc",
        Arc::new(MemoryActionAlias::new(
            memory.clone(),
            "memory_delete_doc",
            "Delete doc in ContextStream",
            "Delete a doc from ContextStream memory. Same as memory(action=\"delete_doc\").",
            "delete_doc",
            ToolAnnotations::destructive(),
            SchemaBuilder::new()
                .description("Delete a doc from ContextStream memory")
                .string("doc_id", "Doc ID", true)
                .build(),
        )),
    );
    registry.register(
        "memory_create_task",
        Arc::new(MemoryActionAlias::new(
            memory.clone(),
            "memory_create_task",
            "Save task to ContextStream",
            "Save a tracked task to ContextStream memory. Same as memory(action=\"create_task\"). \
             Include `plan_id` + `plan_step_id` when the task belongs to a plan.",
            "create_task",
            ToolAnnotations::write(),
            SchemaBuilder::new()
                .description("Save a tracked task to ContextStream memory")
                .string("title", "Task title", true)
                .string(
                    "description",
                    "Concrete work, acceptance criteria, verification",
                    false,
                )
                .uuid("plan_id", "Linked plan ID", false)
                .string("plan_step_id", "Linked plan step ID", false)
                .string("priority", "Priority (low|medium|high|urgent)", false)
                .string_enum(
                    "task_status",
                    "Initial status",
                    &[
                        "pending",
                        "in_progress",
                        "completed",
                        "blocked",
                        "cancelled",
                    ],
                    false,
                )
                .uuid(
                    "workspace_id",
                    "REQUIRED. Exact workspace ID returned by init/context.",
                    true,
                )
                .uuid("project_id", "Project ID. Reuse current.", false)
                .build(),
        )),
    );
    registry.register(
        "memory_update_task",
        Arc::new(MemoryActionAlias::new(
            memory.clone(),
            "memory_update_task",
            "Update task in ContextStream",
            "Update an existing task in ContextStream memory. Same as \
             memory(action=\"update_task\"). `workspace_id` is required; reuse the exact value \
             returned by init/context.",
            "update_task",
            ToolAnnotations::destructive(),
            memory_update_task_schema(),
        )),
    );
    registry.register(
        "memory_create_todo",
        Arc::new(MemoryActionAlias::new(
            memory.clone(),
            "memory_create_todo",
            "Save todo to ContextStream",
            "Save a personal/team todo to ContextStream memory. Same as \
             memory(action=\"create_todo\").",
            "create_todo",
            ToolAnnotations::write(),
            SchemaBuilder::new()
                .description("Save a todo to ContextStream memory")
                .string("title", "Todo title", true)
                .string("content", "Todo details (optional)", false)
                .string("todo_priority", "low|medium|high|urgent", false)
                .string("due_at", "Due date (ISO 8601)", false)
                .boolean("is_personal", "Mark as personal", false)
                .uuid("workspace_id", "Workspace ID. Reuse current.", false)
                .uuid("project_id", "Project ID. Reuse current.", false)
                .build(),
        )),
    );
    registry.register(
        "memory_complete_todo",
        Arc::new(MemoryActionAlias::new(
            memory.clone(),
            "memory_complete_todo",
            "Complete todo in ContextStream",
            "Mark a ContextStream todo as completed. Same as memory(action=\"complete_todo\").",
            "complete_todo",
            ToolAnnotations::write(),
            SchemaBuilder::new()
                .description("Complete a todo in ContextStream memory")
                .uuid("todo_id", "Todo ID", true)
                .build(),
        )),
    );
    registry.register(
        "memory_create_event",
        Arc::new(MemoryActionAlias::new(
            memory,
            "memory_create_event",
            "Save event to ContextStream",
            "Save a memory event (decision, insight, achievement, status_update, etc.) to \
             ContextStream. Same as memory(action=\"create_event\"). Prefer session-side tools \
             (session_capture, session_capture_lesson, capture_plan) when the event maps to one.",
            "create_event",
            ToolAnnotations::write(),
            SchemaBuilder::new()
                .description("Save a memory event to ContextStream")
                .string("title", "Event title", true)
                .string("content", "Event content", false)
                .string_enum(
                    "event_type",
                    "Event type",
                    &[
                        "decision",
                        "preference",
                        "insight",
                        "uncategorized",
                        "note",
                        "general",
                        "manual_note",
                        "implementation",
                        "operation",
                        "command_execution",
                        "file_operation",
                        "task",
                        "bug",
                        "feature",
                        "correction",
                        "lesson",
                        "warning",
                        "frustration",
                        "conversation",
                        "session_snapshot",
                        "standup",
                        "status_update",
                        "question",
                        "approval",
                        "feedback",
                        "discovery",
                        "achievement",
                    ],
                    false,
                )
                .uuid("workspace_id", "Workspace ID. Reuse current.", false)
                .uuid("project_id", "Project ID. Reuse current.", false)
                .build(),
        )),
    );
}

#[cfg(test)]
#[path = "memory_tests.rs"]
mod tests;
