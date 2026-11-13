//! `contextstream-mcp hook mcp-headers`: the header that names one connection.
//!
//! Claude Code speaks the stateless MCP contract, so the hosted gateway
//! cannot tell two of a person's sessions apart: in live progress every
//! session of one API key was one agent. Claude Code runs a command when it
//! connects to a remote server and adds the headers that command prints
//! (`headersHelper`); setup writes this command there. It prints one header,
//! [`mcp_client::client_session::HEADER`], with a random id: nothing of the
//! machine, the person, the folder or the key is in it.
//!
//! Claude Code runs the command again whenever it connects again. It names
//! the session's own process to the command by its messaging socket
//! (`CLAUDE_CODE_MESSAGING_SOCKET`), so the id made for that socket is kept
//! in `~/.contextstream/mcp-connections/` and a reconnect of the same
//! session sends the same id. Where the socket is not named or cannot be
//! read (Windows), every connect gets a new id.
//!
//! The command never keeps a session waiting: it reads no network and waits
//! on nothing, and it ends itself, silent, two seconds after it started.
//! Claude Code then connects with the headers the config holds, as it does
//! when the command is missing or fails, and the session is named by its API
//! key as before.

use mcp_client::client_session;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use super::durable_capture::{sha256_hex, state_dir};

/// The hook's name on the command line.
pub(crate) const HOOK: &str = "mcp-headers";

/// How long the command may take before it ends itself without a word.
const DEADLINE: Duration = Duration::from_secs(2);

/// Turns live progress off altogether; no id is sent then.
const ENABLED_ENV: &str = "CONTEXTSTREAM_RUN_PROGRESS";

/// For tests and proofs only: hold the command back this many milliseconds
/// before it prints, to show that it ends itself at its deadline.
const DELAY_ENV: &str = "CONTEXTSTREAM_MCP_HEADERS_DELAY_MS";

/// How Claude Code names the session's process to the command.
const SOCKET_ENV: &str = "CLAUDE_CODE_MESSAGING_SOCKET";

/// Ids of sessions that connected longest ago are dropped after a week.
const STALE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// Print the headers of this connection as one JSON object.
pub fn handle() -> anyhow::Result<()> {
    std::thread::spawn(|| {
        std::thread::sleep(DEADLINE);
        std::process::exit(0);
    });
    if let Some(delay) = std::env::var(DELAY_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
    {
        std::thread::sleep(Duration::from_millis(delay.min(60_000)));
    }
    let body = headers(std::env::var(ENABLED_ENV).ok().as_deref(), connection_token);
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{body}");
    let _ = out.flush();
    Ok(())
}

/// What the command prints: the one header, or no header at all where live
/// progress is turned off.
fn headers(enabled: Option<&str>, token: impl FnOnce() -> String) -> serde_json::Value {
    let off = enabled.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off" | "disabled"
        )
    });
    if off {
        return serde_json::json!({});
    }
    serde_json::json!({ client_session::HEADER: token() })
}

/// The id of this connection: the one kept for the session's process when
/// Claude Code names it, else a new one.
fn connection_token() -> String {
    kept_token().unwrap_or_else(client_session::new_token)
}

fn kept_token() -> Option<String> {
    let socket = std::env::var_os(SOCKET_ENV).filter(|value| !value.is_empty())?;
    let anchor = process_anchor(Path::new(&socket))?;
    token_for(&state_dir()?.join("mcp-connections"), &anchor)
}

/// What tells one session process from every other, also from a later one
/// that was given the same process id: the socket's path and the file it is.
#[cfg(unix)]
fn process_anchor(socket: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(socket).ok()?;
    Some(format!(
        "{}\n{}\n{}\n{}\n{}",
        socket.display(),
        meta.dev(),
        meta.ino(),
        meta.ctime(),
        meta.ctime_nsec()
    ))
}

#[cfg(not(unix))]
fn process_anchor(_socket: &Path) -> Option<String> {
    None
}

