//! Behavioral contract for every setup client.
//!
//! Pins, per editor: every path the setup code resolves, the capability
//! flags, the exact files each write path produces (global and project,
//! remote and local, fresh and merged into an existing user config), the
//! `generate-configs` payload, and which single piece of install evidence
//! detects it. Refactors of the client registry must keep this byte-for-byte
//! identical; intentional changes are re-blessed with
//! `CONTEXTSTREAM_BLESS_CLIENT_CONTRACT=1 cargo test -p mcp-server --lib client_contract`.
//!
//! Linux only: paths are platform-specific and CI runs tests on Linux.

use super::*;
use crate::env_test_mutex;
use serde_json::{json, Map, Value};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

const CONTRACT_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/src/setup/testdata/client_contract.json"
);
const BLESS_ENV: &str = "CONTEXTSTREAM_BLESS_CLIENT_CONTRACT";

/// Clears every `CONTEXTSTREAM_*` variable plus HOME/XDG/PATH for the
/// duration of a contract run, restoring the exact previous environment.
struct IsolatedEnv {
    saved: Vec<(OsString, Option<OsString>)>,
}

impl IsolatedEnv {
    fn new(home: &Path, path_dir: &Path) -> Self {
        let mut keys: Vec<OsString> = std::env::vars_os()
            .map(|(key, _)| key)
            .filter(|key| key.to_string_lossy().starts_with("CONTEXTSTREAM_"))
            .collect();
        for key in ["HOME", "XDG_CONFIG_HOME", "PATH"] {
            keys.push(OsString::from(key));
        }
        let saved = keys
            .into_iter()
            .map(|key| {
                let value = std::env::var_os(&key);
                (key, value)
            })
            .collect::<Vec<_>>();
        for (key, _) in &saved {
            std::env::remove_var(key);
        }
        std::env::set_var("HOME", home);
        std::env::set_var("XDG_CONFIG_HOME", home.join(".config"));
        std::env::set_var("PATH", path_dir);
        std::env::set_var("CONTEXTSTREAM_API_URL", mcp_types::config::DEFAULT_API_URL);
        Self { saved }
    }
}

impl Drop for IsolatedEnv {
    fn drop(&mut self) {
        for (key, value) in self.saved.drain(..) {
            match value {
                Some(value) => std::env::set_var(&key, value),
                None => std::env::remove_var(&key),
            }
        }
    }
}

struct Sandbox {
    _root: tempfile::TempDir,
    home: PathBuf,
    project: PathBuf,
    bin: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let root = tempfile::tempdir().expect("sandbox");
        let home = root.path().join("home");
        let project = root.path().join("project");
        let bin = root.path().join("bin");
        for dir in [&home, &project, &bin] {
            std::fs::create_dir_all(dir).expect("sandbox dir");
        }
        // Setup installs the managed helper before writing configs. Without
        // it, local entries fall back to the running test binary's path,
        // which differs per build and machine.
        let helper = home.join(".contextstream").join("bin");
        std::fs::create_dir_all(&helper).expect("helper dir");
        std::fs::write(helper.join("contextstream-mcp"), "").expect("helper stub");
        Self {
            _root: root,
            home,
            project,
            bin,
        }
    }

    fn normalize(&self, text: &str) -> String {
        text.replace(&self.project.to_string_lossy().to_string(), "<project>")
            .replace(&self.home.to_string_lossy().to_string(), "~")
    }

    fn display(&self, path: Option<PathBuf>) -> Value {
        match path {
            Some(path) => json!(self.normalize(&path.to_string_lossy())),
            None => Value::Null,
        }
    }

    fn displays(&self, paths: Vec<PathBuf>) -> Value {
        Value::Array(paths.into_iter().map(|p| self.display(Some(p))).collect())
    }

    /// Every regular file under HOME and the project (ContextStream's own
    /// state excluded), keyed by normalized path, with normalized content.
    fn files(&self) -> Value {
        let mut files = Map::new();
        for root in [&self.home, &self.project] {
            collect_files(root, &mut |path| {
                if path
                    .components()
                    .any(|component| component.as_os_str() == ".contextstream")
                {
                    return;
                }
                let content = std::fs::read_to_string(path).unwrap_or_else(|_| "<binary>".into());
                files.insert(
                    self.normalize(&path.to_string_lossy()),
                    json!(self.normalize(&content)),
                );
            });
        }
        Value::Object(files)
    }
}

