//! Agent Coach Kit: the coach REST API (`/api/v1/coach`, contract `coach.v1`).
//!
//! Every request the coach tools send and every response they read is mapped
//! here and nowhere else, so a change in the API's contract touches only this
//! file. The tools see typed requests ([`CoachRequest`]), typed views of what
//! came back (no internal ids: the views drop them), and one typed error
//! ([`CoachApiError`]) carrying the API's stable code.
//!
//! The contract this follows:
//!
//! - Paths take a readable reference (`weekly-note`, `weekly-note#4`,
//!   `weekly-note#4/v2`, `weekly-note#4/f1`, `weekly-note/g3`) percent-encoded
//!   as one segment.
//! - Every write carries `operation_id` in its JSON body. A retry with the same
//!   id and request answers 200 with the stored receipt and
//!   `Idempotent-Replayed: true`; the same id with a different request is 409
//!   `operation_id_conflict`; one still in flight is 409
//!   `operation_in_progress` with `Retry-After`.
//! - Responses use the `{success, data, error: {code, message, details}}`
//!   envelope; error messages start with the code (`version_conflict: ...`).
//! - Only deleting a workflow uses `DELETE`; nothing else in this module can
//!   build one.

use crate::client::{ContextStreamClient, RawJsonResponse};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::time::Duration;

/// The contract `GET /coach/capabilities` names.
pub const COACH_CONTRACT: &str = "coach.v1";
/// The response header that marks a replayed write.
pub const REPLAYED_HEADER: &str = "idempotent-replayed";
/// The header some API errors carry their stable code in.
pub const ERROR_CODE_HEADER: &str = "x-contextstream-error-code";
/// The OAuth scope a delegated token needs to read coach data.
pub const READ_SCOPE: &str = "read:coach";
/// The OAuth scope a delegated token needs for anything else.
pub const WRITE_SCOPE: &str = "write:coach";
/// Days within which a deleted workflow is erased, when the API does not say.
pub const DEFAULT_ERASE_WITHIN_DAYS: i64 = 30;

const READ_TIMEOUT: Duration = Duration::from_secs(25);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// An upload from a URL is fetched by the API before it answers.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(55);
const MAX_ATTEMPTS: u32 = 3;
const MAX_RETRY_WAIT: Duration = Duration::from_secs(5);

// ============================================================================
// Operation ids
// ============================================================================

/// Whether `id` is a valid operation id: 8-128 characters of
/// `[A-Za-z0-9._:-]`.
pub fn is_valid_operation_id(id: &str) -> bool {
    (8..=128).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'))
}

/// The operation id for one write, derived from the tool, the action and the
/// normalized arguments, so a retry of the same call reuses it. `bucket`
/// (`(minutes, unix_seconds)`) adds a time window, for writes the person may
/// legitimately repeat with the same words later (a new job with last week's
/// objective).
///
/// Normalization: the top-level `operation_id` is left out, `null` values are
/// dropped and object keys are sorted. Text is hashed exactly as given, so the
/// person's words are never trimmed into someone else's.
pub fn derive_operation_id(
    tool: &str,
    action: &str,
    args: &Value,
    bucket: Option<(u64, i64)>,
) -> String {
    let mut normalized = args.clone();
    if let Value::Object(map) = &mut normalized {
        map.remove("operation_id");
    }
    let mut canonical = String::new();
    write_canonical(&normalized, &mut canonical);
    let mut material = format!("{COACH_CONTRACT}|{tool}|{action}|{canonical}");
    if let Some((minutes, now)) = bucket {
        let width = (minutes.max(1) * 60) as i64;
        material.push_str(&format!("|{}", now.div_euclid(width)));
    }
    let digest = Sha256::digest(material.as_bytes());
    format!("mcp-{}", &hex::encode(digest)[..40])
}

/// A second operation id belonging to `parent` (the upload a `save_output`
/// makes before it records the output).
pub fn sub_operation_id(parent: &str, step: &str) -> String {
    let digest = Sha256::digest(format!("{parent}|{step}").as_bytes());
    format!("mcp-{}", &hex::encode(digest)[..40])
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map
                .iter()
                .filter(|(_, value)| !value.is_null())
                .map(|(key, _)| key)
                .collect();
            keys.sort();
            out.push('{');
            for (index, key) in keys.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String((*key).clone()).to_string());
                out.push(':');
                write_canonical(&map[*key], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

// ============================================================================
// Requests
// ============================================================================

/// The HTTP method of a coach request. `Delete` is only ever built by
/// [`CoachRequest::delete_workflow`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoachMethod {
    Get,
    Post,
    Put,
    Patch,
    Delete,
}

impl CoachMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
        }
    }
}

/// One request to the coach API. Built only by the constructors below.
#[derive(Debug, Clone, PartialEq)]
pub struct CoachRequest {
    method: CoachMethod,
    path: String,
    body: Option<Value>,
    timeout: Duration,
}

impl CoachRequest {
    pub fn method(&self) -> CoachMethod {
        self.method
    }

    /// The path below `/api/v1`, query included.
    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn body(&self) -> Option<&Value> {
        self.body.as_ref()
    }

    /// The operation id a write carries.
    pub fn operation_id(&self) -> Option<&str> {
        self.body
            .as_ref()
            .and_then(|body| body.get("operation_id"))
            .and_then(Value::as_str)
    }

    pub fn is_write(&self) -> bool {
        self.method != CoachMethod::Get
    }

    fn get(path: String) -> Self {
        Self {
            method: CoachMethod::Get,
            path,
            body: None,
            timeout: READ_TIMEOUT,
        }
    }

    fn write(
        method: CoachMethod,
        path: String,
        operation_id: &str,
        fields: Map<String, Value>,
    ) -> Self {
        debug_assert!(method != CoachMethod::Get);
        let mut body = Map::new();
        body.insert(
            "operation_id".to_string(),
            Value::String(operation_id.to_string()),
        );
        body.extend(fields);
        Self {
            method,
            path,
            body: Some(Value::Object(body)),
            timeout: WRITE_TIMEOUT,
        }
    }

    // ---- reads -------------------------------------------------------------

    /// `GET /coach/capabilities`
    pub fn capabilities() -> Self {
        Self::get("/coach/capabilities".to_string())
    }

    /// `GET /coach/workflows`
    pub fn list_workflows(params: &ListWorkflows) -> Self {
        Self::get(format!(
            "/coach/workflows{}",
            query(&[
                ("status", params.status.clone()),
                ("page", params.page.map(|v| v.to_string())),
                ("per_page", params.per_page.map(|v| v.to_string())),
            ])
        ))
    }

    /// `GET /coach/workflows/:ref`: the goal, recent objectives, active
    /// guidance, open items and the coaching effort.
    pub fn get_workflow(workflow: &str) -> Self {
        Self::get(format!("/coach/workflows/{}", seg(workflow)))
    }

    /// `GET /coach/jobs/:ref`
    pub fn get_job(job: &str) -> Self {
        Self::get(format!("/coach/jobs/{}", seg(job)))
    }

    /// `GET /coach/outputs/:ref`
    pub fn get_output(output: &str) -> Self {
        Self::get(format!("/coach/outputs/{}", seg(output)))
    }

    /// `GET /coach/feedback/:ref`
    pub fn get_feedback(feedback: &str) -> Self {
        Self::get(format!("/coach/feedback/{}", seg(feedback)))
    }

    /// `GET /coach/guidance/:ref`
    pub fn get_guidance(guidance: &str) -> Self {
        Self::get(format!("/coach/guidance/{}", seg(guidance)))
    }

    /// `GET /coach/jobs/:ref/context`: the job packet.
    pub fn job_context(job: &str) -> Self {
        Self::get(format!("/coach/jobs/{}/context", seg(job)))
    }

    /// `GET /coach/workflows/:ref/jobs`, newest first.
    pub fn list_jobs(workflow: &str, status: Option<&str>, per_page: Option<u32>) -> Self {
        Self::get(format!(
            "/coach/workflows/{}/jobs{}",
            seg(workflow),
            query(&[
                ("status", status.map(str::to_string)),
                ("per_page", per_page.map(|v| v.to_string())),
            ])
        ))
    }

    /// `GET /coach/workflows/:ref/goals`
    pub fn goal_history(workflow: &str) -> Self {
        Self::get(format!("/coach/workflows/{}/goals", seg(workflow)))
    }

