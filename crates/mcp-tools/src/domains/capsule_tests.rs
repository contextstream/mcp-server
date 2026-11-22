//! Tests for ContextCapsule domain tools.
#![allow(clippy::field_reassign_with_default)]

use super::*;
use crate::domains::request_probe::RequestProbe;
use crate::registry::ToolHandler;
use crate::testing::TestFixtures;
use mcp_client::CapsuleShareParams;
use mcp_types::api::{
    ContextCapsuleReadiness, ContextCapsuleResolvedScope, ContextCapsuleResponse,
    ContextCapsuleSection, ContextCapsuleShareResponse,
};
use mcp_types::tool::{ToolCategory, ToolResult};
use serde_json::json;
use uuid::Uuid;

fn create_mock_client() -> ContextStreamClient {
    ContextStreamClient::new(TestFixtures::test_config())
}

#[test]
fn test_capsule_tool_metadata() {
    let tool = CapsuleTool::new(create_mock_client());
    let metadata = tool.metadata();

    assert_eq!(metadata.name, "capsule");
    assert_eq!(metadata.title, "ContextCapsule");
    assert!(metadata.description.contains("ContextCapsule"));
    assert!(metadata
        .description
        .contains("entity(kind=\"handoff\", action=\"create\""));
    assert!(metadata
        .description
        .contains("create the entity AND this capsule"));
    assert!(metadata.description.contains("not a replacement"));
    assert!(metadata.description.contains("HANDOFF.md"));
    assert_eq!(metadata.category, ToolCategory::Utility);
}

#[test]
fn test_capsule_tool_schema() {
    let tool = CapsuleTool::new(create_mock_client());
    let schema = tool.input_schema();
    let props = schema["properties"].as_object().unwrap();

    assert!(props.contains_key("action"));
    assert!(props.contains_key("capsule_id"));
    assert!(props.contains_key("share_token"));
    assert!(props.contains_key("url"));
    assert!(props.contains_key("chunk_id"));
    assert!(props.contains_key("cursor_chunk_id"));
    assert!(props.contains_key("audience"));
    assert!(props.contains_key("scope"));
    assert!(props.contains_key("purpose"));
    assert!(props.contains_key("event_kind"));
    assert!(props.contains_key("access_scope"));
    assert!(props.contains_key("limit"));
    assert!(props.contains_key("offset"));
    assert!(props.contains_key("graph"));
    assert!(props.contains_key("max_uses"));
    assert!(props.contains_key("max_inline_tokens"));
    assert!(props.contains_key("refresh_if_stale"));
}

#[test]
fn test_valid_actions_includes_list_get_graph() {
    assert!(VALID_ACTIONS.contains(&"list"));
    assert!(VALID_ACTIONS.contains(&"get"));
    assert!(VALID_ACTIONS.contains(&"graph"));
}

#[test]
fn test_valid_graphs_enum() {
    assert_eq!(VALID_GRAPHS, &["explorer", "knowledge", "code"]);
}

#[test]
fn test_valid_access_scopes_include_authenticated_share() {
    assert!(VALID_ACCESS_SCOPES.contains(&"authenticated"));
    assert!(VALID_ACCESS_SCOPES.contains(&"authenticated_share"));
    assert!(VALID_ACCESS_SCOPES.contains(&"public_share"));
}

#[test]
fn test_apply_safe_share_defaults_preserves_max_uses() {
    let params = apply_safe_share_defaults(CapsuleShareParams {
        audience: Some("external_agent".to_string()),
        max_uses: Some(3),
        ..CapsuleShareParams::default()
    });
    assert_eq!(params.max_uses, Some(3));
}

#[test]
fn test_apply_safe_share_defaults_for_external_audience() {
    let params = apply_safe_share_defaults(CapsuleShareParams {
        audience: Some("external_agent".to_string()),
        ..CapsuleShareParams::default()
    });

    assert_eq!(params.include_personal, Some(false));
    assert_eq!(params.include_code.as_deref(), Some("none"));
    // Strict, not standard: token-gated external links default to the
    // tightest redaction so the backend guardrail passes without
    // allow_risky_policy=true.
    assert_eq!(params.redaction_level.as_deref(), Some("strict"));
    // 3-day expiry stays inside the 7-day risky threshold.
    assert_eq!(params.expires_in_days, Some(3));
    // Default to single-use (burn-after-read).
    assert_eq!(params.multi_use, Some(false));
    assert_eq!(params.max_uses, Some(1));
}

#[test]
fn test_session_create_auto_share_audience_defaults_to_external_agent() {
    let input = CapsuleInput {
        action: "create".to_string(),
        scope: Some("session".to_string()),
        ..CapsuleInput::default()
    };
    assert_eq!(
        session_create_auto_share_audience(&input).as_deref(),
        Some("external_agent")
    );
}

#[test]
fn test_session_create_auto_share_audience_self_opts_out() {
    let input = CapsuleInput {
        action: "create".to_string(),
        scope: Some("session".to_string()),
        audience: Some("Self".to_string()),
        ..CapsuleInput::default()
    };
    assert_eq!(session_create_auto_share_audience(&input), None);
}

#[test]
fn test_session_create_auto_share_audience_normalizes_team() {
    let input = CapsuleInput {
        action: "create".to_string(),
        scope: Some("session".to_string()),
        audience: Some("TEAM".to_string()),
        ..CapsuleInput::default()
    };
    assert_eq!(
        session_create_auto_share_audience(&input).as_deref(),
        Some("team")
    );
}

#[test]
fn test_session_create_auto_share_audience_skips_project_scope() {
    let input = CapsuleInput {
        action: "create".to_string(),
        scope: Some("project".to_string()),
        ..CapsuleInput::default()
    };
    assert_eq!(session_create_auto_share_audience(&input), None);
}

#[test]
fn test_apply_safe_share_defaults_forwards_explicit_risky_for_backend_guardrail() {
    // Explicit risky policy values must be forwarded untouched (except the
    // expiry clamp) so the backend guardrail can block + enumerate them when
    // allow_risky_policy is not set. They must NOT be silently neutralized to
    // safe values (the old behavior, which bypassed the guardrail UX).
    let params = apply_safe_share_defaults(CapsuleShareParams {
        audience: Some("external_agent".to_string()),
        include_personal: Some(true),
        include_code: Some("lazy".to_string()),
        redaction_level: Some("none".to_string()),
        expires_in_days: Some(14),
        multi_use: Some(true),
        ..CapsuleShareParams::default()
    });
    // Explicit risky policy fields pass through to the backend guardrail.
    assert_eq!(params.include_personal, Some(true));
    assert_eq!(params.include_code.as_deref(), Some("lazy"));
    assert_eq!(params.redaction_level.as_deref(), Some("none"));
    // Expiry is still clamped to the safe external window — the one dimension
    // the MCP layer clamps itself rather than delegating to the guardrail.
    assert_eq!(params.expires_in_days, Some(7));
    // Unaffected behavior preserved.
    assert_eq!(params.multi_use, Some(true));
    // max_uses must NOT be auto-clamped to 1 when caller opted out of
    // single-use semantics with multi_use=true.
    assert_eq!(params.max_uses, None);
}

#[test]
fn test_apply_safe_share_defaults_respects_risky_policy_override() {
    let params = apply_safe_share_defaults(CapsuleShareParams {
        audience: Some("external_agent".to_string()),
        include_personal: Some(true),
        include_code: Some("lazy".to_string()),
        redaction_level: Some("none".to_string()),
        expires_in_days: Some(14),
        allow_risky_policy: Some(true),
        ..CapsuleShareParams::default()
    });
    assert_eq!(params.include_personal, Some(true));
    assert_eq!(params.include_code.as_deref(), Some("lazy"));
    assert_eq!(params.redaction_level.as_deref(), Some("none"));
    assert_eq!(params.expires_in_days, Some(14));
}

#[test]
fn test_apply_safe_share_defaults_clamps_explicit_expires_at() {
    let future = (chrono::Utc::now() + chrono::Duration::days(14)).to_rfc3339();
    let params = apply_safe_share_defaults(CapsuleShareParams {
        audience: Some("external_agent".to_string()),
        expires_at: Some(future),
        ..CapsuleShareParams::default()
    });
    let clamped = params
        .expires_at
        .as_deref()
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .expect("expires_at must remain valid RFC3339")
        .with_timezone(&chrono::Utc);
    assert!(
        clamped <= chrono::Utc::now() + chrono::Duration::days(8),
        "external share expires_at should be clamped near seven days"
    );
}

#[test]
fn test_apply_safe_share_defaults_for_bootstrap_link_uses_long_multiuse_defaults() {
    let params = apply_safe_share_defaults(CapsuleShareParams {
        audience: Some("bootstrap_link".to_string()),
        ..CapsuleShareParams::default()
    });
    assert_eq!(params.expires_in_days, Some(14));
    assert_eq!(params.multi_use, Some(true));
    assert_eq!(params.permissions.as_deref(), Some("read_only"));
    // Bootstrap-link policy is server-locked; MCP doesn't auto-clamp
    // include_personal/include_code/redaction_level because the backend
    // hardcodes those for this audience in default_policy_for_audience.
    assert_eq!(params.include_personal, None);
    assert_eq!(params.include_code, None);
    assert_eq!(params.redaction_level, None);
}

#[test]
fn test_format_primer_summary_surfaces_doc_id_and_next_steps() {
    let payload = json!({
        "doc_id": "abc-123",
        "title": "mcp — handoff primer",
        "project_id": "deadbeef",
        "next_steps": [
            "Edit the placeholder sections…",
            "Re-create the capsule (readiness will lift)…",
            "Share with audience=\"bootstrap_link\"…",
        ],
    });
    let text = format_primer_summary(&payload);
    assert!(text.contains("✓ primer drafted · doc_id=abc-123"));
    assert!(text.contains("\"mcp — handoff primer\""));
    assert!(text.contains("project=deadbeef"));
    assert!(text.contains("1. Edit the placeholder sections"));
    assert!(text.contains("memory(action=\"update_doc\""));
}

#[test]
fn test_valid_actions_include_primer() {
    assert!(VALID_ACTIONS.contains(&"primer"));
}

#[test]
fn test_format_ack_summary_surfaces_sections_and_notes() {
    let payload = json!({
        "acked": true,
        "share_id": "9b1c2d3e",
        "acked_at": "2026-05-22T18:00:00Z",
        "sections_read": ["decisions", "lessons"],
        "notes": "picking up Shark training restart",
    });
    let text = format_ack_summary(&payload);
    assert!(text.starts_with("✓ ack recorded · share=9b1c2d3e"));
    assert!(text.contains("Sections read: decisions, lessons"));
    assert!(text.contains("Notes: picking up Shark training restart"));
    assert!(text.contains("latest_ack"));
}

#[test]
fn test_format_ack_summary_omits_optional_fields_when_absent() {
    let payload = json!({
        "share_id": "deadbeef",
        "acked_at": "2026-05-22T18:00:00Z",
    });
    let text = format_ack_summary(&payload);
    assert!(text.contains("✓ ack recorded"));
    assert!(!text.contains("Sections read:"));
    assert!(!text.contains("Notes:"));
}

#[test]
fn test_valid_actions_include_ack() {
    assert!(VALID_ACTIONS.contains(&"ack"));
}

#[test]
fn test_valid_audiences_include_bootstrap_link() {
    assert!(VALID_AUDIENCES.contains(&"bootstrap_link"));
    assert!(VALID_TOKEN_SHARE_AUDIENCES.contains(&"bootstrap_link"));
}

#[test]
fn test_apply_safe_share_defaults_explicit_expires_at_preserves_default_days_unset() {
    // When the caller passes expires_at (RFC3339), the helper must not
    // also auto-populate expires_in_days; the server uses expires_at
    // verbatim.
    let params = apply_safe_share_defaults(CapsuleShareParams {
        audience: Some("external_agent".to_string()),
        expires_at: Some("2026-06-01T00:00:00Z".to_string()),
        ..CapsuleShareParams::default()
    });
    assert_eq!(params.expires_in_days, None);
    assert_eq!(params.expires_at.as_deref(), Some("2026-06-01T00:00:00Z"));
}

