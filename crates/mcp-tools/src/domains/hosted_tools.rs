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
use mcp_client::command_hints::{self, CachedTool, Hints};
use mcp_client::environment;
use mcp_client::frames::{self, Delivery, Event, ExchangeError};
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
pub const ENV_SWITCH: &str = command_hints::ENV_SWITCH;

/// Whether the value of [`ENV_SWITCH`] turns hosted tools off.
fn off_by(value: Option<&str>) -> bool {
    command_hints::switched_off(value)
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
    /// The shell commands the tool says it takes, when it says.
    pub command_hints: Option<Hints>,
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
            command_hints: Hints::from_definition(item),
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
        if let Read::Known(tools) = &read {
            remember_hints(client, identity, tools).await;
        }
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

/// Keep the command hints of the caller's tools where the editor hook can read
/// them (`mcp_client::command_hints`): written when they changed, and also when
/// there are none any more, so that a tool taken away stops being used. A local
/// caller only; the hosted gateway has no home folder to keep them in.
async fn remember_hints(client: &ContextStreamClient, identity: &str, tools: &[HostedTool]) {
    let config = client.config().await;
    if config.is_http_transport || !identity.starts_with("local:") {
        return;
    }
    let cached: Vec<CachedTool> = tools
        .iter()
        .filter_map(|tool| {
            Some(CachedTool {
                name: tool.name.clone(),
                transport: match tool.transport {
                    Transport::Api => "api",
                    Transport::Stream => "stream",
                }
                .to_string(),
                attachments: tool
                    .attachments
                    .iter()
                    .map(|attachment| match attachment {
                        Attachment::CheckoutSnapshot => "checkout_snapshot".to_string(),
                    })
                    .collect(),
                hints: tool.command_hints.clone()?,
            })
        })
        .collect();
    let cache = command_hints::Cache::new(identity, &config.api_url, cached);
    // Best effort, and off the executor: a file that cannot be written only means no rewrite.
    let _ = tokio::task::spawn_blocking(move || command_hints::write(&cache)).await;
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

/// Whether a result is kept for this tool, workspace and arguments, whatever the checkout was.
fn has_given_up(prefix: &str) -> bool {
    let wanted = format!("{prefix}\0");
    let mut kept = given_up();
    kept.retain(|(_, at, _)| at.elapsed() < GIVEN_UP_TTL);
    kept.iter().any(|(key, _, _)| key.starts_with(&wanted))
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

/// A snapshot a tool asked for, begun: whose checkout it is is known, and the
/// rest is being taken while the caller goes on. Dropping it stops that.
pub struct PendingSnapshot {
    /// What a call that carries no more of the snapshot sends: the repository's identity.
    pub identity: Value,
    /// The folder the call is about, relative to the repository's root (`""` for the root).
    pub folder: String,
    task: Option<tokio::task::JoinHandle<Result<CheckoutSnapshot, SnapshotError>>>,
}

impl Drop for PendingSnapshot {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl PendingSnapshot {
    /// The finished snapshot, or the result that tells the caller why there is none.
    pub async fn finish(mut self, tool: &str) -> Result<CheckoutSnapshot, ToolResult> {
        let Some(task) = self.task.take() else {
            return Err(ToolResult::error(format!(
                "{tool} was not run: the snapshot of this checkout was not taken"
            )));
        };
        match task.await {
            Ok(Ok(snapshot)) => Ok(snapshot),
            Ok(Err(error)) => Err(snapshot_refusal(tool, &error)),
            Err(_) => Err(ToolResult::error(format!(
                "{tool} was not run: the snapshot of this checkout could not be taken"
            ))),
        }
    }
}

/// Begin the snapshot a tool asks for (`None` for a tool that asks for none),
/// or give the result that tells the caller it cannot be taken. Refusals that
/// do not need the whole checkout (the person's switches, no repository) come
/// at once, before anything is sent anywhere.
pub async fn begin_snapshot(
    session: &SessionManager,
    tool: &HostedTool,
) -> Result<Option<PendingSnapshot>, ToolResult> {
    if !tool.attachments.contains(&Attachment::CheckoutSnapshot) {
        return Ok(None);
    }
    let Some(folder) = checkout_folder(session).await else {
        return Err(ToolResult::error(format!(
            "{} was not run: it needs a snapshot of this checkout and there is no folder",
            tool.name
        )));
    };
    let start = match CheckoutSnapshot::begin(&folder).await {
        Ok(start) => start,
        Err(error) => return Err(snapshot_refusal(&tool.name, &error)),
    };
    let identity = start.identity();
    let folder = start.folder().to_string();
    let task = tokio::spawn(start.complete());
    Ok(Some(PendingSnapshot {
        identity,
        folder,
        task: Some(task),
    }))
}

/// Make the call to the API: its answer's data, or the result that says why
/// there is none.
pub async fn call_api(
    client: &ContextStreamClient,
    session: &SessionManager,
    tool: &HostedTool,
    arguments: &Value,
    checkout: Option<Value>,
) -> Result<Value, ToolResult> {
    let mut body = serde_json::json!({ "arguments": arguments });
    if let Some(workspace) = workspace_of(client, session).await {
        body["workspace_id"] = Value::String(workspace);
    }
    if let Some(checkout) = checkout {
        body["checkout"] = checkout;
    }
    let answer = match client
        .hosted_tool_call(&tool.name, &body, CALL_TIMEOUT)
        .await
    {
        Ok(answer) => answer,
        Err(error) => {
            return Err(ToolResult::error(format!(
                "{} could not be reached: {error}",
                tool.name
            )))
        }
    };
    if !answer.is_success() {
        return Err(ToolResult::error(answer.error_message().unwrap_or_else(
            || format!("{} failed (HTTP {})", tool.name, answer.status),
        )));
    }
    Ok(answer.data)
}

/// Run a call to `tool` with `arguments`. Every failure is a tool error the
/// caller reads, never a protocol error.
pub async fn run(
    client: &ContextStreamClient,
    session: &SessionManager,
    tool: &HostedTool,
    arguments: Value,
) -> ToolResult {
    let streamed = tool.transport == Transport::Stream;
    let workspace = workspace_of(client, session).await.unwrap_or_default();
    let kept = format!("{}\0{}\0{}", tool.name, workspace, arguments);
    // Held until the call is over: the snapshot owns the objects it will be asked for.
    let mut pending = match begin_snapshot(session, tool).await {
        Ok(pending) => pending,
        Err(refusal) => return refusal,
    };
    let mut snapshot: Option<CheckoutSnapshot> = None;
    let mut checkout: Option<Value> = None;
    // A call to a tool that answers on a stream carries only who the repository
    // is, so it is made while the rest of the snapshot is taken. A call asked
    // again after its caller gave up on it picks up what that run found, when
    // the checkout is the same, so it waits for the snapshot to look for it.
    let overlap = streamed && !has_given_up(&kept);
    if let Some(begun) = pending.take() {
        if overlap {
            checkout = Some(begun.identity.clone());
            pending = Some(begun);
        } else {
            let taken = match begun.finish(&tool.name).await {
                Ok(taken) => taken,
                Err(refusal) => return refusal,
            };
            checkout = Some(if streamed {
                taken.identity()
            } else {
                taken.attachment()
            });
            snapshot = Some(taken);
        }
    }
    if let (true, Some(taken)) = (streamed, &snapshot) {
        if let Some(done) = take_given_up(&format!("{kept}\0{}", taken.target_tree)) {
            return done;
        }
    }
    let data = match call_api(client, session, tool, &arguments, checkout).await {
        Ok(data) => data,
        Err(failure) => return failure,
    };
    if let Some(begun) = pending.take() {
        snapshot = match begun.finish(&tool.name).await {
            Ok(taken) => Some(taken),
            Err(refusal) => return refusal,
        };
    }
    match tool.transport {
        Transport::Api => result_from(&tool.name, &data, String::new()),
        Transport::Stream => match run_stream(&tool.name, &data, snapshot.as_ref()).await {
            Streamed::Ran(result) => {
                let abandoned = peer::current().is_some_and(|peer| peer.was_cancelled());
                if let (true, Some(taken)) = (abandoned, &snapshot) {
                    keep_given_up(format!("{kept}\0{}", taken.target_tree), result.clone());
                }
                result
            }
            Streamed::NotRun(result) => result,
        },
    }
}

/// The stream an API answer names for a call.
#[derive(Debug)]
pub struct StreamCall<'a> {
    pub url: &'a str,
    pub token: &'a str,
    pub protocol: Option<&'a str>,
    /// The description of the run the API made.
    pub request: &'a Value,
    /// The command it is for, as the API has it.
    pub command: &'a str,
    /// The folder it is run from, inside the repository.
    pub cwd: &'a str,
}

/// Read the stream out of an API answer, or say how the answer is wrong.
pub fn stream_call(data: &Value) -> Result<StreamCall<'_>, String> {
    let stream = data.get("stream").filter(|stream| stream.is_object());
    let url = stream.and_then(|s| s.get("url")).and_then(Value::as_str);
    let token = stream.and_then(|s| s.get("token")).and_then(Value::as_str);
    let (Some(stream), Some(url), Some(token)) = (stream, url, token) else {
        return Err("the API named no stream for this call".to_string());
    };
    let Some(request) = stream.get("request").filter(|request| request.is_object()) else {
        return Err("the API described no run for this call".to_string());
    };
    Ok(StreamCall {
        url,
        token,
        protocol: stream.get("protocol").and_then(Value::as_str),
        request,
        command: request
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        cwd: request
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or_default(),
    })
}

/// Run the call on its stream: open it, send the checkout, answer what the
/// service asks of it, and return the service's `result` frame. What the
/// service sends meanwhile goes to `events`. `tool_result` asks the service to
/// make the tool's result itself (it then sends no output as it goes) and
/// `buffered` that the caller takes the output at the end (see [`Delivery`]).
/// The service is also told about the local environment while the stream is
/// opened ([`environment::describe`]). The error is why the call was not run
/// there, in words for the caller.
pub async fn drive_stream(
    call: &StreamCall<'_>,
    snapshot: &CheckoutSnapshot,
    delivery: Delivery,
    events: tokio::sync::mpsc::UnboundedSender<Event>,
) -> Result<Value, String> {
    if call.protocol != Some(frames::PROTOCOL) {
        return Err("its stream is of a kind this client does not know".to_string());
    }
    let env = frames::forwarded_env(call.request, |variable| std::env::var(variable).ok());
    let hello = frames::hello(call.request, snapshot.request_fields(), env, delivery)?;
    let deadline = frames::deadline_of(call.request);
    let outcome = async {
        let (socket, pack, environment) = tokio::join!(
            frames::connect(call.url, call.token),
            snapshot.initial_pack(),
            environment::describe(call.request, &snapshot.root)
        );
        let socket = socket?;
        let pack = pack.map_err(|e| ExchangeError::Snapshot(e.to_string()))?;
        let hello = frames::with_environment(hello, environment);
        frames::exchange_with_pack(socket, &hello, pack, snapshot, deadline, events).await
    }
    .await;
    outcome.map_err(|error| reason_of(&error).to_string())
}

/// Run the call the API answered with a stream for, and turn its result into the tool's;
/// the service's heartbeats are reported as progress while it runs.
async fn run_stream(name: &str, data: &Value, snapshot: Option<&CheckoutSnapshot>) -> Streamed {
    let call = match stream_call(data) {
        Ok(call) => call,
        Err(message) => {
            return Streamed::NotRun(ToolResult::error(format!("{name}: {message}")));
        }
    };
    let Some(snapshot) = snapshot else {
        return Streamed::NotRun(ToolResult::error(format!(
            "{name} was not run: it needs a snapshot of this checkout and none was taken"
        )));
    };
    let (events, mut inbox) = tokio::sync::mpsc::unbounded_channel();
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
    let (outcome, ()) = tokio::join!(
        drive_stream(&call, snapshot, Delivery::ToolResult, events),
        pump
    );
    match outcome {
        Ok(frame) => interpret(name, call.command, call.cwd, &frame),
        Err(reason) => Streamed::NotRun(not_run(name, call.command, call.cwd, &reason)),
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
