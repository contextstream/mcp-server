//! ContextStream MCP Server - High-performance Rust implementation.
//!
//! This is the main entry point for the MCP server binary.

// jemalloc is used on glibc/macOS builds; on musl it aborts at teardown (musl's small default TLS
// stack), so musl targets fall back to the system allocator.
#[cfg(all(not(target_env = "msvc"), not(target_env = "musl")))]
#[global_allocator]
static GLOBAL: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use mcp_types::AccountModePreference;
use tracing_subscriber::{fmt, EnvFilter};

use mcp_server::config::load_config;
use mcp_server::{server, setup, transport};

/// Shared team + non-interactive shortcut guidance appended to `--help`.
const CLI_TEAM_AND_SHORTCUTS_HELP: &str = r#"Team workflows:
  Team accounts unlock shared workspace memory, skill discovery/sharing, ticket
  assignment, linked artifacts (docs/plans/handoffs), and team-aware context
  surfacing on every session(action="context") call.

  • Prefer a shared team workspace during setup (contextstream-mcp setup)
  • Switch execution scope: --account-mode=team|personal|auto
    (or CONTEXTSTREAM_ACCOUNT_MODE; MCP: session set_account_mode)
  • Share skills: skill(action="share", scope="team")
  • Team docs: https://contextstream.io/docs/team

Non-interactive shortcuts (CI, scripts, refresh after login):
  contextstream-mcp doctor --scope=all --only-configured
  contextstream-mcp doctor --support --scope=all --only-configured
  contextstream-mcp update-hooks --scope=global --only-configured
  contextstream-mcp update-rules --scope=all --only-configured
  contextstream-mcp update-configs --scope=global --only-configured
  contextstream-mcp migrate-remote --scope=all --only-configured
  contextstream-mcp detect-editors --format=json
  contextstream-mcp generate-configs --transport=remote --preauth
  contextstream-mcp configure --transcripts=on|off --scope=all

  Use update-hooks/update-rules after upgrading or joining a team workspace.
  migrate-remote converts legacy local MCP configs to hosted remote (default).

Hosted transport and local source sync:
  Editors use hosted MCP by default. The installed helper is a managed sync
  bridge, not a second MCP transport: it keeps validated local checkouts fresh
  across machines and Git worktrees. Diagnose it with:
  contextstream-mcp doctor --only-configured

  Local stdio MCP remains an explicit development/recovery mode. Do not switch
  transports merely because a hosted refresh reports an offline bridge.
"#;

/// ContextStream MCP
#[derive(Parser)]
#[command(name = "contextstream-mcp")]
#[command(author = "ContextStream")]
#[command(version)]
#[command(
    about = "ContextStream MCP setup, indexing, and editor integration",
    long_about = "ContextStream MCP connects your editor to workspace memory, semantic code search, plans, and project context through the hosted gateway by default. A managed machine-local sync bridge keeps validated checkouts fresh across machines and Git worktrees without turning the helper into the editor's MCP transport.\n\nStart with `contextstream-mcp setup` for guided account, editor, workspace, project, and indexing setup.\n\nTeam accounts get shared memory, skill discovery, ticket assignment, linked artifacts, and team-aware context surfacing in every session.",
    after_help = CLI_TEAM_AND_SHORTCUTS_HELP
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Enable verbose output
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Quiet mode (errors only)
    #[arg(short, long, global = true)]
    quiet: bool,

    /// Default team vs personal execution mode (also CONTEXTSTREAM_ACCOUNT_MODE).
    #[arg(long, env = "CONTEXTSTREAM_ACCOUNT_MODE", global = true, value_enum)]
    account_mode: Option<AccountModeCli>,
}

#[derive(Clone, Copy, ValueEnum)]
enum AccountModeCli {
    Auto,
    Team,
    Personal,
}

