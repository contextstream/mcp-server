//! Hooks installation for Claude Code and other editors.
//!
//! Generates hook configurations for ContextStream integration.
//! Claude Code uses ~/.claude/settings.json with the format:
//!   { "hooks": { "EventName": [{ "matcher": "...", "hooks": [{ "type": "command", ... }] }] } }

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use tracing::warn;

use super::{credentials::contextstream_config_dir, editors::Editor, safe_edit};

/// Shorthand for the JSON object type used throughout this module.
type JsonMap = serde_json::Map<String, Value>;

/// Opaque argument appended to every managed editor hook command.
///
/// The host passes trailing hook arguments through to us, so this is
/// cross-platform and does not rely on undocumented JSON fields. Ownership
/// checks require this marker; a user command merely invoking
/// `contextstream-mcp hook ...` is not ours to delete.
pub const MANAGED_HOOK_ARGUMENT: &str = "--contextstream-managed-hook=v1";

// ============================================================================
// Hook Configuration Types (Claude Code settings.json format)
// ============================================================================

/// Single hook matcher group (matches Claude Code settings.json format).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookEntry {
    /// Matcher regex pattern for the hook. Omit or "*" to match all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matcher: Option<String>,

    /// Hook handlers to execute.
    pub hooks: Vec<HookCommand>,
}

/// Hook command configuration (Claude Code settings.json format).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookCommand {
    /// Command type: "command" for shell commands.
    #[serde(rename = "type")]
    pub command_type: String,

    /// Shell command to execute.
    pub command: String,

    /// Timeout in seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
}

/// Complete hooks configuration matching Claude Code settings.json "hooks" key.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClaudeHooksConfig {
    #[serde(rename = "PreToolUse", skip_serializing_if = "Vec::is_empty", default)]
    pub pre_tool_use: Vec<HookEntry>,

    #[serde(rename = "PostToolUse", skip_serializing_if = "Vec::is_empty", default)]
    pub post_tool_use: Vec<HookEntry>,

    #[serde(
        rename = "PostToolUseFailure",
        skip_serializing_if = "Vec::is_empty",
        default
    )]
    pub post_tool_use_failure: Vec<HookEntry>,

    #[serde(
        rename = "InstructionsLoaded",
        skip_serializing_if = "Vec::is_empty",
        default
    )]
    pub instructions_loaded: Vec<HookEntry>,

    #[serde(
        rename = "UserPromptSubmit",
        skip_serializing_if = "Vec::is_empty",
        default
    )]
    pub user_prompt_submit: Vec<HookEntry>,

    #[serde(
        rename = "SessionStart",
        skip_serializing_if = "Vec::is_empty",
        default
    )]
    pub session_start: Vec<HookEntry>,

    #[serde(rename = "Stop", skip_serializing_if = "Vec::is_empty", default)]
    pub stop: Vec<HookEntry>,

    #[serde(rename = "StopFailure", skip_serializing_if = "Vec::is_empty", default)]
    pub stop_failure: Vec<HookEntry>,

    #[serde(rename = "SessionEnd", skip_serializing_if = "Vec::is_empty", default)]
    pub session_end: Vec<HookEntry>,

    #[serde(rename = "PreCompact", skip_serializing_if = "Vec::is_empty", default)]
    pub pre_compact: Vec<HookEntry>,

    #[serde(rename = "PostCompact", skip_serializing_if = "Vec::is_empty", default)]
    pub post_compact: Vec<HookEntry>,

    #[serde(
        rename = "SubagentStart",
        skip_serializing_if = "Vec::is_empty",
        default
    )]
    pub subagent_start: Vec<HookEntry>,

    #[serde(
        rename = "SubagentStop",
        skip_serializing_if = "Vec::is_empty",
        default
    )]
    pub subagent_stop: Vec<HookEntry>,

    #[serde(rename = "TaskCreated", skip_serializing_if = "Vec::is_empty", default)]
    pub task_created: Vec<HookEntry>,

    #[serde(
        rename = "TaskCompleted",
        skip_serializing_if = "Vec::is_empty",
        default
    )]
    pub task_completed: Vec<HookEntry>,

    #[serde(
        rename = "TeammateIdle",
        skip_serializing_if = "Vec::is_empty",
        default
    )]
    pub teammate_idle: Vec<HookEntry>,

    #[serde(
        rename = "Notification",
        skip_serializing_if = "Vec::is_empty",
        default
    )]
    pub notification: Vec<HookEntry>,

    #[serde(
        rename = "PermissionRequest",
        skip_serializing_if = "Vec::is_empty",
        default
    )]
    pub permission_request: Vec<HookEntry>,

    #[serde(
        rename = "ConfigChange",
        skip_serializing_if = "Vec::is_empty",
        default
    )]
    pub config_change: Vec<HookEntry>,

    #[serde(rename = "CwdChanged", skip_serializing_if = "Vec::is_empty", default)]
    pub cwd_changed: Vec<HookEntry>,

    #[serde(rename = "FileChanged", skip_serializing_if = "Vec::is_empty", default)]
    pub file_changed: Vec<HookEntry>,

    #[serde(
        rename = "WorktreeCreate",
        skip_serializing_if = "Vec::is_empty",
        default
    )]
    pub worktree_create: Vec<HookEntry>,

    #[serde(
        rename = "WorktreeRemove",
        skip_serializing_if = "Vec::is_empty",
        default
    )]
    pub worktree_remove: Vec<HookEntry>,

    #[serde(rename = "Elicitation", skip_serializing_if = "Vec::is_empty", default)]
    pub elicitation: Vec<HookEntry>,

    #[serde(
        rename = "ElicitationResult",
        skip_serializing_if = "Vec::is_empty",
        default
    )]
    pub elicitation_result: Vec<HookEntry>,

    /// Hook events this struct does not model — newer Claude Code events, or
    /// keys written by other tools. Captured verbatim so a round-trip through
    /// this type can never silently drop a user's configuration.
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

// ============================================================================
// Binary Installation
// ============================================================================

/// Install the contextstream-mcp binary to a target directory.
///
/// On macOS, clears `com.apple.provenance` and quarantine xattrs that cause
/// Gatekeeper to SIGKILL the binary when copied via `cp`.
///
/// **Note:** On macOS, this command may need to be run from a non-sandboxed
/// terminal (not from within Claude Code) if the target is a system path like
/// `/usr/local/bin`, because sandboxed processes always add `com.apple.provenance`.
pub fn install_binary(target_dir: &std::path::Path) -> Result<()> {
    install_binary_impl(target_dir, true)
}

static BINARY_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Compare two regular files without loading a release binary into memory.
///
/// Hook refreshes run automatically and often encounter the exact binary that
/// is already installed. Treat that as a true no-op: besides avoiding needless
/// wear, it prevents a second full-size staging copy from failing on a
/// space-constrained `$TMPDIR`/home volume even though the working helper is
/// already current.
fn files_have_identical_contents(left: &Path, right: &Path) -> std::io::Result<bool> {
    let left_metadata = std::fs::metadata(left)?;
    let right_metadata = std::fs::metadata(right)?;
    if !left_metadata.is_file()
        || !right_metadata.is_file()
        || left_metadata.len() != right_metadata.len()
    {
        return Ok(false);
    }

    let mut left = std::io::BufReader::new(std::fs::File::open(left)?);
    let mut right = std::io::BufReader::new(std::fs::File::open(right)?);
    let mut left_chunk = [0_u8; 64 * 1024];
    let mut right_chunk = [0_u8; 64 * 1024];

    loop {
        let left_len = left.read(&mut left_chunk)?;
        let right_len = right.read(&mut right_chunk)?;
        if left_len != right_len || left_chunk[..left_len] != right_chunk[..right_len] {
            return Ok(false);
        }
        if left_len == 0 {
            return Ok(true);
        }
    }
}

fn install_binary_impl(target_dir: &std::path::Path, report_success: bool) -> Result<()> {
    let current = std::env::current_exe()
        .map_err(|e| anyhow::anyhow!("Could not determine current executable path: {}", e))?;

    if !target_dir.exists() {
        std::fs::create_dir_all(target_dir)?;
    }

    let binary_name = if cfg!(windows) {
        "contextstream-mcp.exe"
    } else {
        "contextstream-mcp"
    };
    let target = target_dir.join(binary_name);
    if let Ok(metadata) = std::fs::symlink_metadata(&target) {
        if metadata.file_type().is_symlink() {
            anyhow::bail!(
                "Refusing to replace symlinked managed binary {}; remove or repoint the symlink explicitly.",
                target.display()
            );
        }
        if !metadata.is_file() {
            anyhow::bail!(
                "Refusing to replace non-file managed binary path {}",
                target.display()
            );
        }
    }

    let current_canonical = std::fs::canonicalize(&current).unwrap_or_else(|_| current.clone());
    let target_canonical = std::fs::canonicalize(&target).ok();
    if target_canonical.as_ref() == Some(&current_canonical) {
        let version = verify_binary_runs(&target)?;
        if report_success {
            eprintln!("Verified: {}", version);
        }
        return Ok(());
    }
    if target.exists() && files_have_identical_contents(&current, &target).unwrap_or(false) {
        if let Ok(version) = verify_binary_runs(&target) {
            if report_success {
                eprintln!("Verified: {}", version);
            }
            return Ok(());
        }
    }

    let counter = BINARY_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let staged = target_dir.join(format!(
        ".{}.contextstream.tmp.{}.{}",
        binary_name,
        std::process::id(),
        counter
    ));
    let mut source = std::fs::File::open(&current)
        .with_context(|| format!("Could not read current binary {}", current.display()))?;
    let mut destination = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staged)
        .with_context(|| format!("Could not create staged binary beside {}", target.display()))?;
    if let Err(error) = std::io::copy(&mut source, &mut destination) {
        drop(destination);
        let _ = std::fs::remove_file(&staged);
        return Err(error)
            .with_context(|| format!("Failed to stage binary for {}", target.display()));
    }

    // Unix: set executable permissions
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Err(error) =
            std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
        {
            drop(destination);
            let _ = std::fs::remove_file(&staged);
            return Err(error).with_context(|| {
                format!(
                    "Could not make staged binary {} executable",
                    staged.display()
                )
            });
        }
    }
    if let Err(error) = destination.sync_all() {
        drop(destination);
        let _ = std::fs::remove_file(&staged);
        return Err(error)
            .with_context(|| format!("Could not sync staged binary {}", staged.display()));
    }
    drop(destination);

    // macOS: clear quarantine and provenance xattrs, then ad-hoc codesign
    #[cfg(target_os = "macos")]
    {
        let staged_str = staged.to_string_lossy().to_string();
        // Remove all extended attributes
        let _ = std::process::Command::new("xattr")
            .args(["-cr", &staged_str])
            .status();

        // Explicitly remove com.apple.provenance (may fail if system-protected)
        let _ = std::process::Command::new("xattr")
            .args(["-d", "com.apple.provenance", &staged_str])
            .status();

        // Ad-hoc codesign to mark as trusted
        let _ = std::process::Command::new("codesign")
            .args(["--sign", "-", "--force", &staged_str])
            .status();
    }

    // Validate the complete staged executable before replacing the known-good
    // target. A corrupt copy or failed code signature can never take the
    // working helper offline.
    let version = match verify_binary_runs(&staged) {
        Ok(version) => version,
        Err(error) => {
            let _ = std::fs::remove_file(&staged);
            return Err(error).context(
                "The staged ContextStream binary failed validation; the existing binary was left untouched",
            );
        }
    };

    if let Err(error) = replace_binary_atomically(&staged, &target) {
        let _ = std::fs::remove_file(&staged);
        return Err(error);
    }
    sync_binary_parent(&target);

    if report_success {
        eprintln!("Verified: {}", version);
    }
    Ok(())
}

fn verify_binary_runs(path: &Path) -> Result<String> {
    let output = std::process::Command::new(path).arg("--version").output();

    match output {
        Ok(o) if o.status.success() => {
            let version = String::from_utf8_lossy(&o.stdout);
            Ok(version.trim().to_string())
        }
        Ok(o) => {
            let code = o.status.code().unwrap_or(-1);
            Err(anyhow::anyhow!(
                "Binary {} exited with code {} on --version check",
                path.display(),
                code
            ))
        }
        Err(e) => Err(anyhow::anyhow!(
            "Could not execute binary at {}: {}",
            path.display(),
            e
        )),
    }
}

#[cfg(not(windows))]
fn replace_binary_atomically(staged: &Path, target: &Path) -> Result<()> {
    std::fs::rename(staged, target)
        .with_context(|| format!("Could not atomically replace {}", target.display()))
}