    /// `GET /coach/workflows/:ref/feedback`
    pub fn list_feedback(workflow: &str, params: &ListFeedback) -> Self {
        Self::get(format!(
            "/coach/workflows/{}/feedback{}",
            seg(workflow),
            query(&[
                ("job", params.job.clone()),
                ("scope", params.scope.clone()),
                ("kind", params.kind.clone()),
                (
                    "include_retracted",
                    params.include_retracted.then(|| "true".to_string()),
                ),
                ("per_page", params.per_page.map(|v| v.to_string())),
            ])
        ))
    }

    /// `GET /coach/workflows/:ref/guidance`
    pub fn list_guidance(workflow: &str, status: Option<&str>) -> Self {
        Self::get(format!(
            "/coach/workflows/{}/guidance{}",
            seg(workflow),
            query(&[("status", status.map(str::to_string))])
        ))
    }

    /// `GET /coach/guidance/:ref/history`
    pub fn guidance_history(guidance: &str) -> Self {
        Self::get(format!("/coach/guidance/{}/history", seg(guidance)))
    }

    /// `GET /coach/jobs/:ref/verdicts`
    pub fn list_verdicts(job: &str) -> Self {
        Self::get(format!("/coach/jobs/{}/verdicts", seg(job)))
    }

    /// `GET /coach/compare?left=&right=`
    pub fn compare(left: &str, right: &str) -> Self {
        Self::get(format!(
            "/coach/compare{}",
            query(&[
                ("left", Some(left.to_string())),
                ("right", Some(right.to_string())),
            ])
        ))
    }

    /// `GET /coach/workflows/:ref/metrics`
    pub fn metrics(workflow: &str) -> Self {
        Self::get(format!("/coach/workflows/{}/metrics", seg(workflow)))
    }

    /// `GET /coach/workflows/:ref/export?format=`
    pub fn export(workflow: &str, format: ExportFormat) -> Self {
        Self::get(format!(
            "/coach/workflows/{}/export{}",
            seg(workflow),
            query(&[("format", Some(format.as_str().to_string()))])
        ))
    }

    // ---- writes ------------------------------------------------------------

    /// `POST /coach/workflows`: a workflow and its goal version 1, at the
    /// person's own account.
    pub fn create_workflow(operation_id: &str, params: &CreateWorkflow) -> Self {
        let mut fields = Map::new();
        put(&mut fields, "name", Some(params.name.clone()));
        put(&mut fields, "goal", Some(params.goal.clone()));
        put(&mut fields, "audience", params.audience.clone());
        put(&mut fields, "output_kind", params.output_kind.clone());
        Self::write(
            CoachMethod::Post,
            "/coach/workflows".to_string(),
            operation_id,
            fields,
        )
    }

    /// `POST /coach/workflows/:ref/goals`
    pub fn refine_goal(operation_id: &str, workflow: &str, params: &RefineGoal) -> Self {
        let mut fields = Map::new();
        fields.insert(
            "expected_goal_version".to_string(),
            Value::from(params.expected_goal_version),
        );
        put(&mut fields, "goal", Some(params.goal.clone()));
        put(&mut fields, "reason", Some(params.reason.clone()));
        put(
            &mut fields,
            "source_feedback",
            params.source_feedback.clone(),
        );
        Self::write(
            CoachMethod::Post,
            format!("/coach/workflows/{}/goals", seg(workflow)),
            operation_id,
            fields,
        )
    }

    /// `POST /coach/workflows/:ref/jobs`
    pub fn start_job(operation_id: &str, workflow: &str, params: &StartJob) -> Self {
        let mut fields = Map::new();
        put(&mut fields, "objective", Some(params.objective.clone()));
        put(&mut fields, "label", params.label.clone());
        Self::write(
            CoachMethod::Post,
            format!("/coach/workflows/{}/jobs", seg(workflow)),
            operation_id,
            fields,
        )
    }

    /// `POST /coach/jobs/:ref/uploads`: media for an output, fetched by the API
    /// from `source_url` (a file the host handed over).
    pub fn upload(operation_id: &str, job: &str, params: &Upload) -> Self {
        let mut fields = Map::new();
        put(&mut fields, "kind", Some(params.kind.as_str().to_string()));
        put(&mut fields, "filename", Some(params.filename.clone()));
        put(&mut fields, "mime_type", params.mime_type.clone());
        put(&mut fields, "title", params.title.clone());
        put(&mut fields, "source_url", Some(params.source_url.clone()));
        let mut request = Self::write(
            CoachMethod::Post,
            format!("/coach/jobs/{}/uploads", seg(job)),
            operation_id,
            fields,
        );
        request.timeout = UPLOAD_TIMEOUT;
        request
    }

    /// `POST /coach/jobs/:ref/outputs`: one output version.
    pub fn save_output(operation_id: &str, job: &str, params: &SaveOutput) -> Self {
        let mut fields = Map::new();
        match &params.content {
            OutputContent::Text { body } => {
                put(&mut fields, "kind", Some("text".to_string()));
                put(&mut fields, "body", Some(body.clone()));
            }
            OutputContent::Link { url, snapshot } => {
                put(&mut fields, "kind", Some("link".to_string()));
                put(&mut fields, "url", Some(url.clone()));
                put(&mut fields, "body", snapshot.clone());
            }
            OutputContent::Media { kind, content_id } => {
                put(&mut fields, "kind", Some(kind.as_str().to_string()));
                put(&mut fields, "content_id", Some(content_id.clone()));
            }
        }
        put(&mut fields, "title", params.title.clone());
        put(&mut fields, "caption", params.caption.clone());
        put(&mut fields, "source_output", params.source_output.clone());
        put(&mut fields, "context_etag", params.context_etag.clone());
        put(&mut fields, "carry_note", params.carry_note.clone());
        if !params.carried.is_empty() {
            let carried = params
                .carried
                .iter()
                .map(|item| {
                    let mut entry = Map::new();
                    put(&mut entry, "ref", Some(item.reference.clone()));
                    put(&mut entry, "how", Some(item.how.clone()));
                    put(&mut entry, "note", item.note.clone());
                    Value::Object(entry)
                })
                .collect();
            fields.insert("carried".to_string(), Value::Array(carried));
        }
        Self::write(
            CoachMethod::Post,
            format!("/coach/jobs/{}/outputs", seg(job)),
            operation_id,
            fields,
        )
    }

    /// `POST /coach/outputs/:ref/feedback`: the person's words, verbatim.
    pub fn add_feedback(operation_id: &str, output: &str, params: &AddFeedback) -> Self {
        let mut fields = Map::new();
        put(&mut fields, "kind", Some(params.kind.clone()));
        put(&mut fields, "scope", Some(params.scope.clone()));
        // Verbatim: never trimmed or rewritten.
        fields.insert("body".to_string(), Value::String(params.body.clone()));
        put(&mut fields, "interpretation", params.interpretation.clone());
        put(
            &mut fields,
            "origin",
            Some(if params.picked { "picked" } else { "typed" }.to_string()),
        );
        put(&mut fields, "excerpt", params.excerpt.clone());
        if let Some((start, end)) = params.time_range_ms {
            fields.insert("time_start_ms".to_string(), Value::from(start));
            fields.insert("time_end_ms".to_string(), Value::from(end));
        }
        put(
            &mut fields,
            "repeat_of_guidance",
            params.repeat_of_guidance.clone(),
        );
        if params.reports_leak {
            fields.insert("reports_leak".to_string(), Value::Bool(true));
        }
        put(&mut fields, "applies_when", params.applies_when.clone());
        Self::write(
            CoachMethod::Post,
            format!("/coach/outputs/{}/feedback", seg(output)),
            operation_id,
            fields,
        )
    }

    /// `POST /coach/feedback/:ref/rescope`
    pub fn rescope_feedback(operation_id: &str, feedback: &str, scope: &str) -> Self {
        let mut fields = Map::new();
        put(&mut fields, "scope", Some(scope.to_string()));
        Self::write(
            CoachMethod::Post,
            format!("/coach/feedback/{}/rescope", seg(feedback)),
            operation_id,
            fields,
        )
    }

    /// `POST /coach/feedback/:ref/retract`
    pub fn retract_feedback(operation_id: &str, feedback: &str, reason: Option<&str>) -> Self {
        let mut fields = Map::new();
        put(&mut fields, "reason", reason.map(str::to_string));
        Self::write(
            CoachMethod::Post,
            format!("/coach/feedback/{}/retract", seg(feedback)),
            operation_id,
            fields,
        )
    }

