//! What a draft starts from and how its subjects are tried: `email_templates` (the team's
//! reusable emails and Create's built-in ones: list, read, save a campaign as one, start a
//! draft from one, rename) and `email_ab` (a draft's subject tried against one or two
//! others on a sample of its recipients).

use mcp_types::{tool::ToolResult, Error, Result};
use serde_json::{json, Value};
use uuid::Uuid;

use super::{
    campaign_url, description_of, email_refusal, given, moment, name_of, number, operation_of,
    operation_or, outline, said, strings, subjects_words, text, Topic, MAX_LINE_CHARS,
    NOT_ANSWERED, VARIANT_LABELS,
};
use crate::domains::studio::{parse_id, StudioInput, StudioTool};

const TEMPLATE_OPERATIONS: &[&str] = &["list", "get", "save", "use", "update"];
const AB_OPERATIONS: &[&str] = &["show", "set", "clear"];
const METRICS: &[&str] = &["opens", "clicks"];
/// The share of recipients a test is tried on, and how long it waits (Create's limits).
const SPLIT: std::ops::RangeInclusive<i64> = 10..=50;
const DECIDE_AFTER: std::ops::RangeInclusive<i64> = 1..=72;
const PLACEHOLDERS: &str =
    "Words in [square brackets] are placeholders: email_revise fills them in on the draft.";

/// A template's id as Create takes it in a path: a UUID, or a built-in's `builtin:<name>`.
pub(super) fn template_id_of(input: &StudioInput, call: &str) -> Result<String> {
    let raw = given(input.template_id.as_deref()).ok_or_else(|| {
        Error::Validation(format!(
            "template_id is required for {call} (email_templates lists them)"
        ))
    })?;
    if let Ok(id) = Uuid::parse_str(raw) {
        return Ok(id.to_string());
    }
    let built_in = raw.strip_prefix("builtin:").is_some_and(|name| {
        (1..=40).contains(&name.len())
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    });
    if built_in {
        Ok(raw.to_string())
    } else {
        Err(Error::Validation(
            "template_id is a template's id as email_templates lists it, like builtin:announcement"
                .to_string(),
        ))
    }
}

fn is_built_in(template: &Value) -> bool {
    template.get("builtIn").and_then(Value::as_bool) == Some(true)
}

/// A campaign's test in words: "A “…”, B “…”, tried on 20% of its recipients; after 4
/// hours the subject with the most opens goes to the rest."
fn test_words(own_subject: &str, test: &Value) -> String {
    let mut subjects = vec![own_subject.to_string()];
    subjects.extend(strings(test.get("subjects")));
    let by = match text(test, "metric") {
        Some(metric) => format!("the most {metric}"),
        None => format!("metric {NOT_ANSWERED}"),
    };
    format!(
        "{}, tried on {}% of its recipients; after {} hours the subject with {by} goes to the rest.",
        subjects_words(&subjects),
        said(number(test, "samplePercent")),
        said(number(test, "waitHours")),
    )
}

/// The winner of a decided test, in words.
fn winner_words(test: &Value) -> Option<String> {
    let winner = test.get("winner").filter(|w| w.is_object())?;
    let label = number(winner, "variant")
        .and_then(|v| usize::try_from(v).ok())
        .and_then(|v| VARIANT_LABELS.get(v))
        .copied();
    let why = match text(winner, "reason").as_deref() {
        Some("opens") => "the most opens",
        Some("clicks") => "the most clicks",
        Some("tie") => "a tie, which goes to A",
        Some("no_signal") => "no opens or clicks to tell by, which goes to A",
        _ => "how it was picked is not answered",
    };
    let subject = text(winner, "subject").unwrap_or_else(|| NOT_ANSWERED.to_string());
    let at = moment(winner.get("decidedAt").and_then(Value::as_str))
        .map_or(String::new(), |at| format!(", picked on {at}"));
    Some(match label {
        Some(label) => format!("The winner was {label} “{subject}” ({why}){at}."),
        None => format!("The winner (variant {NOT_ANSWERED}) was “{subject}” ({why}){at}."),
    })
}

impl StudioTool {
    // ------------------------------------------------------------------------------------
    // email_templates
    // ------------------------------------------------------------------------------------

