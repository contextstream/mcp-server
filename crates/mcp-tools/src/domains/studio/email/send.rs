//! Review and confirmation for sending now or scheduling at a local time.

use super::go::{persons_go, same_token, Go, Question, ReviewKey, SendToken, ASKED_NOTE};
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

struct SendReview {
    key: ReviewKey,
    workspace_id: Uuid,
    campaign_id: Uuid,
    name: String,
    subject: Option<String>,
    reach: super::ReviewedReach,
    revision: i64,
    recipients: i64,
    audience_id: String,
    segment_id: Option<String>,
    audience: Value,
    segment: Option<Value>,
    warnings: Vec<String>,
}

impl SendReview {
    fn target(&self) -> String {
        target_words(&self.audience, self.segment.as_ref())
    }

    fn description(&self, when: &str) -> String {
        format!(
            "“{}” (subject “{}”, from {}) to {} in {} {when}",
            self.name,
            self.subject.as_deref().unwrap_or(NOT_ANSWERED),
            self.reach.from,
            people(self.recipients),
            self.target()
        )
    }

    fn token(&self, send_at: &str, zone: &str) -> String {
        // The sender can change in email settings without changing the campaign revision.
        let mac = self.key.sign(
            &json!([
                "send1",
                self.workspace_id,
                self.campaign_id,
                self.revision,
                self.name,
                self.subject,
                self.reach.from,
                self.reach.digest,
                self.audience_id,
                self.segment_id,
                self.audience,
                self.segment,
                self.recipients,
                send_at,
                zone,
                self.warnings
            ])
            .to_string(),
        );
        SendToken {
            revision: self.revision,
            recipients: self.recipients,
        }
        .encode(&mac)
    }

    fn checks(&self, previous: Option<i64>) -> Vec<String> {
        let mut warnings = self.warnings.clone();
        if let Some(n) = previous.filter(|n| *n != self.recipients) {
            warnings.push(format!(
                "The count changed from {n} to {}.",
                self.recipients
            ));
        }
        warnings
    }

    fn show(&self, reason: &str, send_at: &str, zone: &str, previous: Option<i64>) -> ToolResult {
        let warnings = self.checks(previous);
        let checks = check_lines(&warnings);
        let what = self.description(&when_words(send_at, zone));
        let token = self.token(send_at, zone);
        let mut confirm = json!({
            "action": "email_send", "campaign_id": self.campaign_id, "send_at": send_at,
            "time_zone": zone, "audience_id": self.audience_id, "confirm_recipients": self.recipients,
            "confirm_review": token,
        });
        let mut ids = format!("audience_id {}", self.audience_id);
        if let Some(id) = &self.segment_id {
            confirm["segment_id"] = json!(id);
            ids.push_str(&format!(", segment_id {id}"));
        }
        ToolResult::with_structured(
            format!("{reason} Show them: {what}.{checks} If they agree, use the arguments in confirm_with to call email_send again with campaign_id {}, send_at {send_at}, time_zone {zone}, {ids}, confirm_recipients {} and confirm_review {token}.", self.campaign_id, self.recipients),
            json!({
                "sent": false, "campaign_id": self.campaign_id, "revision": self.revision,
                "audience": self.audience, "segment": self.segment, "recipients": self.recipients,
                "subject": self.subject, "from": self.reach.from, "send_at": send_at, "time_zone": zone,
                "warnings": warnings, "confirm_with": confirm,
            }),
        )
    }
}

fn check_lines(warnings: &[String]) -> String {
    warnings.iter().map(|w| format!("\nCheck: {w}")).collect()
}

