//! Hook handlers for `contextstream-mcp hook <name>`.
//!
//! Each handler reads JSON from stdin, processes it, and writes JSON to stdout.
//! These replace the previous bash/curl scripts with fast, native Rust handlers.

pub mod antigravity;
pub mod client_model_extractor;
pub mod coaching_turn;
pub mod command_rewrite;
pub mod common;
pub mod compliance;
pub(crate) mod contextagents;
pub(crate) mod credit_notice;
pub mod dirty_drain;
pub(crate) mod durable_capture;
pub(crate) mod durable_paths;
pub mod git_bash_observed;
pub mod git_common;
pub mod git_post_checkout;
pub mod git_post_commit;
pub mod git_post_merge;
pub mod git_pre_push;
pub mod instructions_loaded;
pub mod mid_run_notices;
pub mod notification;
pub mod on_save_intent;
pub mod package_bouncer;
pub mod permission_request;
pub mod post_compact;
pub mod post_tool_use;
pub mod post_tool_use_failure;
pub mod pre_compact;
pub mod pre_tool_use;
pub(crate) mod prompt_state;
mod resume_intent;
mod save_intent;
pub mod session_end;
pub mod session_start;
pub mod stop;
pub mod stop_failure;
pub mod subagent_start;
pub(crate) mod subagent_state;
pub mod subagent_stop;
pub mod task_completed;
pub mod task_created;
pub mod teammate_idle;
pub mod user_prompt_submit;
pub(crate) mod working_set;

use anyhow::Result;
use mcp_types::config::VERSION;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Mutex, OnceLock};

/// Nested output for newer Claude hook schema.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookSpecificOutput {
    /// Hook event name (e.g. "PreToolUse").
    #[serde(rename = "hookEventName", skip_serializing_if = "Option::is_none")]
    pub hook_event_name: Option<String>,

    /// Additional context to inject.
    #[serde(rename = "additionalContext", skip_serializing_if = "Option::is_none")]
    pub additional_context: Option<String>,

    /// Permission decision for PreToolUse.
    #[serde(rename = "permissionDecision", skip_serializing_if = "Option::is_none")]
    pub permission_decision: Option<String>,

    /// Reason paired with a PreToolUse permission decision.
    #[serde(
        rename = "permissionDecisionReason",
        skip_serializing_if = "Option::is_none"
    )]
    pub permission_decision_reason: Option<String>,
}

/// Hook output for Claude Code hooks.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookOutput {
    /// Newer Claude hook schema.
    #[serde(rename = "hookSpecificOutput", skip_serializing_if = "Option::is_none")]
    pub hook_specific_output: Option<HookSpecificOutput>,

    /// Legacy top-level context field retained only for hooks that still rely
    /// on the older schema or non-standard behavior.
    #[serde(rename = "additionalContext", skip_serializing_if = "Option::is_none")]
    pub additional_context: Option<String>,

    /// Top-level decision field used by block-capable Claude hooks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,

    /// Explanation paired with `decision: "block"`.
    #[serde(rename = "reason", skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,

    /// Universal stop control supported by all hook events.
    #[serde(rename = "continue", skip_serializing_if = "Option::is_none")]
    pub continue_processing: Option<bool>,

    /// User-facing stop message paired with `continue: false`.
    #[serde(rename = "stopReason", skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,

    /// Optional user-visible warning.
    #[serde(rename = "systemMessage", skip_serializing_if = "Option::is_none")]
    pub system_message: Option<String>,

    /// Hide hook output from verbose mode.
    #[serde(rename = "suppressOutput", skip_serializing_if = "Option::is_none")]
    pub suppress_output: Option<bool>,
}

impl HookOutput {
    pub fn context(text: String) -> Self {
        let event_name = current_hook_event_name();
        let use_hook_specific = event_name
            .as_deref()
            .map(supports_hook_specific_additional_context)
            .unwrap_or(false);

        let hook_specific_output = if use_hook_specific {
            event_name.map(|name| HookSpecificOutput {
                hook_event_name: Some(name),
                additional_context: Some(text.clone()),
                ..HookSpecificOutput::default()
            })
        } else {
            None
        };

        Self {
            hook_specific_output,
            additional_context: if use_hook_specific { None } else { Some(text) },
            decision: None,
            reason: None,
            continue_processing: None,
            stop_reason: None,
            system_message: None,
            suppress_output: None,
        }
    }

    pub fn block(reason: impl Into<String>) -> Self {
        Self {
            hook_specific_output: None,
            additional_context: None,
            decision: Some("block".to_string()),
            reason: Some(reason.into()),
            continue_processing: None,
            stop_reason: None,
            system_message: None,
            suppress_output: None,
        }
    }

    pub fn deny_pre_tool_use(reason: impl Into<String>) -> Self {
        let reason = reason.into();
        let event_name = current_hook_event_name().unwrap_or_else(|| "PreToolUse".to_string());
        Self {
            hook_specific_output: Some(HookSpecificOutput {
                hook_event_name: Some(event_name),
                additional_context: None,
                permission_decision: Some("deny".to_string()),
                permission_decision_reason: Some(reason),
            }),
            additional_context: None,
            decision: None,
            reason: None,
            continue_processing: None,
            stop_reason: None,
            system_message: None,
            suppress_output: None,
        }
    }

    pub fn stop_processing(stop_reason: impl Into<String>) -> Self {
        Self {
            hook_specific_output: None,
            additional_context: None,
            decision: None,
            reason: None,
            continue_processing: Some(false),
            stop_reason: Some(stop_reason.into()),
            system_message: None,
            suppress_output: None,
        }
    }

    pub fn system_message(message: impl Into<String>) -> Self {
        Self {
            hook_specific_output: None,
            additional_context: None,
            decision: None,
            reason: None,
            continue_processing: None,
            stop_reason: None,
            system_message: Some(message.into()),
            suppress_output: None,
        }
    }

