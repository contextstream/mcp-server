//! Dashboard-initiated exact-checkout sync-bridge enrollment.

use anyhow::{anyhow, Context, Result};
use console::style;
use mcp_client::ContextStreamClient;
use std::path::PathBuf;
use std::time::Duration;
use uuid::Uuid;

const TOKEN_ENV: &str = "CONTEXTSTREAM_BRIDGE_TOKEN";
const READY_WAIT: Duration = Duration::from_secs(120);
const POLL_INTERVAL: Duration = Duration::from_secs(2);

fn resolve_token(explicit: Option<String>) -> Result<String> {
    let token = explicit
        .or_else(|| std::env::var(TOKEN_ENV).ok())
        .map(|token| token.trim().to_string())
        .filter(|token| token.starts_with("csbe_") && token.len() >= 45)
        .ok_or_else(|| {
            anyhow!(
                "Missing or invalid bridge connection token. Copy a fresh command from the project health panel."
            )
        })?;
    Ok(token)
}

fn resolve_project_path(path: Option<String>) -> Result<PathBuf> {
    let requested = path.map(PathBuf::from).unwrap_or(std::env::current_dir()?);
    let canonical = std::fs::canonicalize(&requested).with_context(|| {
        format!(
            "Could not resolve the selected project folder {}",
            requested.display()
        )
    })?;
    if !canonical.is_dir() {
        anyhow::bail!("The selected project path must be a directory");
    }
    mcp_client::validate_ingest_root(&canonical, &mcp_client::IngestRootOptions::from_env())
        .map_err(|rejection| anyhow!(rejection.message().to_string()))?;
    Ok(canonical)
}

/// Connect the exact local checkout selected by `path` to the project-scoped
/// enrollment minted in the dashboard.
pub async fn run_connect(token: Option<String>, path: Option<String>) -> Result<()> {
    let token = resolve_token(token)?;
    let project_path = resolve_project_path(path)?;
    let config = crate::config::load_config().map_err(|_| {
        anyhow!(
            "This machine is not signed in. Run the setup command shown in the dashboard, then run the connection command again."
        )
    })?;
    if config.api_key.as_deref().is_none_or(str::is_empty)
        && config.jwt.as_deref().is_none_or(str::is_empty)
    {
        anyhow::bail!(
            "This machine is not signed in. Run the setup command shown in the dashboard, then run the connection command again."
        );
    }
    let client = ContextStreamClient::new(config);

    eprintln!(
        "{} Connecting {} to ContextStream…",
        style("⬡").cyan(),
        style(
            project_path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("project")
        )
        .cyan()
    );

    let enrollment = client
        .redeem_sync_bridge_enrollment(&token)
        .await
        .context("The connection link could not be redeemed")?;

    if let Some(expected) = enrollment.expected_repository_identity.as_deref() {
        let observed =
            ContextStreamClient::local_repository_identity(project_path.to_string_lossy().as_ref());
        if observed.as_deref() != Some(expected) {
            anyhow::bail!(
                "This folder is a different repository than '{}'. Open the intended checkout and run the command there; no local binding was changed.",
                enrollment.project_name
            );
        }
    }

    mcp_session::auto_init::establish_folder_binding(
        project_path.to_string_lossy().as_ref(),
        enrollment.workspace_id,
        &enrollment.workspace_name,
        Some(enrollment.project_id),
        Some(&enrollment.project_name),
    )
    .await
    .map_err(|error| anyhow!("Could not establish the trusted checkout identity: {error}"))?;

    let checkout = ContextStreamClient::sync_bridge_checkout_registration(
        project_path.to_string_lossy().as_ref(),
        enrollment.project_id,
        enrollment.workspace_id,
    )
    .context("Could not verify the checkout after binding")?;
    let bridge_instance_id = Uuid::new_v4();
    client
        .activate_sync_bridge_enrollment(enrollment.enrollment_id, bridge_instance_id, &checkout)
        .await
        .context("The server rejected the checkout binding")?;

    // Publish immediate path-free evidence while the supervised singleton is
    // starting. Readiness still requires that singleton to claim and complete
    // the initial refresh request.
    client
        .sync_bridge_heartbeat(
            bridge_instance_id,
            std::slice::from_ref(&checkout),
            "starting",
        )
        .await
        .context("Could not register the local sync bridge")?;

    match crate::setup::register_managed_sync_bridge() {
        Ok(registration) => {
            tracing::debug!(
                state = ?registration.state,
                "sync bridge service registration checked"
            );
        }
        Err(error) => {
            // The detached singleton remains a supported recovery path on
            // platforms without a login-service manager. Do not claim success
            // until the server observes its completed refresh below.
            tracing::debug!("could not register persistent sync bridge service: {error}");
        }
    }
    let reloaded = crate::watch::request_sync_bridge_reload().unwrap_or(false);
    if !reloaded {
        crate::watch::spawn_watch_helper();
    }

    eprintln!(
        "  {} Checkout identity verified; waiting for the first managed sync…",
        style("✓").green()
    );
    let deadline = tokio::time::Instant::now() + READY_WAIT;
    let mut last_status = String::new();
    loop {
        let status = client
            .sync_bridge_enrollment_status(enrollment.project_id, enrollment.enrollment_id)
            .await
            .context("Could not read sync-bridge progress")?;
        if status.status != last_status {
            let message = match status.status.as_str() {
                "redeemed" => "Validating the selected checkout…",
                "verifying" => "Starting the managed watcher…",
                "syncing" => "Uploading and indexing the initial snapshot…",
                "ready" => "Local project connected and initial sync verified.",
                "failed" => status
                    .detail
                    .as_deref()
                    .unwrap_or("The initial sync failed."),
                "expired" => "The connection link expired.",
                "superseded" => "A newer connection link replaced this one.",
                "canceled" => "The connection was canceled in the dashboard.",
                _ => "Waiting for the managed watcher…",
            };
            eprintln!("  {} {}", style("•").dim(), message);
            last_status = status.status.clone();
        }
        match status.status.as_str() {
            "ready" => {
                eprintln!(
                    "{} {} is connected. Future edits will sync automatically.",
                    style("✓").green(),
                    style(&enrollment.project_name).bold()
                );
                return Ok(());
            }
            "failed" => anyhow::bail!(
                "The helper connected, but the initial sync did not complete. Retry from the dashboard or run `contextstream-mcp doctor --only-configured`."
            ),
            "expired" | "superseded" | "canceled" => anyhow::bail!(
                "This connection link is no longer active. Generate a fresh link in the dashboard."
            ),
            _ => {}
        }
        if tokio::time::Instant::now() >= deadline {
            let health = crate::watch::sync_bridge_health();
            if matches!(health.state, crate::watch::SyncBridgeHealthState::Running)
                && health.target_count > 0
            {
                eprintln!(
                    "  {} The watcher is running; the initial server-side index is still finishing. The dashboard will update automatically.",
                    style("ℹ").blue()
                );
                return Ok(());
            }
            anyhow::bail!(
                "The managed watcher did not become healthy. Run `contextstream-mcp doctor --only-configured --repair --scope=all`, then retry the connection."
            );
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_validation_never_accepts_unscoped_secrets() {
        assert!(resolve_token(Some("not-a-token".to_string())).is_err());
        assert!(resolve_token(Some(format!("csbe_{}", "a".repeat(43)))).is_ok());
    }
}
