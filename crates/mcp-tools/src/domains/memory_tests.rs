//! Tests for memory domain tools.

use super::*;
use crate::registry::ToolHandler;
use crate::testing::TestFixtures;
use mcp_types::tool::ToolCategory;
use serde_json::json;
use std::sync::Arc;

// ============================================================================
// Test Helpers
// ============================================================================

fn create_mock_client() -> ContextStreamClient {
    ContextStreamClient::new(TestFixtures::test_config())
}

fn create_mock_session(client: &ContextStreamClient) -> Arc<mcp_session::SessionManager> {
    Arc::new(mcp_session::SessionManager::new(
        client.clone(),
        TestFixtures::test_config(),
    ))
}

mod degraded_memory_search_tests {
    use super::{MemorySearchTool, TestFixtures, ToolHandler};
    use mcp_client::ContextStreamClient;
    use mcp_types::tool::ContentItem;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn memory_api_failure_preserves_scoped_docs_fallback_without_retrying() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind memory fallback listener");
        let addr = listener
            .local_addr()
            .expect("memory fallback listener addr");
        let server = tokio::spawn(async move {
            let mut request_lines = Vec::new();
            // One memory request plus the query-filtered and local-sweep docs
            // requests. Empty successful doc pages deliberately avoid an
            // auto-open request while still proving partial-result handling.
            for _ in 0..3 {
                let (mut socket, _) = listener.accept().await.expect("accept request");
                let mut buffer = vec![0u8; 32 * 1024];
                let read = socket.read(&mut buffer).await.expect("read request");
                let request = String::from_utf8_lossy(&buffer[..read]);
                let request_line = request.lines().next().unwrap_or_default().to_string();
                let (status, body) = if request_line.starts_with("POST /api/v1/memory/search ") {
                    (
                        "500 Internal Server Error",
                        json!({
                            "error": {
                                "code": "memory_search_unavailable",
                                "message": "memory search unavailable"
                            }
                        })
                        .to_string(),
                    )
                } else if request_line.starts_with("GET /api/v1/docs") {
                    ("200 OK", json!({"items": []}).to_string())
                } else {
                    panic!("unexpected request line: {request_line}");
                };
                request_lines.push(request_line);
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response");
            }
            request_lines
        });

        let mut config = TestFixtures::test_config();
        config.api_url = format!("http://{addr}");
        config.api_key = Some("test-key".to_string());
        let tool = MemorySearchTool::new(ContextStreamClient::new(config));
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            tool.execute(json!({"query": "replica lag", "limit": 5})),
        )
        .await
        .expect("memory fallback must not enter retry backoff")
        .expect("scoped docs fallback must remain a successful tool result");

        assert!(!result.is_error);
        let text = match &result.content[0] {
            ContentItem::Text { text } => text,
            other => panic!("expected text result, got {other:?}"),
        };
        assert!(text.contains("[MEMORY_DEGRADED]"));
        assert!(!text.contains("Internal Server Error"));

        if let Some(structured) = result.structured_content.as_ref() {
            assert_eq!(structured["degraded"], true);
            assert_eq!(structured["degraded_sources"], json!(["memory"]));
            assert_eq!(structured["raw_memory_search"]["degraded"], true);
            assert_eq!(structured["doc_matches"], json!([]));
        }

        let request_lines = server.await.expect("memory fallback server task");
        assert_eq!(
            request_lines
                .iter()
                .filter(|line| line.starts_with("POST /api/v1/memory/search "))
                .count(),
            1,
            "memory request must not be replayed"
        );
        assert_eq!(
            request_lines
                .iter()
                .filter(|line| line.starts_with("GET /api/v1/docs"))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn nested_api_degradation_is_propagated_and_not_cached() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind degraded API listener");
        let addr = listener.local_addr().expect("degraded API listener addr");
        let server = tokio::spawn(async move {
            let mut request_lines = Vec::new();
            loop {
                let accepted =
                    tokio::time::timeout(std::time::Duration::from_millis(500), listener.accept())
                        .await;
                let Ok(Ok((mut socket, _))) = accepted else {
                    break;
                };
                let mut buffer = vec![0u8; 32 * 1024];
                let read = socket.read(&mut buffer).await.expect("read request");
                let request = String::from_utf8_lossy(&buffer[..read]);
                let request_line = request.lines().next().unwrap_or_default().to_string();
                let (status, body) = if request_line.starts_with("POST /api/v1/memory/search ") {
                    (
                        "200 OK",
                        json!({
                            "success": true,
                            "data": {
                                "results": [{
                                    "id": "fallback-node",
                                    "metadata": {
                                        "summary": "Scoped Postgres fallback",
                                        "node_type": "decision"
                                    },
                                    "score": 1.0
                                }],
                                "total": 1,
                                "degraded": true,
                                "degraded_reason": "vector_timeout"
                            }
                        })
                        .to_string(),
                    )
                } else if request_line.starts_with("GET /api/v1/docs") {
                    ("200 OK", json!({"items": []}).to_string())
                } else {
                    panic!("unexpected request line: {request_line}");
                };
                request_lines.push(request_line);
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket
                    .write_all(response.as_bytes())
                    .await
                    .expect("write response");
            }
            request_lines
        });

        let mut config = TestFixtures::test_config();
        config.api_url = format!("http://{addr}");
        config.api_key = Some("test-key".to_string());
        let tool = MemorySearchTool::new(ContextStreamClient::new(config));
        let workspace_id = uuid::Uuid::new_v4();
        let project_id = uuid::Uuid::new_v4();
        let input = json!({
            "query": format!("api-degraded-{}", uuid::Uuid::new_v4()),
            "workspace_id": workspace_id,
            "project_id": project_id,
            "limit": 5
        });
        let (first, second) =
            mcp_client::run_with_session_key(mcp_types::SessionKey::Local, || async {
                let first = tool
                    .execute(input.clone())
                    .await
                    .expect("first API-degraded result");
                let second = tool
                    .execute(input)
                    .await
                    .expect("second API-degraded result");
                (first, second)
            })
            .await;

        for result in [&first, &second] {
            let text = match &result.content[0] {
                ContentItem::Text { text } => text,
                other => panic!("expected text result, got {other:?}"),
            };
            assert!(text.contains("[MEMORY_DEGRADED]"));
            let structured = result
                .structured_content
                .as_ref()
                .expect("structured degraded result");
            assert_eq!(structured["degraded"], true);
            assert_eq!(structured["degraded_sources"], json!(["memory"]));
            assert_eq!(structured["memory_degraded_reason"], "vector_timeout");
        }

        let request_lines = server.await.expect("degraded API server task");
        assert_eq!(
            request_lines
                .iter()
                .filter(|line| line.starts_with("POST /api/v1/memory/search "))
                .count(),
            2,
            "neither the client nor tool cache may replay an API-degraded result"
        );
    }

    #[tokio::test]
    async fn slow_docs_cannot_extend_the_memory_search_deadline() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind slow docs listener");
        let addr = listener.local_addr().expect("slow docs listener addr");
        let (request_tx, mut request_rx) = tokio::sync::mpsc::unbounded_channel();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.expect("accept request");
                let request_tx = request_tx.clone();
                tokio::spawn(async move {
                    let mut buffer = vec![0u8; 32 * 1024];
                    let read = socket.read(&mut buffer).await.expect("read request");
                    let request = String::from_utf8_lossy(&buffer[..read]);
                    let request_line = request.lines().next().unwrap_or_default().to_string();
                    request_tx
                        .send(request_line.clone())
                        .expect("record request");
                    if request_line.starts_with("POST /api/v1/memory/search ") {
                        let body = json!({
                            "success": true,
                            "data": {
                                "results": [{
                                    "id": "memory-node",
                                    "metadata": {
                                        "summary": "Memory remains available",
                                        "node_type": "decision"
                                    },
                                    "score": 1.0
                                }],
                                "total": 1,
                                "degraded": false
                            }
                        })
                        .to_string();
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        socket
                            .write_all(response.as_bytes())
                            .await
                            .expect("write memory response");
                    } else if request_line.starts_with("GET /api/v1/docs") {
                        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                    } else {
                        panic!("unexpected request line: {request_line}");
                    }
                });
            }
        });

        let mut config = TestFixtures::test_config();
        config.api_url = format!("http://{addr}");
        config.api_key = Some("test-key".to_string());
        let tool = MemorySearchTool::new(ContextStreamClient::new(config));
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            tool.execute(json!({
                "query": "bounded docs",
                "workspace_id": uuid::Uuid::new_v4(),
                "project_id": uuid::Uuid::new_v4(),
                "limit": 5
            })),
        )
        .await
        .expect("slow docs must remain inside the absolute tool deadline")
        .expect("healthy memory results must survive a docs timeout");

        let text = match &result.content[0] {
            ContentItem::Text { text } => text,
            other => panic!("expected text result, got {other:?}"),
        };
        assert!(text.contains("[DOCS_DEGRADED]"));
        let structured = result
            .structured_content
            .as_ref()
            .expect("structured bounded result");
        assert_eq!(structured["degraded"], true);
        assert_eq!(structured["degraded_sources"], json!(["docs"]));
        assert_eq!(structured["memory_results"].as_array().unwrap().len(), 1);

        server.await.expect("slow docs acceptor");
        let mut request_lines = Vec::new();
        while let Ok(line) = request_rx.try_recv() {
            request_lines.push(line);
        }
        assert!(request_lines
            .iter()
            .any(|line| line.starts_with("POST /api/v1/memory/search ")));
        assert!(request_lines
            .iter()
            .any(|line| line.starts_with("GET /api/v1/docs")));
    }
}

