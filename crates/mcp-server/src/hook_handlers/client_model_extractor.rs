//! Per-editor model extractor for hook payloads.
//!
//! Each editor packs the active model into a different field in its hook
//! payload — none of them follow a shared spec. This module knows the layout
//! for each editor we support and runs the extracted value through the
//! shared [`mcp_model_registry`] for strict canonicalization.
//!
//! Resolution order inside a single hook payload:
//! 1. Top-level `model` / `model.id`.
//! 2. Editor-specific nested fields (Cursor `parameters.model`, Cline
//!    `apiConfiguration.modelId`, Windsurf `model_name`, etc.).
//! 3. Claude Code `transcript_path`: lazily parsed when no other field is
//!    populated; we look at the most recent `assistant` message and use its
//!    `model` field.
//!
//! Anything that the registry does not recognize is dropped — we never invent
//! canonical ids and never let a client/editor name leak through as a model.

use mcp_model_registry::{match_model, KnownModel};
use serde_json::Value;
use std::path::Path;

/// Try to extract a registry-validated canonical model id from a hook payload.
///
/// Returns `None` when no field on the payload (or its referenced transcript)
/// matches a curated model alias.
pub fn extract_model_from_hook(input: &Value, hook_event: &str) -> Option<String> {
    extract_known_model_from_hook(input, hook_event).map(|m| m.canonical_id.to_string())
}

/// Extended variant returning the matched [`KnownModel`] for callers that need
/// provider/family without re-querying the registry.
pub fn extract_known_model_from_hook(
    input: &Value,
    hook_event: &str,
) -> Option<&'static KnownModel> {
    // 1. Generic top-level fields used by Claude Code, Codex, ChatGPT gateway.
    if let Some(model) = first_model_string(
        input,
        &[
            "model",
            "model_id",
            "modelId",
            "active_model",
            "active_model_id",
        ],
    ) {
        if let Some(matched) = match_model(&model) {
            return Some(matched);
        }
    }

    // Top-level nested: { "model": { "id": "..." } }
    if let Some(matched) = nested_string(input, &["model", "id"]).and_then(|v| match_model(&v)) {
        return Some(matched);
    }
    if let Some(matched) = nested_string(input, &["model", "name"]).and_then(|v| match_model(&v)) {
        return Some(matched);
    }

    // 2. Cursor — `parameters.model`, `metadata.model`, `context.model`.
    let cursor_paths: &[&[&str]] = &[
        &["parameters", "model"],
        &["metadata", "model"],
        &["context", "model"],
    ];
    for path in cursor_paths {
        if let Some(matched) = nested_string(input, path).and_then(|v| match_model(&v)) {
            return Some(matched);
        }
    }

    // 3. Cline / Roo / Kilo — `apiConfiguration.modelId` (and a few sibling
    // shapes seen in the wild). Cline also nests this under `state`.
    let cline_paths: &[&[&str]] = &[
        &["apiConfiguration", "modelId"],
        &["apiConfiguration", "model"],
        &["state", "apiConfiguration", "modelId"],
        &["state", "apiConfiguration", "model"],
    ];
    for path in cline_paths {
        if let Some(matched) = nested_string(input, path).and_then(|v| match_model(&v)) {
            return Some(matched);
        }
    }

    // 4. Windsurf — `model_name`, also `tool_info.model_name`.
    if let Some(matched) =
        first_model_string(input, &["model_name", "modelName"]).and_then(|v| match_model(&v))
    {
        return Some(matched);
    }
    if let Some(matched) =
        nested_string(input, &["tool_info", "model_name"]).and_then(|v| match_model(&v))
    {
        return Some(matched);
    }

    // 5. ChatGPT gateway / OpenAI Responses — `request.model`,
    // `response.model`, `params.model`.
    let openai_paths: &[&[&str]] = &[
        &["request", "model"],
        &["response", "model"],
        &["params", "model"],
        &["request_payload", "model"],
    ];
    for path in openai_paths {
        if let Some(matched) = nested_string(input, path).and_then(|v| match_model(&v)) {
            return Some(matched);
        }
    }

    // 6. Claude Code transcript fallback (only if nothing else matched).
    if matches!(
        hook_event,
        "" | "SessionStart" | "UserPromptSubmit" | "PreToolUse" | "PostToolUse" | "Stop"
    ) {
        if let Some(transcript_path) = input
            .get("transcript_path")
            .or_else(|| input.get("transcriptPath"))
            .and_then(|v| v.as_str())
        {
            if let Some(matched) = read_model_from_claude_transcript(Path::new(transcript_path)) {
                return Some(matched);
            }
        }
    }

    None
}

