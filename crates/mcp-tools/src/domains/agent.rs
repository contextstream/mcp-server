//! Slipstream agents: specialist agents a person hands one job, run by ContextStream's
//! Slipstream worker and paid per run in credits (ContextStream `handlers/tasks_api.rs`).
//!
//! ## Actions
//!
//! - `find`: the agents this workspace may use, each with its job, its price per run and
//!   whether an owner or admin has turned it on (`GET /tasks/agents?workspace_id=`).
//! - `run`: ask an agent for a task (`POST /workspaces/{id}/tasks`). A priced agent runs only
//!   with `accepted_price_credits`: without it the tool answers with the quote for the person
//!   to accept and nothing runs, so no run is ever charged without its price being shown.
//! - `get_run`: one task with its result (`GET /workspaces/{id}/tasks/{task}?wait_ms=`).
//! - `list_runs`: the workspace's tasks, newest first.
//! - `cancel_run`: stop a task (`POST /workspaces/{id}/tasks/{task}/cancel`).
//!
//! A run is charged its price once, only when it succeeds; a declined, failed, cancelled or
//! timed-out run costs nothing. Every line of text output carries the task id, because many
//! clients drop structured content.

use async_trait::async_trait;
use mcp_client::ContextStreamClient;
use mcp_types::{
    tool::{ToolAnnotations, ToolCategory, ToolMetadata, ToolResult},
    Error, Result,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use uuid::Uuid;

use crate::registry::ToolHandler;
use crate::schema::SchemaBuilder;

const VALID_ACTIONS: &[&str] = &["find", "run", "get_run", "list_runs", "cancel_run"];

/// The longest ContextStream holds a request open for a task to end.
const MAX_WAIT_MS: u64 = 25_000;
/// What `run` waits by default, so a quick agent answers in the same call.
const DEFAULT_RUN_WAIT_MS: u64 = 20_000;
const MAX_LIST_LIMIT: i64 = 50;

const TOOL_DESCRIPTION: &str = "ContextStream specialist agents (Slipstream): hand one job to an agent that does only that job, with the workspace's context, and read its result. Actions: find (the agents, their job and price per run), run (ask for a task; a priced agent needs accepted_price_credits, so show the person the quote first), get_run (a task and its result, waiting up to 25 s), list_runs, cancel_run. A run is charged its price only when it succeeds; declined or failed runs are free. Full action reference: the `action` parameter.";

const TOOL_REFERENCE: &str = "ContextStream specialist agents (Slipstream).\n\nEach agent does one job (read an image, review a PR, write release notes, triage mail, and so on) with the workspace's own context: decisions, lessons, docs and code. It declines anything outside its job, and a declined run is free.\n\nActions:\n- find: list the agents for the workspace (workspace_id defaults to the session's). Each has a job, a price_credits per run, and enabled (an owner or admin turned it on).\n- run: agent (id, or id@version), input (the agent's input object), project_id (defaults to the session's). A priced agent runs only with accepted_price_credits equal to (or above) its price: call run without it to get the quote, show the person the price, and call again with their acceptance. wait_ms (up to 25000, default 20000) waits for the result in the same call; otherwise use get_run.\n- get_run: task_id, wait_ms (optional). Returns the status (queued, running, succeeded, failed, declined, cancelled, timed_out), the result envelope once there is one, and what the run cost.\n- list_runs: agent (optional filter), limit (at most 50).\n- cancel_run: task_id.\n\nBilling: a run costs its price once, only when it succeeds. Declined, failed, cancelled and timed-out runs cost nothing, and the agent's model calls are never charged to the person.";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentInput {
    pub action: String,
    pub workspace_id: Option<String>,
    pub project_id: Option<String>,
    pub agent: Option<String>,
    pub input: Option<Value>,
    pub accepted_price_credits: Option<i64>,
    pub wait_ms: Option<u64>,
    pub task_id: Option<String>,
    pub limit: Option<i64>,
    pub idempotency_key: Option<String>,
    pub session_id: Option<String>,
}

pub struct AgentTool {
    client: ContextStreamClient,
}

impl AgentTool {
    pub fn new(client: ContextStreamClient) -> Self {
        Self { client }
    }

    async fn workspace_id(&self, input: &Option<String>) -> Result<Uuid> {
        if let Some(value) = input.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
            return Uuid::parse_str(value)
                .map_err(|_| Error::Validation("Invalid workspace_id".to_string()));
        }
        self.client
            .config()
            .await
            .default_workspace_id
            .ok_or_else(|| {
                Error::Validation(
                    "workspace_id is required when no default workspace is configured".to_string(),
                )
            })
    }

    async fn project_id(&self, input: &Option<String>) -> Result<Uuid> {
        if let Some(value) = input.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
            return Uuid::parse_str(value)
                .map_err(|_| Error::Validation("Invalid project_id".to_string()));
        }
        self.client
            .config()
            .await
            .default_project_id
            .ok_or_else(|| {
                Error::Validation(
                    "project_id is required when no default project is configured".to_string(),
                )
            })
    }

    fn task_id(input: &Option<String>) -> Result<Uuid> {
        let value = input
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| Error::Validation("task_id is required".to_string()))?;
        Uuid::parse_str(value).map_err(|_| Error::Validation("Invalid task_id".to_string()))
    }

    async fn catalog(&self, workspace_id: Uuid) -> Result<Value> {
        let response: Value = self
            .client
            .get(&format!("/tasks/agents?workspace_id={workspace_id}"))
            .await?;
        Ok(unwrap_data(response))
    }
}

