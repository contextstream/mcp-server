use super::*;
use std::io::Read;
use std::process::Command as Std;

/// Runs git in `dir`; panics on failure; returns trimmed stdout.
fn git_ok(dir: &Path, args: &[&str]) -> String {
    let out = Std::new("git")
        .args(args)
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_INDEX_FILE")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn write(dir: &Path, rel: &str, content: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn commit_all(dir: &Path, message: &str) {
    git_ok(dir, &["add", "-A"]);
    git_ok(
        dir,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "-m",
            message,
        ],
    );
}

/// A repository whose last commit holds a few files, and whose index is fresh.
fn repo_with_history() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    git_ok(dir, &["init", "-q"]);
    write(dir, "a.txt", "alpha\n");
    write(dir, "c.txt", "gone soon\n");
    write(dir, "src/lib.rs", "pub fn one() {}\n");
    write(dir, "Cargo.lock", "# lock\n");
    write(dir, ".gitignore", "ignored.log\n");
    commit_all(dir, "first");
    git_ok(dir, &["status", "--porcelain"]);
    temp
}

/// The tree git itself makes of HEAD plus `add` (paths, relative to the repo
/// root) and minus `remove`, in a scratch index: an independent answer.
fn reference_tree(dir: &Path, add: &[&str], remove: &[&str]) -> String {
    let scratch = tempfile::tempdir().unwrap();
    let index = scratch.path().join("index");
    let run = |args: &[&str]| {
        let out = Std::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_INDEX_FILE", &index)
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    };
    if Std::new("git")
        .args(["rev-parse", "-q", "--verify", "HEAD"])
        .current_dir(dir)
        .output()
        .unwrap()
        .status
        .success()
    {
        run(&["read-tree", "HEAD"]);
    }
    for path in remove {
        run(&["rm", "-q", "--cached", "--", path]);
    }
    for path in add {
        run(&["add", "-f", "--", path]);
    }
    run(&["write-tree"])
}

fn unpack(pack: &[u8]) -> tempfile::TempDir {
    let target = tempfile::tempdir().unwrap();
    git_ok(target.path(), &["init", "-q"]);
    let mut child = Std::new("git")
        .args(["unpack-objects", "-q"])
        .current_dir(target.path())
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(&mut child.stdin.take().unwrap(), pack).unwrap();
    assert!(child.wait().unwrap().success(), "unpack-objects");
    target
}

fn paths(entries: &[Entry]) -> Vec<(String, String)> {
    let mut rows: Vec<(String, String)> = entries
        .iter()
        .map(|e| (e.path.clone(), e.mode.clone()))
        .collect();
    rows.sort();
    rows
}

#[tokio::test]
async fn a_clean_checkout_has_nothing_on_top_of_its_commit() {
    let repo = repo_with_history();
    let snapshot = CheckoutSnapshot::capture(repo.path()).await.unwrap();
    assert!(snapshot.overlay.is_empty());
    assert!(snapshot.deletes.is_empty());
    assert!(snapshot.withheld.is_empty());
    assert_eq!(
        snapshot.base_tree,
        git_ok(repo.path(), &["rev-parse", "HEAD^{tree}"])
    );
    assert_eq!(snapshot.target_tree, snapshot.base_tree);
    assert_eq!(snapshot.folder, "");
}

