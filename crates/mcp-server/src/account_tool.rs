//! `account` tool: connect ContextStream from inside the agent.
//!
//! Browser route (default): start the device-login flow, show the user the
//! link and code, poll once per call, then mint an API key with the JWT and
//! save it with `write_saved_credentials`.
//!
//! Inline route (explicit choice, only when the server offers it): email →
//! email code → phone → consent notice + the user's verbatim reply → SMS
//! code → the server creates the account and mints the key. The in-flight
//! attempt id and client secret live in this process and in a private
//! `pending-connection.json` so a restarted process can recover; they never
//! appear in tool results, and neither does the API key.
//!
//! Stdio only. The hosted HTTP gateway never registers this tool.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use mcp_client::auth::{
    self as client_auth, poll_device_login_once, start_device_login, DevicePollOutcome,
    SignupApiError, SignupCompleteResponse, SignupConfig,
};
use mcp_client::ContextStreamClient;
use mcp_tools::registry::ToolHandler;
use mcp_tools::schema::SchemaBuilder;
use mcp_types::{
    tool::{ToolAnnotations, ToolCategory, ToolMetadata, ToolResult},
    Config, Error, Result,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::setup::{
    clear_pending_connection, credentials_file_path, read_pending_connection,
    read_saved_credentials, write_pending_connection, write_saved_credentials, PendingConnection,
    PendingStage,
};

pub const ACCOUNT_TOOL_NAME: &str = "account";

/// Marker recognised by `mcp_tools::registry::tool_result_is_access_gate` so
/// runtime readiness never counts setup instructions as successful grounding.
pub const SETUP_REQUIRED_MARKER: &str = "[setup_required]";

const DEVICE_FLOW_MAX_AGE: Duration = Duration::from_secs(15 * 60);
const SIGNUP_CONFIG_CACHE_TTL: Duration = Duration::from_secs(60);

pub const RECONNECT_INSTRUCTIONS: &str = "Reconnect the ContextStream MCP server to enable all tools (Claude Code: run /mcp and choose reconnect; other editors: restart the MCP server or the editor).";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountToolMode {
    /// No credentials: the server exposes only `init` and `account`.
    Limited,
    /// Credentials present: `account` is available for status and repair.
    Full,
}

impl AccountToolMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Limited => "limited",
            Self::Full => "full",
        }
    }
}

/// Where the process got its API key from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialsSource {
    None,
    SavedFile,
    /// The editor config (env) supplies a key that differs from the saved
    /// file, so a key saved by this tool will not take effect until the
    /// editor config is updated.
    EnvironmentOverride,
}

pub fn detect_credentials_source() -> CredentialsSource {
    let env_key = std::env::var("CONTEXTSTREAM_API_KEY")
        .ok()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty());
    let saved_key = read_saved_credentials()
        .ok()
        .and_then(|c| c.api_key)
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty());
    match (env_key, saved_key) {
        (None, None) => CredentialsSource::None,
        (None, Some(_)) => CredentialsSource::SavedFile,
        (Some(env), Some(saved)) if env == saved => CredentialsSource::SavedFile,
        (Some(_), _) => CredentialsSource::EnvironmentOverride,
    }
}

/// Client metadata sent with an inline signup (never secrets).
pub fn client_metadata() -> Value {
    json!({
        "hostname": detect_hostname(),
        "os": std::env::consts::OS,
        "arch": std::env::consts::ARCH,
        "mcp_version": env!("CARGO_PKG_VERSION"),
    })
}

#[derive(Debug, Clone)]
struct BrowserFlow {
    device_code: String,
    user_code: String,
    verification_uri: String,
    interval: u64,
    started_at: Instant,
}

#[derive(Debug, Deserialize)]
struct AccountInput {
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    full_name: Option<String>,
    #[serde(default)]
    phone: Option<String>,
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    user_response: Option<String>,
    #[serde(default)]
    channel: Option<String>,
}

pub struct AccountTool {
    metadata: ToolMetadata,
    mode: AccountToolMode,
    api_url: String,
    flow: Arc<Mutex<Option<BrowserFlow>>>,
    inline: Arc<Mutex<Option<PendingConnection>>>,
    signup_config: Arc<Mutex<Option<(Instant, SignupConfig)>>>,
}

fn signup_error_message(error: &anyhow::Error) -> (String, String) {
    match error.downcast_ref::<SignupApiError>() {
        Some(api) => (api.code.clone(), api.message.clone()),
        None => ("error".to_string(), error.to_string()),
    }
}

