//! Secure, privacy-bounded local evidence for coding-harness readiness.
//!
//! This state is deliberately separate from `installation.json`, whose
//! configured-client selection is authoritative for installer scoping. The
//! readiness ledger is diagnostic evidence only: it never grants permission
//! to edit an editor and never stores prompts, paths, tool arguments, model
//! output, usernames, hostnames, or other free-form machine/user data.

use chrono::{DateTime, Utc};
use fs2::FileExt;
use mcp_types::{
    HarnessId, HarnessReadinessEvidence, HarnessReadinessStage, ReadinessEvidenceSource,
    ReadinessEvidenceStatus, TeachingLoadEvidence, HARNESS_READINESS_SCHEMA_VERSION,
    HARNESS_TEACHING_VERSION,
};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs::{File, Metadata};
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

pub const HARNESS_READINESS_LEDGER_SCHEMA_VERSION: u16 = 1;
pub const HARNESS_READINESS_FILE_NAME: &str = "harness-readiness.json";
const MAX_LEDGER_BYTES: u64 = 1024 * 1024;
const MAX_HARNESSES: usize = 20;
const MAX_EVIDENCE: usize = 512;
const MAX_EVIDENCE_PER_HARNESS: usize = 48;
const MAX_VERSION_BYTES: usize = 64;
const MAX_RULES_HASH_BYTES: usize = 128;

/// Bounded local snapshot. There is at most one entry per
/// `(harness_id, stage, source)` tuple.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessReadinessLedger {
    pub schema_version: u16,
    pub installation_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub evidence: Vec<HarnessReadinessEvidence>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceWriteOutcome {
    Created,
    Updated,
    Unchanged,
}

/// Current versions used to interpret stored evidence. Version drift is
/// presented as `stale`; the historical record itself remains unchanged.
#[derive(Debug, Clone, Copy)]
pub struct CurrentHarnessVersions<'a> {
    pub teaching_version: &'a str,
    pub managed_config_version: Option<&'a str>,
    pub rules_hash: Option<&'a str>,
}

impl<'a> CurrentHarnessVersions<'a> {
    pub const fn new(teaching_version: &'a str) -> Self {
        Self {
            teaching_version,
            managed_config_version: None,
            rules_hash: None,
        }
    }
}

pub fn harness_readiness_path() -> PathBuf {
    dirs::home_dir()
        .map(|home| {
            home.join(".contextstream")
                .join(HARNESS_READINESS_FILE_NAME)
        })
        .unwrap_or_else(|| PathBuf::from(".contextstream").join(HARNESS_READINESS_FILE_NAME))
}

fn state_lock_path(path: &Path) -> PathBuf {
    path.with_extension("lock")
}

#[cfg(windows)]
fn metadata_is_unsafe_file(metadata: &Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

    metadata.file_type().is_symlink()
        || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || !metadata.is_file()
}

#[cfg(not(windows))]
fn metadata_is_unsafe_file(metadata: &Metadata) -> bool {
    metadata.file_type().is_symlink() || !metadata.is_file()
}

fn open_readonly_nofollow(path: &Path) -> std::io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    options.open(path)
}

fn read_regular_bytes(path: &Path) -> std::io::Result<Option<Vec<u8>>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata_is_unsafe_file(&metadata) => {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                format!(
                    "Refusing non-regular harness readiness state at {}",
                    path.display()
                ),
            ));
        }
        Ok(metadata) if metadata.len() > MAX_LEDGER_BYTES => {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                format!(
                    "Refusing oversized harness readiness state at {}",
                    path.display()
                ),
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    }

    let mut file = match open_readonly_nofollow(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if metadata_is_unsafe_file(&metadata) || metadata.len() > MAX_LEDGER_BYTES {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!(
                "Refusing non-regular or oversized harness readiness state at {}",
                path.display()
            ),
        ));
    }

    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    Read::by_ref(&mut file)
        .take(MAX_LEDGER_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_LEDGER_BYTES {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!(
                "Refusing oversized harness readiness state at {}",
                path.display()
            ),
        ));
    }
    Ok(Some(bytes))
}

fn parse_ledger(path: &Path, bytes: &[u8]) -> std::io::Result<HarnessReadinessLedger> {
    let text = std::str::from_utf8(bytes).map_err(|error| {
        std::io::Error::new(
            ErrorKind::InvalidData,
            format!(
                "Refusing malformed harness readiness state at {}: {}",
                path.display(),
                error
            ),
        )
    })?;
    let value = crate::json::parse_value_without_duplicate_keys(text).map_err(|error| {
        std::io::Error::new(
            ErrorKind::InvalidData,
            format!(
                "Refusing malformed harness readiness state at {}: {}",
                path.display(),
                error
            ),
        )
    })?;
    let ledger: HarnessReadinessLedger = serde_json::from_value(value).map_err(|error| {
        std::io::Error::new(
            ErrorKind::InvalidData,
            format!(
                "Refusing malformed harness readiness state at {}: {}",
                path.display(),
                error
            ),
        )
    })?;
    validate_ledger(&ledger, path)?;
    Ok(ledger)
}

fn read_ledger_at(path: &Path) -> std::io::Result<Option<(Vec<u8>, HarnessReadinessLedger)>> {
    let Some(bytes) = read_regular_bytes(path)? else {
        return Ok(None);
    };
    let ledger = parse_ledger(path, &bytes)?;
    Ok(Some((bytes, ledger)))
}

fn bounded_version(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_VERSION_BYTES
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_' | '+')
        })
}

fn bounded_rules_hash(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_RULES_HASH_BYTES
        && value.chars().all(|character| character.is_ascii_hexdigit())
}

fn validate_optional_version(value: Option<&str>, field: &str, path: &Path) -> std::io::Result<()> {
    if value.is_some_and(|value| !bounded_version(value)) {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!(
                "Refusing invalid {} in harness readiness state at {}",
                field,
                path.display()
            ),
        ));
    }
    Ok(())
}