    pub(super) async fn email_templates(
        &self,
        workspace_id: Uuid,
        input: &StudioInput,
    ) -> Result<ToolResult> {
        let operation = operation_of(
            input.operation.as_deref(),
            TEMPLATE_OPERATIONS,
            "email_templates",
        )?;
        match operation.as_str() {
            "get" => self.read_template(workspace_id, input).await,
            "save" => self.save_template(workspace_id, input).await,
            "use" => self.use_template(workspace_id, input).await,
            "update" => self.update_template(workspace_id, input).await,
            _ => self.list_templates(workspace_id).await,
        }
    }

    async fn list_templates(&self, workspace_id: Uuid) -> Result<ToolResult> {
        let answer = self
            .email_call(
                "GET",
                &format!("/templates?workspaceId={workspace_id}"),
                None,
                None,
            )
            .await?;
        if !answer.is_success() {
            return Ok(ToolResult::error(email_refusal(&answer, Topic::Other)));
        }
        let Some(templates) = answer.data["templates"].as_array() else {
            return Ok(ToolResult::error(
                "Template list not answered. Try email_templates again.",
            ));
        };
        let mut lines = vec![format!(
            "{} template{} as of now:",
            templates.len(),
            if templates.len() == 1 { "" } else { "s" }
        )];
        let mut listed = Vec::new();
        for template in templates {
            let id = text(template, "id").unwrap_or_default();
            let name = text(template, "name").unwrap_or_else(|| "A template".to_string());
            let about = match (is_built_in(template), text(template, "description")) {
                (true, Some(description)) => format!(" (built in): {description}"),
                (true, None) => " (built in)".to_string(),
                (false, Some(description)) => format!(": {description}"),
                (false, None) => format!(
                    ": subject “{}”",
                    text(template, "subject").unwrap_or_default()
                ),
            };
            lines.push(format!("- {name}{about} (template_id {id})"));
            listed.push(json!({
                "template_id": id,
                "name": name,
                "description": template.get("description"),
                "subject": template.get("subject"),
                "built_in": is_built_in(template),
            }));
        }
        lines.push(
            "email_templates with operation use and a template_id starts a draft from one; operation save and a campaign_id keeps a campaign as one."
                .to_string(),
        );
        Ok(ToolResult::with_structured(
            lines.join("\n"),
            json!({"workspace_id": workspace_id, "templates": listed}),
        ))
    }

    async fn read_template(&self, workspace_id: Uuid, input: &StudioInput) -> Result<ToolResult> {
        let id = template_id_of(input, "email_templates with operation get")?;
        let answer = self
            .email_call(
                "GET",
                &format!("/templates/{id}?workspaceId={workspace_id}"),
                None,
                None,
            )
            .await?;
        if !answer.is_success() {
            return Ok(ToolResult::error(email_refusal(&answer, Topic::Template)));
        }
        let template = answer.data;
        let built_in = is_built_in(&template);
        let blocks = outline(template.get("content").unwrap_or(&Value::Null));
        let mut lines = vec![format!(
            "The template “{}” ({}template_id {id}){}",
            text(&template, "name").unwrap_or_default(),
            if built_in { "built in; " } else { "" },
            text(&template, "description").map_or_else(|| ".".to_string(), |d| format!(": {d}"))
        )];
        lines.push(format!(
            "Subject: {}",
            text(&template, "subject").unwrap_or_default()
        ));
        if let Some(preheader) = text(&template, "preheader") {
            lines.push(format!("Preheader: {preheader}"));
        }
        if !blocks.is_empty() {
            lines.push("Outline:".to_string());
            lines.extend(blocks.iter().cloned());
        }
        if built_in {
            lines.push(PLACEHOLDERS.to_string());
        }
        Ok(ToolResult::with_structured(
            lines.join("\n"),
            json!({
                "workspace_id": workspace_id,
                "template_id": id,
                "name": template.get("name"),
                "description": template.get("description"),
                "subject": template.get("subject"),
                "preheader": template.get("preheader"),
                "outline": blocks,
                "built_in": built_in,
            }),
        ))
    }

