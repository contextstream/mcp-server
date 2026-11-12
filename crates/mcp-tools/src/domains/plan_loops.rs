//! Plan loops on the `session` tool (contract v1.2): `start_loop`,
//! `loop_status`, `advance_loop`, `resolve_gate`, `abort_loop`, `resume_loop`
//! and `list_loops`; and the tickets that ride a loop (contract v1.3):
//! `attach_ticket`, `detach_ticket` and `loop_tickets`.
//!
//! A thin client. Each action is one call to the ContextStream API (a title
//! given as `plan_id` is looked up first, as `update_plan` does, and
//! `loop_status` with `after` also reads the loop's events), made with the
//! caller's own credential. This process keeps no loop state: an API it
//! cannot reach is the client's usual typed error, never a local answer. The
//! only scope read from a call is the `workspace_id`/`project_id` the plan
//! actions already accept. What is cheap to check is checked before anything
//! is sent; the rest is the API's to judge.

use mcp_client::{
    ContextStreamClient, GateResolution, LoopTicketAttachment, NewLoopTicket, PlanLoopConfig,
    PlanLoopConflict, PlanLoopGate,
};
use mcp_session::SessionManager;
use mcp_types::{tool::ToolResult, Error, Result};
use serde_json::Value;
use uuid::Uuid;

use super::display_title::{one_line, quoted_body};
use super::lookup::{Lookup, LookupAction};
use super::session::{resolve_loop_plan, SessionInput};

/// The loop actions of the `session` tool.
pub(crate) const LOOP_ACTIONS: &[&str] = &[
    "start_loop",
    "loop_status",
    "advance_loop",
    "resolve_gate",
    "abort_loop",
    "resume_loop",
    "list_loops",
    "attach_ticket",
    "detach_ticket",
    "loop_tickets",
];

/// `resolve_gate` takes the affected step ids as `step_ids`; `steps` given
/// as ids (the contract's name) is read as `step_ids`, since the session
/// tool's `steps` are plan step objects.
pub(crate) fn normalize_loop_input(mut input: Value) -> Value {
    let Some(object) = input.as_object_mut() else {
        return input;
    };
    let resolving = object
        .get("action")
        .and_then(Value::as_str)
        .is_some_and(|action| action.trim().eq_ignore_ascii_case("resolve_gate"));
    if !resolving || object.contains_key("step_ids") {
        return input;
    }
    let ids = match object.get("steps") {
        Some(Value::Array(items)) if items.iter().all(Value::is_string) => {
            Some(Value::Array(items.clone()))
        }
        Some(Value::String(text)) => match serde_json::from_str::<Vec<String>>(text) {
            Ok(ids) => Some(Value::from(ids)),
            Err(_) if !text.trim().is_empty() && !text.trim_start().starts_with('[') => {
                Some(Value::from(vec![text.trim().to_string()]))
            }
            Err(_) => None,
        },
        _ => None,
    };
    if let Some(ids) = ids {
        object.remove("steps");
        object.insert("step_ids".to_string(), ids);
    }
    input
}

/// Run one loop action. `action` is one of [`LOOP_ACTIONS`], lower case.
pub(crate) async fn execute(
    client: &ContextStreamClient,
    session: &SessionManager,
    action: &str,
    input: SessionInput,
) -> Result<ToolResult> {
    match action {
        "start_loop" => start_loop(client, session, input).await,
        "loop_status" => loop_status(client, session, input).await,
        "advance_loop" => advance_loop(client, input).await,
        "resolve_gate" => resolve_gate(client, input).await,
        "abort_loop" => abort_loop(client, input).await,
        "resume_loop" => resume_loop(client, input).await,
        "list_loops" => list_loops(client, input).await,
        "attach_ticket" => attach_ticket(client, input).await,
        "detach_ticket" => detach_ticket(client, input).await,
        "loop_tickets" => loop_tickets(client, input).await,
        other => Err(Error::Validation(format!(
            "Unknown loop action: {other}. Use one of: {}.",
            LOOP_ACTIONS.join(", ")
        ))),
    }
}

// ============================================================================
// Actions
// ============================================================================

async fn start_loop(
    client: &ContextStreamClient,
    session: &SessionManager,
    input: SessionInput,
) -> Result<ToolResult> {
    let reference = non_empty(input.plan_id.as_deref()).ok_or_else(|| {
        Error::Validation(
            "plan_id is required for start_loop: the id (or the title) of a saved plan. Save one first with session(action=\"capture_plan\", ...).".to_string(),
        )
    })?;
    let config = loop_config(&input)?;
    let found = match resolve_loop_plan(
        client,
        session,
        reference,
        input.workspace_id.as_deref(),
        input.project_id.as_deref(),
        LookupAction::edit("started"),
        "session(action=\"start_loop\", plan_id=\"<id>\")",
    )
    .await?
    {
        Lookup::Found(found) => found,
        Lookup::Candidates(candidates) => return Ok(candidates),
    };
    let view = client
        .start_plan_loop(found.id, &config)
        .await
        .map_err(|error| explain_conflict(error, &format!("plan_id=\"{}\"", found.id)))?;
    Ok(found.tool_result(loop_view_text("Loop started.", &view), view))
}

