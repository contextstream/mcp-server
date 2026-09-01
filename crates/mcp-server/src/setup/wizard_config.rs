//! Workspace and project configuration management.
//!
//! Handles .contextstream/config.json in project directories.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::safe_edit;

/// Project-level ContextStream configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectConfig {
    /// Associated workspace ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,

    /// Associated project ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,

    /// Workspace name (for display).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_name: Option<String>,

    /// Project name (for display).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_name: Option<String>,

    /// Canonical local checkout root that established this binding.
    ///
    /// This is deliberately local-only metadata. It prevents a copied or
    /// stale `.contextstream/config.json` from authorizing uploads from a
    /// different checkout merely because its saved project UUID still exists.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkout_root: Option<String>,

    /// Configured editor types.
    #[serde(default)]
    pub configured_editors: Vec<String>,

    /// ContextStream version used for setup.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,

    /// When the config was created.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,

    /// When the config was last updated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,

    /// Forward-compatible fields written by other MCP versions or clients.
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

impl ProjectConfig {
    /// Create a new project config.
    pub fn new() -> Self {
        Self {
            version: Some(mcp_types::config::VERSION.to_string()),
            created_at: Some(chrono::Utc::now().to_rfc3339()),
            updated_at: Some(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        }
    }

    /// Set workspace info.
    pub fn with_workspace(mut self, id: &str, name: Option<&str>) -> Self {
        self.workspace_id = Some(id.to_string());
        self.workspace_name = name.map(String::from);
        self
    }

    /// Set project info.
    pub fn with_project(mut self, id: &str, name: Option<&str>) -> Self {
        self.project_id = Some(id.to_string());
        self.project_name = name.map(String::from);
        self
    }

    /// Add a configured editor.
    pub fn add_editor(&mut self, editor: &str) {
        if !self.configured_editors.contains(&editor.to_string()) {
            self.configured_editors.push(editor.to_string());
        }
        self.updated_at = Some(chrono::Utc::now().to_rfc3339());
    }
}

/// Get the config directory for a project.
pub fn project_config_dir(project_path: &Path) -> PathBuf {
    project_path.join(".contextstream")
}

/// Get the config file path for a project.
pub fn project_config_path(project_path: &Path) -> PathBuf {
    project_config_dir(project_path).join("config.json")
}

/// Read project configuration.
pub fn read_project_config(project_path: &Path) -> Result<Option<ProjectConfig>> {
    let path = project_config_path(project_path);

    // Use the same duplicate-safe JSON/JSONC reader as writes. This lets us
    // preserve a user's comments without producing a file that a later setup
    // run can no longer read.
    let loaded = safe_edit::read_for_edit(&path, safe_edit::JsonDialect::Strict)?;
    if !loaded.existed {
        return Ok(None);
    }
    let config: ProjectConfig = serde_json::from_value(loaded.value)?;

    Ok(Some(config))
}

/// Write project configuration.
pub fn write_project_config(project_path: &Path, config: &ProjectConfig) -> Result<()> {
    let mut stored_config = config.clone();
    stored_config.checkout_root = Some(canonical_checkout_root(project_path));
    let path = project_config_path(project_path);
    let loaded = safe_edit::read_for_edit(&path, safe_edit::JsonDialect::Strict)?;
    let serialized = serde_json::to_value(&stored_config)?;
    let serialized = serialized
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("Project config did not serialize to an object"))?;
    let mut updated = loaded.value.clone();
    let updated_object = updated
        .as_object_mut()
        .expect("safe_edit always loads an object");
    let known_keys = [
        "workspace_id",
        "project_id",
        "workspace_name",
        "project_name",
        "checkout_root",
        "configured_editors",
        "version",
        "created_at",
        "updated_at",
    ];
    let mut removed_keys = Vec::new();
    for key in known_keys {
        if let Some(value) = serialized.get(key) {
            updated_object.insert(key.to_string(), value.clone());
        } else if updated_object.remove(key).is_some() {
            removed_keys.push(key);
        }
    }
    for (key, value) in serialized {
        if !known_keys.contains(&key.as_str()) {
            updated_object.insert(key.clone(), value.clone());
        }
    }
    safe_edit::commit_with_removals(&path, &loaded, &updated, &removed_keys)?;

    // Add to .gitignore if it exists
    add_to_gitignore(project_path)?;

    Ok(())
}

