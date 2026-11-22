//! The stream a hosted tool's call is answered on, over a WebSocket.
//!
//! The API names the endpoint and a one-call token. This client opens the
//! socket with that token (never its API key), sends a `hello` describing the
//! call and the checkout, then the pack of the changed files, and answers what
//! the service asks for until it sends the result.
//!
//! A frame is one type byte, a four-byte big-endian length and the payload:
//! `J` is JSON, `D` binary data (a git pack, or a zlib-compressed listing),
//! `O` output text. Frames travel as binary messages of at most 1 MiB each.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::stream::SplitSink;
use futures::{SinkExt, StreamExt};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc::UnboundedSender, Mutex};
use tokio_tungstenite::tungstenite::error::ProtocolError;
use tokio_tungstenite::tungstenite::handshake::client::generate_key;
use tokio_tungstenite::tungstenite::handshake::derive_accept_key;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
use tokio_tungstenite::tungstenite::{Error as WsError, Message};
use tokio_tungstenite::WebSocketStream;

use crate::checkout_snapshot::CheckoutSnapshot;

/// The protocol name an API answer must give for its stream.
pub const PROTOCOL: &str = "frames/1";

/// The most one frame may carry.
pub const FRAME_MAX: usize = 64 << 20;

/// The most one message to the service carries: its runtime closes a socket
/// on a larger message, so a frame goes in pieces.
pub const MESSAGE_MAX: usize = 1 << 20;

/// How long a call waits for the service's first word after its hello. Silence
/// this long means the call is stuck somewhere between here and the worker.
pub const FIRST_FRAME_WAIT: Duration = Duration::from_secs(20);

/// Time allowed past the limits a call declares: the service closes a call at
/// its limits plus 30 seconds, and this client gives up shortly after.
pub const DEADLINE_SLACK: Duration = Duration::from_secs(45);

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

/// Why a call on a stream did not end in a result.
#[derive(Debug, thiserror::Error)]
pub enum ExchangeError {
    #[error("the service did not answer in {0:?}")]
    Silent(Duration),
    #[error("the connection ended before a result")]
    Ended,
    #[error("it ran past its limits")]
    Deadline,
    #[error("the service said something this client does not understand ({0})")]
    Protocol(String),
    #[error("the connection failed ({0})")]
    Wire(String),
    #[error("the checkout could not be offered ({0})")]
    Snapshot(String),
}

/// What a running call tells its caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Output text, as it arrives.
    Output(Vec<u8>),
    /// The service is still working on it.
    Heartbeat,
    /// Stage progress from a multi-step check.
    StageProgress {
        stage: String,
        step: u64,
        total: u64,
        message: Option<String>,
    },
}

fn stage_progress_from_info(frame: &Value) -> Option<Event> {
    let stage = frame
        .get("stage")
        .and_then(Value::as_str)
        .and_then(display_text)
        .map(|s| s.chars().take(80).collect::<String>())?;
    let step = frame
        .get("step")
        .and_then(Value::as_u64)
        .filter(|&s| s >= 1)?;
    let total = frame
        .get("total")
        .and_then(Value::as_u64)
        .filter(|&t| t >= step)?;
    let message = frame
        .get("message")
        .and_then(Value::as_str)
        .and_then(display_text);
    Some(Event::StageProgress {
        stage,
        step,
        total,
        message,
    })
}

/// The worker's explanation when it declined a run. A command that ran and
/// exited with 75 still has its own result.
pub fn declined_reason(frame: &Value) -> Option<String> {
    if frame.get("ran").and_then(Value::as_bool) == Some(true)
        || frame.get("exit").and_then(Value::as_i64) != Some(75)
    {
        return None;
    }
    ["refused", "reason"].into_iter().find_map(|key| {
        frame
            .get(key)
            .and_then(Value::as_str)
            .and_then(display_text)
    })
}

/// Bounded, single-line text from the service for a tool result or terminal.
pub fn display_text(value: &str) -> Option<String> {
    let mut at_space = true;
    let mut text: String = value
        .chars()
        .filter(|ch| {
            (!ch.is_control() || ch.is_whitespace())
                && !matches!(
                    ch,
                    '\u{061c}' | '\u{200e}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
                )
        })
        .filter_map(|ch| {
            let was_space = at_space;
            at_space = ch.is_whitespace();
            match (at_space, was_space) {
                (false, _) => Some(ch),
                (true, false) => Some(' '),
                (true, true) => None,
            }
        })
        .take(300)
        .collect();
    text.truncate(text.trim_end().len());
    (!text.is_empty()).then_some(text)
}

/// Explain the decline where the caller already says the command runs locally.
pub fn decline_explanation(reason: &str) -> &str {
    reason
        .strip_suffix(", so it runs in the session")
        .filter(|text| !text.is_empty())
        .unwrap_or(reason)
}