fn looks_negative(reply: &str) -> bool {
    let lower = reply.trim().to_ascii_lowercase();
    lower.is_empty()
        || lower == "no"
        || lower.starts_with("no ")
        || lower.starts_with("no,")
        || lower.starts_with("nope")
        || lower.starts_with("stop")
        || lower.starts_with("cancel")
        || lower.starts_with("don't")
        || lower.starts_with("do not")
}

impl AccountTool {
    pub fn new(config: &Config, mode: AccountToolMode) -> Self {
        let mut annotations = ToolAnnotations::write().idempotent();
        annotations.requires_confirmation = true;
        // Recover an in-flight inline signup from a previous process.
        let inline = read_pending_connection().ok().flatten();
        Self {
            metadata: ToolMetadata {
                name: ACCOUNT_TOOL_NAME.to_string(),
                title: "Connect Account".to_string(),
                description: "Connect ContextStream to an account without handling an API key manually. \
                    Actions: status; connect_browser then connect_poll (browser sign-in or sign-up; show the user \
                    the link and code); signup_start (email) → signup_verify_email (code) → \
                    signup_request_sms_consent (phone) → signup_confirm_sms_consent (the user's verbatim reply to \
                    the notice) → signup_verify_phone (code) for the inline email route, only when status says it \
                    is available; resend (channel email|sms); fetch_credentials; cancel. Let the user choose the \
                    route first; never ask for an email or phone number unless they choose the inline route. \
                    Secrets are never returned in results."
                    .to_string(),
                category: ToolCategory::Session,
                annotations,
                is_pro: false,
                required_tier: None,
            },
            mode,
            api_url: config.api_url.trim_end_matches('/').to_string(),
            flow: Arc::new(Mutex::new(None)),
            inline: Arc::new(Mutex::new(inline)),
            signup_config: Arc::new(Mutex::new(None)),
        }
    }

    pub fn mode(&self) -> AccountToolMode {
        self.mode
    }

