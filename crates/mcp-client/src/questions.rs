//! Saved questions: a question with fixed answers, asked about every row of a
//! list. Each row gets an answer and is either sure or left for a person to
//! check.
//!
//! The ContextStream API is the one implementation. These calls carry one
//! request each, under the client's `/api/v1` base and with the caller's own
//! credential, and every one names the workspace (the one given, else the
//! session's default):
//!
//! - `GET /questions?workspace_id&archived&limit` lists the saved questions;
//!   `GET /questions/templates?workspace_id` the starter templates;
//!   `POST /questions` `{workspace_id, name, spec?, shared?, template?}`
//!   (201) saves one, a copy of the starter template `template` names when
//!   no spec is sent; `GET /questions/:id?workspace_id&version` reads one with
//!   its history; `POST /questions/:id/versions`
//!   `{workspace_id, spec, expected_revision}` (201) writes a new version;
//! - `GET` and `POST /questions/:id/test-rows` read and add the rows a person
//!   labeled; `POST /questions/:id/sample` picks rows worth labeling;
//!   `POST /questions/:id/trial` `{workspace_id, version?, model?}` tries a
//!   version on the labeled rows;
//!   `POST /questions/:id/activate` lets a version answer on its own;
//! - `POST /questions/:id/describe` `{workspace_id, source}` reads a list's
//!   columns; `POST /questions/:id/runs` `{workspace_id, source, mapping?,
//!   surface, document?}` (202) asks the question about every row of a list
//!   (with `document`, a requirement check: each row's requirement against
//!   the document's passages), and
//!   `GET .../runs`, `GET .../runs/:run_id`, `POST .../cancel`,
//!   `POST .../resume` and `GET .../rows?which&after&limit` follow a run;
//!   `GET .../runs/:run_id/export?workspace_id` reads a run's rows as a CSV
//!   (`text/csv`, done or not); `POST .../runs/:run_id/write-back`
//!   `{workspace_id, session_id?, mapping?, attribute?}` writes a finished
//!   run's answers back where its list lives: beside a Google Sheet's or an
//!   Excel workbook's range (`session_id` required), or onto an audience's
//!   contacts (`attribute`);
//! - `POST /questions/:id/answer` `{workspace_id, key?, fields, mapping?}`
//!   answers one row;
//! - the nightly watch: `GET /questions/watch?workspace_id` reads each
//!   answerer's last canary and the workspace's recent checks;
//!   `GET /questions/:id/retests?workspace_id` a question's checks, newest
//!   first; `POST /questions/:id/retests/:retest_id/accept` `{workspace_id}`
//!   takes what a done check chose as the version's trial on its answerer.
//!
//! Every run this client starts says it came from an agent's tools
//! ([`RUN_SURFACE`]). A person's own call on a row is not offered here.
//!
//! Where saved questions are switched off, or the workspace is not one they
//! are offered in, the routes answer 404. While answering is switched off the
//! routes that ask answer 503 with [`NOT_ASKING`], which is not retried. A 409
//! or 422 message ends with a code in brackets ([`refusal_code`]).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::client::{scope_ids_with_defaults, strip_nulls, ContextStreamClient};
use mcp_types::{Error, Result};

/// The shapes a saved question takes.
pub const SHAPES: &[&str] = &["screen", "sort", "rate", "checklist", "match"];
/// How one of its questions is answered: yes or no (`noul`), or one of its
/// options (`choice`).
pub const QUESTION_TYPES: &[&str] = &["noul", "choice"];
/// What a checklist's answers come to.
pub const ROLLUP_RULES: &[&str] = &["all", "any", "at_least"];
/// Where a list comes from: `kind` of a [`ListSource`].
pub const SOURCE_KINDS: &[&str] = &[
    "rows", "csv", "audience", "sheet", "excel", "notion", "linear", "issues", "jira",
];
/// Where a requirement check's document comes from: `kind` of a
/// [`RunDocument`].
pub const DOCUMENT_KINDS: &[&str] = &["text", "google_doc", "word"];
/// Which of a repository's issues an `issues` list takes: open ones unless
/// it says otherwise.
pub const ISSUE_STATES: &[&str] = &["open", "closed", "all"];
/// Who a version is tried on: `bison`, whose trial lets the version answer
/// on its own (the default), or `mammoth`, which then answers the rows of a
/// run `bison` gives no answer to.
pub const TRIAL_MODELS: &[&str] = &["bison", "mammoth"];
/// Which rows of a run to read.
pub const ROW_FILTERS: &[&str] = &["all", "check", "sure"];
/// `surface` of every run this client starts: an agent's tools.
pub const RUN_SURFACE: &str = "mcp";
/// The code a 503 ends with while answering is switched off where the API
/// runs. It does not change in the seconds a retry waits.
pub const NOT_ASKING: &str = "[NOT_ASKING]";
/// The spec format the API reads, filled in when a spec leaves it out.
pub const SPEC_FORMAT: u32 = 1;
/// What a run's export is asked for as.
pub const CSV_MEDIA_TYPE: &str = "text/csv";

