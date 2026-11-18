//! Review and confirmation for sending now or scheduling at a local time.

use super::go::{persons_go, Go, Question, ASKED_NOTE};
use super::{
    campaign_url, email_refusal, given, moment, number, people, purpose_words, send_at_of,
    send_key, strings, text, time_zone_of, when_words, Topic, NOT_ANSWERED,
};
use crate::domains::studio::{parse_id, words::credits, StudioInput, StudioTool};
use mcp_types::{tool::ToolResult, Result};
use serde_json::{json, Value};
use uuid::Uuid;

pub(super) fn target_words(audience: &Value, segment: Option<&Value>) -> String {
    let name = text(audience, "name").unwrap_or_else(|| NOT_ANSWERED.into());
    let audience = format!("the audience “{name}” ({})", purpose_words(audience));
    match segment {
        Some(segment) => format!(
            "the segment “{}” of {audience}",
            text(segment, "name").unwrap_or_else(|| NOT_ANSWERED.into())
        ),
        None => audience,
    }
}

impl StudioTool {
    pub(super) async fn email_send(
        &self,
        workspace_id: Uuid,
        input: &StudioInput,
    ) -> Result<ToolResult> {
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
                    "Not sent: “{name}” is paused. email_resume sends to the people still waiting, with the person's go; email_cancel stops it for good."
                )))
            }
            _ => {}
        }
        let Some(revision) = number(&campaign, "revision") else {
            return Ok(ToolResult::error("Not sent: campaign revision not answered. Read the campaign again before confirming."));
        };
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
        let Some(recipients) = number(&preview, "recipients") else {
            return Ok(ToolResult::error(format!("Not sent: the eligible count for “{name}” was not answered. Check its audience with email_recipients.")));
        };
        if recipients <= 0 {
            return Ok(ToolResult::error(format!(
                "Not sent: “{name}” has no one to send to yet. Choose an audience with subscribed contacts in the studio: {}",
                campaign_url(&campaign_id.to_string())
            )));
        }

        let Some(audience_id) =
            text(&campaign, "audienceId").or_else(|| text(&campaign["audience"], "id"))
        else {
            return Ok(ToolResult::error(
                "Not sent: audience not answered. Check the campaign with email_recipients.",
            ));
        };
        let mut audience = campaign["audience"].clone();
        if text(&audience, "purpose").is_none() || text(&audience, "name").is_none() {
            if let Some(listed) = self.audience(workspace_id, &audience_id).await {
                for field in ["name", "purpose"] {
                    if let Some(value) = text(&listed, field) {
                        audience[field] = json!(value);
                    }
                }
            }
        }
        audience["id"] = json!(audience_id);
        let segment_id = text(&campaign, "segmentId").or_else(|| text(&campaign["segment"], "id"));
        let segment = segment_id
            .as_ref()
            .map(|id| json!({"id": id, "name": campaign.pointer("/segment/name")}));
        let target = target_words(&audience, segment.as_ref());
        let subject = text(&preview, "subject").or_else(|| text(&campaign, "subject"));
        let from = text(&preview, "from");
        let when = when_words(&send_at, &time_zone);
        let count = people(recipients);
        let what = format!(
            "“{name}” (subject “{}”, from {}) to {count} in {target} {when}",
            subject.as_deref().unwrap_or(NOT_ANSWERED),
            from.as_deref().unwrap_or(NOT_ANSWERED)
        );
        let mut warnings = Vec::new();
        let audience_name = text(&audience, "name").unwrap_or_else(|| NOT_ANSWERED.into());
        match audience["purpose"].as_str() {
            Some("test") => warnings.push(format!(
                "“{audience_name}” is a test audience: this is a real send to the {count} in it."
            )),
            Some("production")
                if [Some(name.as_str()), subject.as_deref()]
                    .into_iter()
                    .flatten()
                    .any(|s| {
                        s.split(|c: char| !c.is_alphanumeric())
                            .any(|word| word.eq_ignore_ascii_case("test"))
                    }) =>
            {
                warnings.push(format!("this reads as a test (its name or subject says “test”), and “{audience_name}” is a production audience: {count} would get it."));
            }
            _ => {}
        }
        warnings.extend(strings(preview.get("warnings")));
        if let Some(previous) = input.confirm_recipients.filter(|n| *n != recipients) {
            warnings.push(format!(
                "The count changed from {previous} to {recipients}."
            ));
        }
        let checks: String = warnings.iter().map(|w| format!("\nCheck: {w}")).collect();
        let mut confirm = json!({"action":"email_send","campaign_id":campaign_id,"send_at":send_at,
            "time_zone":time_zone,"audience_id":audience_id,"confirm_recipients":recipients});
        let mut ids = format!("audience_id {audience_id}");
        if let Some(id) = &segment_id {
            confirm["segment_id"] = json!(id);
            ids.push_str(&format!(", segment_id {id}"));
        }
        let review = |reason: String| {
            ToolResult::with_structured(
            format!("{reason} Show them: {what}.{checks} If they agree, call email_send again with campaign_id {campaign_id}, send_at {send_at}, time_zone {time_zone}, {ids} and confirm_recipients {recipients}."),
            json!({"sent": false, "campaign_id": campaign_id, "revision": revision, "audience": audience, "segment": segment,
                "recipients": recipients, "subject": subject, "from": from, "send_at": send_at, "time_zone": time_zone,
                "warnings": warnings, "confirm_with": confirm}),
        )
        };
        let checked_audience = given(input.audience_id.as_deref())
            .map(|id| parse_id(id, "audience_id"))
            .transpose()?;
        let checked_segment = given(input.segment_id.as_deref())
            .map(|id| parse_id(id, "segment_id"))
            .transpose()?;
        let changed = if checked_audience.is_some_and(|id| id.to_string() != audience_id) {
            Some("audience")
        } else if (checked_audience.is_some() || checked_segment.is_some())
            && checked_segment.map(|id| id.to_string()) != segment_id
        {
            Some("segment")
        } else {
            None
        };
        if let Some(changed) = changed {
            return Ok(review(format!(
                "Not sent: “{name}” now goes to {target}, not the {changed} the person agreed to."
            )));
        }
        let go = persons_go(Question {
            key: "confirm-send",
            message: format!("Send {what}?{checks} Once it goes, it can't be unsent."),
            title: "Send it",
            does: format!("Send to {count}."),
            state: format!("studio-email-send:{campaign_id}:{revision}:{recipients}:{send_at}:{time_zone}:{audience_id}:{}", segment_id.as_deref().unwrap_or("-")),
        }, input.confirm_recipients == Some(recipients)).await;
        let mut notes = Vec::new();
        match go {
            Go::Yes { asked: true } => notes.push(ASKED_NOTE.to_string()),
            Go::Yes { asked: false } => {}
            Go::Asking => return Ok(ToolResult::with_structured(
                format!("Asking the person before {what} goes."),
                json!({"sent":false,"campaign_id":campaign_id,"asking":true}))),
            Go::No => return Ok(ToolResult::with_structured(
                format!("Not sent: the person didn't say go to sending {what}. Nothing was sent."),
                json!({"sent":false,"campaign_id":campaign_id,"declined":true}))),
            Go::NotAsked => return Ok(ToolResult::with_structured(
                format!("Not sent: {}. Nothing was sent.", crate::go_ahead::NOT_ASKED),
                json!({"sent":false,"campaign_id":campaign_id,"go_ahead":"asked_only"}))),
            Go::Needed { unanswered } => return Ok(review(match input.confirm_recipients {
                Some(confirmed) => format!("Not sent: it now goes to {count}, not the {} the person agreed to.", credits(confirmed)),
                None if unanswered => "Not sent yet: the person was asked through the client and gave no answer in time.".into(),
                None => "Not sent yet: the person's go is needed first.".into(),
            })),
        }

        let key = send_key(campaign_id, revision, &send_at, &time_zone);
        let mut body = json!({
            "workspaceId": workspace_id,
            "sendAt": send_at,
            "timeZone": time_zone,
            "revision": revision,
        });
        // Older servers do not have the atomic count check. Both API features shipped together.
        if campaign.get("remaining").is_some() {
            body["expectedRecipients"] = json!(recipients);
        }
        let (answer, kept) = self
            .email_call_newer(
                "POST",
                &format!("/campaigns/{campaign_id}/schedule"),
                &body,
                &["expectedRecipients"],
                Some(&key),
            )
            .await?;
        if !kept || body.get("expectedRecipients").is_none() {
            notes.push("Recipient count at schedule time: not answered by this API.".into());
        }
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
                "Sending “{name}” to {} in {target} now: it starts within a minute.",
                people(recipients)
            )
        } else {
            format!(
                "Scheduled “{name}” to {} in {target} {when}{}.",
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
}
