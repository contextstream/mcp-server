//! Antigravity's ContextStream plugin, `~/.gemini/config/plugins/contextstream/`.
//!
//! Antigravity (the desktop app and the `agy` CLI) loads global customizations
//! from `~/.gemini/config/` and bundles them as plugins (`plugins/<name>/`):
//! - `plugin.json`: the manifest. Without it the folder is not a plugin and
//!   none of the files below load.
//! - `rules/AGENTS.md`: the managed rules block (see `rules.rs`). Antigravity
//!   inlines always-on and global rules within one 20,000-token budget and cuts
//!   a rule file at 24 KB, so these rules are compact.
//! - `skills/contextstream/SKILL.md`: a skill Antigravity loads only when the
//!   agent asks for it. It lists the sections of the full reference in
//!   `references/reference.md`, because Antigravity reads only the first
//!   24 KB of a skill.
//! - `hooks.json`: the `contextstream` hook entry (see `hooks.rs` and
//!   `hook_handlers::antigravity`).
//!
//! The MCP server stays in the global `~/.gemini/config/mcp_config.json`: a
//! server in the plugin's own `mcp_config.json` would be a second copy.
//! Whether the plugin is on is the person's choice, recorded by Antigravity in
//! `~/.gemini/config/config.json` (`plugins.contextstream.enabled`); setup
//! never changes it.

use anyhow::Result;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

use super::safe_edit;

/// The plugin's folder name, which Antigravity also uses as its id.
pub(crate) const PLUGIN_NAME: &str = "contextstream";

/// Marks the skill's files as written by ContextStream, so setup replaces
/// them and uninstall removes them only when nobody else wrote them.
const SKILL_OWNER_MARKER: &str = "<!-- contextstream-managed: antigravity skill -->";

/// The skill's reference file, relative to `SKILL.md`.
const REFERENCE_LINK: &str = "references/reference.md";

/// `plugin.json` keys setup writes. Any other key is left as it is.
const MANIFEST_KEYS: [&str; 5] = ["name", "description", "author", "homepage", "repository"];

fn config_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".gemini").join("config"))
}

pub(crate) fn plugin_dir() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("plugins").join(PLUGIN_NAME))
}

pub(crate) fn manifest_path() -> Option<PathBuf> {
    plugin_dir().map(|dir| dir.join("plugin.json"))
}

pub(crate) fn skill_path() -> Option<PathBuf> {
    plugin_dir().map(|dir| dir.join("skills").join(PLUGIN_NAME).join("SKILL.md"))
}

fn reference_path() -> Option<PathBuf> {
    skill_path().map(|skill| skill.with_file_name("references").join("reference.md"))
}

pub(crate) fn hooks_path() -> Option<PathBuf> {
    plugin_dir().map(|dir| dir.join("hooks.json"))
}

/// Where releases before the plugin put the `contextstream` hook entry.
pub(crate) fn legacy_hooks_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("hooks.json"))
}

/// Antigravity's own settings, where it records plugins the person turned off.
fn antigravity_config_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join("config.json"))
}

fn manifest_value() -> Value {
    json!({
        "name": PLUGIN_NAME,
        "description": "ContextStream: persistent memory, decisions, lessons, plans and indexed code search for every session.",
        "author": { "name": "ContextStream", "url": "https://contextstream.io" },
        "homepage": "https://contextstream.io",
        "repository": "https://github.com/contextstream/mcp-server",
    })
}

/// Write `plugin.json`, or bring setup's keys in an existing one up to date.
/// Hooks and rules in the folder load only once it exists.
pub(crate) fn ensure_manifest() -> Result<()> {
    let path =
        manifest_path().ok_or_else(|| anyhow::anyhow!("Could not determine the home directory"))?;
    let loaded = safe_edit::read_for_edit(&path, safe_edit::JsonDialect::Strict)?;
    let mut root = match &loaded.value {
        Value::Object(root) => root.clone(),
        Value::Null => serde_json::Map::new(),
        _ => anyhow::bail!(
            "Refusing to modify {}: it is not a JSON object.",
            path.display()
        ),
    };
    if let Value::Object(ours) = manifest_value() {
        for (key, value) in ours {
            root.insert(key, value);
        }
    }
    safe_edit::commit_public(&path, &loaded, &Value::Object(root))?;
    Ok(())
}

