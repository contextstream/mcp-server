//! Hook handlers for `contextstream-mcp hook <name>`.
//!
//! Each handler reads JSON from stdin, processes it, and writes JSON to stdout.
//! These replace the previous bash/curl scripts with fast, native Rust handlers.

pub mod client_model_extractor;
pub mod common;
pub mod compliance;
pub mod dirty_drain;
pub mod git_bash_observed;
pub mod git_common;
pub mod git_post_checkout;
pub mod git_post_commit;
pub mod git_post_merge;
pub mod git_pre_push;
pub mod instructions_loaded;
pub mod notification;
pub mod on_save_intent;
pub mod permission_request;
pub mod post_compact;
pub mod post_tool_use;
pub mod post_tool_use_failure;
pub mod pre_compact;
pub mod pre_tool_use;
pub(crate) mod prompt_state;
pub(crate) mod protected;
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

use anyhow::Result;
use mcp_types::config::VERSION;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::OnceLock;

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
        Ok(value) => Ok(value),
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

/// Write hook output to stdout.
pub fn write_stdout_json(output: &HookOutput) -> Result<()> {
    let json = serde_json::to_string(output)?;
    println!("{}", json);
    Ok(())
}

/// Whether the hook input came from Cursor.
///
/// Cursor passes `hook_event_name` (snake_case) without Claude's `tool_name`
/// and without Cline/Roo/Kilo's camelCase `toolName`/`hookName`.
pub fn input_is_cursor(input: &Value) -> bool {
    let camel_case = input.get("hookName").is_some() || input.get("toolName").is_some();
    input.get("hook_event_name").is_some() && input.get("tool_name").is_none() && !camel_case
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

/// Dispatch a hook by name.
pub async fn dispatch_hook(name: &str) -> Result<()> {
    match name {
        "user-prompt-submit" => user_prompt_submit::handle().await,
        "pre-tool-use" => pre_tool_use::handle().await,
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
        "permission-request" => permission_request::handle().await,
        "pre-compact" => pre_compact::handle().await,
        "post-compact" => post_compact::handle().await,
        "on-save-intent" => on_save_intent::handle().await,
        // Additional lifecycle hooks currently used for observability and
        // compatibility with newer editor hook surfaces.
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
        build_api_http_client, current_hook_event_name, dispatch_hook, input_is_cursor,
        media_aware_guidance, HookOutput,
    };
    use mcp_types::config::VERSION;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn input_is_cursor_detects_cursor_and_rejects_others() {
        // Cursor: snake_case hook_event_name, no tool_name / camelCase markers.
        assert!(input_is_cursor(&serde_json::json!({
            "hook_event_name": "sessionStart"
        })));
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
}
