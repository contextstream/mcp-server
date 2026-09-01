//! Mock client for testing tools without network calls.

use mcp_types::Result;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Mock response configuration.
#[derive(Debug, Clone)]
pub struct MockResponse {
    pub status: u16,
    pub body: Value,
    pub delay_ms: Option<u64>,
}

impl MockResponse {
    /// Create a successful response with JSON body.
    pub fn ok(body: Value) -> Self {
        Self {
            status: 200,
            body,
            delay_ms: None,
        }
    }

    /// Create an error response.
    pub fn error(status: u16, message: &str) -> Self {
        Self {
            status,
            body: serde_json::json!({
                "error": message,
                "status": status
            }),
            delay_ms: None,
        }
    }

    /// Create a 404 not found response.
    pub fn not_found() -> Self {
        Self::error(404, "Not found")
    }

    /// Create a 401 unauthorized response.
    pub fn unauthorized() -> Self {
        Self::error(401, "Unauthorized")
    }

    /// Create a 500 internal server error response.
    pub fn server_error() -> Self {
        Self::error(500, "Internal server error")
    }

    /// Add a delay to the response (for testing timeouts).
    pub fn with_delay(mut self, delay_ms: u64) -> Self {
        self.delay_ms = Some(delay_ms);
        self
    }
}

/// Recorded request for verification.
#[derive(Debug, Clone)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub query: Option<String>,
    pub body: Option<Value>,
    pub headers: HashMap<String, String>,
}

/// Mock API client for testing.
///
/// Allows configuring expected responses and recording requests for verification.
#[derive(Debug, Clone)]
pub struct MockClient {
    /// Response handlers keyed by "METHOD /path".
    responses: Arc<RwLock<HashMap<String, Vec<MockResponse>>>>,
    /// Recorded requests for verification.
    requests: Arc<RwLock<Vec<RecordedRequest>>>,
    /// Default response for unmatched requests.
    default_response: Arc<RwLock<Option<MockResponse>>>,
    /// Base URL (for compatibility with real client).
    pub base_url: String,
}

impl Default for MockClient {
    fn default() -> Self {
        Self::new()
    }
}

impl MockClient {
    /// Create a new mock client.
    pub fn new() -> Self {
        Self {
            responses: Arc::new(RwLock::new(HashMap::new())),
            requests: Arc::new(RwLock::new(Vec::new())),
            default_response: Arc::new(RwLock::new(None)),
            base_url: "https://mock.contextstream.io".to_string(),
        }
    }

    /// Configure a response for a specific endpoint.
    ///
    /// # Example
    /// ```ignore
    /// client.on("GET /api/v1/me").respond(MockResponse::ok(json!({"id": "123"})));
    /// ```
    pub fn on(&self, endpoint: &str) -> MockEndpoint {
        MockEndpoint {
            client: self.clone(),
            endpoint: endpoint.to_string(),
        }
    }

    /// Set the default response for unmatched requests.
    pub fn set_default_response(&self, response: MockResponse) {
        *self.default_response.write().unwrap() = Some(response);
    }

    /// Get all recorded requests.
    pub fn recorded_requests(&self) -> Vec<RecordedRequest> {
        self.requests.read().unwrap().clone()
    }

    /// Get requests to a specific path.
    pub fn requests_to(&self, path: &str) -> Vec<RecordedRequest> {
        self.requests
            .read()
            .unwrap()
            .iter()
            .filter(|r| r.path == path)
            .cloned()
            .collect()
    }

    /// Assert that a specific endpoint was called.
    pub fn assert_called(&self, endpoint: &str) {
        let parts: Vec<&str> = endpoint.splitn(2, ' ').collect();
        let (method, path) = if parts.len() == 2 {
            (parts[0], parts[1])
        } else {
            ("GET", parts[0])
        };

        let requests = self.requests.read().unwrap();
        let found = requests
            .iter()
            .any(|r| r.method == method && r.path == path);
        assert!(found, "Expected {} {} to be called", method, path);
    }

    /// Assert that an endpoint was called N times.
    pub fn assert_called_times(&self, endpoint: &str, times: usize) {
        let parts: Vec<&str> = endpoint.splitn(2, ' ').collect();
        let (method, path) = if parts.len() == 2 {
            (parts[0], parts[1])
        } else {
            ("GET", parts[0])
        };

        let requests = self.requests.read().unwrap();
        let count = requests
            .iter()
            .filter(|r| r.method == method && r.path == path)
            .count();
        assert_eq!(
            count, times,
            "Expected {} {} to be called {} times, was called {} times",
            method, path, times, count
        );
    }