async fn loop_status(
    client: &ContextStreamClient,
    session: &SessionManager,
    input: SessionInput,
) -> Result<ToolResult> {
    let after = after_cursor(input.after.as_ref())?;
    let (view, found) = if let Some(raw) = non_empty(input.loop_id.as_deref()) {
        let loop_id = loop_uuid(Some(raw))?;
        (client.get_plan_loop(loop_id).await?, None)
    } else if let Some(reference) = non_empty(input.plan_id.as_deref()) {
        let found = match resolve_loop_plan(
            client,
            session,
            reference,
            input.workspace_id.as_deref(),
            input.project_id.as_deref(),
            LookupAction::READ,
            "session(action=\"loop_status\", plan_id=\"<id>\")",
        )
        .await?
        {
            Lookup::Found(found) => found,
            Lookup::Candidates(candidates) => return Ok(candidates),
        };
        let view = client
            .plan_loop_of_plan(found.id)
            .await
            .map_err(|error| no_loop_on_plan(error, found.id))?;
        (view, Some(found))
    } else {
        return Err(Error::Validation(
            "loop_status needs loop_id, or plan_id for the plan's latest loop. session(action=\"list_loops\") lists the loops in scope.".to_string(),
        ));
    };

    let mut text = loop_view_text("Loop status.", &view);
    let mut structured = view;
    if let Some(after) = after {
        let loop_id = structured
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| Uuid::parse_str(id).ok())
            .ok_or_else(|| {
                Error::Validation(
                    "the API's loop view carried no loop id, so its events cannot be read"
                        .to_string(),
                )
            })?;
        let events = client
            .plan_loop_events(loop_id, Some(after.as_str()), input.limit)
            .await?;
        text.push('\n');
        text.push_str(&events_text(&events, &after));
        if let Some(object) = structured.as_object_mut() {
            object.insert("events_page".to_string(), events);
        }
    }
    Ok(match found {
        Some(found) => found.tool_result(text, structured),
        None => ToolResult::with_structured(text, structured),
    })
}

async fn advance_loop(client: &ContextStreamClient, input: SessionInput) -> Result<ToolResult> {
    let loop_id = loop_uuid(input.loop_id.as_deref())?;
    let step_id = non_empty(input.step_id.as_deref()).ok_or_else(|| {
        Error::Validation(
            "step_id is required for advance_loop: the step the loop expects, once it is done (loop_status names it).".to_string(),
        )
    })?;
    let view = client
        .advance_plan_loop(loop_id, step_id, non_empty(input.summary.as_deref()))
        .await
        .map_err(|error| explain_conflict(error, &format!("loop_id=\"{loop_id}\"")))?;
    let headline = if is_noop(&view) {
        format!(
            "Step {} was already recorded in this iteration; nothing changed.",
            one_line(step_id)
        )
    } else {
        format!("Step {} recorded.", one_line(step_id))
    };
    Ok(ToolResult::with_structured(
        loop_view_text(&headline, &view),
        view,
    ))
}

async fn resolve_gate(client: &ContextStreamClient, input: SessionInput) -> Result<ToolResult> {
    let loop_id = loop_uuid(input.loop_id.as_deref())?;
    let gate_id = non_empty(input.gate_id.as_deref()).ok_or_else(|| {
        Error::Validation(
            "gate_id is required for resolve_gate: the open gate's id (loop_status shows it)."
                .to_string(),
        )
    })?;
    let outcome = non_empty(input.outcome.as_deref())
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| {
            Error::Validation(
                "outcome is required for resolve_gate: approve, request_changes or abort."
                    .to_string(),
            )
        })?;
    if !mcp_client::plan_loops::GATE_OUTCOMES.contains(&outcome.as_str()) {
        return Err(Error::Validation(format!(
            "outcome must be approve, request_changes or abort, not {:?}.",
            one_line(&outcome)
        )));
    }
    let note = non_empty(input.note.as_deref()).map(str::to_string);
    if outcome == "request_changes" && note.is_none() {
        return Err(Error::Validation(
            "note is required for request_changes: say what to change, so the next pass can use it."
                .to_string(),
        ));
    }
    let steps = input.step_ids.map(|ids| {
        ids.into_iter()
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
            .collect::<Vec<_>>()
    });
    let resolution = GateResolution {
        outcome: outcome.clone(),
        note,
        steps: steps.filter(|ids| !ids.is_empty()),
    };
    let view = client
        .resolve_plan_loop_gate(loop_id, gate_id, &resolution)
        .await
        .map_err(|error| explain_conflict(error, &format!("loop_id=\"{loop_id}\"")))?;
    let headline = if is_noop(&view) {
        format!("The gate was already resolved with {outcome}; nothing changed.")
    } else {
        format!("Gate resolved: {outcome}.")
    };
    Ok(ToolResult::with_structured(
        loop_view_text(&headline, &view),
        view,
    ))
}

