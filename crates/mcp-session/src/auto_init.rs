//! Auto-initialization logic for sessions.

use crate::checkout_identity::{
    current_checkout_fingerprint, current_repository_remote_identity, ensure_checkout_fingerprint,
    identity_kind_for_establishment, safe_checkout_config_path, validate_checkout_binding,
    validate_checkout_scope, validate_configured_remote_identity, CheckoutId,
    CheckoutIdentityError, CheckoutIdentityKind, RepositoryFingerprint, RepositoryRemoteIdentity,
};
use mcp_client::{
    json::parse_value_without_duplicate_keys, ContextStreamClient, SessionInitParams,
};
use mcp_types::Result;
use std::{
    fs::{File, OpenOptions},
    io::{ErrorKind, Write},
    path::Path,
    sync::OnceLock,
};
use uuid::Uuid;

fn global_mapping_write_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn read_regular_text_snapshot(
    path: &Path,
) -> std::result::Result<Option<String>, CheckoutIdentityError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            Err(CheckoutIdentityError::io(
                "inspect state destination",
                path,
                std::io::Error::new(
                    ErrorKind::InvalidInput,
                    "refusing a symlink or non-regular state file",
                ),
            ))
        }
        Ok(_) => std::fs::read_to_string(path)
            .map(Some)
            .map_err(|error| CheckoutIdentityError::io("read state snapshot", path, error)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(CheckoutIdentityError::io(
            "inspect state destination",
            path,
            error,
        )),
    }
}

#[cfg(not(windows))]
fn replace_state_file(temporary: &Path, destination: &Path) -> std::io::Result<()> {
    std::fs::rename(temporary, destination)
}

#[cfg(windows)]
fn replace_state_file(temporary: &Path, destination: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let temporary: Vec<u16> = temporary.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let result = unsafe {
        MoveFileExW(
            temporary.as_ptr(),
            destination.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn write_regular_text_snapshot(
    path: &Path,
    expected: Option<&str>,
    content: &str,
) -> std::result::Result<bool, CheckoutIdentityError> {
    let current = read_regular_text_snapshot(path)?;
    if current.as_deref() != expected {
        return Err(CheckoutIdentityError::io(
            "compare state snapshot",
            path,
            std::io::Error::other("state file changed after it was read; refusing to overwrite it"),
        ));
    }
    if current.as_deref() == Some(content) {
        return Ok(false);
    }

    let parent = path.parent().ok_or_else(|| {
        CheckoutIdentityError::io(
            "resolve state parent",
            path,
            std::io::Error::new(ErrorKind::InvalidInput, "state path has no parent"),
        )
    })?;
    std::fs::create_dir_all(parent).map_err(|error| {
        CheckoutIdentityError::io("create state parent directory", parent, error)
    })?;

    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("state");
    let temporary = parent.join(format!(".{file_name}.contextstream.{}.tmp", Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).map_err(|error| {
        CheckoutIdentityError::io("create temporary state file", &temporary, error)
    })?;

    let prepared = (|| -> std::io::Result<()> {
        file.write_all(content.as_bytes())?;
        if let Ok(metadata) = std::fs::metadata(path) {
            file.set_permissions(metadata.permissions())?;
        }
        file.sync_all()
    })();
    if let Err(error) = prepared {
        let _ = std::fs::remove_file(&temporary);
        return Err(CheckoutIdentityError::io(
            "prepare temporary state file",
            &temporary,
            error,
        ));
    }
    drop(file);

    let latest = match read_regular_text_snapshot(path) {
        Ok(latest) => latest,
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            return Err(error);
        }
    };
    if latest.as_deref() != expected {
        let _ = std::fs::remove_file(&temporary);
        return Err(CheckoutIdentityError::io(
            "compare state snapshot",
            path,
            std::io::Error::other("state file changed while the replacement was being prepared"),
        ));
    }

    if let Err(error) = replace_state_file(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(CheckoutIdentityError::io("replace state file", path, error));
    }
    #[cfg(unix)]
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| CheckoutIdentityError::io("sync state directory", parent, error))?;
    Ok(true)
}

/// Workspace mapping for folder paths.
#[derive(Debug, Clone)]
pub struct WorkspaceMapping {
    pub workspace_id: Uuid,
    pub workspace_name: String,
    pub project_id: Option<Uuid>,
    pub project_name: Option<String>,
}

enum LocalConfigResolution {
    Absent,
    Valid(WorkspaceMapping),
    InvalidBoundary { root: std::path::PathBuf },
}

/// Resolve workspace from folder path.
///
/// Looks for local config files (.contextstream/config.json) or
/// uses global mappings from ~/.contextstream/mappings.json.
pub async fn resolve_workspace(folder_path: &str) -> Option<WorkspaceMapping> {
    match resolve_local_config(folder_path).await {
        LocalConfigResolution::Valid(mapping) => Some(mapping),
        LocalConfigResolution::Absent => resolve_global_mapping(folder_path, false).await,
        // The nearest config is an explicit scope boundary. Missing legacy
        // identity, a replaced folder/repository, and malformed data all fail
        // closed. Explicit non-Git folder identities validate normally. A
        // same-path global mapping must never heal or re-bless this boundary
        // during ordinary init/context resolution.
        LocalConfigResolution::InvalidBoundary { root } => {
            tracing::debug!(
                checkout_root = %root.display(),
                "checkout-local config boundary failed repository identity validation"
            );
            None
        }
    }
}

/// Verify that a local checkout explicitly authorizes content operations for
/// the supplied project/workspace scope.
///
/// This is intentionally stricter than read-only scope resolution: the config
/// must be rooted at the exact checkout and carry the same project ID. Callers
/// use it before background ingest, reroot, or deletion-capable refresh paths.
pub fn checkout_binding_matches(
    folder_path: &str,
    workspace_id: Option<Uuid>,
    project_id: Uuid,
) -> bool {
    validate_checkout_binding(folder_path, workspace_id, project_id).is_ok()
}

/// Return the workspace authorized by the checkout-local content binding.
///
/// Unlike read-only scope discovery, content operations must have all three
/// pieces of local authority: the exact canonical checkout root, a project ID,
/// and a workspace ID. Returning the bound workspace lets callers reconcile a
/// registry/session hint with the local config and the server-side project
/// record before uploading bytes or applying deletion/reroot diffs.
pub fn checkout_binding_workspace(folder_path: &str, project_id: Uuid) -> Option<Uuid> {
    validate_checkout_binding(folder_path, None, project_id)
        .ok()
        .map(|binding| binding.workspace_id)
}

/// Resolve workspace from local .contextstream/config.json.
///
/// Walks up from `folder_path` through parent directories to find the nearest
/// `.contextstream/config.json`. This handles cases where the editor opens a
/// subdirectory or the project was checked out at a different path.
async fn resolve_local_config(folder_path: &str) -> LocalConfigResolution {
    let mut current = Path::new(folder_path).to_path_buf();

    loop {
        if let Some(resolution) = local_config_resolution_at(&current).await {
            return resolution;
        }

        if !current.pop() {
            break;
        }

        // Stop at filesystem root or home directory to avoid walking too far.
        if current.as_os_str().is_empty() || current == Path::new("/") {
            break;
        }
        if let Some(home) = dirs::home_dir() {
            if current == home {
                // Still check home/.contextstream/config.json before stopping
                if let Some(resolution) = local_config_resolution_at(&current).await {
                    return resolution;
                }
                break;
            }
        }
    }

    LocalConfigResolution::Absent
}

async fn local_config_resolution_at(root: &Path) -> Option<LocalConfigResolution> {
    let config_path = match safe_checkout_config_path(root, false) {
        Ok(config_path) => config_path,
        Err(CheckoutIdentityError::MissingLocalConfig(_)) => return None,
        // A symlink, non-regular path component, permission failure, or other
        // unreadable state is a deliberate fail-closed boundary. `Path::exists`
        // would incorrectly treat a dangling symlink as absence and allow a
        // global/ancestor mapping to bypass it.
        Err(_) => {
            return Some(LocalConfigResolution::InvalidBoundary {
                root: root.to_path_buf(),
            })
        }
    };

    // The nearest config is a scope boundary. If it is malformed or bound to
    // another checkout, never inherit an ancestor or global project.
    Some(match parse_local_mapping(&config_path).await {
        Some(mapping) => LocalConfigResolution::Valid(mapping),
        None => LocalConfigResolution::InvalidBoundary {
            root: root.to_path_buf(),
        },
    })
}

#[cfg(test)]
async fn parse_local_config(config_path: &Path) -> Option<WorkspaceMapping> {
    parse_local_mapping(config_path).await
}

async fn parse_local_mapping(config_path: &Path) -> Option<WorkspaceMapping> {
    let project_root = config_path.parent()?.parent()?;
    if safe_checkout_config_path(project_root, false)
        .ok()?
        .as_path()
        != config_path
    {
        return None;
    }
    let content = tokio::fs::read_to_string(config_path).await.ok()?;
    let config = parse_value_without_duplicate_keys(&content).ok()?;

    // A repository-local config is only authoritative for the checkout that
    // created it. Without this binding, copying a directory (including its
    // ignored `.contextstream` folder) can silently route future context and
    // ingest calls into the source repository's project. Legacy/unbound
    // configs fall through to the canonical global path mapping instead.
    let current_root = std::fs::canonicalize(project_root)
        .unwrap_or_else(|_| project_root.to_path_buf())
        .to_string_lossy()
        .into_owned();
    let stored_root = config
        .get("checkout_root")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|root| !root.is_empty());
    if stored_root.is_none_or(|root| Path::new(root) != Path::new(&current_root)) {
        return None;
    }

    let workspace_id = config
        .get("workspace_id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok())?;

    let workspace_name = config
        .get("workspace_name")
        .and_then(|v| v.as_str())
        .unwrap_or("Unknown")
        .to_string();

    let project_id = match config.get("project_id") {
        None => None,
        Some(value) => Some(
            value
                .as_str()
                .and_then(|value| Uuid::parse_str(value.trim()).ok())?,
        ),
    };

    let project_name = config
        .get("project_name")
        .and_then(|v| v.as_str())
        .map(String::from);

    let identity_kind =
        CheckoutIdentityKind::from_config_value(config.get(CheckoutIdentityKind::FIELD)).ok()?;
    let configured_fingerprint = config
        .get("repository_fingerprint")
        .and_then(|value| value.as_str())
        .and_then(|value| RepositoryFingerprint::parse(value).ok())?;
    let current_fingerprint = current_checkout_fingerprint(project_root, identity_kind).ok()?;
    if configured_fingerprint != current_fingerprint {
        return None;
    }
    match identity_kind {
        CheckoutIdentityKind::Git => {
            validate_configured_remote_identity(
                project_root,
                config.get("repository_remote_identity"),
            )
            .ok()?;
        }
        CheckoutIdentityKind::Folder if config.get("repository_remote_identity").is_none() => {}
        CheckoutIdentityKind::Folder => return None,
    }

    Some(WorkspaceMapping {
        workspace_id,
        workspace_name,
        project_id,
        project_name,
    })
}

/// Resolve workspace from global mappings.
async fn resolve_global_mapping(
    folder_path: &str,
    exact_path_only: bool,
) -> Option<WorkspaceMapping> {
    let home = dirs::home_dir()?;
    let modern_path = home.join(".contextstream").join("mappings.json");
    let legacy_path = home.join(".contextstream-mappings.json");

    // 1) Prefer modern mappings store: ~/.contextstream/mappings.json
    if modern_path.exists() {
        let content = tokio::fs::read_to_string(&modern_path).await.ok()?;
        let parsed = parse_value_without_duplicate_keys(&content).ok()?;

        let mappings_array = parsed
            .get("mappings")
            .and_then(|v| v.as_array())
            .cloned()
            .or_else(|| parsed.as_array().cloned())
            .unwrap_or_default();

        if let Some(mapping) = select_best_mapping(&mappings_array, folder_path, exact_path_only) {
            return parse_workspace_mapping(mapping, folder_path);
        }
    }

    // 3) Backward compatibility: ~/.contextstream-mappings.json (legacy TS format)
    if legacy_path.exists() {
        let content = tokio::fs::read_to_string(&legacy_path).await.ok()?;
        let parsed = parse_value_without_duplicate_keys(&content).ok()?;
        let mappings_array = parsed
            .as_array()
            .cloned()
            .or_else(|| parsed.get("mappings").and_then(|v| v.as_array()).cloned())
            .unwrap_or_default();
        if let Some(mapping) = select_best_mapping(&mappings_array, folder_path, exact_path_only) {
            return parse_workspace_mapping(mapping, folder_path);
        }
    }

    None
}

fn select_best_mapping<'a>(
    mappings: &'a [serde_json::Value],
    folder_path: &str,
    exact_path_only: bool,
) -> Option<&'a serde_json::Value> {
    let normalized_path = normalize_path(folder_path);
    let mut best_match: Option<(usize, &serde_json::Value)> = None;
    let mut best_match_is_ambiguous = false;

    for mapping in mappings {
        let raw_path = mapping
            .get("path")
            .and_then(|v| v.as_str())
            .or_else(|| mapping.get("pattern").and_then(|v| v.as_str()));
        let Some(raw_path) = raw_path else {
            continue;
        };

        let normalized_mapping_path = normalize_mapping_prefix(raw_path);
        if normalized_mapping_path.is_empty() {
            continue;
        }

        let matches = normalized_path == normalized_mapping_path
            || (!exact_path_only
                && normalized_path.starts_with(&(normalized_mapping_path.clone() + "/")));
        if matches {
            let len = normalized_mapping_path.len();
            if best_match.is_none_or(|(best_len, _)| len > best_len) {
                best_match = Some((len, mapping));
                best_match_is_ambiguous = false;
            } else if best_match.is_some_and(|(best_len, _)| len == best_len) {
                best_match_is_ambiguous = true;
            }
        }
    }

    (!best_match_is_ambiguous)
        .then_some(best_match)
        .flatten()
        .map(|(_, mapping)| mapping)
}

