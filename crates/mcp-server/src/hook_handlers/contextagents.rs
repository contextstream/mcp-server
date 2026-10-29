//! ContextAgents in the hooks (the workspace's `auto_run.mcp`).
//!
//! When the person's request (UserPromptSubmit), a step the agent is about to take
//! (PreToolUse: a shell command, an edit) or a command that failed (PostToolUseFailure) fits
//! a ContextAgent, and the workspace's automatic runs are on, the hook adds the exact call
//! that starts it in the background. The moments, the setting, the once-per-task ledger and
//! the words are mcp-tools' (`slipstream::auto`), shared with `context()`: nothing is read
//! unless a moment fits, and the setting is kept 60 s in `~/.contextstream/agents-auto/`.

use mcp_tools::domains::slipstream::auto::{self, RepoFacts, SuggestScope, Trigger};
use mcp_tools::domains::slipstream::moments::{self, Moment};
use serde_json::Value;
use std::time::Duration;
use uuid::Uuid;

/// How long a hook waits for the setting when it is not kept.
const HOOK_BUDGET: Duration = Duration::from_millis(1500);

/// The agents tool as Claude Code and Codex name it.
pub(crate) const CLAUDE_AGENTS_TOOL: &str = "mcp__contextstream__agents";

/// Where a hook works: its credentials, workspace, project and folder.
pub(crate) struct HookScope<'a> {
    pub api_url: &'a str,
    pub api_key: &'a str,
    pub workspace_id: Option<&'a str>,
    pub project_id: Option<&'a str>,
    pub folder: &'a str,
}

/// The hint for `moments`, or `None`: nothing fits, no credentials or workspace, the
/// setting is off, or every fitting agent was suggested for this task already.
pub(crate) async fn hint(
    scope: &HookScope<'_>,
    moments: &[&Moment],
    prompt: Option<&str>,
    trigger: Trigger,
    tool: &str,
) -> Option<String> {
    if moments.is_empty() || scope.api_key.trim().is_empty() {
        return None;
    }
    let workspace_id = scope
        .workspace_id
        .and_then(|id| Uuid::parse_str(id.trim()).ok())?;
    let project_id = scope
        .project_id
        .and_then(|id| Uuid::parse_str(id.trim()).ok());
    let client = mcp_client::ContextStreamClient::new(mcp_types::Config {
        api_url: scope.api_url.to_string(),
        api_key: Some(scope.api_key.to_string()),
        default_workspace_id: Some(workspace_id),
        default_project_id: project_id,
        ..Default::default()
    });
    let caller = auto::caller(&client).await?;
    let suggest_scope = SuggestScope {
        client: &client,
        caller: &caller,
        workspace_id,
        project_id,
        shared: true,
        budget: HOOK_BUDGET,
    };
    let folder = scope.folder;
    auto::suggest(
        &suggest_scope,
        moments,
        prompt,
        || RepoFacts::from_folder(folder),
        trigger,
        tool,
    )
    .await
}

/// The hint for a hook's tool call, in the scope the hook resolves for it.
pub(crate) async fn step_hint(
    input: &Value,
    cwd: &str,
    moments: &[&Moment],
    tool: &str,
) -> Option<String> {
    if moments.is_empty() {
        return None;
    }
    let scope = super::mid_run_notices::resolve_hook_scope(input, cwd);
    hint(
        &HookScope {
            api_url: &scope.config.api_url,
            api_key: &scope.config.api_key,
            workspace_id: scope.config.workspace_id.as_deref(),
            project_id: scope.config.project_id.as_deref(),
            folder: &scope.effective_folder_path,
        },
        moments,
        None,
        Trigger::Step,
        tool,
    )
    .await
}

/// The command a shell tool call runs.
fn shell_command(tool: &str, input: &Value) -> Option<String> {
    if !matches!(
        tool,
        "bash"
            | "shell"
            | "run_shell_command"
            | "exec_command"
            | "local_shell"
            | "run_terminal_cmd"
    ) {
        return None;
    }
    match input.get("command").or_else(|| input.get("cmd"))? {
        Value::String(command) => Some(command.clone()),
        Value::Array(parts) => Some(
            parts
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" "),
        ),
        _ => None,
    }
}

