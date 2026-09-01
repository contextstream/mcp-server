//! MCP server lifecycle management.

use crate::agentic_telemetry::{AgenticTelemetry, AgenticTelemetryInput};
use anyhow::Result;
use mcp_client::{run_with_session_key, ContextStreamClient};
use mcp_session::SessionManager;
use mcp_tools::{domains, ToolRegistry};
use mcp_types::{
    config::ToolSurfaceProfile, decorate_stateless_cacheable_result, decorate_stateless_result,
    has_stateless_protocol_metadata, legacy_initialize_instructions, stateless_discovery_teaching,
    tool::ToolAnnotations, validate_stateless_jsonrpc_envelope, validate_stateless_method_params,
    validate_stateless_request, Config, HarnessId, McpCacheScope, McpProtocolError,
    ReadinessEvidenceSource, SessionKey, StatelessMcpConformance, MCP_PROTOCOL_2024_11_05,
    MCP_PROTOCOL_2026_07_28, MCP_TOOLS_LIST_TTL_MS,
};
use std::sync::Arc;
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tracing::{debug, info};
use uuid::Uuid;

/// Build and register the full tool registry for the provided config.
pub fn build_registry(
    config: &Config,
    client: ContextStreamClient,
    session: Arc<SessionManager>,
) -> ToolRegistry {
    let mut registry = ToolRegistry::new(config);
    registry.set_session_manager(session.clone());
    if !config.is_http_transport {
        registry.set_activation_client(client.clone());
    }

    // Retain the legacy wire-compatibility layer as a public no-op. Current
    // hosted products are provided by the MongoDB-free acceleration layer.
    let atlas_layer = crate::atlas::build_atlas_layer();
    debug!(
        atlas_layer = %crate::atlas::layer_summary(&atlas_layer),
        "atlas product layer constructed"
    );
    registry.set_atlas_layer(atlas_layer.clone());

    let acceleration_layer = crate::acceleration::build_acceleration_layer();
    debug!(
        acceleration_layer = %crate::acceleration::layer_summary(&acceleration_layer),
        "acceleration layer constructed"
    );
    registry.set_acceleration_layer(acceleration_layer.clone());

    if registry.is_progressive_mode() {
        if let Err(e) = registry.load_bundle_state() {
            debug!("Failed to load bundle state: {}", e);
        } else {
            let bundles = registry.enabled_bundles();
            if !bundles.is_empty() {
                debug!("Loaded {} enabled bundles: {:?}", bundles.len(), bundles);
            }
        }
    }

    let index_keeper = Arc::new(domains::index_keeper::IndexKeeper::new(
        client.clone(),
        session.clone(),
        atlas_layer,
        acceleration_layer,
    ));

    domains::session::register_session_tools(
        &mut registry,
        client.clone(),
        session.clone(),
        index_keeper.clone(),
    );
    domains::flash::register_flash_tools(&mut registry, client.clone(), session.clone());
    domains::search::register_search_tools(
        &mut registry,
        client.clone(),
        session.clone(),
        index_keeper,
    );
    domains::memory::register_memory_tools(&mut registry, client.clone(), session.clone());
    domains::graph::register_graph_tools(&mut registry, client.clone(), session.clone());
    domains::workspace::register_workspace_tools(&mut registry, client.clone());
    domains::project::register_project_tools(&mut registry, client.clone(), session.clone());
    domains::integrations::register_integration_tools(
        &mut registry,
        client.clone(),
        session.clone(),
    );
    domains::vcs::register_vcs_tools(&mut registry, client.clone());
    domains::reminder::register_reminder_tools(&mut registry, client.clone());
    domains::coordination::register_coordination_tools(&mut registry, client.clone());
    // Phase 1-3 taxonomy expansion: unified entity CRUD across tickets,
    // handoffs, backlog_views, incidents, releases, experiments, goals,
    // key_results, sprints, reviews, risks.
    domains::entity::register_entity_tools(&mut registry, client.clone(), session.clone());
    domains::media::register_media_tools(&mut registry, client.clone(), session.clone());
    domains::help::register_help_tools(&mut registry, client.clone());
    if config.capsule_enabled {
        domains::capsule::register_capsule_tools(&mut registry, client.clone());
    }
    // Agent Q&A is registered unconditionally and returns a clean unavailable
    // response when its hosted backend is not configured.
    domains::qa::register_qa_tools(&mut registry, client.clone());
    domains::skill::register_skill_tools(&mut registry, client, session.clone());
    // Compatibility chart/job surfaces self-gate when no provider is available.
    domains::charts::register_charts_tools(&mut registry, session.clone());
    domains::atlas_jobs::register_atlas_job_tools(&mut registry, session);

    registry
}

/// Resolve a single-cell Unicode glyph for a tool name by matching
/// the leading verb prefix. Falls back to ⌬ (benzene ring) for
/// anything that doesn't match — chosen as the ContextStream brand
/// mark because it reads as "structured data" and is visually
/// distinct from the generic ⚙ that opencode hard-codes for MCP
/// tools.
///
/// Emitted to MCP clients as `_meta.contextstream.icon` on both
/// `tools/list` entries and `tools/call` results. Opencode currently
/// ignores it, but VS Code, ContextStream desktop, and any future
/// client that respects MCP icon hints can pick it up directly.
pub(crate) fn contextstream_call_icon(name: &str) -> &'static str {
    // Verb → glyph table. Our tool names land in three shapes:
    //   1. Bare verb:           `search`, `index`, `capture` (rare)
    //   2. Verb-first:          `capture_plan`, `save_plan`
    //   3. Router-prefixed:     `memory_create_doc`, `session_capture_lesson`
    //
    // For (3) the verb is the SECOND underscore-separated token. We scan
    // the first two tokens so the resolver works for all three shapes
    // without listing every full prefix combination.
    const VERB_GLYPHS: &[(&str, &str)] = &[
        ("save", "⊕"),
        ("create", "⊕"),
        ("update", "↻"),
        ("delete", "⊖"),
        ("list", "☰"),
        ("get", "▸"),
        ("search", "⌕"),
        ("recall", "⟲"),
        ("restore", "⟲"),
        ("capture", "★"),
        ("complete", "✓"),
        ("ingest", "⇣"),
        ("import", "⇣"),
        ("export", "⇡"),
        ("index", "⊞"),
        ("run", "▶"),
        ("link", "⟷"),
        ("sync", "⟷"),
    ];
    let mut tokens = name.split('_');
    let first = tokens.next();
    let second = tokens.next();
    for (verb, glyph) in VERB_GLYPHS {
        if first == Some(*verb) || second == Some(*verb) {
            return glyph;
        }
    }
    "⌬"
}

/// Resolve the user-facing label for a tool/action pair.
///
/// `tools/call` injects the returned string as the JSON-RPC result's `title`
/// field so MCP clients (opencode, VS Code, anything reading the result
/// `title`) can render "Saving plan to ContextStream" instead of the bare tool
/// name. `tools/list` uses the same logic to populate
/// `_meta.contextstream.status.in_progress` (overall + per-action map).
pub(crate) fn contextstream_call_title(
    name: &str,
    tool_title: &str,
    read_only: bool,
    arguments: &serde_json::Value,
) -> String {
    let status = contextstream_status_metadata(name, tool_title, read_only);

    // Per-action precision when the schema exposes an `action` parameter.
    if let Some(action) = arguments.get("action").and_then(|v| v.as_str()) {
        if let Some(label) = status
            .get("actions")
            .and_then(|m| m.get(action))
            .and_then(|v| v.as_str())
        {
            return label.to_string();
        }
    }

    // Optional second-axis: `event_type` for session(action=capture)
    // and `entity(kind=ticket, action=create)` etc. Fall through to
    // the tool-level in_progress label when no override matches.
    status
        .get("in_progress")
        .and_then(|v| v.as_str())
        .unwrap_or(tool_title)
        .to_string()
}

fn contextstream_status_metadata(name: &str, title: &str, read_only: bool) -> serde_json::Value {
    let in_progress = match name {
        "init" => "Initializing ContextStream session".to_string(),
        "context" => "Loading ContextStream context".to_string(),
        "search" => "Searching ContextStream index".to_string(),
        "graph" => "Analyzing ContextStream code graph".to_string(),
        "help" => "Looking up ContextStream help".to_string(),
        "capture_plan" => "Saving plan to ContextStream".to_string(),
        "session_capture_lesson" => "Saving lesson to ContextStream".to_string(),
        "session_remember" => "Saving memory to ContextStream".to_string(),
        "session_capture" => "Saving session note to ContextStream".to_string(),
        "memory" => "Saving in ContextStream memory".to_string(),
        "memory_create_doc" => "Saving doc to ContextStream".to_string(),
        "memory_update_doc" => "Updating doc in ContextStream".to_string(),
        "memory_delete_doc" => "Deleting doc in ContextStream".to_string(),
        "memory_create_task" => "Saving task to ContextStream".to_string(),
        "memory_update_task" => "Updating task in ContextStream".to_string(),
        "memory_create_todo" => "Saving todo to ContextStream".to_string(),
        "memory_complete_todo" => "Completing todo in ContextStream".to_string(),
        "memory_create_event" => "Saving event to ContextStream".to_string(),
        "session" => "Updating ContextStream session".to_string(),
        "entity" => "Updating ContextStream entity".to_string(),
        "reminder" => "Setting reminder in ContextStream".to_string(),
        "integration" => "Updating ContextStream integration".to_string(),
        "vcs" => "Linking VCS with ContextStream".to_string(),
        "qa" => "Asking ContextStream Q&A".to_string(),
        "project" => "Updating ContextStream project".to_string(),
        "workspace" => "Updating ContextStream workspace".to_string(),
        "capsule" => "Saving ContextStream capsule".to_string(),
        "instruct" => "Updating session instructions in ContextStream".to_string(),
        "skill" => "Running ContextStream skill".to_string(),
        "media" => "Saving media in ContextStream".to_string(),
        "chart" | "atlas_chart" => "Rendering ContextStream chart".to_string(),
        "async_job" | "atlas_job" => "Running ContextStream async job".to_string(),
        _ if read_only => format!("Calling ContextStream — {}", title),
        _ => format!("Saving with ContextStream — {}", title),
    };
    let completed = match name {
        "init" => "ContextStream session ready".to_string(),
        "context" => "ContextStream context loaded".to_string(),
        "search" => "ContextStream search complete".to_string(),
        "graph" => "ContextStream graph analysis complete".to_string(),
        "help" => "ContextStream help complete".to_string(),
        "capture_plan" => "Plan saved to ContextStream".to_string(),
        "session_capture_lesson" => "Lesson saved to ContextStream".to_string(),
        "session_remember" => "Memory saved to ContextStream".to_string(),
        "session_capture" => "Session note saved to ContextStream".to_string(),
        "memory" => "Saved to ContextStream memory".to_string(),
        "memory_create_doc" => "Doc saved to ContextStream".to_string(),
        "memory_update_doc" => "Doc updated in ContextStream".to_string(),
        "memory_delete_doc" => "Doc deleted in ContextStream".to_string(),
        "memory_create_task" => "Task saved to ContextStream".to_string(),
        "memory_update_task" => "Task updated in ContextStream".to_string(),
        "memory_create_todo" => "Todo saved to ContextStream".to_string(),
        "memory_complete_todo" => "Todo completed in ContextStream".to_string(),
        "memory_create_event" => "Event saved to ContextStream".to_string(),
        "session" => "ContextStream session updated".to_string(),
        "entity" => "ContextStream entity updated".to_string(),
        "reminder" => "Reminder saved to ContextStream".to_string(),
        "integration" => "ContextStream integration updated".to_string(),
        "vcs" => "VCS linked with ContextStream".to_string(),
        "qa" => "ContextStream Q&A complete".to_string(),
        "project" => "ContextStream project updated".to_string(),
        "workspace" => "ContextStream workspace updated".to_string(),
        "capsule" => "ContextStream capsule saved".to_string(),
        "instruct" => "Session instructions updated".to_string(),
        "skill" => "ContextStream skill complete".to_string(),
        "media" => "Media saved to ContextStream".to_string(),
        "chart" | "atlas_chart" => "ContextStream chart ready".to_string(),
        "async_job" | "atlas_job" => "ContextStream async job complete".to_string(),
        _ if read_only => format!("ContextStream — {} complete", title),
        _ => format!("ContextStream — {} saved", title),
    };
    let mut metadata = serde_json::json!({
        "in_progress": in_progress,
        "completed": completed,
    });

    if name == "session" {
        metadata["actions"] = serde_json::json!({
            "capture": "Saving session note to ContextStream",
            "capture_lesson": "Saving lesson to ContextStream",
            "remember": "Saving memory to ContextStream",
            "capture_plan": "Saving plan to ContextStream",
            "update_plan": "Updating plan in ContextStream",
            "suggested_rule_action": "Updating suggested rule in ContextStream",
            "recall": "Recalling ContextStream session",
            "ground": "Grounding from ContextStream",
            "get_lessons": "Loading ContextStream lessons",
            "get_plan": "Loading plan from ContextStream",
            "list_plans": "Listing ContextStream plans",
            "summary": "Loading ContextStream session summary",
            "compress": "Compressing ContextStream session",
            "delta": "Loading ContextStream session delta",
            "smart_search": "Searching ContextStream memory",
            "decision_trace": "Tracing ContextStream decision",
            "restore_context": "Restoring ContextStream context",
            "list_suggested_rules": "Listing suggested ContextStream rules",
            "suggested_rules_stats": "Loading ContextStream rule stats",
            "user_context": "Loading ContextStream user context",
            "remember_user": "Saving user context to ContextStream"
        });
    } else if name == "memory" {
        metadata["actions"] = serde_json::json!({
            "create_node": "Saving memory node to ContextStream",
            "update_node": "Updating memory node in ContextStream",
            "delete_node": "Deleting memory node in ContextStream",
            "supersede_node": "Updating memory node in ContextStream",
            "create_event": "Saving event to ContextStream",
            "update_event": "Updating event in ContextStream",
            "delete_event": "Deleting event in ContextStream",
            "distill_event": "Distilling event in ContextStream",
            "import_batch": "Importing batch to ContextStream",
            "create_task": "Saving task to ContextStream",
            "update_task": "Updating task in ContextStream",
            "delete_task": "Deleting task in ContextStream",
            "reorder_tasks": "Updating task order in ContextStream",
            "create_todo": "Saving todo to ContextStream",
            "update_todo": "Updating todo in ContextStream",
            "delete_todo": "Deleting todo in ContextStream",
            "complete_todo": "Completing todo in ContextStream",
            "create_diagram": "Saving diagram to ContextStream",
            "update_diagram": "Updating diagram in ContextStream",
            "delete_diagram": "Deleting diagram in ContextStream",
            "create_doc": "Saving doc to ContextStream",
            "update_doc": "Updating doc in ContextStream",
            "delete_doc": "Deleting doc in ContextStream",
            "create_roadmap": "Saving roadmap to ContextStream",
            "delete_transcript": "Deleting transcript in ContextStream",
            "search": "Searching ContextStream memory",
            "decisions": "Loading ContextStream decisions",
            "timeline": "Loading ContextStream timeline",
            "summary": "Loading ContextStream summary",
            "list_nodes": "Listing ContextStream memory nodes",
            "list_events": "Listing ContextStream events",
            "list_tasks": "Listing ContextStream tasks",
            "list_todos": "Listing ContextStream todos",
            "list_docs": "Listing ContextStream docs",
            "list_diagrams": "Listing ContextStream diagrams",
            "list_transcripts": "Listing ContextStream transcripts",
            "get_node": "Loading ContextStream memory node",
            "get_event": "Loading ContextStream event",
            "get_task": "Loading ContextStream task",
            "get_todo": "Loading ContextStream todo",
            "get_doc": "Loading ContextStream doc",
            "get_diagram": "Loading ContextStream diagram",
            "get_transcript": "Loading ContextStream transcript",
            "search_transcripts": "Searching ContextStream transcripts",
            "search_archive": "Searching ContextStream archive",
            "team_tasks": "Listing team tasks in ContextStream",
            "team_todos": "Listing team todos in ContextStream",
            "team_diagrams": "Listing team diagrams in ContextStream",
            "team_docs": "Listing team docs in ContextStream"
        });
    } else if name == "entity" {
        metadata["actions"] = serde_json::json!({
            "create": "Saving entity to ContextStream",
            "update": "Updating entity in ContextStream",
            "delete": "Deleting entity in ContextStream",
            "list": "Listing ContextStream entities",
            "get": "Loading ContextStream entity"
        });
    } else if name == "skill" {
        metadata["actions"] = serde_json::json!({
            "create": "Saving skill to ContextStream",
            "update": "Updating skill in ContextStream",
            "delete": "Deleting skill in ContextStream",
            "list": "Listing ContextStream skills",
            "get": "Loading ContextStream skill",
            "run": "Running ContextStream skill",
            "import": "Importing skills into ContextStream"
        });
    } else if name == "media" {
        metadata["actions"] = serde_json::json!({
            "index": "Indexing media in ContextStream",
            "delete": "Deleting media in ContextStream",
            "list": "Listing ContextStream media",
            "search": "Searching ContextStream media",
            "status": "Checking ContextStream media status",
            "get_clip": "Extracting clip from ContextStream media"
        });
    } else if name == "reminder" {
        metadata["actions"] = serde_json::json!({
            "create": "Setting reminder in ContextStream",
            "update": "Updating reminder in ContextStream",
            "delete": "Deleting reminder in ContextStream",
            "list": "Listing ContextStream reminders",
            "active": "Loading active ContextStream reminders"
        });
    } else if name == "project" {
        metadata["actions"] = serde_json::json!({
            "create": "Creating ContextStream project",
            "update": "Updating ContextStream project",
            "delete": "Deleting ContextStream project",
            "list": "Listing ContextStream projects",
            "get": "Loading ContextStream project",
            "index": "Indexing ContextStream project",
            "ingest_local": "Ingesting local files into ContextStream",
            "index_status": "Checking ContextStream index status"
        });
    } else if name == "workspace" {
        metadata["actions"] = serde_json::json!({
            "create": "Creating ContextStream workspace",
            "update": "Updating ContextStream workspace",
            "list": "Listing ContextStream workspaces",
            "get": "Loading ContextStream workspace"
        });
    } else if name == "capsule" {
        metadata["actions"] = serde_json::json!({
            "create": "Saving ContextStream capsule",
            "update": "Updating ContextStream capsule",
            "delete": "Deleting ContextStream capsule",
            "list": "Listing ContextStream capsules",
            "get": "Loading ContextStream capsule",
            "export": "Exporting ContextStream capsule",
            "import": "Importing ContextStream capsule"
        });
    } else if name == "integration" {
        metadata["actions"] = serde_json::json!({
            "create": "Saving ContextStream integration",
            "update": "Updating ContextStream integration",
            "delete": "Deleting ContextStream integration",
            "list": "Listing ContextStream integrations",
            "get": "Loading ContextStream integration",
            "test": "Testing ContextStream integration"
        });
    } else if name == "vcs" {
        metadata["actions"] = serde_json::json!({
            "link": "Linking VCS with ContextStream",
            "unlink": "Unlinking VCS from ContextStream",
            "list": "Listing ContextStream VCS links",
            "get": "Loading ContextStream VCS link",
            "search": "Searching VCS in ContextStream",
            "sync": "Syncing VCS with ContextStream"
        });
    } else if name == "instruct" {
        metadata["actions"] = serde_json::json!({
            "bootstrap": "Bootstrapping ContextStream session",
            "push": "Pushing instructions to ContextStream",
            "ack": "Acknowledging ContextStream instructions",
            "clear": "Clearing ContextStream instructions",
            "get": "Loading ContextStream instructions",
            "stats": "Loading ContextStream instruction stats",
            "checkpoint": "Checkpointing ContextStream instructions",
            "verify": "Verifying ContextStream instructions"
        });
    }

    metadata
}

