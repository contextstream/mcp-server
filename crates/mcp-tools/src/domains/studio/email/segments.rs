//! `email_segments`: the segments of an audience (the people in it that rules pick out),
//! listed, previewed, created and changed; and `email_recipients`: exactly who a campaign's
//! send would reach as of now, and who is left out.

use mcp_client::studio::StudioAnswer;
use mcp_types::{tool::ToolResult, Error, Result};
use serde_json::{json, Value};
use uuid::Uuid;

use super::{
    campaign_url, email_refusal, given, name_of, number, operation_of, people, said, text, Topic,
    NOT_ANSWERED,
};
use crate::domains::studio::words::credits;
use crate::domains::studio::{parse_id, StudioInput, StudioTool};

const SEGMENT_OPERATIONS: &[&str] = &["list", "preview", "create", "update"];
const RULES_SHAPE: &str = "{match: all|any, conditions: [{field, op, value}]}, e.g. {\"match\": \"all\", \"conditions\": [{\"field\": \"tag\", \"op\": \"has\", \"value\": \"vip\"}]}";
/// How many addresses an answer names as a sample.
const SAMPLE: usize = 5;
/// How many of an audience's suppressed contacts are read to say why they are left out.
const LEFT_OUT_READ: usize = 50;
/// Why a contact is not sent to, as Create keeps it, and in words.
const REASONS: [(&str, &str); 4] = [
    ("unsubscribed", "unsubscribed"),
    ("bounced", "bounced"),
    ("complained", "marked an email as spam"),
    ("manual", "added by hand"),
];

/// `rules` as given: Create's own object, or its JSON in a string.
fn rules_of(input: &StudioInput) -> Result<Option<Value>> {
    let shape = || Error::Validation(format!("rules is {RULES_SHAPE}"));
    match &input.rules {
        None | Some(Value::Null) => Ok(None),
        Some(rules @ Value::Object(_)) => Ok(Some(rules.clone())),
        Some(Value::String(text)) => match serde_json::from_str::<Value>(text) {
            Ok(rules @ Value::Object(_)) => Ok(Some(rules)),
            _ => Err(shape()),
        },
        Some(_) => Err(shape()),
    }
}

