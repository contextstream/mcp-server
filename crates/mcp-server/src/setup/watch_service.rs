//! Persistent startup registration for the hosted MCP sync bridge.
//!
//! The editor continues to talk to the hosted MCP gateway. This module only
//! arranges for the managed helper on the user's machine to run
//! `contextstream-mcp watch`, because that helper is the component that can
//! safely read local checkouts and upload changes. Every registration file is
//! wholly generated, carries an exact ownership marker, and is removed only
//! when its complete content still matches what this version would generate.

use std::path::{Path, PathBuf};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde::Serialize;

use super::{hooks::managed_binary_path, safe_edit};

const OWNERSHIP_MARKER: &str = "# ContextStream managed hosted sync bridge: hosted-sync-bridge-v1";
const MAX_REGISTRATION_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncBridgeRegistrationState {
    Registered,
    Missing,
    Conflict,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncBridgeActivationState {
    Active,
    Deferred,
    DryRun,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SyncBridgeServiceRegistration {
    pub state: SyncBridgeRegistrationState,
    pub activation: SyncBridgeActivationState,
    pub platform: &'static str,
    pub changed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Each target OS constructs only its own variant.
enum RegistrationPlatform {
    LinuxSystemd,
    MacLaunchd,
    WindowsStartup,
    Unsupported,
}

impl RegistrationPlatform {
    const fn label(self) -> &'static str {
        match self {
            Self::LinuxSystemd => "linux_systemd_user",
            Self::MacLaunchd => "macos_launch_agent",
            Self::WindowsStartup => "windows_startup",
            Self::Unsupported => "unsupported",
        }
    }
}

#[derive(Debug, Clone)]
struct RegistrationSpec {
    platform: RegistrationPlatform,
    path: PathBuf,
    content: String,
}

fn current_platform() -> RegistrationPlatform {
    #[cfg(target_os = "linux")]
    {
        return RegistrationPlatform::LinuxSystemd;
    }
    #[cfg(target_os = "macos")]
    {
        return RegistrationPlatform::MacLaunchd;
    }
    #[cfg(windows)]
    {
        return RegistrationPlatform::WindowsStartup;
    }
    #[allow(unreachable_code)]
    RegistrationPlatform::Unsupported
}

fn registration_spec() -> Result<Option<RegistrationSpec>> {
    let platform = current_platform();
    let binary = managed_binary_path();
    match platform {
        RegistrationPlatform::LinuxSystemd => {
            let config_dir = dirs::config_dir()
                .context("Could not determine the user configuration directory")?;
            Ok(Some(RegistrationSpec {
                platform,
                path: config_dir
                    .join("systemd")
                    .join("user")
                    .join("contextstream-watch.service"),
                content: render_systemd_unit(&binary)?,
            }))
        }
        RegistrationPlatform::MacLaunchd => {
            let home = dirs::home_dir().context("Could not determine the home directory")?;
            Ok(Some(RegistrationSpec {
                platform,
                path: home
                    .join("Library")
                    .join("LaunchAgents")
                    .join("io.contextstream.sync-bridge.plist"),
                content: render_launch_agent(&binary)?,
            }))
        }
        RegistrationPlatform::WindowsStartup => {
            let config_dir = dirs::config_dir()
                .context("Could not determine the user configuration directory")?;
            Ok(Some(RegistrationSpec {
                platform,
                path: config_dir
                    .join("Microsoft")
                    .join("Windows")
                    .join("Start Menu")
                    .join("Programs")
                    .join("Startup")
                    .join("ContextStream Sync Bridge.vbs"),
                content: render_windows_startup(&binary)?,
            }))
        }
        RegistrationPlatform::Unsupported => Ok(None),
    }
}

fn checked_utf8_path(path: &Path) -> Result<&str> {
    let path = path
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Managed helper path is not valid UTF-8"))?;
    if path.chars().any(|character| character.is_control()) {
        bail!("Managed helper path contains a control character");
    }
    Ok(path)
}

fn quote_systemd_argument(path: &Path) -> Result<String> {
    let mut escaped = String::new();
    for character in checked_utf8_path(path)?.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '%' => escaped.push_str("%%"),
            '$' => escaped.push_str("$$"),
            other => escaped.push(other),
        }
    }
    Ok(format!("\"{escaped}\""))
}

