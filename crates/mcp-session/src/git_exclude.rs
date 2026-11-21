//! Keep a machine-local file out of Git by listing it in the exclude file Git
//! reads for its checkout (`<common Git directory>/info/exclude`), never in the
//! shared `.gitignore`.
//!
//! This works without the `git` program, which many Windows machines lack
//! (GitHub Desktop ships its own copy off `PATH`): the Git directory is found
//! from `.git`, a directory or, in a linked worktree or submodule, a file that
//! names it, and the entry is appended directly. When `git` is installed it is
//! also asked whether the file is tracked or ignored already, so no needless
//! entry is written.

use crate::checkout_identity::{git_exclude_target, GitExcludeTarget};
use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
};

/// The fields that tie `.contextstream/config.json` to one checkout on one
/// machine. A copy holding them fails validation in every other clone.
pub const CHECKOUT_IDENTITY_FIELDS: [&str; 3] =
    ["checkout_root", "checkout_id", "repository_fingerprint"];

/// The comment written above the `.contextstream/config.json` exclude entry.
pub const CHECKOUT_CONFIG_REASON: &str =
    "this checkout's link to ContextStream (its path and id on this machine)";

/// Whether a parsed `.contextstream/config.json` names this checkout.
pub fn holds_checkout_identity(config: &serde_json::Value) -> bool {
    CHECKOUT_IDENTITY_FIELDS
        .iter()
        .any(|field| config.get(field).is_some())
}

/// What [`keep_out_of_git`] did.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GitExcludeOutcome {
    /// The entry was added to `exclude_file` now.
    Added {
        entry: String,
        exclude_file: PathBuf,
    },
    /// `exclude_file` listed the entry already.
    AlreadyListed {
        entry: String,
        exclude_file: PathBuf,
    },
    /// Another rule (a `.gitignore`, a global excludes file) ignores the file
    /// already.
    AlreadyIgnored {
        entry: String,
        exclude_file: PathBuf,
    },
    /// Git tracks the file, and no ignore rule applies to a tracked file.
    Tracked { entry: String },
    /// The file is not inside a Git checkout.
    NotInRepository,
    /// `.git` could not be followed or the exclude file could not be written.
    Failed { reason: String },
}

impl GitExcludeOutcome {
    /// Whether `git add -A` now leaves the file out.
    pub fn kept_out(&self) -> bool {
        matches!(
            self,
            Self::Added { .. } | Self::AlreadyListed { .. } | Self::AlreadyIgnored { .. }
        )
    }

    /// The exclude file Git reads for the checkout, when one was resolved.
    pub fn exclude_file(&self) -> Option<&Path> {
        match self {
            Self::Added { exclude_file, .. }
            | Self::AlreadyListed { exclude_file, .. }
            | Self::AlreadyIgnored { exclude_file, .. } => Some(exclude_file),
            _ => None,
        }
    }
}

/// List `path` (a file inside a checkout) in the checkout's
/// `.git/info/exclude`, under a `# ContextStream: <why>` comment. Idempotent:
/// an entry that is there already is not written again, whatever comment it
/// has. Leaves tracked files alone. Never touches `.gitignore`.
pub fn keep_out_of_git(path: &Path, why: &str) -> GitExcludeOutcome {
    keep_out_of_git_with(path, why, GitProgram::IfInstalled)
}

/// Whether to ask the `git` program about tracked and ignored files.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GitProgram {
    IfInstalled,
    #[cfg(test)]
    Never,
}

fn keep_out_of_git_with(path: &Path, why: &str, git: GitProgram) -> GitExcludeOutcome {
    let Some(folder) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return GitExcludeOutcome::Failed {
            reason: format!("{} has no parent folder", path.display()),
        };
    };
    let GitExcludeTarget {
        repository_root,
        exclude_file,
    } = match git_exclude_target(folder) {
        Ok(Some(target)) => target,
        Ok(None) => return GitExcludeOutcome::NotInRepository,
        Err(error) => {
            return GitExcludeOutcome::Failed {
                reason: error.to_string(),
            }
        }
    };
    let relative = match repository_relative_path(path, &repository_root) {
        Ok(relative) => relative,
        Err(reason) => return GitExcludeOutcome::Failed { reason },
    };
    let entry = format!("/{relative}");

    let probe = match git {
        GitProgram::IfInstalled => probe_with_git(&repository_root, &relative),
        #[cfg(test)]
        GitProgram::Never => GitProbe::default(),
    };
    if probe.tracked {
        return GitExcludeOutcome::Tracked { entry };
    }

    let existing = match fs::read(&exclude_file) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return GitExcludeOutcome::Failed {
                reason: format!("could not read {}: {error}", exclude_file.display()),
            }
        }
    };
    if lists_entry(&existing, &entry) {
        return GitExcludeOutcome::AlreadyListed {
            entry,
            exclude_file,
        };
    }
    if probe.ignored {
        return GitExcludeOutcome::AlreadyIgnored {
            entry,
            exclude_file,
        };
    }

    // Keep the file's own line endings: an exclude file edited on Windows may
    // use CRLF, and Git accepts either.
    let newline = if existing.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut addition = String::new();
    if !existing.is_empty() && !existing.ends_with('\n') {
        addition.push_str(newline);
    }
    addition.push_str("# ContextStream: ");
    addition.push_str(why.trim());
    addition.push_str(newline);
    addition.push_str(&entry);
    addition.push_str(newline);

    let written = exclude_file
        .parent()
        .map_or(Ok(()), fs::create_dir_all)
        .and_then(|()| {
            // Append rather than rewrite, so a concurrent edit is never lost.
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&exclude_file)?
                .write_all(addition.as_bytes())
        });
    match written {
        Ok(()) => GitExcludeOutcome::Added {
            entry,
            exclude_file,
        },
        Err(error) => GitExcludeOutcome::Failed {
            reason: format!("could not write {}: {error}", exclude_file.display()),
        },
    }
}