    /// Assert no requests were made.
    pub fn assert_no_requests(&self) {
        let requests = self.requests.read().unwrap();
        assert!(
            requests.is_empty(),
            "Expected no requests, but {} were made",
            requests.len()
        );
    }

    /// Clear all recorded requests.
    pub fn clear_requests(&self) {
        self.requests.write().unwrap().clear();
    }

    /// Reset all configured responses.
    pub fn reset(&self) {
        self.responses.write().unwrap().clear();
        self.requests.write().unwrap().clear();
        *self.default_response.write().unwrap() = None;
    }

    /// Simulate making a request (internal use).
    pub async fn mock_request(
        &self,
        method: &str,
        path: &str,
        query: Option<&str>,
        body: Option<Value>,
    ) -> Result<Value> {
        // Record the request
        {
            let mut requests = self.requests.write().unwrap();
            requests.push(RecordedRequest {
                method: method.to_string(),
                path: path.to_string(),
                query: query.map(String::from),
                body: body.clone(),
                headers: HashMap::new(),
            });
        }

        // Find matching response
        let key = format!("{} {}", method, path);
        let response = {
            let mut responses = self.responses.write().unwrap();
            if let Some(queue) = responses.get_mut(&key) {
                if !queue.is_empty() {
                    Some(queue.remove(0))
                } else {
                    None
                }
            } else {
                None
            }
        };

        let response = response.or_else(|| self.default_response.read().unwrap().clone());

        match response {
            Some(resp) => {
                // Simulate delay if configured
                if let Some(delay) = resp.delay_ms {
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                }

                if resp.status >= 200 && resp.status < 300 {
                    Ok(resp.body)
                } else {
                    Err(mcp_types::Error::http(
                        resp.status,
                        resp.body
                            .get("error")
                            .and_then(|e| e.as_str())
                            .unwrap_or("Unknown error"),
                    ))
                }
            }
            None => Err(mcp_types::Error::Tool(format!(
                "No mock response configured for {} {}",
                method, path
            ))),
        }
    }
}

/// Builder for configuring mock endpoint responses.
pub struct MockEndpoint {
    client: MockClient,
    endpoint: String,
}

impl MockEndpoint {
    /// Configure a response for this endpoint.
    pub fn respond(self, response: MockResponse) -> MockClient {
        {
            let mut responses = self.client.responses.write().unwrap();
            responses.entry(self.endpoint).or_default().push(response);
        }
        self.client
    }

    /// Configure multiple responses (for sequential calls).
    pub fn respond_with_sequence(self, responses: Vec<MockResponse>) -> MockClient {
        {
            let mut resp_map = self.client.responses.write().unwrap();
            resp_map.insert(self.endpoint, responses);
        }
        self.client
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_client_basic() {
        let client = MockClient::new();
        client
            .on("GET /api/v1/me")
            .respond(MockResponse::ok(serde_json::json!({
                "id": "user-123",
                "email": "test@example.com"
            })));

        let result = client.mock_request("GET", "/api/v1/me", None, None).await;
        assert!(result.is_ok());

        let body = result.unwrap();
        assert_eq!(body["id"], "user-123");

        client.assert_called("GET /api/v1/me");
    }

    #[tokio::test]
    async fn test_mock_client_error() {
        let client = MockClient::new();
        client
            .on("GET /api/v1/missing")
            .respond(MockResponse::not_found());

        let result = client
            .mock_request("GET", "/api/v1/missing", None, None)
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_mock_client_sequence() {
        let client = MockClient::new();
        client.on("POST /api/v1/retry").respond_with_sequence(vec![
            MockResponse::server_error(),
            MockResponse::server_error(),
            MockResponse::ok(serde_json::json!({"success": true})),
        ]);

        // First two calls fail
        assert!(client
            .mock_request("POST", "/api/v1/retry", None, None)
            .await
            .is_err());
        assert!(client
            .mock_request("POST", "/api/v1/retry", None, None)
            .await
            .is_err());
        // Third succeeds
        assert!(client
            .mock_request("POST", "/api/v1/retry", None, None)
            .await
            .is_ok());

        client.assert_called_times("POST /api/v1/retry", 3);
    }
}
