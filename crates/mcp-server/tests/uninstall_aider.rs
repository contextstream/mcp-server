//! `uninstall` leaves nothing of ContextStream's behind for Aider (#322): the
//! config files setup created, the backups a refresh leaves, and the shared
//! rules file they point at. Runs the real binary through offline commands.

use std::{
    path::Path,
    process::{Command, Output},
};

fn run(home: &Path, cwd: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"))
        .args(args)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("NO_COLOR", "1")
        .env("CONTEXTSTREAM_WATCH", "0")
        .env("CONTEXTSTREAM_API_URL", "http://127.0.0.1:9")
        .env_remove("CONTEXTSTREAM_API_KEY")
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

fn aider_leftovers(home: &Path, project: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for root in [home, project] {
        for entry in std::fs::read_dir(root).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.contains("aider") {
                found.push(
                    entry
                        .path()
                        .strip_prefix(home)
                        .unwrap_or(&entry.path())
                        .display()
                        .to_string(),
                );
            }
        }
    }
    // `~/.contextstream` is left in place by design (credentials live there).
    let shared = project.join(".contextstream/rules.md");
    if shared.exists() {
        found.push(shared.display().to_string());
    }
    found.sort();
    found
}

#[test]
fn uninstall_removes_every_aider_file_that_setup_and_its_refreshes_created() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let project = home.join("app");
    std::fs::create_dir_all(&project).unwrap();
    assert!(Command::new("git")
        .args(["init", "-q"])
        .current_dir(&project)
        .status()
        .unwrap()
        .success());
    std::fs::write(project.join("README.md"), "# app\n").unwrap();
    assert!(
        aider_leftovers(&home, &project).is_empty(),
        "{:?}",
        aider_leftovers(&home, &project)
    );

    let write_rules = || {
        let output = run(
            &home,
            &project,
            &["update-rules", "--scope", "all", "--editors", "aider"],
        );
        assert!(
            output.status.success(),
            "update-rules failed:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    };
    write_rules();
    let created = aider_leftovers(&home, &project);
    assert!(
        created.iter().any(|path| path.ends_with(".aider.conf.yml")),
        "update-rules should have written the aider configs: {created:?}"
    );

    // A refresh from an older release leaves a backup of each managed file.
    for conf in [
        home.join(".aider.conf.yml"),
        project.join(".aider.conf.yml"),
    ] {
        let older = std::fs::read_to_string(&conf).unwrap().replace(
            "contextstream-rules-hash: ",
            "contextstream-rules-hash: 0000",
        );
        std::fs::write(&conf, older).unwrap();
    }
    write_rules();
    write_rules();
    let refreshed = aider_leftovers(&home, &project);
    assert!(
        refreshed
            .iter()
            .any(|path| path.ends_with(".contextstream.bak")),
        "a refresh should leave backups: {refreshed:?}"
    );

    let output = run(
        &home,
        &project,
        &["uninstall", "--scope", "all", "--editors", "aider"],
    );
    assert!(
        output.status.success(),
        "uninstall failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        aider_leftovers(&home, &project),
        Vec::<String>::new(),
        "uninstall output:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