/// A segment's rules in words: "all of: tag has vip; engagement opened within 30".
pub(super) fn rules_words(rules: &Value) -> String {
    let conditions: Vec<String> = rules
        .get("conditions")
        .and_then(Value::as_array)
        .map(|conditions| {
            conditions
                .iter()
                .map(|condition| {
                    [
                        text(condition, "field").map(|field| field.replace('_', " ")),
                        text(condition, "key"),
                        text(condition, "op").map(|op| op.replace('_', " ")),
                        text(condition, "value"),
                    ]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(" ")
                })
                .filter(|words| !words.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if conditions.is_empty() {
        return format!("rules {NOT_ANSWERED}");
    }
    let matching = match rules.get("match").and_then(Value::as_str) {
        Some("any") => "any of",
        _ => "all of",
    };
    format!("{matching}: {}", conditions.join("; "))
}

/// The addresses of a sample (`[{email, firstName}]`).
fn sample_of(sample: Option<&Value>) -> Vec<String> {
    sample
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .take(SAMPLE)
                .filter_map(|row| text(row, "email"))
                .collect()
        })
        .unwrap_or_default()
}

/// Only the address and reason are part of an excluded-recipient sample.
fn exclusions(rows: &[Value], reason_path: &str) -> Vec<Value> {
    rows.iter().take(20).filter_map(|row| {
        Some(json!({"email": text(row, "email")?, "reason": row.pointer(reason_path).and_then(Value::as_str)}))
    }).collect()
}

/// "12 contacts, 10 subscribed", with what Create didn't answer said as such.
fn reach_words(holder: &Value) -> String {
    format!(
        "{} contacts, {} subscribed",
        said(number(holder, "contacts")),
        said(number(holder, "subscribed"))
    )
}

/// Who a campaign's send reaches, as Create answered it or as its other answers add up.
struct Reach {
    /// `audience`: the audience or segment as it is now; `sends`: the recipients a started
    /// campaign wrote, of whom `eligible` still wait.
    basis: String,
    audience: Option<Value>,
    segment: Option<Value>,
    contacts: Option<i64>,
    eligible: Option<i64>,
    /// Who is left out, by reason.
    left_out: Option<Value>,
    /// How many are left out, where the reasons aren't known.
    left_out_count: Option<i64>,
    /// Why the reasons aren't known, or what they were counted over.
    left_out_note: Option<String>,
    sample: Option<Vec<String>>,
    left_out_sample: Option<Vec<Value>>,
}

impl Reach {
    /// "the segment “Regulars” of the audience “Coffee club” (production)".
    fn to(&self) -> String {
        let Some(audience) = &self.audience else {
            return "no audience".to_string();
        };
        super::send::target_words(audience, self.segment.as_ref())
    }

    fn left_out_line(&self) -> String {
        if let Some(by_reason) = &self.left_out {
            let parts: Vec<String> = REASONS
                .iter()
                .filter_map(|(key, words)| match number(by_reason, key) {
                    Some(0) => None,
                    Some(n) => Some(format!("{} {words}", credits(n))),
                    None => Some(format!("{words} {NOT_ANSWERED}")),
                })
                .collect();
            let note = self
                .left_out_note
                .as_ref()
                .map_or(String::new(), |note| format!(" ({note})"));
            return if parts.is_empty() {
                format!("No one in it is left out{note}.")
            } else {
                format!("Left out: {}{note}.", parts.join(", "))
            };
        }
        match (self.left_out_count, &self.left_out_note) {
            (Some(0), _) => "No one in it is left out.".to_string(),
            (Some(n), Some(note)) => format!("Left out: {}, and why is {note}.", credits(n)),
            (Some(n), None) => format!("Left out: {}, and why is {NOT_ANSWERED}.", credits(n)),
            (None, _) => format!("Who is left out: {NOT_ANSWERED}."),
        }
    }
}

impl StudioTool {
    // ------------------------------------------------------------------------------------
    // email_segments
    // ------------------------------------------------------------------------------------

    pub(super) async fn email_segments(
        &self,
        workspace_id: Uuid,
        input: &StudioInput,
    ) -> Result<ToolResult> {
        let operation = operation_of(
            input.operation.as_deref(),
            SEGMENT_OPERATIONS,
            "email_segments",
        )?;
        match operation.as_str() {
            "preview" => self.preview_segment(workspace_id, input).await,
            "create" => self.create_segment(workspace_id, input).await,
            "update" => self.update_segment(workspace_id, input).await,
            _ => self.list_segments(workspace_id, input).await,
        }
    }

    /// The audience a segment call names, required.
    fn segment_audience(input: &StudioInput, call: &str) -> Result<Uuid> {
        let id = given(input.audience_id.as_deref()).ok_or_else(|| {
            Error::Validation(format!(
                "audience_id is required for {call}: the audience the segment is of (email_audiences lists them)"
            ))
        })?;
        parse_id(id, "audience_id")
    }

    async fn audience_name(&self, workspace_id: Uuid, id: &str) -> Option<String> {
        self.audience(workspace_id, id)
            .await
            .and_then(|audience| text(&audience, "name"))
    }

    async fn list_segments(&self, workspace_id: Uuid, input: &StudioInput) -> Result<ToolResult> {
        let mut path = format!("/segments?workspaceId={workspace_id}");
        let scoped = given(input.audience_id.as_deref())
            .map(|id| parse_id(id, "audience_id"))
            .transpose()?;
        if let Some(audience_id) = scoped {
            path.push_str(&format!("&audienceId={audience_id}"));
        }
        let answer = self.email_call("GET", &path, None, None).await?;
        if !answer.is_success() {
            return Ok(ToolResult::error(email_refusal(&answer, Topic::Audience)));
        }
        let Some(segments) = answer.data.as_array() else {
            return Ok(ToolResult::error(
                "Segment list not answered. Try email_segments again.",
            ));
        };
        if segments.is_empty() {
            return Ok(ToolResult::with_structured(
                format!(
                    "No segments as of now{}: email_segments with operation preview counts who rules would pick out of an audience, and create saves them.",
                    if scoped.is_some() { " in this audience" } else { "" }
                ),
                json!({"workspace_id": workspace_id, "segments": []}),
            ));
        }
        let audiences = self.audiences_now(workspace_id).await.unwrap_or_default();
        let mut lines = vec![format!(
            "{} segment{} as of now:",
            segments.len(),
            if segments.len() == 1 { "" } else { "s" }
        )];
        let mut listed = Vec::new();
        for segment in segments {
            let id = text(segment, "id").unwrap_or_default();
            let name = text(segment, "name").unwrap_or_else(|| "A segment".to_string());
            let audience_id = text(segment, "audienceId").unwrap_or_default();
            let of = audiences
                .iter()
                .find(|a| text(a, "id").as_deref() == Some(audience_id.as_str()))
                .and_then(|a| text(a, "name"))
                .map_or(String::new(), |audience| format!(", of {audience}"));
            let rules = segment.get("rules").cloned().unwrap_or(Value::Null);
            lines.push(format!(
                "- {name}{of}: {}; {} (segment_id {id})",
                reach_words(segment),
                rules_words(&rules)
            ));
            listed.push(json!({
                "segment_id": id,
                "name": name,
                "audience_id": audience_id,
                "rules": rules,
                "contacts": segment.get("contacts"),
                "subscribed": segment.get("subscribed"),
            }));
        }
        Ok(ToolResult::with_structured(
            lines.join("\n"),
            json!({"workspace_id": workspace_id, "segments": listed}),
        ))
    }

    async fn preview_segment(&self, workspace_id: Uuid, input: &StudioInput) -> Result<ToolResult> {
        let call = "email_segments with operation preview";
        let audience_id = Self::segment_audience(input, call)?;
        let rules = rules_of(input)?.ok_or_else(|| {
            Error::Validation(format!("rules is required for {call}: {RULES_SHAPE}"))
        })?;
        let answer = self
            .email_call(
                "POST",
                "/segments/preview",
                Some(&json!({"workspaceId": workspace_id, "audienceId": audience_id, "rules": rules})),
                None,
            )
            .await?;
        if !answer.is_success() {
            return Ok(ToolResult::error(format!(
                "Not previewed: {}",
                email_refusal(&answer, Topic::Audience)
            )));
        }
        let reach = answer.data;
        let audience = self
            .audience_name(workspace_id, &audience_id.to_string())
            .await
            .map_or_else(|| "that audience".to_string(), |name| format!("“{name}”"));
        let sample = sample_of(reach.get("sample"));
        let mut lines = vec![format!(
            "These rules match {} contacts in {audience}, {} of them subscribed: {}.",
            said(number(&reach, "contacts")),
            said(number(&reach, "subscribed")),
            rules_words(&rules)
        )];
        if !sample.is_empty() {
            lines.push(format!("Sample: {}.", sample.join(", ")));
        }
        lines.push(
            "Nothing is saved: email_segments with operation create, a name and the same rules saves it."
                .to_string(),
        );
        Ok(ToolResult::with_structured(
            lines.join("\n"),
            json!({
                "workspace_id": workspace_id,
                "audience_id": audience_id,
                "rules": rules,
                "contacts": reach.get("contacts"),
                "subscribed": reach.get("subscribed"),
                "sample": sample,
            }),
        ))
    }

    /// A saved segment as Create answered it, in words.
    async fn segment_result(
        &self,
        workspace_id: Uuid,
        did: &str,
        segment: &Value,
        with_audience: bool,
    ) -> ToolResult {
        let id = text(segment, "id").unwrap_or_default();
        let audience_id = text(segment, "audienceId").unwrap_or_default();
        let of = if with_audience {
            self.audience_name(workspace_id, &audience_id)
                .await
                .map_or(String::new(), |audience| format!(" of “{audience}”"))
        } else {
            String::new()
        };
        let rules = segment.get("rules").cloned().unwrap_or(Value::Null);
        ToolResult::with_structured(
            format!(
                "{did} the segment “{}”{of} (segment_id {id}): {}.\nRules: {}.\nemail_draft with segment_id drafts a campaign to it; email_recipients says who a campaign would reach.",
                text(segment, "name").unwrap_or_default(),
                reach_words(segment),
                rules_words(&rules)
            ),
            json!({
                "workspace_id": workspace_id,
                "segment_id": id,
                "audience_id": audience_id,
                "name": segment.get("name"),
                "rules": rules,
                "contacts": segment.get("contacts"),
                "subscribed": segment.get("subscribed"),
            }),
        )
    }

    async fn create_segment(&self, workspace_id: Uuid, input: &StudioInput) -> Result<ToolResult> {
        let call = "email_segments with operation create";
        let audience_id = Self::segment_audience(input, call)?;
        let name = name_of(input, call)?.ok_or_else(|| {
            Error::Validation(format!(
                "name is required for {call}: what the segment is called, e.g. \"Regulars\""
            ))
        })?;
        let rules = rules_of(input)?.ok_or_else(|| {
            Error::Validation(format!("rules is required for {call}: {RULES_SHAPE}"))
        })?;
        let answer = self
            .email_call(
                "POST",
                "/segments",
                Some(&json!({
                    "workspaceId": workspace_id,
                    "audienceId": audience_id,
                    "name": name,
                    "rules": rules,
                })),
                None,
            )
            .await?;
        if !answer.is_success() {
            return Ok(ToolResult::error(format!(
                "Not created: {}",
                email_refusal(&answer, Topic::Audience)
            )));
        }
        Ok(self
            .segment_result(workspace_id, "Created", &answer.data, true)
            .await)
    }

    async fn update_segment(&self, workspace_id: Uuid, input: &StudioInput) -> Result<ToolResult> {
        let call = "email_segments with operation update";
        let id = given(input.segment_id.as_deref()).ok_or_else(|| {
            Error::Validation(format!(
                "segment_id is required for {call} (email_segments lists them)"
            ))
        })?;
        let id = parse_id(id, "segment_id")?;
        let name = name_of(input, call)?;
        let rules = rules_of(input)?;
        if name.is_none() && rules.is_none() {
            return Err(Error::Validation(format!(
                "{call} needs something to change: name or rules"
            )));
        }
        let mut body = json!({"workspaceId": workspace_id});
        if let Some(name) = &name {
            body["name"] = json!(name);
        }
        if let Some(rules) = &rules {
            body["rules"] = rules.clone();
        }
        let answer = self
            .email_call("PATCH", &format!("/segments/{id}"), Some(&body), None)
            .await?;
        if !answer.is_success() {
            return Ok(ToolResult::error(format!(
                "Not changed: {}",
                email_refusal(&answer, Topic::Audience)
            )));
        }
        Ok(self
            .segment_result(workspace_id, "Changed", &answer.data, false)
            .await)
    }

    // ------------------------------------------------------------------------------------
    // email_recipients
    // ------------------------------------------------------------------------------------

    pub(super) async fn email_recipients(
        &self,
        workspace_id: Uuid,
        input: &StudioInput,
    ) -> Result<ToolResult> {
        let campaign_id = Self::campaign_id(input, "email_recipients")?;
        let campaign = match self.read_campaign(workspace_id, campaign_id).await? {
            Ok(campaign) => campaign,
            Err(refused) => return Ok(ToolResult::error(refused)),
        };
        let name = text(&campaign, "name").unwrap_or_else(|| "The campaign".to_string());
        let status = text(&campaign, "status").unwrap_or_default();
        // Create's own answer where it has one; a Create without it (the campaign reads,
        // the route doesn't) is asked for the parts.
        let answer = self
            .email_call(
                "GET",
                &format!("/campaigns/{campaign_id}/reach?workspaceId={workspace_id}"),
                None,
                None,
            )
            .await?;
        let reach = if answer.is_success() {
            let reach = answer.data;
            if !reach.is_object() || reach.get("audience").is_none() {
                return Ok(ToolResult::error(
                    "Who this campaign would reach was not answered. Read the campaign again.",
                ));
            }
            Reach {
                basis: text(&reach, "basis").unwrap_or_else(|| "audience".to_string()),
                audience: reach.get("audience").filter(|a| a.is_object()).cloned(),
                segment: reach.get("segment").filter(|s| s.is_object()).cloned(),
                contacts: number(&reach, "contacts"),
                eligible: number(&reach, "eligible"),
                left_out: reach.get("leftOut").filter(|l| l.is_object()).cloned(),
                left_out_count: None,
                left_out_note: None,
                sample: reach
                    .get("sample")
                    .filter(|v| v.is_array())
                    .map(|v| sample_of(Some(v))),
                left_out_sample: reach["leftOutSample"]
                    .as_array()
                    .map(|rows| exclusions(rows, "/reason")),
            }
        } else if answer.status == 404 {
            self.reach_from_parts(workspace_id, campaign_id, &campaign)
                .await
        } else {
            return Ok(ToolResult::error(email_refusal(&answer, Topic::Campaign)));
        };

        let mut lines = Vec::new();
        if reach.audience.is_none() {
            lines.push(format!(
                "As of now “{name}” ({status}) has no audience, so a send would reach no one. Choose one in the studio ({}), or draft again with audience_id (email_audiences lists them).",
                campaign_url(&campaign_id.to_string())
            ));
        } else {
            lines.push(match (reach.basis.as_str(), reach.eligible) {
                (_, None) => format!(
                    "As of now, how many people “{name}” ({status}) would reach in {} is {NOT_ANSWERED}.",
                    reach.to()
                ),
                ("sends", Some(eligible)) => format!(
                    "As of now “{name}” ({status}) has {} still to reach in {}, of the {} it was written for.",
                    people(eligible),
                    reach.to(),
                    said(reach.contacts)
                ),
                (_, Some(eligible)) => format!(
                    "As of now “{name}” ({status}) would reach {} in {}, which holds {} contacts.",
                    people(eligible),
                    reach.to(),
                    said(reach.contacts)
                ),
            });
            lines.push(reach.left_out_line());
            match &reach.left_out_sample {
                Some(rows) if !rows.is_empty() => {
                    let samples: Vec<String> = rows
                        .iter()
                        .map(|row| {
                            format!(
                                "{} ({})",
                                row["email"].as_str().unwrap_or(NOT_ANSWERED),
                                row["reason"].as_str().unwrap_or(NOT_ANSWERED)
                            )
                        })
                        .collect();
                    lines.push(format!("Excluded sample: {}.", samples.join(", ")));
                }
                None => lines.push(format!("Excluded addresses: {NOT_ANSWERED}.")),
                _ => {}
            }
            match &reach.sample {
                Some(rows) if !rows.is_empty() => {
                    lines.push(format!("Sample: {}.", rows.join(", ")))
                }
                None => lines.push(format!("Recipient sample: {NOT_ANSWERED}.")),
                _ => {}
            }
        }
        Ok(ToolResult::with_structured(
            lines.join("\n"),
            json!({
                "workspace_id": workspace_id,
                "campaign_id": campaign_id,
                "name": name,
                "status": status,
                "basis": reach.basis,
                "audience": reach.audience,
                "segment": reach.segment,
                "contacts": reach.contacts,
                "eligible": reach.eligible,
                "left_out": reach.left_out,
                "left_out_count": reach.left_out_count,
                "sample": reach.sample,
                "left_out_sample": reach.left_out_sample,
            }),
        ))
    }

    /// Who a send reaches, from the routes every Create has: the campaign's preview (who
    /// can be sent to), its audience, and the audience's own contacts.
    async fn reach_from_parts(
        &self,
        workspace_id: Uuid,
        campaign_id: Uuid,
        campaign: &Value,
    ) -> Reach {
        let status = text(campaign, "status").unwrap_or_default();
        let named = |key: &str| campaign.get(key).filter(|value| value.is_object()).cloned();
        let segment = named("segment");
        let audience_id = text(campaign, "audienceId").or_else(|| {
            campaign
                .pointer("/audience/id")
                .and_then(Value::as_str)
                .map(str::to_string)
        });
        let Some(audience_id) = audience_id else {
            return Reach {
                basis: "audience".to_string(),
                audience: None,
                segment: None,
                contacts: None,
                eligible: None,
                left_out: None,
                left_out_count: None,
                left_out_note: None,
                sample: None,
                left_out_sample: None,
            };
        };
        // As Create lists it now (with its purpose, where it has one), else as the campaign
        // names it.
        let listed = self.audience(workspace_id, &audience_id).await;
        let audience = listed
            .as_ref()
            .map(|a| json!({"id": audience_id, "name": a.get("name"), "purpose": a.get("purpose")}))
            .or_else(|| named("audience"));

        // A campaign that has started wrote its recipients down: who still waits is its
        // own count.
        let written = number(
            campaign.get("progress").unwrap_or(&Value::Null),
            "recipients",
        );
        if !matches!(status.as_str(), "draft" | "scheduled") && written.is_some_and(|n| n > 0) {
            let progress = campaign.get("progress").cloned().unwrap_or(Value::Null);
            return Reach {
                basis: "sends".to_string(),
                audience,
                segment,
                contacts: written,
                eligible: number(&progress, "queued"),
                left_out: None,
                left_out_count: number(&progress, "skipped"),
                left_out_note: None,
                sample: None,
                left_out_sample: None,
            };
        }

        let eligible = self
            .preview(workspace_id, campaign_id, None)
            .await
            .ok()
            .and_then(|preview| number(&preview, "recipients"));
        let mut reach = Reach {
            basis: "audience".to_string(),
            audience,
            segment: segment.clone(),
            contacts: listed.as_ref().and_then(|a| number(a, "contacts")),
            eligible,
            left_out: None,
            left_out_count: listed.as_ref().and_then(|a| {
                number(a, "contacts")
                    .zip(number(a, "subscribed"))
                    .map(|(contacts, subscribed)| (contacts - subscribed).max(0))
            }),
            left_out_note: None,
            sample: None,
            left_out_sample: None,
        };
        let segment_id = segment.as_ref().and_then(|s| text(s, "id"));
        if let Some(segment_id) = segment_id {
            // The segment's own reach; who of it is suppressed is a count only.
            let row = self
                .email_call(
                    "GET",
                    &format!("/segments?workspaceId={workspace_id}&audienceId={audience_id}"),
                    None,
                    None,
                )
                .await
                .ok()
                .filter(StudioAnswer::is_success)
                .and_then(|answer| {
                    answer.data.as_array().and_then(|segments| {
                        segments
                            .iter()
                            .find(|s| text(s, "id").as_deref() == Some(segment_id.as_str()))
                            .cloned()
                    })
                });
            reach.contacts = row.as_ref().and_then(|s| number(s, "contacts"));
            reach.left_out_count = row.as_ref().and_then(|s| {
                number(s, "contacts")
                    .zip(number(s, "subscribed"))
                    .map(|(contacts, subscribed)| (contacts - subscribed).max(0))
            });
            reach.left_out_note = Some(format!("{NOT_ANSWERED} for a segment"));
            if let Some(rules) = row
                .as_ref()
                .and_then(|s| s.get("rules"))
                .filter(|r| r.is_object())
            {
                reach.sample = self
                    .email_call(
                        "POST",
                        "/segments/preview",
                        Some(&json!({"workspaceId": workspace_id, "audienceId": audience_id, "rules": rules})),
                        None,
                    )
                    .await
                    .ok()
                    .filter(StudioAnswer::is_success)
                    .and_then(|answer| answer.data.get("sample").filter(|v| v.is_array()).map(|v| sample_of(Some(v))));
            }
            return reach;
        }
        let contacts = |status: &str, limit: usize| {
            format!("/contacts?workspaceId={workspace_id}&audienceId={audience_id}&status={status}&limit={limit}")
        };
        if let Some(page) = self
            .email_call("GET", &contacts("subscribed", SAMPLE), None, None)
            .await
            .ok()
            .filter(StudioAnswer::is_success)
        {
            reach.sample = page
                .data
                .get("contacts")
                .filter(|v| v.is_array())
                .map(|v| sample_of(Some(v)));
        }
        if let Some(page) = self
            .email_call("GET", &contacts("suppressed", LEFT_OUT_READ), None, None)
            .await
            .ok()
            .filter(StudioAnswer::is_success)
        {
            let Some(rows) = page.data["contacts"].as_array() else {
                return reach;
            };
            reach.left_out_sample = Some(exclusions(rows, "/suppressed/reason"));
            let mut by_reason = json!({});
            for (key, _) in REASONS {
                let n = rows
                    .iter()
                    .filter(|row| {
                        row.pointer("/suppressed/reason").and_then(Value::as_str) == Some(key)
                    })
                    .count();
                by_reason[key] = json!(n);
            }
            let total = number(&page.data, "total");
            reach.left_out = Some(by_reason);
            reach.left_out_count = total;
            if let Some(total) = total.filter(|n| *n > rows.len() as i64) {
                reach.left_out_note = Some(format!(
                    "the first {} of {} left out",
                    rows.len(),
                    credits(total)
                ));
            }
        }
        reach
    }
}
