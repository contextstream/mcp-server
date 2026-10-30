//! Human consent stays in the authenticated dashboard. CLI automation can
//! inspect the current choice, open the dashboard privacy page for a person,
//! and withdraw, but can never grant consent.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use mcp_client::ContextStreamClient;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::safe_edit::{self, JsonDialect};

pub const PRIVACY_URL: &str = "https://contextstream.io/dashboard/account/privacy";
pub const ENABLE_COMMAND: &str = "contextstream-mcp configure --account-learning on";

/// Effective consent state combining the server's authoritative consent record
/// with per-account local wizard dismissal state under `~/.contextstream`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsentState {
    /// Active consent for the current terms (`enabled && !requires_reconsent`).
    On { version: Option<String> },
    /// Never accepted on the server and never answered in the local wizard.
    NeverAsked { version: Option<String> },
    /// User declined in the wizard (for this account, or for the current
    /// reconsent version) or previously withdrew consent.
    Declined { version: Option<String> },
    /// Previously consented, `consent.version` changed (`requires_reconsent`),
    /// and the user has not yet answered the wizard for the new version.
    ReconsentNeeded {
        accepted_version: Option<String>,
        current_version: Option<String>,
    },
    /// Server response or local preference file could not be read safely.
    Unreadable,
}

impl ConsentState {
    /// Whether interactive `setup` should ask about deep project learning.
    pub fn should_prompt_in_wizard(&self) -> bool {
        matches!(self, Self::NeverAsked { .. } | Self::ReconsentNeeded { .. })
    }

    pub fn is_reconsent(&self) -> bool {
        matches!(self, Self::ReconsentNeeded { .. })
    }

    pub fn current_version(&self) -> Option<&str> {
        match self {
            Self::On { version } | Self::NeverAsked { version } | Self::Declined { version } => {
                version.as_deref()
            }
            Self::ReconsentNeeded {
                current_version, ..
            } => current_version.as_deref(),
            Self::Unreadable => None,
        }
    }

    /// Subtle summary-footer hint for `print_setup_outcome`.
    pub fn summary_hint(&self) -> SummaryLearningHint {
        match self {
            Self::On { .. } => SummaryLearningHint::On,
            Self::Declined { .. } | Self::NeverAsked { .. } | Self::ReconsentNeeded { .. } => {
                SummaryLearningHint::Available
            }
            Self::Unreadable => SummaryLearningHint::None,
        }
    }
}

/// Quiet one-line deep-learning status rendered in the setup summary footer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SummaryLearningHint {
    None,
    On,
    Available,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct LocalLearningStore {
    #[serde(default)]
    accounts: BTreeMap<String, LocalAccountLearningPref>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct LocalAccountLearningPref {
    #[serde(skip_serializing_if = "Option::is_none")]
    email: Option<String>,
    #[serde(default)]
    declined: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    declined_version: Option<String>,
    #[serde(default)]
    declined_reconsent_without_version: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    updated_at: Option<String>,
}

#[derive(Debug, Clone)]
struct ParsedServerConsent {
    enabled: bool,
    requires_reconsent: bool,
    accepted_version: Option<String>,
    accepted_at: Option<String>,
    current_version: Option<String>,
}

fn nonempty_str_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

fn parse_server_consent(value: &Value) -> Option<ParsedServerConsent> {
    let enabled = value.get("enabled").and_then(Value::as_bool)?;
    let accepted_version = nonempty_str_field(value, "accepted_version");
    let accepted_at = nonempty_str_field(value, "accepted_at");
    let current_version = value
        .get("consent")
        .and_then(|consent| nonempty_str_field(consent, "version"));
    let explicit_reconsent = value
        .get("requires_reconsent")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let version_mismatch = match (&accepted_version, &current_version) {
        (Some(accepted), Some(current)) => accepted != current,
        _ => false,
    };
    let requires_reconsent = explicit_reconsent || (enabled && version_mismatch);

    Some(ParsedServerConsent {
        enabled,
        requires_reconsent,
        accepted_version,
        accepted_at,
        current_version,
    })
}

pub fn status_label(value: &Value) -> &'static str {
    match parse_server_consent(value) {
        Some(parsed) if parsed.enabled && parsed.requires_reconsent => "Needs review",
        Some(parsed) if parsed.enabled => "On",
        Some(_) => "Off",
        None => "Unavailable",
    }
}

