//! Commands that would prompt must say so up front when there is no terminal (a
//! script, a CI job or a coding agent) and name the non-interactive way to do
//! the same thing, instead of surfacing the prompt library's "IO error: not a
//! terminal" after a screen of banner text.

use std::{
    path::Path,
    process::{Command, Output, Stdio},
};

fn run(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
        .args(args)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("NO_COLOR", "1")
        .env("CONTEXTSTREAM_WATCH", "0")
        .env("CONTEXTSTREAM_API_URL", "http://127.0.0.1:9")
        .env_remove("CONTEXTSTREAM_API_KEY")
        .env_remove("CONTEXTSTREAM_JWT")
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

fn assert_asks_for_a_terminal(output: &Output, stderr: &str) {
    assert!(!output.status.success(), "expected a failure:\n{stderr}");
    assert!(
        stderr.contains("needs a terminal, and this session has none."),
        "{stderr}"
    );
    assert!(!stderr.contains("not a terminal"), "{stderr}");
    assert!(!stderr.contains("IO error"), "{stderr}");
}

#[test]
fn setup_without_a_terminal_explains_how_to_run_it_unattended() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();

    for args in [&["setup"][..], &["setup", "--dry-run"][..]] {
        let output = run(&home, &project, args);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_asks_for_a_terminal(&output, &stderr);
        assert!(
            stderr.starts_with("Setup failed: Guided setup needs a terminal"),
            "{stderr}"
        );
        assert!(stderr.contains("configure --api-key-stdin"), "{stderr}");
        assert!(stderr.contains("CONTEXTSTREAM_API_KEY"), "{stderr}");
        assert!(
            stderr.contains("`contextstream-mcp setup --yes`"),
            "{stderr}"
        );
        assert!(
            stdout.trim().is_empty(),
            "no banner before the failure:\n{stdout}"
        );
    }
    assert_eq!(
        std::fs::read_dir(&home).unwrap().count(),
        0,
        "nothing is written to HOME"
    );
}

#[test]
fn unattended_setup_keeps_its_own_missing_key_message() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();

    let output = run(&home, &project, &["setup", "--yes"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{stderr}");
    assert!(stderr.contains("--yes requires an API key"), "{stderr}");
    assert!(!stderr.contains("needs a terminal"), "{stderr}");
}

#[test]
fn configure_menu_without_a_terminal_points_at_the_flags() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();

    let output = run(&home, temp.path(), &["configure"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_asks_for_a_terminal(&output, &stderr);
    assert!(
        stderr.contains("Configure failed: The configure menu needs a terminal"),
        "{stderr}"
    );
    assert!(
        stderr.contains("--account-learning status|on|off"),
        "{stderr}"
    );
    assert!(stderr.contains("--transcripts on|off"), "{stderr}");
    assert!(stderr.contains("configure --api-key-stdin"), "{stderr}");
}

#[test]
fn index_and_ingest_without_credentials_or_a_terminal_say_how_to_sign_in() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();

    for (command, failed) in [("index", "Index failed"), ("ingest", "Ingest failed")] {
        let output = run(&home, &project, &[command]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_asks_for_a_terminal(&output, &stderr);
        assert!(
            stderr.contains(&format!("{failed}: Signing in needs a terminal")),
            "{stderr}"
        );
        assert!(stderr.contains("CONTEXTSTREAM_API_KEY"), "{stderr}");
        assert!(stderr.contains("configure --api-key-stdin"), "{stderr}");
        assert!(
            !stderr.contains("Authenticating"),
            "must not claim to authenticate:\n{stderr}"
        );
    }
}

#[test]
fn index_and_ingest_with_a_key_but_no_linked_folder_say_how_to_link_it() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = temp.path().join("project");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    assert!(Command::new("git")
        .args(["init", "-q"])
        .current_dir(&project)
        .status()
        .unwrap()
        .success());

    for (command, failed) in [("index", "Index failed"), ("ingest", "Ingest failed")] {
        // A key is set, so there is nothing to sign in to: the folder just has
        // no workspace yet, and choosing one is a prompt.
        let output = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
            .arg(command)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("NO_COLOR", "1")
            .env("CONTEXTSTREAM_WATCH", "0")
            .env("CONTEXTSTREAM_API_URL", "http://127.0.0.1:9")
            .env("CONTEXTSTREAM_API_KEY", "cs_test_not_used")
            .env_remove("CONTEXTSTREAM_WORKSPACE_ID")
            .current_dir(&project)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_asks_for_a_terminal(&output, &stderr);
        assert!(
            stderr.contains(&format!("{failed}: Choosing a workspace needs a terminal")),
            "{stderr}"
        );
        assert!(stderr.contains("setup --yes"), "{stderr}");
        assert!(stderr.contains("CONTEXTSTREAM_WORKSPACE_ID"), "{stderr}");
        assert!(
            !stderr.contains("Starting workspace setup"),
            "must not claim to start a setup it cannot run:\n{stderr}"
        );
    }
}