fn validate_stage_source(evidence: &HarnessReadinessEvidence, path: &Path) -> std::io::Result<()> {
    let valid = match evidence.stage {
        HarnessReadinessStage::Configured => {
            evidence.source == ReadinessEvidenceSource::ManagedMcpConfig
        }
        HarnessReadinessStage::Taught => evidence.source == ReadinessEvidenceSource::ManagedRules,
        HarnessReadinessStage::Loaded => {
            evidence.source == ReadinessEvidenceSource::InstructionsLoadedHook
        }
        HarnessReadinessStage::Connected => {
            evidence.source == ReadinessEvidenceSource::McpProtocolRequest
        }
        HarnessReadinessStage::Grounded => matches!(
            evidence.source,
            ReadinessEvidenceSource::InitTool | ReadinessEvidenceSource::ContextTool
        ),
        HarnessReadinessStage::Practicing => matches!(
            evidence.source,
            ReadinessEvidenceSource::ComplianceCheck | ReadinessEvidenceSource::RuntimeBehavior
        ),
    };
    if !valid {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!(
                "Refusing mismatched readiness stage/source at {}",
                path.display()
            ),
        ));
    }

    if evidence.source == ReadinessEvidenceSource::InstructionsLoadedHook
        && (evidence.harness_id.profile().teaching_load_evidence
            != TeachingLoadEvidence::DirectHook
            || evidence.status != ReadinessEvidenceStatus::Verified)
    {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!(
                "Refusing unsupported direct instruction-load evidence at {}",
                path.display()
            ),
        ));
    }
    if evidence.source == ReadinessEvidenceSource::RuntimeBehavior
        && evidence.status == ReadinessEvidenceStatus::Verified
    {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!(
                "Runtime behavior may be inferred but not marked verified at {}",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn validate_evidence(evidence: &HarnessReadinessEvidence, path: &Path) -> std::io::Result<()> {
    if evidence.schema_version != HARNESS_READINESS_SCHEMA_VERSION {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!(
                "Unsupported readiness evidence schema {} at {}",
                evidence.schema_version,
                path.display()
            ),
        ));
    }
    validate_stage_source(evidence, path)?;
    validate_optional_version(
        evidence.teaching_version.as_deref(),
        "teaching version",
        path,
    )?;
    validate_optional_version(
        evidence.managed_config_version.as_deref(),
        "managed config version",
        path,
    )?;
    if evidence
        .rules_hash
        .as_deref()
        .is_some_and(|value| !bounded_rules_hash(value))
    {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!(
                "Refusing invalid rules hash in harness readiness state at {}",
                path.display()
            ),
        ));
    }

    if evidence.status.is_ready() {
        match evidence.stage {
            HarnessReadinessStage::Configured if evidence.managed_config_version.is_none() => {
                return Err(std::io::Error::new(
                    ErrorKind::InvalidData,
                    "Ready configured evidence requires a managed config version",
                ));
            }
            HarnessReadinessStage::Taught
                if evidence.teaching_version.is_none() || evidence.rules_hash.is_none() =>
            {
                return Err(std::io::Error::new(
                    ErrorKind::InvalidData,
                    "Ready taught evidence requires teaching version and rules hash",
                ));
            }
            HarnessReadinessStage::Loaded if evidence.teaching_version.is_none() => {
                return Err(std::io::Error::new(
                    ErrorKind::InvalidData,
                    "Ready loaded evidence requires a teaching version",
                ));
            }
            HarnessReadinessStage::Grounded | HarnessReadinessStage::Practicing
                if evidence.teaching_version.is_none() =>
            {
                return Err(std::io::Error::new(
                    ErrorKind::InvalidData,
                    "Ready behavioral evidence requires a teaching version",
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_ledger(ledger: &HarnessReadinessLedger, path: &Path) -> std::io::Result<()> {
    if ledger.schema_version != HARNESS_READINESS_LEDGER_SCHEMA_VERSION {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!(
                "Unsupported harness readiness ledger schema {} at {}",
                ledger.schema_version,
                path.display()
            ),
        ));
    }
    if ledger.updated_at < ledger.created_at {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!(
                "Harness readiness timestamps are out of order at {}",
                path.display()
            ),
        ));
    }
    if ledger.evidence.len() > MAX_EVIDENCE {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!("Too many harness readiness records at {}", path.display()),
        ));
    }

    let mut harnesses = HashSet::new();
    let mut counts = HashMap::new();
    let mut keys = HashSet::new();
    for evidence in &ledger.evidence {
        validate_evidence(evidence, path)?;
        harnesses.insert(evidence.harness_id);
        *counts.entry(evidence.harness_id).or_insert(0usize) += 1;
        if counts[&evidence.harness_id] > MAX_EVIDENCE_PER_HARNESS {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                format!(
                    "Too many readiness records for {} at {}",
                    evidence.harness_id.as_str(),
                    path.display()
                ),
            ));
        }
        if !keys.insert((evidence.harness_id, evidence.stage, evidence.source)) {
            return Err(std::io::Error::new(
                ErrorKind::InvalidData,
                format!(
                    "Duplicate harness readiness evidence key at {}",
                    path.display()
                ),
            ));
        }
    }
    if harnesses.len() > MAX_HARNESSES {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!(
                "Too many harnesses in readiness state at {}",
                path.display()
            ),
        ));
    }
    Ok(())
}

