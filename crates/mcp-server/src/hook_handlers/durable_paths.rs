//! Classify file writes that would keep durable knowledge outside ContextStream.
//!
//! Three kinds of write matter to the hooks:
//! - Claude Code auto memory (`<claude config>/projects/<project>/memory/**`, or
//!   the directory named by `autoMemoryDirectory`). Claude Code's own system
//!   prompt routes "remember X" there, so PreToolUse redirects it.
//! - The plan file Claude Code's plan mode must write (`<claude config>/plans/*.md`).
//!   Plan mode requires it, so it is allowed; PostToolUse(ExitPlanMode) saves the
//!   approved plan to ContextStream instead.
//! - Repository runbooks, ADRs, RFCs, postmortems and notes. The repository keeps
//!   its file and PostToolUse mirrors it to a ContextStream doc.
//!
//! Everything here is pure path and text inspection so PreToolUse stays fast.

use serde_json::Value;
use std::path::{Path, PathBuf};

/// A write that stores durable knowledge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DurableWrite {
    /// Claude Code auto memory. `memory_type` is the frontmatter `type` of the
    /// content being written, when present; `is_index` marks `MEMORY.md`.
    ClaudeAutoMemory {
        memory_type: Option<String>,
        is_index: bool,
    },
    /// The plan file written by Claude Code plan mode.
    ClaudeHarnessPlan,
    /// Repository markdown that is mirrored to a ContextStream doc.
    RepoDoc { doc_type: &'static str },
}

/// Claude Code's configuration directory: `CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub fn claude_config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|value| !value.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    dirs::home_dir().map(|home| home.join(".claude"))
}

/// `autoMemoryDirectory` from the user's Claude settings, with `~` expanded.
fn configured_auto_memory_dir(config_dir: &Path) -> Option<PathBuf> {
    let raw = std::fs::read_to_string(config_dir.join("settings.json")).ok()?;
    let settings: Value = serde_json::from_str(&raw).ok()?;
    let dir = settings
        .get("autoMemoryDirectory")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|dir| !dir.is_empty())?;
    Some(expand_home(dir))
}

fn expand_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

/// Normalize to forward slashes so one matcher covers every platform.
fn slashed(path: &str) -> String {
    path.replace('\\', "/")
}

fn path_is_under(path: &str, dir: &Path) -> bool {
    let dir = slashed(&dir.to_string_lossy());
    let dir = dir.trim_end_matches('/');
    !dir.is_empty() && path.starts_with(dir) && path[dir.len()..].starts_with('/')
}

/// Whether `path` is inside Claude Code's auto memory, given the Claude config
/// directory and an optional `autoMemoryDirectory` override.
pub fn is_auto_memory_path_in(path: &str, config_dir: &Path, custom_dir: Option<&Path>) -> bool {
    let path = slashed(path);
    if let Some(custom) = custom_dir {
        if path_is_under(&path, custom) {
            return true;
        }
    }
    let projects = config_dir.join("projects");
    if !path_is_under(&path, &projects) {
        return false;
    }
    let projects = slashed(&projects.to_string_lossy());
    let rest = &path[projects.trim_end_matches('/').len() + 1..];
    // <project>/memory/<file...>
    let mut segments = rest.split('/');
    let project = segments.next().unwrap_or("");
    let memory = segments.next().unwrap_or("");
    !project.is_empty() && memory == "memory" && segments.next().is_some()
}

/// Whether `path` is a plan file written by Claude Code plan mode.
pub fn is_harness_plan_path_in(path: &str, config_dir: &Path) -> bool {
    let path = slashed(path);
    let plans = config_dir.join("plans");
    if !path_is_under(&path, &plans) {
        return false;
    }
    let plans = slashed(&plans.to_string_lossy());
    let rest = &path[plans.trim_end_matches('/').len() + 1..];
    !rest.contains('/') && rest.to_ascii_lowercase().ends_with(".md")
}

