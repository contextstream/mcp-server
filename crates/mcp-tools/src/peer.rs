//! What a tool call can say to the client while it runs.
//!
//! MCP lets a server send the client a notification or a request in the middle of a tool
//! call: `notifications/progress` for a call that carried a `progressToken` in its `_meta`,
//! and `elicitation/create` (MCP 2025-06-18) to ask the person something directly, when the
//! client declared the `elicitation` capability at `initialize`.
//!
//! A transport that can carry them (stdio) runs each tool call inside
//! [`run_with_call_peer`]; a tool reads [`current`] and falls back when there is none (the
//! hosted gateway answers each request with one JSON body, so it has none): no progress, and
//! an explicit accepted price instead of a question.
//!
//! The stateless revision (2026-07-28, what Claude Code speaks) has no session in which the
//! server could send a request. A question goes back as the call's own result instead
//! (`resultType: "input_required"` with `inputRequests` and `requestState`); the client asks
//! the person and sends the same call again with their answers in `inputResponses` and the
//! `requestState` it was given. [`CallPeer::with_inline_input`] carries those answers into
//! the call, and [`CallPeer::ask`] either reads the answer or records the question, which
//! the transport turns into that result ([`CallPeer::take_input_required`]).

use async_trait::async_trait;
use serde_json::{json, Map, Value};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How a transport sends to the client.
#[async_trait]
pub trait PeerChannel: Send + Sync {
    /// Send a notification. Best effort: a failure is only logged.
    async fn notify(&self, method: &str, params: Value);

    /// Send a request and wait for its answer: the `result` object, or why there is none
    /// (the client answered with an error, went away, or did not answer within `timeout`).
    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> std::result::Result<Value, String>;
}

/// A transport that answers each call with one body (the hosted gateway): nothing reaches
/// the client mid-call, so progress is dropped and only a stateless call's own result can
/// carry a question.
pub struct NoLiveChannel;

#[async_trait]
impl PeerChannel for NoLiveChannel {
    async fn notify(&self, _method: &str, _params: Value) {}

    async fn request(
        &self,
        _method: &str,
        _params: Value,
        _timeout: Duration,
    ) -> std::result::Result<Value, String> {
        Err("the client can't be reached in the middle of this call".to_string())
    }
}

/// The client as one tool call sees it.
#[derive(Clone)]
pub struct CallPeer {
    channel: Arc<dyn PeerChannel>,
    progress_token: Option<Value>,
    elicitation: bool,
    /// The last progress value sent: MCP asks that each be larger than the one before.
    last_progress: Arc<Mutex<Option<f64>>>,
    /// A stateless call's answers, and the question it ends with when it needs one.
    inline: Option<Arc<InlineInput>>,
    /// The protocol revision the client asked for at `initialize` (its newest), which says
    /// which form fields it can show.
    client_protocol: Option<String>,
}

/// The first revision whose elicitation forms have multi-select fields (an array of
/// enumerated strings), MCP 2025-11-25.
pub const MULTI_SELECT_PROTOCOL: &str = "2025-11-25";

/// What a stateless (2026-07-28) call carries back from the person, and what it asks.
#[derive(Debug, Default)]
struct InlineInput {
    responses: Map<String, Value>,
    request_state: Option<String>,
    asked: Mutex<Option<Value>>,
}

tokio::task_local! {
    static CALL_PEER: CallPeer;
}

/// Run `future` (one tool call) with `peer` as its client.
pub async fn run_with_call_peer<F: Future>(peer: CallPeer, future: F) -> F::Output {
    CALL_PEER.scope(peer, future).await
}

/// The client of the tool call running now, when its transport can reach it.
pub fn current() -> Option<CallPeer> {
    CALL_PEER.try_with(Clone::clone).ok()
}

/// What the person answered to an elicitation.
#[derive(Debug, Clone, PartialEq)]
pub enum Elicited {
    /// They accepted, with the form's content (an object, possibly empty).
    Accepted(Map<String, Value>),
    /// They said no.
    Declined,
    /// They closed the question without answering.
    Cancelled,
    /// The question could not be asked or got no answer.
    Unanswered(String),
    /// Asked through the call's own result (stateless revision): the tool returns now, and
    /// the client calls again with the answer.
    Pending,
}

impl CallPeer {
    pub fn new(
        channel: Arc<dyn PeerChannel>,
        progress_token: Option<Value>,
        elicitation: bool,
    ) -> Self {
        Self {
            channel,
            progress_token: progress_token.filter(|token| token.is_string() || token.is_number()),
            elicitation,
            last_progress: Arc::new(Mutex::new(None)),
            inline: None,
            client_protocol: None,
        }
    }

