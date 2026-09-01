//! Shared ingest-root containment guard (P0 ingestion-containment).
//!
//! A single chokepoint that decides whether a directory is a safe *root* to
//! index/ingest. It REJECTS — unless an explicit opt-in is supplied — roots
//! that are dangerously broad or sensitive:
//!   - the filesystem root `/` (or a drive root like `C:\`)
//!   - the user's `$HOME` directory itself
//!   - any ancestor of `$HOME` (e.g. `/home`, `/Users`)
//!   - known sensitive directories (`.ssh`, `.aws`, `.gnupg`, `.kube`,
//!     `.docker`, `.config`) and anything inside them
//!   - OS/system directories (`/etc`, `/proc`, `/sys`, `/dev`, `/boot`,
//!     `/root`) and anything inside them
//!
//! It also *warns* (without failing) when a candidate root lacks a recognised
//! repository marker (`.git`, `Cargo.toml`, `package.json`, `go.mod`,
//! `pyproject.toml`), since such roots are usually selected by accident.
//!
//! Both `mcp-server` and `mcp-tools` depend on `mcp-client`, so the guard lives
//! here to give every ingest entry point one shared implementation.

use std::path::{Component, Path, PathBuf};

/// Process-wide environment override that opts every ingest root out of the
/// breadth/sensitivity guard. Intended for deliberate, operator-driven bulk
/// ingestion of an unusual root.
pub const ALLOW_BROAD_INGEST_ENV: &str = "CONTEXTSTREAM_ALLOW_BROAD_INGEST";

/// Repository markers that signal a directory is a real project root.
pub const REPO_MARKERS: [&str; 5] = [
    ".git",
    "Cargo.toml",
    "package.json",
    "go.mod",
    "pyproject.toml",
];

/// Directory names that are sensitive enough that ingesting them — or anything
/// inside them — requires an explicit opt-in.
pub const SENSITIVE_DIR_NAMES: [&str; 6] =
    [".ssh", ".aws", ".gnupg", ".kube", ".docker", ".config"];

/// Absolute OS/system directories that are never a legitimate project root (they
/// hold OS state and credentials, not project sources). Matched against both the
/// supplied and canonical path. Deliberately excludes `/var` and `/usr` because
/// legitimate trees live there (e.g. `/var/www`). Unix-oriented; a harmless
/// no-op on Windows.
pub const SYSTEM_ROOTS: [&str; 6] = ["/etc", "/proc", "/sys", "/dev", "/boot", "/root"];

/// Directories that hold user home directories. Rejected as over-broad even when
/// `home_dir()` cannot be resolved (e.g. HOME unset under systemd/containers/cron),
/// where the dynamic home-ancestor check would otherwise not fire. Only the bare
/// parent dirs themselves are rejected — a real home like `/home/alice` is a
/// legitimate place to keep projects.
pub const HOME_PARENT_ROOTS: [&str; 2] = ["/home", "/Users"];

/// Why a candidate ingest root was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestRootRejectionReason {
    /// The path is the filesystem root (`/` or a drive root).
    FilesystemRoot,
    /// The path is exactly the user's home directory.
    HomeDirectory,
    /// The path is an ancestor of the user's home directory.
    HomeAncestor,
    /// The path is, or lives inside, a known sensitive directory.
    SensitiveDirectory(String),
    /// The path is, or lives inside, an OS/system directory (`/etc`, `/proc`, …).
    SystemDirectory(String),
}

impl IngestRootRejectionReason {
    fn describe(&self) -> String {
        match self {
            Self::FilesystemRoot => "it is the filesystem root".to_string(),
            Self::HomeDirectory => "it is your home directory".to_string(),
            Self::HomeAncestor => "it is an ancestor of your home directory".to_string(),
            Self::SensitiveDirectory(name) => {
                format!("it is, or lives inside, a sensitive directory ({name})")
            }
            Self::SystemDirectory(name) => {
                format!("it is, or lives inside, a system directory ({name})")
            }
        }
    }
}

/// A rejected ingest root, with a human-readable, actionable message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngestRootRejection {
    /// The path (normalized when possible) that was rejected.
    pub path: PathBuf,
    /// Why it was rejected.
    pub reason: IngestRootRejectionReason,
}