/// Version of the ContextStream-owned metadata nested under a tool's `_meta`.
///
/// MCP clients must ignore unknown namespaced metadata, so this can evolve
/// independently while the standard `name`/`description`/`inputSchema`
/// compatibility contract remains stable.
pub(crate) const TOOL_DISCOVERY_METADATA_VERSION: u64 = 1;

fn contextstream_tool_annotations(title: &str, annotations: &ToolAnnotations) -> serde_json::Value {
    serde_json::json!({
        "title": title,
        "readOnlyHint": annotations.read_only,
        "destructiveHint": annotations.destructive,
        "idempotentHint": annotations.idempotent,
        "openWorldHint": annotations.open_world,
    })
}

fn contextstream_tool_extension(
    name: &str,
    title: &str,
    annotations: &ToolAnnotations,
) -> serde_json::Value {
    serde_json::json!({
        "metadataVersion": TOOL_DISCOVERY_METADATA_VERSION,
        "icon": contextstream_call_icon(name),
        "status": contextstream_status_metadata(name, title, annotations.read_only),
        "safety": {
            "requiresConfirmation": annotations.requires_confirmation,
            "longRunning": annotations.long_running,
        }
    })
}

pub(crate) fn contextstream_tool_list_entry(tool: &mcp_tools::RegisteredTool) -> serde_json::Value {
    let meta = &tool.metadata;
    serde_json::json!({
        "name": meta.name,
        "title": meta.title,
        "description": meta.description,
        "category": meta.category.as_str(),
        "inputSchema": tool.input_schema,
        "annotations": contextstream_tool_annotations(&meta.title, &meta.annotations),
        "_meta": {
            "contextstream": contextstream_tool_extension(
                &meta.name,
                &meta.title,
                &meta.annotations
            )
        }
    })
}

fn contextstream_meta_tool_entry(
    name: &str,
    title: &str,
    description: &str,
    input_schema: serde_json::Value,
    annotations: ToolAnnotations,
) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "title": title,
        "description": description,
        "category": "router",
        "inputSchema": input_schema,
        "annotations": contextstream_tool_annotations(title, &annotations),
        "_meta": {
            "contextstream": contextstream_tool_extension(name, title, &annotations)
        }
    })
}

fn operations_meta_tool() -> serde_json::Value {
    contextstream_meta_tool_entry(
        "operations",
        "List Operations",
        "List available operations. Use execute_operation to run them.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "category": { "type": "string", "description": "Filter by category" },
                "format": { "type": "string", "description": "Output format: grouped, minimal, full" }
            }
        }),
        ToolAnnotations::read_only().closed_world(),
    )
}

pub(crate) fn tool_search_meta_tool() -> serde_json::Value {
    contextstream_meta_tool_entry(
        "tool_search",
        "Search Tools",
        "Search available tools and hidden operations, then call direct tools or use execute_operation for deferred capabilities.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "What capability or task you need" },
                "category": { "type": "string", "description": "Optional category filter" },
                "limit": { "type": "integer", "description": "Maximum results (default 8)" }
            },
            "required": ["query"]
        }),
        ToolAnnotations::read_only().closed_world(),
    )
}

pub(crate) fn execute_operation_meta_tool() -> serde_json::Value {
    contextstream_meta_tool_entry(
        "execute_operation",
        "Execute Operation",
        "Execute a hidden or deferred operation returned by tool_search. The selected operation may modify or delete data, so treat this router as destructive.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Operation name to execute" },
                "arguments": { "type": "object", "description": "Operation arguments" }
            },
            "required": ["name"]
        }),
        ToolAnnotations::destructive(),
    )
}

pub(crate) fn batch_operations_meta_tool() -> serde_json::Value {
    contextstream_meta_tool_entry(
        "batch_operations",
        "Batch Read-Only Operations",
        "Execute multiple independent read-only operations in one call. Rejects write or destructive operations.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "operations": {
                    "type": "array",
                    "description": "Operations to execute",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string", "description": "Tool or operation name" },
                            "arguments": { "type": "object", "description": "Arguments for the tool or operation" }
                        },
                        "required": ["name"]
                    }
                }
            },
            "required": ["operations"]
        }),
        ToolAnnotations::read_only(),
    )
}

pub(crate) fn validate_contextstream_meta_tool_arguments(
    name: &str,
    arguments: &serde_json::Value,
) -> Result<(), McpProtocolError> {
    let invalid = |message: &str| Err(McpProtocolError::invalid_params(message));
    match name {
        "operations" => {
            if arguments
                .get("category")
                .is_some_and(|value| !value.is_string())
            {
                return invalid("operations arguments.category must be a string");
            }
            if arguments
                .get("format")
                .is_some_and(|value| !value.is_string())
            {
                return invalid("operations arguments.format must be a string");
            }
        }
        "tool_search" => {
            if arguments
                .get("query")
                .and_then(serde_json::Value::as_str)
                .is_none_or(|query| query.trim().is_empty())
            {
                return invalid("tool_search arguments.query must be a non-empty string");
            }
            if arguments
                .get("category")
                .is_some_and(|value| !value.is_string())
            {
                return invalid("tool_search arguments.category must be a string");
            }
            if arguments
                .get("limit")
                .is_some_and(|value| !value.is_i64() && !value.is_u64())
            {
                return invalid("tool_search arguments.limit must be an integer");
            }
        }
        "execute_operation" => {
            if arguments
                .get("name")
                .and_then(serde_json::Value::as_str)
                .is_none_or(|name| name.is_empty())
            {
                return invalid("execute_operation arguments.name must be a non-empty string");
            }
            if arguments
                .get("arguments")
                .is_some_and(|value| !value.is_object())
            {
                return invalid("execute_operation arguments.arguments must be an object");
            }
        }
        "batch_operations" => {
            let Some(operations) = arguments
                .get("operations")
                .and_then(|value| value.as_array())
            else {
                return invalid("batch_operations arguments.operations must be an array");
            };
            for operation in operations {
                if operation
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .is_none_or(|name| name.is_empty())
                {
                    return invalid("batch_operations entries require a non-empty string name");
                }
                if operation
                    .get("arguments")
                    .is_some_and(|value| !value.is_object())
                {
                    return invalid("batch_operations entry arguments must be an object");
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn requested_category_matches(requested: Option<&str>, actual: &str) -> bool {
    requested
        .map(|requested| {
            requested.eq_ignore_ascii_case(actual)
                || (actual == "router" && requested.eq_ignore_ascii_case("meta"))
        })
        .unwrap_or(true)
}

/// Build the canonical, deterministic `tools/list` array for every transport.
///
/// All standard and namespaced fields originate here. Keeping the legacy
/// v0.5.62 fields (`name`, `description`, `inputSchema`) mandatory while
/// versioning ContextStream's `_meta` block makes additions backward
/// compatible and prevents stdio/HTTP drift.
pub(crate) fn contextstream_tools_list(
    registry: &ToolRegistry,
    category: Option<&str>,
) -> Vec<serde_json::Value> {
    let mut tools: Vec<_> = registry
        .list()
        .into_iter()
        .filter(|tool| requested_category_matches(category, tool.metadata.category.as_str()))
        .map(contextstream_tool_list_entry)
        .collect();

    if requested_category_matches(category, "router") {
        if registry.is_router_mode() {
            tools.push(operations_meta_tool());
            tools.push(execute_operation_meta_tool());
        } else if registry.is_openai_agentic_surface() {
            tools.push(tool_search_meta_tool());
            tools.push(execute_operation_meta_tool());
            tools.push(batch_operations_meta_tool());
        }
    }

    tools.sort_by(|left, right| {
        left.get("name")
            .and_then(serde_json::Value::as_str)
            .cmp(&right.get("name").and_then(serde_json::Value::as_str))
    });
    tools
}

/// Run the MCP server with stdio transport.
pub async fn run_server(
    config: Config,
    client: ContextStreamClient,
    session: Arc<SessionManager>,
) -> Result<()> {
    // Record process start time for binary mtime comparison during auto-update exec
    record_process_start();

    // Load the pinned vocabulary before registry readiness or the first
    // request. This is synchronous and idempotent; request paths never load it.
    mcp_tools::wire_tokens::warm_o200k();

    let registry = build_registry(&config, client.clone(), session.clone());

    let tool_count = registry.len();
    let op_count = registry.operation_count();

    if !config.log_level.is_quiet() {
        // Log mode information
        let mode = if registry.is_consolidated_mode() {
            "consolidated"
        } else if registry.is_router_mode() {
            "router"
        } else if registry.is_progressive_mode() {
            "progressive"
        } else {
            "standard"
        };

        if registry.is_router_mode() {
            eprintln!("✓ {} tools, {} operations ({})", tool_count, op_count, mode);
        } else {
            eprintln!("✓ {} tools registered ({})", tool_count, mode);
        }

        // Show enabled bundles in progressive mode
        if registry.is_progressive_mode() {
            let bundles = registry.enabled_bundles();
            if !bundles.is_empty() {
                eprintln!("✓ bundles: {}", bundles.join(", "));
            }
        }

        eprintln!("✓ ready");
    }

    let telemetry = AgenticTelemetry::new(client.clone(), session.clone());

    // Keep every locally-mapped project's index warm in the background, so an
    // agent's first search hits a fresh index instead of paying a cold
    // re-ingest. Stdio (single-tenant) only — deliberately NOT started on the
    // shared HTTP gateway, which serves many tenants and has no single local
    // filesystem of user projects to enumerate.
    domains::index_keeper::spawn_keep_warm_daemon(client.clone());

    // Stdio is the only single-user local lane. Install its marker explicitly
    // so missing/lost task-local identity can fail closed instead of being
    // inferred as local by cache code.
    run_with_session_key(SessionKey::Local, || async move {
        run_stdio_transport(registry, client, session, telemetry).await
    })
    .await
}

/// Run the stdio transport for JSON-RPC.
async fn run_stdio_transport(
    registry: ToolRegistry,
    client: ContextStreamClient,
    session: Arc<SessionManager>,
    telemetry: AgenticTelemetry,
) -> Result<()> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let mut reader = BufReader::new(stdin);
    let mut stdout = stdout;

    let registry = Arc::new(registry);
    let client = Arc::new(client);

    let mut line = String::new();
    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line).await?;

        if bytes_read == 0 {
            // EOF - client disconnected
            debug!("Client disconnected");
            break;
        }

        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Parse JSON-RPC request
        let request: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(e) => {
                let error_response = json_rpc_error(None, -32700, &format!("Parse error: {}", e));
                stdout.write_all(error_response.as_bytes()).await?;
                stdout.write_all(b"\n").await?;
                stdout.flush().await?;
                continue;
            }
        };

        let wire_observation = mcp_tools::wire_tokens::WireTokenObservation::default();
        let response = handle_request(
            &registry,
            &client,
            &session,
            &telemetry,
            request,
            wire_observation.clone(),
        )
        .await;

        // Don't write empty responses (e.g. for notifications)
        if !response.is_empty() {
            let mut observed_wire = response.as_bytes().to_vec();
            observed_wire.push(b'\n');
            mcp_tools::wire_tokens::observe_final_wire_bytes(
                &observed_wire,
                &wire_observation,
                "mcp_stdio_final_response",
            );
            stdout.write_all(response.as_bytes()).await?;
            stdout.write_all(b"\n").await?;
            stdout.flush().await?;
        }

        // After flushing the response, check if a background auto-update completed.
        // If so, exec() the new binary to apply the update in this session.
        // exec() replaces the process image but preserves file descriptors (stdin/stdout),
        // so the MCP stdio connection stays alive seamlessly.
        if domains::should_exec_after_update() {
            exec_updated_binary();
        }
    }

    Ok(())
}

/// Captured at process startup to compare against binary mtime after update.
static PROCESS_START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// Call once at startup to record when this process started.
pub fn record_process_start() {
    PROCESS_START.get_or_init(std::time::Instant::now);
}

/// Replace the current process with the updated binary.
/// Uses Unix exec() which preserves stdin/stdout file descriptors,
/// keeping the MCP stdio pipe alive across the update.
fn exec_updated_binary() {
    let binary = match std::env::current_exe() {
        Ok(path) => path,
        Err(e) => {
            tracing::warn!("Cannot determine current binary path for exec: {}", e);
            return;
        }
    };

    // Safety: verify the binary on disk was modified after this process started.
    // Prevents unnecessary exec if the update didn't actually replace the binary.
    if let (Ok(meta), Some(started)) = (std::fs::metadata(&binary), PROCESS_START.get()) {
        if let Ok(modified) = meta.modified() {
            // Binary mtime must be more recent than when we booted.
            // Instant doesn't directly convert to SystemTime, so we compute:
            // binary_mtime > (now - elapsed_since_start)
            let elapsed = started.elapsed();
            let threshold = std::time::SystemTime::now() - elapsed;
            if modified <= threshold {
                info!("Binary has not been modified since process start, skipping exec");
                return;
            }
        }
    }

    let args: Vec<String> = std::env::args().collect();
    info!("Applying in-session update: exec {}", binary.display());

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new(&binary);
        if args.len() > 1 {
            cmd.args(&args[1..]);
        }
        // exec() replaces this process — this line never returns on success
        let err = cmd.exec();
        tracing::error!("exec() failed: {}", err);
    }

    #[cfg(not(unix))]
    {
        tracing::warn!("In-session update via exec() is only supported on Unix. Restart the MCP process to apply the update.");
    }
}

