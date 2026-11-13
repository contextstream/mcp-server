//! Live run progress: the `progress` tool (contract v1.3, `run_progress.v1`).
//!
//! An agent says what it is doing now (its step, its checklist, the links it
//! opened, what it waits on, how it ended) and people watch it live in
//! ContextStream. Each report is one `POST /run-progress/report` made with
//! the caller's own credential: the API derives the run from the workspace
//! and a stable id for the piece of work, and writes only what changed.
//!
//! Three rules shape this module.
//!
//! - A report is a side note to the agent's work, so it never fails the
//!   agent's turn. An API that does not have the route yet (404), cannot be
//!   reached, or refuses the report is answered with a short result that
//!   says progress was not recorded. Only an argument that cannot be right
//!   is an error, said before anything is sent.
//! - One MCP session working one ticket is one run. Without `run_key`, the
//!   run's id is `<session>:<ticket_id or "session">`, so a later report
//!   updates the same run.
//! - A local (stdio) server keeps a reported run alive with a bare heartbeat
//!   on a timer until the run is reported finished or the process exits. A
//!   heartbeat only proves that the client session is open, so it carries no
//!   step. The hosted HTTP gateway keeps no state between requests and runs
//!   no timer; it tells the API to expect a longer silence instead.

use async_trait::async_trait;
use mcp_client::run_progress::{
    LINK_KINDS, MCP_RUNTIME, PLAN_ITEM_STATES, RUN_STATUSES, TERMINAL_RUN_STATUSES, WAITING_ON,
};
use mcp_client::{ContextStreamClient, RunFilter};
use mcp_session::SessionManager;
use mcp_types::{
    tool::{ToolAnnotations, ToolCategory, ToolMetadata, ToolResult},
    Error, Result,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};
use uuid::Uuid;

use super::display_title::one_line;
use crate::registry::ToolHandler;
use crate::schema::SchemaBuilder;

/// The tool's name.
pub const TOOL_NAME: &str = "progress";

/// The tool's actions; `report` is the one a call without `action` makes.
pub(crate) const VALID_ACTIONS: &[&str] = &["report", "get", "list"];

/// Turns reporting off on this machine: `CONTEXTSTREAM_RUN_PROGRESS=off`.
const ENABLED_ENV: &str = "CONTEXTSTREAM_RUN_PROGRESS";

/// How often a local server signals that a reported run's session is open,
/// and the `heartbeat_s` it tells the API to expect.
const LOCAL_HEARTBEAT: Duration = Duration::from_secs(30);

/// The `heartbeat_s` the hosted gateway sends: it runs no timer, so the API
/// is told to expect a signal only when the agent reports again.
const HOSTED_HEARTBEAT_S: u64 = 120;

/// A run nobody has reported on for this long is no longer kept alive: the
/// session may be open, but its agent has stopped saying what it does.
const HEARTBEAT_IDLE_LIMIT: Duration = Duration::from_secs(4 * 60 * 60);

/// Heartbeats that fail in a row (API unreachable) before the timer stops.
const HEARTBEAT_FAILURE_LIMIT: u32 = 5;

/// Most runs one process keeps alive at once; the ones reported longest ago
/// lose their heartbeat first.
const MAX_HEARTBEAT_RUNS: usize = 8;

/// Most runs one process remembers the id of.
const MAX_REMEMBERED_RUNS: usize = 64;

/// How many of a workspace's open runs the hosted gateway reads to find the
/// session's own (see `ProgressTool::sole_open_run`).
const OPEN_RUNS_READ: i64 = 100;

/// The agent's name when neither an agent name nor a client name is known.
const DEFAULT_AGENT_NAME: &str = "MCP agent";

/// `kind` values of a step.
const STEP_KINDS: &[&str] = &[
    "read", "edit", "run", "search", "browse", "git", "test", "think", "post", "other",
];

/// `status` values of a step.
const STEP_STATUSES: &[&str] = &["started", "done", "failed"];

const MAX_TITLE: usize = 200;
const MAX_LABEL: usize = 300;
const MAX_DETAIL: usize = 500;
const MAX_SUMMARY: usize = 2000;
const MAX_RUN_KEY: usize = 200;
const MAX_PLAN_ITEMS: usize = 100;
const MAX_LINKS: usize = 20;

/// Routing summary advertised as the tool description (<= 1024 chars).
pub(crate) const TOOL_DESCRIPTION: &str = "Show your work live. People watch agents' runs in ContextStream as they happen: who is working on what, the current step, the checklist, what is blocked. Report yours; it is one quick call.\n\nCall it:\n- when you start work on a ticket, a plan step or any multi-step task (title, ticket_id when there is one, step, plan);\n- whenever your step or checklist changes;\n- when you open a PR, a preview or a deploy (links);\n- when you are blocked or need a person (waiting);\n- when you finish (status, with an honest summary of what was done and what was not).\n\nstep = what you are doing now, in plain words. plan = your whole checklist each time: [{title, state: done|in_progress|next}]. Give percent only when it is real. action defaults to report; get reads one run, list the runs in the workspace. If a call says progress was not recorded, carry on with your work and do not retry it. Full field reference: the `action` parameter.";

/// Complete tool reference, advertised on the `action` parameter; see
/// [`crate::schema::with_full_reference`].
const TOOL_REFERENCE: &str = "Live run progress: say what you are doing so people can watch it. Each report is one call; ContextStream keeps the run.\n\naction=report (the default when action is left out). Send what changed; every field is optional:\n- title: what this piece of work is, in a few words (on the first report).\n- ticket_id, loop_id, step_id, plan_id: what the work is for. A report with ticket_id moves an open ticket to in_progress, and finishing with status=completed moves it to in_review.\n- step: what you are doing now, in plain words (\"Running the migration tests\"). Or {label, detail, kind: read|edit|run|search|browse|git|test|think|post|other, status: started|done|failed}; detail may name a file.\n- plan: your checklist, whole each time: [{title, state: done|in_progress|next|skipped}].\n- percent: 0-100, only when it is real (3 of 12 files done). remaining: what is left, in words (\"2 test files\").\n- milestone: something worth noting (\"Tests green\").\n- links: [{kind: pr|preview|deploy|doc|other, url, label}] when you open a PR, a preview or a deploy.\n- waiting: {on: person|approval|budget|rate_limit|sign_in|dependency|error, question} when you are blocked or need a person. The next report with a step (or waiting=false) says you are working again.\n- status: working (the default with a step), waiting_input, waiting_approval, or, to finish, completed, partial, stopped, failed or cancelled. summary: what was done and what was not, honestly; give it when you finish.\n- run_key: a stable name for this piece of work. Without it, one session working one ticket is one run (the session and its ticket_id, or the session alone), so a later report updates the same run. Pass your own when several agents share one session (a subagent's name), or to start a separate run.\nA run that goes quiet is shown as stalled, so report when your step changes. A local server also keeps the run alive in the background while the session is open, until you finish it. A report is safe to repeat.\n\naction=get: one run of the workspace with its step, checklist, waiting, links and outcome, by run_id (report returns it); without run_id, this session's run (the same run_key or ticket_id).\naction=list: the runs in the workspace (project_id narrows it), filtered by ticket_id, loop_id and active=true|false; limit.\n\nworkspace_id and project_id default to the session's. If a call answers that progress was not recorded (this server does not have live progress yet, or it cannot be reached), nothing else is affected: carry on with your work and do not retry.";

/// The `progress` tool's arguments. Every one is optional; a call without
/// `action` reports.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ProgressInput {
    pub action: Option<String>,
    pub workspace_id: Option<String>,
    pub project_id: Option<String>,
    /// A stable name for this piece of work (the report's `source_run_id`).
    pub run_key: Option<String>,
    pub title: Option<String>,
    pub ticket_id: Option<String>,
    pub loop_id: Option<String>,
    pub step_id: Option<String>,
    pub plan_id: Option<String>,
    /// What the agent is doing now: plain words, or `{label, detail, kind, status}`.
    pub step: Option<Value>,
    /// The checklist: `[{title, state}]`.
    pub plan: Option<Value>,
    pub percent: Option<Value>,
    pub remaining: Option<String>,
    /// Plain words, or `{text, links}`.
    pub milestone: Option<Value>,
    /// `[{kind, url, label}]`.
    pub links: Option<Value>,
    /// `{on, question}`; `false` says the run no longer waits.
    pub waiting: Option<Value>,
    pub status: Option<String>,
    pub summary: Option<String>,
    // get / list
    pub run_id: Option<String>,
    pub active: Option<bool>,
    pub limit: Option<i64>,
}

/// How a reported run is kept alive between reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Heartbeat {
    /// A bare report on a timer (a local, long-lived server process).
    Timer(Duration),
    /// No timer (the stateless hosted gateway).
    None,
}

impl Heartbeat {
    /// For the transport this process serves: a timer on stdio, none on the
    /// hosted HTTP gateway.
    fn for_transport(is_http: bool) -> Self {
        if is_http {
            Self::None
        } else {
            Self::Timer(LOCAL_HEARTBEAT)
        }
    }

    /// The `heartbeat_s` a report tells the API to expect.
    fn seconds(self) -> u64 {
        match self {
            Self::Timer(every) => every.as_secs().max(1),
            Self::None => HOSTED_HEARTBEAT_S,
        }
    }
}