fn normalize_mapping_prefix(raw: &str) -> String {
    let normalized = normalize_path(raw);
    if normalized.ends_with("/*") {
        return normalized.trim_end_matches("/*").to_string();
    }
    normalized
        .trim_end_matches('*')
        .trim_end_matches('/')
        .to_string()
}

fn parse_workspace_mapping(
    mapping: &serde_json::Value,
    folder_path: &str,
) -> Option<WorkspaceMapping> {
    let workspace_id = mapping
        .get("workspace_id")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok())?;
    let workspace_name = mapping
        .get("workspace_name")
        .and_then(|v| v.as_str())
        .unwrap_or("Unknown")
        .to_string();
    let project_id = match mapping.get("project_id") {
        None => None,
        Some(value) => Some(
            value
                .as_str()
                .and_then(|value| Uuid::parse_str(value.trim()).ok())?,
        ),
    };
    let project_name = mapping
        .get("project_name")
        .and_then(|v| v.as_str())
        .map(String::from);

    // Project-bearing path mappings are content-capable routing hints and must
    // prove they still describe the same repository. Legacy mappings without a
    // fingerprint remain usable only for workspace-only routing. If a
    // workspace-only entry does carry a fingerprint, honor it and reject drift.
    let identity_kind =
        CheckoutIdentityKind::from_config_value(mapping.get(CheckoutIdentityKind::FIELD)).ok()?;
    let configured_fingerprint = mapping
        .get("repository_fingerprint")
        .and_then(|value| value.as_str());
    match (project_id, configured_fingerprint) {
        (Some(_), None) => return None,
        (_, Some(value)) => {
            let configured = RepositoryFingerprint::parse(value).ok()?;
            let current = current_checkout_fingerprint(folder_path, identity_kind).ok()?;
            if configured != current {
                return None;
            }
        }
        (None, None) => {}
    }
    match identity_kind {
        CheckoutIdentityKind::Git => {
            validate_configured_remote_identity(
                folder_path,
                mapping.get("repository_remote_identity"),
            )
            .ok()?;
        }
        CheckoutIdentityKind::Folder if mapping.get("repository_remote_identity").is_none() => {}
        CheckoutIdentityKind::Folder => return None,
    }
    Some(WorkspaceMapping {
        workspace_id,
        workspace_name,
        project_id,
        project_name,
    })
}

/// Normalize a path for comparison.
fn normalize_path(path: &str) -> String {
    let mut normalized = path.replace('\\', "/");

    // Expand ~ to home directory
    if normalized.starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            normalized = format!("{}{}", home.display(), &normalized[1..]);
        }
    }

    // Remove trailing slash
    if normalized.ends_with('/') && normalized.len() > 1 {
        normalized.pop();
    }

    normalized
}

/// Auto-initialize session based on folder path.
pub async fn auto_initialize(
    client: &ContextStreamClient,
    folder_path: Option<&str>,
) -> Result<Option<(Uuid, Option<Uuid>, String)>> {
    let Some(path) = folder_path else {
        return Ok(None);
    };

    let Some(mapping) = resolve_workspace(path).await else {
        return Ok(None);
    };

    // Initialize session via API
    let params = SessionInitParams {
        workspace_id: Some(mapping.workspace_id),
        project_id: mapping.project_id,
        folder_path: Some(path.to_string()),
        repository_url: crate::current_repository_canonical_url(path).ok().flatten(),
        context_hint: None,
        // Auto-init resolves from the user-authored .contextstream mapping,
        // so the backend may learn this folder→project binding.
        scope_provenance: mapping.project_id.map(|_| "local_mapping".to_string()),
        ..Default::default()
    };

    let initialized = client.session_init_best_effort(params).await?;
    if initialized.is_none() {
        tracing::debug!(
            "auto-initialize suppressed non-blocking ParserError from session init response"
        );
    }

    // Update client defaults
    client
        .set_defaults(Some(mapping.workspace_id), mapping.project_id)
        .await;

    Ok(Some((
        mapping.workspace_id,
        mapping.project_id,
        mapping.workspace_name,
    )))
}

/// Dependency for home directory resolution.
mod dirs {
    use std::path::PathBuf;

    pub fn home_dir() -> Option<PathBuf> {
        #[cfg(target_os = "windows")]
        {
            std::env::var_os("USERPROFILE").map(PathBuf::from)
        }

        #[cfg(not(target_os = "windows"))]
        {
            std::env::var_os("HOME").map(PathBuf::from)
        }
    }
}

/// Refresh an already-established folder binding.
///
/// This ordinary init/context/hook path is deliberately incapable of creating
/// a binding, changing workspace/project IDs, copying a fingerprint, or minting
/// repository identity. It only updates descriptive names/timestamps when the
/// existing local config, its recorded Git/folder marker, and the existing
/// global entry agree.
pub async fn persist_folder_mapping(
    folder_path: &str,
    workspace_id: Uuid,
    workspace_name: &str,
    project_id: Option<Uuid>,
    project_name: Option<&str>,
) {
    let Ok(binding) = validate_checkout_scope(folder_path, Some(workspace_id), project_id) else {
        return;
    };
    let repository_identity = RepositoryBindingIdentity {
        fingerprint: &binding.repository_fingerprint,
        remote: binding.repository_remote_identity.as_ref(),
        checkout_id: &binding.checkout_id,
        kind: binding.identity_kind,
    };
    if !refresh_local_config_if_valid(
        folder_path,
        workspace_id,
        workspace_name,
        project_id,
        project_name,
        repository_identity,
    )
    .await
    {
        return;
    }

    refresh_global_mapping_if_valid(
        folder_path,
        workspace_id,
        workspace_name,
        project_id,
        project_name,
        repository_identity,
    )
    .await;
}

#[derive(Clone, Debug)]
enum CheckoutIdCandidate {
    Explicit(CheckoutId),
    Legacy(CheckoutId),
}

impl CheckoutIdCandidate {
    fn into_id(self) -> CheckoutId {
        match self {
            Self::Explicit(id) | Self::Legacy(id) => id,
        }
    }

    fn explicit(&self) -> Option<&CheckoutId> {
        match self {
            Self::Explicit(id) => Some(id),
            Self::Legacy(_) => None,
        }
    }
}

fn trusted_checkout_id_candidate(
    value: &serde_json::Value,
    root_field: &str,
    checkout_root: &Path,
    fingerprint: &RepositoryFingerprint,
    identity_kind: CheckoutIdentityKind,
) -> std::result::Result<Option<CheckoutIdCandidate>, CheckoutIdentityError> {
    let expected_root = normalize_path(&checkout_root.to_string_lossy());
    if value
        .get(root_field)
        .and_then(serde_json::Value::as_str)
        .map(normalize_path)
        .as_deref()
        != Some(expected_root.as_str())
    {
        return Ok(None);
    }
    if value
        .get("repository_fingerprint")
        .and_then(serde_json::Value::as_str)
        .and_then(|raw| RepositoryFingerprint::parse(raw).ok())
        .as_ref()
        != Some(fingerprint)
    {
        return Ok(None);
    }
    if CheckoutIdentityKind::from_config_value(value.get(CheckoutIdentityKind::FIELD))?
        != identity_kind
    {
        return Ok(None);
    }

    match value.get("checkout_id") {
        Some(raw) => raw
            .as_str()
            .ok_or_else(|| CheckoutIdentityError::InvalidCheckoutId {
                value: raw.to_string(),
            })
            .and_then(CheckoutId::parse)
            .map(CheckoutIdCandidate::Explicit)
            .map(Some),
        None => Ok(Some(CheckoutIdCandidate::Legacy(
            CheckoutId::for_legacy_binding(checkout_root, fingerprint),
        ))),
    }
}

fn local_checkout_id_candidate(
    checkout_root: &Path,
    fingerprint: &RepositoryFingerprint,
    identity_kind: CheckoutIdentityKind,
) -> std::result::Result<Option<CheckoutIdCandidate>, CheckoutIdentityError> {
    let config_path = safe_checkout_config_path(checkout_root, true)?;
    let Some(raw) = read_regular_text_snapshot(&config_path)? else {
        return Ok(None);
    };
    let config = parse_value_without_duplicate_keys(&raw)
        .ok()
        .filter(serde_json::Value::is_object)
        .ok_or(CheckoutIdentityError::InvalidLocalConfig(config_path))?;
    trusted_checkout_id_candidate(
        &config,
        "checkout_root",
        checkout_root,
        fingerprint,
        identity_kind,
    )
}

