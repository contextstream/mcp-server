//! Machine-local folder identity for checkout-scoped content operations.
//!
//! A canonical path is not sufficient identity: a folder or repository can be
//! deleted and another one can later appear at the same path. ContextStream
//! therefore stores a random, versioned marker in Git's common directory, or
//! beside the local config for an explicitly linked non-Git folder, and copies
//! that value into the checkout-local config. Automatic content writers require
//! both values to match. Creating the marker is intentionally separate from
//! reading it so ordinary init, context, and hook paths cannot silently bless a
//! replacement folder.

use mcp_client::json::parse_value_without_duplicate_keys;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use uuid::Uuid;

const GIT_METADATA_MAX_BYTES: u64 = 4 * 1024;
const GIT_CONFIG_MAX_BYTES: u64 = 256 * 1024;
const GIT_CONFIG_INCLUDE_DEPTH: usize = 8;
const MARKER_DIRECTORY: &str = "contextstream";
const MARKER_FILE: &str = "repository-id";
const FOLDER_MARKER_FILE: &str = "folder-id";
const FINGERPRINT_PREFIX: &str = "git-common-dir-v1:";
const REMOTE_IDENTITY_PREFIX: &str = "git-remote-v1:";
const CHECKOUT_ID_PREFIX: &str = "checkout-v1:";
const REPOSITORY_FINGERPRINT_PUBLISH_GRACE: Duration = Duration::from_secs(2);
const REPOSITORY_FINGERPRINT_PUBLISH_POLL: Duration = Duration::from_millis(2);

/// Stable, versioned identity for one local clone lineage.
///
/// Linked worktrees share this marker through Git's common directory.
/// Independent clones on other machines normally have different fingerprints;
/// [`RepositoryRemoteIdentity`] is the portable canonical-project key.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct RepositoryFingerprint(String);

impl RepositoryFingerprint {
    /// Parse and canonicalize a repository fingerprint.
    pub fn parse(value: &str) -> Result<Self, CheckoutIdentityError> {
        let trimmed = value.trim();
        let raw_uuid = trimmed.strip_prefix(FINGERPRINT_PREFIX).ok_or_else(|| {
            CheckoutIdentityError::InvalidRepositoryFingerprint {
                value: trimmed.to_string(),
            }
        })?;
        let id = Uuid::parse_str(raw_uuid).map_err(|_| {
            CheckoutIdentityError::InvalidRepositoryFingerprint {
                value: trimmed.to_string(),
            }
        })?;
        Ok(Self(format!("{FINGERPRINT_PREFIX}{id}")))
    }

    fn generate() -> Self {
        Self(format!("{FINGERPRINT_PREFIX}{}", Uuid::new_v4()))
    }

    /// Return the serialized marker value.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RepositoryFingerprint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Stable, opaque identity for one concrete checkout or worktree.
///
/// Several checkout IDs may share one [`RepositoryFingerprint`], which is the
/// expected shape for linked Git worktrees. Independent clones normally have
/// different fingerprints and are joined to one canonical project by
/// [`RepositoryRemoteIdentity`]. Checkout identity answers "which mutable view
/// of that project produced these bytes?".
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CheckoutId(String);

impl CheckoutId {
    /// Parse and canonicalize a serialized checkout ID.
    pub fn parse(value: &str) -> Result<Self, CheckoutIdentityError> {
        let trimmed = value.trim();
        let raw_uuid = trimmed.strip_prefix(CHECKOUT_ID_PREFIX).ok_or_else(|| {
            CheckoutIdentityError::InvalidCheckoutId {
                value: trimmed.to_string(),
            }
        })?;
        let id =
            Uuid::parse_str(raw_uuid).map_err(|_| CheckoutIdentityError::InvalidCheckoutId {
                value: trimmed.to_string(),
            })?;
        Ok(Self(format!("{CHECKOUT_ID_PREFIX}{id}")))
    }

    /// Mint a new checkout identity during explicit binding establishment.
    pub(crate) fn generate() -> Self {
        Self(format!("{CHECKOUT_ID_PREFIX}{}", Uuid::new_v4()))
    }

    /// Derive the transitional identity for a trusted pre-checkout-ID binding.
    ///
    /// Only a hash leaves this function; the absolute local path is never sent
    /// or persisted as part of the opaque identifier. Version/variant bits are
    /// set to an RFC 9562-compatible name-based UUID shape so parsing and
    /// downstream storage remain uniform.
    pub fn for_legacy_binding(
        checkout_root: &Path,
        repository_fingerprint: &RepositoryFingerprint,
    ) -> Self {
        let mut digest = Sha256::new();
        digest.update(b"contextstream-checkout-v1\0");
        digest.update(repository_fingerprint.as_str().as_bytes());
        digest.update(b"\0");
        digest.update(checkout_root.to_string_lossy().as_bytes());
        let hash = digest.finalize();
        let mut bytes = [0_u8; 16];
        bytes.copy_from_slice(&hash[..16]);
        bytes[6] = (bytes[6] & 0x0f) | 0x50;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        Self(format!("{CHECKOUT_ID_PREFIX}{}", Uuid::from_bytes(bytes)))
    }

    /// Return the serialized checkout ID.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for CheckoutId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Credential-free, transport-independent identity of a repository remote.
///
/// The serialized form is versioned and contains only the normalized host and
/// complete repository namespace. User names, passwords, query strings, and
/// fragments from the configured URL are never retained.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct RepositoryRemoteIdentity(String);

impl RepositoryRemoteIdentity {
    /// Parse a previously serialized repository remote identity.
    pub fn parse(value: &str) -> Result<Self, CheckoutIdentityError> {
        let trimmed = value.trim();
        let body = trimmed
            .strip_prefix(REMOTE_IDENTITY_PREFIX)
            .ok_or(CheckoutIdentityError::InvalidRepositoryRemoteIdentity)?;
        let (host, namespace) = body
            .split_once('/')
            .ok_or(CheckoutIdentityError::InvalidRepositoryRemoteIdentity)?;
        let normalized_host = normalize_remote_host(host, None)
            .map_err(|_| CheckoutIdentityError::InvalidRepositoryRemoteIdentity)?;
        let normalized_namespace = normalize_remote_namespace(namespace)
            .map_err(|_| CheckoutIdentityError::InvalidRepositoryRemoteIdentity)?;
        let normalized =
            format!("{REMOTE_IDENTITY_PREFIX}{normalized_host}/{normalized_namespace}");
        if normalized != trimmed {
            return Err(CheckoutIdentityError::InvalidRepositoryRemoteIdentity);
        }
        Ok(Self(normalized))
    }

    /// Normalize a Git remote URL without retaining credentials or transport.
    pub fn from_remote_url(value: &str) -> Result<Self, CheckoutIdentityError> {
        normalize_remote_url(value)
            .map(Self)
            .map_err(|_| CheckoutIdentityError::InvalidRepositoryRemoteIdentity)
    }

    /// Return the safe, versioned serialized identity.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Return a credential-free canonical HTTPS URL suitable for project
    /// metadata and cross-machine repository matching.
    ///
    /// The identity parser has already removed transport-specific usernames,
    /// passwords, query strings, and fragments. Reconstructing one stable URL
    /// lets older project APIs persist the repository identity in their
    /// existing `repository_url` field without ever receiving the user's raw
    /// Git remote.
    pub fn canonical_https_url(&self) -> String {
        let repository = self
            .0
            .strip_prefix(REMOTE_IDENTITY_PREFIX)
            .expect("validated repository identity always has its version prefix");
        format!("https://{repository}.git")
    }
}

impl fmt::Display for RepositoryRemoteIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A checkout binding that has passed root, scope, and repository checks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedCheckoutBinding {
    pub checkout_root: PathBuf,
    pub checkout_id: CheckoutId,
    pub workspace_id: Uuid,
    pub project_id: Option<Uuid>,
    pub identity_kind: CheckoutIdentityKind,
    pub repository_fingerprint: RepositoryFingerprint,
    pub repository_remote_identity: Option<RepositoryRemoteIdentity>,
}

/// Where the opaque fingerprint authorizing an exact local folder lives.
///
/// Git checkouts keep the marker in the common Git directory so linked
/// worktrees share repository lineage. A folder that is explicitly linked
/// before Git exists keeps an equally opaque marker beside its local
/// ContextStream config. The serialized fingerprint stays wire-compatible;
/// this kind is local validation metadata only.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckoutIdentityKind {
    Git,
    Folder,
}

impl CheckoutIdentityKind {
    pub const FIELD: &'static str = "checkout_identity_kind";

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Folder => "folder",
        }
    }

    pub fn from_config_value(value: Option<&Value>) -> Result<Self, CheckoutIdentityError> {
        match value {
            None => Ok(Self::Git),
            Some(Value::String(value)) if value == Self::Git.as_str() => Ok(Self::Git),
            Some(Value::String(value)) if value == Self::Folder.as_str() => Ok(Self::Folder),
            _ => Err(CheckoutIdentityError::InvalidRepositoryIdentityKind),
        }
    }
}

/// Fail-closed checkout identity errors.
#[derive(Debug)]
pub enum CheckoutIdentityError {
    InvalidCheckoutRoot(PathBuf),
    NotGitCheckout(PathBuf),
    GitMetadataTooLarge(PathBuf),
    InvalidGitMetadata {
        path: PathBuf,
        reason: &'static str,
    },
    MissingRepositoryFingerprint(PathBuf),
    InvalidRepositoryFingerprint {
        value: String,
    },
    InvalidRepositoryIdentityKind,
    InvalidCheckoutId {
        value: String,
    },
    ConflictingCheckoutIds {
        local: CheckoutId,
        global: CheckoutId,
    },
    RepositoryFingerprintMismatch {
        configured: RepositoryFingerprint,
        current: RepositoryFingerprint,
    },
    InvalidRepositoryRemoteIdentity,
    MissingRepositoryRemoteIdentity {
        configured: RepositoryRemoteIdentity,
    },
    RepositoryRemoteIdentityMismatch {
        configured: RepositoryRemoteIdentity,
        current: RepositoryRemoteIdentity,
    },
    GitConfigTooLarge(PathBuf),
    InvalidGitConfig {
        path: PathBuf,
        reason: &'static str,
    },
    MissingLocalConfig(PathBuf),
    InvalidLocalConfig(PathBuf),
    InvalidGlobalMappings(PathBuf),
    CheckoutRootMismatch {
        configured: PathBuf,
        current: PathBuf,
    },
    WorkspaceMismatch {
        configured: Option<Uuid>,
        expected: Uuid,
    },
    ProjectMismatch {
        configured: Option<Uuid>,
        expected: Option<Uuid>,
    },
    Io {
        operation: &'static str,
        path: PathBuf,
        source: io::Error,
    },
}

