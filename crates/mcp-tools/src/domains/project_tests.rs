//! Tests for project domain tools.

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

fn create_mock_session(client: &ContextStreamClient) -> Arc<mcp_session::SessionManager> {
    Arc::new(mcp_session::SessionManager::new(
        client.clone(),
        TestFixtures::test_config(),
    ))
}

async fn project_tool_with_http_sequence(
    responses: Vec<(&'static str, String)>,
) -> (ProjectTool, tokio::task::JoinHandle<Vec<String>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind project test listener");
    let address = listener.local_addr().expect("project test address");
    let server = tokio::spawn(async move {
        let mut requests = Vec::with_capacity(responses.len());
        for (status, body) in responses {
            let (mut socket, _) = listener.accept().await.expect("accept project request");
            let mut buffer = vec![0_u8; 1024 * 1024];
            let read = socket
                .read(&mut buffer)
                .await
                .expect("read project request");
            requests.push(String::from_utf8_lossy(&buffer[..read]).to_string());
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write project response");
        }
        requests
    });

    let config = mcp_types::Config {
        api_url: format!("http://{address}"),
        api_key: Some("test-key".to_string()),
        is_http_transport: true,
        ..Default::default()
    };
    let client = ContextStreamClient::new(config.clone());
    let session = Arc::new(mcp_session::SessionManager::new(client.clone(), config));
    (ProjectTool::new(client, session), server)
}

fn tool_result_text(result: &mcp_types::tool::ToolResult) -> &str {
    match result.content.first() {
        Some(mcp_types::tool::ContentItem::Text { text }) => text,
        _ => "",
    }
}

#[tokio::test]
async fn hosted_folder_index_requests_the_installed_bridge_before_reporting_status() {
    let project_id = uuid::Uuid::new_v4();
    let workspace_id = uuid::Uuid::new_v4();
    let installation_id = uuid::Uuid::new_v4();
    let (tool, server) = project_tool_with_http_sequence(vec![
        (
            "202 Accepted",
            json!({"status": "requested", "request_id": uuid::Uuid::new_v4()}).to_string(),
        ),
        (
            "200 OK",
            json!({"indexed": true, "indexed_files": 3688}).to_string(),
        ),
    ])
    .await;

    let result = mcp_client::run_with_installation_id(installation_id, || async {
        tool.remote_index_state_result(
            "index",
            project_id,
            Some("/Users/alice/projects/example-worktree"),
            Some(workspace_id),
            true,
        )
        .await
    })
    .await
    .expect("hosted refresh result");
    let text = tool_result_text(&result);
    assert!(text.contains("Hosted refresh requested"));
    assert!(text.contains("sync bridge"));
    assert!(!text.contains("local MCP process"));

    let requests = server.await.expect("project request server");
    assert_eq!(requests.len(), 2);
    assert!(requests[0].contains(&format!(
        "POST /api/v1/projects/{project_id}/refresh-requests "
    )));
    assert!(requests[1].contains(&format!(
        "GET /api/v1/projects/{project_id}/index/status?installation_id={installation_id}&checkout_locator=checkout-locator-v1%3A"
    )));
    let body: serde_json::Value = serde_json::from_str(
        requests[0]
            .split_once("\r\n\r\n")
            .map(|(_, body)| body)
            .expect("refresh body"),
    )
    .expect("refresh JSON");
    assert_eq!(
        body["expected_scope"]["workspace_id"],
        workspace_id.to_string()
    );
    assert_eq!(body["installation_id"], installation_id.to_string());
    assert!(body["expected_scope"]["checkout_locator"]
        .as_str()
        .is_some_and(|value| value.starts_with("checkout-locator-v1:")));
    assert_eq!(body["force"], true);
    assert_eq!(body["reason"], "project.index");
    assert!(body.get("folder_path").is_none());
    assert!(!requests[0].contains("/Users/alice"));
    assert!(!requests[1].contains("/Users/alice"));
}

#[tokio::test]
async fn legacy_hosted_api_falls_back_without_recommending_local_mcp() {
    let project_id = uuid::Uuid::new_v4();
    let workspace_id = uuid::Uuid::new_v4();
    let installation_id = uuid::Uuid::new_v4();
    let (tool, server) = project_tool_with_http_sequence(vec![
        ("404 Not Found", json!({"message": "not found"}).to_string()),
        (
            "200 OK",
            json!({"indexed": true, "indexed_files": 12}).to_string(),
        ),
    ])
    .await;

    let result = mcp_client::run_with_installation_id(installation_id, || async {
        tool.remote_index_state_result(
            "index",
            project_id,
            Some("/Users/alice/projects/example-project"),
            Some(workspace_id),
            false,
        )
        .await
    })
    .await
    .expect("legacy fallback");
    let text = tool_result_text(&result);
    assert!(text.contains("project-wide index state"));
    assert!(text.contains("active-checkout freshness remains unconfirmed"));
    assert!(text.contains("sync bridge"));
    assert!(!text.contains("local MCP process"));
    assert_eq!(server.await.expect("legacy request server").len(), 2);
}

#[tokio::test]
async fn index_status_preserves_canonical_readiness_when_checkout_is_unconfirmed() {
    let project_id = uuid::Uuid::new_v4();
    let installation_id = uuid::Uuid::new_v4();
    let last_updated = chrono::Utc::now().to_rfc3339();
    let (tool, server) = project_tool_with_http_sequence(vec![(
        "200 OK",
        json!({
            "indexed": false,
            "indexed_file_count": 886,
            "project_index_state": "ready",
            "last_updated": last_updated
        })
        .to_string(),
    )])
    .await;

    let result = mcp_client::run_with_installation_id(installation_id, || async {
        tool.execute(json!({
            "action": "index_status",
            "project_id": project_id,
            "path": "/Users/alice/projects/example-code"
        }))
        .await
    })
    .await
    .expect("checkout-scoped index status");

    let text = tool_result_text(&result);
    assert!(text.contains("Project index is ready"));
    assert!(text.contains("canonical state"));
    assert!(text.contains("did not confirm this exact checkout overlay"));
    assert!(text.contains("not a missing-index condition"));
    assert!(!text.contains("Project index not found"));
    assert!(!text.contains("local MCP"));

    let structured = result
        .structured_content
        .as_ref()
        .expect("structured project status");
    assert_eq!(structured["indexed"], true);
    assert_eq!(structured["canonical_index_ready"], true);
    assert_eq!(structured["checkout_scope_confirmed"], false);
    assert_eq!(structured["checkout_scope_status"], "unconfirmed");
    assert_eq!(structured["index_freshness"], "fresh");
    assert_eq!(
        structured["mcp_checkout_scope"]["requested"],
        serde_json::Value::Bool(true)
    );
    assert_eq!(
        structured["mcp_checkout_scope"]["matched"],
        serde_json::Value::Bool(false)
    );

    let requests = server.await.expect("checkout status request server");
    assert_eq!(requests.len(), 1);
    assert!(requests[0].contains(&format!(
        "GET /api/v1/projects/{project_id}/index/status?installation_id={installation_id}&checkout_locator=checkout-locator-v1%3A"
    )));
    assert!(!requests[0].contains("/Users/alice"));
}

mod helper_tests {
    use super::super::{folder_name_from_path, project_repository_identity};
    use super::{
        api_index_result_is_skipped, api_result_is_indexing, api_result_reports_canonical_indexed,
        api_result_reports_indexed, classify_index_confidence, classify_index_freshness,
        classify_remote_index_disposition, classify_remote_refresh_disposition,
        extract_index_timestamp, folder_paths_equivalent, folder_scope_mismatches_explicit_project,
        format_index_freshness_text, format_project_files_text, index_history_entry_count,
        missing_project_scope_error, project_files_page_count, reconcile_ingest_project_id,
        refresh_endpoint_is_unsupported, remote_index_file_count, requires_ingest_endpoint,
        requires_sync_bridge_message, resolve_project_tool_folder_path, run_git_log,
        server_index_ready_message, server_indexing_in_progress_message, validate_ingest_directory,
        RemoteIndexDisposition, RemoteRefreshDisposition,
    };
    use mcp_types::{api::Project, Error};
    use serde_json::json;
    use std::process::Command;
    use tempfile::tempdir;

    fn project_with_repository(repository_url: Option<&str>) -> Project {
        Project {
            id: uuid::Uuid::new_v4(),
            name: "mcp".to_string(),
            description: None,
            repository_url: repository_url.map(str::to_string),
            repository_type: None,
            workspace_id: Some(uuid::Uuid::new_v4()),
            path: None,
            created_at: None,
            updated_at: None,
            indexed_at: None,
            file_count: None,
        }
    }

    #[test]
    fn project_repository_identity_is_transport_and_credential_independent() {
        let https = project_with_repository(Some(
            "https://token@GitHub.com/Acme/Team/Mcp.git?secret=hidden",
        ));
        let ssh = project_with_repository(Some("git@github.com:Acme/Team/Mcp.git"));
        assert_eq!(
            project_repository_identity(&https),
            project_repository_identity(&ssh)
        );
        assert_eq!(
            project_repository_identity(&https)
                .expect("repository identity")
                .as_str(),
            "git-remote-v1:github.com/Acme/Team/Mcp"
        );
        assert!(project_repository_identity(&project_with_repository(Some("../mcp"))).is_none());
    }
    use uuid::Uuid;

    #[test]
    fn folder_paths_equivalent_matches_exact_and_canonical() {
        let dir = tempdir().expect("tempdir");
        let p = dir.path().to_str().expect("utf8");
        // Exact match.
        assert!(folder_paths_equivalent(p, p));
        // Trailing slash canonicalizes to the same directory.
        assert!(folder_paths_equivalent(p, &format!("{p}/")));
        // Two different existing dirs do not match.
        let other = tempdir().expect("tempdir2");
        assert!(!folder_paths_equivalent(
            p,
            other.path().to_str().expect("utf8")
        ));
        // Two DIFFERENT non-existent paths must NOT match (no false positive
        // from canonicalize failing on both).
        assert!(!folder_paths_equivalent("/no/such/aaa", "/no/such/bbb"));
        // Identical non-existent paths still match via the exact-string check.
        assert!(folder_paths_equivalent("/no/such/aaa", "/no/such/aaa"));
    }

    #[test]
    fn detects_ingest_endpoint_requirement_error() {
        let err = Error::http(
            400,
            "Bad request: This project type requires using the ingest endpoint. For GitHub projects, use the Index button.",
        );
        assert!(requires_ingest_endpoint(&err));
    }

    #[test]
    fn ignores_other_http_errors() {
        let err = Error::http(404, "Not found");
        assert!(!requires_ingest_endpoint(&err));
    }

    #[test]
    fn validate_ingest_directory_allows_missing_path_only_for_http_transport() {
        let temp = tempdir().expect("tempdir");
        let missing = temp.path().join("missing");

        assert!(validate_ingest_directory(missing.to_str().unwrap_or_default(), true).is_ok());
        assert!(validate_ingest_directory(missing.to_str().unwrap_or_default(), false).is_err());
    }

    #[test]
    fn validate_ingest_directory_rejects_existing_file_even_for_http_transport() {
        let temp = tempdir().expect("tempdir");
        let file_path = temp.path().join("file.txt");
        std::fs::write(&file_path, "hello").expect("write file");

        assert!(validate_ingest_directory(file_path.to_str().unwrap_or_default(), true).is_err());
    }

    #[test]
    fn requires_sync_bridge_guidance_keeps_the_editor_on_hosted_mcp() {
        let message = requires_sync_bridge_message(Some(r"D:\repo\agentic-smith"));

        assert!(message.contains("No exact-checkout content upload has completed"));
        assert!(message.contains(r"D:\repo\agentic-smith"));
        assert!(message.contains("Add local repository"));
        assert!(message.contains("sync bridge"));
        assert!(message.contains("contextstream-mcp doctor --repair"));
        assert!(message.contains("hosted MCP"));
        assert!(!message.contains("local MCP process"));
        assert!(!message.contains("cannot read"));
        assert!(message.contains("project(action=\"index\")"));
        assert!(message.contains("index_status"));
        assert!(!message.contains("updating index in background"));

        // Without a folder hint the guidance must still be complete.
        let no_path = requires_sync_bridge_message(None);
        assert!(no_path.contains("No exact-checkout content upload has completed"));
        assert!(no_path.contains("Add local repository"));
    }

    #[test]
    fn remote_disposition_prefers_ready_over_indexing_over_empty() {
        let ready = json!({"indexed": true, "indexed_files": 42});
        let ready_while_refreshing = json!({"indexed": true, "status": "indexing"});
        let first_index_running = json!({"indexed": false, "status": "indexing"});
        let empty = json!({"indexed": false, "total_files": 0});

        assert_eq!(
            classify_remote_index_disposition(&ready),
            RemoteIndexDisposition::Ready
        );
        assert_eq!(
            classify_remote_index_disposition(&ready_while_refreshing),
            RemoteIndexDisposition::Ready
        );
        assert_eq!(
            classify_remote_index_disposition(&first_index_running),
            RemoteIndexDisposition::Indexing
        );
        assert_eq!(
            classify_remote_index_disposition(&empty),
            RemoteIndexDisposition::RequiresSyncBridge
        );
        // A missing/404 status payload must fall through to guidance, never panic.
        assert_eq!(
            classify_remote_index_disposition(&serde_json::Value::Null),
            RemoteIndexDisposition::RequiresSyncBridge
        );
    }

    #[test]
    fn refresh_receipts_cover_bridge_lifecycle_states() {
        assert_eq!(
            classify_remote_refresh_disposition(&json!({"status": "requested"})),
            RemoteRefreshDisposition::Requested
        );
        assert_eq!(
            classify_remote_refresh_disposition(&json!({"state": "claimed"})),
            RemoteRefreshDisposition::Pending
        );
        assert_eq!(
            classify_remote_refresh_disposition(&json!({"status": "committed"})),
            RemoteRefreshDisposition::Completed
        );
        assert_eq!(
            classify_remote_refresh_disposition(&json!({"status": "bridge_offline"})),
            RemoteRefreshDisposition::BridgeOffline
        );
        assert_eq!(
            classify_remote_refresh_disposition(&json!({"status": "no_checkout"})),
            RemoteRefreshDisposition::NoCheckout
        );
    }

    #[test]
    fn only_absent_refresh_routes_are_treated_as_legacy_servers() {
        for status in [404, 405, 501] {
            assert!(refresh_endpoint_is_unsupported(&Error::http(
                status, "missing"
            )));
        }
        assert!(!refresh_endpoint_is_unsupported(&Error::http(
            500,
            "bridge service failed"
        )));
    }

    #[test]
    fn remote_index_messages_explain_managed_hosted_ingestion_without_local_mcp_fallback() {
        let project = Uuid::new_v4();

        let ready = server_index_ready_message(project, Some(1234), Some("/home/user/repo"));
        assert!(ready.contains("1234 files"));
        assert!(ready.contains("/home/user/repo"));
        assert!(ready.contains("Hosted MCP intentionally delegates"));
        assert!(ready.contains("exact-checkout ContextStream sync bridge"));
        assert!(!ready.contains("cannot read"));
        assert!(!ready.contains("local MCP process"));
        assert!(ready.contains("index_status"));

        let in_progress = server_indexing_in_progress_message(project, None);
        assert!(in_progress.contains("in progress"));
        assert!(in_progress.contains("Hosted MCP intentionally delegates"));
        assert!(!in_progress.contains("cannot read"));
    }

    #[test]
    fn skipped_index_ack_is_detected_case_insensitively() {
        assert!(api_index_result_is_skipped(&json!({"status": "skipped"})));
        assert!(api_index_result_is_skipped(&json!({"status": "Skipped"})));
        assert!(!api_index_result_is_skipped(&json!({"status": "started"})));
        assert!(!api_index_result_is_skipped(&json!({})));
    }

    #[test]
    fn remote_index_file_count_reads_known_fields() {
        assert_eq!(
            remote_index_file_count(&json!({"indexed_files": 7})),
            Some(7)
        );
        assert_eq!(
            remote_index_file_count(&json!({"indexed_file_count": 9})),
            Some(9)
        );
        assert_eq!(
            remote_index_file_count(&json!({"total_files": 11})),
            Some(11)
        );
        assert_eq!(remote_index_file_count(&json!({"total_files": 0})), None);
        assert_eq!(remote_index_file_count(&json!({})), None);
    }

    #[test]
    fn ingest_project_id_accepts_consensus_across_all_scope_sources() {
        let explicit_project_id = Uuid::new_v4();

        let resolved = reconcile_ingest_project_id(
            "ingest_local",
            "/work/mcp",
            Some(explicit_project_id),
            Some(explicit_project_id),
            Some(explicit_project_id),
            Some(explicit_project_id),
        )
        .expect("consensus should resolve")
        .expect("resolved project");

        assert_eq!(resolved.project_id, explicit_project_id);
        assert_eq!(resolved.source, "explicit_project_id");
    }

    #[test]
    fn ingest_project_id_rejects_session_and_local_index_disagreement() {
        let session_project_id = Uuid::new_v4();
        let local_index_project_id = Uuid::new_v4();

        let error = reconcile_ingest_project_id(
            "index",
            "/work/mcp",
            None,
            Some(session_project_id),
            None,
            Some(local_index_project_id),
        )
        .expect_err("conflicting session and local index must fail closed");

        let text = error.to_string();
        assert!(text.contains("project scope sources disagree"));
        assert!(text.contains(&format!("session_project_id={session_project_id}")));
        assert!(text.contains(&format!("local_index_metadata={local_index_project_id}")));
        assert!(text.contains("No local ingest was started"));
    }

    #[test]
    fn ingest_project_id_rejects_folder_and_session_mapping_vs_local_index_disagreement() {
        let folder_project_id = Uuid::new_v4();
        let local_index_project_id = Uuid::new_v4();

        let error = reconcile_ingest_project_id(
            "ingest_local",
            "/work/mcp",
            None,
            Some(folder_project_id),
            Some(folder_project_id),
            Some(local_index_project_id),
        )
        .expect_err("folder/session consensus must not override local index disagreement");

        let text = error.to_string();
        assert!(text.contains(&format!("session_project_id={folder_project_id}")));
        assert!(text.contains(&format!("folder_mapping={folder_project_id}")));
        assert!(text.contains(&format!("local_index_metadata={local_index_project_id}")));
        assert!(text.contains("correct the stale folder mapping or local index metadata"));
    }

    #[test]
    fn missing_scope_error_reports_workspace_when_no_workspace_exists() {
        let err = missing_project_scope_error("ingest_local", None, Some("/tmp/example"));
        let text = err.to_string();

        assert!(text.contains("workspace_id is required"));
        assert!(text.contains("project_id is required"));
        assert!(text.contains("ingest_local"));
        assert!(text.contains("No active workspace scope"));
        assert!(text.contains("init(folder_path=\"/tmp/example\")"));
    }

    #[test]
    fn missing_scope_error_does_not_suggest_duplicate_project_creation() {
        let workspace_id = Uuid::new_v4();
        let err = missing_project_scope_error(
            "index_status",
            Some(workspace_id),
            Some("/tmp/contextstream\"quoted\nline"),
        );
        let text = err.to_string();

        assert!(text.contains("project_id is required for index_status"));
        assert!(text.contains("init(folder_path=\"/tmp/contextstream\\\"quoted\\nline\""));
        assert!(text.contains("project(action=\"index\""));
        assert!(!text.contains("project(action=\"index\", path="));
        assert!(!text.contains("project(action=\"ingest_local\""));
        assert!(
            !text.contains("project(action=\"create\""),
            "missing scope recovery should not encourage duplicate project creation"
        );
    }

    #[test]
    fn detects_folder_scope_mismatch_for_explicit_project() {
        let explicit_project_id = Uuid::new_v4();
        let other_project_id = Uuid::new_v4();

        assert!(folder_scope_mismatches_explicit_project(
            Some(explicit_project_id),
            Some(other_project_id),
            None,
        ));
        assert!(!folder_scope_mismatches_explicit_project(
            Some(explicit_project_id),
            Some(other_project_id),
            Some(explicit_project_id),
        ));
        assert!(!folder_scope_mismatches_explicit_project(
            Some(explicit_project_id),
            None,
            None,
        ));
    }

    #[test]
    fn explicit_project_id_keeps_session_folder_for_stale_scope_reconciliation() {
        assert_eq!(
            resolve_project_tool_folder_path(
                None,
                None,
                Some("/home/alice/projects/canonical".to_string()),
                true,
            ),
            Some("/home/alice/projects/canonical".to_string())
        );
    }

    #[test]
    fn explicit_project_id_preserves_explicit_folder_path() {
        assert_eq!(
            resolve_project_tool_folder_path(
                Some("/home/alice/projects/mcp".to_string()),
                None,
                Some("/home/alice/projects/canonical".to_string()),
                true,
            )
            .as_deref(),
            Some("/home/alice/projects/mcp")
        );
    }

    #[test]
    fn session_folder_used_without_explicit_project_id() {
        assert_eq!(
            resolve_project_tool_folder_path(
                None,
                None,
                Some("/home/alice/projects/mcp".to_string()),
                false,
            )
            .as_deref(),
            Some("/home/alice/projects/mcp")
        );
    }

    #[test]
    fn folder_name_from_path_handles_windows_style() {
        assert_eq!(
            folder_name_from_path(r"C:\Users\foo\contextstream"),
            "contextstream"
        );
    }

    #[test]
    fn folder_name_from_path_unix_trailing_slash() {
        assert_eq!(folder_name_from_path("/home/foo/bar/"), "bar");
    }

    #[test]
    fn folder_name_from_path_single_segment() {
        assert_eq!(folder_name_from_path("bar"), "bar");
    }

    #[test]
    fn folder_name_from_path_drive_root() {
        assert_eq!(folder_name_from_path(r"C:\"), "");
    }

    #[test]
    fn api_indexed_detects_indexed_files_contract_field() {
        let payload = json!({
            "status": "completed",
            "indexed_files": 12
        });
        assert!(api_result_reports_indexed(&payload));
    }

    #[test]
    fn api_indexed_detects_indexed_file_count_contract_field() {
        let payload = json!({
            "project_index_state": "ready",
            "indexed_file_count": 4
        });
        assert!(api_result_reports_indexed(&payload));
    }

    #[test]
    fn api_indexed_respects_explicit_false_flag() {
        let payload = json!({
            "indexed": false,
            "indexed_files": 99
        });
        assert!(!api_result_reports_indexed(&payload));
    }

    #[test]
    fn canonical_readiness_uses_committed_evidence_only_when_checkout_is_unconfirmed() {
        let payload = json!({
            "indexed": false,
            "indexed_file_count": 886,
            "project_index_state": "ready",
            "committed_generation": 6933
        });

        assert!(api_result_reports_canonical_indexed(&payload, true));
        assert!(!api_result_reports_canonical_indexed(&payload, false));
    }

    #[test]
    fn api_indexed_does_not_infer_from_indexing_status_alone() {
        let payload = json!({
            "status": "indexing",
            "total_files": 12,
            "indexed_files": 0
        });
        assert!(!api_result_reports_indexed(&payload));
    }

    #[test]
    fn api_indexing_detects_project_index_state() {
        let payload = json!({
            "project_index_state": "indexing",
            "pending_files": 1
        });
        assert!(api_result_is_indexing(&payload));
    }

    #[test]
    fn api_indexing_detects_status_processing() {
        let payload = json!({
            "status": "processing",
            "pending_files": 1
        });
        assert!(api_result_is_indexing(&payload));
    }

    #[test]
    fn api_ready_index_confidence_is_authoritative_without_local_cache() {
        let (confidence, reason) = classify_index_confidence(true, true, false, "aging");

        assert_eq!(confidence, "high");
        assert!(reason.contains("API reports index readiness"));
        assert!(!reason.contains("Only one source"));
    }

    #[test]
    fn aging_ready_index_text_is_not_scary() {
        let text = format_index_freshness_text("aging", Some(133), true);

        assert!(text.contains("Search is ready from the existing index"));
        assert!(text.contains("last confirmed ingest was 133h ago"));
        assert!(!text.contains("Freshness: aging"));
    }

    #[test]
    fn stale_ready_index_text_is_not_scary() {
        let text = format_index_freshness_text("stale", Some(140), true);

        assert!(text.contains("Search is ready from the existing index"));
        assert!(text.contains("last confirmed ingest was 140h ago"));
        assert!(!text.contains("Freshness: stale"));
        assert!(!text.contains("refresh is recommended"));
    }

    #[test]
    fn index_status_marks_older_than_48h_stale() {
        assert_eq!(classify_index_freshness(true, Some(48)), "aging");
        assert_eq!(classify_index_freshness(true, Some(49)), "stale");
    }

    #[test]
    fn index_status_prefers_committed_ingest_timestamp_over_status_update() {
        let committed = chrono::Utc::now() - chrono::Duration::hours(77);
        let status_update = chrono::Utc::now();
        let payload = json!({
            "status": "indexing",
            "ingested_at_max": committed.to_rfc3339(),
            "last_updated": status_update.to_rfc3339()
        });

        let indexed_at = extract_index_timestamp(&payload).expect("committed timestamp");
        assert!(indexed_at <= committed + chrono::Duration::seconds(1));
    }

    #[test]
    fn index_status_ignores_in_progress_status_update_as_freshness() {
        let payload = json!({
            "status": "indexing",
            "last_updated": chrono::Utc::now().to_rfc3339()
        });

        assert!(extract_index_timestamp(&payload).is_none());
    }

    #[test]
    fn history_count_reads_entries_array() {
        let payload = json!({
            "entries": [{ "file_path": "a.rs" }, { "file_path": "b.ts" }]
        });
        assert_eq!(index_history_entry_count(&payload), 2);
    }

    #[test]
    fn history_count_reads_legacy_array_shape() {
        let payload = json!([{ "event": 1 }, { "event": 2 }, { "event": 3 }]);
        assert_eq!(index_history_entry_count(&payload), 3);
    }

    #[test]
    fn project_files_page_count_reads_files_array() {
        let payload = json!({
            "files": [{"path": "a.rs"}, {"path": "b.rs"}],
            "total": 2743
        });
        assert_eq!(project_files_page_count(&payload), 2);
    }

    #[test]
    fn project_files_page_count_reads_paths_array() {
        let payload = json!({
            "paths": ["a.rs", "b.rs", "c.rs"],
            "total": 3
        });
        assert_eq!(project_files_page_count(&payload), 3);
    }

    #[test]
    fn format_project_files_text_includes_total_when_paginated() {
        let payload = json!({
            "files": [{"path": "a.rs"}],
            "total": 2743
        });
        assert_eq!(
            format_project_files_text(&payload),
            "Found 1 indexed files on this page (2743 total indexed)."
        );
    }

    #[test]
    fn format_project_files_text_keeps_legacy_single_count_when_total_matches() {
        let payload = json!({
            "files": [{"path": "a.rs"}, {"path": "b.rs"}],
            "total": 2
        });
        assert_eq!(
            format_project_files_text(&payload),
            "Found 2 indexed files."
        );
    }

    #[test]
    fn format_project_files_text_warns_when_the_checkout_is_unconfirmed() {
        let payload = json!({
            "files": [{"path": "a.rs"}],
            "total": 1,
            "mcp_checkout_scope": {
                "requested": true,
                "recognized": false,
                "matched": false,
            }
        });
        let text = format_project_files_text(&payload);
        assert!(text.contains("Found 1 indexed files."));
        assert!(text.contains("did not confirm this exact checkout"));
        assert!(text.contains("active worktree overlay"));
    }

    #[tokio::test]
    async fn run_git_log_parses_recent_commit_and_changed_files() {
        let temp = tempdir().expect("tempdir");
        let repo_path = temp.path();

        let status = Command::new("git")
            .args(["init"])
            .current_dir(repo_path)
            .status()
            .expect("git init");
        assert!(status.success());

        let status = Command::new("git")
            .args(["config", "user.name", "ContextStream Tests"])
            .current_dir(repo_path)
            .status()
            .expect("git config user.name");
        assert!(status.success());

        let status = Command::new("git")
            .args(["config", "user.email", "tests@example.com"])
            .current_dir(repo_path)
            .status()
            .expect("git config user.email");
        assert!(status.success());

        std::fs::write(repo_path.join("tracked.txt"), "hello\n").expect("write tracked file");

        let status = Command::new("git")
            .args(["add", "tracked.txt"])
            .current_dir(repo_path)
            .status()
            .expect("git add");
        assert!(status.success());

        let status = Command::new("git")
            .args(["commit", "-m", "Initial commit"])
            .current_dir(repo_path)
            .status()
            .expect("git commit");
        assert!(status.success());

        let commits = run_git_log(repo_path.to_str().unwrap_or_default(), 5, None)
            .await
            .expect("git log");

        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].message, "Initial commit");
        assert_eq!(commits[0].files_changed, vec!["tracked.txt".to_string()]);
        assert!(!commits[0].hash.is_empty());
    }
}

// ============================================================================
// Metadata Tests
// ============================================================================

mod metadata_tests {
    use super::{create_mock_client, create_mock_session, ToolCategory, ToolHandler};
    use super::{ProjectTool, ProjectsCreateTool, ProjectsIndexTool, ProjectsListTool};

    #[test]
    fn test_projects_list_tool_metadata() {
        let client = create_mock_client();
        let tool = ProjectsListTool::new(client);
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "projects_list");
        assert_eq!(metadata.title, "List Projects");
        assert!(metadata.description.contains("projects"));
        assert_eq!(metadata.category, ToolCategory::Project);
        assert!(!metadata.is_pro);
    }

    #[test]
    fn test_projects_create_tool_metadata() {
        let client = create_mock_client();
        let tool = ProjectsCreateTool::new(client);
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "projects_create");
        assert_eq!(metadata.title, "Create Project");
        assert_eq!(metadata.category, ToolCategory::Project);
    }

    #[test]
    fn test_projects_index_tool_metadata() {
        let client = create_mock_client();
        let tool = ProjectsIndexTool::new(client);
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "projects_index");
        assert_eq!(metadata.title, "Index Project");
        assert_eq!(metadata.category, ToolCategory::Project);
    }

    #[test]
    fn test_unified_project_tool_metadata() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = ProjectTool::new(client, session);
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "project");
        assert_eq!(metadata.title, "Project Operations");
        assert!(metadata.description.contains("index"));
        assert!(metadata.description.contains("overview"));
        assert!(metadata.description.contains("ingest_local"));
        assert_eq!(metadata.category, ToolCategory::Project);
    }
}