impl IngestRootRejection {
    /// Full actionable message, including how to opt in deliberately.
    pub fn message(&self) -> String {
        format!(
            "Refusing to ingest '{}' because {}. This root is too broad or sensitive to index. \
             Point ingestion at a specific project subdirectory, or — if this is intentional — \
             opt in explicitly (set {}=1).",
            self.path.display(),
            self.reason.describe(),
            ALLOW_BROAD_INGEST_ENV
        )
    }
}

impl std::fmt::Display for IngestRootRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message())
    }
}

impl std::error::Error for IngestRootRejection {}

/// Options controlling the ingest-root guard.
#[derive(Debug, Clone, Default)]
pub struct IngestRootOptions {
    /// When true, broad/sensitive roots (`$HOME`, ancestors, sensitive dirs,
    /// filesystem root) are permitted instead of rejected. This is the explicit
    /// opt-in that legitimate out-of-root ingestion uses.
    pub allow_broad_root: bool,
}

impl IngestRootOptions {
    /// Strict defaults: reject broad/sensitive roots.
    pub fn strict() -> Self {
        Self {
            allow_broad_root: false,
        }
    }

    /// Opt-in derived from the process environment ([`ALLOW_BROAD_INGEST_ENV`]).
    /// Use for callers that want the env override to apply but otherwise stay
    /// strict.
    pub fn from_env() -> Self {
        Self {
            allow_broad_root: broad_ingest_opt_in_from_env(),
        }
    }

    /// OR the environment opt-in into these options.
    pub fn with_env_opt_in(mut self) -> Self {
        self.allow_broad_root = self.allow_broad_root || broad_ingest_opt_in_from_env();
        self
    }
}

/// Whether the env override opts broad ingestion in. Truthy values: `1`,
/// `true`, `yes`, `on` (case-insensitive).
pub fn broad_ingest_opt_in_from_env() -> bool {
    std::env::var(ALLOW_BROAD_INGEST_ENV)
        .ok()
        .map(|v| is_truthy(&v))
        .unwrap_or(false)
}

fn is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

/// Outcome of a *successful* guard check — the root is allowed, but may carry
/// non-fatal warnings the caller should surface or log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IngestRootAssessment {
    /// Non-fatal warnings (e.g. a missing repository marker).
    pub warnings: Vec<String>,
}

impl IngestRootAssessment {
    pub fn has_warnings(&self) -> bool {
        !self.warnings.is_empty()
    }
}

/// Validate that `path` is a safe *root* to ingest/index.
///
/// Returns `Ok(assessment)` when allowed (the assessment may carry non-fatal
/// warnings), or `Err(rejection)` when the root is too broad/sensitive and no
/// opt-in was supplied. The check canonicalizes `path` and `$HOME` when
/// possible and falls back to the given path otherwise, so it works for both
/// existing and not-yet-created roots. Sensitivity is checked against both the
/// caller-supplied path and its canonical form so a symlinked sensitive dir
/// (e.g. `~/.ssh` → elsewhere) cannot slip through.
pub fn validate_ingest_root(
    path: &Path,
    opts: &IngestRootOptions,
) -> Result<IngestRootAssessment, IngestRootRejection> {
    let normalized = normalize(path);

    // Hard structural/sensitivity checks (bypassable only via opt-in).
    if let Some(reason) = classify_breadth(path, &normalized) {
        if !opts.allow_broad_root {
            return Err(IngestRootRejection {
                path: normalized,
                reason,
            });
        }
    }

    // Soft check: warn when no repository marker is present.
    let mut assessment = IngestRootAssessment::default();
    if !has_repo_marker(&normalized) {
        assessment.warnings.push(format!(
            "'{}' has no repository marker ({}); ingesting it as a root may pull in unrelated files.",
            normalized.display(),
            REPO_MARKERS.join(", ")
        ));
    }
    Ok(assessment)
}

