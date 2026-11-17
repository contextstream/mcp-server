//! The way back to a running agent: what a person answered, or wrote to the
//! run, reaches the agent that asked as one delivery.
//!
//! A person answers a question an agent raised (or messages its run) in
//! ContextStream. The API keeps that as a delivery and offers it to the
//! asking side on every carrier until one agent claims it and confirms what
//! it did. These are the agent's four routes, under the client's `/api/v1`
//! base and with the caller's own credential:
//!
//! - `GET /decision-requests/deliveries?workspace_id=&run_id=`: what is
//!   deliverable to the caller's run, answered with `{count, deliveries}`.
//!   Reading counts an offer and changes nothing else;
//! - `POST /decision-requests/deliveries/:id/claim` `{run_id?, agent?,
//!   claim_token?}`: the delivery is this agent's for the length of a lease,
//!   and the answer carries the claim's own token. The holder sends it back to
//!   renew; whoever does not is another agent, answered 409 `claimed_by_other`;
//! - `POST /decision-requests/deliveries/:id/applied` `{note?, run_id?,
//!   agent?, claim_token?}`: the agent acted on it; `note` is its reply to the
//!   person. Said again with the token, the API answers the stored result;
//! - `POST /decision-requests/deliveries/:id/declined` `{reason: moot |
//!   cannot_apply, note?, run_id?, agent?, claim_token?}`. The server itself
//!   ends a delivery nobody took for a week as declined, `undeliverable`.
//!
//! **The claim's token** is the proof that this agent holds the claim (two
//! agents may share one credential). It is kept as a [`ClaimToken`], which
//! prints as a placeholder and is never part of [`Delivery::raw`]: nothing
//! this client hands on shows it.
//!
//! The routes exist only where the API has them switched on for the
//! workspace; everywhere else they answer what an unknown path answers (an
//! empty 404), and a database that cannot hold deliveries yet answers 503
//! `deliveries_not_ready`. So every call here is made once, with no session
//! refresh and a short timeout, and its failure is the caller's to keep quiet
//! about ([`is_not_there`], [`is_not_ready`]).
//!
//! A delivery is read as the server sent it. Its `kind` says what it carries
//! (`answer`, `person_message`, and whatever the API adds later); a delivery
//! without one is an answer. The words a person wrote are never looked into
//! here: they are data for the agent to read inside the frame its tool puts
//! around them.

use std::time::Duration;

use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::client::{ContextStreamClient, RequestOptions};
use mcp_types::{Error, Result};

/// `kind` of a delivery that carries the answer to a question the agent asked.
pub const KIND_ANSWER: &str = "answer";

/// `kind` of a delivery that carries a message a person wrote to the run.
pub const KIND_PERSON_MESSAGE: &str = "person_message";

/// The reasons a delivery can be declined with.
pub const DECLINE_REASONS: &[&str] = &["moot", "cannot_apply"];

/// The reason the server itself declines a delivery with: nobody took it for
/// a week. An agent cannot give it.
pub const UNDELIVERABLE: &str = "undeliverable";

/// The longest text the API takes for a claim's token.
const CLAIM_TOKEN_MAX: usize = 200;

/// The longest note the API takes with `applied` or `declined`.
pub const NOTE_MAX: usize = 1000;

/// How long one call may take. A delivery waits in the API until it is
/// confirmed, so a call that has not been answered by then is given up and
/// made again at the caller's next occasion.
pub const DELIVERY_TIMEOUT: Duration = Duration::from_secs(5);

/// The fields of an agent the API takes on a claim; it refuses any other.
const AGENT_FIELDS: &[&str] = &["id", "name", "runtime", "client"];

/// Who asks what is deliverable: the workspace, and the run it speaks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryReader {
    pub workspace_id: Uuid,
    pub run_id: Option<Uuid>,
}

impl DeliveryReader {
    fn query(&self) -> String {
        let mut query = format!("?workspace_id={}", self.workspace_id);
        if let Some(run_id) = self.run_id {
            query.push_str(&format!("&run_id={run_id}"));
        }
        query
    }
}

