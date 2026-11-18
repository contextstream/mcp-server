//! Email sequences, read only: `email_sequences`.
//!
//! A sequence is emails on delays, started for a person by a trigger and ended by an exit.
//! The API holds them and decides who may read them
//! (`GET /admin/email/sequences[/{id}[/steps/{step_id}]]`), so this action asks the API and
//! says what it answers: an account they aren't offered to gets one plain refusal.
//!
//! - no `sequence_id`: the sequences, each with where it stands and its totals;
//! - `sequence_id`: its people, its emails in order with their numbers (all time and within
//!   `days`), and why sends were skipped;
//! - `sequence_id` and `step`: that email's text as it is sent.
//!
//! Nothing here drafts, publishes, pauses or sends, and no recipient is named: the API
//! counts people and never lists them. What a state means is the API's to say (its `state`,
//! `exit_applies` and `edits_live_on_save` fields); this file only puts them in words.
//!
//! One rule for every sentence below: it says what the API answered and nothing else. A field
//! the answer does not carry is said as "not answered", never as zero, "none" or a default;
//! and whether an email's text is being sent is said from the API's `published` and
//! `sequence_state`, never assumed.

use mcp_client::studio::StudioAnswer;
use mcp_types::{tool::ToolResult, Error, Result};
use serde_json::{json, Value};
use uuid::Uuid;

use super::api::{unreachable, CALL_TIMEOUT};
use super::email::{checked_email_answer, holds_resend_key, KEY_REFUSED, SIGN_IN};
use super::words::credits;
use super::{parse_id, StudioInput, StudioTool};

/// The action, as `VALID_ACTIONS` lists it.
pub(crate) const SEQUENCES_ACTION: &str = "email_sequences";
/// The one refusal for an account the API doesn't offer sequences to.
pub(crate) const SEQUENCES_NOT_OFFERED: &str =
    "Email sequences aren't offered to this account. Nothing was read or changed.";
const DAYS_DEFAULT: i64 = 30;
const DAYS_MAX: i64 = 365;

/// The window in days: 30 by default, 1 to 365.
pub(crate) fn window_days(raw: Option<i64>) -> i64 {
    raw.unwrap_or(DAYS_DEFAULT).clamp(1, DAYS_MAX)
}

