//! `email_contacts`, beyond listing and adding: a contact's fields and the audiences it is
//! in (`update`), and a CSV file of contacts (`import_preview`, then `import` on the
//! person's go for the eligible count the preview showed).

use mcp_types::{tool::ToolResult, Error, Result};
use serde_json::{json, Value};
use uuid::Uuid;

use super::go::{persons_go, Go, Question, ASKED_NOTE};
use super::{
    address_of, consent_of, digest, email_refusal, given, number, purpose_words, said, strings,
    text, unknown_field, Topic, AUDIENCES_URL,
};
use crate::domains::studio::words::credits;
use crate::domains::studio::{parse_id, StudioInput, StudioTool};

/// The largest CSV file Create reads.
const MAX_CSV_BYTES: usize = 5 * 1024 * 1024;
/// A local file read as a CSV, by its name.
const CSV_EXTENSIONS: &[&str] = &["csv", "tsv", "txt"];
/// How many invalid rows and sample addresses a preview names.
const SHOWN_ROWS: usize = 5;

/// A list of strings as given: an array, or one comma-separated string. `None` when absent.
fn string_list(value: Option<&Value>, name: &str) -> Result<Option<Vec<String>>> {
    let items: Vec<String> = match value {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(text)) => text.split(',').map(str::to_string).collect(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| {
                item.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| Error::Validation(format!("{name} is a list of strings")))
            })
            .collect::<Result<_>>()?,
        Some(_) => return Err(Error::Validation(format!("{name} is a list of strings"))),
    };
    let mut out: Vec<String> = Vec::new();
    for item in items {
        let item = item.trim().to_string();
        if !item.is_empty() && !out.contains(&item) {
            out.push(item);
        }
    }
    Ok(Some(out))
}

/// Audience ids as given (`add_to`, `remove_from`).
fn audience_ids(value: Option<&Value>, name: &str) -> Result<Vec<Uuid>> {
    string_list(value, name)?
        .unwrap_or_default()
        .iter()
        .map(|id| parse_id(id, name))
        .collect()
}

/// A name to set: `Some(None)` clears it (an empty string), `None` leaves it.
fn name_change(value: Option<&str>) -> Option<Option<String>> {
    value.map(|name| Some(name.trim().to_string()).filter(|name| !name.is_empty()))
}

/// "1 is", "3 are".
fn is_are(count: i64) -> String {
    if count == 1 {
        "1 is".to_string()
    } else {
        format!("{} are", credits(count))
    }
}

/// The audience an import goes into, in words: "the production audience “Customers”".
fn audience_words(audience: Option<&Value>) -> String {
    match audience {
        Some(audience) => {
            let name = text(audience, "name").unwrap_or_else(|| "this audience".to_string());
            match text(audience, "purpose") {
                Some(purpose) => format!("the {purpose} audience “{name}”"),
                None => format!("the audience “{name}” ({})", purpose_words(audience)),
            }
        }
        None => "the audience".to_string(),
    }
}

/// Which column was read as what: "address from “Email”, first name from “First”".
fn mapping_words(preview: &Value) -> Vec<String> {
    let header = |index: &Value| {
        let index = index.as_u64()?;
        preview
            .get("columns")?
            .as_array()?
            .iter()
            .find(|column| column.get("index").and_then(Value::as_u64) == Some(index))
            .and_then(|column| text(column, "header"))
    };
    let mapping = preview.get("mapping").cloned().unwrap_or(Value::Null);
    let mut words = Vec::new();
    for (key, role) in [
        ("email", "address"),
        ("firstName", "first name"),
        ("lastName", "last name"),
        ("name", "full name"),
        ("tags", "tags"),
    ] {
        if let Some(header) = mapping.get(key).and_then(header) {
            words.push(format!("{role} from “{header}”"));
        }
    }
    if let Some(attributes) = mapping.get("attributes").and_then(Value::as_object) {
        for (key, index) in attributes {
            if let Some(header) = header(index) {
                words.push(format!("attribute {key} from “{header}”"));
            }
        }
    }
    words
}