/// The random id kept under `anchor`, made now when there is none. The
/// anchor only picks the file: the id is random and nothing of the anchor
/// is in it.
fn token_for(dir: &Path, anchor: &str) -> Option<String> {
    let path = dir.join(&sha256_hex(anchor.as_bytes())[..32]);
    let kept = || {
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| client_session::accepted(text.trim()).map(str::to_string))
    };
    if let Some(token) = kept() {
        // Still in use: keep it out of the weekly sweep.
        if let Ok(file) = std::fs::File::options().write(true).open(&path) {
            let _ = file.set_modified(std::time::SystemTime::now());
        }
        return Some(token);
    }
    std::fs::create_dir_all(dir).ok()?;
    prune(dir);
    let token = client_session::new_token();
    let mut options = std::fs::File::options();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(&path) {
        Ok(mut file) => {
            file.write_all(token.as_bytes()).ok()?;
            Some(token)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            // Another connect of the same session is writing it, or the file
            // holds something else: read it once more, then replace it.
            std::thread::sleep(Duration::from_millis(20));
            kept().or_else(|| {
                std::fs::write(&path, token.as_bytes()).ok()?;
                Some(token)
            })
        }
        Err(_) => None,
    }
}

/// Drop the ids nothing has used for a week.
fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > STALE);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_command_prints_one_header_with_a_token_the_gateway_accepts() {
        let body = headers(None, client_session::new_token);
        let fields = body.as_object().expect("an object");
        assert_eq!(fields.len(), 1, "{body}");
        let token = fields[client_session::HEADER].as_str().expect("a string");
        assert_eq!(client_session::accepted(token), Some(token));
        for on in ["on", "1", "true", "", "auto"] {
            assert_eq!(
                headers(Some(on), || "t".repeat(32))
                    .as_object()
                    .map(|o| o.len()),
                Some(1),
                "{on:?}"
            );
        }
    }

    #[test]
    fn no_header_is_sent_where_live_progress_is_turned_off() {
        for off in ["off", "OFF", " 0 ", "false", "no", "disabled"] {
            let body = headers(Some(off), || panic!("no token is made when it is off"));
            assert_eq!(body, serde_json::json!({}), "{off:?}");
        }
    }

    #[test]
    fn one_session_process_keeps_its_id_and_another_gets_its_own() {
        let dir = tempfile::tempdir().expect("a folder");
        let store = dir.path().join("mcp-connections");
        let first = token_for(&store, "socket-a\n1\n2\n3\n4").expect("an id");
        assert_eq!(client_session::accepted(&first), Some(first.as_str()));
        // A reconnect of the same session reads the same id.
        assert_eq!(
            token_for(&store, "socket-a\n1\n2\n3\n4").as_deref(),
            Some(first.as_str())
        );
        // Another process, or a later one with the same path, has its own.
        let other = token_for(&store, "socket-a\n1\n2\n3\n5").expect("an id");
        assert_ne!(first, other);
        // Nothing of the anchor is in the id or in the file's name.
        let names: Vec<String> = std::fs::read_dir(&store)
            .expect("the store")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 2, "{names:?}");
        assert!(
            names.iter().all(|name| !name.contains("socket")),
            "{names:?}"
        );
        assert!(!first.contains("socket"));
    }

    #[test]
    fn a_kept_file_that_holds_no_token_is_replaced() {
        let dir = tempfile::tempdir().expect("a folder");
        let store = dir.path().join("mcp-connections");
        let made = token_for(&store, "anchor").expect("an id");
        let path = std::fs::read_dir(&store)
            .expect("the store")
            .flatten()
            .next()
            .expect("one file")
            .path();
        std::fs::write(&path, "not a token").expect("write");
        let again = token_for(&store, "anchor").expect("an id");
        assert_ne!(again, made);
        assert_eq!(token_for(&store, "anchor").as_deref(), Some(again.as_str()));
    }

    #[cfg(unix)]
    #[test]
    fn the_session_process_is_told_apart_by_the_file_its_socket_is() {
        let dir = tempfile::tempdir().expect("a folder");
        let socket = dir.path().join("1234.sock");
        assert_eq!(process_anchor(&socket), None, "no socket, no anchor");
        std::fs::write(&socket, b"").expect("a file standing in for the socket");
        let first = process_anchor(&socket).expect("an anchor");
        assert_eq!(process_anchor(&socket).as_deref(), Some(first.as_str()));
        // A later process given the same path makes a new file. A second
        // link keeps the first file's inode in use, so the new file cannot
        // be given it back (a file system may reuse a freed inode at once,
        // and its change time may not move within one clock tick).
        std::fs::hard_link(&socket, dir.path().join("keeps-the-inode-busy")).expect("link");
        std::fs::remove_file(&socket).expect("remove");
        std::fs::write(&socket, b"").expect("a new file");
        assert_ne!(process_anchor(&socket).expect("an anchor"), first);
    }
}