    pub fn empty() -> Self {
        Self {
            hook_specific_output: None,
            additional_context: None,
            decision: None,
            reason: None,
            continue_processing: None,
            stop_reason: None,
            system_message: None,
            suppress_output: None,
        }
    }
}

/// Current Claude Code events that accept `hookSpecificOutput.additionalContext`.
fn supports_hook_specific_additional_context(event_name: &str) -> bool {
    matches!(
        event_name,
        "SessionStart"
            | "UserPromptSubmit"
            | "PreToolUse"
            | "PostToolUse"
            | "PostToolUseFailure"
            | "Notification"
            | "SubagentStart"
    )
}

fn current_hook_event_name() -> Option<String> {
    let raw = std::env::var("HOOK_EVENT_NAME")
        .ok()
        .or_else(|| std::env::var("HOOK_TYPE").ok())?
        .trim()
        .to_string();

    if raw.is_empty() {
        return None;
    }

    // If Claude already provides canonical event casing (e.g. SessionStart,
    // UserPromptSubmit), preserve it exactly.
    if raw.chars().all(|c| c.is_ascii_alphanumeric()) && raw.chars().any(|c| c.is_ascii_uppercase())
    {
        return Some(raw);
    }

    let normalized = raw
        .replace('-', "_")
        .to_ascii_lowercase()
        .split('_')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => {
                    let mut s = first.to_ascii_uppercase().to_string();
                    s.push_str(chars.as_str());
                    s
                }
                None => String::new(),
            }
        })
        .collect::<String>();

    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

/// Read hook input from stdin.
///
/// Returns an empty `{}` object on parse failure so downstream dispatch
/// doesn't crash mid-session, but logs the failure to stderr first so
/// operators tailing the hook log can spot misconfiguration. The
/// previous silent-swallow behavior hid real bugs (a malformed
/// `tool_input.command` in the host harness would silently bypass the
/// search-first redirect, for example).
///
/// It also notes which client sent the input (see [`note_hook_input`]), so
/// that what the handler writes back is what that client accepts.
pub fn read_stdin_json() -> Result<Value> {
    let stdin = std::io::stdin();
    // Read the whole body up front so we can include a snippet in the
    // diagnostic when parsing fails. Hooks always receive small
    // payloads (a few KB at most), so buffering is fine.
    let mut body = String::new();
    if let Err(err) = std::io::Read::read_to_string(&mut stdin.lock(), &mut body) {
        eprintln!("hook stdin read error: {err}; continuing with empty input");
        return Ok(serde_json::json!({}));
    }
    if body.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    match serde_json::from_str::<Value>(&body) {
        Ok(value) => {
            note_hook_input(&value);
            Ok(value)
        }
        Err(err) => {
            // Include a redacted preview so the operator can tell
            // which command/tool produced the malformed input. Cap
            // length so we don't dump huge bodies into the log.
            let preview: String = body.chars().take(160).collect();
            let event = std::env::var("HOOK_EVENT_NAME").unwrap_or_else(|_| "unknown".to_string());
            eprintln!(
                "hook stdin parse error in {event}: {err}; body[0..160]={preview:?}; continuing with empty input"
            );
            Ok(serde_json::json!({}))
        }
    }
}

/// The Codex event this hook process was started for, once its input showed the
/// client is Codex. A hook process serves one event, so one value is enough.
static CODEX_EVENT: Mutex<Option<String>> = Mutex::new(None);

/// Whether the hook input came from the OpenAI Codex CLI.
///
/// Every Codex hook event that can carry a model answer sends `turn_id` and
/// `model` together. Claude Code sends `turn_id` only to its display hook and
/// `model` only to `SessionStart`, so the pair does not occur there.
pub fn input_is_codex(input: &Value) -> bool {
    let present = |field: &str| {
        input
            .get(field)
            .and_then(Value::as_str)
            .is_some_and(|value| !value.trim().is_empty())
    };
    present("turn_id") && present("model")
}

/// Remember which client sent this hook's input; [`write_stdout_json`] shapes
/// its output for that client.
fn note_hook_input(input: &Value) {
    let event = input_is_codex(input).then(|| {
        input
            .get("hook_event_name")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(current_hook_event_name)
            .unwrap_or_default()
    });
    *CODEX_EVENT.lock().unwrap_or_else(|err| err.into_inner()) = event;
}

/// What Codex accepts in one event's output. Codex reads a hook's stdout into a
/// per-event type that refuses any field it does not list ("hook returned
/// invalid <Event> hook JSON output"), and refuses a few listed fields for some
/// events ("unsupported ..."). A hook that fails this way is shown to the person
/// as a failed hook on every run.
///
/// Source: Codex 0.163.0, `codex-rs/hooks` (`schema/generated/*.command.output
/// .schema.json` and `engine/output_parser.rs`).
#[derive(Clone, Copy)]
struct CodexContract {
    /// `hookSpecificOutput.additionalContext`.
    context: bool,
    /// `hookSpecificOutput.permissionDecision` (only `deny`; `allow` needs an
    /// `updatedInput` and `ask` is not supported).
    permission: bool,
    /// Top-level `decision: "block"` with `reason`.
    block: bool,
    /// `continue: false` and `stopReason`.
    stop: bool,
    /// `suppressOutput`.
    suppress: bool,
}

