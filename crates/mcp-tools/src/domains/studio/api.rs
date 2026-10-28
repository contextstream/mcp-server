//! The ContextStream routes the studio calls, and their refusals in one sentence.

use mcp_client::studio::StudioAnswer;
use mcp_client::ContextStreamClient;
use mcp_types::{Error, Result};
use serde_json::{json, Value};
use std::time::Duration;
use uuid::Uuid;

use super::words::{credits, quote_problem};

/// How long one ordinary read or write may take.
pub(crate) const CALL_TIMEOUT: Duration = Duration::from_secs(30);
/// How long an image upload may take.
pub(crate) const UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);

/// What a refused call was about, for its sentence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Subject {
    Plan,
    Making,
    Attachment,
    Product,
    Other,
}

impl Subject {
    fn missing(self) -> &'static str {
        match self {
            Self::Plan => "There is no such plan in this workspace (a plan is the plan_id the plan action returned).",
            Self::Making => "There is no such making in this workspace (a making is the make_id the make action returned).",
            Self::Attachment => "That attachment isn't in this workspace.",
            Self::Product => "There is no such product in this workspace.",
            Self::Other => "ContextStudio couldn't find that in this workspace.",
        }
    }
}

/// Said when ContextStudio isn't open to the caller.
pub(crate) const NOT_OPEN: &str = "ContextStudio isn't open to this account yet: one-prompt making is in a pilot. Nothing was made or charged.";

/// One refused answer in one sentence (or two), for the person and their agent.
pub(crate) fn refusal(answer: &StudioAnswer, subject: Subject) -> String {
    let message = answer.error_message();
    let said = message.as_deref().unwrap_or("").trim();
    match answer.status {
        401 => "ContextStream didn't accept this sign-in: sign in again (or set a new API key), then try once more.".to_string(),
        402 => {
            let details = answer.error_details();
            let needed = details
                .and_then(|d| d.get("required"))
                .and_then(Value::as_i64);
            let available = details
                .and_then(|d| d.get("available"))
                .and_then(Value::as_i64);
            match (needed, available) {
                (Some(needed), Some(available)) => format!(
                    "Not enough credits: this needs up to {} and the account has {}. Add credits, then make it again. Nothing was charged.",
                    credits(needed),
                    credits(available)
                ),
                _ => "Not enough credits for this. Add credits, then make it again. Nothing was charged.".to_string(),
            }
        }
        403 if said.contains("Create isn't available") => NOT_OPEN.to_string(),
        400 if said == "Choose image, video or sound." => NOT_OPEN.to_string(),
        404 => subject.missing().to_string(),
        409 if subject == Subject::Plan || said.contains("planning") => {
            "The plan is still being made: read it again with the plan action and its plan_id in a moment.".to_string()
        }
        410 => "This plan has expired (plans keep for 24 hours): plan it again.".to_string(),
        422 => match answer.error_code() {
            Some(code) if is_make_code(code) => quote_problem(Some(code)).to_string(),
            _ if !said.is_empty() => said.to_string(),
            _ => quote_problem(None).to_string(),
        },
        429 if !said.is_empty() => said.to_string(),
        429 => "ContextStudio is busy for this account: try again in a minute.".to_string(),
        500..=599 if !said.is_empty() => said.to_string(),
        500..=599 => "ContextStudio isn't answering right now: try again in a few minutes. Nothing was charged.".to_string(),
        _ if !said.is_empty() => said.to_string(),
        status => format!("ContextStudio refused this (HTTP {status})."),
    }
}

/// The closed set of codes a plan or a making fails with.
pub(crate) fn is_make_code(code: &str) -> bool {
    matches!(
        code,
        "make_unplanned"
            | "media_unavailable"
            | "brand_unavailable"
            | "reference_unavailable"
            | "model_unavailable"
            | "invalid_request"
            | "engine_unavailable"
            | "timed_out"
    )
}

/// Create, for one caller.
pub(crate) struct Create<'a> {
    pub client: &'a ContextStreamClient,
}