#[cfg(unix)]
#[tokio::test]
async fn the_target_tree_is_the_tree_git_makes_of_the_same_files() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let repo = repo_with_history();
    let dir = repo.path();
    write(dir, "a.txt", "alpha, changed\n");
    std::fs::remove_file(dir.join("c.txt")).unwrap();
    write(dir, "b/new.txt", "brand new\n");
    write(dir, "script.sh", "#!/bin/sh\necho hi\n");
    std::fs::set_permissions(
        dir.join("script.sh"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    symlink("a.txt", dir.join("link")).unwrap();
    write(dir, "Cargo.lock", "# lock, changed\n");
    // Never sent: ignored by .gitignore, an install, and Cargo's own target.
    write(dir, "ignored.log", "noise\n");
    write(dir, "web/node_modules/pkg/index.js", "module.exports = 1\n");
    write(
        dir,
        "target/CACHEDIR.TAG",
        "Signature: 8a477f597d28d172789f06886806bc55\n",
    );
    write(dir, "target/debug/app", "binary\n");

    let snapshot = CheckoutSnapshot::capture(dir).await.unwrap();

    assert_eq!(
        paths(&snapshot.overlay),
        vec![
            ("Cargo.lock".to_string(), "100644".to_string()),
            ("a.txt".to_string(), "100644".to_string()),
            ("b/new.txt".to_string(), "100644".to_string()),
            ("link".to_string(), "120000".to_string()),
            ("script.sh".to_string(), "100755".to_string()),
        ]
    );
    assert_eq!(snapshot.deletes, vec!["c.txt".to_string()]);
    assert!(snapshot.withheld.is_empty());
    let expected = reference_tree(
        dir,
        &["Cargo.lock", "a.txt", "b/new.txt", "link", "script.sh"],
        &["c.txt"],
    );
    assert_eq!(snapshot.target_tree, expected);
    // The blob ids are git's.
    let blob = snapshot.overlay.iter().find(|e| e.path == "a.txt").unwrap();
    assert_eq!(blob.oid, git_ok(dir, &["hash-object", "a.txt"]));
}

#[tokio::test]
async fn a_tracked_file_in_an_install_folder_still_counts() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    git_ok(dir, &["init", "-q"]);
    write(dir, "vendor/node_modules/dep/index.js", "v1\n");
    write(dir, "keep.txt", "k\n");
    commit_all(dir, "vendored");
    write(dir, "vendor/node_modules/dep/index.js", "v2\n");
    write(
        dir,
        "vendor/node_modules/other/new.js",
        "untracked install\n",
    );
    let snapshot = CheckoutSnapshot::capture(dir).await.unwrap();
    assert_eq!(
        paths(&snapshot.overlay),
        vec![(
            "vendor/node_modules/dep/index.js".to_string(),
            "100644".to_string()
        )]
    );
}

#[tokio::test]
async fn taking_a_snapshot_leaves_the_repository_as_it_was() {
    let repo = repo_with_history();
    let dir = repo.path();
    write(dir, "a.txt", "changed\n");
    write(dir, "new.txt", "new\n");
    let listing = || {
        let mut rows: Vec<(String, u64)> = walkdir_files(&dir.join(".git"));
        rows.sort();
        rows
    };
    let before = (listing(), std::fs::read(dir.join(".git/index")).unwrap());
    let snapshot = CheckoutSnapshot::capture(dir).await.unwrap();
    assert_eq!(snapshot.overlay.len(), 2);
    let after = (listing(), std::fs::read(dir.join(".git/index")).unwrap());
    assert_eq!(before, after, "the repository changed");
}

fn walkdir_files(root: &Path) -> Vec<(String, u64)> {
    let mut rows = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            let meta = entry.metadata().unwrap();
            if meta.is_dir() {
                stack.push(path);
            } else {
                rows.push((
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .to_string(),
                    meta.len(),
                ));
            }
        }
    }
    rows
}

#[tokio::test]
async fn credentials_and_listed_paths_stay_on_the_machine() {
    let repo = repo_with_history();
    let dir = repo.path();
    write(dir, ".env", "TOKEN=abc\n");
    write(dir, ".env.example", "TOKEN=\n");
    write(dir, "deploy.key", "key material\n");
    let key_body = "A".repeat(64);
    write(
        dir,
        "notes.txt",
        &format!(
            "-----BEGIN PRIVATE KEY-----\n{key_body}\n{key_body}\n-----END PRIVATE KEY-----\n"
        ),
    );
    write(dir, ".contextignore", "private/\n");
    write(dir, "private/plan.md", "internal\n");
    write(dir, ".contextstream/ignore", "vault/\n");
    write(dir, "vault/x.txt", "x\n");
    write(dir, "Cargo.lock", "# lock, changed\n");
    write(dir, "fine.txt", "fine\n");

    let snapshot = CheckoutSnapshot::capture(dir).await.unwrap();

    let sent: Vec<&str> = {
        let mut v: Vec<&str> = snapshot.overlay.iter().map(|e| e.path.as_str()).collect();
        v.sort();
        v
    };
    assert_eq!(
        sent,
        [
            ".contextignore",
            ".contextstream/ignore",
            ".env.example",
            "Cargo.lock",
            "fine.txt"
        ]
    );
    let mut withheld: Vec<(&str, &str)> = snapshot
        .withheld
        .iter()
        .map(|w| (w.path.as_str(), w.reason.as_str()))
        .collect();
    withheld.sort();
    let names: Vec<&str> = withheld.iter().map(|(p, _)| *p).collect();
    assert_eq!(
        names,
        [
            ".env",
            "deploy.key",
            "notes.txt",
            "private/plan.md",
            "vault/x.txt"
        ]
    );
    assert!(withheld[0].1.contains("credentials"), "{withheld:?}");
    assert!(withheld[2].1.contains("private key"), "{withheld:?}");
    assert!(withheld[3].1.contains(".contextignore"), "{withheld:?}");
    assert!(
        withheld[4].1.contains(".contextstream/ignore"),
        "{withheld:?}"
    );
    // The tree the service builds is the one without them.
    let expected = reference_tree(
        dir,
        &[
            ".contextignore",
            ".contextstream/ignore",
            ".env.example",
            "Cargo.lock",
            "fine.txt",
        ],
        &[],
    );
    assert_eq!(snapshot.target_tree, expected);
    // The attachment names them but carries no content.
    let attachment = snapshot.attachment();
    assert_eq!(attachment["withheld"].as_array().unwrap().len(), 5);
    assert!(!attachment.to_string().contains("TOKEN=abc"));
    assert_eq!(attachment["changed"], 5);
}

