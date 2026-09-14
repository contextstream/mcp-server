//! Durable record of an in-flight inline signup so a restarted MCP process
//! can finish (or safely abandon) it. Written with owner-only permissions;
//! removed once credentials are saved or the attempt is cancelled.

use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use super::credentials::contextstream_config_dir;

/// Server-side redelivery window; the record is useless after it.
pub const PENDING_CONNECTION_TTL_MINUTES: i64 = 15;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PendingStage {
    EmailSent,
    EmailVerified,
    ConsentPending,
    SmsSent,
    Finalized,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingConnection {
    pub attempt_id: String,
    /// Never surfaced in tool results; only this process and the server use it.
    pub client_secret: String,
    pub email: String,
    pub stage: PendingStage,
    #[serde(default)]
    pub phone_last4: Option<String>,
    #[serde(default)]
    pub consent_token: Option<String>,
    #[serde(default)]
    pub consent_text: Option<String>,
    pub started_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

impl PendingConnection {
    pub fn new(attempt_id: String, client_secret: String, email: String) -> Self {
        let now = Utc::now();
        Self {
            attempt_id,
            client_secret,
            email,
            stage: PendingStage::EmailSent,
            phone_last4: None,
            consent_token: None,
            consent_text: None,
            started_at: now,
            expires_at: now + Duration::minutes(30),
        }
    }

    pub fn is_expired(&self) -> bool {
        self.expires_at <= Utc::now()
    }

    /// Once finalized, the record only needs to live for the redelivery window.
    pub fn mark_finalized(&mut self) {
        self.stage = PendingStage::Finalized;
        self.expires_at = Utc::now() + Duration::minutes(PENDING_CONNECTION_TTL_MINUTES);
    }
}

pub fn pending_connection_path() -> PathBuf {
    contextstream_config_dir().join("pending-connection.json")
}

pub fn read_pending_connection() -> Result<Option<PendingConnection>> {
    read_pending_connection_at(&pending_connection_path())
}

fn read_pending_connection_at(path: &std::path::Path) -> Result<Option<PendingConnection>> {
    if !path.try_exists()? {
        return Ok(None);
    }
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let pending: PendingConnection = match serde_json::from_str(&content) {
        Ok(p) => p,
        Err(_) => {
            // Corrupt or foreign file: discard rather than guess.
            let _ = std::fs::remove_file(path);
            return Ok(None);
        }
    };
    if pending.is_expired() {
        let _ = std::fs::remove_file(path);
        return Ok(None);
    }
    Ok(Some(pending))
}

pub fn write_pending_connection(pending: &PendingConnection) -> Result<()> {
    let path = pending_connection_path();
    write_pending_connection_at(&path, pending)
}

fn write_pending_connection_at(path: &std::path::Path, pending: &PendingConnection) -> Result<()> {
    let loaded = super::safe_edit::read_for_edit(path, super::safe_edit::JsonDialect::Strict)?;
    super::safe_edit::commit_private(path, &loaded, &serde_json::to_value(pending)?, &[])?;
    Ok(())
}

/// Shared by the chat tool and setup wizard. Never revoke server redelivery
/// until the private credential file has been committed successfully.
pub(crate) async fn save_and_ack_signup(
    pending: &PendingConnection,
    complete: &mcp_client::auth::SignupCompleteResponse,
) -> Result<bool> {
    let api_url = complete.api_url.trim_end_matches('/');
    let api_url_override = (!api_url.is_empty()
        && api_url != mcp_types::config::DEFAULT_API_URL.trim_end_matches('/'))
    .then_some(api_url);
    super::write_saved_credentials(&complete.api_key.secret, api_url_override)?;
    Ok(matches!(
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            mcp_client::auth::ack_signup_credentials(&pending.attempt_id, &pending.client_secret),
        )
        .await,
        Ok(Ok(_))
    ))
}

pub fn clear_pending_connection() -> Result<()> {
    let path = pending_connection_path();
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_a_private_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending-connection.json");
        let mut pending =
            PendingConnection::new("att-1".into(), "secret-xyz".into(), "a@b.c".into());
        pending.stage = PendingStage::SmsSent;
        pending.phone_last4 = Some("1234".into());
        write_pending_connection_at(&path, &pending).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let read = read_pending_connection_at(&path).unwrap().unwrap();
        assert_eq!(read.attempt_id, "att-1");
        assert_eq!(read.stage, PendingStage::SmsSent);
        std::fs::remove_file(&path).unwrap();
        assert!(read_pending_connection_at(&path).unwrap().is_none());
    }

    #[test]
    fn expired_records_are_discarded_on_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pending-connection.json");
        let mut pending = PendingConnection::new("att-2".into(), "secret".into(), "a@b.c".into());
        pending.expires_at = Utc::now() - Duration::minutes(1);
        write_pending_connection_at(&path, &pending).unwrap();
        assert!(read_pending_connection_at(&path).unwrap().is_none());
        assert!(!path.exists());
    }
}
