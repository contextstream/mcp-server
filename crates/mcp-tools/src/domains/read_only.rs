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
/// The parameters keep their types and values but not their prose: the twin
/// is listed beside its tool, whose schema documents every parameter, and
/// copying that prose made each twin nearly as large as its tool (#330 retest:
/// the four twins added 25 KB to every listing).
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
        for property in properties.values_mut() {
            strip_descriptions(property);
        }
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

/// Remove the `description` of a parameter's schema and of the schemas
/// nested in it. A nested parameter that is itself called `description`
/// stays: only schema keywords are walked, never values or property names.
pub(crate) fn strip_descriptions(schema: &mut Value) {
    let Value::Object(map) = schema else {
        return;
    };
    map.remove("description");
    for (keyword, value) in map.iter_mut() {
        match (keyword.as_str(), value) {
            (
                "properties" | "patternProperties" | "$defs" | "definitions",
                Value::Object(named),
            ) => {
                for nested in named.values_mut() {
                    strip_descriptions(nested);
                }
            }
            ("anyOf" | "oneOf" | "allOf" | "prefixItems", Value::Array(nested)) => {
                for item in nested {
                    strip_descriptions(item);
                }
            }
            ("items" | "additionalProperties" | "not" | "contains", nested) => {
                strip_descriptions(nested)
            }
            _ => {}
        }
    }
}

/// The read action `input` asks the twin for, as the twin lists it. The tool
/// behind a twin trims and lower-cases its action, so `"Search"` and
/// `" search"` read here too: refused before any request, a customer's
/// `memory_read` call failed and had to be repeated (2026-10-30). With no
/// action, `memory_read` given a query searches, the only read a bare query
/// can mean. `Err` says what to pass instead: the twin that reads it, the
/// write tool for a write, or the closest reads.
fn read_action(twin: &ReadOnlyTwin, tool_actions: &[&str], input: &Value) -> Result<&'static str> {
    let given = input.get("action").and_then(Value::as_str).unwrap_or("");
    let asked = given.trim().to_lowercase();
    if let Some(action) = twin.actions.iter().copied().find(|action| *action == asked) {
        return Ok(action);
    }
    let reads = twin.actions.join(", ");
    if asked.is_empty() {
        let has_query = input
            .get("query")
            .and_then(Value::as_str)
            .is_some_and(|query| !query.trim().is_empty());
        if twin.name == "memory_read" && has_query {
            return Ok("search");
        }
        return Err(Error::Validation(format!(
            "`{}` needs an action: one of {reads}.",
            twin.name
        )));
    }
    if let Some(other) = READ_ONLY_TWINS
        .iter()
        .find(|other| other.name != twin.name && other.actions.contains(&asked.as_str()))
    {
        return Err(Error::Validation(format!(
            "`{}` has no `{asked}`: call {}(action=\"{asked}\").",
            twin.name, other.name
        )));
    }
    if tool_actions.contains(&asked.as_str()) {
        return Err(Error::Validation(format!(
            "`{}` only reads (actions: {reads}). Use `{}` for `{asked}`.",
            twin.name, twin.tool
        )));
    }
    let close = closest_reads(twin, &asked);
    let hint = match close.as_slice() {
        [] => String::new(),
        [only] => format!(" Did you mean `{only}`?"),
        [rest @ .., last] => format!(
            " Did you mean {} or `{last}`?",
            rest.iter()
                .map(|action| format!("`{action}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    Err(Error::Validation(format!(
        "`{}` has no action `{asked}`.{hint} Its actions: {reads}.",
        twin.name
    )))
}

/// Up to three of the twin's reads sharing a word with `asked` (`docs` and
/// `doc` count as one), shortest first.
fn closest_reads(twin: &ReadOnlyTwin, asked: &str) -> Vec<&'static str> {
    fn words(action: &str) -> Vec<&str> {
        action
            .split('_')
            .map(|word| {
                word.strip_suffix('s')
                    .filter(|w| w.len() > 2)
                    .unwrap_or(word)
            })
            .filter(|word| !word.is_empty())
            .collect()
    }
    let asked = words(asked);
    let mut close: Vec<(usize, &'static str)> = twin
        .actions
        .iter()
        .copied()
        .filter_map(|action| {
            let shared = words(action)
                .iter()
                .filter(|word| asked.contains(word))
                .count();
            (shared > 0).then_some((shared, action))
        })
        .collect();
    close.sort_by(|(a_shared, a), (b_shared, b)| {
        b_shared
            .cmp(a_shared)
            .then(a.len().cmp(&b.len()))
            .then(a.cmp(b))
    });
    close
        .into_iter()
        .take(3)
        .map(|(_, action)| action)
        .collect()
}

