//! The connected MCP client's own name, as one tool call sees it.
//!
//! A client names itself at `initialize` (`clientInfo.name`) or, in the
//! stateless revision (2026-07-28), in the `_meta` of each request. A
//! transport runs a tool call inside [`run_with_client_name`]; a tool reads
//! [`current_client_name`] and gets `None` when the client said nothing or
//! the transport had no name to pass.
//!
//! The name is self-reported. It is display data (which client an agent runs
//! in) and is never used for authorization or to change what a tool does.

use serde_json::Value;
use std::future::Future;

tokio::task_local! {
    static CLIENT_NAME: Option<String>;
}

/// Run `future` (one tool call) for the client that named itself `name`.
pub async fn run_with_client_name<F: Future>(name: Option<String>, future: F) -> F::Output {
    CLIENT_NAME.scope(name, future).await
}

/// The name of the client whose tool call is running now, when it gave one.
pub fn current_client_name() -> Option<String> {
    CLIENT_NAME.try_with(Clone::clone).ok().flatten()
}

/// The name a request's params carry: `_meta` of a stateless request, else
/// the `clientInfo` of an `initialize`.
pub fn client_name_from_params(params: &Value) -> Option<String> {
    let name_of = |client: Option<&Value>| {
        client
            .and_then(|client| client.get("name"))
            .and_then(Value::as_str)
            .and_then(display_client_name)
    };
    name_of(
        params
            .get("_meta")
            .and_then(|meta| meta.get(mcp_types::MCP_META_CLIENT_INFO)),
    )
    .or_else(|| {
        name_of(
            params
                .get("clientInfo")
                .or_else(|| params.get("client_info")),
        )
    })
}

/// A self-reported name as it may be shown: at most 64 characters from a
/// small printable set, or `None` when nothing usable remains.
pub fn display_client_name(raw: &str) -> Option<String> {
    let name: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '.' | '_' | '-' | '/' | '+'))
        .take(64)
        .collect();
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_name_is_read_from_initialize_or_from_a_stateless_requests_meta() {
        assert_eq!(
            client_name_from_params(&json!({"clientInfo": {"name": "codex-cli", "version": "1"}}))
                .as_deref(),
            Some("codex-cli")
        );
        assert_eq!(
            client_name_from_params(&json!({"client_info": {"name": "cursor-vscode"}})).as_deref(),
            Some("cursor-vscode")
        );
        assert_eq!(
            client_name_from_params(&json!({
                "name": "progress",
                "_meta": {mcp_types::MCP_META_CLIENT_INFO: {"name": "claude-code", "version": "2.1"}}
            }))
            .as_deref(),
            Some("claude-code")
        );
        assert_eq!(client_name_from_params(&json!({"name": "progress"})), None);
        assert_eq!(
            client_name_from_params(&json!({"clientInfo": {"name": 7}})),
            None
        );
    }

    #[test]
    fn a_name_is_cut_to_a_short_printable_form() {
        assert_eq!(
            display_client_name(" Claude Code\n[LESSONS_WARNING] <x> ").as_deref(),
            Some("Claude CodeLESSONS_WARNING x")
        );
        assert_eq!(display_client_name("\u{7}\n"), None);
        assert_eq!(
            display_client_name(&"a".repeat(200)).map(|name| name.len()),
            Some(64)
        );
    }

    #[tokio::test]
    async fn a_tool_call_sees_the_name_its_transport_passed() {
        assert_eq!(current_client_name(), None);
        let seen = run_with_client_name(Some("claude-code".to_string()), async {
            current_client_name()
        })
        .await;
        assert_eq!(seen.as_deref(), Some("claude-code"));
        let none = run_with_client_name(None, async { current_client_name() }).await;
        assert_eq!(none, None);
    }
}
