//! Saved questions: the `questions` tool.
//!
//! A saved question is a question with fixed answers that a person asks about
//! every row of a list. Each row gets an answer and is either sure or left
//! for the person to check. With this tool an agent finds a saved question,
//! makes one ready (labels rows with the person's answers, tries it, lets it
//! answer on its own), runs it on a list, answers one row and reads the
//! results.
//!
//! A thin client of the ContextStream API ([`mcp_client::questions`]): each
//! action is one call made with the caller's own credential, and this process
//! keeps no state. What is cheap to check (the action, the ids, the enums,
//! the form of a spec, a row, a list and a mapping) is checked before
//! anything is sent; the rest is the API's to judge, and its message is
//! passed on with what it means and the call to make next. A person's own
//! call on a row is not offered here.

use async_trait::async_trait;
use mcp_client::questions::{
    refusal_code, Mapping, QUESTION_TYPES, ROLLUP_RULES, ROW_FILTERS, SHAPES, SOURCE_KINDS,
};
use mcp_client::{
    ContextStreamClient, LabeledRow, ListRow, ListSource, NewQuestion, OneRow, QuestionSpec,
    RowsPage,
};
use mcp_session::SessionManager;
use mcp_types::{
    tool::{ToolAnnotations, ToolCategory, ToolMetadata, ToolResult},
    Error, Result,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::sync::{Arc, OnceLock};
use uuid::Uuid;

use super::display_title::{one_line, quoted_body};
use crate::input_shape::enum_argument;
use crate::registry::ToolHandler;
use crate::schema::SchemaBuilder;

/// The tool's name.
pub const TOOL_NAME: &str = "questions";

/// The tool's actions.
pub(crate) const VALID_ACTIONS: &[&str] = &[
    "list",
    "get",
    "create",
    "version",
    "label",
    "sample",
    "try",
    "activate",
    "describe",
    "run",
    "status",
    "results",
    "answer",
    "cancel",
    "resume",
    "write_back",
];

/// Rows a page of results shows when the call does not say: a run's rows
/// carry the fields the question looks at, so a page stays small.
const DEFAULT_ROWS_PAGE: i64 = 25;

/// The most rows a text lists; the structured result has all of them.
const MOST_LINES: usize = 30;

/// The most characters of a row's fields a line shows.
const PREVIEW_CHARS: usize = 120;

/// Routing summary advertised as the tool description (<= 1024 chars).
pub(crate) const TOOL_DESCRIPTION: &str = "Saved questions: a question with fixed answers, asked about every row of a list (leads, tickets, contracts). Each row gets an answer and Sure or Check; a person settles the Check rows.\n\nFind one (list, get) or save one (create; version for a change). Make it ready: label rows with the answers the person gives (label; sample picks rows worth asking about), try it on them (try says in plain words whether it may answer on its own), then activate that version. Only an active question runs on a list (run; describe shows the list's columns first) or answers one row (answer). Follow a run with status, read its rows with results (which=check for the rows a person settles), and cancel, resume or write_back (a Google Sheet run, beside its list).\n\nworkspace_id defaults to the session's. Saved questions are switched on per workspace: when a call says they are not switched on here, stop and tell the person. Full reference: the `action` parameter.";

/// Complete tool reference, advertised on the `action` parameter; see
/// [`crate::schema::with_full_reference`].
const TOOL_REFERENCE: &str = "Saved questions: a question with fixed answers asked about every row of a list. Each row gets an answer and Sure (it stands) or Check (a person settles it). Every call names the workspace: workspace_id, else the session's.\n\nActions:\n- list: the saved questions you can see; archived=true for the ones put away; limit.\n- get: question_id, version (else the one that answers, else the newest): the question with its revision, that version's spec and what trying it came to, its versions, and how many rows are labeled.\n- create: name, spec, shared (default true), template (a starter template's key). It is a draft: it answers nothing until a version is tried and activated.\n- version: question_id, spec, expected_revision (the revision get shows). A change is a new version; the shape stays.\n- label: question_id, rows=[{key, fields: {<field>: <value>}, want: {<question key, or \"answer\" for the row's answer>: <answer>}}]. The answers are the person's own, never your guesses. A row labeled before takes the new label. Without rows: the labeled rows.\n- sample: question_id, rows=[{key, fields}] from the list (up to 200), pick (how many, default 25), version: the rows worth asking the person to label, spread over every kind of answer, unclear ones first.\n- try: question_id, version (else the newest): tries it on the labeled rows and says in plain words what it came to and whether it may answer on its own.\n- activate: question_id, version, expected_revision: lets a version its trial calls ready answer on its own.\n- describe: question_id, source (or rows): the list's columns with a few values each, the fields the question looks at, and the ones the list lacks (name their columns with mapping).\n- run: question_id, source (or rows), mapping: asks an active question about every row, in the background; answers with the run and an estimate (rows, credits at most, minutes at least).\n- status: question_id, run_id: how far a run is (answered, sure, to check) and its spot checks; without run_id, the question's runs.\n- results: question_id, run_id, which=all|check|sure, after (the next a page gave), limit (default 25): rows with their answer, Sure or Check and why, and the person's call.\n- answer: question_id, fields={<field>: <value>}, key, mapping: one row now, by an active question; an answer kept for the same row is read, not asked again.\n- cancel, resume: question_id, run_id: stop a run; go on with a paused or stalled one.\n- write_back: question_id, run_id, mapping, session_id (default: the session's): writes a finished Google Sheet run's answers as two columns right of its range.\n\nA spec, sent without lines (trying chooses them): {spec: 1, shape: screen|sort|rate|checklist|match, about: \"what a row is\", show: [the fields it looks at], questions: [{key, ask, type: noul (yes or no) or choice, options: [{key, means}] with one keyed \"none\"}], pair: {first: [fields], second: [fields]} (match only, without show), rollup: {rule: all|any|at_least, k} (checklist only)}.\nA source: {kind: \"rows\", rows: [{key, fields}]}, {kind: \"csv\", text, key_column}, {kind: \"audience\", audience_id}, {kind: \"sheet\", connection_id, spreadsheet_id, range, key_column}, {kind: \"excel\", connection_id, drive_id, item_id, worksheet_id, range, key_column}.\nA mapping: {<field the question looks at>: <column of the list>}.\n\nA refusal says what to do next; one that ends with a code in brackets ([NOT_ACTIVE], [NOT_READY], [NO_TEST_ROWS], ...) names why. When saved questions are not switched on for the workspace, they are not offered there yet: stop and tell the person.";

/// The form of a spec, said with a refusal of one.
const SPEC_FORM: &str = "A spec is {spec: 1, shape: screen|sort|rate|checklist|match, about, show: [fields], questions: [{key, ask, type: noul|choice, options: [{key, means}]}], pair: {first, second} (match only), rollup: {rule: all|any|at_least, k} (checklist only)}, without lines.";

/// The form of a list, said with a refusal of one.
const SOURCE_FORM: &str = "A source is {kind: \"rows\", rows: [{key, fields}]}, {kind: \"csv\", text, key_column}, {kind: \"audience\", audience_id}, {kind: \"sheet\", connection_id, spreadsheet_id, range, key_column} or {kind: \"excel\", connection_id, drive_id, item_id, worksheet_id, range, key_column}.";

/// What a 404 from these routes means when it names no record: the routes
/// are not there for this workspace.
const NOT_SWITCHED_ON: &str = "Saved questions are not switched on for this workspace yet (or this is not a workspace you are a member of). Nothing was changed. Tell the person; there is nothing to retry until they are switched on.";

/// The `questions` tool's arguments.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct QuestionsInput {
    pub action: Option<String>,
    pub workspace_id: Option<String>,
    pub question_id: Option<String>,
    pub run_id: Option<String>,
    // list
    pub archived: Option<bool>,
    pub limit: Option<i64>,
    // create / version / activate
    pub name: Option<String>,
    pub spec: Option<Value>,
    pub shared: Option<bool>,
    pub template: Option<String>,
    pub expected_revision: Option<i64>,
    pub version: Option<i64>,
    // label / sample / a list sent as it is
    pub rows: Option<Value>,
    pub pick: Option<i64>,
    // describe / run / answer / write_back
    pub source: Option<Value>,
    pub mapping: Option<Value>,
    pub fields: Option<Value>,
    pub key: Option<String>,
    // results
    pub which: Option<String>,
    pub after: Option<i64>,
    // write_back
    pub session_id: Option<String>,
}