fn spec_format() -> u32 {
    SPEC_FORMAT
}

/// A saved question as it is sent: without lines, which the API chooses by
/// trying the question on labeled rows. Unknown fields are refused, so a
/// typo (`description` for `about`) is said instead of dropped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionSpec {
    /// The format, [`SPEC_FORMAT`] when left out.
    #[serde(default = "spec_format")]
    pub spec: u32,
    /// One of [`SHAPES`].
    pub shape: String,
    /// One sentence saying what a row is.
    pub about: String,
    /// The fields of a row the question looks at, in order. Empty for a
    /// match, which looks at its [`SpecPair`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub show: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pair: Option<SpecPair>,
    pub questions: Vec<SpecQuestion>,
    /// A checklist's rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollup: Option<SpecRollup>,
}

/// One narrow question of a spec.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpecQuestion {
    /// What its answer is kept under: a short lower-case label.
    pub key: String,
    /// The question, in the person's words.
    pub ask: String,
    /// One of [`QUESTION_TYPES`].
    #[serde(rename = "type")]
    pub kind: String,
    /// A choice's options; a choice always has one keyed `none`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<SpecOption>,
}

/// One answer a choice offers, and what it means.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpecOption {
    pub key: String,
    pub means: String,
}

/// The two things a match compares: the fields of each.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpecPair {
    pub first: Vec<String>,
    pub second: Vec<String>,
}

/// What a checklist's answers come to: `rule` one of [`ROLLUP_RULES`], with
/// `k` for `at_least`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpecRollup {
    pub rule: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub k: Option<u64>,
}

/// A row of a list: its fields by name, and the key it is known by.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListRow {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub fields: Map<String, Value>,
}

/// A row with the answers a person gives it, by question key, or `answer`
/// for what the row comes to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LabeledRow {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub fields: Map<String, Value>,
    pub want: BTreeMap<String, String>,
}

/// Which column of a list is which field the question looks at: by field,
/// the column's name. A field not named is the column of the same name.
pub type Mapping = BTreeMap<String, String>;

/// A list a question is asked about, as the API reads it. Unknown fields are
/// refused, as the API refuses them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ListSource {
    /// Rows sent as they are.
    Rows { rows: Vec<ListRow> },
    /// A CSV or TSV file's text; `key_column` names the column that keys a row.
    Csv {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key_column: Option<String>,
    },
    /// A Create Email audience of the workspace.
    Audience { audience_id: Uuid },
    /// A Google Sheets range (`Sheet1!A1:F500`) through a connected app.
    Sheet {
        connection_id: Uuid,
        spreadsheet_id: String,
        range: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key_column: Option<String>,
    },
    /// An Excel range in OneDrive or SharePoint through a connected app.
    Excel {
        connection_id: Uuid,
        drive_id: String,
        item_id: String,
        worksheet_id: String,
        range: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        key_column: Option<String>,
    },
    /// A Notion database's data source through a connected app: one row a
    /// page, each property a field.
    Notion {
        connection_id: Uuid,
        data_source_id: String,
    },
    /// A Linear project's issues through a connected app: `identifier`,
    /// `title`, `state` and `url`.
    Linear {
        connection_id: Uuid,
        project_id: String,
    },
    /// A repository's issues as the workspace keeps them in sync; `state` is
    /// one of [`ISSUE_STATES`], `open` when left out.
    Issues {
        repository_id: Uuid,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        state: Option<String>,
    },
    /// The Jira issues the workspace's Jira connection keeps in sync that the
    /// caller may see, newest first, keyed by issue key: one project's
    /// (`project_key`, `OPS`) or every project's, of one `status` as Jira
    /// names it (`In Progress`) or of any.
    Jira {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        project_key: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
    },
}