#[async_trait]
impl ToolHandler for AgentTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let input: AgentInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;
        let action = input.action.trim().to_lowercase();
        match action.as_str() {
            "find" => {
                let workspace_id = self.workspace_id(&input.workspace_id).await?;
                let catalog = self.catalog(workspace_id).await?;
                Ok(ToolResult::with_structured(
                    format_catalog(&catalog),
                    catalog,
                ))
            }
            "run" => {
                let agent = input
                    .agent
                    .as_deref()
                    .map(str::trim)
                    .filter(|a| !a.is_empty())
                    .ok_or_else(|| Error::Validation("agent is required for run".to_string()))?
                    .to_string();
                let task_input = input.input.clone().unwrap_or_else(|| json!({}));
                if !task_input.is_object() {
                    return Err(Error::Validation(
                        "input must be the agent's input object".to_string(),
                    ));
                }
                let workspace_id = self.workspace_id(&input.workspace_id).await?;
                let project_id = self.project_id(&input.project_id).await?;
                let catalog = self.catalog(workspace_id).await?;
                let entry = find_entry(&catalog, &agent).ok_or_else(|| {
                    Error::Validation(format!(
                        "No agent {agent} in this workspace's catalog: use agent(action=\"find\")"
                    ))
                })?;
                if let Some(quote) = quote_needed(entry, input.accepted_price_credits) {
                    return Ok(ToolResult::with_structured(
                        quote.text(&agent, project_id),
                        json!({ "quote": quote.price, "agent": agent, "ran": false }),
                    ));
                }
                let wait_ms = input
                    .wait_ms
                    .unwrap_or(DEFAULT_RUN_WAIT_MS)
                    .min(MAX_WAIT_MS);
                let mut body = json!({
                    "agent": agent,
                    "input": task_input,
                    "context": { "project_id": project_id },
                    "wait_ms": wait_ms,
                    "source": "mcp",
                });
                if let Some(accepted) = input.accepted_price_credits {
                    body["accepted_price_credits"] = json!(accepted);
                }
                if let Some(key) = input
                    .idempotency_key
                    .as_deref()
                    .filter(|k| !k.trim().is_empty())
                {
                    body["idempotency_key"] = json!(key.trim());
                }
                if let Some(session) = input.session_id.as_deref().filter(|s| !s.trim().is_empty())
                {
                    body["session_id"] = json!(session.trim());
                }
                let response: Value = self
                    .client
                    .post(&format!("/workspaces/{workspace_id}/tasks"), body)
                    .await?;
                let task = unwrap_data(response);
                Ok(ToolResult::with_structured(format_task(&task), task))
            }
            "get_run" => {
                let workspace_id = self.workspace_id(&input.workspace_id).await?;
                let task_id = Self::task_id(&input.task_id)?;
                let wait = input.wait_ms.unwrap_or(0).min(MAX_WAIT_MS);
                let path = if wait > 0 {
                    format!("/workspaces/{workspace_id}/tasks/{task_id}?wait_ms={wait}")
                } else {
                    format!("/workspaces/{workspace_id}/tasks/{task_id}")
                };
                let response: Value = self.client.get(&path).await?;
                let task = unwrap_data(response);
                Ok(ToolResult::with_structured(format_task(&task), task))
            }
            "list_runs" => {
                let workspace_id = self.workspace_id(&input.workspace_id).await?;
                let limit = input.limit.unwrap_or(20).clamp(1, MAX_LIST_LIMIT);
                let mut path = format!("/workspaces/{workspace_id}/tasks?limit={limit}");
                if let Some(agent) = input
                    .agent
                    .as_deref()
                    .map(str::trim)
                    .filter(|a| !a.is_empty())
                {
                    let id = agent.split('@').next().unwrap_or(agent);
                    if !is_agent_id(id) {
                        return Err(Error::Validation(format!("{agent} is not an agent id")));
                    }
                    path.push_str(&format!("&agent={id}"));
                }
                let response: Value = self.client.get(&path).await?;
                let list = unwrap_data(response);
                Ok(ToolResult::with_structured(format_task_list(&list), list))
            }
            "cancel_run" => {
                let workspace_id = self.workspace_id(&input.workspace_id).await?;
                let task_id = Self::task_id(&input.task_id)?;
                let response: Value = self
                    .client
                    .post(
                        &format!("/workspaces/{workspace_id}/tasks/{task_id}/cancel"),
                        json!({}),
                    )
                    .await?;
                let task = unwrap_data(response);
                Ok(ToolResult::with_structured(format_task(&task), task))
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
            name: "agent".to_string(),
            title: "Specialist Agents".to_string(),
            description: TOOL_DESCRIPTION.to_string(),
            category: ToolCategory::Ai,
            annotations: ToolAnnotations::destructive(),
            is_pro: false,
            required_tier: None,
        })
    }

    fn input_schema(&self) -> Value {
        SchemaBuilder::new()
            .description("Specialist agent operations")
            .string_enum(
                "action",
                &crate::schema::with_full_reference("Action to perform", TOOL_REFERENCE),
                VALID_ACTIONS,
                true,
            )
            .uuid(
                "workspace_id",
                "Workspace ID (defaults to the session's)",
                false,
            )
            .uuid(
                "project_id",
                "Project ID for run (defaults to the session's)",
                false,
            )
            .string(
                "agent",
                "Agent id, or id@version (run; a filter for list_runs)",
                false,
            )
            .object("input", "The agent's input object (run)", false)
            .integer(
                "accepted_price_credits",
                "The price per run the person accepted (run): required for a priced agent",
                false,
            )
            .integer(
                "wait_ms",
                "Milliseconds to wait for the result, at most 25000",
                false,
            )
            .uuid("task_id", "Task ID (get_run, cancel_run)", false)
            .integer("limit", "Maximum tasks (list_runs), at most 50", false)
            .string(
                "idempotency_key",
                "Key that makes a repeated run return the same task",
                false,
            )
            .string(
                "session_id",
                "The caller's session, recorded on the task",
                false,
            )
            .build()
    }
}