impl CodexContract {
    fn for_event(event: &str) -> Option<Self> {
        let (context, permission, block, stop, suppress) = match event {
            "SessionStart" | "SubagentStart" => (true, false, false, true, true),
            "UserPromptSubmit" => (true, false, true, true, true),
            "PreToolUse" => (true, true, true, false, false),
            "PostToolUse" => (true, false, true, true, false),
            "PermissionRequest" => (false, false, false, false, false),
            "Stop" | "SubagentStop" => (false, false, true, true, true),
            "PreCompact" | "PostCompact" => (false, false, false, true, true),
            // Codex has no output type for the rest (or no such event).
            _ => return None,
        };
        Some(Self {
            context,
            permission,
            block,
            stop,
            suppress,
        })
    }
}

/// What Codex can take from `output` for `event`: the fields it lists for that
/// event and nothing else. Text only the model could use goes where Codex has a
/// place for it (`hookSpecificOutput.additionalContext`) and is dropped where
/// it has none. A PreToolUse `ask` or `allow` cannot be expressed, so its
/// message goes to the model as context instead.
fn conform_for_codex(event: &str, mut output: HookOutput) -> HookOutput {
    let Some(contract) = CodexContract::for_event(event) else {
        return output;
    };
    let specific = output.hook_specific_output.take();
    let mut context = specific
        .as_ref()
        .and_then(|s| s.additional_context.clone())
        .or_else(|| output.additional_context.take());
    output.additional_context = None;
    let mut decision = specific
        .as_ref()
        .and_then(|s| s.permission_decision.clone());
    let mut reason = specific
        .as_ref()
        .and_then(|s| s.permission_decision_reason.clone());
    if contract.permission {
        let denied = decision.as_deref() == Some("deny")
            && reason.as_deref().is_some_and(|r| !r.trim().is_empty());
        if !denied {
            if let Some(message) = reason.take().filter(|r| !r.trim().is_empty()) {
                context = Some(match context {
                    Some(text) => format!("{text}\n\n{message}"),
                    None => message,
                });
            }
            decision = None;
        }
    } else {
        decision = None;
        reason = None;
    }
    if !contract.context {
        context = None;
    }
    output.hook_specific_output =
        (context.is_some() || decision.is_some()).then(|| HookSpecificOutput {
            hook_event_name: Some(event.to_string()),
            additional_context: context,
            permission_decision: decision,
            permission_decision_reason: reason,
        });
    if !contract.block {
        output.decision = None;
        output.reason = None;
    }
    if !contract.stop {
        output.continue_processing = None;
        output.stop_reason = None;
    }
    if !contract.suppress {
        output.suppress_output = None;
    }
    output
}

/// The JSON a hook writes for `output`: shaped for Codex when `codex_event` names
/// the Codex event the hook runs for, as built for everyone else otherwise.
fn render_hook_output(output: &HookOutput, codex_event: Option<&str>) -> Result<String> {
    match codex_event {
        Some(event) => Ok(serde_json::to_string(&conform_for_codex(
            event,
            output.clone(),
        ))?),
        None => Ok(serde_json::to_string(output)?),
    }
}

/// Write hook output to stdout.
pub fn write_stdout_json(output: &HookOutput) -> Result<()> {
    let codex_event = CODEX_EVENT
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .clone();
    let json = render_hook_output(output, codex_event.as_deref())?;
    println!("{}", json);
    Ok(())
}

/// Whether the hook input came from Cursor.
///
/// The field name alone is not distinctive: Claude and Codex also send
/// `hook_event_name`, and prompt/lifecycle events have no `tool_name`.
/// Cursor's event values use lower camel case; Claude/Codex use PascalCase.
pub fn input_is_cursor(input: &Value) -> bool {
    matches!(
        input.get("hook_event_name").and_then(Value::as_str),
        Some(
            "beforeSubmitPrompt"
                | "preToolUse"
                | "postToolUse"
                | "beforeMCPExecution"
                | "afterMCPExecution"
                | "beforeShellExecution"
                | "afterShellExecution"
                | "beforeReadFile"
                | "afterFileEdit"
                | "sessionStart"
                | "sessionEnd"
                | "preCompact"
                | "stop"
                | "subagentStart"
                | "subagentStop"
                | "taskCompleted"
                | "teammateIdle"
                | "notification"
                | "permissionRequest"
        )
    )
}

/// Emit context for the current editor.
///
/// Cursor's `sessionStart`/`postToolUse` hooks inject via a top-level snake_case
/// `additional_context` field. Claude and the other editors use [`HookOutput`].
pub fn write_context_for_input(input: &Value, text: String) -> Result<()> {
    if input_is_cursor(input) {
        let output = serde_json::json!({ "additional_context": text });
        println!("{}", serde_json::to_string(&output)?);
        Ok(())
    } else {
        write_stdout_json(&HookOutput::context(text))
    }
}

/// [`write_context_for_input`] plus a message for the person: Claude Code shows
/// a hook's `systemMessage` to the user directly; other editors get only the
/// context, which tells the assistant to relay it.
pub fn write_context_and_notice_for_input(
    input: &Value,
    text: String,
    user_message: Option<String>,
) -> Result<()> {
    if input_is_cursor(input) {
        return write_context_for_input(input, text);
    }
    let mut output = HookOutput::context(text);
    output.system_message = user_message;
    write_stdout_json(&output)
}

/// Shared HTTP client for hook handlers.
///
/// Reusing a single client preserves connection pools across hook invocations.
pub(crate) fn api_http_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(build_api_http_client)
}

fn build_api_http_client() -> reqwest::Client {
    let configured = std::env::var("CONTEXTSTREAM_USER_AGENT")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let fallback = format!("contextstream-mcp-rust/{}", VERSION);
    let user_agent = configured.unwrap_or_else(|| fallback.clone());

    reqwest::Client::builder()
        .user_agent(&user_agent)
        .build()
        .or_else(|_| reqwest::Client::builder().user_agent(fallback).build())
        .unwrap_or_else(|_| reqwest::Client::new())
}