/// Classify a path's breadth/sensitivity, if any. `original` is the
/// caller-supplied path; `normalized` is its canonical form (or a fallback).
fn classify_breadth(original: &Path, normalized: &Path) -> Option<IngestRootRejectionReason> {
    // Filesystem root (no parent) — also covers drive roots like `C:\`.
    if normalized.parent().is_none() {
        return Some(IngestRootRejectionReason::FilesystemRoot);
    }

    // OS/system directory (e.g. `/etc`, `/root`) — never a project root. Checked
    // against both the supplied and canonical path so a typed `/etc` is caught
    // even where it canonicalizes elsewhere (e.g. macOS `/private/etc`).
    if let Some(name) = system_root_match(original).or_else(|| system_root_match(normalized)) {
        return Some(IngestRootRejectionReason::SystemDirectory(name));
    }

    // Parent-of-homes (e.g. /home, /Users): never a project root, and caught
    // here even when home_dir() can't be resolved (HOME unset), where the
    // dynamic ancestor check below would not fire. Bare parent dirs only.
    if home_parent_match(original) || home_parent_match(normalized) {
        return Some(IngestRootRejectionReason::HomeAncestor);
    }

    // Sensitive directory: any component (in either the original or canonical
    // form) matches a sensitive name.
    if let Some(name) = sensitive_component(original).or_else(|| sensitive_component(normalized)) {
        return Some(IngestRootRejectionReason::SensitiveDirectory(name));
    }

    // Home directory / ancestor checks.
    if let Some(home) = normalized_home() {
        if normalized == home {
            return Some(IngestRootRejectionReason::HomeDirectory);
        }
        // `normalized` is an ancestor of home when home starts with it.
        if home.starts_with(normalized) {
            return Some(IngestRootRejectionReason::HomeAncestor);
        }
    }

    None
}

fn system_root_match(path: &Path) -> Option<String> {
    for root in SYSTEM_ROOTS {
        let rp = Path::new(root);
        if path == rp || path.starts_with(rp) {
            return Some(root.to_string());
        }
    }
    None
}

fn home_parent_match(path: &Path) -> bool {
    HOME_PARENT_ROOTS.iter().any(|root| path == Path::new(root))
}

fn sensitive_component(path: &Path) -> Option<String> {
    for component in path.components() {
        if let Component::Normal(os) = component {
            if let Some(name) = os.to_str() {
                if SENSITIVE_DIR_NAMES.contains(&name) {
                    return Some(name.to_string());
                }
            }
        }
    }
    None
}

fn has_repo_marker(path: &Path) -> bool {
    REPO_MARKERS.iter().any(|m| path.join(m).exists())
}

