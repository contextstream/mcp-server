//! `pre-push` git hook handler.
//!
//! Records a `push.local` event. The managed hook script captures the ref list
//! from stdin first (so git is not blocked) and forwards `<remote-name>
//! <remote-url>` as argv. stdin lines are `<local_ref> <local_sha> <remote_ref>
//! <remote_sha>`. Fail-open throughout.
//!
//! The hook runs before the remote accepts the push, so a rejected push is still
//! recorded: the event is evidence that a push was attempted, not that it
//! succeeded. It names the tip commit (`sha`) and, up to a cap, the commits the
//! push carries (`pushed_commits`, from `git rev-list`), so that a commit inside
//! a pushed range counts as pushed too, not only the newest one.

use anyhow::Result;
use mcp_client::checkout_snapshot::compute_repo_key;
use mcp_client::command_hints;
use mcp_client::verification_gates::cached_gate_policy;
use mcp_client::CaptureVcsLocalEventParams;
use std::path::Path;
use std::time::Duration;

use super::{common, git_common, post_tool_use, verification_gate, write_stdout_json, HookOutput};

/// Most commits one event names, across all refs.
const MAX_PUSHED_COMMITS: usize = 50;
/// Most refs whose range is worked out.
const MAX_RANGE_REFS: usize = 4;
/// Time for one `git rev-list`. Tests run on loaded machines, so in a test
/// build the budgets are long: they check what is named, not how fast.
const REV_LIST_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_secs(30)
} else {
    Duration::from_millis(800)
};
/// Time for all of them together: git is waiting on this hook.
const RANGE_BUDGET: Duration = if cfg!(test) {
    Duration::from_secs(60)
} else {
    Duration::from_millis(1500)
};

/// Parse pre-push stdin into (pushed remote refs, tip sha). Deletions (a
/// local sha of all zeros) are skipped.
fn parse_push_refs(stdin: &str) -> (Vec<String>, Option<String>) {
    let mut pushed_refs = Vec::new();
    let mut tip_sha = None;
    for line in stdin.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() < 4 {
            continue;
        }
        let local_sha = cols[1];
        let remote_ref = cols[2];
        if local_sha.chars().all(|c| c == '0') {
            continue; // ref deletion — nothing pushed
        }
        pushed_refs.push(remote_ref.to_string());
        if tip_sha.is_none() {
            tip_sha = Some(local_sha.to_string());
        }
    }
    (pushed_refs, tip_sha)
}

/// One ref update from pre-push stdin.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PushUpdate {
    local_sha: String,
    /// All zeros when the remote does not have the ref yet.
    remote_sha: String,
}

/// A full git object id: 40 hex characters (SHA-1) or 64 (SHA-256).
fn is_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// The ref updates that push something: deletions and lines whose shas are not
/// object ids are skipped, so nothing odd reaches a `git` argument.
fn parse_push_updates(stdin: &str) -> Vec<PushUpdate> {
    stdin
        .lines()
        .filter_map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            let (local_sha, remote_sha) = (*cols.get(1)?, *cols.get(3)?);
            (cols.len() >= 4
                && is_object_id(local_sha)
                && is_object_id(remote_sha)
                && !local_sha.bytes().all(|byte| byte == b'0'))
            .then(|| PushUpdate {
                local_sha: local_sha.to_ascii_lowercase(),
                remote_sha: remote_sha.to_ascii_lowercase(),
            })
        })
        .collect()
}

