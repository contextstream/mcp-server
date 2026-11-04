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
//! - `stream`: the API answers that call with an endpoint and a token for it,
//!   and the result comes from the endpoint as newline-delimited JSON frames
//!   (`{"type":"output","text":…}` …, then `{"type":"result",…}`).
//!
//! A hosted tool never shadows a tool the registry has.

use crate::domains::slipstream::listing_workspace;
use mcp_client::checkout_snapshot::{CheckoutSnapshot, SnapshotError};
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

/// Run a call to `tool` with `arguments`. Every failure is a tool error the
/// caller reads, never a protocol error.
pub async fn run(
    client: &ContextStreamClient,
    session: &SessionManager,
    tool: &HostedTool,
    arguments: Value,
) -> ToolResult {
    let mut body = serde_json::json!({ "arguments": arguments });
    if let Some(workspace) = workspace_of(client, session).await {
        body["workspace_id"] = Value::String(workspace);
    }
    // Held until the call is over: the snapshot owns the objects it will be asked for.
    let mut _snapshot: Option<CheckoutSnapshot> = None;
    if tool.attachments.contains(&Attachment::CheckoutSnapshot) {
        let Some(folder) = checkout_folder(session).await else {
            return ToolResult::error(format!(
                "{} was not run: it needs a snapshot of this checkout and there is no folder",
                tool.name
            ));
        };
        match CheckoutSnapshot::capture(&folder).await {
            Ok(snapshot) => {
                body["checkout"] = snapshot.attachment();
                _snapshot = Some(snapshot);
            }
            Err(error) => return snapshot_refusal(&tool.name, &error),
        }
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
        Transport::Stream => {
            let url = answer.data.pointer("/stream/url").and_then(Value::as_str);
            let token = answer.data.pointer("/stream/token").and_then(Value::as_str);
            let (Some(url), Some(token)) = (url, token) else {
                return ToolResult::error(format!(
                    "{}: the API named no stream for this call",
                    tool.name
                ));
            };
            match client
                .hosted_tool_stream(url, token, &arguments, CALL_TIMEOUT)
                .await
            {
                Ok((status, frames)) if (200..300).contains(&status) => {
                    result_from_frames(&tool.name, &frames)
                }
                Ok((status, _)) => {
                    ToolResult::error(format!("{}: the stream answered HTTP {status}", tool.name))
                }
                Err(error) => ToolResult::error(format!(
                    "{}: the stream could not be read: {error}",
                    tool.name
                )),
            }
        }
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

/// A call's result from a stream's frames: its `output` text, then the
/// `result` frame's content.
fn result_from_frames(name: &str, frames: &str) -> ToolResult {
    let mut output = String::new();
    for line in frames
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        let Ok(frame) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match frame.get("type").and_then(Value::as_str) {
            Some("output") => {
                if let Some(text) = frame.get("text").and_then(Value::as_str) {
                    output.push_str(text);
                }
            }
            Some("result") => return result_from(name, &frame, output),
            _ => {}
        }
    }
    if output.is_empty() {
        ToolResult::error(format!("{name}: the stream ended without a result"))
    } else {
        ToolResult::error(format!(
            "{name}: the stream ended without a result. Its output:\n{output}"
        ))
    }
}

#[cfg(test)]
#[path = "hosted_tools_tests.rs"]
mod tests;
