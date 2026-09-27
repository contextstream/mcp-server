//! Declarative registry of the coding clients setup can configure.
//!
//! One [`ClientDescriptor`] per [`Editor`] holds everything that is a matter
//! of file layout: where the client reads its MCP config and rules (primary,
//! legacy, and cleanup-only locations), which config dialect and remote
//! entry shape it expects, what evidence shows it is installed, and what to
//! tell the user after setup changes it. Capability semantics (hooks,
//! enforcement, teaching evidence) stay in [`mcp_types::HarnessProfile`].
//!
//! A client that reads a standard JSON server map needs only a new `Editor`
//! variant and one descriptor; bespoke formats (Codex TOML, Kilo, OpenCode,
//! VS Code settings) are selected through [`ConfigDialect`].

use std::path::{Path, PathBuf};

use super::editors::{
    claude_user_config_path, cursor_platform_install_present, kilo_config_dir,
    kilo_global_config_file, kilo_project_config_file, opencode_config_dir,
    opencode_global_config_file, vscode_extension_installed, vscode_user_dir,
    windsurf_platform_install_present, xdg_config_home, Editor,
};

/// Tool surface VS Code Copilot is configured with by default.
pub(crate) const COPILOT_TOOL_SURFACE_PROFILE: &str = "openai_agentic";

/// Directory a [`PathSpec`] is relative to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Base {
    /// The user's home directory.
    Home,
    /// `$XDG_CONFIG_HOME`, else `~/.config`, on every OS.
    XdgConfig,
    /// The VS Code `User` directory for the current platform.
    VsCodeUser,
}

/// A user-level location.
#[derive(Debug, Clone, Copy)]
pub enum PathSpec {
    At(Base, &'static [&'static str]),
    /// Locations that depend on what already exists on disk.
    Resolve {
        resolve: fn() -> Option<PathBuf>,
        display: &'static str,
    },
}

impl PathSpec {
    pub fn resolve(&self) -> Option<PathBuf> {
        match self {
            PathSpec::At(base, parts) => {
                let root = match base {
                    Base::Home => dirs::home_dir(),
                    Base::XdgConfig => xdg_config_home(),
                    Base::VsCodeUser => vscode_user_dir(),
                }?;
                Some(join_all(root, parts))
            }
            PathSpec::Resolve { resolve, .. } => resolve(),
        }
    }

    /// Platform-neutral description for docs and the `clients` catalog.
    pub fn display(&self) -> String {
        match self {
            PathSpec::At(base, parts) => {
                let prefix = match base {
                    Base::Home => "~",
                    Base::XdgConfig => "~/.config",
                    Base::VsCodeUser => "<VS Code user dir>",
                };
                format!("{prefix}/{}", parts.join("/"))
            }
            PathSpec::Resolve { display, .. } => (*display).to_string(),
        }
    }
}

/// A location inside a project checkout.
#[derive(Debug, Clone, Copy)]
pub enum ProjectPathSpec {
    At(&'static [&'static str]),
    Resolve {
        resolve: fn(&Path) -> PathBuf,
        display: &'static str,
    },
}

impl ProjectPathSpec {
    pub fn resolve(&self, project: &Path) -> PathBuf {
        match self {
            ProjectPathSpec::At(parts) => join_all(project.to_path_buf(), parts),
            ProjectPathSpec::Resolve { resolve, .. } => resolve(project),
        }
    }

    pub fn display(&self) -> String {
        match self {
            ProjectPathSpec::At(parts) => parts.join("/"),
            ProjectPathSpec::Resolve { display, .. } => (*display).to_string(),
        }
    }
}

fn join_all(mut root: PathBuf, parts: &[&str]) -> PathBuf {
    for part in parts {
        root.push(part);
    }
    root
}