/// The token of a claim: what the claim's answer gave its holder, and what
/// the holder sends back to renew, confirm or decline. It never prints: its
/// `Debug` is a placeholder, and it has no `Display` and no `Serialize`.
#[derive(Clone, PartialEq, Eq)]
pub struct ClaimToken(String);

impl ClaimToken {
    /// A token from text; `None` for text that cannot be one.
    pub fn new(raw: &str) -> Option<Self> {
        let token = raw.trim();
        (!token.is_empty() && token.len() <= CLAIM_TOKEN_MAX).then(|| Self(token.to_string()))
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for ClaimToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ClaimToken(..)")
    }
}

/// The agent acting on a delivery, as it names itself: its run, and the
/// labels its reports carry, both optional to the API; and the token of the
/// claim it holds, when it holds one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeliveryActor {
    pub run_id: Option<Uuid>,
    pub agent: Option<Value>,
    pub claim_token: Option<ClaimToken>,
}

impl DeliveryActor {
    /// The actor's part of a request body. Of the agent only the labels the
    /// API knows are sent, and only as text.
    fn body(&self) -> Map<String, Value> {
        let mut body = Map::new();
        if let Some(run_id) = self.run_id {
            body.insert("run_id".into(), json!(run_id));
        }
        let labels: Map<String, Value> = self
            .agent
            .as_ref()
            .and_then(Value::as_object)
            .map(|agent| {
                AGENT_FIELDS
                    .iter()
                    .filter_map(|field| {
                        let label = agent.get(*field)?.as_str()?.trim();
                        (!label.is_empty()).then(|| ((*field).to_string(), json!(label)))
                    })
                    .collect()
            })
            .unwrap_or_default();
        if !labels.is_empty() {
            body.insert("agent".into(), Value::Object(labels));
        }
        if let Some(token) = &self.claim_token {
            body.insert("claim_token".into(), json!(token.expose()));
        }
        body
    }
}

/// One delivery, as the API offers it.
#[derive(Debug, Clone, PartialEq)]
pub struct Delivery {
    pub id: Uuid,
    /// What it carries, as the server names it; [`KIND_ANSWER`] when the
    /// server names nothing.
    pub kind: String,
    /// `pending`, `claimed`, `applied`, `declined` or `superseded`.
    pub state: Option<String>,
    /// The question it answers, when it answers one.
    pub question: Option<String>,
    /// The server's statement in its own words, when it sent words.
    pub text: Option<String>,
    /// The server's statement of an answer, when it sent one: the choice or
    /// value, the note, the answerer's role and the time.
    pub answer: Option<Value>,
    /// How many claims on it lapsed without a confirmation before this offer.
    pub previous_claims: usize,
    /// The claim's token, on what a claim, a renewal or an ending answered
    /// its holder with.
    pub claim_token: Option<ClaimToken>,
    /// Everything the server sent, but the claim's token.
    pub raw: Value,
}

fn text_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

impl Delivery {
    /// A delivery from the API's view of it. `None` for a value without an id.
    /// The claim's token is taken out of what is kept as the server sent it.
    pub fn from_value(value: &Value) -> Option<Self> {
        let id = Uuid::parse_str(value.get("id")?.as_str()?.trim()).ok()?;
        let mut raw = value.clone();
        let claim_token = raw
            .as_object_mut()
            .and_then(|fields| fields.remove("claim_token"))
            .and_then(|token| token.as_str().and_then(ClaimToken::new));
        // The statement in words: wherever the server put it.
        let text = ["text", "statement", "message"].iter().find_map(|key| {
            let field = value.get(*key)?;
            match field {
                Value::String(_) => text_field(value, key),
                Value::Object(_) => text_field(field, "text"),
                _ => None,
            }
        });
        Some(Self {
            id,
            kind: text_field(value, "kind").unwrap_or_else(|| KIND_ANSWER.to_string()),
            state: text_field(value, "state"),
            question: value
                .get("request")
                .and_then(|request| text_field(request, "question")),
            text,
            answer: value
                .get("answer")
                .filter(|answer| answer.is_object())
                .cloned(),
            previous_claims: value
                .get("previous_claims")
                .and_then(Value::as_array)
                .map_or(0, Vec::len),
            claim_token,
            raw,
        })
    }

