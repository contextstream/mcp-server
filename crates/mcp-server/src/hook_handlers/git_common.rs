//! Shared helpers for the local git-capture hook handlers.
//!
//! The managed git hooks (`post-commit`, `pre-push`, `post-checkout`,
//! `post-merge`) all dispatch into this module. Every helper is **fail-open**:
//! any error, timeout, missing scope, or absent credential results in a quiet
//! no-op rather than a propagated error. Git must never be blocked or slowed by
//! capture, so all git subprocesses run with `GIT_OPTIONAL_LOCKS=0`, a
//! sub-second timeout, and discarded stderr (mirroring
//! `domains/session.rs::git_commits_after_index`). The hook scripts background
//! the whole invocation, so wall-clock here only bounds a detached process.

use mcp_client::CaptureVcsLocalEventParams;
use mcp_session::checkout_identity::{validate_checkout_binding, RepositoryRemoteIdentity};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use uuid::Uuid;

/// Per-call git timeout. Sub-second so a slow/locked repo can never keep the
/// detached capture process alive for long.
const GIT_TIMEOUT: Duration = Duration::from_millis(800);

/// Session-hint freshness window. A hint deposited by the Claude Bash
/// PostToolUse handler is only applied to a capture if it was written within
/// this many seconds, so stale tags from old sessions never attach.
const SESSION_HINT_TTL_SECS: i64 = 120;

/// Canonical event-type strings sent to the backend.
pub const EVENT_COMMIT: &str = "commit.local";
pub const EVENT_PUSH: &str = "push.local";
pub const EVENT_CHECKOUT: &str = "branch.checkout";
pub const EVENT_MERGE: &str = "merge.local";

/// Run a git subcommand under `root`, returning raw stdout on success.
///
/// Fail-open: timeout, spawn failure, or non-zero exit all yield `None`.
pub async fn run_git(root: &str, args: &[&str], timeout: Duration) -> Option<String> {
    let output = tokio::time::timeout(
        timeout,
        Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output(),
    )
    .await
    .ok()?
    .ok()?;

    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Run a git subcommand and return the trimmed single-line stdout.
async fn run_git_line(root: &str, args: &[&str]) -> Option<String> {
    run_git(root, args, GIT_TIMEOUT)
        .await
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Resolve the repository root (worktree top level) for the process cwd.
pub async fn repo_root() -> Option<String> {
    let cwd = std::env::current_dir().ok()?.to_string_lossy().to_string();
    repo_root_from(&cwd).await
}

/// Resolve the repository root for an explicit working directory.
pub async fn repo_root_from(cwd: &str) -> Option<String> {
    run_git_line(cwd, &["rev-parse", "--show-toplevel"]).await
}

/// The current branch, or `None` for a detached HEAD.
pub async fn current_branch(root: &str) -> Option<String> {
    run_git_line(root, &["rev-parse", "--abbrev-ref", "HEAD"])
        .await
        .filter(|b| b != "HEAD")
}

/// Resolve git hook positional arguments forwarded by the managed hook script.
///
/// `main.rs` joins the trailing CLI args of `contextstream-mcp hook <name>` into
/// `CONTEXTSTREAM_HOOK_ARGS` (Unit-Separator delimited) before dispatch. Git
/// argv values (shas, refs, flags, remote URLs) never contain the separator.
pub fn hook_args() -> Vec<String> {
    std::env::var("CONTEXTSTREAM_HOOK_ARGS")
        .ok()
        .filter(|raw| !raw.is_empty())
        .map(|raw| raw.split('\u{1f}').map(str::to_string).collect())
        .unwrap_or_default()
}

/// Read raw (non-JSON) stdin, used by the pre-push handler for the ref list.
pub fn read_stdin_raw() -> String {
    use std::io::Read;
    let mut buf = String::new();
    let _ = std::io::stdin().lock().read_to_string(&mut buf);
    buf
}

// ============================================================================
// Disable / per-repo policy (step 6)
// ============================================================================

fn env_truthy(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "off" | "0" | "false" | "no" | "disabled" => Some(false),
        "on" | "1" | "true" | "yes" | "enabled" => Some(true),
        _ => None,
    }
}

/// Read the nearest `git_capture` config object by walking up from `root`.
fn nearest_git_capture(root: &str) -> Option<serde_json::Value> {
    let mut dir = PathBuf::from(root);
    loop {
        let cfg = dir.join(".contextstream").join("config.json");
        if let Ok(content) = std::fs::read_to_string(&cfg) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(git_capture) = json.get("git_capture") {
                    return Some(git_capture.clone());
                }
            }
        }
        if !dir.pop() {
            break;
        }
    }
    None
}

