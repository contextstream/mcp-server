//! on-save-intent hook handler.
//!
//! Detects save/documentation intent and injects ContextStream persistence guidance.

use anyhow::Result;
use serde_json::Value;

use super::save_intent::guidance_for_input;
use super::{write_stdout_json, HookOutput};

enum EditorFormat {
    Claude,
    ClineLike,
    Cursor,
}

fn detect_editor(input: &Value) -> EditorFormat {
    if input.get("hookName").is_some() || input.get("workspaceRoots").is_some() {
        return EditorFormat::ClineLike;
    }

    if super::input_is_cursor(input) {
        return EditorFormat::Cursor;
    }

    EditorFormat::Claude
}

fn write_editor_output(editor: EditorFormat, guidance: Option<String>) -> Result<()> {
    match editor {
        EditorFormat::Claude => {
            if let Some(text) = guidance {
                write_stdout_json(&HookOutput::context(text))?;
            } else {
                write_stdout_json(&HookOutput::empty())?;
            }
        }
        EditorFormat::ClineLike => {
            let output = if let Some(text) = guidance {
                serde_json::json!({
                    "cancel": false,
                    "contextModification": text,
                })
            } else {
                serde_json::json!({ "cancel": false })
            };
            println!("{}", serde_json::to_string(&output)?);
        }
        EditorFormat::Cursor => {
            let output = if let Some(text) = guidance {
                serde_json::json!({
                    "continue": true,
                    "user_message": text,
                })
            } else {
                serde_json::json!({ "continue": true })
            };
            println!("{}", serde_json::to_string(&output)?);
        }
    }

    Ok(())
}

/// Whether `user-prompt-submit` emits its per-prompt context (and with it the
/// save guidance). Mirrors the switch in `user_prompt_submit::handle`.
fn prompt_reminder_enabled() -> bool {
    !std::env::var("CONTEXTSTREAM_REMINDER_ENABLED")
        .map(|value| value == "false")
        .unwrap_or(false)
}

/// Handle the on-save-intent hook.
pub async fn handle() -> Result<()> {
    // Check env var BEFORE reading stdin to avoid blocking in tests or when disabled.
    if std::env::var("CONTEXTSTREAM_SAVE_INTENT_ENABLED")
        .map(|v| v == "false")
        .unwrap_or(false)
    {
        write_stdout_json(&HookOutput::empty())?;
        return Ok(());
    }

    let input: Value =
        serde_json::from_reader(std::io::stdin().lock()).unwrap_or_else(|_| serde_json::json!({}));
    let editor = detect_editor(&input);

    // Claude Code and Codex run `user-prompt-submit` for the same event, and it
    // already appends this guidance. Older installs register both hooks, which
    // injected the guidance twice; stay silent unless reminders are disabled.
    if matches!(editor, EditorFormat::Claude) && prompt_reminder_enabled() {
        return Ok(());
    }

    // Only produce output when save intent detected — matches TypeScript behavior.
    // Writing nothing when no intent avoids unnecessary hook noise for Claude.
    if let Some(guidance) = guidance_for_input(&input) {
        write_editor_output(editor, Some(guidance))?;
    }
    Ok(())
}