/// How a client stores its MCP server entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigDialect {
    /// A top-level JSON object keyed by server name (`mcpServers`, or
    /// `servers` for VS Code).
    JsonServers { root_key: &'static str },
    /// A key inside VS Code's JSONC `settings.json`.
    VsCodeSettings { key: &'static str },
    /// Kilo CLI `kilo.jsonc`: `mcp` map of `local`/`remote` entries.
    Kilo,
    /// OpenCode `opencode.json`: `mcp` map of `local`/`remote` entries.
    OpenCode,
    /// Codex `config.toml` `[mcp_servers.contextstream]`.
    CodexToml,
    /// No MCP config surface; the client is configured through rules only.
    RulesOnly,
}

impl ConfigDialect {
    /// Key of the server map, for dialects that have one.
    pub fn root_key(&self) -> Option<&'static str> {
        match self {
            ConfigDialect::JsonServers { root_key } => Some(root_key),
            ConfigDialect::VsCodeSettings { key } => Some(key),
            ConfigDialect::Kilo | ConfigDialect::OpenCode => Some("mcp"),
            ConfigDialect::CodexToml | ConfigDialect::RulesOnly => None,
        }
    }

    /// Short format name used by `generate-configs`.
    pub fn format_name(&self) -> &'static str {
        match self {
            ConfigDialect::CodexToml => "toml",
            ConfigDialect::RulesOnly => "yaml",
            ConfigDialect::VsCodeSettings { .. } => "vscode_settings",
            _ => "json",
        }
    }
}

/// Shape of a hosted (Streamable HTTP) server entry in a JSON server map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteShape {
    /// `{ "type": "http", "url": ..., "headers": ... }`
    TypeHttpUrl,
    /// `{ "serverUrl": ..., "headers": ... }` (Antigravity rejects `url`).
    ServerUrl,
}

/// Project-level MCP config file.
#[derive(Debug, Clone, Copy)]
pub struct ProjectConfig {
    pub path: ProjectPathSpec,
    pub root_key: &'static str,
}

/// Where managed rules live.
#[derive(Debug, Clone, Copy)]
pub struct RulesLayout {
    pub project: Option<ProjectPathSpec>,
    pub global: Option<PathSpec>,
    /// Read/update migration targets (see `Editor::legacy_rules_paths`).
    pub legacy_project: &'static [ProjectPathSpec],
    pub legacy_global: &'static [PathSpec],
    /// Scanned for stale blocks but never written.
    pub cleanup_project: &'static [ProjectPathSpec],
    pub cleanup_global: &'static [PathSpec],
}

/// One piece of evidence that a client is installed; any match detects it.
#[derive(Debug, Clone, Copy)]
pub enum Evidence {
    /// An executable on `PATH`.
    Binary(&'static str),
    /// A file or directory under HOME exists.
    HomePath(&'static [&'static str]),
    /// A directory under HOME exists.
    HomeDir(&'static [&'static str]),
    /// A file or directory under the XDG config home exists.
    XdgConfigPath(&'static [&'static str]),
    /// A VS Code extension id prefix is installed.
    VsCodeExtension(&'static str),
    /// Platform-specific probing (app bundles, system install paths).
    Check(fn() -> bool),
}

impl Evidence {
    pub fn present(&self) -> bool {
        match self {
            Evidence::Binary(name) => which::which(name).is_ok(),
            Evidence::HomePath(parts) => {
                dirs::home_dir().is_some_and(|home| join_all(home, parts).exists())
            }
            Evidence::HomeDir(parts) => {
                dirs::home_dir().is_some_and(|home| join_all(home, parts).is_dir())
            }
            Evidence::XdgConfigPath(parts) => {
                xdg_config_home().is_some_and(|root| join_all(root, parts).exists())
            }
            Evidence::VsCodeExtension(id) => vscode_extension_installed(id),
            Evidence::Check(check) => check(),
        }
    }
}

/// Whether setup should offer the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientStatus {
    Supported,
    /// Discontinued upstream; existing installs stay maintainable.
    Discontinued {
        successor: Editor,
    },
}

/// Everything setup needs to know about one client's file layout.
#[derive(Debug, Clone, Copy)]
pub struct ClientDescriptor {
    pub editor: Editor,
    pub global_config: Option<PathSpec>,
    /// Global locations earlier releases wrote; cleanup-only.
    pub legacy_global_configs: &'static [PathSpec],
    pub project_config: Option<ProjectConfig>,
    pub dialect: ConfigDialect,
    pub remote_shape: RemoteShape,
    pub tool_surface_profile: Option<&'static str>,
    pub rules: RulesLayout,
    pub detect: &'static [Evidence],
    pub reload_instruction: &'static str,
    pub status: ClientStatus,
}