impl From<AccountModeCli> for AccountModePreference {
    fn from(value: AccountModeCli) -> Self {
        match value {
            AccountModeCli::Auto => Self::Auto,
            AccountModeCli::Team => Self::Team,
            AccountModeCli::Personal => Self::Personal,
        }
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Guided account, editor, workspace, project, and indexing setup
    #[command(
        about = "Run the guided ContextStream MCP setup wizard",
        long_about = "Run the guided ContextStream MCP setup wizard.\n\nThe wizard authenticates your account, detects supported editors, links an exact folder to a canonical workspace/project, writes hosted MCP configs and rules, registers the managed local sync bridge, and offers foreground or background indexing when files already exist. Empty folders are valid new projects: setup links them immediately and the bridge syncs files as they appear. The bridge is not a local MCP transport; it keeps machine-local folders, checkouts, and Git worktrees fresh for hosted MCP. A review step lets you go back before setup continues.\n\nWith --yes, setup runs non-interactively using saved credentials, selected or auto-detected editors, and the hosted remote gateway. Pass --project-path to link and index an exact folder. Without it, setup uses the current directory when it is safely scoped; HOME and filesystem roots are rejected, while ordinary empty folders are allowed. Use --account-only to intentionally skip project linking.\n\nTeam accounts: after login the wizard surfaces team workspace tips, shared skill discovery, and non-interactive refresh commands you can run later."
    )]
    Setup {
        /// Run non-interactively with defaults (saved credentials required)
        #[arg(long, short = 'y', alias = "non-interactive")]
        yes: bool,

        /// One-time setup token from the web "build your context engine"
        /// flow — runs the branded non-interactive installer for the
        /// editors/workspace chosen in the questionnaire
        #[arg(long, conflicts_with = "yes")]
        profile: Option<String>,

        /// Path to a pre-fetched setup profile JSON (testing / air-gapped)
        #[arg(long, conflicts_with_all = ["yes", "profile"])]
        profile_file: Option<std::path::PathBuf>,

        /// Comma-separated editor ids to configure (e.g. claude,cursor).
        /// Without this, --yes configures every detected editor.
        #[arg(long, value_delimiter = ',')]
        editors: Vec<String>,

        /// Project folder to link and index. Relative paths resolve from the
        /// current directory. HOME and filesystem roots are rejected; ordinary
        /// empty folders are linked now and sync when files are added.
        #[arg(long, value_name = "PATH", conflicts_with = "account_only")]
        project_path: Option<std::path::PathBuf>,

        /// Save credentials and editor-global configuration without linking
        /// or indexing a project. This is an explicit partial setup state.
        #[arg(long, conflicts_with = "project_path")]
        account_only: bool,

        /// Show every file that would change, without writing anything.
        #[arg(long)]
        dry_run: bool,
    },

    /// Validate editor integration health and readiness
    #[command(
        about = "Check configured editor surfaces, teaching readiness, and safe repairs",
        long_about = "Validate ContextStream editor integration health.\n\nTarget precedence is explicit --editors, then the editors recorded by setup, then installed-editor detection for legacy read-only use. --only-configured disables detection. Doctor verifies managed MCP identity, current rules, exact owned hooks, credentials, and local/remote readiness evidence. --repair is opt-in and never uses editor detection; --dry-run previews only managed writes. --json emits the complete local diagnostic report, including local paths. --support emits a separate privacy-bounded report designed for sharing with ContextStream support: version, transport, selected clients, evidence-backed activation state, opaque project/checkout identifiers, and bridge health, but no credentials or local paths. Failures produce a non-zero exit code."
    )]
    Doctor {
        /// Emit a machine-readable JSON report
        #[arg(long, conflicts_with = "support")]
        json: bool,

        /// Emit a privacy-bounded JSON report suitable for sharing with support
        #[arg(long, conflicts_with = "json")]
        support: bool,

        /// Surfaces to inspect or repair: global, project, or all. Read-only
        /// doctor defaults to all; repair requires an explicit scope.
        #[arg(long)]
        scope: Option<String>,

        /// Comma-separated editor ids to inspect or repair
        #[arg(long, value_delimiter = ',')]
        editors: Vec<String>,

        /// Use only editors recorded by setup; never fall back to detection
        #[arg(long)]
        only_configured: bool,

        /// Repair only the selected/configured managed surfaces, then verify
        #[arg(long)]
        repair: bool,

        /// Preview repair writes without changing any file
        #[arg(long, requires = "repair")]
        dry_run: bool,
    },

    /// Show version, build, security, and connection info
    About,

    /// Run HTTP gateway mode
    #[command(hide = true)]
    Http {
        /// Host to bind to
        #[arg(long, default_value = "0.0.0.0")]
        host: String,

        /// Port to listen on
        #[arg(long, default_value = "8787")]
        port: u16,
    },

    /// Verify API key and show account info
    VerifyKey {
        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Run a hook
    Hook {
        /// Hook name
        name: String,

        /// Trailing arguments forwarded from the host hook (e.g. git hook argv:
        /// post-checkout `<prev> <new> <flag>`, pre-push `<remote> <url>`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Install or remove the managed git hooks for local VCS capture
    #[command(
        long_about = "Install (or with --uninstall, remove) the managed git hooks (post-commit, pre-push, post-checkout, post-merge) that capture local commits/pushes/branch-switches/merges into ContextStream. Idempotent; preserves and chains any existing user hooks. Honors the CONTEXTSTREAM_GIT_CAPTURE kill-switch and per-repo .contextstream/config.json git_capture policy."
    )]
    GitHooks {
        /// Repository path (defaults to the current directory)
        #[arg(long)]
        path: Option<String>,

        /// Remove the managed hooks instead of installing them
        #[arg(long)]
        uninstall: bool,
    },

    /// Update hooks for the editors setup configured (non-interactive)
    #[command(
        long_about = "Update lifecycle hooks for the editors setup configured, without running the full setup wizard.\n\nBy default this only touches editors you opted into during setup; it falls back to detection only when no selection was ever recorded. Use --editors to state the target set explicitly.\n\nTeam tip: re-run after joining a team workspace or upgrading so PreToolUse/UserPromptSubmit hooks stay aligned with team context surfacing."
    )]
    UpdateHooks {
        /// Scope: global, project, or all
        #[arg(long, default_value = "global")]
        scope: String,

        /// Comma-separated editor ids to update (e.g. claude,cursor). Defaults
        /// to the editors setup configured on this machine.
        #[arg(long, value_delimiter = ',')]
        editors: Vec<String>,

        /// Refresh only editors setup already configured; do nothing when no
        /// selection has been recorded. Used by install scripts so an
        /// unattended refresh can never configure a new editor on its own.
        #[arg(long)]
        only_configured: bool,

        /// Show every file that would be created or modified, without writing
        /// anything.
        #[arg(long)]
        dry_run: bool,
    },

    /// Update AI rules for the editors setup configured (non-interactive)
    #[command(
        long_about = "Refresh ContextStream rule files (.cursorrules, CLAUDE.md, etc.) for the editors setup configured. Detection is used only for legacy installs that have never recorded a selection.\n\nTeam tip: rules include team workflow guidance; run after setup or when teammates update shared conventions. Use --workspace-id/--workspace-name to target a team workspace."
    )]
    UpdateRules {
        /// Scope: global, project, or all
        #[arg(long, default_value = "all")]
        scope: String,

        /// Workspace ID override
        #[arg(long)]
        workspace_id: Option<String>,

        /// Workspace name override
        #[arg(long)]
        workspace_name: Option<String>,

        /// Comma-separated editor ids to target (e.g. claude,cursor). Defaults
        /// to the editors setup configured on this machine.
        #[arg(long, value_delimiter = ',')]
        editors: Vec<String>,

        /// Only target editors setup already configured; do nothing when no
        /// selection has been recorded.
        #[arg(long)]
        only_configured: bool,

        /// Show every file that would be created or modified, without writing
        /// anything.
        #[arg(long)]
        dry_run: bool,
    },

    /// Update MCP configs for the editors setup configured (non-interactive)
    #[command(
        long_about = "Regenerate MCP config files for the editors setup configured. Detection is used only for legacy installs that have never recorded a selection.\n\nUse after credential rotation, workspace changes, or scripted team onboarding."
    )]
    UpdateConfigs {
        /// Scope: global, project, or all
        #[arg(long, default_value = "global")]
        scope: String,

        /// Comma-separated editor ids to target (e.g. claude,cursor). Defaults
        /// to the editors setup configured on this machine.
        #[arg(long, value_delimiter = ',')]
        editors: Vec<String>,

        /// Only target editors setup already configured; do nothing when no
        /// selection has been recorded.
        #[arg(long)]
        only_configured: bool,

        /// Show every file that would be created or modified, without writing
        /// anything.
        #[arg(long)]
        dry_run: bool,
    },

    /// Migrate existing MCP configs to hosted remote transport
    #[command(
        long_about = "Convert legacy local stdio MCP configs to hosted remote transport (default for new installs).\n\nLocal binary mode is recovery-only; set CONTEXTSTREAM_ALLOW_LOCAL_MCP=1 when explicitly needed."
    )]
    MigrateRemote {
        /// Scope: global, project, or all
        #[arg(long, default_value = "all")]
        scope: String,

        /// Comma-separated editor ids to target (e.g. claude,cursor). Defaults
        /// to the editors setup configured on this machine.
        #[arg(long, value_delimiter = ',')]
        editors: Vec<String>,

        /// Only target editors setup already configured; do nothing when no
        /// selection has been recorded.
        #[arg(long)]
        only_configured: bool,

        /// Show every file that would be created or modified, without writing
        /// anything.
        #[arg(long)]
        dry_run: bool,
    },

    /// Remove ContextStream from your editors (hooks, MCP configs, rules)
    #[command(
        long_about = "Remove ContextStream hooks, MCP config entries, and rules files from your editors.\n\nEach removal is surgical: only ContextStream-owned entries are stripped, and any other content in those files is left untouched. Use --dry-run first to see exactly what would change."
    )]
    Uninstall {
        /// Scope: global, project, or all
        #[arg(long, default_value = "all")]
        scope: String,

        /// Comma-separated editor ids to clean up. Defaults to every editor
        /// setup configured, plus any detected on this machine.
        #[arg(long, value_delimiter = ',')]
        editors: Vec<String>,

        /// Also remove ContextStream's managed git hooks from this repo.
        #[arg(long)]
        git_hooks: bool,

        /// Show every file that would change, without writing anything.
        #[arg(long)]
        dry_run: bool,
    },

    /// Install binary to system PATH
    #[command(hide = true)]
    Install {
        /// Target directory (default: platform-specific)
        #[arg(long)]
        path: Option<String>,
    },

    /// Configure editors, credentials, workspace, and transcript defaults
    #[command(
        hide = true,
        long_about = "Non-interactive configuration for transcript capture defaults and related settings.\n\nExample: contextstream-mcp configure --transcripts=on --scope=all"
    )]
    Configure {
        /// Print configurable options and exit
        #[arg(long)]
        list_options: bool,

        /// Set default transcript capture for context() calls: on|off
        #[arg(long, value_enum)]
        transcripts: Option<ToggleValue>,

        /// Set default hook transcript capture: on|off
        #[arg(long, value_enum)]
        hook_transcripts: Option<ToggleValue>,

        /// Scope for config updates: global, project, or all
        #[arg(long, value_enum, default_value_t = ConfigureScope::All)]
        scope: ConfigureScope,

        /// Comma-separated editor ids whose MCP configs should receive the
        /// transcript defaults. Defaults to the editors setup configured.
        #[arg(long, value_delimiter = ',')]
        editors: Vec<String>,

        /// Do nothing when setup has not recorded an editor selection instead
        /// of falling back to installed-editor detection.
        #[arg(long)]
        only_configured: bool,

        /// Preview transcript-default and follow-up hook changes without
        /// modifying files.
        #[arg(long)]
        dry_run: bool,
    },

    /// Ingest local project files into ContextStream for semantic search
    ///
    /// Scans the current directory (or --path), resolves or creates a ContextStream
    /// project, and uploads files with progress tracking. After ingestion, your
    /// codebase is searchable via the MCP `search()` tool in your editor.
    Ingest {
        /// Path to the project directory (defaults to current directory)
        #[arg(long)]
        path: Option<String>,

        /// Force re-ingest all files, even if unchanged
        #[arg(long)]
        force: bool,

        /// Include media files (images, audio, video)
        #[arg(long)]
        include_media: bool,

        /// Run indexing in the background and notify when complete
        #[arg(long)]
        background: bool,
    },

    /// Index a project with progress tracking (interactive)
    ///
    /// Scans the current directory (or --path), creates/resolves a ContextStream project,
    /// and uploads files with a visual progress bar. After indexing, your codebase
    /// is searchable via the MCP `search()` tool in your editor.
    Index {
        /// Path to the project directory (defaults to current directory)
        #[arg(long)]
        path: Option<String>,

        /// Include media files (images, audio, video)
        #[arg(long)]
        include_media: bool,

        /// Run indexing in the background and notify when complete
        #[arg(long)]
        background: bool,
    },

    /// Connect this exact local checkout to a dashboard project enrollment
    ///
    /// Run the command copied from the dashboard inside the intended project
    /// folder. The one-time token is normally supplied through the
    /// CONTEXTSTREAM_BRIDGE_TOKEN environment variable so it is not exposed in
    /// the process argument list.
    Connect {
        /// Short-lived project enrollment token (prefer CONTEXTSTREAM_BRIDGE_TOKEN)
        #[arg(long, env = "CONTEXTSTREAM_BRIDGE_TOKEN", hide_env_values = true)]
        token: Option<String>,

        /// Exact local checkout to connect (defaults to current directory)
        #[arg(long)]
        path: Option<String>,
    },

    /// Watch mapped projects and auto-ingest local edits (content freshness)
    ///
    /// Managed, editor-agnostic sync bridge. Monitors every validated checkout
    /// mapped in ~/.contextstream/mappings.json + indexed-projects.json and
    /// re-ingests changed files automatically (debounced, ignore-aware,
    /// non-billed). Distinct machines and Git worktrees retain distinct
    /// checkout identities for one canonical project. This is not the editor's
    /// MCP transport: hosted MCP remains the default. Runs as a singleton per
    /// machine; disable with CONTEXTSTREAM_WATCH=0.
    #[command(hide = true)]
    Watch,

    /// Update to the latest version
    Update {
        /// Only check for updates, don't install
        #[arg(long)]
        check: bool,

        /// Force reinstall even if already on latest version
        #[arg(long)]
        force: bool,

        /// Compatibility flag; updates migrate existing configs to hosted remote by default
        #[arg(long)]
        remote: bool,
    },

    /// Detect installed editors and output as JSON (non-interactive)
    #[command(
        long_about = "Detect installed editors and emit JSON for scripting.\n\nPair with generate-configs in CI or team bootstrap scripts."
    )]
    DetectEditors {
        /// Output format: json (default)
        #[arg(long, default_value = "json")]
        format: String,
    },

    /// Manage file exclusion patterns for indexing
    ///
    /// View, add, or remove patterns that control which files are excluded
    /// from indexing. Patterns are stored in .contextignore and follow
    /// gitignore-style syntax. Sensitive files like .env and private keys
    /// are always excluded on the server regardless of these settings.
    Exclude {
        /// Action: list, add, remove, reset, info
        #[arg(value_enum, default_value = "list")]
        action: ExcludeAction,

        /// Path to the project directory (defaults to current directory)
        #[arg(long)]
        path: Option<String>,

        /// Pattern(s) to add or remove (for add/remove actions)
        #[arg(trailing_var_arg = true)]
        patterns: Vec<String>,
    },

    /// Generate MCP configs as JSON without writing files (non-interactive)
    ///
    /// Used by the remote-first install script as a fallback for config generation.
    /// Outputs a JSON payload containing config objects for each specified editor.
    #[command(
        long_about = "Generate MCP config payloads as JSON without writing files.\n\nUsed by install scripts and team bootstrap automation. Pass --preauth to embed a validated API key so the editor can connect after it reloads the generated configuration."
    )]
    GenerateConfigs {
        /// Transport mode: remote or local
        #[arg(long, default_value = "remote")]
        transport: String,

        /// Comma-separated editor IDs (e.g. "claude,cursor,codex"). Defaults to all detected.
        #[arg(long)]
        editors: Option<String>,

        /// API key to embed in configs
        #[arg(long, env = "CONTEXTSTREAM_API_KEY")]
        api_key: Option<String>,

        /// Workspace ID to embed in configs
        #[arg(long)]
        workspace_id: Option<String>,

        /// Project ID to embed in configs
        #[arg(long)]
        project_id: Option<String>,

        /// Pre-authenticate remote configs with the API key
        #[arg(long)]
        preauth: bool,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ToggleValue {
    On,
    Off,
}

impl ToggleValue {
    fn as_bool(self) -> bool {
        matches!(self, Self::On)
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ConfigureScope {
    Global,
    Project,
    All,
}

impl ConfigureScope {
    fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Project => "project",
            Self::All => "all",
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum ExcludeAction {
    /// List current exclusion patterns
    List,
    /// Add one or more exclusion patterns
    Add,
    /// Remove one or more exclusion patterns
    Remove,
    /// Reset to default patterns
    Reset,
    /// Show server-side protections info
    Info,
}

fn setup_logging(verbose: bool, quiet: bool) {
    let filter = if verbose {
        "debug"
    } else if quiet {
        "error"
    } else {
        "info"
    };

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(filter));

    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_thread_ids(false)
        .with_writer(std::io::stderr)
        .init();
}

fn _print_version() {
    println!("contextstream-mcp v{}", mcp_types::config::VERSION);
}

fn _print_help() {
    println!(
        r#"ContextStream MCP Server (contextstream-mcp) v{}

Usage:
  contextstream-mcp
  contextstream-mcp about
  contextstream-mcp setup
  contextstream-mcp update [--check] [--force] [--remote]
  contextstream-mcp verify-key [--json]
  contextstream-mcp ingest [--path=<dir>] [--force] [--include-media] [--background]
  contextstream-mcp index [--path=<dir>] [--include-media] [--background]
  contextstream-mcp connect [--path=<dir>]
  contextstream-mcp update-hooks [--scope=global] [--only-configured]
  contextstream-mcp migrate-remote [--scope=all] [--only-configured]
  contextstream-mcp update-rules [--scope=all] [--only-configured]
  contextstream-mcp exclude [list|add|remove|reset|info] [PATTERNS...]
  contextstream-mcp detect-editors [--format=json]
  contextstream-mcp generate-configs [--transport=remote] [--editors=claude,cursor] [--preauth]

Commands:
  about                        Show version, build, security, and connection info
  setup                        Interactive onboarding wizard
  update [--check] [--force] [--remote] Update to the latest version and refresh hosted remote editor integrations
  ingest [--path] [--include-media] [--background]
                               Index project files for semantic search (alias for index)
  index  [--path] [--include-media] [--background]
                               Index project files with interactive progress bar
  connect [--path]             Connect this exact checkout using a dashboard enrollment link
  exclude [action] [patterns]  Manage file exclusion patterns (.contextignore)
    list                       Show current exclusion patterns (default)
    add <patterns...>          Add patterns (e.g. exclude add vendor/ *.tmp)
    remove <patterns...>       Remove patterns
    reset                      Reset to default exclusion patterns
    info                       Show server-side security protections
  verify-key [--json]          Verify API key and show account info
  update-hooks [--scope=..]    Update hooks for editors selected during setup (non-interactive)
  migrate-remote [--scope=..]  Convert existing local MCP configs to hosted remote transport
  update-rules [--scope=..]    Update AI rules for editors selected during setup (non-interactive)
  detect-editors               Detect installed editors and output as JSON
  generate-configs             Generate MCP config payloads as JSON (for scripted setup)

Environment variables:
  CONTEXTSTREAM_API_URL      Base API URL (default: https://api.contextstream.io)
  CONTEXTSTREAM_API_KEY      API key for authentication
  CONTEXTSTREAM_JWT          JWT for authentication (alternative to API key)
  CONTEXTSTREAM_WORKSPACE_ID Optional default workspace ID
  CONTEXTSTREAM_PROJECT_ID   Optional default project ID
  CONTEXTSTREAM_TOOLSET      Tool mode: light|standard|complete (default: standard)
  CONTEXTSTREAM_LOG_LEVEL    Log level: quiet|normal|verbose (default: quiet)
  CONTEXTSTREAM_VSCODE_MCP_MODE  VS Code/Copilot setup mode: auto|remote|local (local is recovery-only)
  CONTEXTSTREAM_ALLOW_LOCAL_MCP  Allow explicit local MCP recovery mode when set to 1/true
  CONTEXTSTREAM_WATCH        Managed hosted sync bridge (enabled by default; set 0/false/off/no to disable)
  CONTEXTSTREAM_CONCISE_TOOL_TEXT  Return concise tool text while preserving structured payloads (default: true; set false to restore verbose text)

Examples:
  CONTEXTSTREAM_API_KEY="your_api_key" contextstream-mcp

Notes:
  - Setup configures editors for hosted MCP by default. The installed helper
    keeps local checkouts fresh through the managed sync bridge; it is not a
    second editor MCP transport.
  - Multiple machines and Git worktrees may map to one canonical project while
    retaining distinct checkout identities and refresh state.
  - Invoking the binary with no subcommand starts the explicit local stdio
    development/recovery transport; logs are written to stderr.
  - This is the high-performance Rust implementation.
  - GitHub Copilot canonical paths:
      - Global MCP config: ~/.copilot/mcp-config.json
      - Project MCP config (VS Code): .vscode/mcp.json
      - Project rules: .github/copilot-instructions.md
      - Companion skill: .github/skills/contextstream-workflow/SKILL.md
      - Select Copilot in setup; both MCP configs are handled automatically.
  - Copilot/VS Code troubleshooting:
      - Verify JSON shape (`mcpServers` for Copilot CLI, `servers` for VS Code)
      - Use local VS Code/Copilot MCP only for recovery with `CONTEXTSTREAM_ALLOW_LOCAL_MCP=1 CONTEXTSTREAM_VSCODE_MCP_MODE=local`
      - Ensure init() then context(...) on first message
      - Ensure project indexing has completed before relying on search()
  - Claude/Cursor enforcement troubleshooting:
      - Re-run setup/update-hooks to refresh lifecycle hooks
      - Verify ContextStream hook entries exist for PreToolUse + UserPromptSubmit
      - Ensure CONTEXTSTREAM_HOOK_ENABLED is not set to false
  - Antigravity troubleshooting:
      - Antigravity currently uses rules-only guidance (no lifecycle hooks)
      - Verify ~/.gemini/antigravity/mcp_config.json contains contextstream
      - Follow strict init() -> context() -> search(mode="auto") workflow

Team workflows:
  Team accounts unlock shared workspace memory, skill discovery/sharing, ticket
  assignment, linked artifacts, and team-aware context surfacing on every
  session(action="context") call. Prefer a shared team workspace during setup.
  Switch scope: --account-mode=team|personal|auto (CONTEXTSTREAM_ACCOUNT_MODE).

Non-interactive shortcuts (CI, scripts, refresh after login):
  contextstream-mcp update-hooks --scope=global --only-configured
  contextstream-mcp update-rules --scope=all --only-configured
  contextstream-mcp update-configs --scope=global --only-configured
  contextstream-mcp migrate-remote --scope=all --only-configured
  contextstream-mcp detect-editors --format=json
  contextstream-mcp generate-configs --transport=remote --preauth
  contextstream-mcp configure --transcripts=on|off --scope=all
"#,
        mcp_types::config::VERSION
    );
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    if let Some(mode) = cli.account_mode {
        std::env::set_var(
            "CONTEXTSTREAM_ACCOUNT_MODE",
            AccountModePreference::from(mode).as_str(),
        );
    }

    setup_logging(cli.verbose, cli.quiet);

    // Stamp the process-global teaching-bundle fingerprint so request bodies,
    // readiness, doctor, runtime staleness checks, and every editor writer
    // share one contract. It is cached, idempotent, and runs before tools.
    mcp_server::setup::install_canonical_rules_hash();

    match cli.command {
        Some(Commands::Doctor {
            json,
            support,
            scope,
            editors,
            only_configured,
            repair,
            dry_run,
        }) => {
            if repair && scope.is_none() {
                eprintln!(
                    "Doctor failed: --repair requires an explicit --scope global|project|all; no files were changed"
                );
                std::process::exit(2);
            }
            let explicit = match parse_editor_ids(&editors) {
                Ok(list) => list,
                Err(e) => {
                    eprintln!("Doctor failed: {}", e);
                    std::process::exit(2);
                }
            };
            let scope_was_explicit = scope.is_some();
            let scope = match setup::doctor::DoctorScope::parse(scope.as_deref().unwrap_or("all")) {
                Ok(scope) => scope,
                Err(e) => {
                    eprintln!("Doctor failed: {}", e);
                    std::process::exit(2);
                }
            };
            let mut options = setup::doctor::DoctorOptions::new(std::env::current_dir().ok());
            options.explicit_editors = explicit;
            options.only_configured = only_configured;
            options.scope = scope;
            options.scope_was_explicit = scope_was_explicit;
            options.repair = repair;
            options.dry_run = dry_run;
            match setup::doctor::run_doctor(options, json, support).await {
                Ok(true) => std::process::exit(1),
                Ok(false) => {}
                Err(e) => {
                    eprintln!("Doctor failed: {}", e);
                    std::process::exit(2);
                }
            }
        }
        Some(Commands::Setup {
            yes,
            profile,
            profile_file,
            editors,
            project_path,
            account_only,
            dry_run,
        }) => {
            setup::safe_edit::set_dry_run(dry_run);
            if dry_run && profile.is_some() {
                eprintln!(
                    "Setup failed: --dry-run cannot redeem a one-time --profile token because \
                     redemption consumes server-side state. Use --profile-file to preview a \
                     previously fetched profile, or run without --dry-run."
                );
                std::process::exit(1);
            }
            let explicit = match parse_editor_ids(&editors) {
                Ok(list) => list,
                Err(e) => {
                    eprintln!("{}", e);
                    std::process::exit(1);
                }
            };
            let result = if profile.is_some() || profile_file.is_some() {
                setup::profile::run_setup_with_profile(
                    profile,
                    profile_file,
                    explicit.as_deref(),
                    project_path.as_deref(),
                    account_only,
                )
                .await
            } else {
                setup::run_setup_wizard_with_options(
                    yes,
                    explicit.as_deref(),
                    project_path.as_deref(),
                    account_only,
                )
                .await
            };
            if let Err(e) = result {
                eprintln!("Setup failed: {}", e);
                std::process::exit(1);
            }
            // Validate hooks after setup without rewriting MCP configs. Scoped
            // to the selection setup just recorded, so declining every editor
            // leaves every editor alone.
            if !dry_run {
                if let Err(e) = setup::update_hooks_scoped("global", None, true).await {
                    eprintln!("Warning: Could not validate hooks: {}", e);
                }
            }
            if dry_run {
                setup::report_dry_run();
            }
        }

        Some(Commands::About) => {
            run_about().await;
        }

        Some(Commands::Http { host, port }) => {
            run_http_server(&host, port).await?;
        }

        Some(Commands::VerifyKey { json }) => {
            run_verify_key(json).await?;
        }

        Some(Commands::Hook { name, args }) => {
            // SAFETY: hook dispatch is single-threaded at this point (before tokio runtime).
            unsafe { std::env::set_var("HOOK_EVENT_NAME", hook_event_name(&name)) };
            // Forward host hook argv (git hooks) to handlers via env; values are
            // Unit-Separator joined and read by hook_handlers::git_common::hook_args().
            // Editor hooks append an ownership marker to the command. It is
            // consumed here rather than exposed to event handlers as host data.
            let is_managed_hook = args
                .last()
                .is_some_and(|arg| arg == setup::MANAGED_HOOK_ARGUMENT);
            if is_managed_hook {
                unsafe { std::env::set_var("CONTEXTSTREAM_MANAGED_HOOK_INVOCATION", "1") };
            } else {
                unsafe { std::env::remove_var("CONTEXTSTREAM_MANAGED_HOOK_INVOCATION") };
            }
            let forwarded_len = args.len().saturating_sub(is_managed_hook as usize);
            let forwarded_args = &args[..forwarded_len];
            if !forwarded_args.is_empty() {
                unsafe {
                    std::env::set_var("CONTEXTSTREAM_HOOK_ARGS", forwarded_args.join("\u{1f}"))
                };
            } else {
                // Do not let a caller-provided or inherited value masquerade
                // as host hook arguments when the managed marker is the only
                // command-line argument.
                unsafe { std::env::remove_var("CONTEXTSTREAM_HOOK_ARGS") };
            }
            if let Err(e) = mcp_server::hook_handlers::dispatch_hook(&name).await {
                eprintln!("Hook error: {}", e);
                std::process::exit(1);
            }
        }

        Some(Commands::GitHooks { path, uninstall }) => {
            run_git_hooks(path, uninstall);
        }

        Some(Commands::Uninstall {
            scope,
            editors,
            git_hooks,
            dry_run,
        }) => {
            setup::safe_edit::set_dry_run(dry_run);
            let explicit = match parse_editor_ids(&editors) {
                Ok(list) => list,
                Err(e) => {
                    eprintln!("{}", e);
                    std::process::exit(1);
                }
            };
            if let Err(e) = setup::uninstall(&scope, explicit.as_deref(), git_hooks).await {
                eprintln!("Uninstall failed: {}", e);
                std::process::exit(1);
            }
            if dry_run {
                setup::report_dry_run();
            }
        }

        Some(Commands::UpdateHooks {
            scope,
            editors,
            only_configured,
            dry_run,
        }) => {
            setup::safe_edit::set_dry_run(dry_run);
            let explicit = match parse_editor_ids(&editors) {
                Ok(list) => list,
                Err(e) => {
                    eprintln!("{}", e);
                    std::process::exit(1);
                }
            };
            if let Err(e) =
                setup::update_hooks_scoped(&scope, explicit.as_deref(), only_configured).await
            {
                eprintln!("Failed to update hooks: {}", e);
                std::process::exit(1);
            }
            if dry_run {
                setup::report_dry_run();
            }
        }

        Some(Commands::UpdateRules {
            scope,
            workspace_id,
            workspace_name,
            editors,
            only_configured,
            dry_run,
        }) => {
            setup::safe_edit::set_dry_run(dry_run);
            let explicit = match parse_editor_ids(&editors) {
                Ok(list) => list,
                Err(e) => {
                    eprintln!("{}", e);
                    std::process::exit(1);
                }
            };
            if let Err(e) = setup::update_rules_scoped(
                &scope,
                workspace_id.as_deref(),
                workspace_name.as_deref(),
                explicit.as_deref(),
                only_configured,
            )
            .await
            {
                eprintln!("Failed to update rules: {}", e);
                std::process::exit(1);
            }
            if dry_run {
                setup::report_dry_run();
            }
        }

        Some(Commands::UpdateConfigs {
            scope,
            editors,
            only_configured,
            dry_run,
        }) => {
            setup::safe_edit::set_dry_run(dry_run);
            let explicit = match parse_editor_ids(&editors) {
                Ok(list) => list,
                Err(e) => {
                    eprintln!("{}", e);
                    std::process::exit(1);
                }
            };
            if let Err(e) =
                setup::update_configs_scoped(&scope, explicit.as_deref(), only_configured).await
            {
                eprintln!("Failed to update configs: {}", e);
                std::process::exit(1);
            }
            if dry_run {
                setup::report_dry_run();
            }
        }

        Some(Commands::MigrateRemote {
            scope,
            editors,
            only_configured,
            dry_run,
        }) => {
            setup::safe_edit::set_dry_run(dry_run);
            let explicit = match parse_editor_ids(&editors) {
                Ok(list) => list,
                Err(e) => {
                    eprintln!("{}", e);
                    std::process::exit(1);
                }
            };
            if let Err(e) =
                setup::migrate_remote_scoped(&scope, explicit.as_deref(), only_configured).await
            {
                eprintln!("Failed to migrate configs to hosted remote: {}", e);
                std::process::exit(1);
            }
            if dry_run {
                setup::report_dry_run();
            }
        }

        Some(Commands::Install { path }) => {
            let install_dir = path.unwrap_or_else(|| {
                let expected = expected_install_path();
                std::path::Path::new(&expected)
                    .parent()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|| {
                        if cfg!(windows) {
                            std::env::var("LOCALAPPDATA")
                                .map(|d| format!("{}\\ContextStream", d))
                                .unwrap_or_else(|_| ".".to_string())
                        } else {
                            "/usr/local/bin".to_string()
                        }
                    })
            });
            let target = std::path::PathBuf::from(&install_dir);
            match setup::install_binary(&target) {
                Ok(()) => {
                    eprintln!("Installed to {}/{}", install_dir, binary_name());
                }
                Err(e) => {
                    eprintln!("Install failed: {}", e);
                    std::process::exit(1);
                }
            }
        }

        Some(Commands::Configure {
            list_options,
            transcripts,
            hook_transcripts,
            scope,
            editors,
            only_configured,
            dry_run,
        }) => {
            setup::safe_edit::set_dry_run(dry_run);
            let explicit = match parse_editor_ids(&editors) {
                Ok(list) => list,
                Err(e) => {
                    eprintln!("{}", e);
                    std::process::exit(1);
                }
            };
            if transcripts.is_none()
                && hook_transcripts.is_none()
                && !list_options
                && (explicit.is_some() || only_configured || dry_run)
            {
                eprintln!(
                    "--editors, --only-configured, and --dry-run on `configure` require \
                     --transcripts or --hook-transcripts"
                );
                std::process::exit(1);
            }
            if let Err(e) = run_configure(
                list_options,
                transcripts,
                hook_transcripts,
                scope,
                explicit.as_deref(),
                only_configured,
            )
            .await
            {
                eprintln!("Configure failed: {}", e);
                std::process::exit(1);
            }
            if !list_options {
                // Keep `configure --list-options` strictly read-only, and
                // only refresh editors the user already configured — tweaking
                // a setting must not enroll a new editor.
                if let Err(e) =
                    setup::update_hooks_scoped("global", explicit.as_deref(), true).await
                {
                    eprintln!("Warning: Could not validate hooks: {}", e);
                }
            }
            if dry_run {
                setup::report_dry_run();
            }
        }

        Some(Commands::Ingest {
            path,
            force,
            include_media,
            background,
        }) => {
            if let Err(e) = run_index_or_ingest(path, include_media, background, force).await {
                eprintln!("Ingest failed: {}", e);
                std::process::exit(1);
            }
        }

        Some(Commands::Index {
            path,
            include_media,
            background,
        }) => {
            if let Err(e) = run_index_or_ingest(path, include_media, background, false).await {
                eprintln!("Index failed: {}", e);
                std::process::exit(1);
            }
        }

        Some(Commands::Connect { token, path }) => {
            if let Err(error) = mcp_server::connect::run_connect(token, path).await {
                eprintln!("Connection failed: {error:#}");
                std::process::exit(1);
            }
        }

        Some(Commands::Watch) => {
            if let Err(e) = mcp_server::watch::run_watch().await {
                eprintln!("Watch failed: {}", e);
                std::process::exit(1);
            }
        }

        Some(Commands::Update {
            check,
            force,
            remote,
        }) => {
            if let Err(e) = run_update(check, force, remote).await {
                eprintln!("Update failed: {}", e);
                std::process::exit(1);
            }
        }

        Some(Commands::DetectEditors { format: _ }) => {
            let result = setup::editors::detect_installed_editors_json();
            println!("{}", serde_json::to_string_pretty(&result).unwrap());
        }

        Some(Commands::GenerateConfigs {
            transport,
            editors,
            api_key,
            workspace_id,
            project_id,
            preauth,
        }) => {
            let api_key = api_key
                .or_else(|| std::env::var("CONTEXTSTREAM_API_KEY").ok())
                .or_else(setup::get_api_key)
                .unwrap_or_default();

            let target_editors = match editors {
                Some(editor_list) => {
                    let ids: Vec<&str> = editor_list.split(',').map(str::trim).collect();
                    setup::editors::Editor::all()
                        .iter()
                        .copied()
                        .filter(|e| ids.contains(&e.id()))
                        .collect::<Vec<_>>()
                }
                None => setup::editors::detect_installed_editors(),
            };

            let remote_auth = if preauth && !api_key.is_empty() {
                Some(api_key.as_str())
            } else {
                None
            };
            let requested_local = matches!(
                transport.trim().to_ascii_lowercase().as_str(),
                "local" | "binary" | "local-binary"
            );
            let normalized_transport = if requested_local && setup::local_mcp_allowed() {
                "local"
            } else {
                if requested_local {
                    eprintln!(
                        "Ignoring local config generation request. Hosted remote gateway is the default; \
                         set CONTEXTSTREAM_ALLOW_LOCAL_MCP=1 or run `contextstream-mcp setup` and choose \
                         local transport (writes ~/.contextstream/setup-transport-mode)."
                    );
                }
                "remote"
            };

            let result = setup::generate_all_configs_json(
                &target_editors,
                &api_key,
                workspace_id.as_deref(),
                project_id.as_deref(),
                normalized_transport,
                remote_auth,
            )?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }

        Some(Commands::Exclude {
            action,
            patterns,
            path,
        }) => {
            if let Err(e) = run_exclude(action, patterns, path).await {
                eprintln!("Exclude command failed: {}", e);
                std::process::exit(1);
            }
        }

        None => {
            // Default: run MCP server with stdio transport
            run_stdio_server().await?;
        }
    }

    Ok(())
}

const CONTEXTIGNORE_FILE: &str = ".contextignore";

const DEFAULT_EXCLUDE_PATTERNS: &[&str] = &[
    "node_modules/",
    "dist/",
    "build/",
    "out/",
    ".next/",
    ".nuxt/",
    ".svelte-kit/",
    ".parcel-cache/",
    ".turbo/",
    "__pycache__/",
    ".pytest_cache/",
    ".mypy_cache/",
    "*.log",
    ".DS_Store",
    "coverage/",
    ".cache/",
    "target/",
    "bin/",
    "obj/",
    ".venv/",
    "venv/",
    "vendor/",
    ".gradle/",
    ".idea/",
    ".vscode/",
    ".contextstream/",
    ".cursor/",
    ".windsurf/",
    ".roo/",
    ".kilocode/",
    "*.min.js",
    "*.min.css",
];

async fn run_exclude(
    action: ExcludeAction,
    patterns: Vec<String>,
    path: Option<String>,
) -> Result<()> {
    use console::style;

    let project_dir = match path {
        Some(p) => std::path::PathBuf::from(p),
        None => std::env::current_dir()?,
    };

    let ignore_path = project_dir.join(CONTEXTIGNORE_FILE);

    match action {
        ExcludeAction::List => {
            eprintln!(
                "\n{} Exclusion Patterns {}",
                style("⬡").cyan(),
                style(format!("({})", ignore_path.display())).dim()
            );
            eprintln!();

            let current = read_contextignore(&ignore_path);
            if current.is_empty() {
                eprintln!(
                    "  {}No .contextignore file found. Using server defaults only.",
                    style("ℹ  ").blue()
                );
                eprintln!();
                eprintln!(
                    "  Run {} to create one with sensible defaults.",
                    style("contextstream-mcp exclude reset").cyan()
                );
            } else {
                for pattern in &current {
                    if pattern.starts_with('#') || pattern.is_empty() {
                        eprintln!("  {}", style(pattern).dim());
                    } else {
                        eprintln!("  {}", style(pattern).yellow());
                    }
                }
                eprintln!();
                eprintln!(
                    "  {} {} active pattern(s)",
                    style("→").dim(),
                    current
                        .iter()
                        .filter(|p| !p.starts_with('#') && !p.is_empty())
                        .count()
                );
            }

            eprintln!();
            eprintln!(
                "  {} Server-side protections (.env, keys, credentials) are always active.",
                style("🛡").green()
            );
            eprintln!(
                "  Run {} for details.",
                style("contextstream-mcp exclude info").cyan()
            );
        }

        ExcludeAction::Add => {
            if patterns.is_empty() {
                anyhow::bail!("No patterns specified. Usage: contextstream-mcp exclude add <pattern> [pattern...]");
            }

            let mut current = read_contextignore(&ignore_path);
            let active: std::collections::HashSet<String> = current.iter().cloned().collect();
            let mut added = Vec::new();

            for pattern in &patterns {
                if !active.contains(pattern) {
                    current.push(pattern.clone());
                    added.push(pattern.as_str());
                }
            }

            if added.is_empty() {
                eprintln!(
                    "  {}All patterns already exist in .contextignore",
                    style("ℹ  ").blue()
                );
            } else {
                write_contextignore(&ignore_path, &current)?;
                eprintln!(
                    "\n{} Added {} pattern(s) to .contextignore:",
                    style("✓").green(),
                    added.len()
                );
                for p in &added {
                    eprintln!("  {} {}", style("+").green(), style(p).yellow());
                }
            }
        }

        ExcludeAction::Remove => {
            if patterns.is_empty() {
                anyhow::bail!("No patterns specified. Usage: contextstream-mcp exclude remove <pattern> [pattern...]");
            }

            let current = read_contextignore(&ignore_path);
            if current.is_empty() {
                eprintln!("  {}No .contextignore file found.", style("ℹ  ").blue());
                return Ok(());
            }

            let to_remove: std::collections::HashSet<&str> =
                patterns.iter().map(|s| s.as_str()).collect();
            let mut removed = Vec::new();

            let filtered: Vec<String> = current
                .into_iter()
                .filter(|p| {
                    if to_remove.contains(p.as_str()) {
                        removed.push(p.clone());
                        false
                    } else {
                        true
                    }
                })
                .collect();

            if removed.is_empty() {
                eprintln!(
                    "  {}None of the specified patterns were found in .contextignore",
                    style("ℹ  ").blue()
                );
            } else {
                write_contextignore(&ignore_path, &filtered)?;
                eprintln!(
                    "\n{} Removed {} pattern(s) from .contextignore:",
                    style("✓").green(),
                    removed.len()
                );
                for p in &removed {
                    eprintln!("  {} {}", style("-").red(), style(p).yellow());
                }
            }
        }

        ExcludeAction::Reset => {
            let mut lines = vec![
                "# .contextignore — ContextStream indexing exclusion patterns".to_string(),
                "# Syntax follows .gitignore conventions.".to_string(),
                "# Server-side protections (.env, keys, credentials) are always active."
                    .to_string(),
                "".to_string(),
            ];
            for pattern in DEFAULT_EXCLUDE_PATTERNS {
                lines.push(pattern.to_string());
            }

            write_contextignore(&ignore_path, &lines)?;
            eprintln!(
                "\n{} Reset .contextignore to default patterns ({} patterns)",
                style("✓").green(),
                DEFAULT_EXCLUDE_PATTERNS.len()
            );
            eprintln!("  File: {}", style(ignore_path.display()).dim());
        }

        ExcludeAction::Info => {
            eprintln!(
                "\n{} ContextStream Server-Side Security Protections",
                style("⬡").cyan()
            );
            eprintln!();
            eprintln!(
                "  {}",
                style("These protections are always active and cannot be disabled.").dim()
            );
            eprintln!();

            eprintln!(
                "  {} {}",
                style("■").red(),
                style("Always Blocked Files").bold()
            );
            eprintln!("  Files that are never indexed regardless of settings:");
            let blocked = [
                ".env and all .env.* (except .env.example/.sample/.template/.dist/.defaults)",
                ".pem, .key, .p12, .pfx, .jks",
                "id_rsa, id_ed25519, id_ecdsa, known_hosts",
                "credentials.json, serviceAccountKey.json, .git-credentials",
                ".npmrc, .pypirc, .netrc, .pgpass",
                "credential dirs (skipped entirely): .ssh, .aws, .gnupg, .kube, .docker",
            ];
            for group in &blocked {
                eprintln!("    {}", style(group).yellow());
            }

            eprintln!();
            eprintln!(
                "  {} {}",
                style("■").yellow(),
                style("Content Scanning").bold()
            );
            eprintln!(
                "  All file content is scanned before storage. Detected secrets are redacted:"
            );
            let scanned = [
                "API keys and tokens (sk-, ghp_, AKIA, xox-, etc.)",
                "Private keys (PEM blocks)",
                "Connection strings with embedded credentials",
                "High-entropy strings that look like secrets",
            ];
            for item in &scanned {
                eprintln!("    → {}", style(item).dim());
            }

            eprintln!();
            eprintln!(
                "  {} {}",
                style("■").blue(),
                style("Your Custom Exclusions").bold()
            );
            eprintln!(
                "  In addition to server protections, manage project-specific patterns with:"
            );
            eprintln!(
                "    {}",
                style("contextstream-mcp exclude list|add|remove|reset").cyan()
            );
        }
    }

    Ok(())
}

fn read_contextignore(path: &std::path::Path) -> Vec<String> {
    match std::fs::read_to_string(path) {
        Ok(content) => content.lines().map(|l| l.to_string()).collect(),
        Err(_) => Vec::new(),
    }
}

fn write_contextignore(path: &std::path::Path, lines: &[String]) -> Result<()> {
    let content = lines.join("\n");
    let content = if content.ends_with('\n') {
        content
    } else {
        format!("{}\n", content)
    };
    std::fs::write(path, content)?;
    Ok(())
}

async fn run_post_update_command_with_installed_binary(
    args: &[&str],
    description: &str,
) -> Result<()> {
    let install_path = expected_install_path();
    let status = std::process::Command::new(&install_path)
        .args(args)
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .map_err(|e| {
            anyhow::anyhow!(
                "Failed to run post-update {} with installed binary {}: {}",
                description,
                install_path,
                e
            )
        })?;

    if status.success() {
        Ok(())
    } else {
        anyhow::bail!(
            "Installed binary post-update {} exited with status {}",
            description,
            status
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PostUpdateCommand {
    args: &'static [&'static str],
    description: &'static str,
    start_message: &'static str,
    success_message: &'static str,
    warning_message: &'static str,
}

/// Map `--editors` ids onto known editors. `None` means "no explicit list",
/// which leaves scope resolution to the setup-recorded selection. An unknown
/// id is a hard error rather than a silent skip, so a typo can never widen the
/// target set back to "everything detected".
fn parse_editor_ids(ids: &[String]) -> Result<Option<Vec<setup::editors::Editor>>, String> {
    if ids.is_empty() {
        return Ok(None);
    }

    let mut editors = Vec::with_capacity(ids.len());
    for id in ids {
        let id = id.trim();
        if id.is_empty() {
            return Err(
                "--editors contains an empty editor id; provide at least one supported id"
                    .to_string(),
            );
        }
        match setup::editors::Editor::from_id(id) {
            Some(editor) if !editors.contains(&editor) => editors.push(editor),
            Some(_) => {}
            None => {
                return Err(format!(
                    "Unknown editor id '{}'. Supported ids: {}",
                    id,
                    setup::editors::Editor::all()
                        .iter()
                        .map(|e| e.id())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            }
        }
    }

    Ok(Some(editors))
}

// --only-configured: a self-update must refresh the editors the user chose,
// never expand to whatever happens to be installed on the machine.
const UPDATE_HOOKS_ARGS: &[&str] = &["update-hooks", "--scope=global", "--only-configured"];
const UPDATE_GLOBAL_RULES_ARGS: &[&str] = &["update-rules", "--scope=global", "--only-configured"];
const UPDATE_PROJECT_RULES_ARGS: &[&str] =
    &["update-rules", "--scope=project", "--only-configured"];

fn post_update_editor_refresh_commands(refresh_project_rules: bool) -> Vec<PostUpdateCommand> {
    let mut commands = vec![
        PostUpdateCommand {
            args: UPDATE_HOOKS_ARGS,
            description: "hook refresh",
            start_message: "Refreshing editor hooks",
            success_message: "Hooks refreshed",
            warning_message: "Could not refresh hooks",
        },
        PostUpdateCommand {
            args: UPDATE_GLOBAL_RULES_ARGS,
            description: "global rule refresh",
            start_message: "Refreshing global AI rules",
            success_message: "Global AI rules refreshed",
            warning_message: "Could not refresh global AI rules",
        },
    ];

    if refresh_project_rules {
        commands.push(PostUpdateCommand {
            args: UPDATE_PROJECT_RULES_ARGS,
            description: "project rule refresh",
            start_message: "Refreshing project AI rules",
            success_message: "Project AI rules refreshed",
            warning_message: "Could not refresh project AI rules",
        });
    }

    commands
}

fn update_banner(current: &str) {
    use console::style;

    eprintln!();
    eprintln!("{}", style("ContextStream MCP Update").bold().cyan());
    eprintln!(
        "  {} {}",
        style("Current version").dim(),
        style(format!("v{}", current)).bold()
    );
    eprintln!();
}

fn update_step(message: impl AsRef<str>) {
    use console::style;
    eprintln!("{} {}", style("●").cyan(), style(message.as_ref()).bold());
}

fn update_detail(message: impl AsRef<str>) {
    use console::style;
    eprintln!("  {}", style(message.as_ref()).dim());
}

fn update_ok(message: impl AsRef<str>) {
    use console::style;
    eprintln!("{} {}", style("✓").green(), message.as_ref());
}

fn update_warn(message: impl AsRef<str>) {
    use console::style;
    eprintln!("{} {}", style("!").yellow(), message.as_ref());
}

fn update_skip(message: impl AsRef<str>) {
    use console::style;
    eprintln!("{} {}", style("-").dim(), style(message.as_ref()).dim());
}

fn should_refresh_project_rules_after_update(cwd: &std::path::Path) -> bool {
    if setup::read_project_config(cwd).ok().flatten().is_some() {
        return true;
    }

    if cwd.join(".contextstream").join("rules.md").exists() {
        return true;
    }

    setup::editors::Editor::all().iter().any(|editor| {
        editor
            .all_rules_cleanup_paths(Some(cwd))
            .iter()
            .any(|path| path.exists())
    })
}

async fn refresh_editor_integrations_after_update(
    remote_flow: bool,
    refresh_project_rules: bool,
) -> Result<()> {
    update_step("Editor integrations");

    let mut api_key = setup::get_api_key_result()?;
    if remote_flow && api_key.is_none() {
        update_detail("Authentication is required before remote config migration");
        api_key = Some(setup::ensure_authenticated_api_key().await?);
    }

    if api_key.is_some() {
        update_detail("Ensuring MCP configs use the hosted remote gateway");
        run_post_update_command_with_installed_binary(
            &["migrate-remote", "--scope=all", "--only-configured"],
            "hosted remote migration",
        )
        .await?;
        update_ok("Hosted remote MCP configs refreshed");
    } else {
        update_skip("No saved credentials found; skipped hosted remote config migration");
    }

    for command in post_update_editor_refresh_commands(refresh_project_rules) {
        update_detail(command.start_message);
        match run_post_update_command_with_installed_binary(command.args, command.description).await
        {
            Ok(()) => update_ok(command.success_message),
            Err(e) => update_warn(format!("{}: {}", command.warning_message, e)),
        }
    }

    Ok(())
}

/// Map hook handler names to Claude Code hook event names.
///
/// Claude Code expects `hookSpecificOutput.hookEventName` in hook output.
/// The `HookOutput::context()` helper reads `HOOK_EVENT_NAME` from the
/// environment; this mapping ensures the env var is set before dispatch.
fn hook_event_name(hook_name: &str) -> &str {
    match hook_name {
        "session-start" => "SessionStart",
        "instructions-loaded" => "InstructionsLoaded",
        "session-end" => "SessionEnd",
        "user-prompt-submit" | "on-save-intent" => "UserPromptSubmit",
        "pre-tool-use" => "PreToolUse",
        "post-tool-use" => "PostToolUse",
        "post-tool-use-failure" => "PostToolUseFailure",
        // The Bash observer is a PostToolUse hook; git-* hooks are git-native
        // (not Claude events) and fall through to the identity arm below.
        "git-bash-observed" => "PostToolUse",
        "pre-compact" => "PreCompact",
        "post-compact" => "PostCompact",
        "stop" => "Stop",
        "stop-failure" => "StopFailure",
        "notification" => "Notification",
        "subagent-start" => "SubagentStart",
        "subagent-stop" => "SubagentStop",
        "task-created" => "TaskCreated",
        "task-completed" => "TaskCompleted",
        "teammate-idle" => "TeammateIdle",
        "permission-request" => "PermissionRequest",
        "config-change" => "ConfigChange",
        "cwd-changed" => "CwdChanged",
        "file-changed" => "FileChanged",
        "worktree-create" => "WorktreeCreate",
        "worktree-remove" => "WorktreeRemove",
        "elicitation" => "Elicitation",
        "elicitation-result" => "ElicitationResult",
        other => other,
    }
}

/// Install or remove the managed git hooks (`contextstream-mcp git-hooks`).
///
/// Resolves the repo root from `--path` (or cwd), honors the git-capture
/// kill-switch / per-repo policy on install, and is a no-op outside a git repo.
fn run_git_hooks(path: Option<String>, uninstall: bool) {
    use std::path::PathBuf;

    let start = path
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));

    let Some(root) = setup::git_hooks::resolve_repo_root(&start) else {
        eprintln!(
            "Not a git repository ({}); skipping git hooks.",
            start.display()
        );
        return;
    };
    let root_str = root.to_string_lossy().to_string();

    if uninstall {
        match setup::git_hooks::uninstall_git_hooks(&root) {
            Ok(()) => eprintln!("Removed ContextStream git hooks from {}", root.display()),
            Err(e) => {
                eprintln!("Failed to remove git hooks: {}", e);
                std::process::exit(1);
            }
        }
        return;
    }

    if mcp_server::hook_handlers::git_common::capture_disabled(&root_str) {
        eprintln!(
            "Git capture is disabled for {}; skipping hook install.",
            root.display()
        );
        return;
    }

    match setup::git_hooks::install_git_hooks(&root) {
        Ok(()) => eprintln!(
            "Git capture: hooks installed (post-commit, pre-push, post-checkout, post-merge) in {}",
            root.display()
        ),
        Err(e) => {
            eprintln!("Failed to install git hooks: {}", e);
            std::process::exit(1);
        }
    }
}

async fn run_verify_key(json_output: bool) -> Result<()> {
    let config = match load_config() {
        Ok(c) => c,
        Err(e) => {
            if json_output {
                println!(r#"{{"valid": false, "error": "{}"}}"#, e);
            } else {
                eprintln!("Error: {}", e);
            }
            std::process::exit(1);
        }
    };

    let client = mcp_client::ContextStreamClient::new(config);

    match client.me().await {
        Ok(user) => {
            if json_output {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "valid": true,
                        "user": user
                    }))?
                );
            } else {
                eprintln!("API key is valid!");
                eprintln!("User: {} ({})", user.name.unwrap_or_default(), user.email);
            }
            Ok(())
        }
        Err(e) => {
            if json_output {
                println!(r#"{{"valid": false, "error": "{}"}}"#, e);
            } else {
                eprintln!("API key verification failed: {}", e);
            }
            std::process::exit(1);
        }
    }
}

async fn run_about() {
    use console::style;

    const BUILD_DATE: &str = env!("CONTEXTSTREAM_BUILD_DATE");
    const MCP_URL: &str = "https://mcp.contextstream.io";

    println!();
    println!(
        "{}",
        style("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━").cyan()
    );
    println!(
        "  {} v{}",
        style("ContextStream MCP").bold().cyan(),
        mcp_types::config::VERSION
    );
    println!(
        "{}",
        style("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━").cyan()
    );
    println!();

    // Version & build info
    println!(
        "  {}  {}",
        style("Version").bold(),
        mcp_types::config::VERSION
    );
    println!("  {}    {}", style("Built").bold(), BUILD_DATE);
    #[cfg(target_os = "linux")]
    println!(
        "  {} linux-{}",
        style("Platform").bold(),
        std::env::consts::ARCH
    );
    #[cfg(target_os = "macos")]
    println!(
        "  {} darwin-{}",
        style("Platform").bold(),
        std::env::consts::ARCH
    );
    #[cfg(target_os = "windows")]
    println!(
        "  {} windows-{}",
        style("Platform").bold(),
        std::env::consts::ARCH
    );

    // Transport mode detection
    let transport_marker = setup::setup_transport_marker_path();
    let transport_mode = std::fs::read_to_string(&transport_marker)
        .ok()
        .unwrap_or_else(|| "unknown".to_string());
    let transport_display = match transport_mode.trim() {
        "remote" => "Hosted remote (HTTPS → edge → origin)",
        "local" => "Local binary (recovery override)",
        _ => "Not configured (run setup)",
    };
    println!("  {} {}", style("Transport").bold(), transport_display);

    // Region detection via edge latency
    println!();
    println!("  {}", style("Connectivity").bold());

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();

    let start = std::time::Instant::now();
    let resp = client.get(format!("{}/health", MCP_URL)).send().await;
    let latency = start.elapsed();

    match resp {
        Ok(r) if r.status().is_success() => {
            let cf_ray = r
                .headers()
                .get("cf-ray")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            let pop = if cf_ray.len() >= 3 {
                &cf_ray[cf_ray.len().saturating_sub(3)..]
            } else {
                "unknown"
            };
            println!(
                "    Edge POP:   {} ({}ms)",
                style(pop).cyan(),
                latency.as_millis()
            );
            println!("    MCP URL:    {}", style(MCP_URL).dim());
        }
        Ok(r) => {
            println!(
                "    Edge:       {} (HTTP {}; {}ms)",
                style("reachable").yellow(),
                r.status(),
                latency.as_millis()
            );
        }
        Err(e) => {
            println!("    Edge:       {} ({})", style("unreachable").red(), e);
        }
    }

    // Security section
    println!();
    println!("  {}", style("Security").bold());
    println!("    Remote MCP access is protected with end-to-end HTTPS.");
    println!("    Traffic is encrypted from your machine to our Cloudflare");
    println!("    edge, then encrypted again from the edge to the MCP origin");
    println!("    with strict TLS certificate validation on every hop.");
    println!();
    println!(
        "    {}",
        style("Your requests never travel over plaintext at any point in transit.").dim()
    );

    println!();
}

async fn run_configure(
    list_options: bool,
    transcripts: Option<ToggleValue>,
    hook_transcripts: Option<ToggleValue>,
    scope: ConfigureScope,
    only: Option<&[setup::editors::Editor]>,
    only_configured: bool,
) -> Result<()> {
    if list_options {
        print_configure_options();
        return Ok(());
    }

    if transcripts.is_some() || hook_transcripts.is_some() {
        let transcripts_enabled = transcripts.map(ToggleValue::as_bool);
        let hook_transcripts_enabled = hook_transcripts
            .map(ToggleValue::as_bool)
            .or(transcripts_enabled);
        setup::update_transcript_defaults_scoped(
            scope.as_str(),
            transcripts_enabled,
            hook_transcripts_enabled,
            only,
            only_configured,
        )
        .await?;

        eprintln!("Transcript defaults updated (scope: {}).", scope.as_str());
        if let Some(value) = transcripts_enabled {
            eprintln!(
                "  CONTEXTSTREAM_TRANSCRIPTS_ENABLED={}",
                if value { "true" } else { "false" }
            );
        }
        if let Some(value) = hook_transcripts_enabled {
            eprintln!(
                "  CONTEXTSTREAM_HOOK_TRANSCRIPTS_ENABLED={}",
                if value { "true" } else { "false" }
            );
        }
        return Ok(());
    }

    if list_options {
        return Ok(());
    }

    use dialoguer::Select;

    let options = [
        "Editors        — Configure editor integrations (hooks, rules, MCP configs)",
        "Transcripts    — Set default transcript capture policy (on/off)",
        "API Key        — Update authentication credentials",
        "Workspace      — Select or create a workspace",
        "Hooks          — Reinstall hooks for all editors",
        "Rules          — Regenerate AI rules for all editors",
        "MCP Configs    — Regenerate MCP server configs for all editors",
        "Exit           — Leave configure menu",
    ];

    loop {
        eprintln!();
        let selection = Select::new()
            .with_prompt("What would you like to configure?")
            .items(&options)
            .default(0)
            .interact_opt()?;

        let selection = match selection {
            Some(s) => s,
            None => {
                eprintln!("Done.");
                return Ok(());
            }
        };

        match selection {
            // Editors
            0 => {
                run_configure_editors_flow().await?;
            }
            // Transcripts
            1 => {
                run_configure_transcript_defaults().await?;
            }
            // API Key
            2 => {
                let (api_key, email) = setup::authenticate().await?;
                setup::write_saved_credentials(&api_key, None)?;
                eprintln!("Credentials saved. Authenticated as {}.", email);
            }
            // Workspace
            3 => {
                let config = load_config()?;
                let client = mcp_client::ContextStreamClient::new(config);
                match setup::setup_workspace(&client).await? {
                    Some(ws) => eprintln!("Workspace set to: {} ({})", ws.name, ws.id),
                    None => eprintln!("No workspace selected."),
                }
            }
            // Hooks
            4 => {
                setup::update_hooks("all", None).await?;
                eprintln!("Hooks updated.");
            }
            // Rules
            5 => {
                setup::update_rules("all", None, None).await?;
                eprintln!("Rules updated.");
            }
            // MCP Configs
            6 => {
                setup::update_configs("all").await?;
                eprintln!("MCP configs updated.");
            }
            // Exit
            7 => {
                eprintln!("Done.");
                return Ok(());
            }
            _ => unreachable!(),
        }
    }
}

fn print_configure_options() {
    eprintln!("Configurable options:");
    eprintln!("  - editors: editor integrations, hooks, rules, and MCP configs");
    eprintln!("  - api-key: authentication credentials");
    eprintln!("  - workspace: select/create default workspace");
    eprintln!("  - hooks: reinstall hook scripts");
    eprintln!("  - rules: regenerate AI rule files");
    eprintln!("  - mcp-configs: regenerate MCP config files");
    eprintln!("  - transcripts: default transcript policy for new chats");
    eprintln!();
    eprintln!("Quick transcript commands:");
    eprintln!("  contextstream-mcp configure --transcripts off");
    eprintln!("  contextstream-mcp configure --transcripts on");
    eprintln!("  contextstream-mcp configure --transcripts on --hook-transcripts off");
    eprintln!("  contextstream-mcp configure --transcripts on --scope global");
}

async fn run_configure_transcript_defaults() -> Result<()> {
    use dialoguer::Select;

    let options = [
        "Off (recommended) — transcripts only when a chat opts in via save_exchange=true",
        "On                — transcripts saved by default for context + hook flows",
        "Back              — return to configure menu",
    ];

    let selection = Select::new()
        .with_prompt("Default transcript behavior")
        .items(&options)
        .default(0)
        .interact_opt()?;

    let selection = match selection {
        Some(s) => s,
        None => return Ok(()),
    };

    match selection {
        0 => {
            setup::update_transcript_defaults("all", Some(false), Some(false)).await?;
            eprintln!("Transcript defaults set to off.");
        }
        1 => {
            setup::update_transcript_defaults("all", Some(true), Some(true)).await?;
            eprintln!("Transcript defaults set to on.");
        }
        2 => {}
        _ => unreachable!(),
    }

    Ok(())
}

async fn run_configure_editors_flow() -> Result<()> {
    let detected_editors = setup::editors::detect_installed_editors();
    if detected_editors.is_empty() {
        eprintln!("No editors auto-detected. You can still choose editors manually.");
    }

    // Need credentials and workspace for editor configuration
    let mut config = load_config().unwrap_or_default();
    let mut api_key = config.api_key.clone().unwrap_or_default();
    if api_key.is_empty() {
        let choice = setup::select(
            "No API key configured. What would you like to do?",
            &["Configure API Key now", "Back to configure menu"],
        )?;
        if choice == 1 {
            return Ok(());
        }

        let (new_api_key, email) = setup::authenticate().await?;
        setup::write_saved_credentials(&new_api_key, None)?;
        eprintln!("Credentials saved. Authenticated as {}.", email);
        api_key = new_api_key.clone();
        config.api_key = Some(new_api_key);
    }

    let client = mcp_client::ContextStreamClient::new(config);
    let cwd = std::env::current_dir()?;
    let project_path = setup::setup_path_is_project_candidate(&cwd).then_some(cwd.as_path());
    if project_path.is_none() {
        eprintln!(
            "Current directory is not a safely scoped project folder; configure will update editor-global surfaces only. HOME and filesystem roots require a narrower folder."
        );
    }

    let mut selected_editors = setup::select_editors(&detected_editors)?;
    let mut transport_preference = if selected_editors.is_empty() {
        setup::SetupTransportPreference::HostedRemote
    } else {
        setup::prompt_setup_transport_preference(&selected_editors)?
    };

    let mut workspace = setup::setup_workspace(&client).await?;
    let mut selected_project = if let Some(project_path) = project_path {
        setup::select_project_for_current_directory(
            &client,
            project_path,
            workspace.as_ref(),
            true,
            true,
            true,
        )
        .await?
    } else {
        None
    };

    loop {
        let editors_summary = selected_editors
            .iter()
            .map(|e| e.display_name())
            .collect::<Vec<_>>()
            .join(", ");
        let workspace_summary = workspace
            .as_ref()
            .map(|w| format!("{} ({})", w.name, w.id))
            .unwrap_or_else(|| "None (skipped)".to_string());
        let project_summary = selected_project
            .as_ref()
            .map(|project| format!("{} ({})", project.name, project.id))
            .unwrap_or_else(|| "None (skipped)".to_string());

        eprintln!();
        eprintln!("Editors:   {}", editors_summary);
        eprintln!(
            "Connection: {}",
            match transport_preference {
                setup::SetupTransportPreference::HostedRemote => "Hosted remote gateway",
                setup::SetupTransportPreference::LocalBinary => "Local binary (recovery override)",
            }
        );
        eprintln!("Workspace: {}", workspace_summary);
        eprintln!("Project:   {}", project_summary);

        let action = setup::select(
            "Review editor configuration:",
            &[
                "Apply configuration",
                "Change editors",
                "Change workspace",
                "Change project",
                "Back to configure menu",
            ],
        )?;

        match action {
            0 => {
                setup::persist_setup_editor_selection(&selected_editors)?;
                if selected_editors.is_empty() {
                    eprintln!(
                        "Editor selection saved: none. Existing editor files were untouched."
                    );
                    return Ok(());
                }
                setup::write_setup_transport_marker(transport_preference)?;
                for editor in &selected_editors {
                    setup::configure_editor_with_workspace(
                        &client,
                        editor,
                        &api_key,
                        workspace.as_ref(),
                        selected_project.as_ref().map(|project| project.id.as_str()),
                        selected_project.as_ref().and(project_path),
                        transport_preference,
                        false,
                    )
                    .await?;
                }
                tokio::spawn({
                    let client = client.clone();
                    async move {
                        setup::report_setup_telemetry(&client, transport_preference).await;
                    }
                });
                eprintln!("\nEditors configured successfully.");
                return Ok(());
            }
            1 => {
                selected_editors = setup::select_editors(&detected_editors)?;
                if !selected_editors.is_empty() {
                    transport_preference =
                        setup::prompt_setup_transport_preference(&selected_editors)?;
                }
            }
            2 => {
                workspace = setup::setup_workspace(&client).await?;
                selected_project = if let Some(project_path) = project_path {
                    setup::select_project_for_current_directory(
                        &client,
                        project_path,
                        workspace.as_ref(),
                        true,
                        true,
                        true,
                    )
                    .await?
                } else {
                    None
                };
            }
            3 => {
                selected_project = if let Some(project_path) = project_path {
                    setup::select_project_for_current_directory(
                        &client,
                        project_path,
                        workspace.as_ref(),
                        true,
                        true,
                        true,
                    )
                    .await?
                } else {
                    None
                };
            }
            4 => return Ok(()),
            _ => unreachable!(),
        }
    }
}

/// Simple semver comparison: returns true if `a` is newer than `b`.
fn is_version_newer(a: &str, b: &str) -> bool {
    let parse = |s: &str| -> (u64, u64, u64) {
        let mut parts = s.split('.').filter_map(|p| p.parse::<u64>().ok());
        (
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
            parts.next().unwrap_or(0),
        )
    };
    parse(a) > parse(b)
}

/// Get the expected install path for the current platform.
fn expected_install_path() -> String {
    if cfg!(windows) {
        if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
            let p = std::path::PathBuf::from(local_app_data)
                .join("ContextStream")
                .join("contextstream-mcp.exe");
            return p.to_string_lossy().to_string();
        }
        // Fallback for Windows if LOCALAPPDATA is not set
        if let Some(home) = dirs::home_dir() {
            return home
                .join("AppData")
                .join("Local")
                .join("ContextStream")
                .join("contextstream-mcp.exe")
                .to_string_lossy()
                .to_string();
        }
        "contextstream-mcp.exe".to_string()
    } else {
        "/usr/local/bin/contextstream-mcp".to_string()
    }
}

/// Binary name with platform-appropriate extension.
fn binary_name() -> &'static str {
    if cfg!(windows) {
        "contextstream-mcp.exe"
    } else {
        "contextstream-mcp"
    }
}

/// After an update, verify the PATH-resolved binary matches the expected install
/// location. If a stale copy shadows the new install, attempt to remove it
/// (user-writable) or warn the user (system paths).
/// Returns the verified version string from the resolved binary, or None on failure.
fn verify_and_fix_binary_path(_expected_version: &str) -> Option<String> {
    let expected_install = expected_install_path();
    let resolved = match which::which("contextstream-mcp") {
        Ok(p) => p,
        Err(_) => {
            eprintln!(
                "Warning: contextstream-mcp not found on PATH after install. \
                 Ensure {} is in your PATH.",
                std::path::Path::new(&expected_install)
                    .parent()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "the install directory".to_string())
            );
            return None;
        }
    };

    let expected_path = std::path::Path::new(&expected_install);

    // Canonicalize both paths to handle symlinks
    let resolved_canonical = std::fs::canonicalize(&resolved).unwrap_or_else(|_| resolved.clone());
    let expected_canonical =
        std::fs::canonicalize(expected_path).unwrap_or_else(|_| expected_path.to_path_buf());

    if resolved_canonical == expected_canonical {
        return query_binary_version(&resolved.to_string_lossy());
    }

    // PATH resolves to a different binary — shadowing detected
    let resolved_str = resolved.to_string_lossy();
    eprintln!(
        "Warning: PATH resolves contextstream-mcp to '{}' instead of '{}'.",
        resolved_str, expected_install
    );

    if is_user_home_path(&resolved) {
        eprintln!(
            "Removing stale binary at '{}' (shadows the updated install)...",
            resolved_str
        );
        match std::fs::remove_file(&resolved) {
            Ok(()) => {
                eprintln!("Removed stale binary successfully.");
                // Re-verify after removal
                match which::which("contextstream-mcp") {
                    Ok(new_resolved) => {
                        let new_canonical = std::fs::canonicalize(&new_resolved)
                            .unwrap_or_else(|_| new_resolved.clone());
                        if new_canonical == expected_canonical {
                            eprintln!(
                                "PATH now correctly resolves to '{}'.",
                                new_resolved.display()
                            );
                            return query_binary_version(&new_resolved.to_string_lossy());
                        }
                        eprintln!(
                            "Warning: After removal, PATH still resolves to '{}'. \
                             Multiple stale copies may exist. Run: type -a contextstream-mcp",
                            new_resolved.display()
                        );
                    }
                    Err(_) => {
                        eprintln!(
                            "Warning: contextstream-mcp no longer found on PATH after \
                             removing stale copy. Ensure the install directory is in your PATH."
                        );
                    }
                }
            }
            Err(e) => {
                eprintln!(
                    "Could not remove stale binary at '{}': {}.\n\
                     To fix manually, run:\n  rm '{}'\n  hash -r",
                    resolved_str, e, resolved_str
                );
            }
        }
    } else {
        eprintln!(
            "A stale binary at '{}' is shadowing the updated install at '{}'.\n\
             To fix, remove the stale binary and refresh your shell.",
            resolved_str, expected_install
        );
    }

    // Even if we couldn't fix shadowing, query the expected install path directly
    if expected_path.exists() {
        return query_binary_version(&expected_install);
    }

    None
}

