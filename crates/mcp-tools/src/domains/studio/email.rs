//! Email from the studio, on the team's own Resend: ContextStream Create's email
//! (contextstream `handlers/create/email`, runbook `docs/runbooks/create-email.md`). The web
//! studio does the same at ctxproduct.com/studio/email.
//!
//! - `email_audiences`: the audiences and their segments, with how many each reaches, and
//!   whether Resend is connected and ready to send.
//! - `email_draft`: one prompt to a draft campaign in the team's brand
//!   (`POST /create/email/campaigns/draft`), with subject and preheader suggestions, an outline
//!   and who it would go to. Sends nothing.
//! - `email_campaigns`: the workspace's campaigns and where each stands.
//! - `email_test`: a test send to the person's own address or Resend's test addresses.
//! - `email_send`: sends now or at a local time in a time zone, only with the person's go:
//!   asked through MCP elicitation when the client can ask them, otherwise
//!   `confirm_recipients` must be the recipient count they were shown. The schedule carries an
//!   idempotency key, so a retried call schedules once.
//! - `email_cancel`: unschedules, or stops a campaign that is sending.
//! - `email_report`: a campaign's report, or the workspace's trends.
//!
//! Every action first reads Create's catalog: email is offered only where it has an `email`
//! block. Agents draft; people send.

use mcp_client::studio::StudioAnswer;
use mcp_types::{tool::ToolResult, Error, Result};
use serde_json::{json, Value};
use uuid::Uuid;

use super::api::{unreachable, CALL_TIMEOUT};
use super::words::{clip, credits};
use super::{parse_id, StudioInput, StudioTool, ELICIT_TIMEOUT, MAX_PROMPT_CHARS};
use crate::peer::{self, Elicited};

/// The email actions, as `VALID_ACTIONS` lists them.
pub(crate) const EMAIL_ACTIONS: &[&str] = &[
    "email_audiences",
    "email_draft",
    "email_campaigns",
    "email_test",
    "email_send",
    "email_cancel",
    "email_report",
];

/// Where the studio is, for links a person can open.
pub(crate) const STUDIO_APP: &str = "https://ctxproduct.com";

/// Said when email isn't offered to the caller.
pub(crate) const NOT_OFFERED: &str = "Email isn't offered to this account yet: it opens with one-prompt making, team by team. Nothing was sent or changed.";

const CAMPAIGN_STATUSES: &[&str] = &[
    "draft",
    "scheduled",
    "sending",
    "sent",
    "cancelled",
    "failed",
];
const CAMPAIGNS_LIMIT_DEFAULT: usize = 10;
const CAMPAIGNS_LIMIT_MAX: usize = 50;
/// A test send goes to at most this many addresses (Create's limit).
const MAX_TEST_RECIPIENTS: usize = 5;
const TOP_LINKS: usize = 5;
const LISTED_BOUNCES: usize = 5;
const STATS_DAYS_DEFAULT: i64 = 30;

/// What a refused email call was about, for its sentence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Topic {
    Campaign,
    Audience,
    Other,
}

impl Topic {
    fn missing(self) -> &'static str {
        match self {
            Self::Campaign => "There is no such campaign in this workspace (a campaign is the campaign_id email_draft or email_campaigns returned).",
            Self::Audience => "There is no such audience or segment in this workspace (email_audiences lists them).",
            Self::Other => "Create couldn't find that in this workspace.",
        }
    }
}

/// One refused email answer in one sentence, for the person and their agent. Create's own
/// sentences are kept: they say what to fix.
pub(crate) fn email_refusal(answer: &StudioAnswer, topic: Topic) -> String {
    let message = answer.error_message();
    let said = message.as_deref().unwrap_or("").trim();
    match answer.status {
        401 => "ContextStream didn't accept this sign-in: sign in again (or set a new API key), then try once more.".to_string(),
        403 if said.contains("Create isn't available") => NOT_OFFERED.to_string(),
        404 if said.contains("isn't offered") => NOT_OFFERED.to_string(),
        404 if said.contains("No such workspace") => {
            "That workspace isn't one this account can use.".to_string()
        }
        404 => topic.missing().to_string(),
        429 if said.is_empty() => "Too many requests for this workspace: try again in a minute.".to_string(),
        500..=599 if said.is_empty() => {
            "Create isn't answering right now: try again in a few minutes. Nothing was sent.".to_string()
        }
        _ if !said.is_empty() => said.to_string(),
        status => format!("Create refused this (HTTP {status})."),
    }
}

/// A Resend test address: delivered, bounced, complained or suppressed at resend.dev, with an
/// optional `+label`.
pub(crate) fn is_resend_test_address(address: &str) -> bool {
    let Some((local, domain)) = address.rsplit_once('@') else {
        return false;
    };
    if domain != "resend.dev" {
        return false;
    }
    let (base, label) = match local.split_once('+') {
        Some((base, label)) => (base, Some(label)),
        None => (local, None),
    };
    matches!(base, "delivered" | "bounced" | "complained" | "suppressed")
        && label.is_none_or(|label| {
            !label.is_empty()
                && label
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        })
}

/// The addresses a test goes to, as given (one string, a comma-separated string or a list),
/// lower-cased and without repeats.
pub(crate) fn test_recipients(to: Option<&Value>) -> Result<Vec<String>> {
    let raw: Vec<String> = match to {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::String(text)) => text.split([',', ';']).map(str::to_string).collect(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| Error::Validation("to is a list of email addresses".to_string()))
            })
            .collect::<Result<_>>()?,
        Some(_) => {
            return Err(Error::Validation(
                "to is an email address or a list of them".to_string(),
            ))
        }
    };
    let mut out: Vec<String> = Vec::new();
    for address in raw {
        let address = address.trim().to_lowercase();
        if address.is_empty() || out.contains(&address) {
            continue;
        }
        out.push(address);
    }
    if out.len() > MAX_TEST_RECIPIENTS {
        return Err(Error::Validation(format!(
            "A test goes to at most {MAX_TEST_RECIPIENTS} addresses."
        )));
    }
    Ok(out)
}

