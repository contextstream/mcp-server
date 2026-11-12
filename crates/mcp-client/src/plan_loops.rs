//! Plan loops (contract v1.2, with the tickets of v1.3): a saved plan run as
//! a loop that walks the plan's steps and stops at review gates for a person.
//!
//! The ContextStream API is the only place loop state lives. These calls carry
//! one request each, under the client's `/api/v1` base and with the caller's
//! own credential, exactly like the plan calls:
//!
//! - `POST /plans/:plan_id/loop` `{config}` starts a loop; `GET /plans/:plan_id/loop`
//!   reads the plan's latest loop;
//! - `GET /plan-loops?workspace_id&project_id&status&limit` lists loops;
//! - `GET /plan-loops/:loop_id` and `GET /plan-loops/:loop_id/events?after&limit`;
//! - `POST /plan-loops/:loop_id/advance` `{step_id, summary?}`;
//! - `POST /plan-loops/:loop_id/gates/:gate_id/resolve` `{outcome, note, steps?}`;
//! - `POST /plan-loops/:loop_id/abort` `{reason?}` and `POST /plan-loops/:loop_id/resume`
//!   `{max_iterations?}`;
//! - `GET /plan-loops/:loop_id/tickets` lists the tickets riding a loop with
//!   their rollup, `POST /plan-loops/:loop_id/tickets` attaches an existing
//!   ticket (`{ticket_id, step_id?}`) or creates one in the loop's project and
//!   attaches it (`{create, step_id?}`), and
//!   `DELETE /plan-loops/:loop_id/tickets/:ticket_id` detaches one.
//!
//! A transition the API refuses is a 409 whose message starts with its code
//! ([`PlanLoopConflict`]); an invalid config is a 422.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::client::{scope_ids_with_defaults, strip_nulls, ContextStreamClient};
use mcp_types::{Error, Result};

/// `review_policy` values of a loop config.
pub const REVIEW_POLICIES: &[&str] = &["explicit", "every_step", "every_n", "milestone_only"];
/// `type` values of a gate.
pub const GATE_TYPES: &[&str] = &["human_review", "status_report"];
/// `channel` values of a gate.
pub const GATE_CHANNELS: &[&str] = &["coflow", "contextstream", "both"];
/// Outcomes a gate resolves with.
pub const GATE_OUTCOMES: &[&str] = &["approve", "request_changes", "abort"];
/// Loop states, the `status` a list filters on.
pub const LOOP_STATES: &[&str] = &[
    "running",
    "awaiting_review",
    "blocked",
    "completed",
    "aborted",
];

/// One review gate of a loop config. Unknown fields are refused, so a typo
/// (`reviewers` for `reviewer_roles`) is said instead of dropped.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanLoopGate {
    pub step_id: String,
    #[serde(
        rename = "type",
        alias = "gate_type",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub gate_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer_roles: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reviewer_user_ids: Option<Vec<Uuid>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sla_hours: Option<i64>,
}

/// The `config` of `POST /plans/:plan_id/loop`. Fields left out take the
/// API's defaults (5 iterations, `explicit` review, no cadence report,
/// audience `engineering`).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct PlanLoopConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_iterations: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_policy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub review_every_n: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gates: Option<Vec<PlanLoopGate>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report_cadence_minutes: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audience: Option<Vec<String>>,
    /// Where the loop's own events are announced: one of [`GATE_CHANNELS`].
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
}

/// The body of `POST /plan-loops/:loop_id/gates/:gate_id/resolve`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GateResolution {
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// The step ids a `request_changes` affects.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steps: Option<Vec<String>>,
}

/// A ticket to create in the loop's project and attach (the `create` of
/// `POST /plan-loops/:loop_id/tickets`). Unknown fields are refused, so a
/// typo (`labels` as `tags`) is said instead of dropped.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewLoopTicket {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// bug|feature|task|chore|incident|epic; the API's default when left out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<Vec<String>>,
    /// `[{entity_type: human|agent, user_id?, email?, handle?, name?, role?}]`,
    /// as [`crate::normalize_assignees`] writes them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignees: Option<Value>,
}

