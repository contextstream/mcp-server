//! A send in flight: `email_pause` stops it where it is, and `email_resume` sends to the
//! people still waiting, only with the person's go for that count.

use mcp_types::{tool::ToolResult, Result};
use serde_json::{json, Value};
use uuid::Uuid;

use super::go::{persons_go, Go, Question, ResumeToken, ASKED_NOTE};
use super::{campaign_url, digest, email_refusal, given, moment, number, people, text, Topic};
use crate::domains::studio::words::credits;
use crate::domains::studio::{StudioInput, StudioTool};

/// Whether a campaign wrote its recipients down, which it does when it starts.
fn started(campaign: &Value) -> bool {
    number(
        campaign.get("progress").unwrap_or(&Value::Null),
        "recipients",
    )
    .is_some_and(|n| n > 0)
}

/// Who still waits, as the campaign itself answers: Create's `remaining` where it gives
/// one, else the sends a started campaign still holds.
fn waiting(campaign: &Value) -> Option<i64> {
    number(campaign, "remaining").or_else(|| {
        started(campaign)
            .then(|| number(campaign.get("progress").unwrap_or(&Value::Null), "queued"))
            .flatten()
    })
}

fn sent(campaign: &Value) -> Option<i64> {
    number(campaign.get("progress").unwrap_or(&Value::Null), "sent")
}

/// When a campaign is scheduled for, in words.
fn scheduled_for(campaign: &Value) -> Option<String> {
    moment(campaign.pointer("/schedule/sendAt").and_then(Value::as_str))
}

struct ResumeReview {
    workspace_id: Uuid,
    campaign_id: Uuid,
    name: String,
    subject: String,
    revision: i64,
    begun: bool,
    remaining: i64,
    went: Option<i64>,
    when: Option<String>,
    sender: Value,
    audience_id: Option<String>,
    audience: Value,
    segment_id: Option<String>,
    segment: Value,
    schedule: Value,
    checks_remaining: bool,
}

impl ResumeReview {
    fn what(&self) -> String {
        if self.begun {
            let progress = match self.went {
                Some(went) => format!(
                    "{} of {} sent so far",
                    credits(went),
                    credits(went + self.remaining)
                ),
                None => format!("sent so far: {}", super::NOT_ANSWERED),
            };
            format!(
                "“{}” (subject “{}”) to the {} still waiting ({progress})",
                self.name,
                self.subject,
                people(self.remaining),
            )
        } else {
            format!(
                "“{}” (subject “{}”) back on its schedule{} to {}",
                self.name,
                self.subject,
                self.when
                    .as_ref()
                    .map_or(String::new(), |at| format!(" ({at})")),
                people(self.remaining)
            )
        }
    }

    fn token(&self) -> String {
        let mac = digest(
            &json!([
                self.workspace_id,
                self.campaign_id,
                self.revision,
                self.begun,
                self.remaining,
                self.went,
                self.name,
                self.subject,
                self.sender,
                self.audience_id,
                self.audience,
                self.segment_id,
                self.segment,
                self.schedule,
            ])
            .to_string(),
        );
        ResumeToken {
            revision: self.revision,
            remaining: self.remaining,
        }
        .encode(&mac)
    }

    fn show(&self, reason: &str) -> ToolResult {
        let what = self.what();
        let token = self.token();
        ToolResult::with_structured(
            format!(
                "{reason} Show them: {what}. If they agree, use the arguments in confirm_with to call email_resume again with campaign_id {}, confirm_remaining {} and confirm_review {token}.",
                self.campaign_id, self.remaining
            ),
            json!({
                "resumed": false,
                "campaign_id": self.campaign_id,
                "revision": self.revision,
                "remaining": self.remaining,
                "sent": self.went,
                "subject": self.subject,
                "confirm_with": {
                    "action": "email_resume",
                    "campaign_id": self.campaign_id,
                    "confirm_remaining": self.remaining,
                    "confirm_review": token,
                },
            }),
        )
    }
}

impl StudioTool {
    // ------------------------------------------------------------------------------------
    // email_pause
    // ------------------------------------------------------------------------------------

    pub(super) async fn email_pause(
        &self,
        workspace_id: Uuid,
        input: &StudioInput,
    ) -> Result<ToolResult> {
        let campaign_id = Self::campaign_id(input, "email_pause")?;
        let answer = self
            .email_call(
                "POST",
                &format!("/campaigns/{campaign_id}/pause"),
                Some(&json!({"workspaceId": workspace_id})),
                None,
            )
            .await?;
        if !answer.is_success() {
            return Ok(ToolResult::error(format!(
                "Not paused: {}",
                email_refusal(&answer, Topic::Campaign)
            )));
        }
        let campaign = answer.data;
        let name = text(&campaign, "name").unwrap_or_else(|| "The campaign".to_string());
        let went = sent(&campaign);
        let wait = waiting(&campaign);
        let said = if started(&campaign) {
            format!(
                "Paused “{name}”: {} sent, {} still wait.",
                super::said(went),
                super::said(wait)
            )
        } else {
            format!(
                "Paused “{name}” before it started: nothing was sent{}.",
                wait.map_or(String::new(), |n| format!(", and {} wait", people(n)))
            )
        };
        Ok(ToolResult::with_structured(
            format!(
                "{said}\nemail_resume sends to the rest, with the person's go; email_cancel stops it for good."
            ),
            json!({
                "campaign_id": campaign_id,
                "status": campaign.get("status"),
                "sent": went,
                "waiting": wait,
                "follow_up": {"action": "email_resume", "campaign_id": campaign_id},
            }),
        ))
    }

