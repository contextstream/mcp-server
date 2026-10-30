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
//! - `email_preview`: a shareable link to the email in Gmail, Outlook and Apple Mail mockups
//!   (Create's preview links), with the inbox line and what to check.
//! - `email_revise`: changes a draft from plain words (`POST …/revise`), or sets its subject
//!   and preview line.
//! - `email_duplicate`: a draft copy of a campaign.
//! - `email_contacts`: the workspace's contacts: list and search, add (with where they agreed
//!   to hear from the team), erase one, or put one on the suppression list.
//! - `email_unsubscribes`: the suppression list with why and when; add to it; take an address
//!   off it, only with the person's go, as a send.
//!
//! Every action first reads Create's catalog: email is offered only where it has an `email`
//! block. Agents draft; people send. Resend is connected in the studio, never here: a key
//! typed into a chat stays in its transcript, so a key-shaped value is refused unread.

use mcp_client::studio::StudioAnswer;
use mcp_types::{tool::ToolResult, Error, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::api::{unreachable, CALL_TIMEOUT};
use super::words::{clip, credits};
use super::{parse_id, StudioInput, StudioTool, ELICIT_TIMEOUT, MAX_PROMPT_CHARS, TOOL_NAME};
use crate::domains::destructive::DELETE_TOOL;
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
    "email_preview",
    "email_revise",
    "email_duplicate",
    "email_contacts",
    "email_unsubscribes",
];

/// Erasing a contact, which `delete` calls (`studio.email_contact_erase`). It cannot be
/// undone, so it runs only through `delete` (#334's rule), never as an `email_contacts`
/// operation, and isn't listed among the studio's actions.
pub(crate) const ERASE_ACTION: &str = "email_contact_erase";
/// `email_contacts` operations that would erase a contact.
const ERASE_OPERATIONS: &[&str] = &["remove", "erase", "delete"];

/// Where the studio is, for links a person can open.
pub(crate) const STUDIO_APP: &str = "https://ctxproduct.com";
/// Where the team connects Resend: the key, the domains and the sender.
pub(crate) const CONNECT_URL: &str = "https://ctxproduct.com/studio/email/settings";
/// Where a person erases a contact themselves (the ContextStudio connector has no `delete`).
pub(crate) const AUDIENCES_URL: &str = "https://ctxproduct.com/studio/email/audiences";

/// Said when email isn't offered to the caller.
pub(crate) const NOT_OFFERED: &str = "Email isn't offered to this account yet: it opens with one-prompt making, team by team. Nothing was sent or changed.";
/// Said when ContextStream didn't take the sign-in.
pub(crate) const SIGN_IN: &str = "ContextStream didn't accept this sign-in: sign in again (or set a new API key), then try once more.";
/// Said when a call holds something shaped like a Resend API key.
pub(crate) const KEY_REFUSED: &str = "Not done: something in this call looks like a Resend API key. The studio tool never takes one, because a key typed into a chat stays in its transcript. The team connects Resend in the studio: https://ctxproduct.com/studio/email/settings. If that was a real key, rotate it in Resend now.";

