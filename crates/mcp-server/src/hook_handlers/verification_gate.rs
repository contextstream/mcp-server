//! Pre-tool and pre-push verification gate enforcement.
//!
//! Enforces workspace/project verification policy (`require_verified_tree_before_push`,
//! `require_verified_tree_before_pr`, `require_verified_tree_before_complete`) before:
//! - Shell `git push` (`push`) and `gh pr create` (`pr`)
//! - ContextStream `progress(status="completed")` and `entity(kind="ticket", action="update", status="resolved"|"closed")` (`complete`)
//! - Git `pre-push` hook (`push`)
//!
//! Fail-open on offline/404/timeout/non-git directories and bypassed when
//! `CONTEXTSTREAM_HOSTED_TOOLS=off`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use mcp_client::checkout_snapshot::{compute_repo_key, compute_target_tree};
use mcp_client::command_hints;
use mcp_client::verification_gates::{cached_gate_policy, ReceiptCheckResult};
use mcp_client::ContextStreamClient;
use serde_json::Value;

use super::common;

/// Time budget for checking gate policy / verification receipts inside a hook.
pub(crate) const GATE_CHECK_TIMEOUT: Duration = Duration::from_millis(2500);

/// What verification gate an intercepted action triggers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GateKind {
    Push,
    Pr,
    Complete,
}

impl GateKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Push => "push",
            Self::Pr => "pr",
            Self::Complete => "complete",
        }
    }
}

/// Format an actionable block message naming the gate, short `target_tree`,
/// `missing_command_families`, and `contextstream-mcp run -- <cmd>`.
pub(crate) fn format_block_message(gate: &str, resp: &ReceiptCheckResult) -> String {
    let short_tree: String = resp.target_tree.chars().take(12).collect();
    let missing = if !resp.missing_command_families.is_empty() {
        resp.missing_command_families.join(", ")
    } else if !resp.required_command_families.is_empty() {
        resp.required_command_families.join(", ")
    } else {
        "a passing check".to_string()
    };
    let example_cmd = resp
        .missing_command_families
        .first()
        .or_else(|| resp.required_command_families.first())
        .map(String::as_str)
        .unwrap_or("<cmd>");
    format!(
        "Verification gate blocked ({gate}): tree {short_tree} has no passing verification receipt for [{missing}] (ttl {}s). Run `contextstream-mcp run -- {example_cmd}` (or the hosted `check` tool) on this exact tree, then retry.",
        resp.ttl_secs
    )
}

pub(crate) fn build_client(
    api_url: &str,
    api_key: &str,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
) -> ContextStreamClient {
    let client_config = mcp_types::Config {
        api_url: api_url.to_string(),
        api_key: Some(api_key.to_string()),
        default_workspace_id: workspace_id.and_then(|id| uuid::Uuid::parse_str(id).ok()),
        default_project_id: project_id.and_then(|id| uuid::Uuid::parse_str(id).ok()),
        ..Default::default()
    };
    ContextStreamClient::new(client_config)
}

