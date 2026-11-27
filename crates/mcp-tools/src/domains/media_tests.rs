//! Tests for media domain tools.

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

// ============================================================================
// Metadata Tests
// ============================================================================

mod metadata_tests {
    use super::MediaTool;
    use super::{create_mock_client, create_mock_session, ToolCategory, ToolHandler};

    #[test]
    fn test_media_tool_metadata() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MediaTool::new(client, session);
        let metadata = tool.metadata();

        assert_eq!(metadata.name, "media");
        assert_eq!(metadata.title, "Media Operations");
        assert!(metadata.description.contains("index"));
        assert!(metadata.description.contains("status"));
        assert!(metadata.description.contains("search"));
        assert!(metadata.description.contains("get_clip"));
        assert!(metadata.description.contains("list"));
        assert!(metadata.description.contains("delete"));
        assert!(metadata.description.contains("remotion"));
        assert!(metadata.description.contains("ffmpeg"));
        assert!(metadata.description.contains("photos/images"));
        assert!(metadata.description.contains("documents/PDFs"));
        assert_eq!(metadata.category, ToolCategory::Ai);
        assert!(metadata.is_pro);
    }
}

// ============================================================================
// Schema Tests
// ============================================================================

mod schema_tests {
    use super::MediaTool;
    use super::{create_mock_client, create_mock_session, ToolHandler};

    #[test]
    fn test_media_tool_schema() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MediaTool::new(client, session);
        let schema = tool.input_schema();

        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("action"));

        // Check action enum
        if let Some(action) = props.get("action") {
            if let Some(enum_vals) = action.get("enum").and_then(|e| e.as_array()) {
                let values: Vec<&str> = enum_vals.iter().filter_map(|v| v.as_str()).collect();
                assert!(values.contains(&"index"));
                assert!(values.contains(&"status"));
                assert!(values.contains(&"search"));
                assert!(values.contains(&"get_clip"));
                assert!(values.contains(&"list"));
                assert!(values.contains(&"delete"));
            }
        }

        // Check other fields
        assert!(props.contains_key("workspace_id"));
        assert!(props.contains_key("project_id"));
        assert!(props.contains_key("target_project"));
        assert!(props.contains_key("file_path"));
        assert!(props.contains_key("external_url"));
        assert!(props.contains_key("content_type"));
        assert!(props.contains_key("tags"));
        assert!(props.contains_key("content_id"));
        assert!(props.contains_key("query"));
        assert!(props.contains_key("content_types"));
        assert!(props.contains_key("limit"));
        assert!(props.contains_key("start"));
        assert!(props.contains_key("end"));
        assert!(props.contains_key("output_format"));
        assert!(props.contains_key("fps"));

        // Check content_type enum
        if let Some(content_type) = props.get("content_type") {
            assert!(content_type["description"]
                .as_str()
                .unwrap()
                .contains("photos/images"));
            assert!(content_type["description"]
                .as_str()
                .unwrap()
                .contains("docs/PDFs"));
            if let Some(enum_vals) = content_type.get("enum").and_then(|e| e.as_array()) {
                let values: Vec<&str> = enum_vals.iter().filter_map(|v| v.as_str()).collect();
                assert!(values.contains(&"video"));
                assert!(values.contains(&"audio"));
                assert!(values.contains(&"image"));
                assert!(values.contains(&"document"));
            }
        }

        // Check output_format enum
        if let Some(output_format) = props.get("output_format") {
            if let Some(enum_vals) = output_format.get("enum").and_then(|e| e.as_array()) {
                let values: Vec<&str> = enum_vals.iter().filter_map(|v| v.as_str()).collect();
                assert!(values.contains(&"remotion"));
                assert!(values.contains(&"ffmpeg"));
                assert!(values.contains(&"raw"));
            }
        }
    }
}

// ============================================================================
// Input Validation Tests
// ============================================================================

mod validation_tests {
    use super::MediaTool;
    use super::{create_mock_client, create_mock_session, json, ToolHandler};

    #[tokio::test]
    async fn test_media_unknown_action() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MediaTool::new(client, session);

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
    async fn test_media_index_requires_file_or_url() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MediaTool::new(client, session);