const CAMPAIGN_STATUSES: &[&str] = &[
    "draft",
    "scheduled",
    "testing",
    "sending",
    "paused",
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
/// What `change` may say to `email_revise` (Create's limit).
const MAX_CHANGE_CHARS: usize = 1000;
/// A subject or preview line (Create's limit).
const MAX_LINE_CHARS: usize = 200;
/// The most addresses one `email_contacts` or `email_unsubscribes` add takes: the studio
/// imports CSV files for more.
const MAX_ADDRESSES: usize = 100;
const LIST_LIMIT_DEFAULT: i64 = 20;
const LIST_LIMIT_MAX: i64 = 50;
/// A note kept with a suppression change (Create's limit).
const MAX_NOTE_CHARS: usize = 500;
/// A shared preview link is reused only while it has at least this long left.
const LINK_REUSE_HOURS: i64 = 24;
/// The subjects of an A/B test, by variant.
const VARIANT_LABELS: [&str; 3] = ["A", "B", "C"];
const CONTACT_OPERATIONS: &[&str] = &["list", "add", "unsubscribe"];
const SUPPRESSION_OPERATIONS: &[&str] = &["list", "add", "remove"];
const CONSENT_SOURCES: &[&str] = &["manual", "form", "api"];
const SUPPRESSION_REASONS: &[&str] = &["unsubscribed", "bounced", "complained", "manual"];
const CONTACT_STATUSES: &[&str] = &["subscribed", "suppressed"];

/// Whether `text` holds something shaped like a Resend API key: `re_` at the start of a word
/// and twenty or more key characters after it.
pub(crate) fn looks_like_resend_key(text: &str) -> bool {
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(found) = text[from..].find("re_") {
        let at = from + found;
        let starts_word =
            at == 0 || !(bytes[at - 1].is_ascii_alphanumeric() || bytes[at - 1] == b'_');
        let run = text[at + 3..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .count();
        if starts_word && run >= 20 {
            return true;
        }
        from = at + 3;
    }
    false
}

/// Whether any string in `value`, at any depth, looks like a Resend API key.
pub(crate) fn holds_resend_key(value: &Value) -> bool {
    match value {
        Value::String(text) => looks_like_resend_key(text),
        Value::Array(items) => items.iter().any(holds_resend_key),
        Value::Object(map) => map
            .iter()
            .any(|(key, value)| looks_like_resend_key(key) || holds_resend_key(value)),
        _ => false,
    }
}

/// Whether the catalog offers email to this caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Offer {
    Offered,
    NotOffered,
    SignedOut,
}

/// Why `/preview` couldn't be read: Create's own sentence, or `None` when Create couldn't be
/// reached or answered with a server error.
type PreviewRefusal = Option<String>;

/// A short, stable digest of `value`, for idempotency keys.
fn digest(value: &str) -> String {
    hex::encode(&Sha256::digest(value.as_bytes())[..12])
}

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
        401 => SIGN_IN.to_string(),
        403 if said.contains("Create isn't available") => NOT_OFFERED.to_string(),
        404 if said.contains("isn't offered") => NOT_OFFERED.to_string(),
        404 if said.contains("No such workspace") => {
            "That workspace isn't one this account can use.".to_string()
        }
        404 => topic.missing().to_string(),
        429 if said.is_empty() => {
            "Too many requests for this workspace: try again in a minute.".to_string()
        }
        500..=599 if said.is_empty() => {
            "Create isn't answering right now: try again in a few minutes. Nothing was sent."
                .to_string()
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

/// What the person is asked to say go to: an answer given for another campaign, revision,
/// count or time is not taken, so a go on the stateless path can't carry over to an edited
/// campaign.
pub(crate) fn send_state(
    campaign_id: Uuid,
    revision: i64,
    recipients: i64,
    send_at: &str,
    time_zone: &str,
) -> String {
    format!("studio-email-send:{campaign_id}:{revision}:{recipients}:{send_at}:{time_zone}")
}

/// An A/B test's subject: 0 (A, the campaign's own) to 2 (C).
pub(crate) fn variant_of(raw: Option<i64>) -> Result<Option<i64>> {
    match raw {
        None => Ok(None),
        Some(variant) if (0..VARIANT_LABELS.len() as i64).contains(&variant) => Ok(Some(variant)),
        Some(_) => Err(Error::Validation(
            "variant is an A/B test's subject: 0 (A, the campaign's own), 1 (B) or 2 (C)"
                .to_string(),
        )),
    }
}

/// One of `allowed`, by name; `list` when none is given.
pub(crate) fn operation_of(raw: Option<&str>, allowed: &[&str], action: &str) -> Result<String> {
    let operation = raw
        .map(|o| o.trim().to_lowercase())
        .filter(|o| !o.is_empty())
        .unwrap_or_else(|| "list".to_string());
    if allowed.contains(&operation.as_str()) {
        Ok(operation)
    } else {
        Err(Error::Validation(format!(
            "operation for {action} is one of {}",
            allowed.join(", ")
        )))
    }
}

/// An email address as Create keeps it: trimmed and lower-cased, with one `@` between a local
/// part and a dotted domain.
pub(crate) fn address_of(raw: &str) -> Option<String> {
    let address = raw.trim().to_lowercase();
    let (local, domain) = address.split_once('@')?;
    let plausible = !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains('@')
        && address.len() <= 254
        && !address.chars().any(|c| c.is_whitespace() || c.is_control());
    plausible.then_some(address)
}

/// The refusal for `email_contacts` asked to erase a contact, worded as #334's refusals are:
/// it cannot be undone, so it runs only through `delete`.
pub(crate) fn erase_refusal(input: &StudioInput) -> Option<String> {
    let operation = input.operation.as_deref()?.trim().to_lowercase();
    ERASE_OPERATIONS.contains(&operation.as_str()).then(|| {
        format!(
            "email_contacts(operation=\"{operation}\") cannot be undone, so it runs only through the `{DELETE_TOOL}` tool: \
             call {DELETE_TOOL} with action=\"{TOOL_NAME}.{ERASE_ACTION}\" and the same email, after confirming with the person. \
             Where there is no `{DELETE_TOOL}` tool (the ContextStudio connector), the person erases the contact in the studio: {AUDIENCES_URL}"
        )
    })
}

/// The one address a call is about (`email`); `call` names it.
fn one_address(input: &StudioInput, call: &str) -> Result<String> {
    let raw = input
        .email
        .as_deref()
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .ok_or_else(|| {
            Error::Validation(format!(
                "email is required for {call}: the address it is about"
            ))
        })?;
    address_of(raw).ok_or_else(|| Error::Validation(format!("{raw} isn't an email address")))
}

/// A contact to add, as Create takes it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NewContact {
    pub email: String,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub tags: Vec<String>,
}

/// The contacts an add is about: `contacts` (addresses, or `{email, first_name, last_name,
/// tags}`), else `email`. Repeats are dropped; addresses that aren't one are returned apart,
/// so they can be named.
pub(crate) fn contacts_of(input: &StudioInput) -> Result<(Vec<NewContact>, Vec<String>)> {
    let items: Vec<Value> = match (&input.contacts, input.email.as_deref()) {
        (Some(Value::Array(items)), _) => items.clone(),
        (Some(Value::String(text)), _) => text
            .split([',', ';', '\n'])
            .map(|part| Value::String(part.to_string()))
            .collect(),
        (Some(Value::Null) | None, Some(email)) => vec![Value::String(email.to_string())],
        (Some(Value::Null) | None, None) => Vec::new(),
        (Some(_), _) => {
            return Err(Error::Validation(
                "contacts is a list of addresses or of {email, first_name, last_name, tags}"
                    .to_string(),
            ))
        }
    };
    let mut contacts: Vec<NewContact> = Vec::new();
    let mut invalid: Vec<String> = Vec::new();
    for item in items {
        let (raw, first_name, last_name, tags) =
            match &item {
                Value::String(text) => (text.clone(), None, None, Vec::new()),
                Value::Object(map) => {
                    let field = |key: &str| {
                        map.get(key)
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|v| !v.is_empty())
                            .map(str::to_string)
                    };
                    let tags = map
                        .get("tags")
                        .and_then(Value::as_array)
                        .map(|tags| {
                            tags.iter()
                                .filter_map(Value::as_str)
                                .map(|t| t.trim().to_string())
                                .filter(|t| !t.is_empty())
                                .collect()
                        })
                        .unwrap_or_default();
                    (
                        field("email").unwrap_or_default(),
                        field("first_name").or_else(|| field("firstName")),
                        field("last_name").or_else(|| field("lastName")),
                        tags,
                    )
                }
                _ => return Err(Error::Validation(
                    "contacts is a list of addresses or of {email, first_name, last_name, tags}"
                        .to_string(),
                )),
            };
        if raw.trim().is_empty() {
            continue;
        }
        match address_of(&raw) {
            Some(email) if contacts.iter().any(|c| c.email == email) => {}
            Some(email) => contacts.push(NewContact {
                email,
                first_name,
                last_name,
                tags,
            }),
            None => invalid.push(raw.trim().to_string()),
        }
    }
    if contacts.len() + invalid.len() > MAX_ADDRESSES {
        return Err(Error::Validation(format!(
            "At most {MAX_ADDRESSES} addresses a call; the studio imports CSV files for more ({STUDIO_APP}/studio/email/audiences)."
        )));
    }
    Ok((contacts, invalid))
}

/// A note for a suppression change, bounded as Create keeps it.
fn note_of(input: &StudioInput, said: &str) -> String {
    let own = input
        .note
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty());
    let note = match own {
        Some(own) => format!("{said}: {own}"),
        None => said.to_string(),
    };
    note.chars().take(MAX_NOTE_CHARS).collect()
}