#[tokio::test]
async fn a_checkout_over_a_limit_is_not_snapshotted() {
    let repo = repo_with_history();
    let dir = repo.path();
    write(dir, "one.txt", "1\n");
    write(dir, "two.txt", "2\n");
    write(dir, "three.txt", "3\n");
    let few = Limits {
        files: 2,
        ..Limits::default()
    };
    assert!(matches!(
        CheckoutSnapshot::capture_with(dir, few).await,
        Err(SnapshotError::TooLarge(_))
    ));
    let small_files = Limits {
        file_bytes: 1,
        ..Limits::default()
    };
    assert!(matches!(
        CheckoutSnapshot::capture_with(dir, small_files).await,
        Err(SnapshotError::TooLarge(_))
    ));
    let small_total = Limits {
        total_bytes: 3,
        ..Limits::default()
    };
    assert!(matches!(
        CheckoutSnapshot::capture_with(dir, small_total).await,
        Err(SnapshotError::TooLarge(_))
    ));
}

#[tokio::test]
async fn a_service_can_ask_for_the_listing_and_for_blobs() {
    let repo = repo_with_history();
    let dir = repo.path();
    write(dir, "added.txt", "added, not committed\n");
    let snapshot = CheckoutSnapshot::capture(dir).await.unwrap();

    // The base tree's listing, zlib-compressed.
    let listing = snapshot
        .answer_need(&serde_json::json!({"listing": true}))
        .await
        .unwrap();
    let mut plain = Vec::new();
    flate2::read::ZlibDecoder::new(&listing[..])
        .read_to_end(&mut plain)
        .unwrap();
    let expected = Std::new("git")
        .args(["ls-tree", "-r", "-z", "--full-tree", &snapshot.base_tree])
        .current_dir(dir)
        .output()
        .unwrap()
        .stdout;
    assert_eq!(plain, expected);

    // A blob of the base, and a blob of the changes, in one pack.
    let base_blob = git_ok(dir, &["rev-parse", "HEAD:a.txt"]);
    let new_blob = snapshot.overlay[0].oid.clone();
    let pack = snapshot
        .answer_need(&serde_json::json!({"blobs": [base_blob, new_blob]}))
        .await
        .unwrap();
    let target = unpack(&pack);
    assert_eq!(
        git_ok(target.path(), &["cat-file", "-p", &base_blob]),
        "alpha"
    );
    assert_eq!(
        git_ok(target.path(), &["cat-file", "-p", &new_blob]),
        "added, not committed"
    );

    // The first message after the request holds every change.
    let first = unpack(&snapshot.initial_pack().await.unwrap());
    assert_eq!(
        git_ok(first.path(), &["cat-file", "-p", &new_blob]),
        "added, not committed"
    );

    // An object the snapshot does not hold is never given out, though it is in the repository.
    write(dir, "later.txt", "committed later\n");
    commit_all(dir, "second");
    let history_only = git_ok(dir, &["rev-parse", "HEAD:later.txt"]);
    let refused = snapshot
        .answer_need(&serde_json::json!({"blobs": [history_only]}))
        .await;
    assert!(
        matches!(refused, Err(SnapshotError::NotHeld)),
        "{refused:?}"
    );
    // Malformed ids are ignored, and nothing asked is an empty answer.
    let none = snapshot
        .answer_need(&serde_json::json!({"blobs": ["--all", "xyz"]}))
        .await
        .unwrap();
    assert!(none.is_empty());
}

