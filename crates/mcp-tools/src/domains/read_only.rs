//! Read-only twins of the consolidated tools, for clients that ask their
//! person to confirm every tool call that may write.
//!
//! The Gemini app asks for "Allow" before any tool whose annotations do not
//! say it is read-only ("Gemini requires manual confirmation for any write
//! actions", Google's help on custom apps). `memory`, `workspace`, `project`
//! and `session` put reads and writes behind one `action`, so each carries the
//! annotation of its most dangerous action and every read costs a click. Each
//! twin accepts only its tool's read actions, runs the same handler, and is
//! annotated read-only, like the coach surface's read/write split.
//!
//! The twins are direct tools only (never router operations), listed for
//! every client wherever their tool is: directory reviews (the Claude
//! connector directory, ChatGPT's action control) want reads and writes in
//! separate tools so reads run without confirmation (#330).

use std::sync::Arc;

use async_trait::async_trait;
use mcp_types::{
    tool::{ToolAnnotations, ToolMetadata, ToolResult},
    Error, Result,
};
use serde_json::{json, Value};

use crate::registry::{ToolHandler, ToolRegistry};

/// One twin: its name, the tool it reads through, and that tool's read actions.
pub struct ReadOnlyTwin {
    pub name: &'static str,
    pub tool: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub actions: &'static [&'static str],
}

/// Every twin. An action belongs here only when it changes nothing a person
/// could see: no row, claim, transcript or setting is written.
pub const READ_ONLY_TWINS: &[ReadOnlyTwin] = &[
    ReadOnlyTwin {
        name: "memory_read",
        tool: "memory",
        title: "Read Memory",
        description: "Read ContextStream memory without changing anything: search, decisions, \
                      docs, events, tasks, todos, knowledge nodes, diagrams and past-session \
                      transcripts. Same arguments as memory(action=...) for these actions. Use \
                      this for every read; use `memory` only to create, update or delete.",
        actions: &[
            "search",
            "decisions",
            "timeline",
            "summary",
            "get_node",
            "list_nodes",
            "get_event",
            "list_events",
            "get_task",
            "list_tasks",
            "get_todo",
            "list_todos",
            "get_diagram",
            "list_diagrams",
            "get_doc",
            "list_docs",
            "list_transcripts",
            "get_transcript",
            "search_transcripts",
            "search_archive",
            "team_tasks",
            "team_todos",
            "team_diagrams",
            "team_docs",
            "team_discussions",
            "team_transcript_topics",
        ],
    },
    ReadOnlyTwin {
        name: "workspace_read",
        tool: "workspace",
        title: "Read Workspaces",
        description: "List ContextStream workspaces, read one, or list its members, without \
                      changing anything. Same arguments as workspace(action=...) for these \
                      actions. Use `workspace` only to create, delete or change a workspace.",
        actions: &["list", "get", "team_members"],
    },
    ReadOnlyTwin {
        name: "project_read",
        tool: "project",
        title: "Read Projects",
        description: "List and inspect ContextStream projects without changing anything: \
                      list, get, overview, statistics, files, index status and history, and \
                      team projects. Same arguments as project(action=...) for these actions. \
                      Use `project` to create, index, update or delete.",
        actions: &[
            "list",
            "get",
            "overview",
            "statistics",
            "files",
            "index_status",
            "index_history",
            "team_projects",
            // Not `recent_changes`: it reads the server host's own git
            // repository, so the hosted gateway always refuses it (#325).
            // `project(action="recent_changes")` still runs on stdio.
        ],
    },
    ReadOnlyTwin {
        name: "session_read",
        tool: "session",
        title: "Read Session History",
        description: "Read lessons, plans, recaps, past sessions and decision traces without \
                      changing anything. Same arguments as session(action=...) for these \
                      actions. Use `session` to capture, update or resume.",
        actions: &[
            "get_lessons",
            "recall",
            "smart_search",
            "user_context",
            "decision_trace",
            "list_recaps",
            "resume_list",
            "get_plan",
            "list_plans",
            "list_suggested_rules",
            "suggested_rules_stats",
        ],
    },
];

/// Whether `name` is one of the read-only twins.
pub fn is_read_only_twin(name: &str) -> bool {
    READ_ONLY_TWINS.iter().any(|twin| twin.name == name)
}

/// Register every twin whose tool this registry serves directly.
pub fn register_read_only_twins(registry: &mut ToolRegistry) {
    for twin in READ_ONLY_TWINS {
        let Some(inner) = registry.get(twin.tool).map(|tool| tool.handler.clone()) else {
            continue;
        };
        registry.register(twin.name, Arc::new(ReadOnlyTwinTool::new(twin, inner)));
    }
}

struct ReadOnlyTwinTool {
    twin: &'static ReadOnlyTwin,
    inner: Arc<dyn ToolHandler>,
    metadata: ToolMetadata,
}

impl ReadOnlyTwinTool {
    fn new(twin: &'static ReadOnlyTwin, inner: Arc<dyn ToolHandler>) -> Self {
        let metadata = ToolMetadata {
            name: twin.name.to_string(),
            title: twin.title.to_string(),
            description: twin.description.to_string(),
            category: inner.metadata().category,
            annotations: ToolAnnotations::read_only(),
            is_pro: false,
            required_tier: None,
        };
        Self {
            twin,
            inner,
            metadata,
        }
    }
}

