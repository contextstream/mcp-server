//! The one-time "indexing is paused: out of credits" notice.
//!
//! The sync bridge and the hook drain run in the background, where nobody reads
//! a log. When the API refuses their uploads for credits they record it in
//! `mcp_client::credit_hold`; the next prompt or session start in that project
//! tells the person here, once per hold, and the notice is marked told so no
//! later prompt repeats it. The record is cleared by the first accepted ingest,
//! so a later refusal is told again.

use mcp_client::credit_hold;

/// The notice to deliver now for the checkout at `cwd`, if its project has a
/// credit hold nobody was told about yet. Marks it told.
pub(crate) fn take(cwd: &str) -> Option<credit_hold::CreditHold> {
    // The common case, no hold anywhere on this machine, costs one stat.
    if !credit_hold::any_holds() || cwd.trim().is_empty() {
        return None;
    }
    let project_id = super::post_tool_use::find_project_config(cwd, cwd)?.project_id;
    credit_hold::take_unshown_notice(Some(project_id))
}

/// What the person reads (a hook's `systemMessage`, where the editor shows one).
pub(crate) fn user_message(hold: &credit_hold::CreditHold) -> String {
    format!("ContextStream: {}", credit_hold::notice_text(hold))
}

/// What the assistant is told, so the person hears it in the conversation even
/// where an editor shows no hook messages.
pub(crate) fn assistant_context(hold: &credit_hold::CreditHold) -> String {
    format!(
        "[CONTEXTSTREAM_NOTICE] {}\n\
         Tell the user this in your next reply, once, in plain words, with both links. \
         Do not try to re-index, and do not call project(action=\"index\") again until they say \
         credits were added.",
        credit_hold::notice_text(hold)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use uuid::Uuid;

    fn hold() -> credit_hold::CreditHold {
        credit_hold::CreditHold {
            project_id: Uuid::new_v4(),
            workspace_id: None,
            since: Utc::now(),
            last_refused_at: Utc::now(),
            retry_after: Utc::now(),
            streak: 1,
            required: Some(20),
            available: Some(3),
            notified_at: None,
        }
    }

    #[test]
    fn the_person_and_the_assistant_both_get_the_plain_message() {
        let hold = hold();
        let user = user_message(&hold);
        assert!(user.starts_with("ContextStream: "), "{user}");
        assert!(user.contains("out of credits"), "{user}");
        assert!(user.contains("https://contextstream.io/pricing"), "{user}");
        assert!(user.contains("resumes by itself"), "{user}");

        let assistant = assistant_context(&hold);
        assert!(
            assistant.starts_with("[CONTEXTSTREAM_NOTICE] "),
            "{assistant}"
        );
        assert!(assistant.contains("Tell the user this in your next reply, once"));
        assert!(assistant.contains("https://contextstream.io/account/billing"));
    }

    #[test]
    fn no_working_folder_means_no_notice() {
        assert!(take("").is_none());
        assert!(take("   ").is_none());
    }
}