#[cfg(windows)]
fn replace_binary_atomically(staged: &Path, target: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let staged_wide: Vec<u16> = staged.as_os_str().encode_wide().chain(Some(0)).collect();
    let target_wide: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    let replaced = unsafe {
        MoveFileExW(
            staged_wide.as_ptr(),
            target_wide.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if replaced != 0 {
        return Ok(());
    }

    // A running Windows executable may reject replacement while still
    // allowing a rename. Stage the old binary aside, and roll it back if the
    // second rename fails; never delete the only working copy first.
    let first_error = std::io::Error::last_os_error();
    if !target.exists() {
        return Err(first_error)
            .with_context(|| format!("Could not install binary at {}", target.display()));
    }
    let counter = BINARY_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let old = target.with_file_name(format!(
        "{}.contextstream.old.{}.{}",
        target
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("contextstream-mcp.exe"),
        std::process::id(),
        counter
    ));
    std::fs::rename(target, &old).with_context(|| {
        format!(
            "Could not replace the in-use binary at {} ({}). Close running ContextStream processes and retry.",
            target.display(),
            first_error
        )
    })?;
    match std::fs::rename(staged, target) {
        Ok(()) => {
            let _ = std::fs::remove_file(&old);
            Ok(())
        }
        Err(install_error) => {
            let rollback = std::fs::rename(&old, target);
            match rollback {
                Ok(()) => Err(install_error).with_context(|| {
                    format!(
                        "Could not install staged binary at {}; restored the previous binary",
                        target.display()
                    )
                }),
                Err(rollback_error) => Err(anyhow::anyhow!(
                    "Could not install staged binary at {}: {}; rollback from {} also failed: {}",
                    target.display(),
                    install_error,
                    old.display(),
                    rollback_error
                )),
            }
        }
    }
}

fn sync_binary_parent(path: &Path) {
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        if let Ok(directory) = std::fs::File::open(parent) {
            let _ = directory.sync_all();
        }
    }
}

/// Managed helper-binary path used for local hook/thin-client execution.
pub fn managed_binary_path() -> PathBuf {
    let binary_name = if cfg!(windows) {
        "contextstream-mcp.exe"
    } else {
        "contextstream-mcp"
    };
    contextstream_config_dir().join("bin").join(binary_name)
}

/// Ensure the managed local helper binary exists at a stable user-writable path.
pub fn ensure_managed_binary_installed() -> Result<PathBuf> {
    ensure_managed_binary_installed_with_report(true)
}

/// Ensure the managed helper binary exists without printing the successful
/// version check. Interactive setup uses this to avoid repeating low-level
/// install noise for every editor.
pub fn ensure_managed_binary_installed_quiet() -> Result<PathBuf> {
    ensure_managed_binary_installed_with_report(false)
}

fn ensure_managed_binary_installed_with_report(report_success: bool) -> Result<PathBuf> {
    let target = managed_binary_path();
    if safe_edit::is_dry_run() {
        let action = if target.exists() {
            safe_edit::ChangeAction::Modify
        } else {
            safe_edit::ChangeAction::Create
        };
        safe_edit::record_external_change(&target, action);
        return Ok(target);
    }
    if let Ok(current) = std::env::current_exe() {
        let current = std::fs::canonicalize(&current).unwrap_or(current);
        let target_canonical = std::fs::canonicalize(&target).unwrap_or_else(|_| target.clone());
        if current == target_canonical {
            return Ok(target);
        }
    }

    let target_dir = target.parent().ok_or_else(|| {
        anyhow::anyhow!("Managed binary path has no parent: {}", target.display())
    })?;
    match install_binary_impl(target_dir, report_success) {
        Ok(()) => Ok(target),
        Err(_e) if cfg!(windows) && existing_binary_runs(&target) => {
            // Windows/Powershell update-hooks is often launched from the same
            // managed helper. If the exe is locked, keep the verified helper in
            // place so hook refresh can still complete instead of printing a
            // Unix-only sudo hint.
            Ok(target)
        }
        Err(e) => Err(e),
    }
}

fn existing_binary_runs(target: &Path) -> bool {
    target.exists()
        && std::process::Command::new(target)
            .arg("--version")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
}

// ============================================================================
// Hook Installation
// ============================================================================

/// Install hooks for an editor.
pub fn install_hooks(editor: &Editor, api_key: Option<&str>) -> Result<()> {
    match editor {
        Editor::ClaudeCode => install_claude_code_hooks(api_key),
        Editor::Cursor => install_cursor_hooks(api_key),
        Editor::Windsurf => install_windsurf_hooks(api_key),
        Editor::Cline | Editor::RooCode => install_vscode_extension_hooks(editor, api_key),
        Editor::KiloCode => {
            // Kilo CLI doesn't support filesystem hooks — enforcement is
            // handled via rules + skills + MCP tools.
            Ok(())
        }
        _ => {
            // Other editors don't support hooks yet
            Ok(())
        }
    }
}

/// Drop ContextStream-owned entries from a raw Claude `hooks` object.
///
/// Operates on the untyped JSON so that event names we do not model and
/// entries we cannot parse survive untouched — an entry we cannot read is by
/// definition not one we wrote, so it is preserved verbatim.
fn strip_contextstream_hook_entries(hooks: &mut serde_json::Map<String, Value>) {
    let keys: Vec<String> = hooks.keys().cloned().collect();
    for key in keys {
        let Some(Value::Array(entries)) = hooks.get(&key).cloned() else {
            // Not an array of hook entries — leave it exactly as it is.
            continue;
        };

        let filtered: Vec<Value> = entries
            .into_iter()
            .filter_map(|entry| {
                let Value::Object(mut group) = entry else {
                    return Some(entry);
                };
                let Some(Value::Array(commands)) = group.get_mut("hooks") else {
                    return Some(Value::Object(group));
                };

                let before = commands.len();
                commands.retain(|command| {
                    !command
                        .get("command")
                        .and_then(Value::as_str)
                        .is_some_and(is_owned_contextstream_hook_command)
                });
                let removed_owned_command = commands.len() != before;
                if removed_owned_command && commands.is_empty() {
                    None
                } else {
                    // Keep every unknown group/command field byte-for-value.
                    // Deserializing through HookEntry here would silently drop
                    // future host fields from otherwise valid user hooks.
                    Some(Value::Object(group))
                }
            })
            .collect();

        if filtered.is_empty() && !hooks[&key].as_array().is_some_and(Vec::is_empty) {
            hooks.remove(&key);
        } else {
            hooks.insert(key, Value::Array(filtered));
        }
    }
}

/// Merge ContextStream hooks into an existing raw `hooks` object, per key.
fn merge_contextstream_hooks_into(
    hooks: &mut serde_json::Map<String, Value>,
    new_hooks: ClaudeHooksConfig,
) -> Result<()> {
    strip_contextstream_hook_entries(hooks);

    let generated = serde_json::to_value(&new_hooks)?;
    let Value::Object(generated) = generated else {
        anyhow::bail!("Generated hook config was not a JSON object");
    };

    for (event, entries) in generated {
        let Value::Array(entries) = entries else {
            continue;
        };
        match hooks.get_mut(&event) {
            Some(Value::Array(existing)) => existing.extend(entries),
            Some(_) => anyhow::bail!(
                "Refusing to modify hook event '{}': its value is not an array.",
                event
            ),
            None => {
                hooks.insert(event, Value::Array(entries));
            }
        }
    }

    Ok(())
}

/// Borrow the `hooks` object from a loaded settings document.
fn hooks_object_of(loaded: &safe_edit::LoadedConfig, path: &Path) -> Result<JsonMap> {
    match loaded.value.get("hooks") {
        Some(Value::Object(existing)) => Ok(existing.clone()),
        Some(Value::Null) | None => Ok(serde_json::Map::new()),
        Some(_) => anyhow::bail!(
            "Refusing to modify {}: \"hooks\" is not a JSON object.",
            path.display()
        ),
    }
}

fn build_claude_hooks_update(
    loaded: &safe_edit::LoadedConfig,
    settings_path: &Path,
    api_key: Option<&str>,
) -> Result<Value> {
    let mut hooks = hooks_object_of(loaded, settings_path)?;
    merge_contextstream_hooks_into(&mut hooks, generate_contextstream_hooks(api_key))?;

    let mut updated = loaded.value.clone();
    updated["hooks"] = Value::Object(hooks);
    Ok(updated)
}

/// Install Claude Code hooks into ~/.claude/settings.json.
fn install_claude_code_hooks(api_key: Option<&str>) -> Result<()> {
    let settings_path = claude_code_settings_path()
        .ok_or_else(|| anyhow::anyhow!("Could not determine Claude Code settings path"))?;

    let loaded = safe_edit::read_for_edit(&settings_path, safe_edit::JsonDialect::Strict)?;
    warn_on_nonstandard_syntax(&loaded, &settings_path);

    // Merge into the existing hooks object in place, so unknown event names
    // and unparseable entries are preserved rather than replaced wholesale.
    let updated = build_claude_hooks_update(&loaded, &settings_path, api_key)?;

    // Surgical write: only the "hooks" key is rewritten, so everything else in
    // the user's settings file keeps its exact bytes.
    safe_edit::commit(&settings_path, &loaded, &updated)?;

    Ok(())
}

/// Note a settings file whose syntax the host tool may not accept.
fn warn_on_nonstandard_syntax(loaded: &safe_edit::LoadedConfig, path: &Path) {
    if loaded.nonstandard_syntax {
        warn!(
            "{} contains comments or trailing commas. ContextStream preserves them, \
             but the editor itself may not parse this file.",
            path.display()
        );
    }
}

/// Install Cursor hooks.
fn install_cursor_hooks(_api_key: Option<&str>) -> Result<()> {
    let hooks_path = cursor_hooks_path()
        .ok_or_else(|| anyhow::anyhow!("Could not determine Cursor hooks path"))?;

    let loaded = safe_edit::read_for_edit(&hooks_path, safe_edit::JsonDialect::Strict)?;
    warn_on_nonstandard_syntax(&loaded, &hooks_path);
    let config = build_cursor_hooks_update(&loaded)?;

    safe_edit::commit(&hooks_path, &loaded, &config)?;

    Ok(())
}

fn build_cursor_hooks_update(loaded: &safe_edit::LoadedConfig) -> Result<Value> {
    let mut config: Value = loaded.value.clone();
    if config.get("version").is_none() {
        config["version"] = json!(1);
    }
    match config.get("hooks") {
        None | Some(Value::Null) => config["hooks"] = json!({}),
        Some(Value::Object(_)) => {}
        Some(_) => anyhow::bail!("Refusing to modify Cursor hooks: \"hooks\" is not an object."),
    }

    let binary = find_binary_path();
    let new_hooks = generate_cursor_hooks(&binary);

    let hooks = config
        .get_mut("hooks")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow::anyhow!("Invalid Cursor hooks config structure"))?;

    // First, remove ContextStream entries from ALL existing hook types.
    // This catches leftovers from older versions that may have written to
    // different hook types (e.g. postToolUse, preCompact).
    strip_flat_contextstream_hook_entries(hooks);

    // Then insert/extend with new ContextStream hooks
    merge_flat_hook_entries(hooks, new_hooks, "Cursor")?;

    Ok(config)
}

/// Install Windsurf hooks.
fn install_windsurf_hooks(_api_key: Option<&str>) -> Result<()> {
    let hooks_path = windsurf_hooks_path()
        .ok_or_else(|| anyhow::anyhow!("Could not determine Windsurf hooks path"))?;

    let loaded = safe_edit::read_for_edit(&hooks_path, safe_edit::JsonDialect::Strict)?;
    warn_on_nonstandard_syntax(&loaded, &hooks_path);
    let config = build_windsurf_hooks_update(&loaded)?;

    safe_edit::commit(&hooks_path, &loaded, &config)?;

    Ok(())
}

fn build_windsurf_hooks_update(loaded: &safe_edit::LoadedConfig) -> Result<Value> {
    let mut config: Value = loaded.value.clone();
    match config.get("hooks") {
        None | Some(Value::Null) => config["hooks"] = json!({}),
        Some(Value::Object(_)) => {}
        Some(_) => anyhow::bail!("Refusing to modify Windsurf hooks: \"hooks\" is not an object."),
    }

    let binary = find_binary_path();
    let new_hooks = generate_windsurf_hooks(&binary);

    let hooks = config
        .get_mut("hooks")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow::anyhow!("Invalid Windsurf hooks config structure"))?;

    strip_flat_contextstream_hook_entries(hooks);
    merge_flat_hook_entries(hooks, new_hooks, "Windsurf")?;

    Ok(config)
}

/// Install hooks for VS Code extensions (Cline, Roo).
///
/// Kilo has no lifecycle-hook mechanism (Tier B — the content watcher and
/// rules cover it; see `Editor::has_hooks`). These editors look for
/// executable scripts named after hook events. We install thin wrappers that
/// dispatch to `contextstream-mcp hook <name>`.
fn install_vscode_extension_hooks(editor: &Editor, _api_key: Option<&str>) -> Result<()> {
    let hooks_dir = vscode_extension_hooks_dir(editor).ok_or_else(|| {
        anyhow::anyhow!(
            "Could not determine hooks path for {}",
            editor.display_name()
        )
    })?;
    let binary = find_binary_path();
    for (base_name, hook_name) in VSCODE_WRAPPER_HOOK_SPECS {
        write_hook_wrapper_script(&hooks_dir, base_name, hook_name, &binary)?;
    }

    Ok(())
}

/// Get Claude Code settings.json path.
fn claude_code_settings_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".claude").join("settings.json"))
}

/// Get Cursor hooks path.
fn cursor_hooks_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".cursor").join("hooks.json"))
}

/// Get Windsurf hooks path.
fn windsurf_hooks_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".codeium").join("windsurf").join("hooks.json"))
}

/// Get Cline global hooks directory.
fn cline_hooks_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| {
        h.join("Documents")
            .join("Cline")
            .join("Rules")
            .join("Hooks")
    })
}

/// Get Roo global hooks directory.
fn roo_hooks_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".roo").join("hooks"))
}

/// Resolve the hooks directory for VS Code-based extensions.
fn vscode_extension_hooks_dir(editor: &Editor) -> Option<PathBuf> {
    match editor {
        Editor::Cline => cline_hooks_dir(),
        Editor::RooCode => roo_hooks_dir(),
        _ => None,
    }
}

/// Get platform-specific wrapper extension.
fn hook_wrapper_extension() -> &'static str {
    if cfg!(windows) {
        ".cmd"
    } else {
        ""
    }
}