/// The skill's two files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkillFiles {
    /// `SKILL.md`: front matter Antigravity lists the skill by, how to call
    /// ContextStream, and the reference's sections.
    pub skill: String,
    /// `references/reference.md`: the full rules from `rules.rs`.
    pub reference: String,
}

const CALL_MCP_TOOL_NOTE: &str = "In Antigravity, make each ContextStream call with `call_mcp_tool`: `ServerName` \"contextstream\", `ToolName` the tool, `Arguments` its parameters. For example, `search(mode=\"auto\", query=\"...\")` is `call_mcp_tool(ServerName=\"contextstream\", ToolName=\"search\", Arguments={\"mode\": \"auto\", \"query\": \"...\"})`.";

pub(crate) fn skill_files(reference: &str) -> SkillFiles {
    let reference = reference.trim();
    let sections: Vec<String> = reference
        .lines()
        .filter_map(|line| line.strip_prefix("## "))
        .map(|heading| format!("- {}", heading.trim()))
        .collect();
    SkillFiles {
        skill: format!(
            "---\nname: {PLUGIN_NAME}\ndescription: >-\n  Full ContextStream reference: every tool and action, the notices\n  context() returns, plans, tasks, lessons, decisions, docs, handoffs,\n  past sessions and code search. Use it when a ContextStream call or\n  notice needs more detail than the plugin rules give.\n---\n{SKILL_OWNER_MARKER}\n\n# ContextStream reference\n\n{CALL_MCP_TOOL_NOTE}\n\nThe complete reference is [{REFERENCE_LINK}]({REFERENCE_LINK}) in this skill's folder. Open it with `view_file` and read the section you need:\n{}\n",
            sections.join("\n")
        ),
        reference: format!("{SKILL_OWNER_MARKER}\n# ContextStream reference\n\n{CALL_MCP_TOOL_NOTE}\n\n{reference}\n"),
    }
}

fn skill_is_owned(content: &str) -> bool {
    content.contains(SKILL_OWNER_MARKER)
}

/// Write the skill's files, unless a file there is someone else's.
pub(crate) fn write_skill(files: &SkillFiles) -> Result<bool> {
    let home = || anyhow::anyhow!("Could not determine the home directory");
    let targets = [
        (skill_path().ok_or_else(home)?, &files.skill),
        (reference_path().ok_or_else(home)?, &files.reference),
    ];
    let mut existing = Vec::new();
    for (path, _) in &targets {
        let text = read_optional(path)?;
        if text.as_deref().is_some_and(|text| !skill_is_owned(text)) {
            tracing::warn!(
                "Leaving the contextstream skill as it is: ContextStream did not write {}.",
                path.display()
            );
            return Ok(false);
        }
        existing.push(text);
    }
    let mut written = false;
    for ((path, content), existing) in targets.iter().zip(existing) {
        written |=
            safe_edit::write_public_owned_file_if_unchanged(path, content, existing.as_deref())?;
    }
    Ok(written)
}

/// Whether the person turned the plugin off in Antigravity.
pub(crate) fn disabled_in_antigravity() -> bool {
    antigravity_config_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|content| serde_json::from_str::<Value>(&content).ok())
        .is_some_and(|config| plugin_disabled_in(&config))
}

fn plugin_disabled_in(config: &Value) -> bool {
    config
        .get("plugins")
        .and_then(|plugins| plugins.get(PLUGIN_NAME))
        .and_then(|entry| entry.get("enabled"))
        == Some(&Value::Bool(false))
}

/// What `doctor` reports about the plugin's own files (rules and hooks have
/// their own checks).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ManifestState {
    Missing,
    Unreadable(String),
    Present,
}

pub(crate) fn manifest_state() -> ManifestState {
    let Some(path) = manifest_path() else {
        return ManifestState::Missing;
    };
    match std::fs::read_to_string(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ManifestState::Missing,
        Err(error) => ManifestState::Unreadable(error.to_string()),
        Ok(content) => match serde_json::from_str::<Value>(&content) {
            Ok(Value::Object(_)) => ManifestState::Present,
            Ok(_) => ManifestState::Unreadable("not a JSON object".to_string()),
            Err(error) => ManifestState::Unreadable(error.to_string()),
        },
    }
}

