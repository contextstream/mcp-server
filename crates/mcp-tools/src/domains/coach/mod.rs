//! Agent Coach Kit tools: a person coaches an agent on recurring work.
//!
//! A **workflow** has a versioned goal and **jobs**, each with an objective.
//! The agent saves **output versions** (text, a link or media); the person's
//! **feedback** is kept verbatim on the exact version, reusable (it becomes
//! editable **guidance**) or for that job only.
//!
//! Three tools, served on their own `/coach` surface:
//!
//! - `workflow` (read-only): capabilities, list, get, context, history,
//!   compare, metrics, export.
//! - `workflow_save` (append-only writes): every action carries an operation
//!   id, derived from the call unless given, so a retry never saves twice.
//! - `workflow_delete` (destructive, so the host asks the person first).
//!
//! Every request and response mapping lives in `mcp_client::coach`. Replies
//! name things only by readable reference, fence the person's data, keep
//! "You said" apart from "Coach's reading", and every write ends with one
//! status line: `[COACH_SAVED]`, `[COACH_ALREADY_SAVED]`, `[COACH_CONFLICT]`
//! or `[COACH_NOT_SAVED]`.

mod format;
mod refs;
mod schema;
#[cfg(test)]
mod tests;
pub mod widget;

pub use widget::{
    descriptor_extras, read_widget_resource, result_meta, security_schemes, widget_html,
    widget_resource, WIDGET_MIME, WIDGET_URI,
};

use async_trait::async_trait;
use mcp_client::coach::{
    derive_operation_id, export_without_ids, is_valid_operation_id, sub_operation_id, AddFeedback,
    Capabilities, Carried, CloseJob, ClosedJob, CoachApiError, CoachErrorKind, CoachFailure,
    CoachRequest, CoachResponse, Comparison, ContextPacket, CreateWorkflow, DeletedWorkflow,
    EditGuidance, Effort, EffortReceipt, EffortTarget, EffortView, ExportFormat, FeedbackView,
    FieldChange, GoalView, GuidanceStatus, GuidanceVersionView, GuidanceView, ImportedGuidance,
    JobDetail, JobView, ListFeedback, ListWorkflows, MarkdownExport, MediaKind, Metrics,
    OutputContent, OutputDetail, OutputView, Page, PatchedGuidance, RecordedFeedback, RefineGoal,
    RescopedFeedback, RetractedFeedback, SaveOutput, SetEffort, StartJob, Upload, UploadReceipt,
    Verdict, VerdictValue, VerdictView, WorkflowDetail, WorkflowView,
};
use mcp_client::ContextStreamClient;
use mcp_types::{
    tool::{ToolMetadata, ToolResult},
    Error, Result,
};
use serde::{Deserialize, Deserializer};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::registry::ToolHandler;
use format::{
    changes_body, effort_line, feedback_body, feedback_item, goal_body, guidance_body,
    guidance_item, guidance_version_item, job_line, kind_label, media_line, open_items_body,
    open_items_list, output_content, output_head, packet_feedback_item, packet_guidance_item,
    packet_output_item, preview, recent_jobs_body, scope_label, status_line, verdict_line,
    workflow_line, Out, SaveStatus, PREVIEW_CHARS,
};
use refs::RefKind;
use widget::{
    suggestions, Card, CardChange, CardCompare, CardEffort, CardFeedback, CardItem, CardJob,
    CardVersion, CardWorkflow, CompareSide,
};

/// The read-only tool.
pub const WORKFLOW_TOOL: &str = "workflow";
/// The append-only write tool.
pub const WORKFLOW_SAVE_TOOL: &str = "workflow_save";
/// The destructive delete tool.
pub const WORKFLOW_DELETE_TOOL: &str = "workflow_delete";
/// The coach surface: exactly these three tools.
pub const COACH_TOOLS: &[&str] = &[WORKFLOW_TOOL, WORKFLOW_SAVE_TOOL, WORKFLOW_DELETE_TOOL];

pub(crate) const READ_ACTIONS: &[&str] = &[
    "capabilities",
    "list",
    "get",
    "context",
    "history",
    "compare",
    "metrics",
    "export",
];
pub(crate) const WRITE_ACTIONS: &[&str] = &[
    "create",
    "refine_goal",
    "start_job",
    "save_output",
    "add_feedback",
    "rescope_feedback",
    "retract_feedback",
    "edit_guidance",
    "import_guidance",
    "set_effort",
    "close_job",
    "reopen_job",
    "cancel_job",
    "record_verdict",
];
pub(crate) const DELETE_ACTIONS: &[&str] = &["delete"];

/// Writes the person may repeat later with the same words; their derived
/// operation ids include a time window of this many minutes.
const REPEATABLE_WINDOW_MINUTES: u64 = 10;
const DEFAULT_LIMIT: i64 = 20;
const MAX_LIMIT: i64 = 100;
const DEFAULT_PUBLIC_MCP_ORIGIN: &str = "https://mcp.contextstream.io";

/// What a host connected to `/coach` is told at `initialize`.
pub const COACH_SERVER_INSTRUCTIONS: &str = "Agent Coach Kit: the person coaches you on work they ask for again and again. \
Use `workflow` to read (start a job's draft from workflow(action=\"context\")), `workflow_save` to save, and `workflow_delete` only when the person asks to delete a whole workflow. \
Save every reaction the person gives to a version, verbatim, without being asked; say whether you kept it for this job only or as reusable, and let them change it. \
Only the person's words become feedback or guidance. Show what changed and what still needs attention. \
Never claim a save unless the reply ends with [COACH_SAVED] or [COACH_ALREADY_SAVED], and never invent approval, facts or scores. \
Coaching permission is not permission to publish, share, spend money or train models.";

/// Whether `name` is one of the coach tools.
pub fn is_coach_tool(name: &str) -> bool {
    COACH_TOOLS.contains(&name)
}

// ============================================================================
// Inputs
// ============================================================================

/// An integer that may arrive as a JSON number or a numeric string.
fn lenient_i64<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<i64>, D::Error> {
    Ok(match Option::<Value>::deserialize(deserializer)? {
        Some(Value::Number(number)) => number
            .as_i64()
            .or_else(|| number.as_f64().map(|f| f as i64)),
        Some(Value::String(text)) => text.trim().parse().ok(),
        _ => None,
    })
}

/// A boolean that may arrive as `true`/`false` or as a string.
fn lenient_bool<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<bool>, D::Error> {
    Ok(match Option::<Value>::deserialize(deserializer)? {
        Some(Value::Bool(value)) => Some(value),
        Some(Value::String(text)) => match text.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" => Some(true),
            "false" | "no" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    })
}

