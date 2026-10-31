//! Whether the caller of a tool call may read a workspace (and project), asked
//! with the caller's own credentials.
//!
//! The acceleration products (warm cache, analytics, jobs, archive) call the
//! API under the gateway's service token, which reads any workspace. Before one
//! of them acts for a caller, the gateway checks the caller can read the scope
//! it names; the API refuses a request that does not say it was checked.

use mcp_client::ContextStreamClient;
use mcp_types::{Error, Result};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use uuid::Uuid;

/// How long an answer is kept. Losing access is noticed within this time;
/// a refusal is kept shorter so access just granted works soon.
const ALLOWED_FOR: Duration = Duration::from_secs(600);
const REFUSED_FOR: Duration = Duration::from_secs(60);
const MAX_ANSWERS: usize = 20_000;

type ScopeKey = (String, Uuid, Option<Uuid>);

fn answers() -> &'static Mutex<HashMap<ScopeKey, (bool, Instant)>> {
    static ANSWERS: OnceLock<Mutex<HashMap<ScopeKey, (bool, Instant)>>> = OnceLock::new();
    ANSWERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn caller_identity() -> Option<String> {
    super::atlas_warm_cache::current_user_scope_token()
}

fn remembered(key: &ScopeKey) -> Option<bool> {
    let answers = answers()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let (allowed, at) = *answers.get(key)?;
    let keep = if allowed { ALLOWED_FOR } else { REFUSED_FOR };
    (at.elapsed() < keep).then_some(allowed)
}

fn remember(key: ScopeKey, allowed: bool) {
    let mut answers = answers()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if answers.len() >= MAX_ANSWERS {
        answers.clear();
    }
    answers.insert(key, (allowed, Instant::now()));
}

/// The caller's access to the scope as last checked, without asking the API:
/// `None` when it has not been checked (or the answer is too old), and
/// `Some(false)` for a caller with no identity.
pub fn known_caller_access(workspace_id: Uuid, project_id: Option<Uuid>) -> Option<bool> {
    let Some(caller) = caller_identity() else {
        return Some(false);
    };
    remembered(&(caller, workspace_id, project_id))
}

/// Whether the caller can read `workspace_id` and, when given, `project_id`
/// (which must belong to that workspace). Asks the API with the caller's own
/// credentials, and remembers the answer per caller.
pub async fn caller_can_read(
    client: &ContextStreamClient,
    workspace_id: Uuid,
    project_id: Option<Uuid>,
) -> Result<bool> {
    let Some(caller) = caller_identity() else {
        return Ok(false);
    };
    let key = (caller, workspace_id, project_id);
    if let Some(allowed) = remembered(&key) {
        return Ok(allowed);
    }
    let allowed = ask_api(client, workspace_id, project_id).await?;
    remember(key, allowed);
    Ok(allowed)
}

/// Starts [`caller_can_read`] for the current caller without waiting, so a
/// later call of the same scope knows the answer. For the warm cache, whose
/// lookups must not make the primary path slower.
pub fn check_caller_access_in_background(
    client: &ContextStreamClient,
    workspace_id: Uuid,
    project_id: Option<Uuid>,
) {
    let Some(auth) = mcp_client::get_task_auth_override() else {
        return;
    };
    let Some(caller) = caller_identity() else {
        return;
    };
    let client = client.clone();
    tokio::spawn(async move {
        let key = (caller, workspace_id, project_id);
        // The spawned task asks with the caller's own credentials.
        let answer = mcp_client::run_with_auth_override(auth, || async {
            ask_api(&client, workspace_id, project_id).await
        })
        .await;
        if let Ok(allowed) = answer {
            remember(key, allowed);
        }
    });
}

async fn ask_api(
    client: &ContextStreamClient,
    workspace_id: Uuid,
    project_id: Option<Uuid>,
) -> Result<bool> {
    match client.get_workspace(workspace_id).await {
        Ok(_) => {}
        Err(error) if is_refusal(&error) => return Ok(false),
        Err(error) => return Err(error),
    }
    let Some(project_id) = project_id else {
        return Ok(true);
    };
    match client.get_project(project_id).await {
        Ok(project) => Ok(project.workspace_id == Some(workspace_id)),
        Err(error) if is_refusal(&error) => Ok(false),
        Err(error) => Err(error),
    }
}

fn is_refusal(error: &Error) -> bool {
    matches!(error, Error::Http { status, .. } if matches!(status, 401 | 403 | 404))
}

/// What a tool answers when the caller cannot read the scope it named.
pub fn no_access_message(workspace_id: Uuid, project_id: Option<Uuid>) -> String {
    match project_id {
        Some(project_id) => format!(
            "You don't have access to project {project_id} in workspace {workspace_id} \
             (or the project is in another workspace). Use a workspace and project you can \
             read (workspace(action=\"list\"), project(action=\"list\"))."
        ),
        None => format!(
            "You don't have access to workspace {workspace_id}. Use a workspace you can read \
             (workspace(action=\"list\"))."
        ),
    }
}

#[cfg(test)]
pub(crate) fn remember_for_test(
    caller: &str,
    workspace_id: Uuid,
    project_id: Option<Uuid>,
    allowed: bool,
) {
    remember((caller.to_string(), workspace_id, project_id), allowed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remembered_refusal_expires_sooner_than_an_allowance() {
        let workspace = Uuid::from_u128(0x4380_0001);
        let allowed: ScopeKey = ("caller-a".to_string(), workspace, None);
        let refused: ScopeKey = ("caller-b".to_string(), workspace, None);
        remember(allowed.clone(), true);
        remember(refused.clone(), false);
        assert_eq!(remembered(&allowed), Some(true));
        assert_eq!(remembered(&refused), Some(false));
        // Another caller's answer is not this caller's.
        assert_eq!(remembered(&("caller-c".to_string(), workspace, None)), None);
        {
            let mut answers = answers().lock().unwrap();
            let past = Instant::now() - REFUSED_FOR - Duration::from_secs(1);
            answers.insert(allowed.clone(), (true, past));
            answers.insert(refused.clone(), (false, past));
        }
        assert_eq!(remembered(&allowed), Some(true));
        assert_eq!(remembered(&refused), None);
    }

    #[test]
    fn a_caller_without_identity_is_never_allowed() {
        // No task-local identity in a plain test thread.
        assert_eq!(
            known_caller_access(Uuid::from_u128(0x4380_0002), None),
            Some(false)
        );
    }

    #[test]
    fn refusals_are_401_403_and_404_only() {
        assert!(is_refusal(&Error::http(403, "forbidden")));
        assert!(is_refusal(&Error::http(404, "missing")));
        assert!(is_refusal(&Error::http(401, "unauthorized")));
        assert!(!is_refusal(&Error::http(500, "boom")));
        assert!(!is_refusal(&Error::Timeout(30)));
    }
}