impl CheckoutIdentityError {
    pub(crate) fn io(operation: &'static str, path: impl Into<PathBuf>, source: io::Error) -> Self {
        Self::Io {
            operation,
            path: path.into(),
            source,
        }
    }
}

impl fmt::Display for CheckoutIdentityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidCheckoutRoot(path) => {
                write!(
                    formatter,
                    "checkout root is not a readable directory: {}",
                    path.display()
                )
            }
            Self::NotGitCheckout(path) => {
                write!(
                    formatter,
                    "no Git metadata found for checkout: {}",
                    path.display()
                )
            }
            Self::GitMetadataTooLarge(path) => write!(
                formatter,
                "Git metadata exceeds the {} byte safety limit: {}",
                GIT_METADATA_MAX_BYTES,
                path.display()
            ),
            Self::InvalidGitMetadata { path, reason } => {
                write!(
                    formatter,
                    "invalid Git metadata at {}: {reason}",
                    path.display()
                )
            }
            Self::MissingRepositoryFingerprint(path) => {
                write!(
                    formatter,
                    "repository fingerprint is missing: {}",
                    path.display()
                )
            }
            Self::InvalidRepositoryFingerprint { value } => {
                write!(formatter, "invalid repository fingerprint: {value:?}")
            }
            Self::InvalidRepositoryIdentityKind => {
                formatter.write_str("invalid checkout identity kind")
            }
            Self::InvalidCheckoutId { value } => {
                write!(formatter, "invalid checkout identity: {value:?}")
            }
            Self::ConflictingCheckoutIds { local, global } => write!(
                formatter,
                "checkout identity mismatch between local config ({local}) and global mapping ({global})"
            ),
            Self::RepositoryFingerprintMismatch {
                configured,
                current,
            } => write!(
                formatter,
                "repository fingerprint mismatch (configured {configured}, current {current})"
            ),
            Self::InvalidRepositoryRemoteIdentity => {
                formatter.write_str("invalid repository remote identity")
            }
            Self::MissingRepositoryRemoteIdentity { configured } => write!(
                formatter,
                "repository remote identity disappeared (configured {configured})"
            ),
            Self::RepositoryRemoteIdentityMismatch {
                configured,
                current,
            } => write!(
                formatter,
                "repository remote identity mismatch (configured {configured}, current {current})"
            ),
            Self::GitConfigTooLarge(path) => write!(
                formatter,
                "Git config exceeds the {} byte safety limit: {}",
                GIT_CONFIG_MAX_BYTES,
                path.display()
            ),
            Self::InvalidGitConfig { path, reason } => {
                write!(
                    formatter,
                    "invalid Git config at {}: {reason}",
                    path.display()
                )
            }
            Self::MissingLocalConfig(path) => {
                write!(
                    formatter,
                    "checkout-local config is missing: {}",
                    path.display()
                )
            }
            Self::InvalidLocalConfig(path) => {
                write!(
                    formatter,
                    "checkout-local config is invalid: {}",
                    path.display()
                )
            }
            Self::InvalidGlobalMappings(path) => {
                write!(
                    formatter,
                    "global mappings file is invalid: {}",
                    path.display()
                )
            }
            Self::CheckoutRootMismatch {
                configured,
                current,
            } => write!(
                formatter,
                "checkout root mismatch (configured {}, current {})",
                configured.display(),
                current.display()
            ),
            Self::WorkspaceMismatch {
                configured,
                expected,
            } => write!(
                formatter,
                "workspace binding mismatch (configured {configured:?}, expected {expected})"
            ),
            Self::ProjectMismatch {
                configured,
                expected,
            } => write!(
                formatter,
                "project binding mismatch (configured {configured:?}, expected {expected:?})"
            ),
            Self::Io {
                operation,
                path,
                source,
            } => write!(
                formatter,
                "failed to {operation} {}: {source}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for CheckoutIdentityError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Read the repository fingerprint without creating or repairing any files.
pub fn current_repository_fingerprint(
    checkout_root: impl AsRef<Path>,
) -> Result<RepositoryFingerprint, CheckoutIdentityError> {
    let layout = resolve_git_layout(checkout_root.as_ref())?;
    read_repository_marker(&validated_marker_path(&layout.common_dir)?)
}

/// Read the fingerprint for the exact identity kind recorded by an explicit
/// binding. This never falls back between Git and folder markers: changing the
/// marker source would silently bless a replacement directory.
pub fn current_checkout_fingerprint(
    checkout_root: impl AsRef<Path>,
    identity_kind: CheckoutIdentityKind,
) -> Result<RepositoryFingerprint, CheckoutIdentityError> {
    match identity_kind {
        CheckoutIdentityKind::Git => current_repository_fingerprint(checkout_root),
        CheckoutIdentityKind::Folder => {
            let root = canonical_directory(checkout_root.as_ref())?;
            let marker = safe_folder_marker_path(&root, false)?;
            read_repository_marker(&marker)
        }
    }
}

/// Read and normalize the repository's configured Git remote without running
/// Git or retaining credentials.
///
/// The common Git config is authoritative for ordinary checkouts and linked
/// worktrees. When Git's `extensions.worktreeConfig` is enabled, the worktree
/// config is read after the common config, matching Git's precedence. Every
/// file read is individually and cumulatively bounded.
pub fn current_repository_remote_identity(
    checkout_root: impl AsRef<Path>,
) -> Result<Option<RepositoryRemoteIdentity>, CheckoutIdentityError> {
    let layout = resolve_git_layout(checkout_root.as_ref())?;
    read_repository_remote_identity(&layout)
}

/// Return the current checkout's credential-free canonical repository URL.
///
/// This is the safe wire/storage form for session and project resolution. The
/// underlying identity parser removes transport-specific credentials, query
/// strings, and fragments before reconstructing a stable HTTPS URL.
pub fn current_repository_canonical_url(
    checkout_root: impl AsRef<Path>,
) -> Result<Option<String>, CheckoutIdentityError> {
    current_repository_remote_identity(checkout_root)
        .map(|identity| identity.map(|identity| identity.canonical_https_url()))
}

/// Explicitly create a repository fingerprint, or return the existing one.
///
/// Callers must use this only after an explicit setup/rebind/index operation
/// has validated server-side workspace/project ownership. Automatic init,
/// context, search repair, watcher, and hook paths must call the read-only
/// [`current_repository_fingerprint`] function instead.
pub fn ensure_repository_fingerprint(
    checkout_root: impl AsRef<Path>,
) -> Result<RepositoryFingerprint, CheckoutIdentityError> {
    let layout = resolve_git_layout(checkout_root.as_ref())?;
    let marker_dir = layout.common_dir.join(MARKER_DIRECTORY);
    fs::create_dir_all(&marker_dir).map_err(|error| {
        CheckoutIdentityError::io("create marker directory", &marker_dir, error)
    })?;

    let marker_path = validated_marker_path(&layout.common_dir)?;
    match read_repository_marker_with_publish_grace(&marker_path) {
        Ok(fingerprint) => return Ok(fingerprint),
        Err(CheckoutIdentityError::MissingRepositoryFingerprint(_)) => {}
        Err(error) => return Err(error),
    }

    let fingerprint = RepositoryFingerprint::generate();
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    match options.open(&marker_path) {
        Ok(mut file) => {
            let marker = format!("{fingerprint}\n");
            if let Err(error) = file
                .write_all(marker.as_bytes())
                .and_then(|_| file.sync_all())
            {
                let _ = fs::remove_file(&marker_path);
                return Err(CheckoutIdentityError::io(
                    "write repository fingerprint",
                    marker_path,
                    error,
                ));
            }
            Ok(fingerprint)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            // A concurrent explicit establishment won. The final path becomes
            // visible at create_new(), before that winner can finish writing,
            // so use the same bounded publication grace as the initial read.
            read_repository_marker_with_publish_grace(&marker_path)
        }
        Err(error) => Err(CheckoutIdentityError::io(
            "create repository fingerprint",
            marker_path,
            error,
        )),
    }
}

/// Explicitly create the opaque marker for a non-Git folder binding, or
/// return the existing marker. The marker alone grants no authority: content
/// writers also require the matching root/workspace/project local config.
pub fn ensure_folder_fingerprint(
    checkout_root: impl AsRef<Path>,
) -> Result<RepositoryFingerprint, CheckoutIdentityError> {
    let root = canonical_directory(checkout_root.as_ref())?;
    let marker_path = safe_folder_marker_path(&root, true)?;
    let marker_dir = marker_path
        .parent()
        .ok_or_else(|| CheckoutIdentityError::InvalidCheckoutRoot(root.clone()))?;
    fs::create_dir_all(marker_dir).map_err(|error| {
        CheckoutIdentityError::io("create folder marker directory", marker_dir, error)
    })?;
    let marker_path = safe_folder_marker_path(&root, true)?;

    match read_repository_marker_with_publish_grace(&marker_path) {
        Ok(fingerprint) => return Ok(fingerprint),
        Err(CheckoutIdentityError::MissingRepositoryFingerprint(_)) => {}
        Err(error) => return Err(error),
    }

    let fingerprint = RepositoryFingerprint::generate();
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    match options.open(&marker_path) {
        Ok(mut file) => {
            let marker = format!("{fingerprint}\n");
            if let Err(error) = file
                .write_all(marker.as_bytes())
                .and_then(|_| file.sync_all())
            {
                let _ = fs::remove_file(&marker_path);
                return Err(CheckoutIdentityError::io(
                    "write folder fingerprint",
                    marker_path,
                    error,
                ));
            }
            Ok(fingerprint)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            read_repository_marker_with_publish_grace(&marker_path)
        }
        Err(error) => Err(CheckoutIdentityError::io(
            "create folder fingerprint",
            marker_path,
            error,
        )),
    }
}

/// Choose the marker source for an explicit setup/rebind operation.
///
/// Ordinary readers always honor the kind already recorded in local config,
/// so initializing Git cannot silently invalidate or replace a folder binding.
/// An explicit, ownership-checked establishment is different: once the folder
/// is a real Git checkout it upgrades to the common-directory marker, enabling
/// portable remote matching and shared identity across linked worktrees.
pub fn identity_kind_for_establishment(
    checkout_root: impl AsRef<Path>,
) -> Result<CheckoutIdentityKind, CheckoutIdentityError> {
    let root = canonical_directory(checkout_root.as_ref())?;
    match resolve_git_layout(&root) {
        Ok(_) => Ok(CheckoutIdentityKind::Git),
        Err(CheckoutIdentityError::NotGitCheckout(_)) => Ok(CheckoutIdentityKind::Folder),
        Err(error) => Err(error),
    }
}

pub fn ensure_checkout_fingerprint(
    checkout_root: impl AsRef<Path>,
    identity_kind: CheckoutIdentityKind,
) -> Result<RepositoryFingerprint, CheckoutIdentityError> {
    match identity_kind {
        CheckoutIdentityKind::Git => ensure_repository_fingerprint(checkout_root),
        CheckoutIdentityKind::Folder => ensure_folder_fingerprint(checkout_root),
    }
}

/// Read a marker that another explicit establishment may still be publishing.
///
/// `create_new` makes the final path visible before `write_all` and `sync_all`
/// complete. Both the initial read and the create-new loser can therefore
/// observe an empty or partially-written marker. Retry only that transient
/// invalid state for a bounded interval; missing markers and every other error
/// still fail immediately, while persistent corruption remains fail-closed
/// after the grace period.
fn read_repository_marker_with_publish_grace(
    marker_path: &Path,
) -> Result<RepositoryFingerprint, CheckoutIdentityError> {
    let deadline = Instant::now() + REPOSITORY_FINGERPRINT_PUBLISH_GRACE;
    loop {
        match read_repository_marker(marker_path) {
            Err(CheckoutIdentityError::InvalidRepositoryFingerprint { .. })
                if Instant::now() < deadline =>
            {
                std::thread::sleep(REPOSITORY_FINGERPRINT_PUBLISH_POLL);
            }
            result => return result,
        }
    }
}

/// Validate an exact project-bearing checkout binding.
pub fn validate_checkout_binding(
    checkout_root: impl AsRef<Path>,
    expected_workspace_id: Option<Uuid>,
    expected_project_id: Uuid,
) -> Result<ValidatedCheckoutBinding, CheckoutIdentityError> {
    let binding = validate_checkout_scope(
        checkout_root,
        expected_workspace_id,
        Some(expected_project_id),
    )?;
    debug_assert_eq!(binding.project_id, Some(expected_project_id));
    Ok(binding)
}

/// Validate an exact checkout scope. `expected_project_id=None` requires a
/// workspace-only config; it does not mean "ignore the configured project".
pub(crate) fn validate_checkout_scope(
    checkout_root: impl AsRef<Path>,
    expected_workspace_id: Option<Uuid>,
    expected_project_id: Option<Uuid>,
) -> Result<ValidatedCheckoutBinding, CheckoutIdentityError> {
    let root = canonical_directory(checkout_root.as_ref())?;
    let config_path = safe_checkout_config_path(&root, false)?;
    let raw = fs::read_to_string(&config_path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            CheckoutIdentityError::MissingLocalConfig(config_path.clone())
        } else {
            CheckoutIdentityError::io("read checkout-local config", &config_path, error)
        }
    })?;
    let config: Value = parse_value_without_duplicate_keys(&raw)
        .map_err(|_| CheckoutIdentityError::InvalidLocalConfig(config_path.clone()))?;

    let configured_root = config
        .get("checkout_root")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| CheckoutIdentityError::InvalidLocalConfig(config_path.clone()))?;
    if configured_root != root {
        return Err(CheckoutIdentityError::CheckoutRootMismatch {
            configured: configured_root,
            current: root,
        });
    }

    let configured_workspace_id = config
        .get("workspace_id")
        .and_then(Value::as_str)
        .and_then(|value| Uuid::parse_str(value.trim()).ok())
        .ok_or_else(|| CheckoutIdentityError::InvalidLocalConfig(config_path.clone()))?;
    let workspace_id = configured_workspace_id;
    if expected_workspace_id.is_some_and(|expected| expected != workspace_id) {
        return Err(CheckoutIdentityError::WorkspaceMismatch {
            configured: Some(workspace_id),
            expected: expected_workspace_id.expect("checked Some"),
        });
    }

    let configured_project_id = match config.get("project_id") {
        None => None,
        Some(value) => Some(
            value
                .as_str()
                .and_then(|value| Uuid::parse_str(value.trim()).ok())
                .ok_or_else(|| CheckoutIdentityError::InvalidLocalConfig(config_path.clone()))?,
        ),
    };
    if configured_project_id != expected_project_id {
        return Err(CheckoutIdentityError::ProjectMismatch {
            configured: configured_project_id,
            expected: expected_project_id,
        });
    }

    let identity_kind =
        CheckoutIdentityKind::from_config_value(config.get(CheckoutIdentityKind::FIELD))?;
    let configured_fingerprint = config
        .get("repository_fingerprint")
        .and_then(Value::as_str)
        .ok_or_else(|| CheckoutIdentityError::MissingRepositoryFingerprint(config_path.clone()))
        .and_then(RepositoryFingerprint::parse)?;
    let current_fingerprint = current_checkout_fingerprint(&root, identity_kind)?;
    if configured_fingerprint != current_fingerprint {
        return Err(CheckoutIdentityError::RepositoryFingerprintMismatch {
            configured: configured_fingerprint,
            current: current_fingerprint,
        });
    }

    let repository_remote_identity = match identity_kind {
        CheckoutIdentityKind::Git => {
            validate_configured_remote_identity(&root, config.get("repository_remote_identity"))?
        }
        CheckoutIdentityKind::Folder if config.get("repository_remote_identity").is_none() => None,
        CheckoutIdentityKind::Folder => {
            return Err(CheckoutIdentityError::InvalidLocalConfig(config_path))
        }
    };
    let checkout_id = match config.get("checkout_id") {
        Some(value) => value
            .as_str()
            .ok_or_else(|| CheckoutIdentityError::InvalidCheckoutId {
                value: value.to_string(),
            })
            .and_then(CheckoutId::parse)?,
        None => CheckoutId::for_legacy_binding(&root, &current_fingerprint),
    };

    Ok(ValidatedCheckoutBinding {
        checkout_root: root,
        checkout_id,
        workspace_id,
        project_id: configured_project_id,
        identity_kind,
        repository_fingerprint: current_fingerprint,
        repository_remote_identity,
    })
}

