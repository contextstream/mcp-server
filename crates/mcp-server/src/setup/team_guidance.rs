//! Team-aware setup guidance and tips for the setup wizard.

use console::style;
use mcp_types::AccountContextSnapshot;

use super::CHECK;

/// Print team capability guidance immediately after successful authentication.
pub fn print_post_auth_team_guidance(ctx: &AccountContextSnapshot) {
    if !ctx.team_features_available() {
        return;
    }

    println!();
    println!(
        "{} {}",
        style("Team account detected").bold().cyan(),
        style("(shared workspace memory, skills, tickets, and context surfacing)").dim()
    );

    if ctx.is_dual_context() {
        println!(
            "  {} Dual-context account: switch between team and personal mode in your editor via",
            CHECK
        );
        println!(
            "     {} or {}",
            style("session(action=\"set_account_mode\", account_mode=\"team|personal|auto\")")
                .cyan(),
            style("CONTEXTSTREAM_ACCOUNT_MODE=team|personal|auto").cyan()
        );
    } else if let Some(name) = ctx.team_name.as_deref() {
        println!("  {} Team: {}", CHECK, style(name).cyan());
    }

    if let Some(plan) = ctx.effective_plan.as_deref() {
        println!("  {} Plan: {}", CHECK, style(plan).dim());
    }

    println!();
    println!("  {}", style("Team setup tips:").bold());
    println!(
        "    • Link this folder to your {} workspace (step 3)",
        style("shared team").cyan()
    );
    println!("    • Team skills surface in context via matched skills + governance cues");
    println!("    • Assign tickets, link docs/plans/handoffs with indexed refs (no URLs)");
    println!(
        "    • Docs: {}",
        style("https://contextstream.io/docs/team")
            .cyan()
            .underlined()
    );
    println!();
}

/// Tips shown during workspace/project selection for team-capable accounts.
pub fn print_workspace_step_team_tips() {
    println!();
    println!(
        "  {} {}",
        style("Team tip:").yellow().bold(),
        style("Pick the workspace your teammates share — decisions, skills, and tickets stay in sync.")
            .dim()
    );
    println!(
        "  {} Prefer the project that maps to this repo so everyone gets the same indexed context.",
        style("→").dim()
    );
    println!();
}

/// Team-specific next steps appended to the setup success banner.
pub fn print_team_success_next_steps() {
    println!("  {}", style("Team power-ups:").bold());
    println!("    • Share team skills: skill(action=\"share\", scope=\"team\")");
    println!("    • Discover shared skills: skill(action=\"list\", scope=\"team\")");
    println!("    • Team context each turn: session(action=\"context\")");
    println!("    • File/assign tickets: entity(kind=\"ticket\", action=\"create\", ...)");
    println!();
    println!(
        "  {}",
        style("Non-interactive refresh (CI/scripts):").bold()
    );
    println!(
        "    {}",
        style("contextstream-mcp update-hooks --scope=global").cyan()
    );
    println!(
        "    {}",
        style("contextstream-mcp update-rules --scope=all").cyan()
    );
    println!(
        "    {}",
        style("contextstream-mcp migrate-remote --scope=all").cyan()
    );
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
