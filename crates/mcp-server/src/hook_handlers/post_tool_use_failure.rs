//! PostToolUseFailure hook handler.
//!
//! Captures failed tool calls and records recurring failure patterns.
//!
//! Repeats are counted per host session and error class in
//! `~/.contextstream/hook-failure-counts.json`, under that file's lock. Calls
//! that fail together within `FAILURE_BURST_MS` (a parallel batch) count as
//! one failure, and a class the session has not hit for
//! `FAILURE_IDLE_RESET_MS` is counted from one again.

use anyhow::Result;
use serde_json::Value;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use super::common::{extract_cwd, load_config, post_memory_event};
use super::{read_stdin_json, write_stdout_json, HookOutput};

const FAILURE_COUNTERS_FILE: &str = ".contextstream/hook-failure-counts.json";
/// Cap machine-global counter cardinality (architectural review 2026-07-29:
/// 722 unique keys keyed by raw error text never converged).
const MAX_FAILURE_COUNTER_ENTRIES: usize = 200;
/// Failures of one class closer together than this are one mistake: the
/// calls of a parallel batch fail within moments of each other.
const FAILURE_BURST_MS: u64 = 10_000;
/// A class a session has not failed with for this long counts from one again.
const FAILURE_IDLE_RESET_MS: u64 = 30 * 60 * 1000;