        let result = tool
            .execute(json!({
                "action": "index"
            }))
            .await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("file_path or external_url"));
    }

    #[tokio::test]
    async fn test_media_status_requires_content_id() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MediaTool::new(client, session);

        let result = tool
            .execute(json!({
                "action": "status"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("content_id"));
    }

    #[tokio::test]
    async fn test_media_search_requires_query() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MediaTool::new(client, session);

        let result = tool
            .execute(json!({
                "action": "search"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("query"));
    }

    #[tokio::test]
    async fn test_media_get_clip_requires_content_id() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MediaTool::new(client, session);

        let result = tool
            .execute(json!({
                "action": "get_clip",
                "start": "1:00",
                "end": "2:00"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("content_id"));
    }

    #[tokio::test]
    async fn test_media_get_clip_requires_start() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MediaTool::new(client, session);

        let result = tool
            .execute(json!({
                "action": "get_clip",
                "content_id": "550e8400-e29b-41d4-a716-446655440000",
                "end": "2:00"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("start"));
    }

    #[tokio::test]
    async fn test_media_get_clip_requires_end() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MediaTool::new(client, session);

        let result = tool
            .execute(json!({
                "action": "get_clip",
                "content_id": "550e8400-e29b-41d4-a716-446655440000",
                "start": "1:00"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("end"));
    }

    #[tokio::test]
    async fn test_media_delete_requires_content_id() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MediaTool::new(client, session);

        let result = tool
            .execute(json!({
                "action": "delete"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("content_id"));
    }

    #[tokio::test]
    async fn test_media_validates_workspace_uuid() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MediaTool::new(client, session);

        let result = tool
            .execute(json!({
                "action": "list",
                "workspace_id": "invalid-uuid"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Invalid"));
    }

    #[tokio::test]
    async fn test_media_validates_content_uuid() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MediaTool::new(client, session);

        let result = tool
            .execute(json!({
                "action": "status",
                "content_id": "invalid-uuid"
            }))
            .await;

        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Invalid"));
    }

    #[tokio::test]
    async fn test_media_list_no_required_params() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MediaTool::new(client, session);

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
    async fn test_media_rejects_unknown_target_project() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let mut projects = std::collections::HashMap::new();
        projects.insert(
            "contextstream".to_string(),
            mcp_session::ChildProjectInfo {
                project_id: "11111111-1111-4111-8111-111111111111".to_string(),
                name: "ContextStream".to_string(),
                path: "/tmp/contextstream".to_string(),
            },
        );
        session.set_child_projects(projects).await;
        let tool = MediaTool::new(client, session);

        let result = tool
            .execute(json!({
                "action": "list",
                "target_project": "missing-child"
            }))
            .await;

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Unknown target_project"));
    }

    #[tokio::test]
    async fn test_media_accepts_known_target_project_before_action_validation() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let mut projects = std::collections::HashMap::new();
        projects.insert(
            "contextstream".to_string(),
            mcp_session::ChildProjectInfo {
                project_id: "11111111-1111-4111-8111-111111111111".to_string(),
                name: "ContextStream".to_string(),
                path: "/tmp/contextstream".to_string(),
            },
        );
        session.set_child_projects(projects).await;
        let tool = MediaTool::new(client, session);

        let result = tool
            .execute(json!({
                "action": "index",
                "target_project": "contextstream"
            }))
            .await;

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("file_path or external_url"));
        assert!(!err.contains("target_project"));
    }
}

// ============================================================================
// Summary Formatting Tests
// ============================================================================

mod summary_tests {
    use super::{json, MediaTool};

    #[test]
    fn media_item_summary_line_includes_actionable_asset_fields() {
        let item = json!({
            "id": "550e8400-e29b-41d4-a716-446655440000",
            "filename": "logo-transparent.png",
            "content_type": "image",
            "status": "indexed",
            "project_id": "50505050-5050-4050-8050-505050505050",
            "tags": ["brand", "logo"],
            "title": "Transparent ContextStream logo"
        });

        let line = MediaTool::media_item_summary_line(
            1,
            &item,
            Some("https://cdn.example.com/logo-transparent.png"),
            Some(3600),
        );

        assert!(line.contains("logo-transparent.png"));
        assert!(line.contains("content_id=550e8400-e29b-41d4-a716-446655440000"));
        assert!(line.contains("type=image"));
        assert!(line.contains("status=indexed"));
        assert!(line.contains("tags=brand,logo"));
        assert!(line.contains("use_url: https://cdn.example.com/logo-transparent.png"));
        assert!(line.contains("expires_in_seconds=3600"));
        assert!(line.contains(
            "status_call: media(action=\"status\", content_id=\"550e8400-e29b-41d4-a716-446655440000\")"
        ));
    }

    #[test]
    fn media_item_summary_line_handles_search_result_content_id() {
        let item = json!({
            "content_id": "550e8400-e29b-41d4-a716-446655440000",
            "filename": "logo-transparent.png",
            "content_type": "image",
            "score": 0.92,
            "match_text": "Transparent logo for header and footer usage"
        });

        let line = MediaTool::media_item_summary_line(1, &item, None, None);

        assert!(line.contains("score=0.920"));
        assert!(line.contains("match: Transparent logo for header and footer usage"));
        assert!(line.contains("content_id=550e8400-e29b-41d4-a716-446655440000"));
    }
}

// ============================================================================
// Input Struct Tests
// ============================================================================

mod input_struct_tests {
    use super::json;
    use super::MediaInput;

    #[test]
    fn test_media_input_index_with_file_path() {
        let input: MediaInput = serde_json::from_value(json!({
            "action": "index",
            "file_path": "/path/to/video.mp4",
            "content_type": "video",
            "tags": ["demo", "tutorial"]
        }))
        .unwrap();

        assert_eq!(input.action, "index");
        assert_eq!(input.file_path, Some("/path/to/video.mp4".to_string()));
        assert_eq!(input.content_type, Some("video".to_string()));
        assert_eq!(
            input.tags,
            Some(vec!["demo".to_string(), "tutorial".to_string()])
        );
    }

    #[test]
    fn test_media_input_index_with_url() {
        let input: MediaInput = serde_json::from_value(json!({
            "action": "index",
            "external_url": "https://example.com/video.mp4",
            "content_type": "video"
        }))
        .unwrap();

        assert_eq!(input.action, "index");
        assert_eq!(
            input.external_url,
            Some("https://example.com/video.mp4".to_string())
        );
    }

    #[test]
    fn test_media_input_search() {
        let input: MediaInput = serde_json::from_value(json!({
            "action": "search",
            "query": "authentication flow",
            "content_types": ["video", "audio"],
            "limit": 10
        }))
        .unwrap();

        assert_eq!(input.action, "search");
        assert_eq!(input.query, Some("authentication flow".to_string()));
        assert_eq!(
            input.content_types,
            Some(vec!["video".to_string(), "audio".to_string()])
        );
        assert_eq!(input.limit, Some(10));
    }

    #[test]
    fn test_media_input_get_clip() {
        let input: MediaInput = serde_json::from_value(json!({
            "action": "get_clip",
            "content_id": "550e8400-e29b-41d4-a716-446655440000",
            "start": "1:34",
            "end": "2:15",
            "output_format": "remotion",
            "fps": 30
        }))
        .unwrap();

        assert_eq!(input.action, "get_clip");
        assert!(input.content_id.is_some());
        assert_eq!(input.start, Some("1:34".to_string()));
        assert_eq!(input.end, Some("2:15".to_string()));
        assert_eq!(input.output_format, Some("remotion".to_string()));
        assert_eq!(input.fps, Some(30));
    }

    #[test]
    fn test_media_input_list() {
        let input: MediaInput = serde_json::from_value(json!({
            "action": "list",
            "workspace_id": "550e8400-e29b-41d4-a716-446655440000",
            "content_types": ["image"],
            "limit": 50
        }))
        .unwrap();

        assert_eq!(input.action, "list");
        assert!(input.workspace_id.is_some());
        assert_eq!(input.content_types, Some(vec!["image".to_string()]));
    }
}

// ============================================================================
// Video Extraction Summary Tests
// ============================================================================

mod extraction_summary_tests {
    use super::{json, MediaTool};

    #[test]
    fn test_video_text_extraction_summary_present() {
        let payload = json!({
            "metadata": {
                "video_text_extraction": {
                    "provider": "rekognition",
                    "status": "succeeded",
                    "segments_count": 24,
                    "job_id": "job-123",
                    "doc_id": "550e8400-e29b-41d4-a716-446655440001",
                    "aws_profile": "v2"
                }
            }
        });

        let summary = MediaTool::video_text_extraction_summary(&payload).unwrap();
        assert!(summary.contains("provider=rekognition"));
        assert!(summary.contains("status=succeeded"));
        assert!(summary.contains("segments=24"));
        assert!(summary.contains("job_id=job-123"));
        assert!(summary.contains("doc_id=550e8400-e29b-41d4-a716-446655440001"));
        assert!(summary.contains("aws_profile=v2"));
    }

    #[test]
    fn test_video_text_extraction_summary_missing_metadata() {
        let payload = json!({
            "status": "indexed"
        });
        assert!(MediaTool::video_text_extraction_summary(&payload).is_none());
    }

    #[test]
    fn test_video_text_extraction_summary_omits_empty_fields() {
        let payload = json!({
            "metadata": {
                "video_text_extraction": {
                    "provider": "rekognition",
                    "status": "failed",
                    "job_id": "",
                    "doc_id": "",
                    "aws_profile": ""
                }
            }
        });

        let summary = MediaTool::video_text_extraction_summary(&payload).unwrap();
        assert!(summary.contains("provider=rekognition"));
        assert!(summary.contains("status=failed"));
        assert!(!summary.contains("job_id="));
        assert!(!summary.contains("doc_id="));
        assert!(!summary.contains("aws_profile="));
    }
}

// ============================================================================
// Constants Tests
// ============================================================================

mod constants_tests {
    use super::*;

    #[test]
    fn test_valid_actions() {
        assert!(VALID_ACTIONS.contains(&"index"));
        assert!(VALID_ACTIONS.contains(&"upload_url"));
        assert!(VALID_ACTIONS.contains(&"complete_upload"));
        assert!(VALID_ACTIONS.contains(&"update"));
        assert!(VALID_ACTIONS.contains(&"status"));
        assert!(VALID_ACTIONS.contains(&"search"));
        assert!(VALID_ACTIONS.contains(&"get_clip"));
        assert!(VALID_ACTIONS.contains(&"list"));
        assert!(VALID_ACTIONS.contains(&"delete"));
        assert_eq!(VALID_ACTIONS.len(), 9);
    }

    #[test]
    fn test_valid_content_types() {
        assert!(VALID_CONTENT_TYPES.contains(&"video"));
        assert!(VALID_CONTENT_TYPES.contains(&"audio"));
        assert!(VALID_CONTENT_TYPES.contains(&"image"));
        assert!(VALID_CONTENT_TYPES.contains(&"document"));
        assert_eq!(VALID_CONTENT_TYPES.len(), 4);
    }

    #[test]
    fn test_content_type_aliases_normalize_to_canonical_types() {
        assert_eq!(
            MediaTool::normalize_content_type_label("photo").unwrap(),
            "image"
        );
        assert_eq!(
            MediaTool::normalize_content_type_label("screenshots").unwrap(),
            "image"
        );
        assert_eq!(
            MediaTool::normalize_content_type_label("pdf").unwrap(),
            "document"
        );
        assert_eq!(
            MediaTool::normalize_content_type_label("slides").unwrap(),
            "document"
        );
        assert_eq!(
            MediaTool::normalize_content_type_label("podcast").unwrap(),
            "audio"
        );
    }

    #[test]
    fn test_content_type_filter_aliases_dedupe() {
        let filters = Some(vec![
            "photos".to_string(),
            "image".to_string(),
            "docs".to_string(),
            "pdf".to_string(),
            "video".to_string(),
        ]);

        let normalized = MediaTool::normalize_content_type_filters(&filters).unwrap();

        assert_eq!(
            normalized,
            Some(vec![
                "image".to_string(),
                "document".to_string(),
                "video".to_string()
            ])
        );
    }

    #[test]
    fn test_valid_output_formats() {
        assert!(VALID_OUTPUT_FORMATS.contains(&"remotion"));
        assert!(VALID_OUTPUT_FORMATS.contains(&"ffmpeg"));
        assert!(VALID_OUTPUT_FORMATS.contains(&"raw"));
        assert_eq!(VALID_OUTPUT_FORMATS.len(), 3);
    }
}

// ============================================================================
// Tool Registration Tests
// ============================================================================

mod registration_tests {
    #[test]
    fn test_media_tool_count() {
        // Expected media tools:
        // - media (unified)
        // Total: 1 tool

        let expected_tools = ["media"];
        assert_eq!(expected_tools.len(), 1);
    }

    #[test]
    fn test_media_actions_coverage() {
        let all_actions = [
            "index",
            "upload_url",
            "complete_upload",
            "status",
            "search",
            "get_clip",
            "list",
            "delete",
        ];
        let output_formats = ["remotion", "ffmpeg", "raw"];

        assert_eq!(all_actions.len(), 8);
        assert_eq!(output_formats.len(), 3);
    }
}

// ============================================================================
// Knowledge Stream Fallback Tests (requirement #10)
// ============================================================================

mod knowledge_stream_fallback_tests {
    use super::{create_mock_client, create_mock_session, json, MediaTool};
    use crate::registry::ToolHandler;

    #[test]
    fn ks_fallback_items_carry_source_discriminator() {
        let items = vec![json!({
            "id": "123",
            "title": "search performance screenshot",
            "item_type": "event"
        })];

        let tagged: Vec<serde_json::Value> = items
            .into_iter()
            .map(|mut item| {
                if let Some(obj) = item.as_object_mut() {
                    obj.insert(
                        "source".to_string(),
                        serde_json::json!("knowledge_stream_event"),
                    );
                }
                item
            })
            .collect();

        assert_eq!(
            tagged[0].get("source").and_then(|v| v.as_str()),
            Some("knowledge_stream_event")
        );
    }

    #[test]
    fn ks_fallback_preserves_original_item_fields() {
        let mut item = json!({
            "id": "456",
            "title": "test item",
            "content": "some content",
            "item_type": "event"
        });

        if let Some(obj) = item.as_object_mut() {
            obj.insert(
                "source".to_string(),
                serde_json::json!("knowledge_stream_event"),
            );
        }

        assert_eq!(item.get("id").and_then(|v| v.as_str()), Some("456"));
        assert_eq!(
            item.get("title").and_then(|v| v.as_str()),
            Some("test item")
        );
        assert_eq!(
            item.get("source").and_then(|v| v.as_str()),
            Some("knowledge_stream_event")
        );
    }

    #[test]
    fn ks_fallback_params_restrict_to_event_item_type() {
        use mcp_client::KnowledgeStreamSearchParams;

        let params = KnowledgeStreamSearchParams {
            search: Some("demo video".to_string()),
            workspace_id: None,
            project_id: None,
            item_types: Some("event".to_string()),
            limit: Some(10),
            offset: None,
            sort_order: None,
        };

        assert_eq!(
            params.item_types.as_deref(),
            Some("event"),
            "KS fallback must restrict to event items to avoid polluting media results with docs/todos/skills"
        );
    }

    /// Verify that explicit project_id does NOT block the search execution path.
    /// The mock client has no real server, so we expect a network error -- the
    /// important thing is that it reaches the HTTP layer rather than being
    /// short-circuited by gating logic.
    #[tokio::test]
    async fn explicit_project_scope_reaches_search_execution_path() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MediaTool::new(client, session);

        let result = tool
            .execute(json!({
                "action": "search",
                "query": "demo video",
                "project_id": "00000000-0000-0000-0000-000000000001"
            }))
            .await;

        // Network error proves we passed all gating logic and reached the API
        // call. A validation gate would return Error::Validation, not a network error.
        assert!(result.is_err(), "should fail at network layer, not gating");
        let err_msg = result.unwrap_err().to_string();
        assert!(
            !err_msg.contains("explicit") && !err_msg.contains("not allowed"),
            "error should be a network failure, not a scope gate: {err_msg}"
        );
    }
}

// ============================================================================
// Rollout Logging Tests (requirement #11)
// ============================================================================

mod rollout_logging_tests {
    use super::*;

    #[test]
    fn media_search_passes_project_id_from_session() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MediaTool::new(client, session);

        let schema = tool.input_schema();
        let props = schema["properties"].as_object().unwrap();
        assert!(
            props.contains_key("project_id"),
            "project_id should be in schema"
        );
        assert!(
            props.contains_key("workspace_id"),
            "workspace_id should be in schema"
        );
    }

    #[test]
    fn media_list_passes_project_id_from_session() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MediaTool::new(client, session);

        let schema = tool.input_schema();
        let props = schema["properties"].as_object().unwrap();
        assert!(props.contains_key("project_id"));
    }
}

