//! Tools the ContextStream API defines for the caller ("hosted tools").
//!
//! `tools/list` adds the caller's hosted tools beside the registry's own. Their
//! definitions come from the API (`GET /mcp/hosted-tools`) and are cached per
//! caller, as studio's offer is: 5 minutes once known, and read again after
//! 30 s when the API could not be reached, keeping the tools known before so a
//! blip never takes one away mid-session. A route the API doesn't have, or a
//! caller it defines nothing for, lists nothing.
//!
//! A call to a hosted tool runs through the transport its definition names:
//! - `api`: the API runs it (`POST /mcp/hosted-tools/{name}/call`) and answers
//!   with the result;
//! - `stream`: the API answers that call with a WebSocket address, a token for
//!   that one call and a description of the run. This client opens the socket
//!   with the token (never its API key), sends the description with the
//!   checkout (`mcp_client::frames`), answers what the service asks for of the
//!   checkout, and returns the result the service made. The service's
//!   heartbeats become progress notifications. When the service does not run
//!   the call, or cannot be reached, the caller is told to do it itself.
//!
//! A caller that gives up on a call (its own timeout came first) and asks the
//! same thing again, of the same checkout, is handed the finished result
//! instead of a second run.
//!
//! A hosted tool never shadows a tool the registry has.

use crate::domains::slipstream::listing_workspace;
use crate::peer;
use mcp_client::checkout_snapshot::{CheckoutSnapshot, SnapshotError};
use mcp_client::frames::{self, Event, ExchangeError};
use mcp_client::hosted_tools::HostedToolsAnswer;
use mcp_client::ContextStreamClient;
use mcp_session::SessionManager;
use mcp_types::tool::{ContentItem, ToolResult};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How long a listing waits for the caller's definitions.
pub const LISTING_BUDGET: Duration = Duration::from_millis(2500);
/// How long the check after a tool call waits for them.
pub const CHANGE_CHECK_BUDGET: Duration = Duration::from_millis(500);
pub(crate) const KNOWN_TTL: Duration = Duration::from_secs(300);
pub(crate) const UNREACHABLE_RETRY: Duration = Duration::from_secs(30);
/// Callers remembered at once; beyond it the oldest reads go first.
const MAX_ENTRIES: usize = 4096;
/// Hosted tools listed for one caller at most.
pub const MAX_TOOLS: usize = 16;
/// How long one hosted call may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(300);
/// Set to `off` to read no definitions and list no hosted tools.
pub const ENV_SWITCH: &str = "CONTEXTSTREAM_HOSTED_TOOLS";

/// Whether the value of [`ENV_SWITCH`] turns hosted tools off.
fn off_by(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "off" | "0" | "false" | "no" | "disabled"
        )
    })
}

/// How a hosted tool's calls run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// The API runs the call and answers with its result.
    Api,
    /// The API names an endpoint for the call, which answers with its result.
    Stream,
}

/// One tool the API defines for the caller.
#[derive(Debug, Clone, PartialEq)]
pub struct HostedTool {
    pub name: String,
    pub title: Option<String>,
    pub description: String,
    pub input_schema: Value,
    pub annotations: Option<Value>,
    pub transport: Transport,
    /// Extra things a call to this tool carries, named by its definition.
    pub attachments: Vec<Attachment>,
}

/// Something a call carries beyond its arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attachment {
    /// A snapshot of the checkout (`mcp_client::checkout_snapshot`): the call
    /// carries who the repository is, its two trees and what was withheld.
    CheckoutSnapshot,
}

impl HostedTool {
    /// The tool's `tools/list` entry.
    pub fn list_entry(&self) -> Value {
        let mut entry = serde_json::json!({
            "name": self.name,
            "description": self.description,
            "inputSchema": self.input_schema,
        });
        if let Some(title) = &self.title {
            entry["title"] = Value::String(title.clone());
        }
        if let Some(annotations) = &self.annotations {
            entry["annotations"] = annotations.clone();
        }
        entry
    }
}