/// A list that may arrive as one string.
fn lenient_strings<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<String>, D::Error> {
    Ok(match Option::<Value>::deserialize(deserializer)? {
        Some(Value::Array(items)) => items
            .into_iter()
            .filter_map(|item| item.as_str().map(str::to_string))
            .collect(),
        Some(Value::String(text)) => text
            .split(',')
            .map(str::trim)
            .filter(|item| !item.is_empty())
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    })
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct ReadInput {
    action: String,
    #[serde(rename = "ref")]
    reference: Option<String>,
    workflow: Option<String>,
    job: Option<String>,
    output: Option<String>,
    feedback: Option<String>,
    guidance: Option<String>,
    a: Option<String>,
    b: Option<String>,
    kind: Option<String>,
    status: Option<String>,
    format: Option<String>,
    #[serde(deserialize_with = "lenient_i64")]
    limit: Option<i64>,
}

impl ReadInput {
    fn target(&self) -> Option<&str> {
        [
            &self.reference,
            &self.output,
            &self.feedback,
            &self.guidance,
            &self.job,
            &self.workflow,
        ]
        .into_iter()
        .filter_map(|value| value.as_deref())
        .map(str::trim)
        .find(|value| !value.is_empty())
    }

    fn limit(&self) -> u32 {
        self.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as u32
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct FileInput {
    download_url: String,
    file_id: Option<String>,
    mime_type: Option<String>,
    file_name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct CarriedInput {
    #[serde(rename = "ref", alias = "reference")]
    reference: String,
    how: String,
    note: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct SaveInput {
    action: String,
    #[serde(rename = "ref")]
    reference: Option<String>,
    workflow: Option<String>,
    job: Option<String>,
    output: Option<String>,
    feedback: Option<String>,
    guidance: Option<String>,
    name: Option<String>,
    goal: Option<String>,
    reason: Option<String>,
    audience: Option<String>,
    output_kind: Option<String>,
    #[serde(deserialize_with = "lenient_i64")]
    expected_version: Option<i64>,
    objective: Option<String>,
    label: Option<String>,
    text: Option<String>,
    url: Option<String>,
    file: Option<FileInput>,
    media_kind: Option<String>,
    title: Option<String>,
    caption: Option<String>,
    revises: Option<String>,
    context_etag: Option<String>,
    carry_note: Option<String>,
    carried: Vec<CarriedInput>,
    kind: Option<String>,
    scope: Option<String>,
    #[serde(alias = "said")]
    words: Option<String>,
    interpretation: Option<String>,
    #[serde(deserialize_with = "lenient_bool")]
    picked: Option<bool>,
    excerpt: Option<String>,
    #[serde(deserialize_with = "lenient_i64")]
    time_start_ms: Option<i64>,
    #[serde(deserialize_with = "lenient_i64")]
    time_end_ms: Option<i64>,
    repeat_of: Option<String>,
    #[serde(deserialize_with = "lenient_bool")]
    reports_leak: Option<bool>,
    applies_when: Option<String>,
    change: Option<String>,
    #[serde(deserialize_with = "lenient_strings")]
    sources: Vec<String>,
    effort: Option<String>,
    used: Option<String>,
    verdict: Option<String>,
    note: Option<String>,
    baseline: Option<String>,
    candidate: Option<String>,
    operation_id: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
struct DeleteInput {
    #[serde(alias = "ref")]
    workflow: Option<String>,
    confirm: Option<String>,
    operation_id: Option<String>,
}

fn text_field(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn required_text(value: &Option<String>, field: &str, action: &str) -> Result<String> {
    text_field(value).ok_or_else(|| Error::Validation(format!("{field} is required for {action}")))
}

fn optional_ref(value: &Option<String>, field: &str, kinds: &[RefKind]) -> Result<Option<String>> {
    match text_field(value) {
        None => Ok(None),
        Some(raw) => Ok(Some(refs::expect(Some(&raw), field, kinds)?.to_string())),
    }
}

fn choice(value: &Option<String>, field: &str, allowed: &[&str]) -> Result<Option<String>> {
    match text_field(value) {
        None => Ok(None),
        Some(raw) => {
            let normalized = raw.to_ascii_lowercase().replace([' ', '-'], "_");
            if allowed.contains(&normalized.as_str()) {
                Ok(Some(normalized))
            } else {
                Err(Error::Validation(format!(
                    "{field} must be one of {}",
                    allowed.join(", ")
                )))
            }
        }
    }
}

// ============================================================================
// Replies
// ============================================================================

/// A reply before ids are masked and it becomes a tool result.
struct Reply {
    text: String,
    card: Card,
    is_error: bool,
    meta: Option<Value>,
}

impl Reply {
    fn read(text: String, card: Card) -> Self {
        Self {
            text,
            card,
            is_error: false,
            meta: None,
        }
    }

    fn into_result(mut self) -> ToolResult {
        // A card with nothing else to show says what happened, in words.
        if self.card.message.is_none() {
            if let Some(line) = &self.card.status_line {
                let what = line
                    .split_once(' ')
                    .map(|(_, rest)| rest)
                    .unwrap_or_default()
                    .split(" operation_id:")
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .to_string();
                self.card.message = Some(what);
            }
        }
        let text = refs::mask_uuids(&self.text);
        let structured = refs::mask_uuids_in(&self.card.to_value());
        let mut result = ToolResult::with_structured(text, structured);
        result.is_error = self.is_error;
        if matches!(self.card.view, "job_card" | "compare" | "list" | "workflow") {
            result = result.with_meta(widget::result_meta());
        }
        if let Some(meta) = self.meta {
            result = result.with_meta(meta);
        }
        result
    }
}

/// The URL of the `/coach` protected-resource metadata, for a sign-in
/// challenge.
fn coach_resource_metadata_url() -> String {
    let origin = std::env::var("CONTEXTSTREAM_PUBLIC_MCP_ORIGIN")
        .ok()
        .map(|value| value.trim().trim_end_matches('/').to_string())
        .filter(|value| value.starts_with("https://") || value.starts_with("http://"))
        .unwrap_or_else(|| DEFAULT_PUBLIC_MCP_ORIGIN.to_string());
    format!("{origin}/.well-known/oauth-protected-resource/coach")
}

/// `_meta` that makes ChatGPT ask the person to sign in again.
fn sign_in_meta(error: &CoachApiError, needed_scope: &str) -> Value {
    let insufficient = error.kind() == CoachErrorKind::InsufficientScope;
    let scope = error
        .required_scope()
        .unwrap_or_else(|| needed_scope.to_string());
    let (code, description) = if insufficient {
        (
            "insufficient_scope",
            "Agent Coach Kit needs your permission to continue",
        )
    } else {
        (
            "invalid_token",
            "Sign in to ContextStream to use Agent Coach Kit",
        )
    };
    json!({
        "mcp/www_authenticate": [format!(
            "Bearer resource_metadata=\"{}\", error=\"{code}\", error_description=\"{description}\", scope=\"{scope}\"",
            coach_resource_metadata_url()
        )]
    })
}

/// What went wrong and what to do next, for a write.
fn explain_failure(
    error: &CoachApiError,
    operation_id: Option<&str>,
) -> (SaveStatus, String, String) {
    let retry_same = match operation_id {
        Some(id) => {
            format!("retry the same call; it reuses operation_id {id} and will not save twice")
        }
        None => "retry the same call".to_string(),
    };
    let message = error.message.trim().trim_end_matches('.').to_string();
    match (error.kind(), error.code.as_str()) {
        (CoachErrorKind::Unauthorized, _) => (
            SaveStatus::NotSaved,
            "ContextStream needs the person to sign in again".to_string(),
            "ask them to reconnect when prompted, then retry the same call".to_string(),
        ),
        (CoachErrorKind::InsufficientScope, _) => (
            SaveStatus::NotSaved,
            format!(
                "This sign-in does not allow {}",
                error.required_scope().unwrap_or_else(|| "this".to_string())
            ),
            "ask the person to grant access when prompted, then retry the same call".to_string(),
        ),
        (CoachErrorKind::NotEnabled, _) => (
            SaveStatus::NotSaved,
            "Agent Coach Kit is not turned on for this ContextStream account yet".to_string(),
            "tell the person; nothing can be saved until it is enabled".to_string(),
        ),
        (CoachErrorKind::NotFound, _) => (
            SaveStatus::NotSaved,
            format!("Not found among the person's workflows ({message})"),
            "check the reference with workflow(action=\"get\") or workflow(action=\"list\"), then retry".to_string(),
        ),
        (CoachErrorKind::VersionConflict, _) => {
            let current = error
                .current_version()
                .map(|version| format!(" (now version {version})"))
                .unwrap_or_default();
            (
                SaveStatus::Conflict,
                format!("It changed since it was read{current}; nothing changed"),
                format!(
                    "read it again with workflow(action=\"get\"), show the person the current text, and retry with expected_version{} only if they still want this change",
                    error
                        .current_version()
                        .map(|version| format!("={version}"))
                        .unwrap_or_default()
                ),
            )
        }
        (CoachErrorKind::OperationIdConflict, _) => (
            SaveStatus::Conflict,
            "That operation_id was already used for a different request; nothing changed".to_string(),
            "retry without operation_id so a new one is derived".to_string(),
        ),
        (CoachErrorKind::InProgress, _) => (
            SaveStatus::NotSaved,
            "The same save is still in progress".to_string(),
            format!("wait a few seconds and {retry_same}"),
        ),
        (CoachErrorKind::PayloadTooLarge, _) => (
            SaveStatus::NotSaved,
            format!("Too large ({message})"),
            "shorten it (feedback up to 4,000 characters, output text up to 12,000) or attach it as a file".to_string(),
        ),
        (CoachErrorKind::Ambiguous, _) => {
            let names = error
                .candidates()
                .into_iter()
                .map(|(name, slug)| format!("{name} ({slug})"))
                .collect::<Vec<_>>()
                .join(", ");
            (
                SaveStatus::NotSaved,
                format!("That reference names workflows in more than one workspace: {names}"),
                "tell the person; open the workflow from the workspace it belongs to".to_string(),
            )
        }
        (CoachErrorKind::ConfirmationRequired, _) => (
            SaveStatus::NotSaved,
            message,
            "pass confirm exactly equal to the workflow's slug, after the person asked to delete it".to_string(),
        ),
        (CoachErrorKind::Conflict, code) => {
            let next = match code {
                "workflow_name_taken" => "find it with workflow(action=\"list\"), or ask the person for another name",
                "workflow_archived" => "tell the person the workflow is archived",
                "job_not_open" => "reopen_job if the person wants to continue this job, or start_job for a new one",
                "job_has_outputs" => "the objective is fixed once a version exists; start a new job instead",
                "output_limit_reached" => "close this job and start a new one",
                "too_much_active_guidance" => "ask the person which guidance to retire (edit_guidance change=retire)",
                "scope_unchanged" => "nothing to do: it already has that scope",
                "feedback_already_retracted" | "source_retracted" => "nothing to do: it was already retracted",
                "guidance_already_imported" => "nothing to do: it was imported before",
                _ => "tell the person what the conflict is",
            };
            (
                SaveStatus::Conflict,
                format!("{message}; nothing changed"),
                next.to_string(),
            )
        }
        (CoachErrorKind::Unprocessable | CoachErrorKind::Validation, code) => {
            let next = match code {
                "media_not_ready" => "wait for the upload to finish processing, then retry the same call",
                "media_kind_mismatch" => "pass media_kind matching the file",
                "unknown_context_etag" => "read workflow(action=\"context\") again and pass its context_etag",
                "carried_item_out_of_scope" => "carry only this workflow's guidance and this job's own feedback",
                "picked_must_be_direction" => "save a picked suggestion with kind=direction",
                "one_off_feedback_on_closed_job" => "reopen the job, or save it as reusable if the person wants that",
                "time_range_needs_media" | "outside_media" => "give a time range only for audio or video, within its length",
                "nothing_to_judge" | "no_baseline" => "save a version first, or name the baseline and candidate",
                "outputs_in_different_workflows" => "compare versions of one workflow",
                "unknown_reference" | "source_output_not_in_job" => "check the references with workflow(action=\"get\")",
                "guidance_not_importable" => "import active guidance from the person's other workflows only",
                _ => "fix the request and retry",
            };
            (SaveStatus::NotSaved, message, next.to_string())
        }
        (CoachErrorKind::Other, _) if error.status == 403 => (
            SaveStatus::NotSaved,
            format!("ContextStream refused this sign-in ({message})"),
            "ask the person to sign in with their own ContextStream account, then retry the same call"
                .to_string(),
        ),
        (CoachErrorKind::Unavailable | CoachErrorKind::Other, _) => (
            SaveStatus::NotSaved,
            format!("ContextStream could not save it right now ({message})"),
            retry_same,
        ),
    }
}

// ============================================================================
// The tools
// ============================================================================

/// Shared by the three tools.
#[derive(Clone)]
pub struct Coach {
    client: ContextStreamClient,
    clock: fn() -> i64,
}

fn system_clock() -> i64 {
    chrono::Utc::now().timestamp()
}

impl Coach {
    pub fn new(client: ContextStreamClient) -> Self {
        Self {
            client,
            clock: system_clock,
        }
    }

    #[cfg(test)]
    fn with_clock(client: ContextStreamClient, clock: fn() -> i64) -> Self {
        Self { client, clock }
    }

    async fn send(
        &self,
        request: &CoachRequest,
    ) -> std::result::Result<CoachResponse, CoachFailure> {
        self.client.coach_send(request).await
    }

    /// The operation id: the caller's when valid, else derived from the call.
    fn operation_id(
        &self,
        tool: &str,
        action: &str,
        explicit: &Option<String>,
        input: &Value,
        repeatable: bool,
    ) -> Result<String> {
        if let Some(id) = text_field(explicit) {
            if !is_valid_operation_id(&id) {
                return Err(Error::Validation(
                    "operation_id must be 8-128 characters of letters, digits and . _ : -"
                        .to_string(),
                ));
            }
            return Ok(id);
        }
        let mut normalized = input.clone();
        // A host may hand over a fresh download link for the same file.
        if let Some(file) = normalized.get_mut("file").and_then(Value::as_object_mut) {
            if file.get("file_id").and_then(Value::as_str).is_some() {
                file.remove("download_url");
            }
        }
        let bucket = repeatable.then(|| (REPEATABLE_WINDOW_MINUTES, (self.clock)()));
        Ok(derive_operation_id(tool, action, &normalized, bucket))
    }

    /// A failed read.
    fn read_failure(&self, failure: CoachFailure, what: &str) -> Reply {
        let mut card = Card::new("status", "failed");
        let (text, meta) = match &failure {
            CoachFailure::Api(error) => {
                let meta = error
                    .needs_sign_in()
                    .then(|| sign_in_meta(error, mcp_client::coach::READ_SCOPE));
                let text = match error.kind() {
                    CoachErrorKind::NotEnabled => {
                        "Agent Coach Kit is not turned on for this ContextStream account yet.".to_string()
                    }
                    CoachErrorKind::Unauthorized => {
                        "ContextStream needs the person to sign in again before reading coaching workflows.".to_string()
                    }
                    CoachErrorKind::InsufficientScope => format!(
                        "This sign-in does not allow reading coaching workflows ({}). Ask the person to grant access.",
                        error.required_scope().unwrap_or_else(|| "read:coach".to_string())
                    ),
                    _ => format!("Could not read {what}: {} ({}).", error.message, error.code),
                };
                (text, meta)
            }
            CoachFailure::Transport(message) => (
                format!("Could not reach ContextStream to read {what} ({message}). Retry shortly."),
                None,
            ),
        };
        card.message = Some(text.clone());
        Reply {
            text,
            card,
            is_error: true,
            meta,
        }
    }

    /// A failed write: ends with its status line.
    fn write_failure(
        &self,
        failure: CoachFailure,
        attempted: &str,
        operation_id: Option<&str>,
        needed_scope: &str,
    ) -> Reply {
        let (status, why, next, meta, state) = match &failure {
            CoachFailure::Api(error) => {
                let (status, why, next) = explain_failure(error, operation_id);
                let meta = error
                    .needs_sign_in()
                    .then(|| sign_in_meta(error, needed_scope));
                (status, why, next, meta, status.state())
            }
            CoachFailure::Transport(message) => (
                SaveStatus::NotSaved,
                format!("Not confirmed: the request may not have reached ContextStream ({message})"),
                match operation_id {
                    Some(id) => format!("retry the same call; it reuses operation_id {id}, so if it was saved, the reply says it was already saved"),
                    None => "retry the same call".to_string(),
                },
                None,
                "pending",
            ),
        };
        let line = status_line(status, &format!("{attempted}: {why}"), operation_id, &next);
        let mut card = Card::new("status", state);
        card.status_line = Some(line.clone());
        card.operation_id = operation_id.map(str::to_string);
        Reply {
            text: line,
            card,
            is_error: true,
            meta,
        }
    }

    /// A write refused before anything was sent.
    fn not_sent(&self, attempted: &str, error: Error) -> Reply {
        let message = match error {
            Error::Validation(message) => message,
            other => other.user_facing_message(),
        };
        let line = status_line(
            SaveStatus::NotSaved,
            &format!("{attempted}: nothing was sent ({message})"),
            None,
            "fix the arguments and call again",
        );
        let mut card = Card::new("status", "failed");
        card.status_line = Some(line.clone());
        Reply {
            text: line,
            card,
            is_error: true,
            meta: None,
        }
    }

    fn saved_status(response: &CoachResponse) -> SaveStatus {
        if response.replayed {
            SaveStatus::AlreadySaved
        } else {
            SaveStatus::Saved
        }
    }

    // ---- shared card pieces --------------------------------------------------

    async fn job_and_workflow(&self, job: &str) -> (Option<JobDetail>, Option<WorkflowDetail>) {
        let workflow = refs::parse(job).map(|parsed| parsed.workflow());
        let job_request = CoachRequest::get_job(job);
        let workflow_request = workflow.as_deref().map(CoachRequest::get_workflow);
        let (job_detail, workflow_detail) = tokio::join!(self.send(&job_request), async {
            match &workflow_request {
                Some(request) => Some(self.send(request).await),
                None => None,
            }
        });
        (
            job_detail.ok().map(|response| response.view::<JobDetail>()),
            workflow_detail
                .and_then(|outcome| outcome.ok())
                .map(|response| response.view::<WorkflowDetail>()),
        )
    }

    fn card_workflow(workflow: &WorkflowView) -> CardWorkflow {
        CardWorkflow {
            reference: workflow.reference().to_string(),
            name: workflow.name.clone(),
            goal_version: (workflow.goal_version > 0).then_some(workflow.goal_version),
            goal: workflow
                .goal
                .as_ref()
                .map(|goal| preview(&goal.goal, 600))
                .filter(|goal| !goal.is_empty()),
            job_count: Some(workflow.job_count),
            status: (!workflow.status.is_empty()).then(|| workflow.status.clone()),
        }
    }

    fn card_job(job: &JobView) -> CardJob {
        CardJob {
            reference: job.reference.clone(),
            objective: preview(&job.objective, 400),
            status: job.status.clone(),
        }
    }

    fn card_effort(effort: Option<&EffortView>) -> Option<CardEffort> {
        effort
            .filter(|effort| !effort.effort.is_empty())
            .map(|effort| CardEffort {
                effort: effort.effort.clone(),
                said: effort.said.clone(),
                for_this_job: effort.for_this_job == Some(true),
            })
    }

    fn card_version(output: &OutputView) -> CardVersion {
        CardVersion {
            reference: output.reference.clone(),
            kind: output.kind.clone(),
            preview: output
                .body
                .as_deref()
                .map(|body| preview(body, PREVIEW_CHARS))
                .filter(|body| !body.is_empty()),
            url: output.url.clone(),
            media: output.media.as_ref().map(media_line),
        }
    }

    fn card_changes(output: &OutputView) -> Vec<CardChange> {
        output
            .carried
            .iter()
            .map(|item| CardChange {
                how: item.how.replace('_', " "),
                reference: item.reference.clone().unwrap_or_else(|| item.kind.clone()),
                note: item.note.clone(),
            })
            .collect()
    }

    fn card_feedback(feedback: &FeedbackView, guidance_ref: Option<String>) -> CardFeedback {
        CardFeedback {
            reference: feedback.reference.clone(),
            kind: kind_label(&feedback.kind).to_string(),
            scope: scope_label(&feedback.scope).to_string(),
            you_said: feedback.body.clone(),
            reading: feedback.interpretation.clone(),
            guidance_ref,
        }
    }

    /// A job card filled from the job and its workflow, as far as they read.
    fn fill_job_card(
        card: &mut Card,
        job: Option<&JobDetail>,
        workflow: Option<&WorkflowDetail>,
        out: &mut Out,
    ) {
        if let Some(workflow) = workflow {
            card.workflow = Some(Self::card_workflow(&workflow.workflow));
        }
        if let Some(job) = job {
            card.job = Some(Self::card_job(&job.job));
            card.effort = Self::card_effort(
                job.effective_coaching_effort
                    .as_ref()
                    .or(job.job.coaching_effort.as_ref()),
            );
            if let Some(items) = &job.open_items {
                card.attention = open_items_list(items)
                    .into_iter()
                    .map(|(reference, text)| CardItem { reference, text })
                    .collect();
                let body = open_items_body(items);
                if body.is_empty() {
                    out.line(format!(
                        "Still needs attention in {}: nothing open.",
                        job.job.reference
                    ));
                } else {
                    out.line(format!("Still needs attention in {}:", job.job.reference));
                    out.data("open items", &body);
                }
            }
        }
    }

    // ========================================================================
    // workflow (read)
    // ========================================================================

    async fn read(&self, input: ReadInput) -> Result<ToolResult> {
        let action = input.action.trim().to_ascii_lowercase();
        let reply = match action.as_str() {
            "capabilities" => self.capabilities().await,
            "list" => self.list(&input).await,
            "get" => self.get(&input).await?,
            "context" => self.context(&input).await?,
            "history" => self.history(&input).await?,
            "compare" => self.compare(&input).await?,
            "metrics" => self.metrics(&input).await?,
            "export" => self.export(&input).await?,
            _ => {
                return Err(Error::Validation(format!(
                    "Unknown action: {}. Available: {}",
                    input.action,
                    READ_ACTIONS.join(", ")
                )))
            }
        };
        Ok(reply.into_result())
    }

    async fn capabilities(&self) -> Reply {
        match self.send(&CoachRequest::capabilities()).await {
            Ok(response) => {
                let caps: Capabilities = response.view();
                let mut out = Out::new();
                if caps.enabled {
                    out.line("Agent Coach Kit is enabled for this ContextStream account.");
                } else {
                    out.line("Agent Coach Kit is not turned on for this ContextStream account yet (pilot); nothing can be saved until it is.");
                }
                out.line(format!(
                    "Data: a deleted workflow is hidden at once and erased within {} days; deleting the account erases everything. Export any workflow with workflow(action=\"export\").",
                    caps.erase_within_days()
                ));
                let limit = |key: &str| caps.limits.get(key).and_then(Value::as_i64);
                let mut limits = Vec::new();
                for (key, label) in [
                    ("feedback_chars", "feedback"),
                    ("output_text_chars", "output text"),
                    ("goal_chars", "goal"),
                ] {
                    if let Some(value) = limit(key) {
                        limits.push(format!("{label} up to {value} characters"));
                    }
                }
                if let Some(value) = limit("active_guidance") {
                    limits.push(format!("{value} active guidance items per workflow"));
                }
                if !limits.is_empty() {
                    out.line(format!("Limits: {}.", limits.join(", ")));
                }
                out.line("Coaching permission covers saving workflows, versions and the person's feedback; it is not permission to publish, share, spend money or train models.");
                let mut card = Card::new("status", "read");
                card.message = Some(if caps.enabled {
                    "Agent Coach Kit is enabled.".to_string()
                } else {
                    "Agent Coach Kit is not turned on yet.".to_string()
                });
                Reply::read(out.finish(), card)
            }
            Err(CoachFailure::Api(error)) if error.status == 404 => {
                let text = "[COACH_UNAVAILABLE] Agent Coach Kit is not available on this ContextStream server yet.".to_string();
                let mut card = Card::new("status", "failed");
                card.message = Some(text.clone());
                Reply {
                    text,
                    card,
                    is_error: true,
                    meta: None,
                }
            }
            Err(failure) => self.read_failure(failure, "Agent Coach Kit's status"),
        }
    }

    async fn list(&self, input: &ReadInput) -> Reply {
        let status = choice(&input.status, "status", &["active", "archived"])
            .ok()
            .flatten();
        let request = CoachRequest::list_workflows(&ListWorkflows {
            status,
            page: None,
            per_page: Some(input.limit()),
        });
        let page: Page<WorkflowView> = match self.send(&request).await {
            Ok(response) => response.view(),
            Err(failure) => return self.read_failure(failure, "the workflows"),
        };
        let mut out = Out::new();
        let mut card = Card::new("list", "read");
        if page.items.is_empty() {
            out.line("No coaching workflows yet. Create one with workflow_save(action=\"create\", name, goal) once the person describes the recurring work.");
        } else {
            out.line(format!(
                "Coaching workflows ({}{}):",
                page.items.len(),
                page.total
                    .filter(|total| *total > page.items.len() as i64)
                    .map(|total| format!(" of {total}"))
                    .unwrap_or_default()
            ));
            let lines = page
                .items
                .iter()
                .map(workflow_line)
                .collect::<Vec<_>>()
                .join("\n");
            out.data("workflow names", &lines);
            out.line("Read one with workflow(action=\"get\", ref=\"<slug>\").");
            card.workflows = page.items.iter().map(Self::card_workflow).collect();
        }
        Reply::read(out.finish(), card)
    }

    async fn get(&self, input: &ReadInput) -> Result<Reply> {
        let target = refs::expect(
            input.target(),
            "ref",
            &[
                RefKind::Workflow,
                RefKind::Job,
                RefKind::Output,
                RefKind::Feedback,
                RefKind::Guidance,
            ],
        )?;
        let reference = target.to_string();
        Ok(match target.kind {
            RefKind::Workflow => self.get_workflow(&reference).await,
            RefKind::Job => self.get_job(&reference).await,
            RefKind::Output => self.get_output(&reference).await,
            RefKind::Feedback => match self.send(&CoachRequest::get_feedback(&reference)).await {
                Ok(response) => {
                    let feedback: FeedbackView = response.view();
                    let mut out = Out::new();
                    out.data("feedback", &feedback_item(&feedback));
                    out.line(format!(
                            "It is {}. To change that, workflow_save(action=\"rescope_feedback\", feedback=\"{}\", scope=...); to take it back, retract_feedback.",
                            scope_label(&feedback.scope),
                            feedback.reference
                        ));
                    let mut card = Card::new("job_card", "read");
                    card.feedback = Some(Self::card_feedback(&feedback, None));
                    Reply::read(out.finish(), card)
                }
                Err(failure) => self.read_failure(failure, &reference),
            },
            RefKind::Guidance => match self.send(&CoachRequest::get_guidance(&reference)).await {
                Ok(response) => {
                    let guidance: GuidanceView = response.view();
                    let mut out = Out::new();
                    let mut body = guidance_item(&guidance);
                    // Guidance starts as the coach's reading of the person's
                    // words; show their words beside it, kept apart.
                    if let Some(source) = guidance.source_feedback_ref() {
                        if let Ok(response) = self.send(&CoachRequest::get_feedback(&source)).await
                        {
                            let feedback: FeedbackView = response.view();
                            if !feedback.body.is_empty() {
                                body.push_str(&format!("Source ({source}):\n"));
                                body.push_str(&format::words_and_reading(
                                    &feedback.body,
                                    feedback.interpretation.as_deref(),
                                ));
                            }
                        }
                    }
                    out.data("guidance", &body);
                    out.line(format!(
                            "Version {} ({}). Edit with workflow_save(action=\"edit_guidance\", guidance=\"{}\", expected_version={}).",
                            guidance.version,
                            if guidance.status.is_empty() { "active" } else { &guidance.status },
                            guidance.reference,
                            guidance.version
                        ));
                    Reply::read(out.finish(), Card::new("status", "read"))
                }
                Err(failure) => self.read_failure(failure, &reference),
            },
        })
    }

    async fn get_workflow(&self, reference: &str) -> Reply {
        let detail: WorkflowDetail = match self.send(&CoachRequest::get_workflow(reference)).await {
            Ok(response) => response.view(),
            Err(failure) => return self.read_failure(failure, reference),
        };
        let workflow = &detail.workflow;
        let mut out = Out::new();
        out.line(format!("Workflow {}", workflow_line(workflow)));
        if let Some(audience) = workflow
            .audience
            .as_deref()
            .filter(|a| !a.trim().is_empty())
        {
            out.data("audience", audience);
        }
        if let Some(goal) = &workflow.goal {
            out.data("goal", &goal_body(goal));
        }
        out.line(effort_line(workflow.coaching_effort.as_ref()));
        if detail.recent_jobs.is_empty() {
            out.line("No jobs yet.");
        } else {
            out.line("Recent objectives (newest first):");
            out.data("objectives", &recent_jobs_body(&detail.recent_jobs));
        }
        if detail.active_guidance.is_empty() {
            out.line("Active guidance: none yet.");
        } else {
            out.line(format!(
                "Active guidance ({}):",
                detail.active_guidance.len()
            ));
            out.data("guidance", &guidance_body(&detail.active_guidance));
        }
        let mut card = Card::new("workflow", "read");
        card.workflow = Some(Self::card_workflow(workflow));
        card.effort = Self::card_effort(workflow.coaching_effort.as_ref());
        if let Some(items) = &detail.open_items {
            let body = open_items_body(items);
            if body.is_empty() {
                out.line(format!(
                    "Still needs attention in {}: nothing open.",
                    items.job_ref
                ));
            } else {
                out.line(format!("Still needs attention in {}:", items.job_ref));
                out.data("open items", &body);
            }
            card.attention = open_items_list(items)
                .into_iter()
                .map(|(reference, text)| CardItem { reference, text })
                .collect();
        }
        if let Some(latest) = detail.recent_jobs.first() {
            card.job = Some(CardJob {
                reference: latest.reference.clone(),
                objective: preview(&latest.objective, 400),
                status: latest.status.clone(),
            });
        }
        out.line("To start a job: propose its objective from these objectives and the guidance, ask only about changes or missing details, then workflow_save(action=\"start_job\").");
        Reply::read(out.finish(), card)
    }

    async fn get_job(&self, reference: &str) -> Reply {
        let detail: JobDetail = match self.send(&CoachRequest::get_job(reference)).await {
            Ok(response) => response.view(),
            Err(failure) => return self.read_failure(failure, reference),
        };
        let job = &detail.job;
        let mut out = Out::new();
        out.line(format!("Job {}", job_line(job)));
        let mut objective = format!("Objective:\n{}\n", job.objective);
        if let Some(label) = job.label.as_deref().filter(|l| !l.is_empty()) {
            objective.push_str(&format!("Label: {label}\n"));
        }
        out.data("objective", &objective);
        out.line(effort_line(
            detail
                .effective_coaching_effort
                .as_ref()
                .or(job.coaching_effort.as_ref()),
        ));
        if !detail.outputs.is_empty() {
            out.line(format!(
                "Versions: {}.",
                detail
                    .outputs
                    .iter()
                    .map(|o| format!("{} ({})", o.reference, o.kind))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if let Some(previous) = &detail.previous_output {
            out.line(format!(
                "Previous job's latest version: {} (job {}).",
                previous.reference, previous.job_ref
            ));
        }
        if let Some(verdict) = &detail.verdict {
            out.line(verdict_line(verdict));
        }
        let mut card = Card::new("job_card", "read");
        Self::fill_job_card(&mut card, Some(&detail), None, &mut out);
        if let Some(latest) = &detail.latest_output {
            card.version = Some(CardVersion {
                reference: latest.reference.clone(),
                kind: latest.kind.clone(),
                ..CardVersion::default()
            });
        }
        card.suggestions = suggestions(
            Some(&job.reference),
            detail.latest_output.as_ref().map(|o| o.reference.as_str()),
        );
        Reply::read(out.finish(), card)
    }

    async fn get_output(&self, reference: &str) -> Reply {
        let detail: OutputDetail = match self.send(&CoachRequest::get_output(reference)).await {
            Ok(response) => response.view(),
            Err(failure) => return self.read_failure(failure, reference),
        };
        let output = &detail.output;
        let mut out = Out::new();
        out.line(format!("Version {}", output_head(output)));
        out.data(
            "output",
            &output_content(
                &output.kind,
                output.body.as_deref(),
                output.url.as_deref(),
                output.title.as_deref(),
                output.caption.as_deref(),
                output.media.as_ref(),
            ),
        );
        let changes = changes_body(output.carry_note.as_deref(), &output.carried);
        if !changes.is_empty() {
            out.line("What changed (the agent's account):");
            out.line(changes.trim_end());
        }
        if detail.feedback.is_empty() {
            out.line("No feedback on this version yet.");
        } else {
            out.line(format!(
                "Feedback on this version ({}):",
                detail.feedback.len()
            ));
            out.data("feedback", &feedback_body(&detail.feedback));
        }
        let mut card = Card::new("job_card", "read");
        card.version = Some(Self::card_version(output));
        card.carry_note = output.carry_note.clone();
        card.changed = Self::card_changes(output);
        card.job = Some(CardJob {
            reference: output.job_ref.clone(),
            ..CardJob::default()
        });
        card.suggestions = suggestions(Some(&output.job_ref), Some(&output.reference));
        Reply::read(out.finish(), card)
    }

    /// The open job of a workflow (the newest), or the job a ref names.
    async fn resolve_job(
        &self,
        target: &refs::CoachRef,
    ) -> std::result::Result<Option<String>, CoachFailure> {
        if let Some(job) = target.job_ref() {
            return Ok(Some(job));
        }
        let page: Page<JobView> = self
            .send(&CoachRequest::list_jobs(
                &target.workflow(),
                Some("open"),
                Some(1),
            ))
            .await?
            .view();
        Ok(page.items.first().map(|job| job.reference.clone()))
    }

    async fn context(&self, input: &ReadInput) -> Result<Reply> {
        let target = refs::expect(
            input.target(),
            "ref",
            &[
                RefKind::Job,
                RefKind::Workflow,
                RefKind::Output,
                RefKind::Feedback,
            ],
        )?;
        let job = match self.resolve_job(&target).await {
            Ok(Some(job)) => job,
            Ok(None) => {
                let text = format!(
                    "{} has no open job. Propose the next objective from workflow(action=\"get\", ref=\"{}\") and start one with workflow_save(action=\"start_job\").",
                    target.workflow(),
                    target.workflow()
                );
                let mut card = Card::new("status", "read");
                card.message = Some(text.clone());
                return Ok(Reply::read(text, card));
            }
            Err(failure) => return Ok(self.read_failure(failure, &target.to_string())),
        };
        Ok(match self.send(&CoachRequest::job_context(&job)).await {
            Ok(response) => {
                let packet: ContextPacket = response.view();
                let (text, card) = render_packet(&packet);
                Reply::read(text, card)
            }
            Err(failure) => self.read_failure(failure, &format!("the context of {job}")),
        })
    }

    async fn history(&self, input: &ReadInput) -> Result<Reply> {
        let target = refs::expect(
            input.target(),
            "ref",
            &[
                RefKind::Workflow,
                RefKind::Job,
                RefKind::Output,
                RefKind::Feedback,
                RefKind::Guidance,
            ],
        )?;
        let kind = choice(
            &input.kind,
            "kind",
            &["all", "goals", "jobs", "feedback", "guidance", "verdicts"],
        )?
        .unwrap_or_else(|| "all".to_string());
        let wants = |section: &str| kind == "all" || kind == section;
        let mut out = Out::new();
        let mut card = Card::new("status", "read");
        card.message = Some(format!("History of {target}: shown in the chat."));

        if target.kind == RefKind::Guidance {
            let reference = target.to_string();
            return Ok(
                match self.send(&CoachRequest::guidance_history(&reference)).await {
                    Ok(response) => {
                        let page: Page<GuidanceVersionView> = response.view();
                        out.line(format!("History of {reference} (newest first):"));
                        out.data(
                            "guidance versions",
                            &page
                                .items
                                .iter()
                                .map(guidance_version_item)
                                .collect::<Vec<_>>()
                                .join("\n"),
                        );
                        Reply::read(out.finish(), card)
                    }
                    Err(failure) => self.read_failure(failure, &reference),
                },
            );
        }

        let workflow = target.workflow();
        let job = target.job_ref();
        let limit = input.limit();
        let feedback_params = ListFeedback {
            job: job.clone(),
            include_retracted: true,
            per_page: Some(limit),
            ..ListFeedback::default()
        };
        let goals_request = CoachRequest::goal_history(&workflow);
        let jobs_request = CoachRequest::list_jobs(&workflow, None, Some(limit));
        let feedback_request = CoachRequest::list_feedback(&workflow, &feedback_params);
        let guidance_request = CoachRequest::list_guidance(&workflow, None);
        let verdicts_request = job.as_deref().map(CoachRequest::list_verdicts);
        let (goals, jobs, feedback, guidance, verdicts) = tokio::join!(
            async {
                if wants("goals") && job.is_none() {
                    Some(self.send(&goals_request).await)
                } else {
                    None
                }
            },
            async {
                if wants("jobs") && job.is_none() {
                    Some(self.send(&jobs_request).await)
                } else {
                    None
                }
            },
            async {
                if wants("feedback") {
                    Some(self.send(&feedback_request).await)
                } else {
                    None
                }
            },
            async {
                if wants("guidance") && job.is_none() {
                    Some(self.send(&guidance_request).await)
                } else {
                    None
                }
            },
            async {
                match (&verdicts_request, wants("verdicts")) {
                    (Some(request), true) => Some(self.send(request).await),
                    _ => None,
                }
            },
        );
        let scope = job.clone().unwrap_or_else(|| workflow.clone());
        out.line(format!("History of {scope} (newest first; retracted and retired items included, with their reasons)."));
        let mut failures = Vec::new();
        if let Some(outcome) = goals {
            match outcome {
                Ok(response) => {
                    let page: Page<GoalView> = response.view();
                    out.line(format!("Goal versions ({}):", page.items.len()));
                    out.data(
                        "goal versions",
                        &page
                            .items
                            .iter()
                            .map(goal_body)
                            .collect::<Vec<_>>()
                            .join("\n"),
                    );
                }
                Err(failure) => failures.push(("goals", failure)),
            }
        }
        if let Some(outcome) = jobs {
            match outcome {
                Ok(response) => {
                    let page: Page<JobView> = response.view();
                    out.line(format!("Jobs ({}):", page.items.len()));
                    out.data(
                        "objectives",
                        &page
                            .items
                            .iter()
                            .map(|job| format!("{}\nObjective: {}", job_line(job), job.objective))
                            .collect::<Vec<_>>()
                            .join("\n"),
                    );
                }
                Err(failure) => failures.push(("jobs", failure)),
            }
        }
        if let Some(outcome) = feedback {
            match outcome {
                Ok(response) => {
                    let page: Page<FeedbackView> = response.view();
                    out.line(format!("Feedback ({}):", page.items.len()));
                    out.data("feedback", &feedback_body(&page.items));
                }
                Err(failure) => failures.push(("feedback", failure)),
            }
        }
        if let Some(outcome) = guidance {
            match outcome {
                Ok(response) => {
                    let page: Page<GuidanceView> = response.view();
                    out.line(format!("Guidance ({}):", page.items.len()));
                    out.data("guidance", &guidance_body(&page.items));
                }
                Err(failure) => failures.push(("guidance", failure)),
            }
        }
        if let Some(outcome) = verdicts {
            match outcome {
                Ok(response) => {
                    let page: Page<VerdictView> = response.view();
                    out.line(format!("The person's verdicts ({}):", page.items.len()));
                    for verdict in &page.items {
                        out.line(format!(
                            "- revision {}: {}",
                            verdict.revision,
                            verdict_line(verdict)
                        ));
                    }
                }
                Err(failure) => failures.push(("verdicts", failure)),
            }
        }
        if let Some((section, failure)) = failures.into_iter().next() {
            if out.finish().lines().count() <= 1 {
                return Ok(self.read_failure(failure, &format!("the {section} of {scope}")));
            }
            out.line(format!("(The {section} could not be read just now.)"));
        }
        Ok(Reply::read(out.finish(), card))
    }

    async fn compare(&self, input: &ReadInput) -> Result<Reply> {
        let (left, right) = match (text_field(&input.a), text_field(&input.b)) {
            (Some(a), Some(b)) => (
                refs::expect(Some(&a), "a", &[RefKind::Output])?.to_string(),
                refs::expect(Some(&b), "b", &[RefKind::Output])?.to_string(),
            ),
            (Some(_), None) | (None, Some(_)) => {
                return Err(Error::Validation(
                    "compare needs both a and b, or a job ref for its last two versions"
                        .to_string(),
                ))
            }
            (None, None) => {
                let target = refs::expect(
                    input.target(),
                    "ref",
                    &[RefKind::Job, RefKind::Output, RefKind::Workflow],
                )?;
                let job = match self.resolve_job(&target).await {
                    Ok(Some(job)) => job,
                    Ok(None) => {
                        // No open job: the workflow's newest job.
                        let page: Page<JobView> = match self
                            .send(&CoachRequest::list_jobs(&target.workflow(), None, Some(1)))
                            .await
                        {
                            Ok(response) => response.view(),
                            Err(failure) => {
                                return Ok(self.read_failure(failure, &target.to_string()))
                            }
                        };
                        match page.items.first() {
                            Some(job) => job.reference.clone(),
                            None => {
                                return Err(Error::Validation(format!(
                                    "{} has no jobs to compare yet",
                                    target.workflow()
                                )))
                            }
                        }
                    }
                    Err(failure) => return Ok(self.read_failure(failure, &target.to_string())),
                };
                let detail: JobDetail = match self.send(&CoachRequest::get_job(&job)).await {
                    Ok(response) => response.view(),
                    Err(failure) => return Ok(self.read_failure(failure, &job)),
                };
                let mut versions: Vec<_> = detail.outputs.iter().collect();
                versions.sort_by_key(|output| output.version);
                match (versions.len(), &detail.previous_output) {
                    (n, _) if n >= 2 => (
                        versions[n - 2].reference.clone(),
                        versions[n - 1].reference.clone(),
                    ),
                    (1, Some(previous)) => (previous.reference.clone(), versions[0].reference.clone()),
                    _ => {
                        return Err(Error::Validation(format!(
                            "{job} needs two versions (or one, plus a version in an earlier job) to compare; pass a and b to choose them"
                        )))
                    }
                }
            }
        };
        Ok(
            match self.send(&CoachRequest::compare(&left, &right)).await {
                Ok(response) => {
                    let comparison: Comparison = response.view();
                    render_comparison(&comparison, &left, &right)
                }
                Err(failure) => self.read_failure(failure, &format!("{left} and {right}")),
            },
        )
    }

    async fn metrics(&self, input: &ReadInput) -> Result<Reply> {
        let target = refs::expect(input.target(), "ref", &[RefKind::Workflow, RefKind::Job])?;
        let workflow = target.workflow();
        let metrics: Metrics = match self.send(&CoachRequest::metrics(&workflow)).await {
            Ok(response) => response.view(),
            Err(failure) => return Ok(self.read_failure(failure, &workflow)),
        };
        let mut out = Out::new();
        out.line(format!(
            "Coaching measures for {} (counts from real jobs; no quality score):",
            workflow
        ));
        for job in &metrics.jobs {
            let reusable = job.feedback.by_scope.get("workflow").copied().unwrap_or(0);
            let mut line = format!(
                "- {} ({}): {} version{}, {} feedback ({} reusable), repeated corrections {} confirmed / {} likely",
                job.job_ref(),
                job.status,
                job.outputs,
                if job.outputs == 1 { "" } else { "s" },
                job.feedback.total,
                reusable,
                job.repeats.confirmed,
                job.repeats.likely
            );
            if job.reported_leaks > 0 {
                line.push_str(&format!(", {} reported leak(s)", job.reported_leaks));
            }
            if let Some(verdict) = &job.verdict {
                line.push_str(&format!(", the person's verdict: {}", verdict.verdict));
            }
            out.line(line);
        }
        let totals = &metrics.totals;
        out.line(format!(
            "Totals: {} jobs, {} versions, {} feedback, repeated corrections {} confirmed / {} likely.",
            totals.jobs, totals.outputs, totals.feedback, totals.repeats.confirmed, totals.repeats.likely
        ));
        if let (Some(first), Some(second)) = (
            totals.avg_feedback_first_half,
            totals.avg_feedback_second_half,
        ) {
            out.line(format!(
                "Feedback per job: {first:.1} in the first half of the jobs, {second:.1} in the second half (fewer can mean less correcting; ask the person rather than assume)."
            ));
        }
        out.line(format!(
            "Context audit: {} item(s) found outside their job or workflow.",
            metrics.audited_leak_total()
        ));
        let mut card = Card::new("status", "read");
        card.message = Some(format!(
            "Coaching measures for {workflow}: counts from real jobs, no score."
        ));
        Ok(Reply::read(out.finish(), card))
    }

    async fn export(&self, input: &ReadInput) -> Result<Reply> {
        let target = refs::expect(input.target(), "ref", &[RefKind::Workflow])?;
        let workflow = target.workflow();
        let format = match choice(&input.format, "format", &["markdown", "json"])?.as_deref() {
            Some("json") => ExportFormat::Json,
            _ => ExportFormat::Markdown,
        };
        let response = match self.send(&CoachRequest::export(&workflow, format)).await {
            Ok(response) => response,
            Err(failure) => return Ok(self.read_failure(failure, &workflow)),
        };
        let mut out = Out::new();
        out.line(format!(
            "Export of {workflow}: everything saved in it (goals, jobs, versions, feedback, guidance and verdicts). The person can keep this copy; deleting the workflow erases it from ContextStream within 30 days."
        ));
        match format {
            ExportFormat::Markdown => {
                let export: MarkdownExport = response.view();
                if let Some(name) = export.filename {
                    out.line(format!("Suggested file name: {name}"));
                }
                out.data("export (markdown)", &export.markdown);
            }
            ExportFormat::Json => {
                let clean = export_without_ids(&response.data);
                out.data(
                    "export (json)",
                    &serde_json::to_string_pretty(&clean).unwrap_or_default(),
                );
            }
        }
        let mut card = Card::new("status", "read");
        card.message = Some(format!("Export of {workflow}: shown in the chat."));
        Ok(Reply::read(out.finish(), card))
    }

    // ========================================================================
    // workflow_save (append-only writes)
    // ========================================================================

    async fn save(&self, raw: Value) -> Result<ToolResult> {
        let input: SaveInput = match serde_json::from_value(raw.clone()) {
            Ok(input) => input,
            Err(error) => {
                return Ok(self
                    .not_sent("Not saved", Error::Validation(error.to_string()))
                    .into_result())
            }
        };
        let action = input.action.trim().to_ascii_lowercase();
        if !WRITE_ACTIONS.contains(&action.as_str()) {
            return Ok(self
                .not_sent(
                    "Not saved",
                    Error::Validation(format!(
                        "Unknown action: {}. Available: {}",
                        input.action,
                        WRITE_ACTIONS.join(", ")
                    )),
                )
                .into_result());
        }
        let reply = match self.save_action(&action, &input, &raw).await {
            Ok(reply) => reply,
            Err(error) => self.not_sent(&format!("{action} not saved"), error),
        };
        Ok(reply.into_result())
    }

    async fn save_action(&self, action: &str, input: &SaveInput, raw: &Value) -> Result<Reply> {
        let repeatable = matches!(
            action,
            "start_job" | "close_job" | "reopen_job" | "cancel_job" | "set_effort"
        );
        // Validate first, so a refused call never needs an operation id.
        let op = || {
            self.operation_id(
                WORKFLOW_SAVE_TOOL,
                action,
                &input.operation_id,
                raw,
                repeatable,
            )
        };
        match action {
            "create" => self.create(input, op).await,
            "refine_goal" => self.refine_goal(input, op).await,
            "start_job" => self.start_job(input, op).await,
            "save_output" => self.save_output(input, op).await,
            "add_feedback" => self.add_feedback(input, op).await,
            "rescope_feedback" => self.rescope_feedback(input, op).await,
            "retract_feedback" => self.retract_feedback(input, op).await,
            "edit_guidance" => self.edit_guidance(input, op).await,
            "import_guidance" => self.import_guidance(input, op).await,
            "set_effort" => self.set_effort(input, op).await,
            "close_job" | "reopen_job" | "cancel_job" => self.transition(action, input, op).await,
            "record_verdict" => self.record_verdict(input, op).await,
            _ => Err(Error::Validation(format!("Unknown action: {action}"))),
        }
    }

    fn write_scope() -> &'static str {
        mcp_client::coach::WRITE_SCOPE
    }

    async fn create(&self, input: &SaveInput, op: impl Fn() -> Result<String>) -> Result<Reply> {
        let name = required_text(&input.name, "name", "create")?;
        let goal = text_field(&input.goal).ok_or_else(|| {
            Error::Validation(
                "goal is required for create: what good work achieves across every job".to_string(),
            )
        })?;
        let output_kind = choice(
            &input.output_kind,
            "output_kind",
            &[
                "text", "link", "image", "audio", "video", "document", "mixed",
            ],
        )?;
        let op = op()?;
        let request = CoachRequest::create_workflow(
            &op,
            &CreateWorkflow {
                name: name.clone(),
                goal,
                audience: text_field(&input.audience),
                output_kind,
            },
        );
        let response = match self.send(&request).await {
            Ok(response) => response,
            Err(failure) => {
                return Ok(self.write_failure(
                    failure,
                    &format!("Workflow \"{name}\" not created"),
                    Some(&op),
                    Self::write_scope(),
                ))
            }
        };
        let status = Self::saved_status(&response);
        let workflow: WorkflowView = response.view();
        let mut out = Out::new();
        out.line(format!("Workflow {}", workflow_line(&workflow)));
        if let Some(goal) = &workflow.goal {
            out.data("goal", &goal_body(goal));
        }
        let mut card = Card::new("workflow", status.state());
        card.workflow = Some(Self::card_workflow(&workflow));
        let line = status_line(
            status,
            &format!(
                "Created workflow {} (goal v{})",
                workflow.reference(),
                workflow.goal_version.max(1)
            ),
            Some(&op),
            "propose the first job's objective and confirm it with the person, then start_job",
        );
        out.line(line.clone());
        card.status_line = Some(line);
        card.operation_id = Some(op);
        Ok(Reply::read(out.finish(), card))
    }

    async fn refine_goal(
        &self,
        input: &SaveInput,
        op: impl Fn() -> Result<String>,
    ) -> Result<Reply> {
        let workflow = refs::expect(
            input.workflow.as_deref().or(input.reference.as_deref()),
            "workflow",
            &[RefKind::Workflow],
        )?
        .to_string();
        let goal = required_text(&input.goal, "goal", "refine_goal")?;
        let reason = text_field(&input.reason).ok_or_else(|| {
            Error::Validation(
                "reason is required for refine_goal: why the goal changes, in the person's terms"
                    .to_string(),
            )
        })?;
        let expected = input.expected_version.ok_or_else(|| {
            Error::Validation(format!(
                "expected_version is required for refine_goal: the goal version you read with workflow(action=\"get\", ref=\"{workflow}\")"
            ))
        })?;
        let source_feedback = optional_ref(&input.feedback, "feedback", &[RefKind::Feedback])?;
        let op = op()?;
        let request = CoachRequest::refine_goal(
            &op,
            &workflow,
            &RefineGoal {
                expected_goal_version: expected,
                goal,
                reason,
                source_feedback,
            },
        );
        let response = match self.send(&request).await {
            Ok(response) => response,
            Err(failure) => {
                return Ok(self.write_failure(
                    failure,
                    &format!("Goal of {workflow} not refined"),
                    Some(&op),
                    Self::write_scope(),
                ))
            }
        };
        let status = Self::saved_status(&response);
        let goal: GoalView = response.view();
        let mut out = Out::new();
        out.data("goal", &goal_body(&goal));
        let mut card = Card::new("workflow", status.state());
        card.workflow = Some(CardWorkflow {
            reference: workflow.clone(),
            name: workflow.clone(),
            goal_version: Some(goal.version),
            goal: Some(preview(&goal.goal, 600)),
            ..CardWorkflow::default()
        });
        let line = status_line(
            status,
            &format!("Goal of {workflow} is now v{}", goal.version),
            Some(&op),
            "show the person the new goal; later jobs use it",
        );
        out.line(line.clone());
        card.status_line = Some(line);
        card.operation_id = Some(op);
        Ok(Reply::read(out.finish(), card))
    }

    async fn start_job(&self, input: &SaveInput, op: impl Fn() -> Result<String>) -> Result<Reply> {
        let workflow = refs::expect(
            input.workflow.as_deref().or(input.reference.as_deref()),
            "workflow",
            &[RefKind::Workflow],
        )?
        .to_string();
        let objective = text_field(&input.objective).ok_or_else(|| {
            Error::Validation(format!(
                "objective is required for start_job: propose it from workflow(action=\"get\", ref=\"{workflow}\") and confirm it with the person"
            ))
        })?;
        let op = op()?;
        let request = CoachRequest::start_job(
            &op,
            &workflow,
            &StartJob {
                objective,
                label: text_field(&input.label),
            },
        );
        let response = match self.send(&request).await {
            Ok(response) => response,
            Err(failure) => {
                return Ok(self.write_failure(
                    failure,
                    &format!("No job started in {workflow}"),
                    Some(&op),
                    Self::write_scope(),
                ))
            }
        };
        let status = Self::saved_status(&response);
        let job: JobView = response.view();
        let mut out = Out::new();
        let mut card = Card::new("job_card", status.state());
        card.job = Some(Self::card_job(&job));
        match self.send(&CoachRequest::job_context(&job.reference)).await {
            Ok(response) => {
                let packet: ContextPacket = response.view();
                let (text, packet_card) = render_packet(&packet);
                out.line(text);
                card.workflow = packet_card.workflow;
                card.effort = packet_card.effort;
            }
            Err(_) => {
                out.line(format!(
                    "Read its context with workflow(action=\"context\", ref=\"{}\") before writing.",
                    job.reference
                ));
            }
        }
        let line = status_line(
            status,
            &format!("Started job {}", job.reference),
            Some(&op),
            "write the first version from this context, then save it with save_output",
        );
        out.line(line.clone());
        card.status_line = Some(line);
        card.operation_id = Some(op);
        Ok(Reply::read(out.finish(), card))
    }

    async fn save_output(
        &self,
        input: &SaveInput,
        op: impl Fn() -> Result<String>,
    ) -> Result<Reply> {
        let job = refs::expect(
            input.job.as_deref().or(input.reference.as_deref()),
            "job",
            &[RefKind::Job],
        )?
        .to_string();
        let text = input.text.clone().filter(|text| !text.trim().is_empty());
        let url = text_field(&input.url);
        let file = input
            .file
            .clone()
            .filter(|file| !file.download_url.trim().is_empty());
        match (text.is_some(), url.is_some(), file.is_some()) {
            (_, true, true) | (true, false, true) => {
                return Err(Error::Validation(
                    "save_output takes one of text, url or file (text may go with url as the link's snapshot)".to_string(),
                ))
            }
            (false, false, false) => {
                return Err(Error::Validation(
                    "save_output needs the version: text, url, or the file the person attached".to_string(),
                ))
            }
            _ => {}
        }
        if let Some(url) = &url {
            if !url.starts_with("https://") {
                return Err(Error::Validation("url must be an https link".to_string()));
            }
        }
        if let Some(file) = &file {
            if !file.download_url.trim().starts_with("https://") {
                return Err(Error::Validation(
                    "the attached file's download_url must be https".to_string(),
                ));
            }
        }
        let revises = optional_ref(&input.revises, "revises", &[RefKind::Output])?;
        let mut carried = Vec::new();
        for item in &input.carried {
            let reference = refs::expect(
                Some(&item.reference),
                "carried[].ref",
                &[RefKind::Feedback, RefKind::Guidance],
            )?
            .to_string();
            let how = choice(
                &Some(item.how.clone()),
                "carried[].how",
                &["kept", "adapted", "applied", "not_applied"],
            )?
            .unwrap_or_else(|| "applied".to_string());
            carried.push(Carried {
                reference,
                how,
                note: text_field(&item.note),
            });
        }
        let media_kind = match text_field(&input.media_kind) {
            Some(kind) => Some(MediaKind::parse(&kind).ok_or_else(|| {
                Error::Validation("media_kind must be image, audio, video or document".to_string())
            })?),
            None => None,
        };
        let op = op()?;
        let content = if let Some(file) = &file {
            let kind =
                media_kind.unwrap_or_else(|| MediaKind::from_mime(file.mime_type.as_deref()));
            let upload_id = sub_operation_id(&op, "upload");
            let filename = file
                .file_name
                .clone()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| format!("{}-upload", kind.as_str()));
            let upload = CoachRequest::upload(
                &upload_id,
                &job,
                &Upload {
                    kind,
                    filename,
                    mime_type: file.mime_type.clone(),
                    title: text_field(&input.title),
                    source_url: file.download_url.trim().to_string(),
                },
            );
            let receipt: UploadReceipt = match self.send(&upload).await {
                Ok(response) => response.view(),
                Err(failure) => {
                    return Ok(self.write_failure(
                        failure,
                        &format!("The file for {job} was not taken in"),
                        Some(&op),
                        Self::write_scope(),
                    ))
                }
            };
            let Some(content_id) = receipt.content_id.filter(|id| !id.is_empty()) else {
                return Ok(self.write_failure(
                    CoachFailure::Transport("the upload's receipt named no asset".to_string()),
                    &format!("The file for {job} was not taken in"),
                    Some(&op),
                    Self::write_scope(),
                ));
            };
            OutputContent::Media { kind, content_id }
        } else if let Some(url) = url {
            OutputContent::Link {
                url,
                snapshot: text,
            }
        } else {
            OutputContent::Text {
                body: text.unwrap_or_default(),
            }
        };
        let request = CoachRequest::save_output(
            &op,
            &job,
            &SaveOutput {
                content,
                title: text_field(&input.title),
                caption: text_field(&input.caption),
                source_output: revises,
                context_etag: text_field(&input.context_etag),
                carry_note: text_field(&input.carry_note),
                carried,
            },
        );
        let response = match self.send(&request).await {
            Ok(response) => response,
            Err(failure) => {
                return Ok(self.write_failure(
                    failure,
                    &format!("Version for {job} not saved"),
                    Some(&op),
                    Self::write_scope(),
                ))
            }
        };
        let status = Self::saved_status(&response);
        let output: OutputView = response.view();
        let (job_detail, workflow_detail) = self.job_and_workflow(&job).await;
        let mut out = Out::new();
        out.line(format!("Version {}", output_head(&output)));
        let changes = changes_body(output.carry_note.as_deref(), &output.carried);
        if changes.is_empty() {
            out.line("What changed: not stated. Tell the person what changed and which feedback led to it.");
        } else {
            out.line("What changed (the agent's account):");
            out.line(changes.trim_end());
        }
        let mut card = Card::new("job_card", status.state());
        card.version = Some(Self::card_version(&output));
        card.carry_note = output.carry_note.clone();
        card.changed = Self::card_changes(&output);
        Self::fill_job_card(
            &mut card,
            job_detail.as_ref(),
            workflow_detail.as_ref(),
            &mut out,
        );
        if card.job.is_none() {
            card.job = Some(CardJob {
                reference: job.clone(),
                ..CardJob::default()
            });
        }
        let pending = output
            .media
            .as_ref()
            .is_some_and(|media| media.status.as_deref() == Some("pending"));
        if pending {
            card.state = "pending";
            out.line("The media is still being processed; it shows once ready.");
        }
        card.suggestions = suggestions(Some(&job), Some(&output.reference));
        let line = status_line(
            status,
            &format!("Saved {} ({})", output.reference, output.kind),
            Some(&op),
            "show the person this version, what changed and what still needs attention, and save their reaction with add_feedback",
        );
        out.line(line.clone());
        card.status_line = Some(line);
        card.operation_id = Some(op);
        Ok(Reply::read(out.finish(), card))
    }

    async fn add_feedback(
        &self,
        input: &SaveInput,
        op: impl Fn() -> Result<String>,
    ) -> Result<Reply> {
        let output = refs::expect(
            input.output.as_deref().or(input.reference.as_deref()),
            "output",
            &[RefKind::Output],
        )?;
        let output_ref = output.to_string();
        // The person's words exactly as given: never trimmed.
        let words = input
            .words
            .clone()
            .or_else(|| input.text.clone())
            .filter(|words| !words.trim().is_empty())
            .ok_or_else(|| {
                Error::Validation(
                    "words is required for add_feedback: the person's own words, verbatim"
                        .to_string(),
                )
            })?;
        let kind = choice(
            &input.kind,
            "kind",
            &["correction", "partial_approval", "direction"],
        )?
        .ok_or_else(|| {
            Error::Validation(
                "kind is required for add_feedback: correction, partial_approval or direction"
                    .to_string(),
            )
        })?;
        let scope = choice(&input.scope, "scope", &["workflow", "job"])?
            .ok_or_else(|| Error::Validation("scope is required for add_feedback: workflow (reusable) or job (this job only); pick the likely one and tell the person".to_string()))?;
        let picked = input.picked.unwrap_or(false);
        if picked && kind != "direction" {
            return Err(Error::Validation(
                "a suggestion the person picked is saved with kind=direction".to_string(),
            ));
        }
        let time_range = match (input.time_start_ms, input.time_end_ms) {
            (Some(start), Some(end)) if start >= 0 && end > start => Some((start, end)),
            (None, None) => None,
            _ => {
                return Err(Error::Validation(
                    "time_start_ms and time_end_ms go together, with end after start".to_string(),
                ))
            }
        };
        let repeat_of = optional_ref(&input.repeat_of, "repeat_of", &[RefKind::Guidance])?;
        let op = op()?;
        let request = CoachRequest::add_feedback(
            &op,
            &output_ref,
            &AddFeedback {
                kind,
                scope,
                body: words,
                interpretation: text_field(&input.interpretation),
                picked,
                excerpt: input.excerpt.clone().filter(|excerpt| !excerpt.is_empty()),
                time_range_ms: time_range,
                repeat_of_guidance: repeat_of,
                reports_leak: input.reports_leak.unwrap_or(false),
                applies_when: text_field(&input.applies_when),
            },
        );
        let response = match self.send(&request).await {
            Ok(response) => response,
            Err(failure) => {
                return Ok(self.write_failure(
                    failure,
                    &format!("Feedback on {output_ref} not saved"),
                    Some(&op),
                    Self::write_scope(),
                ))
            }
        };
        let status = Self::saved_status(&response);
        let recorded: RecordedFeedback = response.view();
        let feedback = &recorded.feedback;
        let job = output.job_ref().unwrap_or_default();
        let (job_detail, workflow_detail) = self.job_and_workflow(&job).await;
        let mut out = Out::new();
        out.data("feedback", &feedback_item(feedback));
        let guidance_ref = recorded.guidance.as_ref().map(|g| g.reference.clone());
        match (&guidance_ref, feedback.scope.as_str()) {
            (Some(guidance), _) => out.line(format!(
                "Kept as reusable guidance {guidance}: later jobs of this workflow will apply it. Tell the person, in one line, and that they can say \"just this job\" to change it."
            )),
            (None, "job") => out.line(format!(
                "Kept for this job only ({}). Tell the person, in one line, and that they can say \"use that every time\" to make it reusable.",
                feedback.reference
            )),
            _ => out.line(format!("Saved as {}.", feedback.reference)),
        };
        if !recorded.possible_repeats.is_empty() {
            out.line("It may repeat guidance the person already gave (a hint only; ask before treating it as a repeat):");
            out.data(
                "possible repeats",
                &recorded
                    .possible_repeats
                    .iter()
                    .map(|repeat| format!("{}: {}", repeat.reference, preview(&repeat.body, 200)))
                    .collect::<Vec<_>>()
                    .join("\n"),
            );
        }
        let mut card = Card::new("job_card", status.state());
        card.feedback = Some(Self::card_feedback(feedback, guidance_ref));
        card.version = Some(CardVersion {
            reference: output_ref.clone(),
            kind: job_detail
                .as_ref()
                .and_then(|detail| {
                    detail
                        .outputs
                        .iter()
                        .find(|output| output.reference == output_ref)
                        .map(|output| output.kind.clone())
                })
                .unwrap_or_else(|| "version".to_string()),
            ..CardVersion::default()
        });
        Self::fill_job_card(
            &mut card,
            job_detail.as_ref(),
            workflow_detail.as_ref(),
            &mut out,
        );
        card.suggestions = suggestions(Some(&job), Some(&output_ref));
        let line = status_line(
            status,
            &format!(
                "Saved {} on {} ({}, {})",
                feedback.reference,
                output_ref,
                kind_label(&feedback.kind),
                scope_label(&feedback.scope)
            ),
            Some(&op),
            "act on it if it is clear (revise and save_output); if not, offer one question, a suggestion, or two versions to compare",
        );
        out.line(line.clone());
        card.status_line = Some(line);
        card.operation_id = Some(op);
        Ok(Reply::read(out.finish(), card))
    }

    async fn rescope_feedback(
        &self,
        input: &SaveInput,
        op: impl Fn() -> Result<String>,
    ) -> Result<Reply> {
        let feedback = refs::expect(
            input.feedback.as_deref().or(input.reference.as_deref()),
            "feedback",
            &[RefKind::Feedback],
        )?
        .to_string();
        let scope = choice(&input.scope, "scope", &["workflow", "job"])?.ok_or_else(|| {
            Error::Validation("scope is required for rescope_feedback: workflow or job".to_string())
        })?;
        let op = op()?;
        let response = match self
            .send(&CoachRequest::rescope_feedback(&op, &feedback, &scope))
            .await
        {
            Ok(response) => response,
            Err(failure) => {
                return Ok(self.write_failure(
                    failure,
                    &format!("{feedback} not rescoped"),
                    Some(&op),
                    Self::write_scope(),
                ))
            }
        };
        let status = Self::saved_status(&response);
        let receipt: RescopedFeedback = response.view();
        let restated = receipt
            .restated_ref()
            .unwrap_or_else(|| "a new item".to_string());
        let mut out = Out::new();
        out.line(format!(
            "{feedback} is now restated as {restated}, {}. The person's words are unchanged.",
            scope_label(&scope)
        ));
        if let Some(guidance) = receipt.guidance_ref() {
            out.line(format!("It became reusable guidance {guidance}."));
        }
        if let Some(retired) = receipt.retired_guidance_ref() {
            out.line(format!("Guidance {retired} from the original was retired."));
        }
        let mut card = Card::new("status", status.state());
        let line = status_line(
            status,
            &format!("Rescoped {feedback} to {}", scope_label(&scope)),
            Some(&op),
            "confirm the new scope to the person in one line",
        );
        out.line(line.clone());
        card.status_line = Some(line);
        card.operation_id = Some(op);
        Ok(Reply::read(out.finish(), card))
    }

    async fn retract_feedback(
        &self,
        input: &SaveInput,
        op: impl Fn() -> Result<String>,
    ) -> Result<Reply> {
        let feedback = refs::expect(
            input.feedback.as_deref().or(input.reference.as_deref()),
            "feedback",
            &[RefKind::Feedback],
        )?
        .to_string();
        let op = op()?;
        let reason = text_field(&input.reason);
        let response = match self
            .send(&CoachRequest::retract_feedback(
                &op,
                &feedback,
                reason.as_deref(),
            ))
            .await
        {
            Ok(response) => response,
            Err(failure) => {
                return Ok(self.write_failure(
                    failure,
                    &format!("{feedback} not retracted"),
                    Some(&op),
                    Self::write_scope(),
                ))
            }
        };
        let status = Self::saved_status(&response);
        let receipt: RetractedFeedback = response.view();
        let mut out = Out::new();
        out.line(format!(
            "Retracted {feedback}: it no longer applies. Its record stays in the history{}.",
            reason
                .map(|reason| format!(" with the reason \"{reason}\""))
                .unwrap_or_default()
        ));
        if let Some(guidance) = &receipt.retired_guidance {
            out.line(format!(
                "Guidance {} made from it was retired.",
                guidance.reference
            ));
        }
        let mut card = Card::new("status", status.state());
        let line = status_line(
            status,
            &format!("Retracted {feedback}"),
            Some(&op),
            "tell the person it no longer applies",
        );
        out.line(line.clone());
        card.status_line = Some(line);
        card.operation_id = Some(op);
        Ok(Reply::read(out.finish(), card))
    }

    async fn edit_guidance(
        &self,
        input: &SaveInput,
        op: impl Fn() -> Result<String>,
    ) -> Result<Reply> {
        let guidance = refs::expect(
            input.guidance.as_deref().or(input.reference.as_deref()),
            "guidance",
            &[RefKind::Guidance],
        )?
        .to_string();
        let expected = input.expected_version.ok_or_else(|| {
            Error::Validation(format!(
                "expected_version is required for edit_guidance: the version you read with workflow(action=\"get\", ref=\"{guidance}\")"
            ))
        })?;
        let status =
            match choice(&input.change, "change", &["retire", "remove", "restore"])?.as_deref() {
                Some("retire") => Some(GuidanceStatus::Retired),
                Some("remove") => Some(GuidanceStatus::Removed),
                Some("restore") => Some(GuidanceStatus::Active),
                _ => None,
            };
        let body = input.text.clone().filter(|text| !text.trim().is_empty());
        let applies_when = match input.applies_when.as_deref() {
            None => FieldChange::Keep,
            Some(when) if when.trim().is_empty() => FieldChange::Clear,
            Some(when) => FieldChange::Set(when.trim().to_string()),
        };
        if body.is_none() && applies_when == FieldChange::Keep && status.is_none() {
            return Err(Error::Validation(
                "edit_guidance needs text, applies_when, or change (retire, remove or restore)"
                    .to_string(),
            ));
        }
        let op = op()?;
        let request = CoachRequest::edit_guidance(
            &op,
            &guidance,
            &EditGuidance {
                expected_version: expected,
                body,
                applies_when,
                status,
                reason: text_field(&input.reason),
            },
        );
        let response = match self.send(&request).await {
            Ok(response) => response,
            Err(failure) => {
                return Ok(self.write_failure(
                    failure,
                    &format!("{guidance} not changed"),
                    Some(&op),
                    Self::write_scope(),
                ))
            }
        };
        let saved = Self::saved_status(&response);
        let patched: PatchedGuidance = response.view();
        let mut out = Out::new();
        out.data("guidance", &guidance_item(&patched.guidance));
        let change = patched
            .change
            .as_ref()
            .map(|change| change.change.clone())
            .unwrap_or_else(|| "edited".to_string());
        let mut card = Card::new("status", saved.state());
        let line = status_line(
            saved,
            &format!("{guidance} is now v{} ({change})", patched.guidance.version),
            Some(&op),
            "show the person the guidance as it now reads",
        );
        out.line(line.clone());
        card.status_line = Some(line);
        card.operation_id = Some(op);
        Ok(Reply::read(out.finish(), card))
    }

    async fn import_guidance(
        &self,
        input: &SaveInput,
        op: impl Fn() -> Result<String>,
    ) -> Result<Reply> {
        let workflow = refs::expect(
            input.workflow.as_deref().or(input.reference.as_deref()),
            "workflow",
            &[RefKind::Workflow],
        )?;
        let mut sources = Vec::new();
        for source in &input.sources {
            let parsed = refs::expect(Some(source), "sources[]", &[RefKind::Guidance])?;
            if parsed.slug == workflow.slug {
                return Err(Error::Validation(format!(
                    "{parsed} is already in {workflow}; import from the person's other workflows"
                )));
            }
            sources.push(parsed.to_string());
        }
        if sources.is_empty() {
            return Err(Error::Validation(
                "sources is required for import_guidance: the guidance refs the person chose to reuse".to_string(),
            ));
        }
        let workflow = workflow.to_string();
        let op = op()?;
        let response = match self
            .send(&CoachRequest::import_guidance(&op, &workflow, &sources))
            .await
        {
            Ok(response) => response,
            Err(failure) => {
                return Ok(self.write_failure(
                    failure,
                    &format!("Guidance not imported into {workflow}"),
                    Some(&op),
                    Self::write_scope(),
                ))
            }
        };
        let status = Self::saved_status(&response);
        let imported: ImportedGuidance = response.view();
        let mut out = Out::new();
        for (from, to) in imported.pairs() {
            out.line(format!("- {from} → {to} (its source stays visible)"));
        }
        let mut card = Card::new("status", status.state());
        let line = status_line(
            status,
            &format!(
                "Imported {} guidance item(s) into {workflow}",
                imported.pairs().len()
            ),
            Some(&op),
            "check with the person that each one fits this workflow; retire any that does not",
        );
        out.line(line.clone());
        card.status_line = Some(line);
        card.operation_id = Some(op);
        Ok(Reply::read(out.finish(), card))
    }

    async fn set_effort(
        &self,
        input: &SaveInput,
        op: impl Fn() -> Result<String>,
    ) -> Result<Reply> {
        let job = optional_ref(&input.job, "job", &[RefKind::Job])?;
        let workflow = optional_ref(&input.workflow, "workflow", &[RefKind::Workflow])?;
        let target_ref = match (&job, &workflow) {
            (Some(_), Some(_)) => {
                return Err(Error::Validation(
                    "set_effort takes job (this job only) or workflow (every job), not both"
                        .to_string(),
                ))
            }
            (Some(job), None) => job.clone(),
            (None, Some(workflow)) => workflow.clone(),
            (None, None) => {
                return Err(Error::Validation(
                    "set_effort needs job (this job only) or workflow (every job)".to_string(),
                ))
            }
        };
        let effort =
            match choice(&input.effort, "effort", &["quick", "explore", "clear"])?.as_deref() {
                Some("quick") => Some(Effort::Quick),
                Some("explore") => Some(Effort::Explore),
                Some("clear") => None,
                _ => {
                    return Err(Error::Validation(
                        "effort is required for set_effort: quick, explore or clear".to_string(),
                    ))
                }
            };
        let words = input.words.clone().filter(|words| !words.trim().is_empty());
        if effort.is_some() && words.is_none() {
            return Err(Error::Validation(
                "words is required to set an effort: what the person said (\"just fix it\", \"keep exploring\"); never infer it".to_string(),
            ));
        }
        let op = op()?;
        let target = if job.is_some() {
            EffortTarget::Job(&target_ref)
        } else {
            EffortTarget::Workflow(&target_ref)
        };
        let response = match self
            .send(&CoachRequest::set_effort(
                &op,
                target,
                &SetEffort {
                    effort,
                    said: words,
                },
            ))
            .await
        {
            Ok(response) => response,
            Err(failure) => {
                return Ok(self.write_failure(
                    failure,
                    &format!("Coaching effort for {target_ref} not set"),
                    Some(&op),
                    Self::write_scope(),
                ))
            }
        };
        let status = Self::saved_status(&response);
        let receipt: EffortReceipt = response.view();
        let named = receipt.target_ref().unwrap_or_else(|| target_ref.clone());
        let what = match effort {
            Some(effort) => format!(
                "Coaching effort for {named} is {} ({})",
                effort.as_str(),
                if job.is_some() {
                    "this job only"
                } else {
                    "every job of the workflow, unless a job overrides it"
                }
            ),
            None => format!("Coaching effort for {named} cleared"),
        };
        let mut card = Card::new("status", status.state());
        let line = status_line(
            status,
            &what,
            Some(&op),
            "follow it until the person says otherwise, and keep it visible to them",
        );
        card.status_line = Some(line.clone());
        card.operation_id = Some(op);
        card.effort = effort.map(|effort| CardEffort {
            effort: effort.as_str().to_string(),
            said: receipt.said.clone(),
            for_this_job: job.is_some(),
        });
        Ok(Reply::read(line, card))
    }

    fn verdict_from(input: &SaveInput, required: bool) -> Result<Option<Verdict>> {
        let verdict = match text_field(&input.verdict) {
            Some(value) => Some(VerdictValue::parse(&value).ok_or_else(|| {
                Error::Validation("verdict must be better, same or worse".to_string())
            })?),
            None if required => return Err(Error::Validation(
                "verdict is required for record_verdict: the person's own better, same or worse"
                    .to_string(),
            )),
            None => None,
        };
        Ok(match verdict {
            Some(verdict) => Some(Verdict {
                verdict,
                note: text_field(&input.note),
                baseline: optional_ref(&input.baseline, "baseline", &[RefKind::Output])?,
                candidate: optional_ref(&input.candidate, "candidate", &[RefKind::Output])?,
            }),
            None => None,
        })
    }

    async fn transition(
        &self,
        action: &str,
        input: &SaveInput,
        op: impl Fn() -> Result<String>,
    ) -> Result<Reply> {
        let job = refs::expect(
            input.job.as_deref().or(input.reference.as_deref()),
            "job",
            &[RefKind::Job],
        )?
        .to_string();
        let (params, expected_status, verb) = match action {
            "close_job" => (
                Some(CloseJob {
                    used: optional_ref(&input.used, "used", &[RefKind::Output])?,
                    verdict: Self::verdict_from(input, false)?,
                }),
                "closed",
                "Closed",
            ),
            "reopen_job" => (None, "open", "Reopened"),
            _ => (None, "cancelled", "Cancelled"),
        };
        let build = |operation_id: &str| match (action, &params) {
            ("close_job", Some(params)) => CoachRequest::close_job(operation_id, &job, params),
            ("reopen_job", _) => CoachRequest::reopen_job(operation_id, &job),
            _ => CoachRequest::cancel_job(operation_id, &job),
        };
        let mut op = op()?;
        let mut response = match self.send(&build(&op)).await {
            Ok(response) => response,
            Err(failure) => {
                return Ok(self.write_failure(
                    failure,
                    &format!("{job} not {expected_status}"),
                    Some(&op),
                    Self::write_scope(),
                ))
            }
        };
        if response.replayed && text_field(&input.operation_id).is_none() {
            // A replay answers with the state at that time. If the job moved
            // since (closed, reopened, closed again), this is a new request.
            if let Ok(current) = self.send(&CoachRequest::get_job(&job)).await {
                let detail: JobDetail = current.view();
                if !detail.job.status.is_empty() && detail.job.status != expected_status {
                    op = sub_operation_id(&op, &format!("v{}", detail.job.version));
                    response = match self.send(&build(&op)).await {
                        Ok(response) => response,
                        Err(failure) => {
                            return Ok(self.write_failure(
                                failure,
                                &format!("{job} not {expected_status}"),
                                Some(&op),
                                Self::write_scope(),
                            ))
                        }
                    };
                }
            }
        }
        let status = Self::saved_status(&response);
        let mut out = Out::new();
        let mut card = Card::new("job_card", status.state());
        let job_view = if action == "close_job" {
            let closed: ClosedJob = response.view();
            if let Some(used) = closed.used_ref() {
                out.line(format!("The person used {used}."));
            }
            if let Some(verdict) = &closed.verdict {
                out.line(verdict_line(verdict));
            }
            closed.job
        } else {
            response.view::<JobView>()
        };
        if !job_view.reference.is_empty() {
            card.job = Some(Self::card_job(&job_view));
        }
        let next = match action {
            "close_job" => {
                "when the next job starts, propose its objective from what this one taught"
            }
            "reopen_job" => "continue the job: read its context, then revise and save_output",
            _ => "later jobs skip this one as their previous job",
        };
        let line = status_line(status, &format!("{verb} {job}"), Some(&op), next);
        out.line(line.clone());
        card.status_line = Some(line);
        card.operation_id = Some(op);
        Ok(Reply::read(out.finish(), card))
    }

    async fn record_verdict(
        &self,
        input: &SaveInput,
        op: impl Fn() -> Result<String>,
    ) -> Result<Reply> {
        let job = refs::expect(
            input.job.as_deref().or(input.reference.as_deref()),
            "job",
            &[RefKind::Job],
        )?
        .to_string();
        let verdict = Self::verdict_from(input, true)?.expect("a required verdict is present");
        let op = op()?;
        let response = match self
            .send(&CoachRequest::record_verdict(&op, &job, &verdict))
            .await
        {
            Ok(response) => response,
            Err(failure) => {
                return Ok(self.write_failure(
                    failure,
                    &format!("Verdict on {job} not saved"),
                    Some(&op),
                    Self::write_scope(),
                ))
            }
        };
        let status = Self::saved_status(&response);
        let saved: VerdictView = response.view();
        let mut card = Card::new("status", status.state());
        let line = status_line(
            status,
            &format!(
                "{} on {job} (revision {})",
                verdict_line(&saved),
                saved.revision.max(1)
            ),
            Some(&op),
            "it is the person's judgement; never restate it as a score",
        );
        card.status_line = Some(line.clone());
        card.operation_id = Some(op);
        Ok(Reply::read(line, card))
    }

    // ========================================================================
    // workflow_delete
    // ========================================================================

    async fn delete(&self, raw: Value) -> Result<ToolResult> {
        let attempted = "Workflow not deleted";
        let input: DeleteInput = match serde_json::from_value(raw.clone()) {
            Ok(input) => input,
            Err(error) => {
                return Ok(self
                    .not_sent(attempted, Error::Validation(error.to_string()))
                    .into_result())
            }
        };
        let workflow =
            match refs::expect(input.workflow.as_deref(), "workflow", &[RefKind::Workflow]) {
                Ok(workflow) => workflow.to_string(),
                Err(error) => return Ok(self.not_sent(attempted, error).into_result()),
            };
        let confirm = input.confirm.as_deref().map(str::trim).unwrap_or_default();
        if confirm != workflow {
            return Ok(self
                .not_sent(
                    attempted,
                    Error::Validation(format!(
                        "confirm must be exactly \"{workflow}\", after the person explicitly asked to delete it"
                    )),
                )
                .into_result());
        }
        let op = match self.operation_id(
            WORKFLOW_DELETE_TOOL,
            "delete",
            &input.operation_id,
            &raw,
            false,
        ) {
            Ok(op) => op,
            Err(error) => return Ok(self.not_sent(attempted, error).into_result()),
        };
        let reply = match self
            .send(&CoachRequest::delete_workflow(&op, &workflow, confirm))
            .await
        {
            Ok(response) => {
                let status = Self::saved_status(&response);
                let deleted: DeletedWorkflow = response.view();
                let mut card = Card::new("status", status.state());
                let after = deleted
                    .erase_after
                    .as_deref()
                    .and_then(|at| at.get(..10))
                    .map(|day| format!(" (by {day})"))
                    .unwrap_or_default();
                let line = status_line(
                    status,
                    &format!(
                        "Deleted {}: hidden now, and erased with all its jobs, versions, feedback and guidance within 30 days{after}",
                        deleted.workflow_ref().unwrap_or_else(|| workflow.clone())
                    ),
                    Some(&op),
                    "tell the person it is gone from every chat",
                );
                card.status_line = Some(line.clone());
                card.operation_id = Some(op);
                Reply::read(line, card)
            }
            Err(failure) => self.write_failure(failure, attempted, Some(&op), Self::write_scope()),
        };
        Ok(reply.into_result())
    }
}

// ============================================================================
// Rendering of the packet and the comparison
// ============================================================================

fn render_packet(packet: &ContextPacket) -> (String, Card) {
    let mut out = Out::new();
    out.line(format!(
        "Job packet for {} ({}). context_etag: {} (pass it to save_output).",
        packet.job.reference,
        if packet.schema.is_empty() {
            "coach_context.v1"
        } else {
            &packet.schema
        },
        packet.context_etag
    ));
    if !packet.instructions.trim().is_empty() {
        out.line(format!(
            "Agent Coach Kit instructions: {}",
            packet.instructions.trim()
        ));
    }
    let workflow = &packet.workflow;
    let mut head = format!("Workflow {}: {}\n", workflow.reference, workflow.name);
    if let Some(audience) = workflow
        .audience
        .as_deref()
        .filter(|a| !a.trim().is_empty())
    {
        head.push_str(&format!("Audience: {audience}\n"));
    }
    if let Some(kind) = workflow.output_kind.as_deref() {
        head.push_str(&format!("Output kind: {kind}\n"));
    }
    head.push_str(&goal_body(&packet.goal));
    out.data("workflow and goal", &head);
    let mut job = format!(
        "Job {} ({})\nObjective:\n{}\n",
        packet.job.reference, packet.job.status, packet.job.objective
    );
    if let Some(label) = packet.job.label.as_deref().filter(|l| !l.is_empty()) {
        job.push_str(&format!("Label: {label}\n"));
    }
    out.data("job", &job);
    out.line(effort_line(packet.coaching_effort.as_ref()));
    if packet.guidance.is_empty() {
        out.line("Active guidance: none yet.");
    } else {
        out.line(format!(
            "Active guidance ({}), apply each where it fits and say in carry_note what you did with it:",
            packet.guidance.len()
        ));
        out.data(
            "guidance",
            &packet
                .guidance
                .iter()
                .map(packet_guidance_item)
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    if !packet.job_feedback.is_empty() {
        out.line(format!(
            "This job's own feedback ({}):",
            packet.job_feedback.len()
        ));
        out.data(
            "job feedback",
            &packet
                .job_feedback
                .iter()
                .map(packet_feedback_item)
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    if !packet.approved_examples.is_empty() {
        out.line("Approved examples (learn from what worked without copying; other good results are fine):");
        out.data(
            "approved examples",
            &packet
                .approved_examples
                .iter()
                .map(|example| {
                    let mut text = format!(
                        "{} (approved in {}, job {})\n",
                        example.output_ref, example.feedback_ref, example.job_number
                    );
                    text.push_str(&format::words_and_reading(
                        &example.you_said,
                        example.coach_reading.as_deref(),
                    ));
                    if let Some(excerpt) = example.excerpt.as_deref().or(example.text.as_deref()) {
                        text.push_str(&format!("Example:\n{excerpt}\n"));
                    }
                    if let Some(url) = &example.url {
                        text.push_str(&format!("Link: {url}\n"));
                    }
                    if let Some(media) = &example.media {
                        text.push_str(&media_line(media));
                        text.push('\n');
                    }
                    text
                })
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    if packet.outputs.is_empty() {
        out.line("No versions yet in this job.");
    } else {
        out.line(format!("This job's versions ({}):", packet.outputs.len()));
        out.data(
            "versions",
            &packet
                .outputs
                .iter()
                .map(packet_output_item)
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    let mut card = Card::new("job_card", "read");
    card.workflow = Some(CardWorkflow {
        reference: workflow.reference.clone(),
        name: workflow.name.clone(),
        goal_version: Some(packet.goal.version),
        goal: Some(preview(&packet.goal.goal, 600)),
        ..CardWorkflow::default()
    });
    card.job = Some(CardJob {
        reference: packet.job.reference.clone(),
        objective: preview(&packet.job.objective, 400),
        status: packet.job.status.clone(),
    });
    card.effort = Coach::card_effort(packet.coaching_effort.as_ref());
    let latest = packet.outputs.iter().max_by_key(|output| output.version);
    if let Some(latest) = latest {
        card.version = Some(CardVersion {
            reference: latest.reference.clone(),
            kind: latest.kind.clone(),
            preview: latest
                .text
                .as_deref()
                .map(|text| preview(text, PREVIEW_CHARS)),
            url: latest.url.clone(),
            media: latest.media.as_ref().map(media_line),
        });
    }
    card.attention = packet
        .job_feedback
        .iter()
        .map(|feedback| CardItem {
            reference: feedback.reference.clone(),
            text: preview(&feedback.you_said, 160),
        })
        .collect();
    card.suggestions = suggestions(
        Some(&packet.job.reference),
        latest.map(|output| output.reference.as_str()),
    );
    (out.finish(), card)
}

fn compare_side(side: &mcp_client::coach::ComparedView, fallback: &str) -> (String, CompareSide) {
    let output = &side.output;
    let reference = if output.reference.is_empty() {
        fallback.to_string()
    } else {
        output.reference.clone()
    };
    let mut text = format!("{} (job {})\n", reference, side.job_ref());
    if !side.job_objective.is_empty() {
        text.push_str(&format!("Job objective: {}\n", side.job_objective));
    }
    // The API's chain starts with the version itself.
    let chain: Vec<String> = side
        .chain()
        .into_iter()
        .filter(|item| *item != reference)
        .collect();
    if !chain.is_empty() {
        text.push_str(&format!("Revises: {}\n", chain.join(" <- ")));
    }
    if let Some(from) = &side.written_from {
        text.push_str(&format!(
            "Written from: goal v{}, {} guidance, {} job feedback, {} examples{}\n",
            from.goal_version,
            from.guidance_count,
            from.job_feedback_count,
            from.example_count,
            match from.current {
                Some(false) => " (an older context)",
                _ => "",
            }
        ));
    }
    text.push_str(&output_content(
        &output.kind,
        output.body.as_deref(),
        output.url.as_deref(),
        output.title.as_deref(),
        output.caption.as_deref(),
        output.media.as_ref(),
    ));
    let carried = if side.carried.is_empty() {
        &output.carried
    } else {
        &side.carried
    };
    let changes = changes_body(output.carry_note.as_deref(), carried);
    if !changes.is_empty() {
        text.push_str("What changed:\n");
        text.push_str(&changes);
    }
    if !side.feedback.is_empty() {
        text.push_str("Feedback on it:\n");
        text.push_str(&feedback_body(&side.feedback));
    }
    let card = CompareSide {
        reference,
        job_ref: side.job_ref(),
        objective: preview(&side.job_objective, 300),
        kind: output.kind.clone(),
        text: output.body.as_deref().map(|body| preview(body, 4000)),
        url: output.url.clone(),
        media: output.media.as_ref().map(media_line),
        carry_note: output.carry_note.clone(),
        changed: carried
            .iter()
            .map(|item| CardChange {
                how: item.how.replace('_', " "),
                reference: item.reference.clone().unwrap_or_else(|| item.kind.clone()),
                note: item.note.clone(),
            })
            .collect(),
        feedback: side
            .feedback
            .iter()
            .map(|feedback| Coach::card_feedback(feedback, None))
            .collect(),
    };
    (text, card)
}

fn render_comparison(comparison: &Comparison, left_ref: &str, right_ref: &str) -> Reply {
    let (left_text, left) = compare_side(&comparison.left, left_ref);
    let (right_text, right) = compare_side(&comparison.right, right_ref);
    let mut out = Out::new();
    out.line(format!(
        "{} and {} side by side (no score; the person judges):",
        left.reference, right.reference
    ));
    out.data(&format!("earlier: {}", left.reference), &left_text);
    out.data(&format!("later: {}", right.reference), &right_text);
    out.line("Ask which works better, or what is still missing; if they pick one, save their words with add_feedback or record_verdict.");
    let workflow = comparison.workflow_ref();
    let mut card = Card::new("compare", "read");
    card.workflow = (!workflow.is_empty()).then(|| CardWorkflow {
        reference: workflow.clone(),
        name: workflow.clone(),
        ..CardWorkflow::default()
    });
    card.suggestions = suggestions(None, Some(&right.reference));
    card.compare = Some(CardCompare { left, right });
    Reply::read(out.finish(), card)
}

// ============================================================================
// Handlers
// ============================================================================

/// `workflow`: read-only.
pub struct WorkflowTool {
    coach: Coach,
}

/// `workflow_save`: append-only writes.
pub struct WorkflowSaveTool {
    coach: Coach,
}

/// `workflow_delete`: deletes one whole workflow.
pub struct WorkflowDeleteTool {
    coach: Coach,
}

#[async_trait]
impl ToolHandler for WorkflowTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let input: ReadInput =
            serde_json::from_value(input).map_err(|error| Error::Validation(error.to_string()))?;
        self.coach.read(input).await
    }

    fn metadata(&self) -> &ToolMetadata {
        static METADATA: std::sync::OnceLock<ToolMetadata> = std::sync::OnceLock::new();
        METADATA.get_or_init(schema::read_metadata)
    }

    fn input_schema(&self) -> Value {
        schema::read_schema()
    }
}

#[async_trait]
impl ToolHandler for WorkflowSaveTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        self.coach.save(input).await
    }

    fn metadata(&self) -> &ToolMetadata {
        static METADATA: std::sync::OnceLock<ToolMetadata> = std::sync::OnceLock::new();
        METADATA.get_or_init(schema::save_metadata)
    }

    fn input_schema(&self) -> Value {
        schema::save_schema()
    }
}

#[async_trait]
impl ToolHandler for WorkflowDeleteTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        self.coach.delete(input).await
    }

    fn metadata(&self) -> &ToolMetadata {
        static METADATA: std::sync::OnceLock<ToolMetadata> = std::sync::OnceLock::new();
        METADATA.get_or_init(schema::delete_metadata)
    }

    fn input_schema(&self) -> Value {
        schema::delete_schema()
    }
}

/// Register the three coach tools. They are visible only on the coach
/// surface (`/coach`).
pub fn register_coach_tools(
    registry: &mut crate::registry::ToolRegistry,
    client: ContextStreamClient,
) {
    let coach = Coach::new(client);
    registry.register(
        WORKFLOW_TOOL,
        Arc::new(WorkflowTool {
            coach: coach.clone(),
        }),
    );
    registry.register(
        WORKFLOW_SAVE_TOOL,
        Arc::new(WorkflowSaveTool {
            coach: coach.clone(),
        }),
    );
    registry.register(WORKFLOW_DELETE_TOOL, Arc::new(WorkflowDeleteTool { coach }));
}
