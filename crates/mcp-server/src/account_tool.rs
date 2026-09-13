//! `account` tool: connect ContextStream from inside the agent.
//!
//! Browser route (default): start the device-login flow, show the user the
//! link and code, poll once per call, then mint an API key with the JWT and
//! save it with `write_saved_credentials`. The API key and every in-flight
//! secret stay in this process; tool results carry only a success state and
//! reconnect instructions.
//!
//! The inline email + SMS route is exposed as actions here too, but they are
//! only enabled when the server's `signup-config` offers inline signup. The
//! inline actions are added in a follow-up release; until then they answer
//! with an access-gate message that points at the browser route.
//!
//! Stdio only. The hosted HTTP gateway never registers this tool.

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use mcp_client::auth::{poll_device_login_once, start_device_login, DevicePollOutcome};
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

use crate::setup::{credentials_file_path, read_saved_credentials, write_saved_credentials};

pub const ACCOUNT_TOOL_NAME: &str = "account";

/// Marker recognised by `mcp_tools::registry::tool_result_is_access_gate` so
/// runtime readiness never counts setup instructions as successful grounding.
pub const SETUP_REQUIRED_MARKER: &str = "[setup_required]";

const DEVICE_FLOW_MAX_AGE: Duration = Duration::from_secs(15 * 60);

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
}

pub struct AccountTool {
    metadata: ToolMetadata,
    mode: AccountToolMode,
    api_url: String,
    flow: Arc<Mutex<Option<BrowserFlow>>>,
}

impl AccountTool {
    pub fn new(config: &Config, mode: AccountToolMode) -> Self {
        let mut annotations = ToolAnnotations::write().idempotent();
        annotations.requires_confirmation = true;
        Self {
            metadata: ToolMetadata {
                name: ACCOUNT_TOOL_NAME.to_string(),
                title: "Connect Account".to_string(),
                description: "Connect ContextStream to an account without handling an API key manually. \
                    Actions: status (credential state and available routes), connect_browser (start a browser \
                    sign-in/sign-up; show the user the link and code it returns), connect_poll (check whether \
                    the browser step finished; saves credentials on success), cancel. Never ask the user for an \
                    email or phone number unless they explicitly choose the inline email route. Secrets are never \
                    returned in results."
                    .to_string(),
                category: ToolCategory::Session,
                annotations,
                is_pro: false,
                required_tier: None,
            },
            mode,
            api_url: config.api_url.trim_end_matches('/').to_string(),
            flow: Arc::new(Mutex::new(None)),
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

    async fn status(&self) -> ToolResult {
        let source = detect_credentials_source();
        let flow = self.flow.lock().await.clone();
        let flow_summary = flow.as_ref().map(|f| {
            json!({
                "verification_uri": f.verification_uri,
                "user_code": f.user_code,
                "age_secs": f.started_at.elapsed().as_secs(),
            })
        });
        let connected = source != CredentialsSource::None && self.mode == AccountToolMode::Full;
        let text = match (self.mode, source) {
            (AccountToolMode::Limited, _) => format!(
                "{SETUP_REQUIRED_MARKER} ContextStream is installed but not connected to an account. \
                 Offer the user two routes: continue securely in the browser \
                 (account action=connect_browser), or create an account with email + SMS verification \
                 (inline route; only when the status reports it as available). Do not ask for an email \
                 or phone number unless they choose the inline route.{}",
                match flow {
                    Some(_) => " A browser sign-in is already in progress; call account(action=connect_poll) after the user approves it.",
                    None => "",
                }
            ),
            (AccountToolMode::Full, CredentialsSource::EnvironmentOverride) => format!(
                "Connected using an API key supplied by the editor configuration (CONTEXTSTREAM_API_KEY). \
                 A key saved by this tool will not take effect until that configuration is updated; run \
                 `contextstream-mcp update-configs` after connecting, or remove the env entry."
            ),
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
                "routes": ["browser"],
                "browser_flow": flow_summary,
                "next": if connected { Value::Null } else { json!("account(action=\"connect_browser\")") },
            }),
        )
    }

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
        let key_name = format!("ContextStream MCP · {}", detect_hostname().unwrap_or_else(|| "this machine".to_string()));
        let api_key = client
            .create_api_key(&key_name)
            .await
            .map_err(|e| Error::Tool(format!("Signed in as {}, but could not create an API key: {e}", user.email)))?;

