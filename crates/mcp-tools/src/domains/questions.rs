//! Saved questions: the `questions` tool.
//!
//! A saved question is a question with fixed answers that a person asks about
//! every row of a list. Each row gets an answer and is either sure or left
//! for the person to check. With this tool an agent finds a saved question
//! (or copies a starter template), makes one ready (labels rows with the
//! person's answers, tries it, lets it
//! answer on its own), runs it on a list, answers one row, reads the results
//! (page by page, or the whole run as a CSV) and writes them back where the
//! list lives (a Google Sheet, an Excel workbook or an audience's contacts).
//! A run with a document is a requirement check: each row's requirement is
//! asked about the passages that speak to it. The nightly watch asks each
//! answerer made-up rows again and checks a question whose answerer changed;
//! the agent reads it and its checks, and takes a check when the person says
//! so.
//!
//! A thin client of the ContextStream API ([`mcp_client::questions`]): each
//! action is one call made with the caller's own credential, and this process
//! keeps no state. What is cheap to check (the action, the ids, the enums,
//! the form of a spec, a row, a list and a mapping) is checked before
//! anything is sent; the rest is the API's to judge, and its message is
//! passed on with what it means and the call to make next. A person's own
//! call on a row is not offered here.
//!
//! Saved questions are switched on per workspace, for the pilot's workspaces
//! only, so the tool is listed only for a caller whose workspace has them
//! switched on (see [`gate`]). It stays callable everywhere it is registered.

#[path = "questions_gate.rs"]
pub(crate) mod gate;

