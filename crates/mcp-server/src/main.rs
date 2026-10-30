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
  contextstream-mcp clients --format=json|markdown
  contextstream-mcp generate-configs --transport=remote --preauth
  contextstream-mcp configure --account-learning on|off|status
  contextstream-mcp configure --transcripts=on|off --scope=all
  printf %s "$KEY" | contextstream-mcp configure --api-key-stdin
  contextstream-mcp setup --yes --workspace-id=<UUID> --editors=claude

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

    // The variable is read with the rest of the configuration, which reports
    // an unknown value and goes on with the default; read here, an editor's
    // typo in it stopped the server before it started (#319).
    /// Default team vs personal execution mode (also CONTEXTSTREAM_ACCOUNT_MODE).
    #[arg(long, global = true, value_enum)]
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
        long_about = "Run the guided ContextStream MCP setup wizard.\n\nThe wizard authenticates your account, detects supported editors, links an exact folder to a canonical workspace/project, writes hosted MCP configs and rules, registers the managed local sync bridge, and offers foreground or background indexing when files already exist. Empty folders are valid new projects: setup links them immediately and the bridge syncs files as they appear. The bridge is not a local MCP transport; it keeps machine-local folders, checkouts, and Git worktrees fresh for hosted MCP. A review step lets you go back before setup continues.\n\nWith --yes, setup runs non-interactively using saved credentials (or CONTEXTSTREAM_API_KEY), selected or auto-detected editors, and the hosted remote gateway. On accounts with several workspaces, pass --workspace-id (or set CONTEXTSTREAM_WORKSPACE_ID) unless the folder is already linked. Pass --project-path to link and index an exact folder, --project-id to link an existing workspace project, or --project-name to create a project under a chosen name when the folder basename is already in use. Without --project-path, setup uses the current directory when it is safely scoped; HOME and filesystem roots are rejected, while ordinary empty folders are allowed. Use --account-only to intentionally skip project linking.\n\nTeam accounts: after login the wizard surfaces team workspace tips, shared skill discovery, and non-interactive refresh commands you can run later."
    )]
    Setup {
        /// Run non-interactively with defaults (needs saved credentials or CONTEXTSTREAM_API_KEY)
        #[arg(long, short = 'y', alias = "non-interactive")]
        yes: bool,

        /// Optional deep project learning: on opens human consent; off withdraws. Omitted preserves your choice.
        #[arg(long, visible_alias = "deep-learning", value_enum)]
        account_learning: Option<ToggleValue>,

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

        /// Workspace to use with --yes. Needed when the account has more than
        /// one workspace and the folder is not linked yet; without it, --yes
        /// uses the folder's link, then CONTEXTSTREAM_WORKSPACE_ID.
        #[arg(long, value_name = "UUID", requires = "yes")]
        workspace_id: Option<String>,

        /// Existing project UUID to link with --yes (disambiguates same-name
        /// folders or links a second checkout without a Git remote).
        #[arg(
            long,
            value_name = "UUID",
            requires = "yes",
            conflicts_with_all = ["account_only", "project_name"]
        )]
        project_id: Option<String>,

        /// Project name to create with --yes instead of the folder basename
        /// (for example when another project already uses that folder name).
        #[arg(
            long,
            value_name = "NAME",
            requires = "yes",
            conflicts_with_all = ["account_only", "project_id"]
        )]
        project_name: Option<String>,

        /// Leave Claude Code auto memory on. By default setup turns it off so
        /// memories and preferences are saved to ContextStream instead of
        /// local ~/.claude/projects/*/memory files.
        #[arg(long)]
        keep_claude_auto_memory: bool,

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

        /// Surfaces to inspect or repair: global, project, or all. Defaults to
        /// all inside a linked project folder and global elsewhere.
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

        /// Also remove ContextStream's managed git hooks from every repository
        /// this machine has set up (this repo included), not just this one.
        #[arg(long)]
        all_git_hooks: bool,

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

    /// Configure deep project learning, transcript defaults, editors, and credentials
    #[command(
        long_about = "Configure deep project learning consent, transcript defaults, editors, workspace, and credentials. With no flags, opens an interactive menu.\n\nNon-interactive examples:\n  contextstream-mcp configure --account-learning status\n  contextstream-mcp configure --account-learning on\n  contextstream-mcp configure --account-learning off\n  contextstream-mcp configure --transcripts=off --scope=all\n  printf %s \"$KEY\" | contextstream-mcp configure --api-key-stdin"
    )]
    Configure {
        /// Print configurable options and exit
        #[arg(long)]
        list_options: bool,

        /// Optional deep project learning: status shows current state, on opens browser consent, off withdraws.
        #[arg(
            long,
            visible_alias = "deep-learning",
            value_enum,
            num_args = 0..=1,
            default_missing_value = "status",
            conflicts_with_all = ["list_options", "api_key_stdin"]
        )]
        account_learning: Option<AccountLearningAction>,

        /// Verify an API key read from stdin and save it to the credentials
        /// file. Prompts without echo on a terminal. Reading stdin keeps the
        /// key out of the process list and shell history.
        #[arg(long, conflicts_with_all = ["list_options", "transcripts", "hook_transcripts"])]
        api_key_stdin: bool,

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

        /// Internal: run as the detached worker `--background` starts.
        #[arg(long, hide = true)]
        background_worker: bool,
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

        /// Internal: run as the detached worker `--background` starts.
        #[arg(long, hide = true)]
        background_worker: bool,
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

    /// Check or repair which ContextStream project a folder is linked to
    #[command(
        long_about = "Show which ContextStream project a checkout is linked to, check it against every project your account can open, and repair it.\n\nThe check ranks projects by the Git repository the checkout tracks, other linked checkouts of the same repository on this machine (worktrees and clones), repository/project/folder names, and indexing activity. Benchmark, fixture and test workspaces and generated, dated or temporary projects rank low. --fix applies a confident best match; --project-id links an exact project. The sync bridge runs the same check in the background and repairs only links backed by the repository itself, never into an isolated workspace, and never for a pinned folder (--pin). Every change is recorded with the previous link in ~/.contextstream/link-audit.json."
    )]
    Link {
        /// Folder to check (defaults to the current directory)
        #[arg(long)]
        path: Option<std::path::PathBuf>,

        /// Check every folder this machine has linked or indexed
        #[arg(long, conflicts_with = "path")]
        all: bool,

        /// Re-link to the confident best match
        #[arg(long)]
        fix: bool,

        /// Link to this exact project
        #[arg(long, value_name = "UUID", conflicts_with = "all")]
        project_id: Option<uuid::Uuid>,

        /// Keep the background audit from changing this folder's link
        #[arg(long)]
        pin: bool,

        /// Output as JSON
        #[arg(long)]
        json: bool,
    },

    /// Import Claude Code auto memory files into ContextStream (one time)
    #[command(
        long_about = "Copy each Claude Code auto memory file (~/.claude/projects/*/memory/*.md) into a ContextStream memory node: feedback and user memories become preferences, lessons become lessons, project and reference memories become facts. Each file's project is matched to the workspace/project its folder is linked to. Files already imported are skipped, and the local files are left in place.\n\nTurn auto memory off with `contextstream-mcp doctor --repair --scope global --editors claude` so new memories go to ContextStream."
    )]
    ImportLocalMemory {
        /// List what would be imported without sending anything
        #[arg(long)]
        dry_run: bool,

        /// Only import this Claude project directory (e.g. -Users-me-repo)
        #[arg(long, value_name = "NAME", allow_hyphen_values = true)]
        project_dir: Option<String>,

        /// Workspace to import into instead of the folder's link
        #[arg(long, value_name = "UUID")]
        workspace_id: Option<uuid::Uuid>,

        /// Project to import into
        #[arg(long, value_name = "UUID")]
        project_id: Option<uuid::Uuid>,
    },

    /// List every coding client setup can configure (non-interactive)
    #[command(
        long_about = "Print the catalog of coding clients setup can configure: config and rules locations, config format, the hosted server entry each client expects, hook support, install detection, and status.\n\nPaths are platform-neutral (~ for HOME). Use --format markdown for a docs table."
    )]
    Clients {
        /// Output format: json (default) or markdown
        #[arg(long, default_value = "json")]
        format: String,
    },

    /// Write the files of the public Antigravity plugin repository
    #[command(
        hide = true,
        long_about = "Generate the plugin published at github.com/contextstream/antigravity-plugin from the same code `setup` uses to write the plugin on a machine. Use --out DIR to write the files, or --json to print them as one JSON object. The logo (assets/logo.png) is not generated: copy it from the repository's assets folder."
    )]
    ExportAntigravityPlugin {
        /// Directory to write the files into
        #[arg(long, value_name = "DIR")]
        out: Option<std::path::PathBuf>,
        /// Print the files as JSON instead of writing them
        #[arg(long)]
        json: bool,
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
        patterns: Vec<String>,
    },

    /// Generate MCP configs as JSON without writing files (non-interactive)
    ///
    /// Used by the remote-first install script as a fallback for config generation.
    /// Outputs a JSON payload containing config objects for each specified editor.
    #[command(
        long_about = "Generate MCP config payloads as JSON without writing files.\n\nUsed by install scripts and team bootstrap automation. Pass --preauth to embed a validated API key so the editor can connect after it reloads the generated configuration.\n\nLocal stdio config generation is opt-in only: pass --transport=local with CONTEXTSTREAM_ALLOW_LOCAL_MCP=1 (or after selecting local transport in `contextstream-mcp setup`)."
    )]
    GenerateConfigs {
        /// Transport mode: remote or local (local requires CONTEXTSTREAM_ALLOW_LOCAL_MCP=1 or a saved local transport marker)
        #[arg(long, default_value = "remote")]
        transport: String,

        /// Comma-separated editor IDs (e.g. "claude,cursor,codex"). Defaults to all detected.
        #[arg(long)]
        editors: Option<String>,

        /// API key to embed in configs
        #[arg(long, env = "CONTEXTSTREAM_API_KEY", hide_env_values = true)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum AccountLearningAction {
    /// Open the signed-in privacy page in your browser to review and confirm consent
    On,
    /// Withdraw deep project learning consent and queue learned-data deletion
    Off,
    /// Show the current deep project learning consent state
    Status,
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
      - Rules, skill and hooks come from the ContextStream plugin in
        ~/.gemini/config/plugins/contextstream (`agy plugin list` shows it)
      - Verify ~/.gemini/config/mcp_config.json contains contextstream (serverUrl)
      - `contextstream-mcp doctor --repair --editors antigravity` restores the plugin;
        `agy plugin enable contextstream` turns it back on

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
  contextstream-mcp clients --format=json|markdown
  contextstream-mcp generate-configs --transport=remote --preauth
  contextstream-mcp configure --transcripts=on|off --scope=all
"#,
        mcp_types::config::VERSION
    );
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // Process-environment writes happen here, before any runtime (and so any
    // other thread) exists.
    if let Some(mode) = cli.account_mode {
        std::env::set_var(
            "CONTEXTSTREAM_ACCOUNT_MODE",
            AccountModePreference::from(mode).as_str(),
        );
    }

    setup_logging(cli.verbose, cli.quiet);
    restore_default_sigpipe(&cli.command);
    silence_stdout_when_quiet(cli.quiet, &cli.command);

    // Register the process-global teaching-bundle fingerprint so request
    // bodies, readiness, doctor, runtime staleness checks, and every editor
    // writer share one contract. Registration is cheap; the bundle is rendered
    // once, on first read.
    mcp_server::setup::install_canonical_rules_hash();

    match cli.command {
        Some(Commands::Hook { name, args }) => {
            prepare_hook_environment(&name, &args);
            // Editor hooks are one-shot processes that fire on every tool call.
            // A current-thread runtime avoids spawning (and joining) one worker
            // thread per CPU just to read stdin, decide, and print one line.
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(run_hook(&name));
            Ok(())
        }
        command => tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(run_command(command)),
    }
}

