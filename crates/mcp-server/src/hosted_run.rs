//! `contextstream-mcp run -- <command>`: run a command through the tool the
//! account has for it, and where there is none (or it declines, or cannot be
//! reached) run it right here, as the shell would.
//!
//! The tool is found in the hints shared with the editor hook
//! (`mcp_client::command_hints`), refreshed from the API when stale. The command
//! must be one a tool says it takes, or the tool can be named with `--tool`.
//! The call describes the checkout to the API and sends it on its stream,
//! just as a tool call does. Output is written as it arrives, with a first
//! line on stderr, and the process ends with the command's own exit status.
//! When the host declines or says a failure was its own, the command runs
//! here after a line that says why.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mcp_client::command_hints::{self, CachedTool};
use mcp_client::frames::{self, Delivery, Event};
use mcp_client::ContextStreamClient;
use mcp_session::SessionManager;
use mcp_tools::domains::hosted_tools::{
    begin_snapshot, call_api, command_hints_for_run, drive_stream, format_diagnostic_report,
    stream_call, Attachment, CallRefusal, HostedTool, Transport, ADD_CREDITS, CREDIT_MESSAGE,
};
use mcp_types::tool::ContentItem;
use serde_json::Value;

/// How long a command will wait to discover the caller's hosted tools.
const HINTS_BUDGET: Duration = Duration::from_millis(1500);

/// Maximum age of a local remote-run receipt consumed by PostToolUse / PostToolUseFailure.
const RECEIPT_MAX_AGE_MS: u64 = 30_000;

/// Local receipt written when `contextstream-mcp run` completes remotely so
/// Claude Code's `PostToolUse` / `PostToolUseFailure` hooks can surface a
/// user-visible `systemMessage` even when Bash output is collapsed or piped
/// through `grep`/`head`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RemoteRunReceipt {
    pub written_at_ms: u64,
    pub cwd: String,
    pub command: String,
    pub exit: i32,
    pub seconds: Option<f64>,
    pub ran_seconds: Option<f64>,
    pub cached: bool,
    pub line: String,
}

/// How a try at running the command on the service came out.
#[derive(Debug, PartialEq)]
enum Remote {
    /// The service ran it: its exit status, the line said after its output, and metadata for the receipt.
    Ran(i32, String, Option<RemoteRunReceipt>),
    /// It was not run there, so it is run here; the reason when a call was tried.
    Here(Option<String>),
    /// It started on the service and then the stream broke: its output was already written.
    Broken,
    OutOfCredits,
}

fn write_stderr_line(line: &str) {
    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "{line}");
    let _ = stderr.flush();
}

/// Run `command` and return the process's exit status.
pub async fn run(tool: Option<String>, command: String) -> i32 {
    run_with_verbosity(tool, command, false).await
}

/// Run the command, reporting the API's call ID when verbose.
pub async fn run_with_verbosity(tool: Option<String>, command: String, verbose: bool) -> i32 {
    let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
    match try_remote(tool.as_deref(), &command, &cwd, verbose).await {
        Remote::Ran(status, said, receipt) => {
            if let Some(mut receipt) = receipt {
                receipt.cwd = cwd.to_string_lossy().to_string();
                receipt.command = command.clone();
                write_remote_receipt(&receipt);
            }
            write_stderr_line(&said);
            status
        }
        Remote::OutOfCredits => {
            write_stderr_line(&out_of_credits_line());
            run_here(&command, &cwd)
        }
        Remote::Broken => {
            write_stderr_line("contextstream: the remote run ended without a result");
            1
        }
        Remote::Here(reason) => {
            if let Some(reason) = reason {
                write_stderr_line(&format!(
                    "contextstream: running it here instead ({reason})"
                ));
            }
            run_here(&command, &cwd)
        }
    }
}

fn out_of_credits_line() -> String {
    format!("contextstream: {CREDIT_MESSAGE} Running the command locally. {ADD_CREDITS}")
}