/// Whether git capture is disabled for `root`.
///
/// Resolution order (first match wins):
///   1. `CONTEXTSTREAM_GIT_CAPTURE` env kill-switch (`off` disables, `on` forces on)
///   2. nearest `.contextstream/config.json` -> `git_capture.enabled`
///   3. global default ([`crate::config::git_capture_default_enabled`])
///
/// Handlers call this at runtime so toggling capture off stops new events even
/// when the hook scripts remain on disk.
pub fn capture_disabled(root: &str) -> bool {
    if let Ok(raw) = std::env::var("CONTEXTSTREAM_GIT_CAPTURE") {
        if let Some(enabled) = env_truthy(&raw) {
            return !enabled;
        }
    }
    if let Some(enabled) = nearest_git_capture(root)
        .as_ref()
        .and_then(|gc| gc.get("enabled"))
        .and_then(|v| v.as_bool())
    {
        return !enabled;
    }
    !crate::config::git_capture_default_enabled()
}

/// Whether a specific operation (`commit`/`push`/`checkout`/`merge`) is allowed
/// by the per-repo events allow-list. Absent list => all operations allowed.
pub fn event_enabled(root: &str, event: &str) -> bool {
    match nearest_git_capture(root)
        .as_ref()
        .and_then(|gc| gc.get("events"))
        .and_then(|v| v.as_array())
    {
        Some(events) => events
            .iter()
            .filter_map(|e| e.as_str())
            .any(|e| e.eq_ignore_ascii_case(event)),
        None => true,
    }
}

/// Combined gate: capture is on for this repo AND this operation is allowed.
pub fn should_capture(root: &str, event: &str) -> bool {
    !capture_disabled(root) && event_enabled(root, event)
}

// ============================================================================
// Scope resolution (step 3)
// ============================================================================

/// Resolved checkout-bound workspace/project scope.
#[derive(Debug, Clone, Default)]
pub struct RepoScope {
    pub workspace_id: Option<Uuid>,
    pub project_id: Option<Uuid>,
}

fn constrain_event_scope(
    params: &mut CaptureVcsLocalEventParams,
    workspace_id: Uuid,
    project_id: Option<Uuid>,
) {
    params.workspace_id = Some(workspace_id);
    params.project_id = project_id;
}

/// Resolve capture scope only from the checkout-bound local config. Environment
/// IDs may confirm that binding (enforced by `find_project_config`) but never
/// override it. `capture` performs the uncached API ownership check immediately
/// before dispatch and drops project attribution on mismatch.
pub fn resolve_scope(root: &str) -> RepoScope {
    let Some(config) = super::post_tool_use::find_project_config(root, root) else {
        return RepoScope::default();
    };

    RepoScope {
        workspace_id: Some(config.workspace_id),
        project_id: Some(config.project_id),
    }
}

// ============================================================================
// Session hints (step 5)
// ============================================================================

/// A short-TTL session tag deposited by the Claude Bash PostToolUse handler.
#[derive(Debug, Clone, Default)]
pub struct SessionHint {
    pub session_id: Option<String>,
    pub agent: Option<String>,
}

/// Stable per-repo filename for the session-hint sidecar.
fn repo_hash(root: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    root.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Path of the session-hint file for `root`, under
/// `~/.contextstream/git-session-hints/<repo_hash>.json`.
pub fn session_hint_path(root: &str) -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(
        home.join(".contextstream")
            .join("git-session-hints")
            .join(format!("{}.json", repo_hash(root))),
    )
}

/// Deposit a session hint for `root` (called from the Claude Bash handler).
pub fn write_session_hint(root: &str, session_id: Option<&str>, agent: Option<&str>) {
    let Some(path) = session_hint_path(root) else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let payload = serde_json::json!({
        "session_id": session_id,
        "agent": agent,
        "ts": chrono::Utc::now().to_rfc3339(),
    });
    let _ = std::fs::write(&path, payload.to_string());
}

