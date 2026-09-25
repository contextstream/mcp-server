//! Setup wizard for ContextStream MCP server.
//!
//! Provides an interactive setup experience for onboarding users:
//! - Authentication (browser login or API key paste)
//! - Editor detection and configuration
//! - Workspace and project setup
//! - AI rules generation

mod credentials;
pub mod doctor;
pub mod editors;
pub mod git_hooks;
mod hooks;
mod mcp_config;
mod pending_connection;
pub mod profile;
mod prompts;
mod rules;
pub mod safe_edit;
mod team_guidance;
pub mod ui;
mod watch_service;
mod wizard;
mod wizard_config;

pub use credentials::*;
pub use hooks::install_binary;
pub use hooks::managed_binary_path;
pub use hooks::MANAGED_HOOK_ARGUMENT;
pub use pending_connection::*;
pub use prompts::*;
pub use watch_service::{
    register_managed_sync_bridge, sync_bridge_registration_status, unregister_managed_sync_bridge,
    SyncBridgeActivationState, SyncBridgeRegistrationState, SyncBridgeServiceRegistration,
};
pub use wizard_config::*;

pub use mcp_config::generate_all_configs_json;
pub use mcp_config::generate_config_json;
pub use rules::install_canonical_rules_hash;

use anyhow::{Context, Result};
use console::style;
use mcp_client::{ContextParams, ContextStreamClient, IngestLocalParams, IngestProgressEvent};
use mcp_types::{
    build_harness_teaching, Config, HarnessTeachingContract, HarnessTeachingDelivery,
    HARNESS_TEACHING_VERSION,
};
use serde::Serialize;
use std::collections::HashSet;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use tracing::warn;

/// Activity marks in the ContextCode style (see [`ui::Mark`]), carrying the
/// trailing space the old emoji constants had so existing format strings
/// keep their alignment.
struct MarkPrefix(ui::Mark);

impl std::fmt::Display for MarkPrefix {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ", ui::ui().mark(self.0))
    }
}

static CHECK: MarkPrefix = MarkPrefix(ui::Mark::Ok);
static CROSS: MarkPrefix = MarkPrefix(ui::Mark::Fail);

/// Set while setup's final hook refresh runs: setup already reported what it
/// configured, so the refresh prints only problems.
static QUIET_HOOK_REFRESH: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Progress notes from `update-hooks`, silenced during setup's own refresh.
macro_rules! refresh_note {
    ($($arg:tt)*) => {
        if !QUIET_HOOK_REFRESH.load(std::sync::atomic::Ordering::Relaxed) {
            eprintln!($($arg)*);
        }
    };
}

/// Truthful terminal/API outcome for a setup invocation.
///
/// Setup can prove that configuration files are healthy, but it cannot prove
/// that an editor has reloaded them or completed an MCP handshake. Runtime
/// connection is therefore never inferred from installer completion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SetupCompletionState {
    DryRunPreview,
    NoClientConfigured,
    RulesOnlyReady,
    RepairRequired,
    AccountOnly,
    ProjectRequired,
    IndexRequired,
    RestartRequired,
}

impl SetupCompletionState {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::DryRunPreview => "dry_run_preview",
            Self::NoClientConfigured => "no_client_configured",
            Self::RulesOnlyReady => "rules_only_ready",
            Self::RepairRequired => "repair_required",
            Self::AccountOnly => "account_only",
            Self::ProjectRequired => "project_required",
            Self::IndexRequired => "index_required",
            Self::RestartRequired => "restart_required",
        }
    }
}

/// Privacy-bounded evidence behind a setup outcome. No local paths, hostnames,
/// credentials, or free-form errors are included.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct SetupCompletionEvidence {
    pub state: SetupCompletionState,
    pub configured_editor_count: usize,
    pub mcp_editor_count: usize,
    pub project_path_selected: bool,
    pub project_selected: bool,
    pub binding_established: bool,
    pub index_started: bool,
    pub awaiting_first_files: bool,
    pub doctor_healthy: bool,
    pub account_only_requested: bool,
    pub runtime_connected: bool,
}

pub(crate) fn setup_completion_evidence(
    configured_editor_count: usize,
    mcp_editor_count: usize,
    project_path_selected: bool,
    project_selected: bool,
    binding_established: bool,
    index_started: bool,
    awaiting_first_files: bool,
    doctor_healthy: bool,
    account_only_requested: bool,
    dry_run: bool,
) -> SetupCompletionEvidence {
    let state = if dry_run {
        SetupCompletionState::DryRunPreview
    } else if configured_editor_count == 0 {
        SetupCompletionState::NoClientConfigured
    } else if !doctor_healthy {
        SetupCompletionState::RepairRequired
    } else if mcp_editor_count == 0 {
        SetupCompletionState::RulesOnlyReady
    } else if account_only_requested {
        SetupCompletionState::AccountOnly
    } else if !project_path_selected || !project_selected || !binding_established {
        SetupCompletionState::ProjectRequired
    } else if !index_started && !awaiting_first_files {
        SetupCompletionState::IndexRequired
    } else {
        SetupCompletionState::RestartRequired
    };

    SetupCompletionEvidence {
        state,
        configured_editor_count,
        mcp_editor_count,
        project_path_selected,
        project_selected,
        binding_established,
        index_started,
        awaiting_first_files,
        doctor_healthy,
        account_only_requested,
        // Only a server-observed editor handshake may set this true. Setup
        // never fabricates that evidence from files it just wrote.
        runtime_connected: false,
    }
}

pub(crate) const fn first_value_prompt() -> &'static str {
    "Use ContextStream to summarize this repository, cite the files you relied on, and tell me one active decision or risk that should affect my next change."
}

/// Content-capable setup may trust a project only when the API positively
/// attests the exact workspace relationship. A missing workspace is
/// ambiguity, not permission to persist a local upload binding.
pub fn project_workspace_is_verified(
    actual_workspace_id: Option<uuid::Uuid>,
    expected_workspace_id: uuid::Uuid,
) -> bool {
    matches!(
        classify_project_workspace(actual_workspace_id, expected_workspace_id),
        WorkspaceOwnershipEvidence::Attested
    )
}

/// What the project payload told us about workspace ownership.
///
/// The distinction that matters is *contradicted* vs *unattested*. A project
/// that names a different workspace is a hard refusal — no second opinion can
/// override it. A project that names no workspace at all is merely silence,
/// and silence is resolvable by asking a different question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceOwnershipEvidence {
    /// The payload named exactly the expected workspace.
    Attested,
    /// The payload named a different workspace.
    Contradicted,
    /// The payload carried no workspace at all.
    Unattested,
}

pub fn classify_project_workspace(
    actual_workspace_id: Option<uuid::Uuid>,
    expected_workspace_id: uuid::Uuid,
) -> WorkspaceOwnershipEvidence {
    match actual_workspace_id {
        Some(actual) if actual == expected_workspace_id => WorkspaceOwnershipEvidence::Attested,
        Some(_) => WorkspaceOwnershipEvidence::Contradicted,
        None => WorkspaceOwnershipEvidence::Unattested,
    }
}

/// Pages the workspace-scoped project listing looking for `project_id`.
///
/// The server applies `workspace_id` as a real filter, so presence in this
/// listing is a positive attestation of membership — not a weaker substitute
/// for one. Any transport failure propagates: the caller must fail closed.
async fn project_appears_in_workspace_listing(
    client: &ContextStreamClient,
    project_id: uuid::Uuid,
    workspace_id: uuid::Uuid,
) -> Result<bool> {
    const PAGE_SIZE: i64 = 100;
    // Bounded so a paginating server that never shrinks a page cannot spin
    // here forever. 50 pages is far past any real workspace.
    const MAX_PAGES: i64 = 50;

    for page in 1..=MAX_PAGES {
        let items = client
            .list_projects(Some(workspace_id), Some(page), Some(PAGE_SIZE))
            .await?;
        if items.iter().any(|project| project.id == project_id) {
            return Ok(true);
        }
        // Stop on an empty page rather than on a short one. The server clamps
        // page_size to its own maximum, so a "short" page is not evidence of
        // the last page — treating it as one would silently miss projects and
        // refuse a legitimate binding.
        if items.is_empty() {
            return Ok(false);
        }
    }

    anyhow::bail!(
        "Workspace {} lists more than {} projects; ownership of project {} could not be confirmed",
        workspace_id,
        PAGE_SIZE * MAX_PAGES,
        project_id
    )
}

/// Prove that `project_id` belongs to `expected_workspace_id`, or fail.
///
/// Ownership is confirmed from the project payload when it carries the field,
/// and otherwise from the workspace-scoped listing. Both are positive
/// attestations from the server; neither trusts local state. This deliberately
/// does not treat a missing `workspace_id` as proof of anything — it only
/// stops treating it as *disproof*, which is what made the check unsatisfiable
/// against an API that never serialized the field (MCP v0.5.47–v0.5.59).
pub async fn require_project_workspace_ownership(
    client: &ContextStreamClient,
    project_id: uuid::Uuid,
    project_workspace_id: Option<uuid::Uuid>,
    expected_workspace_id: uuid::Uuid,
) -> Result<()> {
    match classify_project_workspace(project_workspace_id, expected_workspace_id) {
        WorkspaceOwnershipEvidence::Attested => Ok(()),
        WorkspaceOwnershipEvidence::Contradicted => anyhow::bail!(
            "Project {} belongs to workspace {}, not {}",
            project_id,
            project_workspace_id
                .map(|id| id.to_string())
                .unwrap_or_else(|| "unknown".to_string()),
            expected_workspace_id
        ),
        WorkspaceOwnershipEvidence::Unattested => {
            // The project payload omitted ownership. Ask the server the
            // question it does answer: which projects are in this workspace?
            let listed =
                project_appears_in_workspace_listing(client, project_id, expected_workspace_id)
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "Could not confirm that project {} belongs to workspace {}: {}",
                            project_id,
                            expected_workspace_id,
                            error
                        )
                    })?;
            if listed {
                Ok(())
            } else {
                anyhow::bail!(
                    "Project {} is not listed in workspace {}",
                    project_id,
                    expected_workspace_id
                )
            }
        }
    }
}

/// Establish the machine-local checkout identity only from an explicit setup
/// flow and only after an uncached API ownership check. Git checkouts retain
/// their common-directory identity; explicitly selected non-Git folders get a
/// folder-local identity so the managed bridge can sync the first files that
/// appear without setup silently creating a Git repository.
pub(crate) async fn establish_validated_setup_binding(
    client: &ContextStreamClient,
    folder: &Path,
    workspace: &WorkspaceInfo,
    project: Option<&ProjectInfo>,
) -> Result<bool> {
    let workspace_id = uuid::Uuid::parse_str(&workspace.id)
        .map_err(|_| anyhow::anyhow!("Invalid selected workspace ID: {}", workspace.id))?;
    let project_id = project
        .map(|project| uuid::Uuid::parse_str(&project.id))
        .transpose()
        .map_err(|_| anyhow::anyhow!("Invalid selected project ID"))?;
    if let Some(project_id) = project_id {
        let current = client.get_project_fresh(project_id).await?;
        require_project_workspace_ownership(client, project_id, current.workspace_id, workspace_id)
            .await
            .map_err(|error| anyhow::anyhow!("{}; no checkout binding was established", error))?;
    }

    if safe_edit::is_dry_run() {
        // The editor-specific workspace config writer already records the
        // checkout config diff. Do not invoke mcp-session here: explicit
        // binding also mints an opaque fingerprint and updates the machine
        // mapping registry, neither of which may happen in a dry run.
        return Ok(true);
    }

    match mcp_session::auto_init::establish_folder_binding(
        folder.to_string_lossy().as_ref(),
        workspace_id,
        &workspace.name,
        project_id,
        project.map(|project| project.name.as_str()),
    )
    .await
    {
        Ok(_) => Ok(true),
        Err(error) => Err(anyhow::anyhow!(
            "Could not establish trusted checkout identity: {}",
            error
        )),
    }
}

/// Sanitize an indexing error string before showing it to the end user.
///
/// Removes internal infrastructure details — host:port pairs, internal
/// IPv4/IPv6 addresses, Rust transport-error type names, gRPC metadata
/// noise — and collapses to a short, actionable message. Operators
/// debugging the same incident in tracing logs still see the full
/// detail; the user just sees that something upstream is down and what
/// to do about it.
fn sanitize_index_error<E: std::fmt::Display>(error: E) -> String {
    let raw = error.to_string();

    // Common signals → user-friendly mappings, in priority order. The
    // first match wins so we don't double-classify.
    let lower = raw.to_lowercase();
    if lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("elapsed")
        || lower.contains("(504)")
        || lower.contains("connect error")
        || lower.contains("transport error")
        || lower.contains("connection refused")
    {
        return "indexing service is temporarily unreachable; keep hosted MCP configured, \
                then retry with `project(action=\"index\")` and verify the exact checkout \
                with `project(action=\"index_status\")`"
            .to_string();
    }
    if lower.contains("(429)") || lower.contains("rate limit") {
        return "indexing rate-limited; please retry in a moment".to_string();
    }
    if lower.contains("(401)") || lower.contains("unauthorized") {
        return "auth expired; re-authenticate with `contextstream-mcp setup`".to_string();
    }
    if lower.contains("(403)") {
        return "permission denied for this workspace/project — check that the \
                resolved scope matches the project you intend to index"
            .to_string();
    }

    // Generic scrub for anything else: drop URLs, host:port pairs, IP
    // addresses, and a few well-known Rust internal identifiers, then
    // collapse whitespace so the result reads as one tidy line.
    let stripped = strip_internal_infra(&raw);
    if stripped.trim().is_empty() {
        "indexing failed; keep hosted MCP configured, retry with \
         `project(action=\"index\")`, then verify with `project(action=\"index_status\")`"
            .to_string()
    } else {
        stripped
    }
}

fn strip_internal_infra(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for word in raw.split_whitespace() {
        // Drop anything that looks like a URL, host:port, IPv4, or
        // gRPC metadata block. Keep ordinary words.
        let trimmed = word.trim_matches(|c: char| matches!(c, '(' | ')' | ',' | ':' | ';' | '"'));
        let lower = trimmed.to_ascii_lowercase();
        let drop = lower.starts_with("http://")
            || lower.starts_with("https://")
            || lower.starts_with("grpc://")
            || lower.contains("metadatamap")
            || lower.contains("tonic::")
            || lower.contains("hyper::")
            || lower.contains("connecterror")
            || is_ip_or_host_port(trimmed);
        if !drop {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(word);
        }
    }
    // Collapse repeated punctuation that's left after dropping fields
    // like "Failed to connect to <url>:" — that trailing colon looks
    // weird without the URL.
    out.replace(": :", ":")
        .replace("()", "")
        .replace("  ", " ")
        .trim_end_matches([':', '.', ',', ' '])
        .to_string()
}

fn is_ip_or_host_port(value: &str) -> bool {
    // Quick check: contains ":" with digits on both sides, OR looks like
    // an IPv4 (four dot-separated digit groups), OR ends in a port suffix.
    let stripped = value.trim_end_matches('/').trim_start_matches('/');
    let mut octets = 0;
    let mut has_only_ip_chars = true;
    for ch in stripped.chars() {
        if ch == '.' {
            octets += 1;
        } else if !ch.is_ascii_digit() && ch != ':' {
            has_only_ip_chars = false;
            break;
        }
    }
    if has_only_ip_chars && octets >= 3 && stripped.contains('.') {
        return true;
    }
    if let Some(idx) = stripped.rfind(':') {
        let port = &stripped[idx + 1..];
        if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) {
            return true;
        }
    }
    false
}
const ENV_SETUP_TRANSPORT: &str = "CONTEXTSTREAM_SETUP_TRANSPORT";
const ENV_ALLOW_LOCAL_MCP: &str = "CONTEXTSTREAM_ALLOW_LOCAL_MCP";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SetupIndexChoice {
    Foreground,
    Background,
    Skip,
}

fn info_label() -> String {
    format!("{} ", ui::ui().mark(ui::Mark::Info))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupTransportPreference {
    HostedRemote,
    LocalBinary,
}

impl SetupTransportPreference {
    fn as_marker_value(self) -> &'static str {
        match self {
            SetupTransportPreference::HostedRemote => "remote",
            SetupTransportPreference::LocalBinary => "local",
        }
    }
}

fn parse_setup_transport_preference(value: &str) -> Option<SetupTransportPreference> {
    match value.trim().to_ascii_lowercase().as_str() {
        "remote" | "hosted" | "hosted-remote" => Some(SetupTransportPreference::HostedRemote),
        "local" | "binary" | "local-binary" => Some(SetupTransportPreference::LocalBinary),
        _ => None,
    }
}

fn local_mcp_override_allowed_from_env_value(value: Option<&str>) -> bool {
    matches!(
        value.map(|value| value.trim().to_ascii_lowercase()),
        Some(value) if matches!(value.as_str(), "1" | "true" | "yes" | "allow" | "recovery")
    )
}

pub fn local_mcp_override_allowed() -> bool {
    local_mcp_override_allowed_from_env_value(std::env::var(ENV_ALLOW_LOCAL_MCP).ok().as_deref())
}

/// Local-mode MCP config generation is allowed when EITHER of these consent
/// signals is present:
///
/// 1. `CONTEXTSTREAM_ALLOW_LOCAL_MCP` env var is set to a truthy value (the
///    one-shot/recovery override — works without persistent setup state).
/// 2. The persisted transport-mode marker (`~/.contextstream/setup-transport-mode`)
///    is `local` (the persistent opt-in written by the setup wizard or by an
///    explicit `update-configs` flow).
///
/// Treating the marker as a valid consent signal removes the friction where
/// agents iterating in a configured local-mode environment (e.g. `cstest`)
/// would otherwise have to remember to export the env var on every shell.
pub fn local_mcp_allowed() -> bool {
    local_mcp_override_allowed()
        || matches!(
            read_setup_transport_marker(),
            Some(SetupTransportPreference::LocalBinary)
        )
}

fn take_preselected_setup_transport_preference() -> Option<SetupTransportPreference> {
    static PRESELECTED: OnceLock<Mutex<Option<SetupTransportPreference>>> = OnceLock::new();

    let slot = PRESELECTED.get_or_init(|| {
        Mutex::new(
            std::env::var(ENV_SETUP_TRANSPORT)
                .ok()
                .and_then(|value| parse_setup_transport_preference(&value)),
        )
    });

    slot.lock().ok()?.take()
}

pub fn setup_transport_marker_path() -> PathBuf {
    contextstream_config_dir().join("setup-transport-mode")
}

pub fn write_setup_transport_marker(mode: SetupTransportPreference) -> Result<()> {
    let path = setup_transport_marker_path();
    // This is wholly generated state, not a user-authored config. Match the
    // shell installer's trailing newline and avoid accumulating recovery
    // sidecars for a two-value marker.
    safe_edit::write_owned_file_if_changed(&path, &format!("{}\n", mode.as_marker_value()))?;
    let backup = safe_edit::backup_path(&path)?;
    if let Some(existing) = safe_edit::read_recovery_file(&backup)? {
        safe_edit::remove_owned_file_if_unchanged(&backup, &existing)?;
    }
    Ok(())
}

/// Read the persisted transport-mode marker, if any.
///
/// Returns `None` when the marker file does not exist or contains an
/// unrecognized value. Setup writes this file when the user chooses local vs
/// remote transport; `update_configs` consults it to keep editor MCP configs
/// aligned with the user's stated preference.
pub fn read_setup_transport_marker() -> Option<SetupTransportPreference> {
    read_setup_transport_marker_result().ok().flatten()
}

/// Read the transport marker without conflating missing state with malformed
/// or unreadable state. Config-refresh commands use this form so damage to the
/// marker cannot silently switch a user's editors from local to remote mode.
pub fn read_setup_transport_marker_result() -> Result<Option<SetupTransportPreference>> {
    let path = setup_transport_marker_path();
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Could not read transport marker {}", path.display()))
        }
    };
    parse_setup_transport_preference(&raw)
        .map(Some)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Invalid transport marker at {}; expected 'remote' or 'local'. Refusing to rewrite editor configs.",
                path.display()
            )
        })
}

/// Resolve which editors a non-interactive hook refresh may touch.
///
/// Hooks are written into shared, hand-maintained config files, so a refresh
/// must never reach an editor the user did not opt into:
///
/// 1. an explicit `--editors` list always wins;
/// 2. otherwise the editors setup actually configured on this machine;
/// 3. detection only as a last resort, when setup has never recorded a choice
///    (pre-existing installs upgrading into this behavior).
///
/// Returns the editors plus the provenance label used in operator output.
///
/// `only_configured` suppresses the detection fallback entirely — unattended
/// refreshes from install scripts use it so they can only ever *refresh* an
/// existing choice, never make one.
fn resolve_hook_refresh_editors(
    explicit: Option<&[editors::Editor]>,
    only_configured: bool,
) -> Result<(Vec<editors::Editor>, &'static str)> {
    // Explicit scope is fully self-contained and must not be blocked by stale
    // or malformed historical installation state.
    if let Some(explicit) = explicit {
        return Ok((dedupe_editors(explicit.iter().copied()), "requested"));
    }
    let selection = mcp_client::activation::configured_client_selection()
        .context("Could not read the saved editor selection")?;
    Ok(resolve_hook_refresh_editors_from(
        explicit,
        only_configured,
        &selection.clients,
        selection.recorded,
        editors::detect_installed_editors,
    ))
}

/// Pure core of [`resolve_hook_refresh_editors`], with the machine-state reads
/// injected so the precedence rules are testable.
fn resolve_hook_refresh_editors_from(
    explicit: Option<&[editors::Editor]>,
    only_configured: bool,
    configured_ids: &[String],
    selection_recorded: bool,
    detect: impl FnOnce() -> Vec<editors::Editor>,
) -> (Vec<editors::Editor>, &'static str) {
    if let Some(list) = explicit {
        return (dedupe_editors(list.iter().copied()), "requested");
    }

    let configured = dedupe_editors(
        configured_ids
            .iter()
            .filter_map(|id| editors::Editor::from_id(id)),
    );

    if selection_recorded || !configured.is_empty() {
        return (configured, "configured by setup");
    }

    if only_configured {
        return (Vec::new(), "configured by setup");
    }

    (dedupe_editors(detect()), "detected")
}

fn dedupe_editors(editors: impl IntoIterator<Item = editors::Editor>) -> Vec<editors::Editor> {
    let mut seen = HashSet::new();
    editors
        .into_iter()
        .filter(|editor| seen.insert(*editor))
        .collect()
}

fn validate_editor_scope(scope: &str, operation: &str) -> Result<()> {
    if matches!(scope, "global" | "project" | "all") {
        Ok(())
    } else {
        anyhow::bail!(
            "Unknown scope '{}' for {}. Supported values: global, project, all.",
            scope,
            operation
        )
    }
}