/// The body of `POST /plan-loops/:loop_id/tickets`: an existing ticket of
/// the loop's workspace, or one to create, on the loop or on one of its
/// steps. Attaching an attached ticket with another `step_id` moves it.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct LoopTicketAttachment {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ticket_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub create: Option<NewLoopTicket>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step_id: Option<String>,
}

/// The codes a refused loop transition (409) starts its message with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanLoopConflict {
    /// The plan already has a loop that is not completed or aborted.
    LoopAlreadyRunning,
    /// The step advanced is not the step the loop expects.
    StepOutOfOrder,
    /// The plan's steps changed since the loop took them; the loop is blocked.
    PlanVersionConflict,
    /// The gate was already resolved with another outcome.
    GateAlreadyResolved,
    /// The loop's state does not allow this action (a finished loop, a gate
    /// that is not open, a resume of a loop that is not blocked, a ticket
    /// attached to an ended loop).
    IllegalTransition,
    /// The ticket already rides another live loop; the message names it
    /// (`ticket_in_another_loop: <loop_id>`).
    TicketInAnotherLoop,
    /// The step advanced still has tickets that are not done
    /// (`step_has_open_tickets: 2 of 3 tickets on this step are not done`).
    StepHasOpenTickets,
}

impl PlanLoopConflict {
    pub const ALL: [Self; 7] = [
        Self::LoopAlreadyRunning,
        Self::StepOutOfOrder,
        Self::PlanVersionConflict,
        Self::GateAlreadyResolved,
        Self::IllegalTransition,
        Self::TicketInAnotherLoop,
        Self::StepHasOpenTickets,
    ];

    /// The code as the API writes it.
    pub const fn code(self) -> &'static str {
        match self {
            Self::LoopAlreadyRunning => "loop_already_running",
            Self::StepOutOfOrder => "step_out_of_order",
            Self::PlanVersionConflict => "plan_version_conflict",
            Self::GateAlreadyResolved => "gate_already_resolved",
            Self::IllegalTransition => "illegal_transition",
            Self::TicketInAnotherLoop => "ticket_in_another_loop",
            Self::StepHasOpenTickets => "step_has_open_tickets",
        }
    }

    /// The conflict a 409 from a loop route names, if any. The message starts
    /// with the code; one the API wraps in more words is still found.
    pub fn from_error(error: &Error) -> Option<Self> {
        let Error::Http {
            status: 409,
            message,
            ..
        } = error
        else {
            return None;
        };
        let message = message.trim_start();
        Self::ALL
            .into_iter()
            .find(|conflict| message.starts_with(conflict.code()))
            .or_else(|| {
                Self::ALL
                    .into_iter()
                    .find(|conflict| message.contains(conflict.code()))
            })
    }
}

fn plan_loop_path(loop_id: Uuid, rest: &str) -> String {
    format!("/plan-loops/{loop_id}{rest}")
}

impl ContextStreamClient {
    /// Start a loop on a plan. Endpoint: `POST /plans/:plan_id/loop` (201).
    pub async fn start_plan_loop(&self, plan_id: Uuid, config: &PlanLoopConfig) -> Result<Value> {
        self.post(
            &format!("/plans/{plan_id}/loop"),
            serde_json::json!({ "config": config }),
        )
        .await
    }

    /// The plan's latest loop. Endpoint: `GET /plans/:plan_id/loop`.
    pub async fn plan_loop_of_plan(&self, plan_id: Uuid) -> Result<Value> {
        self.get(&format!("/plans/{plan_id}/loop")).await
    }

    /// Loops the caller can read, in the session's scope unless one is
    /// given. Endpoint: `GET /plan-loops?workspace_id&project_id&status&limit`.
    pub async fn list_plan_loops(
        &self,
        workspace_id: Option<Uuid>,
        project_id: Option<Uuid>,
        status: Option<&str>,
        limit: Option<i64>,
    ) -> Result<Value> {
        let config = self.config().await;
        let (workspace_id, project_id) = scope_ids_with_defaults(workspace_id, project_id, &config);
        let mut params = Vec::new();
        if let Some(id) = workspace_id {
            params.push(format!("workspace_id={id}"));
        }
        if let Some(id) = project_id {
            params.push(format!("project_id={id}"));
        }
        if let Some(status) = status.map(str::trim).filter(|value| !value.is_empty()) {
            params.push(format!("status={}", urlencoding::encode(status)));
        }
        if let Some(limit) = limit.filter(|value| *value > 0) {
            params.push(format!("limit={limit}"));
        }
        let path = if params.is_empty() {
            "/plan-loops".to_string()
        } else {
            format!("/plan-loops?{}", params.join("&"))
        };
        self.get(&path).await
    }

