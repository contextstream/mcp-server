//! Interactive CLI prompts for the setup wizard.
//!
//! Provides reusable prompt functions using dialoguer, styled with the
//! ContextCode terminal theme in [`super::ui`].

use anyhow::Result;
use dialoguer::{Input, MultiSelect, Password, Select};

use super::editors::Editor;
use super::ui::{self, Mark, PromptTheme};

/// The prompt theme for this terminal.
fn theme() -> PromptTheme {
    PromptTheme::new(*ui::ui())
}

/// Ask for text input.
pub fn input(prompt: &str, default: Option<&str>) -> Result<String> {
    let t = theme();
    let mut builder = Input::with_theme(&t).with_prompt(prompt);

    if let Some(d) = default {
        builder = builder.default(d.to_string());
    }

    Ok(builder.interact_text()?)
}

/// Ask for optional text input.
pub fn optional_input(prompt: &str) -> Result<Option<String>> {
    let value: String = Input::with_theme(&theme())
        .with_prompt(prompt)
        .allow_empty(true)
        .interact_text()?;
    let trimmed = value.trim();

    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_string()))
    }
}

/// Ask for a password (hidden input).
pub fn password(prompt: &str) -> Result<String> {
    Ok(Password::with_theme(&theme())
        .with_prompt(prompt)
        .interact()?)
}

/// Ask a yes/no question with arrow key navigation.
pub fn confirm(prompt: &str, default: bool) -> Result<bool> {
    let options = &["Yes", "No"];
    let default_idx = if default { 0 } else { 1 };

    let choice = Select::with_theme(&theme())
        .with_prompt(prompt)
        .items(options)
        .default(default_idx)
        .interact_opt()?
        .unwrap_or(default_idx);

    Ok(choice == 0)
}

/// Select one option from a list.
pub fn select(prompt: &str, options: &[&str]) -> Result<usize> {
    select_with_default(prompt, options, 0)
}

/// Select one option from a list, starting on `default`.
pub fn select_with_default(prompt: &str, options: &[&str], default: usize) -> Result<usize> {
    Ok(Select::with_theme(&theme())
        .with_prompt(prompt)
        .items(options)
        .default(default.min(options.len().saturating_sub(1)))
        .interact()?)
}

/// Select multiple options from a list.
pub fn multi_select(
    prompt: &str,
    options: &[&str],
    defaults: Option<&[bool]>,
) -> Result<Vec<usize>> {
    let t = theme();
    let mut builder = MultiSelect::with_theme(&t)
        .with_prompt(prompt)
        .items(options);

    if let Some(d) = defaults {
        builder = builder.defaults(d);
    }

    Ok(builder.interact()?)
}

/// Select which editors to configure; detected editors start checked.
pub fn select_editors(detected: &[Editor]) -> Result<Vec<Editor>> {
    select_editors_with_defaults(detected, detected)
}

/// Select which editors to configure, starting from `current`.
pub fn select_editors_with_defaults(
    detected: &[Editor],
    current: &[Editor],
) -> Result<Vec<Editor>> {
    let (all_editors, options, _) = build_editor_selection_model(detected);
    let defaults: Vec<bool> = all_editors
        .iter()
        .map(|editor| current.contains(editor))
        .collect();
    let option_refs: Vec<&str> = options.iter().map(String::as_str).collect();

    let indices = multi_select("Editors to connect", &option_refs, Some(&defaults))?;

    if indices.is_empty() {
        say(
            Mark::Pending,
            "No editors selected",
            Some("nothing will be configured"),
        );
    }

    Ok(indices.into_iter().map(|i| all_editors[i]).collect())
}

fn build_editor_selection_model(detected: &[Editor]) -> (Vec<Editor>, Vec<String>, Vec<bool>) {
    let all_editors = Editor::all().to_vec();
    let mut options = Vec::with_capacity(all_editors.len());
    let mut defaults = Vec::with_capacity(all_editors.len());

    for editor in &all_editors {
        let is_detected = detected.contains(editor);
        if is_detected {
            options.push(format!("{} · detected", editor.display_name()));
        } else {
            options.push(editor.display_name().to_string());
        }
        defaults.push(is_detected);
    }

    (all_editors, options, defaults)
}

/// Ask for a number from a list (1-indexed display).
pub fn number_list<T: Clone + std::fmt::Display>(
    prompt: &str,
    items: &[T],
    allow_multiple: bool,
) -> Result<Vec<T>> {
    let options: Vec<String> = items
        .iter()
        .enumerate()
        .map(|(i, item)| format!("{}. {}", i + 1, item))
        .collect();

    let options_refs: Vec<&str> = options.iter().map(|s| s.as_str()).collect();

    if allow_multiple {
        let indices = multi_select(prompt, &options_refs, None)?;
        Ok(indices.into_iter().map(|i| items[i].clone()).collect())
    } else {
        let index = select(prompt, &options_refs)?;
        Ok(vec![items[index].clone()])
    }
}

/// Display a spinner while an async operation runs.
pub async fn with_spinner<F, T>(message: &str, future: F) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    ui::spin(message, future).await
}

/// Print an activity line with a mark, head, and optional faint detail.
pub fn say(mark: Mark, head: &str, detail: Option<&str>) {
    ui::say(mark, head, detail);
}

/// Print an info message.
pub fn info(message: &str) {
    say(Mark::Info, message, None);
}

/// Print a success message.
pub fn success(message: &str) {
    say(Mark::Ok, message, None);
}

/// Print a warning message.
pub fn warning(message: &str) {
    say(Mark::Warn, message, None);
}

/// Print an error message.
pub fn error(message: &str) {
    say(Mark::Fail, message, None);
}

#[cfg(test)]
mod tests {
    use super::*;

    // Note: Interactive prompts are hard to test automatically.
    // These tests just verify the module compiles correctly.

    #[test]
    fn test_theme() {
        let _theme = theme();
    }

    #[test]
    fn test_build_editor_selection_model_defaults() {
        let detected = [Editor::ClaudeCode, Editor::Codex];
        let (all_editors, options, defaults) = build_editor_selection_model(&detected);

        assert_eq!(all_editors.len(), Editor::all().len());
        assert_eq!(options.len(), all_editors.len());
        assert_eq!(defaults.len(), all_editors.len());

        let claude_idx = all_editors
            .iter()
            .position(|editor| *editor == Editor::ClaudeCode)
            .expect("ClaudeCode should exist in editor list");
        let cursor_idx = all_editors
            .iter()
            .position(|editor| *editor == Editor::Cursor)
            .expect("Cursor should exist in editor list");
        let codex_idx = all_editors
            .iter()
            .position(|editor| *editor == Editor::Codex)
            .expect("Codex should exist in editor list");

        assert!(defaults[claude_idx]);
        assert!(defaults[codex_idx]);
        assert!(!defaults[cursor_idx]);

        assert!(options[claude_idx].ends_with(" · detected"));
        assert_eq!(options[cursor_idx], Editor::Cursor.display_name());
    }
}