/// Whether `name` can name a hosted tool: a lowercase letter, then lowercase
/// letters, digits and `_`, 64 characters at most.
fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 64
        && matches!(chars.next(), Some('a'..='z'))
        && chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_'))
}

/// The tools in one catalog answer (`{"tools": [...]}`). A definition without a
/// valid name, a description and an object input schema, or with a transport
/// this client doesn't know, is left out, as is a repeated name.
pub fn parse_catalog(data: &Value) -> Vec<HostedTool> {
    let mut tools: Vec<HostedTool> = Vec::new();
    for item in data
        .get("tools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(name) = item
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| valid_name(name))
        else {
            continue;
        };
        let Some(description) = item
            .get("description")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|description| !description.is_empty())
        else {
            continue;
        };
        let Some(input_schema) = item
            .get("input_schema")
            .or_else(|| item.get("inputSchema"))
            .filter(|schema| schema.get("type").and_then(Value::as_str) == Some("object"))
            .cloned()
        else {
            continue;
        };
        let transport = match item
            .pointer("/transport/kind")
            .and_then(Value::as_str)
            .unwrap_or("api")
        {
            "api" => Transport::Api,
            "stream" => Transport::Stream,
            _ => continue,
        };
        if tools.iter().any(|tool| tool.name == name) {
            continue;
        }
        tools.push(HostedTool {
            name: name.to_string(),
            title: item
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_string),
            description: description.to_string(),
            input_schema,
            annotations: item.get("annotations").filter(|a| a.is_object()).cloned(),
            transport,
            attachments: item
                .get("attachments")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter_map(|name| match name {
                    "checkout_snapshot" => Some(Attachment::CheckoutSnapshot),
                    // One this client does not know is left out, not an error.
                    _ => None,
                })
                .collect(),
        });
        if tools.len() == MAX_TOOLS {
            break;
        }
    }
    tools
}

/// What one read of the catalog said.
#[derive(Debug, Clone)]
enum Read {
    /// The API answered: these tools (none for a caller it defines none for).
    Known(Arc<Vec<HostedTool>>),
    /// The API could not be reached.
    Unreachable,
}

impl Read {
    fn from_answer(answer: &HostedToolsAnswer) -> Self {
        match answer.status {
            200..=299 => Self::Known(Arc::new(parse_catalog(&answer.data))),
            401 | 403 | 404 => Self::Known(Arc::new(Vec::new())),
            _ => Self::Unreachable,
        }
    }

    fn tools(&self) -> Arc<Vec<HostedTool>> {
        match self {
            Self::Known(tools) => tools.clone(),
            Self::Unreachable => Arc::new(Vec::new()),
        }
    }
}

struct Entry {
    read: Read,
    read_at: Instant,
    refresh_at: Instant,
    /// What `tools/list` last showed this caller of its hosted tools.
    shown: Option<String>,
}

/// The process-wide cache of each caller's hosted tools.
#[derive(Default)]
pub struct HostedToolsGate {
    entries: Mutex<HashMap<String, Entry>>,
}

