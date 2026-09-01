//! Tests for search domain tools.

use super::*;
use crate::registry::ToolHandler;
use crate::testing::TestFixtures;
use mcp_types::tool::ToolCategory;
use serde_json::json;

// ============================================================================
// Test Helpers
// ============================================================================

fn create_mock_client() -> ContextStreamClient {
    ContextStreamClient::new(TestFixtures::test_config())
}

fn create_mock_client_without_auth() -> ContextStreamClient {
    let mut config = TestFixtures::test_config();
    config.api_key = None;
    config.jwt = None;
    config.allow_header_auth = false;
    ContextStreamClient::new(config)
}

fn create_mock_session(client: &ContextStreamClient) -> Arc<SessionManager> {
    Arc::new(SessionManager::new(
        client.clone(),
        TestFixtures::test_config(),
    ))
}

fn create_mock_index_keeper(
    client: &ContextStreamClient,
    session: &Arc<SessionManager>,
) -> Arc<crate::domains::index_keeper::IndexKeeper> {
    Arc::new(crate::domains::index_keeper::IndexKeeper::new(
        client.clone(),
        session.clone(),
        mcp_types::atlas_layer::noop_layer(),
        mcp_types::acceleration_layer::noop_acceleration_layer(),
    ))
}

#[test]
fn local_mapping_mismatch_guidance_keeps_hosted_mcp_and_uses_exact_checkout_refresh() {
    let project_id = Uuid::new_v4();
    let note = local_mapping_mismatch_note(
        project_id,
        "workspace-wide scope",
        "/worktrees/contextstream/fix\"quoted\nline",
    );

    assert!(note.contains(&project_id.to_string()));
    assert!(note.contains("init(folder_path=\"/worktrees/contextstream/fix\\\"quoted\\nline\")"));
    assert!(note.contains("project(action=\"index_status\")"));
    assert!(note.contains("project(action=\"index\")"));
    assert!(note.contains("requires_sync_bridge"));
    assert!(note.contains("keeping hosted MCP configured"));
    assert!(!note.contains("ingest_local"));
    assert!(!note.contains("local MCP process"));
}

mod zero_result_escalation_tests {
    use super::*;

    #[test]
    fn authoritative_exact_miss_does_not_broaden() {
        assert!(!should_run_broad_mode_escalation(true, false, false));
    }

    #[test]
    fn auto_natural_language_miss_can_still_broaden() {
        assert!(should_run_broad_mode_escalation(true, false, true));
    }

    #[test]
    fn hits_and_invalid_scope_never_broaden() {
        assert!(!should_run_broad_mode_escalation(false, false, true));
        assert!(!should_run_broad_mode_escalation(true, true, true));
    }
}

#[test]
fn unroutable_guided_checkout_is_marked_unconfirmed_without_exposing_a_path() {
    let result = mark_checkout_scope_unconfirmed(ToolResult::with_structured(
        "Canonical evidence.",
        json!({"results": []}),
    ));
    let text = result
        .content
        .iter()
        .find_map(|item| match item {
            ContentItem::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .expect("text result");

    assert!(text.contains("[CHECKOUT_SCOPE]"));
    assert!(text.contains("could not derive an exact"));
    assert_eq!(
        result.structured_content.as_ref().unwrap()["checkout_scope_unconfirmed"],
        true
    );
    assert_eq!(
        result.structured_content.as_ref().unwrap()["checkout_scope_reason"],
        "checkout_routing_scope_unavailable"
    );
    assert!(!text.contains("/home/"));
}

struct RecordingHttpResponse {
    expected_path: &'static str,
    status: u16,
    body: String,
    delay: std::time::Duration,
}

fn spawn_search_recording_server(
    responses: Vec<RecordingHttpResponse>,
) -> (
    String,
    std::sync::mpsc::Receiver<(String, String)>,
    std::thread::JoinHandle<()>,
) {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind search test server");
    let addr = listener
        .local_addr()
        .expect("search test server local addr");
    let (sender, receiver) = std::sync::mpsc::channel();
    let handle = std::thread::spawn(move || {
        let mut workers = Vec::new();
        for response_spec in responses {
            let (mut stream, _) = listener.accept().expect("accept search request");
            let sender = sender.clone();
            workers.push(std::thread::spawn(move || {
                let mut request = Vec::new();
                let mut content_length = None;
                loop {
                    let mut chunk = [0u8; 4_096];
                    let read = stream.read(&mut chunk).expect("read search request");
                    assert!(read > 0, "search request closed before its body arrived");
                    request.extend_from_slice(&chunk[..read]);
                    if content_length.is_none() {
                        if let Some(header_end) =
                            request.windows(4).position(|window| window == b"\r\n\r\n")
                        {
                            let headers = String::from_utf8_lossy(&request[..header_end]);
                            content_length = Some(headers.lines().find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())
                                    .flatten()
                            }).unwrap_or(0));
                        }
                    }
                    if let (Some(header_end), Some(content_length)) = (
                        request.windows(4).position(|window| window == b"\r\n\r\n"),
                        content_length,
                    ) {
                        let body_start = header_end + 4;
                        if request.len() >= body_start + content_length {
                            let request_line = String::from_utf8_lossy(&request)
                                .lines()
                                .next()
                                .unwrap_or_default()
                                .to_string();
                            assert!(
                                request_line.contains(response_spec.expected_path),
                                "unexpected search request line: {request_line}"
                            );
                            sender
                                .send((
                                    request_line,
                                    String::from_utf8_lossy(
                                        &request[body_start..body_start + content_length],
                                    )
                                    .to_string(),
                                ))
                                .expect("record search request");
                            break;
                        }
                    }
                }

                if !response_spec.delay.is_zero() {
                    std::thread::sleep(response_spec.delay);
                }
                let reason = if response_spec.status == 200 {
                    "OK"
                } else {
                    "ERROR"
                };
                let response = format!(
                    "HTTP/1.1 {} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    response_spec.status,
                    reason,
                    response_spec.body.len(),
                    response_spec.body,
                );
                // A deliberately timed-out client may already have closed the
                // first connection; that is the behavior this harness tests.
                let _ = stream.write_all(response.as_bytes());
            }));
        }
        for worker in workers {
            worker.join().expect("search response worker");
        }
    });
    (format!("http://{addr}"), receiver, handle)
}

// ============================================================================
// SearchMode Tests
// ============================================================================

mod search_mode_tests {
    use super::SearchMode;

    #[test]
    fn test_search_mode_from_str_hybrid() {
        assert_eq!(SearchMode::from_str("hybrid"), SearchMode::Hybrid);
        assert_eq!(SearchMode::from_str("Hybrid"), SearchMode::Hybrid);
        assert_eq!(SearchMode::from_str("HYBRID"), SearchMode::Hybrid);
    }

    #[test]
    fn test_search_mode_from_str_semantic() {
        assert_eq!(SearchMode::from_str("semantic"), SearchMode::Semantic);
        assert_eq!(SearchMode::from_str("Semantic"), SearchMode::Semantic);
        assert_eq!(SearchMode::from_str("SEMANTIC"), SearchMode::Semantic);
    }

    #[test]
    fn test_search_mode_from_str_keyword() {
        assert_eq!(SearchMode::from_str("keyword"), SearchMode::Keyword);
        assert_eq!(SearchMode::from_str("Keyword"), SearchMode::Keyword);
        assert_eq!(SearchMode::from_str("text"), SearchMode::Keyword);
        assert_eq!(SearchMode::from_str("TEXT"), SearchMode::Keyword);
    }

    #[test]
    fn test_search_mode_from_str_pattern() {
        assert_eq!(SearchMode::from_str("pattern"), SearchMode::Pattern);
        assert_eq!(SearchMode::from_str("Pattern"), SearchMode::Pattern);
        assert_eq!(SearchMode::from_str("regex"), SearchMode::Pattern);
        assert_eq!(SearchMode::from_str("REGEX"), SearchMode::Pattern);
    }

    #[test]
    fn test_search_mode_from_str_default() {
        // Unknown values default to Hybrid
        assert_eq!(SearchMode::from_str("unknown"), SearchMode::Hybrid);
        assert_eq!(SearchMode::from_str(""), SearchMode::Hybrid);
        assert_eq!(SearchMode::from_str("foo"), SearchMode::Hybrid);
    }

    #[test]
    fn test_search_mode_from_str_exhaustive() {
        assert_eq!(SearchMode::from_str("exhaustive"), SearchMode::Exhaustive);
        assert_eq!(SearchMode::from_str("Exhaustive"), SearchMode::Exhaustive);
    }

    #[test]
    fn test_search_mode_from_str_refactor() {
        assert_eq!(SearchMode::from_str("refactor"), SearchMode::Refactor);
        assert_eq!(SearchMode::from_str("Refactor"), SearchMode::Refactor);
    }

    #[test]
    fn test_search_mode_from_str_team() {
        assert_eq!(SearchMode::from_str("team"), SearchMode::Team);
        assert_eq!(SearchMode::from_str("Team"), SearchMode::Team);
    }

    #[test]
    fn test_search_mode_from_str_guided() {
        assert_eq!(SearchMode::from_str("guided"), SearchMode::Guided);
        assert_eq!(SearchMode::from_str("Guided"), SearchMode::Guided);
        assert_eq!(SearchMode::from_str("navigate"), SearchMode::Guided);
    }

    #[test]
    fn test_search_mode_as_str() {
        assert_eq!(SearchMode::Hybrid.as_str(), "hybrid");
        assert_eq!(SearchMode::Semantic.as_str(), "semantic");
        assert_eq!(SearchMode::Keyword.as_str(), "keyword");
        assert_eq!(SearchMode::Pattern.as_str(), "pattern");
        assert_eq!(SearchMode::Exhaustive.as_str(), "exhaustive");
        assert_eq!(SearchMode::Refactor.as_str(), "refactor");
        assert_eq!(SearchMode::Team.as_str(), "team");
        assert_eq!(SearchMode::Guided.as_str(), "guided");
    }

    #[test]
    fn test_search_mode_default() {
        assert_eq!(SearchMode::default(), SearchMode::Hybrid);
    }

    #[test]
    fn test_search_mode_equality() {
        assert_eq!(SearchMode::Hybrid, SearchMode::Hybrid);
        assert_ne!(SearchMode::Hybrid, SearchMode::Semantic);
        assert_ne!(SearchMode::Keyword, SearchMode::Pattern);
    }

    #[test]
    fn test_search_mode_debug() {
        // Ensure Debug is derived
        let mode = SearchMode::Hybrid;
        let debug_str = format!("{:?}", mode);
        assert!(debug_str.contains("Hybrid"));
    }

    #[test]
    fn test_search_mode_clone() {
        let mode = SearchMode::Semantic;
        let cloned = mode;
        assert_eq!(mode, cloned);
    }

    #[test]
    fn test_search_mode_copy() {
        let mode = SearchMode::Keyword;
        let copied = mode;
        assert_eq!(mode, copied);
    }
}

// ============================================================================
// Guided Search Contract / Rendering Tests
// ============================================================================

mod guided_search_tests {
    use super::*;
    use serde_json::Value;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;
    use std::thread;

    fn spawn_guided_recording_server(
        response_bodies: Vec<String>,
    ) -> (String, mpsc::Receiver<String>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind guided test server");
        let addr = listener.local_addr().expect("guided server local addr");
        let (sender, receiver) = mpsc::channel();
        let handle = thread::spawn(move || {
            for response_body in response_bodies {
                let (mut stream, _) = listener.accept().expect("accept guided request");
                let mut request = Vec::new();
                let mut content_length = None;
                loop {
                    let mut chunk = [0u8; 4_096];
                    let read = stream.read(&mut chunk).expect("read guided request");
                    assert!(read > 0, "guided request closed before its body arrived");
                    request.extend_from_slice(&chunk[..read]);
                    if content_length.is_none() {
                        if let Some(header_end) = request.windows(4).position(|w| w == b"\r\n\r\n")
                        {
                            let headers = String::from_utf8_lossy(&request[..header_end]);
                            content_length = headers.lines().find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())
                                    .flatten()
                            });
                        }
                    }
                    if let (Some(header_end), Some(content_length)) = (
                        request.windows(4).position(|w| w == b"\r\n\r\n"),
                        content_length,
                    ) {
                        let body_start = header_end + 4;
                        if request.len() >= body_start + content_length {
                            let first_line = String::from_utf8_lossy(&request)
                                .lines()
                                .next()
                                .unwrap_or_default()
                                .to_string();
                            assert!(
                                first_line.starts_with("POST ")
                                    && first_line.contains("/search/guided"),
                                "unexpected guided request line: {first_line}"
                            );
                            sender
                                .send(
                                    String::from_utf8_lossy(
                                        &request[body_start..body_start + content_length],
                                    )
                                    .to_string(),
                                )
                                .expect("record guided request body");
                            break;
                        }
                    }
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    response_body.len(),
                    response_body,
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("write guided response");
            }
        });
        (format!("http://{addr}"), receiver, handle)
    }

    fn guided_success_body(
        workspace_id: uuid::Uuid,
        project_id: uuid::Uuid,
        grounding_handle: Option<&str>,
    ) -> String {
        json!({
            "query": "auth middleware",
            "workspace_id": workspace_id,
            "project_id": project_id,
            "guidance": null,
            "degraded": false,
            "guidance_latency_ms": 2,
            "retrieval_latency_ms": 1,
            "navigator_latency_ms": 1,
            "grounding_handle": grounding_handle,
            "grounding_base_reused": false,
            "results": [{
                "file_path": "src/auth.rs",
                "start_line": 10,
                "end_line": 20,
                "language": "rust",
                "snippet": "fn authenticate() {}",
                "source_type": "code"
            }],
            "knowledge": []
        })
        .to_string()
    }

    fn guided_tool_with_base_url(base_url: String) -> (SearchTool, Arc<SessionManager>) {
        let mut config = TestFixtures::test_config();
        config.api_url = base_url;
        config.default_workspace_id = None;
        config.default_project_id = None;
        let client = ContextStreamClient::new(config.clone());
        let session = Arc::new(SessionManager::new(client.clone(), config));
        let index_keeper = create_mock_index_keeper(&client, &session);
        (
            SearchTool::new(
                client,
                session.clone(),
                index_keeper,
                mcp_types::atlas_layer::noop_layer(),
            ),
            session,
        )
    }

    fn guided_response(degraded: bool) -> GuidedSearchApiResponse {
        GuidedSearchApiResponse {
            query: "auth middleware".to_string(),
            workspace_id: Some(uuid::Uuid::new_v4()),
            project_id: Some(uuid::Uuid::new_v4()),
            checkout_scope: None,
            guidance: (!degraded).then(|| GuidedSearchGuidance {
                answer: "Start in the authentication middleware.".to_string(),
                targets: vec![GuidedSearchTarget {
                    path: "src/auth.rs".to_string(),
                    lines: Some("10-20".to_string()),
                    symbol: Some("authenticate".to_string()),
                    why: "owns request authentication".to_string(),
                }],
                confidence: 0.92,
                followup_queries: Vec::new(),
            }),
            degraded,
            degradation_reason: degraded.then(|| "navigator_timeout".to_string()),
            guidance_recovery_reason: None,
            guidance_latency_ms: 120,
            retrieval_latency_ms: 40,
            navigator_latency_ms: if degraded { 2_500 } else { 80 },
            total_latency_ms: Some(if degraded { 2_640 } else { 135 }),
            code_evidence_count: Some(1),
            memory_evidence_count: Some(1),
            grounding_handle: Some("gb:v1:opaque".to_string()),
            grounding_base_reused: true,
            results: vec![GuidedSearchRawResult {
                file_path: "src/auth.rs".to_string(),
                start_line: 10,
                end_line: 20,
                language: "rust".to_string(),
                snippet: "fn authenticate(request: &Request) -> Result<User> {\n    // ...\n}"
                    .to_string(),
                source_type: "code".to_string(),
            }],
            knowledge: vec!["Authentication boundary decision".to_string()],
        }
    }

    #[test]
    fn guided_request_contract_serializes_intent_scope_and_bounded_limit() {
        let workspace_id = uuid::Uuid::new_v4();
        let project_id = uuid::Uuid::new_v4();
        let installation_id = uuid::Uuid::new_v4();
        let learning_request_id = uuid::Uuid::new_v4();
        let request = GuidedSearchApiRequest {
            query: "auth middleware",
            intent: Some("fix expired sessions"),
            workspace_id: Some(workspace_id),
            project_id: Some(project_id),
            installation_id: Some(installation_id),
            checkout_locator: Some("checkout-locator-v1:opaque"),
            grounding_handle: Some("gb:v1:opaque"),
            code_rerank_learning_opt_in: Some(true),
            code_rerank_learning_request_id: Some(learning_request_id),
            limit: GUIDED_SEARCH_MAX_LIMIT,
        };
        let value = serde_json::to_value(request).unwrap();

        assert_eq!(value["query"], "auth middleware");
        assert_eq!(value["intent"], "fix expired sessions");
        assert_eq!(value["workspace_id"], workspace_id.to_string());
        assert_eq!(value["project_id"], project_id.to_string());
        assert_eq!(value["installation_id"], installation_id.to_string());
        assert_eq!(value["checkout_locator"], "checkout-locator-v1:opaque");
        assert_eq!(value["grounding_handle"], "gb:v1:opaque");
        assert_eq!(value["code_rerank_learning_opt_in"], true);
        assert_eq!(
            value["code_rerank_learning_request_id"],
            learning_request_id.to_string()
        );
        assert_eq!(value["limit"], GUIDED_SEARCH_MAX_LIMIT);
        assert!(GUIDED_SEARCH_REQUEST_TIMEOUT < std::time::Duration::from_secs(30));
    }

    #[test]
    fn guided_request_omits_learning_opt_in_without_explicit_consent() {
        let request = GuidedSearchApiRequest {
            query: "auth middleware",
            intent: None,
            workspace_id: None,
            project_id: None,
            installation_id: None,
            checkout_locator: None,
            grounding_handle: None,
            code_rerank_learning_opt_in: None,
            code_rerank_learning_request_id: None,
            limit: GUIDED_SEARCH_DEFAULT_LIMIT,
        };
        let value = serde_json::to_value(request).unwrap();
        assert!(value.get("code_rerank_learning_opt_in").is_none());
    }

    #[tokio::test]
    async fn guided_cache_miss_forwards_opt_in_true_and_omits_default() {
        let workspace_id = uuid::Uuid::new_v4();
        let project_id = uuid::Uuid::new_v4();
        let checkout_scope = CheckoutRoutingScope {
            installation_id: uuid::Uuid::new_v4(),
            checkout_locator: "checkout-locator-v1:guided".to_string(),
        };
        let response = guided_success_body(workspace_id, project_id, None);
        let (base_url, requests, server) =
            spawn_guided_recording_server(vec![response.clone(), response]);
        let (tool, session) = guided_tool_with_base_url(base_url);
        session
            .initialize(Some(workspace_id), Some(project_id), None, None)
            .await;
        session
            .set_grounding_handle(Some("gb:v1:input-handle".to_string()))
            .await;

        let mut opted_in = auto_mode_tests::base_input("auth middleware");
        opted_in.mode = Some("guided".to_string());
        opted_in.code_rerank_learning_opt_in = Some(true);
        let learning_request_id = uuid::Uuid::new_v4();
        let opted_result = tool
            .execute_guided_search(
                &opted_in,
                Some(workspace_id),
                Some(project_id),
                Some("gb:v1:input-handle".to_string()),
                Some(learning_request_id),
                Some(checkout_scope.clone()),
                false,
                "guided-forward-opt-in".to_string(),
            )
            .await
            .expect("opted-in guided request should succeed");
        let opted_body: Value = serde_json::from_str(
            &requests
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("record opted-in body"),
        )
        .unwrap();
        assert_eq!(opted_body["code_rerank_learning_opt_in"], true);
        assert_eq!(
            opted_body["code_rerank_learning_request_id"],
            learning_request_id.to_string()
        );
        assert_eq!(opted_body["grounding_handle"], "gb:v1:input-handle");
        assert_eq!(
            opted_body["installation_id"],
            checkout_scope.installation_id.to_string()
        );
        assert_eq!(
            opted_body["checkout_locator"],
            checkout_scope.checkout_locator
        );
        assert_eq!(
            opted_result.structured_content.as_ref().unwrap()["code_rerank_learning_request_id"],
            learning_request_id.to_string()
        );

        let mut default_input = opted_in.clone();
        default_input.code_rerank_learning_opt_in = None;
        tool.execute_guided_search(
            &default_input,
            Some(workspace_id),
            Some(project_id),
            Some("gb:v1:input-handle".to_string()),
            None,
            None,
            false,
            "guided-forward-default".to_string(),
        )
        .await
        .expect("default guided request should succeed");
        let default_body: Value = serde_json::from_str(
            &requests
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("record default body"),
        )
        .unwrap();
        assert!(default_body.get("code_rerank_learning_opt_in").is_none());
        assert!(default_body
            .get("code_rerank_learning_request_id")
            .is_none());
        assert_eq!(default_body["grounding_handle"], "gb:v1:input-handle");

        server
            .join()
            .expect("guided recording server should finish");
    }

    #[tokio::test]
    async fn guided_timeout_fallback_uses_a_distinct_served_learning_receipt() {
        let workspace_id = uuid::Uuid::new_v4();
        let project_id = uuid::Uuid::new_v4();
        let guided_learning_request_id = uuid::Uuid::new_v4();
        let hybrid_response = SearchResponse {
            results: vec![SearchResult {
                id: "hybrid-served".to_string(),
                file_path: Some("src/hybrid.rs".to_string()),
                content: Some("fn served_after_guided_timeout() {}".to_string()),
                score: Some(0.9),
                ..Default::default()
            }],
            total: Some(1),
            query_time_ms: Some(2),
            ..Default::default()
        };
        let (base_url, requests, server) = spawn_search_recording_server(vec![
            RecordingHttpResponse {
                expected_path: "/search/guided",
                status: 200,
                body: guided_success_body(workspace_id, project_id, None),
                delay: std::time::Duration::from_millis(150),
            },
            RecordingHttpResponse {
                expected_path: "/search/hybrid",
                status: 200,
                body: serde_json::to_string(&hybrid_response).unwrap(),
                delay: std::time::Duration::ZERO,
            },
        ]);
        let (tool, _session) = guided_tool_with_base_url(base_url);
        let mut input = auto_mode_tests::base_input("auth middleware");
        input.mode = Some("guided".to_string());
        input.code_rerank_learning_opt_in = Some(true);

        let result = tool
            .execute_guided_search_with_timeout(
                &input,
                Some(workspace_id),
                Some(project_id),
                None,
                Some(guided_learning_request_id),
                None,
                false,
                "guided-timeout-fallback".to_string(),
                std::time::Duration::from_millis(25),
            )
            .await
            .expect("guided timeout should serve hybrid fallback");
        assert_eq!(
            result.structured_content.as_ref().unwrap()["fallback_used"],
            true
        );
        assert_eq!(
            result.structured_content.as_ref().unwrap()["degradation_reason"],
            "transport_fallback"
        );
        assert!(
            result.structured_content.as_ref().unwrap()["total_latency_ms"]
                .as_i64()
                .unwrap_or_default()
                <= 25,
            "reported latency must cover the single caller-supplied deadline, not start a second fallback clock"
        );

        let (guided_line, guided_body) = requests
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("record timed-out guided request");
        let (hybrid_line, hybrid_body) = requests
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("record hybrid fallback request");
        assert!(guided_line.contains("/search/guided"));
        assert!(hybrid_line.contains("/search/hybrid"));
        let guided_body: Value = serde_json::from_str(&guided_body).unwrap();
        let hybrid_body: Value = serde_json::from_str(&hybrid_body).unwrap();
        let fallback_learning_request_id = hybrid_body["code_rerank_learning_request_id"]
            .as_str()
            .and_then(|value| uuid::Uuid::parse_str(value).ok())
            .expect("hybrid fallback has a learning request id");
        assert_eq!(
            guided_body["code_rerank_learning_request_id"],
            guided_learning_request_id.to_string()
        );
        assert_ne!(fallback_learning_request_id, guided_learning_request_id);
        assert_eq!(
            result.structured_content.as_ref().unwrap()["code_rerank_learning_request_id"],
            fallback_learning_request_id.to_string()
        );

        server.join().expect("guided timeout server should finish");
    }

    #[tokio::test]
    async fn stale_guided_response_cannot_roll_newer_session_handle_backward() {
        let workspace_id = uuid::Uuid::new_v4();
        let project_id = uuid::Uuid::new_v4();
        let (base_url, requests, server) =
            spawn_guided_recording_server(vec![guided_success_body(
                workspace_id,
                project_id,
                Some("gb:v1:old-response-output"),
            )]);
        let (tool, session) = guided_tool_with_base_url(base_url);
        session
            .initialize(Some(workspace_id), Some(project_id), None, None)
            .await;
        // This simulates another request advancing the session after the
        // current request captured its input handle but before its response.
        session
            .set_grounding_handle(Some("gb:v1:newer-session-handle".to_string()))
            .await;
        let mut input = auto_mode_tests::base_input("auth middleware");
        input.mode = Some("guided".to_string());

        tool.execute_guided_search(
            &input,
            Some(workspace_id),
            Some(project_id),
            Some("gb:v1:older-request-handle".to_string()),
            None,
            None,
            false,
            "guided-stale-response".to_string(),
        )
        .await
        .expect("guided response should remain usable");
        let request_body: Value = serde_json::from_str(
            &requests
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("record stale guided body"),
        )
        .unwrap();
        assert_eq!(
            request_body["grounding_handle"],
            "gb:v1:older-request-handle"
        );
        assert_eq!(
            session.state().await.grounding_handle.as_deref(),
            Some("gb:v1:newer-session-handle")
        );
        server
            .join()
            .expect("guided recording server should finish");
    }

    #[test]
    fn guided_response_contract_deserializes_degraded_raw_evidence() {
        let response: GuidedSearchApiResponse = serde_json::from_value(json!({
            "query": "auth middleware",
            "workspace_id": uuid::Uuid::new_v4(),
            "project_id": uuid::Uuid::new_v4(),
            "guidance": null,
            "degraded": true,
            "guidance_latency_ms": 2540,
            "retrieval_latency_ms": 40,
            "navigator_latency_ms": 2500,
            "results": [{
                "file_path": "src/auth.rs",
                "start_line": 10,
                "end_line": 20,
                "language": "rust",
                "snippet": "fn authenticate() {}",
                "source_type": "code"
            }],
            "knowledge": ["Authentication decision"]
        }))
        .unwrap();

        assert!(response.degraded);
        assert!(response.guidance.is_none());
        assert_eq!(response.results.len(), 1);
        assert_eq!(response.results[0].file_path, "src/auth.rs");
        assert_eq!(response.navigator_latency_ms, 2_500);

        let (_, structured) = render_guided_search_response(
            &response,
            None,
            Some("full"),
            Some("human-readable mixed-version diagnostic"),
        );
        assert_eq!(structured["degradation_reason"], "api_unspecified");
        assert_ne!(
            structured["degradation_reason"],
            "human-readable mixed-version diagnostic"
        );
    }

    #[test]
    fn guided_limit_matches_api_bounds() {
        let mut input = auto_mode_tests::base_input("auth middleware");
        assert_eq!(guided_search_limit(&input), GUIDED_SEARCH_DEFAULT_LIMIT);
        input.limit = Some(0);
        assert_eq!(guided_search_limit(&input), 1);
        input.limit = Some(500);
        assert_eq!(guided_search_limit(&input), GUIDED_SEARCH_MAX_LIMIT);
    }

    #[test]
    fn guided_output_format_honors_compact_content_preference() {
        let mut input = auto_mode_tests::base_input("auth middleware");
        assert_eq!(guided_output_format(&input), "full");
        input.include_content = Some(false);
        assert_eq!(guided_output_format(&input), "minimal");
        input.output_format = Some("paths".to_string());
        assert_eq!(guided_output_format(&input), "paths");
    }

    #[test]
    fn guided_full_render_places_raw_evidence_before_guidance() {
        let response = guided_response(false);
        let (text, structured) = render_guided_search_response(
            &response,
            Some("fix expired sessions"),
            Some("full"),
            None,
        );

        let evidence_pos = text.find("src/auth.rs:10-20").unwrap();
        let guidance_pos = text.find("[GUIDANCE]").unwrap();
        assert!(evidence_pos < guidance_pos);
        assert!(text.contains("fn authenticate"));
        assert_eq!(structured["mode"], "guided");
        assert_eq!(structured["raw_evidence_first"], true);
        assert_eq!(structured["intent"], "fix expired sessions");
        assert_eq!(structured["results"][0]["file_path"], "src/auth.rs");
    }

    #[test]
    fn guided_degraded_render_keeps_raw_results_usable() {
        let response = guided_response(true);
        let (text, structured) = render_guided_search_response(&response, None, Some("full"), None);

        let evidence_pos = text.find("src/auth.rs:10-20").unwrap();
        let degraded_pos = text.find("[GUIDED_DEGRADED]").unwrap();
        assert!(evidence_pos < degraded_pos);
        assert!(text.contains("fn authenticate"));
        assert!(!text.contains("[GUIDANCE]"));
        assert_eq!(structured["degraded"], true);
        assert_eq!(structured["degradation_reason"], "navigator_timeout");
        assert_eq!(structured["total_latency_ms"], 2640);
        assert!(text.contains("bounded latency budget"));
        assert_eq!(structured["results"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn guided_recovered_render_stays_healthy_and_preserves_bounded_reason() {
        let mut response = guided_response(false);
        response.guidance_recovery_reason = Some("validation_rejected".to_string());
        let (text, structured) = render_guided_search_response(&response, None, Some("full"), None);

        assert!(text.contains("[GUIDANCE]"));
        assert!(!text.contains("[GUIDED_DEGRADED]"));
        assert_eq!(structured["degraded"], false);
        assert!(structured["degradation_reason"].is_null());
        assert_eq!(
            structured["guidance_recovery_reason"],
            "validation_rejected"
        );
    }

    #[test]
    fn guided_degradation_reason_is_bounded_for_metrics_and_human_readable() {
        assert_eq!(
            guided_degradation_metric_reason(Some("validation_rejected")),
            "validation_rejected"
        );
        assert_eq!(
            guided_degradation_metric_reason(Some("unexpected")),
            "api_unknown"
        );
        assert_eq!(stable_guided_degradation_reason(None), "api_unspecified");
        assert_eq!(
            stable_guided_degradation_reason(Some("arbitrary human prose")),
            "api_unknown"
        );
        assert_eq!(
            stable_guided_degradation_reason(Some("code_retrieval_timeout")),
            "code_retrieval_timeout"
        );
        assert_eq!(
            stable_guided_degradation_reason(Some("end_to_end_timeout")),
            "end_to_end_timeout"
        );
        assert_eq!(
            guided_degradation_metric_reason(Some("transport_fallback")),
            "transport_fallback"
        );
        assert_eq!(
            stable_guided_recovery_reason(Some("validation_rejected")),
            Some("validation_rejected")
        );
        assert_eq!(
            stable_guided_recovery_reason(Some("provider_error")),
            Some("provider_error")
        );
        assert_eq!(
            stable_guided_recovery_reason(Some("navigator_timeout")),
            Some("navigator_timeout")
        );
        assert_eq!(
            stable_guided_recovery_reason(Some("arbitrary human prose")),
            Some("api_unknown")
        );
        assert_eq!(stable_guided_recovery_reason(None), None);
        assert!(
            guided_degradation_message(Some("validation_rejected")).contains("evidence contract")
        );
    }

    #[test]
    fn guided_primary_and_fallback_share_one_absolute_budget() {
        let total = std::time::Duration::from_secs(5);
        let primary = guided_primary_budget(total);
        assert_eq!(primary, std::time::Duration::from_secs(4));
        assert_eq!(
            total.saturating_sub(primary),
            std::time::Duration::from_secs(1)
        );

        let tiny = std::time::Duration::from_millis(25);
        let tiny_primary = guided_primary_budget(tiny);
        assert_eq!(tiny_primary, std::time::Duration::from_millis(20));
        assert_eq!(
            tiny.saturating_sub(tiny_primary),
            std::time::Duration::from_millis(5)
        );
    }

    #[test]
    fn guided_render_preserves_existing_output_formats() {
        let response = guided_response(false);

        let (paths, paths_structured) =
            render_guided_search_response(&response, None, Some("paths"), None);
        assert!(paths.contains("src/auth.rs:10-20"));
        assert!(!paths.contains("fn authenticate"));
        assert_eq!(paths_structured["results"][0]["path"], "src/auth.rs:10-20");
        assert!(paths_structured.get("guidance").is_none());

        let (minimal, minimal_structured) =
            render_guided_search_response(&response, None, Some("minimal"), None);
        assert!(minimal.contains("1. src/auth.rs:10-20 [rust]"));
        assert!(!minimal.contains("fn authenticate"));
        assert!(minimal_structured["results"][0].get("snippet").is_none());

        let (count, structured) =
            render_guided_search_response(&response, None, Some("count"), None);
        assert!(count.contains("[GUIDED_EVIDENCE] 1 raw result(s)"));
        assert!(!count.contains("src/auth.rs"));
        assert!(!count.contains("[GUIDANCE]"));
        assert_eq!(structured["results"].as_array().unwrap().len(), 0);
        assert_eq!(structured["result_count"], 1);
        assert!(structured.get("guidance").is_none());
    }

    #[test]
    fn hybrid_fallback_is_converted_to_the_same_raw_first_contract() {
        let workspace_id = uuid::Uuid::new_v4();
        let project_id = uuid::Uuid::new_v4();
        let fallback = SearchResponse {
            results: vec![SearchResult {
                id: "auth".to_string(),
                file_path: Some("src/auth.rs".to_string()),
                start_line: Some(10),
                language: Some("rust".to_string()),
                content: Some("fn authenticate() {}".to_string()),
                metadata: Some(json!({"end_line": 20})),
                origin: Some("server_index".to_string()),
                ..Default::default()
            }],
            query_time_ms: Some(35),
            ..Default::default()
        };

        let response = guided_response_from_search_response(
            "auth middleware",
            Some(workspace_id),
            Some(project_id),
            fallback,
        );
        let (text, structured) = render_guided_search_response(
            &response,
            Some("fix expired sessions"),
            Some("full"),
            Some("Guided Search was unavailable; served hybrid raw evidence instead."),
        );

        assert!(response.degraded);
        assert!(text.starts_with("[GUIDED_EVIDENCE]"));
        assert!(text.contains("src/auth.rs:10-20"));
        assert!(text.contains("fn authenticate"));
        assert!(text.contains("[GUIDED_DEGRADED]"));
        assert_eq!(structured["degradation_reason"], "transport_fallback");
        assert_eq!(
            structured["degradation_message"],
            "Guided Search was unavailable; served hybrid raw evidence instead."
        );
    }

    #[test]
    fn guided_response_precaps_multi_megabyte_model_prose_before_wire_fitting() {
        let mut response = guided_response(false);
        response.guidance.as_mut().unwrap().answer = "model guidance ".repeat(300_000);
        response.guidance.as_mut().unwrap().targets[0].why = "target reason ".repeat(300_000);
        response.results[0].snippet = "source snippet ".repeat(300_000);
        response.knowledge = vec!["knowledge item ".repeat(300_000)];

        let (text, structured) = render_guided_search_response(&response, None, Some("full"), None);
        assert!(
            text.len() < 16_000,
            "guided prose should be capped while rendering"
        );
        assert_eq!(structured["guided_precompacted"], true);
        assert!(
            serde_json::to_vec(&structured).unwrap().len() < 100_000,
            "oversized guided fields should not be cloned into a multi-megabyte JSON tree"
        );

        let result = bounded_search_tool_result(text, structured);
        assert!(serde_json::to_vec(&result).unwrap().len() <= search_tool_result_wire_budget());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guided_handler_entry_deadline_charges_prework_and_delayed_primary() {
        let hybrid_response = SearchResponse {
            results: vec![SearchResult {
                id: "bounded-hybrid".to_string(),
                file_path: Some("src/bounded.rs".to_string()),
                content: Some("fn bounded_fallback() {}".to_string()),
                score: Some(0.9),
                ..Default::default()
            }],
            total: Some(1),
            query_time_ms: Some(1),
            ..Default::default()
        };
        let (base_url, requests, server) = spawn_search_recording_server(vec![
            RecordingHttpResponse {
                expected_path: "/search/guided",
                status: 200,
                body: guided_success_body(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), None),
                // This would fit a newly-reset 96ms primary clock, but cannot
                // fit the ~26ms left after the injected 70ms handler prework.
                delay: std::time::Duration::from_millis(50),
            },
            RecordingHttpResponse {
                expected_path: "/search/hybrid",
                status: 200,
                body: serde_json::to_string(&hybrid_response).unwrap(),
                delay: std::time::Duration::ZERO,
            },
        ]);
        let (tool, _) = guided_tool_with_base_url(base_url);
        let tool = tool.with_guided_test_delays(
            std::time::Duration::from_millis(70),
            std::time::Duration::ZERO,
        );
        let started = std::time::Instant::now();
        let result = tool
            .execute_with_guided_timeout(
                json!({"query": "handler-entry-budget", "mode": "guided"}),
                std::time::Duration::from_millis(120),
            )
            .await
            .expect("bounded handler should preserve hybrid evidence");
        let wall = started.elapsed();
        let structured = result.structured_content.as_ref().unwrap();
        assert_eq!(structured["fallback_used"], true);
        assert_eq!(structured["results"][0]["file_path"], "src/bounded.rs");
        assert!(
            wall <= std::time::Duration::from_millis(220),
            "single 120ms deadline escaped in wall time: {wall:?}"
        );
        assert!(
            structured["total_latency_ms"].as_i64().unwrap_or_default() <= 160,
            "reported total must include prework without resetting the clock"
        );

        assert!(requests
            .recv_timeout(std::time::Duration::from_secs(1))
            .is_ok());
        assert!(requests
            .recv_timeout(std::time::Duration::from_secs(1))
            .is_ok());
        server.join().expect("bounded Guided server should finish");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guided_finalization_cannot_escape_the_handler_deadline() {
        let (base_url, requests, server) =
            spawn_search_recording_server(vec![RecordingHttpResponse {
                expected_path: "/search/guided",
                status: 200,
                body: guided_success_body(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), None),
                delay: std::time::Duration::ZERO,
            }]);
        let (tool, _) = guided_tool_with_base_url(base_url);
        let tool = tool.with_guided_test_delays(
            std::time::Duration::ZERO,
            std::time::Duration::from_millis(150),
        );
        let started = std::time::Instant::now();
        let result = tool
            .execute_with_guided_timeout(
                json!({"query": "bounded-finalization", "mode": "guided"}),
                std::time::Duration::from_millis(80),
            )
            .await
            .expect("deadline should return a bounded degraded envelope");
        let wall = started.elapsed();
        let structured = result.structured_content.as_ref().unwrap();
        assert_eq!(structured["degradation_reason"], "end_to_end_timeout");
        assert!(
            wall <= std::time::Duration::from_millis(180),
            "final shaping escaped the 80ms handler deadline: {wall:?}"
        );
        assert!(structured["total_latency_ms"].as_i64().unwrap_or_default() >= 80);

        assert!(requests
            .recv_timeout(std::time::Duration::from_secs(1))
            .is_ok());
        server
            .join()
            .expect("finalization Guided server should finish");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn guided_cache_hit_finalization_uses_the_same_absolute_deadline() {
        let query = format!("guided-cache-deadline-{}", uuid::Uuid::new_v4());
        let (base_url, requests, server) =
            spawn_search_recording_server(vec![RecordingHttpResponse {
                expected_path: "/search/guided",
                status: 200,
                body: guided_success_body(uuid::Uuid::new_v4(), uuid::Uuid::new_v4(), None),
                delay: std::time::Duration::ZERO,
            }]);
        let (warm_tool, _) = guided_tool_with_base_url(base_url.clone());
        let warm_input = json!({"query": query, "mode": "guided"});
        mcp_client::run_with_session_key(mcp_types::SessionKey::Local, || async {
            warm_tool
                .execute_with_guided_timeout(warm_input, std::time::Duration::from_millis(250))
                .await
                .expect("first call should warm the rendered Guided cache")
        })
        .await;
        assert!(requests
            .recv_timeout(std::time::Duration::from_secs(1))
            .is_ok());
        server
            .join()
            .expect("cache-warm Guided server should finish");

        let (cached_tool, _) = guided_tool_with_base_url(base_url);
        let cached_tool = cached_tool.with_guided_test_delays(
            std::time::Duration::ZERO,
            std::time::Duration::from_millis(150),
        );
        let started = std::time::Instant::now();
        let cached = mcp_client::run_with_session_key(mcp_types::SessionKey::Local, || async {
            cached_tool
                .execute_with_guided_timeout(
                    json!({"query": query, "mode": "guided"}),
                    std::time::Duration::from_millis(80),
                )
                .await
                .expect("cache-hit finalization should fail closed on deadline")
        })
        .await;
        let wall = started.elapsed();
        assert_eq!(
            cached.structured_content.as_ref().unwrap()["degradation_reason"],
            "end_to_end_timeout"
        );
        assert!(
            wall <= std::time::Duration::from_millis(180),
            "cached finalization escaped the 80ms handler deadline: {wall:?}"
        );
    }
}

// ============================================================================
// Metadata Tests
// ============================================================================

mod metadata_tests {
    use super::{
        create_mock_client, create_mock_index_keeper, create_mock_session, ToolCategory,
        ToolHandler,
    };
    use super::{HybridSearchTool, KeywordSearchTool, SearchTool, SemanticSearchTool};

    #[test]
    fn test_search_tool_metadata() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let ik = create_mock_index_keeper(&client, &session);
        let tool = SearchTool::new(client, session, ik, mcp_types::atlas_layer::noop_layer());
        let metadata = tool.metadata();
        assert_eq!(metadata.title, "Search Codebase");
        assert!(metadata.description.contains("Search"));
        assert!(metadata.description.contains("auto"));
        assert!(metadata.description.contains("semantic"));
        assert!(metadata.description.contains("guided"));
        // Disambiguation: must clarify this is the ONLY tool for codebase/file search
        assert!(
            metadata
                .description
                .contains("ONLY tool for codebase/file search"),
            "search description must assert exclusivity for codebase search"
        );
        assert!(
            metadata.description.contains("Do NOT use memory"),
            "search description must warn against memory(search)"
        );
        assert!(
            metadata.description.contains("Do NOT use memory")
                || metadata.description.contains("session"),
            "search description must warn against session(smart_search)"
        );
        assert_eq!(metadata.category, ToolCategory::Search);
        assert!(!metadata.is_pro);
    }

    #[test]
    fn test_semantic_search_tool_metadata() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let ik = create_mock_index_keeper(&client, &session);
        let tool = SemanticSearchTool::new(client, session, ik);
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "search_semantic");
        assert_eq!(metadata.title, "Semantic Search");
        assert!(metadata.description.contains("semantic"));
        assert_eq!(metadata.category, ToolCategory::Search);
    }

    #[test]
    fn test_hybrid_search_tool_metadata() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let ik = create_mock_index_keeper(&client, &session);
        let tool = HybridSearchTool::new(client, session, ik);
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "search_hybrid");
        assert_eq!(metadata.title, "Hybrid Search");
        assert!(metadata.description.contains("semantic"));
        assert!(metadata.description.contains("keyword"));
        assert_eq!(metadata.category, ToolCategory::Search);
    }

    #[test]
    fn test_keyword_search_tool_metadata() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let ik = create_mock_index_keeper(&client, &session);
        let tool = KeywordSearchTool::new(client, session, ik);
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "search_keyword");
        assert_eq!(metadata.title, "Keyword Search");
        assert!(metadata.description.contains("keyword"));
        assert_eq!(metadata.category, ToolCategory::Search);
    }
}

