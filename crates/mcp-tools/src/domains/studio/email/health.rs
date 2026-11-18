//! Whether the numbers can be trusted: `email_diagnostics` (Resend's webhook, and one plain
//! verdict on whether its events are arriving) and the unsubscribe report
//! (`email_unsubscribes` with operation `report`), which ends with that verdict.
//!
//! Unsubscribes are recorded by ContextStream's own unsubscribe link and one-click route, so
//! they are counted whether or not Resend's events arrive. Opens, clicks, bounces and
//! complaints arrive through the webhook: when it is silent, those may be missing.

use mcp_client::studio::StudioAnswer;
use mcp_types::{tool::ToolResult, Error, Result};
use serde_json::{json, Value};
use uuid::Uuid;

use super::{email_refusal, moment, number, percent, said, text, Topic, CONNECT_URL, NOT_ANSWERED};
use crate::domains::studio::words::credits;
use crate::domains::studio::{StudioInput, StudioTool};

const DIAGNOSTICS_DAYS_DEFAULT: i64 = 7;
/// Create keeps 30 days of webhook deliveries.
const DIAGNOSTICS_DAYS_MAX: i64 = 30;
const REPORT_DAYS_DEFAULT: i64 = 30;
/// What Resend's events can be missing of; unsubscribes aren't among them.
const MAY_BE_MISSING: &str = "opens, clicks, bounces and complaints may be missing";
/// Resend's events, in the order they are said.
const EVENT_TYPES: [(&str, &str); 9] = [
    ("sent", "sent"),
    ("delivered", "delivered"),
    ("delivery_delayed", "delayed"),
    ("opened", "opened"),
    ("clicked", "clicked"),
    ("bounced", "bounced"),
    ("complained", "complained"),
    ("failed", "failed"),
    ("suppressed", "suppressed"),
];
/// Why people said they left, as Create keeps it, and in words.
const FEEDBACK: [(&str, &str); 5] = [
    ("too_many", "too many emails"),
    ("not_relevant", "not relevant"),
    ("never_signed_up", "never signed up"),
    ("other", "another reason"),
    ("none", "no reason given"),
];
/// How many of a campaign's unsubscribes are read for why people left.
const FEEDBACK_READ: usize = 50;

/// Whether Resend's events are arriving, in one sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Verdict {
    /// `arriving`, `partial`, `missing`, `idle`, `not_set_up`, or `not_answered`.
    pub status: String,
    pub sentence: String,
}

impl Verdict {
    fn new(status: &str, sentence: String) -> Self {
        Self {
            status: status.to_string(),
            sentence,
        }
    }

    pub(super) fn json(&self) -> Value {
        json!({"status": self.status, "sentence": self.sentence})
    }

    /// A verdict Create gave itself (`{status, sentence}`), when it gave one.
    pub(super) fn given(value: Option<&Value>) -> Option<Self> {
        let value = value?;
        Some(Self::new(
            &text(value, "status").unwrap_or_else(|| "not_answered".to_string()),
            text(value, "sentence")?,
        ))
    }

