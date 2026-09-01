//! Shared display-title extraction for memory/recall/grounding hits.

use serde_json::Value;

const PLACEHOLDER_TITLES: &[&str] = &["untitled", "(no title)", "no title", "unknown"];

/// True when a stored title is empty or a generic placeholder.
pub fn is_placeholder_title(raw: &str) -> bool {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return true;
    }
    PLACEHOLDER_TITLES.contains(&trimmed.to_ascii_lowercase().as_str())
}

fn extract_metadata_str<'a>(item: &'a Value, field: &str) -> Option<&'a str> {
    item.get("metadata")
        .and_then(|value| value.get(field))
        .and_then(|value| value.as_str())
}

fn take_non_placeholder_str(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if is_placeholder_title(trimmed) {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn first_line_preview(content: &str, max_chars: usize) -> String {
    let trimmed = content.trim();
    let first_line = trimmed.lines().next().unwrap_or(trimmed);
    if first_line.chars().count() > max_chars {
        let truncated: String = first_line.chars().take(max_chars).collect();
        format!("{}...", truncated)
    } else {
        first_line.to_string()
    }
}

fn first_str(item: &Value, fields: &[&str]) -> Option<String> {
    for field in fields {
        if let Some(raw) = item.get(field).and_then(|v| v.as_str()) {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
        if let Some(raw) = extract_metadata_str(item, field) {
            return Some(raw.to_string());
        }
    }
    None
}

fn type_date_label(item: &Value) -> Option<String> {
    let node_type =
        first_str(item, &["node_type", "type", "event_type", "original_type"]).unwrap_or_default();
    let created_at =
        first_str(item, &["created_at", "timestamp", "occurred_at"]).unwrap_or_default();
    if node_type.is_empty() && created_at.is_empty() {
        return None;
    }
    let type_label = if node_type.is_empty() {
        "Item".to_string()
    } else {
        let mut chars = node_type.chars();
        match chars.next() {
            Some(c) => c.to_uppercase().to_string() + &node_type[c.len_utf8()..].replace('_', " "),
            None => "Item".to_string(),
        }
    };
    let date_suffix = if created_at.len() >= 10 {
        format!(" ({})", &created_at[..10])
    } else if !created_at.is_empty() {
        format!(" ({})", created_at)
    } else {
        String::new()
    };
    Some(format!("{}{}", type_label, date_suffix))
}

fn short_id_label(item: &Value, kind: &str) -> String {
    let id = item
        .get("id")
        .and_then(|v| v.as_str())
        .or_else(|| extract_metadata_str(item, "event_id"))
        .or_else(|| extract_metadata_str(item, "transcript_id"))
        .unwrap_or("");
    let kind_label = if kind.is_empty() || kind == "hit" {
        "Memory"
    } else {
        kind
    };
    if id.len() >= 8 {
        format!("{} {}", kind_label, &id[..8])
    } else if !id.is_empty() {
        format!("{} {}", kind_label, id)
    } else {
        kind_label.to_string()
    }
}

/// Extract a human-readable title from a memory/recall search result.
pub fn extract_display_title(item: &Value) -> String {
    for field in ["title", "summary", "name"] {
        if let Some(raw) = item.get(field).and_then(|v| v.as_str()) {
            if let Some(title) = take_non_placeholder_str(raw) {
                return title;
            }
        }
    }

    for field in ["title", "summary", "name", "content_preview"] {
        if let Some(raw) = extract_metadata_str(item, field) {
            if let Some(title) = take_non_placeholder_str(raw) {
                return title;
            }
        }
    }

    for field in [
        "content_preview",
        "preview",
        "content",
        "details",
        "description",
    ] {
        if let Some(raw) = item.get(field).and_then(|v| v.as_str()) {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                return first_line_preview(trimmed, 80);
            }
        }
        if let Some(raw) = extract_metadata_str(item, field) {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                return first_line_preview(trimmed, 80);
            }
        }
    }

    if let Some(label) = type_date_label(item) {
        return label;
    }

    let kind = item
        .get("kind")
        .or_else(|| item.get("type"))
        .or_else(|| item.get("event_type"))
        .and_then(|v| v.as_str())
        .unwrap_or("hit");
    short_id_label(item, kind)
}

/// ISO date prefix (YYYY-MM-DD) from recall item metadata or top-level fields.
pub fn extract_display_date(item: &Value) -> Option<String> {
    for field in [
        "occurred_at",
        "created_at",
        "timestamp",
        "started_at",
        "updated_at",
    ] {
        if let Some(raw) = item.get(field).and_then(|v| v.as_str()) {
            if raw.len() >= 10 {
                return Some(raw[..10].to_string());
            }
        }
        if let Some(raw) = extract_metadata_str(item, field) {
            if raw.len() >= 10 {
                return Some(raw[..10].to_string());
            }
        }
    }
    None
}

/// Keywords for search/recall hints when the display title is not useful.
pub fn extract_search_keywords(item: &Value) -> String {
    let title = extract_display_title(item);
    if !is_placeholder_title(&title) && !title.starts_with("Memory ") && !title.starts_with("Item ")
    {
        return title;
    }
    for field in ["content_preview", "content", "summary", "details"] {
        if let Some(raw) = extract_metadata_str(item, field) {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                return first_line_preview(trimmed, 60);
            }
        }
        if let Some(raw) = item.get(field).and_then(|v| v.as_str()) {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                return first_line_preview(trimmed, 60);
            }
        }
    }
    title
}