/// Why an address is on the suppression list, in words.
pub(crate) fn suppression_words(suppression: &Value) -> String {
    let reason = suppression
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("");
    let source = suppression
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or("");
    let how = match (reason, source) {
        ("unsubscribed", "one_click") => "unsubscribed with their mail client's one-click button",
        ("unsubscribed", "page") => "unsubscribed on the unsubscribe page",
        ("unsubscribed", _) => "unsubscribed",
        ("bounced", _) => "hard-bounced",
        ("complained", _) => "marked an email as spam",
        ("manual", "api") => "added through the API",
        ("manual", _) => "added by hand",
        _ => "suppressed",
    };
    let mut words = how.to_string();
    if let Some(at) = moment(suppression.get("createdAt").and_then(Value::as_str)) {
        words.push_str(&format!(" on {at}"));
    }
    if let Some(name) = suppression
        .pointer("/campaign/name")
        .and_then(Value::as_str)
        .filter(|n| !n.trim().is_empty())
    {
        words.push_str(&format!(", from “{name}”"));
    }
    if let Some(feedback) = suppression.get("feedback").filter(|f| f.is_object()) {
        let said = match feedback.get("reason").and_then(Value::as_str).unwrap_or("") {
            "too_many" => "too many emails",
            "not_relevant" => "not relevant to them",
            "never_signed_up" => "they never signed up",
            _ => "another reason",
        };
        words.push_str(&format!("; said: {said}"));
        if let Some(comment) = text(feedback, "comment") {
            words.push_str(&format!(" (“{}”)", clip(&comment, 140)));
        }
    }
    if reason == "bounced" {
        if let Some(detail) = text(suppression, "detail") {
            words.push_str(&format!(": {}", clip(&detail, 140)));
        }
    }
    words
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
            format!("Resend isn't connected yet: the team connects its own Resend key in the studio ({CONNECT_URL}). An agent never takes the key, which would stay in this chat. Drafts and previews work before then."),
            json!({"connected": false, "connect_url": CONNECT_URL}),
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
            "Resend is connected (key ending {last4}), but campaigns can't be sent until the studio has {}: {CONNECT_URL}.",
            if words.is_empty() {
                "its settings finished".to_string()
            } else {
                words.join(", ")
            }
        )
    };
    (
        line,
        json!({"connected": true, "ready": ready, "missing": missing, "key_last4": last4, "connect_url": CONNECT_URL}),
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
        "testing" => match moment(campaign.pointer("/abTest/decideAt").and_then(Value::as_str)) {
            Some(at) => format!(
                "testing its subjects on {} people; the winner goes to the rest at {at}",
                credits(sent)
            ),
            None => format!("testing its subjects on {} people", credits(sent)),
        },
        "paused" => format!("paused: {} of {} sent", credits(sent), credits(recipients)),
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
        // Erasing can't be undone, so it runs only through `delete`: refused before anything
        // is asked of anyone.
        if action == "email_contacts" {
            if let Some(refusal) = erase_refusal(input) {
                return Err(Error::Validation(refusal));
            }
        }
        // Before anything is asked of anyone: a key never travels, not even to Create.
        if serde_json::to_value(input).is_ok_and(|raw| holds_resend_key(&raw)) {
            return Ok(ToolResult::error(KEY_REFUSED.to_string()));
        }
        match self.email_offer().await? {
            Offer::Offered => {}
            Offer::NotOffered => return Ok(ToolResult::error(NOT_OFFERED.to_string())),
            Offer::SignedOut => return Ok(ToolResult::error(SIGN_IN.to_string())),
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
            "email_preview" => self.email_preview(workspace_id, input).await,
            "email_revise" => self.email_revise(workspace_id, input).await,
            "email_duplicate" => self.email_duplicate(workspace_id, input).await,
            "email_contacts" => self.email_contacts(workspace_id, input).await,
            "email_unsubscribes" => self.email_unsubscribes(workspace_id, input).await,
            ERASE_ACTION => self.erase_contact(workspace_id, input).await,
            other => Err(Error::Validation(format!("Unknown email action: {other}"))),
        }
    }

    /// Whether Create's catalog offers email to this caller. A 401 is a sign-in that expired
    /// or was refused, not an account email isn't offered to.
    async fn email_offer(&self) -> Result<Offer> {
        let answer = self
            .client
            .studio_call("GET", "/create/catalog", None, None, CALL_TIMEOUT)
            .await
            .map_err(unreachable)?;
        match answer.status {
            200..=299 if answer.data.get("email").is_some_and(Value::is_object) => {
                Ok(Offer::Offered)
            }
            200..=299 | 403 | 404 => Ok(Offer::NotOffered),
            401 => Ok(Offer::SignedOut),
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

    /// The campaign as it would go: who it goes to and what to check (`/preview`), or why it
    /// couldn't be read.
    async fn preview(
        &self,
        workspace_id: Uuid,
        campaign_id: Uuid,
        variant: Option<i64>,
    ) -> std::result::Result<Value, PreviewRefusal> {
        let mut path = format!("/campaigns/{campaign_id}/preview?workspaceId={workspace_id}");
        if let Some(variant) = variant {
            path.push_str(&format!("&variant={variant}"));
        }
        let answer = self
            .email_call("GET", &path, None, None)
            .await
            .map_err(|_| None)?;
        if answer.is_success() {
            Ok(answer.data)
        } else if answer.status >= 500 {
            Err(None)
        } else {
            Err(Some(email_refusal(&answer, Topic::Campaign)))
        }
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
        let preview = self.preview(workspace_id, campaign_id, None).await.ok();

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
            "Next, with campaign_id {campaign_id}: email_preview shows it in Gmail, Outlook and Apple Mail (a link to share); email_revise changes it from plain words; email_test sends a test to the person; email_send only when they say go."
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
        if let Some(variant) = variant_of(input.variant)? {
            body["variant"] = json!(variant);
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
            "testing" => {
                return Ok(ToolResult::error(format!(
                    "Not sent again: “{name}” is testing its subjects, and the rest go out with the winner on their own. email_report follows it; email_cancel stops it."
                )))
            }
            "paused" => {
                return Ok(ToolResult::error(format!(
                    "Not sent: “{name}” is paused. The person resumes it in the studio ({}), or email_cancel stops it for good.",
                    campaign_url(&campaign_id.to_string())
                )))
            }
            _ => {}
        }
        let revision = campaign
            .get("revision")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let preview = match self.preview(workspace_id, campaign_id, None).await {
            Ok(preview) => preview,
            Err(None) => {
                return Ok(ToolResult::error(format!(
                    "Not sent: Create couldn't be read just now, so who “{name}” would go to can't be checked. Nothing was sent; try again in a few minutes."
                )))
            }
            Err(Some(refused)) => {
                return Ok(ToolResult::error(format!(
                    "Not sent: Create couldn't read “{name}” as it would go: {refused}"
                )))
            }
        };
        let recipients = preview.get("recipients").and_then(Value::as_i64);
        let Some(recipients) = recipients.filter(|n| *n > 0) else {
            return Ok(ToolResult::error(format!(
                "Not sent: “{name}” has no one to send to yet. Choose an audience with subscribed contacts in the studio: {}",
                campaign_url(&campaign_id.to_string())
            )));
        };
        let subject = text(&preview, "subject")
            .or_else(|| text(&campaign, "subject"))
            .unwrap_or_default();
        let from = text(&preview, "from");
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
                // What the question is about: an answer given for another campaign, revision,
                // count or time is not taken.
                let state = send_state(campaign_id, revision, recipients, &send_at, &time_zone);
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

/// Whether a preview link is live and has at least `LINK_REUSE_HOURS` left.
fn link_reusable(link: &Value) -> bool {
    let live =
        link.get("live").and_then(Value::as_bool) == Some(true) && text(link, "url").is_some();
    let lasting = link
        .get("expiresAt")
        .and_then(Value::as_str)
        .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
        .is_some_and(|at| {
            at.with_timezone(&chrono::Utc)
                > chrono::Utc::now() + chrono::Duration::hours(LINK_REUSE_HOURS)
        });
    live && lasting
}

/// "A “…”, B “…”": an A/B test's subjects in order.
fn subjects_words(subjects: &[String]) -> String {
    subjects
        .iter()
        .zip(VARIANT_LABELS)
        .map(|(subject, label)| format!("{label} “{subject}”"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

impl StudioTool {
    // ------------------------------------------------------------------------------------
    // email_preview
    // ------------------------------------------------------------------------------------

    /// The newest shared preview link with a day or more left, else a new one.
    async fn preview_link(
        &self,
        workspace_id: Uuid,
        campaign_id: Uuid,
    ) -> std::result::Result<Value, String> {
        let listed = self
            .email_call(
                "GET",
                &format!("/campaigns/{campaign_id}/preview-links?workspaceId={workspace_id}"),
                None,
                None,
            )
            .await;
        if let Ok(answer) = &listed {
            if answer.is_success() {
                if let Some(link) = answer
                    .data
                    .get("links")
                    .and_then(Value::as_array)
                    .and_then(|links| links.iter().find(|link| link_reusable(link)))
                {
                    return Ok(link.clone());
                }
            }
        }
        let made = self
            .email_call(
                "POST",
                &format!("/campaigns/{campaign_id}/preview-links"),
                Some(&json!({"workspaceId": workspace_id})),
                None,
            )
            .await
            .map_err(|_| "Create didn't answer.".to_string())?;
        if made.is_success() {
            Ok(made.data)
        } else {
            Err(email_refusal(&made, Topic::Campaign))
        }
    }

    async fn email_preview(&self, workspace_id: Uuid, input: &StudioInput) -> Result<ToolResult> {
        let campaign_id = Self::campaign_id(input, "email_preview")?;
        let variant = variant_of(input.variant)?;
        let campaign = match self.read_campaign(workspace_id, campaign_id).await? {
            Ok(campaign) => campaign,
            Err(refused) => return Ok(ToolResult::error(format!("No preview: {refused}"))),
        };
        let name = text(&campaign, "name").unwrap_or_else(|| "The campaign".to_string());
        let preview = match self.preview(workspace_id, campaign_id, variant).await {
            Ok(preview) => preview,
            Err(None) => {
                return Ok(ToolResult::error(format!(
                    "No preview: Create couldn't be read just now for “{name}”. Try again in a few minutes."
                )))
            }
            Err(Some(refused)) => return Ok(ToolResult::error(format!("No preview: {refused}"))),
        };
        let link = self.preview_link(workspace_id, campaign_id).await;

        let subject = text(&preview, "subject").unwrap_or_default();
        let preheader = text(&preview, "preheader").unwrap_or_default();
        let from = text(&preview, "from");
        let subjects = strings(preview.get("subjects"));
        let warnings = strings(preview.get("warnings"));
        let recipients = preview.get("recipients").and_then(Value::as_i64);
        let mut lines = Vec::new();
        let (url, expires_at, link_id) = match &link {
            Ok(link) => {
                let url = text(link, "url");
                let expires = text(link, "expiresAt");
                match &url {
                    Some(url) => {
                        lines.push(format!("Preview of “{name}”: {url}"));
                        lines.push(format!(
                            "It shows the email in Gmail, Outlook and Apple Mail on desktop, tablet and phone, light and dark, with images on or off. Anyone with the link can open it{}; it can be turned off in the studio.",
                            moment(expires.as_deref()).map_or(String::new(), |at| format!(" until {at}"))
                        ));
                    }
                    None => lines.push(format!(
                        "Preview of “{name}”: Create made no link this time. The studio shows the same mockups: {}",
                        campaign_url(&campaign_id.to_string())
                    )),
                }
                (url, expires, text(link, "id"))
            }
            Err(refused) => {
                lines.push(format!(
                    "Preview of “{name}”: no shareable link ({refused}). The studio shows the same mockups: {}",
                    campaign_url(&campaign_id.to_string())
                ));
                (None, None, None)
            }
        };
        lines.push(format!(
            "Inbox line: {} · “{subject}”{}",
            from.clone()
                .unwrap_or_else(|| format!("no sender yet (set one in the studio: {CONNECT_URL})")),
            if preheader.is_empty() {
                String::new()
            } else {
                format!(" · {preheader}")
            }
        ));
        if subjects.len() > 1 {
            lines.push(format!(
                "A/B subjects: {}{}",
                subjects_words(&subjects),
                variant.map_or(String::new(), |v| format!(
                    " (this is {})",
                    VARIANT_LABELS[v as usize]
                ))
            ));
        }
        if let (Some(audience), Some(n)) = (
            campaign.pointer("/audience/name").and_then(Value::as_str),
            recipients,
        ) {
            lines.push(format!("To: {audience} ({}).", people(n)));
        }
        for warning in &warnings {
            lines.push(format!("Check: {warning}"));
        }
        Ok(ToolResult::with_structured(
            lines.join("\n"),
            json!({
                "campaign_id": campaign_id,
                "name": name,
                "preview_url": url,
                "expires_at": expires_at,
                "link_id": link_id,
                "link_problem": link.as_ref().err(),
                "from": from,
                "from_name": preview.get("fromName"),
                "from_email": preview.get("fromEmail"),
                "subject": subject,
                "preheader": preheader,
                "subjects": subjects,
                "variant": variant,
                "recipients": recipients,
                "warnings": warnings,
                "studio_url": campaign_url(&campaign_id.to_string()),
            }),
        ))
    }

    // ------------------------------------------------------------------------------------
    // email_revise
    // ------------------------------------------------------------------------------------

    async fn email_revise(&self, workspace_id: Uuid, input: &StudioInput) -> Result<ToolResult> {
        let campaign_id = Self::campaign_id(input, "email_revise")?;
        let change = input
            .change
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty());
        let subject = input.subject.as_deref().map(str::trim);
        let preheader = input.preheader.as_deref().map(str::trim);
        if change.is_none() && subject.is_none() && preheader.is_none() {
            return Err(Error::Validation(
                "email_revise needs change (plain words, e.g. \"shorter subject\" or \"add a button to https://acme.com/pricing\"), or subject and/or preheader to set".to_string(),
            ));
        }
        if change.is_some() && (subject.is_some() || preheader.is_some()) {
            return Err(Error::Validation(
                "Pass change, or subject and preheader, not both: call email_revise once for each."
                    .to_string(),
            ));
        }
        if change.is_some_and(|c| c.chars().count() > MAX_CHANGE_CHARS) {
            return Err(Error::Validation(format!(
                "change can be up to {MAX_CHANGE_CHARS} characters."
            )));
        }
        if subject.is_some_and(|s| s.is_empty()) {
            return Err(Error::Validation("subject can't be empty.".to_string()));
        }
        if subject.is_some_and(|s| s.chars().count() > MAX_LINE_CHARS)
            || preheader.is_some_and(|p| p.chars().count() > MAX_LINE_CHARS)
        {
            return Err(Error::Validation(format!(
                "A subject or preheader can be up to {MAX_LINE_CHARS} characters."
            )));
        }
        let campaign = match self.read_campaign(workspace_id, campaign_id).await? {
            Ok(campaign) => campaign,
            Err(refused) => return Ok(ToolResult::error(format!("Not changed: {refused}"))),
        };
        let name = text(&campaign, "name").unwrap_or_else(|| "The campaign".to_string());
        let status = text(&campaign, "status").unwrap_or_default();
        if status != "draft" {
            let how = match status.as_str() {
                "scheduled" => "email_cancel unschedules it, and it is a draft again.",
                "sending" | "testing" | "paused" => {
                    "It has started sending; email_duplicate makes a draft copy to change."
                }
                _ => "email_duplicate makes a draft copy to change.",
            };
            return Ok(ToolResult::error(format!(
                "Not changed: only a draft can be changed, and “{name}” is {status}. {how}"
            )));
        }
        let revision = campaign
            .get("revision")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let answer = match change {
            Some(change) => {
                let key = format!("email-revise-{campaign_id}-r{revision}-{}", digest(change));
                self.email_call(
                    "POST",
                    &format!("/campaigns/{campaign_id}/revise"),
                    Some(&json!({
                        "workspaceId": workspace_id,
                        "instruction": change,
                        "revision": revision,
                    })),
                    Some(&key),
                )
                .await?
            }
            None => {
                let mut body = json!({"workspaceId": workspace_id, "revision": revision});
                if let Some(subject) = subject {
                    body["subject"] = json!(subject);
                }
                if let Some(preheader) = preheader {
                    body["preheader"] = json!(preheader);
                }
                self.email_call(
                    "PATCH",
                    &format!("/campaigns/{campaign_id}"),
                    Some(&body),
                    None,
                )
                .await?
            }
        };
        if !answer.is_success() {
            let refused = email_refusal(&answer, Topic::Campaign);
            let refused = if answer.status == 409 {
                format!("{refused} Read it again with email_campaigns, then change it once more.")
            } else {
                refused
            };
            return Ok(ToolResult::error(format!("Not changed: {refused}")));
        }
        // `/revise` answers {campaign, outline, changes}; PATCH answers the campaign.
        let revised = answer
            .data
            .get("campaign")
            .cloned()
            .unwrap_or_else(|| answer.data.clone());
        let changes = strings(answer.data.get("changes"));
        let new_revision = revised.get("revision").and_then(Value::as_i64);
        let new_subject = text(&revised, "subject").unwrap_or_default();
        let new_preheader = text(&revised, "preheader").unwrap_or_default();
        let blocks = outline(revised.get("content").unwrap_or(&Value::Null));
        let mut lines = vec![format!(
            "Changed “{name}”{}. Nothing is sent.",
            new_revision.map_or(String::new(), |r| format!(" (revision {r})"))
        )];
        if change.is_some() {
            if changes.is_empty() {
                lines.push("The studio rewrote it as asked.".to_string());
            }
            for said in &changes {
                lines.push(format!("- {said}"));
            }
        }
        lines.push(format!("Subject: {new_subject}"));
        if !new_preheader.is_empty() {
            lines.push(format!("Preheader: {new_preheader}"));
        }
        if change.is_some() && !blocks.is_empty() {
            lines.push(String::new());
            lines.push("Outline:".to_string());
            lines.extend(blocks.iter().cloned());
        }
        lines.push(format!(
            "See it in the mail-client mockups with email_preview; edit it in the studio: {}",
            campaign_url(&campaign_id.to_string())
        ));
        Ok(ToolResult::with_structured(
            lines.join("\n"),
            json!({
                "campaign_id": campaign_id,
                "revision": new_revision,
                "subject": new_subject,
                "preheader": new_preheader,
                "changes": changes,
                "outline": blocks,
                "studio_url": campaign_url(&campaign_id.to_string()),
            }),
        ))
    }

    // ------------------------------------------------------------------------------------
    // email_duplicate
    // ------------------------------------------------------------------------------------

    async fn email_duplicate(&self, workspace_id: Uuid, input: &StudioInput) -> Result<ToolResult> {
        let campaign_id = Self::campaign_id(input, "email_duplicate")?;
        let answer = self
            .email_call(
                "POST",
                &format!("/campaigns/{campaign_id}/duplicate"),
                Some(&json!({"workspaceId": workspace_id})),
                None,
            )
            .await?;
        if !answer.is_success() {
            return Ok(ToolResult::error(format!(
                "Not duplicated: {}",
                email_refusal(&answer, Topic::Campaign)
            )));
        }
        let copy = answer.data;
        let copy_id = text(&copy, "id").unwrap_or_default();
        let name = text(&copy, "name").unwrap_or_else(|| "The copy".to_string());
        let url = campaign_url(&copy_id);
        Ok(ToolResult::with_structured(
            format!(
                "Made “{name}”, a draft copy (campaign_id {copy_id}). Nothing is sent.\nSubject: {}\nChange it with email_revise, or in the studio: {url}",
                text(&copy, "subject").unwrap_or_default()
            ),
            json!({
                "campaign_id": copy_id,
                "duplicated_from": campaign_id,
                "name": name,
                "status": copy.get("status"),
                "subject": copy.get("subject"),
                "studio_url": url,
            }),
        ))
    }

    // ------------------------------------------------------------------------------------
    // email_contacts
    // ------------------------------------------------------------------------------------

    async fn email_contacts(&self, workspace_id: Uuid, input: &StudioInput) -> Result<ToolResult> {
        let operation = operation_of(
            input.operation.as_deref(),
            CONTACT_OPERATIONS,
            "email_contacts",
        )?;
        match operation.as_str() {
            "add" => self.add_contacts(workspace_id, input).await,
            "unsubscribe" => {
                let email = one_address(input, "email_contacts with operation unsubscribe")?;
                self.suppress(workspace_id, input, vec![email]).await
            }
            _ => self.list_contacts(workspace_id, input).await,
        }
    }

    async fn list_contacts(&self, workspace_id: Uuid, input: &StudioInput) -> Result<ToolResult> {
        let limit = input
            .limit
            .unwrap_or(LIST_LIMIT_DEFAULT)
            .clamp(1, LIST_LIMIT_MAX);
        let mut path = format!("/contacts?workspaceId={workspace_id}&limit={limit}");
        let query = input
            .query
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty());
        if let Some(query) = query {
            path.push_str(&format!("&q={}", urlencoding::encode(query)));
        }
        if let Some(audience) = input
            .audience_id
            .as_deref()
            .filter(|a| !a.trim().is_empty())
        {
            path.push_str(&format!(
                "&audienceId={}",
                parse_id(audience, "audience_id")?
            ));
        }
        let status = input
            .status
            .as_deref()
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty());
        if let Some(status) = &status {
            if !CONTACT_STATUSES.contains(&status.as_str()) {
                return Err(Error::Validation(
                    "status for email_contacts is subscribed or suppressed".to_string(),
                ));
            }
            path.push_str(&format!("&status={status}"));
        }
        let answer = self.email_call("GET", &path, None, None).await?;
        if !answer.is_success() {
            return Ok(ToolResult::error(email_refusal(&answer, Topic::Audience)));
        }
        let contacts = answer
            .data
            .get("contacts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let total = answer
            .data
            .get("total")
            .and_then(Value::as_i64)
            .unwrap_or(contacts.len() as i64);
        let audiences: Vec<Value> = self
            .email_call(
                "GET",
                &format!("/audiences?workspaceId={workspace_id}"),
                None,
                None,
            )
            .await
            .ok()
            .filter(StudioAnswer::is_success)
            .and_then(|answer| answer.data.as_array().cloned())
            .unwrap_or_default();
        let audience_name = |id: &str| {
            audiences
                .iter()
                .find(|a| text(a, "id").as_deref() == Some(id))
                .and_then(|a| text(a, "name"))
        };
        let scope = query.map_or(String::new(), |q| format!(" matching “{q}”"));
        if contacts.is_empty() {
            return Ok(ToolResult::with_structured(
                format!(
                    "No contacts{scope}. Add some with email_contacts operation add, or import a CSV in the studio: {STUDIO_APP}/studio/email/audiences"
                ),
                json!({"workspace_id": workspace_id, "contacts": [], "total": 0}),
            ));
        }
        let mut lines = vec![if total > contacts.len() as i64 {
            format!(
                "{} contacts{scope}; the first {}:",
                credits(total),
                contacts.len()
            )
        } else {
            format!(
                "{} contact{}{scope}:",
                credits(total),
                if total == 1 { "" } else { "s" }
            )
        }];
        let mut listed = Vec::new();
        for contact in &contacts {
            let email = text(contact, "email").unwrap_or_default();
            let full_name = [text(contact, "firstName"), text(contact, "lastName")]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ");
            let mut parts = vec![if full_name.is_empty() {
                email.clone()
            } else {
                format!("{email} ({full_name})")
            }];
            match contact.get("suppressed").filter(|s| s.is_object()) {
                Some(suppressed) => parts.push(format!(
                    "not sent to: {}{}",
                    suppressed
                        .get("reason")
                        .and_then(Value::as_str)
                        .unwrap_or("suppressed"),
                    moment(suppressed.get("at").and_then(Value::as_str))
                        .map_or(String::new(), |at| format!(" {at}"))
                )),
                None => parts.push("subscribed".to_string()),
            }
            let names: Vec<String> = strings(contact.get("audienceIds"))
                .iter()
                .filter_map(|id| audience_name(id))
                .collect();
            if !names.is_empty() {
                parts.push(format!("in {}", names.join(", ")));
            }
            let tags = strings(contact.get("tags"));
            if !tags.is_empty() {
                parts.push(format!("tags {}", tags.join(", ")));
            }
            if let Some(source) = contact.pointer("/consent/source").and_then(Value::as_str) {
                parts.push(format!(
                    "consent {source}{}",
                    contact
                        .pointer("/consent/note")
                        .and_then(Value::as_str)
                        .map_or(String::new(), |n| format!(": {}", clip(n, 80)))
                ));
            }
            lines.push(format!("- {}", parts.join(" · ")));
            listed.push(json!({
                "contact_id": contact.get("id"),
                "email": email,
                "first_name": contact.get("firstName"),
                "last_name": contact.get("lastName"),
                "tags": contact.get("tags"),
                "consent": contact.get("consent"),
                "audience_ids": contact.get("audienceIds"),
                "suppressed": contact.get("suppressed"),
            }));
        }
        Ok(ToolResult::with_structured(
            lines.join("\n"),
            json!({
                "workspace_id": workspace_id,
                "contacts": listed,
                "total": total,
                "next_cursor": answer.data.get("nextCursor"),
            }),
        ))
    }

    async fn add_contacts(&self, workspace_id: Uuid, input: &StudioInput) -> Result<ToolResult> {
        let (contacts, invalid) = contacts_of(input)?;
        if contacts.is_empty() {
            return Err(Error::Validation(if invalid.is_empty() {
                "contacts (or email) is required for email_contacts with operation add".to_string()
            } else {
                format!("None of these is an email address: {}", invalid.join(", "))
            }));
        }
        let source = input
            .consent_source
            .as_deref()
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                Error::Validation(
                    "consent_source is required to add contacts: manual (the person added them), form (they signed up on a form) or api".to_string(),
                )
            })?;
        if !CONSENT_SOURCES.contains(&source.as_str()) {
            return Err(Error::Validation(
                "consent_source is manual, form or api".to_string(),
            ));
        }
        let note = input
            .consent_note
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .ok_or_else(|| {
                Error::Validation(
                    "consent_note is required to add contacts: where these people agreed to hear from the team, e.g. \"Signed up at the Spring market\"".to_string(),
                )
            })?;
        if note.chars().count() > MAX_NOTE_CHARS {
            return Err(Error::Validation(format!(
                "consent_note can be up to {MAX_NOTE_CHARS} characters."
            )));
        }
        let mut body = json!({
            "workspaceId": workspace_id,
            "consentSource": source,
            "contacts": contacts.iter().map(|contact| {
                let mut row = json!({"email": contact.email, "consentNote": note});
                if let Some(first) = &contact.first_name { row["firstName"] = json!(first); }
                if let Some(last) = &contact.last_name { row["lastName"] = json!(last); }
                if !contact.tags.is_empty() { row["tags"] = json!(contact.tags); }
                row
            }).collect::<Vec<_>>(),
        });
        if let Some(audience) = input
            .audience_id
            .as_deref()
            .filter(|a| !a.trim().is_empty())
        {
            body["audienceId"] = json!(parse_id(audience, "audience_id")?);
        }
        let key = format!("email-contacts-{}", digest(&body.to_string()));
        let answer = self
            .email_call("POST", "/contacts", Some(&body), Some(&key))
            .await?;
        if !answer.is_success() {
            return Ok(ToolResult::error(format!(
                "Not added: {}",
                email_refusal(&answer, Topic::Audience)
            )));
        }
        let created = count(&answer.data, "/created");
        let updated = count(&answer.data, "/updated");
        let added = count(&answer.data, "/added");
        let suppressed = count(&answer.data, "/suppressed");
        let mut refused: Vec<String> = answer
            .data
            .get("invalid")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|item| {
                        format!(
                            "{} ({})",
                            text(item, "email").unwrap_or_default(),
                            text(item, "reason").unwrap_or_else(|| "refused".to_string())
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        refused.extend(
            invalid
                .iter()
                .map(|raw| format!("{raw} (isn't an email address)")),
        );
        let mut lines = vec![format!(
            "Added {} ({} new, {} updated){}, consent {source}: “{note}”.",
            people(created + updated),
            credits(created),
            credits(updated),
            if added > 0 {
                format!(", {} put in the audience", credits(added))
            } else {
                String::new()
            }
        )];
        if suppressed > 0 {
            lines.push(format!(
                "{} on the suppression list: kept, and never sent to.",
                if suppressed == 1 {
                    "1 is".to_string()
                } else {
                    format!("{} are", credits(suppressed))
                }
            ));
        }
        if !refused.is_empty() {
            lines.push(format!("Not added: {}", refused.join(", ")));
        }
        Ok(ToolResult::with_structured(
            lines.join("\n"),
            json!({
                "workspace_id": workspace_id,
                "created": created,
                "updated": updated,
                "added_to_audience": added,
                "suppressed": suppressed,
                "not_added": refused,
            }),
        ))
    }

    /// The contact with exactly this address, if any.
    async fn contact_by_address(
        &self,
        workspace_id: Uuid,
        email: &str,
    ) -> Result<std::result::Result<Option<Value>, String>> {
        let answer = self
            .email_call(
                "GET",
                &format!(
                    "/contacts?workspaceId={workspace_id}&limit=50&q={}",
                    urlencoding::encode(email)
                ),
                None,
                None,
            )
            .await?;
        if !answer.is_success() {
            return Ok(Err(email_refusal(&answer, Topic::Other)));
        }
        Ok(Ok(answer
            .data
            .get("contacts")
            .and_then(Value::as_array)
            .and_then(|items| {
                items
                    .iter()
                    .find(|c| text(c, "email").map(|e| e.to_lowercase()).as_deref() == Some(email))
                    .cloned()
            })))
    }

    /// Erases a contact: `delete` with `studio.email_contact_erase`, after the person's go.
    async fn erase_contact(&self, workspace_id: Uuid, input: &StudioInput) -> Result<ToolResult> {
        let email = one_address(
            input,
            &format!("{DELETE_TOOL}(action=\"{TOOL_NAME}.{ERASE_ACTION}\")"),
        )?;
        let contact = match self.contact_by_address(workspace_id, &email).await? {
            Ok(Some(contact)) => contact,
            Ok(None) => {
                return Ok(ToolResult::error(format!(
                    "Not erased: there is no contact {email} in this workspace."
                )))
            }
            Err(refused) => return Ok(ToolResult::error(format!("Not erased: {refused}"))),
        };
        let id = text(&contact, "id").unwrap_or_default();
        let id = parse_id(&id, "contact id")?;
        let answer = self
            .email_call(
                "DELETE",
                &format!("/contacts/{id}?workspaceId={workspace_id}"),
                None,
                None,
            )
            .await?;
        if !answer.is_success() {
            return Ok(ToolResult::error(format!(
                "Not erased: {}",
                email_refusal(&answer, Topic::Other)
            )));
        }
        Ok(ToolResult::with_structured(
            format!(
                "Erased {email} from this workspace's contacts and audiences. If they had unsubscribed, that stays on the suppression list, so adding them again can't send to someone who said no."
            ),
            json!({"workspace_id": workspace_id, "erased": email, "contact_id": id}),
        ))
    }

    /// Puts addresses on the suppression list.
    async fn suppress(
        &self,
        workspace_id: Uuid,
        input: &StudioInput,
        emails: Vec<String>,
    ) -> Result<ToolResult> {
        let body = json!({
            "workspaceId": workspace_id,
            "emails": emails,
            "note": note_of(input, "Added through the studio tool"),
        });
        let key = format!("email-suppress-{}", digest(&body.to_string()));
        let answer = self
            .email_call("POST", "/suppressions", Some(&body), Some(&key))
            .await?;
        if !answer.is_success() {
            return Ok(ToolResult::error(format!(
                "Not added to the suppression list: {}",
                email_refusal(&answer, Topic::Other)
            )));
        }
        let added = count(&answer.data, "/added");
        let already = count(&answer.data, "/already");
        let said = if emails.len() == 1 {
            if added > 0 {
                format!(
                    "{} is on the suppression list now: no campaign will reach them.",
                    emails[0]
                )
            } else {
                format!("{} was already on the suppression list.", emails[0])
            }
        } else {
            format!(
                "Added {} to the suppression list{}: no campaign will reach them.",
                credits(added),
                if already > 0 {
                    format!(" ({} were already on it)", credits(already))
                } else {
                    String::new()
                }
            )
        };
        Ok(ToolResult::with_structured(
            said,
            json!({"workspace_id": workspace_id, "added": added, "already": already, "emails": emails}),
        ))
    }

    // ------------------------------------------------------------------------------------
    // email_unsubscribes
    // ------------------------------------------------------------------------------------

    async fn email_unsubscribes(
        &self,
        workspace_id: Uuid,
        input: &StudioInput,
    ) -> Result<ToolResult> {
        let operation = operation_of(
            input.operation.as_deref(),
            SUPPRESSION_OPERATIONS,
            "email_unsubscribes",
        )?;
        match operation.as_str() {
            "add" => {
                let (contacts, invalid) = contacts_of(input)?;
                if !invalid.is_empty() {
                    return Err(Error::Validation(format!(
                        "Not email addresses: {}",
                        invalid.join(", ")
                    )));
                }
                if contacts.is_empty() {
                    return Err(Error::Validation(
                        "contacts (addresses) or email is required for email_unsubscribes with operation add".to_string(),
                    ));
                }
                let emails = contacts.into_iter().map(|c| c.email).collect();
                self.suppress(workspace_id, input, emails).await
            }
            "remove" => self.unsuppress(workspace_id, input).await,
            _ => self.list_suppressions(workspace_id, input).await,
        }
    }

    async fn list_suppressions(
        &self,
        workspace_id: Uuid,
        input: &StudioInput,
    ) -> Result<ToolResult> {
        let limit = input
            .limit
            .unwrap_or(LIST_LIMIT_DEFAULT)
            .clamp(1, LIST_LIMIT_MAX);
        let mut path = format!("/suppressions?workspaceId={workspace_id}&limit={limit}");
        let query = input
            .query
            .as_deref()
            .map(str::trim)
            .filter(|q| !q.is_empty());
        if let Some(query) = query {
            path.push_str(&format!("&q={}", urlencoding::encode(query)));
        }
        let reason = input
            .reason
            .as_deref()
            .map(|r| r.trim().to_lowercase())
            .filter(|r| !r.is_empty());
        if let Some(reason) = &reason {
            if !SUPPRESSION_REASONS.contains(&reason.as_str()) {
                return Err(Error::Validation(format!(
                    "reason is one of {}",
                    SUPPRESSION_REASONS.join(", ")
                )));
            }
            path.push_str(&format!("&reason={reason}"));
        }
        if let Some(campaign) = input
            .campaign_id
            .as_deref()
            .filter(|c| !c.trim().is_empty())
        {
            path.push_str(&format!(
                "&campaignId={}",
                parse_id(campaign, "campaign_id")?
            ));
        }
        let answer = self.email_call("GET", &path, None, None).await?;
        if !answer.is_success() {
            return Ok(ToolResult::error(email_refusal(&answer, Topic::Other)));
        }
        let days = input.days.unwrap_or(STATS_DAYS_DEFAULT).clamp(1, 365);
        // The report is a bonus: a Create without it still lists the addresses.
        let summary = self
            .email_call(
                "GET",
                &format!("/suppressions/summary?workspaceId={workspace_id}&days={days}"),
                None,
                None,
            )
            .await
            .ok()
            .filter(StudioAnswer::is_success)
            .map(|answer| answer.data);
        let suppressions = answer
            .data
            .get("suppressions")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let total = answer
            .data
            .get("total")
            .and_then(Value::as_i64)
            .unwrap_or(suppressions.len() as i64);
        let scope = match (query, &reason) {
            (Some(q), _) => format!(" matching “{q}”"),
            (None, Some(r)) => format!(" ({r})"),
            (None, None) => String::new(),
        };
        let mut lines = vec![format!(
            "The suppression list: {} address{}{scope}, never sent to.",
            credits(total),
            if total == 1 { "" } else { "es" }
        )];
        if let Some(summary) = &summary {
            let n = |pointer: &str| count(summary, pointer);
            lines.push(format!(
                "Last {days} days: {} unsubscribed, {} bounced, {} spam complaint{}, {} added by hand; {} resubscribed.",
                credits(n("/byReason/unsubscribed")),
                credits(n("/byReason/bounced")),
                credits(n("/byReason/complained")),
                if n("/byReason/complained") == 1 { "" } else { "s" },
                credits(n("/byReason/manual")),
                credits(n("/resubscribed"))
            ));
            let said: Vec<String> = [
                ("too_many", "too many emails"),
                ("not_relevant", "not relevant"),
                ("never_signed_up", "never signed up"),
                ("other", "other"),
            ]
            .iter()
            .filter_map(|(key, words)| {
                let n = count(summary, &format!("/feedback/{key}"));
                (n > 0).then(|| format!("{words} {}", credits(n)))
            })
            .collect();
            if !said.is_empty() {
                lines.push(format!("Why they left: {}.", said.join(", ")));
            }
        }
        let mut listed = Vec::new();
        for suppression in suppressions.iter().take(limit as usize) {
            let email = text(suppression, "email").unwrap_or_default();
            lines.push(format!("- {email}: {}", suppression_words(suppression)));
            listed.push(json!({
                "email": email,
                "reason": suppression.get("reason"),
                "source": suppression.get("source"),
                "campaign": suppression.get("campaign"),
                "feedback": suppression.get("feedback"),
                "detail": suppression.get("detail"),
                "created_at": suppression.get("createdAt"),
            }));
        }
        Ok(ToolResult::with_structured(
            lines.join("\n"),
            json!({
                "workspace_id": workspace_id,
                "suppressions": listed,
                "total": total,
                "summary": summary,
                "next_cursor": answer.data.get("nextCursor"),
            }),
        ))
    }

    /// Takes an address off the suppression list, only with the person's go.
    async fn unsuppress(&self, workspace_id: Uuid, input: &StudioInput) -> Result<ToolResult> {
        let email = one_address(input, "email_unsubscribes with operation remove")?;
        let answer = self
            .email_call(
                "GET",
                &format!(
                    "/suppressions?workspaceId={workspace_id}&limit=50&q={}",
                    urlencoding::encode(&email)
                ),
                None,
                None,
            )
            .await?;
        if !answer.is_success() {
            return Ok(ToolResult::error(format!(
                "Not removed: {}",
                email_refusal(&answer, Topic::Other)
            )));
        }
        let Some(suppression) = answer
            .data
            .get("suppressions")
            .and_then(Value::as_array)
            .and_then(|items| {
                items
                    .iter()
                    .find(|s| {
                        text(s, "email").map(|e| e.to_lowercase()).as_deref()
                            == Some(email.as_str())
                    })
                    .cloned()
            })
        else {
            return Ok(ToolResult::error(format!(
                "Not removed: {email} isn't on the suppression list."
            )));
        };
        let reason = text(&suppression, "reason").unwrap_or_default();
        let why = suppression_words(&suppression);
        let caution = match reason.as_str() {
            "unsubscribed" | "complained" => {
                " Only if they asked to hear from the team again: campaigns will reach them."
            }
            "bounced" => {
                " Their address bounced: sending to it again can hurt the team's sending reputation."
            }
            _ => " Campaigns will reach them again.",
        };
        let what = format!("{email} ({why})");

        let asked = match peer::current().filter(peer::CallPeer::can_elicit) {
            Some(peer) => {
                let message = format!("Take {what} off the suppression list?{caution}");
                let schema = json!({
                    "type": "object",
                    "properties": {
                        "confirm": {
                            "type": "boolean",
                            "title": "Take them off",
                            "description": format!("Campaigns can reach {email} again."),
                            "default": false
                        }
                    },
                    "required": ["confirm"]
                });
                // What the question is about: an answer for another address or another
                // suppression of it is not taken.
                let state = format!(
                    "studio-email-unsuppress:{email}:{reason}:{}",
                    text(&suppression, "createdAt").unwrap_or_default()
                );
                match peer
                    .ask(
                        "confirm-unsuppress",
                        &message,
                        schema,
                        &state,
                        ELICIT_TIMEOUT,
                    )
                    .await
                {
                    Elicited::Pending => {
                        return Ok(ToolResult::with_structured(
                            format!(
                                "Asking the person before {email} comes off the suppression list."
                            ),
                            json!({"removed": false, "email": email, "asking": true}),
                        ))
                    }
                    Elicited::Accepted(content)
                        if content.get("confirm").and_then(Value::as_bool) == Some(true) =>
                    {
                        Some(true)
                    }
                    Elicited::Accepted(_) | Elicited::Declined | Elicited::Cancelled => Some(false),
                    Elicited::Unanswered(reason) => {
                        tracing::debug!(%reason, "studio: the unsuppress question got no answer");
                        None
                    }
                }
            }
            None => None,
        };
        let confirmed = input.confirm_email.as_deref().and_then(address_of);
        match (asked, confirmed) {
            (Some(true), _) => {}
            (Some(false), _) => {
                return Ok(ToolResult::with_structured(
                    format!(
                    "Not removed: the person didn't say go. {email} stays on the suppression list."
                ),
                    json!({"removed": false, "email": email, "declined": true}),
                ))
            }
            (None, Some(confirmed)) if confirmed == email => {}
            (None, confirmed) => {
                let mut said = match confirmed {
                    Some(other) => format!("Not removed: confirm_email is {other}, not {email}."),
                    None => "Not removed yet: the person's go is needed first.".to_string(),
                };
                said.push_str(&format!(
                    " Show them: {what}.{caution} If they agree, call email_unsubscribes again with operation remove, email {email} and confirm_email {email}."
                ));
                return Ok(ToolResult::with_structured(
                    said,
                    json!({
                        "removed": false,
                        "email": email,
                        "reason": reason,
                        "confirm_with": {
                            "action": "email_unsubscribes",
                            "operation": "remove",
                            "email": email,
                            "confirm_email": email,
                        },
                    }),
                ));
            }
        }
        let mut body = json!({
            "workspaceId": workspace_id,
            "email": email,
            "note": note_of(input, "Taken off through the studio tool, with the person's go"),
        });
        if matches!(reason.as_str(), "unsubscribed" | "complained") {
            body["confirm"] = json!(true);
        }
        let answer = self
            .email_call("POST", "/suppressions/remove", Some(&body), None)
            .await?;
        if !answer.is_success() {
            return Ok(ToolResult::error(format!(
                "Not removed: {}",
                email_refusal(&answer, Topic::Other)
            )));
        }
        Ok(ToolResult::with_structured(
            format!(
                "Took {email} off the suppression list: campaigns can reach them again. Create keeps who did it and why in the address's history."
            ),
            json!({"removed": true, "email": email, "was": reason}),
        ))
    }
}
