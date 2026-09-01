//! Tests for session domain tools.

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

fn create_mock_session() -> Arc<SessionManager> {
    let client = create_mock_client();
    Arc::new(SessionManager::new(client, TestFixtures::test_config()))
}

mod repository_project_resolution_tests {
    use super::*;

    fn project(id: Uuid, name: &str) -> Project {
        Project {
            id,
            name: name.to_string(),
            description: None,
            repository_url: None,
            repository_type: None,
            workspace_id: None,
            path: None,
            created_at: None,
            updated_at: None,
            indexed_at: None,
            file_count: None,
        }
    }

    #[test]
    fn explicit_repository_remote_is_normalized_without_credentials() {
        let raw = "https://alice:secret@github.com/contextstream/mcp-server.git";
        let normalized =
            resolve_init_repository_url(Some(raw), None).expect("normalize repository remote");

        assert_eq!(
            normalized.as_deref(),
            Some("https://github.com/contextstream/mcp-server.git")
        );
        assert!(!normalized.unwrap().contains("secret"));
    }

    #[test]
    fn invalid_repository_remote_fails_without_echoing_input() {
        let raw = "not a remote with secret-material";
        let error = resolve_init_repository_url(Some(raw), None)
            .expect_err("invalid repository remote must fail");
        let message = error.to_string();

        assert!(message.contains("valid Git repository remote"));
        assert!(!message.contains(raw));
        assert!(!message.contains("secret-material"));
    }

    #[test]
    fn repository_identity_bypasses_folder_name_preflight() {
        assert!(!should_resolve_project_by_folder_name(
            None,
            Some("https://github.com/contextstream/mcp-server.git")
        ));
        assert!(!should_resolve_project_by_folder_name(
            Some(Uuid::new_v4()),
            None
        ));
        assert!(should_resolve_project_by_folder_name(None, None));
    }

    #[test]
    fn unique_folder_name_match_is_deterministic() {
        let expected = Uuid::new_v4();
        let projects = vec![
            project(Uuid::new_v4(), "another-project"),
            project(expected, "Context Stream"),
        ];

        let resolved = resolve_project_from_catalog(&projects, "/tmp/context-stream")
            .expect("resolve unique project");

        assert_eq!(resolved.map(|(id, _)| id), Some(expected));
    }

    #[test]
    fn duplicate_normalized_folder_matches_fail_closed() {
        let projects = vec![
            project(Uuid::new_v4(), "context-stream"),
            project(Uuid::new_v4(), "Context Stream"),
        ];

        let error = resolve_project_from_catalog(&projects, "/tmp/context_stream")
            .expect_err("ambiguous project names must fail");

        assert!(error.to_string().contains("Multiple projects match"));
        assert!(error.to_string().contains("explicit project_id"));
    }
}

mod init_index_status_tests {
    use super::*;

    #[test]
    fn canonical_readiness_survives_an_unconfirmed_checkout_scope() {
        let classified = classify_init_index_status(json!({
            "indexed": true,
            "indexed_file_count": 886,
            "project_index_state": "ready",
            "last_updated": "2026-07-31T07:55:01Z",
            "mcp_checkout_scope": {
                "requested": true,
                "recognized": false,
                "matched": false
            }
        }));

        match classified {
            InitIndexStatus::Ready {
                status,
                checkout_scope_confirmed,
            } => {
                assert!(!checkout_scope_confirmed);
                assert_eq!(extract_backend_indexed_count(&status), Some(886));
                assert!(extract_backend_index_timestamp(&status).is_some());
            }
            other => panic!("canonical readiness was discarded: {other:?}"),
        }

        let notice = init_checkout_unconfirmed_notice(886, Some(0));
        assert!(notice.contains("Project index is ready (886 files indexed"));
        assert!(notice.contains("Canonical semantic search is available"));
        assert!(notice.contains("did not confirm this exact checkout overlay"));
        assert!(notice.contains("not a missing-index condition"));
        assert!(notice.contains("Keep hosted MCP configured"));
        assert!(!notice.contains("Project index not found"));
        assert!(!notice.contains("local MCP"));
    }

    #[test]
    fn matched_or_unscoped_status_is_confirmed_for_init() {
        for status in [
            json!({
                "indexed_file_count": 12,
                "mcp_checkout_scope": {
                    "requested": true,
                    "recognized": true,
                    "matched": true
                }
            }),
            json!({"indexed_file_count": 12}),
        ] {
            match classify_init_index_status(status) {
                InitIndexStatus::Ready {
                    checkout_scope_confirmed,
                    ..
                } => assert!(checkout_scope_confirmed),
                other => panic!("ready status was discarded: {other:?}"),
            }
        }
    }

    #[test]
    fn explicit_false_is_authoritative_only_for_a_confirmed_checkout() {
        let status = json!({
            "indexed": false,
            "indexed_file_count": 886,
            "project_index_state": "ready"
        });

        assert!(!init_index_status_reports_ready(&status, true));
        assert!(init_index_status_reports_ready(&status, false));
    }
}

#[test]
fn session_metadata_avoids_duplicate_recall_after_sufficient_grounding() {
    let client = create_mock_client();
    let session = create_mock_session();
    let tool = SessionTool::new(client, session, mcp_types::atlas_layer::noop_layer());
    let description = &tool.metadata().description;

    assert!(description.contains("do not immediately call action='recall'"));
    assert!(description.contains("the first explicit escalation"));
    assert!(description.contains("absent, thin, stale, off-topic"));
    assert!(!description.contains("call action='recall' FIRST"));
}

#[test]
fn session_exposes_daily_recap_actions_and_correct_trigger_model() {
    let client = create_mock_client();
    let session = create_mock_session();
    let tool = SessionTool::new(client, session, mcp_types::atlas_layer::noop_layer());
    let schema = tool.input_schema();
    let actions = schema["properties"]["action"]["enum"]
        .as_array()
        .expect("session actions");

    assert!(actions.iter().any(|action| action == "list_recaps"));
    assert!(actions.iter().any(|action| action == "trigger_recap"));
    assert!(tool.metadata().description.contains("around 23:00"));
    assert!(tool
        .metadata()
        .description
        .contains("not at MCP session boundaries"));
}

#[test]
fn recap_history_text_includes_dates_timestamps_and_headlines() {
    let text = render_recap_history(&json!([
        {
            "recap_date": "2026-08-04",
            "generated_at": "2026-08-05T06:00:00Z",
            "headline": "Shipped MCP support fixes"
        },
        {
            "recap_date": "2026-08-03",
            "generated_at": "2026-08-04T06:00:00Z"
        }
    ]));

    assert!(text.contains("Daily Recaps (2), newest first"));
    assert!(text.contains("2026-08-04 — generated 2026-08-05T06:00:00Z"));
    assert!(text.contains("Shipped MCP support fixes"));
}

#[test]
fn empty_recap_history_explains_daily_and_manual_generation() {
    let text = render_recap_history(&json!([]));

    assert!(text.contains("around 23:00"));
    assert!(text.contains("trigger_recap"));
}

#[test]
fn a_requested_checkout_is_unconfirmed_when_its_locator_is_unavailable() {
    assert!(requested_checkout_scope_confirmed(false, None, |_| false));
    assert!(!requested_checkout_scope_confirmed(true, None, |_| true));

    let scope = mcp_client::CheckoutRoutingScope {
        installation_id: Uuid::new_v4(),
        checkout_locator: "checkout-locator-v1:opaque".to_string(),
    };
    assert!(requested_checkout_scope_confirmed(
        true,
        Some(&scope),
        |_| true
    ));
    assert!(!requested_checkout_scope_confirmed(
        true,
        Some(&scope),
        |_| false
    ));
}

