//! ContextStream Agents (internally Slipstream): specialist agents a person hands one job, run
//! by ContextStream's Slipstream worker and paid per successful run in credits (ContextStream
//! `handlers/tasks_api.rs`).
//!
//! One tool, `agents`, listed only for a caller whose workspace has the agents with at least
//! one turned on (see [`gate`]); its description is written from that catalog (see
//! [`describe`]). The tool's first name, `slipstream`, stays callable as a back-compat alias
//! ([`LEGACY_TOOL_NAME`]) and is never listed (ContextStream decision bd3949a7: the public
//! product is ContextStream Agents).
//!
//! ## Actions
//!
//! - `find`: the workspace's agents, each with its job, price, input and whether it is on.
//! - `run`: ask an agent for a task (`POST /workspaces/{id}/tasks`, `source: "mcp"`). A priced
//!   agent runs only with the person's accepted price: without `accepted_price_credits` the
//!   answer is the quote and nothing runs, unless the person set an auto-accept cap at or
//!   above the price ([`AUTO_ACCEPT_ENV`]). The input is checked against the agent's schema
//!   first, and a required `project_id` defaults to the session's project. Every dispatch
//!   carries an idempotency key, so a retried call returns the same task.
//! - `get_run`: one task with its result, waiting up to 25 s for it to end.
//! - `list_runs`: the workspace's tasks, newest first.
//! - `cancel_run`: stop a task.
//!
//! A run is charged its price once, only when it succeeds; a declined, failed, cancelled or
//! timed-out run costs nothing.

pub(crate) mod catalog;
pub(crate) mod describe;
pub(crate) mod format;
pub(crate) mod gate;

