//! Live run progress from the hooks.
//!
//! The `progress` tool lets a model say what it is doing. A model forgets,
//! and a session on the hosted gateway has no local server to keep a run
//! alive between its reports, so a session that is working read as stalled.
//! The hooks are the one piece of ContextStream that runs beside every
//! session, so they keep a session's runs honest:
//!
//! - **Keep-alive.** While a session has an open run, one detached process
//!   per session (`contextstream-mcp hook run-progress-beat`) sends a bare
//!   heartbeat for it every 30 s: who and which run, nothing about the work.
//!   It checks every 2 s that the session's own process still exists and
//!   leaves when it does not.
//! - **Honest end.** When the session ends, a subagent ends, or the session's
//!   process is gone, its open runs end as `stopped` with a fixed sentence.
//!   No summary is invented.
//! - **A run that starts by itself.** When a tool call of the session moves a
//!   ticket to `in_progress`, a run for that ticket starts.
//! - **The session's own run.** A session whose model never calls `progress`
//!   and takes up no ticket was on no floor at all, however long it worked.
//!   Once such a session has worked for two minutes of a turn
//!   ([`session_run_after_ms`]) with no run, the hooks open its own: the key
//!   and the agent a `progress` report that names no run would use, the
//!   workspace and project `init` resolved, and fixed words for a title
//!   ("Working in" and the project's name). The two minutes are the model's:
//!   a run's title is set when it starts, so a report of the model's that
//!   comes first keeps its own. A later report that names no run lands on
//!   this run and makes it the model's; work reported as another run (a
//!   ticket's, a named one) ends it by a fixed sentence. At most once for a
//!   session part, and never after the model reported or a ticket was taken
//!   up. These two are what reports a session whose model never called
//!   `progress`.
//! - **Turn state.** When the model's turn ends, the session's runs wait on a
//!   person (no question text); the next prompt or tool call says working.
//! - **A coarse step.** Only after ten minutes without a step from the model:
//!   one of the fixed sentences in [`super::run_step_labels`], picked by the
//!   tool's name. On the session's own run, while the model has reported
//!   nothing, from the first tool call: no step of the model's stands there.
//!
//! The hooks never choose who they are. The server says it: `init`'s result
//! and every `progress` answer carry the server's session part and agent, and
//! the hooks use the same run key scheme and the same agent as the tool, so
//! the model's own report and the hooks' signals land on one run under one
//! name.
//!
//! Tool-call hooks never touch the network: they update a small state file
//! (`~/.contextstream/run-progress/<digest of the host session id>.json`)
//! and queue reports there; the detached process sends them.
//!
//! All of it is off as shipped ([`AUTOMATIC_REPORTS_DEFAULT`]): the hooks
//! write no state, start no process and send nothing, and live progress is
//! only what the model reports through the `progress` tool. A person turns it
//! on for a machine with `CONTEXTSTREAM_RUN_PROGRESS_AUTO=on`.
//! `CONTEXTSTREAM_RUN_PROGRESS=off` turns live progress off altogether, the
//! tool included, whatever the other switch says. An API that refuses a
//! report (401, 403 or 404) ends reporting for the session.
//!
//! **Signs of life** (`CONTEXTSTREAM_RUN_PROGRESS_SIGNS=on`, off as shipped,
//! [`SIGNS_DEFAULT`]) are the one piece of the above that needs no timer and
//! says nothing of its own: for a run the model reported through `progress`
//! (known by the id its answer named), a real hook event (a tool call starts
//! or ends, a prompt arrives) is a sign the session is at work, and the
//! detached process sends it as a bare report, at most once a minute per run
//! ([`sign_every_ms`]), under the agent the run was reported as. Nothing for
//! a run that waits or that its agent has not reported on for four hours,
//! for a run a subagent reported last once that subagent stopped, or for a
//! default run of a session part that ended (the client reconnected); no
//! run started on a ticket, no waiting at a turn's end, no coarse step, no
//! end. With the automatic reports on, they do the work instead.

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::common::{self, ApiConfig};
use super::durable_capture::{sha256_hex, state_dir, with_locked_json};
use super::run_step_labels::step_for_tool;

/// Whether the hooks report by themselves: the run that starts on a ticket
/// take-up, the keep-alive, the turn state, the coarse step and the honest
/// end. This is the one place that decides the default;
/// `CONTEXTSTREAM_RUN_PROGRESS_AUTO` (`on` or `off`) overrides it on a
/// machine. Off as shipped: nothing is reported for a person who has not
/// asked for it.
pub(crate) const AUTOMATIC_REPORTS_DEFAULT: bool = false;

/// Whether the hooks send a sign of life for the runs a session reported,
/// without the automatic reports: after a real hook event (a tool call
/// starts or ends, a prompt arrives), at most once per [`sign_every_ms`] per
/// run, who and which run and nothing else. No timer, no waiting state, no
/// run started or ended. Off as shipped (decision 47301037: nothing new
/// reaches a customer before it has been seen working in our own
/// workspaces); `CONTEXTSTREAM_RUN_PROGRESS_SIGNS=on` turns it on for a
/// machine.
pub(crate) const SIGNS_DEFAULT: bool = false;

/// Overrides [`SIGNS_DEFAULT`] on this machine.
const SIGNS_ENV: &str = "CONTEXTSTREAM_RUN_PROGRESS_SIGNS";

/// A run its agent has not reported on for this long gets no sign of life:
/// silence is silence (the hosted gateway's limit, and a local server's).
const SIGNS_FOR_MS: u64 = 4 * 60 * 60 * 1000;

/// Turns live progress off on this machine (shared with the `progress` tool).
const ENABLED_ENV: &str = "CONTEXTSTREAM_RUN_PROGRESS";
/// Overrides [`AUTOMATIC_REPORTS_DEFAULT`] on this machine.
const AUTOMATIC_ENV: &str = "CONTEXTSTREAM_RUN_PROGRESS_AUTO";
/// Names the session's own process when the hook cannot find it by itself.
const HOST_PID_ENV: &str = "CONTEXTSTREAM_RUN_PROGRESS_HOST_PID";
/// Seconds between heartbeats (1..=120; 30 when unset).
const BEAT_SECS_ENV: &str = "CONTEXTSTREAM_RUN_PROGRESS_BEAT_SECS";

/// Hook name of the detached keep-alive process.
pub(crate) const BEAT_HOOK: &str = "run-progress-beat";

/// Hook name of the event hook: live progress and nothing else, for a host
/// whose other hook events ContextStream does not use. Setup installs it for
/// Codex ([`handle_event_hook`]).
pub(crate) const EVENT_HOOK: &str = "run-progress";

const DEFAULT_BEAT_SECS: u64 = 30;
/// A run that waits on a person is signalled this many beats apart.
const WAITING_BEATS: u64 = 20;
/// How often the keep-alive process looks at the state file and at the
/// session's process.
const TICK: Duration = Duration::from_secs(2);
/// A coarse step is reported only this long after the model's last step.
const COARSE_STEP_AFTER_MS: u64 = 10 * 60 * 1000;
/// At most one coarse step per run in this span.
const COARSE_STEP_EVERY_MS: u64 = 60 * 1000;
/// Where the session's process cannot be watched (Windows), heartbeats stop
/// this long after the last hook event, or this long after a tool call that
/// never reported back.
const QUIET_WITHOUT_HOST_MS: u64 = 10 * 60 * 1000;
const IN_FLIGHT_WITHOUT_HOST_MS: u64 = 60 * 60 * 1000;
/// A session whose process lives but whose hooks have said nothing for this
/// long is no longer kept alive.
const SILENT_SESSION_MS: u64 = 24 * 60 * 60 * 1000;
/// The keep-alive process leaves this long after the last run closed.
const IDLE_EXIT_MS: u64 = 60 * 1000;
/// Sends that fail in a row (API unreachable) before the process leaves; the
/// next hook event starts another.
const FAILURE_LIMIT: u32 = 5;
const MAX_RUNS: usize = 16;
const MAX_PENDING: usize = 32;
const SEND_TIMEOUT: Duration = Duration::from_secs(5);
const RESPAWN_AFTER_MS: u64 = 5_000;
const STALE_STATE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

const TERMINAL: &[&str] = &["completed", "partial", "stopped", "failed", "cancelled"];

/// The fixed sentences a run ends with when nobody said how it ended.
pub(crate) const SESSION_ENDED: &str = "The session ended without a final report.";
pub(crate) const HOST_GONE: &str = "The session's process ended without a final report.";
pub(crate) const AGENT_ENDED: &str = "The agent ended without a final report.";
pub(crate) const RECONNECTED: &str = "The session reconnected; its work continues as another run.";
/// The end of the run the hooks opened for a session that had reported
/// nothing, when its work is reported as another run after all.
pub(crate) const MOVED_ON: &str = "The session reported its work as another run.";

/// Seconds a session works in a turn, with no run of its own, before the
/// hooks open one for it (0..=3600; 120 when unset).
const SESSION_RUN_AFTER_ENV: &str = "CONTEXTSTREAM_RUN_PROGRESS_SESSION_AFTER_SECS";
/// Two minutes: a model that follows its rules reports within its first few
/// tool calls, and a run's title is set when the run starts, so the hooks
/// wait for the model's own before they start one under fixed words.
const DEFAULT_SESSION_RUN_AFTER_SECS: u64 = 120;
const MAX_SESSION_RUN_AFTER_SECS: u64 = 60 * 60;
/// The title of the run the hooks open for a session, before the project's
/// name.
const SESSION_RUN_TITLE: &str = "Working";
/// The most of a project's name a title carries.
const MAX_PROJECT_NAME: usize = 80;

// ============================================================================
// Settings
// ============================================================================

