//! Team-aware setup guidance and tips for the setup wizard.

use mcp_types::AccountContextSnapshot;

use super::ui::{self, ui, Mark, GUTTER};

/// Print team capability guidance immediately after successful authentication.
pub fn print_post_auth_team_guidance(ctx: &AccountContextSnapshot) {
    if !ctx.team_features_available() {
        return;
    }
    let ui = ui();

    let mut detail = Vec::new();
    if let Some(name) = ctx.team_name.as_deref() {
        detail.push(name.to_string());
    }
    if let Some(plan) = ctx.effective_plan.as_deref() {
        detail.push(format!("{plan} plan"));
    }
    detail.push("shared memory, skills, and tickets".to_string());
    ui::say(Mark::Tip, "Team account", Some(&detail.join(" · ")));

    if ctx.is_dual_context() {
        println!(
            "{GUTTER}    {} {} {} {}",
            ui.muted("Switch between team and personal mode with"),
            ui.path("session(action=\"set_account_mode\")"),
            ui.muted("or"),
            ui.path("CONTEXTSTREAM_ACCOUNT_MODE=team|personal|auto")
        );
    }
    println!(
        "{GUTTER}    {} {}",
        ui.muted("Team guide ·"),
        ui.path("https://contextstream.io/docs/team")
    );
}

/// Tips shown during workspace/project selection for team-capable accounts.
pub fn print_workspace_step_team_tips() {
    ui::say(
        Mark::Tip,
        "Use the workspace your teammates share",
        Some("and the project that maps to this repo, so everyone gets the same context"),
    );
}

/// Team-specific next steps appended to the setup success banner.
pub fn print_team_success_next_steps() {
    let ui = ui();
    println!("{GUTTER}{}", ui.kicker("Team"));
    for (text, command) in [
        (
            "Share a skill with your team",
            "skill(action=\"share\", scope=\"team\")",
        ),
        (
            "Find shared skills",
            "skill(action=\"list\", scope=\"team\")",
        ),
        ("Pull team context each turn", "session(action=\"context\")"),
        (
            "File or assign a ticket",
            "entity(kind=\"ticket\", action=\"create\", ...)",
        ),
    ] {
        println!("{GUTTER}{} {} {}", ui.faint("·"), text, ui.path(command));
    }
    println!();
    println!("{GUTTER}{}", ui.kicker("Refresh from CI or scripts"));
    for command in [
        "contextstream-mcp update-hooks --scope=global",
        "contextstream-mcp update-rules --scope=all",
        "contextstream-mcp migrate-remote --scope=all",
    ] {
        println!("{GUTTER}{}", ui.path(command));
    }
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dual_context_snapshot_is_team_capable() {
        let ctx = AccountContextSnapshot {
            account_type: "dual".to_string(),
            has_team_membership: true,
            ..Default::default()
        };
        assert!(ctx.team_features_available());
        assert!(ctx.is_dual_context());
    }
}