async fn abort_loop(client: &ContextStreamClient, input: SessionInput) -> Result<ToolResult> {
    let loop_id = loop_uuid(input.loop_id.as_deref())?;
    let view = client
        .abort_plan_loop(loop_id, non_empty(input.reason.as_deref()))
        .await
        .map_err(|error| explain_conflict(error, &format!("loop_id=\"{loop_id}\"")))?;
    Ok(ToolResult::with_structured(
        loop_view_text("Loop aborted.", &view),
        view,
    ))
}

async fn resume_loop(client: &ContextStreamClient, input: SessionInput) -> Result<ToolResult> {
    let loop_id = loop_uuid(input.loop_id.as_deref())?;
    let max_iterations = checked_max_iterations(input.max_iterations)?;
    let view = client
        .resume_plan_loop(loop_id, max_iterations)
        .await
        .map_err(|error| explain_conflict(error, &format!("loop_id=\"{loop_id}\"")))?;
    Ok(ToolResult::with_structured(
        loop_view_text("Loop resumed.", &view),
        view,
    ))
}

async fn list_loops(client: &ContextStreamClient, input: SessionInput) -> Result<ToolResult> {
    let workspace_id = scope_uuid(input.workspace_id.as_deref(), "workspace_id")?;
    let project_id = scope_uuid(input.project_id.as_deref(), "project_id")?;
    // `loop_state` is the filter; `status` is read too when it names a loop
    // state (the session tool's `status` lists plan states).
    let state = non_empty(input.loop_state.as_deref())
        .or_else(|| {
            non_empty(input.status.as_deref())
                .filter(|status| mcp_client::plan_loops::LOOP_STATES.contains(status))
        })
        .map(str::to_ascii_lowercase);
    if let Some(state) = state.as_deref() {
        if !mcp_client::plan_loops::LOOP_STATES.contains(&state) {
            return Err(Error::Validation(format!(
                "loop_state must be one of {}, not {:?}.",
                mcp_client::plan_loops::LOOP_STATES.join(", "),
                one_line(state)
            )));
        }
    }
    let result = client
        .list_plan_loops(workspace_id, project_id, state.as_deref(), input.limit)
        .await?;
    Ok(ToolResult::with_structured(list_text(&result), result))
}

async fn attach_ticket(client: &ContextStreamClient, input: SessionInput) -> Result<ToolResult> {
    let loop_id = loop_uuid(input.loop_id.as_deref())?;
    let step_id = non_empty(input.step_id.as_deref()).map(str::to_string);
    let ticket = non_empty(input.ticket_id.as_deref());
    let create = input.create.as_ref().filter(|create| !create.is_null());
    let (attachment, headline) = match (ticket, create) {
        (Some(_), Some(_)) => {
            return Err(Error::Validation(
                "attach_ticket takes ticket_id (a ticket that exists) or create (a new one), not both."
                    .to_string(),
            ))
        }
        (Some(ticket), None) => (
            LoopTicketAttachment {
                ticket_id: Some(ticket_uuid(Some(ticket))?),
                step_id,
                ..LoopTicketAttachment::default()
            },
            "Ticket attached.",
        ),
        (None, Some(create)) => (
            LoopTicketAttachment {
                create: Some(new_ticket(create)?),
                step_id,
                ..LoopTicketAttachment::default()
            },
            "Ticket created and attached.",
        ),
        (None, None) => {
            return Err(Error::Validation(
                "attach_ticket needs ticket_id (a ticket that exists; entity(kind=\"ticket\", action=\"list\") shows ids) or create={\"title\": \"...\"} for a new one in the loop's project."
                    .to_string(),
            ))
        }
    };
    let result = client
        .attach_plan_loop_ticket(loop_id, &attachment)
        .await
        .map_err(|error| explain_conflict(error, &format!("loop_id=\"{loop_id}\"")))?;
    Ok(ToolResult::with_structured(
        ticket_change_text(headline, &result),
        result,
    ))
}

async fn detach_ticket(client: &ContextStreamClient, input: SessionInput) -> Result<ToolResult> {
    let loop_id = loop_uuid(input.loop_id.as_deref())?;
    let ticket_id = ticket_uuid(input.ticket_id.as_deref())?;
    let result = client
        .detach_plan_loop_ticket(loop_id, ticket_id)
        .await
        .map_err(|error| explain_conflict(error, &format!("loop_id=\"{loop_id}\"")))?;
    let headline = format!("Ticket {ticket_id} detached; the ticket itself is unchanged.");
    Ok(ToolResult::with_structured(
        ticket_change_text(&headline, &result),
        result,
    ))
}

async fn loop_tickets(client: &ContextStreamClient, input: SessionInput) -> Result<ToolResult> {
    let loop_id = loop_uuid(input.loop_id.as_deref())?;
    let result = client.plan_loop_tickets(loop_id).await?;
    Ok(ToolResult::with_structured(
        tickets_text(&result, loop_id),
        result,
    ))
}

// ============================================================================
// Input checks
// ============================================================================

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

