//! The most one call on a connection may spend, in credits: a ceiling the connection sets for
//! itself and no answer can raise. ContextStudio's recording machines set it on the MCP their
//! recorded session uses (0 for every film that is not a making; a making's own upper price for
//! one that is), because in a recorded session the person's go-ahead is typed by a script: a
//! client that cannot show the studio's price form makes on the agent's own
//! `accepted_price_credits`, and nothing else bounds it.
//!
//! - A local binary reads it from its environment ([`MAX_SPEND_ENV`]); a hosted connection
//!   sends it as a header ([`MAX_SPEND_HEADER`]). The hosted gateway never takes one from its
//!   own environment.
//! - Unset means no ceiling. Set to anything but a whole number of credits (empty included), it
//!   is 0: a ceiling that was meant is never read as none.
//! - The studio's `make` makes nothing priced above it, before the person is asked; an agent
//!   run priced above it, metered per model call (no price to compare), or automatic, does not
//!   run.

/// The variable a local binary reads.
pub const MAX_SPEND_ENV: &str = "CONTEXTSTREAM_MAX_SPEND_CREDITS";
/// The header a hosted connection sends.
pub const MAX_SPEND_HEADER: &str = "x-contextstream-max-spend-credits";

/// The ceiling a variable's or a header's value sets: none when it is absent, its whole number
/// of credits when it is one, else 0.
pub fn ceiling_from(raw: Option<&str>) -> Option<u32> {
    raw.map(|value| value.trim().parse::<u32>().unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::ceiling_from;

    #[test]
    fn a_ceiling_is_its_whole_number_and_anything_else_that_is_set_is_zero() {
        assert_eq!(ceiling_from(None), None, "unset: no ceiling");
        assert_eq!(ceiling_from(Some("30000")), Some(30_000));
        assert_eq!(ceiling_from(Some(" 0 ")), Some(0));
        for meant in [
            "",
            " ",
            "lots",
            "-1",
            "30,000",
            "1e9",
            "29999.5",
            "99999999999",
        ] {
            assert_eq!(ceiling_from(Some(meant)), Some(0), "{meant:?}");
        }
    }
}