/// Validate an optional serialized remote identity against the live checkout.
///
/// Absence deliberately means a legacy/no-remote marker-only binding. This
/// function never derives and returns a new identity for such a binding, so an
/// automatic refresh cannot silently upgrade or rebind it.
pub(crate) fn validate_configured_remote_identity(
    checkout_root: impl AsRef<Path>,
    configured: Option<&Value>,
) -> Result<Option<RepositoryRemoteIdentity>, CheckoutIdentityError> {
    let Some(configured) = configured else {
        return Ok(None);
    };
    let configured = configured
        .as_str()
        .ok_or(CheckoutIdentityError::InvalidRepositoryRemoteIdentity)
        .and_then(RepositoryRemoteIdentity::parse)?;
    match current_repository_remote_identity(checkout_root)? {
        Some(current) if current == configured => Ok(Some(current)),
        Some(current) => Err(CheckoutIdentityError::RepositoryRemoteIdentityMismatch {
            configured,
            current,
        }),
        None => Err(CheckoutIdentityError::MissingRepositoryRemoteIdentity { configured }),
    }
}

/// Resolve a checkout-local config path while rejecting symlink escapes and
/// non-regular path components. Explicit establishment may allow the directory
/// or file to be absent, but never follows an existing symlink.
pub(crate) fn safe_checkout_config_path(
    checkout_root: &Path,
    allow_missing: bool,
) -> Result<PathBuf, CheckoutIdentityError> {
    let config_dir = checkout_root.join(".contextstream");
    match fs::symlink_metadata(&config_dir) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => return Err(CheckoutIdentityError::InvalidLocalConfig(config_dir)),
        Err(error) if error.kind() == io::ErrorKind::NotFound && allow_missing => {
            return Ok(config_dir.join("config.json"));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(CheckoutIdentityError::MissingLocalConfig(
                config_dir.join("config.json"),
            ));
        }
        Err(error) => {
            return Err(CheckoutIdentityError::io(
                "inspect checkout config directory",
                config_dir,
                error,
            ));
        }
    }

    let config_path = config_dir.join("config.json");
    match fs::symlink_metadata(&config_path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(config_path),
        Ok(_) => Err(CheckoutIdentityError::InvalidLocalConfig(config_path)),
        Err(error) if error.kind() == io::ErrorKind::NotFound && allow_missing => Ok(config_path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            Err(CheckoutIdentityError::MissingLocalConfig(config_path))
        }
        Err(error) => Err(CheckoutIdentityError::io(
            "inspect checkout-local config",
            config_path,
            error,
        )),
    }
}

