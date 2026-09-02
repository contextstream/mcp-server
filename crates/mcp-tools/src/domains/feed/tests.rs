use super::format::*;
use super::*;
use chrono::{TimeZone, Utc};
use mcp_types::Config;
use serde_json::json;

fn tool() -> FeedTool {
    let session = Arc::new(SessionManager::new(
        ContextStreamClient::new(Config::default()),
        Config::default(),
    ));
    FeedTool::new(ContextStreamClient::new(Config::default()), session)
}

fn schema_enum(schema: &Value, field: &str) -> Vec<String> {
    schema["properties"][field]["enum"]
        .as_array()
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn schema_exposes_every_action_and_parameter() {
    let tool = tool();
    let schema = tool.input_schema();
    assert_eq!(schema["required"], json!(["action"]));
    let actions = schema_enum(&schema, "action");
    for action in [
        "list", "ensure", "get", "items", "post", "follow", "unfollow", "read", "share", "unshare",
        "feedback", "curate", "sources", "ground",
    ] {
        assert!(
            actions.iter().any(|a| a == action),
            "missing action {action}"
        );
    }
    for field in [
        "workspace_id",
        "project_id",
        "feed_id",
        "kind",
        "name",
        "description",
        "topic_spec",
        "curation_settings",
        "include",
        "include_archived",
        "view",
        "cursor",
        "limit",
        "since",
        "item_id",
        "title",
        "content",
        "tags",
        "author_kind",
        "feedback_type",
        "pinned_to_sidebar",
        "muted_until",
        "digest_frequency",
        "last_read_sequence",
        "target_workspace_id",
        "target_project_id",
        "audience",
        "share_id",
        "source_workspace_id",
        "source_project_id",
        "origin",
        "source_key",
        "query",
        "expected_revision",
        "idempotency_key",
    ] {
        assert!(
            schema["properties"].get(field).is_some(),
            "schema missing {field}"
        );
    }
    assert_eq!(
        schema_enum(&schema, "view"),
        ["latest", "unread", "posts", "top"]
    );
    assert_eq!(schema_enum(&schema, "author_kind"), ["human", "agent"]);
    assert_eq!(
        schema_enum(&schema, "feedback_type"),
        ["positive", "dismiss", "hard_ignore", "not_relevant"]
    );
    let metadata = tool.metadata();
    assert_eq!(metadata.name, "feed");
    assert!(metadata.description.contains("[FEED]"));
    assert!(!metadata.annotations.read_only);
}

#[tokio::test]
async fn unknown_action_is_rejected_before_any_network_call() {
    let error = tool()
        .execute(json!({"action": "explode"}))
        .await
        .expect_err("unknown action must fail");
    let message = error.to_string();
    assert!(message.contains("Unknown action: explode"), "{message}");
    assert!(message.contains("items"), "{message}");
}

#[tokio::test]
async fn required_parameters_are_validated_per_action() {
    let tool = tool();
    let cases = [
        (
            json!({"action": "unfollow"}),
            "feed_id is required for unfollow",
        ),
        (
            json!({"action": "unshare", "feed_id": Uuid::nil()}),
            "share_id is required for unshare",
        ),
        (
            json!({"action": "feedback", "feed_id": Uuid::nil(), "item_id": Uuid::nil()}),
            "feedback_type is required for feedback",
        ),
        (
            json!({"action": "feedback", "feed_id": Uuid::nil(), "item_id": Uuid::nil(), "feedback_type": "meh"}),
            "Invalid feedback_type",
        ),
        (
            json!({"action": "sources"}),
            "feed_id is required for sources",
        ),
        (
            json!({"action": "update", "feed_id": Uuid::nil()}),
            "expected_revision is required for update",
        ),
        (
            json!({"action": "archive", "feed_id": Uuid::nil()}),
            "expected_revision is required for archive",
        ),
        (
            json!({"action": "post", "feed_id": Uuid::nil()}),
            "title is required for post",
        ),
        (
            json!({"action": "post", "feed_id": Uuid::nil(), "title": "t"}),
            "content is required for post",
        ),
        (
            json!({"action": "ground"}),
            "workspace_id is required for ground",
        ),
        (
            json!({"action": "ensure"}),
            "workspace_id is required for ensure",
        ),
        (
            json!({"action": "items", "feed_id": "not-a-uuid"}),
            "Invalid feed_id",
        ),
        (
            json!({"action": "items", "feed_id": Uuid::nil(), "view": "sideways"}),
            "Invalid view",
        ),
        (
            json!({"action": "list", "include": "mine"}),
            "Invalid include",
        ),
    ];
    for (input, expected) in cases {
        let error = tool
            .execute(input.clone())
            .await
            .expect_err(&format!("{input} must fail validation"));
        assert!(
            error.to_string().contains(expected),
            "{input}: expected {expected:?}, got {error}"
        );
    }
}

#[test]
fn input_accepts_stringified_tags() {
    let input: FeedInput = serde_json::from_value(json!({
        "action": "post",
        "tags": "[\"api\", \"auth\"]"
    }))
    .expect("stringified tags parse");
    assert_eq!(
        input.tags,
        Some(vec!["api".to_string(), "auth".to_string()])
    );
}

#[test]
fn idempotency_keys_are_generated_when_missing() {
    let generated = idempotency_key(&None);
    assert!(generated.starts_with("mcp-feed-"));
    assert_ne!(generated, idempotency_key(&None));
    assert_eq!(idempotency_key(&Some(" explicit ".to_string())), "explicit");
    assert!(idempotency_key(&Some("   ".to_string())).starts_with("mcp-feed-"));
}

#[test]
fn limits_and_cursors_are_bounded() {
    assert_eq!(page_limit(None), 20);
    assert_eq!(page_limit(Some(0)), 1);
    assert_eq!(page_limit(Some(5_000)), 100);
    assert_eq!(page_limit(Some(-3)), 20);
    assert_eq!(ground_limit(None), 5);
    assert_eq!(ground_limit(Some(99)), 10);
    assert_eq!(cursor(Some(0)), None);
    assert_eq!(cursor(Some(40)), Some(40));
    assert_eq!(cursor(Some(-1)), None);
}

#[test]
fn provenance_marks_agent_posts() {
    let provenance = post_provenance();
    assert_eq!(provenance["product"], "contextstream-mcp");
    assert_eq!(provenance["source"], "feed_tool");
}

fn sample_feed() -> Value {
    json!({
        "id": "11111111-1111-1111-1111-111111111111",
        "name": "Engineering",
        "kind": "project",
        "access": "owner",
        "status": "active",
        "revision": 3,
        "unread_count": 4,
        "item_count": 42,
        "latest_sequence": 57,
        "follow": {"following": true, "last_read_sequence": 53}
    })
}

fn sample_item(title: &str, summary: &str) -> Value {
    json!({
        "id": "22222222-2222-2222-2222-222222222222",
        "feed_id": "11111111-1111-1111-1111-111111111111",
        "sequence": 57,
        "item_kind": "decision",
        "title": title,
        "summary": summary,
        "why_it_matters": "Every service validates tokens.",
        "occurred_at": "2020-01-01T00:00:00Z",
        "unread": true
    })
}

#[test]
fn feed_lines_carry_name_kind_access_and_counts() {
    let line = feed_line(&sample_feed());
    assert_eq!(
        line,
        "[FEED] Engineering (project, owner, following) — 4 unread / 42 item(s) · feed_id=11111111-1111-1111-1111-111111111111"
    );

    let wrapped = json!({"data": {"items": [sample_feed()], "total": 1, "next_cursor": null}});
    let text = format_feed_list(&wrapped, "all");
    assert!(text.starts_with("[FEED] 1 of 1 feed(s) visible (include=all)."));
    assert!(text.contains(&line));
    assert!(text.contains("feed(action=\"items\""));
    assert!(!text.contains("next_cursor"));

    let empty = format_feed_list(&json!({"items": [], "total": 0}), "owned");
    assert!(empty.contains("feed(action=\"ensure\")"));
}

#[test]
fn item_lines_follow_title_summary_why_age_shape() {
    let item = sample_item("Auth refactor decided", "Switched to rotating JWTs.");
    let line = item_line(&item);
    assert!(line.starts_with("[FEED] Auth refactor decided — Switched to rotating JWTs. (why: Every service validates tokens.) ["));
    assert!(
        line.contains("y ago] · item_id=22222222-2222-2222-2222-222222222222 #decision · unread")
    );

    let feed_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap();
    let page = json!({"items": [item], "total": 80, "next_cursor": 20});
    let text = format_items(Some(&sample_feed()), &page, feed_id, "unread");
    assert!(text.starts_with("[FEED] Engineering (project, owner, following)"));
    assert!(text.contains("[FEED] view=unread: 1 of 80 item(s)."));
    assert!(text.contains(
        "More: feed(action=\"items\", feed_id=\"11111111-1111-1111-1111-111111111111\", view=\"unread\", cursor=20)"
    ));
    assert!(text.contains("last_read_sequence=57"));
    assert!(text.contains("feedback_type=\"positive|dismiss|not_relevant|hard_ignore\""));

    let without_feed = format_items(None, &json!({"items": []}), feed_id, "latest");
    assert!(without_feed.starts_with("[FEED] feed_id=11111111-1111-1111-1111-111111111111"));
    assert!(without_feed.contains("Nothing here yet"));
}

#[test]
fn long_text_is_truncated_and_whitespace_collapsed() {
    let long = "word ".repeat(100);
    let line = item_line(&sample_item("t", &long));
    let summary_part = line.split(" — ").nth(1).unwrap();
    let summary_only = summary_part.split(" (why:").next().unwrap();
    assert!(summary_only.chars().count() <= 200, "{summary_only}");
    assert!(summary_only.ends_with('…'));
    assert_eq!(truncate("  a   b \n c  ", 100), "a b c");
}

#[test]
fn age_labels_scale_with_elapsed_time() {
    let now = Utc.with_ymd_and_hms(2026, 9, 2, 12, 0, 0).unwrap();
    let at = |seconds: i64| age_label_at(now - chrono::Duration::seconds(seconds), now);
    assert_eq!(at(10), "just now");
    assert_eq!(at(5 * 60), "5m ago");
    assert_eq!(at(3 * 3_600), "3h ago");
    assert_eq!(at(2 * 86_400), "2d ago");
    assert_eq!(at(45 * 86_400), "1mo ago");
    assert_eq!(at(800 * 86_400), "2y ago");
    assert_eq!(age_label(None), "unknown age");
    assert_eq!(age_label(Some("garbage")), "unknown age");
}

#[test]
fn grounding_block_is_bounded_and_carries_feed_hint() {
    let items: Vec<Value> = (0..8)
        .map(|index| {
            json!({
                "feed_id": "11111111-1111-1111-1111-111111111111",
                "feed_name": "Engineering",
                "item_id": format!("3333333{index}-3333-3333-3333-333333333333"),
                "item_kind": "decision",
                "title": format!("Decision {index}"),
                "summary": "s ".repeat(200),
                "why_it_matters": "matters",
                "occurred_at": "2026-09-01T00:00:00Z",
                "rank_score": 90 - index
            })
        })
        .collect();
    let bounded = grounding_items(&json!({"data": {"items": items}}));
    assert_eq!(bounded.len(), GROUNDING_MAX_ITEMS);

    let text = format_feed_grounding(&bounded);
    assert!(
        text.chars().count() <= GROUNDING_MAX_CHARS,
        "{}",
        text.len()
    );
    assert!(text.starts_with("[FEED] Engineering: Decision 0 — "));
    assert!(
        text.contains("(feed(action=\"items\", feed_id=\"11111111-1111-1111-1111-111111111111\"))")
    );
    assert!(text.ends_with('\n'));
    assert!(text.lines().all(|line| line.starts_with("[FEED] ")));

    let workspace_id = Uuid::nil();
    let empty = format_ground(&json!({"items": []}), workspace_id);
    assert!(empty.starts_with("[FEED] No feed items ranked"));
    let ground = format_ground(&json!({"items": [bounded[0].clone()]}), workspace_id);
    assert!(ground.starts_with("[FEED] 1 grounding item(s):\n[FEED] Engineering: Decision 0"));
    assert!(!ground.ends_with('\n'));
}

#[test]
fn write_receipts_render_compactly() {
    let feed_id = Uuid::parse_str("11111111-1111-1111-1111-111111111111").unwrap();
    let post = format_post(&json!({
        "id": "22222222-2222-2222-2222-222222222222",
        "feed_id": feed_id,
        "sequence": 58,
        "title": "Findings"
    }));
    assert!(post.starts_with("[FEED] Posted \"Findings\" to feed 11111111-1111-1111-1111-111111111111 (item_id=22222222-2222-2222-2222-222222222222, sequence=58)."));

    let follow = format_follow_state(
        &json!({"following": true, "pinned_to_sidebar": true, "digest_frequency": "daily", "last_read_sequence": 9, "muted_until": "2026-10-01T00:00:00Z"}),
        feed_id,
        "Following",
    );
    assert_eq!(
        follow,
        "[FEED] Following feed 11111111-1111-1111-1111-111111111111: following=true pinned=true digest=daily last_read_sequence=9 muted_until=2026-10-01T00:00:00Z"
    );

    let feedback = format_feedback(
        &json!({"item_id": "i1", "feedback_type": "dismiss", "recorded": true}),
        feed_id,
    );
    assert!(feedback.starts_with("[FEED] Feedback dismiss on item i1"));

    let curation = format_curation(&json!({"run_id": "r1", "status": "queued"}), feed_id);
    assert!(curation.contains("Curation run r1 queued"));
    assert!(curation.contains("feed(action=\"runs\""));

    let shares = format_shares(
        &json!({"shares": [{"id": "s1", "target_workspace_name": "Ops", "audience": "everyone"}]}),
        feed_id,
    );
    assert!(shares.contains("[FEED] 1 share(s)"));
    assert!(shares.contains("[FEED] share s1 → Ops (audience=everyone)"));

    let sources = format_sources(
        &json!({"sources": [{"source_key": "w:-", "workspace_name": "Core", "origin": "excluded", "relevance_basis_points": 4200}]}),
        feed_id,
    );
    assert!(
        sources.contains("[FEED] source Core (origin=excluded, relevance=4200bp) · source_key=w:-")
    );

    let runs = format_runs(
        &json!({"runs": [{"id": "r1", "status": "failed", "trigger": "manual", "items_written": 0, "candidates_seen": 12, "error_code": "llm_timeout"}]}),
        feed_id,
    );
    assert!(runs.contains(
        "[FEED] run r1: failed (manual) — 0 written / 12 seen [unknown age] error=llm_timeout"
    ));

    let detail = format_item_detail(&json!({
        "item": sample_item("Auth refactor decided", "Switched."),
        "citations": [{"source_kind": "decision", "title": "ADR-12", "safe_reference": "memory://adr-12"}]
    }));
    assert!(detail.contains("1 citation(s):"));
    assert!(detail.contains("  - decision: ADR-12 (memory://adr-12)"));
}