#[async_trait]
impl ToolHandler for ReadOnlyTwinTool {
    async fn execute(&self, mut input: Value) -> Result<ToolResult> {
        let tool_schema = self.inner.input_schema();
        let tool_actions: Vec<&str> = tool_schema
            .pointer("/properties/action/enum")
            .and_then(Value::as_array)
            .map(|actions| actions.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let action = read_action(self.twin, &tool_actions, &input)?;
        if let Some(object) = input.as_object_mut() {
            object.insert("action".to_string(), Value::String(action.to_string()));
        }
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

    #[test]
    fn a_twin_schema_keeps_types_and_values_but_not_the_tools_prose() {
        let tool_schema = json!({
            "type": "object",
            "description": "the tool",
            "properties": {
                "action": {"type": "string", "enum": ["search", "delete_doc"], "description": "long"},
                "query": {"type": "string", "description": "What to look for, at length."},
                "filters": {
                    "type": "object",
                    "description": "Filters.",
                    "properties": {
                        "tags": {"type": "array", "items": {"type": "string", "description": "A tag."}},
                        "description": {"type": "string", "description": "A parameter with that name."}
                    }
                },
                "limit": {"type": "integer", "minimum": 1, "maximum": 100, "default": 10, "description": "How many."}
            }
        });
        let schema = twin_schema(memory_twin(), &tool_schema);
        let properties = &schema["properties"];
        assert_eq!(properties["query"], json!({"type": "string"}));
        assert_eq!(
            properties["limit"],
            json!({"type": "integer", "minimum": 1, "maximum": 100, "default": 10})
        );
        assert_eq!(
            properties["filters"],
            json!({"type": "object", "properties": {
                "tags": {"type": "array", "items": {"type": "string"}},
                "description": {"type": "string"}
            }})
        );
        // The twin's own words stay: what it is, and what `action` reads.
        assert_eq!(schema["description"], json!(memory_twin().description));
        assert!(properties["action"]["description"]
            .as_str()
            .is_some_and(|text| text.contains("only reads")));
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

    // A customer's memory_read failed before any request (2026-10-30, 09:01Z).
    #[tokio::test]
    async fn a_twin_reads_the_spellings_its_tool_reads_and_says_what_to_pass() {
        let inner = Arc::new(Recording::new());
        let tool = ReadOnlyTwinTool::new(memory_twin(), inner.clone());
        for given in ["Search", " search ", "SEARCH"] {
            tool.execute(json!({"action": given, "query": "IGE-94"}))
                .await
                .unwrap_or_else(|error| panic!("{given:?}: {error}"));
        }
        // A bare query is a search.
        tool.execute(json!({"query": "IGE-94"}))
            .await
            .expect("a bare query searches");
        assert_eq!(
            *inner.calls.lock().unwrap(),
            vec![json!({"action": "search", "query": "IGE-94"}); 4]
        );

        let message = |input: Value| {
            let tool = &tool;
            async move { tool.execute(input).await.expect_err("refused").to_string() }
        };
        let other_twin = message(json!({"action": "recall", "query": "x"})).await;
        assert!(
            other_twin.contains("call session_read(action=\"recall\")"),
            "{other_twin}"
        );
        let unknown = message(json!({"action": "search_docs", "query": "x"})).await;
        assert!(unknown.contains("has no action `search_docs`"), "{unknown}");
        assert!(unknown.contains("Did you mean `search`"), "{unknown}");
        assert!(unknown.contains("`list_docs`"), "{unknown}");
        assert!(!unknown.contains("Use `memory`"), "{unknown}");
        // A real write still points at the write tool.
        let write = message(json!({"action": "Create_Doc", "title": "x"})).await;
        assert!(write.contains("Use `memory` for `create_doc`"), "{write}");
        // No action and nothing to search for.
        let bare = message(json!({})).await;
        assert!(bare.contains("needs an action"), "{bare}");
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
