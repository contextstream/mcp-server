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
use mcp_types::{Error, Result};

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

/// Whether `url` may receive a hosted call: HTTPS, or plain HTTP to this machine.
pub fn hosted_stream_url_allowed(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    match parsed.scheme() {
        "https" => parsed.host_str().is_some(),
        "http" => matches!(
            parsed.host_str(),
            Some("localhost") | Some("127.0.0.1") | Some("[::1]")
        ),
        _ => false,
    }
}

impl ContextStreamClient {
    /// The tool definitions the API offers this caller.
    pub async fn hosted_tools_catalog(&self, timeout: Duration) -> Result<HostedToolsAnswer> {
        let raw = self
            .send_raw_json("GET", HOSTED_TOOLS_PATH, None, timeout)
            .await?;
        Ok(HostedToolsAnswer::from_body(raw.status, raw.body))
    }

    /// Run the hosted tool `name` through the API.
    pub async fn hosted_tool_call(
        &self,
        name: &str,
        arguments: &Value,
        timeout: Duration,
    ) -> Result<HostedToolsAnswer> {
        let path = format!("{HOSTED_TOOLS_PATH}/{name}/call");
        let body = serde_json::json!({ "arguments": arguments });
        let raw = self
            .send_raw_json("POST", &path, Some(&body), timeout)
            .await?;
        Ok(HostedToolsAnswer::from_body(raw.status, raw.body))
    }

    /// Read a call's result from the endpoint the API named for it. The endpoint
    /// gets the call's own token, never this client's API key; it answers with
    /// newline-delimited JSON frames, returned whole with the status.
    pub async fn hosted_tool_stream(
        &self,
        url: &str,
        token: &str,
        arguments: &Value,
        timeout: Duration,
    ) -> Result<(u16, String)> {
        if !hosted_stream_url_allowed(url) {
            return Err(Error::Config(
                "a hosted tool's stream endpoint must be an https URL".to_string(),
            ));
        }
        // Its own client: the call's token goes to the named endpoint only, and a
        // redirect is never followed to somewhere else.
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|error| Error::Network(error.without_url().to_string()))?;
        let response = http
            .post(url)
            .bearer_auth(token)
            .json(&serde_json::json!({ "arguments": arguments }))
            .timeout(timeout)
            .send()
            .await
            .map_err(|error| Error::Network(error.without_url().to_string()))?;
        let status = response.status().as_u16();
        let text = response
            .text()
            .await
            .map_err(|error| Error::Network(error.without_url().to_string()))?;
        Ok((status, text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stream_endpoint_is_https_or_this_machine() {
        assert!(hosted_stream_url_allowed("https://example.com/run"));
        assert!(hosted_stream_url_allowed("http://127.0.0.1:8123/run"));
        assert!(hosted_stream_url_allowed("http://localhost:8123/run"));
        assert!(!hosted_stream_url_allowed("http://example.com/run"));
        assert!(!hosted_stream_url_allowed("ws://example.com/run"));
        assert!(!hosted_stream_url_allowed("file:///etc/passwd"));
        assert!(!hosted_stream_url_allowed("not a url"));
    }

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
