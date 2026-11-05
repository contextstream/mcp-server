use super::*;
use std::ffi::OsString;

fn names(request: Value) -> Vec<String> {
    tool_names(&request)
}

#[test]
fn only_the_names_the_api_lists_are_asked_about() {
    assert_eq!(
        names(json!({"versions": ["node", "rustc", "protoc", "python3.12", "a_b-c.d"]})),
        ["node", "rustc", "protoc", "python3.12", "a_b-c.d"]
    );
    // Each once, in the order given.
    assert_eq!(
        names(json!({"versions": ["node", "go", "node"]})),
        ["node", "go"]
    );
    // Not names: paths, options, shell syntax, capitals, spaces, empty, too long, not text.
    let long = "a".repeat(33);
    let ok = "a".repeat(32);
    assert_eq!(
        names(json!({"versions": [
            "/usr/bin/node", "../node", "node;ls", "$(id)", "Node", "no de", "", "-h", ".hidden",
            "..", ".", "a/b", "a\\b", long, 7, null, ["node"], {"n": 1}, ok,
        ]})),
        [ok]
    );
    // None asked: nothing is.
    assert!(names(json!({})).is_empty());
    assert!(names(json!({"versions": []})).is_empty());
    assert!(names(json!({"versions": "node"})).is_empty());
    assert!(names(json!({"versions": {"node": 1}})).is_empty());
}

#[test]
fn at_most_sixteen_tools_are_asked_about() {
    let many: Vec<String> = (0..40).map(|n| format!("tool{n}")).collect();
    let asked = names(json!({ "versions": many }));
    assert_eq!(asked.len(), 16);
    assert_eq!(asked[0], "tool0");
    assert_eq!(asked[15], "tool15");
}

#[test]
fn a_tool_is_asked_in_the_commands_folder_when_that_is_inside_the_repository() {
    let repo = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(repo.path().join("pkg/inner")).unwrap();
    std::fs::write(repo.path().join("file.txt"), "x").unwrap();
    let root = repo.path();
    assert_eq!(probe_folder(root, ""), root);
    assert_eq!(probe_folder(root, "pkg"), root.join("pkg"));
    assert_eq!(probe_folder(root, "pkg/inner"), root.join("pkg/inner"));
    // Not a folder, not there, or not inside: the top of the repository.
    assert_eq!(probe_folder(root, "file.txt"), root);
    assert_eq!(probe_folder(root, "missing"), root);
    assert_eq!(probe_folder(root, "pkg/../pkg"), root);
    assert_eq!(probe_folder(root, "../pkg"), root);
    assert_eq!(probe_folder(root, "./pkg"), root);
    assert_eq!(probe_folder(root, "/tmp"), root);
    #[cfg(windows)]
    assert_eq!(probe_folder(root, "C:\\Windows"), root);
}

#[test]
fn a_tools_answer_is_its_first_line_cut_short() {
    assert_eq!(first_line(b"v24.16.0\n").as_deref(), Some("v24.16.0"));
    assert_eq!(
        first_line(b"\n  \n  rustc 1.99.0 (abc 2026-01-01)  \nsecond\n").as_deref(),
        Some("rustc 1.99.0 (abc 2026-01-01)")
    );
    assert_eq!(first_line(b"a\r\nb\r\n").as_deref(), Some("a"));
    assert_eq!(first_line(b""), None);
    assert_eq!(first_line(b"  \n\t\n"), None);
    let long = "x".repeat(5000);
    assert_eq!(first_line(long.as_bytes()).unwrap().chars().count(), 200);
    // Bytes that are not text do not break it; multi-byte characters are not cut in half.
    assert_eq!(first_line(b"v1\xff\n").as_deref(), Some("v1\u{fffd}"));
    let wide = "é".repeat(300);
    assert_eq!(first_line(wide.as_bytes()).unwrap().chars().count(), 200);
}

