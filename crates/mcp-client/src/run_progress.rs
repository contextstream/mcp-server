//! Live run progress (contract v1.3, `run_progress.v1`): what an agent is
//! doing now, written to the ContextStream API so people can watch it.
//!
//! This client keeps no run state. It uses the flat form the API offers to
//! clients like it, under the client's `/api/v1` base and with the caller's
//! own credential:
//!
//! - `POST /run-progress/report` takes the caller's stable id for a piece of
//!   work (`source_run_id`), who is working (`agent`) and whatever changed
//!   (step, checklist, links, waiting, status). The API derives the run id
//!   from the workspace, the runtime and that id, keeps the run's sequence,
//!   and writes only what changed: a report that changes nothing is a
//!   heartbeat, and a report sent twice is harmless;
//! - `GET /run-progress/runs?workspace_id&project_id&loop_id&ticket_id&active&limit`
//!   lists runs, and `GET /run-progress/runs/:run_id?workspace_id=` reads one
//!   (a run is looked up inside the workspace the caller names, never across
//!   workspaces).
//!
//! Reports are frequent and replay-safe, so `run-progress` is deliberately
//! not one of the agent write routes: no `Idempotency-Key` is sent and the
//! API's key store is not used for them.

use std::time::Duration;

use serde_json::Value;
use uuid::Uuid;

use crate::client::{ContextStreamClient, RequestOptions};
use mcp_types::Result;

/// `agent.runtime` of every run this server reports.
pub const MCP_RUNTIME: &str = "mcp";

/// `status` values of a report.
pub const RUN_STATUSES: &[&str] = &[
    "working",
    "waiting_input",
    "waiting_approval",
    "completed",
    "partial",
    "stopped",
    "failed",
    "cancelled",
];

/// The statuses that end a run.
pub const TERMINAL_RUN_STATUSES: &[&str] =
    &["completed", "partial", "stopped", "failed", "cancelled"];

/// `state` values of a checklist item.
pub const PLAN_ITEM_STATES: &[&str] = &["next", "in_progress", "done", "skipped"];

/// `kind` values of a link.
pub const LINK_KINDS: &[&str] = &["pr", "preview", "deploy", "doc", "other"];

/// What a waiting run waits on.
pub const WAITING_ON: &[&str] = &[
    "person",
    "approval",
    "budget",
    "rate_limit",
    "sign_in",
    "dependency",
    "error",
];

/// How long one report may take. A report is a side note to the agent's
/// work: one the API has not answered by then is given up, not waited for.
pub const REPORT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a read of runs may take.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Which runs `GET /run-progress/runs` lists.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunFilter {
    pub workspace_id: Option<Uuid>,
    pub project_id: Option<Uuid>,
    pub loop_id: Option<Uuid>,
    pub ticket_id: Option<Uuid>,
    /// `Some(true)`: runs still going; `Some(false)`: ended runs.
    pub active: Option<bool>,
    pub limit: Option<i64>,
}

impl RunFilter {
    fn query(&self) -> String {
        let mut params = Vec::new();
        if let Some(id) = self.workspace_id {
            params.push(format!("workspace_id={id}"));
        }
        if let Some(id) = self.project_id {
            params.push(format!("project_id={id}"));
        }
        if let Some(id) = self.loop_id {
            params.push(format!("loop_id={id}"));
        }
        if let Some(id) = self.ticket_id {
            params.push(format!("ticket_id={id}"));
        }
        if let Some(active) = self.active {
            params.push(format!("active={active}"));
        }
        if let Some(limit) = self.limit.filter(|value| *value > 0) {
            params.push(format!("limit={limit}"));
        }
        if params.is_empty() {
            String::new()
        } else {
            format!("?{}", params.join("&"))
        }
    }
}

impl ContextStreamClient {
    /// Report what a run is doing. Endpoint: `POST /run-progress/report`,
    /// answered with `{run, written, cursor}`.
    ///
    /// One attempt of [`REPORT_TIMEOUT`] plus `retries` more after a failure
    /// that is safe to resend (every one is: the API writes only what
    /// changed). A 403 is passed on as it is, with no session refresh: a
    /// report never re-initializes the session.
    pub async fn report_run_progress(&self, body: &Value, retries: u32) -> Result<Value> {
        self.post_without_session_refresh(
            "/run-progress/report",
            body,
            RequestOptions {
                timeout: Some(REPORT_TIMEOUT),
                retries: Some(retries),
                ..RequestOptions::default()
            },
        )
        .await
    }

    /// Runs the caller can read. Endpoint:
    /// `GET /run-progress/runs?workspace_id&project_id&loop_id&ticket_id&active&limit`,
    /// answered with `{runs}`. The filter is sent as given: no default scope
    /// is added here.
    pub async fn list_run_progress(&self, filter: &RunFilter) -> Result<Value> {
        self.get_with_options(
            &format!("/run-progress/runs{}", filter.query()),
            RequestOptions {
                timeout: Some(READ_TIMEOUT),
                ..RequestOptions::default()
            },
        )
        .await
    }

    /// One run of a workspace. Endpoint:
    /// `GET /run-progress/runs/:run_id?workspace_id=`; the workspace is
    /// required, and a run of another workspace is not found.
    pub async fn get_run_progress(&self, workspace_id: Uuid, run_id: Uuid) -> Result<Value> {
        self.get_with_options(
            &format!("/run-progress/runs/{run_id}?workspace_id={workspace_id}"),
            RequestOptions {
                timeout: Some(READ_TIMEOUT),
                ..RequestOptions::default()
            },
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_filter_sends_only_what_was_given() {
        assert_eq!(RunFilter::default().query(), "");
        let workspace = Uuid::new_v4();
        let ticket = Uuid::new_v4();
        let filter = RunFilter {
            workspace_id: Some(workspace),
            ticket_id: Some(ticket),
            active: Some(true),
            limit: Some(0),
            ..RunFilter::default()
        };
        assert_eq!(
            filter.query(),
            format!("?workspace_id={workspace}&ticket_id={ticket}&active=true")
        );
        let ended = RunFilter {
            active: Some(false),
            limit: Some(5),
            ..RunFilter::default()
        };
        assert_eq!(ended.query(), "?active=false&limit=5");
    }

    #[test]
    fn every_terminal_status_is_a_status() {
        for status in TERMINAL_RUN_STATUSES {
            assert!(RUN_STATUSES.contains(status), "{status}");
        }
        assert!(!TERMINAL_RUN_STATUSES.contains(&"working"));
        assert!(!TERMINAL_RUN_STATUSES.contains(&"waiting_input"));
    }
}