impl StudioTool {
    async fn send_review(
        &self,
        workspace_id: Uuid,
        campaign_id: Uuid,
    ) -> Result<std::result::Result<SendReview, String>> {
        let campaign = match self.read_campaign(workspace_id, campaign_id).await? {
            Ok(campaign) => campaign,
            Err(refused) => return Ok(Err(format!("Not sent: {refused}"))),
        };
        let name = text(&campaign, "name").unwrap_or_else(|| "The campaign".to_string());
        let status = text(&campaign, "status").unwrap_or_default();
        match status.as_str() {
            "sending" => {
                return Ok(Err(format!(
                    "Not sent again: “{name}” is sending now. email_report follows it; email_cancel stops it."
                )))
            }
            "sent" => {
                return Ok(Err(format!(
                    "Not sent again: “{name}” was sent. email_report shows how it did."
                )))
            }
            "testing" => {
                return Ok(Err(format!(
                    "Not sent again: “{name}” is testing its subjects, and the rest go out with the winner on their own. email_report follows it; email_cancel stops it."
                )))
            }
            "paused" => {
                return Ok(Err(format!(
                    "Not sent: “{name}” is paused. email_resume sends to the people still waiting, with the person's go; email_cancel stops it for good."
                )))
            }
            _ => {}
        }
        let Some(revision) = number(&campaign, "revision") else {
            return Ok(Err("Not sent: campaign revision not answered. Read the campaign again before confirming.".into()));
        };
        let preview = match self.preview(workspace_id, campaign_id, None).await {
            Ok(preview) => preview,
            Err(None) => {
                return Ok(Err(format!(
                    "Not sent: Create couldn't be read just now, so who “{name}” would go to can't be checked. Nothing was sent; try again in a few minutes."
                )))
            }
            Err(Some(refused)) => {
                return Ok(Err(format!(
                    "Not sent: Create couldn't read “{name}” as it would go: {refused}"
                )))
            }
        };
        let Some(reach) = self
            .reviewed_reach(workspace_id, campaign_id, "recipients")
            .await?
        else {
            return Ok(Err(
                "This API cannot hold the send to what you reviewed, so nothing was sent.".into(),
            ));
        };
        let Some(recipients) = reach.eligible else {
            return Ok(Err(format!("Not sent: the eligible count for “{name}” was not answered. Check its audience with email_recipients.")));
        };
        if recipients <= 0 {
            return Ok(Err(format!(
                "Not sent: “{name}” has no one to send to yet. Choose an audience with subscribed contacts in the studio: {}",
                campaign_url(&campaign_id.to_string())
            )));
        }

        let Some(audience_id) =
            text(&campaign, "audienceId").or_else(|| text(&campaign["audience"], "id"))
        else {
            return Ok(Err(
                "Not sent: audience not answered. Check the campaign with email_recipients.".into(),
            ));
        };
        let mut audience = json!({
            "id": audience_id,
            "name": campaign.pointer("/audience/name").and_then(Value::as_str),
            "purpose": campaign.pointer("/audience/purpose").and_then(Value::as_str),
        });
        if text(&audience, "purpose").is_none() || text(&audience, "name").is_none() {
            if let Some(listed) = self.audience(workspace_id, &audience_id).await {
                for field in ["name", "purpose"] {
                    if let Some(value) = text(&listed, field) {
                        audience[field] = json!(value);
                    }
                }
            }
        }
        let segment_id = text(&campaign, "segmentId").or_else(|| text(&campaign["segment"], "id"));
        let segment = segment_id
            .as_ref()
            .map(|id| json!({"id": id, "name": campaign.pointer("/segment/name")}));
        let subject = text(&preview, "subject").or_else(|| text(&campaign, "subject"));
        let count = people(recipients);
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
        Ok(Ok(SendReview {
            key: ReviewKey::for_call(self).await?,
            workspace_id,
            campaign_id,
            name,
            subject,
            reach,
            revision,
            recipients,
            audience_id,
            segment_id,
            audience,
            segment,
            warnings,
        }))
    }