const NO_RULES_LEGACY: RulesLayout = RulesLayout {
    project: None,
    global: None,
    legacy_project: &[],
    legacy_global: &[],
    cleanup_project: &[],
    cleanup_global: &[],
};

static CLAUDE_CODE: ClientDescriptor = ClientDescriptor {
    editor: Editor::ClaudeCode,
    // User-scoped servers live in the top-level `mcpServers` of
    // `~/.claude.json` (code.claude.com/docs/en/mcp).
    global_config: Some(PathSpec::Resolve {
        resolve: claude_user_config_path,
        display: "~/.claude.json",
    }),
    legacy_global_configs: &[PathSpec::At(Base::Home, &[".claude", "mcp.json"])],
    project_config: Some(ProjectConfig {
        path: ProjectPathSpec::At(&[".mcp.json"]),
        root_key: "mcpServers",
    }),
    dialect: ConfigDialect::JsonServers {
        root_key: "mcpServers",
    },
    remote_shape: RemoteShape::TypeHttpUrl,
    tool_surface_profile: None,
    rules: RulesLayout {
        project: Some(ProjectPathSpec::At(&["CLAUDE.md"])),
        global: Some(PathSpec::At(Base::Home, &[".claude", "CLAUDE.md"])),
        legacy_project: &[ProjectPathSpec::At(&[".claude", "CLAUDE.md"])],
        ..NO_RULES_LEGACY
    },
    // A bare ~/.claude is not evidence; plenty of tools create it. Require
    // state only Claude Code itself writes.
    detect: &[
        Evidence::Binary("claude"),
        Evidence::HomePath(&[".claude", "settings.json"]),
        Evidence::HomePath(&[".claude", "settings.local.json"]),
        Evidence::HomePath(&[".claude", ".credentials.json"]),
        Evidence::HomeDir(&[".claude", "projects"]),
    ],
    reload_instruction:
        "Exit the current Claude Code session, then start a new session in the intended checkout.",
    status: ClientStatus::Supported,
};

static CURSOR: ClientDescriptor = ClientDescriptor {
    editor: Editor::Cursor,
    global_config: Some(PathSpec::At(Base::Home, &[".cursor", "mcp.json"])),
    legacy_global_configs: &[],
    project_config: Some(ProjectConfig {
        path: ProjectPathSpec::At(&[".cursor", "mcp.json"]),
        root_key: "mcpServers",
    }),
    dialect: ConfigDialect::JsonServers {
        root_key: "mcpServers",
    },
    remote_shape: RemoteShape::TypeHttpUrl,
    tool_surface_profile: None,
    rules: RulesLayout {
        // Cursor loads `.cursor/rules/*.mdc` in every mode; the legacy
        // `.cursorrules` is ignored in Agent mode, so it is cleanup-only.
        project: Some(ProjectPathSpec::At(&[".cursor", "rules", "contextstream.mdc"])),
        legacy_project: &[ProjectPathSpec::At(&[".cursor", "rules", "contextstream.md"])],
        cleanup_project: &[ProjectPathSpec::At(&[".cursorrules"])],
        ..NO_RULES_LEGACY
    },
    detect: &[
        Evidence::Binary("cursor"),
        Evidence::Check(cursor_platform_install_present),
        Evidence::HomePath(&[".cursor"]),
    ],
    reload_instruction:
        "In Cursor, run “Developer: Reload Window” (or fully quit and reopen), then open the intended checkout.",
    status: ClientStatus::Supported,
};

