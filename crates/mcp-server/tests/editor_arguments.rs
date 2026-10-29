//! `generate-configs --editors` and `detect-editors --format` reject what they do
//! not understand, as `setup`, `doctor` and `clients` already do (#305). Install
//! scripts and team bootstrap call `generate-configs`, so a typo such as
//! `--editors cluade` used to produce an empty config set and exit 0.

use serde_json::Value;
use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
        .args(args)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("NO_COLOR", "1")
        .env_remove("CONTEXTSTREAM_API_KEY")
        .env_remove("CONTEXTSTREAM_ALLOW_LOCAL_MCP")
        .output()
        .unwrap()
}

fn emitted_editors(output: &Output) -> Vec<String> {
    let parsed: Value = serde_json::from_slice(&output.stdout).expect("JSON on stdout");
    parsed["editors"]
        .as_array()
        .expect("editors array")
        .iter()
        .map(|entry| entry["editor"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn generate_configs_rejects_unknown_editor_ids_without_emitting_anything() {
    for editors in ["bogus", "claude,bogus", "bogus,claude"] {
        let output = run(&["generate-configs", &format!("--editors={editors}")]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{editors}: {stderr}");
        assert!(
            stderr.contains("Unknown editor id 'bogus'. Supported ids: "),
            "{editors}: {stderr}"
        );
        assert!(stderr.contains("claude"), "lists the valid ids: {stderr}");
        assert!(
            output.stdout.is_empty(),
            "{editors}: nothing is emitted for a config set that is wrong:\n{}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}

#[test]
fn generate_configs_rejects_an_empty_editor_id() {
    let output = run(&["generate-configs", "--editors=claude,"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(stderr.contains("empty editor id"), "{stderr}");
    assert!(output.stdout.is_empty());
}

#[test]
fn generate_configs_still_accepts_valid_ids_with_spaces_and_repeats() {
    let one = run(&["generate-configs", "--editors=claude"]);
    assert!(
        one.status.success(),
        "{}",
        String::from_utf8_lossy(&one.stderr)
    );
    assert_eq!(emitted_editors(&one), ["claude"]);

    let several = run(&["generate-configs", "--editors", "claude, cursor,claude"]);
    assert!(
        several.status.success(),
        "{}",
        String::from_utf8_lossy(&several.stderr)
    );
    assert_eq!(emitted_editors(&several), ["claude", "cursor"]);
}

#[test]
fn detect_editors_rejects_an_unknown_format() {
    let output = run(&["detect-editors", "--format", "yaml"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(2), "{stderr}");
    assert!(
        stderr.contains("Unknown --format 'yaml'. Use json."),
        "{stderr}"
    );
    assert!(
        output.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stdout)
    );

    for format in ["json", "JSON", " json "] {
        let accepted = run(&["detect-editors", "--format", format]);
        assert!(
            accepted.status.success(),
            "--format {format:?}: {}",
            String::from_utf8_lossy(&accepted.stderr)
        );
        serde_json::from_slice::<Value>(&accepted.stdout).expect("JSON on stdout");
    }
    let default = run(&["detect-editors"]);
    assert!(default.status.success());
}