pub fn preferences_file_path() -> PathBuf {
    super::contextstream_config_dir().join("account-learning.json")
}

fn normalize_account_token(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_ascii_lowercase())
}

fn account_key(user_id: Option<&str>, email: Option<&str>) -> Option<String> {
    if let Some(uid) = normalize_account_token(user_id) {
        return Some(uid);
    }
    normalize_account_token(email).map(|mail| format!("email:{mail}"))
}

fn read_account_preference_at(
    path: &Path,
    user_id: Option<&str>,
    email: Option<&str>,
) -> Result<Option<LocalAccountLearningPref>> {
    let loaded = safe_edit::read_for_edit(path, JsonDialect::Strict)?;
    if !loaded.existed {
        return Ok(None);
    }
    if loaded.nonstandard_syntax {
        anyhow::bail!(
            "Account learning preferences at {} must be strict JSON",
            path.display()
        );
    }
    let store: LocalLearningStore = serde_json::from_value(loaded.value).with_context(|| {
        format!(
            "Could not parse account learning preferences at {}",
            path.display()
        )
    })?;

    if let Some(uid) = normalize_account_token(user_id) {
        if let Some(pref) = store.accounts.get(&uid) {
            return Ok(Some(pref.clone()));
        }
    }
    if let Some(mail) = normalize_account_token(email) {
        if let Some(pref) = store.accounts.get(&format!("email:{mail}")) {
            return Ok(Some(pref.clone()));
        }
        if let Some(pref) = store.accounts.values().find(|entry| {
            entry
                .email
                .as_deref()
                .and_then(|e| normalize_account_token(Some(e)))
                .as_deref()
                == Some(mail.as_str())
        }) {
            return Ok(Some(pref.clone()));
        }
    }
    Ok(None)
}

/// Record that `user_id` / `email` declined the deep-learning prompt in setup
/// (or withdrew via CLI) so later setup and update runs stay quiet.
pub fn record_declined(
    user_id: Option<&str>,
    email: Option<&str>,
    version: Option<&str>,
    is_reconsent: bool,
) -> Result<()> {
    record_declined_at(
        &preferences_file_path(),
        user_id,
        email,
        version,
        is_reconsent,
    )
}

fn record_declined_at(
    path: &Path,
    user_id: Option<&str>,
    email: Option<&str>,
    version: Option<&str>,
    is_reconsent: bool,
) -> Result<()> {
    let Some(key) = account_key(user_id, email) else {
        return Ok(());
    };
    let loaded = safe_edit::read_for_edit(path, JsonDialect::Strict)?;
    if loaded.nonstandard_syntax {
        anyhow::bail!(
            "Refusing to modify {}: account learning preferences must be strict JSON",
            path.display()
        );
    }
    let mut store: LocalLearningStore = if loaded.existed {
        serde_json::from_value(loaded.value.clone()).with_context(|| {
            format!(
                "Could not parse account learning preferences at {}",
                path.display()
            )
        })?
    } else {
        LocalLearningStore::default()
    };

    if let Some(mail) = normalize_account_token(email) {
        let legacy_email_key = format!("email:{mail}");
        if legacy_email_key != key {
            store.accounts.remove(&legacy_email_key);
        }
    }

    let previous = store.accounts.get(&key).cloned().unwrap_or_default();
    let normalized_version = version
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned);
    let updated_entry = LocalAccountLearningPref {
        email: normalize_account_token(email).or(previous.email),
        declined: if is_reconsent {
            previous.declined
        } else {
            true
        },
        declined_version: normalized_version.clone().or(previous.declined_version),
        declined_reconsent_without_version: is_reconsent && normalized_version.is_none(),
        updated_at: Some(chrono::Utc::now().to_rfc3339()),
    };
    store.accounts.insert(key, updated_entry);

    let serialized = json!({
        "accounts": store.accounts,
    });
    safe_edit::commit_private(path, &loaded, &serialized, &[]).map(|_| ())
}

