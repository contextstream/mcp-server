//! The stdio client as a tool call reaches it mid-call (`mcp_tools::peer`).
//!
//! The stdio loop handles one client message at a time. For a tool to ask the person
//! something (`elicitation/create`) while its call runs, the client's answer must be read
//! while the loop is busy with that call. So a reader task reads every line: an answer to a
//! request this server sent goes straight to whoever is waiting for it, and everything else
//! (the client's requests and notifications) is queued for the loop in arrival order. Every
//! write, the loop's answers and a call's progress or question alike, goes through one lock,
//! so lines never interleave.

use async_trait::async_trait;
use mcp_tools::peer::PeerChannel;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tracing::debug;

type Answer = std::result::Result<Value, String>;
type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Answer>>>>;

/// Writes to the client, one whole line at a time.
pub(crate) struct LineWriter<W> {
    inner: tokio::sync::Mutex<W>,
}

impl<W: AsyncWrite + Unpin + Send> LineWriter<W> {
    pub(crate) fn new(writer: W) -> Self {
        Self {
            inner: tokio::sync::Mutex::new(writer),
        }
    }

    /// Write `line` and a newline, and flush.
    pub(crate) async fn write_line(&self, line: &str) -> std::io::Result<()> {
        let mut writer = self.inner.lock().await;
        writer.write_all(line.as_bytes()).await?;
        writer.write_all(b"\n").await?;
        writer.flush().await
    }
}

/// The client, for requests and notifications a tool call sends.
pub(crate) struct StdioPeer<W> {
    writer: Arc<LineWriter<W>>,
    pending: Pending,
    next_id: AtomicU64,
}

impl<W> StdioPeer<W> {
    pub(crate) fn new(writer: Arc<LineWriter<W>>) -> Self {
        Self {
            writer,
            pending: Arc::new(Mutex::new(HashMap::new())),
            next_id: AtomicU64::new(1),
        }
    }

    /// Where the reader delivers the client's answers.
    pub(crate) fn pending(&self) -> Pending {
        self.pending.clone()
    }
}

#[async_trait]
impl<W: AsyncWrite + Unpin + Send + Sync> PeerChannel for StdioPeer<W> {
    async fn notify(&self, method: &str, params: Value) {
        let line = json!({"jsonrpc": "2.0", "method": method, "params": params}).to_string();
        if let Err(error) = self.writer.write_line(&line).await {
            debug!(%error, method, "stdio: a notification could not be written");
        }
    }

    async fn request(&self, method: &str, params: Value, timeout: Duration) -> Answer {
        let id = format!(
            "contextstream-{}",
            self.next_id.fetch_add(1, Ordering::Relaxed)
        );
        let (sender, receiver) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(id.clone(), sender);
        let line =
            json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
        if let Err(error) = self.writer.write_line(&line).await {
            self.forget(&id);
            return Err(format!("the request could not be written: {error}"));
        }
        let answer = match tokio::time::timeout(timeout, receiver).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(_)) => Err("the client went away".to_string()),
            Err(_) => Err(format!(
                "no answer within {} seconds",
                timeout.as_secs().max(1)
            )),
        };
        self.forget(&id);
        answer
    }
}

impl<W> StdioPeer<W> {
    fn forget(&self, id: &str) {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(id);
    }
}

/// When `message` answers a request (an `id` with a `result` or an `error`, and no
/// `method`): its id as text, and the answer.
pub(crate) fn as_answer(message: &Value) -> Option<(String, Answer)> {
    let object = message.as_object()?;
    if object.contains_key("method") {
        return None;
    }
    let id = match object.get("id")? {
        Value::String(id) => id.clone(),
        Value::Number(id) => id.to_string(),
        _ => return None,
    };
    if let Some(result) = object.get("result") {
        return Some((id, Ok(result.clone())));
    }
    let error = object.get("error")?;
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("the client refused the request");
    Some((
        id,
        Err(format!("the client answered with an error: {message}")),
    ))
}