/// Check if a path is inside the user's home directory.
fn is_user_home_path(path: &std::path::Path) -> bool {
    if let Some(home) = dirs::home_dir() {
        return path.starts_with(&home);
    }
    let s = path.to_string_lossy();
    s.contains("/.local/bin/") || s.contains("/.cargo/bin/")
}

/// Query the version of a specific binary by absolute path.
fn query_binary_version(binary_path: &str) -> Option<String> {
    let output = std::process::Command::new(binary_path)
        .arg("--version")
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        None
    }
}

async fn run_update(check_only: bool, force: bool, remote: bool) -> Result<()> {
    const LATEST_VERSION_URL: &str =
        "https://pub-68429b9f7857416c9484b75bf1887b96.r2.dev/mcp/latest/version.json";
    const SETUP_URL: &str = "https://contextstream.io/scripts/setup.sh";

    let launcher_managed = std::env::var("CONTEXTSTREAM_DISABLE_SELF_UPDATE")
        .ok()
        .is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on" | "enabled"
            )
        });
    if launcher_managed && !check_only {
        anyhow::bail!(
            "This ContextStream runtime is managed by the npm package launcher. Update the pinned @contextstream/mcp-server package version instead of mutating its verified cache."
        );
    }

    let current = mcp_types::config::VERSION;
    update_banner(current);
    let remote_flow = remote;
    let refresh_project_rules = std::env::current_dir()
        .ok()
        .as_deref()
        .is_some_and(should_refresh_project_rules_after_update);

    // Fetch latest version manifest
    update_step("Version check");
    update_detail("Fetching latest release metadata");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    let resp = client.get(LATEST_VERSION_URL).send().await;
    let latest = match resp {
        Ok(r) if r.status().is_success() => {
            let body: serde_json::Value = r.json().await?;
            body.get("version")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        }
        Ok(r) => {
            update_warn(format!("Version check returned status {}", r.status()));
            None
        }
        Err(e) => {
            update_warn(format!("Could not check latest version: {}", e));
            None
        }
    };

    let latest = match latest {
        Some(v) => {
            update_ok(format!("Latest release: v{}", v));
            v
        }
        None => {
            if force {
                update_warn("Could not determine latest version; proceeding with --force");
                String::new()
            } else {
                anyhow::bail!(
                    "Could not determine latest version. Use --force to reinstall anyway."
                );
            }
        }
    };

    if !latest.is_empty() {
        let is_newer = is_version_newer(&latest, current);
        if check_only {
            if is_newer {
                update_warn(format!("Update available: v{} -> v{}", current, latest));
                update_detail("Run `contextstream-mcp update` to install it");
            } else {
                update_ok(format!("Already on latest version (v{})", current));
            }
            return Ok(());
        }
        if !is_newer && !force {
            update_ok(format!("Already up to date (v{})", current));
            refresh_editor_integrations_after_update(remote_flow, refresh_project_rules).await?;
            update_ok("Update check complete");
            return Ok(());
        }
        if is_newer {
            update_step(format!("Install v{}", latest));
            update_detail(format!("Updating from v{}", current));
        } else {
            update_step(format!("Reinstall v{}", latest));
        }
    } else {
        update_step("Reinstall latest available build");
    }

    // Download and install the latest binary
    update_detail("Downloading and installing binary");

    if cfg!(windows) {
        // Windows: download binary directly from CDN
        download_and_install_binary(&client, &latest).await?;
    } else {
        // Unix: use setup shell script
        let mut cmd = std::process::Command::new("bash");
        cmd.arg("-c")
            .arg(format!("curl -fsSL '{}' | bash", SETUP_URL))
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .env("CONTEXTSTREAM_INSTALL_SKIP_SETUP", "true");
        let status = cmd.status();

        match status {
            Ok(s) if s.success() => {}
            Ok(s) => {
                anyhow::bail!("Install script exited with status: {}", s);
            }
            Err(e) => {
                anyhow::bail!(
                    "Failed to run install script: {}. Ensure 'curl' and 'bash' are available.",
                    e
                );
            }
        }
    }

    // Verify the install: check for PATH shadowing and report actual version
    let verified_version = verify_and_fix_binary_path(&latest);
    let install_path = expected_install_path();

    match verified_version {
        Some(version_str) => {
            update_ok(format!("Installed {}", version_str));
            if !latest.is_empty() && !version_str.contains(&latest) {
                update_warn(format!(
                    "Expected version v{} but resolved binary reports: {}",
                    latest, version_str
                ));
            }
        }
        None => {
            if std::path::Path::new(&install_path).exists() {
                update_warn(format!(
                    "Update installed to {}. PATH may not resolve correctly.\n\
                     Verify with: {} --version",
                    install_path, install_path
                ));
            } else {
                update_warn("Update may have failed. Run `contextstream-mcp --version` to verify");
            }
        }
    }

    refresh_editor_integrations_after_update(remote_flow, refresh_project_rules).await?;
    update_ok("Update complete");

    Ok(())
}

