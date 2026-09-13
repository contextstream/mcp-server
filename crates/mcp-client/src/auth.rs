//! Device flow authentication for ContextStream.
//!
//! Implements OAuth 2.0 Device Authorization Grant (RFC 8628).

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Device login response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceLoginResponse {
    /// Device code for polling.
    pub device_code: String,

    /// User code to display.
    pub user_code: String,

    /// Verification URL for user.
    pub verification_uri: String,

    /// Polling interval in seconds.
    #[serde(default = "default_interval")]
    pub interval: u64,

    /// Expiration time in seconds.
    #[serde(default = "default_expires_in")]
    pub expires_in: u64,
}

fn default_interval() -> u64 {
    5
}

fn default_expires_in() -> u64 {
    600
}

/// Token response from device flow completion.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenResponse {
    /// Access token (JWT).
    pub access_token: String,

    /// Token type (usually "Bearer").
    pub token_type: String,

    /// Expiration time in seconds.
    #[serde(default)]
    pub expires_in: Option<u64>,

    /// Refresh token (if provided).
    #[serde(default)]
    pub refresh_token: Option<String>,
}

/// Device flow polling error.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceFlowError {
    pub error: String,
    pub error_description: Option<String>,
}

/// Get the base API URL.
fn api_url() -> String {
    std::env::var("CONTEXTSTREAM_API_URL")
        .unwrap_or_else(|_| "https://api.contextstream.io".to_string())
}

/// Start device login flow.
pub async fn start_device_login() -> Result<DeviceLoginResponse> {
    let client = reqwest::Client::new();
    let url = format!("{}/api/v1/auth/device/start", api_url());

    let response = client
        .post(&url)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({
            "client_id": "contextstream-cli",
            "scope": "openid profile email"
        }))
        .send()
        .await?;

    if !response.status().is_success() {
        let error_text = response.text().await?;
        return Err(anyhow::anyhow!(
            "Failed to start device login: {}",
            error_text
        ));
    }

    let device_response: DeviceLoginResponse = response.json().await?;
    Ok(device_response)
}

/// Poll for device login completion.
pub async fn poll_device_login(device_code: &str, interval: u64) -> Result<TokenResponse> {
    let client = reqwest::Client::new();
    let url = format!("{}/api/v1/auth/device/token", api_url());
    let mut poll_interval = Duration::from_secs(interval);

    loop {
        tokio::time::sleep(poll_interval).await;

        let response = client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&serde_json::json!({
                "device_code": device_code,
            }))
            .send()
            .await?;

        if !response.status().is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!("Device login poll failed: {}", error_text));
        }

        // The API returns 200 for both pending and authorized states.
        // Parse as generic JSON first, then check the status field.
        let body: serde_json::Value = response.json().await?;

        let status = body.get("status").and_then(|s| s.as_str()).unwrap_or("");

        match status {
            "authorized" => {
                let access_token = body
                    .get("access_token")
                    .and_then(|t| t.as_str())
                    .ok_or_else(|| anyhow::anyhow!("Missing access_token in authorized response"))?
                    .to_string();

                return Ok(TokenResponse {
                    access_token,
                    token_type: "Bearer".to_string(),
                    expires_in: body.get("expires_in").and_then(|v| v.as_u64()),
                    refresh_token: body
                        .get("refresh_token")
                        .and_then(|v| v.as_str())
                        .map(String::from),
                });
            }
            "pending" => {
                // Update interval if provided
                if let Some(new_interval) = body.get("interval").and_then(|v| v.as_u64()) {
                    poll_interval = Duration::from_secs(new_interval);
                }
                continue;
            }
            "expired" => {
                return Err(anyhow::anyhow!("Device code expired. Please try again."));
            }
            "denied" => {
                return Err(anyhow::anyhow!("Access denied by user."));
            }
            _ => {
                return Err(anyhow::anyhow!(
                    "Unexpected device login status: {}",
                    status
                ));
            }
        }
    }
}

/// Outcome of a single device-login poll (no sleeping, no looping), for
/// callers that are themselves polled by a host (MCP tool calls).
#[derive(Debug, Clone)]
pub enum DevicePollOutcome {
    Pending { interval: u64 },
    Authorized(TokenResponse),
    Expired,
    Denied,
}

