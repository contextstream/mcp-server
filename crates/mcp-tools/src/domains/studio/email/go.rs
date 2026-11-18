//! The person's go for an email action that reaches people or can't be taken back in one
//! step: a send, a paused send resumed, an import. One rule for all of them
//! (`crate::go_ahead`, spec "Email, complete in every agent surface", rule 3):
//!
//! - A client that shows its person a form in the middle of the call is asked first, for at
//!   most `peer::LIVE_QUESTION_MAX`, and their no wins whatever the agent passes.
//! - On a stateless call the question would go back as the call's result, which a client
//!   may never show: there the value the agent confirms (the count the person was shown) is
//!   the go, and no question is sent.
//! - Where only the person's own yes counts (`go_ahead::asked_only`), nothing goes unless
//!   they were asked and said yes.

use serde_json::{json, Value};

use super::super::ELICIT_TIMEOUT;
use crate::peer::{self, Elicited};

/// What the person is asked, and what an answer is for.
pub(super) struct Question<'a> {
    /// The answer's key in a stateless call's `inputResponses`.
    pub key: &'a str,
    pub message: String,
    /// The tick box: its title, and what ticking it does.
    pub title: &'a str,
    pub does: String,
    /// What the question is about: an answer given for another state is not taken.
    pub state: String,
}

/// Whether the action goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Go {
    /// It goes: the person said yes when the client asked them (`asked`), or the agent
    /// confirmed what they agreed to.
    Yes { asked: bool },
    /// Asked through the call's own result: the client calls again with the answer.
    Asking,
    /// Asked, and they did not say yes.
    No,
    /// Only the person's own yes counts on this connection, and they could not be asked.
    NotAsked,
    /// Nothing says go yet; `unanswered` when a live question got no answer in time.
    Needed { unanswered: bool },
}

/// Asks the person where the client can; `confirmed` is whether the agent passed the value
/// they were shown (the count this would reach now).
pub(super) async fn persons_go(question: Question<'_>, confirmed: bool) -> Go {
    let asked_only = crate::go_ahead::asked_only();
    let agents_word = !asked_only && confirmed;
    let mut unanswered = false;
    let asked = match peer::current()
        .filter(peer::CallPeer::can_elicit)
        .filter(|peer| !(agents_word && peer.asks_through_the_result()))
    {
        Some(peer) => {
            let schema = json!({
                "type": "object",
                "properties": {
                    "confirm": {
                        "type": "boolean",
                        "title": question.title,
                        "description": question.does,
                        "default": false
                    }
                },
                "required": ["confirm"]
            });
            match peer
                .ask(
                    question.key,
                    &question.message,
                    schema,
                    &question.state,
                    ELICIT_TIMEOUT,
                )
                .await
            {
                Elicited::Pending => return Go::Asking,
                Elicited::Accepted(content)
                    if content.get("confirm").and_then(Value::as_bool) == Some(true) =>
                {
                    Some(true)
                }
                Elicited::Accepted(_) | Elicited::Declined | Elicited::Cancelled => Some(false),
                Elicited::Unanswered(reason) => {
                    tracing::debug!(%reason, key = question.key, "studio: the question got no answer");
                    unanswered = true;
                    None
                }
            }
        }
        None => None,
    };
    match asked {
        Some(true) => Go::Yes { asked: true },
        Some(false) => Go::No,
        // A value the agent confirms is not the person's go where only theirs counts.
        None if asked_only => Go::NotAsked,
        None if confirmed => Go::Yes { asked: false },
        None => Go::Needed { unanswered },
    }
}

/// Said when the person said go through the client.
pub(super) const ASKED_NOTE: &str = "The person said go when the client asked them.";