#[test]
fn test_apply_safe_share_defaults_preserves_team_settings() {
    let params = apply_safe_share_defaults(CapsuleShareParams {
        audience: Some("team".to_string()),
        include_code: Some("lazy".to_string()),
        ..CapsuleShareParams::default()
    });

    assert_eq!(params.audience.as_deref(), Some("team"));
    assert_eq!(params.include_code.as_deref(), Some("lazy"));
    assert_eq!(params.permissions.as_deref(), Some("read_only"));
    assert_eq!(params.multi_use, None);
    assert_eq!(params.expires_in_days, None);
}

#[test]
fn test_validate_share_audience_allows_team_and_rejects_self() {
    CapsuleTool::validate_share_audience(&Some("team".to_string()))
        .expect("team token shares should be allowed");

    let self_err = CapsuleTool::validate_share_audience(&Some("self".to_string()))
        .expect_err("self should be rejected");

    assert!(self_err.to_string().contains("does not mint share tokens"));
}

#[test]
fn test_open_format_helpers_route_docs_and_streams() {
    assert!(is_context_doc_format(Some("markdown")));
    assert!(is_context_doc_format(Some("text")));
    assert!(is_context_doc_format(Some("plain")));
    assert!(is_context_doc_format(Some("txt")));
    assert!(is_stream_format(Some("ndjson")));
    assert!(!is_context_doc_format(Some("summary")));
    assert!(!is_stream_format(Some("summary")));
}

#[test]
fn test_format_stream_summary_counts_chunk_records() {
    let summary = format_stream_summary(
        r#"{"kind":"header"}
{"kind":"chunk"}
{"kind":"chunk"}
{"kind":"footer"}"#,
    );
    assert!(summary.contains("4 line"));
    assert!(summary.contains("2 chunk"));
}

#[test]
fn test_format_audit_summary_lists_event_kinds() {
    let events = vec![
        mcp_types::api::ContextCapsuleAuditEventResponse {
            event_kind: "open".to_string(),
            ..Default::default()
        },
        mcp_types::api::ContextCapsuleAuditEventResponse {
            event_kind: "render_markdown".to_string(),
            ..Default::default()
        },
    ];
    let summary = format_audit_summary(&events, Some("cap_123"));
    assert!(summary.contains("cap_123"));
    assert!(summary.contains("open"));
    assert!(summary.contains("render_markdown"));
}

#[test]
fn test_format_list_shares_summary_uses_scope_when_capsule_missing() {
    let summary = format_list_shares_summary(&[], None, Some("proj_123"), None);
    assert!(summary.contains("project proj_123"));
}

#[test]
fn test_format_list_shares_summary_uses_default_scope_fallback() {
    let summary = format_list_shares_summary(&[], None, None, None);
    assert!(summary.contains("current default scope"));
}

#[test]
fn test_format_capsule_summary_surfaces_share_url_and_deep_links() {
    let mut c = ContextCapsuleResponse::default();
    c.capsule_id = "cap_x".into();
    c.name = Some("Handoff".into());
    c.purpose = Some("handoff".into());
    c.mode = Some("live".into());
    c.policy.redaction_level = Some("standard".into());
    c.redaction_summary = Some(json!({"counts": {"secrets": 2}}));
    c.links.share_url = Some("https://ctx.example/c/capsule_tok".into());
    c.links.project_explorer_url = Some("https://ctx.example/dash/explorer".into());
    c.links.knowledge_graph_url = Some("https://ctx.example/dash/kg".into());
    c.links.code_graph_url = Some("https://ctx.example/dash/cg".into());
    c.bootstrap = Some(json!({
        "summary": "Rule-based summary",
        "llm_overview": {
            "summary": "LLM executive summary",
            "recommended_first_actions": ["Read README", "Run tests"],
            "section_summaries": { "files": "Top-level layout" }
        },
        "recommended_first_actions": ["fallback"]
    }));
    c.sections.push(ContextCapsuleSection {
        id: "files".into(),
        item_count: Some(1),
        ..Default::default()
    });
    let s = format_capsule_summary(&c);
    assert!(s.contains("Share URL: https://ctx.example/c/capsule_tok"));
    assert!(s.contains("Project explorer: https://ctx.example/dash/explorer"));
    assert!(s.contains("Knowledge graph: https://ctx.example/dash/kg"));
    assert!(s.contains("Code graph: https://ctx.example/dash/cg"));
    assert!(s.contains("LLM executive summary"));
    assert!(s.contains("1. Read README"));
    assert!(s.contains("Section note: Top-level layout"));
}

#[test]
fn test_format_capsule_summary_surfaces_lazy_chunk_hint() {
    let mut c = ContextCapsuleResponse::default();
    c.capsule_id = "cap_y".into();
    c.sections.push(ContextCapsuleSection {
        id: "files".into(),
        item_count: Some(3),
        chunk_ids: vec!["chunk-1".into()],
        data: None,
        ..Default::default()
    });
    let s = format_capsule_summary(&c);
    assert!(s.contains("lazy (fetch via action=chunk)"));
    assert!(s.contains("action=\"chunk\""));
}

#[test]
fn test_format_graph_summary_explorer_shape() {
    let v = json!({
        "schema": "capsule_explorer_graph.v1",
        "project_id": "550e8400-e29b-41d4-a716-446655440000",
        "metadata": {"file_count": 2, "directory_count": 1},
        "nodes": [
            {"type": "directory", "id": "dir:src"},
            {"type": "file", "id": "file:1"}
        ],
        "edges": [
            {"type": "contains", "source": "dir:src", "target": "file:1"}
        ]
    });
    let s = format_graph_summary(&v);
    assert!(s.contains("capsule_explorer_graph.v1"));
    assert!(s.contains("nodes: 2 total"));
    assert!(s.contains("edges: 1 total"));
    assert!(s.contains("\"directory\": 1"));
    assert!(s.contains("\"file\": 1"));
}

#[test]
fn test_format_list_shares_summary_marks_single_use() {
    let shares = vec![ContextCapsuleShareResponse {
        id: Uuid::nil(),
        single_use: true,
        max_uses: Some(1),
        use_count: 0,
        token_prefix: "capsule_".into(),
        audience: Some("public_link".into()),
        ..Default::default()
    }];
    let s = format_list_shares_summary(&shares, Some("cap_z"), None, None);
    assert!(s.contains("single-use, unread"));
    assert!(s.contains("use_count=0"));
}

#[test]
fn test_format_share_result_text_external_agent_promotes_agent_url() {
    let response = ContextCapsuleShareResponse {
        id: Uuid::nil(),
        audience: Some("external_agent".into()),
        share_url: Some("https://ctx.example/c/capsule_tok".into()),
        agent_url: Some("https://ctx.example/api/v1/capsules/shares/capsule_tok".into()),
        token_prefix: "capsule_".into(),
        ..Default::default()
    };
    let s = format_share_result_text("cap_x", &response);
    let agent_idx = s
        .find("Agent URL (paste into LLMs): https://ctx.example/api/v1/capsules/shares/capsule_tok")
        .expect("agent URL line missing");
    let dashboard_idx = s
        .find("Dashboard URL (open in browser): https://ctx.example/c/capsule_tok")
        .expect("dashboard URL line missing");
    assert!(
        agent_idx < dashboard_idx,
        "external_agent share must list Agent URL before Dashboard URL\noutput:\n{}",
        s
    );
    // Should NOT carry the legacy "URL: ..." line for external_agent — agent
    // URL is now the primary surface.
    assert!(
        !s.contains("\nURL: "),
        "external_agent share should not emit the legacy 'URL:' line\noutput:\n{}",
        s
    );
}

#[test]
fn test_format_share_result_text_external_agent_falls_back_to_api_url() {
    // Until the contextstream API ships agent_url, the MCP layer must fall
    // back to the existing api_url field, which has the same semantics.
    let response = ContextCapsuleShareResponse {
        id: Uuid::nil(),
        audience: Some("external_agent".into()),
        share_url: Some("https://ctx.example/c/capsule_tok".into()),
        agent_url: None,
        api_url: Some("https://ctx.example/api/v1/capsules/shares/capsule_tok".into()),
        token_prefix: "capsule_".into(),
        ..Default::default()
    };
    let s = format_share_result_text("cap_x", &response);
    assert!(
        s.contains(
            "Agent URL (paste into LLMs): https://ctx.example/api/v1/capsules/shares/capsule_tok"
        ),
        "must surface api_url as the agent URL when agent_url is missing\noutput:\n{}",
        s
    );
}

#[test]
fn test_format_share_result_text_public_link_keeps_share_url_primary() {
    // Non-agent audiences keep the legacy ordering: dashboard URL is the
    // primary "URL:" line; agent URL is supplemental.
    let response = ContextCapsuleShareResponse {
        id: Uuid::nil(),
        audience: Some("public_link".into()),
        share_url: Some("https://ctx.example/c/capsule_tok".into()),
        agent_url: Some("https://ctx.example/api/v1/capsules/shares/capsule_tok".into()),
        token_prefix: "capsule_".into(),
        ..Default::default()
    };
    let s = format_share_result_text("cap_x", &response);
    let url_idx = s
        .find("URL: https://ctx.example/c/capsule_tok")
        .expect("primary URL line missing");
    let agent_idx = s
        .find("Agent URL (paste into LLMs): https://ctx.example/api/v1/capsules/shares/capsule_tok")
        .expect("supplemental agent URL line missing");
    assert!(
        url_idx < agent_idx,
        "public_link share must keep dashboard URL primary\noutput:\n{}",
        s
    );
}

#[test]
fn test_format_capsule_create_headline_matches_docs_shape() {
    let mut c = ContextCapsuleResponse::default();
    c.capsule_id = "cap_8f3c".into();
    c.scope = Some(json!("project"));
    c.expires_at = Some((chrono::Utc::now() + chrono::Duration::days(7)).to_rfc3339());
    c.sections.push(ContextCapsuleSection {
        id: "files".into(),
        item_count: Some(47),
        ..Default::default()
    });
    let headline = format_capsule_create_headline(&c);
    assert!(
        headline.starts_with("✓ cap_8f3c"),
        "headline must lead with capsule id\noutput: {headline}"
    );
    assert!(headline.contains("scope=project"));
    assert!(headline.contains("1 sections"));
    assert!(headline.contains("47 items indexed"));
    assert!(headline.contains("expires in"));
    assert_eq!(
        headline.matches('\n').count(),
        0,
        "headline must be a single line\noutput: {headline}"
    );
}

#[test]
fn test_format_capsule_create_result_text_includes_session_share_links() {
    let mut capsule = ContextCapsuleResponse::default();
    capsule.capsule_id = "cap_session".into();
    capsule.scope = Some(json!("session"));
    capsule.sections.push(ContextCapsuleSection {
        id: "session_transcript".into(),
        title: "Session Transcript".into(),
        item_count: Some(4),
        chunk_ids: vec!["session_transcript-1".into()],
        ..Default::default()
    });
    let share = ContextCapsuleShareResponse {
        id: Uuid::nil(),
        audience: Some("external_agent".into()),
        share_url: Some("https://ctx.example/c/capsule_tok".into()),
        agent_url: Some("https://ctx.example/api/v1/capsules/shares/capsule_tok".into()),
        token_prefix: "capsule_".into(),
        single_use: true,
        max_uses: Some(1),
        ..Default::default()
    };

    let text = format_capsule_create_result_text(&capsule, Some(&share), None);

    assert!(text.contains("Session capsule share links:"));
    assert!(text.contains(
        "Agent URL (paste into LLMs): https://ctx.example/api/v1/capsules/shares/capsule_tok"
    ));
    assert!(text.contains("Dashboard URL (open in browser): https://ctx.example/c/capsule_tok"));
    assert!(text.contains("Share policy: single-use, unread"));
}

