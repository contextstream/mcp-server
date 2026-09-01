//! Non-destructive edits to user-owned editor config files.
//!
//! Every file this installer touches (`~/.claude/settings.json`,
//! `~/.config/Code/User/settings.json`, `~/.cursor/hooks.json`, ...) belongs to
//! the user, not to us, and is frequently hand-maintained. Three rules follow:
//!
//! 1. **Never rewrite a file we could not read.** A parse failure is an error,
//!    not a reason to fall back to an empty document.
//! 2. **Never strip formatting the user is entitled to.** Where JSONC is
//!    legitimate (VS Code settings, `*.jsonc`), edits are applied as surgical
//!    text splices so comments, key order, and whitespace survive byte-for-byte.
//! 3. **Never write when nothing changed.** An identical rewrite still bumps
//!    mtime and wakes file watchers.

use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::{
    collections::HashSet,
    fs::{File, OpenOptions},
    io::{ErrorKind, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use super::mcp_config::try_parse_json_like;

/// Whether comments and trailing commas are sanctioned by the host tool.
///
/// This no longer changes whether we *accept* a file — surgical edits preserve
/// comments either way — only whether their presence is worth warning about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonDialect {
    /// The host tool parses this file as strict JSON (Claude `settings.json`,
    /// `.mcp.json`, Cursor configs). Comments here are likely a latent bug in
    /// the user's own config, so we preserve them but say so.
    Strict,
    /// JSONC is legitimate (VS Code `settings.json`, `kilo.jsonc`).
    Jsonc,
}

/// A config file loaded for modification.
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    /// Original file contents (empty when the file did not exist).
    pub raw: String,
    /// Parsed view, used for decisions. Always an object.
    pub value: Value,
    /// Whether the file existed on disk.
    pub existed: bool,
    /// True when the file only parsed after comment/trailing-comma tolerance
    /// and the host tool expects strict JSON.
    pub nonstandard_syntax: bool,
    /// Canonical target used for existing symlinked dotfiles. Keeping this
    /// snapshot prevents a symlink swap between read and commit.
    resolved_path: PathBuf,
}

impl LoadedConfig {
    /// Borrow the parsed root object.
    pub fn object(&self) -> &serde_json::Map<String, Value> {
        static EMPTY: std::sync::OnceLock<serde_json::Map<String, Value>> =
            std::sync::OnceLock::new();
        self.value
            .as_object()
            .unwrap_or_else(|| EMPTY.get_or_init(serde_json::Map::new))
    }
}

/// Resolve an existing final-component symlink without changing the user's
/// link. Atomic replacement is then performed on the target rather than
/// replacing the symlink entry itself. A dangling or unreadable link is an
/// error, never a missing config.
fn resolve_edit_path(path: &Path) -> Result<PathBuf> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => std::fs::canonicalize(path)
            .with_context(|| format!("Could not resolve symlinked config {}", path.display())),
        Ok(_) => Ok(path.to_path_buf()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(path.to_path_buf()),
        Err(error) => {
            Err(error).with_context(|| format!("Could not inspect config path {}", path.display()))
        }
    }
}

fn refuse_symlink_deletion(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => bail!(
            "Refusing to delete symlinked file {}; remove the ContextStream content from its target manually.",
            path.display()
        ),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("Could not inspect {} before deletion", path.display()))
        }
    }
}

/// Read a config file for modification, refusing anything we cannot parse.
///
/// A missing or whitespace-only file yields an empty object so first-install
/// callers need no special case. A file we cannot parse at all is an error —
/// we never fall back to an empty document and rewrite over the user's data.
pub fn read_for_edit(path: &Path, dialect: JsonDialect) -> Result<LoadedConfig> {
    if path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(BACKUP_SUFFIX))
    {
        return Ok(
            read_recovery_for_edit(path, dialect)?.unwrap_or_else(|| LoadedConfig {
                raw: String::new(),
                value: Value::Object(serde_json::Map::new()),
                existed: false,
                nonstandard_syntax: false,
                resolved_path: path.to_path_buf(),
            }),
        );
    }
    let resolved_path = resolve_edit_path(path)?;
    if !resolved_path.try_exists().with_context(|| {
        format!(
            "Could not determine whether {} exists",
            resolved_path.display()
        )
    })? {
        return Ok(LoadedConfig {
            raw: String::new(),
            value: Value::Object(serde_json::Map::new()),
            existed: false,
            nonstandard_syntax: false,
            resolved_path,
        });
    }

    let raw = std::fs::read_to_string(&resolved_path)
        .with_context(|| format!("Could not read {}", resolved_path.display()))?;

    parse_loaded_config(path, raw, true, dialect, resolved_path)
}

fn parse_loaded_config(
    display_path: &Path,
    raw: String,
    existed: bool,
    dialect: JsonDialect,
    resolved_path: PathBuf,
) -> Result<LoadedConfig> {
    if raw.trim().is_empty() {
        return Ok(LoadedConfig {
            raw,
            value: Value::Object(serde_json::Map::new()),
            existed,
            nonstandard_syntax: false,
            resolved_path,
        });
    }

    let had_bom = raw.starts_with('\u{feff}');
    let parse_input = raw.strip_prefix('\u{feff}').unwrap_or(&raw);
    // serde_json's Value deserializer accepts duplicate object keys and keeps
    // only the last value. That is unsafe for an editor: rewriting a managed
    // parent object would silently discard an earlier duplicate. The JSON-like
    // parser performs the same strict/JSONC parsing while rejecting duplicates
    // recursively at every object depth.
    let strict_syntax = serde_json::from_str::<Value>(parse_input);
    let nonstandard_syntax = (strict_syntax.is_err() || had_bom) && dialect == JsonDialect::Strict;
    let value = try_parse_json_like(parse_input).map_err(|parse_error| {
        anyhow::anyhow!(
            "Refusing to modify {}: it is not valid JSON or contains duplicate object keys ({}). \
             Fix or move the file, then re-run.",
            display_path.display(),
            parse_error
        )
    })?;

    if !value.is_object() {
        bail!(
            "Refusing to modify {}: expected a JSON object at the top level.",
            display_path.display()
        );
    }

    Ok(LoadedConfig {
        raw,
        value,
        existed,
        nonstandard_syntax,
        resolved_path,
    })
}

/// Apply the caller's mutations to `loaded` and write the result surgically.
///
/// Only top-level keys whose values actually changed are rewritten, so
/// comments, key order, and formatting elsewhere survive byte-for-byte.
/// Returns `true` when the file was written.
pub fn commit(path: &Path, loaded: &LoadedConfig, updated: &Value) -> Result<bool> {
    commit_with_removals(path, loaded, updated, &[])
}

/// Commit a secret-bearing JSON document with owner-only permissions already
/// applied to the staged file before its atomic rename. Secret-bearing state
/// deliberately gets no recovery sidecar: retaining the previous secret is a
/// larger risk than losing formatting after a failed rotation.
pub(crate) fn commit_private(
    path: &Path,
    loaded: &LoadedConfig,
    updated: &Value,
    removed_keys: &[&str],
) -> Result<bool> {
    let content = render_with_removals(loaded, updated, removed_keys)?;
    write_if_changed_from_snapshot_with_options(path, &content, loaded, true, false)
}

/// Commit a complete parsed document while explicitly authorizing a bounded
/// set of top-level key removals. Any other absent key is treated as a partial
/// object bug and refused.
pub fn commit_with_removals(
    path: &Path,
    loaded: &LoadedConfig,
    updated: &Value,
    removed_keys: &[&str],
) -> Result<bool> {
    let content = render_with_removals(loaded, updated, removed_keys)?;
    write_if_changed_from_snapshot(path, &content, loaded)
}

/// Render a complete updated document using the same surgical splice logic as
/// [`commit`], without touching disk.
pub fn render(loaded: &LoadedConfig, updated: &Value) -> Result<String> {
    render_with_removals(loaded, updated, &[])
}

fn render_with_removals(
    loaded: &LoadedConfig,
    updated: &Value,
    removed_keys: &[&str],
) -> Result<String> {
    if loaded.raw.trim().is_empty() {
        to_pretty(updated)
    } else {
        apply_changed_top_level_keys_with_removals(
            &loaded.raw,
            &loaded.value,
            updated,
            removed_keys,
        )
    }
}

// ============================================================================
// Dry run
// ============================================================================

/// What a write would do to a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeAction {
    /// The file does not exist and would be created.
    Create,
    /// The file exists and its contents would change.
    Modify,
    /// The file exists and the new contents are identical.
    Unchanged,
    /// The file exists and would be deleted.
    Delete,
}

impl ChangeAction {
    /// Short label for operator output.
    pub fn label(self) -> &'static str {
        match self {
            ChangeAction::Create => "create",
            ChangeAction::Modify => "modify",
            ChangeAction::Unchanged => "unchanged",
            ChangeAction::Delete => "delete",
        }
    }
}

/// One file the installer would touch.
#[derive(Debug, Clone)]
pub struct PlannedChange {
    pub path: std::path::PathBuf,
    pub action: ChangeAction,
    /// Privacy-safe size summary, empty for unchanged files.
    ///
    /// Editor configuration frequently contains credentials and arbitrary
    /// user-owned secrets. A dry run must describe the mutation without
    /// copying either the old or new file content into terminal/CI logs.
    pub summary: String,
}

#[cfg(not(test))]
static DRY_RUN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(not(test))]
fn planned_changes() -> &'static std::sync::Mutex<Vec<PlannedChange>> {
    static PLANNED: std::sync::OnceLock<std::sync::Mutex<Vec<PlannedChange>>> =
        std::sync::OnceLock::new();
    PLANNED.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