/// What a preview answered, as the numbers a person decides on.
struct Counts {
    rows: Option<i64>,
    valid: Option<i64>,
    invalid: Option<i64>,
    duplicates: Option<i64>,
    existing: Option<i64>,
    new: Option<i64>,
    suppressed: Option<i64>,
    /// Who the import adds that can be sent to: Create's own count when it answers one,
    /// else its valid rows less those on the suppression list.
    eligible: Option<i64>,
}

impl Counts {
    fn of(preview: &Value) -> Self {
        let valid = number(preview, "valid");
        let suppressed = number(preview, "suppressed");
        Self {
            rows: number(preview, "rows"),
            valid,
            invalid: number(preview, "invalidCount"),
            duplicates: number(preview, "duplicatesInFile"),
            existing: number(preview, "existing"),
            new: number(preview, "new"),
            suppressed,
            eligible: number(preview, "eligible").or_else(|| {
                valid
                    .zip(suppressed)
                    .map(|(valid, suppressed)| (valid - suppressed).max(0))
            }),
        }
    }
}

/// An import as the person is shown it and asked about it.
struct Import {
    csv: String,
    audience_id: Option<Uuid>,
    audience: Option<Value>,
    source: String,
    note: String,
    tags: Option<Vec<String>>,
    preview: Value,
    counts: Counts,
}

impl Import {
    /// "117 contacts into the production audience “Customers” (114 can be sent to; 3 are
    /// on the suppression list), consent form, “…”".
    fn what(&self) -> String {
        let reach = match (self.counts.eligible, self.counts.suppressed) {
            (Some(eligible), Some(suppressed)) if suppressed > 0 => format!(
                " ({} can be sent to; {} on the suppression list)",
                credits(eligible),
                is_are(suppressed)
            ),
            (Some(eligible), _) => format!(" ({} can be sent to)", credits(eligible)),
            (None, _) => String::new(),
        };
        format!(
            "{} contacts into {}{reach}, consent {}, “{}”",
            said(self.counts.valid),
            audience_words(self.audience.as_ref()),
            self.source,
            self.note
        )
    }

    fn review_token(&self) -> String {
        digest(
            &json!([
                self.audience_id,
                digest(&self.csv),
                self.preview.get("digest"),
                self.counts.eligible,
                self.source,
                self.note,
                self.tags
            ])
            .to_string(),
        )
    }

    /// The call that imports it, once the person agrees.
    fn confirm_with(&self) -> Value {
        let mut args = json!({
            "action": "email_contacts",
            "operation": "import",
            "audience_id": self.audience_id,
            "consent_source": self.source,
            "consent_note": self.note,
            "confirm_rows": self.counts.eligible,
            "confirm_review": self.review_token(),
        });
        if let Some(tags) = &self.tags {
            args["tags"] = json!(tags);
        }
        args
    }
}

impl StudioTool {
    // ------------------------------------------------------------------------------------
    // email_contacts: update
    // ------------------------------------------------------------------------------------