fn loop_uuid(raw: Option<&str>) -> Result<Uuid> {
    let raw = non_empty(raw).ok_or_else(|| {
        Error::Validation(
            "loop_id is required: the loop's id (start_loop, loop_status and list_loops show it)."
                .to_string(),
        )
    })?;
    Uuid::parse_str(raw).map_err(|_| {
        Error::Validation(
            "loop_id must be the loop's UUID (start_loop, loop_status and list_loops show it)."
                .to_string(),
        )
    })
}

fn ticket_uuid(raw: Option<&str>) -> Result<Uuid> {
    let raw = non_empty(raw).ok_or_else(|| {
        Error::Validation(
            "ticket_id is required: the ticket's id (entity(kind=\"ticket\", action=\"list\") and loop_tickets show it)."
                .to_string(),
        )
    })?;
    Uuid::parse_str(raw).map_err(|_| {
        Error::Validation(
            "ticket_id must be the ticket's UUID (entity(kind=\"ticket\", action=\"list\") and loop_tickets show it)."
                .to_string(),
        )
    })
}

/// The ticket `attach_ticket` creates, from the call's `create` object.
fn new_ticket(create: &Value) -> Result<NewLoopTicket> {
    let mut ticket: NewLoopTicket = serde_json::from_value(create.clone()).map_err(|error| {
        Error::Validation(format!(
            "create: {error}. A new ticket is {{title, description, kind: bug|feature|task|chore|incident|epic, priority, labels, assignees}}."
        ))
    })?;
    ticket.title = ticket.title.trim().to_string();
    if ticket.title.is_empty() {
        return Err(Error::Validation(
            "create.title is required: the new ticket's title.".to_string(),
        ));
    }
    if let Some(assignees) = ticket.assignees.take() {
        let assignees =
            mcp_client::normalize_assignees(&assignees).map_err(|error| match error {
                Error::Validation(message) => Error::Validation(format!("create.{message}")),
                other => other,
            })?;
        ticket.assignees = Some(assignees);
    }
    Ok(ticket)
}

fn scope_uuid(raw: Option<&str>, field: &str) -> Result<Option<Uuid>> {
    match non_empty(raw) {
        None => Ok(None),
        Some(raw) => Uuid::parse_str(raw)
            .map(Some)
            .map_err(|_| Error::Validation(format!("{field} must be a UUID"))),
    }
}

fn checked_max_iterations(value: Option<i64>) -> Result<Option<i64>> {
    match value {
        Some(value) if value < 1 => Err(Error::Validation(format!(
            "max_iterations must be at least 1 (the API allows up to 20), not {value}."
        ))),
        other => Ok(other),
    }
}

/// The `config` of start_loop from the call's flat fields.
fn loop_config(input: &SessionInput) -> Result<PlanLoopConfig> {
    if let Some(n) = input.review_every_n.filter(|n| *n < 1) {
        return Err(Error::Validation(format!(
            "review_every_n must be at least 1, not {n}."
        )));
    }
    let gates = match &input.gates {
        None => None,
        Some(gates) => Some(
            gates
                .iter()
                .enumerate()
                .map(|(index, gate)| {
                    let gate: PlanLoopGate =
                        serde_json::from_value(gate.clone()).map_err(|error| {
                            Error::Validation(format!(
                                "gates[{index}]: {error}. A gate is {{step_id, type: human_review|status_report, required, reviewer_roles, reviewer_user_ids, channel: coflow|contextstream|both, sla_hours}}."
                            ))
                        })?;
                    if gate.step_id.trim().is_empty() {
                        return Err(Error::Validation(format!(
                            "gates[{index}]: step_id is required (the step whose result the gate reviews)."
                        )));
                    }
                    Ok(gate)
                })
                .collect::<Result<Vec<_>>>()?,
        ),
    };
    let channel = match non_empty(input.channel.as_deref()).map(str::to_ascii_lowercase) {
        None => None,
        Some(channel) if mcp_client::plan_loops::GATE_CHANNELS.contains(&channel.as_str()) => {
            Some(channel)
        }
        Some(other) => {
            return Err(Error::Validation(format!(
                "channel must be coflow, contextstream or both, not {other:?}."
            )))
        }
    };
    Ok(PlanLoopConfig {
        channel,
        max_iterations: checked_max_iterations(input.max_iterations)?,
        review_policy: non_empty(input.review_policy.as_deref()).map(str::to_ascii_lowercase),
        review_every_n: input.review_every_n,
        gates,
        report_cadence_minutes: input.report_cadence_minutes,
        audience: input.audience.as_ref().map(|labels| {
            labels
                .iter()
                .map(|label| label.trim().to_string())
                .filter(|label| !label.is_empty())
                .collect()
        }),
    })
}

/// The events cursor: `next_after` of the previous page (a sequence number).
fn after_cursor(value: Option<&Value>) -> Result<Option<String>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(number)) => Ok(Some(number.to_string())),
        Some(Value::String(text)) if !text.trim().is_empty() => Ok(Some(text.trim().to_string())),
        Some(Value::String(_)) => Ok(None),
        Some(_) => Err(Error::Validation(
            "after must be the events cursor (next_after of the previous page; 0 for the first)"
                .to_string(),
        )),
    }
}

// ============================================================================
// API answers in plain words
// ============================================================================