/// Read a *fresh* session hint for `root`, or `None` when absent/stale.
pub fn recent_session_hint(root: &str) -> Option<SessionHint> {
    let path = session_hint_path(root)?;
    let content = std::fs::read_to_string(&path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&content).ok()?;

    let ts = json
        .get("ts")
        .and_then(|v| v.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())?;
    let age = chrono::Utc::now()
        .signed_duration_since(ts.with_timezone(&chrono::Utc))
        .num_seconds();
    if !(0..=SESSION_HINT_TTL_SECS).contains(&age) {
        return None;
    }

    Some(SessionHint {
        session_id: json
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(String::from),
        agent: json.get("agent").and_then(|v| v.as_str()).map(String::from),
    })
}

// ============================================================================
// Commit collection (step 3) + dispatch
// ============================================================================

/// Parsed metadata for a single commit.
#[derive(Debug, Clone, Default)]
pub struct CommitInfo {
    pub sha: String,
    pub committed_at: Option<String>,
    pub message: Option<String>,
    pub branch: Option<String>,
    pub additions: Option<i64>,
    pub deletions: Option<i64>,
    pub files_changed: Option<i64>,
}

/// Parse `git log -1` output (sha, ISO time, subject) joined by the Unit
/// Separator (`%x1f`). Author identity and full commit bodies are deliberately
/// never collected.
pub fn parse_commit_log(raw: &str) -> Option<CommitInfo> {
    let mut parts = raw.splitn(3, '\u{1f}');
    let sha = parts.next()?.trim().to_string();
    if sha.is_empty() {
        return None;
    }
    let mut next_field = || {
        parts
            .next()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    Some(CommitInfo {
        sha,
        committed_at: next_field(),
        message: next_field(),
        ..Default::default()
    })
}

/// Sum additions/deletions/files for HEAD via `diff-tree --numstat`.
/// Binary files report `-` for adds/dels and are counted in `files` only.
fn parse_numstat(raw: &str) -> (i64, i64, i64) {
    let mut additions = 0i64;
    let mut deletions = 0i64;
    let mut files = 0i64;
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut cols = line.split('\t');
        let adds = cols.next().unwrap_or("");
        let dels = cols.next().unwrap_or("");
        files += 1;
        if let Ok(n) = adds.parse::<i64>() {
            additions += n;
        }
        if let Ok(n) = dels.parse::<i64>() {
            deletions += n;
        }
    }
    (additions, deletions, files)
}

/// Collect privacy-bounded metadata for HEAD: sha, commit time, subject,
/// branch, and aggregate numstat totals.
pub async fn collect_commit_info(root: &str) -> Option<CommitInfo> {
    let raw = run_git(
        root,
        &["log", "-1", "--format=%H%x1f%cI%x1f%s"],
        GIT_TIMEOUT,
    )
    .await?;
    let mut info = parse_commit_log(&raw)?;
    info.branch = current_branch(root).await;
    if let Some(numstat) = run_git(
        root,
        &["diff-tree", "--no-commit-id", "--numstat", "-r", "HEAD"],
        GIT_TIMEOUT,
    )
    .await
    {
        let (additions, deletions, files) = parse_numstat(&numstat);
        info.additions = Some(additions);
        info.deletions = Some(deletions);
        info.files_changed = Some(files);
    }
    Some(info)
}