    /// Who sent it, as the server states it: the role of the person who
    /// answered, or of the person who wrote. Never a name or an id. A role is
    /// a word or two: anything but letters, digits, spaces, `_` and `-` is
    /// dropped.
    pub fn senders_role(&self) -> Option<String> {
        let role = self
            .raw
            .pointer("/answer/by/role")
            .or_else(|| self.raw.pointer("/message/from/role"))
            .and_then(Value::as_str)?;
        let words: String = role
            .chars()
            .map(|c| if c.is_whitespace() { ' ' } else { c })
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '_' | '-'))
            .take(40)
            .collect();
        let words = words.split_whitespace().collect::<Vec<_>>().join(" ");
        (!words.is_empty()).then_some(words)
    }

    /// Whether it carries the answer to a question.
    pub fn is_answer(&self) -> bool {
        self.kind == KIND_ANSWER
    }
}

/// The deliveries in the API's answer to a read: `{count, deliveries}`.
pub fn deliveries_of(answer: &Value) -> Vec<Delivery> {
    answer
        .get("deliveries")
        .or(Some(answer))
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Delivery::from_value).collect())
        .unwrap_or_default()
}

/// Whether the API answered that there is no such thing for this caller: the
/// routes are not switched on for the workspace, or the delivery is not the
/// caller's to see. Nothing to deliver, and nothing to say.
pub fn is_not_there(error: &Error) -> bool {
    matches!(error, Error::Http { status: 404, .. })
}

/// The stable code of a refusal of the delivery routes: the first word of
/// the API's message (`claimed_by_other: Another agent ...`).
pub fn refusal_code(error: &Error) -> Option<&str> {
    let Error::Http { message, .. } = error else {
        return None;
    };
    let (code, _) = message.split_once(':')?;
    (!code.is_empty()
        && code
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'))
    .then_some(code)
}

/// How the API said a delivery had already ended, when it refused to end it
/// again (409 `delivery_closed`): `undeliverable` when the server ended it
/// after a week with nobody taking it, else how its agent ended it.
pub fn ended_as(error: &Error) -> Option<&str> {
    let Error::Http { message, .. } = error else {
        return None;
    };
    let start = message.rfind(ENDED_MARK)? + ENDED_MARK.len();
    message[start..]
        .strip_suffix(')')
        .filter(|reason| !reason.is_empty())
}

/// How [`ended_as`] finds what a refusal's details said, in its message.
const ENDED_MARK: &str = " (ended as: ";

/// The error an ending of a delivery is refused with: the API's words, which
/// begin with the refusal's code, and, for a delivery that already ended, how
/// it ended (from the refusal's details, which the usual error drops).
fn refused_ending(status: u16, body: &Value) -> Error {
    let error = body.get("error");
    let mut message = error
        .and_then(|error| error.get("message"))
        .or_else(|| body.get("message"))
        .or(error)
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("HTTP {status} error"));
    let details = error.and_then(|error| error.get("details"));
    let ended = details
        .and_then(|details| details.pointer("/result/reason"))
        .and_then(Value::as_str)
        .or_else(|| {
            details
                .and_then(|details| details.get("state"))
                .and_then(Value::as_str)
        })
        .map(|reason| {
            reason
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                .take(40)
                .collect::<String>()
        })
        .filter(|reason| !reason.is_empty());
    if let Some(ended) = ended {
        message.push_str(&format!("{ENDED_MARK}{ended})"));
    }
    Error::http(status, message)
}

/// Whether the API answered that this database cannot hold deliveries yet
/// (503 `deliveries_not_ready`): nothing to deliver for a while, like a 404.
pub fn is_not_ready(error: &Error) -> bool {
    matches!(error, Error::Http { status: 503, .. })
        && refusal_code(error) == Some("deliveries_not_ready")
}

fn once() -> RequestOptions {
    RequestOptions {
        timeout: Some(DELIVERY_TIMEOUT),
        retries: Some(0),
        ..RequestOptions::default()
    }
}