use async_trait::async_trait;
use mcp_client::{ContextStreamClient, RequestOptions};
use mcp_session::SessionManager;
use mcp_types::{
    tool::{ToolAnnotations, ToolCategory, ToolMetadata, ToolResult},
    Error, Result,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

use crate::registry::ToolHandler;
use crate::schema::SchemaBuilder;

use catalog::{agent_id_of, Catalog};
use format::Quote;
pub use gate::SlipstreamGate;
use gate::{Availability, GateEntry, GateKey};

/// The tool's name.
pub const TOOL_NAME: &str = "agents";
/// The tool's name before the rename (MCP 1.0.30 to 1.0.34): still callable, never listed, so
/// clients and saved prompts that call it keep working.
pub const LEGACY_TOOL_NAME: &str = "slipstream";
/// The person's auto-accept cap for stdio: runs priced at most this many credits start
/// without a quote. `0` (the default) always quotes.
pub const AUTO_ACCEPT_ENV: &str = "CONTEXTSTREAM_AGENTS_AUTO_ACCEPT_MAX_CREDITS";
/// The cap's first name, read when [`AUTO_ACCEPT_ENV`] is not set.
pub const LEGACY_AUTO_ACCEPT_ENV: &str = "CONTEXTSTREAM_SLIPSTREAM_AUTO_ACCEPT_MAX_CREDITS";
/// The same cap for a hosted (HTTP) connection, sent per request by the person's client.
pub const AUTO_ACCEPT_HEADER: &str = "x-contextstream-agents-auto-accept-max-credits";
/// The header's first name, read when [`AUTO_ACCEPT_HEADER`] is not sent.
pub const LEGACY_AUTO_ACCEPT_HEADER: &str = "x-contextstream-slipstream-auto-accept-max-credits";

/// Whether `name` calls this tool: its name or its back-compat alias.
pub fn is_tool_name(name: &str) -> bool {
    name == TOOL_NAME || name == LEGACY_TOOL_NAME
}

const VALID_ACTIONS: &[&str] = &["find", "run", "get_run", "list_runs", "cancel_run"];

/// The longest ContextStream holds a request open for a task to end.
pub(crate) const MAX_WAIT_MS: u64 = 25_000;
/// What `run` and `get_run` wait by default, so a quick agent answers in the same call.
pub(crate) const DEFAULT_RUN_WAIT_MS: u64 = 20_000;
const MAX_LIST_LIMIT: i64 = 50;
/// What a request may take beyond the wait it asks the API for.
const REQUEST_SLACK: Duration = Duration::from_secs(15);
/// How long `find` and `run` wait for the catalog.
const CATALOG_READ_BUDGET: Duration = Duration::from_secs(10);
/// How long a listing waits for the catalog before leaving the tool out.
pub const LISTING_BUDGET: Duration = Duration::from_millis(2500);
/// How long the after-call check waits for a workspace it has never read.
const CHANGE_CHECK_BUDGET: Duration = Duration::from_millis(1500);
/// An identical ask (agent, input, project, session) inside one window is one task.
const IDEMPOTENCY_WINDOW_SECS: u64 = 600;
/// The API's bound on a session id.
const MAX_SESSION_ID_CHARS: usize = 200;

const TOOL_REFERENCE: &str = "ContextStream Agents: each does one job with the workspace's decisions, lessons, docs and code, and declines anything else (a declined run is free).\n\nActions:\n- find: the workspace's agents with job, price per run, input fields (with what each means) and an example; on = an owner or admin turned it on.\n- run: agent (id or id@version), input (the agent's input object; a required project_id defaults to the session's project). A priced agent needs accepted_price_credits at or above its price: call run without it to get the quote, show the person the price, then call again with their acceptance. Runs priced within the person's auto-accept cap start at once. wait_ms (default 20000, up to 25000) waits for the result; a run still working returns its task_id for get_run. An identical ask within 10 minutes returns the same task; pass a new idempotency_key to run the same job again.\n- get_run: task_id, wait_ms (default 20000, up to 25000): status (queued, running, succeeded, failed, declined, cancelled, timed_out), the result, and what the run cost.\n- list_runs: agent (optional filter), limit (at most 50).\n- cancel_run: task_id.\n\nBilling: a run costs its price once, only when it succeeds.";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SlipstreamInput {
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

pub struct SlipstreamTool {
    client: ContextStreamClient,
    session: Arc<SessionManager>,
    gate: Arc<SlipstreamGate>,
}

impl SlipstreamTool {
    pub fn new(
        client: ContextStreamClient,
        session: Arc<SessionManager>,
        gate: Arc<SlipstreamGate>,
    ) -> Self {
        Self {
            client,
            session,
            gate,
        }
    }

    /// The workspace and project a call works in: what it names, else the session's.
    async fn scope(&self, input: &SlipstreamInput) -> Result<(Uuid, Option<Uuid>)> {
        let scope = crate::domains::scope::resolve_read_scope(
            &self.client,
            &self.session,
            input.workspace_id.as_deref(),
            input.project_id.as_deref(),
        )
        .await?;
        let workspace_id = scope.workspace_id.ok_or_else(|| {
            Error::Validation(
                "No workspace: run init for this folder, or pass workspace_id".to_string(),
            )
        })?;
        Ok((workspace_id, scope.project_id))
    }

    fn task_id(input: &Option<String>) -> Result<Uuid> {
        let value = input
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| Error::Validation("task_id is required".to_string()))?;
        Uuid::parse_str(value).map_err(|_| Error::Validation("Invalid task_id".to_string()))
    }

    /// The catalog read now, recorded for the listing. An unreachable catalog falls back to
    /// the last one read, if any.
    async fn read_catalog(&self, workspace_id: Uuid) -> Availability {
        let availability =
            gate::read_catalog(&self.client, workspace_id, CATALOG_READ_BUDGET).await;
        match caller_identity(&self.client).await {
            Some(identity) => {
                let key = GateKey {
                    identity,
                    workspace_id,
                };
                let recorded = self.gate.record(&key, availability.clone());
                match availability {
                    Availability::Unreachable(_) if recorded.availability.catalog().is_some() => {
                        recorded.availability
                    }
                    _ => availability,
                }
            }
            None => availability,
        }
    }

    /// The catalog for reading a decline: the cached one, else one read now.
    async fn known_catalog(&self, workspace_id: Uuid) -> Option<Arc<Catalog>> {
        if let Some(identity) = caller_identity(&self.client).await {
            let key = GateKey {
                identity,
                workspace_id,
            };
            if let Some(catalog) = self
                .gate
                .entry(&key)
                .and_then(|entry| entry.availability.catalog().cloned())
            {
                return Some(catalog);
            }
        }
        self.read_catalog(workspace_id).await.catalog().cloned()
    }

    async fn auto_accept_max_credits(&self) -> u32 {
        self.client
            .config()
            .await
            .slipstream_auto_accept_max_credits
    }

    async fn find(&self, input: &SlipstreamInput) -> Result<ToolResult> {
        let (workspace_id, _) = self.scope(input).await?;
        match self.read_catalog(workspace_id).await {
            Availability::Available(catalog) | Availability::NoneEnabled(catalog) => {
                let cap = self.auto_accept_max_credits().await;
                Ok(ToolResult::with_structured(
                    format::catalog_text(&catalog, cap),
                    json!({
                        "workspace_id": workspace_id,
                        "auto_accept_max_credits": cap,
                        "agents": catalog.agents.iter().map(agent_json).collect::<Vec<_>>(),
                    }),
                ))
            }
            Availability::Refused(message) => Ok(not_open(workspace_id, &message)),
            Availability::Unreachable(message) => Err(Error::Network(format!(
                "The agent catalog could not be read: {message}"
            ))),
        }
    }

    async fn run(&self, input: &SlipstreamInput) -> Result<ToolResult> {
        let agent = input
            .agent
            .as_deref()
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .ok_or_else(|| Error::Validation("agent is required for run".to_string()))?
            .to_string();
        if !is_agent_id(agent_id_of(&agent)) {
            return Err(Error::Validation(format!("{agent} is not an agent id")));
        }
        let task_input = match input.input.clone() {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(map)) => map,
            Some(_) => {
                return Err(Error::Validation(
                    "input must be the agent's input object".to_string(),
                ))
            }
        };
        let (workspace_id, project_id) = self.scope(input).await?;

        let catalog = match self.read_catalog(workspace_id).await {
            Availability::Available(catalog) | Availability::NoneEnabled(catalog) => catalog,
            Availability::Refused(message) => return Ok(not_open(workspace_id, &message)),
            Availability::Unreachable(message) => {
                return Err(Error::Network(format!(
                    "The agent catalog could not be read, so nothing was run: {message}"
                )))
            }
        };
        let Some(entry) = catalog.find(&agent) else {
            let enabled: Vec<&str> = catalog.enabled().iter().map(|a| a.id.as_str()).collect();
            return Err(Error::Validation(format!(
                "No agent {agent} in this workspace's catalog. Turned on here: {}.",
                if enabled.is_empty() {
                    "none".to_string()
                } else {
                    enabled.join(", ")
                }
            )));
        };
        if !entry.enabled {
            return Ok(ToolResult::error(format!(
                "{} ({}) is not turned on in this workspace: an owner or admin turns it on in the workspace's agent settings. Nothing was run.",
                entry.id, entry.name
            )));
        }
        let project_id = project_id.ok_or_else(|| {
            Error::Validation(
                "No project: run init for this folder, or pass project_id (a run belongs to a project)"
                    .to_string(),
            )
        })?;
        let prepared = catalog::prepare_input(entry, task_input, Some(project_id))
            .map_err(Error::Validation)?;

        // The price: accepted by the person, or under their auto-accept cap.
        let cap = self.auto_accept_max_credits().await;
        let mut notes = prepared.notes.clone();
        let accepted = match (entry.quoted_price(), input.accepted_price_credits) {
            (None, accepted) => accepted,
            (Some(price), Some(accepted)) if accepted >= price => Some(accepted),
            (Some(price), None) if cap > 0 && price <= i64::from(cap) => {
                notes.push(format!(
                    "The {price}-credit price was accepted by the person's auto-accept cap of {cap} credits."
                ));
                Some(price)
            }
            (Some(price), raised_from) => {
                let quote = Quote {
                    agent: entry.id.clone(),
                    name: entry.name.clone(),
                    price,
                    raised_from,
                    auto_accept_max_credits: cap,
                };
                return Ok(ToolResult::with_structured(
                    quote.text(&prepared.input),
                    json!({
                        "ran": false,
                        "agent": entry.id,
                        "price_credits": price,
                        "accepted_price_credits": raised_from,
                        "auto_accept_max_credits": cap,
                        "input": prepared.input,
                        "accept_with": {"accepted_price_credits": price},
                    }),
                ));
            }
        };

        let state = self.session.state().await;
        let session_id = input
            .session_id
            .as_deref()
            .or(state.api_session_id.as_deref())
            .or(state.session_id.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty() && s.chars().count() <= MAX_SESSION_ID_CHARS)
            .map(str::to_string);
        let identity = caller_identity(&self.client).await;
        let idempotency_key = match input
            .idempotency_key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty())
        {
            Some(key) if key.chars().count() <= 200 => key.to_string(),
            Some(_) => {
                return Err(Error::Validation(
                    "idempotency_key is at most 200 characters".to_string(),
                ))
            }
            None => derived_idempotency_key(
                identity.as_deref().unwrap_or("anonymous"),
                workspace_id,
                project_id,
                &agent,
                &prepared.input,
                session_id.as_deref(),
                chrono::Utc::now().timestamp().max(0) as u64,
            ),
        };
        let wait_ms = input
            .wait_ms
            .unwrap_or(DEFAULT_RUN_WAIT_MS)
            .min(MAX_WAIT_MS);
        let mut body = json!({
            "agent": agent,
            "input": prepared.input,
            "context": { "project_id": project_id },
            "wait_ms": wait_ms,
            "source": "mcp",
            "idempotency_key": idempotency_key,
        });
        if let Some(accepted) = accepted {
            body["accepted_price_credits"] = json!(accepted);
        }
        if let Some(session_id) = &session_id {
            body["session_id"] = json!(session_id);
        }
        let asked_at = chrono::Utc::now();
        let sent: Result<Value> = self
            .client
            .post_with_options(
                &format!("/workspaces/{workspace_id}/tasks"),
                body,
                RequestOptions {
                    timeout: Some(Duration::from_millis(wait_ms) + REQUEST_SLACK),
                    ..RequestOptions::default()
                },
            )
            .await;
        let task = match sent {
            Ok(response) => unwrap_data(response),
            Err(error) => {
                return self
                    .refused_run(error, workspace_id, entry.id.as_str(), &prepared.input)
                    .await
            }
        };
        if replayed_task(&task, asked_at) {
            notes.push(format!(
                "This is the task an identical earlier ask started (idempotency key {idempotency_key}); pass a new idempotency_key to run the job again."
            ));
        }
        Ok(task_result(task, Some(catalog.as_ref()), &notes))
    }

    /// A run the API refused: a raised price is a new quote; the rest say what to do.
    async fn refused_run(
        &self,
        error: Error,
        workspace_id: Uuid,
        agent: &str,
        input: &Value,
    ) -> Result<ToolResult> {
        match error {
            Error::Http {
                status: 409,
                ref message,
                ..
            } if message.contains("now costs") => {
                let availability = self.read_catalog(workspace_id).await;
                let entry = availability.catalog().and_then(|c| c.find(agent)).cloned();
                match entry.and_then(|e| e.quoted_price().map(|price| (e, price))) {
                    Some((entry, price)) => {
                        let quote = Quote {
                            agent: entry.id.clone(),
                            name: entry.name.clone(),
                            price,
                            raised_from: None,
                            auto_accept_max_credits: self.auto_accept_max_credits().await,
                        };
                        Ok(ToolResult::with_structured(
                            format!("The price changed. {}", quote.text(input)),
                            json!({"ran": false, "agent": entry.id, "price_credits": price,
                                   "accept_with": {"accepted_price_credits": price}}),
                        ))
                    }
                    None => Ok(ToolResult::error(format!("Not run: {message}"))),
                }
            }
            Error::Http {
                status: 401 | 403,
                ref message,
                ..
            } => {
                if let Some(identity) = caller_identity(&self.client).await {
                    self.gate.record(
                        &GateKey {
                            identity,
                            workspace_id,
                        },
                        Availability::Refused(message.clone()),
                    );
                }
                Ok(ToolResult::error(format!("Not run: {message}")))
            }
            Error::Http {
                status: 400 | 404 | 409 | 422,
                ref message,
                ..
            } => Ok(ToolResult::error(format!("Not run: {message}"))),
            Error::InsufficientCredits {
                required,
                available,
                ..
            } => Ok(ToolResult::error(format!(
                "Not run: not enough credits for {agent} (needs {}, {} available). Nothing was charged.",
                required.map_or("its price".to_string(), |r| r.to_string()),
                available.map_or("fewer".to_string(), |a| a.to_string()),
            ))),
            other => Err(other),
        }
    }

    async fn get_run(&self, input: &SlipstreamInput) -> Result<ToolResult> {
        let task_id = Self::task_id(&input.task_id)?;
        let (workspace_id, _) = self.scope(input).await?;
        let wait = input
            .wait_ms
            .unwrap_or(DEFAULT_RUN_WAIT_MS)
            .min(MAX_WAIT_MS);
        let path = if wait > 0 {
            format!("/workspaces/{workspace_id}/tasks/{task_id}?wait_ms={wait}")
        } else {
            format!("/workspaces/{workspace_id}/tasks/{task_id}")
        };
        let response: Value = self
            .client
            .get_with_options(
                &path,
                RequestOptions {
                    timeout: Some(Duration::from_millis(wait) + REQUEST_SLACK),
                    ..RequestOptions::default()
                },
            )
            .await?;
        let task = unwrap_data(response);
        let declined = task
            .pointer("/output/declined/suggested_agent")
            .is_some_and(Value::is_string);
        let catalog = if declined {
            self.known_catalog(workspace_id).await
        } else {
            None
        };
        Ok(task_result(task, catalog.as_deref(), &[]))
    }

    async fn list_runs(&self, input: &SlipstreamInput) -> Result<ToolResult> {
        let agent = match input
            .agent
            .as_deref()
            .map(str::trim)
            .filter(|a| !a.is_empty())
        {
            Some(agent) if !is_agent_id(agent_id_of(agent)) => {
                return Err(Error::Validation(format!("{agent} is not an agent id")))
            }
            Some(agent) => Some(agent_id_of(agent).to_string()),
            None => None,
        };
        let (workspace_id, _) = self.scope(input).await?;
        let limit = input.limit.unwrap_or(20).clamp(1, MAX_LIST_LIMIT);
        let mut path = format!("/workspaces/{workspace_id}/tasks?limit={limit}");
        if let Some(agent) = agent {
            path.push_str(&format!("&agent={agent}"));
        }
        let response: Value = self.client.get(&path).await?;
        let list = unwrap_data(response);
        Ok(ToolResult::with_structured(
            format::task_list_text(&list),
            list,
        ))
    }

    async fn cancel_run(&self, input: &SlipstreamInput) -> Result<ToolResult> {
        let task_id = Self::task_id(&input.task_id)?;
        let (workspace_id, _) = self.scope(input).await?;
        let response: Value = self
            .client
            .post(
                &format!("/workspaces/{workspace_id}/tasks/{task_id}/cancel"),
                json!({}),
            )
            .await?;
        Ok(task_result(unwrap_data(response), None, &[]))
    }
}

