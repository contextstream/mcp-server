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

/// Records `clients` as the editors setup configured on this machine.
fn record_setup_selection(home: &Path, clients: &[&str]) {
    let dir = home.join(".contextstream");
    std::fs::create_dir_all(&dir).unwrap();
    let clients = clients
        .iter()
        .map(|client| format!("\"{client}\""))
        .collect::<Vec<_>>()
        .join(",");
    std::fs::write(
        dir.join("installation.json"),
        format!(
            "{{\"schema_version\":1,\"installation_id\":\"{WORKSPACE_ID}\",\"created_at\":\"2026-11-03T00:00:00Z\",\"configured_clients\":[{clients}],\"last_install_completed_at\":\"2026-11-03T00:00:00Z\"}}\n"
        ),
    )
    .unwrap();
}

/// Runs `update-rules` in `dir` the way `contextstream-mcp update` does after
/// an upgrade (`--only-configured`), and returns what it printed.
fn refresh_as_update_does(home: &Path, dir: &Path, scope: &str, env: &[(&str, &str)]) -> String {
    let mut command = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"));
    command
        .args([
            "update-rules",
            &format!("--scope={scope}"),
            "--only-configured",
            &format!("--workspace-id={WORKSPACE_ID}"),
            "--workspace-name=Example Team",
        ])
        .current_dir(dir)
        .env_clear()
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
    let printed = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.status.success(), "update-rules failed: {printed}");
    printed
}

/// A machine whose global rules files hold no block (Claude Code and Codex set
/// up, then the block taken out of `~/.claude/CLAUDE.md` and
/// `~/.codex/AGENTS.md`) with a checkout whose files hold it.
fn machine_keeping_the_rules_in_project_files(
    root: &Path,
) -> (std::path::PathBuf, std::path::PathBuf) {
    let home = root.join("home");
    let project = checkout(root);
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    record_setup_selection(&home, &["claude", "codex"]);
    std::fs::write(home.join(".claude/CLAUDE.md"), "# My Claude notes\n").unwrap();
    std::fs::write(home.join(".codex/AGENTS.md"), "# My Codex notes\n").unwrap();
    let printed = refresh_as_update_does(&home, &project, "project", &[]);
    for file in ["CLAUDE.md", "AGENTS.md"] {
        assert_eq!(
            read(&project.join(file)).matches(FULL_BLOCK_LINE).count(),
            1,
            "{file}: {printed}"
        );
    }
    assert!(
        printed.contains("`~/.claude/CLAUDE.md` holds no ContextStream block"),
        "{printed}"
    );
    (home, project)
}

/// The Mac, 2026-11-03: its global rules files were left without the block,
/// so the checkout's files held it. 1.0.62's `update`, run outside the
/// checkout, put the block back into the global files, and every session in
/// the checkout loaded it twice again. Now each editor's block stays where it
/// is.
#[test]
fn update_leaves_the_rules_in_project_files_when_the_global_files_hold_none() {
    let temp = tempfile::tempdir().unwrap();
    let (home, project) = machine_keeping_the_rules_in_project_files(temp.path());
    let elsewhere = temp.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();

    // What `update` runs, from a folder that is no project.
    let printed = refresh_as_update_does(&home, &elsewhere, "global", &[]);
    assert_eq!(
        read(&home.join(".claude/CLAUDE.md")),
        "# My Claude notes\n",
        "{printed}"
    );
    assert_eq!(
        read(&home.join(".codex/AGENTS.md")),
        "# My Codex notes\n",
        "{printed}"
    );
    assert!(
        printed.contains("left `~/.claude/CLAUDE.md` without the ContextStream block"),
        "{printed}"
    );

    // And from the checkout: each editor still loads the block once there.
    refresh_as_update_does(&home, &project, "global", &[]);
    refresh_as_update_does(&home, &project, "project", &[]);
    for (global, local) in [
        (".claude/CLAUDE.md", "CLAUDE.md"),
        (".codex/AGENTS.md", "AGENTS.md"),
    ] {
        let loaded = read(&home.join(global)) + &read(&project.join(local));
        assert_eq!(
            loaded.matches(FULL_BLOCK_LINE).count(),
            1,
            "{global} + {local}"
        );
    }
}

/// `CONTEXTSTREAM_PROJECT_RULES=header` moves the rules back into the global
/// file, and later refreshes keep them there.
#[test]
fn the_header_choice_puts_the_block_back_in_the_global_file() {
    let temp = tempfile::tempdir().unwrap();
    let (home, project) = machine_keeping_the_rules_in_project_files(temp.path());

    let printed = refresh_as_update_does(
        &home,
        &project,
        "all",
        &[("CONTEXTSTREAM_PROJECT_RULES", "header")],
    );
    let global = read(&home.join(".claude/CLAUDE.md"));
    let local = read(&project.join("CLAUDE.md"));
    assert!(global.contains("# My Claude notes"), "{global}");
    assert_eq!(global.matches(FULL_BLOCK_LINE).count(), 1, "{global}");
    assert_eq!(local.matches(FULL_BLOCK_LINE).count(), 0, "{local}");
    assert!(
        local.contains(&format!("# Workspace ID: {WORKSPACE_ID}")),
        "{local}"
    );
    assert!(
        printed.contains("`CLAUDE.md` names the project only"),
        "{printed}"
    );

    // Without the choice, refreshes keep this layout too.
    refresh_as_update_does(&home, temp.path(), "global", &[]);
    refresh_as_update_does(&home, &project, "project", &[]);
    assert_eq!(read(&project.join("CLAUDE.md")), local);
    assert_eq!(
        read(&home.join(".claude/CLAUDE.md"))
            .matches(FULL_BLOCK_LINE)
            .count(),
        1
    );
}

/// The Mac, 2026-11-04: `update` run in HOME took HOME for a set-up project,
/// since the global files live there, and wrote HOME's project header into
/// `~/.claude/CLAUDE.md`, which is also Claude Code's project path
/// `.claude/CLAUDE.md` for a project at HOME. HOME is no project: its global
/// files stay as they are and no `~/CLAUDE.md` appears.
#[test]
fn a_rules_refresh_in_the_home_folder_writes_no_project_rules() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let elsewhere = checkout(temp.path());
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    record_setup_selection(&home, &["claude"]);
    std::fs::write(home.join(".claude/CLAUDE.md"), "# My Claude notes\n").unwrap();
    // Setup wrote the global block (from a project folder).
    refresh_as_update_does(
        &home,
        &elsewhere,
        "global",
        &[("CONTEXTSTREAM_PROJECT_RULES", "header")],
    );
    let global = read(&home.join(".claude/CLAUDE.md"));
    assert_eq!(global.matches(FULL_BLOCK_LINE).count(), 1, "{global}");

    for scope in ["project", "all"] {
        let printed = refresh_as_update_does(&home, &home, scope, &[]);
        assert_eq!(
            read(&home.join(".claude/CLAUDE.md")),
            global,
            "{scope}: {printed}"
        );
        assert!(!home.join("CLAUDE.md").exists(), "{scope}: {printed}");
        assert!(
            printed.contains("Project rules: skipped"),
            "{scope}: {printed}"
        );
    }
}