fn collect_files(dir: &Path, visit: &mut dyn FnMut(&Path)) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by_key(|entry| entry.path());
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, visit);
        } else {
            visit(&path);
        }
    }
}

fn outcome(result: Result<()>, sandbox: &Sandbox) -> Value {
    match result {
        Ok(()) => sandbox.files(),
        Err(error) => json!({ "error": sandbox.normalize(&format!("{error:#}")) }),
    }
}

fn seed_existing_user_config(editor: &Editor) {
    let Some(path) = editor.mcp_config_path() else {
        return;
    };
    std::fs::create_dir_all(path.parent().expect("config parent")).expect("config dir");
    let seed = match mcp_root_key(editor) {
        Some(root) => format!(
            "{{\n  \"userSetting\": true,\n  \"{root}\": {{\n    \"user-server\": {{\n      \"command\": \"user-command\"\n    }}\n  }}\n}}\n"
        ),
        None if matches!(editor, Editor::Codex) => {
            "model = \"user-model\"\n\n[mcp_servers.user]\ncommand = \"user-command\"\n".to_string()
        }
        None => return,
    };
    std::fs::write(&path, seed).expect("seed user config");
}

fn editor_contract(editor: &Editor) -> Value {
    let identity = ManagedConfigIdentity::for_write().expect("test identity");
    let run = |scenario: &dyn Fn(&Sandbox) -> Result<()>| {
        let sandbox = Sandbox::new();
        let _env = IsolatedEnv::new(&sandbox.home, &sandbox.bin);
        let result = scenario(&sandbox);
        outcome(result, &sandbox)
    };

    let paths = {
        let sandbox = Sandbox::new();
        let _env = IsolatedEnv::new(&sandbox.home, &sandbox.bin);
        let project = sandbox.project.as_path();
        json!({
            "mcp_config": sandbox.display(editor.mcp_config_path()),
            "legacy_mcp_config": sandbox.displays(editor.legacy_mcp_config_paths()),
            "project_mcp_config": sandbox.display(editor.project_mcp_config_path(project)),
            "rules_global": sandbox.display(editor.rules_path(None)),
            "rules_project": sandbox.display(editor.rules_path(Some(project))),
            "legacy_rules_global": sandbox.displays(editor.legacy_rules_paths(None)),
            "legacy_rules_project": sandbox.displays(editor.legacy_rules_paths(Some(project))),
            "cleanup_rules_global": sandbox.displays(editor.legacy_cleanup_only_rules_paths(None)),
            "cleanup_rules_project": sandbox.displays(editor.legacy_cleanup_only_rules_paths(Some(project))),
        })
    };

    let flags = json!({
        "id": editor.id(),
        "display_name": editor.display_name(),
        "supports_project_mcp_config": editor.supports_project_mcp_config(),
        "uses_json_config": editor.uses_json_config(),
        "uses_vscode_settings": editor.uses_vscode_settings(),
        "has_hooks": editor.has_hooks(),
        "has_mcp_transport": editor.has_mcp_transport(),
        "supports_remote": editor_supports_remote_mcp(editor),
        "enforcement_tier": format!("{:?}", editor.enforcement_tier()),
        "is_deprecated": editor.is_deprecated(),
        "deprecation_successor": editor.deprecation_successor().map(|e| e.id()),
        "mcp_root_key": mcp_root_key(editor),
        "tool_surface_profile": default_tool_surface_profile_for_editor(editor),
        "reload_instruction": editor.activation_reload_instruction(),
    });

    let writes = json!({
        "global_remote": run(&|_| write_mcp_config_force_remote_with_auth(
            editor, "test-key", Some("ws-1"), Some("proj-1"), None, None, Some("test-key"),
        )),
        "global_local": run(&|_| write_mcp_config_force_local(
            editor, "test-key", Some("ws-1"), Some("proj-1"), None, None,
        )),
        "global_merge_existing": run(&|_| {
            seed_existing_user_config(editor);
            write_mcp_config_with_remote_auth(
                editor, "test-key", Some("ws-1"), Some("proj-1"), None, None, Some("test-key"),
            )
        }),
        "project_remote": run(&|sandbox| write_project_mcp_config_force_remote_with_auth(
            editor, &sandbox.project, "test-key", Some("ws-1"), Some("proj-1"), None, None, Some("test-key"),
        )),
        "project_local": run(&|sandbox| write_project_mcp_config_force_local(
            editor, &sandbox.project, "test-key", Some("ws-1"), Some("proj-1"), None, None,
        )),
    });

    let generated = {
        let sandbox = Sandbox::new();
        let _env = IsolatedEnv::new(&sandbox.home, &sandbox.bin);
        std::env::set_var("CONTEXTSTREAM_ALLOW_LOCAL_MCP", "1");
        let mut generated = Map::new();
        for transport in ["remote", "local"] {
            let mut value = generate_config_json_with_identity(
                editor,
                "test-key",
                Some("ws-1"),
                Some("proj-1"),
                transport,
                Some("test-key"),
                &identity,
            );
            // Depends on the test process's working directory.
            value
                .as_object_mut()
                .map(|object| object.remove("project_config_path"));
            let text = sandbox.normalize(&value.to_string());
            generated.insert(
                transport.to_string(),
                serde_json::from_str(&text).expect("json"),
            );
        }
        Value::Object(generated)
    };

    json!({ "flags": flags, "paths": paths, "writes": writes, "generated": generated })
}