#[async_trait]
impl ToolHandler for SlipstreamTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let input: SlipstreamInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;
        let action = input.action.trim().to_lowercase();
        match action.as_str() {
            "find" => self.find(&input).await,
            "run" => self.run(&input).await,
            "get_run" => self.get_run(&input).await,
            "list_runs" => self.list_runs(&input).await,
            "cancel_run" => self.cancel_run(&input).await,
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
            name: TOOL_NAME.to_string(),
            title: "ContextStream Agents".to_string(),
            description: describe::STATIC_DESCRIPTION.to_string(),
            category: ToolCategory::Ai,
            annotations: ToolAnnotations::destructive(),
            is_pro: false,
            required_tier: None,
        })
    }

    fn input_schema(&self) -> Value {
        input_schema()
    }
}

/// The tool's input schema before a catalog is known.
pub(crate) fn input_schema() -> Value {
    SchemaBuilder::new()
        .description("ContextStream Agents operations")
        .string_enum(
            "action",
            &crate::schema::with_full_reference("Action to perform", TOOL_REFERENCE),
            VALID_ACTIONS,
            true,
        )
        .string(
            "agent",
            "Agent id, or id@version (run; a filter for list_runs). find lists them.",
            false,
        )
        .object(
            "input",
            "The agent's input object (run). find lists each agent's fields; a required project_id defaults to the session's project.",
            false,
        )
        .integer(
            "accepted_price_credits",
            "The price per run the person accepted (run): needed for a priced agent unless it is within the person's auto-accept cap",
            false,
        )
        .integer(
            "wait_ms",
            "Milliseconds to wait for the result (run, get_run): default 20000, at most 25000",
            false,
        )
        .uuid("task_id", "Task ID (get_run, cancel_run)", false)
        .integer("limit", "Maximum tasks (list_runs), at most 50", false)
        .uuid(
            "project_id",
            "Project for run (defaults to the session's)",
            false,
        )
        .uuid(
            "workspace_id",
            "Workspace (defaults to the session's)",
            false,
        )
        .string(
            "idempotency_key",
            "Key for one logical run (run): an identical ask within 10 minutes is one task without it",
            false,
        )
        .string(
            "session_id",
            "The caller's session, recorded on the task (defaults to this session)",
            false,
        )
        .build()
}

