//! Tools the ContextStream API defines for the caller ("hosted tools").
//!
//! The API lists the extra tools it offers a caller at `GET /mcp/hosted-tools`
//! and runs a call to one at `POST /mcp/hosted-tools/{name}/call`. For a tool
//! whose transport is `stream`, that answer names an endpoint and a token for
//! this one call, and the result comes from the endpoint. The endpoint is known
//! only at call time: nothing here names one.

use serde_json::Value;
use std::time::Duration;

use crate::client::ContextStreamClient;
use mcp_types::Result;

/// Where the API lists the tools it defines for the caller.
pub const HOSTED_TOOLS_PATH: &str = "/mcp/hosted-tools";

/// An answer from the hosted-tools routes.
#[derive(Debug, Clone)]
pub struct HostedToolsAnswer {
    pub status: u16,
    /// The body, unwrapped from ContextStream's `{success, data}` envelope when it has one.
    pub data: Value,
    /// The whole body as sent (for an error: `{success: false, error: {code, message}}`).
    pub body: Value,
}

impl HostedToolsAnswer {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The error's message, when the answer is an error that has one.
    pub fn error_message(&self) -> Option<String> {
        self.body
            .pointer("/error/message")
            .and_then(Value::as_str)
            .or_else(|| self.body.get("message").and_then(Value::as_str))
            .or_else(|| self.body.get("error").and_then(Value::as_str))
            .map(str::to_string)
    }

    fn from_body(status: u16, body: Value) -> Self {
        let data = match &body {
            Value::Object(map) if map.contains_key("success") && map.contains_key("data") => {
                map.get("data").cloned().unwrap_or(Value::Null)
            }
            other => other.clone(),
        };
        Self { status, data, body }
    }
}

impl ContextStreamClient {
    /// The tool definitions the API offers this caller.
    pub async fn hosted_tools_catalog(
        &self,
        workspace_id: Option<&str>,
        timeout: Duration,
    ) -> Result<HostedToolsAnswer> {
        let path = match workspace_id {
            Some(id) => format!(
                "{HOSTED_TOOLS_PATH}?workspace_id={}",
                urlencoding::encode(id)
            ),
            None => HOSTED_TOOLS_PATH.to_string(),
        };
        let raw = self.send_raw_json("GET", &path, None, timeout).await?;
        Ok(HostedToolsAnswer::from_body(raw.status, raw.body))
    }

    /// Run the hosted tool `name` through the API. `body` is
    /// `{"arguments": …}`, with the caller's `workspace_id` and, for a tool that
    /// asks for one, a `checkout` (see `checkout_snapshot`).
    pub async fn hosted_tool_call(
        &self,
        name: &str,
        body: &Value,
        timeout: Duration,
    ) -> Result<HostedToolsAnswer> {
        let path = format!("{HOSTED_TOOLS_PATH}/{name}/call");
        let raw = self
            .send_raw_json("POST", &path, Some(body), timeout)
            .await?;
        Ok(HostedToolsAnswer::from_body(raw.status, raw.body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_is_unwrapped_from_the_envelope() {
        let answer = HostedToolsAnswer::from_body(
            200,
            serde_json::json!({"success": true, "data": {"tools": []}}),
        );
        assert_eq!(answer.data, serde_json::json!({"tools": []}));
        let error = HostedToolsAnswer::from_body(
            403,
            serde_json::json!({"success": false, "error": {"code": "FORBIDDEN", "message": "Not for you"}}),
        );
        assert!(!error.is_success());
        assert_eq!(error.error_message().as_deref(), Some("Not for you"));
    }
}