    /// One loop. Endpoint: `GET /plan-loops/:loop_id`.
    pub async fn get_plan_loop(&self, loop_id: Uuid) -> Result<Value> {
        self.get(&plan_loop_path(loop_id, "")).await
    }

    /// A loop's events after a cursor (`next_after` of the previous page).
    /// Endpoint: `GET /plan-loops/:loop_id/events?after&limit`.
    pub async fn plan_loop_events(
        &self,
        loop_id: Uuid,
        after: Option<&str>,
        limit: Option<i64>,
    ) -> Result<Value> {
        let mut params = Vec::new();
        if let Some(after) = after.map(str::trim).filter(|value| !value.is_empty()) {
            params.push(format!("after={}", urlencoding::encode(after)));
        }
        if let Some(limit) = limit.filter(|value| *value > 0) {
            params.push(format!("limit={limit}"));
        }
        let rest = if params.is_empty() {
            "/events".to_string()
        } else {
            format!("/events?{}", params.join("&"))
        };
        self.get(&plan_loop_path(loop_id, &rest)).await
    }

    /// Mark the expected step done. Endpoint: `POST /plan-loops/:loop_id/advance`.
    pub async fn advance_plan_loop(
        &self,
        loop_id: Uuid,
        step_id: &str,
        summary: Option<&str>,
    ) -> Result<Value> {
        let body = strip_nulls(serde_json::json!({
            "step_id": step_id,
            "summary": summary,
        }));
        self.post(&plan_loop_path(loop_id, "/advance"), body).await
    }

    /// Resolve an open gate.
    /// Endpoint: `POST /plan-loops/:loop_id/gates/:gate_id/resolve`.
    pub async fn resolve_plan_loop_gate(
        &self,
        loop_id: Uuid,
        gate_id: &str,
        resolution: &GateResolution,
    ) -> Result<Value> {
        let rest = format!("/gates/{}/resolve", urlencoding::encode(gate_id));
        self.post(&plan_loop_path(loop_id, &rest), resolution).await
    }

    /// Abort a loop. Endpoint: `POST /plan-loops/:loop_id/abort`.
    pub async fn abort_plan_loop(&self, loop_id: Uuid, reason: Option<&str>) -> Result<Value> {
        let body = strip_nulls(serde_json::json!({ "reason": reason }));
        self.post(&plan_loop_path(loop_id, "/abort"), body).await
    }

    /// Resume a blocked loop. Endpoint: `POST /plan-loops/:loop_id/resume`.
    pub async fn resume_plan_loop(
        &self,
        loop_id: Uuid,
        max_iterations: Option<i64>,
    ) -> Result<Value> {
        let body = strip_nulls(serde_json::json!({ "max_iterations": max_iterations }));
        self.post(&plan_loop_path(loop_id, "/resume"), body).await
    }

    /// The tickets riding a loop, with their rollup.
    /// Endpoint: `GET /plan-loops/:loop_id/tickets`, answered `{tickets, rollup}`.
    pub async fn plan_loop_tickets(&self, loop_id: Uuid) -> Result<Value> {
        self.get(&plan_loop_path(loop_id, "/tickets")).await
    }

    /// Attach a ticket to a loop, or create one there and attach it.
    /// Endpoint: `POST /plan-loops/:loop_id/tickets` (201), answered
    /// `{ticket, loop}`.
    pub async fn attach_plan_loop_ticket(
        &self,
        loop_id: Uuid,
        attachment: &LoopTicketAttachment,
    ) -> Result<Value> {
        self.post(&plan_loop_path(loop_id, "/tickets"), attachment)
            .await
    }