impl HostedToolsGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// The gate every registry shares.
    pub fn global() -> Arc<Self> {
        static GATE: OnceLock<Arc<HostedToolsGate>> = OnceLock::new();
        GATE.get_or_init(|| Arc::new(Self::new())).clone()
    }

    /// The caller's tools, when the read is still fresh.
    fn fresh(&self, identity: &str) -> Option<Arc<Vec<HostedTool>>> {
        let now = Instant::now();
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(identity)
            .filter(|entry| now < entry.refresh_at)
            .map(|entry| entry.read.tools())
    }

    /// Store one read. An unreachable API never replaces tools read before.
    fn record(&self, identity: &str, read: Read) -> Arc<Vec<HostedTool>> {
        let now = Instant::now();
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let shown = entries.get(identity).and_then(|entry| entry.shown.clone());
        let kept = match (&read, entries.get(identity)) {
            (Read::Unreachable, Some(previous)) if matches!(previous.read, Read::Known(_)) => {
                Entry {
                    read: previous.read.clone(),
                    read_at: previous.read_at,
                    refresh_at: now + UNREACHABLE_RETRY,
                    shown,
                }
            }
            (Read::Unreachable, _) => Entry {
                read: Read::Unreachable,
                read_at: now,
                refresh_at: now + UNREACHABLE_RETRY,
                shown,
            },
            _ => Entry {
                read: read.clone(),
                read_at: now,
                refresh_at: now + KNOWN_TTL,
                shown,
            },
        };
        let tools = kept.read.tools();
        entries.insert(identity.to_string(), kept);
        if entries.len() > MAX_ENTRIES {
            if let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.read_at)
                .map(|(key, _)| key.clone())
            {
                entries.remove(&oldest);
            }
        }
        tools
    }

    /// The caller's tools: the cached read when fresh, else the catalog read
    /// within `budget` (a read that doesn't finish counts as unreachable).
    async fn tools(
        &self,
        client: &ContextStreamClient,
        identity: &str,
        workspace: Option<&str>,
        budget: Duration,
    ) -> Arc<Vec<HostedTool>> {
        if off_by(std::env::var(ENV_SWITCH).ok().as_deref()) {
            return Arc::new(Vec::new());
        }
        if let Some(tools) = self.fresh(identity) {
            return tools;
        }
        let read = match tokio::time::timeout(
            budget,
            client.hosted_tools_catalog(workspace, budget),
        )
        .await
        {
            Ok(Ok(answer)) => Read::from_answer(&answer),
            _ => Read::Unreachable,
        };
        self.record(identity, read)
    }

    fn mark_shown(&self, identity: &str, shown: String) {
        if let Some(entry) = self
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(identity)
        {
            entry.shown = Some(shown);
        }
    }

    fn shown(&self, identity: &str) -> Option<String> {
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(identity)
            .and_then(|entry| entry.shown.clone())
    }
}

/// The `tools/list` entries for `tools`, leaving out names `taken` says another
/// tool already has.
fn entries_for(tools: &[HostedTool], taken: &impl Fn(&str) -> bool) -> Vec<Value> {
    tools
        .iter()
        .filter(|tool| !taken(&tool.name))
        .map(HostedTool::list_entry)
        .collect()
}

/// The `tools/list` entries for the caller's hosted tools, recorded as what this
/// caller was shown. `taken` names the tools the registry already lists.
pub async fn listing_entries(
    gate: &HostedToolsGate,
    client: &ContextStreamClient,
    session: &SessionManager,
    budget: Duration,
    taken: impl Fn(&str) -> bool,
) -> Vec<Value> {
    let Some(identity) = crate::domains::slipstream::caller_identity(client).await else {
        return Vec::new();
    };
    let workspace = workspace_of(client, session).await;
    let tools = gate
        .tools(client, &identity, workspace.as_deref(), budget)
        .await;
    let entries = entries_for(&tools, &taken);
    gate.mark_shown(&identity, Value::Array(entries.clone()).to_string());
    entries
}

/// Whether what `tools/list` would show of the caller's hosted tools differs
/// from what it last showed this caller. Never before a first listing.
pub async fn listing_changed(
    gate: &HostedToolsGate,
    client: &ContextStreamClient,
    session: &SessionManager,
    taken: impl Fn(&str) -> bool,
) -> bool {
    let Some(identity) = crate::domains::slipstream::caller_identity(client).await else {
        return false;
    };
    let Some(shown) = gate.shown(&identity) else {
        return false;
    };
    let workspace = workspace_of(client, session).await;
    let tools = gate
        .tools(client, &identity, workspace.as_deref(), CHANGE_CHECK_BUDGET)
        .await;
    let now = Value::Array(entries_for(&tools, &taken)).to_string();
    now != shown
}