impl Create<'_> {
    async fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        key: Option<&str>,
    ) -> Result<StudioAnswer> {
        self.client
            .studio_call(method, path, body, key, CALL_TIMEOUT)
            .await
    }

    pub async fn quote(
        &self,
        workspace_id: Uuid,
        project_id: Option<Uuid>,
        request: &Value,
        notes: &[Value],
    ) -> Result<StudioAnswer> {
        let mut body = json!({ "workspaceId": workspace_id, "request": request });
        if let Some(project_id) = project_id {
            body["projectId"] = json!(project_id);
        }
        if !notes.is_empty() {
            body["context"] = json!(notes);
        }
        self.call("POST", "/create/quotes", Some(&body), None).await
    }

    pub async fn read_quote(&self, workspace_id: Uuid, quote_id: Uuid) -> Result<StudioAnswer> {
        self.call(
            "GET",
            &format!("/create/quotes/{quote_id}?workspaceId={workspace_id}"),
            None,
            None,
        )
        .await
    }

    pub async fn start_job(
        &self,
        workspace_id: Uuid,
        project_id: Option<Uuid>,
        quote: &Value,
        quote_id: Uuid,
    ) -> Result<StudioAnswer> {
        let mut body = json!({
            "workspaceId": workspace_id,
            "request": quote.get("request").cloned().unwrap_or(Value::Null),
            "quoteId": quote_id,
        });
        if let Some(project_id) = project_id {
            body["projectId"] = json!(project_id);
        }
        // The key the web studio uses for a plan, so one plan is made once.
        let key = format!("make-{quote_id}");
        self.call("POST", "/create/jobs", Some(&body), Some(&key))
            .await
    }

    pub async fn job(&self, workspace_id: Uuid, job_id: Uuid) -> Result<StudioAnswer> {
        self.call(
            "GET",
            &format!("/create/jobs/{job_id}?workspaceId={workspace_id}"),
            None,
            None,
        )
        .await
    }

    pub async fn jobs(&self, workspace_id: Uuid, limit: i64) -> Result<StudioAnswer> {
        self.call(
            "GET",
            &format!("/create/jobs?workspaceId={workspace_id}&limit={limit}"),
            None,
            None,
        )
        .await
    }

    pub async fn cancel(&self, workspace_id: Uuid, job_id: Uuid) -> Result<StudioAnswer> {
        self.call(
            "POST",
            &format!("/create/jobs/{job_id}/cancel"),
            Some(&json!({ "workspaceId": workspace_id })),
            None,
        )
        .await
    }

    pub async fn balance(&self) -> Option<i64> {
        let answer = self
            .call("GET", "/credits/balance", None, None)
            .await
            .ok()?;
        if !answer.is_success() {
            return None;
        }
        answer
            .data
            .pointer("/credits/spendable")
            .or_else(|| answer.data.pointer("/credits/total_available"))
            .and_then(Value::as_i64)
    }

    pub async fn brand(&self, workspace_id: Uuid) -> Result<StudioAnswer> {
        self.call(
            "GET",
            &format!("/brand-kits/resolve?scope=workspace&workspace_id={workspace_id}"),
            None,
            None,
        )
        .await
    }

    pub async fn products(&self, workspace_id: Uuid) -> Result<StudioAnswer> {
        self.call(
            "GET",
            &format!("/create/products?workspaceId={workspace_id}"),
            None,
            None,
        )
        .await
    }

    pub async fn current_map(&self, workspace_id: Uuid, product_id: &str) -> Result<StudioAnswer> {
        self.call(
            "GET",
            &format!("/create/products/{product_id}/map?workspaceId={workspace_id}"),
            None,
            None,
        )
        .await
    }

    /// A short-lived signed link to a stored piece (`download_url`, `expires_in_seconds`).
    pub async fn download_url(
        &self,
        workspace_id: Uuid,
        content_id: &str,
    ) -> Option<(String, Option<i64>)> {
        let answer = self
            .call(
                "GET",
                &format!("/workspaces/{workspace_id}/content/{content_id}/download-url"),
                None,
                None,
            )
            .await
            .ok()?;
        if !answer.is_success() {
            return None;
        }
        let url = answer
            .data
            .get("download_url")
            .and_then(Value::as_str)?
            .to_string();
        let expires = answer
            .data
            .get("expires_in_seconds")
            .and_then(Value::as_i64);
        Some((url, expires))
    }

    pub async fn attach(
        &self,
        workspace_id: Uuid,
        project_id: Option<Uuid>,
        filename: &str,
        mime_type: &str,
        bytes: &[u8],
    ) -> Result<StudioAnswer> {
        let mut path = format!(
            "/create/attachments?workspaceId={workspace_id}&filename={}",
            urlencoding::encode(filename)
        );
        if let Some(project_id) = project_id {
            path.push_str(&format!("&projectId={project_id}"));
        }
        self.client
            .studio_attach(&path, mime_type, bytes, UPLOAD_TIMEOUT)
            .await
    }
}

/// A network failure, said plainly.
pub(crate) fn unreachable(error: Error) -> Error {
    match error {
        Error::Timeout(_) | Error::Network(_) => Error::Network(format!(
            "ContextStudio could not be reached ({error}); nothing was changed. Try again."
        )),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(status: u16, body: Value) -> StudioAnswer {
        let data = body.get("data").cloned().unwrap_or(Value::Null);
        StudioAnswer {
            status,
            data,
            body,
            retry_after: None,
        }
    }

    #[test]
    fn refusals_are_one_plain_sentence() {
        let pilot = answer(
            403,
            json!({"success": false, "error": {"code": "FORBIDDEN", "message": "Forbidden: Create isn't available for this account yet."}}),
        );
        assert_eq!(refusal(&pilot, Subject::Plan), NOT_OPEN);
        let not_make = answer(
            400,
            json!({"success": false, "error": {"code": "BAD_REQUEST", "message": "Bad request: Choose image, video or sound."}}),
        );
        assert_eq!(refusal(&not_make, Subject::Plan), NOT_OPEN);
        let credits_short = answer(
            402,
            json!({"success": false, "error": {"code": "INSUFFICIENT_CREDITS", "message": "You've run out", "details": {"required": 4258, "available": 1200}}}),
        );
        assert_eq!(
            refusal(&credits_short, Subject::Making),
            "Not enough credits: this needs up to 4,258 and the account has 1,200. Add credits, then make it again. Nothing was charged."
        );
        let brand = answer(
            422,
            json!({"success": false, "error": {"code": "brand_unavailable", "message": "x"}}),
        );
        assert!(refusal(&brand, Subject::Plan).contains("has none yet"));
        let expired = answer(
            410,
            json!({"success": false, "error": {"message": "Gone: expired"}}),
        );
        assert!(refusal(&expired, Subject::Making).contains("expired"));
        let missing = answer(
            404,
            json!({"success": false, "error": {"message": "Not found: No such job."}}),
        );
        assert!(refusal(&missing, Subject::Making).starts_with("There is no such making"));
        let budget = answer(
            429,
            json!({"success": false, "error": {"message": "Too many requests: This needs 3000 credits, and agents have 10 of today's 100000 Create credits left in this workspace (agent budget)."}}),
        );
        assert!(refusal(&budget, Subject::Making).starts_with("This needs 3000 credits"));
        let down = answer(502, Value::String("<html>".into()));
        assert!(refusal(&down, Subject::Plan).contains("isn't answering"));
    }
}