/// Canonicalize when possible; otherwise return the path as-is. Keeps
/// not-yet-created roots usable for the lexical checks.
fn normalize(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn normalized_home() -> Option<PathBuf> {
    dirs::home_dir().map(|h| std::fs::canonicalize(&h).unwrap_or(h))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        let h = dirs::home_dir().expect("home dir available in test env");
        std::fs::canonicalize(&h).unwrap_or(h)
    }

    #[test]
    fn rejects_home_directory() {
        // Serialize against other tests that mutate `HOME` (e.g. client.rs index
        // marker tests) so `home()` resolves the real home, not a temp override.
        let _env = crate::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let err = validate_ingest_root(&home(), &IngestRootOptions::strict())
            .expect_err("home dir must be rejected");
        assert_eq!(err.reason, IngestRootRejectionReason::HomeDirectory);
        assert!(err.message().contains(ALLOW_BROAD_INGEST_ENV));
    }

    #[test]
    fn rejects_home_ancestor() {
        // See rejects_home_directory: hold the shared env lock while reading HOME.
        let _env = crate::test_env_lock()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let ancestor = home().parent().expect("home has a parent").to_path_buf();
        let err = validate_ingest_root(&ancestor, &IngestRootOptions::strict())
            .expect_err("ancestor of home must be rejected");
        // Could be FilesystemRoot if home is one level below `/`; otherwise
        // HomeAncestor. Both are valid rejections of an over-broad root.
        assert!(matches!(
            err.reason,
            IngestRootRejectionReason::HomeAncestor | IngestRootRejectionReason::FilesystemRoot
        ));
    }

    #[test]
    fn rejects_filesystem_root() {
        let err = validate_ingest_root(Path::new("/"), &IngestRootOptions::strict())
            .expect_err("filesystem root must be rejected");
        assert_eq!(err.reason, IngestRootRejectionReason::FilesystemRoot);
    }

    #[test]
    fn rejects_home_parent_roots() {
        // Bare parent-of-homes dirs are rejected as over-broad. /Users does not
        // exist on Linux CI, which also exercises the lexical-fallback path.
        for parent in ["/home", "/Users"] {
            let err = validate_ingest_root(Path::new(parent), &IngestRootOptions::strict())
                .expect_err("parent-of-homes must be rejected");
            assert_eq!(err.reason, IngestRootRejectionReason::HomeAncestor);
        }
        // Opt-in still bypasses.
        assert!(validate_ingest_root(
            Path::new("/home"),
            &IngestRootOptions {
                allow_broad_root: true,
            },
        )
        .is_ok());
    }

    #[test]
    fn rejects_sensitive_directory() {
        let ssh = home().join(".ssh");
        let err = validate_ingest_root(&ssh, &IngestRootOptions::strict())
            .expect_err("sensitive dir must be rejected");
        assert_eq!(
            err.reason,
            IngestRootRejectionReason::SensitiveDirectory(".ssh".to_string())
        );
    }

    #[test]
    fn rejects_path_inside_sensitive_directory() {
        let inside = home().join(".aws").join("cli").join("cache");
        let err = validate_ingest_root(&inside, &IngestRootOptions::strict())
            .expect_err("path inside a sensitive dir must be rejected");
        assert_eq!(
            err.reason,
            IngestRootRejectionReason::SensitiveDirectory(".aws".to_string())
        );
    }

    #[test]
    fn rejects_system_directory() {
        let err = validate_ingest_root(Path::new("/etc"), &IngestRootOptions::strict())
            .expect_err("/etc must be rejected");
        assert_eq!(
            err.reason,
            IngestRootRejectionReason::SystemDirectory("/etc".to_string())
        );
        // The remaining system roots are rejected too (the precise reason may be
        // HomeDirectory for `/root` when the tests run as root, so only assert
        // rejection here).
        for sys in ["/proc", "/sys", "/dev", "/boot"] {
            assert!(
                validate_ingest_root(Path::new(sys), &IngestRootOptions::strict()).is_err(),
                "system dir {sys} must be rejected"
            );
        }
        // A path *inside* a system dir is rejected as well.
        let err = validate_ingest_root(Path::new("/etc/ssl/private"), &IngestRootOptions::strict())
            .expect_err("path inside /etc must be rejected");
        assert_eq!(
            err.reason,
            IngestRootRejectionReason::SystemDirectory("/etc".to_string())
        );
    }

    #[test]
    fn allows_normal_project_dir() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\n").expect("write marker");
        let assessment = validate_ingest_root(dir.path(), &IngestRootOptions::strict())
            .expect("normal project dir must be allowed");
        assert!(
            !assessment.has_warnings(),
            "dir with a repo marker should not warn: {:?}",
            assessment.warnings
        );
    }

    #[test]
    fn warns_when_no_repo_marker() {
        let dir = tempfile::tempdir().expect("tempdir");
        let assessment = validate_ingest_root(dir.path(), &IngestRootOptions::strict())
            .expect("marker-less dir is allowed, only warned");
        assert!(
            assessment.has_warnings(),
            "marker-less dir should produce a warning"
        );
    }

    #[test]
    fn opt_in_bypasses_rejection() {
        let opts = IngestRootOptions {
            allow_broad_root: true,
        };
        // Home, filesystem root, a sensitive dir, and a system dir are all
        // permitted with opt-in.
        assert!(validate_ingest_root(&home(), &opts).is_ok());
        assert!(validate_ingest_root(Path::new("/"), &opts).is_ok());
        assert!(validate_ingest_root(&home().join(".ssh"), &opts).is_ok());
        assert!(validate_ingest_root(Path::new("/etc"), &opts).is_ok());
    }

    #[test]
    fn env_opt_in_parsing() {
        assert!(is_truthy("1"));
        assert!(is_truthy("TRUE"));
        assert!(is_truthy(" yes "));
        assert!(is_truthy("on"));
        assert!(!is_truthy("0"));
        assert!(!is_truthy("false"));
        assert!(!is_truthy(""));
    }
}