/// Editors whose detection also probes absolute system paths
/// (`/usr/bin/cursor`, ...), which a sandbox cannot hide. They are left out
/// of the matrix rows and checked positively instead, so the snapshot does
/// not depend on what the host has installed.
const HOST_PROBING_EDITORS: &[&str] = &["cursor", "windsurf"];

/// Which editors a single piece of install evidence detects, in a sandbox
/// with an otherwise empty HOME and PATH.
fn detection_matrix() -> Value {
    const BINARIES: &[&str] = &[
        "claude",
        "cursor",
        "windsurf",
        "copilot",
        "kilo",
        "codex",
        "aider",
        "antigravity",
        "agy",
        "opencode",
        "muse",
        "kimi",
        "zcode",
        "qwen",
        "gemini",
        "zed",
        "zeditor",
        "zedit",
        "droid",
        "amp",
        "crush",
    ];
    const HOME_PATHS: &[&str] = &[
        ".claude/",
        ".claude/settings.json",
        ".claude/settings.local.json",
        ".claude/.credentials.json",
        ".claude/projects/",
        ".cursor/",
        ".codeium/windsurf/",
        ".copilot/",
        ".config/kilo/",
        ".config/opencode/",
        ".opencode/",
        ".gemini/",
        ".gemini/antigravity/",
        ".gemini/config/mcp_config.json",
        "Applications/cursor.AppImage",
        "Applications/Windsurf.AppImage",
        "Applications/windsurf.AppImage",
        ".vscode/extensions/github.copilot-1.0.0/",
        ".vscode/extensions/github.copilot-chat-1.0.0/",
        ".vscode/extensions/saoudrizwan.claude-dev-1.0.0/",
        ".vscode/extensions/rooveterinaryinc.roo-cline-1.0.0/",
        ".vscode/extensions/kilocode.kilo-code-1.0.0/",
        ".config/muse/",
        ".kimi-code/",
        ".kimi/",
        ".zcode/",
        ".qwen/",
        ".gemini/settings.json",
        ".config/zed/",
        ".config/Claude/",
        ".factory/",
        ".config/amp/",
        ".config/crush/",
    ];
    let detect = |setup: &dyn Fn(&Sandbox)| {
        let sandbox = Sandbox::new();
        let _env = IsolatedEnv::new(&sandbox.home, &sandbox.bin);
        setup(&sandbox);
        let ids = |editors: Vec<Editor>| -> Vec<&'static str> {
            editors
                .iter()
                .map(|e| e.id())
                .filter(|id| !HOST_PROBING_EDITORS.contains(id))
                .collect()
        };
        json!({
            "all": ids(super::super::editors::detect_installed_editors()),
            "setup": ids(super::super::editors::detect_installed_editors_for_setup()),
        })
    };

    let mut matrix = Map::new();
    matrix.insert("<empty>".into(), detect(&|_| {}));
    for binary in BINARIES {
        matrix.insert(
            format!("bin:{binary}"),
            detect(&|sandbox| {
                let path = sandbox.bin.join(binary);
                std::fs::write(&path, "#!/bin/sh\n").expect("stub");
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                    .expect("chmod");
            }),
        );
    }
    for relative in HOME_PATHS {
        matrix.insert(
            format!("home:{relative}"),
            detect(&|sandbox| {
                let path = sandbox.home.join(relative);
                if relative.ends_with('/') {
                    std::fs::create_dir_all(&path).expect("evidence dir");
                } else {
                    std::fs::create_dir_all(path.parent().unwrap()).expect("evidence parent");
                    std::fs::write(&path, "{}").expect("evidence file");
                }
            }),
        );
    }
    Value::Object(matrix)
}