fn harness_from_initialize_params(params: &serde_json::Value) -> Option<HarnessId> {
    params
        .get("clientInfo")
        .or_else(|| params.get("client_info"))
        .and_then(|client| client.get("name"))
        .and_then(serde_json::Value::as_str)
        .and_then(HarnessId::from_client_hint)
}

fn grounding_source(tool_name: &str) -> Option<ReadinessEvidenceSource> {
    match tool_name {
        "init" => Some(ReadinessEvidenceSource::InitTool),
        "context" => Some(ReadinessEvidenceSource::ContextTool),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SuccessfulRuntimeEvidence {
    Grounded(ReadinessEvidenceSource),
    InferredPractice,
}

fn successful_runtime_evidence(
    tool_name: &str,
    result: &mcp_types::tool::ToolResult,
    resolved_workspace_id: Option<Uuid>,
) -> Option<SuccessfulRuntimeEvidence> {
    if result.is_error || mcp_tools::registry::tool_result_is_access_gate(result) {
        return None;
    }
    if let Some(source) = grounding_source(tool_name) {
        resolved_workspace_id
            .is_some_and(|workspace_id| !workspace_id.is_nil())
            .then_some(SuccessfulRuntimeEvidence::Grounded(source))
    } else if tool_name == "search" {
        Some(SuccessfulRuntimeEvidence::InferredPractice)
    } else {
        None
    }
}

async fn record_successful_runtime_evidence(
    client: &ContextStreamClient,
    session: &SessionManager,
    telemetry: &AgenticTelemetry,
    tool_name: &str,
    result: &mcp_types::tool::ToolResult,
) {
    if !crate::readiness_evidence_writes_enabled() {
        return;
    }
    let Some(harness_id) = telemetry.current_harness_id().await else {
        return;
    };
    let workspace_id = session.state().await.workspace_id;
    let Some(evidence) = successful_runtime_evidence(tool_name, result, workspace_id) else {
        return;
    };
    let outcome = match evidence {
        SuccessfulRuntimeEvidence::Grounded(source) => {
            mcp_client::harness_readiness::record_runtime_grounded(harness_id, source)
        }
        SuccessfulRuntimeEvidence::InferredPractice => {
            mcp_client::harness_readiness::record_inferred_runtime_practice(harness_id)
        }
    };
    match outcome {
        Ok(Some(_)) => client.spawn_harness_readiness_sync(),
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(
                tool = tool_name,
                error = %error,
                "Successful runtime evidence could not be recorded in the local readiness ledger"
            );
        }
    }
}

fn initialize_tool_count(registry: &ToolRegistry) -> usize {
    let mut tool_count = registry.len();
    if registry.is_router_mode() {
        tool_count += 2; // operations + execute_operation
    } else if registry.is_openai_agentic_surface() {
        tool_count += 3; // tool_search + execute_operation + batch_operations
    }
    tool_count
}

fn is_workflow_help_call(tool_name: &str, arguments: &serde_json::Value) -> bool {
    let (operation_name, operation_arguments) = if tool_name == "execute_operation" {
        (
            arguments
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default(),
            arguments
                .get("arguments")
                .unwrap_or(&serde_json::Value::Null),
        )
    } else {
        (tool_name, arguments)
    };

    operation_name == "help"
        && operation_arguments
            .get("action")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|action| action.eq_ignore_ascii_case("workflow"))
}

/// Build a legacy initialize result without overstating protocol support.
///
/// Runtime callers pass [`MCP_PROTOCOL_2024_11_05`], for which `instructions`
/// is intentionally absent. Newer legacy revisions may use this builder only
/// after their complete transport behavior is implemented and negotiated.
pub(crate) fn build_legacy_initialize_result(
    registry: &ToolRegistry,
    protocol_version: &str,
    params: &serde_json::Value,
) -> serde_json::Value {
    build_legacy_initialize_result_with_teaching(
        registry,
        protocol_version,
        params,
        crate::protocol_harness_teaching_enabled(),
    )
}

fn build_legacy_initialize_result_with_teaching(
    registry: &ToolRegistry,
    protocol_version: &str,
    params: &serde_json::Value,
    protocol_teaching_enabled: bool,
) -> serde_json::Value {
    let mut result = serde_json::json!({
        "protocolVersion": protocol_version,
        "capabilities": {
            "tools": {
                "listChanged": false
            }
        },
        "serverInfo": {
            "name": "contextstream-mcp",
            "version": mcp_types::config::VERSION
        },
        "toolCount": initialize_tool_count(registry)
    });

    if let Some(instructions) = protocol_teaching_enabled
        .then(|| {
            legacy_initialize_instructions(protocol_version, harness_from_initialize_params(params))
        })
        .flatten()
    {
        result
            .as_object_mut()
            .expect("initialize result is always an object")
            .insert(
                "instructions".to_string(),
                serde_json::Value::String(instructions),
            );
    }

    result
}

/// The finalized adapter is advertised only as one complete, tested unit.
/// Adding a future requirement starts false in [`StatelessMcpConformance`]
/// and therefore fails the discovery-teaching gate until both transports are
/// advanced together.
pub(crate) const MCP_2026_STATELESS_CONFORMANCE: StatelessMcpConformance =
    StatelessMcpConformance {
        server_discover: true,
        no_initialize_or_protocol_sessions: true,
        removed_legacy_core_methods: true,
        per_request_protocol_version: true,
        per_request_client_capabilities: true,
        response_server_identity: true,
        result_type: true,
        cacheable_results: true,
        http_header_routing: true,
    };

pub(crate) fn build_contextstream_stateless_discover_result() -> serde_json::Value {
    let teaching = stateless_discovery_teaching(
        MCP_PROTOCOL_2026_07_28,
        MCP_2026_STATELESS_CONFORMANCE,
        // Modern clientInfo is self-reported display/debugging metadata. The
        // 2026 schema says servers SHOULD NOT change behavior based on it, so
        // discovery guidance deliberately uses the client-neutral contract.
        None,
    )
    .expect("the advertised MCP 2026 adapter must remain fully conformant");
    let instructions =
        crate::protocol_harness_teaching_enabled().then_some(teaching.rendered_guidance);
    mcp_types::build_stateless_discover_result(instructions)
}

fn json_rpc_protocol_error(id: Option<serde_json::Value>, error: McpProtocolError) -> String {
    let mut error_object = serde_json::json!({
        "code": error.code,
        "message": error.message,
    });
    if let Some(data) = error.data {
        error_object
            .as_object_mut()
            .expect("protocol errors are objects")
            .insert("data".to_string(), data);
    }
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": error_object,
    })
    .to_string()
}

fn decorate_stateless_json_rpc_response(response: String, method: &str) -> String {
    if response.is_empty() {
        return response;
    }

    let Ok(mut envelope) = serde_json::from_str::<serde_json::Value>(&response) else {
        return response;
    };
    let Some(result) = envelope.get_mut("result") else {
        return response;
    };
    let original = std::mem::take(result);
    *result = match method {
        "tools/list" => decorate_stateless_cacheable_result(
            original,
            MCP_TOOLS_LIST_TTL_MS,
            McpCacheScope::Private,
        ),
        _ => decorate_stateless_result(original),
    };
    envelope.to_string()
}

/// Handle either the initialize-era wire contract or the finalized stateless
/// contract. Merely carrying a protocol-version metadata key opts the request
/// into strict validation; unsupported/future versions never fall back.
async fn handle_request(
    registry: &Arc<ToolRegistry>,
    client: &Arc<ContextStreamClient>,
    session: &Arc<SessionManager>,
    telemetry: &AgenticTelemetry,
    request: serde_json::Value,
    wire_observation: mcp_tools::wire_tokens::WireTokenObservation,
) -> String {
    let jsonrpc = request
        .get("jsonrpc")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let id = request.get("id").cloned();
    let method = request
        .get("method")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string();
    let params = request
        .get("params")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));

    if method == "server/discover" && id.is_none() {
        return json_rpc_error(None, -32600, "server/discover requires a request id");
    }

    let stateless_request = method == "server/discover" || has_stateless_protocol_metadata(&params);
    if stateless_request {
        if let Err(error) =
            validate_stateless_jsonrpc_envelope(jsonrpc, id.as_ref(), method.as_str())
        {
            return json_rpc_protocol_error(id, error);
        }
        if let Err(error) = validate_stateless_request(&params) {
            return json_rpc_protocol_error(id, error);
        }
        if let Err(error) = validate_stateless_method_params(&method, &params) {
            return json_rpc_protocol_error(id, error);
        }
        // The stateless tool surface is the configured server surface, never
        // a profile left behind by an earlier initialize-era client.
        registry.apply_initialize_surface_profile(None);
        if matches!(
            method.as_str(),
            "initialize"
                | "notifications/initialized"
                | "ping"
                | "logging/setLevel"
                | "resources/subscribe"
                | "resources/unsubscribe"
        ) {
            return json_rpc_error(
                id,
                -32601,
                &format!("Method not found: {method} is not part of MCP 2026-07-28"),
            );
        }

        if method == "tools/call" {
            let tool_name = params.get("name").and_then(serde_json::Value::as_str);
            let available = tool_name.is_some_and(|name| {
                registry.get(name).is_some()
                    || (registry.is_router_mode()
                        && matches!(
                            name,
                            "operations" | "execute_operation" | "batch_operations"
                        ))
                    || (registry.is_openai_agentic_surface()
                        && matches!(
                            name,
                            "tool_search" | "execute_operation" | "batch_operations"
                        ))
            });
            if !available {
                return json_rpc_error(id, -32602, "Unknown or missing tool name");
            }
            if let Err(error) = validate_contextstream_meta_tool_arguments(
                tool_name.expect("available tools/call has a name"),
                params.get("arguments").unwrap_or(&serde_json::Value::Null),
            ) {
                return json_rpc_protocol_error(id, error);
            }
        }
    }

    let response = if stateless_request {
        // The outer stdio transport has a process-local bucket for legacy
        // clients. Modern requests override it with a non-cacheable,
        // request-unique bucket and discard that bucket after dispatch, so no
        // tool can rely on hidden cross-request transport state.
        let transient_key = SessionKey::for_anonymous_http(&format!(
            "stdio-stateless-request:{}",
            Uuid::new_v4().simple()
        ));
        let response = run_with_session_key(transient_key.clone(), || async {
            handle_request_dispatch(
                registry,
                client,
                session,
                telemetry,
                request,
                wire_observation,
                true,
            )
            .await
        })
        .await;
        session.discard_transient_state(&transient_key);
        response
    } else {
        handle_request_dispatch(
            registry,
            client,
            session,
            telemetry,
            request,
            wire_observation,
            false,
        )
        .await
    };
    if stateless_request {
        decorate_stateless_json_rpc_response(response, &method)
    } else {
        response
    }
}