/// The caller's hosted tool `name`, if the API defines one.
pub async fn find(
    gate: &HostedToolsGate,
    client: &ContextStreamClient,
    session: &SessionManager,
    name: &str,
) -> Option<HostedTool> {
    let identity = crate::domains::slipstream::caller_identity(client).await?;
    let workspace = workspace_of(client, session).await;
    gate.tools(client, &identity, workspace.as_deref(), LISTING_BUDGET)
        .await
        .iter()
        .find(|tool| tool.name == name)
        .cloned()
}

/// Read the caller's definitions in the background, so the `tools/list` after
/// an initialize finds them ready.
pub fn prefetch(
    gate: Arc<HostedToolsGate>,
    client: ContextStreamClient,
    session: Arc<SessionManager>,
) {
    mcp_client::spawn_with_task_context(async move {
        if let Some(identity) = crate::domains::slipstream::caller_identity(&client).await {
            let workspace = workspace_of(&client, &session).await;
            gate.tools(
                &client,
                &identity,
                workspace.as_deref(),
                Duration::from_secs(10),
            )
            .await;
        }
    });
}

/// The workspace the caller works in, as the API is told it.
async fn workspace_of(client: &ContextStreamClient, session: &SessionManager) -> Option<String> {
    listing_workspace(client, session)
        .await
        .map(|id| id.to_string())
}

/// The checkout a call is about: the session's folder, else this process's.
async fn checkout_folder(session: &SessionManager) -> Option<std::path::PathBuf> {
    session
        .state()
        .await
        .folder_path
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
}

/// Why a snapshot a tool asked for was not taken, in words the caller reads.
fn snapshot_refusal(tool: &str, error: &SnapshotError) -> ToolResult {
    ToolResult::error(format!(
        "{tool} was not run: it needs a snapshot of this checkout and none was taken ({error}). Do what it was for yourself."
    ))
}

/// Results of calls whose caller gave up on them (its own timeout came first),
/// kept for the same call asked again: keyed by tool, workspace, the checkout's
/// tree and the arguments, each handed out once.
static GIVEN_UP: OnceLock<Mutex<Vec<(String, Instant, ToolResult)>>> = OnceLock::new();
/// How long a finished result waits for its caller to come back.
const GIVEN_UP_TTL: Duration = Duration::from_secs(900);
/// Results kept at once.
const GIVEN_UP_MAX: usize = 8;

fn given_up() -> std::sync::MutexGuard<'static, Vec<(String, Instant, ToolResult)>> {
    GIVEN_UP
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn keep_given_up(key: String, result: ToolResult) {
    let mut kept = given_up();
    kept.retain(|(_, at, _)| at.elapsed() < GIVEN_UP_TTL);
    kept.retain(|(other, _, _)| *other != key);
    if kept.len() >= GIVEN_UP_MAX {
        kept.remove(0);
    }
    kept.push((key, Instant::now(), result));
}

fn take_given_up(key: &str) -> Option<ToolResult> {
    let mut kept = given_up();
    kept.retain(|(_, at, _)| at.elapsed() < GIVEN_UP_TTL);
    let position = kept.iter().position(|(other, _, _)| other == key)?;
    Some(kept.remove(position).2)
}

/// What a call on a stream came to.
enum Streamed {
    /// The service ran it and said how it went.
    Ran(ToolResult),
    /// It was not run there; the caller is told to do it itself.
    NotRun(ToolResult),
}

