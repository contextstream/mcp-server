//! Regression tests for ContextStream incident 0cc8b87e: the hosted HTTP
//! gateway must never read or write the gateway host's own disk on behalf of a
//! caller-supplied local path. These cover the per-tool refusals that fire in
//! HTTP mode, the `CONTEXTSTREAM_HTTP_LOCAL_FILES=allow` operator opt-in, and
//! that stdio/local mode is unaffected.

use std::sync::Arc;

use mcp_client::ContextStreamClient;
use mcp_session::SessionManager;
use mcp_tools::domains::{
    capsule::CapsuleTool, media::MediaTool, project::ProjectTool, skill::SkillTool,
};
use mcp_tools::registry::ToolHandler;
use mcp_types::tool::ToolResult;
use mcp_types::Config;
use serde_json::json;

/// Every refusal message names the gateway so callers (and these tests) can
/// tell a policy refusal apart from a network/validation failure.
const DISABLED_MARKER: &str = "hosted MCP gateway";
const TEST_WS: &str = "11111111-1111-4111-8111-111111111111";
const TEST_PROJECT: &str = "22222222-2222-4222-8222-222222222222";

fn gateway_config(is_http: bool, opt_in: bool) -> Config {
    Config {
        // An unreachable loopback port: if a guard ever fails to short-circuit,
        // the call surfaces a network error here rather than hitting a real API.
        api_url: "http://127.0.0.1:9".to_string(),
        allow_header_auth: true,
        is_http_transport: is_http,
        http_local_files: opt_in,
        ..Config::default()
    }
}

fn build_tools(cfg: Config) -> (SkillTool, MediaTool, ProjectTool) {
    let client = ContextStreamClient::new(cfg.clone());
    let session = Arc::new(SessionManager::new(client.clone(), cfg));
    (
        SkillTool::new(client.clone(), session.clone()),
        MediaTool::new(client.clone(), session.clone()),
        ProjectTool::new(client, session),
    )
}

/// A tool failure can surface as `Err(..)` or as an error `ToolResult`; render
/// either (and a success) to text so assertions can inspect the message.
fn result_text(result: mcp_types::Result<ToolResult>) -> String {
    match result {
        Ok(tool_result) => format!("{tool_result:?}"),
        Err(error) => error.to_string(),
    }
}

#[test]
fn config_local_files_allowed_matrix() {
    assert!(
        gateway_config(false, false).local_files_allowed(),
        "stdio allows"
    );
    assert!(
        gateway_config(false, true).local_files_allowed(),
        "stdio allows"
    );
    assert!(
        !gateway_config(true, false).local_files_allowed(),
        "http denies caller file paths by default"
    );
    assert!(
        gateway_config(true, true).local_files_allowed(),
        "http opt-in re-enables caller file paths"
    );
}

#[tokio::test]
async fn http_gateway_refuses_skill_import_file_path() {
    let (skill, _, _) = build_tools(gateway_config(true, false));
    // The guard fires before the file is read, so even a path that exists on
    // the gateway (and would otherwise be exfiltrated) is refused outright.
    let text = result_text(
        skill
            .execute(json!({"action": "import", "file_path": "/etc/hostname"}))
            .await,
    );
    assert!(text.contains(DISABLED_MARKER), "got: {text}");
    assert!(
        text.to_ascii_lowercase().contains("your own machine"),
        "tells the agent to read the file locally on its own machine: {text}"
    );
    assert!(
        text.contains("content"),
        "names the safe alternative: {text}"
    );
    assert!(
        text.contains("source_file"),
        "tells the agent to pass source_file with content: {text}"
    );
}