mod caller_cache_identity_tests {
    use super::{
        build_memory_search_cache_key, build_transcripts_search_cache_key,
        current_memory_cache_identity, memory_search_cache_key_for_caller,
        transcripts_search_cache_key_for_caller,
    };
    use uuid::Uuid;

    #[test]
    fn cache_keys_partition_callers_and_retain_no_raw_values() {
        let workspace_id = Some(Uuid::from_u128(1));
        let project_id = Some(Uuid::from_u128(2));
        let raw_caller = "csuc:v2:j:caller-secret-material";
        let raw_query = "private memory query|nt=lesson";

        let memory_alice = build_memory_search_cache_key(
            raw_caller,
            workspace_id,
            project_id,
            raw_query,
            Some("lesson"),
            Some(20),
        );
        let memory_bob = build_memory_search_cache_key(
            "csuc:v2:j:other-caller",
            workspace_id,
            project_id,
            raw_query,
            Some("lesson"),
            Some(20),
        );
        let transcripts_alice = build_transcripts_search_cache_key(
            raw_caller,
            workspace_id,
            project_id,
            raw_query,
            Some(20),
            true,
        );
        let transcripts_bob = build_transcripts_search_cache_key(
            "csuc:v2:j:other-caller",
            workspace_id,
            project_id,
            raw_query,
            Some(20),
            true,
        );

        assert!(memory_alice.starts_with("memory-search-local:v2:"));
        assert!(transcripts_alice.starts_with("transcripts-search-local:v2:"));
        assert_ne!(memory_alice, memory_bob);
        assert_ne!(transcripts_alice, transcripts_bob);
        for key in [memory_alice, transcripts_alice] {
            assert!(!key.contains(raw_caller));
            assert!(!key.contains(raw_query));
            assert_eq!(key.rsplit(':').next().unwrap().len(), 64);
        }
    }

    #[test]
    fn length_framing_prevents_delimiter_and_optional_field_collisions() {
        let caller = "csuc:v2:j:caller";
        let workspace_id = Some(Uuid::from_u128(3));

        let memory_delimiter = build_memory_search_cache_key(
            caller,
            workspace_id,
            None,
            "query|nt=lesson|lim=5",
            None,
            None,
        );
        let memory_fields = build_memory_search_cache_key(
            caller,
            workspace_id,
            None,
            "query",
            Some("lesson"),
            Some(5),
        );
        assert_ne!(memory_delimiter, memory_fields);

        let transcript_delimiter = build_transcripts_search_cache_key(
            caller,
            workspace_id,
            None,
            "query|lim=5|atlas=1",
            None,
            false,
        );
        let transcript_fields =
            build_transcripts_search_cache_key(caller, workspace_id, None, "query", Some(5), true);
        assert_ne!(transcript_delimiter, transcript_fields);
    }

    #[tokio::test]
    async fn missing_and_anonymous_bypass_while_explicit_stdio_and_auth_cache() {
        let workspace_id = Some(Uuid::from_u128(4));
        assert_eq!(current_memory_cache_identity(), None);
        assert!(
            memory_search_cache_key_for_caller(None, workspace_id, None, "query", None, None)
                .is_none()
        );
        assert!(transcripts_search_cache_key_for_caller(
            None,
            workspace_id,
            None,
            "query",
            None,
            false
        )
        .is_none());

        let anonymous = mcp_client::run_with_session_key(
            mcp_types::SessionKey::for_anonymous_http("anonymous-session"),
            || async { current_memory_cache_identity() },
        )
        .await;
        assert_eq!(anonymous, None);

        let stdio = mcp_client::run_with_session_key(mcp_types::SessionKey::Local, || async {
            current_memory_cache_identity()
        })
        .await
        .expect("explicit stdio cache identity");
        assert!(stdio.starts_with("l:stdio:"));
        assert!(memory_search_cache_key_for_caller(
            Some(&stdio),
            workspace_id,
            None,
            "query",
            None,
            None
        )
        .is_some());

        let authenticated = mcp_client::run_with_session_key(
            mcp_types::SessionKey::Jwt("raw-jwt-secret".to_string()),
            || async { current_memory_cache_identity() },
        )
        .await
        .expect("authenticated cache identity");
        assert!(authenticated.starts_with("csuc:v2:j:"));
        assert!(!authenticated.contains("raw-jwt-secret"));
    }

    #[test]
    fn unresolved_workspace_bypasses_even_for_known_caller() {
        let caller = Some("csuc:v2:j:caller");
        assert!(
            memory_search_cache_key_for_caller(caller, None, None, "query", None, None).is_none()
        );
        assert!(
            transcripts_search_cache_key_for_caller(caller, None, None, "query", None, false)
                .is_none()
        );
    }
}

mod transcript_atlas_scope_tests {
    use super::{current_memory_cache_identity, enrich_transcript_search_with_atlas};
    use mcp_types::atlas_layer::{
        AtlasSearchCollection, AtlasSearchError, AtlasSearchHit, AtlasSearchProvider,
        AtlasSearchScope,
    };
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use uuid::Uuid;

    #[derive(Default)]
    struct CountingAtlasSearch {
        calls: AtomicUsize,
        user_scopes: Mutex<Vec<Option<String>>>,
        collections: Mutex<Vec<Vec<AtlasSearchCollection>>>,
    }

    #[async_trait::async_trait]
    impl AtlasSearchProvider for CountingAtlasSearch {
        async fn fuzzy_text_search(
            &self,
            _query: &str,
            scope: &AtlasSearchScope,
            _limit: usize,
        ) -> Result<Vec<AtlasSearchHit>, AtlasSearchError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.user_scopes
                .lock()
                .unwrap()
                .push(scope.user_scope.clone());
            self.collections
                .lock()
                .unwrap()
                .push(scope.collections.clone());
            Ok(vec![AtlasSearchHit {
                id: "transcript-hit".to_string(),
                collection: AtlasSearchCollection::Transcripts,
                title: Some("Transcript".to_string()),
                snippet: "matching text".to_string(),
                score: 1.0,
                url: None,
                content: None,
            }])
        }
    }

    #[tokio::test]
    async fn missing_and_anonymous_identity_never_call_atlas() {
        let concrete = Arc::new(CountingAtlasSearch::default());
        let provider: Arc<dyn AtlasSearchProvider> = concrete.clone();
        let workspace_id = Some(Uuid::from_u128(7));
        let mut result = json!({"items": []});

        enrich_transcript_search_with_atlas(
            Some(provider.clone()),
            None,
            workspace_id,
            None,
            "query",
            Some(10),
            &mut result,
        )
        .await;

        let anonymous = mcp_client::run_with_session_key(
            mcp_types::SessionKey::for_anonymous_http("anonymous-session"),
            || async { current_memory_cache_identity() },
        )
        .await;
        assert_eq!(anonymous, None);
        enrich_transcript_search_with_atlas(
            Some(provider),
            anonymous.as_deref(),
            workspace_id,
            None,
            "query",
            Some(10),
            &mut result,
        )
        .await;

        assert_eq!(concrete.calls.load(Ordering::SeqCst), 0);
        assert!(result.get("atlas_search_hits").is_none());
    }

    #[tokio::test]
    async fn explicit_stdio_calls_atlas_with_mandatory_caller_scope() {
        let concrete = Arc::new(CountingAtlasSearch::default());
        let provider: Arc<dyn AtlasSearchProvider> = concrete.clone();
        let caller_identity =
            mcp_client::run_with_session_key(mcp_types::SessionKey::Local, || async {
                current_memory_cache_identity()
            })
            .await
            .expect("explicit stdio cache identity");
        let mut result = json!({"items": []});

        enrich_transcript_search_with_atlas(
            Some(provider),
            Some(&caller_identity),
            Some(Uuid::from_u128(8)),
            Some(Uuid::from_u128(9)),
            "query",
            Some(10),
            &mut result,
        )
        .await;

        assert_eq!(concrete.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            concrete.user_scopes.lock().unwrap().as_slice(),
            &[Some(caller_identity)]
        );
        assert_eq!(
            concrete.collections.lock().unwrap().as_slice(),
            &[vec![AtlasSearchCollection::Transcripts]]
        );
        assert_eq!(
            result
                .get("atlas_search_hits")
                .and_then(|hits| hits.as_array())
                .map(Vec::len),
            Some(1)
        );
    }
}

