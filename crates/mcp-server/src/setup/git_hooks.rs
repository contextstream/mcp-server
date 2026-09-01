//! Managed git-hook installer for local VCS capture.
//!
//! Installs idempotent, fail-open `post-commit`, `pre-push`, `post-checkout`,
//! and `post-merge` hooks that dispatch into `contextstream-mcp hook
//! git-<event>`. Each hook is written as a sentinel-delimited **managed block**
//! so it composes with pre-existing user hooks (their content runs first) and
//! can be cleanly re-pointed on upgrade or stripped on uninstall.
//!
//! The managed block backgrounds the capture call and `exit 0`s, so git is
//! never blocked or slowed even if the binary is missing or the network is down.
//!
//! Windows note: these are `/bin/sh` scripts and run under the Git-for-Windows
//! bundled bash that git uses for hooks. Native `cmd`-only environments without
//! that shell are not covered; chmod is skipped on non-unix.

use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

use super::safe_edit;

/// Hook files managed by ContextStream.
const HOOK_NAMES: &[&str] = &["post-commit", "pre-push", "post-checkout", "post-merge"];

/// Managed-block delimiters. Everything between these (inclusive) is owned by
/// ContextStream and may be rewritten or removed; everything else is preserved.
const SENTINEL_START: &str = "# >>> contextstream managed >>>";
const SENTINEL_END: &str = "# <<< contextstream managed <<<";

const DEFAULT_SHEBANG: &str = "#!/bin/sh";

/// Resolve the repository root (worktree top level) starting from `start`.
pub fn resolve_repo_root(start: &Path) -> Option<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(start)
        .args(["rev-parse", "--show-toplevel"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!root.is_empty()).then(|| PathBuf::from(root))
}

/// Resolve the hooks directory for `repo_root`.
///
/// `core.hooksPath` wins when set (absolute, or relative to the repo root).
/// Otherwise `git rev-parse --git-path hooks` is used, which correctly handles
/// linked worktrees and `.git`-file checkouts.
pub fn resolve_hooks_dir(repo_root: &Path) -> Option<PathBuf> {
    if let Some(custom) = git_config_value(repo_root, "core.hooksPath") {
        let expanded = expand_tilde(&custom);
        let path = if expanded.is_absolute() {
            expanded
        } else {
            repo_root.join(expanded)
        };
        return Some(path);
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["rev-parse", "--git-path", "hooks"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let rel = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if rel.is_empty() {
        return None;
    }
    let path = PathBuf::from(rel);
    Some(if path.is_absolute() {
        path
    } else {
        repo_root.join(path)
    })
}

fn git_config_value(repo_root: &Path, key: &str) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .args(["config", "--get", key])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!value.is_empty()).then_some(value)
}

fn expand_tilde(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(raw)
}

/// Quote a path for safe interpolation inside a double-quoted shell context.
fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if matches!(c, '"' | '\\' | '$' | '`') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// Install (or refresh) the managed git hooks for `repo_root`.
///
/// Idempotent: re-running replaces only the managed block (re-pointing the
/// binary path), preserves any user hook content, and chains after it.
pub fn install_git_hooks(repo_root: &Path) -> Result<()> {
    let hooks_dir = resolve_hooks_dir(repo_root).ok_or_else(|| {
        anyhow!(
            "could not resolve git hooks dir for {}",
            repo_root.display()
        )
    })?;
    let binary = hook_binary_path();
    for name in HOOK_NAMES {
        install_one(&hooks_dir.join(name), name, &binary)?;
    }
    Ok(())
}

