//! Stop hook handler.
//!
//! Fires whenever Claude finishes responding. Captures a lightweight checkpoint
//! so recent progress is not lost between full SessionEnd events, and when
//! transcripts are enabled, appends the final assistant reply to the session's
//! transcript so saved sessions hold both prompts and replies.

use anyhow::Result;
use serde_json::Value;
use std::time::Duration;

use super::common::{
    extract_cwd, extract_id, extract_items, host_session_key, load_transcript_hook_config,
    post_memory_event, prefix_within_bytes, scrub_credential_tokens, scrub_setup_secrets,
    transcript_has_account_setup, ApiConfig, TranscriptHookConfig,
};
use super::{read_stdin_json, write_stdout_json, HookOutput};

/// Maximum UTF-8 byte length kept for an assistant reply appended from Stop.
const MAX_ASSISTANT_MESSAGE_BYTES: usize = 16 * 1024;
const TRUNCATION_SUFFIX: &str = "...";
const TRANSCRIPT_HTTP_TIMEOUT: Duration = Duration::from_secs(3);

/// Handle the Stop hook.
pub async fn handle() -> Result<()> {
    if std::env::var("CONTEXTSTREAM_STOP_ENABLED")
        .map(|v| v == "false")
        .unwrap_or(false)
    {
        write_stdout_json(&HookOutput::empty())?;
        return Ok(());
    }

    let input = read_stdin_json()?;
    let cwd = extract_cwd(&input);
    let host_session = host_session_key(&input);
    let mut transcript_config =
        load_transcript_hook_config(&cwd, "CONTEXTSTREAM_STOP_SAVE_TRANSCRIPT", true);
    if transcript_config.workspace_id.is_none() || transcript_config.project_id.is_none() {
        if let Some((ws_id, proj_id)) = host_session
            .as_deref()
            .and_then(super::durable_capture::session_primary_scope)
        {
            if transcript_config.workspace_id.is_none() {
                transcript_config.workspace_id = Some(ws_id.to_string());
            }
            if transcript_config.project_id.is_none() {
                transcript_config.project_id = Some(proj_id.to_string());
            }
        }
    }
    let config = ApiConfig {
        api_key: transcript_config.api_key.clone(),
        api_url: transcript_config.api_url.clone(),
        workspace_id: transcript_config.workspace_id.clone(),
        project_id: transcript_config.project_id.clone(),
    };
    // The turn is over: this session's runs wait on a person.
    super::run_progress::turn_ended(&input, &cwd);

    if config.is_configured() {
        let session_id = input
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        let reason = input
            .get("reason")
            .or_else(|| input.get("stop_reason"))
            .and_then(|v| v.as_str())
            .unwrap_or("response_complete");
        let last_assistant_message = input
            .get("last_assistant_message")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty());

        let content = serde_json::json!({
            "session_id": session_id,
            "reason": reason,
            "hook": "stop",
            "timestamp": chrono::Utc::now().to_rfc3339(),
            "tool_name": input.get("tool_name").and_then(|v| v.as_str()),
            "model": input.get("model").and_then(|v| v.as_str()),
            "stop_hook_active": input.get("stop_hook_active").and_then(|v| v.as_bool()),
            "last_assistant_message": last_assistant_message,
        });

        let save_transcript = input
            .get("transcripts_enabled")
            .and_then(Value::as_bool)
            .or_else(|| input.get("save_transcript").and_then(Value::as_bool))
            .unwrap_or(transcript_config.save_transcript_default);

        let checkpoint_fut = post_memory_event(
            &config,
            "Stop checkpoint",
            content,
            &["hook", "stop", "checkpoint"],
        );
        let transcript_fut = async {
            if !save_transcript {
                return;
            }
            let Some(reply) = last_assistant_message else {
                return;
            };
            let Some(prepared) = prepare_assistant_reply(reply, &input) else {
                return;
            };
            append_assistant_reply(&transcript_config, &input, &prepared).await;
        };

        tokio::join!(checkpoint_fut, transcript_fut);

        // Keep the reply for the coaching loop; the next prompt hook sends it
        // with the person's answer (see `coaching_turn`).
        if super::coaching_turn::enabled() {
            if let (Some(session), Some(reply)) = (host_session, last_assistant_message) {
                super::coaching_turn::remember_reply(&session, reply);
            }
        }
    }

    // Stop hooks are side-effect only.
    write_stdout_json(&HookOutput::empty())?;
    Ok(())
}

