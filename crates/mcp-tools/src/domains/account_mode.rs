//! Team vs personal mode resolution, scope defaults, and context surfacing.

use mcp_session::SessionManager;
use mcp_types::{
    AccountContextSnapshot, AccountContextSource, AccountModePreference, ExecutionMode,
    TeamPriorityItem, TranscriptTopicSignal,
};

#[derive(Debug, Clone, Default)]
pub struct ModeResolutionInput {
    pub startup_preference: AccountModePreference,
    pub tool_override: Option<AccountModePreference>,
    pub persisted_preference: AccountModePreference,
    pub account_context: Option<AccountContextSnapshot>,
    pub team_context_degraded: bool,
}

#[derive(Debug, Clone)]
pub struct ModeResolution {
    pub preference: AccountModePreference,
    pub execution_mode: ExecutionMode,
    pub note: Option<String>,
}

/// Deterministic precedence:
/// 1. explicit tool override (including `auto` to reset persisted mode)
/// 2. startup/env preference (non-auto)
/// 3. persisted session preference (non-auto)
/// 4. account-context selected_context / account_type default
/// 5. personal (safe default)
pub fn resolve_execution_mode(input: &ModeResolutionInput) -> ModeResolution {
    if input.team_context_degraded {
        return ModeResolution {
            preference: input.persisted_preference,
            execution_mode: ExecutionMode::Personal,
            note: Some(
                "Team context disabled due to account mismatch — using personal scope.".to_string(),
            ),
        };
    }

    let preference = if let Some(override_pref) = input.tool_override {
        override_pref
    } else if input.startup_preference != AccountModePreference::Auto {
        input.startup_preference
    } else if input.persisted_preference != AccountModePreference::Auto {
        input.persisted_preference
    } else {
        AccountModePreference::Auto
    };

    let (execution_mode, note) = match preference {
        AccountModePreference::Team => {
            if team_context_allows_team_mode(input.account_context.as_ref()) {
                (ExecutionMode::Team, None)
            } else {
                (
                    ExecutionMode::Personal,
                    Some(
                        "Team mode requested but account has no team membership — using personal scope."
                            .to_string(),
                    ),
                )
            }
        }
        AccountModePreference::Personal => (ExecutionMode::Personal, None),
        AccountModePreference::Auto => {
            let mode = default_execution_from_account(input.account_context.as_ref());
            (mode, None)
        }
    };

    ModeResolution {
        preference,
        execution_mode,
        note,
    }
}

fn team_context_allows_team_mode(ctx: Option<&AccountContextSnapshot>) -> bool {
    ctx.map(|c| c.team_features_available()).unwrap_or(false)
}

fn default_execution_from_account(ctx: Option<&AccountContextSnapshot>) -> ExecutionMode {
    let Some(ctx) = ctx else {
        return ExecutionMode::Personal;
    };
    if !ctx.team_features_available() {
        return ExecutionMode::Personal;
    }
    if ctx.selected_context.eq_ignore_ascii_case("team") {
        ExecutionMode::Team
    } else {
        ExecutionMode::Personal
    }
}

/// Apply mode-aware default for `is_personal` when caller did not specify.
/// Inject mode-aware `is_personal` when the caller did not specify it.
pub fn apply_is_personal_to_body(
    body: &mut serde_json::Value,
    execution_mode: ExecutionMode,
    team_context_degraded: bool,
) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    if obj.contains_key("is_personal") {
        return;
    }
    if let Some(val) = resolve_is_personal(execution_mode, None, team_context_degraded) {
        obj.insert("is_personal".to_string(), serde_json::Value::Bool(val));
    }
}

pub fn resolve_is_personal(
    execution_mode: ExecutionMode,
    explicit: Option<bool>,
    team_context_degraded: bool,
) -> Option<bool> {
    if let Some(value) = explicit {
        return Some(value);
    }
    if team_context_degraded {
        return Some(true);
    }
    Some(execution_mode.default_is_personal())
}

