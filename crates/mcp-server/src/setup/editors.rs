//! Editor detection and configuration.
//!
//! Supports detecting and configuring:
//! - Claude Code
//! - Cursor
//! - Windsurf
//! - GitHub Copilot (VS Code)
//! - Cline (VS Code extension)
//! - Kilo Code (CLI / VS Code extension)
//! - Roo Code (VS Code extension)
//! - OpenAI Codex CLI
//! - Aider
//! - Antigravity
//! - OpenCode CLI

use std::path::{Path, PathBuf};

use mcp_types::{HarnessId, HarnessProfile};

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
            | HarnessId::ContextStreamCli => None,
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

    /// Get the MCP config file path for this editor.
    pub fn mcp_config_path(&self) -> Option<PathBuf> {
        match self {
            Editor::ClaudeCode => dirs::home_dir().map(|h| h.join(".claude").join("mcp.json")),
            Editor::Cursor => dirs::home_dir().map(|h| h.join(".cursor").join("mcp.json")),
            Editor::Windsurf => dirs::home_dir()
                .map(|h| h.join(".codeium").join("windsurf").join("mcp_config.json")),
            Editor::Copilot => {
                // VS Code user-level mcp.json (same directory as settings.json).
                vscode_user_mcp_json_path()
            }
            Editor::Cline => {
                // Cline uses VS Code settings
                vscode_settings_path()
            }
            Editor::KiloCode => {
                // Kilo accepts kilo.jsonc, kilo.json, and config.json in
                // ~/.config/kilo/ (docs: kilo.ai/docs/automate/mcp/using-in-cli).
                // Reuse whichever exists so we never create a second,
                // conflicting config file.
                kilo_config_dir().map(kilo_global_config_file)
            }
            Editor::RooCode => {
                // Roo uses VS Code settings
                vscode_settings_path()
            }
            Editor::Codex => {
                // Codex uses a TOML config
                dirs::home_dir().map(|h| h.join(".codex").join("config.toml"))
            }
            Editor::Aider => {
                // Aider uses a YAML config
                dirs::home_dir().map(|h| h.join(".aider.conf.yml"))
            }
            Editor::Antigravity => dirs::home_dir().map(|h| {
                h.join(".gemini")
                    .join("antigravity")
                    .join("mcp_config.json")
            }),
            Editor::OpenCode => {
                // OpenCode uses a JSON config
                dirs::home_dir().map(|h| h.join(".opencode").join("mcp.json"))
            }
        }
    }

    /// Get the rules file path for this editor.
    pub fn rules_path(&self, project_path: Option<&std::path::Path>) -> Option<PathBuf> {
        match self {
            Editor::ClaudeCode => {
                if let Some(project) = project_path {
                    Some(project.join("CLAUDE.md"))
                } else {
                    dirs::home_dir().map(|h| h.join(".claude").join("CLAUDE.md"))
                }
            }
            // Modern Cursor loads `.cursor/rules/*.mdc` in every mode (Chat,
            // Composer, Agent). The legacy `.cursorrules` file is NOT read in
            // Agent mode at all, so it can no longer be the primary target.
            Editor::Cursor => project_path.map(|project| {
                project
                    .join(".cursor")
                    .join("rules")
                    .join("contextstream.mdc")
            }),
            Editor::Copilot => {
                project_path.map(|project| project.join(".github").join("copilot-instructions.md"))
            }
            Editor::Windsurf => {
                if let Some(project) = project_path {
                    Some(
                        project
                            .join(".windsurf")
                            .join("rules")
                            .join("contextstream.md"),
                    )
                } else {
                    dirs::home_dir().map(|h| {
                        h.join(".codeium")
                            .join("windsurf")
                            .join("memories")
                            .join("global_rules.md")
                    })
                }
            }
            Editor::Cline => {
                if let Some(project) = project_path {
                    Some(project.join(".clinerules"))
                } else {
                    dirs::home_dir().map(|h| {
                        h.join("Documents")
                            .join("Cline")
                            .join("Rules")
                            .join("contextstream.md")
                    })
                }
            }
            Editor::KiloCode => {
                if let Some(project) = project_path {
                    Some(project.join(".kilo").join("rules").join("contextstream.md"))
                } else {
                    kilo_config_dir().map(|d| d.join("rules").join("contextstream.md"))
                }
            }
            Editor::RooCode => {
                if let Some(project) = project_path {
                    Some(project.join(".roo").join("rules").join("contextstream.md"))
                } else {
                    dirs::home_dir().map(|h| h.join(".roo").join("rules").join("contextstream.md"))
                }
            }
            Editor::Codex => {
                if let Some(project) = project_path {
                    Some(project.join("AGENTS.md"))
                } else {
                    dirs::home_dir().map(|h| h.join(".codex").join("AGENTS.md"))
                }
            }
            Editor::Aider => {
                if let Some(project) = project_path {
                    Some(project.join(".aider.conf.yml"))
                } else {
                    dirs::home_dir().map(|h| h.join(".aider.conf.yml"))
                }
            }
            Editor::Antigravity => {
                if let Some(project) = project_path {
                    Some(project.join("GEMINI.md"))
                } else {
                    dirs::home_dir().map(|h| h.join(".gemini").join("GEMINI.md"))
                }
            }
            Editor::OpenCode => {
                if let Some(project) = project_path {
                    Some(project.join("AGENTS.md"))
                } else {
                    dirs::home_dir().map(|h| h.join(".opencode").join("AGENTS.md"))
                }
            }
        }
    }

    /// Additional legacy/alternate rules paths to check for migration/update.
    ///
    /// These paths are read/update candidates only. The primary managed location
    /// remains `rules_path(...)`.
    pub fn legacy_rules_paths(&self, project_path: Option<&Path>) -> Vec<PathBuf> {
        match self {
            Editor::ClaudeCode => project_path
                .map(|p| vec![p.join(".claude").join("CLAUDE.md")])
                .unwrap_or_default(),
            // `.cursor/rules/contextstream.mdc` is now the primary target
            // (see `rules_path`). Keep the older `.md` variant as a migration
            // read/update target. `.cursorrules` is handled as cleanup-only.
            Editor::Cursor => project_path
                .map(|p| vec![p.join(".cursor").join("rules").join("contextstream.md")])
                .unwrap_or_default(),
            Editor::Cline => {
                if let Some(project) = project_path {
                    vec![project.join(".clinerules").join("contextstream.md")]
                } else {
                    dirs::home_dir()
                        .map(|h| vec![h.join("Cline").join("Rules").join("contextstream.md")])
                        .unwrap_or_default()
                }
            }
            Editor::KiloCode => {
                let mut paths = Vec::new();
                if let Some(p) = project_path {
                    // Legacy .kilocode/ paths for migration
                    paths.push(p.join(".kilocode").join("rules").join("contextstream.md"));
                    paths.push(p.join(".kilocoderules"));
                    paths.push(p.join(".roorules"));
                    paths.push(p.join(".clinerules"));
                } else if let Some(home) = dirs::home_dir() {
                    // Legacy global path
                    paths.push(
                        home.join(".kilocode")
                            .join("rules")
                            .join("contextstream.md"),
                    );
                }
                paths
            }
            Editor::RooCode => project_path
                .map(|p| vec![p.join(".roorules"), p.join(".clinerules")])
                .unwrap_or_default(),
            Editor::Codex => {
                if let Some(project) = project_path {
                    vec![project.join("AGENTS.override.md")]
                } else {
                    dirs::home_dir()
                        .map(|h| vec![h.join(".codex").join("AGENTS.override.md")])
                        .unwrap_or_default()
                }
            }
            Editor::Antigravity => project_path
                .map(|p| vec![p.join(".agent").join("rules").join("contextstream.md")])
                .unwrap_or_default(),
            Editor::OpenCode => {
                if let Some(project) = project_path {
                    vec![project.join("AGENTS.override.md")]
                } else {
                    dirs::home_dir()
                        .map(|h| vec![h.join(".opencode").join("AGENTS.override.md")])
                        .unwrap_or_default()
                }
            }
            Editor::Windsurf | Editor::Copilot | Editor::Aider => Vec::new(),
        }
    }

    /// Legacy rules paths that are cleanup-only.
    ///
    /// These locations are scanned/cleaned for stale ContextStream blocks, but
    /// are never write targets for new managed rules.
    pub fn legacy_cleanup_only_rules_paths(&self, project_path: Option<&Path>) -> Vec<PathBuf> {
        match self {
            Editor::Windsurf => project_path
                .map(|p| vec![p.join(".windsurfrules")])
                .unwrap_or_default(),
            // Legacy Cursor single-file rules. Cursor Agent mode ignores it, so
            // we strip any stale ContextStream block but never recreate it.
            Editor::Cursor => project_path
                .map(|p| vec![p.join(".cursorrules")])
                .unwrap_or_default(),
            _ => Vec::new(),
        }
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
    pub fn project_mcp_config_path(&self, project_path: &std::path::Path) -> Option<PathBuf> {
        match self {
            Editor::ClaudeCode => Some(project_path.join(".mcp.json")),
            Editor::Cursor => Some(project_path.join(".cursor").join("mcp.json")),
            Editor::Windsurf => None, // Windsurf uses global mcp_config.json
            Editor::Copilot => Some(project_path.join(".vscode").join("mcp.json")),
            Editor::Cline => None, // Cline MCP is via extension UI
            Editor::KiloCode => Some(kilo_project_config_file(project_path)),
            Editor::RooCode => Some(project_path.join(".roo").join("mcp.json")),
            Editor::Codex => None, // Codex only supports global config
            Editor::Aider => None, // Aider doesn't use MCP
            Editor::OpenCode => Some(project_path.join("opencode.json")),
            Editor::Antigravity => None,
        }
    }

    /// Check if this editor supports project-level MCP config.
    pub fn supports_project_mcp_config(&self) -> bool {
        matches!(
            self,
            Editor::ClaudeCode
                | Editor::Cursor
                | Editor::Copilot
                | Editor::KiloCode
                | Editor::RooCode
                | Editor::OpenCode
        )
    }

    /// Check if this editor uses JSON MCP config.
    pub fn uses_json_config(&self) -> bool {
        matches!(
            self,
            Editor::ClaudeCode
                | Editor::Cursor
                | Editor::Windsurf
                | Editor::Copilot
                | Editor::Cline
                | Editor::KiloCode
                | Editor::RooCode
                | Editor::Antigravity
        )
    }

    /// Check if this editor uses VS Code extensions settings.
    pub fn uses_vscode_settings(&self) -> bool {
        matches!(self, Editor::Cline | Editor::RooCode)
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
        match self {
            Editor::ClaudeCode => {
                "Exit the current Claude Code session, then start a new session in the intended checkout."
            }
            Editor::Cursor => {
                "In Cursor, run “Developer: Reload Window” (or fully quit and reopen), then open the intended checkout."
            }
            Editor::Windsurf => {
                "In Windsurf, reload the window (or fully quit and reopen), then open the intended checkout."
            }
            Editor::Copilot => {
                "Reload the VS Code window, or start a fresh GitHub Copilot CLI session, in the intended checkout."
            }
            Editor::Cline => {
                "In Cline’s VS Code window, run “Developer: Reload Window”, then open the intended checkout."
            }
            Editor::KiloCode => {
                "Start a fresh Kilo Code CLI session, or reload its VS Code window, in the intended checkout."
            }
            Editor::RooCode => {
                "In Roo Code’s VS Code window, run “Developer: Reload Window”, then open the intended checkout."
            }
            Editor::Codex => {
                "Exit the current Codex session, then start a new Codex session in the intended checkout."
            }
            Editor::Aider => {
                "Start a fresh Aider session in the intended checkout so its managed rules reload; Aider is rules-only and has no MCP handshake."
            }
            Editor::Antigravity => {
                "Fully quit and reopen Antigravity, then open the intended checkout."
            }
            Editor::OpenCode => {
                "Exit the current OpenCode session, then start a new session in the intended checkout."
            }
        }
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

/// Detect all installed editors.
pub fn detect_installed_editors() -> Vec<Editor> {
    Editor::all()
        .iter()
        .copied()
        .filter(|editor| is_editor_installed(*editor))
        .collect()
}

fn is_editor_installed(editor: Editor) -> bool {
    match editor {
        Editor::ClaudeCode => is_claude_code_installed(),
        Editor::Cursor => is_cursor_installed(),
        Editor::Windsurf => is_windsurf_installed(),
        Editor::Copilot => is_copilot_installed(),
        Editor::Cline => is_cline_installed(),
        Editor::KiloCode => is_kilo_installed(),
        Editor::RooCode => is_roo_installed(),
        Editor::Codex => is_codex_installed(),
        Editor::Aider => is_aider_installed(),
        Editor::Antigravity => is_antigravity_installed(),
        Editor::OpenCode => is_opencode_installed(),
    }
}

/// Check if Claude Code is installed.
pub fn is_claude_code_installed() -> bool {
    // Check for claude command
    if which::which("claude").is_ok() {
        return true;
    }

    // A bare ~/.claude directory is not evidence of an install — plenty of
    // tools create it. Require state only Claude Code itself writes, so we
    // never configure an editor the user does not actually run.
    if let Some(home) = dirs::home_dir() {
        let dir = home.join(".claude");
        return dir.join("settings.json").exists()
            || dir.join("settings.local.json").exists()
            || dir.join(".credentials.json").exists()
            || dir.join("projects").is_dir();
    }

    false
}

/// Check if Cursor is installed.
pub fn is_cursor_installed() -> bool {
    // Check for cursor command
    if which::which("cursor").is_ok() {
        return true;
    }

    // Check platform-specific paths
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
        // Check common Linux paths
        let paths = ["/usr/bin/cursor", "/usr/local/bin/cursor"];
        for path in &paths {
            if std::path::Path::new(path).exists() {
                return true;
            }
        }

        // Check for AppImage in home directory
        if let Some(home) = dirs::home_dir() {
            if home.join("Applications").join("cursor.AppImage").exists() {
                return true;
            }
        }
    }

    // Check for config directory
    if let Some(home) = dirs::home_dir() {
        if home.join(".cursor").exists() {
            return true;
        }
    }

    false
}

/// Check if Windsurf is installed.
pub fn is_windsurf_installed() -> bool {
    if which::which("windsurf").is_ok() {
        return true;
    }

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

    if let Some(home) = dirs::home_dir() {
        if home.join(".codeium").join("windsurf").exists() {
            return true;
        }
    }

    false
}

/// Check if GitHub Copilot (extension or CLI) is installed.
pub fn is_copilot_installed() -> bool {
    // Copilot CLI
    if which::which("copilot").is_ok() {
        return true;
    }

    // VS Code extensions
    if is_vscode_extension_installed("github.copilot")
        || is_vscode_extension_installed("github.copilot-chat")
    {
        return true;
    }

    // Copilot CLI config folder
    if let Some(home) = dirs::home_dir() {
        if home.join(".copilot").exists() {
            return true;
        }
    }

    false
}

/// Check if Cline VS Code extension is installed.
pub fn is_cline_installed() -> bool {
    is_vscode_extension_installed("saoudrizwan.claude-dev")
}

/// Check if Kilo Code is installed (CLI or VS Code extension).
pub fn is_kilo_installed() -> bool {
    // Check for kilo CLI command
    if which::which("kilo").is_ok() {
        return true;
    }

    // Check for Kilo CLI config directory
    if kilo_config_dir().is_some_and(|d| d.exists()) {
        return true;
    }

    // Legacy: VS Code extension
    if is_vscode_extension_installed("kilocode.kilo-code") {
        return true;
    }

    false
}

/// Check if Roo Code VS Code extension is installed.
pub fn is_roo_installed() -> bool {
    is_vscode_extension_installed("rooveterinaryinc.roo-cline")
}

/// Check if a VS Code extension is installed.
fn is_vscode_extension_installed(extension_id: &str) -> bool {
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

/// Check if OpenAI Codex CLI is installed.
pub fn is_codex_installed() -> bool {
    which::which("codex").is_ok()
}

/// Check if Aider is installed.
pub fn is_aider_installed() -> bool {
    which::which("aider").is_ok()
}

/// Check if Antigravity is installed.
pub fn is_antigravity_installed() -> bool {
    if which::which("antigravity").is_ok() {
        return true;
    }

    if let Some(home) = dirs::home_dir() {
        if home.join(".gemini").join("antigravity").exists() {
            return true;
        }
    }

    false
}

/// Check if OpenCode CLI is installed.
pub fn is_opencode_installed() -> bool {
    if which::which("opencode").is_ok() {
        return true;
    }

    if let Some(home) = dirs::home_dir() {
        if home.join(".opencode").exists() {
            return true;
        }
    }

    false
}

/// Get Kilo CLI config directory (~/.config/kilo/).
pub fn kilo_config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|c| c.join("kilo"))
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

/// Get VS Code settings path.
fn vscode_settings_path() -> Option<PathBuf> {
    vscode_user_dir().map(|d| d.join("settings.json"))
}

/// VS Code user-level mcp.json path (per VS Code profile folder).
fn vscode_user_mcp_json_path() -> Option<PathBuf> {
    vscode_user_dir().map(|d| d.join("mcp.json"))
}

/// VS Code User directory (platform-specific).
fn vscode_user_dir() -> Option<PathBuf> {
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
        let home = dirs::home_dir().expect("home dir");
        assert_eq!(
            Editor::Antigravity.mcp_config_path(),
            Some(
                home.join(".gemini")
                    .join("antigravity")
                    .join("mcp_config.json")
            )
        );
    }

    #[test]
    fn test_antigravity_does_not_support_project_mcp_config() {
        assert!(!Editor::Antigravity.supports_project_mcp_config());
        assert_eq!(
            Editor::Antigravity.project_mcp_config_path(Path::new("/tmp/project")),
            None
        );
    }

    #[test]
    fn test_copilot_uses_vscode_user_mcp_json_path() {
        assert_eq!(
            Editor::Copilot.mcp_config_path(),
            vscode_user_mcp_json_path()
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