#[cfg(test)]
thread_local! {
    // Unit tests run in parallel in one process. Keeping their dry-run state
    // local to the test thread prevents one test from suppressing another
    // test's real filesystem assertions.
    static TEST_DRY_RUN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static TEST_PLANNED: std::cell::RefCell<Vec<PlannedChange>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Enable or disable dry-run mode for this process.
///
/// Production CLI invocations use process-wide state because one command may
/// cross async worker threads. Unit tests use thread-local state so parallel
/// tests cannot contaminate one another.
pub fn set_dry_run(enabled: bool) {
    #[cfg(not(test))]
    {
        DRY_RUN.store(enabled, std::sync::atomic::Ordering::SeqCst);
        mcp_client::activation::set_installation_state_persistence_enabled(!enabled);
        if enabled {
            planned_changes()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
        }
    }

    #[cfg(test)]
    {
        TEST_DRY_RUN.set(enabled);
        if enabled {
            TEST_PLANNED.with(|planned| planned.borrow_mut().clear());
        }
    }
}

/// Whether dry-run mode is active.
pub fn is_dry_run() -> bool {
    #[cfg(not(test))]
    {
        DRY_RUN.load(std::sync::atomic::Ordering::SeqCst)
    }

    #[cfg(test)]
    {
        TEST_DRY_RUN.get()
    }
}

fn record_planned(path: &Path, action: ChangeAction, summary: String) {
    #[cfg(not(test))]
    {
        let mut guard = planned_changes().lock().unwrap_or_else(|e| e.into_inner());
        record_planned_in(&mut guard, path, action, summary);
    }

    #[cfg(test)]
    {
        TEST_PLANNED.with(|planned| {
            record_planned_in(&mut planned.borrow_mut(), path, action, summary);
        });
    }
}

fn record_planned_in(
    planned: &mut Vec<PlannedChange>,
    path: &Path,
    action: ChangeAction,
    summary: String,
) {
    // Preserve every operation. A dry run deliberately leaves the real file
    // untouched, so a later operation on the same path may have been derived
    // from the same on-disk snapshot. Replacing the earlier entry would hide
    // a real planned mutation (for example, Codex MCP config plus trust).
    planned.push(PlannedChange {
        path: path.to_path_buf(),
        action,
        summary,
    });
}

/// Drain and return everything a dry run planned to do.
pub fn take_planned_changes() -> Vec<PlannedChange> {
    #[cfg(not(test))]
    {
        let mut guard = planned_changes().lock().unwrap_or_else(|e| e.into_inner());
        std::mem::take(&mut *guard)
    }

    #[cfg(test)]
    {
        TEST_PLANNED.with(|planned| std::mem::take(&mut *planned.borrow_mut()))
    }
}

/// Record a non-text mutation implemented outside this module, such as
/// installing the managed executable. Callers must invoke this instead of
/// touching disk when dry-run mode is active.
pub fn record_external_change(path: &Path, action: ChangeAction) {
    record_planned(path, action, String::new());
}

/// Delete a file, honouring dry-run mode.
pub fn remove_file_if_present(path: &Path) -> Result<bool> {
    refuse_symlink_deletion(path)?;
    let existing = match std::fs::read(path) {
        Ok(content) => content,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Could not read {} before deleting it", path.display()))
        }
    };
    remove_file_if_unchanged_bytes(path, &existing)
}

/// Delete a user-owned file only if it still matches the caller's snapshot.
/// A first recovery backup is retained.
pub fn remove_file_if_unchanged(path: &Path, expected: &str) -> Result<bool> {
    remove_file_if_unchanged_bytes(path, expected.as_bytes())
}

fn remove_file_if_unchanged_bytes(path: &Path, expected: &[u8]) -> Result<bool> {
    refuse_symlink_deletion(path)?;
    let existing = match std::fs::read(path) {
        Ok(content) => content,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Could not read {} before deleting it", path.display()))
        }
    };
    if existing != expected {
        bail!(
            "Refusing to delete {} because it changed after ContextStream read it. Re-run the command.",
            path.display()
        );
    }
    if is_dry_run() {
        record_planned(path, ChangeAction::Delete, String::new());
        return Ok(true);
    }

    let backup_created = create_backup_if_absent(path, expected)?;

    let current = match std::fs::read(path)
        .with_context(|| format!("Could not re-read {} before deleting it", path.display()))
    {
        Ok(current) => current,
        Err(error) => {
            discard_new_backup_if_unchanged(path, expected, backup_created);
            return Err(error);
        }
    };
    if current != expected {
        discard_new_backup_if_unchanged(path, expected, backup_created);
        bail!(
            "Refusing to delete {} because it changed while ContextStream was preparing the edit. Re-run the command.",
            path.display()
        );
    }

    std::fs::remove_file(path)?;
    sync_parent_directory(path);
    Ok(true)
}

/// Delete a file known to be wholly generated by ContextStream.
///
/// Unlike [`remove_file_if_present`], this deliberately does not create a
/// recovery backup: callers must supply the exact content they classified as
/// owned, and the function refuses any mismatch or concurrent change.
pub(crate) fn remove_owned_file_if_unchanged(path: &Path, expected: &str) -> Result<bool> {
    refuse_symlink_deletion(path)?;
    let existing = match read_existing_text(path)? {
        Some(existing) => existing,
        None => return Ok(false),
    };
    if existing != expected {
        bail!(
            "Refusing to delete {} because it no longer matches the generated content.",
            path.display()
        );
    }
    if is_dry_run() {
        record_planned(path, ChangeAction::Delete, String::new());
        return Ok(true);
    }

    let current = read_existing_text(path)?;
    if current.as_deref() != Some(expected) {
        bail!(
            "Refusing to delete {} because it changed while ContextStream was preparing the edit. Re-run the command.",
            path.display()
        );
    }
    std::fs::remove_file(path)?;
    sync_parent_directory(path);
    Ok(true)
}

/// Describe a text mutation without returning any file content.
///
/// Config files can contain ContextStream credentials, third-party tokens, or
/// arbitrary user secrets. Even a local dry run is routinely copied into
/// issue reports and CI logs, so content previews are never safe by default.
/// Counting directly over `str` also keeps this bounded for giant minified
/// one-line files; the previous line-diff implementation could emit the whole
/// file and allocate a quadratic LCS matrix.
pub fn privacy_safe_change_summary(before: &str, after: &str) -> String {
    fn line_count(content: &str) -> usize {
        if content.is_empty() {
            0
        } else {
            content.lines().count()
        }
    }

    format!(
        "  (content preview withheld; {} line(s), {} byte(s) -> {} line(s), {} byte(s))\n",
        line_count(before),
        before.len(),
        line_count(after),
        after.len()
    )
}

/// Suffix used for the pre-write backup copy.
pub const BACKUP_SUFFIX: &str = ".contextstream.bak";

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_sibling_path(path: &Path, purpose: &str) -> Result<PathBuf> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("Invalid config path: {}", path.display()))?;
    let file_name = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("Invalid config path: {}", path.display()))?
        .to_string_lossy();
    let counter = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    Ok(parent.join(format!(
        ".{}.contextstream.{}.{}.{}",
        file_name,
        purpose,
        std::process::id(),
        counter
    )))
}

fn create_unique_temp(path: &Path) -> Result<(PathBuf, File)> {
    for _ in 0..100 {
        let candidate = unique_sibling_path(path, "tmp")?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // A staged editor config can contain credentials. Keep it private
            // from creation; widen to the destination's mode only after all
            // bytes have been written.
            options.mode(0o600);
        }
        match options.open(&candidate) {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("Could not create temporary file beside {}", path.display())
                })
            }
        }
    }
    bail!(
        "Could not allocate a unique temporary file beside {}",
        path.display()
    )
}

pub(crate) fn backup_path(path: &Path) -> Result<PathBuf> {
    let resolved = resolve_edit_path(path)?;
    let file_name = resolved
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("Invalid config path: {}", path.display()))?
        .to_string_lossy();
    Ok(resolved.with_file_name(format!("{}{}", file_name, BACKUP_SUFFIX)))
}

fn validate_recovery_file(path: &Path) -> Result<Option<std::fs::Metadata>> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => bail!(
            "Refusing to trust recovery backup {} because it is a symlink.",
            path.display()
        ),
        Ok(metadata) if !metadata.is_file() => bail!(
            "Refusing to trust recovery backup {} because it is not a regular file.",
            path.display()
        ),
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error)
            .with_context(|| format!("Could not inspect recovery backup {}", path.display())),
    }
}

/// Read a recovery sidecar only when it is a regular file, never a symlink or
/// directory. Callers use this instead of following a path an unrelated
/// process could have planted beside a user-owned config.
pub(crate) fn read_recovery_file(path: &Path) -> Result<Option<String>> {
    let Some(before) = validate_recovery_file(path)? else {
        return Ok(None);
    };
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Could not read recovery backup {}", path.display()))?;
    let Some(after) = validate_recovery_file(path)? else {
        bail!(
            "Recovery backup {} disappeared while ContextStream was reading it.",
            path.display()
        );
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != after.dev() || before.ino() != after.ino() {
            bail!(
                "Recovery backup {} changed while ContextStream was reading it.",
                path.display()
            );
        }
    }
    #[cfg(not(unix))]
    let _ = (before, after);
    Ok(Some(content))
}

/// Read and parse a JSON/JSONC recovery sidecar without ever following a
/// final-component symlink. A present but malformed recovery snapshot is an
/// error: callers must not silently discard recovery state and proceed with a
/// destructive fallback.
pub(crate) fn read_recovery_for_edit(
    path: &Path,
    dialect: JsonDialect,
) -> Result<Option<LoadedConfig>> {
    let Some(raw) = read_recovery_file(path)? else {
        return Ok(None);
    };
    parse_loaded_config(path, raw, true, dialect, path.to_path_buf()).map(Some)
}

/// Restore the first backup only after a caller has proven that the current
/// file is exactly the output ContextStream would generate from that backup.
///
/// The backup is deleted after a successful restore, so a clean
/// install/uninstall cycle leaves no sidecar behind. A concurrent change to
/// either file aborts or preserves the backup.
pub(crate) fn restore_first_backup(
    path: &Path,
    loaded: &LoadedConfig,
    backup_content: &str,
) -> Result<bool> {
    let resolved_path = resolve_edit_path(path)?;
    if resolved_path != loaded.resolved_path {
        bail!(
            "Refusing to restore {} because its symlink target changed after ContextStream read it. Re-run the command.",
            path.display()
        );
    }
    restore_text_first_backup(&resolved_path, &loaded.raw, loaded.existed, backup_content)
}