fn first_model_string(input: &Value, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(value) = input.get(*key).and_then(|v| v.as_str()) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn nested_string(input: &Value, path: &[&str]) -> Option<String> {
    let mut current = input;
    for segment in path {
        current = current.get(*segment)?;
    }
    let value = current.as_str()?.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// Read the most recent `assistant` message model from a Claude Code JSONL
/// transcript. Returns `None` on any I/O or parse error.
///
/// Claude Code transcripts can grow large (multi-megabyte). This walks the
/// file once linearly and stops at the *last* assistant entry, so we don't
/// pay for a full re-parse on every hook invocation. We keep the parse
/// scoped to ~256 KB from the tail to bound work in pathological cases.
fn read_model_from_claude_transcript(path: &Path) -> Option<&'static KnownModel> {
    use std::fs::File;
    use std::io::{BufRead, BufReader, Seek, SeekFrom};

    if !path.exists() {
        return None;
    }
    let mut file = File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    const TAIL_LIMIT: u64 = 256 * 1024;
    let start = len.saturating_sub(TAIL_LIMIT);
    if start > 0 {
        file.seek(SeekFrom::Start(start)).ok()?;
        // Drop the (likely partial) first line.
        let mut throwaway = String::new();
        let mut buf = BufReader::new(&mut file);
        buf.read_line(&mut throwaway).ok();
    }

    let reader = BufReader::new(file);
    let mut last_match: Option<&'static KnownModel> = None;

    for line in reader.lines().map_while(Result::ok) {
        let trimmed = line.trim();
        if trimmed.is_empty() || !trimmed.starts_with('{') {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };

        // Claude Code transcript entries usually have `type: "assistant"` and
        // either `message.model` or top-level `model`.
        let role = value
            .get("type")
            .and_then(|v| v.as_str())
            .or_else(|| {
                value
                    .get("message")
                    .and_then(|v| v.get("role"))
                    .and_then(|v| v.as_str())
            })
            .unwrap_or("");

        if !matches!(role, "assistant" | "model") {
            continue;
        }

        let model_field = value
            .get("model")
            .and_then(|v| v.as_str())
            .or_else(|| {
                value
                    .get("message")
                    .and_then(|v| v.get("model"))
                    .and_then(|v| v.as_str())
            })
            .map(str::trim)
            .filter(|v| !v.is_empty());

        if let Some(raw) = model_field {
            if let Some(matched) = match_model(raw) {
                last_match = Some(matched);
            }
        }
    }

    last_match
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn extracts_top_level_model_from_claude_payload() {
        let payload = json!({ "model": "claude-opus-4.7-thinking-high" });
        assert_eq!(
            extract_model_from_hook(&payload, "PreToolUse").as_deref(),
            Some("claude-opus-4.7-thinking-high")
        );
    }

    #[test]
    fn extracts_nested_model_id_from_claude_payload() {
        let payload =
            json!({ "model": { "id": "claude-sonnet-4.5", "display_name": "Sonnet 4.5" } });
        assert_eq!(
            extract_model_from_hook(&payload, "PreToolUse").as_deref(),
            Some("claude-sonnet-4.5")
        );
    }

    #[test]
    fn extracts_cursor_parameters_model() {
        let payload = json!({
            "hook_event_name": "preToolUse",
            "parameters": { "model": "anthropic/claude-opus-4.5" }
        });
        assert_eq!(
            extract_model_from_hook(&payload, "preToolUse").as_deref(),
            Some("claude-opus-4.5")
        );
    }

    #[test]
    fn extracts_cline_api_configuration_model_id() {
        let payload = json!({
            "hookName": "preToolUse",
            "apiConfiguration": { "modelId": "openai/gpt-5-codex-high" }
        });
        assert_eq!(
            extract_model_from_hook(&payload, "preToolUse").as_deref(),
            Some("gpt-5-codex-high")
        );
    }

    #[test]
    fn extracts_cline_state_nested_model_id() {
        let payload = json!({
            "state": { "apiConfiguration": { "modelId": "gpt-5.4" } }
        });
        assert_eq!(
            extract_model_from_hook(&payload, "PreToolUse").as_deref(),
            Some("gpt-5.4-medium")
        );
    }

    #[test]
    fn extracts_windsurf_model_name() {
        let payload = json!({
            "hook_event_name": "pre_mcp_tool_use",
            "model_name": "claude-opus-4-7-thinking-high"
        });
        assert_eq!(
            extract_model_from_hook(&payload, "pre_mcp_tool_use").as_deref(),
            Some("claude-opus-4.7-thinking-high")
        );
    }

    #[test]
    fn unknown_model_returns_none_no_invention() {
        let payload = json!({ "model": "totally-made-up-model-v9000" });
        assert!(extract_model_from_hook(&payload, "PreToolUse").is_none());
    }

    #[test]
    fn ignores_client_name_field_even_when_registry_knows_value() {
        // The extractor only consults model-shaped fields (`model`,
        // `model_id`, `parameters.model`, etc.) — never `client_name`,
        // `client_info`, or other host hints. Even though the registry
        // recognizes `"claude-code"` (it canonicalizes to `claude` as a
        // host attribution), it must NOT be read out of `client_name` and
        // promoted to `model_id`. Editor name -> model leakage was the
        // original bug; recognizing host strings on the dashboard side is
        // separate from extracting them as a model on the wire.
        let payload = json!({ "client_name": "claude-code", "tool": "Read" });
        assert!(extract_model_from_hook(&payload, "PreToolUse").is_none());
    }

    #[test]
    fn reads_model_from_claude_transcript() {
        let mut file = NamedTempFile::new().expect("temp file");
        writeln!(
            file,
            r#"{{"type":"user","message":{{"role":"user","content":"hi"}}}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","model":"claude-opus-4-7-thinking-high","message":{{"role":"assistant","content":"hello"}}}}"#
        )
        .unwrap();
        let path = file.path();

        let payload = json!({ "transcript_path": path.to_string_lossy() });
        assert_eq!(
            extract_model_from_hook(&payload, "UserPromptSubmit").as_deref(),
            Some("claude-opus-4.7-thinking-high")
        );
    }

    #[test]
    fn transcript_fallback_returns_latest_known_assistant_model() {
        let mut file = NamedTempFile::new().expect("temp file");
        writeln!(
            file,
            r#"{{"type":"assistant","model":"unknown-old-model"}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","model":"claude-sonnet-4.5"}}"#
        )
        .unwrap();
        writeln!(
            file,
            r#"{{"type":"assistant","message":{{"role":"assistant","model":"gpt-5-codex-medium"}}}}"#
        )
        .unwrap();
        let path = file.path();

        let payload = json!({ "transcript_path": path.to_string_lossy() });
        assert_eq!(
            extract_model_from_hook(&payload, "UserPromptSubmit").as_deref(),
            Some("gpt-5-codex-medium")
        );
    }
}
