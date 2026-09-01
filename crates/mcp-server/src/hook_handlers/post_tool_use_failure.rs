//! PostToolUseFailure hook handler.
//!
//! Captures failed tool calls and records recurring failure patterns.

use anyhow::Result;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;

use super::common::{extract_cwd, load_config, post_memory_event};
use super::{read_stdin_json, write_stdout_json, HookOutput};

const FAILURE_COUNTERS_FILE: &str = ".contextstream/hook-failure-counts.json";
/// Cap machine-global counter cardinality (architectural review 2026-07-29:
/// 722 unique keys keyed by raw error text never converged).
const MAX_FAILURE_COUNTER_ENTRIES: usize = 200;

/// Handle the PostToolUseFailure hook.
pub async fn handle() -> Result<()> {
    let input = read_stdin_json()?;
    let cwd = extract_cwd(&input);
    let config = load_config(&cwd);

    let tool_name = input
        .get("tool_name")
        .or_else(|| input.get("toolName"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    let error_text = extract_error_text(&input);
    let tool_use_id = input
        .get("tool_use_id")
        .or_else(|| input.get("toolUseId"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let fingerprint = failure_fingerprint(tool_name, &error_text);
    let count = increment_failure_counter(&fingerprint, &error_text);

    if config.is_configured() {
        let content = serde_json::json!({
            "tool_name": tool_name,
            "tool_use_id": if tool_use_id.is_empty() { Value::Null } else { Value::String(tool_use_id.to_string()) },
            "error": error_text,
            "fingerprint": fingerprint,
            "occurrence_count": count,
            "tool_input": input.get("tool_input").cloned().unwrap_or_else(|| serde_json::json!({})),
            "timestamp": chrono::Utc::now().to_rfc3339(),
        });

        post_memory_event(
            &config,
            &format!("Tool failure: {}", tool_name),
            content,
            &["hook", "post_tool_use_failure", "tool_error"],
        )
        .await;

        // Intentionally NOT emitting an auto "Recurring failure lesson:" event.
        // Past behaviour promoted every 3rd repeat of the same tool error into a
        // lesson with a boilerplate prevention ("Add guardrails or alternate
        // fallback path for this failure mode."), which then fired in
        // LESSONS_WARNING on every future session. That trained assistants to
        // ignore the warning block. Recurring-tool-failure signal stays
        // observable via the `tool_error`-tagged event above and the in-flight
        // stdout guidance below; a real lesson should come from a human or
        // from deliberate post-mortem synthesis, not a counter hitting 3.
    }

    if count >= 2 {
        let guidance = format!(
            "The `{}` tool has failed {} times with a similar error in this workspace. Read the error details carefully, avoid repeating the same call unchanged, and switch to a narrower or fallback path if needed.",
            tool_name, count
        );
        write_stdout_json(&HookOutput::context(guidance))?;
    } else {
        write_stdout_json(&HookOutput::empty())?;
    }
    Ok(())
}

fn extract_error_text(input: &Value) -> String {
    input
        .get("error")
        .or_else(|| input.get("tool_error"))
        .or_else(|| input.get("stderr"))
        .and_then(|v| v.as_str())
        .map(String::from)
        .unwrap_or_else(|| "Tool execution failed".to_string())
}

/// Normalize to a low-cardinality class key. Absolute paths and volatile
/// numbers are scrubbed so the counter actually dedupes.
fn failure_fingerprint(tool_name: &str, error_text: &str) -> String {
    let tool = tool_name.to_lowercase();
    let class = failure_class(&tool, error_text);
    format!("{tool}:{class}")
}

fn failure_class(tool: &str, error_text: &str) -> String {
    let lower = error_text.to_lowercase();
    if lower.contains("token") && (lower.contains("exceed") || lower.contains("maximum")) {
        return "token_limit".into();
    }
    if lower.contains("no matches found") || lower.contains("nomatch") {
        return "no_match".into();
    }
    if lower.contains("permission") || lower.contains("denied") || lower.contains("not allowed") {
        return "permission".into();
    }
    if lower.contains("401") || lower.contains("unauthorized") {
        return "auth".into();
    }
    if lower.contains("404") || lower.contains("not found") {
        return "not_found".into();
    }
    if lower.contains("timeout") || lower.contains("timed out") {
        return "timeout".into();
    }
    if lower.contains("exit code") {
        // bash:exit_N without path/command body
        if let Some(code) = extract_exit_code(&lower) {
            return format!("exit_{code}");
        }
        return "exit".into();
    }
    // Fallback: tool + scrubbed first 8 tokens (no absolute paths).
    let scrubbed = scrub_paths(error_text);
    let compact = scrubbed
        .split_whitespace()
        .take(8)
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase();
    if compact.is_empty() {
        format!("{tool}_error")
    } else {
        compact
    }
}

fn extract_exit_code(lower: &str) -> Option<i32> {
    let marker = "exit code ";
    let idx = lower.find(marker)?;
    let rest = &lower[idx + marker.len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

fn scrub_paths(text: &str) -> String {
    text.split_whitespace()
        .map(|tok| {
            if tok.starts_with('/') || tok.contains(":\\") || tok.contains("\\\\") {
                "<path>"
            } else if tok.chars().any(|c| c.is_ascii_digit())
                && tok.chars().filter(|c| c.is_ascii_digit()).count() >= 4
            {
                // Drop long numeric runs (token counts, pids) that explode cardinality.
                "<n>"
            } else {
                tok
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
struct FailureCountersFile {
    /// class -> count
    #[serde(default)]
    counts: HashMap<String, u64>,
    /// class -> most recent raw example (bounded)
    #[serde(default)]
    last_example: HashMap<String, String>,
    /// class -> RFC3339 updated_at for LRU eviction
    #[serde(default)]
    updated_at: HashMap<String, String>,
}

fn increment_failure_counter(fingerprint: &str, raw_example: &str) -> u64 {
    let path = counters_path();

    let mut doc: FailureCountersFile = if let Ok(content) = std::fs::read_to_string(&path) {
        // Backward compatible: old shape was HashMap<String,u64>
        if let Ok(legacy) = serde_json::from_str::<HashMap<String, u64>>(&content) {
            FailureCountersFile {
                counts: legacy,
                ..Default::default()
            }
        } else {
            serde_json::from_str(&content).unwrap_or_default()
        }
    } else {
        FailureCountersFile::default()
    };

    let entry = doc.counts.entry(fingerprint.to_string()).or_insert(0);
    *entry += 1;
    let count = *entry;
    let example = truncate_utf8(raw_example, 240);
    doc.last_example
        .insert(fingerprint.to_string(), scrub_paths(&example));
    doc.updated_at
        .insert(fingerprint.to_string(), chrono::Utc::now().to_rfc3339());

    if doc.counts.len() > MAX_FAILURE_COUNTER_ENTRIES {
        prune_failure_counters(&mut doc, MAX_FAILURE_COUNTER_ENTRIES);
    }

    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string_pretty(&doc) {
        let _ = std::fs::write(&path, json);
    }

    count
}

fn truncate_utf8(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }

    let mut boundary = max_bytes.min(text.len());
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    format!("{}…", &text[..boundary])
}

fn prune_failure_counters(doc: &mut FailureCountersFile, cap: usize) {
    if doc.counts.len() <= cap {
        return;
    }
    let mut keys: Vec<(String, String)> = doc
        .counts
        .keys()
        .map(|k| {
            (
                k.clone(),
                doc.updated_at.get(k).cloned().unwrap_or_default(),
            )
        })
        .collect();
    // Oldest updated_at first (empty sorts first → evict unknown first).
    keys.sort_by(|a, b| a.1.cmp(&b.1));
    let drop_n = keys.len().saturating_sub(cap);
    for (k, _) in keys.into_iter().take(drop_n) {
        doc.counts.remove(&k);
        doc.last_example.remove(&k);
        doc.updated_at.remove(&k);
    }
}

fn counters_path() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        return PathBuf::from(home).join(FAILURE_COUNTERS_FILE);
    }
    PathBuf::from(FAILURE_COUNTERS_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_scrubs_paths_and_classes_token_limit() {
        let fp = failure_fingerprint(
            "Read",
            "File content (33762 tokens) exceeds maximum allowed tokens (25000). use offset",
        );
        assert_eq!(fp, "read:token_limit");
    }

    #[test]
    fn fingerprint_exit_code_without_path() {
        let fp = failure_fingerprint(
            "Bash",
            "Exit code 1 found: /home/alice/projects/example-app/src/app/api/route.ts",
        );
        assert_eq!(fp, "bash:exit_1");
    }

    #[test]
    fn long_unicode_failure_example_truncates_on_a_character_boundary() {
        let raw = format!("{}é{}", "a".repeat(239), "z".repeat(20));
        let truncated = truncate_utf8(&raw, 240);

        assert_eq!(truncated, format!("{}…", "a".repeat(239)));
        assert!(truncated.len() <= 242);
    }

    #[test]
    fn prune_caps_entries() {
        let mut doc = FailureCountersFile::default();
        for i in 0..10 {
            let k = format!("bash:exit_{i}");
            doc.counts.insert(k.clone(), 1);
            doc.updated_at.insert(k, format!("2026-07-2{i}T00:00:00Z"));
        }
        prune_failure_counters(&mut doc, 5);
        assert_eq!(doc.counts.len(), 5);
    }
}
