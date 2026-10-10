//! Connected apps through the ContextStream integration runtime.
//!
//! Microsoft 365, Google Workspace, Dropbox and Box connect per person; Slack,
//! Notion, Linear and Figma connect per workspace. Through one runtime an
//! agent lists them (`apps`), finds an item (`find`), reads it live (`read`),
//! sees what it may do there (`capabilities`) and changes it (`edit`). The
//! server holds every grant: an edit applies only when the person allowed
//! agents to edit that app, and it comes back with a receipt.

use mcp_client::ContextStreamClient;
use mcp_session::SessionManager;
use mcp_types::{tool::ToolResult, Error, Result};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use super::IntegrationInput;

/// Providers the runtime serves, by the name people know them by.
fn app_name(provider: &str) -> &str {
    match provider {
        "microsoft" => "Microsoft 365",
        "google_workspace" => "Google Workspace",
        "dropbox" => "Dropbox",
        "box" => "Box",
        "slack" => "Slack",
        "notion" => "Notion",
        "linear" => "Linear",
        "figma" => "Figma",
        other => other,
    }
}

/// The most text a read puts in the tool's text content; the structured
/// content carries the whole result.
const READ_TEXT_LIMIT: usize = 12_000;

/// Where a runtime call runs: the workspace and project the agent works in,
/// and the host session an attach or edit is recorded under.
pub(super) struct RuntimeScope {
    pub workspace_id: Uuid,
    pub project_id: Option<Uuid>,
    pub session_id: Option<String>,
}

impl RuntimeScope {
    pub(super) async fn resolve(
        client: &ContextStreamClient,
        session: &SessionManager,
        input: &IntegrationInput,
    ) -> Result<Self> {
        let state = session.state().await;
        let workspace_id = match input.workspace_id.as_deref() {
            Some(id) => Some(parse_uuid(id, "workspace_id")?),
            None => state
                .workspace_id
                .or(client.config().await.default_workspace_id),
        }
        .ok_or_else(|| {
            Error::Validation(
                "workspace_id is required: run init first or pass workspace_id".to_string(),
            )
        })?;
        let project_id = match input.project_id.as_deref() {
            Some(id) => Some(parse_uuid(id, "project_id")?),
            None => state.project_id,
        };
        Ok(Self {
            workspace_id,
            project_id,
            session_id: host_session_id(
                input.session_id.as_deref(),
                state.api_session_id.as_deref(),
                state.session_id.as_deref(),
            ),
        })
    }

    fn body(&self, action: &str) -> Map<String, Value> {
        let mut body = Map::new();
        body.insert("action".into(), json!(action));
        body.insert("workspace_id".into(), json!(self.workspace_id));
        if let Some(project_id) = self.project_id {
            body.insert("project_id".into(), json!(project_id));
        }
        body
    }

    fn session(&self, action: &str) -> Result<&str> {
        self.session_id.as_deref().ok_or_else(|| {
            Error::Validation(format!(
                "{action} needs the session id init returned, so the change is recorded under it: pass it as session_id (init's result names it). Run init first if you have not."
            ))
        })
    }
}

/// The host session an attach or edit is recorded under: the explicit
/// `session_id`, else the id the API assigned at init (what snapshots and
/// transcripts are stored under), else the local id this server keeps. A
/// stateless client has no stored state, so only an explicit id reaches it.
fn host_session_id(
    explicit: Option<&str>,
    api_assigned: Option<&str>,
    local: Option<&str>,
) -> Option<String> {
    [explicit, api_assigned, local]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|value| !value.is_empty())
        .map(str::to_string)
}

fn parse_uuid(value: &str, field: &str) -> Result<Uuid> {
    Uuid::parse_str(value).map_err(|_| Error::Validation(format!("Invalid {field}")))
}

fn required<'a>(value: &'a Option<String>, field: &str, action: &str) -> Result<&'a str> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Error::Validation(format!("{field} is required for {action}")))
}

pub(super) async fn run(
    client: &ContextStreamClient,
    session: &SessionManager,
    input: &IntegrationInput,
    provider: &str,
    action: &str,
) -> Result<ToolResult> {
    let scope = RuntimeScope::resolve(client, session, input).await?;
    match action {
        "apps" => apps(client, &scope, provider).await,
        "find" => find(client, &scope, provider, input).await,
        "read" => read(client, &scope, input).await,
        "capabilities" => capabilities(client, &scope, input).await,
        "edit" => edit(client, &scope, input).await,
        other => Err(Error::Validation(format!("Unknown app action: {other}"))),
    }
}