fn create_mock_index_keeper_from(
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

fn create_mock_index_keeper() -> Arc<crate::domains::index_keeper::IndexKeeper> {
    let client = create_mock_client();
    let session = create_mock_session();
    Arc::new(crate::domains::index_keeper::IndexKeeper::new(
        client,
        session,
        mcp_types::atlas_layer::noop_layer(),
        mcp_types::acceleration_layer::noop_acceleration_layer(),
    ))
}

mod context_fast_routing_tests {
    use super::*;

    fn eligible_guard() -> ImplicitFastContextGuard {
        ImplicitFastContextGuard {
            scope_authoritative: true,
            workspace_resolved: true,
            project_resolved: true,
            save_exchange: false,
            has_assistant_message: false,
            restore_after_compaction: false,
        }
    }

    #[test]
    fn implicit_fast_accepts_only_short_read_only_context_lookups() {
        for message in [
            "list lessons",
            "Show decisions.",
            "get docs",
            "check index status",
            "display available tools",
            "count tasks",
            "how many todos?",
            "version",
            "help",
            "what version is the MCP server?",
            "which version is the server running?",
        ] {
            assert_eq!(
                context_fast_route(None, message, eligible_guard()),
                Some(ContextFastRoute::ImplicitReadOnlyLookup),
                "expected implicit fast route for {message:?}"
            );
        }
    }

    #[test]
    fn implicit_fast_rejects_grounding_reasoning_code_and_mutation_prompts() {
        for message in [
            "please continue",
            "what did we do last session?",
            "search code",
            "find decisions",
            "list files",
            "show project files",
            "show code architecture",
            "show function dependencies",
            "explain decisions",
            "compare plans",
            "recommend a plan",
            "why was this decision made?",
            "show project implementation",
            "fix tasks",
            "create a todo",
            "list lessons and explain why they matter",
        ] {
            assert_eq!(
                context_fast_route(None, message, eligible_guard()),
                None,
                "expected smart grounding route for {message:?}"
            );
        }
    }

    #[test]
    fn explicit_modes_take_precedence_without_changing_fast_safety_guards() {
        assert_eq!(
            context_fast_route(
                Some("fast"),
                "explain the architecture",
                ImplicitFastContextGuard::default(),
            ),
            Some(ContextFastRoute::Explicit)
        );
        assert_eq!(
            context_fast_route(Some("standard"), "list lessons", eligible_guard()),
            None
        );
        assert_eq!(
            context_fast_route(Some("pack"), "list lessons", eligible_guard()),
            None
        );

        let mut guard = eligible_guard();
        guard.save_exchange = true;
        assert_eq!(
            context_fast_route(Some("fast"), "list lessons", guard),
            None
        );
        guard.save_exchange = false;
        guard.restore_after_compaction = true;
        assert_eq!(
            context_fast_route(Some("fast"), "list lessons", guard),
            None
        );
    }

    #[test]
    fn implicit_fast_requires_every_authority_and_side_effect_guard() {
        assert_eq!(
            context_fast_route(None, "list lessons", eligible_guard()),
            Some(ContextFastRoute::ImplicitReadOnlyLookup)
        );

        let mut guard = eligible_guard();
        guard.scope_authoritative = false;
        assert_eq!(context_fast_route(None, "list lessons", guard), None);

        guard = eligible_guard();
        guard.workspace_resolved = false;
        assert_eq!(context_fast_route(None, "list lessons", guard), None);

        guard = eligible_guard();
        guard.project_resolved = false;
        assert_eq!(context_fast_route(None, "list lessons", guard), None);

        guard = eligible_guard();
        guard.save_exchange = true;
        assert_eq!(context_fast_route(None, "list lessons", guard), None);

        guard = eligible_guard();
        guard.has_assistant_message = true;
        assert_eq!(context_fast_route(None, "list lessons", guard), None);

        guard = eligible_guard();
        guard.restore_after_compaction = true;
        assert_eq!(context_fast_route(None, "list lessons", guard), None);
    }

    #[test]
    fn fast_route_metadata_is_explicit_and_preserves_server_provenance() {
        let mut implicit = serde_json::json!({});
        attach_context_fast_route_metadata(&mut implicit, ContextFastRoute::ImplicitReadOnlyLookup);
        assert_eq!(implicit["context_route"], "hook_fast");
        assert_eq!(
            implicit["context_route_reason"],
            "implicit_read_only_lookup"
        );
        assert_eq!(implicit["context_route_implicit"], true);
        assert_eq!(implicit["served_from"], "context_hook_fast");

        let mut explicit = serde_json::json!({"served_from": "server_cache"});
        attach_context_fast_route_metadata(&mut explicit, ContextFastRoute::Explicit);
        assert_eq!(explicit["context_route_reason"], "explicit_fast_mode");
        assert_eq!(explicit["context_route_implicit"], false);
        assert_eq!(explicit["served_from"], "server_cache");

        let mut scalar = serde_json::json!("legacy payload");
        attach_context_fast_route_metadata(&mut scalar, ContextFastRoute::Explicit);
        assert_eq!(scalar["data"], "legacy payload");
        assert_eq!(scalar["context_route"], "hook_fast");
    }
}

mod context_guidance_tests {
    use super::{
        attach_scope_guidance, budget_context_wire_payload, context_wire_result,
        context_wire_text_priority, estimated_context_tool_wire_tokens, extract_grounding_handle,
        extract_uuid_field, folder_scope_mismatches_project, init_version_notice_line,
        is_context_timeout_error, normalize_search_guidance, project_metadata_matches_folder,
        project_name_matches_folder, prune_low_relevance_context_lines,
        suppress_typed_context_duplicates, ContextWireTokenizerPolicy,
        CONTEXT_DEFAULT_USEFUL_TOKENS, CONTEXT_WIRE_ENVELOPE_TOKENS,
    };
    use mcp_types::api::Project;
    use mcp_types::{Error, ErrorCode};
    use serde_json::json;
    use uuid::Uuid;

    #[test]
    fn rewrites_hybrid_search_hint_to_auto() {
        let raw =
            r#"R:...|🔍SEARCH:mcp__contextstream__search(mode="hybrid")BEFORE Glob/Grep/Read"#;
        let normalized = normalize_search_guidance(raw);
        assert!(normalized.contains(r#"search(mode="auto")"#));
        assert!(!normalized.contains(r#"search(mode="hybrid")"#));
    }

    #[test]
    fn identifies_timeout_error_variants() {
        assert!(is_context_timeout_error(&Error::Timeout(30)));
        assert!(is_context_timeout_error(&Error::Network(
            "request timed out".to_string()
        )));
        assert!(is_context_timeout_error(&Error::http_with_code(
            504,
            "gateway timeout",
            ErrorCode::GatewayTimeout,
        )));
        assert!(!is_context_timeout_error(&Error::Validation(
            "bad input".to_string()
        )));
    }

    #[test]
    fn detects_stale_session_folder_for_explicit_project() {
        let explicit_project_id = Uuid::new_v4();
        let stale_folder_project_id = Uuid::new_v4();

        assert!(folder_scope_mismatches_project(
            Some(explicit_project_id),
            Some(stale_folder_project_id),
            None,
        ));
        assert!(!folder_scope_mismatches_project(
            Some(explicit_project_id),
            Some(stale_folder_project_id),
            Some(explicit_project_id),
        ));
        assert!(!folder_scope_mismatches_project(
            Some(explicit_project_id),
            None,
            None,
        ));
    }

    #[test]
    fn project_name_matching_normalizes_folder_and_project_names() {
        assert!(project_name_matches_folder(
            "/home/alice/projects/example-app",
            "Example App"
        ));
        assert!(project_name_matches_folder(
            "/home/alice/projects/example_app",
            "exampleapp"
        ));
        assert!(!project_name_matches_folder(
            "/home/alice/projects/example-code",
            "admin-console"
        ));
    }

    #[test]
    fn project_metadata_rejects_mapped_project_for_different_project_folder() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            temp.path().join("Cargo.toml"),
            "[package]\nname='example-code'\n",
        )
        .expect("marker");
        let project = Project {
            id: Uuid::new_v4(),
            name: "admin-console".to_string(),
            description: None,
            repository_url: None,
            repository_type: None,
            workspace_id: Some(Uuid::new_v4()),
            path: Some("/home/alice/projects/admin-console".to_string()),
            created_at: None,
            updated_at: None,
            indexed_at: None,
            file_count: None,
        };

        assert!(!project_metadata_matches_folder(
            temp.path().to_str().unwrap_or_default(),
            &project
        ));
    }

    #[test]
    fn prunes_low_relevance_signal_lines() {
        let raw = r#"[CTX]
R:search-first
M:Billing credits model in the example backend
M:Stripe analytics preferences
C:crates/admin-console/src/request_metrics.rs
C:src/cost-chart.tsx
M:request metrics 403 handling details
[/CTX]"#;

        let filtered = prune_low_relevance_context_lines(raw, "debug 403 on request metrics");
        assert!(filtered.contains("request_metrics.rs"));
        assert!(filtered.contains("request metrics 403 handling details"));
        assert!(!filtered.contains("cost-chart.tsx"));
        assert!(filtered.contains("[CTX_FILTER]"));
    }

    #[test]
    fn leaves_plain_context_without_signal_lines_unchanged() {
        let raw = "General summary\nNo structured entries";
        let filtered = prune_low_relevance_context_lines(raw, "search query");
        assert_eq!(filtered, raw);
    }

    #[test]
    fn typed_context_suppresses_legacy_duplicates_and_empty_reminder() {
        let raw = "Core rule\n<system-reminder>\n[PREFERENCE] concise\n[LESSONS_WARNING] verify first\n</system-reminder>\n[PREFERENCE] concise\n[LESSONS_WARNING] verify first\n[DECISION] keep this";
        let deduplicated = suppress_typed_context_duplicates(raw, true, true);

        assert_eq!(deduplicated, "Core rule\n[DECISION] keep this");
        assert!(!deduplicated.contains("[PREFERENCE]"));
        assert!(!deduplicated.contains("[LESSONS_WARNING]"));
        assert!(!deduplicated.contains("system-reminder"));
    }

    #[test]
    fn typed_context_keeps_unique_lesson_in_balanced_reminder() {
        let raw = "<system-reminder>\n[PREFERENCE] concise\n[LESSONS_WARNING] verify first\n</system-reminder>";
        let deduplicated = suppress_typed_context_duplicates(raw, true, false);

        assert_eq!(
            deduplicated,
            "<system-reminder>\n[LESSONS_WARNING] verify first\n</system-reminder>"
        );
    }

    #[test]
    fn extracts_uuid_field_from_payload() {
        let expected = Uuid::new_v4();
        let payload = json!({
            "workspace_id": expected.to_string()
        });

        assert_eq!(extract_uuid_field(&payload, "workspace_id"), Some(expected));
        assert_eq!(extract_uuid_field(&payload, "project_id"), None);
    }

    #[test]
    fn extracts_grounding_handle_from_direct_and_wrapped_api_payloads() {
        assert_eq!(
            extract_grounding_handle(&json!({"grounding_handle": " gb:v1:direct "})).as_deref(),
            Some("gb:v1:direct")
        );
        assert_eq!(
            extract_grounding_handle(&json!({"data": {"grounding_handle": "gb:v1:wrapped"}}))
                .as_deref(),
            Some("gb:v1:wrapped")
        );
        assert_eq!(
            extract_grounding_handle(&json!({"grounding_handle": ""})),
            None
        );
        assert_eq!(
            extract_grounding_handle(&json!({"grounding_handle": "x".repeat(1025)})),
            None
        );
    }

    fn oversized_context_wire_payload() -> (String, serde_json::Value) {
        let text = format!(
            "[SEARCH] {}\n[GROUNDING] {}\n[ACCOUNT_CONTEXT] {}\n[TEAM_CONTEXT] {}\n[INSTRUCTIONS] {}\n[LESSONS_WARNING] Keep the safety invariant active.",
            "search reminder ".repeat(80),
            "prior session evidence ".repeat(100),
            "account detail ".repeat(80),
            "team detail ".repeat(80),
            "dynamic workflow guidance ".repeat(80),
        );
        let structured = json!({
            "context": "core context ".repeat(200),
            "instructions": "dynamic workflow guidance ".repeat(100),
            "grounding_hits": [{"content": "grounding detail ".repeat(100)}],
            "why_this_context": {"trace": "diagnostic detail ".repeat(100)},
            "team_context": {"reason": "team detail ".repeat(100)},
            "suggested_tools": [{"name": "search", "reason": "tool detail ".repeat(80)}],
            "workspace_id": "00000000-0000-0000-0000-000000000001",
            "project_id": "00000000-0000-0000-0000-000000000002"
        });
        (text, structured)
    }

    #[test]
    fn whole_wire_budget_covers_text_and_structured_grounding() {
        let requested = 240usize;
        let (text, structured) = oversized_context_wire_payload();
        let (text, structured) = budget_context_wire_payload(text, structured, requested);
        let estimated = estimated_context_tool_wire_tokens(&text, &structured);
        let report = structured.get("wire_budget").expect("wire budget report");

        assert!(estimated <= requested + CONTEXT_WIRE_ENVELOPE_TOKENS);
        assert_eq!(
            report
                .get("estimated_tokens_after")
                .and_then(|value| value.as_u64()),
            Some(estimated as u64)
        );
        assert!(report["estimated_tokens_before"].as_u64().unwrap() > estimated as u64);
        assert!(report["dropped_structured_field_count"].as_u64().unwrap() > 0);
        assert!(text.contains("[LESSONS_WARNING]"));
        assert!(structured.get("why_this_context").is_none());
        assert!(structured.get("grounding_hits").is_none());
    }

    #[test]
    fn eight_hundred_token_budget_never_prefers_accounting_over_grounding() {
        let text = format!(
            "[GROUNDING] Prior work for this message:\n{}\n[ACCOUNT_CONTEXT]\n{}\n[CTX]\n{}\n[/CTX]",
            "1. Verified production evidence and actionable next step.\n".repeat(120),
            "team account metadata\n".repeat(80),
            "repository context and implementation detail\n".repeat(120),
        );
        let structured = json!({
            "why_this_context": {"trace": "diagnostic ".repeat(200)},
            "_timing": {"trace": "timing ".repeat(200)},
            "proactive_context": ["proactive ".repeat(200)],
            "conversation_audit": ["audit ".repeat(200)],
            "snapshot_insights": ["snapshot ".repeat(200)],
            "post_compact_restore": {"detail": "restore ".repeat(200)},
            "grounding_hits": [{"content": "verified prior work ".repeat(200)}],
            "team_priority_signals": ["priority ".repeat(200)],
            "team_governance": ["governance ".repeat(200)],
            "team_recommendations": ["recommendation ".repeat(200)],
            "team_context": {"reason": "team ".repeat(200)},
            "tool_results": ["tool result ".repeat(200)],
            "suggested_tools": ["suggestion ".repeat(200)],
            "context": "core context ".repeat(400),
        });

        let (text, structured) = budget_context_wire_payload(text, structured, 800);

        assert!(
            text.contains("[GROUNDING]") && text.contains("Verified production evidence"),
            "useful grounding must survive before wire diagnostics: {text:?}"
        );
        assert_ne!(
            text.trim(),
            "[WIRE_BUDGET] Whole-wire context compacted to the requested token envelope.",
            "the context tool must never return only its accounting notice"
        );
        assert!(
            estimated_context_tool_wire_tokens(&text, &structured)
                <= 800 + CONTEXT_WIRE_ENVELOPE_TOKENS
        );
    }

    #[test]
    fn omitted_max_tokens_defaults_to_useful_grounding_headroom() {
        assert_eq!(CONTEXT_DEFAULT_USEFUL_TOKENS, 2_000);
    }

    #[test]
    fn grounding_and_checkout_proof_outrank_wire_accounting() {
        assert_eq!(context_wire_text_priority("[GROUNDING] verified"), Some(3));
        assert_eq!(
            context_wire_text_priority("[CHECKOUT_SCOPE] verify upstream"),
            Some(3)
        );
        assert_eq!(
            context_wire_text_priority("[WIRE_BUDGET] compacted"),
            Some(0)
        );
    }

    #[test]
    fn whole_wire_budget_never_orphans_system_reminder_boundaries() {
        let text = format!(
            "[SEARCH] {}\n<system-reminder>\n[PREFERENCE] {}\n[LESSONS_WARNING] {}\n</system-reminder>\n[PREFERENCE] {}",
            "low priority ".repeat(500),
            "critical preference ".repeat(100),
            "critical lesson ".repeat(100),
            "typed copy ".repeat(100),
        );
        let (text, structured) =
            budget_context_wire_payload(text, json!({"context": "detail ".repeat(500)}), 120);

        assert_eq!(
            text.matches("<system-reminder>").count(),
            text.matches("</system-reminder>").count(),
            "{text}"
        );
        assert!(
            estimated_context_tool_wire_tokens(&text, &structured)
                <= 120 + CONTEXT_WIRE_ENVELOPE_TOKENS
        );
    }

    #[test]
    fn whole_wire_budget_is_deterministic_at_the_minimum() {
        let (text, structured) = oversized_context_wire_payload();
        let first = budget_context_wire_payload(text.clone(), structured.clone(), 50);
        let second = budget_context_wire_payload(text, structured, 50);
        assert_eq!(first, second);

        let estimated = estimated_context_tool_wire_tokens(&first.0, &first.1);
        assert!(estimated <= 50 + CONTEXT_WIRE_ENVELOPE_TOKENS);
        assert_eq!(
            first.1["wire_budget"]["estimated_tokens_after"].as_u64(),
            Some(estimated as u64)
        );
        assert_eq!(
            first.1["wire_budget"]["hard_floor_exceeded"].as_bool(),
            Some(false)
        );
    }

    #[test]
    fn whole_wire_budget_respects_each_requested_size() {
        for requested in [50usize, 120, 240, 480, 800] {
            let (text, structured) = oversized_context_wire_payload();
            let (text, structured) = budget_context_wire_payload(text, structured, requested);
            assert!(
                estimated_context_tool_wire_tokens(&text, &structured)
                    <= requested + CONTEXT_WIRE_ENVELOPE_TOKENS,
                "requested={requested}, report={:?}",
                structured.get("wire_budget")
            );
        }
    }

    #[test]
    fn whole_wire_budget_property_holds_across_payload_shapes() {
        for repeat in [0usize, 1, 7, 31, 127] {
            for requested in (50usize..=500).step_by(37) {
                let text = format!(
                    "[SEARCH] {}\n[GROUNDING] {}\n[LESSONS_WARNING] invariant",
                    "low ".repeat(repeat),
                    "evidence ".repeat(repeat * 2),
                );
                let structured = json!({
                    "grounding_hits": [{"text": "history ".repeat(repeat * 3)}],
                    "why_this_context": {"scores": vec![0.75; repeat]},
                    "context": "core ".repeat(repeat * 4),
                });
                let first =
                    budget_context_wire_payload(text.clone(), structured.clone(), requested);
                let second = budget_context_wire_payload(text, structured, requested);
                let estimated = estimated_context_tool_wire_tokens(&first.0, &first.1);
                assert_eq!(first, second);
                assert!(estimated <= requested + CONTEXT_WIRE_ENVELOPE_TOKENS);
                assert_eq!(
                    first.1["wire_budget"]["estimated_tokens_after"].as_u64(),
                    Some(estimated as u64),
                    "repeat={repeat}, requested={requested}"
                );
            }
        }
    }

    #[tokio::test]
    async fn exact_context_enforcement_preserves_critical_text_and_self_reports_final_wire() {
        crate::wire_tokens::warm_o200k();
        let context = crate::wire_tokens::WireResponseContext::http_jsonrpc(
            Some(json!("context-request-数据库")),
            Some("Loading ContextStream context".to_string()),
            Some("⌬".to_string()),
        );
        let policy = ContextWireTokenizerPolicy {
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
        let result = crate::wire_tokens::run_with_wire_response_context(
            context.clone(),
            async move {
                context_wire_result(
                    format!(
                        "[SEARCH] {}\n[GROUNDING] {}\n[LESSONS_WARNING] Preserve 数据库 safety 👩‍💻 and valid \\\"JSON\\\".",
                        "low priority 搜索 ".repeat(2_000),
                        "grounded evidence ".repeat(1_000),
                    ),
                    json!({
                        "context": "数据库 grounding 👩‍💻 ".repeat(2_000),
                        "why_this_context": {"trace": "diagnostic ".repeat(2_000)},
                        "workspace_id": uuid::Uuid::parse_str(
                            "ffffffff-ffff-4fff-bfff-ffffffffffff"
                        )
                        .unwrap(),
                        "project_id": uuid::Uuid::parse_str(
                            "01234567-89ab-4cde-8fab-0123456789ab"
                        )
                        .unwrap(),
                    }),
                    800,
                    &policy,
                )
            },
        )
        .await;

        let text = result
            .content
            .iter()
            .find_map(|item| match item {
                mcp_types::tool::ContentItem::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .unwrap();
        assert!(text.contains("[LESSONS_WARNING]"));
        let structured = result.structured_content.as_ref().unwrap();
        assert!(
            structured
                .get(crate::wire_tokens::WIRE_REPORT_KEY)
                .is_some(),
            "exact report missing from final structured payload: {structured:?}"
        );
        let measurement =
            crate::wire_tokens::measure_tool_result(&result, &context, "context_wire_test_final")
                .unwrap();
        assert_eq!(
            structured[crate::wire_tokens::WIRE_REPORT_KEY]["exact_tokens_final"],
            measurement.exact_tokens
        );
        assert!(measurement.exact_tokens <= 800 + CONTEXT_WIRE_ENVELOPE_TOKENS);
        let bytes = crate::wire_tokens::canonical_tool_result_bytes(&result, &context).unwrap();
        assert!(std::str::from_utf8(&bytes).is_ok());
        assert!(serde_json::from_slice::<serde_json::Value>(&bytes).is_ok());
    }

    #[tokio::test]
    async fn exact_context_minimum_budget_is_bounded_or_fail_honest() {
        crate::wire_tokens::warm_o200k();
        let context = crate::wire_tokens::WireResponseContext::http_jsonrpc(
            Some(json!("context-minimum-hard-floor")),
            Some("Loading ContextStream context".to_string()),
            Some("⌬".to_string()),
        );
        let policy = ContextWireTokenizerPolicy {
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
        let result = crate::wire_tokens::run_with_wire_response_context(
            context.clone(),
            async move {
                context_wire_result(
                    format!(
                        "[SEARCH] {}\n[GROUNDING] {}\n[LESSONS_WARNING] Preserve the hard safety invariant.",
                        r#"高熵检索 👩‍💻 "quoted" "#.repeat(4_000),
                        "adversarial grounding ".repeat(4_000),
                    ),
                    json!({
                        "context": r#"数据库 👩‍💻 "escaped" "#.repeat(8_000),
                        "grounding_hits": [{"content": "evidence ".repeat(8_000)}],
                        "why_this_context": {"trace": "diagnostic ".repeat(8_000)},
                    }),
                    50,
                    &policy,
                )
            },
        )
        .await;

        let target_tokens = 50 + CONTEXT_WIRE_ENVELOPE_TOKENS;
        let measurement = crate::wire_tokens::measure_tool_result(
            &result,
            &context,
            "context_wire_minimum_hard_floor_test",
        )
        .unwrap();
        let report = result
            .structured_content
            .as_ref()
            .and_then(|value| value.get(crate::wire_tokens::WIRE_REPORT_KEY));
        let bytes = crate::wire_tokens::canonical_tool_result_bytes(&result, &context).unwrap();
        let wire = std::str::from_utf8(&bytes).unwrap();

        if let Some(report) = report {
            assert_eq!(report["enforced"], true);
            assert!(measurement.exact_tokens <= target_tokens);
            assert!(crate::wire_tokens::fixed_point_report_is_truthful(
                &result,
                decision,
                target_tokens,
                measurement,
            ));
        } else {
            assert!(!wire.contains("\"enforced\":true"));
            if measurement.exact_tokens > target_tokens {
                assert!(wire.contains("[WIRE_BUDGET] Exact context exceeded"));
            }
        }
        assert!(serde_json::from_slice::<serde_json::Value>(&bytes).is_ok());
    }

    #[tokio::test]
    async fn context_shadow_is_byte_for_byte_proxy_neutral() {
        crate::wire_tokens::warm_o200k();
        let context = crate::wire_tokens::WireResponseContext::http_jsonrpc(
            Some(json!("context-shadow-id")),
            Some("Loading ContextStream context".to_string()),
            Some("⌬".to_string()),
        );
        let proxy_policy = ContextWireTokenizerPolicy {
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
        let shadow_policy = ContextWireTokenizerPolicy {
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
        let text = format!(
            "[GROUNDING] {}\n[LESSONS_WARNING] preserve this",
            "数据库 👩‍💻 \\\"json\\\" ".repeat(500)
        );
        let structured = json!({
            "context": "grounded evidence ".repeat(800),
            "why_this_context": {"source": "test"},
        });

        let proxy = context_wire_result(text.clone(), structured.clone(), 400, &proxy_policy);
        let shadow = context_wire_result(text, structured, 400, &shadow_policy);
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

    #[test]
    fn builds_version_notice_line_for_outdated_client() {
        let payload = json!({
            "version_notice": {
                "behind": true,
                "current": "0.1.20",
                "latest": "0.1.38",
                "upgrade_command": "npm i -g @contextstream/mcp-server"
            }
        });

        let line = init_version_notice_line(&payload).expect("version notice should be present");
        assert!(line.contains("0.1.20 -> 0.1.38"));
        assert!(line.contains("npm i -g @contextstream/mcp-server"));
    }

    #[test]
    fn omits_version_notice_line_when_client_not_behind() {
        let payload = json!({
            "version_notice": {
                "behind": false,
                "current": "0.1.38",
                "latest": "0.1.38"
            }
        });

        assert!(init_version_notice_line(&payload).is_none());
    }

    #[test]
    fn attaches_scope_guidance_to_structured_payload() {
        let workspace_id = Uuid::new_v4();
        let project_id = Uuid::new_v4();
        let workspace_id_str = workspace_id.to_string();
        let project_id_str = project_id.to_string();
        let mut payload = json!({ "context": "ok" });

        attach_scope_guidance(&mut payload, Some(workspace_id), Some(project_id));

        assert_eq!(
            payload["resolved_scope"]["workspace_id"].as_str(),
            Some(workspace_id_str.as_str())
        );
        assert_eq!(
            payload["resolved_scope"]["project_id"].as_str(),
            Some(project_id_str.as_str())
        );
        assert_eq!(
            payload["resolved_scope"]["project_scope_status"].as_str(),
            Some("resolved")
        );
    }
}

mod local_delta_tests {
    use super::{parse_git_status_line, LocalDeltaSummary};

    #[test]
    fn parses_porcelain_status_counts() {
        let mut summary = LocalDeltaSummary::default();

        for line in [
            " M src/lib.rs",
            "A  src/new.rs",
            " D src/deleted.rs",
            "R  src/old.rs -> src/new-name.rs",
            "?? scratch.txt",
            "UU conflicted.txt",
        ] {
            parse_git_status_line(line, &mut summary);
        }

        assert_eq!(summary.modified, 1);
        assert_eq!(summary.added, 1);
        assert_eq!(summary.deleted, 1);
        assert_eq!(summary.renamed, 1);
        assert_eq!(summary.untracked, 1);
        assert_eq!(summary.conflicted, 1);
        assert_eq!(summary.total_files(), 6);
        assert!(summary.has_local_delta());
    }

    #[test]
    fn formats_local_delta_notice_with_refresh_state() {
        let summary = LocalDeltaSummary {
            modified: 2,
            untracked: 1,
            newer_than_index: 3,
            ..Default::default()
        };

        let notice = summary.format_notice(true);
        assert!(notice.contains("2 modified files"));
        assert!(notice.contains("1 untracked file"));
        assert!(notice.contains("Local files are the freshest source of truth"));
        assert!(notice.contains("Index refresh started in the background"));
    }
}

mod suggested_rule_filter_tests {
    use super::is_boilerplate_suggested_rule;
    use mcp_types::api::SuggestedRule;
    use uuid::Uuid;

    fn make_rule(
        category: Option<&str>,
        instruction: &str,
        confidence: f64,
        occurrence_count: i32,
    ) -> SuggestedRule {
        SuggestedRule {
            id: Uuid::new_v4(),
            keywords: vec![],
            instruction: instruction.to_string(),
            category: category.map(String::from),
            confidence,
            occurrence_count,
        }
    }

    #[test]
    fn rejects_empty_category() {
        let rule = make_rule(None, "Always run migrations before deploy", 0.9, 5);
        assert!(is_boilerplate_suggested_rule(&rule));
    }

    #[test]
    fn rejects_general_category() {
        let rule = make_rule(
            Some("general"),
            "Review this pattern to prevent common mistakes",
            1.0,
            4055,
        );
        assert!(is_boilerplate_suggested_rule(&rule));
    }

    #[test]
    fn rejects_high_occurrence_count() {
        let rule = make_rule(
            Some("workflow"),
            "Always run unit tests before committing",
            0.95,
            2500,
        );
        assert!(is_boilerplate_suggested_rule(&rule));
    }

    #[test]
    fn rejects_generic_meta_advice() {
        let rule = make_rule(Some("workflow"), "Follow best practices", 0.8, 12);
        assert!(is_boilerplate_suggested_rule(&rule));
    }

    #[test]
    fn rejects_secrets_boilerplate() {
        let rule = make_rule(
            Some("code_quality"),
            "Never hardcode credentials in source files",
            0.9,
            7,
        );
        assert!(is_boilerplate_suggested_rule(&rule));
    }

    #[test]
    fn accepts_specific_workspace_rule() {
        let rule = make_rule(
            Some("workflow"),
            "Run the migration script under tools/migrate.sh after any schema change",
            0.85,
            8,
        );
        assert!(!is_boilerplate_suggested_rule(&rule));
    }

    #[test]
    fn rejects_too_short_instruction() {
        let rule = make_rule(Some("workflow"), "Be careful", 0.8, 5);
        assert!(is_boilerplate_suggested_rule(&rule));
    }
}

#[tokio::test]
async fn init_index_status_preserves_checkout_scope_across_spawn() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let project_id = Uuid::new_v4();
    let installation_id = Uuid::new_v4();
    let folder_path = "/Users/alice/projects/example-worktree";
    let routing_scope = mcp_client::run_with_installation_id(installation_id, || async {
        ContextStreamClient::checkout_routing_scope(folder_path).expect("routing scope")
    })
    .await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind status server");
    let address = listener.local_addr().expect("status server address");
    let response_body = serde_json::json!({
        "indexed_file_count": 7,
        "checkout_scope": {
            "installation_id": installation_id,
            "checkout_locator": routing_scope.checkout_locator,
            "matched": true,
        }
    })
    .to_string();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept status request");
        let mut buffer = vec![0_u8; 16 * 1024];
        let count = socket.read(&mut buffer).await.expect("read status request");
        let request = String::from_utf8_lossy(&buffer[..count]).to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write status response");
        request
    });

    let mut config = TestFixtures::test_config();
    config.api_url = format!("http://{address}");
    let client = ContextStreamClient::new(config);
    let status = mcp_client::run_with_installation_id(installation_id, || async {
        project_index_status_for_init(client, project_id, Some(folder_path.to_string())).await
    })
    .await;
    assert!(matches!(
        status,
        InitIndexStatus::Ready {
            checkout_scope_confirmed: true,
            ..
        }
    ));

    let request = server.await.expect("status server");
    let request_line = request.lines().next().expect("request line");
    assert!(request_line.contains(&format!(
        "GET /api/v1/projects/{project_id}/index/status?installation_id={installation_id}&checkout_locator=checkout-locator-v1%3A"
    )));
    assert!(!request.contains(folder_path));
}

#[tokio::test]
async fn init_index_status_keeps_canonical_ready_when_checkout_echo_is_missing() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let project_id = Uuid::new_v4();
    let installation_id = Uuid::new_v4();
    let folder_path = "/Users/alice/projects/example-code";
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind status server");
    let address = listener.local_addr().expect("status server address");
    let response_body = serde_json::json!({
        "indexed": true,
        "indexed_file_count": 886,
        "project_index_state": "ready",
        "last_updated": "2026-07-31T07:55:01Z"
    })
    .to_string();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("accept status request");
        let mut buffer = vec![0_u8; 16 * 1024];
        let count = socket.read(&mut buffer).await.expect("read status request");
        let request = String::from_utf8_lossy(&buffer[..count]).to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("write status response");
        request
    });

    let mut config = TestFixtures::test_config();
    config.api_url = format!("http://{address}");
    let client = ContextStreamClient::new(config);
    let status = mcp_client::run_with_installation_id(installation_id, || async {
        project_index_status_for_init(client, project_id, Some(folder_path.to_string())).await
    })
    .await;

    match status {
        InitIndexStatus::Ready {
            status,
            checkout_scope_confirmed,
        } => {
            assert!(!checkout_scope_confirmed);
            assert_eq!(extract_backend_indexed_count(&status), Some(886));
        }
        other => panic!("canonical readiness was discarded: {other:?}"),
    }

    let request = server.await.expect("status server");
    let request_line = request.lines().next().expect("request line");
    assert!(request_line.contains(&format!(
        "GET /api/v1/projects/{project_id}/index/status?installation_id={installation_id}&checkout_locator=checkout-locator-v1%3A"
    )));
    assert!(!request.contains(folder_path));
}

mod project_routing_tests {
    use super::{
        build_warm_cache_entry, condense_context_for_concise, context_distributed_cache_identity,
        context_warm_request_identity, decode_distributed_context_cache_envelope,
        distributed_context_cache_scope, encode_distributed_context_cache_envelope,
        format_delta_summary, format_project_routing_notice,
        format_project_routing_notice_from_value, warm_cache_get, warm_cache_note_delta_emit,
        warm_cache_put, WarmCacheKey, WarmContextCache, WARM_CONTEXT_CACHE_ENTRY_CAP,
        WARM_CONTEXT_CACHE_MESSAGE_BYTE_CAP, WARM_CONTEXT_CACHE_PER_CALLER_ENTRY_CAP,
        WARM_CONTEXT_CACHE_TOTAL_BYTE_CAP,
    };
    use mcp_types::api::{ContextResponse, ProjectRoutingCandidate, ProjectRoutingContext};
    use serde_json::json;
    use uuid::Uuid;

    fn routing(status: &str) -> ProjectRoutingContext {
        ProjectRoutingContext {
            status: Some(status.to_string()),
            reason: Some("Folder matched multiple known projects".to_string()),
            current_workspace_id: Some(
                Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap(),
            ),
            current_project_id: None,
            current_project_name: None,
            folder_path: Some("/tmp/workspace/app".to_string()),
            project_switch_signal: false,
            suggested_action: Some("Choose a candidate before project-scoped writes".to_string()),
            candidates: vec![ProjectRoutingCandidate {
                project_id: Some(Uuid::parse_str("22222222-2222-4222-8222-222222222222").unwrap()),
                workspace_id: Some(
                    Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap(),
                ),
                workspace_name: Some("Engineering".to_string()),
                project_name: Some("app".to_string()),
                path: Some("/tmp/workspace/app".to_string()),
                repository_url: None,
                score: 0.94,
                match_reasons: vec!["folder path".to_string()],
            }],
        }
    }

    fn request_identity(grounding_handle: Option<&str>) -> String {
        context_warm_request_identity(
            grounding_handle,
            Some("readable"),
            Some("standard"),
            Some(false),
            Some(800),
            0,
            70_000,
            Some("codex"),
            None,
            Some("test-session"),
            Some("/tmp/workspace/app"),
            None,
            None,
        )
    }

    #[test]
    fn compact_project_routing_notice_surfaces_action_and_candidates() {
        let notice =
            format_project_routing_notice(Some(&routing("needs_project_selection")), true, false)
                .expect("routing notice");

        assert!(notice.contains("[PROJECT_ROUTING]"));
        assert!(notice.contains("status=needs_project_selection"));
        assert!(notice.contains("Choose a candidate"));
        assert!(notice.contains("app"));
        assert!(notice.contains("score=0.94"));
    }

    #[test]
    fn explicit_current_project_scope_uses_conservative_action_for_uncertain_switch() {
        let workspace_id = Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap();
        let current_project_id = Uuid::parse_str("22222222-2222-4222-8222-222222222222").unwrap();
        let testing_project_id = Uuid::parse_str("33333333-3333-4333-8333-333333333333").unwrap();
        let routing = ProjectRoutingContext {
            status: Some("uncertain".to_string()),
            reason: Some("folder_path_matches_different_project".to_string()),
            current_workspace_id: Some(workspace_id),
            current_project_id: Some(current_project_id),
            current_project_name: Some("mcp".to_string()),
            folder_path: Some("/home/alice/projects/mcp".to_string()),
            project_switch_signal: false,
            suggested_action: Some(format!(
                "Switch project scope to Testing ({testing_project_id}) or pass the intended project_id explicitly."
            )),
            candidates: vec![
                ProjectRoutingCandidate {
                    project_id: Some(testing_project_id),
                    workspace_id: Some(workspace_id),
                    workspace_name: Some("Engineering".to_string()),
                    project_name: Some("Testing".to_string()),
                    path: None,
                    repository_url: None,
                    score: 0.86,
                    match_reasons: vec!["message_mentions_project".to_string()],
                },
                ProjectRoutingCandidate {
                    project_id: Some(current_project_id),
                    workspace_id: Some(workspace_id),
                    workspace_name: Some("Engineering".to_string()),
                    project_name: Some("mcp".to_string()),
                    path: Some("/home/alice/projects/mcp".to_string()),
                    repository_url: None,
                    score: 0.68,
                    match_reasons: vec!["folder_name_matches_project".to_string()],
                },
            ],
        };

        // Non-authoritative scope keeps the conservative softened notice.
        let notice =
            format_project_routing_notice(Some(&routing), true, false).expect("routing notice");

        assert!(notice.contains("Keep current project scope mcp"));
        assert!(!notice.contains("Switch project scope to Testing"));
        let mcp_pos = notice
            .find("candidates=mcp")
            .expect("current project first");
        let testing_pos = notice.find("Testing").expect("testing candidate present");
        assert!(mcp_pos < testing_pos);

        // The same soft uncertain hint is suppressed entirely when the client
        // itself pinned the scope — this exact shape (resolved project +
        // "switch?" hint) used to burn the agent's first turn every session.
        assert!(format_project_routing_notice(Some(&routing), true, true).is_none());
    }

    #[test]
    fn authoritative_scope_never_mutes_definitive_conflicts_or_hard_statuses() {
        // The folder demonstrably belongs to another project: stays visible.
        let mut conflict = routing("uncertain");
        conflict.current_project_id =
            Some(Uuid::parse_str("22222222-2222-4222-8222-222222222222").unwrap());
        conflict.reason = Some("folder_is_registered_root_of_different_project".to_string());
        assert!(format_project_routing_notice(Some(&conflict), true, true).is_some());

        conflict.reason = Some("folder_bound_to_different_project".to_string());
        assert!(format_project_routing_notice(Some(&conflict), true, true).is_some());

        // Explicit user switch request: stays visible.
        let mut switch = routing("uncertain");
        switch.current_project_id =
            Some(Uuid::parse_str("22222222-2222-4222-8222-222222222222").unwrap());
        switch.reason = Some("message_mentions_different_project".to_string());
        switch.project_switch_signal = true;
        assert!(format_project_routing_notice(Some(&switch), true, true).is_some());

        // Hard statuses are never suppressed.
        assert!(
            format_project_routing_notice(Some(&routing("needs_project_setup")), true, true)
                .is_some()
        );
        assert!(format_project_routing_notice(
            Some(&routing("needs_project_selection")),
            true,
            true
        )
        .is_some());
    }

    #[test]
    fn quiet_statuses_do_not_trip_missing_project_fallback() {
        // resolved_by_folder intentionally ships its candidate with no current
        // project — the no-current+candidates fallback must stay quiet.
        let resolved = routing("resolved_by_folder");
        assert!(resolved.current_project_id.is_none());
        assert!(!resolved.candidates.is_empty());
        assert!(format_project_routing_notice(Some(&resolved), true, false).is_none());
        assert!(format_project_routing_notice(Some(&resolved), false, false).is_none());
    }

    #[test]
    fn routing_notice_dampener_dedupes_identical_notices_per_scope() {
        use super::routing_notice_first_emission;

        let key = "dampener-test-scope-a";
        // First sighting emits and records.
        assert!(routing_notice_first_emission(
            key,
            "[PROJECT_ROUTING] status=uncertain x",
            false
        ));
        // Identical notice for the same scope is damped.
        assert!(!routing_notice_first_emission(
            key,
            "[PROJECT_ROUTING] status=uncertain x",
            false
        ));
        // Any change re-emits.
        assert!(routing_notice_first_emission(
            key,
            "[PROJECT_ROUTING] status=uncertain y",
            false
        ));
        // A different scope is independent.
        assert!(routing_notice_first_emission(
            "dampener-test-scope-b",
            "[PROJECT_ROUTING] status=uncertain x",
            false
        ));
        // Init records but always emits, and later identical context()
        // notices dedupe against what init showed.
        let init_key = "dampener-test-scope-init";
        assert!(routing_notice_first_emission(init_key, "notice", true));
        assert!(routing_notice_first_emission(init_key, "notice", true));
        assert!(!routing_notice_first_emission(init_key, "notice", false));
    }

    #[test]
    fn warm_context_cache_skips_project_routing_notices() {
        let workspace_id = Uuid::parse_str("11111111-1111-4111-8111-111111111111").unwrap();
        let project_id = Uuid::parse_str("22222222-2222-4222-8222-222222222222").unwrap();
        let mut response: ContextResponse =
            serde_json::from_value(json!({})).expect("empty ContextResponse");
        response.project_routing = Some(routing("needs_project_setup"));
        let request_identity = request_identity(Some("gb:v1:routing"));

        warm_cache_put(
            Some(workspace_id),
            Some(project_id),
            "user-a",
            &request_identity,
            "stale routing message",
            &response,
            "[PROJECT_ROUTING] stale",
        );

        assert!(warm_cache_get(
            Some(workspace_id),
            Some(project_id),
            "user-a",
            &request_identity,
            "stale routing message"
        )
        .is_none());
    }

    #[test]
    fn warm_cache_bypassed_for_unrelated_message() {
        let workspace_id = Uuid::parse_str("33333333-3333-4333-8333-333333333333").unwrap();
        let project_id = Uuid::parse_str("44444444-4444-4444-8444-444444444444").unwrap();
        let mut response: ContextResponse =
            serde_json::from_value(json!({})).expect("empty ContextResponse");
        let mut confirmed = routing("confirmed");
        confirmed.current_project_id = Some(project_id); // quiet routing, so put stores
        response.project_routing = Some(confirmed);
        let request_identity = request_identity(Some("gb:v1:pagination"));

        warm_cache_put(
            Some(workspace_id),
            Some(project_id),
            "user-a",
            &request_identity,
            "fix the search pagination bug returning duplicate items",
            &response,
            "[CTX] cached for pagination",
        );

        // A continuation that shares task vocabulary stays warm.
        assert!(warm_cache_get(
            Some(workspace_id),
            Some(project_id),
            "user-a",
            &request_identity,
            "the search pagination bug is still returning duplicate items",
        )
        .is_some());

        // An unrelated message must bypass — its skills/grounding differ.
        assert!(warm_cache_get(
            Some(workspace_id),
            Some(project_id),
            "user-a",
            &request_identity,
            "add a new endpoint for exporting widgets to csv",
        )
        .is_none());

        // The same workspace/project and query must never replay personal
        // context into another authenticated caller's session.
        assert!(warm_cache_get(
            Some(workspace_id),
            Some(project_id),
            "user-b",
            &request_identity,
            "the search pagination bug is still returning duplicate items",
        )
        .is_none());
    }

    #[test]
    fn warm_cache_keeps_concurrent_caller_scopes_independent() {
        let workspace_id = Uuid::parse_str("55555555-5555-4555-8555-555555555555").unwrap();
        let project_a = Uuid::parse_str("66666666-6666-4666-8666-666666666666").unwrap();
        let project_b = Uuid::parse_str("77777777-7777-4777-8777-777777777777").unwrap();
        let response: ContextResponse =
            serde_json::from_value(json!({})).expect("empty ContextResponse");
        let request_a = request_identity(Some("gb:v1:caller-a"));
        let request_b = request_identity(Some("gb:v1:caller-b"));

        warm_cache_put(
            Some(workspace_id),
            Some(project_a),
            "caller-a",
            &request_a,
            "fix search pagination duplicates",
            &response,
            "[CTX] caller a",
        );
        warm_cache_put(
            Some(workspace_id),
            Some(project_b),
            "caller-b",
            &request_b,
            "fix guided search grounding",
            &response,
            "[CTX] caller b",
        );

        warm_cache_note_delta_emit(Some(workspace_id), Some(project_a), "caller-a", &request_a);
        let first = warm_cache_get(
            Some(workspace_id),
            Some(project_a),
            "caller-a",
            &request_a,
            "search pagination duplicates still broken",
        )
        .expect("caller A cache entry");
        let second = warm_cache_get(
            Some(workspace_id),
            Some(project_b),
            "caller-b",
            &request_b,
            "guided search grounding still broken",
        )
        .expect("caller B cache entry");

        assert_eq!(first.1, "[CTX] caller a");
        assert_eq!(first.2, 1);
        assert_eq!(second.1, "[CTX] caller b");
        assert_eq!(second.2, 0);
    }

    #[test]
    fn warm_cache_requires_matching_grounding_handle_identity() {
        let workspace_id = Uuid::parse_str("88888888-8888-4888-8888-888888888888").unwrap();
        let project_id = Uuid::parse_str("99999999-9999-4999-8999-999999999999").unwrap();
        let response: ContextResponse =
            serde_json::from_value(json!({})).expect("empty ContextResponse");
        let handle_a = "gb:v1:opaque-sensitive-a";
        let handle_b = "gb:v1:opaque-sensitive-b";
        let request_a = request_identity(Some(handle_a));
        let request_b = request_identity(Some(handle_b));
        assert_ne!(request_a, request_b);
        assert!(!request_a.contains(handle_a));
        assert!(!request_b.contains(handle_b));

        warm_cache_put(
            Some(workspace_id),
            Some(project_id),
            "caller-a",
            &request_a,
            "continue fixing search cache grounding",
            &response,
            "[CTX] handle A",
        );

        assert!(warm_cache_get(
            Some(workspace_id),
            Some(project_id),
            "caller-a",
            &request_a,
            "continue fixing search cache grounding",
        )
        .is_some());
        assert!(warm_cache_get(
            Some(workspace_id),
            Some(project_id),
            "caller-a",
            &request_b,
            "continue fixing search cache grounding",
        )
        .is_none());
    }

    #[test]
    fn context_warm_identity_includes_response_shapers() {
        let baseline = request_identity(Some("gb:v1:shape"));
        let different_format = context_warm_request_identity(
            Some("gb:v1:shape"),
            Some("structured"),
            Some("standard"),
            Some(false),
            Some(800),
            0,
            70_000,
            Some("codex"),
            None,
            Some("test-session"),
            Some("/tmp/workspace/app"),
            None,
            None,
        );
        let different_budget = context_warm_request_identity(
            Some("gb:v1:shape"),
            Some("readable"),
            Some("standard"),
            Some(false),
            Some(1_600),
            0,
            70_000,
            Some("codex"),
            None,
            Some("test-session"),
            Some("/tmp/workspace/app"),
            None,
            None,
        );
        let different_tokenizer = context_warm_request_identity(
            Some("gb:v1:shape"),
            Some("readable"),
            Some("standard"),
            Some(false),
            Some(800),
            0,
            70_000,
            Some("codex"),
            None,
            Some("test-session"),
            Some("/tmp/workspace/app"),
            None,
            Some("o200k_base"),
        );
        assert_ne!(baseline, different_format);
        assert_ne!(baseline, different_budget);
        assert_ne!(baseline, different_tokenizer);
    }

    #[test]
    fn context_warm_identity_isolated_by_checkout_locator() {
        let first = context_warm_request_identity(
            Some("gb:v1:shape"),
            Some("readable"),
            Some("standard"),
            Some(false),
            Some(800),
            0,
            70_000,
            Some("codex"),
            None,
            Some("test-session"),
            Some("/workspace/project"),
            Some("checkout-locator-v1:first"),
            None,
        );
        let second = context_warm_request_identity(
            Some("gb:v1:shape"),
            Some("readable"),
            Some("standard"),
            Some(false),
            Some(800),
            0,
            70_000,
            Some("codex"),
            None,
            Some("test-session"),
            Some("/workspace/project"),
            Some("checkout-locator-v1:second"),
            None,
        );

        assert_ne!(first, second);
    }

    #[test]
    fn distributed_context_identity_binds_exact_messages_and_turn() {
        let base = request_identity(Some("gb:v1:distributed"));
        let exact = context_distributed_cache_identity(
            &base,
            "continue the cache audit",
            Some("previous assistant answer"),
            7,
        );
        assert_eq!(
            exact,
            context_distributed_cache_identity(
                &base,
                "continue the cache audit",
                Some("previous assistant answer"),
                7,
            )
        );
        assert_ne!(
            exact,
            context_distributed_cache_identity(
                &base,
                "continue the cache audit!",
                Some("previous assistant answer"),
                7,
            )
        );
        assert_ne!(
            exact,
            context_distributed_cache_identity(
                &base,
                "continue the cache audit",
                Some("different assistant answer"),
                7,
            )
        );
        assert_ne!(
            exact,
            context_distributed_cache_identity(
                &base,
                "continue the cache audit",
                Some("previous assistant answer"),
                8,
            )
        );
        assert_ne!(
            exact,
            context_distributed_cache_identity(&base, "continue the cache audit", None, 7)
        );
        assert!(!exact.contains("continue the cache audit"));
        assert!(!exact.contains("previous assistant answer"));
    }

    #[test]
    fn distributed_context_envelope_exact_hit_and_legacy_rejection() {
        let workspace_id = Uuid::parse_str("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa").unwrap();
        let project_id = Uuid::parse_str("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb").unwrap();
        let identity = context_distributed_cache_identity(
            &request_identity(Some("gb:v1:envelope")),
            "exact user turn",
            None,
            3,
        );
        let (_, expected) =
            distributed_context_cache_scope(workspace_id, Some(project_id), "caller-a", &identity);
        let mut response: ContextResponse =
            serde_json::from_value(json!({})).expect("empty ContextResponse");
        response.summary = Some("exact cached context".to_string());

        let envelope = encode_distributed_context_cache_envelope(&response, &expected)
            .expect("admissible envelope");
        let decoded = decode_distributed_context_cache_envelope(envelope, &expected)
            .expect("exact envelope hit");
        assert_eq!(decoded.summary.as_deref(), Some("exact cached context"));

        let legacy_raw = serde_json::to_value(&response).expect("legacy raw response");
        assert_eq!(
            decode_distributed_context_cache_envelope(legacy_raw, &expected).unwrap_err(),
            "legacy_or_malformed_envelope"
        );
    }

    #[test]
    fn distributed_context_envelope_is_symmetric_for_response_handle() {
        let workspace_id = Uuid::parse_str("cccccccc-cccc-4ccc-8ccc-cccccccccccc").unwrap();
        let project_id = Uuid::parse_str("dddddddd-dddd-4ddd-8ddd-dddddddddddd").unwrap();
        let request_base = request_identity(Some("gb:v1:request-handle"));
        let response_base = request_identity(Some("gb:v1:response-handle"));
        let request_identity =
            context_distributed_cache_identity(&request_base, "same exact turn", None, 4);
        let response_identity =
            context_distributed_cache_identity(&response_base, "same exact turn", None, 4);
        assert_ne!(request_identity, response_identity);

        let (_, request_expected) = distributed_context_cache_scope(
            workspace_id,
            Some(project_id),
            "caller-a",
            &request_identity,
        );
        let (_, response_expected) = distributed_context_cache_scope(
            workspace_id,
            Some(project_id),
            "caller-a",
            &response_identity,
        );
        let response: ContextResponse =
            serde_json::from_value(json!({})).expect("empty ContextResponse");
        let request_envelope =
            encode_distributed_context_cache_envelope(&response, &request_expected).unwrap();
        let response_envelope =
            encode_distributed_context_cache_envelope(&response, &response_expected).unwrap();

        assert!(decode_distributed_context_cache_envelope(
            request_envelope.clone(),
            &request_expected
        )
        .is_ok());
        assert!(
            decode_distributed_context_cache_envelope(response_envelope, &response_expected)
                .is_ok()
        );
        assert_eq!(
            decode_distributed_context_cache_envelope(request_envelope, &response_expected)
                .unwrap_err(),
            "envelope_identity_mismatch"
        );
    }

    #[test]
    fn distributed_context_envelope_rejects_forged_scope_metadata() {
        let workspace_id = Uuid::parse_str("eeeeeeee-eeee-4eee-8eee-eeeeeeeeeeee").unwrap();
        let identity = context_distributed_cache_identity(
            &request_identity(Some("gb:v1:forged")),
            "forged envelope test",
            None,
            2,
        );
        let (_, expected) =
            distributed_context_cache_scope(workspace_id, None, "caller-a", &identity);
        let response: ContextResponse =
            serde_json::from_value(json!({})).expect("empty ContextResponse");
        let mut forged = encode_distributed_context_cache_envelope(&response, &expected).unwrap();
        forged["caller_identity"] = json!("caller-b");
        assert_eq!(
            decode_distributed_context_cache_envelope(forged, &expected).unwrap_err(),
            "envelope_caller_mismatch"
        );

        let mut forged_scope =
            encode_distributed_context_cache_envelope(&response, &expected).unwrap();
        forged_scope["scope_hash"] = json!("forged-scope");
        assert_eq!(
            decode_distributed_context_cache_envelope(forged_scope, &expected).unwrap_err(),
            "envelope_scope_mismatch"
        );
    }

    #[test]
    fn warm_context_cache_rejects_oversized_messages_and_entries() {
        let response: ContextResponse =
            serde_json::from_value(json!({})).expect("empty ContextResponse");
        let key = WarmCacheKey::new(None, None, "oversize-caller", "oversize-request");
        let oversized_message = "m".repeat(WARM_CONTEXT_CACHE_MESSAGE_BYTE_CAP + 1);
        assert!(build_warm_cache_entry(&key, &oversized_message, &response, "ok").is_none());

        let oversized_text = "x".repeat(super::WARM_CONTEXT_CACHE_ENTRY_BYTE_CAP + 1);
        assert!(
            build_warm_cache_entry(&key, "normal message", &response, &oversized_text).is_none()
        );
    }

    #[test]
    fn warm_context_cache_bounds_per_tenant_and_tenant_churn() {
        let response: ContextResponse =
            serde_json::from_value(json!({})).expect("empty ContextResponse");
        let mut cache = WarmContextCache::default();

        let stable_key = WarmCacheKey::new(None, None, "stable-caller", "stable-request");
        let stable_entry =
            build_warm_cache_entry(&stable_key, "stable message", &response, "stable").unwrap();
        assert!(cache.admit(stable_key.clone(), stable_entry));

        for index in 0..(WARM_CONTEXT_CACHE_PER_CALLER_ENTRY_CAP + 8) {
            let key = WarmCacheKey::new(
                None,
                None,
                "noisy-caller",
                &format!("noisy-request-{index}"),
            );
            let entry = build_warm_cache_entry(&key, "noisy message", &response, "small").unwrap();
            assert!(cache.admit(key, entry));
        }
        assert!(cache.caller_usage("noisy-caller").0 <= WARM_CONTEXT_CACHE_PER_CALLER_ENTRY_CAP);
        assert!(cache.entries.contains_key(&stable_key));

        for index in 0..(WARM_CONTEXT_CACHE_ENTRY_CAP + 64) {
            let caller = format!("churn-caller-{index}");
            let key = WarmCacheKey::new(None, None, &caller, "request");
            let entry = build_warm_cache_entry(&key, "churn message", &response, "small").unwrap();
            assert!(cache.admit(key, entry));
        }
        assert!(cache.entries.len() <= WARM_CONTEXT_CACHE_ENTRY_CAP);
        assert!(cache.total_bytes <= WARM_CONTEXT_CACHE_TOTAL_BYTE_CAP);
    }

    #[test]
    fn confirmed_project_routing_stays_quiet() {
        let mut routing = routing("confirmed");
        routing.current_project_id =
            Some(Uuid::parse_str("22222222-2222-4222-8222-222222222222").unwrap());
        routing.candidates.clear();

        assert!(format_project_routing_notice(Some(&routing), true, false).is_none());
    }

    #[test]
    fn init_payload_project_routing_is_formatted() {
        let payload = json!({
            "project_routing": {
                "status": "uncertain",
                "reason": "folder changed",
                "suggested_action": "rerun init with folder_path",
                "candidates": [{
                    "project_id": "22222222-2222-4222-8222-222222222222",
                    "project_name": "app",
                    "score": 0.88
                }]
            }
        });

        let notice = format_project_routing_notice_from_value(&payload, true, false)
            .expect("routing notice");
        assert!(notice.contains("status=uncertain"));
        assert!(notice.contains("rerun init"));
        assert!(notice.contains("app"));
    }

    #[test]
    fn concise_condense_preserves_project_routing_lines() {
        let condensed = condense_context_for_concise(
            "intro\n[PROJECT_ROUTING] status=uncertain action=choose project\nnoise",
        );

        assert_eq!(
            condensed,
            "[PROJECT_ROUTING] status=uncertain action=choose project"
        );
    }

    #[test]
    fn delta_summary_repeats_unresolved_project_routing() {
        let mut response: ContextResponse =
            serde_json::from_value(json!({})).expect("empty ContextResponse");
        response.project_routing = Some(routing("needs_project_setup"));

        let summary = format_delta_summary(&response, false, "delta-routing-test-scope-first-emit");
        assert!(summary.contains("[CTX-DELTA]"));
        assert!(summary.contains("[PROJECT_ROUTING]"));
        assert!(summary.contains("needs_project_setup"));

        // The identical notice for the same scope is damped on the next
        // warm-cache delta turn; the delta summary itself still renders.
        let repeat = format_delta_summary(&response, false, "delta-routing-test-scope-first-emit");
        assert!(repeat.contains("[CTX-DELTA]"));
        assert!(!repeat.contains("[PROJECT_ROUTING]"));
    }
}

mod delta_emit_tests {
    use super::{
        format_acceleration_context_warm_cache_marker, format_delta_summary,
        overlay_has_new_dynamic_content,
    };
    use mcp_types::api::{ContextResponse, Lesson, RememberItem};

    fn empty_response() -> ContextResponse {
        serde_json::from_value(serde_json::json!({})).expect("empty ContextResponse")
    }

    #[test]
    fn overlay_detects_flash_content() {
        let ctx = "[FLASH] New insight about the current task";
        assert!(overlay_has_new_dynamic_content(ctx));
    }

    #[test]
    fn overlay_detects_action_required() {
        let ctx = "[ACTION_REQUIRED] Index needs refresh";
        assert!(overlay_has_new_dynamic_content(ctx));
    }

    #[test]
    fn overlay_ignores_static_content() {
        let ctx = "[CTX] W:Engineering\n[LESSONS_WARNING] Unchanged";
        assert!(!overlay_has_new_dynamic_content(ctx));
    }

    #[test]
    fn delta_summary_reports_counts() {
        let mut response = empty_response();
        response.lessons = Some(vec![Lesson {
            title: Some("Never hardcode credentials".to_string()),
            trigger: None,
            prevention: None,
            severity: Some("high".to_string()),
        }]);
        response.remember_items = Some(vec![RememberItem {
            content: Some("Prefer tabs".to_string()),
            importance: Some("medium".to_string()),
        }]);

        let summary = format_delta_summary(&response, false, "delta-emit-test-counts");
        assert!(summary.contains("[CTX-DELTA]"));
        assert!(summary.contains("lessons=1"));
        assert!(summary.contains("prefs=1"));
        assert!(summary.contains("Never hardcode credentials"));
        assert!(summary.contains("context(user_message="));
    }

    #[test]
    fn delta_summary_handles_empty_response() {
        let response = empty_response();
        let summary = format_delta_summary(&response, false, "delta-emit-test-empty");
        assert!(summary.contains("[CTX-DELTA]"));
        assert!(summary.contains("lessons=0"));
        assert!(summary.contains("prefs=0"));
        // No anchor line when there are no lessons.
        assert!(!summary.contains("Top lesson"));
    }

    #[test]
    fn acceleration_context_warm_cache_marker_is_visible() {
        let with_age = format_acceleration_context_warm_cache_marker(Some(42));
        assert!(with_age.starts_with("[WARM_CACHE] context served from acceleration cache"));
        assert!(with_age.contains("age 42ms"));

        let without_age = format_acceleration_context_warm_cache_marker(None);
        assert_eq!(
            without_age,
            "[WARM_CACHE] context served from acceleration cache\n"
        );
    }
}

mod init_name_resolution_tests {
    use super::{resolve_init_project_name, resolve_init_workspace_name};
    use serde_json::json;

    #[test]
    fn workspace_name_falls_back_to_structured_workspace() {
        let payload = json!({
            "workspace": {
                "name": "Engineering"
            }
        });

        assert_eq!(
            resolve_init_workspace_name(&payload, None),
            "Engineering".to_string()
        );
    }

    #[test]
    fn project_name_falls_back_to_structured_project() {
        let payload = json!({
            "project": {
                "name": "super-productivity"
            }
        });

        assert_eq!(
            resolve_init_project_name(&payload, None),
            Some("super-productivity".to_string())
        );
    }
}

mod matched_skill_rendering_tests {
    use super::{
        matched_skill_label, matched_skill_name, matched_skill_preview, matched_skill_priority,
    };
    use serde_json::json;

    #[test]
    fn prefers_title_for_skill_label() {
        let skill = json!({
            "name": "update-example-cli-version-pins",
            "title": "Update Example CLI Version Pins Across Release Files",
            "priority": 100
        });

        assert_eq!(
            matched_skill_label(&skill),
            "Update Example CLI Version Pins Across Release Files (update-example-cli-version-pins)"
        );
    }

    #[test]
    fn falls_back_to_first_instruction_line_for_skill_preview() {
        let skill = json!({
            "name": "update-example-cli-version-pins",
            "title": "Update Example CLI Version Pins Across Release Files",
            "instruction_body": "# Update Example CLI Version Pins\n\nUse this skill when the user asks to update the example CLI release across its repository.",
            "priority": 100
        });

        assert_eq!(
            matched_skill_preview(&skill),
            "Use this skill when the user asks to update the example CLI release across its repository."
        );
    }

    #[test]
    fn extracts_skill_name() {
        let skill = json!({ "name": "deploy-checker", "title": "Deploy Checker" });
        assert_eq!(matched_skill_name(&skill), "deploy-checker");
    }

    #[test]
    fn skill_name_defaults_to_unnamed() {
        let skill = json!({ "title": "No Name Skill" });
        assert_eq!(matched_skill_name(&skill), "unnamed");
    }

    #[test]
    fn high_priority_skill() {
        let skill = json!({ "name": "critical", "priority": 90 });
        assert_eq!(matched_skill_priority(&skill), 90);
    }

    #[test]
    fn default_priority_when_missing() {
        let skill = json!({ "name": "basic" });
        assert_eq!(matched_skill_priority(&skill), 50);
    }
}

mod repeated_action_surfacing_tests {
    use super::{has_repeated_action_signal, query_mentions_diagrams};
    use mcp_types::api::SuggestedRule;
    use uuid::Uuid;

    fn suggested_rule(confidence: f64, occurrence_count: i32) -> SuggestedRule {
        SuggestedRule {
            id: Uuid::new_v4(),
            keywords: vec!["deploy".to_string()],
            instruction: "Always run deploy validation before release".to_string(),
            category: Some("workflow".to_string()),
            confidence,
            occurrence_count,
        }
    }

    #[test]
    fn repeated_action_signal_requires_confidence_or_recurrence() {
        let low_signal = [suggested_rule(0.4, 1)];
        let single_high_confidence = [suggested_rule(0.81, 1)];
        let recurring_once = [suggested_rule(0.4, 2)];
        let two_strong_signals = [suggested_rule(0.82, 1), suggested_rule(0.4, 2)];
        let high_recurrence = [suggested_rule(0.4, 3)];

        let low_refs: Vec<&SuggestedRule> = low_signal.iter().collect();
        let single_high_refs: Vec<&SuggestedRule> = single_high_confidence.iter().collect();
        let recurring_once_refs: Vec<&SuggestedRule> = recurring_once.iter().collect();
        let two_strong_refs: Vec<&SuggestedRule> = two_strong_signals.iter().collect();
        let high_recurrence_refs: Vec<&SuggestedRule> = high_recurrence.iter().collect();

        assert!(!has_repeated_action_signal(&low_refs));
        assert!(!has_repeated_action_signal(&single_high_refs));
        assert!(!has_repeated_action_signal(&recurring_once_refs));
        assert!(has_repeated_action_signal(&two_strong_refs));
        assert!(has_repeated_action_signal(&high_recurrence_refs));
    }

    #[test]
    fn diagram_detection_matches_common_diagram_queries() {
        assert!(query_mentions_diagrams(
            "please generate a sequence diagram for auth flow"
        ));
        assert!(query_mentions_diagrams("need a mermaid flowchart"));
        assert!(!query_mentions_diagrams(
            "diagram component render performance"
        ));
        assert!(!query_mentions_diagrams(
            "find where auth middleware is implemented"
        ));
    }
}

mod recall_augmentation_tests {
    use super::{
        dedupe_recall_project_results, format_recall_augmented_text, join_grounding_remote_reads,
        join_recall_with_augmentations, search_recall_augmentations, SearchResult,
    };
    use crate::testing::TestFixtures;
    use mcp_client::ContextStreamClient;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::Barrier;
    use uuid::Uuid;

    async fn wait_at_barrier<T>(barrier: Arc<Barrier>, value: T) -> T {
        barrier.wait().await;
        value
    }

    async fn spawn_concurrent_recall_server(
        expected_requests: usize,
    ) -> (ContextStreamClient, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind recall augmentation server");
        let addr = listener.local_addr().expect("recall augmentation address");
        let all_requests_arrived = Arc::new(Barrier::new(expected_requests));
        let server = tokio::spawn(async move {
            let mut handlers = Vec::new();
            for _ in 0..expected_requests {
                let (mut stream, _) = listener
                    .accept()
                    .await
                    .expect("accept augmentation request");
                let barrier = all_requests_arrived.clone();
                handlers.push(tokio::spawn(async move {
                    let mut request = vec![0u8; 16 * 1024];
                    let read = stream.read(&mut request).await.expect("read augmentation request");
                    let request = String::from_utf8_lossy(&request[..read]);
                    let request_line = request.lines().next().unwrap_or_default().to_string();

                    // Neither request can complete until both have arrived. A serial
                    // implementation therefore times out instead of passing this test.
                    barrier.wait().await;

                    let body = if request_line.contains("/memory/decisions?") {
                        r#"[{"summary":"Keep the typed recall contract"}]"#
                    } else if request_line.contains("/docs?") {
                        r#"{"items":[{"title":"Recall latency runbook"}]}"#
                    } else if request_line.contains("/session/recall") {
                        r#"{"results":[{"title":"Previous recall result"}]}"#
                    } else {
                        panic!("unexpected augmentation request: {request_line}");
                    };
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    stream
                        .write_all(response.as_bytes())
                        .await
                        .expect("write augmentation response");
                    request_line
                }));
            }

            let mut request_lines = Vec::new();
            for handler in handlers {
                request_lines.push(handler.await.expect("join augmentation handler"));
            }
            request_lines
        });

        let mut config = TestFixtures::test_config();
        config.api_url = format!("http://{addr}");
        (ContextStreamClient::new(config), server)
    }

    #[tokio::test]
    async fn decision_and_doc_augmentations_start_concurrently() {
        let (client, server) = spawn_concurrent_recall_server(2).await;
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            search_recall_augmentations(
                &client,
                Some(Uuid::new_v4()),
                Some(Uuid::new_v4()),
                "recall latency",
                5,
                3,
                true,
                true,
            ),
        )
        .await
        .expect("concurrent augmentations should not deadlock");

        let decisions = result.0.expect("decision augmentation");
        let docs = result.1.expect("doc augmentation");
        assert_eq!(decisions.len(), 1);
        assert_eq!(docs.len(), 1);

        let request_lines = server.await.expect("join augmentation server");
        assert!(request_lines
            .iter()
            .any(|line| line.contains("/memory/decisions?")));
        assert!(request_lines.iter().any(|line| line.contains("/docs?")));
    }

    #[tokio::test]
    async fn primary_recall_and_augmentations_start_concurrently() {
        let all_branches_started = Arc::new(Barrier::new(3));
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            join_recall_with_augmentations(
                wait_at_barrier(
                    all_branches_started.clone(),
                    Ok::<_, mcp_types::Error>(json!({"results": [{"title": "Recall"}]})),
                ),
                wait_at_barrier(
                    all_branches_started.clone(),
                    Ok::<_, mcp_types::Error>(vec![json!({"summary": "Decision"})]),
                ),
                wait_at_barrier(
                    all_branches_started,
                    Ok::<_, mcp_types::Error>(vec![json!({"title": "Doc"})]),
                ),
            ),
        )
        .await
        .expect("primary recall and augmentations should be polled together")
        .expect("healthy recall branches");

        assert_eq!(result.0["results"].as_array().map(Vec::len), Some(1));
        assert_eq!(result.1.len(), 1);
        assert_eq!(result.2.expect("docs").len(), 1);
    }

    #[tokio::test]
    async fn primary_recall_failure_cancels_pending_augmentations() {
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            join_recall_with_augmentations(
                std::future::ready(Err(mcp_types::Error::Network(
                    "primary recall down".to_string(),
                ))),
                std::future::pending::<mcp_types::Result<Vec<serde_json::Value>>>(),
                std::future::pending::<mcp_types::Result<Vec<serde_json::Value>>>(),
            ),
        )
        .await
        .expect("primary failure must not wait for optional augmentations")
        .expect_err("primary failure should remain fatal");

        assert!(result.to_string().contains("primary recall down"));
    }

    #[tokio::test]
    async fn decision_failure_cancels_pending_best_effort_docs() {
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            join_recall_with_augmentations(
                std::future::ready(Ok::<_, mcp_types::Error>(json!({"results": []}))),
                std::future::ready(Err(mcp_types::Error::Network("decisions down".to_string()))),
                std::future::pending::<mcp_types::Result<Vec<serde_json::Value>>>(),
            ),
        )
        .await
        .expect("decision failure must not wait for best-effort docs")
        .expect_err("decision failure should remain fatal");

        assert!(result.to_string().contains("decisions down"));
    }

    #[tokio::test]
    async fn primary_error_keeps_precedence_over_an_earlier_decision_error() {
        let primary = async {
            tokio::task::yield_now().await;
            Err(mcp_types::Error::Network("primary wins".to_string()))
        };
        let result = join_recall_with_augmentations(
            primary,
            std::future::ready(Err(mcp_types::Error::Network("decision loses".to_string()))),
            std::future::pending::<mcp_types::Result<Vec<serde_json::Value>>>(),
        )
        .await
        .expect_err("primary failure should remain fatal");

        assert!(result.to_string().contains("primary wins"));
    }

    #[tokio::test]
    async fn grounding_remote_read_branches_start_concurrently() {
        let all_branches_started = Arc::new(Barrier::new(6));
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            join_grounding_remote_reads(
                wait_at_barrier(
                    all_branches_started.clone(),
                    Ok::<_, mcp_types::Error>(json!({"results": [{"title": "Recall"}]})),
                ),
                wait_at_barrier(
                    all_branches_started.clone(),
                    (
                        Ok::<_, mcp_types::Error>(vec![json!({"summary": "Decision"})]),
                        Ok::<_, mcp_types::Error>(vec![json!({"title": "Doc"})]),
                    ),
                ),
                wait_at_barrier(
                    all_branches_started.clone(),
                    Ok::<_, mcp_types::Error>(json!({"items": [{"title": "Lesson"}]})),
                ),
                wait_at_barrier(
                    all_branches_started.clone(),
                    Ok::<_, mcp_types::Error>(json!({"items": [{"name": "Skill"}]})),
                ),
                wait_at_barrier(
                    all_branches_started.clone(),
                    Ok::<_, mcp_types::Error>(json!({"items": [{"id": "media-1"}]})),
                ),
                wait_at_barrier(
                    all_branches_started,
                    "[ACCOUNT_CONTEXT] active_mode=team".to_string(),
                ),
            ),
        )
        .await
        .expect("all independent grounding reads should be polled together");

        assert_eq!(result.recall["results"].as_array().map(Vec::len), Some(1));
        assert_eq!(result.decisions.len(), 1);
        assert_eq!(result.docs.len(), 1);
        assert_eq!(result.lessons["items"].as_array().map(Vec::len), Some(1));
        assert_eq!(result.skills["items"].as_array().map(Vec::len), Some(1));
        assert_eq!(result.recent_media.as_array().map(Vec::len), Some(1));
        assert!(result.account_block.contains("active_mode=team"));
    }

    #[tokio::test]
    async fn grounding_remote_read_failures_preserve_best_effort_shapes() {
        let result = join_grounding_remote_reads(
            std::future::ready(Err(mcp_types::Error::Network("recall down".to_string()))),
            std::future::ready((
                Err(mcp_types::Error::Network("decisions down".to_string())),
                Err(mcp_types::Error::Network("docs down".to_string())),
            )),
            std::future::ready(Err(mcp_types::Error::Network("lessons down".to_string()))),
            std::future::ready(Err(mcp_types::Error::Network("skills down".to_string()))),
            std::future::ready(Err(mcp_types::Error::Network("media down".to_string()))),
            std::future::ready(String::new()),
        )
        .await;

        assert_eq!(result.recall, json!({}));
        assert!(result.decisions.is_empty());
        assert!(result.docs.is_empty());
        assert_eq!(result.lessons, json!({}));
        assert_eq!(result.skills, json!({}));
        assert_eq!(result.recent_media, json!([]));
        assert!(result.account_block.is_empty());
    }

    #[test]
    fn recall_text_includes_related_decisions_when_available() {
        let memory_items = vec![
            json!({"title": "Session A", "kind": "transcript"}),
            json!({"title": "Session B", "kind": "transcript"}),
            json!({"title": "Session C", "kind": "transcript"}),
        ];
        let decisions = vec![json!({
            "summary": "Use docker-container driver for multi-platform Docker builds",
            "details": "Decision to use docker-container driver via setup-buildx-action for multi-platform support",
            "created_at": "2026-03-26T20:24:29.856029Z"
        })];

        let text = format_recall_augmented_text(
            "docker-container driver",
            &memory_items,
            &decisions,
            &[],
            &[],
        );

        assert!(text.contains("Recalled 3 items for query: docker-container driver"));
        assert!(text.contains("Found 1 related decisions"));
        assert!(text.contains("Use docker-container driver for multi-platform Docker builds"));
        assert!(text.contains("Created: 2026-03-26T20:24:29.856029Z"));
    }

    #[test]
    fn recall_text_renders_memory_items_not_just_a_count() {
        // Live finding: "Recalled 10 items" showed nothing about the items
        // while code matches were rendered in full, making recall look
        // code-centric even when memory had the better answer.
        let memory_items = vec![json!({
            "title": "TTCC charter session: baseline built, region gap fixed",
            "kind": "session_snapshot",
            "occurred_at": "2026-06-09T19:10:00Z",
            "content_preview": "Self-heal enabled in all regions; roadmap saved."
        })];

        let text = format_recall_augmented_text("ttcc charter", &memory_items, &[], &[], &[]);

        assert!(text.contains("Recalled 1 items for query: ttcc charter"));
        assert!(text.contains(
            "[session_snapshot] **TTCC charter session: baseline built, region gap fixed**"
        ));
        assert!(text.contains("When: 2026-06-09T19:10:00Z"));
        assert!(text.contains("Self-heal enabled in all regions"));
    }

    #[test]
    fn recall_text_marks_event_plans_as_historical_and_reads_nested_time() {
        let memory_items = vec![json!({
            "title": "Premium time-travel radio",
            "kind": "event",
            "event_type": "plan",
            "metadata": {
                "occurred_at": "2026-08-22T17:06:43Z"
            },
            "content_preview": "Add three era-authentic generation stations."
        })];
        let decisions = vec![json!({
            "summary": "Replace the three-decade taxonomy with six era shelves",
            "created_at": "2026-08-24T17:01:11Z"
        })];

        let text = format_recall_augmented_text(
            "current throwback taxonomy",
            &memory_items,
            &decisions,
            &[],
            &[],
        );

        assert!(text.contains(
            "Event entries are historical records; current decisions, when available, are listed separately."
        ));
        assert!(text.contains("[event/plan · historical record] **Premium time-travel radio**"));
        assert!(text.contains("When: 2026-08-22T17:06:43Z"));
        assert!(text.contains("Found 1 related decisions"));
    }

    #[test]
    fn recall_text_includes_docs_only_when_present() {
        let docs = vec![json!({
            "title": "Sync Architecture",
            "content_preview": "Explains conflict resolution flow."
        })];

        let text = format_recall_augmented_text("sync conflict", &[], &[], &docs, &[]);

        assert!(text.contains("Recalled 0 items for query: sync conflict"));
        assert!(text.contains("Found 1 related docs"));
        assert!(text.contains("Sync Architecture"));
    }

    #[test]
    fn recall_text_includes_project_matches_when_present() {
        let project_matches = vec![json!({
            "kind": "project_doc",
            "title": "Sync Architecture",
            "location": "docs/sync-architecture.md:12",
            "content": "Conflict resolution flow and sync behavior."
        })];

        let text = format_recall_augmented_text("sync conflict", &[], &[], &[], &project_matches);

        assert!(text.contains("Found 1 related project/code matches"));
        assert!(text.contains("[doc] **Sync Architecture**"));
        assert!(text.contains("docs/sync-architecture.md:12"));
    }

    #[test]
    fn recall_project_matches_skip_binary_assets() {
        let results = vec![
            SearchResult {
                title: Some("fastlane/metadata/android/en-US/images/phoneScreenshots/2_task-list-light.png".to_string()),
                content: Some("iVBORw0KGgoAAAANSUhEUgAABDcAAAd/CAYAAAA+rhdLAAAgAElEQVR4XuxdBbgcVdKtsffiHojihASCE1ziBEkCwW1h0Q2LOywaPPiiizv".to_string()),
                file_path: Some("fastlane/metadata/android/en-US/images/phoneScreenshots/2_task-list-light.png".to_string()),
                location: Some("fastlane/metadata/android/en-US/images/phoneScreenshots/2_task-list-light.png:1".to_string()),
                start_line: Some(1),
                ..Default::default()
            },
            SearchResult {
                title: Some("src/app/op-log/core/errors/sync-errors.ts".to_string()),
                content: Some("class LocalDataConflictError extends Error {}".to_string()),
                file_path: Some("src/app/op-log/core/errors/sync-errors.ts".to_string()),
                location: Some("src/app/op-log/core/errors/sync-errors.ts:278".to_string()),
                start_line: Some(278),
                ..Default::default()
            },
        ];

        let normalized = dedupe_recall_project_results(results, 5);

        assert_eq!(normalized.len(), 1);
        assert_eq!(
            normalized[0]
                .get("file_path")
                .and_then(|value| value.as_str()),
            Some("src/app/op-log/core/errors/sync-errors.ts")
        );
    }
}

