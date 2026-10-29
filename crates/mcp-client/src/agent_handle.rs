//! An agent's handle: the name other agents address it by in a coordination
//! share (`For cc-qa: …`, or `to: "cc-qa"`). Hooks and MCP check-ins derive it
//! the same way, so a share reaches the agent whichever of them registered
//! its session.

use std::path::Path;

/// A raw handle in its canonical form: `agent-cc-qa.Cz5v` and `Agent-CC-QA`
/// both become `cc-qa`. `None` for an empty or implausible one.
pub fn normalize_agent_handle(raw: &str) -> Option<String> {
    let mut token = raw
        .trim()
        .trim_matches(|c: char| matches!(c, '"' | '\'' | '`' | ':' | ',' | ';'))
        .trim();
    if token.is_empty() {
        return None;
    }
    if let Some((stem, suffix)) = token.rsplit_once('.') {
        if stem.to_ascii_lowercase().starts_with("agent-")
            && (3..=10).contains(&suffix.len())
            && suffix.chars().all(|c| c.is_ascii_alphanumeric())
        {
            token = stem;
        }
    }
    let lowered = token.to_ascii_lowercase();
    let stripped = lowered.strip_prefix("agent-").unwrap_or(&lowered);
    let candidate = if !stripped.is_empty() {
        stripped
    } else {
        lowered.as_str()
    };
    (candidate.len() <= 64
        && candidate
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
    .then(|| candidate.to_string())
}

/// This process's handle: `CONTEXTSTREAM_AGENT_NAME` (or `AGENT_NAME`), else
/// a working folder named `agent-<handle>` (one scratch folder per agent).
pub fn agent_handle(folder: Option<&str>) -> Option<String> {
    let env = ["CONTEXTSTREAM_AGENT_NAME", "AGENT_NAME"]
        .into_iter()
        .find_map(|key| {
            std::env::var(key)
                .ok()
                .filter(|value| !value.trim().is_empty())
        });
    agent_handle_from(env.as_deref(), folder)
}

fn agent_handle_from(env: Option<&str>, folder: Option<&str>) -> Option<String> {
    if let Some(handle) = env.and_then(normalize_agent_handle) {
        return Some(handle);
    }
    let base = Path::new(folder?.trim_end_matches('/'))
        .file_name()?
        .to_str()?;
    if !base.to_ascii_lowercase().starts_with("agent-") {
        return None;
    }
    normalize_agent_handle(base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_handle_comes_from_the_environment_or_an_agent_folder() {
        assert_eq!(
            normalize_agent_handle("agent-cc-qa.Cz5v").as_deref(),
            Some("cc-qa")
        );
        assert_eq!(normalize_agent_handle("QA-B").as_deref(), Some("qa-b"));
        assert_eq!(
            agent_handle_from(Some("qa-b"), Some("/tmp/agent-qa-a")).as_deref(),
            Some("qa-b")
        );
        assert_eq!(
            agent_handle_from(None, Some("/tmp/x/agent-mcp-eng2c/")).as_deref(),
            Some("mcp-eng2c")
        );
        assert_eq!(agent_handle_from(None, Some("/home/dev/project")), None);
        assert_eq!(agent_handle_from(None, None), None);
    }
}