    pub(super) async fn email_send(
        &self,
        workspace_id: Uuid,
        input: &StudioInput,
    ) -> Result<ToolResult> {
        let campaign_id = Self::campaign_id(input, "email_send")?;
        let send_at = send_at_of(input.send_at.as_deref())?;
        let time_zone = time_zone_of(input.time_zone.as_deref(), &send_at)?;
        let review = match self.send_review(workspace_id, campaign_id).await? {
            Ok(review) => review,
            Err(why) => return Ok(ToolResult::error(why)),
        };
        let checked_audience = given(input.audience_id.as_deref())
            .map(|id| parse_id(id, "audience_id"))
            .transpose()?;
        let checked_segment = given(input.segment_id.as_deref())
            .map(|id| parse_id(id, "segment_id"))
            .transpose()?;
        let changed = if checked_audience.is_some_and(|id| id.to_string() != review.audience_id) {
            Some("audience")
        } else if (checked_audience.is_some() || checked_segment.is_some())
            && checked_segment.map(|id| id.to_string()) != review.segment_id
        {
            Some("segment")
        } else {
            None
        };
        let show =
            |reason: &str| review.show(reason, &send_at, &time_zone, input.confirm_recipients);
        if let Some(changed) = changed {
            return Ok(show(&format!(
                "Not sent: “{}” now goes to {}, not the {changed} the person agreed to.",
                review.name,
                review.target()
            )));
        }
        let token = review.token(&send_at, &time_zone);
        let seen_token = match given(input.confirm_review.as_deref()) {
            Some(raw) => match SendToken::parse(raw).filter(|_| same_token(raw, &token)) {
                Some(seen) => input
                    .confirm_recipients
                    .is_none_or(|n| n == seen.recipients)
                    .then_some(seen),
                None => {
                    return Ok(show(
                        "Not sent: the campaign changed since review. Ask the person again.",
                    ));
                }
            },
            None => None,
        };
        let what = review.description(&when_words(&send_at, &time_zone));
        let count = people(review.recipients);
        let checks = check_lines(&review.checks(input.confirm_recipients));
        let question_key = format!("confirm-send-{token}");
        let state = format!(
            "studio-email-send:{campaign_id}:{}:{}:{send_at}:{time_zone}:{}:{}",
            review.revision,
            review.recipients,
            review.audience_id,
            review.segment_id.as_deref().unwrap_or("-")
        );
        let question = Question {
            key: &question_key,
            message: format!("Send {what}?{checks} Once it goes, it can't be unsent."),
            title: "Send it",
            does: format!("Send to {count}."),
            state,
        };
        let go = persons_go(
            question,
            seen_token.is_some(),
            input.confirm_recipients.is_some(),
        )
        .await;
        let mut notes = Vec::new();
        match go {
            Go::Yes { asked: true } => notes.push(ASKED_NOTE.to_string()),
            Go::Yes { asked: false } => {}
            Go::Asking => {
                return Ok(ToolResult::with_structured(
                    format!("Asking the person before {what} goes."),
                    json!({"sent": false, "campaign_id": campaign_id, "asking": true}),
                ))
            }
            Go::No => {
                return Ok(ToolResult::with_structured(
                    format!(
                        "Not sent: the person didn't say go to sending {what}. Nothing was sent."
                    ),
                    json!({"sent": false, "campaign_id": campaign_id, "declined": true}),
                ))
            }
            Go::NotAsked => {
                return Ok(ToolResult::with_structured(
                    format!(
                        "Not sent: {}. Nothing was sent.",
                        crate::go_ahead::NOT_ASKED
                    ),
                    json!({"sent": false, "campaign_id": campaign_id, "go_ahead": "asked_only"}),
                ))
            }
            Go::Needed { unanswered } => {
                let reason = match input.confirm_recipients {
                    Some(confirmed) if confirmed != review.recipients => {
                        format!(
                            "Not sent: it now goes to {count}, not the {} the person agreed to.",
                            credits(confirmed)
                        )
                    }
                    _ if unanswered => {
                        "Not sent yet: the person was asked through the client and gave no answer in time.".into()
                    }
                    Some(_) => {
                        "Not sent yet: confirm_review from showing this review is needed; a count alone is not a go.".into()
                    }
                    None => "Not sent yet: the person's go is needed first.".into(),
                };
                return Ok(show(&reason));
            }
        }

        // A live form may have been open for up to LIVE_QUESTION_MAX. Re-read even
        // when the form timed out and this connection accepted the agent's confirmation.
        if crate::peer::current().is_some_and(|p| p.can_elicit() && !p.asks_through_the_result()) {
            match self.send_review(workspace_id, campaign_id).await? {
                Ok(now) if now.token(&send_at, &time_zone) != token => {
                    return Ok(now.show(
                        "Not sent: the campaign changed during confirmation. Ask the person again.",
                        &send_at,
                        &time_zone,
                        Some(review.recipients),
                    ))
                }
                Err(why) => return Ok(ToolResult::error(why)),
                _ => {}
            }
        }
        let agreed = seen_token.unwrap_or(SendToken {
            revision: review.revision,
            recipients: review.recipients,
        });
        let recipients = agreed.recipients;
        let key = send_key(campaign_id, agreed.revision, &send_at, &time_zone);
        let body = json!({"workspaceId":workspace_id, "sendAt":send_at, "timeZone":time_zone,
            "revision":agreed.revision, "expectedRecipients":recipients,
            "expectedReachDigest":review.reach.digest, "expectedFrom":review.reach.from});
        let answer = self
            .email_call(
                "POST",
                &format!("/campaigns/{campaign_id}/schedule"),
                Some(&body),
                Some(&key),
            )
            .await?;
        if answer.status == 409 && answer.error_code() == Some("campaign_changed") {
            return Ok(match self.send_review(workspace_id, campaign_id).await? {
                Ok(now) => now.show(
                    "Not sent: the campaign changed. Ask the person again.",
                    &send_at,
                    &time_zone,
                    Some(recipients),
                ),
                Err(why) => ToolResult::error(why),
            });
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
        let name = &review.name;
        let target = review.target();
        let when = when_words(&send_at, &time_zone);
        let scheduled = answer.data;
        let state = text(&scheduled, "status").unwrap_or_default();
        if !matches!(state.as_str(), "scheduled" | "testing" | "sending" | "sent") {
            // A repeated key answers the campaign as it is: never say it went when it didn't.
            return Ok(ToolResult::error(format!(
                "Not sent: Create kept “{name}” as {}. Read it with email_campaigns and ask the person again.",
                if state.is_empty() { "it was" } else { state.as_str() }
            )));
        }
        let mut lines = vec![if state == "testing" {
            format!("The A/B send for “{name}” has started in {target}. The sample is testing the subjects, and the remaining recipients will get the winner.")
        } else if send_at == "now" {
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