/// Evaluate consent state from a `/users/me/learning-consent` payload and the
/// local preference store at `prefs_path`.
pub fn resolve_consent_state_at(
    value: &Value,
    prefs_path: &Path,
    user_id: Option<&str>,
    email: Option<&str>,
) -> ConsentState {
    let Some(server) = parse_server_consent(value) else {
        return ConsentState::Unreadable;
    };

    // Active current consent on the server is authoritative even if a user had
    // declined an earlier wizard prompt before enabling it in their browser.
    if server.enabled && !server.requires_reconsent {
        return ConsentState::On {
            version: server.accepted_version.or(server.current_version),
        };
    }

    let local_pref = match read_account_preference_at(prefs_path, user_id, email) {
        Ok(pref) => pref,
        Err(error) => {
            tracing::debug!(%error, "could not read local account-learning preferences");
            return ConsentState::Unreadable;
        }
    };

    if server.requires_reconsent {
        let already_declined_this_version = local_pref.as_ref().is_some_and(|pref| {
            match (
                pref.declined_version.as_deref(),
                server.current_version.as_deref(),
            ) {
                (Some(declined_ver), Some(current_ver)) => declined_ver == current_ver,
                (_, None) => pref.declined_reconsent_without_version,
                _ => false,
            }
        });
        if already_declined_this_version {
            return ConsentState::Declined {
                version: server.current_version,
            };
        }
        return ConsentState::ReconsentNeeded {
            accepted_version: server.accepted_version,
            current_version: server.current_version,
        };
    }

    // `!server.enabled && !server.requires_reconsent`
    if local_pref.as_ref().is_some_and(|pref| pref.declined)
        || server.accepted_version.is_some()
        || server.accepted_at.is_some()
    {
        return ConsentState::Declined {
            version: server.current_version,
        };
    }

    ConsentState::NeverAsked {
        version: server.current_version,
    }
}

pub fn resolve_consent_state(
    value: &Value,
    user_id: Option<&str>,
    email: Option<&str>,
) -> ConsentState {
    resolve_consent_state_at(value, &preferences_file_path(), user_id, email)
}

/// Fetch `/users/me/learning-consent` and resolve [`ConsentState`] for setup.
pub async fn fetch_consent_state(
    client: &ContextStreamClient,
    user_id: Option<&str>,
    email: Option<&str>,
) -> ConsentState {
    match tokio::time::timeout(Duration::from_secs(5), client.account_learning_status()).await {
        Ok(Ok(value)) => resolve_consent_state(&value, user_id, email),
        Ok(Err(error)) => {
            tracing::debug!(%error, "could not load account learning consent status");
            ConsentState::Unreadable
        }
        Err(_) => {
            tracing::debug!("account learning consent status request timed out");
            ConsentState::Unreadable
        }
    }
}

/// Whether this environment can launch a local GUI browser.
pub fn should_open_browser() -> bool {
    should_open_browser_with(|name| std::env::var_os(name).is_some_and(|v| !v.is_empty()))
}

fn should_open_browser_with<F>(var_is_set: F) -> bool
where
    F: Fn(&str) -> bool,
{
    if var_is_set("CI") || var_is_set("CONTEXTSTREAM_NO_BROWSER") {
        return false;
    }
    if cfg!(target_os = "linux") {
        return var_is_set("DISPLAY") || var_is_set("WAYLAND_DISPLAY");
    }
    true
}

fn open_privacy_page_if_supported() -> bool {
    should_open_browser() && open::that(PRIVACY_URL).is_ok()
}

/// Print the human-only browser consent instructions and open the privacy page
/// when a browser is available (otherwise just print the URL).
pub fn open_browser_consent_prompt() {
    println!("Deep project learning needs your separate consent in a signed-in browser.");
    println!("Read the current terms and tick the unchecked box at {PRIVACY_URL}");
    println!("Your choice has not changed. Agents and API keys cannot consent for you.");
    let _ = open_privacy_page_if_supported();
}

/// The status when the consent state could not be read: a refused key says so
/// and how to fix it, and an unreachable server is named. Both read the same
/// before (#343).
fn unverified_status(failure: Option<&mcp_types::Error>, api_url: &str) -> String {
    let reason = match failure {
        Some(error @ mcp_types::Error::Http { status: 401, .. }) => error.user_facing_message(),
        Some(error) => error.reach_message(api_url),
        None => "The answer could not be read.".to_string(),
    };
    format!(
        "Deep project learning: unavailable (could not verify current consent state)
{reason}
Privacy page: {PRIVACY_URL}"
    )
}