fn global_checkout_id_candidate(
    path: Option<&Path>,
    checkout_root: &Path,
    fingerprint: &RepositoryFingerprint,
    identity_kind: CheckoutIdentityKind,
) -> std::result::Result<Option<CheckoutIdCandidate>, CheckoutIdentityError> {
    let Some(path) = path else {
        return Ok(None);
    };
    let Some(raw) = read_regular_text_snapshot(path)? else {
        return Ok(None);
    };
    let root = parse_value_without_duplicate_keys(&raw)
        .map_err(|_| CheckoutIdentityError::InvalidGlobalMappings(path.to_path_buf()))?;
    let mappings = if let Some(mappings) = root.as_array() {
        mappings
    } else if let Some(object) = root.as_object() {
        match object.get("mappings") {
            None => return Ok(None),
            Some(mappings) => mappings
                .as_array()
                .ok_or_else(|| CheckoutIdentityError::InvalidGlobalMappings(path.to_path_buf()))?,
        }
    } else {
        return Err(CheckoutIdentityError::InvalidGlobalMappings(
            path.to_path_buf(),
        ));
    };

    let mut selected: Option<CheckoutIdCandidate> = None;
    for mapping in mappings {
        let Some(candidate) = trusted_checkout_id_candidate(
            mapping,
            "path",
            checkout_root,
            fingerprint,
            identity_kind,
        )?
        else {
            continue;
        };
        if let (Some(existing), Some(incoming)) = (
            selected.as_ref().and_then(CheckoutIdCandidate::explicit),
            candidate.explicit(),
        ) {
            if existing != incoming {
                return Err(CheckoutIdentityError::ConflictingCheckoutIds {
                    local: existing.clone(),
                    global: incoming.clone(),
                });
            }
        }
        if selected
            .as_ref()
            .is_none_or(|existing| existing.explicit().is_none())
            || candidate.explicit().is_some()
        {
            selected = Some(candidate);
        }
    }
    Ok(selected)
}

fn resolve_checkout_id_for_establishment(
    checkout_root: &Path,
    fingerprint: &RepositoryFingerprint,
    identity_kind: CheckoutIdentityKind,
    global_mapping_path: Option<&Path>,
) -> std::result::Result<CheckoutId, CheckoutIdentityError> {
    Ok(resolve_existing_checkout_id(
        checkout_root,
        fingerprint,
        identity_kind,
        global_mapping_path,
    )?
    .unwrap_or_else(CheckoutId::generate))
}

fn resolve_existing_checkout_id(
    checkout_root: &Path,
    fingerprint: &RepositoryFingerprint,
    identity_kind: CheckoutIdentityKind,
    global_mapping_path: Option<&Path>,
) -> std::result::Result<Option<CheckoutId>, CheckoutIdentityError> {
    let local = local_checkout_id_candidate(checkout_root, fingerprint, identity_kind)?;
    let global = global_checkout_id_candidate(
        global_mapping_path,
        checkout_root,
        fingerprint,
        identity_kind,
    )?;

    match (
        local.as_ref().and_then(CheckoutIdCandidate::explicit),
        global.as_ref().and_then(CheckoutIdCandidate::explicit),
    ) {
        (Some(local), Some(global)) if local != global => {
            Err(CheckoutIdentityError::ConflictingCheckoutIds {
                local: local.clone(),
                global: global.clone(),
            })
        }
        (Some(local), _) => Ok(Some(local.clone())),
        (_, Some(global)) => Ok(Some(global.clone())),
        _ => Ok(local.or(global).map(CheckoutIdCandidate::into_id)),
    }
}

/// Preserve checkout continuity when an explicitly bound folder becomes a Git
/// checkout. This reads the old folder marker/config only; it does not change
/// identity on ordinary init, watcher, or hook paths. Missing old state simply
/// means there is no checkout ID to carry forward.
fn folder_checkout_id_for_git_upgrade(
    checkout_root: &Path,
    global_mapping_path: Option<&Path>,
) -> std::result::Result<Option<CheckoutId>, CheckoutIdentityError> {
    let fingerprint =
        match current_checkout_fingerprint(checkout_root, CheckoutIdentityKind::Folder) {
            Ok(fingerprint) => fingerprint,
            Err(CheckoutIdentityError::MissingRepositoryFingerprint(_))
            | Err(CheckoutIdentityError::MissingLocalConfig(_)) => return Ok(None),
            Err(error) => return Err(error),
        };
    resolve_existing_checkout_id(
        checkout_root,
        &fingerprint,
        CheckoutIdentityKind::Folder,
        global_mapping_path,
    )
}

/// Explicitly establish or rebind a checkout after the caller has verified API
/// ownership and explicit user intent.
///
/// This is the only mapping API that may mint a fingerprint, create
/// config/mapping records, or change scope IDs. Git checkouts use the common
/// Git marker; explicitly selected non-Git folders use a folder-local marker
/// so automatic sync can safely begin when the first file appears.
pub async fn establish_folder_binding(
    folder_path: &str,
    workspace_id: Uuid,
    workspace_name: &str,
    project_id: Option<Uuid>,
    project_name: Option<&str>,
) -> std::result::Result<RepositoryFingerprint, CheckoutIdentityError> {
    let dir = Path::new(folder_path);
    let checkout_root = std::fs::canonicalize(dir)
        .map_err(|error| CheckoutIdentityError::io("canonicalize checkout root", dir, error))?;
    if !checkout_root.is_dir() {
        return Err(CheckoutIdentityError::InvalidCheckoutRoot(
            dir.to_path_buf(),
        ));
    }
    let identity_kind = identity_kind_for_establishment(&checkout_root)?;
    let global_mapping_path =
        dirs::home_dir().map(|home| home.join(".contextstream/mappings.json"));
    let upgrade_checkout_id = if identity_kind == CheckoutIdentityKind::Git {
        folder_checkout_id_for_git_upgrade(&checkout_root, global_mapping_path.as_deref())?
    } else {
        None
    };
    let fingerprint = ensure_checkout_fingerprint(&checkout_root, identity_kind)?;
    let remote_identity = match identity_kind {
        CheckoutIdentityKind::Git => current_repository_remote_identity(&checkout_root)?,
        CheckoutIdentityKind::Folder => None,
    };
    let checkout_id = match upgrade_checkout_id {
        Some(checkout_id) => checkout_id,
        None => resolve_checkout_id_for_establishment(
            &checkout_root,
            &fingerprint,
            identity_kind,
            global_mapping_path.as_deref(),
        )?,
    };
    let repository_identity = RepositoryBindingIdentity {
        fingerprint: &fingerprint,
        remote: remote_identity.as_ref(),
        checkout_id: &checkout_id,
        kind: identity_kind,
    };
    if let Some(path) = global_mapping_path {
        write_established_global_mapping(
            &path,
            &checkout_root,
            workspace_id,
            workspace_name,
            project_id,
            project_name,
            repository_identity,
        )
        .await?;
    }
    // The checkout-local config is the final authorization gate for automatic
    // content writers, so commit it last. If global persistence fails, a
    // partially completed explicit operation cannot accidentally authorize
    // background uploads.
    write_established_local_config(
        &checkout_root,
        workspace_id,
        workspace_name,
        project_id,
        project_name,
        repository_identity,
    )
    .await?;
    Ok(fingerprint)
}

#[derive(Clone, Copy)]
struct RepositoryBindingIdentity<'a> {
    fingerprint: &'a RepositoryFingerprint,
    remote: Option<&'a RepositoryRemoteIdentity>,
    checkout_id: &'a CheckoutId,
    kind: CheckoutIdentityKind,
}

async fn write_established_local_config(
    checkout_root: &Path,
    workspace_id: Uuid,
    workspace_name: &str,
    project_id: Option<Uuid>,
    project_name: Option<&str>,
    repository_identity: RepositoryBindingIdentity<'_>,
) -> std::result::Result<(), CheckoutIdentityError> {
    let config_dir = checkout_root.join(".contextstream");
    let config_path = safe_checkout_config_path(checkout_root, true)?;
    let existing = read_regular_text_snapshot(&config_path)?;
    let mut config = match existing.as_deref() {
        Some(raw) => parse_value_without_duplicate_keys(raw)
            .ok()
            .filter(serde_json::Value::is_object)
            .ok_or_else(|| CheckoutIdentityError::InvalidLocalConfig(config_path.clone()))?,
        None => serde_json::json!({}),
    };
    let workspace_id = workspace_id.to_string();
    let checkout_root_text = checkout_root.to_string_lossy().into_owned();
    let project_id = project_id.map(|value| value.to_string());
    let remote_identity = repository_identity.remote.map(|value| value.as_str());
    let association_changed = config
        .get("workspace_id")
        .and_then(serde_json::Value::as_str)
        != Some(workspace_id.as_str())
        || config
            .get("workspace_name")
            .and_then(serde_json::Value::as_str)
            != Some(workspace_name)
        || config
            .get("checkout_root")
            .and_then(serde_json::Value::as_str)
            != Some(checkout_root_text.as_str())
        || config
            .get("checkout_id")
            .and_then(serde_json::Value::as_str)
            != Some(repository_identity.checkout_id.as_str())
        || config
            .get("repository_fingerprint")
            .and_then(serde_json::Value::as_str)
            != Some(repository_identity.fingerprint.as_str())
        || CheckoutIdentityKind::from_config_value(config.get(CheckoutIdentityKind::FIELD)).ok()
            != Some(repository_identity.kind)
        || config
            .get("repository_remote_identity")
            .and_then(serde_json::Value::as_str)
            != remote_identity
        || config.get("project_id").and_then(serde_json::Value::as_str) != project_id.as_deref()
        || config
            .get("project_name")
            .and_then(serde_json::Value::as_str)
            != project_name;
    let has_valid_associated_at = config
        .get("associated_at")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|value| chrono::DateTime::parse_from_rfc3339(value).is_ok());

    config["workspace_id"] = serde_json::json!(workspace_id);
    config["workspace_name"] = serde_json::json!(workspace_name);
    config["checkout_root"] = serde_json::json!(checkout_root_text);
    config["checkout_id"] = serde_json::json!(repository_identity.checkout_id.as_str());
    config["repository_fingerprint"] = serde_json::json!(repository_identity.fingerprint.as_str());
    config[CheckoutIdentityKind::FIELD] = serde_json::json!(repository_identity.kind.as_str());
    match remote_identity {
        Some(identity) => config["repository_remote_identity"] = serde_json::json!(identity),
        None => {
            if let Some(object) = config.as_object_mut() {
                object.remove("repository_remote_identity");
            }
        }
    }
    if let Some(pid) = project_id {
        config["project_id"] = serde_json::json!(pid);
    } else if let Some(obj) = config.as_object_mut() {
        obj.remove("project_id");
    }
    if let Some(pname) = project_name {
        config["project_name"] = serde_json::json!(pname);
    } else {
        if let Some(obj) = config.as_object_mut() {
            obj.remove("project_name");
        }
    }
    if association_changed || !has_valid_associated_at {
        config["associated_at"] = serde_json::json!(chrono::Utc::now().to_rfc3339());
    }

    tokio::fs::create_dir_all(&config_dir)
        .await
        .map_err(|error| {
            CheckoutIdentityError::io("create checkout config directory", &config_dir, error)
        })?;
    // Re-check after directory creation so a concurrently inserted symlink or
    // non-regular config never becomes the destination of the explicit write.
    let config_path = safe_checkout_config_path(checkout_root, true)?;
    let rendered = serde_json::to_string_pretty(&config).map_err(|error| {
        CheckoutIdentityError::io(
            "serialize checkout-local config",
            &config_path,
            std::io::Error::new(ErrorKind::InvalidData, error),
        )
    })?;
    write_regular_text_snapshot(&config_path, existing.as_deref(), &rendered)?;
    Ok(())
}