#[tokio::test]
async fn a_repository_without_a_commit_has_the_empty_tree_as_its_base() {
    let temp = tempfile::tempdir().unwrap();
    let dir = temp.path();
    git_ok(dir, &["init", "-q"]);
    write(dir, "first.txt", "hello\n");
    let snapshot = CheckoutSnapshot::capture(dir).await.unwrap();
    assert_eq!(snapshot.base_tree, EMPTY_TREE);
    assert_eq!(snapshot.overlay.len(), 1);
    assert_eq!(
        snapshot.target_tree,
        reference_tree(dir, &["first.txt"], &[])
    );
}

#[tokio::test]
async fn a_subfolder_is_snapshotted_as_its_whole_repository() {
    let repo = repo_with_history();
    let dir = repo.path();
    write(dir, "src/extra.rs", "pub fn two() {}\n");
    let snapshot = CheckoutSnapshot::capture(&dir.join("src")).await.unwrap();
    assert_eq!(snapshot.folder, "src");
    assert_eq!(snapshot.root, std::fs::canonicalize(dir).unwrap());
    assert_eq!(snapshot.overlay[0].path, "src/extra.rs");
    let fields = snapshot.request_fields();
    assert_eq!(
        fields.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        ["base_tree", "deletes", "overlay", "root", "target_tree"]
            .into_iter()
            .collect()
    );
    assert_eq!(
        fields["overlay"][0],
        serde_json::json!(["100644", snapshot.overlay[0].oid, "src/extra.rs"])
    );
}

#[tokio::test]
async fn names_git_cannot_list_on_a_line_are_still_snapshotted() {
    let repo = repo_with_history();
    let dir = repo.path();
    write(dir, "with space.txt", "s\n");
    write(dir, "naïve.txt", "u\n");
    write(dir, "line\nbreak.txt", "n\n");
    write(dir, "\"quoted\".txt", "q\n");
    let snapshot = CheckoutSnapshot::capture(dir).await.unwrap();
    assert_eq!(snapshot.overlay.len(), 4, "{:?}", snapshot.overlay);
    assert_eq!(
        snapshot.target_tree,
        reference_tree(
            dir,
            &[
                "with space.txt",
                "naïve.txt",
                "line\nbreak.txt",
                "\"quoted\".txt"
            ],
            &[]
        )
    );
}

#[tokio::test]
async fn a_nested_repository_is_not_part_of_the_snapshot() {
    let repo = repo_with_history();
    let dir = repo.path();
    let nested = dir.join("tools/inner");
    std::fs::create_dir_all(&nested).unwrap();
    git_ok(&nested, &["init", "-q"]);
    write(&nested, "x.txt", "x\n");
    let snapshot = CheckoutSnapshot::capture(dir).await.unwrap();
    assert!(snapshot.overlay.is_empty(), "{:?}", snapshot.overlay);
}

#[tokio::test]
async fn a_folder_outside_a_repository_is_not_snapshotted() {
    let temp = tempfile::tempdir().unwrap();
    let result = CheckoutSnapshot::capture(temp.path()).await;
    assert!(
        matches!(result, Err(SnapshotError::NotARepository(_))),
        "{:?}",
        result.err()
    );
}

#[test]
fn the_settings_decide_whether_a_snapshot_may_be_taken() {
    let base = Settings {
        origin: "https://github.com/acme/app.git",
        folder_name: "app",
        ..Settings::default()
    };
    assert_eq!(refusal(&base), None);
    assert!(refusal(&Settings {
        local_files_denied: true,
        ..base.clone()
    })
    .unwrap()
    .contains("no local files"));
    for off in ["off", "0", "false", "No", " disabled "] {
        assert!(
            refusal(&Settings {
                env_switch: Some(off),
                ..base.clone()
            })
            .is_some(),
            "{off}"
        );
    }
    assert_eq!(
        refusal(&Settings {
            env_switch: Some("on"),
            ..base.clone()
        }),
        None
    );
    assert!(refusal(&Settings {
        project_switch: Some(false),
        ..base.clone()
    })
    .unwrap()
    .contains(".contextstream/config.json"));
    assert_eq!(
        refusal(&Settings {
            project_switch: Some(true),
            ..base.clone()
        }),
        None
    );
    let only_acme = Settings {
        env_repos: Some("https://github.com/acme/"),
        ..base.clone()
    };
    assert_eq!(refusal(&only_acme), None);
    let by_name = Settings {
        env_repos: Some("other, app"),
        origin: "",
        ..base.clone()
    };
    assert_eq!(refusal(&by_name), None);
    let elsewhere = Settings {
        env_repos: Some("https://github.com/other/"),
        ..base.clone()
    };
    assert!(refusal(&elsewhere).unwrap().contains(ENV_REPOS));
    // An empty list means every repository.
    assert_eq!(
        refusal(&Settings {
            env_repos: Some("  "),
            ..base
        }),
        None
    );
}

