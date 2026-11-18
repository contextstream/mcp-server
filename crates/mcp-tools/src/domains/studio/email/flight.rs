//! A send in flight: `email_pause` stops it where it is, and `email_resume` sends to the
//! people still waiting, only with the person's go for that count.

use mcp_types::{tool::ToolResult, Result};
use serde_json::{json, Value};
use uuid::Uuid;

use super::go::{persons_go, Go, Question, ASKED_NOTE};
use super::{campaign_url, email_refusal, moment, number, people, text, Topic};
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

    /// Resumes a paused send, only with the person's go for who still waits.
    pub(super) async fn email_resume(
        &self,
        workspace_id: Uuid,
        input: &StudioInput,
    ) -> Result<ToolResult> {
        let campaign_id = Self::campaign_id(input, "email_resume")?;
        let campaign = match self.read_campaign(workspace_id, campaign_id).await? {
            Ok(campaign) => campaign,
            Err(refused) => return Ok(ToolResult::error(format!("Not resumed: {refused}"))),
        };
        let name = text(&campaign, "name").unwrap_or_else(|| "The campaign".to_string());
        let status = text(&campaign, "status").unwrap_or_default();
        if status != "paused" {
            let how = match status.as_str() {
                "draft" | "scheduled" => "email_send sends it, with the person's go.",
                "sending" | "testing" => "It is going out now; email_report follows it.",
                _ => "email_duplicate makes a draft copy to send again.",
            };
            return Ok(ToolResult::error(format!(
                "Not resumed: only a paused campaign can be resumed, and “{name}” is {status}. {how}"
            )));
        }
        let revision = number(&campaign, "revision").unwrap_or(0);
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
            return Ok(ToolResult::error(format!(
                "Not resumed: Create did not answer how many people still wait for “{name}”, so there is no count for the person to agree to. They can resume it in the studio: {}",
                campaign_url(&campaign_id.to_string())
            )));
        };
        let subject = text(&campaign, "subject").unwrap_or_default();
        let when = scheduled_for(&campaign);
        let what = if begun {
            let went = sent(&campaign).unwrap_or(0);
            format!(
                "“{name}” (subject “{subject}”) to the {} still waiting ({} of {} sent so far)",
                people(remaining),
                credits(went),
                credits(went + remaining)
            )
        } else {
            format!(
                "“{name}” (subject “{subject}”) back on its schedule{} to {}",
                when.as_ref().map_or(String::new(), |at| format!(" ({at})")),
                people(remaining)
            )
        };

        let mut notes: Vec<String> = Vec::new();
        let go = persons_go(
            Question {
                key: "confirm-resume",
                message: format!("Resume {what}? Once it goes, it can't be unsent."),
                title: "Resume it",
                does: format!("Sends to {}.", people(remaining)),
                // A go for another pause of it, or another count, is not taken.
                state: format!("studio-email-resume:{campaign_id}:{revision}:{remaining}"),
            },
            input.confirm_remaining == Some(remaining),
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
                let mut said = match input.confirm_remaining {
                    Some(confirmed) => format!(
                        "Not resumed: {} still wait now, not the {} the person agreed to.",
                        people(remaining),
                        credits(confirmed)
                    ),
                    None if unanswered => "Not resumed yet: the person was asked through the client and gave no answer in time.".to_string(),
                    None => "Not resumed yet: the person's go is needed first.".to_string(),
                };
                said.push_str(&format!(
                    " Show them: {what}. If they agree, call email_resume again with campaign_id {campaign_id} and confirm_remaining {remaining}."
                ));
                return Ok(ToolResult::with_structured(
                    said,
                    json!({
                        "resumed": false,
                        "campaign_id": campaign_id,
                        "remaining": remaining,
                        "sent": sent(&campaign),
                        "subject": subject,
                        "confirm_with": {
                            "action": "email_resume",
                            "campaign_id": campaign_id,
                            "confirm_remaining": remaining,
                        },
                    }),
                ));
            }
        }

        // One key per pause: each pause moves the revision on, so a retried resume is one
        // resume. A Create that answers `remaining` also takes the count the person saw.
        let key = format!("email-resume-{campaign_id}-r{revision}");
        let mut body = json!({"workspaceId": workspace_id});
        let mut newer: Vec<&str> = Vec::new();
        if number(&campaign, "remaining").is_some() {
            body["expectedRemaining"] = json!(remaining);
            newer.push("expectedRemaining");
        }
        let (answer, _) = self
            .email_call_newer(
                "POST",
                &format!("/campaigns/{campaign_id}/resume"),
                &body,
                &newer,
                Some(&key),
            )
            .await?;
        if !answer.is_success() {
            let refused = email_refusal(&answer, Topic::Campaign);
            return Ok(ToolResult::error(format!("Not resumed: {refused}")));
        }
        let resumed = answer.data;
        let now = text(&resumed, "status").unwrap_or_default();
        let mut lines = vec![match now.as_str() {
            "sending" | "testing" | "sent" => format!(
                "Resumed “{name}”: it goes on to the {} still waiting.",
                people(remaining)
            ),
            "scheduled" => format!(
                "Resumed “{name}”: it is scheduled again{}, to {}.",
                scheduled_for(&resumed)
                    .or(when)
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