fn non_empty_str<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| value.get(*k).and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Extract a shell command across Claude (`Bash`), Codex (`Bash`/`exec_command`),
/// Cursor (`beforeShellExecution` / `Shell` / `run_terminal_cmd`),
/// Cline (`execute_command`), and Windsurf (`pre_run_command` -> `Bash`).
pub(crate) fn extract_shell_command<'a>(
    input: &'a Value,
    tool_lower: &str,
    tool_input: &'a Value,
) -> Option<&'a str> {
    let hook_event = input
        .get("hook_event_name")
        .or_else(|| input.get("hookEventName"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let is_shell = matches!(
        tool_lower,
        "bash"
            | "shell"
            | "execute_command"
            | "run_terminal_cmd"
            | "exec_command"
            | "local_shell"
            | "run_command"
            | "powershell"
    ) || hook_event == "beforeShellExecution"
        || hook_event.eq_ignore_ascii_case("pre_run_command");
    if !is_shell {
        return None;
    }
    non_empty_str(tool_input, &["command", "cmd", "command_line"])
        .or_else(|| non_empty_str(input, &["command"]))
}

/// Split a shell command into simple-command segments separated by `&&`, `||`,
/// `;`, `|`, `&`, or `\n` outside single/double quotes and backslash escapes.
fn split_shell_segments(command: &str) -> Vec<&str> {
    let bytes = command.as_bytes();
    let mut segments = Vec::new();
    let mut start = 0;
    let mut quote: Option<u8> = None;
    let mut escaped = false;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if escaped {
            escaped = false;
            i += 1;
            continue;
        }
        if b == b'\\' && quote != Some(b'\'') {
            escaped = true;
            i += 1;
            continue;
        }
        if let Some(q) = quote {
            if b == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if b == b'\'' || b == b'"' {
            quote = Some(b);
            i += 1;
            continue;
        }
        if matches!(b, b';' | b'&' | b'|' | b'\n') {
            let pair = (b == b'&' || b == b'|') && bytes.get(i + 1) == Some(&b);
            segments.push(&command[start..i]);
            i += if pair { 2 } else { 1 };
            start = i;
            continue;
        }
        i += 1;
    }
    if start <= bytes.len() {
        segments.push(&command[start..]);
    }
    segments
}

/// Tokenize one shell segment honoring single quotes, double quotes, and backslash escapes.
fn tokenize_shell_segment(segment: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_token = false;
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for c in segment.chars() {
        if escaped {
            current.push(c);
            in_token = true;
            escaped = false;
            continue;
        }
        match quote {
            Some('\'') => {
                if c == '\'' {
                    quote = None;
                } else {
                    current.push(c);
                }
                in_token = true;
            }
            Some('"') => {
                if c == '"' {
                    quote = None;
                } else if c == '\\' {
                    escaped = true;
                } else {
                    current.push(c);
                }
                in_token = true;
            }
            Some(_) => unreachable!(),
            None => {
                if c == '\\' {
                    escaped = true;
                    in_token = true;
                } else if c == '\'' || c == '"' {
                    quote = Some(c);
                    in_token = true;
                } else if c.is_whitespace() || matches!(c, '(' | ')' | '{' | '}') {
                    if in_token {
                        tokens.push(std::mem::take(&mut current));
                        in_token = false;
                    }
                } else {
                    current.push(c);
                    in_token = true;
                }
            }
        }
    }
    if escaped {
        current.push('\\');
    }
    if in_token {
        tokens.push(current);
    }
    tokens
}

fn is_env_assignment(token: &str) -> bool {
    let Some((name, _)) = token.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn command_basename(token: &str) -> &str {
    token.rsplit('/').next().unwrap_or(token)
}

fn resolve_dir(base: &Path, target: &str) -> PathBuf {
    let p = Path::new(target);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    }
}

fn skip_wrappers(tokens: &[String]) -> usize {
    let mut idx = 0;
    while idx < tokens.len() {
        let tok = &tokens[idx];
        if is_env_assignment(tok) {
            idx += 1;
            continue;
        }
        match command_basename(tok) {
            "env" => {
                idx += 1;
                while idx < tokens.len() {
                    let arg = &tokens[idx];
                    if arg == "--" {
                        idx += 1;
                        break;
                    }
                    if is_env_assignment(arg) || arg == "-i" || arg == "-0" {
                        idx += 1;
                    } else if arg == "-u" || arg == "-C" || arg == "-S" {
                        idx += 2;
                    } else if arg.starts_with('-') {
                        idx += 1;
                    } else {
                        break;
                    }
                }
            }
            "exec" => {
                idx += 1;
                while idx < tokens.len() && tokens[idx].starts_with('-') {
                    let arg = &tokens[idx];
                    if arg == "--" {
                        idx += 1;
                        break;
                    }
                    if arg == "-a" {
                        idx += 2;
                    } else {
                        idx += 1;
                    }
                }
            }
            "command" | "builtin" | "time" | "nohup" => {
                idx += 1;
                while idx < tokens.len() && tokens[idx].starts_with('-') {
                    let arg = &tokens[idx];
                    idx += 1;
                    if arg == "--" {
                        break;
                    }
                }
            }
            "nice" => {
                idx += 1;
                if tokens.get(idx).is_some_and(|a| a == "-n") {
                    idx += 2;
                }
            }
            _ => break,
        }
    }
    idx
}

fn detect_git_push(args: &[String], base_dir: &Path) -> Option<(GateKind, PathBuf)> {
    let mut effective_dir = base_dir.to_path_buf();
    let mut i = 0;
    let mut subcommand: Option<&str> = None;

    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--help" || arg == "-h" {
            return None;
        }
        if arg == "-C" {
            if let Some(dir) = args.get(i + 1) {
                effective_dir = resolve_dir(&effective_dir, dir);
            }
            i += 2;
            continue;
        }
        if let Some(dir) = arg.strip_prefix("-C") {
            if !dir.is_empty() {
                effective_dir = resolve_dir(&effective_dir, dir);
            }
            i += 1;
            continue;
        }
        if matches!(
            arg,
            "-c" | "--git-dir"
                | "--work-tree"
                | "--namespace"
                | "--exec-path"
                | "--super-prefix"
                | "--config-env"
        ) {
            i += 2;
            continue;
        }
        if arg.starts_with('-') {
            i += 1;
            continue;
        }
        subcommand = Some(arg);
        i += 1;
        break;
    }

    if subcommand != Some("push") {
        return None;
    }

    let mut after_double_dash = false;
    while i < args.len() {
        let arg = args[i].as_str();
        if !after_double_dash {
            if arg == "--" {
                after_double_dash = true;
                i += 1;
                continue;
            }
            if matches!(arg, "--dry-run" | "-n" | "--help" | "-h") {
                return None;
            }
            if arg.starts_with('-')
                && !arg.starts_with("--")
                && arg.len() > 1
                && arg[1..].chars().all(|c| c.is_ascii_alphabetic())
                && arg[1..].chars().any(|c| c == 'n' || c == 'h')
            {
                return None;
            }
            if matches!(
                arg,
                "-o" | "--push-option" | "--receive-pack" | "--exec" | "--repo"
            ) {
                i += 2;
                continue;
            }
        }
        i += 1;
    }

    Some((GateKind::Push, effective_dir))
}