    /// Keeps a campaign as a template: its subject, preview line and blocks.
    async fn save_template(&self, workspace_id: Uuid, input: &StudioInput) -> Result<ToolResult> {
        let call = "email_templates with operation save";
        let campaign_id = Self::campaign_id(input, call)?;
        let name = name_of(input, call)?.ok_or_else(|| {
            Error::Validation(format!(
                "name is required for {call}: what the template is called"
            ))
        })?;
        let mut body =
            json!({"workspaceId": workspace_id, "name": name, "campaignId": campaign_id});
        if let Some(Some(description)) = description_of(input)? {
            body["description"] = json!(description);
        }
        let answer = self
            .email_call("POST", "/templates", Some(&body), None)
            .await?;
        if !answer.is_success() {
            return Ok(ToolResult::error(format!(
                "Not saved: {}",
                email_refusal(&answer, Topic::Campaign)
            )));
        }
        let template = answer.data;
        let id = text(&template, "id").unwrap_or_default();
        Ok(ToolResult::with_structured(
            format!(
                "Saved the template “{}” (template_id {id}) from the campaign: its subject, preview line and blocks. A draft made from it takes the team's brand as it is then: email_templates with operation use.",
                text(&template, "name").unwrap_or(name)
            ),
            json!({
                "workspace_id": workspace_id,
                "template_id": id,
                "name": template.get("name"),
                "saved_from": campaign_id,
            }),
        ))
    }

    /// A new draft from a template. Sends nothing.
    async fn use_template(&self, workspace_id: Uuid, input: &StudioInput) -> Result<ToolResult> {
        let call = "email_templates with operation use";
        let id = template_id_of(input, call)?;
        let mut body = json!({"workspaceId": workspace_id, "templateId": id});
        if let Some(name) = name_of(input, call)? {
            body["name"] = json!(name);
        }
        if let Some(audience) = given(input.audience_id.as_deref()) {
            body["audienceId"] = json!(parse_id(audience, "audience_id")?);
        }
        if let Some(segment) = given(input.segment_id.as_deref()) {
            body["segmentId"] = json!(parse_id(segment, "segment_id")?);
        }
        let answer = self
            .email_call("POST", "/campaigns", Some(&body), None)
            .await?;
        if !answer.is_success() {
            return Ok(ToolResult::error(format!(
                "Not started: {}",
                email_refusal(&answer, Topic::Template)
            )));
        }
        let campaign = answer.data;
        let campaign_id = text(&campaign, "id").unwrap_or_default();
        let name = text(&campaign, "name").unwrap_or_else(|| "The draft".to_string());
        let blocks = outline(campaign.get("content").unwrap_or(&Value::Null));
        let url = campaign_url(&campaign_id);
        let mut lines = vec![format!(
            "Started the draft “{name}” from the template (campaign_id {campaign_id}). Nothing is sent."
        )];
        lines.push(format!(
            "Subject: {}",
            text(&campaign, "subject").unwrap_or_default()
        ));
        if !blocks.is_empty() {
            lines.push("Outline:".to_string());
            lines.extend(blocks.iter().cloned());
        }
        lines.push(
            match campaign.pointer("/audience/name").and_then(Value::as_str) {
                Some(audience) => format!("To: {audience}."),
                None => "No audience chosen yet: email_audiences lists them.".to_string(),
            },
        );
        if id.starts_with("builtin:") {
            lines.push(PLACEHOLDERS.to_string());
        }
        lines.push(format!(
            "Next, with campaign_id {campaign_id}: email_revise changes it from plain words, email_preview shows it in mail-client mockups, email_ab tries two or three subjects, email_test sends a test. Edit it in the studio: {url}"
        ));
        Ok(ToolResult::with_structured(
            lines.join("\n"),
            json!({
                "workspace_id": workspace_id,
                "campaign_id": campaign_id,
                "template_id": id,
                "name": name,
                "status": campaign.get("status"),
                "subject": campaign.get("subject"),
                "outline": blocks,
                "audience": campaign.get("audience"),
                "studio_url": url,
            }),
        ))
    }