/// Remove ContextStream from the targeted editors: hooks, MCP config entries,
/// and rules files.
///
/// Deliberately surgical rather than "delete the files we know about" — every
/// one of these files can contain user content we never wrote, so each removal
/// path strips only the ContextStream-owned parts and leaves the rest.
pub async fn uninstall(
    scope: &str,
    only: Option<&[editors::Editor]>,
    include_git_hooks: bool,
) -> Result<()> {
    validate_editor_scope(scope, "uninstall")?;
    let include_global = matches!(scope, "global" | "all");
    let include_project = matches!(scope, "project" | "all");

    let cwd = std::env::current_dir().ok();
    let mut failures = Vec::new();
    let mut selection_read_failed = false;

    // For removal, detection is the right fallback: we want to find and clean
    // every editor we may have touched, including ones a stale selection
    // record no longer mentions.
    let targets = match only {
        Some(list) => dedupe_editors(list.iter().copied()),
        None => {
            let configured = match mcp_client::activation::configured_clients() {
                Ok(configured) => configured,
                Err(error) => {
                    // Installation state is not an editor config and must not
                    // prevent us from attempting surgical cleanup everywhere
                    // detection can still prove relevant. Preserve the state
                    // unchanged and return a failure after cleanup.
                    warn!("Could not read the saved editor selection: {}", error);
                    failures.push(format!("saved editor selection: {error}"));
                    selection_read_failed = true;
                    Vec::new()
                }
            };
            dedupe_editors(
                configured
                    .iter()
                    .filter_map(|id| editors::Editor::from_id(id))
                    .chain(editors::detect_installed_editors()),
            )
        }
    };

    if targets.is_empty() {
        eprintln!("No editors to clean up.");
    } else {
        eprintln!(
            "{} Removing ContextStream from {} editor(s): {}",
            info_label(),
            targets.len(),
            selected_editors_summary(&targets)
        );
    }

    for editor in &targets {
        let mut processed: Vec<&str> = Vec::new();

        if include_global {
            match hooks::uninstall_hooks(editor) {
                Ok(()) => {
                    if editor.has_hooks() {
                        processed.push("hooks");
                    }
                }
                Err(e) => {
                    warn!(
                        "Could not remove hooks for {}: {}",
                        editor.display_name(),
                        e
                    );
                    failures.push(format!("{} hooks: {e}", editor.display_name()));
                }
            }

            match mcp_config::remove_contextstream_from_mcp_config(editor) {
                Ok(()) => processed.push("global MCP config"),
                Err(e) => {
                    warn!(
                        "Could not remove global MCP config for {}: {}",
                        editor.display_name(),
                        e
                    );
                    failures.push(format!("{} global MCP config: {e}", editor.display_name()));
                }
            }

            match rules::remove_contextstream_from_rules(editor, None) {
                Ok(true) => processed.push("global rules"),
                Ok(false) => {}
                Err(e) => {
                    warn!(
                        "Could not remove global rules for {}: {}",
                        editor.display_name(),
                        e
                    );
                    failures.push(format!("{} global rules: {e}", editor.display_name()));
                }
            }
        }

        if include_project {
            if let Some(ref project_path) = cwd {
                match mcp_config::remove_contextstream_from_project_mcp_config(editor, project_path)
                {
                    Ok(true) => processed.push("project MCP config"),
                    Ok(false) => {}
                    Err(e) => {
                        warn!(
                            "Could not remove project MCP config for {}: {}",
                            editor.display_name(),
                            e
                        );
                        failures.push(format!("{} project MCP config: {e}", editor.display_name()));
                    }
                }

                match rules::remove_contextstream_from_rules(editor, Some(project_path)) {
                    Ok(true) => processed.push("project rules"),
                    Ok(false) => {}
                    Err(e) => {
                        warn!(
                            "Could not remove project rules for {}: {}",
                            editor.display_name(),
                            e
                        );
                        failures.push(format!("{} project rules: {e}", editor.display_name()));
                    }
                }
            }
        }

        if processed.is_empty() {
            println!("  {} {} (nothing to remove)", CHECK, editor.display_name());
        } else {
            println!(
                "  {} {} — checked {}",
                CHECK,
                style(editor.display_name()).bold(),
                processed.join(", ")
            );
        }
    }

    // Readiness is evidence, not authorization, and it must never outlive an
    // explicit uninstall for the targeted harnesses. Remove it even when an
    // individual editor cleanup failed: retaining a positive readiness claim
    // after the user asked us to uninstall would be the less trustworthy
    // outcome. Project-only removals conservatively demote the harness too;
    // the next successful config/rules/runtime observation can re-establish
    // the applicable stages.
    if !targets.is_empty() && crate::readiness_evidence_writes_enabled() {
        let target_harnesses: Vec<_> = targets.iter().map(editors::Editor::harness_id).collect();
        if safe_edit::is_dry_run() {
            match mcp_client::harness_readiness::read_harness_readiness() {
                Ok(Some(ledger))
                    if mcp_client::harness_readiness::has_evidence_for(
                        &ledger,
                        &target_harnesses,
                    ) =>
                {
                    safe_edit::record_external_change(
                        &mcp_client::harness_readiness::harness_readiness_path(),
                        safe_edit::ChangeAction::Modify,
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    warn!("Could not inspect harness readiness evidence: {}", error);
                    failures.push(format!("harness readiness evidence: {error}"));
                }
            }
        } else if let Err(error) =
            mcp_client::harness_readiness::remove_harnesses(&target_harnesses)
        {
            warn!("Could not remove harness readiness evidence: {}", error);
            failures.push(format!("harness readiness evidence: {error}"));
        }
    }

    // Global uninstall is also a durable de-enrolment. Without this update,
    // a later detached `--only-configured` refresh would recreate the MCP
    // config and hooks that uninstall just removed. Perform it after all file
    // cleanup attempts, but even when one of those attempts failed: preserving
    // the user's intent is safer than letting an unattended process reinstall.
    if include_global && !targets.is_empty() && !selection_read_failed {
        let target_ids: Vec<String> = targets
            .iter()
            .map(|editor| editor.id().to_string())
            .collect();
        if safe_edit::is_dry_run() {
            match mcp_client::activation::configured_client_selection() {
                Ok(selection)
                    if selection
                        .clients
                        .iter()
                        .any(|client| target_ids.contains(client)) =>
                {
                    let path = dirs::home_dir()
                        .map(|home| home.join(".contextstream").join("installation.json"))
                        .unwrap_or_else(|| {
                            PathBuf::from(".contextstream").join("installation.json")
                        });
                    safe_edit::record_external_change(&path, safe_edit::ChangeAction::Modify);
                }
                Ok(_) => {}
                Err(error) => {
                    warn!("Could not read the saved editor selection: {}", error);
                    failures.push(format!("saved editor selection: {error}"));
                }
            }
        } else if let Err(error) = mcp_client::activation::remove_configured_clients(&target_ids) {
            warn!("Could not update the saved editor selection: {}", error);
            failures.push(format!("saved editor selection: {error}"));
        }
    }

    // Remove machine startup registration only when this global uninstall
    // leaves no configured harnesses. A scoped uninstall must never break
    // content freshness for another editor that still uses hosted MCP.
    if include_global && !targets.is_empty() && !selection_read_failed {
        let target_ids: HashSet<&str> = targets.iter().map(editors::Editor::id).collect();
        let remaining_clients = match mcp_client::activation::configured_clients() {
            Ok(configured) if safe_edit::is_dry_run() => Some(
                configured
                    .into_iter()
                    .filter(|client| !target_ids.contains(client.as_str()))
                    .collect::<Vec<_>>(),
            ),
            Ok(configured) => Some(configured),
            Err(error) => {
                warn!(
                    "Could not verify remaining editor selection before sync bridge cleanup: {}",
                    error
                );
                failures.push(format!("sync bridge editor selection: {error}"));
                None
            }
        };
        if remaining_clients.as_ref().is_some_and(Vec::is_empty) {
            match unregister_managed_sync_bridge() {
                Ok(registration) if registration.changed => {
                    println!("  {} hosted sync bridge startup registration", CHECK);
                }
                Ok(_) => {}
                Err(error) => {
                    warn!(
                        "Could not remove hosted sync bridge registration: {}",
                        error
                    );
                    failures.push(format!("hosted sync bridge registration: {error}"));
                }
            }
            match crate::watch::request_sync_bridge_stop() {
                Ok(true) => {
                    println!("  {} hosted sync bridge process stop requested", CHECK);
                }
                Ok(false) => {}
                Err(error) => {
                    warn!("Could not stop the hosted sync bridge process: {}", error);
                    failures.push(format!("hosted sync bridge process: {error}"));
                }
            }
        }
    }

    if include_git_hooks {
        if let Some(ref project_path) = cwd {
            if let Some(repo_root) = git_hooks::resolve_repo_root(project_path) {
                match git_hooks::uninstall_git_hooks(&repo_root) {
                    Ok(()) => println!("  {} managed git hooks", CHECK),
                    Err(e) => {
                        warn!("Could not remove git hooks: {}", e);
                        failures.push(format!("managed git hooks: {e}"));
                    }
                }
            }
        }
    }

    if !safe_edit::is_dry_run() {
        println!();
        println!(
            "  {}",
            style(
                "Credentials in ~/.contextstream were left in place. \
                 Delete that directory to remove them too."
            )
            .dim()
        );
    }

    if failures.is_empty() {
        Ok(())
    } else {
        anyhow::bail!(
            "{} uninstall operation(s) failed: {}",
            failures.len(),
            failures.join("; ")
        )
    }
}

/// Print everything a dry run planned, then clear the recording.
///
/// Answers "what will this actually touch?" without touching anything — the
/// question users otherwise resolve by installing into a throwaway editor
/// profile and diffing by hand.
pub fn report_dry_run() {
    let changes = safe_edit::take_planned_changes();

    println!();
    println!(
        "{} {}",
        style("DRY RUN").yellow().bold(),
        style("no files were modified").dim()
    );
    println!();

    if changes.is_empty() {
        println!("  {}", style("Nothing to change.").dim());
        println!();
        return;
    }

    let mut created = 0usize;
    let mut modified = 0usize;
    let mut unchanged = 0usize;
    let mut deleted = 0usize;

    for change in &changes {
        match change.action {
            safe_edit::ChangeAction::Create => created += 1,
            safe_edit::ChangeAction::Modify => modified += 1,
            safe_edit::ChangeAction::Unchanged => unchanged += 1,
            safe_edit::ChangeAction::Delete => deleted += 1,
        }

        let marker = match change.action {
            safe_edit::ChangeAction::Create => style("+ create  ").green(),
            safe_edit::ChangeAction::Modify => style("~ modify  ").yellow(),
            safe_edit::ChangeAction::Unchanged => style("= unchanged").dim(),
            safe_edit::ChangeAction::Delete => style("- delete  ").red(),
        };
        println!("  {} {}", marker, change.path.display());

        if !change.summary.is_empty() {
            for line in change.summary.lines() {
                println!("  {}", style(line).dim());
            }
        }
    }

    println!();
    let unique_paths = changes
        .iter()
        .map(|change| change.path.as_path())
        .collect::<HashSet<_>>()
        .len();
    println!(
        "  {} planned operation(s) across {} path(s): {} create, {} modify, {} delete, {} unchanged",
        changes.len(),
        unique_paths,
        created,
        modified,
        deleted,
        unchanged
    );
    println!("  {}", style("Re-run without --dry-run to apply.").cyan());
    println!();
}

/// Message shown when scoping resolves to no editors at all.
fn no_target_editors_message(only_configured: bool) -> String {
    if only_configured {
        format!(
            "{} No editors configured by setup yet; leaving editor configs untouched. \
             Run `contextstream-mcp setup` to choose editors.",
            info_label()
        )
    } else {
        "No supported editors to update.".to_string()
    }
}

fn editor_needs_managed_helper(editor: &editors::Editor) -> bool {
    editor.has_mcp_transport() || editor.has_hooks()
}

fn targets_need_managed_helper(targets: &[editors::Editor]) -> bool {
    targets.iter().any(editor_needs_managed_helper)
}

fn targets_need_hosted_sync_bridge(targets: &[editors::Editor], hosted_transport: bool) -> bool {
    hosted_transport && targets.iter().any(editors::Editor::has_mcp_transport)
}

/// Non-interactive hook update for the editors setup configured.
///
/// Called by setup scripts (setup.sh/setup-beta.sh/setup.ps1/setup-beta.ps1) as:
/// `contextstream-mcp update-hooks --scope=global`
pub async fn update_hooks(scope: &str, only: Option<&[editors::Editor]>) -> Result<()> {
    update_hooks_scoped(scope, only, false).await
}

/// Setup's closing hook refresh: the same idempotent validation as
/// `update-hooks --only-configured`, printing only problems.
pub async fn refresh_hooks_after_setup() -> Result<()> {
    QUIET_HOOK_REFRESH.store(true, std::sync::atomic::Ordering::Relaxed);
    let result = update_hooks_scoped("global", None, true).await;
    QUIET_HOOK_REFRESH.store(false, std::sync::atomic::Ordering::Relaxed);
    result
}

/// Non-interactive hook update, scoped to the editors the user opted into.
pub async fn update_hooks_scoped(
    scope: &str,
    only: Option<&[editors::Editor]>,
    only_configured: bool,
) -> Result<()> {
    validate_editor_scope(scope, "update-hooks")?;
    if matches!(scope, "project" | "all") {
        refresh_note!(
            "{} update-hooks currently installs global hook files only; project scope is ignored.",
            info_label()
        );
    }

    let (targets, provenance) = resolve_hook_refresh_editors(only, only_configured)?;
    if targets.is_empty() {
        refresh_note!("{}", no_target_editors_message(only_configured));
        return Ok(());
    }
    let mut failures = Vec::new();
    let hook_target_count = targets.iter().filter(|editor| editor.has_hooks()).count();

    let hosted_transport = !matches!(
        read_setup_transport_marker_result()?,
        Some(SetupTransportPreference::LocalBinary)
    );
    let helper_ready = if targets_need_managed_helper(&targets) {
        match hooks::ensure_managed_binary_installed() {
            Ok(_) => true,
            Err(error) => {
                eprintln!(
                    "{} Could not install the managed helper; hook files were left untouched: {}",
                    CROSS, error
                );
                failures.push(format!("managed helper binary: {error}"));
                false
            }
        }
    } else {
        true
    };
    refresh_note!(
        "{} Updating hooks for {} editor(s) ({}): {}",
        info_label(),
        targets.len(),
        provenance,
        selected_editors_summary(&targets)
    );
    let mut updated_count = 0;
    for editor in &targets {
        if !editor.has_hooks() {
            if matches!(editor, editors::Editor::KiloCode) {
                refresh_note!(
                    "{} {} does not support filesystem hooks; skipping.",
                    info_label(),
                    editor.display_name()
                );
            }
            continue; // Skip editors without hook support
        }
        if !helper_ready {
            continue;
        }

        // Each installer removes only exact managed entries in memory and
        // commits once. Avoiding an uninstall/install pair preserves the
        // original recovery backup and makes refreshes true no-ops.
        match hooks::install_hooks(editor, None) {
            Ok(()) => {
                refresh_note!("{} Hooks updated for {}", CHECK, editor.display_name());
                updated_count += 1;
            }
            Err(e) => {
                eprintln!(
                    "{} Could not update hooks for {}: {}",
                    CROSS,
                    editor.display_name(),
                    e
                );
                failures.push(format!("{} hooks: {e}", editor.display_name()));
            }
        }
    }

    if updated_count == 0 && hook_target_count == 0 {
        refresh_note!("No editors with hook support found.");
    }

    // Hosted MCP remains the editor transport. The managed helper is a
    // machine-local sync bridge only: register it for login/restart recovery,
    // then launch the singleton immediately as a best-effort fast path.
    if targets_need_hosted_sync_bridge(&targets, hosted_transport)
        && helper_ready
        && crate::watch::watch_enabled()
    {
        match register_managed_sync_bridge() {
            Ok(registration) => {
                refresh_note!(
                    "{} Hosted sync bridge registration: {}",
                    CHECK,
                    registration.platform
                );
                if !safe_edit::is_dry_run() {
                    crate::watch::spawn_watch_helper();
                }
            }
            Err(error) => {
                eprintln!(
                    "{} Could not register the hosted sync bridge: {}",
                    CROSS, error
                );
                failures.push(format!("hosted sync bridge: {error}"));
            }
        }
    }

    if helper_ready {
        // update-hooks is a global-only command. Repairing a project MCP file
        // here would make an unattended hook refresh mutate whichever
        // directory happened to launch it.
        match mcp_config::repair_deleted_binary_path_configs(&targets, None) {
            Ok(repaired) if repaired > 0 => {
                refresh_note!(
                    "{} Repaired {} stale local MCP config path{}",
                    CHECK,
                    repaired,
                    if repaired == 1 { "" } else { "s" }
                );
            }
            Ok(_) => {}
            Err(e) => {
                warn!(
                    "Could not repair stale local MCP config paths during hook refresh: {}",
                    e
                );
                failures.push(format!("stale local MCP config repair: {e}"));
            }
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        anyhow::bail!(
            "{} hook refresh operation(s) failed: {}",
            failures.len(),
            failures.join("; ")
        )
    }
}

pub async fn ensure_authenticated_api_key() -> Result<String> {
    if let Some(existing_api_key) = get_api_key_result()? {
        let client = ContextStreamClient::new(Config {
            api_key: Some(existing_api_key.clone()),
            ..Default::default()
        });

        if client.me().await.is_ok() {
            return Ok(existing_api_key);
        }

        warning("Saved ContextStream credentials are no longer valid. Please authenticate again.");
    }

    let (api_key, user_email) = authenticate().await?;
    println!(
        "{}{} Authenticated as {}",
        CHECK,
        style("Success!").green(),
        style(&user_email).cyan()
    );
    write_saved_credentials(&api_key, None)?;
    println!(
        "{} Credentials saved to {}",
        CHECK,
        style(credentials_file_path().display()).dim()
    );

    Ok(api_key)
}

/// Non-interactive MCP config update for all detected editors.
///
/// Called by setup scripts after binary install/update as:
/// `contextstream-mcp update-configs --scope=global`
pub async fn update_configs(scope: &str) -> Result<()> {
    update_configs_scoped(scope, None, false).await
}

/// Non-interactive MCP-config refresh, scoped to the editors the user chose.
pub async fn update_configs_scoped(
    scope: &str,
    only: Option<&[editors::Editor]>,
    only_configured: bool,
) -> Result<()> {
    update_configs_scoped_with_interactivity(
        scope,
        only,
        only_configured,
        std::io::stdin().is_terminal(),
        false,
    )
    .await
}

/// Doctor repair is deliberately non-interactive: a diagnostic/repair command
/// must never pause on a project picker after the caller already supplied its
/// editor and surface scope.
pub(super) async fn update_configs_scoped_noninteractive(
    scope: &str,
    only: Option<&[editors::Editor]>,
    only_configured: bool,
) -> Result<()> {
    update_configs_scoped_with_interactivity(scope, only, only_configured, false, true).await
}

async fn update_configs_scoped_with_interactivity(
    scope: &str,
    only: Option<&[editors::Editor]>,
    only_configured: bool,
    interactive: bool,
    require_credentials: bool,
) -> Result<()> {
    validate_editor_scope(scope, "update-configs")?;
    let (detected, provenance) = resolve_hook_refresh_editors(only, only_configured)?;
    if detected.is_empty() {
        eprintln!("{}", no_target_editors_message(only_configured));
        return Ok(());
    }

    let api_key = match get_api_key_result()? {
        Some(key) => key,
        None => {
            if require_credentials {
                anyhow::bail!(
                    "No API key found. Run 'contextstream-mcp setup' first; no MCP configs were changed."
                );
            }
            eprintln!("No API key found. Run 'contextstream-mcp setup' first.");
            return Ok(());
        }
    };

    // A local config is unusable without the managed helper. Prove that the
    // helper can be installed before resolving or writing any project/editor
    // configuration so a copy failure cannot leave newly broken MCP entries.
    let use_local_transport = matches!(
        read_setup_transport_marker_result()?,
        Some(SetupTransportPreference::LocalBinary)
    );
    let has_mcp_targets = detected.iter().any(editors::Editor::has_mcp_transport);
    if use_local_transport && has_mcp_targets {
        hooks::ensure_managed_binary_installed()
            .context("Could not install the managed helper; no MCP configs were changed")?;
    }

    let include_project = scope == "project" || scope == "all";
    let cwd = if include_project {
        Some(
            std::env::current_dir()
                .context("Could not resolve the project directory; no MCP configs were changed")?,
        )
    } else {
        None
    };
    let local_config = match cwd.as_ref() {
        Some(project_path) => read_project_config(project_path)?,
        None => None,
    };
    let workspace_id = local_config
        .as_ref()
        .and_then(|cfg| cfg.workspace_id.clone());
    let workspace_name = local_config
        .as_ref()
        .and_then(|cfg| cfg.workspace_name.clone());
    let mut project_id = local_config.as_ref().and_then(|cfg| cfg.project_id.clone());

    let client = ContextStreamClient::new(Config {
        api_key: Some(api_key.clone()),
        ..Default::default()
    });

    if (scope == "project" || scope == "all") && cwd.is_some() {
        if let Some(project_path) = cwd.as_ref() {
            let workspace = workspace_id.as_ref().map(|id| WorkspaceInfo {
                id: id.clone(),
                name: workspace_name
                    .clone()
                    .unwrap_or_else(|| "Current workspace".to_string()),
            });
            let resolved_project = select_project_for_current_directory(
                &client,
                project_path,
                workspace.as_ref(),
                interactive,
                interactive,
                false,
            )
            .await?;

            if let Some(project) = resolved_project {
                project_id = Some(project.id);
            } else if workspace.is_some() {
                project_id = None;
            }

            // Keep local project link aligned with whichever project will be written
            // into editor MCP configs. This prevents stale project_id reuse.
            if let (Some(ws_id), Some(ws_name)) =
                (workspace_id.as_deref(), workspace_name.as_deref())
            {
                let project_name = project_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("project");
                mcp_config::write_workspace_config(
                    project_path,
                    ws_id,
                    ws_name,
                    Some(project_name),
                    project_id.as_deref(),
                )
                .context(
                    "Could not update the local workspace config; no editor configs were changed",
                )?;
            }
        }
    }

    let workspace_id_ref = workspace_id.as_deref();
    let project_id_ref = project_id.as_deref();

    let mut failures = Vec::new();
    eprintln!(
        "{} Updating MCP configs for {} editor(s) ({}): {}",
        info_label(),
        detected.len(),
        provenance,
        selected_editors_summary(&detected)
    );

    // Honor the persisted transport-mode marker so users who set up local
    // transport via the wizard keep getting local-binary configs on every
    // subsequent `update-configs`. Default (no marker, or marker=remote) is
    // hosted-remote, matching prior behavior.
    let transport_label = if use_local_transport {
        "Local binary"
    } else {
        "Hosted remote"
    };

    for editor in &detected {
        if !editor.has_mcp_transport() {
            eprintln!(
                "{} {} is rules-only; no MCP config or sync bridge is required.",
                info_label(),
                editor.display_name()
            );
            continue;
        }

        // Update global MCP config
        if scope == "global" || scope == "all" {
            let write_result = if use_local_transport {
                mcp_config::write_mcp_config_force_local(
                    editor,
                    &api_key,
                    workspace_id_ref,
                    // Global editor configuration is workspace-scoped. A
                    // project id belongs only in the project-local config.
                    None,
                    None,
                    None,
                )
            } else {
                mcp_config::write_mcp_config_force_remote_with_auth(
                    editor,
                    &api_key,
                    workspace_id_ref,
                    None,
                    None,
                    None,
                    Some(&api_key),
                )
            };
            match write_result {
                Ok(()) => {
                    eprintln!(
                        "{} {} MCP config updated for {}",
                        CHECK,
                        transport_label,
                        editor.display_name()
                    );
                }
                Err(e) => {
                    warn!(
                        "Could not update MCP config for {}: {}",
                        editor.display_name(),
                        e
                    );
                    failures.push(format!("{} global MCP config: {e}", editor.display_name()));
                }
            }
        }

        // Update project-level MCP config
        if include_project && editor.supports_project_mcp_config() {
            if let Some(project_path) = cwd.as_ref() {
                let project_write_result = if use_local_transport {
                    mcp_config::write_project_mcp_config_force_local(
                        editor,
                        project_path,
                        &api_key,
                        workspace_id_ref,
                        project_id_ref,
                        None,
                        None,
                    )
                } else {
                    mcp_config::write_project_mcp_config_force_remote_with_auth(
                        editor,
                        project_path,
                        &api_key,
                        workspace_id_ref,
                        project_id_ref,
                        None,
                        None,
                        Some(&api_key),
                    )
                };
                match project_write_result {
                    Ok(()) => {
                        eprintln!(
                            "{} Project {} MCP config updated for {}",
                            CHECK,
                            transport_label.to_lowercase(),
                            editor.display_name()
                        );
                        if matches!(editor, editors::Editor::Codex) {
                            if let Err(error) = mcp_config::ensure_codex_project_trust(project_path)
                            {
                                failures.push(format!("Codex project trust: {error}"));
                            }
                        }
                    }
                    Err(e) => {
                        warn!(
                            "Could not update project MCP config for {}: {}",
                            editor.display_name(),
                            e
                        );
                        failures.push(format!("{} project MCP config: {e}", editor.display_name()));
                    }
                }
            }
        }
    }

    if !use_local_transport && has_mcp_targets && crate::watch::watch_enabled() {
        match hooks::ensure_managed_binary_installed()
            .and_then(|_| register_managed_sync_bridge().map(|_| ()))
        {
            Ok(()) => {
                if !safe_edit::is_dry_run() {
                    crate::watch::spawn_watch_helper();
                }
            }
            Err(error) => failures.push(format!("hosted sync bridge: {error}")),
        }
    }

    // Updating editor configuration is not authorization to read and upload a
    // checkout. Indexing remains an explicit setup/index action where checkout
    // identity and current API ownership can be proven immediately beforehand.

    if failures.is_empty() {
        Ok(())
    } else {
        anyhow::bail!(
            "{} MCP config refresh operation(s) failed: {}",
            failures.len(),
            failures.join("; ")
        )
    }
}

/// Update default transcript settings in MCP config env for detected editors.
///
/// Setup discloses the enabled defaults and this command lets users change
/// them explicitly through `contextstream-mcp configure`.
pub async fn update_transcript_defaults(
    scope: &str,
    transcripts_enabled: Option<bool>,
    hook_transcripts_enabled: Option<bool>,
) -> Result<()> {
    update_transcript_defaults_scoped(
        scope,
        transcripts_enabled,
        hook_transcripts_enabled,
        None,
        false,
    )
    .await
}

/// Update transcript defaults with the same explicit > configured > detected
/// editor precedence as every other config refresh command.
pub async fn update_transcript_defaults_scoped(
    scope: &str,
    transcripts_enabled: Option<bool>,
    hook_transcripts_enabled: Option<bool>,
    only: Option<&[editors::Editor]>,
    only_configured: bool,
) -> Result<()> {
    if transcripts_enabled.is_none() && hook_transcripts_enabled.is_none() {
        return Ok(());
    }
    validate_editor_scope(scope, "transcript-default update")?;
    let (detected, _) = resolve_hook_refresh_editors(only, only_configured)?;
    if detected.is_empty() {
        eprintln!("{}", no_target_editors_message(only_configured));
        return Ok(());
    }

    let api_key = match get_api_key_result()? {
        Some(key) => key,
        None => {
            eprintln!("No API key found. Run 'contextstream-mcp setup' first.");
            return Ok(());
        }
    };

    let include_project = scope == "project" || scope == "all";
    let cwd = if include_project {
        Some(std::env::current_dir().context(
            "Could not resolve the project directory; transcript defaults were not changed",
        )?)
    } else {
        None
    };
    let local_config = match cwd.as_ref() {
        Some(project_path) => read_project_config(project_path)?,
        None => None,
    };
    let workspace_id = local_config
        .as_ref()
        .and_then(|cfg| cfg.workspace_id.clone());
    let project_id = local_config.as_ref().and_then(|cfg| cfg.project_id.clone());

    let workspace_id_ref = workspace_id.as_deref();
    let project_id_ref = project_id.as_deref();

    let mut updates = 0usize;
    let mut failures = Vec::new();

    for editor in &detected {
        if scope == "global" || scope == "all" {
            match mcp_config::write_mcp_config_with_overrides(
                editor,
                &api_key,
                workspace_id_ref,
                // Changing transcript defaults must not pin every global
                // editor session to whichever project happens to be cwd.
                None,
                transcripts_enabled,
                hook_transcripts_enabled,
            ) {
                Ok(()) => {
                    updates += 1;
                    eprintln!(
                        "{} Transcript defaults updated for {}",
                        CHECK,
                        editor.display_name()
                    );
                }
                Err(e) => {
                    warn!(
                        "Could not update transcript defaults for {}: {}",
                        editor.display_name(),
                        e
                    );
                    failures.push(format!(
                        "{} global transcript defaults: {e}",
                        editor.display_name()
                    ));
                }
            }
        }

        if include_project && editor.supports_project_mcp_config() {
            if let Some(project_path) = cwd.as_ref() {
                match mcp_config::write_project_mcp_config_with_overrides(
                    editor,
                    project_path,
                    &api_key,
                    workspace_id_ref,
                    project_id_ref,
                    transcripts_enabled,
                    hook_transcripts_enabled,
                ) {
                    Ok(()) => {
                        updates += 1;
                        eprintln!(
                            "{} Project transcript defaults updated for {}",
                            CHECK,
                            editor.display_name()
                        );
                    }
                    Err(e) => {
                        warn!(
                            "Could not update project transcript defaults for {}: {}",
                            editor.display_name(),
                            e
                        );
                        failures.push(format!(
                            "{} project transcript defaults: {e}",
                            editor.display_name()
                        ));
                    }
                }
            }
        }
    }

    if updates == 0 {
        eprintln!("No MCP config targets were updated for scope '{}'.", scope);
    }

    if failures.is_empty() {
        Ok(())
    } else {
        anyhow::bail!(
            "{} transcript-default update operation(s) failed: {}",
            failures.len(),
            failures.join("; ")
        )
    }
}

/// Migrate detected editor MCP configs to hosted remote transport.
///
/// The local thin-client helper is installed separately for local indexing and
/// ingest flows. Regular update flows call this so hosted remote remains the
/// normal MCP transport after binary updates.
pub async fn migrate_remote(scope: &str) -> Result<()> {
    migrate_remote_scoped(scope, None, false).await
}

/// Hosted-remote migration, scoped to the editors the user chose.
pub async fn migrate_remote_scoped(
    scope: &str,
    only: Option<&[editors::Editor]>,
    only_configured: bool,
) -> Result<()> {
    validate_editor_scope(scope, "migrate-remote")?;
    let (detected, provenance) = resolve_hook_refresh_editors(only, only_configured)?;
    if detected.is_empty() {
        eprintln!("{}", no_target_editors_message(only_configured));
        return Ok(());
    }

    let include_project = scope == "project" || scope == "all";
    let cwd = if include_project {
        Some(std::env::current_dir().context(
            "Could not resolve the project directory; hosted migration changed no configs",
        )?)
    } else {
        None
    };
    let local_config = match cwd.as_ref() {
        Some(project_path) => read_project_config(project_path)?,
        None => None,
    };
    let workspace_id = local_config
        .as_ref()
        .and_then(|cfg| cfg.workspace_id.clone());
    let project_id = local_config.as_ref().and_then(|cfg| cfg.project_id.clone());

    let workspace_id_ref = workspace_id.as_deref();
    let project_id_ref = project_id.as_deref();
    eprintln!(
        "{} Migrating {} editor(s) ({}): {}",
        info_label(),
        detected.len(),
        provenance,
        selected_editors_summary(&detected)
    );

    let api_key = match get_api_key_result()? {
        Some(key) => key,
        None => {
            eprintln!("No API key found. Run 'contextstream-mcp setup' first.");
            return Ok(());
        }
    };
    let mut migrated = 0usize;
    let mut failures = Vec::new();

    for editor in &detected {
        if scope == "global" || scope == "all" {
            // Keep global editor configs workspace-scoped. Project scoping belongs
            // in project-level MCP configs, otherwise a single migration run from
            // one repo pins every global config to that project.
            match mcp_config::migrate_mcp_config(
                editor,
                &api_key,
                workspace_id_ref,
                None,
                None,
                None,
            ) {
                Ok(()) => {
                    migrated += 1;
                    eprintln!(
                        "{} Hosted remote MCP configured for {}",
                        CHECK,
                        editor.display_name()
                    );
                }
                Err(e) => {
                    warn!(
                        "Could not migrate MCP config for {} to hosted remote: {}",
                        editor.display_name(),
                        e
                    );
                    failures.push(format!("{} global migration: {e}", editor.display_name()));
                }
            }
        }

        if include_project && editor.supports_project_mcp_config() {
            if let Some(project_path) = cwd.as_ref() {
                match mcp_config::migrate_project_mcp_config(
                    editor,
                    project_path,
                    &api_key,
                    workspace_id_ref,
                    project_id_ref,
                    None,
                    None,
                ) {
                    Ok(()) => {
                        migrated += 1;
                        eprintln!(
                            "{} Project MCP migrated to hosted remote for {}",
                            CHECK,
                            editor.display_name()
                        );
                        if matches!(editor, editors::Editor::Codex) {
                            if let Err(error) = mcp_config::ensure_codex_project_trust(project_path)
                            {
                                failures.push(format!("Codex project trust: {error}"));
                            }
                        }
                    }
                    Err(e) => {
                        warn!(
                            "Could not migrate project MCP config for {} to hosted remote: {}",
                            editor.display_name(),
                            e
                        );
                        failures.push(format!("{} project migration: {e}", editor.display_name()));
                    }
                }
            }
        }
    }

    if migrated == 0 {
        eprintln!("No MCP configs were migrated for scope '{}'.", scope);
    }

    if scope == "global" || scope == "all" {
        if let Err(e) = update_hooks_scoped("global", Some(&detected), true).await {
            warn!(
                "Could not refresh hooks after hosted remote migration: {}",
                e
            );
            failures.push(format!("post-migration hook refresh: {e}"));
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        anyhow::bail!(
            "{} hosted migration operation(s) failed: {}",
            failures.len(),
            failures.join("; ")
        )
    }
}

/// Non-interactive rules update for all detected editors.
///
/// Called as: `contextstream-mcp update-rules --scope=all`
pub async fn update_rules(
    scope: &str,
    workspace_id: Option<&str>,
    workspace_name: Option<&str>,
) -> Result<()> {
    update_rules_scoped(scope, workspace_id, workspace_name, None, false).await
}

/// Non-interactive rules update, scoped to the editors the user chose.
///
/// Rules files (`CLAUDE.md`, `.cursorrules`, ...) are injected into every
/// session of the editor that reads them, so creating one for an editor the
/// user never selected is at least as intrusive as installing hooks.
pub async fn update_rules_scoped(
    scope: &str,
    workspace_id: Option<&str>,
    workspace_name: Option<&str>,
    only: Option<&[editors::Editor]>,
    only_configured: bool,
) -> Result<()> {
    validate_editor_scope(scope, "update-rules")?;
    let cwd = std::env::current_dir()?;
    let project_name = cwd
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");
    let include_global = scope == "global" || scope == "all";
    let include_project = scope == "project" || scope == "all";

    // Rules are injected into every session of an editor. Existing file paths
    // are not proof of current selection or ContextStream ownership, so never
    // broaden the resolved target set merely because CLAUDE.md/.cursorrules
    // happens to exist.
    let (targets, _) = resolve_hook_refresh_editors(only, only_configured)?;
    let editors_to_update = targets;

    if editors_to_update.is_empty() {
        eprintln!("{}", no_target_editors_message(only_configured));
        return Ok(());
    }

    // Workspace identity precedence:
    //   1. Explicit CLI arg (--workspace-id / --workspace-name)
    //   2. Local `.contextstream/config.json`
    //   3. Resolved from the API (so the rule header shows a real UUID + name
    //      instead of the null UUID when neither 1 nor 2 is available)
    //   4. Inferred from existing rule-file headers (offline fallback)
    let (ws_id, ws_name): (Option<String>, Option<String>) = if workspace_id.is_some() {
        (
            workspace_id.map(String::from),
            workspace_name.map(String::from),
        )
    } else if let Some(config) = read_project_config(&cwd)? {
        (config.workspace_id, config.workspace_name)
    } else {
        // Try API-driven resolution first, then fall back to header-sniffing.
        let api_resolved = match get_api_key_result()? {
            Some(key) => {
                let client = ContextStreamClient::new(Config {
                    api_key: Some(key),
                    ..Default::default()
                });
                resolve_workspace_from_api(&client, &cwd).await
            }
            None => None,
        };
        match api_resolved {
            Some(ws) => (Some(ws.id), Some(ws.name)),
            None => rules::infer_workspace_identity_from_existing_rules(
                &editors_to_update,
                Some(&cwd),
                include_global,
                include_project,
            ),
        }
    };

    let ws_id_ref = ws_id.as_deref();
    let ws_name_ref = ws_name.as_deref();

    let mut failures = Vec::new();
    for editor in &editors_to_update {
        let mut updated = Vec::new();

        // Update global rules
        if scope == "global" || scope == "all" {
            match rules::write_editor_rules(editor, ws_id_ref, ws_name_ref) {
                Ok(()) => updated.push("global rules"),
                Err(e) => {
                    if !e.to_string().contains("Could not determine rules path") {
                        warn!("global rules for {}: {}", editor.display_name(), e);
                        failures.push(format!("{} global rules: {e}", editor.display_name()));
                    }
                }
            }
        }

        // Update project rules
        if scope == "project" || scope == "all" {
            match rules::write_project_rules(
                editor,
                &cwd,
                ws_id_ref,
                ws_name_ref,
                Some(project_name),
            ) {
                Ok(()) => updated.push("project rules"),
                Err(e) => {
                    warn!("project rules for {}: {}", editor.display_name(), e);
                    failures.push(format!("{} project rules: {e}", editor.display_name()));
                }
            }
        }

        if !updated.is_empty() {
            eprintln!(
                "{} {} ({})",
                CHECK,
                editor.display_name(),
                updated.join(", ")
            );
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        anyhow::bail!(
            "{} rules refresh operation(s) failed: {}",
            failures.len(),
            failures.join("; ")
        )
    }
}

pub fn prompt_setup_transport_preference(
    _editors_to_configure: &[editors::Editor],
) -> Result<SetupTransportPreference> {
    if let Some(preselected) = take_preselected_setup_transport_preference() {
        if matches!(preselected, SetupTransportPreference::LocalBinary)
            && !local_mcp_override_allowed()
        {
            ui::say(
                ui::Mark::Warn,
                "Ignoring the local connection override",
                Some(&format!(
                    "local mode needs {}=1 with {}=local and is only for recovery or development",
                    ENV_ALLOW_LOCAL_MCP, ENV_SETUP_TRANSPORT
                )),
            );
        } else {
            // The hosted connection is the default and needs no mention.
            if matches!(preselected, SetupTransportPreference::LocalBinary) {
                ui::say(
                    ui::Mark::Warn,
                    "Using the local binary connection",
                    Some("recovery mode, requested by the installer environment"),
                );
            }
            return Ok(preselected);
        }
    }

    Ok(SetupTransportPreference::HostedRemote)
}

fn selected_editors_summary(editors_to_configure: &[editors::Editor]) -> String {
    if editors_to_configure.is_empty() {
        "None selected".to_string()
    } else {
        editors_to_configure
            .iter()
            .map(|editor| editor.display_name())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

pub fn persist_setup_editor_selection(editors_to_configure: &[editors::Editor]) -> Result<()> {
    if safe_edit::is_dry_run() {
        return Ok(());
    }
    let selected: Vec<String> = editors_to_configure
        .iter()
        .map(|editor| editor.id().to_string())
        .collect();
    mcp_client::activation::replace_configured_clients(&selected)
        .context("Could not save the selected editors; no editor files were changed")
}

fn setup_index_choices() -> [&'static str; 3] {
    [
        "In the background · finish now, search fills in as it builds (recommended)",
        "Now · wait here until the first index finishes",
        "Skip for now · index later from your editor",
    ]
}

fn prompt_setup_index_choice(cwd: &std::path::Path) -> Result<SetupIndexChoice> {
    let choices = setup_index_choices();
    let choice = prompts::select(
        &format!("When should {} be indexed?", wizard::display_path(cwd)),
        &choices,
    )?;

    Ok(match choice {
        0 => SetupIndexChoice::Background,
        1 => SetupIndexChoice::Foreground,
        _ => SetupIndexChoice::Skip,
    })
}

fn print_empty_project_ready() {
    if crate::watch::watch_enabled() {
        ui::say(
            ui::Mark::Ok,
            "Project ready",
            Some("it's empty for now; files are indexed as you add them"),
        );
    } else {
        ui::say(
            ui::Mark::Ok,
            "Project ready",
            Some("add files, then ask your agent to run project(action=\"index\")"),
        );
    }
}

fn setup_path_has_project_content_files(project_path: &Path, include_media: bool) -> bool {
    walkdir::WalkDir::new(project_path)
        .follow_links(false)
        .into_iter()
        .filter_entry(setup_should_scan_entry)
        .filter_map(|entry| entry.ok())
        .any(|entry| {
            entry.file_type().is_file()
                && setup_is_project_content_file(entry.path(), project_path, include_media)
        })
}

/// Whether setup may treat `path` as an explicitly scoped project folder.
///
/// Scope safety is deliberately independent of current contents: a normal
/// empty directory is a valid new project and begins syncing when files are
/// added. HOME and filesystem roots remain forbidden even if they contain
/// source files. This check must run before any project-scoped write.
pub fn setup_path_is_project_candidate(path: &Path) -> bool {
    let Ok(canonical) = path.canonicalize() else {
        return false;
    };
    if !canonical.is_dir() || canonical.parent().is_none() {
        return false;
    }
    let is_home = dirs::home_dir().is_some_and(|home| {
        home.canonicalize()
            .ok()
            .is_some_and(|home| home == canonical)
    });
    !is_home
}

fn canonical_setup_project_path(path: &Path, cwd: &Path) -> Result<PathBuf> {
    let display = path.display().to_string();
    let expanded = if display == "~" {
        dirs::home_dir().ok_or_else(|| {
            anyhow::anyhow!("Could not resolve '~' because this account has no home directory")
        })?
    } else if let Some(relative) = display
        .strip_prefix("~/")
        .or_else(|| display.strip_prefix("~\\"))
    {
        dirs::home_dir()
            .ok_or_else(|| {
                anyhow::anyhow!("Could not resolve '~' because this account has no home directory")
            })?
            .join(relative)
    } else if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let canonical = expanded.canonicalize().with_context(|| {
        format!(
            "Project folder {} does not exist or cannot be read",
            expanded.display()
        )
    })?;
    if !canonical.is_dir() {
        anyhow::bail!(
            "Project path {} is not a directory; choose the checkout root",
            canonical.display()
        );
    }
    if !setup_path_is_project_candidate(&canonical) {
        anyhow::bail!(
            "Project path {} is not a safe project folder. HOME and filesystem roots cannot be \
             linked or indexed by setup; ordinary empty folders are allowed",
            canonical.display()
        );
    }
    Ok(canonical)
}

/// Resolve the only directory that project-scoped setup may mutate or index.
///
/// An explicit path is validated strictly. Without one, a safe current
/// directory is accepted; an unsafe current directory yields a truthful
/// partial setup rather than silently scanning a parent or HOME. Empty normal
/// directories are valid project bootstrap targets.
pub(crate) fn resolve_setup_project_path(
    cwd: &Path,
    explicit: Option<&Path>,
    account_only: bool,
) -> Result<Option<PathBuf>> {
    if account_only {
        if explicit.is_some() {
            anyhow::bail!("--account-only cannot be combined with --project-path");
        }
        return Ok(None);
    }
    if let Some(path) = explicit {
        return canonical_setup_project_path(path, cwd).map(Some);
    }
    if setup_path_is_project_candidate(cwd) {
        return cwd
            .canonicalize()
            .context("Could not resolve the current project directory")
            .map(Some);
    }
    Ok(None)
}

fn setup_should_scan_entry(entry: &walkdir::DirEntry) -> bool {
    if entry.depth() == 0 || !entry.file_type().is_dir() {
        return true;
    }

    let name = entry.file_name().to_string_lossy();
    !matches!(
        name.as_ref(),
        ".git"
            | ".contextstream"
            | ".claude"
            | ".cursor"
            | ".windsurf"
            | ".github"
            | ".vscode"
            | ".idea"
            | ".clinerules"
            | ".roo"
            | ".kilo"
            | ".kilocode"
            | ".aider"
            | ".antigravity"
            | "node_modules"
            | "target"
            | "dist"
            | "build"
            | "vendor"
            | "__pycache__"
    )
}

fn setup_is_project_content_file(
    file_path: &Path,
    project_path: &Path,
    include_media: bool,
) -> bool {
    let relative_path = file_path
        .strip_prefix(project_path)
        .unwrap_or(file_path)
        .to_string_lossy()
        .replace('\\', "/");
    let file_name = file_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();

    if file_name.starts_with('.') || relative_path.starts_with('.') {
        return false;
    }

    if matches!(
        relative_path.as_str(),
        "AGENTS.md"
            | "CLAUDE.md"
            | "GEMINI.md"
            | "WARP.md"
            | "opencode.json"
            | "opencode.yaml"
            | "opencode.yml"
            | "contextstream.json"
    ) {
        return false;
    }

    if !include_media && setup_is_media_file(file_name) {
        return false;
    }

    true
}

fn setup_is_media_file(file_name: &str) -> bool {
    let Some(extension) = Path::new(file_name)
        .extension()
        .and_then(|extension| extension.to_str())
    else {
        return false;
    };

    matches!(
        extension.to_ascii_lowercase().as_str(),
        "png"
            | "jpg"
            | "jpeg"
            | "gif"
            | "webp"
            | "avif"
            | "svg"
            | "mp3"
            | "wav"
            | "flac"
            | "m4a"
            | "aac"
            | "ogg"
            | "mp4"
            | "mov"
            | "webm"
            | "mkv"
            | "avi"
            | "pdf"
    )
}

/// Run the interactive setup wizard.
pub async fn run_setup_wizard() -> Result<()> {
    run_setup_wizard_with_mode(false).await
}

/// Entry point for `setup [--yes]`.
pub async fn run_setup_wizard_with_mode(non_interactive: bool) -> Result<()> {
    run_setup_wizard_with_editors(non_interactive, None).await
}

/// Setup wizard, optionally pinned to an explicit editor list.
pub async fn run_setup_wizard_with_editors(
    non_interactive: bool,
    only: Option<&[editors::Editor]>,
) -> Result<()> {
    run_setup_wizard_with_options(non_interactive, only, None, false, None).await
}

/// Setup wizard with explicit project/account-only scope. `workspace_id`
/// only applies to the non-interactive path, where nothing can prompt.
pub async fn run_setup_wizard_with_options(
    non_interactive: bool,
    only: Option<&[editors::Editor]>,
    project_path: Option<&Path>,
    account_only: bool,
    workspace_id: Option<&str>,
) -> Result<()> {
    if non_interactive {
        run_setup_noninteractive(only, project_path, account_only, workspace_id).await
    } else {
        wizard::run(only, project_path, account_only).await
    }
}

/// Fully non-interactive setup (`setup --yes`): saved credentials, detected
/// editors, hosted remote transport, background indexing, no media. Fails
/// fast with guidance when credentials are missing/invalid or the workspace
/// is ambiguous — it never guesses across workspaces.
async fn run_setup_noninteractive(
    only: Option<&[editors::Editor]>,
    explicit_project_path: Option<&Path>,
    account_only: bool,
    explicit_workspace_id: Option<&str>,
) -> Result<()> {
    print_welcome_banner();
    print_data_collection_disclosure(true);
    ui::say(
        ui::Mark::Info,
        "Unattended setup",
        Some("saved sign-in · detected editors · background index"),
    );

    let api_key = get_api_key_result()?.ok_or_else(|| {
        anyhow::anyhow!(
            "--yes requires an API key. Set CONTEXTSTREAM_API_KEY, save one with \
             `contextstream-mcp configure --api-key-stdin`, or run `contextstream-mcp setup` \
             interactively once."
        )
    })?;

    let config = Config {
        api_key: Some(api_key.clone()),
        ..Default::default()
    };
    let client = ContextStreamClient::new(config);
    let user = tokio::time::timeout(std::time::Duration::from_secs(10), client.me())
        .await
        .map_err(|_| {
            anyhow::anyhow!("Credential validation timed out; check connectivity and re-run.")
        })?
        .map_err(|e| {
            anyhow::anyhow!(
                "Saved credentials are invalid ({}); run `contextstream-mcp setup` interactively.",
                e
            )
        })?;
    ui::say(ui::Mark::Ok, "Signed in", Some(&user.email));

    // An explicit --editors list wins; otherwise fall back to detection, which
    // is the documented behaviour of `setup --yes`.
    let editors_to_configure = match only {
        Some(list) => list.to_vec(),
        None => editors::detect_installed_editors(),
    };
    let cwd = std::env::current_dir()?;
    let project_path = resolve_setup_project_path(&cwd, explicit_project_path, account_only)?;
    persist_setup_editor_selection(&editors_to_configure)?;

    if editors_to_configure.is_empty() {
        ui::say(
            ui::Mark::Warn,
            "No editors selected or detected",
            Some("nothing was configured"),
        );
        let report = doctor::build_report(None, &editors_to_configure).await;
        doctor::print_setup_health_report(&report);
        let outcome = setup_completion_evidence(
            0,
            0,
            project_path.is_some(),
            false,
            false,
            false,
            false,
            !report.has_setup_failures(),
            account_only,
            safe_edit::is_dry_run(),
        );
        print_setup_outcome(
            &user.email,
            false,
            &editors_to_configure,
            None,
            project_path.as_deref(),
            &outcome,
        );
        return Ok(());
    }
    ui::say(
        ui::Mark::Ok,
        "Editors",
        Some(&format!(
            "{} · {}",
            selected_editors_summary(&editors_to_configure),
            if only.is_some() {
                "requested"
            } else {
                "detected"
            }
        )),
    );

    let transport_preference = prompt_setup_transport_preference(&editors_to_configure)?;
    // Record the selected transport before any per-editor writes. If one
    // editor fails partway through setup, later `--only-configured` repair
    // runs must not silently fall back to a different transport.
    write_setup_transport_marker(transport_preference)?;
    let workspace_lookup_path = project_path.as_deref().unwrap_or(cwd.as_path());
    let workspace =
        resolve_workspace_noninteractive(&client, workspace_lookup_path, explicit_workspace_id)
            .await?;
    let selected_project = if let Some(project_path) = project_path.as_deref() {
        select_project_for_current_directory(
            &client,
            project_path,
            workspace.as_ref(),
            false,
            true,
            true,
        )
        .await?
    } else {
        if !account_only {
            ui::say(
                ui::Mark::Warn,
                "No safe project folder",
                Some("editors are set up; link a project later with --project-path"),
            );
        }
        None
    };
    let configured_project_path = selected_project.as_ref().and(project_path.as_deref());

    let preauth_remote_configs =
        matches!(transport_preference, SetupTransportPreference::HostedRemote)
            && editors_to_configure
                .iter()
                .any(mcp_config::editor_supports_remote_mcp);

    for editor in &editors_to_configure {
        configure_editor_with_workspace(
            &client,
            editor,
            &api_key,
            workspace.as_ref(),
            selected_project.as_ref().map(|p| p.id.as_str()),
            configured_project_path,
            transport_preference,
            preauth_remote_configs,
        )
        .await?;
    }
    let binding_established = if let Some(project_path) = configured_project_path {
        if let Some(ref workspace) = workspace {
            establish_validated_setup_binding(
                &client,
                project_path,
                workspace,
                selected_project.as_ref(),
            )
            .await?
        } else {
            false
        }
    } else {
        false
    };

    if binding_established {
        if let Some(repo_root) = configured_project_path.and_then(git_hooks::resolve_repo_root) {
            let root_str = repo_root.to_string_lossy().to_string();
            if !crate::hook_handlers::git_common::capture_disabled(&root_str) {
                if let Err(e) = git_hooks::install_git_hooks(&repo_root) {
                    warning(&format!("Could not install git hooks: {}", e));
                }
            }
        }
    }

    let (index_started, awaiting_first_files) =
        if let Some(project_path) = configured_project_path.filter(|_| binding_established) {
            if !setup_path_has_project_content_files(project_path, false) {
                spawn_warmup(&client, workspace.as_ref().map(|w| &w.id));
                print_empty_project_ready();
                (false, true)
            } else {
                let bg_project_id = selected_project
                    .as_ref()
                    .and_then(|project| uuid::Uuid::parse_str(&project.id).ok());
                spawn_background_index(
                    client.clone(),
                    project_path.to_path_buf(),
                    workspace.as_ref().map(|w| w.id.clone()),
                    bg_project_id,
                    false,
                );
                (true, false)
            }
        } else {
            spawn_warmup(&client, workspace.as_ref().map(|w| &w.id));
            (false, false)
        };

    let report = doctor::build_report(configured_project_path, &editors_to_configure).await;
    doctor::print_setup_health_report(&report);
    let outcome = setup_completion_evidence(
        editors_to_configure.len(),
        editors_to_configure
            .iter()
            .filter(|editor| editor.has_mcp_transport())
            .count(),
        project_path.is_some(),
        selected_project.is_some(),
        binding_established,
        index_started,
        awaiting_first_files,
        !report.has_setup_failures(),
        account_only,
        safe_edit::is_dry_run(),
    );
    print_setup_outcome(
        &user.email,
        false,
        &editors_to_configure,
        workspace.as_ref(),
        project_path.as_deref(),
        &outcome,
    );
    Ok(())
}

/// Resolve the workspace for `--yes` without prompting: an explicit
/// `--workspace-id` wins, then the folder's existing link, then the
/// `CONTEXTSTREAM_WORKSPACE_ID` default, then a single account workspace, then
/// auto-create the default. The environment default ranks below the folder
/// link so an ambient variable can never silently relink a folder. Multiple
/// workspaces with none of those is ambiguous — fail with the choices rather
/// than silently picking one.
async fn resolve_workspace_noninteractive(
    client: &ContextStreamClient,
    cwd: &std::path::Path,
    explicit_workspace_id: Option<&str>,
) -> Result<Option<WorkspaceInfo>> {
    if let Some(id) = explicit_workspace_id {
        return resolve_requested_workspace(client, id, "--workspace-id")
            .await
            .map(Some);
    }

    let previous = read_project_config(cwd)?;
    let workspaces = client.list_workspaces(None, None).await?;

    if let Some(prev_id) = previous.and_then(|cfg| cfg.workspace_id) {
        if let Some(ws) = workspaces.iter().find(|w| w.id.to_string() == prev_id) {
            return Ok(Some(WorkspaceInfo {
                id: ws.id.to_string(),
                name: ws.name.clone(),
            }));
        }
    }

    if let Some(id) = std::env::var(WORKSPACE_ID_ENV)
        .ok()
        .filter(|id| !id.trim().is_empty())
    {
        return resolve_requested_workspace(client, &id, WORKSPACE_ID_ENV)
            .await
            .map(Some);
    }

    match workspaces.len() {
        0 => {
            if safe_edit::is_dry_run() {
                anyhow::bail!(
                    "Dry-run refused to create the missing server-side workspace. \
                     Create or select a workspace first, then re-run the preview."
                );
            }
            let workspace = client.create_workspace("My Workspace", None).await?;
            println!(
                "{} Created workspace {}",
                CHECK,
                style(&workspace.name).cyan()
            );
            Ok(Some(WorkspaceInfo {
                id: workspace.id.to_string(),
                name: workspace.name,
            }))
        }
        1 => Ok(Some(WorkspaceInfo {
            id: workspaces[0].id.to_string(),
            name: workspaces[0].name.clone(),
        })),
        _ => Err(anyhow::anyhow!(ambiguous_workspace_message(
            workspaces
                .iter()
                .map(|w| (w.id.to_string(), w.name.as_str()))
        ))),
    }
}

const WORKSPACE_ID_ENV: &str = "CONTEXTSTREAM_WORKSPACE_ID";

/// Validate a caller-named workspace against the account instead of trusting
/// it: a typo or a workspace the key cannot reach must fail before any editor
/// config embeds it.
async fn resolve_requested_workspace(
    client: &ContextStreamClient,
    id: &str,
    source: &str,
) -> Result<WorkspaceInfo> {
    let id = id.trim();
    let workspace_id = uuid::Uuid::parse_str(id)
        .map_err(|_| anyhow::anyhow!("{source} must be a workspace UUID, got {id:?}."))?;
    let workspace = client.get_workspace(workspace_id).await.map_err(|e| {
        anyhow::anyhow!(
            "Workspace {workspace_id} (from {source}) is not available to this account: {e}"
        )
    })?;
    println!(
        "{} Using workspace {} (from {})",
        CHECK,
        style(&workspace.name).cyan(),
        source
    );
    Ok(WorkspaceInfo {
        id: workspace.id.to_string(),
        name: workspace.name,
    })
}

fn ambiguous_workspace_message<'a>(
    workspaces: impl ExactSizeIterator<Item = (String, &'a str)>,
) -> String {
    let mut message = format!(
        "--yes needs an unambiguous workspace, but your account has {}:\n",
        workspaces.len()
    );
    for (id, name) in workspaces {
        message.push_str(&format!("  {id}  {name}\n"));
    }
    message.push_str(&format!(
        "Re-run with --workspace-id <UUID> (or set {WORKSPACE_ID_ENV}), or run \
         `contextstream-mcp setup` interactively once from this folder to link it; \
         later --yes runs will reuse that link."
    ));
    message
}

fn print_welcome_banner() {
    let ui = ui::ui();
    let title = ui.paint("CONTEXTSTREAM SETUP", Some(ui::Tok::AccentInk), true);
    let version = format!("v{}", mcp_types::config::VERSION);
    let inner = ui.card_width().saturating_sub(ui::GUTTER.len() + 6);
    let gap = inner
        .saturating_sub(console::measure_text_width(&title) + version.len())
        .max(2);
    let rows = vec![
        format!("{title}{}{}", " ".repeat(gap), ui.faint(&version)),
        String::new(),
        ui.strong("Give your agents memory of this project."),
        ui.muted("Sign in, confirm one review screen, and you're done."),
    ];
    println!();
    println!("{}", ui.card(&rows));
}

fn disclosure_env_bool(name: &str, default: bool) -> bool {
    std::env::var(name)
        .ok()
        .and_then(|value| match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" | "enabled" => Some(true),
            "0" | "false" | "no" | "off" | "disabled" => Some(false),
            _ => None,
        })
        .unwrap_or(default)
}

fn data_collection_points(non_interactive: bool) -> [String; 5] {
    let transcripts = disclosure_env_bool("CONTEXTSTREAM_TRANSCRIPTS_ENABLED", true);
    let hook_transcripts = disclosure_env_bool("CONTEXTSTREAM_HOOK_TRANSCRIPTS_ENABLED", true);
    let git_capture = crate::config::git_capture_default_enabled();
    let index_behavior = if non_interactive {
        "starts in the background after a validated project binding"
    } else {
        "starts only after you save the review screen"
    };

    [
        format!("Transcript exchange saving default: {transcripts}. Change with `contextstream-mcp configure --transcripts on|off`."),
        format!("Hook transcript saving default: {hook_transcripts}. Change with `contextstream-mcp configure --hook-transcripts on|off`."),
        format!("Project indexing {index_behavior}; matched source files are sent to your ContextStream workspace. Exclude files with `.contextstream/ignore`; de-index with `project(action=\"purge\")`."),
        "Editor lifecycle hooks are installed for selected supported editors. Disable with `CONTEXTSTREAM_HOOK_ENABLED=false` or remove them with the setup/uninstall flow.".to_string(),
        format!("Local Git capture default: {git_capture}. Managed hooks send event type, commit SHA/time, branch/ref names, aggregate line/file counts, a 256-character redacted commit subject, an opaque checkout ID, and a credential-free canonical remote. Absolute paths, commit bodies, and author name/email are not sent. Disable with `CONTEXTSTREAM_GIT_CAPTURE=off` or `.contextstream/config.json` `git_capture.enabled=false`."),
    ]
}

#[cfg(test)]
fn data_collection_disclosure(non_interactive: bool) -> String {
    let mut disclosure = "Data handling before setup changes anything:".to_string();
    for point in data_collection_points(non_interactive) {
        disclosure.push_str("\n  • ");
        disclosure.push_str(&point);
    }
    disclosure
}

/// The data-handling notice, shown before setup changes anything. The
/// facts are complete; they are set as muted fine print so they inform
/// without crowding the flow.
fn print_data_collection_disclosure(non_interactive: bool) {
    let ui = ui::ui();
    let width = ui.card_width().saturating_sub(ui::GUTTER.len() + 2);
    println!();
    println!("{}{}", ui::GUTTER, ui.kicker("Data and privacy"));
    for point in data_collection_points(non_interactive) {
        for (index, line) in ui::wrap(&point, width).into_iter().enumerate() {
            let bullet = if index == 0 {
                ui.faint("·")
            } else {
                " ".to_string()
            };
            println!("{}{bullet} {}", ui::GUTTER, ui.muted(&line));
        }
    }
}

fn setup_teaching_contracts(editors: &[editors::Editor]) -> Vec<HarnessTeachingContract> {
    editors
        .iter()
        .map(|editor| {
            build_harness_teaching(
                Some(editor.harness_id()),
                HarnessTeachingDelivery::HelpWorkflow,
            )
        })
        .collect()
}

fn print_setup_outcome(
    email: &str,
    team_capable: bool,
    editors: &[editors::Editor],
    workspace: Option<&WorkspaceInfo>,
    project_path: Option<&Path>,
    outcome: &SetupCompletionEvidence,
) {
    let ui = ui::ui();
    let teaching_contracts = setup_teaching_contracts(editors);
    debug_assert!(teaching_contracts
        .iter()
        .all(|contract| contract.teaching_version == HARNESS_TEACHING_VERSION));
    let editor_ids = editors
        .iter()
        .map(editors::Editor::id)
        .collect::<Vec<_>>()
        .join(",");
    let resume_project = project_path
        .map(|path| format!("{path:?}"))
        .unwrap_or_else(|| "/path/to/project".to_string());

    let (badge, title) = match outcome.state {
        SetupCompletionState::RestartRequired => ("ready", "ContextStream is set up"),
        SetupCompletionState::DryRunPreview => {
            ("preview", "Preview finished · nothing was changed")
        }
        SetupCompletionState::NoClientConfigured => ("paused", "Choose an editor to finish setup"),
        SetupCompletionState::RulesOnlyReady => {
            ("rules", "Rules updated · these editors can't run MCP")
        }
        SetupCompletionState::RepairRequired => ("repair", "Some editor settings need a repair"),
        SetupCompletionState::AccountOnly => ("saved", "Account and editors are set up"),
        SetupCompletionState::ProjectRequired => {
            ("almost", "Editors are set up · link a project to finish")
        }
        SetupCompletionState::IndexRequired => {
            ("almost", "Project linked · indexing hasn't started")
        }
    };

    let mut account = email.to_string();
    if team_capable {
        account.push_str(&ui.faint(" · team"));
    }
    let mut rows = vec![
        format!("{}  {}", ui.badge(badge), ui.strong(title)),
        String::new(),
        ui.row("Account", &account),
    ];
    if !editors.is_empty() {
        rows.push(ui.row("Editors", &selected_editors_summary(editors)));
    }
    if let Some(workspace) = workspace {
        rows.push(ui.row("Workspace", &workspace.name));
    }
    if outcome.binding_established {
        let folder = project_path
            .map(wizard::display_path)
            .unwrap_or_else(|| "linked checkout".to_string());
        let status = if outcome.index_started {
            "indexing"
        } else if outcome.awaiting_first_files {
            "empty · indexed as files appear"
        } else {
            "not indexed yet"
        };
        rows.push(ui.row(
            "Project",
            &format!("{} {}", ui.path(&folder), ui.faint(&format!("· {status}"))),
        ));
    } else if !outcome.account_only_requested {
        rows.push(ui.row("Project", &ui.muted("Not linked")));
    }
    if !outcome.doctor_healthy {
        rows.push(ui.row("Checks", &ui.error("some checks failed · see below")));
    }
    println!();
    println!("{}", ui.card(&rows));
    println!();

    let step = |number: usize, text: &str| {
        println!(
            "{}{} {}",
            ui::GUTTER,
            ui.paint(&format!("{number:02}"), Some(ui::Tok::Accent), true),
            text
        );
    };
    let detail = |text: &str| println!("{}   {}", ui::GUTTER, text);
    // Long guidance wraps under its step instead of running off the card.
    let wrap_width = ui.card_width().saturating_sub(ui::GUTTER.len() + 3);
    let wrapped = |text: &str, style: &dyn Fn(&str) -> String| {
        for line in ui::wrap(text, wrap_width) {
            detail(&style(&line));
        }
    };
    let editor_detail = |editor: &editors::Editor| {
        let name = editor.display_name();
        let text = format!("{name} {}", editor.activation_reload_instruction());
        for (index, line) in ui::wrap(&text, wrap_width).into_iter().enumerate() {
            match line.strip_prefix(name).filter(|_| index == 0) {
                Some(rest) => detail(&format!("{}{}", ui.strong(name), ui.muted(rest))),
                None => detail(&ui.muted(&line)),
            }
        }
    };
    println!("{}{}", ui::GUTTER, ui.kicker("Next"));

    match outcome.state {
        SetupCompletionState::DryRunPreview => {
            step(
                1,
                &format!(
                    "Run the same command without {} to apply it.",
                    ui.path("--dry-run")
                ),
            );
        }
        SetupCompletionState::NoClientConfigured => {
            step(1, "Pick an editor and the project folder to connect:");
            detail(&ui.path(
                "contextstream-mcp setup --editors <editor-id> --project-path /path/to/project",
            ));
        }
        SetupCompletionState::RulesOnlyReady => {
            for editor in editors {
                editor_detail(editor);
            }
            step(
                1,
                "Add an editor that supports MCP to use ContextStream tools:",
            );
            detail(&ui.path(
                "contextstream-mcp setup --editors <editor-id> --project-path /path/to/project",
            ));
        }
        SetupCompletionState::RepairRequired => {
            let scope = if outcome.binding_established {
                "all"
            } else {
                "global"
            };
            step(1, "Repair the editor settings setup manages:");
            detail(&ui.path(&format!(
                "contextstream-mcp doctor --repair --scope {scope} --editors {editor_ids}"
            )));
            step(2, "Then confirm everything passes:");
            detail(&ui.path(&format!(
                "contextstream-mcp doctor --scope {scope} --editors {editor_ids}"
            )));
        }
        SetupCompletionState::AccountOnly => {
            step(1, "When you're ready, link a project:");
            detail(&ui.path(&format!(
                "contextstream-mcp setup --project-path /path/to/project --editors {editor_ids}"
            )));
        }
        SetupCompletionState::ProjectRequired => {
            step(1, "Link the checkout ContextStream should learn:");
            detail(&ui.path(&format!(
                "contextstream-mcp setup --project-path {resume_project} --editors {editor_ids}"
            )));
            detail(&ui.faint("Nothing outside that folder is indexed."));
        }
        SetupCompletionState::IndexRequired => {
            step(
                1,
                &format!(
                    "Restart your editor, then ask it to run {}.",
                    ui.path("project(action=\"index\")")
                ),
            );
            detail(&ui.faint(&format!(
                "Or re-run setup --project-path {resume_project} and choose when to index."
            )));
        }
        SetupCompletionState::RestartRequired => {
            let mcp_editors: Vec<_> = editors
                .iter()
                .filter(|editor| editor.has_mcp_transport())
                .collect();
            if let [editor] = mcp_editors.as_slice() {
                step(
                    1,
                    &format!("Restart {} so it connects.", editor.display_name()),
                );
                wrapped(editor.activation_reload_instruction(), &|line| {
                    ui.muted(line)
                });
            } else {
                step(1, "Restart your editors so they connect:");
                for editor in &mcp_editors {
                    editor_detail(editor);
                }
            }
            let mut next = 2;
            if outcome.awaiting_first_files {
                if crate::watch::watch_enabled() {
                    step(next, "Add your first file · it's indexed automatically.");
                } else {
                    step(
                        next,
                        &format!(
                            "Add your first file, then ask your agent to run {}.",
                            ui.path("project(action=\"index\")")
                        ),
                    );
                }
                next += 1;
            }
            step(next, "Then ask your agent:");
            wrapped(&format!("\"{}\"", first_value_prompt()), &|line| {
                ui.accent(line)
            });
            println!();
            println!(
                "{}{}",
                ui::GUTTER,
                ui.faint(&format!(
                    "Check the connection anytime · contextstream-mcp doctor --scope all --editors {editor_ids}"
                ))
            );
            if team_capable {
                println!();
                team_guidance::print_team_success_next_steps();
            }
        }
    }
    println!();
}

/// Authenticate via browser login, API key paste, or inline signup, offering
/// to keep working saved credentials first (used by `configure`).
pub async fn authenticate() -> Result<(String, String)> {
    if let Ok(creds) = read_saved_credentials() {
        if let Some(api_key) = creds.api_key {
            ui::say(
                ui::Mark::Info,
                "Saved sign-in found",
                Some(&mask_api_key(&api_key)),
            );
            if prompts::confirm("Keep using it?", true)? {
                let client = ContextStreamClient::new(Config {
                    api_key: Some(api_key.clone()),
                    ..Default::default()
                });
                let check = ui::spin(
                    "Checking your saved sign-in",
                    tokio::time::timeout(std::time::Duration::from_secs(10), client.me()),
                )
                .await;
                match check {
                    Ok(Ok(user)) => {
                        ui::say(ui::Mark::Ok, "Signed in", Some(&user.email));
                        return Ok((api_key, user.email));
                    }
                    Ok(Err(e)) => ui::say(
                        ui::Mark::Warn,
                        "Your saved sign-in no longer works",
                        Some(&e.to_string()),
                    ),
                    Err(_) => ui::say(
                        ui::Mark::Warn,
                        "Couldn't check your saved sign-in",
                        Some("continuing with a new sign-in"),
                    ),
                }
            }
        }
    }

    authenticate_fresh(inline_signup_available().await).await
}

/// Whether the server offers inline email + SMS signup (bounded wait).
async fn inline_signup_available() -> bool {
    tokio::time::timeout(
        std::time::Duration::from_secs(6),
        mcp_client::auth::fetch_signup_config(),
    )
    .await
    .ok()
    .and_then(|result| result.ok())
    .map(|config| config.inline_signup_available)
    .unwrap_or(false)
}

/// Sign in without reusing saved credentials.
async fn authenticate_fresh(inline_signup_available: bool) -> Result<(String, String)> {
    let mut choices = vec!["Continue in your browser (recommended)", "Paste an API key"];
    if inline_signup_available {
        choices.push("Create an account with email and phone");
    }

    match prompts::select("How do you want to sign in?", &choices)? {
        0 => authenticate_browser().await,
        1 => authenticate_api_key().await,
        _ => authenticate_email_signup().await,
    }
}

fn signup_error_parts(error: &anyhow::Error) -> (String, String) {
    match error.downcast_ref::<mcp_client::auth::SignupApiError>() {
        Some(api) => (api.code.clone(), api.message.clone()),
        None => ("error".to_string(), error.to_string()),
    }
}

fn signup_error_is_fatal(code: &str) -> bool {
    matches!(
        code,
        "verification_locked"
            | "signup_attempt_expired"
            | "account_exists"
            | "account_exists_unverified"
            | "NOT_CONFIGURED"
    )
}

/// Create a new account inline: email code, then the phone with the SMS
/// consent notice and an explicit yes, then the SMS code. The server mints
/// the API key; the wizard's caller saves it like any other credential.
async fn authenticate_email_signup() -> Result<(String, String)> {
    use mcp_client::auth as signup;

    println!();
    println!(
        "{}{}",
        ui::GUTTER,
        ui::ui().heading("Create your ContextStream account")
    );
    let email = loop {
        let entered = prompts::input("Email address:", None)?;
        let trimmed = entered.trim().to_lowercase();
        if trimmed.contains('@') && trimmed.len() >= 5 {
            break trimmed;
        }
        println!("{} Enter a valid email address", CROSS);
    };
    let full_name = prompts::input("Your name (optional):", Some(""))?;
    let full_name = {
        let trimmed = full_name.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    };

    let started = signup::start_email_signup(
        &email,
        full_name.as_deref(),
        crate::account_tool::client_metadata(),
    )
    .await
    .map_err(|error| {
        let (code, message) = signup_error_parts(&error);
        if code == "account_exists" || code == "account_exists_unverified" {
            anyhow::anyhow!("{message} Choose \"Continue in your browser\" instead.")
        } else {
            anyhow::anyhow!("Could not start the signup: {message}")
        }
    })?;
    let mut pending = PendingConnection::new(
        started.attempt_id.clone(),
        started.client_secret.clone(),
        started.email.clone(),
    );
    write_pending_connection(&pending)?;
    println!(
        "{} We emailed a 6-digit code to {}.",
        CHECK,
        style(&started.email).cyan()
    );

    loop {
        let entered = prompts::input("Email code (or 'r' to resend):", None)?;
        let entered = entered.trim().to_string();
        if entered.eq_ignore_ascii_case("r") {
            match signup::resend_signup_email(&pending.attempt_id, &pending.client_secret).await {
                Ok(_) => println!("{} A new code was sent.", CHECK),
                Err(error) => println!("{} {}", CROSS, signup_error_parts(&error).1),
            }
            continue;
        }
        match signup::verify_signup_email(&pending.attempt_id, &pending.client_secret, &entered)
            .await
        {
            Ok(_) => break,
            Err(error) => {
                let (code, message) = signup_error_parts(&error);
                println!("{} {}", CROSS, message);
                if signup_error_is_fatal(&code) {
                    let _ = clear_pending_connection();
                    anyhow::bail!(
                        "Signup cancelled. Re-run `contextstream-mcp setup` to try again."
                    );
                }
            }
        }
    }
    pending.stage = PendingStage::EmailVerified;
    write_pending_connection(&pending)?;
    println!("{} Email verified.", CHECK);

    let consent = loop {
        let phone = prompts::input(
            "Mobile number (international format, e.g. +1 555 123 4567):",
            None,
        )?;
        match signup::request_sms_consent(&pending.attempt_id, &pending.client_secret, phone.trim())
            .await
        {
            Ok(consent) => break consent,
            Err(error) => {
                let (code, message) = signup_error_parts(&error);
                println!("{} {}", CROSS, message);
                if signup_error_is_fatal(&code) {
                    let _ = clear_pending_connection();
                    anyhow::bail!(
                        "Signup cancelled. Re-run `contextstream-mcp setup` to try again."
                    );
                }
            }
        }
    };
    pending.stage = PendingStage::ConsentPending;
    pending.phone_last4 = Some(consent.phone_last4.clone());
    pending.consent_token = Some(consent.consent_token.clone());
    write_pending_connection(&pending)?;

    println!();
    println!("{}", consent.consent_text);
    println!();
    if !prompts::confirm(
        &format!(
            "Do you agree to receive a one-time verification text at the number ending in {}?",
            consent.phone_last4
        ),
        false,
    )? {
        let _ = signup::cancel_signup(&pending.attempt_id, &pending.client_secret).await;
        let _ = clear_pending_connection();
        anyhow::bail!("No text was sent. You can choose \"Continue in your browser\" instead.");
    }
    let sent = signup::add_signup_phone(
        &pending.attempt_id,
        &pending.client_secret,
        &consent.consent_token,
        "wizard",
        Some("yes"),
    )
    .await
    .map_err(|error| {
        anyhow::anyhow!(
            "Could not send the verification text: {}",
            signup_error_parts(&error).1
        )
    })?;
    pending.stage = PendingStage::SmsSent;
    write_pending_connection(&pending)?;
    println!(
        "{} Code texted to the number ending in {}.",
        CHECK, sent.phone_last4
    );

    let complete = loop {
        let entered = prompts::input("SMS code (or 'r' to resend):", None)?;
        let entered = entered.trim().to_string();
        if entered.eq_ignore_ascii_case("r") {
            match signup::resend_signup_sms(&pending.attempt_id, &pending.client_secret).await {
                Ok(_) => println!("{} A new code was sent.", CHECK),
                Err(error) => println!("{} {}", CROSS, signup_error_parts(&error).1),
            }
            continue;
        }
        match signup::verify_signup_phone(&pending.attempt_id, &pending.client_secret, &entered)
            .await
        {
            Ok(complete) => break complete,
            Err(error) => {
                let (code, message) = signup_error_parts(&error);
                println!("{} {}", CROSS, message);
                if signup_error_is_fatal(&code) {
                    let _ = clear_pending_connection();
                    anyhow::bail!(
                        "Signup cancelled. Re-run `contextstream-mcp setup` to try again."
                    );
                }
            }
        }
    };
    pending.mark_finalized();
    write_pending_connection(&pending)?;

    // Persist before acknowledging: a crash or failed write must leave redelivery available.
    save_and_ack_signup(&pending, &complete).await?;
    let _ = clear_pending_connection();
    println!(
        "{} Account created for {}. We emailed you a link to set a password for the web dashboard.",
        CHECK,
        style(&complete.email).cyan()
    );
    Ok((complete.api_key.secret, complete.email))
}

/// Authenticate via browser (device flow).
async fn authenticate_browser() -> Result<(String, String)> {
    use mcp_client::auth::{poll_device_login, start_device_login};

    let ui = ui::ui();
    let device_response = ui::spin("Starting browser sign-in", start_device_login()).await?;
    let opened = open::that(&device_response.verification_uri).is_ok();
    let rows = vec![
        ui.kicker("Confirm in your browser"),
        String::new(),
        ui.row(
            "Code",
            &ui.paint(&device_response.user_code, Some(ui::Tok::Accent), true),
        ),
        ui.row("Link", &ui.path(&device_response.verification_uri)),
        String::new(),
        ui.muted(if opened {
            "Your browser opened this page. Check that the code matches, then approve."
        } else {
            "Open the link, check that the code matches, then approve."
        }),
    ];
    println!();
    println!("{}", ui.card(&rows));

    // Poll for completion, but never hang setup forever on an abandoned
    // browser tab — bail with guidance after 5 minutes.
    const DEVICE_LOGIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
    let token = match ui::spin(
        "Waiting for you to approve",
        tokio::time::timeout(
            DEVICE_LOGIN_TIMEOUT,
            poll_device_login(&device_response.device_code, device_response.interval),
        ),
    )
    .await
    {
        Ok(result) => result?,
        Err(_) => {
            return Err(anyhow::anyhow!(
                "Browser sign-in timed out after 5 minutes. Re-run `contextstream-mcp setup` \
                 and approve in the browser, or choose \"Paste an API key\" instead."
            ));
        }
    };

    // Exchange the browser session for a persistent API key.
    let client = ContextStreamClient::new(Config {
        jwt: Some(token.access_token.clone()),
        ..Default::default()
    });
    ui::spin("Finishing sign-in", async {
        let user = client.me().await?;
        let api_key = client.create_api_key("ContextStream CLI").await?;
        Ok::<_, anyhow::Error>((api_key, user.email))
    })
    .await
}

/// Authenticate via pasted API key.
async fn authenticate_api_key() -> Result<(String, String)> {
    ui::say(
        ui::Mark::Info,
        "Create or copy a key",
        Some("https://contextstream.io/settings/api-keys"),
    );

    loop {
        let api_key = prompts::password("Paste your API key")?;
        let api_key = api_key.trim().to_string();

        if api_key.is_empty() {
            ui::say(ui::Mark::Fail, "The key can't be empty", None);
            continue;
        }

        let client = ContextStreamClient::new(Config {
            api_key: Some(api_key.clone()),
            ..Default::default()
        });

        match ui::spin("Checking the key", client.me()).await {
            Ok(user) => {
                return Ok((api_key, user.email));
            }
            Err(e) => {
                ui::say(ui::Mark::Fail, "That key didn't work", Some(&e.to_string()));
                if !prompts::confirm("Try again?", true)? {
                    return Err(anyhow::anyhow!("Authentication cancelled"));
                }
            }
        }
    }
}

/// Configure a single editor with workspace information.
pub async fn configure_editor_with_workspace(
    client: &ContextStreamClient,
    editor: &editors::Editor,
    api_key: &str,
    workspace: Option<&WorkspaceInfo>,
    project_id: Option<&str>,
    project_path: Option<&Path>,
    transport_preference: SetupTransportPreference,
    preauth_remote_configs: bool,
) -> Result<()> {
    let mut mcp_targets = Vec::new();
    let mut rules_targets = Vec::new();
    let mut saved_items = Vec::new();
    let mut issues = Vec::new();

    if project_path.is_none() && project_id.is_some() {
        anyhow::bail!(
            "Internal setup scope mismatch: a project id was supplied while project writes were disabled"
        );
    }
    let folder_project_name = project_path
        .and_then(|path| path.file_name())
        .and_then(|name| name.to_str())
        .unwrap_or("project");
    let selected_project = match project_id.filter(|_| project_path.is_some()) {
        Some(raw_id) => {
            let id = uuid::Uuid::parse_str(raw_id)
                .map_err(|_| anyhow::anyhow!("Invalid selected project ID: {}", raw_id))?;
            Some(client.get_project_fresh(id).await?)
        }
        None => None,
    };
    if let (Some(ws), Some(project)) = (workspace, selected_project.as_ref()) {
        let workspace_id = uuid::Uuid::parse_str(&ws.id)
            .map_err(|_| anyhow::anyhow!("Invalid selected workspace ID: {}", ws.id))?;
        require_project_workspace_ownership(client, project.id, project.workspace_id, workspace_id)
            .await?;
    }

    // Local MCP configs depend on this executable, so a failed copy is a
    // precondition failure and must precede every config write. Hosted configs
    // remain useful without hooks; in that case continue the independent
    // writes, skip hook mutation, and return a precise partial-failure error.
    let helper_ready = if editor_needs_managed_helper(editor) {
        match hooks::ensure_managed_binary_installed_quiet() {
            Ok(_) => true,
            Err(e) if matches!(transport_preference, SetupTransportPreference::LocalBinary) => {
                return Err(e).with_context(|| {
                    format!(
                        "Could not install the managed helper before configuring {}; no editor configs were changed",
                        editor.display_name()
                    )
                });
            }
            Err(e) => {
                warn!(
                    "Could not install the managed sync helper before configuring {}: {}",
                    editor.display_name(),
                    e
                );
                issues.push(format!("hosted sync helper: {}", e));
                false
            }
        }
    } else {
        true
    };

    let project_name = selected_project
        .as_ref()
        .map(|project| project.name.as_str())
        .unwrap_or(folder_project_name);

    let workspace_id = workspace.map(|w| w.id.as_str());
    let workspace_name = workspace.map(|w| w.name.as_str());
    let remote_auth_api_key =
        matches!(transport_preference, SetupTransportPreference::HostedRemote)
            .then_some(api_key)
            .filter(|_| preauth_remote_configs);

    // Generate global MCP config only for integrations that actually expose
    // MCP. Rules-only integrations must not install a helper, register a
    // bridge, or claim that an MCP config was written.
    if editor.has_mcp_transport() {
        let global_config_result = match transport_preference {
            SetupTransportPreference::HostedRemote => {
                mcp_config::write_mcp_config_force_remote_with_auth(
                    editor,
                    api_key,
                    workspace_id,
                    None,
                    None,
                    None,
                    remote_auth_api_key,
                )
            }
            SetupTransportPreference::LocalBinary => mcp_config::write_mcp_config_force_local(
                editor,
                api_key,
                workspace_id,
                None,
                None,
                None,
            ),
        };

        match global_config_result {
            Ok(()) => {
                mcp_targets.push("global");
            }
            Err(e) => {
                issues.push(format!("MCP global config: {}", e));
            }
        }
    }

    // Project trust is project-scoped even though Codex stores it in the
    // global TOML file.
    if let Some(project_path) = project_path.filter(|_| matches!(editor, editors::Editor::Codex)) {
        if let Err(e) = mcp_config::ensure_codex_project_trust(project_path) {
            warn!("Could not add project trust for Codex: {}", e);
            issues.push(format!("Codex project trust: {}", e));
        }
    }

    // Generate project-level MCP config (if supported by this editor)
    let mut wrote_project_mcp = false;
    if let Some(project_path) = project_path.filter(|_| editor.supports_project_mcp_config()) {
        let project_config_result = match transport_preference {
            SetupTransportPreference::HostedRemote => {
                mcp_config::write_project_mcp_config_force_remote_with_auth(
                    editor,
                    project_path,
                    api_key,
                    workspace_id,
                    project_id,
                    None,
                    None,
                    remote_auth_api_key,
                )
            }
            SetupTransportPreference::LocalBinary => {
                mcp_config::write_project_mcp_config_force_local(
                    editor,
                    project_path,
                    api_key,
                    workspace_id,
                    project_id,
                    None,
                    None,
                )
            }
        };

        match project_config_result {
            Ok(()) => {
                mcp_targets.push("project");
                wrote_project_mcp = true;
            }
            Err(e) => {
                warn!(
                    "Could not write project MCP config for {}: {}",
                    editor.display_name(),
                    e
                );
                issues.push(format!("MCP project config: {}", e));
            }
        }
    }
    if matches!(editor, editors::Editor::Copilot) && wrote_project_mcp {
        saved_items.push("Copilot bridge");
    }

    // Generate AI rules (global)
    match rules::write_editor_rules(editor, workspace_id, workspace_name) {
        Ok(()) => {
            rules_targets.push("global");
        }
        Err(e) => {
            // Some editors like Cursor don't have global rules path
            if e.to_string().contains("Could not determine rules path") {
                // Skip silently - this is expected for some editors
            } else {
                warn!(
                    "Could not write global rules for {}: {}",
                    editor.display_name(),
                    e
                );
                issues.push(format!("global rules: {}", e));
            }
        }
    }

    // Generate project-level rules only after the caller authorized cwd as a
    // project. Global setup must not create AGENTS.md/CLAUDE.md in HOME.
    if let Some(project_path) = project_path {
        match rules::write_project_rules(
            editor,
            project_path,
            workspace_id,
            workspace_name,
            Some(project_name),
        ) {
            Ok(()) => {
                rules_targets.push("project");
            }
            Err(e) => {
                warn!(
                    "Could not write project rules for {}: {}",
                    editor.display_name(),
                    e
                );
                issues.push(format!("project rules: {}", e));
            }
        }
    }

    // Write workspace config (.contextstream/config.json)
    if let Some(project_path) = project_path {
        if let Some(ws) = workspace {
            match mcp_config::write_workspace_config(
                project_path,
                &ws.id,
                &ws.name,
                Some(project_name),
                project_id,
            ) {
                Ok(()) => {
                    saved_items.push("workspace link");
                }
                Err(e) => {
                    warn!("Could not write workspace config: {}", e);
                    issues.push(format!("workspace link: {}", e));
                }
            }
        }
    }

    // Install hooks only after the executable they reference is known-good.
    if editor.has_hooks() && helper_ready {
        match hooks::install_hooks(editor, Some(api_key)) {
            Ok(()) => {
                saved_items.push("hooks");
            }
            Err(e) => {
                warn!(
                    "Could not install hooks for {}: {}",
                    editor.display_name(),
                    e
                );
                issues.push(format!("hooks: {}", e));
            }
        }
    }

    // The editor remains connected to hosted MCP. This local process is only
    // the managed sync bridge that gives the hosted service fresh checkout
    // bytes across hookless harnesses, restarts, machines, and worktrees.
    if matches!(transport_preference, SetupTransportPreference::HostedRemote)
        && editor.has_mcp_transport()
        && helper_ready
        && crate::watch::watch_enabled()
    {
        match register_managed_sync_bridge() {
            Ok(_) => {
                if !safe_edit::is_dry_run() {
                    crate::watch::spawn_watch_helper();
                }
                saved_items.push("hosted sync bridge");
            }
            Err(error) => {
                warn!(
                    "Could not register the managed hosted sync bridge for {}: {}",
                    editor.display_name(),
                    error
                );
                issues.push(format!("hosted sync bridge: {error}"));
            }
        }
    }

    let had_issues = !issues.is_empty();
    let mut parts = Vec::new();
    if !mcp_targets.is_empty() {
        parts.push(format!("MCP {}", mcp_targets.join(" + ")));
    }
    if !rules_targets.is_empty() {
        parts.push(format!("rules {}", rules_targets.join(" + ")));
    }
    parts.extend(saved_items.iter().map(|item| match *item {
        "hosted sync bridge" => "sync".to_string(),
        other => other.to_string(),
    }));
    let summary = parts.join(" · ");
    if issues.is_empty() {
        ui::say(ui::Mark::Ok, editor.display_name(), Some(&summary));
    } else {
        let ui = ui::ui();
        ui::say(
            ui::Mark::Warn,
            editor.display_name(),
            Some(&format!(
                "{} problem{}",
                issues.len(),
                if issues.len() == 1 { "" } else { "s" }
            )),
        );
        if !summary.is_empty() {
            println!(
                "{}    {}",
                ui::GUTTER,
                ui.faint(&format!("saved: {summary}"))
            );
        }
        for issue in &issues {
            println!("{}    {} {}", ui::GUTTER, ui.mark(ui::Mark::Fail), issue);
        }
    }

    if had_issues && !safe_edit::is_dry_run() {
        client.spawn_activation_failure(
            workspace_id.and_then(|value| uuid::Uuid::parse_str(value).ok()),
            project_id.and_then(|value| uuid::Uuid::parse_str(value).ok()),
            None,
            "config_write",
            &mcp_types::Error::Config("editor configuration was incomplete".to_string()),
        );
    }

    if had_issues {
        anyhow::bail!(
            "{} could not be configured completely; the detailed failures are listed above",
            editor.display_name()
        );
    }

    Ok(())
}

/// Set up workspace.
pub async fn setup_workspace(client: &ContextStreamClient) -> Result<Option<WorkspaceInfo>> {
    let cwd = std::env::current_dir().ok();
    setup_workspace_for_path(client, cwd.as_deref()).await
}

async fn setup_workspace_for_path(
    client: &ContextStreamClient,
    project_path: Option<&Path>,
) -> Result<Option<WorkspaceInfo>> {
    // Check if we have a previously configured workspace from a local config
    // (handles the case where setup is re-run from the same or nearby directory).
    let previous_config = project_path.and_then(|path| read_project_config(path).ok().flatten());

    // List existing workspaces
    let workspaces = client.list_workspaces(None, None).await?;

    if workspaces.is_empty() {
        // No workspaces - create one
        if prompts::confirm("No workspaces found. Create one?", true)? {
            if safe_edit::is_dry_run() {
                anyhow::bail!(
                    "Dry-run refused to create a server-side workspace. \
                     Create it first, then re-run the preview."
                );
            }
            let name = prompts::input("Workspace name:", Some("My Workspace"))?;
            let description = prompts::optional_input("Description (optional):")?;

            let workspace = client
                .create_workspace(&name, description.as_deref())
                .await?;

            return Ok(Some(WorkspaceInfo {
                id: workspace.id.to_string(),
                name: workspace.name,
            }));
        }
    } else {
        // If there's a previous config, try to auto-detect the workspace
        let previous_ws_idx = previous_config.as_ref().and_then(|cfg| {
            cfg.workspace_id
                .as_ref()
                .and_then(|ws_id| workspaces.iter().position(|w| w.id.to_string() == *ws_id))
        });

        // If we found the previously configured workspace, offer to reuse it
        if let Some(idx) = previous_ws_idx {
            let ws = &workspaces[idx];
            let prev_name = previous_config
                .as_ref()
                .and_then(|c| c.workspace_name.as_deref())
                .unwrap_or(&ws.name);

            if prompts::confirm(
                &format!(
                    "Previously configured workspace detected: '{}'. Use it?",
                    prev_name
                ),
                true,
            )? {
                return Ok(Some(WorkspaceInfo {
                    id: ws.id.to_string(),
                    name: ws.name.clone(),
                }));
            }
        }

        // Select from existing or create new
        let mut choices: Vec<String> = workspaces.iter().map(|w| w.name.clone()).collect();
        choices.push("Create new workspace".to_string());
        choices.push("Skip workspace setup".to_string());

        let choice = prompts::select(
            "Select a workspace:",
            &choices.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
        )?;

        if choice < workspaces.len() {
            let ws = &workspaces[choice];
            return Ok(Some(WorkspaceInfo {
                id: ws.id.to_string(),
                name: ws.name.clone(),
            }));
        } else if choice == workspaces.len() {
            // Create new
            if safe_edit::is_dry_run() {
                anyhow::bail!(
                    "Dry-run refused to create a server-side workspace. \
                     Create it first, then re-run the preview."
                );
            }
            let name = prompts::input("Workspace name:", None)?;
            let description = prompts::optional_input("Description (optional):")?;

            let workspace = client
                .create_workspace(&name, description.as_deref())
                .await?;

            return Ok(Some(WorkspaceInfo {
                id: workspace.id.to_string(),
                name: workspace.name,
            }));
        }
    }

    Ok(None)
}

/// Everything project selection knows about this checkout and workspace.
pub(crate) struct ProjectCandidates {
    pub(crate) ws_id: Option<uuid::Uuid>,
    pub(crate) folder_project_name: String,
    pub(crate) existing_projects: Vec<mcp_types::api::Project>,
    pub(crate) checkout_repository_identity: Option<mcp_session::RepositoryRemoteIdentity>,
    pub(crate) checkout_repository_url: Option<String>,
    /// Projects bound to this checkout's Git remote.
    pub(crate) repository_matches: Vec<mcp_types::api::Project>,
    /// The folder's saved project, only when it still validates.
    pub(crate) linked_project: Option<mcp_types::api::Project>,
    pub(crate) legacy_folder_matches: Vec<mcp_types::api::Project>,
    /// The single project named like the folder (a display name, never identity).
    pub(crate) folder_match: Option<mcp_types::api::Project>,
}

/// List the workspace's projects and classify them against `cwd`.
pub(crate) async fn gather_project_candidates(
    client: &ContextStreamClient,
    cwd: &std::path::Path,
    ws: &WorkspaceInfo,
) -> Result<ProjectCandidates> {
    let ws_id = uuid::Uuid::parse_str(&ws.id).ok();
    let folder_project_name = cwd
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project")
        .to_string();

    let mut existing_projects = Vec::new();
    let mut page = 1_i64;
    loop {
        let batch = client.list_projects(ws_id, Some(page), Some(200)).await?;
        // The API may clamp the requested page size to a lower server-side
        // maximum. A short page therefore does not prove that pagination is
        // complete: large workspaces used to hide older projects from this
        // picker, then offer to create a duplicate with the same name. Only
        // an empty page is an unambiguous end-of-list signal.
        if batch.is_empty() {
            break;
        }
        existing_projects.extend(batch);
        page += 1;
        if page > 500 {
            anyhow::bail!(
                "Project selection exceeded 100,000 projects; narrow the workspace or pass an explicit project binding."
            );
        }
    }
    existing_projects.sort_by_key(|a| a.name.to_lowercase());

    // A normalized Git remote is the only portable checkout signal available
    // before this machine has a local binding. It lets an unattended setup on
    // machine B or in a new worktree select the same canonical project created
    // on machine A. Folder basenames are display names, never identity.
    let checkout_repository_identity = checkout_repository_identity(cwd)?;
    let checkout_repository_url = checkout_repository_identity
        .as_ref()
        .map(mcp_session::RepositoryRemoteIdentity::canonical_https_url);
    let repository_matches = checkout_repository_identity
        .as_ref()
        .map(|identity| {
            existing_projects
                .iter()
                .filter(|project| project_repository_identity(project).as_ref() == Some(identity))
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let linked_project_config = read_project_config(cwd).ok().flatten();
    let linked_project_id = linked_project_config
        .as_ref()
        .and_then(|cfg| cfg.project_id.clone());
    let configured_project_name = linked_project_config
        .as_ref()
        .and_then(|cfg| cfg.project_name.as_deref());
    let configured_checkout_root = linked_project_config
        .as_ref()
        .and_then(|cfg| cfg.checkout_root.as_deref());

    let linked_project = linked_project_id.as_deref().and_then(|id| {
        existing_projects
            .iter()
            .find(|project| project.id.to_string() == id)
            // A UUID can remain syntactically valid after a project was
            // renamed, repurposed, or copied from another checkout. Never let
            // that stale local binding outrank the current folder identity:
            // doing so uploads one repository into another project's index.
            .filter(|project| {
                linked_project_name_matches_checkout(
                    &project.name,
                    configured_project_name,
                    configured_checkout_root,
                    cwd,
                )
            })
            .filter(|project| {
                linked_project_repository_is_compatible(
                    project,
                    checkout_repository_identity.as_ref(),
                )
            })
            .cloned()
    });
    let folder_matches: Vec<_> = existing_projects
        .iter()
        .filter(|project| project.name.eq_ignore_ascii_case(&folder_project_name))
        .cloned()
        .collect();
    let legacy_folder_matches = folder_matches
        .iter()
        .filter(|project| project_repository_identity(project).is_none())
        .cloned()
        .collect::<Vec<_>>();
    let folder_match = (folder_matches.len() == 1).then(|| folder_matches[0].clone());

    if linked_project.is_none() && linked_project_id.is_some() {
        warning(
            "The project UUID saved in this directory is unavailable or does not match this checkout. Ignoring the stale binding to prevent cross-project indexing.",
        );
    }

    Ok(ProjectCandidates {
        ws_id,
        folder_project_name,
        existing_projects,
        checkout_repository_identity,
        checkout_repository_url,
        repository_matches,
        linked_project,
        legacy_folder_matches,
        folder_match,
    })
}

/// Resolve/select a project for the current directory.
///
/// When `prompt_user` is true, this shows an interactive selector. Otherwise it
/// uses non-interactive resolution:
/// 1. Keep currently linked project if valid in workspace
/// 2. Re-link to workspace project matching folder name
/// 3. Create project if `allow_create` is true
///
/// Pick a workspace for the current directory by querying the API.
///
/// Used as a fallback by `update_rules` (and any caller that lacks an explicit
/// workspace) so the rule preamble carries a real UUID + name instead of the
/// null UUID when no local `.contextstream/config.json` exists yet.
///
/// Heuristic, in order:
///   1. A workspace that already contains a project whose name matches the
///      current directory's basename (case-insensitive).
///   2. The first workspace whose name is not `.contextstream-global`
///      (the auto-managed fallback workspace).
///   3. The first workspace in the list.
///
/// Returns `None` if the API call fails or the user has no workspaces.
async fn resolve_workspace_from_api(
    client: &ContextStreamClient,
    cwd: &Path,
) -> Option<WorkspaceInfo> {
    let workspaces = match client.list_workspaces(None, Some(100)).await {
        Ok(ws) if !ws.is_empty() => ws,
        _ => return None,
    };

    let cwd_basename = cwd.file_name().and_then(|n| n.to_str());

    // Pass 1: workspace with a project matching the directory basename.
    if let Some(cwd_name) = cwd_basename {
        for ws in &workspaces {
            if let Ok(projects) = client.list_projects(Some(ws.id), None, Some(200)).await {
                if projects
                    .iter()
                    .any(|p| p.name.eq_ignore_ascii_case(cwd_name))
                {
                    return Some(WorkspaceInfo {
                        id: ws.id.to_string(),
                        name: ws.name.clone(),
                    });
                }
            }
        }
    }

    // Pass 2: first workspace that isn't the auto-managed fallback.
    if let Some(ws) = workspaces
        .iter()
        .find(|w| !w.name.starts_with(".contextstream-global"))
    {
        return Some(WorkspaceInfo {
            id: ws.id.to_string(),
            name: ws.name.clone(),
        });
    }

    // Pass 3: any workspace at all (only the fallback is left).
    workspaces.first().map(|ws| WorkspaceInfo {
        id: ws.id.to_string(),
        name: ws.name.clone(),
    })
}

pub async fn select_project_for_current_directory(
    client: &ContextStreamClient,
    cwd: &std::path::Path,
    workspace: Option<&WorkspaceInfo>,
    prompt_user: bool,
    allow_create: bool,
    allow_skip: bool,
) -> Result<Option<ProjectInfo>> {
    let Some(ws) = workspace else {
        if prompt_user {
            println!(
                "{}Select a workspace first before linking a project.",
                info_label()
            );
        }
        return Ok(None);
    };

    let ProjectCandidates {
        ws_id,
        folder_project_name,
        existing_projects,
        checkout_repository_identity,
        checkout_repository_url,
        repository_matches,
        linked_project,
        legacy_folder_matches,
        folder_match,
    } = gather_project_candidates(client, cwd, ws).await?;
    let folder_project_name = folder_project_name.as_str();

    if !prompt_user {
        // A validated checkout-local UUID wins. Otherwise a unique exact
        // credential-free repository match is portable across
        // machines/worktrees. A same-name project is never identity.
        if let Some(project) = linked_project.clone() {
            return Ok(Some(ProjectInfo {
                id: project.id.to_string(),
                name: project.name,
            }));
        }
        match repository_matches.as_slice() {
            [project] => {
                return Ok(Some(ProjectInfo {
                    id: project.id.to_string(),
                    name: project.name.clone(),
                }))
            }
            [] => {}
            duplicates => {
                let ids = duplicates
                    .iter()
                    .map(|project| project.id.to_string())
                    .collect::<Vec<_>>()
                    .join(", ");
                anyhow::bail!(
                    "Multiple projects in workspace {} claim repository {} ({ids}). Refusing to create or choose another duplicate during unattended setup; select or merge the canonical project explicitly.",
                    ws.id,
                    checkout_repository_identity
                        .as_ref()
                        .map(ToString::to_string)
                        .unwrap_or_else(|| "unknown".to_string())
                );
            }
        }
        if checkout_repository_identity.is_some() && !legacy_folder_matches.is_empty() {
            let ids = legacy_folder_matches
                .iter()
                .map(|project| project.id.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!(
                "Same-name legacy project(s) {ids} in workspace {} have no trustworthy repository identity. Refusing to create another project during unattended setup; select the canonical project explicitly once so it can be bound and backfilled.",
                ws.id
            );
        }

        if allow_create {
            if safe_edit::is_dry_run() {
                warning(
                    "Dry-run did not create a server-side project; previewing editor files without a new project binding.",
                );
                return Ok(None);
            }
            let project = client
                .create_project_with_repository(
                    folder_project_name,
                    None,
                    ws_id,
                    checkout_repository_url.as_deref(),
                )
                .await?;
            return Ok(Some(ProjectInfo {
                id: project.id.to_string(),
                name: project.name,
            }));
        }

        return Ok(None);
    }

    ui::say(
        ui::Mark::Info,
        &format!("Choose the project for {}", wizard::display_path(cwd)),
        Some(&format!("workspace {}", ws.name)),
    );

    enum ProjectChoice {
        Existing(ProjectInfo),
        CreateNew,
        Skip,
    }

    // Strongest evidence first, then creating a project named after the
    // folder, then every other project. Enter therefore never links this
    // folder to an unrelated project by accident.
    let mut options: Vec<(String, ProjectChoice)> = Vec::new();
    let mut added = HashSet::new();

    if let Some(project) = linked_project.as_ref() {
        added.insert(project.id.to_string());
        options.push((
            format!("{} · linked to this folder", project.name),
            ProjectChoice::Existing(ProjectInfo::from(project)),
        ));
    }

    for project in &repository_matches {
        if added.insert(project.id.to_string()) {
            options.push((
                format!("{} · matches this Git repository", project.name),
                ProjectChoice::Existing(ProjectInfo::from(project)),
            ));
        }
    }

    if let Some(project) = folder_match.as_ref() {
        if added.insert(project.id.to_string()) {
            options.push((
                format!("{} · same name as this folder", project.name),
                ProjectChoice::Existing(ProjectInfo::from(project)),
            ));
        }
    }

    if allow_create {
        options.push((
            format!("Create a new project named {}", folder_project_name),
            ProjectChoice::CreateNew,
        ));
    }

    for project in &existing_projects {
        if added.insert(project.id.to_string()) {
            options.push((
                project.name.clone(),
                ProjectChoice::Existing(ProjectInfo::from(project)),
            ));
        }
    }

    if allow_skip {
        options.push((
            "None · don't link a project".to_string(),
            ProjectChoice::Skip,
        ));
    }

    if options.is_empty() {
        return Ok(None);
    }

    let option_refs: Vec<&str> = options.iter().map(|(label, _)| label.as_str()).collect();
    let choice = prompts::select("Which project?", &option_refs)?;

    match &options[choice].1 {
        ProjectChoice::Existing(project) => Ok(Some(project.clone())),
        ProjectChoice::CreateNew => {
            if safe_edit::is_dry_run() {
                anyhow::bail!(
                    "Dry-run refused to create a server-side project. \
                     Create or select it first, then re-run the preview."
                );
            }
            let project = client
                .create_project_with_repository(
                    folder_project_name,
                    None,
                    ws_id,
                    checkout_repository_url.as_deref(),
                )
                .await?;
            Ok(Some(ProjectInfo {
                id: project.id.to_string(),
                name: project.name,
            }))
        }
        ProjectChoice::Skip => Ok(None),
    }
}

fn project_repository_identity(
    project: &mcp_types::api::Project,
) -> Option<mcp_session::RepositoryRemoteIdentity> {
    let value = project.repository_url.as_deref()?.trim();
    if value.starts_with("git-remote-v1:") {
        mcp_session::RepositoryRemoteIdentity::parse(value).ok()
    } else {
        mcp_session::RepositoryRemoteIdentity::from_remote_url(value).ok()
    }
}

fn checkout_repository_identity(
    checkout: &Path,
) -> Result<Option<mcp_session::RepositoryRemoteIdentity>> {
    match mcp_session::current_repository_remote_identity(checkout) {
        Ok(identity) => Ok(identity),
        Err(mcp_session::CheckoutIdentityError::NotGitCheckout(_)) => Ok(None),
        Err(error) => Err(anyhow::Error::new(error).context(format!(
            "Could not establish a credential-free repository identity for {}",
            checkout.display()
        ))),
    }
}

fn linked_project_repository_is_compatible(
    project: &mcp_types::api::Project,
    checkout: Option<&mcp_session::RepositoryRemoteIdentity>,
) -> bool {
    match (project_repository_identity(project), checkout) {
        (Some(project), Some(checkout)) => &project == checkout,
        // Legacy projects may not have repository metadata yet. Their
        // checkout-local UUID still needs the root/name validation above.
        (None, _) | (_, None) => true,
    }
}

/// Return whether a server-side project name is safe for the current checkout.
///
/// Saved project IDs are only trusted when the server-side name still matches
/// the checkout directory name, or the configured project name together with
/// an exact canonical checkout-root binding. Legacy configs without a root
/// marker can therefore use conventional folder-matched projects, but cannot
/// silently authorize a differently named project after being copied.
pub fn linked_project_name_matches_checkout(
    server_project_name: &str,
    configured_project_name: Option<&str>,
    configured_checkout_root: Option<&str>,
    checkout_path: &Path,
) -> bool {
    let server_project_name = server_project_name.trim();
    let folder_project_name = checkout_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("project")
        .trim();
    if configured_checkout_root.is_some()
        && !checkout_root_matches(configured_checkout_root, checkout_path)
    {
        return false;
    }
    if server_project_name.eq_ignore_ascii_case(folder_project_name) {
        return true;
    }

    let configured_matches = configured_project_name
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .is_some_and(|name| server_project_name.eq_ignore_ascii_case(name));
    configured_matches && checkout_root_matches(configured_checkout_root, checkout_path)
}

/// Fire a warmup context call so the API is primed for the first real request.
pub async fn warmup_context(client: &ContextStreamClient, workspace_id: Option<&String>) {
    let ws_id = workspace_id.and_then(|s| uuid::Uuid::parse_str(s).ok());
    let params = ContextParams {
        user_message: "warmup".to_string(),
        grounding_handle: None,
        workspace_id: ws_id,
        project_id: None,
        installation_id: None,
        checkout_locator: None,
        folder_path: None,
        session_id: None,
        format: None,
        tokenizer: None,
        mode: None,
        distill: None,
        max_tokens: None,
        session_tokens: None,
        context_threshold: None,
        save_exchange: Some(false),
        client_name: None,
        tool_surface_profile: None,
        assistant_message: None,
        delta_since: None,
        turn_number: None,
    };
    match client.context_smart(params).await {
        Ok(_) => tracing::debug!("context warmup complete"),
        Err(e) => tracing::debug!("context warmup failed (non-fatal): {}", e),
    }
}

/// Silently measure edge latency and report setup telemetry.
///
/// Runs in the background during setup so the user never sees it.
/// Pings the MCP health endpoint to measure latency and detect the
/// Cloudflare POP, then reports the results alongside the chosen
/// transport mode to the API for aggregate latency analytics.
/// Fire-and-forget context warmup so the first real call is fast without
/// blocking the success banner.
fn spawn_warmup(client: &ContextStreamClient, workspace_id: Option<&String>) {
    if safe_edit::is_dry_run() {
        return;
    }
    let client = client.clone();
    let workspace_id = workspace_id.cloned();
    tokio::spawn(async move {
        warmup_context(&client, workspace_id.as_ref()).await;
    });
}

/// Start the first index in a detached worker process so it keeps running
/// after setup exits. The worker (`index --background-worker`) owns the
/// status file, the desktop notification, and the context warm-up.
fn spawn_background_index(
    _client: ContextStreamClient,
    path: std::path::PathBuf,
    workspace_id: Option<String>,
    project_id: Option<uuid::Uuid>,
    include_media: bool,
) {
    if safe_edit::is_dry_run() {
        return;
    }
    match spawn_detached_index_worker(
        &path,
        workspace_id.as_deref(),
        project_id,
        include_media,
        false,
    ) {
        Ok(()) => ui::say(
            ui::Mark::Step,
            "Indexing in the background",
            Some("search fills in as it builds · you'll get a notification"),
        ),
        Err(error) => ui::say(
            ui::Mark::Warn,
            "Couldn't start background indexing",
            Some(&format!(
                "{error} · run `contextstream-mcp index --path {}` instead",
                wizard::display_path(&path)
            )),
        ),
    }
}

/// Arguments for the detached index worker.
pub fn index_worker_args(path: &Path, include_media: bool, force: bool) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = vec![
        if force { "ingest" } else { "index" }.into(),
        "--path".into(),
        path.as_os_str().to_owned(),
        "--background-worker".into(),
    ];
    if include_media {
        args.push("--include-media".into());
    }
    if force {
        args.push("--force".into());
    }
    args
}

/// Spawn this binary as a detached index worker: no stdio, its own process
/// group (so Ctrl+C and terminal hangup in the parent do not reach it), and
/// the workspace/project hints the caller resolved.
pub fn spawn_detached_index_worker(
    path: &Path,
    workspace_id: Option<&str>,
    project_id: Option<uuid::Uuid>,
    include_media: bool,
    force: bool,
) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    // A binary replaced while running reports "<path> (deleted)" on Linux.
    let exe = exe
        .to_str()
        .and_then(|raw| raw.strip_suffix(" (deleted)"))
        .map(PathBuf::from)
        .filter(|live| live.exists())
        .unwrap_or(exe);

    let mut command = std::process::Command::new(exe);
    command
        .args(index_worker_args(path, include_media, force))
        .current_dir(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Some(workspace_id) = workspace_id {
        command.env("CONTEXTSTREAM_WORKSPACE_ID", workspace_id);
    }
    if let Some(project_id) = project_id {
        command.env("CONTEXTSTREAM_PROJECT_ID", project_id.to_string());
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    command.spawn().map(|_| ())
}

pub async fn report_setup_telemetry(
    client: &ContextStreamClient,
    transport: SetupTransportPreference,
) {
    if safe_edit::is_dry_run() {
        return;
    }
    const MCP_HEALTH_URL: &str = "https://mcp.contextstream.io/health";

    let http = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(_) => return,
    };

    let start = std::time::Instant::now();
    let resp = http.get(MCP_HEALTH_URL).send().await;
    let latency_ms = start.elapsed().as_millis() as u64;

    let (cf_pop, status_code) = match resp {
        Ok(r) => {
            let pop = r
                .headers()
                .get("cf-ray")
                .and_then(|v| v.to_str().ok())
                .map(|ray| {
                    if ray.len() >= 3 {
                        ray[ray.len().saturating_sub(3)..].to_string()
                    } else {
                        ray.to_string()
                    }
                })
                .unwrap_or_default();
            let code = r.status().as_u16();
            (pop, code)
        }
        Err(_) => (String::new(), 0u16),
    };

    let payload = serde_json::json!({
        "event": "setup_latency",
        "version": mcp_types::config::VERSION,
        "transport": transport.as_marker_value(),
        "latency_ms": latency_ms,
        "cf_pop": cf_pop,
        "status_code": status_code,
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
    });

    let _: Result<serde_json::Value, _> = client.post("/cli/telemetry", payload).await;
}

/// Index a project with progress tracking.
pub async fn index_project(
    client: &ContextStreamClient,
    path: &std::path::Path,
    workspace_id: Option<&String>,
    pre_resolved_project_id: Option<uuid::Uuid>,
    include_media: bool,
    force: bool,
) -> Result<()> {
    let project_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");
    if safe_edit::is_dry_run() {
        println!(
            "{} Dry-run: would index project {} after applying the previewed configuration.",
            info_label(),
            style(project_name).cyan()
        );
        return Ok(());
    }

    // Single-line spinner with one steady message that lasts the whole
    // operation. No bar, no per-file counter, no elapsed time, no
    // multi-line layout — the cursor character cycles in place at the
    // start of the line, the text never changes during the active run.
    let progress_pb = ui::ui().spinner("Indexing this project · a minute or two");
    // Task #15: track when we started waiting on the server so the UX can
    // pivot to "indexing continues in background" if no visible server
    // progress arrives within the SLO's degrade window. `waiting_since`
    // is set on the first WaitingForServer event and cleared on any
    // ServerProgress / UploadProgress; the three state-change guards
    // below make sure we only *display* the transitions once.
    // Track whether the server-degraded threshold fires so the final
    // success line can mention "indexing continues in the background"
    // when relevant. We don't change the active spinner message — it
    // stays on the single steady "Catching the stream! Project
    // mapping and setup — takes a minute or two" string for the
    // whole run.
    let mut waiting_since: Option<std::time::Instant> = None;
    let mut degraded_shown = false;
    const DEGRADE_AFTER: std::time::Duration = std::time::Duration::from_secs(60);

    // Phase 2: Setup project — use pre-resolved ID if available
    let project_id = if let Some(pid) = pre_resolved_project_id {
        pid
    } else {
        let ws_id = workspace_id.and_then(|s| uuid::Uuid::parse_str(s).ok());
        // This helper has no proof that a same-name server project represents
        // this checkout. Callers that intend reuse must resolve and pass an
        // explicit validated project UUID.
        let repository_url = checkout_repository_identity(path)?
            .as_ref()
            .map(mcp_session::RepositoryRemoteIdentity::canonical_https_url);
        client
            .create_project_with_repository(project_name, None, ws_id, repository_url.as_deref())
            .await?
            .id
    };
    let ingest_params = IngestLocalParams {
        path: path.to_string_lossy().to_string(),
        workspace_id: workspace_id.and_then(|value| uuid::Uuid::parse_str(value).ok()),
        project_id: Some(project_id),
        force: Some(force),
        generate_editor_rules: None,
        include_media: Some(include_media),
        max_files: None,
        // B1.13: opt into the async fast-path (RFC a07d1e83). Server stages
        // the batch payload to R2 + returns 202; this client polls progress
        // via wait_for_ingest_jobs. The other two ingest_local sites in
        // this file (line 557 auto-index + line 2826 silent helper) have
        // always used Some(true). Credit semantics unchanged: the wizard
        // already sends the WIZARD_FLOW_HEADER which triggers credit-skip
        // independently of `background`.
        background: Some(true),
        origin: Some("setup_wizard".to_string()),
        reroot: None,
    };

    let result = client
        .ingest_local_with_progress(ingest_params, |event| {
            // We deliberately don't update the spinner message during the
            // run — it stays on the single steady "Indexing — large
            // projects continue in the background" line. The only thing
            // we track here is whether the server entered a long wait
            // (degraded), so the FINAL line can mention background.
            if let IngestProgressEvent::WaitingForServer { .. } = event {
                let started = *waiting_since.get_or_insert_with(std::time::Instant::now);
                if started.elapsed() >= DEGRADE_AFTER {
                    degraded_shown = true;
                }
            } else if matches!(event, IngestProgressEvent::ServerProgress { .. }) {
                waiting_since = None;
            }
        })
        .await;

    match result {
        Ok(value) => {
            progress_pb.finish_and_clear();
            // The client now returns Ok with `pending_jobs > 0` when
            // it stopped waiting after the soft-wait window — flip
            // the message even if the in-loop degrade trigger
            // didn't fire.
            let still_indexing = value
                .get("pending_jobs")
                .and_then(|v| v.as_i64())
                .map(|n| n > 0)
                .unwrap_or(false);
            if degraded_shown || still_indexing {
                ui::say(
                    ui::Mark::Ok,
                    "Project linked",
                    Some("indexing continues in the background"),
                );
            } else {
                ui::say(ui::Mark::Ok, "Project indexed", None);
            }
        }
        Err(error) => {
            progress_pb.finish_and_clear();
            eprintln!(
                "{}",
                ui::ui().line(
                    ui::Mark::Fail,
                    "Indexing failed",
                    Some(&sanitize_index_error(&error))
                )
            );
            return Err(error.into());
        }
    }

    Ok(())
}

#[allow(dead_code)] // kept around for the unit tests at the bottom of this
                    // module; production code now uses a single tidy
                    // "Index update complete" line via finish_and_clear.
fn format_index_completion_message(result: &serde_json::Value) -> String {
    let files_indexed = result
        .get("files_indexed")
        .and_then(|value| value.as_i64())
        .unwrap_or(0);
    let files_skipped = result
        .get("files_skipped")
        .and_then(|value| value.as_i64())
        .unwrap_or(0);
    let files_compacted = result
        .get("files_compacted")
        .and_then(|value| value.as_i64())
        .unwrap_or(0);
    let files_deferred = result
        .get("files_deferred")
        .and_then(|value| value.as_i64())
        .unwrap_or(0);

    // When the message branch already produces a parenthesized summary
    // we suppress the notes-appendage below so we don't double-render
    // the deferred count.
    let mut suppress_notes_paren = false;

    let mut message = match result
        .get("message")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
    {
        "No files to index" => format!("{} No indexable files found", CHECK),
        "All files unchanged" => {
            let mut parts = vec![format!(
                "{} file{} unchanged",
                files_skipped,
                if files_skipped == 1 { "" } else { "s" }
            )];
            if files_compacted > 0 {
                parts.push(format!(
                    "{} large file{} compacted for upload",
                    files_compacted,
                    if files_compacted == 1 { "" } else { "s" }
                ));
            }
            if files_deferred > 0 {
                parts.push(format!(
                    "{} oversized file{} deferred",
                    files_deferred,
                    if files_deferred == 1 { "" } else { "s" }
                ));
            }
            suppress_notes_paren = true;
            format!(
                "{} Index is already up to date ({})",
                CHECK,
                parts.join(", ")
            )
        }
        // The default "X files indexed" wording reads as failure when X=0
        // even though nothing went wrong (everything was dedup'd). Lead
        // with "up to date" when nothing was newly indexed and there are
        // skipped files to point at, otherwise keep the "indexed N" phrasing.
        _ => {
            if files_indexed == 0 && files_skipped > 0 {
                format!(
                    "{} Index is up to date: {} file{} unchanged",
                    CHECK,
                    files_skipped,
                    if files_skipped == 1 { "" } else { "s" }
                )
            } else if files_skipped > 0 {
                format!(
                    "{} Index update complete: {} newly indexed, {} unchanged",
                    CHECK, files_indexed, files_skipped
                )
            } else {
                format!(
                    "{} Index update complete: {} file{} indexed",
                    CHECK,
                    files_indexed,
                    if files_indexed == 1 { "" } else { "s" }
                )
            }
        }
    };

    if !suppress_notes_paren {
        let mut notes = Vec::new();
        if files_compacted > 0 {
            notes.push(format!(
                "{} large file{} compacted for upload",
                files_compacted,
                if files_compacted == 1 { "" } else { "s" }
            ));
        }
        if files_deferred > 0 {
            notes.push(format!(
                "{} oversized file{} deferred",
                files_deferred,
                if files_deferred == 1 { "" } else { "s" }
            ));
        }
        if !notes.is_empty() {
            message.push_str(&format!(" ({})", notes.join(", ")));
        }
    }

    message
}

/// Index a project in the background (no progress bar).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackgroundIndexOutcome {
    pub files_indexed: usize,
    pub pending_jobs: usize,
    pub committed: bool,
    pub scan_complete: bool,
    pub files_deferred: usize,
}

pub async fn index_project_background(
    client: &ContextStreamClient,
    path: &std::path::Path,
    workspace_id: Option<&String>,
    pre_resolved_project_id: Option<uuid::Uuid>,
    include_media: bool,
    force: bool,
) -> Result<BackgroundIndexOutcome> {
    let project_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project");

    let project_id = if let Some(pid) = pre_resolved_project_id {
        pid
    } else {
        let ws_id = workspace_id.and_then(|s| uuid::Uuid::parse_str(s).ok());
        // Reuse requires a pre-resolved UUID backed by explicit selection or a
        // validated checkout binding; a folder-name collision is insufficient.
        let repository_url = checkout_repository_identity(path)?
            .as_ref()
            .map(mcp_session::RepositoryRemoteIdentity::canonical_https_url);
        client
            .create_project_with_repository(project_name, None, ws_id, repository_url.as_deref())
            .await?
            .id
    };
    let result = client
        .ingest_local(IngestLocalParams {
            path: path.to_string_lossy().to_string(),
            workspace_id: workspace_id.and_then(|value| uuid::Uuid::parse_str(value).ok()),
            project_id: Some(project_id),
            force: Some(force),
            generate_editor_rules: None,
            include_media: Some(include_media),
            max_files: None,
            background: Some(true),
            origin: None,
            reroot: None,
        })
        .await?;

    let files_indexed = result
        .get("files_indexed")
        .and_then(|value| value.as_i64())
        .unwrap_or(0)
        .max(0) as usize;
    let pending_jobs = result
        .get("pending_jobs")
        .and_then(|value| value.as_i64())
        .unwrap_or(0)
        .max(0) as usize;
    let files_deferred = result
        .get("files_deferred")
        .and_then(|value| value.as_i64())
        .unwrap_or(0)
        .max(0) as usize;
    Ok(BackgroundIndexOutcome {
        files_indexed,
        pending_jobs,
        committed: ContextStreamClient::ingest_result_committed(&result),
        scan_complete: ContextStreamClient::ingest_scan_complete(&result),
        files_deferred,
    })
}

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone)]
struct SetupIngestJobProgress {
    status: String,
    phase: String,
    files_total: usize,
    files_processed: usize,
    #[allow(dead_code)]
    error_message: Option<String>,
}

#[cfg_attr(not(test), allow(dead_code))]
fn parse_setup_ingest_job_progress(
    job_id: uuid::Uuid,
    progress_value: &serde_json::Value,
    fallback_file_count: usize,
) -> SetupIngestJobProgress {
    let status = progress_value
        .get("status")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let phase = progress_value
        .get("phase")
        .and_then(|v| v.as_str())
        .unwrap_or(status.as_str())
        .to_string();

    let files_total = progress_value
        .get("files_total")
        .and_then(|v| v.as_u64())
        .map(|value| value as usize)
        .unwrap_or(fallback_file_count);
    let files_processed = progress_value
        .get("files_processed")
        .and_then(|v| v.as_u64())
        .map(|value| value as usize)
        .unwrap_or_else(|| {
            if matches!(status.as_str(), "completed" | "failed") {
                files_total
            } else {
                0
            }
        });
    let error_message = progress_value
        .get("error_message")
        .and_then(|v| v.as_str())
        .map(ToString::to_string)
        .or_else(|| {
            if status == "failed" {
                Some(format!("ingest job {} failed during {}", job_id, phase))
            } else {
                None
            }
        });

    SetupIngestJobProgress {
        status,
        phase,
        files_total,
        files_processed,
        error_message,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ambiguous_workspace_message, canonical_checkout_root, classify_project_workspace,
        data_collection_disclosure, editor_needs_managed_helper, format_index_completion_message,
        linked_project_name_matches_checkout, local_mcp_allowed,
        local_mcp_override_allowed_from_env_value, no_target_editors_message,
        parse_setup_ingest_job_progress, parse_setup_transport_preference,
        project_workspace_is_verified, read_setup_transport_marker,
        read_setup_transport_marker_result, require_project_workspace_ownership,
        resolve_hook_refresh_editors, resolve_hook_refresh_editors_from,
        resolve_setup_project_path, sanitize_index_error, select_project_for_current_directory,
        setup_completion_evidence, setup_index_choices, setup_path_has_project_content_files,
        setup_path_is_project_candidate, setup_teaching_contracts, setup_transport_marker_path,
        targets_need_hosted_sync_bridge, targets_need_managed_helper, validate_editor_scope,
        write_setup_transport_marker, SetupCompletionState, SetupTransportPreference,
        WorkspaceInfo, WorkspaceOwnershipEvidence, ENV_ALLOW_LOCAL_MCP,
    };
    use crate::setup::editors::Editor;
    use std::path::Path;

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// `setup --yes` on a multi-workspace account must say how to choose one
    /// non-interactively (issue #106), not only point at the interactive wizard.
    #[test]
    fn ambiguous_workspace_message_lists_choices_and_the_non_interactive_selectors() {
        let message = ambiguous_workspace_message(
            [
                (
                    "11111111-1111-1111-1111-111111111111".to_string(),
                    "Personal",
                ),
                ("22222222-2222-2222-2222-222222222222".to_string(), "Team"),
            ]
            .into_iter(),
        );
        assert!(message.contains("your account has 2:"), "{message}");
        assert!(
            message.contains("11111111-1111-1111-1111-111111111111  Personal"),
            "{message}"
        );
        assert!(
            message.contains("22222222-2222-2222-2222-222222222222  Team"),
            "{message}"
        );
        assert!(message.contains("--workspace-id <UUID>"), "{message}");
        assert!(message.contains("CONTEXTSTREAM_WORKSPACE_ID"), "{message}");
    }

    #[test]
    fn setup_disclosure_names_enabled_defaults_fields_and_opt_outs() {
        let _guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let previous_transcripts = std::env::var("CONTEXTSTREAM_TRANSCRIPTS_ENABLED").ok();
        let previous_hook_transcripts =
            std::env::var("CONTEXTSTREAM_HOOK_TRANSCRIPTS_ENABLED").ok();
        let previous_git_capture = std::env::var("CONTEXTSTREAM_GIT_CAPTURE").ok();
        std::env::remove_var("CONTEXTSTREAM_TRANSCRIPTS_ENABLED");
        std::env::remove_var("CONTEXTSTREAM_HOOK_TRANSCRIPTS_ENABLED");
        std::env::remove_var("CONTEXTSTREAM_GIT_CAPTURE");

        let disclosure = data_collection_disclosure(true);
        assert!(disclosure.contains("Transcript exchange saving default: true"));
        assert!(disclosure.contains("Hook transcript saving default: true"));
        assert!(disclosure.contains("matched source files are sent"));
        assert!(disclosure.contains("256-character redacted commit subject"));
        assert!(disclosure
            .contains("Absolute paths, commit bodies, and author name/email are not sent"));
        assert!(disclosure.contains("CONTEXTSTREAM_GIT_CAPTURE=off"));
        assert!(disclosure.contains("project(action=\"purge\")"));

        for (name, previous) in [
            ("CONTEXTSTREAM_TRANSCRIPTS_ENABLED", previous_transcripts),
            (
                "CONTEXTSTREAM_HOOK_TRANSCRIPTS_ENABLED",
                previous_hook_transcripts,
            ),
            ("CONTEXTSTREAM_GIT_CAPTURE", previous_git_capture),
        ] {
            match previous {
                Some(value) => std::env::set_var(name, value),
                None => std::env::remove_var(name),
            }
        }
    }

    #[test]
    fn rules_only_editors_do_not_require_a_helper_or_hosted_bridge() {
        assert!(!editor_needs_managed_helper(&Editor::Aider));
        assert!(!targets_need_managed_helper(&[Editor::Aider]));
        assert!(!targets_need_hosted_sync_bridge(&[Editor::Aider], true));

        assert!(editor_needs_managed_helper(&Editor::Codex));
        assert!(targets_need_managed_helper(&[Editor::Aider, Editor::Codex]));
        assert!(targets_need_hosted_sync_bridge(
            &[Editor::Aider, Editor::Codex],
            true
        ));
        assert!(!targets_need_hosted_sync_bridge(
            &[Editor::Aider, Editor::Codex],
            false
        ));

        assert!(editor_needs_managed_helper(&Editor::ClaudeCode));
        assert!(targets_need_hosted_sync_bridge(&[Editor::ClaudeCode], true));
    }

    #[test]
    fn setup_accepts_empty_folders_but_never_home_or_root() {
        let _guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let home = temp.path().join("home");
        let project = temp.path().join("project");
        let empty = temp.path().join("empty");
        let metadata_only = temp.path().join("metadata-only");
        let regular_file = temp.path().join("not-a-folder");
        std::fs::create_dir_all(&home).expect("create home");
        std::fs::create_dir_all(project.join("src")).expect("create project");
        std::fs::create_dir_all(&empty).expect("create empty directory");
        std::fs::create_dir_all(&metadata_only).expect("create metadata-only directory");
        std::fs::write(metadata_only.join(".gitkeep"), "").expect("seed ignored metadata");
        std::fs::write(&regular_file, "not a directory\n").expect("seed regular file");
        std::fs::write(project.join("src/lib.rs"), "pub fn indexed() {}\n")
            .expect("seed project source");

        let previous_home = std::env::var_os("HOME");
        std::env::set_var("HOME", &home);

        assert!(!setup_path_is_project_candidate(&home));
        assert!(!setup_path_is_project_candidate(Path::new("/")));
        assert!(!setup_path_is_project_candidate(&regular_file));
        #[cfg(unix)]
        {
            let home_alias = temp.path().join("home-alias");
            std::os::unix::fs::symlink(&home, &home_alias).expect("home symlink");
            assert!(!setup_path_is_project_candidate(&home_alias));
            assert!(resolve_setup_project_path(&project, Some(&home_alias), false).is_err());
        }
        assert!(setup_path_is_project_candidate(&empty));
        assert!(setup_path_is_project_candidate(&metadata_only));
        assert!(setup_path_is_project_candidate(&project));
        assert!(!setup_path_has_project_content_files(&empty, false));
        assert!(!setup_path_has_project_content_files(&metadata_only, false));
        assert_eq!(
            resolve_setup_project_path(&home, None, false).expect("home should be a partial scope"),
            None
        );
        assert_eq!(
            resolve_setup_project_path(temp.path(), Some(Path::new("project")), false)
                .expect("relative explicit project"),
            Some(project.canonicalize().expect("canonical project"))
        );
        assert_eq!(
            resolve_setup_project_path(&empty, None, false).expect("empty cwd project"),
            Some(empty.canonicalize().expect("canonical empty project"))
        );
        assert_eq!(
            resolve_setup_project_path(temp.path(), Some(Path::new("empty")), false)
                .expect("explicit empty project"),
            Some(empty.canonicalize().expect("canonical empty project"))
        );
        assert!(resolve_setup_project_path(&project, Some(&home), false).is_err());
        assert!(resolve_setup_project_path(&project, Some(&project), true).is_err());

        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn setup_completion_states_require_exact_evidence() {
        let state = |configured,
                     mcp_editors,
                     project_path,
                     project,
                     binding,
                     index,
                     awaiting_first_files,
                     doctor,
                     account_only,
                     dry_run| {
            setup_completion_evidence(
                configured,
                mcp_editors,
                project_path,
                project,
                binding,
                index,
                awaiting_first_files,
                doctor,
                account_only,
                dry_run,
            )
            .state
        };

        assert_eq!(
            state(1, 1, true, true, true, true, false, true, false, true),
            SetupCompletionState::DryRunPreview
        );
        assert_eq!(
            state(0, 0, true, true, true, true, false, true, false, false),
            SetupCompletionState::NoClientConfigured
        );
        assert_eq!(
            state(1, 0, true, true, true, true, false, false, false, false),
            SetupCompletionState::RepairRequired
        );
        assert_eq!(
            state(1, 0, true, true, true, true, false, true, false, false),
            SetupCompletionState::RulesOnlyReady
        );
        assert_eq!(
            state(1, 1, false, false, false, false, false, true, true, false),
            SetupCompletionState::AccountOnly
        );
        for incomplete in [
            state(1, 1, false, false, false, false, false, true, false, false),
            state(1, 1, true, false, false, false, false, true, false, false),
            state(1, 1, true, true, false, false, false, true, false, false),
        ] {
            assert_eq!(incomplete, SetupCompletionState::ProjectRequired);
        }
        assert_eq!(
            state(1, 1, true, true, true, false, false, true, false, false),
            SetupCompletionState::IndexRequired
        );
        let waiting =
            setup_completion_evidence(1, 1, true, true, true, false, true, true, false, false);
        assert_eq!(waiting.state, SetupCompletionState::RestartRequired);
        assert!(waiting.awaiting_first_files);
        assert!(!waiting.index_started);

        let complete =
            setup_completion_evidence(1, 1, true, true, true, true, false, true, false, false);
        assert_eq!(complete.state, SetupCompletionState::RestartRequired);
        assert_eq!(complete.mcp_editor_count, 1);
        assert!(
            !complete.runtime_connected,
            "installer evidence must never fabricate a runtime handshake"
        );
    }

    #[test]
    fn hook_refresh_prefers_explicit_editor_list() {
        let (editors, provenance) = resolve_hook_refresh_editors_from(
            Some(&[Editor::Codex]),
            false,
            &ids(&["claude", "cursor"]),
            true,
            || panic!("detection must not run when an explicit list is given"),
        );

        assert_eq!(editors, vec![Editor::Codex]);
        assert_eq!(provenance, "requested");
    }

    #[test]
    fn explicit_editor_scope_ignores_malformed_historical_selection() {
        let _guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        std::env::set_var("HOME", temp.path());
        let state_dir = temp.path().join(".contextstream");
        std::fs::create_dir_all(&state_dir).expect("create state dir");
        std::fs::write(state_dir.join("installation.json"), "{not valid json")
            .expect("seed malformed state");

        let result = resolve_hook_refresh_editors(Some(&[Editor::Codex, Editor::Codex]), false);

        if let Some(home) = previous_home {
            std::env::set_var("HOME", home);
        } else {
            std::env::remove_var("HOME");
        }

        let (editors, provenance) = result.expect("explicit scope must not read historical state");
        assert_eq!(editors, vec![Editor::Codex]);
        assert_eq!(provenance, "requested");
    }

    #[test]
    fn refresh_scope_validation_rejects_unknown_values() {
        assert!(validate_editor_scope("global", "test").is_ok());
        assert!(validate_editor_scope("project", "test").is_ok());
        assert!(validate_editor_scope("all", "test").is_ok());
        let error = validate_editor_scope("globla", "test")
            .expect_err("a misspelled scope must not become a silent no-op");
        assert!(error.to_string().contains("Unknown scope 'globla'"));
    }

    #[test]
    fn hook_refresh_uses_setup_selection_over_detection() {
        let (editors, provenance) =
            resolve_hook_refresh_editors_from(None, false, &ids(&["codex"]), true, || {
                panic!("detection must not run when setup recorded a selection")
            });

        // Claude Code is installed on this machine but was never selected, so
        // it must not appear here — this is the reported support bug.
        assert_eq!(editors, vec![Editor::Codex]);
        assert!(!editors.contains(&Editor::ClaudeCode));
        assert_eq!(provenance, "configured by setup");
    }

    #[test]
    fn hook_refresh_without_selection_is_a_noop_when_only_configured() {
        let (editors, _) = resolve_hook_refresh_editors_from(None, true, &[], false, || {
            panic!("detection must not run in only-configured mode")
        });

        assert!(
            editors.is_empty(),
            "unattended refresh must not enroll editors on its own"
        );
    }

    #[test]
    fn hook_refresh_falls_back_to_detection_for_legacy_installs() {
        let (editors, provenance) =
            resolve_hook_refresh_editors_from(None, false, &[], false, || {
                vec![Editor::Cursor, Editor::Cursor]
            });

        assert_eq!(editors, vec![Editor::Cursor]);
        assert_eq!(provenance, "detected");
    }

    #[test]
    fn explicitly_empty_setup_selection_never_falls_back_to_detection() {
        let (editors, provenance) =
            resolve_hook_refresh_editors_from(None, false, &[], true, || {
                panic!("detection must not run after setup recorded an empty selection")
            });

        assert!(editors.is_empty());
        assert_eq!(provenance, "configured by setup");
    }

    /// The reported support scenario, generalised: a user who selected only
    /// Codex must not have Claude Code touched by *any* refresh command —
    /// hooks, rules, MCP configs, or the hosted-remote migration. Rules files
    /// matter as much as hooks here: CLAUDE.md is injected into every session.
    #[test]
    fn no_refresh_command_touches_an_unselected_editor() {
        let configured = ids(&["codex"]);

        for only_configured in [false, true] {
            let (editors, _) =
                resolve_hook_refresh_editors_from(None, only_configured, &configured, true, || {
                    panic!("detection must not run when setup recorded a selection")
                });

            assert!(
                !editors.contains(&Editor::ClaudeCode),
                "Claude Code leaked into the target set (only_configured={only_configured})"
            );
            assert_eq!(editors, vec![Editor::Codex]);
        }
    }

    #[test]
    fn explicit_list_can_narrow_below_the_recorded_selection() {
        let (editors, provenance) = resolve_hook_refresh_editors_from(
            Some(&[Editor::Cursor]),
            false,
            &ids(&["claude", "cursor", "codex"]),
            true,
            Vec::new,
        );

        assert_eq!(editors, vec![Editor::Cursor]);
        assert_eq!(provenance, "requested");
    }

    #[test]
    fn no_target_message_distinguishes_the_two_cases() {
        assert!(no_target_editors_message(true).contains("contextstream-mcp setup"));
        assert!(!no_target_editors_message(false).contains("contextstream-mcp setup"));
    }

    #[test]
    fn onboarding_uses_the_shared_versioned_contract_for_each_selected_editor() {
        let contracts =
            setup_teaching_contracts(&[Editor::ClaudeCode, Editor::Codex, Editor::Aider]);
        assert_eq!(contracts.len(), 3);
        assert!(contracts
            .iter()
            .all(|contract| contract.teaching_version == mcp_types::HARNESS_TEACHING_VERSION));
        assert_eq!(
            contracts[0].harness_id,
            Some(mcp_types::HarnessId::ClaudeCode)
        );
        assert_eq!(contracts[1].harness_id, Some(mcp_types::HarnessId::Codex));
        assert_eq!(contracts[2].harness_id, Some(mcp_types::HarnessId::Aider));
        assert!(contracts[2]
            .delivery_notice
            .as_deref()
            .is_some_and(|notice| notice.contains("no native ContextStream MCP")));
    }

    #[test]
    fn hook_refresh_ignores_unknown_configured_ids() {
        let (editors, _) =
            resolve_hook_refresh_editors_from(None, true, &ids(&["not-an-editor"]), true, Vec::new);

        assert!(editors.is_empty());
    }
    use crate::env_test_mutex;
    use mcp_client::ContextStreamClient;
    use mcp_types::Config;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use uuid::Uuid;

    #[test]
    fn project_workspace_verification_requires_positive_exact_ownership() {
        let expected = Uuid::new_v4();

        assert!(project_workspace_is_verified(Some(expected), expected));
        assert!(!project_workspace_is_verified(None, expected));
        assert!(!project_workspace_is_verified(
            Some(Uuid::new_v4()),
            expected
        ));
    }

    #[test]
    fn workspace_evidence_separates_contradiction_from_silence() {
        let expected = Uuid::new_v4();

        assert_eq!(
            classify_project_workspace(Some(expected), expected),
            WorkspaceOwnershipEvidence::Attested
        );
        assert_eq!(
            classify_project_workspace(Some(Uuid::new_v4()), expected),
            WorkspaceOwnershipEvidence::Contradicted
        );
        // The case that made setup unsatisfiable against an API that never
        // serialized workspace_id: silence, not disproof.
        assert_eq!(
            classify_project_workspace(None, expected),
            WorkspaceOwnershipEvidence::Unattested
        );
    }

    /// Minimal stub API. Replies with `responses` in order, repeating the last
    /// entry, and counts how many requests actually arrived.
    async fn spawn_stub_api(responses: Vec<(u16, String)>) -> (String, Arc<AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local listener");
        let addr = listener.local_addr().expect("listener addr");
        let hits = Arc::new(AtomicUsize::new(0));
        let server_hits = Arc::clone(&hits);

        tokio::spawn(async move {
            let mut index = 0usize;
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = vec![0u8; 8192];
                let _ = socket.read(&mut buf).await;
                server_hits.fetch_add(1, Ordering::SeqCst);

                let (status, body) = responses
                    .get(index)
                    .or_else(|| responses.last())
                    .cloned()
                    .unwrap_or((200, "{}".to_string()));
                index += 1;

                let reason = if status == 200 { "OK" } else { "Error" };
                let response = format!(
                    "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    status,
                    reason,
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });

        (format!("http://{}", addr), hits)
    }

    fn project_listing(ids: &[Uuid]) -> String {
        let items: Vec<serde_json::Value> = ids
            .iter()
            .map(|id| serde_json::json!({ "id": id, "name": "listed-project" }))
            .collect();
        serde_json::json!({
            "items": items,
            "page": 1,
            "per_page": 100,
            "total": items.len(),
            "has_next": false,
            "has_prev": false,
        })
        .to_string()
    }

    fn stub_client(api_url: String) -> ContextStreamClient {
        ContextStreamClient::new(Config {
            api_url,
            api_key: Some("test-key".to_string()),
            ..Config::default()
        })
    }

    #[tokio::test]
    async fn attested_ownership_is_accepted_without_a_listing_call() {
        let workspace = Uuid::new_v4();
        let project = Uuid::new_v4();
        let (api_url, hits) = spawn_stub_api(vec![(500, "{}".to_string())]).await;

        require_project_workspace_ownership(
            &stub_client(api_url),
            project,
            Some(workspace),
            workspace,
        )
        .await
        .expect("matching workspace is accepted");

        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "the fast path must not spend a request"
        );
    }

    #[tokio::test]
    async fn contradicted_ownership_is_refused_without_a_second_opinion() {
        let expected = Uuid::new_v4();
        let project = Uuid::new_v4();
        // Would confirm membership if consulted — it must not be.
        let (api_url, hits) = spawn_stub_api(vec![(200, project_listing(&[project]))]).await;

        let error = require_project_workspace_ownership(
            &stub_client(api_url),
            project,
            Some(Uuid::new_v4()),
            expected,
        )
        .await
        .expect_err("a different owner is a hard refusal");

        assert!(error.to_string().contains("belongs to workspace"));
        assert_eq!(
            hits.load(Ordering::SeqCst),
            0,
            "a stated different owner must never be overridden by the listing"
        );
    }

    #[tokio::test]
    async fn unattested_ownership_is_confirmed_from_the_workspace_listing() {
        let workspace = Uuid::new_v4();
        let project = Uuid::new_v4();
        let (api_url, hits) = spawn_stub_api(vec![(
            200,
            project_listing(&[Uuid::new_v4(), project, Uuid::new_v4()]),
        )])
        .await;

        require_project_workspace_ownership(&stub_client(api_url), project, None, workspace)
            .await
            .expect("membership in the workspace listing proves ownership");

        assert!(
            hits.load(Ordering::SeqCst) >= 1,
            "the listing was consulted"
        );
    }

    #[tokio::test]
    async fn unattested_ownership_is_refused_when_absent_from_the_listing() {
        let workspace = Uuid::new_v4();
        let project = Uuid::new_v4();
        let (api_url, _hits) = spawn_stub_api(vec![
            (200, project_listing(&[Uuid::new_v4()])),
            (200, project_listing(&[])),
        ])
        .await;

        let error =
            require_project_workspace_ownership(&stub_client(api_url), project, None, workspace)
                .await
                .expect_err("a project outside the workspace is refused");

        assert!(error.to_string().contains("is not listed in workspace"));
    }

    #[tokio::test]
    async fn unattested_ownership_fails_closed_when_the_listing_errors() {
        let workspace = Uuid::new_v4();
        let project = Uuid::new_v4();
        let (api_url, _hits) = spawn_stub_api(vec![(500, "{}".to_string())]).await;

        let error =
            require_project_workspace_ownership(&stub_client(api_url), project, None, workspace)
                .await
                .expect_err("an unreachable API must never be read as permission");

        assert!(error.to_string().contains("Could not confirm"));
    }

    #[tokio::test]
    async fn project_selection_reads_past_a_server_clamped_page() {
        let workspace_id = Uuid::new_v4();
        let target_project_id = Uuid::new_v4();
        let first_page = (0..100).map(|_| Uuid::new_v4()).collect::<Vec<_>>();
        let (api_url, hits) = spawn_stub_api(vec![
            (200, project_listing(&first_page)),
            (200, project_listing(&[target_project_id])),
            (200, project_listing(&[])),
        ])
        .await;

        let temp = tempfile::tempdir().expect("tempdir");
        let checkout = temp.path().join("listed-project");
        let config_dir = checkout.join(".contextstream");
        std::fs::create_dir_all(&config_dir).expect("create checkout config dir");
        std::fs::write(
            config_dir.join("config.json"),
            serde_json::json!({
                "workspace_id": workspace_id,
                "workspace_name": "Engineering",
                "project_id": target_project_id,
                "project_name": "listed-project",
            })
            .to_string(),
        )
        .expect("write checkout binding");

        let workspace = WorkspaceInfo {
            id: workspace_id.to_string(),
            name: "Engineering".to_string(),
        };
        let selected = select_project_for_current_directory(
            &stub_client(api_url),
            &checkout,
            Some(&workspace),
            false,
            false,
            false,
        )
        .await
        .expect("project selection should page through the full workspace")
        .expect("the linked project on the second API page should be selected");

        assert_eq!(selected.id, target_project_id.to_string());
        assert_eq!(selected.name, "listed-project");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            3,
            "selection must continue until the API returns an empty page"
        );
    }

    #[test]
    fn linked_project_binding_must_match_configured_or_folder_name() {
        let temp = tempfile::tempdir().expect("tempdir");
        let checkout = temp.path().join("mcp");
        std::fs::create_dir_all(&checkout).expect("checkout dir");
        let checkout_root = canonical_checkout_root(&checkout);

        assert!(linked_project_name_matches_checkout(
            "mcp",
            Some("mcp"),
            None,
            &checkout,
        ));
        assert!(linked_project_name_matches_checkout(
            "MCP",
            Some("old-local-name"),
            None,
            &checkout,
        ));
        assert!(linked_project_name_matches_checkout(
            "custom-project",
            Some("custom-project"),
            Some(&checkout_root),
            &checkout,
        ));
        assert!(!linked_project_name_matches_checkout(
            "streampilot",
            Some("streampilot"),
            None,
            &checkout,
        ));
        assert!(!linked_project_name_matches_checkout(
            "custom-project",
            Some("custom-project"),
            Some(temp.path().to_string_lossy().as_ref()),
            &checkout,
        ));
    }

    #[test]
    fn sanitize_index_error_strips_internal_infrastructure() {
        // Regression guard for the leak where an indexing failure
        // surfaced a backend service address and transport type names
        // directly to the user. Users should see a short actionable
        // message while diagnostic detail remains in protected logs.
        let raw = "Error in the response: Internal error Failed to connect \
                   to http://203.0.113.42:6334/: tonic::transport::Error(Transport, \
                   ConnectError(ConnectError(\"tcp connect error\", 203.0.113.42:6334, \
                   Custom { kind: TimedOut, error: Elapsed(()) }))) MetadataMap { headers: {} }";
        let cleaned = sanitize_index_error(raw);

        // Internal infra MUST NOT leak.
        assert!(
            !cleaned.contains("203.0.113.42"),
            "sanitize_index_error must not leak internal IPs: {}",
            cleaned
        );
        assert!(
            !cleaned.contains(":6334"),
            "sanitize_index_error must not leak internal ports: {}",
            cleaned
        );
        assert!(
            !cleaned.to_lowercase().contains("tonic"),
            "sanitize_index_error must not leak Rust transport type names: {}",
            cleaned
        );
        assert!(
            !cleaned.to_lowercase().contains("metadatamap"),
            "sanitize_index_error must not leak gRPC metadata noise: {}",
            cleaned
        );
        // Should produce a user-actionable message.
        assert!(
            cleaned.to_lowercase().contains("temporarily unreachable")
                || cleaned.to_lowercase().contains("retry"),
            "sanitize_index_error should give the user something to do, got: {}",
            cleaned
        );
        assert!(cleaned.contains("project(action=\"index\")"));
        assert!(cleaned.contains("hosted MCP"));
        assert!(!cleaned.contains("ingest_local"));
    }

    #[test]
    fn sanitize_index_error_handles_504_timeout_path() {
        let raw = "HTTP error (504): Server-side indexing stopped reporting progress \
                   after 45s (last phase: indexing). The upload completed, but the \
                   server did not finish the ingest jobs. Try background indexing or \
                   retry later.";
        let cleaned = sanitize_index_error(raw);
        assert!(cleaned.to_lowercase().contains("temporarily unreachable"));
        assert!(cleaned.contains("project(action=\"index\")"));
        assert!(!cleaned.contains("ingest_local"));
        // Don't expose the 45s SLO or the "ingest jobs" wording — it's
        // operator-internal detail.
        assert!(!cleaned.contains("45s"));
    }

    #[test]
    fn sanitize_index_error_maps_auth_errors_to_actionable_text() {
        assert!(sanitize_index_error("HTTP error (401): Unauthorized").contains("re-authenticate"));
        assert!(sanitize_index_error("HTTP error (403): Forbidden")
            .to_lowercase()
            .contains("permission denied"));
        assert!(sanitize_index_error("HTTP error (429): Too Many Requests")
            .to_lowercase()
            .contains("rate-limited"));
    }

    #[test]
    fn setup_ingest_progress_uses_completed_fallback_counts() {
        let progress = parse_setup_ingest_job_progress(
            Uuid::nil(),
            &serde_json::json!({
                "status": "completed"
            }),
            4,
        );

        assert_eq!(progress.status, "completed");
        assert_eq!(progress.phase, "completed");
        assert_eq!(progress.files_total, 4);
        assert_eq!(progress.files_processed, 4);
    }

    #[test]
    fn setup_ingest_progress_does_not_fake_claimed_progress() {
        let progress = parse_setup_ingest_job_progress(
            Uuid::nil(),
            &serde_json::json!({
                "status": "claimed"
            }),
            4,
        );

        assert_eq!(progress.status, "claimed");
        assert_eq!(progress.phase, "claimed");
        assert_eq!(progress.files_total, 4);
        assert_eq!(progress.files_processed, 0);
    }

    #[test]
    fn format_index_completion_message_mentions_compacted_files() {
        let message = format_index_completion_message(&serde_json::json!({
            "files_indexed": 42,
            "files_compacted": 2
        }));

        assert!(message.contains("42 files indexed"));
        assert!(message.contains("2 large files compacted for upload"));
    }

    #[test]
    fn format_index_completion_message_mentions_deferred_files() {
        let message = format_index_completion_message(&serde_json::json!({
            "files_indexed": 40,
            "files_deferred": 1
        }));

        assert!(message.contains("40 files indexed"));
        assert!(message.contains("1 oversized file deferred"));
    }

    #[test]
    fn setup_index_choices_default_to_background() {
        let choices = setup_index_choices();

        assert_eq!(choices.len(), 3);
        // Background is the default (first) choice so setup finishes
        // immediately; blocking foreground indexing is explicit opt-in.
        assert!(choices[0].contains("background"));
        assert!(choices[0].contains("recommended"));
        assert!(choices[1].starts_with("Now"));
        assert!(choices[2].starts_with("Skip"));
    }

    #[test]
    fn index_worker_args_select_the_detached_worker_mode() {
        let args = super::index_worker_args(Path::new("/work/app"), false, false);
        let args: Vec<_> = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            ["index", "--path", "/work/app", "--background-worker"]
        );

        let args = super::index_worker_args(Path::new("/work/app"), true, true);
        let args: Vec<_> = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "ingest",
                "--path",
                "/work/app",
                "--background-worker",
                "--include-media",
                "--force"
            ]
        );
    }

    #[test]
    fn setup_project_content_ignores_generated_setup_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("AGENTS.md"), "ContextStream rules").unwrap();
        std::fs::write(temp.path().join("CLAUDE.md"), "ContextStream rules").unwrap();
        std::fs::create_dir_all(temp.path().join(".contextstream")).unwrap();
        std::fs::write(temp.path().join(".contextstream").join("config.json"), "{}").unwrap();
        std::fs::create_dir_all(temp.path().join(".cursor").join("rules")).unwrap();
        std::fs::write(
            temp.path()
                .join(".cursor")
                .join("rules")
                .join("contextstream.mdc"),
            "ContextStream rules",
        )
        .unwrap();
        std::fs::create_dir_all(temp.path().join(".windsurf").join("rules")).unwrap();
        std::fs::write(
            temp.path()
                .join(".windsurf")
                .join("rules")
                .join("contextstream.md"),
            "ContextStream rules",
        )
        .unwrap();

        assert!(!setup_path_has_project_content_files(temp.path(), true));
    }

    #[test]
    fn setup_project_content_detects_real_project_files() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(temp.path().join("src")).unwrap();
        std::fs::write(temp.path().join("src").join("main.rs"), "fn main() {}").unwrap();

        assert!(setup_path_has_project_content_files(temp.path(), false));
    }

    #[test]
    fn setup_project_content_respects_media_selection() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("logo.png"), "fake image").unwrap();

        assert!(!setup_path_has_project_content_files(temp.path(), false));
        assert!(setup_path_has_project_content_files(temp.path(), true));
    }

    #[test]
    fn write_setup_transport_marker_persists_remote_choice() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        std::env::set_var("HOME", temp.path());

        write_setup_transport_marker(SetupTransportPreference::HostedRemote)
            .expect("write transport marker");

        let marker = setup_transport_marker_path();
        let content = std::fs::read_to_string(&marker).expect("read transport marker");
        assert_eq!(content, "remote\n");

        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn write_setup_transport_marker_persists_local_choice() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        std::env::set_var("HOME", temp.path());

        write_setup_transport_marker(SetupTransportPreference::LocalBinary)
            .expect("write transport marker");

        let marker = setup_transport_marker_path();
        let content = std::fs::read_to_string(&marker).expect("read transport marker");
        assert_eq!(content, "local\n");

        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn read_setup_transport_marker_round_trips_local_choice() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        std::env::set_var("HOME", temp.path());

        write_setup_transport_marker(SetupTransportPreference::LocalBinary)
            .expect("write transport marker");
        assert_eq!(
            read_setup_transport_marker(),
            Some(SetupTransportPreference::LocalBinary)
        );

        write_setup_transport_marker(SetupTransportPreference::HostedRemote)
            .expect("rewrite transport marker");
        assert_eq!(
            read_setup_transport_marker(),
            Some(SetupTransportPreference::HostedRemote)
        );

        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn local_mcp_allowed_accepts_env_or_marker() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        let previous_env = std::env::var_os(ENV_ALLOW_LOCAL_MCP);
        std::env::set_var("HOME", temp.path());
        std::env::remove_var(ENV_ALLOW_LOCAL_MCP);

        // Baseline: no marker, no env → denied.
        assert!(!local_mcp_allowed());

        // Env opt-in alone → allowed.
        std::env::set_var(ENV_ALLOW_LOCAL_MCP, "1");
        assert!(local_mcp_allowed());
        std::env::remove_var(ENV_ALLOW_LOCAL_MCP);

        // Marker opt-in alone → allowed.
        write_setup_transport_marker(SetupTransportPreference::LocalBinary)
            .expect("write transport marker");
        assert!(local_mcp_allowed());

        // Marker says remote, no env → denied.
        write_setup_transport_marker(SetupTransportPreference::HostedRemote)
            .expect("rewrite transport marker");
        assert!(!local_mcp_allowed());

        // Restore.
        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        if let Some(value) = previous_env {
            std::env::set_var(ENV_ALLOW_LOCAL_MCP, value);
        } else {
            std::env::remove_var(ENV_ALLOW_LOCAL_MCP);
        }
    }

    #[test]
    fn read_setup_transport_marker_returns_none_when_missing() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        std::env::set_var("HOME", temp.path());

        // No marker has been written to the fresh HOME.
        assert!(read_setup_transport_marker().is_none());

        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn strict_transport_marker_read_rejects_invalid_content() {
        let _guard = env_test_mutex().lock().unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        std::env::set_var("HOME", temp.path());
        let marker = setup_transport_marker_path();
        std::fs::create_dir_all(marker.parent().expect("marker parent"))
            .expect("create marker dir");
        std::fs::write(&marker, "surprise-mode").expect("seed invalid marker");

        let error = read_setup_transport_marker_result()
            .expect_err("refresh callers must reject an invalid marker");
        assert!(error
            .to_string()
            .contains("Refusing to rewrite editor configs"));
        assert_eq!(
            std::fs::read_to_string(&marker).expect("marker remains"),
            "surprise-mode"
        );

        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn parse_setup_transport_preference_accepts_remote_aliases() {
        assert_eq!(
            parse_setup_transport_preference("remote"),
            Some(SetupTransportPreference::HostedRemote)
        );
        assert_eq!(
            parse_setup_transport_preference("HOSTED"),
            Some(SetupTransportPreference::HostedRemote)
        );
        assert_eq!(
            parse_setup_transport_preference("hosted-remote"),
            Some(SetupTransportPreference::HostedRemote)
        );
    }

    #[test]
    fn parse_setup_transport_preference_accepts_local_aliases() {
        assert_eq!(
            parse_setup_transport_preference("local"),
            Some(SetupTransportPreference::LocalBinary)
        );
        assert_eq!(
            parse_setup_transport_preference("binary"),
            Some(SetupTransportPreference::LocalBinary)
        );
        assert_eq!(
            parse_setup_transport_preference("local-binary"),
            Some(SetupTransportPreference::LocalBinary)
        );
    }

    #[test]
    fn local_mcp_override_requires_explicit_allow_value() {
        assert!(local_mcp_override_allowed_from_env_value(Some("1")));
        assert!(local_mcp_override_allowed_from_env_value(Some("true")));
        assert!(local_mcp_override_allowed_from_env_value(Some("recovery")));
        assert!(!local_mcp_override_allowed_from_env_value(None));
        assert!(!local_mcp_override_allowed_from_env_value(Some("")));
        assert!(!local_mcp_override_allowed_from_env_value(Some("local")));
    }
}