/// Handle the PostToolUseFailure hook.
pub async fn handle() -> Result<()> {
    let input = read_stdin_json()?;
    super::run_progress::tool_failed(&input);
    let cwd = extract_cwd(&input);
    let config = load_config(&cwd);

    let tool_name = input
        .get("tool_name")
        .or_else(|| input.get("toolName"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");

    // Account calls contain verification codes and personal data. Do not persist
    // their inputs, raw errors, or failure-counter samples.
    if super::common::is_account_setup_tool(tool_name) {
        write_stdout_json(&HookOutput::empty())?;
        return Ok(());
    }

    let error_text = extract_error_text(&input);
    let tool_use_id = input
        .get("tool_use_id")
        .or_else(|| input.get("toolUseId"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let fingerprint = failure_fingerprint(tool_name, &error_text);
    let count = increment_failure_counter(
        &failure_session_key(&input, &cwd),
        &fingerprint,
        &error_text,
        now_unix_ms(),
    );

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

    // ContextAgents: a failed install or CI check fits one; with the workspace's automatic
    // runs on, the call that starts it.
    let fitting = super::contextagents::failed_step_moments(
        tool_name,
        input.get("tool_input").unwrap_or(&Value::Null),
    );
    let contextagents = super::contextagents::step_hint(
        &input,
        &cwd,
        &fitting,
        super::contextagents::CLAUDE_AGENTS_TOOL,
    )
    .await;

    let remote_feedback = if tool_name.eq_ignore_ascii_case("bash") {
        let cmd = input
            .get("tool_input")
            .and_then(|v| v.get("command"))
            .and_then(Value::as_str)
            .unwrap_or("");
        crate::hosted_run::remote_run_hook_feedback(&input, cmd, &cwd)
    } else {
        None
    };

    let guidance = (count >= 2).then(|| format!(
        "The `{}` tool has failed {} times with a similar error in this session. Read the error details carefully, avoid repeating the same call unchanged, and switch to a narrower or fallback path if needed.",
        tool_name, count
    ));
    let mut context_parts = Vec::new();
    if let Some(ref g) = guidance {
        context_parts.push(g.clone());
    }
    if let Some(ref agents) = contextagents {
        context_parts.push(agents.clone());
    }
    let user_message = remote_feedback.as_ref().map(|(msg, _)| msg.clone());
    if let Some(ctx) = remote_feedback.and_then(|(_, ctx)| ctx) {
        context_parts.push(ctx);
    }

    let mut output = if context_parts.is_empty() {
        HookOutput::empty()
    } else {
        HookOutput::context(context_parts.join("\n\n"))
    };
    output.system_message = user_message;
    write_stdout_json(&output)?;
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

/// Who a failure belongs to: a short hash of the host session id, or of the
/// folder when the hook input names no session.
fn failure_session_key(input: &Value, cwd: &str) -> String {
    let owner = match super::common::host_session_key(input) {
        Some(session) => format!("session:{session}"),
        None => format!("cwd:{}", cwd.trim_end_matches(['/', '\\'])),
    };
    let mut hash = super::durable_capture::sha256_hex(owner.as_bytes());
    hash.truncate(16);
    hash
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Every map is keyed by `<session key>:<fingerprint>`. Files written before
/// keys named a session, or before `timing`, still read (every field has a
/// default); their entries carry no timing and are dropped on the next write.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
struct FailureCountersFile {
    /// key -> failures counted (a burst counts once)
    #[serde(default)]
    counts: HashMap<String, u64>,
    /// key -> most recent raw example (bounded)
    #[serde(default)]
    last_example: HashMap<String, String>,
    /// key -> RFC3339 updated_at for LRU eviction
    #[serde(default)]
    updated_at: HashMap<String, String>,
    /// key -> when its current burst started and when it last failed
    #[serde(default)]
    timing: HashMap<String, FailureTiming>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
struct FailureTiming {
    #[serde(default)]
    burst_started_unix_ms: u64,
    #[serde(default)]
    last_failure_unix_ms: u64,
}

/// Count a failure of `fingerprint` in `session` at `now_ms`, under the
/// counter file's lock; returns how many times it has failed. One when the
/// file cannot be written, which shows no repeat warning.
fn increment_failure_counter(
    session: &str,
    fingerprint: &str,
    raw_example: &str,
    now_ms: u64,
) -> u64 {
    let key = format!("{session}:{fingerprint}");
    super::durable_capture::with_locked_json(&counters_path(), |doc: &mut FailureCountersFile| {
        record_failure(doc, &key, raw_example, now_ms)
    })
    .unwrap_or(1)
}

/// Count one failure of `key` at `now_ms` and return its count. Within
/// `FAILURE_BURST_MS` of the start of the key's current burst the count stays;
/// after that a new burst starts and adds one; after `FAILURE_IDLE_RESET_MS`
/// without a failure the count starts over at one.
fn record_failure(doc: &mut FailureCountersFile, key: &str, raw_example: &str, now_ms: u64) -> u64 {
    forget_idle_counters(doc, now_ms);
    let previous = doc.counts.get(key).copied().unwrap_or(0);
    let (count, timing) = match doc.timing.get(key).copied() {
        Some(timing)
            if previous > 0
                && now_ms.saturating_sub(timing.burst_started_unix_ms) < FAILURE_BURST_MS =>
        {
            (
                previous,
                FailureTiming {
                    last_failure_unix_ms: now_ms.max(timing.last_failure_unix_ms),
                    ..timing
                },
            )
        }
        _ => (
            previous + 1,
            FailureTiming {
                burst_started_unix_ms: now_ms,
                last_failure_unix_ms: now_ms,
            },
        ),
    };
    doc.counts.insert(key.to_string(), count);
    doc.timing.insert(key.to_string(), timing);
    let example = truncate_utf8(raw_example, 240);
    doc.last_example
        .insert(key.to_string(), scrub_paths(&example));
    let updated_at = chrono::DateTime::from_timestamp_millis(now_ms as i64)
        .unwrap_or_else(chrono::Utc::now)
        .to_rfc3339();
    doc.updated_at.insert(key.to_string(), updated_at);

    if doc.counts.len() > MAX_FAILURE_COUNTER_ENTRIES {
        prune_failure_counters(doc, MAX_FAILURE_COUNTER_ENTRIES);
    }
    count
}

/// Drop counters idle for `FAILURE_IDLE_RESET_MS`, and those without timing
/// (written before counters were kept per session), which no key matches.
fn forget_idle_counters(doc: &mut FailureCountersFile, now_ms: u64) {
    let idle: Vec<String> = doc
        .counts
        .keys()
        .filter(|key| {
            doc.timing.get(*key).is_none_or(|timing| {
                now_ms.saturating_sub(timing.last_failure_unix_ms) >= FAILURE_IDLE_RESET_MS
            })
        })
        .cloned()
        .collect();
    for key in idle {
        remove_counter(doc, &key);
    }
    let FailureCountersFile {
        counts,
        last_example,
        updated_at,
        timing,
    } = doc;
    last_example.retain(|key, _| counts.contains_key(key));
    updated_at.retain(|key, _| counts.contains_key(key));
    timing.retain(|key, _| counts.contains_key(key));
}

fn remove_counter(doc: &mut FailureCountersFile, key: &str) {
    doc.counts.remove(key);
    doc.last_example.remove(key);
    doc.updated_at.remove(key);
    doc.timing.remove(key);
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
        remove_counter(doc, &k);
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

    const T0: u64 = 1_800_000_000_000;

    #[test]
    fn a_parallel_batch_counts_once_and_a_later_repeat_counts_again() {
        let mut doc = FailureCountersFile::default();
        let key = "aaaaaaaaaaaaaaaa:bash:exit_1";
        // One mistake repeated across a parallel batch: six failures 200 ms apart.
        for n in 0..6 {
            assert_eq!(
                record_failure(&mut doc, key, "Exit code 1", T0 + n * 200),
                1
            );
        }
        assert_eq!(
            record_failure(&mut doc, key, "Exit code 1", T0 + 30_000),
            2,
            "a repeat after the burst is a second failure"
        );
        assert_eq!(
            record_failure(
                &mut doc,
                "bbbbbbbbbbbbbbbb:bash:exit_1",
                "Exit code 1",
                T0 + 30_100
            ),
            1,
            "another session counts its own"
        );
        assert_eq!(
            record_failure(
                &mut doc,
                key,
                "Exit code 1",
                T0 + 30_000 + 2 * 60 * 60 * 1000
            ),
            1,
            "a session idle for two hours counts from one again"
        );
    }

    #[test]
    fn a_steady_trickle_of_failures_is_not_one_endless_burst() {
        let mut doc = FailureCountersFile::default();
        let key = "aaaaaaaaaaaaaaaa:read:token_limit";
        let counts: Vec<u64> = (0..5)
            .map(|n| record_failure(&mut doc, key, "too many tokens", T0 + n * 6_000))
            .collect();
        // Bursts start at 0 s, 12 s and 24 s.
        assert_eq!(counts, vec![1, 1, 2, 2, 3]);
    }

    #[test]
    fn counters_from_before_sessions_read_and_are_dropped() {
        let old = r#"{"counts":{"bash:exit_1":7},"last_example":{"bash:exit_1":"x"},"updated_at":{"bash:exit_1":"2026-07-20T00:00:00Z"}}"#;
        let mut doc: FailureCountersFile = serde_json::from_str(old).unwrap();
        assert!(doc.timing.is_empty());
        assert_eq!(
            record_failure(&mut doc, "aaaaaaaaaaaaaaaa:bash:exit_1", "Exit code 1", T0),
            1
        );
        assert!(!doc.counts.contains_key("bash:exit_1"));
        assert!(!doc.last_example.contains_key("bash:exit_1"));
        assert!(!doc.updated_at.contains_key("bash:exit_1"));

        // The oldest shape, a bare map of counts, reads as an empty file.
        let flat: FailureCountersFile = serde_json::from_str(r#"{"bash:exit_1":3}"#).unwrap();
        assert!(flat.counts.is_empty());
    }

    #[test]
    fn failures_are_keyed_by_host_session_or_folder() {
        let a = failure_session_key(&serde_json::json!({"session_id": "s-a"}), "/repo");
        let b = failure_session_key(&serde_json::json!({"session_id": "s-b"}), "/repo");
        let folder = failure_session_key(&serde_json::json!({}), "/repo/");
        assert_eq!(a.len(), 16);
        assert_ne!(a, b);
        assert_ne!(a, folder);
        assert_eq!(folder, failure_session_key(&serde_json::json!({}), "/repo"));
    }

    #[test]
    fn the_counter_file_is_written_under_its_lock() {
        let _guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = tempfile::tempdir().expect("temp home");
        let previous = std::env::var_os("HOME");
        std::env::set_var("HOME", home.path());

        assert_eq!(increment_failure_counter("s1", "bash:exit_1", "x", T0), 1);
        assert_eq!(
            increment_failure_counter("s1", "bash:exit_1", "x", T0 + 200),
            1
        );
        assert_eq!(
            increment_failure_counter("s1", "bash:exit_1", "x", T0 + 20_000),
            2
        );
        let written: FailureCountersFile =
            serde_json::from_slice(&std::fs::read(counters_path()).unwrap()).unwrap();
        assert_eq!(written.counts.get("s1:bash:exit_1"), Some(&2));
        assert!(home
            .path()
            .join(".contextstream/hook-failure-counts.lock")
            .exists());

        match previous {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }
}