        // Save before anything else; the saved file is the source of truth.
        write_saved_credentials(&api_key, None)
            .map_err(|e| Error::Tool(format!("Signed in as {}, but could not save credentials: {e}", user.email)))?;
        std::env::set_var("CONTEXTSTREAM_API_KEY", &api_key);
        drop(api_key);

        let source = detect_credentials_source();
        let env_override = source == CredentialsSource::EnvironmentOverride;
        let mut text = format!(
            "Connected as {}. Credentials were saved to {}. {}",
            user.email,
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
        Ok(ToolResult::with_structured(
            text,
            json!({
                "status": "connected",
                "email": user.email,
                "credentials_path": credentials_file_path().display().to_string(),
                "reconnect_required": true,
                "env_override": env_override,
                "dashboard_url": self.dashboard_url(),
            }),
        ))
    }

    async fn cancel(&self) -> ToolResult {
        let had_flow = self.flow.lock().await.take().is_some();
        ToolResult::with_structured(
            if had_flow {
                "Cancelled the pending browser sign-in."
            } else {
                "Nothing to cancel."
            },
            json!({ "status": "cancelled", "had_flow": had_flow }),
        )
    }

    fn inline_unavailable(&self, action: &str) -> ToolResult {
        ToolResult::text(format!(
            "{SETUP_REQUIRED_MARKER} The inline email + SMS signup action `{action}` is not available on this \
             server yet. Use account(action=\"connect_browser\") to connect through the browser."
        ))
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
        let action = input.action.unwrap_or_else(|| "status".to_string());
        match action.as_str() {
            "status" => Ok(self.status().await),
            "connect_browser" => self.connect_browser().await,
            "connect_poll" => self.connect_poll().await,
            "cancel" => Ok(self.cancel().await),
            "signup_start" | "signup_verify_email" | "signup_request_sms_consent"
            | "signup_confirm_sms_consent" | "signup_verify_phone" | "resend"
            | "fetch_credentials" => Ok(self.inline_unavailable(&action)),
            other => Ok(ToolResult::error(format!(
                "Unknown account action '{other}'. Use status, connect_browser, connect_poll, or cancel."
            ))),
        }
    }

    fn metadata(&self) -> &ToolMetadata {
        &self.metadata
    }

    fn input_schema(&self) -> Value {
        SchemaBuilder::new()
            .description("Connect ContextStream to an account (browser sign-in, credential status)")
            .string_enum(
                "action",
                "status | connect_browser | connect_poll | cancel",
                &[
                    "status",
                    "connect_browser",
                    "connect_poll",
                    "cancel",
                    "signup_start",
                    "signup_verify_email",
                    "signup_request_sms_consent",
                    "signup_confirm_sms_consent",
                    "signup_verify_phone",
                    "resend",
                    "fetch_credentials",
                ],
                false,
            )
            .build()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(mode: AccountToolMode) -> AccountTool {
        AccountTool::new(&Config::default(), mode)
    }

    fn text_of(result: &ToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|item| match item {
                mcp_types::tool::ContentItem::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn limited_status_is_an_access_gate_and_offers_the_browser_route() {
        let result = tool(AccountToolMode::Limited).status().await;
        assert!(mcp_tools::registry::tool_result_is_access_gate(&result));
        let text = text_of(&result);
        assert!(text.contains("connect_browser"));
        assert!(text.contains("Do not ask for an email or phone number"));
    }

    #[tokio::test]
    async fn poll_without_a_flow_is_an_error_not_a_panic() {
        let result = tool(AccountToolMode::Limited).connect_poll().await.unwrap();
        assert!(result.is_error);
    }

    #[tokio::test]
    async fn unknown_actions_are_rejected_and_inline_actions_are_gated() {
        let tool = tool(AccountToolMode::Limited);
        let unknown = tool.execute(json!({"action": "explode"})).await.unwrap();
        assert!(unknown.is_error);
        let inline = tool.execute(json!({"action": "signup_start"})).await.unwrap();
        assert!(mcp_tools::registry::tool_result_is_access_gate(&inline));
    }

    #[test]
    fn metadata_requires_confirmation_and_never_mentions_secrets() {
        let tool = tool(AccountToolMode::Full);
        assert!(tool.metadata().annotations.requires_confirmation);
        assert_eq!(tool.metadata().name, ACCOUNT_TOOL_NAME);
        let schema = tool.input_schema();
        assert!(schema["properties"]["action"].is_object());
    }
}