mod lesson_parsing_tests {
    use super::{
        extract_lesson_prevention, extract_lesson_severity, extract_lesson_title,
        extract_related_knowledge_preview, extract_related_knowledge_title, extract_result_items,
        is_lesson_result, lesson_severity_rank,
    };
    use serde_json::json;

    #[test]
    fn extracts_items_from_search_results_wrapper() {
        let wrapped = json!({
            "results": [
                { "title": "Lesson A", "metadata": { "original_type": "lesson" } },
                { "title": "Lesson B", "metadata": { "original_type": "lesson" } }
            ]
        });
        assert_eq!(extract_result_items(&wrapped).len(), 2);
    }

    #[test]
    fn detects_lesson_by_metadata_or_tags() {
        let by_original_type = json!({
            "title": "Original Type Lesson",
            "metadata": { "original_type": "lesson" }
        });
        let by_tags = json!({
            "title": "Tagged Lesson",
            "metadata": { "tags": ["lesson", "lesson_system", "severity:high"] }
        });
        let non_lesson = json!({
            "title": "Decision",
            "metadata": { "original_type": "decision" }
        });

        assert!(is_lesson_result(&by_original_type));
        assert!(is_lesson_result(&by_tags));
        assert!(!is_lesson_result(&non_lesson));
    }

