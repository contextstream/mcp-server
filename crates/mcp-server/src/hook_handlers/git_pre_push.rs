//! `pre-push` git hook handler.
//!
//! Records a `push.local` event. The managed hook script captures the ref list
//! from stdin first (so git is not blocked) and forwards `<remote-name>
//! <remote-url>` as argv. stdin lines are `<local_ref> <local_sha> <remote_ref>
//! <remote_sha>`. Fail-open throughout.

use anyhow::Result;
use mcp_client::CaptureVcsLocalEventParams;

use super::{git_common, write_stdout_json, HookOutput};

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

pub async fn handle() -> Result<()> {
    if let Some(root) = git_common::repo_root().await {
        if git_common::should_capture(&root, "push") {
            // argv forwarded by the hook script: [remote_name, remote_url]
            let args = git_common::hook_args();
            let remote_url = args.get(1).cloned().filter(|s| !s.trim().is_empty());

            let (pushed_refs, mut tip_sha) = parse_push_refs(&git_common::read_stdin_raw());
            if tip_sha.is_none() {
                // No usable stdin (e.g. a chained user hook consumed it): fall
                // back to the current HEAD so the push is still recorded.
                tip_sha = git_common::run_git(
                    &root,
                    &["rev-parse", "HEAD"],
                    std::time::Duration::from_millis(800),
                )
                .await
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            }

            let params = CaptureVcsLocalEventParams {
                event_type: git_common::EVENT_PUSH.to_string(),
                sha: tip_sha,
                branch: git_common::current_branch(&root).await,
                pushed_refs: (!pushed_refs.is_empty()).then_some(pushed_refs),
                // Prefer the pushed remote's URL over origin when provided.
                remote_url,
                ..Default::default()
            };
            git_common::capture(&root, params).await;
        }
    }

    write_stdout_json(&HookOutput::empty())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_push_refs;

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
}
