//! Tools the ContextStream API defines for the caller ("hosted tools").
//!
//! The API lists the extra tools it offers a caller at `GET /mcp/hosted-tools`
//! and runs a call to one at `POST /mcp/hosted-tools/{name}/call`. For a tool
//! whose transport is `stream`, that answer names an endpoint and a token for
//! this one call, and the result comes from the endpoint. The endpoint is known
//! only at call time: nothing here names one.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::Duration;

use crate::client::ContextStreamClient;
use mcp_types::Result;

/// Where the API lists the tools it defines for the caller.
pub const HOSTED_TOOLS_PATH: &str = "/mcp/hosted-tools";

/// How long a known catalog is fresh.
pub const KNOWN_TTL: Duration = Duration::from_secs(300);
/// How long to wait before retrying an unreachable catalog.
pub const UNREACHABLE_RETRY: Duration = Duration::from_secs(30);

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

    /// A check refused for lack of credit. A past-due subscription uses
    /// the same HTTP status with its own code and billing message.
    pub fn insufficient_credits(&self) -> bool {
        let code = crate::client::extract_error_code(&self.body);
        code.as_deref() == Some("INSUFFICIENT_CREDITS")
            || (self.status == 402 && code.as_deref() != Some("PAYMENT_REQUIRED"))
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

    pub(crate) fn from_body(status: u16, body: Value) -> Self {
        let data = match &body {
            Value::Object(map) if map.contains_key("success") && map.contains_key("data") => {
                map.get("data").cloned().unwrap_or(Value::Null)
            }
            other => other.clone(),
        };
        Self { status, data, body }
    }
}

/// Request body for `POST /api/v1/mcp/hosted-tools/plan-check`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct PlanCheckRequest {
    pub workspace_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_key: Option<String>,
    pub changed_paths: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_downstream: Option<usize>,
}

/// A planned verification command returned by `/mcp/hosted-tools/plan-check`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct PlanCheckCommand {
    pub command: String,
    pub cwd: Option<String>,
    pub language: String,
    pub targets: Vec<String>,
    pub tier: String,
}

/// Unwrapped `data` payload returned by `POST /api/v1/mcp/hosted-tools/plan-check`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct PlanCheckResponse {
    pub verification_trace_id: Option<uuid::Uuid>,
    pub tier: String,
    pub reason: String,
    pub escalation_reasons: Vec<String>,
    pub project_id: Option<String>,
    pub downstream_count: usize,
    pub covering_tests: Vec<String>,
    pub co_changed_tests: Vec<String>,
    pub commands: Vec<PlanCheckCommand>,
    pub fallback_commands: Vec<PlanCheckCommand>,
    pub elapsed_ms: u64,
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

    /// Fetch bounded (<100 ms) context-enriched failure diagnostics for a failing hosted tool call.
    pub async fn hosted_tool_diagnose(
        &self,
        call_id: &str,
        body: &Value,
        timeout: Duration,
    ) -> Result<HostedToolsAnswer> {
        let path = format!(
            "{HOSTED_TOOLS_PATH}/calls/{}/diagnose",
            urlencoding::encode(call_id)
        );
        let raw = self
            .send_raw_json("POST", &path, Some(body), timeout)
            .await?;
        Ok(HostedToolsAnswer::from_body(raw.status, raw.body))
    }

    /// Ask the API's graph-driven check planner (`POST /api/v1/mcp/hosted-tools/plan-check`)
    /// whether `changed_paths` can be verified with targeted tests or require a full-suite run.
    pub async fn plan_check(
        &self,
        body: &PlanCheckRequest,
        timeout: Duration,
    ) -> Result<HostedToolsAnswer> {
        let path = format!("{HOSTED_TOOLS_PATH}/plan-check");
        let payload = serde_json::to_value(body)?;
        let raw = self
            .send_raw_json("POST", &path, Some(&payload), timeout)
            .await?;
        Ok(HostedToolsAnswer::from_body(raw.status, raw.body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_types::Config;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

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

    #[test]
    fn plan_check_types_tolerate_partial_fields_with_serde_default() {
        let req = PlanCheckRequest {
            workspace_id: "ws-1".to_string(),
            project_id: None,
            repo_key: Some("repo-12345678".to_string()),
            changed_paths: vec!["src/a.rs".to_string()],
            scope: Some("targeted".to_string()),
            max_downstream: None,
        };
        let encoded = serde_json::to_value(&req).expect("serialize request");
        assert!(encoded.get("project_id").is_none());
        assert_eq!(encoded["changed_paths"], serde_json::json!(["src/a.rs"]));

        let partial: PlanCheckResponse = serde_json::from_value(serde_json::json!({
            "tier": "targeted",
            "commands": [{"command": "cargo test -p foo"}]
        }))
        .expect("deserialize partial response");
        assert_eq!(partial.tier, "targeted");
        assert!(partial.escalation_reasons.is_empty());
        assert_eq!(partial.commands.len(), 1);
        assert_eq!(partial.commands[0].command, "cargo test -p foo");
        assert!(partial.commands[0].targets.is_empty());
    }

    #[tokio::test]
    async fn plan_check_posts_to_expected_route_and_unwraps_response() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("local_addr");

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut buf = vec![0u8; 8192];
            let n = socket.read(&mut buf).await.expect("read");
            let req_text = String::from_utf8_lossy(&buf[..n]).to_string();
            assert!(
                req_text.starts_with("POST /api/v1/mcp/hosted-tools/plan-check "),
                "unexpected request line: {req_text}"
            );
            assert!(req_text.contains("\"changed_paths\":[\"web/src/a.ts\"]"));

            let body = serde_json::json!({
                "success": true,
                "data": {
                    "tier": "targeted",
                    "reason": "Selected 1 targeted test",
                    "escalation_reasons": [],
                    "project_id": null,
                    "downstream_count": 2,
                    "covering_tests": ["web/src/a.test.ts"],
                    "co_changed_tests": [],
                    "commands": [{
                        "command": "npx vitest run web/src/a.test.ts",
                        "cwd": null,
                        "language": "typescript",
                        "targets": ["web/src/a.test.ts"],
                        "tier": "targeted"
                    }],
                    "fallback_commands": [],
                    "elapsed_ms": 3
                }
            })
            .to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(resp.as_bytes()).await.expect("write");
        });

        let config = Config {
            api_url: format!("http://{addr}"),
            api_key: Some("test-key".to_string()),
            ..Config::default()
        };
        let client = ContextStreamClient::new(config);
        let answer = client
            .plan_check(
                &PlanCheckRequest {
                    workspace_id: "ws-1".to_string(),
                    changed_paths: vec!["web/src/a.ts".to_string()],
                    scope: Some("targeted".to_string()),
                    ..Default::default()
                },
                Duration::from_secs(3),
            )
            .await
            .expect("plan_check");
        assert!(answer.is_success());
        let parsed: PlanCheckResponse =
            serde_json::from_value(answer.data).expect("PlanCheckResponse");
        assert_eq!(parsed.tier, "targeted");
        assert_eq!(parsed.downstream_count, 2);
        assert_eq!(
            parsed.commands[0].command,
            "npx vitest run web/src/a.test.ts"
        );
        server.await.expect("server join");
    }
}
