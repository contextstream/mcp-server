//! `CONTEXTSTREAM_WATCH=0` is the one opt-out for the sync bridge: under it
//! no command starts a `contextstream-mcp watch` or registers its service.
//!
//! Temporary homes and folders only. The API refuses every connection, and a
//! stand-in `systemctl` (and `launchctl`) first on `PATH` records what would
//! have reached the service manager, so nothing here touches the real one.
//! Every watcher a run starts is stopped.
#![cfg(target_os = "linux")]

#[path = "support/watchers.rs"]
mod watchers;

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

/// A dashboard connection token: well formed, never redeemed here.
const TOKEN: &str = "csbe_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

/// A signed-in machine in a temporary folder.
struct Machine {
    _temp: tempfile::TempDir,
    home: PathBuf,
    repo: PathBuf,
    tools: PathBuf,
    calls: PathBuf,
    api_url: String,
}

impl Machine {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().canonicalize().expect("canonical tempdir");
        let (home, repo, tools) = (root.join("home"), root.join("repo"), root.join("tools"));
        for dir in [&home, &repo, &tools] {
            std::fs::create_dir_all(dir).expect("mkdir");
        }
        // An API that refuses every connection: a port just let go.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|listener| listener.local_addr())
            .expect("a free port")
            .port();
        let api_url = format!("http://127.0.0.1:{port}");
        std::fs::create_dir_all(home.join(".contextstream")).expect("mkdir");
        std::fs::write(
            home.join(".contextstream/credentials.json"),
            serde_json::json!({"api_key": "cs_watch_opt_out_test_key", "api_url": api_url})
                .to_string(),
        )
        .expect("credentials");
        let calls = root.join("service-manager-calls.log");
        for tool in ["systemctl", "launchctl"] {
            let path = tools.join(tool);
            std::fs::write(
                &path,
                format!("#!/bin/sh\necho \"{tool} $*\" >> '{}'\n", calls.display()),
            )
            .expect("stand-in");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        let git = Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .expect("git init");
        assert!(git.success());
        Self {
            _temp: temp,
            home,
            repo,
            tools,
            calls,
            api_url,
        }
    }

    /// Run the CLI here, with `CONTEXTSTREAM_WATCH` set to `watch` if given.
    fn run(&self, args: &[&str], watch: Option<&str>) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_contextstream-mcp"));
        command
            .args(args)
            .env_clear()
            .env("HOME", &self.home)
            .env("PATH", format!("{}:/usr/bin:/bin", self.tools.display()))
            .env("CONTEXTSTREAM_API_URL", &self.api_url)
            .env("CONTEXTSTREAM_NO_BROWSER", "1")
            .env("NO_COLOR", "1")
            .current_dir(&self.repo)
            .stdin(Stdio::null());
        if let Some(value) = watch {
            command.env("CONTEXTSTREAM_WATCH", value);
        }
        command.output().expect("run the CLI")
    }

    fn service_manager_calls(&self) -> String {
        std::fs::read_to_string(&self.calls).unwrap_or_default()
    }

    fn service_file(&self) -> PathBuf {
        self.home
            .join(".config/systemd/user/contextstream-watch.service")
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        for watcher in watchers::watchers_for(&self.home) {
            watchers::stop(watcher.pid);
        }
    }
}

fn said(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn under_the_opt_out_no_command_starts_a_watcher_or_its_service() {
    let machine = Machine::new();
    let repo = machine.repo.to_string_lossy().into_owned();
    let commands: [&[&str]; 4] = [
        &["update-hooks", "--scope=global", "--editors=claude"],
        &["migrate-remote", "--scope=all", "--editors=claude"],
        &["link"],
        &["connect", "--token", TOKEN, "--path", &repo],
    ];
    for args in commands {
        let output = machine.run(args, Some("0"));
        // A detached watcher shows up within moments of its command.
        std::thread::sleep(Duration::from_millis(500));
        let left = watchers::watchers_for(&machine.home);
        assert!(
            left.is_empty(),
            "{args:?} started {left:?}:\n{}",
            said(&output)
        );
        assert_eq!(
            machine.service_manager_calls(),
            "",
            "{args:?} reached the service manager:\n{}",
            said(&output)
        );
        assert!(
            !machine.service_file().exists(),
            "{args:?} registered the service:\n{}",
            said(&output)
        );
    }

    // `connect` starts the bridge and waits for its first sync, so under the
    // opt-out it says why and stops before it spends the link.
    let output = machine.run(&["connect", "--token", TOKEN, "--path", &repo], Some("0"));
    let text = said(&output);
    assert!(!output.status.success(), "{text}");
    assert!(text.contains("CONTEXTSTREAM_WATCH=0"), "{text}");
    assert!(
        text.contains("the connection link is still unused"),
        "{text}"
    );
    assert!(!text.contains("Connecting"), "{text}");
}

/// The check above can fail: without the opt-out, the same refresh registers
/// the service (with the stand-in) and starts a watcher, which this test sees
/// and stops.
#[test]
fn without_the_opt_out_a_refresh_starts_a_watcher_this_test_sees() {
    let machine = Machine::new();
    let output = machine.run(
        &["update-hooks", "--scope=global", "--editors=claude"],
        None,
    );
    assert!(output.status.success(), "{}", said(&output));
    let started = watchers::wait_for_watcher(&machine.home, Duration::from_secs(10));
    assert!(
        !started.is_empty(),
        "no watcher started:\n{}",
        said(&output)
    );
    assert!(
        machine
            .service_manager_calls()
            .contains("systemctl --user enable --now contextstream-watch.service"),
        "{}",
        machine.service_manager_calls()
    );
    assert!(machine.service_file().is_file());
    for watcher in started {
        watchers::stop(watcher.pid);
    }
    assert!(watchers::watchers_for(&machine.home).is_empty());
}