/// The live `questions` tool.
pub struct QuestionsTool {
    client: ContextStreamClient,
    session: Arc<SessionManager>,
}

impl QuestionsTool {
    pub fn new(client: ContextStreamClient, session: Arc<SessionManager>) -> Self {
        Self { client, session }
    }

    /// The workspace a call names: `workspace_id`, else the session's. With
    /// neither, the client says that one is needed.
    async fn workspace(&self, raw: Option<&str>) -> Result<Option<Uuid>> {
        match non_empty(raw) {
            Some(raw) => Uuid::parse_str(raw).map(Some).map_err(|_| {
                Error::Validation(format!(
                    "workspace_id must be a workspace's UUID, not {:?}.",
                    one_line(raw)
                ))
            }),
            None => Ok(super::scope::read_scope_defaults(
                &self.client,
                Some(self.session.as_ref()),
            )
            .await
            .0),
        }
    }

    /// The session a write-back is recorded under: `session_id`, else the id
    /// the API assigned at init, else this server's own, as the integration
    /// tool's edits are.
    async fn host_session(&self, explicit: Option<&str>) -> Option<String> {
        let state = self.session.state().await;
        let found = [
            explicit,
            state.api_session_id.as_deref(),
            state.session_id.as_deref(),
        ]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|value| !value.is_empty())
        .map(str::to_string);
        found
    }

    async fn run_action(&self, action: &str, input: QuestionsInput) -> Result<ToolResult> {
        let workspace = self.workspace(input.workspace_id.as_deref()).await?;
        let client = &self.client;
        match action {
            "list" => {
                let result = client
                    .list_questions(workspace, input.archived.unwrap_or(false), input.limit)
                    .await
                    .map_err(|error| explain(error, None))?;
                Ok(ToolResult::with_structured(list_text(&result), result))
            }
            "get" => {
                let question = question_id(input.question_id.as_deref(), action)?;
                let version = checked_version(input.version)?;
                let result = client
                    .get_question(workspace, question, version)
                    .await
                    .map_err(|error| explain(error, Some(question)))?;
                Ok(ToolResult::with_structured(
                    question_text(&result, question),
                    result,
                ))
            }
            "create" => {
                let name = non_empty(input.name.as_deref()).ok_or_else(|| {
                    Error::Validation(
                        "name is required for create: what the person calls the question."
                            .to_string(),
                    )
                })?;
                let new = NewQuestion {
                    name: name.to_string(),
                    spec: checked_spec(input.spec.as_ref(), action)?,
                    shared: input.shared,
                    template: non_empty(input.template.as_deref()).map(str::to_string),
                };
                let result = client
                    .create_question(workspace, &new)
                    .await
                    .map_err(|error| explain(error, None))?;
                Ok(ToolResult::with_structured(
                    saved_text(&result, true),
                    result,
                ))
            }
            "version" => {
                let question = question_id(input.question_id.as_deref(), action)?;
                let spec = checked_spec(input.spec.as_ref(), action)?;
                let revision = expected_revision(input.expected_revision, action, question)?;
                let result = client
                    .add_question_version(workspace, question, &spec, revision)
                    .await
                    .map_err(|error| explain(error, Some(question)))?;
                Ok(ToolResult::with_structured(
                    saved_text(&result, false),
                    result,
                ))
            }
            "label" => {
                let question = question_id(input.question_id.as_deref(), action)?;
                match input.rows.as_ref().filter(|rows| !rows.is_null()) {
                    None => {
                        let result = client
                            .question_test_rows(workspace, question)
                            .await
                            .map_err(|error| explain(error, Some(question)))?;
                        Ok(ToolResult::with_structured(
                            test_rows_text(&result, question),
                            result,
                        ))
                    }
                    Some(rows) => {
                        let rows = labeled_rows(rows)?;
                        let result = client
                            .label_question_rows(workspace, question, &rows)
                            .await
                            .map_err(|error| explain(error, Some(question)))?;
                        Ok(ToolResult::with_structured(
                            labeled_text(&result, question),
                            result,
                        ))
                    }
                }
            }
            "sample" => {
                let question = question_id(input.question_id.as_deref(), action)?;
                let rows = match input.rows.as_ref().filter(|rows| !rows.is_null()) {
                    Some(rows) => list_rows(rows)?,
                    None => {
                        return Err(Error::Validation(
                            "rows is required for sample: [{key, fields: {<field>: <value>}}] from the list, up to 200."
                                .to_string(),
                        ))
                    }
                };
                let version = checked_version(input.version)?;
                let pick = match input.pick {
                    Some(pick) if pick < 1 => {
                        return Err(Error::Validation(format!(
                            "pick is how many rows to offer, at least 1, not {pick}."
                        )))
                    }
                    other => other,
                };
                let result = client
                    .sample_question_rows(workspace, question, &rows, version, pick)
                    .await
                    .map_err(|error| explain(error, Some(question)))?;
                Ok(ToolResult::with_structured(
                    sample_text(&result, question),
                    result,
                ))
            }
            "try" => {
                let question = question_id(input.question_id.as_deref(), action)?;
                let version = checked_version(input.version)?;
                let result = client
                    .try_question(workspace, question, version)
                    .await
                    .map_err(|error| explain(error, Some(question)))?;
                Ok(ToolResult::with_structured(
                    trial_text(&result, question),
                    result,
                ))
            }
            "activate" => {
                let question = question_id(input.question_id.as_deref(), action)?;
                let version = checked_version(input.version)?.ok_or_else(|| {
                    Error::Validation(format!(
                        "version is required for activate: the version whose trial says it is ready (questions(action=\"get\", question_id=\"{question}\") shows it)."
                    ))
                })?;
                let revision = expected_revision(input.expected_revision, action, question)?;
                let result = client
                    .activate_question(workspace, question, version, revision)
                    .await
                    .map_err(|error| explain(error, Some(question)))?;
                Ok(ToolResult::with_structured(
                    activated_text(&result, question, version),
                    result,
                ))
            }
            "describe" => {
                let question = question_id(input.question_id.as_deref(), action)?;
                let source = list_source(input.source.as_ref(), input.rows.as_ref(), action)?;
                let result = client
                    .describe_question_list(workspace, question, &source)
                    .await
                    .map_err(|error| explain(error, Some(question)))?;
                Ok(ToolResult::with_structured(
                    described_text(&result, question),
                    result,
                ))
            }
            "run" => {
                let question = question_id(input.question_id.as_deref(), action)?;
                let source = list_source(input.source.as_ref(), input.rows.as_ref(), action)?;
                let mapping = checked_mapping(input.mapping.as_ref())?;
                let result = client
                    .start_question_run(workspace, question, &source, mapping.as_ref())
                    .await
                    .map_err(|error| explain(error, Some(question)))?;
                Ok(ToolResult::with_structured(
                    started_text(&result, question),
                    result,
                ))
            }
            "status" => {
                let question = question_id(input.question_id.as_deref(), action)?;
                match optional_uuid(input.run_id.as_deref(), "run_id", RUN_ID_IS)? {
                    Some(run) => {
                        let result = client
                            .question_run(workspace, question, run)
                            .await
                            .map_err(|error| explain(error, Some(question)))?;
                        Ok(ToolResult::with_structured(
                            run_status_text(&result, question),
                            result,
                        ))
                    }
                    None => {
                        let result = client
                            .question_runs(workspace, question)
                            .await
                            .map_err(|error| explain(error, Some(question)))?;
                        Ok(ToolResult::with_structured(
                            runs_text(&result, question),
                            result,
                        ))
                    }
                }
            }
            "results" => {
                let question = question_id(input.question_id.as_deref(), action)?;
                let run = run_id(input.run_id.as_deref(), action)?;
                let which = enum_argument("which", input.which.clone(), ROW_FILTERS)
                    .map_err(Error::Validation)?;
                if let Some(after) = input.after.filter(|after| *after < 0) {
                    return Err(Error::Validation(format!(
                        "after is the `next` a page of results gave (0 or more), not {after}."
                    )));
                }
                let page = RowsPage {
                    which,
                    after: input.after,
                    limit: Some(
                        input
                            .limit
                            .filter(|limit| *limit > 0)
                            .unwrap_or(DEFAULT_ROWS_PAGE),
                    ),
                };
                let result = client
                    .question_run_rows(workspace, question, run, &page)
                    .await
                    .map_err(|error| explain(error, Some(question)))?;
                Ok(ToolResult::with_structured(
                    rows_text(&result, question, run, &page),
                    result,
                ))
            }
            "answer" => {
                let question = question_id(input.question_id.as_deref(), action)?;
                let fields = match input.fields.as_ref() {
                    Some(Value::Object(fields)) => fields.clone(),
                    _ => {
                        return Err(Error::Validation(
                            "fields is required for answer: the row, {<field>: <value>}."
                                .to_string(),
                        ))
                    }
                };
                let row = OneRow {
                    key: non_empty(input.key.as_deref()).map(str::to_string),
                    fields,
                    mapping: checked_mapping(input.mapping.as_ref())?,
                };
                let result = client
                    .answer_question_row(workspace, question, &row)
                    .await
                    .map_err(|error| explain(error, Some(question)))?;
                Ok(ToolResult::with_structured(answer_text(&result), result))
            }
            "cancel" | "resume" => {
                let question = question_id(input.question_id.as_deref(), action)?;
                let run = run_id(input.run_id.as_deref(), action)?;
                let result = if action == "cancel" {
                    client.cancel_question_run(workspace, question, run).await
                } else {
                    client.resume_question_run(workspace, question, run).await
                }
                .map_err(|error| explain(error, Some(question)))?;
                Ok(ToolResult::with_structured(
                    run_changed_text(action, &result, question, run),
                    result,
                ))
            }
            "write_back" => {
                let question = question_id(input.question_id.as_deref(), action)?;
                let run = run_id(input.run_id.as_deref(), action)?;
                let mapping = checked_mapping(input.mapping.as_ref())?;
                let session_id = self
                    .host_session(input.session_id.as_deref())
                    .await
                    .ok_or_else(|| {
                        Error::Validation(
                            "write_back needs the session id init returned, so the change to the sheet is recorded under it: pass it as session_id, or run init first."
                                .to_string(),
                        )
                    })?;
                let result = client
                    .write_back_question_run(
                        workspace,
                        question,
                        run,
                        &session_id,
                        mapping.as_ref(),
                    )
                    .await
                    .map_err(|error| explain(error, Some(question)))?;
                Ok(ToolResult::with_structured(
                    written_back_text(&result),
                    result,
                ))
            }
            other => Err(Error::Validation(format!(
                "Unknown action: {}. Use one of: {}.",
                one_line(other),
                VALID_ACTIONS.join(", ")
            ))),
        }
    }
}