#[test]
fn test_format_capsule_create_result_text_includes_auto_share_warning() {
    let mut capsule = ContextCapsuleResponse::default();
    capsule.capsule_id = "cap_session".into();
    capsule.scope = Some(json!("session"));

    let text = format_capsule_create_result_text(
        &capsule,
        None,
        Some("Session share links were not created because auto-share failed: policy rejected"),
    );

    assert!(text.contains("✓ cap_session"));
    assert!(text.contains(
        "Session share links were not created because auto-share failed: policy rejected"
    ));
    assert!(!text.contains("Session capsule share links:"));
}

#[test]
fn test_capsule_create_structured_records_auto_share_error() {
    let mut capsule = ContextCapsuleResponse::default();
    capsule.capsule_id = "cap_session".into();

    let structured =
        capsule_create_structured(&capsule, Some(false), None, Some("policy rejected"));

    assert_eq!(structured["capsule_id"], json!("cap_session"));
    assert_eq!(structured["auto_shared"], json!(false));
    assert_eq!(structured["auto_share_error"], json!("policy rejected"));
    assert!(structured.get("share").is_none());
}

#[test]
fn test_format_capsule_create_headline_surfaces_folder_path_resolution() {
    let project_id = Uuid::new_v4();
    let mut c = ContextCapsuleResponse::default();
    c.capsule_id = "cap_resolved".into();
    c.scope = Some(json!("project"));
    c.resolved_scope = Some(ContextCapsuleResolvedScope {
        resolution_method: "folder_path".into(),
        confidence: 0.95,
        project_id: Some(project_id),
        project_name: Some("mcp".into()),
        resolved_from: Some("/home/me/dev/mcp".into()),
        ..Default::default()
    });
    let headline = format_capsule_create_headline(&c);
    assert!(
        headline.contains("resolved=\"mcp\" (folder_path, confidence=0.95)"),
        "headline must surface folder_path resolution\noutput: {headline}"
    );
}

#[test]
fn test_format_capsule_create_headline_omits_resolution_for_explicit_project_id() {
    let mut c = ContextCapsuleResponse::default();
    c.capsule_id = "cap_explicit".into();
    c.scope = Some(json!("project"));
    c.resolved_scope = Some(ContextCapsuleResolvedScope {
        resolution_method: "explicit".into(),
        confidence: 1.0,
        project_id: Some(Uuid::new_v4()),
        ..Default::default()
    });
    let headline = format_capsule_create_headline(&c);
    assert!(
        !headline.contains("resolved="),
        "explicit project_id should not show a 'resolved=' chip\noutput: {headline}"
    );
}

#[test]
fn test_format_capsule_create_headline_surfaces_readiness_chip() {
    let mut c = ContextCapsuleResponse::default();
    c.capsule_id = "cap_ready".into();
    c.readiness = Some(ContextCapsuleReadiness {
        score: 0.81,
        label: "rich".into(),
        ..Default::default()
    });
    let headline = format_capsule_create_headline(&c);
    assert!(
        headline.contains("readiness=rich (0.81)"),
        "headline must surface readiness label and score\noutput: {headline}"
    );
}

#[test]
fn test_format_capsule_create_headline_omits_readiness_chip_when_absent() {
    let mut c = ContextCapsuleResponse::default();
    c.capsule_id = "cap_noreadiness".into();
    let headline = format_capsule_create_headline(&c);
    assert!(
        !headline.contains("readiness="),
        "headline must not show readiness chip when field absent\noutput: {headline}"
    );
}

#[test]
fn test_format_capsule_create_headline_falls_back_to_project_id_when_name_missing() {
    let project_id = Uuid::new_v4();
    let mut c = ContextCapsuleResponse::default();
    c.capsule_id = "cap_anon".into();
    c.resolved_scope = Some(ContextCapsuleResolvedScope {
        resolution_method: "folder_path".into(),
        confidence: 0.95,
        project_id: Some(project_id),
        project_name: None,
        ..Default::default()
    });
    let headline = format_capsule_create_headline(&c);
    assert!(
        headline.contains(&project_id.to_string()),
        "headline must fall back to project_id when name missing\noutput: {headline}"
    );
    assert!(headline.contains("(folder_path"));
}

#[test]
fn test_format_capsule_open_headline_matches_docs_shape() {
    let mut c = ContextCapsuleResponse::default();
    c.capsule_id = "cap_8f3c".into();
    c.scope = Some(json!({"kind": "project", "project_id": "p"}));
    c.expires_at = Some((chrono::Utc::now() + chrono::Duration::days(7)).to_rfc3339());
    for id in ["files", "decisions", "lessons", "rules", "skills", "tasks"] {
        c.sections.push(ContextCapsuleSection {
            id: id.into(),
            ..Default::default()
        });
    }
    let headline = format_capsule_open_headline(&c);
    assert!(headline.starts_with("✓ opened"));
    assert!(headline.contains("id=cap_8f3c"));
    assert!(headline.contains("scope=project"));
    assert!(headline.contains("expires in"));
    assert!(headline.contains("6 sections ready"));
}

#[test]
fn test_format_capsule_open_headline_no_expiry_omits_expires() {
    let mut c = ContextCapsuleResponse::default();
    c.capsule_id = "cap_z".into();
    let headline = format_capsule_open_headline(&c);
    assert!(headline.starts_with("✓ opened"));
    assert!(!headline.contains("expires"));
    assert!(headline.contains("0 sections ready"));
}

#[test]
fn test_format_share_headline_team_link() {
    let response = ContextCapsuleShareResponse {
        id: Uuid::nil(),
        audience: Some("team".into()),
        share_url: Some("https://contextstream.io/c/capsule_tok".into()),
        agent_url: Some("https://contextstream.io/api/v1/capsules/shares/capsule_tok".into()),
        expires_at: Some((chrono::Utc::now() + chrono::Duration::days(7)).to_rfc3339()),
        token_prefix: "capsule_".into(),
        ..Default::default()
    };
    let headline = format_share_headline("cap_8f3c", &response);
    assert!(headline.starts_with("✓ https://contextstream.io/c/capsule_tok"));
    assert!(headline.contains("authenticated team link"));
    assert!(headline.contains("expires in"));
}

#[test]
fn test_format_share_headline_external_agent_lists_both_urls() {
    let response = ContextCapsuleShareResponse {
        id: Uuid::nil(),
        audience: Some("external_agent".into()),
        share_url: Some("https://contextstream.io/c/capsule_tok".into()),
        agent_url: Some("https://contextstream.io/api/v1/capsules/shares/capsule_tok".into()),
        token_prefix: "capsule_".into(),
        ..Default::default()
    };
    let headline = format_share_headline("cap_8f3c", &response);
    assert_eq!(
        headline,
        "✓ Agent URL: https://contextstream.io/api/v1/capsules/shares/capsule_tok · \
         Dashboard URL: https://contextstream.io/c/capsule_tok"
    );
}

#[test]
fn test_format_share_headline_external_agent_falls_back_to_api_url() {
    let response = ContextCapsuleShareResponse {
        id: Uuid::nil(),
        audience: Some("external_agent".into()),
        share_url: Some("https://contextstream.io/c/capsule_tok".into()),
        agent_url: None,
        api_url: Some("https://contextstream.io/api/v1/capsules/shares/capsule_tok".into()),
        token_prefix: "capsule_".into(),
        ..Default::default()
    };
    let headline = format_share_headline("cap_8f3c", &response);
    assert!(
        headline.contains("Agent URL: https://contextstream.io/api/v1/capsules/shares/capsule_tok")
    );
}

#[test]
fn test_format_share_headline_public_link_marks_policy() {
    let response = ContextCapsuleShareResponse {
        id: Uuid::nil(),
        audience: Some("public_link".into()),
        share_url: Some("https://contextstream.io/c/capsule_tok".into()),
        single_use: true,
        max_uses: Some(1),
        use_count: 0,
        token_prefix: "capsule_".into(),
        ..Default::default()
    };
    let headline = format_share_headline("cap_8f3c", &response);
    assert!(headline.starts_with("✓ https://contextstream.io/c/capsule_tok"));
    assert!(headline.contains("public_link"));
    assert!(headline.contains("single-use, unread"));
}

#[test]
fn test_format_graph_headline_matches_docs_shape() {
    let v = json!({
        "nodes": [{"id": 1}, {"id": 2}, {"id": 3}],
        "edges": [{"src": 1}, {"src": 2}],
    });
    assert_eq!(
        format_graph_headline(&v),
        "✓ 3 nodes · 2 edges · returned as JSON"
    );
}

#[test]
fn test_format_list_shares_headline_capsule_scope_omits_target() {
    let shares = vec![
        ContextCapsuleShareResponse {
            id: Uuid::nil(),
            single_use: true,
            use_count: 0,
            token_prefix: "capsule_".into(),
            ..Default::default()
        },
        ContextCapsuleShareResponse {
            id: Uuid::nil(),
            single_use: false,
            token_prefix: "capsule_".into(),
            ..Default::default()
        },
        ContextCapsuleShareResponse {
            id: Uuid::nil(),
            single_use: false,
            revoked_at: Some("2026-04-29T00:00:00Z".into()),
            token_prefix: "capsule_".into(),
            ..Default::default()
        },
    ];
    let headline = format_list_shares_headline(&shares, Some("cap_8f3c"), None, None);
    assert_eq!(
        headline,
        "✓ 3 shares · 1 single-use, unread · 1 multi-use · 1 revoked"
    );
    assert!(!headline.contains("for "));
}

#[test]
fn test_format_list_shares_headline_scope_target_when_capsule_missing() {
    let headline = format_list_shares_headline(&[], None, Some("proj_123"), None);
    assert!(headline.contains("for project proj_123"));
    assert!(headline.contains("0 single-use, unread"));
    assert!(headline.contains("0 revoked"));
}

#[test]
fn test_format_revoke_share_text_includes_410_gone_and_url() {
    let response = ContextCapsuleShareResponse {
        id: Uuid::nil(),
        share_url: Some("https://contextstream.io/c/capsule_tok".into()),
        token_prefix: "capsule_".into(),
        revoked_at: Some("2026-04-29T00:00:00Z".into()),
        ..Default::default()
    };
    let text = format_revoke_share_text(&response);
    assert!(
        text.starts_with("✓ revoked · subsequent reads return 410 Gone"),
        "lead line must match docs format\noutput: {text}"
    );
    assert!(text.contains("https://contextstream.io/c/capsule_tok"));
}

#[test]
fn test_format_revoke_share_text_without_url_keeps_headline_only() {
    let response = ContextCapsuleShareResponse::default();
    let text = format_revoke_share_text(&response);
    assert_eq!(text, "✓ revoked · subsequent reads return 410 Gone");
}

#[test]
fn test_format_expires_humanized_handles_future_present_past() {
    let future = (chrono::Utc::now() + chrono::Duration::days(7)).to_rfc3339();
    let past = (chrono::Utc::now() - chrono::Duration::days(2)).to_rfc3339();
    assert!(format_expires_humanized(Some(&future)).starts_with("in "));
    assert!(format_expires_humanized(Some(&past)).starts_with("expired "));
    assert_eq!(format_expires_humanized(None), "no expiry");
    assert_eq!(format_expires_humanized(Some("")), "no expiry");
    assert_eq!(
        format_expires_humanized(Some("not-a-timestamp")),
        "not-a-timestamp"
    );
}

#[test]
fn test_scope_summary_extracts_kind_and_falls_back() {
    assert_eq!(
        scope_summary(&Some(json!("project"))).as_deref(),
        Some("project")
    );
    assert_eq!(
        scope_summary(&Some(json!({"kind": "workspace"}))).as_deref(),
        Some("workspace")
    );
    assert_eq!(
        scope_summary(&Some(json!({"type": "session", "project_id": "p"}))).as_deref(),
        Some("session")
    );
    assert_eq!(
        scope_summary(&Some(json!({"project_id": "p"}))).as_deref(),
        Some("project")
    );
    assert_eq!(scope_summary(&None), None);
    assert_eq!(scope_summary(&Some(json!(""))), None);
}