/// ContextStream wraps answers as `{"success": true, "data": ...}`; the client may already
/// have unwrapped it.
fn unwrap_data(response: Value) -> Value {
    match response {
        Value::Object(mut map) if map.contains_key("data") && map.contains_key("success") => {
            map.remove("data").unwrap_or(Value::Null)
        }
        other => other,
    }
}

fn is_agent_id(id: &str) -> bool {
    let mut chars = id.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

fn catalog_entries(catalog: &Value) -> Vec<&Value> {
    catalog
        .get("agents")
        .and_then(Value::as_array)
        .map(|agents| agents.iter().collect())
        .unwrap_or_default()
}

/// The catalog entry for `agent` (`id` or `id@version`): the consent view's `agent`.
fn find_entry<'a>(catalog: &'a Value, agent: &str) -> Option<&'a Value> {
    let id = agent.split('@').next().unwrap_or(agent);
    catalog_entries(catalog)
        .into_iter()
        .find(|entry| entry.pointer("/agent/id").and_then(Value::as_str) == Some(id))
}

/// A run that must show its price first.
#[derive(Debug, PartialEq, Eq)]
struct Quote {
    price: i64,
    /// The person accepted a lower price than the agent now costs.
    raised_from: Option<i64>,
}

impl Quote {
    fn text(&self, agent: &str, project_id: Uuid) -> String {
        let lead = match self.raised_from {
            Some(accepted) => format!(
                "Not run: {agent} now costs {} credits a run, more than the {accepted} accepted.",
                self.price
            ),
            None => format!("Not run yet: {agent} costs {} credits a run.", self.price),
        };
        format!(
            "{lead} It is charged only if the run succeeds; a declined or failed run is free. \
             Show the person this price. If they accept, call agent(action=\"run\", agent=\"{agent}\", \
             project_id=\"{project_id}\", input=..., accepted_price_credits={}).",
            self.price
        )
    }
}

