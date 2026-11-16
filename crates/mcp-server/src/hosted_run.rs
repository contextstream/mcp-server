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
use mcp_client::frames::{Delivery, Event};
use mcp_client::ContextStreamClient;
use mcp_session::SessionManager;
use mcp_tools::domains::hosted_tools::{
    begin_snapshot, call_api, drive_stream, stream_call, Attachment, HostedTool, Transport,
};
use serde_json::Value;

/// How a try at running the command on the service came out.
#[derive(Debug, PartialEq)]
enum Remote {
    /// The service ran it: its exit status, and the line said after its output.
    Ran(i32, String),
    /// It was not run there, so it is run here; the reason when a call was tried.
    Here(Option<String>),
    /// It started on the service and then the stream broke: its output was already written.
    Broken,
}

/// Run `command` and return the process's exit status.
pub async fn run(tool: Option<String>, command: String) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    match try_remote(tool.as_deref(), &command, &cwd).await {
        Remote::Ran(status, said) => {
            eprintln!("{said}");
            status
        }
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

    // The call is made while the rest of the snapshot is taken: it needs only who the repository is.
    let pending = match begin_snapshot(&session, &tool).await {
        Ok(Some(pending)) => pending,
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
        .filter(|_| !pending.folder.is_empty())
    {
        arguments.insert(name.clone(), Value::String(pending.folder.clone()));
    }
    let data = match call_api(
        &client,
        &session,
        &tool,
        &Value::Object(arguments),
        Some(pending.identity.clone()),
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
    let snapshot = match pending.finish(&tool.name).await {
        Ok(snapshot) => snapshot,
        Err(_) => return Remote::Here(Some("this checkout could not be described".to_string())),
    };

    let (events, mut inbox) = tokio::sync::mpsc::unbounded_channel();
    let writer = async {
        let mut wrote = false;
        let stdout = std::io::stdout();
        while let Some(event) = inbox.recv().await {
            if let Event::Output(bytes) = event {
                if !wrote {
                    if let Some(mark) = output_mark(DELIVERY) {
                        eprintln!("{mark}");
                    }
                }
                let mut out = stdout.lock();
                // A reader that went away is no reason to stop the run.
                let _ = out.write_all(&bytes).and_then(|_| out.flush());
                wrote = true;
            }
        }
        wrote
    };
    let (result, wrote) = tokio::join!(drive_stream(&call, &snapshot, DELIVERY, events), writer);
    outcome(result, wrote)
}

/// How this command takes the run's output from the service.
const DELIVERY: Delivery = Delivery::Buffered;

/// The line said on stderr before the first byte of the run's output, when it is shown as it prints.
fn output_mark(_delivery: Delivery) -> Option<&'static str> {
    None
}

/// What the service's `result` frame (or the reason there is none) comes to, and whether output was already written.
fn outcome(result: Result<Value, String>, wrote: bool) -> Remote {
    match result {
        Ok(frame) if frame.get("ran").and_then(Value::as_bool) == Some(true) => {
            Remote::Ran(exit_status(&frame), ran_line(&frame))
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

/// The shell a command runs with here, outside Windows: bash, the shell the
/// host, ContextCode's own fallback and mc-pilot run it with. With sh (dash on
/// most Linux) a command written for bash failed here and passed everywhere
/// else.
const LOCAL_SHELL: &str = "bash";

/// Run the command here, in `cwd`, as a shell would: its own streams, its own exit status.
pub fn run_here(command: &str, cwd: &Path) -> i32 {
    run_here_with(LOCAL_SHELL, command, cwd)
}

fn run_here_with(local_shell: &str, command: &str, cwd: &Path) -> i32 {
    let mut shell = if cfg!(windows) {
        let mut shell = std::process::Command::new("cmd");
        shell.arg("/C").arg(command);
        shell
    } else {
        let mut shell = std::process::Command::new(local_shell);
        shell.arg("-c").arg(command);
        shell
    };
    match shell.current_dir(cwd).status() {
        Ok(status) => status_code(status),
        // The folder is there, so it is the shell that is missing: say so and
        // run nothing. sh in its place would read some commands differently.
        Err(error)
            if !cfg!(windows) && error.kind() == std::io::ErrorKind::NotFound && cwd.is_dir() =>
        {
            eprintln!("{}", missing_shell_message(local_shell));
            127
        }
        Err(error) => {
            eprintln!("contextstream: could not run it here: {error}");
            127
        }
    }
}

/// What is said when the shell a command runs with here is not installed.
fn missing_shell_message(local_shell: &str) -> String {
    format!(
        "contextstream: {local_shell} is not installed here, so the command was not run. \
         It runs with {local_shell} wherever else it runs; install {local_shell} \
         (sh would read some commands differently)."
    )
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

    /// Lever 8 (ruling of 2026-11-16): the output is shown as it prints, so a failure is seen when it happens
    /// (1.6 to 2.1 s through the gateway on a check that fails at once, against 52 s when the output was held).
    #[test]
    fn the_output_comes_as_it_prints_under_a_mark_that_says_so() {
        assert_eq!(DELIVERY, Delivery::Streamed);
        let mark = output_mark(DELIVERY).expect("a streamed run's output is marked");
        assert!(mark.starts_with("contextstream: "), "{mark}");
        assert!(mark.contains("output so far"), "{mark}");
        assert!(mark.contains("the verdict is the last line"), "{mark}");
        assert_eq!(output_mark(Delivery::Buffered), None);
    }

    /// When the host says a failure was its own (mcd's `environment` on a streamed result), the last line says so and
    /// the command runs here: the output already shown is not the verdict.
    #[test]
    fn a_failure_the_host_calls_its_own_is_said_to_be_the_hosts_and_runs_here() {
        let frame = json!({"ran": true, "exit": 1, "seconds": 3.2, "environment": {
            "refused_code": "host_oom", "detail": "the host ran out of memory during the run", "stage": "after the run"}});
        match outcome(Ok(frame), true) {
            Remote::Here(Some(said)) => {
                assert!(said.starts_with("the host's fault, not your change"), "{said}");
                assert!(said.contains("the host ran out of memory during the run"), "{said}");
            }
            other => panic!("{other:?}"),
        }
        // with no detail, its code
        let frame = json!({"ran": true, "exit": 1, "environment": {"refused_code": "host_disk"}});
        assert_eq!(
            outcome(Ok(frame), true),
            Remote::Here(Some("the host's fault, not your change: host_disk".to_string()))
        );
        // and with nothing in it, the words alone
        let frame = json!({"ran": true, "exit": 1, "environment": {}});
        assert_eq!(
            outcome(Ok(frame), false),
            Remote::Here(Some("the host's fault, not your change".to_string()))
        );
    }

    #[test]
    fn a_run_the_host_stands_by_is_its_exit_and_the_ran_line() {
        let frame = json!({"ran": true, "exit": 101, "seconds": 4.26, "environment": null});
        assert_eq!(outcome(Ok(frame), true), Remote::Ran(101, "[ran remotely: 4.3 s]".to_string()));
        assert_eq!(outcome(Ok(json!({"ran": false})), true), Remote::Broken);
        assert_eq!(
            outcome(Ok(json!({"ran": false})), false),
            Remote::Here(Some("the service declined to run it".to_string()))
        );
        assert_eq!(outcome(Err("gone".to_string()), true), Remote::Broken);
        assert_eq!(outcome(Err("gone".to_string()), false), Remote::Here(Some("gone".to_string())));
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

    /// A command the host did not take ran here with sh (dash), and a bash-only
    /// command failed here and passed everywhere else (ticket d177b042). It
    /// runs with bash; without bash it is said, and nothing runs.
    #[cfg(unix)]
    #[test]
    fn a_bash_only_command_runs_here_as_everywhere_else_and_no_bash_is_said() {
        let folder = tempfile::tempdir().unwrap();
        // Arrays, [[ ]] and $'..' are bash's; dash refuses each of them.
        assert_eq!(
            run_here(
                "a=(x y); [[ ${#a[@]} -eq 2 && $'a\\tb' == *$'\\t'* ]]",
                folder.path()
            ),
            0
        );
        // A missing tool reads as bash says it, as on the host.
        assert_eq!(run_here("no-such-command-here-x", folder.path()), 127);

        // No bash: the command is not run at all, by sh or anything else.
        assert_eq!(
            run_here_with("no-such-shell-here-x", "touch ran", folder.path()),
            127
        );
        assert!(!folder.path().join("ran").exists());
        let said = missing_shell_message("bash");
        assert!(said.contains("bash is not installed here, so the command was not run"));
        assert!(said.contains("install bash"));
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