/// `path` relative to the working-tree root, with `/` separators on every
/// platform, as Git patterns need.
fn repository_relative_path(path: &Path, repository_root: &Path) -> Result<String, String> {
    let canonical = match fs::canonicalize(path) {
        Ok(canonical) => canonical,
        // Not written yet: resolve the folder and keep the file name.
        Err(_) => {
            let (Some(folder), Some(name)) = (path.parent(), path.file_name()) else {
                return Err(format!("{} has no file name", path.display()));
            };
            fs::canonicalize(folder)
                .map_err(|error| format!("could not resolve {}: {error}", folder.display()))?
                .join(name)
        }
    };
    let relative = canonical.strip_prefix(repository_root).map_err(|_| {
        format!(
            "{} is outside the checkout at {}",
            path.display(),
            repository_root.display()
        )
    })?;
    let relative = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/");
    if relative.is_empty() {
        return Err(format!("{} is the checkout itself", path.display()));
    }
    Ok(relative)
}

/// Whether an exclude file already has `entry` (`/path`) or its unanchored
/// form (`path`) on a line of its own.
fn lists_entry(existing: &str, entry: &str) -> bool {
    let unanchored = entry.trim_start_matches('/');
    existing.lines().any(|line| {
        let line = line.trim();
        line == entry || line == unanchored
    })
}

#[derive(Default)]
struct GitProbe {
    tracked: bool,
    ignored: bool,
}

