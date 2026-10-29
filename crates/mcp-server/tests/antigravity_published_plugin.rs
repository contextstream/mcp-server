//! The public Antigravity plugin (#331) installed by `agy plugin install` and
//! then met by `setup`'s own commands. Runs the real binary through offline
//! commands.

use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

fn run(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
        .args(args)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("NO_COLOR", "1")
        .env("CONTEXTSTREAM_WATCH", "0")
        .env("CONTEXTSTREAM_API_URL", "http://127.0.0.1:9")
        .env_remove("CONTEXTSTREAM_API_KEY")
        .current_dir(home)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn files_under(root: &Path) -> Vec<(PathBuf, String)> {
    fn walk(dir: &Path, root: &Path, found: &mut Vec<(PathBuf, String)>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir).unwrap().flatten().collect();
        entries.sort_by_key(|entry| entry.path());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, root, found);
            } else {
                found.push((
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    std::fs::read_to_string(&path).unwrap(),
                ));
            }
        }
    }
    let mut found = Vec::new();
    walk(root, root, &mut found);
    found
}

/// Export the plugin into `home`'s Antigravity plugins folder, as
/// `agy plugin install` leaves it.
fn install_published_plugin(home: &Path) -> PathBuf {
    let plugin = home.join(".gemini/config/plugins/contextstream");
    std::fs::create_dir_all(&plugin).unwrap();
    let output = run(
        home,
        &[
            "export-antigravity-plugin",
            "--out",
            plugin.to_str().unwrap(),
        ],
    );
    assert!(output.status.success(), "export failed:\n{}", text(&output));
    plugin
}

#[test]
fn export_lists_every_file_of_the_public_plugin() {
    let temp = tempfile::tempdir().unwrap();
    let output = run(temp.path(), &["export-antigravity-plugin", "--json"]);
    assert!(output.status.success(), "{}", text(&output));
    let files: std::collections::BTreeMap<String, String> =
        serde_json::from_slice(&output.stdout).expect("--json prints one JSON object");
    assert_eq!(
        files.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            "README.md",
            "hooks.json",
            "mcp_config.json",
            "plugin.json",
            "rules/AGENTS.md",
            "skills/contextstream/SKILL.md",
            "skills/contextstream/references/reference.md",
        ]
    );
    for (path, content) in &files {
        assert!(
            !content.contains("cs_"),
            "{path} holds something key-shaped"
        );
    }

    // Neither --out nor --json: say what to give, and write nothing.
    let output = run(temp.path(), &["export-antigravity-plugin"]);
    assert_eq!(output.status.code(), Some(2), "{}", text(&output));
    assert!(text(&output).contains("--out"), "{}", text(&output));
}

#[test]
fn setups_own_commands_work_on_the_installed_public_plugin() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    let plugin = install_published_plugin(home);
    let installed = files_under(&plugin);

    // A refresh recognises the rules as ContextStream's and changes nothing.
    let output = run(
        home,
        &[
            "update-rules",
            "--scope",
            "global",
            "--editors",
            "antigravity",
        ],
    );
    assert!(output.status.success(), "update-rules:\n{}", text(&output));
    assert_eq!(
        files_under(&plugin),
        installed,
        "update-rules changed files"
    );

    // doctor sees the rules and the hooks as its own.
    let output = run(
        home,
        &["doctor", "--scope", "global", "--editors", "antigravity"],
    );
    let report = text(&output);
    assert!(!report.contains("has no ContextStream block"), "{report}");
    assert!(!report.contains("does not run"), "{report}");
    assert!(report.contains("plugin"), "{report}");

    // Uninstall takes out what setup owns and does not fail on the rest.
    let output = run(
        home,
        &["uninstall", "--scope", "global", "--editors", "antigravity"],
    );
    assert!(output.status.success(), "uninstall:\n{}", text(&output));
    assert!(
        text(&output).contains("agy plugin uninstall contextstream"),
        "{}",
        text(&output)
    );
    let left = files_under(&plugin);
    for (path, content) in &left {
        assert!(
            !content.contains("<contextstream>"),
            "{} still holds the rules block",
            path.display()
        );
    }
    println!(
        "left after uninstall: {:?}",
        left.iter().map(|(path, _)| path).collect::<Vec<_>>()
    );
}

fn hook_output(home: &Path, hook: &str, key: Option<&str>) -> String {
    use std::io::Write;
    let mut command = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"));
    command
        .args(["hook", hook, "--contextstream-managed-hook=v1"])
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("CONTEXTSTREAM_WATCH", "0")
        .env("CONTEXTSTREAM_API_URL", "http://127.0.0.1:9")
        .env_remove("CONTEXTSTREAM_API_KEY")
        .current_dir(home)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    if let Some(key) = key {
        command.env("CONTEXTSTREAM_API_KEY", key);
    }
    let mut child = command.spawn().unwrap();
    let input = serde_json::json!({
        "conversationId": "550e8400-e29b-41d4-a716-446655440000",
        "workspacePaths": [home.join("app")],
        "invocationNum": 0,
    });
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.to_string().as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

#[test]
fn the_hooks_claim_a_connection_only_where_there_is_a_key() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path();
    std::fs::create_dir_all(home.join("app")).unwrap();

    // The plugin installed from GitHub on a machine where setup never ran.
    for hook in ["agy-session-start", "agy-pre-invocation"] {
        assert_eq!(hook_output(home, hook, None), "{}", "{hook} without a key");
    }
    // With a key, as after setup, the same hooks speak.
    let started = hook_output(home, "agy-session-start", Some("cs_test_not_a_real_key"));
    assert!(started.contains("ContextStream is connected"), "{started}");
    let invoked = hook_output(home, "agy-pre-invocation", Some("cs_test_not_a_real_key"));
    assert!(invoked.contains("Ground this request"), "{invoked}");
}