/// Whether `url` may receive a call: `wss`, or `ws` to this machine.
pub fn stream_url_allowed(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    match parsed.scheme() {
        "wss" => parsed.host_str().is_some(),
        "ws" => matches!(
            parsed.host_str(),
            Some("localhost") | Some("127.0.0.1") | Some("[::1]")
        ),
        _ => false,
    }
}

/// One frame, ready to send.
pub fn encode_frame(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.push(kind);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

/// Bytes received so far, cut into frames.
#[derive(Debug, Default)]
pub struct FrameBuffer {
    bytes: Vec<u8>,
}

impl FrameBuffer {
    pub fn push(&mut self, data: &[u8]) {
        self.bytes.extend_from_slice(data);
    }

    /// The next whole frame, if one has arrived.
    pub fn next_frame(&mut self) -> Result<Option<(u8, Vec<u8>)>, ExchangeError> {
        if self.bytes.len() < 5 {
            return Ok(None);
        }
        let size = u32::from_be_bytes([self.bytes[1], self.bytes[2], self.bytes[3], self.bytes[4]])
            as usize;
        if size > FRAME_MAX {
            return Err(ExchangeError::Protocol(format!(
                "a frame of {size} bytes is over the limit"
            )));
        }
        if self.bytes.len() < 5 + size {
            return Ok(None);
        }
        let kind = self.bytes[0];
        let payload = self.bytes[5..5 + size].to_vec();
        self.bytes.drain(..5 + size);
        Ok(Some((kind, payload)))
    }
}

/// Whether a variable may be forwarded to the service: a plain name that does
/// not look like it holds a credential.
pub fn env_name_forwardable(name: &str) -> bool {
    let plain = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !plain {
        return false;
    }
    let upper = name.to_ascii_uppercase();
    const SECRET_WORDS: [&str; 12] = [
        "KEY",
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "PASSWD",
        "CREDENTIAL",
        "AUTH",
        "COOKIE",
        "SESSION",
        "PRIVATE",
        "SIGNATURE",
        "CONTEXTSTREAM",
    ];
    !SECRET_WORDS.iter().any(|word| upper.contains(word))
}

/// The values of the variables a call names, for those that may be forwarded
/// and are set: at most 32 of them, each at most 4 KiB, none that look like a
/// URL with a login in it.
pub fn forwarded_env(
    request: &Value,
    lookup: impl Fn(&str) -> Option<String>,
) -> Map<String, Value> {
    let mut env = Map::new();
    let Some(names) = request.get("env_names").and_then(Value::as_array) else {
        return env;
    };
    for name in names.iter().filter_map(Value::as_str) {
        if env.len() >= 32 {
            break;
        }
        if !env_name_forwardable(name) {
            continue;
        }
        let Some(value) = lookup(name) else {
            continue;
        };
        if value.len() > 4096 || value.contains('\0') || has_login_in_url(&value) {
            continue;
        }
        env.insert(name.to_string(), Value::String(value));
    }
    env
}

/// The first variable the call names in `local_if_set` that is set here. The
/// API lists there the variables that change what the call's command does and
/// that it never asks the value of: a build tool's own options, another set of
/// dependencies, a search path of this machine. A call that names one that is
/// set is not made, and the command is run here instead; the names are looked
/// up on this machine only, and nothing is sent to decide it. Only plain names
/// count (letters, digits and `_`, at most 64 characters), the first 64 of them.
pub fn set_here(request: &Value, is_set: impl Fn(&str) -> bool) -> Option<String> {
    request
        .get("local_if_set")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter(|name| {
            !name.is_empty()
                && name.len() <= 64
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
        .take(64)
        .find(|name| is_set(name))
        .map(str::to_string)
}

/// Whether `value` holds `scheme://user:password@host`.
fn has_login_in_url(value: &str) -> bool {
    value.split("://").skip(1).any(|rest| {
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        authority
            .split_once('@')
            .is_some_and(|(login, _)| login.contains(':'))
    })
}

/// How the caller takes what a call produces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// The service makes the tool's result itself and sends no output as it
    /// goes (`tool_result`).
    ToolResult,
    /// The caller takes the output when the call ends, so the service may hold
    /// it until it has decided the result (`buffered`).
    Buffered,
    /// The caller shows output as it arrives.
    Streamed,
}

/// The `hello` for a call: what the API decided (`request`), what the
/// checkout is (`fields`), the forwarded variables, and how the caller takes
/// the output. Nothing else; the local environment is added by
/// [`with_environment`].
pub fn hello(
    request: &Value,
    fields: Map<String, Value>,
    env: Map<String, Value>,
    delivery: Delivery,
) -> Result<Value, String> {
    let text = |key: &str| {
        request
            .get(key)
            .and_then(Value::as_str)
            .ok_or_else(|| format!("the API's request has no {key}"))
    };
    let number = |key: &str| {
        request
            .get(key)
            .filter(|value| value.is_number())
            .cloned()
            .ok_or_else(|| format!("the API's request has no {key}"))
    };
    let mut hello = Map::new();
    hello.insert("type".into(), json!("hello"));
    hello.insert("v".into(), json!(1));
    hello.insert("id".into(), json!(text("id")?));
    hello.insert("repo_key".into(), json!(text("repo_key")?));
    hello.insert("cwd".into(), json!(text("cwd").unwrap_or("")));
    hello.insert("command".into(), json!(text("command")?));
    hello.insert("timeout_s".into(), number("timeout_s")?);
    hello.insert("wait_s".into(), number("wait_s")?);
    if let Some(pipefail) = request.get("pipefail").filter(|value| value.is_boolean()) {
        hello.insert("pipefail".into(), pipefail.clone());
    }
    for (key, value) in fields {
        hello.insert(key, value);
    }
    hello.insert("env".into(), Value::Object(env));
    match delivery {
        Delivery::ToolResult => {
            hello.insert("tool_result".into(), json!(true));
        }
        Delivery::Buffered => {
            hello.insert("buffered".into(), json!(true));
        }
        Delivery::Streamed => {}
    }
    Ok(Value::Object(hello))
}

/// `hello` with the description of the local environment
/// ([`crate::environment::describe`]) added as `environment`.
pub fn with_environment(mut hello: Value, environment: Option<Value>) -> Value {
    if let (Some(environment), Some(fields)) = (environment, hello.as_object_mut()) {
        fields.insert("environment".into(), environment);
    }
    hello
}

/// How long a call may take, as its request declares it.
pub fn deadline_of(request: &Value) -> Duration {
    let seconds = |key: &str| {
        request
            .get(key)
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite() && *value >= 0.0)
            .unwrap_or(0.0)
    };
    Duration::from_secs_f64(seconds("timeout_s") + seconds("wait_s").min(3600.0)) + DEADLINE_SLACK
}

/// Open the socket for a call. The call's token goes to `url` only: this
/// client's own credentials never do, and a redirect is never followed.
pub async fn connect(
    url: &str,
    token: &str,
) -> Result<WebSocketStream<reqwest::Upgraded>, ExchangeError> {
    if !stream_url_allowed(url) {
        return Err(ExchangeError::Protocol(
            "the stream is not a wss address".to_string(),
        ));
    }
    let mut http_url =
        reqwest::Url::parse(url).map_err(|e| ExchangeError::Protocol(e.to_string()))?;
    let scheme = if http_url.scheme() == "wss" {
        "https"
    } else {
        "http"
    };
    http_url
        .set_scheme(scheme)
        .map_err(|_| ExchangeError::Protocol("the stream address cannot be used".to_string()))?;
    let http = crate::http::client_builder()
        .redirect(reqwest::redirect::Policy::none())
        .http1_only()
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .map_err(|e| ExchangeError::Wire(e.without_url().to_string()))?;
    let key = generate_key();
    let request = http
        .get(http_url)
        .bearer_auth(token)
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", &key)
        .send();
    let response = match tokio::time::timeout(CONNECT_TIMEOUT, request).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => return Err(ExchangeError::Wire(error.without_url().to_string())),
        Err(_) => return Err(ExchangeError::Silent(CONNECT_TIMEOUT)),
    };
    if response.status() != reqwest::StatusCode::SWITCHING_PROTOCOLS {
        return Err(ExchangeError::Wire(format!(
            "the service answered HTTP {}",
            response.status().as_u16()
        )));
    }
    let expected = derive_accept_key(key.as_bytes());
    let accepted = response
        .headers()
        .get("sec-websocket-accept")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == expected);
    if !accepted {
        return Err(ExchangeError::Protocol(
            "the upgrade answer did not match".to_string(),
        ));
    }
    let upgraded = response
        .upgrade()
        .await
        .map_err(|e| ExchangeError::Wire(e.without_url().to_string()))?;
    let config = WebSocketConfig::default()
        .max_message_size(Some(FRAME_MAX + 1024))
        .max_frame_size(Some(FRAME_MAX + 1024));
    Ok(WebSocketStream::from_raw_socket(upgraded, Role::Client, Some(config)).await)
}