#[test]
fn test_render_bootstrap_prompt_matches_dashboard_layout() {
    let mut c = ContextCapsuleResponse::default();
    c.capsule_id = "cap_x".into();
    c.name = Some("Sara handoff".into());
    c.bootstrap = Some(json!({
        "summary": "Top-level rule-based summary",
        "recommended_first_actions": ["fallback action"],
        "llm_overview": {
            "summary": "LLM executive summary",
            "recommended_first_actions": ["Read README", "Run tests", "Open PR #42"],
        }
    }));
    c.sections.push(ContextCapsuleSection {
        id: "decisions".into(),
        title: "Decisions".into(),
        summary: "Recent product decisions".into(),
        item_count: Some(3),
        data: Some(json!({
            "items": [
                {"title": "Pick Atlas Search over OpenSearch", "content": "Cheaper and AWS-native."},
                {"title": "Drop OCI experiment", "content": "Infra is AWS only."},
                {"title": "Adopt search-first", "content": "Hard gate broad Glob/Grep behind ContextStream search."},
            ]
        })),
        ..Default::default()
    });
    c.sections.push(ContextCapsuleSection {
        id: "lessons".into(),
        title: "Lessons".into(),
        item_count: Some(2),
        data: Some(json!({
            "items": [
                {"title": "Never ssh heredocs", "content": "They mangle quoting."},
                {"title": "AWS only", "content": "Don't suggest OCI."},
            ]
        })),
        ..Default::default()
    });
    // A noisy index section that should NOT inflate the body — should
    // appear under "Index summary" with a single-line count.
    c.sections.push(ContextCapsuleSection {
        id: "file_catalog".into(),
        title: "File catalog".into(),
        item_count: Some(4_217),
        chunk_ids: vec!["chunk-1".into(), "chunk-2".into()],
        ..Default::default()
    });
    // An empty section that should be skipped entirely.
    c.sections.push(ContextCapsuleSection {
        id: "empty_one".into(),
        title: "Empty".into(),
        item_count: Some(0),
        ..Default::default()
    });

    let prompt = render_bootstrap_prompt(&c);

    assert!(prompt.starts_with("# Sara handoff\n"));
    assert!(prompt.contains("## Summary\n\nLLM executive summary\n"));
    assert!(prompt.contains("## Recommended First Actions"));
    assert!(prompt.contains("1. Read README"));
    assert!(prompt.contains("3. Open PR #42"));
    assert!(prompt.contains("## Sections"));
    assert!(prompt.contains("### Decisions"));
    assert!(prompt.contains("Recent product decisions"));
    assert!(prompt.contains("Items: 3"));
    assert!(prompt.contains("#### Pick Atlas Search over OpenSearch"));
    assert!(prompt.contains("Cheaper and AWS-native."));
    assert!(prompt.contains("### Lessons"));
    assert!(prompt.contains("## Index summary"));
    assert!(prompt.contains("- **File catalog** — 4217 items, 2 chunks"));
    assert!(!prompt.contains("### Empty"));
    assert!(prompt.ends_with('\n'));
}

#[test]
fn test_render_bootstrap_prompt_uses_capsule_id_when_name_blank() {
    let mut c = ContextCapsuleResponse::default();
    c.capsule_id = "cap_only".into();
    let prompt = render_bootstrap_prompt(&c);
    assert!(prompt.starts_with("# cap_only\n"));
    assert!(!prompt.contains("## Summary"));
    assert!(!prompt.contains("## Sections"));
    assert!(!prompt.contains("## Index summary"));
}

#[test]
fn test_render_bootstrap_prompt_caps_high_budget_section_with_overflow_marker() {
    let mut items = Vec::new();
    for i in 0..40 {
        items.push(json!({
            "title": format!("Decision {i}"),
            "content": format!("Body for decision {i}")
        }));
    }
    let mut c = ContextCapsuleResponse::default();
    c.capsule_id = "cap_y".into();
    c.sections.push(ContextCapsuleSection {
        id: "decisions".into(),
        title: "Decisions".into(),
        item_count: Some(40),
        data: Some(json!({"items": items})),
        ..Default::default()
    });
    let prompt = render_bootstrap_prompt(&c);
    // High-budget cap is 25 items — expect 25 rendered + overflow marker for 15.
    assert!(prompt.contains("#### Decision 24"));
    assert!(!prompt.contains("#### Decision 25"));
    assert!(prompt.contains("_…15 more"));
}

#[test]
fn test_render_bootstrap_prompt_falls_back_to_compact_json_for_unknown_item_shape() {
    let mut c = ContextCapsuleResponse::default();
    c.capsule_id = "cap_z".into();
    c.sections.push(ContextCapsuleSection {
        id: "skills".into(),
        title: "Skills".into(),
        item_count: Some(1),
        data: Some(json!({
            "items": [
                {"weird_field": "no title or body keys"}
            ]
        })),
        ..Default::default()
    });
    let prompt = render_bootstrap_prompt(&c);
    assert!(prompt.contains("weird_field"));
}

#[test]
fn test_format_bootstrap_prompt_headline_counts_only_narrative_sections() {
    let mut c = ContextCapsuleResponse::default();
    c.capsule_id = "cap_8f3c".into();
    c.sections.push(ContextCapsuleSection {
        id: "decisions".into(),
        item_count: Some(3),
        ..Default::default()
    });
    c.sections.push(ContextCapsuleSection {
        id: "lessons".into(),
        item_count: Some(2),
        ..Default::default()
    });
    c.sections.push(ContextCapsuleSection {
        id: "file_catalog".into(),
        item_count: Some(4_217),
        ..Default::default()
    });
    let prompt = "abcdefgh".repeat(128); // 1024 chars
    let headline = format_bootstrap_prompt_headline(&c, &prompt);
    assert!(headline.starts_with("✓ bootstrap prompt"));
    assert!(headline.contains("id=cap_8f3c"));
    // 2 narrative sections (decisions, lessons) — file_catalog is noise.
    assert!(
        headline.contains("2 sections"),
        "expected 2 sections (narrative-only) — got: {headline}"
    );
    assert!(headline.contains("~256 tokens"));
    assert!(headline.contains("1024 chars"));
}

#[test]
fn test_truncate_block_is_unicode_safe() {
    // Pre-fix this would panic on a non-char-boundary slice.
    let multi = "あいうえおかきくけこ".to_string();
    let out = truncate_block(&multi, 5);
    assert_eq!(out.chars().count(), 5);
    assert!(out.ends_with('…'));
}

#[test]
fn test_format_share_result_text_team_marks_auth_required() {
    let response = ContextCapsuleShareResponse {
        id: Uuid::nil(),
        audience: Some("team".into()),
        share_url: Some("https://ctx.example/c/capsule_tok".into()),
        agent_url: Some("https://ctx.example/api/v1/capsules/shares/capsule_tok".into()),
        single_use: false,
        max_uses: None,
        use_count: 2,
        token_prefix: "capsule_".into(),
        ..Default::default()
    };
    let s = format_share_result_text("cap_x", &response);
    assert!(s.contains("URL: https://ctx.example/c/capsule_tok"));
    assert!(s.contains(
        "Agent URL (requires Authorization): https://ctx.example/api/v1/capsules/shares/capsule_tok"
    ));
    assert!(s.contains("authenticated team link"));
    assert!(s.contains("Share policy: authenticated multi-use"));
    assert!(!s.contains("Agent URL (paste into LLMs)"));
}

// Hosted sessions keep the scope `init` resolved in session state, or carry it
// in the connection's headers, never in the process config the API client
// falls back to (#189). These tests run inside the per-request auth scope the
// hosted gateway sets, which also stops `update_scope` from writing process
// defaults.

fn hosted_capsule_tool(base_url: &str) -> (CapsuleTool, Arc<mcp_session::SessionManager>) {
    let mut config = TestFixtures::test_config();
    config.api_url = base_url.to_string();
    config.default_workspace_id = None;
    config.default_project_id = None;
    config.is_http_transport = true;
    let client = ContextStreamClient::new(config.clone());
    let session = Arc::new(mcp_session::SessionManager::new(client.clone(), config));
    (
        CapsuleTool::new(client).with_session(session.clone()),
        session,
    )
}

fn hosted_auth(workspace: Option<Uuid>) -> mcp_types::config::AuthOverride {
    mcp_types::config::AuthOverride {
        api_key: Some("test-key".to_string()),
        workspace_id: workspace,
        ..Default::default()
    }
}

fn requested(probe: &RequestProbe, path: &str) -> bool {
    let expected = format!("GET /api/v1{path}");
    probe
        .request_lines()
        .iter()
        .any(|line| line.starts_with(&expected))
}

#[tokio::test]
async fn list_uses_the_project_that_init_resolved_in_the_session() {
    let probe = RequestProbe::answering("[]").await;
    let (tool, session) = hosted_capsule_tool(&probe.base_url);
    let (workspace, project) = (Uuid::new_v4(), Uuid::new_v4());
    mcp_client::run_with_auth_override(hosted_auth(None), || async {
        session
            .update_scope(Some(workspace), Some(project), None)
            .await;
        tool.execute(json!({"action": "list"})).await
    })
    .await
    .expect("capsule list with the session's scope");
    assert!(
        requested(&probe, &format!("/projects/{project}/capsules")),
        "{:?}",
        probe.request_lines()
    );
}

#[tokio::test]
async fn list_uses_the_workspace_header_when_the_session_has_no_scope() {
    let probe = RequestProbe::answering("[]").await;
    let (tool, _session) = hosted_capsule_tool(&probe.base_url);
    let workspace = Uuid::new_v4();
    mcp_client::run_with_auth_override(hosted_auth(Some(workspace)), || {
        tool.execute(json!({"action": "list"}))
    })
    .await
    .expect("capsule list with the header's workspace");
    assert!(
        requested(&probe, &format!("/workspaces/{workspace}/capsules")),
        "{:?}",
        probe.request_lines()
    );
}

#[tokio::test]
async fn list_in_another_workspace_does_not_borrow_the_sessions_project() {
    let probe = RequestProbe::answering("[]").await;
    let (tool, session) = hosted_capsule_tool(&probe.base_url);
    let (workspace, project, other) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    mcp_client::run_with_auth_override(hosted_auth(None), || async {
        session
            .update_scope(Some(workspace), Some(project), None)
            .await;
        tool.execute(json!({"action": "list", "workspace_id": other}))
            .await
    })
    .await
    .expect("capsule list for another workspace");
    assert!(
        requested(&probe, &format!("/workspaces/{other}/capsules")),
        "{:?}",
        probe.request_lines()
    );
    assert!(!requested(&probe, &format!("/projects/{project}/capsules")));
}

#[tokio::test]
async fn list_without_any_scope_still_asks_for_one() {
    let probe = RequestProbe::start().await;
    let (tool, _session) = hosted_capsule_tool(&probe.base_url);
    let error = mcp_client::run_with_auth_override(hosted_auth(None), || {
        tool.execute(json!({"action": "list"}))
    })
    .await
    .expect_err("no scope anywhere")
    .to_string();
    assert!(error.contains("workspace_id or project_id"), "{error}");
    assert!(probe.request_lines().is_empty());
}

#[tokio::test]
async fn create_fills_the_session_scope_the_call_omits() {
    let probe = RequestProbe::start().await;
    let (tool, session) = hosted_capsule_tool(&probe.base_url);
    let (workspace, project) = (Uuid::new_v4(), Uuid::new_v4());
    // The stub's answer is not a capsule, so the call itself fails; the request
    // it sent is what matters.
    let _ = mcp_client::run_with_auth_override(hosted_auth(None), || async {
        session
            .update_scope(Some(workspace), Some(project), None)
            .await;
        tool.execute(json!({"action": "create", "purpose": "handoff"}))
            .await
    })
    .await;
    let body = probe
        .body_of("POST /api/v1/capsules")
        .expect("create request");
    assert_eq!(body["workspace_id"], json!(workspace));
    assert_eq!(body["project_id"], json!(project));
    assert_eq!(body["scope"], "project");
}