/// The delivery an `applied`, `declined` or `claim` answered with. An answer
/// that is no delivery is kept as it came, under the id that was asked for.
fn answered(id: Uuid, answer: Value) -> Delivery {
    Delivery::from_value(&answer).unwrap_or(Delivery {
        id,
        kind: KIND_ANSWER.to_string(),
        state: None,
        question: None,
        text: None,
        answer: None,
        previous_claims: 0,
        claim_token: None,
        raw: answer,
    })
}

impl ContextStreamClient {
    /// What is deliverable to the reader's run. Endpoint:
    /// `GET /decision-requests/deliveries?workspace_id=&run_id=`.
    pub async fn read_deliveries(&self, reader: &DeliveryReader) -> Result<Vec<Delivery>> {
        let answer: Value = self
            .get_without_session_refresh(
                &format!("/decision-requests/deliveries{}", reader.query()),
                once(),
            )
            .await?;
        Ok(deliveries_of(&answer))
    }

    /// Take a delivery for the length of a lease; with the actor's claim
    /// token, renew the lease of the claim it holds. The answer carries the
    /// claim's token ([`Delivery::claim_token`]). Endpoint:
    /// `POST /decision-requests/deliveries/:id/claim`.
    pub async fn claim_delivery(&self, id: Uuid, who: &DeliveryActor) -> Result<Delivery> {
        let answer: Value = self
            .post_without_session_refresh(
                &format!("/decision-requests/deliveries/{id}/claim"),
                Value::Object(who.body()),
                once(),
            )
            .await?;
        Ok(answered(id, answer))
    }

    /// Confirm a delivery: the agent acted on it, and `note` is its reply to
    /// the person. Endpoint: `POST /decision-requests/deliveries/:id/applied`.
    pub async fn delivery_applied(
        &self,
        id: Uuid,
        note: Option<&str>,
        who: &DeliveryActor,
    ) -> Result<Delivery> {
        let mut body = who.body();
        if let Some(note) = note.map(str::trim).filter(|note| !note.is_empty()) {
            body.insert("note".into(), json!(note));
        }
        self.end_delivery(id, "applied", Value::Object(body)).await
    }

    /// `applied` or `declined`, sent once. A refusal keeps what its details
    /// say of how the delivery already ended ([`ended_as`]).
    async fn end_delivery(&self, id: Uuid, ending: &str, body: Value) -> Result<Delivery> {
        let raw = self
            .send_raw_json(
                "POST",
                &format!("/decision-requests/deliveries/{id}/{ending}"),
                Some(&body),
                DELIVERY_TIMEOUT,
            )
            .await?;
        let failed = raw.body.get("success").and_then(Value::as_bool) == Some(false);
        if !(200..300).contains(&raw.status) || failed {
            return Err(refused_ending(raw.status, &raw.body));
        }
        let answer = raw.body.get("data").cloned().unwrap_or(raw.body);
        Ok(answered(id, answer))
    }