/// Whether Claude settings run ContextStream's `post-tool-use` hook for `tool`.
pub fn post_tool_use_hook_matches(settings: &Value, tool: &str) -> bool {
    let Some(entries) = settings
        .get("hooks")
        .and_then(|hooks| hooks.get("PostToolUse"))
        .and_then(Value::as_array)
    else {
        return false;
    };
    entries.iter().any(|entry| {
        let matcher = entry.get("matcher").and_then(Value::as_str).unwrap_or("");
        let matches_tool = matcher.is_empty()
            || matcher == "*"
            || matcher.split('|').any(|name| name.trim() == tool);
        let runs_post_tool_use =
            entry
                .get("hooks")
                .and_then(Value::as_array)
                .is_some_and(|hooks| {
                    hooks.iter().any(|hook| {
                        hook.get("command")
                            .and_then(Value::as_str)
                            .is_some_and(|command| {
                                command.contains("contextstream")
                                    && command.contains("post-tool-use")
                            })
                    })
                });
        matches_tool && runs_post_tool_use
    })
}

/// Whether this machine's Claude Code saves approved plans through
/// PostToolUse(ExitPlanMode). Older hook installs do not observe it.
pub fn plan_auto_capture_installed() -> bool {
    let Some(config_dir) = claude_config_dir() else {
        return false;
    };
    std::fs::read_to_string(config_dir.join("settings.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .is_some_and(|settings| post_tool_use_hook_matches(&settings, "ExitPlanMode"))
}

/// Frontmatter `type` of an auto memory file (`type:` or `metadata.type:`).
pub fn frontmatter_type(content: &str) -> Option<String> {
    let body = content.strip_prefix("---")?;
    let end = body.find("\n---")?;
    body[..end].lines().find_map(|line| {
        let value = line.trim().strip_prefix("type:")?;
        let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
        (!value.is_empty()).then(|| value.to_ascii_lowercase())
    })
}

const NON_SOURCE_SEGMENTS: &[&str] = &[
    "node_modules",
    ".git",
    "target",
    "vendor",
    "scratchpad",
    "dist",
    "build",
];

/// ContextStream doc type for repository markdown that should be mirrored.
pub fn repo_doc_type(path: &str) -> Option<&'static str> {
    let lower = slashed(path).to_ascii_lowercase();
    if !(lower.ends_with(".md") || lower.ends_with(".mdx")) {
        return None;
    }
    let segments: Vec<&str> = lower.split('/').collect();
    if segments
        .iter()
        .any(|segment| NON_SOURCE_SEGMENTS.contains(segment))
    {
        return None;
    }
    let dirs = &segments[..segments.len().saturating_sub(1)];
    let file_name = segments.last().copied().unwrap_or("");
    let has_dir = |names: &[&str]| dirs.iter().any(|dir| names.contains(dir));

    if has_dir(&["runbooks", "runbook"]) || file_name.contains("runbook") {
        Some("runbook")
    } else if has_dir(&["adr", "adrs", "decisions"]) {
        Some("adr")
    } else if has_dir(&["rfc", "rfcs"]) {
        Some("rfc")
    } else if has_dir(&["postmortems", "postmortem", "incidents"]) {
        Some("postmortem")
    } else if has_dir(&["notes"]) {
        Some("general")
    } else {
        None
    }
}

/// Classify a write with explicit Claude directories (pure; used by tests).
pub fn classify_in(
    path: &str,
    content: Option<&str>,
    config_dir: Option<&Path>,
    custom_memory_dir: Option<&Path>,
) -> Option<DurableWrite> {
    if path.trim().is_empty() {
        return None;
    }
    if let Some(config_dir) = config_dir {
        if is_auto_memory_path_in(path, config_dir, custom_memory_dir) {
            let is_index = Path::new(path)
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case("MEMORY.md"));
            return Some(DurableWrite::ClaudeAutoMemory {
                memory_type: content.and_then(frontmatter_type),
                is_index,
            });
        }
        if is_harness_plan_path_in(path, config_dir) {
            return Some(DurableWrite::ClaudeHarnessPlan);
        }
        if path_is_under(&slashed(path), config_dir) {
            // Other Claude configuration is not repository knowledge.
            return None;
        }
    }
    repo_doc_type(path).map(|doc_type| DurableWrite::RepoDoc { doc_type })
}