pub(crate) fn skill_installed() -> bool {
    [skill_path(), reference_path()].into_iter().all(|path| {
        path.and_then(|path| std::fs::read_to_string(path).ok())
            .is_some_and(|content| skill_is_owned(&content))
    })
}

/// After uninstall took out the rules block and the hook entry: remove the
/// skill, the manifest and the folders when nothing else is left in the
/// plugin. Anything the person added keeps the folder (and the manifest that
/// makes it a plugin) in place.
pub(crate) fn remove_if_unused() -> Result<bool> {
    let Some(dir) = plugin_dir().filter(|dir| dir.is_dir()) else {
        return Ok(false);
    };
    let skill = skill_path().expect("plugin dir resolved");
    let reference = reference_path().expect("plugin dir resolved");
    let manifest = manifest_path().expect("plugin dir resolved");
    let reference_dir = reference
        .parent()
        .expect("reference has a folder")
        .to_path_buf();
    let skill_dir = skill.parent().expect("skill has a folder").to_path_buf();
    let skills_dir = skill_dir
        .parent()
        .expect("skill folder has a parent")
        .to_path_buf();
    let rules_dir = dir.join("rules");

    let ours = |path: &Path| path == skill || path == reference || path == manifest;
    let known_dirs = [&reference_dir, &skill_dir, &skills_dir, &rules_dir];
    if !only_known_entries(&dir, &ours, &known_dirs)? {
        return Ok(false);
    }

    let mut owned_files = Vec::new();
    for path in [&skill, &reference] {
        if let Some(text) = read_optional(path)? {
            if !skill_is_owned(&text) {
                return Ok(false);
            }
            owned_files.push((path, text));
        }
    }
    if let Some(text) = read_optional(&manifest)? {
        if !manifest_is_ours(&text) {
            return Ok(false);
        }
        owned_files.push((&manifest, text));
    }

    let mut removed = false;
    for (path, text) in owned_files {
        removed |= safe_edit::remove_owned_file_if_unchanged(path, &text)?;
    }
    if !safe_edit::is_dry_run() {
        for folder in [&reference_dir, &skill_dir, &skills_dir, &rules_dir, &dir] {
            remove_dir_if_empty(folder)?;
        }
    }
    Ok(removed)
}

/// True when every entry under `dir` is one of `ours` or a known folder that
/// itself holds only such entries. A file a dry run already plans to delete
/// (the rules file, the hooks file) counts as gone.
fn only_known_entries(
    dir: &Path,
    ours: &dyn Fn(&Path) -> bool,
    known_dirs: &[&PathBuf],
) -> Result<bool> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            if !known_dirs.iter().any(|known| known.as_path() == path)
                || !only_known_entries(&path, ours, known_dirs)?
            {
                return Ok(false);
            }
        } else if !ours(&path) && !safe_edit::is_planned_deletion(&path) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// A manifest with only the keys setup writes, set to setup's values.
fn manifest_is_ours(content: &str) -> bool {
    let Ok(Value::Object(root)) = serde_json::from_str::<Value>(content) else {
        return false;
    };
    let ours = manifest_value();
    root.iter()
        .all(|(key, value)| MANIFEST_KEYS.contains(&key.as_str()) && ours.get(key) == Some(value))
}