/// Inspect and print the current deep project learning consent state without
/// mutating anything or opening a browser (`configure --account-learning [status]`).
pub async fn show_status() -> Result<()> {
    let config = match super::doctor::doctor_client_config() {
        Ok(config) => config,
        Err(_) => {
            println!("Deep project learning: unavailable (not signed in)");
            println!("Run `contextstream-mcp setup` first, or visit {PRIVACY_URL}");
            return Ok(());
        }
    };
    let api_url = config.api_url.clone();
    let client = ContextStreamClient::new(config);
    let (status, failure) = match tokio::time::timeout(
        Duration::from_secs(5),
        client.account_learning_status(),
    )
    .await
    {
        Ok(Ok(status)) => (Some(status), None),
        Ok(Err(error)) => (None, Some(error)),
        Err(_) => (None, Some(mcp_types::Error::Timeout(5))),
    };

    match status.as_ref().and_then(parse_server_consent) {
        Some(parsed) if parsed.enabled && !parsed.requires_reconsent => {
            println!("Deep project learning: on");
            println!("Manage or withdraw anytime at {PRIVACY_URL} or with `contextstream-mcp configure --account-learning off`.");
        }
        Some(parsed) if parsed.requires_reconsent => {
            println!("Deep project learning: paused (updated terms need your review)");
            println!("Review and confirm in a signed-in browser: `{ENABLE_COMMAND}`");
            println!("Privacy page: {PRIVACY_URL}");
        }
        Some(_) => {
            println!("Deep project learning: off");
            println!("Turn on in a signed-in browser: `{ENABLE_COMMAND}`");
            println!("Privacy page: {PRIVACY_URL}");
        }
        None => println!("{}", unverified_status(failure.as_ref(), &api_url)),
    }
    Ok(())
}