    async fn update_template(&self, workspace_id: Uuid, input: &StudioInput) -> Result<ToolResult> {
        let call = "email_templates with operation update";
        let id = template_id_of(input, call)?;
        let name = name_of(input, call)?;
        let description = description_of(input)?;
        if name.is_none() && description.is_none() {
            return Err(Error::Validation(format!(
                "{call} needs something to change: name or description"
            )));
        }
        let mut body = json!({"workspaceId": workspace_id});
        if let Some(name) = &name {
            body["name"] = json!(name);
        }
        if let Some(description) = &description {
            body["description"] = json!(description);
        }
        let answer = self
            .email_call("PATCH", &format!("/templates/{id}"), Some(&body), None)
            .await?;
        if !answer.is_success() {
            return Ok(ToolResult::error(format!(
                "Not changed: {}",
                email_refusal(&answer, Topic::Template)
            )));
        }
        let template = answer.data;
        let actual_name = text(&template, "name");
        let actual_desc = text(&template, "description");
        let mut applied = 0usize;
        let mut unapplied: Vec<&'static str> = Vec::new();
        if let Some(asked_name) = &name {
            if actual_name.as_deref() == Some(asked_name.as_str()) {
                applied += 1;
            } else {
                unapplied.push("the name was not changed: this API did not take it");
            }
        }
        if let Some(asked_desc) = &description {
            match asked_desc {
                None if actual_desc.is_none() => applied += 1,
                None => unapplied.push("the description was not cleared: this API did not take it"),
                Some(wanted) if actual_desc.as_deref() == Some(wanted.as_str()) => applied += 1,
                Some(_) => {
                    unapplied.push("the description was not changed: this API did not take it")
                }
            }
        }
        let shown_name = actual_name.as_deref().unwrap_or(super::NOT_ANSWERED);
        let changed = applied > 0;
        let message = if unapplied.is_empty() {
            format!(
                "Changed the template: “{shown_name}” (template_id {id}).{}",
                actual_desc
                    .as_deref()
                    .map_or(String::new(), |d| format!(" {d}"))
            )
        } else if !changed {
            format!(
                "Not changed on “{shown_name}” (template_id {id}): {}.{}",
                unapplied.join("; "),
                actual_desc
                    .as_deref()
                    .map_or(String::new(), |d| format!(" Still “{d}”."))
            )
        } else {
            format!(
                "Updated “{shown_name}” (template_id {id}), except {}.{}",
                unapplied.join("; "),
                actual_desc
                    .as_deref()
                    .map_or(String::new(), |d| format!(" Still “{d}”."))
            )
        };
        let mut out = ToolResult::with_structured(
            message,
            json!({
                "workspace_id": workspace_id,
                "template_id": id,
                "changed": changed,
                "name": template.get("name"),
                "description": template.get("description"),
            }),
        );
        out.is_error = !changed;
        Ok(out)
    }

    // ------------------------------------------------------------------------------------
    // email_ab
    // ------------------------------------------------------------------------------------

    pub(super) async fn email_ab(
        &self,
        workspace_id: Uuid,
        input: &StudioInput,
    ) -> Result<ToolResult> {
        let operation = operation_or(
            input.operation.as_deref(),
            AB_OPERATIONS,
            "show",
            "email_ab",
        )?;
        let campaign_id = Self::campaign_id(input, "email_ab")?;
        // What a set asks for is checked before anything is read.
        let asked = match operation.as_str() {
            "set" => Some(Self::ab_asked(input)?),
            _ => None,
        };
        let campaign = match self.read_campaign(workspace_id, campaign_id).await? {
            Ok(campaign) => campaign,
            Err(refused) => {
                return Ok(ToolResult::error(if operation == "show" {
                    refused
                } else {
                    format!("Not changed: {refused}")
                }))
            }
        };
        let name = text(&campaign, "name").unwrap_or_else(|| "The campaign".to_string());
        let own_subject = text(&campaign, "subject").unwrap_or_default();
        if operation == "show" {
            return Ok(Self::ab_result(None, campaign_id, &name, &campaign));
        }
        let status = text(&campaign, "status").unwrap_or_default();
        if status != "draft" {
            let how = match status.as_str() {
                "scheduled" => "email_cancel unschedules it, and it is a draft again.",
                _ => "email_duplicate makes a draft copy to test.",
            };
            return Ok(ToolResult::error(format!(
                "Not changed: only a draft's subjects can be tested, and “{name}” is {status}. {how}"
            )));
        }
        let Some(revision) = number(&campaign, "revision") else {
            return Ok(ToolResult::error(
                "Not changed: campaign revision not answered. Read the draft again.",
            ));
        };
        let mut body = json!({"workspaceId":workspace_id,"revision":revision});
        match asked {
            Some((subjects, settings)) => {
                // A is the email's own subject: a new A becomes it, in the same change.
                let (a, others) = subjects.split_first().expect("2 or 3 subjects");
                if a != own_subject.trim() {
                    body["subject"] = json!(a);
                }
                let mut test = settings;
                test["subjects"] = json!(others);
                body["abTest"] = test;
            }
            None => body["abTest"] = Value::Null,
        }
        let answer = self
            .email_call(
                "PATCH",
                &format!("/campaigns/{campaign_id}"),
                Some(&body),
                None,
            )
            .await?;
        if !answer.is_success() {
            let refused = email_refusal(&answer, Topic::Campaign);
            let refused = if answer.status == 409 {
                format!("{refused} Read it again with email_ab, then set it once more.")
            } else {
                refused
            };
            return Ok(ToolResult::error(format!("Not changed: {refused}")));
        }
        Ok(Self::ab_result(
            Some(operation.as_str()),
            campaign_id,
            &name,
            &answer.data,
        ))
    }