/// Remove the managed block from each hook. Cleanly modified user hooks are
/// restored byte-for-byte from their first backup; wholly generated hook files
/// are deleted.
pub fn uninstall_git_hooks(repo_root: &Path) -> Result<()> {
    let Some(hooks_dir) = resolve_hooks_dir(repo_root) else {
        return Ok(());
    };
    for name in HOOK_NAMES {
        let path = hooks_dir.join(name);
        let existing = match std::fs::read_to_string(&path) {
            Ok(existing) => existing,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if managed_block_bounds(&existing)?.is_none() {
            continue;
        }

        let backup_path = safe_edit::backup_path(&path)?;
        let backup = safe_edit::read_recovery_file(&backup_path).with_context(|| {
            format!(
                "Could not read recovery backup {} before uninstalling {}",
                backup_path.display(),
                path.display()
            )
        })?;

        // A clean install/uninstall cycle must be byte-for-byte reversible,
        // including the original executable bits. Only consume the backup
        // after proving that the current file is exactly what this installer
        // would have generated from it.
        if let Some(backup) = backup.as_deref() {
            let current_bounds = managed_block_bounds(&existing)?
                .expect("the live hook was classified as managed above");
            let current_block = &existing[current_bounds.start..current_bounds.end];
            if managed_block_bounds(backup)?.is_none()
                && append_block(backup, current_block) == existing
            {
                safe_edit::restore_text_first_backup(&path, &existing, true, backup)?;
                continue;
            }
        }

        let stripped = strip_block(&existing)?;
        let backup_is_wholly_managed = if let Some(backup) = backup.as_deref() {
            managed_block_bounds(backup)?.is_some() && is_effectively_empty(&strip_block(backup)?)
        } else {
            false
        };
        if is_effectively_empty(&stripped) && (backup.is_none() || backup_is_wholly_managed) {
            // No backup, or a backup consisting solely of an older managed
            // block, proves this hook was wholly generated by ContextStream.
            safe_edit::remove_owned_file_if_unchanged(&path, &existing)?;
            if backup_is_wholly_managed {
                safe_edit::remove_owned_file_if_unchanged(
                    &backup_path,
                    backup.as_deref().expect("classified backup"),
                )?;
            }
        } else {
            safe_edit::write_executable_if_unchanged(&path, &stripped, Some(&existing))?;
        }
    }
    Ok(())
}

/// Whether the managed block is present in this repo's `post-commit` hook.
/// Used to decide whether the Claude Bash fallback should self-capture.
pub fn is_managed_installed(repo_root: &Path) -> bool {
    resolve_hooks_dir(repo_root)
        .map(|dir| dir.join("post-commit"))
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|content| managed_block_bounds(&content).ok().flatten())
        .is_some()
}

/// Resolve the binary path baked into hook scripts: the managed helper if
/// installed, else the running executable, else a bare PATH lookup.
fn hook_binary_path() -> String {
    let managed = super::managed_binary_path();
    if safe_edit::is_dry_run() || managed.exists() {
        return managed.to_string_lossy().to_string();
    }
    if let Ok(exe) = std::env::current_exe() {
        let raw = exe.to_string_lossy();
        let raw = raw.strip_suffix(" (deleted)").unwrap_or(&raw);
        if !raw.is_empty() {
            return raw.to_string();
        }
    }
    "contextstream-mcp".to_string()
}

/// Build the managed block for one hook.
///
/// Fire-and-forget + fail-open: the capture runs detached (`&`) with stdio
/// silenced, and the block always `exit 0`s. `pre-push` is special-cased to
/// read the ref list from stdin *before* backgrounding the network call.
fn managed_block(name: &str, binary: &str) -> String {
    let bin = shell_quote(binary);
    let hook_cmd = format!("git-{name}");
    let inner = if name == "pre-push" {
        format!(
            "contextstream_previous_status=$?\n\
             contextstream_refs=\"$(cat)\"\n\
             ( printf '%s' \"$contextstream_refs\" | {bin} hook {hook_cmd} \"$@\" >/dev/null 2>&1 & ) || true\n\
             exit \"$contextstream_previous_status\"\n"
        )
    } else {
        format!(
            "contextstream_previous_status=$?\n\
             ( {bin} hook {hook_cmd} \"$@\" </dev/null >/dev/null 2>&1 & ) || true\n\
             exit \"$contextstream_previous_status\"\n"
        )
    };
    format!("{SENTINEL_START}\n{inner}{SENTINEL_END}\n")
}

