//! Import Claude Code auto memory files into ContextStream.
//!
//! While Claude Code's auto memory is on, its system prompt saves "remember"
//! requests and preferences as `<claude config>/projects/<project>/memory/*.md`.
//! Those stay on one machine and never reach other sessions, machines or
//! Codex. This copies each file into a ContextStream memory node once:
//! feedback/user memories become preferences, lessons become lessons, and
//! project/reference memories become facts. Files are left in place, and a
//! receipt per content hash makes a second run a no-op.

use anyhow::Result;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use uuid::Uuid;

use crate::hook_handlers::durable_capture::{read_receipt, record_receipt};

/// One auto memory file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalMemory {
    pub path: PathBuf,
    /// Claude's encoded project directory name (e.g. `-Users-me-repo`).
    pub project_dir: String,
    pub name: String,
    pub description: String,
    pub memory_type: Option<String>,
    pub body: String,
    pub sha256: String,
}

impl LocalMemory {
    /// The ContextStream memory node type for this memory.
    pub fn node_type(&self) -> &'static str {
        match self.memory_type.as_deref() {
            Some("feedback" | "user" | "preference") => "preference",
            Some("lesson") => "lesson",
            _ => "fact",
        }
    }

    fn title(&self) -> String {
        let title = if self.description.is_empty() {
            self.name.as_str()
        } else {
            self.description.as_str()
        };
        title.chars().take(200).collect()
    }
}

fn frontmatter_value(frontmatter: &str, key: &str) -> Option<String> {
    frontmatter.lines().find_map(|line| {
        let value = line.trim().strip_prefix(key)?.strip_prefix(':')?;
        let value = value.trim().trim_matches(|c| c == '"' || c == '\'').trim();
        (!value.is_empty()).then(|| value.to_string())
    })
}

/// Parse one memory file: YAML-ish frontmatter (`name`, `description`,
/// `type` or `metadata.type`) followed by the body.
pub fn parse_memory(path: &Path, project_dir: &str, raw: &str) -> LocalMemory {
    let (frontmatter, body) = raw
        .strip_prefix("---")
        .and_then(|rest| rest.split_once("\n---"))
        .map(|(frontmatter, body)| (frontmatter, body.trim_start_matches(['-', '\n', '\r'])))
        .unwrap_or(("", raw));
    let file_stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("memory")
        .to_string();
    LocalMemory {
        path: path.to_path_buf(),
        project_dir: project_dir.to_string(),
        name: frontmatter_value(frontmatter, "name").unwrap_or(file_stem),
        description: frontmatter_value(frontmatter, "description").unwrap_or_default(),
        memory_type: frontmatter_value(frontmatter, "type").map(|t| t.to_ascii_lowercase()),
        body: body.trim().to_string(),
        sha256: hex::encode(Sha256::digest(raw.as_bytes())),
    }
}

/// Every auto memory file under `<config_dir>/projects/*/memory/`, except the
/// `MEMORY.md` index.
pub fn discover(config_dir: &Path) -> Vec<LocalMemory> {
    let Ok(projects) = std::fs::read_dir(config_dir.join("projects")) else {
        return Vec::new();
    };
    let mut memories = Vec::new();
    for project in projects.flatten() {
        let project_dir = project.file_name().to_string_lossy().to_string();
        let Ok(files) = std::fs::read_dir(project.path().join("memory")) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            let is_memory = path.extension().is_some_and(|ext| ext == "md")
                && !path
                    .file_name()
                    .is_some_and(|name| name.eq_ignore_ascii_case("MEMORY.md"));
            if !is_memory {
                continue;
            }
            if let Ok(raw) = std::fs::read_to_string(&path) {
                memories.push(parse_memory(&path, &project_dir, &raw));
            }
        }
    }
    memories.sort_by(|a, b| a.path.cmp(&b.path));
    memories
}