fn not_open(workspace_id: Uuid, message: &str) -> ToolResult {
    ToolResult::error(format!(
        "ContextStream Agents are not available in workspace {workspace_id}: {message}"
    ))
}

fn agent_json(agent: &catalog::AgentEntry) -> Value {
    json!({
        "id": agent.id,
        "version": agent.version,
        "name": agent.name,
        "description": agent.description,
        "price_credits": agent.price_credits,
        "timeout_minutes": agent.timeout_minutes,
        "enabled": agent.enabled,
        "input_schema": agent.input_schema,
        "examples": agent.examples.iter().map(|e| json!({"input": e.input, "note": e.note})).collect::<Vec<_>>(),
    })
}

fn task_result(task: Value, catalog: Option<&Catalog>, notes: &[String]) -> ToolResult {
    let text = format::task_text(&task, catalog, notes);
    let mut structured = task;
    if let Some(map) = structured.as_object_mut() {
        if let Some(id) = map.get("id").cloned() {
            map.insert("task_id".to_string(), id.clone());
            if map.get("done").and_then(Value::as_bool) != Some(true) {
                map.insert(
                    "follow_up".to_string(),
                    json!({"action": "get_run", "task_id": id, "wait_ms": MAX_WAIT_MS}),
                );
            }
        }
        if !notes.is_empty() {
            map.insert("notes".to_string(), json!(notes));
        }
    }
    ToolResult::with_structured(text, structured)
}

