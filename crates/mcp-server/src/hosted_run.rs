//! `contextstream-mcp run -- <command>`: run a command through the tool the
//! account has for it, and where there is none (or it declines, or cannot be
//! reached) run it right here, as the shell would.
//!
//! The tool is found in the hints the server keeps for the editor hook
//! (`mcp_client::command_hints`): the command must be one a tool says it takes
//! (or the tool is named with `--tool`). The call is the same a tool call
//! makes (the checkout is described to the API and sent on its stream), except
//! that the output is written as it arrives and the process ends with the
//! command's own exit status. Nothing here reaches the network when no tool
//! takes the command.

use std::io::Write;
use std::path::Path;

use mcp_client::command_hints::{self, CachedTool};
use mcp_client::frames::Event;
use mcp_client::ContextStreamClient;
use mcp_session::SessionManager;
use mcp_tools::domains::hosted_tools::{
    call_api, drive_stream, stream_call, take_snapshot, Attachment, HostedTool, Transport,
};
use serde_json::Value;

/// How a try at running the command on the service came out.
enum Remote {
    /// The service ran it: its exit status.
    Ran(i32),
    /// It was not run there, so it is run here; the reason when a call was tried.
    Here(Option<String>),
    /// It started on the service and then the stream broke: its output was already written.
    Broken,
}

/// Run `command` and return the process's exit status.
pub async fn run(tool: Option<String>, command: String) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    match try_remote(tool.as_deref(), &command, &cwd).await {
        Remote::Ran(status) => status,
        Remote::Broken => {
            eprintln!("contextstream: the remote run ended without a result");
            1
        }
        Remote::Here(reason) => {
            if let Some(reason) = reason {
                eprintln!("contextstream: running it here instead ({reason})");
            }
            run_here(&command, &cwd)
        }
    }
}

async fn try_remote(tool: Option<&str>, command: &str, cwd: &Path) -> Remote {
    if command_hints::switched_off(std::env::var(command_hints::ENV_SWITCH).ok().as_deref()) {
        return Remote::Here(None);
    }
    let config = crate::hook_handlers::common::load_config(&cwd.to_string_lossy());
    if !config.is_configured() {
        return Remote::Here(None);
    }
    let identity = command_hints::local_identity(&config.api_key);
    let Some(cache) = command_hints::read(&identity, &config.api_url) else {
        return Remote::Here(None);
    };
    let cached = match tool {
        Some(name) => cache.tool_named(name),
        None => cache.tool_for(command),
    };
    let Some(cached) = cached else {
        return Remote::Here(None);
    };
    let Some(tool) = hosted_tool(cached) else {
        return Remote::Here(None);
    };

    let client_config = mcp_types::Config {
        api_url: config.api_url.clone(),
        api_key: Some(config.api_key.clone()),
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

    let snapshot = match take_snapshot(&session, &tool).await {
        Ok(Some(snapshot)) => snapshot,
        Ok(None) => return Remote::Here(None),
        Err(_) => return Remote::Here(Some("this checkout could not be described".to_string())),
    };
    let mut arguments = serde_json::Map::new();
    arguments.insert(
        cached.hints.argument.clone(),
        Value::String(command.to_string()),
    );
    if let Some(name) = cached
        .hints
        .cwd_argument
        .as_ref()
        .filter(|_| !snapshot.folder.is_empty())
    {
        arguments.insert(name.clone(), Value::String(snapshot.folder.clone()));
    }
    let data = match call_api(
        &client,
        &session,
        &tool,
        &Value::Object(arguments),
        Some(&snapshot),
    )
    .await
    {
        Ok(data) => data,
        Err(_) => return Remote::Here(Some("the call was not taken".to_string())),
    };
    let call = match stream_call(&data) {
        Ok(call) => call,
        Err(message) => return Remote::Here(Some(message)),
    };

    let (events, mut inbox) = tokio::sync::mpsc::unbounded_channel();
    let writer = async {
        let mut wrote = false;
        let stdout = std::io::stdout();
        while let Some(event) = inbox.recv().await {
            if let Event::Output(bytes) = event {
                let mut out = stdout.lock();
                // A reader that went away is no reason to stop the run.
                let _ = out.write_all(&bytes).and_then(|_| out.flush());
                wrote = true;
            }
        }
        wrote
    };
    let (outcome, wrote) = tokio::join!(drive_stream(&call, &snapshot, false, events), writer);
    match outcome {
        Ok(frame) if frame.get("ran").and_then(Value::as_bool) == Some(true) => {
            eprintln!("{}", ran_line(&frame));
            Remote::Ran(exit_status(&frame))
        }
        Ok(_) if wrote => Remote::Broken,
        Ok(_) => Remote::Here(Some("the service declined to run it".to_string())),
        Err(_) if wrote => Remote::Broken,
        Err(reason) => Remote::Here(Some(reason)),
    }
}

/// The tool a cached entry describes, when this command can use it (a stream).
fn hosted_tool(cached: &CachedTool) -> Option<HostedTool> {
    if cached.transport != "stream" {
        return None;
    }
    Some(HostedTool {
        name: cached.name.clone(),
        title: None,
        description: String::new(),
        input_schema: serde_json::json!({"type": "object"}),
        annotations: None,
        transport: Transport::Stream,
        attachments: cached
            .attachments
            .iter()
            .filter(|name| name.as_str() == "checkout_snapshot")
            .map(|_| Attachment::CheckoutSnapshot)
            .collect(),
        command_hints: Some(cached.hints.clone()),
    })
}

/// The command's own exit status, from the service's `result` frame.
fn exit_status(frame: &Value) -> i32 {
    match frame.get("exit").and_then(Value::as_i64) {
        Some(status) if (0..=255).contains(&status) => status as i32,
        Some(_) => 1,
        None => 0,
    }
}

/// One line, after the output, that says where it ran and how long it took.
fn ran_line(frame: &Value) -> String {
    let seconds = frame.get("seconds").and_then(Value::as_f64);
    let cached = frame.get("cache").and_then(Value::as_str) == Some("hit");
    match (seconds, cached) {
        (Some(seconds), true) => format!("[ran remotely: {seconds:.1} s, answered from a cache]"),
        (Some(seconds), false) => format!("[ran remotely: {seconds:.1} s]"),
        (None, _) => "[ran remotely]".to_string(),
    }
}

/// Run the command here, in `cwd`, as a shell would: its own streams, its own exit status.
pub fn run_here(command: &str, cwd: &Path) -> i32 {
    let mut shell = if cfg!(windows) {
        let mut shell = std::process::Command::new("cmd");
        shell.arg("/C").arg(command);
        shell
    } else {
        let mut shell = std::process::Command::new("sh");
        shell.arg("-c").arg(command);
        shell
    };
    match shell.current_dir(cwd).status() {
        Ok(status) => status_code(status),
        Err(error) => {
            eprintln!("contextstream: could not run it here: {error}");
            127
        }
    }
}

#[cfg(unix)]
fn status_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0))
}