    /// The subjects and settings a `set` asks for: 2 or 3 subject lines with A first, and
    /// Create's `samplePercent`, `waitHours` and `metric` where given.
    fn ab_asked(input: &StudioInput) -> Result<(Vec<String>, Value)> {
        let shape = || {
            Error::Validation(
                "subjects is 2 or 3 subject lines, A first (A is the email's own subject: list the current one first to keep it)"
                    .to_string(),
            )
        };
        let subjects: Vec<String> = match &input.subjects {
            Some(Value::Array(items)) => items
                .iter()
                .map(|item| {
                    item.as_str()
                        .map(|s| s.trim().to_string())
                        .ok_or_else(shape)
                })
                .collect::<Result<_>>()?,
            _ => return Err(shape()),
        };
        if !(2..=VARIANT_LABELS.len()).contains(&subjects.len())
            || subjects.iter().any(String::is_empty)
        {
            return Err(shape());
        }
        if subjects.iter().any(|s| s.chars().count() > MAX_LINE_CHARS) {
            return Err(Error::Validation(format!(
                "A subject can be up to {MAX_LINE_CHARS} characters."
            )));
        }
        for (index, subject) in subjects.iter().enumerate() {
            if subjects[..index]
                .iter()
                .any(|other| other.eq_ignore_ascii_case(subject))
            {
                return Err(Error::Validation(
                    "The subjects to test must differ from each other.".to_string(),
                ));
            }
        }
        let mut settings = json!({});
        if let Some(split) = input.split {
            if !SPLIT.contains(&split) {
                return Err(Error::Validation(
                    "split is 10 to 50: the percent of recipients the subjects are tried on"
                        .to_string(),
                ));
            }
            settings["samplePercent"] = json!(split);
        }
        if let Some(hours) = input.decide_after {
            if !DECIDE_AFTER.contains(&hours) {
                return Err(Error::Validation(
                    "decide_after is 1 to 72: the hours before the winner is picked".to_string(),
                ));
            }
            settings["waitHours"] = json!(hours);
        }
        if let Some(metric) = given(input.metric.as_deref()).map(str::to_lowercase) {
            if !METRICS.contains(&metric.as_str()) {
                return Err(Error::Validation("metric is opens or clicks".to_string()));
            }
            settings["metric"] = json!(metric);
        }
        Ok((subjects, settings))
    }

    /// A campaign's test as Create keeps it, in words; `did` is what was just done to it.
    fn ab_result(did: Option<&str>, campaign_id: Uuid, name: &str, campaign: &Value) -> ToolResult {
        let own_subject = text(campaign, "subject").unwrap_or_default();
        let test = campaign.get("abTest").filter(|t| t.is_object());
        let mut lines = Vec::new();
        match (did, test) {
            (Some("set"), Some(test)) => {
                lines.push(format!(
                    "Set the A/B test on “{name}”: {}",
                    test_words(&own_subject, test)
                ));
                lines.push(
                    "Nothing is sent: email_preview with variant shows each subject, and email_recipients says how many it would reach (under about 100 a subject is too few to tell them apart)."
                        .to_string(),
                );
            }
            (Some(_), None) => lines.push(format!(
                "Cleared the A/B test on “{name}”: it goes with its one subject, “{own_subject}”."
            )),
            (_, Some(test)) => {
                lines.push(format!(
                    "“{name}” tests {} subjects: {}",
                    strings(test.get("subjects")).len() + 1,
                    test_words(&own_subject, test)
                ));
                match winner_words(test) {
                    Some(winner) => lines.push(winner),
                    None => {
                        if let Some(at) = moment(test.get("decideAt").and_then(Value::as_str)) {
                            lines.push(format!("The winner is picked at {at}."));
                        }
                    }
                }
            }
            (None, None) => lines.push(format!(
                "“{name}” has no A/B test: it goes with its one subject, “{own_subject}”. email_ab with operation set and subjects (2 or 3, A first) tries more, on a draft."
            )),
        }
        ToolResult::with_structured(
            lines.join("\n"),
            json!({
                "campaign_id": campaign_id,
                "name": name,
                "status": campaign.get("status"),
                "subject": own_subject,
                "revision": campaign.get("revision"),
                "ab_test": campaign.get("abTest").filter(|t| t.is_object()),
            }),
        )
    }
}