/// Resolve the non-Git folder marker without following a symlink or accepting
/// a non-regular marker. The marker shares the already-protected
/// `.contextstream` directory with the checkout-local authorization config.
fn safe_folder_marker_path(
    checkout_root: &Path,
    allow_missing: bool,
) -> Result<PathBuf, CheckoutIdentityError> {
    let config_path = match safe_checkout_config_path(checkout_root, allow_missing) {
        Ok(path) => path,
        Err(CheckoutIdentityError::MissingLocalConfig(_)) if !allow_missing => {
            return Err(CheckoutIdentityError::MissingRepositoryFingerprint(
                checkout_root
                    .join(".contextstream")
                    .join(FOLDER_MARKER_FILE),
            ))
        }
        Err(error) => return Err(error),
    };
    let marker_path = config_path
        .parent()
        .ok_or_else(|| CheckoutIdentityError::InvalidCheckoutRoot(checkout_root.to_path_buf()))?
        .join(FOLDER_MARKER_FILE);
    match fs::symlink_metadata(&marker_path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(marker_path),
        Ok(_) => Err(CheckoutIdentityError::InvalidLocalConfig(marker_path)),
        Err(error) if error.kind() == io::ErrorKind::NotFound && allow_missing => Ok(marker_path),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Err(
            CheckoutIdentityError::MissingRepositoryFingerprint(marker_path),
        ),
        Err(error) => Err(CheckoutIdentityError::io(
            "inspect folder fingerprint",
            marker_path,
            error,
        )),
    }
}

fn canonical_directory(path: &Path) -> Result<PathBuf, CheckoutIdentityError> {
    let canonical = fs::canonicalize(path)
        .map_err(|_| CheckoutIdentityError::InvalidCheckoutRoot(path.to_path_buf()))?;
    if !canonical.is_dir() {
        return Err(CheckoutIdentityError::InvalidCheckoutRoot(
            path.to_path_buf(),
        ));
    }
    Ok(canonical)
}

#[derive(Debug)]
struct GitLayout {
    git_dir: PathBuf,
    common_dir: PathBuf,
}

fn resolve_git_layout(checkout_root: &Path) -> Result<GitLayout, CheckoutIdentityError> {
    let root = canonical_directory(checkout_root)?;
    let mut current = Some(root.as_path());
    let (repository_root, dot_git) = loop {
        let Some(candidate_root) = current else {
            return Err(CheckoutIdentityError::NotGitCheckout(root));
        };
        let candidate = candidate_root.join(".git");
        match fs::symlink_metadata(&candidate) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(CheckoutIdentityError::InvalidGitMetadata {
                        path: candidate,
                        reason: ".git symlinks are not accepted",
                    });
                }
                break (candidate_root.to_path_buf(), candidate);
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                current = candidate_root.parent();
            }
            Err(error) => {
                return Err(CheckoutIdentityError::io(
                    "inspect Git metadata",
                    candidate,
                    error,
                ));
            }
        }
    };

    let dot_git_metadata = fs::symlink_metadata(&dot_git)
        .map_err(|error| CheckoutIdentityError::io("inspect Git metadata", &dot_git, error))?;
    let git_dir = if dot_git_metadata.is_dir() {
        fs::canonicalize(&dot_git).map_err(|error| {
            CheckoutIdentityError::io("canonicalize Git directory", &dot_git, error)
        })?
    } else if dot_git_metadata.is_file() {
        let raw = read_bounded_text(&dot_git)?;
        let gitdir = parse_single_path_directive(&raw, Some("gitdir:"), &dot_git)?;
        let candidate = if gitdir.is_absolute() {
            gitdir
        } else {
            repository_root.join(gitdir)
        };
        canonical_existing_directory(&candidate, "Git directory")?
    } else {
        return Err(CheckoutIdentityError::InvalidGitMetadata {
            path: dot_git,
            reason: ".git is neither a directory nor a regular file",
        });
    };

    let common_file = git_dir.join("commondir");
    let common_dir = match fs::symlink_metadata(&common_file) {
        Ok(metadata) => {
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(CheckoutIdentityError::InvalidGitMetadata {
                    path: common_file,
                    reason: "commondir is not a regular file",
                });
            }
            let raw = read_bounded_text(&common_file)?;
            let common = parse_single_path_directive(&raw, None, &common_file)?;
            let candidate = if common.is_absolute() {
                common
            } else {
                git_dir.join(common)
            };
            canonical_existing_directory(&candidate, "Git common directory")?
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => git_dir.clone(),
        Err(error) => {
            return Err(CheckoutIdentityError::io(
                "inspect Git common directory metadata",
                common_file,
                error,
            ));
        }
    };
    Ok(GitLayout {
        git_dir,
        common_dir,
    })
}

#[derive(Default)]
struct ParsedGitConfig {
    remote_urls: BTreeMap<String, Vec<String>>,
    worktree_config_enabled: bool,
}

struct GitConfigReader {
    remaining_bytes: u64,
    visited: BTreeSet<PathBuf>,
}

impl GitConfigReader {
    fn new() -> Self {
        Self {
            remaining_bytes: GIT_CONFIG_MAX_BYTES,
            visited: BTreeSet::new(),
        }
    }

    fn read_optional_into(
        &mut self,
        path: &Path,
        depth: usize,
        parsed: &mut ParsedGitConfig,
    ) -> Result<bool, CheckoutIdentityError> {
        match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
            Ok(_) => {
                return Err(CheckoutIdentityError::InvalidGitConfig {
                    path: path.to_path_buf(),
                    reason: "expected a regular config file",
                });
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(CheckoutIdentityError::io("inspect Git config", path, error));
            }
        }
        if depth > GIT_CONFIG_INCLUDE_DEPTH {
            return Err(CheckoutIdentityError::InvalidGitConfig {
                path: path.to_path_buf(),
                reason: "include nesting exceeds the safety limit",
            });
        }
        let visit_key = fs::canonicalize(path)
            .map_err(|error| CheckoutIdentityError::io("canonicalize Git config", path, error))?;
        if !self.visited.insert(visit_key.clone()) {
            return Err(CheckoutIdentityError::InvalidGitConfig {
                path: path.to_path_buf(),
                reason: "include cycle detected",
            });
        }

        // Read through the validated original path, not the canonicalized
        // cycle-detection key. The bounded reader compares the opened file's
        // identity with a second lstat before reading, so a concurrent symlink
        // swap cannot redirect this operation to another file.
        let raw = read_bounded_git_config(path, &mut self.remaining_bytes)?;
        let result = self.parse_into(path, &raw, depth, parsed);
        self.visited.remove(&visit_key);
        result.map(|()| true)
    }

    fn parse_into(
        &mut self,
        path: &Path,
        raw: &str,
        depth: usize,
        parsed: &mut ParsedGitConfig,
    ) -> Result<(), CheckoutIdentityError> {
        let mut section: Option<(String, Option<String>)> = None;
        for line in git_config_logical_lines(raw, path)? {
            let trimmed = line.trim_start_matches('\u{feff}').trim();
            if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
                continue;
            }
            if trimmed.starts_with('[') {
                section = Some(parse_git_config_section(trimmed, path)?);
                continue;
            }

            let Some((section_name, subsection)) = section.as_ref() else {
                return Err(CheckoutIdentityError::InvalidGitConfig {
                    path: path.to_path_buf(),
                    reason: "setting appears before a section header",
                });
            };
            let (key, value) = parse_git_config_assignment(trimmed, path)?;
            match (section_name.as_str(), subsection.as_deref(), key.as_str()) {
                ("remote", Some(remote), "url") if !value.trim().is_empty() => {
                    parsed
                        .remote_urls
                        .entry(remote.to_string())
                        .or_default()
                        .push(value);
                }
                ("extensions", None, "worktreeconfig") => {
                    parsed.worktree_config_enabled = parse_git_bool(&value).ok_or_else(|| {
                        CheckoutIdentityError::InvalidGitConfig {
                            path: path.to_path_buf(),
                            reason: "invalid extensions.worktreeConfig value",
                        }
                    })?;
                }
                ("include", None, "path") => {
                    let include_path = resolve_git_include_path(path, &value)?;
                    self.read_optional_into(&include_path, depth + 1, parsed)?;
                }
                ("includeif", _, "path") => {
                    return Err(CheckoutIdentityError::InvalidGitConfig {
                        path: path.to_path_buf(),
                        reason: "conditional includes are not supported for repository identity",
                    });
                }
                _ => {}
            }
        }
        Ok(())
    }
}

fn read_repository_remote_identity(
    layout: &GitLayout,
) -> Result<Option<RepositoryRemoteIdentity>, CheckoutIdentityError> {
    let mut reader = GitConfigReader::new();
    let mut parsed = ParsedGitConfig::default();
    let common_config = layout.common_dir.join("config");
    if !reader.read_optional_into(&common_config, 0, &mut parsed)? {
        return Ok(None);
    }
    if parsed.worktree_config_enabled {
        let worktree_config = layout.git_dir.join("config.worktree");
        let mut worktree = ParsedGitConfig::default();
        if reader.read_optional_into(&worktree_config, 0, &mut worktree)? {
            // `config.worktree` has higher precedence than the shared config.
            // Overlay each remote as a complete setting group; appending would
            // incorrectly turn a valid per-worktree override into an
            // "ambiguous remotes" failure.
            for (remote, urls) in worktree.remote_urls {
                parsed.remote_urls.insert(remote, urls);
            }
        }
    }
    select_repository_remote(&parsed, &common_config)
}