/// Read the client's lines until it closes stdin: answers to this server's requests go to
/// `pending`, every other non-empty line to `queue`.
pub(crate) async fn read_client<R: AsyncBufRead + Unpin>(
    reader: R,
    pending: Pending,
    queue: mpsc::Sender<String>,
) {
    let mut lines = reader.lines();
    loop {
        let line = match lines.next_line().await {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) => {
                debug!(%error, "stdio: reading the client stopped");
                break;
            }
        };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((id, answer)) = serde_json::from_str::<Value>(line)
            .ok()
            .as_ref()
            .and_then(as_answer)
        {
            let waiting = pending
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .remove(&id);
            match waiting {
                Some(sender) => {
                    let _ = sender.send(answer);
                }
                None => debug!(
                    id,
                    "stdio: an answer to no request this server is waiting on"
                ),
            }
            continue;
        }
        if queue.send(line.to_string()).await.is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, BufReader};

    #[test]
    fn tells_answers_from_requests() {
        assert_eq!(
            as_answer(
                &json!({"jsonrpc": "2.0", "id": "contextstream-1", "result": {"action": "accept"}})
            ),
            Some((
                "contextstream-1".to_string(),
                Ok(json!({"action": "accept"}))
            ))
        );
        assert!(matches!(
            as_answer(&json!({"jsonrpc": "2.0", "id": 4, "error": {"code": -1, "message": "no"}})),
            Some((id, Err(_))) if id == "4"
        ));
        assert_eq!(
            as_answer(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {}})),
            None
        );
        assert_eq!(
            as_answer(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})),
            None
        );
    }

    #[tokio::test]
    async fn a_question_mid_call_gets_its_answer_while_requests_queue() {
        // What the server writes.
        let (server_out, mut client_in) = tokio::io::duplex(64 * 1024);
        // What the client writes.
        let (mut client_out, server_in) = tokio::io::duplex(64 * 1024);

        let writer = Arc::new(LineWriter::new(server_out));
        let peer = Arc::new(StdioPeer::new(writer.clone()));
        let (queue_tx, mut queue_rx) = mpsc::channel(8);
        tokio::spawn(read_client(
            BufReader::new(server_in),
            peer.pending(),
            queue_tx,
        ));

        let asking = {
            let peer = peer.clone();
            tokio::spawn(async move {
                peer.request(
                    "elicitation/create",
                    json!({"message": "Go?"}),
                    Duration::from_secs(5),
                )
                .await
            })
        };

        // The client sees the question, sends another request first, then answers.
        let mut seen = vec![0u8; 4096];
        let read = client_in.read(&mut seen).await.unwrap();
        let question: Value = serde_json::from_slice(&seen[..read]).unwrap();
        assert_eq!(question["method"], "elicitation/create");
        let id = question["id"].as_str().unwrap().to_string();
        client_out
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"ping\"}\n")
            .await
            .unwrap();
        client_out
            .write_all(
                format!(
                    "{}\n",
                    json!({"jsonrpc": "2.0", "id": id, "result": {"action": "decline"}})
                )
                .as_bytes(),
            )
            .await
            .unwrap();

        let answer = asking.await.unwrap();
        assert_eq!(answer, Ok(json!({"action": "decline"})));
        let queued = queue_rx.recv().await.unwrap();
        assert!(queued.contains("\"ping\""));
        assert!(peer.pending().lock().unwrap().is_empty());

        // A notification goes out as one line.
        peer.notify(
            "notifications/progress",
            json!({"progressToken": 1, "progress": 5}),
        )
        .await;
        let read = client_in.read(&mut seen).await.unwrap();
        let note: Value = serde_json::from_slice(seen[..read].trim_ascii()).unwrap();
        assert_eq!(note["method"], "notifications/progress");

        // Closing stdin ends the reader and the queue.
        drop(client_out);
        assert!(queue_rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn an_unanswered_question_times_out_and_is_forgotten() {
        let (server_out, _client_in) = tokio::io::duplex(64 * 1024);
        let peer = StdioPeer::new(Arc::new(LineWriter::new(server_out)));
        let answer = peer
            .request("elicitation/create", json!({}), Duration::from_millis(50))
            .await;
        assert!(answer.unwrap_err().contains("no answer"));
        assert!(peer.pending().lock().unwrap().is_empty());
    }
}