// ============================================================================
// Schema Tests
// ============================================================================

mod schema_tests {
    use super::{create_mock_client, create_mock_session, ToolHandler};
    use super::{ProjectTool, ProjectsCreateTool, ProjectsListTool};

    #[test]
    fn test_projects_list_schema() {
        let client = create_mock_client();
        let tool = ProjectsListTool::new(client);
        let schema = tool.input_schema();

        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("workspace_id"));
        assert!(props.contains_key("page"));
        assert!(props.contains_key("page_size"));
    }

    #[test]
    fn test_projects_create_schema() {
        let client = create_mock_client();
        let tool = ProjectsCreateTool::new(client);
        let schema = tool.input_schema();

        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("name"));
        assert!(props.contains_key("description"));
        assert!(props.contains_key("workspace_id"));

        // name should be required
        if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
            assert!(required.iter().any(|v| v.as_str() == Some("name")));
        }
    }

    #[test]
    fn test_unified_project_schema() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = ProjectTool::new(client, session);
        let schema = tool.input_schema();

        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("action"));

        // Check action enum
        if let Some(action) = props.get("action") {
            if let Some(enum_vals) = action.get("enum").and_then(|e| e.as_array()) {
                let values: Vec<&str> = enum_vals.iter().filter_map(|v| v.as_str()).collect();
                assert!(values.contains(&"list"));
                assert!(values.contains(&"get"));
                assert!(values.contains(&"create"));
                assert!(values.contains(&"update"));
                assert!(values.contains(&"merge"));
                assert!(values.contains(&"combine"));
                assert!(values.contains(&"index"));
                assert!(values.contains(&"overview"));
                assert!(values.contains(&"statistics"));
                assert!(values.contains(&"files"));
                assert!(values.contains(&"index_status"));
                assert!(values.contains(&"index_history"));
                assert!(values.contains(&"ingest_local"));
                assert!(values.contains(&"team_projects"));
            }
        }

        // Check other fields
        assert!(props.contains_key("project_id"));
        assert!(props.contains_key("source_project_id"));
        assert!(props.contains_key("path"));
        assert!(props.contains_key("force"));
        assert!(props.contains_key("sort_by"));
        assert!(props.contains_key("sort_order"));
    }
}