#[async_trait]
impl ToolHandler for QuestionsTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let input: QuestionsInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;
        let action = enum_argument("action", input.action.clone(), VALID_ACTIONS)
            .map_err(Error::Validation)?
            .ok_or_else(|| {
                Error::Validation(format!(
                    "action is required: one of {}.",
                    VALID_ACTIONS.join(", ")
                ))
            })?;
        self.run_action(&action, input).await
    }

    fn metadata(&self) -> &ToolMetadata {
        static METADATA: OnceLock<ToolMetadata> = OnceLock::new();
        METADATA.get_or_init(|| ToolMetadata {
            name: TOOL_NAME.to_string(),
            title: "Saved Questions".to_string(),
            description: TOOL_DESCRIPTION.to_string(),
            category: ToolCategory::Ai,
            // Saves questions, labels and runs that spend credits, so a person
            // confirms; nothing it does removes anything.
            annotations: ToolAnnotations::write(),
            is_pro: false,
            required_tier: None,
        })
    }

    fn input_schema(&self) -> Value {
        SchemaBuilder::new()
            .description("Saved questions asked about every row of a list")
            .string_enum(
                "action",
                &crate::schema::with_full_reference(
                    "What to do: list, get, create, version, label, sample, try, activate, describe, run, status, results, answer, cancel, resume or write_back",
                    TOOL_REFERENCE,
                ),
                VALID_ACTIONS,
                true,
            )
            .uuid(
                "question_id",
                "The saved question (list shows ids); every action but list and create",
                false,
            )
            .uuid(
                "run_id",
                "A run of it (run and status show ids): status, results, cancel, resume, write_back",
                false,
            )
            .string("name", "create: the question's name", false)
            .object(
                "spec",
                "create, version: the question, sent without lines (see action)",
                false,
            )
            .boolean(
                "shared",
                "create: shared with the workspace (default true)",
                false,
            )
            .string(
                "template",
                "create: the starter template's key it was made from",
                false,
            )
            .integer(
                "expected_revision",
                "version, activate: the question's revision, as get shows it",
                false,
            )
            .integer(
                "version",
                "get, sample, try, activate: a version of the question",
                false,
            )
            .property(
                "rows",
                json!({
                    "type": "array",
                    "description": "label: [{key, fields, want}] with the person's answers; sample: [{key, fields}] from the list; describe, run: the list itself when there is no source",
                    "items": {"type": "object"}
                }),
                false,
            )
            .integer("pick", "sample: how many rows to offer (default 25)", false)
            .object(
                "source",
                "describe, run: the list, {kind: rows|csv|audience|sheet|excel, ...} (see action)",
                false,
            )
            .object(
                "mapping",
                "run, answer, write_back: {<field the question looks at>: <column of the list>}",
                false,
            )
            .object(
                "fields",
                "answer: the row, {<field>: <value>}",
                false,
            )
            .string("key", "answer: the row's key, to find its answer again", false)
            .string_enum(
                "which",
                "results: all rows, the ones a person checks, or the sure ones",
                ROW_FILTERS,
                false,
            )
            .integer(
                "after",
                "results: the `next` the previous page gave",
                false,
            )
            .integer(
                "limit",
                "list, results: how many (results default 25)",
                false,
            )
            .boolean(
                "archived",
                "list: the questions put away instead",
                false,
            )
            .string(
                "session_id",
                "write_back: the session the change is recorded under (default: the one init returned)",
                false,
            )
            .uuid("workspace_id", "Workspace ID. Defaults to the session's.", false)
            .build()
    }
}

/// Register the `questions` tool.
pub fn register_questions_tools(
    registry: &mut crate::registry::ToolRegistry,
    client: ContextStreamClient,
    session: Arc<SessionManager>,
) {
    registry.register(TOOL_NAME, Arc::new(QuestionsTool::new(client, session)));
}