/// Dispatch a validated JSON-RPC request through the existing tool surface.
async fn handle_request_dispatch(
    registry: &Arc<ToolRegistry>,
    client: &Arc<ContextStreamClient>,
    session: &Arc<SessionManager>,
    telemetry: &AgenticTelemetry,
    request: serde_json::Value,
    wire_observation: mcp_tools::wire_tokens::WireTokenObservation,
    strict_protocol_errors: bool,
) -> String {
    let id = request.get("id").cloned();
    let method = request.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = request
        .get("params")
        .cloned()
        .unwrap_or(serde_json::json!({}));

    debug!("Handling request: method={}", method);

    match method {
        "server/discover" => json_rpc_result(id, build_contextstream_stateless_discover_result()),

        "initialize" => {
            // Deterministic per-`initialize`: apply the detected profile, or
            // fall back to the registry default so a prior client's
            // auto-detected narrowing can't persist (global-state bleed).
            registry
                .apply_initialize_surface_profile(surface_profile_from_initialize_params(&params));
            telemetry.update_initialize_hints(&params).await;
            let result = build_legacy_initialize_result(registry, MCP_PROTOCOL_2024_11_05, &params);
            if crate::readiness_evidence_writes_enabled() {
                let managed_harness = std::env::var("CONTEXTSTREAM_CLIENT")
                    .ok()
                    .and_then(|value| HarnessId::from_alias(&value));
                let exact_harness =
                    crate::agentic_telemetry::exact_initialize_harness(&params, managed_harness);
                let outcome = match exact_harness {
                    Some(harness_id) => {
                        mcp_client::harness_readiness::record_runtime_connected(harness_id)
                    }
                    None => Ok(None),
                };
                match outcome {
                    Ok(Some(_)) => client.spawn_harness_readiness_sync(),
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(
                            error = %error,
                            "MCP initialize succeeded, but local connection evidence could not be recorded"
                        );
                    }
                }
            }
            json_rpc_result(id, result)
        }

        "notifications/initialized" => {
            // Client initialized notification - no response needed for notifications
            String::new()
        }

        "tools/list" => {
            let tools = contextstream_tools_list(registry, None);
            json_rpc_result(id, serde_json::json!({ "tools": tools }))
        }

        "tools/call" => {
            // Call a tool
            let tool_name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let arguments = params
                .get("arguments")
                .cloned()
                .unwrap_or(serde_json::json!({}));
            let include_workflow_contract = is_workflow_help_call(tool_name, &arguments);

            // Auto-init: if session isn't initialized and this isn't the init tool,
            // automatically trigger session init. This is the biggest difference-maker
            // for editors actually using ContextStream effectively.
            if tool_name != "init" && tool_name != "context" && !session.is_initialized().await {
                debug!(
                    "Auto-init: session not initialized, triggering init before '{}'",
                    tool_name
                );
                if let Some(init_tool) = registry.get("init") {
                    // Build init params from the tool's arguments if they contain
                    // folder_path, otherwise infer from the process cwd.
                    // Track whether the caller *explicitly* supplied a folder vs.
                    // falling back to the process cwd. We still pass a cwd-derived
                    // folder to init (so session scope can resolve), but a
                    // cwd-derived root must NOT trigger auto-indexing: the cwd may
                    // be $HOME, a home ancestor, `/`, or a sensitive dir.
                    let explicit_folder = arguments
                        .get("folder_path")
                        .or_else(|| arguments.get("path"))
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    let explicitly_supplied = explicit_folder.is_some();

                    let folder_path = explicit_folder.or_else(|| {
                        std::env::current_dir()
                            .ok()
                            .map(|p| p.to_string_lossy().to_string())
                    });

                    let auto_index =
                        should_auto_init_index(folder_path.as_deref(), explicitly_supplied);

                    let init_args = serde_json::json!({
                        "folder_path": folder_path,
                        "auto_index": auto_index
                    });

                    match init_tool.handler.execute(init_args).await {
                        Ok(_) => debug!("Auto-init completed successfully"),
                        Err(e) => debug!("Auto-init failed (non-fatal): {}", e),
                    }
                }
            }

            // Handle router mode meta-operations
            if registry.is_router_mode() {
                match tool_name {
                    "operations" => {
                        return handle_router_operations(id, registry, arguments);
                    }
                    "execute_operation" => {
                        let context = mcp_tools::wire_tokens::WireResponseContext::stdio_jsonrpc(
                            id.clone(),
                            None,
                            None,
                        )
                        .with_structured_content(include_workflow_contract)
                        .with_observation(wire_observation.clone());
                        return mcp_tools::wire_tokens::run_with_wire_response_context(
                            context,
                            handle_router_execute(
                                id,
                                registry,
                                client,
                                session,
                                arguments,
                                telemetry,
                                registry.tool_surface_profile(),
                            ),
                        )
                        .await;
                    }
                    "batch_operations" => {
                        return handle_batch_operations(
                            id,
                            registry,
                            arguments,
                            telemetry,
                            registry.tool_surface_profile(),
                        )
                        .await;
                    }
                    _ => {}
                }
            }

            if registry.is_openai_agentic_surface() {
                match tool_name {
                    "tool_search" => {
                        return handle_tool_search(
                            id,
                            registry,
                            arguments,
                            telemetry,
                            registry.tool_surface_profile(),
                        )
                        .await;
                    }
                    "execute_operation" => {
                        let context = mcp_tools::wire_tokens::WireResponseContext::stdio_jsonrpc(
                            id.clone(),
                            None,
                            None,
                        )
                        .with_structured_content(include_workflow_contract)
                        .with_observation(wire_observation.clone());
                        return mcp_tools::wire_tokens::run_with_wire_response_context(
                            context,
                            handle_router_execute(
                                id,
                                registry,
                                client,
                                session,
                                arguments,
                                telemetry,
                                registry.tool_surface_profile(),
                            ),
                        )
                        .await;
                    }
                    "batch_operations" => {
                        return handle_batch_operations(
                            id,
                            registry,
                            arguments,
                            telemetry,
                            registry.tool_surface_profile(),
                        )
                        .await;
                    }
                    _ => {}
                }

                if registry.get(tool_name).is_none() && registry.get_operation(tool_name).is_some()
                {
                    telemetry
                        .emit_hidden_direct_call_blocked(
                            registry.tool_surface_profile(),
                            tool_name,
                            AgenticTelemetryInput::from_arguments(&arguments),
                        )
                        .await;
                    return json_rpc_result(
                        id,
                        serde_json::json!({
                            "content": [{
                                "type": "text",
                                "text": format!(
                                    "[ERROR] '{}' is hidden on the adaptive surface. Call tool_search(query=\"...\") first, then execute_operation(name=\"{}\", arguments={{...}}).",
                                    tool_name,
                                    tool_name
                                )
                            }],
                            "isError": true
                        }),
                    );
                }
            }

            let call_title = registry.get(tool_name).map(|tool| {
                contextstream_call_title(
                    &tool.metadata.name,
                    &tool.metadata.title,
                    tool.metadata.annotations.read_only,
                    &arguments,
                )
            });

            // Tool-use quality telemetry: time the call and emit a per-tool-call
            // event so the Agentic dashboard reflects direct (stdio) tool use —
            // tool mix, latency, and error rate — not just the adaptive surface.
            let tool_call_input = AgenticTelemetryInput::from_arguments(&arguments);
            let tool_call_started = std::time::Instant::now();
            let wire_context = mcp_tools::wire_tokens::WireResponseContext::stdio_jsonrpc(
                id.clone(),
                call_title.clone(),
                Some(contextstream_call_icon(tool_name).to_string()),
            )
            .with_structured_content(include_workflow_contract)
            .with_observation(wire_observation);
            let tool_call_outcome = mcp_tools::wire_tokens::run_with_wire_response_context(
                wire_context,
                registry.execute(tool_name, arguments),
            )
            .await;
            let tool_call_latency_ms = tool_call_started.elapsed().as_millis() as u64;
            telemetry
                .emit_tool_call(
                    tool_name,
                    tool_call_latency_ms,
                    tool_call_outcome
                        .as_ref()
                        .map(|result| result.is_error)
                        .unwrap_or(true),
                    &tool_call_input,
                )
                .await;

            match tool_call_outcome {
                Ok(result) => {
                    record_successful_runtime_evidence(
                        client, session, telemetry, tool_name, &result,
                    )
                    .await;
                    let context = mcp_tools::wire_tokens::WireResponseContext::stdio_jsonrpc(
                        id.clone(),
                        call_title.clone(),
                        Some(contextstream_call_icon(tool_name).to_string()),
                    )
                    .with_structured_content(include_workflow_contract);
                    let payload = mcp_tools::wire_tokens::tool_result_payload(&result, &context);
                    json_rpc_result(id, payload)
                }
                Err(e) => {
                    if strict_protocol_errors
                        && matches!(
                            &e,
                            mcp_types::Error::Validation(_)
                                | mcp_types::Error::InvalidUuid(_)
                                | mcp_types::Error::Serialization(_)
                        )
                    {
                        return json_rpc_error(id, -32602, &e.user_facing_message());
                    }
                    let mut result = match &e {
                        mcp_types::Error::InsufficientCredits {
                            required,
                            available,
                            ..
                        } => {
                            let rich = mcp_types::tool::ToolResult::credits_exhausted(
                                *required, *available,
                            );
                            let content: Vec<serde_json::Value> = rich.content.iter().map(|c| match c {
                                mcp_types::tool::ContentItem::Text { text } => serde_json::json!({"type": "text", "text": text}),
                                mcp_types::tool::ContentItem::Image { data, mime_type } => serde_json::json!({"type": "image", "data": data, "mimeType": mime_type}),
                                mcp_types::tool::ContentItem::Resource { uri, mime_type } => {
                                    let mut r = serde_json::json!({"type": "resource", "uri": uri});
                                    if let Some(mt) = mime_type { r["mimeType"] = serde_json::json!(mt); }
                                    r
                                }
                            }).collect();
                            serde_json::json!({
                                "content": content,
                                "isError": true
                            })
                        }
                        _ => {
                            if e.is_non_blocking_parser_error() {
                                tracing::debug!(
                                    error = %e,
                                    "suppressed non-blocking ParserError in stdio tool response"
                                );
                            }
                            serde_json::json!({
                                "content": [{
                                    "type": "text",
                                    "text": format!("[ERROR] {}", e.user_facing_message())
                                }],
                                "isError": true
                            })
                        }
                    };
                    if let Some(title) = call_title.as_ref() {
                        result["title"] = serde_json::Value::String(title.clone());
                        result["_meta"] = serde_json::json!({
                            "contextstream": {
                                "title": title,
                                "icon": contextstream_call_icon(tool_name),
                            }
                        });
                    }
                    json_rpc_result(id, result)
                }
            }
        }

        "ping" => json_rpc_result(id, serde_json::json!({})),

        _ => {
            // Unknown method
            if method.starts_with("notifications/") {
                // Notifications don't need responses
                String::new()
            } else {
                json_rpc_error(id, -32601, &format!("Method not found: {}", method))
            }
        }
    }
}

/// Decide whether the stdio auto-init should enable background auto-indexing.
///
/// P0 ingestion-containment (defense-in-depth): auto-indexing is only enabled
/// when the caller *explicitly* supplied a folder (via `folder_path`/`path`)
/// AND that folder clears the ingest-containment guard. A folder derived from
/// the process cwd (`explicitly_supplied == false`) may be `$HOME`, a home
/// ancestor, `/`, or a sensitive dir, so it always yields `auto_index: false`.
/// The ingest layer re-checks the root; the operator env opt-in
/// (`CONTEXTSTREAM_ALLOW_BROAD_INGEST=1`) relaxes the guard.
fn should_auto_init_index(folder_path: Option<&str>, explicitly_supplied: bool) -> bool {
    if !explicitly_supplied {
        return false;
    }
    match folder_path {
        Some(path) => mcp_client::validate_ingest_root(
            std::path::Path::new(path),
            &mcp_client::IngestRootOptions::from_env(),
        )
        .is_ok(),
        None => false,
    }
}

fn surface_profile_from_initialize_params(
    params: &serde_json::Value,
) -> Option<ToolSurfaceProfile> {
    // Explicit opt-in always wins (e.g. the agentic eval passes
    // `tool_surface_profile` in the initialize params; Copilot uses the
    // CONTEXTSTREAM_TOOL_SURFACE_PROFILE env / header).
    let explicit = params
        .get("tool_surface_profile")
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<ToolSurfaceProfile>().ok());
    if explicit.is_some() {
        return explicit;
    }

    // Infer the surface ONLY from the client/provider *identity*, never from
    // the model name. The model a client runs (e.g. `gpt-5`, `gpt-5.5`,
    // `gpt-5-codex`) does not determine whether its MCP transport can drive
    // the full tool surface. Codex/Fugu run gpt-5* models but are ordinary
    // full-surface clients; matching the `model` field against `"gpt-5"`
    // previously narrowed them to the 13-tool adaptive surface and hid
    // `search`/`memory`/`project`/etc behind discovery meta-tools, so direct
    // calls failed with "unsupported call: <tool>".
    let combined = [
        params.get("client_name").and_then(|v| v.as_str()),
        params.get("provider").and_then(|v| v.as_str()),
        params
            .get("clientInfo")
            .and_then(|v| v.get("name"))
            .and_then(|v| v.as_str()),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ")
    .to_ascii_lowercase();

    if combined.contains("chatgpt") || combined.contains("openai") || combined.contains("responses")
    {
        Some(ToolSurfaceProfile::OpenaiAgentic)
    } else {
        None
    }
}