#[tokio::test]
async fn http_gateway_skill_import_with_both_content_and_file_path_uses_file_path_only_as_source_file(
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let bodies = Arc::new(std::sync::Mutex::new(Vec::<serde_json::Value>::new()));
    let bodies_clone = bodies.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let bodies_clone = bodies_clone.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let body_start = loop {
                    let mut chunk = [0u8; 4096];
                    let count = socket.read(&mut chunk).await.unwrap_or(0);
                    if count == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..count]);
                    if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..end]).to_string();
                        let len: usize = head
                            .lines()
                            .filter_map(|l| l.split_once(':'))
                            .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                            .and_then(|(_, v)| v.trim().parse().ok())
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + len {
                            break end + 4;
                        }
                    }
                };
                let parsed: serde_json::Value =
                    serde_json::from_slice(&buf[body_start..]).unwrap_or_else(|_| json!({}));
                bodies_clone.lock().unwrap().push(parsed);
                let payload = json!({
                    "success": true,
                    "data": {
                        "imported": 1,
                        "skipped": 0,
                        "rejected": 0,
                        "skills": [{
                            "id": "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa",
                            "name": "team-guidelines",
                            "status": "active",
                            "scope": "team",
                            "is_personal": false,
                            "project_id": TEST_PROJECT
                        }]
                    }
                })
                .to_string();
                let resp = format!(
                    "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = socket.write_all(resp.as_bytes()).await;
            });
        }
    });

    let cfg = Config {
        api_url: base_url,
        api_key: Some("test-api-key".to_string()),
        allow_header_auth: true,
        is_http_transport: true,
        http_local_files: false,
        ..Config::default()
    };
    let client = ContextStreamClient::new(cfg.clone());
    let session = Arc::new(SessionManager::new(client.clone(), cfg));
    session
        .initialize(
            Some(uuid::Uuid::parse_str(TEST_WS).unwrap()),
            Some(uuid::Uuid::parse_str(TEST_PROJECT).unwrap()),
            None,
            None,
        )
        .await;
    let skill = SkillTool::new(client, session);

    // Even though /etc/hostname exists on the gateway host, passing content
    // alongside file_path must NEVER read the file and must only use file_path
    // as source_file metadata.
    let expected_content =
        "## Team Guidelines\n1. Check ledger invariants before deploying worker changes.";
    let first = skill
        .execute(json!({
            "action": "import",
            "content": expected_content,
            "file_path": "/etc/hostname",
            "scope": "team"
        }))
        .await
        .expect("gateway import with both content and file_path should succeed");

    // When source_file is also given explicitly, source_file takes precedence.
    skill
        .execute(json!({
            "action": "import",
            "content": expected_content,
            "file_path": "/etc/hostname",
            "source_file": "CLAUDE.md",
            "scope": "team"
        }))
        .await
        .expect("gateway import with content, file_path and explicit source_file should succeed");

    task.abort();

    let sent = bodies.lock().unwrap().clone();
    assert_eq!(sent.len(), 2, "expected two import requests: {sent:?}");
    assert_eq!(sent[0]["content"].as_str(), Some(expected_content));
    assert_eq!(sent[0]["source_file"].as_str(), Some("/etc/hostname"));
    assert_eq!(sent[0]["scope"].as_str(), Some("team"));
    assert_eq!(sent[0]["workspace_id"].as_str(), Some(TEST_WS));
    assert_eq!(sent[0]["project_id"].as_str(), Some(TEST_PROJECT));

    assert_eq!(sent[1]["content"].as_str(), Some(expected_content));
    assert_eq!(sent[1]["source_file"].as_str(), Some("CLAUDE.md"));

    let rendered = result_text(Ok(first));
    assert!(rendered.contains("team-guidelines"), "got: {rendered}");
    assert!(rendered.contains("active"), "got: {rendered}");
    assert!(rendered.contains("visibility=team"), "got: {rendered}");
    assert!(
        rendered.contains(&format!("project={TEST_PROJECT}")),
        "got: {rendered}"
    );
}

#[tokio::test]
async fn http_gateway_media_index_file_path_returns_upload_plan_without_reading_disk() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}", listener.local_addr().unwrap());
    let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen_clone = seen.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let seen_clone = seen_clone.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let head = loop {
                    let mut chunk = [0u8; 4096];
                    let count = socket.read(&mut chunk).await.unwrap_or(0);
                    if count == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..count]);
                    if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..end]).to_string();
                        let len: usize = head
                            .lines()
                            .filter_map(|l| l.split_once(':'))
                            .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                            .and_then(|(_, v)| v.trim().parse().ok())
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + len {
                            break head;
                        }
                    }
                };
                let line = head.lines().next().unwrap_or_default().to_string();
                seen_clone.lock().unwrap().push(line);
                let payload = json!({
                    "success": true,
                    "data": {
                        "content_id": "550e8400-e29b-41d4-a716-446655440000",
                        "upload_url": "https://storage.example.test/presigned",
                        "headers": {"Content-Type": "text/plain"},
                        "expires_in_seconds": 600
                    }
                })
                .to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = socket.write_all(resp.as_bytes()).await;
            });
        }
    });

    let cfg = Config {
        api_url: base_url,
        api_key: Some("test-api-key".to_string()),
        allow_header_auth: true,
        is_http_transport: true,
        http_local_files: false,
        ..Config::default()
    };
    let (_, media, _) = build_tools(cfg);
    // Even though /etc/hostname exists on the gateway host, hosted media index
    // must NEVER read it; it calls uploads/init and returns a client upload plan.
    let result = media
        .execute(json!({
            "action": "index",
            "file_path": "/etc/hostname",
            "content_type": "document",
            "size_bytes": 16,
            "allow_broad": true,
            "workspace_id": TEST_WS
        }))
        .await
        .expect("hosted media index should return upload plan instead of dead-ending");

    task.abort();

    let calls = seen.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "only uploads/init should run: {calls:?}");
    assert!(
        calls[0].starts_with(&format!(
            "POST /api/v1/workspaces/{TEST_WS}/content/uploads/init "
        )),
        "got: {:?}",
        calls
    );
    let text = result_text(Ok(result));
    assert!(
        text.contains("curl -fsS -X PUT --upload-file '/etc/hostname'"),
        "got: {text}"
    );
    assert!(text.contains("complete_upload"), "got: {text}");
}