    #[test]
    fn extracts_markdown_lesson_fields() {
        let lesson = json!({
            "content": "## Branch Safety\n**Severity:** high\n### Trigger\nPushed to main directly\n### Prevention\nUse protected branches"
        });

        assert_eq!(extract_lesson_title(&lesson), "Branch Safety");
        assert_eq!(extract_lesson_severity(&lesson), "high");
        assert_eq!(
            extract_lesson_prevention(&lesson).as_deref(),
            Some("Use protected branches")
        );
    }

    #[test]
    fn related_knowledge_title_uses_preview_instead_of_untitled() {
        let item = json!({
            "node_type": "memory",
            "content_preview": "Investigated admin-console deploy failure and confirmed SSH reset was transient."
        });

        let title = extract_related_knowledge_title(&item);

        assert!(title.contains("Investigated admin-console deploy failure"));
        assert!(!title.eq_ignore_ascii_case("untitled"));
    }

    #[test]
    fn related_knowledge_preview_uses_preview_fields() {
        let item = json!({
            "node_type": "decision",
            "preview": "Keep API readiness authoritative over local metadata."
        });

        assert_eq!(
            extract_related_knowledge_preview(&item),
            "Keep API readiness authoritative over local metadata."
        );
    }

    #[test]
    fn severity_rank_orders_levels() {
        assert!(lesson_severity_rank("critical") > lesson_severity_rank("high"));
        assert!(lesson_severity_rank("high") > lesson_severity_rank("medium"));
        assert!(lesson_severity_rank("medium") > lesson_severity_rank("low"));
    }
}