/// Claude Code's project directory name for a folder: every character other
/// than an ASCII letter, digit or `-` becomes `-`.
pub fn claude_project_dir_name(folder: &str) -> String {
    folder
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Workspace/project for a Claude project directory, from the folder mappings
/// `init` records in `~/.contextstream/mappings.json`.
fn scope_for_project_dir(mappings: &Value, project_dir: &str) -> (Option<Uuid>, Option<Uuid>) {
    let entries = mappings
        .get("mappings")
        .and_then(Value::as_array)
        .or_else(|| mappings.as_array());
    let Some(entry) = entries.and_then(|entries| {
        entries.iter().find(|entry| {
            entry
                .get("path")
                .and_then(Value::as_str)
                .is_some_and(|path| {
                    claude_project_dir_name(path.trim_end_matches('/')) == project_dir
                })
        })
    }) else {
        return (None, None);
    };
    let id = |key: &str| {
        entry
            .get(key)
            .and_then(Value::as_str)
            .and_then(|id| Uuid::parse_str(id).ok())
    };
    (id("workspace_id"), id("project_id"))
}

fn receipts_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".contextstream").join("imports.json"))
}

fn claude_config_dir() -> Option<PathBuf> {
    crate::hook_handlers::durable_paths::claude_config_dir()
}

/// Which files to import and where, beyond the folder links `init` records.
#[derive(Debug, Clone, Default)]
pub struct ImportOptions {
    pub dry_run: bool,
    /// Only this Claude project directory (e.g. `-Users-me-repo`).
    pub project_dir: Option<String>,
    /// Scope to use instead of the folder's link (for unlinked or stale links).
    pub workspace_id: Option<Uuid>,
    pub project_id: Option<Uuid>,
}