/// The document a requirement check reads each requirement of a list
/// against, as the API reads it: only a question that matches one
/// requirement field with one passage field checks one. Unknown fields are
/// refused, as the API refuses them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RunDocument {
    /// Text pasted or read out of a file, at most 2 MiB: a form feed marks a
    /// page, a blank line a paragraph.
    Text {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        title: Option<String>,
    },
    /// A Google Doc through a connected app.
    GoogleDoc {
        connection_id: Uuid,
        document_id: String,
    },
    /// A Word document in OneDrive or SharePoint through a connected app.
    Word {
        connection_id: Uuid,
        drive_id: String,
        item_id: String,
    },
}

/// A new saved question: the body of `POST /questions` without its workspace.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct NewQuestion {
    pub name: String,
    /// The question. Left out with `template`, it is a copy of that starter
    /// template.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec: Option<QuestionSpec>,
    /// Shared with the workspace (the API's default) or kept to its maker.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shared: Option<bool>,
    /// The starter template it was made from, by its key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template: Option<String>,
}

/// One row to answer now: the body of `POST /questions/:id/answer` without
/// its workspace.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OneRow {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    pub fields: Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mapping: Option<Mapping>,
}

/// How a finished run's answers are written back: the body of
/// `POST .../write-back` without its workspace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WriteBack {
    /// For a Google Sheet or an Excel workbook: the session the change is
    /// recorded under (the app asks for its review there). An audience needs
    /// none.
    pub session_id: Option<String>,
    pub mapping: Option<Mapping>,
    /// For an audience: the contact property the answer is kept under, the
    /// question's name when left out. Sure, Check or Your call is kept under
    /// the same name with `_sure_or_check`.
    pub attribute: Option<String>,
}

/// Which page of a run's rows `GET .../rows` reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RowsPage {
    /// One of [`ROW_FILTERS`]; all rows when left out.
    pub which: Option<String>,
    /// The `next` of the previous page.
    pub after: Option<i64>,
    pub limit: Option<i64>,
}

impl RowsPage {
    fn query(&self) -> String {
        let mut params = String::new();
        if let Some(which) = self.which.as_deref().map(str::trim) {
            if !which.is_empty() {
                params.push_str(&format!("&which={}", urlencoding::encode(which)));
            }
        }
        if let Some(after) = self.after.filter(|after| *after >= 0) {
            params.push_str(&format!("&after={after}"));
        }
        if let Some(limit) = self.limit.filter(|limit| *limit > 0) {
            params.push_str(&format!("&limit={limit}"));
        }
        params
    }
}

