//! Which of the files setup writes into a project stay out of Git.
//!
//! Some project files name this machine: a project MCP config carries an
//! installation id or a local binary path (and after hosted setup the API
//! key), and `.contextstream/config.json` carries this checkout's absolute
//! path and id, which every other clone rejects. Those are listed in the
//! checkout's `.git/info/exclude`, never in the shared `.gitignore`, and
//! without needing the `git` program. Rules files (CLAUDE.md, AGENTS.md, ...)
//! only name the workspace and project, so committing them helps a fresh
//! clone find its project. Setup prints one line saying which is which.

use super::{editors::Editor, mcp_config, safe_edit, ui};
use mcp_session::git_exclude::{self, GitExcludeOutcome};
use std::path::{Path, PathBuf};

/// Exclude comment for a project MCP config that embeds an API key.
pub(crate) const API_KEY_REASON: &str = "this MCP config holds an API key";
/// Exclude comment for a project MCP config that holds only ContextStream's
/// entry, which names this machine.
pub(crate) const MACHINE_MCP_REASON: &str =
    "this MCP config names this machine (installation id or local path)";

/// List `path` in its checkout's `.git/info/exclude`. `None` in a dry run,
/// which writes nothing.
pub(crate) fn keep_out_of_git(path: &Path, why: &str) -> Option<GitExcludeOutcome> {
    if safe_edit::is_dry_run() {
        return None;
    }
    let outcome = git_exclude::keep_out_of_git(path, why);
    if let GitExcludeOutcome::Failed { reason } = &outcome {
        tracing::warn!(path = %path.display(), %reason, "could not keep a project file out of Git");
    }
    Some(outcome)
}

/// What setup left in a Git checkout, for the summary line.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ProjectGitReport {
    /// Files `git add -A` now leaves out, relative to the project folder.
    pub kept_out: Vec<String>,
    /// The exclude file that lists them.
    pub exclude_file: Option<PathBuf>,
    /// Rules files that only name the workspace and project.
    pub safe_to_commit: Vec<String>,
    /// An API key Git would commit.
    pub warnings: Vec<String>,
    /// Machine-specific content Git still sees.
    pub notes: Vec<String>,
}

impl ProjectGitReport {
    fn keep(&mut self, relative: String, outcome: &GitExcludeOutcome) {
        if !self.kept_out.contains(&relative) {
            self.kept_out.push(relative);
        }
        if self.exclude_file.is_none() {
            self.exclude_file = outcome.exclude_file().map(Path::to_path_buf);
        }
    }
}

/// Make sure the machine-local files setup wrote for `editors` are kept out
/// of Git, and say which project files are safe to commit. `None` in a dry
/// run or outside a Git checkout (a folder without Git gets its API key file
/// in `.gitignore` instead; see `protect_non_git_secret_project_config`).
pub(crate) fn project_git_report(
    project_path: &Path,
    editors: &[Editor],
) -> Option<ProjectGitReport> {
    if safe_edit::is_dry_run() {
        return None;
    }
    if matches!(mcp_session::git_exclude_target(project_path), Ok(None)) {
        return None;
    }
    let mut report = ProjectGitReport::default();
    let mut seen = Vec::new();

    for editor in editors {
        let Some((path, kind)) = mcp_config::project_mcp_config_kind(editor, project_path) else {
            continue;
        };
        if seen.contains(&path) {
            continue;
        }
        seen.push(path.clone());
        let relative = project_relative(project_path, &path);
        let holds_key = kind == mcp_config::ProjectMcpConfigKind::HoldsKey;
        let Some(outcome) = mcp_config::keep_project_mcp_config_out_of_git(&path, kind) else {
            report.notes.push(format!(
                "{relative} also holds other settings, so Git still sees it; its contextstream entry is this machine's, so leave it out of commits"
            ));
            continue;
        };
        match outcome {
            outcome if outcome.kept_out() => report.keep(relative, &outcome),
            GitExcludeOutcome::Tracked { .. } if holds_key => report.warnings.push(format!(
                "{relative} holds your API key and Git tracks it; untrack it with `git rm --cached {relative}`"
            )),
            GitExcludeOutcome::Tracked { .. } => report.notes.push(format!(
                "{relative} is tracked by Git; its contextstream entry names this machine, so leave that change out of commits"
            )),
            GitExcludeOutcome::Failed { reason } if holds_key => report.warnings.push(format!(
                "{relative} holds your API key and could not be kept out of Git ({reason}); add /{relative} to .git/info/exclude"
            )),
            GitExcludeOutcome::Failed { reason } => report.notes.push(format!(
                "{relative} names this machine and could not be kept out of Git ({reason})"
            )),
            _ => {}
        }
    }

    let checkout_config = project_path.join(".contextstream").join("config.json");
    if checkout_config_names_this_checkout(&checkout_config) {
        let relative = project_relative(project_path, &checkout_config);
        match keep_out_of_git(&checkout_config, git_exclude::CHECKOUT_CONFIG_REASON) {
            Some(outcome) if outcome.kept_out() => report.keep(relative, &outcome),
            Some(GitExcludeOutcome::Tracked { .. }) => report.notes.push(format!(
                "{relative} is tracked by Git but now holds this checkout's path and id, which other clones reject; commit only shared keys such as checkout_snapshot"
            )),
            Some(GitExcludeOutcome::Failed { reason }) => report.notes.push(format!(
                "{relative} holds this checkout's path and id and could not be kept out of Git ({reason})"
            )),
            _ => {}
        }
    }

    for editor in editors {
        let Some(rules) = editor.rules_path(Some(project_path)) else {
            continue;
        };
        if !rules.is_file() || !rules.starts_with(project_path) {
            continue;
        }
        let relative = project_relative(project_path, &rules);
        if !report.safe_to_commit.contains(&relative) && !report.kept_out.contains(&relative) {
            report.safe_to_commit.push(relative);
        }
    }

    Some(report)
}