/// What this process remembers of a run it reported (stdio only).
#[derive(Debug)]
struct RunMemo {
    /// The run's id, as the API answered it.
    run_id: Option<String>,
    /// The heartbeat task keeping the run alive, by its generation.
    beat: Option<u64>,
    /// The run was reported and has not been finished since.
    open: bool,
    /// When the agent last reported on the run.
    reported_at: Instant,
    /// Held while a report or a heartbeat for the run is on its way, so the
    /// two never cross: no heartbeat leaves after the report that ended the
    /// run was answered.
    sending: Arc<tokio::sync::Mutex<()>>,
}

impl RunMemo {
    fn new() -> Self {
        Self {
            run_id: None,
            beat: None,
            open: false,
            reported_at: Instant::now(),
            sending: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

type Runs = Arc<Mutex<HashMap<String, RunMemo>>>;

fn lock(runs: &Runs) -> MutexGuard<'_, HashMap<String, RunMemo>> {
    runs.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The live `progress` tool.
pub struct ProgressTool {
    client: ContextStreamClient,
    session: Arc<SessionManager>,
    /// Set by tests; otherwise the transport decides ([`Heartbeat::for_transport`]).
    heartbeat: Option<Heartbeat>,
    runs: Runs,
}

impl ProgressTool {
    pub fn new(client: ContextStreamClient, session: Arc<SessionManager>) -> Self {
        Self {
            client,
            session,
            heartbeat: None,
            runs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_heartbeat(mut self, heartbeat: Heartbeat) -> Self {
        self.heartbeat = Some(heartbeat);
        self
    }

    /// The scope of a report or a read of this session's run: the call's
    /// ids, else the session's. The session's project is not borrowed for
    /// another workspace than the session's.
    async fn scope(&self, input: &ProgressInput) -> Result<(Option<Uuid>, Option<Uuid>)> {
        let (session_workspace, session_project) =
            super::scope::read_scope_defaults(&self.client, Some(self.session.as_ref())).await;
        let workspace = scope_id(input.workspace_id.as_deref(), "workspace_id")?;
        let project = scope_id(input.project_id.as_deref(), "project_id")?;
        let names_another_workspace = workspace.is_some_and(|id| Some(id) != session_workspace);
        Ok((
            workspace.or(session_workspace),
            project.or(if names_another_workspace {
                None
            } else {
                session_project
            }),
        ))
    }

    /// Who is working: the configured agent name (`CONTEXTSTREAM_AGENT_NAME`
    /// or an `agent-<name>` folder, as coordination names agents), else the
    /// client's own name; `runtime` is always `mcp`.
    async fn agent(&self, is_http: bool) -> Value {
        let folder = self.session.state().await.folder_path;
        let handle = if is_http {
            // The gateway's own environment and folder say nothing about the
            // caller: only the folder the caller's session named counts.
            mcp_client::agent_handle::agent_handle_of_folder(folder.as_deref())
        } else {
            let folder = folder.or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|path| path.to_string_lossy().into_owned())
            });
            mcp_client::agent_handle::agent_handle(folder.as_deref())
        };
        agent_identity(
            handle,
            crate::client_identity::current_client_name(),
            &session_part(is_http),
        )
    }

    async fn report(&self, input: ProgressInput) -> Result<ToolResult> {
        if !reporting_enabled() {
            return Ok(unrecorded(
                "Not recorded",
                "off",
                None,
                &format!("live progress is turned off on this machine ({ENABLED_ENV}=off)"),
            ));
        }
        let is_http = self.client.config().await.is_http_transport;
        let (workspace_id, project_id) = self.scope(&input).await?;
        let Some(workspace_id) = workspace_id else {
            return Ok(unrecorded(
                "Not recorded",
                "no_workspace",
                None,
                "this session has no workspace yet (run init(folder_path=\"...\") or pass workspace_id)",
            ));
        };
        let heartbeat = self
            .heartbeat
            .unwrap_or_else(|| Heartbeat::for_transport(is_http));
        let context = ReportContext {
            workspace_id,
            project_id,
            session: session_part(is_http),
            agent: self.agent(is_http).await,
            heartbeat_s: heartbeat.seconds(),
        };
        let mut report = build_report(&input, &context)?;
        if names_no_run(&input) {
            if let Some(target) = self.sole_open_run(workspace_id, is_http, &context).await {
                report.address(target);
            }
        }
        let key = run_memo_key(workspace_id, &report.run_key);
        // A local server sends one thing at a time for a run (see `RunMemo`).
        let sending = (!is_http).then(|| self.sending_gate(&key));
        let _sending = match &sending {
            Some(gate) => Some(gate.lock().await),
            None => None,
        };
        // The last word on a run is worth one more try; a step is not.
        let retries = u32::from(report.is_terminal());
        match self.client.report_run_progress(&report.body, retries).await {
            Ok(mut answer) => {
                // What stands for this session and the key this report went
                // to: ContextStream's hooks read them from the answer, so the
                // signals they send land on the same run under the same agent.
                if let Some(fields) = answer.as_object_mut() {
                    fields.insert("session".into(), json!(context.session));
                    if !report.by_run_id {
                        fields.insert("run_key".into(), json!(report.run_key));
                    }
                }
                let run_id = answer
                    .pointer("/run/run_id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if report.is_terminal() {
                    self.finish(&key, run_id, is_http);
                } else {
                    self.keep_alive(&key, run_id, &report.body, heartbeat, is_http);
                }
                Ok(ToolResult::with_structured(
                    recorded_text(&report, &answer),
                    answer,
                ))
            }
            Err(error) => {
                if report.is_terminal() {
                    // The agent said the run is over: it is not kept alive,
                    // whether or not the API heard it.
                    self.finish(&key, None, is_http);
                }
                tracing::debug!("run progress not recorded: {}", error);
                Ok(api_failure("Not recorded", &error))
            }
        }
    }

    /// The session's one open run, for a report that names none (no
    /// `ticket_id`, no `run_key`). A model that took up a ticket and then
    /// reports a question without naming the ticket means that run, not a
    /// second one. `None` when the session has no open run or more than one,
    /// and when the session part is not one session (a stateless client that
    /// names no connection shares it with every session of its key): the
    /// report then goes to the session's default run, as it always did.
    async fn sole_open_run(
        &self,
        workspace_id: Uuid,
        is_http: bool,
        context: &ReportContext,
    ) -> Option<RunTarget> {
        if !is_one_session(&context.session, is_http) {
            return None;
        }
        if !is_http {
            return self.sole_remembered_run(workspace_id).map(RunTarget::Key);
        }
        // The gateway keeps nothing between requests; the API knows the
        // runs, and names each by its agent: this session's is its own.
        let agent_id = context.agent.get("id").and_then(Value::as_str)?;
        let filter = RunFilter {
            workspace_id: Some(workspace_id),
            active: Some(true),
            limit: Some(OPEN_RUNS_READ),
            ..RunFilter::default()
        };
        let result = self.client.list_run_progress(&filter).await.ok()?;
        let listed = runs_of(&result);
        if listed.len() as i64 >= OPEN_RUNS_READ {
            // A full page may hide another run of this session.
            return None;
        }
        let mut mine = listed
            .iter()
            .filter(|view| view.pointer("/agent/id").and_then(Value::as_str) == Some(agent_id));
        let view = mine.next()?;
        if mine.next().is_some() {
            return None;
        }
        let run_id = str_field(view, "run_id").filter(|id| Uuid::parse_str(id).is_ok())?;
        Some(RunTarget::Id {
            run_id: run_id.to_string(),
            on_a_ticket: view
                .pointer("/scope/ticket_id")
                .is_some_and(|id| !id.is_null()),
        })
    }

    /// The key of the one run this process reported in the workspace and
    /// has not finished, when there is exactly one.
    fn sole_remembered_run(&self, workspace_id: Uuid) -> Option<String> {
        let prefix = format!("{workspace_id}:");
        let runs = lock(&self.runs);
        let mut open = runs
            .iter()
            .filter(|(key, memo)| memo.open && key.starts_with(&prefix));
        let (key, _) = open.next()?;
        if open.next().is_some() {
            return None;
        }
        Some(key[prefix.len()..].to_string())
    }

    /// Remember a finished run's id and stop its heartbeat.
    fn finish(&self, key: &str, run_id: Option<String>, is_http: bool) {
        if is_http {
            return;
        }
        let mut runs = lock(&self.runs);
        let memo = runs.entry(key.to_string()).or_insert_with(RunMemo::new);
        memo.beat = None;
        memo.open = false;
        memo.reported_at = Instant::now();
        if run_id.is_some() {
            memo.run_id = run_id;
        }
        forget_oldest(&mut runs);
    }

    /// The gate a report or a heartbeat for the run holds while it is sent.
    fn sending_gate(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        lock(&self.runs)
            .entry(key.to_string())
            .or_insert_with(RunMemo::new)
            .sending
            .clone()
    }

    /// Remember a reported run and, on a local server, keep it alive with a
    /// bare heartbeat until it is finished or the process exits.
    fn keep_alive(
        &self,
        key: &str,
        run_id: Option<String>,
        report_body: &Value,
        heartbeat: Heartbeat,
        is_http: bool,
    ) {
        if is_http {
            // The gateway serves many callers and keeps nothing about them.
            return;
        }
        static GENERATION: AtomicU64 = AtomicU64::new(1);
        let generation = match heartbeat {
            Heartbeat::Timer(_) => Some(GENERATION.fetch_add(1, Ordering::Relaxed)),
            Heartbeat::None => None,
        };
        let sending = {
            let mut runs = lock(&self.runs);
            let memo = runs.entry(key.to_string()).or_insert_with(RunMemo::new);
            if run_id.is_some() {
                memo.run_id = run_id;
            }
            memo.beat = generation;
            memo.open = true;
            memo.reported_at = Instant::now();
            let sending = memo.sending.clone();
            // The runs reported longest ago lose their heartbeat first.
            let mut beating: Vec<(Instant, String)> = runs
                .iter()
                .filter(|(_, memo)| memo.beat.is_some())
                .map(|(key, memo)| (memo.reported_at, key.clone()))
                .collect();
            if beating.len() > MAX_HEARTBEAT_RUNS {
                beating.sort();
                let excess = beating.len() - MAX_HEARTBEAT_RUNS;
                for (_, stale) in beating.into_iter().take(excess) {
                    if let Some(memo) = runs.get_mut(&stale) {
                        memo.beat = None;
                    }
                }
            }
            forget_oldest(&mut runs);
            sending
        };
        let (Heartbeat::Timer(every), Some(generation)) = (heartbeat, generation) else {
            return;
        };
        let body = heartbeat_body(report_body);
        let runs = self.runs.clone();
        let client = self.client.clone();
        let key = key.to_string();
        // Detached, with the caller's identity, so the agent's turn never
        // waits on it. It ends with the process.
        mcp_client::spawn_with_task_context(async move {
            let mut failures = 0_u32;
            loop {
                tokio::time::sleep(every).await;
                // After a report that is on its way, never beside it.
                let _sending = sending.lock().await;
                let ours = lock(&runs).get(&key).is_some_and(|memo| {
                    memo.beat == Some(generation)
                        && memo.reported_at.elapsed() < HEARTBEAT_IDLE_LIMIT
                });
                if !ours {
                    break;
                }
                match client.report_run_progress(&body, 0).await {
                    Ok(answer) => {
                        failures = 0;
                        if run_has_ended(&answer) {
                            break;
                        }
                    }
                    // The API is away for a moment: try again on the next tick.
                    Err(error) if error.is_retryable() => {
                        failures += 1;
                        if failures >= HEARTBEAT_FAILURE_LIMIT {
                            tracing::debug!("run heartbeat stopped: {}", error);
                            break;
                        }
                    }
                    // The API has no such route or refuses this key: asking
                    // again every tick would change nothing.
                    Err(error) => {
                        tracing::debug!("run heartbeat stopped: {}", error);
                        break;
                    }
                }
            }
            if let Some(memo) = lock(&runs).get_mut(&key) {
                if memo.beat == Some(generation) {
                    memo.beat = None;
                }
            }
        });
    }

    /// The id of the run this session reported under the call's `run_key`
    /// (or its default), when this process remembers it.
    fn remembered_run(&self, workspace_id: Uuid, run_key: &str) -> Option<Uuid> {
        lock(&self.runs)
            .get(&run_memo_key(workspace_id, run_key))
            .and_then(|memo| memo.run_id.as_deref())
            .and_then(|id| Uuid::parse_str(id).ok())
    }

    async fn get(&self, input: ProgressInput) -> Result<ToolResult> {
        let ticket_id = id_argument(
            input.ticket_id.as_deref(),
            "ticket_id",
            "the ticket's UUID (entity(kind=\"ticket\", action=\"list\") shows it)",
        )?;
        let mut run_id = id_argument(
            input.run_id.as_deref(),
            "run_id",
            "the run's UUID (progress(action=\"list\") shows it)",
        )?;
        let is_http = self.client.config().await.is_http_transport;
        let (workspace_id, project_id) = self.scope(&input).await?;
        // A run is read inside a workspace: the API looks it up nowhere else.
        let Some(workspace_id) = workspace_id else {
            return Ok(unrecorded(
                "Not read",
                "no_workspace",
                None,
                "this session has no workspace yet (run init(folder_path=\"...\") or pass workspace_id)",
            ));
        };
        if run_id.is_none() && !is_http {
            let run_key = run_key_of(input.run_key.as_deref(), &session_part(is_http), ticket_id);
            run_id = self.remembered_run(workspace_id, &run_key);
        }
        if let Some(run_id) = run_id {
            return Ok(
                match self.client.get_run_progress(workspace_id, run_id).await {
                    Ok(view) => ToolResult::with_structured(run_text(&view), view),
                    Err(error) => api_failure("Not read", &error),
                },
            );
        }
        // No run id is known here (the hosted gateway keeps none): the
        // newest run on the call's ticket is the one meant.
        let Some(ticket_id) = ticket_id else {
            return Ok(ToolResult::text(
                "[PROGRESS] No run of this session is known here. Pass run_id (a report returns it, and progress(action=\"list\") shows the runs in the workspace) or ticket_id for the newest run on a ticket.",
            ));
        };
        let filter = RunFilter {
            workspace_id: Some(workspace_id),
            project_id,
            ticket_id: Some(ticket_id),
            limit: Some(1),
            ..RunFilter::default()
        };
        Ok(match self.client.list_run_progress(&filter).await {
            Ok(result) => match runs_of(&result).first() {
                Some(view) => ToolResult::with_structured(run_text(view), view.clone()),
                None => ToolResult::with_structured(
                    format!("[PROGRESS] No run on ticket {ticket_id} yet."),
                    result,
                ),
            },
            Err(error) => api_failure("Not read", &error),
        })
    }

    async fn list(&self, input: ProgressInput) -> Result<ToolResult> {
        let ticket_id = id_argument(
            input.ticket_id.as_deref(),
            "ticket_id",
            "the ticket's UUID (entity(kind=\"ticket\", action=\"list\") shows it)",
        )?;
        let loop_id = id_argument(
            input.loop_id.as_deref(),
            "loop_id",
            "the loop's UUID (session(action=\"list_loops\") shows it)",
        )?;
        let (session_workspace, session_project) =
            super::scope::read_scope_defaults(&self.client, Some(self.session.as_ref())).await;
        let workspace = scope_id(input.workspace_id.as_deref(), "workspace_id")?;
        let project = scope_id(input.project_id.as_deref(), "project_id")?;
        // A workspace given without a project stays workspace-wide, as for
        // list_loops.
        let project_id = project.or(if workspace.is_some() {
            None
        } else {
            session_project
        });
        let Some(workspace_id) = workspace.or(session_workspace) else {
            return Ok(unrecorded(
                "Not read",
                "no_workspace",
                None,
                "this session has no workspace yet (run init(folder_path=\"...\") or pass workspace_id)",
            ));
        };
        let filter = RunFilter {
            workspace_id: Some(workspace_id),
            project_id,
            loop_id,
            ticket_id,
            active: input.active,
            limit: input.limit.filter(|limit| *limit > 0),
        };
        Ok(match self.client.list_run_progress(&filter).await {
            Ok(result) => ToolResult::with_structured(list_text(&result), result),
            Err(error) => api_failure("Not read", &error),
        })
    }
}

#[async_trait]
impl ToolHandler for ProgressTool {
    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let input: ProgressInput =
            serde_json::from_value(input).map_err(|e| Error::Validation(e.to_string()))?;
        let action = input
            .action
            .as_deref()
            .map(str::trim)
            .filter(|action| !action.is_empty())
            .unwrap_or("report")
            .to_ascii_lowercase();
        match action.as_str() {
            "report" => self.report(input).await,
            "get" => self.get(input).await,
            "list" => self.list(input).await,
            other => Err(Error::Validation(format!(
                "Unknown action: {}. Use report (the default), get or list.",
                one_line(other)
            ))),
        }
    }

    fn metadata(&self) -> &ToolMetadata {
        static METADATA: OnceLock<ToolMetadata> = OnceLock::new();
        METADATA.get_or_init(|| ToolMetadata {
            name: TOOL_NAME.to_string(),
            title: "Report Live Progress".to_string(),
            description: TOOL_DESCRIPTION.to_string(),
            category: ToolCategory::Session,
            // A report adds to a run and is safe to send twice; nothing is
            // ever removed, and no person needs to confirm a status line.
            annotations: ToolAnnotations {
                requires_confirmation: false,
                ..ToolAnnotations::write().idempotent()
            },
            is_pro: false,
            required_tier: None,
        })
    }

    fn input_schema(&self) -> Value {
        SchemaBuilder::new()
            .description("Report what you are doing now, so people can watch your work live.")
            .string_enum(
                "action",
                &crate::schema::with_full_reference(
                    "report (the default when left out), get or list",
                    TOOL_REFERENCE,
                ),
                VALID_ACTIONS,
                false,
            )
            .string(
                "step",
                "What you are doing now, in plain words, e.g. \"Running the migration tests\" (for report)",
                false,
            )
            .property(
                "plan",
                json!({
                    "type": "array",
                    "description": "Your checklist, whole each time (for report)",
                    "items": {
                        "type": "object",
                        "properties": {
                            "title": { "type": "string", "description": "The item, in a few words" },
                            "state": { "type": "string", "enum": PLAN_ITEM_STATES, "description": "done, in_progress, next (not started) or skipped" }
                        },
                        "required": ["title", "state"]
                    }
                }),
                false,
            )
            .string(
                "title",
                "What this piece of work is, in a few words (for report; give it on the first report)",
                false,
            )
            .string(
                "ticket_id",
                "The ticket this work is for (for report: an open ticket moves to in_progress); for get and list, the ticket whose runs to read",
                false,
            )
            .string(
                "loop_id",
                "The plan loop this work belongs to (for report); for list, the loop whose runs to read",
                false,
            )
            .string("step_id", "The plan step this work is for (for report)", false)
            .string("plan_id", "The saved plan this work follows (for report)", false)
            .number(
                "percent",
                "How far along, 0-100, only when it is real, e.g. 3 of 12 files done (for report)",
                false,
            )
            .string(
                "remaining",
                "What is left, in words, e.g. \"2 test files\" (for report)",
                false,
            )
            .string(
                "milestone",
                "Something worth noting, e.g. \"Tests green\" (for report)",
                false,
            )
            .property(
                "links",
                json!({
                    "type": "array",
                    "description": "A PR, preview or deploy you opened (for report)",
                    "items": {
                        "type": "object",
                        "properties": {
                            "kind": { "type": "string", "enum": LINK_KINDS },
                            "url": { "type": "string" },
                            "label": { "type": "string" }
                        },
                        "required": ["url"]
                    }
                }),
                false,
            )
            .property(
                "waiting",
                json!({
                    "type": "object",
                    "description": "What you are blocked on (for report); the next report with a step says you are working again",
                    "properties": {
                        "on": { "type": "string", "enum": WAITING_ON, "description": "person when you need someone to answer or decide" },
                        "question": { "type": "string", "description": "What you need, as you would ask the person" }
                    },
                    "required": ["on"]
                }),
                false,
            )
            .string_enum(
                "status",
                "Where the run stands (for report): working, waiting_input, waiting_approval, or, to finish it, completed, partial, stopped, failed or cancelled",
                RUN_STATUSES,
                false,
            )
            .string(
                "summary",
                "When you finish: what was done and what was not, honestly (for report)",
                false,
            )
            .string(
                "run_key",
                "A stable name for this piece of work. Left out, one session working one ticket is one run; pass your own when several agents share a session, or to start a separate run",
                false,
            )
            .string(
                "run_id",
                "The run to read (for get); a report returns it and list shows it",
                false,
            )
            .boolean(
                "active",
                "For list: true for runs still going, false for ended runs",
                false,
            )
            .integer("limit", "Most runs list returns", false)
            .uuid(
                "workspace_id",
                "Workspace ID. Defaults to the session's.",
                false,
            )
            .uuid(
                "project_id",
                "Project ID. Defaults to the session's project.",
                false,
            )
            .build()
    }
}

/// Register the `progress` tool.
pub fn register_progress_tools(
    registry: &mut crate::registry::ToolRegistry,
    client: ContextStreamClient,
    session: Arc<SessionManager>,
) {
    registry.register(TOOL_NAME, Arc::new(ProgressTool::new(client, session)));
}

// ============================================================================
// The report body
// ============================================================================

/// What a report is built in: the resolved scope, what stands for this MCP
/// session, who is working, and the silence the API should expect.
#[derive(Debug, Clone)]
pub(crate) struct ReportContext {
    pub workspace_id: Uuid,
    pub project_id: Option<Uuid>,
    pub session: String,
    pub agent: Value,
    pub heartbeat_s: u64,
}

/// A report ready to send.
#[derive(Debug, Clone)]
pub(crate) struct Report {
    /// The body of `POST /run-progress/report`.
    pub body: Value,
    /// The report's `source_run_id`.
    pub run_key: String,
    /// The status sent, given or implied.
    pub status: Option<&'static str>,
    /// What the call gave, in the words the answer uses.
    pub gave: Vec<&'static str>,
    /// Things the agent should know that did not stop the report.
    pub notes: Vec<String>,
    /// The report is sent by the run's id (`run_id`), not by a key: the
    /// hosted gateway sent it on to the session's open run (see
    /// [`RunTarget`]), whose key it cannot know.
    pub by_run_id: bool,
}

/// The run a report that named none is sent on to: the session's one open run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RunTarget {
    /// By the key it was reported under (a local server remembers it).
    Key(String),
    /// By its id, which is all the hosted gateway can learn from the API.
    Id { run_id: String, on_a_ticket: bool },
}