async fn try_remote(tool: Option<&str>, command: &str, cwd: &Path, verbose: bool) -> Remote {
    if command_hints::switched_off(std::env::var(command_hints::ENV_SWITCH).ok().as_deref()) {
        return Remote::Here(None);
    }
    let config = crate::hook_handlers::common::load_config(&cwd.to_string_lossy());
    if !config.is_configured() {
        return Remote::Here(None);
    }
    let identity = command_hints::local_identity(&config.api_key);
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
    let Some(cache) = command_hints_for_run(&client, &session, &identity, HINTS_BUDGET).await
    else {
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
        Err(CallRefusal::OutOfCredits) => return Remote::OutOfCredits,
        Err(CallRefusal::Other(result)) => {
            let reason = result.content.iter().find_map(|item| match item {
                ContentItem::Text { text } => frames::display_text(text),
                _ => None,
            });
            return Remote::Here(Some(
                reason.unwrap_or_else(|| "the call was not taken".into()),
            ));
        }
    };
    if verbose {
        if let Some(id) = ["/call/id", "/stream/request/id"]
            .into_iter()
            .find_map(|path| {
                data.pointer(path)
                    .and_then(Value::as_str)
                    .and_then(frames::display_text)
            })
        {
            write_stderr_line(&format!("contextstream: remote call {id}"));
        }
    }
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
                        write_stderr_line(mark);
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
    let (mut result, wrote) =
        tokio::join!(drive_stream(&call, &snapshot, DELIVERY, events), writer);
    if let Ok(ref mut frame) = result {
        mcp_tools::domains::hosted_tools::enrich_frame_diagnostics(
            &client,
            mcp_tools::domains::hosted_tools::call_id_of(&data),
            config.workspace_id.as_deref(),
            config.project_id.as_deref(),
            mcp_tools::domains::hosted_tools::repo_key_of(&data),
            frame,
        )
        .await;
    }
    outcome(result, wrote)
}

/// How this command takes the run's output from the service: as it prints (lever 8, ruling of 2026-11-16), so a
/// failure is seen when it happens and not when the whole check ends. Held (`Buffered`), the service kept it until it
/// had decided its answer; streamed, a failure the host later calls its own has been shown already, and the last line
/// says so (`outcome`).
const DELIVERY: Delivery = Delivery::Streamed;

/// The line said on stderr before the first byte of the run's output, when it is shown as it prints.
fn output_mark(delivery: Delivery) -> Option<&'static str> {
    (delivery == Delivery::Streamed).then_some(
        "contextstream: running remotely on MachineStream (output as it prints; if the host does not stand by it, it runs here)",
    )
}

/// Why the service did not stand by the output it already sent.
fn refusal_reason(frame: &Value) -> String {
    frames::declined_reason(frame)
        .or_else(|| {
            ["refused", "refused_code"].into_iter().find_map(|key| {
                frame
                    .get(key)
                    .and_then(Value::as_str)
                    .and_then(frames::display_text)
            })
        })
        .unwrap_or_else(|| "the service did not finish it".to_string())
}

/// What the host says when a run's failure was its own (mcd's `environment` on the result): its detail, else its code.
fn host_fault(frame: &Value) -> Option<String> {
    let cause = frame.get("environment").filter(|cause| cause.is_object())?;
    let reason = ["detail", "refused_code"].into_iter().find_map(|key| {
        cause
            .get(key)
            .and_then(Value::as_str)
            .and_then(frames::display_text)
    });
    Some(match reason {
        Some(why) => format!("the host's fault, not your change: {why}"),
        None => "the host's fault, not your change".to_string(),
    })
}