#[tokio::test]
async fn create_by_project_name_keeps_the_name_instead_of_the_sessions_project() {
    let probe = RequestProbe::start().await;
    let (tool, session) = hosted_capsule_tool(&probe.base_url);
    let (workspace, project) = (Uuid::new_v4(), Uuid::new_v4());
    let _ = mcp_client::run_with_auth_override(hosted_auth(None), || async {
        session
            .update_scope(Some(workspace), Some(project), None)
            .await;
        tool.execute(json!({"action": "create", "project_name": "other-project"}))
            .await
    })
    .await;
    let body = probe
        .body_of("POST /api/v1/capsules")
        .expect("create request");
    assert_eq!(body["workspace_id"], json!(workspace));
    assert_eq!(body["project_name"], "other-project");
    assert!(body.get("project_id").is_none(), "{body}");
}

fn input_with(fields: serde_json::Value) -> CapsuleInput {
    let mut value = json!({"action": "list"});
    for (key, field) in fields.as_object().unwrap() {
        value[key] = field.clone();
    }
    serde_json::from_value(value).unwrap()
}

#[test]
fn fill_scope_defaults_only_fills_what_the_call_omits() {
    let (workspace, project) = (Uuid::new_v4(), Uuid::new_v4());
    let (named_workspace, named_project) = (Uuid::new_v4(), Uuid::new_v4());

    let mut input = input_with(json!({}));
    fill_scope_defaults(&mut input, (Some(workspace), Some(project)));
    assert_eq!(input.workspace_id, Some(workspace.to_string()));
    assert_eq!(input.project_id, Some(project.to_string()));

    // Ids the call names are kept, and naming the session's own workspace
    // still takes its project.
    let mut input = input_with(json!({
        "workspace_id": workspace.to_string(),
        "project_id": named_project.to_string(),
    }));
    fill_scope_defaults(&mut input, (Some(workspace), Some(project)));
    assert_eq!(input.project_id, Some(named_project.to_string()));
    let mut input = input_with(json!({"workspace_id": workspace.to_string()}));
    fill_scope_defaults(&mut input, (Some(workspace), Some(project)));
    assert_eq!(input.project_id, Some(project.to_string()));

    // Another workspace never inherits the session's project.
    let mut input = input_with(json!({"workspace_id": named_workspace.to_string()}));
    fill_scope_defaults(&mut input, (Some(workspace), Some(project)));
    assert_eq!(input.workspace_id, Some(named_workspace.to_string()));
    assert_eq!(input.project_id, None);

    // A malformed workspace_id is left for the call to reject.
    let mut input = input_with(json!({"workspace_id": "not-a-uuid"}));
    fill_scope_defaults(&mut input, (Some(workspace), Some(project)));
    assert_eq!(input.workspace_id.as_deref(), Some("not-a-uuid"));
    assert_eq!(input.project_id, None);

    // A folder or a project name picks its own project.
    for hint in [
        json!({"folder_path": "/work/app"}),
        json!({"project_name": "app"}),
    ] {
        let mut input = input_with(hint);
        fill_scope_defaults(&mut input, (Some(workspace), Some(project)));
        assert_eq!(input.workspace_id, Some(workspace.to_string()));
        assert_eq!(input.project_id, None);
    }

    // No session scope: nothing to fill.
    let mut input = input_with(json!({}));
    fill_scope_defaults(&mut input, (None, None));
    assert_eq!((input.workspace_id, input.project_id), (None, None));
}

#[test]
fn only_actions_that_read_a_scope_get_the_session_defaults() {
    for action in ["create", "list", "list_shares", "audit", "primer"] {
        assert!(SCOPE_DEFAULTED_ACTIONS.contains(&action), "{action}");
    }
    // `open` keys its cache on the workspace a call names; the session's must
    // not switch that cache on.
    for action in ["open", "get", "delete", "share", "revoke_share"] {
        assert!(!SCOPE_DEFAULTED_ACTIONS.contains(&action), "{action}");
    }
}

#[test]
fn share_text_shows_numbers_and_times_not_debug_values() {
    // #362: max_uses=Some(2), consumed_at=Some("…").
    let response = ContextCapsuleShareResponse {
        id: Uuid::nil(),
        audience: Some("external_agent".into()),
        single_use: false,
        max_uses: Some(2),
        use_count: 1,
        consumed_at: Some("2026-10-30T10:17:10.761665Z".into()),
        expires_at: Some("2026-10-31T10:17:04.424Z".into()),
        token_prefix: "capsule_62d6".into(),
        ..Default::default()
    };
    let text = format_share_result_text("cap_x", &response);
    assert!(
        text.contains("Share policy: multi-use (max_uses=2, use_count=1)"),
        "{text}"
    );
    let list =
        format_list_shares_summary(std::slice::from_ref(&response), Some("cap_x"), None, None);
    assert!(
        list.contains("max_uses=2 | use_count=1 | consumed_at=2026-10-30T10:17:10Z | expires_at=2026-10-31T10:17:04Z"),
        "{list}"
    );
    for text in [&text, &list] {
        assert!(!text.contains("Some("), "{text}");
    }
    assert_eq!(share_limit(None), "unlimited");
    assert_eq!(share_time(None), "none");
    assert_eq!(share_time(Some("not a time")), "not a time");
}

// ─────────────────────────────── Live Capsule ────────────────────────────────

/// One payload that reads as both a capsule (create) and a share (share), so a
/// single stub answers the two calls `action="live"` makes.
const LIVE_STUB: &str = r#"{"capsule_id":"cap_live1","name":"Acme","id":"22222222-2222-4222-8222-222222222222","audience":"external_agent","token_prefix":"capsule_t","share_url":"https://ctx.example/c/capsule_tok","agent_url":"https://api.ctx.example/api/v1/capsules/shares/capsule_tok","live_url":"https://ctx.example/c/capsule_tok/live","mcp_url":"https://ctx.example/c/capsule_tok/mcp","expires_at":"2026-11-27T20:00:00Z","single_use":false}"#;