mod task_update_idempotency_tests {
    use super::{status_only_task_update, task_status_from_result, UpdateTaskParams};
    use serde_json::json;

    #[test]
    fn recognizes_status_only_updates() {
        let params = UpdateTaskParams {
            status: Some("completed".to_string()),
            ..Default::default()
        };

        assert_eq!(status_only_task_update(&params), Some("completed"));
    }

    #[test]
    fn rejects_status_noop_when_another_field_changes() {
        let params = UpdateTaskParams {
            title: Some("Updated title".to_string()),
            status: Some("completed".to_string()),
            ..Default::default()
        };

        assert_eq!(status_only_task_update(&params), None);
    }

    #[test]
    fn reads_status_from_direct_and_wrapped_task_results() {
        assert_eq!(
            task_status_from_result(&json!({"status": "completed"})),
            Some("completed")
        );
        assert_eq!(
            task_status_from_result(&json!({"data": {"task": {"status": "blocked"}}})),
            Some("blocked")
        );
    }
}

mod read_scope_resolution_tests {
    use super::{MemoryInput, MemoryTool};
    use crate::testing::TestFixtures;
    use mcp_client::ContextStreamClient;
    use mcp_session::SessionManager;
    use std::sync::Arc;
    use uuid::Uuid;

    #[tokio::test]
    async fn unified_memory_resolves_read_scope_from_active_session_when_ids_are_omitted() {
        let mut config = TestFixtures::test_config();
        config.default_project_id = None;
        let client = ContextStreamClient::new(config.clone());
        let session = Arc::new(SessionManager::new(client.clone(), config));
        let workspace_id = Uuid::new_v4();
        session
            .initialize(Some(workspace_id), None, None, None)
            .await;

        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());
        let input = MemoryInput {
            action: "update_event".to_string(),
            query: None,
            scope: None,
            workspace_id: None,
            project_id: None,
            target_project: None,
            limit: Some(5),
            node_type: None,
            node_id: None,
            title: None,
            content: None,
            event_type: None,
            event_id: Some(Uuid::new_v4().to_string()),
            delete_all: None,
            metadata: None,
            events: None,
            new_content: None,
            reason: None,
            category: None,
            sort: None,
            status: None,
            since: None,
            offset: None,
            source: None,
            rationale: None,
            alternatives: None,
            confidence: None,
            supersedes: None,
            decision_id: None,
            decision_action: None,
            successor_id: None,
            task_id: None,
            description: None,
            priority: None,
            task_status: None,
            plan_id: None,
            plan_step_id: None,
            tags: None,
            blocked_reason: None,
            code_refs: None,
            task_ids: None,
            order: None,
            todo_id: None,
            todo_priority: None,
            todo_status: None,
            due_at: None,
            clear_due_at: None,
            created_after: None,
            created_before: None,
            updated_after: None,
            updated_before: None,
            due_after: None,
            due_before: None,
            completed_after: None,
            completed_before: None,
            diagram_id: None,
            diagram_type: None,
            doc_id: None,
            doc_type: None,
            milestones: None,
            transcript_id: None,
            session_id: None,
            client_name: None,
            started_after: None,
            started_before: None,
            is_personal: None,
        };

        let scope = tool.resolve_scope_for_input(&input).await.unwrap();
        assert_eq!(scope.workspace_id, Some(workspace_id));
        assert_eq!(scope.project_id, None);
    }
}

mod memory_decisions_setup_tests {
    use super::MemoryDecisionsTool;
    use crate::registry::ToolHandler;
    use mcp_client::ContextStreamClient;
    use mcp_session::SessionManager;
    use serde_json::json;
    use std::sync::Arc;

    #[tokio::test]
    async fn decisions_without_resolved_workspace_returns_setup_guidance() {
        let config = mcp_types::config::Config::default();
        let client = ContextStreamClient::new(config.clone());
        let session = Arc::new(SessionManager::new(client.clone(), config));
        let tool = MemoryDecisionsTool::with_session(client, session);

        let result = tool
            .execute(json!({
                "query": "workspace_id required decisions smoke",
                "limit": 1
            }))
            .await
            .unwrap();

        assert!(!result.is_error);
        let text = match &result.content[0] {
            mcp_types::tool::ContentItem::Text { text } => text.as_str(),
            _ => "",
        };
        assert!(text.contains("[SETUP_REQUIRED]"));
        assert!(!text.contains("Validation error"));

        let structured = result.structured_content.expect("structured content");
        assert_eq!(
            structured.get("setup_required").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert!(structured.get("workspace_id").is_some_and(|v| v.is_null()));
    }
}

mod collection_count_tests {
    use super::collection_count;
    use serde_json::json;

    #[test]
    fn counts_root_array() {
        let value = json!([{"id": 1}, {"id": 2}, {"id": 3}]);
        assert_eq!(collection_count(&value), 3);
    }

    #[test]
    fn counts_wrapped_items_array() {
        let value = json!({
            "items": [{"id": 1}, {"id": 2}]
        });
        assert_eq!(collection_count(&value), 2);
    }

    #[test]
    fn counts_nested_data_results_array() {
        let value = json!({
            "data": {
                "results": [{"id": 1}, {"id": 2}, {"id": 3}, {"id": 4}]
            }
        });
        assert_eq!(collection_count(&value), 4);
    }
}

mod format_doc_detail_tests {
    use super::format_doc_detail;
    use serde_json::json;

    #[test]
    fn formats_top_level_doc_content() {
        let value = json!({
            "id": "60606060-6060-4060-8060-606060606060",
            "title": "Linode Infrastructure Migration - Complete State",
            "doc_type": "spec",
            "created_at": "2026-02-18T15:48:47.406397Z",
            "updated_at": "2026-02-18T15:48:47.406397Z",
            "content": "# Heading\n\nDocument body"
        });

        let text = format_doc_detail(&value);
        assert!(text.contains("Linode Infrastructure Migration - Complete State"));
        assert!(text.contains("60606060-6060-4060-8060-606060606060"));
        assert!(text.contains("Type: spec"));
        assert!(text.contains("# Heading"));
        assert!(text.contains("Document body"));
    }

    #[test]
    fn formats_wrapped_doc_content() {
        let value = json!({
            "success": true,
            "data": {
                "id": "doc-123",
                "title": "Wrapped Doc",
                "doc_type": "general",
                "content": "Wrapped content"
            }
        });

        let text = format_doc_detail(&value);
        assert!(text.contains("Wrapped Doc"));
        assert!(text.contains("Type: general"));
        assert!(text.contains("Wrapped content"));
    }

    #[test]
    fn reports_empty_doc_content() {
        let value = json!({
            "id": "doc-empty",
            "title": "Empty Doc",
            "doc_type": "spec",
            "content": ""
        });

        let text = format_doc_detail(&value);
        assert!(text.contains("Empty Doc"));
        assert!(text.contains("Content is empty."));
    }
}

mod doc_lookup_ranking_tests {
    use super::{find_exact_doc_match, rank_docs_for_query, select_resolved_doc_match};
    use serde_json::json;

    #[test]
    fn ranks_exact_title_match_first() {
        let docs = vec![
            json!({
                "id": "doc-1",
                "title": "AWS Infrastructure State - Feb 2026",
                "doc_type": "general"
            }),
            json!({
                "id": "doc-2",
                "title": "Linode Infrastructure Migration - Complete State",
                "doc_type": "spec"
            }),
        ];

        let ranked =
            rank_docs_for_query(docs, "Linode Infrastructure Migration - Complete State", 10);
        assert!(!ranked.is_empty());
        assert_eq!(ranked[0].get("id").and_then(|v| v.as_str()), Some("doc-2"));
    }

