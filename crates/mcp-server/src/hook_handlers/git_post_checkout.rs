//! `post-checkout` git hook handler.
//!
//! Records a `branch.checkout` event for branch switches. Git invokes
//! post-checkout for file checkouts too (`branch_flag == 0`); those are noise
//! and are skipped. argv forwarded by the hook script is `<prev_head>
//! <new_head> <branch_flag>`. Fail-open throughout.

use anyhow::Result;
use mcp_client::CaptureVcsLocalEventParams;

use super::{git_common, write_stdout_json, HookOutput};

pub async fn handle() -> Result<()> {
    if let Some(root) = git_common::repo_root().await {
        if git_common::should_capture(&root, "checkout") {
            let args = git_common::hook_args();
            let prev_head = args.first().map(String::as_str).unwrap_or("");
            let new_head = args.get(1).map(String::as_str).unwrap_or("");
            let branch_flag = args.get(2).map(String::as_str).unwrap_or("0");

            // Only branch checkouts (flag == 1) are interesting; flag == 0 is a
            // file/path checkout and is pure noise.
            if branch_flag == "1" {
                let branch = git_common::current_branch(&root).await;
                // `git checkout -b` / `git switch -c` leave HEAD pointing at the
                // same commit (prev == new) while creating a new branch ref.
                let created = !prev_head.is_empty() && prev_head == new_head;
                let message = branch.as_deref().map(|b| {
                    if created {
                        format!("Created and switched to branch {}", b)
                    } else {
                        format!("Switched to branch {}", b)
                    }
                });
                let sha = (!new_head.is_empty() && !new_head.chars().all(|c| c == '0'))
                    .then(|| new_head.to_string());

                let params = CaptureVcsLocalEventParams {
                    event_type: git_common::EVENT_CHECKOUT.to_string(),
                    sha,
                    branch,
                    message,
                    ..Default::default()
                };
                git_common::capture(&root, params).await;
            }
        }
    }

    write_stdout_json(&HookOutput::empty())?;
    Ok(())
}
