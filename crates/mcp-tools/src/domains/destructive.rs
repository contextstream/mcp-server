//! Irreversible actions, in a tool of their own (#330).
//!
//! Directory reviews (the Claude connector directory, ChatGPT's action
//! control) read `destructiveHint` per tool and ask the person to confirm a
//! destructive one. `memory`, `project` and most other multi-action tools had
//! a delete, purge or revoke among their actions, so all of them claimed to be
//! destructive. Those actions are now listed in one tool, [`DELETE_TOOL`]
//! (`destructiveHint: true`, id-only parameters), as `<tool>.<action>`. The
//! tools they came from list only their other actions and claim
//! `destructiveHint: false`: what they do is read, create or edit something
//! that still exists afterwards. An edit of a record's fields is not counted
//! as irreversible; replacing a document's text is (`memory.update_doc`, listed
//! as the existing `memory_update_doc`).
//!
//! Nothing stops working: a call that still names a moved action runs as
//! before, with a note naming where it is listed now. Router-mode and
//! `execute_operation` catalogs keep every action, since `execute_operation`
//! is itself destructive.

use std::sync::Arc;

use async_trait::async_trait;
use mcp_types::{
    tool::{ContentItem, ToolAnnotations, ToolCategory, ToolMetadata, ToolResult},
    Error, Result,
};
use serde_json::{json, Map, Value};

use crate::registry::{ToolHandler, ToolRegistry};

/// The tool that lists every irreversible action.
pub const DELETE_TOOL: &str = "delete";

/// One irreversible action: the tool it belongs to, its name there, and the
/// parameters it takes.
pub struct SeparatedAction {
    pub tool: &'static str,
    pub action: &'static str,
    pub params: &'static [&'static str],
    /// Listed in [`DELETE_TOOL`] when `None`; otherwise the dedicated tool that
    /// already existed for it.
    pub listed_in: Option<&'static str>,
}

const fn removal(
    tool: &'static str,
    action: &'static str,
    params: &'static [&'static str],
) -> SeparatedAction {
    SeparatedAction {
        tool,
        action,
        params,
        listed_in: None,
    }
}

/// Every irreversible action: it removes something (delete, purge, merge a
/// project away, remove indexed paths), withdraws something others rely on
/// (revoke or unshare a share, archive a feed, unregister a webhook, clear
/// pushed instructions) or replaces a document's text.
pub const SEPARATED_ACTIONS: &[SeparatedAction] = &[
    removal(
        "memory",
        "delete_node",
        &["node_id", "workspace_id", "project_id"],
    ),
    removal(
        "memory",
        "delete_event",
        &["event_id", "workspace_id", "project_id"],
    ),
    removal(
        "memory",
        "delete_task",
        &["task_id", "workspace_id", "project_id"],
    ),
    removal(
        "memory",
        "delete_todo",
        &["todo_id", "workspace_id", "project_id"],
    ),
    removal(
        "memory",
        "delete_diagram",
        &["diagram_id", "workspace_id", "project_id"],
    ),
    removal(
        "memory",
        "delete_doc",
        &["doc_id", "workspace_id", "project_id"],
    ),
    removal(
        "memory",
        "delete_transcript",
        &["transcript_id", "workspace_id"],
    ),
    SeparatedAction {
        tool: "memory",
        action: "update_doc",
        params: &[],
        listed_in: Some("memory_update_doc"),
    },
    removal(
        "session",
        "delete_lesson",
        &["lesson_id", "workspace_id", "project_id"],
    ),
    removal("qa", "delete_kb", &["id", "workspace_id", "project_id"]),
    removal(
        "skill",
        "delete",
        &["skill_id", "name", "workspace_id", "project_id"],
    ),
    removal(
        "entity",
        "delete",
        &["kind", "id", "workspace_id", "project_id"],
    ),
    removal(
        "media",
        "delete",
        &["content_id", "workspace_id", "project_id"],
    ),
    removal(
        "capsule",
        "delete",
        &["capsule_id", "workspace_id", "project_id"],
    ),
    removal("capsule", "schedule_delete", &["schedule_id"]),
    removal("capsule", "revoke_share", &["share_id"]),
    removal(
        "feed",
        "archive",
        &["feed_id", "expected_revision", "workspace_id", "project_id"],
    ),
    removal("feed", "unshare", &["share_id", "feed_id"]),
    removal(
        "instruct",
        "clear",
        &["session_id", "workspace_id", "project_id"],
    ),
    removal("project", "delete", &["project_id", "workspace_id"]),
    removal("project", "purge", &["project_id", "workspace_id"]),
    removal(
        "project",
        "remove_paths",
        &["project_id", "paths", "workspace_id"],
    ),
    removal(
        "project",
        "merge",
        &["project_id", "source_project_id", "workspace_id"],
    ),
    removal(
        "vcs",
        "delete_repo",
        &["provider", "owner", "repo", "repo_ref", "workspace_id"],
    ),
    removal(
        "vcs",
        "unregister_webhook",
        &["provider", "owner", "repo", "repo_ref", "workspace_id"],
    ),
    removal("vcs", "delete_link", &["link_id", "workspace_id"]),
    removal(
        "vcs",
        "delete_automation",
        &["automation_id", "workspace_id"],
    ),
    removal("workspace", "delete", &["workspace_id"]),
];