/// Run a call to `tool` with `arguments`. Every failure is a tool error the
/// caller reads, never a protocol error.
pub async fn run(
    client: &ContextStreamClient,
    session: &SessionManager,
    tool: &HostedTool,
    arguments: Value,
) -> ToolResult {
    let mut body = serde_json::json!({ "arguments": arguments });
    let workspace = workspace_of(client, session).await;
    if let Some(workspace) = &workspace {
        body["workspace_id"] = Value::String(workspace.clone());
    }
    // Held until the call is over: the snapshot owns the objects it will be asked for.
    let mut snapshot: Option<CheckoutSnapshot> = None;
    if tool.attachments.contains(&Attachment::CheckoutSnapshot) {
        let Some(folder) = checkout_folder(session).await else {
            return ToolResult::error(format!(
                "{} was not run: it needs a snapshot of this checkout and there is no folder",
                tool.name
            ));
        };
        match CheckoutSnapshot::capture(&folder).await {
            Ok(taken) => {
                body["checkout"] = taken.attachment();
                snapshot = Some(taken);
            }
            Err(error) => return snapshot_refusal(&tool.name, &error),
        }
    }
    // A call asked again after its caller gave up on it picks up what that run
    // found, when the checkout is the same: the run is not lost to a client's timeout.
    let pickup = match (&tool.transport, &snapshot) {
        (Transport::Stream, Some(taken)) => Some(format!(
            "{}\0{}\0{}\0{}",
            tool.name,
            workspace.as_deref().unwrap_or(""),
            taken.attachment()["target_tree"].as_str().unwrap_or(""),
            body["arguments"]
        )),
        _ => None,
    };
    if let Some(done) = pickup.as_deref().and_then(take_given_up) {
        return done;
    }
    let answer = match client
        .hosted_tool_call(&tool.name, &body, CALL_TIMEOUT)
        .await
    {
        Ok(answer) => answer,
        Err(error) => {
            return ToolResult::error(format!("{} could not be reached: {error}", tool.name))
        }
    };
    if !answer.is_success() {
        return ToolResult::error(
            answer
                .error_message()
                .unwrap_or_else(|| format!("{} failed (HTTP {})", tool.name, answer.status)),
        );
    }
    match tool.transport {
        Transport::Api => result_from(&tool.name, &answer.data, String::new()),
        Transport::Stream => match run_stream(&tool.name, &answer.data, snapshot.as_ref()).await {
            Streamed::Ran(result) => {
                let abandoned = peer::current().is_some_and(|peer| peer.was_cancelled());
                if let (true, Some(key)) = (abandoned, pickup) {
                    keep_given_up(key, result.clone());
                }
                result
            }
            Streamed::NotRun(result) => result,
        },
    }
}

/// Run the call the API answered with a stream for: open it, send the checkout,
/// report progress while it runs, and turn its result into the tool's.
async fn run_stream(name: &str, data: &Value, snapshot: Option<&CheckoutSnapshot>) -> Streamed {
    let stream = data.get("stream").filter(|stream| stream.is_object());
    let url = stream.and_then(|s| s.get("url")).and_then(Value::as_str);
    let token = stream.and_then(|s| s.get("token")).and_then(Value::as_str);
    let (Some(stream), Some(url), Some(token)) = (stream, url, token) else {
        return Streamed::NotRun(ToolResult::error(format!(
            "{name}: the API named no stream for this call"
        )));
    };
    let Some(request) = stream.get("request").filter(|request| request.is_object()) else {
        return Streamed::NotRun(ToolResult::error(format!(
            "{name}: the API described no run for this call"
        )));
    };
    let command = request
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let cwd = request
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if stream.get("protocol").and_then(Value::as_str) != Some(frames::PROTOCOL) {
        return Streamed::NotRun(not_run(
            name,
            command,
            cwd,
            "its stream is of a kind this client does not know",
        ));
    }
    let Some(snapshot) = snapshot else {
        return Streamed::NotRun(ToolResult::error(format!(
            "{name} was not run: it needs a snapshot of this checkout and none was taken"
        )));
    };
    let env = frames::forwarded_env(request, |variable| std::env::var(variable).ok());
    let hello = match frames::hello(request, snapshot.request_fields(), env, true) {
        Ok(hello) => hello,
        Err(message) => {
            return Streamed::NotRun(not_run(name, command, cwd, &message));
        }
    };
    let deadline = frames::deadline_of(request);

    let (events, mut inbox) = tokio::sync::mpsc::unbounded_channel();
    let exchange = {
        let hello = &hello;
        async move {
            let socket = frames::connect(url, token).await?;
            frames::exchange(socket, hello, snapshot, deadline, events).await
        }
    };
    // The service's heartbeats keep the caller's own clock from running out
    // (clients that reset their timeout on progress). What the service says in
    // them is not passed on.
    let peer = peer::current().filter(peer::CallPeer::reports_progress);
    let started = Instant::now();
    let pump = async {
        while let Some(event) = inbox.recv().await {
            if let (Event::Heartbeat, Some(peer)) = (event, &peer) {
                peer.progress(
                    started.elapsed().as_secs_f64().max(1.0),
                    None,
                    Some("still running"),
                )
                .await;
            }
        }
    };
    let (outcome, ()) = tokio::join!(exchange, pump);
    match outcome {
        Ok(frame) => interpret(name, command, cwd, &frame),
        Err(error) => Streamed::NotRun(not_run(name, command, cwd, reason_of(&error))),
    }
}

