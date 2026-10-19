//! Tests for graph domain tools.

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

// ============================================================================
// Metadata Tests
// ============================================================================

mod metadata_tests {
    use super::{create_mock_client, ToolCategory, ToolHandler};
    use super::{GraphDependenciesTool, GraphImpactTool, GraphRelatedTool, GraphTool};

    #[test]
    fn test_graph_related_tool_metadata() {
        let client = create_mock_client();
        let tool = GraphRelatedTool::new(client);
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "graph_related");
        assert_eq!(metadata.title, "Find Related Code");
        assert!(metadata.description.contains("related"));
        assert_eq!(metadata.category, ToolCategory::Graph);
        assert!(metadata.annotations.read_only);
        assert!(!metadata.annotations.destructive);
        assert!(!metadata.is_pro);
        assert_eq!(metadata.required_tier, Some("lite".to_string()));
    }

    #[test]
    fn test_graph_dependencies_tool_metadata() {
        let client = create_mock_client();
        let tool = GraphDependenciesTool::new(client);
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "graph_dependencies");
        assert_eq!(metadata.title, "Analyze Dependencies");
        assert!(metadata.description.contains("dependencies"));
        assert_eq!(metadata.category, ToolCategory::Graph);
        assert!(metadata.annotations.read_only);
        assert!(!metadata.annotations.destructive);
        assert!(!metadata.annotations.requires_confirmation);
    }

    #[test]
    fn test_graph_impact_tool_metadata() {
        let client = create_mock_client();
        let tool = GraphImpactTool::new(client);
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "graph_impact");
        assert_eq!(metadata.title, "Impact Analysis");
        assert!(metadata.description.contains("impact"));
        assert_eq!(metadata.category, ToolCategory::Graph);
    }

    #[test]
    fn test_unified_graph_tool_metadata() {
        let client = create_mock_client();
        let tool = GraphTool::new(client);
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "graph");
        assert_eq!(metadata.title, "Code Graph Analysis");
        assert!(metadata.description.contains("dependencies"));
        assert!(metadata.description.contains("impact"));
        assert!(metadata.description.contains("call_path"));
        assert!(metadata.description.contains("circular_dependencies"));
        assert!(metadata.description.contains("unused_code"));
        assert!(metadata.description.contains("complexity_metrics"));
        assert!(metadata.description.contains("quality_trends"));
        assert!(metadata.description.contains("quality_history"));
        // Disambiguation: must clarify this is NOT for content/keyword search
        assert!(
            metadata
                .description
                .contains("NOT for searching code by content"),
            "graph description must explicitly disclaim content search"
        );
        assert_eq!(metadata.category, ToolCategory::Graph);
        assert!(!metadata.annotations.read_only);
        assert!(!metadata.annotations.destructive);
        assert!(metadata.annotations.requires_confirmation);
    }
}

// ============================================================================
// Schema Tests
// ============================================================================

mod schema_tests {
    use super::{create_mock_client, ToolHandler};
    use super::{GraphDependenciesTool, GraphImpactTool, GraphRelatedTool, GraphTool};

    #[test]
    fn test_graph_related_schema() {
        let client = create_mock_client();
        let tool = GraphRelatedTool::new(client);
        let schema = tool.input_schema();

        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("node_id"));
        assert!(props.contains_key("workspace_id"));
        assert!(props.contains_key("max_depth"));
        assert!(props.contains_key("relation_types"));