fn render_systemd_unit(binary: &Path) -> Result<String> {
    Ok(format!(
        "{OWNERSHIP_MARKER}\n\
[Unit]\n\
Description=ContextStream hosted MCP sync bridge\n\
After=network-online.target\n\
\n\
[Service]\n\
Type=simple\n\
ExecStart={} watch\n\
Restart=on-failure\n\
RestartSec=5s\n\
Environment=CONTEXTSTREAM_WATCH=1\n\
NoNewPrivileges=true\n\
\n\
[Install]\n\
WantedBy=default.target\n",
        quote_systemd_argument(binary)?
    ))
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn render_launch_agent(binary: &Path) -> Result<String> {
    let binary = xml_escape(checked_utf8_path(binary)?);
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
\"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<!-- {OWNERSHIP_MARKER} -->\n\
<plist version=\"1.0\">\n\
<dict>\n\
  <key>Label</key>\n\
  <string>io.contextstream.sync-bridge</string>\n\
  <key>ProgramArguments</key>\n\
  <array>\n\
    <string>{binary}</string>\n\
    <string>watch</string>\n\
  </array>\n\
  <key>RunAtLoad</key>\n\
  <true/>\n\
  <key>KeepAlive</key>\n\
  <true/>\n\
  <key>ProcessType</key>\n\
  <string>Background</string>\n\
</dict>\n\
</plist>\n"
    ))
}

fn render_windows_startup(binary: &Path) -> Result<String> {
    let binary = checked_utf8_path(binary)?.replace('"', "\"\"");
    Ok(format!(
        "' {OWNERSHIP_MARKER}\r\n\
Option Explicit\r\n\
Dim shell, fileSystem, command\r\n\
Set shell = CreateObject(\"WScript.Shell\")\r\n\
Set fileSystem = CreateObject(\"Scripting.FileSystemObject\")\r\n\
command = \"\"\"{binary}\"\" watch\"\r\n\
Do\r\n\
  shell.Run command, 0, True\r\n\
  If Not fileSystem.FileExists(WScript.ScriptFullName) Then WScript.Quit 0\r\n\
  WScript.Sleep 5000\r\n\
Loop\r\n"
    ))
}

fn read_registration(path: &Path) -> Result<Option<String>> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!(
                    "Could not inspect sync bridge registration {}",
                    path.display()
                )
            })
        }
    };
    if metadata.file_type().is_symlink() {
        bail!(
            "Refusing to trust sync bridge registration {} because it is a symlink",
            path.display()
        );
    }
    if !metadata.is_file() {
        bail!(
            "Refusing to trust sync bridge registration {} because it is not a regular file",
            path.display()
        );
    }
    if metadata.len() > MAX_REGISTRATION_BYTES {
        bail!(
            "Refusing to read oversized sync bridge registration {}",
            path.display()
        );
    }
    std::fs::read_to_string(path)
        .with_context(|| format!("Could not read sync bridge registration {}", path.display()))
        .map(Some)
}

fn status_for_spec(spec: &RegistrationSpec) -> Result<SyncBridgeRegistrationState> {
    match read_registration(&spec.path)? {
        None => Ok(SyncBridgeRegistrationState::Missing),
        Some(existing) if existing == spec.content => Ok(SyncBridgeRegistrationState::Registered),
        Some(_) => Ok(SyncBridgeRegistrationState::Conflict),
    }
}