/// Whether `.contextstream/config.json` holds this checkout's identity. A
/// copy with only shared keys (`checkout_snapshot`, `git_capture`) is a
/// team's to commit.
fn checkout_config_names_this_checkout(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .is_some_and(|config| git_exclude::holds_checkout_identity(&config))
}

fn project_relative(project_path: &Path, path: &Path) -> String {
    slash_path(path.strip_prefix(project_path).unwrap_or(path))
}

/// A relative path with `/` separators on every platform.
fn slash_path(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// The exclude file as the person would look for it: relative to the
/// project when it is inside, else a home-abbreviated path.
fn exclude_display(project_path: &Path, exclude_file: &Path) -> String {
    let project = std::fs::canonicalize(project_path).unwrap_or_else(|_| project_path.into());
    match exclude_file.strip_prefix(&project) {
        Ok(relative) => slash_path(relative),
        Err(_) => super::wizard::display_path(exclude_file),
    }
}

/// The summary line as head and tail, e.g. `Kept out of Git
/// (.git/info/exclude)` and `.mcp.json, .contextstream/config.json · safe to
/// commit: CLAUDE.md`.
pub(crate) fn summary_line(
    project_path: &Path,
    report: &ProjectGitReport,
) -> Option<(String, String)> {
    let safe = report.safe_to_commit.join(", ");
    if report.kept_out.is_empty() {
        return (!safe.is_empty()).then(|| ("Safe to commit".to_string(), safe));
    }
    let head = match report.exclude_file.as_deref() {
        Some(file) => format!("Kept out of Git ({})", exclude_display(project_path, file)),
        None => "Kept out of Git".to_string(),
    };
    let mut tail = report.kept_out.join(", ");
    if !safe.is_empty() {
        tail.push_str(" · safe to commit: ");
        tail.push_str(&safe);
    }
    Some((head, tail))
}

/// Print the summary for `project_path`, with a visible warning for an API
/// key Git would still commit.
pub(crate) fn print_project_git_summary(project_path: Option<&Path>, editors: &[Editor]) {
    let Some(project_path) = project_path else {
        return;
    };
    let Some(report) = project_git_report(project_path, editors) else {
        return;
    };
    if let Some((head, tail)) = summary_line(project_path, &report) {
        let mark = if report.kept_out.is_empty() {
            ui::Mark::Info
        } else {
            ui::Mark::Ok
        };
        ui::say(mark, &head, Some(&tail));
    }
    for warning in &report.warnings {
        ui::say(ui::Mark::Warn, "API key not kept out of Git", Some(warning));
    }
    for note in &report.notes {
        println!("{}    {}", ui::GUTTER, ui::ui().faint(note));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn joined(line: Option<(String, String)>) -> Option<String> {
        line.map(|(head, tail)| format!("{head} · {tail}"))
    }

    #[test]
    fn summary_names_kept_and_shareable_files() {
        let project = tempfile::tempdir().unwrap();
        let exclude = std::fs::canonicalize(project.path())
            .unwrap()
            .join(".git")
            .join("info")
            .join("exclude");
        let report = ProjectGitReport {
            kept_out: vec![".mcp.json".into(), ".contextstream/config.json".into()],
            exclude_file: Some(exclude),
            safe_to_commit: vec!["CLAUDE.md".into()],
            ..ProjectGitReport::default()
        };
        assert_eq!(
            joined(summary_line(project.path(), &report)).as_deref(),
            Some(
                "Kept out of Git (.git/info/exclude) · .mcp.json, .contextstream/config.json · safe to commit: CLAUDE.md"
            )
        );

        let only_rules = ProjectGitReport {
            safe_to_commit: vec!["CLAUDE.md".into(), "AGENTS.md".into()],
            ..ProjectGitReport::default()
        };
        assert_eq!(
            joined(summary_line(project.path(), &only_rules)).as_deref(),
            Some("Safe to commit · CLAUDE.md, AGENTS.md")
        );
        assert_eq!(
            summary_line(project.path(), &ProjectGitReport::default()),
            None
        );
    }
}