/// The email's number (1 is the first), when one was asked for.
pub(crate) fn step_number(raw: Option<i64>) -> Result<Option<i64>> {
    match raw {
        None => Ok(None),
        Some(number) if number >= 1 => Ok(Some(number)),
        Some(_) => Err(Error::Validation(
            "step is the email's number in the sequence: 1 is the first".to_string(),
        )),
    }
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// A number the API answered, or `None`: a missing number is never read as zero.
fn count(value: &Value, pointer: &str) -> Option<i64> {
    value.pointer(pointer).and_then(Value::as_i64)
}

pub(crate) const NOT_ANSWERED: &str = "not answered";
const UNREADABLE: &str = "ContextStream's answer about email sequences couldn't be read: try again in a few minutes. Nothing was changed.";

/// "1,124", or "not answered".
fn number(value: Option<i64>) -> String {
    value
        .map(credits)
        .unwrap_or_else(|| NOT_ANSWERED.to_string())
}

/// "1,124", or "?" where three numbers share a line (the line's legend says what "?" is).
fn short(value: Option<i64>) -> String {
    value.map(credits).unwrap_or_else(|| "?".to_string())
}

/// "no complaints", "3 complained", or that it was not answered.
fn complaints(value: Option<i64>) -> String {
    match value {
        Some(0) => "no complaints".to_string(),
        Some(complained) => format!("{} complained", credits(complained)),
        None => "complaints not answered".to_string(),
    }
}

/// An ISO time as "2026-11-02 09:00 UTC".
fn moment(value: Option<&str>) -> Option<String> {
    let at = chrono::DateTime::parse_from_rfc3339(value?).ok()?;
    Some(format!(
        "{} UTC",
        at.with_timezone(&chrono::Utc).format("%Y-%m-%d %H:%M")
    ))
}

fn people(count: Option<i64>) -> String {
    match count {
        Some(1) => "1 person so far".to_string(),
        Some(count) => format!("{} people so far", credits(count)),
        None => "how many people so far was not answered".to_string(),
    }
}

/// "day 0", "day 2", "day 4.5".
fn day_words(day: f64) -> String {
    if (day - day.round()).abs() < 1e-9 {
        format!("day {}", day.round() as i64)
    } else {
        format!("day {day:.1}")
    }
}

/// Where a sequence stands, in words, from the API's `state`. A state this build does not
/// know is shown as it is rather than guessed at.
pub(crate) fn state_words(sequence: &Value) -> String {
    match sequence.get("state").and_then(Value::as_str) {
        Some("running") => "running".to_string(),
        Some("paused") => "paused: nothing is being sent".to_string(),
        Some("not_running") => match text(sequence, "status") {
            Some(status) => format!("not running (status {status})"),
            None => "not running".to_string(),
        },
        Some("enrolled_only") => {
            "sending only to the people already in it: nobody new is enrolled".to_string()
        }
        Some(other) => format!("state {other}"),
        None => "where it stands was not answered".to_string(),
    }
}

/// What starts and ends it, from the automation's two fields and whether the exit is being
/// applied. A value this build does not know is shown as it is rather than guessed at.
pub(crate) fn rule_words(sequence: &Value) -> String {
    let trigger = match sequence
        .pointer("/automation/trigger")
        .and_then(Value::as_str)
    {
        Some("starter_signup") => "a new Starter sign-up starts it".to_string(),
        Some(other) => format!("its trigger is {other}"),
        None => "what starts it was not answered".to_string(),
    };
    let applies = sequence.get("exit_applies").and_then(Value::as_bool);
    let exit = match (
        sequence
            .pointer("/automation/exit_condition")
            .and_then(Value::as_str),
        applies,
    ) {
        (Some("paid_subscription"), Some(true)) => "paying ends it".to_string(),
        (Some("paid_subscription"), Some(false)) => "paying no longer ends it".to_string(),
        (Some("paid_subscription"), None) => {
            "paying is its exit (whether that is being applied was not answered)".to_string()
        }
        (Some(other), Some(true)) => format!("its exit is {other}"),
        (Some(other), Some(false)) => format!("its exit ({other}) is not being applied"),
        (Some(other), None) => {
            format!("its exit is {other} (whether that is being applied was not answered)")
        }
        (None, _) => "what ends it was not answered".to_string(),
    };
    format!("{trigger}; {exit}")
}

/// Where a change is made and whether a save there is live, from the API's fields.
pub(crate) fn changes_words(sequence: &Value) -> String {
    let live = sequence.get("edits_live_on_save").and_then(Value::as_bool) == Some(true);
    match (text(sequence, "admin_url"), live) {
        (Some(url), true) => format!(
            " A change is made at {url}, and a save there is live at once: there is no draft."
        ),
        (Some(url), false) => format!(" A change is made at {url}."),
        (None, true) => " A saved change is live at once: there is no draft.".to_string(),
        (None, false) => String::new(),
    }
}

/// "sent 1,124, delivered 1,075, opened 340, clicked 36, bounced 21, unsubscribed 4, no complaints".
pub(crate) fn totals_words(totals: &Value) -> String {
    format!(
        "sent {}, delivered {}, opened {}, clicked {}, bounced {}, unsubscribed {}, {}",
        number(count(totals, "/sent")),
        number(count(totals, "/delivered")),
        number(count(totals, "/opened")),
        number(count(totals, "/clicked")),
        number(count(totals, "/bounced")),
        number(count(totals, "/unsubscribed")),
        complaints(count(totals, "/complained"))
    )
}

fn emails_words(sequence: &Value) -> String {
    let Some(steps) = count(sequence, "/steps") else {
        return "how many emails it has was not answered".to_string();
    };
    let published = sequence
        .pointer("/automation/published_steps")
        .and_then(Value::as_i64)
        .map(|n| n.min(steps));
    let noun = if steps == 1 { "email" } else { "emails" };
    match published {
        Some(published) if published < steps => {
            format!("{steps} {noun}, {published} published")
        }
        _ => format!("{steps} {noun}"),
    }
}

/// One line for one sequence in the list.
pub(crate) fn summary_line(sequence: &Value) -> String {
    let name = text(sequence, "name").unwrap_or_else(|| "A sequence".to_string());
    let id = text(sequence, "id").unwrap_or_else(|| NOT_ANSWERED.to_string());
    let last = moment(sequence.get("last_sent_at").and_then(Value::as_str))
        .map(|at| format!(" Last send {at}."))
        .unwrap_or_default();
    format!(
        "- {name}: {}. {}; {}. {}, {} part-way through. All time: {}.{last} (sequence_id {id})",
        state_words(sequence),
        emails_words(sequence),
        rule_words(sequence),
        people(count(sequence, "/people")),
        number(count(sequence, "/people_mid_sequence")),
        totals_words(sequence.get("totals").unwrap_or(&Value::Null)),
    )
}

/// One line for one email of a sequence.
pub(crate) fn step_line(step: &Value) -> String {
    let mut line = format!(
        "{}. {}: {}; {}/{}/{}; {}/{}/{}",
        short(count(step, "/number")),
        subject_words(step),
        day_of(step),
        short(count(step, "/window/sent")),
        short(count(step, "/window/opened")),
        short(count(step, "/window/clicked")),
        short(count(step, "/totals/sent")),
        short(count(step, "/totals/opened")),
        short(count(step, "/totals/clicked")),
    );
    match step.get("published").and_then(Value::as_bool) {
        Some(true) => {}
        Some(false) => line.push_str("; not published, so it is not sent"),
        None => line.push_str("; whether it is published was not answered"),
    }
    match count(step, "/waiting") {
        Some(waiting) if waiting > 0 => {
            line.push_str(&format!("; {} waiting", credits(waiting)));
            if let Some(next) = moment(step.get("next_due_at").and_then(Value::as_str)) {
                line.push_str(&format!(", next {next}"));
            }
        }
        Some(_) => {}
        None => line.push_str("; how many are waiting was not answered"),
    }
    line
}

/// The subject as answered; an empty one and one the answer does not carry are told apart.
fn subject_words(step: &Value) -> String {
    match step.get("subject").and_then(Value::as_str).map(str::trim) {
        Some("") => "(no subject)".to_string(),
        Some(subject) => subject.to_string(),
        None => "(subject not answered)".to_string(),
    }
}

/// "day 2", or that the day was not answered.
fn day_of(step: &Value) -> String {
    match step.get("day").and_then(Value::as_f64) {
        Some(day) => day_words(day),
        None => "day not answered".to_string(),
    }
}

/// One group of sends that were not made: how many, skipped or failed, and why. The API
/// answers a reason only when it is one of the admin's own sentences; the mail provider's
/// own text is counted and withheld, and that is said as it is.
pub(crate) fn skip_words(row: &Value) -> String {
    let why = match (
        text(row, "reason"),
        row.get("withheld").and_then(Value::as_bool),
    ) {
        (Some(reason), _) => reason,
        (None, Some(true)) => {
            "reason withheld: the mail provider's own text, which stays in the admin".to_string()
        }
        (None, Some(false)) => "no reason stored".to_string(),
        (None, None) => "reason not answered".to_string(),
    };
    format!(
        "{} {} ({why})",
        short(count(row, "/count")),
        text(row, "status").unwrap_or_else(|| "not sent".to_string()),
    )
}

/// Whether an email's text is being sent, from the two fields the API answers with the text
/// (`published` and `sequence_state`). Nothing is said that they do not say.
pub(crate) fn sending_words(step: &Value) -> &'static str {
    match (
        step.get("published").and_then(Value::as_bool),
        step.get("sequence_state").and_then(Value::as_str),
    ) {
        (Some(false), _) => "This email is not published, so this text is not being sent.",
        (Some(true), Some("running")) => "This is the text being sent now.",
        (Some(true), Some("paused")) => {
            "The sequence is paused, so this text is not being sent now."
        }
        (Some(true), Some("not_running")) => {
            "The sequence is not running, so this text is not being sent now."
        }
        (Some(true), Some("enrolled_only")) => {
            "This is the text being sent to the people already in the sequence; nobody new is enrolled."
        }
        _ => "Whether this text is being sent now was not answered.",
    }
}