/// Tools that claimed `destructiveHint` without an irreversible action: their
/// writes (dismiss a notice or reminder, change a setting, edit a task's
/// fields, edit a page in a connected app that keeps its history) leave the
/// thing in place.
pub const NO_LONGER_DESTRUCTIVE: &[&str] = &[
    "coordination",
    "integration",
    "reminder",
    "memory_update_task",
];

/// The irreversible action `action` of `tool`, when it is one.
pub fn separated(tool: &str, action: &str) -> Option<&'static SeparatedAction> {
    SEPARATED_ACTIONS
        .iter()
        .find(|moved| moved.tool == tool && moved.action == action)
}

/// Whether `tool` lost its irreversible actions to [`DELETE_TOOL`] (or kept
/// none), and so is listed as not destructive.
pub fn is_split_tool(tool: &str) -> bool {
    NO_LONGER_DESTRUCTIVE.contains(&tool)
        || SEPARATED_ACTIONS.iter().any(|moved| moved.tool == tool)
}

/// The listed schema and annotations of `tool` after the split: its action
/// enum without the irreversible actions, and `destructiveHint: false`.
pub fn split_listing(tool: &str, schema: &mut Value, annotations: &mut ToolAnnotations) {
    if !is_split_tool(tool) {
        return;
    }
    annotations.destructive = false;
    annotations.read_only = false;
    if let Some(Value::Array(actions)) = schema.pointer_mut("/properties/action/enum") {
        actions.retain(|action| {
            action
                .as_str()
                .is_none_or(|action| separated(tool, action).is_none())
        });
    }
}

/// The note for a call that still names a moved action.
pub fn moved_note(tool: &str, input: &Value) -> Option<String> {
    let action = input.get("action").and_then(Value::as_str)?;
    let moved = separated(tool, action)?;
    Some(match moved.listed_in {
        Some(listed_in) => format!(
            "Note: {tool}(action=\"{action}\") is listed as `{listed_in}` now; this call still works."
        ),
        None => format!(
            "Note: {tool}(action=\"{action}\") is listed in `{DELETE_TOOL}` now (action=\"{tool}.{action}\"); this call still works."
        ),
    })
}