/// A task created more than a few seconds before this ask: an earlier identical ask's.
fn replayed_task(task: &Value, asked_at: chrono::DateTime<chrono::Utc>) -> bool {
    task.get("created_at")
        .and_then(Value::as_str)
        .and_then(|created| chrono::DateTime::parse_from_rfc3339(created).ok())
        .is_some_and(|created| {
            created.with_timezone(&chrono::Utc) < asked_at - chrono::Duration::seconds(5)
        })
}

/// ContextStream wraps answers as `{"success": true, "data": ...}`; the client may already
/// have unwrapped it.
pub(crate) fn unwrap_data(response: Value) -> Value {
    match response {
        Value::Object(mut map) if map.contains_key("data") && map.contains_key("success") => {
            map.remove("data").unwrap_or(Value::Null)
        }
        other => other,
    }
}

pub(crate) fn is_agent_id(id: &str) -> bool {
    let mut chars = id.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && id.len() <= 64
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// The same JSON with every object's keys in order, so equal inputs hash equally.
fn canonical(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut sorted = Map::new();
            for key in keys {
                sorted.insert(key.clone(), canonical(&map[key]));
            }
            Value::Object(sorted)
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical).collect()),
        other => other.clone(),
    }
}

/// One key per logical ask: who, where, which agent, which input and session, and the
/// 10-minute window it falls in. A retried call inside the window gets the task the first
/// call started instead of a second one.
pub(crate) fn derived_idempotency_key(
    identity: &str,
    workspace_id: Uuid,
    project_id: Uuid,
    agent: &str,
    input: &Value,
    session_id: Option<&str>,
    now_secs: u64,
) -> String {
    let mut hasher = Sha256::new();
    let window = (now_secs / IDEMPOTENCY_WINDOW_SECS).to_string();
    let input = canonical(input).to_string();
    let workspace = workspace_id.to_string();
    let project = project_id.to_string();
    for field in [
        "slipstream-run-v1",
        identity,
        &workspace,
        &project,
        agent_id_of(agent),
        &input,
        session_id.unwrap_or(""),
        &window,
    ] {
        hasher.update((field.len() as u64).to_be_bytes());
        hasher.update(field.as_bytes());
    }
    format!("mcp-{}", &hex::encode(hasher.finalize())[..40])
}

