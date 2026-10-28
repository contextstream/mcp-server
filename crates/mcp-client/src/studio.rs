//! ContextStream Create as the `studio` tool calls it.
//!
//! Each call is sent once and answered whole (its status, its `Retry-After` and its body),
//! so the tool can say in one sentence what went wrong instead of a folded HTTP error. A
//! write carries the idempotency key the tool chose, so a making started twice is one job.

use base64::Engine;
use serde_json::Value;
use std::time::Duration;

use crate::client::{ContextStreamClient, RawBody};
use mcp_types::Result;

/// The header Create reads a write's idempotency key from.
pub const IDEMPOTENCY_KEY_HEADER: &str = "Idempotency-Key";

/// One answer from the API, whatever its status.
#[derive(Debug, Clone, PartialEq)]
pub struct StudioAnswer {
    pub status: u16,
    /// The body, unwrapped from ContextStream's `{success, data}` envelope when it has one.
    pub data: Value,
    /// The whole body as sent (for an error: `{success: false, error: {code, message}}`).
    pub body: Value,
    /// `Retry-After`, in seconds.
    pub retry_after: Option<u64>,
}

impl StudioAnswer {
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// The error's stable code (`error.code`), when the answer is an error that has one.
    pub fn error_code(&self) -> Option<&str> {
        self.body
            .pointer("/error/code")
            .and_then(Value::as_str)
            .or_else(|| self.body.get("code").and_then(Value::as_str))
    }

    /// The error's message, with the API's category prefix ("Bad request: ") taken off.
    pub fn error_message(&self) -> Option<String> {
        let message = self
            .body
            .pointer("/error/message")
            .and_then(Value::as_str)
            .or_else(|| self.body.get("message").and_then(Value::as_str))
            .or_else(|| self.body.get("error").and_then(Value::as_str))?;
        Some(strip_category(message).to_string())
    }

    /// `error.details`, when the error carries them (402: required and available credits).
    pub fn error_details(&self) -> Option<&Value> {
        self.body.pointer("/error/details")
    }

    fn from_json(status: u16, headers: &reqwest::header::HeaderMap, body: Value) -> Self {
        let retry_after = headers
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok());
        let data = match &body {
            Value::Object(map) if map.contains_key("success") && map.contains_key("data") => {
                map.get("data").cloned().unwrap_or(Value::Null)
            }
            other => other.clone(),
        };
        Self {
            status,
            data,
            body,
            retry_after,
        }
    }
}

/// The API's error messages start with their category ("Bad request: ", "Not found: ");
/// a person needs only the sentence.
fn strip_category(message: &str) -> &str {
    const CATEGORIES: &[&str] = &[
        "Bad request: ",
        "Forbidden: ",
        "Not found: ",
        "Conflict: ",
        "Gone: ",
        "Service unavailable: ",
        "Too many requests: ",
        "Unauthorized: ",
        "Validation error: ",
        "Payment required: ",
    ];
    CATEGORIES
        .iter()
        .find_map(|category| message.strip_prefix(category))
        .unwrap_or(message)
}

/// A made piece's bytes, read for showing inline.
#[derive(Debug, Clone, PartialEq)]
pub enum OutputBytes {
    /// The piece, base64-encoded, with its type.
    Read { mime_type: String, base64: String },
    /// Larger than the caller would read.
    TooLarge,
    /// The API refused it (status).
    Refused(u16),
}

impl ContextStreamClient {
    /// One request to the API, answered whole. Only a connection failure or a timeout is an
    /// error. A write may carry its idempotency key.
    pub async fn studio_call(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        idempotency_key: Option<&str>,
        timeout: Duration,
    ) -> Result<StudioAnswer> {
        let headers: Vec<(&str, String)> = idempotency_key
            .map(|key| vec![(IDEMPOTENCY_KEY_HEADER, key.to_string())])
            .unwrap_or_default();
        let raw = self
            .send_raw(
                method,
                path,
                body.map(RawBody::Json),
                &headers,
                timeout,
                usize::MAX,
            )
            .await?
            .into_json();
        Ok(StudioAnswer::from_json(raw.status, &raw.headers, raw.body))
    }

    /// Send an image as its bytes (`POST /create/attachments`), answered whole.
    pub async fn studio_attach(
        &self,
        path: &str,
        content_type: &str,
        bytes: &[u8],
        timeout: Duration,
    ) -> Result<StudioAnswer> {
        let raw = self
            .send_raw(
                "POST",
                path,
                Some(RawBody::Bytes {
                    content_type,
                    bytes,
                }),
                &[],
                timeout,
                usize::MAX,
            )
            .await?
            .into_json();
        Ok(StudioAnswer::from_json(raw.status, &raw.headers, raw.body))
    }

    /// A made piece's bytes (`/api/v1/create/outputs/{id}/content?...`), when it is at most
    /// `max_bytes`.
    pub async fn studio_output_bytes(
        &self,
        path: &str,
        max_bytes: usize,
        timeout: Duration,
    ) -> Result<OutputBytes> {
        let raw = self
            .send_raw("GET", path, None, &[], timeout, max_bytes)
            .await?;
        if raw.truncated {
            return Ok(OutputBytes::TooLarge);
        }
        if !(200..300).contains(&raw.status) {
            return Ok(OutputBytes::Refused(raw.status));
        }
        let mime_type = raw
            .headers
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.split(';').next().unwrap_or(value).trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "application/octet-stream".to_string());
        Ok(OutputBytes::Read {
            mime_type,
            base64: base64::engine::general_purpose::STANDARD.encode(&raw.bytes),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_answer_reads_its_envelope_code_and_sentence() {
        let headers = reqwest::header::HeaderMap::new();
        let ok = StudioAnswer::from_json(
            202,
            &headers,
            json!({"success": true, "data": {"id": "q"}, "error": null}),
        );
        assert!(ok.is_success());
        assert_eq!(ok.data, json!({"id": "q"}));

        let refused = StudioAnswer::from_json(
            422,
            &headers,
            json!({"success": false, "data": null,
                   "error": {"code": "brand_unavailable", "message": "Add a brand kit first."}}),
        );
        assert!(!refused.is_success());
        assert_eq!(refused.error_code(), Some("brand_unavailable"));
        assert_eq!(
            refused.error_message().as_deref(),
            Some("Add a brand kit first.")
        );

        let prefixed = StudioAnswer::from_json(
            400,
            &headers,
            json!({"success": false, "error": {"code": "BAD_REQUEST", "message": "Bad request: Choose image, video or sound."}}),
        );
        assert_eq!(
            prefixed.error_message().as_deref(),
            Some("Choose image, video or sound.")
        );

        let mut retry = reqwest::header::HeaderMap::new();
        retry.insert(reqwest::header::RETRY_AFTER, "5".parse().unwrap());
        let planning = StudioAnswer::from_json(202, &retry, json!({"id": "q"}));
        assert_eq!(planning.retry_after, Some(5));
        assert_eq!(planning.data, json!({"id": "q"}));
    }
}