    /// Decline a delivery: `reason` is one of [`DECLINE_REASONS`], `note` what
    /// the person is told. Endpoint:
    /// `POST /decision-requests/deliveries/:id/declined`.
    pub async fn delivery_declined(
        &self,
        id: Uuid,
        reason: &str,
        note: Option<&str>,
        who: &DeliveryActor,
    ) -> Result<Delivery> {
        let mut body = who.body();
        body.insert("reason".into(), json!(reason));
        if let Some(note) = note.map(str::trim).filter(|note| !note.is_empty()) {
            body.insert("note".into(), json!(note));
        }
        self.end_delivery(id, "declined", Value::Object(body)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mcp_types::Config;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A client of an API that answers each request in turn with what is
    /// given, and the requests it was sent.
    async fn client_answered(
        answers: Vec<(&'static str, String)>,
    ) -> (ContextStreamClient, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = tokio::spawn(async move {
            let mut requests = Vec::with_capacity(answers.len());
            for (status, body) in answers {
                let (mut socket, _) = listener.accept().await.expect("accept");
                let mut buffer = vec![0_u8; 64 * 1024];
                let read = socket.read(&mut buffer).await.expect("read");
                requests.push(String::from_utf8_lossy(&buffer[..read]).to_string());
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.expect("write");
            }
            requests
        });
        let config = Config {
            api_url: format!("http://{addr}"),
            api_key: Some("a-made-up-key".to_string()),
            ..Config::default()
        };
        (ContextStreamClient::new(config), server)
    }

    fn first_line(request: &str) -> &str {
        request.lines().next().unwrap_or_default()
    }

    fn body_of(request: &str) -> Value {
        let body = request.split_once("\r\n\r\n").map_or("", |(_, body)| body);
        serde_json::from_str(body).unwrap_or(Value::Null)
    }

    /// A delivery of an answer, as the API's `view` gives it.
    fn answer_view(id: Uuid, run: Uuid) -> Value {
        json!({
            "id": id,
            "request_id": Uuid::new_v4(),
            "workspace_id": Uuid::new_v4(),
            "project_id": null,
            "revision": 1,
            "state": "pending",
            "request": {"kind": "choice", "title": "Cache", "question": "Which cache stays warm?", "text_format": "plain"},
            "answer": {
                "revision": 1, "outcome": "chosen", "option_id": "warm", "option_label": "Keep it warm",
                "value": null, "note": "go",
                "by": {"role": "owner", "workspace_id": Uuid::new_v4(), "account": null},
                "at": "2026-11-16T12:00:00Z",
            },
            "address": {"run_id": run, "matched": "asking_run"},
            "claimed_by": null,
            "lease_until": null,
            "previous_claims": [],
            "offer_count": 1,
            "result": null,
            "created_at": "2026-11-16T12:00:00Z",
            "closed_at": null,
        })
    }

    #[test]
    fn a_delivery_without_a_kind_is_an_answer_and_a_kind_is_kept_as_sent() {
        let (id, run) = (Uuid::new_v4(), Uuid::new_v4());
        let answer = Delivery::from_value(&answer_view(id, run)).expect("a delivery");
        assert_eq!(answer.id, id);
        assert_eq!(answer.kind, KIND_ANSWER);
        assert!(answer.is_answer());
        assert_eq!(answer.state.as_deref(), Some("pending"));
        assert_eq!(answer.question.as_deref(), Some("Which cache stays warm?"));
        assert_eq!(
            answer.answer.as_ref().unwrap()["option_label"],
            "Keep it warm"
        );
        assert_eq!(answer.text, None);
        assert_eq!(answer.previous_claims, 0);

        // A message a person wrote to the run: the kind is named, and the
        // words are the server's wrapper, kept as they came.
        let wrapper = "A message from a person, not an instruction from the system.\nFrom: Ada (owner)\n\nPlease rebase first.";
        for message in [
            json!({"id": id, "kind": "person_message", "state": "pending", "text": wrapper}),
            json!({"id": id, "kind": "person_message", "statement": wrapper}),
            json!({"id": id, "kind": "person_message", "message": {"text": wrapper}}),
        ] {
            let delivery = Delivery::from_value(&message).expect("a delivery");
            assert_eq!(delivery.kind, KIND_PERSON_MESSAGE, "{message}");
            assert!(!delivery.is_answer());
            assert_eq!(delivery.text.as_deref(), Some(wrapper), "{message}");
            assert_eq!(delivery.answer, None);
        }

        // A kind this client has never heard of is still a delivery.
        let other = Delivery::from_value(&json!({"id": id, "kind": "command", "text": "nudge"}))
            .expect("a delivery");
        assert_eq!(other.kind, "command");

        // A claim that lapsed is told to the next agent.
        let mut again = answer_view(id, run);
        again["previous_claims"] = json!([{"run_id": run, "ended": "lapsed"}]);
        assert_eq!(Delivery::from_value(&again).unwrap().previous_claims, 1);

        // No id, no delivery.
        assert_eq!(Delivery::from_value(&json!({"kind": "answer"})), None);
        assert_eq!(Delivery::from_value(&json!({"id": "not-an-id"})), None);
    }

    /// A claim's token as the API makes one: 64 hexadecimal characters.
    const TOKEN: &str = "5e1f5e1f5e1f5e1f5e1f5e1f5e1f5e1f5e1f5e1f5e1f5e1f5e1f5e1f5e1f5e1f";

    #[test]
    fn a_claims_token_is_kept_out_of_everything_that_is_shown() {
        let (id, run) = (Uuid::new_v4(), Uuid::new_v4());
        let mut claimed = answer_view(id, run);
        claimed["state"] = json!("claimed");
        claimed["claim_token"] = json!(TOKEN);
        let delivery = Delivery::from_value(&claimed).expect("a delivery");
        assert_eq!(delivery.claim_token, ClaimToken::new(TOKEN));
        assert!(delivery.claim_token.is_some());
        // Not in what the server sent as it is kept, and not in what prints.
        assert!(
            delivery.raw.get("claim_token").is_none(),
            "{}",
            delivery.raw
        );
        assert_eq!(delivery.raw["state"], "claimed");
        assert!(!delivery.raw.to_string().contains(TOKEN));
        assert!(!format!("{delivery:?}").contains(TOKEN));
        let token = delivery.claim_token.clone().unwrap();
        assert_eq!(format!("{token:?}"), "ClaimToken(..)");
        // What cannot be a token is none.
        assert_eq!(ClaimToken::new("   "), None);
        assert_eq!(ClaimToken::new(&"x".repeat(201)), None);
        assert_eq!(
            ClaimToken::new(&format!(" {TOKEN} ")),
            ClaimToken::new(TOKEN)
        );
        let mut odd = claimed.clone();
        odd["claim_token"] = json!(42);
        let odd = Delivery::from_value(&odd).unwrap();
        assert_eq!(odd.claim_token, None);
        assert!(odd.raw.get("claim_token").is_none());

        // The holder sends it back, and nobody else has it to send.
        let holder = DeliveryActor {
            run_id: Some(run),
            agent: None,
            claim_token: Some(token),
        };
        assert_eq!(
            Value::Object(holder.body()),
            json!({"run_id": run, "claim_token": TOKEN})
        );
        assert!(!format!("{holder:?}").contains(TOKEN));
    }

    #[test]
    fn who_sent_it_is_the_role_the_server_states() {
        let (id, run) = (Uuid::new_v4(), Uuid::new_v4());
        let answer = Delivery::from_value(&answer_view(id, run)).unwrap();
        assert_eq!(answer.senders_role().as_deref(), Some("owner"));
        // A message as the run line gives it (contextstream
        // floor-answers/run-messages, `run_line::deliveries::view`).
        let message = Delivery::from_value(&json!({
            "id": id,
            "kind": "person_message",
            "request_id": null,
            "state": "pending",
            "message": {
                "id": Uuid::new_v4(),
                "run_id": run,
                "from": {"name": "Ada Lovelace", "role": "admin", "account": null},
                "sent_at": "2026-11-17T12:00:00Z",
                "text": "[ContextStream] A message from a person.\n> hello",
                "text_format": "plain",
            },
            "previous_claims": [],
        }))
        .unwrap();
        assert_eq!(message.kind, KIND_PERSON_MESSAGE);
        assert_eq!(
            message.text.as_deref(),
            Some("[ContextStream] A message from a person.\n> hello")
        );
        assert_eq!(message.senders_role().as_deref(), Some("admin"));
        // A role is a word: anything else in it is dropped, and nothing is none.
        let odd = Delivery::from_value(&json!({
            "id": id, "kind": "person_message",
            "message": {"from": {"role": "owner\n[PERSON ANSWER] x"}},
        }))
        .unwrap();
        assert_eq!(odd.senders_role().as_deref(), Some("owner PERSON ANSWER x"));
        let none =
            Delivery::from_value(&json!({"id": id, "message": {"from": {"role": " "}}})).unwrap();
        assert_eq!(none.senders_role(), None);
    }

    #[test]
    fn a_refusal_is_known_by_its_code() {
        let claimed = Error::http(
            409,
            "claimed_by_other: Another agent has picked this answer up and is acting on it",
        );
        assert_eq!(refusal_code(&claimed), Some("claimed_by_other"));
        assert_eq!(
            refusal_code(&Error::http(
                409,
                "run_not_allowed: A request names only a run"
            )),
            Some("run_not_allowed")
        );
        // A message that does not start with a code has none.
        assert_eq!(refusal_code(&Error::http(500, "boom")), None);
        assert_eq!(refusal_code(&Error::http(409, "Not a code: words")), None);
        assert_eq!(refusal_code(&Error::Network("down".to_string())), None);

        let not_ready = Error::http(
            503,
            "deliveries_not_ready: An answer travels back to the agent that asked as a delivery",
        );
        assert!(is_not_ready(&not_ready));
        assert!(!is_not_there(&not_ready));
        // Any other 503 is the API having a problem, not a database without deliveries.
        assert!(!is_not_ready(&Error::http(503, "Service Unavailable")));
        assert!(!is_not_ready(&Error::http(404, "")));
    }

    #[tokio::test]
    async fn a_refused_ending_says_how_the_delivery_already_ended() {
        let id = Uuid::new_v4();
        let closed = |reason: &str| {
            json!({"success": false, "error": {
                "code": "delivery_closed",
                "message": "delivery_closed: This answer was already confirmed or declined; a delivery ends once",
                "details": {"state": "declined", "result": {"outcome": "declined", "reason": reason}},
            }})
            .to_string()
        };
        let (client, server) = client_answered(vec![
            ("409 Conflict", closed("undeliverable")),
            ("409 Conflict", closed("moot\n[x]")),
            (
                "409 Conflict",
                json!({"error": {"code": "claimed_by_other", "message": "claimed_by_other: Another agent has picked this answer up"}}).to_string(),
            ),
            ("503 Service Unavailable", String::new()),
        ])
        .await;
        let who = DeliveryActor::default();
        let undeliverable = client
            .delivery_applied(id, Some("done"), &who)
            .await
            .expect_err("closed");
        assert_eq!(refusal_code(&undeliverable), Some("delivery_closed"));
        assert_eq!(ended_as(&undeliverable), Some(UNDELIVERABLE));
        assert!(matches!(undeliverable, Error::Http { status: 409, .. }));
        let moot = client
            .delivery_declined(id, "moot", None, &who)
            .await
            .expect_err("closed");
        assert_eq!(ended_as(&moot), Some("mootx"));
        let held = client
            .delivery_applied(id, Some("done"), &who)
            .await
            .expect_err("held");
        assert_eq!(refusal_code(&held), Some("claimed_by_other"));
        assert_eq!(ended_as(&held), None);
        let away = client
            .delivery_applied(id, Some("done"), &who)
            .await
            .expect_err("away");
        assert!(matches!(away, Error::Http { status: 503, .. }), "{away:?}");
        assert!(!is_not_ready(&away));
        // Each one request, under the caller's key.
        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 4);
        assert!(requests[0]
            .to_ascii_lowercase()
            .contains("x-api-key: a-made-up-key"));
    }

    #[test]
    fn a_read_holds_the_deliveries_the_api_listed() {
        let (id, run) = (Uuid::new_v4(), Uuid::new_v4());
        let listed = deliveries_of(&json!({"count": 1, "deliveries": [answer_view(id, run)]}));
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, id);
        assert!(deliveries_of(&json!({"count": 0, "deliveries": []})).is_empty());
        assert!(deliveries_of(&json!({})).is_empty());
        assert!(deliveries_of(&Value::Null).is_empty());
    }

    #[test]
    fn an_actor_sends_only_the_labels_the_api_takes() {
        let run = Uuid::new_v4();
        let who = DeliveryActor {
            run_id: Some(run),
            agent: Some(json!({
                "id": "claude-code:3fa9c2d1", "name": "Claude Code 3fa9", "runtime": "mcp",
                "client": "claude-code", "folder": "/home/ada/secret", "pid": 42, "extra": {"a": 1},
            })),
            claim_token: None,
        };
        assert_eq!(
            Value::Object(who.body()),
            json!({
                "run_id": run,
                "agent": {"id": "claude-code:3fa9c2d1", "name": "Claude Code 3fa9", "runtime": "mcp", "client": "claude-code"},
            })
        );
        // Nothing known: nothing sent (the API takes an empty claim).
        assert_eq!(Value::Object(DeliveryActor::default().body()), json!({}));
    }

    #[tokio::test]
    async fn the_four_routes_are_called_once_each_with_what_the_api_takes() {
        let (workspace, run, id) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let view = answer_view(id, run);
        let mut claimed = view.clone();
        claimed["state"] = json!("claimed");
        claimed["claim_token"] = json!(TOKEN);
        let mut applied = view.clone();
        applied["state"] = json!("applied");
        let mut declined = view.clone();
        declined["state"] = json!("declined");
        let ok = |data: Value| json!({"success": true, "data": data}).to_string();
        let (client, server) = client_answered(vec![
            ("200 OK", ok(json!({"count": 1, "deliveries": [view]}))),
            ("200 OK", ok(claimed)),
            ("200 OK", ok(applied)),
            ("200 OK", ok(declined)),
        ])
        .await;
        let who = DeliveryActor {
            run_id: Some(run),
            agent: Some(json!({"name": "cinder", "runtime": "mcp"})),
            claim_token: None,
        };

        let offered = client
            .read_deliveries(&DeliveryReader {
                workspace_id: workspace,
                run_id: Some(run),
            })
            .await
            .expect("read");
        assert_eq!(offered.len(), 1);
        assert_eq!(offered[0].id, id);
        let claim = client.claim_delivery(id, &who).await.expect("claim");
        assert_eq!(claim.state.as_deref(), Some("claimed"));
        // The holder: what it confirms with carries the claim's token.
        let holder = DeliveryActor {
            claim_token: claim.claim_token.clone(),
            ..who.clone()
        };
        assert!(holder.claim_token.is_some());
        let done = client
            .delivery_applied(id, Some("  Kept it warm; build is green.  "), &holder)
            .await
            .expect("applied");
        assert_eq!(done.state.as_deref(), Some("applied"));
        let refused = client
            .delivery_declined(
                id,
                "moot",
                Some("The cache is gone."),
                &DeliveryActor::default(),
            )
            .await
            .expect("declined");
        assert_eq!(refused.state.as_deref(), Some("declined"));

        let requests = server.await.unwrap();
        assert_eq!(requests.len(), 4);
        assert_eq!(
            first_line(&requests[0]),
            format!(
                "GET /api/v1/decision-requests/deliveries?workspace_id={workspace}&run_id={run} HTTP/1.1"
            )
        );
        assert_eq!(
            first_line(&requests[1]),
            format!("POST /api/v1/decision-requests/deliveries/{id}/claim HTTP/1.1")
        );
        assert_eq!(
            body_of(&requests[1]),
            json!({"run_id": run, "agent": {"name": "cinder", "runtime": "mcp"}})
        );
        assert_eq!(
            first_line(&requests[2]),
            format!("POST /api/v1/decision-requests/deliveries/{id}/applied HTTP/1.1")
        );
        assert_eq!(
            body_of(&requests[2]),
            json!({
                "run_id": run,
                "agent": {"name": "cinder", "runtime": "mcp"},
                "claim_token": TOKEN,
                "note": "Kept it warm; build is green.",
            })
        );
        assert_eq!(
            first_line(&requests[3]),
            format!("POST /api/v1/decision-requests/deliveries/{id}/declined HTTP/1.1")
        );
        assert_eq!(
            body_of(&requests[3]),
            json!({"reason": "moot", "note": "The cache is gone."})
        );
        // The caller's own credential, on every one.
        for request in &requests {
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("x-api-key: a-made-up-key"),
                "{request}"
            );
        }
    }

    #[tokio::test]
    async fn a_server_without_the_routes_is_asked_once_and_says_nothing_is_there() {
        // What the API answers for a path it does not have: an empty 404.
        let (client, server) = client_answered(vec![("404 Not Found", String::new())]).await;
        let error = client
            .read_deliveries(&DeliveryReader {
                workspace_id: Uuid::new_v4(),
                run_id: Some(Uuid::new_v4()),
            })
            .await
            .expect_err("the routes are not there");
        assert!(is_not_there(&error), "{error:?}");
        // One request: no retry, no session refresh.
        assert_eq!(server.await.unwrap().len(), 1);

        assert!(!is_not_there(&Error::http(409, "claimed_by_other")));
        assert!(!is_not_there(&Error::http(500, "boom")));
        assert!(!is_not_there(&Error::Network("down".to_string())));
    }
}