/// Default list/read scope label for team entity actions.
pub fn default_team_read_enabled(
    execution_mode: ExecutionMode,
    team_features_available: bool,
    team_context_degraded: bool,
) -> bool {
    !team_context_degraded
        && team_features_available
        && matches!(execution_mode, ExecutionMode::Team)
}

pub async fn refresh_account_execution_state(
    session: &SessionManager,
    startup_preference: AccountModePreference,
    tool_override: Option<AccountModePreference>,
    account_context: Option<AccountContextSnapshot>,
) -> ModeResolution {
    let mismatch = if let Some(ctx) = account_context.as_ref() {
        session.detect_account_mismatch(ctx).await
    } else {
        false
    };

    if mismatch {
        session
            .degrade_team_context("Authenticated account no longer matches persisted team context")
            .await;
    } else if account_context.is_some() {
        let state = session.state().await;
        if state.team_context_degraded {
            session.clear_team_context_degradation().await;
        }
    }

    let state = session.state().await;
    let resolution = resolve_execution_mode(&ModeResolutionInput {
        startup_preference,
        tool_override,
        persisted_preference: state.account_mode_preference,
        account_context: account_context.clone(),
        team_context_degraded: state.team_context_degraded,
    });

    session
        .set_account_execution_state(
            resolution.preference,
            resolution.execution_mode,
            account_context,
            state.team_context_degraded,
        )
        .await;

    resolution
}

pub fn format_account_context_block(
    ctx: Option<&AccountContextSnapshot>,
    execution_mode: ExecutionMode,
    preference: AccountModePreference,
    team_context_degraded: bool,
    resolution_note: Option<&str>,
) -> String {
    let mut lines = vec!["[ACCOUNT_CONTEXT]".to_string()];

    if team_context_degraded {
        lines.push(
            "team_context=degraded — team assumptions disabled; personal scope enforced."
                .to_string(),
        );
    }

    lines.push(format!(
        "active_mode={} preference={}",
        execution_mode.as_str(),
        preference.as_str()
    ));

    if let Some(ctx) = ctx {
        lines.push(format!(
            "account_type={} selected_context={} team_membership={}",
            ctx.account_type, ctx.selected_context, ctx.has_team_membership
        ));
        if let Some(plan) = ctx.effective_plan.as_deref() {
            lines.push(format!("effective_plan={}", plan));
        }
        if let Some(name) = ctx.team_name.as_deref() {
            lines.push(format!("team_name={}", name));
        }
        if !ctx.team_capabilities.is_empty() {
            lines.push(format!(
                "team_capabilities={}",
                ctx.team_capabilities.join(", ")
            ));
        }
        lines.push(format!(
            "transcript_content={} (topic metadata may be team-visible; never share transcript bodies)",
            ctx.transcript_sharing
        ));
        if ctx.source != AccountContextSource::Unknown {
            lines.push(format!("context_source={:?}", ctx.source));
        }
    } else {
        lines.push("account_type=individual team_membership=false".to_string());
        lines.push("transcript_content=owner_only".to_string());
    }

    if let Some(note) = resolution_note {
        lines.push(format!("mode_note={}", note));
    }

    lines.join("\n")
}

pub fn format_team_priority_block(items: &[TeamPriorityItem], compact: bool) -> String {
    if items.is_empty() {
        return String::new();
    }
    let limit = if compact { 5 } else { 10 };
    let mut lines = vec!["[TEAM_PRIORITY]".to_string()];
    for item in items.iter().take(limit) {
        let priority = item
            .priority
            .as_deref()
            .map(|p| format!(" priority={}", p))
            .unwrap_or_default();
        let status = item
            .status
            .as_deref()
            .map(|s| format!(" status={}", s))
            .unwrap_or_default();
        lines.push(format!(
            "- {}: {}{}{}",
            item.kind, item.title, priority, status
        ));
    }
    lines.join("\n")
}