fn escape_for_double_quotes_for_platform(value: &str, windows: bool) -> String {
    // A Windows path's backslashes are literal inside cmd.exe double quotes.
    // Prefixing them with another backslash produces a different (usually
    // nonexistent) path such as C:\\Users\\... . Double quotes themselves are
    // not legal in Windows path components, so there is nothing to escape for
    // the managed executable path on that platform.
    if windows {
        return value.to_string();
    }

    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(character, '\\' | '"' | '$' | '`') {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn escape_for_double_quotes(value: &str) -> String {
    escape_for_double_quotes_for_platform(value, cfg!(windows))
}

fn escape_for_powershell_double_quotes(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(character, '`' | '"' | '$') {
            escaped.push('`');
        }
        escaped.push(character);
    }
    escaped
}

/// Build wrapper script content for a hook.
fn hook_wrapper_script_content(binary: &str, hook_name: &str) -> String {
    if cfg!(windows) {
        format!(
            "@echo off\r\nREM ContextStream managed hook wrapper\r\n\"{}\" hook {} {}\r\n",
            escape_for_double_quotes(binary),
            hook_name,
            MANAGED_HOOK_ARGUMENT
        )
    } else {
        format!(
            "#!/bin/bash\n# ContextStream hook wrapper for {}\nexec \"{}\" hook {} {}\n",
            hook_name,
            escape_for_double_quotes(binary),
            hook_name,
            MANAGED_HOOK_ARGUMENT
        )
    }
}

/// Write one hook wrapper script into a hooks directory.
fn write_hook_wrapper_script(
    hooks_dir: &Path,
    base_name: &str,
    hook_name: &str,
    binary: &str,
) -> Result<PathBuf> {
    let path = hooks_dir.join(format!("{}{}", base_name, hook_wrapper_extension()));
    let existing = match std::fs::read_to_string(&path) {
        Ok(existing) => {
            if !is_contextstream_wrapper(&existing, hook_name) {
                anyhow::bail!(
                    "Refusing to overwrite user-owned hook wrapper {}",
                    path.display()
                );
            }
            Some(existing)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Could not inspect existing hook {}", path.display()))
        }
    };
    let content = hook_wrapper_script_content(binary, hook_name);
    safe_edit::write_executable_if_unchanged(&path, &content, existing.as_deref())?;

    Ok(path)
}

fn wrapper_command_has_marker(command: &str, hook_name: &str) -> bool {
    parse_hook_command(command)
        .is_some_and(|(_, rest)| rest == format!("hook {hook_name} {MANAGED_HOOK_ARGUMENT}"))
}

fn legacy_wrapper_program_is_owned(program: &str, windows: bool) -> bool {
    let normalized = program
        .trim()
        .trim_end_matches(" (deleted)")
        .replace('\\', "/");
    let basename = normalized.rsplit('/').next().unwrap_or_default();
    if !(basename.eq_ignore_ascii_case("contextstream-mcp")
        || basename.eq_ignore_ascii_case("contextstream-mcp.exe"))
    {
        return false;
    }

    let paths_match = |left: &str, right: &str| {
        if windows {
            left.eq_ignore_ascii_case(right)
        } else {
            left == right
        }
    };
    let managed = managed_binary_path().to_string_lossy().replace('\\', "/");
    if paths_match(&normalized, &managed) {
        return true;
    }
    if std::env::current_exe().ok().is_some_and(|current| {
        let current = current.to_string_lossy().replace('\\', "/");
        paths_match(&normalized, &current)
    }) {
        return true;
    }

    let normalized_lower = normalized.to_ascii_lowercase();
    normalized_lower.ends_with("/.contextstream/bin/contextstream-mcp")
        || normalized_lower.ends_with("/.contextstream/bin/contextstream-mcp.exe")
        || (!windows
            && matches!(
                normalized_lower.as_str(),
                "/usr/local/bin/contextstream-mcp"
                    | "/usr/bin/contextstream-mcp"
                    | "/opt/homebrew/bin/contextstream-mcp"
            ))
}

fn legacy_wrapper_command_is_owned_for_platform(
    command: &str,
    hook_name: &str,
    windows: bool,
) -> bool {
    let Some((program, rest)) = parse_hook_command(command) else {
        return false;
    };
    if rest != format!("hook {hook_name}") {
        return false;
    }
    legacy_wrapper_program_is_owned(program, windows)
}

fn is_contextstream_wrapper_for_platform(content: &str, hook_name: &str, windows: bool) -> bool {
    let lines: Vec<&str> = content.lines().collect();
    if windows {
        let (command_index, has_wrapper_marker) = match lines.as_slice() {
            ["@echo off", "REM ContextStream managed hook wrapper", _] => (2, true),
            // Exact migration support for the immediately preceding wrapper.
            ["@echo off", _] => (1, false),
            _ => return false,
        };
        let command = lines[command_index];
        return (has_wrapper_marker && wrapper_command_has_marker(command, hook_name))
            || legacy_wrapper_command_is_owned_for_platform(command, hook_name, windows);
    }

    let expected_comment = format!("# ContextStream hook wrapper for {hook_name}");
    let ["#!/bin/bash", comment, command] = lines.as_slice() else {
        return false;
    };
    if *comment != expected_comment {
        return false;
    }
    let Some(command) = command.strip_prefix("exec ") else {
        return false;
    };
    wrapper_command_has_marker(command, hook_name)
        || legacy_wrapper_command_is_owned_for_platform(command, hook_name, windows)
}

fn is_contextstream_wrapper(content: &str, hook_name: &str) -> bool {
    is_contextstream_wrapper_for_platform(content, hook_name, cfg!(windows))
}

// ============================================================================
// Hook Generation
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaudeHookEvent {
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    InstructionsLoaded,
    UserPromptSubmit,
    SessionStart,
    Stop,
    StopFailure,
    SessionEnd,
    PreCompact,
    PostCompact,
    SubagentStart,
    SubagentStop,
    TaskCreated,
    TaskCompleted,
    TeammateIdle,
    Notification,
    PermissionRequest,
    ConfigChange,
    CwdChanged,
    FileChanged,
    WorktreeCreate,
    WorktreeRemove,
    Elicitation,
    ElicitationResult,
}

#[derive(Debug, Clone, Copy)]
struct ClaudeHookSpec {
    event: ClaudeHookEvent,
    matcher: Option<&'static str>,
    hook_name: &'static str,
    timeout: u64,
}

#[derive(Debug, Clone, Copy)]
struct JsonHookSpec {
    event: &'static str,
    hook_name: &'static str,
    timeout: u64,
    matcher: Option<&'static str>,
    show_output: Option<bool>,
}

const CLAUDE_HOOK_SPECS: &[ClaudeHookSpec] = &[
    ClaudeHookSpec {
        event: ClaudeHookEvent::PreToolUse,
        matcher: None,
        hook_name: "pre-tool-use",
        timeout: 5,
    },
    ClaudeHookSpec {
        event: ClaudeHookEvent::PostToolUse,
        matcher: Some(
            "Edit|Write|NotebookEdit|mcp__contextstream__init|mcp__contextstream__project",
        ),
        hook_name: "post-tool-use",
        timeout: 10,
    },
    // Observe Bash tool calls to tag local git capture with the agent session.
    // Coexists with the Edit|Write|… PostToolUse entry above (distinct matcher);
    // merge_hooks dedupes ContextStream entries on reinstall.
    ClaudeHookSpec {
        event: ClaudeHookEvent::PostToolUse,
        matcher: Some("Bash"),
        hook_name: "git-bash-observed",
        timeout: 5,
    },
    ClaudeHookSpec {
        event: ClaudeHookEvent::PostToolUseFailure,
        matcher: None,
        hook_name: "post-tool-use-failure",
        timeout: 10,
    },
    ClaudeHookSpec {
        event: ClaudeHookEvent::InstructionsLoaded,
        matcher: None,
        hook_name: "instructions-loaded",
        timeout: 10,
    },
    ClaudeHookSpec {
        event: ClaudeHookEvent::UserPromptSubmit,
        matcher: None,
        hook_name: "user-prompt-submit",
        timeout: 5,
    },
    ClaudeHookSpec {
        event: ClaudeHookEvent::UserPromptSubmit,
        matcher: None,
        hook_name: "on-save-intent",
        timeout: 5,
    },
    ClaudeHookSpec {
        event: ClaudeHookEvent::SessionStart,
        matcher: Some("startup|resume|compact"),
        hook_name: "session-start",
        timeout: 15,
    },
    ClaudeHookSpec {
        event: ClaudeHookEvent::Stop,
        matcher: None,
        hook_name: "stop",
        timeout: 15,
    },
    ClaudeHookSpec {
        event: ClaudeHookEvent::StopFailure,
        matcher: None,
        hook_name: "stop-failure",
        timeout: 10,
    },
    ClaudeHookSpec {
        event: ClaudeHookEvent::SessionEnd,
        matcher: None,
        hook_name: "session-end",
        timeout: 10,
    },
    ClaudeHookSpec {
        event: ClaudeHookEvent::PreCompact,
        matcher: None,
        hook_name: "pre-compact",
        timeout: 30,
    },
    ClaudeHookSpec {
        event: ClaudeHookEvent::PostCompact,
        matcher: None,
        hook_name: "post-compact",
        timeout: 15,
    },
    ClaudeHookSpec {
        event: ClaudeHookEvent::SubagentStart,
        matcher: Some("Explore|Plan|general-purpose|custom"),
        hook_name: "subagent-start",
        timeout: 10,
    },
    ClaudeHookSpec {
        event: ClaudeHookEvent::SubagentStop,
        matcher: Some("Plan"),
        hook_name: "subagent-stop",
        timeout: 15,
    },
    ClaudeHookSpec {
        event: ClaudeHookEvent::TaskCreated,
        matcher: None,
        hook_name: "task-created",
        timeout: 10,
    },
    ClaudeHookSpec {
        event: ClaudeHookEvent::TaskCompleted,
        matcher: None,
        hook_name: "task-completed",
        timeout: 10,
    },
    ClaudeHookSpec {
        event: ClaudeHookEvent::TeammateIdle,
        matcher: None,
        hook_name: "teammate-idle",
        timeout: 10,
    },
    ClaudeHookSpec {
        event: ClaudeHookEvent::Notification,
        matcher: None,
        hook_name: "notification",
        timeout: 10,
    },
    ClaudeHookSpec {
        event: ClaudeHookEvent::PermissionRequest,
        matcher: None,
        hook_name: "permission-request",
        timeout: 10,
    },
    ClaudeHookSpec {
        event: ClaudeHookEvent::ConfigChange,
        matcher: None,
        hook_name: "config-change",
        timeout: 10,
    },
    ClaudeHookSpec {
        event: ClaudeHookEvent::CwdChanged,
        matcher: None,
        hook_name: "cwd-changed",
        timeout: 10,
    },
    ClaudeHookSpec {
        event: ClaudeHookEvent::FileChanged,
        matcher: Some(".*"),
        hook_name: "file-changed",
        timeout: 10,
    },
    ClaudeHookSpec {
        event: ClaudeHookEvent::WorktreeCreate,
        matcher: None,
        hook_name: "worktree-create",
        timeout: 15,
    },
    ClaudeHookSpec {
        event: ClaudeHookEvent::WorktreeRemove,
        matcher: None,
        hook_name: "worktree-remove",
        timeout: 15,
    },
    ClaudeHookSpec {
        event: ClaudeHookEvent::Elicitation,
        matcher: Some(".*"),
        hook_name: "elicitation",
        timeout: 10,
    },
    ClaudeHookSpec {
        event: ClaudeHookEvent::ElicitationResult,
        matcher: Some(".*"),
        hook_name: "elicitation-result",
        timeout: 10,
    },
];

const CURSOR_HOOK_SPECS: &[JsonHookSpec] = &[
    JsonHookSpec {
        event: "preToolUse",
        hook_name: "pre-tool-use",
        timeout: 5,
        matcher: Some("*"),
        show_output: None,
    },
    // `beforeSubmitPrompt` cannot inject context into the agent (it only
    // continues/blocks), so it is used only for the `mark_context_required`
    // side-effect that the PreToolUse gate enforces. The old `on-save-intent`
    // entry here was inert and has been removed; save nudges are delivered via
    // `postToolUse.additional_context` and the always-on `.mdc` rules.
    JsonHookSpec {
        event: "beforeSubmitPrompt",
        hook_name: "user-prompt-submit",
        timeout: 5,
        matcher: None,
        show_output: None,
    },
    JsonHookSpec {
        event: "beforeMCPExecution",
        hook_name: "pre-tool-use",
        timeout: 5,
        matcher: Some("*"),
        show_output: None,
    },
    // Generic `postToolUse` is the event that supports `additional_context`
    // injection (unlike `afterMCPExecution`/`afterFileEdit`, which are
    // audit-only). This carries ContextStream post-tool nudges to the agent.
    JsonHookSpec {
        event: "postToolUse",
        hook_name: "post-tool-use",
        timeout: 10,
        matcher: Some("*"),
        show_output: None,
    },
    JsonHookSpec {
        event: "afterMCPExecution",
        hook_name: "post-tool-use",
        timeout: 10,
        matcher: Some("*"),
        show_output: None,
    },
    JsonHookSpec {
        event: "beforeShellExecution",
        hook_name: "pre-tool-use",
        timeout: 5,
        matcher: Some("*"),
        show_output: None,
    },
    JsonHookSpec {
        event: "afterShellExecution",
        hook_name: "post-tool-use-failure",
        timeout: 10,
        matcher: Some("*"),
        show_output: None,
    },
    JsonHookSpec {
        event: "beforeReadFile",
        hook_name: "pre-tool-use",
        timeout: 5,
        matcher: Some("*"),
        show_output: None,
    },
    JsonHookSpec {
        event: "afterFileEdit",
        hook_name: "post-tool-use",
        timeout: 10,
        matcher: Some("*"),
        show_output: None,
    },
    JsonHookSpec {
        event: "sessionStart",
        hook_name: "session-start",
        timeout: 15,
        matcher: Some("*"),
        show_output: None,
    },
    JsonHookSpec {
        event: "sessionEnd",
        hook_name: "session-end",
        timeout: 10,
        matcher: Some("*"),
        show_output: None,
    },
    JsonHookSpec {
        event: "preCompact",
        hook_name: "pre-compact",
        timeout: 15,
        matcher: Some("*"),
        show_output: None,
    },
    JsonHookSpec {
        event: "stop",
        hook_name: "stop",
        timeout: 10,
        matcher: Some("*"),
        show_output: None,
    },
    JsonHookSpec {
        event: "subagentStart",
        hook_name: "subagent-start",
        timeout: 10,
        matcher: Some("*"),
        show_output: None,
    },
    JsonHookSpec {
        event: "subagentStop",
        hook_name: "subagent-stop",
        timeout: 10,
        matcher: Some("*"),
        show_output: None,
    },
    JsonHookSpec {
        event: "taskCompleted",
        hook_name: "task-completed",
        timeout: 10,
        matcher: Some("*"),
        show_output: None,
    },
    JsonHookSpec {
        event: "teammateIdle",
        hook_name: "teammate-idle",
        timeout: 10,
        matcher: Some("*"),
        show_output: None,
    },
    JsonHookSpec {
        event: "notification",
        hook_name: "notification",
        timeout: 10,
        matcher: Some("*"),
        show_output: None,
    },
    JsonHookSpec {
        event: "permissionRequest",
        hook_name: "permission-request",
        timeout: 10,
        matcher: Some("*"),
        show_output: None,
    },
];

const WINDSURF_HOOK_SPECS: &[JsonHookSpec] = &[
    JsonHookSpec {
        event: "pre_mcp_tool_use",
        hook_name: "pre-tool-use",
        timeout: 0,
        matcher: None,
        show_output: Some(true),
    },
    JsonHookSpec {
        event: "pre_user_prompt",
        hook_name: "user-prompt-submit",
        timeout: 0,
        matcher: None,
        show_output: None,
    },
    JsonHookSpec {
        event: "pre_read_code",
        hook_name: "pre-tool-use",
        timeout: 0,
        matcher: None,
        show_output: Some(true),
    },
    JsonHookSpec {
        event: "pre_write_code",
        hook_name: "pre-tool-use",
        timeout: 0,
        matcher: None,
        show_output: Some(true),
    },
    JsonHookSpec {
        event: "pre_run_command",
        hook_name: "pre-tool-use",
        timeout: 0,
        matcher: None,
        show_output: Some(true),
    },
    JsonHookSpec {
        event: "post_write_code",
        hook_name: "post-tool-use",
        timeout: 0,
        matcher: None,
        show_output: Some(false),
    },
    JsonHookSpec {
        event: "post_mcp_tool_use",
        hook_name: "post-tool-use",
        timeout: 0,
        matcher: None,
        show_output: Some(false),
    },
    JsonHookSpec {
        event: "post_cascade_response_with_transcript",
        hook_name: "session-end",
        timeout: 0,
        matcher: None,
        show_output: None,
    },
];

const VSCODE_WRAPPER_HOOK_SPECS: &[(&str, &str)] = &[
    ("PreToolUse", "pre-tool-use"),
    ("UserPromptSubmit", "user-prompt-submit"),
    ("PostToolUse", "post-tool-use"),
];

/// Enforcement-critical hook events that should always be installed.
#[cfg(test)]
const CLAUDE_ENFORCEMENT_CRITICAL_EVENTS: &[ClaudeHookEvent] = &[
    ClaudeHookEvent::PreToolUse,
    ClaudeHookEvent::UserPromptSubmit,
    ClaudeHookEvent::SessionStart,
    ClaudeHookEvent::PreCompact,
    ClaudeHookEvent::PostToolUse,
];

#[cfg(test)]
const CURSOR_ENFORCEMENT_CRITICAL_EVENTS: &[&str] = &[
    "preToolUse",
    "postToolUse",
    "beforeSubmitPrompt",
    "beforeMCPExecution",
    "beforeShellExecution",
    "beforeReadFile",
    "afterFileEdit",
    "sessionStart",
    "preCompact",
];

fn hook_command(binary: &str, hook_name: &str) -> String {
    format!(
        "\"{}\" hook {} {}",
        escape_for_double_quotes(binary),
        hook_name,
        MANAGED_HOOK_ARGUMENT
    )
}

fn push_claude_hook(config: &mut ClaudeHooksConfig, spec: ClaudeHookSpec, binary: &str) {
    let entry = HookEntry {
        matcher: spec.matcher.map(str::to_string),
        hooks: vec![HookCommand {
            command_type: "command".to_string(),
            command: hook_command(binary, spec.hook_name),
            timeout: Some(spec.timeout),
        }],
    };

    match spec.event {
        ClaudeHookEvent::PreToolUse => config.pre_tool_use.push(entry),
        ClaudeHookEvent::PostToolUse => config.post_tool_use.push(entry),
        ClaudeHookEvent::PostToolUseFailure => config.post_tool_use_failure.push(entry),
        ClaudeHookEvent::InstructionsLoaded => config.instructions_loaded.push(entry),
        ClaudeHookEvent::UserPromptSubmit => config.user_prompt_submit.push(entry),
        ClaudeHookEvent::SessionStart => config.session_start.push(entry),
        ClaudeHookEvent::Stop => config.stop.push(entry),
        ClaudeHookEvent::StopFailure => config.stop_failure.push(entry),
        ClaudeHookEvent::SessionEnd => config.session_end.push(entry),
        ClaudeHookEvent::PreCompact => config.pre_compact.push(entry),
        ClaudeHookEvent::PostCompact => config.post_compact.push(entry),
        ClaudeHookEvent::SubagentStart => config.subagent_start.push(entry),
        ClaudeHookEvent::SubagentStop => config.subagent_stop.push(entry),
        ClaudeHookEvent::TaskCreated => config.task_created.push(entry),
        ClaudeHookEvent::TaskCompleted => config.task_completed.push(entry),
        ClaudeHookEvent::TeammateIdle => config.teammate_idle.push(entry),
        ClaudeHookEvent::Notification => config.notification.push(entry),
        ClaudeHookEvent::PermissionRequest => config.permission_request.push(entry),
        ClaudeHookEvent::ConfigChange => config.config_change.push(entry),
        ClaudeHookEvent::CwdChanged => config.cwd_changed.push(entry),
        ClaudeHookEvent::FileChanged => config.file_changed.push(entry),
        ClaudeHookEvent::WorktreeCreate => config.worktree_create.push(entry),
        ClaudeHookEvent::WorktreeRemove => config.worktree_remove.push(entry),
        ClaudeHookEvent::Elicitation => config.elicitation.push(entry),
        ClaudeHookEvent::ElicitationResult => config.elicitation_result.push(entry),
    }
}

fn json_hook_entry(binary: &str, spec: JsonHookSpec) -> Value {
    let mut entry = serde_json::Map::new();
    entry.insert(
        "command".to_string(),
        Value::String(hook_command(binary, spec.hook_name)),
    );
    if spec.timeout > 0 {
        entry.insert("timeout".to_string(), Value::Number(spec.timeout.into()));
    }
    if let Some(matcher) = spec.matcher {
        entry.insert("matcher".to_string(), Value::String(matcher.to_string()));
    }
    if let Some(show_output) = spec.show_output {
        entry.insert("show_output".to_string(), Value::Bool(show_output));
    } else {
        entry.insert("type".to_string(), Value::String("command".to_string()));
    }
    Value::Object(entry)
}

/// Generate Cursor-specific hook config entries.
///
/// Returns a map of hook type name -> entries, making it easy to add new hook
/// types without changing the install/uninstall plumbing.
fn generate_cursor_hooks(binary: &str) -> HashMap<String, Vec<Value>> {
    let mut hooks = HashMap::new();
    for spec in CURSOR_HOOK_SPECS {
        hooks
            .entry(spec.event.to_string())
            .or_insert_with(Vec::new)
            .push(json_hook_entry(binary, *spec));
    }

    hooks
}

/// Build one Windsurf Cascade hook entry.
///
/// Contract (docs.devin.ai/desktop/cascade/hooks): per-entry fields are
/// `command` (run via `bash -c` on macOS/Linux), optional `powershell`
/// (Windows), optional `show_output`, optional `working_directory`.
/// `timeout`/`matcher` and Cursor's `type: "command"` are NOT part of this
/// schema and must not leak in — and without `powershell`, hooks never fire
/// on Windows.
fn windsurf_hook_entry(binary: &str, spec: JsonHookSpec) -> Value {
    let mut entry = serde_json::Map::new();
    let command = hook_command(binary, spec.hook_name);
    entry.insert("command".to_string(), Value::String(command));
    if cfg!(target_os = "windows") {
        // PowerShell requires the call operator to run a quoted path.
        entry.insert(
            "powershell".to_string(),
            Value::String(format!(
                "& \"{}\" hook {} {}",
                escape_for_powershell_double_quotes(binary),
                spec.hook_name,
                MANAGED_HOOK_ARGUMENT
            )),
        );
    }
    if let Some(show_output) = spec.show_output {
        entry.insert("show_output".to_string(), Value::Bool(show_output));
    }
    Value::Object(entry)
}

/// Generate Windsurf-specific hook config entries.
fn generate_windsurf_hooks(binary: &str) -> HashMap<String, Vec<Value>> {
    let mut hooks = HashMap::new();
    for spec in WINDSURF_HOOK_SPECS {
        hooks
            .entry(spec.event.to_string())
            .or_insert_with(Vec::new)
            .push(windsurf_hook_entry(binary, *spec));
    }

    hooks
}

/// Remove existing ContextStream entries from a Cursor hook array.
#[cfg(test)]
fn filter_cursor_hooks(existing: Value) -> Vec<Value> {
    existing
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter(|item| !is_contextstream_cursor_hook(item))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
fn is_contextstream_cursor_hook(entry: &Value) -> bool {
    entry
        .get("command")
        .and_then(|value| value.as_str())
        .map(is_owned_contextstream_hook_command)
        .unwrap_or(false)
}

/// Remove existing ContextStream entries from a Windsurf hook array.
#[cfg(test)]
fn filter_windsurf_hooks(existing: Value) -> Vec<Value> {
    existing
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter(|item| !is_contextstream_windsurf_hook(item))
                .cloned()
                .collect()
        })
        .unwrap_or_default()
}