/// What the API said, for a person: a sign-in, the offer, something missing, or nothing
/// usable (which the caller hears as "try again").
pub(crate) enum Refusal {
    Said(String),
    Unreachable,
}

pub(crate) fn refusal(answer: &StudioAnswer, missing: &str) -> Refusal {
    match answer.status {
        401 => Refusal::Said(SIGN_IN.to_string()),
        403 => Refusal::Said(SEQUENCES_NOT_OFFERED.to_string()),
        404 => Refusal::Said(missing.to_string()),
        _ => Refusal::Unreachable,
    }
}

const NO_SEQUENCE: &str =
    "No email sequence has that sequence_id: email_sequences without it lists them.";

impl StudioTool {
    pub(super) async fn email_sequences(&self, input: &StudioInput) -> Result<ToolResult> {
        // Before anything is asked of anyone: a key never travels.
        if serde_json::to_value(input).is_ok_and(|raw| holds_resend_key(&raw)) {
            return Ok(ToolResult::error(KEY_REFUSED.to_string()));
        }
        let step = step_number(input.step)?;
        let days = window_days(input.days);
        let sequence_id = match input
            .sequence_id
            .as_deref()
            .map(str::trim)
            .filter(|raw| !raw.is_empty())
        {
            Some(raw) => Some(parse_id(raw, "sequence_id")?),
            None => None,
        };
        match (sequence_id, step) {
            (None, Some(_)) => Err(Error::Validation(
                "step needs sequence_id: email_sequences without either lists the sequences"
                    .to_string(),
            )),
            (None, None) => self.sequences_list().await,
            (Some(id), None) => self.sequence_detail(id, days).await,
            (Some(id), Some(number)) => self.sequence_step(id, number, days).await,
        }
    }

