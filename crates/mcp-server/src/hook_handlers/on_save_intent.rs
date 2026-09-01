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

    if input.get("hook_event_name").is_some()
        && input.get("tool_name").is_none()
        && input.get("toolName").is_none()
    {
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

    // Only produce output when save intent detected — matches TypeScript behavior.
    // Writing nothing when no intent avoids unnecessary hook noise for Claude.
    if let Some(guidance) = guidance_for_input(&input) {
        write_editor_output(editor, Some(guidance))?;
    }
    Ok(())
}