        // node_id should be required
        if let Some(required) = schema.get("required").and_then(|r| r.as_array()) {
            assert!(required.iter().any(|v| v.as_str() == Some("node_id")));
        }
    }

    #[test]
    fn test_graph_dependencies_schema() {
        let client = create_mock_client();
        let tool = GraphDependenciesTool::new(client);
        let schema = tool.input_schema();

        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("target_type"));
        assert!(props.contains_key("target_id"));
        assert!(props.contains_key("max_depth"));
        assert!(props.contains_key("include_transitive"));
    }

    #[test]
    fn test_graph_impact_schema() {
        let client = create_mock_client();
        let tool = GraphImpactTool::new(client);
        let schema = tool.input_schema();

        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("target_id"));
        assert!(props.contains_key("target_type"));
        assert!(props.contains_key("element_name"));
        assert!(props.contains_key("change_type"));
    }

    #[test]
    fn test_unified_graph_schema() {
        let client = create_mock_client();
        let tool = GraphTool::new(client);
        let schema = tool.input_schema();

        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("action"));

        // Check action enum contains all expected actions
        if let Some(action) = props.get("action") {
            if let Some(enum_vals) = action.get("enum").and_then(|e| e.as_array()) {
                let values: Vec<&str> = enum_vals.iter().filter_map(|v| v.as_str()).collect();
                assert!(values.contains(&"related"));
                assert!(values.contains(&"dependencies"));
                assert!(values.contains(&"impact"));
                assert!(values.contains(&"ingest"));
                assert!(values.contains(&"outbox_status"));
                assert!(values.contains(&"outbox_canary"));
                assert!(values.contains(&"call_path"));
                assert!(values.contains(&"path"));
                assert!(values.contains(&"circular_dependencies"));
                assert!(values.contains(&"unused_code"));
                assert!(values.contains(&"complexity_metrics"));
                assert!(values.contains(&"quality_trends"));
                assert!(values.contains(&"quality_history"));
                assert!(values.contains(&"quality_freshness"));
                assert!(values.contains(&"quality_snapshot"));
                assert!(values.contains(&"contradictions"));
                assert!(values.contains(&"decisions"));
            }
        }

        // Check call_path fields
        assert!(props.contains_key("source_type"));
        assert!(props.contains_key("source_id"));
        assert!(props.contains_key("target_type"));
        assert!(props.contains_key("target_id"));

        // Check path fields
        assert!(props.contains_key("source_node_id"));
        assert!(props.contains_key("target_node_id"));

        // Check other fields
        assert!(props.contains_key("wait"));
        assert!(props.contains_key("requested_tier"));
        assert!(props.contains_key("idempotency_scope"));
        assert!(props.contains_key("node_id"));
        assert!(props.contains_key("offset"));
        assert!(props.contains_key("element_type"));
        assert!(props.contains_key("start_date"));
        assert!(props.contains_key("end_date"));
    }
}

mod usages_target_type_tests {
    use super::{normalize_graph_target_type, normalize_usages_target_type};

    #[test]
    fn infers_pascal_case_targets_as_types() {
        assert_eq!(
            normalize_usages_target_type(None, "HydrationStateService"),
            "type"
        );
    }

    #[test]
    fn normalizes_service_alias_to_type() {
        assert_eq!(
            normalize_usages_target_type(Some("service".to_string()), "HydrationStateService"),
            "type"
        );
    }

    #[test]
    fn impact_infers_pascal_case_targets_as_types() {
        assert_eq!(
            normalize_graph_target_type(None, "TaskService"),
            Some("type".to_string())
        );
    }

    #[test]
    fn impact_normalizes_service_alias_to_type() {
        assert_eq!(
            normalize_graph_target_type(Some("service".to_string()), "TaskService"),
            Some("type".to_string())
        );
    }
}

// ============================================================================
// Input Validation Tests
// ============================================================================

mod validation_tests {
    use super::{create_mock_client, json, ToolHandler};
    use super::{GraphDependenciesTool, GraphImpactTool, GraphRelatedTool, GraphTool};