/// Register [`DELETE_TOOL`] with the handlers of the tools it acts through,
/// when any of them is registered.
pub fn register_delete_tool(registry: &mut ToolRegistry) {
    let mut actions = Vec::new();
    let mut properties = Map::new();
    for moved in SEPARATED_ACTIONS
        .iter()
        .filter(|moved| moved.listed_in.is_none())
    {
        let Some(tool) = registry.get(moved.tool) else {
            continue;
        };
        let tool_properties = tool.input_schema.get("properties").cloned();
        for param in moved.params {
            if properties.contains_key(*param) {
                continue;
            }
            if let Some(mut schema) = tool_properties
                .as_ref()
                .and_then(|props| props.get(*param))
                .cloned()
            {
                crate::domains::read_only::strip_descriptions(&mut schema);
                properties.insert((*param).to_string(), schema);
            }
        }
        actions.push((moved, tool.handler.clone()));
    }
    if actions.is_empty() {
        return;
    }
    let names: Vec<String> = actions
        .iter()
        .map(|(moved, _)| format!("{}.{}", moved.tool, moved.action))
        .collect();
    let mut schema_properties = Map::new();
    schema_properties.insert(
        "action".to_string(),
        json!({
            "type": "string",
            "enum": names,
            "description": ACTION_GUIDE,
        }),
    );
    schema_properties.extend(properties);
    let schema = json!({
        "type": "object",
        "properties": schema_properties,
        "required": ["action"],
    });
    registry.register(
        DELETE_TOOL,
        Arc::new(DeleteTool {
            actions,
            schema,
            metadata: ToolMetadata {
                name: DELETE_TOOL.to_string(),
                title: "Delete, Purge or Revoke".to_string(),
                description: DESCRIPTION.to_string(),
                category: ToolCategory::Utility,
                annotations: ToolAnnotations::destructive(),
                is_pro: false,
                required_tier: None,
            },
        }),
    );
}

const DESCRIPTION: &str = "Delete, purge or revoke ContextStream data. Cannot be undone, so \
    confirm with the person first. Every other ContextStream tool only reads, creates or edits; \
    name the item's tool and action here, e.g. action=\"memory.delete_doc\" with doc_id.";

const ACTION_GUIDE: &str = "<tool>.<action> to run. Ids by action: memory.delete_* take the \
    item's id (node_id, event_id, task_id, todo_id, diagram_id, doc_id, transcript_id); \
    session.delete_lesson lesson_id; qa.delete_kb id; entity.delete kind and id; skill.delete \
    skill_id or name; media.delete content_id; capsule.delete capsule_id; capsule.schedule_delete \
    schedule_id; capsule.revoke_share and feed.unshare share_id; feed.archive feed_id; \
    instruct.clear session_id; project.* project_id (remove_paths also paths, merge also \
    source_project_id); vcs.delete_repo and vcs.unregister_webhook owner and repo; \
    vcs.delete_link link_id; vcs.delete_automation automation_id; workspace.delete workspace_id.";

struct DeleteTool {
    actions: Vec<(&'static SeparatedAction, Arc<dyn ToolHandler>)>,
    schema: Value,
    metadata: ToolMetadata,
}

#[async_trait]
impl ToolHandler for DeleteTool {
    async fn execute(&self, mut input: Value) -> Result<ToolResult> {
        let requested = input
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let Some((moved, handler)) = self
            .actions
            .iter()
            .find(|(moved, _)| requested == format!("{}.{}", moved.tool, moved.action))
        else {
            let known: Vec<String> = self
                .actions
                .iter()
                .map(|(moved, _)| format!("{}.{}", moved.tool, moved.action))
                .collect();
            return Err(Error::Validation(format!(
                "`{DELETE_TOOL}` needs action=\"<tool>.<action>\", one of: {}.",
                known.join(", ")
            )));
        };
        if let Some(object) = input.as_object_mut() {
            object.insert("action".to_string(), json!(moved.action));
        }
        handler.execute(input).await
    }

    fn metadata(&self) -> &ToolMetadata {
        &self.metadata
    }

