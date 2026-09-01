//! Per-workspace prompt state tracking.
//!
//! Tracks whether a workspace requires an initial ContextStream `context(...)`
//! call for the current user prompt before other MCP tools execute.
//!
//! State file: `~/.contextstream/prompt-state.json`

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct PromptStateFile {
    workspaces: HashMap<String, PromptStateEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PromptStateEntry {
    require_context: bool,
    #[serde(default)]
    require_init: bool,
    #[serde(default)]
    last_context_at: Option<String>,
    #[serde(default)]
    last_state_change_at: Option<String>,
    #[serde(default)]
    index_wait_started_at: Option<String>,
    #[serde(default)]
    index_wait_until: Option<String>,
    updated_at: String,
}

fn state_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".contextstream").join("prompt-state.json"))
}

fn read_state() -> PromptStateFile {
    let Some(path) = state_path() else {
        return PromptStateFile::default();
    };
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|c| serde_json::from_str(&c).ok())
        .unwrap_or_default()
}

fn write_state(state: &PromptStateFile) -> bool {
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

fn parse_rfc3339_utc(value: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|ts| ts.with_timezone(&chrono::Utc))
}

fn entry_mut_for_cwd<'a>(
    workspaces: &'a mut HashMap<String, PromptStateEntry>,
    cwd: &str,
) -> Option<&'a mut PromptStateEntry> {
    if workspaces.contains_key(cwd) {
        return workspaces.get_mut(cwd);
    }
    let key = workspaces
        .keys()
        .find(|tracked_cwd| workspace_paths_match(tracked_cwd, cwd))
        .cloned()?;
    workspaces.get_mut(&key)
}

pub fn mark_context_required(cwd: &str) {
    if cwd.trim().is_empty() {
        return;
    }
    let mut state = read_state();
    let now = chrono::Utc::now().to_rfc3339();
    let entry = state
        .workspaces
        .entry(cwd.to_string())
        .or_insert(PromptStateEntry {
            require_context: false,
            require_init: false,
            last_context_at: None,
            last_state_change_at: None,
            index_wait_started_at: None,
            index_wait_until: None,
            updated_at: now.clone(),
        });
    entry.require_context = true;
    entry.updated_at = now;
    write_state(&state);
}

pub fn clear_context_required(cwd: &str) {
    if cwd.trim().is_empty() {
        return;
    }
    let mut state = read_state();
    if let Some(entry) = entry_mut_for_cwd(&mut state.workspaces, cwd) {
        entry.require_context = false;
        entry.last_context_at = Some(chrono::Utc::now().to_rfc3339());
        entry.index_wait_started_at = None;
        entry.index_wait_until = None;
        entry.updated_at = chrono::Utc::now().to_rfc3339();
        write_state(&state);
    }
}

pub fn mark_init_required(cwd: &str) {
    if cwd.trim().is_empty() {
        return;
    }
    let mut state = read_state();
    let now = chrono::Utc::now().to_rfc3339();
    let entry = state
        .workspaces
        .entry(cwd.to_string())
        .or_insert(PromptStateEntry {
            require_context: false,
            require_init: false,
            last_context_at: None,
            last_state_change_at: None,
            index_wait_started_at: None,
            index_wait_until: None,
            updated_at: now.clone(),
        });
    entry.require_init = true;
    entry.updated_at = now;
    write_state(&state);
}

pub fn clear_init_required(cwd: &str) {
    if cwd.trim().is_empty() {
        return;
    }
    let mut state = read_state();
    if let Some(entry) = entry_mut_for_cwd(&mut state.workspaces, cwd) {
        entry.require_init = false;
        entry.updated_at = chrono::Utc::now().to_rfc3339();
        write_state(&state);
    }
}

pub fn is_init_required(cwd: &str) -> bool {
    if cwd.trim().is_empty() {
        return false;
    }
    let state = read_state();

    if let Some(entry) = state.workspaces.get(cwd) {
        return entry.require_init;
    }

    for (tracked_cwd, entry) in &state.workspaces {
        if workspace_paths_match(tracked_cwd, cwd) {
            return entry.require_init;
        }
    }

    false
}