fn text_of(result: &ToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|item| match item {
            mcp_types::tool::ContentItem::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn posted(probe: &RequestProbe, path_suffix: &str) -> Value {
    probe
        .request_lines()
        .iter()
        .find(|line| line.starts_with("POST /api/v1/capsules") && line.contains(path_suffix))
        .and_then(|line| probe.body_of(line.split(' ').nth(1).unwrap_or_default()))
        .unwrap_or_else(|| panic!("no POST {path_suffix}: {:?}", probe.request_lines()))
}

#[tokio::test]
async fn live_makes_a_capsule_and_one_multi_use_link_any_ai_app_reads() {
    let probe = RequestProbe::answering(LIVE_STUB).await;
    let (tool, session) = hosted_capsule_tool(&probe.base_url);
    let (workspace, project) = (Uuid::new_v4(), Uuid::new_v4());
    let result = mcp_client::run_with_auth_override(hosted_auth(None), || async {
        session
            .update_scope(Some(workspace), Some(project), None)
            .await;
        tool.execute(json!({"action": "live", "name": "Acme", "notes": "Start with the API crate"}))
            .await
    })
    .await
    .expect("live capsule");

    let capsule = posted(&probe, "/capsules ");
    assert_eq!(capsule["scope"], "project");
    assert_eq!(capsule["project_id"], project.to_string());
    assert_eq!(capsule["workspace_id"], workspace.to_string());
    assert_eq!(capsule["name"], "Acme");
    assert_eq!(capsule["purpose"], "external_agent");
    assert_eq!(capsule["audience"], "external_agent");
    assert_eq!(capsule["include_code"], "none");
    assert_eq!(capsule["include_personal"], false);
    assert_eq!(capsule["redaction_level"], "standard");
    assert_eq!(capsule["permissions"], "read_only");
    assert_eq!(capsule["notes"], "Start with the API crate");

    let share = posted(&probe, "/cap_live1/share");
    assert_eq!(share["audience"], "external_agent");
    assert_eq!(share["multi_use"], true);
    assert_eq!(share["expires_in_days"], 7);
    assert_eq!(share["include_code"], "none");
    assert_eq!(share["include_personal"], false);
    assert_eq!(share["redaction_level"], "standard");
    assert!(share.get("max_uses").is_none(), "{share}");
    assert!(share.get("allow_risky_policy").is_none(), "{share}");

    let text = text_of(&result);
    assert!(
        text.contains("✓ Live Capsule ready · \"Live Capsule - Acme\""),
        "{text}"
    );
    assert!(text.contains(
        "Live link (paste into ChatGPT or Claude): https://ctx.example/c/capsule_tok/live"
    ));
    assert!(text.contains("https://ctx.example/c/capsule_tok/mcp"));
    assert!(text.contains("Connector name: Live Capsule - Acme"));
    assert!(text.contains("Open this link and use it to understand the project \"Acme\":\nhttps://ctx.example/c/capsule_tok/live"));
    assert!(text.contains(
        "claude mcp add --transport http live-capsule-acme https://ctx.example/c/capsule_tok/mcp"
    ));
    assert!(text.contains("gemini mcp add --transport http live-capsule-acme"));
    assert!(
        text.contains("delete(action=\"capsule.revoke_share\""),
        "{text}"
    );
    assert!(text.contains("no file contents"));

    let structured = result.structured_content.expect("structured");
    assert_eq!(structured["kind"], "live_capsule");
    assert_eq!(structured["capsule_id"], "cap_live1");
    assert_eq!(
        structured["live_url"],
        "https://ctx.example/c/capsule_tok/live"
    );
    assert_eq!(
        structured["mcp_url"],
        "https://ctx.example/c/capsule_tok/mcp"
    );
    assert_eq!(structured["connector_name"], "Live Capsule - Acme");
    assert_eq!(
        structured["mcp_config"]["mcpServers"]["live-capsule-acme"]["url"],
        "https://ctx.example/c/capsule_tok/mcp"
    );
    assert_eq!(structured["policy"]["multi_use"], true);
    assert!(structured["paste_prompt"]
        .as_str()
        .unwrap()
        .contains("https://ctx.example/c/capsule_tok/live"));
}

#[tokio::test]
async fn live_without_a_name_is_named_like_the_web_app_and_the_connector_is_plain() {
    let probe = RequestProbe::answering(LIVE_STUB).await;
    let (tool, session) = hosted_capsule_tool(&probe.base_url);
    let (workspace, project) = (Uuid::new_v4(), Uuid::new_v4());
    let result = mcp_client::run_with_auth_override(hosted_auth(None), || async {
        session
            .update_scope(Some(workspace), Some(project), None)
            .await;
        tool.execute(json!({"action": "live", "project_name": "Orbit"}))
            .await
    })
    .await
    .expect("live capsule");
    assert_eq!(posted(&probe, "/capsules ")["name"], "Orbit · live");
    assert_eq!(posted(&probe, "/share")["name"], "Orbit · live");
    let structured = result.structured_content.expect("structured");
    assert_eq!(structured["connector_name"], "Live Capsule");
    assert_eq!(structured["server_key"], "live-capsule");
}

#[tokio::test]
async fn live_refuses_code_personal_items_or_a_long_link_unless_asked() {
    for extra in [
        json!({"include_code": "lazy"}),
        json!({"include_personal": true}),
        json!({"expires_in_days": 30}),
    ] {
        let probe = RequestProbe::answering(LIVE_STUB).await;
        let (tool, _session) = hosted_capsule_tool(&probe.base_url);
        let mut input =
            json!({"action": "live", "workspace_id": Uuid::new_v4(), "project_id": Uuid::new_v4()});
        input
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let error = mcp_client::run_with_auth_override(hosted_auth(None), || tool.execute(input))
            .await
            .expect_err("risky without opt-in")
            .to_string();
        assert!(error.contains("allow_risky_policy=true"), "{error}");
        assert!(
            probe.request_lines().is_empty(),
            "{:?}",
            probe.request_lines()
        );
    }
}

#[tokio::test]
async fn live_with_the_users_opt_in_sends_it_to_the_api() {
    let probe = RequestProbe::answering(LIVE_STUB).await;
    let (tool, _session) = hosted_capsule_tool(&probe.base_url);
    let input = json!({
        "action": "live", "name": "Acme",
        "workspace_id": Uuid::new_v4(), "project_id": Uuid::new_v4(),
        "include_code": "lazy", "expires_in_days": 30, "allow_risky_policy": true
    });
    let result = mcp_client::run_with_auth_override(hosted_auth(None), || tool.execute(input))
        .await
        .expect("opted in");
    let share = posted(&probe, "/share");
    assert_eq!(share["include_code"], "lazy");
    assert_eq!(share["expires_in_days"], 30);
    assert_eq!(share["allow_risky_policy"], true);
    assert!(text_of(&result).contains("files and code"));
}

#[tokio::test]
async fn live_for_the_workspace_leaves_the_sessions_project_out() {
    let probe = RequestProbe::answering(LIVE_STUB).await;
    let (tool, session) = hosted_capsule_tool(&probe.base_url);
    let (workspace, project) = (Uuid::new_v4(), Uuid::new_v4());
    mcp_client::run_with_auth_override(hosted_auth(None), || async {
        session
            .update_scope(Some(workspace), Some(project), None)
            .await;
        tool.execute(json!({"action": "live", "scope": "workspace", "name": "Team"}))
            .await
    })
    .await
    .expect("workspace live capsule");
    let capsule = posted(&probe, "/capsules ");
    assert_eq!(capsule["scope"], "workspace");
    assert_eq!(capsule["workspace_id"], workspace.to_string());
    assert!(capsule.get("project_id").is_none(), "{capsule}");
}

#[tokio::test]
async fn live_rejects_a_session_scope_and_days_out_of_range() {
    let probe = RequestProbe::answering(LIVE_STUB).await;
    let (tool, _session) = hosted_capsule_tool(&probe.base_url);
    for (input, expected) in [
        (
            json!({"action": "live", "scope": "session", "project_id": Uuid::new_v4(), "workspace_id": Uuid::new_v4()}),
            "session handoff",
        ),
        (
            json!({"action": "live", "expires_in_days": 0, "project_id": Uuid::new_v4(), "workspace_id": Uuid::new_v4()}),
            "between 1 and 30",
        ),
        (
            json!({"action": "live", "expires_in_days": 31, "project_id": Uuid::new_v4(), "workspace_id": Uuid::new_v4(), "allow_risky_policy": true}),
            "between 1 and 30",
        ),
        (
            json!({"action": "live", "include_code": "everything", "project_id": Uuid::new_v4(), "workspace_id": Uuid::new_v4()}),
            "include_code must be",
        ),
    ] {
        let error = mcp_client::run_with_auth_override(hosted_auth(None), || tool.execute(input))
            .await
            .expect_err("invalid")
            .to_string();
        assert!(error.contains(expected), "{error}");
    }
    assert!(probe.request_lines().is_empty());
}

#[tokio::test]
async fn live_read_follows_a_pasted_page_link() {
    let probe = RequestProbe::answering("# src/a.rs\n").await;
    let (tool, _session) = hosted_capsule_tool(&probe.base_url);
    let result = mcp_client::run_with_auth_override(hosted_auth(None), || {
        tool.execute(json!({
            "action": "live_read",
            "url": "https://contextstream.io/c/capsule_tok/live/file?path=src%2Fa.rs&start=10"
        }))
    })
    .await
    .expect("live page");
    assert_eq!(text_of(&result), "# src/a.rs\n");
    assert_eq!(
        probe.request_lines(),
        vec!["GET /api/v1/capsules/shares/capsule_tok/live/file?path=src%2Fa.rs&start=10 HTTP/1.1"]
    );
    assert_eq!(
        probe.header_of("/live/file", "accept").as_deref(),
        Some("text/markdown")
    );
    assert_eq!(result.structured_content.unwrap()["page"], "file");
}

#[tokio::test]
async fn live_read_fields_win_over_the_link_and_send_the_unlock_key() {
    let probe = RequestProbe::answering("ok").await;
    let (tool, _session) = hosted_capsule_tool(&probe.base_url);
    mcp_client::run_with_auth_override(hosted_auth(None), || {
        tool.execute(json!({
            "action": "live_read",
            "share_token": "capsule_tok",
            "page": "search",
            "query": "token refresh & retry",
            "limit": 5,
            "unlock_key": "ck_secret"
        }))
    })
    .await
    .expect("live search");
    assert_eq!(
        probe.request_lines(),
        vec!["GET /api/v1/capsules/shares/capsule_tok/live/search?q=token%20refresh%20%26%20retry&limit=5 HTTP/1.1"]
    );
    assert_eq!(
        probe.header_of("/live/search", "x-capsule-key").as_deref(),
        Some("ck_secret")
    );
}

#[tokio::test]
async fn open_on_a_live_or_mcp_link_reads_the_live_home() {
    for url in [
        "https://contextstream.io/c/capsule_tok/live",
        "https://contextstream.io/c/capsule_tok/mcp",
    ] {
        let probe = RequestProbe::answering("# Live Capsule").await;
        let (tool, _session) = hosted_capsule_tool(&probe.base_url);
        let result = mcp_client::run_with_auth_override(hosted_auth(None), || {
            tool.execute(json!({"action": "open", "url": url}))
        })
        .await
        .expect("live home");
        assert_eq!(text_of(&result), "# Live Capsule");
        assert_eq!(
            probe.request_lines(),
            vec!["GET /api/v1/capsules/shares/capsule_tok/live HTTP/1.1"]
        );
    }
}

#[test]
fn live_page_paths_carry_only_what_each_page_reads() {
    use mcp_client::{capsule_live_path, CapsuleLiveParams};
    let params = |page: &str| CapsuleLiveParams {
        share: "https://contextstream.io/c/capsule_tok/live".to_string(),
        page: Some(page.to_string()),
        path: Some("src".to_string()),
        query: Some("Auth".to_string()),
        kind: Some("decisions".to_string()),
        id: Some("decision:6f1c".to_string()),
        ..Default::default()
    };
    assert_eq!(
        capsule_live_path(&params("home")).unwrap(),
        "/capsules/shares/capsule_tok/live"
    );
    assert_eq!(
        capsule_live_path(&params("tree")).unwrap(),
        "/capsules/shares/capsule_tok/live/tree?path=src"
    );
    assert_eq!(
        capsule_live_path(&params("symbols")).unwrap(),
        "/capsules/shares/capsule_tok/live/symbols?q=Auth"
    );
    assert_eq!(
        capsule_live_path(&params("knowledge")).unwrap(),
        "/capsules/shares/capsule_tok/live/knowledge?kind=decisions&q=Auth"
    );
    assert_eq!(
        capsule_live_path(&params("item")).unwrap(),
        "/capsules/shares/capsule_tok/live/item?id=decision%3A6f1c"
    );
    assert!(capsule_live_path(&params("graph"))
        .unwrap_err()
        .to_string()
        .contains("Pages: home, tree"));
    let bare = |page: &str| CapsuleLiveParams {
        share: "capsule_tok".to_string(),
        page: Some(page.to_string()),
        ..Default::default()
    };
    for (page, needed) in [
        ("file", "path is required"),
        ("search", "query is required"),
        ("item", "id is required"),
    ] {
        assert!(
            capsule_live_path(&bare(page))
                .unwrap_err()
                .to_string()
                .contains(needed),
            "{page}"
        );
    }
}

#[test]
fn live_connector_names_match_the_api_and_web_app() {
    use super::live::{live_connector_name, live_server_key};
    assert_eq!(live_connector_name(None), "Live Capsule");
    assert_eq!(live_connector_name(Some("  ")), "Live Capsule");
    assert_eq!(live_connector_name(Some("Orbit · live")), "Live Capsule");
    assert_eq!(live_connector_name(Some("Orbit - Live")), "Live Capsule");
    assert_eq!(live_connector_name(Some("Orbit")), "Live Capsule - Orbit");
    assert_eq!(live_connector_name(Some("Olive")), "Live Capsule - Olive");
    let long = live_connector_name(Some("A very long project name that keeps going"));
    assert_eq!(long.chars().count(), 40);
    assert!(long.ends_with('…'));
    assert_eq!(live_server_key(Some("Acme API")), "live-capsule-acme-api");
    assert_eq!(live_server_key(None), "live-capsule");
}

#[test]
fn every_share_result_names_its_live_and_mcp_links() {
    let mut share = ContextCapsuleShareResponse {
        id: Uuid::new_v4(),
        audience: Some("external_agent".to_string()),
        share_url: Some("https://ctx.example/c/capsule_tok".to_string()),
        ..Default::default()
    };
    // An API that predates live_url still serves the index under the link.
    let text = format_share_result_text("cap_1", &share);
    assert!(
        text.contains(
            "Live link (paste into ChatGPT or Claude): https://ctx.example/c/capsule_tok/live"
        ),
        "{text}"
    );
    assert!(text.contains("MCP link (add as a connector in Claude, ChatGPT, Gemini, Cursor): https://ctx.example/c/capsule_tok/mcp"));
    assert!(!text.contains("single-use, so its live index"));

    share.live_url = Some("https://eu.ctx.example/c/capsule_tok/live".to_string());
    share.single_use = true;
    share.max_uses = Some(1);
    let text = format_share_result_text("cap_1", &share);
    assert!(text.contains("https://eu.ctx.example/c/capsule_tok/live"));
    assert!(text.contains("single-use, so its live index"));
    assert!(text.contains("capsule(action=\"live\")"));

    share.audience = Some("bootstrap_link".to_string());
    assert!(!format_share_result_text("cap_1", &share).contains("Live link"));
}

#[test]
fn the_tool_tells_agents_when_to_make_and_read_a_live_capsule() {
    let tool = CapsuleTool::new(create_mock_client());
    let description = &tool.metadata().description;
    assert!(description.contains("call action=live"), "{description}");
    assert!(description.contains("action=live_read"));
    assert!(description.contains("ChatGPT, Claude, Gemini or any AI app"));
    assert!(VALID_ACTIONS.contains(&"live") && VALID_ACTIONS.contains(&"live_read"));
    let schema = tool.input_schema();
    let properties = &schema["properties"];
    for field in [
        "page",
        "path",
        "start_line",
        "end_line",
        "query",
        "kind",
        "item_id",
        "unlock_key",
    ] {
        assert!(properties.get(field).is_some(), "{field}");
    }
    let action = properties["action"].to_string();
    assert!(
        action.contains("Live Capsule") && action.contains("live_read"),
        "{action}"
    );
}

#[test]
fn capsule_and_share_responses_carry_the_live_links() {
    let share: ContextCapsuleShareResponse = serde_json::from_value(json!({
        "id": Uuid::new_v4(),
        "live_url": "https://ctx.example/c/capsule_tok/live",
        "mcp_url": "https://ctx.example/c/capsule_tok/mcp"
    }))
    .unwrap();
    assert_eq!(
        share.live_url.as_deref(),
        Some("https://ctx.example/c/capsule_tok/live")
    );
    assert_eq!(
        share.mcp_url.as_deref(),
        Some("https://ctx.example/c/capsule_tok/mcp")
    );
    let capsule: ContextCapsuleResponse = serde_json::from_value(json!({
        "capsule_id": "cap_1",
        "links": {"live_url": "https://ctx.example/c/t/live", "mcp_url": "https://ctx.example/c/t/mcp"}
    }))
    .unwrap();
    assert_eq!(
        capsule.links.live_url.as_deref(),
        Some("https://ctx.example/c/t/live")
    );
    assert_eq!(
        capsule.links.mcp_url.as_deref(),
        Some("https://ctx.example/c/t/mcp")
    );
}

#[derive(Clone, Debug)]
struct RecordedHttpRequest {
    request_line: String,
    headers: String,
    body: String,
}

struct ScriptedReviewMock {
    base_url: String,
    requests: std::sync::Arc<std::sync::Mutex<Vec<RecordedHttpRequest>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ScriptedReviewMock {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl ScriptedReviewMock {
    async fn start<F>(responder: F) -> Self
    where
        F: Fn(&str, &str, &str) -> (u16, &'static str, Vec<(&'static str, String)>, String)
            + Send
            + Sync
            + 'static,
    {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let responder = std::sync::Arc::new(responder);

        let task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let recorded = recorded.clone();
                let responder = responder.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let (head, body) = loop {
                        let mut chunk = [0u8; 4096];
                        let n = socket.read(&mut chunk).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                            continue;
                        };
                        let head = String::from_utf8_lossy(&buf[..end]).to_string();
                        let len: usize = head
                            .lines()
                            .filter_map(|l| l.split_once(':'))
                            .find(|(k, _)| k.trim().eq_ignore_ascii_case("content-length"))
                            .and_then(|(_, v)| v.trim().parse().ok())
                            .unwrap_or(0);
                        if buf.len() >= end + 4 + len {
                            let body =
                                String::from_utf8_lossy(&buf[end + 4..end + 4 + len]).to_string();
                            break (head, body);
                        }
                    };
                    let request_line = head.lines().next().unwrap_or_default().to_string();
                    recorded.lock().unwrap().push(RecordedHttpRequest {
                        request_line: request_line.clone(),
                        headers: head.clone(),
                        body: body.clone(),
                    });
                    let (status, reason, extra_headers, resp_body) =
                        responder(&request_line, &head, &body);
                    let mut resp = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n",
                        resp_body.len()
                    );
                    for (k, v) in extra_headers {
                        resp.push_str(&format!("{k}: {v}\r\n"));
                    }
                    resp.push_str("\r\n");
                    resp.push_str(&resp_body);
                    let _ = socket.write_all(resp.as_bytes()).await;
                });
            }
        });

        Self {
            base_url,
            requests,
            task,
        }
    }

    fn captured(&self) -> Vec<RecordedHttpRequest> {
        self.requests.lock().unwrap().clone()
    }
}