fn detect_gh_pr_create(args: &[String], base_dir: &Path) -> Option<(GateKind, PathBuf)> {
    let mut positionals: Vec<&str> = Vec::new();
    let mut i = 0;
    let mut after_double_dash = false;
    while i < args.len() {
        let arg = args[i].as_str();
        if !after_double_dash {
            if arg == "--" {
                after_double_dash = true;
                i += 1;
                continue;
            }
            if matches!(arg, "--help" | "-h") {
                return None;
            }
            if matches!(arg, "-R" | "--repo" | "--hostname") {
                i += 2;
                continue;
            }
            if arg.starts_with('-') {
                i += 1;
                continue;
            }
        }
        positionals.push(arg);
        i += 1;
    }
    if positionals.get(0..2) == Some(&["pr", "create"]) {
        Some((GateKind::Pr, base_dir.to_path_buf()))
    } else {
        None
    }
}

const MAX_SHELL_RECURSION_DEPTH: usize = 16;

fn extract_shell_c_script<'a>(cmd: &str, args: &'a [String]) -> Option<&'a str> {
    if !matches!(
        cmd.to_ascii_lowercase().as_str(),
        "sh" | "bash" | "zsh" | "dash" | "ksh" | "ash"
    ) {
        return None;
    }
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--" {
            break;
        }
        if matches!(arg, "-o" | "+o" | "--rcfile" | "--init-file") {
            i += 2;
            continue;
        }
        if arg.starts_with("--") {
            i += 1;
            continue;
        }
        if arg.starts_with('-') && arg.len() > 1 {
            if arg[1..].chars().all(|c| c.is_ascii_alphabetic()) && arg[1..].contains('c') {
                return args.get(i + 1).map(String::as_str);
            }
            i += 1;
            continue;
        }
        break;
    }
    None
}

fn detect_shell_gates_inner(
    command: &str,
    cwd: &Path,
    depth: usize,
    out: &mut Vec<(GateKind, PathBuf)>,
) {
    if depth >= MAX_SHELL_RECURSION_DEPTH {
        return;
    }
    let mut current_dir = cwd.to_path_buf();
    for segment in split_shell_segments(command) {
        let tokens = tokenize_shell_segment(segment);
        let idx = skip_wrappers(&tokens);
        let Some(cmd_tok) = tokens.get(idx) else {
            continue;
        };
        let cmd = command_basename(cmd_tok);
        let args = &tokens[idx + 1..];
        if cmd == "cd" {
            let target = args
                .iter()
                .find(|a| a.as_str() != "--" && !a.starts_with('-'));
            if let Some(target) = target {
                current_dir = resolve_dir(&current_dir, target);
            }
            continue;
        }
        if let Some(inner) = extract_shell_c_script(cmd, args) {
            detect_shell_gates_inner(inner, &current_dir, depth + 1, out);
            continue;
        }
        let detected = match cmd {
            "git" => detect_git_push(args, &current_dir),
            "gh" => detect_gh_pr_create(args, &current_dir),
            _ => None,
        };
        if let Some(hit) = detected {
            if !out.contains(&hit) {
                out.push(hit);
            }
        }
    }
}