/// What a report says when it was sent on to another run than its default.
const SENT_ON: &str = "No run was named, so this went to the one run this session has open. Pass ticket_id or run_key to report on another.";

impl Report {
    /// Send the report to `target` instead of the session's default run.
    pub(crate) fn address(&mut self, target: RunTarget) {
        let Some(body) = self.body.as_object_mut() else {
            return;
        };
        match target {
            RunTarget::Key(key) => {
                if key == self.run_key {
                    return;
                }
                body.insert("source_run_id".into(), json!(key));
                self.run_key = key;
                self.notes.push(SENT_ON.to_string());
            }
            RunTarget::Id {
                run_id,
                on_a_ticket,
            } => {
                body.remove("source_run_id");
                body.insert("run_id".into(), json!(run_id));
                self.by_run_id = true;
                // A run without a ticket is as a rule the default run itself.
                if on_a_ticket {
                    self.notes.push(SENT_ON.to_string());
                }
            }
        }
    }

    /// Whether the report ends the run.
    pub fn is_terminal(&self) -> bool {
        self.status
            .is_some_and(|status| TERMINAL_RUN_STATUSES.contains(&status))
    }
}

/// What a waiting argument says.
enum Waiting {
    On(Value),
    Cleared,
}

/// The body of a report from the call's arguments. Cheap checks are made
/// here, before anything is sent; the rest is the API's to judge.
pub(crate) fn build_report(input: &ProgressInput, context: &ReportContext) -> Result<Report> {
    let mut notes = Vec::new();
    let mut gave = Vec::new();
    let ticket_id = id_argument(
        input.ticket_id.as_deref(),
        "ticket_id",
        "the ticket's UUID (entity(kind=\"ticket\", action=\"list\") shows it)",
    )?;
    let loop_id = id_argument(
        input.loop_id.as_deref(),
        "loop_id",
        "the loop's UUID (session(action=\"list_loops\") shows it)",
    )?;
    let plan_id = id_argument(
        input.plan_id.as_deref(),
        "plan_id",
        "the plan's UUID (session(action=\"list_plans\") shows it)",
    )?;
    let explicit_status = status_argument(input.status.as_deref())?;
    let step = step_argument(input.step.as_ref())?;
    let plan = plan_argument(input.plan.as_ref(), &mut notes)?;
    let milestone = milestone_argument(input.milestone.as_ref())?;
    let links = links_argument(input.links.as_ref(), "links", &mut notes)?;
    let waiting = waiting_argument(input.waiting.as_ref())?;
    let percent = percent_argument(input.percent.as_ref(), &mut notes);

    let run_key = run_key_of(input.run_key.as_deref(), &context.session, ticket_id);

    // The status: what the call says, else what it implies. Waiting on
    // something is more specific than "working"; an end is final.
    let waiting_status = |waiting: &Value| {
        if waiting.get("on").and_then(Value::as_str) == Some("approval") {
            "waiting_approval"
        } else {
            "waiting_input"
        }
    };
    let status: Option<&'static str> = match (explicit_status, &waiting) {
        (Some(status), _) if TERMINAL_RUN_STATUSES.contains(&status) => Some(status),
        (Some("working") | None, Some(Waiting::On(waiting))) => Some(waiting_status(waiting)),
        (Some(status), _) => Some(status),
        (None, Some(Waiting::Cleared)) => Some("working"),
        (None, None) if step.is_some() => Some("working"),
        (None, None) => None,
    };
    let is_waiting = matches!(status, Some("waiting_input" | "waiting_approval"));

    let mut body = Map::new();
    body.insert("workspace_id".into(), json!(context.workspace_id));
    if let Some(project_id) = context.project_id {
        body.insert("project_id".into(), json!(project_id));
    }
    body.insert("source_run_id".into(), json!(run_key));
    body.insert("agent".into(), context.agent.clone());
    body.insert("heartbeat_s".into(), json!(context.heartbeat_s));
    if let Some(title) = text_argument(input.title.as_deref(), MAX_TITLE) {
        body.insert("title".into(), json!(title));
    }
    if let Some(id) = ticket_id {
        body.insert("ticket_id".into(), json!(id));
    }
    if let Some(id) = loop_id {
        body.insert("loop_id".into(), json!(id));
    }
    if let Some(step_id) = text_argument(input.step_id.as_deref(), MAX_TITLE) {
        body.insert("step_id".into(), json!(step_id));
    }
    if let Some(id) = plan_id {
        body.insert("plan_id".into(), json!(id));
    }
    if let Some(status) = status {
        body.insert("status".into(), json!(status));
        if TERMINAL_RUN_STATUSES.contains(&status) {
            gave.push("finished");
        }
    }
    if let Some(step) = step {
        body.insert("step".into(), step);
        gave.push("step");
    }
    if let Some(plan) = plan {
        body.insert("plan".into(), Value::Array(plan));
        gave.push("checklist");
    }
    if let Some(percent) = percent {
        body.insert("percent".into(), percent);
        gave.push("percent");
    }
    if let Some(remaining) = text_argument(input.remaining.as_deref(), MAX_TITLE) {
        body.insert("remaining".into(), json!(remaining));
    }
    if let Some(milestone) = milestone {
        body.insert("milestone".into(), milestone);
        gave.push("milestone");
    }
    if let Some(links) = links {
        body.insert("links".into(), Value::Array(links));
        gave.push("links");
    }
    match waiting {
        // A run that ended waits on nothing.
        Some(Waiting::On(waiting)) if is_waiting => {
            body.insert("waiting".into(), waiting);
            gave.push("waiting");
        }
        Some(_) => {
            body.insert("waiting".into(), Value::Null);
        }
        // A waiting status with nothing named waits on a person or an approval.
        None if is_waiting => {
            let on = if status == Some("waiting_approval") {
                "approval"
            } else {
                "person"
            };
            body.insert("waiting".into(), json!({ "on": on }));
            gave.push("waiting");
        }
        // Working again, or done: whatever the run waited on is over.
        None if status.is_some() => {
            body.insert("waiting".into(), Value::Null);
        }
        None => {}
    }
    if let Some(summary) = text_argument(input.summary.as_deref(), MAX_SUMMARY) {
        body.insert("summary".into(), json!(summary));
    } else if status.is_some_and(|status| TERMINAL_RUN_STATUSES.contains(&status)) {
        notes.push(
            "No summary was given: report again with the same status and summary=\"...\" to say what was done and what was not."
                .to_string(),
        );
    }

    Ok(Report {
        body: Value::Object(body),
        run_key,
        status,
        gave,
        notes,
        by_run_id: false,
    })
}

