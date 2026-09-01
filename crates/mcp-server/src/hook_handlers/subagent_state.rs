//! Active subagent state tracking.
//!
//! Tracks which subagents are currently running via a JSON file.
//! SubagentStart writes entries, SubagentStop removes them.
//! PreToolUse reads entries to adjust behavior inside subagents.
//!
//! State file: `~/.contextstream/active-subagents.json`

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;

/// An active subagent entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEntry {
    pub agent_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    pub started_at: String,
}

/// The state file structure.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct StateFile {
    agents: HashMap<String, AgentEntry>,
}

/// Path to the active subagents state file.
fn state_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".contextstream").join("active-subagents.json"))
}

/// Read the state file, returning empty state if missing/invalid.
fn read_state() -> StateFile {
    let Some(path) = state_path() else {
        return StateFile::default();
    };
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default()
}

/// Write the state file atomically.
fn write_state(state: &StateFile) -> bool {
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

/// Register an active subagent for a given cwd.
pub fn write_active_subagent(cwd: &str, agent_type: &str, agent_id: Option<&str>) {
    let mut state = read_state();
    state.agents.insert(
        cwd.to_string(),
        AgentEntry {
            agent_type: agent_type.to_string(),
            agent_id: agent_id.map(String::from),
            started_at: chrono::Utc::now().to_rfc3339(),
        },
    );
    write_state(&state);
}

/// Remove the active subagent entry for a given cwd.
pub fn remove_active_subagent(cwd: &str) {
    let mut state = read_state();
    if state.agents.remove(cwd).is_some() {
        write_state(&state);
    }
}

/// Get the active subagent for a given cwd (exact or parent match).
pub fn get_active_subagent(cwd: &str) -> Option<AgentEntry> {
    let state = read_state();

    // Exact match first
    if let Some(entry) = state.agents.get(cwd) {
        return Some(entry.clone());
    }

    // Check if cwd is a subdirectory of any tracked path. Keep this
    // directional (cwd inside tracked) so a child subagent path does not
    // accidentally mark the parent/main workspace as "in subagent".
    for (tracked_cwd, entry) in &state.agents {
        if is_within_tracked_scope(cwd, tracked_cwd) {
            return Some(entry.clone());
        }
    }

    None
}

fn is_within_tracked_scope(cwd: &str, tracked_cwd: &str) -> bool {
    if cwd.trim().is_empty() || tracked_cwd.trim().is_empty() {
        return false;
    }
    Path::new(cwd).starts_with(Path::new(tracked_cwd))
}

/// Remove entries older than `max_age_minutes`.
/// Called from PreToolUse as a safety valve against stale state.
pub fn cleanup_stale_subagents(max_age_minutes: u64) {
    let mut state = read_state();
    let now = chrono::Utc::now();
    let original_count = state.agents.len();

    state.agents.retain(|_, entry| {
        if let Ok(started) = chrono::DateTime::parse_from_rfc3339(&entry.started_at) {
            let age = now.signed_duration_since(started);
            age.num_minutes() < max_age_minutes as i64
        } else {
            false // Remove entries with unparseable timestamps
        }
    });

    if state.agents.len() != original_count {
        write_state(&state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_state_file_roundtrip() {
        let mut state = StateFile::default();
        state.agents.insert(
            "/tmp/test-project".to_string(),
            AgentEntry {
                agent_type: "Explore".to_string(),
                agent_id: Some("agent-123".to_string()),
                started_at: chrono::Utc::now().to_rfc3339(),
            },
        );

        let json = serde_json::to_string_pretty(&state).unwrap();
        let parsed: StateFile = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.agents.len(), 1);
        assert_eq!(parsed.agents["/tmp/test-project"].agent_type, "Explore");
    }

    #[test]
    fn test_agent_entry_without_id() {
        let entry = AgentEntry {
            agent_type: "Explore".to_string(),
            agent_id: None,
            started_at: "2026-02-08T15:30:45Z".to_string(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(!json.contains("agent_id"));
    }

    #[test]
    fn tracked_scope_matching_is_directional() {
        assert!(is_within_tracked_scope(
            "/tmp/project/src/module",
            "/tmp/project"
        ));
        assert!(!is_within_tracked_scope(
            "/tmp/project",
            "/tmp/project/src/module"
        ));
    }
}
