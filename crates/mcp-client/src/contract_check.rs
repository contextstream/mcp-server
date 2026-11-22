//! Cross-repo & cross-service contract verification client & stream driver (Phase 3.3).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use futures::{SinkExt, StreamExt};
use mcp_types::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::UnboundedSender;
use tokio_tungstenite::tungstenite::{error::ProtocolError, Error as WsError, Message};
use tokio_tungstenite::WebSocketStream;

use crate::client::ContextStreamClient;
use crate::frames::{
    self, connect, deadline_of, encode_frame, stream_url_allowed, Delivery, Event, FrameBuffer,
    FIRST_FRAME_WAIT, MESSAGE_MAX, PROTOCOL,
};
use crate::hosted_tools::{HostedToolsAnswer, HOSTED_TOOLS_PATH};

/// Valid empty git pack (PACK v2, 0 objects + SHA-1 trailer).
pub const EMPTY_GIT_PACK: [u8; 32] = [
    b'P', b'A', b'C', b'K', 0, 0, 0, 2, 0, 0, 0, 0, 0x02, 0x9d, 0x08, 0x82, 0x3b, 0xd8, 0xa8, 0xea,
    0xb5, 0x10, 0xad, 0x6a, 0xc7, 0x5c, 0x82, 0x3c, 0xfd, 0x3e, 0xd3, 0x1e,
];

