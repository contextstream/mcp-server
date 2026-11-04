//! A checkout snapshot: exactly what a checkout holds on top of its last commit.
//!
//! It is HEAD's tree id, plus every changed, added and deleted path with the
//! blob ids git gives the files, so a service can rebuild the checkout's tree
//! from what it already holds and ask only for the blobs it lacks. A tool
//! that reads a person's local changes (a review of them, a check of them)
//! starts from one. Nothing here sends anything: a caller takes a snapshot,
//! gives its fields to the service it was told to use, and answers that
//! service's requests for the base tree's listing and for missing objects.
//!
//! What is in it matches `git add -A`: tracked files that changed, untracked
//! files that `.gitignore` and the repository's other exclude files leave in,
//! and deleted tracked files. Untracked `node_modules` folders anywhere and a
//! root `target` folder that is Cargo's (it holds `CACHEDIR.TAG`) are left out
//! even when nothing ignores them: they are installs and build output, not
//! the person's work.
//!
//! What a snapshot never does:
//! - It does not write to the repository. New blobs and trees go into a
//!   private temporary object store that reads the repository's objects, and
//!   the target tree is built in a scratch index.
//! - It does not hold back the committed base: that is in the repository's
//!   history already. It does withhold what is not committed yet and looks
//!   like a credential (a secret file name, or a private key or service
//!   account inside the file) or that `.contextignore` and
//!   `.contextstream/ignore` list; those paths are named in
//!   [`CheckoutSnapshot::withheld`], and the target tree is built without them.
//! - It does not answer for an object it does not hold: a request is
//!   answered only for blobs of the base tree and of the changes.
//!
//! Whether a snapshot may be taken is the person's call: see [`refusal`].

use crate::client::{local_files_denied, ContextStreamClient};
use flate2::{write::ZlibEncoder, Compression};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Git's tree with nothing in it: the base of a repository with no commit.
pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// The most a message to a service carries; bigger data goes in pieces of this size.
pub const PIECE_BYTES_MAX: usize = 1 << 20;

/// Set to `off` to never take a snapshot.
pub const ENV_SWITCH: &str = "CONTEXTSTREAM_CHECKOUT_SNAPSHOT";

/// Repositories that may be snapshotted, as prefixes of their `origin` URL or
/// of their folder's name, comma separated. Unset allows every repository.
pub const ENV_REPOS: &str = "CONTEXTSTREAM_CHECKOUT_SNAPSHOT_REPOS";

/// Project setting: `"checkout_snapshot": false` in `.contextstream/config.json`
/// turns snapshots off for the project, for everyone who works in it.
pub const PROJECT_SWITCH_KEY: &str = "checkout_snapshot";

/// The most a snapshot may carry; a checkout over them is not snapshotted.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Changed and added files.
    pub files: usize,
    /// Their bytes together.
    pub total_bytes: u64,
    /// One file's bytes.
    pub file_bytes: u64,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            files: 4000,
            total_bytes: 64 << 20,
            file_bytes: 16 << 20,
        }
    }
}

/// Why a snapshot could not be taken.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    /// The settings say no snapshot of this checkout may be taken.
    #[error("{0}")]
    Refused(String),
    /// The folder is not in a git repository.
    #[error("{0} is not inside a git repository")]
    NotARepository(String),
    /// The changes are over a limit.
    #[error("{0}")]
    TooLarge(String),
    /// A request named an object the snapshot does not hold.
    #[error("the request names an object this snapshot does not hold")]
    NotHeld,
    /// Git failed.
    #[error("git {command}: {detail}")]
    Git { command: String, detail: String },
    /// The file system failed.
    #[error("{0}")]
    Io(String),
}

/// One changed or added path with its blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// `100644`, `100755` or `120000`.
    pub mode: String,
    /// The blob's id.
    pub oid: String,
    /// The path from the repository's root, with `/`.
    pub path: String,
}