/// What a 409 from a loop route means and what to do next, after the API's
/// own message. Every other error is passed on unchanged.
fn explain_conflict(error: Error, target: &str) -> Error {
    let Some(conflict) = PlanLoopConflict::from_error(&error) else {
        return error;
    };
    let advice = match conflict {
        PlanLoopConflict::LoopAlreadyRunning => format!(
            "This plan already has a loop that is not finished; no second one was started. Read it with session(action=\"loop_status\", {target}), or end it with abort_loop first."
        ),
        PlanLoopConflict::StepOutOfOrder => format!(
            "That is not the step the loop expects; nothing was recorded. session(action=\"loop_status\", {target}) names the expected step: finish and advance that one."
        ),
        PlanLoopConflict::PlanVersionConflict => format!(
            "The plan's steps changed after the loop took them, so the loop is now blocked (plan changed). Check the plan, then call session(action=\"resume_loop\", {target}) to continue with the new steps."
        ),
        PlanLoopConflict::GateAlreadyResolved => format!(
            "That gate was already resolved with another outcome; nothing was changed. Read the loop with session(action=\"loop_status\", {target})."
        ),
        PlanLoopConflict::IllegalTransition => format!(
            "The loop's state does not allow that: it may be completed or aborted, the gate may not be open, or it is not blocked (only a blocked loop resumes). Nothing was changed; read it with session(action=\"loop_status\", {target})."
        ),
        PlanLoopConflict::TicketInAnotherLoop => format!(
            "That ticket already rides another live loop (the API names it above); a ticket rides one live loop at a time, so it was not attached here. Take it off that loop first with session(action=\"detach_ticket\", loop_id=\"<that loop>\", ticket_id=\"<the ticket>\"), then attach it to this one ({target})."
        ),
        PlanLoopConflict::StepHasOpenTickets => format!(
            "That step still has tickets that are not done, so the loop did not advance. session(action=\"loop_tickets\", {target}) lists them: finish them (a ticket is done when it is resolved or closed), or detach the ones that no longer belong to the step, then advance again."
        ),
    };
    match error {
        Error::Http {
            status,
            message,
            code,
            source,
        } => Error::Http {
            status,
            message: format!("{message}\n[PLAN_LOOP] {}: {advice}", conflict.code()),
            code,
            source,
        },
        other => other,
    }
}

/// A 404 for a plan's loop: the plan has none yet, or this key cannot read it.
fn no_loop_on_plan(error: Error, plan_id: Uuid) -> Error {
    match error {
        Error::Http {
            status: 404,
            message,
            code,
            source,
        } => Error::Http {
            status: 404,
            message: format!(
                "{message}\n[PLAN_LOOP] No loop was found for plan {plan_id}: none was started, or this key cannot read the plan. Start one with session(action=\"start_loop\", plan_id=\"{plan_id}\")."
            ),
            code,
            source,
        },
        other => other,
    }
}

fn is_noop(view: &Value) -> bool {
    view.get("noop").and_then(Value::as_bool).unwrap_or(false)
}

fn text_field<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

fn int_field(value: &Value, key: &str) -> Option<i64> {
    value.get(key).and_then(Value::as_i64)
}

fn string_list(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    item.as_str()
                        .map(str::to_string)
                        .or_else(|| item.get("id").and_then(Value::as_str).map(str::to_string))
                })
                .map(|item| one_line(&item))
                .filter(|item| !item.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn step_title<'a>(view: &'a Value, step_id: &str) -> Option<&'a str> {
    view.get("steps")
        .and_then(Value::as_array)?
        .iter()
        .find(|step| step.get("id").and_then(Value::as_str) == Some(step_id))
        .and_then(|step| text_field(step, &["title"]))
}

fn step_label(view: &Value, step_id: &str) -> String {
    match step_title(view, step_id) {
        Some(title) => format!("{} ({})", one_line(step_id), one_line(title)),
        None => one_line(step_id),
    }
}

/// The open gate in one line: its id, the step it reviews and who it is for.
fn gate_line(view: &Value, gate: &Value) -> String {
    let mut line = format!(
        "Open gate: gate_id={}",
        text_field(gate, &["id", "gate_id"])
            .map(one_line)
            .unwrap_or_else(|| "?".to_string())
    );
    if let Some(step) = text_field(gate, &["step_id"]) {
        line.push_str(&format!(" reviews step {}", step_label(view, step)));
    }
    let kind = text_field(gate, &["type", "gate_type"]).map(one_line);
    let required = gate.get("required").and_then(Value::as_bool);
    match (kind, required) {
        (Some(kind), Some(true)) => line.push_str(&format!(" [{kind}, required]")),
        (Some(kind), _) => line.push_str(&format!(" [{kind}]")),
        (None, Some(true)) => line.push_str(" [required]"),
        _ => {}
    }
    let roles = string_list(gate.get("reviewer_roles"));
    let people = string_list(gate.get("reviewer_user_ids"));
    match (roles.is_empty(), people.is_empty()) {
        (true, true) => line.push_str("; for anyone with write access to the plan's project"),
        _ => {
            line.push_str("; for");
            if !roles.is_empty() {
                line.push_str(&format!(" roles {}", roles.join(", ")));
            }
            if !people.is_empty() {
                if !roles.is_empty() {
                    line.push(';');
                }
                line.push_str(&format!(" reviewer_user_ids {}", people.join(", ")));
            }
        }
    }
    if let Some(channel) = text_field(gate, &["channel"]) {
        line.push_str(&format!("; channel {}", one_line(channel)));
    }
    if let Some(due) = text_field(gate, &["due_at", "sla_due_at"]) {
        line.push_str(&format!("; due {}", one_line(due)));
    } else if let Some(hours) = int_field(gate, "sla_hours") {
        line.push_str(&format!("; SLA {hours} h"));
    }
    line
}