use async_trait::async_trait;
use mcp_client::questions::{
    refusal_code, Mapping, DOCUMENT_KINDS, ISSUE_STATES, QUESTION_TYPES, ROLLUP_RULES, ROW_FILTERS,
    SHAPES, SOURCE_KINDS, TRIAL_MODELS,
};
use mcp_client::{
    ContextStreamClient, LabeledRow, ListRow, ListSource, NewQuestion, OneRow, QuestionSpec,
    RowsPage, RunDocument, WriteBack,
};
use mcp_session::SessionManager;
use mcp_types::{
    tool::{ToolAnnotations, ToolCategory, ToolMetadata, ToolResult},
    Error, Result,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use uuid::Uuid;

use super::display_title::{one_line, quoted_body};
use crate::input_shape::enum_argument;
use crate::registry::ToolHandler;
use crate::schema::SchemaBuilder;
pub use gate::QuestionsGate;

/// The tool's name.
pub const TOOL_NAME: &str = "questions";

/// How long a listing waits for the probe before leaving the tool out.
pub const LISTING_BUDGET: Duration = Duration::from_millis(2500);

/// How long the read after an initialize waits for the probe.
const PREFETCH_BUDGET: Duration = Duration::from_secs(10);

/// The tool's actions.
pub(crate) const VALID_ACTIONS: &[&str] = &[
    "list",
    "templates",
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
    "export",
    "answer",
    "cancel",
    "resume",
    "write_back",
    "watch",
    "checks",
    "take_check",
];

/// Rows a page of results shows when the call does not say: a run's rows
/// carry the fields the question looks at, so a page stays small.
const DEFAULT_ROWS_PAGE: i64 = 25;

/// The most rows a text lists; the structured result has all of them.
const MOST_LINES: usize = 30;

/// The most characters of a row's fields a line shows.
const PREVIEW_CHARS: usize = 120;

/// The most rows of an export its text shows; the structured result has the
/// whole CSV.
const EXPORT_ROWS_SHOWN: usize = 50;

/// The most characters of an export its text shows, whatever the rows.
const EXPORT_CHARS_SHOWN: usize = 16_000;

/// Routing summary advertised as the tool description (<= 1024 chars).
pub(crate) const TOOL_DESCRIPTION: &str = "Saved questions: a question with fixed answers, asked about every row of a list (leads, tickets, contracts). Each row gets an answer, Sure or Check; a person settles Check rows.\n\nFind one (list, get) or save one (create, from a spec or a starter template: templates; version for a change). Make it ready: label rows with the person's answers (sample picks rows to ask about), try it, then activate the ready version. An active question runs on a list (run, with document for a requirement check; describe shows columns) or answers one row (answer). Follow a run with status, read its rows with results (which=check: decisions and Sure spot checks) or as a CSV (export), cancel or resume it, and write_back its answers to the Google Sheet, Excel workbook or audience it read. Nightly watch: watch, checks, take_check (when the person says so).\n\nworkspace_id defaults to the session's. When a call says saved questions are not switched on here, stop and tell the person. Full reference: the `action` parameter.";

/// Complete tool reference, advertised on the `action` parameter; see
/// [`crate::schema::with_full_reference`].
const TOOL_REFERENCE: &str = "Saved questions: a question with fixed answers asked about every row of a list. Each row gets an answer and Sure (it stands) or Check (a person settles it). Every call names the workspace: workspace_id, else the session's.\n\nActions:\n- list: the saved questions you can see; archived=true for the ones put away; limit.\n- templates: the starter templates to copy (key, name, what a row is, the question). An unchanged copy needs fewer labeled rows before it may answer on its own.\n- get: question_id, version (else the one that answers, else the newest): the question with its revision, that version's spec and what trying it came to, its versions, and how many rows are labeled.\n- create: name, then spec, or template (a starter template's key, which templates lists) to save a copy of it; shared (default true). It is a draft: it answers nothing until a version is tried and activated.\n- version: question_id, spec, expected_revision (the revision get shows). A change is a new version; the shape stays.\n- label: question_id, rows=[{key, fields: {<field>: <value>}, want: {<question key, or \"answer\" for the row's answer>: <answer>}}]. The answers are the person's own, never your guesses. A row labeled before takes the new label. Without rows: the labeled rows.\n- sample: question_id, rows=[{key, fields}] from the list (up to 200), pick (how many, default 25), version: the rows worth asking the person to label, spread over every kind of answer, unclear ones first.\n- try: question_id, version (else the newest), and who answers: bison (the default; its trial decides whether the version may answer on its own) or mammoth, the second answerer (a version tried on it too has it answer the rows of a run bison gives no answer to). Tries it on the labeled rows and says in plain words what it came to. A question that decides about people (a job, credit, a place to live or insurance) is kept for a person: none of its rows is Sure, and each comes back as Check, about a person.\n- activate: question_id, version, expected_revision: lets a version its trial calls ready answer on its own.\n- describe: question_id, source (or rows): the list's columns with a few values each, the fields the question looks at, and the ones the list lacks (name their columns with mapping).\n- run: question_id, source (or rows), mapping, document: asks an active question about every row, in the background; answers with the run and an estimate (rows, credits at most, minutes at least). With document it is a requirement check, for a match question of one requirement field and one passage field asked as one yes-or-no question, such as a copy of the requirement-match starter template (Requirement check): each row's requirement is asked about the 3 passages of the document that speak to it best. A sure yes quotes its passage; a requirement no passage says yes to comes back as Check, not found in the document.\n- status: question_id, run_id: how far a run is (answered, sure, to check) and its spot checks; without run_id, the question's runs.\n- results: question_id, run_id, which=all|check|sure, after (the next a page gave), limit (default 25): rows with their answer, Sure or Check and why, and the person's call. which=check includes Check and unanswered rows to decide AND Sure spot checks to confirm; report both groups using the review summary for this page. A requirement check's row has its requirement, the passage its answer rests on and where that is (requirement, passage, where).\n- export: question_id, run_id: the run's rows as a CSV, done or not: row, the fields the question looks at, its answer (the person's call first), Sure or check, Why, and each question's answer when it asks more than one. The text shows the first rows; the structured result has the whole file (csv).\n- answer: question_id, fields={<field>: <value>}, key, mapping: one row now, by an active question; an answer kept for the same row is read, not asked again.\n- cancel, resume: question_id, run_id: stop a run; go on with a paused or stalled one.\n- write_back: question_id, run_id, mapping: writes a finished run's answers where its list lives. A Google Sheet or an Excel workbook gets two columns right of the list's range (the answer, and Sure, Check or Your call), recorded under session_id (default: the session's). An audience gets two properties on each contact: the answer under attribute (default: the question's name; at most 26 letters, digits or _) and Sure, Check or Your call under the same name with _sure_or_check. Any other list (rows, a CSV, Notion, Linear, a repository's issues, Jira, your mail): export it.\n- watch: the nightly watch. Each night every answerer is asked the starter templates' made-up rows again and compared with its last reference: steady, moved (what answers changed, so the active questions it answers are checked again) or a new reference, with its counts; and this workspace's checks of the last 30 days by outcome.\n- checks: question_id: the question's checks, newest first. A check asks the labeled rows again after the answerer changed, or after a person corrected enough rows, and says whether the numbers moved and why. One that finds the version sure and wrong on a labeled row holds it: it still answers, but every row is Check (held) until a person takes the check or tries the version again.\n- take_check: question_id, check_id: what a done check chose becomes the version's trial on its answerer, which ends a hold. Only when the person says so.\n\nA spec, sent without lines (trying chooses them): {spec: 1, shape: screen|sort|rate|checklist|match, about: \"what a row is\", show: [the fields it looks at], questions: [{key, ask, type: noul (yes or no) or choice, options: [{key, means}] with one keyed \"none\"}], pair: {first: [fields], second: [fields]} (match only, without show), rollup: {rule: all|any|at_least, k} (checklist only)}.\nA source: {kind: \"rows\", rows: [{key, fields}]}, {kind: \"csv\", text, key_column}, {kind: \"audience\", audience_id}, {kind: \"sheet\", connection_id, spreadsheet_id, range, key_column}, {kind: \"excel\", connection_id, drive_id, item_id, worksheet_id, range, key_column}, {kind: \"notion\", connection_id, data_source_id} (one row a page, each property a field), {kind: \"linear\", connection_id, project_id} (a project's issues: identifier, title, state, url), {kind: \"issues\", repository_id, state: open|closed|all} (a repository's issues as ContextStream keeps them in sync; open when left out), {kind: \"jira\", project_key, status} (the Jira issues ContextStream keeps in sync that you may see, newest first: key, summary, description, status, priority, type, assignee, reporter, project, labels, updated; every project's and any status when left out), {kind: \"mail\", address, folder, query, type, flagged, priority, since, unread, from} (your own mail, read with your own sign-in or key as ContextStream Mail lets you read it: address is one of your mailboxes; folder inbox (the default), archive, sent, drafts, spam, ignore, trash or id:<a folder's id>; query searches it; type is what triage calls it (customer, newsletter, ...); flagged=true or unread=true for only those; priority any, high, medium or low; since a date or an RFC 3339 time; from a sender's address or name. One row a message, keyed by its id: from, to, cc, subject, preview, received, folder, unread, flagged, priority, kind, attachment, replied; no body. A run over your mail is yours alone: nobody else sees it, and it is never written back).\nA document: {kind: \"text\", text, title} (at most 2 MiB; a form feed marks a page, a blank line a paragraph), {kind: \"google_doc\", connection_id, document_id} or {kind: \"word\", connection_id, drive_id, item_id} (a Word document in OneDrive or SharePoint).\nA mapping: {<field the question looks at>: <column of the list>}.\n\nA refusal says what to do next; one that ends with a code in brackets ([NOT_ACTIVE], [NOT_READY], [NO_TEST_ROWS], [NOT_WRITABLE], [NOT_EMPTY], ...) names why. When saved questions are not switched on for the workspace, they are not offered there yet: stop and tell the person.";

/// The form of a spec, said with a refusal of one.
const SPEC_FORM: &str = "A spec is {spec: 1, shape: screen|sort|rate|checklist|match, about, show: [fields], questions: [{key, ask, type: noul|choice, options: [{key, means}]}], pair: {first, second} (match only), rollup: {rule: all|any|at_least, k} (checklist only)}, without lines.";

/// The form of a list, said with a refusal of one.
const SOURCE_FORM: &str = "A source is {kind: \"rows\", rows: [{key, fields}]}, {kind: \"csv\", text, key_column}, {kind: \"audience\", audience_id}, {kind: \"sheet\", connection_id, spreadsheet_id, range, key_column}, {kind: \"excel\", connection_id, drive_id, item_id, worksheet_id, range, key_column}, {kind: \"notion\", connection_id, data_source_id}, {kind: \"linear\", connection_id, project_id}, {kind: \"issues\", repository_id, state: open|closed|all}, {kind: \"jira\", project_key, status} or {kind: \"mail\", address, folder, query, type, flagged, priority, since, unread, from}.";

/// The form of a requirement check's document, said with a refusal of one.
const DOCUMENT_FORM: &str = "A document is {kind: \"text\", text, title}, {kind: \"google_doc\", connection_id, document_id} or {kind: \"word\", connection_id, drive_id, item_id}.";

/// What a 404 from these routes means when it names no record: the routes
/// are not there for this workspace.
const NOT_SWITCHED_ON: &str = "Saved questions are not switched on for this workspace yet (or this is not a workspace you are a member of). Nothing was changed. Tell the person; there is nothing to retry until they are switched on.";

/// The same for an export, which an API may not have yet where saved
/// questions are switched on.
const NO_EXPORT_HERE: &str = "This API exports no runs here: saved questions are not switched on for this workspace yet, or this ContextStream API does not export runs yet. Nothing was changed. If status still reads the run, read its rows with results instead; otherwise tell the person.";

/// The same for a question's checks, which an API may not keep yet where
/// saved questions are switched on.
const NO_CHECKS_HERE: &str = "This API keeps no checks of questions here: saved questions are not switched on for this workspace yet, or this ContextStream API does not check questions again yet. Nothing was changed. questions(action=\"get\") still shows a question's trials; otherwise tell the person.";

/// The `questions` tool's arguments.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct QuestionsInput {
    pub action: Option<String>,
    pub workspace_id: Option<String>,
    pub question_id: Option<String>,
    pub run_id: Option<String>,
    // take_check
    pub check_id: Option<String>,
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
    // try
    pub model: Option<String>,
    // label / sample / a list sent as it is
    pub rows: Option<Value>,
    pub pick: Option<i64>,
    // describe / run / answer / write_back
    pub source: Option<Value>,
    pub mapping: Option<Value>,
    pub fields: Option<Value>,
    pub key: Option<String>,
    // run: a requirement check's document
    pub document: Option<Value>,
    // results
    pub which: Option<String>,
    pub after: Option<i64>,
    // write_back
    pub session_id: Option<String>,
    pub attribute: Option<String>,
}

/// The live `questions` tool.
pub struct QuestionsTool {
    client: ContextStreamClient,
    session: Arc<SessionManager>,
    gate: Arc<QuestionsGate>,
}

