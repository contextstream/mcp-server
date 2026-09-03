//! Canonical `[NOTICE]` line templates shared by the tool renderers.
//!
//! Every notice that names a tool call is rendered from here so a single
//! test can prove that each referenced tool actually exists on the MCP
//! surface (see `notice_tool_names` and the registry conformance test in
//! `crates/mcp-server/tests/integration_tests.rs`). Terminal commands such as
//! `contextstream-mcp update` are deliberately not tool calls and are written
//! without parentheses so the extractor never mistakes them for one.

/// Terminal command that refreshes installed rules, hooks, and MCP configs.
pub const RULES_REFRESH_COMMAND: &str = "contextstream-mcp update";

/// Terminal command that installs rules for a folder that has none yet.
pub const RULES_INSTALL_COMMAND: &str =
    "contextstream-mcp setup --yes --editors <editor> --project-path <folder>";

/// Tool call that previews the rules content without touching disk.
pub const RULES_PREVIEW_CALL: &str = "help(action=\"editor_rules\")";

/// Header emitted once above rendered lessons in verbose mode.
pub const LESSONS_WARNING_HEADER: &str =
    "[LESSONS_WARNING] Apply these lessons before proceeding; keep them active for this task.";

/// `[RULES_NOTICE]` for a folder with no installed rules.
pub fn rules_notice_missing(update_command: Option<&str>) -> String {
    format!(
        "[RULES_NOTICE] Rules missing. Run `{}` in a terminal to install them; {} previews the content without writing files.",
        update_command
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(RULES_INSTALL_COMMAND),
        RULES_PREVIEW_CALL
    )
}

/// `[RULES_NOTICE]` for installed rules that are behind the bundled version.
pub fn rules_notice_behind(current: &str, latest: &str, update_command: Option<&str>) -> String {
    format!(
        "[RULES_NOTICE] Rules {} → {}. Run `{}` in a terminal to refresh installed rules; {} previews the content.",
        current,
        latest,
        update_command
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(RULES_REFRESH_COMMAND),
        RULES_PREVIEW_CALL
    )
}

/// `[RULES_NOTICE]` for a content-hash drift between the installed file and
/// the binary's bundled teaching.
pub fn rules_notice_drift(installed_hash: &str, canonical_hash: &str) -> String {
    format!(
        "[RULES_NOTICE] Rules content drifted ({} → {}). Run `{}` in a terminal to refresh installed rules; {} previews the content.",
        installed_hash, canonical_hash, RULES_REFRESH_COMMAND, RULES_PREVIEW_CALL
    )
}

/// One `[COORDINATION]` line. `other_project` prefixes the reason with
/// `[other project]`; the line always ends with the manual ack call and is
/// never acked automatically.
pub fn coordination_notice_line(
    reason: &str,
    notice_id: &str,
    other_project: bool,
    urgency: Option<&str>,
) -> String {
    let mut line = String::from("[COORDINATION] ");
    if other_project {
        line.push_str("[other project] ");
    }
    line.push_str(reason);
    if let Some(urgency) = urgency
        .map(str::trim)
        .filter(|value| !value.is_empty() && !value.eq_ignore_ascii_case("normal"))
    {
        line.push_str(&format!(" (urgency={urgency})"));
    }
    line.push_str(&format!(
        " — ack via coordination(action=\"ack\", notice_id=\"{notice_id}\")"
    ));
    line
}

/// Truncation trailer for coordination notices that did not fit.
pub fn coordination_more_line(remaining: usize) -> String {
    format!(
        "[COORDINATION] … {remaining} more — coordination(action=\"inbox\") lists them; ack each one explicitly."
    )
}

/// Header for typed suggested-rule lines.
pub const SUGGESTED_RULES_HEADER: &str = "[SUGGESTED_RULES] ContextStream detected patterns and suggests new rules. Present these to the user and let them accept/reject via session(action=\"suggested_rule_action\", rule_id=\"...\", rule_action=\"accept|reject\").";

/// Hint appended to `[DECISION_TRACE]` output.
pub const DECISION_TRACE_HINT: &str = "Refresh a specific decision with memory(action=\"decisions\", query=\"...\") or act on it with memory(action=\"decision_action\", decision_id=\"...\", decision_action=\"verify|dispute|supersede|invalidate|choose_successor\").";

/// Every static notice template plus one rendered example of each dynamic
/// notice, for the tool-name conformance test.
pub fn notice_templates() -> Vec<String> {
    vec![
        rules_notice_missing(None),
        rules_notice_behind("1.0.0", "1.0.1", None),
        rules_notice_drift("abcd1234", "ef567890"),
        coordination_notice_line("API contract changed", "n1", true, Some("high")),
        coordination_more_line(3),
        LESSONS_WARNING_HEADER.to_string(),
        SUGGESTED_RULES_HEADER.to_string(),
        DECISION_TRACE_HINT.to_string(),
        RULES_PREVIEW_CALL.to_string(),
    ]
}

/// Extract every `tool_name(` reference from a notice. Only identifiers
/// immediately followed by `(` count, so terminal commands and prose never
/// register as tool calls.
pub fn notice_tool_names(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut names = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let ch = bytes[index] as char;
        if ch.is_ascii_lowercase() || ch == '_' {
            let start = index;
            while index < bytes.len()
                && ((bytes[index] as char).is_ascii_lowercase()
                    || (bytes[index] as char).is_ascii_digit()
                    || bytes[index] == b'_')
            {
                index += 1;
            }
            let preceded_by_word = start > 0
                && ((bytes[start - 1] as char).is_ascii_alphanumeric()
                    || bytes[start - 1] == b'_'
                    || bytes[start - 1] == b'-'
                    || bytes[start - 1] == b'.');
            if index < bytes.len() && bytes[index] == b'(' && !preceded_by_word {
                let name = &text[start..index];
                if !names.iter().any(|existing| existing == name) {
                    names.push(name.to_string());
                }
            }
        } else {
            index += 1;
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_name_extraction_ignores_terminal_commands() {
        let names = notice_tool_names(&rules_notice_behind("1", "2", None));
        assert_eq!(names, vec!["help".to_string()]);
        let names = notice_tool_names(&coordination_notice_line("r", "id", false, None));
        assert_eq!(names, vec!["coordination".to_string()]);
        assert!(notice_tool_names("run contextstream-mcp update now").is_empty());
        // A method call is not a tool call: `foo` is not followed by `(`,
        // and `bar` is preceded by a word character.
        assert!(notice_tool_names("foo.bar(1)").is_empty());
        assert_eq!(
            notice_tool_names("memory(action=\"decisions\")"),
            vec!["memory".to_string()]
        );
    }

    #[test]
    fn rules_notice_never_names_a_phantom_tool() {
        for template in notice_templates() {
            assert!(
                !template.contains("generate_rules("),
                "phantom tool referenced: {template}"
            );
        }
        assert!(rules_notice_missing(Some("contextstream-mcp setup --yes"))
            .contains("contextstream-mcp setup --yes"));
    }

    #[test]
    fn coordination_line_marks_other_projects_and_urgency() {
        let line = coordination_notice_line("Shared decision", "n1", true, Some("high"));
        assert!(line.starts_with("[COORDINATION] [other project] Shared decision (urgency=high)"));
        assert!(line.ends_with("notice_id=\"n1\")"));
        let plain = coordination_notice_line("Shared decision", "n2", false, Some("normal"));
        assert!(!plain.contains("[other project]"));
        assert!(!plain.contains("urgency"));
    }
}