fn ensure_state_parent(path: &Path) -> std::io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        std::io::Error::new(
            ErrorKind::InvalidInput,
            format!("Harness readiness path has no parent: {}", path.display()),
        )
    })?;
    let existed = parent.exists();
    std::fs::create_dir_all(parent)?;
    #[cfg(unix)]
    if !existed {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn lock_state(path: &Path) -> std::io::Result<File> {
    ensure_state_parent(path)?;
    let lock_path = state_lock_path(path);
    match std::fs::symlink_metadata(&lock_path) {
        Ok(metadata) if metadata_is_unsafe_file(&metadata) => {
            return Err(std::io::Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "Refusing non-regular harness readiness lock {}",
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
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let lock = options.open(&lock_path)?;
    if metadata_is_unsafe_file(&lock.metadata()?) {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "Refusing non-regular harness readiness lock {}",
                lock_path.display()
            ),
        ));
    }
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

fn write_ledger_at(
    path: &Path,
    ledger: &HarnessReadinessLedger,
    expected_original: Option<&[u8]>,
) -> std::io::Result<()> {
    validate_ledger(ledger, path)?;
    ensure_state_parent(path)?;
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata_is_unsafe_file(&metadata) => {
            return Err(std::io::Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "Refusing non-regular harness readiness state {}",
                    path.display()
                ),
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let mut bytes = serde_json::to_vec_pretty(ledger).map_err(std::io::Error::other)?;
    bytes.push(b'\n');
    if bytes.len() as u64 > MAX_LEDGER_BYTES {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "Harness readiness state exceeds its size limit",
        ));
    }

    let temporary = path.with_extension(format!("json.tmp.{}", Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(&temporary)?;
    let write_result = (|| -> std::io::Result<()> {
        file.write_all(&bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        file.sync_all()?;
        Ok(())
    })();
    drop(file);
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temporary);
        return Err(error);
    }

    let current = read_regular_bytes(path)?;
    if current.as_deref() != expected_original {
        let _ = std::fs::remove_file(&temporary);
        return Err(std::io::Error::new(
            ErrorKind::WouldBlock,
            format!(
                "{} changed while its readiness update was being prepared",
                path.display()
            ),
        ));
    }
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

#[cfg(unix)]
fn tighten_owner_only_permissions(path: &Path) -> std::io::Result<bool> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let metadata = std::fs::symlink_metadata(path)?;
    if metadata_is_unsafe_file(&metadata) {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            format!(
                "Refusing non-regular harness readiness state {}",
                path.display()
            ),
        ));
    }
    if metadata.permissions().mode() & 0o777 == 0o600 {
        return Ok(false);
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(true)
}

#[cfg(not(unix))]
fn tighten_owner_only_permissions(_path: &Path) -> std::io::Result<bool> {
    Ok(false)
}

fn source_rank(source: ReadinessEvidenceSource) -> u8 {
    match source {
        ReadinessEvidenceSource::ManagedMcpConfig => 0,
        ReadinessEvidenceSource::ManagedRules => 1,
        ReadinessEvidenceSource::InstructionsLoadedHook => 2,
        ReadinessEvidenceSource::McpProtocolRequest => 3,
        ReadinessEvidenceSource::InitTool => 4,
        ReadinessEvidenceSource::ContextTool => 5,
        ReadinessEvidenceSource::ComplianceCheck => 6,
        ReadinessEvidenceSource::RuntimeBehavior => 7,
    }
}

fn sort_evidence(evidence: &mut [HarnessReadinessEvidence]) {
    evidence.sort_by_key(|item| (item.harness_id, item.stage.rank(), source_rank(item.source)));
}

fn same_observation(left: &HarnessReadinessEvidence, right: &HarnessReadinessEvidence) -> bool {
    left.schema_version == right.schema_version
        && left.harness_id == right.harness_id
        && left.stage == right.stage
        && left.status == right.status
        && left.source == right.source
        && left.teaching_version == right.teaching_version
        && left.managed_config_version == right.managed_config_version
        && left.rules_hash == right.rules_hash
}

fn record_evidence_at(
    path: &Path,
    installation_id: Uuid,
    mut evidence: HarnessReadinessEvidence,
    now: DateTime<Utc>,
) -> std::io::Result<EvidenceWriteOutcome> {
    evidence.schema_version = HARNESS_READINESS_SCHEMA_VERSION;
    evidence.observed_at = now;
    validate_evidence(&evidence, path)?;

    let _lock = lock_state(path)?;
    let existing = read_ledger_at(path)?;
    let (original, mut ledger, created) = match existing {
        Some((bytes, ledger)) => {
            if ledger.installation_id != installation_id {
                return Err(std::io::Error::new(
                    ErrorKind::InvalidData,
                    format!(
                        "Harness readiness installation id does not match {}",
                        path.display()
                    ),
                ));
            }
            (Some(bytes), ledger, false)
        }
        None => (
            None,
            HarnessReadinessLedger {
                schema_version: HARNESS_READINESS_LEDGER_SCHEMA_VERSION,
                installation_id,
                created_at: now,
                updated_at: now,
                evidence: Vec::new(),
            },
            true,
        ),
    };

    if let Some(current) = ledger.evidence.iter_mut().find(|current| {
        current.harness_id == evidence.harness_id
            && current.stage == evidence.stage
            && current.source == evidence.source
    }) {
        if same_observation(current, &evidence) {
            let permissions_changed = original.is_some() && tighten_owner_only_permissions(path)?;
            return Ok(if permissions_changed {
                EvidenceWriteOutcome::Updated
            } else {
                EvidenceWriteOutcome::Unchanged
            });
        }
        evidence.observed_at = evidence.observed_at.max(current.observed_at);
        *current = evidence;
    } else {
        ledger.evidence.push(evidence);
    }

    ledger.updated_at = now.max(ledger.updated_at).max(ledger.created_at);
    sort_evidence(&mut ledger.evidence);
    write_ledger_at(path, &ledger, original.as_deref())?;
    Ok(if created {
        EvidenceWriteOutcome::Created
    } else {
        EvidenceWriteOutcome::Updated
    })
}

fn remove_harnesses_at(
    path: &Path,
    installation_id: Uuid,
    harnesses: &[HarnessId],
    now: DateTime<Utc>,
) -> std::io::Result<EvidenceWriteOutcome> {
    if harnesses.is_empty() {
        return Ok(EvidenceWriteOutcome::Unchanged);
    }
    let remove: HashSet<HarnessId> = harnesses.iter().copied().collect();
    let _lock = lock_state(path)?;
    let Some((original, mut ledger)) = read_ledger_at(path)? else {
        return Ok(EvidenceWriteOutcome::Unchanged);
    };
    if ledger.installation_id != installation_id {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            format!(
                "Harness readiness installation id does not match {}",
                path.display()
            ),
        ));
    }
    let before = ledger.evidence.len();
    ledger
        .evidence
        .retain(|evidence| !remove.contains(&evidence.harness_id));
    if ledger.evidence.len() == before {
        return Ok(EvidenceWriteOutcome::Unchanged);
    }

    // Keep an empty owner-only ledger and its stable lock. Deleting a lock file
    // while another process is waiting on its inode can split future writers
    // across two locks. Empty evidence reports no readiness and is safe to
    // retain alongside the intentionally retained installation identity.
    ledger.updated_at = now.max(ledger.updated_at).max(ledger.created_at);
    write_ledger_at(path, &ledger, Some(&original))?;
    Ok(EvidenceWriteOutcome::Updated)
}