impl QuestionsTool {
    /// The tool, recording what its answers say of the switch in `gate`.
    pub fn new(
        client: ContextStreamClient,
        session: Arc<SessionManager>,
        gate: Arc<QuestionsGate>,
    ) -> Self {
        Self {
            client,
            session,
            gate,
        }
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

    /// The session a write-back to a sheet or a workbook is recorded under:
    /// `session_id`, else the id the API assigned at init, else this server's
    /// own, as the integration tool's edits are. None when none is known.
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

    /// Record what a call's outcome says of saved questions for the workspace
    /// it was made in, so the listing follows a switch on its next call: an
    /// answer from the API means they are switched on there, a refusal the
    /// tool reads as "not switched on" ([`NOT_SWITCHED_ON`]) means they are
    /// not. Nothing else says either: a missing record, a refusal of the
    /// caller's role, a refused input or an unreachable API change nothing.
    async fn note_offer(&self, workspace: Option<Uuid>, outcome: &Result<ToolResult>) {
        let Some(workspace_id) = workspace else {
            return;
        };
        let offer = match outcome {
            Ok(_) => gate::Offer::Offered,
            Err(Error::Http {
                status: 404,
                message,
                ..
            }) if message.contains(NOT_SWITCHED_ON) => gate::Offer::NotOffered,
            _ => return,
        };
        if let Some(identity) = super::slipstream::caller_identity(&self.client).await {
            self.gate.record(
                &gate::GateKey {
                    identity,
                    workspace_id,
                },
                offer,
            );
        }
    }

    async fn run_action(
        &self,
        action: &str,
        workspace: Option<Uuid>,
        input: QuestionsInput,
    ) -> Result<ToolResult> {
        let client = &self.client;
        match action {
            "list" => {
                let result = client
                    .list_questions(workspace, input.archived.unwrap_or(false), input.limit)
                    .await
                    .map_err(|error| explain(error, action, None, None))?;
                Ok(ToolResult::with_structured(list_text(&result), result))
            }
            "templates" => {
                let result = client
                    .list_question_templates(workspace)
                    .await
                    .map_err(|error| explain(error, action, None, None))?;
                Ok(ToolResult::with_structured(templates_text(&result), result))
            }
            "get" => {
                let question = question_id(input.question_id.as_deref(), action)?;
                let version = checked_version(input.version)?;
                let result = client
                    .get_question(workspace, question, version)
                    .await
                    .map_err(|error| explain(error, action, Some(question), None))?;
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
                let template = non_empty(input.template.as_deref()).map(str::to_string);
                // A spec sent is the question; without one, a template's key
                // makes a copy of that starter template.
                let spec = match (input.spec.as_ref().filter(|spec| !spec.is_null()), &template) {
                    (Some(_), _) => Some(checked_spec(input.spec.as_ref(), action)?),
                    (None, Some(_)) => None,
                    (None, None) => {
                        return Err(Error::Validation(format!(
                            "create needs spec, or template: a starter template's key, to save a copy of it (questions(action=\"templates\") lists them). {SPEC_FORM}"
                        )))
                    }
                };
                let new = NewQuestion {
                    name: name.to_string(),
                    spec,
                    shared: input.shared,
                    template,
                };
                let result = client
                    .create_question(workspace, &new)
                    .await
                    .map_err(|error| explain(error, action, None, None))?;
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
                    .map_err(|error| explain(error, action, Some(question), None))?;
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
                            .map_err(|error| explain(error, action, Some(question), None))?;
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
                            .map_err(|error| explain(error, action, Some(question), None))?;
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
                    .map_err(|error| explain(error, action, Some(question), None))?;
                Ok(ToolResult::with_structured(
                    sample_text(&result, question),
                    result,
                ))
            }
            "try" => {
                let question = question_id(input.question_id.as_deref(), action)?;
                let version = checked_version(input.version)?;
                let model = enum_argument("model", input.model.clone(), TRIAL_MODELS)
                    .map_err(Error::Validation)?;
                let result = client
                    .try_question(workspace, question, version, model.as_deref())
                    .await
                    .map_err(|error| explain(error, action, Some(question), None))?;
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
                    .map_err(|error| explain(error, action, Some(question), None))?;
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
                    .map_err(|error| explain(error, action, Some(question), None))?;
                Ok(ToolResult::with_structured(
                    described_text(&result, question),
                    result,
                ))
            }
            "run" => {
                let question = question_id(input.question_id.as_deref(), action)?;
                let source = list_source(input.source.as_ref(), input.rows.as_ref(), action)?;
                let mapping = checked_mapping(input.mapping.as_ref())?;
                let document = checked_document(input.document.as_ref())?;
                let result = client
                    .start_question_run(
                        workspace,
                        question,
                        &source,
                        mapping.as_ref(),
                        document.as_ref(),
                    )
                    .await
                    .map_err(|error| explain(error, action, Some(question), None))?;
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
                            .map_err(|error| explain(error, action, Some(question), Some(run)))?;
                        Ok(ToolResult::with_structured(
                            run_status_text(&result, question),
                            result,
                        ))
                    }
                    None => {
                        let result = client
                            .question_runs(workspace, question)
                            .await
                            .map_err(|error| explain(error, action, Some(question), None))?;
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
                let mut result = client
                    .question_run_rows(workspace, question, run, &page)
                    .await
                    .map_err(|error| explain(error, action, Some(question), Some(run)))?;
                if page.which.as_deref() == Some("check") {
                    let review = review_summary(items(&result, "rows"));
                    if let Some(object) = result.as_object_mut() {
                        object.insert("review".into(), review);
                    }
                }
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
                    .map_err(|error| explain(error, action, Some(question), None))?;
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
                .map_err(|error| explain(error, action, Some(question), Some(run)))?;
                Ok(ToolResult::with_structured(
                    run_changed_text(action, &result, question, run),
                    result,
                ))
            }
            "export" => {
                let question = question_id(input.question_id.as_deref(), action)?;
                let run = run_id(input.run_id.as_deref(), action)?;
                let csv = client
                    .export_question_run(workspace, question, run)
                    .await
                    .map_err(|error| explain(error, action, Some(question), Some(run)))?;
                let (text, structured) = exported(&csv, question, run);
                Ok(ToolResult::with_structured(text, structured))
            }
            "write_back" => {
                let question = question_id(input.question_id.as_deref(), action)?;
                let run = run_id(input.run_id.as_deref(), action)?;
                // A sheet or a workbook records the change under a session;
                // an audience needs none, so none known is no refusal here.
                let write = WriteBack {
                    session_id: self.host_session(input.session_id.as_deref()).await,
                    mapping: checked_mapping(input.mapping.as_ref())?,
                    attribute: non_empty(input.attribute.as_deref()).map(str::to_string),
                };
                let result = client
                    .write_back_question_run(workspace, question, run, &write)
                    .await
                    .map_err(|error| explain(error, action, Some(question), Some(run)))?;
                Ok(ToolResult::with_structured(
                    written_back_text(&result),
                    result,
                ))
            }
            "watch" => {
                let result = client
                    .question_watch(workspace)
                    .await
                    .map_err(|error| explain(error, action, None, None))?;
                Ok(ToolResult::with_structured(watch_text(&result), result))
            }
            "checks" => {
                let question = question_id(input.question_id.as_deref(), action)?;
                let result = client
                    .question_checks(workspace, question)
                    .await
                    .map_err(|error| explain(error, action, Some(question), None))?;
                Ok(ToolResult::with_structured(
                    checks_text(&result, question),
                    result,
                ))
            }
            "take_check" => {
                let question = question_id(input.question_id.as_deref(), action)?;
                let check = check_id(input.check_id.as_deref(), action)?;
                let result = client
                    .take_question_check(workspace, question, check)
                    .await
                    .map_err(|error| explain(error, action, Some(question), None))?;
                Ok(ToolResult::with_structured(
                    taken_text(&result, question, check),
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
        let workspace = self.workspace(input.workspace_id.as_deref()).await?;
        let outcome = self.run_action(&action, workspace, input).await;
        self.note_offer(workspace, &outcome).await;
        outcome
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
                    "What to do: list, templates, get, create, version, label, sample, try, activate, describe, run, status, results, export, answer, cancel, resume, write_back, watch, checks or take_check",
                    TOOL_REFERENCE,
                ),
                VALID_ACTIONS,
                true,
            )
            .uuid(
                "question_id",
                "The saved question (list shows ids); every action but list, templates, create and watch",
                false,
            )
            .uuid(
                "run_id",
                "A run of it (run and status show ids): status, results, export, cancel, resume, write_back",
                false,
            )
            .uuid(
                "check_id",
                "take_check: a check of the question (checks shows ids)",
                false,
            )
            .string("name", "create: the question's name", false)
            .object(
                "spec",
                "create (or template instead), version: the question, sent without lines (see action)",
                false,
            )
            .boolean(
                "shared",
                "create: shared with the workspace (default true)",
                false,
            )
            .string(
                "template",
                "create: a starter template's key (templates lists them); without spec, the question is a copy of it",
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
            .string_enum(
                "model",
                "try: who answers the trial: bison (the default; its trial decides whether the question answers on its own), or mammoth, the second answerer, which once tried too answers the rows of a run bison gives no answer to",
                TRIAL_MODELS,
                false,
            )
            .integer("pick", "sample: how many rows to offer (default 25)", false)
            .object(
                "source",
                "describe, run: the list, {kind: rows|csv|audience|sheet|excel|notion|linear|issues|jira|mail, ...} (see action); mail is your own, and a run over it yours alone",
                false,
            )
            .object(
                "mapping",
                "run, answer, write_back: {<field the question looks at>: <column of the list>}",
                false,
            )
            .object(
                "document",
                "run, a requirement check (a copy of the requirement-match starter template, or a question like it): the document each row's requirement is checked against, {kind: text|google_doc|word, ...} (see action)",
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
                "results: all rows; check for decisions and Sure spot checks together; sure for Sure answers",
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
                "write_back to a Google Sheet or an Excel workbook: the session the change is recorded under (default: the one init returned)",
                false,
            )
            .string(
                "attribute",
                "write_back to an audience: the contact property the answer goes under (default: the question's name; at most 26 letters, digits or _); Sure, Check or Your call goes under the same name with _sure_or_check",
                false,
            )
            .uuid("workspace_id", "Workspace ID. Defaults to the session's.", false)
            .build()
    }
}

/// Register the `questions` tool. The registry's own listing leaves it out;
/// the transports list it per caller ([`listed`]).
pub fn register_questions_tools(
    registry: &mut crate::registry::ToolRegistry,
    client: ContextStreamClient,
    session: Arc<SessionManager>,
) {
    registry.register(
        TOOL_NAME,
        Arc::new(QuestionsTool::new(client, session, QuestionsGate::global())),
    );
}

// ============================================================================
// Listing: shown only where saved questions are switched on
// ============================================================================

/// Who asks, about which workspace, for a listing: the caller's identity and
/// the workspace the listing is for (the session's, the hosted connection's,
/// and on stdio the configured default or the folder's mapping).
async fn listing_key(
    client: &ContextStreamClient,
    session: &SessionManager,
) -> Option<gate::GateKey> {
    let identity = super::slipstream::caller_identity(client).await?;
    let workspace_id = super::slipstream::listing_workspace(client, session).await?;
    Some(gate::GateKey {
        identity,
        workspace_id,
    })
}

/// Whether `questions` is listed for the current caller: only when saved
/// questions are switched on for its workspace, read within `budget` unless
/// a fresh answer is cached. Without a caller identity or a workspace it is
/// not listed, and nothing is asked.
pub async fn listed(
    gate: &QuestionsGate,
    client: &ContextStreamClient,
    session: &SessionManager,
    budget: Duration,
) -> bool {
    match listing_key(client, session).await {
        Some(key) => gate.offered(client, &key, budget).await,
        None => false,
    }
}

/// Read whether saved questions are switched on for the current caller in
/// the background, so the tools/list after an initialize finds it ready.
pub fn prefetch(
    gate: Arc<QuestionsGate>,
    client: ContextStreamClient,
    session: Arc<SessionManager>,
) {
    mcp_client::spawn_with_task_context(async move {
        listed(&gate, &client, &session, PREFETCH_BUDGET).await;
    });
}

// ============================================================================
// Input checks
// ============================================================================

const QUESTION_ID_IS: &str = "the saved question's id (questions(action=\"list\") shows it)";
const RUN_ID_IS: &str = "the run's id (run and status show it)";
const CHECK_ID_IS: &str = "the check's id (checks shows it)";

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

fn check_id(raw: Option<&str>, action: &str) -> Result<Uuid> {
    optional_uuid(raw, "check_id", CHECK_ID_IS)?.ok_or_else(|| {
        Error::Validation(format!("check_id is required for {action}: {CHECK_ID_IS}."))
    })
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
            if source.get("kind").and_then(Value::as_str) == Some("issues") {
                canonical_enum(&mut source, "state", "source.state", ISSUE_STATES)?;
            }
            serde_json::from_value(Value::Object(source))
                .map_err(|error| Error::Validation(format!("source: {error}. {SOURCE_FORM}")))
        }
        (Some(_), None) => Err(Error::Validation(format!(
            "source must be an object. {SOURCE_FORM}"
        ))),
    }
}

/// The document a run checks each requirement against, when the call names
/// one, in the form the API reads.
fn checked_document(document: Option<&Value>) -> Result<Option<RunDocument>> {
    match document {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(given)) => {
            let mut document = given.clone();
            if !document.get("kind").is_some_and(Value::is_string) {
                return Err(Error::Validation(format!(
                    "document.kind is required: one of {}. {DOCUMENT_FORM}",
                    DOCUMENT_KINDS.join(", ")
                )));
            }
            canonical_enum(&mut document, "kind", "document.kind", DOCUMENT_KINDS)?;
            serde_json::from_value(Value::Object(document))
                .map(Some)
                .map_err(|error| Error::Validation(format!("document: {error}. {DOCUMENT_FORM}")))
        }
        Some(_) => Err(Error::Validation(format!(
            "document must be an object. {DOCUMENT_FORM}"
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
/// message. Every error that is not the API's answer is passed on unchanged,
/// so are the answers this adds nothing to (402, 403 and 429 among them).
fn explain(error: Error, action: &str, question: Option<Uuid>, run: Option<Uuid>) -> Error {
    match error {
        Error::Http {
            status,
            message,
            code,
            source,
        } => {
            let message = match advice(status, &message, action, question, run) {
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

/// `field="<id>"` for a call to make next, with the id the refused call
/// named.
fn id_ref(field: &str, id: Option<Uuid>) -> String {
    match id {
        Some(id) => format!("{field}=\"{id}\""),
        None => format!("{field}=\"<id>\""),
    }
}

/// Whether a 404 is about the routes, not a record: the API's answer for a
/// path it does not have (saved questions switched off), for a workspace
/// they are not offered in, or one that names nothing at all.
fn routes_not_there(message: &str) -> bool {
    let lower = message.trim().to_ascii_lowercase();
    lower.contains("endpoint not found")
        || lower.contains("no such workspace")
        || lower == "not found"
        || lower == "http 404 error"
}

fn advice(
    status: u16,
    message: &str,
    action: &str,
    question: Option<Uuid>,
    run: Option<Uuid>,
) -> Option<String> {
    let lower = message.to_ascii_lowercase();
    let target = id_ref("question_id", question);
    let run_ref = id_ref("run_id", run);
    let export = format!("questions(action=\"export\", {target}, {run_ref})");
    match status {
        404 if routes_not_there(message) => Some(
            match action {
                "export" => NO_EXPORT_HERE,
                "checks" | "take_check" => NO_CHECKS_HERE,
                _ => NOT_SWITCHED_ON,
            }
            .to_string(),
        ),
        404 if refusal_code(message) == Some("NO_SUCH_CHECK") => Some(format!(
            "No check with that id for this question: questions(action=\"checks\", {target}) lists its checks."
        )),
        // A list of mail: the mailbox is one of the caller's own.
        404 if refusal_code(message) == Some("NO_MAILBOX") => Some(
            "Nothing was asked. address is one of your own mailboxes, as ContextStream Mail has them: ask the person which of their addresses to read."
                .to_string(),
        ),
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
        // An audience is read and written as Create Email reads it.
        404 if lower.contains("email isn't offered") || lower.contains("email is not offered") => {
            Some(if action == "write_back" {
                format!(
                    "Answers go onto an audience's contacts only where Create Email is offered to the account, and it is not offered to this one yet. Nothing was written. Get the answers as a CSV instead: {export}."
                )
            } else {
                "An audience is read only where Create Email is offered to the account, and it is not offered to this one yet. Send the list another way: rows, a CSV, a Google Sheet or an Excel workbook."
                    .to_string()
            })
        }
        404 if lower.contains("no such repository") => Some(
            "That repository is not one you can see in this workspace: vcs(action=\"list_repos\") lists the repositories it keeps in sync, with their ids."
                .to_string(),
        ),
        // Any other record the API names in its own words.
        404 => None,
        503 if message.contains(mcp_client::questions::NOT_ASKING) => Some(
            "Questions are not being answered here right now, so nothing was asked (questions about mail are switched on apart from the others). Try again later; list, get, status, results and export still read."
                .to_string(),
        ),
        503 if message.contains(mcp_client::questions::MAIL_UNAVAILABLE) => Some(
            "ContextStream Mail is not set up on this server, so mail is no list here. Nothing was asked. Send the list another way: rows, a CSV, a Google Sheet or an Excel workbook."
                .to_string(),
        ),
        // Mail would not let the caller in this way.
        403 if message.contains("[MAIL_REFUSED]") => Some(
            "Mail is read only for the person whose mail it is, with their own sign-in or key that may read it (the message says why not here). Nothing was asked. Ask the person to run it with their own credential, or send the list another way."
                .to_string(),
        ),
        400 if refusal_code(message) == Some("SESSION") => Some(
            "Writing to a Google Sheet or an Excel workbook is recorded under a session, and none is known here: pass session_id (the one init returned), or run init first, then write back again."
                .to_string(),
        ),
        400 if lower.contains("not in the form") => Some(SPEC_FORM.to_string()),
        // An API before starter templates reads `templates` as a question's id.
        400 if action == "templates" => Some(
            "This ContextStream API has no starter templates yet: save a question with its own spec instead (questions(action=\"create\", name=\"...\", spec={...}))."
                .to_string(),
        ),
        // The same for the nightly watch, read as a question's id.
        400 if action == "watch" => Some(
            "This ContextStream API keeps no nightly watch yet: there is nothing to read. Nothing was changed."
                .to_string(),
        ),
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
            Some("TEMPLATE") => Some(
                "questions(action=\"templates\") lists the starter templates and their keys."
                    .to_string(),
            ),
            Some("SPEC") => Some(format!(
                "Send the question's spec, or template with a starter template's key (questions(action=\"templates\") lists them). {SPEC_FORM}"
            )),
            Some("MODEL") => Some(
                "A trial is answered by bison (the default) or mammoth, the second answerer."
                    .to_string(),
            ),
            Some("SHAPE_CHANGED") => Some(
                "A question keeps its shape: save another question for another shape (action=\"create\")."
                    .to_string(),
            ),
            // NOT_A_SHEET is what an API said before write-back took
            // workbooks and audiences.
            // Mail is read, never changed.
            Some("NOT_WRITABLE") if lower.contains("mail") => Some(format!(
                "A run over mail is never written back: mail is read, never changed. Nothing was written. Get this run's answers as a CSV instead: {export}."
            )),
            Some("NOT_WRITABLE" | "NOT_A_SHEET") => Some(format!(
                "Only a run over a Google Sheet, an Excel workbook or an audience goes back where its list lives. Nothing was written. Get this run's answers as a CSV instead: {export}."
            )),
            Some("ATTRIBUTE") => Some(format!(
                "Name the contact property for the answers with attribute=\"...\": at most 26 letters, digits or _ (a space or - counts as _). Left out, it is the question's name, which must fit the same rule. Then questions(action=\"write_back\", {target}, {run_ref}, attribute=\"...\")."
            )),
            Some("NOT_EMPTY") if lower.contains("contact") => Some(format!(
                "Nothing was written. Write the answers under another property: questions(action=\"write_back\", {target}, {run_ref}, attribute=\"...\")."
            )),
            Some("NOT_EMPTY") => Some(
                "Nothing was written. Ask the person to clear or move what is in the two columns right of the list's range, then write back again."
                    .to_string(),
            ),
            Some("RANGE") => Some(format!(
                "This run read its list from a range that names no columns. Run the question again on a range such as Sheet1!A1:D500 and write that run back; {export} gives this run's answers as a CSV meanwhile."
            )),
            Some("SOURCE") => Some(format!(
                "The list could not be read as it is (the message says why): check the list, its range and the connected app, and try again. questions(action=\"describe\", {target}, source={{...}}) reads a list and shows its columns."
            )),
            // A requirement check.
            Some("NOT_A_MATCH") => Some(
                "Nothing was started. Only a match question that pairs one requirement field with one passage field and asks one yes-or-no question checks a document. Copy the \"requirement-match\" starter template (\"Requirement check\") to get a question a document can be checked against: questions(action=\"create\", name=\"...\", template=\"requirement-match\"), make it ready and run it with the document. Or run this question without document."
                    .to_string(),
            ),
            Some("NO_REQUIREMENT") => Some(format!(
                "Name the column of the list that holds each requirement with mapping={{\"<requirement field>\": \"<column>\"}}, then run again; questions(action=\"describe\", {target}, source={{...}}) shows the list's columns."
            )),
            // Taking a check of the nightly watch.
            Some("CHECK_NOT_DONE") => Some(format!(
                "A check is taken once, when it is done: questions(action=\"checks\", {target}) shows whether it is still running or was taken already."
            )),
            Some("CHECK_NOT_READY") => Some(format!(
                "Nothing was changed. Label more rows with the person's answers or write a reworded version (action=\"version\"), then try it again with questions(action=\"try\", {target}): a new trial ends a hold too."
            )),
            Some("CHECK_STALE") => Some(format!(
                "Nothing was changed: the version's newer trial stands. questions(action=\"get\", {target}) shows it."
            )),
            None if lower.contains("changed since") => Some(format!(
                "Read it again with questions(action=\"get\", {target}): its revision is the expected_revision to send."
            )),
            None if lower.contains("once it is over") => Some(format!(
                "The run is still going: follow it with questions(action=\"status\", {target}, {run_ref}) and write it back once it is done. {export} gives its answers so far as a CSV."
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

/// Who answers first: its trial decides whether a version answers on its own.
const FIRST_ANSWERER: &str = "bison";

fn is_trial(value: &Value) -> bool {
    value.get("ready").is_some() || value.get("summary").is_some()
}

/// The trials kept on a version, by who answered them, the first answerer's
/// first: a version keeps each under its answerer's key. A trial sent as it
/// is says who answered it, else it is the first answerer's.
fn trials_of(trial: Option<&Value>) -> Vec<(&str, &Value)> {
    let Some(trial) = trial.filter(|trial| trial.is_object()) else {
        return Vec::new();
    };
    if is_trial(trial) {
        return vec![(
            text_field(trial, &["model"]).unwrap_or(FIRST_ANSWERER),
            trial,
        )];
    }
    let mut trials: Vec<(&str, &Value)> = trial
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, value)| is_trial(value))
        .map(|(answerer, value)| (answerer.as_str(), value))
        .collect();
    trials.sort_by_key(|(answerer, _)| *answerer != FIRST_ANSWERER);
    trials
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

/// A trial in a few lines: ready or not, whether a person makes every call,
/// its summary, and what it came to. `answerer` is who answered it.
fn trial_lines(trial: &Value, answerer: &str) -> Vec<String> {
    let mut lines = Vec::new();
    let first = answerer == FIRST_ANSWERER;
    let label = if first {
        "Trial:".to_string()
    } else {
        format!("Trial on {}:", one_line(answerer))
    };
    let ready = trial.get("ready").and_then(Value::as_bool);
    let needs = needs_words(trial);
    lines.push(match (ready, needs.is_empty()) {
        (Some(true), _) if first => format!("{label} ready to answer on its own."),
        (Some(true), _) => format!(
            "{label} ready: it answers the rows of a run that {FIRST_ANSWERER} gives no answer to."
        ),
        (Some(false), false) => format!("{label} not ready; it needs {}.", needs.join(", ")),
        (Some(false), true) => format!("{label} not ready."),
        (None, _) => format!("{label} tried."),
    });
    if trial.get("review_only").and_then(Value::as_bool) == Some(true) {
        lines.push(
            "It decides about people (a job, credit, a place to live or insurance), so a person makes every call: no row of it is Sure, and each comes back as Check, about a person."
                .to_string(),
        );
    }
    // A check of the nightly watch found it sure and wrong on labeled rows.
    if let Some(held) = trial.get("held").filter(|held| held.is_object()) {
        let mut line = format!(
            "Held: a check found it sure and wrong on {} labeled answer(s), so it still answers, but no row of it is Sure until a person takes that check",
            int_field(held, "sure_and_wrong").map_or_else(unknown, |n| n.to_string())
        );
        if let Some(check) = text_field(held, &["retest"]) {
            line.push_str(&format!(" (check_id={})", one_line(check)));
        }
        line.push_str(" or it is tried again.");
        lines.push(line);
    }
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
    let trials = trials_of(version.get("trial"));
    for (answerer, trial) in &trials {
        lines.extend(trial_lines(trial, answerer));
    }
    if trials.is_empty() && number.is_some() {
        lines.push("Trial: not tried yet.".to_string());
    }
    // The first answerer's trial decides whether the version answers on its
    // own.
    let trial = trials
        .iter()
        .find(|(answerer, _)| *answerer == FIRST_ANSWERER)
        .map(|(_, trial)| *trial);
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
        let copy = text_field(saved, &["template"])
            .map(|template| {
                format!(
                    " It started from the starter template {}; unchanged, it needs fewer labeled rows.",
                    one_line(template)
                )
            })
            .unwrap_or_default();
        lines.push(format!(
            "Saved as a draft (version {version}): it answers nothing until a version is tried on rows the person labeled and activated.{copy}"
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

fn templates_text(result: &Value) -> String {
    let templates = items(result, "templates");
    if templates.is_empty() {
        return "[QUESTIONS] No starter templates here. Save a question with its own spec: questions(action=\"create\", name=\"...\", spec={...}).".to_string();
    }
    let mut lines = vec![format!(
        "[QUESTIONS] {} starter template(s). Save a copy to change as the workspace's own with questions(action=\"create\", name=\"...\", template=\"<key>\"); an unchanged copy needs fewer labeled rows before it may answer on its own.",
        templates.len()
    )];
    lines.extend(templates.iter().take(MOST_LINES).map(|template| {
        format!(
            "- {} (template=\"{}\"): {}, {}",
            text_field(template, &["name"])
                .map(one_line)
                .unwrap_or_else(|| "(unnamed)".into()),
            text_field(template, &["key"])
                .map(one_line)
                .unwrap_or_else(|| "?".into()),
            text_field(template, &["shape"])
                .map(one_line)
                .unwrap_or_else(|| "?".into()),
            text_field(template, &["about"])
                .map(|about| cut(about, 200))
                .unwrap_or_default()
        )
    }));
    lines.extend(more_line(MOST_LINES, templates.len()));
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
    let tried = trials_of(result.get("trial")).into_iter().next();
    let answerer = tried.map_or(FIRST_ANSWERER, |(answerer, _)| answerer);
    let by = if answerer == FIRST_ANSWERER {
        String::new()
    } else {
        format!(", answered by {}", one_line(answerer))
    };
    let mut lines = vec![format!(
        "[QUESTIONS] Tried version {} of {name} on its labeled rows{by}.",
        version.map_or_else(unknown, |n| n.to_string())
    )];
    if let Some((answerer, trial)) = tried {
        lines.extend(trial_lines(trial, answerer));
    }
    let ready = tried
        .and_then(|(_, trial)| trial.get("ready"))
        .and_then(Value::as_bool);
    if answerer != FIRST_ANSWERER {
        // The second answerer's trial lets it take the rows the first gives
        // no answer to; whether the version answers at all is the first's.
        lines.push(if ready == Some(true) {
            format!(
                "Next: nothing more for it. Once the version answers on its own (its {FIRST_ANSWERER} trial decides: questions(action=\"try\", question_id=\"{question}\")), {} answers the rows of its runs that {FIRST_ANSWERER} gives no answer to.",
                one_line(answerer)
            )
        } else {
            format!(
                "Next: until this trial is ready, the rows {FIRST_ANSWERER} gives no answer to stay for a person. Label more rows with the person's answers, then questions(action=\"try\", question_id=\"{question}\", model=\"{}\").",
                one_line(answerer)
            )
        });
        return lines.join("\n");
    }
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
        "{line}\nNext: follow it with questions(action=\"status\", question_id=\"{question}\", {run_ref}); questions(action=\"results\", question_id=\"{question}\", {run_ref}, which=\"check\") lists decisions and Sure spot checks."
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
    // Where a finished run's answers can go back to, by where its list lives.
    let back = match run.pointer("/source/kind").and_then(Value::as_str) {
        Some("sheet") => Some("beside the list in the Google Sheet"),
        Some("excel") => Some("beside the list in the Excel workbook"),
        Some("audience") => Some("onto the audience's contacts"),
        _ => None,
    };
    lines.push(match text_field(run, &["status"]) {
        Some("queued" | "running") => format!(
            "Next: it goes on in the background; ask again with questions(action=\"status\", question_id=\"{question}\", {run_ref}). Rows answered so far are in results."
        ),
        Some("paused") => format!(
            "Next: it is paused because the credits ran out. Once there are credits again, questions(action=\"resume\", question_id=\"{question}\", {run_ref}) goes on."
        ),
        Some("done") => {
            let mut next = format!(
                "Next: questions(action=\"results\", question_id=\"{question}\", {run_ref}, which=\"check\") lists decisions and Sure spot checks; questions(action=\"export\", question_id=\"{question}\", {run_ref}) gives every row as a CSV"
            );
            if let Some(back) = back {
                next.push_str(&format!(
                    "; questions(action=\"write_back\", question_id=\"{question}\", {run_ref}) writes the answers {back}"
                ));
            }
            next.push('.');
            next
        }
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
        (Some(_), Some("about_a_person")) => {
            words.push_str(" (Check: about a person, a person's call)")
        }
        // Its version is held after a check of the nightly watch.
        (Some(_), Some("held")) => {
            words.push_str(" (Check: being checked again, a person's call for now)")
        }
        // A requirement check found no passage that answers it.
        (Some(_), Some("not_found")) => words.push_str(" (Check: not found in the document)"),
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

/// The check filter includes unanswered rows, uncertain answers, and Sure
/// answers selected for confirmation. A person's recorded call settles any of them.
fn review_summary(rows: &[Value]) -> Value {
    let mut needs_decision = 0;
    let mut sure_spot_checks = 0;
    let mut already_reviewed = 0;
    for row in rows {
        if row.get("called").is_some_and(|call| !call.is_null()) {
            already_reviewed += 1;
        } else if text_field(row, &["trust"]) == Some("sure")
            && row.get("spot_check").and_then(Value::as_bool) == Some(true)
        {
            sure_spot_checks += 1;
        } else {
            needs_decision += 1;
        }
    }
    json!({
        "needs_decision": needs_decision,
        "sure_spot_checks": sure_spot_checks,
        "already_reviewed": already_reviewed,
        "summary": format!(
            "On this page: {needs_decision} to decide (Check or unanswered), {sure_spot_checks} to confirm (Sure spot checks), {already_reviewed} already reviewed."
        ),
    })
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
    if let Some(summary) = result.pointer("/review/summary").and_then(Value::as_str) {
        lines.push(summary.to_string());
    }
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
        let fields = row.get("fields").unwrap_or(&Value::Null);
        // A requirement check's row: the passage its answer rests on (for a
        // requirement not found, the closest one), and where it is.
        let quoted = text_field(fields, &["where"]).zip(text_field(fields, &["passage"]));
        match quoted {
            Some((place, passage)) => {
                if key.starts_with("row-") {
                    if let Some(requirement) = text_field(fields, &["requirement"]) {
                        line.push_str(&format!(" [{}]", cut(requirement, PREVIEW_CHARS)));
                    }
                }
                let from = if text_field(row, &["reason"]) == Some("not_found") {
                    "closest"
                } else {
                    "quoted from"
                };
                line.push_str(&format!(
                    "; {from} {}: \"{}\"",
                    one_line(place),
                    cut(passage, PREVIEW_CHARS)
                ));
            }
            // A key made from the row's hash says nothing; its fields do.
            None if key.starts_with("row-") => {
                let fields = preview(Some(fields));
                if !fields.is_empty() {
                    line.push_str(&format!(" [{fields}]"));
                }
            }
            None => {}
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

/// What became of a write-back: the two columns of a sheet or a workbook,
/// or the two properties of an audience's contacts.
fn written_back_text(result: &Value) -> String {
    if let Some(property) = text_field(result, &["property"]) {
        return contacts_written_text(result, property);
    }
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
            "Sent to the app for {range}; it has not confirmed it yet. Read the list before writing again."
        ),
        Some("awaiting_approval") => format!(
            "Waiting for approval: the answers go into {range} once the person approves the change in ContextStream."
        ),
        Some("failed") => format!(
            "Not written: the app refused it: {}.",
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

/// What writing a run's answers onto its audience's contacts came to.
fn contacts_written_text(result: &Value, property: &str) -> String {
    let contacts = result.get("contacts").unwrap_or(&Value::Null);
    let count = |key: &str| int_field(contacts, key).unwrap_or(0);
    let property = one_line(property);
    let status = text_field(result, &["sure_or_check_property"])
        .map(one_line)
        .unwrap_or_else(|| format!("{property}_sure_or_check"));
    let mut line = format!(
        "[QUESTIONS] Written onto the audience's contacts: {} contact(s) hold their answer under the property {property}, and Sure, Check or Your call under {status}.",
        count("written")
    );
    let mut rest = Vec::new();
    if count("unchanged") > 0 {
        rest.push(format!("{} held it already", count("unchanged")));
    }
    if count("cleared") > 0 {
        rest.push(format!(
            "{} with no answer now had their earlier one taken away",
            count("cleared")
        ));
    }
    if count("full") > 0 {
        rest.push(format!(
            "{} had no room for two more properties and were left as they were",
            count("full")
        ));
    }
    if count("gone") > 0 {
        rest.push(format!("{} are no longer in the workspace", count("gone")));
    }
    if !rest.is_empty() {
        line.push_str(&format!(" Of the others, {}.", rest.join(", ")));
    }
    line
}

/// What the nightly watch says of an answerer, in words.
fn verdict_words(verdict: Option<&str>) -> String {
    match verdict {
        Some("steady") => "steady: it answers as it did".to_string(),
        Some("moved") => {
            "moved: what answers changed, so the questions it answers are checked again".to_string()
        }
        Some("baseline") => "a new reference for the nights after".to_string(),
        Some("unread") => "too few rows got an answer to say".to_string(),
        Some(other) => one_line(other),
        None => unknown(),
    }
}

/// One answerer's last night of the watch, on one line.
fn canary_line(canary: &Value) -> String {
    let count = |key: &str| int_field(canary, key).map_or_else(unknown, |n| n.to_string());
    let mut line = format!(
        "{}: {} ({} of {} made-up row(s) answered, {} as labeled, {} sure, {} sure and wrong)",
        text_field(canary, &["model"])
            .map(one_line)
            .unwrap_or_else(unknown),
        verdict_words(text_field(canary, &["verdict"])),
        count("answered"),
        count("cases"),
        count("as_labeled"),
        count("sure"),
        count("sure_and_wrong"),
    );
    if let Some(flipped) = int_field(canary, "flipped").filter(|flipped| *flipped > 0) {
        line.push_str(&format!(
            "; {flipped} answer(s) changed since its reference"
        ));
    }
    if let Some(missed) = int_field(canary, "guard_missed").filter(|missed| *missed > 0) {
        line.push_str(&format!(
            "; {missed} question(s) about a person taken for not about one"
        ));
    }
    if let Some(at) = text_field(canary, &["ran_at"]) {
        line.push_str(&format!("; ran {}", one_line(at)));
    }
    line
}

/// How a question whose check holds it goes on.
const HELD_NEXT: &str = "A held question still answers, but no row of it is Sure until a person takes its check or tries it again: questions(action=\"checks\", question_id=\"<id>\") shows a question's checks.";

fn watch_text(result: &Value) -> String {
    let canaries = items(result, "canaries");
    let mut lines = vec![if canaries.is_empty() {
        "[QUESTIONS] The nightly watch has not asked any answerer yet.".to_string()
    } else {
        "[QUESTIONS] The nightly watch: each night every answerer is asked the starter templates' made-up rows again and compared with its last reference.".to_string()
    }];
    lines.extend(
        canaries
            .iter()
            .take(MOST_LINES)
            .map(|canary| format!("- {}", canary_line(canary))),
    );
    lines.extend(more_line(MOST_LINES, canaries.len()));
    let held = match result.get("checks") {
        // The workspace's checks by outcome.
        Some(Value::Object(counts)) => {
            let count = |key: &str| counts.get(key).and_then(Value::as_i64).unwrap_or(0);
            let counted: Vec<String> = [
                ("waiting", "waiting"),
                ("steady", "steady"),
                ("moved", "moved"),
                ("held", "held their version"),
                ("failed", "failed"),
            ]
            .into_iter()
            .filter(|&(key, _)| count(key) > 0)
            .map(|(key, words)| format!("{} {words}", count(key)))
            .collect();
            lines.push(if counted.is_empty() {
                "No question of this workspace was checked again in the last 30 days.".to_string()
            } else {
                format!(
                    "This workspace's checks in the last 30 days: {}.",
                    counted.join(", ")
                )
            });
            count("held") > 0
        }
        // Or one by one.
        Some(Value::Array(checks)) => {
            lines.push(format!("This workspace's recent checks: {}.", checks.len()));
            lines.extend(
                checks
                    .iter()
                    .take(MOST_LINES)
                    .map(|check| format!("- {}", check_line(check))),
            );
            lines.extend(more_line(MOST_LINES, checks.len()));
            checks
                .iter()
                .any(|check| check.get("held").and_then(Value::as_bool) == Some(true))
        }
        _ => false,
    };
    if held {
        lines.push(HELD_NEXT.to_string());
    }
    lines.join("\n")
}

/// Why a check ran, in words.
fn check_reason_words(reason: Option<&str>) -> String {
    match reason {
        Some("model_moved") => "the nightly watch saw its answerer change".to_string(),
        Some("corrections") => "a person corrected enough of its rows".to_string(),
        Some(other) => one_line(other).replace('_', " "),
        None => unknown(),
    }
}

/// Why a check's numbers moved, in words.
fn moved_words(check: &Value) -> Vec<String> {
    check
        .pointer("/result/moved")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|why| match why {
            "sure_and_wrong" => "sure and wrong on a labeled row".to_string(),
            "fewer_as_labeled" => "fewer rows answered as labeled".to_string(),
            "more_to_look_at" => "more rows for a person to look at".to_string(),
            "less_to_look_at" => "fewer rows for a person to look at".to_string(),
            other => one_line(other).replace('_', " "),
        })
        .collect()
}

/// Whether a person may take a check: it is done, not taken, and what it
/// chose would let the version answer on its own.
fn takeable(check: &Value) -> bool {
    text_field(check, &["status"]) == Some("done")
        && text_field(check, &["reviewed_at"]).is_none()
        && check
            .pointer("/result/proposal/ready")
            .and_then(Value::as_bool)
            == Some(true)
}

/// One check on one line: what it checked, why, and what it came to.
fn check_line(check: &Value) -> String {
    let status = text_field(check, &["status"]);
    let mut line = format!(
        "check_id={} {}: version {} on {}, after {}",
        text_field(check, &["id"])
            .map(one_line)
            .unwrap_or_else(unknown),
        status.map(one_line).unwrap_or_else(unknown),
        int_field(check, "version").map_or_else(unknown, |n| n.to_string()),
        text_field(check, &["model"])
            .map(one_line)
            .unwrap_or_else(unknown),
        check_reason_words(text_field(check, &["reason"])),
    );
    if status == Some("done") {
        let why = moved_words(check);
        if check.get("moved").and_then(Value::as_bool) == Some(true) || !why.is_empty() {
            line.push_str("; its numbers moved");
            if !why.is_empty() {
                line.push_str(&format!(" ({})", why.join(", ")));
            }
        } else {
            line.push_str("; its numbers held");
        }
        if check.get("held").and_then(Value::as_bool) == Some(true) {
            line.push_str(
                "; it put the version on hold (no row Sure until a person takes this check or tries the version again)",
            );
        }
        match text_field(check, &["reviewed_at"]) {
            Some(at) => line.push_str(&format!("; taken {}", one_line(at))),
            None if takeable(check) => line.push_str("; a person may take it"),
            None => {}
        }
    }
    if let Some(at) = text_field(check, &["created_at"]) {
        line.push_str(&format!("; started {}", one_line(at)));
    }
    line
}

fn checks_text(result: &Value, question: Uuid) -> String {
    // The API names the list `retests`; `checks` is read first.
    let checks = if result.get("checks").is_some() {
        items(result, "checks")
    } else {
        items(result, "retests")
    };
    if checks.is_empty() {
        return "[QUESTIONS] This question has not been checked again. A check runs when the nightly watch sees its answerer change, or once a person has corrected enough of its rows.".to_string();
    }
    let mut lines = vec![format!(
        "[QUESTIONS] {} check(s), newest first:",
        checks.len()
    )];
    lines.extend(
        checks
            .iter()
            .take(MOST_LINES)
            .map(|check| format!("- {}", check_line(check))),
    );
    lines.extend(more_line(MOST_LINES, checks.len()));
    if let Some(check) = checks.iter().find(|check| takeable(check)) {
        let id = text_field(check, &["id"])
            .map(one_line)
            .unwrap_or_else(|| "<id>".into());
        lines.push(format!(
            "A person may take check {id}: questions(action=\"take_check\", question_id=\"{question}\", check_id=\"{id}\") makes what it chose the version's trial on {}, which ends a hold. Only when the person says so.",
            text_field(check, &["model"])
                .map(one_line)
                .unwrap_or_else(|| "its answerer".into())
        ));
    }
    lines.join("\n")
}

fn taken_text(result: &Value, question: Uuid, check: Uuid) -> String {
    let taken = trials_of(result.get("trial")).into_iter().next();
    let answerer = taken.map_or(FIRST_ANSWERER, |(answerer, _)| answerer);
    let mut lines = vec![format!(
        "[QUESTIONS] Check {check} taken: what it chose is the version's trial on {} now, and a hold it put on the version is over.",
        one_line(answerer)
    )];
    if let Some((answerer, trial)) = taken {
        lines.extend(trial_lines(trial, answerer));
    }
    lines.push(format!(
        "Next: questions(action=\"get\", question_id=\"{question}\") shows the version as it answers now."
    ));
    lines.join("\n")
}

/// A run's CSV for the text and the structured result: the text shows its
/// header and the first rows, at most [`EXPORT_ROWS_SHOWN`] of them and
/// [`EXPORT_CHARS_SHOWN`] characters, so an agent's context is not filled by
/// a long list; the structured result has the whole file.
fn exported(csv: &str, question: Uuid, run: Uuid) -> (String, Value) {
    let records = csv_records(csv.strip_prefix('\u{feff}').unwrap_or(csv));
    let (header, rows) = match records.split_first() {
        Some((header, rows)) => (Some(*header), rows),
        None => (None, &[][..]),
    };
    let mut block = header.map(str::to_string).unwrap_or_default();
    let mut used = block.chars().count();
    let mut shown = 0;
    for row in rows.iter().take(EXPORT_ROWS_SHOWN) {
        let size = row.chars().count() + 1;
        if used + size > EXPORT_CHARS_SHOWN {
            break;
        }
        block.push('\n');
        block.push_str(row);
        used += size;
        shown += 1;
    }
    let total = rows.len();
    let headline = match (total, shown) {
        (0, _) => format!("[QUESTIONS] Run {run} as a CSV: no rows."),
        (total, shown) if shown == total => {
            format!("[QUESTIONS] Run {run} as a CSV: {total} row(s).")
        }
        (total, 0) => format!(
            "[QUESTIONS] Run {run} as a CSV: {total} row(s), too long to show here. The structured result has the whole file (csv)."
        ),
        (total, shown) => format!(
            "[QUESTIONS] Run {run} as a CSV: {total} row(s), the first {shown} below. The structured result has the whole file (csv)."
        ),
    };
    let mut lines = vec![headline];
    if !block.is_empty() {
        // Stored words on a line of their own cannot pass for a notice.
        lines.push(quoted_body(&block));
    }
    if shown < total && shown > 0 {
        lines.push(format!(
            "… and {} more row(s) in the structured result (csv). questions(action=\"results\", question_id=\"{question}\", run_id=\"{run}\") reads them page by page.",
            total - shown
        ));
    }
    let structured = json!({
        "question_id": question,
        "run_id": run,
        "rows": total,
        "shown": shown,
        "columns": header.map(csv_cells).unwrap_or_default(),
        "csv": csv,
    });
    (lines.join("\n"), structured)
}

/// A CSV's records, each without its line break: a line break inside a
/// quoted cell stays in its record.
fn csv_records(csv: &str) -> Vec<&str> {
    let mut records = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    for (at, c) in csv.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '\n' if !quoted => {
                let record = &csv[start..at];
                records.push(record.strip_suffix('\r').unwrap_or(record));
                start = at + 1;
            }
            _ => {}
        }
    }
    if start < csv.len() {
        records.push(&csv[start..]);
    }
    records
}

/// The cells of one CSV record, unquoted.
fn csv_cells(record: &str) -> Vec<String> {
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut quoted = false;
    let mut chars = record.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                cell.push('"');
                chars.next();
            }
            '"' => quoted = !quoted,
            ',' if !quoted => cells.push(std::mem::take(&mut cell)),
            other => cell.push(other),
        }
    }
    cells.push(cell);
    cells
}

#[cfg(test)]
#[path = "questions_tests.rs"]
mod tests;