/// Classify a write using this machine's Claude configuration.
pub fn classify(path: &str, content: Option<&str>) -> Option<DurableWrite> {
    let config_dir = claude_config_dir();
    let custom = config_dir.as_deref().and_then(configured_auto_memory_dir);
    classify_in(path, content, config_dir.as_deref(), custom.as_deref())
}

/// Files added or updated by a Codex `apply_patch` body.
pub fn patch_file_paths(patch: &str) -> Vec<String> {
    patch
        .lines()
        .filter_map(|line| {
            let line = line.trim_end();
            line.strip_prefix("*** Add File: ")
                .or_else(|| line.strip_prefix("*** Update File: "))
                .or_else(|| line.strip_prefix("*** Move to: "))
                .map(str::trim)
                .filter(|path| !path.is_empty())
                .map(str::to_string)
        })
        .collect()
}

/// Content added for `path` by an `apply_patch` body (`+` lines of an Add File).
pub fn patch_added_content(patch: &str, path: &str) -> Option<String> {
    let header = format!("*** Add File: {path}");
    let mut lines = patch.lines().skip_while(|line| line.trim_end() != header);
    lines.next()?;
    let body: Vec<&str> = lines
        .take_while(|line| !line.starts_with("*** "))
        .map(|line| line.strip_prefix('+').unwrap_or(line))
        .collect();
    Some(body.join("\n"))
}

/// Whether the tool writes files: Claude `Write`/`Edit`/`MultiEdit`/
/// `NotebookEdit`, Codex `apply_patch`, and editor-specific aliases.
pub fn is_write_tool(tool_lower: &str) -> bool {
    matches!(
        tool_lower,
        "write_to_file"
            | "create_file"
            | "write"
            | "edit"
            | "multiedit"
            | "notebookedit"
            | "apply_patch"
    )
}

/// The patch body of an `apply_patch` call, whichever field carries it.
pub fn patch_body(tool_input: &Value) -> Option<&str> {
    if let Some(text) = tool_input.as_str() {
        return Some(text);
    }
    ["command", "input", "patch"]
        .iter()
        .find_map(|key| tool_input.get(*key).and_then(Value::as_str))
        .or_else(|| {
            // Codex sends `command: ["apply_patch", "<patch>"]` from the shell form.
            tool_input
                .get("command")
                .and_then(Value::as_array)
                .and_then(|parts| parts.iter().rev().find_map(Value::as_str))
        })
        .filter(|text| text.contains("*** Begin Patch") || text.contains("*** Add File: "))
}

/// Every file path a write tool call targets.
pub fn write_target_paths(tool_lower: &str, tool_input: &Value) -> Vec<String> {
    if tool_lower == "apply_patch" {
        return patch_body(tool_input)
            .map(patch_file_paths)
            .unwrap_or_default();
    }
    [
        "file_path",
        "path",
        "target_file",
        "TargetFile",
        "notebook_path",
    ]
    .iter()
    .find_map(|key| tool_input.get(*key).and_then(Value::as_str))
    .map(str::trim)
    .filter(|path| !path.is_empty())
    .map(|path| vec![path.to_string()])
    .unwrap_or_default()
}

/// The text a `Write`-style call stores, when the input carries it whole.
pub fn written_content(tool_input: &Value) -> Option<&str> {
    ["content", "file_text", "CodeContent"]
        .iter()
        .find_map(|key| tool_input.get(*key).and_then(Value::as_str))
}

/// The server segment of an MCP tool name (`mcp__<server>__<tool>`).
fn mcp_server_segment(tool_name: &str) -> Option<&str> {
    let rest = tool_name.strip_prefix("mcp__")?;
    let (server, tool) = rest.rsplit_once("__")?;
    (!server.is_empty() && !tool.is_empty()).then_some(server)
}