#[test]
fn the_time_zone_is_tz_else_the_target_of_the_local_time_link() {
    let link = Path::new;
    assert_eq!(
        time_zone(Some("America/Los_Angeles"), None),
        "America/Los_Angeles"
    );
    assert_eq!(time_zone(Some(":Europe/Berlin"), None), "Europe/Berlin");
    assert_eq!(time_zone(Some(" UTC "), None), "UTC");
    // TZ wins over the link; an empty TZ does not count.
    assert_eq!(
        time_zone(
            Some("Asia/Tokyo"),
            Some(link("/usr/share/zoneinfo/Europe/Berlin"))
        ),
        "Asia/Tokyo"
    );
    assert_eq!(
        time_zone(Some(""), Some(link("/usr/share/zoneinfo/Europe/Berlin"))),
        "Europe/Berlin"
    );
    assert_eq!(
        time_zone(None, Some(link("/usr/share/zoneinfo/Etc/UTC"))),
        "Etc/UTC"
    );
    assert_eq!(
        time_zone(None, Some(link("../usr/share/zoneinfo/Africa/Cairo"))),
        "Africa/Cairo"
    );
    assert_eq!(
        time_zone(
            None,
            Some(link("/var/db/timezone/zoneinfo/America/New_York"))
        ),
        "America/New_York"
    );
    // Unknown is empty, not a guess.
    assert_eq!(time_zone(None, None), "");
    assert_eq!(time_zone(None, Some(link("/etc/some-other-file"))), "");
    assert_eq!(time_zone(None, Some(link("/usr/share/zoneinfo/"))), "");
    assert_eq!(time_zone(Some("   "), None), "");
    let long = "z".repeat(500);
    assert_eq!(time_zone(Some(long.as_str()), None).chars().count(), 64);
}

/// Put a program called `name` in `dir`: the file a platform runs by that name.
fn plant(dir: &Path, name: &str, script: &str) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let file = if cfg!(windows) {
        dir.join(format!("{name}.exe"))
    } else {
        dir.join(name)
    };
    std::fs::write(&file, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    file
}

fn path_of(entries: &[&Path]) -> OsString {
    std::env::join_paths(entries).unwrap()
}

#[test]
fn a_program_is_found_in_the_callers_path_and_nowhere_else() {
    let scratch = tempfile::tempdir().unwrap();
    let repo = scratch.path().join("repo");
    let command_folder = repo.join("pkg");
    let installed = scratch.path().join("installed/bin");
    let wanted = plant(&installed, "faketool", "#!/bin/sh\necho installed\n");
    // What a cloned repository could carry: the program under the command's own
    // folder, the top of the repository, and a bin folder inside it.
    plant(
        &command_folder,
        "faketool",
        "#!/bin/sh\necho planted-in-cwd\n",
    );
    plant(&repo, "faketool", "#!/bin/sh\necho planted-in-root\n");
    plant(
        &repo.join("node_modules/.bin"),
        "faketool",
        "#!/bin/sh\necho planted-bin\n",
    );

    // Only the real folder of PATH counts, wherever it comes in the list.
    let path = path_of(&[&repo.join("node_modules/.bin"), &installed]);
    assert_eq!(
        find_program("faketool", Some(path.as_os_str()), &repo),
        Some(wanted)
    );

    // No folder of PATH outside the repository has it: not found, though the
    // repository has it in three places.
    for entries in [
        vec![repo.join("node_modules/.bin")],
        vec![repo.clone()],
        vec![command_folder.clone()],
        vec![],
    ] {
        let refs: Vec<&Path> = entries.iter().map(PathBuf::as_path).collect();
        assert_eq!(
            find_program("faketool", Some(path_of(&refs).as_os_str()), &repo),
            None,
            "{entries:?}"
        );
    }
    assert_eq!(find_program("faketool", None, &repo), None);
    assert_eq!(
        find_program("othertool", Some(path_of(&[&installed]).as_os_str()), &repo),
        None
    );
}

#[test]
fn a_relative_or_empty_folder_of_path_is_never_the_commands_folder() {
    let scratch = tempfile::tempdir().unwrap();
    let repo = scratch.path().join("repo");
    plant(&repo.join("pkg"), "faketool", "#!/bin/sh\necho planted\n");
    plant(&repo, "faketool", "#!/bin/sh\necho planted\n");
    // Whatever these mean to a shell (here, the folder the command runs in), the
    // probe does not take them as places to look.
    let previous = std::env::current_dir().unwrap();
    for relative in [".", "", "pkg", "./pkg", "..", "bin"] {
        let path = std::env::join_paths([OsString::from(relative)]).unwrap();
        assert_eq!(
            find_program("faketool", Some(path.as_os_str()), &repo),
            None,
            "{relative:?}"
        );
    }
    assert_eq!(std::env::current_dir().unwrap(), previous);
}