// ============================================================================
// Input Validation Tests
// ============================================================================

mod validation_tests {
    use super::{create_mock_client, create_mock_session, json, TestFixtures, ToolHandler};
    use super::{ProjectTool, ProjectsCreateTool, ProjectsIndexTool};
    use mcp_client::{run_with_auth_override, ContextStreamClient};
    use mcp_types::api::Project;
    use mcp_types::tool::ContentItem;
    use mcp_types::AuthOverride;
    use std::process::Command;
    use std::sync::Arc;
    use tempfile::tempdir;
    use uuid::Uuid;

    fn make_project_tool() -> ProjectTool {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        ProjectTool::new(client, session)
    }

    fn project_in_workspace(project_id: Uuid, workspace_id: Uuid) -> Project {
        Project {
            id: project_id,
            name: "mcp".to_string(),
            description: None,
            repository_url: None,
            repository_type: None,
            workspace_id: Some(workspace_id),
            path: None,
            created_at: None,
            updated_at: None,
            indexed_at: None,
            file_count: None,
        }
    }

    async fn client_with_fresh_project(
        project: Project,
    ) -> (
        ContextStreamClient,
        mcp_types::Config,
        tokio::task::JoinHandle<()>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fresh project listener");
        let addr = listener.local_addr().expect("fresh project listener addr");
        let expected_request = format!("GET /api/v1/projects/{} ", project.id);
        let project_body = serde_json::to_string(&project).expect("serialize project fixture");
        let server = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buffer = vec![0u8; 16 * 1024];
                let read = socket.read(&mut buffer).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]);
                let (status, body) = if request.starts_with(&expected_request) {
                    ("200 OK", project_body.as_str())
                } else {
                    (
                        "404 Not Found",
                        r#"{"error":"unexpected project fixture request"}"#,
                    )
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });

        let mut config = TestFixtures::test_config();
        config.api_url = format!("http://{addr}");
        config.default_workspace_id = project.workspace_id;
        (ContextStreamClient::new(config.clone()), config, server)
    }

    fn initialize_git_checkout(path: &std::path::Path) {
        let status = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(path)
            .status()
            .expect("git init for checkout binding fixture");
        assert!(status.success(), "git init must succeed for fixture");
    }

    async fn client_with_project_server_error() -> (
        ContextStreamClient,
        mcp_types::Config,
        tokio::task::JoinHandle<()>,
    ) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind project validation listener");
        let addr = listener
            .local_addr()
            .expect("project validation listener addr");
        let server = tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buffer = vec![0u8; 4096];
                let _ = socket.read(&mut buffer).await;
                let body = r#"{"error":"project validation unavailable"}"#;
                let response = format!(
                    "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
            }
        });

        let mut config = TestFixtures::test_config();
        config.api_url = format!("http://{addr}");
        (ContextStreamClient::new(config.clone()), config, server)
    }

    #[tokio::test]
    async fn test_projects_create_requires_name() {
        let client = create_mock_client();
        let tool = ProjectsCreateTool::new(client);

        let result = tool
            .execute(json!({
                "name": ""
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("name"));
    }

    #[tokio::test]
    async fn test_projects_index_requires_project_id() {
        let client = create_mock_client();
        let tool = ProjectsIndexTool::new(client);

        let result = tool.execute(json!({})).await;

        // Missing required field will cause deserialization error
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_project_tool_unknown_action() {
        let tool = make_project_tool();

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
    async fn test_project_tool_get_requires_project_id() {
        let tool = make_project_tool();

        let result = tool
            .execute(json!({
                "action": "get"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("project_id"));
    }

    #[tokio::test]
    async fn test_project_tool_create_requires_name() {
        let tool = make_project_tool();

        let result = tool
            .execute(json!({
                "action": "create"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("name"));
    }

    #[tokio::test]
    async fn test_project_tool_ingest_local_requires_path() {
        let tool = make_project_tool();

        let result = tool
            .execute(json!({
                "action": "ingest_local"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("path"));
    }

    #[tokio::test]
    async fn test_project_tool_ingest_local_skip_project_creation_errors_without_network() {
        let tool = make_project_tool();
        let tempdir = tempdir().expect("tempdir");
        let child = tempdir.path().join("myproj_leaf");
        std::fs::create_dir_all(&child).expect("mkdir");
        let path = child.to_string_lossy().to_string();
        let ws = Uuid::new_v4();

        let result = tool
            .execute(json!({
                "action": "ingest_local",
                "path": path,
                "workspace_id": ws.to_string(),
                "skip_project_creation": true
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("project_id is required"));
        // With skip_project_creation set and no resolvable project scope, the
        // The explicit direct-ingest request still fails closed, but recovery
        // returns to the hosted-safe init + unified index path.
        assert!(err.contains("init(folder_path="));
        assert!(err.contains("project(action=\"index\""));
        assert!(!err.contains("project(action=\"ingest_local\""));
        assert!(
            err.contains(&path),
            "error should echo the folder path, got: {err}"
        );
    }

    #[tokio::test]
    async fn test_project_tool_update_requires_project_id() {
        let tool = make_project_tool();

        let result = tool
            .execute(json!({
                "action": "update",
                "name": "New Name"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("project_id"));
    }

    #[tokio::test]
    async fn test_project_tool_index_requires_project_id() {
        let tool = make_project_tool();

        let result = tool
            .execute(json!({
                "action": "index"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("project_id"));
    }

    #[tokio::test]
    async fn test_project_tool_index_uses_session_project_id_when_omitted() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let project_id = Uuid::new_v4();
        session.initialize(None, Some(project_id), None, None).await;
        let tool = ProjectTool::new(client, session);

        let result = tool
            .execute(json!({
                "action": "index"
            }))
            .await;

        // Should move past validation and attempt API call.
        assert!(result.is_err());
        assert!(!result
            .unwrap_err()
            .to_string()
            .contains("project_id is required for index"));
    }

    #[tokio::test]
    async fn test_project_tool_index_uses_ingest_local_when_folder_context_exists() {
        let project_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let (client, config, server) =
            client_with_fresh_project(project_in_workspace(project_id, workspace_id)).await;
        let session = Arc::new(mcp_session::SessionManager::new(client.clone(), config));
        let tempdir = tempdir().expect("tempdir");
        session
            .initialize(
                Some(workspace_id),
                Some(project_id),
                Some(tempdir.path().to_string_lossy().to_string()),
                None,
            )
            .await;
        let tool = ProjectTool::new(client.clone(), session);

        let result = tool
            .execute(json!({
                "action": "index"
            }))
            .await;
        server.abort();
        let result = result.expect("index should start background ingest");

        assert!(!result.is_error);
        match result.content.first() {
            Some(ContentItem::Text { text }) => {
                assert!(text.contains("Updating index in background"));
            }
            other => panic!("expected text content, got {:?}", other),
        }
        let structured = result.structured_content.expect("structured content");
        assert_eq!(structured.get("invoked_action"), Some(&json!("index")));
        assert_eq!(
            structured.get("project_id"),
            Some(&json!(project_id.to_string()))
        );
    }

    #[tokio::test]
    async fn test_project_tool_index_prefers_initialized_session_scope_over_task_auth() {
        let session_project_id = Uuid::new_v4();
        let task_project_id = Uuid::new_v4();
        let session_workspace_id = Uuid::new_v4();
        let task_workspace_id = Uuid::new_v4();
        let (client, config, server) = client_with_fresh_project(project_in_workspace(
            session_project_id,
            session_workspace_id,
        ))
        .await;
        let session = Arc::new(mcp_session::SessionManager::new(client.clone(), config));
        let tempdir = tempdir().expect("tempdir");
        let session_folder = tempdir.path().to_string_lossy().to_string();
        session
            .initialize(
                Some(session_workspace_id),
                Some(session_project_id),
                Some(session_folder.clone()),
                None,
            )
            .await;
        let tool = ProjectTool::new(client.clone(), session);

        let result = run_with_auth_override(
            AuthOverride {
                workspace_id: Some(task_workspace_id),
                project_id: Some(task_project_id),
                ..Default::default()
            },
            || async { tool.execute(json!({ "action": "index" })).await },
        )
        .await;
        server.abort();
        let mapping_removed = mcp_session::auto_init::remove_global_mapping(&session_folder).await;
        assert!(mapping_removed, "fixture global mapping should be removed");
        let result = result.expect("index should start background ingest");

        assert!(!result.is_error);
        let structured = result.structured_content.expect("structured content");
        assert_eq!(
            structured.get("project_id"),
            Some(&json!(session_project_id.to_string()))
        );
        assert_eq!(
            structured.get("path").and_then(|value| value.as_str()),
            Some(session_folder.as_str())
        );
    }

    #[tokio::test]
    async fn test_project_tool_ingest_local_uses_session_folder_path() {
        let project_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let (client, config, server) =
            client_with_fresh_project(project_in_workspace(project_id, workspace_id)).await;
        let session = Arc::new(mcp_session::SessionManager::new(client.clone(), config));
        let tempdir = tempdir().expect("tempdir");
        let session_folder = tempdir.path().to_string_lossy().to_string();
        session
            .initialize(
                Some(workspace_id),
                Some(project_id),
                Some(session_folder.clone()),
                None,
            )
            .await;
        let tool = ProjectTool::new(client.clone(), session);

        let result = tool
            .execute(json!({
                "action": "ingest_local"
            }))
            .await;
        server.abort();
        let mapping_removed = mcp_session::auto_init::remove_global_mapping(&session_folder).await;
        assert!(mapping_removed, "fixture global mapping should be removed");
        let result = result.expect("ingest_local should reuse session folder path");

        assert!(!result.is_error);
        match result.content.first() {
            Some(ContentItem::Text { text }) => {
                assert!(text.contains("Updating index in background"));
            }
            other => panic!("expected text content, got {:?}", other),
        }
        let structured = result.structured_content.expect("structured content");
        assert_eq!(
            structured.get("invoked_action"),
            Some(&json!("ingest_local"))
        );
        assert_eq!(
            structured.get("project_id"),
            Some(&json!(project_id.to_string()))
        );
    }

    #[tokio::test]
    async fn test_project_tool_index_rejects_explicit_project_and_folder_mapping_conflict() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let stale_project_id = Uuid::new_v4();
        let mapped_project_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let tempdir = tempdir().expect("tempdir");
        initialize_git_checkout(tempdir.path());
        mcp_session::auto_init::establish_folder_binding(
            tempdir.path().to_string_lossy().as_ref(),
            workspace_id,
            "Engineering",
            Some(mapped_project_id),
            Some("contextstream"),
        )
        .await
        .expect("establish valid checkout binding");

        session
            .initialize(
                Some(workspace_id),
                Some(stale_project_id),
                Some(tempdir.path().to_string_lossy().to_string()),
                None,
            )
            .await;
        let tool = ProjectTool::new(client, session);

        let result = tool
            .execute(json!({
                "action": "index",
                "project_id": stale_project_id.to_string()
            }))
            .await;
        let mapping_removed = mcp_session::auto_init::remove_global_mapping(
            tempdir.path().to_string_lossy().as_ref(),
        )
        .await;
        assert!(mapping_removed, "fixture global mapping should be removed");
        let result =
            result.expect_err("conflicting explicit and folder project IDs must fail closed");

        let text = result.to_string();
        assert!(text.contains("project scope sources disagree"));
        assert!(text.contains(&format!("explicit_project_id={stale_project_id}")));
        assert!(text.contains(&format!("session_project_id={stale_project_id}")));
        assert!(text.contains(&format!("folder_mapping={mapped_project_id}")));
        assert!(text.contains("No local ingest was started"));
        assert!(
            ContextStreamClient::local_indexing_started_at(
                tempdir.path().to_string_lossy().as_ref()
            )
            .is_none(),
            "scope conflict must be rejected before writing local ingest status"
        );
    }

    #[tokio::test]
    async fn test_project_tool_ingest_rejects_project_workspace_mismatch() {
        let project_id = Uuid::new_v4();
        let expected_workspace_id = Uuid::new_v4();
        let actual_workspace_id = Uuid::new_v4();
        let (client, config, server) =
            client_with_fresh_project(project_in_workspace(project_id, actual_workspace_id)).await;
        let session = Arc::new(mcp_session::SessionManager::new(client.clone(), config));
        let tempdir = tempdir().expect("tempdir");
        let path = tempdir.path().to_string_lossy().to_string();
        session
            .initialize(
                Some(expected_workspace_id),
                Some(project_id),
                Some(path.clone()),
                None,
            )
            .await;
        let tool = ProjectTool::new(client.clone(), session);

        let error = tool
            .execute(json!({
                "action": "ingest_local"
            }))
            .await;
        server.abort();
        let error = error.expect_err("project workspace mismatch must fail closed");

        let text = error.to_string();
        assert!(text.contains(&format!("belongs to workspace {actual_workspace_id}")));
        assert!(text.contains(&format!("requires workspace {expected_workspace_id}")));
        assert!(text.contains("No local ingest was started"));
        assert!(
            ContextStreamClient::local_indexing_started_at(&path).is_none(),
            "workspace mismatch must be rejected before writing local ingest status"
        );
    }

    #[tokio::test]
    async fn test_project_tool_ingest_aborts_when_project_validation_returns_500() {
        let (client, config, server) = client_with_project_server_error().await;
        let session = Arc::new(mcp_session::SessionManager::new(client.clone(), config));
        let project_id = Uuid::new_v4();
        let workspace_id = Uuid::new_v4();
        let tempdir = tempdir().expect("tempdir");
        let path = tempdir.path().to_string_lossy().to_string();
        session
            .initialize(
                Some(workspace_id),
                Some(project_id),
                Some(path.clone()),
                None,
            )
            .await;
        let tool = ProjectTool::new(client, session);

        let error = tool
            .execute(json!({
                "action": "ingest_local"
            }))
            .await
            .expect_err("server failure must abort project validation and local ingest");
        server.abort();

        assert!(
            matches!(error, mcp_types::Error::Http { status: 500, .. }),
            "expected the project validation 500 to propagate, got: {error}"
        );
        assert!(
            ContextStreamClient::local_indexing_started_at(&path).is_none(),
            "project validation failure must be rejected before writing local ingest status"
        );
    }

    #[tokio::test]
    async fn test_project_tool_index_invalid_project_id() {
        let tool = make_project_tool();

        let result = tool
            .execute(json!({
                "action": "index",
                "project_id": "not-a-uuid"
            }))
            .await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Invalid project_id"));
    }

    #[tokio::test]
    async fn test_project_tool_overview_requires_project_id() {
        let tool = make_project_tool();

        let result = tool
            .execute(json!({
                "action": "overview"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("project_id"));
    }

    #[tokio::test]
    async fn test_project_tool_statistics_requires_project_id() {
        let tool = make_project_tool();

        let result = tool
            .execute(json!({
                "action": "statistics"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("project_id"));
    }

    #[tokio::test]
    async fn test_project_tool_files_requires_project_id() {
        let tool = make_project_tool();

        let result = tool
            .execute(json!({
                "action": "files"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("project_id"));
    }

    #[tokio::test]
    async fn test_project_tool_purge_requires_project_id() {
        let tool = make_project_tool();

        let result = tool
            .execute(json!({
                "action": "purge"
            }))
            .await;

        // "purge" is a recognized action (not "Unknown action") but needs a
        // resolved project scope.
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            !err.contains("Unknown action"),
            "purge should be a known action: {err}"
        );
        assert!(
            err.contains("project_id"),
            "expected project scope error: {err}"
        );
    }

    #[tokio::test]
    async fn test_project_tool_forget_local_requires_folder() {
        let tool = make_project_tool();

        // No folder_path and no session folder -> recognized action that needs a
        // folder (not "Unknown action").
        let result = tool
            .execute(json!({
                "action": "forget_local"
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            !err.contains("Unknown action"),
            "forget_local should be a known action: {err}"
        );
        assert!(
            err.contains("needs a folder"),
            "expected folder requirement error: {err}"
        );
    }

    #[tokio::test]
    async fn test_project_tool_remove_paths_requires_paths() {
        let tool = make_project_tool();

        // Recognized action with a resolved project_id but no paths -> a paths
        // validation error (not "Unknown action", not a missing-scope error).
        let result = tool
            .execute(json!({
                "action": "remove_paths",
                "workspace_id": "550e8400-e29b-41d4-a716-446655440000",
                "project_id": "650e8400-e29b-41d4-a716-446655440001"
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            !err.contains("Unknown action"),
            "remove_paths should be a known action: {err}"
        );
        assert!(
            err.contains("paths"),
            "expected a paths requirement error: {err}"
        );
    }

    #[tokio::test]
    async fn test_project_tool_purge_with_project_id_reaches_network() {
        let tool = make_project_tool();

        // With an explicit project_id, purge resolves scope and hits the network
        // (the mock client has no server), so the error is a transport failure —
        // NOT "Unknown action" and NOT a missing-scope/validation error. This
        // exercises the happy-path dispatch into client.purge_project_index.
        let result = tool
            .execute(json!({
                "action": "purge",
                "workspace_id": "550e8400-e29b-41d4-a716-446655440000",
                "project_id": "650e8400-e29b-41d4-a716-446655440001"
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            !err.contains("Unknown action"),
            "purge should dispatch: {err}"
        );
        assert!(
            !err.contains("is required"),
            "scope should be resolved: {err}"
        );
    }

    #[tokio::test]
    async fn test_project_tool_index_status_requires_project_id() {
        let tool = make_project_tool();

        let result = tool
            .execute(json!({
                "action": "index_status"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("project_id"));
    }

    #[tokio::test]
    async fn test_project_tool_index_history_requires_project_id() {
        let tool = make_project_tool();

        let result = tool
            .execute(json!({
                "action": "index_history"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("project_id"));
    }

    #[tokio::test]
    async fn test_project_tool_list_no_required_params() {
        let tool = make_project_tool();

        // list action has no required parameters
        let result = tool
            .execute(json!({
                "action": "list"
            }))
            .await;

        // Should fail due to network (not validation)
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(!err.contains("Unknown action"));
        assert!(!err.contains("required"));
    }

    #[tokio::test]
    async fn test_project_tool_team_projects_no_required_params() {
        let tool = make_project_tool();

        // team_projects has no required parameters
        let result = tool
            .execute(json!({
                "action": "team_projects"
            }))
            .await;

        // Should fail due to network (not validation)
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(!err.contains("Unknown action"));
        assert!(!err.contains("required"));
    }

    #[tokio::test]
    async fn test_project_tool_get_invalid_uuid_treated_as_missing() {
        let tool = make_project_tool();

        // Note: Invalid UUIDs are silently ignored (and_then with .ok()),
        // so they're treated as missing, triggering "required" error
        let result = tool
            .execute(json!({
                "action": "get",
                "project_id": "not-a-uuid"
            }))
            .await;

        assert!(result.is_err());
        // Invalid UUID is treated as None, so we get "required" error
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("project_id is required"));
    }
}

// ============================================================================
// Input Struct Tests
// ============================================================================

mod input_struct_tests {
    use super::json;
    use super::{ProjectInput, ProjectsCreateInput, ProjectsListInput};

    #[test]
    fn test_projects_list_input_deserialization() {
        let input: ProjectsListInput = serde_json::from_value(json!({
            "workspace_id": "550e8400-e29b-41d4-a716-446655440000",
            "page": 1,
            "page_size": 20
        }))
        .unwrap();

        assert!(input.workspace_id.is_some());
        assert_eq!(input.page, Some(1));
        assert_eq!(input.page_size, Some(20));
    }

    #[test]
    fn test_projects_create_input_deserialization() {
        let input: ProjectsCreateInput = serde_json::from_value(json!({
            "name": "My Project",
            "description": "A test project",
            "workspace_id": "550e8400-e29b-41d4-a716-446655440000"
        }))
        .unwrap();

        assert_eq!(input.name, "My Project");
        assert_eq!(input.description, Some("A test project".to_string()));
    }

    #[test]
    fn test_project_input_ingest_deserialization() {
        let input: ProjectInput = serde_json::from_value(json!({
            "action": "ingest_local",
            "path": "/home/user/project",
            "force": true,
            "generate_editor_rules": true
        }))
        .unwrap();

        assert_eq!(input.action, "ingest_local");
        assert_eq!(input.path, Some("/home/user/project".to_string()));
        assert_eq!(input.force, Some(true));
        assert_eq!(input.generate_editor_rules, Some(true));
        assert_eq!(input.skip_project_creation, None);
    }

    #[test]
    fn test_project_input_skip_project_creation_deserialization() {
        let input: ProjectInput = serde_json::from_value(json!({
            "action": "ingest_local",
            "path": "/tmp/foo",
            "skip_project_creation": true
        }))
        .unwrap();

        assert_eq!(input.skip_project_creation, Some(true));
    }

    #[test]
    fn test_project_input_files_deserialization() {
        let input: ProjectInput = serde_json::from_value(json!({
            "action": "files",
            "project_id": "550e8400-e29b-41d4-a716-446655440000",
            "path_pattern": "*.ts",
            "sort_by": "path",
            "sort_order": "asc",
            "page_size": 50
        }))
        .unwrap();

        assert_eq!(input.action, "files");
        assert_eq!(input.path_pattern, Some("*.ts".to_string()));
        assert_eq!(input.sort_by, Some("path".to_string()));
        assert_eq!(input.sort_order, Some("asc".to_string()));
    }

    #[test]
    fn test_project_input_merge_deserialization() {
        let input: ProjectInput = serde_json::from_value(json!({
            "action": "merge",
            "project_id": "550e8400-e29b-41d4-a716-446655440000",
            "source_project_id": "650e8400-e29b-41d4-a716-446655440001"
        }))
        .unwrap();

        assert_eq!(input.action, "merge");
        assert_eq!(
            input.source_project_id,
            Some("650e8400-e29b-41d4-a716-446655440001".to_string())
        );
    }
}

// ============================================================================
// Constants Tests
// ============================================================================

mod constants_tests {
    use super::*;

    #[test]
    fn test_valid_sort_by() {
        assert!(VALID_SORT_BY.contains(&"path"));
        assert!(VALID_SORT_BY.contains(&"indexed"));
        assert!(VALID_SORT_BY.contains(&"size"));
    }

    #[test]
    fn test_valid_sort_order() {
        assert!(VALID_SORT_ORDER.contains(&"asc"));
        assert!(VALID_SORT_ORDER.contains(&"desc"));
    }
}

// ============================================================================
// Tool Registration Tests
// ============================================================================

mod registration_tests {
    #[test]
    fn test_project_tool_count() {
        // Expected project tools:
        // - project (unified)
        // - projects_list
        // - projects_create
        // - projects_index
        // Total: 4 tools

        let expected_tools = [
            "project",
            "projects_list",
            "projects_create",
            "projects_index",
        ];

        assert_eq!(expected_tools.len(), 4);
    }

    #[test]
    fn test_project_actions_coverage() {
        // Document all project actions in the unified tool:
        //
        // No-required-params actions:
        // - list: List all projects (workspace_id/pagination optional)
        // - team_projects: List team projects (team plans only)
        //
        // Actions requiring name:
        // - create: Create new project
        //
        // Actions requiring project_id:
        // - get: Get project details
        // - update: Update project
        // - merge/combine: Merge a duplicate source_project_id into project_id
        // - index: Trigger project indexing
        // - overview: Get project overview
        // - statistics: Get project statistics
        // - files: List indexed files
        // - index_status: Get indexing status
        // - index_history: Get indexing audit trail
        //
        // Actions requiring path:
        // - ingest_local: Index local folder

        let all_actions = [
            "list",
            "get",
            "create",
            "update",
            "merge",
            "combine",
            "index",
            "delete",
            "purge",
            "forget_local",
            "remove_paths",
            "overview",
            "statistics",
            "files",
            "index_status",
            "index_history",
            "ingest_local",
            "team_projects",
            "recent_changes",
        ];

        assert_eq!(all_actions.len(), 19);
    }
}