pub fn read_harness_readiness() -> std::io::Result<Option<HarnessReadinessLedger>> {
    let path = harness_readiness_path();
    let Some((_, ledger)) = read_ledger_at(&path)? else {
        return Ok(None);
    };
    let installation_id = crate::activation::existing_installation_id()?.ok_or_else(|| {
        std::io::Error::new(
            ErrorKind::InvalidData,
            "Harness readiness state exists without installation state",
        )
    })?;
    if ledger.installation_id != installation_id {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "Harness readiness state belongs to a different installation id",
        ));
    }
    Ok(Some(ledger))
}

pub fn record_configured(
    harness_id: HarnessId,
    managed_config_version: &str,
    teaching_version: &str,
) -> std::io::Result<EvidenceWriteOutcome> {
    let installation_id = crate::activation::ensure_installation_id()?;
    let mut evidence = HarnessReadinessEvidence::new(
        harness_id,
        HarnessReadinessStage::Configured,
        ReadinessEvidenceStatus::Verified,
        ReadinessEvidenceSource::ManagedMcpConfig,
        Utc::now(),
    );
    evidence.managed_config_version = Some(managed_config_version.to_string());
    evidence.teaching_version = Some(teaching_version.to_string());
    record_evidence_at(
        &harness_readiness_path(),
        installation_id,
        evidence,
        Utc::now(),
    )
}

pub fn record_taught(
    harness_id: HarnessId,
    teaching_version: &str,
    rules_hash: &str,
) -> std::io::Result<EvidenceWriteOutcome> {
    let installation_id = crate::activation::ensure_installation_id()?;
    let mut evidence = HarnessReadinessEvidence::new(
        harness_id,
        HarnessReadinessStage::Taught,
        ReadinessEvidenceStatus::Verified,
        ReadinessEvidenceSource::ManagedRules,
        Utc::now(),
    );
    evidence.teaching_version = Some(teaching_version.to_string());
    evidence.rules_hash = Some(rules_hash.to_string());
    record_evidence_at(
        &harness_readiness_path(),
        installation_id,
        evidence,
        Utc::now(),
    )
}

fn existing_installation_for_runtime() -> std::io::Result<Option<Uuid>> {
    crate::activation::existing_installation_id()
}