    fn dashboard_url(&self) -> String {
        std::env::var("CONTEXTSTREAM_APP_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "https://contextstream.io".to_string())
    }

    async fn inline_available(&self) -> bool {
        self.signup_config()
            .await
            .map(|c| c.inline_signup_available)
            .unwrap_or(false)
    }

    async fn signup_config(&self) -> Option<SignupConfig> {
        {
            let cached = self.signup_config.lock().await;
            if let Some((at, config)) = cached.as_ref() {
                if at.elapsed() < SIGNUP_CONFIG_CACHE_TTL {
                    return Some(config.clone());
                }
            }
        }
        match tokio::time::timeout(Duration::from_secs(8), client_auth::fetch_signup_config()).await
        {
            Ok(Ok(config)) => {
                *self.signup_config.lock().await = Some((Instant::now(), config.clone()));
                Some(config)
            }
            _ => None,
        }
    }

    async fn persist_inline(&self, pending: &PendingConnection) {
        if let Err(error) = write_pending_connection(pending) {
            tracing::warn!(error = %error, "could not persist pending connection");
        }
    }

    async fn clear_inline(&self) {
        *self.inline.lock().await = None;
        let _ = clear_pending_connection();
    }

    // ----------------------------------------------------------------------
    // status / cancel
    // ----------------------------------------------------------------------

    async fn status(&self) -> ToolResult {
        let source = detect_credentials_source();
        let flow = self.flow.lock().await.clone();
        let inline = self.inline.lock().await.clone();
        let inline_available = self.inline_available().await;
        let flow_summary = flow.as_ref().map(|f| {
            json!({
                "verification_uri": f.verification_uri,
                "user_code": f.user_code,
                "age_secs": f.started_at.elapsed().as_secs(),
            })
        });
        let inline_summary = inline.as_ref().map(|p| {
            json!({
                "email": p.email,
                "stage": p.stage,
                "phone_last4": p.phone_last4,
            })
        });
        let connected = source != CredentialsSource::None && self.mode == AccountToolMode::Full;
        let mut routes = vec!["browser"];
        if inline_available {
            routes.push("email");
        }
        let text = match (self.mode, source) {
            (AccountToolMode::Limited, _) => {
                let mut text = format!(
                    "{SETUP_REQUIRED_MARKER} ContextStream is installed but not connected to an account. \
                     Offer the user the routes and let them choose: (1) continue securely in the browser \
                     (account action=connect_browser)"
                );
                if inline_available {
                    text.push_str(
                        "; (2) create an account here with email + SMS verification (account action=signup_start).",
                    );
                } else {
                    text.push_str(". The inline email route is not available on this server.");
                }
                text.push_str(" Do not ask for an email or phone number unless they choose the inline route.");
                if flow.is_some() {
                    text.push_str(" A browser sign-in is already in progress; call account(action=connect_poll) after the user approves it.");
                }
                if let Some(p) = &inline {
                    text.push_str(&format!(
                        " An inline signup for {} is in progress at stage {:?}; continue with {}.",
                        p.email,
                        p.stage,
                        next_inline_action(&p.stage)
                    ));
                }
                text
            }
            (AccountToolMode::Full, CredentialsSource::EnvironmentOverride) => {
                "Connected using an API key supplied by the editor configuration (CONTEXTSTREAM_API_KEY). \
                 A key saved by this tool will not take effect until that configuration is updated; run \
                 `contextstream-mcp update-configs` after connecting, or remove the env entry."
                    .to_string()
            }
            (AccountToolMode::Full, _) => "Connected. Credentials are saved locally.".to_string(),
        };
        ToolResult::with_structured(
            text,
            json!({
                "status": if connected { "connected" } else { "setup_required" },
                "mode": self.mode.as_str(),
                "credentials_source": source,
                "credentials_path": credentials_file_path().display().to_string(),
                "api_url": self.api_url,
                "routes": routes,
                "inline_signup_available": inline_available,
                "browser_flow": flow_summary,
                "inline_signup": inline_summary,
                "next": if connected {
                    Value::Null
                } else if let Some(p) = &inline {
                    json!(next_inline_action(&p.stage))
                } else {
                    json!("account(action=\"connect_browser\")")
                },
            }),
        )
    }

    async fn cancel(&self) -> ToolResult {
        let had_flow = self.flow.lock().await.take().is_some();
        let inline = self.inline.lock().await.take();
        let had_inline = inline.is_some();
        if let Some(p) = inline {
            let _ = client_auth::cancel_signup(&p.attempt_id, &p.client_secret).await;
        }
        let _ = clear_pending_connection();
        ToolResult::with_structured(
            if had_flow || had_inline {
                "Cancelled the pending connection attempt."
            } else {
                "Nothing to cancel."
            },
            json!({ "status": "cancelled", "had_browser_flow": had_flow, "had_inline_signup": had_inline }),
        )
    }

    // ----------------------------------------------------------------------
    // Browser route
    // ----------------------------------------------------------------------

    async fn connect_browser(&self) -> Result<ToolResult> {
        let response = start_device_login()
            .await
            .map_err(|e| Error::Tool(format!("Could not start browser sign-in: {e}")))?;
        let flow = BrowserFlow {
            device_code: response.device_code.clone(),
            user_code: response.user_code.clone(),
            verification_uri: response.verification_uri.clone(),
            interval: response.interval.max(1),
            started_at: Instant::now(),
        };
        *self.flow.lock().await = Some(flow.clone());
        let text = format!(
            "{SETUP_REQUIRED_MARKER} Browser sign-in started. Show the user this link and code exactly:\n\n\
             Open: {uri}\nCode: {code}\n\n\
             They can sign in to an existing account or create a new one there; the pending connection \
             is preserved. When the user says they have approved it (or after about {interval} seconds), \
             call account(action=\"connect_poll\"). The code expires in {expires} seconds.",
            uri = flow.verification_uri,
            code = flow.user_code,
            interval = flow.interval.max(5),
            expires = response.expires_in,
        );
        Ok(ToolResult::with_structured(
            text,
            json!({
                "status": "awaiting_browser",
                "verification_uri": flow.verification_uri,
                "user_code": flow.user_code,
                "expires_in": response.expires_in,
                "interval": flow.interval,
                "next": "account(action=\"connect_poll\")",
            }),
        ))
    }

    async fn connect_poll(&self) -> Result<ToolResult> {
        let flow = { self.flow.lock().await.clone() };
        let Some(flow) = flow else {
            return Ok(ToolResult::error(
                "No browser sign-in is in progress. Call account(action=\"connect_browser\") first.",
            ));
        };
        if flow.started_at.elapsed() > DEVICE_FLOW_MAX_AGE {
            *self.flow.lock().await = None;
            return Ok(ToolResult::error(
                "The browser sign-in expired. Call account(action=\"connect_browser\") to start again.",
            ));
        }

        let outcome = poll_device_login_once(&flow.device_code)
            .await
            .map_err(|e| Error::Tool(format!("Browser sign-in check failed: {e}")))?;
        match outcome {
            DevicePollOutcome::Pending { interval } => Ok(ToolResult::with_structured(
                format!(
                    "{SETUP_REQUIRED_MARKER} Still waiting for the browser approval. Remind the user to open \
                     {} and enter code {}, then call account(action=\"connect_poll\") again in about {} seconds.",
                    flow.verification_uri,
                    flow.user_code,
                    interval.max(5)
                ),
                json!({
                    "status": "pending",
                    "retry_after_secs": interval.max(5),
                    "verification_uri": flow.verification_uri,
                    "user_code": flow.user_code,
                }),
            )),
            DevicePollOutcome::Expired => {
                *self.flow.lock().await = None;
                Ok(ToolResult::error(
                    "The browser sign-in code expired. Call account(action=\"connect_browser\") to start again.",
                ))
            }
            DevicePollOutcome::Denied => {
                *self.flow.lock().await = None;
                Ok(ToolResult::error(
                    "The browser sign-in was denied. Call account(action=\"connect_browser\") to start again.",
                ))
            }
            DevicePollOutcome::Authorized(token) => {
                let result = self.complete_with_jwt(&token.access_token).await;
                *self.flow.lock().await = None;
                result
            }
        }
    }

    /// Mint a persistent API key with a short-lived JWT and save it. The key
    /// never leaves this process.
    async fn complete_with_jwt(&self, jwt: &str) -> Result<ToolResult> {
        let config = Config {
            api_url: self.api_url.clone(),
            jwt: Some(jwt.to_string()),
            api_key: None,
            ..Config::default()
        };
        let client = ContextStreamClient::new(config);
        let user = client
            .me()
            .await
            .map_err(|e| Error::Tool(format!("Signed in, but could not load the account: {e}")))?;
        let key_name = format!(
            "ContextStream MCP · {}",
            detect_hostname().unwrap_or_else(|| "this machine".to_string())
        );
        let api_key = client.create_api_key(&key_name).await.map_err(|e| {
            Error::Tool(format!(
                "Signed in as {}, but could not create an API key: {e}",
                user.email
            ))
        })?;
        self.save_credentials(&api_key, &user.email)?;
        drop(api_key);
        Ok(self.connected_result(&user.email))
    }

    fn save_credentials(&self, api_key: &str, email: &str) -> Result<()> {
        write_saved_credentials(api_key, None).map_err(|e| {
            Error::Tool(format!(
                "Signed in as {email}, but could not save credentials: {e}"
            ))
        })?;
        std::env::set_var("CONTEXTSTREAM_API_KEY", api_key);
        Ok(())
    }

    fn connected_result(&self, email: &str) -> ToolResult {
        let source = detect_credentials_source();
        let env_override = source == CredentialsSource::EnvironmentOverride;
        let mut text = format!(
            "Connected as {}. Credentials were saved to {}. {}",
            email,
            credentials_file_path().display(),
            RECONNECT_INSTRUCTIONS
        );
        if env_override {
            text.push_str(
                " Note: the editor configuration supplies its own CONTEXTSTREAM_API_KEY, which takes \
                 precedence over the saved file. Run `contextstream-mcp update-configs` or remove that env \
                 entry so the new credentials are used.",
            );
        }
        ToolResult::with_structured(
            text,
            json!({
                "status": "connected",
                "email": email,
                "credentials_path": credentials_file_path().display().to_string(),
                "reconnect_required": true,
                "env_override": env_override,
                "dashboard_url": self.dashboard_url(),
            }),
        )
    }

    // ----------------------------------------------------------------------
    // Inline email + SMS route
    // ----------------------------------------------------------------------

    fn inline_gate(&self, message: &str) -> ToolResult {
        ToolResult::text(format!("{SETUP_REQUIRED_MARKER} {message}"))
    }

    async fn current_inline(&self) -> Option<PendingConnection> {
        self.inline.lock().await.clone()
    }

    fn signup_failure(&self, context: &str, error: anyhow::Error) -> ToolResult {
        let (code, message) = signup_error_message(&error);
        let hint = match code.as_str() {
            "account_exists" | "account_exists_unverified" => {
                " An account already exists for this email: use account(action=\"connect_browser\") to sign in and connect this device."
            }
            "verification_locked" | "signup_attempt_expired" => {
                " Call account(action=\"cancel\") then account(action=\"signup_start\") to start again."
            }
            _ => "",
        };
        ToolResult::with_structured(
            format!("{context}: {message}{hint}"),
            json!({ "status": "error", "code": code, "message": message }),
        )
    }

    async fn signup_start(&self, input: &AccountInput) -> Result<ToolResult> {
        if !self.inline_available().await {
            return Ok(self.inline_gate(
                "The inline email + SMS signup is not available on this server. Use account(action=\"connect_browser\") to connect through the browser.",
            ));
        }
        let email = input
            .email
            .as_deref()
            .map(str::trim)
            .filter(|e| e.contains('@'))
            .ok_or_else(|| {
                Error::Validation("signup_start requires the user's email address".to_string())
            })?;
        if let Some(existing) = self.current_inline().await {
            if existing.email.eq_ignore_ascii_case(email)
                && existing.stage != PendingStage::Finalized
            {
                return Ok(ToolResult::with_structured(
                    format!(
                        "{SETUP_REQUIRED_MARKER} An inline signup for {} is already in progress at stage {:?}. Continue with {}, or call account(action=\"cancel\") to start over.",
                        existing.email, existing.stage, next_inline_action(&existing.stage)
                    ),
                    json!({ "status": "in_progress", "stage": existing.stage, "next": next_inline_action(&existing.stage) }),
                ));
            }
            let _ = client_auth::cancel_signup(&existing.attempt_id, &existing.client_secret).await;
            self.clear_inline().await;
        }
        let response = match client_auth::start_email_signup(
            email,
            input.full_name.as_deref(),
            client_metadata(),
        )
        .await
        {
            Ok(r) => r,
            Err(error) => return Ok(self.signup_failure("Could not start the signup", error)),
        };
        let pending = PendingConnection::new(
            response.attempt_id.clone(),
            response.client_secret.clone(),
            response.email.clone(),
        );
        self.persist_inline(&pending).await;
        *self.inline.lock().await = Some(pending);
        Ok(ToolResult::with_structured(
            format!(
                "{SETUP_REQUIRED_MARKER} We emailed a 6-digit verification code to {}. Ask the user for the code from that email, then call account(action=\"signup_verify_email\", code=\"<code>\"). The code expires in {} minutes; account(action=\"resend\", channel=\"email\") sends a new one after {} seconds.",
                response.email,
                (response.email_code_expires_in / 60).max(1),
                response.resend_after.max(30)
            ),
            json!({
                "status": "email_code_sent",
                "email": response.email,
                "email_code_expires_in": response.email_code_expires_in,
                "next": "account(action=\"signup_verify_email\", code=...)",
            }),
        ))
    }

    async fn signup_verify_email(&self, input: &AccountInput) -> Result<ToolResult> {
        let Some(mut pending) = self.current_inline().await else {
            return Ok(ToolResult::error(
                "No inline signup is in progress. Call account(action=\"signup_start\") first.",
            ));
        };
        let code = input
            .code
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .ok_or_else(|| {
                Error::Validation(
                    "signup_verify_email requires the code from the email".to_string(),
                )
            })?;
        match client_auth::verify_signup_email(&pending.attempt_id, &pending.client_secret, code)
            .await
        {
            Ok(_) => {
                pending.stage = PendingStage::EmailVerified;
                self.persist_inline(&pending).await;
                *self.inline.lock().await = Some(pending.clone());
                Ok(ToolResult::with_structured(
                    format!(
                        "{SETUP_REQUIRED_MARKER} Email {} verified. Ask the user for the mobile phone number they want to verify, in international format (for example +1 555 123 4567), then call account(action=\"signup_request_sms_consent\", phone=\"<number>\"). Do not send anything yet.",
                        pending.email
                    ),
                    json!({ "status": "email_verified", "next": "account(action=\"signup_request_sms_consent\", phone=...)" }),
                ))
            }
            Err(error) => Ok(self.signup_failure("Email code check failed", error)),
        }
    }

    async fn signup_request_sms_consent(&self, input: &AccountInput) -> Result<ToolResult> {
        let Some(mut pending) = self.current_inline().await else {
            return Ok(ToolResult::error(
                "No inline signup is in progress. Call account(action=\"signup_start\") first.",
            ));
        };
        let phone = input
            .phone
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .ok_or_else(|| {
                Error::Validation(
                    "signup_request_sms_consent requires the phone number".to_string(),
                )
            })?;
        match client_auth::request_sms_consent(&pending.attempt_id, &pending.client_secret, phone)
            .await
        {
            Ok(response) => {
                pending.stage = PendingStage::ConsentPending;
                pending.phone_last4 = Some(response.phone_last4.clone());
                pending.consent_token = Some(response.consent_token.clone());
                pending.consent_text = Some(response.consent_text.clone());
                self.persist_inline(&pending).await;
                *self.inline.lock().await = Some(pending);
                Ok(ToolResult::with_structured(
                    format!(
                        "{SETUP_REQUIRED_MARKER} The number ending in {} can receive verification texts. Before any text is sent, show the user this notice VERBATIM and ask them to reply yes or no:\n\n\"{}\"\n\nOnly if they agree, call account(action=\"signup_confirm_sms_consent\", user_response=\"<their exact reply>\"). If they decline, call account(action=\"cancel\").",
                        response.phone_last4, response.consent_text
                    ),
                    json!({
                        "status": "consent_required",
                        "phone_last4": response.phone_last4,
                        "line_type": response.line_type,
                        "consent_text": response.consent_text,
                        "consent_version": response.consent_version,
                        "next": "account(action=\"signup_confirm_sms_consent\", user_response=...)",
                    }),
                ))
            }
            Err(error) => Ok(self.signup_failure("That phone number can't be used", error)),
        }
    }

    async fn signup_confirm_sms_consent(&self, input: &AccountInput) -> Result<ToolResult> {
        let Some(mut pending) = self.current_inline().await else {
            return Ok(ToolResult::error(
                "No inline signup is in progress. Call account(action=\"signup_start\") first.",
            ));
        };
        let Some(consent_token) = pending.consent_token.clone() else {
            return Ok(ToolResult::error("Request the consent notice first with account(action=\"signup_request_sms_consent\", phone=...)."));
        };
        let reply = input.user_response.as_deref().map(str::trim).unwrap_or("");
        if looks_negative(reply) {
            return Ok(ToolResult::error(
                "No text will be sent: the recorded reply is empty or declines the notice. If the user agreed, pass their exact affirmative reply in user_response; otherwise call account(action=\"cancel\").",
            ));
        }
        match client_auth::add_signup_phone(
            &pending.attempt_id,
            &pending.client_secret,
            &consent_token,
            "mcp_chat",
            Some(reply),
        )
        .await
        {
            Ok(response) => {
                pending.stage = PendingStage::SmsSent;
                pending.phone_last4 = Some(response.phone_last4.clone());
                self.persist_inline(&pending).await;
                *self.inline.lock().await = Some(pending);
                Ok(ToolResult::with_structured(
                    format!(
                        "{SETUP_REQUIRED_MARKER} A 6-digit code was texted to the number ending in {}. Ask the user for it, then call account(action=\"signup_verify_phone\", code=\"<code>\"). account(action=\"resend\", channel=\"sms\") sends a new one after {} seconds.",
                        response.phone_last4, response.resend_after.max(30)
                    ),
                    json!({
                        "status": "sms_sent",
                        "phone_last4": response.phone_last4,
                        "code_expires_in": response.code_expires_in,
                        "next": "account(action=\"signup_verify_phone\", code=...)",
                    }),
                ))
            }
            Err(error) => Ok(self.signup_failure("Could not send the verification text", error)),
        }
    }

    async fn signup_verify_phone(&self, input: &AccountInput) -> Result<ToolResult> {
        let Some(mut pending) = self.current_inline().await else {
            return Ok(ToolResult::error(
                "No inline signup is in progress. Call account(action=\"signup_start\") first.",
            ));
        };
        let code = input
            .code
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .ok_or_else(|| {
                Error::Validation(
                    "signup_verify_phone requires the code from the text message".to_string(),
                )
            })?;
        match client_auth::verify_signup_phone(&pending.attempt_id, &pending.client_secret, code)
            .await
        {
            Ok(complete) => {
                pending.mark_finalized();
                self.persist_inline(&pending).await;
                *self.inline.lock().await = Some(pending.clone());
                self.finish_inline(pending, complete).await
            }
            Err(error) => Ok(self.signup_failure("Phone code check failed", error)),
        }
    }

    /// Save first, acknowledge second. An acknowledgement timeout after a
    /// successful save is still "connected".
    async fn finish_inline(
        &self,
        pending: PendingConnection,
        complete: SignupCompleteResponse,
    ) -> Result<ToolResult> {
        let api_url_override = (!complete.api_url.is_empty()
            && complete.api_url.trim_end_matches('/')
                != mcp_types::config::DEFAULT_API_URL.trim_end_matches('/'))
        .then(|| complete.api_url.trim_end_matches('/').to_string());
        write_saved_credentials(&complete.api_key.secret, api_url_override.as_deref())
            .map_err(|e| Error::Tool(format!("Account created for {}, but credentials could not be saved: {e}. Call account(action=\"fetch_credentials\") to retry.", complete.email)))?;
        std::env::set_var("CONTEXTSTREAM_API_KEY", &complete.api_key.secret);
        drop(complete.api_key);
        let acked = match tokio::time::timeout(
            Duration::from_secs(10),
            client_auth::ack_signup_credentials(&pending.attempt_id, &pending.client_secret),
        )
        .await
        {
            Ok(Ok(_)) => true,
            _ => false,
        };
        self.clear_inline().await;
        let mut result = self.connected_result(&complete.email);
        if !acked {
            if let Some(mcp_types::tool::ContentItem::Text { text }) = result.content.first_mut() {
                text.push_str(" (Delivery confirmation to the server timed out; that is harmless, the credentials are saved.)");
            }
        }
        Ok(result)
    }

    async fn fetch_credentials(&self) -> Result<ToolResult> {
        let Some(pending) = self.current_inline().await else {
            return Ok(ToolResult::error(
                "No pending signup to recover credentials for. If the account was created, use account(action=\"connect_browser\") to sign in and connect this device.",
            ));
        };
        match client_auth::fetch_signup_credentials(&pending.attempt_id, &pending.client_secret)
            .await
        {
            Ok(complete) => self.finish_inline(pending, complete).await,
            Err(error) => {
                let (code, _) = signup_error_message(&error);
                if matches!(code.as_str(), "GONE" | "signup_attempt_expired") {
                    self.clear_inline().await;
                }
                Ok(self.signup_failure(
                    "Credentials could not be redelivered; sign in through the browser to connect this device",
                    error,
                ))
            }
        }
    }

    async fn resend(&self, input: &AccountInput) -> Result<ToolResult> {
        let Some(pending) = self.current_inline().await else {
            return Ok(ToolResult::error("No inline signup is in progress."));
        };
        let channel = input.channel.as_deref().unwrap_or(match pending.stage {
            PendingStage::EmailSent => "email",
            _ => "sms",
        });
        let result = match channel {
            "email" => {
                client_auth::resend_signup_email(&pending.attempt_id, &pending.client_secret).await
            }
            _ => client_auth::resend_signup_sms(&pending.attempt_id, &pending.client_secret).await,
        };
        match result {
            Ok(r) => Ok(ToolResult::with_structured(
                format!("{SETUP_REQUIRED_MARKER} A new code was sent by {channel}. Ask the user for it ({} sends remaining).", r.sends_remaining),
                json!({ "status": "resent", "channel": channel, "sends_remaining": r.sends_remaining, "code_expires_in": r.code_expires_in }),
            )),
            Err(error) => Ok(self.signup_failure("Could not resend the code", error)),
        }
    }
}

fn next_inline_action(stage: &PendingStage) -> &'static str {
    match stage {
        PendingStage::EmailSent => "account(action=\"signup_verify_email\", code=...)",
        PendingStage::EmailVerified => "account(action=\"signup_request_sms_consent\", phone=...)",
        PendingStage::ConsentPending => {
            "account(action=\"signup_confirm_sms_consent\", user_response=...)"
        }
        PendingStage::SmsSent => "account(action=\"signup_verify_phone\", code=...)",
        PendingStage::Finalized => "account(action=\"fetch_credentials\")",
    }
}