fn current_contract() -> Value {
    let mut editors = Map::new();
    for editor in Editor::all() {
        editors.insert(editor.id().to_string(), editor_contract(editor));
    }
    json!({
        "schema": 1,
        "all_ids": Editor::all().iter().map(|e| e.id()).collect::<Vec<_>>(),
        "selectable_ids": Editor::selectable().iter().map(|e| e.id()).collect::<Vec<_>>(),
        "editors": editors,
        "detection": detection_matrix(),
    })
}

#[test]
fn host_probing_editors_are_detected_by_their_own_evidence() {
    let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
    for (editor, evidence) in [
        (Editor::Cursor, "bin:cursor"),
        (Editor::Cursor, ".cursor/"),
        (Editor::Cursor, "Applications/cursor.AppImage"),
        (Editor::Windsurf, "bin:windsurf"),
        (Editor::Windsurf, ".codeium/windsurf/"),
        (Editor::Windsurf, "Applications/Windsurf.AppImage"),
        (Editor::Windsurf, "Applications/windsurf.AppImage"),
    ] {
        let sandbox = Sandbox::new();
        let _env = IsolatedEnv::new(&sandbox.home, &sandbox.bin);
        if let Some(binary) = evidence.strip_prefix("bin:") {
            let path = sandbox.bin.join(binary);
            std::fs::write(&path, "#!/bin/sh\n").expect("stub");
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        } else if let Some(dir) = evidence.strip_suffix('/') {
            std::fs::create_dir_all(sandbox.home.join(dir)).expect("evidence dir");
        } else {
            let path = sandbox.home.join(evidence);
            std::fs::create_dir_all(path.parent().unwrap()).expect("evidence parent");
            std::fs::write(path, "").expect("evidence file");
        }
        assert!(
            super::super::editors::detect_installed_editors().contains(&editor),
            "{} not detected from {evidence}",
            editor.id()
        );
    }
}

#[test]
fn client_contract_matches_blessed_snapshot() {
    let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
    let actual = current_contract();
    let rendered = serde_json::to_string_pretty(&actual).expect("render contract") + "\n";

    if std::env::var_os(BLESS_ENV).is_some() {
        std::fs::create_dir_all(Path::new(CONTRACT_PATH).parent().unwrap()).expect("testdata");
        std::fs::write(CONTRACT_PATH, &rendered).expect("bless contract");
        return;
    }

    let expected = std::fs::read_to_string(CONTRACT_PATH)
        .unwrap_or_else(|_| panic!("missing {CONTRACT_PATH}; bless it with {BLESS_ENV}=1"));
    if rendered != expected {
        let expected: Value = serde_json::from_str(&expected).expect("parse contract");
        let mut diffs = Vec::new();
        diff_values("", &expected, &actual, &mut diffs);
        panic!(
            "client contract changed ({} difference(s)); if intentional, re-bless with {BLESS_ENV}=1:\n{}",
            diffs.len(),
            diffs.into_iter().take(40).collect::<Vec<_>>().join("\n")
        );
    }
}

fn diff_values(pointer: &str, expected: &Value, actual: &Value, out: &mut Vec<String>) {
    match (expected, actual) {
        (Value::Object(left), Value::Object(right)) => {
            let mut keys: Vec<&String> = left.keys().chain(right.keys()).collect();
            keys.sort();
            keys.dedup();
            for key in keys {
                let path = format!("{pointer}/{key}");
                match (left.get(key), right.get(key)) {
                    (Some(l), Some(r)) => diff_values(&path, l, r, out),
                    (Some(_), None) => out.push(format!("- {path}")),
                    (None, Some(_)) => out.push(format!("+ {path}")),
                    (None, None) => {}
                }
            }
        }
        (left, right) if left != right => out.push(format!("~ {pointer}: {left} -> {right}")),
        _ => {}
    }
}
