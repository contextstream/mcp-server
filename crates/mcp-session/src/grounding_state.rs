//! Cross-process file state for auto-grounding nudges in editor hooks.
//!
//! Written by `mcp-tools` when `context()` emits a `[GROUNDING]` block; read by
//! `mcp-server` `pre_tool_use` to optionally attach `[GROUNDING_AVAILABLE]`.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Default, Deserialize, Serialize)]
struct GroundingStateFile {
    #[serde(default)]
    workspaces: HashMap<String, GroundingEntry>,
}

#[derive(Debug, Clone, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct GroundingSummary {
    /// Number of prior-work hits last emitted in `[GROUNDING]`.
    #[serde(default)]
    pub hit_count: u32,
    /// Number of hits whose kind is decision-like.
    #[serde(default)]
    pub decision_count: u32,
    /// Number of time-sensitive hits marked stale by the renderer.
    #[serde(default)]
    pub stale_count: u32,
    /// Newest source timestamp across hits, if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newest_source_at: Option<String>,
    /// Oldest source timestamp across hits, if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oldest_source_at: Option<String>,
    /// Compact set of source kinds represented in the grounding block.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub top_kinds: Vec<String>,
}

impl GroundingSummary {
    pub fn from_hit_count(hit_count: u32) -> Self {
        Self {
            hit_count,
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
struct GroundingEntry {
    /// Number of prior-work hits last emitted in `[GROUNDING]` (0 = none / cleared).
    #[serde(default)]
    unread_hits: u32,
    /// Grounding-target tool calls since last `mark_grounding_emitted` without consume.
    #[serde(default)]
    skip_counter: u32,
    /// Freshness/source summary for hook nudges. Optional for old state files.
    #[serde(default)]
    summary: GroundingSummary,
    updated_at: String,
}

fn state_path() -> Option<PathBuf> {
    std::env::var_os("CONTEXTSTREAM_GROUNDING_STATE_FILE")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".contextstream").join("grounding-state.json")))
}

fn read_state() -> GroundingStateFile {
    let Some(path) = state_path() else {
        return GroundingStateFile::default();
    };
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default()
}

fn write_state(state: &GroundingStateFile) -> bool {
    let Some(path) = state_path() else {
        return false;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    serde_json::to_string_pretty(state)
        .ok()
        .and_then(|json| std::fs::write(&path, json).ok())
        .is_some()
}

fn workspace_paths_match(tracked_cwd: &str, cwd: &str) -> bool {
    cwd.starts_with(tracked_cwd) || tracked_cwd.starts_with(cwd)
}

fn entry_mut_for_cwd<'a>(
    workspaces: &'a mut HashMap<String, GroundingEntry>,
    cwd: &str,
) -> Option<&'a mut GroundingEntry> {
    if workspaces.contains_key(cwd) {
        return workspaces.get_mut(cwd);
    }
    let key = workspaces
        .keys()
        .find(|tracked| workspace_paths_match(tracked, cwd))
        .cloned()?;
    workspaces.get_mut(&key)
}

/// Record that `context()` emitted `hit_count` items in `[GROUNDING]` (0 clears).
pub fn mark_grounding_emitted(cwd: &str, hit_count: u32) {
    if cwd.trim().is_empty() {
        return;
    }
    let mut state = read_state();
    let now = chrono::Utc::now().to_rfc3339();
    let entry = state
        .workspaces
        .entry(cwd.to_string())
        .or_insert(GroundingEntry {
            unread_hits: 0,
            skip_counter: 0,
            summary: GroundingSummary::default(),
            updated_at: now.clone(),
        });
    entry.unread_hits = hit_count;
    entry.skip_counter = 0;
    entry.summary = GroundingSummary::from_hit_count(hit_count);
    entry.updated_at = now;
    write_state(&state);
}

/// Record that `context()` emitted a grounding block with freshness metadata.
pub fn mark_grounding_emitted_with_summary(cwd: &str, mut summary: GroundingSummary) {
    if cwd.trim().is_empty() {
        return;
    }
    let mut state = read_state();
    let now = chrono::Utc::now().to_rfc3339();
    if summary.hit_count == 0 {
        summary.decision_count = 0;
        summary.stale_count = 0;
        summary.newest_source_at = None;
        summary.oldest_source_at = None;
        summary.top_kinds.clear();
    }
    let entry = state
        .workspaces
        .entry(cwd.to_string())
        .or_insert(GroundingEntry {
            unread_hits: 0,
            skip_counter: 0,
            summary: GroundingSummary::default(),
            updated_at: now.clone(),
        });
    entry.unread_hits = summary.hit_count;
    entry.skip_counter = 0;
    entry.summary = summary;
    entry.updated_at = now;
    write_state(&state);
}