/// Import every not-yet-imported auto memory file. With `dry_run`, only list
/// what would be imported. Returns `(imported, skipped, failed)`.
pub async fn run(options: ImportOptions) -> Result<(usize, usize, usize)> {
    let dry_run = options.dry_run;
    let config_dir =
        claude_config_dir().ok_or_else(|| anyhow::anyhow!("Could not find the Claude config"))?;
    let memories: Vec<LocalMemory> = discover(&config_dir)
        .into_iter()
        .filter(|memory| {
            options
                .project_dir
                .as_deref()
                .is_none_or(|dir| memory.project_dir == dir)
        })
        .collect();
    if memories.is_empty() {
        println!(
            "No Claude auto memory files found under {}",
            config_dir.display()
        );
        return Ok((0, 0, 0));
    }
    let receipts =
        receipts_path().ok_or_else(|| anyhow::anyhow!("Could not find the home directory"))?;
    let home = dirs::home_dir().unwrap_or_default();
    let config = crate::hook_handlers::common::load_config(&home.to_string_lossy());
    if !dry_run && !config.is_configured() {
        anyhow::bail!("No ContextStream credentials found; run `contextstream-mcp setup` first");
    }
    let mappings: Value =
        std::fs::read_to_string(home.join(".contextstream").join("mappings.json"))
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or(Value::Null);

    let (mut imported, mut skipped, mut failed) = (0, 0, 0);
    for memory in &memories {
        if read_receipt(&receipts, &memory.sha256).is_some() {
            skipped += 1;
            continue;
        }
        let (linked_workspace, linked_project) =
            scope_for_project_dir(&mappings, &memory.project_dir);
        let (workspace_id, project_id) = match options.workspace_id {
            Some(workspace) => (Some(workspace), options.project_id),
            None => (linked_workspace, options.project_id.or(linked_project)),
        };
        let label = format!(
            "{} '{}' ({})",
            memory.node_type(),
            memory.name,
            memory.project_dir
        );
        let Some(workspace_id) = workspace_id else {
            eprintln!(
                "skipped {label}: its folder is not linked to a ContextStream workspace. Link it with `contextstream-mcp setup --project-path <folder>`, or rerun with --project-dir={} --workspace-id <id>.",
                memory.project_dir
            );
            failed += 1;
            continue;
        };
        if dry_run {
            println!("would import {label}");
            imported += 1;
            continue;
        }
        let client = mcp_client::ContextStreamClient::new(mcp_types::Config {
            api_url: config.api_url.clone(),
            api_key: Some(config.api_key.clone()),
            default_workspace_id: Some(workspace_id),
            default_project_id: project_id,
            ..Default::default()
        });
        let content = format!(
            "{}\n\nImported from Claude Code auto memory ({}/memory/{}).",
            memory.body,
            memory.project_dir,
            memory
                .path
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default()
        );
        let result = client
            .create_memory_node(mcp_client::CreateMemoryNodeParams {
                node_type: memory.node_type().to_string(),
                title: memory.title(),
                content: Some(crate::hook_handlers::common::scrub_credential_tokens(
                    &content,
                )),
                workspace_id: Some(workspace_id),
                project_id,
                metadata: Some(serde_json::json!({
                    "source": "claude_auto_memory",
                    "claude_project": memory.project_dir,
                    "name": memory.name,
                })),
            })
            .await;
        match result {
            Ok(created) => {
                let id = created
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("created")
                    .to_string();
                record_receipt(&receipts, &memory.sha256, &id, &memory.sha256);
                println!("imported {label}");
                imported += 1;
            }
            Err(error) => {
                let hint = if error.to_string().contains("403") {
                    format!(
                        " The folder's link points at workspace {workspace_id}, which this account cannot write to; rerun with --project-dir={} --workspace-id <id> [--project-id <id>].",
                        memory.project_dir
                    )
                } else {
                    String::new()
                };
                eprintln!("could not import {label}: {error}.{hint}");
                failed += 1;
            }
        }
    }
    println!(
        "{} {imported}, already imported {skipped}, failed {failed}. The local files were left in place.",
        if dry_run { "Would import" } else { "Imported" }
    );
    Ok((imported, skipped, failed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_files_map_to_contextstream_node_types() {
        let raw = "---\nname: commit-directly-to-main\ndescription: \"In this repo, commit straight to main\"\nmetadata:\n  type: feedback\n---\n\nCommit to main.\n**Why:** owner directive.\n";
        let memory = parse_memory(
            Path::new("/c/projects/-Users-me-repo/memory/commit-directly-to-main.md"),
            "-Users-me-repo",
            raw,
        );
        assert_eq!(memory.name, "commit-directly-to-main");
        assert_eq!(memory.description, "In this repo, commit straight to main");
        assert_eq!(memory.memory_type.as_deref(), Some("feedback"));
        assert_eq!(memory.node_type(), "preference");
        assert_eq!(memory.title(), "In this repo, commit straight to main");
        assert!(memory.body.starts_with("Commit to main."));

        for (kind, node) in [
            ("user", "preference"),
            ("lesson", "lesson"),
            ("project", "fact"),
            ("reference", "fact"),
        ] {
            let raw = format!("---\nname: x\ntype: {kind}\n---\nbody");
            assert_eq!(
                parse_memory(Path::new("/x.md"), "-p", &raw).node_type(),
                node
            );
        }
        let bare = parse_memory(Path::new("/c/note.md"), "-p", "just text");
        assert_eq!(bare.name, "note");
        assert_eq!(bare.node_type(), "fact");
        assert_eq!(bare.body, "just text");
    }

    #[test]
    fn claude_project_directories_resolve_through_folder_mappings() {
        assert_eq!(
            claude_project_dir_name("/Users/dev/src/app-desktop"),
            "-Users-dev-src-app-desktop"
        );
        assert_eq!(
            claude_project_dir_name("/Users/dev/.buzz"),
            "-Users-dev--buzz"
        );
        let workspace = Uuid::new_v4();
        let project = Uuid::new_v4();
        let mappings = serde_json::json!({"mappings": [
            {"path": "/Users/dev/src/app", "workspace_id": Uuid::new_v4().to_string()},
            {"path": "/Users/dev/src/app-desktop", "workspace_id": workspace.to_string(), "project_id": project.to_string()},
        ]});
        assert_eq!(
            scope_for_project_dir(&mappings, "-Users-dev-src-app-desktop"),
            (Some(workspace), Some(project))
        );
        assert_eq!(
            scope_for_project_dir(&mappings, "-Users-dev-other"),
            (None, None)
        );
    }

    #[test]
    fn discovery_skips_indexes_and_non_markdown() {
        let temp = tempfile::tempdir().unwrap();
        let memory = temp.path().join("projects/-repo/memory");
        std::fs::create_dir_all(&memory).unwrap();
        std::fs::write(memory.join("MEMORY.md"), "- [a](a.md)").unwrap();
        std::fs::write(memory.join("a.md"), "---\nname: a\ntype: user\n---\nA").unwrap();
        std::fs::write(memory.join("notes.txt"), "x").unwrap();
        let found = discover(temp.path());
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "a");
        assert_eq!(found[0].project_dir, "-repo");
    }
}