/// What this machine allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Settings {
    /// Whether the hooks report by themselves (the automatic reports). Off:
    /// no state, no process, nothing sent, unless `signs` is on.
    pub enabled: bool,
    /// Whether the hooks send signs of life ([`SIGNS_DEFAULT`]).
    pub signs: bool,
}

impl Settings {
    pub(crate) fn from_env() -> Self {
        Self::from_switches(
            std::env::var(ENABLED_ENV).ok().as_deref(),
            std::env::var(AUTOMATIC_ENV).ok().as_deref(),
            std::env::var(SIGNS_ENV).ok().as_deref(),
        )
    }

    /// `live` is what `CONTEXTSTREAM_RUN_PROGRESS` says (off wins over
    /// everything); `automatic` is what `CONTEXTSTREAM_RUN_PROGRESS_AUTO` says.
    #[cfg(test)]
    pub(crate) fn from_values(live: Option<&str>, automatic: Option<&str>) -> Self {
        Self::from_switches(live, automatic, None)
    }

    /// The same, with what `CONTEXTSTREAM_RUN_PROGRESS_SIGNS` says.
    pub(crate) fn from_switches(
        live: Option<&str>,
        automatic: Option<&str>,
        signs: Option<&str>,
    ) -> Self {
        let live = switch(live).unwrap_or(true);
        Self {
            enabled: live && switch(automatic).unwrap_or(AUTOMATIC_REPORTS_DEFAULT),
            signs: live && switch(signs).unwrap_or(SIGNS_DEFAULT),
        }
    }

    /// Signs of life only: the automatic reports are off.
    pub(crate) fn signs_only(self) -> bool {
        !self.enabled && self.signs
    }
}

fn switch(value: Option<&str>) -> Option<bool> {
    match value?.trim().to_ascii_lowercase().as_str() {
        "0" | "false" | "no" | "off" | "disabled" => Some(false),
        "1" | "true" | "yes" | "on" | "enabled" => Some(true),
        _ => None,
    }
}

fn beat_secs() -> u64 {
    std::env::var(BEAT_SECS_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .map(|secs| secs.clamp(1, 120))
        .unwrap_or(DEFAULT_BEAT_SECS)
}

/// How long a session works in a turn with no run of its own before the
/// hooks open one ([`SESSION_RUN_AFTER_ENV`]).
pub(crate) fn session_run_after_ms() -> u64 {
    session_run_after_ms_of(std::env::var(SESSION_RUN_AFTER_ENV).ok().as_deref())
}

pub(crate) fn session_run_after_ms_of(setting: Option<&str>) -> u64 {
    setting
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|secs| *secs <= MAX_SESSION_RUN_AFTER_SECS)
        .unwrap_or(DEFAULT_SESSION_RUN_AFTER_SECS)
        * 1000
}

/// ContextStream's own tools: bookkeeping, not the session's work.
fn is_contextstream_tool(tool: &str) -> bool {
    tool.to_ascii_lowercase().contains("contextstream")
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

// ============================================================================
// State
// ============================================================================

/// One open run of a session.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Run {
    /// The run's `source_run_id`, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// The run's id, as a `progress` answer named it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub workspace_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket_id: Option<String>,
    /// The subagent (`agent_id`) whose run this is; `None` is the session's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// A subagent's own agent. The session's runs use the session's agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<Value>,
    #[serde(default)]
    pub started_ms: u64,
    /// When the model last gave a step for the run.
    #[serde(default)]
    pub model_step_ms: u64,
    #[serde(default)]
    pub coarse_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coarse_kind: Option<String>,
    /// The hooks said the run waits on a person (the turn ended).
    #[serde(default)]
    pub hook_waiting: bool,
    /// The model said the run waits; the hooks leave that alone.
    #[serde(default)]
    pub model_waiting: bool,
    /// When the model last reported the run: a report is a signal of its own.
    #[serde(default)]
    pub reported_ms: u64,
    /// The run's agent as the report's answer named it. A sign of life
    /// carries it, so it renames nothing (every signal sets a run's agent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_agent: Option<Value>,
    /// When the last sign of life for the run went out.
    #[serde(default)]
    pub signed_ms: u64,
    /// The subagent (`agent_id`) whose call reported the run last; `None`
    /// is the session's main agent. When that subagent stops, the run gets
    /// no more signs of life.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_by: Option<String>,
    /// The hooks opened this run for a session whose model had reported
    /// nothing ([`SessionFile::open_session_run`]), and the model has not
    /// reported on it since.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unreported: bool,
    /// A run reported without a project: the project the session works in,
    /// which the run's next signal offers the API ([`SessionFile::signal`]).
    /// It becomes `project_id` once the API took it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offer_project: Option<String>,
    /// The API refused the project offered for the run: it is not offered
    /// again.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub project_declined: bool,
}

/// What the hooks keep about one host session.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct SessionFile {
    /// The folder the session works in (where its credentials are found).
    #[serde(default)]
    pub cwd: String,
    /// The server's session part, as `init` or a `progress` answer said it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    /// The agent the server reports this session as.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<Value>,
    /// The session's own process, when it could be found.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_start: Option<u64>,
    #[serde(default)]
    pub last_hook_ms: u64,
    /// When a tool call started that has not reported back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_flight_ms: Option<u64>,
    /// The API refused a report: nothing more is sent for this session.
    #[serde(default)]
    pub refused: bool,
    /// The session ended; the keep-alive process leaves once the queue is sent.
    #[serde(default)]
    pub closed: bool,
    /// Subagents (`agent_id`) at work now.
    #[serde(default)]
    pub subagents: Vec<String>,
    #[serde(default)]
    pub runs: Vec<Run>,
    /// Reports waiting to be sent, oldest first.
    #[serde(default)]
    pub pending: Vec<Value>,
    #[serde(default)]
    pub spawned_ms: u64,
    /// When `init` first named the session under its current part.
    #[serde(default)]
    pub identity_ms: u64,
    /// When the person's last prompt arrived.
    #[serde(default)]
    pub prompt_ms: u64,
    /// The workspace and project `init` resolved for the session, and the
    /// project's name as the server gave it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_name: Option<String>,
    /// The session part whose work is spoken for: the model reported a run,
    /// a ticket was taken up, or the hooks opened the session's own run. The
    /// hooks open no run of their own for it (again).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spoken_for: Option<String>,
}

/// Who made a tool call: the session's main agent, or one of its subagents.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Actor<'a> {
    pub agent_id: Option<&'a str>,
    pub agent_type: Option<&'a str>,
}

impl<'a> Actor<'a> {
    pub(crate) fn of(input: &'a Value) -> Self {
        let field = |key: &str| {
            input
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
        };
        Self {
            agent_id: field("agent_id"),
            agent_type: field("agent_type").or_else(|| field("subagent_type")),
        }
    }
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn uuid_text(value: &Value, key: &str) -> Option<String> {
    text(value, key).filter(|id| uuid::Uuid::parse_str(id).is_ok())
}

fn ends_a_run(body: &Value) -> bool {
    body.get("status")
        .and_then(Value::as_str)
        .is_some_and(|status| TERMINAL.contains(&status))
}

/// A subagent's type as it may be shown: a short plain name, or nothing.
fn shown_agent_type(raw: &str) -> Option<String> {
    let name: String = raw
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, ' ' | '-' | '_' | '.'))
        .take(40)
        .collect();
    let name = name.trim();
    (!name.is_empty()).then(|| name.to_string())
}

/// The agent a subagent's own run is reported as: the session's agent, a
/// short digest of the subagent's id, and the subagent's type.
fn subagent_agent(session_agent: &Value, actor: &Actor) -> Option<(String, Value)> {
    let agent_id = actor.agent_id?;
    let tag = sha256_hex(agent_id.as_bytes())[..8].to_string();
    let name = text(session_agent, "name")?;
    let id = text(session_agent, "id").unwrap_or_else(|| name.clone());
    let kind = actor
        .agent_type
        .and_then(shown_agent_type)
        .unwrap_or_else(|| "subagent".to_string());
    let mut agent = Map::new();
    agent.insert("id".into(), json!(format!("{id}/{tag}")));
    agent.insert("name".into(), json!(format!("{name} \u{b7} {kind}")));
    agent.insert("runtime".into(), json!("mcp"));
    if let Some(client) = text(session_agent, "client") {
        agent.insert("client".into(), json!(client));
    }
    Some((tag, Value::Object(agent)))
}

/// The workspace and project a run falls back to when the tool's answer
/// names none.
#[derive(Debug, Clone, Default)]
pub(crate) struct Scope {
    pub workspace_id: Option<String>,
    pub project_id: Option<String>,
}

impl SessionFile {
    /// What groups a session's runs with its subagents' on a floor: the
    /// session agent's id.
    fn group(&self) -> Option<String> {
        let agent = self.agent.as_ref()?;
        text(agent, "id").or_else(|| text(agent, "name"))
    }

    /// Who and which run: the body of a heartbeat, and the start of every
    /// other report.
    pub(crate) fn base(&self, run: &Run) -> Option<Map<String, Value>> {
        let agent = run.agent.clone().or_else(|| self.agent.clone())?;
        let mut body = Map::new();
        body.insert("workspace_id".into(), json!(run.workspace_id));
        if let Some(project_id) = &run.project_id {
            body.insert("project_id".into(), json!(project_id));
        }
        match (&run.run_id, &run.key) {
            (Some(run_id), _) => body.insert("run_id".into(), json!(run_id)),
            (None, Some(key)) => body.insert("source_run_id".into(), json!(key)),
            (None, None) => return None,
        };
        body.insert("agent".into(), agent);
        body.insert("heartbeat_s".into(), json!(beat_secs()));
        if let Some(group) = self.group() {
            body.insert("group_id".into(), json!(group));
        }
        Some(body)
    }

