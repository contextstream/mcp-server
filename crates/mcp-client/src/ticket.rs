//! Ticket assignee + linked-artifact normalization and summary helpers.

use mcp_types::Error;
use serde_json::{Map, Value};
use std::collections::HashSet;

/// Canonical linked artifact kinds for tickets.
pub const LINKED_ITEM_KINDS: &[&str] = &[
    "doc", "diagram", "plan", "task", "todo", "handoff", "runbook", "capsule",
];
/// Canonical linked artifact kinds for plans.
pub const PLAN_LINKED_ITEM_KINDS: &[&str] = &["doc", "diagram", "runbook", "handoff"];

/// Assignee entity types (human now; agent reserved for future use).
pub const ASSIGNEE_ENTITY_TYPES: &[&str] = &["human", "agent"];

/// Summary of linked items on a ticket, grouped by kind.
#[derive(Debug, Clone, Default)]
pub struct TicketLinkedSummary {
    pub total: usize,
    pub by_kind: Vec<(String, usize)>,
    pub stale_count: usize,
}

/// Normalize ticket create/update body fields (`assignees`, `linked_items`).
pub fn normalize_ticket_body(body: &mut Value) -> Result<(), Error> {
    let Some(obj) = body.as_object_mut() else {
        return Ok(());
    };

    if obj.contains_key("assignees") {
        let assignees = obj.remove("assignees").unwrap_or(Value::Null);
        obj.insert("assignees".to_string(), normalize_assignees(&assignees)?);
    }

    if obj.contains_key("linked_items") {
        let linked = obj.remove("linked_items").unwrap_or(Value::Null);
        obj.insert(
            "linked_items".to_string(),
            normalize_ticket_linked_items(&linked)?,
        );
    }

    Ok(())
}

/// Canonicalize a linked-item kind string.
pub fn canonical_linked_item_kind(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "doc" | "docs" | "document" | "documents" => Some("doc"),
        "diagram" | "diagrams" => Some("diagram"),
        "plan" | "plans" => Some("plan"),
        "task" | "tasks" => Some("task"),
        "todo" | "todos" => Some("todo"),
        "handoff" | "handoffs" => Some("handoff"),
        "runbook" | "runbooks" => Some("runbook"),
        "capsule" | "capsules" => Some("capsule"),
        _ => None,
    }
}