    pub(super) async fn update_contact(
        &self,
        workspace_id: Uuid,
        input: &StudioInput,
    ) -> Result<ToolResult> {
        let call = "email_contacts with operation update";
        let first_name = name_change(input.first_name.as_deref());
        let last_name = name_change(input.last_name.as_deref());
        let tags = string_list(input.tags.as_ref(), "tags")?;
        let add_to = audience_ids(input.add_to.as_ref(), "add_to")?;
        let remove_from = audience_ids(input.remove_from.as_ref(), "remove_from")?;
        let membership = !add_to.is_empty() || !remove_from.is_empty();
        let email = match given(input.email.as_deref()) {
            Some(raw) => Some(
                address_of(raw)
                    .ok_or_else(|| Error::Validation(format!("{raw} isn't an email address")))?,
            ),
            None => None,
        };
        let contact_id = given(input.contact_id.as_deref())
            .map(|id| parse_id(id, "contact_id"))
            .transpose()?;
        if email.is_none() && contact_id.is_none() {
            return Err(Error::Validation(format!(
                "email or contact_id is required for {call}: whose contact it is"
            )));
        }
        if first_name.is_none() && last_name.is_none() && tags.is_none() && !membership {
            return Err(Error::Validation(format!(
                "{call} needs something to change: first_name, last_name, tags, add_to or remove_from"
            )));
        }
        if add_to.iter().any(|id| remove_from.contains(id)) {
            return Err(Error::Validation(
                "An audience cannot appear in both add_to and remove_from.".into(),
            ));
        }
        let (id, audiences_now) = match &email {
            Some(email) => match self.contact_by_address(workspace_id, email).await? {
                Ok(Some(contact)) => (
                    parse_id(&text(&contact, "id").unwrap_or_default(), "contact id")?,
                    contact
                        .get("audienceIds")
                        .filter(|ids| ids.is_array())
                        .cloned(),
                ),
                Ok(None) => {
                    return Ok(ToolResult::error(format!(
                        "Not changed: there is no contact {email} in this workspace."
                    )))
                }
                Err(refused) => return Ok(ToolResult::error(format!("Not changed: {refused}"))),
            },
            None => (
                contact_id.expect("checked above: one of the two is given"),
                None,
            ),
        };
        if contact_id.is_some_and(|given| given != id) {
            return Err(Error::Validation(
                "email and contact_id must identify the same contact.".into(),
            ));
        }
        let mut body = json!({"workspaceId": workspace_id});
        if let Some(name) = &first_name {
            body["firstName"] = json!(name);
        }
        if let Some(name) = &last_name {
            body["lastName"] = json!(name);
        }
        if let Some(tags) = &tags {
            body["tags"] = json!(tags);
        }
        if !add_to.is_empty() {
            body["addAudienceIds"] = json!(add_to);
        }
        if !remove_from.is_empty() {
            body["removeAudienceIds"] = json!(remove_from);
        }
        let path = format!("/contacts/{id}");
        let mut answer = self.email_call("PATCH", &path, Some(&body), None).await?;
        if membership
            && (unknown_field(&answer, "addAudienceIds")
                || unknown_field(&answer, "removeAudienceIds"))
        {
            // Legacy servers accept only a whole membership list. Never build one from
            // missing data or drop the requested membership change to apply just the fields.
            let Some(current) = audiences_now else {
                return Ok(ToolResult::error("Not changed: membership was not answered by this API. Retry with the contact's email or use the studio."));
            };
            let mut set: Vec<Uuid> = serde_json::from_value(current).map_err(|_| {
                Error::Validation(
                    "Current audience ids were not answered. Read the contact again.".into(),
                )
            })?;
            set.retain(|id| !remove_from.contains(id));
            for id in add_to {
                if !set.contains(&id) {
                    set.push(id);
                }
            }
            let fields = body.as_object_mut().expect("contact patch is an object");
            fields.remove("addAudienceIds");
            fields.remove("removeAudienceIds");
            body["audienceIds"] = json!(set);
            answer = self.email_call("PATCH", &path, Some(&body), None).await?;
        }
        if !answer.is_success() {
            return Ok(ToolResult::error(format!(
                "Not changed: {}",
                email_refusal(&answer, Topic::Contact)
            )));
        }
        let contact = answer.data;
        let address = text(&contact, "email").unwrap_or_else(|| email.unwrap_or_default());
        let full_name = [text(&contact, "firstName"), text(&contact, "lastName")]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        let now_tags = strings(contact.get("tags"));
        let now_in = strings(contact.get("audienceIds"));
        let audiences = if now_in.is_empty() {
            Vec::new()
        } else {
            self.audiences_now(workspace_id).await.unwrap_or_default()
        };
        let names: Vec<String> = now_in
            .iter()
            .filter_map(|id| {
                audiences
                    .iter()
                    .find(|a| text(a, "id").as_deref() == Some(id.as_str()))
                    .and_then(|a| text(a, "name"))
            })
            .collect();
        let mut said = vec![if full_name.is_empty() {
            format!("Changed {address}.")
        } else {
            format!("Changed {address} ({full_name}).")
        }];
        said.push(if !contact["tags"].is_array() {
            "Tags not answered.".to_string()
        } else if now_tags.is_empty() {
            "No tags.".to_string()
        } else {
            format!("Tags: {}.", now_tags.join(", "))
        });
        said.push(if !contact["audienceIds"].is_array() {
            "Membership not answered.".to_string()
        } else if now_in.is_empty() {
            "In no audience.".to_string()
        } else if names.len() == now_in.len() {
            format!("In: {}.", names.join(", "))
        } else {
            format!(
                "In {} audience{}.",
                now_in.len(),
                if now_in.len() == 1 { "" } else { "s" }
            )
        });
        Ok(ToolResult::with_structured(
            said.join(" "),
            json!({
                "workspace_id": workspace_id,
                "contact_id": id,
                "email": address,
                "first_name": contact.get("firstName"),
                "last_name": contact.get("lastName"),
                "tags": contact.get("tags"),
                "audience_ids": contact.get("audienceIds"),
            }),
        ))
    }

