//! A remote-checks choice is staged with other wizard choices and applied on Save.

use std::path::Path;

use anyhow::Result;
use mcp_client::remote_checks::{ProjectLanguages, RemoteChecks};
use mcp_client::ContextStreamClient;
use mcp_tools::domains::remote_checks::{local_languages, settings};

use super::ui::{self, Mark};
use super::{prompts, safe_edit, wizard};

const BENEFIT: &str =
    "Run builds, tests and type-checks on your own warm remote machine. Slow commands can finish in a fraction of the time.";
const OPTIONS: [&str; 3] = [
    "Turn on standard engines",
    "Turn on standard and more engines",
    "Keep checks local",
];

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Choice {
    Standard,
    More,
    Off,
}

impl Choice {
    fn recommended(project: Option<&ProjectLanguages>) -> Self {
        match project.map(ProjectLanguages::recommendation) {
            Some((true, true)) => Self::More,
            Some((true, false)) => Self::Standard,
            _ => Self::Off,
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Standard => "On · standard engines",
            Self::More => "On · standard and more engines",
            Self::Off => "Off",
        }
    }
}

pub(super) struct Step {
    state: RemoteChecks,
    project: Option<ProjectLanguages>,
    pub choice: Choice,
}

fn on_line(state: &RemoteChecks, project: Option<&ProjectLanguages>) -> String {
    let names: Vec<_> = project
        .into_iter()
        .flat_map(|project| {
            project.standard.iter().chain(
                project
                    .more
                    .iter()
                    .filter(|_| state.more_engines == Some(true)),
            )
        })
        .cloned()
        .collect();
    if names.is_empty() {
        "Remote checks: on".into()
    } else {
        format!("Remote checks: on ({})", names.join(", "))
    }
}

impl Step {
    pub(super) async fn prepare(
        client: &ContextStreamClient,
        folder: Option<&Path>,
        explicit: Option<bool>,
    ) -> Result<Option<Self>> {
        let state = match client.remote_checks_status().await {
            Ok(state) => state,
            Err(error) => {
                report_error(&error);
                return Ok(None);
            }
        };
        let project = local_languages(folder, &state);
        if state.enabled && explicit != Some(false) {
            ui::say(Mark::Ok, &on_line(&state, project.as_ref()), None);
            return Ok(None);
        }
        if !state.available && !state.enabled {
            ui::say(
                Mark::Info,
                "Remote checks are unavailable for this account.",
                None,
            );
            return Ok(None);
        }
        let recommended = Choice::recommended(project.as_ref());
        let choice = match explicit {
            Some(false) => Choice::Off,
            Some(true) if recommended == Choice::More || state.more_engines == Some(true) => {
                Choice::More
            }
            Some(true) => Choice::Standard,
            None => recommended,
        };
        let mut step = Self {
            state,
            project,
            choice,
        };
        if explicit.is_none() {
            step.prompt()?;
        }
        Ok(Some(step))
    }

    pub(super) fn prompt(&mut self) -> Result<()> {
        wizard::print_step(4, "Remote checks", BENEFIT);
        let ui = ui::ui();
        println!("{}{}", ui::GUTTER, self.state.price_line());
        if let Some(balance) = self.state.balance_line() {
            println!("{}{}", ui::GUTTER, ui.strong(&balance));
        }
        let summary = self
            .project
            .as_ref()
            .map(ProjectLanguages::summary)
            .unwrap_or_else(|| "No project folder to check.".into());
        let width = ui.card_width().saturating_sub(8);
        let mut rows = vec![ui.heading("This project")];
        rows.extend(ui::wrap(&summary, width));
        if self
            .project
            .as_ref()
            .is_some_and(|p| !p.local.is_empty() && !p.recommendation().0)
        {
            rows.push("No detected language runs remotely.".into());
        }
        println!("{}", ui.card(&rows));
        let choices = [Choice::Standard, Choice::More, Choice::Off];
        let selected = choices
            .iter()
            .position(|choice| *choice == self.choice)
            .unwrap_or(2);
        let recommended = Choice::recommended(self.project.as_ref());
        let options: [String; 3] = std::array::from_fn(|i| {
            if choices[i] == recommended {
                format!("{} (recommended)", OPTIONS[i])
            } else {
                OPTIONS[i].to_owned()
            }
        });
        self.choice = choices[prompts::select_with_default(
            "Where should checks run?",
            &options.each_ref().map(String::as_str),
            selected,
        )?];
        Ok(())
    }

    pub(super) async fn apply(&self, client: &ContextStreamClient, workspace: Option<&str>) {
        let enabled = self.choice != Choice::Off;
        if !enabled && !self.state.enabled {
            ui::say(Mark::Info, "Remote checks: off", None);
            return;
        }
        if safe_edit::is_dry_run() {
            ui::say(
                Mark::Pending,
                "Would set remote checks",
                Some(self.choice.label()),
            );
            return;
        }
        let more = enabled.then_some(self.choice == Choice::More);
        match settings(client, workspace, Some(enabled), more).await {
            Ok(state) => {
                let line = match (state.enabled, state.more_engines) {
                    (true, Some(true)) => "Remote checks: on (standard and more engines)",
                    (true, Some(false)) => "Remote checks: on (standard engines)",
                    (true, None) => "Remote checks: on",
                    (false, _) => "Remote checks: off",
                };
                ui::say(Mark::Ok, line, None);
            }
            Err(error) => report_error(&error),
        }
    }
}

fn report_error(error: &mcp_types::Error) {
    let text = match error {
        mcp_types::Error::Http { status: 403, .. } => {
            "Remote checks are unavailable for this account."
        }
        mcp_types::Error::Tool(message) => message,
        _ => "Could not confirm remote checks. Setup will continue.",
    };
    ui::say(Mark::Info, text, None);
}

pub(super) async fn scripted(
    client: &ContextStreamClient,
    folder: Option<&Path>,
    workspace: Option<&str>,
    choice: Option<bool>,
) {
    if let Some(choice) = choice {
        if let Ok(Some(step)) = Step::prepare(client, folder, Some(choice)).await {
            step.apply(client, workspace).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recommend_only_engines_the_project_can_use() {
        let mut project = ProjectLanguages::default();
        assert_eq!(Choice::recommended(None), Choice::Off);
        project.local.push("Haskell".into());
        assert_eq!(Choice::recommended(Some(&project)), Choice::Off);
        project.standard.push("Rust".into());
        assert_eq!(Choice::recommended(Some(&project)), Choice::Standard);
        project.more.push("Ruby".into());
        assert_eq!(Choice::recommended(Some(&project)), Choice::More);
        project.standard.clear();
        assert_eq!(Choice::recommended(Some(&project)), Choice::More);
    }

    #[test]
    fn already_on_shows_only_the_projects_active_engines() {
        let mut state: RemoteChecks = serde_json::from_value(serde_json::json!({
            "tool":"check","enabled":true,"available":true,"more_engines":false
        }))
        .unwrap();
        let project = ProjectLanguages {
            standard: vec!["Rust".into()],
            more: vec!["Ruby".into()],
            local: vec!["Haskell".into()],
        };
        assert_eq!(on_line(&state, Some(&project)), "Remote checks: on (Rust)");
        state.more_engines = Some(true);
        assert_eq!(
            on_line(&state, Some(&project)),
            "Remote checks: on (Rust, Ruby)"
        );
        assert_eq!(on_line(&state, None), "Remote checks: on");
    }
}