type Sink<S> = SplitSink<WebSocketStream<S>, Message>;

/// Send one frame, as messages of at most [`MESSAGE_MAX`]. The sink stays
/// locked for the whole frame, so two frames never interleave.
async fn send_frame<S>(sink: &Mutex<Sink<S>>, kind: u8, payload: &[u8]) -> Result<(), ExchangeError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let frame = encode_frame(kind, payload);
    let mut sink = sink.lock().await;
    for piece in frame.chunks(MESSAGE_MAX) {
        sink.send(Message::binary(piece.to_vec()))
            .await
            .map_err(|e| ExchangeError::Wire(e.to_string()))?;
    }
    Ok(())
}

/// Aborts its task when dropped, so a call that is given up on stops sending.
struct AbortOnDrop(tokio::task::JoinHandle<Result<(), ExchangeError>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Run one call on `ws`: send the hello and the pack of the changes, answer
/// what the service asks for of the checkout, report what it says on the way
/// through `events`, and return its `result` frame. The call is over by
/// `deadline`; the service gets [`FIRST_FRAME_WAIT`] to say its first word.
pub async fn exchange<S>(
    ws: WebSocketStream<S>,
    hello: &Value,
    snapshot: &CheckoutSnapshot,
    deadline: Duration,
    events: UnboundedSender<Event>,
) -> Result<Value, ExchangeError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let first_pack = snapshot
        .initial_pack()
        .await
        .map_err(|e| ExchangeError::Snapshot(e.to_string()))?;
    exchange_with_pack(ws, hello, first_pack, snapshot, deadline, events).await
}

