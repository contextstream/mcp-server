//! Editor identity, detection, and configuration layout.
//!
//! [`Editor`] is setup's view of an installable harness. Everything that is a
//! matter of file layout (config and rules paths, config dialect, install
//! evidence, reload guidance) is declared once per editor in
//! [`super::clients`]; the methods here resolve those descriptors.

use std::path::{Path, PathBuf};

use mcp_types::{HarnessId, HarnessProfile};

use super::clients::{self, ClientDescriptor, ClientStatus, ConfigDialect};

/// Supported editor types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Editor {
    ClaudeCode,
    Cursor,
    Windsurf,
    Copilot,
    Cline,
    KiloCode,
    RooCode,
    Codex,
    Aider,
    Antigravity,
    OpenCode,
}

/// Enforcement capability tier by editor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnforcementTier {
    /// Hard first-call enforcement can be reliably blocked in pre-tool hooks.
    TierA,
    /// Dynamic reminders/injection possible, but hard blocking is not fully documented.
    TierB,
    /// No reliable hook lifecycle documented; static rule guidance is the fallback.
    TierC,
}

/// Editors exposed to users in setup/configure flows.
///
/// To disable an editor, remove it from this list.
const ENABLED_EDITORS: [Editor; 11] = [
    Editor::ClaudeCode,
    Editor::Cursor,
    Editor::Windsurf,
    Editor::Copilot,
    Editor::Cline,
    Editor::KiloCode,
    Editor::RooCode,
    Editor::Codex,
    Editor::Aider,
    Editor::Antigravity,
    Editor::OpenCode,
];