/// Scrub secrets and cap an assistant reply before appending it to a transcript.
fn prepare_assistant_reply(reply: &str, input: &Value) -> Option<String> {
    let trimmed = reply.trim();
    if trimmed.is_empty() {
        return None;
    }

    let setup_flow = input
        .get("transcript_path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .and_then(|path| std::fs::read_to_string(path).ok())
        .is_some_and(|raw| transcript_has_account_setup(&raw));

    let scrubbed = if setup_flow {
        scrub_setup_secrets(trimmed)
    } else {
        scrub_credential_tokens(trimmed)
    };
    let scrubbed = scrubbed.trim();
    if scrubbed.is_empty() {
        return None;
    }

    Some(cap_assistant_message(scrubbed, MAX_ASSISTANT_MESSAGE_BYTES))
}

/// Bound `text` to at most `max_bytes` UTF-8 bytes, cutting on a character
/// boundary and appending `...` when truncated.
fn cap_assistant_message(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    if max_bytes <= TRUNCATION_SUFFIX.len() {
        return prefix_within_bytes(text, max_bytes).to_string();
    }
    let prefix = prefix_within_bytes(text, max_bytes - TRUNCATION_SUFFIX.len());
    format!("{prefix}{TRUNCATION_SUFFIX}")
}

/// Session IDs to query for this conversation's transcript, preferring the
/// ContextStream API session ID recorded when `init` ran over the host's own
/// session ID.
fn candidate_session_ids(input: &Value) -> Vec<String> {
    let mut ids = Vec::new();
    if let Some(host_session) = host_session_key(input) {
        if let Some(api_session) = super::durable_capture::session_api_session_id(&host_session) {
            if api_session != "unknown" {
                ids.push(api_session);
            }
        }
        if host_session != "unknown" && !ids.iter().any(|existing| existing == &host_session) {
            ids.push(host_session);
        }
    }
    ids
}

/// Best-effort append of the assistant reply to the session's existing
/// transcript via `GET /api/v1/transcripts` + `POST /api/v1/transcripts/:id/messages`.
/// Network and API errors are ignored so the Stop hook never fails.
async fn append_assistant_reply(config: &TranscriptHookConfig, input: &Value, content: &str) {
    let candidates = candidate_session_ids(input);
    if candidates.is_empty() {
        return;
    }

    let mut transcript_id = None;
    for session_id in &candidates {
        if let Some(id) = find_session_transcript_id(config, session_id).await {
            transcript_id = Some(id);
            break;
        }
    }

    let Some(transcript_id) = transcript_id else {
        return;
    };

    append_transcript_message(config, &transcript_id, content).await;
}

async fn find_session_transcript_id(
    config: &TranscriptHookConfig,
    session_id: &str,
) -> Option<String> {
    let mut query = vec![("session_id", session_id), ("per_page", "1")];
    if let Some(ref ws_id) = config.workspace_id {
        query.push(("workspace_id", ws_id.as_str()));
    }
    if let Some(ref proj_id) = config.project_id {
        query.push(("project_id", proj_id.as_str()));
    }

    let response = super::api_http_client()
        .get(format!("{}/api/v1/transcripts", config.api_url))
        .header("X-API-Key", &config.api_key)
        .query(&query)
        .timeout(TRANSCRIPT_HTTP_TIMEOUT)
        .send()
        .await
        .ok()?;

    if !response.status().is_success() {
        return None;
    }

    let value: Value = response.json().await.ok()?;
    let items = extract_items(&value);
    items
        .first()
        .and_then(extract_id)
        .and_then(|id| super::durable_capture::plain_session_id(&id))
}

async fn append_transcript_message(
    config: &TranscriptHookConfig,
    transcript_id: &str,
    content: &str,
) {
    let payload = serde_json::json!({
        "role": "assistant",
        "content": content,
        "timestamp": chrono::Utc::now().to_rfc3339(),
    });

    let _ = super::api_http_client()
        .post(format!(
            "{}/api/v1/transcripts/{}/messages",
            config.api_url, transcript_id
        ))
        .header("X-API-Key", &config.api_key)
        .header("Content-Type", "application/json")
        .json(&payload)
        .timeout(TRANSCRIPT_HTTP_TIMEOUT)
        .send()
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    const MULTI_BYTE_CHARS: [&str; 3] = ["\u{1F600}", "\u{4E16}", "\u{E9}"];

    #[test]
    fn cap_assistant_message_keeps_short_text_and_bounds_long_utf8() {
        assert_eq!(cap_assistant_message("short reply", 100), "short reply");
        assert_eq!(cap_assistant_message("exact", 5), "exact");

        for ch in MULTI_BYTE_CHARS {
            let repeated = ch.repeat(20);
            for limit in 4..20 {
                let capped = cap_assistant_message(&repeated, limit);
                assert!(capped.len() <= limit, "limit {limit} for {ch:?}");
                assert!(capped.ends_with("..."), "limit {limit} for {ch:?}");
            }
        }
    }

    #[test]
    fn prepare_assistant_reply_scrubs_credentials_and_setup_flows() {
        let normal = prepare_assistant_reply(
            "  token cbiq_live_123 and cbst_setup_456 for #123456  ",
            &serde_json::json!({}),
        )
        .expect("prepared normal reply");
        assert_eq!(
            normal,
            "token [redacted-credential] and [redacted-credential] for #123456"
        );

        let dir = tempfile::tempdir().unwrap();
        let transcript = dir.path().join("transcript.jsonl");
        std::fs::write(
            &transcript,
            format!(
                "{}\n",
                serde_json::json!({"type": "tool_use", "name": "mcp__contextstream__account"})
            ),
        )
        .unwrap();
        let setup = prepare_assistant_reply(
            "Code 654321 phone +15555550123 device ABCD-EFGH key cbiq_abc",
            &serde_json::json!({"transcript_path": transcript}),
        )
        .expect("prepared setup reply");
        assert!(!setup.contains("654321"));
        assert!(!setup.contains("+15555550123"));
        assert!(!setup.contains("ABCD-EFGH"));
        assert!(!setup.contains("cbiq_abc"));
        assert!(setup.contains("[code]"));
        assert!(setup.contains("[phone]"));
        assert!(setup.contains("[device-code]"));
        assert!(setup.contains("[redacted-credential]"));
    }

    #[test]
    fn prepare_assistant_reply_skips_blank_text() {
        assert_eq!(
            prepare_assistant_reply("   \n\t ", &serde_json::json!({})),
            None
        );
    }
}