/// One-shot commands print for a person or a script, and the reader may close
/// the pipe early (`about | head -2`). Rust ignores SIGPIPE, so `println!` then
/// panics on the broken pipe and the process aborts with exit 134 (#159).
/// Restore the default so these commands end quietly on SIGPIPE, as other
/// CLIs do. The stdio server, the HTTP gateway, the sync bridge and editor
/// hooks are left alone: they outlive their reader or own their stdout.
///
/// Sockets are unaffected: std and mio send with `MSG_NOSIGNAL` on Linux and
/// set `SO_NOSIGPIPE` on Apple platforms. Must run before any thread exists.
#[cfg(unix)]
fn restore_default_sigpipe(command: &Option<Commands>) {
    let keeps_ignoring = matches!(
        command,
        None | Some(Commands::Http { .. } | Commands::Hook { .. } | Commands::Watch)
    );
    if !keeps_ignoring {
        // SAFETY: called from `main` before any tokio runtime or other thread
        // exists, so no signal disposition is changed under a running thread.
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    }
}

#[cfg(not(unix))]
fn restore_default_sigpipe(_command: &Option<Commands>) {}

/// `-q` / `--quiet` ("errors only", #255) for one-shot commands: progress and
/// confirmation lines on stderr are silenced (`setup::ui::is_quiet`), and
/// stdout goes to the null device unless it carries the data the command was
/// asked for (`--json`, `--support`, `clients`, `detect-editors`,
/// `generate-configs`, `exclude list|info`). Errors and the exit status are
/// unchanged. The stdio server, the HTTP gateway, the sync bridge and editor
/// hooks own their output and are left alone. Must run before any output.
fn silence_stdout_when_quiet(quiet: bool, command: &Option<Commands>) {
    let owns_output = matches!(
        command,
        None | Some(Commands::Http { .. } | Commands::Hook { .. } | Commands::Watch)
    );
    if !quiet || owns_output {
        return;
    }
    setup::ui::set_quiet(true);
    let stdout_is_data = matches!(
        command,
        Some(
            Commands::Doctor { json: true, .. }
                | Commands::Doctor { support: true, .. }
                | Commands::VerifyKey { json: true, .. }
                | Commands::Link { json: true, .. }
                | Commands::Clients { .. }
                | Commands::DetectEditors { .. }
                | Commands::GenerateConfigs { .. }
                | Commands::ExportAntigravityPlugin { json: true, .. }
                | Commands::Exclude {
                    action: ExcludeAction::List | ExcludeAction::Info,
                    ..
                }
        )
    );
    if !stdout_is_data {
        redirect_stdout_to_null();
    }
}

#[cfg(unix)]
fn redirect_stdout_to_null() {
    use std::os::fd::AsRawFd;
    if let Ok(null) = std::fs::OpenOptions::new().write(true).open("/dev/null") {
        // SAFETY: called from `main` before any output or other thread; dup2
        // replaces fd 1 atomically and the duplicate outlives `null`.
        unsafe { libc::dup2(null.as_raw_fd(), libc::STDOUT_FILENO) };
    }
}

#[cfg(windows)]
fn redirect_stdout_to_null() {
    use std::os::windows::io::IntoRawHandle;
    use windows_sys::Win32::System::Console::{SetStdHandle, STD_OUTPUT_HANDLE};
    if let Ok(null) = std::fs::OpenOptions::new().write(true).open("NUL") {
        // Rust looks the standard handle up on every write; the handle stays
        // open for the rest of the process.
        unsafe { SetStdHandle(STD_OUTPUT_HANDLE, null.into_raw_handle() as _) };
    }
}

#[cfg(not(any(unix, windows)))]
fn redirect_stdout_to_null() {}

/// Export the hook's event name and host arguments for the handlers.
///
/// Must run before the hook runtime is built: `std::env::set_var` is only
/// sound while no other thread can read the environment concurrently.
fn prepare_hook_environment(name: &str, args: &[String]) {
    // SAFETY: called from `main` before any tokio runtime or other thread
    // exists, so nothing can read the environment concurrently.
    unsafe { std::env::set_var("HOOK_EVENT_NAME", hook_event_name(name)) };
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
        unsafe { std::env::set_var("CONTEXTSTREAM_HOOK_ARGS", forwarded_args.join("\u{1f}")) };
    } else {
        // Do not let a caller-provided or inherited value masquerade
        // as host hook arguments when the managed marker is the only
        // command-line argument.
        unsafe { std::env::remove_var("CONTEXTSTREAM_HOOK_ARGS") };
    }
}

async fn run_hook(name: &str) {
    if let Err(e) = mcp_server::hook_handlers::dispatch_hook(name).await {
        eprintln!("Hook error: {}", e);
        std::process::exit(1);
    }
}