/// Encode raw bytes as standard base64 for `ContractFileInput::content_b64`.
pub fn encode_contract_b64(bytes: &[u8]) -> String {
    BASE64.encode(bytes)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ContractBlob {
    pub path: String,
    pub sha256: String,
    pub content_utf8: String,
}

impl ContractBlob {
    pub fn to_contract_file_input(&self) -> ContractFileInput {
        ContractFileInput {
            source_path: self.path.clone(),
            target_path: None,
            content_b64: encode_contract_b64(self.content_utf8.as_bytes()),
            sha256: (!self.sha256.is_empty()).then(|| self.sha256.clone()),
            mode: Some("100644".to_string()),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ContractFileInput {
    pub source_path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_path: Option<String>,
    pub content_b64: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ContractCheckRequest {
    pub workspace_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin_sha256: Option<String>,
    pub changed_paths: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub contract_files: Vec<ContractFileInput>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub consumer_project_ids: Vec<String>,
    #[serde(skip_serializing_if = "HashMap::is_empty")]
    pub consumer_base_trees: HashMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub issue_tickets: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ConsumerStreamSpec {
    pub url: String,
    pub token: String,
    pub protocol: Option<String>,
    pub request: Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ConsumerCallInfo {
    pub id: String,
    pub expires_at: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ConsumerInlineResult {
    pub ran: bool,
    pub exit_code: Option<i32>,
    pub output: String,
    pub refused_reason: Option<String>,
    pub timed_out: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ConsumerContractTarget {
    pub project_id: Option<String>,
    pub project_name: Option<String>,
    pub repo_key: String,
    #[serde(alias = "branch")]
    pub default_branch: Option<String>,
    pub base_tree_hint: Option<String>,
    pub language: String,
    pub command: String,
    pub cwd: Option<String>,
    pub call: Option<ConsumerCallInfo>,
    pub call_id: Option<String>,
    pub ticket_error: Option<String>,
    pub stream: Option<ConsumerStreamSpec>,
    pub result: Option<ConsumerInlineResult>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ContractCheckResponse {
    pub source_project_id: Option<String>,
    pub matched_contract_paths: Vec<String>,
    pub consumers: Vec<ConsumerContractTarget>,
    pub elapsed_ms: u64,
}

impl ContextStreamClient {
    /// POST `/api/v1/mcp/hosted-tools/contract-check`
    pub async fn contract_check(
        &self,
        body: &ContractCheckRequest,
        timeout: Duration,
    ) -> Result<HostedToolsAnswer> {
        let path = format!("{HOSTED_TOOLS_PATH}/contract-check");
        let payload = serde_json::to_value(body)?;
        let raw = self
            .send_raw_json("POST", &path, Some(&payload), timeout)
            .await?;
        Ok(HostedToolsAnswer::from_body(raw.status, raw.body))
    }
}

/// Build `hello` for a server-dispatched consumer contract stream (no local `CheckoutSnapshot`).
pub fn contract_hello(request: &Value) -> std::result::Result<Value, String> {
    let mut normalized = request.clone();
    if let Some(obj) = normalized.as_object_mut() {
        obj.entry("cwd").or_insert_with(|| json!(""));
        obj.entry("timeout_s").or_insert_with(|| json!(300));
        obj.entry("wait_s").or_insert_with(|| json!(60));
    }
    let base_tree = normalized
        .get("base_tree")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "consumer stream request has no base_tree".to_string())?;
    let target_tree = normalized
        .get("target_tree")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(base_tree);

    let mut fields = Map::new();
    fields.insert("base_tree".into(), Value::String(base_tree.to_string()));
    fields.insert("target_tree".into(), Value::String(target_tree.to_string()));
    fields.insert("overlay".into(), json!([]));
    fields.insert("deletes".into(), json!([]));
    if let Some(contract_overlay) = normalized.get("contract_overlay").filter(|v| v.is_array()) {
        fields.insert("contract_overlay".into(), contract_overlay.clone());
    }

    frames::hello(&normalized, fields, Map::new(), Delivery::ToolResult)
}

async fn send_contract_frame<S>(
    sink: &mut futures::stream::SplitSink<WebSocketStream<S>, Message>,
    kind: u8,
    payload: &[u8],
) -> std::result::Result<(), String>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let frame = encode_frame(kind, payload);
    for piece in frame.chunks(MESSAGE_MAX) {
        sink.send(Message::binary(piece.to_vec()))
            .await
            .map_err(|e| format!("stream send failed: {e}"))?;
    }
    Ok(())
}

/// Drive a consumer contract stream (`frames/1`) over a `WebSocketStream`, collecting
/// `O` output bytes, forwarding `O` + `J` `info` frames to `events`, and returning the
/// final `J` `result` frame + accumulated output. Gracefully fails closed if `need`
/// arrives because the caller has no local Repo B checkout.
pub async fn exchange_contract_stream<S>(
    ws: WebSocketStream<S>,
    hello: &Value,
    deadline: Duration,
    events: UnboundedSender<Event>,
) -> std::result::Result<(Value, Vec<u8>), String>
where
    S: AsyncRead + AsyncWrite + Unpin + Send,
{
    let hello_bytes =
        serde_json::to_vec(hello).map_err(|e| format!("failed to serialize hello: {e}"))?;
    let (mut sink, mut stream) = ws.split();
    send_contract_frame(&mut sink, b'J', &hello_bytes).await?;
    send_contract_frame(&mut sink, b'D', &EMPTY_GIT_PACK).await?;

    let started = Instant::now();
    let mut heard = false;
    let mut buffer = FrameBuffer::default();
    let mut output = Vec::new();
    loop {
        let left = deadline
            .checked_sub(started.elapsed())
            .filter(|left| !left.is_zero())
            .ok_or_else(|| "consumer contract stream exceeded deadline".to_string())?;
        let wait = if heard {
            left
        } else {
            left.min(FIRST_FRAME_WAIT)
        };
        let message = match tokio::time::timeout(wait, stream.next()).await {
            Err(_) if heard => return Err("consumer contract stream timed out".to_string()),
            Err(_) => return Err("consumer contract stream did not respond in time".to_string()),
            Ok(None)
            | Ok(Some(Err(
                WsError::ConnectionClosed
                | WsError::AlreadyClosed
                | WsError::Io(_)
                | WsError::Protocol(ProtocolError::ResetWithoutClosingHandshake),
            ))) => return Err("consumer contract stream ended before result".to_string()),
            Ok(Some(Err(error))) => return Err(format!("consumer contract stream error: {error}")),
            Ok(Some(Ok(message))) => message,
        };
        match message {
            Message::Binary(bytes) => buffer.push(&bytes),
            Message::Close(_) => {
                return Err("consumer contract stream closed before result".to_string());
            }
            _ => continue,
        }
        while let Some((kind, payload)) = buffer
            .next_frame()
            .map_err(|e| format!("consumer contract stream protocol error: {e}"))?
        {
            heard = true;
            match kind {
                b'O' => {
                    if output.len() < 256 * 1024 {
                        let remaining = 256 * 1024 - output.len();
                        let take = payload.len().min(remaining);
                        output.extend_from_slice(&payload[..take]);
                    }
                    let _ = events.send(Event::Output(payload));
                }
                b'J' => {
                    let message: Value = serde_json::from_slice(&payload).map_err(|_| {
                        "consumer contract stream sent non-JSON J frame".to_string()
                    })?;
                    match message.get("type").and_then(Value::as_str) {
                        Some("need") => {
                            let _ =
                                tokio::time::timeout(Duration::from_secs(1), sink.close()).await;
                            return Err(
                                "consumer base_tree is not cached on the worker and no local checkout is available"
                                    .to_string(),
                            );
                        }
                        Some("info") => {
                            let _ = events.send(Event::Heartbeat);
                        }
                        Some("result") => {
                            let _ =
                                tokio::time::timeout(Duration::from_secs(2), sink.close()).await;
                            return Ok((message, output));
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
}

/// Connect and drive a consumer contract stream (`frames/1`).
pub async fn drive_contract_stream(
    spec: &ConsumerStreamSpec,
    events: UnboundedSender<Event>,
) -> std::result::Result<(Value, Vec<u8>), String> {
    if spec.protocol.as_deref() != Some(PROTOCOL) {
        return Err("consumer stream protocol is unsupported".to_string());
    }
    if !stream_url_allowed(&spec.url) {
        return Err("consumer stream URL is not permitted".to_string());
    }
    let hello = contract_hello(&spec.request)?;
    let deadline = deadline_of(&spec.request);
    let ws = connect(&spec.url, &spec.token)
        .await
        .map_err(|e| format!("consumer stream connection failed: {e}"))?;
    exchange_contract_stream(ws, &hello, deadline, events).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::SinkExt;
    use mcp_types::Config;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_tungstenite::accept_async;

    fn test_client(api_url: &str) -> ContextStreamClient {
        ContextStreamClient::new(Config {
            api_url: api_url.to_string(),
            api_key: Some("test-key".to_string()),
            ..Config::default()
        })
    }

    #[tokio::test]
    async fn contract_check_posts_request_and_handles_404_gracefully() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            for idx in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = vec![0u8; 4096];
                let n = socket.read(&mut buf).await.unwrap();
                let req = String::from_utf8_lossy(&buf[..n]);
                assert!(req.contains("POST /api/v1/mcp/hosted-tools/contract-check"));
                let (status, body) = if idx == 0 {
                    (
                        "200 OK",
                        json!({
                            "success": true,
                            "data": {
                                "source_project_id": "11111111-1111-4111-8111-111111111111",
                                "matched_contract_paths": ["openapi/billing.yaml"],
                                "consumers": [{
                                    "project_id": "22222222-2222-4222-8222-222222222222",
                                    "project_name": "checkout-web",
                                    "repo_key": "checkout-web-12345678",
                                    "default_branch": "main",
                                    "language": "typescript",
                                    "command": "npx tsc --noEmit"
                                }],
                                "elapsed_ms": 5
                            }
                        })
                        .to_string(),
                    )
                } else {
                    (
                        "404 Not Found",
                        json!({"error": {"message": "not found"}}).to_string(),
                    )
                };
                let resp = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(resp.as_bytes()).await.unwrap();
            }
        });

        let client = test_client(&format!("http://{addr}"));
        let req = ContractCheckRequest {
            workspace_id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
            changed_paths: vec!["openapi/billing.yaml".to_string()],
            contract_files: vec![ContractFileInput {
                source_path: "openapi/billing.yaml".to_string(),
                target_path: None,
                content_b64: encode_contract_b64(b"openapi: 3.0.0\n"),
                sha256: None,
                mode: Some("100644".to_string()),
            }],
            ..Default::default()
        };

        let ok_ans = client
            .contract_check(&req, Duration::from_secs(5))
            .await
            .unwrap();
        assert!(ok_ans.is_success());
        let parsed: ContractCheckResponse = serde_json::from_value(ok_ans.data).unwrap();
        assert_eq!(parsed.consumers.len(), 1);
        assert_eq!(parsed.consumers[0].repo_key, "checkout-web-12345678");

        let not_found = client
            .contract_check(&req, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(not_found.status, 404);
        assert!(!not_found.is_success());
        srv.abort();
    }

    #[tokio::test]
    async fn drive_contract_stream_streams_output_and_aborts_cleanly_on_need() {
        let evil = ConsumerStreamSpec {
            url: "ws://evil.example/v1/run".to_string(),
            token: "mct_1".to_string(),
            protocol: Some(PROTOCOL.to_string()),
            request: json!({
                "id": "mc-0123456789abcdef",
                "repo_key": "web-12345678",
                "command": "npx tsc --noEmit",
                "base_tree": "1".repeat(40),
                "target_tree": "1".repeat(40),
            }),
        };
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(drive_contract_stream(&evil, tx).await.is_err());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            for mode in ["ok", "need"] {
                let (stream, _) = listener.accept().await.unwrap();
                let mut ws = accept_async(stream).await.unwrap();
                // Read hello + D pack frames
                let mut buf = FrameBuffer::default();
                let mut got_hello = false;
                let mut got_pack = false;
                while !(got_hello && got_pack) {
                    if let Some(Ok(Message::Binary(bytes))) = ws.next().await {
                        buf.push(&bytes);
                        while let Some((kind, payload)) = buf.next_frame().unwrap() {
                            if kind == b'J' {
                                let v: Value = serde_json::from_slice(&payload).unwrap();
                                assert_eq!(v["type"], "hello");
                                assert_eq!(v["base_tree"], "a".repeat(40));
                                assert!(v["contract_overlay"].is_array());
                                got_hello = true;
                            } else if kind == b'D' {
                                assert_eq!(payload, EMPTY_GIT_PACK);
                                got_pack = true;
                            }
                        }
                    }
                }
                if mode == "ok" {
                    ws.send(Message::binary(encode_frame(
                        b'J',
                        br#"{"type":"info","text":"checking"}"#,
                    )))
                    .await
                    .unwrap();
                    ws.send(Message::binary(encode_frame(
                        b'O',
                        b"src/client.ts(14,22): error TS2339: Property 'userId' does not exist.\n",
                    )))
                    .await
                    .unwrap();
                    ws.send(Message::binary(encode_frame(
                        b'J',
                        br#"{"type":"result","ran":true,"exit":2}"#,
                    )))
                    .await
                    .unwrap();
                } else {
                    ws.send(Message::binary(encode_frame(
                        b'J',
                        br#"{"type":"need","listing":true}"#,
                    )))
                    .await
                    .unwrap();
                }
            }
        });

        let spec = ConsumerStreamSpec {
            url: format!("ws://127.0.0.1:{}/v1/run", addr.port()),
            token: "mct_test".to_string(),
            protocol: Some(PROTOCOL.to_string()),
            request: json!({
                "id": "mc-0123456789abcdef",
                "repo_key": "web-12345678",
                "command": "npx tsc --noEmit",
                "cwd": "",
                "timeout_s": 60,
                "wait_s": 10,
                "base_tree": "a".repeat(40),
                "target_tree": "a".repeat(40),
                "contract_overlay": [{"path": "contracts/user.ts", "mode": "100644", "content_b64": "e30="}],
            }),
        };

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (frame, output) = drive_contract_stream(&spec, tx).await.unwrap();
        assert_eq!(frame["ran"], true);
        assert_eq!(frame["exit"], 2);
        assert!(String::from_utf8_lossy(&output).contains("TS2339"));
        let mut saw_hb = false;
        let mut saw_out = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                Event::Heartbeat => saw_hb = true,
                Event::Output(b) if String::from_utf8_lossy(&b).contains("TS2339") => {
                    saw_out = true;
                }
                _ => {}
            }
        }
        assert!(saw_hb && saw_out);

        let (tx2, _rx2) = tokio::sync::mpsc::unbounded_channel();
        let err = drive_contract_stream(&spec, tx2).await.unwrap_err();
        assert!(err.contains("base_tree"), "{err}");
        srv.abort();
    }
}
