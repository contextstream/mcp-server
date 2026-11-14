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

use std::sync::OnceLock;
use std::time::Duration;

use serde_json::Value;
use uuid::Uuid;

use crate::client::{ContextStreamClient, RequestOptions};
use mcp_types::{AuthOverride, Config, Result};

/// The credential of a caller's own that a request carries to the API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SentCredential<'a> {
    /// The caller's API key (`X-API-Key`).
    ApiKey(&'a str),
    /// The caller's bearer token (`Authorization: Bearer`).
    Bearer(&'a str),
}

/// Which of the credentials a caller presented (`auth`) its requests go to
/// the API under, exactly as the client attaches them: the caller's API key;
/// else its bearer token, unless this process has an API key of its own
/// (`config`), which is then what is sent. `None` when the request carries no
/// credential of the caller's: what the API answers then says nothing about
/// the caller.
pub fn sent_credential<'a>(auth: &'a AuthOverride, config: &Config) -> Option<SentCredential<'a>> {
    let given = |value: &'a Option<String>| value.as_deref().filter(|value| !value.is_empty());
    if let Some(key) = given(&auth.api_key) {
        return Some(SentCredential::ApiKey(key));
    }
    if config.api_key.is_some() {
        return None;
    }
    given(&auth.jwt).map(SentCredential::Bearer)
}

/// The longest subject a token may name and still count as one.
pub const MAX_SUBJECT: usize = 256;

/// The subject (`sub`) a bearer token names, when the token is a JWT that
/// names one. Read from the token as it came: nothing is verified here, so
/// this is a label for the token's own requests, never an authority (the API
/// verifies the token on every request it is sent with).
pub fn bearer_subject(token: &str) -> Option<String> {
    use base64::Engine;

    let mut segments = token.split('.');
    let (_, claims, _) = (segments.next()?, segments.next()?, segments.next()?);
    if segments.next().is_some() {
        return None;
    }
    let claims = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(claims.trim_end_matches('='))
        .ok()?;
    let claims: Value = serde_json::from_slice(&claims).ok()?;
    let subject = claims.get("sub")?.as_str()?.trim();
    (!subject.is_empty() && subject.len() <= MAX_SUBJECT && !subject.chars().any(char::is_control))
        .then(|| subject.to_string())
}

/// Told of an answer of the API: the caller's credential the request carried
/// and the HTTP status.
pub type ApiAnswerObserver = fn(SentCredential<'_>, u16);

static API_ANSWER_OBSERVER: OnceLock<ApiAnswerObserver> = OnceLock::new();

/// From now on, tell `observer` of every answer the API gives to a request
/// that carried a caller's own credential (the hosted gateway's signs of life
/// go out only for a credential the API was seen to accept). Nothing is
/// observed until this is called, and the first observer stays.
pub fn observe_api_answers(observer: ApiAnswerObserver) {
    let _ = API_ANSWER_OBSERVER.set(observer);
}

/// The API answered a request sent under `auth` with `status`.
pub(crate) fn api_answered(auth: Option<&AuthOverride>, config: &Config, status: u16) {
    let Some(observer) = API_ANSWER_OBSERVER.get() else {
        return;
    };
    if let Some(credential) = auth.and_then(|auth| sent_credential(auth, config)) {
        observer(credential, status);
    }
}

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

    /// [`Self::get_run_progress`] for a side channel (a sign of life): one
    /// attempt, no retry and no session refresh, so a 403 or a 404 comes
    /// back as it is and nothing is logged above debug.
    pub async fn get_run_progress_quietly(
        &self,
        workspace_id: Uuid,
        run_id: Uuid,
    ) -> Result<Value> {
        self.get_without_session_refresh(
            &format!("/run-progress/runs/{run_id}?workspace_id={workspace_id}"),
            RequestOptions {
                timeout: Some(READ_TIMEOUT),
                retries: Some(0),
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
    fn the_credential_a_callers_request_carries_is_the_one_the_client_attaches() {
        let auth = |api_key: Option<&str>, jwt: Option<&str>| AuthOverride {
            api_key: api_key.map(str::to_string),
            jwt: jwt.map(str::to_string),
            ..Default::default()
        };
        let own_key = Config {
            api_key: Some("the-process-key".to_string()),
            ..Config::default()
        };
        let no_key = Config {
            api_key: None,
            ..Config::default()
        };
        // The caller's key goes first, whatever else there is.
        for config in [&own_key, &no_key] {
            assert_eq!(
                sent_credential(&auth(Some("key"), Some("token")), config),
                Some(SentCredential::ApiKey("key"))
            );
            assert_eq!(
                sent_credential(&auth(Some("key"), None), config),
                Some(SentCredential::ApiKey("key"))
            );
            // Nothing of the caller's: nothing.
            assert_eq!(sent_credential(&auth(None, None), config), None);
            assert_eq!(sent_credential(&auth(Some(""), Some("")), config), None);
        }
        // A bearer token is sent only when the process has no key to send
        // in its place.
        assert_eq!(
            sent_credential(&auth(None, Some("token")), &no_key),
            Some(SentCredential::Bearer("token"))
        );
        assert_eq!(sent_credential(&auth(None, Some("token")), &own_key), None);
    }

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