/// A path left out of a snapshot, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Withheld {
    pub path: String,
    pub reason: String,
}

/// What decides whether a snapshot may be taken.
#[derive(Debug, Default, Clone)]
pub struct Settings<'a> {
    /// The hosted gateway has no local files to give.
    pub local_files_denied: bool,
    /// [`ENV_SWITCH`].
    pub env_switch: Option<&'a str>,
    /// [`ENV_REPOS`].
    pub env_repos: Option<&'a str>,
    /// [`PROJECT_SWITCH_KEY`] in the repository's `.contextstream/config.json`.
    pub project_switch: Option<bool>,
    /// The repository's `origin` URL, if it has one.
    pub origin: &'a str,
    /// The repository's folder name.
    pub folder_name: &'a str,
}

/// Why `settings` allow no snapshot, or `None` when they allow one.
pub fn refusal(settings: &Settings<'_>) -> Option<String> {
    if settings.local_files_denied {
        return Some("this connection has no local files to snapshot".to_string());
    }
    if let Some(value) = settings.env_switch {
        if matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "off" | "0" | "false" | "no" | "disabled"
        ) {
            return Some(format!("{ENV_SWITCH} is off"));
        }
    }
    if settings.project_switch == Some(false) {
        return Some(format!(
            "this project turns checkout snapshots off ({PROJECT_SWITCH_KEY} in .contextstream/config.json)"
        ));
    }
    if let Some(list) = settings.env_repos.map(str::trim).filter(|v| !v.is_empty()) {
        let allowed = list
            .split(|c: char| c == ',' || c.is_whitespace())
            .filter(|prefix| !prefix.is_empty())
            .any(|prefix| {
                (!settings.origin.is_empty() && settings.origin.starts_with(prefix))
                    || settings.folder_name.starts_with(prefix)
            });
        if !allowed {
            return Some(format!("this repository is not one {ENV_REPOS} allows"));
        }
    }
    None
}

/// A snapshot of a checkout. It keeps a private object store alive; drop it
/// when its service is done.
pub struct CheckoutSnapshot {
    /// The repository's top folder.
    pub root: PathBuf,
    /// The folder the snapshot was taken from, relative to `root` (`""` for the root).
    pub folder: String,
    /// The repository's folder name, as it is.
    pub repo_name: String,
    /// [`origin_digest`] of its `origin` URL.
    pub origin_sha256: String,
    /// HEAD's tree id ([`EMPTY_TREE`] before the first commit).
    pub base_tree: String,
    /// The tree of the checkout without the paths in `withheld`.
    pub target_tree: String,
    pub overlay: Vec<Entry>,
    pub deletes: Vec<String>,
    pub withheld: Vec<Withheld>,
    git: GitEnv,
    _objects: tempfile::TempDir,
}

/// What every git command of a snapshot runs with.
#[derive(Clone)]
struct GitEnv {
    repo: PathBuf,
    /// The private store new objects go to, and the repository's own, read through it.
    objects: Vec<(String, String)>,
}

async fn git(
    dir: &Path,
    env: &[(String, String)],
    stdin: Option<Vec<u8>>,
    args: &[&str],
) -> Result<Vec<u8>, SnapshotError> {
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(dir)
        // The environment a hook or a wrapper left must not point git elsewhere.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_COMMON_DIR")
        // Reading the checkout never takes its index lock.
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for (name, value) in env {
        command.env(name, value);
    }
    let failed = |detail: String| SnapshotError::Git {
        command: args.first().copied().unwrap_or("").to_string(),
        detail,
    };
    let mut child = command.spawn().map_err(|e| failed(e.to_string()))?;
    if let Some(data) = stdin {
        if let Some(mut pipe) = child.stdin.take() {
            // Written beside the reading, so a big list cannot fill both pipes.
            tokio::spawn(async move {
                let _ = pipe.write_all(&data).await;
                let _ = pipe.shutdown().await;
            });
        }
    }
    let output = child
        .wait_with_output()
        .await
        .map_err(|e| failed(e.to_string()))?;
    if !output.status.success() {
        let text = String::from_utf8_lossy(&output.stderr);
        let text = text.trim();
        let tail: String = text
            .chars()
            .rev()
            .take(200)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        return Err(failed(tail));
    }
    Ok(output.stdout)
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_string()
}