pub async fn configure(enabled: bool) -> Result<()> {
    if safe_edit::is_dry_run() {
        println!(
            "Deep project learning: would {}",
            if enabled {
                "open the consent page (unless already enabled)"
            } else {
                "withdraw consent"
            }
        );
        return Ok(());
    }
    if enabled {
        if let Ok(config) = super::doctor::doctor_client_config() {
            let client = ContextStreamClient::new(config);
            if let Ok(Ok(status)) =
                tokio::time::timeout(Duration::from_secs(5), client.account_learning_status()).await
            {
                if let Some(parsed) = parse_server_consent(&status) {
                    if parsed.enabled && !parsed.requires_reconsent {
                        println!("Deep project learning: on ({PRIVACY_URL})");
                        return Ok(());
                    }
                }
            }
        }
        open_browser_consent_prompt();
        return Ok(());
    }

    let client = ContextStreamClient::new(super::doctor::doctor_client_config()?);
    let me = tokio::time::timeout(Duration::from_secs(5), client.me())
        .await
        .ok()
        .and_then(Result::ok);
    let value = client.withdraw_account_learning().await?;
    anyhow::ensure!(
        status_label(&value) == "Off",
        "Withdrawal was not confirmed; retry or use Account → Privacy."
    );
    let current_version = value
        .get("consent")
        .and_then(|c| nonempty_str_field(c, "version"));
    if let Some(user) = me {
        let uid = user.id.to_string();
        if let Err(error) = record_declined(
            Some(&uid),
            Some(&user.email),
            current_version.as_deref(),
            false,
        ) {
            tracing::debug!(%error, "could not persist local account-learning decline after withdrawal");
        }
    }
    println!(
        "Deep project learning is off. Collection has stopped; learned-data deletion is queued."
    );
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
        assert_eq!(
            status_label(&serde_json::json!({"enabled":true,"requires_reconsent":true})),
            "Needs review"
        );
        assert_eq!(status_label(&serde_json::json!({"enabled":false})), "Off");
    }

    #[test]
    fn consent_state_on_when_enabled_and_current() {
        let dir = tempfile::tempdir().expect("tempdir");
        let prefs = dir.path().join("account-learning.json");

        // Even if the user previously declined in the wizard before enabling in
        // the browser, active server consent wins and never prompts again.
        record_declined_at(
            &prefs,
            Some("11111111-1111-1111-1111-111111111111"),
            Some("owner@contextstream.io"),
            Some("account-learning-v1"),
            false,
        )
        .expect("record prior decline");

        let server = json!({
            "enabled": true,
            "requires_reconsent": false,
            "accepted_version": "account-learning-v1",
            "accepted_at": "2026-04-01T12:00:00Z",
            "consent": { "version": "account-learning-v1" }
        });
        let state = resolve_consent_state_at(
            &server,
            &prefs,
            Some("11111111-1111-1111-1111-111111111111"),
            Some("owner@contextstream.io"),
        );
        assert_eq!(
            state,
            ConsentState::On {
                version: Some("account-learning-v1".to_string())
            }
        );
        assert!(!state.should_prompt_in_wizard());
        assert_eq!(state.summary_hint(), SummaryLearningHint::On);
    }

    #[test]
    fn consent_state_never_asked_for_first_time_account() {
        let dir = tempfile::tempdir().expect("tempdir");
        let prefs = dir.path().join("account-learning.json");
        let server = json!({
            "enabled": false,
            "requires_reconsent": false,
            "accepted_version": null,
            "accepted_at": null,
            "consent": { "version": "account-learning-v1" }
        });

        let state = resolve_consent_state_at(
            &server,
            &prefs,
            Some("11111111-1111-1111-1111-111111111111"),
            Some("first@contextstream.io"),
        );
        assert_eq!(
            state,
            ConsentState::NeverAsked {
                version: Some("account-learning-v1".to_string())
            }
        );
        assert!(state.should_prompt_in_wizard());
    }

    #[test]
    fn consent_state_declined_persists_per_account_without_silencing_other_accounts() {
        let dir = tempfile::tempdir().expect("tempdir");
        let prefs = dir.path().join("account-learning.json");
        let server = json!({
            "enabled": false,
            "requires_reconsent": false,
            "accepted_version": null,
            "accepted_at": null,
            "consent": { "version": "account-learning-v1" }
        });

        record_declined_at(
            &prefs,
            Some("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
            Some("alice@example.com"),
            Some("account-learning-v1"),
            false,
        )
        .expect("record decline for alice");

        let alice = resolve_consent_state_at(
            &server,
            &prefs,
            Some("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
            Some("alice@example.com"),
        );
        assert_eq!(
            alice,
            ConsentState::Declined {
                version: Some("account-learning-v1".to_string())
            }
        );
        assert!(!alice.should_prompt_in_wizard());
        assert_eq!(alice.summary_hint(), SummaryLearningHint::Available);

        // Even if server bumps consent.version later, an account that never
        // opted in and said "no" stays Declined rather than being nagged again.
        let server_v2 = json!({
            "enabled": false,
            "requires_reconsent": false,
            "accepted_version": null,
            "accepted_at": null,
            "consent": { "version": "account-learning-v2" }
        });
        let alice_v2 = resolve_consent_state_at(
            &server_v2,
            &prefs,
            Some("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa"),
            Some("alice@example.com"),
        );
        assert!(matches!(alice_v2, ConsentState::Declined { .. }));

        // Different account on the same machine still gets asked once.
        let bob = resolve_consent_state_at(
            &server,
            &prefs,
            Some("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb"),
            Some("bob@example.com"),
        );
        assert!(matches!(bob, ConsentState::NeverAsked { .. }));
        assert!(bob.should_prompt_in_wizard());
    }

    #[test]
    fn consent_state_reconsent_needed_asks_once_per_new_version_then_remembers_no() {
        let dir = tempfile::tempdir().expect("tempdir");
        let prefs = dir.path().join("account-learning.json");
        let uid = "11111111-1111-1111-1111-111111111111";

        let reconsent_v2 = json!({
            "enabled": true,
            "requires_reconsent": true,
            "accepted_version": "account-learning-v1",
            "accepted_at": "2026-01-10T00:00:00Z",
            "consent": { "version": "account-learning-v2" }
        });

        let first = resolve_consent_state_at(&reconsent_v2, &prefs, Some(uid), None);
        assert_eq!(
            first,
            ConsentState::ReconsentNeeded {
                accepted_version: Some("account-learning-v1".to_string()),
                current_version: Some("account-learning-v2".to_string()),
            }
        );
        assert!(first.should_prompt_in_wizard());
        assert!(first.is_reconsent());

        // Also detect version mismatch defensively if requires_reconsent was false.
        let mismatch_only = json!({
            "enabled": true,
            "requires_reconsent": false,
            "accepted_version": "account-learning-v1",
            "consent": { "version": "account-learning-v2" }
        });
        assert!(matches!(
            resolve_consent_state_at(&mismatch_only, &prefs, Some(uid), None),
            ConsentState::ReconsentNeeded { .. }
        ));

        // User declines v2 reconsent: never ask again for v2.
        record_declined_at(
            &prefs,
            Some(uid),
            Some("owner@contextstream.io"),
            Some("account-learning-v2"),
            true,
        )
        .expect("record v2 decline");

        let second = resolve_consent_state_at(&reconsent_v2, &prefs, Some(uid), None);
        assert_eq!(
            second,
            ConsentState::Declined {
                version: Some("account-learning-v2".to_string())
            }
        );
        assert!(!second.should_prompt_in_wizard());

        // When v3 ships later, ask once for v3.
        let reconsent_v3 = json!({
            "enabled": true,
            "requires_reconsent": true,
            "accepted_version": "account-learning-v1",
            "consent": { "version": "account-learning-v3" }
        });
        let third = resolve_consent_state_at(&reconsent_v3, &prefs, Some(uid), None);
        assert_eq!(
            third,
            ConsentState::ReconsentNeeded {
                accepted_version: Some("account-learning-v1".to_string()),
                current_version: Some("account-learning-v3".to_string()),
            }
        );
        assert!(third.should_prompt_in_wizard());
    }

    #[test]
    fn consent_state_unreadable_when_server_or_local_file_is_invalid() {
        let dir = tempfile::tempdir().expect("tempdir");
        let prefs = dir.path().join("account-learning.json");
        let uid = "11111111-1111-1111-1111-111111111111";

        for bad_server in [
            json!({}),
            json!({"enabled": "yes"}),
            json!({"error": "unauthorized"}),
        ] {
            let state = resolve_consent_state_at(&bad_server, &prefs, Some(uid), None);
            assert_eq!(state, ConsentState::Unreadable);
            assert!(!state.should_prompt_in_wizard());
            assert_eq!(state.summary_hint(), SummaryLearningHint::None);
        }

        // Corrupt local file fails closed as Unreadable when server is off/reconsent,
        // rather than re-prompting or overwriting the corrupt file.
        std::fs::write(&prefs, "{not valid json").expect("write corrupt prefs");
        let server_off = json!({
            "enabled": false,
            "requires_reconsent": false,
            "consent": { "version": "account-learning-v1" }
        });
        assert_eq!(
            resolve_consent_state_at(&server_off, &prefs, Some(uid), None),
            ConsentState::Unreadable
        );
        assert!(record_declined_at(&prefs, Some(uid), None, Some("v1"), false).is_err());
    }

    #[test]
    fn browser_open_check_honors_headless_and_opt_out_flags() {
        assert!(!should_open_browser_with(|k| k == "CI"));
        assert!(!should_open_browser_with(
            |k| k == "CONTEXTSTREAM_NO_BROWSER"
        ));
        if cfg!(target_os = "linux") {
            assert!(!should_open_browser_with(|_| false));
            assert!(should_open_browser_with(|k| k == "DISPLAY"));
            assert!(should_open_browser_with(|k| k == "WAYLAND_DISPLAY"));
        }
    }
}

#[cfg(test)]
mod unverified_status_tests {
    use super::*;

    // #343: a refused key read exactly like an offline run.
    #[test]
    fn a_refused_key_and_an_unreachable_server_each_say_what_happened() {
        let url = "http://127.0.0.1:9";
        let refused = unverified_status(Some(&mcp_types::Error::http(401, "Unauthorized")), url);
        assert!(refused.contains("did not accept the API key"), "{refused}");
        assert!(refused.contains("contextstream-mcp setup"), "{refused}");
        let offline = unverified_status(
            Some(&mcp_types::Error::Network(
                "Connection failed: refused".into(),
            )),
            url,
        );
        assert!(
            offline.contains("Could not reach ContextStream at http://127.0.0.1:9"),
            "{offline}"
        );
        assert_ne!(refused, offline);
        for status in [&refused, &offline] {
            assert!(
                status.starts_with("Deep project learning: unavailable"),
                "{status}"
            );
            assert!(status.ends_with(PRIVACY_URL), "{status}");
        }
    }
}