/// The review note the loop carries into its next pass, quoted so it cannot
/// pass for the tool's own lines.
fn replan_lines(view: &Value) -> Option<String> {
    let context = view
        .get("replan_context")
        .filter(|value| !value.is_null())?;
    let (note, steps, iteration) = match context {
        Value::String(note) => (Some(note.as_str()), Vec::new(), None),
        Value::Object(_) => (
            text_field(context, &["review_note", "note"]),
            {
                let mut steps = Vec::new();
                for key in ["steps", "affected_steps", "affected_step_ids", "step_ids"] {
                    steps = string_list(context.get(key));
                    if !steps.is_empty() {
                        break;
                    }
                }
                steps
            },
            int_field(context, "iteration"),
        ),
        _ => (None, Vec::new(), None),
    };
    let note = note.map(str::trim).filter(|note| !note.is_empty());
    if note.is_none() && steps.is_empty() {
        return None;
    }
    let mut text = match iteration {
        Some(iteration) => {
            format!("Replan context (review note for iteration {iteration}; use it on this pass):")
        }
        None => "Replan context (review note; use it on this pass):".to_string(),
    };
    if let Some(note) = note {
        for line in quoted_body(note).lines() {
            text.push_str("\n  | ");
            text.push_str(line);
        }
    }
    if !steps.is_empty() {
        text.push_str(&format!("\nSteps to redo: {}", steps.join(", ")));
    }
    Some(text)
}

/// A loop view in a few lines: status, iteration, the expected step, the
/// open gate, the review note to use, and the call to make next.
fn loop_view_text(headline: &str, view: &Value) -> String {
    let id = text_field(view, &["id", "loop_id"]).map(one_line);
    let status = text_field(view, &["status"]).map(one_line);
    let mut head = format!("[PLAN_LOOP] {headline}");
    head.push_str(&format!(
        "\nstatus={}",
        status.as_deref().unwrap_or("unknown")
    ));
    if let Some(reason) = text_field(view, &["blocked_reason"]) {
        head.push_str(&format!(" blocked_reason={}", one_line(reason)));
    }
    match (
        int_field(view, "iteration"),
        int_field(view, "max_iterations"),
    ) {
        (Some(n), Some(max)) => head.push_str(&format!(" iteration={n}/{max}")),
        (Some(n), None) => head.push_str(&format!(" iteration={n}")),
        _ => {}
    }
    if let Some(id) = &id {
        head.push_str(&format!(" loop_id={id}"));
    }
    if let Some(plan) = text_field(view, &["plan_id"]) {
        head.push_str(&format!(" plan_id={}", one_line(plan)));
    }
    if view.get("plan_changed").and_then(Value::as_bool) == Some(true) {
        head.push_str(" plan_changed=true");
    }
    let mut lines = vec![head];

    let expected = text_field(view, &["expected_step_id"]);
    if let Some(step) = expected {
        lines.push(format!("Expected step: {}", step_label(view, step)));
    }
    let gate = view.get("open_gate").filter(|gate| gate.is_object());
    if let Some(gate) = gate {
        lines.push(gate_line(view, gate));
    }
    if let Some(rollup) = view.get("tickets").and_then(rollup_words) {
        lines.push(format!("Tickets: {rollup}"));
    }
    if let Some(health) = text_field(view, &["health"]).filter(|health| *health != "on_track") {
        let reasons = string_list(view.get("health_reasons"));
        lines.push(if reasons.is_empty() {
            format!("Health: {}", one_line(health))
        } else {
            format!("Health: {} ({})", one_line(health), reasons.join("; "))
        });
    }
    if let Some(replan) = replan_lines(view) {
        lines.push(replan);
    }

    let loop_ref = id
        .as_deref()
        .map(|id| format!("loop_id=\"{id}\""))
        .unwrap_or_else(|| "loop_id=\"<id>\"".to_string());
    let next = match status.as_deref() {
        Some("running") => expected.map(|step| {
            let step = one_line(step);
            format!(
                "Next: do step {step}, then session(action=\"advance_loop\", {loop_ref}, step_id=\"{step}\", summary=\"<what was done>\")."
            )
        }),
        Some("awaiting_review") => {
            let gate_id = gate
                .and_then(|gate| text_field(gate, &["id", "gate_id"]))
                .map(one_line)
                .unwrap_or_else(|| "<gate_id>".to_string());
            Some(format!(
                "Next: stop here until the gate is resolved. The reviewer resolves it in Coflow or ContextStream, or with session(action=\"resolve_gate\", {loop_ref}, gate_id=\"{gate_id}\", outcome=\"approve|request_changes|abort\", note=\"...\"); check with session(action=\"loop_status\", {loop_ref})."
            ))
        }
        Some("blocked") => Some(match text_field(view, &["blocked_reason"]) {
            Some("plan_changed") => format!(
                "Next: the plan's steps changed after the loop took them. Check the plan, then session(action=\"resume_loop\", {loop_ref}) continues with the new steps."
            ),
            Some("max_iterations") => format!(
                "Next: every iteration is used. session(action=\"resume_loop\", {loop_ref}, max_iterations=<more, up to 20>) allows more, or session(action=\"abort_loop\", {loop_ref}) ends it."
            ),
            _ => format!(
                "Next: session(action=\"resume_loop\", {loop_ref}) once the cause is fixed, or session(action=\"abort_loop\", {loop_ref})."
            ),
        }),
        Some("completed") => Some("The loop is complete: every step is done.".to_string()),
        Some("aborted") => Some("The loop is aborted; start a new one with start_loop.".to_string()),
        _ => None,
    };
    if let Some(next) = next {
        lines.push(next);
    }
    if let Some(link) = view
        .get("links")
        .and_then(|links| text_field(links, &["plan"]))
    {
        lines.push(format!("Plan: {}", one_line(link)));
    }
    lines.join("\n")
}