/// [`exchange`] with the pack of the changes already built, for a caller that
/// builds it while the socket is being opened.
pub async fn exchange_with_pack<S>(
    ws: WebSocketStream<S>,
    hello: &Value,
    first_pack: Vec<u8>,
    snapshot: &CheckoutSnapshot,
    deadline: Duration,
    events: UnboundedSender<Event>,
) -> Result<Value, ExchangeError>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let hello_bytes =
        serde_json::to_vec(hello).map_err(|e| ExchangeError::Protocol(e.to_string()))?;
    let (sink, mut stream) = ws.split();
    let sink = Arc::new(Mutex::new(sink));
    let _sender = AbortOnDrop(tokio::spawn({
        let sink = sink.clone();
        async move {
            send_frame(&sink, b'J', &hello_bytes).await?;
            send_frame(&sink, b'D', &first_pack).await
        }
    }));

    let started = Instant::now();
    let mut heard = false;
    let mut buffer = FrameBuffer::default();
    loop {
        let left = deadline
            .checked_sub(started.elapsed())
            .filter(|left| !left.is_zero())
            .ok_or(ExchangeError::Deadline)?;
        let wait = if heard {
            left
        } else {
            left.min(FIRST_FRAME_WAIT)
        };
        let message = match tokio::time::timeout(wait, stream.next()).await {
            Err(_) if heard => return Err(ExchangeError::Deadline),
            Err(_) => return Err(ExchangeError::Silent(FIRST_FRAME_WAIT)),
            // A connection that is closed, or dropped without the closing exchange, is over.
            Ok(None)
            | Ok(Some(Err(
                WsError::ConnectionClosed
                | WsError::AlreadyClosed
                | WsError::Io(_)
                | WsError::Protocol(ProtocolError::ResetWithoutClosingHandshake),
            ))) => return Err(ExchangeError::Ended),
            Ok(Some(Err(error))) => return Err(ExchangeError::Wire(error.to_string())),
            Ok(Some(Ok(message))) => message,
        };
        match message {
            Message::Binary(bytes) => buffer.push(&bytes),
            Message::Close(_) => return Err(ExchangeError::Ended),
            _ => continue,
        }
        while let Some((kind, payload)) = buffer.next_frame()? {
            heard = true;
            match kind {
                b'O' => {
                    let _ = events.send(Event::Output(payload));
                }
                b'J' => {
                    let message: Value = serde_json::from_slice(&payload).map_err(|_| {
                        ExchangeError::Protocol("a frame that is not JSON".to_string())
                    })?;
                    match message.get("type").and_then(Value::as_str) {
                        Some("need") => {
                            let data = snapshot
                                .answer_need(&message)
                                .await
                                .map_err(|e| ExchangeError::Snapshot(e.to_string()))?;
                            send_frame(&sink, b'D', &data).await?;
                        }
                        Some("info") => {
                            let event =
                                stage_progress_from_info(&message).unwrap_or(Event::Heartbeat);
                            let _ = events.send(event);
                        }
                        Some("result") => {
                            // Closing is a courtesy; the call is done either way.
                            let _ = tokio::time::timeout(Duration::from_secs(2), async {
                                sink.lock().await.close().await
                            })
                            .await;
                            return Ok(message);
                        }
                        _ => {}
                    }
                }
                // Artifacts and frame types from a newer service are not needed here.
                _ => {}
            }
        }
    }
}

#[cfg(test)]
#[path = "frames_tests.rs"]
mod tests;