/// `"now"`, or a local time `YYYY-MM-DDTHH:MM` (seconds allowed) to be read in a time zone.
pub(crate) fn send_at_of(raw: Option<&str>) -> Result<String> {
    let value = raw
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .unwrap_or("now");
    if value.eq_ignore_ascii_case("now") {
        return Ok("now".to_string());
    }
    let local = value.replacen(' ', "T", 1);
    let parsed = chrono::NaiveDateTime::parse_from_str(&local, "%Y-%m-%dT%H:%M")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(&local, "%Y-%m-%dT%H:%M:%S"));
    match parsed {
        Ok(at) => Ok(at.format("%Y-%m-%dT%H:%M").to_string()),
        Err(_) => Err(Error::Validation(
            "send_at is \"now\" or a local time like 2026-11-02T09:00 (with time_zone)".to_string(),
        )),
    }
}

/// An IANA time zone name as Create reads them (`Europe/London`, `UTC`); Create checks it is
/// a real one.
pub(crate) fn time_zone_of(raw: Option<&str>, send_at: &str) -> Result<String> {
    match raw.map(str::trim).filter(|v| !v.is_empty()) {
        Some(zone)
            if zone.len() <= 64
                && zone
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '+')) =>
        {
            Ok(zone.to_string())
        }
        Some(_) => Err(Error::Validation(
            "time_zone is an IANA name, like Europe/London or America/New_York".to_string(),
        )),
        None if send_at == "now" => Ok("UTC".to_string()),
        None => Err(Error::Validation(
            "time_zone is needed with a local send_at: the person's IANA zone, like Europe/London"
                .to_string(),
        )),
    }
}

/// When a send goes, in words.
pub(crate) fn when_words(send_at: &str, time_zone: &str) -> String {
    if send_at == "now" {
        "now".to_string()
    } else {
        format!("at {} ({time_zone})", send_at.replace('T', " "))
    }
}

/// The key a schedule is sent with: the same campaign, revision, time and zone schedule once.
pub(crate) fn send_key(campaign_id: Uuid, revision: i64, send_at: &str, time_zone: &str) -> String {
    let tail: String = format!("{send_at}-{time_zone}")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("email-send-{campaign_id}-r{revision}-{tail}")
}

/// "1 person", "1,200 people".
fn people(count: i64) -> String {
    if count == 1 {
        "1 person".to_string()
    } else {
        format!("{} people", credits(count))
    }
}

/// A rate (0 to 1) as a percentage: one decimal, two under 1%.
pub(crate) fn percent(rate: f64) -> String {
    let value = rate * 100.0;
    if value > 0.0 && value < 1.0 {
        format!("{value:.2}%")
    } else {
        format!("{value:.1}%")
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

fn count(value: &Value, pointer: &str) -> i64 {
    value.pointer(pointer).and_then(Value::as_i64).unwrap_or(0)
}

/// An ISO time as "2026-11-02 09:00 UTC".
fn moment(value: Option<&str>) -> Option<String> {
    let at = chrono::DateTime::parse_from_rfc3339(value?).ok()?;
    Some(format!(
        "{} UTC",
        at.with_timezone(&chrono::Utc).format("%Y-%m-%d %H:%M")
    ))
}

/// The campaign's studio page.
pub(crate) fn campaign_url(campaign_id: &str) -> String {
    format!("{STUDIO_APP}/studio/email/campaigns/{campaign_id}")
}

/// What keeps a connection from sending, in words.
fn missing_words(missing: &Value) -> Vec<&'static str> {
    missing
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(|item| match item {
                    "from_email" => "a from address",
                    "verified_domain" => "a verified sending domain for the from address",
                    "postal_address" => "a postal address for the footer",
                    "company_name" => "a company name for the footer",
                    "webhook" => "the webhook's signing secret (for statistics and bounces)",
                    _ => "a setting in the studio",
                })
                .collect()
        })
        .unwrap_or_default()
}

/// One line on the Resend connection, and its facts.
fn connection_line(connection: &Value) -> (String, Value) {
    if connection.get("connected").and_then(Value::as_bool) != Some(true) {
        return (
            format!("Resend isn't connected yet: the team connects its own Resend key in the studio ({STUDIO_APP}/studio/email). Drafts work before then."),
            json!({"connected": false}),
        );
    }
    let last4 = connection
        .pointer("/key/last4")
        .and_then(Value::as_str)
        .unwrap_or("????");
    let ready = connection.get("ready").and_then(Value::as_bool) == Some(true);
    let missing = connection.get("missing").cloned().unwrap_or(json!([]));
    let sender = connection.get("sender").cloned().unwrap_or(Value::Null);
    let line = if ready {
        let from = match (text(&sender, "fromName"), text(&sender, "fromEmail")) {
            (Some(name), Some(email)) => format!(", sending as {name} <{email}>"),
            (None, Some(email)) => format!(", sending as {email}"),
            _ => String::new(),
        };
        format!("Resend is connected (key ending {last4}){from}.")
    } else {
        let words = missing_words(&missing);
        format!(
            "Resend is connected (key ending {last4}), but campaigns can't be sent until the studio has {}: {STUDIO_APP}/studio/email.",
            if words.is_empty() {
                "its settings finished".to_string()
            } else {
                words.join(", ")
            }
        )
    };
    (
        line,
        json!({"connected": true, "ready": ready, "missing": missing, "key_last4": last4}),
    )
}

