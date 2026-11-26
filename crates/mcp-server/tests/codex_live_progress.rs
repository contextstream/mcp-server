//! Setup and Codex: the hooks that give a Codex session live progress.
//!
//! Codex runs a hook only after a person approved it, so setup writes the
//! two it takes (PostToolUse and Stop, both `hook run-progress`) only for a
//! person who turned live progress from the hooks on, says that they wait
//! for approval, and takes them out again when it is off and on uninstall.
//! A person's own hooks are never touched.

use std::path::PathBuf;
use std::process::{Command, Output};

use mcp_server::setup::codex_hooks::{
    codex_hook_identity_hash_for_event, codex_hook_identity_hash_for_platform,
};
use serde_json::{json, Value};

const MARKER: &str = "--contextstream-managed-hook=v1";
const EVENTS: [&str; 2] = ["PostToolUse", "Stop"];

struct Home {
    _temp: tempfile::TempDir,
    home: PathBuf,
    project: PathBuf,
}

impl Home {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        std::fs::create_dir_all(home.join(".codex")).expect("codex dir");
        std::fs::create_dir_all(&project).expect("project dir");
        // The helper the hooks run, where setup expects it.
        let helper = home
            .join(".contextstream")
            .join("bin")
            .join(if cfg!(windows) {
                "contextstream-mcp.exe"
            } else {
                "contextstream-mcp"
            });
        std::fs::create_dir_all(helper.parent().unwrap()).expect("bin dir");
        std::fs::copy(env!("CARGO_BIN_EXE_contextstream-mcp"), &helper).expect("copy helper");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&helper).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&helper, perms).unwrap();
        }
        std::fs::write(
            home.join(".contextstream").join("credentials.json"),
            json!({"api_key": "a-made-up-key", "api_url": "http://127.0.0.1:9"}).to_string(),
        )
        .expect("credentials");
        Self {
            _temp: temp,
            home,
            project,
        }
    }

    fn hooks_path(&self) -> PathBuf {
        self.home.join(".codex").join("hooks.json")
    }

    fn hooks(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.hooks_path()).expect("hooks.json"))
            .expect("hooks.json parses")
    }

    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"));
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("CONTEXTSTREAM_") {
                cmd.env_remove(key);
            }
        }
        cmd.args(args)
            .current_dir(&self.project)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("XDG_CONFIG_HOME", self.home.join(".config"))
            .env("CONTEXTSTREAM_API_URL", "http://127.0.0.1:9")
            .env("CONTEXTSTREAM_WATCH", "0")
            .env("NO_COLOR", "1");
        for (key, value) in env {
            cmd.env(key, value);
        }
        cmd.output().expect("run contextstream-mcp")
    }

    fn update_hooks(&self, env: &[(&str, &str)]) -> String {
        let output = self.run(
            &["update-hooks", "--scope", "global", "--editors", "codex"],
            env,
        );
        assert!(
            output.status.success(),
            "update-hooks failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    }
}

/// The commands of ContextStream's own handlers under `event`.
fn managed(doc: &Value, event: &str) -> Vec<String> {
    commands(doc, event)
        .into_iter()
        .filter(|command| command.contains(MARKER))
        .collect()
}

fn commands(doc: &Value, event: &str) -> Vec<String> {
    let mut found = Vec::new();
    for group in doc["hooks"][event].as_array().into_iter().flatten() {
        for handler in group["hooks"].as_array().into_iter().flatten() {
            if let Some(command) = handler["command"].as_str() {
                found.push(command.to_string());
            }
        }
    }
    found
}

fn the_handler<'a>(doc: &'a Value, event: &str) -> (&'a Value, &'a Value) {
    for group in doc["hooks"][event].as_array().into_iter().flatten() {
        for handler in group["hooks"].as_array().into_iter().flatten() {
            if handler["command"]
                .as_str()
                .is_some_and(|command| command.contains(MARKER))
            {
                return (group, handler);
            }
        }
    }
    panic!("no managed {event} handler in {doc:#}");
}

const ON: [(&str, &str); 1] = [("CONTEXTSTREAM_RUN_PROGRESS_AUTO", "on")];

#[test]
fn as_shipped_codex_gets_the_one_hook_it_always_had() {
    let home = Home::new();
    home.update_hooks(&[]);
    let doc = home.hooks();
    assert_eq!(managed(&doc, "PreToolUse").len(), 1);
    for event in EVENTS {
        assert!(doc["hooks"].get(event).is_none(), "{event}: {doc:#}");
    }
}