// ============================================================================
// Input checks
// ============================================================================

const QUESTION_ID_IS: &str = "the saved question's id (questions(action=\"list\") shows it)";
const RUN_ID_IS: &str = "the run's id (run and status show it)";

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn optional_uuid(raw: Option<&str>, field: &str, what: &str) -> Result<Option<Uuid>> {
    match non_empty(raw) {
        None => Ok(None),
        Some(raw) => Uuid::parse_str(raw).map(Some).map_err(|_| {
            Error::Validation(format!(
                "{field} must be {what}: a UUID, not {:?}.",
                one_line(raw)
            ))
        }),
    }
}

fn question_id(raw: Option<&str>, action: &str) -> Result<Uuid> {
    optional_uuid(raw, "question_id", QUESTION_ID_IS)?.ok_or_else(|| {
        Error::Validation(format!(
            "question_id is required for {action}: {QUESTION_ID_IS}."
        ))
    })
}

fn run_id(raw: Option<&str>, action: &str) -> Result<Uuid> {
    optional_uuid(raw, "run_id", RUN_ID_IS)?
        .ok_or_else(|| Error::Validation(format!("run_id is required for {action}: {RUN_ID_IS}.")))
}

fn checked_version(version: Option<i64>) -> Result<Option<i64>> {
    match version {
        Some(version) if version < 1 => Err(Error::Validation(format!(
            "version is a version number of the question, 1 or more, not {version}."
        ))),
        other => Ok(other),
    }
}

fn expected_revision(revision: Option<i64>, action: &str, question: Uuid) -> Result<i64> {
    revision.ok_or_else(|| {
        Error::Validation(format!(
            "expected_revision is required for {action}: the question's revision as you read it (questions(action=\"get\", question_id=\"{question}\") shows it), so a change made since is not overwritten."
        ))
    })
}

/// A string field of `object` as one of `allowed`, written back in its
/// canonical form (`Screen` is `screen`).
fn canonical_enum(
    object: &mut Map<String, Value>,
    key: &str,
    field: &str,
    allowed: &[&str],
) -> Result<()> {
    let Some(Value::String(given)) = object.get(key) else {
        return Ok(());
    };
    if let Some(canonical) =
        enum_argument(field, Some(given.clone()), allowed).map_err(Error::Validation)?
    {
        object.insert(key.to_string(), Value::String(canonical));
    }
    Ok(())
}

/// The spec of a create or a version, in the form the API reads: without
/// lines, its enums in their canonical form, and its format filled in.
fn checked_spec(spec: Option<&Value>, action: &str) -> Result<QuestionSpec> {
    let Some(Value::Object(given)) = spec else {
        return Err(Error::Validation(format!(
            "spec is required for {action}, as an object. {SPEC_FORM}"
        )));
    };
    if given.contains_key("lines") {
        return Err(Error::Validation(
            "A question is sent without lines: they are chosen by trying it on rows the person labeled (action=\"try\"). Leave `lines` out. [LINES_NOT_SENT]"
                .to_string(),
        ));
    }
    let mut spec = given.clone();
    canonical_enum(&mut spec, "shape", "spec.shape", SHAPES)?;
    if let Some(Value::Array(questions)) = spec.get_mut("questions") {
        for (index, question) in questions.iter_mut().enumerate() {
            if let Some(question) = question.as_object_mut() {
                canonical_enum(
                    question,
                    "type",
                    &format!("spec.questions[{index}].type"),
                    QUESTION_TYPES,
                )?;
            }
        }
    }
    if let Some(Value::Object(rollup)) = spec.get_mut("rollup") {
        canonical_enum(rollup, "rule", "spec.rollup.rule", ROLLUP_RULES)?;
    }
    serde_json::from_value(Value::Object(spec))
        .map_err(|error| Error::Validation(format!("spec: {error}. {SPEC_FORM}")))
}

fn row_array<'a>(rows: &'a Value, form: &str) -> Result<&'a Vec<Value>> {
    match rows {
        Value::Array(rows) if !rows.is_empty() => Ok(rows),
        _ => Err(Error::Validation(format!(
            "rows must be a list of rows: {form}."
        ))),
    }
}

const LABELED_ROW_FORM: &str = "[{key, fields: {<field>: <value>}, want: {<question key, or \"answer\">: <the person's answer>}}]";
const LIST_ROW_FORM: &str = "[{key, fields: {<field>: <value>}}]";

/// Rows the person labeled, each in the form the API keeps.
fn labeled_rows(rows: &Value) -> Result<Vec<LabeledRow>> {
    row_array(rows, LABELED_ROW_FORM)?
        .iter()
        .enumerate()
        .map(|(index, row)| {
            serde_json::from_value(row.clone()).map_err(|error| {
                Error::Validation(format!(
                    "rows[{index}]: {error}. A labeled row is {LABELED_ROW_FORM}."
                ))
            })
        })
        .collect()
}

/// Rows of a list, each in the form the API reads.
fn list_rows(rows: &Value) -> Result<Vec<ListRow>> {
    row_array(rows, LIST_ROW_FORM)?
        .iter()
        .enumerate()
        .map(|(index, row)| {
            serde_json::from_value(row.clone()).map_err(|error| {
                Error::Validation(format!(
                    "rows[{index}]: {error}. A row of a list is {LIST_ROW_FORM}."
                ))
            })
        })
        .collect()
}

/// The list a describe or a run names: `source`, or `rows` sent as they are.
fn list_source(source: Option<&Value>, rows: Option<&Value>, action: &str) -> Result<ListSource> {
    let source = source.filter(|source| !source.is_null());
    let rows = rows.filter(|rows| !rows.is_null());
    match (source, rows) {
        (Some(_), Some(_)) => Err(Error::Validation(format!(
            "{action} takes the list once: source, or rows for rows sent as they are, not both."
        ))),
        (None, Some(rows)) => Ok(ListSource::Rows {
            rows: list_rows(rows)?,
        }),
        (None, None) => Err(Error::Validation(format!(
            "source is required for {action}: the list. {SOURCE_FORM}"
        ))),
        (Some(Value::Object(given)), None) => {
            let mut source = given.clone();
            if !source.get("kind").is_some_and(Value::is_string) {
                return Err(Error::Validation(format!(
                    "source.kind is required: one of {}. {SOURCE_FORM}",
                    SOURCE_KINDS.join(", ")
                )));
            }
            canonical_enum(&mut source, "kind", "source.kind", SOURCE_KINDS)?;
            serde_json::from_value(Value::Object(source))
                .map_err(|error| Error::Validation(format!("source: {error}. {SOURCE_FORM}")))
        }
        (Some(_), None) => Err(Error::Validation(format!(
            "source must be an object. {SOURCE_FORM}"
        ))),
    }
}

/// Which column of the list is which field, when the call names any.
fn checked_mapping(mapping: Option<&Value>) -> Result<Option<Mapping>> {
    match mapping {
        None | Some(Value::Null) => Ok(None),
        Some(mapping) => {
            let mapping: Mapping = serde_json::from_value(mapping.clone()).map_err(|error| {
                Error::Validation(format!(
                    "mapping: {error}. A mapping is {{<field the question looks at>: <column of the list>}}."
                ))
            })?;
            Ok((!mapping.is_empty()).then_some(mapping))
        }
    }
}

// ============================================================================
// API refusals in plain words
// ============================================================================