/// Resolve scope, fill in repo metadata + session hint, and POST the event.
///
/// Fail-open: a missing workspace, missing credentials, or a network error all
/// result in a quiet return. The managed git hook is the single source of truth
/// that creates events; the backend dedupes by commit sha so a later
/// Claude-Bash annotation collapses onto the same row.
pub async fn capture(root: &str, mut params: CaptureVcsLocalEventParams) {
    let scope = resolve_scope(root);
    let Some(workspace_id) = scope.workspace_id else {
        // No resolvable workspace — cannot attribute the event. Skip quietly.
        return;
    };

    // Incoming hook/environment scope may never override the checkout.
    constrain_event_scope(&mut params, workspace_id, scope.project_id);

    // Convert every machine-local and configured value into a validated,
    // portable identity before it can reach the HTTP client. The legacy
    // `repo_path` wire field intentionally carries an opaque checkout ID.
    let Some(config) = super::post_tool_use::find_project_config(root, root) else {
        return;
    };
    if config.workspace_id != workspace_id {
        return;
    }
    let Ok(binding) = validate_checkout_binding(root, Some(workspace_id), config.project_id) else {
        return;
    };
    params.repo_path = Some(binding.checkout_id.as_str().to_string());
    let pushed_remote = params
        .remote_url
        .as_deref()
        .and_then(|value| RepositoryRemoteIdentity::from_remote_url(value).ok());
    params.remote_url = pushed_remote
        .or(binding.repository_remote_identity)
        .map(|identity| identity.canonical_https_url());
    if let Some(hint) = recent_session_hint(root) {
        if params.session_id.is_none() {
            params.session_id = hint.session_id;
        }
        if params.agent.is_none() {
            params.agent = hint.agent;
        }
    }

    // Re-resolve immediately before sending. Managed git hooks are observers:
    // they never persist/rebind checkout scope. If ownership changed since the
    // initial resolution, drop project scope rather than misattribute metadata.
    let Some(config) = super::post_tool_use::find_project_config(root, root) else {
        return;
    };
    if config.workspace_id != workspace_id {
        return;
    }
    let client = super::post_tool_use::build_hook_client(&config);
    params.project_id = match client.get_project_fresh(config.project_id).await {
        Ok(project) if project.workspace_id == Some(workspace_id) => Some(config.project_id),
        _ => None,
    };
    let _ = client.capture_vcs_local_event(params).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_commit_log_extracts_all_fields() {
        let raw = "abc123\u{1f}2026-06-19T10:00:00+00:00\u{1f}Add analytical engine\n";
        let info = parse_commit_log(raw).expect("parsed");
        assert_eq!(info.sha, "abc123");
        assert_eq!(
            info.committed_at.as_deref(),
            Some("2026-06-19T10:00:00+00:00")
        );
        assert_eq!(info.message.as_deref(), Some("Add analytical engine"));
    }

    #[test]
    fn parse_commit_log_rejects_empty_sha() {
        assert!(parse_commit_log("").is_none());
        assert!(parse_commit_log("\u{1f}name").is_none());
    }

    #[test]
    fn parse_numstat_sums_and_skips_binaries() {
        let raw = "10\t2\tsrc/a.rs\n5\t0\tsrc/b.rs\n-\t-\tassets/logo.png\n";
        let (adds, dels, files) = parse_numstat(raw);
        assert_eq!(adds, 15);
        assert_eq!(dels, 2);
        assert_eq!(files, 3);
    }

    #[test]
    fn checkout_scope_overrides_env_or_handler_scope_and_drops_mismatch() {
        let local_workspace = Uuid::new_v4();
        let local_project = Uuid::new_v4();
        let mut params = CaptureVcsLocalEventParams {
            workspace_id: Some(Uuid::new_v4()),
            project_id: Some(Uuid::new_v4()),
            ..Default::default()
        };

        constrain_event_scope(&mut params, local_workspace, Some(local_project));
        assert_eq!(params.workspace_id, Some(local_workspace));
        assert_eq!(params.project_id, Some(local_project));

        constrain_event_scope(&mut params, local_workspace, None);
        assert_eq!(params.workspace_id, Some(local_workspace));
        assert_eq!(params.project_id, None);
    }

    #[test]
    fn env_truthy_parses_common_forms() {
        assert_eq!(env_truthy("off"), Some(false));
        assert_eq!(env_truthy("ON"), Some(true));
        assert_eq!(env_truthy("1"), Some(true));
        assert_eq!(env_truthy("maybe"), None);
    }

    #[test]
    fn capture_disabled_honors_env_kill_switch() {
        let _guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("CONTEXTSTREAM_GIT_CAPTURE").ok();

        std::env::set_var("CONTEXTSTREAM_GIT_CAPTURE", "off");
        assert!(capture_disabled("/nonexistent/repo"));

        std::env::set_var("CONTEXTSTREAM_GIT_CAPTURE", "on");
        assert!(!capture_disabled("/nonexistent/repo"));

        match prev {
            Some(v) => std::env::set_var("CONTEXTSTREAM_GIT_CAPTURE", v),
            None => std::env::remove_var("CONTEXTSTREAM_GIT_CAPTURE"),
        }
    }

    #[test]
    fn per_repo_config_overrides_default_and_filters_events() {
        let _guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("CONTEXTSTREAM_GIT_CAPTURE").ok();
        std::env::remove_var("CONTEXTSTREAM_GIT_CAPTURE");

        let temp = tempfile::tempdir().unwrap();
        let cfg_dir = temp.path().join(".contextstream");
        std::fs::create_dir_all(&cfg_dir).unwrap();
        std::fs::write(
            cfg_dir.join("config.json"),
            serde_json::json!({
                "git_capture": { "enabled": true, "events": ["commit", "push"] }
            })
            .to_string(),
        )
        .unwrap();

        let root = temp.path().to_str().unwrap();
        assert!(!capture_disabled(root));
        assert!(event_enabled(root, "commit"));
        assert!(event_enabled(root, "push"));
        assert!(!event_enabled(root, "checkout"));
        assert!(should_capture(root, "commit"));
        assert!(!should_capture(root, "merge"));

        // Disabling via config stops capture even with an events list present.
        std::fs::write(
            cfg_dir.join("config.json"),
            serde_json::json!({ "git_capture": { "enabled": false } }).to_string(),
        )
        .unwrap();
        assert!(capture_disabled(root));

        match prev {
            Some(v) => std::env::set_var("CONTEXTSTREAM_GIT_CAPTURE", v),
            None => std::env::remove_var("CONTEXTSTREAM_GIT_CAPTURE"),
        }
    }

    #[test]
    fn session_hint_round_trips_and_expires() {
        let _guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        let prev_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", temp.path());

        let root = "/some/repo/path";
        write_session_hint(root, Some("sess-123"), Some("claude_code"));
        let hint = recent_session_hint(root).expect("fresh hint");
        assert_eq!(hint.session_id.as_deref(), Some("sess-123"));
        assert_eq!(hint.agent.as_deref(), Some("claude_code"));

        // A hint timestamped outside the TTL window must be ignored.
        let path = session_hint_path(root).unwrap();
        std::fs::write(
            &path,
            serde_json::json!({
                "session_id": "old",
                "agent": "claude_code",
                "ts": (chrono::Utc::now() - chrono::Duration::seconds(SESSION_HINT_TTL_SECS + 60)).to_rfc3339(),
            })
            .to_string(),
        )
        .unwrap();
        assert!(recent_session_hint(root).is_none());

        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
    }

    #[tokio::test]
    async fn collect_commit_info_reads_a_real_commit() {
        use std::process::Command as StdCommand;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_str().unwrap();
        let git = |args: &[&str]| {
            let ok = StdCommand::new("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .env("GIT_OPTIONAL_LOCKS", "0")
                .output()
                .expect("git runs")
                .status
                .success();
            assert!(ok, "git {:?} failed", args);
        };

        git(&["init", "-q"]);
        git(&["config", "user.email", "ada@example.com"]);
        git(&["config", "user.name", "Ada Lovelace"]);
        std::fs::write(temp.path().join("a.txt"), "one\ntwo\n").unwrap();
        git(&["add", "a.txt"]);
        git(&["commit", "-q", "-m", "Initial commit"]);
        git(&["branch", "-M", "main"]);
        // Second commit so HEAD has a parent and numstat is meaningful.
        std::fs::write(temp.path().join("a.txt"), "one\ntwo\nthree\n").unwrap();
        git(&["commit", "-qam", "Add third line"]);

        let info = collect_commit_info(root).await.expect("commit info");
        assert_eq!(info.sha.len(), 40, "full sha expected");
        assert_eq!(info.branch.as_deref(), Some("main"));
        assert!(info.message.as_deref().unwrap().contains("Add third line"));
        assert!(info.committed_at.is_some());
        assert_eq!(info.additions, Some(1));
        assert_eq!(info.deletions, Some(0));
        assert_eq!(info.files_changed, Some(1));
    }
}