async fn refresh_local_config_if_valid(
    folder_path: &str,
    workspace_id: Uuid,
    workspace_name: &str,
    project_id: Option<Uuid>,
    project_name: Option<&str>,
    repository_identity: RepositoryBindingIdentity<'_>,
) -> bool {
    let Ok(config_path) = safe_checkout_config_path(Path::new(folder_path), false) else {
        return false;
    };
    let Ok(Some(existing)) = read_regular_text_snapshot(&config_path) else {
        return false;
    };
    let Ok(mut config) = parse_value_without_duplicate_keys(&existing) else {
        return false;
    };
    if !json_scope_matches(
        &config,
        folder_path,
        workspace_id,
        project_id,
        repository_identity,
        true,
    ) {
        return false;
    }
    config["workspace_name"] = serde_json::json!(workspace_name);
    config["checkout_id"] = serde_json::json!(repository_identity.checkout_id.as_str());
    match project_name {
        Some(name) => config["project_name"] = serde_json::json!(name),
        None => {
            if let Some(object) = config.as_object_mut() {
                object.remove("project_name");
            }
        }
    }
    config["associated_at"] = serde_json::json!(chrono::Utc::now().to_rfc3339());
    let Ok(rendered) = serde_json::to_string_pretty(&config) else {
        return false;
    };
    write_regular_text_snapshot(&config_path, Some(&existing), &rendered).is_ok()
}

async fn refresh_global_mapping_if_valid(
    folder_path: &str,
    workspace_id: Uuid,
    workspace_name: &str,
    project_id: Option<Uuid>,
    project_name: Option<&str>,
    repository_identity: RepositoryBindingIdentity<'_>,
) {
    let _write_guard = global_mapping_write_lock().lock().await;
    let Some(path) = dirs::home_dir().map(|home| home.join(".contextstream/mappings.json")) else {
        return;
    };
    let Ok(Some(existing)) = read_regular_text_snapshot(&path) else {
        return;
    };
    let Ok(mut root) = parse_value_without_duplicate_keys(&existing) else {
        return;
    };
    let Some(mappings) = root
        .as_object_mut()
        .and_then(|object| object.get_mut("mappings"))
        .and_then(serde_json::Value::as_array_mut)
    else {
        return;
    };
    let Ok(canonical_folder_path) = std::fs::canonicalize(folder_path) else {
        return;
    };
    let normalized_folder_path = normalize_path(&canonical_folder_path.to_string_lossy());
    let matching_indices = mappings
        .iter()
        .enumerate()
        .filter_map(|(index, mapping)| {
            mapping
                .get("path")
                .and_then(|value| value.as_str())
                .is_some_and(|path| normalize_path(path) == normalized_folder_path)
                .then_some(index)
        })
        .collect::<Vec<_>>();
    if matching_indices.len() != 1 {
        return;
    }
    let mapping = &mut mappings[matching_indices[0]];
    if !json_scope_matches(
        mapping,
        folder_path,
        workspace_id,
        project_id,
        repository_identity,
        false,
    ) {
        return;
    }
    mapping["workspace_name"] = serde_json::json!(workspace_name);
    mapping["checkout_id"] = serde_json::json!(repository_identity.checkout_id.as_str());
    match project_name {
        Some(name) => mapping["project_name"] = serde_json::json!(name),
        None => {
            if let Some(object) = mapping.as_object_mut() {
                object.remove("project_name");
            }
        }
    }
    mapping["updated_at"] = serde_json::json!(chrono::Utc::now().to_rfc3339());
    if let Ok(rendered) = serde_json::to_string_pretty(&root) {
        let _ = write_regular_text_snapshot(&path, Some(&existing), &rendered);
    }
}

fn json_scope_matches(
    value: &serde_json::Value,
    folder_path: &str,
    workspace_id: Uuid,
    project_id: Option<Uuid>,
    repository_identity: RepositoryBindingIdentity<'_>,
    require_checkout_root: bool,
) -> bool {
    if require_checkout_root {
        let Ok(canonical) = std::fs::canonicalize(folder_path) else {
            return false;
        };
        if value.get("checkout_root").and_then(|field| field.as_str())
            != Some(canonical.to_string_lossy().as_ref())
        {
            return false;
        }
    }
    value
        .get("workspace_id")
        .and_then(|field| field.as_str())
        .and_then(|field| Uuid::parse_str(field.trim()).ok())
        == Some(workspace_id)
        && match (project_id, value.get("project_id")) {
            (None, None) => true,
            (Some(expected), Some(field)) => {
                field
                    .as_str()
                    .and_then(|field| Uuid::parse_str(field.trim()).ok())
                    == Some(expected)
            }
            _ => false,
        }
        && value
            .get("repository_fingerprint")
            .and_then(|field| field.as_str())
            .and_then(|field| RepositoryFingerprint::parse(field).ok())
            .as_ref()
            == Some(repository_identity.fingerprint)
        && CheckoutIdentityKind::from_config_value(value.get(CheckoutIdentityKind::FIELD)).ok()
            == Some(repository_identity.kind)
        && match value.get("checkout_id") {
            None => true,
            Some(field) => {
                field
                    .as_str()
                    .and_then(|field| CheckoutId::parse(field).ok())
                    .as_ref()
                    == Some(repository_identity.checkout_id)
            }
        }
        && match (
            repository_identity.remote,
            value.get("repository_remote_identity"),
        ) {
            (None, None) => true,
            (Some(expected), Some(field)) => {
                field
                    .as_str()
                    .and_then(|field| RepositoryRemoteIdentity::parse(field).ok())
                    .as_ref()
                    == Some(expected)
            }
            _ => false,
        }
}

async fn write_established_global_mapping(
    path: &Path,
    checkout_root: &Path,
    workspace_id: Uuid,
    workspace_name: &str,
    project_id: Option<Uuid>,
    project_name: Option<&str>,
    repository_identity: RepositoryBindingIdentity<'_>,
) -> std::result::Result<(), CheckoutIdentityError> {
    let _write_guard = global_mapping_write_lock().lock().await;
    let existing = read_regular_text_snapshot(path)?;
    let mut root = match existing.as_deref() {
        Some(raw) => parse_value_without_duplicate_keys(raw)
            .map_err(|_| CheckoutIdentityError::InvalidGlobalMappings(path.to_path_buf()))?,
        None => serde_json::json!({}),
    };

    if root.is_array() {
        root = serde_json::json!({ "mappings": root });
    } else if !root.is_object() {
        return Err(CheckoutIdentityError::InvalidGlobalMappings(
            path.to_path_buf(),
        ));
    }

    let mappings = root.as_object_mut().and_then(|obj| {
        obj.entry("mappings")
            .or_insert_with(|| serde_json::json!([]))
            .as_array_mut()
    });
    let Some(mappings) = mappings else {
        return Err(CheckoutIdentityError::InvalidGlobalMappings(
            path.to_path_buf(),
        ));
    };

    let normalized = normalize_path(&checkout_root.to_string_lossy());

    let existing_index = mappings.iter().position(|mapping| {
        mapping
            .get("path")
            .and_then(|value| value.as_str())
            .map(normalize_path)
            .as_ref()
            == Some(&normalized)
    });
    let mut entry = existing_index
        .and_then(|index| mappings.get(index).cloned())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    let checkout_root_text = checkout_root.to_string_lossy().into_owned();
    let workspace_id = workspace_id.to_string();
    let project_id = project_id.map(|value| value.to_string());
    let remote_identity = repository_identity.remote.map(|value| value.as_str());
    let association_changed = entry.get("path").and_then(serde_json::Value::as_str)
        != Some(checkout_root_text.as_str())
        || entry
            .get("workspace_id")
            .and_then(serde_json::Value::as_str)
            != Some(workspace_id.as_str())
        || entry
            .get("workspace_name")
            .and_then(serde_json::Value::as_str)
            != Some(workspace_name)
        || entry.get("checkout_id").and_then(serde_json::Value::as_str)
            != Some(repository_identity.checkout_id.as_str())
        || entry
            .get("repository_fingerprint")
            .and_then(serde_json::Value::as_str)
            != Some(repository_identity.fingerprint.as_str())
        || CheckoutIdentityKind::from_config_value(entry.get(CheckoutIdentityKind::FIELD)).ok()
            != Some(repository_identity.kind)
        || entry
            .get("repository_remote_identity")
            .and_then(serde_json::Value::as_str)
            != remote_identity
        || entry.get("project_id").and_then(serde_json::Value::as_str) != project_id.as_deref()
        || entry
            .get("project_name")
            .and_then(serde_json::Value::as_str)
            != project_name;
    let has_valid_updated_at = entry
        .get("updated_at")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|value| chrono::DateTime::parse_from_rfc3339(value).is_ok());

    entry["path"] = serde_json::json!(checkout_root_text);
    entry["workspace_id"] = serde_json::json!(workspace_id);
    entry["workspace_name"] = serde_json::json!(workspace_name);
    entry["checkout_id"] = serde_json::json!(repository_identity.checkout_id.as_str());
    entry["repository_fingerprint"] = serde_json::json!(repository_identity.fingerprint.as_str());
    entry[CheckoutIdentityKind::FIELD] = serde_json::json!(repository_identity.kind.as_str());

    if let Some(pid) = project_id {
        entry["project_id"] = serde_json::json!(pid);
    } else if let Some(object) = entry.as_object_mut() {
        object.remove("project_id");
    }
    if let Some(pname) = project_name {
        entry["project_name"] = serde_json::json!(pname);
    } else if let Some(object) = entry.as_object_mut() {
        object.remove("project_name");
    }
    if let Some(identity) = remote_identity {
        entry["repository_remote_identity"] = serde_json::json!(identity);
    } else if let Some(object) = entry.as_object_mut() {
        object.remove("repository_remote_identity");
    }
    if association_changed || !has_valid_updated_at {
        entry["updated_at"] = serde_json::json!(chrono::Utc::now().to_rfc3339());
    }

    if let Some(index) = existing_index {
        mappings[index] = entry;
    } else {
        mappings.push(entry);
    }

    let rendered = serde_json::to_string_pretty(&root).map_err(|error| {
        CheckoutIdentityError::io(
            "serialize global mappings",
            path,
            std::io::Error::new(ErrorKind::InvalidData, error),
        )
    })?;
    write_regular_text_snapshot(path, existing.as_deref(), &rendered)?;
    Ok(())
}

