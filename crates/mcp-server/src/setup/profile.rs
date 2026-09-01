//! Profile-driven setup (`setup --profile <token>`): the context-engine
//! installer path.
//!
//! The web questionnaire ("build your context engine") mints a one-time
//! `cbst_` setup token per device. This module redeems it — receiving a
//! freshly minted API key plus the user's chosen editors / workspace — and
//! then drives the same building blocks as `setup --yes`, configuring
//! exactly what the user picked. Everything the user sees is this branded
//! terminal flow; the shell shim that downloads the binary is invisible
//! plumbing.
//!
//! Idempotency rules on machines that already ran a setup:
//! - Valid saved credentials for the SAME user are kept; the key minted at
//!   redemption is revoked so the dashboard ledger stays clean.
//! - Credentials for a DIFFERENT user are never silently overwritten.
//! - Only profile-chosen editors are written; other editors and foreign
//!   servers in shared MCP config files are left untouched (the config
//!   writer merges, the rules writer replaces only its marker block).

use anyhow::{anyhow, Context, Result};
use console::style;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mcp_client::{json::parse_value_without_duplicate_keys, ContextStreamClient};
use mcp_types::{Config, Error as McpError};

use super::editors::{self, Editor};
use super::{
    doctor, git_hooks, mcp_config, normalize_api_url, prompt_setup_transport_preference,
    read_saved_credentials, safe_edit, select_project_for_current_directory,
    spawn_background_index, spawn_warmup, write_saved_credentials, write_setup_transport_marker,
    SetupTransportPreference, WorkspaceInfo, CHECK, CROSS,
};