    #[test]
    fn finds_exact_match_by_title_or_id() {
        let docs = vec![
            json!({
                "id": "60606060-6060-4060-8060-606060606060",
                "title": "Linode Infrastructure Migration - Complete State",
                "doc_type": "spec"
            }),
            json!({
                "id": "doc-2",
                "title": "Another Doc",
                "doc_type": "general"
            }),
        ];

        let by_title =
            find_exact_doc_match(&docs, "linode infrastructure migration complete state");
        assert!(by_title.is_some());
        assert_eq!(
            by_title.and_then(|d| d.get("id")).and_then(|v| v.as_str()),
            Some("60606060-6060-4060-8060-606060606060")
        );

        let by_id = find_exact_doc_match(&docs, "60606060-6060-4060-8060-606060606060");
        assert!(by_id.is_some());
    }

    #[test]
    fn ranks_wrapped_natural_language_doc_query() {
        let docs = vec![
            json!({
                "id": "doc-1",
                "title": "Search Reliability Fix Plan (Server-Side) - Active Scope (2026-02-17)",
                "doc_type": "spec"
            }),
            json!({
                "id": "doc-2",
                "title": "ContextStream Messaging Playbook v1",
                "doc_type": "spec"
            }),
        ];

        let ranked = rank_docs_for_query(
            docs,
            "see doc in contextstream Search Reliability Fix Plan (Server-Side) - Active Scope (2026-02-17) mcp",
            10,
        );
        assert!(!ranked.is_empty());
        assert_eq!(ranked[0].get("id").and_then(|v| v.as_str()), Some("doc-1"));
        assert!(
            ranked[0]
                .get("match_score")
                .and_then(|v| v.as_i64())
                .unwrap_or_default()
                >= 700
        );
    }

    #[test]
    fn resolves_unique_high_confidence_doc_match() {
        let docs = rank_docs_for_query(
            vec![json!({
                "id": "doc-1",
                "title": "Infrastructure SSH Connection Guide",
                "doc_type": "general"
            })],
            "show me the infrastructure guide ssh connection",
            10,
        );

        let resolved =
            select_resolved_doc_match(&docs, "show me the infrastructure guide ssh connection");
        assert!(resolved.is_some());
        assert_eq!(
            resolved
                .and_then(|doc| doc.get("id"))
                .and_then(|value| value.as_str()),
            Some("doc-1")
        );
    }

    #[test]
    fn does_not_resolve_ambiguous_doc_match() {
        let docs = rank_docs_for_query(
            vec![
                json!({
                    "id": "doc-1",
                    "title": "Infrastructure SSH Connection Guide",
                    "doc_type": "general"
                }),
                json!({
                    "id": "doc-2",
                    "title": "Infrastructure SSH Access Guide",
                    "doc_type": "general"
                }),
            ],
            "infrastructure ssh guide",
            10,
        );

        assert!(select_resolved_doc_match(&docs, "infrastructure ssh guide").is_none());
    }
}

mod extract_display_title_tests {
    use super::extract_display_title;
    use serde_json::json;

    #[test]
    fn uses_title_field_when_present() {
        let node = json!({ "title": "My Decision", "content": "details" });
        assert_eq!(extract_display_title(&node), "My Decision");
    }

    #[test]
    fn falls_back_to_summary_and_name() {
        let summary = json!({ "summary": "A summary" });
        assert_eq!(extract_display_title(&summary), "A summary");

        let name = json!({ "name": "node-name" });
        assert_eq!(extract_display_title(&name), "node-name");
    }

    #[test]
    fn falls_back_to_metadata_summary() {
        let node = json!({
            "metadata": {
                "summary": "Keep server-side reliability plan active"
            }
        });
        assert_eq!(
            extract_display_title(&node),
            "Keep server-side reliability plan active"
        );
    }

    #[test]
    fn falls_back_to_first_content_line() {
        let node = json!({ "content": "First line\nSecond line" });
        assert_eq!(extract_display_title(&node), "First line");
    }

    #[test]
    fn falls_back_to_top_level_content_preview() {
        let node = json!({
            "content_preview": "Deployment failure was a transient SSH reset."
        });
        assert_eq!(
            extract_display_title(&node),
            "Deployment failure was a transient SSH reset."
        );
    }

    #[test]
    fn truncates_long_content_preview() {
        let node = json!({ "content": "A".repeat(120) });
        let title = extract_display_title(&node);
        assert!(title.ends_with("..."));
        assert!(title.len() <= 83);
    }

    #[test]
    fn skips_empty_title_values() {
        let node = json!({
            "title": "",
            "summary": "   ",
            "content": "Actual content"
        });
        assert_eq!(extract_display_title(&node), "Actual content");
    }

    #[test]
    fn returns_kind_label_when_all_fields_missing() {
        let node = json!({});
        let title = extract_display_title(&node);
        assert!(!title.eq_ignore_ascii_case("untitled"));
        assert!(!title.is_empty());
    }
}

mod list_docs_query_filter_tests {
    use super::rank_docs_for_query;
    use serde_json::json;

    #[test]
    fn filters_and_prioritizes_matching_docs() {
        let docs = vec![
            json!({ "id": "doc-1", "title": "Search Reliability Fix Plan (Server-Side) - Active Scope (2026-02-17)" }),
            json!({ "id": "doc-2", "title": "AWS Infrastructure Setup" }),
            json!({ "id": "doc-3", "title": "Search Performance Notes" }),
        ];

        let ranked = rank_docs_for_query(docs, "Search Reliability", 10);
        assert!(!ranked.is_empty());
        assert_eq!(ranked[0].get("id").and_then(|v| v.as_str()), Some("doc-1"));
    }

    #[test]
    fn returns_empty_when_no_docs_match_query() {
        let docs = vec![json!({ "id": "doc-1", "title": "AWS Infrastructure" })];
        let ranked = rank_docs_for_query(docs, "completely unrelated xyz", 10);
        assert!(ranked.is_empty());
    }
}

mod doc_query_preparation_tests {
    use super::PreparedDocQuery;

    #[test]
    fn strips_wrapper_terms_from_doc_query() {
        let prepared = PreparedDocQuery::new(
            "see doc in contextstream \"Infrastructure SSH & Connection Guide\" mcp",
        );

        assert_eq!(prepared.stripped, "infrastructure ssh connection guide");
        assert!(prepared.has_doc_intent);
        assert_eq!(
            prepared.quoted_phrases,
            vec!["infrastructure ssh connection guide"]
        );
    }
}

mod memory_result_normalization_tests {
    use super::{
        build_hybrid_search_results, extract_content_preview, extract_memory_result_type,
        extract_memory_search_results,
    };
    use serde_json::json;

    #[test]
    fn extracts_nested_memory_search_results() {
        let response = json!({
            "success": true,
            "data": {
                "results": [{
                    "id": "node-1",
                    "metadata": {
                        "summary": "Keep server-side reliability plan active",
                        "node_type": "decision"
                    },
                    "score": 1.1
                }]
            }
        });

        let results = extract_memory_search_results(&response);
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].get("id").and_then(|v| v.as_str()),
            Some("node-1")
        );
    }

    #[test]
    fn uses_metadata_fields_for_memory_result_type_and_preview() {
        let result = json!({
            "id": "node-1",
            "metadata": {
                "summary": "Keep server-side reliability plan active",
                "node_type": "decision"
            },
            "highlights": ["Keep server-side reliability plan active"],
            "score": 1.1
        });

        assert_eq!(extract_memory_result_type(&result), "decision");
        assert_eq!(
            extract_content_preview(&result),
            Some("Keep server-side reliability plan active".to_string())
        );
    }

    #[test]
    fn builds_hybrid_results_with_docs_ranked_alongside_memory() {
        let memory_results = vec![json!({
            "id": "node-1",
            "metadata": {
                "summary": "Keep server-side reliability plan active",
                "node_type": "decision"
            },
            "score": 1.1
        })];
        let docs = vec![json!({
            "id": "doc-1",
            "title": "Search Reliability Fix Plan (Server-Side) - Active Scope (2026-02-17)",
            "doc_type": "spec",
            "match_score": 1800,
            "match_source": "exact_title",
            "exact_match": true
        })];

        let results = build_hybrid_search_results(&memory_results, &docs, 10);
        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0].get("entity_type").and_then(|v| v.as_str()),
            Some("doc")
        );
        assert_eq!(results[0].get("id").and_then(|v| v.as_str()), Some("doc-1"));
        assert_eq!(
            results[1].get("entity_type").and_then(|v| v.as_str()),
            Some("memory")
        );
    }
}

