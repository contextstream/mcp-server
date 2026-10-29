//! Antigravity (desktop app and `agy`) lifecycle hooks, installed as the
//! `contextstream` entry of the ContextStream plugin's `hooks.json`
//! (`~/.gemini/config/plugins/contextstream/hooks.json`).
//!
//! Antigravity runs hook commands in the plugin's folder, so the session's
//! folder comes from `workspacePaths[0]`, and its session from
//! `conversationId`. Antigravity reaches MCP tools only through one generic
//! `call_mcp_tool`, so an agent nobody reminds falls back to its own grep and
//! find. These hooks bring the ContextStream workflow to it outside the rules
//! budget:
//! - `SessionStart` (`agy-session-start`): records the repository and agent
//!   handle the first message names, and adds a system message with the
//!   workflow for this workspace.
//! - `PreInvocation` (`agy-pre-invocation`): runs before each model call.
//!   Shows notices a background poll fetched as a system message, which stays
//!   in the conversation, and starts the next poll when one is due. Then, as
//!   an ephemeral message: until a ContextStream call grounds the request, a
//!   reminder to call `init` (once) and `context` (at most
//!   [`MAX_GROUNDING_REMINDERS`] per request); after a local code search in an
//!   indexed folder before any ContextStream `search` for the request, the
//!   `search` call to make instead (at most [`MAX_SEARCH_NUDGES`]).
//! - `PostToolUse` (`agy-post-tool-use`, matcher `*`): records ContextStream
//!   calls and local code searches for the next `PreInvocation`, and reads
//!   tool arguments (`CommandLine`, `TargetFile`, ...) so a scratch-folder
//!   session is scoped to the repository it works in. Outputs `{}`.
//!
//! There is no `PreToolUse` hook: Antigravity denies a tool call when a
//! `PreToolUse` hook answers `{}`, and `"allow"` would skip the person's own
//! permission settings, so such a hook could not stay out of the way.
//!
//! Per-conversation state lives in `~/.contextstream/agy-sessions/`.

use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;

use super::common;
use super::durable_capture::{sha256_hex, state_dir, with_locked_json};
use super::mid_run_notices;
use super::pre_tool_use;
use super::read_stdin_json;

/// Reminders to ground one request before the hooks stop asking.
const MAX_GROUNDING_REMINDERS: u32 = 3;
/// Search-first nudges for one request.
const MAX_SEARCH_NUDGES: u32 = 2;
/// Conversation state untouched this long goes at the next session start.
const MAX_STATE_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Antigravity's own code-search tools, by hook and by model-facing name.
const LOCAL_SEARCH_TOOLS: &[&str] = &[
    "grep_search",
    "find_by_name",
    "find",
    "codebase_search",
    "code_search",
];
const SHELL_TOOLS: &[&str] = &["run_command", "shell_exec"];
const MCP_TOOL: &str = "call_mcp_tool";

/// What the hooks know about one conversation.
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
struct Conversation {
    /// Hash of the request (`lastUserInput`) the fields below describe.
    #[serde(default)]
    request: String,
    /// `init` or `context` ran in this conversation.
    #[serde(default)]
    initialized: bool,
    /// A ContextStream call ran for this request.
    #[serde(default)]
    grounded: bool,
    /// ContextStream `search` ran for this request.
    #[serde(default)]
    searched: bool,
    #[serde(default)]
    grounding_reminders: u32,
    #[serde(default)]
    search_nudges: u32,
    /// The latest local code search, for the next model call's nudge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    local_search: Option<LocalSearch>,
    #[serde(default)]
    updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct LocalSearch {
    tool: String,
    query: String,
}

/// A tool call, as far as these hooks care.
#[derive(Debug, PartialEq)]
enum ToolCall {
    ContextStream(String),
    LocalSearch(LocalSearch),
    Other,
}

impl Conversation {
    /// Start a new request's counts on the first model call of a request, or
    /// when the request text changed without one.
    fn begin_request(&mut self, request: &str, first_invocation: bool) {
        if first_invocation || self.request != request {
            *self = Conversation {
                request: request.to_string(),
                initialized: self.initialized,
                ..Conversation::default()
            };
        }
    }

    fn observe(&mut self, call: ToolCall) {
        match call {
            ToolCall::ContextStream(tool) => {
                self.grounded = true;
                if matches!(tool.as_str(), "init" | "context") {
                    self.initialized = true;
                }
                if tool == "search" {
                    self.searched = true;
                    self.local_search = None;
                }
            }
            ToolCall::LocalSearch(search) if !self.searched => self.local_search = Some(search),
            ToolCall::LocalSearch(_) | ToolCall::Other => {}
        }
    }

