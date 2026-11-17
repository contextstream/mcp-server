//! `contextstream-mcp await-answer --run-id <id>`: wait until a person
//! answers or messages a run, print one notice, and exit.
//!
//! An agent that asked a person something and ended its turn makes no tool
//! call, so no tool answer and no hook can show it what the person says. A
//! background command that exits is what brings such an agent back (Claude
//! Code hands a finished background command's output to the session). So the
//! agent starts this command in the background before it ends its turn (the
//! `progress` tool's answer to a waiting report names the exact command),
//! and this process does the waiting:
//!
//! - every [`DEFAULT_POLL_SECS`] it asks the API what is deliverable to the
//!   run (`GET /decision-requests/deliveries?workspace_id=&run_id=`);
//! - when something is, it prints one notice naming the
//!   `progress(action="get", run_id=...)` that reads it and exits 0;
//! - when nothing arrived within `--timeout-min` it says so in one line and
//!   exits [`EXIT_TIMED_OUT`].
//!
//! **It claims nothing, and prints no word a person wrote.** A delivery is
//! confirmed with its claim's token, which only the claim's answer carries,
//! and its lease is renewed only by the claiming credential. So the one that
//! claims must be the one that replies: the session's own `progress` call,
//! under the credential that reported the run (`mcp_tools::domains::
//! run_deliveries`). This process may hold another credential (the folder's
//! key, where the session is signed in another way); it only wakes.
//!
//! It is quiet while it waits. An API that fails is asked again after twice
//! the wait, up to [`MAX_BACKOFF`] times the poll; an API without the routes
//! (they answer an unknown path's 404 where the feature is off) or whose
//! database cannot hold deliveries yet (503 `deliveries_not_ready`) is asked
//! at a [`NOT_THERE_SLOWER`]th of the pace, and nothing is said about either.
//!
//! The credentials are the ones the hooks and `contextstream-mcp run` use in
//! this folder (`hook_handlers::common::load_config`), else what the server
//! itself would start with (`config::load_config`). Nothing of them is
//! printed.

use std::io::Write;
use std::time::{Duration, Instant};

use mcp_client::deliveries::{is_not_ready, is_not_there, DeliveryReader};
use mcp_client::ContextStreamClient;
use mcp_types::Error;
use uuid::Uuid;

/// Something arrived, and was printed.
pub const EXIT_ARRIVED: i32 = 0;

/// The wait could not start: no credentials, or no workspace.
pub const EXIT_NOT_STARTED: i32 = 1;

/// Nothing arrived in time: the code `timeout(1)` ends with.
pub const EXIT_TIMED_OUT: i32 = 124;

/// How long a wait lasts when `--timeout-min` is not given.
pub const DEFAULT_TIMEOUT_MIN: f64 = 240.0;

/// Seconds between two asks.
pub const DEFAULT_POLL_SECS: f64 = 20.0;

/// An API without the routes is asked this many times more rarely.
const NOT_THERE_SLOWER: u32 = 6;

/// The longest wait after failures, in polls.
const MAX_BACKOFF: u32 = 15;

/// The longest a wait may be asked to last: a week.
const MAX_TIMEOUT_MIN: f64 = 7.0 * 24.0 * 60.0;

/// Keep the inode: unlinking it at exit could let a third process lock a
/// different file while a second process holds the original lock. Only the
/// kernel lock denotes a running waiter; a crash releases it automatically.
fn reserve(workspace: Uuid, run: Uuid) -> Result<std::fs::File, &'static str> {
    const UNAVAILABLE: &str = "contextstream: could not reserve a waiter for this run.";
    let dir = dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .ok_or(UNAVAILABLE)?
        .join("contextstream/waiters");
    let mut dirs = std::fs::DirBuilder::new();
    dirs.recursive(true);
    let mut file = std::fs::OpenOptions::new();
    file.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        dirs.mode(0o700);
        file.mode(0o600);
    }
    dirs.create(&dir).map_err(|_| UNAVAILABLE)?;
    let lock = file
        .open(dir.join(format!("{workspace}-{run}.lock")))
        .map_err(|_| UNAVAILABLE)?;
    match lock.try_lock() {
        Ok(()) => Ok(lock),
        Err(std::fs::TryLockError::WouldBlock) => {
            Err("contextstream: already waiting for this run.")
        }
        Err(std::fs::TryLockError::Error(_)) => Err(UNAVAILABLE),
    }
}

/// What to wait for, and for how long.
#[derive(Debug, Clone, Copy)]
pub struct Wait {
    pub run_id: Uuid,
    /// The run's workspace; the folder's when not given.
    pub workspace_id: Option<Uuid>,
    pub timeout_min: f64,
    pub poll_secs: f64,
}

/// A line on stdout. A reader that went away is not this command's failure.
fn say(text: &str) {
    let mut stdout = std::io::stdout();
    let _ = writeln!(stdout, "{text}");
    let _ = stdout.flush();
}