/// Promote nested metadata fields to top-level for recall/grounding consumers.
pub fn normalize_recall_result_item(item: &mut Value) {
    let title = extract_display_title(item);
    let Some(obj) = item.as_object_mut() else {
        return;
    };

    obj.insert("title".to_string(), Value::String(title));

    for (meta_key, top_key) in [
        ("event_id", "event_id"),
        ("transcript_id", "transcript_id"),
        ("doc_id", "doc_id"),
        ("event_type", "event_type"),
        ("original_type", "kind"),
        ("node_type", "node_type"),
        ("content_preview", "content_preview"),
    ] {
        if obj.get(top_key).is_none() {
            if let Some(v) = obj.get("metadata").and_then(|m| m.get(meta_key)).cloned() {
                obj.insert(top_key.to_string(), v);
            }
        }
    }

    if obj.get("kind").is_none() {
        if let Some(rt) = obj.get("result_type").and_then(|v| v.as_str()) {
            obj.insert("kind".to_string(), Value::String(rt.to_ascii_lowercase()));
        }
    }

    // MemorySearchResult id is the event/node uuid; surface as event_id when missing.
    if obj.get("event_id").is_none() {
        if let Some(id) = obj.get("id").cloned() {
            obj.insert("event_id".to_string(), id);
        }
    }
}

/// Normalize all items in a session/recall JSON payload.
pub fn normalize_recall_payload(recall: &mut Value) {
    if let Some(results) = recall.get_mut("results").and_then(|r| r.as_array_mut()) {
        for item in results {
            normalize_recall_result_item(item);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn uses_metadata_title() {
        let item = json!({
            "metadata": { "title": "LLM audit cleanup", "content_preview": "remove bedrock" }
        });
        assert_eq!(extract_display_title(&item), "LLM audit cleanup");
    }

    #[test]
    fn skips_literal_untitled_and_uses_preview() {
        let item = json!({
            "title": "Untitled",
            "metadata": { "content_preview": "Implement Gemini routing" }
        });
        assert_eq!(extract_display_title(&item), "Implement Gemini routing");
    }

    #[test]
    fn normalize_promotes_metadata_event_id() {
        let mut item = json!({
            "id": "evt-uuid",
            "result_type": "Event",
            "metadata": { "title": "Decision", "event_id": "evt-uuid", "event_type": "decision" }
        });
        normalize_recall_result_item(&mut item);
        assert_eq!(item["title"], "Decision");
        assert_eq!(item["event_id"], "evt-uuid");
    }

    #[test]
    fn search_keywords_from_content_preview_when_title_junk() {
        let item = json!({
            "title": "Untitled",
            "metadata": { "content_preview": "Bedrock removal plan" }
        });
        assert_eq!(extract_search_keywords(&item), "Bedrock removal plan");
    }
}