    // ------------------------------------------------------------------------------------
    // email_resume
    // ------------------------------------------------------------------------------------

    async fn resume_review(
        &self,
        workspace_id: Uuid,
        campaign_id: Uuid,
    ) -> Result<std::result::Result<ResumeReview, String>> {
        let campaign = match self.read_campaign(workspace_id, campaign_id).await? {
            Ok(campaign) => campaign,
            Err(refused) => return Ok(Err(format!("Not resumed: {refused}"))),
        };
        let name = text(&campaign, "name").unwrap_or_else(|| "The campaign".to_string());
        let status = text(&campaign, "status").unwrap_or_default();
        if status != "paused" {
            let how = match status.as_str() {
                "draft" | "scheduled" => "email_send sends it, with the person's go.",
                "sending" | "testing" => "It is going out now; email_report follows it.",
                _ => "email_duplicate makes a draft copy to send again.",
            };
            return Ok(Err(format!(
                "Not resumed: only a paused campaign can be resumed, and “{name}” is {status}. {how}"
            )));
        }
        let Some(revision) = number(&campaign, "revision") else {
            return Ok(Err(
                "Not resumed: campaign revision not answered. Read the campaign again before confirming."
                    .into(),
            ));
        };
        let begun = started(&campaign);
        // Who it would reach: its own count of who waits; a campaign paused before it
        // started goes back on its schedule, to whoever its audience holds then.
        let remaining = match waiting(&campaign) {
            Some(remaining) => Some(remaining),
            None if !begun => self
                .preview(workspace_id, campaign_id, None)
                .await
                .ok()
                .and_then(|preview| number(&preview, "recipients")),
            None => None,
        };
        let Some(remaining) = remaining else {
            return Ok(Err(format!(
                "Not resumed: Create did not answer how many people still wait for “{name}”, so there is no count for the person to agree to. They can resume it in the studio: {}",
                campaign_url(&campaign_id.to_string())
            )));
        };
        let subject = text(&campaign, "subject").unwrap_or_else(|| super::NOT_ANSWERED.to_string());
        Ok(Ok(ResumeReview {
            workspace_id,
            campaign_id,
            name,
            subject,
            revision,
            begun,
            remaining,
            went: sent(&campaign),
            when: scheduled_for(&campaign),
            sender: campaign.get("sender").cloned().unwrap_or(Value::Null),
            audience_id: text(&campaign, "audienceId")
                .or_else(|| text(&campaign["audience"], "id")),
            audience: campaign.get("audience").cloned().unwrap_or(Value::Null),
            segment_id: text(&campaign, "segmentId").or_else(|| text(&campaign["segment"], "id")),
            segment: campaign.get("segment").cloned().unwrap_or(Value::Null),
            schedule: campaign.get("schedule").cloned().unwrap_or(Value::Null),
            checks_remaining: number(&campaign, "remaining").is_some(),
        }))
    }

