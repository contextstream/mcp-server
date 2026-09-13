//! Limited mode: the stdio server without credentials.
//!
//! Instead of a dead process, the agent gets a real MCP session with exactly
//! two tools: a lightweight `init` that explains the state and offers the
//! connection routes, and `account`, which performs them. Once credentials
//! are saved the user reconnects the server to get the full tool set (the
//! transport advertises `tools.listChanged: false`, so a live swap is not
//! offered).

use std::sync::Arc;

use async_trait::async_trait;
use mcp_session::SessionManager;
use mcp_tools::registry::{ToolHandler, ToolRegistry};
use mcp_tools::schema::SchemaBuilder;
use mcp_types::{
    tool::{ToolAnnotations, ToolCategory, ToolMetadata, ToolResult},
    Config, Result,
};
use serde_json::{json, Value};

use crate::account_tool::{
    detect_credentials_source, AccountTool, AccountToolMode, ACCOUNT_TOOL_NAME, SETUP_REQUIRED_MARKER,
};

pub const LIMITED_MODE_TOOLS: [&str; 2] = ["init", ACCOUNT_TOOL_NAME];

/// The two-option connection prompt shown by limited `init`.
pub const CONNECT_PROMPT: &str = "Connect ContextStream. Continue securely in your browser, or create an account here using email and SMS verification.";

/// Stand-in `init` for limited mode. Cheap, never fails, and every result is
/// an access gate so runtime readiness never treats it as grounding.
pub struct LimitedInitTool {
    metadata: ToolMetadata,
    api_url: String,
}

impl LimitedInitTool {
    pub fn new(config: &Config) -> Self {
        Self {
            metadata: ToolMetadata {
                name: "init".to_string(),
                title: "Initialize Session".to_string(),
                description: "Initialize a ContextStream session. This server is not connected to an \
                    account yet; init explains the connection routes. Call it FIRST in every conversation."
                    .to_string(),
                category: ToolCategory::Session,
                annotations: ToolAnnotations::read_only(),
                is_pro: false,
                required_tier: None,
            },
            api_url: config.api_url.trim_end_matches('/').to_string(),
        }
    }

    fn result(&self) -> ToolResult {
        let source = detect_credentials_source();
        let text = format!(
            "{SETUP_REQUIRED_MARKER} {CONNECT_PROMPT}\n\n\
             Present both options and let the user choose:\n\
             1. Browser (recommended): call account(action=\"connect_browser\"), show the user the link \
             and code it returns, then call account(action=\"connect_poll\") once they have approved it. \
             Works for existing accounts and new sign-ups alike.\n\
             2. Email + SMS, inline: only if account(action=\"status\") lists it as available; then follow \
             the account tool's steps. Do NOT ask for an email address or phone number unless the user \
             picks this option.\n\n\
             Credentials are saved by the server itself and never appear in this chat. After connecting, \
             reconnect the ContextStream MCP server to enable all tools."
        );
        ToolResult::with_structured(
            text,
            json!({
                "status": "setup_required",
                "mode": "limited",
                "routes": ["browser", "email"],
                "credentials_source": source,
                "api_url": self.api_url,
                "next": "account(action=\"status\") or account(action=\"connect_browser\")",
            }),
        )
    }
}

#[async_trait]
impl ToolHandler for LimitedInitTool {
    async fn execute(&self, _input: Value) -> Result<ToolResult> {
        Ok(self.result())
    }

    fn metadata(&self) -> &ToolMetadata {
        &self.metadata
    }

    fn input_schema(&self) -> Value {
        SchemaBuilder::new()
            .description("Initialize a ContextStream session (limited mode: not connected yet)")
            .string(
                "folder_path",
                "Absolute path to the project folder (accepted for compatibility; used after connecting).",
                false,
            )
            .string("context_hint", "The user's first message (accepted for compatibility).", false)
            .boolean("allow_no_workspace", "Accepted for compatibility.", false)
            .build()
    }
}

/// Registry with exactly `init` and `account`.
pub fn build_limited_registry(config: &Config, session: Arc<SessionManager>) -> ToolRegistry {
    let mut registry = ToolRegistry::new(config);
    registry.set_session_manager(session);
    registry.register("init", Arc::new(LimitedInitTool::new(config)));
    registry.register(
        ACCOUNT_TOOL_NAME,
        Arc::new(AccountTool::new(config, AccountToolMode::Limited)),
    );
    registry
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_client::ContextStreamClient;

    fn limited_config() -> Config {
        Config {
            api_key: None,
            jwt: None,
            ..Config::default()
        }
    }

    #[tokio::test]
    async fn limited_registry_has_exactly_init_and_account() {
        let config = limited_config();
        let client = ContextStreamClient::new(config.clone());
        let session = Arc::new(SessionManager::new(client, config.clone()));
        let registry = build_limited_registry(&config, session);
        let mut names: Vec<String> = registry
            .list()
            .into_iter()
            .map(|tool| tool.metadata.name.clone())
            .collect();
        names.sort();
        let mut expected: Vec<String> = LIMITED_MODE_TOOLS.iter().map(|s| s.to_string()).collect();
        expected.sort();
        assert_eq!(names, expected);
    }

    #[tokio::test]
    async fn limited_init_is_an_access_gate_with_both_routes() {
        let tool = LimitedInitTool::new(&limited_config());
        let result = tool.execute(json!({"folder_path": "/tmp/x"})).await.unwrap();
        assert!(!result.is_error);
        assert!(mcp_tools::registry::tool_result_is_access_gate(&result));
        let text = result
            .content
            .iter()
            .filter_map(|c| match c {
                mcp_types::tool::ContentItem::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert!(text.contains("connect_browser"));
        assert!(text.contains("Do NOT ask for an email address or phone number"));
    }
}