const REDEEM_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// Payload types (mirror the API's /setup/redeem response)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProfileApiKey {
    pub id: String,
    pub secret: String,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ProfileDetails {
    #[serde(default)]
    pub editors: Vec<String>,
    #[serde(default)]
    pub rules_mode: Option<String>,
    #[serde(default)]
    pub workspace_id: Option<String>,
    #[serde(default)]
    pub workspace_name: Option<String>,
    #[serde(default)]
    pub kit_version: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SetupProfilePayload {
    pub device_id: String,
    pub user_id: String,
    pub email: String,
    pub api_key: ProfileApiKey,
    pub api_url: String,
    #[serde(default)]
    pub profile: ProfileDetails,
}

#[derive(Debug, Deserialize)]
#[serde(bound(deserialize = "T: serde::Deserialize<'de>"))]
struct ApiEnvelope<T> {
    #[serde(default)]
    data: Option<T>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Run the context-engine installer flow. `token` and `profile_file` are
/// mutually exclusive sources for the setup profile (`--profile` /
/// `--profile-file`); the file form exists for tests and air-gapped runs.
pub async fn run_setup_with_profile(
    token: Option<String>,
    profile_file: Option<PathBuf>,
    editor_override: Option<&[Editor]>,
    explicit_project_path: Option<&Path>,
    account_only: bool,
) -> Result<()> {
    let started = Instant::now();
    print_engine_banner();

    let payload = match profile_file {
        Some(path) => load_profile_file(&path)?,
        None => {
            let token = token
                .ok_or_else(|| anyhow!("setup --profile requires a token or --profile-file"))?;
            redeem_token(token.trim()).await?
        }
    };

    println!(
        "{}Signed in as {}",
        CHECK,
        style(&payload.email).cyan().bold()
    );

    // ------------------------------------------------------------------
    // Credentials: keep a valid same-user key, never clobber another user's.
    // ------------------------------------------------------------------
    let api_url = normalize_api_url(&payload.api_url);
    let (active_key, kept_existing) = resolve_credentials(&payload, &api_url).await?;

    let config = Config {
        api_key: Some(active_key.clone()),
        ..Default::default()
    };
    let client = ContextStreamClient::new(config);

    // ------------------------------------------------------------------
    // Editors: exactly what the profile chose, present on this machine.
    // ------------------------------------------------------------------
    let (profile_editors, editors_skipped, unknown_slugs) =
        partition_profile_editors(&payload.profile.editors);
    let editors_to_configure = editor_override
        .map(ToOwned::to_owned)
        .unwrap_or(profile_editors);

    if editor_override.is_none() {
        for slug in &unknown_slugs {
            println!(
                "{}Skipping unknown editor id '{}' from your profile (newer questionnaire than this binary?)",
                super::info_label(),
                slug
            );
        }
        if !editors_skipped.is_empty() {
            println!(
                "{}Not installed here (skipping): {}",
                super::info_label(),
                editors_skipped
                    .iter()
                    .map(|e| e.display_name())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }

    if editors_to_configure.is_empty() {
        println!(
            "{}None of your chosen editors are installed on this machine — no coding harness will be configured.",
            super::info_label()
        );
    } else {
        println!(
            "{}Wiring {} editor(s){}: {}",
            CHECK,
            editors_to_configure.len(),
            if editor_override.is_some() {
                " requested on the command line"
            } else {
                ""
            },
            editors_to_configure
                .iter()
                .map(|e| e.display_name())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // ------------------------------------------------------------------
    // Workspace + project context.
    // ------------------------------------------------------------------
    let cwd = std::env::current_dir()?;
    let project_path =
        super::resolve_setup_project_path(&cwd, explicit_project_path, account_only)?;
    let workspace_lookup_path = project_path.as_deref().unwrap_or(cwd.as_path());
    let workspace =
        resolve_profile_workspace(&client, &payload.profile, workspace_lookup_path).await;
    let workspace = workspace?;
    if let Some(ref ws) = workspace {
        println!("{}Workspace: {}", CHECK, style(&ws.name).cyan());
    }

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
            println!(
                "{}No safe project folder was selected. Editor-global setup will continue, but project binding and indexing will remain incomplete.",
                super::info_label()
            );
        }
        None
    };
    let configured_project_path = selected_project.as_ref().and(project_path.as_deref());

    let transport_preference = prompt_setup_transport_preference(&editors_to_configure)?;
    let preauth_remote_configs =
        matches!(transport_preference, SetupTransportPreference::HostedRemote)
            && editors_to_configure
                .iter()
                .any(mcp_config::editor_supports_remote_mcp);

    super::persist_setup_editor_selection(&editors_to_configure)?;
    if !editors_to_configure.is_empty() {
        // Keep the repair source of truth in sync even when a later editor
        // fails to configure.
        write_setup_transport_marker(transport_preference)?;
    }
    for editor in &editors_to_configure {
        super::configure_editor_with_workspace(
            &client,
            editor,
            &active_key,
            workspace.as_ref(),
            selected_project.as_ref().map(|p| p.id.as_str()),
            configured_project_path,
            transport_preference,
            preauth_remote_configs,
        )
        .await
        .with_context(|| format!("configuring {}", editor.display_name()))?;
    }
    if !editors_to_configure.is_empty() {
        // Heal deleted local-binary paths so a re-run leaves a working machine.
        mcp_config::repair_deleted_binary_path_configs(&editors_to_configure, None)
            .context("repairing stale local editor config paths")?;
    }

    // Folder binding + git hooks + index warmup mirror `setup --yes`.
    let binding_established = if let Some(configured_project_path) = configured_project_path {
        if let Some(ref workspace) = workspace {
            super::establish_validated_setup_binding(
                &client,
                configured_project_path,
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
                    println!("{}Could not install git hooks: {}", super::info_label(), e);
                }
            }
        }
    }

    let (index_started, awaiting_first_files) = if let Some(configured_project_path) =
        configured_project_path.filter(|_| binding_established)
    {
        if !super::setup_path_has_project_content_files(configured_project_path, false) {
            spawn_warmup(&client, workspace.as_ref().map(|w| &w.id));
            super::print_empty_project_ready();
            (false, true)
        } else {
            let bg_project_id = selected_project
                .as_ref()
                .and_then(|p| uuid::Uuid::parse_str(&p.id).ok());
            spawn_background_index(
                client.clone(),
                configured_project_path.to_path_buf(),
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

    // ------------------------------------------------------------------
    // Verify, report back, celebrate.
    // ------------------------------------------------------------------
    let report = doctor::build_report(configured_project_path, &editors_to_configure).await;
    doctor::print_setup_health_report(&report);
    let outcome = super::setup_completion_evidence(
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

    report_completion(
        &client,
        &payload,
        &editors_to_configure,
        &editors_skipped,
        &report,
        &outcome,
        started,
    )
    .await;

    print_engine_outcome(
        &payload,
        &editors_to_configure,
        workspace.as_ref(),
        kept_existing,
        project_path.as_deref(),
        &outcome,
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Redemption
// ---------------------------------------------------------------------------

fn load_profile_file(path: &Path) -> Result<SetupProfilePayload> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("reading profile file {}", path.display()))?;
    let value = parse_value_without_duplicate_keys(&content)
        .with_context(|| format!("parsing profile file {}", path.display()))?;
    serde_json::from_value(value)
        .with_context(|| format!("parsing profile file {}", path.display()))
}

fn redeem_api_url() -> String {
    if let Ok(url) = std::env::var("CONTEXTSTREAM_API_URL") {
        if !url.trim().is_empty() {
            return normalize_api_url(&url);
        }
    }
    if let Ok(creds) = read_saved_credentials() {
        if let Some(url) = creds.api_url {
            if !url.trim().is_empty() {
                return normalize_api_url(&url);
            }
        }
    }
    "https://api.contextstream.io".to_string()
}

fn detect_hostname() -> Option<String> {
    for var in ["HOSTNAME", "COMPUTERNAME"] {
        if let Ok(value) = std::env::var(var) {
            let trimmed = value.trim().to_string();
            if !trimmed.is_empty() {
                return Some(trimmed);
            }
        }
    }
    if let Ok(content) = std::fs::read_to_string("/etc/hostname") {
        let trimmed = content.trim().to_string();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

async fn redeem_token(token: &str) -> Result<SetupProfilePayload> {
    let api_url = redeem_api_url();
    let endpoint = format!("{}/api/v1/setup/redeem", api_url.trim_end_matches('/'));

    println!(
        "{}Unlocking your context engine profile...",
        super::info_label()
    );

    let body = serde_json::json!({
        "token": token,
        "hostname": detect_hostname(),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "mcp_version": env!("CARGO_PKG_VERSION"),
        "existing_credentials": read_saved_credentials()?.api_key.is_some(),
    });

    let client = reqwest::Client::builder()
        .timeout(REDEEM_TIMEOUT)
        .build()
        .context("building HTTP client")?;

    let response = client
        .post(&endpoint)
        .json(&body)
        .send()
        .await
        .context("reaching the ContextStream API — check your connection and re-run")?;

    let status = response.status();
    if status.is_success() {
        let raw = response
            .text()
            .await
            .context("reading setup profile response")?;
        let value =
            parse_value_without_duplicate_keys(&raw).context("parsing setup profile response")?;
        let envelope: ApiEnvelope<SetupProfilePayload> =
            serde_json::from_value(value).context("parsing setup profile response")?;
        return envelope
            .data
            .ok_or_else(|| anyhow!("Setup profile response had no data"));
    }

    match status.as_u16() {
        410 => {
            println!();
            println!(
                "{}{}",
                CROSS,
                style("This setup link has expired (links stay valid for 72 hours).").yellow()
            );
            println!("   Falling back to browser sign-in — everything else continues as normal.");
            println!();
            fallback_authenticated_payload(&api_url).await
        }
        409 => {
            println!();
            println!(
                "{}{}",
                CROSS,
                style("This setup link was already used on a machine.").yellow()
            );
            println!("   Falling back to browser sign-in — generate a fresh link for additional devices.");
            println!();
            fallback_authenticated_payload(&api_url).await
        }
        402 => Err(anyhow!(
            "Your ContextStream subscription is not active. Reactivate your plan at https://app.contextstream.io/account, then run the setup command again."
        )),
        429 => Err(anyhow!(
            "Too many setup attempts from this network — wait a minute and re-run the command."
        )),
        _ => {
            let detail = response.text().await.unwrap_or_default();
            Err(anyhow!(
                "Setup link was not accepted ({}). Generate a fresh link from the dashboard and try again. {}",
                status,
                truncate_detail(&detail)
            ))
        }
    }
}

/// Dead-token fallback: browser device-login (the existing flow), then pull
/// the questionnaire profile with the authenticated key so the user still
/// gets their chosen editors — nobody gets stranded on an expired link.
async fn fallback_authenticated_payload(api_url: &str) -> Result<SetupProfilePayload> {
    let api_key = super::ensure_authenticated_api_key().await?;

    let config = Config {
        api_key: Some(api_key.clone()),
        ..Default::default()
    };
    let client = ContextStreamClient::new(config);
    let user = client.me().await.context("validating browser sign-in")?;

    #[derive(Deserialize, Default)]
    struct StoredProfile {
        #[serde(default)]
        workspace_id: Option<String>,
        #[serde(default)]
        kit_version: Option<String>,
        #[serde(default)]
        kit_manifest: serde_json::Value,
    }

    let stored: Option<StoredProfile> = client
        .get::<ApiEnvelope<StoredProfile>>("/onboarding/profile")
        .await
        .context("loading the saved onboarding profile")?
        .data;

    let mut profile = ProfileDetails::default();
    if let Some(stored) = stored {
        profile.workspace_id = stored.workspace_id;
        profile.kit_version = stored.kit_version;
        if let Some(manifest) = stored.kit_manifest.as_object() {
            if let Some(editors) = manifest
                .get("recommended_editors")
                .and_then(|v| v.as_array())
            {
                profile.editors = editors
                    .iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect();
            }
            if let Some(mode) = manifest.get("rules_mode").and_then(|v| v.as_str()) {
                profile.rules_mode = Some(mode.to_string());
            }
        }
    }

    Ok(SetupProfilePayload {
        device_id: String::new(), // no device row — completion reporting is skipped
        user_id: user.id.to_string(),
        email: user.email,
        api_key: ProfileApiKey {
            id: String::new(),
            secret: api_key,
        },
        api_url: api_url.to_string(),
        profile,
    })
}

fn truncate_detail(detail: &str) -> String {
    let trimmed = detail.trim();
    if trimmed.len() > 200 {
        format!("{}…", &trimmed[..200])
    } else {
        trimmed.to_string()
    }
}

// ---------------------------------------------------------------------------
// Credentials
// ---------------------------------------------------------------------------

/// Decide which API key this machine ends up with. Returns the active key
/// and whether the pre-existing credentials were kept.
async fn resolve_credentials(
    payload: &SetupProfilePayload,
    api_url: &str,
) -> Result<(String, bool)> {
    let minted = payload.api_key.secret.clone();
    if minted.trim().is_empty() {
        return Err(anyhow!("Setup profile did not contain an API key"));
    }

    let existing = read_saved_credentials()?.api_key;
    let Some(existing_key) = existing else {
        write_saved_credentials(&minted, Some(api_url))?;
        println!("{}Credentials installed for this machine", CHECK);
        return Ok((minted, false));
    };

    // Validate the existing key and compare account identity.
    let config = Config {
        api_key: Some(existing_key.clone()),
        ..Default::default()
    };
    let client = ContextStreamClient::new(config);
    let existing_user = tokio::time::timeout(Duration::from_secs(10), client.me()).await;

    match existing_user {
        Ok(Ok(user)) if user.id.to_string() == payload.user_id => {
            // Same account: keep the machine's key, retire the minted one so
            // the dashboard ledger doesn't accumulate one key per re-run.
            println!(
                "{}This machine is already connected to {} — keeping its existing credentials",
                CHECK,
                style(&user.email).cyan()
            );
            revoke_minted_key(payload, &existing_key, api_url).await;
            Ok((existing_key, true))
        }
        Ok(Ok(user)) => {
            // Different account: require an explicit decision, never silent.
            if std::io::IsTerminal::is_terminal(&std::io::stdin()) {
                println!(
                    "{}This machine is currently signed in as {} but the setup link belongs to {}.",
                    super::info_label(),
                    style(&user.email).yellow(),
                    style(&payload.email).cyan()
                );
                let replace = super::prompts::confirm(
                    &format!("Switch this machine to {}?", payload.email),
                    true,
                )?;
                if !replace {
                    return Err(anyhow!(
                        "Keeping existing credentials — re-run with a setup link for {} if that was unintended.",
                        user.email
                    ));
                }
            } else {
                return Err(anyhow!(
                    "This machine already has credentials for {} but the setup link belongs to {}. \
                     Re-run in an interactive terminal to switch accounts.",
                    user.email,
                    payload.email
                ));
            }
            write_saved_credentials(&minted, Some(api_url))?;
            println!("{}Credentials replaced for this machine", CHECK);
            Ok((minted, false))
        }
        Ok(Err(McpError::Http {
            status: 401 | 403, ..
        })) => {
            // The API definitively rejected the old key. The user explicitly
            // invoked this one-time setup profile, so replacing it with the
            // freshly minted key is the requested recovery action.
            write_saved_credentials(&minted, Some(api_url))?;
            println!("{}Stale credentials replaced", CHECK);
            Ok((minted, false))
        }
        Ok(Err(error)) => Err(anyhow!(
            "Could not verify the existing credentials ({error}); they were left untouched. Check connectivity and re-run."
        )),
        Err(_) => Err(anyhow!(
            "Timed out while verifying existing credentials; they were left untouched. Check connectivity and re-run."
        )),
    }
}

/// Best-effort revoke of the key minted at redemption when we kept an
/// existing one. Failure is cosmetic (an unused key in the dashboard list),
/// so warn-and-continue.
async fn revoke_minted_key(payload: &SetupProfilePayload, authed_key: &str, api_url: &str) {
    if safe_edit::is_dry_run() || payload.api_key.id.is_empty() {
        return;
    }
    let endpoint = format!(
        "{}/api/v1/auth/api-keys/{}",
        api_url.trim_end_matches('/'),
        payload.api_key.id
    );
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(client) => client,
        Err(_) => return,
    };
    let result = client
        .delete(&endpoint)
        .header("Authorization", format!("Bearer {}", authed_key))
        .header("X-API-Key", authed_key)
        .send()
        .await;
    match result {
        Ok(resp) if resp.status().is_success() => {}
        Ok(resp) => tracing::debug!(
            "Could not retire duplicate setup key (status {}): leaving it revocable in the dashboard",
            resp.status()
        ),
        Err(e) => tracing::debug!("Could not retire duplicate setup key: {}", e),
    }
}

// ---------------------------------------------------------------------------
// Editors + workspace
// ---------------------------------------------------------------------------

/// Split profile editor slugs into (installed-and-chosen, chosen-but-missing,
/// unknown-slug) sets. An empty chosen list is authoritative: absence of a
/// selection is never permission to modify every editor detected locally.
fn partition_profile_editors(slugs: &[String]) -> (Vec<Editor>, Vec<Editor>, Vec<String>) {
    if slugs.is_empty() {
        return (Vec::new(), Vec::new(), Vec::new());
    }

    let installed: std::collections::HashSet<Editor> =
        editors::detect_installed_editors().into_iter().collect();

    let mut to_configure = Vec::new();
    let mut skipped = Vec::new();
    let mut unknown = Vec::new();

    for slug in slugs {
        match Editor::from_id(slug) {
            Some(editor) if installed.contains(&editor) => to_configure.push(editor),
            Some(editor) => skipped.push(editor),
            None => unknown.push(slug.clone()),
        }
    }

    (to_configure, skipped, unknown)
}

/// The profile's workspace wins outright — this is what makes the installer
/// deterministic for existing users with many workspaces. Fall back to the
/// `--yes` resolution only when the profile carries none.
async fn resolve_profile_workspace(
    client: &ContextStreamClient,
    profile: &ProfileDetails,
    workspace_lookup_path: &Path,
) -> Result<Option<WorkspaceInfo>> {
    if let Some(ref id) = profile.workspace_id {
        let workspaces = client
            .list_workspaces(None, None)
            .await
            .context("validating the profile workspace")?;
        let matching = workspaces
            .iter()
            .find(|workspace| workspace.id.to_string() == *id)
            .ok_or_else(|| {
                anyhow!(
                    "The setup profile workspace {} is not available to the authenticated account",
                    id
                )
            })?;
        let name = profile
            .workspace_name
            .clone()
            .unwrap_or_else(|| matching.name.clone());
        return Ok(Some(WorkspaceInfo {
            id: id.clone(),
            name,
        }));
    }

    super::resolve_workspace_noninteractive(client, workspace_lookup_path)
        .await
        .context("resolving a workspace for profile setup")
}

// ---------------------------------------------------------------------------
// Completion + presentation
// ---------------------------------------------------------------------------

async fn report_completion(
    client: &ContextStreamClient,
    payload: &SetupProfilePayload,
    configured: &[Editor],
    skipped: &[Editor],
    report: &doctor::DoctorReport,
    outcome: &super::SetupCompletionEvidence,
    started: Instant,
) {
    if safe_edit::is_dry_run() || payload.device_id.is_empty() {
        return; // browser-auth fallback: no device row to complete
    }

    let body = serde_json::json!({
        "editors_configured": configured.iter().map(|e| e.id()).collect::<Vec<_>>(),
        "editors_skipped": skipped.iter().map(|e| e.id()).collect::<Vec<_>>(),
        "doctor": report.completion_summary(),
        "installer_state": outcome.state.as_str(),
        "installer_evidence": outcome,
        "mcp_version": env!("CARGO_PKG_VERSION"),
        "hostname": detect_hostname(),
        "duration_ms": started.elapsed().as_millis() as u64,
    });

    let path = format!("/setup/devices/{}/complete", payload.device_id);
    match client.post::<serde_json::Value, _>(&path, body).await {
        Ok(_) => {
            println!(
                "{}Dashboard updated with installer state '{}'; connection remains pending until an editor handshake is observed.",
                CHECK,
                outcome.state.as_str()
            );
        }
        Err(e) => {
            tracing::debug!("Completion report failed: {}", e);
            println!(
                "{}Setup finished, but the dashboard may take a little longer to notice this device.",
                super::info_label()
            );
        }
    }
}

fn print_engine_banner() {
    println!();
    println!(
        "{}",
        style("╭──────────────────────────────────────────╮").blue()
    );
    println!(
        "{}  {}  {}",
        style("│").blue(),
        style("Building your context engine").bold(),
        style("        │").blue()
    );
    println!(
        "{}",
        style("╰──────────────────────────────────────────╯").blue()
    );
    println!();
}

fn print_engine_outcome(
    payload: &SetupProfilePayload,
    configured: &[Editor],
    workspace: Option<&WorkspaceInfo>,
    kept_existing: bool,
    project_path: Option<&Path>,
    outcome: &super::SetupCompletionEvidence,
) {
    println!();
    match outcome.state {
        super::SetupCompletionState::RestartRequired => println!(
            "{}{}",
            style("✦ ").green(),
            style("Configuration verified — restart your editor to connect.").bold()
        ),
        super::SetupCompletionState::DryRunPreview => println!(
            "{}{}",
            style("◇ ").cyan(),
            style("Setup preview finished — no local files were changed.").bold()
        ),
        super::SetupCompletionState::NoClientConfigured => println!(
            "{}{}",
            style("○ ").yellow(),
            style("Account saved, but no coding harness was configured.").bold()
        ),
        super::SetupCompletionState::RulesOnlyReady => println!(
            "{}{}",
            style("○ ").yellow(),
            style("Rules refreshed; select an MCP-capable harness to connect.").bold()
        ),
        super::SetupCompletionState::RepairRequired => println!(
            "{}{}",
            style("⚠ ").yellow(),
            style("Configuration needs repair before restart.").bold()
        ),
        super::SetupCompletionState::AccountOnly => println!(
            "{}{}",
            style("○ ").yellow(),
            style("Account-only setup saved; no project was linked.").bold()
        ),
        super::SetupCompletionState::ProjectRequired => println!(
            "{}{}",
            style("○ ").yellow(),
            style("Editor setup saved; project setup is incomplete.").bold()
        ),
        super::SetupCompletionState::IndexRequired => println!(
            "{}{}",
            style("○ ").yellow(),
            style("Project linked; indexing still needs to start.").bold()
        ),
    }
    println!();
    println!("   Account   {}", style(&payload.email).cyan());
    if let Some(ws) = workspace {
        println!("   Workspace {}", style(&ws.name).cyan());
    }
    if !configured.is_empty() {
        println!(
            "   Agents    {}",
            configured
                .iter()
                .map(|e| e.display_name())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let teaching_contracts = super::setup_teaching_contracts(configured);
    let teaching_version = teaching_contracts
        .first()
        .map(|contract| contract.teaching_version.as_str())
        .unwrap_or(mcp_types::HARNESS_TEACHING_VERSION);
    debug_assert!(teaching_contracts
        .iter()
        .all(|contract| { contract.teaching_version == mcp_types::HARNESS_TEACHING_VERSION }));
    if !configured.is_empty() {
        println!("   Workflow  {}", style(teaching_version).cyan());
    }
    if outcome.binding_established {
        if let Some(path) = project_path {
            println!("   Project   {}", style(path.display()).cyan());
        }
    }
    if outcome.awaiting_first_files {
        if crate::watch::watch_enabled() {
            println!("   Index     Ready; files sync automatically when added");
        } else {
            println!("   Index     Ready; automatic sync is disabled on this machine");
        }
    }
    if let Some(kit) = payload.profile.kit_version.as_deref() {
        println!("   Starter kit {}", style(kit).dim());
    }
    if kept_existing {
        println!(
            "   {}",
            style("Existing credentials kept — no duplicate keys were left behind.").dim()
        );
    }
    println!();
    let editor_ids = configured
        .iter()
        .map(Editor::id)
        .collect::<Vec<_>>()
        .join(",");
    match outcome.state {
        super::SetupCompletionState::DryRunPreview => {
            println!("   Run the same command without --dry-run to apply it.");
        }
        super::SetupCompletionState::NoClientConfigured => {
            println!("   No coding harness can use ContextStream yet.");
            println!(
                "   Run {}.",
                style(
                    "contextstream-mcp setup --editors <editor-id> --project-path /path/to/project"
                )
                .cyan()
            );
        }
        super::SetupCompletionState::RulesOnlyReady => {
            println!(
                "   The selected harness rules were refreshed, but no selected harness has an MCP transport."
            );
            for editor in configured {
                println!(
                    "   {}: {}",
                    style(editor.display_name()).bold(),
                    editor.activation_reload_instruction()
                );
            }
            println!(
                "   Add an MCP-capable harness with {}.",
                style(
                    "contextstream-mcp setup --editors <editor-id> --project-path /path/to/project"
                )
                .cyan()
            );
        }
        super::SetupCompletionState::RepairRequired => {
            let scope = if outcome.binding_established {
                "all"
            } else {
                "global"
            };
            println!(
                "   Repair with {}.",
                style(format!(
                    "contextstream-mcp doctor --repair --scope {scope} --editors {editor_ids}"
                ))
                .cyan()
            );
        }
        super::SetupCompletionState::AccountOnly => {
            println!("   No project was linked or indexed, as requested.");
            println!(
                "   Finish later with {}.",
                style(format!(
                    "contextstream-mcp setup --project-path /path/to/project --editors {editor_ids}"
                ))
                .cyan()
            );
        }
        super::SetupCompletionState::ProjectRequired => {
            let project = project_path
                .map(|path| format!("{path:?}"))
                .unwrap_or_else(|| "/path/to/project".to_string());
            println!(
                "   Finish project setup with {}.",
                style(format!(
                    "contextstream-mcp setup --project-path {project} --editors {editor_ids}"
                ))
                .cyan()
            );
        }
        super::SetupCompletionState::IndexRequired => {
            println!(
                "   After restarting, ask the editor to run {} for this checkout.",
                style("project(action=\"index\")").cyan()
            );
        }
        super::SetupCompletionState::RestartRequired => {
            println!("   1. Reload each configured harness:");
            for editor in configured
                .iter()
                .filter(|editor| editor.has_mcp_transport())
            {
                println!(
                    "      {}: {}",
                    style(editor.display_name()).bold(),
                    editor.activation_reload_instruction()
                );
            }
            if outcome.awaiting_first_files {
                if crate::watch::watch_enabled() {
                    println!(
                        "   2. Add or generate the first project file; the managed sync bridge will index it automatically."
                    );
                } else {
                    println!(
                        "   2. Add or generate the first project file, then run {}.",
                        style("project(action=\"index\")").cyan()
                    );
                }
                println!(
                    "   3. Ask the harness to run {} for this exact folder.",
                    style("project(action=\"index_status\")").cyan()
                );
            } else {
                println!(
                    "   2. Ask the harness to run {} for this exact checkout.",
                    style("project(action=\"index_status\")").cyan()
                );
            }
            println!(
                "      If the checkout is unconfirmed or the bridge is offline, keep hosted MCP configured and run:"
            );
            println!(
                "      {}",
                style(format!(
                    "contextstream-mcp doctor --repair --scope global --editors {editor_ids}"
                ))
                .cyan()
            );
            let prompt_step = if outcome.awaiting_first_files { 4 } else { 3 };
            println!(
                "   {prompt_step}. When checkout readiness and indexed coverage are confirmed, ask:"
            );
            println!("      {}", style(super::first_value_prompt()).cyan());
            let doctor_step = prompt_step + 1;
            println!(
                "   {doctor_step}. Verify the handshake and grounding evidence with {}.",
                style(format!(
                    "contextstream-mcp doctor --scope all --editors {editor_ids}"
                ))
                .cyan()
            );
            let workflow_step = doctor_step + 1;
            println!(
                "   {workflow_step}. Inspect the workflow with {}.",
                style("help(action=\"workflow\", client_name=\"<editor-id>\")").cyan()
            );
        }
    }
    if outcome.mcp_editor_count > 0 {
        println!();
        println!("   Connection is still pending. The dashboard should show connected only after");
        println!("   the editor completes a real MCP handshake.");
    } else if !configured.is_empty() {
        println!();
        println!(
            "   No runtime connection is pending because the selected integration is rules-only."
        );
    }
    println!();
    println!(
        "   Dashboard: {}",
        style("https://app.contextstream.io/dashboard-v2")
            .cyan()
            .underlined()
    );
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_from_id_roundtrips_every_editor() {
        for editor in Editor::all() {
            assert_eq!(Editor::from_id(editor.id()), Some(*editor));
        }
        assert_eq!(Editor::from_id("not-an-editor"), None);
        assert_eq!(Editor::from_id("  CLAUDE  "), Some(Editor::ClaudeCode));
    }

    #[test]
    fn partition_handles_unknown_and_empty() {
        let (_configured, _skipped, unknown) =
            partition_profile_editors(&["claude".to_string(), "definitely-new-editor".to_string()]);
        assert_eq!(unknown, vec!["definitely-new-editor".to_string()]);

        let (configured, skipped, unknown) = partition_profile_editors(&[]);
        assert!(configured.is_empty());
        assert!(skipped.is_empty());
        assert!(unknown.is_empty());
    }

    #[test]
    fn profile_payload_parses_redeem_shape() {
        let json = r#"{
            "device_id": "6a3d1b2c-0000-0000-0000-000000000000",
            "user_id": "7b4e2c3d-0000-0000-0000-000000000000",
            "email": "dev@example.com",
            "api_key": {"id": "8c5f3d4e-0000-0000-0000-000000000000", "secret": "cbiq_abc"},
            "api_url": "https://api.contextstream.io",
            "profile": {
                "editors": ["claude", "cursor"],
                "rules_mode": "standard",
                "workspace_id": "9d6a4e5f-0000-0000-0000-000000000000",
                "workspace_name": "Engineering",
                "kit_version": "context-kit-v1"
            }
        }"#;
        let payload: SetupProfilePayload = serde_json::from_str(json).unwrap();
        assert_eq!(payload.profile.editors, vec!["claude", "cursor"]);
        assert_eq!(
            payload.profile.workspace_name.as_deref(),
            Some("Engineering")
        );
    }

    #[test]
    fn profile_file_rejects_duplicate_keys_at_any_depth() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("profile.json");
        let original = r#"{
            "device_id": "device",
            "user_id": "user",
            "email": "dev@example.com",
            "api_key": {"id": "first", "id": "second", "secret": "cbiq_abc"},
            "api_url": "https://api.contextstream.io"
        }"#;
        std::fs::write(&path, original).unwrap();

        let error = load_profile_file(&path).expect_err("duplicate profile key must be ambiguous");
        assert!(error.to_string().contains("parsing profile file"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }
}