    /// Resumes a paused send, only with the person's go for who still waits.
    pub(super) async fn email_resume(
        &self,
        workspace_id: Uuid,
        input: &StudioInput,
    ) -> Result<ToolResult> {
        let campaign_id = Self::campaign_id(input, "email_resume")?;
        let review = match self.resume_review(workspace_id, campaign_id).await? {
            Ok(review) => review,
            Err(why) => return Ok(ToolResult::error(why)),
        };
        let token = review.token();
        let seen_token = match given(input.confirm_review.as_deref()) {
            Some(raw) => match ResumeToken::parse(raw).filter(|_| raw == token) {
                Some(seen) => input
                    .confirm_remaining
                    .is_none_or(|n| n == seen.remaining)
                    .then_some(seen),
                None => {
                    return Ok(review.show(
                        "Not resumed: the campaign changed since review. Ask the person again.",
                    ));
                }
            },
            None => None,
        };
        let what = review.what();
        let question_key = format!("confirm-resume-{token}");
        let mut notes: Vec<String> = Vec::new();
        let go = persons_go(
            Question {
                key: &question_key,
                message: format!("Resume {what}? Once it goes, it can't be unsent."),
                title: "Resume it",
                does: format!("Sends to {}.", people(review.remaining)),
                // A go for another pause of it, or another count, is not taken.
                state: format!(
                    "studio-email-resume:{campaign_id}:{}:{}",
                    review.revision, review.remaining
                ),
            },
            seen_token.is_some(),
            input.confirm_remaining.is_some(),
        )
        .await;
        match go {
            Go::Yes { asked } => {
                if asked {
                    notes.push(ASKED_NOTE.to_string());
                }
            }
            Go::Asking => {
                return Ok(ToolResult::with_structured(
                    format!("Asking the person before {what} is resumed."),
                    json!({"resumed": false, "campaign_id": campaign_id, "asking": true}),
                ))
            }
            Go::No => {
                return Ok(ToolResult::with_structured(
                    format!(
                        "Not resumed: the person didn't say go to resuming {what}. It stays paused."
                    ),
                    json!({"resumed": false, "campaign_id": campaign_id, "declined": true}),
                ))
            }
            Go::NotAsked => {
                return Ok(ToolResult::with_structured(
                    format!(
                        "Not resumed: {}. It stays paused.",
                        crate::go_ahead::NOT_ASKED
                    ),
                    json!({"resumed": false, "campaign_id": campaign_id, "go_ahead": "asked_only"}),
                ))
            }
            Go::Needed { unanswered } => {
                let reason = match input.confirm_remaining {
                    Some(confirmed) if confirmed != review.remaining => format!(
                        "Not resumed: {} still wait now, not the {} the person agreed to.",
                        people(review.remaining),
                        credits(confirmed)
                    ),
                    _ if unanswered => {
                        "Not resumed yet: the person was asked through the client and gave no answer in time."
                            .to_string()
                    }
                    Some(_) => {
                        "Not resumed yet: confirm_review from showing this review is needed; a count alone is not a go."
                            .to_string()
                    }
                    None => "Not resumed yet: the person's go is needed first.".to_string(),
                };
                return Ok(review.show(&reason));
            }
        }

        if crate::peer::current().is_some_and(|p| p.can_elicit() && !p.asks_through_the_result()) {
            match self.resume_review(workspace_id, campaign_id).await? {
                Ok(now) if now.token() != token => return Ok(now.show(
                    "Not resumed: the campaign changed during confirmation. Ask the person again.",
                )),
                Err(why) => return Ok(ToolResult::error(why)),
                _ => {}
            }
        }

        let agreed = seen_token.unwrap_or(ResumeToken {
            revision: review.revision,
            remaining: review.remaining,
        });
        let remaining = agreed.remaining;
        // One key per pause: each pause moves the revision on, so a retried resume is one
        // resume. A Create that answers `remaining` also takes the count the person saw.
        let key = format!("email-resume-{campaign_id}-r{}", agreed.revision);
        let mut body = json!({"workspaceId": workspace_id});
        let mut newer: Vec<&str> = Vec::new();
        if review.checks_remaining {
            body["expectedRemaining"] = json!(remaining);
            newer.push("expectedRemaining");
        }
        let (answer, kept) = self
            .email_call_newer(
                "POST",
                &format!("/campaigns/{campaign_id}/resume"),
                &body,
                &newer,
                Some(&key),
            )
            .await?;
        if !kept || body.get("expectedRemaining").is_none() {
            notes.push("Remaining recipients at resume time: not answered by this API.".into());
        }
        if !answer.is_success() {
            let refused = email_refusal(&answer, Topic::Campaign);
            return Ok(ToolResult::error(format!("Not resumed: {refused}")));
        }
        let resumed = answer.data;
        let name = &review.name;
        let now = text(&resumed, "status").unwrap_or_default();
        let mut lines = vec![match now.as_str() {
            "sending" | "testing" | "sent" => format!(
                "Resumed “{name}”: it goes on to the {} still waiting.",
                people(remaining)
            ),
            "scheduled" => format!(
                "Resumed “{name}”: it is scheduled again{}, to {}.",
                scheduled_for(&resumed)
                    .or(review.when)
                    .map_or(String::new(), |at| format!(", for {at}")),
                people(remaining)
            ),
            // Never say it went on when Create says it didn't.
            other => {
                return Ok(ToolResult::error(format!(
                    "Not resumed: Create kept “{name}” as {}. Read it with email_campaigns and ask the person again.",
                    if other.is_empty() { "it was" } else { other }
                )))
            }
        }];
        lines.extend(notes);
        lines.push(format!(
            "email_pause with campaign_id {campaign_id} stops it again; email_report follows it."
        ));
        Ok(ToolResult::with_structured(
            lines.join("\n"),
            json!({
                "resumed": true,
                "campaign_id": campaign_id,
                "status": now,
                "remaining": remaining,
                "schedule": resumed.get("schedule"),
                "studio_url": campaign_url(&campaign_id.to_string()),
                "follow_up": {"action": "email_report", "campaign_id": campaign_id},
            }),
        ))
    }
}