    /// `PATCH /coach/guidance/:ref`
    pub fn edit_guidance(operation_id: &str, guidance: &str, params: &EditGuidance) -> Self {
        let mut fields = Map::new();
        fields.insert(
            "expected_version".to_string(),
            Value::from(params.expected_version),
        );
        put(&mut fields, "body", params.body.clone());
        match &params.applies_when {
            FieldChange::Keep => {}
            FieldChange::Clear => {
                fields.insert("applies_when".to_string(), Value::Null);
            }
            FieldChange::Set(value) => {
                fields.insert("applies_when".to_string(), Value::String(value.clone()));
            }
        }
        put(
            &mut fields,
            "status",
            params.status.map(|status| status.as_str().to_string()),
        );
        put(&mut fields, "reason", params.reason.clone());
        Self::write(
            CoachMethod::Patch,
            format!("/coach/guidance/{}", seg(guidance)),
            operation_id,
            fields,
        )
    }

    /// `POST /coach/workflows/:ref/guidance/import`
    pub fn import_guidance(operation_id: &str, workflow: &str, sources: &[String]) -> Self {
        let mut fields = Map::new();
        fields.insert(
            "sources".to_string(),
            Value::Array(sources.iter().cloned().map(Value::String).collect()),
        );
        Self::write(
            CoachMethod::Post,
            format!("/coach/workflows/{}/guidance/import", seg(workflow)),
            operation_id,
            fields,
        )
    }

    /// `PUT /coach/workflows/:ref/effort` or `PUT /coach/jobs/:ref/effort`.
    pub fn set_effort(operation_id: &str, target: EffortTarget<'_>, params: &SetEffort) -> Self {
        let mut fields = Map::new();
        fields.insert(
            "effort".to_string(),
            params
                .effort
                .map(|effort| Value::String(effort.as_str().to_string()))
                .unwrap_or(Value::Null),
        );
        put(&mut fields, "said", params.said.clone());
        let path = match target {
            EffortTarget::Workflow(workflow) => {
                format!("/coach/workflows/{}/effort", seg(workflow))
            }
            EffortTarget::Job(job) => format!("/coach/jobs/{}/effort", seg(job)),
        };
        Self::write(CoachMethod::Put, path, operation_id, fields)
    }

    /// `POST /coach/jobs/:ref/close`
    pub fn close_job(operation_id: &str, job: &str, params: &CloseJob) -> Self {
        let mut fields = Map::new();
        put(&mut fields, "used", params.used.clone());
        if let Some(verdict) = &params.verdict {
            fields.insert(
                "verdict".to_string(),
                Value::Object(verdict_fields(verdict)),
            );
        }
        Self::write(
            CoachMethod::Post,
            format!("/coach/jobs/{}/close", seg(job)),
            operation_id,
            fields,
        )
    }

    /// `POST /coach/jobs/:ref/reopen`
    pub fn reopen_job(operation_id: &str, job: &str) -> Self {
        Self::write(
            CoachMethod::Post,
            format!("/coach/jobs/{}/reopen", seg(job)),
            operation_id,
            Map::new(),
        )
    }

    /// `POST /coach/jobs/:ref/cancel`
    pub fn cancel_job(operation_id: &str, job: &str) -> Self {
        Self::write(
            CoachMethod::Post,
            format!("/coach/jobs/{}/cancel", seg(job)),
            operation_id,
            Map::new(),
        )
    }

    /// `POST /coach/jobs/:ref/verdicts`
    pub fn record_verdict(operation_id: &str, job: &str, verdict: &Verdict) -> Self {
        Self::write(
            CoachMethod::Post,
            format!("/coach/jobs/{}/verdicts", seg(job)),
            operation_id,
            verdict_fields(verdict),
        )
    }

    /// `DELETE /coach/workflows/:ref` with `confirm` equal to its slug: hidden
    /// at once, erased within the retention window. The only `DELETE`.
    pub fn delete_workflow(operation_id: &str, workflow: &str, confirm: &str) -> Self {
        let mut fields = Map::new();
        put(&mut fields, "confirm", Some(confirm.to_string()));
        Self::write(
            CoachMethod::Delete,
            format!("/coach/workflows/{}", seg(workflow)),
            operation_id,
            fields,
        )
    }
}

fn verdict_fields(verdict: &Verdict) -> Map<String, Value> {
    let mut fields = Map::new();
    put(
        &mut fields,
        "verdict",
        Some(verdict.verdict.as_str().to_string()),
    );
    put(&mut fields, "note", verdict.note.clone());
    put(&mut fields, "baseline", verdict.baseline.clone());
    put(&mut fields, "candidate", verdict.candidate.clone());
    fields
}

fn put(fields: &mut Map<String, Value>, key: &str, value: Option<String>) {
    if let Some(value) = value {
        fields.insert(key.to_string(), Value::String(value));
    }
}

/// A readable reference as one path segment (`weekly-note%234%2Fv2`).
fn seg(reference: &str) -> String {
    urlencoding::encode(reference).into_owned()
}

fn query(pairs: &[(&str, Option<String>)]) -> String {
    let parts: Vec<String> = pairs
        .iter()
        .filter_map(|(key, value)| {
            value
                .as_ref()
                .map(|value| format!("{key}={}", urlencoding::encode(value)))
        })
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("?{}", parts.join("&"))
    }
}