/// What an API refusal means and what to do next, after the API's own
/// message. Every error that is not the API's answer is passed on unchanged.
fn explain(error: Error, question: Option<Uuid>) -> Error {
    match error {
        Error::Http {
            status,
            message,
            code,
            source,
        } => {
            let message = match advice(status, &message, question) {
                Some(advice) => format!("{message}\n[QUESTIONS] {advice}"),
                None => message,
            };
            Error::Http {
                status,
                message,
                code,
                source,
            }
        }
        other => other,
    }
}

fn question_ref(question: Option<Uuid>) -> String {
    question
        .map(|question| format!("question_id=\"{question}\""))
        .unwrap_or_else(|| "question_id=\"<id>\"".to_string())
}

fn advice(status: u16, message: &str, question: Option<Uuid>) -> Option<String> {
    let lower = message.to_ascii_lowercase();
    let target = question_ref(question);
    match status {
        // A record that is not there is named; anything else that is not
        // found is the routes not being there for this workspace.
        404 if lower.contains("no such question") => Some(
            "No saved question with that id is here for you: questions(action=\"list\") shows the ones you can see."
                .to_string(),
        ),
        404 if lower.contains("no such version") => Some(format!(
            "That version does not exist: questions(action=\"get\", {target}) lists the versions."
        )),
        404 if lower.contains("no such run") => Some(format!(
            "No run with that id for this question: questions(action=\"status\", {target}) lists its runs."
        )),
        404 if lower.contains("no such") && !lower.contains("no such workspace") => None,
        404 => Some(NOT_SWITCHED_ON.to_string()),
        503 if message.contains(mcp_client::questions::NOT_ASKING) => Some(
            "Questions are not being answered here right now, so nothing was asked. Try again later; list, get, status and results still read."
                .to_string(),
        ),
        400 if lower.contains("not in the form") => Some(SPEC_FORM.to_string()),
        409 | 422 => match refusal_code(message) {
            Some("NOT_ACTIVE") => Some(format!(
                "Make it ready first: label rows with the person's answers (action=\"label\"), try it (questions(action=\"try\", {target})), then activate the version its trial calls ready."
            )),
            Some("NOT_READY") => Some(format!(
                "Its trial says it is not ready to answer on its own: label more rows with the person's answers (action=\"sample\" picks good ones), try it again with questions(action=\"try\", {target}), then activate."
            )),
            Some("NOT_TRIED") => Some(format!(
                "Try that version on the labeled rows first: questions(action=\"try\", {target}, version=<n>)."
            )),
            Some("NO_TEST_ROWS") => Some(format!(
                "Label rows with the person's answers first: questions(action=\"label\", {target}, rows=[{{key, fields, want}}])."
            )),
            Some("SHAPE_CHANGED") => Some(
                "A question keeps its shape: save another question for another shape (action=\"create\")."
                    .to_string(),
            ),
            None if lower.contains("changed since") => Some(format!(
                "Read it again with questions(action=\"get\", {target}): its revision is the expected_revision to send."
            )),
            _ => None,
        },
        _ => None,
    }
}

// ============================================================================
// API answers in plain words
// ============================================================================

fn text_field<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

fn int_field(value: &Value, key: &str) -> Option<i64> {
    value.get(key).and_then(Value::as_i64)
}

/// What a number the API did not send reads as.
fn unknown() -> String {
    "?".to_string()
}

fn items<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// Text cut to `most` characters, on one line.
fn cut(text: &str, most: usize) -> String {
    let line = one_line(text);
    if line.chars().count() <= most {
        return line;
    }
    let mut short: String = line.chars().take(most.saturating_sub(1)).collect();
    short.push('…');
    short
}

/// A row's fields in a few words: `company=Acme; notes=Wants a demo`.
fn preview(fields: Option<&Value>) -> String {
    let Some(fields) = fields.and_then(Value::as_object) else {
        return String::new();
    };
    let words = fields
        .iter()
        .map(|(name, value)| {
            let value = match value {
                Value::String(text) => text.clone(),
                Value::Null => String::new(),
                other => other.to_string(),
            };
            format!("{}={}", one_line(name), one_line(&value))
        })
        .collect::<Vec<_>>()
        .join("; ");
    cut(&words, PREVIEW_CHARS)
}

/// The rest of a list a text left out.
fn more_line(shown: usize, total: usize) -> Option<String> {
    (total > shown).then(|| {
        format!(
            "… and {} more (the structured result has all of them).",
            total - shown
        )
    })
}

/// One saved question on one line.
fn question_line(question: &Value) -> String {
    let id = text_field(question, &["id"]).unwrap_or("?");
    let name = text_field(question, &["name"]).map(one_line);
    let mut line = format!(
        "{} (question_id={}): {}, {}",
        name.as_deref().unwrap_or("(unnamed)"),
        one_line(id),
        text_field(question, &["shape"])
            .map(one_line)
            .unwrap_or_else(|| "?".into()),
        text_field(question, &["status"])
            .map(one_line)
            .unwrap_or_else(|| "?".into()),
    );
    if let Some(active) = int_field(question, "active_version") {
        line.push_str(&format!(", version {active} answers"));
    }
    if let Some(latest) = int_field(question, "latest_version") {
        line.push_str(&format!(", newest version {latest}"));
    }
    if let Some(revision) = int_field(question, "revision") {
        line.push_str(&format!(", revision {revision}"));
    }
    if question.get("shared").and_then(Value::as_bool) == Some(false) {
        line.push_str(", not shared");
    }
    line
}

fn list_text(result: &Value) -> String {
    let questions = items(result, "questions");
    if questions.is_empty() {
        return "[QUESTIONS] No saved questions here. Save one with questions(action=\"create\", name=\"...\", spec={...}).".to_string();
    }
    let mut lines = vec![format!(
        "[QUESTIONS] {} saved question(s). Read one with questions(action=\"get\", question_id=\"<id>\").",
        questions.len()
    )];
    lines.extend(
        questions
            .iter()
            .take(MOST_LINES)
            .map(|question| format!("- {}", question_line(question))),
    );
    lines.extend(more_line(MOST_LINES, questions.len()));
    lines.join("\n")
}

/// The trial of a version as the API keeps it: under its answering engine's
/// key, or the trial itself.
fn trial_of(trial: Option<&Value>) -> Option<&Value> {
    let trial = trial.filter(|trial| trial.is_object())?;
    if trial.get("ready").is_some() || trial.get("summary").is_some() {
        return Some(trial);
    }
    trial
        .as_object()?
        .values()
        .find(|value| value.get("ready").is_some() || value.get("summary").is_some())
}

/// What a trial lacks, in words.
fn needs_words(trial: &Value) -> Vec<String> {
    let fewest = int_field(trial, "fewest_rows");
    items(trial, "needs")
        .iter()
        .filter_map(Value::as_str)
        .map(|need| match need {
            "few_rows" => match fewest {
                Some(fewest) => format!("more labeled rows (at least {fewest})"),
                None => "more labeled rows".to_string(),
            },
            "sure_and_wrong" => "no labeled row answered sure and wrong".to_string(),
            "not_answered" => "more rows answered".to_string(),
            other => one_line(other),
        })
        .collect()
}

/// A trial in a few lines: ready or not, its summary, and what it came to.
fn trial_lines(trial: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    let ready = trial.get("ready").and_then(Value::as_bool);
    let needs = needs_words(trial);
    lines.push(match (ready, needs.is_empty()) {
        (Some(true), _) => "Trial: ready to answer on its own.".to_string(),
        (Some(false), false) => format!("Trial: not ready; it needs {}.", needs.join(", ")),
        (Some(false), true) => "Trial: not ready.".to_string(),
        (None, _) => "Trial: tried.".to_string(),
    });
    if let Some(summary) = text_field(trial, &["summary"]) {
        for line in quoted_body(summary).lines() {
            lines.push(format!("  | {line}"));
        }
    }
    let disagreed = items(trial, "disagreed").len();
    if disagreed > 0 {
        lines.push(format!(
            "It answered otherwise than the person on {disagreed} labeled answer(s); the structured result lists them."
        ));
    }
    lines
}