#[test]
fn live_progress_on_adds_the_two_hooks_and_says_they_wait_for_approval() {
    let home = Home::new();
    let said = home.update_hooks(&ON);
    let doc = home.hooks();

    // The hook Codex always had is as it was.
    let before_tool = managed(&doc, "PreToolUse");
    assert_eq!(before_tool.len(), 1);
    assert!(
        before_tool[0].contains(" hook mid-run-notices "),
        "{before_tool:?}"
    );

    // One handler for each event Codex tells a session's runs by: a tool
    // call's answer, and the turn's end.
    for event in EVENTS {
        let ours = managed(&doc, event);
        assert_eq!(ours.len(), 1, "{event}: {doc:#}");
        assert!(
            ours[0].ends_with(&format!(" hook run-progress {MARKER}")),
            "{event}: {}",
            ours[0]
        );
        let (group, handler) = the_handler(&doc, event);
        // Every tool, no matcher; a command that must answer within seconds.
        assert!(group.get("matcher").is_none(), "{event}: {group:#}");
        assert_eq!(handler["type"], "command");
        assert_eq!(handler["timeout"], 5);
        assert!(handler.get("async").is_none());
        #[cfg(windows)]
        assert!(handler["commandWindows"].is_string());
    }

    // Codex runs a hook only once a person approved it: setup says so.
    assert!(said.contains("/hooks"), "{said}");
    assert!(
        said.contains("PostToolUse") && said.contains("Stop"),
        "{said}"
    );
    assert!(said.to_lowercase().contains("live progress"), "{said}");

    // Again changes nothing: the handlers Codex approved keep their hash.
    let before = std::fs::read(home.hooks_path()).unwrap();
    home.update_hooks(&ON);
    assert_eq!(std::fs::read(home.hooks_path()).unwrap(), before);
}

#[test]
fn signs_of_life_alone_also_need_the_hooks() {
    let home = Home::new();
    home.update_hooks(&[("CONTEXTSTREAM_RUN_PROGRESS_SIGNS", "on")]);
    let doc = home.hooks();
    for event in EVENTS {
        assert_eq!(managed(&doc, event).len(), 1, "{event}: {doc:#}");
    }
    // Live progress off altogether wins over both switches.
    let home = Home::new();
    home.update_hooks(&[
        ("CONTEXTSTREAM_RUN_PROGRESS_AUTO", "on"),
        ("CONTEXTSTREAM_RUN_PROGRESS", "off"),
    ]);
    let doc = home.hooks();
    for event in EVENTS {
        assert!(managed(&doc, event).is_empty(), "{event}: {doc:#}");
    }
}

#[test]
fn a_persons_own_hooks_stay_and_turning_it_off_takes_ours_out() {
    let home = Home::new();
    let own = json!({
        "hooks": {
            "PostToolUse": [
                {"matcher": "Bash", "hooks": [{"type": "command", "command": "echo audit"}]}
            ],
            "SessionStart": [
                {"hooks": [{"type": "command", "command": "echo hello"}]}
            ]
        }
    });
    std::fs::write(
        home.hooks_path(),
        serde_json::to_string_pretty(&own).unwrap(),
    )
    .unwrap();

    home.update_hooks(&ON);
    let doc = home.hooks();
    // Theirs first and untouched (Codex keys an approval by position), ours
    // after it.
    assert_eq!(
        doc["hooks"]["PostToolUse"][0],
        own["hooks"]["PostToolUse"][0]
    );
    assert_eq!(doc["hooks"]["SessionStart"], own["hooks"]["SessionStart"]);
    assert_eq!(managed(&doc, "PostToolUse").len(), 1);
    assert_eq!(managed(&doc, "Stop").len(), 1);

    // Live progress is turned off again: the next update takes ours out and
    // leaves no empty event behind.
    home.update_hooks(&[]);
    let doc = home.hooks();
    assert_eq!(doc["hooks"]["PostToolUse"], own["hooks"]["PostToolUse"]);
    assert_eq!(doc["hooks"]["SessionStart"], own["hooks"]["SessionStart"]);
    assert!(doc["hooks"].get("Stop").is_none(), "{doc:#}");
    assert_eq!(managed(&doc, "PreToolUse").len(), 1);
}