impl Editor {
    /// Get all supported editors in display order.
    pub fn all() -> &'static [Editor] {
        &ENABLED_EDITORS
    }

    /// Get the display name for this editor.
    pub fn display_name(&self) -> &'static str {
        self.harness_id().display_name()
    }

    /// Get the identifier for this editor.
    pub fn id(&self) -> &'static str {
        self.harness_id().as_str()
    }

    /// Canonical dependency-neutral harness id.
    pub const fn harness_id(&self) -> HarnessId {
        match self {
            Editor::ClaudeCode => HarnessId::ClaudeCode,
            Editor::Cursor => HarnessId::Cursor,
            Editor::Windsurf => HarnessId::Windsurf,
            Editor::Copilot => HarnessId::Copilot,
            Editor::Cline => HarnessId::Cline,
            Editor::KiloCode => HarnessId::KiloCode,
            Editor::RooCode => HarnessId::RooCode,
            Editor::Codex => HarnessId::Codex,
            Editor::Aider => HarnessId::Aider,
            Editor::Antigravity => HarnessId::Antigravity,
            Editor::OpenCode => HarnessId::OpenCode,
        }
    }

    /// Shared capability profile used by setup, teaching, and readiness.
    pub const fn profile(&self) -> HarnessProfile {
        self.harness_id().profile()
    }

    /// Inverse of [`Editor::id`]: resolve a slug (as stored in a setup
    /// profile) back to the editor. Unknown slugs return `None` so callers
    /// can skip-and-warn instead of failing the whole setup.
    pub fn from_id(id: &str) -> Option<Editor> {
        let harness = HarnessId::from_alias(id)?;
        Self::from_harness_id(harness)
    }

    /// Resolve a canonical installable harness id to setup's file-layout type.
    pub const fn from_harness_id(harness: HarnessId) -> Option<Editor> {
        match harness {
            HarnessId::ClaudeCode => Some(Editor::ClaudeCode),
            HarnessId::Cursor => Some(Editor::Cursor),
            HarnessId::Windsurf => Some(Editor::Windsurf),
            HarnessId::Copilot => Some(Editor::Copilot),
            HarnessId::Cline => Some(Editor::Cline),
            HarnessId::KiloCode => Some(Editor::KiloCode),
            HarnessId::RooCode => Some(Editor::RooCode),
            HarnessId::Codex => Some(Editor::Codex),
            HarnessId::Aider => Some(Editor::Aider),
            HarnessId::Antigravity => Some(Editor::Antigravity),
            HarnessId::OpenCode => Some(Editor::OpenCode),
            HarnessId::ChatGptGateway
            | HarnessId::OpenAiResponses
            | HarnessId::ContextStreamCli
            | HarnessId::ContextCode => None,
        }
    }

    /// Return the enforcement tier for this editor.
    pub fn enforcement_tier(&self) -> EnforcementTier {
        let profile = self.profile();
        if profile.hard_first_call_enforcement {
            EnforcementTier::TierA
        } else if profile.dynamic_guidance || profile.hooks.any() {
            EnforcementTier::TierB
        } else {
            EnforcementTier::TierC
        }
    }

    /// Whether this editor supports hard first-call blocking.
    pub fn supports_hard_enforcement(&self) -> bool {
        matches!(self.enforcement_tier(), EnforcementTier::TierA)
    }

    /// The declarative file-layout descriptor for this editor.
    pub fn descriptor(&self) -> &'static ClientDescriptor {
        clients::descriptor(*self)
    }

    /// Whether this editor is discontinued upstream.
    ///
    /// Deprecated editors are never offered for a new setup (selection or
    /// detection), but existing installs stay fully maintainable: doctor,
    /// hook refresh, update, and uninstall still reach them.
    pub fn is_deprecated(&self) -> bool {
        matches!(self.descriptor().status, ClientStatus::Discontinued { .. })
    }

    /// Where users of a deprecated editor should go instead.
    pub fn deprecation_successor(&self) -> Option<Editor> {
        match self.descriptor().status {
            ClientStatus::Discontinued { successor } => Some(successor),
            ClientStatus::Supported => None,
        }
    }

    /// Editors a new setup may offer or auto-select.
    pub fn selectable() -> Vec<Editor> {
        Editor::all()
            .iter()
            .copied()
            .filter(|editor| !editor.is_deprecated())
            .collect()
    }

    /// Get the MCP config file path for this editor.
    pub fn mcp_config_path(&self) -> Option<PathBuf> {
        self.descriptor()
            .global_config
            .and_then(|spec| spec.resolve())
    }

    /// Global MCP config locations earlier releases wrote that the client
    /// never reads (or no longer reads). They are cleanup-only: setup and
    /// uninstall strip a managed ContextStream entry from them, and nothing
    /// ever writes to them again.
    pub fn legacy_mcp_config_paths(&self) -> Vec<PathBuf> {
        let primary = self.mcp_config_path();
        self.descriptor()
            .legacy_global_configs
            .iter()
            .filter_map(|spec| spec.resolve())
            .filter(|path| Some(path) != primary.as_ref())
            .collect()
    }

    /// Get the rules file path for this editor.
    pub fn rules_path(&self, project_path: Option<&Path>) -> Option<PathBuf> {
        let rules = &self.descriptor().rules;
        match project_path {
            Some(project) => rules.project.map(|spec| spec.resolve(project)),
            None => rules.global.and_then(|spec| spec.resolve()),
        }
    }

    /// Additional legacy/alternate rules paths to check for migration/update.
    ///
    /// These paths are read/update candidates only. The primary managed location
    /// remains `rules_path(...)`.
    pub fn legacy_rules_paths(&self, project_path: Option<&Path>) -> Vec<PathBuf> {
        let rules = &self.descriptor().rules;
        resolve_rules(rules.legacy_project, rules.legacy_global, project_path)
    }

    /// Legacy rules paths that are cleanup-only.
    ///
    /// These locations are scanned/cleaned for stale ContextStream blocks, but
    /// are never write targets for new managed rules.
    pub fn legacy_cleanup_only_rules_paths(&self, project_path: Option<&Path>) -> Vec<PathBuf> {
        let rules = &self.descriptor().rules;
        resolve_rules(rules.cleanup_project, rules.cleanup_global, project_path)
    }

    /// All managed rules paths for this editor (primary first, then alternates).
    pub fn all_rules_paths(&self, project_path: Option<&Path>) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        if let Some(primary) = self.rules_path(project_path) {
            paths.push(primary);
        }

        for legacy in self.legacy_rules_paths(project_path) {
            if !paths.contains(&legacy) {
                paths.push(legacy);
            }
        }

        paths
    }

    /// All rules paths that should be scanned for ContextStream cleanup.
    pub fn all_rules_cleanup_paths(&self, project_path: Option<&Path>) -> Vec<PathBuf> {
        let mut paths = self.all_rules_paths(project_path);
        for path in self.legacy_cleanup_only_rules_paths(project_path) {
            if !paths.contains(&path) {
                paths.push(path);
            }
        }
        paths
    }

    /// Get the project-level MCP config path for this editor.
    pub fn project_mcp_config_path(&self, project_path: &Path) -> Option<PathBuf> {
        self.descriptor()
            .project_config
            .map(|config| config.path.resolve(project_path))
    }

    /// Check if this editor supports project-level MCP config.
    pub fn supports_project_mcp_config(&self) -> bool {
        self.descriptor().project_config.is_some()
    }

    /// Check if this editor uses JSON MCP config.
    pub fn uses_json_config(&self) -> bool {
        matches!(
            self.descriptor().dialect,
            ConfigDialect::JsonServers { .. }
                | ConfigDialect::VsCodeSettings { .. }
                | ConfigDialect::Kilo
        )
    }

    /// Check if this editor uses VS Code extensions settings.
    pub fn uses_vscode_settings(&self) -> bool {
        matches!(
            self.descriptor().dialect,
            ConfigDialect::VsCodeSettings { .. }
        )
    }

    /// Check if this editor supports hooks (dynamic enforcement).
    /// Editors without hooks need expanded static rules.
    pub fn has_hooks(&self) -> bool {
        self.profile().hooks.any()
    }

    /// Whether this harness has an MCP transport that can produce a runtime
    /// handshake. Aider is intentionally rules-only and must never be
    /// presented as merely waiting for an MCP connection.
    pub fn has_mcp_transport(&self) -> bool {
        self.profile().mcp_support != mcp_types::McpTransportSupport::None
    }

    /// Client-specific action required after setup changes an MCP config or
    /// managed rules. Keep these instructions conservative: a fresh process
    /// or window is valid even when a client also supports a narrower hot
    /// reload.
    pub fn activation_reload_instruction(&self) -> &'static str {
        self.descriptor().reload_instruction
    }

    /// Whether any install evidence for this editor is present.
    pub fn is_installed(&self) -> bool {
        self.descriptor()
            .detect
            .iter()
            .any(|evidence| evidence.present())
    }

    /// Get the heading used in generated rules files.
    ///
    /// Keep this neutral across editors to avoid redundant or confusing titles
    /// like "Claude Code Instructions" in a file that is already all rules.
    pub fn rules_heading(&self) -> &'static str {
        "# ContextStream Rules"
    }
}