/// Remove the global mapping entry (in `~/.contextstream/mappings.json`) for an
/// exact folder path. Returns true if an entry was removed.
///
/// Used to make a local de-index "stick": once the mapping is gone, `init`/
/// `context` no longer auto-resolve the folder to the project, and the keep-warm
/// daemon (which enumerates `mappings.json` ∪ `indexed-projects.json`) no longer
/// re-seeds it. Callers should also clear the local index registry
/// (`ContextStreamClient::clear_index_status`) and, for the active folder, drop
/// the session's project scope so the in-session aging tick doesn't re-seed.
pub async fn remove_global_mapping(folder_path: &str) -> bool {
    let _write_guard = global_mapping_write_lock().lock().await;
    let Some(path) = dirs::home_dir().map(|h| h.join(".contextstream").join("mappings.json"))
    else {
        return false;
    };
    let Ok(Some(existing)) = read_regular_text_snapshot(&path) else {
        return false;
    };
    let Ok(parsed) = parse_value_without_duplicate_keys(&existing) else {
        return false;
    };
    let mut root = parsed;
    if !root.is_object() {
        root = serde_json::json!({ "mappings": root });
    }
    let normalized = normalize_path(folder_path);
    let mut removed = false;
    if let Some(mappings) = root
        .as_object_mut()
        .and_then(|obj| obj.get_mut("mappings"))
        .and_then(|v| v.as_array_mut())
    {
        let before = mappings.len();
        mappings.retain(|m| {
            m.get("path")
                .and_then(|v| v.as_str())
                .map(normalize_path)
                .as_ref()
                != Some(&normalized)
        });
        removed = mappings.len() != before;
    }
    if removed {
        let Ok(rendered) = serde_json::to_string_pretty(&root) else {
            return false;
        };
        if write_regular_text_snapshot(&path, Some(&existing), &rendered).is_err() {
            return false;
        }
    }
    removed
}

/// Clear the project association from a per-folder `.contextstream/config.json`
/// (removes `project_id`/`project_name`; any workspace association is kept).
/// Returns true if the file existed and carried a project that was removed.
///
/// Needed by `forget_local`: the per-folder config is one of the fallbacks the
/// PostToolUse persist path uses to re-resolve scope, so leaving it in place
/// would let the mapping be re-seeded right after it is removed. A workspace-only
/// config does not trigger keep-warm (which requires a project_id).
pub async fn clear_local_config_project(folder_path: &str) -> bool {
    let Ok(config_path) = safe_checkout_config_path(Path::new(folder_path), false) else {
        return false;
    };
    let Ok(Some(existing)) = read_regular_text_snapshot(&config_path) else {
        return false;
    };
    let Ok(parsed) = parse_value_without_duplicate_keys(&existing) else {
        return false;
    };
    let Some(mut obj) = parsed.as_object().cloned() else {
        return false;
    };
    if !obj.contains_key("project_id") && !obj.contains_key("project_name") {
        return false;
    }
    obj.remove("project_id");
    obj.remove("project_name");
    obj.insert(
        "associated_at".to_string(),
        serde_json::json!(chrono::Utc::now().to_rfc3339()),
    );
    let Ok(rendered) = serde_json::to_string_pretty(&serde_json::Value::Object(obj)) else {
        return false;
    };
    write_regular_text_snapshot(&config_path, Some(&existing), &rendered).is_ok()
}

#[cfg(test)]
mod tests {
    use super::{
        checkout_binding_matches, checkout_binding_workspace, folder_checkout_id_for_git_upgrade,
        parse_local_config, parse_workspace_mapping, persist_folder_mapping,
        refresh_local_config_if_valid, resolve_checkout_id_for_establishment, resolve_local_config,
        select_best_mapping, write_established_global_mapping, write_established_local_config,
        LocalConfigResolution, RepositoryBindingIdentity,
    };
    use crate::checkout_identity::{
        current_repository_fingerprint, current_repository_remote_identity,
        ensure_checkout_fingerprint, ensure_folder_fingerprint, ensure_repository_fingerprint,
        identity_kind_for_establishment, validate_checkout_binding, CheckoutId,
        CheckoutIdentityKind, RepositoryFingerprint,
    };
    use serde_json::json;
    use std::fs;
    use std::path::Path;
    use tempfile::tempdir;
    use uuid::Uuid;

    fn create_git_checkout(root: &Path) {
        fs::create_dir_all(root.join(".git")).expect("create .git");
        fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").expect("write HEAD");
    }

    fn write_origin(root: &Path, url: &str) {
        fs::write(
            root.join(".git/config"),
            format!("[remote \"origin\"]\n\turl = {url}\n"),
        )
        .expect("write Git config");
    }

    async fn establish_local(
        root: &Path,
        workspace_id: Uuid,
        project_id: Option<Uuid>,
    ) -> RepositoryFingerprint {
        let fingerprint = ensure_repository_fingerprint(root).expect("repository identity");
        let remote_identity =
            current_repository_remote_identity(root).expect("read remote identity");
        let checkout_id = CheckoutId::for_legacy_binding(root, &fingerprint);
        let repository_identity = RepositoryBindingIdentity {
            fingerprint: &fingerprint,
            remote: remote_identity.as_ref(),
            checkout_id: &checkout_id,
            kind: CheckoutIdentityKind::Git,
        };
        write_established_local_config(
            root,
            workspace_id,
            "Engineering",
            project_id,
            project_id.map(|_| "mcp"),
            repository_identity,
        )
        .await
        .expect("local binding");
        fingerprint
    }

    #[tokio::test]
    async fn explicit_establishment_writes_local_and_global_repository_identity() {
        let temp = tempdir().expect("tempdir");
        create_git_checkout(temp.path());
        write_origin(
            temp.path(),
            "https://user:secret@GitHub.COM/Org/Platform/Repo.git",
        );
        let mappings_path = temp.path().join("machine/mappings.json");
        let workspace_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        let fingerprint = ensure_repository_fingerprint(temp.path()).expect("identity");
        let remote_identity = current_repository_remote_identity(temp.path())
            .expect("remote")
            .expect("remote identity");
        let checkout_id = CheckoutId::for_legacy_binding(temp.path(), &fingerprint);
        let repository_identity = RepositoryBindingIdentity {
            fingerprint: &fingerprint,
            remote: Some(&remote_identity),
            checkout_id: &checkout_id,
            kind: CheckoutIdentityKind::Git,
        };
        write_established_local_config(
            temp.path(),
            workspace_id,
            "Engineering",
            Some(project_id),
            Some("mcp"),
            repository_identity,
        )
        .await
        .expect("local binding");
        write_established_global_mapping(
            &mappings_path,
            temp.path(),
            workspace_id,
            "Engineering",
            Some(project_id),
            Some("mcp"),
            repository_identity,
        )
        .await
        .expect("global binding");

        let local: serde_json::Value = serde_json::from_str(
            &tokio::fs::read_to_string(temp.path().join(".contextstream/config.json"))
                .await
                .expect("read local"),
        )
        .expect("parse local");
        assert_eq!(
            local
                .get("repository_fingerprint")
                .and_then(|value| value.as_str()),
            Some(fingerprint.as_str())
        );
        assert_eq!(
            local
                .get("repository_remote_identity")
                .and_then(|value| value.as_str()),
            Some(remote_identity.as_str())
        );
        assert!(!local.to_string().contains("secret"));
        let global: serde_json::Value = serde_json::from_str(
            &tokio::fs::read_to_string(&mappings_path)
                .await
                .expect("read mappings"),
        )
        .expect("parse mappings");
        assert_eq!(
            global["mappings"][0]["repository_fingerprint"].as_str(),
            Some(fingerprint.as_str())
        );
        assert_eq!(
            global["mappings"][0]["repository_remote_identity"].as_str(),
            Some(remote_identity.as_str())
        );
        assert!(checkout_binding_matches(
            temp.path().to_str().unwrap(),
            Some(workspace_id),
            project_id
        ));

        let local_path = temp.path().join(".contextstream/config.json");
        let local_before = fs::read(&local_path).expect("local bytes before repeat");
        let global_before = fs::read(&mappings_path).expect("global bytes before repeat");
        write_established_local_config(
            temp.path(),
            workspace_id,
            "Engineering",
            Some(project_id),
            Some("mcp"),
            repository_identity,
        )
        .await
        .expect("repeat local binding");
        write_established_global_mapping(
            &mappings_path,
            temp.path(),
            workspace_id,
            "Engineering",
            Some(project_id),
            Some("mcp"),
            repository_identity,
        )
        .await
        .expect("repeat global binding");
        assert_eq!(
            fs::read(local_path).expect("local bytes after repeat"),
            local_before,
            "unchanged explicit local binding must be byte-identical"
        );
        assert_eq!(
            fs::read(mappings_path).expect("global bytes after repeat"),
            global_before,
            "unchanged explicit global binding must be byte-identical"
        );
    }