/// What the service's `result` frame (or the reason there is none) comes to, and whether output was already written.
fn outcome(result: Result<Value, String>, wrote: bool) -> Remote {
    match result {
        Ok(frame) if frame.get("ran").and_then(Value::as_bool) == Some(true) => {
            match host_fault(&frame) {
                // The output shown is not the verdict: the command runs here, after a last line that says why.
                Some(said) => Remote::Here(Some(said)),
                None => {
                    let exit = exit_status(&frame);
                    let summary_line = ran_line(&frame);
                    let receipt = RemoteRunReceipt {
                        written_at_ms: now_unix_ms(),
                        cwd: String::new(),
                        command: String::new(),
                        exit,
                        seconds: frame.get("seconds").and_then(Value::as_f64),
                        ran_seconds: frame.get("ran_seconds").and_then(Value::as_f64),
                        cached: frame.get("cache").and_then(Value::as_str) == Some("hit"),
                        line: summary_line.clone(),
                    };
                    let mut line = summary_line;
                    if exit != 0 {
                        if let Some(diag) = format_diagnostic_report(&frame, "") {
                            line = format!("{diag}\n{line}");
                        }
                    }
                    Remote::Ran(exit, line, Some(receipt))
                }
            }
        }
        // Refused after its output was shown (mcd's checks after the run): the output does not count; it runs here.
        Ok(frame) if wrote => Remote::Here(Some(format!(
            "the host did not stand by the output above: {}",
            refusal_reason(&frame)
        ))),
        Ok(frame) => Remote::Here(Some(
            frames::declined_reason(&frame)
                .map(|reason| frames::decline_explanation(&reason).to_string())
                .unwrap_or_else(|| "the service declined to run it".to_string()),
        )),
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
    let ran_seconds = frame.get("ran_seconds").and_then(Value::as_f64);
    match (seconds, cached, ran_seconds) {
        (Some(seconds), true, Some(ran_s)) => format!(
            "[ran remotely: {seconds:.1} s, verified exact-tree cache hit (previously ran in {ran_s:.1} s; identical git tree and command, no rerun needed)]"
        ),
        (Some(seconds), true, None) => format!(
            "[ran remotely: {seconds:.1} s, verified exact-tree cache hit (identical git tree and command, no rerun needed)]"
        ),
        (Some(seconds), false, _) => format!("[ran remotely: {seconds:.1} s]"),
        (None, _, _) => "[ran remotely]".to_string(),
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn receipt_dir() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".contextstream"))
}

fn receipt_path_in(dir: &Path, command: &str) -> PathBuf {
    let digest = hex::encode(Sha256::digest(command.trim().as_bytes()));
    dir.join(format!("remote-run-receipt-{}.json", &digest[..16]))
}

fn write_remote_receipt(receipt: &RemoteRunReceipt) {
    if let Some(dir) = receipt_dir() {
        write_remote_receipt_in(&dir, receipt);
    }
}

/// Persist a remote-run receipt atomically under `dir`.
pub(crate) fn write_remote_receipt_in(dir: &Path, receipt: &RemoteRunReceipt) {
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let path = receipt_path_in(dir, &receipt.command);
    if std::fs::symlink_metadata(&path).is_ok_and(|meta| !meta.is_file()) {
        return;
    }
    let Ok(bytes) = serde_json::to_vec(receipt) else {
        return;
    };
    let tmp = dir.join(format!(
        ".remote-run-receipt-{}-{}.tmp",
        std::process::id(),
        now_unix_ms()
    ));
    let Ok(mut file) = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp)
    else {
        return;
    };
    if file.write_all(&bytes).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    drop(file);
    if std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

pub(crate) fn take_remote_receipt_in(
    dir: &Path,
    command: &str,
    cwd: &str,
    now_ms: u64,
) -> Option<RemoteRunReceipt> {
    let path = receipt_path_in(dir, command);
    if !std::fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_file()) {
        return None;
    }
    let raw = std::fs::read(&path).ok()?;
    let receipt: RemoteRunReceipt = serde_json::from_slice(&raw).ok()?;
    let age = now_ms.saturating_sub(receipt.written_at_ms);
    if age > RECEIPT_MAX_AGE_MS {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    if !cwd.is_empty() && !receipt.cwd.is_empty() {
        let receipt_path = Path::new(&receipt.cwd);
        let hook_path = Path::new(cwd);
        if receipt_path != hook_path
            && !receipt_path.starts_with(hook_path)
            && !hook_path.starts_with(receipt_path)
        {
            return None;
        }
    }
    let _ = std::fs::remove_file(&path);
    Some(receipt)
}