/// Strip only flat hook entries carrying ContextStream's ownership marker.
///
/// Non-array event values and pre-existing empty arrays are preserved. The
/// former may be a future host schema we do not understand; overwriting it
/// would be data loss, while a generated entry for the same event is handled
/// as an explicit schema conflict by `merge_flat_hook_entries`.
fn strip_flat_contextstream_hook_entries(hooks: &mut serde_json::Map<String, Value>) {
    let keys: Vec<String> = hooks.keys().cloned().collect();
    for key in keys {
        let Some(Value::Array(entries)) = hooks.get_mut(&key) else {
            continue;
        };
        let before = entries.len();
        entries.retain(|entry| {
            !entry
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(is_owned_contextstream_hook_command)
        });
        if before != entries.len() && entries.is_empty() {
            hooks.remove(&key);
        }
    }
}

fn merge_flat_hook_entries(
    hooks: &mut serde_json::Map<String, Value>,
    generated: HashMap<String, Vec<Value>>,
    editor_name: &str,
) -> Result<()> {
    for (event, entries) in generated {
        match hooks.get_mut(&event) {
            Some(Value::Array(existing)) => existing.extend(entries),
            Some(_) => anyhow::bail!(
                "Refusing to modify {} hook event '{}': its value is not an array.",
                editor_name,
                event
            ),
            None => {
                hooks.insert(event, Value::Array(entries));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
fn is_contextstream_windsurf_hook(entry: &Value) -> bool {
    entry
        .get("command")
        .and_then(|value| value.as_str())
        .map(is_owned_contextstream_hook_command)
        .unwrap_or(false)
}

/// Generate ContextStream hooks using binary dispatch.
///
/// All hooks call `contextstream-mcp hook <name>` which dispatches to
/// native Rust handlers. Format matches Claude Code settings.json schema:
///   { "matcher": "pattern", "hooks": [{ "type": "command", "command": "...", "timeout": N }] }
fn generate_contextstream_hooks(_api_key: Option<&str>) -> ClaudeHooksConfig {
    let binary = find_binary_path();
    let mut config = ClaudeHooksConfig::default();
    for spec in CLAUDE_HOOK_SPECS {
        push_claude_hook(&mut config, *spec, &binary);
    }
    config
}

/// Find the contextstream-mcp binary path.
fn find_binary_path() -> String {
    let managed = managed_binary_path();
    // During a dry run the helper is intentionally not copied, but subsequent
    // planned hook content must still show the path the real run would use.
    if safe_edit::is_dry_run() || managed.exists() {
        if let Some(path) = sanitize_binary_path(&managed) {
            return path;
        }
    }

    // Prefer the currently running binary so local dev builds can safely
    // regenerate hooks without accidentally pinning them back to an older
    // PATH-resolved install.
    if let Ok(path) = std::env::current_exe() {
        if let Some(path) = sanitize_binary_path(&path) {
            return path;
        }
    }

    // Fall back to PATH lookup for wrapper-driven invocations.
    if let Ok(path) = which::which("contextstream-mcp") {
        if let Some(path) = sanitize_binary_path(&path) {
            return path;
        }
    }

    // Last resort
    "contextstream-mcp".to_string()
}

fn sanitize_binary_path(path: &Path) -> Option<String> {
    let raw = path.to_string_lossy();
    if raw.is_empty() {
        return None;
    }

    if let Some(stripped) = raw.strip_suffix(" (deleted)") {
        if Path::new(stripped).exists() {
            return Some(stripped.to_string());
        }
        return None;
    }

    Some(raw.to_string())
}

/// Check if a hook entry is a ContextStream hook.
#[cfg(test)]
fn is_contextstream_hook(hook: &HookEntry) -> bool {
    hook.hooks
        .iter()
        .any(|command| is_owned_contextstream_hook_command(&command.command))
}

fn parse_hook_command(command: &str) -> Option<(&str, &str)> {
    let command = command.trim();
    let (program, rest) = if let Some(quoted) = command.strip_prefix('"') {
        let mut escaped = false;
        let mut end = None;
        for (index, character) in quoted.char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            match character {
                '\\' => escaped = true,
                '"' => {
                    end = Some(index);
                    break;
                }
                _ => {}
            }
        }
        let end = end?;
        (&quoted[..end], quoted[end + 1..].trim())
    } else {
        let (program, rest) = command.split_once(char::is_whitespace)?;
        (program, rest.trim())
    };

    Some((program, rest))
}

/// New hooks carry an explicit marker. The marker, not the executable's
/// filename, is the ownership proof: development builds and renamed binaries
/// must remain refreshable and uninstallable without falling back to unsafe
/// product-name sniffing.
fn is_contextstream_hook_command(command: &str) -> bool {
    let Some((_, rest)) = parse_hook_command(command) else {
        return false;
    };
    let mut args = rest.split_whitespace();
    matches!(
        (args.next(), args.next(), args.next(), args.next()),
        (Some("hook"), Some(_), Some(MANAGED_HOOK_ARGUMENT), None)
    )
}

/// Recognize unmarked hooks from older ContextStream versions only when their
/// executable lives at ContextStream's managed-binary path. An unmarked
/// `contextstream-mcp hook ...` elsewhere may be user-authored and survives.
fn is_legacy_managed_hook_command(command: &str) -> bool {
    let Some((program, rest)) = parse_hook_command(command) else {
        return false;
    };
    let mut args = rest.split_whitespace();
    if !matches!(
        (args.next(), args.next(), args.next()),
        (Some("hook"), Some(_), None)
    ) {
        return false;
    }

    let managed = managed_binary_path().to_string_lossy().to_string();
    if cfg!(windows) {
        program.eq_ignore_ascii_case(&managed)
    } else {
        program == managed
    }
}

pub(super) fn is_owned_contextstream_hook_command(command: &str) -> bool {
    is_contextstream_hook_command(command) || is_legacy_managed_hook_command(command)
}

pub(super) fn contains_owned_hook_command(value: &Value) -> bool {
    match value {
        Value::Array(items) => items.iter().any(contains_owned_hook_command),
        Value::Object(object) => {
            object
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(is_owned_contextstream_hook_command)
                || object.values().any(contains_owned_hook_command)
        }
        _ => false,
    }
}

fn current_managed_hook_name(command: &str) -> Option<&str> {
    let command = command.trim().strip_prefix("& ").unwrap_or(command.trim());
    let (_, rest) = parse_hook_command(command)?;
    let mut args = rest.split_whitespace();
    match (args.next(), args.next(), args.next(), args.next()) {
        (Some("hook"), Some(hook_name), Some(MANAGED_HOOK_ARGUMENT), None) => Some(hook_name),
        _ => None,
    }
}

const fn claude_hook_event_name(event: ClaudeHookEvent) -> &'static str {
    match event {
        ClaudeHookEvent::PreToolUse => "PreToolUse",
        ClaudeHookEvent::PostToolUse => "PostToolUse",
        ClaudeHookEvent::PostToolUseFailure => "PostToolUseFailure",
        ClaudeHookEvent::InstructionsLoaded => "InstructionsLoaded",
        ClaudeHookEvent::UserPromptSubmit => "UserPromptSubmit",
        ClaudeHookEvent::SessionStart => "SessionStart",
        ClaudeHookEvent::Stop => "Stop",
        ClaudeHookEvent::StopFailure => "StopFailure",
        ClaudeHookEvent::SessionEnd => "SessionEnd",
        ClaudeHookEvent::PreCompact => "PreCompact",
        ClaudeHookEvent::PostCompact => "PostCompact",
        ClaudeHookEvent::SubagentStart => "SubagentStart",
        ClaudeHookEvent::SubagentStop => "SubagentStop",
        ClaudeHookEvent::TaskCreated => "TaskCreated",
        ClaudeHookEvent::TaskCompleted => "TaskCompleted",
        ClaudeHookEvent::TeammateIdle => "TeammateIdle",
        ClaudeHookEvent::Notification => "Notification",
        ClaudeHookEvent::PermissionRequest => "PermissionRequest",
        ClaudeHookEvent::ConfigChange => "ConfigChange",
        ClaudeHookEvent::CwdChanged => "CwdChanged",
        ClaudeHookEvent::FileChanged => "FileChanged",
        ClaudeHookEvent::WorktreeCreate => "WorktreeCreate",
        ClaudeHookEvent::WorktreeRemove => "WorktreeRemove",
        ClaudeHookEvent::Elicitation => "Elicitation",
        ClaudeHookEvent::ElicitationResult => "ElicitationResult",
    }
}

fn count_owned_flat_entries(hooks: &serde_json::Map<String, Value>) -> usize {
    hooks
        .values()
        .filter_map(Value::as_array)
        .flat_map(|entries| entries.iter())
        .filter(|entry| {
            entry
                .get("command")
                .and_then(Value::as_str)
                .is_some_and(is_owned_contextstream_hook_command)
        })
        .count()
}

fn flat_entry_matches_spec(entry: &Value, spec: JsonHookSpec, windsurf: bool) -> bool {
    let Some(object) = entry.as_object() else {
        return false;
    };
    if object
        .get("command")
        .and_then(Value::as_str)
        .and_then(current_managed_hook_name)
        != Some(spec.hook_name)
    {
        return false;
    }

    if windsurf {
        if !object.keys().all(|key| {
            matches!(
                key.as_str(),
                "command" | "powershell" | "show_output" | "working_directory"
            )
        }) || object.get("show_output").and_then(Value::as_bool) != spec.show_output
        {
            return false;
        }
        if cfg!(windows)
            && object
                .get("powershell")
                .and_then(Value::as_str)
                .and_then(current_managed_hook_name)
                != Some(spec.hook_name)
        {
            return false;
        }
        return true;
    }

    object
        .keys()
        .all(|key| matches!(key.as_str(), "command" | "timeout" | "matcher" | "type"))
        && object.get("type").and_then(Value::as_str) == Some("command")
        && object.get("timeout").and_then(Value::as_u64) == Some(spec.timeout)
        && object.get("matcher").and_then(Value::as_str) == spec.matcher
}

fn validate_flat_hook_specs(
    hooks: &serde_json::Map<String, Value>,
    specs: &[JsonHookSpec],
    windsurf: bool,
) -> std::result::Result<usize, String> {
    for spec in specs {
        let matches = hooks
            .get(spec.event)
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter(|entry| flat_entry_matches_spec(entry, *spec, windsurf))
                    .count()
            })
            .unwrap_or(0);
        if matches != 1 {
            return Err(format!(
                "expected exactly one current managed {} entry for event {}",
                spec.hook_name, spec.event
            ));
        }
    }
    let owned = count_owned_flat_entries(hooks);
    if owned != specs.len() {
        return Err(format!(
            "found {owned} owned hook entries but the current schema requires {}",
            specs.len()
        ));
    }
    Ok(owned)
}

