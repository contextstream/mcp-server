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
use mcp_client::command_hints::{self, CachedTool, CheckMarker, Hints};
use mcp_client::environment;
use mcp_client::frames::{self, Delivery, Event, ExchangeError};
use mcp_client::hosted_tools::{HostedToolsAnswer, KNOWN_TTL, UNREACHABLE_RETRY};
use mcp_client::ContextStreamClient;
use mcp_session::SessionManager;
use mcp_types::tool::{ContentItem, ToolResult};
use serde_json::Value;
use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

/// How long a listing waits for the caller's definitions.
pub const LISTING_BUDGET: Duration = Duration::from_millis(2500);
/// How long the check after a tool call waits for them.
pub const CHANGE_CHECK_BUDGET: Duration = Duration::from_millis(500);
/// Callers remembered at once; beyond it the oldest reads go first.
const MAX_ENTRIES: usize = 4096;
/// Hosted tools listed for one caller at most.
pub const MAX_TOOLS: usize = 16;
/// How long one hosted call may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(300);
/// Bounded (<100 ms) HTTP timeout for fail-open context-enriched failure diagnostics.
const DIAGNOSE_TIMEOUT: Duration = Duration::from_millis(95);
/// Server-side lookup budget requested from `/calls/{call_id}/diagnose`.
const DIAGNOSE_BUDGET_MS: u64 = 65;
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
    async fn fetch(
        client: &ContextStreamClient,
        workspace: Option<&str>,
        budget: Duration,
    ) -> Self {
        match client.hosted_tools_catalog(workspace, budget).await {
            Ok(answer) => Self::from_answer(&answer),
            Err(_) => Self::Unreachable,
        }
    }

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
    settings_changed: Option<SystemTime>,
}

/// The process-wide cache of each caller's hosted tools.
#[derive(Default)]
pub struct HostedToolsGate {
    entries: Mutex<HashMap<String, Entry>>,
    // Serialize catalog reads with settings refreshes for the same caller. A
    // listing started before a PUT must never overwrite its new command hints.
    readers: [tokio::sync::Mutex<()>; 32],
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
    fn fresh(&self, identity: &str, changed: Option<SystemTime>) -> Option<Arc<Vec<HostedTool>>> {
        let now = Instant::now();
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(identity)
            .filter(|entry| now < entry.refresh_at && changed == entry.settings_changed)
            .map(|entry| entry.read.tools())
    }