fn json_rpc_result(id: Option<serde_json::Value>, result: serde_json::Value) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result
    })
    .to_string()
}

fn json_rpc_error(id: Option<serde_json::Value>, code: i32, message: &str) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message
        }
    })
    .to_string()
}

// ============================================================================
// Router Mode Handlers
// ============================================================================

/// Handle router mode `operations` call.
fn handle_router_operations(
    id: Option<serde_json::Value>,
    registry: &Arc<ToolRegistry>,
    arguments: serde_json::Value,
) -> String {
    let category = arguments.get("category").and_then(|c| c.as_str());
    let format = arguments
        .get("format")
        .and_then(|f| f.as_str())
        .unwrap_or("grouped");

    let operations = registry.list_operations();

    // Filter by category if specified
    let filtered: Vec<_> = operations
        .iter()
        .filter(|op| {
            if let Some(cat) = category {
                op.metadata.category.as_str().eq_ignore_ascii_case(cat)
            } else {
                true
            }
        })
        .collect();

    let result = match format {
        "minimal" => {
            let names: Vec<&str> = filtered
                .iter()
                .map(|op| op.metadata.name.as_str())
                .collect();
            serde_json::json!({
                "operations": names,
                "count": names.len()
            })
        }
        "full" => {
            let ops: Vec<serde_json::Value> = filtered
                .iter()
                .map(|op| {
                    serde_json::json!({
                        "name": op.metadata.name,
                        "description": op.metadata.description,
                        "category": op.metadata.category.as_str(),
                        "inputSchema": op.input_schema
                    })
                })
                .collect();
            serde_json::json!({
                "operations": ops,
                "count": ops.len()
            })
        }
        _ => {
            // Grouped by category
            let mut groups: std::collections::HashMap<String, Vec<serde_json::Value>> =
                std::collections::HashMap::new();

            for op in &filtered {
                let cat = op.metadata.category.as_str().to_string();
                groups.entry(cat).or_default().push(serde_json::json!({
                    "name": op.metadata.name,
                    "description": op.metadata.description
                }));
            }

            serde_json::json!({
                "operations": groups,
                "count": filtered.len()
            })
        }
    };

    let text = serde_json::to_string_pretty(&result).unwrap_or_default();
    json_rpc_result(
        id,
        serde_json::json!({
            "content": [{
                "type": "text",
                "text": text
            }],
            "isError": false
        }),
    )
}

async fn handle_tool_search(
    id: Option<serde_json::Value>,
    registry: &Arc<ToolRegistry>,
    arguments: serde_json::Value,
    telemetry: &AgenticTelemetry,
    surface_profile: ToolSurfaceProfile,
) -> String {
    let started = Instant::now();
    let telemetry_input = AgenticTelemetryInput::from_arguments(&arguments);
    let query = match arguments.get("query").and_then(|q| q.as_str()) {
        Some(query) if !query.trim().is_empty() => query.trim(),
        _ => {
            return json_rpc_result(
                id,
                serde_json::json!({
                    "content": [{
                        "type": "text",
                        "text": "[ERROR] Missing 'query' parameter"
                    }],
                    "isError": true
                }),
            );
        }
    };

    let category = arguments.get("category").and_then(|c| c.as_str());
    let limit = arguments
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(8);

    let matches = registry.search_catalog(query, category, limit);
    telemetry
        .emit_tool_search(
            surface_profile,
            query,
            matches.len(),
            started.elapsed(),
            telemetry_input,
        )
        .await;
    let text = serde_json::to_string_pretty(&serde_json::json!({
        "query": query,
        "count": matches.len(),
        "matches": matches
    }))
    .unwrap_or_default();

    json_rpc_result(
        id,
        serde_json::json!({
            "content": [{
                "type": "text",
                "text": text
            }],
            "isError": false
        }),
    )
}

async fn handle_batch_operations(
    id: Option<serde_json::Value>,
    registry: &Arc<ToolRegistry>,
    arguments: serde_json::Value,
    telemetry: &AgenticTelemetry,
    surface_profile: ToolSurfaceProfile,
) -> String {
    let started = Instant::now();
    let telemetry_input = AgenticTelemetryInput::from_arguments(&arguments);
    let operations = match arguments.get("operations").and_then(|ops| ops.as_array()) {
        Some(ops) if !ops.is_empty() => ops,
        _ => {
            return json_rpc_result(
                id,
                serde_json::json!({
                    "content": [{
                        "type": "text",
                        "text": "[ERROR] Missing or empty 'operations' array"
                    }],
                    "isError": true
                }),
            );
        }
    };

    if let Some(name) = batch_operation_requiring_direct_wire_accounting(&arguments) {
        let metric_operation = match name {
            "context" => "context",
            _ => "search",
        };
        metrics::counter!(
            "mcp_wire_tokenizer_batch_rejected_total",
            "transport" => "stdio",
            "operation" => metric_operation,
        )
        .increment(1);
        return json_rpc_result(
            id,
            serde_json::json!({
                "content": [{
                    "type": "text",
                    "text": batch_wire_accounting_rejection_message(name)
                }],
                "isError": true
            }),
        );
    }

    let mut results = Vec::with_capacity(operations.len());
    let mut operation_names = Vec::with_capacity(operations.len());
    for op in operations {
        let Some(name) = op.get("name").and_then(|v| v.as_str()) else {
            return json_rpc_result(
                id,
                serde_json::json!({
                    "content": [{
                        "type": "text",
                        "text": "[ERROR] Each operation requires a 'name'"
                    }],
                    "isError": true
                }),
            );
        };
        operation_names.push(name.to_string());

        let args = op
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));

        if let Some(tool) = registry.get(name) {
            if !tool.metadata.annotations.read_only || tool.metadata.annotations.destructive {
                return json_rpc_result(
                    id,
                    serde_json::json!({
                        "content": [{
                            "type": "text",
                            "text": format!("[ERROR] '{}' is not eligible for batch_operations because it is not read-only.", name)
                        }],
                        "isError": true
                    }),
                );
            }
            match tool.handler.execute(args).await {
                Ok(result) => results.push(serde_json::json!({
                    "name": name,
                    "call_mode": "direct",
                    "result": result
                })),
                Err(err) => {
                    telemetry
                        .emit_batch_operations(
                            surface_profile,
                            &operation_names,
                            started.elapsed(),
                            results.len(),
                            telemetry_input.clone(),
                        )
                        .await;
                    if err.is_non_blocking_parser_error() {
                        tracing::debug!(
                            error = %err,
                            "suppressed non-blocking ParserError in stdio batch response"
                        );
                    }
                    return json_rpc_result(
                        id,
                        serde_json::json!({
                            "content": [{
                                "type": "text",
                                "text": format!("[ERROR] {} failed: {}", name, err.user_facing_message())
                            }],
                            "isError": true
                        }),
                    );
                }
            }
        } else if let Some(operation) = registry.get_operation(name) {
            if !operation.metadata.annotations.read_only
                || operation.metadata.annotations.destructive
            {
                return json_rpc_result(
                    id,
                    serde_json::json!({
                        "content": [{
                            "type": "text",
                            "text": format!("[ERROR] '{}' is not eligible for batch_operations because it is not read-only.", name)
                        }],
                        "isError": true
                    }),
                );
            }
            match registry.execute_operation(name, args).await {
                Ok(result) => results.push(serde_json::json!({
                    "name": name,
                    "call_mode": "execute_operation",
                    "result": result
                })),
                Err(err) => {
                    telemetry
                        .emit_batch_operations(
                            surface_profile,
                            &operation_names,
                            started.elapsed(),
                            results.len(),
                            telemetry_input.clone(),
                        )
                        .await;
                    if err.is_non_blocking_parser_error() {
                        tracing::debug!(
                            error = %err,
                            "suppressed non-blocking ParserError in stdio batch response"
                        );
                    }
                    return json_rpc_result(
                        id,
                        serde_json::json!({
                            "content": [{
                                "type": "text",
                                "text": format!("[ERROR] {} failed: {}", name, err.user_facing_message())
                            }],
                            "isError": true
                        }),
                    );
                }
            }
        } else {
            return json_rpc_result(
                id,
                serde_json::json!({
                    "content": [{
                        "type": "text",
                        "text": format!("[ERROR] Unknown tool or operation: {}", name)
                    }],
                    "isError": true
                }),
            );
        }
    }

    telemetry
        .emit_batch_operations(
            surface_profile,
            &operation_names,
            started.elapsed(),
            results.len(),
            telemetry_input,
        )
        .await;
    json_rpc_result(
        id,
        serde_json::json!({
            "content": [{
                "type": "text",
                "text": format!("Executed {} batched read-only operations.", results.len())
            }],
            "structuredContent": {
                "count": results.len(),
                "results": results
            },
            "isError": false
        }),
    )
}

/// Context/search own per-call semantic budgets and exact-token rollout
/// decisions. Combining either inside `batch_operations` has no unambiguous
/// single target for the outer response, so fail closed before any operation
/// executes. Direct calls and `execute_operation` both install a concrete wire
/// transport and remain supported.
pub(crate) fn batch_operation_requiring_direct_wire_accounting(
    arguments: &serde_json::Value,
) -> Option<&str> {
    arguments
        .get("operations")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|operation| operation.get("name").and_then(serde_json::Value::as_str))
        .find(|name| matches!(*name, "context" | "search"))
}

pub(crate) fn batch_wire_accounting_rejection_message(name: &str) -> String {
    format!(
        "[ERROR] '{name}' requires a direct tool call (or execute_operation) so ContextStream can enforce one exact whole-wire token budget; it is not eligible for batch_operations."
    )
}