/// A remote name that is safe to put in a `--remotes=` pattern. A push to a URL
/// has no name, and the pattern is a glob.
fn remote_name_is_plain(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// The `git rev-list` arguments for the commits one ref update carries, or
/// `None` when they cannot be told without claiming too much.
///
/// An existing ref carries `remote..local`. A new ref carries what the remote's
/// known refs do not have, which needs the remote's name; without it the whole
/// history would look pushed.
fn rev_list_args(update: &PushUpdate, remote_name: Option<&str>) -> Option<Vec<String>> {
    let limit = format!("--max-count={MAX_PUSHED_COMMITS}");
    if update.remote_sha.bytes().all(|byte| byte == b'0') {
        let name = remote_name.filter(|name| remote_name_is_plain(name))?;
        Some(vec![
            "rev-list".to_string(),
            limit,
            update.local_sha.clone(),
            "--not".to_string(),
            format!("--remotes={name}"),
        ])
    } else {
        Some(vec![
            "rev-list".to_string(),
            limit,
            format!("{}..{}", update.remote_sha, update.local_sha),
        ])
    }
}

/// Add the object ids in `rev-list` output to `into`: each once, in order, up
/// to the cap. Lines that are not object ids are ignored.
fn collect_rev_list(output: &str, into: &mut Vec<String>) {
    for line in output.lines() {
        if into.len() >= MAX_PUSHED_COMMITS {
            break;
        }
        let sha = line.trim();
        if is_object_id(sha) {
            let sha = sha.to_ascii_lowercase();
            if !into.contains(&sha) {
                into.push(sha);
            }
        }
    }
}

/// The commits this push carries, newest first, within the cap and the time
/// budget. Empty when none can be worked out (an unknown remote tip, a missing
/// object, a timeout): the event then names only its tip, as before.
async fn pushed_commit_range(
    root: &str,
    updates: &[PushUpdate],
    remote_name: Option<&str>,
) -> Vec<String> {
    let work = async {
        let mut commits = Vec::new();
        for update in updates.iter().take(MAX_RANGE_REFS) {
            if commits.len() >= MAX_PUSHED_COMMITS {
                break;
            }
            let Some(args) = rev_list_args(update, remote_name) else {
                continue;
            };
            let args: Vec<&str> = args.iter().map(String::as_str).collect();
            if let Some(output) = git_common::run_git(root, &args, REV_LIST_TIMEOUT).await {
                collect_rev_list(&output, &mut commits);
            }
        }
        commits
    };
    tokio::time::timeout(RANGE_BUDGET, work)
        .await
        .unwrap_or_default()
}

/// The event for one push: the tip, the branch, the refs, the commits it
/// carries and the remote. `args` is what the hook script forwards:
/// `[remote_name, remote_url]`.
async fn push_event_params(root: &str, stdin: &str, args: &[String]) -> CaptureVcsLocalEventParams {
    let remote_url = args.get(1).cloned().filter(|s| !s.trim().is_empty());
    let remote_name = args
        .first()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty());

    let (pushed_refs, mut tip_sha) = parse_push_refs(stdin);
    if tip_sha.is_none() {
        // No usable stdin (e.g. a chained user hook consumed it): fall back to
        // the current HEAD so the push is still recorded.
        tip_sha = git_common::run_git(
            root,
            &["rev-parse", "HEAD"],
            std::time::Duration::from_millis(800),
        )
        .await
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    }
    let pushed_commits =
        pushed_commit_range(root, &parse_push_updates(stdin), remote_name.as_deref()).await;

    CaptureVcsLocalEventParams {
        event_type: git_common::EVENT_PUSH.to_string(),
        sha: tip_sha,
        branch: git_common::current_branch(root).await,
        pushed_refs: (!pushed_refs.is_empty()).then_some(pushed_refs),
        pushed_commits: (!pushed_commits.is_empty()).then_some(pushed_commits),
        // Prefer the pushed remote's URL over origin when provided.
        remote_url,
        ..Default::default()
    }
}

async fn resolve_push_target_trees(root: &str, stdin: &str) -> Vec<String> {
    let updates = parse_push_updates(stdin);
    let mut trees = Vec::new();
    if !updates.is_empty() {
        for update in updates {
            let spec = format!("{}^{{tree}}", update.local_sha);
            if let Some(tree) = git_common::run_git(
                root,
                &["rev-parse", "-q", "--verify", &spec],
                Duration::from_millis(800),
            )
            .await
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| is_object_id(s))
            {
                if !trees.contains(&tree) {
                    trees.push(tree);
                }
            }
        }
    } else if stdin.trim().is_empty() {
        if let Some(tree) = git_common::run_git(
            root,
            &["rev-parse", "-q", "--verify", "HEAD^{tree}"],
            Duration::from_millis(800),
        )
        .await
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| is_object_id(s))
        {
            trees.push(tree);
        }
    }
    trees
}