/// The blocks of a campaign, one line each.
pub(crate) fn outline(content: &Value) -> Vec<String> {
    let blocks = content
        .get("blocks")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut lines = Vec::new();
    for block in &blocks {
        let line = match block.get("type").and_then(Value::as_str).unwrap_or("") {
            "heading" => format!(
                "Heading: {}",
                clip(&text(block, "text").unwrap_or_default(), 100)
            ),
            "text" => format!(
                "Text: {}",
                clip(&text(block, "text").unwrap_or_default(), 140)
            ),
            "button" => format!(
                "Button: {} → {}",
                text(block, "label").unwrap_or_default(),
                text(block, "url").unwrap_or_default()
            ),
            "image" => format!(
                "Image: {}",
                text(block, "alt").unwrap_or_else(|| "(no alt text)".to_string())
            ),
            "divider" => "Divider".to_string(),
            "spacer" => continue,
            other => format!("{other} block"),
        };
        lines.push(format!("{}. {line}", lines.len() + 1));
    }
    lines
}

/// One campaign in a list, in words.
fn campaign_state(campaign: &Value) -> String {
    let status = campaign.get("status").and_then(Value::as_str).unwrap_or("");
    let sent = count(campaign, "/progress/sent");
    let recipients = count(campaign, "/progress/recipients");
    match status {
        "draft" => "draft".to_string(),
        "scheduled" => match moment(campaign.pointer("/schedule/sendAt").and_then(Value::as_str)) {
            Some(at) => format!("scheduled for {at}"),
            None => "scheduled".to_string(),
        },
        "sending" => format!("sending: {} of {} sent", credits(sent), credits(recipients)),
        "sent" => format!(
            "sent to {}: {} delivered, {} opened, {} clicked",
            people(sent),
            credits(count(campaign, "/stats/delivered")),
            credits(count(campaign, "/stats/opened")),
            credits(count(campaign, "/stats/clicked"))
        ),
        "cancelled" => format!("stopped after {} sent", credits(sent)),
        "failed" => format!(
            "stopped: {}",
            text(campaign, "lastError").unwrap_or_else(|| "Resend refused it".to_string())
        ),
        other => other.to_string(),
    }
}

impl StudioTool {
    /// The email actions: offered only where Create's catalog has an `email` block.
    pub(super) async fn email(&self, action: &str, input: &StudioInput) -> Result<ToolResult> {
        if !self.email_offered().await? {
            return Ok(ToolResult::error(NOT_OFFERED.to_string()));
        }
        let (workspace_id, _) = self.scope(input).await?;
        match action {
            "email_audiences" => self.email_audiences(workspace_id).await,
            "email_draft" => self.email_draft(workspace_id, input).await,
            "email_campaigns" => self.email_campaigns(workspace_id, input).await,
            "email_test" => self.email_test(workspace_id, input).await,
            "email_send" => self.email_send(workspace_id, input).await,
            "email_cancel" => self.email_cancel(workspace_id, input).await,
            "email_report" => self.email_report(workspace_id, input).await,
            other => Err(Error::Validation(format!("Unknown email action: {other}"))),
        }
    }

    /// Whether Create's catalog offers email to this caller.
    async fn email_offered(&self) -> Result<bool> {
        let answer = self
            .client
            .studio_call("GET", "/create/catalog", None, None, CALL_TIMEOUT)
            .await
            .map_err(unreachable)?;
        match answer.status {
            200..=299 => Ok(answer.data.get("email").is_some_and(Value::is_object)),
            401 | 403 | 404 => Ok(false),
            _ => Err(Error::Network(
                "Create isn't answering right now: try again in a few minutes. Nothing was sent."
                    .to_string(),
            )),
        }
    }

    async fn email_call(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        key: Option<&str>,
    ) -> Result<StudioAnswer> {
        self.client
            .studio_call(
                method,
                &format!("/create/email{path}"),
                body,
                key,
                CALL_TIMEOUT,
            )
            .await
            .map_err(unreachable)
    }

