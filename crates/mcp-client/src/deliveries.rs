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
//! - `POST /decision-requests/deliveries/:id/claim` `{run_id?, agent?}`: the
//!   delivery is this agent's for the length of a lease. While another agent
//!   holds it the API answers 409;
//! - `POST /decision-requests/deliveries/:id/applied` `{note?, run_id?,
//!   agent?}`: the agent acted on it; `note` is its reply to the person. Said
//!   again, the API answers the stored result;
//! - `POST /decision-requests/deliveries/:id/declined` `{reason: moot |
//!   cannot_apply, note?, run_id?, agent?}`.
//!
//! The routes exist only where the API has them switched on for the
//! workspace; everywhere else they answer what an unknown path answers (an
//! empty 404). So every call here is made once, with no session refresh and a
//! short timeout, and its failure is the caller's to keep quiet about
//! ([`is_not_there`]).
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

/// The agent acting on a delivery, as it names itself: its run, and the
/// labels its reports carry. Both are optional to the API.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeliveryActor {
    pub run_id: Option<Uuid>,
    pub agent: Option<Value>,
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
    /// Everything the server sent.
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
    pub fn from_value(value: &Value) -> Option<Self> {
        let id = Uuid::parse_str(value.get("id")?.as_str()?.trim()).ok()?;
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
            raw: value.clone(),
        })
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
        raw: answer,
    })
}

/// Not built yet: the calls are named and typed, and none is made.
fn not_built<T>() -> Result<T> {
    Err(Error::Tool("run deliveries are not built yet".to_string()))
}

impl ContextStreamClient {
    /// What is deliverable to the reader's run. Endpoint:
    /// `GET /decision-requests/deliveries?workspace_id=&run_id=`.
    pub async fn read_deliveries(&self, reader: &DeliveryReader) -> Result<Vec<Delivery>> {
        let _ = (reader.query(), once(), deliveries_of(&Value::Null));
        not_built()
    }

    /// Take a delivery for the length of a lease. Endpoint:
    /// `POST /decision-requests/deliveries/:id/claim`.
    pub async fn claim_delivery(&self, id: Uuid, who: &DeliveryActor) -> Result<Delivery> {
        let _ = answered(id, Value::Object(who.body()));
        not_built()
    }

    /// Confirm a delivery: the agent acted on it, and `note` is its reply to
    /// the person. Endpoint: `POST /decision-requests/deliveries/:id/applied`.
    pub async fn delivery_applied(
        &self,
        id: Uuid,
        note: Option<&str>,
        who: &DeliveryActor,
    ) -> Result<Delivery> {
        let _ = (id, note, who);
        not_built()
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
        let _ = (id, reason, note, who);
        not_built()
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
        let done = client
            .delivery_applied(id, Some("  Kept it warm; build is green.  "), &who)
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
