//! Whose word a go-ahead is. Several actions need the person's go-ahead: the studio's `make`
//! (a price), `email_send` (who it goes to), taking an address off the suppression list, a
//! priced agent run, anything the `delete` tool does. Where the client can show the person a
//! question, they are asked. Where it cannot, or the question gets no answer, the agent may
//! pass the go-ahead itself (`accepted_price_credits`, `confirm_recipients`, `confirm_email`),
//! and the `delete` tool and an agent run never ask at all: the go-ahead is then the agent's
//! own word that the person agreed.
//!
//! A connection can refuse that word: with [`GO_AHEAD_ENV`] set (a local binary) or
//! [`GO_AHEAD_HEADER`] sent (a hosted connection), a go-ahead counts only when the person was
//! asked through the client and said yes. Asked and accepted, the action goes as before; not
//! asked, it is not done, whatever the agent passes. ContextStudio's recording machines set it
//! on the MCP their recorded session uses, because there the person's words are typed by a
//! script and a recorded session never emails anyone or spends on its own word.
//!
//! - Any value turns it on (`asked` is the one to write), a value that cannot be read
//!   included: a setting that was meant is never read as absent. Only `0`, `false`, `no` and
//!   `off` say off.
//! - A hosted connection's header decides for that connection; a local binary reads its
//!   environment.
//! - What still goes without the person: a free agent run, and a priced one under the
//!   person's own auto-accept cap, which is the connection's standing setting and not the
//!   agent's word.

/// The variable a local binary reads.
pub const GO_AHEAD_ENV: &str = "CONTEXTSTREAM_GO_AHEAD";
/// The header a hosted connection sends.
pub const GO_AHEAD_HEADER: &str = "x-contextstream-go-ahead";
/// Why an action was not done, for every answer that refuses the agent's word.
pub const NOT_ASKED: &str = "on this connection a go-ahead counts only when the person is asked and says yes, and they could not be asked";

/// What a setting that is present says: on, unless it says off in so many words. `None` is a
/// value that could not be read.
pub fn on(value: Option<&str>) -> bool {
    !matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("0" | "false" | "no" | "off")
    )
}

/// Whether, on this connection, a go-ahead counts only when the person was asked: the
/// connection's header when it sent one, else the local binary's environment.
pub fn asked_only() -> bool {
    mcp_client::get_task_config_override()
        .and_then(|config| config.go_ahead_asked_only)
        .unwrap_or_else(|| std::env::var_os(GO_AHEAD_ENV).is_some_and(|value| on(value.to_str())))
}

#[cfg(test)]
mod tests {
    use super::on;

    #[test]
    fn a_setting_that_is_present_is_on_unless_it_says_off() {
        for meant in [
            "asked", "1", "true", "yes", "on", "", " ", "agent", "ASKED", "anything",
        ] {
            assert!(on(Some(meant)), "{meant:?}");
        }
        assert!(on(None), "a value that cannot be read");
        for off in ["0", "false", "no", "off", " OFF ", "False"] {
            assert!(!on(Some(off)), "{off:?}");
        }
    }
}