#[cfg(unix)]
#[test]
fn a_link_into_the_repository_or_a_file_that_does_not_run_is_not_a_program() {
    let scratch = tempfile::tempdir().unwrap();
    let repo = scratch.path().join("repo");
    let bin = scratch.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let planted = plant(&repo.join("tools"), "faketool", "#!/bin/sh\necho planted\n");
    // A real-looking program in PATH that is a link into the repository.
    std::os::unix::fs::symlink(&planted, bin.join("faketool")).unwrap();
    let path = path_of(&[&bin]);
    assert_eq!(
        find_program("faketool", Some(path.as_os_str()), &repo),
        None
    );

    // A link to a program outside the repository is fine (version managers use them).
    let outside = plant(
        &scratch.path().join("managed"),
        "real",
        "#!/bin/sh\necho ok\n",
    );
    std::os::unix::fs::symlink(&outside, bin.join("shimmed")).unwrap();
    assert_eq!(
        find_program("shimmed", Some(path.as_os_str()), &repo),
        Some(bin.join("shimmed"))
    );

    // A file without the execute bit, and a folder, are not programs.
    std::fs::write(bin.join("plain"), "data").unwrap();
    std::fs::create_dir_all(bin.join("adir")).unwrap();
    assert_eq!(find_program("plain", Some(path.as_os_str()), &repo), None);
    assert_eq!(find_program("adir", Some(path.as_os_str()), &repo), None);
}

#[cfg(windows)]
#[test]
fn only_programs_that_run_by_themselves_are_looked_for_on_windows() {
    let scratch = tempfile::tempdir().unwrap();
    let repo = scratch.path().join("repo");
    let bin = scratch.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("faketool.cmd"), "@echo off").unwrap();
    std::fs::write(bin.join("faketool.bat"), "@echo off").unwrap();
    let path = path_of(&[&bin]);
    assert_eq!(
        find_program("faketool", Some(path.as_os_str()), &repo),
        None
    );
    let exe = plant(&bin, "faketool", "MZ");
    assert_eq!(
        find_program("faketool", Some(path.as_os_str()), &repo),
        Some(exe)
    );
}