    async fn sequences_call(&self, path: &str) -> Result<StudioAnswer> {
        self.client
            .studio_call("GET", path, None, None, CALL_TIMEOUT)
            .await
            .map_err(unreachable)
            .and_then(checked_email_answer)
    }

    fn sequences_refused(answer: &StudioAnswer, missing: &str) -> Result<ToolResult> {
        match refusal(answer, missing) {
            Refusal::Said(said) => Ok(ToolResult::error(said)),
            Refusal::Unreachable => Err(Error::Network(
                "ContextStream isn't answering about email sequences right now: try again in a few minutes. Nothing was changed."
                    .to_string(),
            )),
        }
    }

    async fn sequences_list(&self) -> Result<ToolResult> {
        let answer = self.sequences_call("/admin/email/sequences").await?;
        if !answer.is_success() {
            return Self::sequences_refused(&answer, NO_SEQUENCE);
        }
        // An answer without the list is not "no sequences": it is an answer that can't be read.
        let Some(sequences) = answer
            .data
            .get("sequences")
            .and_then(Value::as_array)
            .cloned()
        else {
            return Err(Error::Network(UNREADABLE.to_string()));
        };
        if sequences.is_empty() {
            return Ok(ToolResult::with_structured(
                "No email sequences yet.".to_string(),
                json!({"sequences": [], "as_of": answer.data.get("as_of")}),
            ));
        }
        let mut lines = vec![format!(
            "{} email sequence{}:",
            sequences.len(),
            if sequences.len() == 1 { "" } else { "s" }
        )];
        lines.extend(sequences.iter().map(summary_line));
        lines.push(
            "email_sequences with a sequence_id shows its emails and their numbers; nothing here changes or sends anything."
                .to_string(),
        );
        Ok(ToolResult::with_structured(
            lines.join("\n"),
            json!({"sequences": sequences, "as_of": answer.data.get("as_of")}),
        ))
    }

    async fn read_sequence(&self, id: Uuid, days: i64) -> Result<StudioAnswer> {
        self.sequences_call(&format!("/admin/email/sequences/{id}?days={days}"))
            .await
    }

    async fn sequence_detail(&self, id: Uuid, days: i64) -> Result<ToolResult> {
        let answer = self.read_sequence(id, days).await?;
        if !answer.is_success() {
            return Self::sequences_refused(&answer, NO_SEQUENCE);
        }
        let detail = &answer.data;
        let (Some(sequence), Some(steps)) = (
            detail.get("sequence").filter(|s| s.is_object()).cloned(),
            detail.get("steps").and_then(Value::as_array).cloned(),
        ) else {
            return Err(Error::Network(UNREADABLE.to_string()));
        };
        let name = text(&sequence, "name").unwrap_or_else(|| "The sequence".to_string());
        let days = detail
            .get("window_days")
            .and_then(Value::as_i64)
            .unwrap_or(days);
        let window = if days == 1 {
            "the last day".to_string()
        } else {
            format!("the last {days} days")
        };
        let mut lines = vec![
            format!(
                "{name}: {}. {}; {}.",
                state_words(&sequence),
                emails_words(&sequence),
                rule_words(&sequence)
            ),
            format!(
                "People: {} so far, {} in {window}, {} part-way through, {} left on paying, {} unsubscribed, {} bounced, {}.",
                number(count(detail, "/people/all_time")),
                number(count(detail, "/people/enrolled_in_window")),
                number(count(detail, "/people/mid_sequence")),
                number(count(detail, "/people/exited_paid")),
                number(count(detail, "/people/unsubscribed")),
                number(count(detail, "/people/bounced")),
                complaints(count(detail, "/people/complained"))
            ),
            format!(
                "In {window}: {}.",
                totals_words(detail.get("window_totals").unwrap_or(&Value::Null))
            ),
            format!(
                "All time: {}.",
                totals_words(sequence.get("totals").unwrap_or(&Value::Null))
            ),
        ];
        if !steps.is_empty() {
            let step_lines: Vec<String> = steps.iter().map(step_line).collect();
            lines.push(format!(
                "Emails (day after enrollment; sent/opened/clicked in {window}; sent/opened/clicked all time{}):",
                if step_lines.iter().any(|line| line.contains('?')) {
                    "; ? is a number that was not answered"
                } else {
                    ""
                }
            ));
            lines.extend(step_lines);
        }
        match detail.get("skipped").and_then(Value::as_array) {
            Some(rows) if !rows.is_empty() => lines.push(format!(
                "Not sent in {window}: {}.",
                rows.iter().map(skip_words).collect::<Vec<_>>().join("; ")
            )),
            Some(_) => {}
            None => lines.push(format!(
                "Why sends were not made in {window} was not answered."
            )),
        }
        if let Some(at) = moment(detail.get("as_of").and_then(Value::as_str)) {
            lines.push(format!(
                "As of {at}. Opens are what mail apps report: treat them as rough."
            ));
        }
        lines.push(format!(
            "email_sequences with this sequence_id and step (1 to {}) reads an email's text. Nothing here changes or sends anything.{}",
            steps.len().max(1),
            changes_words(&sequence)
        ));
        Ok(ToolResult::with_structured(
            lines.join("\n"),
            detail.clone(),
        ))
    }