/// Detect verification gates triggered by a shell command (`git push` -> `push`,
/// `gh pr create` -> `pr`) in a quote-aware manner, excluding dry-run/help.
pub(crate) fn detect_shell_gates(command: &str, cwd: &Path) -> Vec<(GateKind, PathBuf)> {
    let mut out = Vec::new();
    detect_shell_gates_inner(command, cwd, 0, &mut out);
    out
}

/// Detect whether a ContextStream MCP tool call triggers the `complete` gate.
pub(crate) fn detect_mcp_gate(
    is_contextstream_call: bool,
    normalized_tool: &str,
    tool_input: &Value,
) -> Option<GateKind> {
    if !is_contextstream_call {
        return None;
    }
    match normalized_tool {
        "progress" => {
            let action = tool_input
                .get("action")
                .and_then(Value::as_str)
                .map(str::trim)
                .map(str::to_ascii_lowercase);
            if !matches!(action.as_deref(), None | Some("") | Some("report")) {
                return None;
            }
            let status = tool_input
                .get("status")
                .and_then(Value::as_str)
                .map(str::trim)
                .map(str::to_ascii_lowercase);
            (status.as_deref() == Some("completed")).then_some(GateKind::Complete)
        }
        "entity" => {
            let kind = tool_input
                .get("kind")
                .and_then(Value::as_str)
                .map(str::trim)
                .map(str::to_ascii_lowercase);
            let action = tool_input
                .get("action")
                .and_then(Value::as_str)
                .map(str::trim)
                .map(str::to_ascii_lowercase);
            if kind.as_deref() != Some("ticket") || action.as_deref() != Some("update") {
                return None;
            }
            let status = tool_input
                .pointer("/body/status")
                .or_else(|| tool_input.get("status"))
                .and_then(Value::as_str)
                .map(str::trim)
                .map(str::to_ascii_lowercase);
            matches!(status.as_deref(), Some("resolved" | "closed")).then_some(GateKind::Complete)
        }
        _ => None,
    }
}

/// Evaluate one `(gate, dir)` against the API (using the 45s policy disk cache).
/// Returns `Some(block_message)` only when `resp.gate_enabled && !resp.allowed`.
pub(crate) async fn check_gate_with_client(
    client: &ContextStreamClient,
    identity: &str,
    workspace_id: Option<&str>,
    project_id: Option<&str>,
    gate: GateKind,
    dir: &Path,
) -> Option<String> {
    if command_hints::switched_off(std::env::var(command_hints::ENV_SWITCH).ok().as_deref()) {
        return None;
    }
    if workspace_id.is_none() && project_id.is_none() {
        return None;
    }
    let policy_resp = cached_gate_policy(
        client,
        identity,
        workspace_id,
        project_id,
        GATE_CHECK_TIMEOUT,
    )
    .await?;
    if !policy_resp.policy.is_gate_enabled(gate.as_str()) {
        return None;
    }
    let target_tree = compute_target_tree(dir).await?;
    let repo_key = compute_repo_key(dir).await;
    let resp = client
        .check_receipts(
            &target_tree,
            gate.as_str(),
            workspace_id,
            project_id,
            repo_key.as_deref(),
            GATE_CHECK_TIMEOUT,
        )
        .await
        .ok()
        .flatten()?;
    if resp.gate_enabled && !resp.allowed {
        Some(format_block_message(gate.as_str(), &resp))
    } else {
        None
    }
}

