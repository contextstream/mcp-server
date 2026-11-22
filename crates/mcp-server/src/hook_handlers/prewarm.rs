//! Predictive remote workspace pre-warming (`:prepare`) from editor hooks.
//!
//! Editor hooks run as one-shot processes whose Tokio runtime exits as soon as
//! the hook returns. `maybe_spawn` checks local gates in <1ms and launches a
//! detached `contextstream-mcp hook hosted-prepare` process group so the hook
//! never blocks on network or snapshot work.

use anyhow::Result;
use mcp_client::{command_hints, ContextStreamClient};
use mcp_session::SessionManager;
use serde_json::Value;
use std::path::Path;
use std::process::{Command, Stdio};

use super::common;

pub(crate) const PREPARE_HOOK: &str = "hosted-prepare";

pub(crate) fn can_prewarm_locally(cwd: &str) -> bool {
    if cwd.trim().is_empty() {
        return false;
    }
    if command_hints::switched_off(std::env::var(command_hints::ENV_SWITCH).ok().as_deref()) {
        return false;
    }
    let config = common::load_config(cwd);
    if !config.is_configured() {
        return false;
    }
    let identity = command_hints::local_identity(&config.api_key);
    if let Some(cache) = command_hints::read(&identity, &config.api_url) {
        if cache.is_fresh(mcp_client::hosted_tools::KNOWN_TTL)
            && cache.tool_named("check").is_none()
        {
            return false;
        }
    }
    true
}

/// Spawn a detached background `:prepare` worker for `cwd` without blocking the hook.
pub(crate) fn maybe_spawn(cwd: &str) {
    if cfg!(test) || !can_prewarm_locally(cwd) {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut command = Command::new(exe);
    command
        .args(["hook", PREPARE_HOOK])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    let Ok(mut child) = command.spawn() else {
        return;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = serde_json::to_writer(&mut stdin, &serde_json::json!({ "cwd": cwd }));
    }
}

/// Detached worker entrypoint for `contextstream-mcp hook hosted-prepare`.
pub(crate) async fn run_background_prepare() -> Result<()> {
    let job = super::read_stdin_json().unwrap_or(Value::Null);
    let cwd = job
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(String::from)
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .and_then(|p| p.to_str().map(String::from))
        })
        .unwrap_or_default();
    if cwd.is_empty() {
        return Ok(());
    }
    let config = common::load_config(&cwd);
    if !config.is_configured() {
        return Ok(());
    }
    let client_config = mcp_types::Config {
        api_url: config.api_url,
        api_key: Some(config.api_key),
        default_workspace_id: config
            .workspace_id
            .as_ref()
            .and_then(|id| uuid::Uuid::parse_str(id).ok()),
        default_project_id: config
            .project_id
            .as_ref()
            .and_then(|id| uuid::Uuid::parse_str(id).ok()),
        ..Default::default()
    };
    let client = ContextStreamClient::new(client_config.clone());
    let session = SessionManager::new(client.clone(), client_config);
    mcp_tools::domains::hosted_tools::fire_and_forget_prepare(&client, &session, Path::new(&cwd))
        .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn gating_respects_env_switch_config_and_cached_catalog() {
        let _guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let home = tempfile::tempdir().unwrap();
        let nested = home.path().join("a/b/c/d/e/f/g/h/project");
        std::fs::create_dir_all(&nested).unwrap();
        let cwd = nested.to_str().unwrap();

        // Save and isolate env.
        let prev_home = std::env::var_os("HOME");
        let prev_switch = std::env::var_os(command_hints::ENV_SWITCH);
        let prev_key = std::env::var_os("CONTEXTSTREAM_API_KEY");
        let prev_url = std::env::var_os("CONTEXTSTREAM_API_URL");
        std::env::set_var("HOME", home.path());
        std::env::remove_var(command_hints::ENV_SWITCH);
        std::env::remove_var("CONTEXTSTREAM_API_KEY");
        std::env::set_var("CONTEXTSTREAM_API_URL", "https://api.example.invalid");

        // Unconfigured API key -> false.
        assert!(!can_prewarm_locally(""));
        assert!(!can_prewarm_locally(cwd));

        // Configured API key -> true when hints uncached.
        std::env::set_var("CONTEXTSTREAM_API_KEY", "cs_test_prewarm_key");
        assert!(can_prewarm_locally(cwd));

        // Switched off -> false.
        std::env::set_var(command_hints::ENV_SWITCH, "off");
        assert!(!can_prewarm_locally(cwd));
        std::env::remove_var(command_hints::ENV_SWITCH);

        // Fresh cached catalog without `check` -> false; with `check` -> true.
        let identity = command_hints::local_identity("cs_test_prewarm_key");
        let empty_cache =
            command_hints::Cache::new(&identity, "https://api.example.invalid", vec![]);
        command_hints::write(&empty_cache).unwrap();
        assert!(!can_prewarm_locally(cwd));

        let with_check = command_hints::Cache::new(
            &identity,
            "https://api.example.invalid",
            vec![command_hints::CachedTool {
                name: "check".into(),
                transport: "stream".into(),
                attachments: vec!["checkout_snapshot".into()],
                hints: command_hints::Hints {
                    argument: "command".into(),
                    cwd_argument: Some("cwd".into()),
                    patterns: vec!["^cargo check$".into()],
                },
            }],
        );
        command_hints::write(&with_check).unwrap();
        assert!(can_prewarm_locally(cwd));

        // maybe_spawn returns immediately (< 5ms) without blocking.
        let started = Instant::now();
        maybe_spawn(cwd);
        assert!(started.elapsed() < Duration::from_millis(5));

        // Restore env.
        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        match prev_switch {
            Some(v) => std::env::set_var(command_hints::ENV_SWITCH, v),
            None => std::env::remove_var(command_hints::ENV_SWITCH),
        }
        match prev_key {
            Some(v) => std::env::set_var("CONTEXTSTREAM_API_KEY", v),
            None => std::env::remove_var("CONTEXTSTREAM_API_KEY"),
        }
        match prev_url {
            Some(v) => std::env::set_var("CONTEXTSTREAM_API_URL", v),
            None => std::env::remove_var("CONTEXTSTREAM_API_URL"),
        }
    }
}