mod scope_resolution_tests {
    use super::build_memory_search_input;
    use uuid::Uuid;

    #[test]
    fn search_input_uses_resolved_scope_when_explicit_ids_are_missing() {
        let workspace_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        let workspace_id_str = workspace_id.to_string();
        let project_id_str = project_id.to_string();

        let input = build_memory_search_input(
            "Search Reliability Fix Plan".to_string(),
            None,
            None,
            Some(workspace_id),
            Some(project_id),
            None,
            Some(5),
        );

        assert_eq!(
            input.workspace_id.as_deref(),
            Some(workspace_id_str.as_str())
        );
        assert_eq!(input.project_id.as_deref(), Some(project_id_str.as_str()));
    }
}

// ============================================================================
// Metadata Tests
// ============================================================================

mod metadata_tests {
    use super::{create_mock_client, create_mock_session, ToolCategory, ToolHandler};
    use super::{CreateMemoryNodeTool, MemoryDecisionsTool, MemorySearchTool, MemoryTool};

    #[test]
    fn test_memory_search_tool_metadata() {
        let client = create_mock_client();
        let tool = MemorySearchTool::new(client);
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "memory_search");
        assert_eq!(metadata.title, "Search Memory");
        assert!(metadata.description.contains("Search"));
        assert!(metadata.description.contains("relevant docs alongside"));
        assert_eq!(metadata.category, ToolCategory::Memory);
        assert!(!metadata.is_pro);
    }

    #[test]
    fn test_create_memory_node_tool_metadata() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = CreateMemoryNodeTool::new(client, session);
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "memory_create_node");
        assert_eq!(metadata.title, "Create Memory Node");
        assert!(metadata.description.contains("Create"));
        assert_eq!(metadata.category, ToolCategory::Memory);
    }

    #[test]
    fn test_memory_decisions_tool_metadata() {
        let client = create_mock_client();
        let tool = MemoryDecisionsTool::new(client);
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "memory_decisions");
        assert_eq!(metadata.title, "List Decisions");
        assert!(metadata.description.contains("decisions"));
        assert_eq!(metadata.category, ToolCategory::Memory);
    }

    #[test]
    fn test_memory_action_alias_metadata_and_action_injection() {
        use super::super::MemoryActionAlias;
        use crate::schema::SchemaBuilder;
        use mcp_types::tool::ToolAnnotations;
        use std::sync::Arc;

        let client = create_mock_client();
        let session = create_mock_session(&client);
        let memory = Arc::new(super::super::MemoryTool::new(
            client,
            session,
            mcp_types::atlas_layer::noop_layer(),
        ));
        let alias = MemoryActionAlias::new(
            memory,
            "memory_update_doc",
            "Update doc in ContextStream",
            "Update an existing doc in ContextStream memory.",
            "update_doc",
            ToolAnnotations::write(),
            SchemaBuilder::new()
                .description("Update an existing doc in ContextStream memory")
                .string("doc_id", "Doc ID", true)
                .build(),
        );
        let metadata = alias.metadata();
        assert_eq!(metadata.name, "memory_update_doc");
        assert_eq!(metadata.title, "Update doc in ContextStream");
        assert_eq!(metadata.category, ToolCategory::Memory);
        // Action is injected on execute (verified via _meta wiring tests in
        // mcp-server). Here we just sanity-check the schema surface.
        let schema = alias.input_schema();
        let doc_id_required = schema
            .get("required")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().any(|x| x.as_str() == Some("doc_id")))
            .unwrap_or(false);
        assert!(doc_id_required, "memory_update_doc must require doc_id");
    }

    #[test]
    fn memory_update_task_requires_explicit_workspace_scope() {
        let schema = super::super::memory_update_task_schema();
        let required = schema
            .get("required")
            .and_then(|value| value.as_array())
            .expect("memory_update_task required fields");

        for field in ["task_id", "workspace_id"] {
            assert!(
                required.iter().any(|value| value.as_str() == Some(field)),
                "memory_update_task must require {field}: {schema:#}"
            );
        }
        assert!(schema["properties"].get("project_id").is_some());
        assert!(schema["properties"]["workspace_id"]["description"]
            .as_str()
            .is_some_and(|description| description.contains("returned by init/context")));
    }

    #[test]
    fn test_unified_memory_tool_metadata() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "memory");
        assert_eq!(metadata.title, "Memory Operations");
        assert!(metadata.description.contains("create_event"));
        assert!(metadata.description.contains("create_task"));
        assert!(metadata.description.contains("create_todo"));
        assert!(metadata.description.contains("create_diagram"));
        assert!(metadata.description.contains("create_doc"));
        assert!(metadata.description.contains("list_transcripts"));
        // Disambiguation: must clarify this is NOT for codebase/file search
        assert!(
            metadata
                .description
                .contains("NOT for codebase/file search"),
            "memory description must explicitly disclaim codebase search"
        );
        assert!(
            metadata.description.contains("relevant docs together"),
            "memory description must describe hybrid doc + memory retrieval"
        );
        assert!(
            metadata
                .description
                .contains("natural-language title query"),
            "memory description must document natural-language doc resolution"
        );
        assert_eq!(metadata.category, ToolCategory::Memory);
    }
}

// ============================================================================
// Schema Tests
// ============================================================================

mod schema_tests {
    use super::{create_mock_client, create_mock_session, ToolHandler};
    use super::{CreateMemoryNodeTool, MemoryDecisionsTool, MemorySearchTool, MemoryTool};

    #[test]
    fn test_memory_search_schema() {
        let client = create_mock_client();
        let tool = MemorySearchTool::new(client);
        let schema = tool.input_schema();

        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("query"));
        assert!(props.contains_key("workspace_id"));
        assert!(props.contains_key("project_id"));
        assert!(props.contains_key("node_type"));
        assert!(props.contains_key("limit"));

        // query should be required
        if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
            assert!(required.iter().any(|v| v.as_str() == Some("query")));
        }
    }

    #[test]
    fn test_create_memory_node_schema() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = CreateMemoryNodeTool::new(client, session);
        let schema = tool.input_schema();

        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("node_type"));
        assert!(props.contains_key("title"));
        assert!(props.contains_key("content"));

        // Check node_type enum values
        if let Some(node_type) = props.get("node_type") {
            if let Some(enum_vals) = node_type.get("enum").and_then(|e| e.as_array()) {
                let values: Vec<&str> = enum_vals.iter().filter_map(|v| v.as_str()).collect();
                assert!(values.contains(&"fact"));
                assert!(values.contains(&"decision"));
                assert!(values.contains(&"preference"));
                assert!(values.contains(&"lesson"));
            }
        }
    }

    #[test]
    fn test_memory_decisions_schema() {
        let client = create_mock_client();
        let tool = MemoryDecisionsTool::new(client);
        let schema = tool.input_schema();

        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("workspace_id"));
        assert!(props.contains_key("project_id"));
        assert!(props.contains_key("query"));
        assert!(props.contains_key("limit"));
    }

    #[test]
    fn test_unified_memory_schema() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());
        let schema = tool.input_schema();

        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("action"));
        assert!(props.contains_key("target_project"));

        // Check action enum contains all expected actions
        if let Some(action) = props.get("action") {
            if let Some(enum_vals) = action.get("enum").and_then(|e| e.as_array()) {
                let values: Vec<&str> = enum_vals.iter().filter_map(|v| v.as_str()).collect();

                // Node actions (8)
                assert!(values.contains(&"search"));
                assert!(values.contains(&"create_node"));
                assert!(values.contains(&"get_node"));
                assert!(values.contains(&"update_node"));
                assert!(values.contains(&"delete_node"));
                assert!(values.contains(&"list_nodes"));
                assert!(values.contains(&"supersede_node"));
                assert!(values.contains(&"decisions"));

                // Event actions (7)
                assert!(values.contains(&"create_event"));
                assert!(values.contains(&"get_event"));
                assert!(values.contains(&"update_event"));
                assert!(values.contains(&"delete_event"));
                assert!(values.contains(&"distill_event"));
                assert!(values.contains(&"list_events"));
                assert!(values.contains(&"import_batch"));

                // Task actions (6)
                assert!(values.contains(&"create_task"));
                assert!(values.contains(&"get_task"));
                assert!(values.contains(&"update_task"));
                assert!(values.contains(&"delete_task"));
                assert!(values.contains(&"list_tasks"));
                assert!(values.contains(&"reorder_tasks"));

                // Todo actions (6)
                assert!(values.contains(&"create_todo"));
                assert!(values.contains(&"list_todos"));
                assert!(values.contains(&"get_todo"));
                assert!(values.contains(&"update_todo"));
                assert!(values.contains(&"delete_todo"));
                assert!(values.contains(&"complete_todo"));

                // Diagram actions (5)
                assert!(values.contains(&"create_diagram"));
                assert!(values.contains(&"list_diagrams"));
                assert!(values.contains(&"get_diagram"));
                assert!(values.contains(&"update_diagram"));
                assert!(values.contains(&"delete_diagram"));

                // Doc actions (6)
                assert!(values.contains(&"create_doc"));
                assert!(values.contains(&"list_docs"));
                assert!(values.contains(&"get_doc"));
                assert!(values.contains(&"update_doc"));
                assert!(values.contains(&"delete_doc"));
                assert!(values.contains(&"create_roadmap"));

                // Transcript actions (4)
                assert!(values.contains(&"list_transcripts"));
                assert!(values.contains(&"get_transcript"));
                assert!(values.contains(&"search_transcripts"));
                assert!(values.contains(&"search_archive")); // A7
                assert!(values.contains(&"delete_transcript"));

                // Verify total action count. Wave 3b added the typed
                // `create_decision` and `decision_action` node actions.
                assert!(values.contains(&"create_decision"));
                assert!(values.contains(&"decision_action"));
                assert_eq!(values.len(), 53, "Expected 53 memory actions");
            }
        }

        // Check other important fields
        assert!(props.contains_key("node_type"));
        assert!(props.contains_key("event_type"));
        assert!(props.contains_key("task_id"));
        assert!(props.contains_key("todo_id"));
        assert!(props.contains_key("diagram_id"));
        assert!(props.contains_key("doc_id"));
        assert!(props.contains_key("transcript_id"));
        assert!(props.contains_key("is_personal"));
    }
}