/// Clear unread grounding (user or agent consumed prior-work hints).
pub fn clear_grounding_consumed(cwd: &str) {
    if cwd.trim().is_empty() {
        return;
    }
    let mut state = read_state();
    if let Some(entry) = entry_mut_for_cwd(&mut state.workspaces, cwd) {
        entry.unread_hits = 0;
        entry.skip_counter = 0;
        entry.summary = GroundingSummary::default();
        entry.updated_at = chrono::Utc::now().to_rfc3339();
        write_state(&state);
    }
}

/// Returns hit count when a nudge may be shown (`Allow` → `AllowWithContext`).
pub fn peek_unread_hits(cwd: &str) -> Option<u32> {
    peek_unread_summary(cwd).map(|summary| summary.hit_count)
}

/// Returns grounding freshness summary when a nudge may be shown.
pub fn peek_unread_summary(cwd: &str) -> Option<GroundingSummary> {
    if cwd.trim().is_empty() {
        return None;
    }
    let state = read_state();
    let entry = state.workspaces.get(cwd).or_else(|| {
        state
            .workspaces
            .iter()
            .find_map(|(tracked, entry)| workspace_paths_match(tracked, cwd).then_some(entry))
    })?;
    if entry.unread_hits == 0 || entry.skip_counter >= 3 {
        return None;
    }
    let mut summary = entry.summary.clone();
    if summary.hit_count == 0 {
        summary.hit_count = entry.unread_hits;
    }
    Some(summary)
}

/// Count a grounding-target tool use toward auto-decay (3 strikes clears unread).
pub fn record_grounding_target_tool(cwd: &str) {
    if cwd.trim().is_empty() {
        return;
    }
    let mut state = read_state();
    let Some(entry) = entry_mut_for_cwd(&mut state.workspaces, cwd) else {
        return;
    };
    if entry.unread_hits == 0 {
        return;
    }
    entry.skip_counter = entry.skip_counter.saturating_add(1);
    if entry.skip_counter >= 3 {
        entry.unread_hits = 0;
        entry.skip_counter = 0;
        entry.summary = GroundingSummary::default();
    }
    entry.updated_at = chrono::Utc::now().to_rfc3339();
    write_state(&state);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_summary_and_migrates_old_count_only_state() {
        let path = std::env::temp_dir().join(format!(
            "contextstream-grounding-state-{}.json",
            uuid::Uuid::new_v4()
        ));
        std::env::set_var("CONTEXTSTREAM_GROUNDING_STATE_FILE", &path);
        let cwd = "/tmp/contextstream-summary-test";

        mark_grounding_emitted_with_summary(
            cwd,
            GroundingSummary {
                hit_count: 3,
                decision_count: 2,
                stale_count: 1,
                newest_source_at: Some("2026-05-24T00:00:00Z".to_string()),
                oldest_source_at: Some("2026-04-01T00:00:00Z".to_string()),
                top_kinds: vec!["decision".to_string(), "transcript".to_string()],
            },
        );

        let summary = peek_unread_summary(cwd).expect("summary should be unread");
        assert_eq!(summary.hit_count, 3);
        assert_eq!(summary.decision_count, 2);
        assert_eq!(summary.stale_count, 1);
        assert_eq!(summary.top_kinds, vec!["decision", "transcript"]);

        let old_state = serde_json::json!({
            "workspaces": {
                cwd: {
                    "unread_hits": 2,
                    "skip_counter": 0,
                    "updated_at": "2026-05-24T00:00:00Z"
                }
            }
        });
        std::fs::write(&path, serde_json::to_string(&old_state).unwrap()).unwrap();
        let migrated = peek_unread_summary(cwd).expect("old state should still nudge");
        assert_eq!(migrated.hit_count, 2);
        assert_eq!(migrated.stale_count, 0);

        let _ = std::fs::remove_file(path);
        std::env::remove_var("CONTEXTSTREAM_GROUNDING_STATE_FILE");
    }
}