pub fn sync_bridge_registration_status() -> Result<SyncBridgeServiceRegistration> {
    let Some(spec) = registration_spec()? else {
        return Ok(SyncBridgeServiceRegistration {
            state: SyncBridgeRegistrationState::Unsupported,
            activation: SyncBridgeActivationState::Unsupported,
            platform: RegistrationPlatform::Unsupported.label(),
            changed: false,
        });
    };
    Ok(SyncBridgeServiceRegistration {
        state: status_for_spec(&spec)?,
        activation: SyncBridgeActivationState::Deferred,
        platform: spec.platform.label(),
        changed: false,
    })
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_silently(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

#[cfg(target_os = "linux")]
fn activate_registration(_spec: &RegistrationSpec, force_restart: bool) -> bool {
    let reloaded = run_silently("systemctl", &["--user", "daemon-reload"]);
    let enabled = run_silently(
        "systemctl",
        &["--user", "enable", "--now", "contextstream-watch.service"],
    );
    let restarted = !force_restart
        || run_silently(
            "systemctl",
            &["--user", "restart", "contextstream-watch.service"],
        );
    reloaded && enabled && restarted
}

#[cfg(target_os = "macos")]
fn activate_registration(spec: &RegistrationSpec, force_restart: bool) -> bool {
    let domain = format!("gui/{}", unsafe { libc::geteuid() });
    activate_launch_agent_with(spec, &domain, force_restart, run_silently)
}

#[cfg(any(target_os = "macos", test))]
fn activate_launch_agent_with(
    spec: &RegistrationSpec,
    domain: &str,
    force_restart: bool,
    mut run: impl FnMut(&str, &[&str]) -> bool,
) -> bool {
    let Ok(path) = checked_utf8_path(&spec.path) else {
        return false;
    };
    let service = format!("{domain}/io.contextstream.sync-bridge");
    let loaded =
        run("launchctl", &["bootstrap", domain, path]) || run("launchctl", &["print", &service]);
    if !loaded {
        return false;
    }
    !force_restart || run("launchctl", &["kickstart", "-k", &service])
}

#[cfg(windows)]
fn activate_registration(_spec: &RegistrationSpec, _force_restart: bool) -> bool {
    // The Startup folder takes effect at the next login. Setup also launches
    // the singleton directly, so the current session does not wait for that.
    false
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn activate_registration(_spec: &RegistrationSpec, _force_restart: bool) -> bool {
    false
}

#[cfg(target_os = "linux")]
fn deactivate_registration(_spec: &RegistrationSpec) {
    let _ = run_silently(
        "systemctl",
        &["--user", "disable", "--now", "contextstream-watch.service"],
    );
}

#[cfg(target_os = "macos")]
fn deactivate_registration(_spec: &RegistrationSpec) {
    let service = format!("gui/{}/io.contextstream.sync-bridge", unsafe {
        libc::geteuid()
    });
    let _ = run_silently("launchctl", &["bootout", &service]);
}

#[cfg(any(windows, not(any(target_os = "linux", target_os = "macos", windows))))]
fn deactivate_registration(_spec: &RegistrationSpec) {}

#[cfg(target_os = "linux")]
fn reload_after_removal() {
    let _ = run_silently("systemctl", &["--user", "daemon-reload"]);
}

#[cfg(not(target_os = "linux"))]
fn reload_after_removal() {}

pub fn register_managed_sync_bridge() -> Result<SyncBridgeServiceRegistration> {
    let Some(spec) = registration_spec()? else {
        return Ok(SyncBridgeServiceRegistration {
            state: SyncBridgeRegistrationState::Unsupported,
            activation: SyncBridgeActivationState::Unsupported,
            platform: RegistrationPlatform::Unsupported.label(),
            changed: false,
        });
    };
    if status_for_spec(&spec)? == SyncBridgeRegistrationState::Conflict {
        bail!(
            "Refusing to replace sync bridge registration {} because it is not the exact ContextStream-managed file",
            spec.path.display()
        );
    }
    let existing = read_registration(&spec.path)?;
    let health = crate::watch::sync_bridge_health();
    let force_restart = existing.is_some()
        && health
            .version
            .as_deref()
            .is_some_and(|version| version != mcp_types::config::VERSION);
    let transfer_existing_helper = existing.is_none() && health.lock_held == Some(true);
    let changed =
        safe_edit::write_owned_file_if_unchanged(&spec.path, &spec.content, existing.as_deref())?;
    if (force_restart || transfer_existing_helper) && !safe_edit::is_dry_run() {
        // A prior setup may have launched the singleton directly before the
        // service manager owned it, or an older managed version may still be
        // running. Ask that exact lock owner to exit first, then give its
        // 500ms control tick a short bounded window before starting the
        // replacement service.
        let _ = crate::watch::request_sync_bridge_stop();
        for _ in 0..20 {
            let health = crate::watch::sync_bridge_health();
            if health.lock_held != Some(true) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    let activation = if safe_edit::is_dry_run() {
        SyncBridgeActivationState::DryRun
    } else if activate_registration(&spec, force_restart) {
        SyncBridgeActivationState::Active
    } else {
        SyncBridgeActivationState::Deferred
    };
    Ok(SyncBridgeServiceRegistration {
        state: SyncBridgeRegistrationState::Registered,
        activation,
        platform: spec.platform.label(),
        changed,
    })
}

pub fn unregister_managed_sync_bridge() -> Result<SyncBridgeServiceRegistration> {
    let Some(spec) = registration_spec()? else {
        return Ok(SyncBridgeServiceRegistration {
            state: SyncBridgeRegistrationState::Unsupported,
            activation: SyncBridgeActivationState::Unsupported,
            platform: RegistrationPlatform::Unsupported.label(),
            changed: false,
        });
    };
    let Some(existing) = read_registration(&spec.path)? else {
        return Ok(SyncBridgeServiceRegistration {
            state: SyncBridgeRegistrationState::Missing,
            activation: SyncBridgeActivationState::Deferred,
            platform: spec.platform.label(),
            changed: false,
        });
    };
    if existing != spec.content {
        bail!(
            "Refusing to remove sync bridge registration {} because it is not the exact ContextStream-managed file",
            spec.path.display()
        );
    }
    if !safe_edit::is_dry_run() {
        deactivate_registration(&spec);
    }
    let changed = safe_edit::remove_owned_file_if_unchanged(&spec.path, existing.as_str())?;
    if !safe_edit::is_dry_run() {
        reload_after_removal();
    }
    Ok(SyncBridgeServiceRegistration {
        state: SyncBridgeRegistrationState::Missing,
        activation: if safe_edit::is_dry_run() {
            SyncBridgeActivationState::DryRun
        } else {
            SyncBridgeActivationState::Deferred
        },
        platform: spec.platform.label(),
        changed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_at(path: PathBuf, content: String) -> RegistrationSpec {
        RegistrationSpec {
            platform: RegistrationPlatform::LinuxSystemd,
            path,
            content,
        }
    }

    #[test]
    fn platform_renderers_escape_paths_without_a_shell() {
        let path = Path::new("/home/A & B/$cache/%slot/ctx\"helper");
        let systemd = render_systemd_unit(path).expect("systemd");
        assert!(systemd.contains("\"/home/A & B/$$cache/%%slot/ctx\\\"helper\" watch"));

        let launchd = render_launch_agent(path).expect("launchd");
        assert!(launchd.contains("/home/A &amp; B/$cache/%slot/ctx&quot;helper"));
        assert!(launchd.contains("<string>watch</string>"));

        let windows = render_windows_startup(Path::new(r"C:\Users\100%\Context Stream\mcp.exe"))
            .expect("windows");
        assert!(windows.contains(r#"command = """C:\Users\100%\Context Stream\mcp.exe"" watch""#));
        assert!(windows.contains("shell.Run command, 0, True"));
        assert!(windows
            .contains("If Not fileSystem.FileExists(WScript.ScriptFullName) Then WScript.Quit 0"));
        assert!(!windows.contains("@echo off"));
    }

    #[test]
    fn launch_agent_activation_requires_a_loaded_service_and_verifies_restarts() {
        let spec = spec_at(
            PathBuf::from("/Users/alice/Library/LaunchAgents/bridge.plist"),
            "managed".to_string(),
        );

        let mut unavailable_calls = Vec::new();
        let unavailable =
            activate_launch_agent_with(&spec, "gui/501", false, |program, arguments| {
                unavailable_calls.push(format!("{program} {}", arguments.join(" ")));
                false
            });
        assert!(!unavailable);
        assert_eq!(unavailable_calls.len(), 2);
        assert!(unavailable_calls[0].contains("bootstrap"));
        assert!(unavailable_calls[1].contains("print"));

        let already_loaded =
            activate_launch_agent_with(&spec, "gui/501", false, |_program, arguments| {
                arguments.first() == Some(&"print")
            });
        assert!(already_loaded);

        let mut restarted = false;
        let restarted_successfully = activate_launch_agent_with(
            &spec,
            "gui/501",
            true,
            |_program, arguments| match arguments.first().copied() {
                Some("bootstrap") => false,
                Some("print") => true,
                Some("kickstart") => {
                    restarted = true;
                    true
                }
                _ => false,
            },
        );
        assert!(restarted_successfully);
        assert!(restarted);

        let restart_failed =
            activate_launch_agent_with(&spec, "gui/501", true, |_program, arguments| {
                arguments.first() == Some(&"print")
            });
        assert!(!restart_failed);
    }

    #[test]
    fn owned_registration_round_trips_but_foreign_or_modified_files_are_preserved() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("contextstream-watch.service");
        let content = render_systemd_unit(Path::new("/managed/contextstream-mcp")).unwrap();
        let spec = spec_at(path.clone(), content.clone());

        assert_eq!(
            status_for_spec(&spec).unwrap(),
            SyncBridgeRegistrationState::Missing
        );
        assert!(safe_edit::write_owned_file_if_unchanged(&path, &content, None).unwrap());
        assert_eq!(
            status_for_spec(&spec).unwrap(),
            SyncBridgeRegistrationState::Registered
        );
        assert!(safe_edit::remove_owned_file_if_unchanged(&path, &content).unwrap());
        assert_eq!(
            status_for_spec(&spec).unwrap(),
            SyncBridgeRegistrationState::Missing
        );

        std::fs::write(&path, "[Unit]\nDescription=user service\n").unwrap();
        assert_eq!(
            status_for_spec(&spec).unwrap(),
            SyncBridgeRegistrationState::Conflict
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "[Unit]\nDescription=user service\n"
        );

        std::fs::write(&path, format!("{content}# user change\n")).unwrap();
        assert_eq!(
            status_for_spec(&spec).unwrap(),
            SyncBridgeRegistrationState::Conflict
        );
    }
}