    /// The signal that keeps `run` alive, and whether it offers the API a
    /// project for it: a heartbeat, with the session's project for a run
    /// that was reported without one. Only this signal carries an offer, so
    /// a refusal of it is about the project and nothing else.
    pub(crate) fn signal(&self, run: &Run) -> Option<(Map<String, Value>, bool)> {
        let mut body = self.base(run)?;
        let offer = run
            .offer_project
            .as_ref()
            .filter(|_| run.project_id.is_none());
        if let Some(project_id) = offer {
            body.insert("project_id".into(), json!(project_id));
        }
        Some((body, offer.is_some()))
    }

    /// The API answered the signal that offered a project for the run
    /// `ident`. Taken, the project is the run's from now on; refused (a
    /// project this key may not write), the run stays as it was reported and
    /// the project is not offered again.
    pub(crate) fn offer_answered(&mut self, ident: &str, taken: bool) {
        for run in self.runs.iter_mut().filter(|run| run_ident(run) == ident) {
            let offered = run.offer_project.take();
            if taken {
                if run.project_id.is_none() {
                    run.project_id = offered;
                }
            } else {
                run.project_declined = true;
            }
        }
    }

    fn queue(&mut self, body: Map<String, Value>) {
        self.pending.push(Value::Object(body));
        if self.pending.len() > MAX_PENDING {
            // The last word on a run is kept; an older step gives way.
            let dropped = self
                .pending
                .iter()
                .position(|body| !ends_a_run(body))
                .unwrap_or(0);
            self.pending.remove(dropped);
        }
    }

    fn end(&mut self, index: usize, sentence: &str) {
        let run = self.runs.remove(index);
        if let Some(mut body) = self.base(&run) {
            body.insert("status".into(), json!("stopped"));
            body.insert("summary".into(), json!(sentence));
            body.insert("waiting".into(), Value::Null);
            self.queue(body);
        }
    }

    fn end_where(&mut self, sentence: &str, which: impl Fn(&Run) -> bool) {
        let mut index = 0;
        while index < self.runs.len() {
            if which(&self.runs[index]) {
                self.end(index, sentence);
            } else {
                index += 1;
            }
        }
    }

    /// End every open run with a fixed sentence.
    pub(crate) fn end_all(&mut self, sentence: &str) {
        self.end_where(sentence, |_| true);
    }

    fn refuse(&mut self) {
        self.refused = true;
        self.runs.clear();
        self.pending.clear();
    }

    /// The session is at work again: runs the hooks set waiting say working.
    fn resume(&mut self) {
        for index in 0..self.runs.len() {
            if self.runs[index].owner.is_some() || !self.runs[index].hook_waiting {
                continue;
            }
            self.runs[index].hook_waiting = false;
            if let Some(mut body) = self.base(&self.runs[index]) {
                body.insert("status".into(), json!("working"));
                body.insert("waiting".into(), Value::Null);
                self.queue(body);
            }
        }
    }

    /// A tool call starts.
    pub(crate) fn tool_started(&mut self, actor: &Actor, tool: &str, now: u64) {
        self.last_hook_ms = now;
        self.in_flight_ms = Some(now);
        self.closed = false;
        if actor.agent_id.is_none() {
            self.resume();
        }
        // ContextStream's own calls are bookkeeping, not the work.
        if is_contextstream_tool(tool) {
            return;
        }
        let (kind, label) = step_for_tool(tool);
        let own: Vec<usize> = (0..self.runs.len())
            .filter(|index| self.runs[*index].owner.as_deref() == actor.agent_id)
            .collect();
        // A subagent without a run of its own works for its session's runs.
        let targets = if own.is_empty() && actor.agent_id.is_some() {
            (0..self.runs.len())
                .filter(|index| self.runs[*index].owner.is_none())
                .collect()
        } else {
            own
        };
        for index in targets {
            let run = &self.runs[index];
            if run.model_waiting || run.hook_waiting {
                continue;
            }
            let model_spoke = run.model_step_ms.max(run.started_ms);
            // On a run the model has said nothing on, no step of its own
            // stands to be written over.
            if (!run.unreported && now.saturating_sub(model_spoke) < COARSE_STEP_AFTER_MS)
                || run.coarse_kind.as_deref() == Some(kind)
                || now.saturating_sub(run.coarse_ms) < COARSE_STEP_EVERY_MS
            {
                continue;
            }
            let Some(mut body) = self.base(run) else {
                continue;
            };
            body.insert(
                "step".into(),
                json!({ "label": label, "kind": kind, "status": "started" }),
            );
            self.runs[index].coarse_kind = Some(kind.to_string());
            self.runs[index].coarse_ms = now;
            self.queue(body);
        }
    }

    /// A hook event that is evidence of work and says nothing else (signs of
    /// life only): a tool call started, or a prompt arrived.
    pub(crate) fn hook_event(&mut self, now: u64) {
        self.last_hook_ms = now;
        self.closed = false;
    }

    /// Signs of life only: the session part `ended` is gone (the client
    /// reconnected under a new one), and with it the session's default runs
    /// under it, whose keys start with it: the model's reports can no longer
    /// land on them. They get no more signs and fall silent; nothing is sent.
    pub(crate) fn forget_part(&mut self, ended: &str) {
        let plain = format!("{ended}:");
        let sub = format!("{ended}/");
        self.runs.retain(|run| {
            !run.key
                .as_deref()
                .is_some_and(|key| key.starts_with(&plain) || key.starts_with(&sub))
        });
    }

    /// Signs of life only: the subagent `agent_id` stopped. The runs it
    /// reported last get no more signs and fall silent; nothing is sent.
    pub(crate) fn forget_subagent(&mut self, agent_id: &str) {
        self.subagents.retain(|known| known != agent_id);
        self.runs
            .retain(|run| run.reported_by.as_deref() != Some(agent_id));
    }

    /// The signs of life due at `now` (signs only), as (run, body): one for
    /// each run this session reported that has an id and does not wait, when
    /// a hook event came after both its last report and its last sign, at
    /// least `every_ms` after the later of the two, and within
    /// [`SIGNS_FOR_MS`] of the report. A body says who and which run, under
    /// the agent the run was reported as, and nothing else: no heartbeat_s,
    /// no group, no scope.
    pub(crate) fn signs_due(&self, every_ms: u64, now: u64) -> Vec<(String, Value)> {
        let mut due = Vec::new();
        for run in &self.runs {
            let Some(run_id) = run.run_id.as_deref() else {
                continue;
            };
            if run.model_waiting || run.hook_waiting || run.reported_ms == 0 {
                continue;
            }
            let last = run.reported_ms.max(run.signed_ms);
            if self.last_hook_ms <= last
                || now.saturating_sub(last) < every_ms
                || now.saturating_sub(run.reported_ms) >= SIGNS_FOR_MS
            {
                continue;
            }
            let Some(agent) = run
                .reported_agent
                .clone()
                .or_else(|| run.agent.clone())
                .or_else(|| self.agent.clone())
            else {
                continue;
            };
            due.push((
                run_ident(run),
                json!({"workspace_id": run.workspace_id, "run_id": run_id, "agent": agent}),
            ));
        }
        due
    }

    /// A tool call reported back.
    pub(crate) fn tool_ended(&mut self, now: u64) {
        self.last_hook_ms = now;
        self.in_flight_ms = None;
    }

    /// The person sent a prompt.
    pub(crate) fn prompt_submitted(&mut self, now: u64) {
        self.last_hook_ms = now;
        self.closed = false;
        self.prompt_ms = now;
        self.resume();
    }

    /// The main agent's turn ended: its runs wait on a person, unless a
    /// subagent is still at work or the model said what the run waits on.
    pub(crate) fn turn_ended(&mut self, now: u64) {
        self.last_hook_ms = now;
        self.in_flight_ms = None;
        if !self.subagents.is_empty() {
            return;
        }
        for index in 0..self.runs.len() {
            let run = &self.runs[index];
            if run.owner.is_some() || run.hook_waiting || run.model_waiting {
                continue;
            }
            self.runs[index].hook_waiting = true;
            if let Some(mut body) = self.base(&self.runs[index]) {
                body.insert("status".into(), json!("waiting_input"));
                body.insert("waiting".into(), json!({ "on": "person" }));
                self.queue(body);
            }
        }
    }

    pub(crate) fn subagent_started(&mut self, agent_id: &str, now: u64) {
        self.last_hook_ms = now;
        if !self.subagents.iter().any(|known| known == agent_id) {
            self.subagents.push(agent_id.to_string());
        }
    }

    /// A subagent ended: its own runs end with it.
    pub(crate) fn subagent_stopped(&mut self, agent_id: &str, now: u64) {
        self.last_hook_ms = now;
        self.subagents.retain(|known| known != agent_id);
        self.end_where(AGENT_ENDED, |run| run.owner.as_deref() == Some(agent_id));
    }

    /// `init` answered: who the session is, where it works (the workspace and
    /// project the server resolved, and the project's name), and when. A
    /// session part that is new starts its own span before the session's run.
    pub(crate) fn named(
        &mut self,
        session: &str,
        agent: &Value,
        scope: &Scope,
        project_name: Option<&str>,
        now: u64,
    ) {
        let part = session.trim();
        let new_part = !part.is_empty() && self.session.as_deref() != Some(part);
        self.learn_identity(Some(session), Some(agent));
        if new_part {
            self.identity_ms = now;
        }
        if let Some(workspace_id) = &scope.workspace_id {
            self.workspace_id = Some(workspace_id.clone());
            self.project_id = scope.project_id.clone();
            self.project_name = project_name
                .map(|name| name.split_whitespace().collect::<Vec<_>>().join(" "))
                .filter(|name| !name.is_empty())
                .map(|name| common::truncate_with_ellipsis(&name, MAX_PROJECT_NAME));
        }
    }