/// A heartbeat for a reported run: who and which run, and nothing about the
/// work. A report that changes nothing only moves the run's last signal.
pub(crate) fn heartbeat_body(report_body: &Value) -> Value {
    let mut body = Map::new();
    for key in [
        "workspace_id",
        "project_id",
        "source_run_id",
        "agent",
        "heartbeat_s",
    ] {
        if let Some(value) = report_body.get(key) {
            body.insert(key.to_string(), value.clone());
        }
    }
    Value::Object(body)
}

/// The run a report goes to when the call names none: one MCP session
/// working one ticket is one run.
pub(crate) fn run_key_of(given: Option<&str>, session: &str, ticket_id: Option<Uuid>) -> String {
    if let Some(given) = text_argument(given, MAX_RUN_KEY) {
        return given;
    }
    match ticket_id {
        Some(ticket_id) => format!("{session}:{ticket_id}"),
        None => format!("{session}:session"),
    }
}

/// What stands for "this MCP session" in a default run key. A local server
/// process serves one client session, so it is the process. The hosted
/// gateway uses the transport's session id; for a client on the stateless
/// contract, which has none, the token the client sent to name its
/// connection, together with the caller (`mcp_client::client_session`: the
/// token alone is never used, so a token chosen under one key cannot name a
/// session of another); else the caller alone, which is every session of
/// that key. Each is a digest: none is sent as it is.
pub(crate) fn session_part(is_http: bool) -> String {
    if !is_http {
        static PROCESS: OnceLock<String> = OnceLock::new();
        return PROCESS
            .get_or_init(|| Uuid::new_v4().simple().to_string())
            .clone();
    }
    if let Some(id) = mcp_client::get_task_mcp_session_id().filter(|id| !id.trim().is_empty()) {
        return format!("s-{}", short_digest(&id));
    }
    let caller = mcp_client::get_task_caller_cache_identity();
    if let (Some(caller), Some(token)) = (caller.as_deref(), mcp_client::client_session::current())
    {
        return format!(
            "s-{}",
            short_digest(&format!("client-session\n{caller}\n{token}"))
        );
    }
    match caller {
        Some(caller) => format!("c-{}", short_digest(&caller)),
        None => "mcp".to_string(),
    }
}