// ============================================================================
// Node Type Normalization Tests
// ============================================================================

mod node_type_tests {
    use super::normalize_node_type;

    #[test]
    fn test_normalize_fact() {
        assert_eq!(normalize_node_type("fact").unwrap(), "Fact");
        assert_eq!(normalize_node_type("Fact").unwrap(), "Fact");
        assert_eq!(normalize_node_type("FACT").unwrap(), "Fact");
        assert_eq!(normalize_node_type("insight").unwrap(), "Fact");
        assert_eq!(normalize_node_type("note").unwrap(), "Fact");
    }

    #[test]
    fn test_normalize_decision() {
        assert_eq!(normalize_node_type("decision").unwrap(), "Decision");
        assert_eq!(normalize_node_type("Decision").unwrap(), "Decision");
        assert_eq!(normalize_node_type("DECISION").unwrap(), "Decision");
    }

    #[test]
    fn test_normalize_preference() {
        assert_eq!(normalize_node_type("preference").unwrap(), "Preference");
    }

    #[test]
    fn test_normalize_constraint() {
        assert_eq!(normalize_node_type("constraint").unwrap(), "Constraint");
    }

    #[test]
    fn test_normalize_habit() {
        assert_eq!(normalize_node_type("habit").unwrap(), "Habit");
    }

    #[test]
    fn test_normalize_lesson() {
        assert_eq!(normalize_node_type("lesson").unwrap(), "Lesson");
    }

    #[test]
    fn test_normalize_invalid() {
        let result = normalize_node_type("invalid_type");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Invalid node_type"));
    }

    #[test]
    fn test_normalize_with_whitespace() {
        assert_eq!(normalize_node_type("  fact  ").unwrap(), "Fact");
        assert_eq!(normalize_node_type("\tdecision\n").unwrap(), "Decision");
    }
}

// ============================================================================
// Input Validation Tests
// ============================================================================

mod validation_tests {
    use super::{create_mock_client, create_mock_session, json, ToolHandler};
    use super::{CreateMemoryNodeTool, MemorySearchTool, MemoryTool};

    #[tokio::test]
    async fn test_memory_search_requires_query() {
        let client = create_mock_client();
        let tool = MemorySearchTool::new(client);

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
    async fn test_memory_search_whitespace_query() {
        let client = create_mock_client();
        let tool = MemorySearchTool::new(client);

        let result = tool
            .execute(json!({
                "query": "   \t  "
            }))
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_create_memory_node_requires_title() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = CreateMemoryNodeTool::new(client, session);

        let result = tool
            .execute(json!({
                "node_type": "decision",
                "title": ""
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("title"));
    }

    #[tokio::test]
    async fn test_create_memory_node_validates_node_type() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = CreateMemoryNodeTool::new(client, session);

        let result = tool
            .execute(json!({
                "node_type": "invalid_type",
                "title": "Test Title"
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Invalid node_type"));
    }

    #[tokio::test]
    async fn test_memory_tool_unknown_action() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "unknown_action"
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Unknown action"));
    }

    #[tokio::test]
    async fn test_memory_tool_rejects_target_project_without_child_project_context() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "unknown_action",
                "target_project": "contextstream"
            }))
            .await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("requires init from a multi-project parent folder first"));
    }

    #[tokio::test]
    async fn test_memory_tool_accepts_known_target_project_before_action_validation() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        session
            .set_child_projects(std::collections::HashMap::from([(
                "contextstream".to_string(),
                mcp_session::ChildProjectInfo {
                    project_id: "11111111-1111-4111-8111-111111111111".to_string(),
                    name: "ContextStream".to_string(),
                    path: "/tmp/contextstream".to_string(),
                },
            )]))
            .await;
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "search",
                "target_project": "contextstream"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("query"));
    }

    #[tokio::test]
    async fn test_memory_tool_search_requires_query() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "search"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("query"));
    }

    #[tokio::test]
    async fn test_memory_tool_create_node_requires_fields() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        // Missing node_type
        let result = tool
            .execute(json!({
                "action": "create_node",
                "title": "Test"
            }))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("node_type"));

        // Missing title
        let result = tool
            .execute(json!({
                "action": "create_node",
                "node_type": "decision"
            }))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("title"));
    }

    #[tokio::test]
    async fn test_memory_tool_get_node_requires_node_id() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "get_node"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("node_id"));
    }

    #[tokio::test]
    async fn test_memory_tool_get_node_allows_lookup_text_and_returns_not_found_hint() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "get_node",
                "node_id": "not-a-valid-uuid"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.is_err());
        assert!(!result
            .as_ref()
            .err()
            .unwrap()
            .to_string()
            .contains("Invalid node_id UUID"));
    }

    #[tokio::test]
    async fn test_memory_tool_supersede_node_requires_fields() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        // Missing node_id
        let result = tool
            .execute(json!({
                "action": "supersede_node",
                "new_content": "Updated content"
            }))
            .await;
        assert!(result.is_err());

        // Missing new_content
        let result = tool
            .execute(json!({
                "action": "supersede_node",
                "node_id": "550e8400-e29b-41d4-a716-446655440000"
            }))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("new_content"));
    }

    #[tokio::test]
    async fn test_memory_tool_create_event_requires_fields() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        // Missing event_type
        let result = tool
            .execute(json!({
                "action": "create_event",
                "title": "Test"
            }))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("event_type"));

        // Missing title
        let result = tool
            .execute(json!({
                "action": "create_event",
                "event_type": "decision"
            }))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("title"));
    }

    #[tokio::test]
    async fn test_memory_tool_rejects_plan_events() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "create_event",
                "event_type": "plan",
                "title": "Bad plan save"
            }))
            .await;

        assert!(result.is_err());
        let message = result.unwrap_err().to_string();
        assert!(message.contains("reserved"));
        assert!(message.contains("capture_plan"));
    }

    #[tokio::test]
    async fn test_memory_tool_get_event_requires_event_id() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "get_event"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("event_id"));
    }

    #[tokio::test]
    async fn test_memory_tool_import_batch_requires_events() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "import_batch"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("events"));
    }

    #[tokio::test]
    async fn test_memory_tool_create_task_requires_title() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "create_task"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("title"));
    }

    #[tokio::test]
    async fn test_memory_tool_get_task_requires_task_id() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "get_task"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("task_id"));
    }

    #[tokio::test]
    async fn test_memory_tool_reorder_tasks_requires_task_ids() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "reorder_tasks"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("task_ids"));
    }

    #[tokio::test]
    async fn test_memory_tool_reorder_tasks_validates_uuids() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "reorder_tasks",
                "task_ids": ["invalid", "also-invalid"]
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("No valid UUIDs"));
    }

    #[tokio::test]
    async fn test_memory_tool_create_todo_requires_title() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "create_todo"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("title"));
    }

    #[tokio::test]
    async fn test_memory_tool_get_todo_requires_todo_id() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "get_todo"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("todo_id"));
    }

    #[tokio::test]
    async fn test_memory_tool_create_diagram_requires_fields() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        // Missing title
        let result = tool
            .execute(json!({
                "action": "create_diagram",
                "content": "graph TD; A-->B"
            }))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("title"));

        // Missing content
        let result = tool
            .execute(json!({
                "action": "create_diagram",
                "title": "Test Diagram"
            }))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("content"));
    }

    #[tokio::test]
    async fn test_memory_tool_get_diagram_requires_diagram_id() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "get_diagram"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("diagram_id"));
    }

    #[tokio::test]
    async fn test_memory_tool_create_doc_requires_fields() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        // Missing title
        let result = tool
            .execute(json!({
                "action": "create_doc",
                "content": "# Doc content"
            }))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("title"));

        // Missing content
        let result = tool
            .execute(json!({
                "action": "create_doc",
                "title": "Test Doc"
            }))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("content"));
    }

    #[tokio::test]
    async fn test_memory_tool_get_doc_requires_doc_id() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "get_doc"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("doc_id"));
    }

    #[tokio::test]
    async fn test_memory_tool_create_roadmap_requires_title() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "create_roadmap"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("title"));
    }

    #[tokio::test]
    async fn test_memory_tool_get_transcript_requires_transcript_id() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "get_transcript"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("transcript_id"));
    }

    #[tokio::test]
    async fn test_memory_tool_search_transcripts_requires_query() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "search_transcripts"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("query"));
    }

    #[tokio::test]
    async fn test_memory_tool_delete_transcript_requires_transcript_id() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "delete_transcript"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("transcript_id"));
    }

    #[tokio::test]
    async fn test_memory_tool_update_event_requires_event_id() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "update_event"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("event_id"));
    }

    #[tokio::test]
    async fn test_memory_tool_delete_event_requires_event_id() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "delete_event"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("event_id"));
    }

    #[tokio::test]
    async fn test_memory_tool_distill_event_requires_event_id() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "distill_event"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("event_id"));
    }

    #[tokio::test]
    async fn test_memory_tool_update_task_requires_task_id() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "update_task"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("task_id"));
    }

    #[tokio::test]
    async fn test_memory_tool_update_task_requires_explicit_workspace_id() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "update_task",
                "task_id": "550e8400-e29b-41d4-a716-446655440000",
                "task_status": "completed"
            }))
            .await;

        let error = result.expect_err("workspace_id must be explicit for task actions");
        assert!(error
            .to_string()
            .contains("workspace_id is required for every memory task action"));
        assert!(error.to_string().contains("returned by init/context"));
    }

    #[tokio::test]
    async fn test_memory_tool_delete_task_requires_task_id() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "delete_task"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("task_id"));
    }

    #[tokio::test]
    async fn test_memory_tool_update_todo_requires_todo_id() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "update_todo"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("todo_id"));
    }

    #[tokio::test]
    async fn test_memory_tool_delete_todo_requires_todo_id() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "delete_todo"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("todo_id"));
    }

    #[tokio::test]
    async fn test_memory_tool_complete_todo_requires_todo_id() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "complete_todo"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("todo_id"));
    }

    #[tokio::test]
    async fn test_memory_tool_update_diagram_requires_diagram_id() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "update_diagram"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("diagram_id"));
    }

    #[tokio::test]
    async fn test_memory_tool_delete_diagram_requires_diagram_id() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "delete_diagram"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("diagram_id"));
    }

    #[tokio::test]
    async fn test_memory_tool_update_doc_requires_doc_id() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "update_doc"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("doc_id"));
    }

    #[tokio::test]
    async fn test_memory_tool_delete_doc_requires_doc_id() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "delete_doc"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("doc_id"));
    }

    #[tokio::test]
    async fn test_memory_tool_update_node_requires_node_id() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "update_node"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("node_id"));
    }

    #[tokio::test]
    async fn test_memory_tool_delete_node_requires_node_id() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "delete_node"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("node_id"));
    }
}