/// The code in brackets that ends a refusal's message (`... [NOT_ACTIVE]`),
/// when it has one.
pub fn refusal_code(message: &str) -> Option<&str> {
    let inner = message.trim_end().strip_suffix(']')?;
    let code = &inner[inner.rfind('[')? + 1..];
    (!code.is_empty()
        && code
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
    .then_some(code)
}

fn question_path(question_id: Uuid, rest: &str) -> String {
    format!("/questions/{question_id}{rest}")
}

fn run_path(question_id: Uuid, run_id: Uuid, rest: &str) -> String {
    question_path(question_id, &format!("/runs/{run_id}{rest}"))
}

impl ContextStreamClient {
    /// The workspace a saved-question call names: the one given, else the
    /// session's default. Every route takes one.
    async fn question_workspace(&self, workspace_id: Option<Uuid>) -> Result<Uuid> {
        let config = self.config().await;
        scope_ids_with_defaults(workspace_id, None, &config)
            .0
            .ok_or_else(|| {
                Error::Validation(
                    "workspace_id is required for saved questions: pass it, or run init so the session has a workspace."
                        .to_string(),
                )
            })
    }

    /// The saved questions the caller can see, most recently changed first.
    /// Endpoint: `GET /questions?workspace_id&archived&limit`.
    pub async fn list_questions(
        &self,
        workspace_id: Option<Uuid>,
        archived: bool,
        limit: Option<i64>,
    ) -> Result<Value> {
        let workspace_id = self.question_workspace(workspace_id).await?;
        let mut path = format!("/questions?workspace_id={workspace_id}");
        if archived {
            path.push_str("&archived=true");
        }
        if let Some(limit) = limit.filter(|limit| *limit > 0) {
            path.push_str(&format!("&limit={limit}"));
        }
        self.get(&path).await
    }

    /// One saved question, one version of it (the one named, else the one
    /// that answers, else the newest), its history and how many rows are
    /// labeled. Endpoint: `GET /questions/:id?workspace_id&version`.
    pub async fn get_question(
        &self,
        workspace_id: Option<Uuid>,
        question_id: Uuid,
        version: Option<i64>,
    ) -> Result<Value> {
        let workspace_id = self.question_workspace(workspace_id).await?;
        let mut path = question_path(question_id, &format!("?workspace_id={workspace_id}"));
        if let Some(version) = version {
            path.push_str(&format!("&version={version}"));
        }
        self.get(&path).await
    }

    /// Save a question, as a draft. Endpoint: `POST /questions` (201),
    /// answered `{question, version}`.
    pub async fn create_question(
        &self,
        workspace_id: Option<Uuid>,
        question: &NewQuestion,
    ) -> Result<Value> {
        let workspace_id = self.question_workspace(workspace_id).await?;
        let body = strip_nulls(serde_json::json!({
            "workspace_id": workspace_id,
            "name": question.name,
            "spec": question.spec,
            "shared": question.shared,
            "template": question.template,
        }));
        self.post("/questions", body).await
    }

    /// Write a changed question as its next version; the ones before stay as
    /// they were. Endpoint: `POST /questions/:id/versions` (201).
    pub async fn add_question_version(
        &self,
        workspace_id: Option<Uuid>,
        question_id: Uuid,
        spec: &QuestionSpec,
        expected_revision: i64,
    ) -> Result<Value> {
        let workspace_id = self.question_workspace(workspace_id).await?;
        let body = serde_json::json!({
            "workspace_id": workspace_id,
            "spec": spec,
            "expected_revision": expected_revision,
        });
        self.post(&question_path(question_id, "/versions"), body)
            .await
    }

    /// The rows a person labeled to try the question on.
    /// Endpoint: `GET /questions/:id/test-rows?workspace_id`.
    pub async fn question_test_rows(
        &self,
        workspace_id: Option<Uuid>,
        question_id: Uuid,
    ) -> Result<Value> {
        let workspace_id = self.question_workspace(workspace_id).await?;
        self.get(&question_path(
            question_id,
            &format!("/test-rows?workspace_id={workspace_id}"),
        ))
        .await
    }

    /// Keep labeled rows to try the question on; a row kept already takes
    /// the new label. Endpoint: `POST /questions/:id/test-rows`, answered
    /// `{added, relabeled, test_rows}`.
    pub async fn label_question_rows(
        &self,
        workspace_id: Option<Uuid>,
        question_id: Uuid,
        rows: &[LabeledRow],
    ) -> Result<Value> {
        let workspace_id = self.question_workspace(workspace_id).await?;
        let body = serde_json::json!({ "workspace_id": workspace_id, "rows": rows });
        self.post(&question_path(question_id, "/test-rows"), body)
            .await
    }

    /// The rows of a list worth labeling, spread over every kind of answer.
    /// Endpoint: `POST /questions/:id/sample`, answered `{rows, asked, answered}`.
    pub async fn sample_question_rows(
        &self,
        workspace_id: Option<Uuid>,
        question_id: Uuid,
        rows: &[ListRow],
        version: Option<i64>,
        pick: Option<i64>,
    ) -> Result<Value> {
        let workspace_id = self.question_workspace(workspace_id).await?;
        let body = strip_nulls(serde_json::json!({
            "workspace_id": workspace_id,
            "version": version,
            "rows": rows,
            "pick": pick,
        }));
        self.post(&question_path(question_id, "/sample"), body)
            .await
    }

    /// Try a version (else the newest) on the labeled rows, on `model` (one
    /// of [`TRIAL_MODELS`], `bison` when left out).
    /// Endpoint: `POST /questions/:id/trial`, answered `{question, version, trial}`.
    pub async fn try_question(
        &self,
        workspace_id: Option<Uuid>,
        question_id: Uuid,
        version: Option<i64>,
        model: Option<&str>,
    ) -> Result<Value> {
        let workspace_id = self.question_workspace(workspace_id).await?;
        let body = strip_nulls(serde_json::json!({
            "workspace_id": workspace_id,
            "version": version,
            "model": model,
        }));
        self.post(&question_path(question_id, "/trial"), body).await
    }

    /// The starter templates a workspace copies: `{templates: [{key, name,
    /// about, shape, spec}]}`. Endpoint: `GET /questions/templates?workspace_id`.
    pub async fn list_question_templates(&self, workspace_id: Option<Uuid>) -> Result<Value> {
        let workspace_id = self.question_workspace(workspace_id).await?;
        self.get(&format!("/questions/templates?workspace_id={workspace_id}"))
            .await
    }

    /// Let a version whose trial said it may answer on its own do so.
    /// Endpoint: `POST /questions/:id/activate`, answered `{question}`.
    pub async fn activate_question(
        &self,
        workspace_id: Option<Uuid>,
        question_id: Uuid,
        version: i64,
        expected_revision: i64,
    ) -> Result<Value> {
        let workspace_id = self.question_workspace(workspace_id).await?;
        let body = serde_json::json!({
            "workspace_id": workspace_id,
            "version": version,
            "expected_revision": expected_revision,
        });
        self.post(&question_path(question_id, "/activate"), body)
            .await
    }

    /// A list's columns with a few values of each, and the fields the
    /// question looks at that the list lacks.
    /// Endpoint: `POST /questions/:id/describe`, answered
    /// `{rows, columns, looks_at, missing}`.
    pub async fn describe_question_list(
        &self,
        workspace_id: Option<Uuid>,
        question_id: Uuid,
        source: &ListSource,
    ) -> Result<Value> {
        let workspace_id = self.question_workspace(workspace_id).await?;
        let body = serde_json::json!({ "workspace_id": workspace_id, "source": source });
        self.post(&question_path(question_id, "/describe"), body)
            .await
    }

    /// Ask the question, as it answers now, about every row of a list, as a
    /// run started from an agent's tools ([`RUN_SURFACE`]). With `document`,
    /// a requirement check: each row's requirement is asked about the
    /// document's passages that speak to it.
    /// Endpoint: `POST /questions/:id/runs` (202), answered `{run, estimate}`.
    pub async fn start_question_run(
        &self,
        workspace_id: Option<Uuid>,
        question_id: Uuid,
        source: &ListSource,
        mapping: Option<&Mapping>,
        document: Option<&RunDocument>,
    ) -> Result<Value> {
        let workspace_id = self.question_workspace(workspace_id).await?;
        let body = strip_nulls(serde_json::json!({
            "workspace_id": workspace_id,
            "source": source,
            "mapping": mapping,
            "surface": RUN_SURFACE,
            "document": document,
        }));
        self.post(&question_path(question_id, "/runs"), body).await
    }

    /// The question's runs, newest first.
    /// Endpoint: `GET /questions/:id/runs?workspace_id`, answered `{runs}`.
    pub async fn question_runs(
        &self,
        workspace_id: Option<Uuid>,
        question_id: Uuid,
    ) -> Result<Value> {
        let workspace_id = self.question_workspace(workspace_id).await?;
        self.get(&question_path(
            question_id,
            &format!("/runs?workspace_id={workspace_id}"),
        ))
        .await
    }

    /// One run; a run whose worker went away goes on when it is read.
    /// Endpoint: `GET /questions/:id/runs/:run_id?workspace_id`, answered
    /// `{run, resumed, spot_checks}`.
    pub async fn question_run(
        &self,
        workspace_id: Option<Uuid>,
        question_id: Uuid,
        run_id: Uuid,
    ) -> Result<Value> {
        let workspace_id = self.question_workspace(workspace_id).await?;
        self.get(&run_path(
            question_id,
            run_id,
            &format!("?workspace_id={workspace_id}"),
        ))
        .await
    }

    /// A page of a run's rows in the list's order.
    /// Endpoint: `GET /questions/:id/runs/:run_id/rows?workspace_id&which&after&limit`,
    /// answered `{rows, next}`.
    pub async fn question_run_rows(
        &self,
        workspace_id: Option<Uuid>,
        question_id: Uuid,
        run_id: Uuid,
        page: &RowsPage,
    ) -> Result<Value> {
        let workspace_id = self.question_workspace(workspace_id).await?;
        self.get(&run_path(
            question_id,
            run_id,
            &format!("/rows?workspace_id={workspace_id}{}", page.query()),
        ))
        .await
    }

    /// Stop a run. Endpoint: `POST /questions/:id/runs/:run_id/cancel`,
    /// answered `{run}`.
    pub async fn cancel_question_run(
        &self,
        workspace_id: Option<Uuid>,
        question_id: Uuid,
        run_id: Uuid,
    ) -> Result<Value> {
        let workspace_id = self.question_workspace(workspace_id).await?;
        self.post(
            &run_path(question_id, run_id, "/cancel"),
            serde_json::json!({ "workspace_id": workspace_id }),
        )
        .await
    }

    /// Go on with a paused run, or one whose worker went away.
    /// Endpoint: `POST /questions/:id/runs/:run_id/resume`, answered `{run}`.
    pub async fn resume_question_run(
        &self,
        workspace_id: Option<Uuid>,
        question_id: Uuid,
        run_id: Uuid,
    ) -> Result<Value> {
        let workspace_id = self.question_workspace(workspace_id).await?;
        self.post(
            &run_path(question_id, run_id, "/resume"),
            serde_json::json!({ "workspace_id": workspace_id }),
        )
        .await
    }

    /// Answer one row now; an answer kept for the same row before is read,
    /// not asked again. Endpoint: `POST /questions/:id/answer`, answered
    /// `{answer, version, kept_before}`.
    pub async fn answer_question_row(
        &self,
        workspace_id: Option<Uuid>,
        question_id: Uuid,
        row: &OneRow,
    ) -> Result<Value> {
        let workspace_id = self.question_workspace(workspace_id).await?;
        let body = strip_nulls(serde_json::json!({
            "workspace_id": workspace_id,
            "key": row.key,
            "fields": row.fields,
            "mapping": row.mapping,
        }));
        self.post(&question_path(question_id, "/answer"), body)
            .await
    }

    /// Write a finished run's answers back where its list lives: two columns
    /// right of a Google Sheet's or an Excel workbook's range, through the
    /// connected app, or two properties on each contact of an audience.
    /// Endpoint: `POST /questions/:id/runs/:run_id/write-back`, answered
    /// `{range, write}` for a sheet or a workbook and
    /// `{property, sure_or_check_property, contacts}` for an audience.
    pub async fn write_back_question_run(
        &self,
        workspace_id: Option<Uuid>,
        question_id: Uuid,
        run_id: Uuid,
        write: &WriteBack,
    ) -> Result<Value> {
        let workspace_id = self.question_workspace(workspace_id).await?;
        let body = strip_nulls(serde_json::json!({
            "workspace_id": workspace_id,
            "session_id": write.session_id,
            "mapping": write.mapping,
            "attribute": write.attribute,
        }));
        self.post(&run_path(question_id, run_id, "/write-back"), body)
            .await
    }

    /// A run's rows as a CSV, done or not: each row's key and the fields the
    /// question looks at, its answer (a person's call first), Sure, Check or
    /// Your call, why a person should look, and each question's answer when
    /// it asks more than one.
    /// Endpoint: `GET /questions/:id/runs/:run_id/export?workspace_id`,
    /// answered with `text/csv`.
    pub async fn export_question_run(
        &self,
        workspace_id: Option<Uuid>,
        question_id: Uuid,
        run_id: Uuid,
    ) -> Result<String> {
        let workspace_id = self.question_workspace(workspace_id).await?;
        self.get_text(
            &run_path(
                question_id,
                run_id,
                &format!("/export?workspace_id={workspace_id}"),
            ),
            Some(CSV_MEDIA_TYPE),
        )
        .await
    }

    /// The nightly watch: each answerer's last canary (the starter templates'
    /// made-up rows asked again, as counts and a verdict) and the workspace's
    /// checks of the last 30 days. Endpoint: `GET /questions/watch?workspace_id`,
    /// answered `{canaries, checks}`.
    pub async fn question_watch(&self, workspace_id: Option<Uuid>) -> Result<Value> {
        let workspace_id = self.question_workspace(workspace_id).await?;
        self.get(&format!("/questions/watch?workspace_id={workspace_id}"))
            .await
    }

    /// The question's checks, newest first: why each ran, what it came to and
    /// whether it held the version. Endpoint:
    /// `GET /questions/:id/retests?workspace_id`.
    pub async fn question_checks(
        &self,
        workspace_id: Option<Uuid>,
        question_id: Uuid,
    ) -> Result<Value> {
        let workspace_id = self.question_workspace(workspace_id).await?;
        self.get(&question_path(
            question_id,
            &format!("/retests?workspace_id={workspace_id}"),
        ))
        .await
    }

    /// Take what a done check chose as the version's trial on its answerer,
    /// which ends a hold the check put on the version. Endpoint:
    /// `POST /questions/:id/retests/:retest_id/accept`, answered `{trial}`.
    pub async fn take_question_check(
        &self,
        workspace_id: Option<Uuid>,
        question_id: Uuid,
        check_id: Uuid,
    ) -> Result<Value> {
        let workspace_id = self.question_workspace(workspace_id).await?;
        self.post(
            &question_path(question_id, &format!("/retests/{check_id}/accept")),
            serde_json::json!({ "workspace_id": workspace_id }),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_refusal_code_is_the_label_in_brackets_that_ends_the_message() {
        assert_eq!(
            refusal_code("This question does not answer on its own yet. [NOT_ACTIVE]"),
            Some("NOT_ACTIVE")
        );
        assert_eq!(
            refusal_code("Too few rows. [NO_TEST_ROWS]  "),
            Some("NO_TEST_ROWS")
        );
        assert_eq!(refusal_code("No such question."), None);
        assert_eq!(refusal_code("A list [of rows]"), None);
        assert_eq!(refusal_code("[]"), None);
        assert_eq!(refusal_code("]"), None);
    }

    #[test]
    fn a_spec_fills_its_format_and_names_a_typo() {
        let spec: QuestionSpec = serde_json::from_value(json!({
            "shape": "screen",
            "about": "A sales lead",
            "show": ["company", "notes"],
            "questions": [{"key": "fit", "ask": "Is this lead a fit?", "type": "noul"}],
        }))
        .unwrap();
        assert_eq!(spec.spec, SPEC_FORMAT);
        // Sent as the API reads it: nothing left out comes back as null.
        assert_eq!(
            serde_json::to_value(&spec).unwrap(),
            json!({
                "spec": 1,
                "shape": "screen",
                "about": "A sales lead",
                "show": ["company", "notes"],
                "questions": [{"key": "fit", "ask": "Is this lead a fit?", "type": "noul"}],
            })
        );
        let typo = serde_json::from_value::<QuestionSpec>(json!({
            "shape": "screen", "description": "A sales lead", "questions": [],
        }));
        assert!(typo.unwrap_err().to_string().contains("description"));
    }

    #[test]
    fn a_source_is_tagged_by_its_kind_and_refuses_what_its_kind_lacks() {
        let audience = Uuid::new_v4();
        assert_eq!(
            serde_json::to_value(ListSource::Audience {
                audience_id: audience
            })
            .unwrap(),
            json!({"kind": "audience", "audience_id": audience})
        );
        let csv: ListSource =
            serde_json::from_value(json!({"kind": "csv", "text": "company\nAcme\n"})).unwrap();
        assert_eq!(
            serde_json::to_value(csv).unwrap(),
            json!({"kind": "csv", "text": "company\nAcme\n"})
        );
        let missing =
            serde_json::from_value::<ListSource>(json!({"kind": "sheet", "range": "A1:B2"}));
        assert!(missing.unwrap_err().to_string().contains("connection_id"));
        let stray =
            serde_json::from_value::<ListSource>(json!({"kind": "csv", "text": "a", "sheet": "x"}));
        assert!(stray.unwrap_err().to_string().contains("sheet"));

        // Lists read through a connected app, and a repository's issues.
        let connection = Uuid::new_v4();
        let repository = Uuid::new_v4();
        for (sent, kind) in [
            (
                json!({"kind": "notion", "connection_id": connection, "data_source_id": "ds-1"}),
                "notion",
            ),
            (
                json!({"kind": "linear", "connection_id": connection, "project_id": "proj-1"}),
                "linear",
            ),
            (
                json!({"kind": "issues", "repository_id": repository, "state": "all"}),
                "issues",
            ),
            (
                json!({"kind": "issues", "repository_id": repository}),
                "issues",
            ),
            // The Jira issues kept in sync: one project's of one status, or
            // every one.
            (
                json!({"kind": "jira", "project_key": "OPS", "status": "In Progress"}),
                "jira",
            ),
            (json!({"kind": "jira"}), "jira"),
        ] {
            let source: ListSource = serde_json::from_value(sent.clone()).unwrap();
            assert_eq!(serde_json::to_value(&source).unwrap(), sent, "{kind}");
            assert!(SOURCE_KINDS.contains(&kind));
        }
        let missing = serde_json::from_value::<ListSource>(
            json!({"kind": "notion", "connection_id": connection}),
        );
        assert!(missing.unwrap_err().to_string().contains("data_source_id"));
        let stray = serde_json::from_value::<ListSource>(json!({"kind": "jira", "project": "OPS"}));
        assert!(stray
            .unwrap_err()
            .to_string()
            .contains("unknown field `project`"));
        let unknown = serde_json::from_value::<ListSource>(json!({"kind": "asana"}));
        assert!(unknown.unwrap_err().to_string().contains("asana"));
    }

    #[test]
    fn a_document_is_tagged_by_its_kind_and_sent_as_it_was_given() {
        let connection = Uuid::new_v4();
        for (sent, kind) in [
            (
                json!({"kind": "text", "text": "Security\n\nWe encrypt at rest.\u{c}Support\n\n24/7.", "title": "Reply"}),
                "text",
            ),
            (json!({"kind": "text", "text": "One page."}), "text"),
            (
                json!({"kind": "google_doc", "connection_id": connection, "document_id": "doc-1"}),
                "google_doc",
            ),
            (
                json!({"kind": "word", "connection_id": connection, "drive_id": "b!x", "item_id": "01AB"}),
                "word",
            ),
        ] {
            let document: RunDocument = serde_json::from_value(sent.clone()).unwrap();
            assert_eq!(serde_json::to_value(&document).unwrap(), sent, "{kind}");
            assert!(DOCUMENT_KINDS.contains(&kind));
        }
        let missing = serde_json::from_value::<RunDocument>(
            json!({"kind": "word", "connection_id": connection, "item_id": "01AB"}),
        );
        assert!(missing.unwrap_err().to_string().contains("drive_id"));
        let stray = serde_json::from_value::<RunDocument>(
            json!({"kind": "text", "text": "a", "url": "https://example.com"}),
        );
        assert!(stray
            .unwrap_err()
            .to_string()
            .contains("unknown field `url`"));
        let unknown = serde_json::from_value::<RunDocument>(json!({"kind": "pdf", "text": "a"}));
        assert!(unknown.unwrap_err().to_string().contains("pdf"));
    }

    #[test]
    fn a_question_made_from_a_template_sends_no_spec() {
        let copy = NewQuestion {
            name: "Lead fit".into(),
            spec: None,
            shared: None,
            template: Some("lead-fit".into()),
        };
        assert_eq!(
            serde_json::to_value(&copy).unwrap(),
            json!({"name": "Lead fit", "template": "lead-fit"})
        );
    }

    #[test]
    fn a_rows_page_sends_only_what_was_given() {
        assert_eq!(RowsPage::default().query(), "");
        let page = RowsPage {
            which: Some("check".into()),
            after: Some(41),
            limit: Some(25),
        };
        assert_eq!(page.query(), "&which=check&after=41&limit=25");
        let first = RowsPage {
            after: Some(-1),
            limit: Some(0),
            ..RowsPage::default()
        };
        assert_eq!(first.query(), "");
    }

    #[test]
    fn creates_a_resend_could_repeat_are_not_resent_blindly() {
        // A question, a version and a run are made by the request that
        // reached the API; the API keeps no idempotency key for them.
        let id = Uuid::new_v4();
        for path in [
            "/api/v1/questions".to_string(),
            format!("/api/v1/questions/{id}/versions"),
            format!("/api/v1/questions/{id}/runs"),
        ] {
            assert!(
                crate::client::is_unkeyed_create_request("POST", &path),
                "{path}"
            );
        }
        // The rest change nothing twice: a label replaces the label, an
        // answer kept before is read, a trial or a pick is asked again, and a
        // check is taken once (a resend after it was taken is refused).
        for path in [
            format!("/api/v1/questions/{id}/test-rows"),
            format!("/api/v1/questions/{id}/answer"),
            format!("/api/v1/questions/{id}/trial"),
            format!("/api/v1/questions/{id}/sample"),
            format!("/api/v1/questions/{id}/runs/{id}/cancel"),
            format!("/api/v1/questions/{id}/retests/{id}/accept"),
        ] {
            assert!(
                !crate::client::is_unkeyed_create_request("POST", &path),
                "{path}"
            );
        }
    }
}