async fn run_command(command: Option<Commands>) -> Result<()> {
    match command {
        Some(Commands::Doctor {
            json,
            support,
            scope,
            editors,
            only_configured,
            repair,
            dry_run,
        }) => {
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
            account_learning,
            profile,
            profile_file,
            editors,
            project_path,
            account_only,
            workspace_id,
            project_id,
            project_name,
            dry_run,
            keep_claude_auto_memory,
        }) => {
            setup::safe_edit::set_dry_run(dry_run);
            setup::set_keep_claude_auto_memory(keep_claude_auto_memory);
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
                .map(|_| true)
            } else {
                setup::run_setup_with_project_overrides(
                    yes,
                    explicit.as_deref(),
                    project_path.as_deref(),
                    account_only,
                    workspace_id.as_deref(),
                    project_id.as_deref(),
                    project_name.as_deref(),
                    account_learning.map(|choice| choice.as_bool()),
                )
                .await
            };
            match result {
                Ok(false) => return Ok(()),
                Err(e) => {
                    eprintln!("Setup failed: {}", e);
                    std::process::exit(1);
                }
                Ok(true) => {}
            }
            if let Some(choice) = account_learning {
                setup::account_learning::configure(choice.as_bool()).await?;
            }
            // Validate hooks after setup without rewriting MCP configs. Scoped
            // to the selection setup just recorded, so declining every editor
            // leaves every editor alone. Setup already reported what it
            // configured, so this prints only problems.
            if !dry_run {
                if let Err(e) = setup::refresh_hooks_after_setup().await {
                    setup::ui::say(
                        setup::ui::Mark::Warn,
                        "Couldn't validate editor hooks",
                        Some(&e.to_string()),
                    );
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
            // Checking a key writes no installation state (#341).
            mcp_client::activation::set_installation_state_persistence_enabled(false);
            run_verify_key(json).await?;
        }

        Some(Commands::Hook { .. }) => {
            unreachable!("`main` dispatches hooks on their own current-thread runtime")
        }

        Some(Commands::GitHooks { path, uninstall }) => {
            run_git_hooks(path, uninstall);
        }

        Some(Commands::Uninstall {
            scope,
            editors,
            git_hooks,
            all_git_hooks,
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
                setup::uninstall(&scope, explicit.as_deref(), git_hooks, all_git_hooks).await
            {
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
                    if !setup::ui::is_quiet() {
                        eprintln!("Installed to {}/{}", install_dir, binary_name());
                    }
                }
                Err(e) => {
                    eprintln!("Install failed: {}", e);
                    std::process::exit(1);
                }
            }
        }

        Some(Commands::Configure {
            list_options,
            account_learning,
            api_key_stdin,
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
            if let Some(action) = account_learning {
                match action {
                    AccountLearningAction::Status => {
                        if !setup::account_learning::show_status().await? {
                            std::process::exit(1);
                        }
                    }
                    AccountLearningAction::On => {
                        setup::account_learning::configure(true).await?;
                    }
                    AccountLearningAction::Off => {
                        setup::account_learning::configure(false).await?;
                    }
                }
                if transcripts.is_none() && hook_transcripts.is_none() {
                    return Ok(());
                }
            }
            if api_key_stdin {
                if explicit.is_some() || only_configured || dry_run {
                    eprintln!(
                        "--api-key-stdin cannot be combined with --editors, --only-configured, \
                         or --dry-run"
                    );
                    std::process::exit(1);
                }
                if let Err(e) = run_configure_api_key_from_stdin().await {
                    eprintln!("Configure failed: {}", e);
                    std::process::exit(1);
                }
                return Ok(());
            }
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
            let outcome = match run_configure(
                list_options,
                transcripts,
                hook_transcripts,
                scope,
                explicit.as_deref(),
                only_configured,
            )
            .await
            {
                Ok(outcome) => outcome,
                Err(e) => {
                    eprintln!("Configure failed: {}", e);
                    std::process::exit(1);
                }
            };
            if outcome == ConfigureOutcome::Changed {
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
            background_worker,
        }) => {
            if let Err(e) =
                run_index_or_ingest(path, include_media, background, force, background_worker).await
            {
                eprintln!("Ingest failed: {}", e);
                std::process::exit(1);
            }
        }

        Some(Commands::Index {
            path,
            include_media,
            background,
            background_worker,
        }) => {
            if let Err(e) =
                run_index_or_ingest(path, include_media, background, false, background_worker).await
            {
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

        Some(Commands::Link {
            path,
            all,
            fix,
            project_id,
            pin,
            json,
        }) => {
            let all_correct = mcp_server::link::run(mcp_server::link::LinkCommand {
                path,
                all,
                fix,
                project_id,
                pin,
                json,
            })
            .await?;
            if !all_correct {
                std::process::exit(1);
            }
        }

        Some(Commands::ImportLocalMemory {
            dry_run,
            project_dir,
            workspace_id,
            project_id,
        }) => {
            let (_, _, failed) =
                setup::local_memory_import::run(setup::local_memory_import::ImportOptions {
                    dry_run,
                    project_dir,
                    workspace_id,
                    project_id,
                })
                .await?;
            if failed > 0 {
                std::process::exit(1);
            }
        }

        Some(Commands::Clients { format }) => match format.trim().to_ascii_lowercase().as_str() {
            "json" => println!(
                "{}",
                serde_json::to_string_pretty(&setup::clients::catalog_json()).unwrap()
            ),
            "markdown" | "md" => print!("{}", setup::clients::catalog_markdown()),
            other => {
                eprintln!("Unknown --format '{other}'. Use json or markdown.");
                std::process::exit(2);
            }
        },

        Some(Commands::ExportAntigravityPlugin { out, json }) => {
            if let Err(error) = setup::export_antigravity_plugin(out.as_deref(), json) {
                eprintln!("{error}");
                std::process::exit(2);
            }
        }

        Some(Commands::DetectEditors { format }) => {
            if !format.trim().eq_ignore_ascii_case("json") {
                eprintln!("Unknown --format '{}'. Use json.", format.trim());
                std::process::exit(2);
            }
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

            // Bootstrap scripts call this, so a mistyped id must fail instead of
            // quietly producing an empty or partial config set (#305).
            let target_editors = match editors {
                Some(editor_list) => {
                    let ids: Vec<String> = editor_list.split(',').map(str::to_string).collect();
                    match parse_editor_ids(&ids) {
                        // Emit in the catalog's order whatever order they were typed in.
                        Ok(parsed) => {
                            let parsed = parsed.unwrap_or_default();
                            setup::editors::Editor::all()
                                .iter()
                                .copied()
                                .filter(|editor| parsed.contains(editor))
                                .collect::<Vec<_>>()
                        }
                        Err(message) => {
                            eprintln!("{message}");
                            std::process::exit(2);
                        }
                    }
                }
                None => setup::editors::detect_installed_editors(),
            };

            let remote_auth = if preauth && !api_key.is_empty() {
                Some(api_key.as_str())
            } else {
                None
            };
            let normalized_transport = match transport.trim().to_ascii_lowercase().as_str() {
                "remote" | "http" | "hosted" | "hosted-remote" => "remote",
                "local" | "binary" | "local-binary" => {
                    if !setup::local_mcp_allowed() {
                        eprintln!(
                            "Refusing local MCP config generation without explicit opt-in. Hosted remote gateway is the default; \
                             set CONTEXTSTREAM_ALLOW_LOCAL_MCP=1 or run `contextstream-mcp setup` and choose \
                             local transport (writes ~/.contextstream/setup-transport-mode)."
                        );
                        std::process::exit(1);
                    }
                    "local"
                }
                other => {
                    eprintln!("Unknown --transport '{other}'. Use remote or local.");
                    std::process::exit(2);
                }
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

    // `list` and `info` print the listing asked for, on stdout; `add`,
    // `remove` and `reset` confirm on stderr, which `-q` silences (#354).
    macro_rules! note {
        ($($arg:tt)*) => {
            if !setup::ui::is_quiet() {
                eprintln!($($arg)*);
            }
        };
    }

    let project_dir = match path {
        Some(p) => std::path::PathBuf::from(p),
        None => std::env::current_dir()?,
    };
    if !matches!(action, ExcludeAction::Info) {
        if !project_dir.exists() {
            anyhow::bail!("Path does not exist: {}", project_dir.display());
        }
        if !project_dir.is_dir() {
            anyhow::bail!("Path must be a directory: {}", project_dir.display());
        }
    }

    let ignore_path = project_dir.join(CONTEXTIGNORE_FILE);

    match action {
        ExcludeAction::List => {
            println!(
                "\n{} Exclusion Patterns {}",
                style("⬡").cyan(),
                style(format!("({})", ignore_path.display())).dim()
            );
            println!();

            let current = read_contextignore(&ignore_path)?.lines;
            if current.is_empty() {
                println!(
                    "  {}No .contextignore file found. Using server defaults only.",
                    style("ℹ  ").blue()
                );
                println!();
                println!(
                    "  Run {} to create one with sensible defaults.",
                    style("contextstream-mcp exclude reset").cyan()
                );
            } else {
                for pattern in &current {
                    if pattern.starts_with('#') || pattern.is_empty() {
                        println!("  {}", style(pattern).dim());
                    } else {
                        println!("  {}", style(pattern).yellow());
                    }
                }
                println!();
                println!(
                    "  {} {} active pattern(s)",
                    style("→").dim(),
                    current
                        .iter()
                        .filter(|p| !p.starts_with('#') && !p.is_empty())
                        .count()
                );
            }

            println!();
            println!(
                "  {} Server-side protections (.env, keys, credentials) are always active.",
                style("🛡").green()
            );
            println!(
                "  Run {} for details.",
                style("contextstream-mcp exclude info").cyan()
            );
        }

        ExcludeAction::Add => {
            if patterns.is_empty() {
                anyhow::bail!("No patterns specified. Usage: contextstream-mcp exclude add <pattern> [pattern...]");
            }

            let patterns = normalize_exclude_patterns(&patterns, true)?;
            let mut file = read_contextignore(&ignore_path)?;
            let mut active: std::collections::HashSet<String> =
                file.lines.iter().cloned().collect();
            let mut added = Vec::new();

            for pattern in &patterns {
                if active.insert(pattern.clone()) {
                    file.lines.push(pattern.clone());
                    added.push(pattern.as_str());
                }
            }

            if added.is_empty() {
                note!(
                    "  {}All patterns already exist in .contextignore",
                    style("ℹ  ").blue()
                );
            } else {
                write_contextignore(&ignore_path, &file.lines, file.line_ending)?;
                note!(
                    "\n{} Added {} pattern(s) to .contextignore:",
                    style("✓").green(),
                    added.len()
                );
                for p in &added {
                    note!("  {} {}", style("+").green(), style(p).yellow());
                }
            }
        }

        ExcludeAction::Remove => {
            if patterns.is_empty() {
                anyhow::bail!("No patterns specified. Usage: contextstream-mcp exclude remove <pattern> [pattern...]");
            }

            let patterns = normalize_exclude_patterns(&patterns, false)?;
            let file = read_contextignore(&ignore_path)?;
            if file.lines.is_empty() {
                note!("  {}No .contextignore file found.", style("ℹ  ").blue());
                return Ok(());
            }

            let to_remove: std::collections::HashSet<&str> =
                patterns.iter().map(|s| s.as_str()).collect();
            let mut removed = Vec::new();

            let filtered: Vec<String> = file
                .lines
                .into_iter()
                .filter(|p| {
                    // A line an older version stored with surrounding spaces
                    // still matches the pattern as typed.
                    if to_remove.contains(p.as_str()) || to_remove.contains(p.trim()) {
                        removed.push(p.clone());
                        false
                    } else {
                        true
                    }
                })
                .collect();

            if removed.is_empty() {
                note!(
                    "  {}None of the specified patterns were found in .contextignore",
                    style("ℹ  ").blue()
                );
            } else {
                write_contextignore(&ignore_path, &filtered, file.line_ending)?;
                note!(
                    "\n{} Removed {} pattern(s) from .contextignore:",
                    style("✓").green(),
                    removed.len()
                );
                for p in &removed {
                    note!("  {} {}", style("-").red(), style(p).yellow());
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

            write_contextignore(&ignore_path, &lines, "\n")?;
            note!(
                "\n{} Reset .contextignore to default patterns ({} patterns)",
                style("✓").green(),
                DEFAULT_EXCLUDE_PATTERNS.len()
            );
            note!("  File: {}", style(ignore_path.display()).dim());
        }

        ExcludeAction::Info => {
            println!(
                "\n{} ContextStream Server-Side Security Protections",
                style("⬡").cyan()
            );
            println!();
            println!(
                "  {}",
                style("These protections are always active and cannot be disabled.").dim()
            );
            println!();

            println!(
                "  {} {}",
                style("■").red(),
                style("Always Blocked Files").bold()
            );
            println!("  Files that are never indexed regardless of settings:");
            let blocked = [
                ".env and all .env.* (except .env.example/.sample/.template/.dist/.defaults)",
                ".pem, .key, .p12, .pfx, .jks",
                "id_rsa, id_ed25519, id_ecdsa, known_hosts",
                "credentials.json, serviceAccountKey.json, .git-credentials",
                ".npmrc, .pypirc, .netrc, .pgpass",
                "credential dirs (skipped entirely): .ssh, .aws, .gnupg, .kube, .docker",
            ];
            for group in &blocked {
                println!("    {}", style(group).yellow());
            }

            println!();
            println!(
                "  {} {}",
                style("■").yellow(),
                style("Content Scanning").bold()
            );
            println!(
                "  All file content is scanned before storage. Detected secrets are redacted:"
            );
            let scanned = [
                "API keys and tokens (sk-, ghp_, AKIA, xox-, etc.)",
                "Private keys (PEM blocks)",
                "Connection strings with embedded credentials",
                "High-entropy strings that look like secrets",
            ];
            for item in &scanned {
                println!("    → {}", style(item).dim());
            }

            println!();
            println!(
                "  {} {}",
                style("■").blue(),
                style("Your Custom Exclusions").bold()
            );
            println!("  In addition to server protections, manage project-specific patterns with:");
            println!(
                "    {}",
                style("contextstream-mcp exclude list|add|remove|reset").cyan()
            );
        }
    }

    Ok(())
}

/// A `.contextignore` as `exclude` reads it: its lines and the line ending it
/// uses, so a rewrite keeps the person's CRLF.
struct ContextIgnoreFile {
    lines: Vec<String>,
    line_ending: &'static str,
}

/// The patterns in `path`. A file that is not there is an empty list; one that
/// is there and cannot be read (not UTF-8, no permission, a folder) is an error,
/// so no action treats the person's patterns as absent and writes over them.
fn read_contextignore(path: &std::path::Path) -> Result<ContextIgnoreFile> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(ContextIgnoreFile {
                lines: Vec::new(),
                line_ending: "\n",
            });
        }
        Err(error) => anyhow::bail!(
            "Could not read {}: {}. The file was left unchanged.",
            path.display(),
            error
        ),
    };
    let text = String::from_utf8(bytes).map_err(|_| {
        anyhow::anyhow!(
            "Could not read {}: it is not valid UTF-8. The file was left unchanged; fix or remove it and run this again.",
            path.display()
        )
    })?;
    Ok(ContextIgnoreFile {
        line_ending: if text.contains("\r\n") { "\r\n" } else { "\n" },
        lines: text.lines().map(|l| l.to_string()).collect(),
    })
}

fn write_contextignore(path: &std::path::Path, lines: &[String], line_ending: &str) -> Result<()> {
    let mut content = lines.join(line_ending);
    if !content.ends_with('\n') {
        content.push_str(line_ending);
    }
    std::fs::write(path, content)
        .map_err(|error| anyhow::anyhow!("Could not write {}: {}", path.display(), error))
}

/// The patterns as typed, trimmed (gitignore drops a trailing space and nobody
/// means a leading one), refusing what could not exclude anything: an empty
/// pattern, and when adding one that starts with `#`, which is a comment line.
fn normalize_exclude_patterns(patterns: &[String], adding: bool) -> Result<Vec<String>> {
    let mut normalized = Vec::with_capacity(patterns.len());
    for pattern in patterns {
        let trimmed = pattern.trim();
        if trimmed.is_empty() {
            anyhow::bail!(
                "Empty pattern. Pass a gitignore-style pattern such as 'dist/' or '*.log'."
            );
        }
        if adding && trimmed.starts_with('#') {
            anyhow::bail!(
                "'{trimmed}' starts with # and would be a comment in .contextignore, so it would exclude nothing. Write \\{trimmed} to exclude a file whose name starts with #."
            );
        }
        normalized.push(trimmed.to_string());
    }
    Ok(normalized)
}

/// The binary an update just installed, once `run_update` has located it.
static UPDATED_BINARY: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

async fn run_post_update_command_with_installed_binary(
    args: &[&str],
    description: &str,
) -> Result<()> {
    // After an install the new binary is wherever the installer put it; with
    // nothing installed, the running binary is already the current one.
    let install_path = UPDATED_BINARY
        .get()
        .cloned()
        .or_else(|| std::env::current_exe().ok())
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(expected_install_path);
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
/// Hosted-remote migration for the editors setup configured. Its last step
/// refreshes the global hooks for those same editors.
const MIGRATE_REMOTE_ARGS: &[&str] = &["migrate-remote", "--scope=all", "--only-configured"];
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

/// The refresh steps `update` still has to run. A successful
/// `migrate-remote --scope=all --only-configured` has just refreshed the hooks
/// of the same editors, so a separate hook refresh would only print every
/// editor's result a second time (#302).
fn post_update_steps(
    refresh_project_rules: bool,
    hooks_refreshed_by_migration: bool,
) -> Vec<PostUpdateCommand> {
    post_update_editor_refresh_commands(refresh_project_rules)
        .into_iter()
        .filter(|command| !(hooks_refreshed_by_migration && command.args == UPDATE_HOOKS_ARGS))
        .collect()
}

/// `update`'s progress lines (banner, steps, details, ✓ and skipped lines) are
/// silent under `-q`; warnings stay, as they carry the answer or the failure
/// (#354).
fn update_banner(current: &str) {
    if setup::ui::is_quiet() {
        return;
    }
    let ui = setup::ui::ui();
    eprintln!();
    eprintln!(
        "{}{}  {}",
        setup::ui::GUTTER,
        ui.paint(
            "CONTEXTSTREAM UPDATE",
            Some(setup::ui::Tok::AccentInk),
            true
        ),
        ui.faint(&format!("current v{current}"))
    );
    eprintln!();
}

fn update_step(message: impl AsRef<str>) {
    if setup::ui::is_quiet() {
        return;
    }
    let ui = setup::ui::ui();
    eprintln!(
        "{}{} {}",
        setup::ui::GUTTER,
        ui.mark(setup::ui::Mark::Step),
        ui.strong(message.as_ref())
    );
}

fn update_detail(message: impl AsRef<str>) {
    if setup::ui::is_quiet() {
        return;
    }
    eprintln!(
        "{}  {}",
        setup::ui::GUTTER,
        setup::ui::ui().muted(message.as_ref())
    );
}

fn update_ok(message: impl AsRef<str>) {
    if setup::ui::is_quiet() {
        return;
    }
    eprintln!(
        "{}",
        setup::ui::ui().line(setup::ui::Mark::Ok, message.as_ref(), None)
    );
}

fn update_warn(message: impl AsRef<str>) {
    eprintln!(
        "{}",
        setup::ui::ui().line(setup::ui::Mark::Warn, message.as_ref(), None)
    );
}

fn update_skip(message: impl AsRef<str>) {
    if setup::ui::is_quiet() {
        return;
    }
    let ui = setup::ui::ui();
    eprintln!(
        "{}",
        ui.line(setup::ui::Mark::Pending, &ui.muted(message.as_ref()), None)
    );
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

    let migrated = api_key.is_some();
    if migrated {
        update_detail("Ensuring MCP configs use the hosted remote gateway");
        run_post_update_command_with_installed_binary(
            MIGRATE_REMOTE_ARGS,
            "hosted remote migration",
        )
        .await?;
        update_ok("Hosted remote MCP configs and hooks refreshed");
    } else {
        update_skip("No saved credentials found; skipped hosted remote config migration");
    }

    for command in post_update_steps(refresh_project_rules, migrated) {
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
        "pre-tool-use" | "package-bouncer" | "mid-run-notices" => "PreToolUse",
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
            Ok(()) if setup::ui::is_quiet() => {}
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
        Ok(()) if setup::ui::is_quiet() => {}
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

/// `verify-key`: whether ContextStream accepts the configured credential.
/// A server that cannot be reached has not refused the key: the key is not
/// checked, and the message says where it tried and why that failed (#338).
async fn run_verify_key(json_output: bool) -> Result<()> {
    fn fail(json_output: bool, message: String, reachable: Option<bool>) -> ! {
        if json_output {
            println!("{}", verify_key_failure_json(&message, reachable));
        } else {
            eprintln!("{message}");
        }
        std::process::exit(1);
    }
    let config = match load_config() {
        Ok(config) => config,
        Err(e) => fail(json_output, format!("Error: {e}"), None),
    };
    let api_url = config.api_url.clone();
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
            } else if !setup::ui::is_quiet() {
                eprintln!("API key is valid!");
                eprintln!(
                    "{}",
                    verify_key_user_line(user.name.as_deref(), &user.email)
                );
            }
            Ok(())
        }
        Err(e) => {
            let (message, reachable) = verify_key_failure(&e, &api_url);
            fail(json_output, message, reachable)
        }
    }
}

/// "User: Ada (ada@example.com)", or "User: ada@example.com" with no name.
fn verify_key_user_line(name: Option<&str>, email: &str) -> String {
    match name.map(str::trim).filter(|name| !name.is_empty()) {
        Some(name) => format!("User: {name} ({email})"),
        None => format!("User: {email}"),
    }
}

/// What `verify-key` says when the check fails, and whether the server was
/// reached (`None` when the failure says nothing about it).
fn verify_key_failure(error: &mcp_types::Error, api_url: &str) -> (String, Option<bool>) {
    match error {
        mcp_types::Error::Network(_) => (
            format!(
                "Could not reach ContextStream at {api_url}, so the API key was not checked ({error}). \
                 Check the network connection, and CONTEXTSTREAM_API_URL if you set it."
            ),
            Some(false),
        ),
        mcp_types::Error::Timeout(seconds) => (
            format!(
                "ContextStream at {api_url} did not answer within {seconds} s, so the API key was \
                 not checked. Check the network connection, and CONTEXTSTREAM_API_URL if you set it."
            ),
            Some(false),
        ),
        // A refused key says how to replace it (`user_facing_message`, #252).
        error => (
            format!("API key verification failed: {}", error.user_facing_message()),
            Some(true),
        ),
    }
}

/// The `--json` failure object, serialized: a message holding a quote or a
/// backslash broke the hand-built one.
fn verify_key_failure_json(message: &str, reachable: Option<bool>) -> String {
    let mut object = serde_json::json!({"valid": false, "error": message});
    if let Some(reachable) = reachable {
        object["reachable"] = serde_json::Value::Bool(reachable);
    }
    object.to_string()
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
) -> Result<ConfigureOutcome> {
    if list_options {
        print_configure_options();
        return Ok(ConfigureOutcome::Unchanged);
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

        if setup::ui::is_quiet() {
            return Ok(ConfigureOutcome::Changed);
        }
        if mcp_server::setup::safe_edit::is_dry_run() {
            eprintln!(
                "Transcript defaults would be updated (scope: {}).",
                scope.as_str()
            );
        } else {
            eprintln!("Transcript defaults updated (scope: {}).", scope.as_str());
        }
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
        return Ok(ConfigureOutcome::Changed);
    }

    if list_options {
        return Ok(ConfigureOutcome::Unchanged);
    }

    setup::require_terminal(
        "The configure menu",
        "Pass a flag instead: `--account-learning status|on|off`, `--transcripts on|off`, \
         `--hook-transcripts on|off`, or `printf %s \"$KEY\" | contextstream-mcp configure \
         --api-key-stdin` (see `contextstream-mcp configure --help`).",
    )?;

    use dialoguer::Select;

    let options = [
        "Editors        — Configure editor integrations (hooks, rules, MCP configs)",
        "Deep Learning  — Check status, open browser consent, or withdraw",
        "Transcripts    — Set default transcript capture policy (on/off)",
        "API Key        — Update authentication credentials",
        "Workspace      — Select or create a workspace",
        "Hooks          — Reinstall hooks for all editors",
        "Rules          — Regenerate AI rules for all editors",
        "MCP Configs    — Regenerate MCP server configs for all editors",
        "Exit           — Leave configure menu",
    ];

    // Leaving the menu without acting changes nothing, so the caller skips
    // its hook refresh (#257).
    let mut outcome = ConfigureOutcome::Unchanged;
    loop {
        eprintln!();
        let selection = Select::new()
            .with_prompt("What would you like to configure?")
            .items(options)
            .default(0)
            .interact_opt()?;

        let selection = match selection {
            Some(s) => s,
            None => {
                eprintln!("Done.");
                return Ok(outcome);
            }
        };
        if selection != options.len() - 1 {
            outcome = ConfigureOutcome::Changed;
        }

        match selection {
            // Editors
            0 => {
                run_configure_editors_flow().await?;
            }
            // Deep Learning
            1 => {
                run_configure_account_learning_flow().await?;
            }
            // Transcripts
            2 => {
                run_configure_transcript_defaults().await?;
            }
            // API Key
            3 => {
                let (api_key, email) = setup::authenticate().await?;
                let api_url = setup::get_api_url();
                setup::persist_setup_credentials(&api_key, &api_url)?;
                if !setup::ui::is_quiet() {
                    eprintln!("Credentials saved. Authenticated as {}.", email);
                }
                let refreshed = setup::refresh_existing_mcp_config_credentials(&api_key);
                if refreshed > 0 && !setup::ui::is_quiet() {
                    if mcp_server::setup::safe_edit::is_dry_run() {
                        eprintln!("Would refresh API key in {refreshed} editor MCP config(s).");
                    } else {
                        eprintln!("Refreshed API key in {refreshed} editor MCP config(s).");
                    }
                }
            }
            // Workspace
            4 => {
                let config = load_config()?;
                let client = mcp_client::ContextStreamClient::new(config);
                match setup::setup_workspace(&client).await? {
                    Some(ws) => eprintln!("Workspace set to: {} ({})", ws.name, ws.id),
                    None => eprintln!("No workspace selected."),
                }
            }
            // Hooks
            5 => {
                setup::update_hooks("all", None).await?;
                eprintln!("Hooks updated.");
            }
            // Rules
            6 => {
                setup::update_rules("all", None, None).await?;
                eprintln!("Rules updated.");
            }
            // MCP Configs
            7 => {
                setup::update_configs("all").await?;
                eprintln!("MCP configs updated.");
            }
            // Exit
            8 => {
                eprintln!("Done.");
                return Ok(outcome);
            }
            _ => unreachable!(),
        }
    }
}

/// Whether a `configure` run changed anything the editors' hooks depend on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConfigureOutcome {
    Changed,
    Unchanged,
}

async fn run_configure_account_learning_flow() -> Result<()> {
    use dialoguer::Select;

    // The menu goes on either way: its choices still work.
    let _ = setup::account_learning::show_status().await?;
    let options = [
        "Open privacy page in browser to review/enable consent",
        "Turn off — withdraw consent and queue learned-data deletion",
        "Back — return to configure menu",
    ];
    let Some(selection) = Select::new()
        .with_prompt("Deep project learning")
        .items(options)
        .default(0)
        .interact_opt()?
    else {
        return Ok(());
    };
    match selection {
        0 => setup::account_learning::configure(true).await?,
        1 => setup::account_learning::configure(false).await?,
        2 => {}
        _ => unreachable!(),
    }
    Ok(())
}

fn print_configure_options() {
    eprintln!("Configurable options (run `contextstream-mcp configure` for the interactive menu):");
    eprintln!("  - editors: editor integrations, hooks, rules, and MCP configs");
    eprintln!("  - deep project learning: check status, open browser consent, or withdraw (--account-learning status|on|off)");
    eprintln!("  - transcripts: default transcript policy for new chats (--transcripts on|off)");
    eprintln!("  - api-key: authentication credentials (non-interactive: --api-key-stdin)");
    eprintln!("  - workspace: select/create default workspace");
    eprintln!("  - hooks: reinstall hook scripts");
    eprintln!("  - rules: regenerate AI rule files");
    eprintln!("  - mcp-configs: regenerate MCP config files");
    eprintln!();
    eprintln!("Save an existing API key without the browser flow:");
    eprintln!(
        "  printf %s \"$CONTEXTSTREAM_API_KEY\" | contextstream-mcp configure --api-key-stdin"
    );
    eprintln!();
    eprintln!("Deep project learning commands:");
    eprintln!("  contextstream-mcp configure --account-learning status");
    eprintln!("  contextstream-mcp configure --account-learning on");
    eprintln!("  contextstream-mcp configure --account-learning off");
    eprintln!();
    eprintln!("Quick transcript commands:");
    eprintln!("  contextstream-mcp configure --transcripts off");
    eprintln!("  contextstream-mcp configure --transcripts on");
    eprintln!("  contextstream-mcp configure --transcripts on --hook-transcripts off");
    eprintln!("  contextstream-mcp configure --transcripts on --scope global");
}

/// `configure --api-key-stdin`: the non-interactive way to save an existing
/// key, so `setup --yes` and scripts do not need the browser flow.
async fn run_configure_api_key_from_stdin() -> Result<()> {
    use anyhow::Context;
    use std::io::{IsTerminal, Read};

    let input = if std::io::stdin().is_terminal() {
        dialoguer::Password::new()
            .with_prompt("ContextStream API key")
            .interact()?
    } else {
        let mut input = String::new();
        std::io::stdin()
            .read_to_string(&mut input)
            .context("Could not read the API key from stdin")?;
        input
    };
    let api_key = parse_api_key_input(&input)?;

    let config = setup::setup_client_config(api_key);
    let api_url = config.api_url.clone();
    let client = mcp_client::ContextStreamClient::new(config);
    let user = tokio::time::timeout(std::time::Duration::from_secs(10), client.me())
        .await
        .map_err(|_| anyhow::anyhow!("Key verification timed out; check connectivity and re-run."))?
        .map_err(|e| {
            // Offline, a timeout, a 5xx or a rate limit says nothing about the
            // key; only a refusal does (#248).
            if e.is_retryable() {
                anyhow::anyhow!(
                    "Could not verify the API key: {api_url} did not answer ({e}). Check the \
                     connection and re-run; nothing was saved."
                )
            } else {
                anyhow::anyhow!("The API key was not accepted ({e}); nothing was saved.")
            }
        })?;

    setup::persist_setup_credentials(api_key, &api_url)?;
    if !setup::ui::is_quiet() {
        eprintln!(
            "Credentials saved to {}. Authenticated as {}.",
            setup::credentials_file_path().display(),
            user.email
        );
    }
    let refreshed = setup::refresh_existing_mcp_config_credentials(api_key);
    if refreshed > 0 && !setup::ui::is_quiet() {
        if mcp_server::setup::safe_edit::is_dry_run() {
            eprintln!("Would refresh API key in {refreshed} editor MCP config(s).");
        } else {
            eprintln!("Refreshed API key in {refreshed} editor MCP config(s).");
        }
    }
    if std::env::var("CONTEXTSTREAM_API_KEY").is_ok_and(|env| env.trim() != api_key) {
        eprintln!(
            "Note: CONTEXTSTREAM_API_KEY is set in this environment and takes precedence \
             over the saved key."
        );
    }
    Ok(())
}

/// Accept exactly one key: surrounding whitespace and a trailing newline are
/// normal for piped input, but anything else means the wrong thing was piped.
fn parse_api_key_input(input: &str) -> Result<&str> {
    let api_key = input.trim();
    if api_key.is_empty() {
        anyhow::bail!(
            "No API key received. Pipe one in, e.g. \
             printf %s \"$CONTEXTSTREAM_API_KEY\" | contextstream-mcp configure --api-key-stdin"
        );
    }
    if api_key.chars().any(char::is_whitespace) {
        anyhow::bail!("Expected a single API key on stdin, but the input contains whitespace.");
    }
    Ok(api_key)
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
        .items(options)
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

/// The running binary's directory, unless it is the hooks' managed copy under
/// `~/.contextstream`, which the installer refreshes on its own.
fn running_binary_dir() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    let dir = exe.parent()?.to_path_buf();
    if dirs::home_dir().is_some_and(|home| dir.starts_with(home.join(".contextstream"))) {
        return None;
    }
    Some(dir)
}

fn dir_is_writable(dir: &std::path::Path) -> bool {
    let probe = dir.join(format!(".contextstream-write-probe-{}", std::process::id()));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Where an update should install so it replaces the binary that is running:
/// its own directory when this user can write there. Otherwise the installer
/// decides (its default, then a per-user directory when sudo is unavailable),
/// and an explicit `CONTEXTSTREAM_INSTALL_DIR` is inherited unchanged.
fn update_install_dir() -> Option<std::path::PathBuf> {
    if std::env::var_os("CONTEXTSTREAM_INSTALL_DIR").is_some_and(|dir| !dir.is_empty()) {
        return None;
    }
    running_binary_dir().filter(|dir| dir_is_writable(dir))
}

/// Directories the installer can have written to, most specific first.
fn unix_install_dirs() -> Vec<std::path::PathBuf> {
    let mut candidates = Vec::new();
    if let Some(dir) = std::env::var_os("CONTEXTSTREAM_INSTALL_DIR").filter(|dir| !dir.is_empty()) {
        candidates.push(std::path::PathBuf::from(dir));
    }
    candidates.extend(running_binary_dir());
    candidates.push(std::path::PathBuf::from("/usr/local/bin"));
    candidates.extend(dirs::home_dir().map(|home| home.join(".local").join("bin")));
    let mut unique = Vec::new();
    for dir in candidates {
        if !unique.contains(&dir) {
            unique.push(dir);
        }
    }
    unique
}

/// The installed binary that reports `version` (any existing one when empty).
fn located_installed_binary(version: &str) -> Option<std::path::PathBuf> {
    unix_install_dirs()
        .into_iter()
        .map(|dir| dir.join(binary_name()))
        .filter(|path| path.is_file())
        .find(|path| {
            version.is_empty()
                || query_binary_version(&path.to_string_lossy())
                    .is_some_and(|reported| reported.contains(version))
        })
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
fn verify_and_fix_binary_path(expected_install: &str, expected_version: &str) -> Option<String> {
    let expected_install = expected_install.to_string();
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

    // PATH resolves to a different binary. Another copy of the same release
    // is not stale and must never be removed.
    let resolved_str = resolved.to_string_lossy();
    if !expected_version.is_empty() {
        if let Some(version) = query_binary_version(&resolved_str) {
            if version.contains(expected_version) {
                return Some(version);
            }
        }
    }
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

/// Why `update` stopped when the latest version could not be read. A check
/// only asked whether there is a newer version, so it is not told to
/// reinstall with --force (#343).
fn unknown_latest_version(check_only: bool) -> &'static str {
    if check_only {
        "Could not check for a newer version. Check the network connection and try again."
    } else {
        "Could not determine the latest version. Check the network connection and try again, \
         or use --force to reinstall anyway."
    }
}

async fn run_update(check_only: bool, force: bool, remote: bool) -> Result<()> {
    const LATEST_VERSION_URL: &str =
        "https://pub-68429b9f7857416c9484b75bf1887b96.r2.dev/mcp/latest/version.json";
    const SETUP_URL: &str = "https://contextstream.io/scripts/mcp.sh";

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
            update_warn(format!(
                "Could not check the latest version: the release server did not answer ({}).",
                mcp_client::client::describe_transport_error(&e)
            ));
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
                anyhow::bail!(unknown_latest_version(check_only));
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
        // Unix: use setup shell script. Without pipefail a failed download
        // pipes nothing into bash, which exits 0 and reports a false success.
        let mut cmd = std::process::Command::new("bash");
        cmd.arg("-c")
            .arg(format!(
                "set -o pipefail; curl -fsSL '{}' | bash",
                SETUP_URL
            ))
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .env("CONTEXTSTREAM_INSTALL_SKIP_SETUP", "true")
            // Tells the installer this updater verifies wherever it installs.
            .env("CONTEXTSTREAM_UPDATER", "2");
        if let Some(dir) = update_install_dir() {
            cmd.env("CONTEXTSTREAM_INSTALL_DIR", dir);
        }
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
    let install_path = if cfg!(windows) {
        expected_install_path()
    } else {
        located_installed_binary(&latest)
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_else(expected_install_path)
    };
    let _ = UPDATED_BINARY.set(std::path::PathBuf::from(&install_path));
    let verified_version = verify_and_fix_binary_path(&install_path, &latest);

    match verified_version {
        Some(version_str) => {
            update_ok(format!("Your PATH runs {}", version_str));
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
    if !setup::ui::is_quiet() {
        eprintln!("Downloading {} binary from CDN...", arch_key);
    }

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

    // Stage, prove the download runs, then replace atomically (a running
    // Windows executable is renamed aside and rolled back on failure). A failed
    // or interrupted update leaves the previous binary in place, never a
    // missing or half-written file that every hook would then fail on (#296).
    // A leftover from the older rename-aside scheme is tidied up first.
    let _ = std::fs::remove_file(install_path.with_extension("old.exe"));
    setup::install_binary_bytes(&install_path, &bytes).map_err(|e| {
        anyhow::anyhow!(
            "Failed to install binary at {}: {:#}. \
             If the binary is locked by a running MCP server, \
             restart your editor and try again.",
            install_path.display(),
            e
        )
    })?;

    if !setup::ui::is_quiet() {
        eprintln!(
            "Installed to {} ({} bytes)",
            install_path.display(),
            bytes.len()
        );
    }

    // Verify the new binary runs
    let output = std::process::Command::new(&install_path)
        .arg("--version")
        .output();

    match output {
        Ok(o) if o.status.success() && setup::ui::is_quiet() => {}
        Ok(o) if o.status.success() => {
            let version = String::from_utf8_lossy(&o.stdout);
            eprintln!(
                "{}{} Verified: {}",
                mcp_server::setup::ui::GUTTER,
                mcp_server::setup::ui::ui().mark(mcp_server::setup::ui::Mark::Ok),
                version.trim()
            );
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
    background_worker: bool,
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

    if !background_worker {
        let ui = setup::ui::ui();
        eprintln!();
        eprintln!(
            "{}{} {}",
            setup::ui::GUTTER,
            ui.heading("Index"),
            ui.path(project_name)
        );
        eprintln!(
            "{}{}",
            setup::ui::GUTTER,
            ui.muted("Builds the searchable code graph your agents use for search, impact analysis, and context.")
        );
        eprintln!();
    }

    // Ensure we have valid credentials
    let config = match load_config() {
        Ok(c) if c.api_key.as_ref().is_none_or(|k| k.is_empty()) => {
            setup::require_terminal_to_sign_in()?;
            eprintln!(
                "  {}No API key found. Authenticating...",
                style("ℹ  ").blue()
            );
            let api_key = setup::ensure_authenticated_api_key().await?;
            setup::setup_client_config(&api_key)
        }
        Ok(c) => c,
        Err(_) => {
            setup::require_terminal_to_sign_in()?;
            eprintln!(
                "  {}No credentials found. Authenticating...",
                style("ℹ  ").blue()
            );
            let api_key = setup::ensure_authenticated_api_key().await?;
            setup::setup_client_config(&api_key)
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
            // Choosing a workspace is a prompt, so a script or coding agent
            // needs the unattended way to link the folder first (#288).
            setup::require_terminal(
                "Choosing a workspace",
                "Link this folder first with `contextstream-mcp setup --yes` (add \
                 `--workspace-id <ID>` if your account has several workspaces), or set \
                 CONTEXTSTREAM_WORKSPACE_ID.",
            )?;
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

    if background || background_worker {
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

        if !background_worker {
            // The upload must outlive this process, so it runs in a detached
            // worker rather than a task that dies when this command exits.
            setup::spawn_detached_index_worker(
                &project_path,
                workspace.as_ref().map(|w| w.id.as_str()),
                resolved_project_id,
                include_media,
                force,
            )
            .map_err(|error| anyhow::anyhow!("Could not start background indexing: {error}"))?;
            setup::ui::say(
                setup::ui::Mark::Step,
                "Indexing in the background",
                Some("you'll get a desktop notification when it finishes"),
            );
            eprintln!(
                "{}{}",
                setup::ui::GUTTER,
                setup::ui::ui().faint(&format!("Status file: {}", status_file.display()))
            );
            return Ok(());
        }

        let status_file_clone = status_file.clone();
        let project_name_clone = project_name_owned.clone();
        async move {
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
                            "Status: Complete\nProject: {}\nFiles indexed: {}\n{}Completed: {}\n",
                            project_name_clone,
                            outcome.files_indexed,
                            too_large_status_line(outcome.files_too_large),
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
                        "Status: Incomplete\nProject: {}\nFiles indexed so far: {}\nFiles deferred: {}\n{}Scan complete: {}\nFinished: {}\n",
                        project_name_clone,
                        outcome.files_indexed,
                        outcome.files_deferred,
                        too_large_status_line(outcome.files_too_large),
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
        }
        .await;
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

/// The status file's line for files left out for size, or nothing.
fn too_large_status_line(files_too_large: usize) -> String {
    if files_too_large == 0 {
        String::new()
    } else {
        format!("Files over 5 MiB, not indexed: {files_too_large}\n")
    }
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
    // No credentials: serve a real MCP session with exactly two tools so the
    // agent can explain the state and connect an account (browser device
    // login, or inline email signup when the server offers it). Stdio only;
    // the hosted HTTP gateway keeps requiring bearer auth.
    let config = mcp_server::config::load_limited_config()?;
    eprintln!("Limited mode: `init` and `account` tools only until an account is connected.");
    let client = mcp_client::ContextStreamClient::new(config.clone());
    let session = std::sync::Arc::new(mcp_session::SessionManager::new(
        client.clone(),
        config.clone(),
    ));
    let registry = mcp_server::limited_mode::build_limited_registry(&config, session.clone());
    server::run_server_with_registry(config, client, session, registry).await
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
    // Load configuration
    let mut config = load_config()?;
    config.is_http_transport = true;
    // A Slipstream auto-accept cap is a person's consent to be charged. The gateway serves
    // everyone, so its own environment never sets one: each caller sends theirs as a header.
    config.slipstream_auto_accept_max_credits = 0;

    // Caller-supplied local filesystem paths must not drive reads/writes of the
    // gateway's own disk. Publish the policy process-wide so the filesystem
    // helpers in mcp-client/mcp-session that have no Config in hand honor it.
    mcp_client::set_local_files_denied(!config.local_files_allowed());

    // The hosted gateway's binary is pinned by its release. A session init that
    // reports a newer version must not make the gateway replace itself.
    mcp_client::set_runtime_self_update_disabled(true);

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

    // The same registry the stdio server and the per-request HTTP registries use
    // (server::build_registry). A list of its own here drifted: answer, feed, vcs,
    // capsule, chart and async_job were never added, so a fresh gateway listed six
    // tools fewer than a per-request registry, and tools/list depended on which kind
    // of request filled its cache first.
    let registry = server::build_registry(&config, client.clone(), session.clone());

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
    // #343
    #[test]
    fn a_version_check_is_not_told_to_reinstall() {
        assert!(!unknown_latest_version(true).contains("--force"));
        assert!(unknown_latest_version(true).contains("Check the network connection"));
        assert!(unknown_latest_version(false).contains("--force"));
    }

    // #338
    #[test]
    fn verify_key_names_the_user_without_an_empty_name() {
        assert_eq!(
            verify_key_user_line(Some("Ada"), "ada@example.com"),
            "User: Ada (ada@example.com)"
        );
        assert_eq!(
            verify_key_user_line(Some(" "), "qa@example.com"),
            "User: qa@example.com"
        );
        assert_eq!(
            verify_key_user_line(None, "qa@example.com"),
            "User: qa@example.com"
        );
    }

    #[test]
    fn verify_key_says_an_unreachable_server_left_the_key_unchecked() {
        let url = "http://127.0.0.1:9";
        let (message, reachable) = verify_key_failure(
            &mcp_types::Error::Network(
                "Connection failed: error sending request: Connection refused".into(),
            ),
            url,
        );
        assert_eq!(reachable, Some(false));
        assert!(message.starts_with("Could not reach ContextStream at http://127.0.0.1:9, so the API key was not checked"), "{message}");
        assert!(message.contains("Connection refused"), "{message}");
        assert!(message.contains("CONTEXTSTREAM_API_URL"), "{message}");
        assert!(!message.contains("verification failed"), "{message}");

        let (message, reachable) = verify_key_failure(&mcp_types::Error::Timeout(30), url);
        assert_eq!(reachable, Some(false));
        assert!(
            message.contains("did not answer within 30 s, so the API key was not checked"),
            "{message}"
        );

        let (message, reachable) =
            verify_key_failure(&mcp_types::Error::http(401, "Unauthorized"), url);
        assert_eq!(reachable, Some(true));
        assert!(
            message.starts_with("API key verification failed: HTTP error (401): Unauthorized."),
            "{message}"
        );
        assert!(message.contains("did not accept the API key"), "{message}");
        assert!(message.contains("contextstream-mcp setup"), "{message}");
    }

    #[test]
    fn verify_key_json_failure_is_valid_json_whatever_the_message() {
        for message in [
            "plain",
            r#"a "quoted" word"#,
            r"a back\slash",
            "line\nbreak",
        ] {
            let json: serde_json::Value =
                serde_json::from_str(&verify_key_failure_json(message, Some(false))).unwrap();
            assert_eq!(json["valid"], false);
            assert_eq!(json["error"], message);
            assert_eq!(json["reachable"], false);
        }
        let json: serde_json::Value =
            serde_json::from_str(&verify_key_failure_json("Error: missing", None)).unwrap();
        assert!(json.get("reachable").is_none());
    }

    use super::*;
    use tempfile::tempdir;

    #[test]
    fn the_account_mode_variable_cannot_stop_the_server() {
        // #319: CONTEXTSTREAM_ACCOUNT_MODE=both made clap exit before the
        // server started; config reads the variable and reports a bad value.
        use clap::CommandFactory;
        let command = Cli::command();
        let flag = command
            .get_arguments()
            .find(|arg| arg.get_id() == "account_mode")
            .expect("the --account-mode flag");
        assert!(flag.get_env().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn update_check_keeps_a_shadowing_copy_of_the_new_release() {
        use std::os::unix::fs::PermissionsExt;
        // HOME and PATH are process-wide; any env-changing test here takes this lock.
        static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _env = ENV.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let home = tempdir().expect("home");
        let fake = |dir: &std::path::Path, version: &str| {
            std::fs::create_dir_all(dir).expect("dir");
            let path = dir.join("contextstream-mcp");
            std::fs::write(
                &path,
                format!("#!/bin/sh\necho 'contextstream-mcp {version}'\n"),
            )
            .expect("fake binary");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
            path
        };
        // The installer fell back to ~/.local/bin, which PATH resolves first;
        // the check expected another directory holding the same release.
        let user_copy = fake(&home.path().join(".local/bin"), "9.9.9");
        let expected = fake(&home.path().join("system/bin"), "9.9.9");
        let (old_home, old_path) = (std::env::var_os("HOME"), std::env::var_os("PATH"));
        std::env::set_var("HOME", home.path());
        std::env::set_var("PATH", home.path().join(".local/bin"));

        let verified = verify_and_fix_binary_path(&expected.to_string_lossy(), "9.9.9");

        match old_home {
            Some(value) => std::env::set_var("HOME", value),
            None => std::env::remove_var("HOME"),
        }
        match old_path {
            Some(value) => std::env::set_var("PATH", value),
            None => std::env::remove_var("PATH"),
        }
        assert_eq!(verified.as_deref(), Some("contextstream-mcp 9.9.9"));
        assert!(
            user_copy.exists(),
            "a copy of the new release must never be removed"
        );
    }

    #[test]
    fn the_http_gateway_never_updates_itself() {
        let source = include_str!("main.rs");
        let start = source
            .find("async fn run_http_server(")
            .expect("run_http_server");
        let body = &source[start..start + source[start..].find("\n}\n").expect("end")];
        assert!(body.contains("mcp_client::set_runtime_self_update_disabled(true);"));
    }

    #[test]
    fn the_http_gateway_builds_the_shared_registry() {
        // One registry builder for every transport: the gateway must not keep a
        // list of its own (it drifted once and dropped six hosted tools).
        let source = include_str!("main.rs");
        let start = source
            .find("async fn run_http_server(")
            .expect("run_http_server");
        let body = &source[start..start + source[start..].find("\n}\n").expect("end")];
        assert!(body.contains(
            "let registry = server::build_registry(&config, client.clone(), session.clone());"
        ));
        assert!(!body.contains("ToolRegistry::new("));
        assert!(!body.contains("::register_"));
    }

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
    fn a_successful_migration_has_already_refreshed_the_hooks() {
        // The step `update` skips relies on the migration covering the same
        // editors and refreshing global hooks as its last step.
        assert!(MIGRATE_REMOTE_ARGS.contains(&"--scope=all"));
        assert!(MIGRATE_REMOTE_ARGS.contains(&"--only-configured"));
        assert!(UPDATE_HOOKS_ARGS.contains(&"--only-configured"));

        let after_migration = post_update_steps(true, true);
        let args: Vec<&[&str]> = after_migration.iter().map(|command| command.args).collect();
        assert!(!args.contains(&UPDATE_HOOKS_ARGS));
        assert!(args.contains(&UPDATE_GLOBAL_RULES_ARGS));
        assert!(args.contains(&UPDATE_PROJECT_RULES_ARGS));

        // Without a migration (no saved credentials) the hooks still refresh.
        let without_migration = post_update_steps(false, false);
        let args: Vec<&[&str]> = without_migration
            .iter()
            .map(|command| command.args)
            .collect();
        assert!(args.contains(&UPDATE_HOOKS_ARGS));
        assert!(args.contains(&UPDATE_GLOBAL_RULES_ARGS));
        assert!(!args.contains(&UPDATE_PROJECT_RULES_ARGS));
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

    /// `--help` renders `[env: NAME=value]` for env-backed args unless the
    /// value is hidden, so any credential-shaped env var would print the
    /// caller's secret (issue #105). Walk every subcommand so a new arg
    /// cannot regress this.
    #[test]
    fn credential_env_args_never_render_their_values_in_help() {
        use clap::CommandFactory;

        fn visit(command: &clap::Command, path: &str, offenders: &mut Vec<String>) {
            for arg in command.get_arguments() {
                let Some(env) = arg.get_env().and_then(|env| env.to_str()) else {
                    continue;
                };
                let upper = env.to_ascii_uppercase();
                let credential = ["KEY", "TOKEN", "SECRET", "JWT", "PASSWORD"]
                    .iter()
                    .any(|marker| upper.contains(marker));
                if credential && !arg.is_hide_env_values_set() {
                    offenders.push(format!("{path} --{} ({env})", arg.get_id()));
                }
            }
            for sub in command.get_subcommands() {
                visit(sub, &format!("{path} {}", sub.get_name()), offenders);
            }
        }

        let mut offenders = Vec::new();
        visit(&Cli::command(), "contextstream-mcp", &mut offenders);
        assert!(
            offenders.is_empty(),
            "credential env args must set hide_env_values: {offenders:?}"
        );
    }

    #[test]
    fn generate_configs_help_names_the_api_key_env_var_without_its_value() {
        use clap::CommandFactory;

        let mut command = Cli::command();
        let help = command
            .find_subcommand_mut("generate-configs")
            .expect("generate-configs subcommand")
            .render_long_help()
            .to_string();
        assert!(help.contains("[env: CONTEXTSTREAM_API_KEY]"), "{help}");
        assert!(!help.contains("CONTEXTSTREAM_API_KEY="), "{help}");
        assert!(
            help.contains("CONTEXTSTREAM_ALLOW_LOCAL_MCP=1"),
            "generate-configs --help must document CONTEXTSTREAM_ALLOW_LOCAL_MCP=1:
{help}"
        );
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
    fn account_learning_requires_an_explicit_choice_and_yes_preserves_it() {
        let cli = Cli::try_parse_from(["contextstream-mcp", "setup", "--yes"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Commands::Setup {
                account_learning: None,
                ..
            })
        ));
        for command in ["setup", "configure"] {
            assert!(Cli::try_parse_from([
                "contextstream-mcp",
                command,
                "--account-learning",
                "on"
            ])
            .is_ok());
            assert!(Cli::try_parse_from([
                "contextstream-mcp",
                command,
                "--account-learning",
                "off"
            ])
            .is_ok());
            assert!(Cli::try_parse_from([
                "contextstream-mcp",
                command,
                "--account-learning",
                "true"
            ])
            .is_err());
        }

        // Bare `--account-learning` and `--deep-learning` default to `status` on `configure`.
        for argv in [
            vec!["contextstream-mcp", "configure", "--account-learning"],
            vec![
                "contextstream-mcp",
                "configure",
                "--account-learning",
                "status",
            ],
            vec!["contextstream-mcp", "configure", "--deep-learning"],
            vec![
                "contextstream-mcp",
                "configure",
                "--deep-learning",
                "status",
            ],
        ] {
            let parsed = Cli::try_parse_from(&argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
            assert!(matches!(
                parsed.command,
                Some(Commands::Configure {
                    account_learning: Some(AccountLearningAction::Status),
                    ..
                })
            ));
        }

        let parsed_on =
            Cli::try_parse_from(["contextstream-mcp", "configure", "--deep-learning", "on"])
                .unwrap();
        assert!(matches!(
            parsed_on.command,
            Some(Commands::Configure {
                account_learning: Some(AccountLearningAction::On),
                ..
            })
        ));

        // Bare `doctor --repair` parses without requiring `--scope` or `--editors`.
        let doctor_repair =
            Cli::try_parse_from(["contextstream-mcp", "doctor", "--repair"]).unwrap();
        assert!(matches!(
            doctor_repair.command,
            Some(Commands::Doctor {
                repair: true,
                scope: None,
                ..
            })
        ));
    }

    #[test]
    fn api_key_input_accepts_one_trimmed_key_only() {
        assert_eq!(parse_api_key_input("cs_key\n").unwrap(), "cs_key");
        assert_eq!(parse_api_key_input("  cs_key  ").unwrap(), "cs_key");
        assert!(parse_api_key_input("").is_err());
        assert!(parse_api_key_input(" \n").is_err());
        assert!(parse_api_key_input("cs_key other").is_err());
        assert!(parse_api_key_input("cs_key\ncs_other\n").is_err());
    }

    #[test]
    fn configure_cli_api_key_stdin_conflicts_with_other_modes() {
        assert!(Cli::try_parse_from(["contextstream-mcp", "configure", "--api-key-stdin"]).is_ok());
        for other in [["--list-options", ""], ["--transcripts", "on"]] {
            let mut argv = vec![
                "contextstream-mcp",
                "configure",
                "--api-key-stdin",
                other[0],
            ];
            if !other[1].is_empty() {
                argv.push(other[1]);
            }
            assert!(Cli::try_parse_from(argv).is_err(), "{other:?}");
        }
    }

    #[test]
    fn setup_cli_accepts_workspace_id_only_for_non_interactive_runs() {
        let cli = Cli::try_parse_from([
            "contextstream-mcp",
            "setup",
            "--yes",
            "--account-only",
            "--workspace-id",
            "11111111-1111-1111-1111-111111111111",
        ])
        .expect("--yes --workspace-id should parse");
        match cli.command {
            Some(Commands::Setup {
                workspace_id,
                account_only,
                ..
            }) => {
                assert_eq!(
                    workspace_id.as_deref(),
                    Some("11111111-1111-1111-1111-111111111111")
                );
                assert!(account_only);
            }
            _ => panic!("expected setup command"),
        }

        // The interactive wizard asks for the workspace itself.
        assert!(Cli::try_parse_from([
            "contextstream-mcp",
            "setup",
            "--workspace-id",
            "11111111-1111-1111-1111-111111111111",
        ])
        .is_err());
    }

    #[test]
    fn setup_cli_accepts_project_id_and_project_name_for_non_interactive_runs() {
        let with_id = Cli::try_parse_from([
            "contextstream-mcp",
            "setup",
            "--yes",
            "--project-id",
            "33333333-3333-4333-8333-333333333333",
        ])
        .expect("--yes --project-id should parse");
        match with_id.command {
            Some(Commands::Setup {
                project_id,
                project_name,
                ..
            }) => {
                assert_eq!(
                    project_id.as_deref(),
                    Some("33333333-3333-4333-8333-333333333333")
                );
                assert!(project_name.is_none());
            }
            _ => panic!("expected setup command"),
        }

        let with_name = Cli::try_parse_from([
            "contextstream-mcp",
            "setup",
            "--yes",
            "--project-name",
            "qa-shared-name-2",
        ])
        .expect("--yes --project-name should parse");
        match with_name.command {
            Some(Commands::Setup {
                project_id,
                project_name,
                ..
            }) => {
                assert!(project_id.is_none());
                assert_eq!(project_name.as_deref(), Some("qa-shared-name-2"));
            }
            _ => panic!("expected setup command"),
        }

        // Requires --yes, conflicts with each other, and conflicts with --account-only.
        for bad_args in [
            vec![
                "contextstream-mcp",
                "setup",
                "--project-id",
                "33333333-3333-4333-8333-333333333333",
            ],
            vec![
                "contextstream-mcp",
                "setup",
                "--project-name",
                "qa-shared-name-2",
            ],
            vec![
                "contextstream-mcp",
                "setup",
                "--yes",
                "--project-id",
                "33333333-3333-4333-8333-333333333333",
                "--project-name",
                "qa-shared-name-2",
            ],
            vec![
                "contextstream-mcp",
                "setup",
                "--yes",
                "--account-only",
                "--project-id",
                "33333333-3333-4333-8333-333333333333",
            ],
            vec![
                "contextstream-mcp",
                "setup",
                "--yes",
                "--account-only",
                "--project-name",
                "qa-shared-name-2",
            ],
        ] {
            assert!(
                Cli::try_parse_from(bad_args.clone()).is_err(),
                "expected clap rejection for {bad_args:?}"
            );
        }
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

    #[test]
    fn exclude_cli_parses_path_and_flags_after_patterns() {
        let add = Cli::try_parse_from([
            "contextstream-mcp",
            "exclude",
            "add",
            "*.log",
            "dist/",
            "--path",
            "/tmp/target-project",
            "-q",
        ])
        .expect("exclude add with trailing --path and -q should parse");
        assert!(add.quiet);
        match add.command {
            Some(Commands::Exclude {
                action: ExcludeAction::Add,
                path,
                patterns,
            }) => {
                assert_eq!(path.as_deref(), Some("/tmp/target-project"));
                assert_eq!(patterns, vec!["*.log".to_string(), "dist/".to_string()]);
            }
            _ => panic!("expected exclude add command"),
        }

        let remove = Cli::try_parse_from([
            "contextstream-mcp",
            "exclude",
            "remove",
            "*.log",
            "--path",
            "/tmp/target-project",
        ])
        .expect("exclude remove with trailing --path should parse");
        match remove.command {
            Some(Commands::Exclude {
                action: ExcludeAction::Remove,
                path,
                patterns,
            }) => {
                assert_eq!(path.as_deref(), Some("/tmp/target-project"));
                assert_eq!(patterns, vec!["*.log".to_string()]);
            }
            _ => panic!("expected exclude remove command"),
        }
    }
}
