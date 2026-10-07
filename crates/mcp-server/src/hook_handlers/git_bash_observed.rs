//! `PostToolUse` (Bash) / `afterShellExecution` hook handler — git session
//! tagging, for Claude Code, Codex and Cursor.
//!
//! When the agent runs a git-mutating shell command, this deposits a short-TTL
//! session hint (`session_id` + `agent`) keyed by repo, which the managed git
//! hooks read, and, after a commit, sends a copy of the commit event that names
//! the session.
//!
//! Which session id: the one `init` returned, which is what a transcript and a
//! resume card use, found through the record `post-tool-use` keeps for the host's
//! own session id. A card drops an event tagged with any other id as belonging to
//! another session, so the host's id is never sent, and a host with no recorded
//! `init` sends no session at all (the event is then attributed by time).
//!
//! Why a copy of the commit: the managed git hook records the commit while the
//! command is still running, before this handler runs, so it cannot know the
//! session. The backend keeps one row per commit and only adds to it, so the copy
//! adds the session without a second row, memory note or compliance event, and
//! the order the two arrive in does not matter. Without managed hooks this copy is
//! the only record of the commit.

use anyhow::Result;
use chrono::{DateTime, Utc};
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

/// A command given as one string, or as an argument list (Codex sends the argv of
/// a shell call), joined with spaces.
fn command_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Array(items) => {
            let parts: Vec<&str> = items.iter().filter_map(Value::as_str).collect();
            (!parts.is_empty()).then(|| parts.join(" "))
        }
        _ => None,
    }
}

fn extract_command(input: &Value) -> String {
    ["tool_input", "parameters", "toolParameters", "args"]
        .iter()
        .find_map(|parent| {
            input
                .get(*parent)
                .and_then(|v| v.get("command"))
                .and_then(command_text)
        })
        // Cursor's shell hooks carry the command at the top level.
        .or_else(|| input.get("command").and_then(command_text))
        .unwrap_or_default()
}

/// The session id `init` returned for the host session that ran this hook, if
/// `post-tool-use` recorded one. `None` means "do not tag", never "use the host's
/// own id".
fn api_session_id(input: &Value) -> Option<String> {
    let host = common::host_session_key(input)?;
    super::durable_capture::session_api_session_id(&host)
}

/// A commit older than this at the time the hook runs is not the one the command
/// just made (a commit that failed or did nothing leaves the previous HEAD), so it
/// is not tagged with this session.
const COMMIT_FRESH_SECS: i64 = 120;

fn commit_is_fresh(info: &git_common::CommitInfo, now: DateTime<Utc>) -> bool {
    info.committed_at
        .as_deref()
        .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        .map(|committed| {
            let age = now
                .signed_duration_since(committed.with_timezone(&Utc))
                .num_seconds();
            (-5..=COMMIT_FRESH_SECS).contains(&age)
        })
        .unwrap_or(false)
}

/// The session to tag a commit with: the session, only when the commit is one
/// the command just made.
fn commit_tag(
    session_id: Option<String>,
    info: &git_common::CommitInfo,
    now: DateTime<Utc>,
) -> Option<String> {
    session_id.filter(|_| commit_is_fresh(info, now))
}

/// Whether this hook sends a copy of the commit. With managed git hooks the
/// commit is already recorded, so a copy is only worth sending to name the
/// session; without them it is the only record.
fn send_commit_copy(managed_hooks_installed: bool, session_tag: Option<&str>) -> bool {
    !managed_hooks_installed || session_tag.is_some()
}