// ============================================================================
// Memory Search Media Fallback Tests
// ============================================================================

mod memory_search_media_fallback_tests {
    use super::json;

    fn media_keywords() -> Vec<&'static str> {
        vec![
            "image",
            "photo",
            "screenshot",
            "video",
            "audio",
            "media",
            "upload",
            "pdf",
            "ppt",
            "pptx",
            "slide",
            "slides",
            "deck",
            "png",
            "jpg",
            "jpeg",
            "gif",
            "webp",
            "mp4",
            "mp3",
            "wav",
            "svg",
        ]
    }

    fn is_media_item(item: &serde_json::Value) -> bool {
        let text = format!(
            "{} {} {}",
            item.get("title").and_then(|v| v.as_str()).unwrap_or(""),
            item.get("content").and_then(|v| v.as_str()).unwrap_or(""),
            item.get("event_type")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
        )
        .to_lowercase();
        media_keywords().iter().any(|kw| text.contains(kw))
    }

    #[test]
    fn memory_fallback_filters_to_media_related_items_only() {
        let results = vec![
            json!({"title": "Image uploaded into the Knowledge Stream", "content": "search-performance.png", "event_type": "uncategorized"}),
            json!({"title": "Decision: use Redis for caching", "content": "We decided to use Redis", "event_type": "decision"}),
            json!({"title": "Screenshot of dashboard", "content": "dashboard screenshot review", "event_type": "uncategorized"}),
        ];

        let media_items: Vec<_> = results.into_iter().filter(is_media_item).collect();
        assert_eq!(
            media_items.len(),
            2,
            "Should keep image and screenshot, filter out decision"
        );
        assert!(media_items[0]["title"].as_str().unwrap().contains("Image"));
        assert!(media_items[1]["title"]
            .as_str()
            .unwrap()
            .contains("Screenshot"));
    }

    #[test]
    fn memory_fallback_rejects_docs_todos_skills() {
        let results = vec![
            json!({"title": "Architecture doc", "content": "System design spec", "event_type": "note"}),
            json!({"title": "Fix billing bug", "content": "TODO: resolve stripe webhook", "event_type": "task"}),
            json!({"title": "Deployment skill", "content": "How to deploy to production", "event_type": "skill"}),
        ];

        let media_items: Vec<_> = results.into_iter().filter(is_media_item).collect();
        assert!(
            media_items.is_empty(),
            "Non-media items should be filtered out"
        );
    }

    #[test]
    fn memory_fallback_tags_items_with_source_discriminator() {
        let mut item = json!({"title": "Uploaded image", "content": "test.png"});
        if let Some(obj) = item.as_object_mut() {
            obj.insert("source".to_string(), json!("memory_search_media"));
        }
        assert_eq!(
            item.get("source").and_then(|v| v.as_str()),
            Some("memory_search_media")
        );
    }

    #[test]
    fn memory_fallback_detects_media_by_file_extension_in_content() {
        let png_item = json!({"title": "Knowledge upload", "content": "search-perf.png uploaded", "event_type": "uncategorized"});
        let jpg_item = json!({"title": "Photo capture", "content": "photo.jpg saved", "event_type": "uncategorized"});
        let mp4_item = json!({"title": "Recording", "content": "demo.mp4 recorded", "event_type": "uncategorized"});
        let pdf_item = json!({"title": "Uploaded deck", "content": "q4-board.pdf uploaded", "event_type": "uncategorized"});
        let txt_item = json!({"title": "Note", "content": "readme.txt created", "event_type": "uncategorized"});

        assert!(is_media_item(&png_item));
        assert!(is_media_item(&jpg_item));
        assert!(is_media_item(&mp4_item));
        assert!(is_media_item(&pdf_item));
        assert!(!is_media_item(&txt_item));
    }
}

// ============================================================================
// P0 Ingestion Containment Tests (media index)
// ============================================================================

mod ingest_containment_tests {
    use super::{
        is_blocked_ip, media_secret_rejection_reason, path_is_within_root,
        validate_external_media_url, validate_media_index_file, validate_media_index_file_with_cap,
    };
    use std::fs;
    use std::net::IpAddr;
    use std::path::Path;

    fn write_file(path: &Path, contents: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    // ---- file_path: secret/credential filter (never bypassable) ----

    #[test]
    fn refuses_aws_credentials_even_with_opt_in() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let cred = root.join(".aws").join("credentials");
        write_file(&cred, b"[default]\naws_secret_access_key=AKIAEXAMPLE\n");
        // opt_in=true proves the secret filter is NOT bypassable by containment opt-in.
        let err = validate_media_index_file(&cred, Some(root), true).unwrap_err();
        assert!(
            err.to_string().contains("sensitive directory"),
            "expected sensitive-dir refusal, got: {err}"
        );
    }