async fn connections(
    client: &ContextStreamClient,
    scope: &RuntimeScope,
    provider: &str,
) -> Result<Vec<Value>> {
    let mut body = scope.body("connections");
    if provider != "all" {
        body.insert("provider".into(), json!(provider));
    }
    let result = client.integration_runtime_read(Value::Object(body)).await?;
    Ok(result["connections"]
        .as_array()
        .cloned()
        .unwrap_or_default())
}

/// What this person's agents may do through a connection here.
pub(super) fn agent_access(connection: &Value) -> &'static str {
    let policies: Vec<&str> = connection["grants"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|grant| grant["agentPolicy"].as_str())
        .collect();
    if policies.contains(&"approved_write") {
        "read and edit"
    } else if policies.contains(&"propose") {
        "read, and propose edits for approval"
    } else if policies.contains(&"read") {
        "read only"
    } else {
        "no agent access"
    }
}

async fn apps(
    client: &ContextStreamClient,
    scope: &RuntimeScope,
    provider: &str,
) -> Result<ToolResult> {
    let connections = connections(client, scope, provider).await?;
    let summaries: Vec<Value> = connections
        .iter()
        .map(|connection| {
            json!({
                "connectionId": connection["id"],
                "app": app_name(connection["providerId"].as_str().unwrap_or_default()),
                "providerId": connection["providerId"],
                "account": connection["displayName"],
                "status": connection["status"],
                "agentAccess": agent_access(connection),
            })
        })
        .collect();
    let text = if summaries.is_empty() {
        "No apps are connected here. Connect Microsoft 365, Google Workspace, Slack, Notion and others from Apps in ContextStream.".to_string()
    } else {
        let lines: Vec<String> = summaries
            .iter()
            .map(|app| {
                format!(
                    "- {} ({}): {}, {} [connection_id {}]",
                    app["app"].as_str().unwrap_or_default(),
                    app["account"].as_str().unwrap_or("account"),
                    app["status"].as_str().unwrap_or("unknown"),
                    app["agentAccess"].as_str().unwrap_or_default(),
                    app["connectionId"].as_str().unwrap_or_default(),
                )
            })
            .collect();
        format!(
            "Connected apps:\n{}\nFind an item with action \"find\", then read or edit it.",
            lines.join("\n")
        )
    };
    Ok(ToolResult::with_structured(
        text,
        json!({ "apps": summaries }),
    ))
}

async fn find(
    client: &ContextStreamClient,
    scope: &RuntimeScope,
    provider: &str,
    input: &IntegrationInput,
) -> Result<ToolResult> {
    let query = required(&input.query, "query", "find")?;
    let mut body = scope.body("search");
    body.insert("query".into(), json!(query));
    body.insert(
        "limit".into(),
        json!(input.limit.unwrap_or(10).clamp(1, 50)),
    );
    if provider != "all" {
        body.insert("provider".into(), json!(provider));
    }
    let result = client.integration_runtime_read(Value::Object(body)).await?;
    let items = result["items"].as_array().cloned().unwrap_or_default();
    let partial = result["partial"] == true;
    let mut text = if items.is_empty() {
        format!("Nothing matched \"{query}\" in your connected apps.")
    } else {
        let lines: Vec<String> = items
            .iter()
            .map(|item| {
                format!(
                    "- {} — {} [connection_id {}, resource_id {}, type {}{}]",
                    item["name"].as_str().unwrap_or("Untitled"),
                    app_name(item["providerId"].as_str().unwrap_or_default()),
                    item["connectionId"].as_str().unwrap_or_default(),
                    item["id"].as_str().unwrap_or_default(),
                    item["type"].as_str().unwrap_or("item"),
                    item["container"]
                        .as_str()
                        .map(|container| format!(", container {container}"))
                        .unwrap_or_default(),
                )
            })
            .collect();
        format!(
            "Found {} item(s):\n{}\nRead one with action \"read\" and its connection_id, resource_id, resource_type, mime_type and container.",
            items.len(),
            lines.join("\n")
        )
    };
    if partial {
        text.push_str("\nSome apps did not answer in time, so this list may be incomplete.");
    }
    Ok(ToolResult::with_structured(text, result))
}