fn read_optional(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn remove_dir_if_empty(dir: &Path) -> Result<()> {
    match std::fs::remove_dir(dir) {
        Ok(()) => Ok(()),
        Err(error)
            if error.kind() == std::io::ErrorKind::NotFound
                || error.kind() == std::io::ErrorKind::DirectoryNotEmpty =>
        {
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

/// `plugin.json` `version` in the public repository. Setup's own manifest has
/// none (it would go stale in a person's folder); change this when the
/// published files change in a way people should see.
const PUBLISHED_VERSION: &str = "1.0.0";
const PUBLISHED_LOGO: &str = "assets/logo.png";
const API_KEY_VARIABLE: &str = "CONTEXTSTREAM_API_KEY";
const API_KEYS_URL: &str = "https://contextstream.io/account/api-keys";

fn published_manifest() -> Value {
    let mut manifest = manifest_value();
    if let Value::Object(map) = &mut manifest {
        for (key, value) in [
            (
                "$schema",
                json!("https://antigravity.google/schemas/v1/plugin.json"),
            ),
            ("displayName", json!("ContextStream")),
            ("version", json!(PUBLISHED_VERSION)),
            ("logo", json!(PUBLISHED_LOGO)),
            ("license", json!("MIT")),
            (
                "keywords",
                json!([
                    "memory",
                    "context",
                    "code-search",
                    "mcp",
                    "decisions",
                    "lessons"
                ]),
            ),
            (
                "suggestedPrompts",
                json!([
                    "Ground this session with ContextStream",
                    "Search this codebase with ContextStream before grepping",
                    "Save this decision to ContextStream memory"
                ]),
            ),
        ] {
            map.insert(key.to_string(), value);
        }
    }
    manifest
}

/// The hosted server, signed in with the person's API key. The plugin stays
/// off until they enter it, and the key lives in Antigravity, not in the file.
fn published_mcp_config() -> Value {
    json!({
        "mcpServers": {
            "contextstream": {
                "serverUrl": "https://mcp.contextstream.io/mcp",
                "headers": { "Authorization": "Bearer ${CONTEXTSTREAM_API_KEY}" },
                "variables": [{
                    "name": API_KEY_VARIABLE,
                    "title": "ContextStream API key",
                    "description": "Your ContextStream API key. Create one in your account under API keys.",
                    "link": API_KEYS_URL
                }]
            }
        }
    })
}

/// Whether the plugin in the person's folder came from the public repository:
/// its `plugin.json` names a version, which setup's own never does.
pub(crate) fn installed_from_repository() -> bool {
    manifest_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .is_some_and(|manifest| manifest.get("version").is_some())
}

const PUBLISHED_README: &str = r#"# ContextStream for Antigravity

Persistent memory for your Antigravity sessions: decisions, lessons, plans and
indexed code search from [ContextStream](https://contextstream.io), through the
hosted MCP server at `https://mcp.contextstream.io/mcp`.

## Install

```sh
agy plugin install https://github.com/contextstream/antigravity-plugin
```

The plugin stays off until you give it your ContextStream API key. Open
`/plugin` in Antigravity, turn **contextstream** on and enter the key. Create
one at <https://contextstream.io/account/api-keys>.

## What it holds

| File | What it does |
| --- | --- |
| `rules/AGENTS.md` | Compact always-on rules: ground each request with `context`, search ContextStream before `grep_search` and `find_by_name`, and save decisions and lessons |
| `skills/contextstream/` | The full reference, loaded only when the agent asks for it |
| `mcp_config.json` | The hosted ContextStream server; your API key is asked for when you turn the plugin on |
| `hooks.json` | Reminders to call `init` and `context`, and to search ContextStream first |

## The hooks need `contextstream-mcp`

The hooks run the local `contextstream-mcp` binary. Everything else works
without it: with no binary on your `PATH`, or one that is not signed in, the
hooks add nothing (they answer `{}`, an empty hook result, and print no
error). To add it, run `npx -y @contextstream/mcp-server setup`, which also
signs the binary in. The hooks use a POSIX shell; on Windows run `setup`, which
writes hooks for Windows.

If you already ran `contextstream-mcp setup` for Antigravity, you have this
plugin and don't need to install it again.

## Privacy

What ContextStream collects, and how to turn each part off, is in
[docs/data-handling.md](https://github.com/contextstream/mcp-server/blob/main/docs/data-handling.md).
Hooks and indexing send nothing until `contextstream-mcp` is set up and signed in.

## Where this comes from

This repository is generated from
[contextstream/mcp-server](https://github.com/contextstream/mcp-server) by the
same code `setup` uses to write the plugin on your machine, so the two do not
drift. Please don't edit files here; open issues and pull requests on
`mcp-server`.
"#;

/// The text files of the public plugin repository, as `(path, content)`.
/// `setup` writes the same rules, skill and hooks into a person's folder;
/// this adds the manifest's listing fields, the hosted server entry and a
/// README. The logo (`assets/logo.png`) is copied from `assets/`.
pub(crate) fn published_files() -> Vec<(&'static str, String)> {
    let pretty = |value: &Value| {
        let mut text = serde_json::to_string_pretty(value).expect("plugin JSON serializes");
        text.push('\n');
        text
    };
    let skill = super::rules::antigravity_skill_files();
    vec![
        ("plugin.json", pretty(&published_manifest())),
        ("README.md", PUBLISHED_README.to_string()),
        (
            "rules/AGENTS.md",
            super::rules::antigravity_plugin_rules_file(),
        ),
        ("skills/contextstream/SKILL.md", skill.skill),
        (
            "skills/contextstream/references/reference.md",
            skill.reference,
        ),
        (
            "hooks.json",
            pretty(&super::hooks::antigravity_published_hooks()),
        ),
        ("mcp_config.json", pretty(&published_mcp_config())),
    ]
}

/// `export-antigravity-plugin`: print the published files as JSON, or write them
/// under `out`.
pub(crate) fn export(out: Option<&Path>, as_json: bool) -> Result<()> {
    let files = published_files();
    if as_json {
        let map: std::collections::BTreeMap<_, _> = files.into_iter().collect();
        println!("{}", serde_json::to_string_pretty(&map)?);
        return Ok(());
    }
    let out = out.ok_or_else(|| anyhow::anyhow!("Give --out DIR, or --json to print them."))?;
    for (relative, content) in &files {
        let path = out.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, content)?;
    }
    println!("Wrote {} files to {}", files.len(), out.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct HomeGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        previous: Option<std::ffi::OsString>,
        _temp: tempfile::TempDir,
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(home) => std::env::set_var("HOME", home),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    fn temp_home() -> (HomeGuard, PathBuf) {
        let lock = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().to_path_buf();
        let previous = std::env::var_os("HOME");
        std::env::set_var("HOME", &home);
        (
            HomeGuard {
                _lock: lock,
                previous,
                _temp: temp,
            },
            home,
        )
    }

    #[test]
    fn manifest_is_written_and_keeps_keys_the_person_added() {
        let (_guard, home) = temp_home();
        ensure_manifest().expect("write manifest");
        let path = home.join(".gemini/config/plugins/contextstream/plugin.json");
        let written: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written["name"], "contextstream");
        assert!(manifest_is_ours(&std::fs::read_to_string(&path).unwrap()));

        let mut edited = written.clone();
        edited["logo"] = json!("assets/mine.svg");
        edited["name"] = json!("renamed");
        std::fs::write(&path, serde_json::to_string_pretty(&edited).unwrap()).unwrap();
        ensure_manifest().expect("refresh manifest");
        let refreshed: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(refreshed["name"], "contextstream");
        assert_eq!(refreshed["logo"], "assets/mine.svg");
        assert!(!manifest_is_ours(&std::fs::read_to_string(&path).unwrap()));
        assert_eq!(manifest_state(), ManifestState::Present);
    }

    #[test]
    fn skill_is_replaced_only_when_contextstream_wrote_it() {
        let (_guard, home) = temp_home();
        let skill_dir = home.join(".gemini/config/plugins/contextstream/skills/contextstream");
        let reference = skill_dir.join("references/reference.md");
        assert!(write_skill(&skill_files("## First\nbody")).unwrap());
        assert!(skill_installed());
        assert!(write_skill(&skill_files("## Second\nbody")).unwrap());
        assert!(std::fs::read_to_string(&reference)
            .unwrap()
            .contains("## Second"));
        assert!(std::fs::read_to_string(skill_dir.join("SKILL.md"))
            .unwrap()
            .contains("- Second"));
        assert!(
            !write_skill(&skill_files("## Second\nbody")).unwrap(),
            "no-op rewrite"
        );

        std::fs::write(&reference, "my notes\n").unwrap();
        assert!(!write_skill(&skill_files("## Third\nbody")).unwrap());
        assert_eq!(std::fs::read_to_string(&reference).unwrap(), "my notes\n");
        assert!(!std::fs::read_to_string(skill_dir.join("SKILL.md"))
            .unwrap()
            .contains("Third"));
        assert!(!skill_installed());
    }

    #[test]
    fn skill_lists_the_reference_sections_under_its_front_matter() {
        let files =
            skill_files("# Rules\n## Plans and Tasks\ntext\n### Detail\n## Past Sessions\n");
        assert!(files
            .skill
            .starts_with("---\nname: contextstream\ndescription: >-\n"));
        let front_matter_end = files.skill[4..].find("\n---\n").expect("closing fence") + 4;
        let body = &files.skill[front_matter_end..];
        assert!(body.contains(SKILL_OWNER_MARKER));
        assert!(body.contains("[references/reference.md](references/reference.md)"));
        assert!(
            body.ends_with("- Plans and Tasks\n- Past Sessions\n"),
            "{body}"
        );
        assert!(files.reference.starts_with(SKILL_OWNER_MARKER));
        assert!(files.reference.contains("### Detail"));
    }

    #[test]
    fn disabled_plugin_is_read_from_antigravity_config() {
        assert!(plugin_disabled_in(
            &json!({"plugins": {"contextstream": {"enabled": false}}})
        ));
        assert!(!plugin_disabled_in(
            &json!({"plugins": {"contextstream": {"enabled": true}}})
        ));
        assert!(!plugin_disabled_in(
            &json!({"plugins": {"other": {"enabled": false}}})
        ));
        assert!(!plugin_disabled_in(&json!({})));
    }

    #[test]
    fn unused_plugin_is_removed_but_one_with_the_persons_files_stays() {
        let (_guard, home) = temp_home();
        let dir = home.join(".gemini/config/plugins/contextstream");
        ensure_manifest().unwrap();
        write_skill(&skill_files("## Reference")).unwrap();
        std::fs::create_dir_all(dir.join("rules")).unwrap();

        // Something of the person's own keeps everything.
        std::fs::write(dir.join("rules").join("mine.md"), "my rule\n").unwrap();
        assert!(!remove_if_unused().unwrap());
        assert!(dir.join("plugin.json").exists());
        assert!(dir.join("skills/contextstream/SKILL.md").exists());

        std::fs::remove_file(dir.join("rules").join("mine.md")).unwrap();
        assert!(remove_if_unused().unwrap());
        assert!(!dir.exists());
        assert!(!remove_if_unused().unwrap(), "second run is a no-op");
    }

    #[test]
    fn dry_run_lists_the_whole_plugin_once_its_rules_are_planned_away() {
        let (_guard, home) = temp_home();
        let dir = home.join(".gemini/config/plugins/contextstream");
        ensure_manifest().unwrap();
        write_skill(&skill_files("## Reference")).unwrap();
        let rules = dir.join("rules").join("AGENTS.md");
        std::fs::create_dir_all(rules.parent().unwrap()).unwrap();
        std::fs::write(&rules, "managed\n").unwrap();

        safe_edit::set_dry_run(true);
        let _ = safe_edit::take_planned_changes();
        // Uninstall's earlier step plans the rules file away.
        safe_edit::remove_owned_file_if_unchanged(&rules, "managed\n").unwrap();
        let planned_removal = remove_if_unused();
        let planned: Vec<PathBuf> = safe_edit::take_planned_changes()
            .into_iter()
            .map(|change| change.path)
            .collect();
        safe_edit::set_dry_run(false);

        assert!(planned_removal.unwrap());
        for file in [
            "plugin.json",
            "skills/contextstream/SKILL.md",
            "skills/contextstream/references/reference.md",
        ] {
            assert!(planned.contains(&dir.join(file)), "{file} not planned");
            assert!(dir.join(file).exists(), "dry run deleted {file}");
        }
    }

    #[test]
    fn plugin_with_an_edited_manifest_is_kept() {
        let (_guard, home) = temp_home();
        let dir = home.join(".gemini/config/plugins/contextstream");
        ensure_manifest().unwrap();
        let path = dir.join("plugin.json");
        let mut manifest: Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        manifest["logo"] = json!("assets/mine.svg");
        std::fs::write(&path, serde_json::to_string(&manifest).unwrap()).unwrap();
        assert!(!remove_if_unused().unwrap());
        assert!(path.exists());
    }

    #[test]
    fn the_published_manifest_keeps_to_antigravitys_schema() {
        // `additionalProperties: false` in antigravity.google/schemas/v1/plugin.json.
        const ALLOWED: [&str; 19] = [
            "$schema",
            "name",
            "displayName",
            "display_name",
            "description",
            "version",
            "disabled",
            "logo",
            "suggestedPrompts",
            "suggested_prompts",
            "skills",
            "author",
            "homepage",
            "repository",
            "license",
            "keywords",
            "extensions",
            "source",
            "sourcePath",
        ];
        let manifest = published_manifest();
        for key in manifest.as_object().unwrap().keys() {
            assert!(
                ALLOWED.contains(&key.as_str()),
                "{key} is not in the schema"
            );
        }
        assert_eq!(manifest["name"], PLUGIN_NAME);
        assert!(manifest["suggestedPrompts"].as_array().unwrap().len() <= 3);
        // Everything setup writes to its own manifest is in the published one.
        for (key, value) in manifest_value().as_object().unwrap() {
            assert_eq!(manifest.get(key), Some(value), "{key}");
        }
        assert!(manifest_is_ours(
            &serde_json::to_string(&manifest_value()).unwrap()
        ));
    }

    #[test]
    fn the_published_files_carry_the_same_rules_skill_and_hooks_setup_writes() {
        let files: std::collections::BTreeMap<_, _> = published_files().into_iter().collect();
        assert_eq!(
            files.keys().copied().collect::<Vec<_>>(),
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
        for text in files.values() {
            assert!(text.ends_with('\n'));
        }
    }

    #[test]
    fn the_published_rules_and_skill_are_what_setup_writes() {
        let (_guard, _home) = temp_home();
        super::super::rules::write_editor_rules(&super::super::editors::Editor::Antigravity)
            .expect("write the plugin's rules");
        let files: std::collections::BTreeMap<_, _> = published_files().into_iter().collect();
        let read = |path: Option<PathBuf>| std::fs::read_to_string(path.unwrap()).unwrap();
        let rules = plugin_dir().unwrap().join("rules").join("AGENTS.md");
        assert_eq!(files["rules/AGENTS.md"], read(Some(rules)));
        assert_eq!(files["skills/contextstream/SKILL.md"], read(skill_path()));
        assert_eq!(
            files["skills/contextstream/references/reference.md"],
            read(reference_path())
        );
    }

    #[test]
    fn setup_recognises_the_published_hooks_as_its_own() {
        let hooks = super::super::hooks::antigravity_published_hooks();
        assert_eq!(
            super::super::hooks::validate_published_antigravity_hooks(&hooks),
            Ok(3)
        );
    }

    #[test]
    fn the_published_hooks_name_the_binary_and_degrade_quietly() {
        let hooks = super::super::hooks::antigravity_published_hooks();
        let text = hooks.to_string();
        // The same events and handlers setup installs, by name from the PATH.
        for hook in [
            "agy-session-start",
            "agy-pre-invocation",
            "agy-post-tool-use",
        ] {
            assert!(
                text.contains(&format!("contextstream-mcp hook {hook} ")),
                "{hook}: {text}"
            );
        }
        let commands = text.matches("2>/dev/null || echo '{}'").count();
        assert_eq!(commands, 3, "{text}");
        assert!(hooks["contextstream"]["PostToolUse"][0]["matcher"] == "*");
    }

    #[test]
    fn the_published_server_entry_holds_no_key() {
        let config = published_mcp_config();
        let server = &config["mcpServers"]["contextstream"];
        assert_eq!(server["serverUrl"], "https://mcp.contextstream.io/mcp");
        assert_eq!(
            server["headers"]["Authorization"],
            format!("Bearer ${{{API_KEY_VARIABLE}}}")
        );
        assert_eq!(server["variables"][0]["name"], API_KEY_VARIABLE);
        assert!(!config.to_string().contains("cs_"));
    }
}