fn run_git_sync(dir: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Tester")
        .env("GIT_AUTHOR_EMAIL", "tester@example.com")
        .env("GIT_COMMITTER_NAME", "Tester")
        .env("GIT_COMMITTER_EMAIL", "tester@example.com")
        .output()
        .expect("git command");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[tokio::test]
async fn review_capsule_create_from_throwaway_repo_scrubs_and_omits_auto_share() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    run_git_sync(root, &["init"]);
    std::fs::write(root.join("lib.rs"), "pub fn ok() -> i32 { 1 }\n").unwrap();
    run_git_sync(root, &["add", "."]);
    run_git_sync(root, &["commit", "-m", "base"]);
    let base_sha = run_git_sync(root, &["rev-parse", "HEAD"]);

    // Commit both a real code change with an embedded secret and a .env.example sensitive file.
    std::fs::write(
        root.join("lib.rs"),
        "pub fn ok() -> i32 { 2 }\npub const KEY: &str = \"sk_live_51TestSecretValue123456\";\n",
    )
    .unwrap();
    std::fs::write(
        root.join(".env.example"),
        "API_TOKEN=secret_example_value\n",
    )
    .unwrap();
    run_git_sync(root, &["add", "."]);
    run_git_sync(root, &["commit", "-m", "head"]);
    let head_sha = run_git_sync(root, &["rev-parse", "HEAD"]);

    let mock = ScriptedReviewMock::start(|line, _headers, _body| {
        if line.starts_with("POST /api/v1/capsules/review ") {
            let body = serde_json::json!({
                "success": true,
                "data": {
                    "capsule_id": "cap_rev_1",
                    "name": "auth-check",
                    "review": {
                        "label": "auth-check",
                        "digest": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                        "contract_version": 1,
                        "repo": "local/repo",
                        "base": "base",
                        "head": "head",
                        "title": "Check auth",
                        "files": 2,
                        "files_shown": 1,
                        "add": 2,
                        "del": 0,
                        "parts": 1,
                        "chars": 120,
                        "claims": 1,
                        "evidence": 0,
                        "redactions": 1,
                        "source": "client"
                    },
                    "redactions": [
                        { "at": "lib.rs:2", "rule": "known_prefix", "count": 1 }
                    ],
                    "links": {
                        "review": "/api/v1/capsules/cap_rev_1/review",
                        "prompt": "/api/v1/capsules/cap_rev_1/review/prompt.md"
                    }
                }
            });
            return (
                201,
                "Created",
                vec![("Content-Type", "application/json".to_string())],
                body.to_string(),
            );
        }
        panic!("unexpected request line: {line}");
    })
    .await;

    let mut cfg = TestFixtures::test_config();
    cfg.api_url = mock.base_url.clone();
    let client = ContextStreamClient::new(cfg);
    let tool = CapsuleTool::new(client);
    let project_id = Uuid::new_v4().to_string();

    let res = tool
        .execute(json!({
            "action": "create",
            "project_id": project_id,
            "folder_path": root.to_string_lossy(),
            "name": "auth-check",
            "review": {
                "base": base_sha,
                "head": head_sha,
                "title": "Check auth",
                "task": "Verify code safety",
                "claims": ["No secrets leave client"]
            }
        }))
        .await
        .unwrap();
    assert!(!res.is_error);

    let captured = mock.captured();
    assert_eq!(
        captured.len(),
        1,
        "only POST /api/v1/capsules/review expected"
    );
    assert!(captured[0]
        .request_line
        .starts_with("POST /api/v1/capsules/review "));
    assert!(
        !captured[0].body.contains("sk_live_51TestSecretValue123456"),
        "secret must be scrubbed client-side before upload: {}",
        captured[0].body
    );
    assert!(
        captured[0].body.contains("[REDACTED]"),
        "expected [REDACTED] placeholder: {}",
        captured[0].body
    );
    assert!(
        !captured[0].body.contains("secret_example_value"),
        "sensitive .env.example content must never upload: {}",
        captured[0].body
    );
    let parsed_body: serde_json::Value = serde_json::from_str(&captured[0].body).unwrap();
    let files = parsed_body["review"]["files"].as_array().unwrap();
    let env_entry = files.iter().find(|f| f["path"] == ".env.example").unwrap();
    assert_eq!(env_entry["skipped"], "excluded");
}

#[tokio::test]
async fn review_capsule_create_purpose_review_without_review_object_uses_legacy_endpoint() {
    let probe = RequestProbe::answering(
        r#"{"success":true,"data":{"capsule_id":"cap_legacy","status":"ready","warnings":[]}}"#,
    )
    .await;
    let mut cfg = TestFixtures::test_config();
    cfg.api_url = probe.base_url.clone();
    let client = ContextStreamClient::new(cfg);
    let tool = CapsuleTool::new(client);

    let res = tool
        .execute(json!({
            "action": "create",
            "scope": "workspace",
            "workspace_id": Uuid::new_v4().to_string(),
            "purpose": "review"
        }))
        .await
        .unwrap();
    assert!(!res.is_error);
    let lines = probe.request_lines();
    assert_eq!(lines.len(), 1);
    assert!(
        lines[0].starts_with("POST /api/v1/capsules "),
        "expected legacy POST /api/v1/capsules, got {:?}",
        lines
    );
}

#[tokio::test]
async fn review_capsule_open_fetches_ordered_parts_verifies_sha256_and_skips_warm_cache() {
    let part1_md = "# Review capsule 0123456789abcdef  (part 1 of 2)\n\nPart 1 body";
    let part2_md = "# Review capsule 0123456789abcdef  (part 2 of 2)\n\nPart 2 body\n\nCAPSULE: 0123456789abcdef";
    let part1_sha = crate::domains::review_capsule::sha256_hex(part1_md);
    let part2_sha = crate::domains::review_capsule::sha256_hex(part2_md);
    let digest = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    let p1_md = part1_md.to_string();
    let p2_md = part2_md.to_string();
    let p1_sha = part1_sha.clone();
    let p2_sha = part2_sha.clone();

    let mock = ScriptedReviewMock::start(move |line, _headers, _body| {
        if line.starts_with("GET /api/v1/capsules/cap_rev_open ") {
            let body = json!({
                "success": true,
                "data": {
                    "capsule_id": "cap_rev_open",
                    "scope": "project",
                    "mode": "snapshot",
                    "status": "ready",
                    "review": { "label": "rev", "digest": digest }
                }
            });
            return (
                200,
                "OK",
                vec![("Content-Type", "application/json".to_string())],
                body.to_string(),
            );
        }
        if line.starts_with("GET /api/v1/capsules/cap_rev_open/review ") {
            let body = json!({
                "success": true,
                "data": {
                    "capsule_id": "cap_rev_open",
                    "name": "rev",
                    "review": {
                        "label": "rev",
                        "digest": digest,
                        "contract_version": 1,
                        "repo": "org/repo",
                        "base": "aaa",
                        "head": "bbb",
                        "files": 1,
                        "files_shown": 1,
                        "parts": 2
                    },
                    "manifest": {
                        "contract_version": 1,
                        "digest": digest,
                        "label": "rev",
                        "source": "client",
                        "repo": "org/repo",
                        "base": "aaa",
                        "head": "bbb",
                        "title": "Title",
                        "task": "Task",
                        "part_chars": 4000,
                        "parts": [
                            { "n": 2, "chars": p2_md.len(), "sha256": p2_sha },
                            { "n": 1, "chars": p1_md.len(), "sha256": p1_sha }
                        ]
                    }
                }
            });
            return (
                200,
                "OK",
                vec![("Content-Type", "application/json".to_string())],
                body.to_string(),
            );
        }
        if line.starts_with("GET /api/v1/capsules/cap_rev_open/review/prompt.md?part=1 ") {
            return (
                200,
                "OK",
                vec![
                    ("Content-Type", "text/markdown; charset=utf-8".to_string()),
                    ("X-Capsule-Digest", digest.to_string()),
                    ("X-Capsule-Part", "1 of 2".to_string()),
                    ("X-Capsule-Part-SHA256", p1_sha.clone()),
                ],
                p1_md.clone(),
            );
        }
        if line.starts_with("GET /api/v1/capsules/cap_rev_open/review/prompt.md?part=2 ") {
            return (
                200,
                "OK",
                vec![
                    ("Content-Type", "text/markdown; charset=utf-8".to_string()),
                    ("X-Capsule-Digest", digest.to_string()),
                    ("X-Capsule-Part", "2 of 2".to_string()),
                    ("X-Capsule-Part-SHA256", p2_sha.clone()),
                ],
                p2_md.clone(),
            );
        }
        panic!("unexpected request: {line}");
    })
    .await;

    let mut cfg = TestFixtures::test_config();
    cfg.api_url = mock.base_url.clone();
    let client = ContextStreamClient::new(cfg);
    let tool = CapsuleTool::new(client);

    let res = tool
        .execute(json!({
            "action": "open",
            "capsule_id": "cap_rev_open",
            "part": "all"
        }))
        .await
        .unwrap();
    let text = match &res.content[0] {
        mcp_types::tool::ContentItem::Text { text } => text.clone(),
        _ => panic!("expected text"),
    };
    assert!(text.contains("CAPSULE: 0123456789abcdef"));
    let p1_pos = text.find("Part 1 body").unwrap();
    let p2_pos = text.find("Part 2 body").unwrap();
    assert!(p1_pos < p2_pos, "parts must be assembled 1 then 2: {text}");

    let lines: Vec<String> = mock
        .captured()
        .into_iter()
        .map(|r| r.request_line)
        .collect();
    assert_eq!(
        lines,
        vec![
            "GET /api/v1/capsules/cap_rev_open/review HTTP/1.1",
            "GET /api/v1/capsules/cap_rev_open/review/prompt.md?part=1 HTTP/1.1",
            "GET /api/v1/capsules/cap_rev_open/review/prompt.md?part=2 HTTP/1.1",
        ]
    );
}

