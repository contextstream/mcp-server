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
    let path = pending_connection_path();
    if !path.try_exists()? {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}", path.display()))?;
    let pending: PendingConnection = match serde_json::from_str(&content) {
        Ok(p) => p,
        Err(_) => {
            // Corrupt or foreign file: discard rather than guess.
            let _ = std::fs::remove_file(&path);
            return Ok(None);
        }
    };
    if pending.is_expired() {
        let _ = std::fs::remove_file(&path);
        return Ok(None);
    }
    Ok(Some(pending))
}

pub fn write_pending_connection(pending: &PendingConnection) -> Result<()> {
    let path = pending_connection_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let tmp = path.with_extension("json.tmp");
    let content = serde_json::to_string_pretty(pending)?;
    {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&tmp)
            .with_context(|| format!("writing {}", tmp.display()))?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, &path)
        .with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
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
        std::env::set_var("HOME", dir.path());
        let mut pending = PendingConnection::new("att-1".into(), "secret-xyz".into(), "a@b.c".into());
        pending.stage = PendingStage::SmsSent;
        pending.phone_last4 = Some("1234".into());
        write_pending_connection(&pending).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(pending_connection_path()).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let read = read_pending_connection().unwrap().unwrap();
        assert_eq!(read.attempt_id, "att-1");
        assert_eq!(read.stage, PendingStage::SmsSent);
        clear_pending_connection().unwrap();
        assert!(read_pending_connection().unwrap().is_none());
    }

    #[test]
    fn expired_records_are_discarded_on_read() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", dir.path());
        let mut pending = PendingConnection::new("att-2".into(), "secret".into(), "a@b.c".into());
        pending.expires_at = Utc::now() - Duration::minutes(1);
        write_pending_connection(&pending).unwrap();
        assert!(read_pending_connection().unwrap().is_none());
        assert!(!pending_connection_path().exists());
    }
}