    /// The project the session works in, within `workspace_id`: the one
    /// `init` resolved, else the folder's (`fallback`). A project belongs to
    /// one workspace, so one resolved for another workspace is nobody's here.
    fn project_in(&self, workspace_id: &str, fallback: &Scope) -> Option<String> {
        let within = |workspace: &Option<String>, project: &Option<String>| {
            project
                .clone()
                .filter(|_| workspace.as_deref() == Some(workspace_id))
        };
        within(&self.workspace_id, &self.project_id)
            .or_else(|| within(&fallback.workspace_id, &fallback.project_id))
    }

    /// Whether the hooks may still open this session's own run: the server
    /// said who the session is, and nothing has spoken for its work under
    /// this part (see `spoken_for`).
    pub(crate) fn awaits_session_run(&self) -> bool {
        !self.refused
            && self.agent.is_some()
            && self
                .session
                .as_deref()
                .is_some_and(|part| self.spoken_for.as_deref() != Some(part))
    }

    /// Whether the session's own run is due at `now`: it may still be opened,
    /// the session has no run of its own, and `after_ms` have passed since
    /// its turn began (the later of `init`'s answer and the last prompt).
    pub(crate) fn session_run_due(&self, after_ms: u64, now: u64) -> bool {
        self.awaits_session_run()
            && self.runs.len() < MAX_RUNS
            && !self.runs.iter().any(|run| run.owner.is_none())
            && now.saturating_sub(self.identity_ms.max(self.prompt_ms)) >= after_ms
    }

    /// Open the session's own run when it is due: a session at work whose
    /// model reported nothing is shown with its agent and where it works.
    /// The workspace and project are the ones `init` resolved, else the
    /// folder's (`fallback`); where `init` resolved a workspace and no
    /// project, the folder's project in that same workspace. With no
    /// workspace nothing is opened, and nothing is decided. The key is the
    /// one a `progress` report that names no run goes to, so the model's
    /// later report lands on this run.
    pub(crate) fn open_session_run(&mut self, fallback: &Scope, after_ms: u64, now: u64) -> bool {
        if !self.session_run_due(after_ms, now) {
            return false;
        }
        let Some(session) = self.session.clone() else {
            return false;
        };
        let (workspace_id, project_id) = match &self.workspace_id {
            Some(workspace_id) => (
                workspace_id.clone(),
                self.project_in(workspace_id, fallback),
            ),
            None => match &fallback.workspace_id {
                Some(workspace_id) => (workspace_id.clone(), fallback.project_id.clone()),
                None => return false,
            },
        };
        let run = Run {
            key: Some(mcp_tools::domains::progress::default_run_key(
                &session, None,
            )),
            workspace_id,
            project_id,
            started_ms: now,
            unreported: true,
            ..Run::default()
        };
        let Some(mut body) = self.base(&run) else {
            return false;
        };
        // Fixed words and the project's own name: nothing of the work.
        let title = match &self.project_name {
            Some(name) => format!("{SESSION_RUN_TITLE} in {name}"),
            None => SESSION_RUN_TITLE.to_string(),
        };
        body.insert("title".into(), json!(title));
        body.insert("status".into(), json!("working"));
        self.runs.push(run);
        self.queue(body);
        self.spoken_for = Some(session);
        true
    }

    /// The session's work is reported as another run: the run the hooks
    /// opened for it, which the model never reported on, ends.
    fn end_unreported(&mut self) {
        self.end_where(MOVED_ON, |run| run.unreported && run.owner.is_none());
    }

    /// The server said who this session is. A changed session part (the
    /// client reconnected) ends the runs the hooks started under the old one:
    /// the tool's next report can no longer land on them.
    pub(crate) fn learn_identity(&mut self, session: Option<&str>, agent: Option<&Value>) {
        if let Some(session) = session.map(str::trim).filter(|part| !part.is_empty()) {
            if self
                .session
                .as_deref()
                .is_some_and(|known| known != session)
            {
                let prefix_plain = format!("{session}:");
                let prefix_sub = format!("{session}/");
                self.end_where(RECONNECTED, |run| {
                    run.key.as_deref().is_some_and(|key| {
                        !key.starts_with(&prefix_plain) && !key.starts_with(&prefix_sub)
                    }) && run.run_id.is_none()
                });
            }
            self.session = Some(session.to_string());
        }
        if let Some(agent) = agent.filter(|agent| text(agent, "name").is_some()) {
            self.agent = Some(agent.clone());
        }
    }

    /// The model called `progress`: the run it reported is this session's.
    pub(crate) fn model_reported(
        &mut self,
        input: &Value,
        answer: &Value,
        scope: &Scope,
        now: u64,
    ) -> Option<usize> {
        self.last_hook_ms = now;
        self.closed = false;
        if answer.get("recorded").and_then(Value::as_bool) == Some(false) {
            let reason = text(answer, "reason").unwrap_or_default();
            if matches!(reason.as_str(), "off" | "not_available" | "not_allowed") {
                self.refuse();
            }
            return None;
        }
        let view = answer.get("run").filter(|view| view.is_object())?;
        self.learn_identity(
            answer.get("session").and_then(Value::as_str),
            view.get("agent"),
        );
        // The model has spoken for this session's work: the hooks open no
        // run of their own for it.
        if self.session.is_some() {
            self.spoken_for = self.session.clone();
        }
        let run_id = uuid_text(view, "run_id");
        let key = text(answer, "run_key");
        let found = self
            .runs
            .iter()
            .position(|run| {
                (key.is_some() && run.key == key) || (run_id.is_some() && run.run_id == run_id)
            })
            .or_else(|| self.sole_own_run(input, view, run_id.as_deref()));
        let found_by_key = found.is_some_and(|index| key.is_some() && self.runs[index].key == key);
        let status = text(input, "status").unwrap_or_default();
        let ended = TERMINAL.contains(&status.as_str())
            || matches!(
                text(view, "state").as_deref(),
                Some("stopped" | "finished" | "failed")
            )
            || text(view, "liveness").as_deref() == Some("ended");
        if ended {
            if let Some(index) = found {
                self.runs.remove(index);
            }
            return None;
        }
        let index = match found {
            Some(index) => index,
            None => {
                let workspace_id =
                    uuid_text(view, "workspace_id").or_else(|| scope.workspace_id.clone())?;
                if self.runs.len() >= MAX_RUNS || (run_id.is_none() && key.is_none()) {
                    return None;
                }
                // The work is this run now, not the one the hooks opened.
                self.end_unreported();
                self.runs.push(Run {
                    workspace_id,
                    project_id: uuid_text(view, "project_id"),
                    started_ms: now,
                    ..Run::default()
                });
                self.runs.len() - 1
            }
        };
        // A run the server had no project for is shown in the session's: its
        // next signal offers the API that project, and the API takes a run's
        // project from a signal's scope. A project the report named stands.
        if let Some(named) = uuid_text(view, "project_id") {
            self.runs[index].project_id = Some(named);
            self.runs[index].offer_project = None;
        } else if self.runs[index].project_id.is_none() && !self.runs[index].project_declined {
            let workspace_id = self.runs[index].workspace_id.clone();
            self.runs[index].offer_project = self.project_in(&workspace_id, scope);
        }
        let run = &mut self.runs[index];
        // The model reported on it: the run is the model's.
        run.unreported = false;
        if run_id.is_some() {
            run.run_id = run_id;
        }
        // A run found by its id keeps the key it is known by: a report that
        // named no run is sent on to the session's one open run, and a local
        // server's words do not say under which key.
        if key.is_some() && (run.key.is_none() || found_by_key) {
            run.key = key;
        }
        let ticket = view
            .get("scope")
            .and_then(|scope| uuid_text(scope, "ticket_id"))
            .or_else(|| uuid_text(input, "ticket_id"));
        if ticket.is_some() {
            run.ticket_id = ticket;
        }
        let gave_step = input.get("step").is_some_and(|step| !step.is_null());
        if gave_step {
            run.model_step_ms = now;
            run.coarse_kind = None;
        }
        let asks_to_wait = input
            .get("waiting")
            .is_some_and(|waiting| waiting.is_object() || waiting.as_bool() == Some(true))
            || matches!(status.as_str(), "waiting_input" | "waiting_approval");
        if asks_to_wait {
            run.model_waiting = true;
        } else if gave_step || status == "working" || input.get("waiting").is_some() {
            run.model_waiting = false;
        }
        // The model's word is the run's state now.
        run.hook_waiting = false;
        run.reported_ms = now;
        if let Some(agent) = view.get("agent").filter(|agent| agent.is_object()) {
            run.reported_agent = Some(agent.clone());
        }
        Some(index)
    }

    /// A report that named no run went to the session's one open run. The
    /// hosted gateway sends it there by the run's id and cannot know the key
    /// the hooks opened it under (a ticket's take-up), so its answer names no
    /// key: when the hooks keep exactly one run of the session's own, on the
    /// same ticket and with no other id, that is the run.
    fn sole_own_run(&self, input: &Value, view: &Value, run_id: Option<&str>) -> Option<usize> {
        let named = ["run_key", "ticket_id", "run_id"].iter().any(|field| {
            input
                .get(*field)
                .and_then(Value::as_str)
                .is_some_and(|value| !value.trim().is_empty())
        });
        if named {
            return None;
        }
        let ticket = view
            .get("scope")
            .and_then(|scope| uuid_text(scope, "ticket_id"));
        let mut own = self
            .runs
            .iter()
            .enumerate()
            .filter(|(_, run)| run.owner.is_none());
        let (index, run) = own.next()?;
        if own.next().is_some() || run.ticket_id != ticket {
            return None;
        }
        if run.run_id.is_some() && run.run_id.as_deref() != run_id {
            return None;
        }
        Some(index)
    }

