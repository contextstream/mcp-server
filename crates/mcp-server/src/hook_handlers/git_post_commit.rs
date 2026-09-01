//! `post-commit` git hook handler.
//!
//! Records a `commit.local` VCS event for the just-created HEAD commit.
//! Fail-open throughout: any problem results in a quiet no-op so git is never
//! affected.

use anyhow::Result;
use mcp_client::CaptureVcsLocalEventParams;

use super::{git_common, write_stdout_json, HookOutput};

pub async fn handle() -> Result<()> {
    if let Some(root) = git_common::repo_root().await {
        if git_common::should_capture(&root, "commit") {
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
                    ..Default::default()
                };
                git_common::capture(&root, params).await;
            }
        }
    }

    write_stdout_json(&HookOutput::empty())?;
    Ok(())
}