/// A ticket rollup in words: how many of a loop's (or a step's) live tickets
/// are done, and the ones that need attention. `None` without tickets.
fn rollup_words(rollup: &Value) -> Option<String> {
    let total = int_field(rollup, "total").filter(|total| *total > 0)?;
    let mut words = format!("{}/{total} done", int_field(rollup, "done").unwrap_or(0));
    for (key, label) in [
        ("in_progress", "in progress"),
        ("in_review", "in review"),
        ("blocked", "blocked"),
    ] {
        if let Some(count) = int_field(rollup, key).filter(|count| *count > 0) {
            words.push_str(&format!(", {count} {label}"));
        }
    }
    Some(words)
}

/// One ticket riding a loop on one line: where it stands, who has it, and
/// its newest run.
fn ticket_line(ticket: &Value) -> String {
    let mut line = format!(
        "- ticket_id={}",
        text_field(ticket, &["ticket_id", "id"])
            .map(one_line)
            .unwrap_or_else(|| "?".to_string())
    );
    if let Some(kind) = text_field(ticket, &["kind"]) {
        line.push_str(&format!(" [{}]", one_line(kind)));
    }
    if let Some(station) = text_field(ticket, &["station"]) {
        line.push_str(&format!(" station={}", one_line(station)));
    }
    if let Some(status) = text_field(ticket, &["status"]) {
        line.push_str(&format!(" status={}", one_line(status)));
    }
    if let Some(step) = text_field(ticket, &["step_id"]) {
        line.push_str(&format!(" step={}", one_line(step)));
    }
    if let Some(assignee) = ticket
        .get("assignee")
        .filter(|assignee| assignee.is_object())
    {
        if let Some(name) = text_field(assignee, &["handle", "name", "user_id"]) {
            line.push_str(&format!(" assignee={}", one_line(name)));
            if text_field(assignee, &["entity_type"]) == Some("agent") {
                line.push_str(" [agent]");
            }
        }
    }
    if ticket.get("blocked").and_then(Value::as_bool) == Some(true) {
        match text_field(ticket, &["blocked_reason"]) {
            Some(reason) => line.push_str(&format!(" blocked ({})", one_line(reason))),
            None => line.push_str(" blocked"),
        }
    }
    if let Some(run) = ticket.get("run").filter(|run| run.is_object()) {
        let mut words = vec![text_field(run, &["agent_name"])
            .map(one_line)
            .unwrap_or_else(|| "an agent".to_string())];
        if let Some(state) = text_field(run, &["state"]) {
            words.push(one_line(state));
        }
        if let Some(liveness) = text_field(run, &["liveness"]) {
            words.push(format!("({})", one_line(liveness)));
        }
        if let Some(step) = text_field(run, &["step_label"]) {
            words.push(format!("{:?}", one_line(step)));
        }
        if let (Some(done), Some(total)) =
            (int_field(run, "plan_done"), int_field(run, "plan_total"))
        {
            if total > 0 {
                words.push(format!("{done}/{total}"));
            }
        }
        line.push_str(&format!("; run: {}", words.join(" ")));
    }
    if let Some(title) = text_field(ticket, &["title"]) {
        line.push_str(&format!(" — {}", one_line(title)));
    }
    line
}

/// An attach or a detach: what was done, the ticket, and the loop after it.
fn ticket_change_text(headline: &str, result: &Value) -> String {
    let mut lines = vec![format!("[PLAN_LOOP] {headline}")];
    if let Some(ticket) = result.get("ticket").filter(|ticket| ticket.is_object()) {
        lines.push(ticket_line(ticket));
    }
    if let Some(view) = result.get("loop").filter(|view| view.is_object()) {
        // The loop's own lines, under this answer's headline.
        let text = loop_view_text(headline, view);
        lines.extend(text.lines().skip(1).map(str::to_string));
    }
    lines.join("\n")
}