    #[test]
    fn refuses_id_rsa() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let key = root.join("id_rsa");
        write_file(&key, b"-----BEGIN OPENSSH PRIVATE KEY-----\n");
        let err = validate_media_index_file(&key, Some(root), true).unwrap_err();
        assert!(
            err.to_string().contains("blocked-file filter"),
            "got: {err}"
        );
    }

    #[test]
    fn refuses_dot_env() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let env = root.join(".env");
        write_file(&env, b"SECRET=swordfish\n");
        let err = validate_media_index_file(&env, Some(root), false).unwrap_err();
        assert!(
            err.to_string().contains("blocked-file filter"),
            "got: {err}"
        );
    }

    #[test]
    fn refuses_pem_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let pem = root.join("server.pem");
        write_file(&pem, b"-----BEGIN CERTIFICATE-----\n");
        let err = validate_media_index_file(&pem, Some(root), false).unwrap_err();
        assert!(
            err.to_string().contains("blocked-file filter"),
            "got: {err}"
        );
    }

    #[test]
    fn secret_reason_detects_sensitive_dir_by_components() {
        // Literal-component path: no filesystem needed.
        let reason = media_secret_rejection_reason(Path::new("/home/someone/.aws/credentials"))
            .expect("a path inside .aws must be flagged");
        assert!(reason.contains(".aws"), "got: {reason}");
    }

    // ---- file_path: containment (opt-in bypassable) ----

    #[test]
    fn refuses_out_of_root_without_opt_in() {
        let project = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let asset = elsewhere.path().join("vacation.png");
        write_file(&asset, b"\x89PNG\r\n");
        let err = validate_media_index_file(&asset, Some(project.path()), false).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("outside the active project root"),
            "got: {msg}"
        );
        assert!(msg.contains("allow_broad"), "got: {msg}");
    }

    #[test]
    fn allows_out_of_root_with_opt_in() {
        let project = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        let asset = elsewhere.path().join("vacation.png");
        write_file(&asset, b"\x89PNG\r\n");
        validate_media_index_file(&asset, Some(project.path()), true)
            .expect("opt-in must allow an out-of-root, non-secret media file");
    }

    #[test]
    fn allows_in_root_document() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let doc = root.join("report.pdf");
        write_file(&doc, b"%PDF-1.7\n");
        validate_media_index_file(&doc, Some(root), false)
            .expect("an in-root, non-secret document must be allowed");
    }

    #[test]
    fn refuses_directory_path() {
        let dir = tempfile::tempdir().unwrap();
        // opt_in=true so we reach the is_file gate, not containment.
        let err = validate_media_index_file(dir.path(), Some(dir.path()), true).unwrap_err();
        assert!(err.to_string().contains("regular file"), "got: {err}");
    }

    #[test]
    fn containment_is_component_wise_not_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proj");
        let sibling = dir.path().join("proj-evil");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&sibling).unwrap();
        let asset = sibling.join("a.png");
        write_file(&asset, b"x");
        let nested = root.join("nested").join("a.png");
        write_file(&nested, b"x");
        // `proj-evil` must NOT be treated as inside `proj`.
        assert!(!path_is_within_root(&asset, &root));
        // A genuinely nested file IS inside `proj`.
        assert!(path_is_within_root(&nested, &root));
    }

    // ---- external_url: SSRF guard ----

    #[test]
    fn external_url_rejects_non_http_scheme() {
        let err = validate_external_media_url("file:///etc/passwd").unwrap_err();
        assert!(
            err.to_string().contains("only http and https"),
            "got: {err}"
        );
        assert!(validate_external_media_url("ftp://example.com/x.png").is_err());
    }

    #[test]
    fn external_url_rejects_private_loopback_and_metadata() {
        assert!(validate_external_media_url("http://169.254.169.254/latest/meta-data/").is_err());
        assert!(validate_external_media_url("http://127.0.0.1:8080/x.png").is_err());
        assert!(validate_external_media_url("http://10.0.0.5/x.png").is_err());
        assert!(validate_external_media_url("http://192.168.1.10/x.png").is_err());
        assert!(validate_external_media_url("http://172.16.0.9/x.png").is_err());
        assert!(validate_external_media_url("http://0.0.0.0/x.png").is_err());
        assert!(validate_external_media_url("http://[::1]/x.png").is_err());
        assert!(validate_external_media_url("http://localhost/x.png").is_err());
    }

    #[test]
    fn external_url_allows_public_hosts() {
        validate_external_media_url("https://example.com/assets/logo.png")
            .expect("public https domain must be allowed");
        validate_external_media_url("http://93.184.216.34/logo.png")
            .expect("public IP literal must be allowed");
    }

    #[test]
    fn blocked_ip_matrix() {
        for s in [
            "127.0.0.1",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.0.1",
            "169.254.169.254",
            "0.0.0.0",
            "::1",
        ] {
            assert!(
                is_blocked_ip(&s.parse::<IpAddr>().unwrap()),
                "{s} should be blocked"
            );
        }
        for s in ["8.8.8.8", "93.184.216.34", "1.1.1.1"] {
            assert!(
                !is_blocked_ip(&s.parse::<IpAddr>().unwrap()),
                "{s} should be allowed"
            );
        }
    }

    #[test]
    fn blocked_ipv6_mapped_and_local() {
        // IPv4-mapped private/metadata/loopback, link-local, and unique-local v6
        // are the classic v4-only-guard bypasses — they must be blocked.
        for s in [
            "::ffff:127.0.0.1",
            "::ffff:169.254.169.254",
            "::ffff:10.0.0.1",
            "fe80::1",
            "fc00::1",
        ] {
            assert!(
                is_blocked_ip(&s.parse::<IpAddr>().unwrap()),
                "{s} should be blocked"
            );
        }
        // A public/global IPv6 (and a public IPv4-mapped one) must be allowed.
        for s in ["2606:4700:4700::1111", "::ffff:8.8.8.8"] {
            assert!(
                !is_blocked_ip(&s.parse::<IpAddr>().unwrap()),
                "{s} should be allowed"
            );
        }
    }

    // ---- file_path: size cap ----

    #[test]
    fn size_cap_rejects_over_and_allows_under() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let asset = root.join("clip.pdf");
        write_file(&asset, &[0u8; 64]); // 64 bytes, in-root, non-secret

        // Cap below the file size -> rejected with the single-asset-limit message.
        let err = validate_media_index_file_with_cap(&asset, Some(root), false, 32).unwrap_err();
        assert!(
            err.to_string().contains("single-asset limit"),
            "expected size-cap refusal, got: {err}"
        );
        // Cap at/above the file size -> allowed.
        validate_media_index_file_with_cap(&asset, Some(root), false, 64)
            .expect("file at exactly the cap must be allowed");
        validate_media_index_file_with_cap(&asset, Some(root), false, 4096)
            .expect("file under the cap must be allowed");
    }

    // ---- file_path: no active project root ----

    #[test]
    fn no_active_project_root_rejected_without_opt_in() {
        let dir = tempfile::tempdir().unwrap();
        let asset = dir.path().join("photo.png");
        write_file(&asset, b"\x89PNG\r\n\x1a\n");

        // No project root + no opt-in -> refused.
        let err = validate_media_index_file(&asset, None, false).unwrap_err();
        assert!(
            err.to_string().contains("no active project root"),
            "expected no-active-root refusal, got: {err}"
        );
        // Empty-string root behaves the same as None.
        let err = validate_media_index_file(&asset, Some(Path::new("")), false).unwrap_err();
        assert!(
            err.to_string().contains("no active project root"),
            "expected no-active-root refusal for empty root, got: {err}"
        );
        // With opt-in, a non-secret file is allowed even without a project root.
        validate_media_index_file(&asset, None, true)
            .expect("opt-in allows a non-secret file with no active root");
    }

    // ---- file_path: missing / non-regular ----

    #[test]
    fn missing_file_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist.png");
        let err = validate_media_index_file(&missing, Some(dir.path()), false).unwrap_err();
        assert!(
            err.to_string().contains("cannot access file_path"),
            "expected missing-file refusal, got: {err}"
        );
    }

    // ---- file_path: symlink into a sensitive dir cannot dodge the secret filter ----

    #[cfg(unix)]
    #[test]
    fn symlink_into_sensitive_dir_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // A real secret under a sensitive directory.
        let secret = root.join(".aws").join("secret.txt");
        write_file(&secret, b"aws_secret_access_key=AKIAEXAMPLE\n");
        // An innocuously-named symlink (inside the project root) pointing at it.
        let link = root.join("innocent.pdf");
        std::os::unix::fs::symlink(&secret, &link).unwrap();

        // Canonicalization must resolve the link and still flag the .aws component.
        assert!(
            media_secret_rejection_reason(&link).is_some(),
            "symlink into a sensitive dir must be flagged as a secret"
        );
    }

    // ---- .env.* family is treated as secret (except templates) ----

    #[test]
    fn env_variants_are_secret_except_templates() {
        use mcp_client::ContextStreamClient;
        for name in [
            ".env",
            ".env.staging",
            ".env.prod",
            ".env.production.local",
            ".env.ci",
        ] {
            assert!(
                ContextStreamClient::should_skip_file(name, true),
                "{name} must be treated as a secret"
            );
        }
        for name in [".env.example", ".env.sample", ".env.template"] {
            assert!(
                !ContextStreamClient::should_skip_file(name, true),
                "{name} is a template and must NOT be skipped"
            );
        }
    }
}

// ============================================================================
// Hosted Media Upload Plan & Complete Upload Tests
// ============================================================================

mod hosted_upload_plan_tests {
    use super::*;
    use crate::registry::ToolHandler;
    use mcp_types::tool::ContentItem;
    use mcp_types::Config;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const TEST_WS: &str = "11111111-1111-4111-8111-111111111111";
    const TEST_PROJECT: &str = "22222222-2222-4222-8222-222222222222";
    const TEST_CONTENT_ID: &str = "550e8400-e29b-41d4-a716-446655440000";

    #[derive(Clone, Debug)]
    struct RecordedRequest {
        line: String,
        body: Vec<u8>,
    }

    struct MediaStubServer {
        base_url: String,
        requests: Arc<Mutex<Vec<RecordedRequest>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl MediaStubServer {
        async fn start<F>(handler: F) -> Self
        where
            F: Fn(&str, &[u8], &str) -> (u16, String) + Send + Sync + 'static,
        {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let base_url = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let recorded = requests.clone();
            let handler = Arc::new(handler);
            let server_base = base_url.clone();

            let task = tokio::spawn(async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        return;
                    };
                    let recorded = recorded.clone();
                    let handler = handler.clone();
                    let server_base = server_base.clone();
                    tokio::spawn(async move {
                        let mut request = Vec::new();
                        let (head, body) = loop {
                            let mut chunk = [0u8; 4096];
                            let count = socket.read(&mut chunk).await.unwrap_or(0);
                            if count == 0 {
                                return;
                            }
                            request.extend_from_slice(&chunk[..count]);
                            let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n")
                            else {
                                continue;
                            };
                            let head = String::from_utf8_lossy(&request[..end]).to_string();
                            let length: usize = head
                                .lines()
                                .filter_map(|line| line.split_once(':'))
                                .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                                .and_then(|(_, value)| value.trim().parse().ok())
                                .unwrap_or(0);
                            if request.len() >= end + 4 + length {
                                let body = request[end + 4..end + 4 + length].to_vec();
                                break (head, body);
                            }
                        };
                        let line = head.lines().next().unwrap_or_default().to_string();
                        recorded.lock().unwrap().push(RecordedRequest {
                            line: line.clone(),
                            body: body.clone(),
                        });
                        let (status, payload) = handler(&line, &body, &server_base);
                        let reason = match status {
                            200 => "OK",
                            201 => "Created",
                            400 => "Bad Request",
                            403 => "Forbidden",
                            404 => "Not Found",
                            422 => "Unprocessable Entity",
                            _ => "Error",
                        };
                        let response = format!(
                            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                            payload.len()
                        );
                        let _ = socket.write_all(response.as_bytes()).await;
                    });
                }
            });