/// Download and install binary directly from CDN (used on Windows).
///
/// Downloads the platform-appropriate binary from the R2 CDN and installs it
/// to the expected location. On Windows, handles the case where the target
/// binary may be locked by renaming it first.
const MCP_RELEASE_BASE_URL: &str = "https://pub-68429b9f7857416c9484b75bf1887b96.r2.dev/mcp";

fn release_binary_url(version: &str, file_name: &str) -> String {
    let channel = if version.is_empty() {
        "latest".to_string()
    } else {
        format!("v{version}")
    };
    format!("{MCP_RELEASE_BASE_URL}/{channel}/{file_name}")
}

async fn download_and_install_binary(client: &reqwest::Client, version: &str) -> Result<()> {
    let (arch_key, file_name) = if cfg!(windows) {
        if cfg!(target_arch = "x86_64") {
            ("win-x64", "contextstream-mcp-win-x64.exe")
        } else if cfg!(target_arch = "aarch64") {
            ("win-arm64", "contextstream-mcp-win-arm64.exe")
        } else {
            anyhow::bail!("Unsupported Windows architecture");
        }
    } else if cfg!(target_os = "macos") {
        if cfg!(target_arch = "aarch64") {
            ("darwin-arm64", "contextstream-mcp-darwin-arm64")
        } else {
            ("darwin-x64", "contextstream-mcp-darwin-x64")
        }
    } else {
        if cfg!(target_arch = "aarch64") {
            ("linux-arm64", "contextstream-mcp-linux-arm64")
        } else {
            ("linux-x64", "contextstream-mcp-linux-x64")
        }
    };

    // Pin the artifact to the exact version selected from the manifest. Using
    // the mutable `latest` path here could cross a release promotion boundary
    // between the manifest fetch and the binary download.
    let download_url = release_binary_url(version, file_name);
    eprintln!("Downloading {} binary from CDN...", arch_key);

    let response = client
        .get(&download_url)
        .timeout(std::time::Duration::from_secs(120))
        .send()
        .await?;

    if !response.status().is_success() {
        anyhow::bail!(
            "Failed to download binary: HTTP {} from {}",
            response.status(),
            download_url
        );
    }

    let bytes = response.bytes().await?;
    if bytes.len() < 1024 {
        anyhow::bail!(
            "Downloaded binary is suspiciously small ({} bytes)",
            bytes.len()
        );
    }

    let install_path = std::path::PathBuf::from(expected_install_path());

    // Ensure parent directory exists
    if let Some(parent) = install_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // On Windows, a running binary can't be overwritten directly.
    // Rename the existing binary to .old first, then write the new one.
    let old_path = install_path.with_extension("old.exe");
    if install_path.exists() {
        // Remove any previous .old file
        let _ = std::fs::remove_file(&old_path);
        // Try to rename the current binary
        match std::fs::rename(&install_path, &old_path) {
            Ok(()) => {
                eprintln!("Renamed existing binary to {}", old_path.display());
            }
            Err(e) => {
                // If rename fails (binary is locked by running MCP server), try direct overwrite
                eprintln!(
                    "Warning: Could not rename existing binary ({}). Attempting direct write...",
                    e
                );
            }
        }
    }

    // Write the new binary
    std::fs::write(&install_path, &bytes).map_err(|e| {
        anyhow::anyhow!(
            "Failed to write binary to {}: {}. \
             If the binary is locked by a running MCP server, \
             restart your editor and try again.",
            install_path.display(),
            e
        )
    })?;

    // Set executable permissions on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&install_path, std::fs::Permissions::from_mode(0o755))?;
    }

    // Clean up old binary
    let _ = std::fs::remove_file(&old_path);

    eprintln!(
        "Installed to {} ({} bytes)",
        install_path.display(),
        bytes.len()
    );

    // Verify the new binary runs
    let output = std::process::Command::new(&install_path)
        .arg("--version")
        .output();

    match output {
        Ok(o) if o.status.success() => {
            let version = String::from_utf8_lossy(&o.stdout);
            eprintln!("Verified: {}", version.trim());
        }
        Ok(o) => {
            eprintln!(
                "Warning: Installed binary exited with code {}",
                o.status.code().unwrap_or(-1)
            );
        }
        Err(e) => {
            eprintln!("Warning: Could not verify installed binary: {}", e);
        }
    }

    Ok(())
}