pub fn normalize_assignees(value: &Value) -> Result<Value, Error> {
    let items: Vec<Value> = match value {
        Value::Array(arr) => arr.clone(),
        Value::Object(_) => vec![value.clone()],
        Value::Null => return Ok(Value::Array(vec![])),
        _ => {
            return Err(Error::Validation(
                "assignees must be an array of objects".to_string(),
            ))
        }
    };

    let mut out = Vec::new();
    let mut seen = HashSet::new();

    for (idx, item) in items.into_iter().enumerate() {
        let Some(obj) = item.as_object() else {
            return Err(Error::Validation(format!(
                "assignees[{}] must be an object",
                idx
            )));
        };

        let user_id = obj
            .get("user_id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let email = obj
            .get("email")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let handle = obj
            .get("handle")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        if user_id.is_none() && email.is_none() && handle.is_none() {
            return Err(Error::Validation(format!(
                "assignees[{}] requires at least one of user_id, email, or handle",
                idx
            )));
        }

        let entity_type = obj
            .get("entity_type")
            .or_else(|| obj.get("kind"))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_else(|| "human".to_string());

        if !ASSIGNEE_ENTITY_TYPES.contains(&entity_type.as_str()) {
            return Err(Error::Validation(format!(
                "assignees[{}].entity_type must be one of: {}",
                idx,
                ASSIGNEE_ENTITY_TYPES.join(", ")
            )));
        }

        let dedupe_key = user_id
            .clone()
            .or_else(|| email.clone())
            .or_else(|| handle.clone())
            .unwrap_or_default()
            + "|"
            + &entity_type;
        if !seen.insert(dedupe_key) {
            continue;
        }

        let mut normalized = Map::new();
        normalized.insert("entity_type".to_string(), Value::String(entity_type));
        if let Some(v) = user_id {
            normalized.insert("user_id".to_string(), Value::String(v));
        }
        if let Some(v) = email {
            normalized.insert("email".to_string(), Value::String(v));
        }
        if let Some(v) = handle {
            normalized.insert("handle".to_string(), Value::String(v));
        }
        if let Some(role) = obj.get("role").and_then(|v| v.as_str()) {
            let role = role.trim();
            if !role.is_empty() {
                normalized.insert("role".to_string(), Value::String(role.to_string()));
            }
        }
        // Preserve extra metadata for future agent evolution.
        for (key, val) in obj {
            if matches!(
                key.as_str(),
                "user_id" | "email" | "handle" | "role" | "entity_type" | "kind"
            ) {
                continue;
            }
            normalized.insert(key.clone(), val.clone());
        }
        out.push(Value::Object(normalized));
    }

    Ok(Value::Array(out))
}

pub fn normalize_ticket_linked_items(value: &Value) -> Result<Value, Error> {
    normalize_linked_items_with_allowed_kinds(value, LINKED_ITEM_KINDS)
}

/// Normalize linked_items using a caller-provided allowed kind list.
pub fn normalize_linked_items_with_allowed_kinds(
    value: &Value,
    allowed_kinds: &[&str],
) -> Result<Value, Error> {
    let items: Vec<Value> = match value {
        Value::Array(arr) => arr.clone(),
        Value::Object(_) => vec![value.clone()],
        Value::Null => return Ok(Value::Array(vec![])),
        _ => {
            return Err(Error::Validation(
                "linked_items must be an array of objects".to_string(),
            ))
        }
    };

    let mut out = Vec::new();
    let mut seen = HashSet::new();

    for (idx, item) in items.into_iter().enumerate() {
        let Some(obj) = item.as_object() else {
            return Err(Error::Validation(format!(
                "linked_items[{}] must be an object",
                idx
            )));
        };

        let kind_raw = obj
            .get("kind")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Validation(format!("linked_items[{}].kind is required", idx)))?;
        let kind = canonical_linked_item_kind(kind_raw)
            .filter(|k| allowed_kinds.contains(k))
            .ok_or_else(|| {
                Error::Validation(format!(
                    "linked_items[{}].kind '{}' is invalid; use one of: {}",
                    idx,
                    kind_raw,
                    allowed_kinds.join(", ")
                ))
            })?;

        let id = obj
            .get("id")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| Error::Validation(format!("linked_items[{}].id is required", idx)))?;

        let dedupe_key = format!("{}:{}", kind, id);
        if !seen.insert(dedupe_key) {
            continue;
        }

        let mut normalized = Map::new();
        normalized.insert("kind".to_string(), Value::String(kind.to_string()));
        normalized.insert("id".to_string(), Value::String(id.to_string()));

        for field in ["title_snapshot", "status_snapshot", "updated_at"] {
            if let Some(val) = obj.get(field).and_then(|v| v.as_str()) {
                let trimmed = val.trim();
                if !trimmed.is_empty() {
                    normalized.insert(field.to_string(), Value::String(trimmed.to_string()));
                }
            }
        }

        for (key, val) in obj {
            if matches!(
                key.as_str(),
                "kind" | "id" | "title_snapshot" | "status_snapshot" | "updated_at"
            ) {
                continue;
            }
            normalized.insert(key.clone(), val.clone());
        }
        out.push(Value::Object(normalized));
    }

    Ok(Value::Array(out))
}

/// Ticket fields the MCP accepts that the API may not store. The API ignores
/// fields it does not know, so a field the request set and the reply lacks
/// was dropped.
const TICKET_FIELDS_CHECKED_AGAINST_REPLY: &[&str] = &["assignees", "linked_items"];

/// The ticket fields this request set that the API's reply does not carry,
/// meaning the API did not save them. The reply is never filled in from the
/// request, so a summary only shows what was stored.
pub fn ticket_fields_not_saved(request_body: &Value, result: &Value) -> Vec<&'static str> {
    let Some(req_obj) = request_body.as_object() else {
        return Vec::new();
    };
    let entity_obj = find_ticket_entity_object(result);
    TICKET_FIELDS_CHECKED_AGAINST_REPLY
        .iter()
        .copied()
        .filter(|field| has_items(req_obj.get(*field)))
        .filter(|field| !has_items(entity_obj.and_then(|obj| obj.get(*field))))
        .collect()
}

/// What to tell the agent about ticket fields the API did not save.
pub fn format_ticket_fields_not_saved(fields: &[&str]) -> Option<String> {
    let lines: Vec<&str> = fields
        .iter()
        .filter_map(|field| match *field {
            "linked_items" => Some(
                "Not saved: linked_items. The API does not store ticket links yet; \
                 name the linked items in the description instead.",
            ),
            "assignees" => Some(
                "Not saved: assignees. The API keeps a single assignee as assignee_id \
                 (a user id); set assignee_id instead.",
            ),
            _ => None,
        })
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

fn has_items(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Array(items)) => !items.is_empty(),
        Some(_) => true,
    }
}