/// Mask an API key for display.
fn mask_api_key(key: &str) -> String {
    if key.len() <= 8 {
        "*".repeat(key.len())
    } else {
        format!("{}...{}", &key[..4], &key[key.len() - 4..])
    }
}

/// Workspace info for setup.
#[derive(Debug, Clone)]
pub struct WorkspaceInfo {
    pub id: String,
    pub name: String,
}

/// Project info for setup/configuration flows.
#[derive(Debug, Clone)]
pub struct ProjectInfo {
    pub id: String,
    pub name: String,
}

impl From<&mcp_types::api::Project> for ProjectInfo {
    fn from(project: &mcp_types::api::Project) -> Self {
        Self {
            id: project.id.to_string(),
            name: project.name.clone(),
        }
    }
}

/// Send a desktop notification (cross-platform).
pub fn send_desktop_notification(title: &str, body: &str) {
    use std::process::Command;

    #[cfg(target_os = "linux")]
    {
        // Try notify-send (most Linux distros)
        let _ = Command::new("notify-send")
            .args([title, body, "--app-name=ContextStream"])
            .spawn();
    }

    #[cfg(target_os = "macos")]
    {
        // Use osascript for macOS notifications
        let script = format!(
            r#"display notification "{}" with title "{}""#,
            body.replace('"', r#"\""#),
            title.replace('"', r#"\""#)
        );
        let _ = Command::new("osascript").args(["-e", &script]).spawn();
    }

    #[cfg(target_os = "windows")]
    {
        // Use PowerShell for Windows notifications
        let script = format!(
            r#"[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null; $template = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02); $textNodes = $template.GetElementsByTagName('text'); $textNodes.Item(0).AppendChild($template.CreateTextNode('{}')) | Out-Null; $textNodes.Item(1).AppendChild($template.CreateTextNode('{}')) | Out-Null; $toast = [Windows.UI.Notifications.ToastNotification]::new($template); [Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('ContextStream').Show($toast)"#,
            title.replace('\'', "''"),
            body.replace('\'', "''")
        );
        let _ = Command::new("powershell")
            .args(["-Command", &script])
            .spawn();
    }
}