/// Unified index/ingest: authenticate, resolve workspace & project interactively,
/// then index files using the same multi-phase progress flow as the setup wizard.
async fn run_index_or_ingest(
    path: Option<String>,
    include_media: bool,
    background: bool,
    force: bool,
) -> Result<()> {
    use console::style;

    let requested_path = match path {
        Some(p) => std::path::PathBuf::from(p),
        None => std::env::current_dir()?,
    };

    if !requested_path.exists() {
        anyhow::bail!("Path does not exist: {}", requested_path.display());
    }
    if !requested_path.is_dir() {
        anyhow::bail!("Path must be a directory: {}", requested_path.display());
    }
    let project_path = std::fs::canonicalize(&requested_path).map_err(|error| {
        anyhow::anyhow!(
            "Could not resolve project path {}: {}",
            requested_path.display(),
            error
        )
    })?;

    // P0 ingestion-containment: refuse over-broad / sensitive roots ($HOME,
    // home ancestors, `/`, `.ssh`/`.aws`/...) unless the operator opts in via
    // CONTEXTSTREAM_ALLOW_BROAD_INGEST=1.
    match mcp_client::validate_ingest_root(
        &project_path,
        &mcp_client::IngestRootOptions::from_env(),
    ) {
        Ok(assessment) => {
            for warning in assessment.warnings {
                eprintln!("  {}{}", style("⚠  ").yellow(), style(warning).dim());
            }
        }
        Err(rejection) => anyhow::bail!("{}", rejection.message()),
    }

    let project_name = project_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");

    eprintln!(
        "\n{} Index '{}' with ContextStream",
        style("⬡").cyan(),
        style(project_name).cyan()
    );
    eprintln!(
        "  {}",
        style("Indexing scans your project files, generates embeddings, and builds").dim()
    );
    eprintln!(
        "  {}",
        style("a searchable code graph. This powers semantic search, impact analysis,").dim()
    );
    eprintln!(
        "  {}",
        style("and context packs — so your AI actually understands your codebase.").dim()
    );
    eprintln!();

    // Ensure we have valid credentials
    let config = match load_config() {
        Ok(c) if c.api_key.as_ref().is_none_or(|k| k.is_empty()) => {
            eprintln!(
                "  {}No API key found. Authenticating...",
                style("ℹ  ").blue()
            );
            let api_key = setup::ensure_authenticated_api_key().await?;
            mcp_types::config::Config {
                api_key: Some(api_key),
                ..Default::default()
            }
        }
        Ok(c) => c,
        Err(_) => {
            eprintln!(
                "  {}No credentials found. Authenticating...",
                style("ℹ  ").blue()
            );
            let api_key = setup::ensure_authenticated_api_key().await?;
            mcp_types::config::Config {
                api_key: Some(api_key),
                ..Default::default()
            }
        }
    };

    let default_workspace_id = config.default_workspace_id;
    let client = mcp_client::ContextStreamClient::new(config);

    // A present-but-invalid local config is a hard boundary. Treating it as
    // "no config" would allow a copied checkout to inherit global defaults
    // and upload (or delete) files in an unrelated project's index.
    let config_path = setup::project_config_path(&project_path);
    let configured_binding = match setup::read_project_config(&project_path) {
        Ok(config) => config,
        Err(error) if config_path.exists() => anyhow::bail!(
            "Invalid project binding at {}: {}. Run `contextstream-mcp setup` to repair it before indexing.",
            config_path.display(),
            error
        ),
        Err(_) => None,
    };

    let folder_mapping =
        mcp_session::auto_init::resolve_workspace(project_path.to_string_lossy().as_ref()).await;
    if configured_binding.is_some() && folder_mapping.is_none() {
        anyhow::bail!(
            "The project binding at {} is untrusted or conflicts with this checkout's machine-local mapping. Run `contextstream-mcp setup` to rebind it; no files were uploaded.",
            config_path.display()
        );
    }

    let mapped_project_id = folder_mapping
        .as_ref()
        .and_then(|mapping| mapping.project_id);
    let local_index_project_id = mcp_client::ContextStreamClient::tracked_project_id_for_folder(
        project_path.to_string_lossy().as_ref(),
    );
    if let (Some(mapped), Some(indexed)) = (mapped_project_id, local_index_project_id) {
        if mapped != indexed {
            anyhow::bail!(
                "Conflicting project bindings for {} (folder mapping {}, local index {}). Run `contextstream-mcp setup` to repair the scope; no files were uploaded.",
                project_path.display(),
                mapped,
                indexed
            );
        }
    }
    let candidate_project_id = mapped_project_id.or(local_index_project_id);

    // Validate the project before using it to resolve a workspace. Any API
    // error is a hard stop: a timeout/5xx is not evidence that the binding is
    // stale and must never trigger an automatic rebind or project creation.
    let validated_project = match candidate_project_id {
        Some(candidate) => Some(client.get_project_fresh(candidate).await.map_err(|error| {
            anyhow::anyhow!(
                "Could not validate project binding {}: {}. No files were uploaded; retry when the API is reachable or run setup to repair a deleted binding.",
                candidate,
                error
            )
        })?),
        None => None,
    };

    let mapped_workspace_id = folder_mapping.as_ref().map(|mapping| mapping.workspace_id);
    if let (Some(expected), Some(project)) = (mapped_workspace_id, validated_project.as_ref()) {
        setup::require_project_workspace_ownership(
            &client,
            project.id,
            project.workspace_id,
            expected,
        )
        .await
        .map_err(|error| {
            anyhow::anyhow!(
                "{}. Run setup to repair the binding; no files were uploaded.",
                error
            )
        })?;
    }

    let workspace_id = mapped_workspace_id
        .or_else(|| {
            validated_project
                .as_ref()
                .and_then(|project| project.workspace_id)
        })
        .or(default_workspace_id);
    let workspace = match workspace_id {
        Some(id) => {
            let server_workspace = client.get_workspace(id).await.map_err(|error| {
                anyhow::anyhow!(
                    "Could not validate workspace binding {}: {}. No files were uploaded.",
                    id,
                    error
                )
            })?;
            Some(setup::WorkspaceInfo {
                id: server_workspace.id.to_string(),
                name: server_workspace.name,
            })
        }
        None => {
            eprintln!(
                "  {}No trusted workspace mapping found. Starting workspace setup...",
                style("ℹ  ").blue()
            );
            setup::setup_workspace(&client).await?
        }
    };

    let resolved_project_id = if let Some(project) = validated_project.as_ref() {
        Some(project.id)
    } else {
        let selected = setup::select_project_for_current_directory(
            &client,
            &project_path,
            workspace.as_ref(),
            false,
            true,
            false,
        )
        .await?;
        selected.and_then(|p| uuid::Uuid::parse_str(&p.id).ok())
    };
    if resolved_project_id.is_none() {
        anyhow::bail!(
            "No trusted project could be resolved for {}. Run `contextstream-mcp setup` to select one; no files were uploaded.",
            project_path.display()
        );
    }

    // Explicit CLI indexing may establish/rebind the checkout, but only after
    // an uncached API ownership check. Ordinary init/hooks are refresh-only.
    if let (Some(resolved_id), Some(resolved_workspace)) = (resolved_project_id, workspace.as_ref())
    {
        let resolved_project = client.get_project_fresh(resolved_id).await.map_err(|error| {
            anyhow::anyhow!(
                "Could not revalidate resolved project {} before ingest: {}. No files were uploaded.",
                resolved_id,
                error
            )
        })?;
        let resolved_workspace_id =
            uuid::Uuid::parse_str(&resolved_workspace.id).map_err(|_| {
                anyhow::anyhow!(
                    "Resolved workspace ID is invalid: {}. No files were uploaded.",
                    resolved_workspace.id
                )
            })?;
        setup::require_project_workspace_ownership(
            &client,
            resolved_id,
            resolved_project.workspace_id,
            resolved_workspace_id,
        )
        .await
        .map_err(|error| anyhow::anyhow!("{}. No files were uploaded.", error))?;

        mcp_session::auto_init::establish_folder_binding(
            project_path.to_string_lossy().as_ref(),
            resolved_workspace_id,
            &resolved_workspace.name,
            Some(resolved_id),
            Some(&resolved_project.name),
        )
        .await
        .map_err(|error| {
            anyhow::anyhow!(
                "Could not establish the validated folder binding: {}. No files were uploaded.",
                error
            )
        })?;
    }

    if background {
        let bg_client = client.clone();
        let bg_path = project_path.clone();
        let bg_ws_id = workspace.as_ref().map(|w| w.id.clone());
        let bg_project_id = resolved_project_id;
        let project_name_owned = project_name.to_string();

        let status_file = dirs::home_dir()
            .map(|h| h.join(".contextstream").join("index-status.txt"))
            .unwrap_or_else(|| std::env::temp_dir().join("contextstream-index-status.txt"));

        if let Some(parent) = status_file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let _ = std::fs::write(
            &status_file,
            format!(
                "Status: Index update in progress\nProject: {}\nStarted: {}\n",
                project_name_owned,
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
            ),
        );

        eprintln!(
            "  {}Index update is running in the background",
            style("ℹ  ").blue()
        );
        eprintln!("    You'll get a desktop notification when complete");
        eprintln!("    Status file: {}", style(status_file.display()).dim());

        let status_file_clone = status_file.clone();
        let project_name_clone = project_name_owned.clone();
        tokio::spawn(async move {
            let result = setup::index_project_background(
                &bg_client,
                &bg_path,
                bg_ws_id.as_ref(),
                bg_project_id,
                include_media,
                force,
            )
            .await;

            let (status_msg, notification_title, notification_body) = match result {
                Ok(outcome)
                    if outcome.committed
                        && outcome.scan_complete
                        && outcome.files_deferred == 0 =>
                {
                    setup::warmup_context(&bg_client, bg_ws_id.as_ref()).await;
                    (
                        format!(
                            "Status: Complete\nProject: {}\nFiles indexed: {}\nCompleted: {}\n",
                            project_name_clone,
                            outcome.files_indexed,
                            chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
                        ),
                        "ContextStream Index Update Complete".to_string(),
                        format!(
                            "{}: {} files indexed",
                            project_name_clone, outcome.files_indexed
                        ),
                    )
                }
                Ok(outcome) if outcome.pending_jobs > 0 => (
                    format!(
                        "Status: Index update in progress\nProject: {}\nFiles indexed so far: {}\nPending jobs: {}\nAccepted: {}\n",
                        project_name_clone,
                        outcome.files_indexed,
                        outcome.pending_jobs,
                        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
                    ),
                    "ContextStream Index Update Continues in Background".to_string(),
                    format!(
                        "{}: accepted; {} server job(s) still indexing",
                        project_name_clone, outcome.pending_jobs
                    ),
                ),
                Ok(outcome) => (
                    format!(
                        "Status: Incomplete\nProject: {}\nFiles indexed so far: {}\nFiles deferred: {}\nScan complete: {}\nFinished: {}\n",
                        project_name_clone,
                        outcome.files_indexed,
                        outcome.files_deferred,
                        outcome.scan_complete,
                        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
                    ),
                    "ContextStream Index Update Incomplete".to_string(),
                    format!(
                        "{}: coverage is incomplete ({} deferred file(s)); review the status file",
                        project_name_clone, outcome.files_deferred
                    ),
                ),
                Err(e) => (
                    format!(
                        "Status: Failed\nProject: {}\nError: {}\nCompleted: {}\n",
                        project_name_clone,
                        e,
                        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
                    ),
                    "ContextStream Index Update Failed".to_string(),
                    format!("{}: {}", project_name_clone, e),
                ),
            };

            let _ = std::fs::write(&status_file_clone, status_msg);
            setup::send_desktop_notification(&notification_title, &notification_body);
            print!("\x07");
        });

        // Wait briefly so the spawned task gets started before process exits
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    } else {
        // Foreground index with multi-phase progress bars
        match setup::index_project(
            &client,
            &project_path,
            workspace.as_ref().map(|w| &w.id),
            resolved_project_id,
            include_media,
            force,
        )
        .await
        {
            Ok(()) => {
                setup::warmup_context(&client, workspace.as_ref().map(|w| &w.id)).await;
                eprintln!(
                    "\n  Semantic search is now available via search(mode=\"auto\", query=\"...\") in your editor."
                );
            }
            Err(e) => {
                eprintln!(
                    "\n  You can retry later using: {}",
                    style("contextstream-mcp index").cyan()
                );
                return Err(e);
            }
        }
    }

    Ok(())
}