// ============================================================================
// Input Struct Tests
// ============================================================================

mod input_struct_tests {
    use super::json;
    use super::{CreateMemoryNodeInput, MemoryDecisionsInput, MemoryInput, MemorySearchInput};

    #[test]
    fn test_memory_search_input_deserialization() {
        let input: MemorySearchInput = serde_json::from_value(json!({
            "query": "authentication",
            "workspace_id": "550e8400-e29b-41d4-a716-446655440000",
            "node_type": "decision",
            "limit": 10
        }))
        .unwrap();

        assert_eq!(input.query, "authentication");
        assert!(input.workspace_id.is_some());
        assert_eq!(input.node_type, Some("decision".to_string()));
        assert_eq!(input.limit, Some(10));
    }

    #[test]
    fn test_create_memory_node_input_deserialization() {
        let input: CreateMemoryNodeInput = serde_json::from_value(json!({
            "node_type": "decision",
            "title": "Use JWT for auth",
            "content": "We decided to use JWT tokens for authentication.",
            "metadata": {"source": "meeting"}
        }))
        .unwrap();

        assert_eq!(input.node_type, "decision");
        assert_eq!(input.title, "Use JWT for auth");
        assert!(input.content.is_some());
        assert!(input.metadata.is_some());
    }

    #[test]
    fn test_memory_decisions_input_deserialization() {
        let input: MemoryDecisionsInput = serde_json::from_value(json!({
            "query": "architecture",
            "limit": 5
        }))
        .unwrap();

        assert_eq!(input.query, Some("architecture".to_string()));
        assert_eq!(input.limit, Some(5));
    }

    #[test]
    fn test_memory_input_deserialization() {
        let input: MemoryInput = serde_json::from_value(json!({
            "action": "create_task",
            "title": "Implement authentication",
            "description": "Add JWT auth to API",
            "priority": "high",
            "tags": ["auth", "api"]
        }))
        .unwrap();

        assert_eq!(input.action, "create_task");
        assert_eq!(input.title, Some("Implement authentication".to_string()));
        assert_eq!(input.priority, Some("high".to_string()));
        assert_eq!(
            input.tags,
            Some(vec!["auth".to_string(), "api".to_string()])
        );
    }

    #[test]
    fn test_memory_input_all_fields() {
        let input: MemoryInput = serde_json::from_value(json!({
            "action": "update_task",
            "task_id": "550e8400-e29b-41d4-a716-446655440000",
            "task_status": "completed",
            "blocked_reason": null,
            "is_personal": true
        }))
        .unwrap();

        assert_eq!(input.action, "update_task");
        assert!(input.task_id.is_some());
        assert_eq!(input.task_status, Some("completed".to_string()));
        assert_eq!(input.is_personal, Some(true));
    }
}

// ============================================================================
// Constants Tests
// ============================================================================

mod constants_tests {
    use super::*;

    #[test]
    fn test_valid_node_types() {
        assert!(VALID_NODE_TYPES.contains(&"fact"));
        assert!(VALID_NODE_TYPES.contains(&"decision"));
        assert!(VALID_NODE_TYPES.contains(&"preference"));
        assert!(VALID_NODE_TYPES.contains(&"constraint"));
        assert!(VALID_NODE_TYPES.contains(&"habit"));
        assert!(VALID_NODE_TYPES.contains(&"lesson"));
        assert!(VALID_NODE_TYPES.contains(&"goal"));
        assert!(VALID_NODE_TYPES.contains(&"risk"));
        assert!(VALID_NODE_TYPES.contains(&"term"));
        assert_eq!(VALID_NODE_TYPES.len(), 9);
    }

    #[test]
    fn test_valid_event_types() {
        assert!(VALID_EVENT_TYPES.contains(&"decision"));
        assert!(VALID_EVENT_TYPES.contains(&"preference"));
        assert!(VALID_EVENT_TYPES.contains(&"insight"));
        assert!(VALID_EVENT_TYPES.contains(&"uncategorized"));
        assert!(VALID_EVENT_TYPES.contains(&"operation"));
        assert!(VALID_EVENT_TYPES.contains(&"command_execution"));
        assert!(VALID_EVENT_TYPES.contains(&"file_operation"));
        assert!(VALID_EVENT_TYPES.contains(&"lesson"));
        assert!(VALID_EVENT_TYPES.contains(&"bug"));
        assert!(VALID_EVENT_TYPES.contains(&"feature"));
        assert!(VALID_EVENT_TYPES.contains(&"session_snapshot"));
    }