/// Poll the device-login endpoint exactly once.
pub async fn poll_device_login_once(device_code: &str) -> Result<DevicePollOutcome> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()?;
    let url = format!("{}/api/v1/auth/device/token", api_url());

    let response = client
        .post(&url)
        .header("Content-Type", "application/json")
        .json(&serde_json::json!({ "device_code": device_code }))
        .send()
        .await?;

    let status_code = response.status();
    if status_code.as_u16() == 404 || status_code.as_u16() == 410 {
        return Ok(DevicePollOutcome::Expired);
    }
    if !status_code.is_success() {
        let error_text = response.text().await.unwrap_or_default();
        return Err(anyhow::anyhow!("Device login poll failed: {}", error_text));
    }

    let body: serde_json::Value = response.json().await?;
    let status = body.get("status").and_then(|s| s.as_str()).unwrap_or("");
    match status {
        "authorized" => {
            let access_token = body
                .get("access_token")
                .and_then(|t| t.as_str())
                .ok_or_else(|| anyhow::anyhow!("Missing access_token in authorized response"))?
                .to_string();
            Ok(DevicePollOutcome::Authorized(TokenResponse {
                access_token,
                token_type: "Bearer".to_string(),
                expires_in: body.get("expires_in").and_then(|v| v.as_u64()),
                refresh_token: body
                    .get("refresh_token")
                    .and_then(|v| v.as_str())
                    .map(String::from),
            }))
        }
        "pending" => Ok(DevicePollOutcome::Pending {
            interval: body
                .get("interval")
                .and_then(|v| v.as_u64())
                .unwrap_or_else(default_interval),
        }),
        "expired" => Ok(DevicePollOutcome::Expired),
        "denied" => Ok(DevicePollOutcome::Denied),
        other => Err(anyhow::anyhow!("Unexpected device login status: {}", other)),
    }
}

// ---------------------------------------------------------------------------
// Inline email + SMS signup (server family: /api/v1/auth/mcp-signup/*)
// ---------------------------------------------------------------------------

/// Public signup requirements published by the server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignupConfig {
    #[serde(default)]
    pub phone_required: bool,
    #[serde(default)]
    pub inline_signup_available: bool,
    #[serde(default)]
    pub sms_consent_text: String,
    #[serde(default)]
    pub consent_version: String,
    #[serde(default)]
    pub email_first_passwordless: bool,
}