#[test]
fn a_repository_is_named_by_its_folder_and_a_digest_of_its_origin() {
    // The digest of the empty string, and of an origin with a newline after it.
    let empty = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
    assert_eq!(origin_digest(""), empty);
    assert_eq!(origin_digest("\n"), empty);
    assert_eq!(
        origin_digest("git@github.com:acme/app.git\n"),
        origin_digest("git@github.com:acme/app.git")
    );
    assert_ne!(origin_digest("git@github.com:acme/app.git"), empty);
    assert_eq!(origin_digest("x").len(), 64);
}

#[test]
fn big_data_goes_in_pieces_of_at_most_one_mebibyte() {
    let data = vec![7u8; PIECE_BYTES_MAX * 2 + 5];
    let sizes: Vec<usize> = in_pieces(&data).map(<[u8]>::len).collect();
    assert_eq!(sizes, [PIECE_BYTES_MAX, PIECE_BYTES_MAX, 5]);
    assert_eq!(in_pieces(&[]).count(), 0);
}

#[test]
fn secret_names_are_told_from_lock_files_and_archives() {
    for secret in [
        ".env",
        ".env.local",
        "id_rsa",
        "server.pem",
        "tls.KEY",
        "creds.p12",
        "credentials.json",
        "prod.tfvars",
    ] {
        assert!(ContextStreamClient::is_secret_file_name(secret), "{secret}");
        assert!(
            ContextStreamClient::should_skip_file(secret, true),
            "{secret}"
        );
    }
    // Ingest skips these; a snapshot must send them: a changed Cargo.lock changes a build.
    for not_secret in [
        "Cargo.lock",
        "yarn.lock",
        "bun.lockb",
        "bundle.zip",
        ".env.example",
        "main.rs",
    ] {
        assert!(
            !ContextStreamClient::is_secret_file_name(not_secret),
            "{not_secret}"
        );
    }
    assert!(ContextStreamClient::should_skip_file("Cargo.lock", true));
    assert!(ContextStreamClient::should_skip_file("bundle.zip", true));
    assert!(!ContextStreamClient::should_skip_file("main.rs", true));
}

#[derive(serde::Deserialize)]
struct CredentialFileNameVector {
    withheld: Vec<String>,
    sent: Vec<String>,
}

/// Shared credential file-name vector (`testdata/credential-file-names.json`,
/// sha256 `127fa51c62c935cf15fc5529819585f2feb179ef3c0a1e9e08a9a0f927651dce`).
#[test]
fn credential_file_names_follow_the_shared_vector() {
    let expected_sha256 = "127fa51c62c935cf15fc5529819585f2feb179ef3c0a1e9e08a9a0f927651dce";
    let raw = include_str!("testdata/credential-file-names.json");
    let normalized = raw.replace("\r\n", "\n");
    let actual_sha256 = hex::encode(Sha256::digest(normalized.as_bytes()));
    assert_eq!(actual_sha256, expected_sha256);

    let vector: CredentialFileNameVector = serde_json::from_str(raw).unwrap();
    assert_eq!(vector.withheld.len(), 47);
    assert_eq!(vector.sent.len(), 34);

    let mut disagreements = Vec::new();
    let cases = [(&vector.withheld[..], true), (&vector.sent[..], false)];
    for (names, expected) in cases {
        for name in names {
            let actual = ContextStreamClient::is_secret_file_name(name);
            if actual != expected {
                let detail = format!("{name}: expected {expected}, actual {actual}");
                disagreements.push(detail);
            }
        }
    }
    assert!(
        disagreements.is_empty(),
        "credential file-name disagreements:\n{}",
        disagreements.join("\n")
    );
}