fn select_repository_remote(
    parsed: &ParsedGitConfig,
    config_path: &Path,
) -> Result<Option<RepositoryRemoteIdentity>, CheckoutIdentityError> {
    let exact_origin = parsed.remote_urls.get("origin");
    let case_folded_origins = parsed
        .remote_urls
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("origin"))
        .collect::<Vec<_>>();
    let selected_urls = if let Some(urls) = exact_origin {
        Some(urls.as_slice())
    } else if case_folded_origins.len() == 1 {
        Some(case_folded_origins[0].1.as_slice())
    } else if case_folded_origins.len() > 1 {
        return Err(CheckoutIdentityError::InvalidGitConfig {
            path: config_path.to_path_buf(),
            reason: "multiple case-variant origin remotes are ambiguous",
        });
    } else {
        None
    };

    let identities = if let Some(urls) = selected_urls {
        normalize_remote_set(urls, config_path)?
    } else {
        normalize_remote_set(
            &parsed
                .remote_urls
                .values()
                .flat_map(|urls| urls.iter().cloned())
                .collect::<Vec<_>>(),
            config_path,
        )?
    };
    match identities.len() {
        0 => Ok(None),
        1 => Ok(identities.into_iter().next()),
        _ => Err(CheckoutIdentityError::InvalidGitConfig {
            path: config_path.to_path_buf(),
            reason: "multiple remote repository identities are ambiguous",
        }),
    }
}

fn normalize_remote_set(
    urls: &[String],
    config_path: &Path,
) -> Result<BTreeSet<RepositoryRemoteIdentity>, CheckoutIdentityError> {
    urls.iter()
        .map(|url| {
            RepositoryRemoteIdentity::from_remote_url(url).map_err(|_| {
                CheckoutIdentityError::InvalidGitConfig {
                    path: config_path.to_path_buf(),
                    reason: "remote URL has no safe host and namespace identity",
                }
            })
        })
        .collect()
}

fn git_config_logical_lines(raw: &str, path: &Path) -> Result<Vec<String>, CheckoutIdentityError> {
    let mut logical = Vec::new();
    let mut current = String::new();
    for physical in raw.lines() {
        if current.is_empty() {
            current.push_str(physical);
        } else {
            current.push_str(physical.trim_start());
        }
        let trailing_backslashes = current
            .as_bytes()
            .iter()
            .rev()
            .take_while(|byte| **byte == b'\\')
            .count();
        if trailing_backslashes % 2 == 1 {
            current.pop();
            continue;
        }
        logical.push(std::mem::take(&mut current));
    }
    if !current.is_empty() {
        return Err(CheckoutIdentityError::InvalidGitConfig {
            path: path.to_path_buf(),
            reason: "unterminated line continuation",
        });
    }
    Ok(logical)
}

fn parse_git_config_section(
    line: &str,
    path: &Path,
) -> Result<(String, Option<String>), CheckoutIdentityError> {
    let mut quoted = false;
    let mut escaped = false;
    let mut close = None;
    for (index, ch) in line.char_indices().skip(1) {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ']' if !quoted => {
                close = Some(index);
                break;
            }
            _ => {}
        }
    }
    let close = close.ok_or_else(|| CheckoutIdentityError::InvalidGitConfig {
        path: path.to_path_buf(),
        reason: "unterminated section header",
    })?;
    let trailing = line[close + 1..].trim();
    if !trailing.is_empty() && !trailing.starts_with('#') && !trailing.starts_with(';') {
        return Err(CheckoutIdentityError::InvalidGitConfig {
            path: path.to_path_buf(),
            reason: "unexpected text after section header",
        });
    }
    let body = line[1..close].trim();
    if body.is_empty() {
        return Err(CheckoutIdentityError::InvalidGitConfig {
            path: path.to_path_buf(),
            reason: "empty section header",
        });
    }

    let split = body
        .char_indices()
        .find(|(_, ch)| ch.is_whitespace())
        .map(|(index, _)| index);
    let (raw_section, subsection) = match split {
        Some(index) => {
            let raw_subsection = body[index..].trim();
            if !raw_subsection.starts_with('"') || !raw_subsection.ends_with('"') {
                return Err(CheckoutIdentityError::InvalidGitConfig {
                    path: path.to_path_buf(),
                    reason: "subsection must be quoted",
                });
            }
            (
                &body[..index],
                Some(parse_git_config_value(raw_subsection, path)?),
            )
        }
        None => {
            if let Some((section, subsection)) = body.split_once('.') {
                (section, Some(subsection.to_string()))
            } else {
                (body, None)
            }
        }
    };
    if raw_section.is_empty()
        || !raw_section
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
    {
        return Err(CheckoutIdentityError::InvalidGitConfig {
            path: path.to_path_buf(),
            reason: "invalid section name",
        });
    }
    Ok((raw_section.to_ascii_lowercase(), subsection))
}

fn parse_git_config_assignment(
    line: &str,
    path: &Path,
) -> Result<(String, String), CheckoutIdentityError> {
    let separator = line.find(|ch: char| ch == '=' || ch.is_whitespace());
    let (raw_key, raw_value) = match separator {
        Some(index) => {
            let tail = &line[index..];
            let tail = tail.trim_start();
            let tail = tail.strip_prefix('=').unwrap_or(tail).trim_start();
            (&line[..index], tail)
        }
        None => (line, "true"),
    };
    let key = raw_key.trim().to_ascii_lowercase();
    if key.is_empty()
        || !key
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
    {
        return Err(CheckoutIdentityError::InvalidGitConfig {
            path: path.to_path_buf(),
            reason: "invalid setting name",
        });
    }
    Ok((key, parse_git_config_value(raw_value, path)?))
}

fn parse_git_config_value(value: &str, path: &Path) -> Result<String, CheckoutIdentityError> {
    let mut parsed = String::new();
    let mut quoted = false;
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => quoted = !quoted,
            '#' | ';' if !quoted => break,
            '\\' => {
                let escaped =
                    chars
                        .next()
                        .ok_or_else(|| CheckoutIdentityError::InvalidGitConfig {
                            path: path.to_path_buf(),
                            reason: "unterminated value escape",
                        })?;
                parsed.push(match escaped {
                    'n' => '\n',
                    't' => '\t',
                    'b' => '\u{0008}',
                    '\\' => '\\',
                    '"' => '"',
                    _ => {
                        return Err(CheckoutIdentityError::InvalidGitConfig {
                            path: path.to_path_buf(),
                            reason: "unsupported value escape",
                        });
                    }
                });
            }
            _ => parsed.push(ch),
        }
    }
    if quoted {
        return Err(CheckoutIdentityError::InvalidGitConfig {
            path: path.to_path_buf(),
            reason: "unterminated quoted value",
        });
    }
    Ok(parsed.trim().to_string())
}

fn parse_git_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Some(true),
        "false" | "no" | "off" | "0" => Some(false),
        _ => None,
    }
}

fn resolve_git_include_path(
    including_config: &Path,
    value: &str,
) -> Result<PathBuf, CheckoutIdentityError> {
    let expanded = if let Some(rest) = value.strip_prefix("~/") {
        let home =
            std::env::var_os("HOME").ok_or_else(|| CheckoutIdentityError::InvalidGitConfig {
                path: including_config.to_path_buf(),
                reason: "HOME is unavailable for included config",
            })?;
        PathBuf::from(home).join(rest)
    } else if value.starts_with("%(") {
        return Err(CheckoutIdentityError::InvalidGitConfig {
            path: including_config.to_path_buf(),
            reason: "runtime-prefixed includes are not supported for repository identity",
        });
    } else {
        PathBuf::from(value)
    };
    if expanded.is_absolute() {
        Ok(expanded)
    } else {
        including_config
            .parent()
            .map(|parent| parent.join(expanded))
            .ok_or_else(|| CheckoutIdentityError::InvalidGitConfig {
                path: including_config.to_path_buf(),
                reason: "included config has no parent directory",
            })
    }
}

fn normalize_remote_url(value: &str) -> Result<String, ()> {
    let value = value.trim();
    if value.is_empty() || value.chars().any(char::is_control) {
        return Err(());
    }

    let (host, namespace) = if let Some(scheme_index) = value.find("://") {
        let scheme = &value[..scheme_index];
        if scheme.is_empty()
            || !scheme.chars().enumerate().all(|(index, ch)| {
                ch.is_ascii_alphabetic()
                    || (index > 0 && (ch.is_ascii_digit() || matches!(ch, '+' | '-' | '.')))
            })
        {
            return Err(());
        }
        let remainder = &value[scheme_index + 3..];
        let authority_end = remainder.find(['/', '?', '#']).unwrap_or(remainder.len());
        let authority = &remainder[..authority_end];
        let authority = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        let host = normalize_remote_host(authority, Some(scheme))?;
        let path = remainder.get(authority_end..).unwrap_or_default();
        let path_end = path.find(['?', '#']).unwrap_or(path.len());
        (host, normalize_remote_namespace(&path[..path_end])?)
    } else {
        let (authority, path) = split_scp_remote(value)?;
        let authority = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        (
            normalize_remote_host(authority, Some("ssh"))?,
            normalize_remote_namespace(path)?,
        )
    };
    Ok(format!("{REMOTE_IDENTITY_PREFIX}{host}/{namespace}"))
}

fn split_scp_remote(value: &str) -> Result<(&str, &str), ()> {
    if value.starts_with('/') || value.starts_with("./") || value.starts_with("../") {
        return Err(());
    }
    if let Some(open) = value.find('[') {
        let close = value[open + 1..]
            .find(']')
            .map(|index| open + 1 + index)
            .ok_or(())?;
        let colon = close + 1;
        if value.as_bytes().get(colon) != Some(&b':') {
            return Err(());
        }
        return Ok((&value[..colon], &value[colon + 1..]));
    }
    let colon = value.find(':').ok_or(())?;
    if colon == 1 && value.as_bytes()[0].is_ascii_alphabetic() {
        return Err(());
    }
    let authority = &value[..colon];
    if authority.is_empty() || authority.contains('/') || authority.contains('\\') {
        return Err(());
    }
    Ok((authority, &value[colon + 1..]))
}