async fn run_stdio_server() -> Result<()> {
    // Load configuration
    let config = match load_config() {
        Ok(c) => c,
        Err(e) => {
            if e.to_string().contains("Missing credentials") {
                // Run in limited mode
                eprintln!(
                    "ContextStream MCP server v{} (limited mode)",
                    mcp_types::config::VERSION
                );
                eprintln!("Run 'contextstream-mcp setup' or set CONTEXTSTREAM_API_KEY to enable all tools.");
                run_limited_mode_server().await?;
                return Ok(());
            }
            return Err(e);
        }
    };

    // Log startup
    let log_level = config.log_level;
    if !log_level.is_quiet() {
        eprintln!(
            "━━━ ContextStream v{} (Rust) ━━━",
            mcp_types::config::VERSION
        );
    }

    // Create client and session manager
    let client = mcp_client::ContextStreamClient::new(config.clone());
    let session = std::sync::Arc::new(mcp_session::SessionManager::new(
        client.clone(),
        config.clone(),
    ));
    session.register_session_refresh_hook().await;

    // Run the server
    server::run_server(config, client, session).await?;

    Ok(())
}

async fn run_limited_mode_server() -> Result<()> {
    // In limited mode, we just expose a help tool
    eprintln!("Limited mode: Only help tool available.");
    eprintln!("Configure authentication to enable all tools.");

    // For now, just wait forever (in a real implementation, this would run an MCP server with limited tools)
    tokio::signal::ctrl_c().await?;
    Ok(())
}