static WINDSURF: ClientDescriptor = ClientDescriptor {
    editor: Editor::Windsurf,
    global_config: Some(PathSpec::At(
        Base::Home,
        &[".codeium", "windsurf", "mcp_config.json"],
    )),
    legacy_global_configs: &[],
    project_config: None,
    dialect: ConfigDialect::JsonServers {
        root_key: "mcpServers",
    },
    remote_shape: RemoteShape::TypeHttpUrl,
    tool_surface_profile: None,
    rules: RulesLayout {
        project: Some(ProjectPathSpec::At(&[".windsurf", "rules", "contextstream.md"])),
        global: Some(PathSpec::At(
            Base::Home,
            &[".codeium", "windsurf", "memories", "global_rules.md"],
        )),
        cleanup_project: &[ProjectPathSpec::At(&[".windsurfrules"])],
        ..NO_RULES_LEGACY
    },
    detect: &[
        Evidence::Binary("windsurf"),
        Evidence::Check(windsurf_platform_install_present),
        Evidence::HomePath(&[".codeium", "windsurf"]),
    ],
    reload_instruction:
        "In Windsurf, reload the window (or fully quit and reopen), then open the intended checkout.",
    status: ClientStatus::Supported,
};

static COPILOT: ClientDescriptor = ClientDescriptor {
    editor: Editor::Copilot,
    global_config: Some(PathSpec::At(Base::VsCodeUser, &["mcp.json"])),
    legacy_global_configs: &[],
    project_config: Some(ProjectConfig {
        path: ProjectPathSpec::At(&[".vscode", "mcp.json"]),
        root_key: "servers",
    }),
    dialect: ConfigDialect::JsonServers {
        root_key: "servers",
    },
    remote_shape: RemoteShape::TypeHttpUrl,
    tool_surface_profile: Some(COPILOT_TOOL_SURFACE_PROFILE),
    rules: RulesLayout {
        project: Some(ProjectPathSpec::At(&[".github", "copilot-instructions.md"])),
        ..NO_RULES_LEGACY
    },
    detect: &[
        Evidence::Binary("copilot"),
        Evidence::VsCodeExtension("github.copilot"),
        Evidence::VsCodeExtension("github.copilot-chat"),
        Evidence::HomePath(&[".copilot"]),
    ],
    reload_instruction:
        "Reload the VS Code window, or start a fresh GitHub Copilot CLI session, in the intended checkout.",
    status: ClientStatus::Supported,
};

static CLINE: ClientDescriptor = ClientDescriptor {
    editor: Editor::Cline,
    global_config: Some(PathSpec::At(Base::VsCodeUser, &["settings.json"])),
    legacy_global_configs: &[],
    project_config: None,
    dialect: ConfigDialect::VsCodeSettings {
        key: "cline.mcpServers",
    },
    remote_shape: RemoteShape::TypeHttpUrl,
    tool_surface_profile: None,
    rules: RulesLayout {
        project: Some(ProjectPathSpec::At(&[".clinerules"])),
        global: Some(PathSpec::At(
            Base::Home,
            &["Documents", "Cline", "Rules", "contextstream.md"],
        )),
        legacy_project: &[ProjectPathSpec::At(&[".clinerules", "contextstream.md"])],
        legacy_global: &[PathSpec::At(
            Base::Home,
            &["Cline", "Rules", "contextstream.md"],
        )],
        ..NO_RULES_LEGACY
    },
    detect: &[Evidence::VsCodeExtension("saoudrizwan.claude-dev")],
    reload_instruction:
        "In Cline’s VS Code window, run “Developer: Reload Window”, then open the intended checkout.",
    status: ClientStatus::Supported,
};

fn kilo_global_config() -> Option<PathBuf> {
    kilo_config_dir().map(kilo_global_config_file)
}

/// Earlier releases used the platform config dir, which differs from
/// `~/.config` on macOS and Windows.
fn kilo_platform_config() -> Option<PathBuf> {
    dirs::config_dir().map(|dir| kilo_global_config_file(dir.join("kilo")))
}