/// Handle router mode `execute_operation` call.
async fn handle_router_execute(
    id: Option<serde_json::Value>,
    registry: &Arc<ToolRegistry>,
    client: &ContextStreamClient,
    session: &SessionManager,
    arguments: serde_json::Value,
    telemetry: &AgenticTelemetry,
    surface_profile: ToolSurfaceProfile,
) -> String {
    let started = Instant::now();
    let telemetry_input = AgenticTelemetryInput::from_arguments(&arguments);
    let op_name = match arguments.get("name").and_then(|n| n.as_str()) {
        Some(n) => n,
        None => {
            return json_rpc_result(
                id,
                serde_json::json!({
                    "content": [{
                        "type": "text",
                        "text": "[ERROR] Missing 'name' parameter"
                    }],
                    "isError": true
                }),
            );
        }
    };

    let op_args = arguments
        .get("arguments")
        .cloned()
        .unwrap_or(serde_json::json!({}));
    let query = op_args
        .get("query")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    match registry.execute_operation(op_name, op_args).await {
        Ok(result) => {
            record_successful_runtime_evidence(client, session, telemetry, op_name, &result).await;
            telemetry
                .emit_execute_operation(
                    surface_profile,
                    op_name,
                    query.as_deref(),
                    started.elapsed(),
                    Some(&result),
                    telemetry_input,
                )
                .await;
            let context = mcp_tools::wire_tokens::current_wire_response_context();
            json_rpc_result(
                id,
                mcp_tools::wire_tokens::tool_result_payload(&result, &context),
            )
        }
        Err(e) => {
            telemetry
                .emit_execute_operation(
                    surface_profile,
                    op_name,
                    query.as_deref(),
                    started.elapsed(),
                    None,
                    telemetry_input,
                )
                .await;
            if e.is_non_blocking_parser_error() {
                tracing::debug!(
                    error = %e,
                    "suppressed non-blocking ParserError in stdio execute_operation response"
                );
            }
            json_rpc_result(
                id,
                serde_json::json!({
                    "content": [{
                        "type": "text",
                        "text": format!("[ERROR] {}", e.user_facing_message())
                    }],
                    "isError": true
                }),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_registry(mut config: Config) -> ToolRegistry {
        config.api_key = Some("test-api-key".to_string());
        let client = ContextStreamClient::new(config.clone());
        let session = Arc::new(SessionManager::new(client.clone(), config.clone()));
        build_registry(&config, client, session)
    }

    fn tool_by_name<'a>(tools: &'a [serde_json::Value], name: &str) -> &'a serde_json::Value {
        tools
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("missing tool {name}"))
    }

    #[test]
    fn only_real_successes_are_eligible_for_runtime_readiness() {
        use mcp_types::tool::ToolResult;

        assert_eq!(
            successful_runtime_evidence(
                "init",
                &ToolResult::text("initialized"),
                Some(Uuid::parse_str("00000000-0000-4000-8000-000000000042").unwrap())
            ),
            Some(SuccessfulRuntimeEvidence::Grounded(
                ReadinessEvidenceSource::InitTool
            ))
        );
        assert_eq!(
            successful_runtime_evidence("init", &ToolResult::text("initialized"), None),
            None,
            "verified grounding requires a resolved caller-partitioned session scope"
        );
        assert_eq!(
            successful_runtime_evidence(
                "context",
                &ToolResult::text("grounded"),
                Some(Uuid::nil())
            ),
            None,
            "a nil scope identifier is not grounding evidence"
        );
        assert_eq!(
            successful_runtime_evidence(
                "context",
                &ToolResult::text("[AUTH_REQUIRED] Authentication required"),
                Some(Uuid::new_v4())
            ),
            None
        );
        assert_eq!(
            successful_runtime_evidence(
                "search",
                &ToolResult::text("[SETUP_REQUIRED] Run setup first"),
                Some(Uuid::new_v4())
            ),
            None
        );
        assert_eq!(
            successful_runtime_evidence("search", &ToolResult::text("Found 3 results"), None),
            Some(SuccessfulRuntimeEvidence::InferredPractice)
        );
        assert_eq!(
            successful_runtime_evidence(
                "tool_search",
                &ToolResult::text("Found 3 tools"),
                Some(Uuid::new_v4())
            ),
            None,
            "catalog discovery is not evidence of code-search-first behavior"
        );
        assert_eq!(
            successful_runtime_evidence(
                "search",
                &ToolResult::error("backend unavailable"),
                Some(Uuid::new_v4())
            ),
            None
        );
    }

    const V0_5_62_BROAD_TOOL_NAMES: &[&str] = &[
        "capsule",
        "capture_plan",
        "context",
        "coordination",
        "entity",
        "graph",
        "help",
        "init",
        "instruct",
        "integration",
        "media",
        "memory",
        "memory_complete_todo",
        "memory_create_doc",
        "memory_create_event",
        "memory_create_task",
        "memory_create_todo",
        "memory_delete_doc",
        "memory_update_doc",
        "memory_update_task",
        "project",
        "qa",
        "reminder",
        "search",
        "session",
        "session_capture",
        "session_capture_lesson",
        "session_remember",
        "skill",
        "vcs",
        "workspace",
    ];

    const V0_5_62_ROUTER_TOOL_NAMES: &[&str] = &["execute_operation", "operations"];

    const V0_5_62_OPENAI_AGENTIC_TOOL_NAMES: &[&str] = &[
        "batch_operations",
        "capsule",
        "context",
        "execute_operation",
        "help",
        "init",
        "instruct",
        "media",
        "memory",
        "project",
        "search",
        "session",
        "tool_search",
        "workspace",
    ];

    // SHA-256 of each v0.5.62 broad-surface input schema after recursively
    // removing prose-only `description` fields and canonicalizing object key
    // order. This keeps copy improvements compatible while detecting removed
    // parameters, changed types/required fields, and enum drift.
    // Current broad-tool input contracts. Most entries remain the v0.5.62
    // baseline; entries intentionally advance when an additive, versioned
    // input capability is added.
    const EXPECTED_BROAD_SCHEMA_CONTRACTS: &[(&str, &str)] = &[
        (
            "capsule",
            "c680655059bf1f41e916674fe9610f1e6947f7fdda1a833ac9624770eb6b015a",
        ),
        (
            "capture_plan",
            "5476f8dce574bfa5be96e6cfb9d8a9acba4a401114aaa4f61a2e78af12d90dbc",
        ),
        (
            "context",
            "30f657e3f262c2fde96ce544d7dc78e3446a51dce8eebe19c6f5051793ba62e9",
        ),
        (
            "coordination",
            "e943310bd255e0b99063e09c232fff3e6a00c537f4d0191eb2fc668f33f57934",
        ),
        (
            "entity",
            "8d09ed3df23786cc510692f6d7be167c1836a2b52873f9ce7dc232870d52f0a9",
        ),
        (
            "graph",
            "8badfe1e6b204a8ece5fc2f664aaa25a05c035841faf4dce63e2c49e5e742f76",
        ),
        (
            "help",
            "a74422b93ba5f0b6c7119bb62e601bcab5951f6a6a7c69d80d9a672c9b7aaaf3",
        ),
        (
            "init",
            "857e6c70edddd7cff29738d6baeb7df1010997dbdfade8fae583bfdcb300b37f",
        ),
        (
            "instruct",
            "13c56d2c81bf7f87fbe8f60dbd639ad101ff5770f2f1bbdf1bda146c0c5663c1",
        ),
        (
            "integration",
            "55981cfc657ff54f030c0a0b15411709455f18ba74e0c71fa7c30b8e7ebe9f33",
        ),
        (
            "media",
            "00334ef5436ef083b374795f098791bb29e4fdffe7bf4bf252c92c05cf7ca13d",
        ),
        (
            "memory",
            "e2b4682203559a4240f4980ac2571a52edcbaaefee8adf575ba2510e2424b3f6",
        ),
        (
            "memory_complete_todo",
            "7f12a11995cb407642794e100fd62d71c2018c0b90691c1e680c6618b1234ac3",
        ),
        (
            "memory_create_doc",
            "d5c7af3b559d5b6e8126c6cea7d67a274224d69aa389eb990a15590eb1d94def",
        ),
        (
            "memory_create_event",
            "41b0dd9c8591d7edcd3ccb7a4205aeb719e6c5e8d632a196e470a89e7d620cdd",
        ),
        (
            "memory_create_task",
            "60559ff57a59db8fb9c3d0cc3a2697ef964d447c7fd3c77bec1953b616995794",
        ),
        (
            "memory_create_todo",
            "ecd98039e145cbc1dd0815eb506a8ad66216398e38b436c067ee556380cf18fa",
        ),
        (
            "memory_delete_doc",
            "a7e9172ec0cb3d3f14889a92bf1d0c196d7d345647a04b3b7964e46485a5a7c1",
        ),
        (
            "memory_update_doc",
            "7725b8a5114fa31238e2d7a7f57fc12a91d612ebcee08ac5312c98a82e8124dc",
        ),
        (
            "memory_update_task",
            "75339a9a7858741f90dbe3109e10ee6aa2b82648c2594fe080e5f033c21387ae",
        ),
        (
            "project",
            "5e48f6a08af6a5c6c933e3552fb47b49917cfc2a75273486158d3b8454f8afea",
        ),
        (
            "qa",
            "1539c9c06ffa32bb2d362a5c1e034ad23bccb919dce8f160ce60758e278f4e78",
        ),
        (
            "reminder",
            "4786b35cf4e5fca2d5d1e38fe0602ac7f81ae552d3594bb2c03c9e44efccbfc2",
        ),
        (
            "search",
            "2a4168ef3625b67f01478562408bdacb6400c66ee6a23d8cf6ecfb2df3dba336",
        ),
        (
            "session",
            "eba8ac7837f0d9a82a13928ac4f893307a2c4cd579e82cbf53525eaa54a7dcda",
        ),
        (
            "session_capture",
            "54a4a2e146e231cc03898aba8ab163a596c12bb9428616ebfb9a49ea263e8e4e",
        ),
        (
            "session_capture_lesson",
            "f6b4ffcb8eaa3f111352fa810d819847d36bd885a21b3eec6a1f4641396e5085",
        ),
        (
            "session_remember",
            "fe47bd9f48eb0c15ab0cf223f0210dddb6af84ae8ba1531e88f2b3a4d93ae4ca",
        ),
        (
            "skill",
            "bf7ec63a561c62abff91b06fa92da4f752bda856b7f1786848a80ffb9efeffcc",
        ),
        (
            "vcs",
            "24082880cc8144907b2ae49f4aa54e22436278d9fa813cb783f8ba753a34d371",
        ),
        (
            "workspace",
            "29c375c10c2a2d8e131ec3765bcb1e2fa90e3e2ef8fe28aa6518c24c147562b6",
        ),
    ];

    fn listed_names(tools: &[serde_json::Value]) -> Vec<&str> {
        tools
            .iter()
            .map(|tool| tool["name"].as_str().expect("tool name"))
            .collect()
    }

    fn canonical_schema_contract(value: &serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(object) => {
                let mut entries: Vec<_> = object
                    .iter()
                    .filter(|(key, _)| key.as_str() != "description")
                    .collect();
                entries.sort_by_key(|(key, _)| *key);

                let mut canonical = serde_json::Map::new();
                for (key, value) in entries {
                    canonical.insert(key.clone(), canonical_schema_contract(value));
                }
                serde_json::Value::Object(canonical)
            }
            serde_json::Value::Array(values) => {
                serde_json::Value::Array(values.iter().map(canonical_schema_contract).collect())
            }
            scalar => scalar.clone(),
        }
    }

    fn schema_contract_hash(schema: &serde_json::Value) -> String {
        use sha2::{Digest, Sha256};
        use std::fmt::Write;

        let canonical = canonical_schema_contract(schema);
        let digest = Sha256::digest(
            serde_json::to_vec(&canonical).expect("canonical schema must serialize"),
        );
        let mut encoded = String::with_capacity(digest.len() * 2);
        for byte in digest {
            write!(&mut encoded, "{byte:02x}").expect("writing to a string cannot fail");
        }
        encoded
    }

    #[test]
    fn initialize_teaching_is_legacy_revision_gated_and_client_aware() {
        let registry = test_registry(Config::default());
        let codex = serde_json::json!({
            "clientInfo": {
                "name": "codex-cli",
                "version": "1.2.3"
            }
        });

        let current = build_legacy_initialize_result_with_teaching(
            &registry,
            MCP_PROTOCOL_2024_11_05,
            &codex,
            true,
        );
        assert_eq!(current["protocolVersion"], MCP_PROTOCOL_2024_11_05);
        assert!(
            current.get("instructions").is_none(),
            "2024 initialize must not emit an undefined instructions field"
        );

        let supported = build_legacy_initialize_result_with_teaching(
            &registry,
            mcp_types::MCP_PROTOCOL_2025_06_18,
            &codex,
            true,
        );
        let instructions = supported["instructions"]
            .as_str()
            .expect("2025 legacy instructions");
        assert!(instructions.contains(mcp_types::HARNESS_TEACHING_VERSION));
        assert!(instructions.contains("`init("));
        assert!(!instructions.contains("mcp__contextstream__"));

        let claude = build_legacy_initialize_result_with_teaching(
            &registry,
            mcp_types::MCP_PROTOCOL_2025_03_26,
            &serde_json::json!({"clientInfo": {"name": "Claude-Code/1.0"}}),
            true,
        );
        assert!(claude["instructions"]
            .as_str()
            .is_some_and(|value| value.contains("mcp__contextstream__init")));

        let stateless = build_legacy_initialize_result_with_teaching(
            &registry,
            mcp_types::MCP_PROTOCOL_2026_07_28,
            &serde_json::json!({"clientInfo": {"name": "unknown-wrapper"}}),
            true,
        );
        assert!(
            stateless.get("instructions").is_none(),
            "the stateless revision is not a legacy initialize variant"
        );

        let rollback = build_legacy_initialize_result_with_teaching(
            &registry,
            mcp_types::MCP_PROTOCOL_2025_06_18,
            &codex,
            false,
        );
        assert!(
            rollback.get("instructions").is_none(),
            "the protocol teaching rollback gate must remove only instructions"
        );
        assert_eq!(rollback["toolCount"], supported["toolCount"]);
        assert_eq!(rollback["capabilities"], supported["capabilities"]);
    }

    #[test]
    fn surface_name_manifests_match_the_v0_5_62_compatibility_baseline() {
        let broad = contextstream_tools_list(&test_registry(Config::default()), None);
        assert_eq!(listed_names(&broad), V0_5_62_BROAD_TOOL_NAMES);

        let router = contextstream_tools_list(
            &test_registry(Config {
                router_mode: true,
                ..Config::default()
            }),
            None,
        );
        assert_eq!(listed_names(&router), V0_5_62_ROUTER_TOOL_NAMES);

        let openai = contextstream_tools_list(
            &test_registry(Config {
                tool_surface_profile: ToolSurfaceProfile::OpenaiAgentic,
                ..Config::default()
            }),
            None,
        );
        assert_eq!(listed_names(&openai), V0_5_62_OPENAI_AGENTIC_TOOL_NAMES);
    }

    #[test]
    fn broad_input_schemas_match_the_expected_structural_contract() {
        let tools = contextstream_tools_list(&test_registry(Config::default()), None);
        let actual: Vec<_> = tools
            .iter()
            .map(|tool| {
                (
                    tool["name"].as_str().expect("tool name"),
                    schema_contract_hash(&tool["inputSchema"]),
                )
            })
            .collect();
        let expected: Vec<_> = EXPECTED_BROAD_SCHEMA_CONTRACTS
            .iter()
            .map(|(name, hash)| (*name, (*hash).to_string()))
            .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn canonical_tool_list_is_deterministic_complete_and_v0_5_62_compatible() {
        let first_registry = test_registry(Config::default());
        let second_registry = test_registry(Config::default());
        let first = contextstream_tools_list(&first_registry, None);
        let second = contextstream_tools_list(&second_registry, None);

        assert_eq!(
            first, second,
            "separately built registries must serialize identically"
        );

        let listed_names: Vec<&str> = first
            .iter()
            .map(|tool| tool["name"].as_str().expect("tool name"))
            .collect();
        let mut expected_names = first_registry.names();
        expected_names.sort_unstable();
        assert_eq!(
            listed_names, expected_names,
            "no registered tool may disappear"
        );

        let mut sorted_names = listed_names.clone();
        sorted_names.sort_unstable();
        sorted_names.dedup();
        assert_eq!(
            listed_names, sorted_names,
            "tool names must be unique and sorted"
        );

        for tool in &first {
            // These three keys are the v0.5.62 compatibility floor.
            assert!(tool["name"].is_string());
            assert!(tool["description"].is_string());
            assert!(tool["inputSchema"].is_object());

            assert!(tool["title"].is_string());
            assert!(tool["category"].is_string());
            assert!(tool["annotations"].is_object());
            assert_eq!(
                tool["_meta"]["contextstream"]["metadataVersion"],
                TOOL_DISCOVERY_METADATA_VERSION
            );
            assert!(tool["_meta"]["contextstream"]["safety"].is_object());
        }
    }

    #[test]
    fn mixed_action_tools_advertise_worst_case_safety() {
        let registry = test_registry(Config::default());
        let tools = contextstream_tools_list(&registry, None);

        let init = tool_by_name(&tools, "init");
        assert_eq!(init["annotations"]["readOnlyHint"], false);
        assert_eq!(init["annotations"]["destructiveHint"], false);
        assert_eq!(init["annotations"]["idempotentHint"], true);

        let graph = tool_by_name(&tools, "graph");
        assert_eq!(graph["annotations"]["readOnlyHint"], false);
        assert_eq!(graph["annotations"]["destructiveHint"], false);

        let context = tool_by_name(&tools, "context");
        assert_eq!(context["annotations"]["readOnlyHint"], false);
        assert_eq!(context["annotations"]["destructiveHint"], false);

        let help = tool_by_name(&tools, "help");
        assert_eq!(help["annotations"]["readOnlyHint"], true);
        assert_eq!(help["annotations"]["destructiveHint"], false);

        for name in [
            "capsule",
            "entity",
            "integration",
            "memory",
            "media",
            "project",
            "qa",
            "reminder",
            "session",
            "skill",
            "vcs",
            "workspace",
        ] {
            let tool = tool_by_name(&tools, name);
            assert_eq!(
                tool["annotations"]["readOnlyHint"], false,
                "{name} exposes mutating actions"
            );
            assert_eq!(
                tool["annotations"]["destructiveHint"], true,
                "{name} exposes delete/purge/revoke/overwrite actions"
            );
            assert_eq!(
                tool["_meta"]["contextstream"]["safety"]["requiresConfirmation"], true,
                "{name} must request confirmation"
            );
        }
    }

    /// The ChatGPT Apps directory imports server-advertised annotations and
    /// rejects a submission where any tool is missing one, so the guarantee has
    /// to cover the whole surface rather than the handful spot-checked above.
    /// `tools/list` is behind OAuth in production, which is why this could not
    /// be confirmed from outside (contextstream/contextstream#263).
    ///
    /// Run with `--nocapture` to print the table the portal form needs.
    #[test]
    fn every_tool_advertises_all_three_reviewed_hints() {
        for (profile_name, config) in [
            ("default", Config::default()),
            (
                "router",
                Config {
                    router_mode: true,
                    ..Config::default()
                },
            ),
            (
                "openai-agentic",
                Config {
                    tool_surface_profile: ToolSurfaceProfile::OpenaiAgentic,
                    ..Config::default()
                },
            ),
        ] {
            let tools = contextstream_tools_list(&test_registry(config), None);
            assert!(!tools.is_empty(), "{profile_name} advertised no tools");

            println!("\n=== {profile_name} ({} tools) ===", tools.len());
            println!(
                "{:<24} {:>8} {:>11} {:>11}",
                "tool", "readOnly", "openWorld", "destructive"
            );

            for tool in &tools {
                let name = tool["name"].as_str().unwrap_or("<unnamed>");
                let annotations = &tool["annotations"];

                // A missing hint is worse than a permissive one: the reviewer
                // reads absence as unknown and the scan fails on it.
                for hint in ["readOnlyHint", "openWorldHint", "destructiveHint"] {
                    assert!(
                        annotations[hint].is_boolean(),
                        "{profile_name}/{name} has no explicit {hint}, so review sees it as unknown"
                    );
                }

                // Read-only and destructive are contradictory claims. Shipping
                // both would read as a tool that mutates nothing while
                // destroying something.
                if annotations["readOnlyHint"] == true {
                    assert_eq!(
                        annotations["destructiveHint"], false,
                        "{profile_name}/{name} claims read-only and destructive at once"
                    );
                }

                // Single-action aliases advertise their own safety rather than
                // inheriting the parent domain's, so a delete or an overwrite
                // has to claim it here. Name matching is only a backstop
                // against a new alias forgetting; the values are still explicit
                // at each registration site.
                if name.contains("delete") || name.contains("update") {
                    assert_eq!(
                        annotations["destructiveHint"], true,
                        "{profile_name}/{name} removes or overwrites data, which is not an additive update"
                    );
                }

                println!(
                    "{:<24} {:>8} {:>11} {:>11}",
                    name,
                    annotations["readOnlyHint"],
                    annotations["openWorldHint"],
                    annotations["destructiveHint"],
                );
            }
        }
    }

    #[test]
    fn router_and_openai_meta_tools_have_truthful_annotations() {
        let router_config = Config {
            router_mode: true,
            ..Config::default()
        };
        let router_tools = contextstream_tools_list(&test_registry(router_config), None);
        let operations = tool_by_name(&router_tools, "operations");
        assert_eq!(operations["annotations"]["readOnlyHint"], true);
        assert_eq!(operations["annotations"]["openWorldHint"], false);
        let execute = tool_by_name(&router_tools, "execute_operation");
        assert_eq!(execute["annotations"]["readOnlyHint"], false);
        assert_eq!(execute["annotations"]["destructiveHint"], true);

        let openai_config = Config {
            tool_surface_profile: ToolSurfaceProfile::OpenaiAgentic,
            ..Config::default()
        };
        let openai_tools = contextstream_tools_list(&test_registry(openai_config), None);
        let search = tool_by_name(&openai_tools, "tool_search");
        assert_eq!(search["annotations"]["readOnlyHint"], true);
        assert_eq!(search["annotations"]["openWorldHint"], false);
        let batch = tool_by_name(&openai_tools, "batch_operations");
        assert_eq!(batch["annotations"]["readOnlyHint"], true);
        assert_eq!(batch["annotations"]["destructiveHint"], false);
    }

    #[test]
    fn router_category_filter_is_canonical_and_supports_meta_alias() {
        let config = Config {
            router_mode: true,
            ..Config::default()
        };
        let registry = test_registry(config);

        let router = contextstream_tools_list(&registry, Some("router"));
        let meta = contextstream_tools_list(&registry, Some("meta"));
        assert_eq!(router, meta);
        assert_eq!(
            router
                .iter()
                .map(|tool| tool["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["execute_operation", "operations"]
        );
    }

    fn stateless_params(version: &str) -> serde_json::Value {
        serde_json::json!({
            "_meta": {
                mcp_types::MCP_META_PROTOCOL_VERSION: version,
                mcp_types::MCP_META_CLIENT_INFO: {
                    "name": "codex-cli",
                    "version": "1.2.3"
                },
                mcp_types::MCP_META_CLIENT_CAPABILITIES: {}
            }
        })
    }

    fn protocol_test_runtime() -> (
        Arc<ToolRegistry>,
        Arc<ContextStreamClient>,
        Arc<SessionManager>,
        AgenticTelemetry,
    ) {
        let config = Config {
            api_key: Some("test-api-key".to_string()),
            ..Config::default()
        };
        let client = Arc::new(ContextStreamClient::new(config.clone()));
        let session = Arc::new(SessionManager::new(client.as_ref().clone(), config.clone()));
        let registry = Arc::new(build_registry(
            &config,
            client.as_ref().clone(),
            session.clone(),
        ));
        let telemetry = AgenticTelemetry::new(client.as_ref().clone(), session.clone());
        (registry, client, session, telemetry)
    }

    #[test]
    fn advertised_stateless_adapter_is_complete_as_one_unit() {
        assert!(MCP_2026_STATELESS_CONFORMANCE.fully_conformant());
    }

    #[tokio::test]
    async fn stdio_stateless_discovery_returns_versioned_identity_and_cache_contract() {
        let (registry, client, session, telemetry) = protocol_test_runtime();
        let response = handle_request(
            &registry,
            &client,
            &session,
            &telemetry,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": "discover-2026",
                "method": "server/discover",
                "params": stateless_params(MCP_PROTOCOL_2026_07_28)
            }),
            mcp_tools::wire_tokens::WireTokenObservation::default(),
        )
        .await;
        let envelope: serde_json::Value =
            serde_json::from_str(&response).expect("valid discovery response");
        let result = &envelope["result"];

        assert_eq!(
            result["supportedVersions"],
            serde_json::json!([MCP_PROTOCOL_2026_07_28])
        );
        assert_eq!(result["capabilities"]["tools"]["listChanged"], false);
        assert_eq!(result["resultType"], "complete");
        assert_eq!(result["ttlMs"], mcp_types::MCP_DISCOVERY_TTL_MS);
        assert_eq!(result["cacheScope"], "private");
        assert_eq!(
            result["_meta"][mcp_types::MCP_META_SERVER_INFO]["name"],
            "contextstream-mcp"
        );
        assert_eq!(
            result["_meta"][mcp_types::MCP_META_SERVER_INFO]["version"],
            mcp_types::config::VERSION
        );
        if crate::protocol_harness_teaching_enabled() {
            let instructions = result["instructions"]
                .as_str()
                .expect("stateless discovery instructions");
            assert!(instructions.contains(mcp_types::HARNESS_TEACHING_VERSION));
            assert!(instructions.contains("`init("));
            assert!(!instructions.contains("mcp__contextstream__"));
        }
    }

    #[tokio::test]
    async fn stdio_stateless_tools_list_is_typed_private_and_cacheable() {
        let (registry, client, session, telemetry) = protocol_test_runtime();
        let expected = contextstream_tools_list(&registry, None);
        registry.set_tool_surface_profile(ToolSurfaceProfile::OpenaiAgentic);
        let response = handle_request(
            &registry,
            &client,
            &session,
            &telemetry,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": "tools-2026",
                "method": "tools/list",
                "params": stateless_params(MCP_PROTOCOL_2026_07_28)
            }),
            mcp_tools::wire_tokens::WireTokenObservation::default(),
        )
        .await;
        let envelope: serde_json::Value =
            serde_json::from_str(&response).expect("valid tools/list response");
        let result = &envelope["result"];

        assert_eq!(result["tools"], serde_json::Value::Array(expected));
        assert_eq!(result["resultType"], "complete");
        assert_eq!(result["ttlMs"], MCP_TOOLS_LIST_TTL_MS);
        assert_eq!(result["cacheScope"], "private");
        assert_eq!(
            result["_meta"][mcp_types::MCP_META_SERVER_INFO]["name"],
            "contextstream-mcp"
        );
        assert_eq!(registry.tool_surface_profile(), ToolSurfaceProfile::Default);
    }

    #[tokio::test]
    async fn stdio_stateless_requests_fail_closed_on_missing_or_future_metadata() {
        let (registry, client, session, telemetry) = protocol_test_runtime();
        let missing = handle_request(
            &registry,
            &client,
            &session,
            &telemetry,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": "missing-meta",
                "method": "server/discover",
                "params": {}
            }),
            mcp_tools::wire_tokens::WireTokenObservation::default(),
        )
        .await;
        let missing: serde_json::Value =
            serde_json::from_str(&missing).expect("valid missing-metadata response");
        assert_eq!(missing["error"]["code"], -32602);
        assert!(missing["error"].get("data").is_none());

        let future = handle_request(
            &registry,
            &client,
            &session,
            &telemetry,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": "future-version",
                "method": "tools/list",
                "params": stateless_params("2099-01-01")
            }),
            mcp_tools::wire_tokens::WireTokenObservation::default(),
        )
        .await;
        let future: serde_json::Value =
            serde_json::from_str(&future).expect("valid unsupported-version response");
        assert_eq!(
            future["error"]["code"],
            mcp_types::MCP_ERROR_UNSUPPORTED_PROTOCOL_VERSION
        );
        assert_eq!(future["error"]["data"]["requested"], "2099-01-01");
        assert_eq!(
            future["error"]["data"]["supported"],
            serde_json::json!([MCP_PROTOCOL_2026_07_28])
        );

        let initialize = handle_request(
            &registry,
            &client,
            &session,
            &telemetry,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": "initialize-2026",
                "method": "initialize",
                "params": stateless_params(MCP_PROTOCOL_2026_07_28)
            }),
            mcp_tools::wire_tokens::WireTokenObservation::default(),
        )
        .await;
        let initialize: serde_json::Value =
            serde_json::from_str(&initialize).expect("valid initialize rejection");
        assert_eq!(initialize["error"]["code"], -32601);

        let ping = handle_request(
            &registry,
            &client,
            &session,
            &telemetry,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": "ping-2026",
                "method": "ping",
                "params": stateless_params(MCP_PROTOCOL_2026_07_28)
            }),
            mcp_tools::wire_tokens::WireTokenObservation::default(),
        )
        .await;
        let ping: serde_json::Value = serde_json::from_str(&ping).expect("valid ping rejection");
        assert_eq!(ping["error"]["code"], -32601);

        let unknown_tool = handle_request(
            &registry,
            &client,
            &session,
            &telemetry,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": "unknown-tool-2026",
                "method": "tools/call",
                "params": {
                    "name": "does_not_exist",
                    "arguments": {},
                    "_meta": stateless_params(MCP_PROTOCOL_2026_07_28)["_meta"].clone()
                }
            }),
            mcp_tools::wire_tokens::WireTokenObservation::default(),
        )
        .await;
        let unknown_tool: serde_json::Value =
            serde_json::from_str(&unknown_tool).expect("valid unknown-tool rejection");
        assert_eq!(unknown_tool["error"]["code"], -32602);

        let invalid_arguments = handle_request(
            &registry,
            &client,
            &session,
            &telemetry,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": "invalid-arguments-2026",
                "method": "tools/call",
                "params": {
                    "name": "search",
                    "arguments": [],
                    "_meta": stateless_params(MCP_PROTOCOL_2026_07_28)["_meta"].clone()
                }
            }),
            mcp_tools::wire_tokens::WireTokenObservation::default(),
        )
        .await;
        let invalid_arguments: serde_json::Value =
            serde_json::from_str(&invalid_arguments).expect("valid invalid-arguments rejection");
        assert_eq!(invalid_arguments["error"]["code"], -32602);

        let missing_id = handle_request(
            &registry,
            &client,
            &session,
            &telemetry,
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "tools/list",
                "params": stateless_params(MCP_PROTOCOL_2026_07_28)
            }),
            mcp_tools::wire_tokens::WireTokenObservation::default(),
        )
        .await;
        let missing_id: serde_json::Value =
            serde_json::from_str(&missing_id).expect("valid missing-id rejection");
        assert_eq!(missing_id["error"]["code"], -32600);
    }

    async fn assert_stdio_protocol_smoke(config: Config) {
        let mut config = config;
        config.api_key = Some("test-api-key".to_string());
        let client = Arc::new(ContextStreamClient::new(config.clone()));
        let session = Arc::new(SessionManager::new(client.as_ref().clone(), config.clone()));
        let registry = Arc::new(build_registry(
            &config,
            client.as_ref().clone(),
            session.clone(),
        ));
        let telemetry = AgenticTelemetry::new(client.as_ref().clone(), session.clone());

        let initialize = handle_request(
            &registry,
            &client,
            &session,
            &telemetry,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": "initialize-smoke",
                "method": "initialize",
                "params": {}
            }),
            mcp_tools::wire_tokens::WireTokenObservation::default(),
        )
        .await;
        let initialize: serde_json::Value =
            serde_json::from_str(&initialize).expect("valid initialize response");

        let expected = contextstream_tools_list(&registry, None);
        assert_eq!(
            initialize["result"]["protocolVersion"],
            serde_json::json!(MCP_PROTOCOL_2024_11_05)
        );
        assert!(
            initialize["result"].get("instructions").is_none(),
            "advertised 2024 protocol must omit instructions"
        );
        assert_eq!(
            initialize["result"]["toolCount"],
            serde_json::json!(expected.len())
        );

        let list = handle_request(
            &registry,
            &client,
            &session,
            &telemetry,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": "tools-list-smoke",
                "method": "tools/list",
                "params": {}
            }),
            mcp_tools::wire_tokens::WireTokenObservation::default(),
        )
        .await;
        let list: serde_json::Value =
            serde_json::from_str(&list).expect("valid tools/list response");
        assert_eq!(list["result"]["tools"], serde_json::Value::Array(expected));

        // Avoid the unrelated auto-init path: workflow help is a static,
        // read-only recovery contract and this smoke is exercising its real
        // protocol serialization on each tool-surface shape.
        session.initialize(None, None, None, None).await;
        let (tool_name, arguments) = if registry.is_router_mode() {
            (
                "execute_operation",
                serde_json::json!({
                    "name": "help",
                    "arguments": {
                        "action": "workflow",
                        "client_name": "codex-cli/1.2.3"
                    }
                }),
            )
        } else {
            (
                "help",
                serde_json::json!({
                    "action": "workflow",
                    "client_name": "codex-cli/1.2.3"
                }),
            )
        };
        let workflow = handle_request(
            &registry,
            &client,
            &session,
            &telemetry,
            serde_json::json!({
                "jsonrpc": "2.0",
                "id": "workflow-smoke",
                "method": "tools/call",
                "params": {
                    "name": tool_name,
                    "arguments": arguments
                }
            }),
            mcp_tools::wire_tokens::WireTokenObservation::default(),
        )
        .await;
        let workflow: serde_json::Value =
            serde_json::from_str(&workflow).expect("valid workflow response");
        let result = &workflow["result"];
        assert_eq!(result["isError"], false, "{workflow:#}");
        assert_eq!(
            result["structuredContent"]["teaching_version"],
            mcp_types::HARNESS_TEACHING_VERSION
        );
        assert_eq!(result["structuredContent"]["harness_id"], "codex");
        assert_eq!(
            result["structuredContent"]["steps"]
                .as_array()
                .map(Vec::len),
            Some(mcp_types::HarnessTeachingStepId::ALL.len())
        );
        assert!(result["structuredContent"]["steps"]
            .as_array()
            .is_some_and(|steps| steps.iter().any(|step| {
                step["id"] == serde_json::Value::String("create_canonical_handoff".to_string())
            })));
        assert!(
            result.get("structured").is_none(),
            "MCP JSON-RPC must use the standard structuredContent field"
        );
        assert!(result["content"]
            .as_array()
            .is_some_and(|items| items.iter().any(|item| {
                item["text"]
                    .as_str()
                    .is_some_and(|text| text.contains(mcp_types::HARNESS_TEACHING_VERSION))
            })));
    }

    #[tokio::test]
    async fn stdio_initialize_then_tools_list_smoke_covers_every_surface() {
        assert_stdio_protocol_smoke(Config::default()).await;
        assert_stdio_protocol_smoke(Config {
            router_mode: true,
            ..Config::default()
        })
        .await;
        assert_stdio_protocol_smoke(Config {
            tool_surface_profile: ToolSurfaceProfile::OpenaiAgentic,
            ..Config::default()
        })
        .await;
    }

    #[test]
    fn stdio_batch_rejects_context_and_search_before_dispatch() {
        for name in ["context", "search"] {
            let arguments = serde_json::json!({
                "operations": [
                    {"name": "help", "arguments": {"action": "version"}},
                    {"name": name, "arguments": {"tokenizer": "o200k_base"}}
                ]
            });
            assert_eq!(
                batch_operation_requiring_direct_wire_accounting(&arguments),
                Some(name)
            );
            let message = batch_wire_accounting_rejection_message(name);
            assert!(message.contains("direct tool call"));
            assert!(message.contains("whole-wire token budget"));
        }

        assert_eq!(
            batch_operation_requiring_direct_wire_accounting(&serde_json::json!({
                "operations": [{"name": "help", "arguments": {"action": "version"}}]
            })),
            None
        );
    }

    #[test]
    fn stdio_tool_result_counter_matches_actual_jsonrpc_bytes_and_newline() {
        let result = mcp_types::tool::ToolResult::with_structured(
            "数据库 👩‍💻 \\\"json\\\"",
            serde_json::json!({"answer": "grounded"}),
        );
        for id in [serde_json::json!(42), serde_json::json!("request-long-id")] {
            let context = mcp_tools::wire_tokens::WireResponseContext::stdio_jsonrpc(
                Some(id.clone()),
                Some("Loading ContextStream context".to_string()),
                Some("⌬".to_string()),
            );
            let payload = mcp_tools::wire_tokens::tool_result_payload(&result, &context);
            let mut actual = json_rpc_result(Some(id), payload).into_bytes();
            actual.push(b'\n');
            let canonical =
                mcp_tools::wire_tokens::canonical_tool_result_bytes(&result, &context).unwrap();
            assert_eq!(actual, canonical);
            assert_eq!(actual.last(), Some(&b'\n'));
        }
    }

    #[test]
    fn contextstream_status_metadata_labels_plan_saves() {
        let status = contextstream_status_metadata("capture_plan", "Capture Plan", false);
        assert_eq!(
            status["in_progress"],
            serde_json::json!("Saving plan to ContextStream")
        );
        assert_eq!(
            status["completed"],
            serde_json::json!("Plan saved to ContextStream")
        );
    }

    #[test]
    fn contextstream_status_metadata_exposes_unified_action_labels() {
        let status = contextstream_status_metadata("session", "Session Operations", false);
        assert_eq!(
            status["actions"]["capture_plan"],
            serde_json::json!("Saving plan to ContextStream")
        );
        assert_eq!(
            status["actions"]["capture_lesson"],
            serde_json::json!("Saving lesson to ContextStream")
        );
        assert_eq!(
            status["actions"]["remember"],
            serde_json::json!("Saving memory to ContextStream")
        );
    }

    #[test]
    fn contextstream_status_metadata_memory_actions_cover_doc_updates() {
        let status = contextstream_status_metadata("memory", "Memory Operations", false);
        assert_eq!(
            status["actions"]["update_doc"],
            serde_json::json!("Updating doc in ContextStream")
        );
        assert_eq!(
            status["actions"]["create_doc"],
            serde_json::json!("Saving doc to ContextStream")
        );
        assert_eq!(
            status["actions"]["delete_doc"],
            serde_json::json!("Deleting doc in ContextStream")
        );
        assert_eq!(
            status["actions"]["list_docs"],
            serde_json::json!("Listing ContextStream docs")
        );
    }

    #[test]
    fn contextstream_status_metadata_entity_actions_present() {
        let status = contextstream_status_metadata("entity", "Structured Entity Operations", false);
        assert_eq!(
            status["actions"]["create"],
            serde_json::json!("Saving entity to ContextStream")
        );
        assert_eq!(
            status["actions"]["update"],
            serde_json::json!("Updating entity in ContextStream")
        );
        assert_eq!(
            status["actions"]["delete"],
            serde_json::json!("Deleting entity in ContextStream")
        );
    }

    #[test]
    fn contextstream_status_metadata_fallback_reads_naturally() {
        let status_ro = contextstream_status_metadata("acme", "Acme", true);
        assert_eq!(
            status_ro["in_progress"],
            serde_json::json!("Calling ContextStream — Acme")
        );
        assert_eq!(
            status_ro["completed"],
            serde_json::json!("ContextStream — Acme complete")
        );
        let status_wr = contextstream_status_metadata("acme", "Acme", false);
        assert_eq!(
            status_wr["in_progress"],
            serde_json::json!("Saving with ContextStream — Acme")
        );
        assert_eq!(
            status_wr["completed"],
            serde_json::json!("ContextStream — Acme saved")
        );
    }

    #[test]
    fn contextstream_call_icon_maps_save_create_to_oplus() {
        assert_eq!(contextstream_call_icon("memory_create_doc"), "⊕");
        assert_eq!(contextstream_call_icon("memory_create_task"), "⊕");
        assert_eq!(contextstream_call_icon("memory_create_event"), "⊕");
        assert_eq!(contextstream_call_icon("create_capsule"), "⊕");
        assert_eq!(contextstream_call_icon("save_plan"), "⊕");
    }

    #[test]
    fn contextstream_call_icon_maps_update_to_clockwise_arrow() {
        assert_eq!(contextstream_call_icon("memory_update_doc"), "↻");
        assert_eq!(contextstream_call_icon("update_session"), "↻");
    }

    #[test]
    fn contextstream_call_icon_maps_delete_to_ominus() {
        assert_eq!(contextstream_call_icon("memory_delete_doc"), "⊖");
        assert_eq!(contextstream_call_icon("delete_node"), "⊖");
    }

    #[test]
    fn contextstream_call_icon_maps_list_get_search() {
        assert_eq!(contextstream_call_icon("list_plans"), "☰");
        assert_eq!(contextstream_call_icon("get_doc"), "▸");
        assert_eq!(contextstream_call_icon("search_memory"), "⌕");
    }

    #[test]
    fn contextstream_call_icon_maps_recall_capture_complete() {
        assert_eq!(contextstream_call_icon("session_recall"), "⟲");
        assert_eq!(contextstream_call_icon("session_restore_context"), "⟲");
        assert_eq!(contextstream_call_icon("capture_plan"), "★");
        assert_eq!(contextstream_call_icon("session_capture_lesson"), "★");
        assert_eq!(contextstream_call_icon("memory_complete_todo"), "✓");
    }

    #[test]
    fn contextstream_call_icon_maps_ingest_export_index_run_link() {
        assert_eq!(contextstream_call_icon("ingest_local"), "⇣");
        assert_eq!(contextstream_call_icon("import_batch"), "⇣");
        assert_eq!(contextstream_call_icon("export_capsule"), "⇡");
        assert_eq!(contextstream_call_icon("index_project"), "⊞");
        assert_eq!(contextstream_call_icon("run_skill"), "▶");
        assert_eq!(contextstream_call_icon("link_vcs"), "⟷");
        assert_eq!(contextstream_call_icon("sync_workspace"), "⟷");
    }

    #[test]
    fn contextstream_call_icon_falls_back_to_benzene_for_routers() {
        // Aggregate router tools, no clear verb prefix → fallback glyph.
        assert_eq!(contextstream_call_icon("memory"), "⌬");
        assert_eq!(contextstream_call_icon("session"), "⌬");
        assert_eq!(contextstream_call_icon("entity"), "⌬");
        assert_eq!(contextstream_call_icon("context"), "⌬");
        assert_eq!(contextstream_call_icon("instruct"), "⌬");
        assert_eq!(contextstream_call_icon("workspace"), "⌬");
        assert_eq!(contextstream_call_icon("project"), "⌬");
        assert_eq!(contextstream_call_icon("graph"), "⌬");
        assert_eq!(contextstream_call_icon("vcs"), "⌬");
        assert_eq!(contextstream_call_icon("media"), "⌬");
        assert_eq!(contextstream_call_icon("reminder"), "⌬");
        assert_eq!(contextstream_call_icon("integration"), "⌬");
        assert_eq!(contextstream_call_icon("skill"), "⌬");
        assert_eq!(contextstream_call_icon("capsule"), "⌬");
        assert_eq!(contextstream_call_icon("qa"), "⌬");
        assert_eq!(contextstream_call_icon("help"), "⌬");
        assert_eq!(contextstream_call_icon("init"), "⌬");
    }

    #[test]
    fn contextstream_call_icon_handles_bare_verb_names() {
        // `search`, `index`, etc. without an underscore tail.
        assert_eq!(contextstream_call_icon("search"), "⌕");
        assert_eq!(contextstream_call_icon("index"), "⊞");
        assert_eq!(contextstream_call_icon("create"), "⊕");
        assert_eq!(contextstream_call_icon("list"), "☰");
        assert_eq!(contextstream_call_icon("get"), "▸");
        assert_eq!(contextstream_call_icon("delete"), "⊖");
        assert_eq!(contextstream_call_icon("update"), "↻");
        assert_eq!(contextstream_call_icon("recall"), "⟲");
        assert_eq!(contextstream_call_icon("capture"), "★");
        assert_eq!(contextstream_call_icon("complete"), "✓");
        assert_eq!(contextstream_call_icon("run"), "▶");
    }

    #[test]
    fn contextstream_call_title_resolves_per_action_for_memory_update_doc() {
        let args = serde_json::json!({"action": "update_doc", "doc_id": "x", "content": "y"});
        let title = contextstream_call_title("memory", "Memory Operations", false, &args);
        assert_eq!(title, "Updating doc in ContextStream");
    }

    #[test]
    fn contextstream_call_title_resolves_per_action_for_session_capture_plan() {
        let args = serde_json::json!({"action": "capture_plan", "title": "x"});
        let title = contextstream_call_title("session", "Session Operations", false, &args);
        assert_eq!(title, "Saving plan to ContextStream");
    }

    #[test]
    fn contextstream_call_title_resolves_per_action_for_entity_create() {
        let args = serde_json::json!({"kind": "ticket", "action": "create", "body": {}});
        let title =
            contextstream_call_title("entity", "Structured Entity Operations", false, &args);
        assert_eq!(title, "Saving entity to ContextStream");
    }

    #[test]
    fn contextstream_call_title_falls_back_to_in_progress_when_action_missing() {
        let args = serde_json::json!({"action": "some_unmapped_action"});
        let title = contextstream_call_title("memory", "Memory Operations", false, &args);
        assert_eq!(title, "Saving in ContextStream memory");
    }

    #[test]
    fn contextstream_call_title_uses_in_progress_for_dedicated_tools() {
        let args = serde_json::json!({"title": "x"});
        let title = contextstream_call_title("capture_plan", "Capture Plan", false, &args);
        assert_eq!(title, "Saving plan to ContextStream");
    }

    #[test]
    fn contextstream_status_metadata_uses_contextstream_brand_for_known_tools() {
        for name in [
            "reminder",
            "integration",
            "vcs",
            "qa",
            "project",
            "workspace",
            "capsule",
            "instruct",
            "skill",
            "media",
        ] {
            let status = contextstream_status_metadata(name, "X", false);
            let in_progress = status["in_progress"].as_str().unwrap();
            assert!(
                in_progress.contains("ContextStream"),
                "tool {} in_progress {:?} missing ContextStream brand",
                name,
                in_progress
            );
        }
    }

    #[test]
    fn auto_init_disables_auto_index_for_cwd_derived_home_root() {
        // A cwd-derived root (explicitly_supplied == false) must never enable
        // auto-indexing — even when the cwd is a $HOME-like / broad root.
        let home = dirs::home_dir().expect("home dir");
        assert!(!should_auto_init_index(
            Some(home.to_string_lossy().as_ref()),
            false,
        ));
        assert!(!should_auto_init_index(Some("/"), false));
        assert!(!should_auto_init_index(None, false));
    }

    #[test]
    fn auto_init_enables_auto_index_for_explicit_project_folder() {
        // An explicitly-supplied folder that clears the ingest guard enables it.
        let temp = tempfile::tempdir().expect("tempdir");
        assert!(should_auto_init_index(
            Some(temp.path().to_string_lossy().as_ref()),
            true,
        ));
    }

    #[test]
    fn auto_init_disables_auto_index_for_explicit_sensitive_root() {
        // Defense-in-depth: an explicit folder is still rejected when it fails
        // the ingest-containment guard ($HOME here).
        let home = dirs::home_dir().expect("home dir");
        assert!(!should_auto_init_index(
            Some(home.to_string_lossy().as_ref()),
            true,
        ));
    }
}
