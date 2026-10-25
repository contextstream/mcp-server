//! Tests for the specialist agent tool.

use super::*;
use crate::registry::ToolHandler;
use crate::testing::TestFixtures;
use serde_json::json;

fn tool() -> AgentTool {
    AgentTool::new(ContextStreamClient::new(TestFixtures::test_config()))
}

fn catalog() -> Value {
    json!({
        "agents": [
            {"agent": {"id": "lens", "version": "0.2.0", "name": "Lens",
                       "description": "Reads an image.", "price_credits": 400},
             "enabled": true},
            {"agent": {"id": "guide", "version": "0.1.0", "name": "Repo Guide",
                       "description": "", "price_credits": 0},
             "enabled": false},
            {"agent": {"id": "legacy", "version": "0.1.0", "name": "Legacy"},
             "enabled": true}
        ]
    })
}

#[test]
fn the_tool_is_named_agent_and_lists_its_actions() {
    let tool = tool();
    let metadata = tool.metadata();
    assert_eq!(metadata.name, "agent");
    for action in VALID_ACTIONS {
        assert!(metadata.description.contains(action), "{action}");
    }
    assert!(metadata.description.len() <= 1024);
    let schema = tool.input_schema();
    let props = schema["properties"].as_object().unwrap();
    for field in [
        "action",
        "workspace_id",
        "project_id",
        "agent",
        "input",
        "accepted_price_credits",
        "wait_ms",
        "task_id",
        "limit",
    ] {
        assert!(props.contains_key(field), "{field}");
    }
    assert_eq!(schema["required"], json!(["action"]));
}

#[test]
fn an_agent_is_found_by_id_or_id_at_version() {
    let catalog = catalog();
    assert_eq!(
        find_entry(&catalog, "lens").unwrap()["agent"]["version"],
        "0.2.0"
    );
    assert!(find_entry(&catalog, "lens@0.2.0").is_some());
    assert!(find_entry(&catalog, "pdf").is_none());
}

#[test]
fn a_priced_agent_runs_only_once_its_price_is_accepted() {
    let catalog = catalog();
    let lens = find_entry(&catalog, "lens").unwrap();
    assert_eq!(
        quote_needed(lens, None),
        Some(Quote {
            price: 400,
            raised_from: None
        })
    );
    assert_eq!(
        quote_needed(lens, Some(300)),
        Some(Quote {
            price: 400,
            raised_from: Some(300)
        })
    );
    assert_eq!(quote_needed(lens, Some(400)), None);
    assert_eq!(quote_needed(lens, Some(500)), None);
    // Free and metered agents run without a quote.
    assert_eq!(
        quote_needed(find_entry(&catalog, "guide").unwrap(), None),
        None
    );
    assert_eq!(
        quote_needed(find_entry(&catalog, "legacy").unwrap(), None),
        None
    );

    let project = Uuid::nil();
    let text = quote_needed(lens, None).unwrap().text("lens", project);
    assert!(text.starts_with("Not run yet: lens costs 400 credits a run."));
    assert!(text.contains("charged only if the run succeeds"));
    assert!(text.contains("accepted_price_credits=400"));
    let raised = quote_needed(lens, Some(300)).unwrap().text("lens", project);
    assert!(raised.contains("now costs 400 credits a run, more than the 300 accepted"));
}

#[tokio::test]
async fn run_needs_an_agent_and_an_object_input() {
    let tool = tool();
    let missing = tool.execute(json!({"action": "run"})).await.unwrap_err();
    assert!(missing.to_string().contains("agent is required"));
    let not_object = tool
        .execute(json!({"action": "run", "agent": "lens", "input": ["x"]}))
        .await
        .unwrap_err();
    assert!(not_object.to_string().contains("input must be"));
    let unknown = tool.execute(json!({"action": "deploy"})).await.unwrap_err();
    assert!(unknown.to_string().contains("Unknown action"));
    let bad_task = tool
        .execute(json!({"action": "get_run", "workspace_id": Uuid::nil(), "task_id": "nope"}))
        .await
        .unwrap_err();
    assert!(bad_task.to_string().contains("Invalid task_id"));
    let bad_filter = tool
        .execute(json!({"action": "list_runs", "workspace_id": Uuid::nil(), "agent": "Lens&x=1"}))
        .await
        .unwrap_err();
    assert!(bad_filter.to_string().contains("is not an agent id"));
}

#[test]
fn an_api_envelope_is_unwrapped_once() {
    assert_eq!(
        unwrap_data(json!({"success": true, "data": {"agents": []}})),
        json!({"agents": []})
    );
    assert_eq!(unwrap_data(json!({"agents": []})), json!({"agents": []}));
    assert_eq!(unwrap_data(json!({"data": 1})), json!({"data": 1}));
}

#[test]
fn the_catalog_text_names_each_agent_its_price_and_whether_it_is_on() {
    let text = format_catalog(&catalog());
    assert!(text.starts_with("3 agent(s):"));
    assert!(text.contains("**Lens** (lens@0.2.0) — 400 credits a run, only if it succeeds; on"));
    assert!(
        text.contains("**Repo Guide** (guide@0.1.0) — free; off: an owner or admin turns it on")
    );
    assert!(text.contains("metered per model call"));
    assert!(text.contains("Reads an image."));
    assert_eq!(
        format_catalog(&json!({"agents": []})),
        "No agents are available in this workspace yet."
    );
}

#[test]
fn a_task_text_carries_its_id_status_cost_and_result() {
    let done = json!({
        "id": "t-1", "agent": "lens@0.2.0", "status": "succeeded", "done": true,
        "price_credits": 400, "charged_credits": 400,
        "output": {"status": "succeeded", "summary": "A login form.", "data": {"text": "Sign in"}}
    });
    let text = format_task(&done);
    assert!(text.starts_with("Task t-1 (lens@0.2.0): succeeded. Charged 400 credits."));
    assert!(text.contains("Summary: A login form."));
    assert!(text.contains("\"text\":\"Sign in\""));

    let declined = json!({
        "id": "t-2", "agent": "lens@0.2.0", "status": "declined", "done": true,
        "price_credits": 400,
        "output": {"status": "declined", "summary": "Not an image.",
                   "declined": {"reason": "That is a PDF"}}
    });
    let text = format_task(&declined);
    assert!(text.contains("Not charged (price 400 credits applies only on success)."));
    assert!(text.contains("Declined: That is a PDF"));

    let running = json!({
        "id": "t-3", "agent": "lens@0.2.0", "status": "running", "done": false,
        "stage": "reading_image", "price_credits": 400
    });
    let text = format_task(&running);
    assert!(text.contains("running, at reading_image. Price 400 credits if it succeeds."));
    assert!(text.contains("agent(action=\"get_run\", task_id=\"t-3\", wait_ms=20000)"));

    let list = format_task_list(&json!({"tasks": [running, done]}));
    assert!(list.starts_with("2 run(s):"));
    assert!(list.contains("lens@0.2.0 — running (task_id: t-3)"));
    assert_eq!(
        format_task_list(&json!({"tasks": []})),
        "No agent runs in this workspace yet."
    );
}

#[test]
fn agent_ids_are_checked_before_they_reach_a_query_string() {
    for ok in ["lens", "release-notes", "ci_fix", "a1"] {
        assert!(is_agent_id(ok), "{ok}");
    }
    for bad in [
        "",
        "Lens",
        "lens&x=1",
        "lens/../x",
        "-lens",
        &"a".repeat(65),
    ] {
        assert!(!is_agent_id(bad), "{bad:?}");
    }
}