static KILO_CODE: ClientDescriptor = ClientDescriptor {
    editor: Editor::KiloCode,
    // Kilo accepts kilo.jsonc, kilo.json, and config.json in ~/.config/kilo/
    // (kilo.ai/docs/automate/mcp/using-in-cli); reuse whichever exists.
    global_config: Some(PathSpec::Resolve {
        resolve: kilo_global_config,
        display: "~/.config/kilo/kilo.jsonc",
    }),
    legacy_global_configs: &[PathSpec::Resolve {
        resolve: kilo_platform_config,
        display: "<platform config dir>/kilo/kilo.jsonc",
    }],
    project_config: Some(ProjectConfig {
        path: ProjectPathSpec::Resolve {
            resolve: kilo_project_config_file,
            display: "kilo.jsonc",
        },
        root_key: "mcp",
    }),
    dialect: ConfigDialect::Kilo,
    remote_shape: RemoteShape::TypeHttpUrl,
    tool_surface_profile: None,
    rules: RulesLayout {
        project: Some(ProjectPathSpec::At(&[".kilo", "rules", "contextstream.md"])),
        global: Some(PathSpec::At(
            Base::XdgConfig,
            &["kilo", "rules", "contextstream.md"],
        )),
        legacy_project: &[
            ProjectPathSpec::At(&[".kilocode", "rules", "contextstream.md"]),
            ProjectPathSpec::At(&[".kilocoderules"]),
            ProjectPathSpec::At(&[".roorules"]),
            ProjectPathSpec::At(&[".clinerules"]),
        ],
        legacy_global: &[PathSpec::At(
            Base::Home,
            &[".kilocode", "rules", "contextstream.md"],
        )],
        ..NO_RULES_LEGACY
    },
    detect: &[
        Evidence::Binary("kilo"),
        Evidence::XdgConfigPath(&["kilo"]),
        Evidence::VsCodeExtension("kilocode.kilo-code"),
    ],
    reload_instruction:
        "Start a fresh Kilo Code CLI session, or reload its VS Code window, in the intended checkout.",
    status: ClientStatus::Supported,
};

static ROO_CODE: ClientDescriptor = ClientDescriptor {
    editor: Editor::RooCode,
    global_config: Some(PathSpec::At(Base::VsCodeUser, &["settings.json"])),
    legacy_global_configs: &[],
    project_config: Some(ProjectConfig {
        path: ProjectPathSpec::At(&[".roo", "mcp.json"]),
        root_key: "mcpServers",
    }),
    dialect: ConfigDialect::VsCodeSettings {
        key: "roo-cline.mcpServers",
    },
    remote_shape: RemoteShape::TypeHttpUrl,
    tool_surface_profile: None,
    rules: RulesLayout {
        project: Some(ProjectPathSpec::At(&[".roo", "rules", "contextstream.md"])),
        global: Some(PathSpec::At(
            Base::Home,
            &[".roo", "rules", "contextstream.md"],
        )),
        legacy_project: &[
            ProjectPathSpec::At(&[".roorules"]),
            ProjectPathSpec::At(&[".clinerules"]),
        ],
        ..NO_RULES_LEGACY
    },
    detect: &[Evidence::VsCodeExtension("rooveterinaryinc.roo-cline")],
    reload_instruction:
        "In Roo Code’s VS Code window, run “Developer: Reload Window”, then open the intended checkout.",
    // Roo Code shut down on 2026-05-15 and pointed users to Cline
    // (docs.roocode.com/sunset).
    status: ClientStatus::Discontinued {
        successor: Editor::Cline,
    },
};

static CODEX: ClientDescriptor = ClientDescriptor {
    editor: Editor::Codex,
    global_config: Some(PathSpec::At(Base::Home, &[".codex", "config.toml"])),
    legacy_global_configs: &[],
    project_config: None,
    dialect: ConfigDialect::CodexToml,
    remote_shape: RemoteShape::TypeHttpUrl,
    tool_surface_profile: None,
    rules: RulesLayout {
        project: Some(ProjectPathSpec::At(&["AGENTS.md"])),
        global: Some(PathSpec::At(Base::Home, &[".codex", "AGENTS.md"])),
        legacy_project: &[ProjectPathSpec::At(&["AGENTS.override.md"])],
        legacy_global: &[PathSpec::At(Base::Home, &[".codex", "AGENTS.override.md"])],
        ..NO_RULES_LEGACY
    },
    detect: &[Evidence::Binary("codex")],
    reload_instruction:
        "Exit the current Codex session, then start a new Codex session in the intended checkout.",
    status: ClientStatus::Supported,
};