/// The tickets riding a loop, one line each, under their rollup.
fn tickets_text(result: &Value, loop_id: Uuid) -> String {
    let tickets = result
        .get("tickets")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if tickets.is_empty() {
        return format!(
            "[LOOP_TICKETS] none on loop {loop_id}. Attach one with session(action=\"attach_ticket\", loop_id=\"{loop_id}\", ticket_id=\"<id>\"), or create one there with create={{\"title\": \"...\"}}."
        );
    }
    let mut text = format!(
        "[LOOP_TICKETS] {} ticket(s) on loop {loop_id}",
        tickets.len()
    );
    match result.get("rollup").and_then(rollup_words) {
        Some(rollup) => text.push_str(&format!(": {rollup}.")),
        None => text.push('.'),
    }
    for ticket in &tickets {
        text.push('\n');
        text.push_str(&ticket_line(ticket));
    }
    text
}

/// A page of loop events, one line each.
fn events_text(page: &Value, after: &str) -> String {
    let events = page
        .get("events")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut text = if events.is_empty() {
        format!("Events after {}: none.", one_line(after))
    } else {
        format!("Events after {} ({}):", one_line(after), events.len())
    };
    for event in &events {
        let mut line = String::from("- ");
        if let Some(seq) = event.get("seq").filter(|seq| !seq.is_null()) {
            line.push_str(&format!("#{} ", one_line(&seq.to_string())));
        }
        line.push_str(
            &text_field(event, &["transition"])
                .map(one_line)
                .unwrap_or_else(|| "event".to_string()),
        );
        if let Some(n) = int_field(event, "iteration") {
            line.push_str(&format!(" iteration={n}"));
        }
        if let Some(step) = text_field(event, &["step_id"]) {
            line.push_str(&format!(" step={}", one_line(step)));
        }
        if let Some(gate) = text_field(event, &["gate_id"]) {
            line.push_str(&format!(" gate={}", one_line(gate)));
        }
        if let Some(at) = text_field(event, &["occurred_at"]) {
            line.push_str(&format!(" at={}", one_line(at)));
        }
        if let Some(summary) = text_field(event, &["summary"]) {
            line.push_str(&format!(" — {}", one_line(summary)));
        }
        text.push('\n');
        text.push_str(&line);
    }
    if let Some(next) = page.get("next_after").filter(|next| !next.is_null()) {
        let next = match next {
            Value::String(text) => one_line(text),
            other => one_line(&other.to_string()),
        };
        text.push_str(&format!("\nNext page: after={next}"));
    }
    text
}

/// The loops of a list, one line each.
fn list_text(result: &Value) -> String {
    let loops = result
        .get("loops")
        .and_then(Value::as_array)
        .or_else(|| result.as_array())
        .cloned()
        .unwrap_or_default();
    if loops.is_empty() {
        return "[PLAN_LOOPS] none in this scope. Start one on a saved plan with session(action=\"start_loop\", plan_id=\"<id>\").".to_string();
    }
    let mut text = format!(
        "[PLAN_LOOPS] {} loop(s). Read one with session(action=\"loop_status\", loop_id=\"<id>\").",
        loops.len()
    );
    for item in &loops {
        let mut line = format!(
            "\n- loop_id={} status={}",
            text_field(item, &["id", "loop_id"])
                .map(one_line)
                .unwrap_or_else(|| "?".to_string()),
            text_field(item, &["status"])
                .map(one_line)
                .unwrap_or_else(|| "unknown".to_string())
        );
        if let Some(reason) = text_field(item, &["blocked_reason"]) {
            line.push_str(&format!(" blocked_reason={}", one_line(reason)));
        }
        match (
            int_field(item, "iteration"),
            int_field(item, "max_iterations"),
        ) {
            (Some(n), Some(max)) => line.push_str(&format!(" iteration={n}/{max}")),
            (Some(n), None) => line.push_str(&format!(" iteration={n}")),
            _ => {}
        }
        if let Some(plan) = text_field(item, &["plan_id"]) {
            line.push_str(&format!(" plan_id={}", one_line(plan)));
        }
        if let Some(step) = text_field(item, &["expected_step_id"]) {
            line.push_str(&format!(" expected_step={}", one_line(step)));
        }
        let gate = item
            .get("open_gate")
            .and_then(|gate| text_field(gate, &["id", "gate_id"]))
            .or_else(|| text_field(item, &["open_gate_id"]));
        if let Some(gate) = gate {
            line.push_str(&format!(" open_gate={}", one_line(gate)));
        }
        if let Some(updated) = text_field(item, &["updated_at"]) {
            line.push_str(&format!(" updated={}", one_line(updated)));
        }
        if let Some(title) = text_field(item, &["plan_title", "title"]) {
            line.push_str(&format!(" — {}", one_line(title)));
        }
        text.push_str(&line);
    }
    text
}