fn is_oid(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The lowercase hex SHA-256 of a repository's trimmed `origin` URL (of the empty
/// string without one). A digest names the repository without putting a URL,
/// which may carry credentials, on the wire.
pub fn origin_digest(origin: &str) -> String {
    hex::encode(Sha256::digest(origin.trim().as_bytes()))
}

/// Pathspecs for the untracked folders an installer or a build makes itself.
fn left_to_the_service(repo: &Path) -> Vec<String> {
    let mut specs = vec![":(exclude,glob)**/node_modules/**".to_string()];
    if repo.join("target").join("CACHEDIR.TAG").is_file() {
        specs.push(":(exclude,glob)target/**".to_string());
    }
    specs
}

/// The credential-looking reason to keep an uncommitted file out, if any.
fn secret_reason(path: &str, full: &Path, size: u64) -> Option<String> {
    let name = path.rsplit('/').next().unwrap_or(path);
    if ContextStreamClient::is_secret_file_name(name) {
        return Some("a file name that holds credentials".to_string());
    }
    if size == 0 || size > (1 << 20) {
        return None;
    }
    let bytes = std::fs::read(full).ok()?;
    let head = &bytes[..bytes.len().min(256 << 10)];
    let content = std::str::from_utf8(head).ok()?;
    ContextStreamClient::secret_content_reason(content).map(|what| format!("it holds {what}"))
}

/// `.contextignore` files (in any folder) and `.contextstream/ignore` (at the
/// root): what the person asked ContextStream never to read.
struct PrivacyIgnore {
    root: PathBuf,
    project: Option<Gitignore>,
    project_unreadable: bool,
    nested: HashMap<PathBuf, Option<Gitignore>>,
    unreadable: BTreeSet<PathBuf>,
}

impl PrivacyIgnore {
    fn new(root: &Path) -> Self {
        // Its patterns are relative to the project's root, not to its folder.
        let file = root.join(".contextstream").join("ignore");
        let mut project_unreadable = false;
        let project = file.is_file().then(|| {
            let mut builder = GitignoreBuilder::new(root);
            // Fails closed: what a broken ignore file lists cannot be known.
            project_unreadable = builder.add(&file).is_some();
            builder.build().unwrap_or_else(|_| {
                project_unreadable = true;
                Gitignore::empty()
            })
        });
        Self {
            root: root.to_path_buf(),
            project,
            project_unreadable,
            nested: HashMap::new(),
            unreadable: BTreeSet::new(),
        }
    }

    fn matcher_in(&mut self, dir: &Path) -> Option<&Gitignore> {
        if !self.nested.contains_key(dir) {
            let file = dir.join(".contextignore");
            let loaded = file.is_file().then(|| {
                let (matcher, error) = Gitignore::new(&file);
                if error.is_some() {
                    // Fails closed: what a broken ignore file lists cannot be known.
                    self.unreadable.insert(dir.to_path_buf());
                }
                matcher
            });
            self.nested.insert(dir.to_path_buf(), loaded);
        }
        self.nested.get(dir).and_then(Option::as_ref)
    }

    /// Why `relative` is not for ContextStream to read, if it is not.
    fn reason(&mut self, relative: &str) -> Option<String> {
        let full = self.root.join(relative);
        if self.project_unreadable {
            return Some("its .contextstream/ignore cannot be read".to_string());
        }
        if let Some(project) = &self.project {
            if project
                .matched_path_or_any_parents(&full, false)
                .is_ignore()
            {
                return Some("it is listed in .contextstream/ignore".to_string());
            }
        }
        let mut dir = self.root.clone();
        let mut dirs = vec![dir.clone()];
        if let Some(parent) = Path::new(relative).parent() {
            for part in parent.components() {
                dir.push(part);
                dirs.push(dir.clone());
            }
        }
        for dir in dirs {
            let ignored = self
                .matcher_in(&dir)
                .map(|m| m.matched_path_or_any_parents(&full, false).is_ignore());
            if self.unreadable.contains(&dir) {
                return Some("its .contextignore cannot be read".to_string());
            }
            if ignored == Some(true) {
                return Some("it is listed in .contextignore".to_string());
            }
        }
        None
    }
}

/// The first, cheap part of a snapshot: whose checkout it is, and that the
/// person's switches allow one. See [`CheckoutSnapshot::begin`].
#[derive(Debug)]
pub struct SnapshotStart {
    root: PathBuf,
    folder: String,
    repo_name: String,
    origin_sha256: String,
    limits: Limits,
}

impl SnapshotStart {
    /// The repository's top folder.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The folder the snapshot was asked of, relative to [`SnapshotStart::root`] (`""` for the root).
    pub fn folder(&self) -> &str {
        &self.folder
    }

    /// `{"repo": {"name", "origin_sha256"}}`: who the repository is and nothing
    /// else of it, for a call that carries no more than that.
    pub fn identity(&self) -> Value {
        identity_of(&self.repo_name, &self.origin_sha256)
    }

    /// The rest of the snapshot: the two trees, the changes and what was withheld.
    pub async fn complete(self) -> Result<CheckoutSnapshot, SnapshotError> {
        let SnapshotStart {
            root: repo,
            folder,
            repo_name,
            origin_sha256,
            limits,
        } = self;
        let (real_objects, head) = tokio::join!(
            git(
                &repo,
                &[],
                None,
                &[
                    "rev-parse",
                    "--path-format=absolute",
                    "--git-path",
                    "objects",
                ],
            ),
            git(
                &repo,
                &[],
                None,
                &["rev-parse", "-q", "--verify", "HEAD^{tree}"],
            ),
        );
        let real_objects = PathBuf::from(text(&real_objects?));
        let base_tree = match head {
            Ok(out) if is_oid(&text(&out)) => text(&out),
            _ => EMPTY_TREE.to_string(),
        };
        let temp = tempfile::tempdir().map_err(|e| SnapshotError::Io(e.to_string()))?;
        let private_objects = temp.path().join("objects");
        std::fs::create_dir_all(&private_objects).map_err(|e| SnapshotError::Io(e.to_string()))?;
        let env = GitEnv {
            repo: repo.clone(),
            objects: vec![
                (
                    "GIT_OBJECT_DIRECTORY".to_string(),
                    private_objects.to_string_lossy().to_string(),
                ),
                (
                    "GIT_ALTERNATE_OBJECT_DIRECTORIES".to_string(),
                    real_objects.to_string_lossy().to_string(),
                ),
            ],
        };

        // The scratch index is made while git lists the changes; a checkout with
        // none needs neither it nor a tree of its own: its tree is its base.
        let (prepared, listed) = tokio::join!(
            prepare_scratch_index(&env, temp.path(), &base_tree),
            overlay(&env, limits),
        );
        let (overlay, deletes, withheld) = listed?;
        let target_tree = if overlay.is_empty() && deletes.is_empty() {
            base_tree.clone()
        } else {
            prepared?;
            finish_target_tree(&env, temp.path(), &overlay, &deletes).await?
        };
        Ok(CheckoutSnapshot {
            root: repo,
            folder,
            repo_name,
            origin_sha256,
            base_tree,
            target_tree,
            overlay,
            deletes,
            withheld,
            git: env,
            _objects: temp,
        })
    }
}

/// `{"repo": {"name", "origin_sha256"}}`.
fn identity_of(repo_name: &str, origin_sha256: &str) -> Value {
    json!({ "repo": {"name": repo_name, "origin_sha256": origin_sha256} })
}

impl CheckoutSnapshot {
    /// Take a snapshot of the checkout `folder` is in, within the default [`Limits`].
    pub async fn capture(folder: &Path) -> Result<Self, SnapshotError> {
        Self::capture_with(folder, Limits::default()).await
    }

    /// Take a snapshot within `limits`.
    pub async fn capture_with(folder: &Path, limits: Limits) -> Result<Self, SnapshotError> {
        Self::begin_with(folder, limits).await?.complete().await
    }

    /// Begin a snapshot of the checkout `folder` is in: say whose checkout it is
    /// (a few quick git questions and the person's switches), so that a caller
    /// that can use that early may start its own work, and finish the snapshot
    /// with [`SnapshotStart::complete`] meanwhile.
    pub async fn begin(folder: &Path) -> Result<SnapshotStart, SnapshotError> {
        Self::begin_with(folder, Limits::default()).await
    }

    /// [`CheckoutSnapshot::begin`] within `limits`.
    pub async fn begin_with(folder: &Path, limits: Limits) -> Result<SnapshotStart, SnapshotError> {
        let shown = folder.display().to_string();
        let top = git(folder, &[], None, &["rev-parse", "--show-toplevel"])
            .await
            .map_err(|_| SnapshotError::NotARepository(shown.clone()))?;
        let repo = PathBuf::from(text(&top));
        let origin = git(&repo, &[], None, &["config", "--get", "remote.origin.url"])
            .await
            .map(|out| text(&out))
            .unwrap_or_default();
        let folder_name = repo
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let project_switch =
            std::fs::read_to_string(repo.join(".contextstream").join("config.json"))
                .ok()
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                .and_then(|config| config.get(PROJECT_SWITCH_KEY).and_then(Value::as_bool));
        let env_switch = std::env::var(ENV_SWITCH).ok();
        let env_repos = std::env::var(ENV_REPOS).ok();
        if let Some(reason) = refusal(&Settings {
            local_files_denied: local_files_denied(),
            env_switch: env_switch.as_deref(),
            env_repos: env_repos.as_deref(),
            project_switch,
            origin: &origin,
            folder_name: &folder_name,
        }) {
            return Err(SnapshotError::Refused(reason));
        }

        let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let relative = canonical(folder)
            .strip_prefix(canonical(&repo))
            .map(Path::to_path_buf)
            .map_err(|_| SnapshotError::NotARepository(shown))?;
        let relative = relative.to_string_lossy().replace('\\', "/");
        let relative = if relative == "." {
            String::new()
        } else {
            relative
        };
        Ok(SnapshotStart {
            root: repo,
            folder: relative,
            repo_name: folder_name,
            origin_sha256: origin_digest(&origin),
            limits,
        })
    }

    /// The fields a service's request is completed with: what it needs to
    /// rebuild the checkout's tree. `overlay` rows are `[mode, oid, path]`.
    /// The rest of the request (the command, the folder, the repository's key)
    /// is the service's own to say.
    pub fn request_fields(&self) -> Map<String, Value> {
        let mut fields = Map::new();
        fields.insert("base_tree".into(), json!(self.base_tree));
        fields.insert("target_tree".into(), json!(self.target_tree));
        fields.insert(
            "overlay".into(),
            Value::Array(
                self.overlay
                    .iter()
                    .map(|e| json!([e.mode, e.oid, e.path]))
                    .collect(),
            ),
        );
        fields.insert("deletes".into(), json!(self.deletes));
        // Output then names this checkout's files, not the service's copy of them.
        fields.insert("root".into(), json!(self.root.to_string_lossy()));
        fields
    }

    /// The `checkout` of a call to a service that asked for a snapshot: who
    /// the repository is (its name and a digest of its origin, never a URL),
    /// the two trees, how much changed, and what was withheld. No file
    /// content and no path of a change is in it.
    pub fn attachment(&self) -> Value {
        json!({
            "repo": {"name": self.repo_name, "origin_sha256": self.origin_sha256},
            "base_tree": self.base_tree,
            "target_tree": self.target_tree,
            "changed": self.overlay.len(),
            "deleted": self.deletes.len(),
            "withheld": self.withheld_summary(),
        })
    }

    /// `{"repo": {"name", "origin_sha256"}}`: the same as [`SnapshotStart::identity`].
    pub fn identity(&self) -> Value {
        identity_of(&self.repo_name, &self.origin_sha256)
    }

    /// What was left out and why, for the person or the service to be told.
    pub fn withheld_summary(&self) -> Value {
        Value::Array(
            self.withheld
                .iter()
                .map(|w| json!({"path": w.path, "reason": w.reason}))
                .collect(),
        )
    }

    /// A git pack of exactly the blobs of the changes.
    pub async fn initial_pack(&self) -> Result<Vec<u8>, SnapshotError> {
        let oids: Vec<String> = self.overlay.iter().map(|e| e.oid.clone()).collect();
        self.pack(&oids).await
    }

    /// Answer a service's request: `{"listing": true}` for the base tree's
    /// listing (zlib-compressed `git ls-tree -r -z --full-tree`), or
    /// `{"blobs": [oid, …]}` for a git pack of those blobs. A blob that is
    /// neither in the base tree nor in the changes is refused.
    pub async fn answer_need(&self, need: &Value) -> Result<Vec<u8>, SnapshotError> {
        if need.get("listing").and_then(Value::as_bool) == Some(true) {
            let listing = self.listing().await?;
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
            encoder
                .write_all_bytes(&listing)
                .map_err(|e| SnapshotError::Io(e.to_string()))?;
            return encoder
                .finish()
                .map_err(|e| SnapshotError::Io(e.to_string()));
        }
        let wanted: Vec<String> = need
            .get("blobs")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|oid| is_oid(oid))
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        if wanted.is_empty() {
            return Ok(Vec::new());
        }
        let mut held: BTreeSet<String> = self.overlay.iter().map(|e| e.oid.clone()).collect();
        let base = self.base_blobs().await?;
        held.extend(base);
        if wanted.iter().any(|oid| !held.contains(oid)) {
            return Err(SnapshotError::NotHeld);
        }
        self.pack(&wanted).await
    }

    async fn listing(&self) -> Result<Vec<u8>, SnapshotError> {
        git(
            &self.git.repo,
            &self.git.objects,
            None,
            &["ls-tree", "-r", "-z", "--full-tree", &self.base_tree],
        )
        .await
    }

    async fn base_blobs(&self) -> Result<Vec<String>, SnapshotError> {
        let rows = self.listing().await?;
        Ok(rows
            .split(|b| *b == 0)
            .filter_map(|row| {
                // "<mode> blob <oid>\t<path>"
                let head = row.split(|b| *b == b'\t').next()?;
                let head = std::str::from_utf8(head).ok()?;
                let oid = head.split(' ').nth(2)?;
                is_oid(oid).then(|| oid.to_string())
            })
            .collect())
    }

    /// No delta search: the blobs are new, and the time matters more than the bytes.
    async fn pack(&self, oids: &[String]) -> Result<Vec<u8>, SnapshotError> {
        if oids.is_empty() {
            return Ok(Vec::new());
        }
        git(
            &self.git.repo,
            &self.git.objects,
            Some((oids.join("\n") + "\n").into_bytes()),
            &["pack-objects", "--stdout", "-q", "--window=0"],
        )
        .await
    }
}