/// A script that stands in for a tool. Retried once when the system says the file
/// is still busy from another test's fork (a flake of writing then running files).
#[cfg(unix)]
async fn ask(script: &str, limit: Duration) -> Option<String> {
    let scratch = tempfile::tempdir().unwrap();
    let file = plant(
        scratch.path(),
        "faketool",
        &format!("#!/bin/sh\n{script}\n"),
    );
    for _ in 0..3 {
        let answer = version_of(file.as_os_str(), scratch.path(), limit).await;
        if answer.is_some() {
            return answer;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
}

#[cfg(unix)]
#[tokio::test]
async fn a_tool_is_asked_its_version_and_only_that() {
    let limit = Duration::from_secs(5);
    assert_eq!(
        ask("echo 'faketool 1.2.3'; echo second", limit)
            .await
            .as_deref(),
        Some("faketool 1.2.3")
    );
    // Standard error when standard output has nothing.
    assert_eq!(
        ask("echo 'java 21' >&2", limit).await.as_deref(),
        Some("java 21")
    );
    // One argument, `--version`, and no shell: nothing in it is interpreted.
    assert_eq!(
        ask("echo \"$# $1\"", limit).await.as_deref(),
        Some("1 --version")
    );
    // Told not to install anything.
    assert_eq!(
        ask(
            "echo \"$RUSTUP_AUTO_INSTALL $COREPACK_ENABLE_DOWNLOAD_PROMPT\"",
            limit
        )
        .await
        .as_deref(),
        Some("0 0")
    );
    // Nothing to read from: standard input is closed.
    assert_eq!(
        ask(
            "if read line; then echo has-input; else echo no-input; fi",
            limit
        )
        .await
        .as_deref(),
        Some("no-input")
    );
    // An answer of any size is cut to a line.
    let long = ask("head -c 200000 /dev/zero | tr '\\0' 'a'; echo", limit)
        .await
        .unwrap();
    assert_eq!(long.chars().count(), 200);
}

#[cfg(unix)]
#[tokio::test]
async fn a_tool_that_fails_is_missing_or_is_slow_has_no_version() {
    let limit = Duration::from_secs(5);
    let scratch = tempfile::tempdir().unwrap();
    assert_eq!(ask("echo 'oops'; exit 1", limit).await, None);
    assert_eq!(ask("exit 0", limit).await, None);
    assert_eq!(
        version_of(
            scratch.path().join("no-such-program").as_os_str(),
            scratch.path(),
            limit
        )
        .await,
        None
    );
    // Slow: given up on at the limit, and the wait is the limit, not the sleep.
    let started = std::time::Instant::now();
    assert_eq!(ask("exec sleep 30", Duration::from_millis(300)).await, None);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

#[cfg(unix)]
#[tokio::test]
async fn a_tool_is_asked_in_the_folder_it_is_given() {
    let scratch = tempfile::tempdir().unwrap();
    let folder = scratch.path().join("work").join("pkg");
    std::fs::create_dir_all(&folder).unwrap();
    let tool = plant(scratch.path(), "faketool", "#!/bin/sh\npwd -P\n");
    let mut answer = None;
    for _ in 0..3 {
        answer = version_of(tool.as_os_str(), &folder, Duration::from_secs(5)).await;
        if answer.is_some() {
            break;
        }
    }
    assert_eq!(
        answer.as_deref(),
        Some(folder.canonicalize().unwrap().to_str().unwrap())
    );
}

#[cfg(unix)]
#[tokio::test]
async fn the_environment_holds_the_machine_and_the_tools_asked_about() {
    let scratch = tempfile::tempdir().unwrap();
    let repo = scratch.path().join("repo");
    std::fs::create_dir_all(repo.join("pkg")).unwrap();
    let tool = plant(
        &scratch.path().join("installed"),
        "faketool",
        "#!/bin/sh\necho \"faketool 1.2.3\"\n",
    );
    let request = json!({
        "cwd": "pkg",
        "versions": ["faketool", "missing", "Bad Name", "faketool"],
    });
    let mut environment = Value::Null;
    for _ in 0..3 {
        environment = describe_using(&request, &repo, |name| {
            (name == "faketool").then(|| tool.clone())
        })
        .await;
        if environment["versions"]["faketool"].is_string() {
            break;
        }
    }
    let object = environment.as_object().unwrap();
    let mut keys: Vec<_> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    // Nothing else is in it: no variable, no path, nothing of the checkout.
    assert_eq!(keys, ["cpus", "platform", "root", "tz", "versions"]);
    assert_eq!(
        environment["platform"],
        json!({"os": std::env::consts::OS, "arch": std::env::consts::ARCH})
    );
    assert!(environment["cpus"].as_u64().unwrap() >= 1);
    assert_eq!(environment["root"], unsafe { libc::geteuid() } == 0);
    assert!(environment["tz"].is_string());
    assert_eq!(
        environment["versions"],
        json!({"faketool": "faketool 1.2.3", "missing": null})
    );

    // No tools named: the key is there and empty.
    let none = describe_using(&json!({"cwd": "pkg"}), &repo, |_| None).await;
    assert_eq!(none["versions"], json!({}));
}

#[tokio::test]
async fn the_switch_sends_no_environment() {
    // The only test that sets the variable; none of the others reads it.
    let request = json!({"versions": []});
    let scratch = tempfile::tempdir().unwrap();
    let previous = std::env::var_os(ENV_SWITCH);
    for off in ["off", "OFF", "0", "false", "no", "disabled"] {
        std::env::set_var(ENV_SWITCH, off);
        assert!(describe(&request, scratch.path()).await.is_none(), "{off}");
    }
    for on in ["on", "1", "true", ""] {
        std::env::set_var(ENV_SWITCH, on);
        assert!(describe(&request, scratch.path()).await.is_some(), "{on:?}");
    }
    std::env::remove_var(ENV_SWITCH);
    assert!(describe(&request, scratch.path()).await.is_some());
    if let Some(previous) = previous {
        std::env::set_var(ENV_SWITCH, previous);
    }
}