    /// The protocol revision the client asked for at `initialize`.
    pub fn with_client_protocol(mut self, protocol: Option<&str>) -> Self {
        self.client_protocol = protocol
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string);
        self
    }

    /// The protocol revision an `initialize`'s params ask for.
    pub fn initialize_protocol(initialize_params: &Value) -> Option<String> {
        initialize_params
            .get("protocolVersion")
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    /// Whether a form may hold a multi-select field: a stateless (2026-07-28) call, or a
    /// client that asked for 2025-11-25 or later. Revisions are dates, so they order as text.
    pub fn can_multi_select(&self) -> bool {
        self.inline.is_some()
            || self
                .client_protocol
                .as_deref()
                .is_some_and(|protocol| protocol >= MULTI_SELECT_PROTOCOL)
    }

    /// A stateless call whose client can ask its person: its `inputResponses` and
    /// `requestState` (from the call's params), for [`Self::ask`].
    pub fn with_inline_input(mut self, params: &Value) -> Self {
        self.inline = Some(Arc::new(InlineInput {
            responses: params
                .get("inputResponses")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
            request_state: params
                .get("requestState")
                .and_then(Value::as_str)
                .map(str::to_string),
            asked: Mutex::new(None),
        }));
        self
    }

    /// Whether a stateless request's capabilities (`_meta`'s
    /// `io.modelcontextprotocol/clientCapabilities`) say the client can ask its person.
    pub fn stateless_client_can_elicit(params: &Value) -> bool {
        params
            .get("_meta")
            .and_then(|meta| meta.get("io.modelcontextprotocol/clientCapabilities"))
            .and_then(|caps| caps.get("elicitation"))
            .is_some_and(Value::is_object)
    }

    /// Ask the person `message` with a form (`requested_schema`), under `key`, for the
    /// state `request_state` (what the question is about; an answer given for another state
    /// is not taken). In a stateless call the answer comes from the call's own
    /// `inputResponses`, or the question is recorded and [`Elicited::Pending`] returned;
    /// otherwise it is asked live ([`Self::elicit`]).
    pub async fn ask(
        &self,
        key: &str,
        message: &str,
        requested_schema: Value,
        request_state: &str,
        timeout: Duration,
    ) -> Elicited {
        let Some(inline) = &self.inline else {
            return self.elicit(message, requested_schema, timeout).await;
        };
        if inline.request_state.as_deref() == Some(request_state) {
            if let Some(answer) = inline.responses.get(key) {
                return read_elicitation(answer);
            }
        }
        let question = json!({
            "resultType": "input_required",
            "inputRequests": {
                key: {
                    "method": "elicitation/create",
                    "params": {"mode": "form", "message": message, "requestedSchema": requested_schema},
                }
            },
            "requestState": request_state,
        });
        *inline
            .asked
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(question);
        Elicited::Pending
    }

    /// The question this call ended with, for the transport to send as the call's result
    /// (`resultType`, `inputRequests`, `requestState`). Taken once.
    pub fn take_input_required(&self) -> Option<Value> {
        self.inline.as_ref().and_then(|inline| {
            inline
                .asked
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
        })
    }

    /// The progress token of a `tools/call`'s params (`params._meta.progressToken`).
    pub fn progress_token_of(params: &Value) -> Option<Value> {
        params
            .pointer("/_meta/progressToken")
            .filter(|token| token.is_string() || token.is_number())
            .cloned()
    }

    /// Whether an `initialize`'s params declare the elicitation capability.
    pub fn client_can_elicit(initialize_params: &Value) -> bool {
        initialize_params
            .pointer("/capabilities/elicitation")
            .is_some_and(Value::is_object)
    }

    /// Whether this call reports progress (the client sent a progress token).
    pub fn reports_progress(&self) -> bool {
        self.progress_token.is_some()
    }

    /// Whether the person can be asked: live, or through the call's result.
    pub fn can_elicit(&self) -> bool {
        self.elicitation || self.inline.is_some()
    }

    /// Report progress (`total` when known, for example 100 for a percentage). A value not
    /// above the last one sent is raised just above it, since MCP asks each to increase.
    /// Nothing is sent when the call carried no progress token.
    pub async fn progress(&self, progress: f64, total: Option<f64>, message: Option<&str>) {
        let Some(token) = self.progress_token.clone() else {
            return;
        };
        let value = {
            let mut last = self
                .last_progress
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let value = match *last {
                Some(previous) if progress <= previous => previous + 0.01,
                _ => progress,
            };
            *last = Some(value);
            value
        };
        let mut params = json!({ "progressToken": token, "progress": value });
        if let Some(total) = total {
            params["total"] = json!(total.max(value));
        }
        if let Some(message) = message.filter(|m| !m.trim().is_empty()) {
            params["message"] = json!(message);
        }
        self.channel.notify("notifications/progress", params).await;
    }

    /// Ask the person through a form (`elicitation/create`): `message` says what is asked,
    /// `requested_schema` is the flat object schema of the answer. Without the capability the
    /// answer is [`Elicited::Unanswered`].
    pub async fn elicit(
        &self,
        message: &str,
        requested_schema: Value,
        timeout: Duration,
    ) -> Elicited {
        if !self.elicitation {
            return Elicited::Unanswered("the client cannot ask the person".to_string());
        }
        let params = json!({ "message": message, "requestedSchema": requested_schema });
        match self
            .channel
            .request("elicitation/create", params, timeout)
            .await
        {
            Ok(result) => read_elicitation(&result),
            Err(reason) => Elicited::Unanswered(reason),
        }
    }
}