// ---- request parameters ----------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ListWorkflows {
    pub status: Option<String>,
    pub page: Option<u32>,
    pub per_page: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ListFeedback {
    pub job: Option<String>,
    pub scope: Option<String>,
    pub kind: Option<String>,
    pub include_retracted: bool,
    pub per_page: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExportFormat {
    Markdown,
    Json,
}

impl ExportFormat {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Markdown => "markdown",
            Self::Json => "json",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CreateWorkflow {
    pub name: String,
    pub goal: String,
    pub audience: Option<String>,
    pub output_kind: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RefineGoal {
    pub expected_goal_version: i64,
    pub goal: String,
    pub reason: String,
    pub source_feedback: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct StartJob {
    pub objective: String,
    pub label: Option<String>,
}

/// The kinds of media an output can hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Image,
    Audio,
    Video,
    Document,
}

impl MediaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::Audio => "audio",
            Self::Video => "video",
            Self::Document => "document",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "image" => Some(Self::Image),
            "audio" => Some(Self::Audio),
            "video" => Some(Self::Video),
            "document" => Some(Self::Document),
            _ => None,
        }
    }

    /// The kind a MIME type implies; anything else is a document.
    pub fn from_mime(mime: Option<&str>) -> Self {
        let mime = mime.unwrap_or_default().trim().to_ascii_lowercase();
        if mime.starts_with("image/") {
            Self::Image
        } else if mime.starts_with("audio/") {
            Self::Audio
        } else if mime.starts_with("video/") {
            Self::Video
        } else {
            Self::Document
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Upload {
    pub kind: MediaKind,
    pub filename: String,
    pub mime_type: Option<String>,
    pub title: Option<String>,
    /// Where the API fetches the file (the host's download link).
    pub source_url: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum OutputContent {
    /// Text, verbatim.
    Text { body: String },
    /// An https link, with an optional text snapshot of it.
    Link {
        url: String,
        snapshot: Option<String>,
    },
    /// Media already taken in by an upload.
    Media { kind: MediaKind, content_id: String },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Carried {
    /// A feedback or guidance reference.
    pub reference: String,
    /// `kept`, `adapted`, `applied` or `not_applied`.
    pub how: String,
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SaveOutput {
    pub content: OutputContent,
    pub title: Option<String>,
    pub caption: Option<String>,
    pub source_output: Option<String>,
    pub context_etag: Option<String>,
    pub carry_note: Option<String>,
    pub carried: Vec<Carried>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AddFeedback {
    /// `correction`, `partial_approval` or `direction`.
    pub kind: String,
    /// `workflow` (reusable) or `job` (this job only).
    pub scope: String,
    /// The person's words, exactly as they said them.
    pub body: String,
    /// The coach's reading of them, kept apart.
    pub interpretation: Option<String>,
    /// A suggestion the person picked (always a direction).
    pub picked: bool,
    pub excerpt: Option<String>,
    pub time_range_ms: Option<(i64, i64)>,
    pub repeat_of_guidance: Option<String>,
    pub reports_leak: bool,
    pub applies_when: Option<String>,
}

/// A field a PATCH leaves alone, clears or sets.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum FieldChange {
    #[default]
    Keep,
    Clear,
    Set(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuidanceStatus {
    Active,
    Retired,
    Removed,
}

impl GuidanceStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Retired => "retired",
            Self::Removed => "removed",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct EditGuidance {
    pub expected_version: i64,
    pub body: Option<String>,
    pub applies_when: FieldChange,
    pub status: Option<GuidanceStatus>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effort {
    Quick,
    Explore,
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Quick => "quick",
            Self::Explore => "explore",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EffortTarget<'a> {
    Workflow(&'a str),
    Job(&'a str),
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SetEffort {
    /// `None` clears it.
    pub effort: Option<Effort>,
    /// What the person said when they chose it, verbatim.
    pub said: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerdictValue {
    Better,
    Same,
    Worse,
}

impl VerdictValue {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Better => "better",
            Self::Same => "same",
            Self::Worse => "worse",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "better" => Some(Self::Better),
            "same" => Some(Self::Same),
            "worse" => Some(Self::Worse),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Verdict {
    pub verdict: VerdictValue,
    pub note: Option<String>,
    pub baseline: Option<String>,
    pub candidate: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CloseJob {
    /// The output version the person used.
    pub used: Option<String>,
    pub verdict: Option<Verdict>,
}

// ============================================================================
// Responses and errors
// ============================================================================

/// A successful answer.
#[derive(Debug, Clone, PartialEq)]
pub struct CoachResponse {
    pub status: u16,
    /// The API replayed a write it had already committed.
    pub replayed: bool,
    /// The unwrapped `data`.
    pub data: Value,
}

impl CoachResponse {
    /// `data` read as `T`; fields the view does not know are ignored and
    /// missing ones take their defaults.
    pub fn view<T: for<'de> Deserialize<'de> + Default>(&self) -> T {
        serde_json::from_value(self.data.clone()).unwrap_or_default()
    }
}

/// What a stable error code means for the person.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoachErrorKind {
    /// Not signed in, or the token expired: sign in again.
    Unauthorized,
    /// Signed in, but the token lacks `read:coach` or `write:coach`.
    InsufficientScope,
    /// Coach is not turned on for this account (pilot).
    NotEnabled,
    /// The reference names nothing the person can see.
    NotFound,
    /// Someone changed it first; nothing was saved.
    VersionConflict,
    /// The same operation id was used for a different request.
    OperationIdConflict,
    /// The same write is still being saved; retry shortly.
    InProgress,
    /// The request was refused as invalid.
    Validation,
    /// Too large.
    PayloadTooLarge,
    /// A state conflict (job not open, name taken, already retracted...).
    Conflict,
    /// The request was understood but cannot apply (media, scope, refs...).
    Unprocessable,
    /// A slug names workflows in several workspaces.
    Ambiguous,
    /// The delete needs the workflow's slug as confirmation.
    ConfirmationRequired,
    /// The API is down or failed.
    Unavailable,
    Other,
}

/// An error the coach API answered with.
#[derive(Debug, Clone, PartialEq)]
pub struct CoachApiError {
    pub status: u16,
    /// The stable code (`version_conflict`).
    pub code: String,
    /// The message without its leading code.
    pub message: String,
    pub details: Option<Value>,
    /// The API's `WWW-Authenticate` challenge, when it sent one.
    pub www_authenticate: Option<String>,
    pub retry_after: Option<u64>,
}

impl CoachApiError {
    pub fn kind(&self) -> CoachErrorKind {
        match self.code.as_str() {
            "unauthorized" | "invalid_token" => CoachErrorKind::Unauthorized,
            "insufficient_scope" => CoachErrorKind::InsufficientScope,
            "coach_not_enabled" => CoachErrorKind::NotEnabled,
            "not_found" | "media_not_found" => CoachErrorKind::NotFound,
            "version_conflict" => CoachErrorKind::VersionConflict,
            "operation_id_conflict" => CoachErrorKind::OperationIdConflict,
            "operation_in_progress" => CoachErrorKind::InProgress,
            "validation_failed"
            | "operation_id_required"
            | "scope_conflict"
            | "team_workflows_not_available" => CoachErrorKind::Validation,
            "payload_too_large" => CoachErrorKind::PayloadTooLarge,
            "ambiguous_reference" => CoachErrorKind::Ambiguous,
            "confirmation_required" => CoachErrorKind::ConfirmationRequired,
            "workflow_name_taken"
            | "workflow_archived"
            | "job_not_open"
            | "job_has_outputs"
            | "invalid_transition"
            | "output_limit_reached"
            | "too_much_active_guidance"
            | "feedback_already_retracted"
            | "source_retracted"
            | "scope_unchanged"
            | "guidance_already_imported"
            | "conflict" => CoachErrorKind::Conflict,
            "nothing_to_judge"
            | "no_baseline"
            | "output_not_in_scope"
            | "media_not_in_workflow_workspace"
            | "media_not_ready"
            | "media_kind_mismatch"
            | "source_output_not_in_job"
            | "unknown_context_etag"
            | "carried_item_out_of_scope"
            | "time_range_needs_media"
            | "outside_media"
            | "picked_must_be_direction"
            | "one_off_feedback_on_closed_job"
            | "outputs_in_different_workflows"
            | "guidance_not_importable"
            | "unknown_reference"
            | "unprocessable" => CoachErrorKind::Unprocessable,
            "internal" | "unavailable" => CoachErrorKind::Unavailable,
            "forbidden" if self.status == 403 => CoachErrorKind::Other,
            _ => match self.status {
                401 => CoachErrorKind::Unauthorized,
                404 => CoachErrorKind::NotFound,
                409 => CoachErrorKind::Conflict,
                413 => CoachErrorKind::PayloadTooLarge,
                400 => CoachErrorKind::Validation,
                422 => CoachErrorKind::Unprocessable,
                500..=599 => CoachErrorKind::Unavailable,
                _ => CoachErrorKind::Other,
            },
        }
    }

    /// Whether the host should ask the person to sign in again: any 401 or
    /// 403, except Coach not being turned on for the account (signing in
    /// again cannot change that).
    pub fn needs_sign_in(&self) -> bool {
        match self.kind() {
            CoachErrorKind::Unauthorized | CoachErrorKind::InsufficientScope => true,
            CoachErrorKind::NotEnabled => false,
            _ => self.status == 403,
        }
    }

    /// The scope a refused request needed, from the API's challenge.
    pub fn required_scope(&self) -> Option<String> {
        let challenge = self.www_authenticate.as_deref()?;
        let start = challenge.find("scope=\"")? + "scope=\"".len();
        let end = challenge[start..].find('"')? + start;
        Some(challenge[start..end].to_string())
    }

    /// The current version a `version_conflict` reports.
    pub fn current_version(&self) -> Option<i64> {
        self.details
            .as_ref()
            .and_then(|details| details.get("current_version"))
            .and_then(Value::as_i64)
    }

    /// The candidates of an `ambiguous_reference`, by name and slug.
    pub fn candidates(&self) -> Vec<(String, String)> {
        self.details
            .as_ref()
            .and_then(|details| details.get("candidates"))
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        (
                            item.get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            item.get("slug")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether retrying the same request may succeed.
    pub fn is_retryable(&self) -> bool {
        matches!(self.status, 429 | 502 | 503 | 504) || self.kind() == CoachErrorKind::InProgress
    }
}

/// Why a coach request did not produce an answer to use.
#[derive(Debug, Clone, PartialEq)]
pub enum CoachFailure {
    /// The API answered with an error.
    Api(CoachApiError),
    /// The request may not have arrived, or its answer was lost.
    Transport(String),
}

impl CoachFailure {
    pub fn api(&self) -> Option<&CoachApiError> {
        match self {
            Self::Api(error) => Some(error),
            Self::Transport(_) => None,
        }
    }
}

/// The response of one attempt, mapped.
pub fn map_response(raw: &RawJsonResponse) -> Result<CoachResponse, Box<CoachApiError>> {
    if (200..300).contains(&raw.status) {
        let replayed = raw
            .headers
            .get(REPLAYED_HEADER)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("true"));
        return Ok(CoachResponse {
            status: raw.status,
            replayed,
            data: unwrap_envelope(raw.body.clone()),
        });
    }
    Err(Box::new(api_error(raw)))
}

fn unwrap_envelope(body: Value) -> Value {
    match body {
        Value::Object(mut map) if map.contains_key("success") && map.contains_key("data") => {
            map.remove("data").unwrap_or(Value::Null)
        }
        other => other,
    }
}

fn header(raw: &RawJsonResponse, name: &str) -> Option<String> {
    raw.headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// The code a message starts with (`version_conflict: ...`).
fn leading_code(message: &str) -> Option<&str> {
    let (code, _) = message.split_once(": ")?;
    let valid = !code.is_empty()
        && code.len() <= 48
        && code.starts_with(|c: char| c.is_ascii_lowercase())
        && code
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    valid.then_some(code)
}

fn api_error(raw: &RawJsonResponse) -> CoachApiError {
    let (code, message, details) = match raw.body.get("error") {
        Some(Value::Object(error)) => (
            error
                .get("code")
                .and_then(Value::as_str)
                .map(str::to_string),
            error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            error
                .get("details")
                .filter(|value| !value.is_null())
                .cloned(),
        ),
        Some(Value::String(message)) => (None, message.clone(), None),
        _ => (
            None,
            raw.body
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| raw.body.as_str())
                .unwrap_or_default()
                .to_string(),
            None,
        ),
    };
    let code = code
        .filter(|code| !code.trim().is_empty())
        .or_else(|| header(raw, ERROR_CODE_HEADER))
        .or_else(|| leading_code(&message).map(str::to_string))
        .unwrap_or_else(|| {
            match raw.status {
                401 => "unauthorized",
                403 => "forbidden",
                404 => "not_found",
                409 => "conflict",
                413 => "payload_too_large",
                400 => "validation_failed",
                422 => "unprocessable",
                429 => "rate_limited",
                _ if raw.status >= 500 => "unavailable",
                _ => "error",
            }
            .to_string()
        });
    let message = match message.split_once(": ") {
        Some((prefix, rest)) if prefix == code => rest.to_string(),
        _ => message,
    };
    let message = if message.trim().is_empty() {
        format!("the API answered {}", raw.status)
    } else {
        message
    };
    CoachApiError {
        status: raw.status,
        code,
        message,
        details,
        www_authenticate: header(raw, "www-authenticate"),
        retry_after: header(raw, "retry-after").and_then(|value| value.parse().ok()),
    }
}

/// A transport failure in words, without the API's address.
fn transport_summary(error: &mcp_types::Error) -> String {
    match error {
        mcp_types::Error::Timeout(seconds) => format!("no answer within {seconds} s"),
        mcp_types::Error::Network(message) if message.starts_with("Connection failed") => {
            "the connection failed".to_string()
        }
        _ => "a network error".to_string(),
    }
}

impl ContextStreamClient {
    /// Send a coach request. Connection failures, timeouts, 429, 502-504 and
    /// `operation_in_progress` are retried (three attempts in all); a write
    /// carries the same operation id on every attempt, so a retry never saves
    /// twice.
    pub async fn coach_send(&self, request: &CoachRequest) -> Result<CoachResponse, CoachFailure> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let outcome = self
                .send_raw_json(
                    request.method.as_str(),
                    &request.path,
                    request.body.as_ref(),
                    request.timeout,
                )
                .await;
            let wait = match outcome {
                Ok(raw) => match map_response(&raw) {
                    Ok(response) => return Ok(response),
                    Err(error) => {
                        if !error.is_retryable() || attempt >= MAX_ATTEMPTS {
                            return Err(CoachFailure::Api(*error));
                        }
                        error.retry_after.map(Duration::from_secs)
                    }
                },
                Err(error) => {
                    if attempt >= MAX_ATTEMPTS {
                        return Err(CoachFailure::Transport(transport_summary(&error)));
                    }
                    None
                }
            };
            let backoff = Duration::from_millis(300 * 2u64.pow(attempt - 1));
            tokio::time::sleep(wait.unwrap_or(backoff).min(MAX_RETRY_WAIT)).await;
        }
    }
}

// ============================================================================
// Views of what the API returns (no internal ids)
// ============================================================================

/// An object named by its readable reference.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RefLink {
    #[serde(rename = "ref")]
    pub reference: String,
}

fn link_ref(link: &Option<RefLink>) -> Option<String> {
    link.as_ref()
        .map(|link| link.reference.clone())
        .filter(|reference| !reference.is_empty())
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Capabilities {
    pub contract: String,
    pub enabled: bool,
    pub recorded_via: Option<String>,
    pub limits: Value,
    pub retention: Value,
}

impl Capabilities {
    /// Days within which a deleted workflow is erased.
    pub fn erase_within_days(&self) -> i64 {
        self.retention
            .get("deleted_workflow_erased_within_days")
            .and_then(Value::as_i64)
            .unwrap_or(DEFAULT_ERASE_WITHIN_DAYS)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub total: Option<i64>,
    pub page: Option<i64>,
    pub per_page: Option<i64>,
    pub total_pages: Option<i64>,
}

/// A coaching effort the person chose, with their words.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EffortView {
    pub effort: String,
    pub said: Option<String>,
    pub set_at: Option<String>,
    /// Chosen for this job only, over the workflow's.
    pub for_this_job: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GoalView {
    pub version: i64,
    #[serde(alias = "text")]
    pub goal: String,
    pub reason: Option<String>,
    source_feedback: Option<RefLink>,
    pub recorded_via: Option<String>,
    pub created_at: Option<String>,
}

impl GoalView {
    pub fn source_feedback_ref(&self) -> Option<String> {
        link_ref(&self.source_feedback)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkflowView {
    #[serde(rename = "ref")]
    pub reference: String,
    pub name: String,
    pub slug: String,
    pub audience: Option<String>,
    pub output_kind: Option<String>,
    pub status: String,
    pub goal_version: i64,
    pub version: i64,
    pub job_count: i64,
    pub guidance_count: i64,
    pub coaching_effort: Option<EffortView>,
    pub goal: Option<GoalView>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

impl WorkflowView {
    /// Its readable reference: `ref`, else the slug.
    pub fn reference(&self) -> &str {
        if self.reference.is_empty() {
            &self.slug
        } else {
            &self.reference
        }
    }
}

/// A job named in a workflow read.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecentJob {
    #[serde(rename = "ref")]
    pub reference: String,
    pub number: i64,
    pub objective: String,
    pub label: Option<String>,
    pub status: String,
    pub created_at: Option<String>,
}

/// `GET /coach/workflows/:ref`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkflowDetail {
    #[serde(flatten)]
    pub workflow: WorkflowView,
    pub recent_jobs: Vec<RecentJob>,
    pub active_guidance: Vec<GuidanceView>,
    pub open_items: Option<OpenItems>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct JobView {
    #[serde(rename = "ref")]
    pub reference: String,
    pub workflow_ref: String,
    pub number: i64,
    pub objective: String,
    pub label: Option<String>,
    pub status: String,
    pub version: i64,
    pub output_count: i64,
    pub feedback_count: i64,
    pub coaching_effort: Option<EffortView>,
    pub closed_at: Option<String>,
    pub cancelled_at: Option<String>,
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputSummary {
    #[serde(rename = "ref")]
    pub reference: String,
    pub version: i64,
    pub kind: String,
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PreviousOutput {
    #[serde(rename = "ref")]
    pub reference: String,
    pub job_ref: String,
    pub job_number: i64,
    pub objective: String,
}

/// `GET /coach/jobs/:ref`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct JobDetail {
    #[serde(flatten)]
    pub job: JobView,
    pub effective_coaching_effort: Option<EffortView>,
    pub latest_output: Option<OutputSummary>,
    pub previous_output: Option<PreviousOutput>,
    pub outputs: Vec<OutputSummary>,
    pub verdict: Option<VerdictView>,
    pub open_items: Option<OpenItems>,
}

/// Media as the reader may see it (no links: they carry internal ids).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MediaView {
    pub available: bool,
    pub content_type: Option<String>,
    pub filename: Option<String>,
    pub mime_type: Option<String>,
    /// `pending` until its upload is completed.
    pub status: Option<String>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    pub duration_ms: Option<i64>,
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CarriedView {
    /// `feedback` or `guidance`.
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(rename = "ref")]
    pub reference: Option<String>,
    pub version: Option<i64>,
    /// `kept`, `adapted`, `applied` or `not_applied`.
    pub how: String,
    pub note: Option<String>,
    /// The item's words, when the API resolved them (compare).
    pub text: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct ContextUse {
    current: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputView {
    #[serde(rename = "ref")]
    pub reference: String,
    pub job_ref: String,
    pub workflow_ref: String,
    pub version: i64,
    pub kind: String,
    /// Text, or a link's text snapshot.
    pub body: Option<String>,
    /// A link output's address.
    pub url: Option<String>,
    pub title: Option<String>,
    pub caption: Option<String>,
    pub media: Option<MediaView>,
    source_output: Option<RefLink>,
    pub carry_note: Option<String>,
    pub carried: Vec<CarriedView>,
    pub goal_version: i64,
    context: Option<ContextUse>,
    pub recorded_via: Option<String>,
    pub created_at: Option<String>,
}

impl OutputView {
    pub fn source_output_ref(&self) -> Option<String> {
        link_ref(&self.source_output)
    }

    /// Whether the context it was written from was still current.
    pub fn context_current(&self) -> Option<bool> {
        self.context.as_ref().and_then(|context| context.current)
    }
}

/// `GET /coach/outputs/:ref`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputDetail {
    #[serde(flatten)]
    pub output: OutputView,
    pub feedback: Vec<FeedbackView>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TimeRange {
    pub start_ms: i64,
    pub end_ms: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FeedbackView {
    #[serde(rename = "ref")]
    pub reference: String,
    pub output_ref: String,
    pub job_ref: String,
    /// `correction`, `partial_approval` or `direction`.
    pub kind: String,
    /// `workflow` (reusable) or `job` (this job only).
    pub scope: String,
    /// The person's words, verbatim.
    pub body: String,
    /// The coach's reading of them.
    pub interpretation: Option<String>,
    /// `typed` or `picked`.
    pub origin: Option<String>,
    pub excerpt: Option<String>,
    pub excerpt_found: Option<bool>,
    pub time_range: Option<TimeRange>,
    repeat_of_guidance: Option<RefLink>,
    pub reports_leak: bool,
    restates: Option<RefLink>,
    pub recorded_via: Option<String>,
    pub created_at: Option<String>,
    pub retracted_at: Option<String>,
    pub retraction_reason: Option<String>,
}

impl FeedbackView {
    pub fn repeat_of_guidance_ref(&self) -> Option<String> {
        link_ref(&self.repeat_of_guidance)
    }

    pub fn restates_ref(&self) -> Option<String> {
        link_ref(&self.restates)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct ImportedFrom {
    #[serde(rename = "ref")]
    reference: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GuidanceView {
    #[serde(rename = "ref")]
    pub reference: String,
    pub workflow_ref: String,
    pub kind: String,
    pub body: String,
    pub applies_when: Option<String>,
    /// `active`, `retired` or `removed`.
    pub status: String,
    pub version: i64,
    source_feedback: Option<RefLink>,
    imported_from: Option<ImportedFrom>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

impl GuidanceView {
    pub fn source_feedback_ref(&self) -> Option<String> {
        link_ref(&self.source_feedback)
    }

    pub fn imported_from_ref(&self) -> Option<String> {
        self.imported_from
            .as_ref()
            .and_then(|from| from.reference.clone())
            .filter(|reference| !reference.is_empty())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GuidanceVersionView {
    pub guidance_ref: String,
    pub version: i64,
    pub body: String,
    pub applies_when: Option<String>,
    pub status: String,
    /// `created`, `edited`, `retired`, `removed`, `restored` or
    /// `source_retracted`.
    pub change: String,
    pub reason: Option<String>,
    pub recorded_via: Option<String>,
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct VerdictView {
    pub job_ref: String,
    pub revision: i64,
    /// `better`, `same` or `worse`.
    pub verdict: String,
    pub note: Option<String>,
    baseline: Option<RefLink>,
    candidate: Option<RefLink>,
    pub recorded_via: Option<String>,
    pub created_at: Option<String>,
}

impl VerdictView {
    pub fn baseline_ref(&self) -> Option<String> {
        link_ref(&self.baseline)
    }

    pub fn candidate_ref(&self) -> Option<String> {
        link_ref(&self.candidate)
    }
}

/// Active guidance that reads like new feedback. A hint only.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RepeatView {
    #[serde(rename = "ref")]
    pub reference: String,
    pub body: String,
    pub similarity: f64,
}

/// What still needs attention in a job.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenItems {
    pub job_ref: String,
    pub latest_output_version: Option<i64>,
    pub feedback: Vec<FeedbackView>,
    pub guidance: Vec<GuidanceView>,
}

/// `POST /coach/outputs/:ref/feedback`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecordedFeedback {
    pub feedback: FeedbackView,
    pub guidance: Option<GuidanceView>,
    pub possible_repeats: Vec<RepeatView>,
}

/// `POST /coach/feedback/:ref/retract`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RetractedFeedback {
    pub feedback: FeedbackView,
    pub retired_guidance: Option<GuidanceView>,
}

/// `POST /coach/feedback/:ref/rescope`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RescopedFeedback {
    pub scope: String,
    original: Option<RefLink>,
    restated: Option<RefLink>,
    retired_guidance: Option<RefLink>,
    guidance: Option<RefLink>,
}

impl RescopedFeedback {
    pub fn original_ref(&self) -> Option<String> {
        link_ref(&self.original)
    }

    pub fn restated_ref(&self) -> Option<String> {
        link_ref(&self.restated)
    }

    pub fn retired_guidance_ref(&self) -> Option<String> {
        link_ref(&self.retired_guidance)
    }

    pub fn guidance_ref(&self) -> Option<String> {
        link_ref(&self.guidance)
    }
}

/// `PATCH /coach/guidance/:ref`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PatchedGuidance {
    #[serde(flatten)]
    pub guidance: GuidanceView,
    pub change: Option<GuidanceVersionView>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct ImportedItem {
    from: Option<String>,
    guidance: Option<RefLink>,
}

/// `POST /coach/workflows/:ref/guidance/import`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImportedGuidance {
    imported: Vec<ImportedItem>,
}

impl ImportedGuidance {
    /// `(source ref, new guidance ref)` pairs.
    pub fn pairs(&self) -> Vec<(String, String)> {
        self.imported
            .iter()
            .map(|item| {
                (
                    item.from.clone().unwrap_or_default(),
                    link_ref(&item.guidance).unwrap_or_default(),
                )
            })
            .collect()
    }
}

/// `PUT .../effort`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EffortReceipt {
    workflow: Option<RefLink>,
    job: Option<RefLink>,
    pub coaching_effort: Option<String>,
    pub said: Option<String>,
}

impl EffortReceipt {
    /// The workflow or job the effort was set on.
    pub fn target_ref(&self) -> Option<String> {
        link_ref(&self.job).or_else(|| link_ref(&self.workflow))
    }
}

/// `POST /coach/jobs/:ref/close`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClosedJob {
    pub job: JobView,
    pub verdict: Option<VerdictView>,
    used: Option<RefLink>,
}

impl ClosedJob {
    pub fn used_ref(&self) -> Option<String> {
        link_ref(&self.used)
    }
}

/// `DELETE /coach/workflows/:ref`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeletedWorkflow {
    workflow: Option<RefLink>,
    pub name: Option<String>,
    pub deleted_at: Option<String>,
    pub erase_after: Option<String>,
}

impl DeletedWorkflow {
    pub fn workflow_ref(&self) -> Option<String> {
        link_ref(&self.workflow)
    }
}

/// `POST /coach/jobs/:ref/uploads`. `content_id` names the asset to the
/// output it becomes; it is never shown.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UploadReceipt {
    pub content_id: Option<String>,
    /// `stored`, or `awaiting_upload`.
    pub status: Option<String>,
    pub kind: Option<String>,
    pub filename: Option<String>,
}

// ---- the context packet ----------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PacketWorkflow {
    #[serde(rename = "ref")]
    pub reference: String,
    pub name: String,
    pub audience: Option<String>,
    pub output_kind: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PacketJob {
    #[serde(rename = "ref")]
    pub reference: String,
    pub number: i64,
    pub objective: String,
    pub label: Option<String>,
    pub status: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GuidanceSource {
    pub feedback_ref: Option<String>,
    pub job_number: Option<i64>,
    pub output_version: Option<i64>,
    pub you_said: Option<String>,
    pub coach_reading: Option<String>,
    pub excerpt: Option<String>,
    pub time_range: Option<TimeRange>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PacketGuidance {
    #[serde(rename = "ref")]
    pub reference: String,
    pub version: i64,
    pub kind: String,
    pub text: String,
    pub applies_when: Option<String>,
    pub edited: bool,
    pub source: Option<GuidanceSource>,
    pub imported_from: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PacketFeedback {
    #[serde(rename = "ref")]
    pub reference: String,
    pub kind: String,
    pub you_said: String,
    pub coach_reading: Option<String>,
    pub origin: Option<String>,
    pub output_ref: String,
    pub output_version: i64,
    pub excerpt: Option<String>,
    pub excerpt_found: Option<bool>,
    pub time_range: Option<TimeRange>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PacketExample {
    pub guidance_ref: String,
    pub feedback_ref: String,
    pub output_ref: String,
    pub job_number: i64,
    pub you_said: String,
    pub coach_reading: Option<String>,
    pub excerpt: Option<String>,
    pub text: Option<String>,
    pub url: Option<String>,
    pub media: Option<MediaView>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PacketOutput {
    #[serde(rename = "ref")]
    pub reference: String,
    pub version: i64,
    pub kind: String,
    /// An older version shown without its text.
    pub stub: bool,
    pub text: Option<String>,
    pub url: Option<String>,
    pub title: Option<String>,
    pub caption: Option<String>,
    pub media: Option<MediaView>,
    pub carry_note: Option<String>,
    pub carried: Vec<CarriedView>,
    pub created_at: Option<String>,
}

/// `GET /coach/jobs/:ref/context` (`coach_context.v1`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextPacket {
    pub schema: String,
    /// Sent back with the next output.
    pub context_etag: String,
    /// What the agent is told before any data (from the API, verbatim).
    pub instructions: String,
    pub workflow: PacketWorkflow,
    pub goal: GoalView,
    pub job: PacketJob,
    pub coaching_effort: Option<EffortView>,
    pub guidance: Vec<PacketGuidance>,
    pub job_feedback: Vec<PacketFeedback>,
    pub approved_examples: Vec<PacketExample>,
    pub outputs: Vec<PacketOutput>,
    /// The API's own plain-text rendering.
    pub text: String,
}

// ---- compare, metrics, export ------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WrittenFrom {
    pub goal_version: i64,
    pub guidance_count: i64,
    pub job_feedback_count: i64,
    pub example_count: i64,
    pub current: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ComparedView {
    pub output: OutputView,
    job: Option<RefLink>,
    pub job_objective: String,
    chain: Vec<RefLink>,
    pub carried: Vec<CarriedView>,
    pub feedback: Vec<FeedbackView>,
    pub written_from: Option<WrittenFrom>,
}

impl ComparedView {
    pub fn job_ref(&self) -> String {
        link_ref(&self.job).unwrap_or_else(|| self.output.job_ref.clone())
    }

    /// The versions this one revises, newest first.
    pub fn chain(&self) -> Vec<String> {
        self.chain
            .iter()
            .map(|link| link.reference.clone())
            .filter(|reference| !reference.is_empty())
            .collect()
    }
}

/// `GET /coach/compare`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Comparison {
    workflow: Option<RefLink>,
    pub left: ComparedView,
    pub right: ComparedView,
    pub text: String,
}

impl Comparison {
    pub fn workflow_ref(&self) -> String {
        link_ref(&self.workflow).unwrap_or_else(|| self.left.output.workflow_ref.clone())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FeedbackCounts {
    pub total: i64,
    pub by_kind: std::collections::BTreeMap<String, i64>,
    pub by_scope: std::collections::BTreeMap<String, i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RepeatCounts {
    pub confirmed: i64,
    pub likely: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct JobMetrics {
    job: Option<RefLink>,
    pub number: i64,
    pub status: String,
    pub outputs: i64,
    pub feedback: FeedbackCounts,
    pub repeats: RepeatCounts,
    pub reported_leaks: i64,
    pub audited_leaks: i64,
    pub verdict: Option<VerdictView>,
}

impl JobMetrics {
    pub fn job_ref(&self) -> String {
        link_ref(&self.job).unwrap_or_default()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkflowTotals {
    pub jobs: i64,
    pub outputs: i64,
    pub feedback: i64,
    pub repeats: RepeatCounts,
    pub reported_leaks: i64,
    pub audited_leaks: i64,
    pub avg_feedback_first_half: Option<f64>,
    pub avg_feedback_second_half: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct LeakAudit {
    total: i64,
}

/// `GET /coach/workflows/:ref/metrics`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Metrics {
    workflow: Option<RefLink>,
    pub jobs: Vec<JobMetrics>,
    pub totals: WorkflowTotals,
    leak_audit: LeakAudit,
}

impl Metrics {
    pub fn workflow_ref(&self) -> String {
        link_ref(&self.workflow).unwrap_or_default()
    }

    /// Context items the independent audit found outside their scope.
    pub fn audited_leak_total(&self) -> i64 {
        self.leak_audit.total
    }
}

/// `GET /coach/workflows/:ref/export?format=markdown`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MarkdownExport {
    pub filename: Option<String>,
    pub markdown: String,
}

/// A JSON export with every internal id taken out: keys named `id` or
/// ending in `_id`, and download paths.
pub fn export_without_ids(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| {
                    key.as_str() != "id"
                        && !key.ends_with("_id")
                        && !key.ends_with("_ids")
                        && key.as_str() != "download_url_path"
                        && key.as_str() != "url_expires_at"
                })
                .map(|(key, value)| {
                    // Signed media links carry storage keys.
                    if key == "url" && is_signed_link(value) {
                        (key.clone(), Value::Null)
                    } else {
                        (key.clone(), export_without_ids(value))
                    }
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(export_without_ids).collect()),
        other => other.clone(),
    }
}

fn is_signed_link(value: &Value) -> bool {
    value
        .as_str()
        .is_some_and(|url| url.contains("X-Amz-Signature=") || url.contains("X-Amz-Credential="))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue};
    use serde_json::json;

    fn raw(status: u16, body: Value, headers: &[(&'static str, &'static str)]) -> RawJsonResponse {
        let mut map = HeaderMap::new();
        for (name, value) in headers {
            map.insert(*name, HeaderValue::from_static(value));
        }
        RawJsonResponse {
            status,
            headers: map,
            body,
        }
    }

    #[test]
    fn derived_operation_ids_are_stable_valid_and_ignore_key_order_and_nulls() {
        let a = derive_operation_id(
            "workflow_save",
            "add_feedback",
            &json!({"output": "weekly-note#1/v2", "text": "  Shorter.\n", "scope": "job", "excerpt": null}),
            None,
        );
        let b = derive_operation_id(
            "workflow_save",
            "add_feedback",
            &json!({"scope": "job", "text": "  Shorter.\n", "output": "weekly-note#1/v2", "operation_id": "ignored-123"}),
            None,
        );
        assert_eq!(a, b);
        assert!(a.starts_with("mcp-"));
        assert_eq!(a.len(), 44);
        assert!(is_valid_operation_id(&a));
        // The person's words are hashed exactly: whitespace counts.
        let c = derive_operation_id(
            "workflow_save",
            "add_feedback",
            &json!({"output": "weekly-note#1/v2", "text": "Shorter.", "scope": "job"}),
            None,
        );
        assert_ne!(a, c);
        // The tool and the action are part of the id.
        assert_ne!(
            derive_operation_id(
                "workflow_save",
                "retract_feedback",
                &json!({"feedback": "a#1/f1"}),
                None
            ),
            derive_operation_id(
                "workflow_save",
                "rescope_feedback",
                &json!({"feedback": "a#1/f1"}),
                None
            ),
        );
    }

    #[test]
    fn time_buckets_separate_later_repeats() {
        let args = json!({"workflow": "weekly-note", "objective": "Week 3"});
        let at = |now| derive_operation_id("workflow_save", "start_job", &args, Some((10, now)));
        assert_eq!(at(6_000), at(6_000 + 599));
        assert_ne!(at(6_000), at(6_000 + 600));
    }

    #[test]
    fn operation_id_format() {
        assert!(is_valid_operation_id("cli-0123456789"));
        assert!(!is_valid_operation_id("short"));
        assert!(!is_valid_operation_id("has space in it"));
        assert!(!is_valid_operation_id(&"x".repeat(129)));
        let sub = sub_operation_id("mcp-abc", "upload");
        assert!(is_valid_operation_id(&sub));
        assert_ne!(sub, sub_operation_id("mcp-abd", "upload"));
    }

    #[test]
    fn references_travel_as_one_encoded_segment() {
        let request = CoachRequest::job_context("weekly-note#4");
        assert_eq!(request.path(), "/coach/jobs/weekly-note%234/context");
        let request = CoachRequest::get_output("weekly-note#4/v2");
        assert_eq!(request.path(), "/coach/outputs/weekly-note%234%2Fv2");
        let request = CoachRequest::compare("weekly-note#4/v1", "weekly-note#4/v2");
        assert_eq!(
            request.path(),
            "/coach/compare?left=weekly-note%234%2Fv1&right=weekly-note%234%2Fv2"
        );
    }

    #[test]
    fn only_delete_workflow_builds_a_delete() {
        let op = "mcp-0123456789";
        let writes = [
            CoachRequest::create_workflow(op, &CreateWorkflow::default()),
            CoachRequest::refine_goal(op, "w", &RefineGoal::default()),
            CoachRequest::start_job(op, "w", &StartJob::default()),
            CoachRequest::rescope_feedback(op, "w#1/f1", "job"),
            CoachRequest::retract_feedback(op, "w#1/f1", None),
            CoachRequest::edit_guidance(op, "w/g1", &EditGuidance::default()),
            CoachRequest::import_guidance(op, "w", &[]),
            CoachRequest::set_effort(op, EffortTarget::Job("w#1"), &SetEffort::default()),
            CoachRequest::close_job(op, "w#1", &CloseJob::default()),
            CoachRequest::reopen_job(op, "w#1"),
            CoachRequest::cancel_job(op, "w#1"),
        ];
        for request in &writes {
            assert_ne!(request.method(), CoachMethod::Delete, "{}", request.path());
            assert_eq!(request.operation_id(), Some(op));
        }
        let delete = CoachRequest::delete_workflow(op, "weekly-note", "weekly-note");
        assert_eq!(delete.method(), CoachMethod::Delete);
        assert_eq!(
            delete.body(),
            Some(&json!({"operation_id": op, "confirm": "weekly-note"}))
        );
    }

    #[test]
    fn effort_clear_sends_null_and_patch_distinguishes_keep_from_clear() {
        let cleared = CoachRequest::set_effort(
            "mcp-0123456789",
            EffortTarget::Workflow("weekly-note"),
            &SetEffort {
                effort: None,
                said: Some("Stop asking".to_string()),
            },
        );
        assert_eq!(cleared.path(), "/coach/workflows/weekly-note/effort");
        assert_eq!(cleared.body().unwrap()["effort"], Value::Null);
        let keep = CoachRequest::edit_guidance(
            "mcp-0123456789",
            "weekly-note/g1",
            &EditGuidance {
                expected_version: 2,
                ..EditGuidance::default()
            },
        );
        assert!(keep.body().unwrap().get("applies_when").is_none());
        let clear = CoachRequest::edit_guidance(
            "mcp-0123456789",
            "weekly-note/g1",
            &EditGuidance {
                expected_version: 2,
                applies_when: FieldChange::Clear,
                ..EditGuidance::default()
            },
        );
        assert_eq!(clear.body().unwrap()["applies_when"], Value::Null);
    }

    #[test]
    fn responses_unwrap_the_envelope_and_see_replays() {
        let fresh = map_response(&raw(
            201,
            json!({"success": true, "data": {"ref": "a"}}),
            &[],
        ))
        .unwrap();
        assert!(!fresh.replayed);
        assert_eq!(fresh.data, json!({"ref": "a"}));
        let replay = map_response(&raw(
            200,
            json!({"success": true, "data": {"ref": "a"}}),
            &[("idempotent-replayed", "true")],
        ))
        .unwrap();
        assert!(replay.replayed);
    }

    #[test]
    fn errors_keep_their_stable_codes() {
        let conflict = map_response(&raw(
            409,
            json!({"success": false, "data": null, "error": {
                "code": "version_conflict",
                "message": "version_conflict: the guidance changed",
                "details": {"current_version": 3}
            }}),
            &[],
        ))
        .unwrap_err();
        assert_eq!(conflict.code, "version_conflict");
        assert_eq!(conflict.message, "the guidance changed");
        assert_eq!(conflict.kind(), CoachErrorKind::VersionConflict);
        assert_eq!(conflict.current_version(), Some(3));

        let scope = map_response(&raw(
            403,
            json!({"success": false, "error": {"code": "insufficient_scope", "message": "insufficient_scope: needs write:coach"}}),
            &[("www-authenticate", "Bearer error=\"insufficient_scope\", scope=\"write:coach\"")],
        ))
        .unwrap_err();
        assert!(scope.needs_sign_in());
        assert_eq!(scope.required_scope().as_deref(), Some("write:coach"));

        // A code only in the message, or only in the header, is still found.
        let leading = map_response(&raw(
            422,
            json!({"error": {"message": "media_not_ready: still processing"}}),
            &[],
        ))
        .unwrap_err();
        assert_eq!(leading.code, "media_not_ready");
        assert_eq!(leading.kind(), CoachErrorKind::Unprocessable);
        let from_header = map_response(&raw(
            409,
            json!({"error": "busy"}),
            &[
                ("x-contextstream-error-code", "operation_in_progress"),
                ("retry-after", "2"),
            ],
        ))
        .unwrap_err();
        assert_eq!(from_header.kind(), CoachErrorKind::InProgress);
        assert_eq!(from_header.retry_after, Some(2));
        assert!(from_header.is_retryable());

        for (code, kind) in [
            ("not_found", CoachErrorKind::NotFound),
            ("operation_id_conflict", CoachErrorKind::OperationIdConflict),
            ("coach_not_enabled", CoachErrorKind::NotEnabled),
            ("validation_failed", CoachErrorKind::Validation),
            ("payload_too_large", CoachErrorKind::PayloadTooLarge),
            ("ambiguous_reference", CoachErrorKind::Ambiguous),
            (
                "confirmation_required",
                CoachErrorKind::ConfirmationRequired,
            ),
            ("job_not_open", CoachErrorKind::Conflict),
            ("media_kind_mismatch", CoachErrorKind::Unprocessable),
        ] {
            let error = CoachApiError {
                status: 400,
                code: code.to_string(),
                message: String::new(),
                details: None,
                www_authenticate: None,
                retry_after: None,
            };
            assert_eq!(error.kind(), kind, "{code}");
        }
    }

    #[test]
    fn views_drop_internal_ids() {
        let detail: OutputDetail = serde_json::from_value(json!({
            "id": "11111111-1111-4111-8111-111111111111",
            "ref": "weekly-note#1/v2",
            "job_ref": "weekly-note#1",
            "workflow_ref": "weekly-note",
            "version": 2,
            "kind": "text",
            "body": "Hello",
            "content_id": null,
            "source_output": {"id": "22222222-2222-4222-8222-222222222222", "ref": "weekly-note#1/v1"},
            "carried": [{"type": "feedback", "id": "33333333-3333-4333-8333-333333333333", "ref": "weekly-note#1/f1", "how": "applied"}],
            "context": {"etag": "abc", "current": true},
            "feedback": []
        }))
        .unwrap();
        assert_eq!(
            detail.output.source_output_ref().as_deref(),
            Some("weekly-note#1/v1")
        );
        assert_eq!(detail.output.context_current(), Some(true));
        let serialized = serde_json::to_string(&detail).unwrap();
        assert!(!serialized.contains("1111-4111"), "{serialized}");
        assert!(!serialized.contains("2222-4222"), "{serialized}");
        assert!(!serialized.contains("3333-4333"), "{serialized}");
    }

    #[test]
    fn json_exports_lose_their_ids() {
        let export = json!({
            "workflow": {"id": "x", "ref": "weekly-note", "workspace_id": "y"},
            "jobs": [{"job": {"id": "z", "ref": "weekly-note#1"},
                      "outputs": [{"output": {"media": {"content_id": "c", "url": "https://store/k?X-Amz-Signature=s", "download_url_path": "/api/v1/media/c/download-url"}}}]}]
        });
        let clean = export_without_ids(&export);
        let text = clean.to_string();
        for leaked in [
            "\"id\"",
            "workspace_id",
            "content_id",
            "download_url_path",
            "X-Amz",
        ] {
            assert!(!text.contains(leaked), "{leaked} in {text}");
        }
        assert_eq!(clean["workflow"]["ref"], "weekly-note");
    }
}