pub fn mark_state_changed(cwd: &str) {
    if cwd.trim().is_empty() {
        return;
    }
    let mut state = read_state();
    let now = chrono::Utc::now().to_rfc3339();
    let entry = state
        .workspaces
        .entry(cwd.to_string())
        .or_insert(PromptStateEntry {
            require_context: false,
            require_init: false,
            last_context_at: None,
            last_state_change_at: None,
            index_wait_started_at: None,
            index_wait_until: None,
            updated_at: now.clone(),
        });
    entry.last_state_change_at = Some(now.clone());
    entry.updated_at = now;
    write_state(&state);
}

#[allow(dead_code)]
pub fn is_context_fresh_and_clean(cwd: &str, max_age_seconds: u64) -> bool {
    if cwd.trim().is_empty() {
        return false;
    }
    let state = read_state();

    let entry = state.workspaces.get(cwd).or_else(|| {
        state.workspaces.iter().find_map(|(tracked_cwd, entry)| {
            workspace_paths_match(tracked_cwd, cwd).then_some(entry)
        })
    });

    let Some(entry) = entry else {
        return false;
    };

    let Some(last_context_at) = entry.last_context_at.as_deref() else {
        return false;
    };
    let Ok(last_context) = chrono::DateTime::parse_from_rfc3339(last_context_at) else {
        return false;
    };
    let age = chrono::Utc::now().signed_duration_since(last_context.with_timezone(&chrono::Utc));
    if age.num_seconds() < 0 || age.num_seconds() as u64 > max_age_seconds {
        return false;
    }

    if let Some(last_state_change_at) = entry.last_state_change_at.as_deref() {
        if let Ok(last_change) = chrono::DateTime::parse_from_rfc3339(last_state_change_at) {
            if last_change.with_timezone(&chrono::Utc) > last_context.with_timezone(&chrono::Utc) {
                return false;
            }
        }
    }

    true
}

pub fn is_context_required(cwd: &str) -> bool {
    if cwd.trim().is_empty() {
        return false;
    }
    let state = read_state();

    if let Some(entry) = state.workspaces.get(cwd) {
        return entry.require_context;
    }

    for (tracked_cwd, entry) in &state.workspaces {
        if workspace_paths_match(tracked_cwd, cwd) {
            return entry.require_context;
        }
    }

    false
}

pub fn start_index_wait_window(cwd: &str, wait_seconds: u64) {
    if cwd.trim().is_empty() || wait_seconds == 0 {
        return;
    }
    let mut state = read_state();
    let now = chrono::Utc::now();
    let now_iso = now.to_rfc3339();
    let wait_until_iso = (now + chrono::Duration::seconds(wait_seconds as i64)).to_rfc3339();
    let entry = state
        .workspaces
        .entry(cwd.to_string())
        .or_insert(PromptStateEntry {
            require_context: false,
            require_init: false,
            last_context_at: None,
            last_state_change_at: None,
            index_wait_started_at: None,
            index_wait_until: None,
            updated_at: now_iso.clone(),
        });

    if let Some(existing_until) = entry
        .index_wait_until
        .as_deref()
        .and_then(parse_rfc3339_utc)
    {
        if existing_until > now {
            entry.updated_at = now_iso;
            write_state(&state);
            return;
        }
    }

    entry.index_wait_started_at = Some(now_iso.clone());
    entry.index_wait_until = Some(wait_until_iso);
    entry.updated_at = now_iso;
    write_state(&state);
}

pub fn clear_index_wait_window(cwd: &str) {
    if cwd.trim().is_empty() {
        return;
    }
    let mut state = read_state();
    if let Some(entry) = entry_mut_for_cwd(&mut state.workspaces, cwd) {
        entry.index_wait_started_at = None;
        entry.index_wait_until = None;
        entry.updated_at = chrono::Utc::now().to_rfc3339();
        write_state(&state);
    }
}