fn normalize_remote_host(authority: &str, scheme: Option<&str>) -> Result<String, ()> {
    if authority.is_empty()
        || authority.contains('@')
        || authority.contains('/')
        || authority.contains('\\')
        || authority
            .chars()
            .any(|ch| ch.is_control() || ch.is_whitespace())
    {
        return Err(());
    }
    let (raw_host, port) = if authority.starts_with('[') {
        let close = authority.find(']').ok_or(())?;
        let host = &authority[..=close];
        let suffix = &authority[close + 1..];
        let port = if suffix.is_empty() {
            None
        } else {
            Some(suffix.strip_prefix(':').ok_or(())?)
        };
        (host, port)
    } else {
        let colon_count = authority.bytes().filter(|byte| *byte == b':').count();
        if colon_count > 1 {
            return Err(());
        }
        if colon_count == 1 {
            let (host, port) = authority.rsplit_once(':').ok_or(())?;
            (host, Some(port))
        } else {
            (authority, None)
        }
    };

    let host = if raw_host.starts_with('[') {
        let inner = raw_host
            .strip_prefix('[')
            .and_then(|host| host.strip_suffix(']'))
            .ok_or(())?;
        let address = inner.parse::<std::net::Ipv6Addr>().map_err(|_| ())?;
        format!("[{address}]")
    } else {
        let host = raw_host.trim_end_matches('.').to_ascii_lowercase();
        if host.is_empty()
            || !host
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_'))
        {
            return Err(());
        }
        host
    };

    let Some(port) = port else {
        return Ok(host);
    };
    if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(());
    }
    let port = port.parse::<u16>().map_err(|_| ())?;
    if port == 0 {
        return Err(());
    }
    let is_default = scheme.is_some_and(|scheme| match scheme.to_ascii_lowercase().as_str() {
        "ssh" => port == 22,
        "http" => port == 80,
        "https" => port == 443,
        "git" => port == 9418,
        _ => false,
    });
    if is_default {
        Ok(host)
    } else {
        Ok(format!("{host}:{port}"))
    }
}

fn normalize_remote_namespace(path: &str) -> Result<String, ()> {
    let path = path.trim().trim_matches('/');
    if path.is_empty() || path.chars().any(char::is_control) {
        return Err(());
    }
    let mut segments = Vec::new();
    for raw_segment in path.split('/') {
        if raw_segment.is_empty() || raw_segment == "." {
            continue;
        }
        if raw_segment == ".." {
            segments.pop().ok_or(())?;
            continue;
        }
        segments.push(normalize_percent_encoding(raw_segment)?);
    }
    let last = segments.last_mut().ok_or(())?;
    if last.to_ascii_lowercase().ends_with(".git") {
        last.truncate(last.len() - 4);
    }
    if last.is_empty() {
        return Err(());
    }
    Ok(segments.join("/"))
}

fn normalize_percent_encoding(segment: &str) -> Result<String, ()> {
    let bytes = segment.as_bytes();
    let mut result = String::with_capacity(segment.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'%' {
            let ch = segment[index..].chars().next().ok_or(())?;
            result.push(ch);
            index += ch.len_utf8();
            continue;
        }
        let high = *bytes.get(index + 1).ok_or(())?;
        let low = *bytes.get(index + 2).ok_or(())?;
        let decoded = (hex_value(high)? << 4) | hex_value(low)?;
        if decoded.is_ascii_alphanumeric() || matches!(decoded, b'-' | b'.' | b'_' | b'~') {
            result.push(decoded as char);
        } else {
            result.push('%');
            result.push(char::from(high).to_ascii_uppercase());
            result.push(char::from(low).to_ascii_uppercase());
        }
        index += 3;
    }
    Ok(result)
}

fn hex_value(value: u8) -> Result<u8, ()> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err(()),
    }
}

fn read_bounded_git_config(
    path: &Path,
    remaining_bytes: &mut u64,
) -> Result<String, CheckoutIdentityError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| CheckoutIdentityError::io("inspect Git config", path, error))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(CheckoutIdentityError::InvalidGitConfig {
            path: path.to_path_buf(),
            reason: "expected a regular config file",
        });
    }
    if metadata.len() > *remaining_bytes {
        return Err(CheckoutIdentityError::GitConfigTooLarge(path.to_path_buf()));
    }
    let file = File::open(path)
        .map_err(|error| CheckoutIdentityError::io("open Git config", path, error))?;
    let opened_metadata = file
        .metadata()
        .map_err(|error| CheckoutIdentityError::io("inspect open Git config", path, error))?;
    let current_metadata = fs::symlink_metadata(path)
        .map_err(|error| CheckoutIdentityError::io("reinspect Git config", path, error))?;
    if !same_file_identity(&opened_metadata, &current_metadata)
        || !current_metadata.is_file()
        || current_metadata.file_type().is_symlink()
    {
        return Err(CheckoutIdentityError::InvalidGitConfig {
            path: path.to_path_buf(),
            reason: "config changed while it was being opened",
        });
    }
    let limit = *remaining_bytes;
    let mut bytes = Vec::with_capacity(opened_metadata.len() as usize);
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| CheckoutIdentityError::io("read Git config", path, error))?;
    if bytes.len() as u64 > limit {
        return Err(CheckoutIdentityError::GitConfigTooLarge(path.to_path_buf()));
    }
    *remaining_bytes -= bytes.len() as u64;
    String::from_utf8(bytes).map_err(|_| CheckoutIdentityError::InvalidGitConfig {
        path: path.to_path_buf(),
        reason: "config is not UTF-8",
    })
}

#[cfg(unix)]
fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file_identity(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.len() == right.len()
        && left.modified().ok() == right.modified().ok()
        && left.file_type() == right.file_type()
}

fn canonical_existing_directory(
    path: &Path,
    label: &'static str,
) -> Result<PathBuf, CheckoutIdentityError> {
    let canonical = fs::canonicalize(path).map_err(|error| {
        CheckoutIdentityError::io("canonicalize Git metadata path", path, error)
    })?;
    if !canonical.is_dir() {
        return Err(CheckoutIdentityError::InvalidGitMetadata {
            path: path.to_path_buf(),
            reason: label,
        });
    }
    Ok(canonical)
}

fn parse_single_path_directive(
    raw: &str,
    prefix: Option<&str>,
    path: &Path,
) -> Result<PathBuf, CheckoutIdentityError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.lines().count() != 1 {
        return Err(CheckoutIdentityError::InvalidGitMetadata {
            path: path.to_path_buf(),
            reason: "expected one non-empty path line",
        });
    }
    let value = match prefix {
        Some(prefix) => trimmed.strip_prefix(prefix).map(str::trim).ok_or_else(|| {
            CheckoutIdentityError::InvalidGitMetadata {
                path: path.to_path_buf(),
                reason: "missing gitdir directive",
            }
        })?,
        None => trimmed,
    };
    if value.is_empty() {
        return Err(CheckoutIdentityError::InvalidGitMetadata {
            path: path.to_path_buf(),
            reason: "path is empty",
        });
    }
    Ok(PathBuf::from(value))
}

fn read_bounded_text(path: &Path) -> Result<String, CheckoutIdentityError> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| CheckoutIdentityError::io("inspect Git metadata file", path, error))?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(CheckoutIdentityError::InvalidGitMetadata {
            path: path.to_path_buf(),
            reason: "expected a regular metadata file",
        });
    }
    if metadata.len() > GIT_METADATA_MAX_BYTES {
        return Err(CheckoutIdentityError::GitMetadataTooLarge(
            path.to_path_buf(),
        ));
    }
    let file = File::open(path)
        .map_err(|error| CheckoutIdentityError::io("open Git metadata file", path, error))?;
    let opened_metadata = file.metadata().map_err(|error| {
        CheckoutIdentityError::io("inspect open Git metadata file", path, error)
    })?;
    let current_metadata = fs::symlink_metadata(path)
        .map_err(|error| CheckoutIdentityError::io("reinspect Git metadata file", path, error))?;
    if !same_file_identity(&opened_metadata, &current_metadata)
        || !current_metadata.is_file()
        || current_metadata.file_type().is_symlink()
    {
        return Err(CheckoutIdentityError::InvalidGitMetadata {
            path: path.to_path_buf(),
            reason: "metadata changed while it was being opened",
        });
    }
    if opened_metadata.len() > GIT_METADATA_MAX_BYTES {
        return Err(CheckoutIdentityError::GitMetadataTooLarge(
            path.to_path_buf(),
        ));
    }
    let mut bytes = Vec::with_capacity(opened_metadata.len() as usize);
    file.take(GIT_METADATA_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| CheckoutIdentityError::io("read Git metadata file", path, error))?;
    if bytes.len() as u64 > GIT_METADATA_MAX_BYTES {
        return Err(CheckoutIdentityError::GitMetadataTooLarge(
            path.to_path_buf(),
        ));
    }
    String::from_utf8(bytes).map_err(|_| CheckoutIdentityError::InvalidGitMetadata {
        path: path.to_path_buf(),
        reason: "metadata is not UTF-8",
    })
}

fn read_repository_marker(path: &Path) -> Result<RepositoryFingerprint, CheckoutIdentityError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(CheckoutIdentityError::InvalidGitMetadata {
                    path: path.to_path_buf(),
                    reason: "repository fingerprint is not a regular file",
                });
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(CheckoutIdentityError::MissingRepositoryFingerprint(
                path.to_path_buf(),
            ));
        }
        Err(error) => {
            return Err(CheckoutIdentityError::io(
                "inspect repository fingerprint",
                path,
                error,
            ));
        }
    }
    let raw = read_bounded_text(path)?;
    RepositoryFingerprint::parse(&raw)
}

