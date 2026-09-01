//! Tests for ContextCapsule domain tools.
#![allow(clippy::field_reassign_with_default)]

use super::*;
use crate::registry::ToolHandler;
use crate::testing::TestFixtures;
use mcp_client::CapsuleShareParams;
use mcp_types::api::{
    ContextCapsuleReadiness, ContextCapsuleResolvedScope, ContextCapsuleResponse,
    ContextCapsuleSection, ContextCapsuleShareResponse,
};
use mcp_types::tool::ToolCategory;
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