    /// Store one read. An unreachable API never replaces tools read before.
    fn record(
        &self,
        identity: &str,
        read: Read,
        changed: Option<SystemTime>,
    ) -> Arc<Vec<HostedTool>> {
        let now = Instant::now();
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        let shown = entries.get(identity).and_then(|entry| entry.shown.clone());
        let kept = match (&read, entries.get(identity)) {
            (Read::Unreachable, Some(previous)) if matches!(previous.read, Read::Known(_)) => {
                Entry {
                    read: previous.read.clone(),
                    read_at: previous.read_at,
                    refresh_at: now + UNREACHABLE_RETRY,
                    settings_changed: changed,
                    shown,
                }
            }
            (Read::Unreachable, _) => Entry {
                read: Read::Unreachable,
                read_at: now,
                refresh_at: now + UNREACHABLE_RETRY,
                settings_changed: changed,
                shown,
            },
            _ => Entry {
                read: read.clone(),
                read_at: now,
                refresh_at: now + KNOWN_TTL,
                settings_changed: changed,
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
        self.read_catalog(client, identity, workspace, budget, false)
            .await
    }

    async fn read_catalog(
        &self,
        client: &ContextStreamClient,
        identity: &str,
        workspace: Option<&str>,
        budget: Duration,
        force: bool,
    ) -> Arc<Vec<HostedTool>> {
        let config = client.config().await;
        let local = !config.is_http_transport && identity.starts_with("local:");
        let changed = || {
            local
                .then(|| CheckMarker::SettingsChanged.modified(identity, &config.api_url))
                .flatten()
        };
        if !force {
            if off_by(std::env::var(ENV_SWITCH).ok().as_deref()) {
                return Arc::new(Vec::new());
            }
            if let Some(tools) = self.fresh(identity, changed()) {
                return tools;
            }
        }
        let mut hash = DefaultHasher::new();
        identity.hash(&mut hash);
        let reader = &self.readers[hash.finish() as usize % self.readers.len()];
        let deadline = tokio::time::Instant::now() + budget;
        // A prefetch can have a longer deadline than tools/list. Waiting for
        // its read must not spend the caller's budget a second time.
        let _reading = if force {
            reader.lock().await
        } else {
            match tokio::time::timeout_at(deadline, reader.lock()).await {
                Ok(guard) => guard,
                Err(_) => {
                    return self
                        .entries
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .get(identity)
                        .filter(|entry| entry.settings_changed == changed())
                        .map(|entry| entry.read.tools())
                        .unwrap_or_default()
                }
            }
        };
        if force && local {
            CheckMarker::SettingsChanged.mark(identity, &config.api_url);
        }
        let changed = changed();
        if !force {
            if let Some(tools) = self.fresh(identity, changed) {
                return tools;
            }
        }
        // After a settings change, a failed refresh must not keep offering a
        // disabled check. Preserve 'shown' for notifications/tools/list_changed.
        if let Some(entry) = self
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(identity)
        {
            if force || changed != entry.settings_changed {
                if let Read::Known(tools) = &mut entry.read {
                    Arc::make_mut(tools).retain(|tool| tool.name != "check");
                }
                entry.refresh_at = Instant::now();
            }
        }
        if off_by(std::env::var(ENV_SWITCH).ok().as_deref()) {
            return self.record(identity, Read::Known(Arc::new(Vec::new())), changed);
        }
        let remaining = if force {
            budget
        } else {
            deadline.saturating_duration_since(tokio::time::Instant::now())
        };
        let read = tokio::time::timeout(remaining, Read::fetch(client, workspace, remaining))
            .await
            .unwrap_or(Read::Unreachable);
        if let (true, Read::Known(tools)) = (local, &read) {
            remember_hints(identity, &config.api_url, tools, changed).await;
        }
        self.record(identity, read, changed)
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

/// Read the local caller's command hints, refreshing them if they are missing
/// or stale. A failed refresh skips remote execution and is shared with the
/// next invocation so it doesn't wait on the same unreachable API.
pub async fn command_hints_for_run(
    client: &ContextStreamClient,
    session: &SessionManager,
    identity: &str,
    budget: Duration,
) -> Option<command_hints::Cache> {
    let config = client.config().await;
    let api_url = &config.api_url;
    let settings_changed = || CheckMarker::SettingsChanged.modified(identity, api_url);
    let changed = settings_changed();
    if let Some(cache) = command_hints::read(identity, api_url)
        .filter(|cache| cache.settings_changed == changed && cache.is_fresh(KNOWN_TTL))
    {
        return Some(cache);
    }
    if CheckMarker::CatalogUnreachable
        .modified(identity, api_url)
        .filter(|failed| changed.is_none_or(|changed| *failed >= changed))
        .and_then(|failed| failed.elapsed().ok())
        .is_some_and(|age| age < UNREACHABLE_RETRY)
    {
        return None;
    }

    let read = tokio::time::timeout(budget, async {
        let workspace = workspace_of(client, session).await;
        Read::fetch(client, workspace.as_deref(), budget).await
    })
    .await
    .unwrap_or(Read::Unreachable);
    match read {
        Read::Known(tools) => remember_hints(identity, api_url, &tools, changed)
            .await
            .filter(|cache| cache.settings_changed == settings_changed()),
        Read::Unreachable => {
            if changed == settings_changed() {
                CheckMarker::CatalogUnreachable.mark(identity, api_url);
            }
            None
        }
    }
}

/// Keep a local caller's command hints where the editor hook can read them,
/// including an empty catalog when a tool was taken away.
async fn remember_hints(
    identity: &str,
    api_url: &str,
    tools: &[HostedTool],
    changed: Option<SystemTime>,
) -> Option<command_hints::Cache> {
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
    let mut cache = command_hints::Cache::new(identity, api_url, cached);
    cache.settings_changed = changed;
    // Best effort, and off the executor. The current run can still use the hints.
    tokio::task::spawn_blocking(move || {
        let _ = command_hints::write(&cache);
        cache
    })
    .await
    .ok()
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

/// Refresh both the advertised tools and the editor hook's disk cache after
/// an account change, even before this caller's first tools/list.
pub(super) async fn refresh_settings(client: &ContextStreamClient, workspace: Option<&str>) {
    if let Some(identity) = crate::domains::slipstream::caller_identity(client).await {
        HostedToolsGate::global()
            .read_catalog(client, &identity, workspace, LISTING_BUDGET, true)
            .await;
    }
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

/// The project the caller works in, when known to the session, request auth, or local config.
async fn project_of(client: &ContextStreamClient, session: &SessionManager) -> Option<String> {
    let state = session.state().await;
    if let Some(project_id) = state.project_id {
        return Some(project_id.to_string());
    }
    if let Some(project_id) = mcp_client::get_task_auth_override().and_then(|a| a.project_id) {
        return Some(project_id.to_string());
    }
    let config = client.config().await;
    if config.is_http_transport {
        return None;
    }
    config.default_project_id.map(|id| id.to_string())
}

/// Extract the hosted call id from a `POST /hosted-tools/{name}/call` response payload.
pub fn call_id_of(data: &Value) -> Option<&str> {
    ["/call/id", "/stream/request/id"]
        .into_iter()
        .find_map(|path| data.pointer(path).and_then(Value::as_str))
        .map(str::trim)
        .filter(|id| !id.is_empty())
}

/// Extract the hosted call `repo_key` from a `POST /hosted-tools/{name}/call` response payload.
pub fn repo_key_of(data: &Value) -> Option<&str> {
    data.pointer("/stream/request/repo_key")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|key| !key.is_empty())
}

fn frame_field<'a>(frame: &'a Value, key: &str) -> Option<&'a Value> {
    frame.get(key).filter(|value| !value.is_null()).or_else(|| {
        frame
            .pointer(&format!("/tool_result/structuredContent/{key}"))
            .filter(|value| !value.is_null())
    })
}

fn frame_needs_diagnose(frame: &Value) -> bool {
    if frame.get("ran").and_then(Value::as_bool) != Some(true) {
        return false;
    }
    if frame
        .get("environment")
        .filter(|value| value.is_object())
        .is_some()
    {
        return false;
    }
    let non_zero_exit = frame
        .get("exit")
        .or_else(|| frame.pointer("/tool_result/structuredContent/exit"))
        .and_then(Value::as_i64)
        .is_some_and(|code| code != 0)
        || frame
            .pointer("/tool_result/isError")
            .and_then(Value::as_bool)
            == Some(true);
    if !non_zero_exit {
        return false;
    }
    if frame_field(frame, "diagnostics").is_some_and(Value::is_object) {
        return false;
    }
    let has_failures = frame_field(frame, "failures")
        .and_then(Value::as_array)
        .is_some_and(|arr| !arr.is_empty());
    let has_new_fob = frame_field(frame, "failing_on_base")
        .and_then(|value| value.get("new"))
        .and_then(Value::as_array)
        .is_some_and(|arr| !arr.is_empty());
    has_failures || has_new_fob
}

/// Enrich a failing streamed result frame in place from
/// `POST /api/v1/mcp/hosted-tools/calls/{call_id}/diagnose` within a strict
/// `<100 ms` fail-open budget.
pub async fn enrich_frame_diagnostics(
    client: &ContextStreamClient,
    call_id: Option<&str>,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    repo_key: Option<&str>,
    frame: &mut Value,
) {
    if !frame_needs_diagnose(frame) {
        return;
    }
    let Some(call_id) = call_id.map(str::trim).filter(|id| !id.is_empty()) else {
        return;
    };
    let mut body = serde_json::Map::new();
    body.insert("budget_ms".to_string(), Value::from(DIAGNOSE_BUDGET_MS));
    if let Some(ws) = workspace_id.map(str::trim).filter(|s| !s.is_empty()) {
        body.insert("workspace_id".to_string(), Value::String(ws.to_string()));
    }
    if let Some(pid) = project_id.map(str::trim).filter(|s| !s.is_empty()) {
        body.insert("project_id".to_string(), Value::String(pid.to_string()));
    }
    if let Some(rk) = repo_key.map(str::trim).filter(|s| !s.is_empty()) {
        body.insert("repo_key".to_string(), Value::String(rk.to_string()));
    }
    if let Some(failures) = frame_field(frame, "failures").cloned() {
        body.insert("failures".to_string(), failures);
    }
    if let Some(fob) = frame_field(frame, "failing_on_base").cloned() {
        body.insert("failing_on_base".to_string(), fob);
    }
    let req_body = Value::Object(body);
    let Ok(Ok(answer)) = tokio::time::timeout(
        DIAGNOSE_TIMEOUT,
        client.hosted_tool_diagnose(call_id, &req_body, DIAGNOSE_TIMEOUT),
    )
    .await
    else {
        return;
    };
    if !answer.is_success() || !answer.data.is_object() {
        return;
    }
    let diag = answer.data;
    if let Some(obj) = frame.as_object_mut() {
        obj.insert("diagnostics".to_string(), diag.clone());
    }
    if let Some(sc) = frame
        .pointer_mut("/tool_result/structuredContent")
        .and_then(Value::as_object_mut)
    {
        sc.entry("diagnostics".to_string()).or_insert(diag);
    }
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

const CREDIT_RETRY: Duration = Duration::from_secs(180);
pub const CREDIT_MESSAGE: &str = "Your account is out of Context Credits for remote checks.";
pub const ADD_CREDITS: &str = "Add credits at https://contextstream.io/account/billing";

#[derive(Debug)]
pub enum CallRefusal {
    OutOfCredits,
    Other(ToolResult),
}

impl CallRefusal {
    fn into_result(self) -> ToolResult {
        match self {
            Self::OutOfCredits => ToolResult::error(format!(
                "{CREDIT_MESSAGE} Run the command locally. {ADD_CREDITS}"
            )),
            Self::Other(result) => result,
        }
    }
}

/// Local calls (including one-shot hook processes) share a timestamp on disk.
/// The HTTP gateway keeps only scoped, expiring entries in its own memory.
async fn credit_cooldown(client: &ContextStreamClient, remember: bool) -> bool {
    let Some(identity) = crate::domains::slipstream::caller_identity(client).await else {
        return false;
    };
    let config = client.config().await;
    if !config.is_http_transport {
        if remember {
            CheckMarker::OutOfCredits.mark(&identity, &config.api_url);
        }
        return CheckMarker::OutOfCredits
            .modified(&identity, &config.api_url)
            .and_then(|time| time.elapsed().ok())
            .is_some_and(|age| age < CREDIT_RETRY);
    }
    static REFUSED: OnceLock<Mutex<HashMap<(String, String), Instant>>> = OnceLock::new();
    let mut refused = REFUSED
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let key = (identity, config.api_url);
    if remember {
        refused.retain(|_, at| at.elapsed() < CREDIT_RETRY);
        if refused.len() < MAX_ENTRIES {
            refused.insert(key, Instant::now());
        }
        return true;
    }
    refused
        .get(&key)
        .is_some_and(|at| at.elapsed() < CREDIT_RETRY)
}

/// Make the call to the API: its answer's data, or the result that says why
/// there is none.
pub async fn call_api(
    client: &ContextStreamClient,
    session: &SessionManager,
    tool: &HostedTool,
    arguments: &Value,
    checkout: Option<Value>,
) -> Result<Value, CallRefusal> {
    if tool.name == "check" && credit_cooldown(client, false).await {
        return Err(CallRefusal::OutOfCredits);
    }
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
            return Err(CallRefusal::Other(ToolResult::error(format!(
                "{} could not be reached: {error}",
                tool.name
            ))))
        }
    };
    if tool.name == "check" && answer.insufficient_credits() {
        credit_cooldown(client, true).await;
        return Err(CallRefusal::OutOfCredits);
    }
    if !answer.is_success() {
        return Err(CallRefusal::Other(ToolResult::error(
            answer
                .error_message()
                .unwrap_or_else(|| format!("{} failed (HTTP {})", tool.name, answer.status)),
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
        Err(failure) => return failure.into_result(),
    };
    if let Some(begun) = pending.take() {
        snapshot = match begun.finish(&tool.name).await {
            Ok(taken) => Some(taken),
            Err(refusal) => return refusal,
        };
    }
    let workspace_id = workspace_of(client, session).await;
    let project_id = project_of(client, session).await;
    match tool.transport {
        Transport::Api => result_from(&tool.name, &data, String::new()),
        Transport::Stream => match run_stream(
            &tool.name,
            &data,
            snapshot.as_ref(),
            client,
            workspace_id.as_deref(),
            project_id.as_deref(),
        )
        .await
        {
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
    // A variable that changes the command and never leaves this machine: the
    // stream is not opened, and the caller runs the command here.
    if let Some(name) = frames::set_here(call.request, |variable| {
        std::env::var_os(variable).is_some()
    }) {
        return Err(format!(
            "this session sets {name}, which changes what the command does and is never sent"
        ));
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
async fn run_stream(
    name: &str,
    data: &Value,
    snapshot: Option<&CheckoutSnapshot>,
    client: &ContextStreamClient,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
) -> Streamed {
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
    // (clients that reset their timeout on progress). Plain heartbeats pass on
    // nothing the service said; structured stage progress updates forward the
    // step, total, and sanitized stage message.
    let peer = peer::current().filter(peer::CallPeer::reports_progress);
    let started = Instant::now();
    let pump = async {
        let mut active_stage: Option<(u64, u64)> = None;
        while let Some(event) = inbox.recv().await {
            let Some(peer) = &peer else {
                continue;
            };
            match event {
                Event::Heartbeat => {
                    let (progress, total) = match active_stage {
                        Some((step, total)) => (step as f64, Some(total as f64)),
                        None => (started.elapsed().as_secs_f64().clamp(0.1, 0.9), None),
                    };
                    peer.progress(progress, total, Some("still running")).await;
                }
                Event::StageProgress {
                    stage,
                    step,
                    total,
                    message,
                } => {
                    active_stage = Some((step, total));
                    let summary = message.unwrap_or_else(|| format!("[{step}/{total}] {stage}"));
                    peer.progress(step as f64, Some(total as f64), Some(&summary))
                        .await;
                }
                Event::Output(_) => {}
            }
        }
    };
    let (outcome, ()) = tokio::join!(
        drive_stream(&call, snapshot, Delivery::ToolResult, events),
        pump
    );
    match outcome {
        Ok(mut frame) => {
            enrich_frame_diagnostics(
                client,
                call_id_of(data),
                workspace_id,
                project_id,
                repo_key_of(data),
                &mut frame,
            )
            .await;
            interpret(name, call.command, call.cwd, &frame)
        }
        Err(reason) => Streamed::NotRun(not_run(name, call.command, call.cwd, &reason)),
    }
}

/// What the service's `result` frame comes to: the result it made, or that it
/// did not run the command.
fn interpret(name: &str, command: &str, cwd: &str, frame: &Value) -> Streamed {
    if frame.get("ran").and_then(Value::as_bool) != Some(true) {
        let Some(reason) = frames::declined_reason(frame) else {
            return Streamed::NotRun(not_run(
                name,
                command,
                cwd,
                "the service declined to run it",
            ));
        };
        let explanation = frames::decline_explanation(&reason);
        let mut result = not_run(
            name,
            command,
            cwd,
            &format!("the remote machine can't match this one ({explanation})"),
        );
        if let Some(Value::Object(fields)) = &mut result.structured_content {
            fields.insert("declined_reason".into(), Value::String(reason));
            if frame.get("refused_code").is_some()
                || frame.get("refused_class").is_some()
                || frame.get("detail").is_some()
            {
                for key in ["refused_code", "refused_class", "detail"] {
                    if let Some(val) = frame.get(key).and_then(Value::as_str) {
                        fields.insert(key.into(), Value::String(val.to_string()));
                    }
                }
            }
        }
        return Streamed::NotRun(result);
    }
    if let Some(made) = frame.get("tool_result").filter(|made| made.is_object()) {
        let mut enriched = made.clone();
        if let Some(sc) = enriched
            .get_mut("structuredContent")
            .and_then(Value::as_object_mut)
        {
            for key in ["failures", "failing_on_base", "diagnostics"] {
                if !sc.contains_key(key) {
                    if let Some(val) = frame.get(key).filter(|v| !v.is_null()) {
                        sc.insert(key.to_string(), val.clone());
                    }
                }
            }
        } else if let Some(obj) = enriched.as_object_mut() {
            let mut sc = serde_json::Map::new();
            for key in ["failures", "failing_on_base", "diagnostics"] {
                if let Some(val) = frame.get(key).filter(|v| !v.is_null()) {
                    sc.insert(key.to_string(), val.clone());
                }
            }
            if !sc.is_empty() {
                obj.insert("structuredContent".to_string(), Value::Object(sc));
            }
        }
        return Streamed::Ran(result_from(name, &enriched, String::new()));
    }
    // It ran but sent no formatted result: the command's own status is all there is,
    // enriched with diagnostics when present on the frame.
    let exit = frame.get("exit").and_then(Value::as_i64).unwrap_or(0);
    let mut sc = serde_json::Map::new();
    sc.insert("exit".to_string(), Value::from(exit));
    sc.insert("ran".to_string(), Value::Bool(true));
    for key in ["failures", "failing_on_base", "diagnostics"] {
        if let Some(val) = frame.get(key).filter(|v| !v.is_null()) {
            sc.insert(key.to_string(), val.clone());
        }
    }
    let sc_val = Value::Object(sc);
    let text = format_diagnostic_report(&sc_val, "")
        .unwrap_or_else(|| format!("{name}: the command exited with status {exit}."));
    Streamed::Ran(ToolResult {
        content: vec![ContentItem::text(text)],
        structured_content: Some(sc_val),
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

fn failure_label(item: &Value) -> String {
    let file = item.get("file").and_then(Value::as_str).unwrap_or("");
    let line = item.get("line").and_then(Value::as_i64);
    let col = item.get("col").and_then(Value::as_i64);
    let code = item
        .get("error_code")
        .or_else(|| item.get("code"))
        .or_else(|| item.get("rule"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let symbol = item
        .get("symbol")
        .or_else(|| item.get("test"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let message = item.get("message").and_then(Value::as_str).unwrap_or("");
    let mut loc = String::new();
    if !file.is_empty() {
        loc.push_str(file);
        if let Some(l) = line.filter(|l| *l > 0) {
            loc.push_str(&format!(":{l}"));
            if let Some(c) = col.filter(|c| *c > 0) {
                loc.push_str(&format!(":{c}"));
            }
        }
    }
    let mut parts = Vec::new();
    if !loc.is_empty() {
        parts.push(loc);
    }
    if !code.is_empty() {
        parts.push(format!("[{code}]"));
    }
    if !symbol.is_empty() {
        parts.push(format!("({symbol})"));
    }
    if !message.is_empty() {
        let short: String = message.chars().take(160).collect();
        parts.push(short);
    }
    let callers: Vec<&str> = item
        .get("callers")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_str).take(3).collect())
        .unwrap_or_default();
    if !callers.is_empty() {
        parts.push(format!("callers: {}", callers.join(", ")));
    }
    let defs: Vec<&str> = item
        .get("definitions")
        .and_then(Value::as_array)
        .map(|arr| arr.iter().filter_map(Value::as_str).take(2).collect())
        .unwrap_or_default();
    if !defs.is_empty() {
        parts.push(format!("defs: {}", defs.join(", ")));
    }
    parts.join(" ")
}

fn is_noisy_passing_line(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();
    lower.starts_with("compiling ")
        || lower.starts_with("checking ")
        || lower.starts_with("downloading ")
        || lower.starts_with("downloaded ")
        || lower.starts_with("finished ")
        || lower.starts_with("running ")
        || lower.starts_with("pass ")
        || lower.starts_with("✓ ")
        || lower.starts_with("ok ")
        || lower.starts_with("--- pass")
        || lower.starts_with("=== run")
        || (lower.starts_with("test ") && lower.ends_with(" ... ok"))
        || (lower.starts_with("ok\t") || lower.starts_with("ok  "))
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn contains_token(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let h_bytes = haystack.as_bytes();
    let mut start = 0usize;
    while start <= haystack.len() {
        let Some(rel) = haystack[start..].find(needle) else {
            return false;
        };
        let idx = start + rel;
        let end = idx + needle.len();
        let left_ok = idx == 0 || !is_ident_byte(h_bytes[idx - 1]);
        let right_ok = end == h_bytes.len() || !is_ident_byte(h_bytes[end]);
        if left_ok && right_ok {
            return true;
        }
        let step = haystack[idx..].chars().next().map_or(1, char::len_utf8);
        start = idx + step;
    }
    false
}

fn item_codes_lower(items: &[Value]) -> std::collections::HashSet<String> {
    let mut codes = std::collections::HashSet::new();
    for item in items {
        for key in ["error_code", "code", "rule"] {
            if let Some(val) = item.get(key).and_then(Value::as_str) {
                let v = val.trim();
                if v.len() >= 3 {
                    codes.insert(v.to_ascii_lowercase());
                }
            }
        }
    }
    codes
}

fn item_files_lower(items: &[Value]) -> std::collections::HashSet<String> {
    let mut files = std::collections::HashSet::new();
    for item in items {
        if let Some(val) = item.get("file").and_then(Value::as_str) {
            let v = val.trim();
            if v.len() >= 3 {
                files.insert(v.to_ascii_lowercase());
                if let Some(base) = v.rsplit('/').next() {
                    if base.len() >= 3 {
                        files.insert(base.to_ascii_lowercase());
                    }
                }
            }
        }
    }
    files
}

fn line_matches_any_token(line_lower: &str, tokens: &[String]) -> bool {
    tokens.iter().any(|t| contains_token(line_lower, t))
}

fn line_matches_symbol_or_test(line_lower: &str, items: &[Value]) -> bool {
    items.iter().any(|item| {
        for key in ["symbol", "test"] {
            if let Some(val) = item.get(key).and_then(Value::as_str) {
                let v = val.trim();
                if v.len() >= 3 && contains_token(line_lower, &v.to_ascii_lowercase()) {
                    return true;
                }
            }
        }
        false
    })
}

fn line_matches_identity(line_lower: &str, items: &[Value]) -> bool {
    items.iter().any(|item| {
        for key in ["file", "symbol", "test"] {
            if let Some(val) = item.get(key).and_then(Value::as_str) {
                let v = val.trim();
                if v.len() >= 3 && contains_token(line_lower, &v.to_ascii_lowercase()) {
                    return true;
                }
                if key == "file" {
                    if let Some(base) = v.rsplit('/').next() {
                        if base.len() >= 3 && contains_token(line_lower, &base.to_ascii_lowercase())
                        {
                            return true;
                        }
                    }
                }
            }
        }
        false
    })
}

fn line_matches_code(
    line_lower: &str,
    items: &[Value],
    excluded_codes: Option<&std::collections::HashSet<String>>,
) -> bool {
    items.iter().any(|item| {
        for key in ["error_code", "code", "rule"] {
            if let Some(val) = item.get(key).and_then(Value::as_str) {
                let v = val.trim();
                if v.len() >= 3 {
                    let lower = v.to_ascii_lowercase();
                    if excluded_codes.is_some_and(|ex| ex.contains(&lower)) {
                        continue;
                    }
                    if contains_token(line_lower, &lower) {
                        return true;
                    }
                }
            }
        }
        false
    })
}

fn is_pytest_failure_section_header(trimmed: &str) -> bool {
    trimmed.starts_with("____ ")
        || (trimmed.len() > 8 && trimmed.starts_with("____") && trimmed.ends_with("____"))
}

fn is_pytest_banner_header(trimmed: &str) -> bool {
    trimmed.starts_with("==== ")
        || (trimmed.len() > 8 && trimmed.starts_with("====") && trimmed.ends_with("===="))
}

fn is_pytest_captured_subheader(trimmed: &str) -> bool {
    if !(trimmed.len() > 8 && trimmed.starts_with("----") && trimmed.ends_with("----")) {
        return false;
    }
    let inner = trimmed.trim_matches('-').trim().to_ascii_lowercase();
    inner.starts_with("live log ")
        || (inner.starts_with("captured ")
            && (inner.ends_with(" call")
                || inner.ends_with(" setup")
                || inner.ends_with(" teardown")))
}

fn is_cargo_test_section_header(trimmed: &str) -> bool {
    !is_pytest_captured_subheader(trimmed)
        && trimmed.len() > 8
        && trimmed.starts_with("----")
        && (trimmed.ends_with(" stdout ----") || trimmed.ends_with(" stderr ----"))
}

fn is_non_captured_dash_header(trimmed: &str) -> bool {
    !is_pytest_captured_subheader(trimmed)
        && (trimmed.starts_with("---- ")
            || (trimmed.len() > 8 && trimmed.starts_with("----") && trimmed.ends_with("----")))
}

fn is_test_case_header_line(trimmed: &str) -> bool {
    trimmed.starts_with("FAILED ")
        || trimmed.starts_with("ERROR ")
        || trimmed.starts_with("FAIL ")
        || trimmed.starts_with("--- FAIL:")
        || trimmed.starts_with("● ")
        || trimmed.starts_with("× ")
        || trimmed.starts_with("✗ ")
}

fn is_named_error_or_exception_line(trimmed: &str) -> bool {
    let Some((prefix, rest)) = trimmed.split_once(':') else {
        return false;
    };
    if prefix.is_empty()
        || !(prefix.ends_with("Error") || prefix.ends_with("Exception"))
        || prefix.contains("..")
    {
        return false;
    }
    let bytes = prefix.as_bytes();
    if !(bytes[0].is_ascii_alphabetic() || bytes[0] == b'_') {
        return false;
    }
    if !bytes
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.')
    {
        return false;
    }
    let final_segment = prefix.rsplit('.').next().unwrap_or(prefix);
    if !final_segment
        .as_bytes()
        .first()
        .is_some_and(|b| b.is_ascii_uppercase())
    {
        return false;
    }
    rest.is_empty() || rest.starts_with(|c: char| c.is_ascii_whitespace())
}

fn is_goroutine_header_line(trimmed: &str) -> bool {
    let Some(rest) = trimmed.strip_prefix("goroutine ") else {
        return false;
    };
    let digits = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
    digits > 0 && rest[digits..].starts_with(" [")
}

fn is_go_stack_frame_line(trimmed: &str) -> bool {
    trimmed.ends_with(')')
        || trimmed.starts_with("created by ")
        || trimmed.starts_with("...")
        || trimmed.starts_with("[originating from goroutine ")
}

fn is_failure_header_line(trimmed: &str) -> bool {
    let lower = trimmed.to_ascii_lowercase();
    is_test_case_header_line(trimmed)
        || trimmed.starts_with("error[")
        || lower.starts_with("error:")
        || lower.starts_with("fatal:")
        || lower.starts_with("panic:")
        || is_named_error_or_exception_line(trimmed)
}

fn is_failure_continuation_line(line: &str, trimmed: &str) -> bool {
    if trimmed.is_empty() {
        return false;
    }
    if line.starts_with(' ') || line.starts_with('\t') {
        return true;
    }
    if trimmed.starts_with('|')
        || trimmed.starts_with("-->")
        || trimmed.starts_with("= ")
        || trimmed.starts_with("> ")
        || trimmed.starts_with("E ")
        || trimmed.starts_with("E\t")
        || trimmed.starts_with("at ")
        || trimmed.starts_with("_ _ _")
        || is_named_error_or_exception_line(trimmed)
        || is_goroutine_header_line(trimmed)
        || trimmed.starts_with("note:")
        || trimmed.starts_with("help:")
        || trimmed.starts_with("left:")
        || trimmed.starts_with("right:")
        || trimmed.starts_with("❯")
        || trimmed.starts_with("Expected:")
        || trimmed.starts_with("Received:")
        || trimmed.starts_with("Caused by:")
        || trimmed.starts_with("Serialized Error:")
    {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();
    if (lower.starts_with("assertion ") && lower.contains("failed"))
        || (trimmed.starts_with("thread '") && trimmed.contains(" panicked"))
    {
        return true;
    }
    if let Some((prefix, _)) = trimmed.split_once('|') {
        let p = prefix.trim();
        if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) {
            return true;
        }
    }
    false
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiagBlockKind {
    PytestSection {
        in_captured: bool,
    },
    CargoTestSection,
    FailureBlock {
        is_test_case: bool,
        in_goroutine_stack: bool,
    },
    Line,
    ExitFooter,
}

#[derive(Clone, Copy)]
struct DiagBlock {
    kind: DiagBlockKind,
    start: usize,
    end: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DiagDisposition {
    SuppressOld,
    KeepNew,
    KeepGeneric,
    Skip,
}

fn line_starts_diagnostic_block(
    trimmed_lower: &str,
    new_items: &[Value],
    old_items: &[Value],
    shared_codes: &std::collections::HashSet<String>,
) -> bool {
    (!new_items.is_empty() && line_matches_identity(trimmed_lower, new_items))
        || (!old_items.is_empty() && line_matches_identity(trimmed_lower, old_items))
        || (!new_items.is_empty()
            && line_matches_code(trimmed_lower, new_items, Some(shared_codes)))
        || (!old_items.is_empty()
            && line_matches_code(trimmed_lower, old_items, Some(shared_codes)))
        || trimmed_lower.contains("error")
        || trimmed_lower.contains("failed")
        || trimmed_lower.contains("panic")
        || trimmed_lower.contains("fatal")
}

fn segment_diagnostic_blocks(
    lines: &[&str],
    new_items: &[Value],
    old_items: &[Value],
    shared_codes: &std::collections::HashSet<String>,
) -> Vec<DiagBlock> {
    let mut blocks = Vec::new();
    let mut active: Option<DiagBlock> = None;
    let mut in_summary_list = false;

    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("[exit ") && idx + 1 == lines.len() {
            if let Some(b) = active.take() {
                blocks.push(b);
            }
            in_summary_list = false;
            blocks.push(DiagBlock {
                kind: DiagBlockKind::ExitFooter,
                start: idx,
                end: idx + 1,
            });
            continue;
        }

        if let Some(mut b) = active {
            match b.kind {
                DiagBlockKind::PytestSection { in_captured } => {
                    if is_pytest_captured_subheader(trimmed) {
                        b.kind = DiagBlockKind::PytestSection { in_captured: true };
                        b.end = idx + 1;
                        active = Some(b);
                        continue;
                    }
                    let exit_section = is_pytest_failure_section_header(trimmed)
                        || is_pytest_banner_header(trimmed)
                        || is_cargo_test_section_header(trimmed)
                        || (!in_captured
                            && (trimmed.starts_with("FAILED ")
                                || trimmed.starts_with("FAIL ")
                                || trimmed.starts_with("--- FAIL:")
                                || trimmed.starts_with("error[")));
                    if !exit_section {
                        b.end = idx + 1;
                        active = Some(b);
                        continue;
                    }
                    blocks.push(b);
                    active = None;
                }
                DiagBlockKind::CargoTestSection => {
                    let exit_section = is_pytest_failure_section_header(trimmed)
                        || is_pytest_banner_header(trimmed)
                        || is_non_captured_dash_header(trimmed)
                        || trimmed == "failures:"
                        || trimmed.starts_with("test result:")
                        || trimmed.starts_with("FAILED ")
                        || trimmed.starts_with("FAIL ")
                        || trimmed.starts_with("FAIL\t")
                        || trimmed.starts_with("--- FAIL:")
                        || trimmed.starts_with("error[")
                        || (!trimmed.is_empty() && is_noisy_passing_line(trimmed));
                    if !exit_section {
                        b.end = idx + 1;
                        active = Some(b);
                        continue;
                    }
                    blocks.push(b);
                    active = None;
                }
                DiagBlockKind::FailureBlock {
                    is_test_case,
                    in_goroutine_stack,
                } => {
                    let force_exit = is_pytest_failure_section_header(trimmed)
                        || is_pytest_banner_header(trimmed)
                        || is_non_captured_dash_header(trimmed)
                        || trimmed == "failures:"
                        || trimmed.starts_with("test result:")
                        || is_test_case_header_line(trimmed)
                        || trimmed.starts_with("FAIL\t")
                        || trimmed.starts_with("error[")
                        || (!trimmed.is_empty() && is_noisy_passing_line(trimmed));
                    if force_exit {
                        blocks.push(b);
                        active = None;
                    } else if trimmed.is_empty() {
                        let keep_blank = (idx + 1..lines.len())
                            .find(|&j| !lines[j].trim().is_empty())
                            .is_some_and(|next_idx| {
                                let next_line = lines[next_idx];
                                let next_trimmed = next_line.trim();
                                let next_exit = next_trimmed.starts_with("[exit ")
                                    && next_idx + 1 == lines.len();
                                let next_force_exit =
                                    is_pytest_failure_section_header(next_trimmed)
                                        || is_pytest_banner_header(next_trimmed)
                                        || is_non_captured_dash_header(next_trimmed)
                                        || next_trimmed == "failures:"
                                        || next_trimmed.starts_with("test result:")
                                        || is_test_case_header_line(next_trimmed)
                                        || next_trimmed.starts_with("FAIL\t")
                                        || next_trimmed.starts_with("error[")
                                        || is_noisy_passing_line(next_trimmed);
                                let next_lower = next_trimmed.to_ascii_lowercase();
                                let next_is_test_case_detail = is_test_case
                                    && (next_lower.starts_with("error:")
                                        || next_lower.starts_with("fatal:")
                                        || next_lower.starts_with("panic:")
                                        || next_lower.starts_with("fatal error:")
                                        || next_lower.starts_with("runtime error:"));
                                !next_exit
                                    && !next_force_exit
                                    && ((is_failure_continuation_line(next_line, next_trimmed)
                                        && (is_test_case || !is_failure_header_line(next_trimmed)))
                                        || next_is_test_case_detail)
                            });
                        if keep_blank {
                            b.end = idx + 1;
                            active = Some(b);
                        } else {
                            blocks.push(b);
                            active = None;
                        }
                        continue;
                    } else if is_goroutine_header_line(trimmed) {
                        b.kind = DiagBlockKind::FailureBlock {
                            is_test_case,
                            in_goroutine_stack: true,
                        };
                        b.end = idx + 1;
                        active = Some(b);
                        continue;
                    } else if (is_failure_continuation_line(line, trimmed)
                        && (is_test_case
                            || line.starts_with(' ')
                            || line.starts_with('\t')
                            || !is_failure_header_line(trimmed)))
                        || (in_goroutine_stack && is_go_stack_frame_line(trimmed))
                    {
                        b.end = idx + 1;
                        active = Some(b);
                        continue;
                    } else {
                        let lower = trimmed.to_ascii_lowercase();
                        if (is_test_case || in_goroutine_stack)
                            && (lower.starts_with("error:")
                                || lower.starts_with("fatal:")
                                || lower.starts_with("panic:")
                                || lower.starts_with("fatal error:")
                                || lower.starts_with("runtime error:"))
                        {
                            b.end = idx + 1;
                            active = Some(b);
                            continue;
                        }
                        blocks.push(b);
                        active = None;
                    }
                }
                DiagBlockKind::Line | DiagBlockKind::ExitFooter => {
                    blocks.push(b);
                    active = None;
                }
            }
        }

        if trimmed == "failures:" {
            in_summary_list = true;
            continue;
        }
        if trimmed.is_empty() || is_noisy_passing_line(trimmed) {
            in_summary_list = false;
            continue;
        }
        if trimmed.starts_with("test result:") || is_pytest_banner_header(trimmed) {
            in_summary_list = false;
            blocks.push(DiagBlock {
                kind: DiagBlockKind::Line,
                start: idx,
                end: idx + 1,
            });
            continue;
        }
        if is_pytest_failure_section_header(trimmed) {
            in_summary_list = false;
            active = Some(DiagBlock {
                kind: DiagBlockKind::PytestSection { in_captured: false },
                start: idx,
                end: idx + 1,
            });
            continue;
        }
        if is_non_captured_dash_header(trimmed) {
            in_summary_list = false;
            active = Some(DiagBlock {
                kind: DiagBlockKind::CargoTestSection,
                start: idx,
                end: idx + 1,
            });
            continue;
        }
        if in_summary_list {
            blocks.push(DiagBlock {
                kind: DiagBlockKind::Line,
                start: idx,
                end: idx + 1,
            });
            continue;
        }
        if is_failure_header_line(trimmed) {
            active = Some(DiagBlock {
                kind: DiagBlockKind::FailureBlock {
                    is_test_case: is_test_case_header_line(trimmed),
                    in_goroutine_stack: false,
                },
                start: idx,
                end: idx + 1,
            });
            continue;
        }
        let lower = trimmed.to_ascii_lowercase();
        if !is_failure_continuation_line(line, trimmed)
            && line_starts_diagnostic_block(&lower, new_items, old_items, shared_codes)
        {
            active = Some(DiagBlock {
                kind: DiagBlockKind::FailureBlock {
                    is_test_case: false,
                    in_goroutine_stack: false,
                },
                start: idx,
                end: idx + 1,
            });
            continue;
        }
        blocks.push(DiagBlock {
            kind: DiagBlockKind::Line,
            start: idx,
            end: idx + 1,
        });
    }

    if let Some(b) = active {
        blocks.push(b);
    }
    blocks
}

#[allow(clippy::too_many_arguments)]
fn classify_diagnostic_block(
    lines: &[&str],
    block: DiagBlock,
    new_items: &[Value],
    old_items: &[Value],
    shared_codes: &std::collections::HashSet<String>,
    new_only_files: &[String],
    old_only_files: &[String],
) -> DiagDisposition {
    if block.kind == DiagBlockKind::ExitFooter {
        return DiagDisposition::KeepNew;
    }

    let hdr_lower = lines[block.start].trim().to_ascii_lowercase();
    let hdr_new_id = !new_items.is_empty() && line_matches_identity(&hdr_lower, new_items);
    let hdr_old_id = !old_items.is_empty() && line_matches_identity(&hdr_lower, old_items);
    let hdr_new_sym = !new_items.is_empty() && line_matches_symbol_or_test(&hdr_lower, new_items);
    let hdr_old_sym = !old_items.is_empty() && line_matches_symbol_or_test(&hdr_lower, old_items);
    let hdr_new_only_file = line_matches_any_token(&hdr_lower, new_only_files);
    let hdr_old_only_file = line_matches_any_token(&hdr_lower, old_only_files);
    let hdr_new_unshared_code =
        !new_items.is_empty() && line_matches_code(&hdr_lower, new_items, Some(shared_codes));
    let hdr_old_unshared_code =
        !old_items.is_empty() && line_matches_code(&hdr_lower, old_items, Some(shared_codes));

    let mut any_new_id = false;
    let mut any_old_id = false;
    let mut any_new_sym = false;
    let mut any_old_sym = false;
    let mut any_new_only_file = false;
    let mut any_old_only_file = false;
    let mut any_generic_failure = false;

    for line in &lines[block.start..block.end] {
        let t = line.trim();
        if t.is_empty() || is_noisy_passing_line(t) {
            continue;
        }
        let lower = t.to_ascii_lowercase();
        if !new_items.is_empty() {
            any_new_id |= line_matches_identity(&lower, new_items);
            any_new_sym |= line_matches_symbol_or_test(&lower, new_items);
        }
        if !old_items.is_empty() {
            any_old_id |= line_matches_identity(&lower, old_items);
            any_old_sym |= line_matches_symbol_or_test(&lower, old_items);
        }
        any_new_only_file |= line_matches_any_token(&lower, new_only_files);
        any_old_only_file |= line_matches_any_token(&lower, old_only_files);
        any_generic_failure |= lower.contains("error")
            || lower.contains("failed")
            || lower.contains("panic")
            || lower.contains("fatal");
    }

    // Priority 1: Symbol/test disambiguation across whole block
    if any_old_sym && !any_new_sym && !any_new_only_file {
        return DiagDisposition::SuppressOld;
    }
    if any_new_sym && !any_old_sym && !any_old_only_file {
        return DiagDisposition::KeepNew;
    }

    // Priority 2: Whole-block identity
    if any_old_id && !any_new_id {
        return DiagDisposition::SuppressOld;
    }
    if any_new_id && !any_old_id {
        return DiagDisposition::KeepNew;
    }
    if any_new_id && any_old_id {
        if hdr_old_sym && !hdr_new_sym && !hdr_new_only_file {
            return DiagDisposition::SuppressOld;
        }
        if hdr_new_sym && !hdr_old_sym && !hdr_old_only_file {
            return DiagDisposition::KeepNew;
        }
        if hdr_old_id && !hdr_new_id {
            return DiagDisposition::SuppressOld;
        }
        if hdr_new_id && !hdr_old_id {
            return DiagDisposition::KeepNew;
        }
        if any_old_only_file && !any_new_only_file {
            return DiagDisposition::SuppressOld;
        }
        if any_new_only_file && !any_old_only_file {
            return DiagDisposition::KeepNew;
        }
        if hdr_old_unshared_code && !hdr_new_unshared_code {
            return DiagDisposition::SuppressOld;
        }
        if hdr_new_unshared_code && !hdr_old_unshared_code {
            return DiagDisposition::KeepNew;
        }
        return DiagDisposition::KeepNew;
    }

    // Priority 3: Header-only unshared error_code (never body lines)
    if hdr_old_unshared_code && !hdr_new_unshared_code {
        return DiagDisposition::SuppressOld;
    }
    if hdr_new_unshared_code && !hdr_old_unshared_code {
        return DiagDisposition::KeepNew;
    }

    // Priority 4: Generic unstructured failure fallback
    if any_generic_failure {
        return DiagDisposition::KeepGeneric;
    }
    DiagDisposition::Skip
}

fn trim_diagnostic_slice(
    raw_text: &str,
    new_items: &[Value],
    old_items: &[Value],
    max_bytes: usize,
) -> String {
    if raw_text.trim().is_empty() || max_bytes == 0 {
        return String::new();
    }
    let lines: Vec<&str> = raw_text.lines().collect();
    let new_codes = item_codes_lower(new_items);
    let old_codes = item_codes_lower(old_items);
    let shared_codes: std::collections::HashSet<String> =
        new_codes.intersection(&old_codes).cloned().collect();
    let new_files = item_files_lower(new_items);
    let old_files = item_files_lower(old_items);
    let new_only_files: Vec<String> = new_files.difference(&old_files).cloned().collect();
    let old_only_files: Vec<String> = old_files.difference(&new_files).cloned().collect();

    let blocks = segment_diagnostic_blocks(&lines, new_items, old_items, &shared_codes);
    let mut out = String::new();
    let mut budget_exhausted = false;

    for block in blocks {
        let disp = classify_diagnostic_block(
            &lines,
            block,
            new_items,
            old_items,
            &shared_codes,
            &new_only_files,
            &old_only_files,
        );
        if !matches!(
            disp,
            DiagDisposition::KeepNew | DiagDisposition::KeepGeneric
        ) {
            continue;
        }
        for line in &lines[block.start..block.end] {
            if block.kind != DiagBlockKind::ExitFooter
                && (line.trim().is_empty() || is_noisy_passing_line(line))
            {
                continue;
            }
            let sep = usize::from(!out.is_empty());
            if out.len() + sep + line.len() > max_bytes {
                budget_exhausted = true;
                break;
            }
            if sep > 0 {
                out.push('\n');
            }
            out.push_str(line);
        }
        if budget_exhausted {
            break;
        }
    }
    out
}

/// Format a compact structured diagnostic header and trimmed diagnostic slice
/// from `structured` (`failures`, `failing_on_base`, `diagnostics`) and `raw_text`.
/// Returns `None` when no structured failures or diagnostic enrichment are present.
pub fn format_diagnostic_report(structured: &Value, raw_text: &str) -> Option<String> {
    let diag = structured.get("diagnostics").filter(|v| v.is_object());
    let fob = structured.get("failing_on_base").filter(|v| v.is_object());
    let raw_failures = structured
        .get("failures")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    let mut new_items: Vec<Value> = diag
        .and_then(|d| d.get("new_failures"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if new_items.is_empty() {
        if let Some(fob_new) = fob.and_then(|f| f.get("new")).and_then(Value::as_array) {
            new_items = fob_new.clone();
        } else {
            new_items = raw_failures;
        }
    }
    let old_items: Vec<Value> = fob
        .and_then(|f| f.get("old"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let base_suppressed = diag
        .and_then(|d| d.get("base_failures_suppressed"))
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(old_items.len());
    let new_count = diag
        .and_then(|d| d.get("new_failures_count"))
        .and_then(Value::as_u64)
        .map(|n| n as usize)
        .unwrap_or(new_items.len());

    let decisions: Vec<&Value> = diag
        .and_then(|d| d.get("decisions"))
        .and_then(Value::as_array)
        .map(|a| a.iter().take(3).collect())
        .unwrap_or_default();
    let commits: Vec<&Value> = diag
        .and_then(|d| d.get("commits"))
        .and_then(Value::as_array)
        .map(|a| a.iter().take(3).collect())
        .unwrap_or_default();
    let lessons: Vec<&Value> = diag
        .and_then(|d| d.get("lessons"))
        .and_then(Value::as_array)
        .map(|a| a.iter().take(3).collect())
        .unwrap_or_default();

    if new_items.is_empty()
        && base_suppressed == 0
        && decisions.is_empty()
        && commits.is_empty()
        && lessons.is_empty()
    {
        return None;
    }

    let mut header = if base_suppressed > 0 || fob.is_some() {
        format!(
            "[diagnostics: {} new failure(s), {} base failure(s) suppressed]",
            new_count, base_suppressed
        )
    } else {
        format!("[diagnostics: {} failure(s)]", new_count)
    };

    if !commits.is_empty() {
        let cited: Vec<String> = commits
            .iter()
            .filter_map(|c| {
                let sha = c.get("sha").and_then(Value::as_str)?;
                let msg = c.get("message").and_then(Value::as_str).unwrap_or("");
                let short_msg: String = msg.chars().take(60).collect();
                Some(format!("{sha} {short_msg}").trim().to_string())
            })
            .collect();
        if !cited.is_empty() {
            header.push_str(&format!("\ncommits: {}", cited.join(" | ")));
        }
    }
    if !decisions.is_empty() {
        let cited: Vec<String> = decisions
            .iter()
            .filter_map(|d| d.get("summary").and_then(Value::as_str))
            .map(|s| s.chars().take(80).collect())
            .collect();
        if !cited.is_empty() {
            header.push_str(&format!("\ndecisions: {}", cited.join(" | ")));
        }
    }
    if !lessons.is_empty() {
        let cited: Vec<String> = lessons
            .iter()
            .filter_map(|l| {
                let title = l.get("title").and_then(Value::as_str)?;
                let prev = l.get("prevention").and_then(Value::as_str).unwrap_or("");
                if prev.is_empty() {
                    Some(title.chars().take(80).collect())
                } else {
                    Some(format!(
                        "{}: {}",
                        title.chars().take(50).collect::<String>(),
                        prev.chars().take(60).collect::<String>()
                    ))
                }
            })
            .collect();
        if !cited.is_empty() {
            header.push_str(&format!("\nlessons: {}", cited.join(" | ")));
        }
    }

    for item in new_items.iter().take(10) {
        let line = failure_label(item);
        if !line.is_empty() {
            header.push_str(&format!("\n- {line}"));
        }
    }
    if new_items.len() > 10 {
        header.push_str(&format!(
            "\n(+{} more new failure(s))",
            new_items.len() - 10
        ));
    }
    if base_suppressed > 0 {
        header.push_str(&format!(
            "\n(+{base_suppressed} base failure(s) suppressed)"
        ));
    }

    let raw_len = raw_text.len();
    // Monotonic piecewise-linear budget: preserve at least 512 bytes for short
    // outputs while keeping large noisy outputs well below 50% of raw bytes.
    let scaled_budget = (raw_len * 45 / 100).saturating_sub(header.len()).min(4096);
    let slice_budget = scaled_budget.max(512);
    let slice = trim_diagnostic_slice(raw_text, &new_items, &old_items, slice_budget);
    if slice.is_empty() {
        Some(header)
    } else {
        Some(format!("{header}\n{slice}"))
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
    if let Some(sc) = &structured_content {
        let raw_text = content
            .iter()
            .filter_map(|item| match item {
                ContentItem::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if let Some(formatted) = format_diagnostic_report(sc, &raw_text) {
            content = vec![ContentItem::text(formatted)];
        }
    }
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
