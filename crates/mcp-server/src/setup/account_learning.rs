//! Human consent stays in the authenticated dashboard. CLI automation can
//! inspect the current choice and withdraw, but cannot grant consent.
use anyhow::Result;
use mcp_client::ContextStreamClient;

pub const PRIVACY_URL: &str = "https://contextstream.io/dashboard/account/privacy";

pub fn status_label(value: &serde_json::Value) -> &'static str {
    match value.get("enabled").and_then(serde_json::Value::as_bool) {
        Some(true) => "On",
        Some(false) => "Off",
        None => "Unavailable",
    }
}

pub async fn configure(enabled: bool) -> Result<()> {
    if super::safe_edit::is_dry_run() {
        println!(
            "Account learning: would {}",
            if enabled {
                "open the consent page"
            } else {
                "withdraw consent"
            }
        );
        return Ok(());
    }
    if enabled {
        println!("Account learning needs your separate consent in a signed-in browser.");
        println!("Read the current terms and tick the unchecked box at {PRIVACY_URL}");
        println!("Your choice has not changed. Agents and API keys cannot consent for you.");
        let _ = open::that(PRIVACY_URL);
        return Ok(());
    }
    let client = ContextStreamClient::new(super::doctor::doctor_client_config()?);
    let value = client.withdraw_account_learning().await?;
    anyhow::ensure!(
        status_label(&value) == "Off",
        "Withdrawal was not confirmed; retry or use Account → Privacy."
    );
    println!("Account learning is off. Collection has stopped; learned-data deletion is queued.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_or_missing_status_is_never_reported_as_off() {
        for value in [
            serde_json::json!({}),
            serde_json::json!({"enabled":"true"}),
            serde_json::json!({"enabled":null}),
        ] {
            assert_eq!(status_label(&value), "Unavailable");
        }
        assert_eq!(status_label(&serde_json::json!({"enabled":true})), "On");
        assert_eq!(status_label(&serde_json::json!({"enabled":false})), "Off");
    }
}