/// `data` in pieces of at most [`PIECE_BYTES_MAX`] bytes, for a writer to a
/// service whose messages are limited.
pub fn in_pieces(data: &[u8]) -> std::slice::Chunks<'_, u8> {
    data.chunks(PIECE_BYTES_MAX)
}

trait WriteAll {
    fn write_all_bytes(&mut self, data: &[u8]) -> std::io::Result<()>;
}

impl WriteAll for ZlibEncoder<Vec<u8>> {
    fn write_all_bytes(&mut self, data: &[u8]) -> std::io::Result<()> {
        use std::io::Write;
        self.write_all(data)
    }
}

/// The work tree against HEAD: changed, added and untracked files (with the
/// ignore rules honoured, and never an install a service makes itself), each
/// with its blob written to the private store, and the deleted paths.
async fn overlay(
    env: &GitEnv,
    limits: Limits,
) -> Result<(Vec<Entry>, Vec<String>, Vec<Withheld>), SnapshotError> {
    let repo = &env.repo;
    let mut args: Vec<String> = [
        "status",
        "--porcelain=v1",
        "-z",
        "--untracked-files=all",
        "--no-renames",
        "--ignore-submodules=all",
        "--",
        ".",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.extend(left_to_the_service(repo));
    // Tracked files in those folders still count (a vendored node_modules):
    // with no untracked files, git lists only them, without walking an install.
    // The two questions are asked together.
    let untracked_args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (status, tracked) = tokio::join!(
        git(repo, &[], None, &untracked_args),
        git(
            repo,
            &[],
            None,
            &[
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=no",
                "--no-renames",
                "--ignore-submodules=all",
                "--",
                ":(glob)**/node_modules/**",
                ":(glob)target/**",
            ],
        ),
    );
    let status = status?;
    let tracked = tracked?;

    let mut seen = BTreeSet::new();
    let mut changed: Vec<String> = Vec::new();
    let mut deletes: Vec<String> = Vec::new();
    for record in status.split(|b| *b == 0).chain(tracked.split(|b| *b == 0)) {
        if record.len() < 4 {
            continue;
        }
        let path = String::from_utf8_lossy(&record[3..]).to_string();
        if !seen.insert(path.clone()) {
            continue;
        }
        match std::fs::symlink_metadata(repo.join(&path)) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => deletes.push(path),
            Err(e) => return Err(SnapshotError::Io(format!("{path}: {e}"))),
            // A nested repository, or a folder git lists as one entry.
            Ok(meta) if meta.is_dir() => {}
            Ok(_) => changed.push(path),
        }
    }
    if changed.len() > limits.files {
        return Err(SnapshotError::TooLarge(format!(
            "{} changed files are over the limit of {}",
            changed.len(),
            limits.files
        )));
    }

    let mut privacy = PrivacyIgnore::new(repo);
    let mut withheld = Vec::new();
    let mut symlinks: Vec<(String, Vec<u8>)> = Vec::new();
    let mut regular: Vec<(String, String)> = Vec::new();
    let mut total: u64 = 0;
    for path in changed {
        let full = repo.join(&path);
        let meta = std::fs::symlink_metadata(&full)
            .map_err(|e| SnapshotError::Io(format!("{path}: {e}")))?;
        if let Some(reason) = privacy.reason(&path) {
            withheld.push(Withheld { path, reason });
            continue;
        }
        if meta.file_type().is_symlink() {
            let target =
                std::fs::read_link(&full).map_err(|e| SnapshotError::Io(format!("{path}: {e}")))?;
            let bytes = target.to_string_lossy().as_bytes().to_vec();
            total += bytes.len() as u64;
            symlinks.push((path, bytes));
            continue;
        }
        if !meta.is_file() {
            return Err(SnapshotError::Io(format!("{path} is not a regular file")));
        }
        if meta.len() > limits.file_bytes {
            return Err(SnapshotError::TooLarge(format!(
                "{path} is {} MB, over the per-file limit",
                meta.len() >> 20
            )));
        }
        if let Some(reason) = secret_reason(&path, &full, meta.len()) {
            withheld.push(Withheld { path, reason });
            continue;
        }
        total += meta.len();
        regular.push((path, file_mode(&meta)));
    }
    if total > limits.total_bytes {
        return Err(SnapshotError::TooLarge(format!(
            "the changed files are {} MB, over the limit",
            total >> 20
        )));
    }

    let mut entries = Vec::new();
    for (path, bytes) in symlinks {
        let out = git(
            repo,
            &env.objects,
            Some(bytes),
            &["hash-object", "-w", "--stdin"],
        )
        .await?;
        entries.push(Entry {
            mode: "120000".to_string(),
            oid: text(&out),
            path,
        });
    }
    // A name git cannot list on a line goes through alone.
    let (plain, odd): (Vec<_>, Vec<_>) = regular
        .into_iter()
        .partition(|(path, _)| !path.contains('\n') && !path.starts_with('"'));
    if !plain.is_empty() {
        let list = plain
            .iter()
            .map(|(path, _)| path.as_str())
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        let out = git(
            repo,
            &env.objects,
            Some(list.into_bytes()),
            &["hash-object", "-w", "--stdin-paths"],
        )
        .await?;
        let oids: Vec<String> = text(&out).split_whitespace().map(str::to_string).collect();
        if oids.len() != plain.len() {
            return Err(SnapshotError::Git {
                command: "hash-object".to_string(),
                detail: "a wrong count of ids".to_string(),
            });
        }
        for ((path, mode), oid) in plain.into_iter().zip(oids) {
            entries.push(Entry { mode, oid, path });
        }
    }
    for (path, mode) in odd {
        let out = git(
            repo,
            &env.objects,
            None,
            &["hash-object", "-w", "--", &path],
        )
        .await?;
        entries.push(Entry {
            mode,
            oid: text(&out),
            path,
        });
    }
    Ok((entries, deletes, withheld))
}