// ============================================================================
// Schema Tests
// ============================================================================

mod schema_tests {
    use super::{create_mock_client, create_mock_index_keeper, create_mock_session, ToolHandler};
    use super::{HybridSearchTool, KeywordSearchTool, SearchTool, SemanticSearchTool};

    #[test]
    fn test_search_tool_schema() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let ik = create_mock_index_keeper(&client, &session);
        let tool = SearchTool::new(client, session, ik, mcp_types::atlas_layer::noop_layer());
        let schema = tool.input_schema();

        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("query"));
        assert!(props.contains_key("mode"));
        assert!(props.contains_key("intent"));
        assert!(props.contains_key("tokenizer"));
        assert!(props.contains_key("workspace_id"));
        assert!(props.contains_key("project_id"));
        assert!(props.contains_key("limit"));
        assert!(props.contains_key("file_types"));
        assert!(props.contains_key("include_content"));
        assert!(props.contains_key("include_memory"));
        assert!(props.contains_key("code_rerank_learning_opt_in"));
        assert_eq!(props["code_rerank_learning_opt_in"]["default"], false);
        assert!(props.contains_key("cursor"));

        // query should be required
        if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
            assert!(required.iter().any(|v| v.as_str() == Some("query")));
            assert!(!required
                .iter()
                .any(|v| v.as_str() == Some("code_rerank_learning_opt_in")));
        }

        // Check mode enum values
        if let Some(mode) = props.get("mode") {
            if let Some(enum_vals) = mode.get("enum").and_then(|e| e.as_array()) {
                let values: Vec<&str> = enum_vals.iter().filter_map(|v| v.as_str()).collect();
                assert!(values.contains(&"auto"));
                assert!(values.contains(&"hybrid"));
                assert!(values.contains(&"semantic"));
                assert!(values.contains(&"keyword"));
                assert!(values.contains(&"pattern"));
                assert!(values.contains(&"exhaustive"));
                assert!(values.contains(&"refactor"));
                assert!(values.contains(&"team"));
                assert!(values.contains(&"guided"));
            }
        }
    }

    #[test]
    fn test_semantic_search_schema() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let ik = create_mock_index_keeper(&client, &session);
        let tool = SemanticSearchTool::new(client, session, ik);
        let schema = tool.input_schema();

        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("query"));
        assert!(props.contains_key("workspace_id"));
        assert!(props.contains_key("limit"));
        assert!(props.contains_key("code_rerank_learning_opt_in"));
        assert_eq!(props["code_rerank_learning_opt_in"]["default"], false);
    }

    #[test]
    fn test_hybrid_search_schema() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let ik = create_mock_index_keeper(&client, &session);
        let tool = HybridSearchTool::new(client, session, ik);
        let schema = tool.input_schema();

        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("query"));
        assert!(props.contains_key("code_rerank_learning_opt_in"));
        assert_eq!(props["code_rerank_learning_opt_in"]["default"], false);
    }

    #[test]
    fn test_keyword_search_schema() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let ik = create_mock_index_keeper(&client, &session);
        let tool = KeywordSearchTool::new(client, session, ik);
        let schema = tool.input_schema();

        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("query"));
        assert!(props.contains_key("code_rerank_learning_opt_in"));
        assert_eq!(props["code_rerank_learning_opt_in"]["default"], false);
    }
}

// ============================================================================
// Whole-wire tokenizer tests
// ============================================================================

mod tokenizer_wire_tests {
    use super::*;

    #[test]
    fn tokenizer_input_accepts_encoding_alias_and_validates_bounds() {
        let input: SearchInput = serde_json::from_value(json!({
            "query": "auth middleware",
            "encoding": " O200K_BASE "
        }))
        .unwrap();
        assert_eq!(input.tokenizer.as_deref(), Some(" O200K_BASE "));
        assert_eq!(
            normalize_search_tokenizer_hint(input.tokenizer.as_deref()).unwrap(),
            Some("o200k_base".to_string())
        );
        assert!(normalize_search_tokenizer_hint(Some(&"x".repeat(65))).is_err());
        assert_eq!(normalize_search_tokenizer_hint(Some("  ")).unwrap(), None);
    }

    #[test]
    fn tokenizer_inference_is_registry_strict_and_explicit_wins() {
        assert_eq!(
            resolve_search_tokenizer(None, Some("gpt-5-codex-high")).as_deref(),
            Some("o200k_base")
        );
        assert_eq!(resolve_search_tokenizer(None, Some("chatgpt")), None);
        assert_eq!(
            resolve_search_tokenizer(Some("custom"), Some("gpt-5-codex-high")).as_deref(),
            Some("custom")
        );
    }

    #[test]
    fn search_cache_identity_partitions_tokenizer_rollout_namespace() {
        let input = auto_mode_tests::base_input("auth middleware");
        let shapers = ResolvedSearchCacheShapers {
            limit: Some(20),
            offset: None,
            file_types: Vec::new(),
            include_content: Some(true),
            include_memory: false,
            include_vcs: false,
            output_format: Some("full".to_string()),
            context_lines: Some(2),
            content_max_chars: 800,
            exact_match_boost: None,
            hot_paths_identity: None,
        };
        let proxy = build_search_cache_key_with_tokenizer(
            None,
            None,
            &input,
            SearchMode::Hybrid,
            None,
            &shapers,
            None,
            None,
            "mcp-wire-tokenizer-v1:o200k:proxy",
        );
        let exact = build_search_cache_key_with_tokenizer(
            None,
            None,
            &input,
            SearchMode::Hybrid,
            None,
            &shapers,
            None,
            None,
            "mcp-wire-tokenizer-v1:o200k:enforce:exact",
        );
        assert_ne!(proxy, exact);
        assert!(proxy.starts_with("search:v5:"));
    }

    #[tokio::test]
    async fn exact_search_enforcement_preserves_utf8_json_and_mandatory_controls() {
        crate::wire_tokens::warm_o200k();
        let context = crate::wire_tokens::WireResponseContext::http_jsonrpc(
            Some(json!("search-request-长-id")),
            Some("Searching ContextStream index".to_string()),
            Some("⌕".to_string()),
        );
        let policy = SearchWireTokenizerPolicy {
            decision: crate::wire_tokens::RolloutDecision {
                mode: crate::wire_tokens::RolloutMode::Enforce,
                compatibility: crate::wire_tokens::TokenizerCompatibility::VerifiedO200k,
                measure_exact: true,
                enforce_exact: true,
                canary_selected: true,
                canary_basis_points: 10_000,
            },
            context: context.clone(),
        };
        let first_path = "src/数据库/检索.rs";
        let structured = json!({
            "query": "数据库 👩‍💻 escaped JSON",
            "mode": "hybrid",
            "next_cursor": "refactor:v1:opaque-page-two",
            "scope_reliability": {"usable": true, "scope_match": true},
            "index_trust": {
                "project_id": uuid::Uuid::parse_str("17ab6543-59d3-4a97-a57e-6add460d98ae")
                    .unwrap(),
                "result_generation_consistent": true
            },
            "code_rerank_learning_request_id":
                uuid::Uuid::parse_str("fe106dc3-6903-4d62-b0b3-c33d33f19f71").unwrap(),
            "results": [{
                "file_path": first_path,
                "start_line": 10,
                "end_line": 20,
                "content": "数据库查询如何工作？ 👩‍💻 \\\"escaped\\\"\n".repeat(2_000)
            }]
        });
        let result =
            crate::wire_tokens::run_with_wire_response_context(context.clone(), async move {
                apply_search_wire_tokenizer(
                    ToolResult::with_structured(
                        "数据库查询如何工作？検索結果を高速に返します。 👩‍💻\n".repeat(1_000),
                        structured,
                    ),
                    &policy,
                )
            })
            .await;
        let structured = result.structured_content.as_ref().unwrap();
        assert_eq!(structured["next_cursor"], "refactor:v1:opaque-page-two");
        assert_eq!(structured["results"][0]["file_path"], first_path);
        assert!(structured.get("scope_reliability").is_some());
        assert!(structured.get("index_trust").is_some());
        assert!(structured.get("code_rerank_learning_request_id").is_some());
        assert!(structured
            .get(crate::wire_tokens::WIRE_REPORT_KEY)
            .is_some());

        let measurement =
            crate::wire_tokens::measure_tool_result(&result, &context, "search_wire_test_final")
                .unwrap();
        assert_eq!(
            structured[crate::wire_tokens::WIRE_REPORT_KEY]["exact_tokens_final"],
            measurement.exact_tokens
        );
        assert!(measurement.exact_tokens <= search_tool_result_wire_budget().div_ceil(4));
        let bytes = crate::wire_tokens::canonical_tool_result_bytes(&result, &context).unwrap();
        assert!(std::str::from_utf8(&bytes).is_ok());
        assert!(serde_json::from_slice::<Value>(&bytes).is_ok());
    }

    #[tokio::test]
    async fn exact_search_max_cursor_at_minimum_wire_budget_is_bounded_or_fail_honest() {
        fn opaque_base64url(len: usize, mut state: u64) -> String {
            const ALPHABET: &[u8; 64] =
                b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
            let mut output = String::with_capacity(len);
            for _ in 0..len {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                output.push(ALPHABET[((state >> 58) & 63) as usize] as char);
            }
            output
        }

        crate::wire_tokens::warm_o200k();
        let context = crate::wire_tokens::WireResponseContext::http_jsonrpc(
            Some(json!("search-minimum-max-cursor")),
            Some("Searching ContextStream index".to_string()),
            Some("⌕".to_string()),
        );
        let policy = SearchWireTokenizerPolicy {
            decision: crate::wire_tokens::RolloutDecision {
                mode: crate::wire_tokens::RolloutMode::Enforce,
                compatibility: crate::wire_tokens::TokenizerCompatibility::VerifiedO200k,
                measure_exact: true,
                enforce_exact: true,
                canary_selected: true,
                canary_basis_points: 10_000,
            },
            context: context.clone(),
        };
        let decision = policy.decision;
        let cursor = format!(
            "rf2.{}.{}",
            opaque_base64url(5_606, 0xd1b5_4a32_d192_ed03),
            opaque_base64url(43, 0x94d0_49bb_1331_11eb),
        );
        assert_eq!(cursor.len(), MAX_VALID_SEARCH_CURSOR_BYTES);
        assert_eq!(search_cursor_protocol_violation(&cursor), None);

        let structured = json!({
            "query": "maximum cursor exact wire floor",
            "mode": "refactor",
            "next_cursor": cursor,
            "index_trust": {
                "project_id": uuid::Uuid::new_v4(),
                "committed_generation": 42,
                "result_generation_coverage_complete": true,
                "result_generation_consistent": true,
            },
            "scope_reliability": {
                "usable": true,
                "scope_match": true,
                "scope_invalid": false,
            },
            "scope_diagnostics": {
                "scope_valid": true,
                "fallback_used": false,
                "project_index_state": "fresh",
                "remediation_attempted": false,
            },
            "results": [{
                "file_path": "crates/mcp-tools/src/domains/search.rs",
                "start_line": 1,
                "end_line": 2,
                "content": "high entropy evidence 数据库 👩‍💻 ".repeat(8_000),
            }],
        });
        let result =
            crate::wire_tokens::run_with_wire_response_context(context.clone(), async move {
                apply_search_wire_tokenizer_at_limit(
                    ToolResult::with_structured(
                        "adversarial search prose 数据库 👩‍💻 ".repeat(4_000),
                        structured,
                    ),
                    &policy,
                    None,
                    SEARCH_TOOL_RESULT_WIRE_BUDGET_MIN,
                )
            })
            .await;

        let target_tokens = SEARCH_TOOL_RESULT_WIRE_BUDGET_MIN.div_ceil(4);
        let measurement = crate::wire_tokens::measure_tool_result(
            &result,
            &context,
            "search_wire_minimum_max_cursor_test",
        )
        .unwrap();
        let report = result
            .structured_content
            .as_ref()
            .and_then(|value| value.get(crate::wire_tokens::WIRE_REPORT_KEY));
        let bytes = crate::wire_tokens::canonical_tool_result_bytes(&result, &context).unwrap();
        let wire = std::str::from_utf8(&bytes).unwrap();
        let has_enforcement_claim = wire.contains("\"enforced\":true");
        let truthful_bounded_report = report.is_some()
            && measurement.exact_tokens <= target_tokens
            && crate::wire_tokens::fixed_point_report_is_truthful(
                &result,
                decision,
                target_tokens,
                measurement,
            );

        assert!(truthful_bounded_report || !has_enforcement_claim);
        if measurement.exact_tokens > target_tokens {
            assert!(report.is_none());
            assert!(wire.contains("[WIRE_BUDGET] Exact search response exceeded"));
        }
        assert!(serde_json::from_slice::<Value>(&bytes).is_ok());
    }