fn validate_claude_hook_specs(
    hooks: &serde_json::Map<String, Value>,
) -> std::result::Result<usize, String> {
    let mut owned = 0;
    for entries in hooks.values().filter_map(Value::as_array) {
        for entry in entries {
            if let Some(commands) = entry.get("hooks").and_then(Value::as_array) {
                owned += commands
                    .iter()
                    .filter(|command| {
                        command
                            .get("command")
                            .and_then(Value::as_str)
                            .is_some_and(is_owned_contextstream_hook_command)
                    })
                    .count();
            }
        }
    }

    for spec in CLAUDE_HOOK_SPECS {
        let matches = hooks
            .get(claude_hook_event_name(spec.event))
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter(|entry| {
                        entry.get("matcher").and_then(Value::as_str) == spec.matcher
                            && entry.get("hooks").and_then(Value::as_array).is_some_and(
                                |commands| {
                                    commands.iter().any(|command| {
                                        command.get("type").and_then(Value::as_str)
                                            == Some("command")
                                            && command.get("timeout").and_then(Value::as_u64)
                                                == Some(spec.timeout)
                                            && command
                                                .get("command")
                                                .and_then(Value::as_str)
                                                .and_then(current_managed_hook_name)
                                                == Some(spec.hook_name)
                                    })
                                },
                            )
                    })
                    .count()
            })
            .unwrap_or(0);
        if matches != 1 {
            return Err(format!(
                "expected exactly one current managed {} entry for event {}",
                spec.hook_name,
                claude_hook_event_name(spec.event)
            ));
        }
    }
    if owned != CLAUDE_HOOK_SPECS.len() {
        return Err(format!(
            "found {owned} owned hook commands but the current schema requires {}",
            CLAUDE_HOOK_SPECS.len()
        ));
    }
    Ok(owned)
}

/// Validate complete current managed-hook coverage without interpreting any
/// unrelated user entries as ContextStream-owned.
pub(super) fn validate_managed_hook_config(
    editor: &Editor,
    value: &Value,
) -> std::result::Result<usize, String> {
    let hooks = value
        .get("hooks")
        .and_then(Value::as_object)
        .ok_or_else(|| "hooks is missing or is not an object".to_string())?;
    match editor {
        Editor::ClaudeCode => validate_claude_hook_specs(hooks),
        Editor::Cursor => validate_flat_hook_specs(hooks, CURSOR_HOOK_SPECS, false),
        Editor::Windsurf => validate_flat_hook_specs(hooks, WINDSURF_HOOK_SPECS, true),
        _ => Err("this editor does not use a JSON hook config".to_string()),
    }
}

/// Validate all expected Cline/Roo wrapper files by exact managed template.
pub(super) fn validate_managed_wrapper_set(hooks_dir: &Path) -> std::result::Result<usize, String> {
    for (base_name, hook_name) in VSCODE_WRAPPER_HOOK_SPECS {
        let path = hooks_dir.join(format!("{}{}", base_name, hook_wrapper_extension()));
        let content = std::fs::read_to_string(&path)
            .map_err(|_| format!("managed wrapper {base_name} is missing or unreadable"))?;
        if !is_contextstream_wrapper(&content, hook_name) {
            return Err(format!(
                "wrapper {base_name} does not match the current managed template"
            ));
        }
    }
    Ok(VSCODE_WRAPPER_HOOK_SPECS.len())
}

/// Restore a byte-for-byte pre-install snapshot only when replaying the
/// current installer against that snapshot reproduces the live file exactly.
///
/// This proves the user made no intervening edit, including whitespace or
/// comments. Otherwise uninstall falls back to a surgical removal.
fn try_restore_exact_hook_backup(
    path: &Path,
    current: &safe_edit::LoadedConfig,
    build_update: impl FnOnce(&safe_edit::LoadedConfig) -> Result<Value>,
) -> Result<bool> {
    let backup_path = safe_edit::backup_path(path)?;
    let Some(backup) =
        safe_edit::read_recovery_for_edit(&backup_path, safe_edit::JsonDialect::Strict)?
    else {
        return Ok(false);
    };
    if contains_owned_hook_command(&backup.value) {
        // A recovery snapshot that already contains managed hooks is not a
        // clean pre-install state and must never be reintroduced.
        return Ok(false);
    }

    let updated = build_update(&backup)?;
    let expected = safe_edit::render(&backup, &updated)?;
    if expected != current.raw {
        return Ok(false);
    }

    safe_edit::restore_first_backup(path, current, &backup.raw)
}

#[derive(Debug, Clone, Copy)]
enum HookConfigFlavor {
    Claude,
    Cursor,
    Windsurf,
}

/// Prove that a hook config contains only fields this installer generates and
/// only ContextStream-owned hook entries. This bounded classification is used
/// solely to clean up files that did not exist before installation.
fn hook_config_is_wholly_managed(
    loaded: &safe_edit::LoadedConfig,
    flavor: HookConfigFlavor,
) -> bool {
    if loaded.nonstandard_syntax {
        return false;
    }
    let Some(root) = loaded.value.as_object() else {
        return false;
    };
    let allowed_top_level = match flavor {
        HookConfigFlavor::Claude | HookConfigFlavor::Windsurf => {
            root.keys().all(|key| key == "hooks")
        }
        HookConfigFlavor::Cursor => {
            root.keys().all(|key| key == "hooks" || key == "version")
                && root.get("version") == Some(&json!(1))
        }
    };
    if !allowed_top_level {
        return false;
    }

    let Some(Value::Object(mut hooks)) = root.get("hooks").cloned() else {
        return false;
    };
    match flavor {
        HookConfigFlavor::Claude => strip_contextstream_hook_entries(&mut hooks),
        HookConfigFlavor::Cursor | HookConfigFlavor::Windsurf => {
            strip_flat_contextstream_hook_entries(&mut hooks)
        }
    }
    hooks.is_empty()
}

fn commit_or_remove_hook_config(
    path: &Path,
    loaded: &safe_edit::LoadedConfig,
    updated: &Value,
    flavor: HookConfigFlavor,
) -> Result<()> {
    let backup_path = safe_edit::backup_path(path)?;
    let backup = safe_edit::read_recovery_for_edit(&backup_path, safe_edit::JsonDialect::Strict)?;
    let current_is_wholly_managed = hook_config_is_wholly_managed(loaded, flavor);
    let backup_is_wholly_managed = backup
        .as_ref()
        .is_some_and(|backup| hook_config_is_wholly_managed(backup, flavor));

    if current_is_wholly_managed && (backup.is_none() || backup_is_wholly_managed) {
        safe_edit::remove_owned_file_if_unchanged(path, &loaded.raw)?;
        if let Some(backup) = backup.filter(|_| backup_is_wholly_managed) {
            safe_edit::remove_owned_file_if_unchanged(&backup_path, &backup.raw)?;
        }
    } else {
        safe_edit::commit(path, loaded, updated)?;
    }
    Ok(())
}

// ============================================================================
// Uninstall
// ============================================================================

/// Remove ContextStream hooks from an editor.
#[allow(dead_code)]
pub fn uninstall_hooks(editor: &Editor) -> Result<()> {
    match editor {
        Editor::ClaudeCode => uninstall_claude_code_hooks(),
        Editor::Cursor => uninstall_cursor_hooks(),
        Editor::Windsurf => uninstall_windsurf_hooks(),
        Editor::Cline | Editor::RooCode => uninstall_vscode_extension_hooks(editor),
        _ => Ok(()),
    }
}

fn uninstall_vscode_extension_hooks(editor: &Editor) -> Result<()> {
    let Some(hooks_dir) = vscode_extension_hooks_dir(editor) else {
        return Ok(());
    };
    if !hooks_dir.exists() {
        return Ok(());
    }

    let mut removed_any = false;
    for (base_name, hook_name) in VSCODE_WRAPPER_HOOK_SPECS {
        let wrapper = hooks_dir.join(format!("{}{}", base_name, hook_wrapper_extension()));
        if wrapper.exists() {
            let content = std::fs::read_to_string(&wrapper)
                .with_context(|| format!("Could not inspect hook {}", wrapper.display()))?;
            if is_contextstream_wrapper(&content, hook_name) {
                let backup = safe_edit::backup_path(&wrapper)?;
                let backup_content = safe_edit::read_recovery_file(&backup)?;
                if backup_content
                    .as_deref()
                    .is_some_and(|content| !is_contextstream_wrapper(content, hook_name))
                {
                    anyhow::bail!(
                        "Refusing to remove {} because recovery backup {} is not a recognized ContextStream wrapper",
                        wrapper.display(),
                        backup.display()
                    );
                }
                safe_edit::remove_owned_file_if_unchanged(&wrapper, &content)?;
                if let Some(backup_content) = backup_content {
                    safe_edit::remove_owned_file_if_unchanged(&backup, &backup_content)?;
                }
                removed_any = true;
            }
        }
    }

    if removed_any && !safe_edit::is_dry_run() {
        let mut entries = std::fs::read_dir(&hooks_dir)?;
        if entries.next().is_none() {
            let _ = std::fs::remove_dir(&hooks_dir);
        }
    }

    Ok(())
}

/// Remove ContextStream hooks from Claude Code settings.json.
fn uninstall_claude_code_hooks() -> Result<()> {
    let settings_path = match claude_code_settings_path() {
        Some(p) => p,
        None => return Ok(()),
    };

    if !settings_path.exists() {
        return Ok(());
    }

    let loaded = safe_edit::read_for_edit(&settings_path, safe_edit::JsonDialect::Strict)?;
    if try_restore_exact_hook_backup(&settings_path, &loaded, |backup| {
        build_claude_hooks_update(backup, &settings_path, None)
    })? {
        return Ok(());
    }

    let Some(Value::Object(mut hooks)) = loaded.value.get("hooks").cloned() else {
        return Ok(());
    };

    strip_contextstream_hook_entries(&mut hooks);

    let mut updated = loaded.value.clone();
    updated["hooks"] = Value::Object(hooks);
    commit_or_remove_hook_config(&settings_path, &loaded, &updated, HookConfigFlavor::Claude)?;

    Ok(())
}

/// Remove ContextStream hooks from Cursor.
fn uninstall_cursor_hooks() -> Result<()> {
    let hooks_path = match cursor_hooks_path() {
        Some(p) => p,
        None => return Ok(()),
    };

    if !hooks_path.exists() {
        return Ok(());
    }

    let loaded = safe_edit::read_for_edit(&hooks_path, safe_edit::JsonDialect::Strict)?;
    if try_restore_exact_hook_backup(&hooks_path, &loaded, build_cursor_hooks_update)? {
        return Ok(());
    }
    let mut config: Value = loaded.value.clone();

    let Some(hooks) = config.get_mut("hooks").and_then(Value::as_object_mut) else {
        return Ok(());
    };

    // Remove ContextStream entries from ALL hook types, not just specific ones.
    strip_flat_contextstream_hook_entries(hooks);

    let has_any_hooks = hooks.values().any(|value| {
        value
            .as_array()
            .map(|items| !items.is_empty())
            .unwrap_or(true)
    });

    if !has_any_hooks {
        config["hooks"] = json!({});
    }
    commit_or_remove_hook_config(&hooks_path, &loaded, &config, HookConfigFlavor::Cursor)?;

    Ok(())
}