    #[tokio::test]
    async fn concurrent_establishments_preserve_both_global_mappings() {
        let temp = tempdir().expect("tempdir");
        let first = temp.path().join("first");
        let second = temp.path().join("second");
        create_git_checkout(&first);
        create_git_checkout(&second);
        let first_fingerprint = ensure_repository_fingerprint(&first).expect("first identity");
        let second_fingerprint = ensure_repository_fingerprint(&second).expect("second identity");
        let first_checkout_id = CheckoutId::generate();
        let second_checkout_id = CheckoutId::generate();
        let workspace_id = Uuid::new_v4();
        let mappings_path = temp.path().join("machine/mappings.json");

        let first_write = write_established_global_mapping(
            &mappings_path,
            &first,
            workspace_id,
            "Engineering",
            Some(Uuid::new_v4()),
            Some("first"),
            RepositoryBindingIdentity {
                fingerprint: &first_fingerprint,
                remote: None,
                checkout_id: &first_checkout_id,
                kind: CheckoutIdentityKind::Git,
            },
        );
        let second_write = write_established_global_mapping(
            &mappings_path,
            &second,
            workspace_id,
            "Engineering",
            Some(Uuid::new_v4()),
            Some("second"),
            RepositoryBindingIdentity {
                fingerprint: &second_fingerprint,
                remote: None,
                checkout_id: &second_checkout_id,
                kind: CheckoutIdentityKind::Git,
            },
        );
        let (first_result, second_result) = tokio::join!(first_write, second_write);
        first_result.expect("first mapping write");
        second_result.expect("second mapping write");

        let mappings: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&mappings_path).expect("read mappings"))
                .expect("parse mappings");
        let paths = mappings["mappings"]
            .as_array()
            .expect("mapping array")
            .iter()
            .filter_map(|mapping| mapping["path"].as_str())
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(paths.len(), 2);
        assert!(paths.contains(first.to_string_lossy().as_ref()));
        assert!(paths.contains(second.to_string_lossy().as_ref()));
    }

    #[tokio::test]
    async fn explicit_reestablishment_preserves_checkout_id_across_partial_global_state() {
        let checkout = tempdir().expect("checkout");
        create_git_checkout(checkout.path());
        let fingerprint = ensure_repository_fingerprint(checkout.path()).expect("identity");
        let checkout_id = CheckoutId::generate();
        let mappings_path = checkout.path().join("machine/mappings.json");
        let identity = RepositoryBindingIdentity {
            fingerprint: &fingerprint,
            remote: None,
            checkout_id: &checkout_id,
            kind: CheckoutIdentityKind::Git,
        };
        write_established_global_mapping(
            &mappings_path,
            checkout.path(),
            Uuid::new_v4(),
            "Engineering",
            Some(Uuid::new_v4()),
            Some("mcp"),
            identity,
        )
        .await
        .expect("partial global binding");

        assert_eq!(
            resolve_checkout_id_for_establishment(
                checkout.path(),
                &fingerprint,
                CheckoutIdentityKind::Git,
                Some(&mappings_path)
            )
            .expect("resolved checkout id"),
            checkout_id
        );
    }

    #[tokio::test]
    async fn ordinary_refresh_backfills_checkout_id_only_after_trusted_validation() {
        let checkout = tempdir().expect("checkout");
        create_git_checkout(checkout.path());
        let fingerprint = ensure_repository_fingerprint(checkout.path()).expect("identity");
        let workspace_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        let canonical_root = fs::canonicalize(checkout.path()).expect("canonical root");
        let config_dir = checkout.path().join(".contextstream");
        fs::create_dir_all(&config_dir).expect("config dir");
        let config_path = config_dir.join("config.json");
        fs::write(
            &config_path,
            serde_json::to_string_pretty(&json!({
                "workspace_id": workspace_id,
                "workspace_name": "Old",
                "project_id": project_id,
                "project_name": "Old project",
                "checkout_root": canonical_root,
                "repository_fingerprint": fingerprint.as_str(),
            }))
            .expect("legacy config"),
        )
        .expect("write legacy config");

        let binding = crate::checkout_identity::validate_checkout_binding(
            checkout.path(),
            Some(workspace_id),
            project_id,
        )
        .expect("trusted legacy binding");
        assert!(
            refresh_local_config_if_valid(
                checkout.path().to_str().expect("path"),
                workspace_id,
                "Engineering",
                Some(project_id),
                Some("mcp"),
                RepositoryBindingIdentity {
                    fingerprint: &fingerprint,
                    remote: None,
                    checkout_id: &binding.checkout_id,
                    kind: CheckoutIdentityKind::Git,
                },
            )
            .await
        );
        let refreshed: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(config_path).expect("refreshed config"))
                .expect("parse refreshed config");
        assert_eq!(
            refreshed["checkout_id"].as_str(),
            Some(binding.checkout_id.as_str())
        );
    }

    #[tokio::test]
    async fn explicit_establishment_refuses_malformed_global_mapping_store() {
        let checkout = tempdir().expect("checkout");
        create_git_checkout(checkout.path());
        let fingerprint = ensure_repository_fingerprint(checkout.path()).expect("identity");
        let checkout_id = CheckoutId::for_legacy_binding(checkout.path(), &fingerprint);
        let mappings_path = checkout.path().join("machine/mappings.json");
        tokio::fs::create_dir_all(mappings_path.parent().unwrap())
            .await
            .expect("mapping parent");
        tokio::fs::write(&mappings_path, r#"{"mappings": {}}"#)
            .await
            .expect("malformed mappings");

        let result = write_established_global_mapping(
            &mappings_path,
            checkout.path(),
            Uuid::new_v4(),
            "Engineering",
            Some(Uuid::new_v4()),
            Some("mcp"),
            RepositoryBindingIdentity {
                fingerprint: &fingerprint,
                remote: None,
                checkout_id: &checkout_id,
                kind: CheckoutIdentityKind::Git,
            },
        )
        .await;
        assert!(matches!(
            result,
            Err(crate::checkout_identity::CheckoutIdentityError::InvalidGlobalMappings(_))
        ));
        assert_eq!(
            tokio::fs::read_to_string(mappings_path)
                .await
                .expect("unchanged mappings"),
            r#"{"mappings": {}}"#
        );
    }

    #[tokio::test]
    async fn explicit_establishment_refuses_malformed_local_config_without_rewriting_it() {
        let checkout = tempdir().expect("checkout");
        create_git_checkout(checkout.path());
        let config_dir = checkout.path().join(".contextstream");
        fs::create_dir_all(&config_dir).expect("config dir");
        let config_path = config_dir.join("config.json");
        fs::write(&config_path, "{ definitely not json").expect("malformed config");
        let fingerprint = ensure_repository_fingerprint(checkout.path()).expect("identity");
        let checkout_id = CheckoutId::for_legacy_binding(checkout.path(), &fingerprint);

        let result = write_established_local_config(
            checkout.path(),
            Uuid::new_v4(),
            "Engineering",
            Some(Uuid::new_v4()),
            Some("mcp"),
            RepositoryBindingIdentity {
                fingerprint: &fingerprint,
                remote: None,
                checkout_id: &checkout_id,
                kind: CheckoutIdentityKind::Git,
            },
        )
        .await;

        assert!(matches!(
            result,
            Err(crate::checkout_identity::CheckoutIdentityError::InvalidLocalConfig(_))
        ));
        assert_eq!(
            fs::read_to_string(config_path).expect("unchanged config"),
            "{ definitely not json"
        );
    }

    #[tokio::test]
    async fn explicit_establishment_refuses_duplicate_nested_keys_without_rewriting_it() {
        let checkout = tempdir().expect("checkout");
        create_git_checkout(checkout.path());
        let config_dir = checkout.path().join(".contextstream");
        fs::create_dir_all(&config_dir).expect("config dir");
        let config_path = config_dir.join("config.json");
        let original = r#"{"custom":{"mode":"first","mode":"second"},"workspace_id":"old"}"#;
        fs::write(&config_path, original).expect("config with duplicate nested key");
        let fingerprint = ensure_repository_fingerprint(checkout.path()).expect("identity");
        let checkout_id = CheckoutId::for_legacy_binding(checkout.path(), &fingerprint);

        let result = write_established_local_config(
            checkout.path(),
            Uuid::new_v4(),
            "Engineering",
            Some(Uuid::new_v4()),
            Some("mcp"),
            RepositoryBindingIdentity {
                fingerprint: &fingerprint,
                remote: None,
                checkout_id: &checkout_id,
                kind: CheckoutIdentityKind::Git,
            },
        )
        .await;

        assert!(matches!(
            result,
            Err(crate::checkout_identity::CheckoutIdentityError::InvalidLocalConfig(_))
        ));
        assert_eq!(
            fs::read_to_string(config_path).expect("unchanged config"),
            original
        );
    }

    #[tokio::test]
    async fn explicit_establishment_preserves_unknown_local_config_fields() {
        let checkout = tempdir().expect("checkout");
        create_git_checkout(checkout.path());
        let config_dir = checkout.path().join(".contextstream");
        fs::create_dir_all(&config_dir).expect("config dir");
        let config_path = config_dir.join("config.json");
        fs::write(
            &config_path,
            r#"{"custom":{"nested":true},"workspace_id":"old"}"#,
        )
        .expect("seed config");
        let fingerprint = ensure_repository_fingerprint(checkout.path()).expect("identity");
        let checkout_id = CheckoutId::for_legacy_binding(checkout.path(), &fingerprint);

        write_established_local_config(
            checkout.path(),
            Uuid::new_v4(),
            "Engineering",
            Some(Uuid::new_v4()),
            Some("mcp"),
            RepositoryBindingIdentity {
                fingerprint: &fingerprint,
                remote: None,
                checkout_id: &checkout_id,
                kind: CheckoutIdentityKind::Git,
            },
        )
        .await
        .expect("establish binding");

        let updated: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(config_path).expect("read config"))
                .expect("parse config");
        assert_eq!(updated["custom"], json!({"nested": true}));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn explicit_establishment_refuses_symlinked_global_mapping_file() {
        use std::os::unix::fs::symlink;

        let checkout = tempdir().expect("checkout");
        create_git_checkout(checkout.path());
        let fingerprint = ensure_repository_fingerprint(checkout.path()).expect("identity");
        let checkout_id = CheckoutId::for_legacy_binding(checkout.path(), &fingerprint);
        let machine = checkout.path().join("machine");
        fs::create_dir_all(&machine).expect("machine dir");
        let unrelated = checkout.path().join("unrelated.json");
        let mappings_path = machine.join("mappings.json");
        fs::write(&unrelated, r#"{"keep":true}"#).expect("unrelated");
        symlink(&unrelated, &mappings_path).expect("mapping symlink");

        assert!(write_established_global_mapping(
            &mappings_path,
            checkout.path(),
            Uuid::new_v4(),
            "Engineering",
            Some(Uuid::new_v4()),
            Some("mcp"),
            RepositoryBindingIdentity {
                fingerprint: &fingerprint,
                remote: None,
                checkout_id: &checkout_id,
                kind: CheckoutIdentityKind::Git,
            },
        )
        .await
        .is_err());
        assert_eq!(
            fs::read_to_string(unrelated).expect("unrelated unchanged"),
            r#"{"keep":true}"#
        );
        assert!(mappings_path.is_symlink());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn explicit_establishment_refuses_symlinked_checkout_config_directory() {
        use std::os::unix::fs::symlink;

        let checkout = tempdir().expect("checkout");
        let outside = tempdir().expect("outside");
        create_git_checkout(checkout.path());
        let fingerprint = ensure_repository_fingerprint(checkout.path()).expect("identity");
        let checkout_id = CheckoutId::for_legacy_binding(checkout.path(), &fingerprint);
        symlink(outside.path(), checkout.path().join(".contextstream"))
            .expect("config directory symlink");

        let result = write_established_local_config(
            checkout.path(),
            Uuid::new_v4(),
            "Engineering",
            Some(Uuid::new_v4()),
            Some("mcp"),
            RepositoryBindingIdentity {
                fingerprint: &fingerprint,
                remote: None,
                checkout_id: &checkout_id,
                kind: CheckoutIdentityKind::Git,
            },
        )
        .await;
        assert!(matches!(
            result,
            Err(crate::checkout_identity::CheckoutIdentityError::InvalidLocalConfig(_))
        ));
        assert!(!outside.path().join("config.json").exists());
    }

    #[tokio::test]
    async fn ordinary_persistence_is_refresh_only_and_cannot_create_binding() {
        let temp = tempdir().expect("tempdir");
        create_git_checkout(temp.path());
        let workspace_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();

        persist_folder_mapping(
            temp.path().to_str().unwrap(),
            workspace_id,
            "Engineering",
            Some(project_id),
            Some("mcp"),
        )
        .await;

        assert!(!temp.path().join(".contextstream/config.json").exists());
        assert!(current_repository_fingerprint(temp.path()).is_err());
    }

    #[tokio::test]
    async fn explicit_non_git_folder_config_resolves_as_a_project_scope() {
        let folder = tempdir().expect("folder");
        let workspace_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        let fingerprint = ensure_folder_fingerprint(folder.path()).expect("folder identity");
        let checkout_id = CheckoutId::generate();
        write_established_local_config(
            folder.path(),
            workspace_id,
            "Engineering",
            Some(project_id),
            Some("new-project"),
            RepositoryBindingIdentity {
                fingerprint: &fingerprint,
                remote: None,
                checkout_id: &checkout_id,
                kind: CheckoutIdentityKind::Folder,
            },
        )
        .await
        .expect("folder config");

        let mapping = parse_local_config(&folder.path().join(".contextstream/config.json"))
            .await
            .expect("resolved folder mapping");
        assert_eq!(mapping.workspace_id, workspace_id);
        assert_eq!(mapping.project_id, Some(project_id));
        assert_eq!(mapping.project_name.as_deref(), Some("new-project"));
        assert!(checkout_binding_matches(
            folder.path().to_string_lossy().as_ref(),
            Some(workspace_id),
            project_id,
        ));
    }

    #[tokio::test]
    async fn explicit_git_upgrade_preserves_the_folder_checkout_id() {
        let folder = tempdir().expect("folder");
        let workspace_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        let folder_fingerprint = ensure_folder_fingerprint(folder.path()).expect("folder identity");
        let checkout_id = CheckoutId::generate();
        write_established_local_config(
            folder.path(),
            workspace_id,
            "Engineering",
            Some(project_id),
            Some("new-project"),
            RepositoryBindingIdentity {
                fingerprint: &folder_fingerprint,
                remote: None,
                checkout_id: &checkout_id,
                kind: CheckoutIdentityKind::Folder,
            },
        )
        .await
        .expect("folder config");

        create_git_checkout(folder.path());
        let identity_kind =
            identity_kind_for_establishment(folder.path()).expect("upgrade identity kind");
        assert_eq!(identity_kind, CheckoutIdentityKind::Git);
        assert_eq!(
            folder_checkout_id_for_git_upgrade(folder.path(), None)
                .expect("read prior folder identity"),
            Some(checkout_id.clone())
        );

        let git_fingerprint = ensure_checkout_fingerprint(folder.path(), identity_kind)
            .expect("Git repository identity");
        write_established_local_config(
            folder.path(),
            workspace_id,
            "Engineering",
            Some(project_id),
            Some("new-project"),
            RepositoryBindingIdentity {
                fingerprint: &git_fingerprint,
                remote: None,
                checkout_id: &checkout_id,
                kind: identity_kind,
            },
        )
        .await
        .expect("upgraded Git config");

        let binding = validate_checkout_binding(folder.path(), Some(workspace_id), project_id)
            .expect("validated upgraded binding");
        assert_eq!(binding.identity_kind, CheckoutIdentityKind::Git);
        assert_eq!(binding.checkout_id, checkout_id);
        assert_eq!(binding.repository_fingerprint, git_fingerprint);
    }

    #[tokio::test]
    async fn ordinary_persistence_cannot_heal_missing_fingerprint() {
        let temp = tempdir().expect("tempdir");
        create_git_checkout(temp.path());
        let workspace_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        establish_local(temp.path(), workspace_id, Some(project_id)).await;
        let config_path = temp.path().join(".contextstream/config.json");
        let mut config: serde_json::Value = serde_json::from_str(
            &tokio::fs::read_to_string(&config_path)
                .await
                .expect("read config"),
        )
        .expect("parse config");
        config
            .as_object_mut()
            .expect("config object")
            .remove("repository_fingerprint");
        tokio::fs::write(&config_path, serde_json::to_string_pretty(&config).unwrap())
            .await
            .expect("remove fingerprint");

        persist_folder_mapping(
            temp.path().to_str().unwrap(),
            workspace_id,
            "Renamed workspace",
            Some(project_id),
            Some("renamed project"),
        )
        .await;

        let unchanged = tokio::fs::read_to_string(config_path)
            .await
            .expect("read config");
        let unchanged: serde_json::Value = serde_json::from_str(&unchanged).expect("parse");
        assert!(unchanged.get("repository_fingerprint").is_none());
        assert_eq!(unchanged["workspace_name"], "Engineering");
        assert_eq!(unchanged["project_name"], "mcp");
    }

    #[tokio::test]
    async fn local_config_requires_matching_checkout_root_and_repository() {
        let source = tempdir().expect("source tempdir");
        let copied = tempdir().expect("copied tempdir");
        create_git_checkout(source.path());
        create_git_checkout(copied.path());
        let source_fingerprint =
            ensure_repository_fingerprint(source.path()).expect("source identity");
        ensure_repository_fingerprint(copied.path()).expect("copied identity");
        let config_dir = copied.path().join(".contextstream");
        tokio::fs::create_dir_all(&config_dir)
            .await
            .expect("config dir");
        let config_path = config_dir.join("config.json");
        tokio::fs::write(
            &config_path,
            serde_json::to_string_pretty(&json!({
                "workspace_id": Uuid::new_v4().to_string(),
                "workspace_name": "Engineering",
                "project_id": Uuid::new_v4().to_string(),
                "project_name": "copied-project",
                "checkout_root": std::fs::canonicalize(source.path())
                    .expect("canonical source")
                    .to_string_lossy(),
                "repository_fingerprint": source_fingerprint.as_str()
            }))
            .expect("json"),
        )
        .await
        .expect("write config");

        assert!(parse_local_config(&config_path).await.is_none());

        let mut valid: serde_json::Value = serde_json::from_str(
            &tokio::fs::read_to_string(&config_path)
                .await
                .expect("read config"),
        )
        .expect("parse config");
        valid["checkout_root"] = json!(std::fs::canonicalize(copied.path())
            .expect("canonical copied")
            .to_string_lossy());
        tokio::fs::write(&config_path, serde_json::to_string_pretty(&valid).unwrap())
            .await
            .expect("write valid config");
        assert!(
            parse_local_config(&config_path).await.is_none(),
            "changing only the copied path must not bless another repository"
        );
    }

    #[tokio::test]
    async fn content_binding_requires_exact_project_workspace_and_root() {
        let temp = tempdir().expect("tempdir");
        let workspace_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        create_git_checkout(temp.path());
        establish_local(temp.path(), workspace_id, Some(project_id)).await;

        assert!(checkout_binding_matches(
            temp.path().to_str().unwrap(),
            Some(workspace_id),
            project_id
        ));
        assert_eq!(
            checkout_binding_workspace(temp.path().to_str().unwrap(), project_id),
            Some(workspace_id)
        );
        assert!(!checkout_binding_matches(
            temp.path().to_str().unwrap(),
            Some(Uuid::new_v4()),
            project_id
        ));
        assert!(!checkout_binding_matches(
            temp.path().to_str().unwrap(),
            Some(workspace_id),
            Uuid::new_v4()
        ));

        let config_path = temp.path().join(".contextstream/config.json");
        let mut config: serde_json::Value = serde_json::from_str(
            &tokio::fs::read_to_string(&config_path)
                .await
                .expect("read config"),
        )
        .expect("parse config");
        config
            .as_object_mut()
            .expect("config object")
            .remove("workspace_id");
        tokio::fs::write(&config_path, serde_json::to_string_pretty(&config).unwrap())
            .await
            .expect("write config without workspace");
        assert!(checkout_binding_workspace(temp.path().to_str().unwrap(), project_id).is_none());
    }

    #[tokio::test]
    async fn legacy_local_config_without_checkout_root_fails_closed() {
        let temp = tempdir().expect("tempdir");
        create_git_checkout(temp.path());
        ensure_repository_fingerprint(temp.path()).expect("identity");
        let config_dir = temp.path().join(".contextstream");
        tokio::fs::create_dir_all(&config_dir)
            .await
            .expect("config dir");
        let config_path = config_dir.join("config.json");
        tokio::fs::write(
            &config_path,
            serde_json::to_string_pretty(&json!({
                "workspace_id": Uuid::new_v4().to_string(),
                "workspace_name": "Engineering",
                "project_id": Uuid::new_v4().to_string(),
                "project_name": "legacy"
            }))
            .expect("json"),
        )
        .await
        .expect("write config");

        assert!(parse_local_config(&config_path).await.is_none());
    }

    #[tokio::test]
    async fn legacy_local_config_with_root_but_without_fingerprint_fails_closed() {
        let checkout = tempdir().expect("checkout");
        create_git_checkout(checkout.path());
        ensure_repository_fingerprint(checkout.path()).expect("identity");
        let config_dir = checkout.path().join(".contextstream");
        tokio::fs::create_dir_all(&config_dir)
            .await
            .expect("config dir");
        let config_path = config_dir.join("config.json");
        tokio::fs::write(
            &config_path,
            serde_json::to_string_pretty(&json!({
                "workspace_id": Uuid::new_v4().to_string(),
                "project_id": Uuid::new_v4().to_string(),
                "checkout_root": std::fs::canonicalize(checkout.path()).unwrap().to_string_lossy()
            }))
            .unwrap(),
        )
        .await
        .expect("legacy config");

        assert!(parse_local_config(&config_path).await.is_none());
    }

    #[tokio::test]
    async fn malformed_project_id_is_not_downgraded_to_workspace_only() {
        let checkout = tempdir().expect("checkout");
        create_git_checkout(checkout.path());
        let fingerprint = ensure_repository_fingerprint(checkout.path()).expect("identity");
        let config_dir = checkout.path().join(".contextstream");
        tokio::fs::create_dir_all(&config_dir)
            .await
            .expect("config dir");
        let config_path = config_dir.join("config.json");
        tokio::fs::write(
            &config_path,
            serde_json::to_string_pretty(&json!({
                "workspace_id": Uuid::new_v4().to_string(),
                "project_id": "not-a-uuid",
                "checkout_root": std::fs::canonicalize(checkout.path()).unwrap().to_string_lossy(),
                "repository_fingerprint": fingerprint.as_str()
            }))
            .unwrap(),
        )
        .await
        .expect("malformed config");

        assert!(parse_local_config(&config_path).await.is_none());
    }

    #[tokio::test]
    async fn invalid_nested_config_is_a_hard_scope_boundary() {
        let parent = tempdir().expect("parent tempdir");
        create_git_checkout(parent.path());
        let fingerprint = ensure_repository_fingerprint(parent.path()).expect("identity");
        let child = parent.path().join("child");
        let nested = child.join("src");
        tokio::fs::create_dir_all(parent.path().join(".contextstream"))
            .await
            .expect("parent config dir");
        tokio::fs::create_dir_all(child.join(".contextstream"))
            .await
            .expect("child config dir");
        tokio::fs::create_dir_all(&nested)
            .await
            .expect("nested dir");

        tokio::fs::write(
            parent.path().join(".contextstream/config.json"),
            serde_json::to_string_pretty(&json!({
                "workspace_id": Uuid::new_v4().to_string(),
                "project_id": Uuid::new_v4().to_string(),
                "checkout_root": std::fs::canonicalize(parent.path())
                    .expect("canonical parent")
                    .to_string_lossy(),
                "repository_fingerprint": fingerprint.as_str()
            }))
            .unwrap(),
        )
        .await
        .expect("parent config");
        tokio::fs::write(
            child.join(".contextstream/config.json"),
            serde_json::to_string_pretty(&json!({
                "workspace_id": Uuid::new_v4().to_string(),
                "project_id": Uuid::new_v4().to_string(),
                "checkout_root": "/copied/from/elsewhere",
                "repository_fingerprint": fingerprint.as_str()
            }))
            .unwrap(),
        )
        .await
        .expect("child config");

        match resolve_local_config(nested.to_str().unwrap()).await {
            LocalConfigResolution::InvalidBoundary { root, .. } => {
                assert_eq!(root, child);
            }
            _ => panic!("invalid child config must block ancestor inheritance"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dangling_checkout_config_symlink_is_a_hard_scope_boundary() {
        use std::os::unix::fs::symlink;

        let checkout = tempdir().expect("checkout");
        create_git_checkout(checkout.path());
        let config_dir = checkout.path().join(".contextstream");
        fs::create_dir_all(&config_dir).expect("config directory");
        symlink(
            checkout.path().join("missing-config-target.json"),
            config_dir.join("config.json"),
        )
        .expect("dangling config symlink");

        match resolve_local_config(checkout.path().to_str().expect("checkout path")).await {
            LocalConfigResolution::InvalidBoundary { root } => {
                assert_eq!(root, checkout.path());
            }
            _ => panic!("a dangling checkout config symlink must block fallback resolution"),
        }
    }

    #[test]
    fn global_mapping_prefix_respects_path_boundaries_and_exact_mode() {
        let mappings = vec![json!({
            "path": "/work/api",
            "workspace_id": Uuid::new_v4().to_string()
        })];

        assert!(select_best_mapping(&mappings, "/work/api/src", false).is_some());
        assert!(select_best_mapping(&mappings, "/work/api/src", true).is_none());
        assert!(select_best_mapping(&mappings, "/work/api-copy", false).is_none());
        assert!(select_best_mapping(&mappings, "/work/api", true).is_some());
    }

    #[test]
    fn duplicate_equally_specific_global_mappings_fail_closed() {
        let mappings = vec![
            json!({
                "path": "/work/api",
                "workspace_id": Uuid::new_v4().to_string()
            }),
            json!({
                "path": "/work/api/*",
                "workspace_id": Uuid::new_v4().to_string()
            }),
        ];
        assert!(select_best_mapping(&mappings, "/work/api", true).is_none());
        assert!(select_best_mapping(&mappings, "/work/api/src", false).is_none());
    }

    #[test]
    fn project_bearing_global_mapping_requires_matching_repository_fingerprint() {
        let checkout = tempdir().expect("checkout");
        create_git_checkout(checkout.path());
        write_origin(
            checkout.path(),
            "git@code.example.test:Org/Team/Original.git",
        );
        let fingerprint = ensure_repository_fingerprint(checkout.path()).expect("identity");
        let remote_identity = current_repository_remote_identity(checkout.path())
            .expect("remote")
            .expect("remote identity");
        let workspace_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        let mut mapping = json!({
            "path": checkout.path().to_string_lossy(),
            "workspace_id": workspace_id.to_string(),
            "project_id": project_id.to_string(),
            "repository_fingerprint": fingerprint.as_str(),
            "repository_remote_identity": remote_identity.as_str()
        });

        assert!(parse_workspace_mapping(&mapping, checkout.path().to_str().unwrap()).is_some());
        mapping
            .as_object_mut()
            .unwrap()
            .remove("repository_fingerprint");
        assert!(parse_workspace_mapping(&mapping, checkout.path().to_str().unwrap()).is_none());
        mapping["repository_fingerprint"] = json!(RepositoryFingerprint::parse(&format!(
            "git-common-dir-v1:{}",
            Uuid::new_v4()
        ))
        .unwrap()
        .as_str());
        assert!(parse_workspace_mapping(&mapping, checkout.path().to_str().unwrap()).is_none());

        mapping["repository_fingerprint"] = json!(fingerprint.as_str());
        mapping
            .as_object_mut()
            .expect("mapping object")
            .remove("repository_remote_identity");
        assert!(
            parse_workspace_mapping(&mapping, checkout.path().to_str().unwrap()).is_some(),
            "legacy mappings without the optional remote field remain marker-bound"
        );
        mapping["repository_remote_identity"] = json!(remote_identity.as_str());
        write_origin(
            checkout.path(),
            "git@code.example.test:Org/Team/Replacement.git",
        );
        assert!(
            parse_workspace_mapping(&mapping, checkout.path().to_str().unwrap()).is_none(),
            "global workspace lookup must reject a changed stored remote identity"
        );
    }

    #[tokio::test]
    async fn same_path_remote_change_invalidates_lookup_and_refresh_cannot_heal_it() {
        let checkout = tempdir().expect("checkout");
        create_git_checkout(checkout.path());
        write_origin(checkout.path(), "git@github.com:Acme/Platform/Original.git");
        let workspace_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        establish_local(checkout.path(), workspace_id, Some(project_id)).await;
        let config_path = checkout.path().join(".contextstream/config.json");
        let established: serde_json::Value = serde_json::from_str(
            &tokio::fs::read_to_string(&config_path)
                .await
                .expect("read established config"),
        )
        .expect("parse established config");
        let established_remote = established["repository_remote_identity"]
            .as_str()
            .expect("stored remote")
            .to_string();

        write_origin(
            checkout.path(),
            "https://github.com/Acme/Platform/Replacement.git",
        );
        assert!(!checkout_binding_matches(
            checkout.path().to_str().unwrap(),
            Some(workspace_id),
            project_id
        ));
        assert!(parse_local_config(&config_path).await.is_none());

        persist_folder_mapping(
            checkout.path().to_str().unwrap(),
            workspace_id,
            "Must not refresh",
            Some(project_id),
            Some("must-not-refresh"),
        )
        .await;
        let unchanged: serde_json::Value = serde_json::from_str(
            &tokio::fs::read_to_string(&config_path)
                .await
                .expect("read unchanged config"),
        )
        .expect("parse unchanged config");
        assert_eq!(unchanged["workspace_name"], "Engineering");
        assert_eq!(unchanged["project_name"], "mcp");
        assert_eq!(
            unchanged["repository_remote_identity"].as_str(),
            Some(established_remote.as_str()),
            "ordinary persistence must not rewrite the trust anchor"
        );
    }

    #[tokio::test]
    async fn configured_remote_disappearance_invalidates_binding() {
        let checkout = tempdir().expect("checkout");
        create_git_checkout(checkout.path());
        write_origin(
            checkout.path(),
            "ssh://git@code.example.test/Org/Team/Repo.git",
        );
        let workspace_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        establish_local(checkout.path(), workspace_id, Some(project_id)).await;
        assert!(checkout_binding_matches(
            checkout.path().to_str().unwrap(),
            Some(workspace_id),
            project_id
        ));

        fs::write(
            checkout.path().join(".git/config"),
            "[core]\n\trepositoryformatversion = 0\n",
        )
        .expect("remove remote");
        assert!(!checkout_binding_matches(
            checkout.path().to_str().unwrap(),
            Some(workspace_id),
            project_id
        ));
    }

    #[tokio::test]
    async fn no_remote_binding_remains_marker_only() {
        let checkout = tempdir().expect("checkout");
        create_git_checkout(checkout.path());
        let workspace_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        establish_local(checkout.path(), workspace_id, Some(project_id)).await;

        let config: serde_json::Value = serde_json::from_str(
            &tokio::fs::read_to_string(checkout.path().join(".contextstream/config.json"))
                .await
                .expect("read config"),
        )
        .expect("parse config");
        assert!(config.get("repository_remote_identity").is_none());
        assert!(checkout_binding_matches(
            checkout.path().to_str().unwrap(),
            Some(workspace_id),
            project_id
        ));

        write_origin(
            checkout.path(),
            "git@github.com:Acme/Platform/LaterRemote.git",
        );
        assert!(checkout_binding_matches(
            checkout.path().to_str().unwrap(),
            Some(workspace_id),
            project_id
        ));
        let unchanged: serde_json::Value = serde_json::from_str(
            &tokio::fs::read_to_string(checkout.path().join(".contextstream/config.json"))
                .await
                .expect("read unchanged config"),
        )
        .expect("parse unchanged config");
        assert!(unchanged.get("repository_remote_identity").is_none());
    }

    #[tokio::test]
    async fn same_path_repository_replacement_invalidates_binding() {
        let checkout = tempdir().expect("checkout");
        create_git_checkout(checkout.path());
        let workspace_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        let original = establish_local(checkout.path(), workspace_id, Some(project_id)).await;
        assert!(checkout_binding_matches(
            checkout.path().to_str().unwrap(),
            Some(workspace_id),
            project_id
        ));

        fs::remove_dir_all(checkout.path().join(".git")).expect("remove original Git metadata");
        create_git_checkout(checkout.path());
        assert!(
            checkout_binding_workspace(checkout.path().to_str().unwrap(), project_id).is_none(),
            "a replacement without a marker must fail closed"
        );
        let replacement = ensure_repository_fingerprint(checkout.path()).expect("new identity");
        assert_ne!(replacement, original);
        assert!(
            checkout_binding_workspace(checkout.path().to_str().unwrap(), project_id).is_none(),
            "minting the replacement's marker must not rewrite the stored binding"
        );
    }

    #[tokio::test]
    async fn clear_local_config_project_removes_project_keeps_workspace() {
        use super::clear_local_config_project;

        let temp = tempdir().expect("tempdir");
        let config_dir = temp.path().join(".contextstream");
        tokio::fs::create_dir_all(&config_dir)
            .await
            .expect("config dir");
        let workspace_id = Uuid::new_v4();
        create_git_checkout(temp.path());
        let fingerprint = ensure_repository_fingerprint(temp.path()).expect("identity");
        tokio::fs::write(
            config_dir.join("config.json"),
            serde_json::to_string_pretty(&json!({
                "workspace_id": workspace_id.to_string(),
                "workspace_name": "Engineering",
                "project_id": Uuid::new_v4().to_string(),
                "project_name": "mcp",
                "checkout_root": std::fs::canonicalize(temp.path()).unwrap().to_string_lossy(),
                "repository_fingerprint": fingerprint.as_str()
            }))
            .expect("json"),
        )
        .await
        .expect("write config");

        let folder = temp.path().to_str().unwrap_or_default();
        assert!(
            clear_local_config_project(folder).await,
            "should report a project was cleared"
        );

        let parsed: serde_json::Value = serde_json::from_str(
            &tokio::fs::read_to_string(config_dir.join("config.json"))
                .await
                .expect("read config"),
        )
        .expect("parse config");
        // Project scope gone, workspace association preserved.
        assert!(parsed.get("project_id").is_none());
        assert!(parsed.get("project_name").is_none());
        assert_eq!(
            parsed
                .get("repository_fingerprint")
                .and_then(|value| value.as_str()),
            Some(fingerprint.as_str())
        );
        assert_eq!(
            parsed.get("workspace_id").and_then(|v| v.as_str()),
            Some(workspace_id.to_string().as_str())
        );

        // Idempotent: a second call (no project left) reports false.
        assert!(!clear_local_config_project(folder).await);
        // Missing config -> false (no panic).
        assert!(!clear_local_config_project("/nonexistent/forget-local-test").await);
    }
}