/// Error surfaced by the signup endpoints, with the server's error code so
/// callers can branch (account_exists, verification_locked, ...).
#[derive(Debug, Clone)]
pub struct SignupApiError {
    pub status: u16,
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for SignupApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

impl std::error::Error for SignupApiError {}

#[derive(Debug, Clone, Deserialize)]
pub struct SignupStartResponse {
    pub attempt_id: String,
    pub client_secret: String,
    pub email: String,
    #[serde(default)]
    pub email_code_expires_in: i64,
    #[serde(default)]
    pub resend_after: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SignupStepResponse {
    pub status: String,
    #[serde(default)]
    pub next: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SignupConsentResponse {
    pub status: String,
    pub phone_last4: String,
    #[serde(default)]
    pub line_type: String,
    #[serde(default)]
    pub country_code: Option<String>,
    pub consent_text: String,
    pub consent_version: String,
    pub consent_token: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SignupSmsSentResponse {
    pub status: String,
    pub phone_last4: String,
    #[serde(default)]
    pub code_expires_in: Option<i64>,
    #[serde(default)]
    pub resend_after: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SignupResendResponse {
    pub status: String,
    #[serde(default)]
    pub code_expires_in: Option<i64>,
    #[serde(default)]
    pub resend_after: i64,
    #[serde(default)]
    pub sends_remaining: i16,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SignupApiKey {
    pub id: String,
    pub secret: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SignupCompleteResponse {
    pub status: String,
    pub user_id: String,
    pub email: String,
    pub api_key: SignupApiKey,
    pub api_url: String,
    #[serde(default)]
    pub dashboard_url: String,
}

fn signup_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .timeout(Duration::from_secs(25))
        .build()?)
}

async fn signup_post<T: for<'de> Deserialize<'de>>(
    path: &str,
    body: serde_json::Value,
) -> Result<T> {
    let url = format!("{}/api/v1/auth/mcp-signup/{}", api_url(), path);
    let response = signup_client()?
        .post(&url)
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await?;
    let status = response.status().as_u16();
    let text = response.text().await.unwrap_or_default();
    if !(200..300).contains(&status) {
        let parsed: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        return Err(SignupApiError {
            status,
            code: parsed
                .get("code")
                .and_then(|c| c.as_str())
                .unwrap_or("error")
                .to_string(),
            message: parsed
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or(if text.is_empty() { "request failed" } else { text.as_str() })
                .to_string(),
        }
        .into());
    }
    Ok(serde_json::from_str(&text)?)
}

/// `GET /api/v1/auth/signup-config`.
pub async fn fetch_signup_config() -> Result<SignupConfig> {
    let url = format!("{}/api/v1/auth/signup-config", api_url());
    let response = signup_client()?.get(&url).send().await?;
    if !response.status().is_success() {
        return Err(anyhow::anyhow!(
            "signup-config request failed: {}",
            response.status()
        ));
    }
    Ok(response.json().await?)
}

pub async fn start_email_signup(
    email: &str,
    full_name: Option<&str>,
    client: serde_json::Value,
) -> Result<SignupStartResponse> {
    signup_post(
        "start",
        serde_json::json!({
            "email": email,
            "full_name": full_name,
            "signup_source": "mcp_inline",
            "client": client,
        }),
    )
    .await
}

pub async fn verify_signup_email(
    attempt_id: &str,
    client_secret: &str,
    code: &str,
) -> Result<SignupStepResponse> {
    signup_post(
        "verify-email",
        serde_json::json!({ "attempt_id": attempt_id, "client_secret": client_secret, "code": code }),
    )
    .await
}

pub async fn resend_signup_email(attempt_id: &str, client_secret: &str) -> Result<SignupResendResponse> {
    signup_post(
        "resend-email",
        serde_json::json!({ "attempt_id": attempt_id, "client_secret": client_secret }),
    )
    .await
}

pub async fn request_sms_consent(
    attempt_id: &str,
    client_secret: &str,
    phone: &str,
) -> Result<SignupConsentResponse> {
    signup_post(
        "request-sms-consent",
        serde_json::json!({ "attempt_id": attempt_id, "client_secret": client_secret, "phone": phone }),
    )
    .await
}

pub async fn add_signup_phone(
    attempt_id: &str,
    client_secret: &str,
    consent_token: &str,
    surface: &str,
    user_response: Option<&str>,
) -> Result<SignupSmsSentResponse> {
    signup_post(
        "add-phone",
        serde_json::json!({
            "attempt_id": attempt_id,
            "client_secret": client_secret,
            "consent_token": consent_token,
            "surface": surface,
            "user_response": user_response,
        }),
    )
    .await
}

pub async fn resend_signup_sms(attempt_id: &str, client_secret: &str) -> Result<SignupResendResponse> {
    signup_post(
        "resend-sms",
        serde_json::json!({ "attempt_id": attempt_id, "client_secret": client_secret }),
    )
    .await
}

pub async fn verify_signup_phone(
    attempt_id: &str,
    client_secret: &str,
    code: &str,
) -> Result<SignupCompleteResponse> {
    signup_post(
        "verify-phone",
        serde_json::json!({ "attempt_id": attempt_id, "client_secret": client_secret, "code": code }),
    )
    .await
}

pub async fn fetch_signup_credentials(attempt_id: &str, client_secret: &str) -> Result<SignupCompleteResponse> {
    signup_post(
        "credentials",
        serde_json::json!({ "attempt_id": attempt_id, "client_secret": client_secret }),
    )
    .await
}

pub async fn ack_signup_credentials(attempt_id: &str, client_secret: &str) -> Result<SignupStepResponse> {
    signup_post(
        "ack",
        serde_json::json!({ "attempt_id": attempt_id, "client_secret": client_secret }),
    )
    .await
}

pub async fn cancel_signup(attempt_id: &str, client_secret: &str) -> Result<SignupStepResponse> {
    signup_post(
        "cancel",
        serde_json::json!({ "attempt_id": attempt_id, "client_secret": client_secret }),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_interval() {
        assert_eq!(default_interval(), 5);
    }

    #[test]
    fn test_default_expires_in() {
        assert_eq!(default_expires_in(), 600);
    }
}