fn validated_marker_path(common_dir: &Path) -> Result<PathBuf, CheckoutIdentityError> {
    let marker_dir = common_dir.join(MARKER_DIRECTORY);
    let marker_path = marker_dir.join(MARKER_FILE);
    let marker_dir_metadata = match fs::symlink_metadata(&marker_dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(CheckoutIdentityError::MissingRepositoryFingerprint(
                marker_path,
            ));
        }
        Err(error) => {
            return Err(CheckoutIdentityError::io(
                "inspect marker directory",
                marker_dir,
                error,
            ));
        }
    };
    // Git's common directory may be outside the worktree (normal linked-
    // worktree behavior), but this child must be a real directory inside that
    // canonical common directory.
    if marker_dir_metadata.file_type().is_symlink() || !marker_dir_metadata.is_dir() {
        return Err(CheckoutIdentityError::InvalidGitMetadata {
            path: marker_dir,
            reason: "marker directory is not a regular directory",
        });
    }
    let canonical_marker_dir = fs::canonicalize(&marker_dir).map_err(|error| {
        CheckoutIdentityError::io("canonicalize marker directory", &marker_dir, error)
    })?;
    if !canonical_marker_dir.starts_with(common_dir) {
        return Err(CheckoutIdentityError::InvalidGitMetadata {
            path: marker_dir,
            reason: "marker directory escapes the Git common directory",
        });
    }
    Ok(canonical_marker_dir.join(MARKER_FILE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

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

    #[test]
    fn normal_git_checkout_gets_stable_versioned_fingerprint() {
        let checkout = tempdir().expect("checkout");
        create_git_checkout(checkout.path());

        assert!(matches!(
            current_repository_fingerprint(checkout.path()),
            Err(CheckoutIdentityError::MissingRepositoryFingerprint(_))
        ));
        let created = ensure_repository_fingerprint(checkout.path()).expect("establish identity");
        assert!(created.as_str().starts_with(FINGERPRINT_PREFIX));
        assert_eq!(
            current_repository_fingerprint(checkout.path()).expect("read identity"),
            created
        );
        assert_eq!(
            fs::read_to_string(checkout.path().join(".git/contextstream/repository-id"))
                .expect("marker")
                .trim(),
            created.as_str()
        );
    }

    #[test]
    fn explicit_non_git_folder_binding_survives_first_files_and_later_git_init() {
        let folder = tempdir().expect("folder");
        let fingerprint =
            ensure_folder_fingerprint(folder.path()).expect("establish folder identity");
        let workspace_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        let checkout_id = CheckoutId::generate();
        let canonical_root = fs::canonicalize(folder.path()).expect("canonical folder");
        let config_path = folder.path().join(".contextstream/config.json");
        fs::write(
            &config_path,
            serde_json::to_string_pretty(&serde_json::json!({
                "workspace_id": workspace_id,
                "project_id": project_id,
                "checkout_root": canonical_root,
                "checkout_id": checkout_id.as_str(),
                "checkout_identity_kind": "folder",
                "repository_fingerprint": fingerprint.as_str(),
            }))
            .expect("config"),
        )
        .expect("write config");

        let assert_folder_binding = || {
            let binding = validate_checkout_binding(folder.path(), Some(workspace_id), project_id)
                .expect("validated folder binding");
            assert_eq!(binding.identity_kind, CheckoutIdentityKind::Folder);
            assert_eq!(binding.repository_fingerprint, fingerprint);
            assert_eq!(binding.checkout_id, checkout_id);
            assert!(binding.repository_remote_identity.is_none());
        };
        assert_folder_binding();
        assert_eq!(
            identity_kind_for_establishment(folder.path()).expect("folder establishment kind"),
            CheckoutIdentityKind::Folder
        );

        fs::write(folder.path().join("first.rs"), "fn first() {}\n").expect("first file");
        assert_folder_binding();

        // A harness may initialize Git after setup. Ordinary validation keeps
        // honoring the recorded folder identity, while a later explicit,
        // ownership-checked establishment can opt into Git identity.
        create_git_checkout(folder.path());
        assert_folder_binding();
        assert_eq!(
            identity_kind_for_establishment(folder.path()).expect("Git establishment kind"),
            CheckoutIdentityKind::Git
        );

        fs::write(
            folder.path().join(".contextstream/folder-id"),
            format!("{}\n", RepositoryFingerprint::generate()),
        )
        .expect("replace marker");
        assert!(matches!(
            validate_checkout_binding(folder.path(), Some(workspace_id), project_id),
            Err(CheckoutIdentityError::RepositoryFingerprintMismatch { .. })
        ));
    }

    #[test]
    fn legacy_checkout_id_is_stable_but_distinct_per_worktree_root() {
        let first = tempdir().expect("first checkout");
        let second = tempdir().expect("second checkout");
        let fingerprint =
            RepositoryFingerprint::parse("git-common-dir-v1:11111111-1111-4111-8111-111111111111")
                .expect("fingerprint");

        let first_id = CheckoutId::for_legacy_binding(first.path(), &fingerprint);
        assert_eq!(
            first_id,
            CheckoutId::for_legacy_binding(first.path(), &fingerprint)
        );
        assert_ne!(
            first_id,
            CheckoutId::for_legacy_binding(second.path(), &fingerprint)
        );
        assert_eq!(
            CheckoutId::parse(first_id.as_str()).expect("round trip"),
            first_id
        );
    }

    #[test]
    fn legacy_binding_without_checkout_id_validates_and_invalid_id_fails_closed() {
        let checkout = tempdir().expect("checkout");
        create_git_checkout(checkout.path());
        let fingerprint =
            ensure_repository_fingerprint(checkout.path()).expect("establish identity");
        let workspace_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        let canonical_root = fs::canonicalize(checkout.path()).expect("canonical root");
        let config_dir = checkout.path().join(".contextstream");
        fs::create_dir_all(&config_dir).expect("config directory");
        let config_path = config_dir.join("config.json");
        let mut config = serde_json::json!({
            "workspace_id": workspace_id,
            "project_id": project_id,
            "checkout_root": canonical_root,
            "repository_fingerprint": fingerprint.as_str(),
        });
        fs::write(
            &config_path,
            serde_json::to_string_pretty(&config).expect("config"),
        )
        .expect("write legacy config");

        let binding = validate_checkout_binding(checkout.path(), Some(workspace_id), project_id)
            .expect("legacy binding");
        assert_eq!(
            binding.checkout_id,
            CheckoutId::for_legacy_binding(&binding.checkout_root, &fingerprint)
        );

        config["checkout_id"] = serde_json::json!("checkout-v1:not-a-uuid");
        fs::write(
            &config_path,
            serde_json::to_string_pretty(&config).expect("invalid config"),
        )
        .expect("write invalid config");
        assert!(matches!(
            validate_checkout_binding(checkout.path(), Some(workspace_id), project_id),
            Err(CheckoutIdentityError::InvalidCheckoutId { .. })
        ));
    }

    #[test]
    fn content_binding_rejects_duplicate_checkout_config_keys() {
        let checkout = tempdir().expect("checkout");
        create_git_checkout(checkout.path());
        let fingerprint =
            ensure_repository_fingerprint(checkout.path()).expect("establish identity");
        let workspace_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        let config_dir = checkout.path().join(".contextstream");
        fs::create_dir_all(&config_dir).expect("config directory");
        let canonical_root = fs::canonicalize(checkout.path()).expect("canonical checkout");
        fs::write(
            config_dir.join("config.json"),
            format!(
                concat!(
                    "{{",
                    "\"workspace_id\":\"{}\",",
                    "\"workspace_id\":\"{}\",",
                    "\"project_id\":\"{}\",",
                    "\"checkout_root\":{},",
                    "\"repository_fingerprint\":\"{}\"",
                    "}}"
                ),
                Uuid::new_v4(),
                workspace_id,
                project_id,
                serde_json::to_string(&canonical_root.to_string_lossy()).expect("root JSON"),
                fingerprint.as_str(),
            ),
        )
        .expect("duplicate-key config");

        assert!(matches!(
            validate_checkout_binding(checkout.path(), Some(workspace_id), project_id),
            Err(CheckoutIdentityError::InvalidLocalConfig(_))
        ));
    }

    #[test]
    fn fingerprint_is_stable_across_branch_and_head_changes() {
        let checkout = tempdir().expect("checkout");
        create_git_checkout(checkout.path());
        let before = ensure_repository_fingerprint(checkout.path()).expect("identity");

        fs::write(
            checkout.path().join(".git/HEAD"),
            "ref: refs/heads/feature\n",
        )
        .expect("change branch");
        fs::write(checkout.path().join(".git/ORIG_HEAD"), "deadbeef\n")
            .expect("change commit metadata");

        assert_eq!(
            current_repository_fingerprint(checkout.path()).expect("identity after changes"),
            before
        );
    }

    #[test]
    fn remote_identity_normalizes_host_transport_credentials_and_complete_namespace() {
        let checkout = tempdir().expect("checkout");
        create_git_checkout(checkout.path());
        write_origin(
            checkout.path(),
            "\"https://oauth2:top-secret@GitLab.Example.COM/Parent/Team/Repo.git/?token=hidden#fragment\"",
        );

        let https_identity = current_repository_remote_identity(checkout.path())
            .expect("https remote")
            .expect("remote identity");
        assert_eq!(
            https_identity.as_str(),
            "git-remote-v1:gitlab.example.com/Parent/Team/Repo"
        );
        assert_eq!(
            https_identity.canonical_https_url(),
            "https://gitlab.example.com/Parent/Team/Repo.git"
        );
        assert_eq!(
            current_repository_canonical_url(checkout.path())
                .expect("canonical repository URL")
                .as_deref(),
            Some("https://gitlab.example.com/Parent/Team/Repo.git")
        );
        assert!(!https_identity.as_str().contains("oauth2"));
        assert!(!https_identity.as_str().contains("top-secret"));
        assert!(!https_identity.as_str().contains("hidden"));

        write_origin(
            checkout.path(),
            "git@GITLAB.EXAMPLE.COM:Parent/Team/Repo.git",
        );
        assert_eq!(
            current_repository_remote_identity(checkout.path())
                .expect("scp remote")
                .expect("remote identity"),
            https_identity,
            "transport and user-info changes must not change repository identity"
        );
    }

    #[test]
    fn linked_worktree_reads_common_and_worktree_git_config() {
        let checkout = tempdir().expect("checkout");
        let storage = tempdir().expect("storage");
        let common = storage.path().join("main.git");
        let worktree_git_dir = common.join("worktrees/feature");
        fs::create_dir_all(&worktree_git_dir).expect("worktree metadata");
        fs::write(worktree_git_dir.join("commondir"), "../..\n").expect("commondir");
        fs::write(
            checkout.path().join(".git"),
            format!("gitdir: {}\n", worktree_git_dir.display()),
        )
        .expect("gitdir file");
        fs::write(
            common.join("config"),
            "[extensions]\n\tworktreeConfig = true\n",
        )
        .expect("common config");
        fs::write(
            worktree_git_dir.join("config.worktree"),
            "[remote \"origin\"]\n\turl = ssh://git@Code.Example.test:22/Org/Platform/Repo.git\n",
        )
        .expect("worktree config");

        assert_eq!(
            current_repository_remote_identity(checkout.path())
                .expect("worktree remote")
                .expect("remote identity")
                .as_str(),
            "git-remote-v1:code.example.test/Org/Platform/Repo"
        );
        assert_eq!(
            current_repository_canonical_url(checkout.path())
                .expect("worktree canonical URL")
                .as_deref(),
            Some("https://code.example.test/Org/Platform/Repo.git")
        );
    }

    #[test]
    fn linked_worktree_remote_overrides_the_common_remote() {
        let checkout = tempdir().expect("checkout");
        let storage = tempdir().expect("storage");
        let common = storage.path().join("main.git");
        let worktree_git_dir = common.join("worktrees/feature");
        fs::create_dir_all(&worktree_git_dir).expect("worktree metadata");
        fs::write(worktree_git_dir.join("commondir"), "../..\n").expect("commondir");
        fs::write(
            checkout.path().join(".git"),
            format!("gitdir: {}\n", worktree_git_dir.display()),
        )
        .expect("gitdir file");
        fs::write(
            common.join("config"),
            concat!(
                "[extensions]\n",
                "\tworktreeConfig = true\n",
                "[remote \"origin\"]\n",
                "\turl = https://code.example.test/Org/Shared.git\n",
            ),
        )
        .expect("common config");
        fs::write(
            worktree_git_dir.join("config.worktree"),
            "[remote \"origin\"]\n\turl = ssh://git@code.example.test/Org/Feature.git\n",
        )
        .expect("worktree config");

        assert_eq!(
            current_repository_remote_identity(checkout.path())
                .expect("worktree remote")
                .expect("remote identity")
                .as_str(),
            "git-remote-v1:code.example.test/Org/Feature"
        );
    }

    #[test]
    fn repository_without_remote_has_no_remote_identity() {
        let checkout = tempdir().expect("checkout");
        create_git_checkout(checkout.path());
        fs::write(
            checkout.path().join(".git/config"),
            "[core]\n\trepositoryformatversion = 0\n",
        )
        .expect("Git config");

        assert_eq!(
            current_repository_remote_identity(checkout.path()).expect("read Git config"),
            None
        );
    }

    #[test]
    fn linked_worktree_uses_canonical_git_common_directory() {
        let checkout = tempdir().expect("checkout");
        let storage = tempdir().expect("storage");
        let common = storage.path().join("main.git");
        let worktree_git_dir = common.join("worktrees/feature");
        fs::create_dir_all(&worktree_git_dir).expect("worktree metadata");
        fs::write(worktree_git_dir.join("commondir"), "../..\n").expect("commondir");
        fs::write(
            checkout.path().join(".git"),
            format!("gitdir: {}\n", worktree_git_dir.display()),
        )
        .expect("gitdir file");

        let fingerprint = ensure_repository_fingerprint(checkout.path()).expect("identity");
        assert_eq!(
            fs::read_to_string(common.join("contextstream/repository-id"))
                .expect("common marker")
                .trim(),
            fingerprint.as_str()
        );
        assert_eq!(
            current_repository_fingerprint(checkout.path()).expect("worktree identity"),
            fingerprint
        );
    }

    #[test]
    fn relative_gitdir_file_is_resolved_from_checkout_root() {
        let container = tempdir().expect("container");
        let checkout = container.path().join("checkout");
        let git_dir = container.path().join("metadata.git");
        fs::create_dir_all(&checkout).expect("checkout");
        fs::create_dir_all(&git_dir).expect("git metadata");
        fs::write(checkout.join(".git"), "gitdir: ../metadata.git\n").expect("gitdir file");

        let fingerprint = ensure_repository_fingerprint(&checkout).expect("identity");
        assert_eq!(
            fs::read_to_string(git_dir.join("contextstream/repository-id"))
                .expect("marker")
                .trim(),
            fingerprint.as_str()
        );
    }

    #[test]
    fn non_git_directories_fail_closed_without_creating_metadata() {
        let checkout = tempdir().expect("checkout");
        assert!(matches!(
            current_repository_fingerprint(checkout.path()),
            Err(CheckoutIdentityError::NotGitCheckout(_))
        ));
        assert!(matches!(
            ensure_repository_fingerprint(checkout.path()),
            Err(CheckoutIdentityError::NotGitCheckout(_))
        ));
        assert!(!checkout.path().join(".contextstream").exists());
    }

    #[test]
    fn oversized_gitdir_metadata_is_rejected() {
        let checkout = tempdir().expect("checkout");
        fs::write(
            checkout.path().join(".git"),
            vec![b'x'; GIT_METADATA_MAX_BYTES as usize + 1],
        )
        .expect("oversized git metadata");
        assert!(matches!(
            current_repository_fingerprint(checkout.path()),
            Err(CheckoutIdentityError::GitMetadataTooLarge(_))
        ));
    }

    #[test]
    fn oversized_git_config_is_rejected_without_unbounded_reading() {
        let checkout = tempdir().expect("checkout");
        create_git_checkout(checkout.path());
        fs::write(
            checkout.path().join(".git/config"),
            vec![b'x'; GIT_CONFIG_MAX_BYTES as usize + 1],
        )
        .expect("oversized Git config");

        assert!(matches!(
            current_repository_remote_identity(checkout.path()),
            Err(CheckoutIdentityError::GitConfigTooLarge(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_git_config_is_rejected() {
        use std::os::unix::fs::symlink;

        let checkout = tempdir().expect("checkout");
        let outside = tempdir().expect("outside");
        create_git_checkout(checkout.path());
        fs::write(
            outside.path().join("config"),
            "[remote \"origin\"]\n\turl = git@example.test:Org/Repo.git\n",
        )
        .expect("outside config");
        symlink(
            outside.path().join("config"),
            checkout.path().join(".git/config"),
        )
        .expect("config symlink");

        assert!(matches!(
            current_repository_remote_identity(checkout.path()),
            Err(CheckoutIdentityError::InvalidGitConfig { .. })
        ));
    }

    #[test]
    fn concurrent_explicit_establishment_converges_on_one_identity() {
        let checkout = tempdir().expect("checkout");
        create_git_checkout(checkout.path());
        let root = checkout.path().to_path_buf();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let threads = (0..8)
            .map(|_| {
                let root = root.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    ensure_repository_fingerprint(root).expect("concurrent identity")
                })
            })
            .collect::<Vec<_>>();
        let identities = threads
            .into_iter()
            .map(|thread| thread.join().expect("join"))
            .collect::<Vec<_>>();
        assert!(identities.iter().all(|value| value == &identities[0]));
    }

    #[test]
    fn explicit_establishment_waits_for_an_inflight_marker_write() {
        let checkout = tempdir().expect("checkout");
        create_git_checkout(checkout.path());
        let marker_dir = checkout.path().join(".git/contextstream");
        fs::create_dir_all(&marker_dir).expect("marker directory");
        let marker_path = marker_dir.join(MARKER_FILE);
        let marker_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker_path)
            .expect("empty in-flight marker");
        assert!(matches!(
            read_repository_marker(&marker_path),
            Err(CheckoutIdentityError::InvalidRepositoryFingerprint { .. })
        ));

        let expected = RepositoryFingerprint::generate();
        let expected_for_writer = expected.clone();
        let writer = std::thread::spawn(move || {
            let mut marker_file = marker_file;
            std::thread::sleep(Duration::from_millis(100));
            marker_file
                .write_all(format!("{expected_for_writer}\n").as_bytes())
                .and_then(|_| marker_file.sync_all())
                .expect("finish marker publication");
        });

        let observed =
            ensure_repository_fingerprint(checkout.path()).expect("wait for marker publication");
        writer.join().expect("writer");
        assert_eq!(observed, expected);
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_marker_directory_is_rejected() {
        use std::os::unix::fs::symlink;

        let checkout = tempdir().expect("checkout");
        let outside = tempdir().expect("outside");
        create_git_checkout(checkout.path());
        let outside_fingerprint = RepositoryFingerprint::generate();
        fs::write(
            outside.path().join(MARKER_FILE),
            format!("{outside_fingerprint}\n"),
        )
        .expect("outside marker");
        symlink(outside.path(), checkout.path().join(".git/contextstream"))
            .expect("marker directory symlink");
        assert!(matches!(
            current_repository_fingerprint(checkout.path()),
            Err(CheckoutIdentityError::InvalidGitMetadata { .. })
        ));
        assert!(matches!(
            ensure_repository_fingerprint(checkout.path()),
            Err(CheckoutIdentityError::InvalidGitMetadata { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_folder_marker_is_rejected() {
        use std::os::unix::fs::symlink;

        let folder = tempdir().expect("folder");
        let outside = tempdir().expect("outside");
        fs::create_dir_all(folder.path().join(".contextstream")).expect("marker directory");
        fs::write(folder.path().join(".contextstream/config.json"), "{}\n").expect("local config");
        fs::write(
            outside.path().join(FOLDER_MARKER_FILE),
            format!("{}\n", RepositoryFingerprint::generate()),
        )
        .expect("outside marker");
        symlink(
            outside.path().join(FOLDER_MARKER_FILE),
            folder.path().join(".contextstream/folder-id"),
        )
        .expect("folder marker symlink");

        assert!(matches!(
            current_checkout_fingerprint(folder.path(), CheckoutIdentityKind::Folder),
            Err(CheckoutIdentityError::InvalidLocalConfig(_))
        ));
        assert!(matches!(
            ensure_folder_fingerprint(folder.path()),
            Err(CheckoutIdentityError::InvalidLocalConfig(_))
        ));
    }
}
