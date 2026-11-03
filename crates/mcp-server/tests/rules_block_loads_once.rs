//! The managed ContextStream block reaches each session once (2026-11-03).
//!
//! In a set-up checkout Claude Code loads `~/.claude/CLAUDE.md` and the
//! checkout's `CLAUDE.md`; Codex loads `~/.codex/AGENTS.md` and the checkout's
//! `AGENTS.md`. `update-rules` wrote the whole block into both, so every session
//! in the project carried it twice (about 45 KB for Claude Code, 58 KB for
//! Codex, whose 32 KiB project-doc limit then also cut off the checkout's own
//! instructions after it). Now the global file holds the block and the
//! project's file only the project's identity, unless an editor would then miss
//! the block.
#![cfg(unix)]

use std::path::Path;
use std::process::Command;

const WORKSPACE_ID: &str = "550e8400-e29b-41d4-a716-446655440000";
/// A line only the whole block holds.
const FULL_BLOCK_LINE: &str = "Core ContextStream Workflow";

fn update_rules(home: &Path, project: &Path, editors: &str, env: &[(&str, &Path)]) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"));
    command
        .args([
            "update-rules",
            "--scope=all",
            &format!("--editors={editors}"),
            &format!("--workspace-id={WORKSPACE_ID}"),
            "--workspace-name=Example Team",
        ])
        .current_dir(project)
        .env_clear()
        // No editor binaries on PATH: only the editors named take part.
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("CONTEXTSTREAM_WATCH", "0")
        .env("CONTEXTSTREAM_API_URL", "http://127.0.0.1:9");
    for (name, value) in env {
        command.env(name, value);
    }
    let output = command.output().expect("run update-rules");
    assert!(
        output.status.success(),
        "update-rules failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

fn checkout(root: &Path) -> std::path::PathBuf {
    let project = root.join("app");
    std::fs::create_dir_all(project.join(".git")).unwrap();
    project
}

#[test]
fn a_claude_code_session_in_a_set_up_checkout_loads_the_block_once() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = checkout(temp.path());
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        project.join("CLAUDE.md"),
        "# App notes\n\nRun the tests with make check.\n",
    )
    .unwrap();

    update_rules(&home, &project, "claude", &[]);

    let global = read(&home.join(".claude/CLAUDE.md"));
    let local = read(&project.join("CLAUDE.md"));
    assert_eq!(global.matches(FULL_BLOCK_LINE).count(), 1, "{global}");
    assert_eq!(
        local.matches(FULL_BLOCK_LINE).count(),
        0,
        "the checkout's CLAUDE.md repeats the whole block:\n{local}"
    );
    assert!(
        local.contains(&format!("# Workspace ID: {WORKSPACE_ID}")),
        "{local}"
    );
    assert!(local.contains("# Workspace: Example Team"), "{local}");
    assert!(local.contains("# Project: app"), "{local}");
    assert!(local.contains("`~/.claude/CLAUDE.md`"), "{local}");
    assert!(local.contains("Run the tests with make check."), "{local}");
    assert!(local.len() < 2_000, "{} bytes", local.len());
    // The global file names no workspace: its rules apply to every folder.
    assert!(!global.contains("# Workspace ID:"), "{global}");

    // A second run changes nothing.
    update_rules(&home, &project, "claude", &[]);
    assert_eq!(read(&project.join("CLAUDE.md")), local);
}

#[test]
fn a_codex_session_in_a_set_up_checkout_loads_the_block_once() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = checkout(temp.path());
    std::fs::create_dir_all(&home).unwrap();

    update_rules(&home, &project, "codex", &[]);

    let global = read(&home.join(".codex/AGENTS.md"));
    let local = read(&project.join("AGENTS.md"));
    assert_eq!(global.matches(FULL_BLOCK_LINE).count(), 1, "{global}");
    assert_eq!(local.matches(FULL_BLOCK_LINE).count(), 0, "{local}");
    assert!(
        local.contains(&format!("# Workspace ID: {WORKSPACE_ID}")),
        "{local}"
    );
    assert!(local.contains("`~/.codex/AGENTS.md`"), "{local}");
    assert!(local.len() < 2_000, "{} bytes", local.len());
}

#[test]
fn the_whole_block_stays_in_the_checkout_when_the_editor_would_miss_the_global_one() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    // Claude Code reads $CLAUDE_CONFIG_DIR/CLAUDE.md, which setup does not write.
    let project = checkout(&temp.path().join("claude"));
    let config_dir = temp.path().join("claude-config");
    std::fs::create_dir_all(&config_dir).unwrap();
    update_rules(
        &home,
        &project,
        "claude",
        &[("CLAUDE_CONFIG_DIR", &config_dir)],
    );
    assert_eq!(
        read(&project.join("CLAUDE.md"))
            .matches(FULL_BLOCK_LINE)
            .count(),
        1
    );

    // Codex reads $CODEX_HOME/AGENTS.md, which setup does not write either.
    let project = checkout(&temp.path().join("codex"));
    let codex_home = temp.path().join("codex-home");
    std::fs::create_dir_all(&codex_home).unwrap();
    update_rules(&home, &project, "codex", &[("CODEX_HOME", &codex_home)]);
    assert_eq!(
        read(&project.join("AGENTS.md"))
            .matches(FULL_BLOCK_LINE)
            .count(),
        1
    );

    // An AGENTS.override.md that Codex reads in place of AGENTS.md gets the
    // block too, so the checkout's file can name only the project.
    let project = checkout(&temp.path().join("codex-override"));
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    std::fs::write(
        home.join(".codex/AGENTS.override.md"),
        "My own Codex rules.\n",
    )
    .unwrap();
    update_rules(&home, &project, "codex", &[]);
    let override_rules = read(&home.join(".codex/AGENTS.override.md"));
    assert!(
        override_rules.contains("My own Codex rules."),
        "{override_rules}"
    );
    assert_eq!(
        override_rules.matches(FULL_BLOCK_LINE).count(),
        1,
        "{override_rules}"
    );
    assert_eq!(
        read(&project.join("AGENTS.md"))
            .matches(FULL_BLOCK_LINE)
            .count(),
        0
    );
}

#[test]
fn contextstream_project_rules_full_keeps_the_whole_block_in_the_checkout() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = checkout(temp.path());
    std::fs::create_dir_all(&home).unwrap();
    let full = Path::new("full");
    update_rules(
        &home,
        &project,
        "claude",
        &[("CONTEXTSTREAM_PROJECT_RULES", full)],
    );
    assert_eq!(
        read(&project.join("CLAUDE.md"))
            .matches(FULL_BLOCK_LINE)
            .count(),
        1
    );
}