pub fn format_transcript_topic_block(signals: &[TranscriptTopicSignal], compact: bool) -> String {
    if signals.is_empty() {
        return String::new();
    }
    let limit = if compact { 3 } else { 8 };
    let mut lines = vec![
        "[TRANSCRIPT_TOPIC_SIGNALS] metadata-only — transcript content is never shared".to_string(),
    ];
    for signal in signals.iter().take(limit) {
        let confidence = signal
            .confidence
            .map(|c| format!(" confidence={:.2}", c))
            .unwrap_or_default();
        let when = signal
            .last_discussed_at
            .as_deref()
            .map(|d| format!(" last={}", d))
            .unwrap_or_default();
        lines.push(format!(
            "- topic=\"{}\"{}{}",
            signal.topic, confidence, when
        ));
    }
    lines.join("\n")
}

pub fn parse_account_mode_override(value: Option<&str>) -> Option<AccountModePreference> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn team_ctx() -> AccountContextSnapshot {
        AccountContextSnapshot {
            account_type: "dual".to_string(),
            has_team_membership: true,
            selected_context: "team".to_string(),
            team_capabilities: vec!["shared_tasks".to_string()],
            ..Default::default()
        }
    }

    #[test]
    fn tool_override_beats_startup_preference() {
        let resolution = resolve_execution_mode(&ModeResolutionInput {
            startup_preference: AccountModePreference::Personal,
            tool_override: Some(AccountModePreference::Team),
            persisted_preference: AccountModePreference::Auto,
            account_context: Some(team_ctx()),
            team_context_degraded: false,
        });
        assert_eq!(resolution.execution_mode, ExecutionMode::Team);
    }

    #[test]
    fn startup_preference_beats_persisted() {
        let resolution = resolve_execution_mode(&ModeResolutionInput {
            startup_preference: AccountModePreference::Personal,
            tool_override: None,
            persisted_preference: AccountModePreference::Team,
            account_context: Some(team_ctx()),
            team_context_degraded: false,
        });
        assert_eq!(resolution.execution_mode, ExecutionMode::Personal);
    }

    #[test]
    fn auto_follows_selected_context() {
        let resolution = resolve_execution_mode(&ModeResolutionInput {
            startup_preference: AccountModePreference::Auto,
            tool_override: None,
            persisted_preference: AccountModePreference::Auto,
            account_context: Some(team_ctx()),
            team_context_degraded: false,
        });
        assert_eq!(resolution.execution_mode, ExecutionMode::Team);
    }

    #[test]
    fn degraded_forces_personal() {
        let resolution = resolve_execution_mode(&ModeResolutionInput {
            startup_preference: AccountModePreference::Team,
            tool_override: None,
            persisted_preference: AccountModePreference::Team,
            account_context: Some(team_ctx()),
            team_context_degraded: true,
        });
        assert_eq!(resolution.execution_mode, ExecutionMode::Personal);
    }

    #[test]
    fn explicit_auto_override_resets_persisted_team() {
        let resolution = resolve_execution_mode(&ModeResolutionInput {
            startup_preference: AccountModePreference::Auto,
            tool_override: Some(AccountModePreference::Auto),
            persisted_preference: AccountModePreference::Team,
            account_context: Some(team_ctx()),
            team_context_degraded: false,
        });
        assert_eq!(resolution.preference, AccountModePreference::Auto);
        assert_eq!(resolution.execution_mode, ExecutionMode::Team);
    }

    #[test]
    fn resolve_is_personal_defaults_by_mode() {
        assert_eq!(
            resolve_is_personal(ExecutionMode::Team, None, false),
            Some(false)
        );
        assert_eq!(
            resolve_is_personal(ExecutionMode::Personal, None, false),
            Some(true)
        );
        assert_eq!(
            resolve_is_personal(ExecutionMode::Team, Some(true), false),
            Some(true)
        );
        assert_eq!(
            resolve_is_personal(ExecutionMode::Team, None, true),
            Some(true)
        );
    }
}
