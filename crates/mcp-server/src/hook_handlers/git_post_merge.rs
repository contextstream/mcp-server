//! `post-merge` git hook handler.
//!
//! Records a `merge.local` event after a successful merge (or `git pull` merge).
//! argv forwarded by the hook script is `<squash_flag>`. Fail-open throughout.

use anyhow::Result;
use mcp_client::CaptureVcsLocalEventParams;

use super::{git_common, write_stdout_json, HookOutput};

pub async fn handle() -> Result<()> {
    if let Some(root) = git_common::repo_root().await {
        if git_common::should_capture(&root, "merge") {
            // HEAD now points at the merge commit (or unchanged for a squash
            // merge, which only stages). Collect what we can either way.
            let info = git_common::collect_commit_info(&root).await;
            let branch = git_common::current_branch(&root).await;

            let params = CaptureVcsLocalEventParams {
                event_type: git_common::EVENT_MERGE.to_string(),
                sha: info.as_ref().map(|i| i.sha.clone()),
                message: info.as_ref().and_then(|i| i.message.clone()),
                committed_at: info.as_ref().and_then(|i| i.committed_at.clone()),
                additions: info.as_ref().and_then(|i| i.additions),
                deletions: info.as_ref().and_then(|i| i.deletions),
                files_changed: info.as_ref().and_then(|i| i.files_changed),
                branch,
                ..Default::default()
            };
            git_common::capture(&root, params).await;
        }
    }

    write_stdout_json(&HookOutput::empty())?;
    Ok(())
}