#[tokio::test]
async fn the_identity_is_known_before_the_rest_of_the_snapshot_is_taken() {
    let temp = repo_with_history();
    write(temp.path(), "a.txt", "alpha, changed\n");
    let start = CheckoutSnapshot::begin(temp.path()).await.unwrap();
    let identity = start.identity();
    let name = temp.path().file_name().unwrap().to_str().unwrap();
    assert_eq!(identity["repo"]["name"], name);
    assert_eq!(
        identity["repo"]["origin_sha256"].as_str().unwrap().len(),
        64
    );
    assert_eq!(identity.as_object().unwrap().len(), 1, "{identity}");
    assert_eq!(start.folder(), "");
    let snapshot = start.complete().await.unwrap();
    assert_eq!(snapshot.identity(), identity);
    // The rest is what a whole capture gives.
    let again = CheckoutSnapshot::capture(temp.path()).await.unwrap();
    assert_eq!(snapshot.base_tree, again.base_tree);
    assert_eq!(snapshot.target_tree, again.target_tree);
    assert_eq!(snapshot.overlay, again.overlay);
    assert_eq!(snapshot.overlay.len(), 1);
}

#[tokio::test]
async fn a_checkout_with_nothing_changed_is_its_own_base_tree() {
    let temp = repo_with_history();
    let snapshot = CheckoutSnapshot::capture(temp.path()).await.unwrap();
    assert!(snapshot.overlay.is_empty() && snapshot.deletes.is_empty());
    assert_eq!(snapshot.target_tree, snapshot.base_tree);
    assert_eq!(
        snapshot.base_tree,
        git_ok(temp.path(), &["rev-parse", "HEAD^{tree}"])
    );
    // A change makes the trees differ, and an independent answer agrees on the new one.
    write(temp.path(), "a.txt", "alpha, changed\n");
    let changed = CheckoutSnapshot::capture(temp.path()).await.unwrap();
    assert_ne!(changed.target_tree, changed.base_tree);
    assert_eq!(
        changed.target_tree,
        reference_tree(temp.path(), &["a.txt"], &[])
    );
    // Withholding the only change leaves the base tree as the target.
    std::fs::remove_file(temp.path().join("a.txt")).ok();
    git_ok(temp.path(), &["checkout", "--", "a.txt"]);
    write(temp.path(), ".env", "TOKEN=abc\n");
    let withheld = CheckoutSnapshot::capture(temp.path()).await.unwrap();
    assert_eq!(withheld.withheld.len(), 1);
    assert_eq!(withheld.target_tree, withheld.base_tree);
}

#[tokio::test]
async fn begin_names_the_folder_and_refuses_what_is_not_a_repository() {
    let temp = repo_with_history();
    let start = CheckoutSnapshot::begin(&temp.path().join("src"))
        .await
        .unwrap();
    assert_eq!(start.folder(), "src");
    assert_eq!(
        std::fs::canonicalize(start.root()).unwrap(),
        std::fs::canonicalize(temp.path()).unwrap()
    );
    let not_a_repo = tempfile::tempdir().unwrap();
    assert!(matches!(
        CheckoutSnapshot::begin(not_a_repo.path()).await,
        Err(SnapshotError::NotARepository(_))
    ));
}

#[tokio::test]
async fn a_repository_before_its_first_commit_is_its_own_empty_base() {
    let temp = tempfile::tempdir().unwrap();
    git_ok(temp.path(), &["init", "-q"]);
    let snapshot = CheckoutSnapshot::capture(temp.path()).await.unwrap();
    assert_eq!(snapshot.base_tree, EMPTY_TREE);
    assert_eq!(snapshot.target_tree, EMPTY_TREE);
    write(temp.path(), "first.txt", "hello\n");
    let with_file = CheckoutSnapshot::capture(temp.path()).await.unwrap();
    assert_eq!(with_file.base_tree, EMPTY_TREE);
    assert_eq!(with_file.overlay.len(), 1);
    assert_eq!(
        with_file.target_tree,
        reference_tree(temp.path(), &["first.txt"], &[])
    );
}

fn sha(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

/// The digest a snapshot of `dir` names its repository with.
async fn named_as(dir: &Path) -> String {
    let identity = CheckoutSnapshot::begin(dir).await.unwrap().identity();
    identity["repo"]["origin_sha256"]
        .as_str()
        .unwrap()
        .to_string()
}

/// The ids of the first commits of `dir`, sorted.
fn root_commits(dir: &Path) -> Vec<String> {
    let mut ids: Vec<String> = git_ok(dir, &["rev-list", "--max-parents=0", "HEAD"])
        .lines()
        .map(str::to_string)
        .collect();
    ids.sort();
    ids
}

/// A commit with the tree of HEAD and no parent, made without touching any branch.
fn parentless_commit(dir: &Path, message: &str) -> String {
    let tree = git_ok(dir, &["rev-parse", "HEAD^{tree}"]);
    git_ok(
        dir,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit-tree",
            &tree,
            "-m",
            message,
        ],
    )
}