/// Whether a session part stands for one session of a client (see
/// [`session_part`]): a local server's process or a hosted `s-` part. A `c-`
/// part is every session of a key, and `mcp` is everyone.
fn is_one_session(session: &str, is_http: bool) -> bool {
    !is_http || session.starts_with("s-")
}

/// Whether a report names no run: no `run_key` and no `ticket_id`.
fn names_no_run(input: &ProgressInput) -> bool {
    text_argument(input.run_key.as_deref(), MAX_RUN_KEY).is_none()
        && text_argument(input.ticket_id.as_deref(), MAX_RUN_KEY).is_none()
}

fn short_digest(text: &str) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;

    let digest = Sha256::digest(text.as_bytes());
    let mut encoded = String::with_capacity(16);
    for byte in digest.iter().take(8) {
        write!(&mut encoded, "{byte:02x}").expect("writing to a string cannot fail");
    }
    encoded
}

fn run_memo_key(workspace_id: Uuid, run_key: &str) -> String {
    format!("{workspace_id}:{run_key}")
}

/// Drop the runs reported longest ago once more than
/// [`MAX_REMEMBERED_RUNS`] are remembered; a run still kept alive stays.
fn forget_oldest(runs: &mut HashMap<String, RunMemo>) {
    if runs.len() <= MAX_REMEMBERED_RUNS {
        return;
    }
    let mut idle: Vec<(Instant, String)> = runs
        .iter()
        .filter(|(_, memo)| memo.beat.is_none())
        .map(|(key, memo)| (memo.reported_at, key.clone()))
        .collect();
    idle.sort();
    let excess = runs.len() - MAX_REMEMBERED_RUNS;
    for (_, key) in idle.into_iter().take(excess) {
        runs.remove(&key);
    }
}

/// The `agent` of a report. A configured agent name is the agent as the
/// person named it. Otherwise the agent is this session of the client: its
/// name is the client's name and four characters of the session part
/// ("Claude Code 3fa9"), its `id` the client and eight, so two sessions are
/// two agents and one session is always the same one. Nothing of the machine
/// (no folder, no host name) is in either. `runtime` is always `mcp`;
/// `client` is the connected client's name when it gave one.
pub(crate) fn agent_identity(
    handle: Option<String>,
    client: Option<String>,
    session: &str,
) -> Value {
    let mut agent = Map::new();
    match handle {
        Some(handle) => {
            agent.insert("name".into(), json!(handle));
        }
        None => {
            let shown = client
                .as_deref()
                .map(shown_client_name)
                .unwrap_or_else(|| DEFAULT_AGENT_NAME.to_string());
            match session_tag(session) {
                Some(tag) => {
                    let client_id = client.as_deref().unwrap_or(MCP_RUNTIME);
                    agent.insert("id".into(), json!(format!("{client_id}:{}", &tag[..8])));
                    agent.insert("name".into(), json!(format!("{shown} {}", &tag[..4])));
                }
                None => {
                    agent.insert("name".into(), json!(shown));
                }
            }
        }
    }
    agent.insert("runtime".into(), json!(MCP_RUNTIME));
    if let Some(client) = client {
        agent.insert("client".into(), json!(client));
    }
    Value::Object(agent)
}

/// The agent a session is reported as, for callers outside this module (the
/// hooks' tests hold the hooks to it).
pub fn agent_for_session(handle: Option<String>, client: Option<String>, session: &str) -> Value {
    agent_identity(handle, client, session)
}

/// The run key a report without `run_key` goes to (see [`run_key_of`]).
pub fn default_run_key(session: &str, ticket_id: Option<Uuid>) -> String {
    run_key_of(None, session, ticket_id)
}

/// The run key a report goes to: the one the call gave, else the default.
pub fn run_key_for(given: Option<&str>, session: &str, ticket_id: Option<Uuid>) -> String {
    run_key_of(given, session, ticket_id)
}

/// How `init`'s text names the session for ContextStream's hooks: this label,
/// a colon, and the `run_progress` object on the same line. A local server's
/// answers reach a hook as text only, so the hooks read it there.
pub const IDENTITY_LINE: &str =
    "Live progress identity (read by ContextStream's hooks; nothing for you to do)";

/// The hexadecimal part of a session part (`s-` and `c-` mark where the
/// hosted gateway took it from), when it has one.
fn session_tag(session: &str) -> Option<&str> {
    let tag = session
        .strip_prefix("s-")
        .or_else(|| session.strip_prefix("c-"))
        .unwrap_or(session);
    (tag.len() >= 8 && tag.chars().all(|c| c.is_ascii_hexdigit())).then_some(tag)
}

/// A client's self-reported name as a person reads it: the usual clients by
/// their product names, any other as it named itself.
fn shown_client_name(raw: &str) -> String {
    let lower = raw.trim().to_ascii_lowercase();
    let known = if lower == "claude-code" || lower == "claude code" {
        "Claude Code"
    } else if lower.starts_with("claude") {
        "Claude"
    } else if lower.starts_with("cursor") {
        "Cursor"
    } else if lower.starts_with("codex") {
        "Codex"
    } else if lower.starts_with("windsurf") {
        "Windsurf"
    } else if lower.starts_with("vscode") || lower.starts_with("visual studio code") {
        "VS Code"
    } else if lower.starts_with("gemini") {
        "Gemini CLI"
    } else if lower.starts_with("antigravity") {
        "Antigravity"
    } else if lower.starts_with("github-copilot") || lower.starts_with("copilot") {
        "GitHub Copilot"
    } else if lower.starts_with("opencode") {
        "OpenCode"
    } else if lower.starts_with("cline") {
        "Cline"
    } else if lower == "zed" {
        "Zed"
    } else {
        return crate::client_identity::display_client_name(raw).unwrap_or_else(|| raw.to_string());
    };
    known.to_string()
}