/// Enforce `require_verified_tree_before_push` for the ref updates in `stdin`.
/// Returns `Some(error_message)` when the push must be blocked, or `None` to
/// allow (including fail-open on offline/404/timeout or `CONTEXTSTREAM_HOSTED_TOOLS=off`).
pub async fn enforce_push_gate(root: &str, stdin: &str) -> Option<String> {
    if command_hints::switched_off(std::env::var(command_hints::ENV_SWITCH).ok().as_deref()) {
        return None;
    }

    let (api_url, api_key, workspace_id, project_id) =
        if let Some(cfg) = post_tool_use::find_project_config(root, root) {
            (
                cfg.api_url,
                cfg.api_key,
                Some(cfg.workspace_id.to_string()),
                Some(cfg.project_id.to_string()),
            )
        } else {
            let cfg = common::load_config(root);
            if !cfg.is_configured() {
                return None;
            }
            (cfg.api_url, cfg.api_key, cfg.workspace_id, cfg.project_id)
        };

    if api_key.is_empty() || (workspace_id.is_none() && project_id.is_none()) {
        return None;
    }

    let identity = command_hints::local_identity(&api_key);
    let client = verification_gate::build_client(
        &api_url,
        &api_key,
        workspace_id.as_deref(),
        project_id.as_deref(),
    );
    let policy_resp = cached_gate_policy(
        &client,
        &identity,
        workspace_id.as_deref(),
        project_id.as_deref(),
        verification_gate::GATE_CHECK_TIMEOUT,
    )
    .await?;
    if !policy_resp.policy.require_verified_tree_before_push {
        return None;
    }

    let trees = resolve_push_target_trees(root, stdin).await;
    if trees.is_empty() {
        return None;
    }
    let repo_key = compute_repo_key(Path::new(root)).await;
    for tree in trees {
        let resp = match client
            .check_receipts(
                &tree,
                "push",
                workspace_id.as_deref(),
                project_id.as_deref(),
                repo_key.as_deref(),
                verification_gate::GATE_CHECK_TIMEOUT,
            )
            .await
        {
            Ok(Some(resp)) => resp,
            _ => continue,
        };
        if resp.gate_enabled && !resp.allowed {
            return Some(verification_gate::format_block_message("push", &resp));
        }
    }
    None
}