#[tokio::test]
async fn a_repository_with_an_origin_is_named_by_a_digest_of_the_origin() {
    let temp = repo_with_history();
    let url = "git@example.com:acme/app.git";
    git_ok(temp.path(), &["remote", "add", "origin", url]);
    assert_eq!(named_as(temp.path()).await, sha(url));
    // Another copy of the same origin is the same repository, whatever its history.
    let other = tempfile::tempdir().unwrap();
    git_ok(other.path(), &["init", "-q"]);
    git_ok(other.path(), &["remote", "add", "origin", url]);
    assert_eq!(named_as(other.path()).await, sha(url));
    // The digest, not the URL, is all the identity holds.
    let identity = CheckoutSnapshot::begin(temp.path())
        .await
        .unwrap()
        .identity();
    assert!(!identity.to_string().contains("example.com"), "{identity}");
}

#[tokio::test]
async fn without_an_origin_the_first_commit_names_the_repository() {
    let temp = repo_with_history();
    let first = git_ok(temp.path(), &["rev-parse", "HEAD"]);
    let named = sha(&format!("root:{first}"));
    assert_eq!(named_as(temp.path()).await, named);
    // Later commits do not change it.
    write(temp.path(), "more.txt", "more\n");
    commit_all(temp.path(), "second");
    assert_eq!(named_as(temp.path()).await, named);
    // It is no longer the digest of the empty string.
    assert_ne!(named, origin_digest(""));
    // A copy made in another folder, with the remote taken off, is the same repository.
    let parent = tempfile::tempdir().unwrap();
    git_ok(
        parent.path(),
        &["clone", "-q", temp.path().to_str().unwrap(), "another-name"],
    );
    let copy = parent.path().join("another-name");
    git_ok(&copy, &["remote", "remove", "origin"]);
    assert_eq!(named_as(&copy).await, named);
}

#[tokio::test]
async fn an_origin_that_is_blank_is_no_origin() {
    let temp = repo_with_history();
    let first = git_ok(temp.path(), &["rev-parse", "HEAD"]);
    git_ok(temp.path(), &["config", "remote.origin.url", " "]);
    assert_eq!(named_as(temp.path()).await, sha(&format!("root:{first}")));
}

#[tokio::test]
async fn several_first_commits_are_all_in_the_name_sorted() {
    let temp = repo_with_history();
    let dir = temp.path();
    let second_root = parentless_commit(dir, "second root");
    let third_root = parentless_commit(dir, "third root");
    let tree = git_ok(dir, &["rev-parse", "HEAD^{tree}"]);
    let join = git_ok(
        dir,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.com",
            "commit-tree",
            &tree,
            "-p",
            "HEAD",
            "-p",
            &second_root,
            "-p",
            &third_root,
            "-m",
            "join",
        ],
    );
    git_ok(dir, &["update-ref", "HEAD", &join]);
    let ids = root_commits(dir);
    assert_eq!(ids.len(), 3, "{ids:?}");
    assert_eq!(
        named_as(dir).await,
        sha(&format!("root:{}", ids.join(","))),
        "{ids:?}"
    );
}

/// The inputs and digests that ContextCode pins in its own tests, character for
/// character: the two name a repository the same way, so a change to one of these
/// values is a change to both.
#[test]
fn the_digests_of_the_shared_inputs_are_fixed() {
    let digest = |identity: &[u8]| hex::encode(Sha256::digest(identity));
    // An origin URL, as the digest of the URL itself (and of its trimmed form).
    assert_eq!(
        origin_digest("git@github.com:acme/app.git"),
        "ac4dee71355abf0a1f7c0da63d264fdff4f107b83d8c124a09ccef3bf693eedb"
    );
    assert_eq!(
        origin_digest(" git@github.com:acme/app.git\n"),
        origin_digest("git@github.com:acme/app.git")
    );
    // The first commits' ids: one root, then two roots listed out of order.
    let one = "0123456789abcdef0123456789abcdef01234567";
    assert_eq!(
        digest(root_identity(&format!("{one}\n")).unwrap().as_bytes()),
        "8714f0af35167c2165576d0b8918a5246398b20d14e764172e15e87d74859649"
    );
    let low = "1111111111111111111111111111111111111111";
    let high = "2222222222222222222222222222222222222222";
    assert_eq!(
        digest(
            root_identity(&format!("{high}\n{low}\n"))
                .unwrap()
                .as_bytes()
        ),
        "10fe543afe15e64e64f60cac97f7b0781618525cb724580f9847ef8cc05d5c17"
    );
    // The folder, before the first commit.
    assert_eq!(
        digest(&path_identity(Path::new("/work/app"))),
        "613c761bfc946f685fac386a0cad288aaea1a1a4f0d2a02d6b0f4b94729c9fea"
    );
}

