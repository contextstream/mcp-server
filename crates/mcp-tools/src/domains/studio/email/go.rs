//! The person's go for an email action that reaches people or can't be taken back in one
//! step: a send, a paused send resumed, an import. One rule for all of them
//! (`crate::go_ahead`, spec "Email, complete in every agent surface", rule 3):
//!
//! - A client that shows its person a form in the middle of the call is asked first, for at
//!   most `peer::LIVE_QUESTION_MAX`, and their no wins whatever the agent passes.
//! - On a stateless call the question would go back as the call's result, which a client
//!   may never show: there the review token the tool gave when it showed the review
//!   (`confirm_review`) is the go, and no question is sent. A count alone is never a go.
//! - Where only the person's own yes counts (`go_ahead::asked_only`), nothing goes unless
//!   they were asked and said yes.

use serde_json::{json, Value};

use super::super::ELICIT_TIMEOUT;
use crate::peer::{self, Elicited};

fn is_mac_hex(mac: &str) -> bool {
    mac.len() == 24
        && mac
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Values carried inside a send review token: the revision and count the person saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct SendToken {
    pub revision: i64,
    pub recipients: i64,
}

impl SendToken {
    pub fn encode(self, mac: &str) -> String {
        format!("send1.{}.{}.{mac}", self.revision, self.recipients)
    }

    pub fn parse(raw: &str) -> Option<Self> {
        let mut parts = raw.split('.');
        if parts.next()? != "send1" {
            return None;
        }
        let revision = parts.next()?.parse::<i64>().ok()?;
        let recipients = parts.next()?.parse::<i64>().ok()?;
        let mac = parts.next()?;
        if parts.next().is_some() || !is_mac_hex(mac) || recipients < 0 {
            return None;
        }
        Some(Self {
            revision,
            recipients,
        })
    }
}

/// Values carried inside a resume review token: the revision and remaining count the person saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ResumeToken {
    pub revision: i64,
    pub remaining: i64,
}

impl ResumeToken {
    pub fn encode(self, mac: &str) -> String {
        format!("resume1.{}.{}.{mac}", self.revision, self.remaining)
    }

    pub fn parse(raw: &str) -> Option<Self> {
        let mut parts = raw.split('.');
        if parts.next()? != "resume1" {
            return None;
        }
        let revision = parts.next()?.parse::<i64>().ok()?;
        let remaining = parts.next()?.parse::<i64>().ok()?;
        let mac = parts.next()?;
        if parts.next().is_some() || !is_mac_hex(mac) || remaining < 0 {
            return None;
        }
        Some(Self {
            revision,
            remaining,
        })
    }
}

/// Values carried inside an import preview token: the eligible count and digest the person saw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ImportToken {
    pub eligible: i64,
    pub digest: Option<String>,
}

impl ImportToken {
    pub fn encode(&self, mac: &str) -> String {
        let digest_hex = self
            .digest
            .as_deref()
            .map_or_else(|| "-".to_string(), |d| hex::encode(d.as_bytes()));
        format!("import1.{}.{digest_hex}.{mac}", self.eligible)
    }

    pub fn parse(raw: &str) -> Option<Self> {
        let mut parts = raw.split('.');
        if parts.next()? != "import1" {
            return None;
        }
        let eligible = parts.next()?.parse::<i64>().ok()?;
        let digest_part = parts.next()?;
        let mac = parts.next()?;
        if parts.next().is_some() || !is_mac_hex(mac) || eligible < 0 {
            return None;
        }
        let digest = if digest_part == "-" {
            None
        } else {
            if !digest_part
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return None;
            }
            Some(String::from_utf8(hex::decode(digest_part).ok()?).ok()?)
        };
        Some(Self { eligible, digest })
    }
}

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

/// Asks the person where the client can; `confirmed` is whether the agent passed the review
/// token the tool gave for this exact state, and `count_given` is whether the agent passed a
/// count argument (which on a stateless call without `asked_only` answers the review instead of
/// hanging a client that never displays inline questions).
pub(super) async fn persons_go(question: Question<'_>, confirmed: bool, count_given: bool) -> Go {
    let asked_only = crate::go_ahead::asked_only();
    let skip_stateless_question = !asked_only && (confirmed || count_given);
    let mut unanswered = false;
    let asked = match peer::current()
        .filter(peer::CallPeer::can_elicit)
        .filter(|peer| {
            !(skip_stateless_question
                && peer.asks_through_the_result()
                && !peer.carries_inline_answer())
        }) {
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