/// `Some` when the run must stop at the quote: the agent has a price above zero and the caller
/// has not accepted at least that much.
fn quote_needed(entry: &Value, accepted: Option<i64>) -> Option<Quote> {
    let price = entry
        .pointer("/agent/price_credits")
        .and_then(Value::as_i64)
        .filter(|price| *price > 0)?;
    match accepted {
        Some(accepted) if accepted >= price => None,
        Some(accepted) => Some(Quote {
            price,
            raised_from: Some(accepted),
        }),
        None => Some(Quote {
            price,
            raised_from: None,
        }),
    }
}

fn format_catalog(catalog: &Value) -> String {
    let entries = catalog_entries(catalog);
    if entries.is_empty() {
        return "No agents are available in this workspace yet.".to_string();
    }
    let mut out = format!("{} agent(s):\n\n", entries.len());
    for (i, entry) in entries.iter().enumerate() {
        let agent = entry.get("agent").unwrap_or(entry);
        let id = agent.get("id").and_then(Value::as_str).unwrap_or("?");
        let version = agent.get("version").and_then(Value::as_str).unwrap_or("?");
        let name = agent.get("name").and_then(Value::as_str).unwrap_or(id);
        let price = match agent.get("price_credits").and_then(Value::as_i64) {
            Some(0) => "free".to_string(),
            Some(price) => format!("{price} credits a run, only if it succeeds"),
            None => "metered per model call".to_string(),
        };
        let on = if entry.get("enabled").and_then(Value::as_bool) == Some(true) {
            "on"
        } else {
            "off: an owner or admin turns it on"
        };
        out.push_str(&format!(
            "{}. **{name}** ({id}@{version}) — {price}; {on}\n",
            i + 1
        ));
        if let Some(description) = agent
            .get("description")
            .and_then(Value::as_str)
            .filter(|d| !d.trim().is_empty())
        {
            out.push_str(&format!("   {}\n", description.trim()));
        }
        let fields = input_fields(agent.get("input_schema").unwrap_or(&Value::Null));
        if !fields.is_empty() {
            out.push_str(&format!("   Input: {}\n", fields.join(", ")));
        }
        if let Some(example) = agent
            .get("examples")
            .and_then(Value::as_array)
            .and_then(|examples| examples.first())
        {
            let input = example.get("input").cloned().unwrap_or(Value::Null);
            let note = example.get("note").and_then(Value::as_str).unwrap_or("");
            out.push_str(&format!("   Example input: {input} — {note}\n"));
        }
    }
    out.trim_end().to_string()
}

