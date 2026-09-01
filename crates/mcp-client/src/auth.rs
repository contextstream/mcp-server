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