/// Whether an MCP tool name belongs to a ContextStream server under any
/// registration name (`contextstream`, `claude_ai_ContextStream`, ...).
pub fn is_contextstream_mcp_tool_name(tool_name: &str) -> bool {
    mcp_server_segment(tool_name)
        .is_some_and(|server| server.to_ascii_lowercase().contains("contextstream"))
}

/// The bare ContextStream tool name (`memory`, `session`, ...), lowercased.
pub fn normalize_contextstream_tool_name(tool_name: &str) -> String {
    if is_contextstream_mcp_tool_name(tool_name) {
        if let Some((_, tool)) = tool_name.rsplit_once("__") {
            return tool.to_ascii_lowercase();
        }
    }
    tool_name.to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn claude() -> PathBuf {
        PathBuf::from("/Users/dev/.claude")
    }

    #[test]
    fn auto_memory_files_are_classified_with_their_type() {
        let content = "---\nname: commit-directly-to-main\ndescription: x\nmetadata:\n  type: feedback\n---\nbody";
        assert_eq!(
            classify_in(
                "/Users/dev/.claude/projects/-Users-dev-repo/memory/commit-directly-to-main.md",
                Some(content),
                Some(&claude()),
                None,
            ),
            Some(DurableWrite::ClaudeAutoMemory {
                memory_type: Some("feedback".into()),
                is_index: false
            })
        );
        assert_eq!(
            classify_in(
                "/Users/dev/.claude/projects/-Users-dev-repo/memory/MEMORY.md",
                None,
                Some(&claude()),
                None,
            ),
            Some(DurableWrite::ClaudeAutoMemory {
                memory_type: None,
                is_index: true
            })
        );
    }

    #[test]
    fn custom_auto_memory_directory_is_honored() {
        let custom = PathBuf::from("/data/claude-memory");
        assert!(matches!(
            classify_in(
                "/data/claude-memory/notes.md",
                None,
                Some(&claude()),
                Some(&custom)
            ),
            Some(DurableWrite::ClaudeAutoMemory { .. })
        ));
    }

    #[test]
    fn session_transcripts_and_other_claude_files_are_not_memory() {
        for path in [
            "/Users/dev/.claude/projects/-Users-dev-repo/abc.jsonl",
            "/Users/dev/.claude/projects/-Users-dev-repo/memory",
            "/Users/dev/.claude/settings.json",
            "/Users/dev/.claude/projects-old/x/memory/a.md",
            "/Users/dev/.claudex/projects/x/memory/a.md",
        ] {
            assert_eq!(
                classify_in(path, None, Some(&claude()), None),
                None,
                "{path}"
            );
        }
    }

    #[test]
    fn plan_mode_file_is_the_harness_plan() {
        assert_eq!(
            classify_in(
                "/Users/dev/.claude/plans/need-to-look-for-sequential-zebra.md",
                None,
                Some(&claude()),
                None
            ),
            Some(DurableWrite::ClaudeHarnessPlan)
        );
        assert_eq!(
            classify_in(
                "/Users/dev/.claude/plans/nested/plan.md",
                None,
                Some(&claude()),
                None
            ),
            None
        );
    }

    #[test]
    fn repository_docs_get_a_doc_type() {
        let cases = [
            ("/repo/docs/runbooks/ops-vm.md", Some("runbook")),
            ("/repo/ops/deploy-runbook.md", Some("runbook")),
            ("/repo/docs/adr/0007-queues.md", Some("adr")),
            ("/repo/docs/decisions/use-postgres.md", Some("adr")),
            ("/repo/docs/rfcs/12-sync.mdx", Some("rfc")),
            ("/repo/docs/postmortems/2026-10-01.md", Some("postmortem")),
            ("/repo/notes/today.md", Some("general")),
            ("/repo/README.md", None),
            ("/repo/docs/architecture.md", None),
            ("/repo/docs/runbooks/script.sh", None),
            ("/repo/node_modules/pkg/runbooks/x.md", None),
            (
                "/private/tmp/claude-501/x/scratchpad/wt/docs/runbooks/a.md",
                None,
            ),
        ];
        for (path, expected) in cases {
            assert_eq!(repo_doc_type(path), expected, "{path}");
        }
        assert_eq!(
            classify_in("C:\\repo\\docs\\runbooks\\ops.md", None, None, None),
            Some(DurableWrite::RepoDoc {
                doc_type: "runbook"
            })
        );
    }

    #[test]
    fn apply_patch_paths_and_added_content_are_parsed() {
        let patch = "*** Begin Patch\n*** Add File: HANDOFF.md\n+# Handoff\n+next steps\n*** Update File: docs/runbooks/ops.md\n@@\n-old\n+new\n*** End Patch\n";
        assert_eq!(
            patch_file_paths(patch),
            vec!["HANDOFF.md".to_string(), "docs/runbooks/ops.md".to_string()]
        );
        assert_eq!(
            patch_added_content(patch, "HANDOFF.md").as_deref(),
            Some("# Handoff\nnext steps")
        );
        assert_eq!(
            write_target_paths("apply_patch", &json!({ "command": patch })),
            vec!["HANDOFF.md".to_string(), "docs/runbooks/ops.md".to_string()]
        );
        assert_eq!(
            write_target_paths("apply_patch", &json!({ "command": ["apply_patch", patch] })),
            vec!["HANDOFF.md".to_string(), "docs/runbooks/ops.md".to_string()]
        );
        assert!(write_target_paths("apply_patch", &json!({ "command": "ls" })).is_empty());
        assert_eq!(
            write_target_paths("write", &json!({ "file_path": "/repo/a.md" })),
            vec!["/repo/a.md".to_string()]
        );
    }

    #[test]
    fn plan_auto_capture_requires_a_contextstream_post_tool_use_matcher() {
        let installed = json!({"hooks": {"PostToolUse": [{
            "matcher": "Edit|Write|ExitPlanMode|mcp__contextstream__init",
            "hooks": [{"type": "command", "command": "\"/u/.contextstream/bin/contextstream-mcp\" hook post-tool-use --contextstream-managed-hook=v1"}]
        }]}});
        assert!(post_tool_use_hook_matches(&installed, "ExitPlanMode"));
        assert!(!post_tool_use_hook_matches(&installed, "ExitPlan"));

        let old_matcher = json!({"hooks": {"PostToolUse": [{
            "matcher": "Edit|Write|NotebookEdit",
            "hooks": [{"type": "command", "command": "contextstream-mcp hook post-tool-use"}]
        }]}});
        assert!(!post_tool_use_hook_matches(&old_matcher, "ExitPlanMode"));

        let foreign = json!({"hooks": {"PostToolUse": [{
            "matcher": "*",
            "hooks": [{"type": "command", "command": "my-linter"}]
        }]}});
        assert!(!post_tool_use_hook_matches(&foreign, "ExitPlanMode"));
        assert!(!post_tool_use_hook_matches(&json!({}), "ExitPlanMode"));
    }

    #[test]
    fn contextstream_tools_are_recognized_under_any_server_name() {
        for name in [
            "mcp__contextstream__memory",
            "mcp__claude_ai_ContextStream__memory",
            "mcp__ContextStream__memory",
            "mcp__plugin_contextstream_contextstream__memory",
        ] {
            assert!(is_contextstream_mcp_tool_name(name), "{name}");
            assert_eq!(normalize_contextstream_tool_name(name), "memory");
        }
        for name in [
            "mcp__github__search",
            "Write",
            "mcp__contextstream",
            "memory",
        ] {
            assert!(!is_contextstream_mcp_tool_name(name), "{name}");
        }
        assert_eq!(normalize_contextstream_tool_name("Search"), "search");
    }
}