#[cfg(unix)]
fn file_mode(meta: &std::fs::Metadata) -> String {
    use std::os::unix::fs::PermissionsExt;
    if meta.permissions().mode() & 0o111 != 0 {
        "100755".to_string()
    } else {
        "100644".to_string()
    }
}

#[cfg(not(unix))]
fn file_mode(_meta: &std::fs::Metadata) -> String {
    "100644".to_string()
}

/// The environment for the scratch index in `scratch`.
fn scratch_vars(env: &GitEnv, scratch: &Path) -> Vec<(String, String)> {
    let mut vars = env.objects.clone();
    vars.push((
        "GIT_INDEX_FILE".to_string(),
        scratch.join("index").to_string_lossy().to_string(),
    ));
    vars
}

/// Start the scratch index from the base tree.
async fn prepare_scratch_index(
    env: &GitEnv,
    scratch: &Path,
    base: &str,
) -> Result<(), SnapshotError> {
    git(
        &env.repo,
        &scratch_vars(env, scratch),
        None,
        &["read-tree", base],
    )
    .await?;
    Ok(())
}

/// The base tree plus the changes: the changes go into the scratch index
/// [`prepare_scratch_index`] made, and it is written out as a tree.
async fn finish_target_tree(
    env: &GitEnv,
    scratch: &Path,
    entries: &[Entry],
    deletes: &[String],
) -> Result<String, SnapshotError> {
    let vars = scratch_vars(env, scratch);
    let mut lines: Vec<u8> = Vec::new();
    for entry in entries {
        lines.extend_from_slice(format!("{} {}\t{}", entry.mode, entry.oid, entry.path).as_bytes());
        lines.push(0);
    }
    for path in deletes {
        lines.extend_from_slice(format!("0 {}\t{}", "0".repeat(40), path).as_bytes());
        lines.push(0);
    }
    if !lines.is_empty() {
        git(
            &env.repo,
            &vars,
            Some(lines),
            &["update-index", "-z", "--index-info"],
        )
        .await?;
    }
    let out = git(&env.repo, &vars, None, &["write-tree"]).await?;
    Ok(text(&out))
}

#[cfg(test)]
#[path = "checkout_snapshot_tests.rs"]
mod tests;