    // ------------------------------------------------------------------------------------
    // email_contacts: import_preview, import
    // ------------------------------------------------------------------------------------

    /// The file's text: `csv` as passed, or `csv_path` read from this machine. A path is a
    /// caller's word for a file on the machine the tool runs on, so only a local binary
    /// reads one, and only a file named as a CSV.
    async fn csv_text(&self, input: &StudioInput) -> Result<String> {
        let too_large =
            || Error::Validation("The file is over 5 MB: split it and import each part.".into());
        match (input.csv.as_deref(), given(input.csv_path.as_deref())) {
            (Some(_), Some(_)) => Err(Error::Validation(
                "Pass csv or csv_path, not both.".to_string(),
            )),
            (Some(csv), None) if csv.trim().is_empty() => Err(Error::Validation(
                "csv is empty: pass the file's text, header row first.".to_string(),
            )),
            (Some(csv), None) if csv.len() > MAX_CSV_BYTES => Err(too_large()),
            (Some(csv), None) => Ok(csv.to_string()),
            (None, Some(path)) => {
                let config = self.client.config().await;
                if config.is_http_transport || mcp_client::client::local_files_denied() {
                    return Err(Error::Validation(
                        "Local files can't be read here: pass the file's text as csv.".to_string(),
                    ));
                }
                let path = std::path::Path::new(path);
                let named_csv = path
                    .extension()
                    .and_then(|e| e.to_str())
                    .map(str::to_ascii_lowercase)
                    .is_some_and(|e| CSV_EXTENSIONS.contains(&e.as_str()));
                if !named_csv {
                    return Err(Error::Validation(format!(
                        "{} isn't a .csv, .tsv or .txt file.",
                        path.display()
                    )));
                }
                let unreadable =
                    |why: String| Error::Validation(format!("{} can't be read: {why}", path.display()));
                let meta = tokio::fs::metadata(path)
                    .await
                    .map_err(|e| unreadable(e.to_string()))?;
                if !meta.is_file() {
                    return Err(unreadable("it isn't a file".to_string()));
                }
                if meta.len() > MAX_CSV_BYTES as u64 {
                    return Err(too_large());
                }
                let bytes = tokio::fs::read(path)
                    .await
                    .map_err(|e| unreadable(e.to_string()))?;
                String::from_utf8(bytes).map_err(|_| unreadable("it isn't text (UTF-8)".to_string()))
            }
            (None, None) => Err(Error::Validation(
                "csv (the file's text) or csv_path is required: the contacts to import, header row first"
                    .to_string(),
            )),
        }
    }

    /// Reads what an import would do (`POST /contacts/import/preview`, which changes
    /// nothing), or why it can't be read.
    async fn read_import(
        &self,
        workspace_id: Uuid,
        input: &StudioInput,
        needs_audience: bool,
    ) -> Result<std::result::Result<Import, String>> {
        let (source, note) = consent_of(input, "import contacts")?;
        let audience_id = given(input.audience_id.as_deref())
            .map(|id| parse_id(id, "audience_id"))
            .transpose()?;
        if needs_audience && audience_id.is_none() {
            return Err(Error::Validation(
                "audience_id is required for email_contacts with operation import: the audience they go into (email_audiences lists them, and creates one)"
                    .to_string(),
            ));
        }
        let tags = string_list(input.tags.as_ref(), "tags")?.filter(|tags| !tags.is_empty());
        let csv = self.csv_text(input).await?;
        if super::looks_like_resend_key(&csv) {
            return Err(Error::Validation(super::KEY_REFUSED.to_string()));
        }
        let mut body = json!({"workspaceId": workspace_id, "csv": csv});
        if let Some(audience_id) = audience_id {
            body["audienceId"] = json!(audience_id);
        }
        let answer = self
            .email_call("POST", "/contacts/import/preview", Some(&body), None)
            .await?;
        if !answer.is_success() {
            return Ok(Err(email_refusal(&answer, Topic::Audience)));
        }
        let preview = answer.data;
        if let Some(problem) = text(&preview, "problem") {
            let columns: Vec<String> = preview
                .get("columns")
                .and_then(Value::as_array)
                .map(|columns| {
                    columns
                        .iter()
                        .filter_map(|column| text(column, "header"))
                        .map(|header| format!("“{header}”"))
                        .collect()
                })
                .unwrap_or_default();
            let said = if problem.contains("which column holds the email") {
                format!(
                    "Create couldn't tell which column holds the addresses (its columns: {}). Name that column email in the file's first row and try again, or import it in the studio, which lets the person pick the columns: {AUDIENCES_URL}",
                    columns.join(", ")
                )
            } else {
                format!("{problem} The studio's import lets the person pick the columns: {AUDIENCES_URL}")
            };
            return Ok(Err(said));
        }
        let audience = match audience_id {
            Some(id) => self.audience(workspace_id, &id.to_string()).await,
            None => None,
        };
        Ok(Ok(Import {
            csv,
            audience_id,
            audience,
            source,
            note,
            tags,
            counts: Counts::of(&preview),
            preview,
        }))
    }