/// Read an `elicitation/create` result: `{action: accept|decline|cancel, content?}`.
pub fn read_elicitation(result: &Value) -> Elicited {
    match result.get("action").and_then(Value::as_str) {
        Some("accept") => Elicited::Accepted(
            result
                .get("content")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
        ),
        Some("decline") => Elicited::Declined,
        Some("cancel") => Elicited::Cancelled,
        _ => Elicited::Unanswered("the client's answer had no action".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Recorder {
        sent: Mutex<Vec<(String, Value)>>,
        answer: Mutex<Option<std::result::Result<Value, String>>>,
    }

    #[async_trait]
    impl PeerChannel for Recorder {
        async fn notify(&self, method: &str, params: Value) {
            self.sent.lock().unwrap().push((method.to_string(), params));
        }

        async fn request(
            &self,
            method: &str,
            params: Value,
            _timeout: Duration,
        ) -> std::result::Result<Value, String> {
            self.sent.lock().unwrap().push((method.to_string(), params));
            self.answer
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| Err("no answer".to_string()))
        }
    }

    #[tokio::test]
    async fn progress_rises_and_needs_a_token() {
        let recorder = Arc::new(Recorder::default());
        let silent = CallPeer::new(recorder.clone(), None, false);
        silent.progress(10.0, Some(100.0), Some("Making")).await;
        assert!(recorder.sent.lock().unwrap().is_empty());

        let peer = CallPeer::new(recorder.clone(), Some(json!("tok-1")), false);
        assert!(peer.reports_progress());
        peer.progress(10.0, Some(100.0), Some("Making takes")).await;
        peer.progress(10.0, Some(100.0), Some("Judging")).await;
        peer.progress(5.0, Some(100.0), None).await;
        let sent = recorder.sent.lock().unwrap().clone();
        assert_eq!(sent.len(), 3);
        assert_eq!(sent[0].0, "notifications/progress");
        assert_eq!(
            sent[0].1,
            json!({"progressToken": "tok-1", "progress": 10.0, "total": 100.0, "message": "Making takes"})
        );
        let values: Vec<f64> = sent
            .iter()
            .map(|(_, p)| p["progress"].as_f64().unwrap())
            .collect();
        assert!(values.windows(2).all(|w| w[1] > w[0]), "{values:?}");
        assert!(sent[2].1.get("message").is_none());
    }

    #[tokio::test]
    async fn elicitation_needs_the_capability_and_reads_the_answer() {
        let recorder = Arc::new(Recorder::default());
        let unable = CallPeer::new(recorder.clone(), None, false);
        assert!(matches!(
            unable
                .elicit("Go?", json!({"type": "object"}), Duration::from_secs(1))
                .await,
            Elicited::Unanswered(_)
        ));
        assert!(recorder.sent.lock().unwrap().is_empty());

        let peer = CallPeer::new(recorder.clone(), None, true);
        *recorder.answer.lock().unwrap() =
            Some(Ok(json!({"action": "accept", "content": {"go": true}})));
        let answer = peer
            .elicit(
                "Spend up to 300 credits?",
                json!({"type": "object", "properties": {}}),
                Duration::from_secs(1),
            )
            .await;
        let Elicited::Accepted(content) = answer else {
            panic!("accepted");
        };
        assert_eq!(content["go"], json!(true));
        let sent = recorder.sent.lock().unwrap().clone();
        assert_eq!(sent[0].0, "elicitation/create");
        assert_eq!(sent[0].1["message"], "Spend up to 300 credits?");

        assert_eq!(
            read_elicitation(&json!({"action": "decline"})),
            Elicited::Declined
        );
        assert_eq!(
            read_elicitation(&json!({"action": "cancel"})),
            Elicited::Cancelled
        );
        assert!(matches!(
            read_elicitation(&json!({})),
            Elicited::Unanswered(_)
        ));
    }

    #[tokio::test]
    async fn a_stateless_call_asks_through_its_result_and_reads_the_answer() {
        let recorder = Arc::new(Recorder::default());
        let first = CallPeer::new(recorder.clone(), None, false).with_inline_input(&json!({}));
        assert!(first.can_elicit());
        let asked = first
            .ask(
                "price",
                "Spend up to 300?",
                json!({"type": "object"}),
                "plan:1:300",
                Duration::from_secs(1),
            )
            .await;
        assert_eq!(asked, Elicited::Pending);
        let question = first.take_input_required().expect("the question");
        assert_eq!(question["resultType"], "input_required");
        assert_eq!(question["requestState"], "plan:1:300");
        assert_eq!(
            question["inputRequests"]["price"]["method"],
            "elicitation/create"
        );
        assert_eq!(
            question["inputRequests"]["price"]["params"]["message"],
            "Spend up to 300?"
        );
        assert!(first.take_input_required().is_none());
        // Nothing went over the live channel.
        assert!(recorder.sent.lock().unwrap().is_empty());

        let answered = CallPeer::new(recorder.clone(), None, false).with_inline_input(&json!({
            "inputResponses": {"price": {"action": "accept", "content": {"confirm": true}}},
            "requestState": "plan:1:300",
        }));
        let answer = answered
            .ask(
                "price",
                "Spend up to 300?",
                json!({"type": "object"}),
                "plan:1:300",
                Duration::from_secs(1),
            )
            .await;
        assert!(matches!(answer, Elicited::Accepted(ref c) if c["confirm"] == json!(true)));
        assert!(answered.take_input_required().is_none());

        // An answer given for another state is asked again.
        let stale = CallPeer::new(recorder, None, false).with_inline_input(&json!({
            "inputResponses": {"price": {"action": "accept"}},
            "requestState": "plan:1:250",
        }));
        let again = stale
            .ask(
                "price",
                "Spend up to 300?",
                json!({"type": "object"}),
                "plan:1:300",
                Duration::from_secs(1),
            )
            .await;
        assert_eq!(again, Elicited::Pending);
        assert!(CallPeer::stateless_client_can_elicit(&json!({"_meta": {
            "io.modelcontextprotocol/clientCapabilities": {"elicitation": {"form": {}}}
        }})));
        assert!(!CallPeer::stateless_client_can_elicit(
            &json!({"_meta": {}})
        ));
    }

    #[test]
    fn reads_the_token_and_the_capability() {
        assert_eq!(
            CallPeer::progress_token_of(&json!({"_meta": {"progressToken": 7}})),
            Some(json!(7))
        );
        assert_eq!(
            CallPeer::progress_token_of(&json!({"_meta": {"progressToken": {"x": 1}}})),
            None
        );
        assert!(CallPeer::client_can_elicit(
            &json!({"capabilities": {"elicitation": {"form": {}}}})
        ));
        assert!(CallPeer::client_can_elicit(
            &json!({"capabilities": {"elicitation": {}}})
        ));
        assert!(!CallPeer::client_can_elicit(&json!({"capabilities": {}})));
    }

    #[test]
    fn multi_select_forms_need_2025_11_25_or_a_stateless_call() {
        let recorder: Arc<dyn PeerChannel> = Arc::new(Recorder::default());
        let live = |protocol: Option<&str>| {
            CallPeer::new(recorder.clone(), None, true).with_client_protocol(protocol)
        };
        assert!(!live(None).can_multi_select());
        assert!(!live(Some("2024-11-05")).can_multi_select());
        assert!(!live(Some("2025-06-18")).can_multi_select());
        assert!(live(Some("2025-11-25")).can_multi_select());
        assert!(live(Some("2026-07-28")).can_multi_select());
        assert!(!live(Some("  ")).can_multi_select());
        let stateless = CallPeer::new(recorder.clone(), None, false).with_inline_input(&json!({}));
        assert!(stateless.can_multi_select());
        assert_eq!(
            CallPeer::initialize_protocol(&json!({"protocolVersion": "2025-11-25"})).as_deref(),
            Some("2025-11-25")
        );
        assert_eq!(CallPeer::initialize_protocol(&json!({})), None);
    }

    #[tokio::test]
    async fn the_peer_is_set_only_inside_its_call() {
        assert!(current().is_none());
        let peer = CallPeer::new(Arc::new(Recorder::default()), Some(json!("t")), true);
        let inside = run_with_call_peer(peer, async { current().map(|p| p.can_elicit()) }).await;
        assert_eq!(inside, Some(true));
        assert!(current().is_none());
    }
}
