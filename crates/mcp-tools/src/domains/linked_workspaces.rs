//! Shared knowledge: knowledge from the workspaces linked to the one an agent works in
//! (contextstream plan 483f30df).
//!
//! When workspaces are linked, the API also searches the linked workspaces the person can open.
//! It labels each linked hit with its workspace (`workspace_name`, `workspace_id` and
//! `linked: true`, on the item or in its `metadata`) and reports what it read in
//! `linked_workspaces`. These helpers show both, so an agent always knows which hits came from
//! another workspace, that they are read-only here, and how to search one of them fully.

use serde_json::Value;

/// The field of `item` (or of its `metadata`) named `key`.
fn field<'a>(item: &'a Value, key: &str) -> Option<&'a Value> {
    item.get(key)
        .or_else(|| item.get("metadata").and_then(|metadata| metadata.get(key)))
}

/// Whether the API marked `item` as read from a linked workspace.
pub(crate) fn is_linked(item: &Value) -> bool {
    field(item, "linked").and_then(Value::as_bool).unwrap_or(false)
}

/// The linked workspace `item` came from, by name, when it came from one.
pub(crate) fn linked_workspace_name(item: &Value) -> Option<&str> {
    if !is_linked(item) {
        return None;
    }
    field(item, "workspace_name")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|name| !name.is_empty())
}

/// The linked workspace `item` came from, by id, when it came from one.
pub(crate) fn linked_workspace_id(item: &Value) -> Option<&str> {
    if !is_linked(item) {
        return None;
    }
    field(item, "workspace_id").and_then(Value::as_str)
}

/// `label`, followed by where a linked hit came from: `decision · from Product`.
pub(crate) fn label_with_workspace(label: &str, item: &Value) -> String {
    match linked_workspace_name(item) {
        Some(name) => format!("{label} · from {}", one_line(name)),
        None => label.to_string(),
    }
}