/// Check verification gates for a PreToolUse invocation before `rewritten_bash`.
/// Returns `Some(block_message)` when the call must be blocked, or `None` to allow.
pub(crate) async fn check_pre_tool_use(
    input: &Value,
    tool_lower: &str,
    tool_input: &Value,
    cwd: &str,
    is_contextstream_call: bool,
    normalized_contextstream_tool: &str,
) -> Option<String> {
    if command_hints::switched_off(std::env::var(command_hints::ENV_SWITCH).ok().as_deref()) {
        return None;
    }

    let mut targets: Vec<(GateKind, PathBuf)> = Vec::new();
    if let Some(command) = extract_shell_command(input, tool_lower, tool_input) {
        targets.extend(detect_shell_gates(command, Path::new(cwd)));
    }
    if let Some(mcp_gate) = detect_mcp_gate(
        is_contextstream_call,
        normalized_contextstream_tool,
        tool_input,
    ) {
        targets.push((mcp_gate, PathBuf::from(cwd)));
    }
    if targets.is_empty() {
        return None;
    }

    let config = common::load_config(cwd);
    if !config.is_configured() {
        return None;
    }
    let workspace_id = if is_contextstream_call {
        non_empty_str(tool_input, &["workspace_id", "workspaceId"])
            .map(str::to_string)
            .or(config.workspace_id)
    } else {
        config.workspace_id
    };
    let project_id = if is_contextstream_call {
        non_empty_str(tool_input, &["project_id", "projectId"])
            .map(str::to_string)
            .or(config.project_id)
    } else {
        config.project_id
    };
    if workspace_id.is_none() && project_id.is_none() {
        return None;
    }

    let identity = command_hints::local_identity(&config.api_key);
    let client = build_client(
        &config.api_url,
        &config.api_key,
        workspace_id.as_deref(),
        project_id.as_deref(),
    );

    for (gate, dir) in targets {
        if let Some(msg) = check_gate_with_client(
            &client,
            &identity,
            workspace_id.as_deref(),
            project_id.as_deref(),
            gate,
            &dir,
        )
        .await
        {
            return Some(msg);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    #![allow(clippy::await_holding_lock)]
    use super::*;
    use std::process::Command as StdCommand;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn shell_parser_detects_git_push_and_gh_pr_create_with_exclusions() {
        let cwd = Path::new("/repo");

        assert_eq!(
            detect_shell_gates("git push origin main", cwd),
            vec![(GateKind::Push, cwd.to_path_buf())]
        );
        assert_eq!(
            detect_shell_gates("cargo test && FOO=1 git -C crates push -u origin feat", cwd),
            vec![(GateKind::Push, PathBuf::from("/repo/crates"))]
        );
        assert_eq!(
            detect_shell_gates("cd /other && gh pr create --title \"hi\"", cwd),
            vec![(GateKind::Pr, PathBuf::from("/other"))]
        );
        assert_eq!(
            detect_shell_gates("git push && gh pr create", cwd),
            vec![
                (GateKind::Push, cwd.to_path_buf()),
                (GateKind::Pr, cwd.to_path_buf())
            ]
        );

        // Dry-run, help, and quoted strings must NOT trigger gates.
        for safe in [
            "git push --dry-run origin main",
            "git push -n origin main",
            "git push -un origin main",
            "git push --help",
            "git push -h",
            "gh pr create --help",
            "gh pr create -h",
            "gh pr list",
            "echo \"git push\"",
            "echo 'gh pr create'",
            "echo \"(git push)\"",
            "echo '(gh pr create)'",
            "bash -c \"echo 'git push'\"",
            "bash -c \"git push --dry-run origin main\"",
            "bash script.sh",
            "git commit -m \"mentions git push and gh pr create\"",
            "git status",
        ] {
            assert!(
                detect_shell_gates(safe, cwd).is_empty(),
                "unexpected gate for {safe:?}"
            );
        }

        // Shell -c/-lc/-ec, exec/command/builtin wrappers, and subshell/brace groupings.
        assert_eq!(
            detect_shell_gates("bash -c \"git push origin main\"", cwd),
            vec![(GateKind::Push, cwd.to_path_buf())]
        );
        assert_eq!(
            detect_shell_gates("/bin/bash -lc \"cd sub && git push origin main\"", cwd),
            vec![(GateKind::Push, PathBuf::from("/repo/sub"))]
        );
        assert_eq!(
            detect_shell_gates("sh -lc \"gh pr create --title x\"", cwd),
            vec![(GateKind::Pr, cwd.to_path_buf())]
        );
        assert_eq!(
            detect_shell_gates("zsh -ec 'git push origin main'", cwd),
            vec![(GateKind::Push, cwd.to_path_buf())]
        );
        assert_eq!(
            detect_shell_gates("exec git push origin main", cwd),
            vec![(GateKind::Push, cwd.to_path_buf())]
        );
        assert_eq!(
            detect_shell_gates("exec -a mygit builtin command git push origin main", cwd),
            vec![(GateKind::Push, cwd.to_path_buf())]
        );
        assert_eq!(
            detect_shell_gates("(git push origin main)", cwd),
            vec![(GateKind::Push, cwd.to_path_buf())]
        );
        assert_eq!(
            detect_shell_gates("{ gh pr create --title x; }", cwd),
            vec![(GateKind::Pr, cwd.to_path_buf())]
        );
        assert_eq!(
            detect_shell_gates("((cd sub && exec bash -c \"git push\"))", cwd),
            vec![(GateKind::Push, PathBuf::from("/repo/sub"))]
        );
    }

    #[test]
    fn mcp_gate_detects_completed_progress_and_resolved_closed_tickets() {
        assert_eq!(
            detect_mcp_gate(
                true,
                "progress",
                &serde_json::json!({"status": " completed "})
            ),
            Some(GateKind::Complete)
        );
        for non_blocking in [
            "working",
            "waiting_input",
            "waiting_approval",
            "partial",
            "failed",
            "stopped",
            "cancelled",
        ] {
            assert_eq!(
                detect_mcp_gate(
                    true,
                    "progress",
                    &serde_json::json!({"status": non_blocking})
                ),
                None,
                "status {non_blocking} must not be blocked"
            );
        }
        assert_eq!(
            detect_mcp_gate(
                true,
                "entity",
                &serde_json::json!({"kind": "ticket", "action": "update", "id": "t1", "body": {"status": "resolved"}})
            ),
            Some(GateKind::Complete)
        );
        assert_eq!(
            detect_mcp_gate(
                true,
                "entity",
                &serde_json::json!({"kind": "ticket", "action": "update", "id": "t1", "status": "CLOSED"})
            ),
            Some(GateKind::Complete)
        );
        assert_eq!(
            detect_mcp_gate(
                true,
                "entity",
                &serde_json::json!({"kind": "ticket", "action": "update", "id": "t1", "body": {"status": "in_progress"}})
            ),
            None
        );
        assert_eq!(
            detect_mcp_gate(
                false,
                "progress",
                &serde_json::json!({"status": "completed"})
            ),
            None
        );
    }

    #[test]
    fn extract_shell_command_handles_all_editor_shapes() {
        let claude =
            serde_json::json!({"tool_name": "Bash", "tool_input": {"command": "git push"}});
        assert_eq!(
            extract_shell_command(&claude, "bash", &claude["tool_input"]),
            Some("git push")
        );
        let cursor_shell = serde_json::json!({"hook_event_name": "beforeShellExecution", "command": "gh pr create"});
        assert_eq!(
            extract_shell_command(&cursor_shell, "", &serde_json::json!({})),
            Some("gh pr create")
        );
        let cline = serde_json::json!({"toolName": "execute_command", "toolParameters": {"command": "git push"}});
        assert_eq!(
            extract_shell_command(&cline, "execute_command", &cline["toolParameters"]),
            Some("git push")
        );
    }

    fn init_git_repo(dir: &Path) -> String {
        let git = |args: &[&str]| {
            let out = StdCommand::new("git")
                .arg("-C")
                .arg(dir)
                .args([
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.local",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .expect("git");
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&["init", "-q"]);
        std::fs::write(dir.join("README.md"), "hello\n").unwrap();
        git(&["add", "README.md"]);
        git(&["commit", "-q", "-m", "init"]);
        git(&["rev-parse", "HEAD^{tree}"])
    }

    async fn spawn_mock_gate_server(
        policy_body: serde_json::Value,
        receipt_body: serde_json::Value,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            for _ in 0..2 {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let mut buf = [0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let body = if req.contains("/gate-policy") {
                    policy_body.to_string()
                } else {
                    receipt_body.to_string()
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(resp.as_bytes()).await;
                let _ = socket.shutdown().await;
            }
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn check_pre_tool_use_blocks_when_gate_enabled_and_unverified() {
        let _guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev_home = std::env::var_os("HOME");
        let prev_switch = std::env::var_os(command_hints::ENV_SWITCH);
        let temp_home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", temp_home.path());
        std::env::remove_var(command_hints::ENV_SWITCH);

        let repo = tempfile::tempdir().unwrap();
        let tree = init_git_repo(repo.path());
        let ws = "11111111-1111-1111-1111-111111111111";
        let proj = "22222222-2222-2222-2222-222222222222";

        let policy_json = serde_json::json!({
            "success": true,
            "data": {
                "workspace_id": ws,
                "project_id": proj,
                "inherited": false,
                "policy": {
                    "require_verified_tree_before_push": true,
                    "require_verified_tree_before_pr": true,
                    "require_verified_tree_before_complete": true,
                    "required_command_families": ["cargo test"],
                    "receipt_ttl_secs": 3600
                }
            }
        });
        let check_json = serde_json::json!({
            "success": true,
            "data": {
                "allowed": false,
                "gate": "push",
                "gate_enabled": true,
                "workspace_id": ws,
                "project_id": proj,
                "repo_key": "repo-12345678",
                "target_tree": tree,
                "ttl_secs": 3600,
                "missing_command_families": ["cargo test"],
                "required_command_families": ["cargo test"]
            }
        });
        let (url, _srv) = spawn_mock_gate_server(policy_json, check_json).await;

        std::fs::create_dir_all(repo.path().join(".contextstream")).unwrap();
        std::fs::write(
            repo.path().join(".contextstream/config.json"),
            serde_json::json!({"workspace_id": ws, "project_id": proj}).to_string(),
        )
        .unwrap();
        std::fs::write(
            repo.path().join(".mcp.json"),
            serde_json::json!({
                "mcpServers": {
                    "contextstream": {
                        "env": {
                            "CONTEXTSTREAM_API_KEY": "test-key",
                            "CONTEXTSTREAM_API_URL": url
                        }
                    }
                }
            })
            .to_string(),
        )
        .unwrap();

        let input = serde_json::json!({
            "tool_name": "Bash",
            "tool_input": {"command": "git push origin main"},
            "cwd": repo.path().to_str().unwrap()
        });
        let blocked = check_pre_tool_use(
            &input,
            "bash",
            &input["tool_input"],
            repo.path().to_str().unwrap(),
            false,
            "bash",
        )
        .await
        .expect("should block unverified git push");
        assert!(blocked.contains("push"), "{blocked}");
        assert!(blocked.contains(&tree[..12]), "{blocked}");
        assert!(blocked.contains("cargo test"), "{blocked}");
        assert!(
            blocked.contains("contextstream-mcp run -- cargo test"),
            "{blocked}"
        );

        // Second check (progress completed) hits the 45s cached policy and checks receipts.
        let (url2, _srv2) = spawn_mock_gate_server(
            serde_json::json!({}),
            serde_json::json!({
                "success": true,
                "data": {
                    "allowed": false,
                    "gate": "complete",
                    "gate_enabled": true,
                    "target_tree": tree,
                    "ttl_secs": 3600,
                    "missing_command_families": ["cargo test"]
                }
            }),
        )
        .await;
        std::fs::write(
            repo.path().join(".mcp.json"),
            serde_json::json!({
                "mcpServers": {
                    "contextstream": {
                        "env": {
                            "CONTEXTSTREAM_API_KEY": "test-key-2",
                            "CONTEXTSTREAM_API_URL": url2
                        }
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        // Also verify fail-open on unreachable server.
        let offline_input = serde_json::json!({
            "tool_name": "mcp__contextstream__progress",
            "tool_input": {"status": "completed"},
            "cwd": repo.path().to_str().unwrap()
        });
        // Here srv2 has empty policy `{}` -> policy.is_gate_enabled("complete") is false -> fail-open Allow!
        assert!(check_pre_tool_use(
            &offline_input,
            "mcp__contextstream__progress",
            &offline_input["tool_input"],
            repo.path().to_str().unwrap(),
            true,
            "progress",
        )
        .await
        .is_none());

        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match prev_switch {
            Some(v) => std::env::set_var(command_hints::ENV_SWITCH, v),
            None => std::env::remove_var(command_hints::ENV_SWITCH),
        }
    }
}
