//! The token a client sends to name one of its connections.
//!
//! A client on the stateless MCP contract (no `initialize`, no
//! `Mcp-Session-Id`) tells the hosted gateway nothing about which of its
//! sessions a request belongs to: every session of one API key looks the
//! same, on every machine. A client whose config can run a command when it
//! connects (Claude Code's `headersHelper`) sends one header with a random id
//! per connection instead; `contextstream-mcp hook mcp-headers` prints it.
//!
//! The gateway treats the value as untrusted input: a value of any other
//! shape than [`accepted`] describes is ignored, as if no header had been
//! sent, and a tool never uses the token alone. What stands for the session
//! is a digest of the caller and the token together, so a token chosen by
//! one key can never name a session of another key.

use uuid::Uuid;

/// The header the token travels in, as a config file spells it.
pub const HEADER: &str = "X-ContextStream-Client-Session";

/// The same name as an HTTP stack reads it.
pub const HEADER_LOWERCASE: &str = "x-contextstream-client-session";

/// The token's length: 32 lowercase hexadecimal characters.
pub const TOKEN_LEN: usize = 32;

/// A token as the gateway accepts it: exactly [`TOKEN_LEN`] characters of
/// `0-9a-f`, nothing around them. `None` for anything else.
pub fn accepted(raw: &str) -> Option<&str> {
    (raw.len() == TOKEN_LEN && raw.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
        .then_some(raw)
}

/// A fresh token: random, with nothing of the machine, the person, the
/// folder or the key in it.
pub fn new_token() -> String {
    Uuid::new_v4().simple().to_string()
}

tokio::task_local! {
    /// The token the request in hand carried, set by the hosted transport
    /// only when the value passed [`accepted`].
    static TASK_CLIENT_SESSION: String;
}

/// Run a future for a request that carried `token`.
pub async fn run_with<F, Fut>(token: String, f: F) -> Fut::Output
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future,
{
    TASK_CLIENT_SESSION.scope(token, f()).await
}

/// The token of the request in hand. `None` outside a hosted request and for
/// a request that carried none.
pub fn current() -> Option<String> {
    TASK_CLIENT_SESSION.try_with(|token| token.clone()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_thirty_two_lowercase_hex_characters_are_a_token() {
        let token = new_token();
        assert_eq!(accepted(&token), Some(token.as_str()));
        assert_eq!(
            accepted("0123456789abcdef0123456789abcdef"),
            Some("0123456789abcdef0123456789abcdef")
        );
        for refused in [
            "",
            "0123456789abcdef0123456789abcde",
            "0123456789abcdef0123456789abcdef0",
            "0123456789ABCDEF0123456789ABCDEF",
            "0123456789abcdef0123456789abcdeg",
            " 0123456789abcdef0123456789abcdef",
            "0123456789abcdef0123456789abcdef ",
            "0123456789abcdef-0123456789abcde",
            "0123456789abcdef0123456789abcd\u{e9}",
            "../../../../../../../../../../etc",
        ] {
            assert_eq!(accepted(refused), None, "{refused:?}");
        }
    }

    #[test]
    fn a_fresh_token_is_never_the_one_before() {
        let tokens: std::collections::HashSet<String> = (0..64).map(|_| new_token()).collect();
        assert_eq!(tokens.len(), 64);
    }

    #[tokio::test]
    async fn the_token_is_known_inside_the_request_it_came_with_and_nowhere_else() {
        assert_eq!(current(), None);
        let inside = run_with("0123456789abcdef0123456789abcdef".to_string(), || async {
            current()
        })
        .await;
        assert_eq!(inside.as_deref(), Some("0123456789abcdef0123456789abcdef"));
        assert_eq!(current(), None);
    }
}
