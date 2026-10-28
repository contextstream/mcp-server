//! A stub API for tests that must see which request a tool sent.

use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Answers every request `200` with one fixed JSON payload and records each
/// request's first line and body.
pub(crate) struct RequestProbe {
    pub(crate) base_url: String,
    requests: Arc<Mutex<Vec<(String, String)>>>,
    task: tokio::task::JoinHandle<()>,
}

impl RequestProbe {
    /// A probe that answers `{}`.
    pub(crate) async fn start() -> Self {
        Self::answering("{}").await
    }

    /// A probe that answers `payload`, for calls that expect, say, a list.
    pub(crate) async fn answering(payload: &'static str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let recorded = recorded.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let (head, body) = loop {
                        let mut chunk = [0u8; 4096];
                        let count = socket.read(&mut chunk).await.unwrap_or(0);
                        if count == 0 {
                            return;
                        }
                        request.extend_from_slice(&chunk[..count]);
                        let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
                            continue;
                        };
                        let head = String::from_utf8_lossy(&request[..end]).to_string();
                        let length: usize = head
                            .lines()
                            .filter_map(|line| line.split_once(':'))
                            .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                            .and_then(|(_, value)| value.trim().parse().ok())
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            let body = request[end + 4..end + 4 + length].to_vec();
                            break (head, String::from_utf8_lossy(&body).to_string());
                        }
                    };
                    let line = head.lines().next().unwrap_or_default().to_string();
                    recorded.lock().unwrap().push((line, body));
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                        payload.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        Self {
            base_url,
            requests,
            task,
        }
    }

    /// The first line of every request received so far, for example
    /// `GET /api/v1/projects/<id>/capsules?limit=50 HTTP/1.1`.
    pub(crate) fn request_lines(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|(line, _)| line.clone())
            .collect()
    }

    /// The JSON body of the first request whose line contains `needle`.
    pub(crate) fn body_of(&self, needle: &str) -> Option<serde_json::Value> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .find(|(line, _)| line.contains(needle))
            .and_then(|(_, body)| serde_json::from_str(body).ok())
    }
}

impl Drop for RequestProbe {
    fn drop(&mut self) {
        self.task.abort();
    }
}