static AIDER: ClientDescriptor = ClientDescriptor {
    editor: Editor::Aider,
    // Aider's own YAML config; it has no MCP surface and is never written.
    global_config: Some(PathSpec::At(Base::Home, &[".aider.conf.yml"])),
    legacy_global_configs: &[],
    project_config: None,
    dialect: ConfigDialect::RulesOnly,
    remote_shape: RemoteShape::TypeHttpUrl,
    tool_surface_profile: None,
    rules: RulesLayout {
        project: Some(ProjectPathSpec::At(&[".aider.conf.yml"])),
        global: Some(PathSpec::At(Base::Home, &[".aider.conf.yml"])),
        ..NO_RULES_LEGACY
    },
    detect: &[Evidence::Binary("aider")],
    reload_instruction:
        "Start a fresh Aider session in the intended checkout so its managed rules reload; Aider is rules-only and has no MCP handshake.",
    status: ClientStatus::Supported,
};

static ANTIGRAVITY: ClientDescriptor = ClientDescriptor {
    editor: Editor::Antigravity,
    // Antigravity 2 (IDE and `agy` CLI) shares one global config
    // (antigravity.google/docs/mcp).
    global_config: Some(PathSpec::At(
        Base::Home,
        &[".gemini", "config", "mcp_config.json"],
    )),
    legacy_global_configs: &[PathSpec::At(
        Base::Home,
        &[".gemini", "antigravity", "mcp_config.json"],
    )],
    project_config: Some(ProjectConfig {
        path: ProjectPathSpec::At(&[".agents", "mcp_config.json"]),
        root_key: "mcpServers",
    }),
    dialect: ConfigDialect::JsonServers {
        root_key: "mcpServers",
    },
    remote_shape: RemoteShape::ServerUrl,
    tool_surface_profile: None,
    rules: RulesLayout {
        project: Some(ProjectPathSpec::At(&["GEMINI.md"])),
        global: Some(PathSpec::At(Base::Home, &[".gemini", "GEMINI.md"])),
        legacy_project: &[ProjectPathSpec::At(&[
            ".agent",
            "rules",
            "contextstream.md",
        ])],
        ..NO_RULES_LEGACY
    },
    // A bare ~/.gemini may belong to Gemini CLI alone, so it is not evidence.
    detect: &[
        Evidence::Binary("antigravity"),
        Evidence::Binary("agy"),
        Evidence::HomePath(&[".gemini", "antigravity"]),
        Evidence::HomePath(&[".gemini", "config", "mcp_config.json"]),
    ],
    reload_instruction: "Fully quit and reopen Antigravity, then open the intended checkout.",
    status: ClientStatus::Supported,
};

fn opencode_global_config() -> Option<PathBuf> {
    opencode_config_dir().map(opencode_global_config_file)
}

static OPENCODE: ClientDescriptor = ClientDescriptor {
    editor: Editor::OpenCode,
    // OpenCode reads `~/.config/opencode/opencode.json[c]`
    // (opencode.ai/docs/config); reuse whichever variant exists.
    global_config: Some(PathSpec::Resolve {
        resolve: opencode_global_config,
        display: "~/.config/opencode/opencode.json",
    }),
    legacy_global_configs: &[PathSpec::At(Base::Home, &[".opencode", "mcp.json"])],
    project_config: Some(ProjectConfig {
        path: ProjectPathSpec::At(&["opencode.json"]),
        root_key: "mcp",
    }),
    dialect: ConfigDialect::OpenCode,
    remote_shape: RemoteShape::TypeHttpUrl,
    tool_surface_profile: None,
    rules: RulesLayout {
        project: Some(ProjectPathSpec::At(&["AGENTS.md"])),
        // opencode.ai/docs/rules: global rules live next to the global config.
        global: Some(PathSpec::At(Base::XdgConfig, &["opencode", "AGENTS.md"])),
        legacy_project: &[ProjectPathSpec::At(&["AGENTS.override.md"])],
        // Earlier releases wrote global rules under `~/.opencode`, which
        // OpenCode never reads for AGENTS.md.
        cleanup_global: &[
            PathSpec::At(Base::Home, &[".opencode", "AGENTS.md"]),
            PathSpec::At(Base::Home, &[".opencode", "AGENTS.override.md"]),
        ],
        ..NO_RULES_LEGACY
    },
    detect: &[
        Evidence::Binary("opencode"),
        Evidence::XdgConfigPath(&["opencode"]),
        Evidence::HomePath(&[".opencode"]),
    ],
    reload_instruction:
        "Exit the current OpenCode session, then start a new session in the intended checkout.",
    status: ClientStatus::Supported,
};