pub fn index_wait_remaining_seconds(cwd: &str) -> Option<u64> {
    if cwd.trim().is_empty() {
        return None;
    }
    let state = read_state();
    let entry = state.workspaces.get(cwd).or_else(|| {
        state.workspaces.iter().find_map(|(tracked_cwd, entry)| {
            workspace_paths_match(tracked_cwd, cwd).then_some(entry)
        })
    })?;
    let until = parse_rfc3339_utc(entry.index_wait_until.as_deref()?)?;
    let now = chrono::Utc::now();
    let remaining = until.signed_duration_since(now).num_seconds();
    (remaining > 0).then_some(remaining as u64)
}

pub fn cleanup_stale(max_age_minutes: u64) {
    let mut state = read_state();
    let now = chrono::Utc::now();
    let original_count = state.workspaces.len();

    state.workspaces.retain(|_, entry| {
        if let Ok(updated) = chrono::DateTime::parse_from_rfc3339(&entry.updated_at) {
            let age = now.signed_duration_since(updated);
            age.num_minutes() < max_age_minutes as i64
        } else {
            false
        }
    });

    if state.workspaces.len() != original_count {
        let _ = write_state(&state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_state_roundtrip_shape_is_stable() {
        let mut state = PromptStateFile::default();
        state.workspaces.insert(
            "/tmp/project".to_string(),
            PromptStateEntry {
                require_context: true,
                require_init: false,
                last_context_at: Some(chrono::Utc::now().to_rfc3339()),
                last_state_change_at: None,
                index_wait_started_at: None,
                index_wait_until: None,
                updated_at: chrono::Utc::now().to_rfc3339(),
            },
        );
        let json = serde_json::to_string_pretty(&state).expect("serialize prompt state");
        let parsed: PromptStateFile = serde_json::from_str(&json).expect("parse prompt state");
        assert_eq!(parsed.workspaces.len(), 1);
        assert!(parsed.workspaces["/tmp/project"].require_context);
        assert!(!parsed.workspaces["/tmp/project"].require_init);
        assert!(parsed.workspaces["/tmp/project"].last_context_at.is_some());
        assert!(parsed.workspaces["/tmp/project"]
            .last_state_change_at
            .is_none());
        assert!(parsed.workspaces["/tmp/project"]
            .index_wait_started_at
            .is_none());
        assert!(parsed.workspaces["/tmp/project"].index_wait_until.is_none());
    }

    #[test]
    fn legacy_prompt_state_defaults_require_init_false() {
        let legacy = serde_json::json!({
            "workspaces": {
                "/tmp/project": {
                    "require_context": true,
                    "updated_at": chrono::Utc::now().to_rfc3339()
                }
            }
        });

        let parsed: PromptStateFile =
            serde_json::from_value(legacy).expect("parse legacy prompt state");
        assert!(parsed.workspaces["/tmp/project"].require_context);
        assert!(!parsed.workspaces["/tmp/project"].require_init);
        assert!(parsed.workspaces["/tmp/project"].last_context_at.is_none());
        assert!(parsed.workspaces["/tmp/project"]
            .last_state_change_at
            .is_none());
        assert!(parsed.workspaces["/tmp/project"]
            .index_wait_started_at
            .is_none());
        assert!(parsed.workspaces["/tmp/project"].index_wait_until.is_none());
    }

    #[test]
    fn workspace_paths_match_handles_parent_and_child_paths() {
        assert!(workspace_paths_match("/tmp/project", "/tmp/project"));
        assert!(workspace_paths_match(
            "/tmp/project",
            "/tmp/project/subdir/module"
        ));
        assert!(workspace_paths_match("/tmp/project/subdir", "/tmp/project"));
        assert!(!workspace_paths_match("/tmp/project-a", "/tmp/project-b"));
    }
}