/// Extract inner command string(s) wrapped by `... run --tool <tool> -- <quoted_cmd>`
/// in a shell command executed by Claude Code's Bash tool.
pub(crate) fn extract_run_commands(shell_command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = shell_command;
    while let Some(idx) = rest.find(" run --tool ") {
        let after_tool_flag = &rest[idx + " run --tool ".len()..];
        let Some((_, after_tool)) = take_shell_word(after_tool_flag.trim_start()) else {
            rest = after_tool_flag;
            continue;
        };
        let after_tool = after_tool.trim_start();
        let Some(after_sep) = after_tool.strip_prefix("--") else {
            rest = after_tool;
            continue;
        };
        let Some(after_sep) = after_sep
            .strip_prefix(' ')
            .or_else(|| after_sep.strip_prefix('\t'))
        else {
            rest = after_tool;
            continue;
        };
        if let Some((cmd, tail)) = take_shell_word(after_sep.trim_start()) {
            if !cmd.trim().is_empty() {
                out.push(cmd);
            }
            rest = tail;
        } else {
            break;
        }
    }
    out
}

/// Parse one POSIX shell word (supporting adjacent `'...'`, `"..."`, and backslash escapes)
/// and return `(unquoted_word, remaining_slice)`.
fn take_shell_word(input: &str) -> Option<(String, &str)> {
    let bytes = input.as_bytes();
    let len = bytes.len();
    if len == 0 {
        return None;
    }
    let mut i = 0;
    let mut word = String::new();
    while i < len {
        match bytes[i] {
            b' ' | b'\t' | b'\n' | b';' | b'|' | b'&' | b'<' | b'>' => break,
            b'\'' => {
                i += 1;
                let start = i;
                while i < len && bytes[i] != b'\'' {
                    i += 1;
                }
                if i >= len {
                    return None;
                }
                word.push_str(&input[start..i]);
                i += 1;
            }
            b'"' => {
                i += 1;
                while i < len && bytes[i] != b'"' {
                    if bytes[i] == b'\\' && i + 1 < len {
                        i += 1;
                    }
                    let ch = input[i..].chars().next()?;
                    word.push(ch);
                    i += ch.len_utf8();
                }
                if i >= len {
                    return None;
                }
                i += 1;
            }
            b'\\' => {
                i += 1;
                if i >= len {
                    return None;
                }
                let ch = input[i..].chars().next()?;
                word.push(ch);
                i += ch.len_utf8();
            }
            _ => {
                let ch = input[i..].chars().next()?;
                word.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    if i == 0 {
        None
    } else {
        Some((word, &input[i..]))
    }
}

/// Format a concise user-facing banner for Claude Code's `systemMessage` field
/// (shown outside collapsed Bash output).
pub(crate) fn format_user_notice(receipt: &RemoteRunReceipt) -> String {
    let timing = match (receipt.seconds, receipt.cached, receipt.ran_seconds) {
        (Some(s), true, Some(ran_s)) => format!(
            "ran remotely in {s:.1} s (verified exact-tree cache hit; previously ran in {ran_s:.1} s)"
        ),
        (Some(s), true, None) => format!("ran remotely in {s:.1} s (verified exact-tree cache hit)"),
        (Some(s), false, _) => format!("ran remotely in {s:.1} s"),
        (None, true, _) => "ran remotely (verified exact-tree cache hit)".to_string(),
        (None, false, _) => "ran remotely".to_string(),
    };
    if receipt.exit == 0 {
        format!("MachineStream: {timing}")
    } else {
        format!("MachineStream: {timing} (exit {})", receipt.exit)
    }
}

/// Extract a `[ran remotely...]` line directly from tool output when no local
/// receipt file is present.
pub(crate) fn extract_ran_remotely_line(text: &str) -> Option<String> {
    let start = text.rfind("[ran remotely")?;
    let tail = &text[start..];
    let end = tail.find(']')?;
    Some(tail[..=end].to_string())
}

fn extract_hook_output_text(input: &Value) -> String {
    let mut parts = Vec::new();
    for key in [
        "tool_response",
        "response",
        "output",
        "stdout",
        "stderr",
        "error",
        "tool_error",
    ] {
        match input.get(key) {
            Some(Value::String(s)) if !s.is_empty() => parts.push(s.as_str()),
            Some(Value::Object(map)) => {
                for sub in ["stdout", "stderr", "output", "text", "error"] {
                    if let Some(s) = map.get(sub).and_then(Value::as_str) {
                        if !s.is_empty() {
                            parts.push(s);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    parts.join("\n")
}

/// Inspect a finished Claude Code `Bash` hook payload for `contextstream-mcp run`
/// invocations and return `(system_message_for_user, optional_additional_context_for_agent)`.
///
/// The `system_message` is always shown outside Claude Code's collapsed Bash output
/// so the user immediately sees that MachineStream ran the check remotely.
/// `additional_context` is emitted when the command's output does not already contain
/// `[ran remotely` (for example, when `2>&1 | grep ... | head` filtered out stderr).
pub(crate) fn remote_run_hook_feedback(
    input: &Value,
    command: &str,
    cwd: &str,
) -> Option<(String, Option<String>)> {
    let dir = receipt_dir();
    remote_run_hook_feedback_in(input, command, cwd, dir.as_deref(), now_unix_ms())
}

pub(crate) fn remote_run_hook_feedback_in(
    input: &Value,
    command: &str,
    cwd: &str,
    dir: Option<&Path>,
    now_ms: u64,
) -> Option<(String, Option<String>)> {
    let inner_cmds = extract_run_commands(command);
    if inner_cmds.is_empty() {
        return None;
    }
    let output_text = extract_hook_output_text(input);
    let output_shows_remote = output_text.contains("[ran remotely");

    let mut receipts = Vec::new();
    if let Some(dir) = dir {
        for inner in &inner_cmds {
            if let Some(receipt) = take_remote_receipt_in(dir, inner, cwd, now_ms) {
                receipts.push(receipt);
            }
        }
    }

    if !receipts.is_empty() {
        let user_msg = receipts
            .iter()
            .map(format_user_notice)
            .collect::<Vec<_>>()
            .join(" · ");
        let agent_ctx = if output_shows_remote {
            None
        } else {
            let lines = receipts
                .iter()
                .map(|r| r.line.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            Some(format!(
                "MachineStream ran this check remotely ({lines}). Do not rerun locally unless you changed code."
            ))
        };
        return Some((user_msg, agent_ctx));
    }

    // Fallback when no receipt file was present: parse `[ran remotely...]` from tool output.
    let line = extract_ran_remotely_line(&output_text)?;
    let trimmed = line.trim_start_matches('[').trim_end_matches(']').trim();
    let user_msg = format!("MachineStream: {trimmed}");
    Some((user_msg, None))
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
            write_stderr_line(&missing_shell_message(local_shell));
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
    fn credit_fallback_explains_the_local_run() {
        assert_eq!(out_of_credits_line(), "contextstream: Your account is out of Context Credits for remote checks. Running the command locally. Add credits at https://contextstream.io/account/billing");
    }

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
        assert!(mark.contains("as it prints"), "{mark}");
        // the last line is not always the verdict: after a run the host does not stand by, the command runs here
        assert!(!mark.contains("the verdict is the last line"), "{mark}");
        assert!(mark.contains("if the host does not stand by it"), "{mark}");
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
                assert!(
                    said.starts_with("the host's fault, not your change"),
                    "{said}"
                );
                assert!(
                    said.contains("the host ran out of memory during the run"),
                    "{said}"
                );
            }
            other => panic!("{other:?}"),
        }
        // with no detail, its code
        let frame = json!({"ran": true, "exit": 1, "environment": {"refused_code": "host_disk"}});
        assert_eq!(
            outcome(Ok(frame), true),
            Remote::Here(Some(
                "the host's fault, not your change: host_disk".to_string()
            ))
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
        match outcome(Ok(frame), true) {
            Remote::Ran(101, line, Some(receipt)) => {
                assert_eq!(line, "[ran remotely: 4.3 s]");
                assert_eq!(receipt.exit, 101);
                assert!(!receipt.cached);
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
        assert_eq!(
            outcome(Ok(json!({"ran": false})), true),
            Remote::Here(Some(
                "the host did not stand by the output above: the service did not finish it"
                    .to_string()
            ))
        );
        assert_eq!(
            outcome(Ok(json!({"ran": false})), false),
            Remote::Here(Some("the service declined to run it".to_string()))
        );
        assert_eq!(outcome(Err("gone".to_string()), true), Remote::Broken);
        assert_eq!(
            outcome(Err("gone".to_string()), false),
            Remote::Here(Some("gone".to_string()))
        );
    }

    /// Review of ee2f156 (review-lever8-mcp): with the output streamed, mcd's refusals after a run (a tool missing on
    /// the host, a run that wrote the tree) come after the output, as `ran: false` and 75. On main such a command ran
    /// here; it still does, and the last line says the output shown does not count, with the host's own reason.
    #[test]
    fn a_run_the_host_refuses_after_its_output_runs_here_and_the_output_does_not_count() {
        let frame = json!({"type": "result", "ran": false, "exit": 75, "refused_code": "command_missing",
            "refused": "command not found on the check host (exit 127); the session runs it itself"});
        assert_eq!(
            outcome(Ok(frame.clone()), true),
            Remote::Here(Some(
                "the host did not stand by the output above: command not found on the check host (exit 127); \
                 the session runs it itself"
                    .to_string()
            ))
        );
        let frame =
            json!({"type": "result", "ran": false, "exit": 75, "refused_code": "run_wrote_tree"});
        assert_eq!(
            outcome(Ok(frame), true),
            Remote::Here(Some(
                "the host did not stand by the output above: run_wrote_tree".to_string()
            ))
        );
        // With nothing shown yet, give the reason on its own.
        assert_eq!(
            outcome(
                Ok(json!({"ran": false, "exit": 75, "refused": "busy"})),
                false
            ),
            Remote::Here(Some("busy".to_string()))
        );
    }

    #[test]
    fn the_ran_line_names_no_host() {
        let line =
            ran_line(&json!({"seconds": 4.26, "cache": "miss", "host": "a-host", "warm": true}));
        assert_eq!(line, "[ran remotely: 4.3 s]");
        assert_eq!(
            ran_line(&json!({"seconds": 0.04, "cache": "hit"})),
            "[ran remotely: 0.0 s, verified exact-tree cache hit (identical git tree and command, no rerun needed)]"
        );
        assert_eq!(
            ran_line(&json!({"seconds": 0.81, "cache": "hit", "ran_seconds": 12.93})),
            "[ran remotely: 0.8 s, verified exact-tree cache hit (previously ran in 12.9 s; identical git tree and command, no rerun needed)]"
        );
        assert_eq!(ran_line(&json!({})), "[ran remotely]");
    }

    #[test]
    fn remote_run_receipt_round_trips_extracts_wrapped_commands_and_formats_notice() {
        let tmp = tempfile::tempdir().unwrap();
        let cmd = "CARGO_BUILD_WARNINGS=deny cargo clippy --workspace --all-targets";
        let receipt = RemoteRunReceipt {
            written_at_ms: 1_700_000_000_000,
            cwd: "/work/ruff/crates/ruff".to_string(),
            command: cmd.to_string(),
            exit: 0,
            seconds: Some(0.81),
            ran_seconds: Some(12.93),
            cached: true,
            line: "[ran remotely: 0.8 s, verified exact-tree cache hit (previously ran in 12.9 s; identical git tree and command, no rerun needed)]".to_string(),
        };
        write_remote_receipt_in(tmp.path(), &receipt);

        // Unrelated directory cannot consume the receipt.
        assert!(
            take_remote_receipt_in(tmp.path(), cmd, "/other/repo", 1_700_000_001_000).is_none()
        );
        // Repo root consumes receipt written from a subdirectory (`cd crates/ruff && ...`).
        let loaded =
            take_remote_receipt_in(tmp.path(), cmd, "/work/ruff", 1_700_000_001_000).unwrap();
        assert_eq!(loaded, receipt);
        // Consumed once.
        assert!(take_remote_receipt_in(tmp.path(), cmd, "/work/ruff", 1_700_000_001_000).is_none());

        // Expired receipt is dropped.
        write_remote_receipt_in(tmp.path(), &receipt);
        assert!(take_remote_receipt_in(tmp.path(), cmd, "/work/ruff", 1_700_000_060_000).is_none());

        assert_eq!(
            format_user_notice(&receipt),
            "MachineStream: ran remotely in 0.8 s (verified exact-tree cache hit; previously ran in 12.9 s)"
        );
        let mut failed = receipt.clone();
        failed.cached = false;
        failed.exit = 101;
        failed.seconds = Some(14.4);
        assert_eq!(
            format_user_notice(&failed),
            "MachineStream: ran remotely in 14.4 s (exit 101)"
        );

        let wrapped = r#"cd crates/ruff && '/usr/local/bin/contextstream-mcp' run --tool 'check' -- 'CARGO_BUILD_WARNINGS=deny cargo clippy --workspace --all-targets' 2>&1 | grep error | head -n 40; echo "exit: ${PIPESTATUS[0]}""#;
        assert_eq!(extract_run_commands(wrapped), vec![cmd.to_string()]);

        // Piped output that stripped stderr still gets both user systemMessage AND agent additionalContext.
        write_remote_receipt_in(tmp.path(), &receipt);
        let piped_input = json!({"tool_response": {"stdout": "exit: 0\n", "stderr": ""}});
        let (user_msg, agent_ctx) = remote_run_hook_feedback_in(
            &piped_input,
            wrapped,
            "/work/ruff",
            Some(tmp.path()),
            1_700_000_001_000,
        )
        .unwrap();
        assert!(user_msg.contains("MachineStream: ran remotely in 0.8 s"));
        let ctx = agent_ctx.expect("piped output without [ran remotely] should inform the agent");
        assert!(ctx.contains("verified exact-tree cache hit"));

        // Unpiped output that already shows [ran remotely] gets systemMessage without duplicate agent context.
        write_remote_receipt_in(tmp.path(), &receipt);
        let unpiped_input =
            json!({"tool_response": {"stdout": "", "stderr": format!("{}\n", receipt.line)}});
        let (user_msg, agent_ctx) = remote_run_hook_feedback_in(
            &unpiped_input,
            wrapped,
            "/work/ruff",
            Some(tmp.path()),
            1_700_000_001_000,
        )
        .unwrap();
        assert!(user_msg.contains("MachineStream: ran remotely in 0.8 s"));
        assert!(agent_ctx.is_none());

        // Fallback when no receipt file exists parses [ran remotely...] from tool output.
        let (user_msg, agent_ctx) = remote_run_hook_feedback_in(
            &json!({"tool_response": "ok\n[ran remotely: 12.9 s]\n"}),
            wrapped,
            "/work/ruff",
            Some(tmp.path()),
            1_700_000_001_000,
        )
        .unwrap();
        assert_eq!(user_msg, "MachineStream: ran remotely: 12.9 s");
        assert!(agent_ctx.is_none());
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

    #[test]
    fn outcome_appends_compact_diagnostic_header_on_failing_run_with_diagnostics() {
        let frame = json!({
            "ran": true,
            "exit": 101,
            "seconds": 2.4,
            "failing_on_base": {
                "known": true,
                "base_exit": 101,
                "old": [{"file": "src/old.rs", "line": 3, "error_code": "E0425"}],
                "new": [{"file": "src/new.rs", "line": 12, "symbol": "check", "error_code": "E0308", "message": "type mismatch"}]
            },
            "diagnostics": {
                "new_failures_count": 1,
                "base_failures_suppressed": 1,
                "new_failures": [{"file": "src/new.rs", "line": 12, "symbol": "check", "error_code": "E0308", "message": "type mismatch"}],
                "commits": [{"sha": "deadbeef", "message": "update check()"}],
                "decisions": [],
                "lessons": []
            }
        });
        match outcome(Ok(frame), true) {
            Remote::Ran(101, said, _) => {
                assert!(
                    said.contains("[diagnostics: 1 new failure(s), 1 base failure(s) suppressed]"),
                    "{said}"
                );
                assert!(said.contains("commits: deadbeef update check()"), "{said}");
                assert!(said.contains("src/new.rs:12 [E0308] (check)"), "{said}");
                assert!(said.contains("[ran remotely: 2.4 s]"), "{said}");
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[tokio::test]
    async fn enrich_frame_diagnostics_and_outcome_fetch_diagnose_when_j_frame_has_none() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen_paths = std::sync::Arc::new(std::sync::Mutex::new(Vec::<(String, Value)>::new()));
        let seen_clone = seen_paths.clone();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let seen_clone = seen_clone.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let header_end = loop {
                        let n = socket.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break pos + 4;
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
                    let content_len = head
                        .lines()
                        .skip(1)
                        .find_map(|line| {
                            let (k, v) = line.split_once(':')?;
                            k.trim()
                                .eq_ignore_ascii_case("content-length")
                                .then(|| v.trim().parse::<usize>().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    while buf.len() < header_end + content_len {
                        let n = socket.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let path = head
                        .lines()
                        .next()
                        .and_then(|l| l.split_whitespace().nth(1))
                        .unwrap_or("")
                        .to_string();
                    let body: Value =
                        serde_json::from_slice(&buf[header_end..]).unwrap_or(Value::Null);
                    seen_clone.lock().unwrap().push((path.clone(), body));
                    let resp_body = if path == "/api/v1/mcp/hosted-tools/calls/call-run-1/diagnose"
                    {
                        json!({
                            "success": true,
                            "data": {
                                "call_id": "00000000-0000-0000-0000-000000000001",
                                "new_failures_count": 1,
                                "base_failures_suppressed": 1,
                                "new_failures": [{
                                    "file": "src/new.rs",
                                    "line": 12,
                                    "symbol": "check",
                                    "error_code": "E0308",
                                    "message": "type mismatch",
                                    "callers": ["main::dispatch"]
                                }],
                                "commits": [{"sha": "deadbeef", "message": "update check()", "matched_by": "symbol"}],
                                "decisions": [{"id": "00000000-0000-0000-0000-000000000002", "summary": "Keep check strict"}],
                                "lessons": [{"id": "00000000-0000-0000-0000-000000000003", "title": "Borrow &str in check", "prevention": "Use &str"}],
                                "elapsed_ms": 11,
                                "timed_out": false
                            }
                        })
                        .to_string()
                    } else {
                        "{}".to_string()
                    };
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{resp_body}",
                        resp_body.len()
                    );
                    let _ = socket.write_all(resp.as_bytes()).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        let client = ContextStreamClient::new(mcp_types::Config {
            api_url: url,
            api_key: Some("run-test-key".to_string()),
            ..Default::default()
        });
        let call_data = json!({
            "call": {"id": "call-run-1"},
            "stream": {"request": {"id": "call-run-1", "repo_key": "repo-1234abcd"}}
        });
        let mut frame = json!({
            "ran": true,
            "exit": 101,
            "seconds": 2.4,
            "failures": [{"file": "src/new.rs", "line": 12, "symbol": "check", "error_code": "E0308", "message": "type mismatch"}],
            "failing_on_base": {
                "known": true,
                "base_exit": 101,
                "old": [{"file": "src/old.rs", "line": 3, "error_code": "E0425"}],
                "new": [{"file": "src/new.rs", "line": 12, "symbol": "check", "error_code": "E0308", "message": "type mismatch"}]
            }
        });
        assert!(frame.get("diagnostics").is_none());

        // The production bound is 95 ms, round trip included; on a loaded host
        // that ran out before the request left (no request seen, no
        // diagnostics). What is checked here is the request and the
        // enrichment, so this waits for the stub's answer; the bound is
        // tested on a frozen clock in mcp-tools.
        mcp_tools::domains::hosted_tools::enrich_frame_diagnostics_within(
            std::time::Duration::from_secs(30),
            &client,
            mcp_tools::domains::hosted_tools::call_id_of(&call_data),
            Some("550e8400-e29b-41d4-a716-446655440000"),
            Some("660e8400-e29b-41d4-a716-446655440000"),
            mcp_tools::domains::hosted_tools::repo_key_of(&call_data),
            &mut frame,
        )
        .await;

        let seen = seen_paths.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            seen[0].0,
            "/api/v1/mcp/hosted-tools/calls/call-run-1/diagnose"
        );
        assert_eq!(seen[0].1["budget_ms"], 65);
        assert_eq!(
            seen[0].1["workspace_id"],
            "550e8400-e29b-41d4-a716-446655440000"
        );

        match outcome(Ok(frame), true) {
            Remote::Ran(101, said, _) => {
                assert!(
                    said.contains("[diagnostics: 1 new failure(s), 1 base failure(s) suppressed]"),
                    "{said}"
                );
                assert!(said.contains("commits: deadbeef update check()"), "{said}");
                assert!(said.contains("decisions: Keep check strict"), "{said}");
                assert!(
                    said.contains("lessons: Borrow &str in check: Use &str"),
                    "{said}"
                );
                assert!(said.contains("callers: main::dispatch"), "{said}");
                assert!(said.contains("[ran remotely: 2.4 s]"), "{said}");
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }
}