    /// The model called `progress` and the answer came in words (a local
    /// server): the run is the one the call's own arguments name, under the
    /// session the server said this is.
    pub(crate) fn model_reported_in_text(
        &mut self,
        input: &Value,
        run_id: Option<&str>,
        state: Option<&str>,
        scope: &Scope,
        now: u64,
    ) -> Option<usize> {
        let (Some(session), Some(agent)) = (self.session.clone(), self.agent.clone()) else {
            return None;
        };
        let ticket = uuid_text(input, "ticket_id");
        let key = mcp_tools::domains::progress::run_key_for(
            input.get("run_key").and_then(Value::as_str),
            &session,
            ticket
                .as_deref()
                .and_then(|id| uuid::Uuid::parse_str(id).ok()),
        );
        let answer = json!({
            "run": {
                "run_id": run_id,
                "workspace_id": uuid_text(input, "workspace_id").or_else(|| scope.workspace_id.clone()),
                "project_id": uuid_text(input, "project_id").or_else(|| scope.project_id.clone()),
                "state": state.unwrap_or("working"),
                "agent": agent,
                "scope": { "ticket_id": ticket },
            },
            "session": session,
            "run_key": key,
        });
        self.model_reported(input, &answer, scope, now)
    }

    /// A tool call changed a ticket's status. Taking a ticket up starts a run
    /// for it; moving it on ends the session's runs on it.
    pub(crate) fn ticket_changed(
        &mut self,
        actor: &Actor,
        ticket: &Value,
        scope: &Scope,
        now: u64,
    ) {
        self.last_hook_ms = now;
        self.closed = false;
        let Some(ticket_id) = uuid_text(ticket, "id") else {
            return;
        };
        let status = text(ticket, "status")
            .unwrap_or_default()
            .to_ascii_lowercase();
        if status != "in_progress" {
            let word: String = status
                .chars()
                .filter(|c| c.is_ascii_lowercase() || *c == '_')
                .take(24)
                .collect();
            let sentence = if word.is_empty() {
                "The session moved the ticket on.".to_string()
            } else {
                format!("The session moved the ticket to {word}.")
            };
            self.end_where(&sentence, |run| {
                run.ticket_id.as_deref() == Some(ticket_id.as_str())
            });
            return;
        }
        if self.refused || self.runs.len() >= MAX_RUNS {
            return;
        }
        let (Some(session), Some(session_agent)) = (self.session.clone(), self.agent.clone())
        else {
            // The server has not said who this session is: a run started here
            // could not be the one the model's own report lands on.
            return;
        };
        let taken = self.runs.iter().any(|run| {
            run.ticket_id.as_deref() == Some(ticket_id.as_str())
                && (run.owner.is_none() || run.owner.as_deref() == actor.agent_id)
        });
        if taken {
            return;
        }
        let Some(workspace_id) =
            uuid_text(ticket, "workspace_id").or_else(|| scope.workspace_id.clone())
        else {
            return;
        };
        let (key, agent) = match subagent_agent(&session_agent, actor) {
            Some((tag, agent)) => (format!("{session}/{tag}:{ticket_id}"), Some(agent)),
            None => (format!("{session}:{ticket_id}"), None),
        };
        let run = Run {
            key: Some(key),
            workspace_id,
            project_id: uuid_text(ticket, "project_id").or_else(|| scope.project_id.clone()),
            ticket_id: Some(ticket_id.clone()),
            owner: actor.agent_id.map(str::to_string),
            agent,
            started_ms: now,
            ..Run::default()
        };
        let Some(mut body) = self.base(&run) else {
            return;
        };
        body.insert("ticket_id".into(), json!(ticket_id));
        // The ticket's own title names the run; nothing else is said about it.
        if let Some(title) = text(ticket, "title") {
            body.insert(
                "title".into(),
                json!(common::truncate_with_ellipsis(&title, 200)),
            );
        }
        body.insert("status".into(), json!("working"));
        if run.owner.is_none() {
            // The ticket's run is the session's work now.
            self.end_unreported();
            self.spoken_for = Some(session);
        }
        self.runs.push(run);
        self.queue(body);
    }
}

// ============================================================================
// The state file
// ============================================================================

fn state_path(host_session: &str) -> Option<PathBuf> {
    let digest = sha256_hex(host_session.as_bytes());
    Some(
        state_dir()?
            .join("run-progress")
            .join(format!("{}.json", &digest[..32])),
    )
}

fn read(path: &Path) -> Option<SessionFile> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn update<T>(path: &Path, change: impl FnOnce(&mut SessionFile) -> T) -> Option<T> {
    with_locked_json(path, change)
}

/// Drop state files nothing has touched for a week.
fn prune(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age > STALE_STATE);
        if old {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// The session's state file, when the hooks report on this machine and the
/// host named its session.
fn session_state(input: &Value) -> Option<(String, PathBuf)> {
    let settings = Settings::from_env();
    if !settings.enabled && !settings.signs {
        return None;
    }
    let host = common::host_session_key(input)?;
    let path = state_path(&host)?;
    Some((host, path))
}

/// The same, only for a session the hooks already keep state for.
fn known_session(input: &Value) -> Option<PathBuf> {
    let (_, path) = session_state(input)?;
    path.exists().then_some(path)
}

/// Where the session's own run would open, when it is due at `now`: the
/// folder's workspace and project, read only for a session `init` resolved
/// no workspace or no project for. `None` when the run is not due, or there
/// is no workspace to open it in: nothing is written for such a session on
/// the tool path.
fn session_run_place(
    known: &SessionFile,
    host: &str,
    cwd: &str,
    after_ms: u64,
    now: u64,
) -> Option<Scope> {
    if !known.session_run_due(after_ms, now) {
        return None;
    }
    if known.workspace_id.is_some() && known.project_id.is_some() {
        return Some(Scope::default());
    }
    Some(default_scope(host, cwd))
        .filter(|scope| known.workspace_id.is_some() || scope.workspace_id.is_some())
}

fn default_scope(host: &str, cwd: &str) -> Scope {
    if let Some((workspace_id, project_id)) = super::durable_capture::session_primary_scope(host) {
        return Scope {
            workspace_id: Some(workspace_id.to_string()),
            project_id: Some(project_id.to_string()),
        };
    }
    let config = common::load_config(cwd);
    Scope {
        workspace_id: config.workspace_id,
        project_id: config.project_id,
    }
}

// ============================================================================
// The session's own process
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Host {
    Alive,
    Gone,
    /// The process could not be found (Windows, or an unusual launcher).
    Unknown,
}

fn host_state(file: &SessionFile) -> Host {
    match file.host_pid {
        None => Host::Unknown,
        Some(pid) if process_alive(pid, file.host_start) => Host::Alive,
        Some(_) => Host::Gone,
    }
}

/// Find the session's process again when it is unknown or gone (a resumed
/// session runs in a new one).
fn refresh_host(file: &mut SessionFile) {
    if host_state(file) == Host::Alive {
        return;
    }
    match host_process() {
        Some((pid, start)) => {
            file.host_pid = Some(pid);
            file.host_start = start;
        }
        None => {
            file.host_pid = None;
            file.host_start = None;
        }
    }
}

fn host_process() -> Option<(u32, Option<u64>)> {
    if let Some(pid) = std::env::var(HOST_PID_ENV)
        .ok()
        .and_then(|raw| raw.trim().parse::<u32>().ok())
        .filter(|pid| *pid > 1)
    {
        return Some((pid, process_start(pid)));
    }
    discover_host()
}

fn is_launcher(name: &str) -> bool {
    matches!(
        name.trim_start_matches('-'),
        "sh" | "bash" | "zsh" | "dash" | "fish" | "ksh" | "csh" | "tcsh" | "ash" | "env"
    )
}

#[cfg(target_os = "linux")]
struct ProcStat {
    name: String,
    state: char,
    parent: u32,
    start: u64,
}

#[cfg(target_os = "linux")]
fn proc_stat(pid: u32) -> Option<ProcStat> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `pid (name) state ppid ...`: the name may hold spaces and brackets.
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let name = stat.get(open + 1..close)?.to_string();
    let rest: Vec<&str> = stat.get(close + 1..)?.split_whitespace().collect();
    Some(ProcStat {
        name,
        state: rest.first()?.chars().next()?,
        parent: rest.get(1)?.parse().ok()?,
        // Field 22 of the line, the 20th after the name.
        start: rest.get(19)?.parse().ok()?,
    })
}

/// The nearest ancestor that is not a shell and started a few seconds before
/// this hook did: the host starts a shell (or nothing) for each hook, and
/// lives much longer. Both start times are read from `/proc` in the same
/// clock, so a container whose uptime is not the machine's changes nothing.
#[cfg(target_os = "linux")]
fn discover_host() -> Option<(u32, Option<u64>)> {
    let own_start = proc_stat(std::process::id()).map(|stat| stat.start);
    let mut pid = std::os::unix::process::parent_id();
    for _ in 0..8 {
        if pid <= 1 {
            return None;
        }
        let stat = proc_stat(pid)?;
        // Clock ticks are 100 a second on every Linux this ships for.
        let lived = own_start.is_none_or(|own| own.saturating_sub(stat.start) >= 300);
        if !is_launcher(&stat.name) && lived {
            return Some((pid, Some(stat.start)));
        }
        pid = stat.parent;
    }
    None
}