/// The descriptor for `editor`.
pub fn descriptor(editor: Editor) -> &'static ClientDescriptor {
    match editor {
        Editor::ClaudeCode => &CLAUDE_CODE,
        Editor::Cursor => &CURSOR,
        Editor::Windsurf => &WINDSURF,
        Editor::Copilot => &COPILOT,
        Editor::Cline => &CLINE,
        Editor::KiloCode => &KILO_CODE,
        Editor::RooCode => &ROO_CODE,
        Editor::Codex => &CODEX,
        Editor::Aider => &AIDER,
        Editor::Antigravity => &ANTIGRAVITY,
        Editor::OpenCode => &OPENCODE,
    }
}

impl Evidence {
    /// Platform-neutral description for the `clients` catalog.
    pub fn display(&self) -> String {
        match self {
            Evidence::Binary(name) => format!("`{name}` on PATH"),
            Evidence::HomePath(parts) => format!("~/{}", parts.join("/")),
            Evidence::HomeDir(parts) => format!("~/{}/", parts.join("/")),
            Evidence::XdgConfigPath(parts) => format!("~/.config/{}", parts.join("/")),
            Evidence::VsCodeExtension(id) => format!("VS Code extension {id}"),
            Evidence::Check(_) => "platform install location".to_string(),
        }
    }
}

/// Machine-readable catalog of every setup client, for docs, the dashboard,
/// and install tooling (`contextstream-mcp clients --format json`).
pub fn catalog_json() -> serde_json::Value {
    let clients: Vec<serde_json::Value> = Editor::all()
        .iter()
        .map(|editor| {
            let d = descriptor(*editor);
            let profile = editor.profile();
            serde_json::json!({
                "id": editor.id(),
                "name": editor.display_name(),
                "status": match d.status {
                    ClientStatus::Supported => "supported",
                    ClientStatus::Discontinued { .. } => "discontinued",
                },
                "successor": match d.status {
                    ClientStatus::Discontinued { successor } => Some(successor.id()),
                    ClientStatus::Supported => None,
                },
                "mcp": {
                    "supported": d.dialect != ConfigDialect::RulesOnly,
                    "format": (d.dialect != ConfigDialect::RulesOnly).then(|| d.dialect.format_name()),
                    "root_key": d.dialect.root_key(),
                    "global_config": (d.dialect != ConfigDialect::RulesOnly)
                        .then(|| d.global_config.map(|spec| spec.display()))
                        .flatten(),
                    "project_config": d.project_config.map(|config| config.path.display()),
                    "project_root_key": d.project_config.map(|config| config.root_key),
                    "remote_entry": super::mcp_config::catalog_remote_entry(editor),
                },
                "rules": {
                    "project": d.rules.project.map(|spec| spec.display()),
                    "global": d.rules.global.map(|spec| spec.display()),
                },
                "hooks": profile.hooks.any(),
                "enforcement_tier": match editor.enforcement_tier() {
                    super::editors::EnforcementTier::TierA => "A",
                    super::editors::EnforcementTier::TierB => "B",
                    super::editors::EnforcementTier::TierC => "C",
                },
                "detect": d.detect.iter().map(Evidence::display).collect::<Vec<_>>(),
                "reload": d.reload_instruction,
            })
        })
        .collect();
    serde_json::json!({
        "schema_version": 1,
        "contextstream_mcp_version": mcp_types::config::VERSION,
        "clients": clients,
    })
}