    fn input_schema(&self) -> Value {
        self.schema.clone()
    }
}

/// Add [`moved_note`] to a result.
pub fn add_moved_note(result: &mut ToolResult, note: String) {
    result.content.push(ContentItem::text(note));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_separated_action_is_named_once() {
        let mut seen = std::collections::HashSet::new();
        for moved in SEPARATED_ACTIONS {
            assert!(
                seen.insert((moved.tool, moved.action)),
                "{}.{}",
                moved.tool,
                moved.action
            );
            if moved.listed_in.is_none() {
                assert!(
                    !moved.params.is_empty(),
                    "{}.{} takes no id",
                    moved.tool,
                    moved.action
                );
            }
        }
    }

    #[test]
    fn a_split_tool_lists_only_its_other_actions_and_is_not_destructive() {
        let mut schema = json!({"type": "object", "properties": {"action": {
            "type": "string",
            "enum": ["list", "get", "create", "delete", "purge", "remove_paths", "forget_local"]
        }}});
        let mut annotations = ToolAnnotations::destructive();
        split_listing("project", &mut schema, &mut annotations);
        assert_eq!(
            schema["properties"]["action"]["enum"],
            json!(["list", "get", "create", "forget_local"])
        );
        assert!(!annotations.destructive);
        assert!(!annotations.read_only);

        // A tool with no irreversible action keeps what it had.
        let mut search = ToolAnnotations::read_only();
        let mut schema = json!({"type": "object"});
        split_listing("search", &mut schema, &mut search);
        assert!(search.read_only);
    }

    struct Recording {
        metadata: ToolMetadata,
        calls: std::sync::Mutex<Vec<Value>>,
    }

    #[async_trait]
    impl ToolHandler for Recording {
        async fn execute(&self, input: Value) -> Result<ToolResult> {
            self.calls.lock().unwrap().push(input);
            Ok(ToolResult::text("deleted"))
        }
        fn metadata(&self) -> &ToolMetadata {
            &self.metadata
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }
    }

    #[tokio::test]
    async fn delete_runs_the_action_through_its_own_tool() {
        let workspace = Arc::new(Recording {
            metadata: ToolMetadata {
                name: "workspace".to_string(),
                title: "Workspace".to_string(),
                description: "test".to_string(),
                category: ToolCategory::Workspace,
                annotations: ToolAnnotations::destructive(),
                is_pro: false,
                required_tier: None,
            },
            calls: std::sync::Mutex::new(Vec::new()),
        });
        let tool = DeleteTool {
            actions: vec![(separated("workspace", "delete").unwrap(), workspace.clone())],
            schema: json!({}),
            metadata: ToolMetadata {
                name: DELETE_TOOL.to_string(),
                title: "Delete".to_string(),
                description: DESCRIPTION.to_string(),
                category: ToolCategory::Utility,
                annotations: ToolAnnotations::destructive(),
                is_pro: false,
                required_tier: None,
            },
        };
        let result = tool
            .execute(json!({"action": "workspace.delete", "workspace_id": "w1"}))
            .await
            .unwrap();
        assert!(!result.is_error);
        assert_eq!(
            workspace.calls.lock().unwrap().as_slice(),
            &[json!({"action": "delete", "workspace_id": "w1"})]
        );
        let refused = tool
            .execute(json!({"action": "workspace.list"}))
            .await
            .unwrap_err();
        assert!(
            refused.to_string().contains("workspace.delete"),
            "{refused}"
        );
    }

    #[test]
    fn an_old_action_name_gets_a_note_naming_its_new_place() {
        assert_eq!(
            moved_note("memory", &json!({"action": "delete_doc"})).as_deref(),
            Some("Note: memory(action=\"delete_doc\") is listed in `delete` now (action=\"memory.delete_doc\"); this call still works.")
        );
        assert!(moved_note("memory", &json!({"action": "update_doc"}))
            .is_some_and(|note| note.contains("`memory_update_doc`")));
        assert_eq!(moved_note("memory", &json!({"action": "search"})), None);
        assert_eq!(moved_note("search", &json!({"query": "x"})), None);
    }
}
