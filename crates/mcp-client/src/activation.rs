//! PII-safe activation metadata for the ContextStream MCP runtime.
//!
//! The installation id is a random UUID persisted under `~/.contextstream`.
//! It is never derived from a hostname, username, path, MAC address, or other
//! machine fingerprint.

use chrono::{DateTime, Utc};
use dashmap::DashMap;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{OnceLock, RwLock};
use uuid::Uuid;

pub const ACTIVATION_SCHEMA_VERSION: i16 = 1;
pub const ONBOARDING_VERSION: &str = "starter_mcp_v1";
const MAX_INSTALLATION_STATE_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InstallationState {
    schema_version: i16,
    installation_id: Uuid,
    created_at: DateTime<Utc>,
    #[serde(default)]
    configured_clients: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_install_completed_at: Option<DateTime<Utc>>,
    /// Preserve fields written by newer installers so an older runtime
    /// updating only the configured-client selection never erases them.
    #[serde(flatten)]
    extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeMetadata {
    pub installation_id: Uuid,
    pub client_name: Option<String>,
    pub client_version: String,
    pub os_family: String,
    pub os_version: String,
    pub architecture: String,
    pub onboarding_version: String,
    pub configured_clients: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredClientSelection {
    pub clients: Vec<String>,
    /// True after setup explicitly saved a selection, including an empty one.
    pub recorded: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ActivationEventPayload {
    pub event_id: Uuid,
    pub event_schema_version: i16,
    pub event_name: &'static str,
    pub occurred_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    pub client_version: String,
    pub os_family: String,
    pub os_version: String,
    pub architecture: String,
    pub onboarding_version: String,
    pub installation_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub properties: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_stage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    #[serde(default)]
    pub result_is_error: bool,
}

static RUNTIME_METADATA: OnceLock<RwLock<RuntimeMetadata>> = OnceLock::new();
static REPORTED_ACTIONS: OnceLock<DashMap<String, Uuid>> = OnceLock::new();
static REPORTED_FAILURES: OnceLock<DashMap<String, Uuid>> = OnceLock::new();

#[cfg(not(test))]
static INSTALLATION_STATE_PERSISTENCE_ENABLED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);

#[cfg(test)]
thread_local! {
    static TEST_INSTALLATION_STATE_PERSISTENCE_ENABLED: std::cell::Cell<bool> =
        const { std::cell::Cell::new(true) };
}

/// Enable or disable installation-state persistence for this process.
///
/// Installer dry runs disable this before any API client is used. Runtime
/// request headers still receive an opaque ephemeral UUID when no durable
/// identity exists, but no directory, lock, or installation file is created.
/// Production callers should set this once for the lifetime of a CLI command.
pub fn set_installation_state_persistence_enabled(enabled: bool) {
    #[cfg(not(test))]
    INSTALLATION_STATE_PERSISTENCE_ENABLED.store(enabled, std::sync::atomic::Ordering::SeqCst);

    #[cfg(test)]
    TEST_INSTALLATION_STATE_PERSISTENCE_ENABLED.set(enabled);
}

fn installation_state_persistence_enabled() -> bool {
    #[cfg(not(test))]
    {
        INSTALLATION_STATE_PERSISTENCE_ENABLED.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[cfg(test)]
    {
        TEST_INSTALLATION_STATE_PERSISTENCE_ENABLED.get()
    }
}

pub fn activation_enabled() -> bool {
    std::env::var("CONTEXTSTREAM_ACTIVATION_SCHEMA_VERSION")
        .ok()
        .map(|value| value.trim() != "0")
        .unwrap_or(true)
}

fn installation_path() -> PathBuf {
    dirs::home_dir()
        .map(|home| home.join(".contextstream").join("installation.json"))
        .unwrap_or_else(|| PathBuf::from(".contextstream/installation.json"))
}

fn read_state(path: &Path) -> std::io::Result<Option<InstallationState>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                format!(
                    "Refusing non-regular installation state at {}",
                    path.display()
                ),
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_INSTALLATION_STATE_BYTES {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!(
                "Refusing non-regular or oversized installation state at {}",
                path.display()
            ),
        ));
    }
    let mut content = String::with_capacity(metadata.len() as usize);
    file.read_to_string(&mut content)?;
    let value = crate::json::parse_value_without_duplicate_keys(&content).map_err(|error| {
        std::io::Error::new(
            ErrorKind::InvalidData,
            format!(
                "Refusing to replace malformed installation state at {}: {}",
                path.display(),
                error
            ),
        )
    })?;
    let state: InstallationState = serde_json::from_value(value).map_err(|error| {
        std::io::Error::new(
            ErrorKind::InvalidData,
            format!(
                "Refusing to replace malformed installation state at {}: {}",
                path.display(),
                error
            ),
        )
    })?;
    if state.schema_version != ACTIVATION_SCHEMA_VERSION {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!(
                "Unsupported installation state schema {} at {} (expected {})",
                state.schema_version,
                path.display(),
                ACTIVATION_SCHEMA_VERSION
            ),
        ));
    }
    Ok(Some(state))
}