const MCP_WIRE_TOKENIZER_WARM_LATENCY_MS_BUCKETS: [f64; 13] = [
    1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1_000.0, 2_500.0, 5_000.0, 10_000.0, 30_000.0,
];
const MCP_WIRE_TOKENIZER_COUNT_LATENCY_US_BUCKETS: [f64; 13] = [
    10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1_000.0, 2_000.0, 5_000.0, 10_000.0, 20_000.0, 40_000.0,
    100_000.0,
];
const MCP_WIRE_TOKENIZER_TOKEN_COUNT_BUCKETS: [f64; 14] = [
    32.0, 64.0, 128.0, 256.0, 512.0, 1_024.0, 2_048.0, 4_096.0, 8_192.0, 16_384.0, 32_768.0,
    65_536.0, 131_072.0, 262_144.0,
];
const MCP_WIRE_TOKENIZER_DELTA_BUCKETS: [f64; 17] = [
    -8_192.0, -4_096.0, -2_048.0, -1_024.0, -512.0, -256.0, -128.0, -64.0, 0.0, 64.0, 128.0, 256.0,
    512.0, 1_024.0, 2_048.0, 4_096.0, 8_192.0,
];
const MCP_WIRE_TOKENIZER_RATIO_BUCKETS: [f64; 11] =
    [0.5, 0.75, 0.9, 1.0, 1.05, 1.1, 1.2, 1.5, 2.0, 3.0, 5.0];