/// Remove ContextStream hooks from Windsurf.
fn uninstall_windsurf_hooks() -> Result<()> {
    let hooks_path = match windsurf_hooks_path() {
        Some(p) => p,
        None => return Ok(()),
    };

    if !hooks_path.exists() {
        return Ok(());
    }

    let loaded = safe_edit::read_for_edit(&hooks_path, safe_edit::JsonDialect::Strict)?;
    if try_restore_exact_hook_backup(&hooks_path, &loaded, build_windsurf_hooks_update)? {
        return Ok(());
    }
    let mut config: Value = loaded.value.clone();

    let Some(hooks) = config.get_mut("hooks").and_then(Value::as_object_mut) else {
        return Ok(());
    };

    strip_flat_contextstream_hook_entries(hooks);

    let has_any_hooks = hooks.values().any(|value| {
        value
            .as_array()
            .map(|items| !items.is_empty())
            .unwrap_or(true)
    });

    if !has_any_hooks {
        config["hooks"] = json!({});
    }
    commit_or_remove_hook_config(&hooks_path, &loaded, &config, HookConfigFlavor::Windsurf)?;

    Ok(())
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_content_comparison_is_streaming_and_exact() {
        let temp = tempfile::tempdir().expect("tempdir");
        let source = temp.path().join("source");
        let target = temp.path().join("target");
        let mut bytes = vec![0x5a; 128 * 1024 + 7];
        bytes[64 * 1024 - 1] = 0x11;
        bytes[64 * 1024] = 0x22;
        std::fs::write(&source, &bytes).expect("write source");
        std::fs::write(&target, &bytes).expect("write target");

        assert!(files_have_identical_contents(&source, &target).expect("compare identical"));

        bytes[64 * 1024] ^= 0xff;
        std::fs::write(&target, &bytes).expect("rewrite target");
        assert!(!files_have_identical_contents(&source, &target).expect("compare changed"));

        bytes.push(0);
        std::fs::write(&target, &bytes).expect("rewrite longer target");
        assert!(!files_have_identical_contents(&source, &target).expect("compare length"));
    }

    /// Cascade hook events documented at docs.devin.ai/desktop/cascade/hooks
    /// (formerly docs.windsurf.com). If Windsurf renames or removes events,
    /// this list is the tripwire to update alongside WINDSURF_HOOK_SPECS.
    const WINDSURF_DOCUMENTED_EVENTS: &[&str] = &[
        "pre_read_code",
        "post_read_code",
        "pre_write_code",
        "post_write_code",
        "pre_run_command",
        "post_run_command",
        "pre_mcp_tool_use",
        "post_mcp_tool_use",
        "pre_user_prompt",
        "post_cascade_response",
        "post_cascade_response_with_transcript",
        "post_setup_worktree",
    ];

    /// Events ContextStream enforcement depends on in Windsurf: first-call
    /// blocking (exit code 2) + prompt grounding + capture + transcripts.
    const WINDSURF_ENFORCEMENT_CRITICAL_EVENTS: &[&str] = &[
        "pre_mcp_tool_use",
        "pre_user_prompt",
        "post_mcp_tool_use",
        "post_cascade_response_with_transcript",
    ];

    #[test]
    fn windsurf_hook_specs_use_documented_events_only() {
        for spec in WINDSURF_HOOK_SPECS {
            assert!(
                WINDSURF_DOCUMENTED_EVENTS.contains(&spec.event),
                "WINDSURF_HOOK_SPECS event '{}' is not in the documented Cascade event list",
                spec.event
            );
        }
    }

    #[test]
    fn windsurf_enforcement_critical_events_are_installed() {
        for event in WINDSURF_ENFORCEMENT_CRITICAL_EVENTS {
            assert!(
                WINDSURF_HOOK_SPECS.iter().any(|spec| spec.event == *event),
                "enforcement-critical Cascade event '{}' missing from WINDSURF_HOOK_SPECS",
                event
            );
        }
    }

    #[test]
    fn windsurf_hook_entries_match_cascade_schema() {
        let hooks = generate_windsurf_hooks("/usr/local/bin/contextstream-mcp");
        assert!(!hooks.is_empty());
        for (event, entries) in &hooks {
            for entry in entries {
                let obj = entry.as_object().expect("hook entry must be an object");
                for key in obj.keys() {
                    assert!(
                        matches!(
                            key.as_str(),
                            "command" | "powershell" | "show_output" | "working_directory"
                        ),
                        "undocumented key '{}' in Windsurf '{}' entry — Cascade schema \
                         only defines command/powershell/show_output/working_directory",
                        key,
                        event
                    );
                }
                let command = obj
                    .get("command")
                    .and_then(Value::as_str)
                    .expect("command is required");
                assert!(command.contains("contextstream-mcp"));
                assert!(command.contains(" hook "));
                // Cursor's shape must not leak into Windsurf entries.
                assert!(obj.get("type").is_none());
                assert!(obj.get("timeout").is_none());
                assert!(obj.get("matcher").is_none());
                if cfg!(target_os = "windows") {
                    assert!(
                        obj.get("powershell").is_some(),
                        "Windows entries need the powershell command variant"
                    );
                }
            }
        }
    }

    #[test]
    fn test_generate_contextstream_hooks() {
        let hooks = generate_contextstream_hooks(Some("test-key"));

        assert!(!hooks.pre_tool_use.is_empty());
        assert!(!hooks.post_tool_use.is_empty());
        assert!(!hooks.post_tool_use_failure.is_empty());
        assert!(!hooks.instructions_loaded.is_empty());
        assert!(!hooks.user_prompt_submit.is_empty());
        assert!(!hooks.session_start.is_empty());
        assert!(!hooks.stop.is_empty());
        assert!(!hooks.stop_failure.is_empty());
        assert!(!hooks.session_end.is_empty());
        assert!(!hooks.pre_compact.is_empty());
        assert!(!hooks.post_compact.is_empty());
        assert!(!hooks.subagent_start.is_empty());
        assert!(!hooks.subagent_stop.is_empty());
        assert!(!hooks.task_created.is_empty());
        assert!(!hooks.task_completed.is_empty());
        assert!(!hooks.teammate_idle.is_empty());
        assert!(!hooks.notification.is_empty());
        assert!(!hooks.permission_request.is_empty());
        assert!(!hooks.config_change.is_empty());
        assert!(!hooks.cwd_changed.is_empty());
        assert!(!hooks.file_changed.is_empty());
        assert!(!hooks.worktree_create.is_empty());
        assert!(!hooks.worktree_remove.is_empty());
        assert!(!hooks.elicitation.is_empty());
        assert!(!hooks.elicitation_result.is_empty());
        assert!(!hooks.user_prompt_submit[0].hooks.is_empty());
    }

    #[test]
    fn test_claude_enforcement_critical_events_are_present() {
        for event in CLAUDE_ENFORCEMENT_CRITICAL_EVENTS {
            assert!(
                CLAUDE_HOOK_SPECS.iter().any(|spec| spec.event == *event),
                "missing critical event in CLAUDE_HOOK_SPECS"
            );
        }
    }

    #[test]
    fn test_subagent_start_hook_generated() {
        let hooks = generate_contextstream_hooks(None);

        assert_eq!(hooks.subagent_start.len(), 1);
        let entry = &hooks.subagent_start[0];
        assert_eq!(
            entry.matcher.as_deref(),
            Some("Explore|Plan|general-purpose|custom")
        );
        assert_eq!(entry.hooks.len(), 1);
        assert_eq!(entry.hooks[0].command_type, "command");
        assert!(entry.hooks[0].command.contains("subagent-start"));
        assert_eq!(entry.hooks[0].timeout, Some(10));
    }

    #[test]
    fn test_new_format_uses_command_type() {
        let hooks = generate_contextstream_hooks(None);

        // All hooks should use "command" type, not "run"
        for entry in &hooks.pre_tool_use {
            for cmd in &entry.hooks {
                assert_eq!(cmd.command_type, "command");
            }
        }
    }

    #[test]
    fn test_new_format_timeout_in_seconds() {
        let hooks = generate_contextstream_hooks(None);

        // PreToolUse should be 5 seconds, not 5000 milliseconds
        let entry = &hooks.pre_tool_use[0];
        assert_eq!(entry.hooks[0].timeout, Some(5));
    }

    #[test]
    fn test_new_format_uses_matcher_string() {
        let hooks = generate_contextstream_hooks(None);
        let json = serde_json::to_string_pretty(&hooks).unwrap();

        // Should use "matcher" not "match"
        assert!(json.contains("\"matcher\""));
        assert!(!json.contains("\"match\""));
        // Should use "hooks" array not "commands"
        // (The field is "hooks" inside each entry)
        assert!(json.contains("\"type\": \"command\""));
        assert!(!json.contains("\"type\": \"run\""));
    }

    #[test]
    fn test_is_contextstream_hook() {
        let hook = HookEntry {
            matcher: None,
            hooks: vec![HookCommand {
                command_type: "command".to_string(),
                command: format!(
                    "contextstream-mcp hook session-start {}",
                    MANAGED_HOOK_ARGUMENT
                ),
                timeout: Some(10),
            }],
        };

        assert!(is_contextstream_hook(&hook));
    }

    #[test]
    fn test_is_not_contextstream_hook() {
        let hook = HookEntry {
            matcher: None,
            hooks: vec![HookCommand {
                command_type: "command".to_string(),
                command: "echo user hook".to_string(),
                timeout: None,
            }],
        };

        assert!(!is_contextstream_hook(&hook));
    }

    #[test]
    fn user_script_with_contextstream_in_its_name_is_not_owned() {
        for command in [
            "/Users/me/bin/my-contextstream-sync.sh",
            "\"/Users/me/bin/my-contextstream-sync.sh\" --upload",
            "bash /Users/me/bin/contextstream-notes.sh",
        ] {
            assert!(
                !is_contextstream_hook_command(command),
                "misclassified user command: {command}"
            );
        }
    }

    #[test]
    fn only_explicitly_marked_hook_commands_are_owned() {
        for command in [
            "contextstream-mcp hook session-start --contextstream-managed-hook=v1",
            "\"/Users/me/.contextstream/bin/contextstream-mcp\" hook pre-tool-use --contextstream-managed-hook=v1",
            "\"C:\\\\Users\\\\me\\\\contextstream-mcp.exe\" hook post-tool-use --contextstream-managed-hook=v1",
        ] {
            assert!(
                is_contextstream_hook_command(command),
                "missed managed command: {command}"
            );
        }
        assert!(!is_contextstream_hook_command(
            "contextstream-mcp hook session-start && echo user"
        ));
        assert!(!is_contextstream_hook_command(
            "contextstream-mcp hook session-start"
        ));
    }

    #[test]
    fn legacy_unmarked_hook_requires_the_managed_binary_path() {
        // managed_binary_path() resolves through $HOME, so this must serialize
        // with the tests that swap HOME out from under it.
        let _env_guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        let managed = managed_binary_path();
        let managed_command = format!("\"{}\" hook session-start", managed.display());

        assert!(is_legacy_managed_hook_command(&managed_command));
        assert!(!is_legacy_managed_hook_command(
            "\"/opt/contextstream-mcp\" hook session-start"
        ));
    }

    #[test]
    fn stripping_a_mixed_matcher_group_keeps_user_commands() {
        let mut hooks = serde_json::json!({
            "SessionStart": [{
                "hooks": [
                    {"type": "command", "command": "contextstream-mcp hook session-start --contextstream-managed-hook=v1"},
                    {"type": "command", "command": "echo user hook"}
                ]
            }]
        })
        .as_object()
        .unwrap()
        .clone();

        strip_contextstream_hook_entries(&mut hooks);

        let commands = hooks["SessionStart"][0]["hooks"].as_array().unwrap();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0]["command"], "echo user hook");
    }

    #[test]
    fn test_hooks_serialization_new_format() {
        let hooks = generate_contextstream_hooks(None);
        let json = serde_json::to_string_pretty(&hooks).unwrap();

        assert!(json.contains("\"PreToolUse\""));
        assert!(json.contains("\"PostToolUse\""));
        assert!(json.contains("\"PostToolUseFailure\""));
        assert!(json.contains("\"InstructionsLoaded\""));
        assert!(json.contains("\"SessionStart\""));
        assert!(json.contains("\"Stop\""));
        assert!(json.contains("\"StopFailure\""));
        assert!(json.contains("\"SessionEnd\""));
        assert!(json.contains("\"PostCompact\""));
        assert!(json.contains("\"SubagentStart\""));
        assert!(json.contains("\"SubagentStop\""));
        assert!(json.contains("\"TaskCreated\""));
        assert!(json.contains("\"TaskCompleted\""));
        assert!(json.contains("\"TeammateIdle\""));
        assert!(json.contains("\"Notification\""));
        assert!(json.contains("\"PermissionRequest\""));
        assert!(json.contains("\"ConfigChange\""));
        assert!(json.contains("\"CwdChanged\""));
        assert!(json.contains("\"FileChanged\""));
        assert!(json.contains("\"WorktreeCreate\""));
        assert!(json.contains("\"WorktreeRemove\""));
        assert!(json.contains("\"Elicitation\""));
        assert!(json.contains("\"ElicitationResult\""));
    }

    fn merged_hooks_object(existing: Value) -> serde_json::Map<String, Value> {
        let mut hooks = match existing {
            Value::Object(map) => map,
            _ => serde_json::Map::new(),
        };
        merge_contextstream_hooks_into(&mut hooks, generate_contextstream_hooks(None))
            .expect("merge hooks");
        hooks
    }

    fn event_commands(hooks: &serde_json::Map<String, Value>, event: &str) -> Vec<String> {
        hooks
            .get(event)
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter_map(|entry| entry.get("hooks").and_then(Value::as_array))
                    .flatten()
                    .filter_map(|cmd| cmd.get("command").and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn test_merge_hooks_preserves_user_hooks() {
        let hooks = merged_hooks_object(json!({
            "PreToolUse": [{
                "matcher": "MyTool",
                "hooks": [{ "type": "command", "command": "echo user hook" }]
            }]
        }));

        let commands = event_commands(&hooks, "PreToolUse");
        assert!(commands.len() >= 2);
        assert!(commands.iter().any(|c| c == "echo user hook"));
    }

    #[test]
    fn test_merge_hooks_includes_subagent_start() {
        let hooks = merged_hooks_object(json!({}));

        for event in [
            "SubagentStart",
            "SubagentStop",
            "TaskCreated",
            "TaskCompleted",
            "TeammateIdle",
            "PostToolUseFailure",
            "InstructionsLoaded",
            "StopFailure",
            "PostCompact",
            "ConfigChange",
            "CwdChanged",
            "FileChanged",
            "WorktreeCreate",
            "WorktreeRemove",
            "Elicitation",
            "ElicitationResult",
        ] {
            assert!(
                hooks
                    .get(event)
                    .and_then(Value::as_array)
                    .is_some_and(|a| !a.is_empty()),
                "expected generated hooks for {event}"
            );
        }
    }

    #[test]
    fn managed_hook_validation_requires_complete_current_coverage() {
        let claude = json!({
            "hooks": serde_json::to_value(generate_contextstream_hooks(None)).unwrap()
        });
        assert_eq!(
            validate_managed_hook_config(&Editor::ClaudeCode, &claude).unwrap(),
            CLAUDE_HOOK_SPECS.len()
        );

        let mut cursor = json!({
            "version": 1,
            "hooks": generate_cursor_hooks("contextstream-mcp")
        });
        assert_eq!(
            validate_managed_hook_config(&Editor::Cursor, &cursor).unwrap(),
            CURSOR_HOOK_SPECS.len()
        );
        cursor["hooks"]["preToolUse"]
            .as_array_mut()
            .unwrap()
            .clear();
        assert!(validate_managed_hook_config(&Editor::Cursor, &cursor).is_err());

        let mut windsurf = json!({
            "hooks": generate_windsurf_hooks("contextstream-mcp")
        });
        windsurf["hooks"]["user_event"] = json!([{
            "command": "/Users/me/bin/my-contextstream-sync.sh",
            "unexpected_user_field": true
        }]);
        assert_eq!(
            validate_managed_hook_config(&Editor::Windsurf, &windsurf).unwrap(),
            WINDSURF_HOOK_SPECS.len()
        );
        windsurf["hooks"]["user_event"] = json!([{
            "command": "contextstream-mcp hook unknown --contextstream-managed-hook=v1"
        }]);
        assert!(validate_managed_hook_config(&Editor::Windsurf, &windsurf).is_err());
    }

    #[test]
    fn managed_wrapper_validation_requires_every_exact_wrapper() {
        let temp = tempfile::tempdir().unwrap();
        for (base_name, hook_name) in VSCODE_WRAPPER_HOOK_SPECS {
            let path = temp
                .path()
                .join(format!("{}{}", base_name, hook_wrapper_extension()));
            std::fs::write(
                path,
                hook_wrapper_script_content("contextstream-mcp", hook_name),
            )
            .unwrap();
        }
        assert_eq!(
            validate_managed_wrapper_set(temp.path()).unwrap(),
            VSCODE_WRAPPER_HOOK_SPECS.len()
        );

        let missing = temp.path().join(format!(
            "{}{}",
            VSCODE_WRAPPER_HOOK_SPECS[0].0,
            hook_wrapper_extension()
        ));
        std::fs::remove_file(missing).unwrap();
        assert!(validate_managed_wrapper_set(temp.path()).is_err());
    }

    #[test]
    fn test_merge_preserves_unknown_hook_events() {
        let hooks = merged_hooks_object(json!({
            "SomeFutureEvent": [{
                "matcher": "*",
                "hooks": [{ "type": "command", "command": "echo future" }]
            }]
        }));

        assert_eq!(
            event_commands(&hooks, "SomeFutureEvent"),
            vec!["echo future".to_string()],
            "unknown hook events must survive a merge"
        );
    }

    #[test]
    fn test_merge_preserves_unparseable_user_entries() {
        // `timeout` as a string does not match HookCommand. The entry is not
        // ours, so it must be preserved verbatim rather than dropped.
        let hooks = merged_hooks_object(json!({
            "PreToolUse": [{
                "matcher": "Bash",
                "hooks": [{ "type": "command", "command": "echo odd", "timeout": "30" }]
            }]
        }));

        let raw = serde_json::to_string(hooks.get("PreToolUse").unwrap()).unwrap();
        assert!(
            raw.contains("echo odd"),
            "unparseable entry was dropped: {raw}"
        );
    }

    #[test]
    fn merge_preserves_unknown_fields_on_parseable_user_hook_groups_and_commands() {
        let hooks = merged_hooks_object(json!({
            "PreToolUse": [{
                "matcher": "MyTool",
                "future_group_field": {"keep": true},
                "hooks": [
                    {
                        "type": "command",
                        "command": "contextstream-mcp hook pre-tool-use --contextstream-managed-hook=v1",
                        "managed_future_field": "may disappear with the managed command"
                    },
                    {
                        "type": "command",
                        "command": "echo user hook",
                        "timeout": 30,
                        "future_command_field": {"keep": true}
                    }
                ]
            }]
        }));

        let user_group = hooks["PreToolUse"]
            .as_array()
            .unwrap()
            .iter()
            .find(|group| group["matcher"] == "MyTool")
            .expect("user group");
        assert_eq!(user_group["future_group_field"], json!({"keep": true}));
        let user_command = user_group["hooks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|command| command["command"] == "echo user hook")
            .expect("user command");
        assert_eq!(user_command["future_command_field"], json!({"keep": true}));
    }

    #[test]
    fn hook_schema_conflicts_fail_closed_and_unknown_flat_values_survive_filtering() {
        let mut claude = json!({
            "PreToolUse": {"future_schema": true}
        })
        .as_object()
        .unwrap()
        .clone();
        assert!(
            merge_contextstream_hooks_into(&mut claude, generate_contextstream_hooks(None))
                .is_err()
        );

        let mut flat = json!({
            "preToolUse": {"future_schema": true},
            "emptyUserEvent": [],
            "ownedOnly": [{
                "command": "contextstream-mcp hook pre-tool-use --contextstream-managed-hook=v1"
            }]
        })
        .as_object()
        .unwrap()
        .clone();
        strip_flat_contextstream_hook_entries(&mut flat);
        assert_eq!(flat["preToolUse"], json!({"future_schema": true}));
        assert_eq!(flat["emptyUserEvent"], json!([]));
        assert!(!flat.contains_key("ownedOnly"));

        assert!(merge_flat_hook_entries(
            &mut flat,
            HashMap::from([("preToolUse".to_string(), vec![json!({"command": "new"})])]),
            "Cursor",
        )
        .is_err());
    }

    #[test]
    fn test_reinstall_does_not_duplicate_contextstream_hooks() {
        let once = merged_hooks_object(json!({}));
        let twice = merged_hooks_object(Value::Object(once.clone()));

        assert_eq!(
            event_commands(&once, "PreToolUse").len(),
            event_commands(&twice, "PreToolUse").len(),
            "re-running install must not stack duplicate hooks"
        );
    }

    #[test]
    fn test_read_settings_refuses_invalid_json() {
        let dir = std::env::temp_dir().join(format!("cs-hooks-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings-invalid.json");
        std::fs::write(&path, "{ not json at all ").unwrap();

        let err = safe_edit::read_for_edit(&path, safe_edit::JsonDialect::Strict)
            .expect_err("must refuse invalid JSON");
        assert!(err.to_string().contains("Refusing to modify"));
        // The unreadable file must be left exactly as it was.
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{ not json at all "
        );

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn test_jsonc_settings_keep_their_comments_through_an_install() {
        let dir = std::env::temp_dir().join(format!("cs-hooks-jsonc-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings-jsonc.json");
        std::fs::write(&path, "{\n  // a comment\n  \"model\": \"opus\"\n}").unwrap();

        let loaded = safe_edit::read_for_edit(&path, safe_edit::JsonDialect::Strict).unwrap();
        assert!(
            loaded.nonstandard_syntax,
            "comments in a strict-dialect file should be flagged"
        );

        let mut hooks = hooks_object_of(&loaded, &path).unwrap();
        merge_contextstream_hooks_into(&mut hooks, generate_contextstream_hooks(None)).unwrap();
        let mut updated = loaded.value.clone();
        updated["hooks"] = Value::Object(hooks);
        safe_edit::commit(&path, &loaded, &updated).unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(
            after.contains("// a comment"),
            "comment was stripped: {after}"
        );
        assert!(after.contains("\"model\": \"opus\""));
        assert!(after.contains("contextstream"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_atomic_write_leaves_backup() {
        let dir = std::env::temp_dir().join(format!("cs-hooks-bak-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, "{\"model\":\"opus\"}").unwrap();

        safe_edit::write_if_changed(&path, "{\"model\":\"sonnet\"}").unwrap();

        let backup = dir.join("settings.json.contextstream.bak");
        assert!(backup.exists(), "backup was not created");
        assert!(std::fs::read_to_string(&backup).unwrap().contains("opus"));
        assert!(std::fs::read_to_string(&path).unwrap().contains("sonnet"));
        assert!(!dir.join("settings.json.contextstream.tmp").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn test_install_preserves_unrelated_settings_keys() {
        let mut settings = serde_json::Map::new();
        settings.insert("model".to_string(), json!("opus"));
        settings.insert("permissions".to_string(), json!({ "allow": ["Bash"] }));

        let mut hooks = serde_json::Map::new();
        merge_contextstream_hooks_into(&mut hooks, generate_contextstream_hooks(None)).unwrap();
        settings.insert("hooks".to_string(), Value::Object(hooks));

        assert_eq!(settings.get("model").unwrap(), &json!("opus"));
        assert_eq!(
            settings.get("permissions").unwrap(),
            &json!({ "allow": ["Bash"] })
        );
    }

    #[test]
    fn test_session_start_matcher_includes_compact() {
        let hooks = generate_contextstream_hooks(None);
        let entry = &hooks.session_start[0];
        assert!(entry.matcher.as_deref().unwrap_or("").contains("compact"));
        assert_eq!(entry.hooks[0].timeout, Some(15));
    }

    #[test]
    fn test_pre_compact_has_no_matcher() {
        let hooks = generate_contextstream_hooks(None);
        let entry = &hooks.pre_compact[0];
        assert!(
            entry.matcher.is_none(),
            "PreCompact should run for every compaction event"
        );
    }

    #[test]
    fn test_stop_and_session_end_commands() {
        let hooks = generate_contextstream_hooks(None);
        assert!(hooks.stop[0].hooks[0].command.contains("hook stop"));
        assert!(hooks.stop_failure[0].hooks[0]
            .command
            .contains("hook stop-failure"));
        assert!(hooks.session_end[0].hooks[0]
            .command
            .contains("hook session-end"));
    }

    #[test]
    fn test_user_prompt_submit_includes_save_intent_hook() {
        let hooks = generate_contextstream_hooks(None);
        let commands: Vec<&str> = hooks
            .user_prompt_submit
            .iter()
            .flat_map(|entry| entry.hooks.iter().map(|hook| hook.command.as_str()))
            .collect();

        assert!(commands
            .iter()
            .any(|cmd| cmd.contains("user-prompt-submit")));
        assert!(commands.iter().any(|cmd| cmd.contains("on-save-intent")));
    }

    #[test]
    fn test_generate_cursor_hooks_events() {
        let hooks = generate_cursor_hooks("contextstream-mcp");

        let pre_tool_use = hooks.get("preToolUse").unwrap();
        let before_submit_prompt = hooks.get("beforeSubmitPrompt").unwrap();

        assert_eq!(pre_tool_use.len(), 1);
        // `beforeSubmitPrompt` is gate-only now: a single continue/mark hook,
        // no inert `on-save-intent` (Cursor cannot inject there).
        assert_eq!(before_submit_prompt.len(), 1);
        assert!(!before_submit_prompt
            .iter()
            .any(|entry| entry.to_string().contains("on-save-intent")));
        // `postToolUse` is the injectable event that carries additional_context.
        let post_tool_use = hooks.get("postToolUse").unwrap();
        assert!(post_tool_use
            .iter()
            .any(|entry| entry.to_string().contains("post-tool-use")));
        assert!(hooks.contains_key("beforeMCPExecution"));
        assert!(hooks.contains_key("afterMCPExecution"));
        assert!(hooks.contains_key("beforeShellExecution"));
        assert!(hooks.contains_key("afterShellExecution"));
        assert!(hooks.contains_key("beforeReadFile"));
        assert!(hooks.contains_key("afterFileEdit"));
        assert!(hooks.contains_key("preCompact"));
    }

    #[test]
    fn test_generate_cursor_hooks_matcher_is_string() {
        let hooks = generate_cursor_hooks("contextstream-mcp");
        let pre_tool_use = hooks.get("preToolUse").unwrap();
        let matcher = &pre_tool_use[0]["matcher"];
        assert!(
            matcher.is_string(),
            "matcher must be a string, not an object. Got: {}",
            matcher
        );
    }

    #[test]
    fn test_generate_cursor_hooks_matcher_includes_cursor_tools() {
        let hooks = generate_cursor_hooks("contextstream-mcp");
        let pre_tool_use = hooks.get("preToolUse").unwrap();
        let matcher = pre_tool_use[0]["matcher"].as_str().unwrap();
        assert_eq!(matcher, "*");
    }

    #[test]
    fn test_filter_cursor_hooks_removes_contextstream_entries() {
        let existing = serde_json::json!([
            { "command": "contextstream-mcp hook pre-tool-use --contextstream-managed-hook=v1" },
            { "command": "echo custom hook" }
        ]);

        let filtered = filter_cursor_hooks(existing);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["command"], "echo custom hook");
    }

    #[test]
    fn cursor_filter_preserves_unmarked_user_commands() {
        let existing = serde_json::json!([
            { "command": "/Users/me/bin/my-contextstream-sync.sh" },
            { "command": "contextstream-mcp hook pre-tool-use" }
        ]);

        let filtered = filter_cursor_hooks(existing);
        assert_eq!(filtered.len(), 2);
        assert_eq!(
            filtered[0]["command"],
            "/Users/me/bin/my-contextstream-sync.sh"
        );
        assert_eq!(
            filtered[1]["command"],
            "contextstream-mcp hook pre-tool-use"
        );
    }

    #[test]
    fn test_filter_cursor_hooks_cleans_all_hook_types() {
        // Simulate a Cursor hooks.json with ContextStream entries in multiple hook types
        let config = json!({
            "version": 1,
            "hooks": {
                "preToolUse": [
                    { "command": "contextstream-mcp hook pre-tool-use --contextstream-managed-hook=v1", "type": "command" },
                    { "command": "echo user-hook", "type": "command" }
                ],
                "beforeSubmitPrompt": [
                    { "command": "contextstream-mcp hook user-prompt-submit --contextstream-managed-hook=v1", "type": "command" }
                ],
                "postToolUse": [
                    { "command": "contextstream-mcp hook post-tool-use --contextstream-managed-hook=v1", "type": "command" },
                    { "command": "echo user-post-hook", "type": "command" }
                ],
                "someOtherType": [
                    { "command": "contextstream-mcp hook old-thing --contextstream-managed-hook=v1", "type": "command" }
                ]
            }
        });

        let hooks = config.get("hooks").unwrap().as_object().unwrap();

        // Verify ContextStream entries exist in all types before filtering
        for (_key, value) in hooks {
            let arr = value.as_array().unwrap();
            assert!(
                arr.iter().any(is_contextstream_cursor_hook),
                "expected ContextStream hook in each type"
            );
        }

        // Simulate the cleanup loop from install_cursor_hooks.
        let mut hooks_mut = hooks.clone();
        strip_flat_contextstream_hook_entries(&mut hooks_mut);

        // preToolUse should still have the user hook
        assert_eq!(
            hooks_mut
                .get("preToolUse")
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            hooks_mut.get("preToolUse").unwrap()[0]["command"],
            "echo user-hook"
        );

        // postToolUse should still have the user hook
        assert_eq!(
            hooks_mut
                .get("postToolUse")
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            hooks_mut.get("postToolUse").unwrap()[0]["command"],
            "echo user-post-hook"
        );

        // beforeSubmitPrompt and someOtherType had only CS entries — should be removed
        assert!(hooks_mut.get("beforeSubmitPrompt").is_none());
        assert!(hooks_mut.get("someOtherType").is_none());
    }

    #[test]
    fn test_generate_cursor_hooks_returns_hashmap() {
        let hooks = generate_cursor_hooks("contextstream-mcp");
        assert!(
            hooks.contains_key("preToolUse"),
            "must contain preToolUse key"
        );
        assert!(
            hooks.contains_key("beforeSubmitPrompt"),
            "must contain beforeSubmitPrompt key"
        );
        assert!(
            hooks.contains_key("beforeMCPExecution"),
            "must contain beforeMCPExecution key"
        );
        assert!(
            hooks.contains_key("afterFileEdit"),
            "must contain afterFileEdit key"
        );
    }

    #[test]
    fn test_cursor_enforcement_critical_events_are_present() {
        let hooks = generate_cursor_hooks("contextstream-mcp");
        for event_name in CURSOR_ENFORCEMENT_CRITICAL_EVENTS {
            assert!(
                hooks.contains_key(*event_name),
                "missing critical Cursor event {}",
                event_name
            );
        }

        assert!(hooks["beforeSubmitPrompt"]
            .iter()
            .any(|entry| entry.to_string().contains("user-prompt-submit")));
        assert!(hooks["preToolUse"]
            .iter()
            .any(|entry| entry.to_string().contains("pre-tool-use")));
        assert!(hooks["beforeMCPExecution"]
            .iter()
            .any(|entry| entry.to_string().contains("pre-tool-use")));
    }

    #[test]
    fn test_generate_windsurf_hooks_returns_expected_keys() {
        let hooks = generate_windsurf_hooks("contextstream-mcp");
        assert!(hooks.contains_key("pre_mcp_tool_use"));
        assert!(hooks.contains_key("pre_user_prompt"));
        assert!(hooks.contains_key("pre_read_code"));
        assert!(hooks.contains_key("pre_write_code"));
        assert!(hooks.contains_key("pre_run_command"));
        assert!(hooks.contains_key("post_write_code"));
        assert!(hooks.contains_key("post_mcp_tool_use"));
        assert!(hooks.contains_key("post_cascade_response_with_transcript"));

        let pre_tool_use = hooks.get("pre_mcp_tool_use").unwrap();
        let pre_user_prompt = hooks.get("pre_user_prompt").unwrap();
        let post_transcript = hooks.get("post_cascade_response_with_transcript").unwrap();

        assert_eq!(pre_tool_use.len(), 1);
        assert_eq!(pre_user_prompt.len(), 1);
        assert_eq!(post_transcript.len(), 1);
        assert_eq!(
            pre_tool_use[0]["command"],
            "\"contextstream-mcp\" hook pre-tool-use --contextstream-managed-hook=v1"
        );
        assert_eq!(
            pre_user_prompt[0]["command"],
            "\"contextstream-mcp\" hook user-prompt-submit --contextstream-managed-hook=v1"
        );
        assert_eq!(
            post_transcript[0]["command"],
            "\"contextstream-mcp\" hook session-end --contextstream-managed-hook=v1"
        );
    }

    #[test]
    fn test_filter_windsurf_hooks_removes_contextstream_entries() {
        let existing = serde_json::json!([
            { "command": "contextstream-mcp hook pre-tool-use --contextstream-managed-hook=v1" },
            { "command": "echo custom hook" }
        ]);

        let filtered = filter_windsurf_hooks(existing);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["command"], "echo custom hook");
    }

    #[test]
    fn test_hook_wrapper_script_contains_hook_command() {
        let content =
            hook_wrapper_script_content("/usr/local/bin/contextstream-mcp", "pre-tool-use");
        assert!(content.contains("pre-tool-use"));
        assert!(content.contains("contextstream-mcp"));
        assert!(content.contains(MANAGED_HOOK_ARGUMENT));
        assert!(is_contextstream_wrapper(&content, "pre-tool-use"));
        assert!(!is_contextstream_wrapper(
            &format!("{content}# user customization\n"),
            "pre-tool-use"
        ));
        assert!(!is_contextstream_wrapper(&content, "session-start"));
    }

    #[test]
    fn legacy_wrapper_migration_requires_the_contextstream_executable_name() {
        let product_wrapper = "#!/bin/bash\n# ContextStream hook wrapper for pre-tool-use\nexec \"/usr/local/bin/contextstream-mcp\" hook pre-tool-use\n";
        let user_wrapper = "#!/bin/bash\n# ContextStream hook wrapper for pre-tool-use\nexec \"/usr/local/bin/my-contextstream-sync.sh\" hook pre-tool-use\n";
        let user_same_name_wrapper = "#!/bin/bash\n# ContextStream hook wrapper for pre-tool-use\nexec \"/opt/user-tools/contextstream-mcp\" hook pre-tool-use\n";

        assert!(is_contextstream_wrapper(product_wrapper, "pre-tool-use"));
        assert!(!is_contextstream_wrapper(user_wrapper, "pre-tool-use"));
        assert!(!is_contextstream_wrapper(
            user_same_name_wrapper,
            "pre-tool-use"
        ));
    }

    #[test]
    fn windows_legacy_wrapper_requires_an_installer_controlled_binary_path() {
        let managed_legacy = "@echo off\r\n\"C:\\Users\\alice\\.contextstream\\bin\\contextstream-mcp.exe\" hook pre-tool-use\r\n";
        let user_legacy =
            "@echo off\r\n\"C:\\Users\\alice\\tools\\contextstream-mcp.exe\" hook pre-tool-use\r\n";
        let explicitly_managed = "@echo off\r\nREM ContextStream managed hook wrapper\r\n\"C:\\Users\\alice\\tools\\renamed.exe\" hook pre-tool-use --contextstream-managed-hook=v1\r\n";

        assert!(is_contextstream_wrapper_for_platform(
            managed_legacy,
            "pre-tool-use",
            true
        ));
        assert!(!is_contextstream_wrapper_for_platform(
            user_legacy,
            "pre-tool-use",
            true
        ));
        assert!(is_contextstream_wrapper_for_platform(
            explicitly_managed,
            "pre-tool-use",
            true
        ));
    }

    #[test]
    fn hook_wrapper_install_refuses_a_user_modified_managed_template() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp
            .path()
            .join(format!("PreToolUse{}", hook_wrapper_extension()));
        let customized = format!(
            "{}# user customization\n",
            hook_wrapper_script_content("contextstream-mcp", "pre-tool-use")
        );
        std::fs::write(&path, &customized).expect("seed customized wrapper");

        let error = write_hook_wrapper_script(
            temp.path(),
            "PreToolUse",
            "pre-tool-use",
            "contextstream-mcp",
        )
        .expect_err("whole-file overwrite must require exact ownership");

        assert!(error
            .to_string()
            .contains("Refusing to overwrite user-owned"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), customized);
        assert!(!safe_edit::backup_path(&path).unwrap().exists());
    }

    #[test]
    fn test_hook_command_quotes_binary_paths_with_spaces() {
        let command = hook_command(
            "/Applications/Claude Code/contextstream-mcp",
            "session-start",
        );

        assert_eq!(
            command,
            "\"/Applications/Claude Code/contextstream-mcp\" hook session-start --contextstream-managed-hook=v1"
        );
    }

    #[test]
    fn windows_hook_paths_do_not_double_literal_backslashes() {
        let path = r"C:\Users\Example\AppData\Local\ContextStream\contextstream-mcp.exe";
        assert_eq!(escape_for_double_quotes_for_platform(path, true), path);
    }

    #[cfg(not(windows))]
    #[test]
    fn hook_command_escapes_shell_expansion_in_binary_paths() {
        let command = hook_command(
            "/tmp/$(touch should-not-run)/`also-not-run`/contextstream-mcp",
            "session-start",
        );

        assert!(command.contains("\\$(touch should-not-run)"));
        assert!(command.contains("\\`also-not-run\\`"));
    }

    #[test]
    fn untouched_claude_install_uninstall_restores_exact_bytes() {
        let _env_guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let settings = temp.path().join("settings.json");
        let original = "{\n  // user comment\n  \"theme\": \"custom\",\n  \"hooks\": {\n    \"PreToolUse\": [{\"hooks\":[{\"type\":\"command\",\"command\":\"echo user\"}]}]\n  }\n}\n";
        std::fs::write(&settings, original).expect("seed settings");

        let before =
            safe_edit::read_for_edit(&settings, safe_edit::JsonDialect::Strict).expect("load");
        let installed =
            build_claude_hooks_update(&before, &settings, None).expect("build hook update");
        safe_edit::commit(&settings, &before, &installed).expect("install hooks");

        let current =
            safe_edit::read_for_edit(&settings, safe_edit::JsonDialect::Strict).expect("reload");
        assert!(
            try_restore_exact_hook_backup(&settings, &current, |backup| {
                build_claude_hooks_update(backup, &settings, None)
            })
            .expect("restore")
        );

        assert_eq!(std::fs::read_to_string(&settings).unwrap(), original);
        assert!(!safe_edit::backup_path(&settings).unwrap().exists());
    }

    #[test]
    fn exact_restore_refuses_intervening_user_formatting_edits() {
        let _env_guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let settings = temp.path().join("settings.json");
        let original = "{\"hooks\":{\"PreToolUse\":[{\"hooks\":[{\"type\":\"command\",\"command\":\"echo user\"}]}]}}\n";
        std::fs::write(&settings, original).expect("seed settings");

        let before =
            safe_edit::read_for_edit(&settings, safe_edit::JsonDialect::Strict).expect("load");
        let installed =
            build_claude_hooks_update(&before, &settings, None).expect("build hook update");
        safe_edit::commit(&settings, &before, &installed).expect("install hooks");

        let mut edited = std::fs::read_to_string(&settings).unwrap();
        edited.push('\n');
        std::fs::write(&settings, &edited).expect("user formatting edit");
        let current =
            safe_edit::read_for_edit(&settings, safe_edit::JsonDialect::Strict).expect("reload");

        assert!(
            !try_restore_exact_hook_backup(&settings, &current, |backup| {
                build_claude_hooks_update(backup, &settings, None)
            })
            .expect("check restore")
        );
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), edited);
        assert!(safe_edit::backup_path(&settings).unwrap().exists());
    }

    #[test]
    fn corrupt_hook_recovery_backup_blocks_uninstall_before_live_edit() {
        let temp = tempfile::tempdir().expect("tempdir");
        let settings = temp.path().join("settings.json");
        let live = json!({
            "theme": "preserve",
            "hooks": {
                "PreToolUse": [{
                    "hooks": [{
                        "type": "command",
                        "command": "contextstream-mcp hook pre-tool-use --contextstream-managed-hook=v1"
                    }]
                }]
            }
        });
        let live_raw = serde_json::to_string_pretty(&live).unwrap();
        std::fs::write(&settings, &live_raw).expect("seed live settings");
        let backup = safe_edit::backup_path(&settings).expect("backup path");
        std::fs::write(&backup, "{ corrupt recovery JSON").expect("seed corrupt backup");
        let current =
            safe_edit::read_for_edit(&settings, safe_edit::JsonDialect::Strict).expect("load live");

        let error = try_restore_exact_hook_backup(&settings, &current, |snapshot| {
            build_claude_hooks_update(snapshot, &settings, None)
        })
        .expect_err("corrupt recovery state must not be ignored");

        assert!(error.to_string().contains("not valid JSON"), "{error:#}");
        assert_eq!(std::fs::read_to_string(&settings).unwrap(), live_raw);
    }

    #[test]
    fn refreshed_generated_hook_configs_uninstall_without_backup_debris() {
        let _env_guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        std::env::set_var("HOME", temp.path());

        for (editor, path) in [
            (
                Editor::ClaudeCode,
                claude_code_settings_path().expect("Claude path"),
            ),
            (Editor::Cursor, cursor_hooks_path().expect("Cursor path")),
            (
                Editor::Windsurf,
                windsurf_hooks_path().expect("Windsurf path"),
            ),
        ] {
            install_hooks(&editor, None).expect("initial generated install");
            let installed = std::fs::read_to_string(&path).expect("read generated hooks");
            let old_managed = installed.replace(&find_binary_path(), "/old/contextstream-mcp");
            assert_ne!(old_managed, installed, "test must alter a managed command");
            std::fs::write(&path, old_managed).expect("simulate an older managed hook config");

            install_hooks(&editor, None).expect("refresh generated hooks");
            assert!(
                safe_edit::backup_path(&path).unwrap().exists(),
                "refresh must retain the first managed snapshot"
            );
            let flavor = match editor {
                Editor::ClaudeCode => HookConfigFlavor::Claude,
                Editor::Cursor => HookConfigFlavor::Cursor,
                Editor::Windsurf => HookConfigFlavor::Windsurf,
                _ => unreachable!(),
            };
            let current = safe_edit::read_for_edit(&path, safe_edit::JsonDialect::Strict).unwrap();
            assert!(
                hook_config_is_wholly_managed(&current, flavor),
                "{} current config was not classified as managed:\n{}",
                editor.display_name(),
                current.raw
            );
            let backup = safe_edit::read_recovery_for_edit(
                &safe_edit::backup_path(&path).unwrap(),
                safe_edit::JsonDialect::Strict,
            )
            .unwrap()
            .unwrap();
            assert!(
                hook_config_is_wholly_managed(&backup, flavor),
                "{} backup was not classified as managed:\n{}",
                editor.display_name(),
                backup.raw
            );

            uninstall_hooks(&editor).expect("uninstall generated hooks");
            assert!(
                !path.exists(),
                "{} hook config remained",
                editor.display_name()
            );
            assert!(!safe_edit::backup_path(&path).unwrap().exists());
        }

        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn staged_binary_replacement_never_deletes_the_old_file_first() {
        let temp = tempfile::tempdir().expect("tempdir");
        let target = temp.path().join("contextstream-mcp");
        let missing_stage = temp.path().join("missing-stage");
        std::fs::write(&target, "known-good").unwrap();

        assert!(replace_binary_atomically(&missing_stage, &target).is_err());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "known-good");

        let staged = temp.path().join("staged");
        std::fs::write(&staged, "validated-new").unwrap();
        replace_binary_atomically(&staged, &target).expect("atomic replace");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "validated-new");
        assert!(!staged.exists());
    }

    #[test]
    fn test_sanitize_binary_path_strips_deleted_suffix_when_live_binary_exists() {
        let temp = tempfile::tempdir().expect("tempdir");
        let live_binary = temp.path().join("contextstream-mcp");
        std::fs::write(&live_binary, b"#!/bin/sh\n").expect("write binary");

        let deleted_view = std::path::PathBuf::from(format!("{} (deleted)", live_binary.display()));

        assert_eq!(
            sanitize_binary_path(&deleted_view).as_deref(),
            Some(live_binary.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn test_uninstall_vscode_hooks_removes_cline_wrappers() {
        // Mutating HOME must serialize with all other env-touching tests.
        let _env_guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        std::env::set_var("HOME", temp.path());

        let hooks_dir = cline_hooks_dir().expect("cline hooks dir");
        std::fs::create_dir_all(&hooks_dir).expect("create hooks dir");
        for (base_name, hook_name) in VSCODE_WRAPPER_HOOK_SPECS {
            let wrapper = hooks_dir.join(format!("{}{}", base_name, hook_wrapper_extension()));
            std::fs::write(
                &wrapper,
                hook_wrapper_script_content("contextstream-mcp", hook_name),
            )
            .expect("write wrapper");
        }

        uninstall_hooks(&Editor::Cline).expect("uninstall cline hooks");

        for (base_name, _) in VSCODE_WRAPPER_HOOK_SPECS {
            let wrapper = hooks_dir.join(format!("{}{}", base_name, hook_wrapper_extension()));
            assert!(
                !wrapper.exists(),
                "expected wrapper to be removed: {}",
                wrapper.display()
            );
            assert!(!safe_edit::backup_path(&wrapper).unwrap().exists());
        }

        std::fs::create_dir_all(&hooks_dir).expect("recreate hooks dir");
        let user_wrapper = hooks_dir.join(format!("PreToolUse{}", hook_wrapper_extension()));
        std::fs::write(&user_wrapper, "echo user-owned\n").expect("write user wrapper");
        uninstall_hooks(&Editor::Cline).expect("re-run uninstall");
        assert_eq!(
            std::fs::read_to_string(&user_wrapper).unwrap(),
            "echo user-owned\n"
        );

        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }

    #[test]
    fn wrapper_uninstall_validates_backup_before_removing_live_script() {
        let _env_guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let temp = tempfile::tempdir().expect("tempdir");
        let previous_home = std::env::var_os("HOME");
        std::env::set_var("HOME", temp.path());

        let hooks_dir = cline_hooks_dir().expect("Cline hooks directory");
        let wrapper = write_hook_wrapper_script(
            &hooks_dir,
            "PreToolUse",
            "pre-tool-use",
            "/old/contextstream-mcp",
        )
        .expect("initial wrapper");
        write_hook_wrapper_script(
            &hooks_dir,
            "PreToolUse",
            "pre-tool-use",
            "/new/contextstream-mcp",
        )
        .expect("refresh wrapper");
        let live = std::fs::read_to_string(&wrapper).expect("read live wrapper");
        let backup = safe_edit::backup_path(&wrapper).expect("backup path");
        std::fs::write(&backup, "user or corrupt recovery content\n").expect("corrupt backup");

        let error = uninstall_hooks(&Editor::Cline)
            .expect_err("unrecognized recovery state must fail closed");

        assert!(error.to_string().contains("not a recognized"), "{error:#}");
        assert_eq!(std::fs::read_to_string(&wrapper).unwrap(), live);

        if let Some(value) = previous_home {
            std::env::set_var("HOME", value);
        } else {
            std::env::remove_var("HOME");
        }
    }
}

#[cfg(test)]
mod customized_settings_tests {
    use super::*;

    /// Regression for the reported support issue: a heavily customized
    /// settings.json must survive an install with every unrelated key,
    /// unknown hook event, and unparseable entry intact.
    #[test]
    fn customized_settings_survive_install() {
        let dir = std::env::temp_dir().join(format!("cs-custom-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");

        std::fs::write(
            &path,
            serde_json::to_string_pretty(&json!({
                "model": "opus",
                "statusLine": { "type": "command", "command": "~/bin/statusline.sh" },
                "permissions": { "allow": ["Bash(git:*)"], "deny": ["Bash(rm:*)"] },
                "env": { "MY_VAR": "1" },
                "hooks": {
                    "PreToolUse": [
                        { "matcher": "Bash",
                          "hooks": [{ "type": "command", "command": "~/bin/audit.sh" }] }
                    ],
                    "SomeFutureEvent": [
                        { "matcher": "*",
                          "hooks": [{ "type": "command", "command": "~/bin/future.sh" }] }
                    ],
                    // `timeout` as a string does not match HookCommand.
                    "WeirdShape": [
                        { "matcher": "X",
                          "hooks": [{ "type": "command", "command": "~/bin/odd.sh",
                                      "timeout": "30" }] }
                    ]
                }
            }))
            .unwrap(),
        )
        .unwrap();

        let loaded = safe_edit::read_for_edit(&path, safe_edit::JsonDialect::Strict)
            .expect("parse settings");
        let mut hooks = hooks_object_of(&loaded, &path).unwrap();
        merge_contextstream_hooks_into(&mut hooks, generate_contextstream_hooks(None)).unwrap();
        let mut updated = loaded.value.clone();
        updated["hooks"] = Value::Object(hooks);
        safe_edit::commit(&path, &loaded, &updated).unwrap();

        let after: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();

        assert_eq!(after["model"], json!("opus"), "model key lost");
        assert_eq!(after["env"]["MY_VAR"], json!("1"), "env lost");
        assert_eq!(
            after["permissions"]["deny"],
            json!(["Bash(rm:*)"]),
            "permissions lost"
        );
        assert_eq!(after["statusLine"]["command"], json!("~/bin/statusline.sh"));

        let dump = serde_json::to_string(&after["hooks"]).unwrap();
        assert!(dump.contains("audit.sh"), "user PreToolUse hook lost");
        assert!(dump.contains("future.sh"), "unknown hook event lost");
        assert!(dump.contains("odd.sh"), "unparseable entry lost");
        assert!(
            dump.contains("contextstream"),
            "our hooks were not installed"
        );

        // The pre-write backup must hold the original content.
        let backup = dir.join("settings.json.contextstream.bak");
        assert!(std::fs::read_to_string(&backup)
            .unwrap()
            .contains("audit.sh"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