/// What `init` tells ContextStream's hooks about this session's runs: the
/// session part default run keys start with, and the agent reports name.
/// `None` when live progress is off on this machine.
pub(crate) async fn session_identity(
    client: &ContextStreamClient,
    session: &SessionManager,
) -> Option<Value> {
    if !reporting_enabled() {
        return None;
    }
    let is_http = client.config().await.is_http_transport;
    let part = session_part(is_http);
    let folder = session.state().await.folder_path;
    let handle = if is_http {
        mcp_client::agent_handle::agent_handle_of_folder(folder.as_deref())
    } else {
        let folder = folder.or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|path| path.to_string_lossy().into_owned())
        });
        mcp_client::agent_handle::agent_handle(folder.as_deref())
    };
    let agent = agent_identity(handle, crate::client_identity::current_client_name(), &part);
    Some(json!({ "session": part, "agent": agent }))
}

/// Whether reporting is on: off only when `CONTEXTSTREAM_RUN_PROGRESS` says so.
fn reporting_enabled() -> bool {
    reporting_enabled_by(std::env::var(ENABLED_ENV).ok().as_deref())
}

fn reporting_enabled_by(setting: Option<&str>) -> bool {
    !setting.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "no" | "off" | "disabled"
        )
    })
}

// ============================================================================
// Arguments
// ============================================================================

fn truncated(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        text.chars().take(max).collect()
    }
}

/// A text argument: trimmed, `None` when empty, cut to `max` characters.
fn text_argument(value: Option<&str>, max: usize) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| truncated(value, max))
}

fn text_of(value: &Value, keys: &[&str], max: usize) -> Option<String> {
    keys.iter()
        .find_map(|key| text_argument(value.get(*key).and_then(Value::as_str), max))
}

/// `-` or a space for `_`, any case: `In-Progress` is `in_progress`.
fn word(raw: &str) -> String {
    raw.trim().to_ascii_lowercase().replace(['-', ' '], "_")
}

fn scope_id(raw: Option<&str>, field: &str) -> Result<Option<Uuid>> {
    match raw.map(str::trim).filter(|raw| !raw.is_empty()) {
        None => Ok(None),
        Some(raw) => Uuid::parse_str(raw)
            .map(Some)
            .map_err(|_| Error::Validation(format!("{field} must be a UUID"))),
    }
}

fn id_argument(raw: Option<&str>, field: &str, what: &str) -> Result<Option<Uuid>> {
    match raw.map(str::trim).filter(|raw| !raw.is_empty()) {
        None => Ok(None),
        Some(raw) => Uuid::parse_str(raw)
            .map(Some)
            .map_err(|_| Error::Validation(format!("{field} must be {what}."))),
    }
}

fn status_argument(raw: Option<&str>) -> Result<Option<&'static str>> {
    let Some(raw) = raw.map(str::trim).filter(|raw| !raw.is_empty()) else {
        return Ok(None);
    };
    let key = word(raw);
    if let Some(status) = RUN_STATUSES.iter().copied().find(|status| *status == key) {
        return Ok(Some(status));
    }
    // What other tools call the same states.
    let status = match key.as_str() {
        "done" | "complete" | "finished" => "completed",
        "in_progress" | "running" => "working",
        "blocked" | "waiting" => "waiting_input",
        "canceled" => "cancelled",
        "error" => "failed",
        _ => {
            return Err(Error::Validation(format!(
                "status must be one of {}, not {:?}.",
                RUN_STATUSES.join(", "),
                one_line(raw)
            )))
        }
    };
    Ok(Some(status))
}

/// An object an agent sent for an argument the schema lists as a string. The
/// server's input normalizer hands such an object over as its JSON text, so
/// without this a step given as `{label, kind}` would be stored with that
/// JSON as its label.
fn object_sent_as_text(text: &str) -> Option<Value> {
    let text = text.trim();
    if !text.starts_with('{') {
        return None;
    }
    serde_json::from_str::<Value>(text)
        .ok()
        .filter(Value::is_object)
}

fn step_argument(value: Option<&Value>) -> Result<Option<Value>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => match object_sent_as_text(text) {
            Some(object) => step_argument(Some(&object)),
            None => Ok(text_argument(Some(text), MAX_LABEL).map(Value::String)),
        },
        Some(value @ Value::Object(_)) => {
            let label = text_of(value, &["label", "text", "title", "step", "name"], MAX_LABEL)
                .ok_or_else(|| {
                    Error::Validation(
                        "step needs a label: what you are doing now, in plain words (step=\"Running the tests\", or step={\"label\": \"...\", \"detail\": \"...\"})."
                            .to_string(),
                    )
                })?;
            let mut step = Map::new();
            step.insert("label".into(), json!(label));
            if let Some(detail) = text_of(value, &["detail"], MAX_DETAIL) {
                step.insert("detail".into(), json!(detail));
            }
            if let Some(kind) = value.get("kind").and_then(Value::as_str).map(word) {
                let kind = STEP_KINDS
                    .iter()
                    .copied()
                    .find(|known| *known == kind)
                    .unwrap_or("other");
                step.insert("kind".into(), json!(kind));
            }
            if let Some(status) = value.get("status").and_then(Value::as_str).map(word) {
                if let Some(status) = STEP_STATUSES.iter().copied().find(|known| *known == status) {
                    step.insert("status".into(), json!(status));
                }
            }
            Ok(Some(Value::Object(step)))
        }
        Some(_) => Err(Error::Validation(
            "step must be what you are doing now, in plain words (step=\"Running the tests\")."
                .to_string(),
        )),
    }
}

fn plan_state(raw: &str, index: usize) -> Result<&'static str> {
    let key = word(raw);
    if let Some(state) = PLAN_ITEM_STATES.iter().copied().find(|state| *state == key) {
        return Ok(state);
    }
    // What the checklists of coding agents call the same states.
    Ok(match key.as_str() {
        "completed" | "complete" | "finished" | "checked" => "done",
        "doing" | "active" | "current" | "working" | "started" => "in_progress",
        "pending" | "todo" | "to_do" | "not_started" | "open" | "queued" | "planned" => "next",
        "skip" | "cancelled" | "canceled" | "dropped" => "skipped",
        _ => {
            return Err(Error::Validation(format!(
                "plan[{index}].state must be done, in_progress, next or skipped, not {:?}. Say what blocks you with waiting={{\"on\": \"...\", \"question\": \"...\"}}.",
                one_line(raw)
            )))
        }
    })
}

fn plan_argument(value: Option<&Value>, notes: &mut Vec<String>) -> Result<Option<Vec<Value>>> {
    let items = match value {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Array(items)) => items,
        Some(_) => {
            return Err(Error::Validation(
                "plan must be your checklist: [{\"title\": \"...\", \"state\": \"done|in_progress|next\"}]."
                    .to_string(),
            ))
        }
    };
    if items.len() > MAX_PLAN_ITEMS {
        notes.push(format!(
            "The checklist has {} items; only the first {MAX_PLAN_ITEMS} were sent.",
            items.len()
        ));
    }
    let mut plan = Vec::new();
    for (index, item) in items.iter().take(MAX_PLAN_ITEMS).enumerate() {
        let missing_title = || {
            Error::Validation(format!(
                "plan[{index}] needs a title: {{\"title\": \"...\", \"state\": \"done|in_progress|next\"}}."
            ))
        };
        let entry = match item {
            Value::String(title) => {
                let title = text_argument(Some(title), MAX_LABEL).ok_or_else(missing_title)?;
                json!({ "title": title, "state": "next" })
            }
            Value::Object(_) => {
                let title = text_of(
                    item,
                    &["title", "content", "step", "text", "label", "name", "task"],
                    MAX_LABEL,
                )
                .ok_or_else(missing_title)?;
                let state = match item
                    .get("state")
                    .or_else(|| item.get("status"))
                    .and_then(Value::as_str)
                    .filter(|state| !state.trim().is_empty())
                {
                    Some(state) => plan_state(state, index)?,
                    None => "next",
                };
                let mut entry = Map::new();
                match item.get("id") {
                    Some(Value::String(id)) if !id.trim().is_empty() => {
                        entry.insert("id".into(), json!(id.trim()));
                    }
                    Some(Value::Number(id)) => {
                        entry.insert("id".into(), json!(id.to_string()));
                    }
                    _ => {}
                }
                entry.insert("title".into(), json!(title));
                entry.insert("state".into(), json!(state));
                Value::Object(entry)
            }
            _ => return Err(missing_title()),
        };
        plan.push(entry);
    }
    Ok(Some(plan))
}

/// `percent` as a number from 0 to 100. One that is not is left out and
/// said, not refused: a wrong percentage is not worth losing the report.
fn percent_argument(value: Option<&Value>, notes: &mut Vec<String>) -> Option<Value> {
    let number = match value {
        None | Some(Value::Null) => return None,
        Some(Value::Number(number)) => number.as_f64(),
        Some(Value::String(text)) => text.trim().trim_end_matches('%').trim().parse::<f64>().ok(),
        Some(_) => None,
    };
    match number {
        Some(number) if (0.0..=100.0).contains(&number) => {
            if number.fract() == 0.0 {
                Some(json!(number as i64))
            } else {
                Some(json!(number))
            }
        }
        _ => {
            notes.push(
                "percent was left out: it must be a number from 0 to 100, given only when it is real."
                    .to_string(),
            );
            None
        }
    }
}

fn link_kind(raw: Option<&str>, url: &str) -> &'static str {
    let key = raw.map(word).unwrap_or_default();
    if let Some(kind) = LINK_KINDS.iter().copied().find(|kind| *kind == key) {
        return kind;
    }
    match key.as_str() {
        "pull_request" | "pull" | "merge_request" | "mr" => "pr",
        "deployment" => "deploy",
        "docs" | "document" => "doc",
        "" if url.contains("/pull/") || url.contains("/merge_requests/") => "pr",
        _ => "other",
    }
}