#[cfg(not(unix))]
fn status_code(status: std::process::ExitStatus) -> i32 {
    status.code().unwrap_or(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_commands_own_status_is_the_services() {
        assert_eq!(exit_status(&json!({"exit": 0})), 0);
        assert_eq!(exit_status(&json!({"exit": 101})), 101);
        assert_eq!(exit_status(&json!({"exit": 255})), 255);
        assert_eq!(exit_status(&json!({"exit": 256})), 1);
        assert_eq!(exit_status(&json!({"exit": -1})), 1);
        assert_eq!(exit_status(&json!({})), 0);
    }

    #[test]
    fn the_ran_line_names_no_host() {
        let line =
            ran_line(&json!({"seconds": 4.26, "cache": "miss", "host": "a-host", "warm": true}));
        assert_eq!(line, "[ran remotely: 4.3 s]");
        assert_eq!(
            ran_line(&json!({"seconds": 0.04, "cache": "hit"})),
            "[ran remotely: 0.0 s, answered from a cache]"
        );
        assert_eq!(ran_line(&json!({})), "[ran remotely]");
    }

    #[cfg(unix)]
    #[test]
    fn here_a_command_runs_in_its_folder_with_its_own_status() {
        let folder = tempfile::tempdir().unwrap();
        std::fs::write(folder.path().join("marker"), "x").unwrap();
        assert_eq!(run_here("test -f marker", folder.path()), 0);
        assert_eq!(run_here("test -f nothing", folder.path()), 1);
        assert_eq!(run_here("exit 7", folder.path()), 7);
        assert_eq!(run_here("kill -9 $$", folder.path()), 128 + 9);
        // A word the shell does not know is the shell's 127.
        assert_eq!(run_here("no-such-command-here-x", folder.path()), 127);
    }

    #[test]
    fn a_cached_tool_that_is_not_a_stream_is_not_run_here() {
        let hints = command_hints::Hints {
            argument: "command".to_string(),
            cwd_argument: None,
            patterns: vec!["^x$".to_string()],
        };
        let mut cached = CachedTool {
            name: "check".to_string(),
            transport: "api".to_string(),
            attachments: vec!["checkout_snapshot".to_string(), "other".to_string()],
            hints,
        };
        assert!(hosted_tool(&cached).is_none());
        cached.transport = "stream".to_string();
        let tool = hosted_tool(&cached).unwrap();
        assert_eq!(tool.attachments, vec![Attachment::CheckoutSnapshot]);
        assert_eq!(tool.transport, Transport::Stream);
    }
}