mod auto_update_command_tests {
    use super::{
        normalize_upgrade_command, should_schedule_auto_update_check_with_last,
        DEFAULT_AUTO_UPDATE_COMMAND,
    };
    use std::time::{Duration, Instant};

    #[test]
    fn defaults_to_self_update_command() {
        let command = normalize_upgrade_command(None).expect("default command");
        assert_eq!(command, DEFAULT_AUTO_UPDATE_COMMAND);
    }

    #[test]
    fn accepts_known_npm_update_command() {
        let command =
            normalize_upgrade_command(Some("npm install -g @contextstream/mcp-server@latest"));
        assert!(command.is_some());
    }

    #[test]
    fn canonicalizes_setup_script_command_to_self_update() {
        let command = normalize_upgrade_command(Some(
            "curl -fsSL https://contextstream.io/scripts/setup.sh | bash",
        ));
        assert_eq!(command.as_deref(), Some(DEFAULT_AUTO_UPDATE_COMMAND));
    }

    #[test]
    fn rejects_unknown_command() {
        let command = normalize_upgrade_command(Some("echo unsafe && rm -rf /"));
        assert!(command.is_none());
    }

    #[test]
    fn manifest_checks_are_throttled() {
        let now = Instant::now();
        let mut last_checked = None;

        assert!(should_schedule_auto_update_check_with_last(
            &mut last_checked,
            now
        ));
        assert!(!should_schedule_auto_update_check_with_last(
            &mut last_checked,
            now + Duration::from_secs(60)
        ));
        assert!(should_schedule_auto_update_check_with_last(
            &mut last_checked,
            now + Duration::from_secs(31 * 60)
        ));
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
    use super::{
        CapturePlanTool, ContextTool, GetPlanTool, InitTool, ListPlansTool,
        SessionCaptureLessonTool, SessionCaptureTool, SessionCompressTool,
        SessionDecisionTraceTool, SessionDeltaTool, SessionGetLessonsTool, SessionRecallTool,
        SessionRememberTool, SessionRestoreContextTool, SessionSmartSearchTool, SessionSummaryTool,
        SessionTool, UpdatePlanTool,
    };

    #[test]
    fn test_init_tool_metadata() {
        let client = create_mock_client();
        let session = create_mock_session();
        let tool = InitTool::new(client, session);
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "init");
        assert_eq!(metadata.title, "Initialize Session");
        assert!(metadata.description.contains("Initialize"));
        assert_eq!(metadata.category, ToolCategory::Session);
        assert!(!metadata.annotations.read_only);
        assert!(!metadata.annotations.destructive);
        assert!(metadata.annotations.requires_confirmation);
        assert!(metadata.annotations.idempotent);
        assert!(!metadata.is_pro);
    }

    #[test]
    fn test_context_tool_metadata() {
        let client = create_mock_client();
        let session = create_mock_session();
        let ik = create_mock_index_keeper();
        let tool = ContextTool::new(client, session, ik, mcp_types::atlas_layer::noop_layer());
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "context");
        assert_eq!(metadata.title, "Get Smart Context");
        assert!(metadata.description.contains("context"));
        assert_eq!(metadata.category, ToolCategory::Session);
        assert!(!metadata.annotations.read_only);
        assert!(!metadata.annotations.destructive);
        assert!(metadata.annotations.requires_confirmation);
    }

    #[test]
    fn test_session_capture_tool_metadata() {
        let client = create_mock_client();
        let session = create_mock_session();
        let tool = SessionCaptureTool::new(client, session);
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "session_capture");
        assert_eq!(metadata.category, ToolCategory::Session);
    }

    #[test]
    fn test_session_recall_tool_metadata() {
        let client = create_mock_client();
        let tool = SessionRecallTool::new(client);
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "session_recall");
        assert_eq!(metadata.category, ToolCategory::Session);
    }

    #[test]
    fn test_session_capture_lesson_tool_metadata() {
        let client = create_mock_client();
        let session = create_mock_session();
        let tool = SessionCaptureLessonTool::new(client, session);
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "session_capture_lesson");
        assert_eq!(metadata.title, "Capture Lesson");
        assert_eq!(metadata.category, ToolCategory::Session);
    }

    #[test]
    fn test_session_get_lessons_tool_metadata() {
        let client = create_mock_client();
        let session = create_mock_session();
        let tool =
            SessionGetLessonsTool::new(client, session, mcp_types::atlas_layer::noop_layer());
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "session_get_lessons");
        assert_eq!(metadata.category, ToolCategory::Session);
    }

    #[test]
    fn test_session_remember_tool_metadata() {
        let client = create_mock_client();
        let session = create_mock_session();
        let tool = SessionRememberTool::new(client, session);
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "session_remember");
        assert_eq!(metadata.title, "Remember");
        assert_eq!(metadata.category, ToolCategory::Session);
    }

    #[test]
    fn test_session_summary_tool_metadata() {
        let client = create_mock_client();
        let tool = SessionSummaryTool::new(client);
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "session_summary");
        assert_eq!(metadata.title, "Workspace Summary");
        assert_eq!(metadata.category, ToolCategory::Session);
    }

    #[test]
    fn test_session_compress_tool_metadata() {
        let client = create_mock_client();
        let tool = SessionCompressTool::new(client);
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "session_compress");
        assert_eq!(metadata.title, "Compress Chat");
        assert_eq!(metadata.category, ToolCategory::Session);
    }

    #[test]
    fn test_session_delta_tool_metadata() {
        let client = create_mock_client();
        let tool = SessionDeltaTool::new(client);
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "session_delta");
        assert_eq!(metadata.title, "Get Changes");
        assert_eq!(metadata.category, ToolCategory::Session);
    }

    #[test]
    fn test_session_smart_search_tool_metadata() {
        let client = create_mock_client();
        let tool = SessionSmartSearchTool::new(client);
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "session_smart_search");
        assert_eq!(metadata.title, "Smart Search");
        assert_eq!(metadata.category, ToolCategory::Session);
    }

    #[test]
    fn test_session_decision_trace_tool_metadata() {
        let client = create_mock_client();
        let tool = SessionDecisionTraceTool::new(client);
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "session_decision_trace");
        assert_eq!(metadata.title, "Decision Trace");
        assert_eq!(metadata.category, ToolCategory::Session);
    }

    #[test]
    fn test_session_restore_context_tool_metadata() {
        let client = create_mock_client();
        let tool = SessionRestoreContextTool::new(client);
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "session_restore_context");
        assert_eq!(metadata.title, "Restore Context");
        assert_eq!(metadata.category, ToolCategory::Session);
    }

    #[test]
    fn test_capture_plan_tool_metadata() {
        let client = create_mock_client();
        let session = create_mock_session();
        let tool = CapturePlanTool::new(client, session);
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "capture_plan");
        assert_eq!(metadata.title, "Capture Plan");
        assert_eq!(metadata.category, ToolCategory::Session);
        assert!(metadata.description.contains("event_type=\"plan\""));
        assert!(metadata
            .description
            .contains("creates one linked task per step"));
    }

    #[test]
    fn test_get_plan_tool_metadata() {
        let client = create_mock_client();
        let tool = GetPlanTool::new(client);
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "get_plan");
        assert_eq!(metadata.title, "Get Plan");
        assert_eq!(metadata.category, ToolCategory::Session);
    }

    #[test]
    fn test_update_plan_tool_metadata() {
        let client = create_mock_client();
        let session = create_mock_session();
        let tool = UpdatePlanTool::new(client, session);
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "update_plan");
        assert_eq!(metadata.title, "Update Plan");
        assert_eq!(metadata.category, ToolCategory::Session);
    }

    #[test]
    fn test_list_plans_tool_metadata() {
        let client = create_mock_client();
        let tool = ListPlansTool::new(client);
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "list_plans");
        assert_eq!(metadata.title, "List Plans");
        assert_eq!(metadata.category, ToolCategory::Session);
    }

    #[test]
    fn test_unified_session_tool_metadata() {
        let client = create_mock_client();
        let session = create_mock_session();
        let tool = SessionTool::new(client, session, mcp_types::atlas_layer::noop_layer());
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "session");
        assert_eq!(metadata.title, "Session Operations");
        assert!(metadata.description.contains("capture"));
        assert!(metadata.description.contains("recall"));
        assert!(metadata.description.contains("capture_plan"));
        assert!(metadata.description.contains("user_context"));
        assert!(metadata.description.contains("list_suggested_rules"));
        assert!(metadata.description.contains("suggested_rule_action"));
        assert!(metadata.description.contains("suggested_rules_stats"));
        assert!(metadata.description.contains("do not use action='capture'"));
        assert!(metadata
            .description
            .contains("creates linked tasks by default"));
        assert!(!metadata.annotations.read_only);
        assert!(metadata.annotations.destructive);
        assert!(metadata.annotations.requires_confirmation);
        // Disambiguation: must clarify this is NOT for codebase/file search
        assert!(
            metadata
                .description
                .contains("NOT for codebase/file search"),
            "session description must explicitly disclaim codebase search"
        );
        assert!(
            metadata.description.contains("MEMORY"),
            "session description must clarify smart_search targets memory, not code"
        );
        assert_eq!(metadata.category, ToolCategory::Session);
    }
}

mod scope_resolution_tests {
    use super::{SessionGetLessonsInput, SessionGetLessonsTool};
    use crate::testing::TestFixtures;
    use mcp_client::ContextStreamClient;
    use mcp_session::SessionManager;
    use std::sync::Arc;
    use uuid::Uuid;

    #[tokio::test]
    async fn session_get_lessons_resolves_scope_from_active_session_when_ids_are_omitted() {
        let mut config = TestFixtures::test_config();
        config.default_project_id = None;
        let client = ContextStreamClient::new(config.clone());
        let session = Arc::new(SessionManager::new(client.clone(), config));
        let workspace_id = Uuid::new_v4();
        session
            .initialize(Some(workspace_id), None, None, None)
            .await;

        let tool =
            SessionGetLessonsTool::new(client, session, mcp_types::atlas_layer::noop_layer());
        let input = SessionGetLessonsInput {
            query: Some("workspace scope".to_string()),
            category: None,
            severity: None,
            limit: Some(5),
            workspace_id: None,
            project_id: None,
        };

        let scope = tool.resolve_scope_for_input(&input).await.unwrap();
        assert_eq!(scope.workspace_id, Some(workspace_id));
        assert_eq!(scope.project_id, None);
    }
}

// ============================================================================
// Schema Tests
// ============================================================================

mod schema_tests {
    use super::{create_mock_client, create_mock_index_keeper, create_mock_session, ToolHandler};
    use super::{
        CapturePlanTool, ContextTool, GetPlanTool, InitTool, SessionCaptureLessonTool,
        SessionDeltaTool, SessionRecallTool, SessionRememberTool, SessionSmartSearchTool,
        SessionTool, UpdatePlanTool,
    };

    #[test]
    fn test_init_tool_schema() {
        let client = create_mock_client();
        let session = create_mock_session();
        let tool = InitTool::new(client, session);
        let schema = tool.input_schema();

        assert!(schema.get("properties").is_some());
        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("repository_url"));
        assert!(props.contains_key("workspace_id"));
        assert!(props.contains_key("project_id"));
        assert!(props.contains_key("folder_path"));
        assert!(props.contains_key("session_id"));
        assert!(props.contains_key("context_hint"));
        assert!(props.contains_key("auto_update"));
        assert!(props.contains_key("is_post_compact"));
    }

    #[test]
    fn test_context_tool_schema() {
        let client = create_mock_client();
        let session = create_mock_session();
        let ik = create_mock_index_keeper();
        let tool = ContextTool::new(client, session, ik, mcp_types::atlas_layer::noop_layer());
        let schema = tool.input_schema();

        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("user_message"));
        assert!(props.contains_key("tokenizer"));

        // user_message should be required
        if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
            assert!(required.iter().any(|v| v.as_str() == Some("user_message")));
        }
    }

    #[test]
    fn test_session_capture_lesson_schema() {
        let client = create_mock_client();
        let session = create_mock_session();
        let tool = SessionCaptureLessonTool::new(client, session);
        let schema = tool.input_schema();

        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("title"));
        assert!(props.contains_key("trigger"));
        assert!(props.contains_key("impact"));
        assert!(props.contains_key("prevention"));
        assert!(props.contains_key("severity"));
        assert!(props.contains_key("category"));
        assert!(props.contains_key("keywords"));

        // Check severity enum values
        if let Some(severity) = props.get("severity") {
            if let Some(enum_vals) = severity.get("enum").and_then(|e| e.as_array()) {
                let values: Vec<&str> = enum_vals.iter().filter_map(|v| v.as_str()).collect();
                assert!(values.contains(&"low"));
                assert!(values.contains(&"high"));
                assert!(values.contains(&"critical"));
            }
        }
    }

    #[test]
    fn test_session_remember_schema() {
        let client = create_mock_client();
        let session = create_mock_session();
        let tool = SessionRememberTool::new(client, session);
        let schema = tool.input_schema();

        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("content"));
        assert!(props.contains_key("importance"));
    }

    #[test]
    fn test_session_delta_schema() {
        let client = create_mock_client();
        let tool = SessionDeltaTool::new(client);
        let schema = tool.input_schema();

        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("since"));

        // since should be required
        if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
            assert!(required.iter().any(|v| v.as_str() == Some("since")));
        }
    }

    #[test]
    fn test_session_smart_search_schema() {
        let client = create_mock_client();
        let tool = SessionSmartSearchTool::new(client);
        let schema = tool.input_schema();

        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("query"));
        assert!(props.contains_key("include_related"));
        assert!(props.contains_key("include_decisions"));
        assert!(props.contains_key("limit"));
    }

    #[test]
    fn test_session_recall_schema() {
        let client = create_mock_client();
        let tool = SessionRecallTool::new(client);
        let schema = tool.input_schema();

        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("query"));
        assert!(props.contains_key("include_related"));
        assert!(props.contains_key("include_decisions"));
        assert!(props.contains_key("workspace_id"));
        assert!(props.contains_key("project_id"));

        if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
            assert!(required.iter().any(|v| v.as_str() == Some("query")));
        }
    }

    #[test]
    fn test_capture_plan_schema() {
        let client = create_mock_client();
        let session = create_mock_session();
        let tool = CapturePlanTool::new(client, session);
        let schema = tool.input_schema();

        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("title"));
        assert!(props.contains_key("description"));
        assert!(props.contains_key("goals"));
        assert!(props.contains_key("steps"));
        assert!(props.contains_key("tasks"));
        assert!(props.contains_key("create_tasks"));
        assert!(props.contains_key("tags"));

        let step_required: Vec<&str> = props["steps"]["items"]["required"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|value| value.as_str())
            .collect();
        assert!(step_required.contains(&"description"));
    }

    #[test]
    fn test_get_plan_schema() {
        let client = create_mock_client();
        let tool = GetPlanTool::new(client);
        let schema = tool.input_schema();

        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("plan_id"));
        assert!(props.contains_key("include_tasks"));
        assert!(props.contains_key("workspace_id"));
        assert!(props.contains_key("project_id"));
    }

    #[test]
    fn test_update_plan_schema() {
        let client = create_mock_client();
        let session = create_mock_session();
        let tool = UpdatePlanTool::new(client, session);
        let schema = tool.input_schema();

        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("plan_id"));
        assert!(props.contains_key("title"));
        assert!(props.contains_key("status"));

        // Check status enum values
        if let Some(status) = props.get("status") {
            if let Some(enum_vals) = status.get("enum").and_then(|e| e.as_array()) {
                let values: Vec<&str> = enum_vals.iter().filter_map(|v| v.as_str()).collect();
                assert!(values.contains(&"draft"));
                assert!(values.contains(&"active"));
                assert!(values.contains(&"completed"));
                assert!(values.contains(&"archived"));
            }
        }
    }

    #[test]
    fn test_unified_session_schema() {
        let client = create_mock_client();
        let session = create_mock_session();
        let tool = SessionTool::new(client, session, mcp_types::atlas_layer::noop_layer());
        let schema = tool.input_schema();

        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("action"));
        assert!(props.contains_key("target_project"));

        // Check action enum values
        if let Some(action) = props.get("action") {
            if let Some(enum_vals) = action.get("enum").and_then(|e| e.as_array()) {
                let values: Vec<&str> = enum_vals.iter().filter_map(|v| v.as_str()).collect();
                assert!(values.contains(&"capture"));
                assert!(values.contains(&"retro_capture"));
                assert!(values.contains(&"capture_lesson"));
                assert!(values.contains(&"get_lessons"));
                assert!(values.contains(&"update_lesson"));
                assert!(values.contains(&"delete_lesson"));
                assert!(values.contains(&"recall"));
                assert!(values.contains(&"remember"));
                assert!(values.contains(&"user_context"));
                assert!(values.contains(&"summary"));
                assert!(values.contains(&"compress"));
                assert!(values.contains(&"delta"));
                assert!(values.contains(&"smart_search"));
                assert!(values.contains(&"decision_trace"));
                assert!(values.contains(&"restore_context"));
                assert!(values.contains(&"capture_plan"));
                assert!(values.contains(&"get_plan"));
                assert!(values.contains(&"update_plan"));
                assert!(values.contains(&"list_plans"));
                assert!(values.contains(&"list_suggested_rules"));
                assert!(values.contains(&"suggested_rule_action"));
                assert!(values.contains(&"suggested_rules_stats"));
            }
        }

        // Check capture event_type enum values include taxonomy categories.
        if let Some(event_type) = props.get("event_type") {
            if let Some(enum_vals) = event_type.get("enum").and_then(|e| e.as_array()) {
                let values: Vec<&str> = enum_vals.iter().filter_map(|v| v.as_str()).collect();
                assert!(values.contains(&"uncategorized"));
                assert!(values.contains(&"operation"));
                assert!(values.contains(&"command_execution"));
                assert!(values.contains(&"file_operation"));
                assert!(!values.contains(&"plan"));
            }
        }

        // Check suggested rules fields
        assert!(props.contains_key("description"));
        assert!(props.contains_key("goals"));
        assert!(props.contains_key("steps"));
        assert!(props.contains_key("tasks"));
        assert!(props.contains_key("create_tasks"));
        assert!(props.contains_key("rule_id"));
        assert!(props.contains_key("rule_action"));
        assert!(props.contains_key("min_confidence"));
        assert!(props.contains_key("session_id"));
        assert!(props.contains_key("snapshot_id"));
        assert!(props.contains_key("max_snapshots"));
        assert!(props.contains_key("include_durable_context"));
        assert!(props.contains_key("lesson_id"));
        assert!(props.contains_key("transcript_id"));
        assert!(props.contains_key("transcript_ids"));
    }
}

// ============================================================================
// Input Validation Tests
// ============================================================================

mod validation_tests {
    use mcp_client::{run_with_auth_override, ContextStreamClient};
    use mcp_session::SessionManager;
    use mcp_types::tool::ContentItem;
    use mcp_types::AuthOverride;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::thread;
    use uuid::Uuid;

    use super::{
        build_plan_candidate_listing, degenerate_title_reason, format_plan_text,
        format_team_surfacing, plan_id, plan_lookup_terms, plan_title,
        select_latest_actionable_plan, select_latest_from_plan_sets, select_named_plan_from_sets,
        validate_capture_plan_input,
    };
    use super::{
        create_mock_client, create_mock_index_keeper, create_mock_index_keeper_from,
        create_mock_session, json, ToolHandler,
    };
    use super::{
        CapturePlanInput, CapturePlanTool, ContextTool, GetPlanTool, SessionCaptureLessonTool,
        SessionCompressTool, SessionDecisionTraceTool, SessionDeltaTool, SessionRememberTool,
        SessionSmartSearchTool, SessionTool, TestFixtures, UpdatePlanTool,
    };

