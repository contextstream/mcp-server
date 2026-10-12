//! Regression tests for ContextStream incident 0cc8b87e: the hosted HTTP
//! gateway must never read or write the gateway host's own disk on behalf of a
//! caller-supplied local path. These cover the per-tool refusals that fire in
//! HTTP mode, the `CONTEXTSTREAM_HTTP_LOCAL_FILES=allow` operator opt-in, and
//! that stdio/local mode is unaffected.

use std::sync::Arc;

use mcp_client::ContextStreamClient;
use mcp_session::SessionManager;
use mcp_tools::domains::{media::MediaTool, project::ProjectTool, skill::SkillTool};
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
        text.contains("content"),
        "names the safe alternative: {text}"
    );
}

#[tokio::test]
async fn http_gateway_refuses_media_index_file_path() {
    let (_, media, _) = build_tools(gateway_config(true, false));
    let text = result_text(
        media
            .execute(json!({
                "action": "index",
                "file_path": "/etc/hostname",
                "content_type": "document",
                "allow_broad": true,
                "workspace_id": TEST_WS
            }))
            .await,
    );
    assert!(text.contains(DISABLED_MARKER), "got: {text}");
    assert!(
        text.contains("external_url"),
        "names the safe alternative: {text}"
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