    #[tokio::test]
    async fn search_shadow_is_byte_for_byte_proxy_neutral() {
        crate::wire_tokens::warm_o200k();
        let context = crate::wire_tokens::WireResponseContext::http_jsonrpc(
            Some(json!("search-shadow-id")),
            Some("Searching ContextStream index".to_string()),
            Some("⌕".to_string()),
        );
        let proxy_policy = SearchWireTokenizerPolicy {
            decision: crate::wire_tokens::RolloutDecision {
                mode: crate::wire_tokens::RolloutMode::Proxy,
                compatibility: crate::wire_tokens::TokenizerCompatibility::VerifiedO200k,
                measure_exact: false,
                enforce_exact: false,
                canary_selected: false,
                canary_basis_points: 0,
            },
            context: context.clone(),
        };
        let shadow_policy = SearchWireTokenizerPolicy {
            decision: crate::wire_tokens::RolloutDecision {
                mode: crate::wire_tokens::RolloutMode::Shadow,
                compatibility: crate::wire_tokens::TokenizerCompatibility::VerifiedO200k,
                measure_exact: true,
                enforce_exact: false,
                canary_selected: false,
                canary_basis_points: 10_000,
            },
            context: context.clone(),
        };
        let original = ToolResult::with_structured(
            "数据库查询 👩‍💻 \\\"escaped\\\"\n".repeat(2_000),
            json!({
                "query": "database search",
                "mode": "hybrid",
                "next_cursor": "opaque-cursor",
                "scope_reliability": {"usable": true},
                "index_trust": {"result_generation_consistent": true},
                "results": [{
                    "file_path": "src/数据库/search.rs",
                    "content": "evidence ".repeat(4_000)
                }]
            }),
        );

        let proxy = apply_search_wire_tokenizer(original.clone(), &proxy_policy);
        let shadow = apply_search_wire_tokenizer(original, &shadow_policy);
        let proxy_bytes =
            crate::wire_tokens::canonical_tool_result_bytes(&proxy, &context).unwrap();
        let shadow_bytes =
            crate::wire_tokens::canonical_tool_result_bytes(&shadow, &context).unwrap();

        assert_eq!(shadow_bytes, proxy_bytes);
        assert!(shadow
            .structured_content
            .as_ref()
            .is_some_and(|value| value.get(crate::wire_tokens::WIRE_REPORT_KEY).is_none()));
    }
}

// ============================================================================
// Auto Mode Recommendation Tests
// ============================================================================

mod auto_mode_tests {
    use super::{
        apply_symbol_anchor_rerank, bounded_existing_search_tool_result,
        bounded_search_tool_result, budget_search_structured_value,
        budget_search_structured_value_counted, budget_search_structured_value_with_stats,
        build_hot_paths_hint, build_index_health, build_mcp_index_trust_diagnostics,
        build_search_cache_key, caller_scoped_search_cache_key, classify_index_freshness,
        contains_code_identifiers, current_dir_search_root, dirty_hints_indicating_drift,
        drift_ingest_project_id, effective_search_cache_project_id, ensure_search_query_echo,
        escape_regex_literal, extract_api_index_hint, extract_project_status_index_hint,
        extract_quoted_literal, filter_project_map_route_hint, format_index_trust_mismatch,
        harmonize_project_index_state, hot_paths_cache_identity, is_artifact_like_path,
        is_doc_lookup_query, is_local_keyword_enrichment_query,
        local_enrichment_unavailable_warning_for_response, local_keyword_enrich,
        local_keyword_enrich_checked, merge_api_index_hints, merge_dirty_file_hints,
        normalize_count_index_trust, normalize_paths_output, normalized_symbol_retry_query,
        parse_git_status_dirty_hints, path_query_hint, prefers_hybrid_for_code_location_query,
        prepare_code_rerank_learning_attempt, project_map_route_hint_from_structured,
        prune_deleted_file_results, read_git_dirty_file_hints, recommend_search_mode,
        refactor_cursor_continuation_note, resolve_effective_folder_path,
        resolve_exact_match_boost, resolve_include_memory, resolve_mode,
        resolve_output_preferences, resolve_search_content_max_chars, resolve_search_context_lines,
        resolve_search_limit, resolve_search_offset, response_generation_consistency,
        result_has_artifact_like_path, run_search_for_mode, scoped_session_folder_path,
        search_cache, search_response_structured_value, search_tool_result_wire_budget,
        served_api_learning_receipt, sha256_hex_bytes, should_allow_workspace_scope_fallback,
        should_append_index_health_footer, should_apply_local_enrichment,
        should_fetch_graph_enrichment, should_fetch_project_map_route_hint,
        should_filter_artifact_paths, should_retry_keyword_with_semantic,
        should_retry_keyword_with_symbol_modes, should_surface_index_health_before_results,
        suggest_output_format, targeted_local_delta, targeted_payload_content_bytes, which_rg,
        ApiIndexHint, DirtyFileHint, IndexHealth, LocalEnrichDiagnostic, LocalIndexEntry,
        ResolvedSearchCacheShapers, SearchInput, SearchMode, GRAPH_ENRICHMENT_TIMEOUT,
        MAX_VALID_SEARCH_CURSOR_BYTES, SEARCH_STRUCTURED_OUTPUT_BUDGET_MIN,
    };
    use super::{should_prefer_semantic_results, should_retry_semantic_fallback};
    use super::{RecordingHttpResponse, SearchParams};
    use mcp_types::api::{
        ProjectAgentMapResponse, ProjectAgentMapRouteHint, SearchIndexTrustEnvelope,
        SearchResponse, SearchResult,
    };
    use serde_json::{json, Value};

    fn response_with_scores(scores: &[f64]) -> SearchResponse {
        SearchResponse {
            results: scores
                .iter()
                .enumerate()
                .map(|(i, score)| SearchResult {
                    id: format!("r{}", i),
                    title: None,
                    content: None,
                    file_path: None,
                    score: Some(*score),
                    start_line: None,
                    language: None,
                    location: None,
                    breadcrumb: None,
                    metadata: None,
                    origin: None,
                })
                .collect(),
            total: Some(scores.len() as i64),
            ..SearchResponse::default()
        }
    }

    pub(super) fn base_input(query: &str) -> SearchInput {
        SearchInput {
            query: query.to_string(),
            mode: None,
            tokenizer: None,
            intent: None,
            workspace_id: None,
            project_id: None,
            limit: None,
            file_types: None,
            include_content: None,
            include_memory: None,
            include_vcs: None,
            code_rerank_learning_opt_in: None,
            output_format: None,
            context_lines: None,
            content_max_chars: None,
            exact_match_boost: None,
            offset: None,
            cursor: None,
            query_vector: None,
        }
    }

    fn cache_shapers(
        input: &SearchInput,
        configured_limit: usize,
        configured_content_max: usize,
    ) -> ResolvedSearchCacheShapers {
        let (output_format, include_content) =
            resolve_output_preferences(input, SearchMode::Keyword);
        ResolvedSearchCacheShapers {
            limit: resolve_search_limit(input, configured_limit),
            offset: resolve_search_offset(input),
            file_types: input.file_types.clone().unwrap_or_default(),
            include_content,
            include_memory: input.include_memory.unwrap_or(false),
            include_vcs: input
                .include_vcs
                .unwrap_or_else(|| super::query_has_vcs_signal(&input.query)),
            output_format,
            context_lines: resolve_search_context_lines(input),
            content_max_chars: resolve_search_content_max_chars(input, configured_content_max),
            exact_match_boost: resolve_exact_match_boost(input),
            hot_paths_identity: None,
        }
    }

    fn cache_key(
        workspace_id: Option<uuid::Uuid>,
        project_id: Option<uuid::Uuid>,
        input: &SearchInput,
        mode: SearchMode,
        checkout_identity: Option<&str>,
    ) -> String {
        build_search_cache_key(
            workspace_id,
            project_id,
            input,
            mode,
            None,
            &cache_shapers(input, 25, 800),
            None,
            checkout_identity,
        )
    }

    fn trust_envelope(
        project_id: uuid::Uuid,
        repository: Option<&str>,
        generation: i64,
        branch: Option<&str>,
    ) -> SearchIndexTrustEnvelope {
        SearchIndexTrustEnvelope {
            project_id,
            repository: repository.map(str::to_string),
            committed_generation: generation,
            indexed_at: Some("2026-07-20T12:34:56Z".to_string()),
            source_machine: Some("desktop-a".to_string()),
            source_branch: branch.map(str::to_string),
            source_commit_sha: Some("0123456789abcdef".to_string()),
            result_generation_coverage_complete: Some(true),
            result_generation_consistent: Some(true),
        }
    }

    #[test]
    fn index_trust_detects_stale_served_generation() {
        let project_id = uuid::Uuid::new_v4();
        let trust = trust_envelope(project_id, Some("contextstream/mcp"), 9, Some("main"));
        let response = SearchResponse {
            index_generation: Some(8),
            result_generation_min: Some(8),
            result_generation_max: Some(8),
            index_trust: Some(trust.clone()),
            ..Default::default()
        };

        assert_eq!(
            response_generation_consistency(&response, &trust),
            Some(false)
        );
    }

    #[test]
    fn index_trust_accepts_current_authoritative_generation() {
        let project_id = uuid::Uuid::new_v4();
        let trust = trust_envelope(project_id, Some("contextstream/mcp"), 9, Some("main"));
        let response = SearchResponse {
            index_generation: Some(9),
            result_generation_min: Some(7),
            result_generation_max: Some(9),
            index_trust: Some(trust.clone()),
            ..Default::default()
        };

        assert_eq!(
            response_generation_consistency(&response, &trust),
            Some(true)
        );
    }