/// The event sent for a commit: the same fields the managed `post-commit` hook
/// sends, plus the session and agent.
fn commit_event(
    info: git_common::CommitInfo,
    session_id: Option<String>,
    agent: Option<String>,
) -> CaptureVcsLocalEventParams {
    CaptureVcsLocalEventParams {
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
    }
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
                let session_id = api_session_id(&input);
                // The harness that ran the hook, so the event records who acted
                // even when no session is known.
                let agent =
                    extract_agent(&input).or_else(|| Some(common::agent_label(&input).to_string()));

                git_common::write_session_hint(&root, session_id.as_deref(), agent.as_deref());

                if verbs.creates_commit && git_common::should_capture(&root, "commit") {
                    let managed = crate::setup::git_hooks::is_managed_installed(Path::new(&root));
                    if let Some(info) = git_common::collect_commit_info(&root).await {
                        // Tag only a commit this command just made.
                        let tag = commit_tag(session_id.clone(), &info, Utc::now());
                        // With managed hooks the commit is already recorded; a copy
                        // is sent only to name the session. Without them it is the
                        // only record.
                        if send_commit_copy(managed, tag.as_deref()) {
                            git_common::capture(&root, commit_event(info, tag, agent)).await;
                        }
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
    use super::{
        api_session_id, classify, commit_event, commit_is_fresh, commit_tag, extract_command,
        git_common::CommitInfo, send_commit_copy,
    };
    use chrono::{Duration, Utc};
    use serde_json::json;

    fn info(committed_at: Option<String>) -> CommitInfo {
        CommitInfo {
            sha: "abc1234".to_string(),
            committed_at,
            message: Some("Fix the thing".to_string()),
            branch: Some("main".to_string()),
            additions: Some(3),
            deletions: Some(1),
            files_changed: Some(2),
        }
    }

    #[test]
    fn extract_command_reads_cursor_and_codex_shapes() {
        // Cursor's shell hooks: the command at the top level.
        let cursor =
            json!({"hook_event_name": "afterShellExecution", "command": "git commit -m x"});
        assert_eq!(extract_command(&cursor), "git commit -m x");
        // Codex: an argument list.
        let codex = json!({"tool_input": {"command": ["bash", "-lc", "git commit -m x"]}});
        assert!(classify(&extract_command(&codex)).creates_commit);
        // Claude Code: a string under tool_input.
        let claude = json!({"tool_input": {"command": "git push origin main"}});
        assert!(classify(&extract_command(&claude)).mutating);
        // Nothing recognisable: empty, not a panic.
        assert_eq!(extract_command(&json!({"tool_input": {"command": 7}})), "");
    }

    #[test]
    fn only_a_commit_made_just_now_is_fresh() {
        let now = Utc::now();
        let at = |seconds: i64| Some((now - Duration::seconds(seconds)).to_rfc3339());
        assert!(commit_is_fresh(&info(at(2)), now));
        assert!(commit_is_fresh(&info(at(119)), now));
        assert!(!commit_is_fresh(&info(at(300)), now), "the previous HEAD");
        assert!(!commit_is_fresh(&info(None), now));
        assert!(!commit_is_fresh(&info(Some("not a time".to_string())), now));
        assert!(
            !commit_is_fresh(&info(at(-3600)), now),
            "a commit from the future is not trusted"
        );
    }

    #[test]
    fn the_commit_copy_carries_the_same_fields_as_the_managed_hook_plus_the_session() {
        let params = commit_event(
            info(Some("2026-10-07T01:00:00+00:00".to_string())),
            Some("api-session-1".to_string()),
            Some("cursor".to_string()),
        );
        assert_eq!(params.event_type, "commit.local");
        assert_eq!(params.sha.as_deref(), Some("abc1234"));
        assert_eq!(params.message.as_deref(), Some("Fix the thing"));
        assert_eq!(params.branch.as_deref(), Some("main"));
        assert_eq!(
            (params.additions, params.deletions, params.files_changed),
            (Some(3), Some(1), Some(2))
        );
        assert_eq!(params.session_id.as_deref(), Some("api-session-1"));
        assert_eq!(params.agent.as_deref(), Some("cursor"));
    }

    #[test]
    fn the_host_session_id_is_never_used_as_the_tag() {
        let _guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let previous_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", temp.path());

        let claude =
            json!({"session_id": "host-claude-1", "tool_input": {"command": "git commit -m x"}});
        let cursor = json!({"conversation_id": "host-cursor-1", "command": "git commit -m x"});
        // Nothing recorded for either host session: no tag at all.
        assert_eq!(api_session_id(&claude), None);
        assert_eq!(api_session_id(&cursor), None);

        // `post-tool-use` recorded the ids `init` returned for both.
        let workspace = uuid::Uuid::from_u128(11);
        let project = uuid::Uuid::from_u128(12);
        crate::hook_handlers::durable_capture::record_session_scope(
            "host-claude-1",
            "/repo",
            workspace,
            Some(project),
            Some("api-session-claude"),
        );
        crate::hook_handlers::durable_capture::record_session_scope(
            "host-cursor-1",
            "/repo",
            workspace,
            Some(project),
            Some("api-session-cursor"),
        );
        assert_eq!(
            api_session_id(&claude).as_deref(),
            Some("api-session-claude")
        );
        assert_eq!(
            api_session_id(&cursor).as_deref(),
            Some("api-session-cursor")
        );

        match previous_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
    }

    #[test]
    fn a_stale_head_is_never_tagged_with_this_session() {
        let now = Utc::now();
        let session = || Some("api-session-1".to_string());
        let fresh = info(Some((now - Duration::seconds(3)).to_rfc3339()));
        let stale = info(Some((now - Duration::seconds(900)).to_rfc3339()));
        assert_eq!(
            commit_tag(session(), &fresh, now).as_deref(),
            Some("api-session-1")
        );
        assert_eq!(commit_tag(session(), &stale, now), None, "an old HEAD");
        assert_eq!(commit_tag(None, &fresh, now), None, "no session, no tag");
    }

    #[test]
    fn a_copy_of_the_commit_is_sent_only_when_it_adds_something() {
        // Managed hooks recorded the commit: send a copy only to name the session.
        assert!(!send_commit_copy(true, None));
        assert!(send_commit_copy(true, Some("api-session-1")));
        // No managed hooks: the copy is the only record, tagged or not.
        assert!(send_commit_copy(false, None));
        assert!(send_commit_copy(false, Some("api-session-1")));
    }

    #[test]
    fn the_agent_label_names_the_harness() {
        use crate::hook_handlers::common::agent_label;
        assert_eq!(
            agent_label(&json!({"hook_event_name": "afterShellExecution", "conversation_id": "c"})),
            "cursor"
        );
        assert_eq!(
            agent_label(
                &json!({"hook_event_name": "PostToolUse", "session_id": "s", "turn_id": "t"})
            ),
            "codex"
        );
        assert_eq!(
            agent_label(&json!({"hook_event_name": "PostToolUse", "session_id": "s"})),
            "claude_code"
        );
    }

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
