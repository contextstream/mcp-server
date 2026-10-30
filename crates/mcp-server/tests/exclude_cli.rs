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

// #349: `exclude` found problems with a `.contextignore` it cannot read and with
// a `--path` that is not a folder.

fn exclude(args: &[&str]) -> (bool, String) {
    let output = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
        .arg("exclude")
        .args(args)
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.success(), text)
}

#[test]
fn a_contextignore_that_cannot_be_read_is_reported_and_never_overwritten() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    let ignore = dir.path().join(".contextignore");
    // Not valid UTF-8 (a Latin-1 e acute): the person's other patterns must survive.
    let original: &[u8] = b"keep-me/\ncaf\xe9/\nbuild/\n";
    std::fs::write(&ignore, original).unwrap();

    for args in [
        vec!["list", "--path", path],
        vec!["add", "newpat", "--path", path],
        vec!["remove", "build/", "--path", path],
    ] {
        let (ok, text) = exclude(&args);
        assert!(!ok, "{args:?} must fail: {text}");
        assert!(text.contains("not valid UTF-8"), "{args:?}: {text}");
        assert!(text.contains(".contextignore"), "{args:?}: {text}");
        assert_eq!(
            std::fs::read(&ignore).unwrap(),
            original,
            "{args:?} left the file alone"
        );
    }

    // A `.contextignore` that is a folder cannot be read either.
    let other = tempfile::tempdir().unwrap();
    std::fs::create_dir(other.path().join(".contextignore")).unwrap();
    let (ok, text) = exclude(&["add", "x", "--path", other.path().to_str().unwrap()]);
    assert!(!ok, "{text}");
    assert!(text.contains("Could not read"), "{text}");
}

#[test]
fn a_path_that_is_not_a_folder_is_named_in_the_error_for_every_action() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nodir");
    let missing = missing.to_str().unwrap();
    let file = dir.path().join("file.txt");
    std::fs::write(&file, "hi").unwrap();
    let file = file.to_str().unwrap();

    for action in ["list", "reset"] {
        let (ok, text) = exclude(&[action, "--path", missing]);
        assert!(!ok, "{action}: {text}");
        assert!(
            text.contains(&format!("Path does not exist: {missing}")),
            "{action}: {text}"
        );
        let (ok, text) = exclude(&[action, "--path", file]);
        assert!(!ok, "{action}: {text}");
        assert!(
            text.contains(&format!("Path must be a directory: {file}")),
            "{action}: {text}"
        );
    }
    let (ok, text) = exclude(&["add", "x/**", "--path", missing]);
    assert!(!ok && text.contains("Path does not exist"), "{text}");
    assert!(
        !std::path::Path::new(missing).exists(),
        "nothing was created"
    );
    // `info` has nothing to do with a folder.
    let (ok, _) = exclude(&["info", "--path", missing]);
    assert!(ok);
}

#[test]
fn patterns_that_could_exclude_nothing_are_refused_and_the_rest_are_trimmed_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    let ignore = dir.path().join(".contextignore");

    for bad in ["", "   "] {
        let (ok, text) = exclude(&["add", bad, "--path", path]);
        assert!(!ok && text.contains("Empty pattern"), "{bad:?}: {text}");
    }
    let (ok, text) = exclude(&["add", "#comment", "--path", path]);
    assert!(!ok, "{text}");
    assert!(
        text.contains("would be a comment") && text.contains("\\#comment"),
        "{text}"
    );
    assert!(!ignore.exists(), "a refused add writes nothing");

    // Trimmed, and the same pattern twice in one call is added once.
    let (ok, text) = exclude(&["add", "  spaced  ", "dist/", "dist/", "--path", path]);
    assert!(ok, "{text}");
    assert_eq!(std::fs::read_to_string(&ignore).unwrap(), "spaced\ndist/\n");
    // A line an older version stored with spaces is still removable as typed.
    std::fs::write(&ignore, "  old  \nkeep/\n").unwrap();
    let (ok, text) = exclude(&["remove", "old", "--path", path]);
    assert!(ok, "{text}");
    assert_eq!(std::fs::read_to_string(&ignore).unwrap(), "keep/\n");
}

#[test]
fn a_crlf_contextignore_keeps_its_line_endings_when_edited() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    let ignore = dir.path().join(".contextignore");
    std::fs::write(&ignore, "x\r\ny\r\n").unwrap();

    let (ok, text) = exclude(&["add", "z", "--path", path]);
    assert!(ok, "{text}");
    assert_eq!(std::fs::read(&ignore).unwrap(), b"x\r\ny\r\nz\r\n");
    let (ok, text) = exclude(&["remove", "x", "--path", path]);
    assert!(ok, "{text}");
    assert_eq!(std::fs::read(&ignore).unwrap(), b"y\r\nz\r\n");
}