// ---------------------------------------------------------------------------------------
// Listing: what tools/list shows for this caller
// ---------------------------------------------------------------------------------------

/// A secret-free identity for the caller: the hosted gateway's per-principal identity, or,
/// for the single-person stdio server, a hash of the credential it runs with.
pub(crate) async fn caller_identity(client: &ContextStreamClient) -> Option<String> {
    if let Some(identity) = mcp_client::get_task_caller_cache_identity() {
        return Some(identity);
    }
    let config = client.config().await;
    if config.is_http_transport {
        return None;
    }
    let credential = config.api_key.or(config.jwt)?;
    let digest = Sha256::digest(credential.as_bytes());
    Some(format!("local:{}", &hex::encode(digest)[..16]))
}

/// The workspace a listing is for: the session's, the hosted connection's
/// (`X-ContextStream-Workspace-Id`), and on stdio the configured default or the folder's
/// mapping. `None` lists no tool.
pub(crate) async fn listing_workspace(
    client: &ContextStreamClient,
    session: &SessionManager,
) -> Option<Uuid> {
    let state = session.state().await;
    if let Some(workspace_id) = state.workspace_id {
        return Some(workspace_id);
    }
    if let Some(workspace_id) = mcp_client::get_task_auth_override().and_then(|a| a.workspace_id) {
        return Some(workspace_id);
    }
    let config = client.config().await;
    if config.is_http_transport {
        return None;
    }
    if let Some(workspace_id) = config.default_workspace_id {
        return Some(workspace_id);
    }
    let folder = state.folder_path.or_else(|| {
        std::env::current_dir()
            .ok()
            .map(|path| path.to_string_lossy().into_owned())
    })?;
    mcp_session::auto_init::resolve_workspace(&folder)
        .await
        .map(|mapping| mapping.workspace_id)
}