    /// The ephemeral message for the next model call, if one is due.
    fn next_guidance(&mut self, workspace: &str) -> Option<String> {
        if !self.grounded && self.grounding_reminders < MAX_GROUNDING_REMINDERS {
            self.grounding_reminders += 1;
            return Some(grounding_reminder(workspace, !self.initialized));
        }
        let search = self.local_search.take()?;
        if self.searched || self.search_nudges >= MAX_SEARCH_NUDGES {
            return None;
        }
        self.search_nudges += 1;
        Some(search_nudge(&search))
    }
}

fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

fn session_guidance(workspace: &str) -> String {
    let folder = json_string(workspace);
    format!(
        "ContextStream is connected for this workspace as the `contextstream` MCP server. Call its tools with `call_mcp_tool` (`ServerName` \"contextstream\"):\n\
         - First, `init` with {{\"folder_path\": {folder}}}. Then, at the start of each request and before other tools, `context` with {{\"user_message\": \"<the request>\", \"session_id\": \"<from init>\"}}, and act on the lessons, preferences and prior work it returns.\n\
         - Find code with `search` ({{\"query\": \"...\", \"mode\": \"auto\"}}) before `grep_search`, `find_by_name`, or grep/rg/find in `run_command`; open the files it returns with `view_file`.\n\
         - Keep decisions, lessons and plans in ContextStream (`session`, `memory`), not in local files. The `contextstream` skill is the full reference."
    )
}

fn grounding_reminder(workspace: &str, needs_init: bool) -> String {
    let init = if needs_init {
        format!(
            "`call_mcp_tool(ServerName=\"contextstream\", ToolName=\"init\", Arguments={{\"folder_path\": {}}})`, then ",
            json_string(workspace)
        )
    } else {
        String::new()
    };
    format!(
        "[ContextStream] Ground this request before other tools: {init}`call_mcp_tool(ServerName=\"contextstream\", ToolName=\"context\", Arguments={{\"user_message\": \"<this request>\", \"session_id\": \"<from init>\"}})`. Then find code with ContextStream `search` before `grep_search`, `find_by_name`, or grep/rg/find in `run_command`."
    )
}

fn search_nudge(search: &LocalSearch) -> String {
    let query = search.query.trim();
    let (shown, arguments) = if query.is_empty() {
        (
            String::new(),
            "{\"query\": \"<what you are looking for>\", \"mode\": \"auto\"}".to_string(),
        )
    } else {
        (
            format!(" for {}", json_string(query)),
            format!(
                "{{\"query\": {}, \"mode\": \"{}\"}}",
                json_string(query),
                pre_tool_use::search_mode_for(query)
            ),
        )
    };
    format!(
        "[ContextStream] You searched with `{tool}`{shown}. ContextStream has an index of this folder: search it first with `call_mcp_tool(ServerName=\"contextstream\", ToolName=\"search\", Arguments={arguments})`. Its results are file paths with line numbers. Keep local search for files created in this conversation, logs and command output, or for when ContextStream finds nothing relevant.",
        tool = search.tool
    )
}

fn is_contextstream_server(name: &str) -> bool {
    name.to_ascii_lowercase().contains("contextstream")
}

/// The first non-empty string among `keys` in `args`.
fn string_arg(args: &Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| {
            args.get(*key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_default()
        .to_string()
}

fn classify(name: &str, args: &Value, folder_is_indexed: impl FnOnce() -> bool) -> ToolCall {
    if name == MCP_TOOL {
        let server = string_arg(args, &["ServerName", "serverName", "server_name"]);
        if is_contextstream_server(&server) {
            let tool = string_arg(args, &["ToolName", "toolName", "tool_name"]);
            return ToolCall::ContextStream(tool.to_ascii_lowercase());
        }
        return ToolCall::Other;
    }
    let search = if LOCAL_SEARCH_TOOLS.contains(&name) {
        Some(LocalSearch {
            tool: name.to_string(),
            query: string_arg(
                args,
                &["Query", "SearchQuery", "Pattern", "query", "pattern"],
            ),
        })
    } else if SHELL_TOOLS.contains(&name) {
        pre_tool_use::shell_code_search(&string_arg(args, &["CommandLine", "command"])).map(
            |(tool, query)| LocalSearch {
                tool: tool.to_string(),
                query,
            },
        )
    } else {
        None
    };
    match search {
        Some(search) if folder_is_indexed() => ToolCall::LocalSearch(search),
        _ => ToolCall::Other,
    }
}

fn sessions_dir() -> Option<PathBuf> {
    state_dir().map(|dir| dir.join("agy-sessions"))
}

fn state_path(input: &Value, cwd: &str) -> Option<PathBuf> {
    let key = mid_run_notices::session_state_key(input, cwd);
    sessions_dir().map(|dir| dir.join(format!("{key}.json")))
}

fn request_key(input: &Value) -> String {
    let request = input
        .get("lastUserInput")
        .and_then(Value::as_str)
        .unwrap_or_default();
    sha256_hex(request.as_bytes())
}

fn update_conversation<T>(
    input: &Value,
    cwd: &str,
    update: impl FnOnce(&mut Conversation) -> T,
) -> Option<T> {
    let path = state_path(input, cwd)?;
    with_locked_json(&path, |conversation: &mut Conversation| {
        let result = update(conversation);
        conversation.updated_at = Utc::now().to_rfc3339();
        result
    })
}

/// Remove conversation state nobody touched for a day.
fn remove_stale_state() {
    let Some(dir) = sessions_dir() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > MAX_STATE_AGE);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn session_init_enabled() -> bool {
    std::env::var("CONTEXTSTREAM_SESSION_INIT_ENABLED")
        .map(|value| value != "false")
        .unwrap_or(true)
}

fn write_agy_steps(steps: Vec<Value>) -> Result<()> {
    let out = if steps.is_empty() {
        json!({})
    } else {
        json!({ "injectSteps": steps })
    };
    println!("{}", serde_json::to_string(&out)?);
    Ok(())
}

pub async fn handle_session_start() -> Result<()> {
    let input = read_stdin_json()?;
    let cwd = common::extract_cwd(&input);
    mid_run_notices::observe_session_context(&input, &cwd);
    remove_stale_state();
    let mut steps = Vec::new();
    if session_init_enabled() && !cwd.trim().is_empty() {
        steps.push(json!({"systemMessage": {"systemMessage": session_guidance(&cwd)}}));
    }
    write_agy_steps(steps)
}

pub async fn handle_pre_invocation() -> Result<()> {
    let input = read_stdin_json()?;
    let cwd = common::extract_cwd(&input);
    let mut steps = Vec::new();
    if let Some(notice) = mid_run_notices::notice_for_hook(&input, &cwd, "antigravity") {
        steps.push(json!({"systemMessage": {"systemMessage": notice}}));
    }
    let first_invocation = input.get("invocationNum").and_then(Value::as_u64) == Some(0);
    let request = request_key(&input);
    let guidance = update_conversation(&input, &cwd, |conversation| {
        conversation.begin_request(&request, first_invocation);
        conversation.next_guidance(&cwd)
    })
    .flatten();
    if let Some(guidance) = guidance {
        steps.push(json!({ "ephemeralMessage": guidance }));
    }
    write_agy_steps(steps)
}

pub async fn handle_post_tool_use() -> Result<()> {
    let input = read_stdin_json()?;
    let cwd = common::extract_cwd(&input);
    if let Some(call) = input.get("toolCall") {
        let args = call.get("args").cloned().unwrap_or(Value::Null);
        if !args.is_null() {
            mid_run_notices::observe_tool_args(&input, &cwd, &args);
        }
        let name = call.get("name").and_then(Value::as_str).unwrap_or_default();
        let observed = classify(name, &args, || pre_tool_use::folder_is_indexed(&cwd));
        if observed != ToolCall::Other {
            let request = request_key(&input);
            update_conversation(&input, &cwd, |conversation| {
                conversation.begin_request(&request, false);
                conversation.observe(observed);
            });
        }
    }
    write_agy_steps(Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn antigravity_payloads_resolve_folder_and_session() {
        let input = json!({
            "conversationId": "550e8400-e29b-41d4-a716-446655440000",
            "workspacePaths": ["/home/dev/work/agent-proof.a1b2"],
            "invocationNum": 3,
        });
        assert_eq!(
            common::extract_cwd(&input),
            "/home/dev/work/agent-proof.a1b2"
        );
        assert_eq!(
            common::host_session_key(&input).as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
    }

    fn mcp(server: &str, tool: &str) -> Value {
        json!({"ServerName": server, "ToolName": tool, "Arguments": {}})
    }

    #[test]
    fn tool_calls_are_classified_from_antigravity_payloads() {
        assert_eq!(
            classify("call_mcp_tool", &mcp("contextstream", "Search"), || true),
            ToolCall::ContextStream("search".into())
        );
        assert_eq!(
            classify("call_mcp_tool", &mcp("github", "search"), || true),
            ToolCall::Other
        );
        assert_eq!(
            classify(
                "run_command",
                &json!({"CommandLine": "grep -rn \"mcpServers\" crates"}),
                || true
            ),
            ToolCall::LocalSearch(LocalSearch {
                tool: "grep".into(),
                query: "mcpServers".into()
            })
        );
        // Filtering command output is not code search.
        assert_eq!(
            classify(
                "run_command",
                &json!({"CommandLine": "ps aux | grep agy"}),
                || true
            ),
            ToolCall::Other
        );
        assert_eq!(
            classify("grep_search", &json!({"Query": "fn install_hooks"}), || {
                true
            }),
            ToolCall::LocalSearch(LocalSearch {
                tool: "grep_search".into(),
                query: "fn install_hooks".into()
            })
        );
        // Without an index there is nothing better to point at.
        assert_eq!(
            classify("grep_search", &json!({"Query": "x"}), || false),
            ToolCall::Other
        );
        assert_eq!(
            classify("read_file", &json!({"Path": "/x"}), || true),
            ToolCall::Other
        );
    }

    #[test]
    fn a_request_is_reminded_until_contextstream_grounds_it() {
        let mut conversation = Conversation::default();
        conversation.begin_request("r1", true);
        let first = conversation.next_guidance("/repo").expect("first reminder");
        assert!(first.contains("ToolName=\"init\""), "{first}");
        assert!(first.contains("\"folder_path\": \"/repo\""), "{first}");
        assert!(first.contains("ToolName=\"context\""), "{first}");

        // Ignored: asked again, up to the cap.
        assert!(conversation.next_guidance("/repo").is_some());
        assert!(conversation.next_guidance("/repo").is_some());
        assert_eq!(conversation.next_guidance("/repo"), None);

        // The next request starts over, and no longer asks for init once it ran.
        conversation.observe(ToolCall::ContextStream("init".into()));
        conversation.begin_request("r2", true);
        let next = conversation
            .next_guidance("/repo")
            .expect("reminder for r2");
        assert!(!next.contains("ToolName=\"init\""), "{next}");
        conversation.observe(ToolCall::ContextStream("context".into()));
        assert_eq!(conversation.next_guidance("/repo"), None);
    }

    #[test]
    fn a_local_search_before_contextstream_search_is_nudged_once_each() {
        let mut conversation = Conversation::default();
        conversation.begin_request("r1", true);
        conversation.observe(ToolCall::ContextStream("context".into()));
        assert_eq!(conversation.next_guidance("/repo"), None);

        let grep = LocalSearch {
            tool: "grep".into(),
            query: "mcp_config_path".into(),
        };
        conversation.observe(ToolCall::LocalSearch(grep.clone()));
        let nudge = conversation.next_guidance("/repo").expect("nudge");
        assert!(nudge.contains("ToolName=\"search\""), "{nudge}");
        assert!(nudge.contains("\"query\": \"mcp_config_path\""), "{nudge}");
        assert_eq!(conversation.next_guidance("/repo"), None, "shown once");

        conversation.observe(ToolCall::LocalSearch(grep.clone()));
        assert!(conversation.next_guidance("/repo").is_some());
        conversation.observe(ToolCall::LocalSearch(grep.clone()));
        assert_eq!(conversation.next_guidance("/repo"), None, "capped");

        // After a ContextStream search, local search is the agent's call.
        conversation.begin_request("r2", true);
        conversation.observe(ToolCall::ContextStream("search".into()));
        conversation.observe(ToolCall::LocalSearch(grep));
        assert_eq!(conversation.next_guidance("/repo"), None);
    }

    #[test]
    fn a_changed_request_resets_counts_without_a_first_invocation() {
        let mut conversation = Conversation::default();
        conversation.begin_request("r1", true);
        conversation.observe(ToolCall::ContextStream("search".into()));
        conversation.begin_request("r1", false);
        assert!(conversation.searched);
        conversation.begin_request("r2", false);
        assert!(!conversation.searched);
        assert!(!conversation.initialized);
    }

    #[test]
    fn messages_are_valid_for_any_workspace_path() {
        let guidance = session_guidance("/tmp/a \"quoted\" dir");
        assert!(
            guidance.contains(r#"{"folder_path": "/tmp/a \"quoted\" dir"}"#),
            "{guidance}"
        );
        let nudge = search_nudge(&LocalSearch {
            tool: "find".into(),
            query: String::new(),
        });
        assert!(nudge.contains("<what you are looking for>"), "{nudge}");
    }
}