/// The `linked_workspaces` report of an API response (at its top or under `data`).
pub(crate) fn linked_workspaces(response: &Value) -> Vec<Value> {
    response
        .get("linked_workspaces")
        .or_else(|| response.get("data").and_then(|data| data.get("linked_workspaces")))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// One line per response that read linked workspaces: which, how many of their hits are shown,
/// which were not read, and how to search one fully. `None` when nothing linked was read.
pub(crate) fn linked_footer(response: &Value) -> Option<String> {
    let reports = linked_workspaces(response);
    if reports.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    let mut ids = Vec::new();
    for report in &reports {
        let Some(name) = report.get("name").and_then(Value::as_str) else {
            continue;
        };
        let name = one_line(name);
        let hits = report.get("hits").and_then(Value::as_u64).unwrap_or(0);
        let status = report.get("status").and_then(Value::as_str).unwrap_or("ok");
        let mut notes = Vec::new();
        if report.get("include_code").and_then(Value::as_bool) == Some(false) {
            notes.push("no code".to_string());
        }
        match status {
            "ok" => notes.push(format!("{hits} shown")),
            "timed_out" => notes.push("not read in time".to_string()),
            "unavailable" => notes.push("not readable from this region".to_string()),
            _ => notes.push("not read".to_string()),
        }
        parts.push(format!("{name} ({})", notes.join("; ")));
        if let Some(id) = report.get("workspace_id").and_then(Value::as_str) {
            ids.push(format!("{name}={id}"));
        }
    }
    if parts.is_empty() {
        return None;
    }
    let mut text = format!(
        "[LINKED_WORKSPACES] Also searched the workspaces linked to this one: {}. \
         Their hits say \"from <workspace>\": read-only here, and only what you can open there.",
        parts.join(", ")
    );
    if !ids.is_empty() {
        text.push_str(&format!(
            " To search one fully, pass its workspace_id ({}).",
            ids.join(", ")
        ));
    }
    Some(text)
}

/// The code a search found in the workspaces linked to this one (the API's `linked_results` and
/// `linked_workspaces`): the `[LINKED_WORKSPACES]` line, then one line per hit naming its
/// workspace and project. Both go into the structured output too. Nothing when the search read no
/// linked workspace.
pub(crate) fn append_linked_code(
    text: &mut String,
    structured: &mut Value,
    linked_results: &[mcp_types::api::SearchResult],
    linked_workspaces: &[Value],
) {
    let Some(footer) = linked_footer(&serde_json::json!({ "linked_workspaces": linked_workspaces }))
    else {
        return;
    };
    text.push('\n');
    text.push_str(&footer);
    text.push('\n');
    let mut hits = Vec::new();
    for hit in linked_results {
        let item = serde_json::json!({ "metadata": hit.metadata.clone().unwrap_or(Value::Null) });
        let workspace = linked_workspace_name(&item)
            .map(one_line)
            .unwrap_or_else(|| "a linked workspace".to_string());
        let project = item["metadata"]
            .get("project_name")
            .and_then(Value::as_str)
            .map(one_line)
            .filter(|name| !name.is_empty());
        let location = match (hit.file_path.as_deref(), hit.start_line) {
            (Some(path), Some(line)) => format!("{path}:{line}"),
            (Some(path), None) => path.to_string(),
            _ => hit.location.clone().unwrap_or_else(|| hit.id.clone()),
        };
        let snippet = hit
            .content
            .as_deref()
            .and_then(|content| content.lines().map(str::trim).find(|line| !line.is_empty()))
            .map(|line| line.chars().take(140).collect::<String>())
            .unwrap_or_default();
        let source = match &project {
            Some(project) => format!("from {workspace}, project {project}"),
            None => format!("from {workspace}"),
        };
        if snippet.is_empty() {
            text.push_str(&format!("  {location}  · {source}\n"));
        } else {
            text.push_str(&format!("  {location}  {snippet}  · {source}\n"));
        }
        hits.push(serde_json::json!({
            "workspace_id": linked_workspace_id(&item),
            "workspace_name": workspace,
            "project_name": project,
            "file_path": hit.file_path,
            "start_line": hit.start_line,
            "snippet": snippet,
        }));
    }
    if let Some(object) = structured.as_object_mut() {
        object.insert("linked_results".to_string(), Value::Array(hits));
        object.insert(
            "linked_workspaces".to_string(),
            Value::Array(linked_workspaces.to_vec()),
        );
    }
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_linked_hit_names_its_workspace_from_the_item_or_its_metadata() {
        let top = json!({"linked": true, "workspace_name": "Product", "workspace_id": "w-1"});
        let nested = json!({"metadata": {"linked": true, "workspace_name": "Design"}});
        let home = json!({"workspace_name": "Engineering"});
        assert_eq!(linked_workspace_name(&top), Some("Product"));
        assert_eq!(linked_workspace_id(&top), Some("w-1"));
        assert_eq!(linked_workspace_name(&nested), Some("Design"));
        assert_eq!(linked_workspace_name(&home), None, "only linked hits are labeled");
        assert_eq!(label_with_workspace("decision", &top), "decision · from Product");
        assert_eq!(label_with_workspace("decision", &home), "decision");
    }

    #[test]
    fn the_footer_names_each_linked_workspace_and_how_to_search_it() {
        let response = json!({"data": {"linked_workspaces": [
            {"workspace_id": "w-1", "name": "Product", "include_code": true, "status": "ok", "hits": 2},
            {"workspace_id": "w-2", "name": "Design", "include_code": false, "status": "timed_out", "hits": 0},
        ]}});
        let footer = linked_footer(&response).expect("footer");
        assert!(footer.starts_with("[LINKED_WORKSPACES] Also searched the workspaces linked to this one: Product (2 shown), Design (no code; not read in time)."));
        assert!(footer.ends_with("To search one fully, pass its workspace_id (Product=w-1, Design=w-2)."));
        assert!(linked_footer(&json!({"results": []})).is_none());
        assert!(linked_footer(&json!({"linked_workspaces": []})).is_none());
    }

    #[test]
    fn linked_code_lists_each_hit_with_its_workspace_after_the_footer() {
        let hit = mcp_types::api::SearchResult {
            id: "h1".into(),
            file_path: Some("src/upload.rs".into()),
            start_line: Some(42),
            content: Some("\n  pub fn handle_upload(\n".into()),
            metadata: Some(json!({
                "linked": true,
                "workspace_name": "Product",
                "workspace_id": "w-1",
                "project_name": "api",
            })),
            ..Default::default()
        };
        let reports = vec![json!({
            "workspace_id": "w-1", "name": "Product", "include_code": true, "status": "ok", "hits": 1,
        })];
        let mut text = String::from("results\n");
        let mut structured = json!({"results": []});
        append_linked_code(&mut text, &mut structured, &[hit], &reports);
        assert!(text.contains(
            "\n[LINKED_WORKSPACES] Also searched the workspaces linked to this one: Product (1 shown)."
        ));
        assert!(text.ends_with("  src/upload.rs:42  pub fn handle_upload(  · from Product, project api\n"));
        assert_eq!(structured["linked_results"][0]["workspace_id"], "w-1");
        assert_eq!(structured["linked_results"][0]["project_name"], "api");
        assert_eq!(structured["linked_workspaces"][0]["name"], "Product");

        let mut untouched = String::from("results\n");
        let mut untouched_structured = json!({});
        append_linked_code(&mut untouched, &mut untouched_structured, &[], &[]);
        assert_eq!(untouched, "results\n");
        assert_eq!(untouched_structured, json!({}));
    }
}