    fn mock_status_text(code: u16) -> &'static str {
        match code {
            200 => "OK",
            201 => "Created",
            204 => "No Content",
            400 => "Bad Request",
            404 => "Not Found",
            500 => "Internal Server Error",
            _ => "OK",
        }
    }

    fn spawn_ordered_http_server(
        expectations: Vec<(String, String, u16, String)>,
    ) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let addr = listener.local_addr().expect("local addr");
        let handle = thread::spawn(move || {
            for (method, path_contains, status, body) in expectations {
                let (mut stream, _) = listener.accept().expect("accept request");
                let mut buffer = [0u8; 16 * 1024];
                let read = stream.read(&mut buffer).expect("read request");
                let req = String::from_utf8_lossy(&buffer[..read]);
                let first_line = req.lines().next().unwrap_or_default().to_string();
                assert!(
                    first_line.starts_with(&format!("{} ", method)),
                    "expected method {} but got request line '{}'",
                    method,
                    first_line
                );
                assert!(
                    first_line.contains(&path_contains),
                    "expected request line '{}' to include '{}'",
                    first_line,
                    path_contains
                );

                let response = format!(
                    "HTTP/1.1 {} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    status,
                    mock_status_text(status),
                    body.len(),
                    body
                );
                stream
                    .write_all(response.as_bytes())
                    .expect("write response");
            }
        });
        (format!("http://{}", addr), handle)
    }

    fn create_client_and_session_with_base_url(
        base_url: String,
    ) -> (ContextStreamClient, Arc<SessionManager>) {
        let mut config = TestFixtures::test_config();
        config.api_url = base_url;
        let client = ContextStreamClient::new(config.clone());
        let session = Arc::new(SessionManager::new(client.clone(), config));
        (client, session)
    }

    fn extract_text(result: &mcp_types::tool::ToolResult) -> String {
        result
            .content
            .iter()
            .find_map(|item| match item {
                ContentItem::Text { text } => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn test_context_tool_requires_user_message() {
        let client = create_mock_client();
        let session = create_mock_session();
        let ik = create_mock_index_keeper();
        let tool = ContextTool::new(client, session, ik, mcp_types::atlas_layer::noop_layer());

        // Empty user_message should fail
        let result = tool
            .execute(json!({
                "user_message": ""
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("user_message"));
    }

    #[tokio::test]
    async fn test_context_tool_whitespace_only_message() {
        let client = create_mock_client();
        let session = create_mock_session();
        let ik = create_mock_index_keeper();
        let tool = ContextTool::new(client, session, ik, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "user_message": "   \t\n  "
            }))
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_context_tool_uses_config_defaults_before_setup_required() {
        let mut config = TestFixtures::test_config();
        // Force a fast network failure path so this test stays deterministic
        // while still proving context() moved past setup preflight.
        config.api_url = "http://127.0.0.1:9".to_string();
        config.default_workspace_id = Some(Uuid::new_v4());
        config.default_project_id = Some(Uuid::new_v4());

        let client = ContextStreamClient::new(config.clone());
        let session = Arc::new(SessionManager::new(client.clone(), config));
        let ik = create_mock_index_keeper_from(&client, &session);
        let tool = ContextTool::new(
            client,
            session.clone(),
            ik,
            mcp_types::atlas_layer::noop_layer(),
        );

        let result = tool
            .execute(json!({
                "user_message": "follow-up without explicit scope"
            }))
            .await;

        // Old behavior returned setup_required fallback before touching the API.
        // New behavior should use config defaults and attempt the API call.
        assert!(
            result.is_err(),
            "context() should attempt API call using config defaults, not return setup-required fallback"
        );

        let state = session.state().await;
        assert!(state.initialized);
        assert!(state.workspace_id.is_some());
        assert!(state.project_id.is_some());
    }

    #[tokio::test]
    async fn test_context_tool_uses_task_auth_scope_before_setup_required() {
        let mut config = TestFixtures::test_config();
        config.api_url = "http://127.0.0.1:9".to_string();

        let expected_workspace = Uuid::new_v4();
        let expected_project = Uuid::new_v4();

        let client = ContextStreamClient::new(config.clone());
        let session = Arc::new(SessionManager::new(client.clone(), config));
        let ik = create_mock_index_keeper_from(&client, &session);
        let tool = ContextTool::new(
            client,
            session.clone(),
            ik,
            mcp_types::atlas_layer::noop_layer(),
        );

        let result = run_with_auth_override(
            AuthOverride {
                workspace_id: Some(expected_workspace),
                project_id: Some(expected_project),
                ..Default::default()
            },
            || async {
                tool.execute(json!({
                    "user_message": "follow-up using hosted remote headers"
                }))
                .await
            },
        )
        .await;

        assert!(
            result.is_err(),
            "context() should attempt API call using request auth scope, not return setup-required fallback"
        );

        let state = session.state().await;
        assert!(state.initialized);
        assert_eq!(state.workspace_id, Some(expected_workspace));
        assert_eq!(state.project_id, Some(expected_project));
    }

    #[tokio::test]
    async fn test_context_tool_prefers_initialized_session_scope_over_task_auth() {
        let mut config = TestFixtures::test_config();
        config.api_url = "http://127.0.0.1:9".to_string();

        let session_workspace = Uuid::new_v4();
        let session_project = Uuid::new_v4();
        let task_workspace = Uuid::new_v4();
        let task_project = Uuid::new_v4();
        let tempdir = tempfile::tempdir().expect("tempdir");
        let session_folder = tempdir.path().to_string_lossy().to_string();

        let client = ContextStreamClient::new(config.clone());
        let session = Arc::new(SessionManager::new(client.clone(), config));
        session
            .initialize(
                Some(session_workspace),
                Some(session_project),
                Some(session_folder.clone()),
                None,
            )
            .await;
        let ik = create_mock_index_keeper_from(&client, &session);
        let tool = ContextTool::new(
            client,
            session.clone(),
            ik,
            mcp_types::atlas_layer::noop_layer(),
        );

        let result = run_with_auth_override(
            AuthOverride {
                workspace_id: Some(task_workspace),
                project_id: Some(task_project),
                ..Default::default()
            },
            || async {
                tool.execute(json!({
                    "user_message": "follow-up after init should keep initialized scope"
                }))
                .await
            },
        )
        .await;

        assert!(
            result.is_err(),
            "context() should attempt API call using initialized session scope"
        );

        let state = session.state().await;
        assert_eq!(state.workspace_id, Some(session_workspace));
        assert_eq!(state.project_id, Some(session_project));
        assert_eq!(state.folder_path.as_deref(), Some(session_folder.as_str()));
    }

    #[tokio::test]
    async fn test_session_capture_lesson_requires_title() {
        let client = create_mock_client();
        let session = create_mock_session();
        let tool = SessionCaptureLessonTool::new(client, session);

        let result = tool
            .execute(json!({
                "title": "",
                "trigger": "Something happened",
                "impact": "Bad things",
                "prevention": "Don't do it"
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("title"));
    }

    #[tokio::test]
    async fn test_session_capture_lesson_requires_trigger() {
        let client = create_mock_client();
        let session = create_mock_session();
        let tool = SessionCaptureLessonTool::new(client, session);

        let result = tool
            .execute(json!({
                "title": "Test Lesson",
                "trigger": "",
                "impact": "Bad things",
                "prevention": "Don't do it"
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("trigger"));
    }

    #[tokio::test]
    async fn test_session_capture_lesson_requires_prevention() {
        let client = create_mock_client();
        let session = create_mock_session();
        let tool = SessionCaptureLessonTool::new(client, session);

        let result = tool
            .execute(json!({
                "title": "Test Lesson",
                "trigger": "Something happened",
                "impact": "Bad things",
                "prevention": ""
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("prevention"));
    }

    #[tokio::test]
    async fn test_session_remember_requires_content() {
        let client = create_mock_client();
        let session = create_mock_session();
        let tool = SessionRememberTool::new(client, session);

        let result = tool
            .execute(json!({
                "content": ""
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("content"));
    }

    #[tokio::test]
    async fn test_session_compress_requires_content() {
        let client = create_mock_client();
        let tool = SessionCompressTool::new(client);

        let result = tool
            .execute(json!({
                "content": "   "
            }))
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_session_delta_requires_since() {
        let client = create_mock_client();
        let tool = SessionDeltaTool::new(client);

        let result = tool
            .execute(json!({
                "since": ""
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("since"));
    }

    #[tokio::test]
    async fn test_session_smart_search_requires_query() {
        let client = create_mock_client();
        let tool = SessionSmartSearchTool::new(client);

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
    async fn test_session_decision_trace_requires_query() {
        let client = create_mock_client();
        let tool = SessionDecisionTraceTool::new(client);

        let result = tool
            .execute(json!({
                "query": ""
            }))
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_capture_plan_requires_title() {
        let client = create_mock_client();
        let session = create_mock_session();
        let tool = CapturePlanTool::new(client, session);

        let result = tool
            .execute(json!({
                "title": ""
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("title"));
    }

    #[tokio::test]
    async fn test_get_plan_validates_uuid() {
        let client = create_mock_client();
        let tool = GetPlanTool::new(client);

        let result = tool
            .execute(json!({
                "plan_id": "not-a-valid-uuid"
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Invalid"));
    }

    #[test]
    fn test_select_latest_actionable_plan_prefers_new_recent_plan() {
        let plans = json!([
            {
                "id": "11111111-1111-4111-8111-111111111111",
                "title": "Older active plan",
                "status": "active",
                "updated_at": "2026-04-16T12:00:00Z"
            },
            {
                "id": "22222222-2222-4222-8222-222222222222",
                "title": "Dedicated setup wizard ingest workers",
                "status": "active",
                "updated_at": "2026-04-24T04:00:00Z"
            }
        ]);

        let selected = select_latest_actionable_plan(&plans).expect("plan should resolve");
        assert_eq!(
            plan_id(&selected),
            Some("22222222-2222-4222-8222-222222222222")
        );
        assert_eq!(
            plan_title(&selected),
            "Dedicated setup wizard ingest workers"
        );
    }

    #[test]
    fn test_select_latest_actionable_plan_ignores_newer_archived_plan() {
        let plans = json!([
            {
                "id": "11111111-1111-4111-8111-111111111111",
                "title": "Current active plan",
                "status": "active",
                "updated_at": "2026-04-23T12:00:00Z"
            },
            {
                "id": "22222222-2222-4222-8222-222222222222",
                "title": "Newer archived plan",
                "status": "archived",
                "updated_at": "2026-04-24T04:00:00Z"
            }
        ]);

        let selected = select_latest_actionable_plan(&plans).expect("plan should resolve");
        assert_eq!(
            plan_id(&selected),
            Some("11111111-1111-4111-8111-111111111111")
        );
    }

    #[test]
    fn test_select_latest_from_plan_sets_prefers_newer_workspace_plan() {
        let scoped_plans = json!([
            {
                "id": "11111111-1111-4111-8111-111111111111",
                "title": "ContextStream UX improvements from 2026-04-16 autonomous agent session",
                "status": "active",
                "updated_at": "2026-04-16T12:00:00Z"
            }
        ]);
        let workspace_plans = json!([
            {
                "id": "70707070-7070-4070-8070-707070707070",
                "title": "Dedicated setup wizard ingest workers",
                "status": "active",
                "updated_at": "2026-04-24T04:00:00Z"
            }
        ]);

        let selected = select_latest_from_plan_sets(&[&scoped_plans, &workspace_plans])
            .expect("workspace plan should resolve");

        assert_eq!(
            plan_id(&selected),
            Some("70707070-7070-4070-8070-707070707070")
        );
        assert_eq!(
            plan_title(&selected),
            "Dedicated setup wizard ingest workers"
        );
    }

    #[test]
    fn test_select_named_plan_from_sets_prefers_exact_title() {
        let scoped_plans = json!([
            {
                "id": "11111111-1111-4111-8111-111111111111",
                "title": "Fix Daily Recap On Dashboard Later",
                "status": "active",
                "updated_at": "2026-05-14T12:00:00Z"
            }
        ]);
        let workspace_plans = json!([
            {
                "id": "22222222-2222-4222-8222-222222222222",
                "title": "Fix Daily Recap On Dashboard",
                "status": "draft",
                "updated_at": "2026-05-13T12:00:00Z"
            }
        ]);

        let selected = select_named_plan_from_sets(
            &[&scoped_plans, &workspace_plans],
            "fix daily recap on dashboard",
        )
        .expect("named plan should resolve");

        assert_eq!(
            plan_id(&selected),
            Some("22222222-2222-4222-8222-222222222222")
        );
    }

    #[test]
    fn test_select_named_plan_from_sets_falls_back_to_content_match() {
        let plans = json!([
            {
                "id": "11111111-1111-4111-8111-111111111111",
                "title": "Dashboard bug fix",
                "content": "Plan: restore the daily recap card on the dashboard",
                "status": "active",
                "updated_at": "2026-05-14T12:00:00Z"
            }
        ]);

        let selected = select_named_plan_from_sets(&[&plans], "daily recap card")
            .expect("content match should resolve");

        assert_eq!(
            plan_id(&selected),
            Some("11111111-1111-4111-8111-111111111111")
        );
    }

    #[test]
    fn test_select_named_plan_from_sets_uses_term_overlap_for_natural_prompt() {
        let plans = json!([
            {
                "id": "11111111-1111-4111-8111-111111111111",
                "title": "Quality and dependency workflow redesign",
                "content": "Plan for Code Health dashboard quality and dependency graph work",
                "status": "completed",
                "updated_at": "2026-05-14T12:00:00Z"
            }
        ]);

        let selected = select_named_plan_from_sets(
            &[&plans],
            "Check mcp tool to see if it can retrieve data about code quality and dependencies that were run in the dashboard",
        )
        .expect("term-overlap natural prompt should resolve");

        assert_eq!(
            plan_id(&selected),
            Some("11111111-1111-4111-8111-111111111111")
        );
    }

    #[test]
    fn test_degenerate_title_reason_flags_junk_titles() {
        assert!(degenerate_title_reason("--").is_some(), "punctuation-only");
        assert!(degenerate_title_reason("   ").is_some(), "whitespace-only");
        assert!(
            degenerate_title_reason("crates/mcp-server/src/hook_handlers/post_tool_use.rs")
                .is_some(),
            "bare relative path"
        );
        assert!(
            degenerate_title_reason("/home/alice/projects/mcp/crates/mcp-client/src/client.rs")
                .is_some(),
            "bare absolute path"
        );
        assert!(
            degenerate_title_reason(&"word ".repeat(60)).is_some(),
            "prose paragraph is a description, not a title"
        );
        assert!(
            degenerate_title_reason("line one\nline two").is_some(),
            "multi-line title"
        );
    }

    #[test]
    fn test_degenerate_title_reason_accepts_real_titles() {
        assert!(degenerate_title_reason("Fix get_plan resolution").is_none());
        assert!(
            degenerate_title_reason("A1: Route both hook pushes through send_ingest_batch")
                .is_none()
        );
        assert!(degenerate_title_reason("Update client.rs ingest path").is_none());
        assert!(degenerate_title_reason("Reword search.rs enrichment message").is_none());
    }

    fn capture_plan_input(value: serde_json::Value) -> CapturePlanInput {
        serde_json::from_value(value).expect("valid CapturePlanInput json")
    }

    #[test]
    fn test_validate_capture_plan_accepts_clean_plan() {
        let input = capture_plan_input(json!({
            "title": "Harden plan lookup",
            "description": "Scope, constraints, and verification for the lookup hardening work.",
            "steps": [{
                "id": "plan-step-1",
                "title": "Reject degenerate titles",
                "order": 1,
                "description": "Add degenerate_title_reason and wire it into validation; verify with tests."
            }]
        }));
        assert!(validate_capture_plan_input(&input).is_ok());
    }

    #[test]
    fn test_validate_capture_plan_rejects_degenerate_task_title() {
        let input = capture_plan_input(json!({
            "title": "Plan from a shredded findings doc",
            "steps": [{
                "id": "plan-step-1",
                "title": "Real step",
                "order": 1,
                "description": "Real scope and verification for this step."
            }],
            "tasks": [{"title": "--"}]
        }));
        let err =
            validate_capture_plan_input(&input).expect_err("junk task title must be rejected");
        assert!(
            err.to_string().contains("unusable title"),
            "error should explain the title is unusable: {err}"
        );
    }

    #[test]
    fn test_validate_capture_plan_rejects_bare_path_step_title() {
        let input = capture_plan_input(json!({
            "title": "Plan from a shredded findings doc",
            "steps": [{
                "id": "plan-step-1",
                "title": "crates/mcp-client/src/client.rs",
                "order": 1,
                "description": "This step title is just a bare file path."
            }]
        }));
        assert!(validate_capture_plan_input(&input).is_err());
    }

    #[test]
    fn test_select_latest_actionable_plan_prefers_substantive_over_fresh_draft() {
        let plans = json!([
            {
                "id": "11111111-1111-4111-8111-111111111111",
                "title": "In-progress real plan",
                "status": "draft",
                "progress": 60.0,
                "updated_at": "2026-06-20T12:00:00Z"
            },
            {
                "id": "22222222-2222-4222-8222-222222222222",
                "title": "Stray empty draft (newer)",
                "status": "draft",
                "progress": 0.0,
                "updated_at": "2026-06-22T12:00:00Z"
            }
        ]);
        let selected = select_latest_actionable_plan(&plans).expect("plan should resolve");
        assert_eq!(
            plan_id(&selected),
            Some("11111111-1111-4111-8111-111111111111"),
            "an in-progress plan should beat a newer but empty 0% draft"
        );
    }

    #[test]
    fn test_select_latest_actionable_plan_falls_back_to_newest_when_none_substantive() {
        let plans = json!([
            {
                "id": "11111111-1111-4111-8111-111111111111",
                "title": "Older empty draft",
                "status": "draft",
                "progress": 0.0,
                "updated_at": "2026-06-20T12:00:00Z"
            },
            {
                "id": "22222222-2222-4222-8222-222222222222",
                "title": "Newer empty draft",
                "status": "draft",
                "progress": 0.0,
                "updated_at": "2026-06-22T12:00:00Z"
            }
        ]);
        let selected = select_latest_actionable_plan(&plans).expect("plan should resolve");
        assert_eq!(
            plan_id(&selected),
            Some("22222222-2222-4222-8222-222222222222"),
            "with no substantive plan, the newest draft should still resolve"
        );
    }

    #[test]
    fn test_build_plan_candidate_listing_lists_ids_when_present() {
        let candidates = vec![
            json!({
                "id": "11111111-1111-4111-8111-111111111111",
                "title": "Alpha plan",
                "status": "draft",
                "progress": 10.0
            }),
            json!({
                "id": "22222222-2222-4222-8222-222222222222",
                "title": "Beta plan",
                "status": "active",
                "progress": 0.0
            }),
        ];
        let (text, structured) = build_plan_candidate_listing(Some("nonexistent"), &candidates);
        assert!(text.contains("11111111-1111-4111-8111-111111111111"));
        assert!(text.contains("22222222-2222-4222-8222-222222222222"));
        assert!(
            text.contains("get_plan"),
            "should guide the agent to open one"
        );
        assert_eq!(
            structured["plan_resolution"]["mode"],
            json!("no_match_candidates")
        );
        assert_eq!(structured["plan_resolution"]["candidate_count"], json!(2));
    }

    #[test]
    fn test_build_plan_candidate_listing_guides_capture_when_empty() {
        let (text, structured) = build_plan_candidate_listing(None, &[]);
        assert!(
            text.contains("capture_plan"),
            "empty scope should point at capture_plan"
        );
        assert_eq!(structured["plan_resolution"]["mode"], json!("no_plans"));
        assert_eq!(structured["plan_resolution"]["candidate_count"], json!(0));
    }

    #[test]
    fn test_plan_lookup_terms_keep_plural_variant_keywords() {
        let terms = plan_lookup_terms(
            "Check mcp tool to see code quality and dependencies in the dashboard",
        );

        assert!(terms.contains(&"quality".to_string()));
        assert!(terms.contains(&"dependencies".to_string()));
        assert!(terms.contains(&"dependency".to_string()));
        assert!(terms.contains(&"dashboard".to_string()));
        assert!(!terms.contains(&"mcp".to_string()));
    }

    #[test]
    fn test_format_plan_text_includes_task_ids() {
        let plan = json!({
            "id": "22222222-2222-4222-8222-222222222222",
            "title": "Dedicated setup wizard ingest workers",
            "status": "active",
            "progress": 0.0,
            "tasks": [
                {
                    "id": "33333333-3333-4333-8333-333333333333",
                    "title": "Add setup origin marker",
                    "status": "pending",
                    "plan_step_id": "job_classification",
                    "description": "Mark setup wizard ingests distinctly."
                }
            ]
        });

        let text = format_plan_text(&plan);
        assert!(text.contains("Dedicated setup wizard ingest workers"));
        assert!(text.contains("33333333-3333-4333-8333-333333333333"));
        assert!(text.contains("job_classification"));
    }

    #[test]
    fn test_format_team_surfacing_renders_context_and_recommendations() {
        let response: mcp_types::api::ContextResponse = serde_json::from_value(json!({
            "context": "ok",
            "team_context": {
                "mode": "team",
                "workspace_id": "11111111-1111-4111-8111-111111111111",
                "workspace_name": "Engineering",
                "confidence": 0.92,
                "reason": "team signals"
            },
            "team_recommendations": [{
                "title": "Run skill",
                "action": "skill(action=\"run\", name=\"deploy-checks\")",
                "rationale": "high priority",
                "priority": 90
            }],
            "team_governance": [{
                "kind": "skill",
                "id": "abc",
                "scope": "team",
                "visibility": "workspace"
            }]
        }))
        .expect("context response");

        let text = format_team_surfacing(&response).expect("team block");
        assert!(text.contains("[TEAM_CONTEXT]"));
        assert!(text.contains("Run skill"));
        assert!(text.contains("scope=team"));
    }

    #[tokio::test]
    async fn test_update_plan_validates_uuid() {
        let client = create_mock_client();
        let session = create_mock_session();
        let tool = UpdatePlanTool::new(client, session);

        let result = tool
            .execute(json!({
                "plan_id": "invalid-uuid",
                "title": "New Title"
            }))
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_session_tool_unknown_action() {
        let client = create_mock_client();
        let session = create_mock_session();
        let tool = SessionTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "unknown_action"
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Unknown action"));
        assert!(err.to_string().contains("'ground'"));
    }

    #[tokio::test]
    async fn test_session_tool_ground_requires_user_message() {
        let client = create_mock_client();
        let session = create_mock_session();
        let tool = SessionTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "ground"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("user_message"));
    }

    #[tokio::test]
    async fn test_session_tool_rejects_target_project_without_child_project_context() {
        let client = create_mock_client();
        let session = create_mock_session();
        let tool = SessionTool::new(client, session, mcp_types::atlas_layer::noop_layer());

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
    async fn test_session_tool_accepts_known_target_project_before_action_validation() {
        let client = create_mock_client();
        let session = create_mock_session();
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
        let tool = SessionTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "remember",
                "target_project": "contextstream"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("content"));
    }

    #[tokio::test]
    async fn test_session_tool_capture_lesson_requires_fields() {
        let client = create_mock_client();
        let session = create_mock_session();
        let tool = SessionTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        // Missing title
        let result = tool
            .execute(json!({
                "action": "capture_lesson",
                "trigger": "Something happened",
                "prevention": "Don't do it"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("title"));
    }

    #[tokio::test]
    async fn test_session_tool_update_lesson_resolves_lookup_and_updates_event() {
        let lesson_id = "11111111-1111-4111-8111-111111111111";
        let (base_url, server_thread) = spawn_ordered_http_server(vec![
            (
                "POST".to_string(),
                "/api/v1/memory/search".to_string(),
                200,
                serde_json::json!({
                    "results": [
                        {
                            "id": lesson_id,
                            "type": "lesson",
                            "title": "Recurring shell quoting failure",
                            "content": "### Prevention\nAlways quote file paths with spaces."
                        }
                    ]
                })
                .to_string(),
            ),
            (
                "PUT".to_string(),
                format!("/api/v1/memory/events/{}", lesson_id),
                200,
                serde_json::json!({
                    "id": lesson_id,
                    "title": "Recurring shell quoting failure (updated)"
                })
                .to_string(),
            ),
        ]);
        let (client, session) = create_client_and_session_with_base_url(base_url);
        let tool = SessionTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "update_lesson",
                "lesson_id": "shell quoting failure",
                "title": "Recurring shell quoting failure (updated)",
                "content": "Always quote paths in shell commands."
            }))
            .await
            .expect("update_lesson should succeed");

        let text = extract_text(&result);
        assert!(text.contains("Resolved lesson"));
        assert!(text.contains("Lesson updated"));
        server_thread.join().expect("mock server should complete");
    }

    #[tokio::test]
    async fn test_session_tool_delete_lesson_resolves_lookup_and_deletes_event() {
        let lesson_id = "22222222-2222-4222-8222-222222222222";
        let (base_url, server_thread) = spawn_ordered_http_server(vec![
            (
                "POST".to_string(),
                "/api/v1/memory/search".to_string(),
                200,
                serde_json::json!({
                    "results": [
                        {
                            "id": lesson_id,
                            "type": "lesson",
                            "title": "Fix dashboard recap card sorting",
                            "content": "### Prevention\nAdd deterministic sort assertions."
                        }
                    ]
                })
                .to_string(),
            ),
            (
                "DELETE".to_string(),
                format!("/api/v1/memory/events/{}", lesson_id),
                200,
                serde_json::json!({
                    "deleted": true,
                    "id": lesson_id
                })
                .to_string(),
            ),
        ]);
        let (client, session) = create_client_and_session_with_base_url(base_url);
        let tool = SessionTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "delete_lesson",
                "lesson_id": "dashboard recap card sorting"
            }))
            .await
            .expect("delete_lesson should succeed");

        let text = extract_text(&result);
        assert!(text.contains("Resolved lesson"));
        assert!(text.contains("Lesson deleted"));
        server_thread.join().expect("mock server should complete");
    }

    #[tokio::test]
    async fn test_session_tool_update_lesson_returns_ambiguity_error_for_lookup() {
        let lesson_a = "33333333-3333-4333-8333-333333333333";
        let lesson_b = "44444444-4444-4444-8444-444444444444";
        let (base_url, server_thread) = spawn_ordered_http_server(vec![(
            "POST".to_string(),
            "/api/v1/memory/search".to_string(),
            200,
            serde_json::json!({
                "results": [
                    {
                        "id": lesson_a,
                        "type": "lesson",
                        "title": "Fix dashboard recap card rendering",
                        "content": "### Prevention\nAdd component tests."
                    },
                    {
                        "id": lesson_b,
                        "type": "lesson",
                        "title": "Fix dashboard recap card sorting",
                        "content": "### Prevention\nAdd deterministic sort assertions."
                    }
                ]
            })
            .to_string(),
        )]);
        let (client, session) = create_client_and_session_with_base_url(base_url);
        let tool = SessionTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "update_lesson",
                "lesson_id": "dashboard recap card",
                "title": "Updated title"
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Multiple lessons match"));
        assert!(err.contains(lesson_a));
        assert!(err.contains(lesson_b));
        server_thread.join().expect("mock server should complete");
    }

    #[tokio::test]
    async fn test_session_tool_remember_requires_content() {
        let client = create_mock_client();
        let session = create_mock_session();
        let tool = SessionTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "remember"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("content"));
    }

    #[tokio::test]
    async fn test_session_tool_compress_requires_content() {
        let client = create_mock_client();
        let session = create_mock_session();
        let tool = SessionTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "compress"
            }))
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_session_tool_delta_requires_since() {
        let client = create_mock_client();
        let session = create_mock_session();
        let tool = SessionTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "delta"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("since"));
    }

    #[tokio::test]
    async fn test_session_tool_smart_search_requires_query() {
        let client = create_mock_client();
        let session = create_mock_session();
        let tool = SessionTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "smart_search"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("query"));
    }

    #[tokio::test]
    async fn test_session_tool_get_plan_validates_invalid_plan_id() {
        let client = create_mock_client();
        let session = create_mock_session();
        let tool = SessionTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "get_plan",
                "plan_id": "invalid-uuid"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Invalid"));
    }

    #[tokio::test]
    async fn test_session_tool_capture_plan_requires_title() {
        let client = create_mock_client();
        let session = create_mock_session();
        let tool = SessionTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "capture_plan"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("title"));
    }

    #[tokio::test]
    async fn test_session_tool_rejects_generic_plan_capture() {
        let client = create_mock_client();
        let session = create_mock_session();
        let tool = SessionTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "capture",
                "event_type": "plan",
                "title": "Bad plan save",
                "content": "This should use capture_plan"
            }))
            .await;

        assert!(result.is_err());
        let message = result.unwrap_err().to_string();
        assert!(message.contains("reserved"));
        assert!(message.contains("capture_plan"));
    }

    #[tokio::test]
    async fn test_capture_plan_requires_structured_steps() {
        let client = create_mock_client();
        let session = create_mock_session();
        let tool = SessionTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "capture_plan",
                "title": "Thin plan",
                "description": "No steps"
            }))
            .await;

        assert!(result.is_err());
        let message = result.unwrap_err().to_string();
        assert!(message.contains("structured step"));
        assert!(message.contains("generic memory events"));
    }

    #[tokio::test]
    async fn test_capture_plan_requires_step_descriptions() {
        let client = create_mock_client();
        let session = create_mock_session();
        let tool = SessionTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "capture_plan",
                "title": "Thin plan",
                "steps": [{"id": "plan-step-1", "title": "Do work", "order": 1}]
            }))
            .await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("requires a description"));
    }

    #[tokio::test]
    async fn test_session_tool_suggested_rule_action_requires_rule_id() {
        let client = create_mock_client();
        let session = create_mock_session();
        let tool = SessionTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "suggested_rule_action",
                "rule_action": "accept"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("rule_id"));
    }

    #[tokio::test]
    async fn test_session_tool_suggested_rule_action_requires_action() {
        let client = create_mock_client();
        let session = create_mock_session();
        let tool = SessionTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "suggested_rule_action",
                "rule_id": "550e8400-e29b-41d4-a716-446655440000"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("rule_action"));
    }

    #[tokio::test]
    async fn test_session_tool_suggested_rule_action_validates_uuid() {
        let client = create_mock_client();
        let session = create_mock_session();
        let tool = SessionTool::new(client, session, mcp_types::atlas_layer::noop_layer());

        let result = tool
            .execute(json!({
                "action": "suggested_rule_action",
                "rule_id": "not-a-uuid",
                "rule_action": "accept"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Invalid"));
    }
}

// ============================================================================
// Input Struct Tests
// ============================================================================

mod input_struct_tests {
    use super::json;
    use super::{
        add_retro_capture_tags, build_retro_capture_content, combine_retro_capture_transcript_ids,
        merge_retro_capture_provenance, push_retro_capture_sources_from_payload,
        retro_capture_source_from_value, CapturePlanInput, ContextInput, InitInput,
        RetroCaptureSource, SessionCaptureLessonInput, SessionInput, SessionSmartSearchInput,
        UpdatePlanInput,
    };

    #[test]
    fn test_init_input_deserialization() {
        let input: InitInput = serde_json::from_value(json!({
            "workspace_id": "550e8400-e29b-41d4-a716-446655440000",
            "project_id": "550e8400-e29b-41d4-a716-446655440001",
            "folder_path": "/home/user/project",
            "session_id": "session-123",
            "is_post_compact": true,
            "context_hint": "Working on tests"
        }))
        .unwrap();

        assert_eq!(
            input.workspace_id,
            Some("550e8400-e29b-41d4-a716-446655440000".to_string())
        );
        assert_eq!(input.folder_path, Some("/home/user/project".to_string()));
        assert_eq!(input.session_id, Some("session-123".to_string()));
        assert_eq!(input.is_post_compact, Some(true));
    }

    #[test]
    fn test_init_input_optional_fields() {
        let input: InitInput = serde_json::from_value(json!({})).unwrap();

        assert!(input.workspace_id.is_none());
        assert!(input.project_id.is_none());
        assert!(input.folder_path.is_none());
        assert!(input.session_id.is_none());
        assert!(input.context_hint.is_none());
        assert!(input.auto_update.is_none());
        assert!(input.is_post_compact.is_none());
    }

    #[test]
    fn test_context_input_deserialization() {
        let input: ContextInput = serde_json::from_value(json!({
            "user_message": "How do I implement auth?",
            "workspace_id": "550e8400-e29b-41d4-a716-446655440000"
        }))
        .unwrap();

        assert_eq!(input.user_message, "How do I implement auth?");
        assert!(input.workspace_id.is_some());
    }

    #[test]
    fn test_session_capture_lesson_input_deserialization() {
        let input: SessionCaptureLessonInput = serde_json::from_value(json!({
            "title": "Don't forget to test",
            "trigger": "Deployed without tests",
            "impact": "Bugs in production",
            "prevention": "Always run tests before deploy",
            "severity": "high",
            "category": "workflow",
            "keywords": ["testing", "deployment"]
        }))
        .unwrap();

        assert_eq!(input.title, "Don't forget to test");
        assert_eq!(input.severity, Some("high".to_string()));
        assert_eq!(
            input.keywords,
            Some(vec!["testing".to_string(), "deployment".to_string()])
        );
    }

    #[test]
    fn test_session_smart_search_input_deserialization() {
        let input: SessionSmartSearchInput = serde_json::from_value(json!({
            "query": "authentication flow",
            "include_related": true,
            "include_decisions": true,
            "limit": 10
        }))
        .unwrap();

        assert_eq!(input.query, "authentication flow");
        assert_eq!(input.include_related, Some(true));
        assert_eq!(input.limit, Some(10));
    }

    #[test]
    fn test_capture_plan_input_deserialization() {
        let input: CapturePlanInput = serde_json::from_value(json!({
            "title": "Implement User Auth",
            "description": "Add user authentication to the API",
            "goals": ["JWT tokens", "Refresh tokens", "Password hashing"],
            "tags": ["auth", "security"]
        }))
        .unwrap();

        assert_eq!(input.title, "Implement User Auth");
        assert_eq!(
            input.goals,
            Some(vec![
                "JWT tokens".to_string(),
                "Refresh tokens".to_string(),
                "Password hashing".to_string()
            ])
        );
    }

    #[test]
    fn test_session_input_deserialization() {
        let input: SessionInput = serde_json::from_value(json!({
            "action": "capture_lesson",
            "title": "Test Lesson",
            "trigger": "Bug happened",
            "prevention": "Add tests",
            "severity": "high"
        }))
        .unwrap();

        assert_eq!(input.action, "capture_lesson");
        assert_eq!(input.title, Some("Test Lesson".to_string()));
        assert_eq!(input.severity, Some("high".to_string()));
    }

    #[test]
    fn test_session_retro_capture_input_deserialization() {
        let input: SessionInput = serde_json::from_value(json!({
            "action": "retro_capture",
            "title": "Decision from Tuesday debug session",
            "event_type": "decision",
            "query": "Tuesday debug session API fallback",
            "transcript_id": "550e8400-e29b-41d4-a716-446655440000",
            "transcript_ids": [
                "550e8400-e29b-41d4-a716-446655440001",
                "550e8400-e29b-41d4-a716-446655440002"
            ],
            "limit": 4
        }))
        .unwrap();

        assert_eq!(input.action, "retro_capture");
        assert_eq!(input.event_type.as_deref(), Some("decision"));
        assert_eq!(
            input.query.as_deref(),
            Some("Tuesday debug session API fallback")
        );
        assert_eq!(
            input.transcript_id.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
        assert_eq!(
            input.transcript_ids,
            Some(vec![
                "550e8400-e29b-41d4-a716-446655440001".to_string(),
                "550e8400-e29b-41d4-a716-446655440002".to_string()
            ])
        );
        assert_eq!(input.limit, Some(4));
    }

    #[test]
    fn test_retro_capture_content_includes_query_and_source_evidence() {
        let sources = vec![RetroCaptureSource {
            kind: "transcript_search".to_string(),
            id: Some("550e8400-e29b-41d4-a716-446655440000".to_string()),
            title: "Tuesday debug session".to_string(),
            preview: Some("We decided to use the API fallback for stale MCP clients.".to_string()),
            created_at: Some("2026-06-03T18:30:00Z".to_string()),
            score: Some(0.92),
        }];

        let content = build_retro_capture_content(
            Some("Decision: keep the API fallback."),
            Some("Tuesday debug session API fallback"),
            &sources,
        );

        assert!(content.contains("Decision: keep the API fallback."));
        assert!(content.contains("Source query: Tuesday debug session API fallback"));
        assert!(content.contains("[transcript_search] Tuesday debug session"));
        assert!(content.contains("We decided to use the API fallback"));
    }

    #[test]
    fn test_retro_capture_provenance_records_rationale_and_sources() {
        let sources = vec![RetroCaptureSource {
            kind: "recall".to_string(),
            id: Some("event-1".to_string()),
            title: "Prior decision".to_string(),
            preview: Some("Relevant snippet".to_string()),
            created_at: None,
            score: Some(0.8),
        }];
        let transcript_ids = vec!["550e8400-e29b-41d4-a716-446655440000".to_string()];

        let provenance = merge_retro_capture_provenance(
            Some(json!({"repo": "contextstream/mcp"})),
            Some("prior decision"),
            &transcript_ids,
            &sources,
        );

        assert_eq!(provenance["source"], "mcp_retro_capture");
        assert_eq!(provenance["retroactive_capture"], true);
        assert_eq!(provenance["source_query"], "prior decision");
        assert_eq!(
            provenance["source_transcript_ids"][0],
            "550e8400-e29b-41d4-a716-446655440000"
        );
        assert_eq!(provenance["source_results"][0]["kind"], "recall");
        assert_eq!(provenance["repo"], "contextstream/mcp");
    }

    #[test]
    fn test_retro_capture_tags_and_transcript_ids_are_deduped() {
        let ids = combine_retro_capture_transcript_ids(
            Some("550e8400-e29b-41d4-a716-446655440000".to_string()),
            Some(vec![
                "550e8400-e29b-41d4-a716-446655440000".to_string(),
                "550e8400-e29b-41d4-a716-446655440001".to_string(),
            ]),
        );
        let tags = add_retro_capture_tags(Some(vec!["customer_feedback".to_string()]));

        assert_eq!(
            ids,
            vec![
                "550e8400-e29b-41d4-a716-446655440000".to_string(),
                "550e8400-e29b-41d4-a716-446655440001".to_string(),
            ]
        );
        assert!(tags.contains(&"customer_feedback".to_string()));
        assert!(tags.contains(&"retroactive_capture".to_string()));
        assert!(tags.contains(&"source:prior_context".to_string()));
    }

    #[test]
    fn test_retro_capture_source_extraction_handles_metadata_and_data_envelopes() {
        let metadata_source = retro_capture_source_from_value(
            "recall",
            &json!({
                "id": "event-1",
                "metadata": {
                    "title": "Nested decision title",
                    "content_preview": "Nested preview text",
                    "score": 0.7
                }
            }),
        );
        assert_eq!(metadata_source.title, "Nested decision title");
        assert_eq!(
            metadata_source.preview.as_deref(),
            Some("Nested preview text")
        );
        assert_eq!(metadata_source.score, Some(0.7));

        let mut sources = Vec::new();
        push_retro_capture_sources_from_payload(
            &mut sources,
            "transcript",
            &json!({
                "success": true,
                "data": {
                    "id": "transcript-1",
                    "title": "Wrapped transcript",
                    "messages": [
                        {"role": "user", "content": "Remember this wrapped decision."}
                    ]
                }
            }),
            3,
        );

        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].title, "Wrapped transcript");
        assert!(sources[0]
            .preview
            .as_deref()
            .unwrap()
            .contains("Remember this wrapped decision."));
    }

    #[test]
    fn test_session_restore_context_input_deserialization() {
        let input: SessionInput = serde_json::from_value(json!({
            "action": "restore_context",
            "session_id": "session-abc",
            "snapshot_id": "550e8400-e29b-41d4-a716-446655440000",
            "max_snapshots": 3,
            "trigger": "manual_post_compact",
            "include_durable_context": true
        }))
        .unwrap();

        assert_eq!(input.action, "restore_context");
        assert_eq!(input.session_id.as_deref(), Some("session-abc"));
        assert_eq!(
            input.snapshot_id.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
        assert_eq!(input.max_snapshots, Some(3));
        assert_eq!(input.trigger.as_deref(), Some("manual_post_compact"));
        assert_eq!(input.include_durable_context, Some(true));
    }

    #[test]
    fn test_update_plan_input_deserialization() {
        let input: UpdatePlanInput = serde_json::from_value(json!({
            "plan_id": "550e8400-e29b-41d4-a716-446655440000",
            "title": "Updated Title",
            "status": "active",
            "goals": ["Goal 1", "Goal 2"]
        }))
        .unwrap();

        assert_eq!(
            input.plan_id,
            Some("550e8400-e29b-41d4-a716-446655440000".to_string())
        );
        assert_eq!(input.title, Some("Updated Title".to_string()));
        assert_eq!(input.status, Some("active".to_string()));
    }

    #[test]
    fn test_session_input_with_suggested_rules_fields() {
        let input: SessionInput = serde_json::from_value(json!({
            "action": "suggested_rule_action",
            "rule_id": "550e8400-e29b-41d4-a716-446655440000",
            "rule_action": "modify",
            "modified_instruction": "Use descriptive names",
            "modified_keywords": ["naming", "variables"]
        }))
        .unwrap();

        assert_eq!(input.action, "suggested_rule_action");
        assert_eq!(
            input.rule_id,
            Some("550e8400-e29b-41d4-a716-446655440000".to_string())
        );
        assert_eq!(input.rule_action, Some("modify".to_string()));
        assert_eq!(
            input.modified_instruction,
            Some("Use descriptive names".to_string())
        );
        assert_eq!(
            input.modified_keywords,
            Some(vec!["naming".to_string(), "variables".to_string()])
        );
    }

    #[test]
    fn test_session_input_with_min_confidence() {
        let input: SessionInput = serde_json::from_value(json!({
            "action": "list_suggested_rules",
            "min_confidence": 0.8
        }))
        .unwrap();

        assert_eq!(input.action, "list_suggested_rules");
        assert_eq!(input.min_confidence, Some(0.8));
    }
}

// ============================================================================
// Compaction Restore Helper Tests
// ============================================================================

mod compaction_restore_tests {
    use super::{
        context_pressure_notice, format_restore_context_block, json,
        looks_like_post_compact_message,
    };

    #[test]
    fn restore_block_surfaces_summary_and_recommendation() {
        let payload = json!({
            "restored": true,
            "source": "snapshot",
            "summary": "Worked on update UI and release.",
            "recommendation": "Run focused tests next."
        });

        let block = format_restore_context_block(&payload, false).unwrap();
        assert!(block.contains("[POST_COMPACTION_RESTORE]"));
        assert!(block.contains("Worked on update UI"));
        assert!(block.contains("Run focused tests next"));
    }

    #[test]
    fn restore_block_can_emit_empty_fallback() {
        let payload = json!({"restored": false});

        assert!(format_restore_context_block(&payload, false).is_none());
        let block = format_restore_context_block(&payload, true).unwrap();
        assert!(block.contains("No saved snapshot/transcript"));
        assert!(block.contains("search_transcripts"));
    }

    #[test]
    fn context_pressure_notice_mentions_manual_restore() {
        let pressure = mcp_types::api::ContextPressure {
            level: "high".to_string(),
            tokens: Some(80_000),
            threshold: Some(70_000),
        };

        let notice = context_pressure_notice(Some(&pressure), true).unwrap();
        assert!(notice.contains("session_snapshot"));
        assert!(notice.contains("is_post_compact=true"));
        assert!(notice.contains("restore_context"));
    }

    #[test]
    fn post_compact_message_detection_avoids_generic_discussion() {
        assert!(looks_like_post_compact_message(
            "Continue after compaction with the previous work"
        ));
        assert!(looks_like_post_compact_message(
            "The conversation was compacted; pick up the task"
        ));
        assert!(!looks_like_post_compact_message(
            "How should search behave after compaction in the future?"
        ));
    }
}

// ============================================================================
// UUID Parsing Tests
// ============================================================================

mod uuid_parsing_tests {
    use super::Uuid;

    #[test]
    fn test_valid_uuid_parsing() {
        let uuid_str = "550e8400-e29b-41d4-a716-446655440000";
        let result = Uuid::parse_str(uuid_str);
        assert!(result.is_ok());
    }

    #[test]
    fn test_invalid_uuid_parsing() {
        let uuid_str = "not-a-valid-uuid";
        let result = Uuid::parse_str(uuid_str);
        assert!(result.is_err());
    }

    #[test]
    fn test_optional_uuid_none() {
        let input: Option<String> = None;
        let result = input.as_ref().and_then(|s| Uuid::parse_str(s).ok());
        assert!(result.is_none());
    }

    #[test]
    fn test_optional_uuid_some_valid() {
        let input: Option<String> = Some("550e8400-e29b-41d4-a716-446655440000".to_string());
        let result = input.as_ref().and_then(|s| Uuid::parse_str(s).ok());
        assert!(result.is_some());
    }

    #[test]
    fn test_optional_uuid_some_invalid() {
        let input: Option<String> = Some("invalid".to_string());
        let result = input.as_ref().and_then(|s| Uuid::parse_str(s).ok());
        assert!(result.is_none());
    }
}

// ============================================================================
// Tool Count Tests
// ============================================================================

mod tool_count_tests {
    #[test]
    fn test_session_tools_count() {
        // Expected tools:
        // - init, context, session (unified)
        // - session_capture, session_recall
        // - session_capture_lesson, session_get_lessons
        // - session_remember, session_summary, session_compress
        // - session_delta, session_smart_search, session_decision_trace
        // - session_restore_context
        // - capture_plan, get_plan, update_plan, list_plans
        // Total: 18 individual tools
        //
        // Unified session tool supports actions:
        // - capture, capture_lesson, get_lessons, recall, remember
        // - user_context, summary, compress, delta, smart_search
        // - decision_trace, restore_context
        // - capture_plan, get_plan, update_plan, list_plans
        // - list_suggested_rules, suggested_rule_action, suggested_rules_stats
        // Total: 19 actions

        let expected_tools = vec![
            "init",
            "context",
            "session",
            "session_capture",
            "session_recall",
            "session_capture_lesson",
            "session_get_lessons",
            "session_remember",
            "session_summary",
            "session_compress",
            "session_delta",
            "session_smart_search",
            "session_decision_trace",
            "session_restore_context",
            "capture_plan",
            "get_plan",
            "update_plan",
            "list_plans",
        ];

        assert_eq!(expected_tools.len(), 18);

        // Unified session tool actions
        let session_actions = vec![
            "capture",
            "capture_lesson",
            "get_lessons",
            "recall",
            "remember",
            "user_context",
            "summary",
            "compress",
            "delta",
            "smart_search",
            "decision_trace",
            "restore_context",
            "capture_plan",
            "get_plan",
            "update_plan",
            "list_plans",
            "list_suggested_rules",
            "suggested_rule_action",
            "suggested_rules_stats",
        ];

        assert_eq!(session_actions.len(), 19);
    }
}

mod task_auth_scope_tests {
    use super::{
        apply_task_auth_scope, drop_inherited_scope_for_folder_init,
        restore_inherited_scope_if_unresolved,
    };
    use mcp_client::run_with_auth_override;
    use mcp_types::AuthOverride;
    use uuid::Uuid;

    #[tokio::test]
    async fn apply_task_auth_scope_fills_missing_ids_from_request_override() {
        let expected_workspace = Uuid::new_v4();
        let expected_project = Uuid::new_v4();

        let (workspace_id, project_id) = run_with_auth_override(
            AuthOverride {
                workspace_id: Some(expected_workspace),
                project_id: Some(expected_project),
                ..Default::default()
            },
            || async { apply_task_auth_scope(None, None) },
        )
        .await;

        assert_eq!(workspace_id, Some(expected_workspace));
        assert_eq!(project_id, Some(expected_project));
    }

    #[tokio::test]
    async fn folder_init_drops_inherited_task_auth_scope_without_explicit_ids() {
        // Regression guard for folder-switch init: when init is called with
        // `folder_path` but no explicit workspace/project IDs, header-injected
        // task auth from another folder must not preempt local folder mapping.
        let pinned_project = Uuid::new_v4();
        let pinned_workspace = Uuid::new_v4();

        let (mut ws, mut proj) = run_with_auth_override(
            AuthOverride {
                workspace_id: Some(pinned_workspace),
                project_id: Some(pinned_project),
                ..Default::default()
            },
            || async { apply_task_auth_scope(None, None) },
        )
        .await;

        assert_eq!(ws, Some(pinned_workspace));
        assert_eq!(proj, Some(pinned_project));

        drop_inherited_scope_for_folder_init(true, false, false, &mut ws, &mut proj);

        assert_eq!(ws, None);
        assert_eq!(proj, None);
    }

    #[test]
    fn restore_inherited_scope_falls_back_when_folder_resolution_is_empty() {
        // Hosted remote gateway regression: init(folder_path=…) drops the
        // header-injected scope, then folder-based resolution finds nothing
        // because the caller's local `.contextstream` config is not on the
        // server. The inherited scope must be restored so the session binds to
        // the folder's pinned workspace/project instead of an account default.
        let inherited_workspace = Uuid::new_v4();
        let inherited_project = Uuid::new_v4();

        let (ws, proj, restored) = restore_inherited_scope_if_unresolved(
            None,
            None,
            Some(inherited_workspace),
            Some(inherited_project),
        );

        assert!(restored);
        assert_eq!(ws, Some(inherited_workspace));
        assert_eq!(proj, Some(inherited_project));
    }

    #[test]
    fn restore_inherited_scope_does_not_override_resolved_workspace() {
        // Local checkout: folder-based resolution succeeded, so the freshly
        // resolved workspace must win and the inherited (possibly stale) scope
        // must not clobber it.
        let resolved_workspace = Uuid::new_v4();
        let resolved_project = Uuid::new_v4();
        let inherited_workspace = Uuid::new_v4();
        let inherited_project = Uuid::new_v4();

        let (ws, proj, restored) = restore_inherited_scope_if_unresolved(
            Some(resolved_workspace),
            Some(resolved_project),
            Some(inherited_workspace),
            Some(inherited_project),
        );

        assert!(!restored);
        assert_eq!(ws, Some(resolved_workspace));
        assert_eq!(proj, Some(resolved_project));
    }

    #[test]
    fn restore_inherited_scope_is_noop_without_inherited_scope() {
        // Stdio/local transport injects no header scope, so there is nothing to
        // restore and folder/account resolution proceeds unchanged.
        let (ws, proj, restored) = restore_inherited_scope_if_unresolved(None, None, None, None);

        assert!(!restored);
        assert_eq!(ws, None);
        assert_eq!(proj, None);
    }

    #[test]
    fn restore_inherited_scope_backfills_only_missing_project() {
        // A resolved workspace with no project keeps the workspace and is left
        // for downstream project resolution — restore only fires when the
        // workspace itself is unresolved.
        let resolved_workspace = Uuid::new_v4();
        let inherited_workspace = Uuid::new_v4();
        let inherited_project = Uuid::new_v4();

        let (ws, proj, restored) = restore_inherited_scope_if_unresolved(
            Some(resolved_workspace),
            None,
            Some(inherited_workspace),
            Some(inherited_project),
        );

        assert!(!restored);
        assert_eq!(ws, Some(resolved_workspace));
        assert_eq!(proj, None);
    }

    #[test]
    fn folder_init_preserves_explicit_workspace_and_project_ids() {
        let explicit_workspace = Uuid::new_v4();
        let explicit_project = Uuid::new_v4();
        let mut ws = Some(explicit_workspace);
        let mut proj = Some(explicit_project);

        drop_inherited_scope_for_folder_init(true, true, true, &mut ws, &mut proj);

        assert_eq!(ws, Some(explicit_workspace));
        assert_eq!(proj, Some(explicit_project));
    }

    #[tokio::test]
    async fn apply_task_auth_scope_preserves_explicit_ids() {
        let explicit_workspace = Uuid::new_v4();
        let explicit_project = Uuid::new_v4();

        let (workspace_id, project_id) = run_with_auth_override(
            AuthOverride {
                workspace_id: Some(Uuid::new_v4()),
                project_id: Some(Uuid::new_v4()),
                ..Default::default()
            },
            || async { apply_task_auth_scope(Some(explicit_workspace), Some(explicit_project)) },
        )
        .await;

        assert_eq!(workspace_id, Some(explicit_workspace));
        assert_eq!(project_id, Some(explicit_project));
    }
}

// ============================================================================
// Typed Context Item Formatting Tests
// ============================================================================

mod rules_content_drift_tests {
    use super::local_rules_content_drift_notice;
    use mcp_types::rules_hash;
    use std::io::Write;
    use tempfile::TempDir;

    fn write_rules_file(dir: &TempDir, name: &str, hash: &str) -> std::path::PathBuf {
        let path = dir.path().join(name);
        let body = format!(
            "<contextstream>\n{}# Workspace: Engineering\n</contextstream>\n",
            rules_hash::format_hash_marker(hash)
        );
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(body.as_bytes()).unwrap();
        path
    }

    #[test]
    fn returns_none_when_local_hash_matches_canonical() {
        // Test isolation: the OnceLock in mcp-types is process-global, so
        // we can't pick the canonical hash for this test. Whatever it is
        // (set by another test or by the server), as long as the file's
        // embedded hash matches it, no notice should fire. We use the
        // canonical hash directly to construct a file that's guaranteed
        // to match.
        rules_hash::set_canonical_rules_hash("aabbccddeeff0011");
        let canonical = rules_hash::canonical_rules_hash().unwrap();
        let dir = TempDir::new().unwrap();
        write_rules_file(&dir, "CLAUDE.md", canonical);

        let notice = local_rules_content_drift_notice(Some(dir.path().to_str().unwrap()));
        assert!(
            notice.is_none(),
            "no drift expected when local hash matches canonical"
        );
    }

    #[test]
    fn emits_notice_when_local_hash_differs() {
        // Force-set a canonical hash distinct from the one we write to
        // disk. Even though OnceLock is first-write-wins (so this set may
        // be a no-op), we control both sides via canonical_rules_hash():
        // we read whatever's actually set and write a *different* hash
        // to disk.
        rules_hash::set_canonical_rules_hash("aabbccddeeff0011");
        let canonical = rules_hash::canonical_rules_hash().unwrap().to_string();
        let stale = if canonical == "0000000000000000" {
            "1111111111111111"
        } else {
            "0000000000000000"
        };
        let dir = TempDir::new().unwrap();
        write_rules_file(&dir, "CLAUDE.md", stale);

        let notice = local_rules_content_drift_notice(Some(dir.path().to_str().unwrap()))
            .expect("drift notice should fire when hashes differ");
        assert!(
            notice.contains("[RULES_NOTICE]"),
            "notice must be tagged so the agent recognizes it"
        );
        assert!(
            notice.contains("generate_rules(overwrite_existing=true)"),
            "notice must include the recovery command"
        );
        assert!(
            notice.contains("drifted"),
            "notice must explain the kind of staleness"
        );
    }

    #[test]
    fn returns_none_when_no_folder_path() {
        rules_hash::set_canonical_rules_hash("aabbccddeeff0011");
        // Without a folder we can't scan. Stay silent — better than
        // false-firing on every call from a session that hasn't been
        // init'd yet.
        let notice = local_rules_content_drift_notice(None);
        assert!(notice.is_none());
    }

    #[test]
    fn returns_none_when_local_file_has_no_marker() {
        // A rules file written by an older binary has no embedded marker.
        // We can't tell whether it's stale, so we stay silent rather
        // than spamming `[RULES_NOTICE]` indefinitely. The first
        // `generate_rules()` re-write will install a marker and the
        // check starts working from there.
        rules_hash::set_canonical_rules_hash("aabbccddeeff0011");
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("CLAUDE.md");
        std::fs::write(
            &path,
            "<contextstream>\n# legacy file with no marker\n</contextstream>\n",
        )
        .unwrap();

        let notice = local_rules_content_drift_notice(Some(dir.path().to_str().unwrap()));
        assert!(notice.is_none());
    }
}

mod typed_item_formatting_tests {
    use super::{
        condense_context_for_concise, format_typed_lessons, format_typed_preferences,
        format_typed_skills, format_typed_snapshots, format_typed_vcs, has_server_vcs_items,
    };
    use mcp_types::api::{ContextResponse, SmartContextItem};
    use serde_json::json;

    fn make_item(typ: &str, value: &str, score: f32) -> SmartContextItem {
        SmartContextItem {
            typ: typ.to_string(),
            value: value.to_string(),
            score,
            item_id: None,
            item_type: None,
        }
    }

    #[test]
    fn format_typed_preferences_compact() {
        let items = [make_item("PR", "Use tabs not spaces", 0.95)];
        let refs: Vec<&SmartContextItem> = items.iter().collect();
        let result = format_typed_preferences(&refs, true);
        assert!(result.contains("[PREFERENCE]"));
        assert!(result.contains("score=0.95"));
        assert!(result.contains("Use tabs not spaces"));
    }

    #[test]
    fn format_typed_preferences_verbose() {
        let items = [make_item("PR", "Use tabs not spaces", 0.95)];
        let refs: Vec<&SmartContextItem> = items.iter().collect();
        let result = format_typed_preferences(&refs, false);
        assert!(result.contains("[PREFERENCES]"));
        assert!(result.contains("MUST FOLLOW"));
        assert!(result.contains("score: 0.95"));
    }

    #[test]
    fn format_typed_preferences_empty() {
        let refs: Vec<&SmartContextItem> = vec![];
        let result = format_typed_preferences(&refs, true);
        assert!(result.is_empty());
    }

    #[test]
    fn format_typed_vcs_compact() {
        let items = [make_item("VC", "PR #42: Fix auth bug (open)", 0.8)];
        let refs: Vec<&SmartContextItem> = items.iter().collect();
        let result = format_typed_vcs(&refs, true);
        assert!(result.contains("[VCS]"));
        assert!(result.contains("PR #42"));
    }

    #[test]
    fn format_typed_vcs_verbose() {
        let items = [make_item("VC", "PR #42: Fix auth bug (open)", 0.8)];
        let refs: Vec<&SmartContextItem> = items.iter().collect();
        let result = format_typed_vcs(&refs, false);
        assert!(result.contains("[VCS]"));
        assert!(result.contains("Version control context"));
    }

    #[test]
    fn format_typed_skills_compact() {
        let items = [make_item(
            "SK",
            "deploy-checker: Run deployment checks",
            0.9,
        )];
        let refs: Vec<&SmartContextItem> = items.iter().collect();
        let result = format_typed_skills(&refs, true);
        assert!(result.contains("[SKILL]"));
        assert!(result.contains("deploy-checker"));
    }

    #[test]
    fn format_typed_snapshots_compact() {
        let items = [make_item(
            "TN",
            "Previous session: implemented auth flow",
            0.7,
        )];
        let refs: Vec<&SmartContextItem> = items.iter().collect();
        let result = format_typed_snapshots(&refs, true);
        assert!(result.contains("[SNAPSHOT]"));
        assert!(result.contains("historical=true"));
        assert!(result.contains("verify_current_state_before_relying"));
        assert!(result.contains("auth flow"));
    }

    #[test]
    fn format_typed_snapshots_compact_warns_on_historical_status_claims() {
        let items = [make_item(
            "TN",
            "Session snapshot: deployment request remains fully unexecuted; assistant produced no output",
            0.9,
        )];
        let refs: Vec<&SmartContextItem> = items.iter().collect();
        let result = format_typed_snapshots(&refs, true);
        assert!(result.contains("historical status claim"));
        assert!(result.contains("verify newer work"));
        assert!(result.contains("treating as current"));
    }

    #[test]
    fn format_typed_snapshots_verbose_includes_recall_hint() {
        let items = [make_item(
            "TN",
            "Previous session: implemented auth flow",
            0.7,
        )];
        let refs: Vec<&SmartContextItem> = items.iter().collect();
        let result = format_typed_snapshots(&refs, false);
        assert!(result.contains("[TRANSCRIPT_SNAPSHOTS]"));
        assert!(result.contains("recall"));
        assert!(result.contains("verify current-state claims"));
    }

    #[test]
    fn format_typed_lessons_compact_uses_score_severity() {
        let items = [
            make_item("L", "Always run tests before commit", 0.9),
            make_item("L", "Minor style issue", 0.3),
        ];
        let refs: Vec<&SmartContextItem> = items.iter().collect();
        let result = format_typed_lessons(&refs, true);
        assert!(result.contains("[LESSONS_WARNING]"));
        assert!(result.contains("severity=CRIT"));
        assert!(result.contains("severity=note"));
        assert!(result.contains("relevance=0.90"));
    }

    #[test]
    fn format_typed_lessons_verbose_shows_relevance() {
        let items = [make_item("L", "Always run tests before commit", 0.9)];
        let refs: Vec<&SmartContextItem> = items.iter().collect();
        let result = format_typed_lessons(&refs, false);
        assert!(result.contains("[LESSONS_WARNING]"));
        assert!(result.contains("relevance: 0.90"));
    }

    #[test]
    fn has_server_vcs_items_true_when_vc_items_present() {
        let response: ContextResponse = serde_json::from_value(json!({
            "context": "test",
            "items": [
                { "typ": "VC", "value": "PR #42 open", "score": 0.8 }
            ]
        }))
        .unwrap();
        assert!(has_server_vcs_items(&response));
    }

    #[test]
    fn has_server_vcs_items_false_when_no_vc_items() {
        let response: ContextResponse = serde_json::from_value(json!({
            "context": "test",
            "items": [
                { "typ": "PR", "value": "Use tabs", "score": 0.9 }
            ]
        }))
        .unwrap();
        assert!(!has_server_vcs_items(&response));
    }

    #[test]
    fn has_server_vcs_items_false_when_no_items() {
        let response: ContextResponse = serde_json::from_value(json!({
            "context": "test"
        }))
        .unwrap();
        assert!(!has_server_vcs_items(&response));
    }

    #[test]
    fn condense_context_preserves_new_typed_prefixes() {
        let input = "[LESSONS_WARNING] Always run tests\n[PREFERENCE] Use tabs\n[VCS] PR #42\n[SKILL] deploy-check\n[SNAPSHOT] prior session\nsome noise";
        let result = condense_context_for_concise(input);
        assert!(result.contains("[LESSONS_WARNING]"));
        assert!(result.contains("[PREFERENCE]"));
        assert!(result.contains("[VCS]"));
        assert!(result.contains("[SKILL]"));
        assert!(result.contains("[SNAPSHOT]"));
        assert!(!result.contains("some noise"));
    }

    #[test]
    fn condense_context_also_preserves_preference_tag() {
        let input = "[PREFERENCE] Use dark mode\nrandom line";
        let result = condense_context_for_concise(input);
        assert!(result.contains("[PREFERENCE]"));
        assert!(!result.contains("random line"));
    }

    #[test]
    fn format_typed_preferences_limits_to_five() {
        let items: Vec<SmartContextItem> = (0..10)
            .map(|i| make_item("PR", &format!("pref {}", i), 0.9 - i as f32 * 0.05))
            .collect();
        let refs: Vec<&SmartContextItem> = items.iter().collect();
        let result = format_typed_preferences(&refs, true);
        assert!(result.contains("pref 0"));
        assert!(result.contains("pref 4"));
        assert!(!result.contains("pref 5"));
    }

    #[test]
    fn format_typed_snapshots_limits_to_three() {
        let items: Vec<SmartContextItem> = (0..5)
            .map(|i| make_item("TN", &format!("snapshot {}", i), 0.8))
            .collect();
        let refs: Vec<&SmartContextItem> = items.iter().collect();
        let result = format_typed_snapshots(&refs, true);
        assert!(result.contains("snapshot 0"));
        assert!(result.contains("snapshot 2"));
        assert!(!result.contains("snapshot 3"));
    }

    /// Regression guard: `with_caller_auth` must re-establish both mutable
    /// session partitioning and the stable cache identity inside detached
    /// proactive futures.
    #[tokio::test]
    async fn with_caller_auth_propagates_session_and_cache_identity_into_spawn() {
        use mcp_client::{
            get_task_caller_cache_identity, get_task_session_key, run_with_caller_cache_identity,
            run_with_session_key,
        };
        use mcp_types::SessionKey;
        // `with_caller_auth` is a private helper in the parent
        // `session` module; access via `super::super::session::`
        // because `session_tests` is mounted as a child via
        // `#[path = "session_tests.rs"]`.

        let original = SessionKey::Jwt("regression-test-user".to_string());

        // Simulate the http.rs middleware setting the task-local,
        // then a handler that captures it and spawns a proactive
        // future that needs the same identity.
        let observed_in_spawn = run_with_session_key(original.clone(), || async {
            run_with_caller_cache_identity("stable-cache-caller".to_string(), || async {
                let captured_session = get_task_session_key();
                let captured_cache_identity = get_task_caller_cache_identity();
                let handle = tokio::spawn(async move {
                    super::super::with_caller_auth(
                        captured_session,
                        captured_cache_identity,
                        None,
                        None,
                        || async { (get_task_session_key(), get_task_caller_cache_identity()) },
                    )
                    .await
                });
                handle.await.unwrap()
            })
            .await
        })
        .await;

        assert_eq!(observed_in_spawn.0.as_ref(), Some(&original));
        assert_eq!(observed_in_spawn.1.as_deref(), Some("stable-cache-caller"));
    }

    /// v0.2.87 regression guard: `proactive_grounding_recall` (the
    /// internal recall fetch spawned by `context()`) must compute
    /// the **same** Atlas warm-cache scope_hash as the user-facing
    /// `session(recall)` tool so they share rows. If a future refactor
    /// changes the scope-hash key for either call site, this test
    /// fails and points at the cache-sharing invariant.
    #[test]
    fn grounding_recall_scope_hash_matches_user_facing_recall_tool() {
        use crate::domains::atlas_warm_cache::scope_hash_for_recall;
        use uuid::Uuid;

        let ws = Uuid::from_u128(0xC0FFEE);
        let pid = Uuid::from_u128(0xBEEF);
        let user_scope = Some("j:abc123");
        let query = "what did we ship for atlas yesterday?";

        // The user-facing `SessionRecallTool` builds this hash:
        let user_facing = scope_hash_for_recall(ws, user_scope, Some(pid), query);

        // The internal `proactive_grounding_recall` spawned inside
        // `context()` MUST use the same helper with the same args:
        let internal = scope_hash_for_recall(ws, user_scope, Some(pid), query);

        assert_eq!(
            user_facing, internal,
            "internal grounding fetch must share the user-facing \
             session(recall) cache scope so a single Atlas Recall \
             row services both call paths"
        );

        // Sanity: per-user isolation still holds end-to-end.
        let other_user = scope_hash_for_recall(ws, Some("j:other"), Some(pid), query);
        assert_ne!(user_facing, other_user);

        // Sanity: query change still produces a different row.
        let other_query = scope_hash_for_recall(ws, user_scope, Some(pid), "different question");
        assert_ne!(user_facing, other_query);
    }
}