impl std::fmt::Display for Editor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.display_name())
    }
}

fn resolve_rules(
    project_specs: &[clients::ProjectPathSpec],
    global_specs: &[clients::PathSpec],
    project_path: Option<&Path>,
) -> Vec<PathBuf> {
    match project_path {
        Some(project) => project_specs
            .iter()
            .map(|spec| spec.resolve(project))
            .collect(),
        None => global_specs
            .iter()
            .filter_map(|spec| spec.resolve())
            .collect(),
    }
}

/// Serialize detected editors as JSON for non-interactive use by install scripts.
pub fn detect_installed_editors_json() -> serde_json::Value {
    let detected = detect_installed_editors();
    let editors: Vec<serde_json::Value> = detected
        .iter()
        .map(|editor| {
            serde_json::json!({
                "id": editor.id(),
                "name": editor.display_name(),
                "enforcement_tier": match editor.enforcement_tier() {
                    EnforcementTier::TierA => "A",
                    EnforcementTier::TierB => "B",
                    EnforcementTier::TierC => "C",
                },
                "supports_remote": super::mcp_config::editor_supports_remote_mcp(editor),
                "supports_hooks": editor.has_hooks(),
                "supports_project_config": editor.supports_project_mcp_config(),
                "config_path": editor.mcp_config_path().map(|p| p.to_string_lossy().to_string()).unwrap_or_default(),
                "rules_path": editor.rules_path(None).map(|p| p.to_string_lossy().to_string()).unwrap_or_default(),
            })
        })
        .collect();

    let all_editors: Vec<serde_json::Value> = Editor::all()
        .iter()
        .map(|editor| {
            serde_json::json!({
                "id": editor.id(),
                "name": editor.display_name(),
                "installed": detected.contains(editor),
            })
        })
        .collect();

    serde_json::json!({
        "detected": editors,
        "all": all_editors,
    })
}