/// What tools/list shows of the agents tool for one caller.
#[derive(Debug, Clone, PartialEq)]
pub struct SlipstreamListing {
    identity: Option<String>,
    /// The description and input schema, when the tool is listed.
    listed: Option<(String, Value)>,
}

impl SlipstreamListing {
    fn hidden(identity: Option<String>) -> Self {
        Self {
            identity,
            listed: None,
        }
    }

    fn from_entry(
        identity: Option<String>,
        entry: &GateEntry,
        base_schema: &Value,
        auto_accept_max_credits: u32,
    ) -> Self {
        let availability = &entry.availability;
        match availability.catalog().filter(|_| availability.is_listed()) {
            Some(catalog) => {
                let described = describe::describe(catalog, auto_accept_max_credits);
                Self {
                    identity,
                    listed: Some((
                        described.description.clone(),
                        describe::schema_with(base_schema, &described),
                    )),
                }
            }
            None => Self::hidden(identity),
        }
    }

    pub fn is_listed(&self) -> bool {
        self.listed.is_some()
    }

    pub fn description(&self) -> Option<&str> {
        self.listed
            .as_ref()
            .map(|(description, _)| description.as_str())
    }

    pub fn input_schema(&self) -> Option<&Value> {
        self.listed.as_ref().map(|(_, schema)| schema)
    }

    /// Put this caller's description and schema into a tools/list entry.
    pub fn apply(&self, tool_entry: &mut Value) {
        if let (Some((description, schema)), Some(map)) = (&self.listed, tool_entry.as_object_mut())
        {
            map.insert(
                "description".to_string(),
                Value::String(description.clone()),
            );
            map.insert("inputSchema".to_string(), schema.clone());
        }
    }