#[tokio::test]
async fn http_gateway_client_media_index_refusal_names_upload_path() {
    let client = ContextStreamClient::new(gateway_config(true, false));
    let err = client
        .media_index(mcp_client::MediaIndexParams {
            file_path: Some("/etc/hostname".to_string()),
            workspace_id: Some(uuid::Uuid::parse_str(TEST_WS).unwrap()),
            ..Default::default()
        })
        .await
        .expect_err("direct client.media_index(file_path) must still refuse on hosted gateway")
        .to_string();
    assert!(err.contains(DISABLED_MARKER), "got: {err}");
    assert!(err.contains("upload_url"), "names upload_url path: {err}");
    assert!(
        err.contains("complete_upload"),
        "names complete_upload follow-up: {err}"
    );
    assert!(
        err.contains("external_url"),
        "still names external_url: {err}"
    );
}

#[tokio::test]
async fn http_gateway_refuses_project_ingest_local() {
    let (_, _, project) = build_tools(gateway_config(true, false));
    let text = result_text(
        project
            .execute(json!({
                "action": "ingest_local",
                "path": "/etc",
                "workspace_id": TEST_WS,
                "project_id": TEST_PROJECT
            }))
            .await,
    );
    assert!(text.contains(DISABLED_MARKER), "got: {text}");
}

#[tokio::test]
async fn opt_in_reenables_local_file_paths() {
    // With the operator opt-in the guard must NOT short-circuit: calls proceed
    // past it (and then fail on the unreachable test API), never with the
    // hosted-gateway refusal.
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("asset.txt");
    std::fs::write(&file, "opt-in-allowed").unwrap();
    let file = file.to_string_lossy().to_string();

    let (skill, media, _) = build_tools(gateway_config(true, true));

    let skill_text = result_text(
        skill
            .execute(json!({"action": "import", "file_path": file}))
            .await,
    );
    assert!(
        !skill_text.contains(DISABLED_MARKER),
        "opt-in must not refuse skill import: {skill_text}"
    );

    let media_text = result_text(
        media
            .execute(json!({
                "action": "index",
                "file_path": file,
                "content_type": "document",
                "allow_broad": true,
                "workspace_id": TEST_WS
            }))
            .await,
    );
    assert!(
        !media_text.contains(DISABLED_MARKER),
        "opt-in must not refuse media index: {media_text}"
    );
}

#[tokio::test]
async fn stdio_mode_allows_local_file_paths() {
    // stdio/local transport is unaffected: a file path is accepted (and here
    // simply fails against the unreachable test API), never refused.
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("asset.txt");
    std::fs::write(&file, "stdio-allowed").unwrap();
    let file = file.to_string_lossy().to_string();

    let (skill, _, _) = build_tools(gateway_config(false, false));
    let text = result_text(
        skill
            .execute(json!({"action": "import", "file_path": file}))
            .await,
    );
    assert!(
        !text.contains(DISABLED_MARKER),
        "stdio must not refuse: {text}"
    );
}

#[tokio::test]
async fn http_gateway_refuses_capsule_create_with_review() {
    let cfg = gateway_config(true, false);
    let client = ContextStreamClient::new(cfg.clone());
    let session = Arc::new(SessionManager::new(client.clone(), cfg));
    let capsule = CapsuleTool::new(client).with_session(session);
    // Fires before git or network calls even when folder_path points at a local path.
    let text = result_text(
        capsule
            .execute(json!({
                "action": "create",
                "project_id": TEST_PROJECT,
                "folder_path": "/etc",
                "review": {
                    "base": "HEAD~1",
                    "head": "HEAD",
                    "task": "Review commit diff"
                }
            }))
            .await,
    );
    assert!(text.contains(DISABLED_MARKER), "got: {text}");
}

#[tokio::test]
async fn http_gateway_allows_capsule_create_purpose_review_without_review_object() {
    let cfg = gateway_config(true, false);
    let client = ContextStreamClient::new(cfg.clone());
    let session = Arc::new(SessionManager::new(client.clone(), cfg));
    let capsule = CapsuleTool::new(client).with_session(session);
    let text = result_text(
        capsule
            .execute(json!({
                "action": "create",
                "scope": "workspace",
                "workspace_id": TEST_WS,
                "purpose": "review"
            }))
            .await,
    );
    assert!(
        !text.contains(DISABLED_MARKER),
        "legacy purpose='review' without review object must not hit hosted local-files refusal: {text}"
    );
}