/// Installed editors a new setup may auto-select (discontinued ones excluded).
pub fn detect_installed_editors_for_setup() -> Vec<Editor> {
    detect_installed_editors()
        .into_iter()
        .filter(|editor| !editor.is_deprecated())
        .collect()
}

/// Detect all installed editors, including discontinued ones so cleanup,
/// doctor, and hook refresh can still reach existing installs.
pub fn detect_installed_editors() -> Vec<Editor> {
    Editor::all()
        .iter()
        .copied()
        .filter(|editor| is_editor_installed(*editor))
        .collect()
}

fn is_editor_installed(editor: Editor) -> bool {
    editor.is_installed()
}

/// Cursor install locations outside HOME and PATH.
pub(crate) fn cursor_platform_install_present() -> bool {
    #[cfg(target_os = "macos")]
    {
        if std::path::Path::new("/Applications/Cursor.app").exists() {
            return true;
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Some(local_app_data) = dirs::data_local_dir() {
            if local_app_data
                .join("Programs")
                .join("cursor")
                .join("Cursor.exe")
                .exists()
            {
                return true;
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        for path in ["/usr/bin/cursor", "/usr/local/bin/cursor"] {
            if std::path::Path::new(path).exists() {
                return true;
            }
        }

        if let Some(home) = dirs::home_dir() {
            if home.join("Applications").join("cursor.AppImage").exists() {
                return true;
            }
        }
    }

    false
}

/// Windsurf install locations outside HOME and PATH.
pub(crate) fn windsurf_platform_install_present() -> bool {
    #[cfg(target_os = "macos")]
    {
        if std::path::Path::new("/Applications/Windsurf.app").exists() {
            return true;
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Some(local_app_data) = dirs::data_local_dir() {
            if local_app_data
                .join("Programs")
                .join("Windsurf")
                .join("Windsurf.exe")
                .exists()
            {
                return true;
            }
        }
    }

    #[cfg(target_os = "linux")]
    {
        for path in ["/usr/bin/windsurf", "/usr/local/bin/windsurf"] {
            if std::path::Path::new(path).exists() {
                return true;
            }
        }

        if let Some(home) = dirs::home_dir() {
            for app_image in ["Windsurf.AppImage", "windsurf.AppImage"] {
                if home.join("Applications").join(app_image).exists() {
                    return true;
                }
            }
        }
    }

    false
}

/// Check if a VS Code extension is installed.
pub(crate) fn vscode_extension_installed(extension_id: &str) -> bool {
    if let Some(extensions_dir) = vscode_extensions_dir() {
        if extensions_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(&extensions_dir) {
                for entry in entries.filter_map(|e| e.ok()) {
                    let name = entry.file_name().to_string_lossy().to_lowercase();
                    if name.starts_with(&extension_id.to_lowercase()) {
                        return true;
                    }
                }
            }
        }
    }

    false
}

/// `$XDG_CONFIG_HOME`, else `~/.config`, on every OS.
///
/// Node CLIs built on xdg-basedir (OpenCode, Kilo) use this layout even on
/// macOS and Windows, where the platform config dir (`dirs::config_dir`)
/// points somewhere else.
pub fn xdg_config_home() -> Option<PathBuf> {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
}

/// Claude Code's user-scope state file, which holds user-scoped MCP servers.
pub fn claude_user_config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude.json"))
}

/// OpenCode global config directory (`~/.config/opencode/`).
pub fn opencode_config_dir() -> Option<PathBuf> {
    xdg_config_home().map(|c| c.join("opencode"))
}

/// Pick the OpenCode global config file, reusing `opencode.jsonc` when the
/// user already has one so setup never creates a competing file.
pub fn opencode_global_config_file(dir: PathBuf) -> PathBuf {
    let jsonc = dir.join("opencode.jsonc");
    if jsonc.exists() {
        return jsonc;
    }
    dir.join("opencode.json")
}

/// Get Kilo CLI config directory (~/.config/kilo/).
pub fn kilo_config_dir() -> Option<PathBuf> {
    xdg_config_home().map(|c| c.join("kilo"))
}

/// Pick the Kilo global config file inside ~/.config/kilo/.
///
/// Kilo accepts kilo.jsonc, kilo.json, and config.json; reuse whichever
/// already exists (first match wins in that order) so setup upserts into the
/// user's real config instead of creating a competing file. Defaults to
/// kilo.jsonc for fresh installs.
pub fn kilo_global_config_file(dir: PathBuf) -> PathBuf {
    for name in ["kilo.jsonc", "kilo.json", "config.json"] {
        let candidate = dir.join(name);
        if candidate.exists() {
            return candidate;
        }
    }
    dir.join("kilo.jsonc")
}

/// Pick the Kilo project-level config file.
///
/// Kilo reads root `kilo.jsonc`/`kilo.json` or `.kilo/kilo.jsonc`/`.kilo/kilo.json`;
/// reuse whichever exists, defaulting to the root kilo.jsonc the docs use as
/// the idiomatic example.
pub fn kilo_project_config_file(project_path: &Path) -> PathBuf {
    for rel in [
        "kilo.jsonc",
        "kilo.json",
        ".kilo/kilo.jsonc",
        ".kilo/kilo.json",
    ] {
        let candidate = project_path.join(rel);
        if candidate.exists() {
            return candidate;
        }
    }
    project_path.join("kilo.jsonc")
}

/// Get VS Code extensions directory.
fn vscode_extensions_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".vscode").join("extensions"))
}