fn find_ticket_entity_object(result: &Value) -> Option<&Map<String, Value>> {
    let obj = result.as_object()?;
    for key in ["ticket", "data", "entity", "item", "result"] {
        if let Some(inner) = obj.get(key).and_then(Value::as_object) {
            return Some(inner);
        }
    }
    Some(obj)
}

/// Human-readable assignee summary for ticket list/get text output.
pub fn format_ticket_assignee_summary(item: &Value) -> Option<String> {
    let assignees = item.get("assignees")?.as_array()?;
    if assignees.is_empty() {
        return None;
    }

    let labels: Vec<String> = assignees
        .iter()
        .filter_map(|a| {
            let obj = a.as_object()?;
            let entity_type = obj
                .get("entity_type")
                .or_else(|| obj.get("kind"))
                .and_then(|v| v.as_str())
                .unwrap_or("human");
            let label = obj
                .get("email")
                .or_else(|| obj.get("handle"))
                .or_else(|| obj.get("user_id"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            let role = obj
                .get("role")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(|r| format!(" ({})", r))
                .unwrap_or_default();
            Some(format!(
                "{}{}{}",
                label,
                role,
                if entity_type == "agent" {
                    " [agent]"
                } else {
                    ""
                }
            ))
        })
        .collect();

    if labels.is_empty() {
        None
    } else {
        Some(format!("assignees: {}", labels.join(", ")))
    }
}

/// Summarize linked items grouped by kind.
pub fn summarize_ticket_linked_items(item: &Value) -> TicketLinkedSummary {
    let mut summary = TicketLinkedSummary::default();
    let Some(items) = item.get("linked_items").and_then(|v| v.as_array()) else {
        return summary;
    };

    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for link in items {
        let Some(obj) = link.as_object() else {
            continue;
        };
        let kind = obj
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        *counts.entry(kind).or_insert(0) += 1;
        summary.total += 1;

        // Stale hint: missing title_snapshot or updated_at.
        let has_title = obj
            .get("title_snapshot")
            .and_then(|v| v.as_str())
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        let has_updated = obj
            .get("updated_at")
            .and_then(|v| v.as_str())
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        if !has_title || !has_updated {
            summary.stale_count += 1;
        }
    }

    summary.by_kind = counts.into_iter().collect();
    summary
}

/// Human-readable linked-item summary for list/get text output.
pub fn format_linked_summary(item: &Value) -> Option<String> {
    let summary = summarize_ticket_linked_items(item);
    if summary.total == 0 {
        return None;
    }

    let parts: Vec<String> = summary
        .by_kind
        .iter()
        .map(|(kind, count)| format!("{}={}", kind, count))
        .collect();
    let mut text = format!("linked: {} ({})", summary.total, parts.join(", "));
    if summary.stale_count > 0 {
        text.push_str(&format!(
            "; {} snapshot(s) may be stale — refresh title_snapshot/updated_at",
            summary.stale_count
        ));
    }
    Some(text)
}

/// Human-readable linked-item summary for ticket list/get text output.
pub fn format_ticket_linked_summary(item: &Value) -> Option<String> {
    format_linked_summary(item)
}

/// Append ticket-specific extras (assignees, linked items) to a summary line.
pub fn append_ticket_extras(base: &str, item: &Value) -> String {
    let mut out = base.to_string();
    if let Some(assignees) = format_ticket_assignee_summary(item) {
        if !out.is_empty() {
            out.push_str(" | ");
        }
        out.push_str(&assignees);
    }
    if let Some(linked) = format_linked_summary(item) {
        if !out.is_empty() {
            out.push_str(" | ");
        }
        out.push_str(&linked);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalize_assignees_hybrid_dedupes_and_defaults_human() {
        let mut body = json!({
            "title": "Fix bug",
            "assignees": [
                {"email": "alice@example.com", "role": "owner"},
                {"email": "alice@example.com"},
                {"user_id": "11111111-1111-1111-1111-111111111111", "entity_type": "agent", "handle": "bot-1"}
            ]
        });
        normalize_ticket_body(&mut body).unwrap();
        let assignees = body["assignees"].as_array().unwrap();
        assert_eq!(assignees.len(), 2);
        assert_eq!(assignees[0]["entity_type"], "human");
        assert_eq!(assignees[0]["email"], "alice@example.com");
        assert_eq!(assignees[0]["role"], "owner");
        assert_eq!(assignees[1]["entity_type"], "agent");
    }

    #[test]
    fn normalize_linked_items_canonicalizes_and_dedupes() {
        let mut body = json!({
            "linked_items": [
                {"kind": "docs", "id": "abc", "title_snapshot": "Runbook"},
                {"kind": "doc", "id": "abc"},
                {"kind": "plan", "id": "plan-1", "status_snapshot": "active", "updated_at": "2026-05-19T00:00:00Z"}
            ]
        });
        normalize_ticket_body(&mut body).unwrap();
        let links = body["linked_items"].as_array().unwrap();
        assert_eq!(links.len(), 2);
        assert_eq!(links[0]["kind"], "doc");
        assert_eq!(links[1]["kind"], "plan");
    }

    #[test]
    fn normalize_linked_items_accepts_diagram_for_tickets() {
        let mut body = json!({
            "linked_items": [
                {"kind": "diagrams", "id": "dia-1", "title_snapshot": "Arch"}
            ]
        });
        normalize_ticket_body(&mut body).unwrap();
        let links = body["linked_items"].as_array().unwrap();
        assert_eq!(links[0]["kind"], "diagram");
    }

    #[test]
    fn normalize_linked_items_restricts_to_allowed_kinds() {
        let linked = json!([
            {"kind": "plan", "id": "plan-1"}
        ]);
        let err = normalize_linked_items_with_allowed_kinds(&linked, PLAN_LINKED_ITEM_KINDS)
            .unwrap_err()
            .to_string();
        assert!(err.contains("invalid"));
        assert!(err.contains("diagram"));
    }

    #[test]
    fn linked_summary_counts_by_kind_and_stale() {
        let item = json!({
            "linked_items": [
                {"kind": "doc", "id": "d1", "title_snapshot": "ADR"},
                {"kind": "task", "id": "t1"}
            ]
        });
        let summary = summarize_ticket_linked_items(&item);
        assert_eq!(summary.total, 2);
        assert_eq!(summary.stale_count, 2);
        let text = format_ticket_linked_summary(&item).unwrap();
        assert!(text.contains("linked: 2"));
        assert!(text.contains("doc=1"));
        assert!(text.contains("stale"));
    }

    #[test]
    fn assignee_summary_includes_agent_marker() {
        let item = json!({
            "assignees": [
                {"email": "alice@example.com"},
                {"handle": "review-bot", "entity_type": "agent"}
            ]
        });
        let text = format_ticket_assignee_summary(&item).unwrap();
        assert!(text.contains("alice@example.com"));
        assert!(text.contains("[agent]"));
    }

    #[test]
    fn fields_the_api_dropped_are_reported_not_echoed() {
        let request = json!({
            "title": "Fix bug",
            "assignees": [{"email": "alice@example.com"}],
            "linked_items": [{"kind": "doc", "id": "doc-1", "title_snapshot": "ADR"}]
        });
        let result = json!({
            "ticket": {
                "id": "t-1",
                "title": "Fix bug",
                "status": "open"
            }
        });
        assert_eq!(
            ticket_fields_not_saved(&request, &result),
            vec!["assignees", "linked_items"]
        );
        let text = format_ticket_fields_not_saved(&["assignees", "linked_items"]).unwrap();
        assert!(text.contains("Not saved: linked_items"));
        assert!(text.contains("Not saved: assignees"));
        assert!(format_linked_summary(&result["ticket"]).is_none());
    }

    #[test]
    fn fields_the_api_returned_count_as_saved() {
        let request = json!({
            "assignees": [{"email": "alice@example.com"}],
            "linked_items": [{"kind": "doc", "id": "doc-1"}]
        });
        let result = json!({
            "id": "t-2",
            "assignees": [{"email": "bob@example.com"}],
            "linked_items": [{"kind": "plan", "id": "plan-1"}]
        });
        assert!(ticket_fields_not_saved(&request, &result).is_empty());
        assert!(format_ticket_fields_not_saved(&[]).is_none());
    }

    #[test]
    fn fields_the_request_left_out_or_cleared_are_not_reported() {
        let request = json!({"title": "Fix bug", "linked_items": []});
        let result = json!({"id": "t-3", "title": "Fix bug"});
        assert!(ticket_fields_not_saved(&request, &result).is_empty());
    }
}