#[cfg(target_os = "macos")]
fn discover_host() -> Option<(u32, Option<u64>)> {
    let mut pid = std::os::unix::process::parent_id();
    for _ in 0..8 {
        if pid <= 1 {
            return None;
        }
        let output = std::process::Command::new("ps")
            .args(["-o", "ppid=,comm=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        let line = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let (parent, command) = line.split_once(char::is_whitespace)?;
        let parent: u32 = parent.trim().parse().ok()?;
        let name = command.trim().rsplit('/').next().unwrap_or_default();
        if !is_launcher(name) {
            return Some((pid, None));
        }
        pid = parent;
    }
    None
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn discover_host() -> Option<(u32, Option<u64>)> {
    None
}

#[cfg(target_os = "linux")]
fn process_start(pid: u32) -> Option<u64> {
    proc_stat(pid).map(|stat| stat.start)
}

#[cfg(not(target_os = "linux"))]
fn process_start(_pid: u32) -> Option<u64> {
    None
}

/// Whether the process still runs (and is the same one, where its start time
/// is known).
#[cfg(target_os = "linux")]
fn process_alive(pid: u32, start: Option<u64>) -> bool {
    proc_stat(pid)
        .is_some_and(|stat| stat.state != 'Z' && start.is_none_or(|known| known == stat.start))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn process_alive(pid: u32, _start: Option<u64>) -> bool {
    // Signal 0 checks that the process exists and sends nothing.
    let answer = unsafe { libc::kill(pid as libc::pid_t, 0) };
    answer == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn process_alive(_pid: u32, _start: Option<u64>) -> bool {
    true
}

// ============================================================================
// What the hooks call
// ============================================================================

/// `contextstream-mcp hook run-progress`: what one hook event means for the
/// session's live runs, and nothing else of what ContextStream's hooks do.
///
/// Codex runs a hook only after a person approved it, and setup installs one
/// hook for it (the mid-run notices, before every tool call). That hook
/// notes a tool call's start; this one is the rest of what live progress
/// takes: the answers of `init` and `progress` and a tool call's end
/// (PostToolUse), and the turn's end (Stop). The event is the one the input
/// names, so one command serves every event it is registered for.
///
/// It prints nothing, which every host takes for "nothing to say" (Codex
/// refuses any field an event does not list), and never touches the network.
pub async fn handle_event_hook() -> anyhow::Result<()> {
    let Ok(input) = super::read_stdin_json() else {
        return Ok(());
    };
    let cwd = common::extract_cwd(&input);
    let event = text(&input, "hook_event_name").unwrap_or_default();
    match event.as_str() {
        "PostToolUse" => super::post_tool_use::note_live_progress(&input, &cwd),
        "PreToolUse" => tool_started(&input, &cwd, &text(&input, "tool_name").unwrap_or_default()),
        "UserPromptSubmit" => prompt_submitted(&input, &cwd),
        "Stop" => turn_ended(&input, &cwd),
        _ => {}
    }
    Ok(())
}

/// PreToolUse: a tool call starts. Never touches the network.
pub(crate) fn tool_started(input: &Value, cwd: &str, tool: &str) {
    let Some((host, path)) = session_state(input) else {
        return;
    };
    let Some(known) = read(&path).filter(|file| !file.refused) else {
        return;
    };
    let settings = Settings::from_env();
    let now = now_ms();
    let after_ms = session_run_after_ms();
    // A tool call of the session's own work, and no run to show it on.
    let fallback = (settings.enabled && !is_contextstream_tool(tool))
        .then(|| session_run_place(&known, &host, cwd, after_ms, now))
        .flatten();
    // Nothing open: nothing to note, and no write on the tool path.
    if known.runs.is_empty() && fallback.is_none() {
        return;
    }
    let actor = Actor::of(input);
    let signs_only = settings.signs_only();
    update(&path, |file: &mut SessionFile| {
        refresh_host(file);
        if signs_only {
            file.hook_event(now);
        } else {
            if let Some(fallback) = &fallback {
                file.open_session_run(fallback, after_ms, now);
            }
            file.tool_started(&actor, tool, now);
        }
    });
    ensure_beater(&path, cwd);
}

/// What a hook reads of a tool call's answer: whether the call succeeded, its
/// structured content where the transport carries one (the hosted gateway
/// does), and its text (all a local server sends).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct Answer<'a> {
    pub succeeded: bool,
    pub json: Option<&'a Value>,
    pub text: Option<&'a str>,
}

/// PostToolUse: a tool call reported back. `contextstream_tool` is the
/// tool's plain name when ContextStream's own server was called.
pub(crate) fn tool_finished(
    input: &Value,
    cwd: &str,
    contextstream_tool: Option<&str>,
    tool_input: &Value,
    answer: Answer,
) {
    let Some((host, path)) = session_state(input) else {
        return;
    };
    let finding = contextstream_tool.and_then(|tool| classify(tool, tool_input, &answer));
    // Signs of life learn the runs the model reported, and who the session
    // is: a local server's report says which run only together with the
    // session `init` named, and a session part that changed (the client
    // reconnected) takes its default runs with it. A ticket's take-up starts
    // nothing in this mode.
    let signs_only = Settings::from_env().signs_only();
    let finding = finding.filter(|finding| !signs_only || !matches!(finding, Finding::Ticket(_)));
    let now = now_ms();
    let Some(finding) = finding else {
        let Some(known) = read(&path).filter(|file| !file.refused) else {
            return;
        };
        // A long tool call of the session's own work came back, and there
        // is still no run to show it on.
        let after_ms = session_run_after_ms();
        let fallback = (Settings::from_env().enabled && contextstream_tool.is_none())
            .then(|| session_run_place(&known, &host, cwd, after_ms, now))
            .flatten();
        if !known.runs.is_empty() || fallback.is_some() {
            update(&path, |file: &mut SessionFile| {
                if let Some(fallback) = &fallback {
                    refresh_host(file);
                    file.open_session_run(fallback, after_ms, now);
                }
                file.tool_ended(now);
            });
            ensure_beater(&path, cwd);
        }
        return;
    };
    let fresh = !path.exists();
    let actor = Actor::of(input);
    update(&path, |file: &mut SessionFile| {
        file.tool_ended(now);
        if file.refused {
            return;
        }
        file.cwd = cwd.to_string();
        refresh_host(file);
        let part_before = file.session.clone();
        let reported = match &finding {
            Finding::Identity {
                session,
                agent,
                scope,
                project_name,
            } => {
                file.named(session, agent, scope, project_name.as_deref(), now);
                None
            }
            Finding::Report(answer) => {
                let scope = default_scope(&host, cwd);
                file.model_reported(tool_input, answer, &scope, now)
            }
            Finding::ReportInText { run_id, state } => {
                let scope = default_scope(&host, cwd);
                file.model_reported_in_text(
                    tool_input,
                    run_id.as_deref(),
                    state.as_deref(),
                    &scope,
                    now,
                )
            }
            Finding::Ticket(ticket) => {
                let scope = default_scope(&host, cwd);
                file.ticket_changed(&actor, ticket, &scope, now);
                None
            }
        };
        if let Some(run) = reported.and_then(|index| file.runs.get_mut(index)) {
            run.reported_by = actor.agent_id.map(str::to_string);
        }
        if signs_only {
            // The hooks report nothing of their own here.
            file.pending.clear();
            // A session part that ended takes its default runs with it.
            if let Some(ended) = part_before.filter(|before| file.session.as_ref() != Some(before))
            {
                file.forget_part(&ended);
            }
        }
    });
    if fresh {
        if let Some(dir) = path.parent() {
            prune(dir);
        }
    }
    ensure_beater(&path, cwd);
}

/// PostToolUseFailure: the tool call is over.
pub(crate) fn tool_failed(input: &Value) {
    let Some(path) = known_session(input) else {
        return;
    };
    if read(&path).is_some_and(|file| !file.runs.is_empty() && !file.refused) {
        let now = now_ms();
        update(&path, |file: &mut SessionFile| file.tool_ended(now));
    }
}

/// UserPromptSubmit.
pub(crate) fn prompt_submitted(input: &Value, cwd: &str) {
    let Some(path) = known_session(input) else {
        return;
    };
    let Some(known) = read(&path).filter(|file| !file.refused) else {
        return;
    };
    let signs_only = Settings::from_env().signs_only();
    // With no run open, a prompt is noted only for a session the hooks may
    // still open a run for: its turn, and the model's span to report, begin.
    if known.runs.is_empty() && (signs_only || !known.awaits_session_run()) {
        return;
    }
    let now = now_ms();
    update(&path, |file: &mut SessionFile| {
        refresh_host(file);
        if signs_only {
            file.hook_event(now);
        } else {
            file.prompt_submitted(now);
        }
    });
    ensure_beater(&path, cwd);
}

/// Stop: the main agent's turn ended. Signs of life say nothing of it.
pub(crate) fn turn_ended(input: &Value, cwd: &str) {
    if Settings::from_env().signs_only() {
        return;
    }
    let Some(path) = known_session(input) else {
        return;
    };
    if read(&path).is_none_or(|file| file.runs.is_empty() || file.refused) {
        return;
    }
    let now = now_ms();
    update(&path, |file: &mut SessionFile| file.turn_ended(now));
    ensure_beater(&path, cwd);
}

/// SubagentStart.
pub(crate) fn subagent_started(input: &Value) {
    if Settings::from_env().signs_only() {
        return;
    }
    let (Some(path), Some(agent_id)) = (known_session(input), Actor::of(input).agent_id) else {
        return;
    };
    let now = now_ms();
    update(&path, |file: &mut SessionFile| {
        file.subagent_started(agent_id, now)
    });
}

/// SubagentStop. With signs of life only, the runs the subagent reported
/// last fall silent: no end is invented, and no more signs go out for them.
pub(crate) fn subagent_stopped(input: &Value, cwd: &str) {
    let (Some(path), Some(agent_id)) = (known_session(input), Actor::of(input).agent_id) else {
        return;
    };
    if Settings::from_env().signs_only() {
        update(&path, |file: &mut SessionFile| {
            file.forget_subagent(agent_id)
        });
        return;
    }
    let now = now_ms();
    update(&path, |file: &mut SessionFile| {
        file.subagent_stopped(agent_id, now)
    });
    ensure_beater(&path, cwd);
}

/// SessionEnd: every open run ends now, sent from this hook so the end does
/// not wait on another process.
pub(crate) async fn session_ended(input: &Value, cwd: &str) {
    let Some(path) = known_session(input) else {
        return;
    };
    if Settings::from_env().signs_only() {
        // Signs of life invent no end: the runs fall silent, and read so.
        update(&path, |file: &mut SessionFile| {
            file.runs.clear();
            file.pending.clear();
            file.closed = true;
        });
        return;
    }
    let owes = update(&path, |file: &mut SessionFile| {
        file.end_all(SESSION_ENDED);
        file.subagents.clear();
        file.closed = true;
        !file.pending.is_empty() && !file.refused
    })
    .unwrap_or(false);
    if !owes {
        return;
    }
    let config = common::load_config(cwd);
    if config.is_configured() {
        let _ = tokio::time::timeout(Duration::from_secs(8), flush(&path, &config)).await;
    }
}

/// What a ContextStream tool call means for the session's runs.
enum Finding {
    /// `init` said who the session is, and where it works: the workspace and
    /// project it resolved, and the project's name.
    Identity {
        session: String,
        agent: Value,
        scope: Scope,
        project_name: Option<String>,
    },
    /// The model called `progress` (a report), and the answer is at hand.
    Report(Value),
    /// The same, answered in words: the run's id and state, when the words
    /// name them.
    ReportInText {
        run_id: Option<String>,
        state: Option<String>,
    },
    /// A ticket's status was set.
    Ticket(Value),
}

/// The `run_progress` object on `init`'s identity line.
fn identity_in_text(text: &str) -> Option<Value> {
    let line = text
        .lines()
        .find(|line| line.starts_with(mcp_tools::domains::progress::IDENTITY_LINE))?;
    serde_json::from_str(line.get(line.find('{')?..)?).ok()
}

/// Where the session works, as `init` says it in words (a local server): the
/// ids on its "Resolved workspace_id:" and "Resolved project_id:" lines, and
/// the project's name after ", project: " on its first line.
fn place_in_text(text: &str) -> (Scope, Option<String>) {
    let id_on = |label: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix(label))
            .map(str::trim)
            .filter(|id| uuid::Uuid::parse_str(id).is_ok())
            .map(str::to_string)
    };
    let name = text
        .lines()
        .next()
        .filter(|line| line.starts_with("Session ready for workspace: "))
        .and_then(|line| line.rsplit_once(", project: "))
        .map(|(_, name)| name.trim().to_string())
        .filter(|name| !name.is_empty());
    (
        Scope {
            workspace_id: id_on("Resolved workspace_id:"),
            project_id: id_on("Resolved project_id:"),
        },
        name,
    )
}

/// The word after `mark` in a tool's text (`run_id=...`, `state=...`).
fn word_after(text: &str, mark: &str) -> Option<String> {
    let rest = text.get(text.find(mark)? + mark.len()..)?;
    let word: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        .collect();
    (!word.is_empty()).then_some(word)
}

/// ContextStream's own tools whose ANSWERS the hooks read after the call
/// (PostToolUse): who the server says the session is (`init`), a run the
/// model reported (`progress`), a ticket taken up or moved on (`entity`).
/// [`classify`] reads no other tool, and setup builds the PostToolUse matcher
/// it writes for Claude Code from this list (`setup::hooks`), so a tool named
/// here reaches the hook in a real session. 1.0.77 named only `init` in that
/// matcher: the answers of `progress` and `entity` never reached the hook,
/// and with the switch on the hooks learned no run.
pub(crate) const ANSWERS_READ: &[&str] = &["init", "progress", "entity"];

/// The name Claude Code gives a tool of ContextStream's server in a matcher.
pub(crate) fn claude_tool_name(tool: &str) -> String {
    format!("mcp__contextstream__{tool}")
}

fn classify(tool: &str, tool_input: &Value, answer: &Answer) -> Option<Finding> {
    if !answer.succeeded || !ANSWERS_READ.contains(&tool) {
        return None;
    }
    let action = text(tool_input, "action").unwrap_or_default();
    match tool {
        "init" => {
            let identity = answer
                .json
                .and_then(|json| json.get("run_progress"))
                .cloned()
                .or_else(|| answer.text.and_then(identity_in_text))?;
            let (scope, project_name) = match answer.json {
                Some(json) => (
                    Scope {
                        workspace_id: uuid_text(json, "workspace_id"),
                        project_id: uuid_text(json, "project_id"),
                    },
                    json.get("project")
                        .and_then(|project| text(project, "name")),
                ),
                None => answer.text.map(place_in_text).unwrap_or_default(),
            };
            Some(Finding::Identity {
                session: text(&identity, "session")?,
                agent: identity
                    .get("agent")
                    .filter(|agent| agent.is_object())?
                    .clone(),
                // A project belongs to the workspace it was resolved in.
                scope: Scope {
                    project_id: scope.project_id.filter(|_| scope.workspace_id.is_some()),
                    workspace_id: scope.workspace_id,
                },
                project_name,
            })
        }
        "progress" if action.is_empty() || action == "report" => {
            let structured = answer
                .json
                .filter(|json| json.get("run").is_some() || json.get("recorded").is_some());
            if let Some(json) = structured {
                return Some(Finding::Report(json.clone()));
            }
            let said = answer.text?;
            if !said.contains("[PROGRESS] Recorded") {
                return None;
            }
            Some(Finding::ReportInText {
                run_id: word_after(said, " run_id=").filter(|id| uuid::Uuid::parse_str(id).is_ok()),
                state: word_after(said, " state="),
            })
        }
        "entity"
            if text(tool_input, "kind").as_deref() == Some("ticket")
                && matches!(action.as_str(), "update" | "create")
                && tool_input
                    .get("body")
                    .is_some_and(|body| text(body, "status").is_some()) =>
        {
            // The ticket as the API answered it, wherever the answer holds it.
            let answered = answer.json.and_then(|json| {
                [None, Some("ticket"), Some("data"), Some("entity")]
                    .into_iter()
                    .filter_map(|key| match key {
                        None => Some(json),
                        Some(key) => json.get(key),
                    })
                    .find(|ticket| {
                        uuid_text(ticket, "id").is_some() && text(ticket, "status").is_some()
                    })
                    .cloned()
            });
            answered
                .or_else(|| {
                    // Answered in words (a local server): the call itself
                    // names the ticket and the status it was given.
                    answer.text?;
                    let body = tool_input.get("body")?;
                    let mut ticket = Map::new();
                    ticket.insert("id".into(), json!(uuid_text(tool_input, "id")?));
                    ticket.insert("status".into(), json!(text(body, "status")?));
                    for key in ["workspace_id", "project_id"] {
                        if let Some(id) = uuid_text(tool_input, key) {
                            ticket.insert(key.into(), json!(id));
                        }
                    }
                    Some(Value::Object(ticket))
                })
                .map(Finding::Ticket)
        }
        _ => None,
    }
}

// ============================================================================
// The keep-alive process
// ============================================================================

/// Held by the one keep-alive process of a session for as long as it lives.
struct BeaterLock {
    _file: File,
}

fn beater_lock_path(path: &Path) -> PathBuf {
    path.with_extension("beater")
}

impl BeaterLock {
    fn take(path: &Path) -> Option<Self> {
        use std::io::Write;
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(beater_lock_path(path))
            .ok()?;
        fs2::FileExt::try_lock_exclusive(&file).ok()?;
        // The process id, for a person (or a test) who wants to see it.
        let _ = file.set_len(0);
        let _ = write!(file, "{}", std::process::id());
        Some(Self { _file: file })
    }
}

fn beater_running(path: &Path) -> bool {
    let Ok(file) = OpenOptions::new()
        .read(true)
        .write(true)
        .open(beater_lock_path(path))
    else {
        return false;
    };
    match fs2::FileExt::try_lock_exclusive(&file) {
        Ok(()) => {
            let _ = fs2::FileExt::unlock(&file);
            false
        }
        Err(_) => true,
    }
}

/// Start the session's keep-alive process when it has something to do and
/// none is running.
fn ensure_beater(path: &Path, cwd: &str) {
    if cfg!(test) {
        return;
    }
    let Some(file) = read(path) else {
        return;
    };
    if file.refused || (file.runs.is_empty() && file.pending.is_empty()) || beater_running(path) {
        return;
    }
    let now = now_ms();
    let start = update(path, |file: &mut SessionFile| {
        if now.saturating_sub(file.spawned_ms) < RESPAWN_AFTER_MS {
            return false;
        }
        file.spawned_ms = now;
        true
    })
    .unwrap_or(false);
    if start {
        spawn_beater(path, cwd);
    }
}

/// Start `contextstream-mcp hook run-progress-beat` detached, hand it the
/// job on stdin, and return without waiting. Its own process group keeps it
/// alive when the host reaps the hook's group; it watches the host itself.
fn spawn_beater(path: &Path, cwd: &str) {
    use std::process::{Command, Stdio};
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let mut command = Command::new(exe);
    command
        .args(["hook", BEAT_HOOK])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
    }
    let Ok(mut child) = command.spawn() else {
        return;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = serde_json::to_writer(&mut stdin, &json!({ "state": path, "cwd": cwd }));
    }
}

enum Sent {
    Ok(Value),
    /// The API will not take reports from this key here (401, 403, 404).
    Refused,
    /// The API will never take this report; the next one may be fine.
    Dropped,
    /// The API is away for a moment.
    Retry,
}

async fn send(config: &ApiConfig, body: &Value) -> Sent {
    let url = format!(
        "{}/api/v1/run-progress/report",
        config.api_url.trim_end_matches('/')
    );
    let response = super::api_http_client()
        .post(url)
        .header("X-API-Key", &config.api_key)
        .header("Content-Type", "application/json")
        .json(body)
        .timeout(SEND_TIMEOUT)
        .send()
        .await;
    match response {
        Ok(response) if response.status().is_success() => {
            // The API wraps its answer: `{success, data}`.
            let answer: Value = response.json().await.unwrap_or(Value::Null);
            Sent::Ok(answer.get("data").cloned().unwrap_or(answer))
        }
        Ok(response) => match response.status().as_u16() {
            401 | 403 | 404 => Sent::Refused,
            408 | 429 => Sent::Retry,
            400..=499 => Sent::Dropped,
            _ => Sent::Retry,
        },
        Err(_) => Sent::Retry,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Flush {
    Sent,
    Refused,
    Failed,
}

/// Send the queued reports, oldest first, one sender at a time.
async fn flush(path: &Path, config: &ApiConfig) -> Flush {
    let Ok(gate) = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("send"))
    else {
        return Flush::Failed;
    };
    let mut waited = 0;
    while fs2::FileExt::try_lock_exclusive(&gate).is_err() {
        // Another sender is at it; the queue is its to drain for now.
        if waited >= 60 {
            return Flush::Sent;
        }
        waited += 1;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    loop {
        let Some(next) = read(path).and_then(|file| file.pending.first().cloned()) else {
            return Flush::Sent;
        };
        match send(config, &next).await {
            Sent::Ok(_) | Sent::Dropped => {
                update(path, |file: &mut SessionFile| {
                    if file.pending.first() == Some(&next) {
                        file.pending.remove(0);
                    }
                });
            }
            Sent::Refused => {
                update(path, SessionFile::refuse);
                return Flush::Refused;
            }
            Sent::Retry => return Flush::Failed,
        }
    }
}

/// Whether the API's answer says the run is over by its emitter's own word.
fn answer_says_ended(answer: &Value) -> bool {
    let view = answer.get("run").unwrap_or(answer);
    text(view, "liveness").as_deref() == Some("ended")
        || matches!(
            text(view, "state").as_deref(),
            Some("stopped" | "finished" | "failed")
        )
}

fn run_ident(run: &Run) -> String {
    run.run_id
        .clone()
        .or_else(|| run.key.clone())
        .unwrap_or_default()
}

/// `contextstream-mcp hook run-progress-beat`: the job is on stdin.
pub async fn run_beater() -> anyhow::Result<()> {
    let job: Value = serde_json::from_reader(std::io::stdin().lock()).unwrap_or(Value::Null);
    let (Some(state), Some(cwd)) = (
        job.get("state").and_then(Value::as_str),
        job.get("cwd").and_then(Value::as_str),
    ) else {
        return Ok(());
    };
    beat(Path::new(state), cwd).await;
    Ok(())
}

/// Keep one session's runs alive until the session is over. Leaves when:
/// the session's process is gone (after ending its runs), the session ended,
/// nothing has been open for a minute, the API refuses, or it cannot be
/// reached five times in a row.
async fn beat(path: &Path, cwd: &str) {
    let settings = Settings::from_env();
    if !settings.enabled && !settings.signs {
        return;
    }
    let config = common::load_config(cwd);
    if !config.is_configured() {
        return;
    }
    let Some(_lock) = BeaterLock::take(path) else {
        return;
    };
    if let Some(dir) = path.parent() {
        prune(dir);
    }
    if settings.signs_only() {
        signs(path, &config).await;
        return;
    }
    let every_ms = beat_secs() * 1000;
    let tick = TICK.min(Duration::from_millis(every_ms));
    let mut last_sent: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    let mut failures = 0_u32;
    let mut idle_since: Option<u64> = None;
    loop {
        tokio::time::sleep(tick).await;
        let now = now_ms();
        let Some(file) = read(path) else {
            break;
        };
        if file.refused {
            break;
        }
        let host = host_state(&file);
        if host == Host::Gone && (!file.runs.is_empty() || !file.closed) {
            update(path, |file: &mut SessionFile| {
                // A hook may have found the session's new process meanwhile.
                if host_state(file) == Host::Gone {
                    file.end_all(HOST_GONE);
                    file.closed = true;
                }
            });
        }
        match flush(path, &config).await {
            Flush::Refused => break,
            Flush::Failed => failures += 1,
            Flush::Sent => {}
        }
        let Some(file) = read(path) else {
            break;
        };
        if failures >= FAILURE_LIMIT || (file.closed && file.pending.is_empty()) {
            break;
        }
        if file.runs.is_empty() {
            if file.pending.is_empty()
                && now.saturating_sub(*idle_since.get_or_insert(now)) >= IDLE_EXIT_MS
            {
                break;
            }
            continue;
        }
        idle_since = None;
        let silent = now.saturating_sub(file.last_hook_ms);
        let quiet = match host {
            Host::Alive => silent >= SILENT_SESSION_MS,
            // Without a process to watch, hook events are the only sign of life.
            _ => {
                silent >= QUIET_WITHOUT_HOST_MS
                    && file
                        .in_flight_ms
                        .is_none_or(|since| now.saturating_sub(since) >= IN_FLIGHT_WITHOUT_HOST_MS)
            }
        };
        if quiet {
            break;
        }
        let mut refused = false;
        for run in &file.runs {
            let ident = run_ident(run);
            let waits = run.hook_waiting || run.model_waiting;
            let Some((body, offers)) = file.signal(run) else {
                continue;
            };
            // A project to offer goes with the next look; a heartbeat waits
            // its turn.
            let due_ms = if offers {
                0
            } else if waits {
                every_ms * WAITING_BEATS
            } else {
                every_ms
            };
            let last = *last_sent.entry(ident.clone()).or_insert(now);
            if now.saturating_sub(last) < due_ms {
                continue;
            }
            let sent = send(&config, &Value::Object(body)).await;
            if offers && !matches!(sent, Sent::Retry) {
                // The offer is answered either way. A refusal of it is about
                // the project (one this key may not write), not the run: the
                // run stays as the model reported it, and is kept alive so.
                let taken = matches!(sent, Sent::Ok(_));
                update(path, |file: &mut SessionFile| {
                    file.offer_answered(&ident, taken)
                });
                if !taken {
                    last_sent.insert(ident, now_ms());
                    continue;
                }
            }
            match sent {
                Sent::Ok(answer) => {
                    failures = 0;
                    last_sent.insert(ident.clone(), now_ms());
                    if answer_says_ended(&answer) {
                        // Someone ended the run by its own word: it is not ours
                        // to keep alive.
                        update(path, |file: &mut SessionFile| {
                            file.runs.retain(|open| run_ident(open) != ident);
                        });
                    }
                }
                Sent::Dropped => {
                    last_sent.insert(ident, now_ms());
                }
                Sent::Refused => {
                    update(path, SessionFile::refuse);
                    refused = true;
                    break;
                }
                Sent::Retry => {
                    failures += 1;
                    break;
                }
            }
        }
        if refused || failures >= FAILURE_LIMIT {
            break;
        }
    }
}

/// The shortest span between two signs of life for one run: two beats, at
/// most two minutes (a minute when nothing is set). Under the hosted path's
/// promise of a signal every 120 s (stalled after 360 s), with room to spare.
fn sign_every_ms() -> u64 {
    (2 * beat_secs()).min(120) * 1000
}

/// Send a session's signs of life while there is something to send them for.
/// Leaves when the session ended or has no run, when no hook event came for
/// two intervals (the next one starts it again), when the API refuses, or
/// when it cannot be reached five times in a row. No timer: a sign goes out
/// only after a hook event ([`SessionFile::signs_due`]).
async fn signs(path: &Path, config: &ApiConfig) {
    let every_ms = sign_every_ms();
    let mut failures = 0_u32;
    loop {
        tokio::time::sleep(TICK).await;
        let now = now_ms();
        let Some(file) = read(path) else {
            break;
        };
        if file.refused
            || file.closed
            || file.runs.is_empty()
            || now.saturating_sub(file.last_hook_ms) >= 2 * every_ms
        {
            break;
        }
        for (ident, body) in file.signs_due(every_ms, now) {
            let ended = match send(config, &body).await {
                Sent::Ok(answer) => {
                    failures = 0;
                    answer_says_ended(&answer)
                }
                Sent::Dropped => false,
                Sent::Refused => {
                    update(path, SessionFile::refuse);
                    return;
                }
                Sent::Retry => {
                    failures += 1;
                    break;
                }
            };
            update(path, |file: &mut SessionFile| {
                if ended {
                    // Someone ended the run by its own word: no more signs.
                    file.runs.retain(|open| run_ident(open) != ident);
                } else if let Some(run) = file.runs.iter_mut().find(|open| run_ident(open) == ident)
                {
                    run.signed_ms = now;
                }
            });
        }
        if failures >= FAILURE_LIMIT {
            break;
        }
    }
}

#[cfg(test)]
#[path = "run_progress_tests.rs"]
mod tests;