    /// The verdict of a diagnostics answer over `days`: Create's own when it gives one,
    /// else read from how well events cover what was sent. A status this tool doesn't know
    /// is never read as good news.
    pub(super) fn of(diagnostics: &Value, days: i64) -> Self {
        if let Some(given) = Self::given(diagnostics.get("verdict")) {
            return given;
        }
        if diagnostics.get("secretSet").and_then(Value::as_bool) == Some(false) {
            return Self::new(
                "not_set_up",
                "events are possibly missing: the webhook has no signing secret, so no event can be verified.".to_string(),
            );
        }
        let sent = diagnostics
            .pointer("/coverage/sent")
            .and_then(Value::as_i64);
        let applied = diagnostics
            .pointer("/outcomes/applied")
            .and_then(Value::as_i64);
        let last = moment(diagnostics.get("lastAppliedAt").and_then(Value::as_str));
        let status = diagnostics
            .pointer("/coverage/status")
            .and_then(Value::as_str)
            .unwrap_or("");
        match (status, sent, applied) {
            ("healthy", Some(sent), Some(applied)) => Self::new(
                "arriving",
                format!(
                    "events are arriving: {} applied for {} emails sent in the last {days} days{}.",
                    credits(applied),
                    credits(sent),
                    last.map_or(String::new(), |at| format!(", the last on {at}"))
                ),
            ),
            ("idle", _, _) => Self::new(
                "idle",
                format!(
                    "nothing was sent in the last {days} days, so there is nothing to compare; {}.",
                    last.map_or_else(
                        || "no event has been applied yet".to_string(),
                        |at| format!("the last event was applied on {at}")
                    )
                ),
            ),
            ("low", Some(sent), Some(applied)) => Self::new(
                "partial",
                format!(
                    "events may be missing: {} arrived for {} emails sent in the last {days} days, fewer than one an email{}: some {MAY_BE_MISSING}.",
                    credits(applied),
                    credits(sent),
                    last.map_or(String::new(), |at| format!(" (the last on {at})"))
                ),
            ),
            ("missing", Some(sent), Some(applied)) => Self::new(
                "missing",
                format!(
                    "events are possibly missing: {} arrived in the last {days} days although {} emails were sent ({}): {MAY_BE_MISSING}.",
                    if applied == 0 {
                        "none".to_string()
                    } else {
                        format!("only {}", credits(applied))
                    },
                    credits(sent),
                    last.map_or_else(
                        || "none has been applied yet".to_string(),
                        |at| format!("the last one arrived on {at}")
                    )
                ),
            ),
            _ => Self::new("not_answered", format!("{NOT_ANSWERED}.")),
        }
    }
}

/// How the webhook is set up and what Resend says of it, in one line.
fn setup_line(diagnostics: &Value) -> String {
    let by = match diagnostics.get("mode").and_then(Value::as_str) {
        Some("auto") => "Set up by Create".to_string(),
        Some(_) => "Set up by hand".to_string(),
        None => format!("How it was set up is {NOT_ANSWERED}"),
    };
    let secret = match diagnostics.get("secretSet").and_then(Value::as_bool) {
        Some(true) => ", with its signing secret".to_string(),
        Some(false) => {
            format!(", with no signing secret (the team pastes it in the studio: {CONNECT_URL})")
        }
        None => format!(", and whether its signing secret is set is {NOT_ANSWERED}"),
    };
    let resend = diagnostics.get("resend").cloned().unwrap_or(Value::Null);
    let problem = text(&resend, "problem");
    let flag = |key: &str| resend.get(key).and_then(Value::as_bool) == Some(true);
    let in_resend = match (resend.get("checked").and_then(Value::as_bool), problem) {
        (Some(true), None) if flag("found") && flag("endpointMatches") && flag("enabled") => {
            "; in Resend it is registered and enabled, with every event.".to_string()
        }
        (Some(true), Some(problem)) => format!("; in Resend: {problem}"),
        (_, Some(problem)) => format!("; whether Resend has it is {NOT_ANSWERED}: {problem}"),
        _ => format!("; whether Resend has it is {NOT_ANSWERED}."),
    };
    format!("- {by}{secret}{in_resend}")
}

/// An event's type without Resend's prefix: `email.opened` is "opened".
fn event_word(kind: &str) -> String {
    let kind = kind.strip_prefix("email.").unwrap_or(kind);
    EVENT_TYPES
        .iter()
        .find(|(key, _)| *key == kind)
        .map_or_else(|| kind.replace('_', " "), |(_, word)| (*word).to_string())
}

