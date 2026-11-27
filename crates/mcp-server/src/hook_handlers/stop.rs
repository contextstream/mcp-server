//! Stop hook handler.
//!
//! Fires whenever Claude finishes responding. Captures a lightweight checkpoint
//! so recent progress is not lost between full SessionEnd events, and when
//! transcripts are enabled, appends the final assistant reply to the session's
//! transcript so saved sessions hold both prompts and replies.

use anyhow::Result;
use serde_json::Value;

use super::common::{
    extract_cwd, host_session_key, load_transcript_hook_config, post_memory_event, ApiConfig,
};
use super::transcript_capture::{append_assistant_reply, prepare_transcript_message};
use super::{read_stdin_json, write_stdout_json, HookOutput};

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
    prepare_transcript_message(reply, input)
}

#[cfg(test)]
fn cap_assistant_message(text: &str, max_bytes: usize) -> String {
    super::transcript_capture::cap_transcript_message(text, max_bytes)
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