    #[tokio::test]
    async fn test_graph_related_requires_node_id() {
        let client = create_mock_client();
        let tool = GraphRelatedTool::new(client);

        let result = tool
            .execute(json!({
                "node_id": ""
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("node_id") || err.to_string().contains("UUID"));
    }

    #[tokio::test]
    async fn test_graph_related_invalid_uuid() {
        let client = create_mock_client();
        let tool = GraphRelatedTool::new(client);

        let result = tool
            .execute(json!({
                "node_id": "not-a-valid-uuid"
            }))
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_graph_dependencies_requires_target_id() {
        let client = create_mock_client();
        let tool = GraphDependenciesTool::new(client);

        let result = tool
            .execute(json!({
                "target_type": "function",
                "target_id": ""
            }))
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_graph_impact_requires_target_id() {
        let client = create_mock_client();
        let tool = GraphImpactTool::new(client);

        let result = tool
            .execute(json!({
                "target_id": ""
            }))
            .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_graph_tool_unknown_action() {
        let client = create_mock_client();
        let tool = GraphTool::new(client);

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
    async fn test_graph_tool_related_requires_node_id() {
        let client = create_mock_client();
        let tool = GraphTool::new(client);

        let result = tool
            .execute(json!({
                "action": "related"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("node_id"));
    }

    #[tokio::test]
    async fn test_graph_tool_dependencies_requires_target_id() {
        let client = create_mock_client();
        let tool = GraphTool::new(client);

        let result = tool
            .execute(json!({
                "action": "dependencies"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("target_id"));
    }

    #[tokio::test]
    async fn test_graph_tool_impact_requires_target_id() {
        let client = create_mock_client();
        let tool = GraphTool::new(client);

        let result = tool
            .execute(json!({
                "action": "impact"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("target_id"));
    }

    #[tokio::test]
    async fn test_graph_tool_call_path_requires_fields() {
        let client = create_mock_client();
        let tool = GraphTool::new(client);

        // Missing source_type
        let result = tool
            .execute(json!({
                "action": "call_path",
                "source_id": "main"
            }))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("source_type"));

        // Missing source_id
        let result = tool
            .execute(json!({
                "action": "call_path",
                "source_type": "function"
            }))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("source_id"));
    }

    #[tokio::test]
    async fn test_graph_tool_path_requires_fields() {
        let client = create_mock_client();
        let tool = GraphTool::new(client);

        // Missing source_node_id
        let result = tool
            .execute(json!({
                "action": "path",
                "target_node_id": "550e8400-e29b-41d4-a716-446655440000"
            }))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("source_node_id"));

        // Missing target_node_id
        let result = tool
            .execute(json!({
                "action": "path",
                "source_node_id": "550e8400-e29b-41d4-a716-446655440000"
            }))
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("target_node_id"));
    }

    #[tokio::test]
    async fn test_graph_tool_path_validates_uuids() {
        let client = create_mock_client();
        let tool = GraphTool::new(client);

        // Invalid source_node_id
        let result = tool
            .execute(json!({
                "action": "path",
                "source_node_id": "invalid-uuid",
                "target_node_id": "550e8400-e29b-41d4-a716-446655440000"
            }))
            .await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Invalid source_node_id"));

        // Invalid target_node_id
        let result = tool
            .execute(json!({
                "action": "path",
                "source_node_id": "550e8400-e29b-41d4-a716-446655440000",
                "target_node_id": "invalid"
            }))
            .await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Invalid target_node_id"));
    }

    #[tokio::test]
    async fn test_graph_tool_deps_alias() {
        let client = create_mock_client();
        let tool = GraphTool::new(client);

        // "deps" should work as alias for "dependencies"
        let result = tool
            .execute(json!({
                "action": "deps"
            }))
            .await;

        assert!(result.is_err());
        // Should fail because target_id is missing, not because action is invalid
        assert!(result.unwrap_err().to_string().contains("target_id"));
    }

    #[tokio::test]
    async fn test_graph_tool_decisions_requires_node_id() {
        let client = create_mock_client();
        let tool = GraphTool::new(client);

        // decisions action requires node_id
        let result = tool
            .execute(json!({
                "action": "decisions"
            }))
            .await;

        // Should fail because node_id is required
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("node_id"));
    }

    #[tokio::test]
    async fn test_graph_tool_circular_dependencies_requires_project_id() {
        let client = create_mock_client();
        let tool = GraphTool::new(client);

        // circular_dependencies now requires project_id
        let result = tool
            .execute(json!({
                "action": "circular_dependencies"
            }))
            .await;

        // Should fail (either validation or network error)
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        // Should not be an unknown action error
        assert!(!err.contains("Unknown action"));
    }

    #[tokio::test]
    async fn test_graph_tool_unused_code_requires_project_id() {
        let client = create_mock_client();
        let tool = GraphTool::new(client);

        // unused_code now requires project_id
        let result = tool
            .execute(json!({
                "action": "unused_code"
            }))
            .await;

        // Should fail (either validation or network error)
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        // Should not be an unknown action error
        assert!(!err.contains("Unknown action"));
    }

    #[tokio::test]
    async fn test_graph_tool_ingest_requires_project_id() {
        let client = create_mock_client();
        let tool = GraphTool::new(client);

        // ingest now requires project_id
        let result = tool
            .execute(json!({
                "action": "ingest"
            }))
            .await;

        // Should fail (either validation or network error)
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        // Should not be an unknown action error
        assert!(!err.contains("Unknown action"));
    }

    #[tokio::test]
    async fn test_graph_tool_outbox_status_requires_project_id() {
        let client = create_mock_client();
        let tool = GraphTool::new(client);

        let result = tool
            .execute(json!({
                "action": "outbox_status"
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(!err.contains("Unknown action"));
    }

    #[tokio::test]
    async fn test_graph_tool_outbox_canary_requires_project_id() {
        let client = create_mock_client();
        let tool = GraphTool::new(client);

        let result = tool
            .execute(json!({
                "action": "outbox_canary"
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(!err.contains("Unknown action"));
    }

    #[tokio::test]
    async fn test_graph_tool_contradictions_accepts_optional_node_id() {
        let client = create_mock_client();
        let tool = GraphTool::new(client);

        // contradictions treats node_id as optional — passing None scopes
        // to workspace-level contradiction detection.
        let result = tool
            .execute(json!({
                "action": "contradictions"
            }))
            .await;

        // Should not fail on missing node_id (it's optional).
        // It may fail for other reasons (mock client, no workspace) but
        // the error should NOT be about a missing node_id.
        if let Err(err) = &result {
            let msg = err.to_string();
            assert!(
                !msg.contains("node_id is required"),
                "contradictions should accept optional node_id, got: {}",
                msg
            );
        }
    }
}

// ============================================================================
// Input Struct Tests
// ============================================================================

mod input_struct_tests {
    use super::json;
    use super::{GraphDependenciesInput, GraphImpactInput, GraphInput, GraphRelatedInput};

    #[test]
    fn test_graph_related_input_deserialization() {
        let input: GraphRelatedInput = serde_json::from_value(json!({
            "node_id": "550e8400-e29b-41d4-a716-446655440000",
            "workspace_id": "550e8400-e29b-41d4-a716-446655440001",
            "max_depth": 3,
            "relation_types": ["depends_on", "calls"]
        }))
        .unwrap();

        assert_eq!(input.node_id, "550e8400-e29b-41d4-a716-446655440000");
        assert!(input.workspace_id.is_some());
        assert_eq!(input.max_depth, Some(3));
        assert_eq!(
            input.relation_types,
            Some(vec!["depends_on".to_string(), "calls".to_string()])
        );
    }

    #[test]
    fn test_graph_dependencies_input_deserialization() {
        let input: GraphDependenciesInput = serde_json::from_value(json!({
            "target_type": "module",
            "target_id": "src/utils/helpers.ts",
            "max_depth": 2
        }))
        .unwrap();

        assert_eq!(input.target_type, "module");
        assert_eq!(input.target_id, "src/utils/helpers.ts");
        assert_eq!(input.max_depth, Some(2));
    }

    #[test]
    fn test_graph_impact_input_deserialization() {
        let input: GraphImpactInput = serde_json::from_value(json!({
            "target_id": "types/index.ts",
            "element_name": "UserType",
            "change_type": "modify_signature"
        }))
        .unwrap();

        assert_eq!(input.target_id, "types/index.ts");
        assert_eq!(input.element_name, Some("UserType".to_string()));
        assert_eq!(input.change_type, Some("modify_signature".to_string()));
    }

    #[test]
    fn test_graph_input_deserialization() {
        let input: GraphInput = serde_json::from_value(json!({
            "action": "call_path",
            "source_type": "function",
            "source_id": "handleLogin",
            "target_type": "function",
            "target_id": "validateToken"
        }))
        .unwrap();

        assert_eq!(input.action, "call_path");
        assert_eq!(input.source_type, Some("function".to_string()));
        assert_eq!(input.source_id, Some("handleLogin".to_string()));
        assert_eq!(input.target_type, Some("function".to_string()));
        assert_eq!(input.target_id, Some("validateToken".to_string()));
    }

    #[test]
    fn test_graph_input_path_deserialization() {
        let input: GraphInput = serde_json::from_value(json!({
            "action": "path",
            "source_node_id": "550e8400-e29b-41d4-a716-446655440000",
            "target_node_id": "550e8400-e29b-41d4-a716-446655440001"
        }))
        .unwrap();

        assert_eq!(input.action, "path");
        assert!(input.source_node_id.is_some());
        assert!(input.target_node_id.is_some());
    }

    #[test]
    fn test_graph_input_ingest_deserialization() {
        let input: GraphInput = serde_json::from_value(json!({
            "action": "ingest",
            "wait": true
        }))
        .unwrap();

        assert_eq!(input.action, "ingest");
        assert_eq!(input.wait, Some(true));
    }

    #[test]
    fn test_graph_input_outbox_canary_deserialization() {
        let input: GraphInput = serde_json::from_value(json!({
            "action": "outbox_canary",
            "requested_tier": "full",
            "idempotency_scope": "manual-smoke"
        }))
        .unwrap();

        assert_eq!(input.action, "outbox_canary");
        assert_eq!(input.requested_tier, Some("full".to_string()));
        assert_eq!(input.idempotency_scope, Some("manual-smoke".to_string()));
    }

    #[test]
    fn test_graph_input_contradictions_deserialization() {
        let input: GraphInput = serde_json::from_value(json!({
            "action": "contradictions",
            "node_id": "550e8400-e29b-41d4-a716-446655440000",
            "limit": 10
        }))
        .unwrap();

        assert_eq!(input.action, "contradictions");
        assert!(input.node_id.is_some());
        assert_eq!(input.limit, Some(10));
    }

    #[test]
    fn test_graph_input_quality_fields_deserialization() {
        let input: GraphInput = serde_json::from_value(json!({
            "action": "quality_trends",
            "limit": 30,
            "offset": 10,
            "element_type": "Function",
            "start_date": "2026-05-01",
            "end_date": "2026-05-14"
        }))
        .unwrap();

        assert_eq!(input.action, "quality_trends");
        assert_eq!(input.limit, Some(30));
        assert_eq!(input.offset, Some(10));
        assert_eq!(input.element_type, Some("Function".to_string()));
        assert_eq!(input.start_date, Some("2026-05-01".to_string()));
        assert_eq!(input.end_date, Some("2026-05-14".to_string()));
    }
}

// ============================================================================
// Tool Registration Tests
// ============================================================================

mod registration_tests {
    #[test]
    fn test_graph_tool_count() {
        // Expected graph tools:
        // - graph (unified)
        // - graph_related
        // - graph_dependencies
        // - graph_impact
        // Total: 4 tools

        let expected_tools = [
            "graph",
            "graph_related",
            "graph_dependencies",
            "graph_impact",
        ];

        assert_eq!(expected_tools.len(), 4);
    }

    #[test]
    fn test_graph_actions_coverage() {
        // Document all graph actions in the unified tool:
        // Query-requiring actions:
        // - related: Find related code (requires query)
        // - dependencies/deps: Module dependencies (requires query)
        // - impact: Change impact analysis (requires query)
        //
        // Call path action:
        // - call_path: Function call path (requires source_type + source_id)
        //
        // Path action:
        // - path: Path between nodes (requires source_node_id + target_node_id)
        //
        // No-required-params actions:
        // - ingest: Build/rebuild graph
        // - outbox_status: Inspect relationship storage graph outbox
        // - outbox_canary: Enqueue relationship storage graph outbox canary
        // - circular_dependencies: Find circular deps
        // - unused_code: Find unused code
        // - complexity_metrics: Retrieve dashboard complexity data
        // - quality_trends: Retrieve Code Health trend data
        // - quality_history: Retrieve saved Code Health scan history
        // - quality_freshness: Inspect dashboard-quality cache freshness
        // - quality_snapshot: Record a Code Health trend/history snapshot
        // - contradictions: Find contradictions (optional node_id)
        // - decisions: Decision history (optional query)

        let all_actions = [
            "related",
            "dependencies",
            "impact",
            "call_path",
            "path",
            "ingest",
            "outbox_status",
            "outbox_canary",
            "circular_dependencies",
            "unused_code",
            "complexity_metrics",
            "quality_trends",
            "quality_history",
            "quality_freshness",
            "quality_snapshot",
            "contradictions",
            "decisions",
        ];

        assert_eq!(all_actions.len(), 17);
    }
}

// ============================================================================
// Call Path Compatibility Tests
// ============================================================================

mod call_path_compat_tests {
    use super::{call_path_node_count, json, normalize_call_path_result};

    #[test]
    fn test_call_path_node_count_legacy_path() {
        let result = json!({
            "path": ["find_dependencies", "find_module_dependencies"]
        });

        assert_eq!(call_path_node_count(&result), 2);
    }

    #[test]
    fn test_call_path_node_count_modern_paths_functions() {
        let result = json!({
            "paths": [
                {
                    "functions": ["find_dependencies", "find_module_dependencies"],
                    "length": 1
                }
            ],
            "shortest_path_length": 1,
            "total_paths_found": 1
        });

        assert_eq!(call_path_node_count(&result), 2);
    }

    #[test]
    fn test_call_path_node_count_modern_paths_length_only() {
        let result = json!({
            "paths": [
                {
                    "length": 3
                }
            ]
        });

        assert_eq!(call_path_node_count(&result), 4);
    }

    #[test]
    fn test_normalize_call_path_result_injects_legacy_path() {
        let result = json!({
            "paths": [
                {
                    "functions": ["a", "b", "c"],
                    "length": 2
                }
            ],
            "shortest_path_length": 2,
            "total_paths_found": 1
        });

        let normalized = normalize_call_path_result(result);
        assert_eq!(normalized["path"], json!(["a", "b", "c"]));
    }

    #[test]
    fn test_normalize_call_path_result_preserves_existing_path() {
        let result = json!({
            "path": ["legacy_a", "legacy_b"],
            "paths": [
                {
                    "functions": ["modern_a", "modern_b"],
                    "length": 1
                }
            ]
        });

        let normalized = normalize_call_path_result(result);
        assert_eq!(normalized["path"], json!(["legacy_a", "legacy_b"]));
    }
}

// ============================================================================
// Call Path Text Rendering Tests
// ============================================================================

mod call_path_render_tests {
    use super::{json, render_call_paths};

    #[test]
    fn renders_http_and_event_hops_and_counts_service_crossings() {
        let result = json!({
            "paths": [
                {
                    "functions": ["Purchase", "Complete", "GrantItems", "Grant", "GrantInventory"],
                    "length": 4,
                    "hops": [
                        {"kind": "call"},
                        {"kind": "call"},
                        {"kind": "http", "via": "POST /inventory/grant"},
                        {"kind": "call"}
                    ]
                },
                {
                    "functions": ["Purchase", "Publish", "OnPurchased", "GrantInventory"],
                    "length": 3,
                    "hops": [
                        {"kind": "indirect"},
                        {"kind": "event", "via": "purchase.completed"},
                        {"kind": "call"}
                    ]
                }
            ],
            "path": ["Purchase", "Complete", "GrantItems", "Grant", "GrantInventory"],
            "shortest_path_length": 3,
            "total_paths_found": 2
        });

        let text = render_call_paths(&result, "Purchase", Some("GrantInventory"), None);
        assert_eq!(
            text,
            "1. Purchase -> Complete -> GrantItems =http POST /inventory/grant=> Grant -> GrantInventory (4 steps)\n\
             2. Purchase -(registers)-> Publish =event purchase.completed=> OnPurchased -> GrantInventory (3 steps)\n\
             Crosses services: 1 HTTP hop(s), 1 event hop(s)."
        );
    }

    #[test]
    fn joins_with_calls_when_hops_are_missing() {
        let result = json!({
            "paths": [{"functions": ["a", "b", "c"], "length": 2}],
            "path": ["a", "b", "c"],
            "shortest_path_length": 2,
            "total_paths_found": 1
        });

        let text = render_call_paths(&result, "a", Some("c"), Some(3));
        assert_eq!(text, "1. a -> b -> c (2 steps)");
    }

    #[test]
    fn joins_with_calls_when_hops_do_not_line_up() {
        let result = json!({
            "paths": [{
                "functions": ["a", "b", "c"],
                "length": 2,
                "hops": [{"kind": "http", "via": "GET /b"}]
            }]
        });

        let text = render_call_paths(&result, "a", Some("c"), None);
        assert_eq!(text, "1. a -> b -> c (2 steps)");
        assert!(!text.contains("Crosses services"));
    }

    #[test]
    fn says_no_path_was_found_with_the_request_and_default_depth() {
        let result = json!({
            "paths": [],
            "path": [],
            "shortest_path_length": 0,
            "total_paths_found": 0
        });

        assert_eq!(
            render_call_paths(&result, "Purchase", Some("GrantInventory"), None),
            "No call path found from Purchase to GrantInventory within depth 5."
        );
        assert_eq!(
            render_call_paths(&result, "Purchase", Some("GrantInventory"), Some(8)),
            "No call path found from Purchase to GrantInventory within depth 8."
        );
    }

    #[test]
    fn caps_the_listing_at_five_paths_and_counts_shared_crossings_once() {
        let paths = (0..7)
            .map(|index| {
                json!({
                    "functions": ["Checkout", format!("Step{}", index), "Charge", "Settle"],
                    "length": 3,
                    "hops": [
                        {"kind": "call"},
                        {"kind": "call"},
                        {"kind": "http", "via": "POST /payments/settle"}
                    ]
                })
            })
            .collect::<Vec<_>>();
        let result = json!({"paths": paths, "total_paths_found": 7});

        let text = render_call_paths(&result, "Checkout", Some("Settle"), None);
        let lines = text.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 7);
        assert_eq!(
            lines[0],
            "1. Checkout -> Step0 -> Charge =http POST /payments/settle=> Settle (3 steps)"
        );
        assert!(lines[4].starts_with("5. Checkout -> Step4 "));
        assert_eq!(lines[5], "...and 2 more paths in the structured result.");
        assert_eq!(lines[6], "Crosses services: 1 HTTP hop(s), 0 event hop(s).");
    }

    #[test]
    fn renders_the_legacy_path_when_paths_are_absent() {
        let result = json!({"path": ["find_dependencies", "find_module_dependencies"]});

        assert_eq!(
            render_call_paths(&result, "find_dependencies", None, None),
            "1. find_dependencies -> find_module_dependencies (1 step)"
        );
    }
}

// ============================================================================
// Knowledge that refers to code
// ============================================================================

#[test]
fn code_knowledge_text_groups_by_type_and_says_how_each_refers() {
    let result = json!({
        "target": {"id": "services/inventory/store.go::Store.GrantInventory"},
        "knowledge": [
            {"node": {"id": "d1", "node_type": "decision", "summary": "Grants are idempotent per receipt"}, "reason": "code_ref"},
            {"node": {"id": "l1", "node_type": "lesson", "summary": "Retry storms double-granted items"}, "reason": "mentions_path"},
            {"node": {"id": "d2", "node_type": "decision", "summary": "Store.GrantInventory must stay idempotent"}, "reason": "mentions_symbol"}
        ],
        "events": [{"event_type": "task", "title": "Make grants idempotent", "reason": "code_ref"}],
        "docs": [{"title": "Inventory runbook", "reason": "mentions_path"}]
    });
    assert_eq!(
        render_code_knowledge(&result),
        "Knowledge that refers to services/inventory/store.go::Store.GrantInventory:\n\
         Decisions (2):\n\
         1. Grants are idempotent per receipt (records a reference to it; d1)\n\
         2. Store.GrantInventory must stay idempotent (names it; d2)\n\
         Lessons (1):\n\
         1. Retry storms double-granted items (names its file; l1)\n\
         Tasks and notes (1):\n\
         1. [task] Make grants idempotent (records a reference to it)\n\
         Docs (1):\n\
         1. Inventory runbook (names its file)"
    );
}

#[test]
fn code_knowledge_text_says_when_nothing_refers_to_the_code() {
    let result = json!({"target": {"id": "src/x.rs"}, "knowledge": [], "events": [], "docs": []});
    assert_eq!(
        render_code_knowledge(&result),
        "No decisions, lessons, tasks or docs refer to src/x.rs yet."
    );
}

#[test]
fn related_decisions_text_lists_each_decision() {
    let result = json!([
        {"id": "d1", "summary": "Use Postgres for the graph"},
        {"id": "d2", "summary": "Keep Neo4j for parity checks"}
    ]);
    assert_eq!(
        render_related_decisions(&result),
        "Related decisions (2):\n1. Use Postgres for the graph (d1)\n2. Keep Neo4j for parity checks (d2)"
    );
    assert_eq!(
        render_related_decisions(&json!([])),
        "No decisions are related to this node."
    );
}

#[test]
fn neighborhood_text_lists_items_by_distance_with_their_links() {
    let result = json!({
        "center": {"kind": "event", "subtype": "plan", "title": "Make grants idempotent", "id": "p"},
        "neighbors": [
            {"item": {"kind": "knowledge", "subtype": "decision", "title": "Grants are idempotent per receipt", "id": "d"},
             "depth": 1, "relation": "links", "direction": "out"},
            {"item": {"kind": "event", "subtype": "task", "title": "Add the receipt key", "id": "t"},
             "depth": 1, "relation": "part_of", "direction": "in"},
            {"item": {"kind": "code", "title": "services/inventory/store.go::Store.GrantInventory", "id": "services/inventory/store.go::Store.GrantInventory"},
             "depth": 2, "relation": "about", "direction": "out"}
        ]
    });
    assert_eq!(
        render_neighborhood(&result),
        "Linked to [plan] Make grants idempotent:\n\
         Directly (2):\n\
         1. [decision] Grants are idempotent per receipt (-> links; d)\n\
         2. [task] Add the receipt key (<- part_of; t)\n\
         2 links away (1):\n\
         1. [code] services/inventory/store.go::Store.GrantInventory (-> about; services/inventory/store.go::Store.GrantInventory)"
    );
    assert_eq!(
        render_neighborhood(
            &json!({"center": {"kind": "doc", "title": "Runbook"}, "neighbors": []})
        ),
        "Nothing is linked to [doc] Runbook."
    );
}

#[test]
fn item_path_text_follows_each_link_in_the_direction_it_was_walked() {
    let result = json!({
        "items": [
            {"id": "d", "kind": "knowledge", "subtype": "decision", "title": "Grants are idempotent"},
            {"id": "p", "kind": "event", "subtype": "plan", "title": "Make grants idempotent"},
            {"id": "h", "kind": "handoff", "title": "Hand over grants"},
            {"id": "b", "kind": "ticket", "subtype": "bug", "title": "Double grants"}
        ],
        "links": [
            {"from": {"kind": "event", "id": "p"}, "to": {"kind": "knowledge", "id": "d"}, "relation": "links"},
            {"from": {"kind": "handoff", "id": "h"}, "to": {"kind": "event", "id": "p"}, "relation": "links"},
            {"from": {"kind": "handoff", "id": "h"}, "to": {"kind": "ticket", "id": "b"}, "relation": "links"}
        ]
    });
    assert_eq!(
        render_item_path(&result),
        "Path (3 links):\n\
         [decision] Grants are idempotent\n  \
         <- links [plan] Make grants idempotent\n  \
         <- links [handoff] Hand over grants\n  \
         -> links [bug] Double grants"
    );
    assert_eq!(
        render_item_path(&Value::Null),
        "No path links these items within reach."
    );
}