fn question_text(result: &Value, question: Uuid) -> String {
    let saved = result.get("question").unwrap_or(&Value::Null);
    let mut lines = vec![format!("[QUESTIONS] {}", question_line(saved))];
    let labeled = int_field(result, "test_rows");
    if let Some(labeled) = labeled {
        lines.push(format!("Labeled rows: {labeled}."));
    }
    let version = result.get("version").unwrap_or(&Value::Null);
    let number = int_field(version, "version");
    if let Some(spec) = version.get("spec").filter(|spec| spec.is_object()) {
        let mut line = format!("Version {}", number.map_or_else(unknown, |n| n.to_string()));
        if let Some(about) = text_field(spec, &["about"]) {
            line.push_str(&format!(": {}", cut(about, 200)));
        }
        let looks_at: Vec<String> = items(spec, "show")
            .iter()
            .chain(
                spec.pointer("/pair/first")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten(),
            )
            .chain(
                spec.pointer("/pair/second")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten(),
            )
            .filter_map(Value::as_str)
            .map(one_line)
            .collect();
        if !looks_at.is_empty() {
            line.push_str(&format!(". Looks at: {}", looks_at.join(", ")));
        }
        let asks: Vec<String> = items(spec, "questions")
            .iter()
            .filter_map(|question| text_field(question, &["key"]))
            .map(one_line)
            .collect();
        if !asks.is_empty() {
            line.push_str(&format!(". Asks: {}", asks.join(", ")));
        }
        line.push('.');
        lines.push(line);
    }
    let trial = trial_of(version.get("trial"));
    match trial {
        Some(trial) => lines.extend(trial_lines(trial)),
        None if number.is_some() => lines.push("Trial: not tried yet.".to_string()),
        None => {}
    }
    let versions = items(result, "versions");
    if !versions.is_empty() {
        let listed: Vec<String> = versions
            .iter()
            .filter_map(|line| {
                let number = int_field(line, "version")?;
                Some(match line.get("tried").and_then(Value::as_bool) {
                    Some(true) => format!("{number} (tried)"),
                    _ => format!("{number}"),
                })
            })
            .collect();
        lines.push(format!("Versions: {}.", listed.join(", ")));
    }
    let status = text_field(saved, &["status"]);
    let revision = int_field(saved, "revision");
    let next = match status {
        Some("active") => Some(format!(
            "Next: it answers on its own. Run it on a list with questions(action=\"run\", question_id=\"{question}\", source={{...}}), or answer one row with action=\"answer\"."
        )),
        Some("archived") => Some("It is put away; it answers nothing.".to_string()),
        _ if labeled == Some(0) => Some(format!(
            "Next: label rows with the person's answers: questions(action=\"label\", question_id=\"{question}\", rows=[{{key, fields, want}}]); action=\"sample\" picks rows worth asking about."
        )),
        _ => match (trial.and_then(|trial| trial.get("ready")).and_then(Value::as_bool), number, revision) {
            (Some(true), Some(number), Some(revision)) => Some(format!(
                "Next: questions(action=\"activate\", question_id=\"{question}\", version={number}, expected_revision={revision}) lets it answer on its own."
            )),
            (Some(false), _, _) => Some(format!(
                "Next: label more rows with the person's answers, then questions(action=\"try\", question_id=\"{question}\")."
            )),
            _ => Some(format!(
                "Next: try it on the labeled rows: questions(action=\"try\", question_id=\"{question}\")."
            )),
        },
    };
    lines.extend(next);
    lines.join("\n")
}

fn saved_text(result: &Value, created: bool) -> String {
    let saved = result.get("question").unwrap_or(&Value::Null);
    let id = text_field(saved, &["id"]).map(one_line);
    let target = id
        .as_deref()
        .map(|id| format!("question_id=\"{id}\""))
        .unwrap_or_else(|| "question_id=\"<id>\"".to_string());
    let version = result
        .pointer("/version/version")
        .and_then(Value::as_i64)
        .or_else(|| int_field(saved, "latest_version"));
    let version = version.map_or_else(unknown, |n| n.to_string());
    let mut lines = vec![format!("[QUESTIONS] {}", question_line(saved))];
    if created {
        lines.push(format!(
            "Saved as a draft (version {version}): it answers nothing until a version is tried on rows the person labeled and activated."
        ));
        lines.push(format!(
            "Next: label rows with the person's answers: questions(action=\"label\", {target}, rows=[{{key, fields, want}}]); action=\"sample\" picks rows worth asking about."
        ));
    } else {
        lines.push(format!(
            "Version {version} written; the ones before stay as they were. When the question has labeled rows it is tried on them in the background: questions(action=\"get\", {target}, version={version}) shows what it came to, or questions(action=\"try\", {target}, version={version}) tries it now."
        ));
    }
    lines.join("\n")
}