/// Text-file variant of [`restore_first_backup`].
pub(crate) fn restore_text_first_backup(
    path: &Path,
    current_content: &str,
    current_existed: bool,
    backup_content: &str,
) -> Result<bool> {
    let backup = backup_path(path)?;
    let actual_backup = read_recovery_file(&backup)?.ok_or_else(|| {
        anyhow::anyhow!(
            "Refusing to restore {} because recovery backup {} is missing.",
            path.display(),
            backup.display()
        )
    })?;
    if actual_backup != backup_content {
        bail!(
            "Refusing to restore {} because recovery backup {} changed after it was validated.",
            path.display(),
            backup.display()
        );
    }
    let backup_permissions = validate_recovery_file(&backup)?
        .expect("recovery backup was just read")
        .permissions();
    let changed = write_if_changed_impl_with_permissions(
        path,
        backup_content,
        Some(current_existed.then_some(current_content)),
        true,
        false,
        Some(backup_permissions),
    )?;
    if !changed {
        return Ok(false);
    }

    if is_dry_run() {
        record_planned(&backup, ChangeAction::Delete, String::new());
        return Ok(true);
    }

    let current_backup = read_recovery_file(&backup)?;
    if current_backup.as_deref() != Some(backup_content) {
        bail!(
            "Restored {}, but kept {} because the recovery backup changed concurrently.",
            path.display(),
            backup.display()
        );
    }
    std::fs::remove_file(&backup)
        .with_context(|| format!("Could not remove restored backup {}", backup.display()))?;
    sync_parent_directory(&backup);
    Ok(true)
}

/// Keep the first pre-ContextStream snapshot. Later refreshes must never
/// overwrite the only byte-for-byte recovery copy with an intermediate state.
fn create_backup_if_absent(path: &Path, content: &[u8]) -> Result<bool> {
    let backup = backup_path(path)?;
    if validate_recovery_file(&backup)?.is_some() {
        File::open(&backup).with_context(|| {
            format!(
                "Refusing to modify {} because recovery backup {} is unreadable",
                path.display(),
                backup.display()
            )
        })?;
        return Ok(false);
    }

    // Prepare the complete backup under a private, unique name, then publish
    // it with an atomic no-replace hard link. Writing directly to the final
    // `.bak` path could leave a truncated-but-trusted recovery file if the
    // process crashed halfway through the copy.
    let (temporary, mut file) = create_unique_temp(&backup)?;
    let result = (|| -> Result<()> {
        file.write_all(content)?;
        if let Ok(metadata) = std::fs::metadata(path) {
            file.set_permissions(metadata.permissions())?;
        }
        file.sync_all()?;
        Ok(())
    })();
    drop(file);
    if let Err(error) = result {
        let _ = std::fs::remove_file(&temporary);
        return Err(error)
            .with_context(|| format!("Could not back up {} before writing", path.display()));
    }

    match std::fs::hard_link(&temporary, &backup) {
        Ok(()) => {
            let _ = std::fs::remove_file(&temporary);
            sync_parent_directory(&backup);
            Ok(true)
        }
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {
            let _ = std::fs::remove_file(&temporary);
            let Some(_) = validate_recovery_file(&backup)? else {
                bail!(
                    "Recovery backup {} disappeared while ContextStream was validating it.",
                    backup.display()
                );
            };
            File::open(&backup).with_context(|| {
                format!(
                    "Refusing to modify {} because recovery backup {} is unreadable",
                    path.display(),
                    backup.display()
                )
            })?;
            Ok(false)
        }
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            Err(error).with_context(|| format!("Could not create backup {}", backup.display()))
        }
    }
}

fn discard_new_backup_if_unchanged(path: &Path, content: &[u8], was_created: bool) {
    if !was_created {
        return;
    }
    let Ok(backup) = backup_path(path) else {
        return;
    };
    if std::fs::read(&backup).ok().as_deref() == Some(content) {
        let _ = std::fs::remove_file(&backup);
        sync_parent_directory(&backup);
    }
}

fn read_existing_text(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("Could not read {} before writing", path.display()))
        }
    }
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) {
    if let Some(parent) = path.parent() {
        if let Ok(directory) = File::open(parent) {
            let _ = directory.sync_all();
        }
    }
}

#[cfg(not(unix))]
fn sync_parent_directory(_path: &Path) {}

#[cfg(not(windows))]
fn replace_with_temp(path: &Path, tmp: &Path) -> Result<()> {
    if let Err(error) = std::fs::rename(tmp, path) {
        let _ = std::fs::remove_file(tmp);
        return Err(error)
            .with_context(|| format!("Could not atomically replace {}", path.display()));
    }
    Ok(())
}

#[cfg(windows)]
fn replace_with_temp(path: &Path, tmp: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let temporary_wide: Vec<u16> = tmp.as_os_str().encode_wide().chain(Some(0)).collect();
    let path_wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let result = unsafe {
        MoveFileExW(
            temporary_wide.as_ptr(),
            path_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        let error = std::io::Error::last_os_error();
        let _ = std::fs::remove_file(tmp);
        Err(error).with_context(|| format!("Could not atomically replace {}", path.display()))
    } else {
        Ok(())
    }
}

/// Write `content` to `path` only if it differs from what is already there.
///
/// Returns `true` when the file was actually written. Backups are taken only
/// on a real change, so repeated no-op runs never clobber a good backup with
/// our own previous output.
pub fn write_if_changed(path: &Path, content: &str) -> Result<bool> {
    write_if_changed_impl(path, content, None, true)
}

/// Write a derived replacement only if the file still matches the exact text
/// the caller used to derive it. `None` means the caller observed no file.
pub fn write_if_unchanged(
    path: &Path,
    content: &str,
    expected_existing: Option<&str>,
) -> Result<bool> {
    write_if_changed_impl(path, content, Some(expected_existing), true)
}

/// Write a Git hook replacement with executable permissions already applied
/// to the staged file before its atomic rename.
pub(crate) fn write_executable_if_unchanged(
    path: &Path,
    content: &str,
    expected_existing: Option<&str>,
) -> Result<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        write_if_changed_impl_with_permissions(
            path,
            content,
            Some(expected_existing),
            true,
            false,
            Some(std::fs::Permissions::from_mode(0o755)),
        )
    }

    #[cfg(not(unix))]
    write_if_changed_impl(path, content, Some(expected_existing), true)
}

/// Replace a file that the caller has proven is wholly generated by
/// ContextStream. No recovery backup is created because there is no user state
/// to recover. The exact snapshot and concurrent-change checks still apply.
pub(crate) fn write_owned_file_if_unchanged(
    path: &Path,
    content: &str,
    expected_existing: Option<&str>,
) -> Result<bool> {
    write_if_changed_impl(path, content, Some(expected_existing), false)
}

/// Atomically update a wholly ContextStream-owned status/generated file.
///
/// Unlike editor configuration, these files contain no user-authored state,
/// so they need no recovery sidecar. The write still gets no-op detection,
/// private creation permissions, fsync, and the process-wide dry-run gate.
pub(crate) fn write_owned_file_if_changed(path: &Path, content: &str) -> Result<bool> {
    refuse_symlink_deletion(path)?;
    write_if_changed_impl(path, content, None, false)
}

fn write_if_changed_from_snapshot(
    path: &Path,
    content: &str,
    loaded: &LoadedConfig,
) -> Result<bool> {
    write_if_changed_from_snapshot_with_permissions(path, content, loaded, false)
}

fn write_if_changed_from_snapshot_with_permissions(
    path: &Path,
    content: &str,
    loaded: &LoadedConfig,
    force_private_permissions: bool,
) -> Result<bool> {
    write_if_changed_from_snapshot_with_options(
        path,
        content,
        loaded,
        force_private_permissions,
        true,
    )
}

fn write_if_changed_from_snapshot_with_options(
    path: &Path,
    content: &str,
    loaded: &LoadedConfig,
    force_private_permissions: bool,
    create_backup: bool,
) -> Result<bool> {
    let resolved_path = resolve_edit_path(path)?;
    if resolved_path != loaded.resolved_path {
        bail!(
            "Refusing to modify {} because its symlink target changed after ContextStream read it. Re-run the command.",
            path.display()
        );
    }
    let expected = if loaded.existed {
        Some(Some(loaded.raw.as_str()))
    } else {
        Some(None)
    };
    write_if_changed_impl_with_permissions(
        &resolved_path,
        content,
        expected,
        create_backup,
        force_private_permissions,
        None,
    )
}

/// `expected` is `None` for an unconditioned direct write, `Some(None)` when
/// the caller loaded a missing file, and `Some(Some(raw))` when the caller
/// loaded an existing snapshot. Snapshot callers fail closed if another
/// process or the editor changed the file before commit.
fn write_if_changed_impl(
    path: &Path,
    content: &str,
    expected: Option<Option<&str>>,
    create_backup: bool,
) -> Result<bool> {
    write_if_changed_impl_with_permissions(path, content, expected, create_backup, false, None)
}