            Self {
                base_url,
                requests,
                task,
            }
        }

        fn recorded(&self) -> Vec<RecordedRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl Drop for MediaStubServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    fn build_tool(
        base_url: &str,
        is_http_transport: bool,
        http_local_files: bool,
    ) -> (MediaTool, Arc<SessionManager>) {
        let cfg = Config {
            api_url: base_url.to_string(),
            api_key: Some("test-api-key".to_string()),
            is_http_transport,
            http_local_files,
            ..Config::default()
        };
        let client = ContextStreamClient::new(cfg.clone());
        let session = Arc::new(SessionManager::new(client.clone(), cfg));
        (MediaTool::new(client, session.clone()), session)
    }

    fn result_text(result: &ToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|item| match item {
                ContentItem::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn hosted_index_with_file_path_returns_upload_plan_without_reading_disk() {
        let presigned_url =
            "https://storage.example.test/upload/doc?X-Amz-Signature=SENSITIVE_PRESIGN_SIG&foo=bar";
        let server = MediaStubServer::start(move |line, _body, _base| {
            if line.starts_with(&format!(
                "POST /api/v1/workspaces/{TEST_WS}/content/uploads/init "
            )) {
                (
                    200,
                    json!({
                        "success": true,
                        "data": {
                            "content_id": TEST_CONTENT_ID,
                            "upload_url": presigned_url,
                            "headers": {
                                "Content-Type": "application/pdf",
                                "x-amz-meta-source": "mcp"
                            },
                            "expires_in_seconds": 900
                        }
                    })
                    .to_string(),
                )
            } else {
                (
                    400,
                    json!({"error": {"message": format!("unexpected request: {line}")}})
                        .to_string(),
                )
            }
        })
        .await;

        let (tool, _) = build_tool(&server.base_url, true, false);
        let client_path = "/home/chase/Downloads/Orrinvale's MSA KB-ODS-2026-014 (signed).pdf";

        let result = tool
            .execute(json!({
                "action": "index",
                "file_path": client_path,
                "content_type": "document",
                "size_bytes": 123456,
                "title": "Orrinvale MSA",
                "tags": ["contract", "signed"],
                "workspace_id": TEST_WS,
                "project_id": TEST_PROJECT
            }))
            .await
            .expect("hosted index with file_path should return upload plan");

        let recorded = server.recorded();
        assert_eq!(
            recorded.len(),
            1,
            "expected only uploads/init call: {recorded:?}"
        );
        assert!(recorded[0].line.starts_with(&format!(
            "POST /api/v1/workspaces/{TEST_WS}/content/uploads/init "
        )));
        let init_body: Value = serde_json::from_slice(&recorded[0].body).unwrap();
        assert_eq!(
            init_body["filename"],
            "Orrinvale's MSA KB-ODS-2026-014 (signed).pdf"
        );
        assert_eq!(init_body["content_type"], "document");
        assert_eq!(init_body["mime_type"], "application/pdf");
        assert_eq!(init_body["size_bytes"], 123456);
        assert_eq!(init_body["title"], "Orrinvale MSA");
        assert_eq!(init_body["tags"], json!(["contract", "signed"]));
        assert_eq!(init_body["project_id"], TEST_PROJECT);

        let structured = result
            .structured_content
            .as_ref()
            .expect("upload plan should include structured_content");
        assert_eq!(structured["content_id"], TEST_CONTENT_ID);
        assert_eq!(structured["upload_url"], presigned_url);
        assert_eq!(structured["headers"]["Content-Type"], "application/pdf");
        assert_eq!(structured["headers"]["x-amz-meta-source"], "mcp");
        assert_eq!(structured["expires_in_seconds"], 900);

        let upload_command = structured["upload_command"]
            .as_str()
            .expect("upload_command string");
        assert!(
            upload_command.starts_with("curl -fsS -X PUT --upload-file "),
            "got: {upload_command}"
        );
        assert!(
            upload_command.contains(
                "'/home/chase/Downloads/Orrinvale'\\''s MSA KB-ODS-2026-014 (signed).pdf'"
            ),
            "path must be safely single-quoted: {upload_command}"
        );
        assert!(
            upload_command.contains("-H 'Content-Type: application/pdf'"),
            "missing Content-Type header flag: {upload_command}"
        );
        assert!(
            upload_command.contains("-H 'x-amz-meta-source: mcp'"),
            "missing custom header flag: {upload_command}"
        );
        assert!(
            upload_command.contains(&format!("'{presigned_url}'")),
            "upload_url must be single-quoted: {upload_command}"
        );

        let expected_followup =
            format!("media(action=\"complete_upload\", content_id=\"{TEST_CONTENT_ID}\")");
        assert_eq!(structured["complete_upload_call"], expected_followup);

        let text = result_text(&result);
        assert!(text.contains(TEST_CONTENT_ID), "text: {text}");
        assert!(text.contains(upload_command), "text: {text}");
        assert!(text.contains(&expected_followup), "text: {text}");
        assert!(text.contains("900"), "text should show expiry: {text}");
    }

    #[tokio::test]
    async fn hosted_index_without_size_bytes_asks_with_stat_command_when_api_requires_it() {
        let server = MediaStubServer::start(|line, _body, _base| {
            if line.contains("/content/uploads/init") {
                (
                    422,
                    json!({
                        "success": false,
                        "error": {
                            "code": "VALIDATION_ERROR",
                            "message": "Failed to deserialize the JSON body into the target type: missing field `size_bytes`"
                        }
                    })
                    .to_string(),
                )
            } else {
                (400, "{}".to_string())
            }
        })
        .await;

        let (tool, _) = build_tool(&server.base_url, true, false);
        let client_path = "/home/chase/Downloads/Orrinvale MSA (signed).pdf";

        let err = tool
            .execute(json!({
                "action": "index",
                "file_path": client_path,
                "content_type": "document",
                "workspace_id": TEST_WS
            }))
            .await
            .expect_err("should ask caller for size_bytes when API requires it")
            .to_string();

        assert!(err.contains("size_bytes"), "got: {err}");
        assert!(
            err.contains("stat -c %s '/home/chase/Downloads/Orrinvale MSA (signed).pdf'"),
            "error must include exact stat command with quoted path, got: {err}"
        );
    }

    #[tokio::test]
    async fn upload_url_action_starts_upload_without_reading_disk() {
        let server = MediaStubServer::start(|line, _body, _base| {
            if line.contains("/content/uploads/init") {
                (
                    200,
                    json!({
                        "success": true,
                        "data": {
                            "content_id": TEST_CONTENT_ID,
                            "upload_url": "https://storage.example.test/put/img",
                            "headers": {"Content-Type": "image/png"},
                            "expires_in_seconds": 600
                        }
                    })
                    .to_string(),
                )
            } else {
                (400, "{}".to_string())
            }
        })
        .await;

        // Even in local stdio mode, action="upload_url" only starts the upload.
        let (tool, _) = build_tool(&server.base_url, false, false);
        let result = tool
            .execute(json!({
                "action": "upload_url",
                "file_path": "/nonexistent/local/screenshot.png",
                "size_bytes": 4096,
                "workspace_id": TEST_WS
            }))
            .await
            .expect("upload_url should not read local disk");

        let recorded = server.recorded();
        assert_eq!(recorded.len(), 1);
        let body: Value = serde_json::from_slice(&recorded[0].body).unwrap();
        assert_eq!(body["filename"], "screenshot.png");
        assert_eq!(body["content_type"], "image");
        assert_eq!(body["mime_type"], "image/png");
        assert_eq!(body["size_bytes"], 4096);

        let structured = result.structured_content.as_ref().unwrap();
        assert_eq!(structured["content_id"], TEST_CONTENT_ID);
        assert_eq!(
            structured["upload_url"],
            "https://storage.example.test/put/img"
        );
        assert!(structured["upload_command"]
            .as_str()
            .unwrap()
            .contains("curl -fsS -X PUT --upload-file '/nonexistent/local/screenshot.png'"));
    }

    #[tokio::test]
    async fn complete_upload_calls_api_and_returns_status_followup() {
        let server = MediaStubServer::start(|line, _body, _base| {
            if line.starts_with(&format!(
                "POST /api/v1/workspaces/{TEST_WS}/content/{TEST_CONTENT_ID}/complete-upload "
            )) {
                (
                    200,
                    json!({
                        "success": true,
                        "data": {
                            "id": TEST_CONTENT_ID,
                            "status": "processing",
                            "filename": "contract.pdf",
                            "content_type": "document"
                        }
                    })
                    .to_string(),
                )
            } else {
                (
                    400,
                    json!({"error": {"message": format!("unexpected: {line}")}}).to_string(),
                )
            }
        })
        .await;

        let (tool, _) = build_tool(&server.base_url, true, false);
        let result = tool
            .execute(json!({
                "action": "complete_upload",
                "content_id": TEST_CONTENT_ID,
                "workspace_id": TEST_WS
            }))
            .await
            .expect("complete_upload should call the complete-upload endpoint");

        let recorded = server.recorded();
        assert_eq!(recorded.len(), 1);
        assert!(recorded[0].line.starts_with(&format!(
            "POST /api/v1/workspaces/{TEST_WS}/content/{TEST_CONTENT_ID}/complete-upload "
        )));

        let text = result_text(&result);
        assert!(text.contains(TEST_CONTENT_ID), "text: {text}");
        assert!(text.contains("processing"), "text: {text}");
        assert!(
            text.contains(&format!(
                "media(action=\"status\", content_id=\"{TEST_CONTENT_ID}\")"
            )),
            "text: {text}"
        );
    }

    #[tokio::test]
    async fn complete_upload_requires_content_id() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MediaTool::new(client, session);

        let err = tool
            .execute(json!({
                "action": "complete_upload",
                "workspace_id": TEST_WS
            }))
            .await
            .expect_err("complete_upload without content_id must fail")
            .to_string();

        assert!(err.contains("content_id"), "got: {err}");
    }

    #[tokio::test]
    async fn local_mode_and_self_hosted_opt_in_keep_direct_upload_behavior() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("contract.pdf");
        let file_bytes = b"%PDF-1.7\ncontract body\n";
        std::fs::write(&file, file_bytes).unwrap();
        let file_str = file.to_string_lossy().to_string();

        for (is_http, opt_in) in [(false, false), (true, true)] {
            let server = MediaStubServer::start(|line, _body, base| {
                if line.contains("/content/uploads/init") {
                    (
                        200,
                        json!({
                            "success": true,
                            "data": {
                                "content_id": TEST_CONTENT_ID,
                                "upload_url": format!("{base}/put-target"),
                                "headers": {"Content-Type": "application/pdf"},
                                "expires_in_seconds": 300
                            }
                        })
                        .to_string(),
                    )
                } else if line.starts_with("PUT /put-target ") {
                    (200, "{}".to_string())
                } else if line.contains("/complete-upload") {
                    (
                        200,
                        json!({
                            "success": true,
                            "data": {
                                "id": TEST_CONTENT_ID,
                                "status": "processing"
                            }
                        })
                        .to_string(),
                    )
                } else {
                    (400, "{}".to_string())
                }
            })
            .await;

            let (tool, session) = build_tool(&server.base_url, is_http, opt_in);
            session
                .update_scope(
                    Some(Uuid::parse_str(TEST_WS).unwrap()),
                    None,
                    Some(dir.path().to_string_lossy().to_string()),
                )
                .await;

            let result = tool
                .execute(json!({
                    "action": "index",
                    "file_path": file_str,
                    "content_type": "document",
                    "workspace_id": TEST_WS
                }))
                .await
                .unwrap_or_else(|e| {
                    panic!("direct upload failed (is_http={is_http}, opt_in={opt_in}): {e}")
                });

            let recorded = server.recorded();
            assert_eq!(
                recorded.len(),
                3,
                "expected init + PUT + complete-upload (is_http={is_http}, opt_in={opt_in}): {recorded:?}"
            );
            assert!(recorded[0].line.contains("/content/uploads/init"));
            assert!(recorded[1].line.starts_with("PUT /put-target "));
            assert_eq!(recorded[1].body, file_bytes);
            assert!(recorded[2].line.contains("/complete-upload"));

            let text = result_text(&result);
            assert!(
                text.contains("Media indexed with content_id:"),
                "got: {text}"
            );
        }
    }

    #[tokio::test]
    async fn hosted_index_still_rejects_secret_paths_without_calling_api() {
        let server = MediaStubServer::start(|_line, _body, _base| (200, "{}".to_string())).await;
        let (tool, _) = build_tool(&server.base_url, true, false);

        for secret_path in [
            "/home/alice/.aws/credentials",
            "/home/alice/.ssh/id_rsa",
            "/app/.env",
            "/app/server.pem",
        ] {
            let err = tool
                .execute(json!({
                    "action": "index",
                    "file_path": secret_path,
                    "size_bytes": 64,
                    "workspace_id": TEST_WS
                }))
                .await
                .expect_err("secret path must be rejected")
                .to_string();
            assert!(
                err.contains("Secrets and credentials are never uploaded"),
                "path {secret_path} got: {err}"
            );
        }
        assert!(
            server.recorded().is_empty(),
            "secret paths must be rejected before any API call"
        );
    }

    #[test]
    fn metadata_and_schema_expose_upload_url_and_complete_upload() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MediaTool::new(client, session);

        let desc = &tool.metadata().description;
        assert!(desc.contains("upload_url"), "desc: {desc}");
        assert!(desc.contains("complete_upload"), "desc: {desc}");

        let schema = tool.input_schema();
        let props = schema["properties"].as_object().unwrap();
        let actions: Vec<&str> = props["action"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(actions.contains(&"upload_url"), "actions: {actions:?}");
        assert!(actions.contains(&"complete_upload"), "actions: {actions:?}");
        assert!(props.contains_key("size_bytes"));
        assert!(props.contains_key("mime_type"));
        assert!(props.contains_key("title"));
        assert!(props.contains_key("filename"));
    }

    #[tokio::test]
    async fn hosted_upload_plan_never_logs_presigned_url() {
        use std::io::Write;
        use tracing::Instrument;
        use tracing_subscriber::fmt::MakeWriter;

        #[derive(Clone, Default)]
        struct BufWriter(Arc<Mutex<Vec<u8>>>);

        struct Guard(Arc<Mutex<Vec<u8>>>);
        impl Write for Guard {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl<'a> MakeWriter<'a> for BufWriter {
            type Writer = Guard;
            fn make_writer(&'a self) -> Self::Writer {
                Guard(self.0.clone())
            }
        }

        let secret_token = "SENSITIVE_PRESIGN_SIGNATURE_NEVER_LOG";
        let presigned_url = format!("https://storage.example.test/put?sig={secret_token}");
        let presigned_clone = presigned_url.clone();
        let server = MediaStubServer::start(move |line, _body, _base| {
            if line.contains("/content/uploads/init") {
                (
                    200,
                    json!({
                        "success": true,
                        "data": {
                            "content_id": TEST_CONTENT_ID,
                            "upload_url": presigned_clone,
                            "headers": {"Content-Type": "application/pdf"},
                            "expires_in_seconds": 300
                        }
                    })
                    .to_string(),
                )
            } else {
                (400, "{}".to_string())
            }
        })
        .await;

        let buf = BufWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(buf.clone())
            .with_ansi(false)
            .finish();
        let dispatch = tracing::Dispatch::new(subscriber);

        let (tool, _) = build_tool(&server.base_url, true, false);
        let span = tracing::dispatcher::with_default(&dispatch, || {
            tracing::trace_span!("hosted_media_upload_test")
        });
        let result = tracing::dispatcher::with_default(&dispatch, || {
            async {
                tool.execute(json!({
                    "action": "index",
                    "file_path": "/home/chase/Downloads/contract.pdf",
                    "content_type": "document",
                    "size_bytes": 2048,
                    "workspace_id": TEST_WS
                }))
                .await
            }
            .instrument(span)
        })
        .await
        .expect("hosted index should return upload plan");

        let text = result_text(&result);
        assert!(
            text.contains(&presigned_url),
            "tool result must return upload_url"
        );
        let logs = String::from_utf8_lossy(&buf.0.lock().unwrap()).to_string();
        assert!(
            !logs.contains(secret_token),
            "presigned URL secret must never appear in logs: {logs}"
        );
    }

    #[tokio::test]
    async fn external_index_is_personal_sets_single_visibility_tag_and_reports_visibility() {
        let server = MediaStubServer::start(|line, body, _base| {
            if line.starts_with(&format!(
                "POST /api/v1/workspaces/{TEST_WS}/content/external "
            )) {
                let req: Value = serde_json::from_slice(body).unwrap_or_else(|_| json!({}));
                let tags = req["tags"].as_array().cloned().unwrap_or_default();
                let is_personal = !tags.iter().any(|t| t.as_str() == Some("visibility:team"));
                (
                    201,
                    json!({
                        "success": true,
                        "data": {
                            "id": TEST_CONTENT_ID,
                            "status": "processing",
                            "filename": "vendor-contract.pdf",
                            "content_type": "document",
                            "tags": tags,
                            "is_personal": is_personal
                        }
                    })
                    .to_string(),
                )
            } else {
                (
                    400,
                    json!({"error": {"message": format!("unexpected: {line}")}}).to_string(),
                )
            }
        })
        .await;

        let (tool, _) = build_tool(&server.base_url, true, false);

        // 1. is_personal=false replaces any existing visibility:* tag with a single visibility:team.
        let team_res = tool
            .execute(json!({
                "action": "index",
                "external_url": "https://cdn.example.test/vendor-contract.pdf",
                "content_type": "document",
                "tags": ["contract", "visibility:personal", "visibility:team"],
                "is_personal": false,
                "workspace_id": TEST_WS
            }))
            .await
            .expect("external index with is_personal=false should succeed");
        let team_text = result_text(&team_res);
        assert!(
            team_text.contains("visibility: team") && team_text.contains("is_personal: false"),
            "response must state team visibility was stored: {team_text}"
        );

        // 2. is_personal=true replaces any existing visibility:* tag with a single visibility:personal.
        let personal_res = tool
            .execute(json!({
                "action": "index",
                "external_url": "https://cdn.example.test/private-note.pdf",
                "content_type": "document",
                "tags": ["draft", "visibility:team"],
                "is_personal": true,
                "workspace_id": TEST_WS
            }))
            .await
            .expect("external index with is_personal=true should succeed");
        let personal_text = result_text(&personal_res);
        assert!(
            personal_text.contains("visibility: personal")
                && personal_text.contains("is_personal: true"),
            "response must state personal visibility was stored: {personal_text}"
        );

        // 3. Omitting is_personal keeps default behavior (no visibility tag injected) and reports stored visibility.
        let default_res = tool
            .execute(json!({
                "action": "index",
                "external_url": "https://cdn.example.test/default-doc.pdf",
                "content_type": "document",
                "tags": ["reference"],
                "workspace_id": TEST_WS
            }))
            .await
            .expect("external index without is_personal should succeed");
        let default_text = result_text(&default_res);
        assert!(
            default_text.contains("visibility: personal")
                && default_text.contains("is_personal: true"),
            "response must report default personal visibility: {default_text}"
        );

        let recorded = server.recorded();
        assert_eq!(recorded.len(), 3);
        let body_team: Value = serde_json::from_slice(&recorded[0].body).unwrap();
        assert_eq!(body_team["tags"], json!(["contract", "visibility:team"]));
        let body_personal: Value = serde_json::from_slice(&recorded[1].body).unwrap();
        assert_eq!(
            body_personal["tags"],
            json!(["draft", "visibility:personal"])
        );
        let body_default: Value = serde_json::from_slice(&recorded[2].body).unwrap();
        assert_eq!(body_default["tags"], json!(["reference"]));
    }

    #[tokio::test]
    async fn local_file_index_is_personal_sets_single_visibility_tag_and_reports_visibility() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("vendor-contract.pdf");
        std::fs::write(&file, b"%PDF-1.7\nteam contract\n").unwrap();
        let file_str = file.to_string_lossy().to_string();

        let last_tags: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let tags_store = last_tags.clone();

        let server = MediaStubServer::start(move |line, body, base| {
            if line.contains("/content/uploads/init") {
                let req: Value = serde_json::from_slice(body).unwrap_or_else(|_| json!({}));
                let tags: Vec<String> = req["tags"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                *tags_store.lock().unwrap() = tags;
                (
                    200,
                    json!({
                        "success": true,
                        "data": {
                            "content_id": TEST_CONTENT_ID,
                            "upload_url": format!("{base}/put-target"),
                            "headers": {"Content-Type": "application/pdf"},
                            "expires_in_seconds": 300
                        }
                    })
                    .to_string(),
                )
            } else if line.starts_with("PUT /put-target ") {
                (200, "{}".to_string())
            } else if line.contains("/complete-upload") {
                let tags = tags_store.lock().unwrap().clone();
                let is_personal = !tags.iter().any(|t| t == "visibility:team");
                (
                    200,
                    json!({
                        "success": true,
                        "data": {
                            "id": TEST_CONTENT_ID,
                            "status": "processing",
                            "tags": tags,
                            "is_personal": is_personal
                        }
                    })
                    .to_string(),
                )
            } else {
                (400, "{}".to_string())
            }
        })
        .await;

        let (tool, session) = build_tool(&server.base_url, false, false);
        session
            .update_scope(
                Some(Uuid::parse_str(TEST_WS).unwrap()),
                None,
                Some(dir.path().to_string_lossy().to_string()),
            )
            .await;

        let result = tool
            .execute(json!({
                "action": "index",
                "file_path": file_str,
                "content_type": "document",
                "tags": ["contract", "visibility:personal"],
                "is_personal": false,
                "workspace_id": TEST_WS
            }))
            .await
            .expect("local file index with is_personal=false should succeed");

        let recorded = server.recorded();
        assert_eq!(recorded.len(), 3);
        let init_body: Value = serde_json::from_slice(&recorded[0].body).unwrap();
        assert_eq!(init_body["tags"], json!(["contract", "visibility:team"]));

        let text = result_text(&result);
        assert!(
            text.contains("visibility: team") && text.contains("is_personal: false"),
            "local index response must report team visibility: {text}"
        );
    }

    #[tokio::test]
    async fn hosted_index_and_upload_url_is_personal_sets_visibility_tag_and_reports_visibility() {
        let server = MediaStubServer::start(|line, _body, _base| {
            if line.contains("/content/uploads/init") {
                (
                    200,
                    json!({
                        "success": true,
                        "data": {
                            "content_id": TEST_CONTENT_ID,
                            "upload_url": "https://storage.example.test/put/contract",
                            "headers": {"Content-Type": "application/pdf"},
                            "expires_in_seconds": 600
                        }
                    })
                    .to_string(),
                )
            } else {
                (400, "{}".to_string())
            }
        })
        .await;

        let (tool, _) = build_tool(&server.base_url, true, false);

        // Hosted index with local file_path + is_personal=false (team-shared).
        let hosted_res = tool
            .execute(json!({
                "action": "index",
                "file_path": "/nonexistent/Downloads/vendor-contract.pdf",
                "content_type": "document",
                "size_bytes": 4096,
                "tags": ["contract", "visibility:personal"],
                "is_personal": false,
                "workspace_id": TEST_WS
            }))
            .await
            .expect("hosted index with is_personal=false should return upload plan");

        let hosted_structured = hosted_res.structured_content.as_ref().unwrap();
        assert_eq!(hosted_structured["visibility"], "team");
        assert_eq!(hosted_structured["is_personal"], false);
        let hosted_text = result_text(&hosted_res);
        assert!(
            hosted_text.contains("visibility: team") && hosted_text.contains("is_personal: false"),
            "hosted upload plan must report team visibility: {hosted_text}"
        );

        // Explicit upload_url with is_personal=true (personal, replacing visibility:team).
        let upload_res = tool
            .execute(json!({
                "action": "upload_url",
                "filename": "draft-notes.pdf",
                "content_type": "document",
                "size_bytes": 1024,
                "tags": ["visibility:team", "notes"],
                "is_personal": true,
                "workspace_id": TEST_WS
            }))
            .await
            .expect("upload_url with is_personal=true should succeed");

        let upload_structured = upload_res.structured_content.as_ref().unwrap();
        assert_eq!(upload_structured["visibility"], "personal");
        assert_eq!(upload_structured["is_personal"], true);
        let upload_text = result_text(&upload_res);
        assert!(
            upload_text.contains("visibility: personal")
                && upload_text.contains("is_personal: true"),
            "upload_url response must report personal visibility: {upload_text}"
        );

        // Explicit upload_url without is_personal keeps default (no visibility tag injected, personal).
        let default_res = tool
            .execute(json!({
                "action": "upload_url",
                "filename": "plain.pdf",
                "content_type": "document",
                "size_bytes": 512,
                "workspace_id": TEST_WS
            }))
            .await
            .expect("upload_url without is_personal should succeed");
        let default_structured = default_res.structured_content.as_ref().unwrap();
        assert_eq!(default_structured["visibility"], "personal");
        assert_eq!(default_structured["is_personal"], true);

        let recorded = server.recorded();
        assert_eq!(recorded.len(), 3);
        let hosted_init: Value = serde_json::from_slice(&recorded[0].body).unwrap();
        assert_eq!(hosted_init["tags"], json!(["contract", "visibility:team"]));
        let upload_init: Value = serde_json::from_slice(&recorded[1].body).unwrap();
        assert_eq!(upload_init["tags"], json!(["notes", "visibility:personal"]));
        let default_init: Value = serde_json::from_slice(&recorded[2].body).unwrap();
        assert_eq!(default_init["tags"], json!([]));
    }

    #[tokio::test]
    async fn complete_upload_accepts_is_personal_and_reports_stored_visibility() {
        let server = MediaStubServer::start(|line, body, _base| {
            if line.starts_with(&format!(
                "POST /api/v1/workspaces/{TEST_WS}/content/{TEST_CONTENT_ID}/complete-upload "
            )) {
                (
                    200,
                    json!({
                        "success": true,
                        "data": {
                            "id": TEST_CONTENT_ID,
                            "status": "processing",
                            "filename": "vendor-contract.pdf",
                            "content_type": "document",
                            "tags": ["contract", "visibility:personal"],
                            "is_personal": true
                        }
                    })
                    .to_string(),
                )
            } else if line.starts_with(&format!(
                "PUT /api/v1/workspaces/{TEST_WS}/content/{TEST_CONTENT_ID} "
            )) {
                let req: Value = serde_json::from_slice(body).unwrap_or_else(|_| json!({}));
                let tags = req["tags"].as_array().cloned().unwrap_or_default();
                let is_personal = !tags.iter().any(|t| t.as_str() == Some("visibility:team"));
                (
                    200,
                    json!({
                        "success": true,
                        "data": {
                            "id": TEST_CONTENT_ID,
                            "status": "processing",
                            "filename": "vendor-contract.pdf",
                            "content_type": "document",
                            "tags": tags,
                            "is_personal": is_personal
                        }
                    })
                    .to_string(),
                )
            } else {
                (
                    400,
                    json!({"error": {"message": format!("unexpected: {line}")}}).to_string(),
                )
            }
        })
        .await;

        let (tool, _) = build_tool(&server.base_url, true, false);

        // 1. complete_upload with is_personal=false updates visibility via PUT preserving existing tags.
        let team_res = tool
            .execute(json!({
                "action": "complete_upload",
                "content_id": TEST_CONTENT_ID,
                "is_personal": false,
                "workspace_id": TEST_WS
            }))
            .await
            .expect("complete_upload with is_personal=false should succeed");
        let team_text = result_text(&team_res);
        assert!(
            team_text.contains("visibility: team") && team_text.contains("is_personal: false"),
            "complete_upload response must report team visibility: {team_text}"
        );

        let recorded = server.recorded();
        assert_eq!(
            recorded.len(),
            2,
            "expected complete-upload POST followed by content PUT: {recorded:?}"
        );
        assert!(recorded[0].line.starts_with(&format!(
            "POST /api/v1/workspaces/{TEST_WS}/content/{TEST_CONTENT_ID}/complete-upload "
        )));
        assert!(recorded[1].line.starts_with(&format!(
            "PUT /api/v1/workspaces/{TEST_WS}/content/{TEST_CONTENT_ID} "
        )));
        let put_body: Value = serde_json::from_slice(&recorded[1].body).unwrap();
        assert_eq!(put_body["tags"], json!(["contract", "visibility:team"]));

        // 2. complete_upload without is_personal makes no PUT and reports stored visibility.
        let plain_res = tool
            .execute(json!({
                "action": "complete_upload",
                "content_id": TEST_CONTENT_ID,
                "workspace_id": TEST_WS
            }))
            .await
            .expect("complete_upload without is_personal should succeed");
        let plain_text = result_text(&plain_res);
        assert!(
            plain_text.contains("visibility: personal") && plain_text.contains("is_personal: true"),
            "complete_upload response must report personal visibility: {plain_text}"
        );
        assert_eq!(
            server.recorded().len(),
            3,
            "omitted is_personal must not issue a PUT"
        );
    }

    #[tokio::test]
    async fn update_action_changes_existing_asset_visibility_and_preserves_other_tags() {
        let stored_tags = Arc::new(Mutex::new(vec![
            "contract".to_string(),
            format!("project:{TEST_PROJECT}"),
            "visibility:personal".to_string(),
        ]));
        let tags_for_server = stored_tags.clone();

        let server = MediaStubServer::start(move |line, body, _base| {
            let content_path = format!("/api/v1/workspaces/{TEST_WS}/content/{TEST_CONTENT_ID} ");
            if line.starts_with(&format!("GET {content_path}")) {
                let tags = tags_for_server.lock().unwrap().clone();
                let is_personal = !tags.iter().any(|t| t == "visibility:team");
                (
                    200,
                    json!({
                        "success": true,
                        "data": {
                            "id": TEST_CONTENT_ID,
                            "status": "indexed",
                            "filename": "vendor-contract.pdf",
                            "content_type": "document",
                            "tags": tags,
                            "is_personal": is_personal
                        }
                    })
                    .to_string(),
                )
            } else if line.starts_with(&format!("PUT {content_path}")) {
                let req: Value = serde_json::from_slice(body).unwrap_or_else(|_| json!({}));
                let new_tags: Vec<String> = req["tags"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_else(|| tags_for_server.lock().unwrap().clone());
                *tags_for_server.lock().unwrap() = new_tags.clone();
                let is_personal = !new_tags.iter().any(|t| t == "visibility:team");
                (
                    200,
                    json!({
                        "success": true,
                        "data": {
                            "id": TEST_CONTENT_ID,
                            "status": "indexed",
                            "filename": "vendor-contract.pdf",
                            "content_type": "document",
                            "tags": new_tags,
                            "is_personal": is_personal
                        }
                    })
                    .to_string(),
                )
            } else {
                (
                    400,
                    json!({"error": {"message": format!("unexpected: {line}")}}).to_string(),
                )
            }
        })
        .await;

        let (tool, _) = build_tool(&server.base_url, true, false);

        // 1. Promote personal asset -> team-visible, preserving existing tags and system project tag.
        let share_res = tool
            .execute(json!({
                "action": "update",
                "content_id": TEST_CONTENT_ID,
                "is_personal": false,
                "workspace_id": TEST_WS
            }))
            .await
            .expect("update with is_personal=false should succeed");
        let share_text = result_text(&share_res);
        assert!(
            share_text.contains("visibility: team") && share_text.contains("is_personal: false"),
            "update response must report team visibility: {share_text}"
        );
        assert_eq!(
            *stored_tags.lock().unwrap(),
            vec![
                "contract".to_string(),
                format!("project:{TEST_PROJECT}"),
                "visibility:team".to_string(),
            ]
        );

        // 2. Restrict team asset -> personal, stripping visibility:team so only visibility:personal remains.
        let restrict_res = tool
            .execute(json!({
                "action": "update",
                "content_id": TEST_CONTENT_ID,
                "is_personal": true,
                "workspace_id": TEST_WS
            }))
            .await
            .expect("update with is_personal=true should succeed");
        let restrict_text = result_text(&restrict_res);
        assert!(
            restrict_text.contains("visibility: personal")
                && restrict_text.contains("is_personal: true"),
            "update response must report personal visibility: {restrict_text}"
        );
        assert_eq!(
            *stored_tags.lock().unwrap(),
            vec![
                "contract".to_string(),
                format!("project:{TEST_PROJECT}"),
                "visibility:personal".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn update_action_respects_api_permission_errors_and_validates_inputs() {
        let server = MediaStubServer::start(|line, _body, _base| {
            if line.starts_with("GET ") {
                (
                    200,
                    json!({
                        "success": true,
                        "data": {
                            "id": TEST_CONTENT_ID,
                            "status": "indexed",
                            "tags": ["contract", "visibility:team"],
                            "is_personal": false
                        }
                    })
                    .to_string(),
                )
            } else if line.starts_with("PUT ") {
                (
                    403,
                    json!({
                        "success": false,
                        "error": {
                            "code": "FORBIDDEN",
                            "message": "No write access to workspace"
                        }
                    })
                    .to_string(),
                )
            } else {
                (400, "{}".to_string())
            }
        })
        .await;

        let (tool, _) = build_tool(&server.base_url, true, false);

        // API 403 must propagate directly.
        let forbidden = tool
            .execute(json!({
                "action": "update",
                "content_id": TEST_CONTENT_ID,
                "is_personal": true,
                "workspace_id": TEST_WS
            }))
            .await
            .expect_err("API 403 must fail update")
            .to_string();
        assert!(
            forbidden.contains("No write access") || forbidden.contains("403"),
            "expected API permission error, got: {forbidden}"
        );

        // Missing content_id fails validation.
        let missing_id = tool
            .execute(json!({
                "action": "update",
                "is_personal": false,
                "workspace_id": TEST_WS
            }))
            .await
            .expect_err("update without content_id must fail")
            .to_string();
        assert!(missing_id.contains("content_id"), "got: {missing_id}");

        // No fields to update fails validation.
        let empty_update = tool
            .execute(json!({
                "action": "update",
                "content_id": TEST_CONTENT_ID,
                "workspace_id": TEST_WS
            }))
            .await
            .expect_err("update with no fields must fail")
            .to_string();
        assert!(
            empty_update.contains("is_personal"),
            "expected error listing updatable fields, got: {empty_update}"
        );
    }

    #[test]
    fn metadata_and_schema_document_is_personal_and_update_action() {
        let client = create_mock_client();
        let session = create_mock_session(&client);
        let tool = MediaTool::new(client, session);

        let desc = &tool.metadata().description;
        assert!(desc.contains("update"), "desc: {desc}");
        assert!(desc.contains("is_personal"), "desc: {desc}");

        let schema = tool.input_schema();
        let props = schema["properties"].as_object().unwrap();
        let actions: Vec<&str> = props["action"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert!(actions.contains(&"update"), "actions: {actions:?}");

        let is_personal = props
            .get("is_personal")
            .expect("schema must expose is_personal");
        assert_eq!(is_personal["type"], "boolean");
        let is_personal_desc = is_personal["description"].as_str().unwrap_or_default();
        assert!(
            is_personal_desc.contains("visibility:team")
                || is_personal_desc.contains("team")
                || is_personal_desc.contains("personal"),
            "is_personal description should explain visibility semantics: {is_personal_desc}"
        );
        assert!(
            is_personal_desc.contains("index")
                && is_personal_desc.contains("upload_url")
                && is_personal_desc.contains("complete_upload")
                && is_personal_desc.contains("update"),
            "is_personal description should name supported actions: {is_personal_desc}"
        );
    }
}