async fn read(
    client: &ContextStreamClient,
    scope: &RuntimeScope,
    input: &IntegrationInput,
) -> Result<ToolResult> {
    if let Some(reference_id) = input.reference_id.as_deref() {
        // An item the person attached in the composer, read live.
        let mut body = scope.body("read");
        body.insert(
            "reference_id".into(),
            json!(parse_uuid(reference_id, "reference_id")?),
        );
        let live = client.integration_runtime_read(Value::Object(body)).await?;
        let title = live["reference"]["title"]
            .as_str()
            .unwrap_or("The attached item")
            .to_string();
        return Ok(read_result(&title, live));
    }

    let connection_id = required(
        &input.connection_id,
        "connection_id or reference_id",
        "read",
    )?;
    let mut body = scope.body("read");
    body.insert(
        "connection_id".into(),
        json!(parse_uuid(connection_id, "connection_id")?),
    );
    if let Some(operation) = input.operation.as_deref() {
        body.insert("operation".into(), json!(operation));
        body.insert(
            "arguments".into(),
            input.arguments.clone().unwrap_or_else(|| json!({})),
        );
    } else {
        // The item reads the way it opens; what `find` said about it picks how.
        let resource_id = required(&input.resource_id, "resource_id or operation", "read")?;
        body.insert("resource_id".into(), json!(resource_id));
        let mut hint = Map::new();
        for (field, value) in [
            ("type", &input.resource_type),
            ("mime_type", &input.mime_type),
            ("container", &input.container),
        ] {
            if let Some(value) = value {
                hint.insert(field.into(), json!(value));
            }
        }
        if !hint.is_empty() {
            body.insert("resource".into(), Value::Object(hint));
        }
    }
    let live = client.integration_runtime_read(Value::Object(body)).await?;
    let label = input
        .resource_id
        .clone()
        .unwrap_or_else(|| "The item".to_string());
    Ok(read_result(&label, live))
}

/// A live read for the agent: what was read, its revision for a later edit,
/// and the content itself.
fn read_result(label: &str, live: Value) -> ToolResult {
    let operation = live["operation"].as_str().unwrap_or("read");
    let revision = live["provenance"]["nativeRevision"]
        .as_str()
        .map(|revision| {
            format!(" Revision {revision}: pass it as the expected revision when you edit.")
        })
        .unwrap_or_default();
    let content = live["result"].to_string();
    let excerpt: String = content.chars().take(READ_TEXT_LIMIT).collect();
    let more = if content.chars().count() > READ_TEXT_LIMIT {
        "\n[truncated; the structured result has the rest]"
    } else {
        ""
    };
    ToolResult::with_structured(
        format!("Read {label} with {operation}.{revision}\n{excerpt}{more}"),
        live,
    )
}

async fn capabilities(
    client: &ContextStreamClient,
    scope: &RuntimeScope,
    input: &IntegrationInput,
) -> Result<ToolResult> {
    let connection_id = parse_uuid(
        required(&input.connection_id, "connection_id", "capabilities")?,
        "connection_id",
    )?;
    let mut body = scope.body("capabilities");
    body.insert("connection_id".into(), json!(connection_id));
    let result = client.integration_runtime_read(Value::Object(body)).await?;
    Ok(capabilities_result(result))
}

