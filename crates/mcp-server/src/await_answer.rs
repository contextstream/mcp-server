//! `contextstream-mcp await-answer --run-id <id>`: wait until a person
//! answers or messages a run, print it, and exit.
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
//! - when something is, it claims it, prints it inside the frame the
//!   `progress` tool uses (with how to confirm it), and exits 0. Confirming
//!   stays the agent's: `progress(action="reply", ...)`;
//! - when nothing arrived within `--timeout-min` it says so in one line and
//!   exits [`EXIT_TIMED_OUT`].
//!
//! It is quiet while it waits. An API that fails is asked again after twice
//! the wait, up to [`MAX_BACKOFF`] times the poll; an API without the routes
//! (they answer an unknown path's 404 where the feature is off) is asked at a
//! [`NOT_THERE_SLOWER`]th of the pace, and nothing is said about either.
//!
//! The credentials are the ones the hooks and `contextstream-mcp run` use in
//! this folder (`hook_handlers::common::load_config`), else what the server
//! itself would start with (`config::load_config`). Nothing of them is
//! printed.

use std::io::Write;
use std::time::{Duration, Instant};

use mcp_client::deliveries::{is_not_there, DeliveryActor, DeliveryReader};
use mcp_client::ContextStreamClient;
use mcp_tools::domains::run_deliveries::{framed, TOOL};
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

/// Most deliveries one wait prints; the rest are offered to the next read.
const MAX_PRINTED: usize = 5;

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

    let minutes = within(wait.timeout_min, DEFAULT_TIMEOUT_MIN, MAX_TIMEOUT_MIN);
    let poll = Duration::from_secs_f64(within(wait.poll_secs, DEFAULT_POLL_SECS, 3600.0).max(0.05));
    let deadline = Instant::now() + Duration::from_secs_f64(minutes * 60.0);
    let reader = DeliveryReader {
        workspace_id,
        run_id: Some(wait.run_id),
    };
    let who = DeliveryActor {
        run_id: Some(wait.run_id),
        agent: None,
    };
    let mut failures = 0_u32;
    loop {
        let next = match client.read_deliveries(&reader).await {
            Ok(deliveries) => {
                failures = 0;
                let mut blocks = Vec::new();
                for delivery in deliveries.iter().take(MAX_PRINTED) {
                    // Printed only once it is this run's: one another agent
                    // holds, or one whose claim did not get through, is
                    // offered again.
                    if client.claim_delivery(delivery.id, &who).await.is_ok() {
                        blocks.push(framed(delivery, TOOL));
                    }
                }
                if !blocks.is_empty() {
                    say(&blocks.join("\n\n"));
                    return EXIT_ARRIVED;
                }
                poll
            }
            // The feature is off for this workspace, or on this server: it
            // may be switched on while the agent waits, so keep asking, rarely.
            Err(error) if is_not_there(&error) => {
                failures = 0;
                poll.saturating_mul(NOT_THERE_SLOWER)
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