/// Ask `git`, when it is installed, whether `relative` is tracked or ignored.
/// Without `git` both answers are "no", which only means the entry is written.
fn probe_with_git(repository_root: &Path, relative: &str) -> GitProbe {
    let run = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(repository_root)
            .args(args)
            // A caller running inside a Git hook may have these set for
            // another repository.
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    };
    // `--literal-pathspecs` keeps a `*` or `:` in a file name from matching
    // other files; `check-ignore` takes plain paths and refuses the flag.
    let tracked = run(&[
        "--literal-pathspecs",
        "ls-files",
        "--error-unmatch",
        "--",
        relative,
    ]);
    let ignored = !tracked && run(&["check-ignore", "-q", "--", relative]);
    GitProbe { tracked, ignored }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    }

    fn git(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    }

    fn entry_count(exclude: &str, entry: &str) -> usize {
        exclude.lines().filter(|line| line.trim() == entry).count()
    }

    #[test]
    fn plain_git_directory_gets_one_entry_without_the_git_program() {
        let repo = tempfile::tempdir().unwrap();
        fs::create_dir_all(repo.path().join(".git")).unwrap();
        fs::create_dir_all(repo.path().join(".contextstream")).unwrap();
        let config = repo.path().join(".contextstream").join("config.json");
        fs::write(&config, "{}\n").unwrap();

        let first = keep_out_of_git_with(&config, CHECKOUT_CONFIG_REASON, GitProgram::Never);
        assert!(
            matches!(first, GitExcludeOutcome::Added { ref entry, .. } if entry == "/.contextstream/config.json"),
            "{first:?}"
        );
        let second = keep_out_of_git_with(&config, CHECKOUT_CONFIG_REASON, GitProgram::Never);
        assert!(
            matches!(second, GitExcludeOutcome::AlreadyListed { .. }),
            "{second:?}"
        );
        assert!(second.kept_out());

        let exclude = fs::read_to_string(repo.path().join(".git/info/exclude")).unwrap();
        assert_eq!(entry_count(&exclude, "/.contextstream/config.json"), 1);
        assert!(
            exclude.contains(&format!("# ContextStream: {CHECKOUT_CONFIG_REASON}\n")),
            "{exclude}"
        );
        assert!(!repo.path().join(".gitignore").exists());
    }

    #[test]
    fn existing_crlf_exclude_keeps_its_lines_and_line_endings() {
        let repo = tempfile::tempdir().unwrap();
        fs::create_dir_all(repo.path().join(".git/info")).unwrap();
        let exclude_path = repo.path().join(".git/info/exclude");
        fs::write(&exclude_path, "# git ls-files --others\r\n*.log").unwrap();
        let mcp = repo.path().join(".mcp.json");
        fs::write(&mcp, "{}").unwrap();

        let outcome = keep_out_of_git_with(&mcp, "why", GitProgram::Never);
        assert!(
            matches!(outcome, GitExcludeOutcome::Added { .. }),
            "{outcome:?}"
        );
        assert_eq!(
            fs::read_to_string(&exclude_path).unwrap(),
            "# git ls-files --others\r\n*.log\r\n# ContextStream: why\r\n/.mcp.json\r\n"
        );
        // An entry the person wrote without the leading slash counts.
        fs::write(&exclude_path, ".mcp.json\n").unwrap();
        assert!(matches!(
            keep_out_of_git_with(&mcp, "why", GitProgram::Never),
            GitExcludeOutcome::AlreadyListed { .. }
        ));
        assert_eq!(fs::read_to_string(&exclude_path).unwrap(), ".mcp.json\n");
    }

    #[test]
    fn linked_worktree_file_writes_the_main_repositorys_exclude() {
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("main");
        let worktree_git_dir = main.join(".git").join("worktrees").join("feature");
        fs::create_dir_all(&worktree_git_dir).unwrap();
        fs::write(worktree_git_dir.join("commondir"), "../..\n").unwrap();
        let worktree = temp.path().join("feature");
        fs::create_dir_all(&worktree).unwrap();
        // Windows Git writes this file with CRLF.
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\r\n", worktree_git_dir.display()),
        )
        .unwrap();
        let mcp = worktree.join(".mcp.json");
        fs::write(&mcp, "{}").unwrap();

        let outcome = keep_out_of_git_with(&mcp, "why", GitProgram::Never);
        let main_exclude = fs::canonicalize(main.join(".git"))
            .unwrap()
            .join("info")
            .join("exclude");
        assert_eq!(
            outcome,
            GitExcludeOutcome::Added {
                entry: "/.mcp.json".to_string(),
                exclude_file: main_exclude.clone(),
            }
        );
        let exclude = fs::read_to_string(&main_exclude).unwrap();
        assert_eq!(entry_count(&exclude, "/.mcp.json"), 1);
        assert!(!worktree_git_dir.join("info").exists());
        assert!(!worktree.join(".gitignore").exists());
    }

    #[test]
    fn folder_outside_any_checkout_is_left_alone() {
        let folder = tempfile::tempdir().unwrap();
        let mcp = folder.path().join(".mcp.json");
        fs::write(&mcp, "{}").unwrap();
        // A temp dir may sit inside a checkout on a developer machine.
        if git_exclude_target(folder.path()).unwrap().is_some() {
            return;
        }
        assert_eq!(
            keep_out_of_git_with(&mcp, "why", GitProgram::Never),
            GitExcludeOutcome::NotInRepository
        );
        assert!(!folder.path().join(".gitignore").exists());
        assert!(!folder.path().join(".git").exists());
    }

    #[test]
    fn with_git_installed_tracked_and_ignored_files_get_no_entry() {
        if !git_available() {
            return;
        }
        let repo = tempfile::tempdir().unwrap();
        git(repo.path(), &["init", "-q"]);
        let tracked = repo.path().join("CLAUDE.md");
        fs::write(&tracked, "# rules\n").unwrap();
        git(repo.path(), &["add", "CLAUDE.md"]);
        fs::write(repo.path().join(".gitignore"), "/ignored.json\n").unwrap();
        let ignored = repo.path().join("ignored.json");
        fs::write(&ignored, "{}").unwrap();
        let fresh = repo.path().join(".mcp.json");
        fs::write(&fresh, "{}").unwrap();

        assert_eq!(
            keep_out_of_git(&tracked, "why"),
            GitExcludeOutcome::Tracked {
                entry: "/CLAUDE.md".to_string()
            }
        );
        assert!(matches!(
            keep_out_of_git(&ignored, "why"),
            GitExcludeOutcome::AlreadyIgnored { .. }
        ));
        assert!(matches!(
            keep_out_of_git(&fresh, "why"),
            GitExcludeOutcome::Added { .. }
        ));
        assert!(matches!(
            keep_out_of_git(&fresh, "why"),
            GitExcludeOutcome::AlreadyListed { .. }
        ));
        let exclude = fs::read_to_string(repo.path().join(".git/info/exclude")).unwrap();
        assert_eq!(entry_count(&exclude, "/.mcp.json"), 1);
        assert_eq!(entry_count(&exclude, "/CLAUDE.md"), 0);
        assert_eq!(entry_count(&exclude, "/ignored.json"), 0);
        assert_eq!(
            fs::read_to_string(repo.path().join(".gitignore")).unwrap(),
            "/ignored.json\n"
        );
    }

    #[test]
    fn checkout_identity_fields_are_recognised() {
        assert!(holds_checkout_identity(
            &serde_json::json!({"workspace_id": "w", "checkout_root": "/x"})
        ));
        assert!(!holds_checkout_identity(
            &serde_json::json!({"checkout_snapshot": false, "git_capture": {"enabled": false}})
        ));
    }
}