/// Legacy hooks from older ContextStream installs that should be ignored safely.
fn handle_legacy_noop_hook() -> Result<()> {
    write_stdout_json(&HookOutput::empty())
}

fn media_aware_guidance() -> &'static str {
    "[CONTEXTSTREAM MEDIA] Media assets are first-class ContextStream context. Photos/images, videos, audio, and documents/PDFs should be inspected with the media tool, not code search or local text reads. Use media(action=\"list\") to list indexed assets, media(action=\"search\", query=\"...\", content_types=[\"image\"]) to search indexed assets/transcripts/OCR using image/video/audio/document as needed, media(action=\"index\", file_path=\"...\", content_type=\"image\") or external_url to make a local/URL asset readable, media(action=\"status\", content_id=\"...\") for processing, and media(action=\"get_clip\", content_id=\"...\", start=\"...\", end=\"...\", output_format=\"raw\") for clips (also supports ffmpeg/remotion). Friendly labels map as photos/images -> image and docs/PDFs/slides -> document."
}

fn handle_media_aware_hook() -> Result<()> {
    write_stdout_json(&HookOutput::context(media_aware_guidance().to_string()))
}

/// Whether editor lifecycle hooks are disabled via `CONTEXTSTREAM_HOOK_ENABLED`
/// (`false`, `0`, `off`, or `no`, case-insensitive).
pub fn editor_hooks_disabled() -> bool {
    std::env::var("CONTEXTSTREAM_HOOK_ENABLED")
        .ok()
        .is_some_and(|raw| {
            matches!(
                raw.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "no" | "off"
            )
        })
}