/// An input schema's top-level fields as `name (type[, required])`, required ones first.
fn input_fields(schema: &Value) -> Vec<String> {
    let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
        return Vec::new();
    };
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut fields: Vec<(bool, &String, String)> = properties
        .iter()
        .map(|(name, property)| {
            let kind = if let Some(values) = property.get("enum").and_then(Value::as_array) {
                values
                    .iter()
                    .map(Value::to_string)
                    .collect::<Vec<_>>()
                    .join(" | ")
            } else {
                match property.get("type").and_then(Value::as_str) {
                    Some("array") => format!(
                        "{}[]",
                        property
                            .pointer("/items/type")
                            .and_then(Value::as_str)
                            .unwrap_or("value")
                    ),
                    Some(kind) => kind.to_string(),
                    None => "value".to_string(),
                }
            };
            (required.contains(&name.as_str()), name, kind)
        })
        .collect();
    fields.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    fields
        .into_iter()
        .map(|(required, name, kind)| {
            if required {
                format!("{name} ({kind}, required)")
            } else {
                format!("{name} ({kind})")
            }
        })
        .collect()
}

fn format_task(task: &Value) -> String {
    let id = task.get("id").and_then(Value::as_str).unwrap_or("?");
    let agent = task.get("agent").and_then(Value::as_str).unwrap_or("?");
    let status = task.get("status").and_then(Value::as_str).unwrap_or("?");
    let mut out = format!("Task {id} ({agent}): {status}");
    if let Some(stage) = task.get("stage").and_then(Value::as_str) {
        if task.get("done").and_then(Value::as_bool) != Some(true) {
            out.push_str(&format!(", at {stage}"));
        }
    }
    match (
        task.get("charged_credits").and_then(Value::as_i64),
        task.get("price_credits").and_then(Value::as_i64),
    ) {
        (Some(charged), _) => out.push_str(&format!(". Charged {charged} credits.")),
        (None, Some(price)) if task.get("done").and_then(Value::as_bool) == Some(true) => out
            .push_str(&format!(
                ". Not charged (price {price} credits applies only on success)."
            )),
        (None, Some(price)) => out.push_str(&format!(". Price {price} credits if it succeeds.")),
        _ => out.push('.'),
    }
    if let Some(output) = task.get("output").filter(|o| !o.is_null()) {
        if let Some(summary) = output.get("summary").and_then(Value::as_str) {
            out.push_str(&format!("\n\nSummary: {summary}"));
        }
        if let Some(reason) = output.pointer("/declined/reason").and_then(Value::as_str) {
            out.push_str(&format!("\nDeclined: {reason}"));
        }
        if let Some(data) = output.get("data").filter(|d| !d.is_null()) {
            out.push_str(&format!("\n\nResult:\n{data}"));
        }
    } else if task.get("done").and_then(Value::as_bool) != Some(true) {
        out.push_str(&format!(
            "\n\nStill working: call agent(action=\"get_run\", task_id=\"{id}\", wait_ms=20000)."
        ));
    }
    if let Some(error) = task.get("error").and_then(Value::as_str) {
        out.push_str(&format!("\nError: {error}"));
    }
    out
}

fn format_task_list(list: &Value) -> String {
    let tasks = list
        .get("tasks")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if tasks.is_empty() {
        return "No agent runs in this workspace yet.".to_string();
    }
    let mut out = format!("{} run(s):\n\n", tasks.len());
    for (i, task) in tasks.iter().enumerate() {
        let id = task.get("id").and_then(Value::as_str).unwrap_or("?");
        let agent = task.get("agent").and_then(Value::as_str).unwrap_or("?");
        let status = task.get("status").and_then(Value::as_str).unwrap_or("?");
        let created = task.get("created_at").and_then(Value::as_str).unwrap_or("");
        out.push_str(&format!(
            "{}. {agent} — {status} (task_id: {id}) {created}\n",
            i + 1
        ));
    }
    out.trim_end().to_string()
}

/// Register the specialist agent tool.
pub fn register_agent_tools(
    registry: &mut crate::registry::ToolRegistry,
    client: ContextStreamClient,
) {
    registry.register("agent", Arc::new(AgentTool::new(client)));
}

#[cfg(test)]
#[path = "agent_tests.rs"]
mod tests;
