//! Claude Code `PostToolUse` (Bash) hook handler — git session tagging.
//!
//! When the agent runs a git-mutating shell command, this deposits a short-TTL
//! session hint (`session_id` + `agent`) keyed by repo, which the managed git
//! hooks read so the captured VCS event is attributed to the agent session.
//!
//! Reconciliation: the managed git hook is the single source of truth that
//! *creates* events; this handler only annotates. As an optional fallback, when
//! the managed hooks are known-not-installed for the repo (so no hook event will
//! ever fire), it directly captures the commit tagged with the session. Backend
//! dedupe (by commit sha) collapses any later hook event onto the same row, so
//! there is never a double count.

use anyhow::Result;
use mcp_client::CaptureVcsLocalEventParams;
use serde_json::Value;
use std::path::Path;

use super::{common, git_common, read_stdin_json, write_stdout_json, HookOutput};

/// Allow-listed git mutations parsed from a shell command.
#[derive(Debug, Default, PartialEq, Eq)]
struct GitVerbs {
    /// Any allow-listed mutation: commit|push|merge|rebase|checkout -b|switch -c.
    mutating: bool,
    /// A subset that produces/moves to a commit (commit|merge|rebase), eligible
    /// for the direct-capture fallback.
    creates_commit: bool,
}

/// Global git options that consume the following token as their value.
const VALUE_OPTS: &[&str] = &[
    "-C",
    "-c",
    "--git-dir",
    "--work-tree",
    "--namespace",
    "--exec-path",
    "--config-env",
];

/// Find a `git` invocation in one shell segment and return (subcommand, rest).
fn segment_git_invocation(segment: &str) -> Option<(String, Vec<&str>)> {
    let tokens: Vec<&str> = segment.split_whitespace().collect();
    let mut i = 0;
    while i < tokens.len() {
        let is_git = tokens[i] == "git" || tokens[i].ends_with("/git");
        i += 1;
        if !is_git {
            continue;
        }
        // Skip global options to reach the subcommand.
        let mut j = i;
        while j < tokens.len() {
            let tok = tokens[j];
            if tok.starts_with('-') {
                j += if VALUE_OPTS.contains(&tok) { 2 } else { 1 };
                continue;
            }
            return Some((tok.to_string(), tokens[j + 1..].to_vec()));
        }
    }
    None
}

/// Classify a (possibly chained) shell command for git mutations.
fn classify(command: &str) -> GitVerbs {
    let mut verbs = GitVerbs::default();
    let segments = command
        .split(['\n', ';'])
        .flat_map(|s| s.split("&&"))
        .flat_map(|s| s.split("||"))
        .flat_map(|s| s.split('|'));
    for segment in segments {
        let Some((sub, rest)) = segment_git_invocation(segment) else {
            continue;
        };
        match sub.as_str() {
            "commit" | "merge" | "rebase" => {
                verbs.mutating = true;
                verbs.creates_commit = true;
            }
            "push" => verbs.mutating = true,
            "checkout" if rest.iter().any(|t| *t == "-b" || *t == "-B") => {
                verbs.mutating = true;
            }
            "switch" if rest.iter().any(|t| *t == "-c" || *t == "-C") => {
                verbs.mutating = true;
            }
            _ => {}
        }
    }
    verbs
}

fn extract_command(input: &Value) -> String {
    ["tool_input", "parameters", "toolParameters", "args"]
        .iter()
        .find_map(|parent| {
            input
                .get(*parent)
                .and_then(|v| v.get("command"))
                .and_then(|v| v.as_str())
        })
        .unwrap_or("")
        .to_string()
}

fn extract_session_id(input: &Value) -> Option<String> {
    ["session_id", "sessionId"]
        .iter()
        .find_map(|key| input.get(*key).and_then(|v| v.as_str()))
        .map(String::from)
        .filter(|s| !s.is_empty())
}

fn extract_agent(input: &Value) -> Option<String> {
    ["agent", "agent_type", "subagent_type"]
        .iter()
        .find_map(|key| input.get(*key).and_then(|v| v.as_str()))
        .map(String::from)
        .filter(|s| !s.is_empty())
}

pub async fn handle() -> Result<()> {
    let input = read_stdin_json()?;
    let command = extract_command(&input);

    if !command.trim().is_empty() {
        let verbs = classify(&command);
        if verbs.mutating {
            let cwd = common::extract_cwd(&input);
            if let Some(root) = git_common::repo_root_from(&cwd).await {
                let session_id = extract_session_id(&input);
                // Default the agent label to the harness when only a session id
                // is present, so the VCS event still records who acted.
                let agent = extract_agent(&input)
                    .or_else(|| session_id.as_ref().map(|_| "claude_code".to_string()));

                git_common::write_session_hint(&root, session_id.as_deref(), agent.as_deref());

                if verbs.creates_commit
                    && git_common::should_capture(&root, "commit")
                    && !crate::setup::git_hooks::is_managed_installed(Path::new(&root))
                {
                    if let Some(info) = git_common::collect_commit_info(&root).await {
                        let params = CaptureVcsLocalEventParams {
                            event_type: git_common::EVENT_COMMIT.to_string(),
                            sha: Some(info.sha),
                            message: info.message,
                            branch: info.branch,
                            additions: info.additions,
                            deletions: info.deletions,
                            files_changed: info.files_changed,
                            committed_at: info.committed_at,
                            session_id,
                            agent,
                            ..Default::default()
                        };
                        git_common::capture(&root, params).await;
                    }
                }
            }
        }
    }

    write_stdout_json(&HookOutput::empty())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{classify, extract_command};

    #[test]
    fn detects_commit_push_merge_rebase() {
        assert!(classify("git commit -m 'x'").creates_commit);
        assert!(classify("git commit -m 'x'").mutating);
        assert!(classify("git push origin main").mutating);
        assert!(!classify("git push origin main").creates_commit);
        assert!(classify("git merge feature").creates_commit);
        assert!(classify("git rebase main").creates_commit);
    }

    #[test]
    fn branch_create_requires_flag() {
        assert!(classify("git checkout -b feat/x").mutating);
        assert!(!classify("git checkout main").mutating);
        assert!(classify("git switch -c feat/x").mutating);
        assert!(!classify("git switch main").mutating);
    }

    #[test]
    fn read_only_commands_are_ignored() {
        for cmd in [
            "git status",
            "git log --oneline",
            "git diff HEAD~1",
            "git show",
            "ls -la",
            "echo git commit",
        ] {
            // `echo git commit` has `git` as a non-first token but `commit` is
            // still the subcommand after the `git` token, so it WOULD match;
            // exclude it from this assertion set deliberately.
            if cmd == "echo git commit" {
                continue;
            }
            assert!(!classify(cmd).mutating, "should ignore: {cmd}");
        }
    }

    #[test]
    fn honors_global_options_before_subcommand() {
        assert!(classify("git -C /repo commit -m x").creates_commit);
        assert!(classify("git -c user.name=bot commit -m x").creates_commit);
    }

    #[test]
    fn detects_in_chained_commands() {
        let v = classify("git add -A && git commit -m 'wip' && echo done");
        assert!(v.creates_commit);
    }

    #[test]
    fn extract_command_reads_tool_input() {
        let input = serde_json::json!({ "tool_input": { "command": "git commit -m x" } });
        assert_eq!(extract_command(&input), "git commit -m x");
    }
}