pub async fn handle() -> Result<()> {
    if let Some(root) = git_common::repo_root().await {
        let stdin = git_common::read_stdin_raw();
        let args = git_common::hook_args();
        if git_common::should_capture(&root, "push") {
            let params = push_event_params(&root, &stdin, &args).await;
            git_common::capture(&root, params).await;
        }
        if let Some(msg) = enforce_push_gate(&root, &stdin).await {
            eprintln!("{msg}");
            std::process::exit(1);
        }
    }

    write_stdout_json(&HookOutput::empty())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const ZERO: &str = "0000000000000000000000000000000000000000";

    #[test]
    fn parses_refs_and_tip_skipping_deletions() {
        let stdin = "\
refs/heads/main 1111111111111111111111111111111111111111 refs/heads/main 0000000000000000000000000000000000000000
refs/heads/dead 0000000000000000000000000000000000000000 refs/heads/dead 2222222222222222222222222222222222222222
";
        let (refs, tip) = parse_push_refs(stdin);
        assert_eq!(refs, vec!["refs/heads/main".to_string()]);
        assert_eq!(
            tip.as_deref(),
            Some("1111111111111111111111111111111111111111")
        );
    }

    #[test]
    fn empty_stdin_yields_nothing() {
        let (refs, tip) = parse_push_refs("");
        assert!(refs.is_empty());
        assert!(tip.is_none());
    }

    #[test]
    fn push_updates_skip_deletions_and_anything_that_is_not_an_object_id() {
        let new_sha = "a".repeat(40);
        let old_sha = "B".repeat(40);
        let stdin = format!(
            "refs/heads/main {new_sha} refs/heads/main {old_sha}\n\
             refs/heads/gone {ZERO} refs/heads/gone {old_sha}\n\
             refs/heads/odd --upload-pack=x refs/heads/odd {old_sha}\n\
             refs/heads/short abc123 refs/heads/short {old_sha}\n\
             too few columns\n"
        );
        assert_eq!(
            parse_push_updates(&stdin),
            vec![PushUpdate {
                local_sha: new_sha,
                remote_sha: "b".repeat(40),
            }]
        );
        assert!(parse_push_updates("").is_empty());
    }

    #[test]
    fn an_existing_ref_carries_the_range_after_the_remote_tip() {
        let update = PushUpdate {
            local_sha: "c".repeat(40),
            remote_sha: "a".repeat(40),
        };
        let args = rev_list_args(&update, None).expect("a range needs no remote name");
        assert_eq!(
            args,
            vec![
                "rev-list".to_string(),
                "--max-count=50".to_string(),
                format!("{}..{}", "a".repeat(40), "c".repeat(40)),
            ]
        );
    }

    #[test]
    fn a_new_ref_carries_what_the_remote_does_not_have_and_needs_the_remote_name() {
        let update = PushUpdate {
            local_sha: "e".repeat(40),
            remote_sha: ZERO.to_string(),
        };
        assert_eq!(
            rev_list_args(&update, Some("origin")).unwrap(),
            vec![
                "rev-list".to_string(),
                "--max-count=50".to_string(),
                "e".repeat(40),
                "--not".to_string(),
                "--remotes=origin".to_string(),
            ]
        );
        // Without a usable name the whole history would look pushed.
        for name in [
            None,
            Some(""),
            Some("--all"),
            Some("https://example.local/repo.git"),
            Some("or*gin"),
            Some("a b"),
        ] {
            assert_eq!(rev_list_args(&update, name), None, "{name:?}");
        }
    }

    #[test]
    fn rev_list_output_is_deduplicated_ordered_bounded_and_cleaned() {
        let mut commits = Vec::new();
        collect_rev_list(
            &format!(
                "{}\n{}\nnot a sha\n{}\n\n{}\n",
                "A".repeat(40),
                "b".repeat(40),
                "a".repeat(40),
                "c".repeat(7)
            ),
            &mut commits,
        );
        assert_eq!(commits, vec!["a".repeat(40), "b".repeat(40)]);

        let many: String = (0..MAX_PUSHED_COMMITS + 20)
            .map(|n| format!("{n:040x}\n"))
            .collect();
        let mut capped = Vec::new();
        collect_rev_list(&many, &mut capped);
        assert_eq!(capped.len(), MAX_PUSHED_COMMITS);
        assert_eq!(capped[0], format!("{:040x}", 0));
    }

    // ---- against a real repository -------------------------------------

    fn git(dir: &Path, args: &[&str]) -> String {
        let mut command = std::process::Command::new("git");
        command
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
            .env("GIT_CONFIG_SYSTEM", "/dev/null");
        // Starting a process can fail for a moment when the test binary has
        // many threads and open files and the machine is busy: try again.
        let mut attempt = 0;
        let output = loop {
            match command.output() {
                Ok(output) => break output,
                Err(error) if attempt < 8 => {
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(100 * attempt));
                    drop(error);
                }
                Err(error) => panic!("could not run git {args:?}: {error}"),
            }
        };
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn commit(dir: &Path, message: &str) -> String {
        git(dir, &["commit", "-q", "--allow-empty", "-m", message]);
        git(dir, &["rev-parse", "HEAD"])
    }

    /// A repository with a bare `origin` that already has the first commit.
    fn repository_with_remote() -> (tempfile::TempDir, String, String) {
        let temp = tempfile::tempdir().expect("tempdir");
        let work = temp.path().join("work");
        let remote = temp.path().join("remote.git");
        std::fs::create_dir_all(&work).unwrap();
        git(
            temp.path(),
            &["init", "-q", "--bare", remote.to_str().unwrap()],
        );
        git(&work, &["init", "-q", "-b", "main"]);
        git(
            &work,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        let first = commit(&work, "first");
        git(&work, &["push", "-q", "origin", "main"]);
        (temp, work.to_string_lossy().to_string(), first)
    }

    #[tokio::test]
    async fn a_push_to_an_existing_ref_names_every_commit_after_the_remote_tip() {
        let (_temp, work, first) = repository_with_remote();
        let work_dir = Path::new(&work);
        let second = commit(work_dir, "second");
        let third = commit(work_dir, "third");
        let updates = vec![PushUpdate {
            local_sha: third.clone(),
            remote_sha: first.clone(),
        }];

        let range = pushed_commit_range(&work, &updates, Some("origin")).await;
        assert_eq!(
            range,
            vec![third, second],
            "newest first, without the remote tip"
        );
        assert!(!range.contains(&first));
    }

    #[tokio::test]
    async fn a_new_branch_names_only_the_commits_the_remote_does_not_have() {
        let (_temp, work, first) = repository_with_remote();
        let work_dir = Path::new(&work);
        git(work_dir, &["checkout", "-q", "-b", "feature"]);
        let fourth = commit(work_dir, "fourth");
        let fifth = commit(work_dir, "fifth");
        let updates = vec![PushUpdate {
            local_sha: fifth.clone(),
            remote_sha: ZERO.to_string(),
        }];

        let range = pushed_commit_range(&work, &updates, Some("origin")).await;
        assert_eq!(range, vec![fifth, fourth]);
        assert!(!range.contains(&first), "origin/main already has it");

        // A push to a URL has no remote name: nothing is claimed.
        assert!(pushed_commit_range(&work, &updates, None).await.is_empty());
        assert!(
            pushed_commit_range(&work, &updates, Some("https://example.local/r.git"))
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn a_remote_tip_this_repository_never_had_names_nothing() {
        let (_temp, work, _first) = repository_with_remote();
        let tip = git(Path::new(&work), &["rev-parse", "HEAD"]);
        let updates = vec![PushUpdate {
            local_sha: tip,
            remote_sha: "9".repeat(40),
        }];
        assert!(pushed_commit_range(&work, &updates, Some("origin"))
            .await
            .is_empty());
        // And a path that is not a repository is not an error either.
        assert!(
            pushed_commit_range("/definitely/not/a/repo", &updates, None)
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn the_push_event_carries_the_tip_the_refs_the_commits_and_the_remote() {
        let (_temp, work, first) = repository_with_remote();
        let work_dir = Path::new(&work);
        let second = commit(work_dir, "second");
        let third = commit(work_dir, "third");
        let stdin = format!("refs/heads/main {third} refs/heads/main {first}\n");
        let args = vec![
            "origin".to_string(),
            "https://example.local/org/repo.git".to_string(),
        ];

        let params = push_event_params(&work, &stdin, &args).await;
        assert_eq!(params.event_type, "push.local");
        assert_eq!(params.sha.as_deref(), Some(third.as_str()));
        assert_eq!(params.branch.as_deref(), Some("main"));
        assert_eq!(
            params.pushed_refs,
            Some(vec!["refs/heads/main".to_string()])
        );
        assert_eq!(params.pushed_commits, Some(vec![third.clone(), second]));
        assert_eq!(
            params.remote_url.as_deref(),
            Some("https://example.local/org/repo.git")
        );

        // No usable stdin: the tip falls back to HEAD, and no range is claimed.
        let params = push_event_params(&work, "", &args).await;
        assert_eq!(params.sha.as_deref(), Some(third.as_str()));
        assert_eq!(params.pushed_commits, None);
        assert_eq!(params.pushed_refs, None);
    }

    #[tokio::test]
    async fn a_long_range_is_capped_at_the_newest_commits() {
        let (_temp, work, first) = repository_with_remote();
        let work_dir = Path::new(&work);
        let mut newest = String::new();
        for n in 0..MAX_PUSHED_COMMITS + 10 {
            newest = commit(work_dir, &format!("commit {n}"));
        }
        let updates = vec![PushUpdate {
            local_sha: newest.clone(),
            remote_sha: first,
        }];
        let range = pushed_commit_range(&work, &updates, Some("origin")).await;
        assert_eq!(range.len(), MAX_PUSHED_COMMITS);
        assert_eq!(range[0], newest, "the tip comes first");
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn enforce_push_gate_blocks_unverified_tree_allows_verified_and_fails_open() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let _guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let prev_home = std::env::var_os("HOME");
        let prev_switch = std::env::var_os(command_hints::ENV_SWITCH);
        let temp_home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", temp_home.path());
        std::env::remove_var(command_hints::ENV_SWITCH);

        let (_temp, work, first) = repository_with_remote();
        let work_dir = Path::new(&work);
        let second = commit(work_dir, "second");
        let second_tree = git(work_dir, &["rev-parse", &format!("{second}^{{tree}}")]);

        let ws = uuid::Uuid::new_v4();
        let proj = uuid::Uuid::new_v4();
        let fingerprint =
            mcp_session::checkout_identity::ensure_repository_fingerprint(work_dir).unwrap();
        let canonical_root = std::fs::canonicalize(work_dir)
            .unwrap()
            .to_string_lossy()
            .into_owned();
        std::fs::create_dir_all(work_dir.join(".contextstream")).unwrap();
        std::fs::write(
            work_dir.join(".contextstream/config.json"),
            serde_json::json!({
                "workspace_id": ws.to_string(),
                "project_id": proj.to_string(),
                "checkout_root": canonical_root,
                "repository_fingerprint": fingerprint.as_str(),
            })
            .to_string(),
        )
        .unwrap();

        async fn spawn_server(
            ws: uuid::Uuid,
            proj: uuid::Uuid,
            tree_clone: String,
            allowed: bool,
            status_line: &'static str,
        ) -> String {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            tokio::spawn(async move {
                for _ in 0..2 {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        break;
                    };
                    let mut buf = [0u8; 4096];
                    let n = socket.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let body = if req.contains("/gate-policy") {
                        serde_json::json!({
                            "success": true,
                            "data": {
                                "workspace_id": ws.to_string(),
                                "project_id": proj.to_string(),
                                "inherited": false,
                                "policy": {
                                    "require_verified_tree_before_push": true,
                                    "required_command_families": ["cargo test"],
                                    "receipt_ttl_secs": 3600
                                }
                            }
                        })
                        .to_string()
                    } else {
                        serde_json::json!({
                            "success": true,
                            "data": {
                                "allowed": allowed,
                                "gate": "push",
                                "gate_enabled": true,
                                "workspace_id": ws.to_string(),
                                "project_id": proj.to_string(),
                                "repo_key": "work-12345678",
                                "target_tree": tree_clone,
                                "ttl_secs": 3600,
                                "missing_command_families": if allowed { Vec::<String>::new() } else { vec!["cargo test".to_string()] },
                                "required_command_families": ["cargo test"]
                            }
                        })
                        .to_string()
                    };
                    let resp = format!(
                        "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                    let _ = socket.shutdown().await;
                }
            });
            format!("http://{addr}")
        }

        let write_mcp = |url: &str, key: &str| {
            std::fs::write(
                work_dir.join(".mcp.json"),
                serde_json::json!({
                    "mcpServers": {
                        "contextstream": {
                            "env": {
                                "CONTEXTSTREAM_API_KEY": key,
                                "CONTEXTSTREAM_API_URL": url
                            }
                        }
                    }
                })
                .to_string(),
            )
            .unwrap();
        };

        let stdin = format!("refs/heads/main {second} refs/heads/main {first}\n");

        // 1. Unverified tree is blocked with actionable message.
        let url_block = spawn_server(ws, proj, second_tree.clone(), false, "200 OK").await;
        write_mcp(&url_block, "key-block");
        let blocked = enforce_push_gate(&work, &stdin)
            .await
            .expect("unverified push must block");
        assert!(blocked.contains("push"), "{blocked}");
        assert!(blocked.contains("cargo test"), "{blocked}");
        assert!(
            blocked.contains("contextstream-mcp run -- cargo test"),
            "{blocked}"
        );

        // 2. Verified tree is allowed.
        let url_allow = spawn_server(ws, proj, second_tree.clone(), true, "200 OK").await;
        write_mcp(&url_allow, "key-allow");
        assert_eq!(enforce_push_gate(&work, &stdin).await, None);

        // 3. Deletion-only stdin is never blocked.
        let deletion_stdin = format!("refs/heads/old {ZERO} refs/heads/old {first}\n");
        let url_del = spawn_server(ws, proj, second_tree.clone(), false, "200 OK").await;
        write_mcp(&url_del, "key-del");
        assert_eq!(enforce_push_gate(&work, &deletion_stdin).await, None);

        // 4. 404 / offline fails open.
        let url_404 = spawn_server(ws, proj, second_tree.clone(), false, "404 Not Found").await;
        write_mcp(&url_404, "key-404");
        assert_eq!(enforce_push_gate(&work, &stdin).await, None);

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