/// VS Code User directory (platform-specific).
pub(crate) fn vscode_user_dir() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        dirs::home_dir().map(|h| {
            h.join("Library")
                .join("Application Support")
                .join("Code")
                .join("User")
        })
    }

    #[cfg(target_os = "windows")]
    {
        dirs::data_dir().map(|d| d.join("Code").join("User"))
    }

    #[cfg(target_os = "linux")]
    {
        dirs::config_dir().map(|c| c.join("Code").join("User"))
    }
}

/// Get Claude Desktop config path (for reference).
pub fn _claude_desktop_config_path() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        dirs::home_dir().map(|h| {
            h.join("Library")
                .join("Application Support")
                .join("Claude")
                .join("claude_desktop_config.json")
        })
    }

    #[cfg(target_os = "windows")]
    {
        dirs::data_dir().map(|d| d.join("Claude").join("claude_desktop_config.json"))
    }

    #[cfg(target_os = "linux")]
    {
        dirs::config_dir().map(|c| c.join("Claude").join("claude_desktop_config.json"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_editor_display_name() {
        assert_eq!(Editor::ClaudeCode.display_name(), "Claude Code");
        assert_eq!(Editor::Cursor.display_name(), "Cursor");
    }

    #[test]
    fn test_editor_id() {
        assert_eq!(Editor::ClaudeCode.id(), "claude");
        assert_eq!(Editor::Cursor.id(), "cursor");
    }

    #[test]
    fn test_detect_installed_editors() {
        // Just verify it doesn't panic
        let _ = detect_installed_editors();
    }

    #[test]
    fn test_all_editors_list() {
        let all = Editor::all();
        assert_eq!(all.len(), 11);
        assert!(all.contains(&Editor::ClaudeCode));
        assert!(all.contains(&Editor::Cursor));
        assert!(all.contains(&Editor::Windsurf));
        assert!(all.contains(&Editor::Copilot));
        assert!(all.contains(&Editor::Cline));
        assert!(all.contains(&Editor::KiloCode));
        assert!(all.contains(&Editor::RooCode));
        assert!(all.contains(&Editor::Codex));
        assert!(all.contains(&Editor::Aider));
        assert!(all.contains(&Editor::Antigravity));
        assert!(all.contains(&Editor::OpenCode));
    }

    #[test]
    fn activation_reload_instructions_cover_every_enabled_editor() {
        for editor in Editor::all() {
            let instruction = editor.activation_reload_instruction();
            assert!(
                !instruction.trim().is_empty(),
                "{} needs an activation reload instruction",
                editor.display_name()
            );
            assert!(
                instruction.contains("checkout"),
                "{} instruction must retain exact checkout scope: {instruction}",
                editor.display_name()
            );
        }
    }

    #[test]
    fn aider_is_explicitly_rules_only_while_other_enabled_editors_have_mcp() {
        for editor in Editor::all() {
            assert_eq!(
                editor.has_mcp_transport(),
                !matches!(editor, Editor::Aider),
                "unexpected MCP transport classification for {}",
                editor.display_name()
            );
        }
        let aider_instruction = Editor::Aider.activation_reload_instruction();
        assert!(aider_instruction.contains("rules-only"));
        assert!(aider_instruction.contains("no MCP handshake"));
    }

    #[test]
    fn test_editor_enforcement_tiers() {
        assert_eq!(
            Editor::ClaudeCode.enforcement_tier(),
            EnforcementTier::TierA
        );
        assert_eq!(Editor::Cursor.enforcement_tier(), EnforcementTier::TierA);
        assert_eq!(Editor::Windsurf.enforcement_tier(), EnforcementTier::TierA);
        assert_eq!(Editor::Cline.enforcement_tier(), EnforcementTier::TierA);
        assert_eq!(Editor::Copilot.enforcement_tier(), EnforcementTier::TierC);
        assert_eq!(Editor::KiloCode.enforcement_tier(), EnforcementTier::TierB);
        assert_eq!(Editor::RooCode.enforcement_tier(), EnforcementTier::TierB);
        assert_eq!(Editor::Codex.enforcement_tier(), EnforcementTier::TierC);
        assert_eq!(Editor::Aider.enforcement_tier(), EnforcementTier::TierC);
        assert_eq!(
            Editor::Antigravity.enforcement_tier(),
            EnforcementTier::TierC
        );
        assert_eq!(Editor::OpenCode.enforcement_tier(), EnforcementTier::TierC);
    }

    #[test]
    fn every_setup_editor_round_trips_through_the_canonical_harness_registry() {
        assert_eq!(Editor::all().len(), HarnessId::INSTALLABLE.len());
        for editor in Editor::all() {
            let harness = editor.harness_id();
            assert!(HarnessId::INSTALLABLE.contains(&harness));
            assert_eq!(Editor::from_harness_id(harness), Some(*editor));
            assert_eq!(Editor::from_id(harness.as_str()), Some(*editor));
            assert_eq!(editor.id(), harness.as_str());
            assert_eq!(editor.display_name(), harness.display_name());
            assert_eq!(editor.profile().id, harness);
        }
    }

    #[test]
    fn runtime_only_harnesses_cannot_become_setup_targets() {
        for harness in [
            HarnessId::ChatGptGateway,
            HarnessId::OpenAiResponses,
            HarnessId::ContextStreamCli,
            HarnessId::ContextCode,
        ] {
            assert_eq!(Editor::from_harness_id(harness), None);
        }
    }

    #[test]
    fn test_hook_support_matrix() {
        assert!(Editor::ClaudeCode.has_hooks());
        assert!(Editor::Cursor.has_hooks());
        assert!(Editor::Windsurf.has_hooks());
        assert!(Editor::Cline.has_hooks());
        assert!(Editor::RooCode.has_hooks());
        assert!(!Editor::KiloCode.has_hooks());
        assert!(!Editor::Antigravity.has_hooks());
        assert!(!Editor::Copilot.has_hooks());
        assert!(!Editor::Codex.has_hooks());
    }

    #[test]
    fn test_cursor_all_rules_paths_include_primary_and_alternates() {
        let project = Path::new("/tmp/project");
        let mdc = project
            .join(".cursor")
            .join("rules")
            .join("contextstream.mdc");
        let md = project
            .join(".cursor")
            .join("rules")
            .join("contextstream.md");
        let cursorrules = project.join(".cursorrules");

        // Primary is now the `.mdc` project rule (loaded in Cursor Agent mode).
        assert_eq!(Editor::Cursor.rules_path(Some(project)), Some(mdc.clone()));

        let paths = Editor::Cursor.all_rules_paths(Some(project));
        assert!(paths.contains(&mdc));
        assert!(paths.contains(&md));
        // Legacy `.cursorrules` is no longer a write target.
        assert!(!paths.contains(&cursorrules));

        // But it is still scanned/cleaned so stale blocks get stripped.
        let cleanup_paths = Editor::Cursor.all_rules_cleanup_paths(Some(project));
        assert!(cleanup_paths.contains(&cursorrules));
    }

    #[test]
    fn test_antigravity_all_rules_paths_include_agent_rules_path() {
        let project = Path::new("/tmp/project");
        let paths = Editor::Antigravity.all_rules_paths(Some(project));
        assert!(paths.contains(&project.join("GEMINI.md")));
        assert!(paths.contains(
            &project
                .join(".agent")
                .join("rules")
                .join("contextstream.md")
        ));
    }

    #[test]
    fn test_antigravity_uses_gemini_global_mcp_config_path() {
        let _guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = dirs::home_dir().expect("home dir");
        assert_eq!(
            Editor::Antigravity.mcp_config_path(),
            Some(home.join(".gemini").join("config").join("mcp_config.json"))
        );
        assert_eq!(
            Editor::Antigravity.legacy_mcp_config_paths(),
            vec![home
                .join(".gemini")
                .join("antigravity")
                .join("mcp_config.json")]
        );
    }

    #[test]
    fn test_antigravity_supports_workspace_mcp_config() {
        assert!(Editor::Antigravity.supports_project_mcp_config());
        assert_eq!(
            Editor::Antigravity.project_mcp_config_path(Path::new("/tmp/project")),
            Some(Path::new("/tmp/project/.agents/mcp_config.json").to_path_buf())
        );
    }

    #[test]
    fn claude_code_user_scope_lives_in_claude_json() {
        let _guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = dirs::home_dir().expect("home dir");
        assert_eq!(
            Editor::ClaudeCode.mcp_config_path(),
            Some(home.join(".claude.json"))
        );
        assert_eq!(
            Editor::ClaudeCode.legacy_mcp_config_paths(),
            vec![home.join(".claude").join("mcp.json")]
        );
    }

    #[test]
    fn legacy_mcp_config_paths_never_include_the_primary_path() {
        let _guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for editor in Editor::all() {
            let primary = editor.mcp_config_path();
            for legacy in editor.legacy_mcp_config_paths() {
                assert_ne!(Some(&legacy), primary.as_ref(), "{}", editor.id());
            }
        }
    }

    #[test]
    fn opencode_uses_xdg_config_home_and_reuses_jsonc() {
        let _guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous = std::env::var_os("XDG_CONFIG_HOME");
        std::env::set_var("XDG_CONFIG_HOME", temp.path());

        let dir = temp.path().join("opencode");
        assert_eq!(
            Editor::OpenCode.mcp_config_path(),
            Some(dir.join("opencode.json"))
        );
        assert_eq!(
            Editor::OpenCode.rules_path(None),
            Some(dir.join("AGENTS.md"))
        );
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("opencode.jsonc"), "{}").unwrap();
        assert_eq!(
            Editor::OpenCode.mcp_config_path(),
            Some(dir.join("opencode.jsonc"))
        );
        let home = dirs::home_dir().expect("home dir");
        assert!(Editor::OpenCode
            .legacy_cleanup_only_rules_paths(None)
            .contains(&home.join(".opencode").join("AGENTS.md")));

        match previous {
            Some(value) => std::env::set_var("XDG_CONFIG_HOME", value),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
    }

    #[test]
    fn roo_code_is_deprecated_in_favor_of_cline() {
        assert!(Editor::RooCode.is_deprecated());
        assert_eq!(Editor::RooCode.deprecation_successor(), Some(Editor::Cline));
        assert!(!Editor::selectable().contains(&Editor::RooCode));
        // Existing installs stay reachable for doctor/uninstall.
        assert!(Editor::all().contains(&Editor::RooCode));
        for editor in Editor::selectable() {
            assert!(!editor.is_deprecated());
        }
    }

    #[test]
    fn test_copilot_uses_vscode_user_mcp_json_path() {
        assert_eq!(
            Editor::Copilot.mcp_config_path(),
            vscode_user_dir().map(|dir| dir.join("mcp.json"))
        );
    }

    #[test]
    fn test_copilot_project_rules_path_and_project_mcp_config() {
        let project = Path::new("/tmp/project");
        let rules = Editor::Copilot
            .rules_path(Some(project))
            .expect("copilot project rules path");
        assert_eq!(
            rules,
            project.join(".github").join("copilot-instructions.md")
        );
        assert!(Editor::Copilot.supports_project_mcp_config());
        assert_eq!(
            Editor::Copilot.project_mcp_config_path(project),
            Some(project.join(".vscode").join("mcp.json"))
        );
    }

    #[test]
    fn test_opencode_supports_project_mcp_config() {
        let project = Path::new("/tmp/project");
        assert!(Editor::OpenCode.supports_project_mcp_config());
        assert_eq!(
            Editor::OpenCode.project_mcp_config_path(project),
            Some(project.join("opencode.json"))
        );
    }

    #[test]
    fn test_kilo_global_config_file_reuses_existing_name() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dir = temp.path().to_path_buf();

        // Fresh install → idiomatic default.
        assert_eq!(kilo_global_config_file(dir.clone()), dir.join("kilo.jsonc"));

        // Existing kilo.json must be reused, never shadowed by a new kilo.jsonc.
        std::fs::write(dir.join("kilo.json"), "{}").unwrap();
        assert_eq!(kilo_global_config_file(dir.clone()), dir.join("kilo.json"));

        // kilo.jsonc wins when both exist (first accepted name).
        std::fs::write(dir.join("kilo.jsonc"), "{}").unwrap();
        assert_eq!(kilo_global_config_file(dir.clone()), dir.join("kilo.jsonc"));
    }

    #[test]
    fn test_kilo_project_config_file_reuses_existing_location() {
        let temp = tempfile::tempdir().expect("tempdir");
        let project = temp.path();

        // Fresh project → root kilo.jsonc (docs' idiomatic example).
        assert_eq!(
            kilo_project_config_file(project),
            project.join("kilo.jsonc")
        );

        // An existing .kilo/kilo.json is respected.
        std::fs::create_dir_all(project.join(".kilo")).unwrap();
        std::fs::write(project.join(".kilo").join("kilo.json"), "{}").unwrap();
        assert_eq!(
            kilo_project_config_file(project),
            project.join(".kilo").join("kilo.json")
        );

        // A root config takes precedence over the .kilo/ variant.
        std::fs::write(project.join("kilo.json"), "{}").unwrap();
        assert_eq!(kilo_project_config_file(project), project.join("kilo.json"));
    }

    #[test]
    fn test_windsurf_cleanup_paths_include_legacy_windsurfrules() {
        let project = Path::new("/tmp/project");
        let cleanup_paths = Editor::Windsurf.all_rules_cleanup_paths(Some(project));
        assert!(cleanup_paths.contains(
            &project
                .join(".windsurf")
                .join("rules")
                .join("contextstream.md")
        ));
        assert!(cleanup_paths.contains(&project.join(".windsurfrules")));
    }
}