/// The client this folder's session talks to the API with, and the workspace
/// the folder is linked to.
fn client_here(cwd: &str) -> Option<(mcp_types::Config, Option<Uuid>)> {
    let found = crate::hook_handlers::common::load_config(cwd);
    if found.is_configured() {
        let workspace_id = found
            .workspace_id
            .as_deref()
            .and_then(|id| Uuid::parse_str(id.trim()).ok());
        let config = mcp_types::Config {
            api_url: found.api_url,
            api_key: Some(found.api_key),
            default_workspace_id: workspace_id,
            ..Default::default()
        };
        return Some((config, workspace_id));
    }
    // No key in reach of this folder: what the server itself would start
    // with (a sign-in token in the environment, saved credentials).
    let config = crate::config::load_config().ok()?;
    (config.api_key.is_some() || config.jwt.is_some()).then(|| {
        let workspace_id = config.default_workspace_id;
        (config, workspace_id)
    })
}

/// `value` when it is a number above zero, else `otherwise`; never above `most`.
fn within(value: f64, otherwise: f64, most: f64) -> f64 {
    if value.is_finite() && value > 0.0 {
        value.min(most)
    } else {
        otherwise
    }
}

/// Wait for a delivery of the run and return the process's exit status.
pub async fn run(wait: Wait) -> i32 {
    let cwd = std::env::current_dir()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| ".".to_string());
    let Some((mut config, linked_workspace)) = client_here(&cwd) else {
        say("contextstream: no ContextStream credentials were found here, so there is nothing to wait on. Run `contextstream-mcp setup` first.");
        return EXIT_NOT_STARTED;
    };
    let Some(workspace_id) = wait.workspace_id.or(linked_workspace) else {
        say("contextstream: this folder is linked to no workspace. Pass --workspace-id with the run's workspace.");
        return EXIT_NOT_STARTED;
    };
    config.default_workspace_id = Some(workspace_id);
    let client = ContextStreamClient::new(config);
    // The one rule of every delivery operation: only a person's full API key
    // receives. Under anything else the wait could only end with nothing.
    if !client.receives_deliveries().await {
        say("contextstream: only a person's full API key receives what a person sends a run, and the credentials found here are not one, so there is nothing to wait on.");
        return EXIT_NOT_STARTED;
    }

    let _reservation = match reserve(workspace_id, wait.run_id) {
        Ok(lock) => lock,
        Err(message) => {
            say(message);
            return EXIT_NOT_STARTED;
        }
    };
    let minutes = within(wait.timeout_min, DEFAULT_TIMEOUT_MIN, MAX_TIMEOUT_MIN);
    let poll = Duration::from_secs_f64(within(wait.poll_secs, DEFAULT_POLL_SECS, 3600.0).max(1.0));
    let deadline = Instant::now() + Duration::from_secs_f64(minutes * 60.0);
    let reader = DeliveryReader {
        workspace_id,
        run_id: Some(wait.run_id),
    };
    let mut failures = 0_u32;
    loop {
        if Instant::now() >= deadline {
            break;
        }
        let Ok(response) =
            tokio::time::timeout_at(deadline.into(), client.read_deliveries(&reader)).await
        else {
            break;
        };
        let next = match response {
            Ok(deliveries) if !deliveries.is_empty() => {
                say(&format!("[CONTEXTSTREAM LIVE NOTICE] Something from a person waits for this run. Read it with progress(action=\"get\", run_id=\"{}\").", wait.run_id));
                return EXIT_ARRIVED;
            }
            Ok(_) => {
                failures = 0;
                poll
            }
            // The feature is off for this workspace, or on this server, or its
            // database cannot hold deliveries yet: that may change while the
            // agent waits, so keep asking, rarely.
            Err(error) if is_not_there(&error) || is_not_ready(&error) => {
                failures = 0;
                poll.saturating_mul(NOT_THERE_SLOWER)
            }
            Err(Error::RateLimited { retry_after, .. }) => {
                poll.max(Duration::from_secs(retry_after.unwrap_or(60)))
            }
            Err(_) => {
                failures = (failures + 1).min(8);
                poll.saturating_mul(2_u32.pow(failures))
                    .min(poll.saturating_mul(MAX_BACKOFF))
            }
        };
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        tokio::time::sleep(next.min(deadline - now)).await;
    }
    say(&format!(
        "contextstream: nothing arrived for run {} within {minutes} minutes. Run the same command again to keep waiting.",
        wait.run_id
    ));
    EXIT_TIMED_OUT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_time_that_is_no_time_is_the_default_and_none_is_endless() {
        assert_eq!(within(0.02, 240.0, MAX_TIMEOUT_MIN), 0.02);
        assert_eq!(within(240.0, 240.0, MAX_TIMEOUT_MIN), 240.0);
        for nothing in [0.0, -3.0, f64::NAN, f64::NEG_INFINITY] {
            assert_eq!(within(nothing, 240.0, MAX_TIMEOUT_MIN), 240.0);
        }
        assert_eq!(within(f64::INFINITY, 240.0, MAX_TIMEOUT_MIN), 240.0);
        assert_eq!(within(1e12, 240.0, MAX_TIMEOUT_MIN), MAX_TIMEOUT_MIN);
        // The one line a wait ends with says the minutes as they were given.
        assert_eq!(format!("{}", 240.0_f64), "240");
        assert_eq!(format!("{}", 0.02_f64), "0.02");
    }

    #[test]
    fn the_exit_codes_are_three_different_ones() {
        assert_eq!(EXIT_ARRIVED, 0);
        assert_ne!(EXIT_TIMED_OUT, EXIT_NOT_STARTED);
        // Not clap's code for a bad argument either.
        assert_ne!(EXIT_TIMED_OUT, 2);
        assert_ne!(EXIT_NOT_STARTED, 2);
    }
}