    pub(super) async fn import_preview(
        &self,
        workspace_id: Uuid,
        input: &StudioInput,
    ) -> Result<ToolResult> {
        let import = match self.read_import(workspace_id, input, false).await? {
            Ok(import) => import,
            Err(refused) => return Ok(ToolResult::error(format!("Not imported: {refused}"))),
        };
        let counts = &import.counts;
        let preview = &import.preview;
        let mut lines = vec![match import.audience_id {
            Some(_) => format!(
                "Preview of an import into {}: nothing is changed.",
                audience_words(import.audience.as_ref())
            ),
            None => "Preview of an import: nothing is changed.".to_string(),
        }];
        lines.push(format!(
            "{} rows: {} valid, {} invalid, {} repeating an address already in the file.",
            said(counts.rows),
            said(counts.valid),
            said(counts.invalid),
            said(counts.duplicates)
        ));
        lines.push(format!(
            "Of the {}: {} new, {} already contacts, {} on the suppression list (kept, never sent to).",
            said(counts.valid),
            said(counts.new),
            said(counts.existing),
            said(counts.suppressed)
        ));
        lines.push(match counts.eligible {
            Some(eligible) => format!("Eligible: {} can be sent to.", credits(eligible)),
            None => "Eligible: not answered.".to_string(),
        });
        if counts.suppressed.is_some_and(|n| n > 0) {
            lines.push("Suppressed addresses: not answered by this preview.".into());
        }
        let invalid = preview
            .get("invalid")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if !invalid.is_empty() {
            let shown: Vec<String> = invalid
                .iter()
                .take(SHOWN_ROWS)
                .map(|row| {
                    let at = number(row, "row")
                        .map_or_else(|| "a row".to_string(), |n| format!("row {n}"));
                    let why = text(row, "reason").unwrap_or_else(|| "couldn't be read".to_string());
                    match text(row, "value") {
                        Some(value) => format!("{at} “{value}” ({why})"),
                        None => format!("{at} ({why})"),
                    }
                })
                .collect();
            let more = counts.invalid.unwrap_or(invalid.len() as i64) - shown.len() as i64;
            lines.push(format!(
                "Invalid: {}{}.",
                shown.join("; "),
                if more > 0 {
                    format!("; and {} more", credits(more))
                } else {
                    String::new()
                }
            ));
        }
        let read_as = mapping_words(preview);
        if !read_as.is_empty() {
            lines.push(format!("Read as: {}.", read_as.join(", ")));
        }
        let sample: Vec<String> = preview
            .get("sample")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .take(SHOWN_ROWS)
                    .filter_map(|row| text(row, "email"))
                    .collect()
            })
            .unwrap_or_default();
        if !sample.is_empty() {
            lines.push(format!("Sample: {}.", sample.join(", ")));
        }
        lines.push(format!("Consent: {}, “{}”.", import.source, import.note));
        lines.push(match (import.audience_id, counts.eligible) {
            (Some(audience_id), Some(eligible)) => format!(
                "To import it: show the person this preview and, once they agree, call email_contacts with operation import, the same csv (or csv_path), audience_id {audience_id}, consent_source {}, the same consent_note and confirm_rows {eligible}.",
                import.source
            ),
            (None, _) => "To import it, an audience is needed: email_audiences lists them (operation create makes one), then preview again with audience_id.".to_string(),
            (Some(_), None) => "Create did not answer how many could be sent to, so there is no count for the person to agree to: import it in the studio.".to_string(),
        });
        Ok(ToolResult::with_structured(
            lines.join("\n"),
            json!({
                "workspace_id": workspace_id,
                "imported": false,
                "audience_id": import.audience_id,
                "audience": import.audience.as_ref().map(|a| json!({"name": a.get("name"), "purpose": a.get("purpose")})),
                "rows": counts.rows,
                "valid": counts.valid,
                "invalid_count": counts.invalid,
                "invalid": invalid,
                "duplicates_in_file": counts.duplicates,
                "existing": counts.existing,
                "new": counts.new,
                "suppressed": counts.suppressed,
                "eligible": counts.eligible,
                "sample": sample,
                "read_as": read_as,
                "consent": {"source": import.source, "note": import.note},
                "confirm_with": import.confirm_with(),
            }),
        ))
    }

    /// Imports the file, only with the person's go for the eligible count they were shown.
    pub(super) async fn import_contacts(
        &self,
        workspace_id: Uuid,
        input: &StudioInput,
    ) -> Result<ToolResult> {
        let import = match self.read_import(workspace_id, input, true).await? {
            Ok(import) => import,
            Err(refused) => return Ok(ToolResult::error(format!("Not imported: {refused}"))),
        };
        let audience_id = import
            .audience_id
            .expect("read_import refuses an import without its audience");
        let audience_name = import
            .audience
            .as_ref()
            .and_then(|a| text(a, "name"))
            .unwrap_or_else(|| "the audience".to_string());
        if import.counts.valid == Some(0) {
            return Ok(ToolResult::error(
                "Not imported: no row in the file can be imported. email_contacts with operation import_preview says why for each."
                    .to_string(),
            ));
        }
        let Some(eligible) = import.counts.eligible else {
            return Ok(ToolResult::error(
                "Not imported: Create did not answer how many of these people could be sent to, so there is no count for the person to agree to. Import the file in the studio instead."
                    .to_string(),
            ));
        };
        let token = import.review_token();
        if input
            .confirm_review
            .as_ref()
            .is_some_and(|seen| seen != &token)
        {
            return Ok(ToolResult::error("Not imported: the file, audience, consent or eligible recipients changed. Run the preview again and confirm it."));
        }
        let what = import.what();
        let mut notes: Vec<String> = Vec::new();
        let go = persons_go(
            Question {
                key: "confirm-import",
                message: format!(
                    "Import {what}? Each is added, or updated by its address; an import isn't undone in one step."
                ),
                title: "Import them",
                does: format!("Imports them into “{audience_name}”."),
                state: format!("studio-email-import:{audience_id}:{token}:{eligible}"),
            },
            input.confirm_rows == Some(eligible),
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
                    format!("Asking the person before {what} are imported."),
                    json!({"imported": false, "audience_id": audience_id, "asking": true}),
                ))
            }
            Go::No => {
                return Ok(ToolResult::with_structured(
                    format!(
                        "Not imported: the person didn't say go to importing {what}. Nothing was changed."
                    ),
                    json!({"imported": false, "audience_id": audience_id, "declined": true}),
                ))
            }
            Go::NotAsked => {
                return Ok(ToolResult::with_structured(
                    format!(
                        "Not imported: {}. Nothing was changed.",
                        crate::go_ahead::NOT_ASKED
                    ),
                    json!({"imported": false, "audience_id": audience_id, "go_ahead": "asked_only"}),
                ))
            }
            Go::Needed { unanswered } => {
                let mut said = match input.confirm_rows {
                    Some(confirmed) => format!(
                        "Not imported: the file now has {} who can be sent to, not the {} the person agreed to.",
                        super::people(eligible),
                        credits(confirmed)
                    ),
                    None if unanswered => "Not imported yet: the person was asked through the client and gave no answer in time.".to_string(),
                    None => "Not imported yet: the person's go is needed first.".to_string(),
                };
                said.push_str(&format!(
                    " Show them: {what}. If they agree, call email_contacts again with operation import, the same file, audience_id {audience_id}, consent_source {}, the same consent_note and confirm_rows {eligible}.",
                    import.source
                ));
                return Ok(ToolResult::with_structured(
                    said,
                    json!({
                        "imported": false,
                        "audience_id": audience_id,
                        "eligible": eligible,
                        "valid": import.counts.valid,
                        "suppressed": import.counts.suppressed,
                        "confirm_with": import.confirm_with(),
                    }),
                ));
            }
        }

        let legacy_note = format!("{} (consent source: {})", import.note, import.source);
        let mut body = json!({
            "workspaceId": workspace_id,
            "audienceId": audience_id,
            "csv": import.csv,
            "mapping": import.preview.get("mapping"),
            "consentNote": legacy_note,
        });
        if let Some(tags) = &import.tags {
            body["tags"] = json!(tags);
        }
        // A Create that answers the count and a digest of who it would add also takes them
        // back, and refuses an import that is no longer what the person saw.
        let mut newer: Vec<&str> = Vec::new();
        if number(&import.preview, "eligible").is_some() {
            body["expectedEligible"] = json!(eligible);
            newer.push("expectedEligible");
        }
        if let Some(seen) = text(&import.preview, "digest") {
            body["expectedDigest"] = json!(seen);
            newer.push("expectedDigest");
        }
        if !newer.is_empty() {
            body["consentSource"] = json!(import.source);
            body["consentNote"] = json!(import.note);
        }
        // Different writes carry different keys, including their tags and preview digest.
        let key = format!("email-import-{}", digest(&body.to_string()));
        let (mut answer, mut kept) = self
            .email_call_newer("POST", "/contacts/import", &body, &newer, Some(&key))
            .await?;
        if unknown_field(&answer, "consentSource") {
            body.as_object_mut()
                .expect("import is an object")
                .remove("consentSource");
            body["consentNote"] = json!(legacy_note);
            let key = format!("email-import-{}", digest(&body.to_string()));
            (answer, kept) = self
                .email_call_newer("POST", "/contacts/import", &body, &newer, Some(&key))
                .await?;
        }
        if !kept || newer.is_empty() {
            notes.push("Eligible recipients at import time: not answered by this API.".into());
        }
        if !answer.is_success() {
            return Ok(ToolResult::error(format!(
                "Not imported: {}",
                email_refusal(&answer, Topic::Audience)
            )));
        }
        let done = answer.data;
        let created = number(&done, "created");
        let updated = number(&done, "updated");
        let added = number(&done, "added");
        let suppressed = number(&done, "suppressed");
        let invalid = number(&done, "invalidCount");
        let duplicates = number(&done, "duplicatesInFile");
        let mut lines = vec![format!(
            "Imported into “{audience_name}”: {} new contacts and {} updated, {} put in the audience.",
            said(created),
            said(updated),
            said(added)
        )];
        if let Some(suppressed) = suppressed.filter(|n| *n > 0) {
            lines.push(format!(
                "{} on the suppression list: kept, and never sent to.",
                is_are(suppressed)
            ));
        }
        match (invalid.unwrap_or(0), duplicates.unwrap_or(0)) {
            (0, 0) => {}
            (invalid, 0) => lines.push(format!(
                "{} row{} weren't read (invalid).",
                credits(invalid),
                if invalid == 1 { "" } else { "s" }
            )),
            (0, duplicates) => lines.push(format!(
                "{} repeated an address already in the file.",
                credits(duplicates)
            )),
            (invalid, duplicates) => lines.push(format!(
                "{} row{} weren't read (invalid), and {} repeated an address already in the file.",
                credits(invalid),
                if invalid == 1 { "" } else { "s" },
                credits(duplicates)
            )),
        }
        lines.extend(notes);
        Ok(ToolResult::with_structured(
            lines.join("\n"),
            json!({
                "imported": true,
                "workspace_id": workspace_id,
                "audience_id": audience_id,
                "created": created,
                "updated": updated,
                "added_to_audience": added,
                "suppressed": suppressed,
                "invalid_count": invalid,
                "duplicates_in_file": duplicates,
                "eligible": done.get("eligible"),
            }),
        ))
    }
}