    /// Take a ticket off a loop; the ticket itself is untouched.
    /// Endpoint: `DELETE /plan-loops/:loop_id/tickets/:ticket_id`, answered
    /// `{loop}`.
    pub async fn detach_plan_loop_ticket(&self, loop_id: Uuid, ticket_id: Uuid) -> Result<Value> {
        self.delete(&plan_loop_path(loop_id, &format!("/tickets/{ticket_id}")))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_conflict_is_read_from_the_code_its_message_starts_with() {
        for conflict in PlanLoopConflict::ALL {
            let error = Error::http(409, format!("{}: the API's words", conflict.code()));
            assert_eq!(PlanLoopConflict::from_error(&error), Some(conflict));
        }
        let wrapped = Error::http(409, "refused (plan_version_conflict): plan changed");
        assert_eq!(
            PlanLoopConflict::from_error(&wrapped),
            Some(PlanLoopConflict::PlanVersionConflict)
        );
        // The two ticket codes of contract v1.3, as the API words them.
        assert_eq!(
            PlanLoopConflict::from_error(&Error::http(
                409,
                format!("ticket_in_another_loop: {}", Uuid::new_v4())
            )),
            Some(PlanLoopConflict::TicketInAnotherLoop)
        );
        assert_eq!(
            PlanLoopConflict::from_error(&Error::http(
                409,
                "step_has_open_tickets: 2 of 3 tickets on this step are not done"
            )),
            Some(PlanLoopConflict::StepHasOpenTickets)
        );
        // Another status, or a 409 that names no loop code, is not one.
        assert_eq!(
            PlanLoopConflict::from_error(&Error::http(422, "step_out_of_order")),
            None
        );
        assert_eq!(
            PlanLoopConflict::from_error(&Error::http(409, "Idempotency-Key is still in progress")),
            None
        );
        assert_eq!(
            PlanLoopConflict::from_error(&Error::Network("down".into())),
            None
        );
    }

    #[test]
    fn a_config_sends_only_what_was_given_in_the_contract_names() {
        assert_eq!(
            serde_json::to_value(PlanLoopConfig::default()).unwrap(),
            serde_json::json!({})
        );
        let gate: PlanLoopGate = serde_json::from_value(serde_json::json!({
            "step_id": "plan-step-2", "gate_type": "human_review", "reviewer_roles": ["product"]
        }))
        .unwrap();
        let config = PlanLoopConfig {
            max_iterations: Some(3),
            review_policy: Some("explicit".into()),
            gates: Some(vec![gate]),
            report_cadence_minutes: Some(0),
            ..PlanLoopConfig::default()
        };
        assert_eq!(
            serde_json::to_value(config).unwrap(),
            serde_json::json!({
                "max_iterations": 3,
                "review_policy": "explicit",
                "gates": [{"step_id": "plan-step-2", "type": "human_review", "reviewer_roles": ["product"]}],
                "report_cadence_minutes": 0,
            })
        );
        let typo = serde_json::from_value::<PlanLoopGate>(serde_json::json!({
            "step_id": "plan-step-2", "reviewers": ["product"]
        }));
        assert!(typo.unwrap_err().to_string().contains("reviewers"));
    }

    #[test]
    fn an_attachment_is_an_existing_ticket_or_one_to_create() {
        let ticket = Uuid::new_v4();
        assert_eq!(
            serde_json::to_value(LoopTicketAttachment {
                ticket_id: Some(ticket),
                step_id: Some("plan-step-2".into()),
                ..LoopTicketAttachment::default()
            })
            .unwrap(),
            serde_json::json!({"ticket_id": ticket, "step_id": "plan-step-2"})
        );
        let create: NewLoopTicket = serde_json::from_value(serde_json::json!({
            "title": "Gate engine", "kind": "task",
            "assignees": [{"entity_type": "agent", "handle": "cinder", "name": "Cinder"}],
        }))
        .unwrap();
        assert_eq!(
            serde_json::to_value(LoopTicketAttachment {
                create: Some(create),
                ..LoopTicketAttachment::default()
            })
            .unwrap(),
            serde_json::json!({"create": {
                "title": "Gate engine", "kind": "task",
                "assignees": [{"entity_type": "agent", "handle": "cinder", "name": "Cinder"}],
            }})
        );
        let typo = serde_json::from_value::<NewLoopTicket>(serde_json::json!({
            "title": "Gate engine", "tags": ["loop"]
        }));
        assert!(typo.unwrap_err().to_string().contains("tags"));
    }
}