/// Return the stable local identity used to bind a config to a checkout.
pub fn canonical_checkout_root(project_path: &Path) -> String {
    std::fs::canonicalize(project_path)
        .unwrap_or_else(|_| project_path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// Return whether a stored checkout identity belongs to `project_path`.
pub fn checkout_root_matches(stored_root: Option<&str>, project_path: &Path) -> bool {
    stored_root
        .map(str::trim)
        .filter(|root| !root.is_empty())
        .is_some_and(|root| Path::new(root) == Path::new(&canonical_checkout_root(project_path)))
}

/// Add .contextstream to .gitignore if not already present.
fn add_to_gitignore(project_path: &Path) -> Result<()> {
    let gitignore_path = project_path.join(".gitignore");

    if !gitignore_path.exists() {
        return Ok(());
    }

    let content = std::fs::read_to_string(&gitignore_path)?;

    if content.contains(".contextstream") {
        return Ok(());
    }

    let separator = if content.ends_with('\n') { "" } else { "\n" };
    let updated = format!("{content}{separator}\n# ContextStream config\n.contextstream/\n");
    safe_edit::write_if_unchanged(&gitignore_path, &updated, Some(&content))?;

    Ok(())
}

/// Find project root by looking for common project markers.
pub fn find_project_root(start_path: &Path) -> Option<PathBuf> {
    let markers = [
        ".git",
        "package.json",
        "Cargo.toml",
        "pyproject.toml",
        "go.mod",
        "pom.xml",
        "build.gradle",
        ".contextstream",
    ];

    let mut current = start_path.to_path_buf();

    loop {
        for marker in &markers {
            if current.join(marker).exists() {
                return Some(current);
            }
        }

        if !current.pop() {
            break;
        }
    }

    None
}

/// Discover projects in a directory.
pub fn discover_projects(root: &Path, max_depth: usize) -> Vec<PathBuf> {
    let mut projects = Vec::new();

    discover_projects_recursive(root, 0, max_depth, &mut projects);

    projects
}

fn discover_projects_recursive(
    path: &Path,
    depth: usize,
    max_depth: usize,
    projects: &mut Vec<PathBuf>,
) {
    if depth > max_depth {
        return;
    }

    // Check if this directory is a project
    let markers = ["package.json", "Cargo.toml", "pyproject.toml", "go.mod"];

    for marker in &markers {
        if path.join(marker).exists() {
            projects.push(path.to_path_buf());
            return; // Don't recurse into project subdirectories
        }
    }

    // Recurse into subdirectories
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.filter_map(|e| e.ok()) {
            let entry_path = entry.path();

            if entry_path.is_dir() {
                let name = entry_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("");

                // Skip hidden and common non-project directories
                if name.starts_with('.')
                    || name == "node_modules"
                    || name == "target"
                    || name == "dist"
                    || name == "build"
                    || name == "__pycache__"
                    || name == "venv"
                    || name == ".venv"
                {
                    continue;
                }

                discover_projects_recursive(&entry_path, depth + 1, max_depth, projects);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn test_project_config_new() {
        let config = ProjectConfig::new();
        assert!(config.version.is_some());
        assert!(config.created_at.is_some());
    }

    #[test]
    fn test_find_project_root() {
        let dir = tempdir().unwrap();
        // Create a nested structure deep enough that none of the parent directories
        // outside our tempdir can interfere (e.g. ~/.contextstream existing).
        let project_dir = dir
            .path()
            .join("isolated")
            .join("workspace")
            .join("project");
        fs::create_dir_all(&project_dir).unwrap();

        // Add a Cargo.toml marker — should find it in project_dir
        fs::write(project_dir.join("Cargo.toml"), "").unwrap();
        assert_eq!(find_project_root(&project_dir), Some(project_dir.clone()));

        // Remove the marker and verify it doesn't match the project_dir itself
        fs::remove_file(project_dir.join("Cargo.toml")).unwrap();
        // Note: find_project_root walks up directories, so it may find markers in
        // parent directories (e.g. ~/.contextstream). We only test the positive case.
    }

    #[test]
    fn write_project_config_preserves_unknown_fields_and_binds_checkout() {
        let dir = tempdir().unwrap();
        let mut config = ProjectConfig::new();
        config.extra.insert(
            "associated_at".to_string(),
            Value::String("2026-07-20T00:00:00Z".to_string()),
        );
        config.extra.insert(
            "future_field".to_string(),
            serde_json::json!({"enabled": true}),
        );

        write_project_config(dir.path(), &config).expect("write config");
        let stored = read_project_config(dir.path())
            .expect("read config")
            .expect("config");

        assert_eq!(
            stored.extra.get("associated_at").and_then(Value::as_str),
            Some("2026-07-20T00:00:00Z")
        );
        assert_eq!(
            stored.extra.get("future_field"),
            Some(&serde_json::json!({"enabled": true}))
        );
        assert_eq!(
            stored.checkout_root.as_deref(),
            Some(canonical_checkout_root(dir.path()).as_str())
        );
    }

    #[test]
    fn write_project_config_preserves_unknown_existing_fields_and_comments() {
        let dir = tempdir().unwrap();
        let config_dir = project_config_dir(dir.path());
        fs::create_dir_all(&config_dir).unwrap();
        let path = project_config_path(dir.path());
        let original = "{\n  // user comment\n  \"future\": {\"keep\": true},\n  \"workspace_id\": \"old\"\n}\n";
        fs::write(&path, original).unwrap();

        let config = ProjectConfig::new().with_workspace("new", Some("Engineering"));
        write_project_config(dir.path(), &config).expect("surgical config update");

        let updated = fs::read_to_string(path).unwrap();
        assert!(updated.contains("// user comment"));
        assert!(updated.contains("\"future\": {\"keep\": true}"));
        let stored = read_project_config(dir.path())
            .expect("read updated config")
            .expect("config");
        assert_eq!(stored.workspace_id.as_deref(), Some("new"));
        assert_eq!(
            stored.extra.get("future"),
            Some(&serde_json::json!({"keep": true}))
        );
    }

    #[test]
    fn project_config_duplicate_keys_fail_closed() {
        let dir = tempdir().unwrap();
        let config_dir = project_config_dir(dir.path());
        fs::create_dir_all(&config_dir).unwrap();
        let path = project_config_path(dir.path());
        let original = "{\"future\":{\"mode\":1,\"mode\":2},\"workspace_id\":\"old\"}\n";
        fs::write(&path, original).unwrap();

        assert!(read_project_config(dir.path()).is_err());
        let config = ProjectConfig::new().with_workspace("new", None);
        assert!(write_project_config(dir.path(), &config).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), original);
    }
}