    async fn sequence_step(&self, id: Uuid, number: i64, days: i64) -> Result<ToolResult> {
        let answer = self.read_sequence(id, days).await?;
        if !answer.is_success() {
            return Self::sequences_refused(&answer, NO_SEQUENCE);
        }
        let Some(steps) = answer.data.get("steps").and_then(Value::as_array).cloned() else {
            return Err(Error::Network(UNREADABLE.to_string()));
        };
        let name = answer
            .data
            .pointer("/sequence/name")
            .and_then(Value::as_str)
            .unwrap_or("the sequence")
            .to_string();
        let found = steps
            .iter()
            .find(|step| step.get("number").and_then(Value::as_i64) == Some(number));
        let Some(found) = found else {
            return Ok(ToolResult::error(format!(
                "“{name}” has {} email{}: there is no email {number}.",
                steps.len(),
                if steps.len() == 1 { "" } else { "s" }
            )));
        };
        let Some(step_id) = text(found, "id").and_then(|raw| Uuid::parse_str(&raw).ok()) else {
            return Err(Error::Network(
                "ContextStream's answer about that email couldn't be read: try again in a few minutes. Nothing was changed."
                    .to_string(),
            ));
        };
        let content = self
            .sequences_call(&format!("/admin/email/sequences/{id}/steps/{step_id}"))
            .await?;
        if !content.is_success() {
            return Self::sequences_refused(
                &content,
                "That email is no longer in the sequence: email_sequences with the sequence_id lists its emails.",
            );
        }
        let step = &content.data;
        let mut lines = vec![format!(
            "Email {number} of “{name}” ({}; {}):",
            match step.get("published").and_then(Value::as_bool) {
                Some(true) => "published",
                Some(false) => "not published, so it is not sent",
                None => "whether it is published was not answered",
            },
            day_of(found)
        )];
        lines.push(format!("Subject: {}", subject_words(step)));
        if let Some(headline) = text(step, "headline") {
            lines.push(format!("Headline: {headline}"));
        }
        match (text(step, "cta_text"), text(step, "cta_url")) {
            (Some(label), Some(url)) => lines.push(format!("Button: {label} -> {url}")),
            (Some(label), None) => lines.push(format!("Button: {label} (no link)")),
            (None, Some(url)) => lines.push(format!("Button link: {url}")),
            (None, None) => {}
        }
        lines.push(match text(step, "editor_mode") {
            Some(mode) => format!("Body, as stored ({mode}):"),
            None => "Body, as stored:".to_string(),
        });
        lines.push(match step.get("message").and_then(Value::as_str) {
            Some(body) => body.to_string(),
            None => "(the body was not answered)".to_string(),
        });
        lines.push(format!(
            "{} Nothing here changes it.{}",
            sending_words(step),
            changes_words(answer.data.get("sequence").unwrap_or(&Value::Null))
        ));
        // What the API answered for this email, with the two things its sequence answered
        // about it (both only when they were answered).
        let mut structured = step.clone();
        if let Some(day) = found.get("day").filter(|day| day.is_number()) {
            structured["day"] = day.clone();
        }
        if let Some(name) = answer
            .data
            .pointer("/sequence/name")
            .filter(|n| n.is_string())
        {
            structured["sequence_name"] = name.clone();
        }
        Ok(ToolResult::with_structured(lines.join("\n"), structured))
    }
}