fn write_if_changed_impl_with_permissions(
    path: &Path,
    content: &str,
    expected: Option<Option<&str>>,
    create_backup: bool,
    force_private_permissions: bool,
    replacement_permissions: Option<std::fs::Permissions>,
) -> Result<bool> {
    #[cfg(not(unix))]
    let _ = force_private_permissions;

    let resolved_path = resolve_edit_path(path)?;
    let path = resolved_path.as_path();
    let existing = read_existing_text(path)?;

    if let Some(expected) = expected {
        let matches = match (expected, existing.as_deref()) {
            (None, None) => true,
            (Some(expected), Some(current)) => expected == current,
            _ => false,
        };
        if !matches {
            bail!(
                "Refusing to modify {} because it changed after ContextStream read it. Re-run the command.",
                path.display()
            );
        }
    }

    let action = match existing.as_deref() {
        Some(current) if current == content => ChangeAction::Unchanged,
        Some(_) => ChangeAction::Modify,
        None => ChangeAction::Create,
    };

    if is_dry_run() {
        let summary = match action {
            ChangeAction::Unchanged => String::new(),
            _ => privacy_safe_change_summary(existing.as_deref().unwrap_or(""), content),
        };
        record_planned(path, action, summary);
        return Ok(action != ChangeAction::Unchanged);
    }

    if action == ChangeAction::Unchanged {
        return Ok(false);
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let original_permissions = replacement_permissions.or_else(|| {
        std::fs::metadata(path)
            .ok()
            .map(|metadata| metadata.permissions())
    });
    let (tmp, mut file) = create_unique_temp(path)?;
    let prepare_result = (|| -> Result<()> {
        file.write_all(content.as_bytes())?;
        if let Some(permissions) = original_permissions {
            std::fs::set_permissions(&tmp, permissions)?;
        }
        #[cfg(unix)]
        if existing.is_none() || force_private_permissions {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        file.sync_all()?;
        Ok(())
    })();
    if let Err(error) = prepare_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(error)
            .with_context(|| format!("Could not prepare replacement for {}", path.display()));
    }
    drop(file);

    // Narrow the lost-update window to the final rename. The editor may have
    // changed the file while serialization or temp-file I/O was in progress.
    let current = read_existing_text(path)?;
    if current != existing {
        let _ = std::fs::remove_file(&tmp);
        bail!(
            "Refusing to modify {} because it changed while ContextStream was preparing the edit. Re-run the command.",
            path.display()
        );
    }

    let backup_created = match (create_backup, existing.as_ref()) {
        (true, Some(previous)) => create_backup_if_absent(path, previous.as_bytes())?,
        _ => false,
    };

    // Backup I/O opens another small race window. Re-check after it so a
    // failed compare does not strand a stale "first backup" that would later
    // mask the actual pre-write contents.
    let current_after_backup = match read_existing_text(path) {
        Ok(current) => current,
        Err(error) => {
            let _ = std::fs::remove_file(&tmp);
            if let Some(ref previous) = existing {
                discard_new_backup_if_unchanged(path, previous.as_bytes(), backup_created);
            }
            return Err(error);
        }
    };
    if current_after_backup != existing {
        let _ = std::fs::remove_file(&tmp);
        if let Some(ref previous) = existing {
            discard_new_backup_if_unchanged(path, previous.as_bytes(), backup_created);
        }
        bail!(
            "Refusing to modify {} because it changed while ContextStream was backing it up. Re-run the command.",
            path.display()
        );
    }

    if let Err(error) = replace_with_temp(path, &tmp) {
        if let Some(ref previous) = existing {
            if read_existing_text(path).ok().flatten().as_deref() == Some(previous.as_str()) {
                discard_new_backup_if_unchanged(path, previous.as_bytes(), backup_created);
            }
        }
        return Err(error);
    }
    sync_parent_directory(path);
    Ok(true)
}

/// Serialize `value` as pretty JSON with a stable trailing newline.
pub fn to_pretty(value: &Value) -> Result<String> {
    let mut out = serde_json::to_string_pretty(value)?;
    out.push('\n');
    Ok(out)
}

// ============================================================================
// Surgical JSONC editing
// ============================================================================

#[derive(Debug, Clone)]
struct Entry {
    key: String,
    key_start: usize,
    value_start: usize,
    value_end: usize,
}

#[derive(Debug)]
struct RootScan {
    entries: Vec<Entry>,
    close: usize,
}

fn skip_trivia(b: &[u8], mut i: usize) -> Result<usize> {
    loop {
        while i < b.len() && (b[i] as char).is_ascii_whitespace() {
            i += 1;
        }
        if i + 1 < b.len() && b[i] == b'/' && b[i + 1] == b'/' {
            i += 2;
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if i + 1 < b.len() && b[i] == b'/' && b[i + 1] == b'*' {
            let comment_start = i;
            i += 2;
            let mut terminated = false;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            if i + 1 < b.len() {
                i += 2;
                terminated = true;
            }
            if !terminated {
                bail!("unterminated block comment at offset {}", comment_start);
            }
            continue;
        }
        return Ok(i);
    }
}

/// Advance past a JSON string literal starting at `i` (which must be `"`).
fn skip_string(b: &[u8], mut i: usize) -> Result<usize> {
    if i >= b.len() || b[i] != b'"' {
        bail!("expected string literal");
    }
    i += 1;
    while i < b.len() {
        match b[i] {
            b'\\' => i += 2,
            b'"' => return Ok(i + 1),
            _ => i += 1,
        }
    }
    bail!("unterminated string literal")
}

fn read_string_value(b: &[u8], i: usize) -> Result<(String, usize)> {
    let end = skip_string(b, i)?;
    let raw = std::str::from_utf8(&b[i..end])?;
    let parsed: String = serde_json::from_str(raw)
        .with_context(|| format!("invalid JSON object key at offset {i}"))?;
    Ok((parsed, end))
}

/// Advance past one JSON value starting at `i`, tracking nesting.
fn skip_value(b: &[u8], mut i: usize) -> Result<usize> {
    if i >= b.len() {
        bail!("unexpected end of input while reading value");
    }

    match b[i] {
        b'"' => skip_string(b, i),
        b'{' | b'[' => {
            let mut expected_closers = Vec::new();
            while i < b.len() {
                match b[i] {
                    b'"' => {
                        i = skip_string(b, i)?;
                        continue;
                    }
                    b'/' if i + 1 < b.len() && (b[i + 1] == b'/' || b[i + 1] == b'*') => {
                        i = skip_trivia(b, i)?;
                        continue;
                    }
                    b'{' => expected_closers.push(b'}'),
                    b'[' => expected_closers.push(b']'),
                    closer @ (b'}' | b']') => {
                        let expected = expected_closers.pop().ok_or_else(|| {
                            anyhow::anyhow!("unexpected closing delimiter at offset {i}")
                        })?;
                        if closer != expected {
                            bail!(
                                "mismatched closing delimiter '{}' at offset {} (expected '{}')",
                                closer as char,
                                i,
                                expected as char
                            );
                        }
                        if expected_closers.is_empty() {
                            return Ok(i + 1);
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            bail!("unterminated object or array")
        }
        _ => {
            // Primitive: number, true, false, null.
            let start = i;
            while i < b.len() && !matches!(b[i], b',' | b'}' | b']') && !b[i].is_ascii_whitespace()
            {
                if b[i] == b'/' && i + 1 < b.len() && (b[i + 1] == b'/' || b[i + 1] == b'*') {
                    break;
                }
                i += 1;
            }
            if i == start {
                bail!("empty value");
            }
            Ok(i)
        }
    }
}

fn finish_root_scan(raw: &str, entries: Vec<Entry>, close: usize) -> Result<RootScan> {
    let trailing = skip_trivia(raw.as_bytes(), close + 1)?;
    if trailing != raw.len() {
        bail!("unexpected trailing content at offset {}", trailing);
    }
    let parsed = try_parse_json_like(raw)?;
    if !parsed.is_object() {
        bail!("expected a JSON object at the top level");
    }
    Ok(RootScan { entries, close })
}

fn scan_root(raw: &str) -> Result<RootScan> {
    let b = raw.as_bytes();
    let mut i = skip_trivia(b, 0)?;

    // Tolerate a UTF-8 BOM.
    if raw.starts_with('\u{feff}') {
        i = skip_trivia(b, i.max('\u{feff}'.len_utf8()))?;
    }

    if i >= b.len() || b[i] != b'{' {
        bail!("expected a JSON object at the top level");
    }
    i = skip_trivia(b, i + 1)?;

    let mut entries = Vec::new();
    let mut seen_keys = HashSet::new();
    loop {
        if i >= b.len() {
            bail!("unterminated top-level object");
        }
        match b[i] {
            b'}' => {
                return finish_root_scan(raw, entries, i);
            }
            b'"' => {}
            other => bail!("unexpected byte '{}' at offset {}", other as char, i),
        }

        let key_start = i;
        let (key, after_key) = read_string_value(b, i)?;
        let mut j = skip_trivia(b, after_key)?;
        if j >= b.len() || b[j] != b':' {
            bail!("expected ':' after key '{}'", key);
        }
        j = skip_trivia(b, j + 1)?;
        let value_start = j;
        let value_end = skip_value(b, j)?;

        if !seen_keys.insert(key.clone()) {
            bail!(
                "refusing to edit a JSON object with duplicate top-level key '{}'",
                key
            );
        }

        entries.push(Entry {
            key,
            key_start,
            value_start,
            value_end,
        });
        i = skip_trivia(b, value_end)?;
        if i >= b.len() {
            bail!("unterminated top-level object");
        }
        match b[i] {
            b',' => {
                i = skip_trivia(b, i + 1)?;
                if i < b.len() && b[i] == b'}' {
                    return finish_root_scan(raw, entries, i);
                }
            }
            b'}' => return finish_root_scan(raw, entries, i),
            other => {
                bail!(
                    "expected ',' or '}}' after top-level value, found '{}' at offset {}",
                    other as char,
                    i
                )
            }
        }
    }
}

fn contains_comment_outside_strings(raw: &str, start: usize, end: usize) -> Result<bool> {
    let bytes = raw.as_bytes();
    let mut i = start;
    while i < end {
        match bytes[i] {
            b'"' => {
                i = skip_string(bytes, i)?;
            }
            b'/' if i + 1 < end && matches!(bytes[i + 1], b'/' | b'*') => return Ok(true),
            _ => i += 1,
        }
    }
    Ok(false)
}

/// Infer the indentation used for top-level entries.
fn detect_indent(raw: &str, scan: &RootScan) -> String {
    if let Some(first) = scan.entries.first() {
        let head = &raw[..first.value_start];
        if let Some(line_start) = head.rfind('\n') {
            let line = &raw[line_start + 1..];
            let ws: String = line
                .chars()
                .take_while(|c| *c == ' ' || *c == '\t')
                .collect();
            if !ws.is_empty() {
                return ws;
            }
        }
    }
    "  ".to_string()
}

fn indent_block(text: &str, indent: &str) -> String {
    let mut out = String::with_capacity(text.len() + indent.len() * 4);
    for (idx, line) in text.lines().enumerate() {
        if idx > 0 {
            out.push('\n');
            if !line.is_empty() {
                out.push_str(indent);
            }
        }
        out.push_str(line);
    }
    out
}

/// Set a top-level key, preserving every other byte of the document.
///
/// Replaces the value in place when the key exists, otherwise appends a new
/// entry. Comments, key order, and whitespace elsewhere are untouched.
pub fn set_top_level_key(raw: &str, key: &str, value: &Value) -> Result<String> {
    if raw.trim().is_empty() {
        return to_pretty(&serde_json::json!({ key: value }));
    }

    let scan = scan_root(raw)?;
    let indent = detect_indent(raw, &scan);
    let rendered = indent_block(&serde_json::to_string_pretty(value)?, &indent);

    if let Some(entry) = scan.entries.iter().find(|e| e.key == key) {
        if contains_comment_outside_strings(raw, entry.value_start, entry.value_end)? {
            bail!(
                "refusing to replace top-level key '{}' because its value contains comments that cannot be preserved safely",
                key
            );
        }
        let mut out = String::with_capacity(raw.len() + rendered.len());
        out.push_str(&raw[..entry.value_start]);
        out.push_str(&rendered);
        out.push_str(&raw[entry.value_end..]);
        return Ok(out);
    }

    // Insert a new entry immediately before the root's closing indentation.
    // Existing trivia stays before the new key, so a comment following the
    // previous value cannot silently become a comment on the inserted value.
    let b = raw.as_bytes();
    let comma_at = match scan.entries.last() {
        Some(last) => {
            let after = skip_trivia(b, last.value_end)?;
            (after >= b.len() || b[after] != b',').then_some(last.value_end)
        }
        None => None,
    };
    let line_start = raw[..scan.close]
        .rfind('\n')
        .map_or(scan.close, |newline| newline + 1);
    let insert_at = if raw[line_start..scan.close]
        .bytes()
        .all(|byte| matches!(byte, b' ' | b'\t' | b'\r'))
    {
        line_start
    } else {
        scan.close
    };

    let mut out = String::with_capacity(raw.len() + rendered.len() + indent.len() + 4);
    if let Some(comma_at) = comma_at {
        out.push_str(&raw[..comma_at]);
        out.push(',');
        out.push_str(&raw[comma_at..insert_at]);
    } else {
        out.push_str(&raw[..insert_at]);
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&indent);
    out.push_str(&serde_json::to_string(key)?);
    out.push_str(": ");
    out.push_str(&rendered);
    if !raw[insert_at..].starts_with('\n') {
        out.push('\n');
    }
    out.push_str(&raw[insert_at..]);
    Ok(out)
}

/// Remove a top-level key, preserving every other byte of the document.
///
/// Returns the input unchanged when the key is absent.
pub fn remove_top_level_key(raw: &str, key: &str) -> Result<String> {
    if raw.trim().is_empty() {
        return Ok(raw.to_string());
    }

    let scan = scan_root(raw)?;
    let Some(idx) = scan.entries.iter().position(|e| e.key == key) else {
        return Ok(raw.to_string());
    };
    let entry = &scan.entries[idx];
    let b = raw.as_bytes();
    if contains_comment_outside_strings(raw, entry.key_start, entry.value_end)? {
        bail!(
            "refusing to remove top-level key '{}' because its key or value contains comments that cannot be preserved safely",
            key
        );
    }

    // Remove only the key/value syntax and one delimiter. Trivia outside that
    // syntax is spliced back into place so user comments before or after the
    // managed key survive a surgical uninstall.
    let after = skip_trivia(b, entry.value_end)?;
    let mut out = String::with_capacity(raw.len());
    if after < b.len() && b[after] == b',' {
        out.push_str(&raw[..entry.key_start]);
        out.push_str(&raw[entry.value_end..after]);
        out.push_str(&raw[after + 1..]);
    } else if idx > 0 {
        let prev = &scan.entries[idx - 1];
        let delimiter = skip_trivia(b, prev.value_end)?;
        if delimiter >= b.len() || b[delimiter] != b',' {
            bail!(
                "could not find the delimiter before top-level key '{}'",
                key
            );
        }
        out.push_str(&raw[..delimiter]);
        out.push_str(&raw[delimiter + 1..entry.key_start]);
        out.push_str(&raw[entry.value_end..]);
    } else {
        out.push_str(&raw[..entry.key_start]);
        out.push_str(&raw[entry.value_end..]);
    }
    Ok(out)
}

/// Splice every top-level key that differs between `original` and `updated`
/// into `raw`, leaving untouched regions byte-for-byte identical.
///
/// Lets a caller keep its existing "parse, mutate a `Value`, done" logic while
/// still writing back surgically: only the keys it actually changed are
/// rewritten, so comments and formatting elsewhere survive.
pub fn apply_changed_top_level_keys(
    raw: &str,
    original: &Value,
    updated: &Value,
) -> Result<String> {
    apply_changed_top_level_keys_with_removals(raw, original, updated, &[])
}

fn apply_changed_top_level_keys_with_removals(
    raw: &str,
    original: &Value,
    updated: &Value,
    removed_keys: &[&str],
) -> Result<String> {
    let Some(updated_obj) = updated.as_object() else {
        bail!("updated config is not a JSON object");
    };
    let empty = serde_json::Map::new();
    let original_obj = original.as_object().unwrap_or(&empty);

    if raw.trim().is_empty() {
        return to_pretty(updated);
    }

    let mut out = raw.to_string();

    for (key, new_value) in updated_obj {
        if original_obj.get(key) != Some(new_value) {
            out = set_top_level_key(&out, key, new_value)?;
        }
    }

    for key in original_obj.keys() {
        if !updated_obj.contains_key(key) {
            if removed_keys.contains(&key.as_str()) {
                out = remove_top_level_key(&out, key)?;
            } else {
                bail!(
                    "refusing implicit deletion of top-level key '{}'; use an explicit removal helper",
                    key
                );
            }
        }
    }

    Ok(out)
}

/// Whether a top-level key is present.
pub fn has_top_level_key(raw: &str, key: &str) -> bool {
    scan_root(raw)
        .map(|scan| scan.entries.iter().any(|e| e.key == key))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn replaces_existing_key_and_keeps_comments() {
        let raw = r#"{
  // keep this comment
  "editor.fontSize": 14,
  "mcp": { "servers": { "old": true } },
  /* and this block comment */
  "files.autoSave": "off"
}"#;

        let out = set_top_level_key(raw, "mcp", &json!({ "servers": { "new": true } })).unwrap();

        assert!(out.contains("// keep this comment"));
        assert!(out.contains("/* and this block comment */"));
        assert!(out.contains("\"editor.fontSize\": 14"));
        assert!(out.contains("\"files.autoSave\": \"off\""));
        assert!(out.contains("\"new\""));
        assert!(!out.contains("\"old\""));
    }

    #[test]
    fn appends_missing_key_without_touching_comments() {
        let raw = r#"{
  // user settings
  "editor.fontSize": 14
}"#;

        let out = set_top_level_key(raw, "mcp", &json!({ "a": 1 })).unwrap();

        assert!(out.contains("// user settings"));
        assert!(out.contains("\"editor.fontSize\": 14"));
        assert!(out.contains("\"mcp\""));
        // Valid once comments are stripped.
        try_parse_json_like(&out).expect("result parses as JSONC");
    }

    #[test]
    fn reuses_existing_trailing_comma() {
        let raw = "{\n  \"a\": 1,\n}";
        let out = set_top_level_key(raw, "b", &json!(2)).unwrap();

        assert!(!out.contains(",,"), "produced a double comma: {out}");
        try_parse_json_like(&out).expect("result parses as JSONC");
    }

    #[test]
    fn insertion_does_not_reassociate_trailing_comments() {
        for raw in [
            "{\n  \"a\": 1 /* belongs to a */\n}\n",
            "{\n  \"a\": 1, // belongs to a\n}\n",
            "{\n  // object-level context\n}\n",
        ] {
            let out = set_top_level_key(raw, "managed", &json!(true)).unwrap();
            let comment = if raw.contains("belongs to a") {
                "belongs to a"
            } else {
                "object-level context"
            };
            assert!(
                out.find(comment).unwrap() < out.find("\"managed\"").unwrap(),
                "existing comment moved after the inserted key:\n{out}"
            );
            let parsed = try_parse_json_like(&out).expect("result parses as JSONC");
            assert_eq!(parsed["managed"], json!(true));
        }
    }

    #[test]
    fn insertion_preserves_closing_brace_indentation() {
        let raw = "{\n  \"a\": 1\n  }";
        let out = set_top_level_key(raw, "b", &json!(2)).unwrap();
        assert!(out.ends_with("\n  }"), "{out}");
        try_parse_json_like(&out).expect("result parses as JSONC");
    }

    #[test]
    fn fills_an_empty_object() {
        let out = set_top_level_key("{}", "mcp", &json!({ "a": 1 })).unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(parsed["mcp"]["a"], json!(1));
    }

    #[test]
    fn handles_nested_braces_and_strings_in_values() {
        let raw = r#"{
  "a": { "s": "} not a brace // not a comment", "n": [1, {"x": 2}] },
  "b": 3
}"#;
        let out = set_top_level_key(raw, "b", &json!(4)).unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();

        assert_eq!(parsed["b"], json!(4));
        assert_eq!(parsed["a"]["s"], json!("} not a brace // not a comment"));
        assert_eq!(parsed["a"]["n"][1]["x"], json!(2));
    }

    #[test]
    fn removes_a_key_and_keeps_the_rest() {
        let raw = r#"{
  // top
  "a": 1,
  "mcp": { "x": 1 },
  "b": 2
}"#;
        let out = remove_top_level_key(raw, "mcp").unwrap();
        let parsed = try_parse_json_like(&out).unwrap();

        assert!(parsed.get("mcp").is_none());
        assert_eq!(parsed["a"], json!(1));
        assert_eq!(parsed["b"], json!(2));
        assert!(out.contains("// top"));
    }

    #[test]
    fn removes_the_last_key_without_leaving_a_dangling_comma() {
        let raw = "{\n  \"a\": 1,\n  \"mcp\": 2\n}";
        let out = remove_top_level_key(raw, "mcp").unwrap();

        let parsed: Value = serde_json::from_str(&out).expect("strict JSON after removal");
        assert_eq!(parsed["a"], json!(1));
        assert!(parsed.get("mcp").is_none());
    }

    #[test]
    fn removing_an_absent_key_is_a_noop() {
        let raw = "{\n  \"a\": 1\n}";
        assert_eq!(remove_top_level_key(raw, "nope").unwrap(), raw);
    }

    #[test]
    fn does_not_match_a_same_named_key_nested_deeper() {
        let raw = r#"{
  "wrapper": { "mcp": "NESTED - must not be touched" },
  "mcp": "top-level"
}"#;
        let out = set_top_level_key(raw, "mcp", &json!("replaced")).unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();

        assert_eq!(parsed["mcp"], json!("replaced"));
        assert_eq!(
            parsed["wrapper"]["mcp"],
            json!("NESTED - must not be touched")
        );
    }

    #[test]
    fn inserts_when_only_a_nested_key_of_that_name_exists() {
        let raw = r#"{ "wrapper": { "mcp": 1 } }"#;
        let out = set_top_level_key(raw, "mcp", &json!(2)).unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();

        assert_eq!(parsed["mcp"], json!(2));
        assert_eq!(parsed["wrapper"]["mcp"], json!(1));
    }

    #[test]
    fn handles_escaped_quotes_and_backslashes() {
        let raw = r#"{
  "path": "C:\\Users\\me\\\"quoted\"",
  "mcp": 1
}"#;
        let out = set_top_level_key(raw, "mcp", &json!(2)).unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();

        assert_eq!(parsed["path"], json!(r#"C:\Users\me\"quoted""#));
        assert_eq!(parsed["mcp"], json!(2));
    }

    #[test]
    fn handles_comments_inside_nested_values() {
        let raw = r#"{
  "a": {
    // inner comment with { and " and ,
    "b": [1, 2]
  },
  "mcp": 1
}"#;
        let out = set_top_level_key(raw, "mcp", &json!(2)).unwrap();

        assert!(out.contains("// inner comment with { and \" and ,"));
        let parsed = try_parse_json_like(&out).unwrap();
        assert_eq!(parsed["a"]["b"], json!([1, 2]));
        assert_eq!(parsed["mcp"], json!(2));
    }

    #[test]
    fn refuses_to_replace_a_value_with_comments_inside_it() {
        let raw = r#"{
  "hooks": {
    // user-owned explanation
    "BeforeTool": [{"command": "my-hook"}]
  },
  "theme": "custom"
}"#;

        let error = set_top_level_key(raw, "hooks", &json!({})).unwrap_err();

        assert!(error.to_string().contains("contains comments"));
    }

    #[test]
    fn keeps_a_comment_immediately_after_a_replaced_value() {
        let raw = r#"{"a": 1/* keep */, "b": 2}"#;
        let out = set_top_level_key(raw, "a", &json!(3)).unwrap();

        assert_eq!(out, r#"{"a": 3/* keep */, "b": 2}"#);
        assert_eq!(try_parse_json_like(&out).unwrap()["a"], json!(3));
    }

    #[test]
    fn handles_unicode_and_non_ascii_keys() {
        let raw = "{\n  \"emoji \u{1f600}\": \"caf\u{e9}\",\n  \"mcp\": 1\n}";
        let out = set_top_level_key(raw, "mcp", &json!(2)).unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();

        assert_eq!(parsed["emoji \u{1f600}"], json!("caf\u{e9}"));
        assert_eq!(parsed["mcp"], json!(2));
    }

    #[test]
    fn preserves_primitive_and_negative_number_values() {
        let raw = "{\n  \"n\": -1.5e3,\n  \"t\": true,\n  \"z\": null,\n  \"mcp\": 1\n}";
        let out = set_top_level_key(raw, "mcp", &json!(2)).unwrap();
        let parsed: Value = serde_json::from_str(&out).unwrap();

        assert_eq!(parsed["n"], json!(-1500.0));
        assert_eq!(parsed["t"], json!(true));
        assert_eq!(parsed["z"], Value::Null);
        assert_eq!(parsed["mcp"], json!(2));
    }

    #[test]
    fn round_trips_a_realistic_vscode_settings_file() {
        let raw = r#"// VS Code settings
{
  "editor.formatOnSave": true, // inline comment
  "editor.rulers": [80, 120],
  /* multi
     line */
  "terminal.integrated.env.linux": { "FOO": "bar" },
  "mcp": {
    "servers": {
      "other": { "command": "x" }
    }
  }
}"#;

        let mut value = try_parse_json_like(raw).unwrap();
        value["mcp"]["servers"]["contextstream"] = json!({ "command": "contextstream-mcp" });
        let out = set_top_level_key(raw, "mcp", &value["mcp"]).unwrap();

        assert!(out.starts_with("// VS Code settings"));
        assert!(out.contains("// inline comment"));
        assert!(out.contains("/* multi"));
        assert!(out.contains("\"editor.rulers\": [80, 120]"));

        let parsed = try_parse_json_like(&out).unwrap();
        assert_eq!(parsed["mcp"]["servers"]["other"]["command"], json!("x"));
        assert_eq!(
            parsed["mcp"]["servers"]["contextstream"]["command"],
            json!("contextstream-mcp")
        );
        assert_eq!(parsed["editor.formatOnSave"], json!(true));
    }

    #[test]
    fn strict_dialect_accepts_comments_but_flags_them() {
        let dir = std::env::temp_dir().join(format!("cs-safe-strict-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, "{\n // c\n \"a\": 1\n}").unwrap();

        // Surgical edits preserve comments, so a strict-dialect file with
        // comments is handled rather than refused — but it is flagged so the
        // caller can warn that the host tool may not read it.
        let loaded = read_for_edit(&path, JsonDialect::Strict).unwrap();
        assert_eq!(loaded.value["a"], json!(1));
        assert!(loaded.nonstandard_syntax);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn commit_preserves_comments_in_a_strict_dialect_file() {
        let dir = std::env::temp_dir().join(format!("cs-safe-commit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, "{\n  // keep me\n  \"model\": \"opus\"\n}").unwrap();

        let loaded = read_for_edit(&path, JsonDialect::Strict).unwrap();
        let mut updated = loaded.value.clone();
        updated["hooks"] = json!({ "Stop": [] });

        assert!(commit(&path, &loaded, &updated).unwrap());

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("// keep me"), "comment lost: {after}");
        assert!(after.contains("\"model\": \"opus\""));
        assert!(after.contains("\"hooks\""));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn commit_is_a_noop_when_nothing_changed() {
        let dir = std::env::temp_dir().join(format!("cs-safe-same-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, "{\n  // keep\n  \"a\": 1\n}").unwrap();

        let loaded = read_for_edit(&path, JsonDialect::Strict).unwrap();
        let updated = loaded.value.clone();

        assert!(
            !commit(&path, &loaded, &updated).unwrap(),
            "expected no write"
        );
        assert!(!dir.join(format!("settings.json{}", BACKUP_SUFFIX)).exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn jsonc_dialect_accepts_comments() {
        let dir = std::env::temp_dir().join(format!("cs-safe-jsonc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, "{\n // c\n \"a\": 1\n}").unwrap();

        let loaded = read_for_edit(&path, JsonDialect::Jsonc).unwrap();
        assert_eq!(loaded.value["a"], json!(1));
        assert!(loaded.raw.contains("// c"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn both_dialects_refuse_broken_input() {
        let dir = std::env::temp_dir().join(format!("cs-safe-broken-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, "{ definitely not json").unwrap();

        assert!(read_for_edit(&path, JsonDialect::Strict).is_err());
        assert!(read_for_edit(&path, JsonDialect::Jsonc).is_err());
        // Crucially, the broken file is left exactly as it was.
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{ definitely not json"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_if_changed_skips_identical_content() {
        let dir = std::env::temp_dir().join(format!("cs-safe-noop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, "hello").unwrap();

        assert!(!write_if_changed(&path, "hello").unwrap(), "no-op expected");
        assert!(
            !dir.join(format!("settings.json{}", BACKUP_SUFFIX)).exists(),
            "a no-op must not create a backup"
        );

        assert!(write_if_changed(&path, "world").unwrap(), "change expected");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "world");
        assert_eq!(
            std::fs::read_to_string(dir.join(format!("settings.json{}", BACKUP_SUFFIX))).unwrap(),
            "hello"
        );
        assert!(!dir.join("settings.json.contextstream.tmp").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Dry run is enforced at the single write choke point, so no call site
    /// can bypass it. Production uses process-global state; tests use an
    /// isolated backend and still serialize process-scoped setup fixtures.
    #[test]
    fn dry_run_writes_nothing_and_records_the_plan() {
        let _guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let dir = std::env::temp_dir().join(format!("cs-safe-dry-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let existing = dir.join("existing.json");
        let fresh = dir.join("fresh.json");
        std::fs::write(&existing, "{\n  \"a\": 1\n}").unwrap();

        set_dry_run(true);
        write_if_changed(&existing, "{\n  \"a\": 2\n}").unwrap();
        write_if_changed(&fresh, "{}").unwrap();
        remove_file_if_present(&existing).unwrap();
        let plan = take_planned_changes();
        set_dry_run(false);

        // Nothing on disk moved.
        assert_eq!(
            std::fs::read_to_string(&existing).unwrap(),
            "{\n  \"a\": 1\n}"
        );
        assert!(!fresh.exists(), "dry run created a file");
        assert!(!dir.join(format!("existing.json{}", BACKUP_SUFFIX)).exists());

        // But the plan describes what would have happened.
        assert_eq!(
            plan.len(),
            3,
            "expected every planned operation to remain visible: {plan:?}"
        );
        let for_existing: Vec<_> = plan.iter().filter(|c| c.path == existing).collect();
        assert_eq!(for_existing.len(), 2);
        assert_eq!(for_existing[0].action, ChangeAction::Modify);
        assert_eq!(for_existing[1].action, ChangeAction::Delete);
        assert!(for_existing[0].summary.contains("content preview withheld"));
        assert!(!for_existing[0].summary.contains("\"a\""));
        let for_fresh = plan.iter().find(|c| c.path == fresh).unwrap();
        assert_eq!(for_fresh.action, ChangeAction::Create);
        assert!(!for_fresh.summary.contains("{}"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dry_run_change_summary_never_exposes_file_content() {
        let before = "token = \"cs_live_before_must_not_leak\"\n";
        let after = "token = \"cs_live_after_must_not_leak\"\n";
        let summary = privacy_safe_change_summary(before, after);

        assert!(summary.contains("content preview withheld"));
        assert!(summary.contains("1 line(s)"));
        assert!(summary.contains(&format!("{} byte(s)", before.len())));
        assert!(summary.contains(&format!("{} byte(s)", after.len())));
        assert!(!summary.contains("cs_live_before_must_not_leak"));
        assert!(!summary.contains("cs_live_after_must_not_leak"));
    }

    #[test]
    fn dry_run_change_summary_is_bounded_for_huge_and_minified_files() {
        let huge_minified = format!(
            "{{\"secret\":\"{}\",\"managed\":false}}",
            "single-line-secret-".repeat(256 * 1024)
        );
        let huge_multiline = "another-secret-line\n".repeat(100_000);
        let summary = privacy_safe_change_summary(&huge_minified, &huge_multiline);

        assert!(summary.len() < 256, "summary unexpectedly grew: {summary}");
        assert!(summary.contains("1 line(s)"));
        assert!(summary.contains("100000 line(s)"));
        assert!(!summary.contains("single-line-secret"));
        assert!(!summary.contains("another-secret-line"));
    }

    #[test]
    fn repeated_noop_runs_do_not_clobber_a_good_backup() {
        let dir = std::env::temp_dir().join(format!("cs-safe-bak-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, "original").unwrap();

        write_if_changed(&path, "updated").unwrap();
        write_if_changed(&path, "updated").unwrap();
        write_if_changed(&path, "updated").unwrap();

        assert_eq!(
            std::fs::read_to_string(dir.join(format!("settings.json{}", BACKUP_SUFFIX))).unwrap(),
            "original",
            "backup must still hold the pre-ContextStream content"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn commit_refuses_a_stale_loaded_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "{\"a\":1}\n").unwrap();

        let loaded = read_for_edit(&path, JsonDialect::Strict).unwrap();
        let mut updated = loaded.value.clone();
        updated["a"] = json!(2);

        std::fs::write(&path, "{\"a\":1,\"user\":true}\n").unwrap();
        let error = commit(&path, &loaded, &updated).unwrap_err();

        assert!(error
            .to_string()
            .contains("changed after ContextStream read"));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"a\":1,\"user\":true}\n"
        );
    }

    #[cfg(unix)]
    #[test]
    fn replacement_preserves_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "secret").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        write_if_changed(&path, "updated").unwrap();

        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(backup_path(&path).unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn newly_created_files_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        write_if_changed(&path, "{\"api_key\":\"secret\"}\n").unwrap();

        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn first_backup_survives_multiple_real_updates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "original").unwrap();

        write_if_changed(&path, "first").unwrap();
        write_if_changed(&path, "second").unwrap();

        assert_eq!(
            std::fs::read_to_string(backup_path(&path).unwrap()).unwrap(),
            "original"
        );
    }

    #[cfg(unix)]
    #[test]
    fn planted_backup_symlink_blocks_the_live_edit() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let backup = backup_path(&path).unwrap();
        let unrelated = dir.path().join("unrelated");
        std::fs::write(&path, "user config").unwrap();
        std::fs::write(&unrelated, "do not touch").unwrap();
        symlink(&unrelated, &backup).unwrap();

        let error = write_if_changed(&path, "managed config").unwrap_err();
        assert!(error.to_string().contains("symlink"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "user config");
        assert_eq!(std::fs::read_to_string(&unrelated).unwrap(), "do not touch");
        assert!(backup.is_symlink());
    }

    #[test]
    fn planted_backup_directory_blocks_the_live_edit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let backup = backup_path(&path).unwrap();
        std::fs::write(&path, "user config").unwrap();
        std::fs::create_dir(&backup).unwrap();

        let error = write_if_changed(&path, "managed config").unwrap_err();
        assert!(error.to_string().contains("not a regular file"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "user config");
        assert!(backup.is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn recovery_json_reader_never_follows_a_planted_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let backup = backup_path(&path).unwrap();
        let unrelated = dir.path().join("unrelated.json");
        std::fs::write(&unrelated, "{\"user\":\"do not trust as a backup\"}\n").unwrap();
        symlink(&unrelated, &backup).unwrap();

        let error = read_recovery_for_edit(&backup, JsonDialect::Strict).unwrap_err();
        assert!(error.to_string().contains("symlink"));
        assert_eq!(
            std::fs::read_to_string(&unrelated).unwrap(),
            "{\"user\":\"do not trust as a backup\"}\n"
        );
    }

    #[test]
    fn exact_restore_refuses_a_changed_backup_before_touching_live_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let backup = backup_path(&path).unwrap();
        std::fs::write(&path, "original").unwrap();
        write_if_changed(&path, "managed").unwrap();
        std::fs::write(&backup, "tampered").unwrap();

        let error = restore_text_first_backup(&path, "managed", true, "original").unwrap_err();
        assert!(error.to_string().contains("changed after it was validated"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "managed");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "tampered");
    }

    #[test]
    fn owned_status_write_is_atomic_without_recovery_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index-status.txt");

        assert!(write_owned_file_if_changed(&path, "running\n").unwrap());
        assert!(!write_owned_file_if_changed(&path, "running\n").unwrap());
        assert!(write_owned_file_if_changed(&path, "complete\n").unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "complete\n");
        assert!(!backup_path(&path).unwrap().exists());
    }

    #[cfg(unix)]
    #[test]
    fn owned_status_write_refuses_a_symlink_target() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("user-file");
        let link = dir.path().join("index-status.txt");
        std::fs::write(&target, "user data").unwrap();
        symlink(&target, &link).unwrap();

        assert!(write_owned_file_if_changed(&link, "installer data").is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "user data");
        assert!(link.is_symlink());
    }

    #[test]
    fn deletion_keeps_a_recovery_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hooks.json");
        std::fs::write(&path, "user-owned").unwrap();

        assert!(remove_file_if_present(&path).unwrap());
        assert!(!path.exists());
        assert_eq!(
            std::fs::read_to_string(backup_path(&path).unwrap()).unwrap(),
            "user-owned"
        );
    }

    #[test]
    fn bom_is_accepted_without_being_removed_from_untouched_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, "\u{feff}{\n  \"a\": 1\n}\n").unwrap();

        let loaded = read_for_edit(&path, JsonDialect::Jsonc).unwrap();
        let mut updated = loaded.value.clone();
        updated["b"] = json!(2);
        commit(&path, &loaded, &updated).unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.starts_with('\u{feff}'));
        assert_eq!(
            try_parse_json_like(after.trim_start_matches('\u{feff}')).unwrap()["b"],
            json!(2)
        );
    }

    #[test]
    fn duplicate_top_level_keys_are_refused() {
        let error = set_top_level_key("{\"a\":1,\"a\":2}", "a", &json!(3)).unwrap_err();
        assert!(error.to_string().contains("duplicate top-level key 'a'"));
    }

    #[test]
    fn nested_duplicate_keys_are_rejected_before_a_managed_parent_can_be_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let original =
            "{\"hooks\":{\"PreToolUse\":[{\"command\":\"user-a\"}],\"PreToolUse\":[{\"command\":\"user-b\"}]},\"theme\":\"custom\"}\n";
        std::fs::write(&path, original).unwrap();

        let error = read_for_edit(&path, JsonDialect::Strict)
            .expect_err("duplicate nested hook event must be ambiguous");

        assert!(error.to_string().contains("duplicate object key"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert!(!backup_path(&path).unwrap().exists());
    }

    #[test]
    fn removing_last_key_preserves_comment_after_previous_value() {
        let raw = "{\n  \"a\": 1/* user comment */,\n  \"mcp\": 2\n}\n";
        let after = remove_top_level_key(raw, "mcp").unwrap();

        assert!(after.contains("/* user comment */"));
        assert_eq!(try_parse_json_like(&after).unwrap()["a"], json!(1));
    }

    #[test]
    fn partial_updated_object_cannot_delete_unrelated_keys() {
        let original = json!({"a": 1, "user": true});
        let updated = json!({"a": 2});
        let error = apply_changed_top_level_keys("{\"a\":1,\"user\":true}", &original, &updated)
            .unwrap_err();

        assert!(error
            .to_string()
            .contains("refusing implicit deletion of top-level key 'user'"));
    }

    #[test]
    fn removal_preserves_comments_around_the_removed_entry() {
        let raw = r#"{
  // user comment before managed key
  "mcp": 1 /* user comment after managed value */,
  "theme": "custom"
}"#;

        let after = remove_top_level_key(raw, "mcp").expect("surgical removal");

        assert!(after.contains("// user comment before managed key"));
        assert!(after.contains("/* user comment after managed value */"));
        let parsed = try_parse_json_like(&after).expect("result remains valid JSONC");
        assert!(parsed.get("mcp").is_none());
        assert_eq!(parsed["theme"], json!("custom"));
    }

    #[test]
    fn removal_refuses_comments_inside_the_key_syntax() {
        let raw = r#"{"mcp" /* explain key */ : 1, "theme": "custom"}"#;

        let error = remove_top_level_key(raw, "mcp")
            .expect_err("a comment inside removed syntax must not be discarded");

        assert!(error.to_string().contains("key or value contains comments"));
        assert_eq!(
            try_parse_json_like(raw).expect("input parses")["theme"],
            json!("custom")
        );
    }

    #[test]
    fn scanner_rejects_malformed_delimiters_comments_and_trailing_content() {
        for raw in [
            r#"{"a": [1, 2}}"#,
            r#"{"a": 1,, "b": 2}"#,
            r#"{"a": 1 "b": 2}"#,
            r#"{"a": 1} /* never closed"#,
            r#"{"a": 1} trailing"#,
        ] {
            assert!(
                set_top_level_key(raw, "mcp", &json!(true)).is_err(),
                "malformed input unexpectedly accepted: {raw}"
            );
        }
    }

    #[test]
    fn scanner_handles_a_string_ending_in_an_escaped_backslash() {
        let raw = r#"{"path":"ends with \\","mcp":1}"#;
        let after = set_top_level_key(raw, "mcp", &json!(2)).expect("replace managed key");
        let parsed: Value = serde_json::from_str(&after).expect("strict JSON");

        assert_eq!(parsed["path"], json!("ends with \\"));
        assert_eq!(parsed["mcp"], json!(2));
    }

    #[test]
    fn splice_matrix_matches_value_level_updates() {
        let values = [
            Value::Null,
            json!(false),
            json!(-1250.5),
            json!("plain"),
            json!("ends with \\"),
            json!("/* not a comment */ // nor this , } ]"),
            json!([1, {"target": "nested"}, [true, null]]),
            json!({"target": "nested", "path": "C:\\Users\\me"}),
        ];

        for (before_index, before) in values.iter().enumerate() {
            for (after_index, replacement) in values.iter().enumerate() {
                for trailing_comma in [false, true] {
                    let comma = if trailing_comma { "," } else { "" };
                    let raw = format!(
                        "\u{feff}{{\n  // matrix {before_index}-{after_index}\n  \"unchanged\": {{\"target\":\"nested\"}},\n  \"target\": {}{}\n}}\n",
                        serde_json::to_string(before).unwrap(),
                        comma
                    );
                    let updated =
                        set_top_level_key(&raw, "target", replacement).expect("matrix splice");
                    let parsed = try_parse_json_like(&updated).expect("matrix result parses");

                    assert_eq!(
                        parsed["target"], *replacement,
                        "before={before_index} after={after_index} trailing={trailing_comma}"
                    );
                    assert_eq!(parsed["unchanged"]["target"], json!("nested"));
                    assert!(updated.starts_with('\u{feff}'));
                    assert!(updated.contains(&format!("// matrix {before_index}-{after_index}")));
                }
            }
        }
    }

    fn next_fuzz_u64(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        *state
    }

    fn fuzz_json_value(state: &mut u64, depth: usize) -> Value {
        let choice = next_fuzz_u64(state) % if depth == 0 { 5 } else { 8 };
        match choice {
            0 => Value::Null,
            1 => json!(next_fuzz_u64(state) & 1 == 0),
            2 => json!((next_fuzz_u64(state) as i64).wrapping_rem(1_000_000)),
            3 => json!(format!(
                "fuzz-{}-\\\\-\\\"-//-/*-}}-]-\u{1f600}",
                next_fuzz_u64(state)
            )),
            4 => json!(format!("ends-in-backslash-{}\\", next_fuzz_u64(state))),
            5 => {
                let len = (next_fuzz_u64(state) % 4) as usize;
                Value::Array(
                    (0..len)
                        .map(|_| fuzz_json_value(state, depth - 1))
                        .collect(),
                )
            }
            _ => {
                let len = (next_fuzz_u64(state) % 4) as usize;
                let mut object = serde_json::Map::new();
                for index in 0..len {
                    object.insert(
                        format!("key-{index}-{}", next_fuzz_u64(state) % 17),
                        fuzz_json_value(state, depth - 1),
                    );
                }
                Value::Object(object)
            }
        }
    }

    #[test]
    fn deterministic_jsonc_splice_fuzz_matches_expected_values() {
        let mut state = 0x7f4a_7c15_9e37_79b9;
        for iteration in 0..512 {
            let untouched = fuzz_json_value(&mut state, 5);
            let before = fuzz_json_value(&mut state, 5);
            let replacement = fuzz_json_value(&mut state, 5);
            let trailing_comment = match iteration % 3 {
                0 => "/* value-adjacent block comment */",
                1 => " // value-adjacent line comment",
                _ => "",
            };
            let trailing_comma = if iteration % 2 == 0 { "," } else { "" };
            let raw = format!(
                "\u{feff}{{\n  // deterministic fuzz iteration {iteration}\n  \
                 \"untouched\": {},\n  \"target\": {}{}{}\n}}\n",
                serde_json::to_string(&untouched).unwrap(),
                serde_json::to_string(&before).unwrap(),
                trailing_comment,
                trailing_comma,
            );

            let replaced =
                set_top_level_key(&raw, "target", &replacement).expect("fuzz replacement");
            let parsed = try_parse_json_like(&replaced).expect("replacement must parse");
            assert_eq!(parsed["untouched"], untouched, "iteration {iteration}");
            assert_eq!(parsed["target"], replacement, "iteration {iteration}");
            assert!(replaced.starts_with('\u{feff}'));
            assert!(replaced.contains(&format!("// deterministic fuzz iteration {iteration}")));

            let inserted =
                set_top_level_key(&replaced, "inserted", &before).expect("fuzz insertion");
            let parsed = try_parse_json_like(&inserted).expect("insertion must parse");
            assert_eq!(parsed["untouched"], untouched, "iteration {iteration}");
            assert_eq!(parsed["target"], replacement, "iteration {iteration}");
            assert_eq!(parsed["inserted"], before, "iteration {iteration}");
        }
    }

    #[test]
    fn scanner_handles_deep_and_large_untouched_values_without_corruption() {
        let mut deep = json!("leaf");
        for index in 0..90 {
            deep = json!({"level": index, "next": deep});
        }
        let large = "x\\\"///*}]".repeat(128 * 1024);
        let raw = serde_json::to_string(&json!({
            "deep": deep,
            "large": large,
            "target": 1,
        }))
        .unwrap();

        let replaced = set_top_level_key(&raw, "target", &json!(2)).expect("large/deep splice");
        let parsed: Value = serde_json::from_str(&replaced).expect("large/deep result");

        assert_eq!(parsed["deep"], deep);
        assert_eq!(parsed["large"], large);
        assert_eq!(parsed["target"], json!(2));
    }

    #[test]
    fn stale_text_write_and_delete_snapshots_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rules.md");
        std::fs::write(&path, "snapshot").unwrap();
        std::fs::write(&path, "user changed it").unwrap();

        assert!(write_if_unchanged(&path, "replacement", Some("snapshot")).is_err());
        assert!(remove_file_if_unchanged(&path, "snapshot").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "user changed it");
        assert!(!backup_path(&path).unwrap().exists());
    }

    #[test]
    fn exact_restore_removes_the_consumed_backup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let original = "{\n  // original\n  \"theme\": \"custom\"\n}\n";
        std::fs::write(&path, original).unwrap();
        write_if_changed(&path, "{\"theme\":\"custom\",\"mcp\":{}}\n").unwrap();

        let loaded = read_for_edit(&path, JsonDialect::Strict).unwrap();
        assert!(restore_first_backup(&path, &loaded, original).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        assert!(!backup_path(&path).unwrap().exists());
    }

    #[cfg(unix)]
    #[test]
    fn atomic_commit_preserves_a_symlinked_dotfile() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("tracked-settings.json");
        let link = dir.path().join("settings.json");
        std::fs::write(&target, "{\n  \"theme\": \"custom\"\n}\n").unwrap();
        symlink(&target, &link).unwrap();

        let loaded = read_for_edit(&link, JsonDialect::Strict).unwrap();
        let mut updated = loaded.value.clone();
        updated["mcp"] = json!({"contextstream": {"command": "managed"}});
        commit(&link, &loaded, &updated).unwrap();

        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "atomic replacement must not overwrite the user's symlink"
        );
        let target_content = std::fs::read_to_string(&target).unwrap();
        assert!(target_content.contains("\"theme\": \"custom\""));
        assert!(target_content.contains("\"contextstream\""));
        assert!(backup_path(&link).unwrap().exists());
    }

    #[cfg(unix)]
    #[test]
    fn commit_refuses_a_symlink_target_swap() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.json");
        let second = dir.path().join("second.json");
        let link = dir.path().join("settings.json");
        let original = "{\"theme\":\"same\"}\n";
        std::fs::write(&first, original).unwrap();
        std::fs::write(&second, original).unwrap();
        symlink(&first, &link).unwrap();

        let loaded = read_for_edit(&link, JsonDialect::Strict).unwrap();
        std::fs::remove_file(&link).unwrap();
        symlink(&second, &link).unwrap();
        let mut updated = loaded.value.clone();
        updated["mcp"] = json!({});

        let error = commit(&link, &loaded, &updated).expect_err("swapped symlink must fail");
        assert!(error.to_string().contains("symlink target changed"));
        assert_eq!(std::fs::read_to_string(&first).unwrap(), original);
        assert_eq!(std::fs::read_to_string(&second).unwrap(), original);
    }

    #[cfg(unix)]
    #[test]
    fn deletion_through_a_symlink_fails_closed() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("generated.json");
        let link = dir.path().join("settings.json");
        std::fs::write(&target, "generated").unwrap();
        symlink(&target, &link).unwrap();

        let error = remove_owned_file_if_unchanged(&link, "generated")
            .expect_err("symlinked deletion must fail");
        assert!(error.to_string().contains("Refusing to delete symlinked"));
        assert!(link.exists());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "generated");
    }

    #[cfg(unix)]
    #[test]
    fn dangling_symlink_is_an_error_not_a_missing_config() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("settings.json");
        symlink(dir.path().join("missing-target.json"), &link).unwrap();

        let error = read_for_edit(&link, JsonDialect::Strict)
            .expect_err("dangling symlink must not be treated as missing");
        assert!(error
            .to_string()
            .contains("Could not resolve symlinked config"));
    }

    #[test]
    fn deeply_nested_input_fails_closed_without_touching_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let nested = format!("{{\"value\":{}0{}}}", "[".repeat(256), "]".repeat(256));
        std::fs::write(&path, &nested).unwrap();

        assert!(read_for_edit(&path, JsonDialect::Jsonc).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), nested);
        assert!(!backup_path(&path).unwrap().exists());
    }
}