#[test]
fn uninstall_takes_the_live_progress_hooks_out_too() {
    let home = Home::new();
    let own = json!({
        "hooks": {
            "Stop": [{"hooks": [{"type": "command", "command": "echo bye"}]}]
        }
    });
    std::fs::write(
        home.hooks_path(),
        serde_json::to_string_pretty(&own).unwrap(),
    )
    .unwrap();
    home.update_hooks(&ON);
    assert_eq!(managed(&home.hooks(), "PostToolUse").len(), 1);

    let output = home.run(
        &["uninstall", "--scope", "global", "--editors", "codex"],
        &ON,
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let doc = home.hooks();
    for event in ["PreToolUse", "PostToolUse", "Stop"] {
        assert!(managed(&doc, event).is_empty(), "{event}: {doc:#}");
    }
    // A hook that points at a binary uninstall may have removed would fail
    // on every tool call; a person's own hook is theirs to keep.
    assert_eq!(commands(&doc, "Stop"), ["echo bye"]);
}

/// What Codex writes to `~/.codex/config.toml` when a person approves the
/// hooks under `events` in `/hooks`: each handler's fingerprint, keyed by
/// the hooks file, the event and the handler's place in it.
fn approve(home: &Home, events: &[(&str, &str)]) {
    let doc = home.hooks();
    let mut config = String::new();
    for (event, label) in events {
        let groups = doc["hooks"][*event].as_array().expect("event groups");
        for (group_index, group) in groups.iter().enumerate() {
            for (handler_index, handler) in group["hooks"].as_array().unwrap().iter().enumerate() {
                if !handler["command"]
                    .as_str()
                    .is_some_and(|command| command.contains(MARKER))
                {
                    continue;
                }
                let hash = if *event == "PreToolUse" {
                    codex_hook_identity_hash_for_platform(group, handler, cfg!(windows))
                } else {
                    codex_hook_identity_hash_for_event(group, handler, cfg!(windows), event)
                }
                .expect("fingerprint");
                config.push_str(&format!(
                    "[hooks.state.\"{}:{label}:{group_index}:{handler_index}\"]\ntrusted_hash = \"{hash}\"\n\n",
                    home.hooks_path().display()
                ));
            }
        }
    }
    std::fs::write(home.home.join(".codex").join("config.toml"), config).unwrap();
}

fn doctor_hooks_check(home: &Home) -> Value {
    let output = home.run(
        &[
            "doctor",
            "--json",
            "--scope",
            "global",
            "--editors",
            "codex",
        ],
        &ON,
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("doctor JSON");
    report["editors"][0]["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .find(|check| check["surface"] == "hooks")
        .expect("hooks check")
        .clone()
}

#[test]
fn approval_is_asked_for_until_codex_has_it_and_doctor_says_which_hooks_wait() {
    let home = Home::new();
    home.update_hooks(&ON);

    // Only the hook Codex always had is approved: the two new ones are
    // written and Codex does not run them. Setup and doctor both say so.
    approve(&home, &[("PreToolUse", "pre_tool_use")]);
    let said = home.update_hooks(&ON);
    assert!(said.contains("/hooks"), "{said}");
    assert!(said.contains("PostToolUse and Stop"), "{said}");
    let check = doctor_hooks_check(&home);
    assert_eq!(check["status"], "warn", "{check:#}");
    let detail = check.to_string();
    assert!(detail.contains("PostToolUse and Stop"), "{detail}");
    assert!(detail.contains("/hooks"), "{detail}");

    // One of the two is approved: the other is named alone.
    approve(
        &home,
        &[
            ("PreToolUse", "pre_tool_use"),
            ("PostToolUse", "post_tool_use"),
        ],
    );
    let said = home.update_hooks(&ON);
    assert!(said.contains("under Stop"), "{said}");
    assert!(!said.contains("PostToolUse"), "{said}");

    // All three approved: nothing more is asked, and doctor passes.
    approve(
        &home,
        &[
            ("PreToolUse", "pre_tool_use"),
            ("PostToolUse", "post_tool_use"),
            ("Stop", "stop"),
        ],
    );
    let said = home.update_hooks(&ON);
    assert!(!said.contains("/hooks"), "{said}");
    let check = doctor_hooks_check(&home);
    assert_eq!(check["status"], "pass", "{check:#}");
}