fn configured_http_metrics_builder() -> Result<metrics_exporter_prometheus::PrometheusBuilder> {
    let metric_buckets: &[(&str, &[f64])] = &[
        (
            "mcp_wire_tokenizer_warm_latency_ms",
            &MCP_WIRE_TOKENIZER_WARM_LATENCY_MS_BUCKETS,
        ),
        (
            "mcp_wire_tokenizer_final_count_latency_us",
            &MCP_WIRE_TOKENIZER_COUNT_LATENCY_US_BUCKETS,
        ),
        (
            "mcp_wire_tokenizer_final_proxy_tokens",
            &MCP_WIRE_TOKENIZER_TOKEN_COUNT_BUCKETS,
        ),
        (
            "mcp_wire_tokenizer_final_exact_tokens",
            &MCP_WIRE_TOKENIZER_TOKEN_COUNT_BUCKETS,
        ),
        (
            "mcp_wire_tokenizer_final_exact_minus_proxy_tokens",
            &MCP_WIRE_TOKENIZER_DELTA_BUCKETS,
        ),
        (
            "mcp_wire_tokenizer_final_exact_to_proxy_ratio",
            &MCP_WIRE_TOKENIZER_RATIO_BUCKETS,
        ),
    ];
    let mut builder = metrics_exporter_prometheus::PrometheusBuilder::new();
    for (metric, buckets) in metric_buckets {
        builder = builder
            .set_buckets_for_metric(
                metrics_exporter_prometheus::Matcher::Full((*metric).to_string()),
                buckets,
            )
            .map_err(|error| {
                anyhow::anyhow!("configure Prometheus buckets for {metric}: {error}")
            })?;
    }
    Ok(builder)
}

async fn run_http_server(host: &str, port: u16) -> Result<()> {
    use mcp_tools::{domains, ToolRegistry};

    // Load configuration
    let mut config = load_config()?;
    config.is_http_transport = true;

    // Install the Prometheus recorder BEFORE any code path that emits
    // metrics — once the global recorder is set, every
    // `metrics::counter!` / `metrics::histogram!` macro in the dep tree
    // (including mcp-tools and the public acceleration layer)
    // is captured. The handle is stored on HttpState and rendered by the
    // unauthenticated /metrics route below.
    //
    // `install_recorder` is idempotent within a process but only the
    // first call wins — running multiple HTTP servers from one binary
    // would share the same registry, which is fine for our use case.
    let metrics_handle = configured_http_metrics_builder()?
        .install_recorder()
        .map_err(|e| anyhow::anyhow!("install Prometheus recorder: {}", e))?;

    // Log startup
    if !config.log_level.is_quiet() {
        eprintln!(
            "━━━ ContextStream HTTP Gateway v{} (Rust) ━━━",
            mcp_types::config::VERSION
        );
    }

    // Create client and session manager
    let client = mcp_client::ContextStreamClient::new(config.clone());
    let session = std::sync::Arc::new(mcp_session::SessionManager::new(
        client.clone(),
        config.clone(),
    ));
    session.register_session_refresh_hook().await;

    // Create tool registry and register all tools
    let mut registry = ToolRegistry::new(&config);

    // Legacy wire-compatibility layer; public builds always install a no-op.
    let atlas_layer = mcp_server::atlas::build_atlas_layer();
    registry.set_atlas_layer(atlas_layer.clone());

    let acceleration_layer = mcp_server::acceleration::build_acceleration_layer();
    registry.set_acceleration_layer(acceleration_layer.clone());

    // Register domain tools
    let index_keeper = std::sync::Arc::new(domains::index_keeper::IndexKeeper::new(
        client.clone(),
        session.clone(),
        atlas_layer,
        acceleration_layer,
    ));
    domains::session::register_session_tools(
        &mut registry,
        client.clone(),
        session.clone(),
        index_keeper.clone(),
    );
    domains::flash::register_flash_tools(&mut registry, client.clone(), session.clone());
    domains::search::register_search_tools(
        &mut registry,
        client.clone(),
        session.clone(),
        index_keeper,
    );
    domains::memory::register_memory_tools(&mut registry, client.clone(), session.clone());
    domains::graph::register_graph_tools(&mut registry, client.clone(), session.clone());
    domains::workspace::register_workspace_tools(&mut registry, client.clone());
    domains::project::register_project_tools(&mut registry, client.clone(), session.clone());
    domains::integrations::register_integration_tools(
        &mut registry,
        client.clone(),
        session.clone(),
    );
    domains::reminder::register_reminder_tools(&mut registry, client.clone());
    domains::coordination::register_coordination_tools(&mut registry, client.clone());
    // Phase 1-3 taxonomy expansion: unified entity CRUD across tickets,
    // handoffs, backlog_views, incidents, releases, experiments, goals,
    // key_results, sprints, reviews, risks.
    domains::entity::register_entity_tools(&mut registry, client.clone(), session.clone());
    domains::media::register_media_tools(&mut registry, client.clone(), session.clone());
    domains::help::register_help_tools(&mut registry, client.clone());
    domains::skill::register_skill_tools(&mut registry, client.clone(), session.clone());
    domains::qa::register_qa_tools(&mut registry, client.clone());

    let tool_count = registry.len();

    if !config.log_level.is_quiet() {
        eprintln!("✓ {} tools registered", tool_count);
        eprintln!("✓ HTTP server starting on {}:{}", host, port);
    }

    // Get auth settings from environment
    let jwt_secret = std::env::var("CONTEXTSTREAM_JWT_SECRET").ok();
    let require_auth = std::env::var("MCP_HTTP_REQUIRE_AUTH")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false);

    // Run HTTP server
    transport::http::run_http_server(
        std::sync::Arc::new(registry),
        client,
        session,
        host,
        port,
        jwt_secret,
        require_auth,
        metrics_handle,
    )
    .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn should_refresh_project_rules_when_project_config_exists() {
        let temp = tempdir().expect("tempdir");
        let project = temp.path();

        let config = setup::ProjectConfig::new().with_workspace("ws", Some("Workspace"));
        setup::write_project_config(project, &config).expect("write config");

        assert!(should_refresh_project_rules_after_update(project));
    }

    #[test]
    fn should_refresh_project_rules_when_existing_project_rules_exist() {
        let temp = tempdir().expect("tempdir");
        let project = temp.path();
        let rules_path = project
            .join(".cursor")
            .join("rules")
            .join("contextstream.md");
        std::fs::create_dir_all(rules_path.parent().expect("parent")).expect("mkdirs");
        std::fs::write(&rules_path, "<contextstream>\nmanaged\n</contextstream>\n")
            .expect("write rules");

        assert!(should_refresh_project_rules_after_update(project));
    }

    #[test]
    fn should_not_refresh_project_rules_in_plain_directory() {
        let temp = tempdir().expect("tempdir");
        assert!(!should_refresh_project_rules_after_update(temp.path()));
    }

    #[test]
    fn post_update_refresh_commands_always_include_hooks_and_global_rules() {
        let commands = post_update_editor_refresh_commands(false);
        let args: Vec<&[&str]> = commands.iter().map(|command| command.args).collect();

        assert!(args.contains(&UPDATE_HOOKS_ARGS));
        assert!(args.contains(&UPDATE_GLOBAL_RULES_ARGS));
        assert!(!args.contains(&UPDATE_PROJECT_RULES_ARGS));
    }

    #[test]
    fn post_update_refresh_commands_include_project_rules_when_project_scope_exists() {
        let commands = post_update_editor_refresh_commands(true);
        let args: Vec<&[&str]> = commands.iter().map(|command| command.args).collect();

        assert!(args.contains(&UPDATE_HOOKS_ARGS));
        assert!(args.contains(&UPDATE_GLOBAL_RULES_ARGS));
        assert!(args.contains(&UPDATE_PROJECT_RULES_ARGS));
    }

    #[test]
    fn release_binary_url_pins_manifest_version() {
        assert_eq!(
            release_binary_url("0.5.46", "contextstream-mcp-win-x64.exe"),
            "https://pub-68429b9f7857416c9484b75bf1887b96.r2.dev/mcp/v0.5.46/contextstream-mcp-win-x64.exe"
        );
        assert_eq!(
            release_binary_url("", "contextstream-mcp-win-x64.exe"),
            "https://pub-68429b9f7857416c9484b75bf1887b96.r2.dev/mcp/latest/contextstream-mcp-win-x64.exe"
        );
    }

    #[test]
    fn editor_id_parser_deduplicates_but_never_turns_explicit_empty_into_detection() {
        let parsed = parse_editor_ids(&[
            " codex ".to_string(),
            "claude".to_string(),
            "codex".to_string(),
        ])
        .expect("valid editor list")
        .expect("explicit selection");
        assert_eq!(
            parsed,
            vec![
                setup::editors::Editor::Codex,
                setup::editors::Editor::ClaudeCode
            ]
        );

        assert!(parse_editor_ids(&[]).expect("omitted list").is_none());
        assert!(parse_editor_ids(&["".to_string()]).is_err());
        assert!(parse_editor_ids(&["codex".to_string(), " ".to_string()]).is_err());
        assert!(parse_editor_ids(&["not-an-editor".to_string()]).is_err());
    }

    #[test]
    fn setup_cli_accepts_explicit_project_path_and_rejects_account_only_conflict() {
        let cli = Cli::try_parse_from([
            "contextstream-mcp",
            "setup",
            "--yes",
            "--editors",
            "codex",
            "--project-path",
            "/work/project",
        ])
        .expect("explicit project setup should parse");
        match cli.command {
            Some(Commands::Setup {
                yes,
                editors,
                project_path,
                account_only,
                ..
            }) => {
                assert!(yes);
                assert_eq!(editors, vec!["codex"]);
                assert_eq!(
                    project_path,
                    Some(std::path::PathBuf::from("/work/project"))
                );
                assert!(!account_only);
            }
            _ => panic!("expected setup command"),
        }

        let conflict = Cli::try_parse_from([
            "contextstream-mcp",
            "setup",
            "--project-path",
            "/work/project",
            "--account-only",
        ]);
        assert!(conflict.is_err());
    }

    #[test]
    fn whole_wire_metrics_use_finite_production_buckets() {
        let recorder = configured_http_metrics_builder()
            .expect("whole-wire Prometheus buckets should be valid")
            .build_recorder();
        let handle = recorder.handle();

        metrics::with_local_recorder(&recorder, || {
            metrics::histogram!("mcp_wire_tokenizer_warm_latency_ms").record(150.0);
            metrics::histogram!("mcp_wire_tokenizer_final_count_latency_us").record(838.0);
            metrics::histogram!("mcp_wire_tokenizer_final_proxy_tokens").record(922.0);
            metrics::histogram!("mcp_wire_tokenizer_final_exact_tokens").record(1_070.0);
            metrics::histogram!("mcp_wire_tokenizer_final_exact_minus_proxy_tokens").record(148.0);
            metrics::histogram!("mcp_wire_tokenizer_final_exact_to_proxy_ratio").record(1.161);
        });

        let rendered = handle.render();
        for metric in [
            "mcp_wire_tokenizer_warm_latency_ms",
            "mcp_wire_tokenizer_final_count_latency_us",
            "mcp_wire_tokenizer_final_proxy_tokens",
            "mcp_wire_tokenizer_final_exact_tokens",
            "mcp_wire_tokenizer_final_exact_minus_proxy_tokens",
            "mcp_wire_tokenizer_final_exact_to_proxy_ratio",
        ] {
            let bucket_prefix = format!("{metric}_bucket");
            assert!(
                rendered.lines().any(|line| {
                    line.starts_with(&bucket_prefix)
                        && !line.contains("le=\"+Inf\"")
                        && line
                            .rsplit_once(' ')
                            .and_then(|(_, count)| count.parse::<f64>().ok())
                            .is_some_and(|count| count >= 1.0)
                }),
                "representative {metric} observation only reached +Inf: {rendered}"
            );
        }
    }
}
