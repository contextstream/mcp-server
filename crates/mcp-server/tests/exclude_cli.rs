//! Regression tests for `contextstream-mcp exclude` option ordering (#260).

use std::process::{Command, Stdio};

#[test]
fn exclude_add_and_remove_honor_trailing_path_flag_without_touching_cwd() {
    let cwd_dir = tempfile::tempdir().unwrap();
    let target_dir = tempfile::tempdir().unwrap();
    let target_path = target_dir.path().to_str().unwrap();

    let add_out = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
        .args([
            "exclude",
            "add",
            "*.log",
            "coverage/",
            "--path",
            target_path,
        ])
        .env("NO_COLOR", "1")
        .current_dir(cwd_dir.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let add_stderr = String::from_utf8_lossy(&add_out.stderr);
    assert!(add_out.status.success(), "exclude add failed: {add_stderr}");
    assert!(
        !cwd_dir.path().join(".contextignore").exists(),
        "CWD .contextignore must not be created when --path is given"
    );

    let target_ignore = target_dir.path().join(".contextignore");
    let content = std::fs::read_to_string(&target_ignore).expect("target .contextignore created");
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines, vec!["*.log", "coverage/"]);

    let remove_out = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
        .args(["exclude", "remove", "*.log", "--path", target_path])
        .env("NO_COLOR", "1")
        .current_dir(cwd_dir.path())
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let remove_stderr = String::from_utf8_lossy(&remove_out.stderr);
    assert!(
        remove_out.status.success(),
        "exclude remove failed: {remove_stderr}"
    );
    assert!(
        !cwd_dir.path().join(".contextignore").exists(),
        "CWD .contextignore must remain absent after exclude remove --path"
    );

    let after_remove = std::fs::read_to_string(&target_ignore).unwrap();
    let remaining: Vec<&str> = after_remove.lines().collect();
    assert_eq!(remaining, vec!["coverage/"]);
}