/// Events by type: Create's own counts over the window, else counted over the deliveries
/// it listed, and said as such.
fn by_type_line(diagnostics: &Value) -> Option<String> {
    if let Some(events) = diagnostics.get("events").filter(|e| e.is_object()) {
        let parts: Vec<String> = EVENT_TYPES
            .iter()
            .filter_map(|(key, word)| {
                number(events, key)
                    .filter(|n| *n > 0)
                    .map(|n| format!("{word} {}", credits(n)))
            })
            .collect();
        return Some(
            if parts.is_empty()
                && EVENT_TYPES
                    .iter()
                    .any(|(key, _)| number(events, key).is_none())
            {
                format!("- By type: {NOT_ANSWERED}.")
            } else if parts.is_empty() {
                "- By type: no event was applied.".to_string()
            } else {
                format!("- By type: {}.", parts.join(", "))
            },
        );
    }
    let recent = diagnostics.get("recent")?.as_array()?;
    if recent.is_empty() {
        return None;
    }
    let mut counts: Vec<(String, i64)> = Vec::new();
    for delivery in recent {
        let Some(kind) = text(delivery, "type") else {
            continue;
        };
        let word = event_word(&kind);
        match counts.iter_mut().find(|(known, _)| *known == word) {
            Some((_, n)) => *n += 1,
            None => counts.push((word, 1)),
        }
    }
    let order = |word: &str| {
        EVENT_TYPES
            .iter()
            .position(|(_, known)| *known == word)
            .unwrap_or(EVENT_TYPES.len())
    };
    counts.sort_by(|a, b| order(&a.0).cmp(&order(&b.0)).then_with(|| a.0.cmp(&b.0)));
    Some(format!(
        "- By type, over the last {} deliveries read: {}.",
        recent.len(),
        counts
            .iter()
            .map(|(word, n)| format!("{word} {}", credits(*n)))
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

/// Why people said they left, from Create's own tally (`{too_many: 2, ...}`).
fn feedback_line(feedback: &Value) -> Option<String> {
    let parts: Vec<String> = FEEDBACK
        .iter()
        .filter_map(|(key, words)| {
            number(feedback, key)
                .filter(|n| *n > 0)
                .map(|n| format!("{words} {}", credits(n)))
        })
        .collect();
    (!parts.is_empty()).then(|| format!("- Why they left: {}.", parts.join(", ")))
}

/// "3 unsubscribed, 1 spam complaint".
fn campaign_words(campaign: &Value) -> String {
    let mut parts = Vec::new();
    for (key, one, many) in [
        ("unsubscribed", "unsubscribed", "unsubscribed"),
        ("bounced", "bounced", "bounced"),
        ("complained", "spam complaint", "spam complaints"),
    ] {
        if let Some(n) = number(campaign, key).filter(|n| *n > 0) {
            parts.push(format!(
                "{} {}",
                credits(n),
                if n == 1 { one } else { many }
            ));
        }
    }
    let mut words = if parts.is_empty() {
        "no opt-outs".to_string()
    } else {
        parts.join(", ")
    };
    if let Some(sent) = number(campaign, "sent") {
        words.push_str(&format!(", of {} sent", credits(sent)));
        if let Some(rate) = campaign
            .get("rate")
            .and_then(Value::as_f64)
            .filter(|_| sent > 0)
        {
            words.push_str(&format!(" ({})", percent(rate)));
        }
    }
    words
}

impl StudioTool {
    /// Create's webhook diagnostics over `days`, or why they can't be read.
    async fn read_diagnostics(
        &self,
        workspace_id: Uuid,
        days: i64,
    ) -> Result<std::result::Result<Value, String>> {
        let answer = self
            .email_call(
                "GET",
                &format!("/webhook/diagnostics?workspaceId={workspace_id}&days={days}"),
                None,
                None,
            )
            .await?;
        Ok(if answer.is_success() {
            Ok(answer.data)
        } else if answer.status == 404
            && answer
                .error_message()
                .is_some_and(|said| said.contains("isn't connected"))
        {
            Err(format!(
                "Resend isn't connected yet, so there is no webhook and no event can arrive: the team connects it in the studio ({CONNECT_URL})."
            ))
        } else {
            Err(email_refusal(&answer, Topic::Other))
        })
    }

    // ------------------------------------------------------------------------------------
    // email_diagnostics
    // ------------------------------------------------------------------------------------

    pub(super) async fn email_diagnostics(
        &self,
        workspace_id: Uuid,
        input: &StudioInput,
    ) -> Result<ToolResult> {
        let days = input.days.unwrap_or(DIAGNOSTICS_DAYS_DEFAULT);
        if !(1..=DIAGNOSTICS_DAYS_MAX).contains(&days) {
            return Err(Error::Validation(format!(
                "days is 1 to {DIAGNOSTICS_DAYS_MAX} for email_diagnostics: Create keeps {DIAGNOSTICS_DAYS_MAX} days of webhook deliveries"
            )));
        }
        let diagnostics = match self.read_diagnostics(workspace_id, days).await? {
            Ok(diagnostics) => diagnostics,
            Err(refused) => return Ok(ToolResult::error(refused)),
        };
        let verdict = Verdict::of(&diagnostics, days);
        let outcomes = diagnostics.get("outcomes").cloned().unwrap_or(Value::Null);
        let n = |key: &str| said(number(&outcomes, key));
        let mut lines = vec![
            format!("The webhook over the last {days} days, as of now."),
            format!("Verdict: {}", verdict.sentence),
            setup_line(&diagnostics),
        ];
        let last_event = diagnostics.get("lastEvent").filter(|e| e.is_object());
        lines.push(match last_event {
            Some(event) => format!(
                "- Last event: {}, on {}.",
                text(event, "type")
                    .map_or_else(|| NOT_ANSWERED.to_string(), |kind| event_word(&kind)),
                moment(event.get("at").and_then(Value::as_str))
                    .unwrap_or_else(|| NOT_ANSWERED.to_string())
            ),
            None => match (
                moment(diagnostics.get("lastDeliveryAt").and_then(Value::as_str)),
                moment(diagnostics.get("lastAppliedAt").and_then(Value::as_str)),
            ) {
                (Some(delivery), Some(applied)) => format!(
                    "- Last delivery on {delivery}; the last one applied to a send on {applied}."
                ),
                (Some(delivery), None) => {
                    format!("- Last delivery on {delivery}; none has been applied to a send yet.")
                }
                (None, _) if diagnostics.get("lastDeliveryAt") == Some(&Value::Null) => {
                    "- No delivery has arrived yet.".to_string()
                }
                (None, _) => format!("- Last delivery: {NOT_ANSWERED}."),
            },
        });
        lines.push(format!(
            "- Deliveries: {} applied, {} repeats, {} for other mail on this Resend account, {} test.",
            n("applied"),
            n("duplicate"),
            n("not_ours"),
            n("test")
        ));
        lines.push(format!(
            "- Refused: {} with a bad signature, {} too late, {} malformed, {} while no signing secret was set.",
            n("refused_signature"),
            n("refused_late"),
            n("malformed"),
            n("no_secret")
        ));
        lines.extend(by_type_line(&diagnostics));
        Ok(ToolResult::with_structured(
            lines.join("\n"),
            json!({
                "workspace_id": workspace_id,
                "days": days,
                "verdict": verdict.json(),
                "mode": diagnostics.get("mode"),
                "secret_set": diagnostics.get("secretSet"),
                "last_delivery_at": diagnostics.get("lastDeliveryAt"),
                "last_applied_at": diagnostics.get("lastAppliedAt"),
                "last_event": diagnostics.get("lastEvent"),
                "outcomes": outcomes,
                "coverage": diagnostics.get("coverage"),
                "events": diagnostics.get("events"),
                "resend": diagnostics.get("resend"),
            }),
        ))
    }

    // ------------------------------------------------------------------------------------
    // email_unsubscribes: report
    // ------------------------------------------------------------------------------------

    /// Opt-outs, bounces and complaints as separate numbers, why people left, and whether
    /// the numbers can be trusted.
    pub(super) async fn unsubscribe_report(
        &self,
        workspace_id: Uuid,
        input: &StudioInput,
    ) -> Result<ToolResult> {
        let days = input.days.unwrap_or(REPORT_DAYS_DEFAULT).clamp(1, 365);
        let campaign = match super::given(input.campaign_id.as_deref()) {
            Some(_) => {
                let id = Self::campaign_id(input, "email_unsubscribes with operation report")?;
                match self.read_campaign(workspace_id, id).await? {
                    Ok(campaign) => Some((id, campaign)),
                    Err(refused) => return Ok(ToolResult::error(refused)),
                }
            }
            None => None,
        };
        let mut path = format!("/suppressions/summary?workspaceId={workspace_id}&days={days}");
        if let Some((id, _)) = &campaign {
            path.push_str(&format!("&campaignId={id}"));
        }
        let answer = self.email_call("GET", &path, None, None).await?;
        if !answer.is_success() {
            return Ok(ToolResult::error(email_refusal(&answer, Topic::Other)));
        }
        let summary = answer.data;
        // A Create that narrows the summary to a campaign also says when the last opt-out
        // was; one that doesn't ignores `campaignId`, and its summary is the workspace's.
        let narrowed = match (&campaign, summary.get("campaignId")) {
            (Some((id, _)), Some(echo)) => echo.as_str() == Some(&id.to_string()),
            _ => summary.get("lastOptOutAt").is_some(),
        };
        let mut lines = Vec::new();
        let by_reason;
        let mut feedback = None;
        let mut sent = number(&summary, "sent");
        match &campaign {
            Some((id, row)) if !narrowed => {
                let name = text(row, "name").unwrap_or_else(|| "the campaign".to_string());
                let report = self
                    .email_call(
                        "GET",
                        &format!("/campaigns/{id}/report?workspaceId={workspace_id}"),
                        None,
                        None,
                    )
                    .await?;
                if !report.is_success() {
                    return Ok(ToolResult::error(email_refusal(&report, Topic::Campaign)));
                }
                let totals = report.data.get("totals").cloned().unwrap_or(Value::Null);
                sent = number(&totals, "sent");
                by_reason = json!({
                    "unsubscribed": totals.get("unsubscribed"),
                    "bounced": totals.get("bounced"),
                    "complained": totals.get("complained"),
                });
                lines.push(format!("Opt-outs from “{name}”, as of now:"));
                lines.push(format!(
                    "- Unsubscribed {} · bounced {} · spam complaints {}{}.",
                    said(number(&totals, "unsubscribed")),
                    said(number(&totals, "bounced")),
                    said(number(&totals, "complained")),
                    sent.map_or(String::new(), |sent| format!(", of {} sent", credits(sent)))
                ));
                // Why they left, from the campaign's own unsubscribes.
                let listed = self
                    .email_call(
                        "GET",
                        &format!(
                            "/suppressions?workspaceId={workspace_id}&campaignId={id}&reason=unsubscribed&limit={FEEDBACK_READ}"
                        ),
                        None,
                        None,
                    )
                    .await
                    .ok()
                    .filter(StudioAnswer::is_success)
                    .map(|answer| answer.data);
                if let Some(listed) = listed {
                    let page = listed
                        .get("suppressions")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    let rows: Vec<&Value> = page
                        .iter()
                        .filter(|row| text(row, "reason").as_deref() == Some("unsubscribed"))
                        .collect();
                    let mut tally = json!({});
                    for (key, _) in FEEDBACK {
                        let n = rows
                            .iter()
                            .filter(|row| {
                                row.pointer("/feedback/reason")
                                    .and_then(Value::as_str)
                                    .unwrap_or("none")
                                    == key
                            })
                            .count();
                        tally[key] = json!(n);
                    }
                    if let Some(mut line) = feedback_line(&tally) {
                        // Only part of them was read: say over how many.
                        let total = number(&listed, "total").unwrap_or(page.len() as i64);
                        if total > page.len() as i64 {
                            line = format!(
                                "{} (of the first {} of {}).",
                                line.trim_end_matches('.'),
                                page.len(),
                                credits(total)
                            );
                        }
                        lines.push(line);
                    }
                    feedback = Some(tally);
                }
            }
            _ => {
                by_reason = summary.get("byReason").cloned().unwrap_or(Value::Null);
                let from = campaign
                    .as_ref()
                    .and_then(|(_, row)| text(row, "name"))
                    .map_or(String::new(), |name| format!(" from “{name}”"));
                lines.push(format!(
                    "Opt-outs{from} over the last {days} days, as of now:"
                ));
                let mut numbers = format!(
                    "- Unsubscribed {} · bounced {} · spam complaints {} · added by hand {}",
                    said(number(&by_reason, "unsubscribed")),
                    said(number(&by_reason, "bounced")),
                    said(number(&by_reason, "complained")),
                    said(number(&by_reason, "manual"))
                );
                if let Some(sent) = sent {
                    numbers.push_str(&format!(", of {} sent", credits(sent)));
                }
                if let Some(at) = moment(summary.get("lastOptOutAt").and_then(Value::as_str)) {
                    numbers.push_str(&format!("; the last on {at}"));
                }
                // Resubscribes are the workspace's, so they are said with its report only.
                if let Some(back) = number(&summary, "resubscribed").filter(|_| campaign.is_none())
                {
                    numbers.push_str(&format!("; {} resubscribed", credits(back)));
                }
                lines.push(format!("{numbers}."));
                feedback = summary.get("feedback").filter(|f| f.is_object()).cloned();
                lines.extend(feedback.as_ref().and_then(feedback_line));
                let campaigns: Vec<String> = summary
                    .get("campaigns")
                    .and_then(Value::as_array)
                    .filter(|_| campaign.is_none())
                    .map(|rows| {
                        rows.iter()
                            .map(|row| {
                                format!(
                                    "“{}” {}",
                                    text(row, "name").unwrap_or_else(|| "a campaign".to_string()),
                                    campaign_words(row)
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                if !campaigns.is_empty() {
                    lines.push(format!("- By campaign: {}.", campaigns.join("; ")));
                }
            }
        }
        // Whether to trust them: Create's own verdict with the summary, else the webhook's
        // diagnostics over the days it keeps.
        let verdict = match Verdict::given(summary.get("webhook")) {
            Some(verdict) => verdict,
            None => {
                let window = days.min(DIAGNOSTICS_DAYS_MAX);
                match self.read_diagnostics(workspace_id, window).await {
                    Ok(Ok(diagnostics)) => Verdict::of(&diagnostics, window),
                    Ok(Err(refused)) => Verdict::new("not_answered", refused),
                    Err(_) => Verdict::new(
                        "not_answered",
                        format!("whether its events are arriving is {NOT_ANSWERED}."),
                    ),
                }
            }
        };
        lines.push(format!(
            "- Can they be trusted? Unsubscribes are recorded by ContextStream's own unsubscribe link, so they don't depend on Resend's events. Bounces and complaints arrive through Resend's webhook: {}",
            verdict.sentence
        ));
        Ok(ToolResult::with_structured(
            lines.join("\n"),
            json!({
                "workspace_id": workspace_id,
                "days": days,
                "campaign_id": campaign.as_ref().map(|(id, _)| id),
                "by_reason": by_reason,
                "sent": sent,
                "feedback": feedback,
                "campaigns": summary.get("campaigns").filter(|_| campaign.is_none()),
                "resubscribed": summary.get("resubscribed").filter(|_| campaign.is_none()),
                "last_opt_out_at": summary.get("lastOptOutAt"),
                "verdict": verdict.json(),
            }),
        ))
    }
}

// ---------------------------------------------------------------------------------------
// email_connection
// ---------------------------------------------------------------------------------------

const CONNECTION_OPERATIONS: &[&str] = &["show", "refresh"];

/// The sender as campaigns go out: "Northwind <hello@…>, replies to team@…".
fn sender_words(sender: &Value) -> String {
    let from = match (text(sender, "fromName"), text(sender, "fromEmail")) {
        (Some(name), Some(email)) => format!("{name} <{email}>"),
        (None, Some(email)) => email,
        _ => return "no from address yet".to_string(),
    };
    match text(sender, "replyTo") {
        Some(reply_to) => format!("{from}, replies to {reply_to}"),
        None => from,
    }
}

/// A sending domain's state: "news.acme.com verified (opens tracked, clicks not tracked)".
fn domain_words(domain: &Value) -> String {
    let name = text(domain, "name").unwrap_or_else(|| "a domain".to_string());
    let status = text(domain, "status").unwrap_or_else(|| NOT_ANSWERED.to_string());
    if status != "verified" {
        return format!("{name} {}", status.replace('_', " "));
    }
    let tracked = |key: &str, what: &str| match domain.get(key).and_then(Value::as_bool) {
        Some(true) => format!("{what} tracked"),
        Some(false) => format!("{what} not tracked"),
        None => format!("{what} tracking {NOT_ANSWERED}"),
    };
    format!(
        "{name} verified ({}, {})",
        tracked("openTracking", "opens"),
        tracked("clickTracking", "clicks")
    )
}

impl StudioTool {
    /// The connection to the team's Resend: what it sends as, its domains, its webhook and
    /// what is missing. `refresh` has Create ask Resend again. Connecting stays in the
    /// studio: no key is taken here, and none is shown.
    pub(super) async fn email_connection(
        &self,
        workspace_id: Uuid,
        input: &StudioInput,
    ) -> Result<ToolResult> {
        let operation = super::operation_or(
            input.operation.as_deref(),
            CONNECTION_OPERATIONS,
            "show",
            "email_connection",
        )?;
        let refresh = operation == "refresh";
        let answer = if refresh {
            self.email_call(
                "POST",
                "/connection/refresh",
                Some(&json!({"workspaceId": workspace_id})),
                None,
            )
            .await?
        } else {
            self.email_call(
                "GET",
                &format!("/connection?workspaceId={workspace_id}"),
                None,
                None,
            )
            .await?
        };
        if !answer.is_success() {
            let unconnected = answer.status == 404
                && answer
                    .error_message()
                    .is_some_and(|said| said.contains("hasn't connected"));
            return Ok(ToolResult::error(if unconnected {
                format!(
                    "Not refreshed: Resend isn't connected yet. The team connects it in the studio: {CONNECT_URL}"
                )
            } else if refresh {
                format!("Not refreshed: {}", email_refusal(&answer, Topic::Other))
            } else {
                email_refusal(&answer, Topic::Other)
            }));
        }
        let connection = answer.data;
        let (line, mut facts) = super::connection_line(&connection);
        let mut lines = Vec::new();
        if refresh {
            lines.push("Asked Resend again.".to_string());
        }
        lines.push(line);
        if connection.get("connected").and_then(Value::as_bool) == Some(true) {
            let sender = connection.get("sender").cloned().unwrap_or(Value::Null);
            lines.push(format!("- Sender: {}.", sender_words(&sender)));
            let domains_answered = connection["domains"].is_array();
            let domains = connection
                .get("domains")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            lines.push(if !domains_answered {
                format!("- Domains: {NOT_ANSWERED}.")
            } else if domains.is_empty() {
                match connection.pointer("/key/access").and_then(Value::as_str) {
                    Some("sending_access") => format!(
                        "- Domains: {NOT_ANSWERED} (a sending-only key can't list them; the from address is checked by its shape only)."
                    ),
                    _ => "- Domains: none yet; the team adds and verifies one in Resend.".to_string(),
                }
            } else {
                format!(
                    "- Domains: {}.",
                    domains.iter().map(domain_words).collect::<Vec<_>>().join("; ")
                )
            });
            let webhook = connection.get("webhook").cloned().unwrap_or(Value::Null);
            lines.push(format!(
                "- Webhook: set up by {}, {}; {}.",
                match webhook.get("mode").and_then(Value::as_str) {
                    Some("auto") => "Create",
                    Some(_) => "hand",
                    None => "whom is not answered",
                },
                match webhook.get("secretSet").and_then(Value::as_bool) {
                    Some(true) => "with its signing secret",
                    Some(false) => "with no signing secret yet",
                    None => "its signing secret not answered",
                },
                moment(webhook.get("lastEventAt").and_then(Value::as_str)).map_or_else(
                    || match webhook.get("lastEventAt") {
                        Some(Value::Null) => "no event has arrived yet".into(),
                        _ => format!("last event {NOT_ANSWERED}"),
                    },
                    |at| format!("last event on {at}")
                )
            ));
            facts["sender"] = json!({
                "from_name": sender.get("fromName"),
                "from_email": sender.get("fromEmail"),
                "reply_to": sender.get("replyTo"),
            });
            facts["domains"] = json!(domains_answered.then(|| domains
                .iter()
                .map(|domain| json!({
                    "name": domain.get("name"),
                    "status": domain.get("status"),
                    "open_tracking": domain.get("openTracking"),
                    "click_tracking": domain.get("clickTracking"),
                }))
                .collect::<Vec<_>>()));
            facts["webhook"] = json!({
                "mode": webhook.get("mode"),
                "secret_set": webhook.get("secretSet"),
                "last_event_at": webhook.get("lastEventAt"),
            });
            lines.push(format!(
                "The key, the sender and the domains are set in the studio ({CONNECT_URL}); email_connection with operation refresh has Create ask Resend again, and email_diagnostics says whether its events arrive."
            ));
        }
        facts["workspace_id"] = json!(workspace_id);
        facts["refreshed"] = json!(refresh);
        Ok(ToolResult::with_structured(lines.join("\n"), facts))
    }
}