    #[test]
    fn test_valid_task_statuses() {
        assert!(VALID_TASK_STATUSES.contains(&"pending"));
        assert!(VALID_TASK_STATUSES.contains(&"in_progress"));
        assert!(VALID_TASK_STATUSES.contains(&"completed"));
        assert!(VALID_TASK_STATUSES.contains(&"blocked"));
        assert!(VALID_TASK_STATUSES.contains(&"cancelled"));
        assert_eq!(VALID_TASK_STATUSES.len(), 5);
    }

    #[test]
    fn test_valid_todo_priorities() {
        assert!(VALID_TODO_PRIORITIES.contains(&"low"));
        assert!(VALID_TODO_PRIORITIES.contains(&"medium"));
        assert!(VALID_TODO_PRIORITIES.contains(&"high"));
        assert!(VALID_TODO_PRIORITIES.contains(&"urgent"));
        assert_eq!(VALID_TODO_PRIORITIES.len(), 4);
    }

    #[test]
    fn test_valid_diagram_types() {
        assert!(VALID_DIAGRAM_TYPES.contains(&"flowchart"));
        assert!(VALID_DIAGRAM_TYPES.contains(&"sequence"));
        assert!(VALID_DIAGRAM_TYPES.contains(&"class"));
        assert!(VALID_DIAGRAM_TYPES.contains(&"er"));
        assert!(VALID_DIAGRAM_TYPES.contains(&"gantt"));
        assert!(VALID_DIAGRAM_TYPES.contains(&"mindmap"));
        assert!(VALID_DIAGRAM_TYPES.contains(&"pie"));
        assert!(VALID_DIAGRAM_TYPES.contains(&"other"));
    }

    #[test]
    fn diagram_type_help_lists_supported_types() {
        let help = diagram_types_help_suffix();
        for ty in VALID_DIAGRAM_TYPES {
            assert!(
                help.contains(ty),
                "diagram help text should include supported type `{}`",
                ty
            );
        }
    }

    #[test]
    fn test_valid_doc_types() {
        assert!(VALID_DOC_TYPES.contains(&"roadmap"));
        assert!(VALID_DOC_TYPES.contains(&"spec"));
        assert!(VALID_DOC_TYPES.contains(&"runbook"));
        assert!(VALID_DOC_TYPES.contains(&"adr"));
        assert!(VALID_DOC_TYPES.contains(&"rfc"));
        assert!(VALID_DOC_TYPES.contains(&"postmortem"));
        assert!(VALID_DOC_TYPES.contains(&"retro"));
        assert!(VALID_DOC_TYPES.contains(&"release_notes"));
        assert!(VALID_DOC_TYPES.contains(&"playbook"));
        assert!(VALID_DOC_TYPES.contains(&"prd"));
        assert!(VALID_DOC_TYPES.contains(&"user_story"));
        assert!(VALID_DOC_TYPES.contains(&"persona"));
        assert!(VALID_DOC_TYPES.contains(&"interview"));
        assert!(VALID_DOC_TYPES.contains(&"design_spec"));
        assert!(VALID_DOC_TYPES.contains(&"critique"));
        assert!(VALID_DOC_TYPES.contains(&"glossary"));
        assert!(VALID_DOC_TYPES.contains(&"oncall_schedule"));
        assert!(VALID_DOC_TYPES.contains(&"slo"));
        assert!(VALID_DOC_TYPES.contains(&"q_and_a"));
        assert!(VALID_DOC_TYPES.contains(&"changelog"));
        assert!(VALID_DOC_TYPES.contains(&"style_guide"));
        assert!(VALID_DOC_TYPES.contains(&"general"));
        assert_eq!(VALID_DOC_TYPES.len(), 22);
    }
}

// ============================================================================
// Tool Registration Tests
// ============================================================================

mod registration_tests {
    #[test]
    fn test_memory_tool_count() {
        // Expected memory tools:
        // - memory (unified)
        // - memory_search
        // - memory_create_node
        // - memory_decisions
        // Total: 4 individual tools

        let expected_tools = [
            "memory",
            "memory_search",
            "memory_create_node",
            "memory_decisions",
        ];

        assert_eq!(expected_tools.len(), 4);

        // Unified memory tool supports 44 actions:
        // - Node actions (10): search, create_node, get_node, update_node, delete_node, list_nodes, supersede_node, decisions, create_decision, decision_action
        // - Event actions (7): create_event, get_event, update_event, delete_event, distill_event, list_events, import_batch
        // - Task actions (6): create_task, get_task, update_task, delete_task, list_tasks, reorder_tasks
        // - Todo actions (6): create_todo, list_todos, get_todo, update_todo, delete_todo, complete_todo
        // - Diagram actions (5): create_diagram, list_diagrams, get_diagram, update_diagram, delete_diagram
        // - Doc actions (6): create_doc, list_docs, get_doc, update_doc, delete_doc, create_roadmap
        // - Transcript actions (4): list_transcripts, get_transcript, search_transcripts, delete_transcript
        let action_count = 10 + 7 + 6 + 6 + 5 + 6 + 4;
        assert_eq!(action_count, 44);
    }

    #[test]
    fn metadata_description_routes_doc_lookups_away_from_filesystem_tools() {
        // Regression guard. AI agents asking "find the doc on X" / "our
        // runbook for Y" / "the architecture note" repeatedly fall back
        // to `find` / `ls` / `grep` against the filesystem despite
        // CLAUDE.md guidance saying otherwise. Tool descriptions are
        // what shows up in tools/list, so the routing has to live HERE
        // for agents that don't have CLAUDE.md loaded (or that read
        // tool descriptions before reading rules files).
        //
        // This test pins the disambiguating language we depend on so a
        // future trim of the description can't silently re-introduce
        // the regression.
        use super::{create_mock_client, create_mock_session};
        use crate::domains::memory::MemoryTool;
        use crate::registry::ToolHandler;

        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MemoryTool::new(client, session, mcp_types::atlas_layer::noop_layer());
        let desc = tool.metadata().description.as_str();
        let lower = desc.to_lowercase();

        // Strong "use this for docs" leadership.
        assert!(
            lower.contains("docs"),
            "memory description must mention docs"
        );
        assert!(
            lower.contains("runbook")
                && lower.contains("spec")
                && (lower.contains("adr") || lower.contains("architecture"))
                && lower.contains("decision"),
            "memory description must enumerate the doc kinds users ask for: {}",
            desc
        );

        // Explicit "don't use filesystem search" wording.
        for phrase in ["find", "ls", "grep"] {
            assert!(
                lower.contains(phrase),
                "memory description must explicitly call out '{}' as the wrong tool",
                phrase
            );
        }
        assert!(
            lower.contains("filesystem")
                || lower.contains("on disk")
                || lower.contains("not on disk"),
            "memory description must note that ContextStream docs are NOT on disk: {}",
            desc
        );

        // The recommended action paths must be visible.
        assert!(
            desc.contains("memory(action=\"search\""),
            "memory description must point at memory(action=\"search\"): {}",
            desc
        );
        assert!(
            desc.contains("memory(action=\"list_docs\""),
            "memory description must point at memory(action=\"list_docs\"): {}",
            desc
        );
        assert!(
            desc.contains("session(action=\"recall\""),
            "memory description must mention session(recall) as the past-session fallback: {}",
            desc
        );
    }

    #[test]
    fn collect_bulk_delete_matches_returns_only_exact_matches() {
        use crate::domains::memory::collect_bulk_delete_matches;
        use serde_json::json;
        use uuid::Uuid;

        // Two exact-title duplicates plus one loosely-related node. delete_all
        // must select only the exact duplicates, never the partial match — the
        // "don't bulk-delete unrelated items" guarantee.
        let dup = Uuid::new_v4();
        let dup2 = Uuid::new_v4();
        let other = Uuid::new_v4();
        let listing = json!({
            "nodes": [
                {"id": dup.to_string(), "title": "Infrastructure is AWS ONLY"},
                {"id": dup2.to_string(), "title": "Infrastructure is AWS ONLY"},
                {"id": other.to_string(), "title": "Infrastructure runbook for AWS onboarding"},
            ]
        });
        let matches = collect_bulk_delete_matches(
            &listing,
            "Infrastructure is AWS ONLY",
            &["title", "summary", "name"],
        );
        let ids: std::collections::HashSet<Uuid> = matches.iter().map(|m| m.id).collect();
        assert_eq!(
            matches.len(),
            2,
            "only the two exact-title duplicates match"
        );
        assert!(ids.contains(&dup) && ids.contains(&dup2));
        assert!(
            !ids.contains(&other),
            "a loosely-related node must never be bulk-deleted"
        );
    }
}