/// One app's operations as the agent reads them: read and edit, each with
/// the arguments it needs, and whether edits apply directly.
pub(super) fn capabilities_result(result: Value) -> ToolResult {
    let app = app_name(result["providerId"].as_str().unwrap_or_default()).to_string();
    let operations = result["operations"].as_array().cloned().unwrap_or_default();
    let list = |kind: &str| {
        operations
            .iter()
            .filter(|operation| operation["kind"] == kind)
            .map(|operation| {
                let required: Vec<&str> = operation["argumentSchema"]["required"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                format!(
                    "{}({})",
                    operation["operation"].as_str().unwrap_or_default(),
                    required.join(", ")
                )
            })
            .collect::<Vec<_>>()
            .join("; ")
    };
    let edits = match result["edits"].as_str() {
        Some("apply") => "Edits apply directly.",
        Some("need_approval") => "Edits wait for approval in ContextStream.",
        _ => "Agents may not edit here; turn on \"Agents can edit\" in Apps.",
    };
    ToolResult::with_structured(
        format!(
            "{app}: {edits}\n  read: {}\n  edit: {}\nThe structured result has each operation's full argument schema.",
            list("read"),
            list("write")
        ),
        result,
    )
}

async fn edit(
    client: &ContextStreamClient,
    scope: &RuntimeScope,
    input: &IntegrationInput,
) -> Result<ToolResult> {
    let connection_id = parse_uuid(
        required(&input.connection_id, "connection_id", "edit")?,
        "connection_id",
    )?;
    let operation = required(&input.operation, "operation", "edit")?;
    let arguments = input
        .arguments
        .clone()
        .filter(Value::is_object)
        .ok_or_else(|| Error::Validation("arguments must be an object for edit".to_string()))?;
    let session_id = scope.session("edit")?.to_string();
    // A repeated edit with the same key is the same change, never a second one.
    let idempotency_key = input
        .idempotency_key
        .clone()
        .filter(|key| !key.trim().is_empty())
        .unwrap_or_else(|| format!("mcp-{}", Uuid::new_v4()));

    // Prepare and, when the person's grant already allows it, execute.
    let mut body = scope.body("apply");
    body.insert("session_id".into(), json!(session_id));
    body.insert("connection_id".into(), json!(connection_id));
    body.insert("operation".into(), json!(operation));
    body.insert("arguments".into(), arguments);
    if let Some(base_revisions) = input.base_revisions.clone().filter(Value::is_object) {
        body.insert("base_revisions".into(), base_revisions);
    }
    body.insert("idempotency_key".into(), json!(idempotency_key));
    let outcome = client
        .integration_runtime_action(Value::Object(body))
        .await
        .map_err(stale_revision_hint)?;
    Ok(edit_result(operation, &idempotency_key, outcome))
}

/// What happened to an edit, said so the agent knows its next step.
pub(super) fn edit_result(operation: &str, idempotency_key: &str, outcome: Value) -> ToolResult {
    let id = outcome["operationId"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let link = outcome["receipt"]["result"]["webUrl"]
        .as_str()
        .or_else(|| outcome["receipt"]["result"]["url"].as_str())
        .or_else(|| outcome["receipt"]["result"]["canonical_url"].as_str())
        .map(|url| format!(" Open it: {url}"))
        .unwrap_or_default();
    let text = match outcome["status"].as_str().unwrap_or_default() {
        "succeeded" => format!("Done: {operation} applied and confirmed.{link} [operation {id}]"),
        "reconciling" | "running" => format!(
            "Sent: {operation} went to the app, which has not confirmed it yet. Read the item to check before repeating; reuse idempotency_key {idempotency_key} if you retry. [operation {id}]"
        ),
        "awaiting_approval" => format!(
            "Waiting for approval: {operation} runs once someone approves it in ContextStream. Agents edit without asking only where \"Agents can edit\" is on in Apps. [operation {id}]"
        ),
        "awaiting_editor" => format!(
            "Waiting for the Figma editor: open the file with the ContextStream plugin to apply {operation}. [operation {id}]"
        ),
        "failed" => format!(
            "Not applied: the app refused {operation}: {}. If the item changed since you read it, read it again and edit with the new revision. [operation {id}]",
            outcome["error"]["message"].as_str().unwrap_or("no reason given")
        ),
        other => format!("{operation}: {other}. [operation {id}]"),
    };
    ToolResult::with_structured(text, outcome)
}

/// A conflict means the item or the grant moved since it was read.
fn stale_revision_hint(error: Error) -> Error {
    match error {
        Error::Http { status: 409, message, .. } => Error::Validation(format!(
            "{message}. The item or its access changed since you read it: read it again, then edit with the new revision."
        )),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_session_an_edit_is_recorded_under_prefers_what_init_returned() {
        // An explicit id wins: a stateless client has nothing else.
        assert_eq!(
            host_session_id(Some("from-init"), None, None).as_deref(),
            Some("from-init")
        );
        assert_eq!(
            host_session_id(Some(" from-init "), Some("api"), Some("local")).as_deref(),
            Some("from-init")
        );
        // Without one, the id the API assigned beats the server's local id.
        assert_eq!(
            host_session_id(None, Some("api"), Some("local")).as_deref(),
            Some("api")
        );
        assert_eq!(
            host_session_id(None, None, Some("local")).as_deref(),
            Some("local")
        );
        // Blank values count as absent.
        assert_eq!(
            host_session_id(Some("  "), Some(""), Some("local")).as_deref(),
            Some("local")
        );
        assert_eq!(host_session_id(Some(" "), None, None), None);
    }

    #[test]
    fn an_edit_with_no_session_says_where_to_get_one() {
        let scope = RuntimeScope {
            workspace_id: Uuid::nil(),
            project_id: None,
            session_id: None,
        };
        let message = scope.session("edit").unwrap_err().to_string();
        assert!(message.contains("session_id"), "{message}");
        assert!(message.contains("init"), "{message}");
    }

    #[test]
    fn agent_access_names_the_strongest_grant() {
        let connection = |policies: &[&str]| json!({"grants": policies.iter().map(|p| json!({"agentPolicy": p})).collect::<Vec<_>>()});
        assert_eq!(
            agent_access(&connection(&["read", "approved_write"])),
            "read and edit"
        );
        assert_eq!(
            agent_access(&connection(&["propose"])),
            "read, and propose edits for approval"
        );
        assert_eq!(agent_access(&connection(&["read"])), "read only");
        assert_eq!(agent_access(&connection(&["off"])), "no agent access");
        assert_eq!(agent_access(&json!({})), "no agent access");
    }

    fn text(result: &ToolResult) -> String {
        serde_json::to_value(result).unwrap()["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }

    #[test]
    fn an_edit_says_what_happened_and_what_to_do_next() {
        let done = edit_result(
            "excel.range.update",
            "key-1",
            json!({"operationId":"op-1","status":"succeeded","receipt":{"result":{"webUrl":"https://contoso.sharepoint.com/b.xlsx"}}}),
        );
        assert!(text(&done).starts_with("Done: excel.range.update"));
        assert!(text(&done).contains("https://contoso.sharepoint.com/b.xlsx"));

        let waiting = edit_result(
            "docs.document.update",
            "key-2",
            json!({"operationId":"op-2","status":"awaiting_approval"}),
        );
        assert!(text(&waiting).contains("approval"));

        let unsure = edit_result(
            "slack.message.create",
            "key-3",
            json!({"operationId":"op-3","status":"reconciling"}),
        );
        // Never invite a blind second write.
        assert!(text(&unsure).contains("key-3"));
        assert!(text(&unsure).contains("before repeating"));

        let refused = edit_result(
            "excel.range.update",
            "key-4",
            json!({"operationId":"op-4","status":"failed","error":{"message":"eTag mismatch"}}),
        );
        assert!(text(&refused).contains("eTag mismatch"));
        assert!(text(&refused).contains("read it again"));
    }

    #[test]
    fn a_conflict_tells_the_agent_to_read_again() {
        let error = stale_revision_hint(Error::Http {
            status: 409,
            message: "Connection scope or membership changed".into(),
            code: mcp_types::ErrorCode::Conflict,
            source: None,
        });
        assert!(error.to_string().contains("read it again"));
        let other = stale_revision_hint(Error::Network("down".into()));
        assert!(matches!(other, Error::Network(_)));
    }

    #[test]
    fn capabilities_name_each_operation_and_its_arguments() {
        let result = capabilities_result(json!({
            "providerId": "microsoft",
            "edits": "apply",
            "operations": [
                {"operation": "excel.workbook.read", "kind": "read", "argumentSchema": {"required": ["drive_id", "item_id"]}},
                {"operation": "excel.range.update", "kind": "write", "argumentSchema": {"required": ["drive_id", "item_id", "worksheet_id", "range", "expected_etag"]}}
            ]
        }));
        let shown = text(&result);
        assert!(shown.starts_with("Microsoft 365: Edits apply directly."));
        assert!(shown.contains("read: excel.workbook.read(drive_id, item_id)"));
        assert!(shown.contains(
            "edit: excel.range.update(drive_id, item_id, worksheet_id, range, expected_etag)"
        ));
        let locked =
            capabilities_result(json!({"providerId": "box", "edits": "none", "operations": []}));
        assert!(text(&locked).contains("Agents can edit"));
    }

    #[test]
    fn a_read_shows_its_content_and_revision() {
        let live = json!({
            "operation": "excel.workbook.read",
            "result": {"worksheets": [{"name": "Q3"}]},
            "provenance": {"nativeRevision": "\"{etag},3\""}
        });
        let result = read_result("Budget.xlsx", live);
        let shown = text(&result);
        assert!(shown.contains("Read Budget.xlsx with excel.workbook.read"));
        assert!(shown.contains("Revision"));
        assert!(shown.contains("\"Q3\""));

        let long = json!({"operation": "docs.document.read", "result": {"text": "x".repeat(READ_TEXT_LIMIT * 2)}});
        assert!(text(&read_result("Spec", long))
            .ends_with("[truncated; the structured result has the rest]"));
    }
}