fn install_one(path: &Path, name: &str, binary: &str) -> Result<()> {
    let existing = match std::fs::read_to_string(path) {
        Ok(existing) => Some(existing),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let new_content = desired_hook_content(existing.as_deref(), name, binary)?;
    safe_edit::write_executable_if_unchanged(path, &new_content, existing.as_deref())?;
    Ok(())
}

fn desired_hook_content(existing: Option<&str>, name: &str, binary: &str) -> Result<String> {
    let block = managed_block(name, binary);
    match existing {
        Some(existing) if managed_block_bounds(existing)?.is_some() => {
            replace_block(existing, &block)
        }
        Some(existing) => Ok(append_block(existing, &block)),
        None => Ok(format!("{DEFAULT_SHEBANG}\n{block}")),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ManagedBlockBounds {
    start: usize,
    end: usize,
}

/// Locate exactly one well-formed managed block.
///
/// Delimiters count only when they occupy an entire line. A command such as
/// `echo "# >>> contextstream managed >>>"` is user content, not ownership.
/// Duplicate, reversed, or unmatched delimiters are ambiguous and therefore
/// rejected without touching the file.
fn managed_block_bounds(existing: &str) -> Result<Option<ManagedBlockBounds>> {
    let mut starts = Vec::new();
    let mut ends = Vec::new();
    let bytes = existing.as_bytes();
    let mut line_start = 0;

    while line_start < bytes.len() {
        let newline = bytes[line_start..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|offset| line_start + offset);
        let line_end = newline.unwrap_or(bytes.len());
        let content_end = if line_end > line_start && bytes[line_end - 1] == b'\r' {
            line_end - 1
        } else {
            line_end
        };
        let line = &existing[line_start..content_end];
        let after_line = newline.map_or(bytes.len(), |index| index + 1);

        if line == SENTINEL_START {
            starts.push(line_start);
        } else if line == SENTINEL_END {
            ends.push(after_line);
        }

        if after_line == bytes.len() {
            break;
        }
        line_start = after_line;
    }

    match (starts.as_slice(), ends.as_slice()) {
        ([], []) => Ok(None),
        ([start], [end]) if *start < *end => Ok(Some(ManagedBlockBounds {
            start: *start,
            end: *end,
        })),
        _ => bail!(
            "Refusing to edit a git hook with malformed or duplicate ContextStream managed-block delimiters"
        ),
    }
}

/// Replace the existing managed block in place (preserving surrounding content).
fn replace_block(existing: &str, block: &str) -> Result<String> {
    let Some(bounds) = managed_block_bounds(existing)? else {
        return Ok(append_block(existing, block));
    };
    let mut result = String::with_capacity(existing.len() + block.len());
    result.push_str(&existing[..bounds.start]);
    result.push_str(block);
    result.push_str(&existing[bounds.end..]);
    Ok(result)
}

/// Append the managed block after existing (user) content, separated by a blank
/// line so the two never run together.
fn append_block(existing: &str, block: &str) -> String {
    let mut result = existing.to_string();
    if !result.ends_with('\n') {
        result.push('\n');
    }
    if !result.ends_with("\n\n") {
        result.push('\n');
    }
    result.push_str(block);
    result
}

/// Remove the managed block, leaving surrounding content intact.
fn strip_block(existing: &str) -> Result<String> {
    let Some(bounds) = managed_block_bounds(existing)? else {
        return Ok(existing.to_string());
    };
    let mut result = String::with_capacity(existing.len());
    result.push_str(&existing[..bounds.start]);
    result.push_str(&existing[bounds.end..]);
    Ok(result)
}

/// Whether stripped content is just a shebang / whitespace. It may be deleted
/// only when no pre-install backup exists, proving the file was generated.
fn is_effectively_empty(content: &str) -> bool {
    let meaningful: Vec<&str> = content
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    match meaningful.as_slice() {
        [] => true,
        [only] => only.starts_with("#!"),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;
    use tempfile::tempdir;

    fn init_repo(dir: &Path) {
        let run = |args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .expect("git runs")
                .status
                .success();
            assert!(ok, "git {:?} failed", args);
        };
        run(&["init", "-q"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn install_is_idempotent_single_block() {
        let temp = tempdir().unwrap();
        init_repo(temp.path());

        install_git_hooks(temp.path()).expect("install 1");
        install_git_hooks(temp.path()).expect("install 2");

        let hooks_dir = resolve_hooks_dir(temp.path()).unwrap();
        for name in HOOK_NAMES {
            let content = read(&hooks_dir.join(name));
            assert_eq!(
                content.matches(SENTINEL_START).count(),
                1,
                "{name} should have exactly one managed block"
            );
            assert!(content.contains(&format!("hook git-{name}")));
        }
    }

    #[test]
    fn preserves_and_chains_existing_user_hook() {
        let temp = tempdir().unwrap();
        init_repo(temp.path());
        let hooks_dir = resolve_hooks_dir(temp.path()).unwrap();
        std::fs::create_dir_all(&hooks_dir).unwrap();
        let post_commit = hooks_dir.join("post-commit");
        std::fs::write(&post_commit, "#!/bin/sh\necho user hook ran\n").unwrap();

        install_git_hooks(temp.path()).expect("install");

        let content = read(&post_commit);
        assert!(content.contains("echo user hook ran"));
        assert!(content.contains(SENTINEL_START));
        // User content precedes the managed block.
        assert!(
            content.find("echo user hook ran").unwrap() < content.find(SENTINEL_START).unwrap()
        );
    }

    #[test]
    fn honors_core_hooks_path() {
        let temp = tempdir().unwrap();
        init_repo(temp.path());
        let custom = temp.path().join("my-hooks");
        Command::new("git")
            .arg("-C")
            .arg(temp.path())
            .args(["config", "core.hooksPath", custom.to_str().unwrap()])
            .output()
            .unwrap();

        install_git_hooks(temp.path()).expect("install");
        assert!(custom.join("post-commit").exists());
        assert_eq!(resolve_hooks_dir(temp.path()).unwrap(), custom);
    }

    #[test]
    fn uninstall_strips_managed_and_removes_owned_files() {
        let temp = tempdir().unwrap();
        init_repo(temp.path());
        let hooks_dir = resolve_hooks_dir(temp.path()).unwrap();
        std::fs::create_dir_all(&hooks_dir).unwrap();
        // pre-existing user post-commit; managed-only pre-push.
        let original = "#!/bin/sh\necho user hook ran\n";
        std::fs::write(hooks_dir.join("post-commit"), original).unwrap();

        install_git_hooks(temp.path()).expect("install");
        assert!(is_managed_installed(temp.path()));

        uninstall_git_hooks(temp.path()).expect("uninstall");

        // User hook restored exactly and its consumed backup cleaned up.
        let post_commit = read(&hooks_dir.join("post-commit"));
        assert_eq!(post_commit, original);
        assert!(!safe_edit::backup_path(&hooks_dir.join("post-commit"))
            .unwrap()
            .exists());
        // Managed-only files removed entirely.
        assert!(!hooks_dir.join("pre-push").exists());
        assert!(!safe_edit::backup_path(&hooks_dir.join("pre-push"))
            .unwrap()
            .exists());
        assert!(!is_managed_installed(temp.path()));
    }

    #[cfg(unix)]
    #[test]
    fn clean_uninstall_restores_original_hook_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempdir().unwrap();
        init_repo(temp.path());
        let hooks_dir = resolve_hooks_dir(temp.path()).unwrap();
        std::fs::create_dir_all(&hooks_dir).unwrap();
        let post_commit = hooks_dir.join("post-commit");
        let original = "#!/bin/sh\necho deliberately non-executable\n";
        std::fs::write(&post_commit, original).unwrap();
        std::fs::set_permissions(&post_commit, std::fs::Permissions::from_mode(0o640)).unwrap();

        install_git_hooks(temp.path()).expect("install");
        assert_eq!(
            std::fs::metadata(&post_commit)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );

        uninstall_git_hooks(temp.path()).expect("uninstall");
        assert_eq!(read(&post_commit), original);
        assert_eq!(
            std::fs::metadata(&post_commit)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
    }

    #[test]
    fn uninstall_preserves_user_edits_made_after_install() {
        let temp = tempdir().unwrap();
        init_repo(temp.path());
        let hooks_dir = resolve_hooks_dir(temp.path()).unwrap();
        std::fs::create_dir_all(&hooks_dir).unwrap();
        let post_commit = hooks_dir.join("post-commit");
        std::fs::write(&post_commit, "#!/bin/sh\necho original\n").unwrap();

        install_git_hooks(temp.path()).expect("install");
        let installed = read(&post_commit);
        let edited = installed.replacen("#!/bin/sh\n", "#!/bin/sh\necho added-after-install\n", 1);
        std::fs::write(&post_commit, &edited).unwrap();

        uninstall_git_hooks(temp.path()).expect("uninstall");

        let result = read(&post_commit);
        assert!(result.contains("echo original"));
        assert!(result.contains("echo added-after-install"));
        assert!(!result.contains(SENTINEL_START));
        // The first backup remains as a recovery aid because exact restoration
        // was intentionally impossible after the user's edit.
        assert!(safe_edit::backup_path(&post_commit).unwrap().exists());
    }

    #[test]
    fn upgraded_generated_hook_uninstalls_without_shebang_or_backup_debris() {
        let temp = tempdir().unwrap();
        init_repo(temp.path());
        let hooks_dir = resolve_hooks_dir(temp.path()).unwrap();
        let post_commit = hooks_dir.join("post-commit");

        install_one(&post_commit, "post-commit", "/old/contextstream-mcp").expect("old install");
        install_one(&post_commit, "post-commit", "/new/contextstream-mcp").expect("refresh");
        assert!(safe_edit::backup_path(&post_commit).unwrap().exists());

        uninstall_git_hooks(temp.path()).expect("uninstall");

        assert!(!post_commit.exists());
        assert!(!safe_edit::backup_path(&post_commit).unwrap().exists());
    }

    #[test]
    fn sentinel_text_inside_a_user_command_is_not_ownership() {
        let temp = tempdir().unwrap();
        init_repo(temp.path());
        let hooks_dir = resolve_hooks_dir(temp.path()).unwrap();
        std::fs::create_dir_all(&hooks_dir).unwrap();
        let post_commit = hooks_dir.join("post-commit");
        let original =
            "#!/bin/sh\necho \"# >>> contextstream managed >>>\" > /tmp/user-owned-log\n";
        std::fs::write(&post_commit, original).unwrap();

        assert!(!is_managed_installed(temp.path()));
        install_git_hooks(temp.path()).expect("install");
        assert!(is_managed_installed(temp.path()));
        uninstall_git_hooks(temp.path()).expect("uninstall");

        assert_eq!(read(&post_commit), original);
    }

    #[test]
    fn malformed_or_duplicate_delimiters_fail_closed() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("post-commit");
        let cases = [
            format!("#!/bin/sh\n{SENTINEL_START}\necho user\n"),
            format!("#!/bin/sh\n{SENTINEL_END}\necho user\n"),
            format!("#!/bin/sh\n{SENTINEL_START}\n{SENTINEL_START}\n{SENTINEL_END}\n"),
            format!("#!/bin/sh\n{SENTINEL_END}\n{SENTINEL_START}\n"),
        ];

        for original in cases {
            std::fs::write(&path, &original).unwrap();
            let error = install_one(&path, "post-commit", "/managed/bin")
                .expect_err("ambiguous ownership must fail closed");
            assert!(
                error.to_string().contains("malformed or duplicate"),
                "{error:#}"
            );
            assert_eq!(read(&path), original);
            assert!(!safe_edit::backup_path(&path).unwrap().exists());
        }
    }

    #[test]
    fn stripping_a_valid_block_preserves_all_surrounding_bytes() {
        let block = managed_block("post-commit", "/managed/bin");
        let existing = format!("before\r\n\r\n{block}after-without-newline");
        assert_eq!(
            strip_block(&existing).unwrap(),
            "before\r\n\r\nafter-without-newline"
        );
    }

    #[test]
    fn is_effectively_empty_detects_shebang_only() {
        assert!(is_effectively_empty(""));
        assert!(is_effectively_empty("#!/bin/sh\n"));
        assert!(is_effectively_empty("  \n#!/usr/bin/env bash\n\n"));
        assert!(!is_effectively_empty("#!/bin/sh\necho hi\n"));
    }

    #[test]
    fn managed_block_is_fail_open_and_backgrounded() {
        let block = managed_block("post-commit", "/path/to/bin");
        assert!(block.contains("hook git-post-commit"));
        assert!(block.contains("& ) || true"));
        assert!(block.trim_end().ends_with(SENTINEL_END));
        assert!(block.contains("contextstream_previous_status=$?"));
        assert!(block.contains("exit \"$contextstream_previous_status\""));

        let pre_push = managed_block("pre-push", "/path/to/bin");
        assert!(pre_push.contains("contextstream_refs=\"$(cat)\""));
        assert!(pre_push.contains("printf '%s'"));
    }

    #[cfg(unix)]
    #[test]
    fn managed_block_preserves_the_user_hook_exit_status() {
        let temp = tempdir().unwrap();
        let rejecting = temp.path().join("rejecting-hook");
        let successful = temp.path().join("successful-hook");
        std::fs::write(
            &rejecting,
            format!(
                "#!/bin/sh\nfalse\n{}",
                managed_block("pre-push", "/missing/binary")
            ),
        )
        .unwrap();
        std::fs::write(
            &successful,
            format!(
                "#!/bin/sh\ntrue\n{}",
                managed_block("pre-push", "/missing/binary")
            ),
        )
        .unwrap();

        let rejected = Command::new("sh")
            .arg(&rejecting)
            .stdin(std::process::Stdio::null())
            .status()
            .unwrap();
        let accepted = Command::new("sh")
            .arg(&successful)
            .stdin(std::process::Stdio::null())
            .status()
            .unwrap();

        assert_eq!(rejected.code(), Some(1));
        assert!(accepted.success());
    }
}