/// The tool's own schema with `action` narrowed to its read actions and
/// required. Conditional rules at the root name write actions, so they go.
fn twin_schema(twin: &ReadOnlyTwin, tool_schema: &Value) -> Value {
    let mut schema = tool_schema.clone();
    let Some(root) = schema.as_object_mut() else {
        return schema;
    };
    for key in ["allOf", "anyOf", "oneOf", "if", "then", "else"] {
        root.remove(key);
    }
    root.insert("description".to_string(), json!(twin.description));
    let properties = root
        .entry("properties")
        .or_insert_with(|| json!({}))
        .as_object_mut();
    if let Some(properties) = properties {
        properties.insert(
            "action".to_string(),
            json!({
                "type": "string",
                "enum": twin.actions,
                "description": format!(
                    "What to read. Every action only reads; to change anything use `{}`.",
                    twin.tool
                ),
            }),
        );
    }
    match root.get_mut("required") {
        Some(Value::Array(required)) => {
            if !required.iter().any(|name| name == "action") {
                required.push(json!("action"));
            }
        }
        _ => {
            root.insert("required".to_string(), json!(["action"]));
        }
    }
    schema
}

/// `Err` unless `input` names one of the twin's read actions.
fn check_read_action(twin: &ReadOnlyTwin, input: &Value) -> Result<()> {
    let action = input.get("action").and_then(Value::as_str).unwrap_or("");
    if twin.actions.contains(&action) {
        return Ok(());
    }
    let reads = twin.actions.join(", ");
    Err(Error::Validation(if action.is_empty() {
        format!("`{}` needs an action: one of {reads}.", twin.name)
    } else {
        format!(
            "`{}` only reads (actions: {reads}). Use `{}` for `{action}`.",
            twin.name, twin.tool
        )
    }))
}

#[async_trait]
impl ToolHandler for ReadOnlyTwinTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        check_read_action(self.twin, &input)?;
        self.inner.execute(input).await
    }

    fn metadata(&self) -> &ToolMetadata {
        &self.metadata
    }

    fn input_schema(&self) -> Value {
        twin_schema(self.twin, &self.inner.input_schema())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_types::tool::ToolCategory;
    use std::sync::Mutex;

    struct Recording {
        metadata: ToolMetadata,
        calls: Mutex<Vec<Value>>,
    }

    impl Recording {
        fn new() -> Self {
            Self {
                metadata: ToolMetadata {
                    name: "memory".to_string(),
                    title: "Memory".to_string(),
                    description: "test".to_string(),
                    category: ToolCategory::Memory,
                    annotations: ToolAnnotations::destructive(),
                    is_pro: false,
                    required_tier: None,
                },
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl ToolHandler for Recording {
        async fn execute(&self, input: Value) -> Result<ToolResult> {
            self.calls.lock().unwrap().push(input);
            Ok(ToolResult::text("ok"))
        }
        fn metadata(&self) -> &ToolMetadata {
            &self.metadata
        }
        fn input_schema(&self) -> Value {
            json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["search", "create_doc", "delete_doc"]},
                    "query": {"type": "string"}
                },
                "required": ["action"],
                "allOf": [{"if": {"properties": {"action": {"const": "create_doc"}}},
                           "then": {"required": ["query"]}}]
            })
        }
    }

    fn memory_twin() -> &'static ReadOnlyTwin {
        READ_ONLY_TWINS
            .iter()
            .find(|twin| twin.name == "memory_read")
            .expect("memory_read")
    }

    #[tokio::test]
    async fn a_twin_runs_reads_and_refuses_writes() {
        let inner = Arc::new(Recording::new());
        let tool = ReadOnlyTwinTool::new(memory_twin(), inner.clone());
        assert!(tool.metadata().annotations.read_only);
        assert!(!tool.metadata().annotations.destructive);

        tool.execute(json!({"action": "search", "query": "auth"}))
            .await
            .expect("a read runs");
        let refused = tool
            .execute(json!({"action": "create_doc", "title": "x"}))
            .await
            .expect_err("a write is refused");
        assert!(
            refused
                .to_string()
                .contains("Use `memory` for `create_doc`"),
            "{refused}"
        );
        let missing = tool.execute(json!({})).await.expect_err("no action");
        assert!(missing.to_string().contains("needs an action"), "{missing}");
        assert_eq!(
            *inner.calls.lock().unwrap(),
            vec![json!({"action": "search", "query": "auth"})]
        );
    }

    #[test]
    fn a_twin_schema_offers_only_read_actions() {
        let inner = Recording::new();
        let schema = twin_schema(memory_twin(), &inner.input_schema());
        assert_eq!(
            schema["properties"]["action"]["enum"],
            json!(memory_twin().actions)
        );
        assert_eq!(schema["required"], json!(["action"]));
        assert!(schema.get("allOf").is_none());
        assert_eq!(schema["properties"]["query"], json!({"type": "string"}));
    }

    #[test]
    fn twins_have_distinct_names_and_short_descriptions() {
        let mut names: Vec<_> = READ_ONLY_TWINS.iter().map(|twin| twin.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), READ_ONLY_TWINS.len());
        for twin in READ_ONLY_TWINS {
            assert!(is_read_only_twin(twin.name));
            assert!(twin.description.chars().count() <= 1024, "{}", twin.name);
            assert!(!twin.actions.is_empty(), "{}", twin.name);
        }
        assert!(!is_read_only_twin("memory"));
    }
}