fn links_argument(
    value: Option<&Value>,
    field: &str,
    notes: &mut Vec<String>,
) -> Result<Option<Vec<Value>>> {
    let single;
    let items: &[Value] = match value {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Array(items)) => items.as_slice(),
        Some(other @ (Value::String(_) | Value::Object(_))) => {
            single = [other.clone()];
            &single[..]
        }
        Some(_) => {
            return Err(Error::Validation(format!(
                "{field} must be a list: [{{\"kind\": \"pr|preview|deploy|doc|other\", \"url\": \"https://...\"}}]."
            )))
        }
    };
    if items.len() > MAX_LINKS {
        notes.push(format!(
            "{field} has {} items; only the first {MAX_LINKS} were sent.",
            items.len()
        ));
    }
    let mut links = Vec::new();
    for (index, item) in items.iter().take(MAX_LINKS).enumerate() {
        let (url, kind, label) = match item {
            Value::String(url) => (url.trim().to_string(), None, None),
            Value::Object(_) => (
                text_of(item, &["url", "href", "link"], 2000).unwrap_or_default(),
                item.get("kind")
                    .or_else(|| item.get("type"))
                    .and_then(Value::as_str),
                text_of(item, &["label", "title", "name"], MAX_TITLE),
            ),
            _ => (String::new(), None, None),
        };
        // People open these from the live view: only web links are shown.
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Err(Error::Validation(format!(
                "{field}[{index}].url must be an http(s) URL: {{\"kind\": \"pr|preview|deploy|doc|other\", \"url\": \"https://...\"}}."
            )));
        }
        let mut link = Map::new();
        link.insert("kind".into(), json!(link_kind(kind, &url)));
        link.insert("url".into(), json!(url));
        if let Some(label) = label {
            link.insert("label".into(), json!(label));
        }
        links.push(Value::Object(link));
    }
    Ok(Some(links))
}

fn milestone_argument(value: Option<&Value>) -> Result<Option<Value>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => match object_sent_as_text(text) {
            Some(object) => milestone_argument(Some(&object)),
            None => Ok(text_argument(Some(text), MAX_LABEL).map(Value::String)),
        },
        Some(value @ Value::Object(_)) => {
            let text = text_of(value, &["text", "title", "label"], MAX_LABEL).ok_or_else(|| {
                Error::Validation(
                    "milestone needs its text: milestone=\"Tests green\", or {\"text\": \"...\", \"links\": [...]}."
                        .to_string(),
                )
            })?;
            let mut milestone = Map::new();
            milestone.insert("text".into(), json!(text));
            let mut dropped = Vec::new();
            if let Some(links) =
                links_argument(value.get("links"), "milestone.links", &mut dropped)?
            {
                milestone.insert("links".into(), Value::Array(links));
            }
            Ok(Some(Value::Object(milestone)))
        }
        Some(_) => Err(Error::Validation(
            "milestone must be a few words: milestone=\"Tests green\".".to_string(),
        )),
    }
}

fn waiting_argument(value: Option<&Value>) -> Result<Option<Waiting>> {
    let waiting_on = |raw: &str| -> Result<&'static str> {
        let key = word(raw);
        WAITING_ON
            .iter()
            .copied()
            .find(|on| *on == key)
            .ok_or_else(|| {
                Error::Validation(format!(
                    "waiting.on must be one of {}, not {:?}.",
                    WAITING_ON.join(", "),
                    one_line(raw)
                ))
            })
    };
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(false)) => Ok(Some(Waiting::Cleared)),
        Some(Value::Bool(true)) => Ok(Some(Waiting::On(json!({ "on": "person" })))),
        Some(Value::String(text)) => {
            let text = text.trim();
            let key = word(text);
            if matches!(key.as_str(), "" | "none" | "no" | "false" | "nothing") {
                Ok(Some(Waiting::Cleared))
            } else if let Some(on) = WAITING_ON.iter().copied().find(|on| *on == key) {
                Ok(Some(Waiting::On(json!({ "on": on }))))
            } else {
                // A sentence is the question, asked of a person.
                Ok(Some(Waiting::On(
                    json!({ "on": "person", "question": truncated(text, MAX_DETAIL) }),
                )))
            }
        }
        Some(value @ Value::Object(fields)) => {
            if fields.is_empty() {
                return Ok(Some(Waiting::Cleared));
            }
            let on = match value
                .get("on")
                .or_else(|| value.get("for"))
                .and_then(Value::as_str)
                .filter(|on| !on.trim().is_empty())
            {
                Some(on) => waiting_on(on)?,
                None => "person",
            };
            let mut waiting = Map::new();
            waiting.insert("on".into(), json!(on));
            if let Some(question) = text_of(value, &["question", "message", "text"], MAX_DETAIL) {
                waiting.insert("question".into(), json!(question));
            }
            Ok(Some(Waiting::On(Value::Object(waiting))))
        }
        Some(_) => Err(Error::Validation(
            "waiting must be {\"on\": \"person|approval|budget|rate_limit|sign_in|dependency|error\", \"question\": \"...\"}, or false when you are working again."
                .to_string(),
        )),
    }
}

// ============================================================================
// Answers in plain words
// ============================================================================

/// A result that says, calmly, that progress was not recorded (or read).
/// It is not an error: the agent's work is unaffected and nothing should be
/// retried.
fn unrecorded(headline: &str, reason: &str, status: Option<u16>, why: &str) -> ToolResult {
    let recorded_key = if headline == "Not read" {
        "read"
    } else {
        "recorded"
    };
    let mut structured = Map::new();
    structured.insert(recorded_key.to_string(), json!(false));
    structured.insert("reason".to_string(), json!(reason));
    if let Some(status) = status {
        structured.insert("status".to_string(), json!(status));
    }
    ToolResult::with_structured(
        format!(
            "[PROGRESS] {headline}: {why}. Nothing else is affected: carry on with your work; there is no need to retry this call."
        ),
        Value::Object(structured),
    )
}

/// The API's own words, on one line and short.
fn api_words(message: &str) -> String {
    truncated(&one_line(message), 200)
}

/// A 404 that names what it could not find ("ticket not found") comes from
/// the route itself; one that does not is an API without the route.
fn names_a_missing_record(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "ticket",
        "loop",
        "plan",
        "step",
        "run",
        "workspace",
        "project",
    ]
    .iter()
    .any(|record| {
        [
            format!("{record} not found"),
            format!("{record}_not_found"),
            format!("{record} not visible"),
            format!("{record} is not visible"),
            format!("unknown {record}"),
            format!("no such {record}"),
        ]
        .iter()
        .any(|phrase| message.contains(phrase.as_str()))
    })
}

/// What an API failure means for a report or a read, as a calm result.
fn api_failure(headline: &str, error: &Error) -> ToolResult {
    match error {
        Error::Http {
            status: 404,
            message,
            ..
        } if names_a_missing_record(message) => unrecorded(
            headline,
            "not_found",
            Some(404),
            &format!(
                "ContextStream could not find what the call names ({})",
                api_words(message)
            ),
        ),
        Error::Http { status: 404, .. } => unrecorded(
            headline,
            "not_available",
            Some(404),
            "live progress is not available on this ContextStream server yet (the API answered 404)",
        ),
        Error::Http {
            status: status @ (401 | 403),
            message,
            ..
        } => unrecorded(
            headline,
            "not_allowed",
            Some(*status),
            &format!(
                "this key may not do that in the workspace (the API answered {status}: {})",
                api_words(message)
            ),
        ),
        Error::Http {
            status, message, ..
        } if *status < 500 => unrecorded(
            headline,
            "refused",
            Some(*status),
            &format!(
                "the API refused the call ({status}: {})",
                api_words(message)
            ),
        ),
        Error::Http { status, .. } => unrecorded(
            headline,
            "unavailable",
            Some(*status),
            &format!("the ContextStream API had a problem (it answered {status})"),
        ),
        Error::Network(_) | Error::Timeout(_) => unrecorded(
            headline,
            "unreachable",
            None,
            "the ContextStream API could not be reached in time",
        ),
        Error::RateLimited { .. } => unrecorded(
            headline,
            "rate_limited",
            Some(429),
            "the API is limiting this key's requests for the moment",
        ),
        other => unrecorded(
            headline,
            "failed",
            None,
            &truncated(&one_line(&other.to_string()), 200),
        ),
    }
}

fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

/// Whether the API's view of a run says it is over.
fn run_has_ended(answer: &Value) -> bool {
    let view = answer.get("run").unwrap_or(answer);
    str_field(view, "liveness") == Some("ended")
        || matches!(
            str_field(view, "state"),
            Some("stopped" | "finished" | "failed")
        )
}

/// What the API wrote, in a word each: `run.step` is "step".
fn written_words(answer: &Value) -> Option<Vec<String>> {
    let written = answer.get("written")?.as_array()?;
    Some(
        written
            .iter()
            .filter_map(Value::as_str)
            .filter(|kind| *kind != "run.heartbeat")
            .map(|kind| match kind {
                "run.started" => "run started".to_string(),
                "run.plan" => "checklist".to_string(),
                "run.resumed" => "run resumed".to_string(),
                other => one_line(other.strip_prefix("run.").unwrap_or(other)),
            })
            .collect(),
    )
}