#[tokio::test]
async fn review_capsule_open_rejects_sha256_mismatch() {
    let digest = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let mock = ScriptedReviewMock::start(move |line, _headers, _body| {
        if line.starts_with("GET /api/v1/capsules/cap_bad_sha/review ") {
            let body = json!({
                "success": true,
                "data": {
                    "capsule_id": "cap_bad_sha",
                    "review": { "label": "rev", "digest": digest },
                    "manifest": {
                        "contract_version": 1,
                        "digest": digest,
                        "label": "rev",
                        "source": "client",
                        "repo": "org/repo",
                        "base": "aaa",
                        "head": "bbb",
                        "title": "Title",
                        "task": "Task",
                        "part_chars": 4000,
                        "parts": [
                            { "n": 1, "chars": 10, "sha256": "0000000000000000000000000000000000000000000000000000000000000000" }
                        ]
                    }
                }
            });
            return (200, "OK", vec![("Content-Type", "application/json".to_string())], body.to_string());
        }
        if line.starts_with("GET /api/v1/capsules/cap_bad_sha/review/prompt.md?part=1 ") {
            return (
                200,
                "OK",
                vec![("Content-Type", "text/markdown; charset=utf-8".to_string())],
                "Tampered markdown body".to_string(),
            );
        }
        panic!("unexpected request: {line}");
    })
    .await;

    let mut cfg = TestFixtures::test_config();
    cfg.api_url = mock.base_url.clone();
    let tool = CapsuleTool::new(ContextStreamClient::new(cfg));
    let err = tool
        .execute(json!({ "action": "open", "capsule_id": "cap_bad_sha", "part": 1 }))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("SHA-256 mismatch"), "got: {err}");
}

#[tokio::test]
async fn review_capsule_review_answer_201_and_422_preserve_problems() {
    let mock = ScriptedReviewMock::start(|line, _headers, body| {
        if line.starts_with("POST /api/v1/capsules/cap_ans/review/answers ") {
            let parsed: serde_json::Value = serde_json::from_str(body).unwrap();
            if parsed["answer"].as_str().unwrap_or("").contains("BAD") {
                return (
                    422,
                    "Unprocessable Entity",
                    vec![("Content-Type", "application/json".to_string())],
                    json!({
                        "success": false,
                        "error": {
                            "code": "REVIEW_ANSWER_INVALID",
                            "message": "The review answer did not pass validation."
                        },
                        "data": {
                            "valid": false,
                            "problems": ["Missing VERDICT line", "Claim C1 is unjudged"]
                        }
                    })
                    .to_string(),
                );
            }
            return (
                201,
                "Created",
                vec![("Content-Type", "application/json".to_string())],
                json!({
                    "success": true,
                    "data": {
                        "valid": true,
                        "verdict": "SAFE TO LAND",
                        "must": 0,
                        "should": 0,
                        "status": "approved",
                        "review_id": Uuid::new_v4()
                    }
                })
                .to_string(),
            );
        }
        panic!("unexpected request: {line}");
    })
    .await;

    let mut cfg = TestFixtures::test_config();
    cfg.api_url = mock.base_url.clone();
    let tool = CapsuleTool::new(ContextStreamClient::new(cfg));

    let invalid = tool
        .execute(json!({
            "action": "review_answer",
            "capsule_id": "cap_ans",
            "answer": "BAD"
        }))
        .await
        .unwrap();
    assert!(invalid.is_error, "422 must surface as is_error=true");
    let structured = invalid.structured_content.unwrap();
    assert_eq!(structured["valid"], false);
    assert_eq!(
        structured["problems"],
        json!(["Missing VERDICT line", "Claim C1 is unjudged"])
    );

    let ok = tool
        .execute(json!({
            "action": "review_answer",
            "capsule_id": "cap_ans",
            "answer": "CAPSULE: 0123456789abcdef\nVERDICT: SAFE TO LAND"
        }))
        .await
        .unwrap();
    assert!(!ok.is_error);
}

#[tokio::test]
async fn review_capsule_review_answers_and_review_credential_member_actions() {
    let cred_id = Uuid::new_v4();
    let ans_id = Uuid::new_v4();
    let mock = ScriptedReviewMock::start(move |line, _headers, _body| {
        if line.starts_with("GET /api/v1/capsules/cap_mem/review/answers ") {
            return (
                200,
                "OK",
                vec![("Content-Type", "application/json".to_string())],
                json!({
                    "success": true,
                    "data": {
                        "capsule_id": "cap_mem",
                        "digest": "0123456789abcdef",
                        "answers": [{
                            "review_id": ans_id,
                            "title": "Review",
                            "status": "approved",
                            "verdict": "SAFE TO LAND",
                            "by": "credential",
                            "for_this_digest": true,
                            "headline": "All claims verified"
                        }]
                    }
                })
                .to_string(),
            );
        }
        if line.starts_with("POST /api/v1/capsules/cap_mem/review/credentials ") {
            return (
                201,
                "Created",
                vec![("Content-Type", "application/json".to_string())],
                json!({
                    "success": true,
                    "data": {
                        "credential": "capsule_testcred1234567890",
                        "credential_id": cred_id,
                        "capsule_id": "cap_mem",
                        "digest": "0123456789abcdef",
                        "answer": true,
                        "max_answers": 2,
                        "expires_at": "2026-12-01T00:00:00Z",
                        "shown_once": true,
                        "use": { "header": "X-Capsule-Credential" }
                    }
                })
                .to_string(),
            );
        }
        panic!("unexpected request: {line}");
    })
    .await;

    let mut cfg = TestFixtures::test_config();
    cfg.api_url = mock.base_url.clone();
    let tool = CapsuleTool::new(ContextStreamClient::new(cfg));

    let list_res = tool
        .execute(json!({ "action": "review_answers", "capsule_id": "cap_mem" }))
        .await
        .unwrap();
    assert!(!list_res.is_error);
    let list_text = match &list_res.content[0] {
        mcp_types::tool::ContentItem::Text { text } => text.clone(),
        _ => panic!(),
    };
    assert!(list_text.contains("SAFE TO LAND"));

    let cred_res = tool
        .execute(json!({
            "action": "review_credential",
            "capsule_id": "cap_mem",
            "allow_answer": true,
            "expires_in_hours": 12,
            "max_answers": 2
        }))
        .await
        .unwrap();
    assert!(!cred_res.is_error);
    let cred_text = match &cred_res.content[0] {
        mcp_types::tool::ContentItem::Text { text } => text.clone(),
        _ => panic!(),
    };
    assert!(cred_text.contains("capsule_testcred1234567890"));
}

#[tokio::test]
async fn review_capsule_credential_only_mode_isolates_actions_and_headers() {
    let md = "# Review capsule 0123456789abcdef  (part 1 of 1)\n\nReviewer view";
    let sha = crate::domains::review_capsule::sha256_hex(md);
    let digest = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    let md_clone = md.to_string();
    let sha_clone = sha.clone();

    let mock = ScriptedReviewMock::start(move |line, _headers, _body| {
        if line.starts_with("GET /api/v1/capsules/review/open ") {
            return (
                200,
                "OK",
                vec![("Content-Type", "application/json".to_string())],
                json!({
                    "success": true,
                    "data": {
                        "review": { "label": "rev", "digest": digest, "repo": "org/repo", "base": "a", "head": "b", "files": 1, "files_shown": 1, "parts": 1 },
                        "manifest": {
                            "contract_version": 1,
                            "digest": digest,
                            "label": "rev",
                            "source": "client",
                            "repo": "org/repo",
                            "base": "a",
                            "head": "b",
                            "title": "T",
                            "task": "Task",
                            "part_chars": 4000,
                            "parts": [{ "n": 1, "chars": md_clone.len(), "sha256": sha_clone }]
                        }
                    }
                })
                .to_string(),
            );
        }
        if line.starts_with("GET /api/v1/capsules/review/open/prompt.md?part=1 ") {
            return (
                200,
                "OK",
                vec![
                    ("Content-Type", "text/markdown; charset=utf-8".to_string()),
                    ("X-Capsule-Digest", digest.to_string()),
                    ("X-Capsule-Part-SHA256", sha_clone.clone()),
                ],
                md_clone.clone(),
            );
        }
        if line.starts_with("POST /api/v1/capsules/review/open/answers ") {
            return (
                201,
                "Created",
                vec![("Content-Type", "application/json".to_string())],
                json!({
                    "success": true,
                    "data": {
                        "valid": true,
                        "verdict": "SAFE TO LAND",
                        "must": 0,
                        "should": 0,
                        "status": "approved"
                    }
                })
                .to_string(),
            );
        }
        panic!("unexpected credential-only request: {line}");
    })
    .await;

    let mut cfg = TestFixtures::test_config();
    cfg.api_url = mock.base_url.clone();
    cfg.api_key = None;
    cfg.jwt = None;
    cfg.capsule_credential = Some("capsule_only_secret_tok".to_string());
    cfg.tool_surface_profile = mcp_types::config::ToolSurfaceProfile::ReviewCredential;
    let tool = CapsuleTool::new(ContextStreamClient::new(cfg));

    // All non-(open, review_answer) actions fail locally with ZERO HTTP requests.
    for blocked in [
        "create",
        "live",
        "list",
        "share",
        "review_answers",
        "review_credential",
    ] {
        let err = tool
            .execute(json!({ "action": blocked }))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("review credential"),
            "{blocked}: {err}"
        );
    }
    assert!(
        mock.captured().is_empty(),
        "blocked actions sent network requests"
    );

    // Passing capsule_id in credential-only mode is rejected locally.
    let err = tool
        .execute(json!({ "action": "open", "capsule_id": "cap_other" }))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("Do not pass capsule_id"));
    assert!(mock.captured().is_empty());

    // open + review_answer work with ONLY X-Capsule-Credential.
    let opened = tool.execute(json!({ "action": "open" })).await.unwrap();
    assert!(!opened.is_error);
    let answered = tool
        .execute(json!({
            "action": "review_answer",
            "answer": "CAPSULE: 0123456789abcdef\nVERDICT: SAFE TO LAND"
        }))
        .await
        .unwrap();
    assert!(!answered.is_error);

    let reqs = mock.captured();
    assert_eq!(reqs.len(), 3);
    for r in &reqs {
        let lower = r.headers.to_ascii_lowercase();
        assert!(
            lower.contains("x-capsule-credential: capsule_only_secret_tok"),
            "missing X-Capsule-Credential header: {}",
            r.headers
        );
        assert!(
            !lower.contains("x-api-key:") && !lower.contains("authorization:"),
            "member auth headers leaked on review-credential request: {}",
            r.headers
        );
    }
}

#[tokio::test]
async fn review_capsule_registry_isolation_exposes_only_capsule_and_no_operations() {
    use crate::registry::ToolRegistry;
    use mcp_types::config::{ToolSurfaceProfile, Toolset};

    let mut cfg = TestFixtures::test_config();
    cfg.api_key = None;
    cfg.capsule_credential = Some("capsule_reg_tok".to_string());
    cfg.tool_surface_profile = ToolSurfaceProfile::ReviewCredential;
    cfg.toolset = Toolset::Light;
    cfg.router_mode = true;
    cfg.progressive_mode = true;
    cfg.capsule_enabled = true;
    let client = ContextStreamClient::new(cfg.clone());
    let session = Arc::new(mcp_session::SessionManager::new(
        client.clone(),
        cfg.clone(),
    ));

    let mut registry = ToolRegistry::new(&cfg);
    crate::domains::capsule::register_capsule_tools(&mut registry, client.clone(), session.clone());
    crate::domains::memory::register_memory_tools(&mut registry, client, session);

    assert_eq!(registry.names(), vec!["capsule"]);
    assert_eq!(registry.operation_count(), 0);
    assert!(registry.list_operations().is_empty());
    assert!(registry.search_catalog("memory", None, 10).is_empty());
    assert!(!registry.should_expose_discovery_tools());
    registry.apply_initialize_surface_profile(Some(ToolSurfaceProfile::Default));
    assert_eq!(
        registry.tool_surface_profile(),
        ToolSurfaceProfile::ReviewCredential
    );
    let err = registry
        .execute("memory", json!({ "action": "list_docs" }))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("review credential"));
}