/// Dispatch a hook by name.
pub async fn dispatch_hook(name: &str) -> Result<()> {
    if editor_hooks_disabled() {
        match name {
            // Managed local Git hooks have their own dedicated kill-switch
            // (`CONTEXTSTREAM_GIT_CAPTURE=off` / `.contextstream/config.json`).
            "git-post-commit" | "git-pre-push" | "git-post-checkout" | "git-post-merge" => {}
            // Silent/detached editor hooks emit nothing on stdout.
            "codex-notify" | mid_run_notices::BACKGROUND_POLL_HOOK => return Ok(()),
            // Known JSON-stdout editor lifecycle hooks immediately no-op (`{}`).
            "user-prompt-submit"
            | "pre-tool-use"
            | "package-bouncer"
            | "post-tool-use"
            | "post-write"
            | "post-tool-use-failure"
            | "git-bash-observed"
            | "instructions-loaded"
            | "session-start"
            | "session-init"
            | "stop"
            | "stop-failure"
            | "session-end"
            | "subagent-start"
            | "subagent-stop"
            | "task-created"
            | "task-completed"
            | "teammate-idle"
            | "notification"
            | "agy-session-start"
            | "agy-pre-invocation"
            | "agy-post-tool-use"
            | "mid-run-notices"
            | "permission-request"
            | "pre-compact"
            | "post-compact"
            | "on-save-intent"
            | "config-change"
            | "cwd-changed"
            | "file-changed"
            | "worktree-create"
            | "worktree-remove"
            | "elicitation"
            | "elicitation-result"
            | "media-aware"
            | "on-read"
            | "on-bash"
            | "on-task"
            | "on-web"
            | "auto-rules" => return write_stdout_json(&HookOutput::empty()),
            _ => {}
        }
    }

    match name {
        "user-prompt-submit" => user_prompt_submit::handle().await,
        "pre-tool-use" => pre_tool_use::handle().await,
        // Opt-in PreToolUse on Bash: checks the packages an install command names.
        "package-bouncer" => package_bouncer::handle().await,
        "post-tool-use" | "post-write" => post_tool_use::handle().await,
        "post-tool-use-failure" => post_tool_use_failure::handle().await,
        // Local git capture: managed git hooks dispatch here.
        "git-post-commit" => git_post_commit::handle().await,
        "git-pre-push" => git_pre_push::handle().await,
        "git-post-checkout" => git_post_checkout::handle().await,
        "git-post-merge" => git_post_merge::handle().await,
        // Claude Code Bash PostToolUse: session tagging for git capture.
        "git-bash-observed" => git_bash_observed::handle().await,
        "instructions-loaded" => instructions_loaded::handle().await,
        "session-start" | "session-init" => session_start::handle().await,
        "stop" => stop::handle().await,
        "stop-failure" => stop_failure::handle().await,
        "session-end" => session_end::handle().await,
        "subagent-start" => subagent_start::handle().await,
        "subagent-stop" => subagent_stop::handle().await,
        "task-created" => task_created::handle().await,
        "task-completed" => task_completed::handle().await,
        "teammate-idle" => teammate_idle::handle().await,
        "notification" => notification::handle().await,
        // Codex `notify` program: hands finished turns to the coaching loop.
        "codex-notify" => coaching_turn::handle_codex_notify().await,
        // Antigravity (`agy`) lifecycle hooks (~/.gemini/config/hooks.json).
        "agy-session-start" => antigravity::handle_session_start().await,
        "agy-pre-invocation" => antigravity::handle_pre_invocation().await,
        "agy-post-tool-use" => antigravity::handle_post_tool_use().await,
        // Codex `PreToolUse`: mid-run coordination notices only.
        "mid-run-notices" => mid_run_notices::handle_notice_hook().await,
        // Detached mid-run notice poll started by a tool-call hook.
        mid_run_notices::BACKGROUND_POLL_HOOK => mid_run_notices::run_background_poll().await,
        "permission-request" => permission_request::handle().await,
        "pre-compact" => pre_compact::handle().await,
        "post-compact" => post_compact::handle().await,
        "on-save-intent" => on_save_intent::handle().await,
        // Lifecycle events earlier versions installed with no-op handlers.
        // `update-hooks` no longer installs them (and removes the managed
        // entries), but configs that still list them must keep working.
        "config-change" | "cwd-changed" | "file-changed" | "worktree-create"
        | "worktree-remove" | "elicitation" | "elicitation-result" => handle_legacy_noop_hook(),
        // Backward compatibility for legacy hook names still present in
        // older ~/.claude/settings.json configurations.
        "media-aware" => handle_media_aware_hook(),
        "on-read" | "on-bash" | "on-task" | "on-web" | "auto-rules" => handle_legacy_noop_hook(),
        _ => {
            eprintln!("Unknown hook: {}", name);
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    // These tests hold the process-wide env mutex across `.await` on purpose, to
    // keep env-var state stable while an async hook dispatch runs.
    #![allow(clippy::await_holding_lock)]
    use super::{
        build_api_http_client, current_hook_event_name, dispatch_hook, input_is_codex,
        input_is_cursor, media_aware_guidance, render_hook_output, HookOutput, HookSpecificOutput,
    };
    use mcp_types::config::VERSION;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn input_is_cursor_detects_cursor_and_rejects_others() {
        // Cursor is identified by the event, even when tool_name is present.
        assert!(input_is_cursor(&serde_json::json!({
            "hook_event_name": "sessionStart"
        })));
        assert!(input_is_cursor(&serde_json::json!({
            "hook_event_name": "preToolUse", "tool_name": "Read"
        })));
        for event in [
            "UserPromptSubmit",
            "SessionStart",
            "SubagentStart",
            "Stop",
            "pre_user_prompt",
        ] {
            assert!(
                !input_is_cursor(&serde_json::json!({
                    "hook_event_name": event
                })),
                "misclassified {event} as Cursor"
            );
        }
        // Claude uses tool_name.
        assert!(!input_is_cursor(&serde_json::json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Edit"
        })));
        // Cline/Roo/Kilo use camelCase.
        assert!(!input_is_cursor(
            &serde_json::json!({ "toolName": "read_file" })
        ));
        assert!(!input_is_cursor(&serde_json::json!({})));
    }

    fn with_hook_env(name: &str, f: impl FnOnce()) {
        // Serialize on the shared crate-wide env mutex so HOOK_EVENT_NAME /
        // CONTEXTSTREAM_* mutations here never race with other env-touching tests.
        let _guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let prev_hook_event_name = std::env::var("HOOK_EVENT_NAME").ok();
        let prev_hook_type = std::env::var("HOOK_TYPE").ok();

        std::env::set_var("HOOK_EVENT_NAME", name);
        std::env::remove_var("HOOK_TYPE");

        f();

        if let Some(value) = prev_hook_event_name {
            std::env::set_var("HOOK_EVENT_NAME", value);
        } else {
            std::env::remove_var("HOOK_EVENT_NAME");
        }

        if let Some(value) = prev_hook_type {
            std::env::set_var("HOOK_TYPE", value);
        } else {
            std::env::remove_var("HOOK_TYPE");
        }
    }

    async fn capture_user_agent(client: reqwest::Client) -> String {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local listener");
        let addr = listener.local_addr().expect("listener addr");

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept connection");
            let mut buf = vec![0u8; 4096];
            let n = socket.read(&mut buf).await.expect("read request");
            let request = String::from_utf8_lossy(&buf[..n]);
            let user_agent = request
                .lines()
                .find_map(|line| {
                    let mut parts = line.splitn(2, ':');
                    let name = parts.next()?.trim();
                    let value = parts.next()?.trim();
                    if name.eq_ignore_ascii_case("user-agent") {
                        Some(value.to_string())
                    } else {
                        None
                    }
                })
                .unwrap_or_default();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await
                .expect("write response");
            user_agent
        });

        client
            .get(format!("http://{}", addr))
            .send()
            .await
            .expect("request succeeds");

        server.await.expect("server task")
    }

    #[tokio::test]
    async fn legacy_hook_aliases_are_safe_noops() {
        let aliases = [
            "on-read",
            "on-bash",
            "on-task",
            "on-web",
            "auto-rules",
            "media-aware",
        ];

        for alias in aliases {
            assert!(
                dispatch_hook(alias).await.is_ok(),
                "alias failed: {}",
                alias
            );
        }
    }

    #[test]
    fn media_aware_hook_guidance_names_supported_asset_types() {
        let guidance = media_aware_guidance();
        assert!(guidance.contains("Photos/images") || guidance.contains("photos/images"));
        assert!(guidance.contains("documents/PDFs"));
        assert!(guidance.contains("media(action=\"search\""));
        assert!(guidance.contains("media(action=\"index\""));
        assert!(guidance.contains("docs/PDFs/slides -> document"));
    }

    #[tokio::test]
    async fn on_save_intent_hook_dispatches() {
        // Set env var so handler returns early without blocking on stdin.
        let _guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let prev = std::env::var("CONTEXTSTREAM_SAVE_INTENT_ENABLED").ok();
        std::env::set_var("CONTEXTSTREAM_SAVE_INTENT_ENABLED", "false");

        let result = dispatch_hook("on-save-intent").await;

        if let Some(value) = prev {
            std::env::set_var("CONTEXTSTREAM_SAVE_INTENT_ENABLED", value);
        } else {
            std::env::remove_var("CONTEXTSTREAM_SAVE_INTENT_ENABLED");
        }

        assert!(result.is_ok());
    }

    #[test]
    fn preserves_canonical_hook_event_name() {
        with_hook_env("SessionStart", || {
            assert_eq!(current_hook_event_name().as_deref(), Some("SessionStart"));
        });
    }

    #[test]
    fn normalizes_snake_case_hook_event_name() {
        with_hook_env("user_prompt_submit", || {
            assert_eq!(
                current_hook_event_name().as_deref(),
                Some("UserPromptSubmit")
            );
        });
    }

    #[test]
    fn hook_output_omits_hook_specific_output_for_unsupported_events() {
        for event in &[
            "PreCompact",
            "PostCompact",
            "SessionEnd",
            "Stop",
            "SubagentStop",
            "PermissionRequest",
            "TaskCreated",
            "TaskCompleted",
            "TeammateIdle",
            "InstructionsLoaded",
            "StopFailure",
        ] {
            with_hook_env(event, || {
                let output = HookOutput::context("test".to_string());
                let serialized = serde_json::to_value(output).expect("serialize hook output");
                assert!(
                    serialized.get("hookSpecificOutput").is_none(),
                    "{} should not include hookSpecificOutput",
                    event
                );
                assert_eq!(serialized["additionalContext"], serde_json::json!("test"));
            });
        }
    }

    #[test]
    fn hook_output_includes_hook_specific_output_for_supported_events() {
        for event in &[
            "SessionStart",
            "UserPromptSubmit",
            "PreToolUse",
            "PostToolUse",
            "PostToolUseFailure",
            "Notification",
            "SubagentStart",
        ] {
            with_hook_env(event, || {
                let output = HookOutput::context("test".to_string());
                let serialized = serde_json::to_value(output).expect("serialize hook output");
                assert!(
                    serialized.get("hookSpecificOutput").is_some(),
                    "{} should include hookSpecificOutput",
                    event
                );
                assert_eq!(
                    serialized["hookSpecificOutput"]["hookEventName"],
                    serde_json::json!(event)
                );
                assert!(serialized.get("additionalContext").is_none());
            });
        }
    }

    #[test]
    fn hook_output_pre_tool_use_denial_uses_permission_schema() {
        with_hook_env("PreToolUse", || {
            let output = HookOutput::deny_pre_tool_use("Blocked command");
            let serialized = serde_json::to_value(output).expect("serialize hook output");
            assert_eq!(
                serialized["hookSpecificOutput"]["permissionDecision"],
                serde_json::json!("deny")
            );
            assert_eq!(
                serialized["hookSpecificOutput"]["permissionDecisionReason"],
                serde_json::json!("Blocked command")
            );
            assert!(serialized.get("decision").is_none());
        });
    }

    #[test]
    fn hook_output_block_uses_top_level_decision() {
        let output = HookOutput::block("Need more work");
        let serialized = serde_json::to_value(output).expect("serialize hook output");
        assert_eq!(serialized["decision"], serde_json::json!("block"));
        assert_eq!(serialized["reason"], serde_json::json!("Need more work"));
    }

    #[test]
    fn hook_output_stop_processing_uses_universal_fields() {
        let output = HookOutput::stop_processing("Stop now");
        let serialized = serde_json::to_value(output).expect("serialize hook output");
        assert_eq!(serialized["continue"], serde_json::json!(false));
        assert_eq!(serialized["stopReason"], serde_json::json!("Stop now"));
    }

    #[tokio::test]
    async fn api_http_client_uses_default_user_agent_when_env_missing() {
        let _guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let prev = std::env::var("CONTEXTSTREAM_USER_AGENT").ok();
        std::env::remove_var("CONTEXTSTREAM_USER_AGENT");

        let observed = capture_user_agent(build_api_http_client()).await;

        if let Some(prev) = prev {
            std::env::set_var("CONTEXTSTREAM_USER_AGENT", prev);
        } else {
            std::env::remove_var("CONTEXTSTREAM_USER_AGENT");
        }

        assert_eq!(observed, format!("contextstream-mcp-rust/{}", VERSION));
    }

    #[tokio::test]
    async fn api_http_client_uses_configured_user_agent_when_present() {
        let _guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let prev = std::env::var("CONTEXTSTREAM_USER_AGENT").ok();
        std::env::set_var("CONTEXTSTREAM_USER_AGENT", "contextstream-mcp-rust/test");

        let observed = capture_user_agent(build_api_http_client()).await;

        if let Some(prev) = prev {
            std::env::set_var("CONTEXTSTREAM_USER_AGENT", prev);
        } else {
            std::env::remove_var("CONTEXTSTREAM_USER_AGENT");
        }

        assert_eq!(observed, "contextstream-mcp-rust/test");
    }

    #[test]
    fn editor_hooks_disabled_parses_falsey_env_values() {
        let _guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let prev = std::env::var("CONTEXTSTREAM_HOOK_ENABLED").ok();

        for disabled in ["false", "FALSE", "0", "off", "OFF", "no", " false "] {
            std::env::set_var("CONTEXTSTREAM_HOOK_ENABLED", disabled);
            assert!(super::editor_hooks_disabled(), "{disabled}");
        }
        for enabled in ["true", "1", "on", "yes", ""] {
            std::env::set_var("CONTEXTSTREAM_HOOK_ENABLED", enabled);
            assert!(!super::editor_hooks_disabled(), "{enabled}");
        }
        std::env::remove_var("CONTEXTSTREAM_HOOK_ENABLED");
        assert!(!super::editor_hooks_disabled());

        match prev {
            Some(v) => std::env::set_var("CONTEXTSTREAM_HOOK_ENABLED", v),
            None => std::env::remove_var("CONTEXTSTREAM_HOOK_ENABLED"),
        }
    }

    // ---- What Codex accepts, per event -------------------------------------
    //
    // Written from Codex 0.163.0's published hook output schemas
    // (`codex-rs/hooks/schema/generated/*.command.output.schema.json`, which its
    // serde types enforce with `deny_unknown_fields`) and `engine/output_parser.rs`
    // (the "unsupported ..." refusals). Independent of `CodexContract`, so the
    // two can disagree and fail a test.

    const CODEX_EVENTS: [&str; 10] = [
        "PermissionRequest",
        "PostCompact",
        "PostToolUse",
        "PreCompact",
        "PreToolUse",
        "SessionStart",
        "Stop",
        "SubagentStart",
        "SubagentStop",
        "UserPromptSubmit",
    ];

    /// Top-level fields in the event's Codex output type.
    fn codex_top_level(event: &str) -> &'static [&'static str] {
        match event {
            "PermissionRequest" => &[
                "continue",
                "hookSpecificOutput",
                "stopReason",
                "suppressOutput",
                "systemMessage",
            ],
            "PostCompact" | "PreCompact" => {
                &["continue", "stopReason", "suppressOutput", "systemMessage"]
            }
            "PostToolUse" | "PreToolUse" | "UserPromptSubmit" => &[
                "continue",
                "decision",
                "hookSpecificOutput",
                "reason",
                "stopReason",
                "suppressOutput",
                "systemMessage",
            ],
            "SessionStart" | "SubagentStart" => &[
                "continue",
                "hookSpecificOutput",
                "stopReason",
                "suppressOutput",
                "systemMessage",
            ],
            "Stop" | "SubagentStop" => &[
                "continue",
                "decision",
                "reason",
                "stopReason",
                "suppressOutput",
                "systemMessage",
            ],
            other => panic!("no Codex output type for {other}"),
        }
    }

    /// Fields of the event's `hookSpecificOutput` in Codex.
    fn codex_specific(event: &str) -> &'static [&'static str] {
        match event {
            "PermissionRequest" => &["hookEventName", "decision"],
            "PostToolUse" | "SessionStart" | "SubagentStart" | "UserPromptSubmit" => {
                &["hookEventName", "additionalContext"]
            }
            "PreToolUse" => &[
                "hookEventName",
                "additionalContext",
                "permissionDecision",
                "permissionDecisionReason",
                "updatedInput",
            ],
            _ => &[],
        }
    }

    /// `Err` with Codex's own wording when it would refuse `output` for `event`.
    fn codex_refuses(event: &str, output: &serde_json::Value) -> Result<(), String> {
        let object = output.as_object().expect("a JSON object");
        for key in object.keys() {
            if !codex_top_level(event).contains(&key.as_str()) {
                return Err(format!(
                    "hook returned invalid {event} hook JSON output: unknown field `{key}`"
                ));
            }
        }
        if let Some(specific) = object.get("hookSpecificOutput") {
            let specific = specific.as_object().expect("an object");
            for key in specific.keys() {
                if !codex_specific(event).contains(&key.as_str()) {
                    return Err(format!(
                        "hook returned invalid {event} hook JSON output: unknown field `{key}` in hookSpecificOutput"
                    ));
                }
            }
            if specific.get("hookEventName").and_then(|v| v.as_str()) != Some(event) {
                return Err(format!("{event}: hookEventName missing or different"));
            }
            match specific.get("permissionDecision").and_then(|v| v.as_str()) {
                Some("deny") => {
                    let reason = specific
                        .get("permissionDecisionReason")
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if reason.trim().is_empty() {
                        return Err("PreToolUse deny without a reason".to_string());
                    }
                }
                Some(other) => {
                    return Err(format!(
                        "PreToolUse hook returned unsupported permissionDecision:{other}"
                    ))
                }
                None => {
                    if specific.contains_key("permissionDecisionReason") {
                        return Err("permissionDecisionReason without permissionDecision".into());
                    }
                }
            }
        }
        let stops = output.get("continue") == Some(&serde_json::json!(false))
            || output.get("stopReason").is_some();
        let suppresses = output.get("suppressOutput").is_some();
        match event {
            "PreToolUse" | "PermissionRequest" if stops || suppresses => {
                return Err(format!(
                    "{event} hook returned unsupported continue/stopReason/suppressOutput"
                ))
            }
            "PostToolUse" if suppresses => {
                return Err("PostToolUse hook returned unsupported suppressOutput".to_string())
            }
            _ => {}
        }
        Ok(())
    }

    /// Everything a handler can build, for the event the process runs for.
    fn every_output(event: &str) -> Vec<(&'static str, HookOutput)> {
        let mut all = vec![
            ("empty", HookOutput::empty()),
            ("context", HookOutput::context("some context".to_string())),
            ("block", HookOutput::block("not now")),
            ("deny", HookOutput::deny_pre_tool_use("not allowed")),
            ("stop", HookOutput::stop_processing("stopped")),
            ("message", HookOutput::system_message("for the person")),
            (
                "legacy additionalContext",
                HookOutput {
                    additional_context: Some("legacy".to_string()),
                    ..HookOutput::empty()
                },
            ),
            (
                "everything at once",
                HookOutput {
                    hook_specific_output: Some(HookSpecificOutput {
                        hook_event_name: Some(event.to_string()),
                        additional_context: Some("ctx".to_string()),
                        permission_decision: Some("deny".to_string()),
                        permission_decision_reason: Some("why".to_string()),
                    }),
                    additional_context: Some("legacy".to_string()),
                    decision: Some("block".to_string()),
                    reason: Some("because".to_string()),
                    continue_processing: Some(false),
                    stop_reason: Some("stop".to_string()),
                    system_message: Some("note".to_string()),
                    suppress_output: Some(true),
                },
            ),
        ];
        for decision in ["ask", "allow"] {
            all.push((
                decision,
                HookOutput {
                    hook_specific_output: Some(HookSpecificOutput {
                        hook_event_name: Some(event.to_string()),
                        additional_context: None,
                        permission_decision: Some(decision.to_string()),
                        permission_decision_reason: Some("look at this first".to_string()),
                    }),
                    ..HookOutput::empty()
                },
            ));
        }
        all
    }

    #[test]
    fn codex_is_told_from_the_pair_of_turn_id_and_model() {
        // What Codex sends (PostCompact, from its input schema).
        assert!(input_is_codex(&serde_json::json!({
            "hook_event_name": "PostCompact", "session_id": "s", "turn_id": "t-1",
            "model": "gpt-5", "transcript_path": null, "cwd": "/w", "trigger": "auto"
        })));
        // What Claude Code sends: PostCompact has a summary and no turn or model,
        // SessionStart may name the model, and only its display hook has a turn.
        for claude in [
            serde_json::json!({"hook_event_name": "PostCompact", "session_id": "s",
                "trigger": "auto", "compact_summary": "..."}),
            serde_json::json!({"hook_event_name": "SessionStart", "source": "compact", "model": "m"}),
            serde_json::json!({"hook_event_name": "MessageDisplay", "turn_id": "t", "delta": "x"}),
            serde_json::json!({"hook_event_name": "PreToolUse", "tool_name": "Bash"}),
            serde_json::json!({"turn_id": "", "model": ""}),
            serde_json::json!({}),
        ] {
            assert!(!input_is_codex(&claude), "{claude}");
        }
    }

    #[test]
    fn nothing_we_can_build_is_refused_by_codex_in_any_event() {
        for event in CODEX_EVENTS {
            with_hook_env(event, || {
                for (what, output) in every_output(event) {
                    let json = render_hook_output(&output, Some(event)).unwrap();
                    let value: serde_json::Value = serde_json::from_str(&json).unwrap();
                    if let Err(refusal) = codex_refuses(event, &value) {
                        panic!("{event} / {what}: {refusal}\n{json}");
                    }
                }
            });
        }
    }

    #[test]
    fn the_checker_would_have_refused_the_old_postcompact_answer() {
        // The answer that Codex 0.163.0 reported as "hook returned invalid
        // PostCompact hook JSON output".
        let old = with_event("PostCompact", || {
            serde_json::to_string(&HookOutput::context("restored".to_string())).unwrap()
        });
        let value: serde_json::Value = serde_json::from_str(&old).unwrap();
        assert!(codex_refuses("PostCompact", &value).is_err());
        assert!(codex_refuses(
            "PreToolUse",
            &serde_json::json!({"hookSpecificOutput": {"hookEventName": "PreToolUse",
                "permissionDecision": "ask", "permissionDecisionReason": "x"}})
        )
        .is_err());
    }

    fn with_event<T>(event: &str, f: impl FnOnce() -> T) -> T {
        let mut out = None;
        with_hook_env(event, || out = Some(f()));
        out.unwrap()
    }

    #[test]
    fn a_codex_postcompact_hook_answers_with_nothing_to_refuse() {
        with_hook_env("PostCompact", || {
            let json = render_hook_output(
                &HookOutput::context("[POST-COMPACTION] restored".to_string()),
                Some("PostCompact"),
            )
            .unwrap();
            assert_eq!(json, "{}");
        });
    }

    #[test]
    fn claude_code_output_is_not_changed() {
        // Not Codex (`None`): the output is exactly what the handler built, in
        // every event, including the top-level context of PostCompact.
        for event in [
            "PostCompact",
            "PreCompact",
            "PreToolUse",
            "SessionStart",
            "Stop",
            "Notification",
            "PermissionRequest",
        ] {
            with_hook_env(event, || {
                for (what, output) in every_output(event) {
                    assert_eq!(
                        render_hook_output(&output, None).unwrap(),
                        serde_json::to_string(&output).unwrap(),
                        "{event} / {what}"
                    );
                }
            });
        }
        let post_compact = with_event("PostCompact", || {
            render_hook_output(&HookOutput::context("kept".to_string()), None).unwrap()
        });
        assert_eq!(post_compact, r#"{"additionalContext":"kept"}"#);
    }

    #[test]
    fn codex_keeps_what_its_events_take() {
        let render = |event: &str, output: HookOutput| {
            serde_json::from_str::<serde_json::Value>(
                &render_hook_output(&output, Some(event)).unwrap(),
            )
            .unwrap()
        };
        let context = |event: &str| {
            with_event(event, || {
                render(event, HookOutput::context("hello".to_string()))
            })
        };
        for event in ["SessionStart", "SubagentStart", "PreToolUse", "PostToolUse"] {
            assert_eq!(
                context(event),
                serde_json::json!({"hookSpecificOutput": {"hookEventName": event, "additionalContext": "hello"}}),
                "{event}"
            );
        }
        assert_eq!(
            render("PreToolUse", HookOutput::deny_pre_tool_use("no")),
            serde_json::json!({"hookSpecificOutput": {"hookEventName": "PreToolUse",
                "permissionDecision": "deny", "permissionDecisionReason": "no"}})
        );
        // A PreToolUse `ask` cannot be said to Codex; the message reaches the model.
        let ask = HookOutput {
            hook_specific_output: Some(HookSpecificOutput {
                hook_event_name: Some("PreToolUse".to_string()),
                additional_context: None,
                permission_decision: Some("ask".to_string()),
                permission_decision_reason: Some("a lookalike package".to_string()),
            }),
            ..HookOutput::empty()
        };
        assert_eq!(
            render("PreToolUse", ask),
            serde_json::json!({"hookSpecificOutput": {"hookEventName": "PreToolUse",
                "additionalContext": "a lookalike package"}})
        );
        assert_eq!(
            render("PermissionRequest", HookOutput::system_message("hi")),
            serde_json::json!({"systemMessage": "hi"})
        );
        assert_eq!(
            render("Stop", HookOutput::block("again")),
            serde_json::json!({"decision": "block", "reason": "again"})
        );
        // An event Codex has no output type for is left as built.
        assert_eq!(
            render("SessionEnd", HookOutput::empty()),
            serde_json::json!({})
        );
    }
}
