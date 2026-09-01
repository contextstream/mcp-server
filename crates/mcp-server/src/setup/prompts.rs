//! Interactive CLI prompts for the setup wizard.
//!
//! Provides reusable prompt functions using dialoguer.

use anyhow::Result;
use console::{style, Style};
use dialoguer::{theme::ColorfulTheme, Input, MultiSelect, Password, Select};

use super::editors::Editor;

/// Get the colorful theme for prompts.
fn theme() -> ColorfulTheme {
    ColorfulTheme {
        success_prefix: style("✓ ".to_string()).for_stderr().green(),
        active_item_style: Style::new().for_stderr().cyan(),
        inactive_item_style: Style::new().for_stderr(),
        checked_item_prefix: style("● ".to_string()).for_stderr().cyan(),
        unchecked_item_prefix: style("○ ".to_string()).for_stderr().black().bright(),
        ..ColorfulTheme::default()
    }
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
    // Print usage hint
    println!(
        "{}",
        style("  ↑/↓ move • Enter confirm • Ctrl+C exits").dim()
    );

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
    // Print usage hint
    println!(
        "{}",
        style("  ↑/↓ move • Enter select • Ctrl+C exits").dim()
    );

    Ok(Select::with_theme(&theme())
        .with_prompt(prompt)
        .items(options)
        .default(0)
        .interact()?)
}

/// Select multiple options from a list.
pub fn multi_select(
    prompt: &str,
    options: &[&str],
    defaults: Option<&[bool]>,
) -> Result<Vec<usize>> {
    // Print usage hint
    println!(
        "{}",
        style("  ↑/↓ move • Space toggle • Enter confirm • Ctrl+C exits").dim()
    );

    let t = theme();
    let mut builder = MultiSelect::with_theme(&t)
        .with_prompt(prompt)
        .items(options);

    if let Some(d) = defaults {
        builder = builder.defaults(d);
    }

    Ok(builder.interact()?)
}

/// Select which editors to configure.
pub fn select_editors(detected: &[Editor]) -> Result<Vec<Editor>> {
    let (all_editors, options, defaults) = build_editor_selection_model(detected);
    let option_refs: Vec<&str> = options.iter().map(String::as_str).collect();

    println!(
        "{}",
        style("  Tip: Detected editors are preselected; the review step lets you come back and change this.").dim()
    );

    let indices = multi_select(
        "Select editors to configure:",
        &option_refs,
        Some(&defaults),
    )?;

    if indices.is_empty() {
        println!("{}", style("  Skipping editor configuration.").dim());
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
            options.push(format!("{} [detected]", editor.display_name()));
        } else {
            options.push(format!("{} [manual setup]", editor.display_name()));
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
    use indicatif::{ProgressBar, ProgressStyle};

    let pb = ProgressBar::new_spinner();
    pb.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.cyan} {msg}")
            .unwrap(),
    );
    pb.set_message(message.to_string());
    pb.enable_steady_tick(std::time::Duration::from_millis(100));

    let result = future.await;

    pb.finish_and_clear();

    result
}

/// Print an info message.
pub fn info(message: &str) {
    println!("{}{}", style("ℹ  ").blue(), message);
}

/// Print a success message.
pub fn success(message: &str) {
    println!("{} {}", style("✓").green(), message);
}

/// Print a warning message.
pub fn warning(message: &str) {
    println!("{} {}", style("⚠").yellow(), message);
}

/// Print an error message.
pub fn error(message: &str) {
    println!("{} {}", style("✗").red(), message);
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

        assert!(options[claude_idx].contains("[detected]"));
        assert!(options[cursor_idx].contains("[manual setup]"));
    }
}