fn runtime_identity(claimed_harness: HarnessId) -> std::io::Result<Option<(Uuid, HarnessId)>> {
    let Some(raw_installation_id) = std::env::var("CONTEXTSTREAM_INSTALLATION_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
    else {
        return Ok(None);
    };
    let installation_id = Uuid::parse_str(raw_installation_id.trim()).map_err(|_| {
        std::io::Error::new(
            ErrorKind::InvalidData,
            "Managed runtime installation id is not a UUID",
        )
    })?;
    if existing_installation_for_runtime()? != Some(installation_id) {
        return Ok(None);
    }
    let Some(configured_harness) = std::env::var("CONTEXTSTREAM_CLIENT")
        .ok()
        .and_then(|value| HarnessId::from_alias(&value))
    else {
        return Ok(None);
    };
    if claimed_harness != configured_harness {
        return Ok(None);
    }
    Ok(Some((installation_id, configured_harness)))
}

fn managed_hook_identity(claimed_harness: HarnessId) -> std::io::Result<Option<(Uuid, HarnessId)>> {
    let Some(installation_id) = existing_installation_for_runtime()? else {
        return Ok(None);
    };
    let configured = crate::activation::configured_clients()?;
    if !configured
        .iter()
        .filter_map(|client| HarnessId::from_alias(client))
        .any(|configured_harness| configured_harness == claimed_harness)
    {
        return Ok(None);
    }
    Ok(Some((installation_id, claimed_harness)))
}

pub fn record_runtime_connected(
    claimed_harness: HarnessId,
) -> std::io::Result<Option<EvidenceWriteOutcome>> {
    let Some((installation_id, harness_id)) = runtime_identity(claimed_harness)? else {
        return Ok(None);
    };
    let mut evidence = HarnessReadinessEvidence::new(
        harness_id,
        HarnessReadinessStage::Connected,
        ReadinessEvidenceStatus::Verified,
        ReadinessEvidenceSource::McpProtocolRequest,
        Utc::now(),
    );
    evidence.teaching_version = std::env::var("CONTEXTSTREAM_TEACHING_VERSION")
        .ok()
        .filter(|value| bounded_version(value));
    evidence.managed_config_version = std::env::var("CONTEXTSTREAM_MANAGED_CONFIG_VERSION")
        .ok()
        .filter(|value| bounded_version(value));
    record_evidence_at(
        &harness_readiness_path(),
        installation_id,
        evidence,
        Utc::now(),
    )
    .map(Some)
}

pub fn record_runtime_grounded(
    claimed_harness: HarnessId,
    source: ReadinessEvidenceSource,
) -> std::io::Result<Option<EvidenceWriteOutcome>> {
    if !matches!(
        source,
        ReadinessEvidenceSource::InitTool | ReadinessEvidenceSource::ContextTool
    ) {
        return Err(std::io::Error::new(
            ErrorKind::InvalidInput,
            "Grounded evidence requires init or context source",
        ));
    }
    let Some((installation_id, harness_id)) = runtime_identity(claimed_harness)? else {
        return Ok(None);
    };
    let mut evidence = HarnessReadinessEvidence::new(
        harness_id,
        HarnessReadinessStage::Grounded,
        ReadinessEvidenceStatus::Verified,
        source,
        Utc::now(),
    );
    evidence.teaching_version = Some(HARNESS_TEACHING_VERSION.to_string());
    record_evidence_at(
        &harness_readiness_path(),
        installation_id,
        evidence,
        Utc::now(),
    )
    .map(Some)
}

pub fn record_direct_instruction_load(
    harness_id: HarnessId,
    rules_hash: &str,
) -> std::io::Result<Option<EvidenceWriteOutcome>> {
    let Some((installation_id, harness_id)) = managed_hook_identity(harness_id)? else {
        return Ok(None);
    };
    let mut evidence = HarnessReadinessEvidence::new(
        harness_id,
        HarnessReadinessStage::Loaded,
        ReadinessEvidenceStatus::Verified,
        ReadinessEvidenceSource::InstructionsLoadedHook,
        Utc::now(),
    );
    evidence.teaching_version = Some(HARNESS_TEACHING_VERSION.to_string());
    evidence.rules_hash = Some(rules_hash.to_string());
    record_evidence_at(
        &harness_readiness_path(),
        installation_id,
        evidence,
        Utc::now(),
    )
    .map(Some)
}

pub fn record_deterministic_practice(
    harness_id: HarnessId,
) -> std::io::Result<Option<EvidenceWriteOutcome>> {
    let Some((installation_id, harness_id)) = managed_hook_identity(harness_id)? else {
        return Ok(None);
    };
    let mut evidence = HarnessReadinessEvidence::new(
        harness_id,
        HarnessReadinessStage::Practicing,
        ReadinessEvidenceStatus::Verified,
        ReadinessEvidenceSource::ComplianceCheck,
        Utc::now(),
    );
    evidence.teaching_version = Some(HARNESS_TEACHING_VERSION.to_string());
    record_evidence_at(
        &harness_readiness_path(),
        installation_id,
        evidence,
        Utc::now(),
    )
    .map(Some)
}

/// Record privacy-bounded evidence that a managed harness successfully used
/// ContextStream's search surface. This is intentionally `inferred`, not
/// `verified`: a successful search proves real runtime use without claiming
/// that an editor hook directly observed the next local tool call.
pub fn record_inferred_runtime_practice(
    claimed_harness: HarnessId,
) -> std::io::Result<Option<EvidenceWriteOutcome>> {
    let Some((installation_id, harness_id)) = runtime_identity(claimed_harness)? else {
        return Ok(None);
    };
    let mut evidence = HarnessReadinessEvidence::new(
        harness_id,
        HarnessReadinessStage::Practicing,
        ReadinessEvidenceStatus::Inferred,
        ReadinessEvidenceSource::RuntimeBehavior,
        Utc::now(),
    );
    evidence.teaching_version = Some(HARNESS_TEACHING_VERSION.to_string());
    record_evidence_at(
        &harness_readiness_path(),
        installation_id,
        evidence,
        Utc::now(),
    )
    .map(Some)
}

pub fn remove_harnesses(harnesses: &[HarnessId]) -> std::io::Result<EvidenceWriteOutcome> {
    let Some(installation_id) = crate::activation::existing_installation_id()? else {
        return Ok(EvidenceWriteOutcome::Unchanged);
    };
    remove_harnesses_at(
        &harness_readiness_path(),
        installation_id,
        harnesses,
        Utc::now(),
    )
}

pub fn has_evidence_for(ledger: &HarnessReadinessLedger, harnesses: &[HarnessId]) -> bool {
    let harnesses: HashSet<HarnessId> = harnesses.iter().copied().collect();
    ledger
        .evidence
        .iter()
        .any(|evidence| harnesses.contains(&evidence.harness_id))
}

fn evidence_is_stale(
    evidence: &HarnessReadinessEvidence,
    current: CurrentHarnessVersions<'_>,
) -> bool {
    if !evidence.status.is_ready() {
        return false;
    }

    let teaching_stale = match evidence.stage {
        HarnessReadinessStage::Configured | HarnessReadinessStage::Connected => evidence
            .teaching_version
            .as_deref()
            .is_some_and(|version| version != current.teaching_version),
        HarnessReadinessStage::Taught
        | HarnessReadinessStage::Loaded
        | HarnessReadinessStage::Grounded
        | HarnessReadinessStage::Practicing => {
            evidence.teaching_version.as_deref() != Some(current.teaching_version)
        }
    };
    let config_stale =
        current
            .managed_config_version
            .is_some_and(|expected| match evidence.stage {
                HarnessReadinessStage::Configured => {
                    evidence.managed_config_version.as_deref() != Some(expected)
                }
                HarnessReadinessStage::Connected => evidence
                    .managed_config_version
                    .as_deref()
                    .is_some_and(|version| version != expected),
                _ => false,
            });
    let rules_stale = current.rules_hash.is_some_and(|expected| {
        matches!(
            evidence.stage,
            HarnessReadinessStage::Taught | HarnessReadinessStage::Loaded
        ) && evidence.rules_hash.as_deref() != Some(expected)
    });
    teaching_stale || config_stale || rules_stale
}

pub fn effective_evidence_for(
    ledger: &HarnessReadinessLedger,
    harness_id: HarnessId,
    current: CurrentHarnessVersions<'_>,
) -> Vec<HarnessReadinessEvidence> {
    effective_evidence_records_for(&ledger.evidence, harness_id, current)
}

/// Apply current-version semantics to an arbitrary readiness projection.
///
/// The remote status API returns the same privacy-bounded evidence fields as
/// the local ledger but not a local [`HarnessReadinessLedger`] wrapper. Keeping
/// drift interpretation here ensures doctor and the runtime cannot disagree
/// about whether old teaching/config/rules evidence is still current.
pub fn effective_evidence_records_for(
    evidence: &[HarnessReadinessEvidence],
    harness_id: HarnessId,
    current: CurrentHarnessVersions<'_>,
) -> Vec<HarnessReadinessEvidence> {
    evidence
        .iter()
        .filter(|evidence| evidence.harness_id == harness_id)
        .cloned()
        .map(|mut evidence| {
            if evidence_is_stale(&evidence, current) {
                evidence.status = ReadinessEvidenceStatus::Stale;
            }
            evidence
        })
        .collect()
}

pub fn highest_effective_stage(
    evidence: &[HarnessReadinessEvidence],
) -> Option<HarnessReadinessStage> {
    evidence
        .iter()
        .filter(|evidence| evidence.status.is_ready())
        .map(|evidence| evidence.stage)
        .max_by_key(|stage| stage.rank())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    fn configured(harness_id: HarnessId, version: &str) -> HarnessReadinessEvidence {
        let mut evidence = HarnessReadinessEvidence::new(
            harness_id,
            HarnessReadinessStage::Configured,
            ReadinessEvidenceStatus::Verified,
            ReadinessEvidenceSource::ManagedMcpConfig,
            Utc::now(),
        );
        evidence.managed_config_version = Some(version.to_string());
        evidence.teaching_version = Some(HARNESS_TEACHING_VERSION.to_string());
        evidence
    }

    #[test]
    fn identical_observation_is_byte_identical_and_timestamp_stable() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join(HARNESS_READINESS_FILE_NAME);
        let installation_id = Uuid::new_v4();
        let first_time = DateTime::parse_from_rfc3339("2026-07-28T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let later = first_time + chrono::Duration::hours(1);

        assert_eq!(
            record_evidence_at(
                &path,
                installation_id,
                configured(HarnessId::Codex, "2"),
                first_time,
            )
            .unwrap(),
            EvidenceWriteOutcome::Created
        );
        let before = std::fs::read(&path).unwrap();
        assert_eq!(
            record_evidence_at(
                &path,
                installation_id,
                configured(HarnessId::Codex, "2"),
                later,
            )
            .unwrap(),
            EvidenceWriteOutcome::Unchanged
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn concurrent_updates_do_not_lose_harnesses() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = Arc::new(temp.path().join(HARNESS_READINESS_FILE_NAME));
        let installation_id = Uuid::new_v4();
        let harnesses = HarnessId::INSTALLABLE.to_vec();
        let barrier = Arc::new(Barrier::new(harnesses.len()));
        let mut workers = Vec::new();

        for harness_id in harnesses.iter().copied() {
            let path = Arc::clone(&path);
            let barrier = Arc::clone(&barrier);
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                record_evidence_at(
                    &path,
                    installation_id,
                    configured(harness_id, "2"),
                    Utc::now(),
                )
                .unwrap();
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }

        let (_, ledger) = read_ledger_at(&path).unwrap().unwrap();
        assert_eq!(ledger.evidence.len(), harnesses.len());
        for harness in harnesses {
            assert!(ledger
                .evidence
                .iter()
                .any(|evidence| evidence.harness_id == harness));
        }
    }

    #[test]
    fn changed_observation_never_moves_timestamps_backwards() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join(HARNESS_READINESS_FILE_NAME);
        let installation_id = Uuid::new_v4();
        let later = DateTime::parse_from_rfc3339("2026-07-28T02:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let earlier = later - chrono::Duration::hours(1);
        record_evidence_at(
            &path,
            installation_id,
            configured(HarnessId::Codex, "1"),
            later,
        )
        .unwrap();
        record_evidence_at(
            &path,
            installation_id,
            configured(HarnessId::Codex, "2"),
            earlier,
        )
        .unwrap();

        let (_, ledger) = read_ledger_at(&path).unwrap().unwrap();
        assert_eq!(ledger.updated_at, later);
        assert_eq!(ledger.evidence[0].observed_at, later);
        assert_eq!(
            ledger.evidence[0].managed_config_version.as_deref(),
            Some("2")
        );
    }

    #[test]
    fn malformed_and_duplicate_key_state_is_preserved() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join(HARNESS_READINESS_FILE_NAME);
        let malformed = br#"{"schema_version":1,"schema_version":1}"#;
        std::fs::write(&path, malformed).unwrap();

        assert!(record_evidence_at(
            &path,
            Uuid::new_v4(),
            configured(HarnessId::Codex, "2"),
            Utc::now(),
        )
        .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), malformed);
    }

    #[test]
    fn oversized_state_and_installation_mismatch_are_preserved() {
        let temp = tempfile::tempdir().expect("tempdir");
        let oversized_path = temp.path().join("oversized.json");
        let oversized = vec![b' '; MAX_LEDGER_BYTES as usize + 1];
        std::fs::write(&oversized_path, &oversized).unwrap();
        assert!(record_evidence_at(
            &oversized_path,
            Uuid::new_v4(),
            configured(HarnessId::Codex, "2"),
            Utc::now(),
        )
        .is_err());
        assert_eq!(std::fs::read(&oversized_path).unwrap(), oversized);

        let mismatch_path = temp.path().join("mismatch.json");
        let original_installation = Uuid::new_v4();
        record_evidence_at(
            &mismatch_path,
            original_installation,
            configured(HarnessId::Codex, "2"),
            Utc::now(),
        )
        .unwrap();
        let before = std::fs::read(&mismatch_path).unwrap();
        assert!(record_evidence_at(
            &mismatch_path,
            Uuid::new_v4(),
            configured(HarnessId::ClaudeCode, "2"),
            Utc::now(),
        )
        .is_err());
        assert_eq!(std::fs::read(&mismatch_path).unwrap(), before);
    }

    #[test]
    fn count_bounds_and_reversed_ledger_timestamps_fail_closed() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join(HARNESS_READINESS_FILE_NAME);
        let installation_id = Uuid::new_v4();
        let created_at = Utc::now();
        let mut ledger = HarnessReadinessLedger {
            schema_version: HARNESS_READINESS_LEDGER_SCHEMA_VERSION,
            installation_id,
            created_at,
            updated_at: created_at,
            evidence: vec![configured(HarnessId::Codex, "2"); MAX_EVIDENCE + 1],
        };
        let over_limit = serde_json::to_vec(&ledger).unwrap();
        std::fs::write(&path, &over_limit).unwrap();
        assert!(record_evidence_at(
            &path,
            installation_id,
            configured(HarnessId::ClaudeCode, "2"),
            Utc::now(),
        )
        .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), over_limit);

        ledger.evidence.clear();
        ledger.updated_at = created_at - chrono::Duration::seconds(1);
        let reversed = serde_json::to_vec(&ledger).unwrap();
        std::fs::write(&path, &reversed).unwrap();
        assert!(record_evidence_at(
            &path,
            installation_id,
            configured(HarnessId::ClaudeCode, "2"),
            Utc::now(),
        )
        .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), reversed);
    }

    #[cfg(unix)]
    #[test]
    fn state_and_lock_are_owner_only_and_symlinks_are_refused() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("real").join(HARNESS_READINESS_FILE_NAME);
        let installation_id = Uuid::new_v4();
        record_evidence_at(
            &path,
            installation_id,
            configured(HarnessId::Codex, "2"),
            Utc::now(),
        )
        .unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(state_lock_path(&path))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        let linked_path = temp.path().join("linked").join(HARNESS_READINESS_FILE_NAME);
        std::fs::create_dir_all(linked_path.parent().unwrap()).unwrap();
        symlink(&path, &linked_path).unwrap();
        assert!(record_evidence_at(
            &linked_path,
            installation_id,
            configured(HarnessId::ClaudeCode, "2"),
            Utc::now(),
        )
        .is_err());

        let lock_target = temp.path().join("lock-target");
        std::fs::write(&lock_target, b"user").unwrap();
        let lock_state_path = temp
            .path()
            .join("lock-link")
            .join(HARNESS_READINESS_FILE_NAME);
        std::fs::create_dir_all(lock_state_path.parent().unwrap()).unwrap();
        symlink(&lock_target, state_lock_path(&lock_state_path)).unwrap();
        assert!(record_evidence_at(
            &lock_state_path,
            installation_id,
            configured(HarnessId::ClaudeCode, "2"),
            Utc::now(),
        )
        .is_err());
        assert_eq!(std::fs::read(&lock_target).unwrap(), b"user");
    }

    #[cfg(unix)]
    #[test]
    fn identical_observation_tightens_existing_permissions_without_rewriting_bytes() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join(HARNESS_READINESS_FILE_NAME);
        let installation_id = Uuid::new_v4();
        let observed_at = Utc::now();
        record_evidence_at(
            &path,
            installation_id,
            configured(HarnessId::Codex, "2"),
            observed_at,
        )
        .unwrap();
        let before = std::fs::read(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert_eq!(
            record_evidence_at(
                &path,
                installation_id,
                configured(HarnessId::Codex, "2"),
                observed_at + chrono::Duration::seconds(1),
            )
            .unwrap(),
            EvidenceWriteOutcome::Updated
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn invalid_stage_sources_and_false_direct_loads_are_rejected() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join(HARNESS_READINESS_FILE_NAME);
        let installation_id = Uuid::new_v4();
        let mut wrong_source = configured(HarnessId::Codex, "2");
        wrong_source.source = ReadinessEvidenceSource::RuntimeBehavior;
        assert!(record_evidence_at(&path, installation_id, wrong_source, Utc::now()).is_err());

        let mut false_direct = HarnessReadinessEvidence::new(
            HarnessId::Codex,
            HarnessReadinessStage::Loaded,
            ReadinessEvidenceStatus::Verified,
            ReadinessEvidenceSource::InstructionsLoadedHook,
            Utc::now(),
        );
        false_direct.teaching_version = Some(HARNESS_TEACHING_VERSION.to_string());
        false_direct.rules_hash = Some("0123456789abcdef".to_string());
        assert!(record_evidence_at(&path, installation_id, false_direct, Utc::now()).is_err());

        let mut false_inferred_load = HarnessReadinessEvidence::new(
            HarnessId::Codex,
            HarnessReadinessStage::Loaded,
            ReadinessEvidenceStatus::Inferred,
            ReadinessEvidenceSource::RuntimeBehavior,
            Utc::now(),
        );
        false_inferred_load.teaching_version = Some(HARNESS_TEACHING_VERSION.to_string());
        assert!(
            record_evidence_at(&path, installation_id, false_inferred_load, Utc::now()).is_err(),
            "runtime behavior cannot claim that teaching entered the harness context"
        );
        assert!(!path.exists());
    }

    #[test]
    fn version_drift_demotes_effective_readiness_without_rewriting_history() {
        let now = Utc::now();
        let installation_id = Uuid::new_v4();
        let mut taught = HarnessReadinessEvidence::new(
            HarnessId::Codex,
            HarnessReadinessStage::Taught,
            ReadinessEvidenceStatus::Verified,
            ReadinessEvidenceSource::ManagedRules,
            now,
        );
        taught.teaching_version = Some("harness_teaching_v1".to_string());
        taught.rules_hash = Some("0123456789abcdef".to_string());
        let ledger = HarnessReadinessLedger {
            schema_version: HARNESS_READINESS_LEDGER_SCHEMA_VERSION,
            installation_id,
            created_at: now,
            updated_at: now,
            evidence: vec![taught.clone()],
        };

        let effective = effective_evidence_for(
            &ledger,
            HarnessId::Codex,
            CurrentHarnessVersions {
                teaching_version: HARNESS_TEACHING_VERSION,
                managed_config_version: Some("2"),
                rules_hash: Some("fedcba9876543210"),
            },
        );
        assert_eq!(effective[0].status, ReadinessEvidenceStatus::Stale);
        assert_eq!(ledger.evidence[0].status, ReadinessEvidenceStatus::Verified);
        assert_eq!(highest_effective_stage(&effective), None);
    }

    #[test]
    fn uninstall_removes_target_evidence_but_retains_stable_empty_ledger() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join(HARNESS_READINESS_FILE_NAME);
        let installation_id = Uuid::new_v4();
        record_evidence_at(
            &path,
            installation_id,
            configured(HarnessId::Codex, "2"),
            Utc::now(),
        )
        .unwrap();
        remove_harnesses_at(&path, installation_id, &[HarnessId::Codex], Utc::now()).unwrap();

        let (_, ledger) = read_ledger_at(&path).unwrap().unwrap();
        assert_eq!(ledger.installation_id, installation_id);
        assert!(ledger.evidence.is_empty());
        assert!(state_lock_path(&path).is_file());
    }

    #[test]
    fn runtime_without_managed_identity_creates_no_files() {
        let _guard = crate::test_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        let previous_id = std::env::var_os("CONTEXTSTREAM_INSTALLATION_ID");
        let previous_client = std::env::var_os("CONTEXTSTREAM_CLIENT");
        std::env::set_var("HOME", temp.path());
        std::env::remove_var("CONTEXTSTREAM_INSTALLATION_ID");
        std::env::remove_var("CONTEXTSTREAM_CLIENT");

        assert_eq!(record_runtime_connected(HarnessId::Codex).unwrap(), None);
        assert!(!temp.path().join(".contextstream").exists());

        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        if let Some(value) = previous_id {
            std::env::set_var("CONTEXTSTREAM_INSTALLATION_ID", value);
        } else {
            std::env::remove_var("CONTEXTSTREAM_INSTALLATION_ID");
        }
        if let Some(value) = previous_client {
            std::env::set_var("CONTEXTSTREAM_CLIENT", value);
        } else {
            std::env::remove_var("CONTEXTSTREAM_CLIENT");
        }
    }

    #[test]
    fn managed_runtime_identity_is_exact_and_search_practice_is_only_inferred() {
        let _guard = crate::test_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        let previous_id = std::env::var_os("CONTEXTSTREAM_INSTALLATION_ID");
        let previous_client = std::env::var_os("CONTEXTSTREAM_CLIENT");
        std::env::set_var("HOME", temp.path());
        let installation_id = crate::activation::ensure_installation_id().expect("installation");
        std::env::set_var("CONTEXTSTREAM_INSTALLATION_ID", installation_id.to_string());
        std::env::set_var("CONTEXTSTREAM_CLIENT", HarnessId::Codex.as_str());

        assert_eq!(
            record_runtime_connected(HarnessId::ClaudeCode).unwrap(),
            None,
            "a conflicting initialized harness must not write connection evidence"
        );
        assert_eq!(
            record_direct_instruction_load(HarnessId::ClaudeCode, "0123456789abcdef").unwrap(),
            None,
            "a conflicting harness claim must not write direct-load evidence"
        );
        assert_eq!(
            record_deterministic_practice(HarnessId::ClaudeCode).unwrap(),
            None,
            "a conflicting hook identity must not write practice evidence"
        );
        assert!(record_inferred_runtime_practice(HarnessId::Codex)
            .unwrap()
            .is_some());

        let ledger = read_harness_readiness()
            .expect("readiness read")
            .expect("readiness ledger");
        assert_eq!(ledger.evidence.len(), 1);
        let evidence = &ledger.evidence[0];
        assert_eq!(evidence.harness_id, HarnessId::Codex);
        assert_eq!(evidence.stage, HarnessReadinessStage::Practicing);
        assert_eq!(evidence.status, ReadinessEvidenceStatus::Inferred);
        assert_eq!(evidence.source, ReadinessEvidenceSource::RuntimeBehavior);

        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        if let Some(value) = previous_id {
            std::env::set_var("CONTEXTSTREAM_INSTALLATION_ID", value);
        } else {
            std::env::remove_var("CONTEXTSTREAM_INSTALLATION_ID");
        }
        if let Some(value) = previous_client {
            std::env::set_var("CONTEXTSTREAM_CLIENT", value);
        } else {
            std::env::remove_var("CONTEXTSTREAM_CLIENT");
        }
    }

    #[test]
    fn managed_hook_evidence_uses_persisted_selection_without_mcp_process_env() {
        let _guard = crate::test_env_lock()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        let previous_id = std::env::var_os("CONTEXTSTREAM_INSTALLATION_ID");
        let previous_client = std::env::var_os("CONTEXTSTREAM_CLIENT");
        std::env::set_var("HOME", temp.path());
        std::env::remove_var("CONTEXTSTREAM_INSTALLATION_ID");
        std::env::remove_var("CONTEXTSTREAM_CLIENT");
        crate::activation::replace_configured_clients(&["claude".to_string()])
            .expect("persist configured client");

        assert!(
            record_direct_instruction_load(HarnessId::ClaudeCode, "0123456789abcdef")
                .unwrap()
                .is_some()
        );
        assert!(record_deterministic_practice(HarnessId::ClaudeCode)
            .unwrap()
            .is_some());
        assert_eq!(
            record_deterministic_practice(HarnessId::Codex).unwrap(),
            None,
            "a stale hook from an unselected editor must not produce evidence"
        );

        let ledger = read_harness_readiness()
            .expect("readiness read")
            .expect("readiness ledger");
        assert_eq!(ledger.evidence.len(), 2);
        assert!(ledger
            .evidence
            .iter()
            .all(|evidence| evidence.harness_id == HarnessId::ClaudeCode));

        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
        if let Some(value) = previous_id {
            std::env::set_var("CONTEXTSTREAM_INSTALLATION_ID", value);
        } else {
            std::env::remove_var("CONTEXTSTREAM_INSTALLATION_ID");
        }
        if let Some(value) = previous_client {
            std::env::set_var("CONTEXTSTREAM_CLIENT", value);
        } else {
            std::env::remove_var("CONTEXTSTREAM_CLIENT");
        }
    }
}