fn want_words(want: Option<&Value>) -> String {
    want.and_then(Value::as_object)
        .map(|want| {
            want.iter()
                .map(|(key, answer)| {
                    format!(
                        "{}={}",
                        one_line(key),
                        one_line(answer.as_str().unwrap_or_default())
                    )
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

fn test_rows_text(result: &Value, question: Uuid) -> String {
    let rows = items(result, "test_rows");
    if rows.is_empty() {
        return format!(
            "[QUESTIONS] No labeled rows yet. Label rows with the person's answers: questions(action=\"label\", question_id=\"{question}\", rows=[{{key, fields, want}}])."
        );
    }
    let mut lines = vec![format!("[QUESTIONS] {} labeled row(s):", rows.len())];
    lines.extend(rows.iter().take(MOST_LINES).map(|row| {
        let mut line = format!(
            "- {}: {}",
            text_field(row, &["row_key", "key"])
                .map(one_line)
                .unwrap_or_else(|| "?".into()),
            want_words(row.get("want"))
        );
        if row.get("held_out").and_then(Value::as_bool) == Some(true) {
            line.push_str(" (held out)");
        }
        if text_field(row, &["source"]) == Some("correction") {
            line.push_str(" (a person's call on a run)");
        }
        let fields = preview(row.get("fields"));
        if !fields.is_empty() {
            line.push_str(&format!(" [{fields}]"));
        }
        line
    }));
    lines.extend(more_line(MOST_LINES, rows.len()));
    lines.join("\n")
}

fn labeled_text(result: &Value, question: Uuid) -> String {
    format!(
        "[QUESTIONS] Labeled rows kept: {} added, {} relabeled; the question has {} labeled row(s).\nNext: questions(action=\"try\", question_id=\"{question}\") tries it on them.",
        int_field(result, "added").unwrap_or(0),
        int_field(result, "relabeled").unwrap_or(0),
        int_field(result, "test_rows").map_or_else(unknown, |n| n.to_string()),
    )
}

fn sample_text(result: &Value, question: Uuid) -> String {
    let rows = items(result, "rows");
    let mut lines = vec![format!(
        "[QUESTIONS] Asked about {} row(s), {} answered. Ask the person to label these {}, then send their answers with questions(action=\"label\", question_id=\"{question}\", rows=[{{key, fields, want}}]):",
        int_field(result, "asked").map_or_else(unknown, |n| n.to_string()),
        int_field(result, "answered").map_or_else(unknown, |n| n.to_string()),
        rows.len()
    )];
    lines.extend(rows.iter().take(MOST_LINES).map(|row| {
        format!(
            "- {}: {}",
            text_field(row, &["key"])
                .map(one_line)
                .unwrap_or_else(|| "?".into()),
            preview(row.get("fields"))
        )
    }));
    lines.extend(more_line(MOST_LINES, rows.len()));
    lines.join("\n")
}

fn trial_text(result: &Value, question: Uuid) -> String {
    let saved = result.get("question").unwrap_or(&Value::Null);
    let version = int_field(result, "version");
    let name = text_field(saved, &["name"])
        .map(|name| format!("\"{}\"", one_line(name)))
        .unwrap_or_else(|| "the question".to_string());
    let mut lines = vec![format!(
        "[QUESTIONS] Tried version {} of {name} on its labeled rows.",
        version.map_or_else(unknown, |n| n.to_string())
    )];
    let trial = trial_of(result.get("trial"));
    if let Some(trial) = trial {
        lines.extend(trial_lines(trial));
    }
    let ready = trial
        .and_then(|trial| trial.get("ready"))
        .and_then(Value::as_bool);
    lines.push(match (ready, version, int_field(saved, "revision")) {
        (Some(true), Some(version), Some(revision)) => format!(
            "Next: questions(action=\"activate\", question_id=\"{question}\", version={version}, expected_revision={revision}) lets it answer on its own."
        ),
        (Some(true), _, _) => format!(
            "Next: activate it with the version and the revision questions(action=\"get\", question_id=\"{question}\") shows."
        ),
        _ => format!(
            "Next: label more rows with the person's answers (action=\"sample\" picks good ones), then try it again with questions(action=\"try\", question_id=\"{question}\")."
        ),
    });
    lines.join("\n")
}

fn activated_text(result: &Value, question: Uuid, version: i64) -> String {
    let saved = result.get("question").unwrap_or(&Value::Null);
    format!(
        "[QUESTIONS] {}\nVersion {version} answers on its own now. Run it on a list with questions(action=\"run\", question_id=\"{question}\", source={{...}}), or answer one row with action=\"answer\".",
        question_line(saved)
    )
}

fn described_text(result: &Value, question: Uuid) -> String {
    let columns = items(result, "columns");
    let mut lines = vec![format!(
        "[QUESTIONS] The list has {} row(s) and {} column(s).",
        int_field(result, "rows").map_or_else(unknown, |n| n.to_string()),
        columns.len()
    )];
    lines.extend(columns.iter().take(MOST_LINES).map(|column| {
        let samples: Vec<String> = items(column, "samples")
            .iter()
            .filter_map(Value::as_str)
            .map(|sample| cut(sample, 40))
            .collect();
        format!(
            "- {}: {}",
            text_field(column, &["name"])
                .map(one_line)
                .unwrap_or_else(|| "?".into()),
            samples.join(" | ")
        )
    }));
    lines.extend(more_line(MOST_LINES, columns.len()));
    let names = |key: &str| -> Vec<String> {
        items(result, key)
            .iter()
            .filter_map(Value::as_str)
            .map(one_line)
            .collect()
    };
    let looks_at = names("looks_at");
    if !looks_at.is_empty() {
        lines.push(format!("The question looks at: {}.", looks_at.join(", ")));
    }
    let missing = names("missing");
    if missing.is_empty() {
        lines.push(format!(
            "Every field it looks at is a column. Next: questions(action=\"run\", question_id=\"{question}\", source={{...}})."
        ));
    } else {
        lines.push(format!(
            "Not in the list: {}. Name the column that holds each with mapping={{\"<field>\": \"<column>\"}} when you run it.",
            missing.join(", ")
        ));
    }
    lines.join("\n")
}

fn credits_words(estimate: &Value) -> Option<String> {
    let credits = int_field(estimate, "credits_at_most")?;
    Some(match text_field(estimate, &["billing"]) {
        Some("shadow") => format!("at most {credits} credits, counted but not charged"),
        Some("off") => "no credits counted".to_string(),
        _ => format!("at most {credits} credits"),
    })
}

fn started_text(result: &Value, question: Uuid) -> String {
    let run = result.get("run").unwrap_or(&Value::Null);
    let id = text_field(run, &["id"]).map(one_line);
    let run_ref = id
        .as_deref()
        .map(|id| format!("run_id=\"{id}\""))
        .unwrap_or_else(|| "run_id=\"<id>\"".to_string());
    let estimate = result.get("estimate").unwrap_or(&Value::Null);
    let mut line = format!(
        "[QUESTIONS] Run {} started on {} row(s) ({}).",
        id.as_deref().unwrap_or("?"),
        int_field(estimate, "rows")
            .or_else(|| int_field(run, "rows_total"))
            .map_or_else(unknown, |n| n.to_string()),
        text_field(run, &["status"])
            .map(one_line)
            .unwrap_or_else(|| "queued".into())
    );
    let mut costs = Vec::new();
    costs.extend(credits_words(estimate));
    if let Some(minutes) = int_field(estimate, "minutes_at_least") {
        costs.push(format!("about {minutes} minute(s) or more"));
    }
    if !costs.is_empty() {
        line.push_str(&format!(" It takes {}.", costs.join(", ")));
    }
    format!(
        "{line}\nNext: follow it with questions(action=\"status\", question_id=\"{question}\", {run_ref}); questions(action=\"results\", question_id=\"{question}\", {run_ref}, which=\"check\") lists the rows a person settles."
    )
}

/// How far a run is, in one line.
fn run_counts(run: &Value) -> String {
    let count = |key: &str| int_field(run, key).unwrap_or(0);
    let mut words = format!(
        "{} of {} row(s) answered: {} sure, {} to check",
        count("rows_answered"),
        count("rows_total"),
        count("rows_sure"),
        count("rows_check"),
    );
    if count("rows_unanswered") > 0 {
        words.push_str(&format!(", {} not answered", count("rows_unanswered")));
    }
    if count("rows_reused") > 0 {
        words.push_str(&format!(", {} answered before", count("rows_reused")));
    }
    words
}

fn run_line(run: &Value) -> String {
    let mut line = format!(
        "run_id={} {}: {}",
        text_field(run, &["id"])
            .map(one_line)
            .unwrap_or_else(|| "?".into()),
        text_field(run, &["status"])
            .map(one_line)
            .unwrap_or_else(|| "?".into()),
        run_counts(run)
    );
    if let Some(version) = int_field(run, "version") {
        line.push_str(&format!("; version {version}"));
    }
    if let Some(kind) = run.pointer("/source/kind").and_then(Value::as_str) {
        line.push_str(&format!("; from {}", one_line(kind)));
    }
    if let Some(at) = text_field(run, &["created_at"]) {
        line.push_str(&format!("; started {}", one_line(at)));
    }
    line
}

fn run_status_text(result: &Value, question: Uuid) -> String {
    let run = result.get("run").unwrap_or(&Value::Null);
    let id = text_field(run, &["id"]).map(one_line);
    let run_ref = id
        .as_deref()
        .map(|id| format!("run_id=\"{id}\""))
        .unwrap_or_else(|| "run_id=\"<id>\"".to_string());
    let mut lines = vec![format!("[QUESTIONS] {}", run_line(run))];
    if let Some(credits) = int_field(run, "credits").filter(|credits| *credits > 0) {
        lines.push(format!("Credits so far: {credits}."));
    }
    if result.get("resumed").and_then(Value::as_bool) == Some(true) {
        lines.push("It had stopped; reading it set it going again.".to_string());
    }
    let checks = result.get("spot_checks").unwrap_or(&Value::Null);
    if let Some(asked) = int_field(checks, "asked").filter(|asked| *asked > 0) {
        lines.push(format!(
            "Spot checks: {asked} sure row(s) put before a person, {} called, {} answered otherwise.",
            int_field(checks, "called").unwrap_or(0),
            int_field(checks, "disagreed").unwrap_or(0)
        ));
    }
    let sheet = run.pointer("/source/kind").and_then(Value::as_str) == Some("sheet");
    lines.push(match text_field(run, &["status"]) {
        Some("queued" | "running") => format!(
            "Next: it goes on in the background; ask again with questions(action=\"status\", question_id=\"{question}\", {run_ref}). Rows answered so far are in results."
        ),
        Some("paused") => format!(
            "Next: it is paused because the credits ran out. Once there are credits again, questions(action=\"resume\", question_id=\"{question}\", {run_ref}) goes on."
        ),
        Some("done") if sheet => format!(
            "Next: questions(action=\"results\", question_id=\"{question}\", {run_ref}, which=\"check\") lists the rows a person settles; questions(action=\"write_back\", question_id=\"{question}\", {run_ref}) writes the answers beside the list in the sheet."
        ),
        Some("done") => format!(
            "Next: questions(action=\"results\", question_id=\"{question}\", {run_ref}, which=\"check\") lists the rows a person settles."
        ),
        Some("failed") => "It failed. Start a new run if the list is still to be asked.".to_string(),
        Some("cancelled") => "It was cancelled; the rows answered before stay.".to_string(),
        _ => format!(
            "Next: questions(action=\"results\", question_id=\"{question}\", {run_ref}) reads its rows."
        ),
    });
    lines.join("\n")
}

fn runs_text(result: &Value, question: Uuid) -> String {
    let runs = items(result, "runs");
    if runs.is_empty() {
        return format!(
            "[QUESTIONS] No runs of this question yet. Start one with questions(action=\"run\", question_id=\"{question}\", source={{...}})."
        );
    }
    let mut lines = vec![format!(
        "[QUESTIONS] {} run(s), newest first. Read one with questions(action=\"status\", question_id=\"{question}\", run_id=\"<id>\").",
        runs.len()
    )];
    lines.extend(
        runs.iter()
            .take(MOST_LINES)
            .map(|run| format!("- {}", run_line(run))),
    );
    lines.extend(more_line(MOST_LINES, runs.len()));
    lines.join("\n")
}

/// The answer a row came to, in words: its label, Sure or Check and why,
/// and the person's own call where there is one.
fn answer_words(answer: &Value) -> String {
    let label = text_field(answer, &["label"]).map(one_line);
    let mut words = match label {
        Some(label) => label,
        None => "no answer".to_string(),
    };
    match (
        text_field(answer, &["trust"]),
        text_field(answer, &["reason"]),
    ) {
        (Some("sure"), _) => words.push_str(" (Sure)"),
        (Some(_), Some(reason)) => {
            words.push_str(&format!(" (Check: {})", one_line(reason).replace('_', " ")))
        }
        (Some(_), None) => words.push_str(" (Check)"),
        (None, _) => {}
    }
    if let Some(called) = answer
        .get("called")
        .and_then(|called| text_field(called, &["answer"]))
    {
        words.push_str(&format!("; the person's call: {}", one_line(called)));
    }
    words
}

fn rows_text(result: &Value, question: Uuid, run: Uuid, page: &RowsPage) -> String {
    let rows = items(result, "rows");
    let which = page.which.as_deref().unwrap_or("all");
    if rows.is_empty() {
        return format!("[QUESTIONS] No rows ({which}) on this page of run {run}.");
    }
    let mut lines = vec![format!(
        "[QUESTIONS] {} row(s) of run {run} ({which}):",
        rows.len()
    )];
    lines.extend(rows.iter().map(|row| {
        let place = int_field(row, "place").map_or_else(unknown, |n| n.to_string());
        let key = text_field(row, &["key"])
            .map(one_line)
            .unwrap_or_else(|| "?".into());
        let words = match text_field(row, &["state"]) {
            Some("waiting") => "waiting".to_string(),
            _ => answer_words(row),
        };
        let mut line = format!("- #{place} {key}: {words}");
        if row.get("spot_check").and_then(Value::as_bool) == Some(true) {
            line.push_str("; spot check");
        }
        // A key made from the row's hash says nothing; its fields do.
        if key.starts_with("row-") {
            let fields = preview(row.get("fields"));
            if !fields.is_empty() {
                line.push_str(&format!(" [{fields}]"));
            }
        }
        line
    }));
    if let Some(next) = result.get("next").and_then(Value::as_i64) {
        lines.push(format!(
            "Next page: questions(action=\"results\", question_id=\"{question}\", run_id=\"{run}\", which=\"{which}\", after={next})."
        ));
    }
    lines.join("\n")
}

fn answer_text(result: &Value) -> String {
    let answer = result.get("answer").unwrap_or(&Value::Null);
    let mut lines = vec![format!(
        "[QUESTIONS] Answer (version {}): {}.",
        int_field(result, "version").map_or_else(unknown, |n| n.to_string()),
        answer_words(answer)
    )];
    let parts = items(answer, "answers");
    if parts.len() > 1 {
        let each: Vec<String> = parts
            .iter()
            .map(|part| {
                format!(
                    "{}={}",
                    text_field(part, &["key"])
                        .map(one_line)
                        .unwrap_or_else(|| "?".into()),
                    answer_words(part)
                )
            })
            .collect();
        lines.push(format!("Each question: {}.", each.join("; ")));
    }
    if text_field(answer, &["trust"]) == Some("check") {
        lines.push("A person settles a Check answer: show it to them.".to_string());
    }
    if result.get("kept_before").and_then(Value::as_bool) == Some(true) {
        lines.push(
            "This row was answered before; that answer was read, nothing was asked again."
                .to_string(),
        );
    }
    lines.join("\n")
}

fn run_changed_text(action: &str, result: &Value, question: Uuid, run: Uuid) -> String {
    let view = result.get("run").unwrap_or(&Value::Null);
    let headline = if action == "cancel" {
        "Run cancelled; the rows answered before stay.".to_string()
    } else {
        format!(
            "Run goes on. Follow it with questions(action=\"status\", question_id=\"{question}\", run_id=\"{run}\")."
        )
    };
    match view.get("id") {
        Some(_) => format!("[QUESTIONS] {headline}\n{}", run_line(view)),
        None => format!("[QUESTIONS] {headline}"),
    }
}

fn written_back_text(result: &Value) -> String {
    let range = text_field(result, &["range"])
        .map(one_line)
        .unwrap_or_else(|| "the two columns right of the list".to_string());
    let write = result.get("write").unwrap_or(&Value::Null);
    let operation = text_field(write, &["operationId"])
        .map(|id| format!(" [operation {}]", one_line(id)))
        .unwrap_or_default();
    let words = match text_field(write, &["status"]) {
        Some("succeeded") => format!("Written: the answers are in {range}."),
        Some("reconciling" | "running") => format!(
            "Sent to the sheet for {range}; it has not confirmed it yet. Read the sheet before writing again."
        ),
        Some("awaiting_approval") => format!(
            "Waiting for approval: the answers go into {range} once the person approves the change in ContextStream."
        ),
        Some("failed") => format!(
            "Not written: the sheet refused it: {}.",
            write
                .pointer("/error/message")
                .and_then(Value::as_str)
                .map(one_line)
                .unwrap_or_else(|| "no reason given".to_string())
        ),
        Some(other) => format!("{range}: {}.", one_line(other)),
        None => format!("Written back to {range}."),
    };
    format!("[QUESTIONS] {words}{operation}")
}

#[cfg(test)]
#[path = "questions_tests.rs"]
mod tests;