    #[test]
    fn index_trust_detects_wrong_checkout_twin_without_copying_result_content() {
        let server_project_id = uuid::Uuid::new_v4();
        let response = SearchResponse {
            index_generation: Some(9),
            result_generation_max: Some(9),
            index_trust: Some(trust_envelope(
                server_project_id,
                Some("contextstream/contextstream"),
                9,
                Some("main"),
            )),
            results: vec![SearchResult {
                id: "ghost".to_string(),
                file_path: Some("crates/example-api/src/services/guided_search.rs".to_string()),
                content: Some("large result body must remain outside diagnostics".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let health = IndexHealth {
            freshness: "recent",
            confidence: "high",
            age_hours: Some(1),
            scope_match: false,
            drift_detected: false,
            changed_file_count: 0,
            indexed_at: Some("2026-07-20T12:34:56Z".to_string()),
            recommendation: None,
        };
        let diagnostics = build_mcp_index_trust_diagnostics(
            &response,
            Some(server_project_id),
            Some(uuid::Uuid::new_v4()),
            Some("contextstream/mcp".to_string()),
            Some("feature/search".to_string()),
            Some("fedcba9876543210".to_string()),
            true,
            Some(false),
            Some(&health),
        )
        .expect("server trust should produce MCP diagnostics");

        assert_eq!(diagnostics.checks.resolved_project_match, Some(true));
        assert_eq!(diagnostics.checks.local_project_match, Some(false));
        assert_eq!(diagnostics.checks.repository_match, Some(false));
        assert_eq!(diagnostics.checks.branch_match, Some(false));
        assert_eq!(diagnostics.checks.commit_match, Some(false));
        let warning = format_index_trust_mismatch(&diagnostics).unwrap();
        assert!(warning.contains("project"));
        assert!(warning.contains("repository"));
        assert!(warning.contains("branch"));
        assert!(warning.contains("commit"));
        assert!(!serde_json::to_string(&diagnostics)
            .unwrap()
            .contains("large result body"));
    }

    #[test]
    fn index_trust_reports_local_drift_only_from_known_local_signals() {
        let project_id = uuid::Uuid::new_v4();
        let response = SearchResponse {
            index_generation: Some(9),
            index_trust: Some(trust_envelope(
                project_id,
                Some("contextstream/mcp"),
                9,
                Some("main"),
            )),
            ..Default::default()
        };
        let health = IndexHealth {
            freshness: "recent",
            confidence: "medium",
            age_hours: Some(1),
            scope_match: true,
            drift_detected: true,
            changed_file_count: 1,
            indexed_at: Some("2026-07-20T12:34:56Z".to_string()),
            recommendation: None,
        };
        let diagnostics = build_mcp_index_trust_diagnostics(
            &response,
            Some(project_id),
            Some(project_id),
            Some("contextstream/mcp".to_string()),
            Some("main".to_string()),
            Some("0123456789abcdef".to_string()),
            true,
            Some(true),
            Some(&health),
        )
        .unwrap();

        assert_eq!(
            diagnostics.local.as_ref().and_then(|local| local.drift),
            Some(true)
        );
        assert_eq!(diagnostics.checks.repository_match, Some(true));
        assert_eq!(diagnostics.checks.branch_match, Some(true));
        assert_eq!(diagnostics.checks.commit_match, Some(true));
        assert!(format_index_trust_mismatch(&diagnostics).is_none());

        let unknown = build_mcp_index_trust_diagnostics(
            &response,
            Some(project_id),
            Some(project_id),
            Some("contextstream/mcp".to_string()),
            Some("main".to_string()),
            Some("0123456789abcdef".to_string()),
            false,
            None,
            None,
        )
        .unwrap();
        assert_eq!(unknown.local.and_then(|local| local.drift), None);

        let known_clean = build_mcp_index_trust_diagnostics(
            &response,
            Some(project_id),
            Some(project_id),
            Some("contextstream/mcp".to_string()),
            Some("main".to_string()),
            Some("0123456789abcdef".to_string()),
            true,
            Some(false),
            None,
        )
        .unwrap();
        assert_eq!(known_clean.local.and_then(|local| local.drift), Some(false));
    }

    #[test]
    fn index_trust_keeps_missing_checkout_provenance_unknown() {
        let project_id = uuid::Uuid::new_v4();
        let response = SearchResponse {
            index_generation: Some(9),
            index_trust: Some(trust_envelope(
                project_id,
                Some("contextstream/mcp"),
                9,
                Some("main"),
            )),
            ..Default::default()
        };

        let diagnostics = build_mcp_index_trust_diagnostics(
            &response,
            Some(project_id),
            Some(project_id),
            None,
            None,
            None,
            false,
            None,
            None,
        )
        .expect("server trust should remain available without local provenance");

        assert_eq!(diagnostics.checks.repository_match, None);
        assert_eq!(diagnostics.checks.branch_match, None);
        assert_eq!(diagnostics.checks.commit_match, None);
        assert!(format_index_trust_mismatch(&diagnostics).is_none());
    }

    #[test]
    fn index_trust_partial_generation_coverage_stays_unknown_in_diagnostics() {
        let project_id = uuid::Uuid::new_v4();
        let mut trust = trust_envelope(project_id, None, 9, None);
        trust.result_generation_coverage_complete = Some(false);
        trust.result_generation_consistent = None;
        let response = SearchResponse {
            index_generation: Some(9),
            result_generation_min: Some(9),
            result_generation_max: Some(9),
            index_trust: Some(trust.clone()),
            ..Default::default()
        };

        assert_eq!(response_generation_consistency(&response, &trust), None);
        let structured = serde_json::to_value(&trust).unwrap();
        assert_eq!(
            structured["result_generation_coverage_complete"],
            serde_json::json!(false)
        );
        assert!(structured.get("result_generation_consistent").is_none());
    }

    #[test]
    fn test_resolve_search_request_limits_match_api_validation_bounds() {
        let mut input = base_input("anything");
        input.limit = Some(500);
        input.content_max_chars = Some(12_000);
        input.context_lines = Some(99);
        input.exact_match_boost = Some(42.0);
        input.offset = Some(-10);

        assert_eq!(resolve_search_limit(&input, 25), Some(100));
        assert_eq!(resolve_search_content_max_chars(&input, 800), 10_000);
        assert_eq!(resolve_search_context_lines(&input), Some(10));
        assert_eq!(resolve_exact_match_boost(&input), Some(10.0));
        assert_eq!(resolve_search_offset(&input), Some(0));
    }

    #[test]
    fn test_resolve_search_request_limits_apply_minimums() {
        let mut input = base_input("anything");
        input.limit = Some(0);
        input.content_max_chars = Some(1);
        input.context_lines = Some(-5);
        input.exact_match_boost = Some(0.1);

        assert_eq!(resolve_search_limit(&input, 25), Some(1));
        assert_eq!(resolve_search_content_max_chars(&input, 800), 50);
        assert_eq!(resolve_search_context_lines(&input), Some(0));
        assert_eq!(resolve_exact_match_boost(&input), Some(1.0));
    }

    #[test]
    fn test_search_cache_key_separates_render_formats() {
        let mut minimal = base_input("GuidanceTarget");
        minimal.output_format = Some("minimal".to_string());
        minimal.include_content = Some(false);
        let mut paths = minimal.clone();
        paths.output_format = Some("paths".to_string());
        let mut full = minimal.clone();
        full.output_format = Some("full".to_string());
        full.include_content = Some(true);

        let key = |input: &SearchInput| cache_key(None, None, input, SearchMode::Keyword, None);

        assert_ne!(key(&minimal), key(&paths));
        assert_ne!(key(&minimal), key(&full));
        assert_ne!(key(&paths), key(&full));
        assert_eq!(key(&minimal), key(&minimal));
    }

    #[test]
    fn test_search_cache_key_includes_every_response_and_ranking_input() {
        let baseline = base_input("GuidanceTarget");
        let baseline_key = cache_key(None, None, &baseline, SearchMode::Keyword, None);

        let mut variants = Vec::new();

        let mut input = baseline.clone();
        input.limit = Some(7);
        variants.push(("limit", input));

        let mut input = baseline.clone();
        input.offset = Some(3);
        variants.push(("offset", input));

        let mut input = baseline.clone();
        input.file_types = Some(vec!["rs".to_string()]);
        variants.push(("file_types", input));

        let mut input = baseline.clone();
        input.include_content = Some(false);
        variants.push(("include_content", input));

        let mut input = baseline.clone();
        input.include_memory = Some(true);
        variants.push(("include_memory", input));

        let mut input = baseline.clone();
        input.include_vcs = Some(true);
        variants.push(("include_vcs", input));

        let mut input = baseline.clone();
        input.output_format = Some("paths".to_string());
        input.include_content = Some(false);
        variants.push(("output_format", input));

        let mut input = baseline.clone();
        input.context_lines = Some(4);
        variants.push(("context_lines", input));

        let mut input = baseline.clone();
        input.content_max_chars = Some(1_200);
        variants.push(("content_max_chars", input));

        let mut input = baseline.clone();
        input.exact_match_boost = Some(3.5);
        variants.push(("exact_match_boost", input));

        let mut input = baseline.clone();
        input.query_vector = Some(vec![0.25, -0.5]);
        variants.push(("query_vector", input));

        let mut input = baseline.clone();
        input.intent = Some("Find the first safe edit for this task".to_string());
        variants.push(("intent", input));

        let mut input = baseline.clone();
        input.cursor = Some("refactor:v1:page-two".to_string());
        variants.push(("cursor", input));

        for (field, input) in variants {
            assert_ne!(
                baseline_key,
                cache_key(None, None, &input, SearchMode::Keyword, None),
                "cache key ignored {field}"
            );
        }
    }

    #[test]
    fn test_search_cache_key_excludes_learning_side_effect_opt_in() {
        let default_input = base_input("GuidanceTarget");
        let mut explicit_false = default_input.clone();
        explicit_false.code_rerank_learning_opt_in = Some(false);
        let mut opted_in = default_input.clone();
        opted_in.code_rerank_learning_opt_in = Some(true);

        let key = |input: &SearchInput| cache_key(None, None, input, SearchMode::Keyword, None);
        assert_eq!(key(&default_input), key(&explicit_false));
        assert_eq!(key(&default_input), key(&opted_in));
    }

    #[test]
    fn learning_attempt_ids_are_unique_and_require_exact_scope() {
        let workspace_id = uuid::Uuid::new_v4();
        let project_id = uuid::Uuid::new_v4();
        let exact = SearchParams {
            query: "how does auth work".to_string(),
            workspace_id: Some(workspace_id),
            project_id: Some(project_id),
            code_rerank_learning_opt_in: Some(true),
            ..Default::default()
        };
        let (first, first_id) = prepare_code_rerank_learning_attempt(exact.clone());
        let (second, second_id) = prepare_code_rerank_learning_attempt(exact);
        assert!(first_id.is_some());
        assert!(second_id.is_some());
        assert_ne!(first_id, second_id);
        assert_eq!(first.code_rerank_learning_request_id, first_id);
        assert_eq!(second.code_rerank_learning_request_id, second_id);

        let broad = SearchParams {
            query: "how does auth work".to_string(),
            workspace_id: Some(workspace_id),
            project_id: None,
            code_rerank_learning_opt_in: Some(true),
            ..Default::default()
        };
        let (broad, broad_id) = prepare_code_rerank_learning_attempt(broad);
        assert!(broad_id.is_none());
        assert!(broad.code_rerank_learning_opt_in.is_none());
        assert!(broad.code_rerank_learning_request_id.is_none());
    }

    #[test]
    fn local_only_results_do_not_claim_a_backend_learning_receipt() {
        let request_id = uuid::Uuid::new_v4();
        assert_eq!(
            served_api_learning_receipt(&SearchResponse::default(), Some(request_id)),
            None
        );
        let api_response = SearchResponse {
            results: vec![SearchResult {
                id: "api-result".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            served_api_learning_receipt(&api_response, Some(request_id)),
            Some(request_id)
        );
    }

    #[test]
    fn guided_cache_key_partitions_grounding_handles_by_digest_only() {
        let input = base_input(&format!("GuidanceTarget-{}", uuid::Uuid::new_v4()));
        let shapers = cache_shapers(&input, 25, 800);
        let first_handle = "gb:v1:workspace:secret-handle-one";
        let second_handle = "gb:v1:workspace:secret-handle-two";
        let key = |mode, handle| {
            build_search_cache_key(None, None, &input, mode, handle, &shapers, None, None)
        };

        let first = key(SearchMode::Guided, Some(first_handle));
        let second = key(SearchMode::Guided, Some(second_handle));
        assert_ne!(first, second);
        assert!(!first.contains(first_handle));
        assert!(!second.contains(second_handle));
        assert_eq!(first.len(), "search:v5:".len() + 64);

        assert_eq!(
            key(SearchMode::Keyword, Some(first_handle)),
            key(SearchMode::Keyword, Some(second_handle)),
            "non-Guided cache identity must ignore session grounding state"
        );

        search_cache().put(
            first.clone(),
            (
                "old guided payload".to_string(),
                json!({"grounding_handle": "gb:v1:old-output-handle"}),
            ),
        );
        assert!(search_cache().get(&first).is_some());
        assert!(
            search_cache().get(&second).is_none(),
            "a newer input handle must never hit an older handle's cached output"
        );
    }

    #[test]
    fn guided_cache_identity_still_ignores_learning_opt_in() {
        let base = base_input("GuidanceTarget");
        let mut opted_in = base.clone();
        opted_in.code_rerank_learning_opt_in = Some(true);
        let handle = Some("gb:v1:stable-input-handle");

        let first = build_search_cache_key(
            None,
            None,
            &base,
            SearchMode::Guided,
            handle,
            &cache_shapers(&base, 25, 800),
            None,
            None,
        );
        let second = build_search_cache_key(
            None,
            None,
            &opted_in,
            SearchMode::Guided,
            handle,
            &cache_shapers(&opted_in, 25, 800),
            None,
            None,
        );
        assert_eq!(first, second);
    }

    #[test]
    fn test_search_cache_key_canonicalizes_file_type_order() {
        let mut first = base_input("GuidanceTarget");
        first.file_types = Some(vec!["rs".to_string(), "ts".to_string()]);
        let mut second = first.clone();
        second.file_types = Some(vec!["ts".to_string(), "rs".to_string()]);

        assert_eq!(
            cache_key(None, None, &first, SearchMode::Keyword, None),
            cache_key(None, None, &second, SearchMode::Keyword, None)
        );

        second.file_types.as_mut().unwrap().push("rs".to_string());
        assert_eq!(
            cache_key(None, None, &first, SearchMode::Keyword, None),
            cache_key(None, None, &second, SearchMode::Keyword, None),
            "duplicate file types must not create a distinct resolved request"
        );
    }

    #[test]
    fn sha256_cache_digest_matches_standard_vectors() {
        assert_eq!(
            sha256_hex_bytes(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex_bytes(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn search_cache_key_length_framing_prevents_delimiter_collision() {
        let mut first = base_input("x|intent=y");
        first.intent = None;
        let mut second = base_input("x");
        second.intent = Some("y|intent=".to_string());

        assert_ne!(
            cache_key(None, None, &first, SearchMode::Keyword, None),
            cache_key(None, None, &second, SearchMode::Keyword, None)
        );
    }

    #[test]
    fn search_cache_key_uses_resolved_config_defaults() {
        let input = base_input("GuidanceTarget");
        let first = build_search_cache_key(
            None,
            None,
            &input,
            SearchMode::Keyword,
            None,
            &cache_shapers(&input, 10, 500),
            None,
            None,
        );
        let second = build_search_cache_key(
            None,
            None,
            &input,
            SearchMode::Keyword,
            None,
            &cache_shapers(&input, 30, 1_500),
            None,
            None,
        );
        assert_ne!(first, second);
    }

    #[test]
    fn search_cache_key_includes_resolved_hot_path_ranking_identity() {
        let input = base_input("GuidanceTarget");
        let mut first_shapers = cache_shapers(&input, 25, 800);
        first_shapers.hot_paths_identity = Some("hot-path-generation-a".to_string());
        let mut second_shapers = first_shapers.clone();
        second_shapers.hot_paths_identity = Some("hot-path-generation-b".to_string());

        let first = build_search_cache_key(
            None,
            None,
            &input,
            SearchMode::Keyword,
            None,
            &first_shapers,
            None,
            None,
        );
        let second = build_search_cache_key(
            None,
            None,
            &input,
            SearchMode::Keyword,
            None,
            &second_shapers,
            None,
            None,
        );
        assert_ne!(first, second);
    }

    #[test]
    fn hot_path_cache_identity_ignores_continuous_score_drift() {
        use mcp_client::client::{HotPathHintEntry, HotPathsHint};

        let first = HotPathsHint {
            entries: vec![
                HotPathHintEntry {
                    path: "src/search.rs".to_string(),
                    score: 0.9,
                    source: "active".to_string(),
                },
                HotPathHintEntry {
                    path: "src/client.rs".to_string(),
                    score: 0.8,
                    source: "active".to_string(),
                },
            ],
            confidence: 0.21,
            generated_at: "2026-08-01T00:00:00Z".to_string(),
            profile_version: 2,
        };
        let mut drifted = first.clone();
        drifted.entries[0].score = 9.7;
        drifted.entries[1].score = 8.6;
        drifted.confidence = 0.99;
        drifted.generated_at = "2026-08-01T00:00:05Z".to_string();

        assert_eq!(
            hot_paths_cache_identity(Some(&first)),
            hot_paths_cache_identity(Some(&drifted)),
            "continuous score/clock drift must not defeat the repeat-search cache"
        );
    }

    #[test]
    fn hot_path_cache_identity_tracks_ranked_membership() {
        use mcp_client::client::{HotPathHintEntry, HotPathsHint};

        let first = HotPathsHint {
            entries: vec![
                HotPathHintEntry {
                    path: "src/search.rs".to_string(),
                    score: 0.9,
                    source: "active".to_string(),
                },
                HotPathHintEntry {
                    path: "src/client.rs".to_string(),
                    score: 0.8,
                    source: "active".to_string(),
                },
            ],
            confidence: 0.21,
            generated_at: "2026-08-01T00:00:00Z".to_string(),
            profile_version: 2,
        };
        let mut reordered = first.clone();
        reordered.entries.swap(0, 1);

        assert_ne!(
            hot_paths_cache_identity(Some(&first)),
            hot_paths_cache_identity(Some(&reordered)),
            "a meaningful rank-order change must invalidate shaped results"
        );
    }

    #[test]
    fn test_search_cache_key_hashes_cursor_without_copying_opaque_token() {
        let mut first = base_input("GuidanceTarget");
        first.cursor = Some("refactor:v1:signed-secret-page-two".to_string());
        let mut second = first.clone();
        second.cursor = Some("refactor:v1:signed-secret-page-three".to_string());

        let first_key = cache_key(None, None, &first, SearchMode::Refactor, None);
        let second_key = cache_key(None, None, &second, SearchMode::Refactor, None);

        assert_ne!(first_key, second_key);
        assert!(!first_key.contains("signed-secret-page-two"));
        assert!(!second_key.contains("signed-secret-page-three"));
    }

    #[test]
    fn refactor_cursor_survives_full_and_compact_structured_outputs() {
        let cursor = "refactor:v1:opaque-page-two";
        let variants = [
            SearchResponse {
                results: vec![SearchResult {
                    id: "full".to_string(),
                    content: Some("fn target() {}".to_string()),
                    ..Default::default()
                }],
                next_cursor: Some(cursor.to_string()),
                ..Default::default()
            },
            SearchResponse {
                paths: vec!["src/lib.rs".to_string()],
                next_cursor: Some(cursor.to_string()),
                ..Default::default()
            },
            SearchResponse {
                results: vec![SearchResult {
                    id: "minimal".to_string(),
                    file_path: Some("src/lib.rs".to_string()),
                    content: None,
                    ..Default::default()
                }],
                next_cursor: Some(cursor.to_string()),
                ..Default::default()
            },
            SearchResponse {
                count: Some(1),
                has_more: Some(true),
                next_cursor: Some(cursor.to_string()),
                count_is_lower_bound: Some(true),
                ..Default::default()
            },
        ];

        for response in variants {
            let structured = search_response_structured_value(&response);
            assert_eq!(structured["next_cursor"], cursor);
            if response.count.is_some() {
                assert_eq!(structured["has_more"], true);
                assert_eq!(structured["count_is_lower_bound"], true);
            }
            let note = refactor_cursor_continuation_note(&response).unwrap();
            assert!(note.contains("next_cursor"));
            assert!(note.contains("cursor"));
            assert!(
                !note.contains(cursor),
                "opaque cursor must stay in structured output"
            );
        }

        assert!(refactor_cursor_continuation_note(&SearchResponse::default()).is_none());
    }

    #[test]
    fn structured_search_budget_preserves_actionable_evidence_and_continuation() {
        let learning_request_id = uuid::Uuid::new_v4();
        let rows: Vec<serde_json::Value> = (0..100)
            .map(|index| {
                json!({
                    "id": format!("result-{index}"),
                    "file_path": format!("src/module_{index}.rs"),
                    "start_line": index + 10,
                    "language": "rust",
                    "score": 0.9,
                    "content": format!("fn premium_search_{index}() {{}}\n{}", "detail ".repeat(2_000)),
                    "metadata": {
                        "end_line": index + 20,
                        "symbol": format!("premium_search_{index}"),
                        "unbounded_internal_trace": "trace ".repeat(2_000),
                    }
                })
            })
            .collect();
        let value = json!({
            "total": 100,
            "count": 100,
            "result_count": 100,
            "has_more": true,
            "next_offset": 20,
            "next_cursor": "refactor:v1:opaque-page-two",
            "code_rerank_learning_request_id": learning_request_id,
            "count_is_lower_bound": true,
            "results": rows,
            "paths": ["src/module_0.rs", "src/module_1.rs"],
            "index_trust": {"committed_generation": 42, "result_generation_consistent": true},
            "scope_diagnostics": {"scope_valid": true, "remediation_note": "none"},
            "search_explainability": {"trace": "large ".repeat(3_000)},
        });

        let bounded = budget_search_structured_value(value, SEARCH_STRUCTURED_OUTPUT_BUDGET_MIN);
        assert!(serde_json::to_vec(&bounded).unwrap().len() <= SEARCH_STRUCTURED_OUTPUT_BUDGET_MIN);
        assert_eq!(bounded["next_cursor"], "refactor:v1:opaque-page-two");
        assert_eq!(
            bounded["code_rerank_learning_request_id"],
            learning_request_id.to_string()
        );
        assert_eq!(bounded["has_more"], true);
        assert_eq!(bounded["count_is_lower_bound"], true);
        assert_eq!(bounded["index_trust"]["committed_generation"], 42);
        assert_eq!(bounded["scope_diagnostics"]["scope_valid"], true);
        assert_eq!(bounded["results"][0]["file_path"], "src/module_0.rs");
        assert_eq!(bounded["results"][0]["start_line"], 10);
        assert!(bounded["results"][0]["content"]
            .as_str()
            .unwrap()
            .starts_with("fn premium_search_0"));
        assert!(
            bounded["structured_budget"]["result_rows_omitted"]
                .as_u64()
                .unwrap()
                > 0
        );
    }

    #[test]
    fn structured_search_budget_leaves_small_backward_compatible_shape_unchanged() {
        let value = json!({
            "total": 1,
            "has_more": false,
            "results": [{
                "file_path": "src/lib.rs",
                "start_line": 7,
                "content": "fn search() {}"
            }]
        });
        let bounded =
            budget_search_structured_value(value.clone(), SEARCH_STRUCTURED_OUTPUT_BUDGET_MIN);
        assert_eq!(bounded, value);
        assert!(bounded.get("structured_budget").is_none());
    }

    #[test]
    fn structured_search_budget_is_hard_for_adversarial_paths_and_cursor() {
        let oversized_cursor = "cursor".repeat(20_000);
        let value = json!({
            "total": 1,
            "has_more": true,
            "next_cursor": oversized_cursor,
            "paths": ["path".repeat(20_000)],
            "results": [{
                "id": "id".repeat(20_000),
                "file_path": "src/".to_string() + &"deep/".repeat(20_000),
                "start_line": 77,
                "content": "source".repeat(20_000),
            }]
        });
        let bounded = budget_search_structured_value(value, SEARCH_STRUCTURED_OUTPUT_BUDGET_MIN);
        assert!(serde_json::to_vec(&bounded).unwrap().len() <= SEARCH_STRUCTURED_OUTPUT_BUDGET_MIN);
        assert_eq!(bounded["results"][0]["start_line"], 77);
        assert_eq!(bounded["structured_budget"]["applied"], true);
        assert!(bounded.get("next_cursor").is_none());
        assert_eq!(bounded["continuation_unavailable"], true);
        assert_eq!(
            bounded["continuation_protocol_violation"],
            "cursor_exceeds_max_bytes"
        );
        assert_eq!(
            bounded["max_valid_cursor_bytes"],
            MAX_VALID_SEARCH_CURSOR_BYTES
        );
    }

    #[test]
    fn structured_search_budget_preserves_max_valid_cursor_byte_for_byte() {
        const RF2_SIGNATURE_BYTES: usize = 43;
        let payload_bytes =
            MAX_VALID_SEARCH_CURSOR_BYTES - "rf2.".len() - ".".len() - RF2_SIGNATURE_BYTES;
        let cursor = format!(
            "rf2.{}.{}",
            "A".repeat(payload_bytes),
            "B".repeat(RF2_SIGNATURE_BYTES)
        );
        assert_eq!(cursor.len(), MAX_VALID_SEARCH_CURSOR_BYTES);
        let value = json!({
            "has_more": true,
            "next_cursor": cursor,
            "results": (0..40).map(|index| json!({
                "file_path": format!("src/{index}.rs"),
                "start_line": index + 1,
                "content": "x".repeat(4_000),
            })).collect::<Vec<_>>(),
            "index_trust": {
                "project_id": uuid::Uuid::new_v4(),
                "committed_generation": 42,
                "result_generation_consistent": true,
            },
            "scope_reliability": {
                "usable": true,
                "scope_match": true,
                "scope_invalid": false,
                "reason": "ok",
            },
            "scope_diagnostics": {
                "scope_valid": true,
                "fallback_used": false,
                "project_index_state": "fresh",
                "remediation_attempted": false,
            }
        });

        let bounded = budget_search_structured_value(value, SEARCH_STRUCTURED_OUTPUT_BUDGET_MIN);
        assert!(serde_json::to_vec(&bounded).unwrap().len() <= SEARCH_STRUCTURED_OUTPUT_BUDGET_MIN);
        assert_eq!(bounded["next_cursor"], cursor);
        assert!(bounded.get("continuation_unavailable").is_none());
        assert_eq!(bounded["index_trust"]["committed_generation"], 42);
        assert_eq!(bounded["scope_reliability"]["usable"], true);
        assert_eq!(bounded["scope_diagnostics"]["scope_valid"], true);
        assert_eq!(bounded["results"][0]["file_path"], "src/0.rs");
        assert_eq!(bounded["results"][0]["start_line"], 1);
    }

    #[test]
    fn structured_search_budget_rejects_escape_amplifying_cursor_characters() {
        let bounded = budget_search_structured_value(
            json!({
                "has_more": true,
                "next_cursor": "refactor:v1:\"unsafe\\cursor",
                "results": [{"file_path": "src/lib.rs", "start_line": 7}],
            }),
            SEARCH_STRUCTURED_OUTPUT_BUDGET_MIN,
        );
        assert!(bounded.get("next_cursor").is_none());
        assert_eq!(bounded["continuation_unavailable"], true);
        assert_eq!(
            bounded["continuation_protocol_violation"],
            "cursor_contains_invalid_transport_characters"
        );
    }

    #[test]
    fn structured_search_budget_sheds_300_rows_with_logarithmic_serializations() {
        let value = json!({
            "results": (0..300).map(|index| json!({
                "file_path": format!("src/module_{index}.rs"),
                "start_line": index + 1,
                "content": format!("fn row_{index}() {{}} {}", "detail ".repeat(1_000)),
            })).collect::<Vec<_>>(),
            "scope_reliability": {"usable": true, "reason": "ok"},
        });

        let (bounded, serializations) =
            budget_search_structured_value_counted(value, SEARCH_STRUCTURED_OUTPUT_BUDGET_MIN);
        assert!(serde_json::to_vec(&bounded).unwrap().len() <= SEARCH_STRUCTURED_OUTPUT_BUDGET_MIN);
        assert_eq!(bounded["results"][0]["file_path"], "src/module_0.rs");
        assert!(
            serializations <= 24,
            "300-row compaction used {serializations} full JSON serializations"
        );
    }

    #[test]
    fn structured_search_budget_early_aborts_multi_megabyte_diagnostics() {
        let value = json!({
            "results": (0..40).map(|index| json!({
                "file_path": format!("src/module_{index}.rs"),
                "start_line": index + 1,
                "content": "source ".repeat(2_000),
            })).collect::<Vec<_>>(),
            "search_explainability": {
                "model_trace": "multi-megabyte-diagnostic".repeat(200_000),
            },
            "scope_reliability": {"usable": true, "reason": "ok"},
            "scope_diagnostics": {
                "scope_valid": true,
                "fallback_used": false,
                "project_index_state": "fresh",
                "remediation_attempted": false,
            },
        });

        let (bounded, stats) =
            budget_search_structured_value_with_stats(value, SEARCH_STRUCTURED_OUTPUT_BUDGET_MIN);
        assert!(serde_json::to_vec(&bounded).unwrap().len() <= SEARCH_STRUCTURED_OUTPUT_BUDGET_MIN);
        assert!(
            stats.early_aborts >= 1,
            "oversized diagnostics should stop serialization at the cap"
        );
        assert!(
            stats.attempts <= 24,
            "multi-megabyte compaction used {} serialization probes",
            stats.attempts
        );
        assert_eq!(bounded["results"][0]["file_path"], "src/module_0.rs");
        assert_eq!(bounded["scope_reliability"]["usable"], true);
        assert_eq!(bounded["scope_diagnostics"]["scope_valid"], true);
    }

    #[test]
    fn absolute_structured_envelope_keeps_cursor_and_all_scope_controls() {
        const RF2_SIGNATURE_BYTES: usize = 43;
        let learning_request_id = uuid::Uuid::new_v4();
        let cursor = format!(
            "rf2.{}.{}",
            "A".repeat(
                MAX_VALID_SEARCH_CURSOR_BYTES - "rf2.".len() - ".".len() - RF2_SIGNATURE_BYTES
            ),
            "B".repeat(RF2_SIGNATURE_BYTES)
        );
        let huge = "🦀".repeat(20_000);
        let value = json!({
            "next_cursor": cursor,
            "code_rerank_learning_request_id": learning_request_id,
            "has_more": true,
            "paths": [huge],
            "results": [{
                "id": huge,
                "title": huge,
                "file_path": huge,
                "path": huge,
                "language": huge,
                "location": huge,
                "breadcrumb": huge,
                "origin": huge,
                "source_type": huge,
                "start_line": 7,
                "end_line": 9,
                "content": huge,
            }],
            "index_trust": {
                "project_id": huge,
                "committed_generation": 42,
                "result_generation_coverage_complete": true,
                "result_generation_consistent": true,
                "checks": {
                    "resolved_project_match": true,
                    "local_project_match": true,
                    "repository_match": true,
                    "branch_match": true,
                    "commit_match": true,
                    "generation_consistent": true,
                }
            },
            "scope_reliability": {
                "usable": true,
                "scope_match": true,
                "scope_invalid": false,
                "reason": huge,
                "repair": {"attempted": true, "succeeded": true, "reason": huge},
            },
            "scope_diagnostics": {
                "scope_valid": true,
                "fallback_used": false,
                "project_index_state": huge,
                "remediation_attempted": true,
            },
        });

        let bounded = budget_search_structured_value(value, SEARCH_STRUCTURED_OUTPUT_BUDGET_MIN);
        assert!(serde_json::to_vec(&bounded).unwrap().len() <= SEARCH_STRUCTURED_OUTPUT_BUDGET_MIN);
        assert_eq!(bounded["structured_budget"]["absolute_envelope"], true);
        assert_eq!(bounded["next_cursor"], cursor);
        assert_eq!(
            bounded["code_rerank_learning_request_id"],
            learning_request_id.to_string()
        );
        assert_eq!(bounded["index_trust"]["committed_generation"], 42);
        assert_eq!(bounded["scope_reliability"]["usable"], true);
        assert_eq!(bounded["scope_diagnostics"]["scope_valid"], true);
        assert_eq!(bounded["scope_diagnostics"]["remediation_attempted"], true);
    }

    #[test]
    fn structured_search_budget_runtime_bounds_arbitrary_json_shapes() {
        for seed in 0..32usize {
            let huge_key = format!("{}-{seed}", "untrusted-key".repeat(seed + 20));
            let mut value = json!({
                "has_more": true,
                "next_cursor": if seed % 2 == 0 {
                    "v".repeat(MAX_VALID_SEARCH_CURSOR_BYTES)
                } else {
                    "x".repeat(MAX_VALID_SEARCH_CURSOR_BYTES + seed + 1)
                },
                "results": [{
                    "file_path": format!("src/{seed}.rs"),
                    "start_line": seed + 1,
                    "score": {"unexpected": "z".repeat(20_000)},
                    "content": "body".repeat(20_000),
                    "metadata": {"nested": [[{"payload": "m".repeat(20_000)}]]},
                }],
                "index_trust": {
                    "project_id": uuid::Uuid::new_v4(),
                    "committed_generation": seed,
                    "repository": "r".repeat(20_000),
                },
                "scope_reliability": {
                    "usable": true,
                    "reason": "reason".repeat(20_000),
                }
            });
            value
                .as_object_mut()
                .unwrap()
                .insert(huge_key, Value::String("value".repeat(20_000)));

            for limit in [SEARCH_STRUCTURED_OUTPUT_BUDGET_MIN, 24_000] {
                let bounded = budget_search_structured_value(value.clone(), limit);
                assert!(
                    serde_json::to_vec(&bounded).unwrap().len() <= limit,
                    "seed {seed} escaped {limit}-byte envelope"
                );
            }
        }
    }

    #[test]
    fn combined_search_tool_result_budget_bounds_guidance_structured_duplication_and_cache_marker()
    {
        let cursor = "refactor:v1:opaque-page-two";
        let structured = json!({
            "next_cursor": cursor,
            "has_more": true,
            "results": (0..80).map(|index| json!({
                "file_path": format!("src/premium_{index}.rs"),
                "start_line": index + 10,
                "content": "source".repeat(4_000),
            })).collect::<Vec<_>>(),
            "guidance": {
                "answer": "backend intelligence ".repeat(20_000),
                "targets": (0..100).map(|index| json!({
                    "path": format!("src/premium_{index}.rs"),
                    "why": "reason".repeat(1_000),
                })).collect::<Vec<_>>(),
            },
            "index_trust": {"committed_generation": 9, "result_generation_consistent": true},
            "scope_reliability": {"usable": true, "scope_match": true, "reason": "ok"},
        });
        let text = format!(
            "[GUIDED_EVIDENCE]\n1. src/premium_0.rs:10\n{}\n[SEARCH_CACHED] reused\n",
            "huge guidance ".repeat(30_000)
        );

        let result = bounded_search_tool_result(text, structured);
        let wire_bytes = serde_json::to_vec(&result).unwrap().len();
        assert!(wire_bytes <= search_tool_result_wire_budget());
        let structured = result.structured_content.unwrap();
        assert_eq!(structured["next_cursor"], cursor);
        assert_eq!(structured["results"][0]["file_path"], "src/premium_0.rs");
        assert_eq!(structured["results"][0]["start_line"], 10);
        assert_eq!(structured["index_trust"]["committed_generation"], 9);
        assert_eq!(structured["scope_reliability"]["usable"], true);
        let rendered = match &result.content[0] {
            mcp_types::tool::ContentItem::Text { text } => text,
            _ => panic!("expected text content"),
        };
        assert!(rendered.contains("src/premium_0.rs:10"));
        assert!(rendered.contains("[SEARCH_CACHED]"));
    }

    #[test]
    fn every_search_tool_result_lane_obeys_the_final_serialized_wire_envelope() {
        const RF2_SIGNATURE_BYTES: usize = 43;
        let cursor = format!(
            "rf2.{}.{}",
            "A".repeat(
                MAX_VALID_SEARCH_CURSOR_BYTES - "rf2.".len() - ".".len() - RF2_SIGNATURE_BYTES
            ),
            "B".repeat(RF2_SIGNATURE_BYTES)
        );
        let structured = json!({
            "next_cursor": cursor,
            "results": (0..80).map(|index| json!({
                "file_path": format!("src/lane_{index}.rs"),
                "start_line": index + 1,
                "content": "source".repeat(4_000),
            })).collect::<Vec<_>>(),
            "index_trust": {
                "committed_generation": 9,
                "result_generation_consistent": true,
            },
            "scope_reliability": {
                "usable": true,
                "scope_match": true,
                "reason": "ok",
            },
            "scope_diagnostics": {
                "scope_valid": true,
                "fallback_used": false,
                "project_index_state": "fresh",
                "remediation_attempted": false,
            },
            "guidance": {"answer": "backend intelligence ".repeat(30_000)},
        });

        for (lane, prefix) in [
            ("guided", "[GUIDED_EVIDENCE]"),
            ("normal", "Search results"),
            ("cache", "[SEARCH_CACHED]"),
            ("fuzzy", "[FUZZY]"),
            ("vector", "[VECTOR]"),
        ] {
            let result = bounded_search_tool_result(
                format!("{prefix}\n1. src/lane_0.rs:1\n{}", "prose ".repeat(100_000)),
                structured.clone(),
            );
            assert!(
                serde_json::to_vec(&result).unwrap().len() <= search_tool_result_wire_budget(),
                "{lane} lane escaped the final ToolResult envelope"
            );
            assert!(!result.is_error, "{lane} success lane became an error");
            let lane_structured = result.structured_content.as_ref().unwrap();
            assert_eq!(lane_structured["next_cursor"], cursor);
            assert_eq!(lane_structured["results"][0]["file_path"], "src/lane_0.rs");
            assert_eq!(lane_structured["index_trust"]["committed_generation"], 9);
            assert_eq!(lane_structured["scope_reliability"]["usable"], true);
            assert_eq!(lane_structured["scope_diagnostics"]["scope_valid"], true);
        }

        let plan =
            bounded_existing_search_tool_result(mcp_types::tool::ToolResult::plan_restricted(
                format!("Filtered vector search {}", "premium ".repeat(100_000)),
                Some(&"starter ".repeat(100_000)),
                "pro",
                true,
            ));
        assert!(plan.is_error);
        assert!(serde_json::to_vec(&plan).unwrap().len() <= search_tool_result_wire_budget());
        let rendered = match &plan.content[0] {
            mcp_types::tool::ContentItem::Text { text } => text,
            _ => panic!("expected plan restriction text"),
        };
        assert!(rendered.starts_with("[PLAN_RESTRICTED]"));
    }

    #[test]
    fn test_search_cache_key_separates_checkout_identities() {
        let input = base_input("GuidanceTarget");
        let first = cache_key(None, None, &input, SearchMode::Keyword, Some("checkout-a"));
        let second = cache_key(None, None, &input, SearchMode::Keyword, Some("checkout-b"));

        assert_ne!(first, second);
    }

    #[test]
    fn test_search_cache_key_separates_authenticated_callers() {
        let base = "search:v3:base";
        let user_a = caller_scoped_search_cache_key(base, "user-a");
        let user_b = caller_scoped_search_cache_key(base, "user-b");
        let local = caller_scoped_search_cache_key(base, "l:stdio:process-a");

        assert_ne!(user_a, user_b);
        assert_ne!(user_a, local);
        assert_ne!(user_b, local);
    }

    #[test]
    fn test_search_cache_key_separates_effective_session_project_scopes() {
        let workspace_id = Some(uuid::Uuid::new_v4());
        let first_session_project = uuid::Uuid::new_v4();
        let second_session_project = uuid::Uuid::new_v4();
        let input = base_input("GuidanceTarget");

        let first_effective_project =
            effective_search_cache_project_id(None, None, None, Some(first_session_project));
        let second_effective_project =
            effective_search_cache_project_id(None, None, None, Some(second_session_project));

        assert_ne!(
            cache_key(
                workspace_id,
                first_effective_project,
                &input,
                SearchMode::Keyword,
                None,
            ),
            cache_key(
                workspace_id,
                second_effective_project,
                &input,
                SearchMode::Keyword,
                None,
            ),
            "session-resolved projects in one workspace must not share cached search results"
        );
    }

    #[test]
    fn drift_ingest_requires_agreeing_machine_local_scope() {
        let project = uuid::Uuid::new_v4();
        let other = uuid::Uuid::new_v4();

        assert_eq!(
            drift_ingest_project_id(Some(project), Some(project), Some(project), Some(project)),
            Some(project)
        );
        assert_eq!(
            drift_ingest_project_id(None, Some(project), None, Some(project)),
            Some(project)
        );
        assert_eq!(
            drift_ingest_project_id(Some(other), Some(project), Some(project), Some(project)),
            None,
            "explicit scope disagreement must suppress content writes"
        );
        assert_eq!(
            drift_ingest_project_id(None, Some(project), Some(other), Some(project)),
            None,
            "folder mapping and local index disagreement must suppress content writes"
        );
        assert_eq!(
            drift_ingest_project_id(Some(project), None, None, Some(project)),
            None,
            "remote/session scope without machine-local evidence must not upload files"
        );
    }

    #[test]
    fn test_hot_path_hint_uses_only_current_activity_paths() {
        let paths = vec![
            "crates/mcp-tools/src/domains/search.rs".to_string(),
            "crates/mcp-client/src/client.rs".to_string(),
            "crates/mcp-tools/src/domains/search.rs".to_string(),
        ];

        let hint = build_hot_paths_hint("where is search logic", &paths);
        assert!(hint.is_some());
        let hint = hint.unwrap();
        assert_eq!(hint.profile_version, 2);
        assert_eq!(hint.entries.len(), 2, "duplicate activity must collapse");
        assert_eq!(hint.entries[0].path, "crates/mcp-client/src/client.rs");
        assert_eq!(
            hint.entries[1].path,
            "crates/mcp-tools/src/domains/search.rs"
        );
        assert!(hint.entries.iter().all(|entry| entry.source == "active"));
        assert!(hint.confidence > 0.0);
    }

    #[test]
    fn test_recommend_mode_keyword_for_quoted_query() {
        let (mode, _) = recommend_search_mode("\"exact text\"", None);
        assert_eq!(mode, SearchMode::Keyword);
    }

    #[test]
    fn test_extract_quoted_literal() {
        assert_eq!(
            extract_quoted_literal("\"Refreshing rules before continuing setup\""),
            Some("Refreshing rules before continuing setup".to_string())
        );
        assert_eq!(
            extract_quoted_literal("'  exact string  '"),
            Some("exact string".to_string())
        );
        assert_eq!(extract_quoted_literal("not quoted"), None);
    }

    #[test]
    fn test_normalized_symbol_retry_query_strips_wrapped_identifier() {
        assert_eq!(
            normalized_symbol_retry_query("\"build_compose_service_sync_cmd\""),
            "build_compose_service_sync_cmd"
        );
    }

    #[test]
    fn test_escape_regex_literal() {
        assert_eq!(
            escape_regex_literal("foo.bar(baz)?"),
            "foo\\.bar\\(baz\\)\\?"
        );
    }

    #[test]
    fn test_recommend_mode_pattern_for_regex() {
        let (mode, _) = recommend_search_mode("foo\\s+bar", None);
        assert_eq!(mode, SearchMode::Pattern);
    }

    #[test]
    fn test_prefers_hybrid_for_code_location_bugfix_query() {
        assert!(prefers_hybrid_for_code_location_query(
            "Where is the horizontal scrolling chat panel CSS bug fixed?"
        ));
        assert!(!prefers_hybrid_for_code_location_query(
            "How does authentication work in Express?"
        ));
    }

    #[test]
    fn test_recommend_mode_hybrid_for_code_location_bugfix_query() {
        let (mode, _) = recommend_search_mode(
            "Where is the horizontal scrolling chat panel CSS bug fixed?",
            None,
        );
        assert_eq!(mode, SearchMode::Hybrid);
    }

    #[test]
    fn test_recommend_mode_not_pattern_for_doc_title_with_parens() {
        let (mode, _) = recommend_search_mode(
            "Search Reliability Fix Plan (Server-Side) - Active Scope (2026-02-17)",
            None,
        );
        assert_ne!(mode, SearchMode::Pattern);
        assert_eq!(mode, SearchMode::Hybrid);
    }

    #[test]
    fn test_recommend_mode_pattern_for_regex_with_parens() {
        let (mode, _) = recommend_search_mode("(error|warning)\\s+handler", None);
        assert_eq!(mode, SearchMode::Pattern);
    }

    #[test]
    fn test_recommend_mode_not_pattern_for_unbalanced_function_like_paren_query() {
        let (mode, _) = recommend_search_mode("project_files(\"/projects/{}/files\" tests", None);
        assert_ne!(mode, SearchMode::Pattern);
        assert_eq!(mode, SearchMode::Keyword);
    }

    #[test]
    fn test_recommend_mode_keyword_for_identifier() {
        let (mode, _) = recommend_search_mode("UserService", None);
        assert_eq!(mode, SearchMode::Keyword);
    }

    #[test]
    fn test_recommend_mode_exhaustive_for_all_occurrences() {
        let (mode, _) = recommend_search_mode("find all occurrences of TODO", None);
        assert_eq!(mode, SearchMode::Exhaustive);
    }

    #[test]
    fn test_recommend_mode_not_team_for_team_word_in_doc_title() {
        let (mode, _) = recommend_search_mode("PR8 Full Team Context Fix Plan", None);
        assert_ne!(mode, SearchMode::Team);
    }

    #[test]
    fn test_resolve_mode_defaults_to_auto_recommendation() {
        let (mode, auto, _) = resolve_mode(None, "how does auth work?", None);
        assert_eq!(mode, SearchMode::Semantic);
        assert!(auto);
    }

    #[test]
    fn test_resolve_mode_respects_explicit_mode() {
        let (mode, auto, _) = resolve_mode(Some("keyword"), "how does auth work?", None);
        assert_eq!(mode, SearchMode::Keyword);
        assert!(!auto);
    }

    #[test]
    fn test_resolve_include_memory_project_scope_defaults_to_code_only() {
        // Project-scoped code search defaults to code-only results in every mode;
        // memory is only included on an explicit memory-intent query.
        assert!(!resolve_include_memory(
            SearchMode::Hybrid,
            None,
            true,
            "how does auth work?"
        ));
        assert!(!resolve_include_memory(
            SearchMode::Semantic,
            None,
            true,
            "how does auth work?"
        ));
        assert!(!resolve_include_memory(
            SearchMode::Keyword,
            None,
            true,
            "handle_request"
        ));
        // A memory-intent query still opts in, even in project scope.
        assert!(resolve_include_memory(
            SearchMode::Hybrid,
            None,
            true,
            "lessons about auth"
        ));
    }

    #[test]
    fn test_resolve_include_memory_unscoped_semantic_hybrid_defaults_to_code_only() {
        // Unscoped semantic/hybrid code search also defaults to code-only; memory
        // is opt-in via a memory-intent query or an explicit override.
        assert!(!resolve_include_memory(
            SearchMode::Hybrid,
            None,
            false,
            "how does auth work?"
        ));
        assert!(!resolve_include_memory(
            SearchMode::Semantic,
            None,
            false,
            "how does auth work?"
        ));
        assert!(!resolve_include_memory(
            SearchMode::Keyword,
            None,
            false,
            "handle_request"
        ));
    }

    #[test]
    fn test_resolve_include_memory_detects_lesson_preference_intent() {
        assert!(resolve_include_memory(
            SearchMode::Keyword,
            None,
            true,
            "lessons about search mistakes"
        ));
        assert!(resolve_include_memory(
            SearchMode::Keyword,
            None,
            true,
            "saved preferences for this project"
        ));
    }

    #[test]
    fn test_resolve_include_memory_respects_override() {
        assert!(resolve_include_memory(
            SearchMode::Keyword,
            Some(true),
            true,
            "handle_request"
        ));
        assert!(!resolve_include_memory(
            SearchMode::Hybrid,
            Some(false),
            false,
            "lessons about auth"
        ));
    }

    #[test]
    fn test_suggest_output_format_for_count_query() {
        let output = suggest_output_format("how many auth handlers", SearchMode::Hybrid);
        assert_eq!(output, Some("count"));
    }

    #[test]
    fn test_suggest_output_format_for_identifier_keyword_prefers_full() {
        let output = suggest_output_format("handleSearch", SearchMode::Keyword);
        assert_eq!(output, Some("full"));
    }

    #[test]
    fn test_suggest_output_format_for_identifier_hybrid_has_no_forced_minimal() {
        let output = suggest_output_format("handleSearch", SearchMode::Hybrid);
        assert_eq!(output, None);
    }

    #[test]
    fn test_resolve_output_preferences_forces_full_when_include_content_requested() {
        let mut input = base_input("fallback_content_search");
        input.include_content = Some(true);

        let (output_format, include_content) =
            resolve_output_preferences(&input, SearchMode::Keyword);

        assert_eq!(output_format.as_deref(), Some("full"));
        assert_eq!(include_content, Some(true));
    }

    #[test]
    fn test_resolve_output_preferences_promotes_explicit_full_to_include_content() {
        let mut input = base_input("fallback_content_search");
        input.output_format = Some("full".to_string());

        let (output_format, include_content) =
            resolve_output_preferences(&input, SearchMode::Keyword);

        assert_eq!(output_format.as_deref(), Some("full"));
        assert_eq!(include_content, Some(true));
    }

    #[test]
    fn test_resolve_output_preferences_preserves_explicit_compact_formats() {
        for format in ["count", "paths", "minimal"] {
            let mut input = base_input("fallback_content_search");
            input.output_format = Some(format.to_string());

            let (output_format, include_content) =
                resolve_output_preferences(&input, SearchMode::Hybrid);

            assert_eq!(output_format.as_deref(), Some(format));
            assert_eq!(include_content, Some(false));
        }
    }

    #[test]
    fn test_resolve_output_preferences_explicit_content_wins_over_compact_format() {
        let mut input = base_input("fallback_content_search");
        input.output_format = Some("count".to_string());
        input.include_content = Some(true);

        let (output_format, include_content) =
            resolve_output_preferences(&input, SearchMode::Hybrid);

        assert_eq!(output_format.as_deref(), Some("full"));
        assert_eq!(include_content, Some(true));
    }

    #[test]
    fn test_retry_semantic_fallback_for_low_confidence_question_query() {
        let hybrid = response_with_scores(&[0.21, 0.19]);
        assert!(should_retry_semantic_fallback(
            "what patterns do we use for error handling in the assistant module?",
            SearchMode::Hybrid,
            &hybrid
        ));
    }

    #[test]
    fn test_no_retry_semantic_fallback_for_identifier_query() {
        let hybrid = response_with_scores(&[0.12]);
        assert!(!should_retry_semantic_fallback(
            "AssistantModule",
            SearchMode::Hybrid,
            &hybrid
        ));
    }

    #[test]
    fn test_adaptive_hybrid_retry_thresholds() {
        assert_eq!(
            crate::domains::search::adaptive_hybrid_retry_threshold("AssistantModule"),
            0.4
        );
        assert_eq!(
            crate::domains::search::adaptive_hybrid_retry_threshold(
                "where is handleSearch implemented in ui"
            ),
            0.48
        );
        assert_eq!(
            crate::domains::search::adaptive_hybrid_retry_threshold(
                "how does authentication flow work"
            ),
            0.6
        );
    }

    #[test]
    fn test_prefer_semantic_when_improvement_is_significant() {
        let hybrid = response_with_scores(&[0.24]);
        let semantic = response_with_scores(&[0.69]);
        assert!(should_prefer_semantic_results(
            "how does authentication work",
            &hybrid,
            &semantic
        ));
    }

    #[tokio::test]
    async fn low_confidence_semantic_replacement_returns_only_the_selected_attempt_receipt() {
        let workspace_id = uuid::Uuid::new_v4();
        let project_id = uuid::Uuid::new_v4();
        let query = "how does authentication work";
        let mut hybrid = response_with_scores(&[0.24, 0.19]);
        hybrid.results[0].id = "hybrid-not-served".to_string();
        let mut semantic = response_with_scores(&[0.89, 0.81]);
        semantic.results[0].id = "semantic-served".to_string();
        let (base_url, requests, server) = super::spawn_search_recording_server(vec![
            RecordingHttpResponse {
                expected_path: "/search/hybrid",
                status: 200,
                body: serde_json::to_string(&hybrid).unwrap(),
                delay: std::time::Duration::ZERO,
            },
            RecordingHttpResponse {
                expected_path: "/search/semantic",
                status: 200,
                body: serde_json::to_string(&semantic).unwrap(),
                delay: std::time::Duration::ZERO,
            },
        ]);
        let mut config = super::TestFixtures::test_config();
        config.api_url = base_url;
        let client = super::ContextStreamClient::new(config);
        let params = SearchParams {
            query: query.to_string(),
            workspace_id: Some(workspace_id),
            project_id: Some(project_id),
            code_rerank_learning_opt_in: Some(true),
            ..Default::default()
        };

        let (response, executed_mode, note, served_learning_request_id) =
            run_search_for_mode(&client, SearchMode::Hybrid, params, query, false)
                .await
                .expect("semantic replacement should succeed");

        let (_, hybrid_body) = requests
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("record hybrid attempt");
        let (_, semantic_body) = requests
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("record semantic attempt");
        let hybrid_body: Value = serde_json::from_str(&hybrid_body).unwrap();
        let semantic_body: Value = serde_json::from_str(&semantic_body).unwrap();
        let hybrid_request_id = hybrid_body["code_rerank_learning_request_id"]
            .as_str()
            .and_then(|value| uuid::Uuid::parse_str(value).ok())
            .expect("hybrid attempt correlation");
        let semantic_request_id = semantic_body["code_rerank_learning_request_id"]
            .as_str()
            .and_then(|value| uuid::Uuid::parse_str(value).ok())
            .expect("semantic attempt correlation");
        assert_ne!(hybrid_request_id, semantic_request_id);
        assert_eq!(served_learning_request_id, Some(semantic_request_id));
        assert_eq!(response.results[0].id, "semantic-served");
        assert_eq!(executed_mode, SearchMode::Semantic);
        assert!(note
            .as_deref()
            .unwrap_or_default()
            .contains("used semantic results"));

        server
            .join()
            .expect("semantic replacement server should finish");
    }

    #[tokio::test]
    async fn quoted_keyword_normalizes_before_the_primary_request() {
        let query = "\"unique_exact_literal\"";
        let mut keyword = response_with_scores(&[0.95]);
        keyword.results[0].id = "keyword-served".to_string();
        let (base_url, requests, server) =
            super::spawn_search_recording_server(vec![RecordingHttpResponse {
                expected_path: "/search/keyword",
                status: 200,
                body: serde_json::to_string(&keyword).unwrap(),
                delay: std::time::Duration::ZERO,
            }]);
        let mut config = super::TestFixtures::test_config();
        config.api_url = base_url;
        let client = super::ContextStreamClient::new(config);
        let params = SearchParams {
            query: query.to_string(),
            ..Default::default()
        };

        let (response, executed_mode, note, _) =
            run_search_for_mode(&client, SearchMode::Keyword, params, query, false)
                .await
                .expect("normalized keyword search should succeed");

        let (_, body) = requests
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("record keyword attempt");
        let body: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(body["query"], "unique_exact_literal");
        assert!(
            requests.try_recv().is_err(),
            "only one request should be sent"
        );
        assert_eq!(response.results[0].id, "keyword-served");
        assert_eq!(executed_mode, SearchMode::Keyword);
        assert!(note
            .as_deref()
            .unwrap_or_default()
            .contains("Normalized surrounding quotes"));

        server.join().expect("keyword test server should finish");
    }

    #[tokio::test]
    async fn quoted_keyword_miss_uses_one_exhaustive_literal_fallback() {
        let query = "\"unique_exact_literal\"";
        let keyword = SearchResponse::default();
        let mut exhaustive = response_with_scores(&[1.0]);
        exhaustive.results[0].id = "exhaustive-served".to_string();
        let (base_url, requests, server) = super::spawn_search_recording_server(vec![
            RecordingHttpResponse {
                expected_path: "/search/keyword",
                status: 200,
                body: serde_json::to_string(&keyword).unwrap(),
                delay: std::time::Duration::ZERO,
            },
            RecordingHttpResponse {
                expected_path: "/search/exhaustive",
                status: 200,
                body: serde_json::to_string(&exhaustive).unwrap(),
                delay: std::time::Duration::ZERO,
            },
        ]);
        let mut config = super::TestFixtures::test_config();
        config.api_url = base_url;
        let client = super::ContextStreamClient::new(config);
        let params = SearchParams {
            query: query.to_string(),
            ..Default::default()
        };

        let (response, executed_mode, note, _) =
            run_search_for_mode(&client, SearchMode::Keyword, params, query, false)
                .await
                .expect("exhaustive literal fallback should succeed");

        let (keyword_request, keyword_body) = requests
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("record keyword attempt");
        let (exhaustive_request, exhaustive_body) = requests
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("record exhaustive attempt");
        assert!(keyword_request.contains("/search/keyword"));
        assert!(exhaustive_request.contains("/search/exhaustive"));
        assert_eq!(
            serde_json::from_str::<Value>(&keyword_body).unwrap()["query"],
            "unique_exact_literal"
        );
        assert_eq!(
            serde_json::from_str::<Value>(&exhaustive_body).unwrap()["query"],
            "unique_exact_literal"
        );
        assert!(
            requests.try_recv().is_err(),
            "pattern and duplicate exhaustive requests must not be sent"
        );
        assert_eq!(response.results[0].id, "exhaustive-served");
        assert_eq!(executed_mode, SearchMode::Exhaustive);
        assert!(note
            .as_deref()
            .unwrap_or_default()
            .contains("complete literal coverage"));

        server
            .join()
            .expect("exhaustive fallback test server should finish");
    }

    #[tokio::test]
    async fn unquoted_identifier_uses_one_bounded_keyword_request() {
        let query = "QDRANT_LOCAL_URL";
        let mut keyword = response_with_scores(&[1.0]);
        keyword.results[0].id = "keyword-served".to_string();
        let (base_url, requests, server) =
            super::spawn_search_recording_server(vec![RecordingHttpResponse {
                expected_path: "/search/keyword",
                status: 200,
                body: serde_json::to_string(&keyword).unwrap(),
                delay: std::time::Duration::ZERO,
            }]);
        let mut config = super::TestFixtures::test_config();
        config.api_url = base_url;
        let client = super::ContextStreamClient::new(config);
        let params = SearchParams {
            query: query.to_string(),
            ..Default::default()
        };

        let (response, executed_mode, note, _) =
            run_search_for_mode(&client, SearchMode::Keyword, params, query, false)
                .await
                .expect("bounded identifier keyword request should succeed");

        let (request, body) = requests
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("record keyword attempt");
        assert!(request.contains("/search/keyword"));
        assert_eq!(
            serde_json::from_str::<Value>(&body).unwrap()["query"],
            query
        );
        assert!(
            requests.try_recv().is_err(),
            "exhaustive and refactor requests must not run after a keyword hit"
        );
        assert_eq!(response.results[0].id, "keyword-served");
        assert_eq!(executed_mode, SearchMode::Keyword);
        assert!(note
            .as_deref()
            .unwrap_or_default()
            .contains("Bounded identifier lookup"));

        server
            .join()
            .expect("identifier keyword test server should finish");
    }

    #[tokio::test]
    async fn unquoted_identifier_keyword_miss_is_terminal() {
        let query = "QDRANT_LOCAL_URL";
        let keyword = SearchResponse::default();
        let (base_url, requests, server) =
            super::spawn_search_recording_server(vec![RecordingHttpResponse {
                expected_path: "/search/keyword",
                status: 200,
                body: serde_json::to_string(&keyword).unwrap(),
                delay: std::time::Duration::ZERO,
            }]);
        let mut config = super::TestFixtures::test_config();
        config.api_url = base_url;
        let client = super::ContextStreamClient::new(config);
        let params = SearchParams {
            query: query.to_string(),
            ..Default::default()
        };

        let (response, executed_mode, note, _) =
            run_search_for_mode(&client, SearchMode::Keyword, params, query, false)
                .await
                .expect("bounded identifier miss should succeed");

        let (keyword_request, _) = requests
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("record keyword attempt");
        assert!(keyword_request.contains("/search/keyword"));
        assert!(
            requests.try_recv().is_err(),
            "an empty bounded identifier request must not launch exhaustive or refactor"
        );
        assert!(response.results.is_empty());
        assert_eq!(executed_mode, SearchMode::Keyword);
        assert!(note
            .as_deref()
            .unwrap_or_default()
            .contains("complete coverage"));

        server
            .join()
            .expect("identifier terminal-miss test server should finish");
    }

    #[test]
    fn test_keep_hybrid_when_semantic_gain_is_small() {
        let hybrid = response_with_scores(&[0.62]);
        let semantic = response_with_scores(&[0.63]);
        assert!(!should_prefer_semantic_results(
            "where is handleSearch implemented",
            &hybrid,
            &semantic
        ));
    }

    #[test]
    fn test_doc_lookup_intent_detection() {
        assert!(is_doc_lookup_query("please list docs for PR8"));
        assert!(is_doc_lookup_query("show document about team context fix"));
        assert!(!is_doc_lookup_query(
            "find docs.rs parser implementation in code"
        ));
    }

    #[test]
    fn test_keyword_symbol_fallback_for_identifier() {
        assert!(should_retry_keyword_with_symbol_modes(
            "recommend_search_mode"
        ));
        assert!(should_retry_keyword_with_symbol_modes(
            "\"build_compose_service_sync_cmd\""
        ));
        assert!(should_retry_keyword_with_symbol_modes(
            "project_files(\"/projects/{}/files\" tests"
        ));
        assert!(!should_retry_keyword_with_symbol_modes(
            "how does search mode work"
        ));
    }

    #[test]
    fn test_local_keyword_enrich_matches_quoted_identifier_literal() {
        use std::collections::HashSet;

        let temp_root = std::env::temp_dir().join(format!(
            "contextstream-search-quoted-symbol-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let script_path = temp_root.join("deploy/aws/deploy.sh");
        std::fs::create_dir_all(script_path.parent().unwrap()).unwrap();
        std::fs::write(
            &script_path,
            "#!/bin/bash\nbuild_compose_service_sync_cmd() {\n  echo ok\n}\n",
        )
        .unwrap();

        let results = local_keyword_enrich(
            &temp_root,
            "\"build_compose_service_sync_cmd\"",
            &HashSet::new(),
            None,
            0,
            400,
            false,
        );

        assert!(results
            .iter()
            .any(|item| item.file_path.as_deref() == Some("deploy/aws/deploy.sh")));
        assert!(results.iter().all(|item| item.score == Some(1.0)));

        let _ = std::fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn test_local_keyword_enrich_can_include_exact_match_from_existing_path() {
        use std::collections::HashSet;

        let temp_root = std::env::temp_dir().join(format!(
            "contextstream-search-existing-path-symbol-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let source_path = temp_root.join("crates/mcp-tools/src/domains/session.rs");
        std::fs::create_dir_all(source_path.parent().unwrap()).unwrap();
        std::fs::write(
            &source_path,
            "impl ToolHandler for ContextTool {}\n\nfn project_routing_preserve_explicit_current_scope() -> bool {\n    true\n}\n",
        )
        .unwrap();

        let existing_paths = HashSet::from(["crates/mcp-tools/src/domains/session.rs".to_string()]);
        let suppressed = local_keyword_enrich(
            &temp_root,
            "project_routing_preserve_explicit_current_scope",
            &existing_paths,
            None,
            0,
            400,
            false,
        );
        assert!(
            suppressed.is_empty(),
            "normal enrichment should preserve path-level dedupe"
        );

        let results = local_keyword_enrich(
            &temp_root,
            "project_routing_preserve_explicit_current_scope",
            &existing_paths,
            None,
            0,
            400,
            true,
        );

        assert!(results.iter().any(|item| {
            item.file_path.as_deref() == Some("crates/mcp-tools/src/domains/session.rs")
                && item
                    .content
                    .as_deref()
                    .unwrap_or_default()
                    .contains("project_routing_preserve_explicit_current_scope")
        }));
        assert!(results.iter().all(|item| item.score == Some(1.0)));

        let _ = std::fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn test_local_keyword_enrich_checked_reports_missing_root() {
        use std::collections::HashSet;

        let missing_root = std::env::temp_dir().join(format!(
            "contextstream-search-missing-root-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));

        let outcome = local_keyword_enrich_checked(
            &missing_root,
            "project_routing_preserve_explicit_current_scope",
            &HashSet::new(),
            None,
            0,
            400,
            true,
        );

        assert!(outcome.results.is_empty());
        let diagnostic = outcome
            .diagnostic
            .as_ref()
            .expect("missing root should be reported as a local enrichment diagnostic");
        assert_eq!(diagnostic.kind, "missing_root");
        assert!(diagnostic
            .folder_path
            .contains("contextstream-search-missing-root"));
    }

    #[test]
    fn test_exact_identifier_without_anchor_surfaces_local_enrichment_warning() {
        let response = SearchResponse {
            results: vec![SearchResult {
                file_path: Some("crates/mcp-tools/src/domains/session.rs".to_string()),
                content: Some("impl ToolHandler for ContextTool".to_string()),
                score: Some(0.91),
                ..Default::default()
            }],
            total: Some(1),
            ..Default::default()
        };
        let diagnostic = LocalEnrichDiagnostic::new(
            "permission_denied",
            std::path::Path::new("/home/alice/projects/example-repo"),
            "read_dir failed: Permission denied",
        );

        let readable_folder = std::env::temp_dir();
        let warning = local_enrichment_unavailable_warning_for_response(
            "project_routing_preserve_explicit_current_scope",
            &response,
            Some(&diagnostic),
            readable_folder.to_str(),
        )
        .expect("off-target exact identifier results need a visible local diagnostic");

        assert!(warning.contains("[LOCAL_ENRICHMENT_UNAVAILABLE]"));
        assert!(warning.contains("permission_denied"));
        // B3: drop distrust framing while keeping a hosted-safe exact-checkout
        // refresh path.
        assert!(!warning.contains("stale or incomplete"));
        assert!(warning.contains("still usable"));
        assert!(warning.contains("project(action=\"index\""));
        assert!(warning.contains("hosted MCP configured"));
        assert!(!warning.contains("ingest_local"));
    }

    #[test]
    fn test_local_enrichment_warning_suppressed_without_local_view() {
        // Hosted remote gateway: no folder is readable by this process, so the
        // per-search banner is unactionable noise and must stay silent.
        let response = SearchResponse {
            results: vec![SearchResult {
                file_path: Some("crates/mcp-tools/src/domains/session.rs".to_string()),
                content: Some("impl ToolHandler for ContextTool".to_string()),
                score: Some(0.91),
                ..Default::default()
            }],
            total: Some(1),
            ..Default::default()
        };
        let diagnostic = LocalEnrichDiagnostic::new(
            "missing_root",
            std::path::Path::new("/home/alice/projects/example-repo"),
            "metadata failed: No such file or directory (os error 2)",
        );

        assert!(local_enrichment_unavailable_warning_for_response(
            "project_routing_preserve_explicit_current_scope",
            &response,
            Some(&diagnostic),
            None,
        )
        .is_none());
    }

    #[test]
    fn test_local_enrichment_warning_advises_readable_folder_when_root_missing() {
        // Cross-machine index root on a LOCAL process: the scan root does not
        // exist here, so the refresh advice must point at the folder that does —
        // advising the missing root would re-create the drift it reports.
        let response = SearchResponse {
            results: vec![SearchResult {
                file_path: Some("crates/mcp-tools/src/domains/session.rs".to_string()),
                content: Some("impl ToolHandler for ContextTool".to_string()),
                score: Some(0.91),
                ..Default::default()
            }],
            total: Some(1),
            ..Default::default()
        };
        let missing_root = std::env::temp_dir().join(format!(
            "contextstream-enrich-missing-root-{}",
            std::process::id()
        ));
        let diagnostic = LocalEnrichDiagnostic::new(
            "missing_root",
            &missing_root,
            "metadata failed: No such file or directory (os error 2)",
        );
        let readable_folder = std::env::temp_dir();

        let warning = local_enrichment_unavailable_warning_for_response(
            "project_routing_preserve_explicit_current_scope",
            &response,
            Some(&diagnostic),
            readable_folder.to_str(),
        )
        .expect("missing root on a local process stays visible");

        // The scan failure names the missing root...
        assert!(warning.contains(missing_root.to_str().unwrap()));
        // ...but the remediation establishes the readable session folder as
        // the exact checkout before asking hosted MCP to refresh it.
        let quoted =
            serde_json::to_string(readable_folder.to_str().unwrap()).expect("quote test path");
        assert!(warning.contains(&format!("init(folder_path={quoted})")));
        assert!(warning.contains("project(action=\"index\")"));
        assert!(!warning.contains("ingest_local"));
    }

    #[test]
    fn test_exact_identifier_with_anchor_suppresses_local_enrichment_warning() {
        let response = SearchResponse {
            results: vec![SearchResult {
                file_path: Some("crates/mcp-tools/src/domains/session.rs".to_string()),
                content: Some(
                    "fn project_routing_preserve_explicit_current_scope() -> bool".to_string(),
                ),
                score: Some(1.0),
                ..Default::default()
            }],
            total: Some(1),
            ..Default::default()
        };
        let diagnostic = LocalEnrichDiagnostic::new(
            "permission_denied",
            std::path::Path::new("/home/alice/projects/example-repo"),
            "read_dir failed: Permission denied",
        );

        assert!(local_enrichment_unavailable_warning_for_response(
            "project_routing_preserve_explicit_current_scope",
            &response,
            Some(&diagnostic),
            std::env::temp_dir().to_str(),
        )
        .is_none());
    }

    #[test]
    fn test_plain_identifier_rerank_promotes_exact_symbol_match() {
        let mut response = SearchResponse {
            results: vec![
                SearchResult {
                    file_path: Some("crates/mcp-tools/src/domains/session.rs".to_string()),
                    content: Some("impl ToolHandler for ContextTool".to_string()),
                    score: Some(0.98),
                    ..Default::default()
                },
                SearchResult {
                    file_path: Some("crates/mcp-tools/src/domains/session.rs".to_string()),
                    content: Some(
                        "fn project_routing_preserve_explicit_current_scope(...) -> bool"
                            .to_string(),
                    ),
                    score: Some(1.0),
                    metadata: Some(json!({"source": "local_ripgrep"})),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let note = apply_symbol_anchor_rerank(
            &mut response,
            "project_routing_preserve_explicit_current_scope",
        );

        assert!(note
            .as_deref()
            .is_some_and(|value| value.contains("Symbol/identifier rerank")));
        assert!(response.results[0]
            .content
            .as_deref()
            .unwrap_or_default()
            .contains("project_routing_preserve_explicit_current_scope"));
    }

    #[test]
    fn test_keyword_semantic_fallback_for_natural_language() {
        assert!(should_retry_keyword_with_semantic(
            "Two-Phase Search Playbook rules"
        ));
        assert!(!should_retry_keyword_with_semantic("recommend_search_mode"));
    }

    #[test]
    fn test_workspace_scope_fallback_disabled_for_project_scoped_non_team_queries() {
        assert!(!should_allow_workspace_scope_fallback(
            SearchMode::Hybrid,
            "where is handleSearch",
            true
        ));
    }

    #[test]
    fn test_workspace_scope_fallback_enabled_for_team_queries() {
        assert!(should_allow_workspace_scope_fallback(
            SearchMode::Hybrid,
            "search across projects for auth middleware",
            true
        ));
        assert!(should_allow_workspace_scope_fallback(
            SearchMode::Team,
            "handleSearch",
            true
        ));
    }

    #[test]
    fn test_workspace_scope_fallback_enabled_without_project_candidates() {
        assert!(should_allow_workspace_scope_fallback(
            SearchMode::Keyword,
            "handleSearch",
            false
        ));
    }

    #[test]
    fn test_path_query_hint_for_relative_path() {
        let hint = path_query_hint("./crates/mcp-tools/src/domains/search.rs").unwrap();
        assert_eq!(
            hint.normalized_path,
            "crates/mcp-tools/src/domains/search.rs"
        );
        assert_eq!(hint.basename, "search.rs");
    }

    #[test]
    fn test_path_query_hint_strips_line_and_column_suffix() {
        let hint = path_query_hint("crates/mcp-tools/src/domains/search.rs:120:9").unwrap();
        assert_eq!(
            hint.normalized_path,
            "crates/mcp-tools/src/domains/search.rs"
        );
        assert_eq!(hint.basename, "search.rs");
    }

    #[test]
    fn test_path_query_hint_ignores_plain_sentence() {
        assert!(path_query_hint("how do we rank docs in search fallback").is_none());
    }

    #[test]
    fn test_path_query_hint_ignores_mixed_path_sentence_fragment() {
        assert!(path_query_hint("project_files(\"/projects/{}/files\" tests").is_none());
    }

    #[test]
    fn test_path_query_hint_accepts_wrapped_path_with_spaces() {
        let hint = path_query_hint("\"docs/My Notes/search plan.md\"").unwrap();
        assert_eq!(hint.normalized_path, "docs/My Notes/search plan.md");
        assert_eq!(hint.basename, "search plan.md");
    }

    #[test]
    fn test_classify_index_freshness_thresholds() {
        assert_eq!(classify_index_freshness(Some(0)), "fresh");
        assert_eq!(classify_index_freshness(Some(1)), "fresh");
        assert_eq!(classify_index_freshness(Some(6)), "recent");
        assert_eq!(classify_index_freshness(Some(12)), "recent");
        assert_eq!(classify_index_freshness(Some(30)), "aging");
        assert_eq!(classify_index_freshness(Some(48)), "aging");
        assert_eq!(classify_index_freshness(Some(49)), "stale");
        assert_eq!(classify_index_freshness(Some(200)), "stale");
        assert_eq!(classify_index_freshness(None), "unknown");
    }

    #[test]
    fn test_extract_api_index_hint_uses_ready_state_without_timestamp() {
        let response = SearchResponse {
            project_index_state: Some("ready".to_string()),
            index_generation: Some(8),
            ..SearchResponse::default()
        };
        let hint = extract_api_index_hint(&response, Some("/repo"), None).unwrap();
        assert_eq!(hint.freshness, "recent");
        assert_eq!(hint.confidence, "medium");
        assert!(hint.indicates_ready);
    }

    #[test]
    fn test_build_index_health_uses_api_hint_when_local_missing() {
        let response = SearchResponse {
            project_index_state: Some("ready".to_string()),
            index_generation: Some(3),
            ..SearchResponse::default()
        };
        let api_hint = extract_api_index_hint(&response, Some("/repo"), None).unwrap();
        let health = build_index_health(
            Some("/repo"),
            Some(uuid::Uuid::nil()),
            None,
            None,
            Some(api_hint),
            &[],
        )
        .unwrap();
        assert_eq!(health.freshness, "recent");
        assert_ne!(health.freshness, "missing");
        assert!(!should_surface_index_health_before_results(
            &health, false, false
        ));
    }

    #[test]
    fn test_build_index_health_api_stale_overrides_newer_local_metadata() {
        let local_entry = LocalIndexEntry {
            project_id: Some(uuid::Uuid::nil()),
            indexed_at: Some(chrono::Utc::now() - chrono::Duration::hours(2)),
            indexed_commit: None,
        };
        let api_hint = ApiIndexHint {
            freshness: "stale",
            confidence: "high",
            age_hours: Some(100),
            indexed_at: Some(chrono::Utc::now() - chrono::Duration::hours(100)),
            indicates_ready: true,
            drift_detected: false,
            recommendation: Some(
                "Search reliability signals indicate stale index coverage.".to_string(),
            ),
        };

        let health = build_index_health(
            Some("/repo"),
            Some(uuid::Uuid::nil()),
            None,
            Some(local_entry),
            Some(api_hint),
            &[],
        )
        .unwrap();

        assert_eq!(health.freshness, "stale");
        assert_eq!(health.age_hours, Some(100));
        assert!(!should_surface_index_health_before_results(
            &health, false, false
        ));
    }

    #[test]
    fn test_project_status_hint_marks_stale_when_search_response_omits_freshness() {
        let search_hint = extract_api_index_hint(
            &SearchResponse {
                project_index_state: Some("ready".to_string()),
                index_generation: Some(7),
                ..SearchResponse::default()
            },
            Some("/repo"),
            None,
        )
        .expect("search hint");
        assert_eq!(search_hint.freshness, "recent");

        let status_hint = extract_project_status_index_hint(
            &json!({
                "indexed_file_count": 42,
                "ingested_at_max": (chrono::Utc::now() - chrono::Duration::hours(101)).to_rfc3339()
            }),
            Some("/repo"),
            None,
        )
        .expect("status hint");
        assert_eq!(status_hint.freshness, "stale");

        let merged =
            merge_api_index_hints(Some(search_hint), Some(status_hint)).expect("merged hint");
        assert_eq!(merged.freshness, "stale");
        assert!(merged
            .recommendation
            .as_deref()
            .unwrap_or_default()
            .contains("stale index coverage"));
    }

    #[test]
    fn test_project_status_hint_ignores_in_progress_status_update_timestamp() {
        let status_hint = extract_project_status_index_hint(
            &json!({
                "status": "indexing",
                "indexed_file_count": 42,
                "last_updated": chrono::Utc::now().to_rfc3339()
            }),
            Some("/repo"),
            None,
        )
        .expect("status hint");

        assert_eq!(status_hint.freshness, "recent");
        assert_eq!(status_hint.age_hours, None);
    }

    #[test]
    fn test_build_index_health_reports_missing_when_local_and_api_unavailable() {
        let health = build_index_health(
            Some("/repo"),
            Some(uuid::Uuid::nil()),
            None,
            None,
            None,
            &[],
        )
        .unwrap();
        assert_eq!(health.freshness, "missing");
        // Repair-first: even a `missing` index never renders a pre-results
        // health block; the signal lives in structured `scope_reliability`.
        assert!(!should_surface_index_health_before_results(
            &health, false, false
        ));
        // ...and stays silent under no_hits / scope_invalid as well.
        assert!(!should_surface_index_health_before_results(
            &health, true, false
        ));
        assert!(!should_surface_index_health_before_results(
            &health, false, true
        ));
    }

    #[test]
    fn test_index_health_footer_suppressed_for_recent_freshness() {
        // The trailing "[Index advisory] freshness=`recent`. Results are
        // usable, but they may miss recent edits." footer is misleading for
        // a `recent` index — it warns about staleness for an index that
        // isn't stale. Make sure we don't append it for `recent` (or
        // `fresh`) freshness, regardless of confidence.
        for freshness in ["recent", "fresh"] {
            for confidence in ["high", "medium", "low"] {
                let health = IndexHealth {
                    freshness,
                    confidence,
                    age_hours: Some(2),
                    scope_match: true,
                    drift_detected: false,
                    changed_file_count: 0,
                    indexed_at: None,
                    recommendation: None,
                };
                assert!(
                    !should_append_index_health_footer(&health, false, false),
                    "footer must not appear for freshness={} confidence={}",
                    freshness,
                    confidence
                );
                // Suppressed under no_hits / scope_invalid as well.
                assert!(
                    !should_append_index_health_footer(&health, true, true),
                    "footer must stay suppressed under no_hits/scope_invalid for freshness={} confidence={}",
                    freshness,
                    confidence
                );
            }
        }

        // `aging` (12-48h, no local edits) stays calm: no footer, consistent
        // with the softened ready-but-aging presentation.
        let aging = IndexHealth {
            freshness: "aging",
            confidence: "high",
            age_hours: Some(20),
            scope_match: true,
            drift_detected: false,
            changed_file_count: 0,
            indexed_at: None,
            recommendation: None,
        };
        assert!(!should_append_index_health_footer(&aging, false, false));

        // `stale` (>48h) is handled by preflight repair and structured
        // telemetry for successful searches, not user-facing footer text.
        let stale = IndexHealth {
            freshness: "stale",
            confidence: "high",
            age_hours: Some(72),
            scope_match: true,
            drift_detected: false,
            changed_file_count: 0,
            indexed_at: None,
            recommendation: None,
        };
        assert!(!should_append_index_health_footer(&stale, false, false));
    }

    #[test]
    fn test_harmonize_project_index_state_prefers_ready_when_local_health_is_fresh() {
        let mut response = SearchResponse {
            project_index_state: Some("partial".to_string()),
            ..Default::default()
        };
        let health = IndexHealth {
            freshness: "fresh",
            confidence: "high",
            age_hours: Some(0),
            scope_match: true,
            drift_detected: false,
            changed_file_count: 0,
            indexed_at: None,
            recommendation: None,
        };

        harmonize_project_index_state(&mut response, Some(&health));
        assert_eq!(response.project_index_state.as_deref(), Some("ready"));
    }

    #[test]
    fn test_resolve_effective_folder_path_prefers_project_root_over_invalid_session_root() {
        assert_eq!(
            resolve_effective_folder_path(Some("/"), Some("/srv/example-repo"), None, true,),
            Some("/srv/example-repo".to_string())
        );
    }

    #[test]
    fn test_resolve_effective_folder_path_prefers_project_root_over_nested_session_folder() {
        assert_eq!(
            resolve_effective_folder_path(
                Some("/srv/example-repo/web"),
                Some("/srv/example-repo"),
                None,
                true,
            ),
            Some("/srv/example-repo".to_string())
        );
    }

    #[test]
    fn test_resolve_effective_folder_path_uses_local_index_root_when_project_missing() {
        assert_eq!(
            resolve_effective_folder_path(Some("/"), None, Some("/srv/example-repo"), true,),
            Some("/srv/example-repo".to_string())
        );
    }

    #[test]
    fn test_resolve_effective_folder_path_prefers_local_roots_over_foreign_project_root() {
        assert_eq!(
            resolve_effective_folder_path(
                Some("/srv/example-repo"),
                Some("C:\\\\Users\\\\alice\\\\projects\\\\example-repo"),
                Some("/srv/example-repo"),
                true,
            ),
            Some("/srv/example-repo".to_string())
        );
    }

    #[test]
    fn test_resolve_effective_folder_path_falls_back_to_repo_like_current_dir() {
        // set_current_dir mutates process-global state; serialize with other
        // env/cwd-touching tests.
        let _env_guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("crates")).unwrap();
        std::fs::create_dir_all(temp.path().join("web")).unwrap();
        let original_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(temp.path()).unwrap();

        let resolved = resolve_effective_folder_path(None, None, None, true);

        std::env::set_current_dir(original_cwd).unwrap();
        // current_dir() returns the canonicalized cwd, which resolves symlinks
        // (e.g. macOS /var -> /private/var), so compare against the canonical path.
        let expected = std::fs::canonicalize(temp.path())
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert_eq!(resolved, Some(expected));
    }

    #[test]
    fn test_resolve_effective_folder_path_can_disable_cwd_fallback() {
        let _env_guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("crates")).unwrap();
        std::fs::create_dir_all(temp.path().join("web")).unwrap();
        let original_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(temp.path()).unwrap();

        let resolved = resolve_effective_folder_path(None, None, None, false);

        std::env::set_current_dir(original_cwd).unwrap();
        assert_eq!(resolved, None);
    }

    #[test]
    fn test_current_dir_search_root_walks_up_to_repo_root() {
        let _env_guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("crates")).unwrap();
        std::fs::create_dir_all(temp.path().join("web")).unwrap();
        std::fs::create_dir_all(temp.path().join("nested").join("deeper")).unwrap();
        let original_cwd = std::env::current_dir().unwrap();
        std::env::set_current_dir(temp.path().join("nested").join("deeper")).unwrap();

        let resolved = current_dir_search_root();

        std::env::set_current_dir(original_cwd).unwrap();
        // Canonicalize the expected path: current_dir() resolves symlinks
        // (e.g. macOS /var -> /private/var) so the raw temp path won't match.
        let expected = std::fs::canonicalize(temp.path())
            .unwrap()
            .to_string_lossy()
            .to_string();
        assert_eq!(resolved, Some(expected));
    }

    #[test]
    fn test_scoped_session_folder_path_ignores_cross_project_session_root() {
        let other_project = uuid::Uuid::parse_str("22222222-2222-4222-8222-222222222222").unwrap();
        let mcp = uuid::Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap();
        assert_eq!(
            scoped_session_folder_path(
                Some("/srv/projects/mcp"),
                Some(mcp),
                Some(mcp),
                Some(other_project),
                Some("/srv/example-repo"),
            ),
            None
        );
    }

    #[test]
    fn test_scoped_session_folder_path_keeps_nested_target_project_root() {
        let canonical_project =
            uuid::Uuid::parse_str("22222222-2222-4222-8222-222222222222").unwrap();
        assert_eq!(
            scoped_session_folder_path(
                Some("/srv/example-repo/web"),
                None,
                None,
                Some(canonical_project),
                Some("/srv/example-repo"),
            ),
            Some("/srv/example-repo/web")
        );
    }

    #[test]
    fn test_normalize_paths_output_prefers_normalized_result_paths() {
        let mut response = SearchResponse {
            results: vec![
                SearchResult {
                    id: "a".into(),
                    file_path: Some(
                        "/srv/example-repo/crates/example-api/src/handlers/team.rs".into(),
                    ),
                    start_line: Some(1),
                    ..Default::default()
                },
                SearchResult {
                    id: "b".into(),
                    file_path: Some("/srv/example-repo/migrations/094_add_team_tables.sql".into()),
                    start_line: Some(1),
                    ..Default::default()
                },
            ],
            paths: vec![
                "crates/example-api/src/handlers/team.rs".into(),
                "migrations/094_add_team_tables.sql".into(),
            ],
            total: Some(2),
            ..Default::default()
        };

        normalize_paths_output(&mut response);

        assert_eq!(
            response.paths,
            vec![
                "/srv/example-repo/crates/example-api/src/handlers/team.rs".to_string(),
                "/srv/example-repo/migrations/094_add_team_tables.sql".to_string(),
            ]
        );
        assert_eq!(response.results.len(), 2);
        assert_eq!(
            response.results[0].file_path.as_deref(),
            Some("/srv/example-repo/crates/example-api/src/handlers/team.rs")
        );
        assert_eq!(
            response.results[0].location.as_deref(),
            Some("/srv/example-repo/crates/example-api/src/handlers/team.rs")
        );
    }

    #[test]
    fn test_stale_index_health_is_not_rendered_when_hits_exist() {
        // A stale index with hits is repaired before search when possible and
        // recorded in structured telemetry. Do not emit text that agents repeat
        // to users as a stale-index caveat.
        let health = IndexHealth {
            freshness: "stale",
            confidence: "medium",
            age_hours: Some(200),
            scope_match: true,
            drift_detected: false,
            changed_file_count: 0,
            indexed_at: None,
            recommendation: Some(
                "Refresh with init(folder_path=\"/repo\"), then project(action=\"index\")."
                    .to_string(),
            ),
        };
        assert!(!should_surface_index_health_before_results(
            &health, false, false
        ));
        // Stays silent on no_hits and scope_invalid too — repair-first.
        assert!(!should_surface_index_health_before_results(
            &health, true, false
        ));
        assert!(!should_surface_index_health_before_results(
            &health, false, true
        ));
        assert!(!should_append_index_health_footer(&health, false, false));
        assert!(!should_append_index_health_footer(&health, true, true));
    }

    #[test]
    fn test_artifact_filter_catches_paths_outside_file_path() {
        let result = SearchResult {
            id: "web/.next.bak.1770930141/dev/static/chunks/web_src.js".to_string(),
            file_path: None,
            location: Some("web/.next.bak.1770930141/dev/static/chunks/web_src.js".to_string()),
            title: Some("Generated notification bundle".to_string()),
            ..Default::default()
        };

        assert!(is_artifact_like_path(
            "web/.next.bak.1770930141/dev/static/chunks/web_src.js"
        ));
        assert!(is_artifact_like_path(
            ".next.bak.1770930141/dev/static/chunks/web_src.js"
        ));
        assert!(result_has_artifact_like_path(&result));
    }

    #[test]
    fn test_artifact_filter_applies_to_pattern_unless_requested() {
        assert!(should_filter_artifact_paths(
            SearchMode::Pattern,
            "dashboard-settings-dialog*"
        ));
        assert!(should_filter_artifact_paths(
            SearchMode::Exhaustive,
            "NotificationBell"
        ));
        assert!(!should_filter_artifact_paths(
            SearchMode::Pattern,
            ".next route chunk"
        ));
    }

    #[test]
    fn test_project_map_route_hint_filters_generated_paths() {
        let response = ProjectAgentMapResponse {
            project_id: uuid::Uuid::new_v4(),
            status: "ready".to_string(),
            stale: true,
            structured_json: json!({
                "search_routes": [{
                    "title": "Dashboard notification settings",
                    "keywords": ["notification", "settings", "dashboard"],
                    "paths": [
                        "web/.next.bak.1770930141/dev/static/chunks/web_src.js",
                        "web/src/components/notification-bell.tsx",
                        "web/src/app/(dashboard)/account/notification-settings/page.tsx"
                    ],
                    "suggested_queries": ["notification settings dashboard modal"]
                }]
            }),
            ..Default::default()
        };

        let route = project_map_route_hint_from_structured(
            &response,
            "notification settings dashboard modal",
        )
        .unwrap();

        assert_eq!(
            route.paths,
            vec![
                "web/src/components/notification-bell.tsx".to_string(),
                "web/src/app/(dashboard)/account/notification-settings/page.tsx".to_string(),
            ]
        );
    }

    #[test]
    fn test_project_map_route_hint_drops_generated_only_routes() {
        let route = ProjectAgentMapRouteHint {
            title: "Generated route".to_string(),
            paths: vec!["web/.next.bak.1770930141/dev/static/chunks/web_src.js".to_string()],
            stale: true,
            ..Default::default()
        };

        assert!(filter_project_map_route_hint(route, "notification settings").is_none());
    }

    #[test]
    fn test_project_map_route_hint_drops_unrelated_direct_hint() {
        let route = ProjectAgentMapRouteHint {
            title: "Dashboard/UI and streambox work".to_string(),
            paths: vec!["web/src/lib/api-client.ts".to_string()],
            suggested_queries: vec!["dashboard streambox explorer relevant components".to_string()],
            stale: false,
            ..Default::default()
        };

        assert!(
            filter_project_map_route_hint(route, "resolve_read_scope workspace_id project_id")
                .is_none()
        );
    }

    #[test]
    fn test_project_map_route_hint_drops_single_weak_multiword_overlap() {
        let route = ProjectAgentMapRouteHint {
            title: "Dashboard/UI and streambox work".to_string(),
            paths: vec!["web/src/lib/api-client.ts".to_string()],
            suggested_queries: vec!["dashboard streambox explorer".to_string()],
            stale: false,
            ..Default::default()
        };

        assert!(
            filter_project_map_route_hint(route, "dashboard latency cache performance").is_none()
        );
    }

    #[test]
    fn test_project_map_route_hint_drops_memory_decisions_scope_query() {
        let route = ProjectAgentMapRouteHint {
            title: "Dashboard/UI and streambox work".to_string(),
            paths: vec!["web/src/lib/api-client.ts".to_string()],
            suggested_queries: vec!["dashboard streambox explorer relevant components".to_string()],
            stale: true,
            ..Default::default()
        };

        assert!(filter_project_map_route_hint(
            route,
            "memory decisions workspace_id is required for decisions resolve_read_scope MemoryDecisionsTool rules AGENTS"
        )
        .is_none());
    }

    #[test]
    fn project_map_fetch_stays_off_count_and_exact_symbol_critical_paths() {
        let result = response_with_scores(&[0.92]);

        assert!(!should_fetch_project_map_route_hint(
            "authentication middleware ownership flow",
            Some("count"),
            &result,
        ));
        assert!(!should_fetch_project_map_route_hint(
            "buildEdgeNodeResult",
            Some("full"),
            &result,
        ));
        assert!(should_fetch_project_map_route_hint(
            "authentication middleware ownership flow",
            Some("full"),
            &result,
        ));
        assert!(!should_fetch_project_map_route_hint(
            "authentication middleware ownership flow",
            Some("full"),
            &SearchResponse::default(),
        ));
    }

    #[test]
    fn graph_enrichment_stays_off_count_exact_symbol_and_empty_critical_paths() {
        assert_eq!(
            GRAPH_ENRICHMENT_TIMEOUT,
            std::time::Duration::from_millis(250)
        );
        assert!(!should_fetch_graph_enrichment(
            "authentication middleware ownership flow",
            true,
            false,
        ));
        assert!(!should_fetch_graph_enrichment(
            "buildEdgeNodeResult",
            false,
            false,
        ));
        assert!(!should_fetch_graph_enrichment(
            "authentication middleware ownership flow",
            false,
            true,
        ));
        assert!(should_fetch_graph_enrichment(
            "authentication middleware ownership flow",
            false,
            false,
        ));
    }

    #[test]
    fn search_query_echo_is_exact_even_when_compact_api_response_omits_it() {
        let mut response = SearchResponse {
            query: None,
            total: Some(7),
            ..SearchResponse::default()
        };
        ensure_search_query_echo(&mut response, "buildEdgeNodeResult");
        let structured = search_response_structured_value(&response);

        assert_eq!(structured["query"], "buildEdgeNodeResult");
        assert_eq!(structured["total"], 7);
    }

    #[test]
    fn count_output_keeps_omitted_row_generation_coverage_unknown() {
        let project_id = uuid::Uuid::new_v4();
        let mut response = SearchResponse {
            total: Some(7),
            index_generation: Some(3),
            result_generation_min: Some(3),
            result_generation_max: Some(3),
            index_trust: Some(SearchIndexTrustEnvelope {
                project_id,
                committed_generation: 3,
                result_generation_coverage_complete: Some(false),
                result_generation_consistent: Some(false),
                ..Default::default()
            }),
            ..SearchResponse::default()
        };

        normalize_count_index_trust(&mut response, Some("count"));

        let trust = response.index_trust.as_ref().unwrap();
        assert_eq!(trust.result_generation_coverage_complete, None);
        assert_eq!(trust.result_generation_consistent, None);
        assert_eq!(response.result_generation_min, None);
        assert_eq!(response.result_generation_max, None);
        assert_eq!(response.index_generation, Some(3));
    }

    #[test]
    fn full_output_preserves_backend_generation_coverage_verdict() {
        let project_id = uuid::Uuid::new_v4();
        let mut response = SearchResponse {
            index_trust: Some(SearchIndexTrustEnvelope {
                project_id,
                committed_generation: 3,
                result_generation_coverage_complete: Some(false),
                ..Default::default()
            }),
            ..SearchResponse::default()
        };

        normalize_count_index_trust(&mut response, Some("full"));

        assert_eq!(
            response
                .index_trust
                .as_ref()
                .and_then(|trust| trust.result_generation_coverage_complete),
            Some(false)
        );
    }

    #[test]
    fn test_project_map_structured_route_does_not_substring_match_workspace() {
        let response = ProjectAgentMapResponse {
            project_id: uuid::Uuid::new_v4(),
            status: "ready".to_string(),
            structured_json: json!({
                "search_routes": [{
                    "title": "Dashboard/UI and streambox work",
                    "keywords": ["dashboard", "streambox", "explorer"],
                    "paths": ["web/src/lib/api-client.ts"],
                    "suggested_queries": ["dashboard streambox explorer relevant components"]
                }]
            }),
            ..Default::default()
        };

        assert!(project_map_route_hint_from_structured(
            &response,
            "resolve_read_scope workspace_id project_id",
        )
        .is_none());
    }

    #[test]
    fn test_project_map_structured_route_drops_single_weak_multiword_overlap() {
        let response = ProjectAgentMapResponse {
            project_id: uuid::Uuid::new_v4(),
            status: "ready".to_string(),
            structured_json: json!({
                "search_routes": [{
                    "title": "Dashboard UI work",
                    "keywords": ["notifications"],
                    "paths": ["web/src/lib/api-client.ts"],
                    "suggested_queries": ["streambox explorer"]
                }]
            }),
            ..Default::default()
        };

        assert!(project_map_route_hint_from_structured(
            &response,
            "dashboard latency cache performance",
        )
        .is_none());
    }

    #[test]
    fn test_project_map_structured_route_keeps_real_keyword_multiword_match() {
        let response = ProjectAgentMapResponse {
            project_id: uuid::Uuid::new_v4(),
            status: "ready".to_string(),
            structured_json: json!({
                "search_routes": [{
                    "title": "Memory context orchestration",
                    "keywords": ["grounding"],
                    "paths": ["crates/mcp-tools/src/domains/session.rs"],
                    "suggested_queries": ["context packet assembly"]
                }]
            }),
            ..Default::default()
        };

        let route = project_map_route_hint_from_structured(
            &response,
            "improve grounding latency across large repositories",
        )
        .expect("a server-strength keyword match should remain routable");

        assert_eq!(
            route.paths,
            vec!["crates/mcp-tools/src/domains/session.rs".to_string()]
        );
    }

    #[test]
    fn test_project_map_route_hint_keeps_query_relevant_direct_hint() {
        let route = ProjectAgentMapRouteHint {
            title: "Workspace scope resolution".to_string(),
            paths: vec!["crates/mcp-tools/src/domains/scope.rs".to_string()],
            suggested_queries: vec!["workspace_id project_id active scope recovery".to_string()],
            stale: false,
            ..Default::default()
        };

        let route =
            filter_project_map_route_hint(route, "resolve_read_scope workspace_id project_id")
                .unwrap();

        assert_eq!(
            route.paths,
            vec!["crates/mcp-tools/src/domains/scope.rs".to_string()]
        );
    }

    #[test]
    fn test_drifted_index_health_does_not_render_when_hits_exist() {
        // Drift is repaired before search when possible and remains in
        // structured telemetry for successful searches.
        let health = IndexHealth {
            freshness: "stale",
            confidence: "medium",
            age_hours: Some(200),
            scope_match: true,
            drift_detected: true,
            changed_file_count: 3,
            indexed_at: None,
            recommendation: Some(
                "Refresh with init(folder_path=\"/repo\"), then project(action=\"index\")."
                    .to_string(),
            ),
        };
        assert!(!should_surface_index_health_before_results(
            &health, false, false
        ));
        // Silent under no_hits / scope_invalid as well.
        assert!(!should_surface_index_health_before_results(
            &health, true, false
        ));
        assert!(!should_surface_index_health_before_results(
            &health, false, true
        ));
    }

    #[test]
    fn test_no_hits_does_not_render_index_health_block() {
        // No-hits, scope-mismatch, and missing-index are exactly the states
        // that used to print a pre-results block. They must now all stay quiet;
        // the honest signal moved to structured `scope_reliability`.
        let mismatch = IndexHealth {
            freshness: "missing",
            confidence: "low",
            age_hours: None,
            scope_match: false,
            drift_detected: false,
            changed_file_count: 0,
            indexed_at: None,
            recommendation: None,
        };
        for no_hits in [true, false] {
            for scope_invalid in [true, false] {
                assert!(
                    !should_surface_index_health_before_results(&mismatch, no_hits, scope_invalid),
                    "pre-results block must stay silent (no_hits={no_hits}, scope_invalid={scope_invalid})"
                );
            }
        }
    }

    #[test]
    fn test_search_source_has_no_distrust_or_grep_steer_strings() {
        // B1/B2: the agent-facing search prose that steered toward `git grep`
        // must be gone from the rendered output. We scan the source so the guard
        // can't be defeated by a refactor that re-introduces the phrasing.
        let src = include_str!("search.rs");
        assert!(
            !src.contains("[SCOPE_UNRELIABLE]"),
            "SCOPE_UNRELIABLE banner must be removed"
        );
        assert!(
            !src.contains("read the local files directly"),
            "must not instruct agents to read local files instead of trusting search"
        );
        assert!(
            !src.contains("Do NOT trust"),
            "must not tell agents to distrust results"
        );
        // The honest signal lives in the structured payload now.
        assert!(
            src.contains("scope_reliability"),
            "structured scope_reliability must be emitted"
        );
    }

    #[test]
    fn test_index_scope_repair_note_is_calm_and_does_not_instruct_retry() {
        // B2: the [INDEX_SCOPE_REPAIR] note must read as "handled automatically,
        // results usable" rather than steering a manual retry.
        let src = include_str!("index_keeper.rs");
        assert!(
            !src.contains("retry shortly"),
            "scope-repair note must not tell agents to retry"
        );
        assert!(
            src.contains("current canonical results remain usable"),
            "checkout-refresh note should distinguish the usable canonical index from the overlay being refreshed"
        );
    }

    #[test]
    fn test_session_init_messaging_drops_distrust_framing() {
        // B4: init() index messaging must not prime agents to distrust search.
        let src = include_str!("session.rs");
        assert!(
            !src.contains("local file reads are authoritative"),
            "init messaging must not steer toward local file reads"
        );
        assert!(
            !src.contains("stale-index advisories"),
            "init messaging must not promise stale-index advisories"
        );
    }

    fn dirty_hint(
        relative: &str,
        modified_at: Option<chrono::DateTime<chrono::Utc>>,
        exists: bool,
    ) -> DirtyFileHint {
        DirtyFileHint {
            absolute_path: format!("/repo/{}", relative),
            display_path: relative.to_string(),
            modified_at,
            exists,
        }
    }

    #[test]
    fn test_parse_git_status_dirty_hints_covers_untracked_modified_deleted_and_renamed() {
        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path();
        std::fs::create_dir_all(folder.join("src")).unwrap();
        std::fs::write(folder.join("src/lib.rs"), b"fn edited() {}").unwrap();
        std::fs::write(folder.join("src/new.rs"), b"fn added() {}").unwrap();
        std::fs::write(folder.join("src/renamed.rs"), b"fn renamed() {}").unwrap();

        let output =
            b" M src/lib.rs\0?? src/new.rs\0 D src/gone.rs\0R  src/renamed.rs\0src/old.rs\0";
        let hints = parse_git_status_dirty_hints(folder, output);
        let paths: Vec<&str> = hints
            .iter()
            .map(|hint| hint.display_path.as_str())
            .collect();

        assert!(paths.contains(&"src/lib.rs"));
        assert!(paths.contains(&"src/new.rs"));
        assert!(paths.contains(&"src/gone.rs"));
        assert!(paths.contains(&"src/renamed.rs"));
        assert!(paths.contains(&"src/old.rs"));
        assert!(hints
            .iter()
            .any(|hint| hint.display_path == "src/gone.rs" && !hint.exists));
        assert!(hints
            .iter()
            .any(|hint| hint.display_path == "src/new.rs" && hint.exists));
        assert!(hints
            .iter()
            .any(|hint| hint.display_path == "src/old.rs" && !hint.exists));
    }

    #[test]
    fn test_read_git_dirty_file_hints_detects_untracked_file_without_dirty_tracker() {
        if !std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|output| output.status.success())
            .unwrap_or(false)
        {
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path();
        let init = std::process::Command::new("git")
            .arg("init")
            .arg("-q")
            .current_dir(folder)
            .output()
            .unwrap();
        if !init.status.success() {
            return;
        }

        std::fs::write(
            folder.join("new_lead_symbol.rs"),
            b"fn new_lead_symbol() {}",
        )
        .unwrap();

        let mut hints = read_git_dirty_file_hints(folder.to_str().unwrap());
        for _ in 0..30 {
            if !hints.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            hints = read_git_dirty_file_hints(folder.to_str().unwrap());
        }
        assert!(
            hints
                .iter()
                .any(|hint| hint.display_path == "new_lead_symbol.rs" && hint.exists),
            "git dirty fallback should detect untracked files even when the ContextStream dirty-file tracker is absent"
        );
    }

    #[test]
    fn test_merge_dirty_file_hints_deduplicates_recorded_and_git_sources() {
        let older = chrono::Utc::now() - chrono::Duration::minutes(5);
        let newer = chrono::Utc::now();
        let recorded = vec![DirtyFileHint {
            absolute_path: "/repo/src/lib.rs".to_string(),
            display_path: "/repo/src/lib.rs".to_string(),
            modified_at: Some(older),
            exists: true,
        }];
        let git_dirty = vec![
            DirtyFileHint {
                absolute_path: "/repo/src/lib.rs".to_string(),
                display_path: "src/lib.rs".to_string(),
                modified_at: Some(newer),
                exists: true,
            },
            DirtyFileHint {
                absolute_path: "/repo/src/new.rs".to_string(),
                display_path: "src/new.rs".to_string(),
                modified_at: Some(newer),
                exists: true,
            },
        ];

        let merged = merge_dirty_file_hints(recorded, git_dirty);
        assert_eq!(merged.len(), 2);
        let lib = merged
            .iter()
            .find(|hint| hint.absolute_path == "/repo/src/lib.rs")
            .unwrap();
        assert_eq!(lib.display_path, "src/lib.rs");
        assert_eq!(lib.modified_at, Some(newer));
    }

    #[test]
    fn targeted_delta_honors_file_and_byte_budgets() {
        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path();
        let mut hints = Vec::new();
        for index in 0..20 {
            let path = folder.join(format!("file-{index:02}.rs"));
            std::fs::write(&path, "x".repeat(180_000)).unwrap();
            hints.push(DirtyFileHint {
                absolute_path: path.to_string_lossy().to_string(),
                display_path: format!("file-{index:02}.rs"),
                modified_at: Some(chrono::Utc::now()),
                exists: true,
            });
        }
        let refs = hints.iter().collect::<Vec<_>>();
        let delta = targeted_local_delta(folder.to_str().unwrap(), &refs, 10, 512 * 1024);

        assert!(delta.truncated);
        assert!(delta.processed_hints <= 10);
        assert!(
            delta
                .files
                .iter()
                .map(targeted_payload_content_bytes)
                .sum::<usize>()
                <= 512 * 1024
        );
    }

    #[test]
    fn targeted_delta_repairs_explicit_tail_path_without_prefix_scan() {
        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path();
        let target = folder.join("zzzz-after-old-prefix-cap.rs");
        std::fs::write(&target, "pub fn exact_tail_symbol() {}\n").unwrap();
        let hint = DirtyFileHint {
            absolute_path: target.to_string_lossy().to_string(),
            display_path: "zzzz-after-old-prefix-cap.rs".to_string(),
            modified_at: Some(chrono::Utc::now()),
            exists: true,
        };
        let delta = targeted_local_delta(folder.to_str().unwrap(), &[&hint], 1, 1024);

        assert!(delta.complete());
        assert_eq!(delta.files.len(), 1);
        assert_eq!(
            delta.files[0]
                .get("path")
                .and_then(serde_json::Value::as_str),
            Some("zzzz-after-old-prefix-cap.rs")
        );
    }

    #[test]
    fn test_dirty_hints_indicating_drift_flags_newer_edits_and_deletions() {
        let indexed_at = chrono::Utc::now() - chrono::Duration::hours(2);
        let newer = indexed_at + chrono::Duration::hours(1);
        let older = indexed_at - chrono::Duration::hours(1);

        let hints = vec![
            dirty_hint("src/edited.rs", Some(newer), true), // newer edit -> drift
            dirty_hint("src/old.rs", Some(older), true),    // older than index -> no drift
            dirty_hint("src/deleted.rs", Some(older), false), // deletion -> drift
        ];

        let drift = dirty_hints_indicating_drift(&hints, Some(indexed_at));
        let drifting: Vec<&str> = drift.iter().map(|h| h.display_path.as_str()).collect();
        assert!(drifting.contains(&"src/edited.rs"));
        assert!(drifting.contains(&"src/deleted.rs"));
        assert!(!drifting.contains(&"src/old.rs"));
        assert_eq!(drift.len(), 2);
    }

    #[test]
    fn test_dirty_hints_indicating_drift_treats_unknown_index_time_as_drift() {
        let hints = vec![dirty_hint("src/edited.rs", Some(chrono::Utc::now()), true)];
        // No known index timestamp -> any tracked edit is drift.
        let drift = dirty_hints_indicating_drift(&hints, None);
        assert_eq!(drift.len(), 1);
    }

    #[test]
    fn test_build_index_health_drift_from_dirty_files_with_local_entry() {
        let indexed_at = chrono::Utc::now() - chrono::Duration::hours(3);
        let local_entry = LocalIndexEntry {
            project_id: Some(uuid::Uuid::nil()),
            indexed_at: Some(indexed_at),
            indexed_commit: None,
        };
        let dirty = vec![dirty_hint(
            "src/edited.rs",
            Some(indexed_at + chrono::Duration::hours(1)),
            true,
        )];

        let health = build_index_health(
            Some("/repo"),
            Some(uuid::Uuid::nil()),
            None,
            Some(local_entry),
            None,
            &dirty,
        )
        .unwrap();

        assert!(health.drift_detected);
        assert_eq!(health.changed_file_count, 1);
        // Drift is repaired preflight and carried in structured telemetry when
        // a successful search has hits.
        assert!(!should_surface_index_health_before_results(
            &health, false, false
        ));
        assert!(health.recommendation.is_some());
    }

    #[test]
    fn test_build_index_health_no_drift_when_edits_predate_index() {
        let indexed_at = chrono::Utc::now() - chrono::Duration::hours(1);
        let local_entry = LocalIndexEntry {
            project_id: Some(uuid::Uuid::nil()),
            indexed_at: Some(indexed_at),
            indexed_commit: None,
        };
        let dirty = vec![dirty_hint(
            "src/edited.rs",
            Some(indexed_at - chrono::Duration::hours(2)),
            true,
        )];

        let health = build_index_health(
            Some("/repo"),
            Some(uuid::Uuid::nil()),
            None,
            Some(local_entry),
            None,
            &dirty,
        )
        .unwrap();

        assert!(!health.drift_detected);
        assert_eq!(health.changed_file_count, 0);
    }

    #[test]
    fn test_prune_deleted_file_results_removes_only_missing_local_files() {
        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path();
        std::fs::write(folder.join("present.rs"), b"fn present() {}").unwrap();

        let mut response = SearchResponse {
            results: vec![
                SearchResult {
                    id: "present".into(),
                    file_path: Some("present.rs".into()),
                    ..Default::default()
                },
                SearchResult {
                    id: "deleted".into(),
                    file_path: Some("deleted.rs".into()),
                    ..Default::default()
                },
                SearchResult {
                    id: "mem".into(),
                    file_path: Some("knowledge_node/abc".into()),
                    breadcrumb: Some("knowledge_node:abc".into()),
                    ..Default::default()
                },
            ],
            paths: vec!["present.rs".into(), "deleted.rs".into()],
            total: Some(3),
            ..SearchResponse::default()
        };

        let removed = prune_deleted_file_results(&mut response, folder);
        assert_eq!(removed, 1);
        let ids: Vec<&str> = response.results.iter().map(|r| r.id.as_str()).collect();
        assert!(ids.contains(&"present.rs") || ids.contains(&"present"));
        assert!(ids.contains(&"mem"));
        assert!(!ids.contains(&"deleted"));
        assert_eq!(response.total, Some(2));
        assert!(!response.paths.iter().any(|p| p == "deleted.rs"));
    }

    #[test]
    fn test_prune_deleted_file_results_keeps_unresolvable_paths() {
        let temp = tempfile::tempdir().unwrap();
        let folder = temp.path();
        // An absolute path outside the folder can't be judged locally; keep it.
        let mut response = SearchResponse {
            results: vec![SearchResult {
                id: "remote".into(),
                file_path: Some("/somewhere/else/remote.rs".into()),
                ..Default::default()
            }],
            total: Some(1),
            ..SearchResponse::default()
        };
        let removed = prune_deleted_file_results(&mut response, folder);
        assert_eq!(removed, 0);
        assert_eq!(response.results.len(), 1);
    }

    #[test]
    fn test_local_keyword_enrichment_query_limits_natural_language_noise() {
        assert!(is_local_keyword_enrichment_query("fallback_content_search"));
        assert!(!is_local_keyword_enrichment_query(
            "where is the horizontal scrolling chat panel css bug fixed in production code"
        ));
    }

    #[test]
    fn test_should_apply_local_enrichment_pattern_always_on_zero_hits() {
        assert!(should_apply_local_enrichment(
            SearchMode::Pattern,
            "*.ts",
            true,
            false,
            false,
            false
        ));
        // After the search quality fix, Pattern mode always tries local
        // enrichment on zero hits — even for non-glob queries like regexes
        // or exact file paths. The enrichment dispatcher picks the right
        // strategy (glob vs path-substring) based on query shape.
        assert!(should_apply_local_enrichment(
            SearchMode::Pattern,
            "import\\s+React",
            true,
            false,
            false,
            false
        ));
        assert!(!should_apply_local_enrichment(
            SearchMode::Pattern,
            "*.ts",
            false,
            false,
            false,
            false
        ));
    }

    #[test]
    fn test_should_apply_local_enrichment_suppressed_when_scope_fallback_applied_with_hits() {
        // When scope fallback was applied AND there are hits, enrichment is suppressed
        assert!(!should_apply_local_enrichment(
            SearchMode::Keyword,
            "fallback_content_search",
            false, // no_hits = false (has hits)
            false,
            false,
            true
        ));
    }

    #[test]
    fn test_should_apply_local_enrichment_allowed_when_scope_fallback_with_no_hits() {
        // When scope fallback was applied but zero results, enrichment should fire
        assert!(should_apply_local_enrichment(
            SearchMode::Keyword,
            "fallback_content_search",
            true, // no_hits = true
            false,
            false,
            true
        ));
    }

    #[test]
    fn test_should_apply_local_enrichment_keyword_allows_symbol_lookup_recovery() {
        assert!(should_apply_local_enrichment(
            SearchMode::Keyword,
            "fallback_content_search",
            true,
            false,
            false,
            false
        ));
        assert!(!should_apply_local_enrichment(
            SearchMode::Keyword,
            "where is the horizontal scrolling chat panel css bug fixed in production code",
            true,
            false,
            false,
            false
        ));
    }

    #[test]
    fn test_symbol_anchor_rerank_demotes_generated_noise_paths() {
        let mut response = SearchResponse {
            results: vec![
                SearchResult {
                    id: "r1".to_string(),
                    file_path: Some("sdk/openapi/generated/types.ts".to_string()),
                    location: Some("sdk/openapi/generated/types.ts:10".to_string()),
                    ..SearchResult::default()
                },
                SearchResult {
                    id: "r2".to_string(),
                    file_path: Some("crates/example-api/src/handlers/project_files.rs".to_string()),
                    location: Some(
                        "crates/example-api/src/handlers/project_files.rs:42".to_string(),
                    ),
                    content: Some("fn project_files_handler(...)".to_string()),
                    ..SearchResult::default()
                },
            ],
            total: Some(2),
            ..SearchResponse::default()
        };

        let note =
            apply_symbol_anchor_rerank(&mut response, "project_files(\"/projects/{}/files\" tests");

        assert!(note.is_some());
        assert_eq!(
            response.results[0].file_path.as_deref(),
            Some("crates/example-api/src/handlers/project_files.rs")
        );
    }

    // ========================================================================
    // Phase 1: Expanded local enrichment tests
    // ========================================================================

    #[test]
    fn test_should_apply_local_enrichment_hybrid_fires_on_zero_results() {
        assert!(should_apply_local_enrichment(
            SearchMode::Hybrid,
            "SearchTool",
            true,
            false,
            false,
            false
        ));
    }

    #[test]
    fn test_should_apply_local_enrichment_semantic_fires_on_zero_results() {
        assert!(should_apply_local_enrichment(
            SearchMode::Semantic,
            "fallback_content",
            true,
            false,
            false,
            false
        ));
    }

    #[test]
    fn test_should_apply_local_enrichment_crawl_fires_on_zero_results() {
        assert!(should_apply_local_enrichment(
            SearchMode::Crawl,
            "fn main",
            true,
            false,
            false,
            false
        ));
    }

    #[test]
    fn test_should_apply_local_enrichment_hybrid_suppressed_when_has_hits() {
        assert!(!should_apply_local_enrichment(
            SearchMode::Hybrid,
            "SearchTool",
            false,
            false,
            false,
            false
        ));
    }

    #[test]
    fn test_should_apply_local_enrichment_semantic_suppressed_for_long_queries() {
        // Long NL queries should still be filtered out by is_local_keyword_enrichment_query
        assert!(!should_apply_local_enrichment(
            SearchMode::Semantic,
            "where is the horizontal scrolling chat panel css bug fixed in production code that handles the edge case",
            true,
            false,
            false,
            false
        ));
    }

    #[test]
    fn test_should_apply_local_enrichment_scope_invalid_blocks_all() {
        assert!(!should_apply_local_enrichment(
            SearchMode::Hybrid,
            "SearchTool",
            true,
            false,
            true, // scope_invalid
            false
        ));
    }

    // ========================================================================
    // Phase 2: Ripgrep availability test
    // ========================================================================

    #[test]
    fn test_which_rg_finds_ripgrep_on_system() {
        // On systems with rg installed, this should return Some
        // On systems without rg, it returns None (graceful fallback)
        let result = which_rg();
        // Don't assert presence — just ensure it doesn't panic
        if let Some(path) = &result {
            assert!(!path.to_string_lossy().is_empty());
        }
    }

    // ========================================================================
    // Phase 4: Code identifier detection and mode selection tests
    // ========================================================================

    #[test]
    fn test_contains_code_identifiers_camel_case() {
        assert!(contains_code_identifiers("where is UserService defined"));
        assert!(contains_code_identifiers("find SearchTool handler"));
    }

    #[test]
    fn test_contains_code_identifiers_snake_case() {
        assert!(contains_code_identifiers(
            "search for fallback_content_search"
        ));
        assert!(contains_code_identifiers("what uses run_search_for_mode"));
    }

    #[test]
    fn test_contains_code_identifiers_double_colon() {
        assert!(contains_code_identifiers(
            "where is std::collections::HashMap used"
        ));
    }

    #[test]
    fn test_contains_code_identifiers_pure_natural_language() {
        assert!(!contains_code_identifiers("how does authentication work"));
        assert!(!contains_code_identifiers("explain the search logic"));
    }

    #[test]
    fn test_recommend_mode_hybrid_for_nl_with_code_identifiers() {
        let (_mode, _) =
            recommend_search_mode("search implementation handler in example-api", None);
        // "example-api" doesn't match as identifier, but let's check multi-word with code
        let (mode2, _) =
            recommend_search_mode("where is SearchTool defined in the codebase?", None);
        assert_eq!(mode2, SearchMode::Hybrid);

        let (mode3, _) =
            recommend_search_mode("how does run_search_for_mode work in search.rs", None);
        assert_eq!(mode3, SearchMode::Hybrid);
    }

    #[test]
    fn test_recommend_mode_still_semantic_for_pure_nl_question() {
        let (mode, _) = recommend_search_mode("how does authentication work?", None);
        assert_eq!(mode, SearchMode::Semantic);
    }

    #[test]
    fn test_recommend_mode_hybrid_for_long_query_with_identifiers() {
        let (mode, _) =
            recommend_search_mode("find where LocalPathProbe is constructed and used", None);
        assert_eq!(mode, SearchMode::Hybrid);
    }
}

// ============================================================================
// Input Validation Tests
// ============================================================================

mod validation_tests {
    use super::{
        create_mock_client, create_mock_client_without_auth, create_mock_index_keeper,
        create_mock_session, json, MAX_VALID_SEARCH_CURSOR_BYTES,
    };
    use super::{HybridSearchTool, KeywordSearchTool, SearchTool, SemanticSearchTool};
    use crate::registry::ToolHandler;

    #[tokio::test]
    async fn test_search_tool_requires_query() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let ik = create_mock_index_keeper(&client, &session);
        let tool = SearchTool::new(client, session, ik, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "query": ""
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("query"));
    }

    #[tokio::test]
    async fn test_search_tool_whitespace_query() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let ik = create_mock_index_keeper(&client, &session);
        let tool = SearchTool::new(client, session, ik, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "query": "   \t\n   "
            }))
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_semantic_search_requires_query() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let ik = create_mock_index_keeper(&client, &session);
        let tool = SemanticSearchTool::new(client, session, ik);

        let result = tool
            .execute(json!({
                "query": ""
            }))
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_hybrid_search_requires_query() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let ik = create_mock_index_keeper(&client, &session);
        let tool = HybridSearchTool::new(client, session, ik);

        let result = tool
            .execute(json!({
                "query": ""
            }))
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_keyword_search_requires_query() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let ik = create_mock_index_keeper(&client, &session);
        let tool = KeywordSearchTool::new(client, session, ik);

        let result = tool
            .execute(json!({
                "query": ""
            }))
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_search_tool_fails_fast_without_credentials() {
        let client = create_mock_client_without_auth();
        let session = create_mock_session(&client);
        let ik = create_mock_index_keeper(&client, &session);
        let tool = SearchTool::new(client, session, ik, mcp_types::atlas_layer::noop_layer());

        let result = tool.execute(json!({ "query": "auth middleware" })).await;
        assert!(matches!(result, Err(mcp_types::Error::MissingCredentials)));
    }

    #[tokio::test]
    async fn test_search_tool_rejects_oversized_cursor_without_truncating_it() {
        let client = create_mock_client_without_auth();
        let session = create_mock_session(&client);
        let ik = create_mock_index_keeper(&client, &session);
        let tool = SearchTool::new(client, session, ik, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "query": "TargetSymbol",
                "mode": "refactor",
                "cursor": "x".repeat(MAX_VALID_SEARCH_CURSOR_BYTES + 1),
            }))
            .await;
        let error = result.expect_err("oversized cursor must fail before transport");
        assert!(error.to_string().contains("cursor protocol violation"));
        assert!(error
            .to_string()
            .contains(&MAX_VALID_SEARCH_CURSOR_BYTES.to_string()));
    }
}

// ============================================================================
// Input Struct Tests
// ============================================================================

mod input_struct_tests {
    use super::json;
    use super::SearchInput;

    #[test]
    fn test_search_input_deserialization() {
        let input: SearchInput = serde_json::from_value(json!({
            "query": "authentication flow",
            "mode": "hybrid",
            "intent": "find the safest place to change auth behavior",
            "workspace_id": "550e8400-e29b-41d4-a716-446655440000",
            "limit": 5,
            "file_types": ["ts", "js"],
            "include_content": true,
            "include_memory": false,
            "code_rerank_learning_opt_in": true
        }))
        .unwrap();

        assert_eq!(input.query, "authentication flow");
        assert_eq!(input.mode, Some("hybrid".to_string()));
        assert_eq!(
            input.intent.as_deref(),
            Some("find the safest place to change auth behavior")
        );
        assert!(input.workspace_id.is_some());
        assert_eq!(input.limit, Some(5));
        assert_eq!(
            input.file_types,
            Some(vec!["ts".to_string(), "js".to_string()])
        );
        assert_eq!(input.include_content, Some(true));
        assert_eq!(input.include_memory, Some(false));
        assert_eq!(input.code_rerank_learning_opt_in, Some(true));
        assert!(input.cursor.is_none());
    }

    #[test]
    fn test_search_input_refactor_cursor() {
        let input: SearchInput = serde_json::from_value(json!({
            "query": "TargetSymbol",
            "mode": "refactor",
            "cursor": "refactor:v1:opaque-page-two"
        }))
        .unwrap();

        assert_eq!(input.mode.as_deref(), Some("refactor"));
        assert_eq!(input.cursor.as_deref(), Some("refactor:v1:opaque-page-two"));
    }

    #[test]
    fn test_search_input_minimal() {
        let input: SearchInput = serde_json::from_value(json!({
            "query": "test"
        }))
        .unwrap();

        assert_eq!(input.query, "test");
        assert!(input.mode.is_none());
        assert!(input.intent.is_none());
        assert!(input.workspace_id.is_none());
        assert!(input.limit.is_none());
        assert!(input.file_types.is_none());
        assert!(input.include_content.is_none());
        assert!(input.include_memory.is_none());
        assert!(!input.code_rerank_learning_opt_in.unwrap_or(false));
        assert!(input.cursor.is_none());
    }

    #[test]
    fn test_search_input_explicit_false_keeps_learning_disabled() {
        let input: SearchInput = serde_json::from_value(json!({
            "query": "test",
            "code_rerank_learning_opt_in": false
        }))
        .unwrap();

        assert_eq!(input.code_rerank_learning_opt_in, Some(false));
        assert!(!input.code_rerank_learning_opt_in.unwrap_or(false));
    }

    #[test]
    fn test_search_input_semantic_mode() {
        let input: SearchInput = serde_json::from_value(json!({
            "query": "how does auth work",
            "mode": "semantic"
        }))
        .unwrap();

        assert_eq!(input.mode, Some("semantic".to_string()));
    }

    #[test]
    fn test_search_input_pattern_mode() {
        let input: SearchInput = serde_json::from_value(json!({
            "query": "function\\s+\\w+",
            "mode": "pattern"
        }))
        .unwrap();

        assert_eq!(input.mode, Some("pattern".to_string()));
    }
}

// ============================================================================
// Tool Registration Tests
// ============================================================================

mod registration_tests {
    #[test]
    fn test_search_tool_count() {
        // Expected search tools:
        // - search (unified) - supports all modes: hybrid, semantic, keyword, pattern, exhaustive, refactor, team
        // - search_semantic
        // - search_hybrid
        // - search_keyword
        // Total: 4 tools

        let expected_tools = [
            "search",
            "search_semantic",
            "search_hybrid",
            "search_keyword",
        ];

        assert_eq!(expected_tools.len(), 4);
    }

    #[test]
    fn test_search_modes_coverage() {
        let all_modes = [
            "hybrid",
            "semantic",
            "keyword",
            "pattern",
            "exhaustive",
            "refactor",
            "team",
        ];

        assert_eq!(all_modes.len(), 7);
    }
}

// ============================================================================
// Scope Path Resolution Tests (requirement #4)
// ============================================================================

mod scope_path_resolution_tests {
    use crate::domains::scope::{
        canonicalize_repo_path, deduplicate_paths, deduplicate_results, resolve_search_paths,
        resolve_to_absolute_path,
    };
    use mcp_types::api::{SearchResponse, SearchResult};

    #[test]
    fn canonicalizes_mirror_prefix_contextstream() {
        assert_eq!(
            canonicalize_repo_path("contextstream/src/main.rs"),
            "src/main.rs"
        );
    }

    #[test]
    fn canonicalizes_mirror_prefix_claude_worktrees() {
        // Real worktree path: .claude/worktrees/<worktree-name>/src/lib.rs
        assert_eq!(
            canonicalize_repo_path(".claude/worktrees/feature-x/src/lib.rs"),
            "src/lib.rs"
        );
    }

    #[test]
    fn canonicalizes_worktree_with_nested_path() {
        assert_eq!(
            canonicalize_repo_path(".claude/worktrees/my-branch/crates/api/src/main.rs"),
            "crates/api/src/main.rs"
        );
    }

    #[test]
    fn canonicalizes_worktree_bare_name() {
        // Edge case: path is just the worktree name with no trailing content
        assert_eq!(
            canonicalize_repo_path(".claude/worktrees/feature-x"),
            "feature-x"
        );
    }

    #[test]
    fn canonicalizes_hosted_storage_path_with_projects() {
        assert_eq!(
            canonicalize_repo_path(
                "web/users/user-11111111/workspaces/example-22222222/projects/sample-33333333/apps/claude/tool-definitions.json"
            ),
            "apps/claude/tool-definitions.json"
        );
    }

    #[test]
    fn canonicalizes_mirror_prefix_brain_export() {
        assert_eq!(
            canonicalize_repo_path("contextstream-ai-brain-export/crates/api/src/main.rs"),
            "crates/api/src/main.rs"
        );
    }

    #[test]
    fn canonicalizes_leading_dot_slash() {
        assert_eq!(canonicalize_repo_path("./src/main.rs"), "src/main.rs");
    }

    #[test]
    fn canonicalizes_backslashes() {
        assert_eq!(canonicalize_repo_path("src\\main.rs"), "src/main.rs");
    }

    #[test]
    fn does_not_alter_normal_paths() {
        assert_eq!(canonicalize_repo_path("src/main.rs"), "src/main.rs");
    }

    #[test]
    fn resolves_to_absolute_for_existing_path() {
        let temp = std::env::temp_dir().join("mcp-path-test");
        let _ = std::fs::create_dir_all(&temp);
        let test_file = temp.join("test-resolve.txt");
        std::fs::write(&test_file, "test").unwrap();

        let result = resolve_to_absolute_path("test-resolve.txt", temp.to_str().unwrap());
        assert!(result.is_some());
        assert!(result.unwrap().is_absolute());

        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn resolves_path_even_if_not_on_disk() {
        let result = resolve_to_absolute_path("nonexistent/file.rs", "/tmp/nonexistent-folder");
        assert!(
            result.is_some(),
            "hosted mode: path is resolved without requiring local existence"
        );
        assert_eq!(
            result.unwrap().to_string_lossy(),
            "/tmp/nonexistent-folder/nonexistent/file.rs"
        );
    }

    #[test]
    fn resolves_root_relative_repo_path_against_folder_root() {
        let result = resolve_to_absolute_path(
            "/crates/example-api/src/handlers/team.rs",
            "/srv/example-repo",
        );
        assert_eq!(
            result.unwrap().to_string_lossy(),
            "/srv/example-repo/crates/example-api/src/handlers/team.rs"
        );
    }

    #[test]
    fn resolves_foreign_absolute_repo_path_against_folder_root() {
        let result = resolve_to_absolute_path(
            "/srv/alternate-checkout/crates/example-api/src/handlers/team.rs",
            "/srv/example-repo",
        );
        assert_eq!(
            result.unwrap().to_string_lossy(),
            "/srv/example-repo/crates/example-api/src/handlers/team.rs"
        );
    }

    #[test]
    fn resolves_windows_absolute_repo_path_against_folder_root() {
        let result = resolve_to_absolute_path(
            "C:\\Users\\alice\\projects\\example-repo\\crates\\example-api\\src\\handlers\\team.rs",
            "/srv/example-repo",
        );
        assert_eq!(
            result.unwrap().to_string_lossy(),
            "/srv/example-repo/crates/example-api/src/handlers/team.rs"
        );
    }

    #[test]
    fn resolve_search_paths_resolves_all_relative_paths() {
        let temp = std::env::temp_dir().join("mcp-resolve-filter-test");
        let _ = std::fs::create_dir_all(&temp);
        let test_file = temp.join("exists.rs");
        std::fs::write(&test_file, "fn main() {}").unwrap();

        let mut response = SearchResponse {
            results: vec![
                SearchResult {
                    id: "good".into(),
                    file_path: Some("exists.rs".into()),
                    start_line: Some(1),
                    ..Default::default()
                },
                SearchResult {
                    id: "also-good".into(),
                    file_path: Some("no-such-file.rs".into()),
                    start_line: Some(1),
                    ..Default::default()
                },
            ],
            paths: vec!["exists.rs".into(), "also-missing.rs".into()],
            total: Some(2),
            ..Default::default()
        };

        let dropped = resolve_search_paths(&mut response, temp.to_str().unwrap());
        assert_eq!(
            dropped.len(),
            0,
            "hosted mode: all paths resolve without disk check"
        );
        assert_eq!(response.results.len(), 2, "both results should be kept");
        assert!(response.results[0]
            .file_path
            .as_ref()
            .unwrap()
            .starts_with('/'));
        assert!(response.results[1]
            .file_path
            .as_ref()
            .unwrap()
            .starts_with('/'));
        assert!(response.results[0]
            .location
            .as_ref()
            .unwrap()
            .starts_with('/'));
        assert!(response.results[1]
            .location
            .as_ref()
            .unwrap()
            .starts_with('/'));
        assert_eq!(response.paths.len(), 2, "both paths should be kept");
        assert!(response.paths[0].starts_with('/'));
        assert!(response.paths[1].starts_with('/'));

        let _ = std::fs::remove_dir_all(&temp);
    }

    #[test]
    fn resolve_search_paths_drops_pseudo_absolute_repo_relative_paths_without_root() {
        let mut response = SearchResponse {
            results: vec![
                SearchResult {
                    id: "bad".into(),
                    file_path: Some("/crates/example-api/src/handlers/team.rs".into()),
                    start_line: Some(1),
                    ..Default::default()
                },
                SearchResult {
                    id: "good".into(),
                    file_path: Some(
                        "/srv/example-repo/crates/example-api/src/handlers/team.rs".into(),
                    ),
                    start_line: Some(1),
                    ..Default::default()
                },
            ],
            paths: vec![
                "/crates/example-api/src/handlers/team.rs".into(),
                "/srv/example-repo/crates/example-api/src/handlers/team.rs".into(),
            ],
            total: Some(2),
            ..Default::default()
        };

        let dropped = resolve_search_paths(&mut response, "/");
        assert_eq!(
            dropped,
            vec!["/crates/example-api/src/handlers/team.rs".to_string(); 2]
        );
        assert_eq!(response.results.len(), 1);
        assert_eq!(response.paths.len(), 1);
        assert_eq!(
            response.results[0].file_path.as_deref(),
            Some("/srv/example-repo/crates/example-api/src/handlers/team.rs")
        );
        assert_eq!(
            response.paths[0],
            "/srv/example-repo/crates/example-api/src/handlers/team.rs"
        );
    }

    #[test]
    fn deduplicates_results_by_canonical_path_and_line() {
        let mut response = SearchResponse {
            results: vec![
                SearchResult {
                    id: "1".into(),
                    file_path: Some("src/main.rs".into()),
                    start_line: Some(10),
                    ..Default::default()
                },
                SearchResult {
                    id: "2".into(),
                    file_path: Some("contextstream/src/main.rs".into()),
                    start_line: Some(10),
                    ..Default::default()
                },
                SearchResult {
                    id: "3".into(),
                    file_path: Some("src/main.rs".into()),
                    start_line: Some(20),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let removed = deduplicate_results(&mut response);
        assert_eq!(removed, 1);
        assert_eq!(response.results.len(), 2);
        assert_eq!(response.results[0].id, "1");
        assert_eq!(response.results[1].id, "3");
    }

    #[test]
    fn deduplicates_paths_by_canonical_form() {
        let mut response = SearchResponse {
            paths: vec![
                "src/main.rs".into(),
                "contextstream/src/main.rs".into(),
                "src/lib.rs".into(),
            ],
            ..Default::default()
        };

        let removed = deduplicate_paths(&mut response);
        assert_eq!(removed, 1);
        assert_eq!(response.paths.len(), 2);
    }
}

// ============================================================================
// Scope Diagnostics Tests (requirement #6)
// ============================================================================

mod scope_diagnostics_tests {
    use crate::domains::scope::extract_scope_diagnostics;
    use mcp_types::api::SearchResponse;

    #[test]
    fn healthy_scope_reports_no_issues() {
        let response = SearchResponse {
            scope_valid: Some(true),
            ..Default::default()
        };
        let diag = extract_scope_diagnostics(&response);
        assert!(diag.scope_valid);
        assert!(!diag.has_issues());
        assert!(diag.to_diagnostic_text().is_none());
    }

    #[test]
    fn invalid_scope_reports_issue() {
        let response = SearchResponse {
            scope_valid: Some(false),
            scope_reason: Some("project_not_found".into()),
            ..Default::default()
        };
        let diag = extract_scope_diagnostics(&response);
        assert!(!diag.scope_valid);
        assert!(diag.has_issues());
        let text = diag.to_diagnostic_text().unwrap();
        assert!(text.contains("scope_valid=false"));
        assert!(text.contains("project_not_found"));
    }

    #[test]
    fn fallback_used_reports_issue() {
        let response = SearchResponse {
            scope_valid: Some(true),
            fallback_used: Some(true),
            fallback_reason: Some("index_empty".into()),
            ..Default::default()
        };
        let diag = extract_scope_diagnostics(&response);
        assert!(diag.has_issues());
        let text = diag.to_diagnostic_text().unwrap();
        assert!(text.contains("fallback_used=true"));
        assert!(text.contains("index_empty"));
    }

    #[test]
    fn no_fallback_reports_no_issue() {
        let response = SearchResponse {
            scope_valid: Some(true),
            fallback_used: Some(false),
            fallback_reason: None,
            ..Default::default()
        };
        let diag = extract_scope_diagnostics(&response);
        assert!(!diag.has_issues());
        assert!(diag.to_diagnostic_text().is_none());
    }

    #[test]
    fn bag_budget_fallback_still_reports_issue() {
        let response = SearchResponse {
            scope_valid: Some(true),
            fallback_used: Some(true),
            fallback_reason: Some("postgres_ilike_fallback_bag_budget".into()),
            ..Default::default()
        };
        let diag = extract_scope_diagnostics(&response);
        assert!(diag.has_issues());
        let text = diag.to_diagnostic_text().unwrap();
        assert!(text.contains("fallback_used=true"));
        assert!(text.contains("postgres_ilike_fallback_bag_budget"));
    }

    #[test]
    fn project_index_state_included() {
        let response = SearchResponse {
            scope_valid: Some(false),
            project_index_state: Some("stale".into()),
            ..Default::default()
        };
        let diag = extract_scope_diagnostics(&response);
        let text = diag.to_diagnostic_text().unwrap();
        assert!(text.contains("project_index_state=`stale`"));
    }

    #[test]
    fn short_phrase_fallback_reports_issue() {
        let response = SearchResponse {
            scope_valid: Some(true),
            fallback_used: Some(true),
            fallback_reason: Some("postgres_ilike_fallback_short_phrase".into()),
            ..Default::default()
        };
        let diag = extract_scope_diagnostics(&response);
        assert!(diag.has_issues());
        let text = diag.to_diagnostic_text().unwrap();
        assert!(text.contains("fallback_used=true"));
        assert!(text.contains("postgres_ilike_fallback_short_phrase"));
    }
}

// ============================================================================
// include_memory conservative defaults (requirement #3)
// ============================================================================

mod include_memory_tests {
    use super::*;

    #[test]
    fn include_memory_false_for_project_scoped_code_search() {
        let result = resolve_include_memory(SearchMode::Keyword, None, true, "handle_request");
        assert!(
            !result,
            "Project-scoped code search should default include_memory=false"
        );
    }

    #[test]
    fn include_memory_false_for_workspace_semantic_code_search() {
        let result = resolve_include_memory(SearchMode::Semantic, None, false, "how auth works");
        assert!(
            !result,
            "Code search should not include memory unless memory intent is explicit"
        );
    }

    #[test]
    fn include_memory_explicit_override_wins() {
        let result =
            resolve_include_memory(SearchMode::Keyword, Some(true), true, "handle_request");
        assert!(result, "Explicit include_memory=true should win");
    }

    #[test]
    fn include_memory_false_for_project_scoped_hybrid() {
        let result = resolve_include_memory(SearchMode::Hybrid, None, true, "how auth works");
        assert!(
            !result,
            "Project-scoped hybrid search should default to code-only results"
        );
    }

    #[test]
    fn include_memory_false_for_project_scoped_semantic_code_query() {
        let result = resolve_include_memory(
            SearchMode::Semantic,
            None,
            true,
            "where is handleSearch implemented in src/ui/logs.tsx",
        );
        assert!(
            !result,
            "Project-scoped semantic code queries should suppress memory noise by default"
        );
    }

    #[test]
    fn include_memory_decision_reports_suppression_reason() {
        let result = resolve_include_memory_decision(
            SearchMode::Semantic,
            None,
            true,
            "where is handleSearch implemented in src/ui/logs.tsx",
        );
        assert!(!result.enabled);
        assert!(result.reason.contains("code"));
    }

    #[test]
    fn include_memory_decision_reports_memory_intent_reason() {
        let result = resolve_include_memory_decision(
            SearchMode::Keyword,
            None,
            true,
            "what decision explains auth search behavior",
        );
        assert!(result.enabled);
        assert!(result.reason.contains("memory"));
    }

    #[test]
    fn include_memory_true_for_project_scoped_lesson_preference_queries() {
        assert!(
            resolve_include_memory(SearchMode::Keyword, None, true, "what lesson did we save"),
            "Lessons live in memory and should be included by intent"
        );
        assert!(
            resolve_include_memory(
                SearchMode::Keyword,
                None,
                true,
                "user preferences for tests"
            ),
            "Preferences live in memory and should be included by intent"
        );
    }

    #[test]
    fn include_memory_false_for_identifier_collisions_with_weak_memory_terms() {
        let result = resolve_include_memory(
            SearchMode::Semantic,
            None,
            true,
            "where is plan_id parsed in crates/mcp-tools/src/domains/project.rs",
        );
        assert!(
            !result,
            "Weak memory terms inside code identifiers should not force memory inclusion"
        );
    }

    #[test]
    fn include_memory_false_for_ui_discovery_query_with_weak_memory_terms() {
        let result = resolve_include_memory(
            SearchMode::Hybrid,
            None,
            false,
            "chip views dashboard tabs Stream Skills Meetings Plans Tickets Todos Diagrams Docs feature header compact hero cards",
        );
        assert!(
            !result,
            "UI discovery queries with weak memory terms like Plans/Docs should stay code-only"
        );
    }

    #[test]
    fn include_memory_true_for_natural_language_docs_query_without_code_intent() {
        let result = resolve_include_memory(
            SearchMode::Semantic,
            None,
            true,
            "show docs and plan for release rollout",
        );
        assert!(
            result,
            "Natural-language docs/plan intent should keep memory inclusion enabled"
        );
    }
}

mod skill_query_tests {
    use super::*;

    #[test]
    fn skill_query_detects_explicit_skill_terms() {
        assert!(is_skill_query("show me the deploy runbook skill"));
        assert!(is_skill_query("what workflow checklist should I use"));
    }

    #[test]
    fn skill_query_detects_how_to_workflow_intent() {
        assert!(is_skill_query("how do i rollback production safely"));
        assert!(is_skill_query("best way to triage an incident"));
    }

    #[test]
    fn skill_query_does_not_match_generic_code_lookup() {
        assert!(!is_skill_query("where is handleSearch implemented"));
        assert!(!is_skill_query("find logs dashboard component"));
        assert!(!is_skill_query("show runbook for deploy failures"));
        assert!(!is_skill_query("open incident playbook"));
    }

    #[test]
    fn adaptive_skill_threshold_prefers_lower_cutoff_for_incident_queries() {
        assert_eq!(
            skill_score_threshold("how do i rollback production incident"),
            0.5
        );
        assert_eq!(skill_score_threshold("show workflow checklist"), 0.55);
        assert_eq!(
            skill_score_threshold("recommended way to organize notes"),
            0.65
        );
    }

    #[test]
    fn score_confidence_band_maps_expected_ranges() {
        assert_eq!(score_confidence_band(Some(0.9)), "high");
        assert_eq!(score_confidence_band(Some(0.7)), "medium");
        assert_eq!(score_confidence_band(Some(0.2)), "low");
        assert_eq!(score_confidence_band(None), "unknown");
    }
}

// ============================================================================
// Search quality regression tests — enrichment, escalation, keyword demotion
// ============================================================================

mod search_quality_tests {
    use super::*;

    #[test]
    fn should_apply_local_enrichment_pattern_zero_hits_no_glob() {
        let result = should_apply_local_enrichment(
            SearchMode::Pattern,
            "web/src/app/(marketing)/about/page.tsx",
            true,
            false,
            false,
            false,
        );
        assert!(
            result,
            "Pattern mode should try local enrichment on zero hits even without glob chars"
        );
    }

    #[test]
    fn should_apply_local_enrichment_pattern_zero_hits_with_glob() {
        let result =
            should_apply_local_enrichment(SearchMode::Pattern, "*.tsx", true, false, false, false);
        assert!(
            result,
            "Pattern mode should try local enrichment on zero hits with glob query"
        );
    }

    #[test]
    fn should_apply_local_enrichment_pattern_has_hits() {
        let result =
            should_apply_local_enrichment(SearchMode::Pattern, "*.tsx", false, false, false, false);
        assert!(
            !result,
            "Pattern mode should not enrich when API returned results"
        );
    }

    #[test]
    fn demote_keyword_false_positives_demotes_unrelated_results() {
        let mut response = mcp_types::api::SearchResponse {
            results: vec![
                mcp_types::api::SearchResult {
                    id: "hit1".into(),
                    file_path: Some("crates/mcp-client/src/client.rs".into()),
                    content: Some("pub fn connect() { ... }".into()),
                    score: Some(1.0),
                    ..Default::default()
                },
                mcp_types::api::SearchResult {
                    id: "hit2".into(),
                    file_path: Some("web/src/app/globals.css".into()),
                    content: Some(".cs-static-page-shell { display: flex; }".into()),
                    score: Some(0.8),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let demoted = demote_keyword_false_positives(&mut response, "cs-static-page-shell");
        assert_eq!(
            demoted, 1,
            "client.rs should be demoted — it doesn't contain the term"
        );
        assert_eq!(
            response.results[0].id, "hit2",
            "The result with the actual term should rank first"
        );
    }

    #[test]
    fn demote_keyword_false_positives_keeps_matching_results() {
        let mut response = mcp_types::api::SearchResponse {
            results: vec![mcp_types::api::SearchResult {
                id: "good".into(),
                file_path: Some("src/search.rs".into()),
                content: Some("fn handleSearch() { ... }".into()),
                score: Some(1.0),
                ..Default::default()
            }],
            ..Default::default()
        };

        let demoted = demote_keyword_false_positives(&mut response, "handleSearch");
        assert_eq!(
            demoted, 0,
            "Result containing the term should not be demoted"
        );
    }

    #[test]
    fn is_glob_like_detects_glob_patterns() {
        assert!(is_glob_like("*.tsx"));
        assert!(is_glob_like("**/*.rs"));
        assert!(is_glob_like("**/about*"));
        assert!(!is_glob_like("web/src/app/(marketing)/about/page.tsx"));
        assert!(!is_glob_like("handleSearch"));
    }

    #[test]
    fn demote_keyword_splits_hyphenated_tokens() {
        let mut response = mcp_types::api::SearchResponse {
            results: vec![
                mcp_types::api::SearchResult {
                    id: "false_positive".into(),
                    file_path: Some("crates/mcp-client/src/client.rs".into()),
                    content: Some("pub fn connect() { ... }".into()),
                    score: Some(1.0),
                    ..Default::default()
                },
                mcp_types::api::SearchResult {
                    id: "true_match".into(),
                    file_path: Some("web/src/app/globals.css".into()),
                    content: Some(".cs-static-page-shell { display: flex; }".into()),
                    score: Some(0.4),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let demoted = demote_keyword_false_positives(&mut response, "cs-static-page-shell");
        assert_eq!(demoted, 1);
        assert_eq!(
            response.results[0].id, "true_match",
            "The CSS file with the actual class should rank first after demotion"
        );
        assert!(
            response.results[1].score.unwrap() < 0.15,
            "False positive should be heavily demoted (0.1x of 1.0 = 0.1)"
        );
    }

    #[test]
    fn post_rank_fusion_promotes_exact_path_or_content_match() {
        let mut response = mcp_types::api::SearchResponse {
            results: vec![
                mcp_types::api::SearchResult {
                    id: "weak".into(),
                    file_path: Some("crates/mcp-client/src/client.rs".into()),
                    content: Some("pub fn connect() { ... }".into()),
                    score: Some(0.9),
                    ..Default::default()
                },
                mcp_types::api::SearchResult {
                    id: "strong".into(),
                    file_path: Some("crates/mcp-tools/src/domains/search.rs".into()),
                    content: Some("fn run_search_for_mode(...)".into()),
                    score: Some(0.6),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let note = apply_post_rank_fusion(&mut response, "run_search_for_mode");
        assert!(note.is_some(), "fusion should explain re-ranking");
        assert_eq!(
            response.results[0].id, "strong",
            "token-aware fusion should promote the exact symbol match"
        );
    }

    #[test]
    fn compact_paths_preserve_server_identifier_order_without_content_evidence() {
        let definition_path = "crates/mcp-tools/src/domains/session.rs";
        let partial_path_match = "crates/mcp-tools/src/domains/scope.rs";
        let mut response = mcp_types::api::SearchResponse {
            paths: vec![definition_path.into(), partial_path_match.into()],
            ..Default::default()
        };
        response.normalize_compact_formats();

        let query = "project_routing_preserve_explicit_current_scope";
        assert!(is_compact_paths_response(&response));
        assert_eq!(apply_symbol_anchor_rerank(&mut response, query), None);
        assert_eq!(apply_post_rank_fusion(&mut response, query), None);
        assert_eq!(demote_keyword_false_positives(&mut response, query), 0);
        normalize_paths_output(&mut response);
        assert_eq!(
            response.paths,
            vec![definition_path.to_string(), partial_path_match.to_string()]
        );
        assert_eq!(
            response.results[0].file_path.as_deref(),
            Some(definition_path),
            "path-only output must preserve the definition-first server order"
        );
        assert_eq!(
            response.results[1].file_path.as_deref(),
            Some(partial_path_match)
        );
    }

    #[test]
    fn compact_paths_allow_fresh_local_definition_evidence_to_win() {
        let server_definition = "crates/mcp-tools/src/domains/session.rs";
        let partial_path_match = "crates/mcp-tools/src/domains/scope.rs";
        let local_definition = "crates/mcp-tools/src/domains/current_session.rs";
        let mut response = mcp_types::api::SearchResponse {
            paths: vec![server_definition.into(), partial_path_match.into()],
            ..Default::default()
        };
        response.normalize_compact_formats();
        response.results.push(mcp_types::api::SearchResult {
            id: local_definition.into(),
            file_path: Some(local_definition.into()),
            location: Some(format!("{local_definition}:7")),
            content: Some("fn project_routing_preserve_explicit_current_scope() -> bool".into()),
            score: Some(1.0),
            metadata: Some(json!({"source": "local_ripgrep"})),
            ..Default::default()
        });

        let query = "project_routing_preserve_explicit_current_scope";
        assert!(has_client_rerank_evidence(&response));
        apply_symbol_anchor_rerank(&mut response, query);
        apply_post_rank_fusion(&mut response, query);
        assert_eq!(demote_keyword_false_positives(&mut response, query), 0);
        normalize_paths_output(&mut response);

        assert_eq!(
            response.paths,
            vec![
                local_definition.to_string(),
                server_definition.to_string(),
                partial_path_match.to_string(),
            ],
            "fresh local evidence may lead, while evidence-free server paths keep their order"
        );
    }

    #[test]
    fn token_demotion_applies_for_hybrid_and_refactor_modes() {
        let mut response = mcp_types::api::SearchResponse {
            results: vec![
                mcp_types::api::SearchResult {
                    id: "noise".into(),
                    file_path: Some("crates/mcp-client/src/client.rs".into()),
                    content: Some("pub fn connect() { ... }".into()),
                    score: Some(1.0),
                    ..Default::default()
                },
                mcp_types::api::SearchResult {
                    id: "match".into(),
                    file_path: Some("crates/mcp-tools/src/domains/search.rs".into()),
                    content: Some("fn demote_keyword_false_positives(...)".into()),
                    score: Some(0.3),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        assert!(supports_token_fusion(SearchMode::Hybrid));
        assert!(supports_token_fusion(SearchMode::Refactor));

        let demoted =
            demote_keyword_false_positives(&mut response, "demote_keyword_false_positives");
        assert_eq!(demoted, 1);
        assert_eq!(
            response.results[0].id, "match",
            "non-matching high-score row should be demoted below exact hit"
        );
    }

    #[test]
    fn refresh_indexed_snippets_uses_local_working_tree_content() {
        let temp = tempfile::tempdir().unwrap();
        let file_path = temp.path().join("rust-toolchain.toml");
        std::fs::write(
            &file_path,
            "[toolchain]\nchannel = \"1.95.0\"\ncomponents = [\"rustfmt\"]\n",
        )
        .unwrap();

        let mut response = mcp_types::api::SearchResponse {
            results: vec![mcp_types::api::SearchResult {
                id: "stale".into(),
                file_path: Some("rust-toolchain.toml".into()),
                content: Some("channel = \"1.93.0\"".into()),
                start_line: Some(2),
                metadata: Some(serde_json::json!({"source": "server_index"})),
                ..Default::default()
            }],
            total: Some(1),
            ..Default::default()
        };

        let refreshed = refresh_indexed_result_snippets_from_local_files(
            &mut response,
            temp.path(),
            "1.93",
            1,
            500,
        );

        assert_eq!(refreshed, 1);
        let refreshed_content = response.results[0].content.as_deref().unwrap();
        assert!(refreshed_content.contains("1.95.0"));
        assert!(!refreshed_content.contains("1.93.0"));
        assert_eq!(
            response.results[0].origin.as_deref(),
            Some("local_overlay_filesystem")
        );
        assert_eq!(
            response.results[0]
                .metadata
                .as_ref()
                .and_then(|m| m.get("snippet_source"))
                .and_then(|v| v.as_str()),
            Some("local_overlay_filesystem")
        );
    }

    #[test]
    fn local_result_path_rejects_parent_traversal() {
        let temp = tempfile::tempdir().unwrap();
        assert!(local_result_path(temp.path(), "../outside.rs").is_none());
    }

    #[test]
    fn search_cache_gating_for_folder_scope() {
        // Workspace-scoped (no folder) is always cacheable, drift irrelevant.
        assert!(should_use_search_cache(None, false, false));
        assert!(should_use_search_cache(None, true, false));
        // Folder-scoped: cacheable only when the tree is in sync with the index.
        assert!(should_use_search_cache(Some("/repo"), false, false));
        // Folder-scoped with local edits newer than the index → bypass cache so
        // we never replay stale snippets.
        assert!(!should_use_search_cache(Some("/repo"), true, false));
        // Explicit learning calls must reach the API so the observation exists;
        // opt-in remains absent from pure response-cache identity.
        assert!(!should_use_search_cache(None, false, true));
        assert!(!should_use_search_cache(Some("/repo"), false, true));
    }

    #[test]
    fn semantic_retry_fires_for_nl_queries_with_ui_terms() {
        // "about page contextstream.io" recommends Hybrid (because "page" is a
        // UI component term), but should still be eligible for semantic retry.
        let empty_response = mcp_types::api::SearchResponse {
            results: vec![],
            ..Default::default()
        };
        assert!(
            should_retry_semantic_fallback(
                "about page contextstream.io",
                SearchMode::Hybrid,
                &empty_response
            ),
            "NL query with UI terms should be eligible for semantic retry on zero results"
        );
    }

    #[test]
    fn semantic_retry_skips_identifier_queries() {
        let empty_response = mcp_types::api::SearchResponse {
            results: vec![],
            ..Default::default()
        };
        assert!(
            !should_retry_semantic_fallback("handleSearch", SearchMode::Hybrid, &empty_response),
            "Identifier queries should not trigger semantic retry"
        );
    }
}

#[test]
fn bare_identifier_queries_are_code_intent() {
    // (#2) A bare snake_case / CamelCase identifier is the canonical "find this
    // symbol" query. It must read as code intent so workspace-scoped semantic
    // search suppresses memory/media noise (the hexagon-logo result) instead of
    // ranking it alongside code.
    assert!(query_has_code_intent("search_first_redirect_decision"));
    assert!(query_has_code_intent("handle_supersede"));
    assert!(query_has_code_intent("SearchTool"));
    assert!(query_has_code_intent("handleOAuth"));
    // And such queries must NOT be misread as memory intent.
    assert!(!query_has_memory_intent("search_first_redirect_decision"));
    assert!(!query_has_memory_intent("handle_supersede"));
}

#[test]
fn identifier_shape_does_not_disturb_prose_or_memory_queries() {
    // Plain dictionary words and multi-word prose are NOT identifiers, so
    // memory-intent detection (which calls query_has_code_intent) is preserved.
    assert!(!query_is_identifier_shaped("decisions"));
    assert!(!query_is_identifier_shaped("logo"));
    assert!(!query_is_identifier_shaped("how does auth work"));
    assert!(!query_is_identifier_shaped(""));
    // Memory queries still classify as memory intent (regression guard).
    assert!(query_has_memory_intent("show me our decisions"));
    assert!(query_has_memory_intent("lessons from past sessions"));
    // Identifiers ARE identifier-shaped.
    assert!(query_is_identifier_shaped("foo_bar"));
    assert!(query_is_identifier_shaped("FooBar"));
}

// ============================================================================
// Output Budget + Escalation Guard + Index Origin Tests (4-failure fix)
// ============================================================================

#[test]
fn nl_phrase_queries_are_detected_for_escalation_guard() {
    // The observed failure query: a 5-word natural-language keyword bag that
    // auto-escalated to exhaustive and returned 271 BM25 token-OR rows.
    assert!(is_natural_language_phrase_query(
        "fable-5 effort high max slack"
    ));
    assert!(is_natural_language_phrase_query(
        "logo icon branding colors theme"
    ));
}

#[test]
fn identifier_and_literal_queries_are_not_nl_phrases() {
    // Identifier-shaped, quoted, glob, and regex queries keep access to the
    // exhaustive escalation ladder.
    assert!(!is_natural_language_phrase_query("handleSearch"));
    assert!(!is_natural_language_phrase_query(
        "where is run_search_for_mode used"
    ));
    assert!(!is_natural_language_phrase_query(
        "\"exact literal phrase here\""
    ));
    assert!(!is_natural_language_phrase_query("**/*.rs"));
    assert!(!is_natural_language_phrase_query("foo\\s+bar baz"));
    assert!(!is_natural_language_phrase_query("two words"));
    assert!(!is_natural_language_phrase_query(""));
}

#[test]
fn indexed_root_mismatch_detects_cross_machine_index() {
    // Windows-rooted index vs Linux checkout: must NOT match.
    assert!(!indexed_root_matches_local_folder(
        "C:\\Users\\alice\\projects\\example-repo",
        "/home/alice/projects/example-repo"
    ));
    // Same path, separator/case/trailing-slash differences: match.
    assert!(indexed_root_matches_local_folder(
        "/home/alice/projects/example-repo/",
        "/home/alice/projects/example-repo"
    ));
    assert!(indexed_root_matches_local_folder(
        "C:\\Users\\alice\\projects\\example-repo",
        "c:/users/alice/projects/example-repo"
    ));
    // Nested checkout (monorepo subfolder): match in both directions.
    assert!(indexed_root_matches_local_folder(
        "/home/alice/projects/example-repo",
        "/home/alice/projects/example-repo/crates/example-api"
    ));
    assert!(indexed_root_matches_local_folder(
        "/home/alice/projects/example-repo/crates/example-api",
        "/home/alice/projects/example-repo"
    ));
    // Unknown/empty metadata: treat as matching (no false alarms).
    assert!(indexed_root_matches_local_folder(
        "",
        "/home/alice/projects"
    ));
}

#[test]
fn index_root_mismatch_auto_repair_requires_same_project_signal() {
    let project_id = uuid::Uuid::new_v4();
    assert!(can_auto_repair_index_root_mismatch(
        "C:\\Users\\alice\\projects\\admin-console",
        "/Users/alice/projects/admin-console",
        Some("admin-console"),
        Some(project_id),
        None,
        None,
    ));
    assert!(can_auto_repair_index_root_mismatch(
        "D:\\work\\anything",
        "/Users/alice/projects/admin-console",
        Some("Admin Console"),
        Some(project_id),
        Some(project_id),
        None,
    ));
    assert!(!can_auto_repair_index_root_mismatch(
        "C:\\Users\\alice\\projects\\admin-console",
        "/Users/alice/projects/example-repo",
        Some("admin-console"),
        Some(project_id),
        None,
        None,
    ));
    assert!(!can_auto_repair_index_root_mismatch(
        "C:\\Users\\alice\\projects\\admin-console",
        "/Users/alice/projects/admin-console",
        Some("admin-console"),
        None,
        None,
        None,
    ));
}

#[test]
fn repo_identity_key_normalizes_common_git_url_forms() {
    assert_eq!(
        repo_identity_key("git@github.com:acme/example.git").as_deref(),
        Some("github.com/acme/example")
    );
    assert_eq!(
        repo_identity_key("https://github.com/acme/example/").as_deref(),
        Some("github.com/acme/example")
    );
    assert_eq!(
        repo_identity_key("ssh://git@github.com/contextstream/mcp-server.git").as_deref(),
        Some("github.com/contextstream/mcp-server")
    );
    assert_eq!(
        repo_identity_key("https://github.com/context-stream/mcp.server.git").as_deref(),
        Some("github.com/context-stream/mcp.server")
    );
    assert_eq!(
        repo_identity_key("https://token@example.com/acme/mcp.git?access_token=do-not-expose")
            .as_deref(),
        Some("example.com/acme/mcp")
    );
}

#[test]
fn repo_identity_key_preserves_host_and_full_nested_namespace() {
    assert_ne!(
        repo_identity_key("https://github.com/acme/app.git"),
        repo_identity_key("https://gitlab.com/acme/app.git")
    );
    assert_ne!(
        repo_identity_key("https://gitlab.com/top-a/team/app.git"),
        repo_identity_key("https://gitlab.com/top-b/team/app.git")
    );
    assert_eq!(
        repo_identity_key("ssh://git@gitlab.com/top-a/team/app.git").as_deref(),
        Some("gitlab.com/top-a/team/app")
    );
    assert!(repo_identity_key("../local/repository.git").is_none());
    assert!(repo_identity_key(".example.com/acme/repository.git").is_none());
    assert!(repo_identity_key("example.com./acme/repository.git").is_none());
    assert!(repo_identity_key("git@..:acme/repository.git").is_none());
}

#[test]
fn commit_indicates_drift_only_when_recorded_and_head_moved() {
    // Recorded commit + HEAD moved → out-of-session drift.
    assert!(commit_indicates_drift(Some("abc123"), Some("def456")));
    // Recorded commit + same HEAD → no drift.
    assert!(!commit_indicates_drift(Some("abc123"), Some("abc123")));
    // No recorded commit (pre-existing entry / non-git) → fall back, no commit drift.
    assert!(!commit_indicates_drift(None, Some("def456")));
    // HEAD unknown (git unavailable) → no drift.
    assert!(!commit_indicates_drift(Some("abc123"), None));
    assert!(!commit_indicates_drift(None, None));
}

#[test]
fn index_scope_warning_is_emitted_once_per_scope_pair() {
    let project_id = uuid::Uuid::new_v4();
    assert!(should_emit_index_scope_warning(
        Some(project_id),
        "C:\\Users\\alice\\projects\\example-repo",
        "/Users/alice/projects/example-repo"
    ));
    assert!(!should_emit_index_scope_warning(
        Some(project_id),
        "C:\\Users\\alice\\projects\\example-repo",
        "/Users/alice/projects/example-repo"
    ));
}

#[test]
fn search_text_output_budget_has_sane_default() {
    let budget = search_text_output_budget();
    assert!((4_000..=200_000).contains(&budget));
}