    fn campaign_id(input: &StudioInput, action: &str) -> Result<Uuid> {
        let raw = input
            .campaign_id
            .as_deref()
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| {
                Error::Validation(format!(
                    "campaign_id is required for {action} (email_campaigns lists them)"
                ))
            })?;
        parse_id(raw, "campaign_id")
    }

    async fn read_campaign(
        &self,
        workspace_id: Uuid,
        campaign_id: Uuid,
    ) -> Result<std::result::Result<Value, String>> {
        let answer = self
            .email_call(
                "GET",
                &format!("/campaigns/{campaign_id}?workspaceId={workspace_id}"),
                None,
                None,
            )
            .await?;
        Ok(if answer.is_success() {
            Ok(answer.data)
        } else {
            Err(email_refusal(&answer, Topic::Campaign))
        })
    }

    /// The campaign as it would go: who it goes to and what to check (`/preview`).
    async fn preview(&self, workspace_id: Uuid, campaign_id: Uuid) -> Option<Value> {
        let answer = self
            .email_call(
                "GET",
                &format!("/campaigns/{campaign_id}/preview?workspaceId={workspace_id}"),
                None,
                None,
            )
            .await
            .ok()?;
        answer.is_success().then_some(answer.data)
    }

    // ------------------------------------------------------------------------------------
    // email_audiences
    // ------------------------------------------------------------------------------------

    async fn email_audiences(&self, workspace_id: Uuid) -> Result<ToolResult> {
        let audiences = self
            .email_call(
                "GET",
                &format!("/audiences?workspaceId={workspace_id}"),
                None,
                None,
            )
            .await?;
        if !audiences.is_success() {
            return Ok(ToolResult::error(email_refusal(&audiences, Topic::Other)));
        }
        let connection = self
            .email_call(
                "GET",
                &format!("/connection?workspaceId={workspace_id}"),
                None,
                None,
            )
            .await
            .ok()
            .filter(StudioAnswer::is_success)
            .map(|answer| answer.data);
        let segments = self
            .email_call(
                "GET",
                &format!("/segments?workspaceId={workspace_id}"),
                None,
                None,
            )
            .await
            .ok()
            .filter(StudioAnswer::is_success)
            .and_then(|answer| answer.data.as_array().cloned())
            .unwrap_or_default();

        let mut lines = Vec::new();
        let mut structured = json!({"workspace_id": workspace_id});
        if let Some(connection) = &connection {
            let (line, facts) = connection_line(connection);
            lines.push(line);
            structured["connection"] = facts;
        }
        let list = audiences.data.as_array().cloned().unwrap_or_default();
        if list.is_empty() {
            lines.push(format!(
                "No audiences yet: add contacts in the studio (CSV import, or one by one): {STUDIO_APP}/studio/email."
            ));
        } else {
            lines.push(format!(
                "{} audience{}:",
                list.len(),
                if list.len() == 1 { "" } else { "s" }
            ));
        }
        let mut listed = Vec::new();
        for audience in &list {
            let id = text(audience, "id").unwrap_or_default();
            let name = text(audience, "name").unwrap_or_else(|| "An audience".to_string());
            let contacts = count(audience, "/contacts");
            let subscribed = count(audience, "/subscribed");
            lines.push(format!(
                "- {name}: {} contacts, {} subscribed (audience_id {id})",
                credits(contacts),
                credits(subscribed)
            ));
            let mut own = Vec::new();
            for segment in segments
                .iter()
                .filter(|s| text(s, "audienceId").as_deref() == Some(id.as_str()))
            {
                let segment_id = text(segment, "id").unwrap_or_default();
                let segment_name = text(segment, "name").unwrap_or_else(|| "A segment".to_string());
                let reach = count(segment, "/contacts");
                lines.push(format!(
                    "  - segment {segment_name}: {} contacts (segment_id {segment_id})",
                    credits(reach)
                ));
                own.push(
                    json!({"segment_id": segment_id, "name": segment_name, "contacts": reach}),
                );
            }
            listed.push(json!({
                "audience_id": id,
                "name": name,
                "contacts": contacts,
                "subscribed": subscribed,
                "segments": own,
            }));
        }
        structured["audiences"] = json!(listed);
        Ok(ToolResult::with_structured(lines.join("\n"), structured))
    }

    // ------------------------------------------------------------------------------------
    // email_draft
    // ------------------------------------------------------------------------------------

    async fn email_draft(&self, workspace_id: Uuid, input: &StudioInput) -> Result<ToolResult> {
        let prompt = input
            .prompt
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .ok_or_else(|| {
                Error::Validation(
                    "prompt is required for email_draft: say what the email is for and who gets it, e.g. \"announce our winter sale to customers\"".to_string(),
                )
            })?;
        if prompt.chars().count() > MAX_PROMPT_CHARS {
            return Err(Error::Validation(format!(
                "The prompt can be up to {MAX_PROMPT_CHARS} characters."
            )));
        }
        let mut body = json!({"workspaceId": workspace_id, "prompt": prompt});
        if let Some(audience) = input
            .audience_id
            .as_deref()
            .filter(|a| !a.trim().is_empty())
        {
            body["audienceId"] = json!(parse_id(audience, "audience_id")?);
        }
        if let Some(segment) = input.segment_id.as_deref().filter(|s| !s.trim().is_empty()) {
            body["segmentId"] = json!(parse_id(segment, "segment_id")?);
        }
        let answer = self
            .email_call("POST", "/campaigns/draft", Some(&body), None)
            .await?;
        if !answer.is_success() {
            return Ok(ToolResult::error(format!(
                "Not drafted: {}",
                email_refusal(&answer, Topic::Audience)
            )));
        }
        let campaign = answer.data.get("campaign").cloned().unwrap_or(Value::Null);
        let suggestions = answer
            .data
            .get("suggestions")
            .cloned()
            .unwrap_or(Value::Null);
        let campaign_id = text(&campaign, "id")
            .and_then(|id| Uuid::parse_str(&id).ok())
            .ok_or_else(|| Error::Tool("Create's draft had no id".to_string()))?;
        let preview = self.preview(workspace_id, campaign_id).await;

        let name = text(&campaign, "name").unwrap_or_else(|| "The draft".to_string());
        let subject = text(&campaign, "subject").unwrap_or_default();
        let preheader = text(&campaign, "preheader").unwrap_or_default();
        let brand = campaign
            .pointer("/brand/name")
            .and_then(Value::as_str)
            .filter(|b| !b.trim().is_empty());
        let mut lines = vec![format!(
            "Drafted “{name}”{} (campaign_id {campaign_id}). Nothing is sent.",
            brand.map_or(String::new(), |b| format!(" in {b}'s brand"))
        )];
        lines.push(format!("Subject: {subject}"));
        if !preheader.is_empty() {
            lines.push(format!("Preheader: {preheader}"));
        }
        let others = |key: &str, current: &str| -> Vec<String> {
            suggestions
                .get(key)
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty() && *s != current)
                        .map(|s| format!("“{s}”"))
                        .collect()
                })
                .unwrap_or_default()
        };
        let other_subjects = others("subjects", &subject);
        if !other_subjects.is_empty() {
            lines.push(format!("Other subjects: {}", other_subjects.join(", ")));
        }
        let other_preheaders = others("preheaders", &preheader);
        if !other_preheaders.is_empty() {
            lines.push(format!("Other preheaders: {}", other_preheaders.join(", ")));
        }
        let blocks = outline(campaign.get("content").unwrap_or(&Value::Null));
        if !blocks.is_empty() {
            lines.push(String::new());
            lines.push("Outline:".to_string());
            lines.extend(blocks.iter().cloned());
        }
        lines.push(String::new());
        let audience = campaign
            .pointer("/audience/name")
            .and_then(Value::as_str)
            .map(str::to_string);
        let segment = campaign
            .pointer("/segment/name")
            .and_then(Value::as_str)
            .map(str::to_string);
        let recipients = preview
            .as_ref()
            .and_then(|p| p.get("recipients"))
            .and_then(Value::as_i64);
        match (&audience, recipients) {
            (Some(audience), Some(n)) => lines.push(format!(
                "To: {audience}{} ({}).",
                segment
                    .as_ref()
                    .map_or(String::new(), |s| format!(", segment {s}")),
                people(n)
            )),
            (Some(audience), None) => lines.push(format!("To: {audience}.")),
            (None, _) => lines.push(
                "No audience chosen yet: pick one in the studio, or draft again with audience_id (email_audiences lists them)."
                    .to_string(),
            ),
        }
        let warnings: Vec<String> = preview
            .as_ref()
            .and_then(|p| p.get("warnings"))
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        for warning in &warnings {
            lines.push(format!("Check: {warning}"));
        }
        let url = campaign_url(&campaign_id.to_string());
        lines.push(format!("Edit and preview it in the studio: {url}"));
        lines.push(format!(
            "Next: email_test with campaign_id {campaign_id} sends a test to the person; email_send only when they say go."
        ));
        Ok(ToolResult::with_structured(
            lines.join("\n"),
            json!({
                "campaign_id": campaign_id,
                "workspace_id": workspace_id,
                "name": name,
                "status": campaign.get("status"),
                "subject": subject,
                "preheader": preheader,
                "suggestions": suggestions,
                "outline": blocks,
                "audience": campaign.get("audience"),
                "segment": campaign.get("segment"),
                "recipients": recipients,
                "warnings": warnings,
                "studio_url": url,
                "next": {"action": "email_test", "campaign_id": campaign_id},
            }),
        ))
    }

    // ------------------------------------------------------------------------------------
    // email_campaigns
    // ------------------------------------------------------------------------------------

    async fn email_campaigns(&self, workspace_id: Uuid, input: &StudioInput) -> Result<ToolResult> {
        let status = input
            .status
            .as_deref()
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty());
        if let Some(status) = &status {
            if !CAMPAIGN_STATUSES.contains(&status.as_str()) {
                return Err(Error::Validation(format!(
                    "status is one of {}",
                    CAMPAIGN_STATUSES.join(", ")
                )));
            }
        }
        let mut path = format!("/campaigns?workspaceId={workspace_id}");
        if let Some(status) = &status {
            path.push_str(&format!("&status={status}"));
        }
        let answer = self.email_call("GET", &path, None, None).await?;
        if !answer.is_success() {
            return Ok(ToolResult::error(email_refusal(&answer, Topic::Other)));
        }
        let limit = input
            .limit
            .map(|l| l.clamp(1, CAMPAIGNS_LIMIT_MAX as i64) as usize)
            .unwrap_or(CAMPAIGNS_LIMIT_DEFAULT);
        let all = answer.data.as_array().cloned().unwrap_or_default();
        if all.is_empty() {
            return Ok(ToolResult::with_structured(
                match &status {
                    Some(status) => format!("No {status} campaigns in this workspace."),
                    None => {
                        "No campaigns in this workspace yet: email_draft writes one from a prompt."
                            .to_string()
                    }
                },
                json!({"workspace_id": workspace_id, "campaigns": []}),
            ));
        }
        let shown: Vec<&Value> = all.iter().take(limit).collect();
        let mut lines = vec![if all.len() > shown.len() {
            format!("The {} newest of {} campaigns:", shown.len(), all.len())
        } else {
            format!(
                "{} campaign{}:",
                all.len(),
                if all.len() == 1 { "" } else { "s" }
            )
        }];
        let mut listed = Vec::new();
        for campaign in shown {
            let id = text(campaign, "id").unwrap_or_default();
            let name = text(campaign, "name").unwrap_or_else(|| "A campaign".to_string());
            lines.push(format!(
                "- {name}: {} (campaign_id {id})",
                campaign_state(campaign)
            ));
            listed.push(json!({
                "campaign_id": id,
                "name": name,
                "status": campaign.get("status"),
                "subject": campaign.get("subject"),
                "audience": campaign.get("audience"),
                "schedule": campaign.get("schedule"),
                "progress": campaign.get("progress"),
                "stats": campaign.get("stats"),
                "last_error": campaign.get("lastError"),
            }));
        }
        Ok(ToolResult::with_structured(
            lines.join("\n"),
            json!({"workspace_id": workspace_id, "campaigns": listed}),
        ))
    }

    // ------------------------------------------------------------------------------------
    // email_test
    // ------------------------------------------------------------------------------------

    async fn email_test(&self, workspace_id: Uuid, input: &StudioInput) -> Result<ToolResult> {
        let campaign_id = Self::campaign_id(input, "email_test")?;
        let to = test_recipients(input.to.as_ref())?;
        if to.iter().any(|address| !is_resend_test_address(address)) {
            let me = self
                .client
                .studio_call("GET", "/auth/me", None, None, CALL_TIMEOUT)
                .await
                .map_err(unreachable)?;
            let own = me
                .data
                .get("email")
                .and_then(Value::as_str)
                .map(|e| e.trim().to_lowercase());
            let refused: Vec<&String> = to
                .iter()
                .filter(|address| {
                    !is_resend_test_address(address) && Some(address.as_str()) != own.as_deref()
                })
                .collect();
            if !refused.is_empty() {
                return Ok(ToolResult::error(format!(
                    "Not sent: test sends go to the person's own address or Resend's test addresses (delivered@resend.dev, bounced@resend.dev, complained@resend.dev), not {}.",
                    refused
                        .iter()
                        .map(|a| a.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
        }
        let mut body = json!({"workspaceId": workspace_id});
        if !to.is_empty() {
            body["to"] = json!(to);
        }
        let answer = self
            .email_call(
                "POST",
                &format!("/campaigns/{campaign_id}/test"),
                Some(&body),
                None,
            )
            .await?;
        if !answer.is_success() {
            return Ok(ToolResult::error(format!(
                "Test not sent: {}",
                email_refusal(&answer, Topic::Campaign)
            )));
        }
        let sent = answer
            .data
            .get("sent")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let addresses: Vec<String> = sent.iter().filter_map(|s| text(s, "email")).collect();
        let said = if addresses.is_empty() {
            "Test sent, with its subject marked [Test].".to_string()
        } else {
            format!(
                "Test sent to {}, with its subject marked [Test]. What Resend says of it shows in the campaign's test list in the studio: {}",
                addresses.join(", "),
                campaign_url(&campaign_id.to_string())
            )
        };
        Ok(ToolResult::with_structured(
            said,
            json!({"campaign_id": campaign_id, "sent": sent}),
        ))
    }

    // ------------------------------------------------------------------------------------
    // email_send
    // ------------------------------------------------------------------------------------

    async fn email_send(&self, workspace_id: Uuid, input: &StudioInput) -> Result<ToolResult> {
        let campaign_id = Self::campaign_id(input, "email_send")?;
        let send_at = send_at_of(input.send_at.as_deref())?;
        let time_zone = time_zone_of(input.time_zone.as_deref(), &send_at)?;
        let campaign = match self.read_campaign(workspace_id, campaign_id).await? {
            Ok(campaign) => campaign,
            Err(refused) => return Ok(ToolResult::error(format!("Not sent: {refused}"))),
        };
        let name = text(&campaign, "name").unwrap_or_else(|| "The campaign".to_string());
        let status = text(&campaign, "status").unwrap_or_default();
        match status.as_str() {
            "sending" => {
                return Ok(ToolResult::error(format!(
                    "Not sent again: “{name}” is sending now. email_report follows it; email_cancel stops it."
                )))
            }
            "sent" => {
                return Ok(ToolResult::error(format!(
                    "Not sent again: “{name}” was sent. email_report shows how it did."
                )))
            }
            _ => {}
        }
        let revision = campaign
            .get("revision")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let preview = self.preview(workspace_id, campaign_id).await;
        let recipients = preview
            .as_ref()
            .and_then(|p| p.get("recipients"))
            .and_then(Value::as_i64);
        let Some(recipients) = recipients.filter(|n| *n > 0) else {
            return Ok(ToolResult::error(format!(
                "Not sent: “{name}” has no one to send to yet. Choose an audience with subscribed contacts in the studio: {}",
                campaign_url(&campaign_id.to_string())
            )));
        };
        let subject = preview
            .as_ref()
            .and_then(|p| text(p, "subject"))
            .or_else(|| text(&campaign, "subject"))
            .unwrap_or_default();
        let from = preview.as_ref().and_then(|p| text(p, "from"));
        let when = when_words(&send_at, &time_zone);
        let what = format!(
            "“{name}” (subject “{subject}”{}) to {} {when}",
            from.as_ref()
                .map_or(String::new(), |f| format!(", from {f}")),
            people(recipients)
        );

        // The person's go: asked of them when the client can, else the count they were shown.
        let mut notes: Vec<String> = Vec::new();
        let asked = match peer::current().filter(peer::CallPeer::can_elicit) {
            Some(peer) => {
                let message = format!("Send {what}? Once it goes, it can't be unsent.");
                let schema = json!({
                    "type": "object",
                    "properties": {
                        "confirm": {
                            "type": "boolean",
                            "title": "Send it",
                            "description": format!("Send to {}.", people(recipients)),
                            "default": false
                        }
                    },
                    "required": ["confirm"]
                });
                // What the question is about: an answer given for another campaign, count or
                // time is not taken.
                let state =
                    format!("studio-email-send:{campaign_id}:{recipients}:{send_at}:{time_zone}");
                match peer
                    .ask("confirm-send", &message, schema, &state, ELICIT_TIMEOUT)
                    .await
                {
                    Elicited::Pending => {
                        return Ok(ToolResult::with_structured(
                            format!("Asking the person before {what} goes."),
                            json!({"sent": false, "campaign_id": campaign_id, "asking": true}),
                        ))
                    }
                    Elicited::Accepted(content)
                        if content.get("confirm").and_then(Value::as_bool) == Some(true) =>
                    {
                        notes.push("The person said go when the client asked them.".to_string());
                        Some(true)
                    }
                    Elicited::Accepted(_) | Elicited::Declined | Elicited::Cancelled => Some(false),
                    Elicited::Unanswered(reason) => {
                        tracing::debug!(%reason, "studio: the send question got no answer");
                        None
                    }
                }
            }
            None => None,
        };
        match (asked, input.confirm_recipients) {
            (Some(true), _) => {}
            (Some(false), _) => {
                return Ok(ToolResult::with_structured(
                    format!(
                        "Not sent: the person didn't say go to sending {what}. Nothing was sent."
                    ),
                    json!({"sent": false, "campaign_id": campaign_id, "declined": true}),
                ))
            }
            (None, Some(confirmed)) if confirmed == recipients => {}
            (None, confirmed) => {
                let mut said = match confirmed {
                    Some(confirmed) => format!(
                        "Not sent: it now goes to {}, not the {} the person agreed to.",
                        people(recipients),
                        credits(confirmed)
                    ),
                    None => "Not sent yet: the person's go is needed first.".to_string(),
                };
                said.push_str(&format!(
                    " Show them: {what}. If they agree, call email_send again with campaign_id {campaign_id}, send_at {send_at}, time_zone {time_zone} and confirm_recipients {recipients}."
                ));
                return Ok(ToolResult::with_structured(
                    said,
                    json!({
                        "sent": false,
                        "campaign_id": campaign_id,
                        "recipients": recipients,
                        "subject": subject,
                        "from": from,
                        "send_at": send_at,
                        "time_zone": time_zone,
                        "confirm_with": {
                            "action": "email_send",
                            "campaign_id": campaign_id,
                            "send_at": send_at,
                            "time_zone": time_zone,
                            "confirm_recipients": recipients,
                        },
                    }),
                ));
            }
        }

        let key = send_key(campaign_id, revision, &send_at, &time_zone);
        let body = json!({
            "workspaceId": workspace_id,
            "sendAt": send_at,
            "timeZone": time_zone,
            "revision": revision,
        });
        let answer = self
            .email_call(
                "POST",
                &format!("/campaigns/{campaign_id}/schedule"),
                Some(&body),
                Some(&key),
            )
            .await?;
        if !answer.is_success() {
            let refused = email_refusal(&answer, Topic::Campaign);
            let refused = if answer.status == 409 && refused.to_lowercase().contains("changed") {
                format!("{refused} Show the person the campaign as it is now and ask again.")
            } else {
                refused
            };
            return Ok(ToolResult::error(format!("Not sent: {refused}")));
        }
        let scheduled = answer.data;
        let state = text(&scheduled, "status").unwrap_or_default();
        if !matches!(state.as_str(), "scheduled" | "sending" | "sent") {
            // A repeated key answers the campaign as it is: never say it went when it didn't.
            return Ok(ToolResult::error(format!(
                "Not sent: Create kept “{name}” as {}. Read it with email_campaigns and ask the person again.",
                if state.is_empty() { "it was" } else { state.as_str() }
            )));
        }
        let mut lines = vec![if send_at == "now" {
            format!(
                "Sending “{name}” to {} now: it starts within a minute.",
                people(recipients)
            )
        } else {
            format!(
                "Scheduled “{name}” to {} {when}{}.",
                people(recipients),
                moment(
                    scheduled
                        .pointer("/schedule/sendAt")
                        .and_then(Value::as_str)
                )
                .map_or(String::new(), |at| format!(", that is {at}"))
            )
        }];
        lines.extend(notes);
        lines.push(format!(
            "email_cancel with campaign_id {campaign_id} stops it; email_report follows it."
        ));
        Ok(ToolResult::with_structured(
            lines.join("\n"),
            json!({
                "sent": true,
                "campaign_id": campaign_id,
                "status": state,
                "recipients": recipients,
                "schedule": scheduled.get("schedule"),
                "studio_url": campaign_url(&campaign_id.to_string()),
                "follow_up": {"action": "email_report", "campaign_id": campaign_id},
            }),
        ))
    }

    // ------------------------------------------------------------------------------------
    // email_cancel
    // ------------------------------------------------------------------------------------

    async fn email_cancel(&self, workspace_id: Uuid, input: &StudioInput) -> Result<ToolResult> {
        let campaign_id = Self::campaign_id(input, "email_cancel")?;
        let answer = self
            .email_call(
                "POST",
                &format!("/campaigns/{campaign_id}/cancel"),
                Some(&json!({"workspaceId": workspace_id})),
                None,
            )
            .await?;
        if !answer.is_success() {
            return Ok(ToolResult::error(format!(
                "Not cancelled: {}",
                email_refusal(&answer, Topic::Campaign)
            )));
        }
        let campaign = answer.data;
        let name = text(&campaign, "name").unwrap_or_else(|| "The campaign".to_string());
        let sent = count(&campaign, "/progress/sent");
        let said = match campaign.get("status").and_then(Value::as_str) {
            Some("cancelled") => format!(
                "Stopped “{name}”: {} sent before it stopped; no one else will get it.",
                credits(sent)
            ),
            _ => format!("Unscheduled “{name}”: it is a draft again, and nothing was sent."),
        };
        Ok(ToolResult::with_structured(
            said,
            json!({"campaign_id": campaign_id, "status": campaign.get("status"), "sent": sent}),
        ))
    }

    // ------------------------------------------------------------------------------------
    // email_report
    // ------------------------------------------------------------------------------------

    async fn email_report(&self, workspace_id: Uuid, input: &StudioInput) -> Result<ToolResult> {
        if input
            .campaign_id
            .as_deref()
            .is_none_or(|id| id.trim().is_empty())
        {
            return self.email_trends(workspace_id, input).await;
        }
        let campaign_id = Self::campaign_id(input, "email_report")?;
        let campaign = match self.read_campaign(workspace_id, campaign_id).await? {
            Ok(campaign) => campaign,
            Err(refused) => return Ok(ToolResult::error(refused)),
        };
        let answer = self
            .email_call(
                "GET",
                &format!("/campaigns/{campaign_id}/report?workspaceId={workspace_id}"),
                None,
                None,
            )
            .await?;
        if !answer.is_success() {
            return Ok(ToolResult::error(email_refusal(&answer, Topic::Campaign)));
        }
        let report = answer.data;
        let name = text(&campaign, "name").unwrap_or_else(|| "The campaign".to_string());
        let status = text(&campaign, "status").unwrap_or_default();
        let sent = count(&report, "/totals/sent");
        let mut lines = vec![format!("“{name}”: {}.", campaign_state(&campaign))];
        if sent > 0 {
            let rate = |key: &str| {
                report
                    .pointer(&format!("/rates/{key}"))
                    .and_then(Value::as_f64)
                    .map_or(String::new(), |r| format!(" ({})", percent(r)))
            };
            lines.push(format!(
                "Sent {} · delivered {}{} · opened {}{} · clicked {}{}",
                credits(sent),
                credits(count(&report, "/totals/delivered")),
                rate("delivered"),
                credits(count(&report, "/totals/opened")),
                rate("opened"),
                credits(count(&report, "/totals/clicked")),
                rate("clicked")
            ));
            lines.push(format!(
                "Bounced {}{} · complaints {}{} · unsubscribed {}{}{}",
                credits(count(&report, "/totals/bounced")),
                rate("bounced"),
                credits(count(&report, "/totals/complained")),
                rate("complained"),
                credits(count(&report, "/totals/unsubscribed")),
                rate("unsubscribed"),
                match (
                    count(&report, "/totals/failed"),
                    count(&report, "/totals/skipped")
                ) {
                    (0, 0) => String::new(),
                    (failed, skipped) => format!(
                        " · not sent: {} failed, {} skipped (suppressed or stopped)",
                        credits(failed),
                        credits(skipped)
                    ),
                }
            ));
        } else if status == "draft" || status == "scheduled" {
            lines.push("Nothing sent yet, so there is nothing to report.".to_string());
        }
        let links = report
            .get("links")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if !links.is_empty() {
            lines.push("Top links:".to_string());
            for link in links.iter().take(TOP_LINKS) {
                lines.push(format!(
                    "- {}: {} clicks by {}",
                    text(link, "url").unwrap_or_default(),
                    credits(count(link, "/clicks")),
                    people(count(link, "/people"))
                ));
            }
        }
        let bounces = report
            .get("bounces")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if !bounces.is_empty() {
            lines.push("Bounces:".to_string());
            for bounce in bounces.iter().take(LISTED_BOUNCES) {
                lines.push(format!(
                    "- {}{}{}",
                    text(bounce, "email").unwrap_or_default(),
                    text(bounce, "type").map_or(String::new(), |t| format!(" ({t})")),
                    text(bounce, "detail")
                        .map_or(String::new(), |d| format!(": {}", clip(&d, 160)))
                ));
            }
            if bounces.len() > LISTED_BOUNCES {
                lines.push(format!("  and {} more", bounces.len() - LISTED_BOUNCES));
            }
        }
        let complaints = report
            .get("complaints")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if !complaints.is_empty() {
            let shown: Vec<String> = complaints
                .iter()
                .take(LISTED_BOUNCES)
                .filter_map(|c| text(c, "email"))
                .collect();
            lines.push(format!(
                "Marked as spam by: {}{}",
                shown.join(", "),
                if complaints.len() > shown.len() {
                    format!(" and {} more", complaints.len() - shown.len())
                } else {
                    String::new()
                }
            ));
        }
        if !bounces.is_empty() || !complaints.is_empty() {
            lines.push(
                "Hard bounces and complaints are on the suppression list: they won't be sent to again."
                    .to_string(),
            );
        }
        lines.push(format!(
            "The full report: {}",
            campaign_url(&campaign_id.to_string())
        ));
        Ok(ToolResult::with_structured(
            lines.join("\n"),
            json!({
                "campaign_id": campaign_id,
                "name": name,
                "status": status,
                "totals": report.get("totals"),
                "rates": report.get("rates"),
                "links": links,
                "bounces": bounces,
                "complaints": complaints,
                "timeline": report.get("timeline"),
            }),
        ))
    }

    async fn email_trends(&self, workspace_id: Uuid, input: &StudioInput) -> Result<ToolResult> {
        let days = input.days.unwrap_or(STATS_DAYS_DEFAULT).clamp(7, 365);
        let answer = self
            .email_call(
                "GET",
                &format!("/stats?workspaceId={workspace_id}&days={days}"),
                None,
                None,
            )
            .await?;
        if !answer.is_success() {
            return Ok(ToolResult::error(email_refusal(&answer, Topic::Other)));
        }
        let stats = answer.data;
        let sent = count(&stats, "/totals/sent");
        let rate = |key: &str| {
            stats
                .pointer(&format!("/rates/{key}"))
                .and_then(Value::as_f64)
                .map_or(String::new(), |r| format!(" ({})", percent(r)))
        };
        let mut lines = vec![format!(
            "The last {days} days: {} campaign{}, sent {}.",
            credits(count(&stats, "/campaigns")),
            if count(&stats, "/campaigns") == 1 {
                ""
            } else {
                "s"
            },
            credits(sent)
        )];
        if sent > 0 {
            lines.push(format!(
                "Delivered {}{} · opened {}{} · clicked {}{} · bounced {}{} · complaints {}{} · unsubscribed {}{}",
                credits(count(&stats, "/totals/delivered")),
                rate("delivered"),
                credits(count(&stats, "/totals/opened")),
                rate("opened"),
                credits(count(&stats, "/totals/clicked")),
                rate("clicked"),
                credits(count(&stats, "/totals/bounced")),
                rate("bounced"),
                credits(count(&stats, "/totals/complained")),
                rate("complained"),
                credits(count(&stats, "/totals/unsubscribed")),
                rate("unsubscribed")
            ));
        }
        lines.push(format!(
            "Contacts: {}; on the suppression list: {}.",
            credits(count(&stats, "/contacts")),
            credits(count(&stats, "/suppressed"))
        ));
        lines.push("A campaign's own report: email_report with its campaign_id.".to_string());
        Ok(ToolResult::with_structured(
            lines.join("\n"),
            json!({
                "workspace_id": workspace_id,
                "days": days,
                "totals": stats.get("totals"),
                "rates": stats.get("rates"),
                "campaigns": stats.get("campaigns"),
                "contacts": stats.get("contacts"),
                "suppressed": stats.get("suppressed"),
                "by_day": stats.get("days"),
            }),
        ))
    }
}