/// A recorded report in a line or two: what was written, where the run
/// stands, and its id.
fn recorded_text(report: &Report, answer: &Value) -> String {
    let view = answer.get("run").unwrap_or(&Value::Null);
    let words = written_words(answer)
        .unwrap_or_else(|| report.gave.iter().map(|word| word.to_string()).collect());
    let mut head = if words.is_empty() {
        "[PROGRESS] Recorded: nothing new, so it counts as a sign of life.".to_string()
    } else {
        format!("[PROGRESS] Recorded: {}.", words.join(", "))
    };
    if let Some(state) = str_field(view, "state") {
        head.push_str(&format!(" state={}", one_line(state)));
    }
    if let (Some(done), Some(total)) = (
        view.get("plan_done").and_then(Value::as_i64),
        view.get("plan_total").and_then(Value::as_i64),
    ) {
        if total > 0 {
            head.push_str(&format!(" checklist={done}/{total}"));
        }
    }
    if let Some(run_id) = str_field(view, "run_id") {
        head.push_str(&format!(" run_id={}", one_line(run_id)));
    }
    let mut lines = vec![head];
    if let Some(ticket) = view.get("ticket").filter(|ticket| ticket.is_object()) {
        if let Some(status) = str_field(ticket, "status") {
            lines.push(format!(
                "Ticket {}: {}.",
                str_field(ticket, "title")
                    .map(|title| format!("{:?}", one_line(title)))
                    .or_else(|| str_field(ticket, "id").map(one_line))
                    .unwrap_or_else(|| "of this run".to_string()),
                one_line(status)
            ));
        }
    }
    lines.extend(report.notes.iter().cloned());
    if report.is_terminal() {
        lines.push("The run is finished; a later report on it opens it again.".to_string());
    } else if words.iter().any(|word| word == "run started") {
        lines.push(
            "People can watch this run live. Report again when your step or checklist changes, when you open a PR, when you are blocked, and when you finish (status and summary)."
                .to_string(),
        );
    }
    lines.join("\n")
}

/// One run on one line, for a list.
fn run_line(view: &Value) -> String {
    let mut line = format!(
        "- {}",
        str_field(view, "title")
            .map(one_line)
            .unwrap_or_else(|| "(untitled run)".to_string())
    );
    if let Some(agent) = view.get("agent") {
        if let Some(name) = str_field(agent, "name") {
            line.push_str(&format!(" — {}", one_line(name)));
            if let Some(client) = str_field(agent, "client") {
                line.push_str(&format!(" ({})", one_line(client)));
            }
        }
    }
    if let Some(state) = str_field(view, "state") {
        line.push_str(&format!(" state={}", one_line(state)));
    }
    if let Some(liveness) = str_field(view, "liveness") {
        line.push_str(&format!(" liveness={}", one_line(liveness)));
    }
    if let Some(step) = view
        .get("current_step")
        .and_then(|step| str_field(step, "label"))
    {
        line.push_str(&format!(" step={:?}", one_line(step)));
    }
    if let (Some(done), Some(total)) = (
        view.get("plan_done").and_then(Value::as_i64),
        view.get("plan_total").and_then(Value::as_i64),
    ) {
        if total > 0 {
            line.push_str(&format!(" checklist={done}/{total}"));
        }
    }
    if let Some(percent) = view.pointer("/progress/percent").and_then(Value::as_f64) {
        line.push_str(&format!(" {percent:.0}%"));
    }
    if let Some(on) = view
        .get("waiting")
        .and_then(|waiting| str_field(waiting, "on"))
    {
        line.push_str(&format!(" waiting_on={}", one_line(on)));
    }
    if let Some(ticket) = view
        .get("ticket")
        .and_then(|ticket| str_field(ticket, "id"))
    {
        line.push_str(&format!(" ticket={}", one_line(ticket)));
    }
    if let Some(run_id) = str_field(view, "run_id") {
        line.push_str(&format!(" run_id={}", one_line(run_id)));
    }
    line
}

fn runs_of(result: &Value) -> &[Value] {
    result
        .get("runs")
        .and_then(Value::as_array)
        .or_else(|| result.as_array())
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// The runs of a list, one line each.
fn list_text(result: &Value) -> String {
    let runs = runs_of(result);
    if runs.is_empty() {
        return "[PROGRESS] No runs in this scope.".to_string();
    }
    let mut text = format!(
        "[PROGRESS] {} run(s). Read one with progress(action=\"get\", run_id=\"<id>\").",
        runs.len()
    );
    for view in runs {
        text.push('\n');
        text.push_str(&run_line(view));
    }
    text
}

/// One run in full: its step, checklist, what it waits on, links, outcome.
fn run_text(view: &Value) -> String {
    let mut head = format!(
        "[PROGRESS] {}",
        str_field(view, "title")
            .map(one_line)
            .unwrap_or_else(|| "(untitled run)".to_string())
    );
    if let Some(state) = str_field(view, "state") {
        head.push_str(&format!(" state={}", one_line(state)));
    }
    if let Some(liveness) = str_field(view, "liveness") {
        head.push_str(&format!(" liveness={}", one_line(liveness)));
    }
    if let Some(run_id) = str_field(view, "run_id") {
        head.push_str(&format!(" run_id={}", one_line(run_id)));
    }
    let mut lines = vec![head];
    if let Some(agent) = view.get("agent") {
        if let Some(name) = str_field(agent, "name") {
            let mut line = format!("Agent: {}", one_line(name));
            if let Some(client) = str_field(agent, "client") {
                line.push_str(&format!(" ({})", one_line(client)));
            }
            lines.push(line);
        }
    }
    if let Some(step) = view.get("current_step").filter(|step| step.is_object()) {
        if let Some(label) = str_field(step, "label") {
            let mut line = format!("Step: {}", one_line(label));
            if let Some(detail) = str_field(step, "detail") {
                line.push_str(&format!(" — {}", one_line(detail)));
            }
            if let Some(status) = str_field(step, "status") {
                line.push_str(&format!(" [{}]", one_line(status)));
            }
            lines.push(line);
        }
    }
    let plan = view
        .get("plan")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    if !plan.is_empty() {
        let done = view
            .get("plan_done")
            .and_then(Value::as_i64)
            .unwrap_or_else(|| {
                plan.iter()
                    .filter(|item| str_field(item, "state") == Some("done"))
                    .count() as i64
            });
        lines.push(format!("Checklist: {done}/{} done", plan.len()));
        for item in plan {
            let mark = match str_field(item, "state") {
                Some("done") => "[x]",
                Some("in_progress") => "[>]",
                Some("skipped") => "[-]",
                _ => "[ ]",
            };
            lines.push(format!(
                "  {mark} {}",
                str_field(item, "title")
                    .map(one_line)
                    .unwrap_or_else(|| "(untitled)".to_string())
            ));
        }
    }
    if let Some(progress) = view.get("progress").filter(|progress| progress.is_object()) {
        let mut parts = Vec::new();
        if let Some(percent) = progress.get("percent").and_then(Value::as_f64) {
            parts.push(format!("{percent:.0}%"));
        }
        if let Some(remaining) = str_field(progress, "remaining") {
            parts.push(format!("remaining: {}", one_line(remaining)));
        }
        if !parts.is_empty() {
            lines.push(format!("Progress: {}", parts.join(", ")));
        }
    }
    if let Some(waiting) = view.get("waiting").filter(|waiting| waiting.is_object()) {
        let mut line = format!(
            "Waiting on: {}",
            str_field(waiting, "on")
                .map(one_line)
                .unwrap_or_else(|| "something".to_string())
        );
        if let Some(question) = str_field(waiting, "question") {
            line.push_str(&format!(" — {:?}", one_line(question)));
        }
        if let Some(since) = str_field(waiting, "since") {
            line.push_str(&format!(" (since {})", one_line(since)));
        }
        lines.push(line);
    }
    for link in view
        .get("links")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        if let Some(url) = str_field(link, "url") {
            lines.push(format!(
                "Link: {} {}",
                str_field(link, "kind")
                    .map(one_line)
                    .unwrap_or_else(|| "other".to_string()),
                one_line(url)
            ));
        }
    }
    if let Some(milestone) = view
        .get("last_milestone")
        .and_then(|milestone| str_field(milestone, "text"))
    {
        lines.push(format!("Last milestone: {}", one_line(milestone)));
    }
    if let Some(outcome) = view.get("outcome").filter(|outcome| outcome.is_object()) {
        let mut line = format!(
            "Outcome: {}",
            str_field(outcome, "status")
                .map(one_line)
                .unwrap_or_else(|| "ended".to_string())
        );
        if let Some(reason) = str_field(outcome, "reason") {
            line.push_str(&format!(" ({})", one_line(reason)));
        }
        if let Some(summary) = str_field(outcome, "summary") {
            line.push_str(&format!(" — {}", one_line(summary)));
        }
        lines.push(line);
    }
    if let Some(ticket) = view.get("ticket").filter(|ticket| ticket.is_object()) {
        let mut line = format!(
            "Ticket: {}",
            str_field(ticket, "title")
                .map(one_line)
                .or_else(|| str_field(ticket, "id").map(one_line))
                .unwrap_or_else(|| "(untitled)".to_string())
        );
        if let Some(status) = str_field(ticket, "status") {
            line.push_str(&format!(" [{}]", one_line(status)));
        }
        lines.push(line);
    }
    if let Some(last) = str_field(view, "last_signal_at") {
        lines.push(format!("Last signal: {}", one_line(last)));
    }
    lines.join("\n")
}

#[cfg(test)]
#[path = "progress_tests.rs"]
mod tests;