    /// A short hash of what a client would see, to notice a change.
    pub fn fingerprint(&self) -> u64 {
        let mut hasher = Sha256::new();
        match &self.listed {
            None => hasher.update(b"hidden"),
            Some((description, schema)) => {
                hasher.update(description.as_bytes());
                hasher.update(schema.to_string().as_bytes());
            }
        }
        let digest = hasher.finalize();
        u64::from_be_bytes(digest[..8].try_into().unwrap_or([0; 8]))
    }

    /// Remember this as what the caller was last shown.
    pub fn mark_advertised(&self, gate: &SlipstreamGate) {
        if let Some(identity) = &self.identity {
            gate.set_advertised(identity, self.fingerprint());
        }
    }
}

/// The listing for the current request, reading the catalog when the cached answer is not
/// fresh and waiting at most `budget` for it.
pub async fn listing(
    gate: &SlipstreamGate,
    client: &ContextStreamClient,
    session: &SessionManager,
    base_schema: &Value,
    budget: Duration,
) -> SlipstreamListing {
    let identity = caller_identity(client).await;
    let (Some(identity), Some(workspace_id)) =
        (identity.clone(), listing_workspace(client, session).await)
    else {
        return SlipstreamListing::hidden(identity);
    };
    let key = GateKey {
        identity: identity.clone(),
        workspace_id,
    };
    let entry = gate.ensure(client, &key, budget).await;
    let cap = client.config().await.slipstream_auto_accept_max_credits;
    SlipstreamListing::from_entry(Some(identity), &entry, base_schema, cap)
}

/// Read the catalog for the current caller in the background, so the tools/list that
/// follows an initialize finds it ready.
pub fn prefetch(
    gate: Arc<SlipstreamGate>,
    client: ContextStreamClient,
    session: Arc<SessionManager>,
) {
    mcp_client::spawn_with_task_context(async move {
        let identity = caller_identity(&client).await;
        let workspace_id = listing_workspace(&client, &session).await;
        if let (Some(identity), Some(workspace_id)) = (identity, workspace_id) {
            let key = GateKey {
                identity,
                workspace_id,
            };
            gate.ensure(&client, &key, CATALOG_READ_BUDGET).await;
        }
    });
}

/// After a tool call: whether this caller's listing now differs from the one it was last
/// shown (another workspace, an agent turned on or off, Slipstream opened or closed). A
/// workspace never read is read now (briefly); a stale answer is refreshed in the
/// background and noticed on a later call. `false` until a listing was shown.
pub async fn listing_changed(
    gate: &Arc<SlipstreamGate>,
    client: &ContextStreamClient,
    session: &SessionManager,
    base_schema: &Value,
) -> bool {
    let Some(identity) = caller_identity(client).await else {
        return false;
    };
    let Some(advertised) = gate.advertised(&identity) else {
        return false;
    };
    let current = match listing_workspace(client, session).await {
        None => SlipstreamListing::hidden(Some(identity.clone())),
        Some(workspace_id) => {
            let key = GateKey {
                identity: identity.clone(),
                workspace_id,
            };
            let entry = match gate.entry(&key) {
                Some(entry) if entry.is_fresh() => entry,
                Some(stale) => {
                    let gate = gate.clone();
                    let client_for_refresh = client.clone();
                    let key = key.clone();
                    mcp_client::spawn_with_task_context(async move {
                        gate.ensure(&client_for_refresh, &key, CATALOG_READ_BUDGET)
                            .await;
                    });
                    stale
                }
                None => gate.ensure(client, &key, CHANGE_CHECK_BUDGET).await,
            };
            let cap = client.config().await.slipstream_auto_accept_max_credits;
            SlipstreamListing::from_entry(Some(identity.clone()), &entry, base_schema, cap)
        }
    };
    let changed = current.fingerprint() != advertised;
    if changed {
        current.mark_advertised(gate);
    }
    changed
}

/// Register the agents tool, and its first name as a back-compat alias that is never listed.
pub fn register_slipstream_tools(
    registry: &mut crate::registry::ToolRegistry,
    client: ContextStreamClient,
    session: Arc<SessionManager>,
) {
    registry.register(
        TOOL_NAME,
        Arc::new(SlipstreamTool::new(
            client,
            session,
            SlipstreamGate::global(),
        )),
    );
    registry.register_alias(LEGACY_TOOL_NAME, TOOL_NAME);
}

#[cfg(test)]
mod tests;