fn detect_hostname() -> Option<String> {
    std::env::var("HOSTNAME")
        .ok()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .or_else(|| {
            std::process::Command::new("hostname")
                .output()
                .ok()
                .and_then(|out| String::from_utf8(out.stdout).ok())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
}

#[async_trait]
impl ToolHandler for AccountTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let input: AccountInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;
        let action = input.action.clone().unwrap_or_else(|| "status".to_string());
        match action.as_str() {
            "status" => Ok(self.status().await),
            "connect_browser" => self.connect_browser().await,
            "connect_poll" => self.connect_poll().await,
            "cancel" => Ok(self.cancel().await),
            "signup_start" => self.signup_start(&input).await,
            "signup_verify_email" => self.signup_verify_email(&input).await,
            "signup_request_sms_consent" => self.signup_request_sms_consent(&input).await,
            "signup_confirm_sms_consent" => self.signup_confirm_sms_consent(&input).await,
            "signup_verify_phone" => self.signup_verify_phone(&input).await,
            "resend" => self.resend(&input).await,
            "fetch_credentials" => self.fetch_credentials().await,
            other => Ok(ToolResult::error(format!(
                "Unknown account action '{other}'. Use status, connect_browser, connect_poll, signup_start, signup_verify_email, signup_request_sms_consent, signup_confirm_sms_consent, signup_verify_phone, resend, fetch_credentials, or cancel."
            ))),
        }
    }

    fn metadata(&self) -> &ToolMetadata {
        &self.metadata
    }

    fn input_schema(&self) -> Value {
        SchemaBuilder::new()
            .description("Connect ContextStream to an account (browser sign-in, or inline email + SMS signup when offered)")
            .string_enum(
                "action",
                "status | connect_browser | connect_poll | signup_start | signup_verify_email | signup_request_sms_consent | signup_confirm_sms_consent | signup_verify_phone | resend | fetch_credentials | cancel",
                &[
                    "status",
                    "connect_browser",
                    "connect_poll",
                    "signup_start",
                    "signup_verify_email",
                    "signup_request_sms_consent",
                    "signup_confirm_sms_consent",
                    "signup_verify_phone",
                    "resend",
                    "fetch_credentials",
                    "cancel",
                ],
                false,
            )
            .string("email", "signup_start: the user's email address", false)
            .string("full_name", "signup_start: the user's name (optional)", false)
            .string("phone", "signup_request_sms_consent: mobile number in international format", false)
            .string("code", "signup_verify_email / signup_verify_phone: the 6-digit code the user received", false)
            .string("user_response", "signup_confirm_sms_consent: the user's exact reply to the consent notice", false)
            .string_enum("channel", "resend: which code to resend", &["email", "sms"], false)
            .build()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(mode: AccountToolMode) -> AccountTool {
        AccountTool::new(&Config::default(), mode)
    }

    #[tokio::test]
    async fn poll_without_a_flow_is_an_error_not_a_panic() {
        let result = tool(AccountToolMode::Limited).connect_poll().await.unwrap();
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn inline_steps_without_a_signup_are_errors() {
        let t = tool(AccountToolMode::Limited);
        for action in [
            "signup_verify_email",
            "signup_request_sms_consent",
            "signup_confirm_sms_consent",
            "signup_verify_phone",
            "resend",
        ] {
            let result = t.execute(json!({"action": action, "code": "123456", "phone": "+15555550123", "user_response": "yes"})).await.unwrap();
            assert!(result.is_error, "{action}");
        }
        let unknown = t.execute(json!({"action": "explode"})).await.unwrap();
        assert!(unknown.is_error);
    }

    #[tokio::test]
    async fn missing_required_inputs_are_validation_errors() {
        let t = tool(AccountToolMode::Limited);
        assert!(
            matches!(t.signup_verify_email(&AccountInput { action: None, email: None, full_name: None, phone: None, code: None, user_response: None, channel: None }).await, Ok(r) if r.is_error)
        );
    }

    #[test]
    fn negative_replies_never_send_a_text() {
        assert!(looks_negative(""));
        assert!(looks_negative("no"));
        assert!(looks_negative("No, don't text me"));
        assert!(looks_negative("stop"));
        assert!(!looks_negative("yes"));
        assert!(!looks_negative("Yes, that's fine"));
        assert!(!looks_negative("ok go ahead"));
    }

    #[test]
    fn metadata_requires_confirmation_and_schema_lists_inline_inputs() {
        let t = tool(AccountToolMode::Full);
        assert!(t.metadata().annotations.requires_confirmation);
        assert_eq!(t.metadata().name, ACCOUNT_TOOL_NAME);
        let schema = t.input_schema();
        for field in [
            "action",
            "email",
            "phone",
            "code",
            "user_response",
            "channel",
        ] {
            assert!(schema["properties"][field].is_object(), "{field}");
        }
        assert!(!t.metadata().description.contains("cbiq_"));
    }

    #[test]
    fn client_metadata_never_includes_secrets() {
        let meta = client_metadata();
        let rendered = meta.to_string();
        assert!(meta.get("os").is_some());
        assert!(!rendered.contains("cbiq_"));
        assert!(meta.get("api_key").is_none());
    }

    #[test]
    fn next_action_follows_the_stage_machine() {
        assert!(next_inline_action(&PendingStage::EmailSent).contains("signup_verify_email"));
        assert!(
            next_inline_action(&PendingStage::EmailVerified).contains("signup_request_sms_consent")
        );
        assert!(next_inline_action(&PendingStage::ConsentPending)
            .contains("signup_confirm_sms_consent"));
        assert!(next_inline_action(&PendingStage::SmsSent).contains("signup_verify_phone"));
        assert!(next_inline_action(&PendingStage::Finalized).contains("fetch_credentials"));
    }
}