/// What the service's `result` frame comes to: the result it made, or that it
/// did not run the command.
fn interpret(name: &str, command: &str, cwd: &str, frame: &Value) -> Streamed {
    if frame.get("ran").and_then(Value::as_bool) != Some(true) {
        return Streamed::NotRun(not_run(
            name,
            command,
            cwd,
            "the service declined to run it",
        ));
    }
    if let Some(made) = frame.get("tool_result").filter(|made| made.is_object()) {
        return Streamed::Ran(result_from(name, made, String::new()));
    }
    // It ran but sent no formatted result: the command's own status is all there is.
    let exit = frame.get("exit").and_then(Value::as_i64).unwrap_or(0);
    Streamed::Ran(ToolResult {
        content: vec![ContentItem::text(format!(
            "{name}: the command exited with status {exit}."
        ))],
        structured_content: Some(serde_json::json!({"exit": exit, "ran": true})),
        is_error: exit != 0,
        meta: None,
    })
}

/// The caller is told that the call was not run remotely and what to do instead.
fn not_run(name: &str, command: &str, cwd: &str, reason: &str) -> ToolResult {
    let place = if cwd.is_empty() {
        String::new()
    } else {
        format!(" (from the folder {cwd})")
    };
    ToolResult {
        content: vec![ContentItem::text(format!(
            "{name} was not run remotely: {reason}. Run it yourself: {command}{place}"
        ))],
        structured_content: Some(serde_json::json!({"ran": false})),
        is_error: true,
        meta: None,
    }
}

fn reason_of(error: &ExchangeError) -> &'static str {
    match error {
        ExchangeError::Silent(_) => "the service did not answer in time",
        ExchangeError::Ended => "the connection ended before a result",
        ExchangeError::Deadline => "it ran past its limits",
        ExchangeError::Wire(_) => "the service could not be reached",
        ExchangeError::Protocol(_) => {
            "the service answered in a way this client does not understand"
        }
        ExchangeError::Snapshot(_) => "the checkout could not be offered to it",
    }
}

/// A call's result from its answer (`{content, isError, structuredContent}`),
/// after `output` when there is some.
fn result_from(name: &str, data: &Value, output: String) -> ToolResult {
    let mut content: Vec<ContentItem> = match data.get("content") {
        None | Some(Value::Null) => Vec::new(),
        Some(items) => match serde_json::from_value(items.clone()) {
            Ok(items) => items,
            Err(_) => {
                return ToolResult::error(format!(
                    "{name} answered with content this client can't read"
                ))
            }
        },
    };
    if !output.is_empty() {
        content.insert(0, ContentItem::text(output));
    }
    let structured_content = data
        .get("structuredContent")
        .filter(|value| value.is_object())
        .cloned();
    if content.is_empty() && structured_content.is_none() {
        return ToolResult::error(format!("{name} answered with no result"));
    }
    let is_error = data
        .get("isError")
        .or_else(|| data.get("is_error"))
        .and_then(Value::as_bool)
        .unwrap_or(false);
    ToolResult {
        content,
        structured_content,
        is_error,
        meta: None,
    }
}

#[cfg(test)]
#[path = "hosted_tools_tests.rs"]
mod tests;