fn new_installation_state() -> InstallationState {
    InstallationState {
        schema_version: ACTIVATION_SCHEMA_VERSION,
        installation_id: Uuid::new_v4(),
        created_at: Utc::now(),
        configured_clients: Vec::new(),
        last_install_completed_at: None,
        extra: serde_json::Map::new(),
    }
}

fn lock_state(path: &Path) -> std::io::Result<File> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            ErrorKind::InvalidInput,
            format!("Installation state path has no parent: {}", path.display()),
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let lock_path = path.with_extension("lock");
    match std::fs::symlink_metadata(&lock_path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(std::io::Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "Refusing non-regular installation-state lock {}",
                    lock_path.display()
                ),
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let lock = options.open(lock_path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        lock.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    FileExt::lock_exclusive(&lock)?;
    Ok(lock)
}

#[cfg(not(windows))]
fn replace_file_atomically(temporary: &Path, path: &Path) -> std::io::Result<()> {
    std::fs::rename(temporary, path)
}

#[cfg(windows)]
fn replace_file_atomically(temporary: &Path, path: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let temporary_wide: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
    let path_wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let result = unsafe {
        MoveFileExW(
            temporary_wide.as_ptr(),
            path_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn write_state(path: &Path, state: &InstallationState) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            return Err(std::io::Error::new(
                ErrorKind::InvalidInput,
                format!("Refusing non-regular installation state {}", path.display()),
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let temporary = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    let bytes = serde_json::to_vec_pretty(state).map_err(std::io::Error::other)?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    file.write_all(&bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.sync_all()?;
    drop(file);
    if let Err(error) = replace_file_atomically(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

fn load_or_create_state_at(path: &Path) -> InstallationState {
    if !installation_state_persistence_enabled() {
        return match read_state(path) {
            Ok(Some(state)) => state,
            Ok(None) => new_installation_state(),
            Err(error) => {
                tracing::warn!(
                    %error,
                    path = %path.display(),
                    "installation state is unreadable; preserving it and using ephemeral runtime metadata"
                );
                new_installation_state()
            }
        };
    }

    let _lock = match lock_state(path) {
        Ok(lock) => lock,
        Err(error) => {
            tracing::warn!(
                %error,
                path = %path.display(),
                "installation state lock is unavailable; using ephemeral runtime metadata"
            );
            return new_installation_state();
        }
    };
    match read_state(path) {
        Ok(Some(state)) => return state,
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(
                %error,
                path = %path.display(),
                "installation state is unreadable; preserving it and using ephemeral runtime metadata"
            );
            return new_installation_state();
        }
    }
    let state = new_installation_state();
    if let Err(error) = write_state(path, &state) {
        tracing::debug!(%error, "could not persist opaque MCP installation id");
    }
    state
}

fn bounded_version(value: &str) -> String {
    let token = value.split_whitespace().next().unwrap_or_default();
    let clean: String = token
        .trim()
        .chars()
        .filter(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_' | '+')
        })
        .take(64)
        .collect();
    if clean.is_empty() {
        "unknown".to_string()
    } else {
        clean
    }
}

fn detected_os_version() -> String {
    #[cfg(target_os = "windows")]
    let output = Command::new("cmd").args(["/C", "ver"]).output();
    #[cfg(target_os = "macos")]
    let output = Command::new("sw_vers").arg("-productVersion").output();
    #[cfg(all(unix, not(target_os = "macos")))]
    let output = Command::new("uname").arg("-r").output();
    #[cfg(not(any(unix, target_os = "windows")))]
    let output: std::io::Result<std::process::Output> = Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "OS version detection unsupported",
    ));

    output
        .ok()
        .filter(|output| output.status.success())
        .map(|output| bounded_version(&String::from_utf8_lossy(&output.stdout)))
        .unwrap_or_else(|| "unknown".to_string())
}

fn normalized_client_name(value: &str) -> Option<String> {
    let value = value.trim().to_ascii_lowercase();
    (!value.is_empty()
        && value.len() <= 64
        && value
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_')))
    .then_some(value)
}

fn runtime_metadata_lock() -> &'static RwLock<RuntimeMetadata> {
    RUNTIME_METADATA.get_or_init(|| {
        let state = load_or_create_state_at(&installation_path());
        RwLock::new(RuntimeMetadata {
            installation_id: state.installation_id,
            client_name: std::env::var("CONTEXTSTREAM_CLIENT")
                .ok()
                .and_then(|value| normalized_client_name(&value)),
            client_version: mcp_types::config::VERSION.to_string(),
            os_family: std::env::consts::OS.to_string(),
            os_version: detected_os_version(),
            architecture: std::env::consts::ARCH.to_string(),
            onboarding_version: ONBOARDING_VERSION.to_string(),
            configured_clients: state.configured_clients,
        })
    })
}

pub fn runtime_metadata() -> RuntimeMetadata {
    runtime_metadata_lock()
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
}

/// Return the persisted opaque installation id without creating any files.
///
/// Config previews and dry runs use this read-only path so inspecting a
/// prospective editor config can never create `~/.contextstream`, a lock file,
/// or installation state. Malformed, oversized, or non-regular state is an
/// error rather than permission to silently mint a replacement identity.
pub fn existing_installation_id() -> std::io::Result<Option<Uuid>> {
    existing_installation_id_at(&installation_path())
}

fn existing_installation_id_at(path: &Path) -> std::io::Result<Option<Uuid>> {
    Ok(read_state(path)?.map(|state| state.installation_id))
}

/// Return a durable installation id, creating state only when it is absent.
///
/// Unlike runtime telemetry's best-effort loader, editor config writers need a
/// strict identity: an ephemeral UUID must never be written into user config.
/// Existing malformed state therefore fails closed and remains byte-identical.
pub fn ensure_installation_id() -> std::io::Result<Uuid> {
    ensure_installation_id_at(&installation_path())
}

fn ensure_installation_id_at(path: &Path) -> std::io::Result<Uuid> {
    if !installation_state_persistence_enabled() {
        return read_state(path)?
            .map(|state| state.installation_id)
            .ok_or_else(|| {
                std::io::Error::new(
                    ErrorKind::PermissionDenied,
                    "Installation-state persistence is disabled for this command",
                )
            });
    }
    let _lock = lock_state(path)?;
    if let Some(state) = read_state(path)? {
        return Ok(state.installation_id);
    }
    let state = new_installation_state();
    let installation_id = state.installation_id;
    write_state(path, &state)?;
    Ok(installation_id)
}

fn update_cached_configured_clients(configured_clients: &[String]) {
    if let Some(metadata) = RUNTIME_METADATA.get() {
        metadata
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .configured_clients = configured_clients.to_vec();
    }
}

/// Append one configured client for compatibility with older callers.
///
/// Setup selection should use [`replace_configured_clients`] so a later run can
/// deliberately deselect an editor.
pub fn record_configured_client(client_name: &str) {
    if !installation_state_persistence_enabled() {
        return;
    }
    let path = installation_path();
    let state = match record_configured_client_at(&path, client_name) {
        Ok(Some(state)) => state,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(%error, "refusing to overwrite malformed MCP installation state");
            return;
        }
    };
    if let Some(metadata) = RUNTIME_METADATA.get() {
        metadata
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .configured_clients = state.configured_clients;
    }
}

fn record_configured_client_at(
    path: &Path,
    client_name: &str,
) -> std::io::Result<Option<InstallationState>> {
    let Some(client_name) = normalized_client_name(client_name) else {
        return Ok(None);
    };
    let _lock = lock_state(path)?;
    let mut state = read_state(path)?.unwrap_or_else(new_installation_state);
    let changed = if !state.configured_clients.contains(&client_name) {
        state.configured_clients.push(client_name);
        state.configured_clients.sort();
        state.configured_clients.truncate(20);
        true
    } else {
        false
    };
    if !changed && state.last_install_completed_at.is_some() {
        return Ok(Some(state));
    }
    state.last_install_completed_at = Some(Utc::now());
    write_state(path, &state)?;
    Ok(Some(state))
}

/// Replace the authoritative editor selection recorded by setup.
///
/// The write fails closed when the existing state is unreadable. Selection is
/// recorded before per-editor writes so a partial setup can be repaired by
/// later `--only-configured` refreshes.
pub fn replace_configured_clients(client_names: &[String]) -> std::io::Result<()> {
    if !installation_state_persistence_enabled() {
        return Ok(());
    }
    replace_configured_clients_at(&installation_path(), client_names)
}

fn replace_configured_clients_at(path: &Path, client_names: &[String]) -> std::io::Result<()> {
    let mut configured_clients: Vec<String> = client_names
        .iter()
        .filter_map(|name| normalized_client_name(name))
        .collect();
    configured_clients.sort();
    configured_clients.dedup();
    configured_clients.truncate(20);

    let _lock = lock_state(path)?;
    let mut state = read_state(path)?.unwrap_or_else(new_installation_state);
    if state.configured_clients == configured_clients && state.last_install_completed_at.is_some() {
        update_cached_configured_clients(&state.configured_clients);
        return Ok(());
    }
    state.configured_clients = configured_clients;
    state.last_install_completed_at = Some(Utc::now());
    write_state(path, &state)?;

    update_cached_configured_clients(&state.configured_clients);
    Ok(())
}

/// Remove editors from setup's authoritative selection.
///
/// Uninstall uses this after attempting every requested cleanup so a later
/// unattended `--only-configured` refresh cannot recreate an integration the
/// user explicitly removed. The update is serialized with setup writes and
/// rereads the state while holding the lock, so concurrent setup/uninstall
/// processes cannot lose unrelated client ids.
pub fn remove_configured_clients(client_names: &[String]) -> std::io::Result<bool> {
    if !installation_state_persistence_enabled() {
        return Ok(false);
    }
    remove_configured_clients_at(&installation_path(), client_names)
}

fn remove_configured_clients_at(path: &Path, client_names: &[String]) -> std::io::Result<bool> {
    let removed: std::collections::HashSet<String> = client_names
        .iter()
        .filter_map(|name| normalized_client_name(name))
        .collect();
    if removed.is_empty() {
        return Ok(false);
    }

    let _lock = lock_state(path)?;
    let Some(mut state) = read_state(path)? else {
        // Missing installation state already behaves like "no configured
        // clients"; do not create telemetry state merely because uninstall
        // was run.
        return Ok(false);
    };
    let previous_len = state.configured_clients.len();
    state
        .configured_clients
        .retain(|client| !removed.contains(client));
    if state.configured_clients.len() == previous_len {
        return Ok(false);
    }

    // Preserve an explicitly empty selection as authoritative. Otherwise a
    // later interactive refresh could fall back to detection and reinstall an
    // editor the user just removed.
    state.last_install_completed_at = Some(Utc::now());
    write_state(path, &state)?;

    update_cached_configured_clients(&state.configured_clients);
    Ok(true)
}

/// Editor ids that setup selected on this machine.
///
/// Read straight from disk with no side effects so hook/rule refreshes can
/// scope themselves to the editors the user actually chose. Malformed state is
/// an error, never an implicit empty selection or permission to detect broadly.
pub fn configured_clients() -> std::io::Result<Vec<String>> {
    Ok(configured_client_selection()?.clients)
}

/// Editor selection plus whether setup ever recorded it.
///
/// An explicitly empty selection must not be confused with an old install
/// that predates selection tracking; only the latter may fall back to editor
/// detection.
pub fn configured_client_selection() -> std::io::Result<ConfiguredClientSelection> {
    configured_client_selection_at(&installation_path())
}

fn configured_client_selection_at(path: &Path) -> std::io::Result<ConfiguredClientSelection> {
    Ok(match read_state(path)? {
        Some(state) => ConfiguredClientSelection {
            recorded: state.last_install_completed_at.is_some(),
            clients: state.configured_clients,
        },
        None => ConfiguredClientSelection {
            clients: Vec::new(),
            recorded: false,
        },
    })
}

fn runtime_headers_for(metadata: &RuntimeMetadata) -> Vec<(&'static str, String)> {
    let mut headers = vec![
        ("X-ContextStream-MCP-Runtime", "1".to_string()),
        (
            "X-ContextStream-Installation-Id",
            metadata.installation_id.to_string(),
        ),
        (
            "X-ContextStream-MCP-Version",
            metadata.client_version.clone(),
        ),
        ("X-ContextStream-OS-Family", metadata.os_family.clone()),
        ("X-ContextStream-OS-Version", metadata.os_version.clone()),
        ("X-ContextStream-Arch", metadata.architecture.clone()),
        (
            "X-ContextStream-Onboarding-Version",
            metadata.onboarding_version.clone(),
        ),
    ];
    if let Some(client_name) = metadata.client_name.clone() {
        headers.push(("X-ContextStream-Client", client_name));
    }
    headers
}

pub fn runtime_headers() -> Vec<(&'static str, String)> {
    if !activation_enabled() {
        return Vec::new();
    }
    runtime_headers_for(&runtime_metadata())
}

pub fn first_action_event(
    workspace_id: Option<Uuid>,
    project_id: Option<Uuid>,
    session_id: Option<String>,
    tool_name: &str,
    operation_category: &str,
) -> Option<ActivationEventPayload> {
    if !activation_enabled() {
        return None;
    }
    let metadata = runtime_metadata();
    first_action_event_with_metadata(
        metadata,
        workspace_id,
        project_id,
        session_id,
        tool_name,
        operation_category,
    )
}

fn first_action_event_with_metadata(
    metadata: RuntimeMetadata,
    workspace_id: Option<Uuid>,
    project_id: Option<Uuid>,
    session_id: Option<String>,
    tool_name: &str,
    operation_category: &str,
) -> Option<ActivationEventPayload> {
    let dedupe_key = format!(
        "{}:{}",
        metadata.installation_id,
        session_id.as_deref().unwrap_or("process")
    );
    let event_id = Uuid::new_v4();
    if REPORTED_ACTIONS
        .get_or_init(DashMap::new)
        .insert(dedupe_key, event_id)
        .is_some()
    {
        return None;
    }
    Some(ActivationEventPayload {
        event_id,
        event_schema_version: ACTIVATION_SCHEMA_VERSION,
        event_name: "first_mcp_action_succeeded",
        occurred_at: Utc::now(),
        workspace_id,
        project_id,
        client_name: metadata.client_name,
        client_version: metadata.client_version,
        os_family: metadata.os_family,
        os_version: metadata.os_version,
        architecture: metadata.architecture,
        onboarding_version: metadata.onboarding_version,
        installation_id: metadata.installation_id,
        session_id,
        properties: serde_json::json!({
            "tool_name": tool_name,
            "operation_category": operation_category,
        }),
        error_stage: None,
        error_code: None,
        retryable: None,
        http_status: None,
        result_is_error: false,
    })
}

pub fn failure_event(
    workspace_id: Option<Uuid>,
    project_id: Option<Uuid>,
    session_id: Option<String>,
    stage: &str,
    error_code: &str,
    retryable: bool,
    http_status: Option<u16>,
) -> Option<ActivationEventPayload> {
    if !activation_enabled() {
        return None;
    }
    let metadata = runtime_metadata();
    failure_event_with_metadata(
        metadata,
        workspace_id,
        project_id,
        session_id,
        stage,
        error_code,
        retryable,
        http_status,
    )
}

#[allow(clippy::too_many_arguments)]
fn failure_event_with_metadata(
    metadata: RuntimeMetadata,
    workspace_id: Option<Uuid>,
    project_id: Option<Uuid>,
    session_id: Option<String>,
    stage: &str,
    error_code: &str,
    retryable: bool,
    http_status: Option<u16>,
) -> Option<ActivationEventPayload> {
    let dedupe_key = format!(
        "{}:{}:{}:{}",
        metadata.installation_id,
        session_id.as_deref().unwrap_or("process"),
        stage,
        error_code
    );
    let event_id = Uuid::new_v4();
    if REPORTED_FAILURES
        .get_or_init(DashMap::new)
        .insert(dedupe_key, event_id)
        .is_some()
    {
        return None;
    }
    Some(ActivationEventPayload {
        event_id,
        event_schema_version: ACTIVATION_SCHEMA_VERSION,
        event_name: "mcp_connection_failed",
        occurred_at: Utc::now(),
        workspace_id,
        project_id,
        client_name: metadata.client_name,
        client_version: metadata.client_version,
        os_family: metadata.os_family,
        os_version: metadata.os_version,
        architecture: metadata.architecture,
        onboarding_version: metadata.onboarding_version,
        installation_id: metadata.installation_id,
        session_id,
        properties: serde_json::json!({ "connection_transport": "stdio" }),
        error_stage: Some(stage.to_string()),
        error_code: Some(error_code.to_string()),
        retryable: Some(retryable),
        http_status,
        result_is_error: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[test]
    fn installation_id_persists_and_corruption_is_preserved() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("installation.json");
        let first = load_or_create_state_at(&path);
        let second = load_or_create_state_at(&path);
        assert_eq!(first.installation_id, second.installation_id);

        std::fs::write(&path, "not-json").unwrap();
        let ephemeral = load_or_create_state_at(&path);
        assert_ne!(first.installation_id, ephemeral.installation_id);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "not-json");
        assert!(read_state(&path).is_err());
    }

    #[test]
    fn read_only_installation_id_never_creates_state_or_lock_files() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("not-created").join("installation.json");

        assert_eq!(existing_installation_id_at(&path).unwrap(), None);
        assert!(!path.exists());
        assert!(!path.with_extension("lock").exists());
        assert!(!path.parent().unwrap().exists());
    }

    #[test]
    fn disabled_persistence_keeps_runtime_identity_fully_ephemeral() {
        struct PersistenceReset;
        impl Drop for PersistenceReset {
            fn drop(&mut self) {
                set_installation_state_persistence_enabled(true);
            }
        }

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("not-created").join("installation.json");
        set_installation_state_persistence_enabled(false);
        let _reset = PersistenceReset;

        let first = load_or_create_state_at(&path);
        let second = load_or_create_state_at(&path);
        assert_ne!(first.installation_id, second.installation_id);
        assert_eq!(
            ensure_installation_id_at(&path)
                .expect_err("strict persistence must be unavailable")
                .kind(),
            ErrorKind::PermissionDenied
        );
        assert!(!path.exists());
        assert!(!path.with_extension("lock").exists());
        assert!(!path.parent().unwrap().exists());
    }

    #[test]
    fn strict_installation_id_creation_is_stable_and_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("installation.json");

        let first = ensure_installation_id_at(&path).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let second = ensure_installation_id_at(&path).unwrap();
        assert_eq!(first, second);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);

        std::fs::write(&path, "{ definitely not json").unwrap();
        assert!(ensure_installation_id_at(&path).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{ definitely not json"
        );
    }

    #[test]
    fn configured_client_selection_is_replaced_not_appended() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("installation.json");

        replace_configured_clients_at(&path, &["claude".to_string(), "codex".to_string()]).unwrap();
        replace_configured_clients_at(&path, &["codex".to_string()]).unwrap();

        let state = read_state(&path).unwrap().unwrap();
        assert_eq!(state.configured_clients, vec!["codex"]);
    }

    #[test]
    fn identical_configured_client_selection_is_byte_identical() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("installation.json");
        replace_configured_clients_at(&path, &["codex".to_string()]).unwrap();
        let before = std::fs::read(&path).unwrap();

        replace_configured_clients_at(
            &path,
            &[
                "codex".to_string(),
                "codex".to_string(),
                "CODEX".to_string(),
            ],
        )
        .unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn duplicate_compatibility_record_is_byte_identical() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("installation.json");
        record_configured_client_at(&path, "codex").unwrap();
        let before = std::fs::read(&path).unwrap();

        record_configured_client_at(&path, "CODEX").unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn configured_client_removal_is_selective_and_empty_remains_recorded() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("installation.json");

        replace_configured_clients_at(
            &path,
            &[
                "claude".to_string(),
                "codex".to_string(),
                "cursor".to_string(),
            ],
        )
        .unwrap();

        assert!(
            remove_configured_clients_at(&path, &["codex".to_string(), "cursor".to_string()])
                .unwrap()
        );
        let remaining = configured_client_selection_at(&path).unwrap();
        assert!(remaining.recorded);
        assert_eq!(remaining.clients, vec!["claude"]);

        assert!(remove_configured_clients_at(&path, &["claude".to_string()]).unwrap());
        let empty = configured_client_selection_at(&path).unwrap();
        assert!(empty.recorded);
        assert!(empty.clients.is_empty());
        assert!(
            !remove_configured_clients_at(&path, &["claude".to_string()]).unwrap(),
            "an idempotent removal must not rewrite state"
        );
    }

    #[test]
    fn configured_client_removal_does_not_create_or_replace_state() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing.json");
        assert!(!remove_configured_clients_at(&missing, &["codex".to_string()]).unwrap());
        assert!(!missing.exists());

        let malformed = temp.path().join("malformed.json");
        std::fs::write(&malformed, "{ definitely not json").unwrap();
        assert!(remove_configured_clients_at(&malformed, &["codex".to_string()]).is_err());
        assert_eq!(
            std::fs::read_to_string(&malformed).unwrap(),
            "{ definitely not json"
        );
    }

    #[test]
    fn configured_client_update_preserves_unknown_future_fields() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("installation.json");
        let installation_id = Uuid::new_v4();
        let original = serde_json::json!({
            "schema_version": ACTIVATION_SCHEMA_VERSION,
            "installation_id": installation_id,
            "created_at": "2026-07-27T00:00:00Z",
            "configured_clients": ["claude"],
            "last_install_completed_at": "2026-07-27T00:00:01Z",
            "future_scalar": "preserve",
            "future_object": {"nested": [1, 2, 3]}
        });
        std::fs::write(&path, serde_json::to_vec_pretty(&original).unwrap()).unwrap();

        replace_configured_clients_at(&path, &["codex".to_string()]).unwrap();

        let updated: Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).expect("parse updated state");
        assert_eq!(updated["installation_id"], installation_id.to_string());
        assert_eq!(updated["configured_clients"], serde_json::json!(["codex"]));
        assert_eq!(updated["future_scalar"], "preserve");
        assert_eq!(
            updated["future_object"],
            serde_json::json!({"nested": [1, 2, 3]})
        );
    }

    #[test]
    fn concurrent_client_records_do_not_lose_updates() {
        const CLIENT_COUNT: usize = 12;
        let temp = tempfile::tempdir().unwrap();
        let path = Arc::new(temp.path().join("installation.json"));
        let barrier = Arc::new(Barrier::new(CLIENT_COUNT));
        let mut workers = Vec::new();

        for index in 0..CLIENT_COUNT {
            let path = Arc::clone(&path);
            let barrier = Arc::clone(&barrier);
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                record_configured_client_at(&path, &format!("client-{index}"))
                    .expect("record configured client");
            }));
        }
        for worker in workers {
            worker.join().expect("record worker");
        }

        let state = read_state(&path).unwrap().unwrap();
        assert_eq!(state.configured_clients.len(), CLIENT_COUNT);
        for index in 0..CLIENT_COUNT {
            assert!(
                state
                    .configured_clients
                    .contains(&format!("client-{index}")),
                "missing concurrent client {index}: {:?}",
                state.configured_clients
            );
        }
    }

    #[test]
    fn explicitly_empty_selection_is_distinct_from_never_recorded() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("installation.json");

        let missing = configured_client_selection_at(&path).unwrap();
        assert!(!missing.recorded);
        assert!(missing.clients.is_empty());

        replace_configured_clients_at(&path, &[]).unwrap();
        let recorded = configured_client_selection_at(&path).unwrap();
        assert!(recorded.recorded);
        assert!(recorded.clients.is_empty());
    }

    #[test]
    fn malformed_state_is_never_overwritten_by_selection_update() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("installation.json");
        std::fs::write(&path, "{ definitely not json").unwrap();

        assert!(replace_configured_clients_at(&path, &["codex".to_string()]).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{ definitely not json"
        );
    }

    #[test]
    fn duplicate_unknown_state_fields_are_never_collapsed_by_an_update() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("installation.json");
        let original = format!(
            concat!(
                "{{",
                "\"schema_version\":1,",
                "\"installation_id\":\"{}\",",
                "\"created_at\":\"2026-07-27T00:00:00Z\",",
                "\"configured_clients\":[\"claude\"],",
                "\"future\":{{\"mode\":\"one\",\"mode\":\"two\"}}",
                "}}"
            ),
            Uuid::new_v4()
        );
        std::fs::write(&path, &original).unwrap();

        assert!(replace_configured_clients_at(&path, &["codex".to_string()]).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    }

    #[cfg(unix)]
    #[test]
    fn configured_client_update_refuses_symlinked_state_and_lock_files() {
        use std::os::unix::fs::symlink;

        let temp = tempfile::tempdir().unwrap();
        let state_path = temp.path().join("installation.json");
        let unrelated_state = temp.path().join("unrelated-state.json");
        write_state(&unrelated_state, &new_installation_state()).unwrap();
        let original_state = std::fs::read_to_string(&unrelated_state).unwrap();
        symlink(&unrelated_state, &state_path).unwrap();

        assert!(replace_configured_clients_at(&state_path, &["codex".to_string()]).is_err());
        assert_eq!(
            std::fs::read_to_string(&unrelated_state).unwrap(),
            original_state
        );
        assert!(state_path.is_symlink());

        std::fs::remove_file(&state_path).unwrap();
        let lock_target = temp.path().join("unrelated-lock");
        let lock_path = state_path.with_extension("lock");
        std::fs::remove_file(&lock_path).unwrap();
        std::fs::write(&lock_target, "do not chmod or lock").unwrap();
        symlink(&lock_target, &lock_path).unwrap();

        assert!(replace_configured_clients_at(&state_path, &["codex".to_string()]).is_err());
        assert_eq!(
            std::fs::read_to_string(&lock_target).unwrap(),
            "do not chmod or lock"
        );
        assert!(lock_path.is_symlink());
        assert!(!state_path.exists());
    }

    #[test]
    fn version_and_client_metadata_are_bounded_and_path_free() {
        assert_eq!(bounded_version(" 23.4.0 (build /Users/alice) "), "23.4.0");
        assert_eq!(
            normalized_client_name("Codex-CLI"),
            Some("codex-cli".to_string())
        );
        assert_eq!(normalized_client_name("/home/alice/client"), None);
    }

    #[test]
    fn runtime_headers_are_deterministic_and_pii_free() {
        let installation_id = Uuid::parse_str("20202020-2020-4020-8020-202020202020").unwrap();
        let metadata = RuntimeMetadata {
            installation_id,
            client_name: Some("codex".to_string()),
            client_version: "0.5.34".to_string(),
            os_family: "linux".to_string(),
            os_version: "6.8.0".to_string(),
            architecture: "x86_64".to_string(),
            onboarding_version: ONBOARDING_VERSION.to_string(),
            configured_clients: vec!["codex".to_string()],
        };
        let headers = runtime_headers_for(&metadata);
        assert!(headers.contains(&("X-ContextStream-MCP-Runtime", "1".to_string())));
        assert!(headers.contains(&(
            "X-ContextStream-Installation-Id",
            installation_id.to_string()
        )));
        assert!(headers.contains(&("X-ContextStream-Client", "codex".to_string())));
        let snapshot = serde_json::to_string(&headers).unwrap();
        assert!(!snapshot.contains("/home/"));
        assert!(!snapshot.contains("api_key"));
        assert!(!snapshot.contains("token"));
    }

    #[test]
    fn first_action_is_emitted_once_per_installation_session() {
        let session_id = format!("test-{}", Uuid::new_v4());
        let metadata = RuntimeMetadata {
            installation_id: Uuid::new_v4(),
            client_name: Some("test".to_string()),
            client_version: "0.0.0".to_string(),
            os_family: "linux".to_string(),
            os_version: "test".to_string(),
            architecture: "x86_64".to_string(),
            onboarding_version: ONBOARDING_VERSION.to_string(),
            configured_clients: Vec::new(),
        };
        let first = first_action_event_with_metadata(
            metadata.clone(),
            None,
            None,
            Some(session_id.clone()),
            "search",
            "code_search",
        );
        let duplicate = first_action_event_with_metadata(
            metadata,
            None,
            None,
            Some(session_id),
            "memory",
            "memory",
        );
        assert!(first.is_some());
        assert!(duplicate.is_none());
    }

    #[test]
    fn failure_payload_contains_codes_not_raw_errors() {
        let metadata = RuntimeMetadata {
            installation_id: Uuid::new_v4(),
            client_name: Some("test".to_string()),
            client_version: "0.0.0".to_string(),
            os_family: "linux".to_string(),
            os_version: "test".to_string(),
            architecture: "x86_64".to_string(),
            onboarding_version: ONBOARDING_VERSION.to_string(),
            configured_clients: Vec::new(),
        };
        let payload = failure_event_with_metadata(
            metadata,
            None,
            None,
            Some(format!("test-{}", Uuid::new_v4())),
            "initialize",
            "unauthorized",
            false,
            Some(401),
        )
        .expect("first failure");
        let json = serde_json::to_value(payload).unwrap();
        assert_eq!(json["error_stage"], "initialize");
        assert_eq!(json["error_code"], "unauthorized");
        assert!(json.get("raw_error").is_none());
    }
}