/// Markdown table of supported clients (for the README and docs).
pub fn catalog_markdown() -> String {
    let mut out = String::from(
        "| Client | `--editors` id | MCP config | Project config | Project rules | Hooks |\n\
         |---|---|---|---|---|---|\n",
    );
    for editor in Editor::all() {
        let d = descriptor(*editor);
        let code = |text: Option<String>| text.map_or("—".to_string(), |t| format!("`{t}`"));
        let name = match d.status {
            ClientStatus::Supported => editor.display_name().to_string(),
            ClientStatus::Discontinued { successor } => format!(
                "{} — discontinued; use {}",
                editor.display_name(),
                successor.display_name()
            ),
        };
        let mcp = if d.dialect == ConfigDialect::RulesOnly {
            "rules only".to_string()
        } else {
            code(d.global_config.map(|spec| spec.display()))
        };
        out.push_str(&format!(
            "| {name} | `{}` | {mcp} | {} | {} | {} |\n",
            editor.id(),
            code(d.project_config.map(|config| config.path.display())),
            code(d.rules.project.map(|spec| spec.display())),
            if editor.has_hooks() { "yes" } else { "—" },
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_editor_has_a_matching_descriptor() {
        for editor in Editor::all() {
            assert_eq!(descriptor(*editor).editor, *editor);
        }
    }

    #[test]
    fn dialects_and_transport_agree_with_the_harness_profile() {
        for editor in Editor::all() {
            let descriptor = descriptor(*editor);
            assert_eq!(
                descriptor.dialect == ConfigDialect::RulesOnly,
                !editor.has_mcp_transport(),
                "{}: rules-only dialect must match a transport-less profile",
                editor.id()
            );
            if let Some(project) = descriptor.project_config {
                assert!(!project.root_key.is_empty(), "{}", editor.id());
            }
            assert!(
                descriptor.reload_instruction.contains("checkout"),
                "{}",
                editor.id()
            );
            assert!(!descriptor.detect.is_empty(), "{}", editor.id());
        }
    }

    #[test]
    fn catalog_lists_every_client_without_local_paths() {
        let catalog = catalog_json();
        let clients = catalog["clients"].as_array().expect("clients");
        assert_eq!(clients.len(), Editor::all().len());
        let rendered = catalog.to_string();
        if let Some(home) = dirs::home_dir() {
            assert!(
                !rendered.contains(&*home.to_string_lossy()),
                "catalog must not leak resolved local paths"
            );
        }
        let roo = clients.iter().find(|c| c["id"] == "roo").expect("roo");
        assert_eq!(roo["status"], "discontinued");
        assert_eq!(roo["successor"], "cline");
        let aider = clients.iter().find(|c| c["id"] == "aider").expect("aider");
        assert_eq!(aider["mcp"]["supported"], false);
        assert!(aider["mcp"]["remote_entry"].is_null());
        let claude = clients
            .iter()
            .find(|c| c["id"] == "claude")
            .expect("claude");
        assert_eq!(claude["mcp"]["global_config"], "~/.claude.json");
        assert_eq!(claude["mcp"]["remote_entry"]["type"], "http");

        let markdown = catalog_markdown();
        assert_eq!(markdown.lines().count(), 2 + Editor::all().len());
        assert!(markdown.contains("| Claude Code | `claude` | `~/.claude.json` |"));
        assert!(markdown.contains("discontinued; use Cline"));
    }

    #[test]
    fn display_paths_are_platform_neutral() {
        assert_eq!(
            CURSOR.global_config.unwrap().display(),
            "~/.cursor/mcp.json"
        );
        assert_eq!(
            COPILOT.global_config.unwrap().display(),
            "<VS Code user dir>/mcp.json"
        );
        assert_eq!(
            OPENCODE.rules.global.unwrap().display(),
            "~/.config/opencode/AGENTS.md"
        );
        assert_eq!(
            KILO_CODE.project_config.unwrap().path.display(),
            "kilo.jsonc"
        );
    }
}