#[test]
fn the_first_commits_are_named_in_sorted_order_whatever_git_printed() {
    assert_eq!(root_identity("bbb\naaa\n").as_deref(), Some("root:aaa,bbb"));
    assert_eq!(root_identity("c\nb\na").as_deref(), Some("root:a,b,c"));
    assert_eq!(root_identity("abc\r\n").as_deref(), Some("root:abc"));
    assert_eq!(root_identity(""), None);
    assert_eq!(root_identity("\n  \n"), None);
}

#[test]
fn a_windows_path_loses_the_prefix_that_resolving_it_adds() {
    assert_eq!(without_verbatim_prefix(r"\\?\C:\work\app"), r"C:\work\app");
    assert_eq!(
        without_verbatim_prefix(r"\\?\UNC\host\share\app"),
        r"\\host\share\app"
    );
    assert_eq!(without_verbatim_prefix(r"C:\work\app"), r"C:\work\app");
    assert_eq!(without_verbatim_prefix("/home/a/app"), "/home/a/app");
    assert_eq!(
        without_verbatim_prefix(r"\\?\Volume{1}\app"),
        r"\\?\Volume{1}\app"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn before_the_first_commit_the_folder_names_the_repository() {
    let temp = tempfile::tempdir().unwrap();
    git_ok(temp.path(), &["init", "-q"]);
    let folder = std::fs::canonicalize(temp.path()).unwrap();
    let before = named_as(temp.path()).await;
    assert_eq!(before, sha(&format!("path:{}", folder.to_str().unwrap())));
    assert_ne!(before, origin_digest(""));
    // Files that are not committed change nothing.
    write(temp.path(), "a.txt", "alpha\n");
    assert_eq!(named_as(temp.path()).await, before);
    // The first commit gives it the name its history gives.
    commit_all(temp.path(), "first");
    let first = git_ok(temp.path(), &["rev-parse", "HEAD"]);
    assert_eq!(named_as(temp.path()).await, sha(&format!("root:{first}")));
}

#[cfg(unix)]
#[tokio::test]
async fn two_empty_repositories_in_folders_of_one_name_are_two_repositories() {
    let one = tempfile::tempdir().unwrap();
    let two = tempfile::tempdir().unwrap();
    for parent in [&one, &two] {
        let dir = parent.path().join("app");
        std::fs::create_dir(&dir).unwrap();
        git_ok(&dir, &["init", "-q"]);
    }
    let first = named_as(&one.path().join("app")).await;
    let second = named_as(&two.path().join("app")).await;
    assert_ne!(first, second);
    assert_eq!(first, named_as(&one.path().join("app")).await);
}

#[cfg(unix)]
#[tokio::test]
async fn an_odd_folder_name_is_part_of_the_name_as_it_is() {
    let parent = tempfile::tempdir().unwrap();
    for name in [
        "My App, v2",
        "quote\"and'apostrophe",
        "ünïcode-日本",
        "has\nnewline",
        "a:b;c|d",
        "-leading-dash",
        ".dotted",
        "UPPER Case",
    ] {
        let dir = parent.path().join(name);
        std::fs::create_dir(&dir).unwrap();
        git_ok(&dir, &["init", "-q"]);
        let folder = std::fs::canonicalize(&dir).unwrap();
        assert_eq!(
            named_as(&dir).await,
            sha(&format!("path:{}", folder.to_str().unwrap())),
            "{name:?}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_symlinked_folder_is_named_by_the_folder_it_leads_to() {
    let temp = tempfile::tempdir().unwrap();
    let real = temp.path().join("real");
    std::fs::create_dir(&real).unwrap();
    git_ok(&real, &["init", "-q"]);
    let link = temp.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let folder = std::fs::canonicalize(&real).unwrap();
    assert_eq!(
        named_as(&link).await,
        sha(&format!("path:{}", folder.to_str().unwrap()))
    );
    assert_eq!(named_as(&link).await, named_as(&real).await);
}