/// The files an edit tool call writes.
fn edited_paths(tool: &str, input: &Value) -> Vec<String> {
    if !matches!(
        tool,
        "edit"
            | "write"
            | "multiedit"
            | "notebookedit"
            | "str_replace_editor"
            | "apply_patch"
            | "write_file"
            | "edit_file"
            | "replace"
    ) {
        return Vec::new();
    }
    let mut paths: Vec<String> = ["file_path", "path", "notebook_path", "target_file"]
        .iter()
        .filter_map(|key| input.get(*key).and_then(Value::as_str))
        .map(str::to_string)
        .collect();
    // Codex's apply_patch names its files inside the patch.
    for key in ["patch", "input"] {
        if let Some(patch) = input.get(key).and_then(Value::as_str) {
            for line in patch.lines() {
                for prefix in ["*** Update File: ", "*** Add File: "] {
                    if let Some(path) = line.strip_prefix(prefix) {
                        paths.push(path.trim().to_string());
                    }
                }
            }
        }
    }
    paths
}

/// The moments a tool call about to run is: a shell command, or an edit to a file.
pub(crate) fn step_moments(tool: &str, input: &Value) -> Vec<&'static Moment> {
    let tool = tool.to_ascii_lowercase();
    if let Some(command) = shell_command(&tool, input) {
        return moments::command_moments(&command, false);
    }
    let mut found: Vec<&'static Moment> = Vec::new();
    for path in edited_paths(&tool, input) {
        for moment in moments::path_moments(&path) {
            if !found.iter().any(|m| m.id == moment.id) {
                found.push(moment);
            }
        }
    }
    found
}

/// The moments a failed tool call is: a shell command that failed.
pub(crate) fn failed_step_moments(tool: &str, input: &Value) -> Vec<&'static Moment> {
    shell_command(&tool.to_ascii_lowercase(), input)
        .map(|command| moments::command_moments(&command, true))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ids(moments: &[&Moment]) -> Vec<String> {
        moments.iter().map(|m| m.id.clone()).collect()
    }

    #[test]
    fn shell_commands_and_edits_are_read_from_each_clients_tool_shape() {
        assert_eq!(
            ids(&step_moments(
                "Bash",
                &json!({"command": "cd app && npm outdated"})
            )),
            ["dependencies"]
        );
        assert_eq!(
            ids(&step_moments(
                "shell",
                &json!({"command": ["bash", "-lc", "npm audit --json"]})
            )),
            ["security"]
        );
        assert_eq!(
            ids(&step_moments(
                "Bash",
                &json!({"command": "git tag v1.2.0 && git push --tags"})
            )),
            ["release"]
        );
        assert_eq!(
            ids(&step_moments(
                "Edit",
                &json!({"file_path": "/repo/docs/setup.md"})
            )),
            ["docs"]
        );
        assert_eq!(
            ids(&step_moments(
                "Write",
                &json!({"file_path": "/repo/Cargo.lock"})
            )),
            ["security"]
        );
        assert_eq!(
            ids(&step_moments(
                "apply_patch",
                &json!({"input": "*** Begin Patch\n*** Update File: CHANGELOG.md\n@@\n"})
            )),
            ["release"]
        );
        // Ordinary work is no moment.
        assert!(step_moments("Bash", &json!({"command": "cargo test -p api"})).is_empty());
        assert!(step_moments("Bash", &json!({"command": "npm init -y"})).is_empty());
        assert!(step_moments("Edit", &json!({"file_path": "/repo/src/main.rs"})).is_empty());
        assert!(step_moments("Read", &json!({"file_path": "/repo/README.md"})).is_empty());
    }

    #[test]
    fn a_failed_install_or_ci_check_is_a_moment_and_a_failed_test_run_is_not() {
        assert_eq!(
            ids(&failed_step_moments("Bash", &json!({"command": "npm ci"}))),
            ["dependencies"]
        );
        assert_eq!(
            ids(&failed_step_moments(
                "Bash",
                &json!({"command": "gh pr checks 42"})
            )),
            ["ci_failure"]
        );
        assert!(failed_step_moments("Bash", &json!({"command": "npm test"})).is_empty());
        assert!(failed_step_moments("Edit", &json!({"file_path": "a.md"})).is_empty());
    }
}
