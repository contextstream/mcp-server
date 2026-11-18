//! The rest of Create's email, as the `studio` tool reaches it (spec "Email, complete in
//! every agent surface", 2026-11-18): audiences with a purpose, a contact's fields and
//! membership, CSV imports, segments, who a send reaches, the unsubscribe report and the
//! webhook's verdict, templates, A/B subjects, pause and resume, and the connection.
//!
//! Each test names what the action sends (method, path, body, idempotency key) and what it
//! says. The stub answers as contextstream main does today unless a test says "newer": the
//! fields the API lane adds are optional, so both are pinned.

use super::*;

const PROOF: &str = "12121212-1212-4212-8212-121212121212";
const NEW_AUDIENCE: &str = "13131313-1313-4313-8313-131313131313";

/// `extra` first, then the usual routes.
fn over(extra: impl Fn(&Recorded) -> Option<Answer> + Send + Sync + 'static) -> Responder {
    let usual = usual();
    Arc::new(move |request: &Recorded| extra(request).unwrap_or_else(|| usual(request)))
}

/// What the API says of a field it does not know (`common::body`).
fn unknown_field(field: &str, known: &str) -> Answer {
    Answer::error(
        400,
        "BAD_REQUEST",
        &format!("Bad request: unknown field `{field}`, expected one of {known}"),
    )
}

fn audience(id: &str, name: &str, purpose: Option<&str>, contacts: i64, subscribed: i64) -> Value {
    let mut audience = json!({
        "id": id, "name": name, "description": null, "contacts": contacts, "subscribed": subscribed,
        "createdAt": "2026-10-01T10:00:00Z", "updatedAt": "2026-10-01T10:00:00Z"
    });
    if let Some(purpose) = purpose {
        audience["purpose"] = json!(purpose);
    }
    audience
}

async fn run(stub: &Stub, input: Value) -> ToolResult {
    stub.tool().execute(input).await.unwrap()
}

// ============================================================================
// email_audiences: purpose, create, update
// ============================================================================

#[tokio::test]
async fn audiences_are_listed_with_their_purpose_and_who_is_suppressed() {
    let stub = Stub::start(over(|request| {
        (request.method == "GET" && request.path == "/create/email/audiences").then(|| {
            Answer::ok(json!([
                audience(AUDIENCE, "Coffee club", Some("production"), 40, 37),
                audience(PROOF, "Proof", Some("test"), 3, 3),
            ]))
        })
    }))
    .await;
    let result = run(&stub, json!({"action": "email_audiences"})).await;
    let text = text_of(&result);
    assert!(!result.is_error, "{text}");
    assert!(text.contains("2 audiences as of now:"), "{text}");
    assert!(
        text.contains(&format!(
            "- Coffee club (production): 40 contacts, 37 subscribed, 3 suppressed (audience_id {AUDIENCE})"
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "- Proof (test): 3 contacts, 3 subscribed, 0 suppressed (audience_id {PROOF})"
        )),
        "{text}"
    );
    let out = structured(&result);
    assert_eq!(out["audiences"][0]["purpose"], "production");
    assert_eq!(out["audiences"][0]["suppressed"], 3);
    assert_eq!(out["audiences"][1]["purpose"], "test");
    // Only reads.
    assert!(stub.requests().iter().all(|r| r.method == "GET"));
}

#[tokio::test]
async fn a_purpose_the_api_does_not_answer_is_said_as_not_answered() {
    // Today's main: an audience has no purpose yet. Never "production" by default.
    let stub = Stub::start(usual()).await;
    let result = run(&stub, json!({"action": "email_audiences"})).await;
    let text = text_of(&result);
    assert!(
        text.contains(&format!(
            "- Coffee club (purpose not answered): 40 contacts, 37 subscribed, 3 suppressed (audience_id {AUDIENCE})"
        )),
        "{text}"
    );
    assert_eq!(structured(&result)["audiences"][0]["purpose"], Value::Null);
}

#[tokio::test]
async fn an_audience_is_created_with_its_purpose() {
    let stub = Stub::start(over(|request| {
        (request.method == "POST" && request.path == "/create/email/audiences").then(|| {
            let mut made = audience(
                NEW_AUDIENCE,
                request.body["name"].as_str().unwrap_or(""),
                request.body["purpose"].as_str(),
                0,
                0,
            );
            made["description"] = request.body["description"].clone();
            Answer::status(201, made)
        })
    }))
    .await;
    let result = run(
        &stub,
        json!({
            "action": "email_audiences", "operation": "create",
            "name": " Customers ", "purpose": "Production", "description": "People who bought",
        }),
    )
    .await;
    let text = text_of(&result);
    assert!(!result.is_error, "{text}");
    let posts = stub.calls_to("POST", "/create/email/audiences");
    assert_eq!(posts.len(), 1);
    assert_eq!(
        posts[0].body,
        json!({"workspaceId": WORKSPACE, "name": "Customers", "purpose": "production", "description": "People who bought"})
    );
    assert!(
        text.starts_with(&format!(
            "Created the production audience “Customers” (audience_id {NEW_AUDIENCE}). It holds no one yet"
        )),
        "{text}"
    );
    let out = structured(&result);
    assert_eq!(out["audience_id"], NEW_AUDIENCE);
    assert_eq!(out["purpose"], "production");

    // The tool never guesses a purpose, and takes only the two.
    for (input, said) in [
        (
            json!({"action": "email_audiences", "operation": "create", "name": "Customers"}),
            "purpose is required",
        ),
        (
            json!({"action": "email_audiences", "operation": "create", "name": "Customers", "purpose": "real"}),
            "purpose is test or production",
        ),
        (
            json!({"action": "email_audiences", "operation": "create", "purpose": "test"}),
            "name is required",
        ),
    ] {
        let refused = stub.tool().execute(input).await.unwrap_err();
        assert!(refused.to_string().contains(said), "{refused}");
    }
    assert_eq!(stub.calls_to("POST", "/create/email/audiences").len(), 1);
}

#[tokio::test]
async fn where_the_api_takes_no_purpose_yet_the_audience_is_made_and_the_answer_says_so() {
    // Today's main refuses a field it does not know. The audience is what was asked for; its
    // purpose is said as not kept, never as kept.
    let stub = Stub::start(over(|request| {
        (request.method == "POST" && request.path == "/create/email/audiences").then(|| {
            if request.body.get("purpose").is_some() {
                unknown_field("purpose", "`workspaceId`, `name`, `description`")
            } else {
                Answer::status(201, audience(NEW_AUDIENCE, "Customers", None, 0, 0))
            }
        })
    }))
    .await;
    let result = run(
        &stub,
        json!({"action": "email_audiences", "operation": "create", "name": "Customers", "purpose": "production"}),
    )
    .await;
    let text = text_of(&result);
    assert!(!result.is_error, "{text}");
    let posts = stub.calls_to("POST", "/create/email/audiences");
    assert_eq!(posts.len(), 2);
    assert_eq!(
        posts[1].body,
        json!({"workspaceId": WORKSPACE, "name": "Customers"})
    );
    assert!(
        text.starts_with(&format!(
            "Created the audience “Customers” (audience_id {NEW_AUDIENCE})."
        )),
        "{text}"
    );
    assert!(
        text.contains("Its purpose (production) was not kept: Create doesn't take one here yet."),
        "{text}"
    );
    assert_eq!(structured(&result)["purpose"], Value::Null);
    assert_eq!(structured(&result)["purpose_kept"], false);
}

#[tokio::test]
async fn an_audience_is_renamed_or_given_another_purpose() {
    let stub = Stub::start(over(|request| {
        (request.method == "PATCH" && request.path == format!("/create/email/audiences/{PROOF}"))
            .then(|| {
                Answer::ok(audience(
                    PROOF,
                    request.body["name"].as_str().unwrap_or("Proof"),
                    request.body["purpose"].as_str().or(Some("test")),
                    3,
                    3,
                ))
            })
    }))
    .await;
    let path = format!("/create/email/audiences/{PROOF}");
    let result = run(
        &stub,
        json!({"action": "email_audiences", "operation": "update", "audience_id": PROOF, "name": "Proof readers"}),
    )
    .await;
    let text = text_of(&result);
    assert!(!result.is_error, "{text}");
    let patches = stub.calls_to("PATCH", &path);
    assert_eq!(
        patches[0].body,
        json!({"workspaceId": WORKSPACE, "name": "Proof readers"})
    );
    assert!(
        text.starts_with(&format!(
            "Changed the audience: “Proof readers” (test; audience_id {PROOF}), 3 contacts, 3 subscribed."
        )),
        "{text}"
    );

    let result = run(
        &stub,
        json!({"action": "email_audiences", "operation": "update", "audience_id": PROOF, "purpose": "production", "description": ""}),
    )
    .await;
    assert!(!result.is_error, "{}", text_of(&result));
    assert_eq!(
        stub.calls_to("PATCH", &path)[1].body,
        json!({"workspaceId": WORKSPACE, "purpose": "production", "description": null})
    );
    assert!(
        text_of(&result).contains("“Proof” (production;"),
        "{}",
        text_of(&result)
    );

    // Nothing to change, or no audience named: refused before anything is sent.
    for input in [
        json!({"action": "email_audiences", "operation": "update", "audience_id": PROOF}),
        json!({"action": "email_audiences", "operation": "update", "name": "X"}),
    ] {
        assert!(matches!(
            stub.tool().execute(input).await,
            Err(Error::Validation(_))
        ));
    }
    assert_eq!(stub.calls_to("PATCH", &path).len(), 2);

    // An audience that isn't there is said in one sentence.
    let missing = run(
        &stub,
        json!({"action": "email_audiences", "operation": "update", "audience_id": NEW_AUDIENCE, "name": "X"}),
    )
    .await;
    assert!(missing.is_error);
    assert_eq!(
        text_of(&missing),
        "Not changed: There is no such audience or segment in this workspace (email_audiences lists them)."
    );
}

#[test]
fn the_audience_arguments_are_in_the_schema_and_the_reference() {
    let schema = input_schema();
    let properties = schema["properties"].as_object().unwrap();
    for field in ["name", "purpose", "description"] {
        assert!(properties.contains_key(field), "{field}");
    }
    assert_eq!(properties["purpose"]["enum"], json!(["test", "production"]));
    let operations = properties["operation"]["enum"].as_array().unwrap();
    for operation in ["list", "create", "update"] {
        assert!(operations.contains(&json!(operation)), "{operation}");
    }
    assert!(
        TOOL_REFERENCE.contains("- email_audiences: operation list (default"),
        "the reference names the operations"
    );
    assert!(TOOL_REFERENCE.contains("purpose test|production"));
}

// ============================================================================
// email_contacts: update, import_preview, import
// ============================================================================

const CSV: &str =
    "Email,First,Plan\nada@northwind.example,Ada,pro\nbob@northwind.example,Bob,free\n";
const NOTE: &str = "Signed up at the Spring market";

/// A contact as `PATCH /contacts/:id` answers it: what was sent, over what it was.
fn patched_contact(body: &Value) -> Value {
    json!({
        "id": CONTACT, "email": "ada@northwind.example",
        "firstName": body.get("firstName").cloned().unwrap_or(json!("Ada")),
        "lastName": body.get("lastName").cloned().unwrap_or(json!("Lovelace")),
        "tags": body.get("tags").cloned().unwrap_or(json!(["vip"])),
        "attributes": {},
        "consent": {"source": "manual", "note": "Met at the Spring market", "at": "2026-10-01T10:00:00Z"},
        "audienceIds": body.get("audienceIds").cloned().unwrap_or(json!([AUDIENCE])),
        "suppressed": null,
    })
}

fn contacts_api() -> Responder {
    over(
        |request| match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/create/email/audiences") => Some(Answer::ok(json!([
                audience(AUDIENCE, "Coffee club", Some("production"), 40, 37),
                audience(PROOF, "Proof", Some("test"), 3, 3),
            ]))),
            ("PATCH", path) if path == format!("/create/email/contacts/{CONTACT}") => {
                if request.body.get("addAudienceIds").is_some()
                    || request.body.get("removeAudienceIds").is_some()
                {
                    return Some(unknown_field(
                        "addAudienceIds",
                        "`workspaceId`, `audienceIds`",
                    ));
                }
                Some(Answer::ok(patched_contact(&request.body)))
            }
            ("PATCH", path) if path.starts_with("/create/email/contacts/") => Some(Answer::error(
                404,
                "NOT_FOUND",
                "Not found: No such contact.",
            )),
            _ => None,
        },
    )
}

#[tokio::test]
async fn a_contacts_fields_and_membership_are_changed() {
    let stub = Stub::start(contacts_api()).await;
    let path = format!("/create/email/contacts/{CONTACT}");

    // By address: its audiences are read first, since Create takes the whole set.
    let result = run(
        &stub,
        json!({
            "action": "email_contacts", "operation": "update", "email": "Ada@Northwind.example",
            "first_name": "Ada B", "last_name": "", "tags": ["vip", " wholesale "],
            "add_to": [PROOF], "remove_from": [AUDIENCE],
        }),
    )
    .await;
    let text = text_of(&result);
    assert!(!result.is_error, "{text}");
    let patches = stub.calls_to("PATCH", &path);
    assert_eq!(patches.len(), 2);
    assert_eq!(
        patches[1].body,
        json!({
            "workspaceId": WORKSPACE, "firstName": "Ada B", "lastName": null,
            "tags": ["vip", "wholesale"], "audienceIds": [PROOF],
        })
    );
    assert_eq!(
        text,
        "Changed ada@northwind.example (Ada B). Tags: vip, wholesale. In: Proof."
    );
    assert_eq!(structured(&result)["audience_ids"], json!([PROOF]));

    // By id, its fields alone: nothing is read first.
    let before = stub.calls_to("GET", "/create/email/contacts").len();
    let result = run(
        &stub,
        json!({"action": "email_contacts", "operation": "update", "contact_id": CONTACT, "tags": []}),
    )
    .await;
    assert!(!result.is_error, "{}", text_of(&result));
    assert_eq!(
        stub.calls_to("PATCH", &path)[2].body,
        json!({"workspaceId": WORKSPACE, "tags": []})
    );
    assert_eq!(stub.calls_to("GET", "/create/email/contacts").len(), before);
    assert!(
        text_of(&result).starts_with("Changed ada@northwind.example (Ada Lovelace). No tags."),
        "{}",
        text_of(&result)
    );

    // An older API cannot apply a delta by id. There must be no partial write.
    let by_id = run(&stub, json!({"action":"email_contacts","operation":"update","contact_id":CONTACT,"add_to":[PROOF]})).await;
    assert!(by_id.is_error && text_of(&by_id).contains("email"));
    for (input, said) in [
        (
            json!({"action": "email_contacts", "operation": "update", "email": "ada@northwind.example"}),
            "needs something to change",
        ),
        (
            json!({"action": "email_contacts", "operation": "update", "first_name": "Ada"}),
            "email or contact_id is required",
        ),
        (
            json!({"action": "email_contacts", "operation": "update", "email": "ada@northwind.example", "add_to": ["nope"]}),
            "add_to",
        ),
    ] {
        let refused = stub.tool().execute(input).await.unwrap_err();
        assert!(refused.to_string().contains(said), "{refused}");
    }
    assert_eq!(stub.calls_to("PATCH", &path).len(), 4);

    let nobody = run(
        &stub,
        json!({"action": "email_contacts", "operation": "update", "email": "nobody@northwind.example", "tags": ["x"]}),
    )
    .await;
    assert_eq!(
        text_of(&nobody),
        "Not changed: there is no contact nobody@northwind.example in this workspace."
    );
    let gone = run(
        &stub,
        json!({"action": "email_contacts", "operation": "update", "contact_id": PROOF, "tags": ["x"]}),
    )
    .await;
    assert_eq!(
        text_of(&gone),
        "Not changed: There is no such contact in this workspace (email_contacts lists them)."
    );
}

#[tokio::test]
async fn newer_contacts_apply_membership_deltas_by_id_in_one_write() {
    let stub = Stub::start(over(|r| {
        (r.method == "PATCH" && r.path == format!("/create/email/contacts/{CONTACT}")).then(|| {
            let mut contact = patched_contact(&r.body);
            contact["audienceIds"] = json!([PROOF]);
            Answer::ok(contact)
        })
    }))
    .await;
    let updated = run(
        &stub,
        json!({"action":"email_contacts", "operation":"update", "contact_id":CONTACT,
        "add_to":[PROOF], "remove_from":[AUDIENCE], "first_name":"Ada B"}),
    )
    .await;
    assert!(!updated.is_error);
    let patches = stub.calls_to("PATCH", &format!("/create/email/contacts/{CONTACT}"));
    assert_eq!(patches.len(), 1);
    assert_eq!(
        patches[0].body,
        json!({"workspaceId":WORKSPACE,"addAudienceIds":[PROOF],"removeAudienceIds":[AUDIENCE],"firstName":"Ada B"})
    );
    assert!(stub.calls_to("GET", "/create/email/contacts").is_empty());
}

/// What `POST /contacts/import/preview` answers on today's main.
fn import_preview_answer() -> Value {
    json!({
        "columns": [
            {"index": 0, "header": "Email", "guess": "email", "samples": ["ada@northwind.example"]},
            {"index": 1, "header": "First", "guess": "first_name", "samples": ["Ada"]},
            {"index": 2, "header": "Plan", "guess": "attribute", "samples": ["pro"]},
        ],
        "mapping": {"email": 0, "firstName": 1, "lastName": null, "name": null, "tags": null, "attributes": {"plan": 2}},
        "rows": 120, "valid": 117, "invalidCount": 2,
        "invalid": [
            {"row": 4, "value": "bob@", "reason": "isn't an email address"},
            {"row": 9, "value": "", "reason": "has no email address"},
        ],
        "duplicatesInFile": 1, "existing": 7, "new": 110, "suppressed": 3,
        "sample": [
            {"email": "ada@northwind.example", "firstName": "Ada", "lastName": null, "tags": [], "attributes": {"plan": "pro"}},
            {"email": "bob@northwind.example", "firstName": "Bob", "lastName": null, "tags": [], "attributes": {"plan": "free"}},
        ],
    })
}

/// The import routes: `newer` answers and takes what the API lane adds.
fn import_api(newer: bool) -> Responder {
    let base = contacts_api();
    Arc::new(
        move |request: &Recorded| match (request.method.as_str(), request.path.as_str()) {
            ("POST", "/create/email/contacts/import/preview") => {
                let mut answer = import_preview_answer();
                if newer {
                    answer["eligible"] = json!(114);
                    answer["digest"] = json!("5f2c9a");
                }
                Answer::ok(answer)
            }
            ("POST", "/create/email/contacts/import") => {
                let expects = request.body.get("expectedEligible").is_some()
                    || request.body.get("expectedDigest").is_some();
                if expects && !newer {
                    return unknown_field(
                        "expectedEligible",
                        "`workspaceId`, `audienceId`, `csv`, `mapping`, `consentNote`, `tags`",
                    );
                }
                if request.body.get("expectedEligible") == Some(&json!(999)) {
                    return Answer::error(422, "email_import_changed", "This import would now add 114 people who can be sent to, not 999. Nothing was imported; preview it again.");
                }
                Answer::ok(json!({
                    "created": 110, "updated": 7, "added": 117, "invalidCount": 2,
                    "duplicatesInFile": 1, "suppressed": 3,
                }))
            }
            _ => base(request),
        },
    )
}

fn import_input(operation: &str) -> Value {
    json!({
        "action": "email_contacts", "operation": operation, "csv": CSV, "audience_id": AUDIENCE,
        "consent_source": "form", "consent_note": NOTE,
    })
}

#[tokio::test]
async fn an_import_is_previewed_and_changes_nothing() {
    let stub = Stub::start(import_api(false)).await;
    let result = run(&stub, import_input("import_preview")).await;
    let text = text_of(&result);
    assert!(!result.is_error, "{text}");
    let previews = stub.calls_to("POST", "/create/email/contacts/import/preview");
    assert_eq!(previews.len(), 1);
    assert_eq!(
        previews[0].body,
        json!({"workspaceId": WORKSPACE, "csv": CSV, "audienceId": AUDIENCE})
    );
    assert!(
        text.starts_with(
            "Preview of an import into the production audience “Coffee club”: nothing is changed."
        ),
        "{text}"
    );
    assert!(
        text.contains(
            "120 rows: 117 valid, 2 invalid, 1 repeating an address already in the file."
        ),
        "{text}"
    );
    assert!(
        text.contains("Of the 117: 110 new, 7 already contacts, 3 on the suppression list (kept, never sent to)."),
        "{text}"
    );
    assert!(text.contains("Eligible: 114 can be sent to."), "{text}");
    assert!(
        text.contains(
            "Invalid: row 4 “bob@” (isn't an email address); row 9 (has no email address)."
        ),
        "{text}"
    );
    assert!(
        text.contains(
            "Read as: address from “Email”, first name from “First”, attribute plan from “Plan”."
        ),
        "{text}"
    );
    assert!(
        text.contains("Sample: “ada@northwind.example”, “bob@northwind.example”."),
        "{text}"
    );
    assert!(
        text.contains("Consent: form, “Signed up at the Spring market”."),
        "{text}"
    );
    assert!(text.contains("confirm_rows 114"), "{text}");
    let out = structured(&result);
    assert_eq!(out["eligible"], 114);
    assert_eq!(out["confirm_with"]["confirm_rows"], 114);
    assert_eq!(out["confirm_with"]["operation"], "import");
    assert!(
        out.to_string()
            .find("ada@northwind.example,Ada,pro")
            .is_none(),
        "the file isn't echoed"
    );
    assert!(stub
        .calls_to("POST", "/create/email/contacts/import")
        .is_empty());

    // Where they agreed is asked before anything is read.
    for (strip, said) in [
        ("consent_source", "consent_source is required"),
        ("consent_note", "consent_note is required"),
        ("csv", "csv (the file's text) or csv_path is required"),
    ] {
        let mut input = import_input("import_preview");
        input.as_object_mut().unwrap().remove(strip);
        let refused = stub.tool().execute(input).await.unwrap_err();
        assert!(refused.to_string().contains(said), "{refused}");
    }
    let mut both = import_input("import_preview");
    both["csv_path"] = json!("/tmp/x.csv");
    let refused = stub.tool().execute(both).await.unwrap_err();
    assert!(
        refused.to_string().contains("csv or csv_path, not both"),
        "{refused}"
    );
    assert_eq!(
        stub.calls_to("POST", "/create/email/contacts/import/preview")
            .len(),
        1
    );
}

#[tokio::test]
async fn a_file_create_cant_read_as_contacts_is_said_plainly() {
    let stub = Stub::start(over(|request| {
        (request.path == "/create/email/contacts/import/preview").then(|| {
            Answer::ok(json!({
                "columns": [{"index": 0, "header": "Who", "guess": "attribute", "samples": ["Ada"]}],
                "mapping": {"email": null, "firstName": null, "lastName": null, "name": null, "tags": null, "attributes": {"who": 0}},
                "rows": 2,
                "problem": "Say which column holds the email addresses.",
            }))
        })
    }))
    .await;
    for operation in ["import_preview", "import"] {
        let mut input = import_input(operation);
        input["confirm_rows"] = json!(2);
        let result = run(&stub, input).await;
        assert!(result.is_error);
        let text = text_of(&result);
        assert!(
            text.starts_with("Not imported: Create couldn't tell which column holds the addresses (its columns: “Who”)."),
            "{text}"
        );
        assert!(
            text.contains("Name that column email in the file's first row"),
            "{text}"
        );
    }
    assert!(stub
        .calls_to("POST", "/create/email/contacts/import")
        .is_empty());
}

#[tokio::test]
async fn an_import_needs_the_persons_go_for_the_eligible_count() {
    let stub = Stub::start(import_api(false)).await;
    let tool = stub.tool();
    let import = "/create/email/contacts/import";

    let unasked = tool.execute(import_input("import")).await.unwrap();
    let text = text_of(&unasked);
    assert!(
        text.starts_with("Not imported yet: the person's go is needed first. Show them: 117 contacts into the production audience “Coffee club” (114 can be sent to; 3 are on the suppression list), consent form, “Signed up at the Spring market”."),
        "{text}"
    );
    assert!(text.contains("confirm_rows 114"), "{text}");
    assert_eq!(structured(&unasked)["imported"], false);
    assert_eq!(structured(&unasked)["confirm_with"]["confirm_rows"], 114);

    let mut wrong = import_input("import");
    wrong["confirm_rows"] = json!(100);
    let refused = tool.execute(wrong).await.unwrap();
    assert!(
        text_of(&refused).starts_with(
            "Not imported: the file now has 114 people who can be sent to, not the 100 the person agreed to."
        ),
        "{}",
        text_of(&refused)
    );
    assert!(stub.calls_to("POST", import).is_empty());

    let mut count_only = import_input("import");
    count_only["confirm_rows"] = json!(114);
    let need_token = tool.execute(count_only).await.unwrap();
    assert!(
        text_of(&need_token).starts_with("Not imported yet:"),
        "{}",
        text_of(&need_token)
    );
    assert!(stub.calls_to("POST", import).is_empty());

    let mut go = structured(&unasked)["confirm_with"].clone();
    go["csv"] = json!(CSV);
    let imported = tool.execute(go.clone()).await.unwrap();
    let text = text_of(&imported);
    assert!(!imported.is_error, "{text}");
    let posts = stub.calls_to("POST", import);
    assert_eq!(posts.len(), 1);
    assert_eq!(
        posts[0].body,
        json!({
            "workspaceId": WORKSPACE, "audienceId": AUDIENCE, "csv": CSV,
            "mapping": import_preview_answer()["mapping"],
            "consentNote": "Signed up at the Spring market (consent source: form)",
        })
    );
    let key = posts[0].header("idempotency-key").unwrap().to_string();
    assert!(key.starts_with("email-import-"), "{key}");
    assert!(
        text.starts_with(
            "Imported into “Coffee club”: 110 new contacts and 7 updated, 117 put in the audience."
        ),
        "{text}"
    );
    assert!(
        text.contains("3 are on the suppression list: kept, and never sent to."),
        "{text}"
    );
    assert!(text.contains("2 rows weren't read"), "{text}");
    assert_eq!(structured(&imported)["imported"], true);

    // The same call again carries the same key, so a retried import is one import.
    tool.execute(go).await.unwrap();
    assert_eq!(
        stub.calls_to("POST", import)[1].header("idempotency-key"),
        Some(key.as_str())
    );

    // An import needs its audience.
    let mut nowhere = import_input("import");
    nowhere.as_object_mut().unwrap().remove("audience_id");
    let refused = tool.execute(nowhere).await.unwrap_err();
    assert!(
        refused.to_string().contains("audience_id is required"),
        "{refused}"
    );
}

#[tokio::test]
async fn an_import_tells_a_newer_create_what_the_person_saw() {
    let stub = Stub::start(import_api(true)).await;
    let tool = stub.tool();
    let mut preview_in = import_input("import_preview");
    preview_in["tags"] = json!(["spring-2026"]);
    let preview = tool.execute(preview_in).await.unwrap();
    let mut go = structured(&preview)["confirm_with"].clone();
    go["csv"] = json!(CSV);
    let imported = tool.execute(go).await.unwrap();
    assert!(!imported.is_error, "{}", text_of(&imported));
    let posts = stub.calls_to("POST", "/create/email/contacts/import");
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].body["consentSource"], "form");
    assert_eq!(posts[0].body["consentNote"], NOTE);
    assert_eq!(posts[0].body["expectedEligible"], 114);
    assert_eq!(posts[0].body["expectedDigest"], "5f2c9a");
    assert_eq!(posts[0].body["tags"], json!(["spring-2026"]));
}

#[tokio::test]
async fn an_import_checks_the_people_in_the_original_preview_even_if_the_count_matches() {
    let changed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = changed.clone();
    let base = import_api(true);
    let stub = Stub::start(Arc::new(move |r| {
        let mut answer = base(r);
        if r.path == "/create/email/contacts/import/preview"
            && flag.load(std::sync::atomic::Ordering::Relaxed)
        {
            answer.body["data"]["digest"] = json!("new-digest");
        }
        answer
    }))
    .await;
    let preview = run(&stub, import_input("import_preview")).await;
    assert!(text_of(&preview).contains("Suppressed addresses: not answered"));
    let mut approved = structured(&preview)["confirm_with"].clone();
    approved["csv"] = json!(CSV);
    changed.store(true, std::sync::atomic::Ordering::Relaxed);
    let result = run(&stub, approved).await;
    assert!(text_of(&result).contains("preview again"));
    assert!(stub
        .calls_to("POST", "/create/email/contacts/import")
        .is_empty());
}

#[tokio::test]
async fn an_import_asks_the_person_where_the_client_can() {
    let stub = Stub::start(import_api(false)).await;
    let tool = stub.tool();
    let import = "/create/email/contacts/import";
    let client = Arc::new(FakeClient::default());

    // Asked live: their no stands, whatever the agent confirms.
    let previewed = tool.execute(import_input("import_preview")).await.unwrap();
    *client.answer.lock().unwrap() = Some(json!({"action": "decline"}));
    let mut confirmed = structured(&previewed)["confirm_with"].clone();
    confirmed["csv"] = json!(CSV);
    let declined = run_with_call_peer(
        CallPeer::new(client.clone(), None, true),
        tool.execute(confirmed.clone()),
    )
    .await
    .unwrap();
    assert!(
        text_of(&declined).starts_with("Not imported: the person didn't say go"),
        "{}",
        text_of(&declined)
    );
    let asked = client.sent.lock().unwrap()[0].clone();
    assert_eq!(asked.0, "elicitation/create");
    let message = asked.1["message"].as_str().unwrap();
    assert!(
        message.starts_with("Import 117 contacts into the production audience “Coffee club” (114 can be sent to; 3 are on the suppression list), consent form, “Signed up at the Spring market”?"),
        "{message}"
    );
    assert!(stub.calls_to("POST", import).is_empty());

    *client.answer.lock().unwrap() =
        Some(json!({"action": "accept", "content": {"confirm": true}}));
    let imported = run_with_call_peer(
        CallPeer::new(client.clone(), None, true),
        tool.execute(import_input("import")),
    )
    .await
    .unwrap();
    assert!(!imported.is_error, "{}", text_of(&imported));
    assert!(
        text_of(&imported).contains("The person said go when the client asked them."),
        "{}",
        text_of(&imported)
    );
    assert_eq!(stub.calls_to("POST", import).len(), 1);

    // A stateless call: the question goes back as the result, bound to this file, this
    // audience and this count; with the agent's confirmed count no question is sent.
    let stateless = || {
        CallPeer::new(Arc::new(FakeClient::default()), None, false).with_inline_input(&json!({}))
    };
    let peer = stateless();
    let asking = run_with_call_peer(peer.clone(), tool.execute(import_input("import")))
        .await
        .unwrap();
    assert!(
        text_of(&asking).starts_with("Asking the person before"),
        "{}",
        text_of(&asking)
    );
    let question = peer.take_input_required().expect("input_required");
    let state = question["requestState"].as_str().unwrap();
    assert!(
        state.starts_with(&format!("studio-email-import:{AUDIENCE}:")) && state.ends_with(":114"),
        "{state}"
    );
    let peer = stateless();
    let sent = run_with_call_peer(peer.clone(), tool.execute(confirmed.clone()))
        .await
        .unwrap();
    assert!(!sent.is_error, "{}", text_of(&sent));
    assert!(peer.take_input_required().is_none(), "nobody is asked");
    assert_eq!(stub.calls_to("POST", import).len(), 2);

    // Where only the person's own yes counts, the agent's count imports nothing.
    let asked_only = mcp_types::config::ConfigOverride {
        go_ahead_asked_only: Some(true),
        ..Default::default()
    };
    let kept = mcp_client::run_with_config_override(asked_only, || async {
        tool.execute(confirmed.clone()).await.unwrap()
    })
    .await;
    assert!(
        text_of(&kept).starts_with("Not imported: on this connection a go-ahead counts only when the person is asked and says yes, and they could not be asked"),
        "{}",
        text_of(&kept)
    );
    assert_eq!(structured(&kept)["go_ahead"], "asked_only");
    assert_eq!(stub.calls_to("POST", import).len(), 2);
}

#[tokio::test]
async fn a_csv_file_is_read_only_by_a_local_binary_and_only_as_a_csv() {
    let stub = Stub::start(import_api(false)).await;
    let folder = tempfile::tempdir().unwrap();
    let file = folder.path().join("spring.csv");
    std::fs::write(&file, CSV).unwrap();
    let with_path = |path: &std::path::Path| {
        let mut input = import_input("import_preview");
        input.as_object_mut().unwrap().remove("csv");
        input["csv_path"] = json!(path.to_str().unwrap());
        input
    };

    let result = run(&stub, with_path(&file)).await;
    assert!(!result.is_error, "{}", text_of(&result));
    assert_eq!(
        stub.calls_to("POST", "/create/email/contacts/import/preview")[0].body["csv"],
        CSV
    );

    // Not a CSV by its name, not there, or too large: said before anything is sent.
    let secret = folder.path().join("id_rsa");
    std::fs::write(&secret, "-----BEGIN").unwrap();
    let large = folder.path().join("large.csv");
    std::fs::write(&large, vec![b'a'; 5 * 1024 * 1024 + 1]).unwrap();
    for (path, said) in [
        (secret.as_path(), "isn't a .csv, .tsv or .txt file"),
        (folder.path().join("missing.csv").as_path(), "can't be read"),
        (large.as_path(), "is over 5 MB"),
    ] {
        let refused = stub.tool().execute(with_path(path)).await.unwrap_err();
        assert!(refused.to_string().contains(said), "{refused}");
    }
    let mut inline = import_input("import_preview");
    inline["csv"] = json!("a".repeat(5 * 1024 * 1024 + 1));
    let refused = stub.tool().execute(inline).await.unwrap_err();
    assert!(refused.to_string().contains("is over 5 MB"), "{refused}");

    // The hosted gateway reads no caller's path.
    let mut config: Config = TestFixtures::test_config();
    config.api_url = stub.url.clone();
    config.api_key = Some("studio-test-key".to_string());
    config.default_workspace_id = Some(Uuid::parse_str(WORKSPACE).unwrap());
    config.is_http_transport = true;
    let client = ContextStreamClient::new(config.clone());
    let session = Arc::new(SessionManager::new(client.clone(), config));
    let hosted = StudioTool::new(client, session, Arc::new(StudioGate::new()));
    let refused = hosted.execute(with_path(&file)).await.unwrap_err();
    assert_eq!(
        refused.to_string(),
        Error::Validation(
            "Local files can't be read here: pass the file's text as csv.".to_string()
        )
        .to_string()
    );
    assert_eq!(
        stub.calls_to("POST", "/create/email/contacts/import/preview")
            .len(),
        1
    );
}

// ============================================================================
// email_segments and email_recipients
// ============================================================================

const NEW_SEGMENT: &str = "14141414-1414-4414-8414-141414141414";

fn rules() -> Value {
    json!({"match": "all", "conditions": [
        {"field": "tag", "op": "has", "value": "vip"},
        {"field": "engagement", "op": "opened_within", "value": "30"},
    ]})
}

fn segment(id: &str, name: &str, rules: &Value) -> Value {
    json!({
        "id": id, "audienceId": AUDIENCE, "name": name, "rules": rules,
        "contacts": 12, "subscribed": 10,
        "createdAt": "2026-10-01T10:00:00Z", "updatedAt": "2026-10-01T10:00:00Z",
    })
}

fn segments_api() -> Responder {
    over(
        |request| match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/create/email/audiences") => Some(Answer::ok(json!([
                audience(AUDIENCE, "Coffee club", Some("production"), 40, 37),
                audience(PROOF, "Proof", Some("test"), 3, 3),
            ]))),
            ("GET", "/create/email/segments") => {
                Some(Answer::ok(json!([segment(SEGMENT, "Regulars", &rules())])))
            }
            ("POST", "/create/email/segments/preview") => Some(Answer::ok(json!({
                "contacts": 12, "subscribed": 10,
                "sample": [
                    {"email": "ada@northwind.example", "firstName": "Ada"},
                    {"email": "bob@northwind.example", "firstName": null},
                ],
            }))),
            ("POST", "/create/email/segments") => {
                if request.body["rules"]["conditions"][0]["op"] == "wears" {
                    return Some(Answer::error(
                        400,
                        "BAD_REQUEST",
                        "Bad request: Condition 1: tag takes has, has_not.",
                    ));
                }
                Some(Answer::status(
                    201,
                    segment(
                        NEW_SEGMENT,
                        request.body["name"].as_str().unwrap_or(""),
                        &request.body["rules"],
                    ),
                ))
            }
            ("PATCH", path) if path == format!("/create/email/segments/{SEGMENT}") => {
                Some(Answer::ok(segment(
                    SEGMENT,
                    request.body["name"].as_str().unwrap_or("Regulars"),
                    request.body.get("rules").unwrap_or(&rules()),
                )))
            }
            _ => None,
        },
    )
}

#[tokio::test]
async fn segments_are_listed_previewed_created_and_changed() {
    let stub = Stub::start(segments_api()).await;

    let listed = run(&stub, json!({"action": "email_segments"})).await;
    let text = text_of(&listed);
    assert!(!listed.is_error, "{text}");
    assert!(text.starts_with("1 segment as of now:"), "{text}");
    assert!(
        text.contains(&format!(
            "- Regulars, of Coffee club: 12 contacts, 10 subscribed; all of: tag has vip; engagement opened within 30 (segment_id {SEGMENT})"
        )),
        "{text}"
    );
    assert_eq!(structured(&listed)["segments"][0]["subscribed"], 10);
    let scoped = run(
        &stub,
        json!({"action": "email_segments", "audience_id": AUDIENCE}),
    )
    .await;
    assert!(!scoped.is_error);
    assert!(stub
        .calls_to("GET", "/create/email/segments")
        .last()
        .unwrap()
        .query
        .contains(&format!("audienceId={AUDIENCE}")));

    // A preview counts who matches and saves nothing.
    let preview = run(
        &stub,
        json!({"action": "email_segments", "operation": "preview", "audience_id": AUDIENCE, "rules": rules()}),
    )
    .await;
    let text = text_of(&preview);
    assert!(!preview.is_error, "{text}");
    assert_eq!(
        stub.calls_to("POST", "/create/email/segments/preview")[0].body,
        json!({"workspaceId": WORKSPACE, "audienceId": AUDIENCE, "rules": rules()})
    );
    assert!(
        text.starts_with("These rules match 12 contacts in “Coffee club”, 10 of them subscribed: all of: tag has vip; engagement opened within 30."),
        "{text}"
    );
    assert!(
        text.contains("Sample: ada@northwind.example, bob@northwind.example."),
        "{text}"
    );
    assert!(text.contains("Nothing is saved"), "{text}");
    assert!(stub.calls_to("POST", "/create/email/segments").is_empty());

    let created = run(
        &stub,
        json!({"action": "email_segments", "operation": "create", "audience_id": AUDIENCE, "name": "VIPs", "rules": rules()}),
    )
    .await;
    let text = text_of(&created);
    assert!(!created.is_error, "{text}");
    assert_eq!(
        stub.calls_to("POST", "/create/email/segments")[0].body,
        json!({"workspaceId": WORKSPACE, "audienceId": AUDIENCE, "name": "VIPs", "rules": rules()})
    );
    assert!(
        text.starts_with(&format!(
            "Created the segment “VIPs” of “Coffee club” (segment_id {NEW_SEGMENT}): 12 contacts, 10 subscribed."
        )),
        "{text}"
    );
    assert_eq!(structured(&created)["segment_id"], NEW_SEGMENT);

    let renamed = run(
        &stub,
        json!({"action": "email_segments", "operation": "update", "segment_id": SEGMENT, "name": "Loyal regulars"}),
    )
    .await;
    assert!(!renamed.is_error, "{}", text_of(&renamed));
    assert_eq!(
        stub.calls_to("PATCH", &format!("/create/email/segments/{SEGMENT}"))[0].body,
        json!({"workspaceId": WORKSPACE, "name": "Loyal regulars"})
    );
    assert!(
        text_of(&renamed).starts_with(&format!(
            "Changed the segment “Loyal regulars” (segment_id {SEGMENT}): 12 contacts, 10 subscribed."
        )),
        "{}",
        text_of(&renamed)
    );

    // Create's own sentence about rules it refuses is kept.
    let mut bad = rules();
    bad["conditions"][0]["op"] = json!("wears");
    let refused = run(
        &stub,
        json!({"action": "email_segments", "operation": "create", "audience_id": AUDIENCE, "name": "Odd", "rules": bad}),
    )
    .await;
    assert_eq!(
        text_of(&refused),
        "Not created: Condition 1: tag takes has, has_not."
    );

    for (input, said) in [
        (
            json!({"action": "email_segments", "operation": "preview", "audience_id": AUDIENCE}),
            "rules is required",
        ),
        (
            json!({"action": "email_segments", "operation": "preview", "rules": rules()}),
            "audience_id is required",
        ),
        (
            json!({"action": "email_segments", "operation": "create", "audience_id": AUDIENCE, "rules": rules()}),
            "name is required",
        ),
        (
            json!({"action": "email_segments", "operation": "update", "segment_id": SEGMENT}),
            "needs something to change",
        ),
        (
            json!({"action": "email_segments", "operation": "update", "name": "X"}),
            "segment_id is required",
        ),
        (
            json!({"action": "email_segments", "operation": "preview", "audience_id": AUDIENCE, "rules": "vips"}),
            "rules is",
        ),
    ] {
        let refused = stub.tool().execute(input).await.unwrap_err();
        assert!(refused.to_string().contains(said), "{refused}");
    }
}

fn reach(basis: &str) -> Value {
    json!({
        "campaignId": CAMPAIGN, "status": if basis == "sends" { "paused" } else { "draft" }, "revision": 4,
        "audience": {"id": AUDIENCE, "name": "Coffee club", "purpose": "production"},
        "segment": null, "basis": basis, "contacts": 40, "eligible": if basis == "sends" { 25 } else { 37 },
        "leftOut": {"unsubscribed": 2, "bounced": 1, "complained": 0, "manual": 0},
        "leftOutSample": [{"email": "grace@northwind.example", "reason": "unsubscribed"}],
        "sample": [
            {"email": "ada@northwind.example", "firstName": "Ada"},
            {"email": "bob@northwind.example", "firstName": null},
        ],
    })
}

#[tokio::test]
async fn who_a_send_would_reach_is_read_from_create() {
    let stub = Stub::start(over(|request| {
        (request.path == format!("/create/email/campaigns/{CAMPAIGN}/reach"))
            .then(|| Answer::ok(reach("audience")))
    }))
    .await;
    let result = run(
        &stub,
        json!({"action": "email_recipients", "campaign_id": CAMPAIGN}),
    )
    .await;
    let text = text_of(&result);
    assert!(!result.is_error, "{text}");
    assert!(
        text.starts_with("As of now “Winter sale” (draft) would reach 37 people in the audience “Coffee club” (production), which holds 40 contacts."),
        "{text}"
    );
    assert!(
        text.contains("Left out: 2 unsubscribed, 1 bounced."),
        "{text}"
    );
    assert!(
        text.contains("Sample: ada@northwind.example, bob@northwind.example."),
        "{text}"
    );
    let out = structured(&result);
    assert_eq!(out["eligible"], 37);
    assert_eq!(out["audience"]["purpose"], "production");
    assert_eq!(out["left_out"]["unsubscribed"], 2);
    assert_eq!(
        out["left_out_sample"][0],
        json!({"email":"grace@northwind.example", "reason":"unsubscribed"})
    );
    assert!(text.contains("grace@northwind.example (unsubscribed)"));
    assert!(stub.requests().iter().all(|r| r.method == "GET"), "a read");

    // A campaign that has started: who still waits, of those it was written for.
    let paused = Stub::start(api_with_reach("sends")).await;
    let result = run(
        &paused,
        json!({"action": "email_recipients", "campaign_id": CAMPAIGN}),
    )
    .await;
    assert!(
        text_of(&result).starts_with("As of now “Winter sale” (paused) has 25 people still to reach in the audience “Coffee club” (production), of the 40 it was written for."),
        "{}",
        text_of(&result)
    );

    let missing = stub
        .tool()
        .execute(json!({"action": "email_recipients"}))
        .await;
    assert!(matches!(missing, Err(Error::Validation(_))));
}

#[tokio::test]
async fn reach_falls_back_only_on_a_missing_route_and_preserves_unknown_counts() {
    let forbidden = Stub::start(over(|r| {
        r.path
            .ends_with("/reach")
            .then(|| Answer::error(403, "FORBIDDEN", "Workspace access required."))
    }))
    .await;
    let input = json!({"action":"email_recipients","campaign_id":CAMPAIGN});
    let result = run(&forbidden, input.clone()).await;
    assert!(result.is_error);
    assert!(forbidden
        .calls_to("GET", "/create/email/contacts")
        .is_empty());
    let partial = Stub::start(over(|r| {
        r.path.ends_with("/reach").then(|| {
            let mut answer = reach("audience");
            answer["leftOut"] = json!({});
            answer.as_object_mut().unwrap().remove("leftOutSample");
            Answer::ok(answer)
        })
    }))
    .await;
    let result = run(&partial, input).await;
    assert!(text_of(&result).contains("not answered"));
    assert!(!text_of(&result).contains("No one in it is left out"));
    assert!(structured(&result)["left_out_sample"].is_null());
}

fn api_with_reach(basis: &'static str) -> Responder {
    let status = Arc::new(Mutex::new(
        if basis == "sends" { "paused" } else { "draft" }.to_string(),
    ));
    let base = api(status, 37);
    Arc::new(move |request: &Recorded| {
        if request.path == format!("/create/email/campaigns/{CAMPAIGN}/reach") {
            Answer::ok(reach(basis))
        } else {
            base(request)
        }
    })
}

/// Today's main has no `/reach`: the answer is put together from what it does answer, and
/// what it doesn't is said as not answered.
#[tokio::test]
async fn without_reach_the_recipients_are_put_together_from_what_create_answers() {
    let stub = Stub::start(over(|request| {
        if request.method != "GET" {
            return None;
        }
        match request.path.as_str() {
            "/create/email/audiences" => Some(Answer::ok(json!([audience(
                AUDIENCE,
                "Coffee club",
                None,
                40,
                37
            )]))),
            "/create/email/contacts" if request.query.contains("status=suppressed") => {
                Some(Answer::ok(json!({
                    "contacts": [
                        {"id": CONTACT, "email": "grace@northwind.example", "suppressed": {"reason": "unsubscribed", "at": "2026-10-20T08:00:00Z"}},
                        {"id": PROOF, "email": "hal@northwind.example", "suppressed": {"reason": "unsubscribed", "at": "2026-10-21T08:00:00Z"}},
                        {"id": NEW_AUDIENCE, "email": "bounced@resend.dev", "suppressed": {"reason": "bounced", "at": "2026-10-19T08:00:00Z"}},
                    ],
                    "nextCursor": null, "total": 3,
                })))
            }
            "/create/email/contacts" if request.query.contains("status=subscribed") => {
                Some(Answer::ok(json!({
                    "contacts": [{"id": CONTACT, "email": "ada@northwind.example", "suppressed": null}],
                    "nextCursor": null, "total": 37,
                })))
            }
            _ => None,
        }
    }))
    .await;
    let result = run(
        &stub,
        json!({"action": "email_recipients", "campaign_id": CAMPAIGN}),
    )
    .await;
    let text = text_of(&result);
    assert!(!result.is_error, "{text}");
    assert!(
        text.starts_with("As of now “Winter sale” (draft) would reach 37 people in the audience “Coffee club” (purpose not answered), which holds 40 contacts."),
        "{text}"
    );
    assert!(
        text.contains("Left out: 2 unsubscribed, 1 bounced."),
        "{text}"
    );
    assert!(text.contains("Sample: ada@northwind.example."), "{text}");
    assert_eq!(
        stub.calls_to("GET", &format!("/create/email/campaigns/{CAMPAIGN}/reach"))
            .len(),
        1,
        "asked once, and it isn't there"
    );
    let asked = stub.calls_to("GET", "/create/email/contacts");
    assert!(asked
        .iter()
        .any(|r| r.query.contains(&format!("audienceId={AUDIENCE}"))
            && r.query.contains("status=suppressed")));
    assert!(stub.requests().iter().all(|r| r.method == "GET"), "a read");
}

#[tokio::test]
async fn a_segments_left_out_people_are_not_answered_without_reach() {
    let stub = Stub::start({
        let segments = segments_api();
        Arc::new(move |request: &Recorded| {
            if request.method == "GET"
                && request.path == format!("/create/email/campaigns/{CAMPAIGN}")
            {
                let mut campaign = campaign("draft");
                campaign["segmentId"] = json!(SEGMENT);
                campaign["segment"] = json!({"id": SEGMENT, "name": "Regulars"});
                return Answer::ok(campaign);
            }
            if request.path == format!("/create/email/campaigns/{CAMPAIGN}/preview") {
                return Answer::ok(
                    json!({"subject": "s", "from": "Northwind <hello@news.northwind.example>", "warnings": [], "recipients": 10}),
                );
            }
            segments(request)
        })
    })
    .await;
    let result = run(
        &stub,
        json!({"action": "email_recipients", "campaign_id": CAMPAIGN}),
    )
    .await;
    let text = text_of(&result);
    assert!(!result.is_error, "{text}");
    assert!(
        text.starts_with("As of now “Winter sale” (draft) would reach 10 people in the segment “Regulars” of the audience “Coffee club” (production), which holds 12 contacts."),
        "{text}"
    );
    assert!(
        text.contains("Left out: 2, and why is not answered for a segment."),
        "{text}"
    );
    assert!(
        text.contains("Sample: ada@northwind.example, bob@northwind.example."),
        "{text}"
    );
    assert_eq!(structured(&result)["left_out"], Value::Null);
    // The sample comes from the segment's own rules.
    assert_eq!(
        stub.calls_to("POST", "/create/email/segments/preview")[0].body,
        json!({"workspaceId": WORKSPACE, "audienceId": AUDIENCE, "rules": rules()})
    );
}

#[tokio::test]
async fn a_campaign_with_no_audience_reaches_no_one_and_says_so() {
    let stub = Stub::start(over(|request| {
        if request.method == "GET" && request.path == format!("/create/email/campaigns/{CAMPAIGN}")
        {
            let mut campaign = campaign("draft");
            campaign["audienceId"] = Value::Null;
            campaign["audience"] = Value::Null;
            return Some(Answer::ok(campaign));
        }
        (request.path == format!("/create/email/campaigns/{CAMPAIGN}/preview")).then(|| {
            Answer::ok(
                json!({"subject": "s", "warnings": ["Pick who it goes to."], "recipients": null}),
            )
        })
    }))
    .await;
    let result = run(
        &stub,
        json!({"action": "email_recipients", "campaign_id": CAMPAIGN}),
    )
    .await;
    assert!(
        text_of(&result).starts_with(
            "As of now “Winter sale” (draft) has no audience, so a send would reach no one."
        ),
        "{}",
        text_of(&result)
    );
    assert_eq!(structured(&result)["eligible"], Value::Null);
}

#[test]
fn segments_and_recipients_are_listed_described_and_in_the_schema() {
    for action in ["email_segments", "email_recipients"] {
        assert!(EMAIL_ACTIONS.contains(&action), "{action}");
        assert!(VALID_ACTIONS.contains(&action), "{action}");
        assert!(TOOL_REFERENCE.contains(&format!("- {action}:")), "{action}");
    }
    let schema = input_schema();
    let properties = schema["properties"].as_object().unwrap();
    assert_eq!(properties["rules"]["type"], "object");
    assert!(properties["operation"]["enum"]
        .as_array()
        .unwrap()
        .contains(&json!("preview")));
}

// ============================================================================
// email_diagnostics and the unsubscribe report
// ============================================================================

/// `GET /webhook/diagnostics` as today's main answers it.
fn diagnostics(status: &str, sent: i64, applied: i64, last_applied: Option<&str>) -> Value {
    json!({
        "endpoint": format!("https://api.contextstream.io/api/v1/create/email/webhooks/{WORKSPACE}"),
        "mode": "auto", "secretSet": true,
        "lastDeliveryAt": "2026-11-17T09:05:00Z", "lastAppliedAt": last_applied,
        "days": 7,
        "outcomes": {"applied": applied, "duplicate": 3, "not_ours": 120, "test": 1,
                     "refused_signature": 2, "refused_late": 0, "malformed": 0, "no_secret": 0},
        "coverage": {"sent": sent, "eventsPerSend": 1.3, "status": status},
        "recent": [
            {"receivedAt": "2026-11-17T09:05:00Z", "type": "email.opened", "outcome": "applied", "detail": null, "svixId": "msg_1"},
            {"receivedAt": "2026-11-17T09:00:00Z", "type": "email.delivered", "outcome": "applied", "detail": null, "svixId": "msg_2"},
            {"receivedAt": "2026-11-17T08:59:00Z", "type": "email.delivered", "outcome": "applied", "detail": null, "svixId": "msg_3"},
        ],
        "resend": {"checked": true, "found": true, "endpointMatches": true, "enabled": true, "missingEvents": [], "problem": null},
    })
}

fn diagnostics_api(answer: Value) -> Responder {
    over(move |request| {
        (request.path == "/create/email/webhook/diagnostics").then(|| Answer::ok(answer.clone()))
    })
}

#[tokio::test]
async fn the_webhook_is_read_with_one_plain_verdict() {
    let stub = Stub::start(diagnostics_api(diagnostics(
        "healthy",
        40,
        52,
        Some("2026-11-17T09:05:00Z"),
    )))
    .await;
    let result = run(&stub, json!({"action": "email_diagnostics"})).await;
    let text = text_of(&result);
    assert!(!result.is_error, "{text}");
    let asked = stub.calls_to("GET", "/create/email/webhook/diagnostics");
    assert_eq!(asked.len(), 1);
    assert!(asked[0].query.contains("days=7"), "{}", asked[0].query);
    assert!(
        text.starts_with("The webhook over the last 7 days, as of now.\nVerdict: events are arriving: 52 applied for 40 emails sent in the last 7 days, the last on 2026-11-17 09:05 UTC."),
        "{text}"
    );
    assert!(
        text.contains("- Set up by Create, with its signing secret; in Resend it is registered and enabled, with every event."),
        "{text}"
    );
    assert!(
        text.contains("- Deliveries: 52 applied, 3 repeats, 120 for other mail on this Resend account, 1 test."),
        "{text}"
    );
    assert!(
        text.contains("- Refused: 2 with a bad signature, 0 too late, 0 malformed, 0 while no signing secret was set."),
        "{text}"
    );
    assert!(
        text.contains("- By type, over the last 3 deliveries read: delivered 2, opened 1."),
        "{text}"
    );
    let out = structured(&result);
    assert_eq!(out["verdict"]["status"], "arriving");
    assert_eq!(out["outcomes"]["refused_signature"], 2);
    assert!(
        !text.contains("api.contextstream.io"),
        "the URL names the workspace: not in the words"
    );

    let refused = stub
        .tool()
        .execute(json!({"action": "email_diagnostics", "days": 31}))
        .await
        .unwrap_err();
    assert!(refused.to_string().contains("days is 1 to 30"), "{refused}");
}

#[tokio::test]
async fn events_that_stopped_arriving_are_said_as_possibly_missing() {
    let stub = Stub::start(diagnostics_api(diagnostics(
        "missing",
        40,
        0,
        Some("2026-11-10T09:00:00Z"),
    )))
    .await;
    let text = text_of(&run(&stub, json!({"action": "email_diagnostics", "days": 14})).await);
    assert!(
        text.contains("Verdict: events are possibly missing: none arrived in the last 14 days although 40 emails were sent (the last one arrived on 2026-11-10 09:00 UTC): opens, clicks, bounces and complaints may be missing."),
        "{text}"
    );

    // Fewer than one event an email.
    let stub = Stub::start(diagnostics_api(diagnostics(
        "low",
        40,
        24,
        Some("2026-11-17T09:05:00Z"),
    )))
    .await;
    let text = text_of(&run(&stub, json!({"action": "email_diagnostics"})).await);
    assert!(
        text.contains("Verdict: events may be missing: 24 arrived for 40 emails sent in the last 7 days, fewer than one an email (the last on 2026-11-17 09:05 UTC): some opens, clicks, bounces and complaints may be missing."),
        "{text}"
    );

    // Nothing sent: nothing to compare, and no number is made up.
    let stub = Stub::start(diagnostics_api(diagnostics("idle", 0, 0, None))).await;
    let result = run(&stub, json!({"action": "email_diagnostics"})).await;
    let text = text_of(&result);
    assert!(
        text.contains("Verdict: nothing was sent in the last 7 days, so there is nothing to compare; no event has been applied yet."),
        "{text}"
    );
    assert_eq!(structured(&result)["verdict"]["status"], "idle");

    // No signing secret: nothing can be verified, whatever the counts.
    let mut unset = diagnostics("healthy", 40, 52, Some("2026-11-17T09:05:00Z"));
    unset["secretSet"] = json!(false);
    unset["mode"] = json!("manual");
    unset["resend"] = json!({"checked": false, "problem": "Create didn't make this webhook (it was set up by hand), so it can't check it."});
    let stub = Stub::start(diagnostics_api(unset)).await;
    let text = text_of(&run(&stub, json!({"action": "email_diagnostics"})).await);
    assert!(
        text.contains("Verdict: events are possibly missing: the webhook has no signing secret, so no event can be verified."),
        "{text}"
    );
    assert!(
        text.contains("- Set up by hand, with no signing secret (the team pastes it in the studio: https://ctxproduct.com/studio/email/settings); whether Resend has it is not answered: Create didn't make this webhook (it was set up by hand), so it can't check it."),
        "{text}"
    );

    // A status this tool doesn't know is not read as good news.
    let mut odd = diagnostics("sideways", 40, 52, None);
    odd["coverage"] = json!({});
    let stub = Stub::start(diagnostics_api(odd)).await;
    let text = text_of(&run(&stub, json!({"action": "email_diagnostics"})).await);
    assert!(text.contains("Verdict: not answered."), "{text}");
}

#[tokio::test]
async fn a_newer_creates_own_verdict_and_event_types_are_used() {
    let mut newer = diagnostics("healthy", 40, 52, Some("2026-11-17T09:05:00Z"));
    newer["verdict"] = json!({
        "status": "missing",
        "sentence": "No events since 2026-11-10T09:00:00Z although 40 emails were sent after it: opens, bounces and complaints may be missing.",
        "lastEventAt": "2026-11-10T09:00:00Z", "sent": 40, "sentSinceLastEvent": 40,
    });
    newer["events"] = json!({"sent": 40, "delivered": 38, "delivery_delayed": 0, "opened": 20, "clicked": 6, "bounced": 1, "complained": 0, "failed": 0, "suppressed": 0});
    newer["lastEvent"] = json!({"type": "email.opened", "at": "2026-11-10T09:00:00Z"});
    let stub = Stub::start(diagnostics_api(newer)).await;
    let result = run(&stub, json!({"action": "email_diagnostics"})).await;
    let text = text_of(&result);
    assert!(
        text.contains("Verdict: No events since 2026-11-10T09:00:00Z although 40 emails were sent after it: opens, bounces and complaints may be missing."),
        "{text}"
    );
    assert!(
        text.contains("- By type: sent 40, delivered 38, opened 20, clicked 6, bounced 1."),
        "{text}"
    );
    assert!(
        text.contains("- Last event: opened, on 2026-11-10 09:00 UTC."),
        "{text}"
    );
    assert_eq!(structured(&result)["verdict"]["status"], "missing");
}

#[tokio::test]
async fn diagnostics_without_a_connection_say_so() {
    let stub = Stub::start(over(|request| {
        (request.path == "/create/email/webhook/diagnostics")
            .then(|| Answer::error(404, "NOT_FOUND", "Not found: Resend isn't connected."))
    }))
    .await;
    let result = run(&stub, json!({"action": "email_diagnostics"})).await;
    assert!(result.is_error);
    assert_eq!(
        text_of(&result),
        "Resend isn't connected yet, so there is no webhook and no event can arrive: the team connects it in the studio (https://ctxproduct.com/studio/email/settings)."
    );
}

fn summary() -> Value {
    json!({
        "days": 30, "total": 7,
        "byReason": {"unsubscribed": 4, "bounced": 2, "complained": 1, "manual": 0},
        "bySource": {"one_click": 3, "page": 1, "webhook": 3, "manual": 0, "api": 0},
        "feedback": {"too_many": 2, "not_relevant": 1, "never_signed_up": 0, "other": 0, "none": 1},
        "series": [],
        "campaigns": [
            {"id": CAMPAIGN, "name": "Winter sale", "sent": 37, "unsubscribed": 3, "complained": 1, "rate": 0.0811},
            {"id": COPY, "name": "Autumn news", "sent": 40, "unsubscribed": 1, "complained": 0, "rate": 0.025},
        ],
        "resubscribed": 1,
    })
}

#[tokio::test]
async fn the_unsubscribe_report_keeps_its_numbers_apart_and_says_whether_to_trust_them() {
    let stub = Stub::start(over(|request| match request.path.as_str() {
        "/create/email/suppressions/summary" => Some(Answer::ok(summary())),
        "/create/email/webhook/diagnostics" => Some(Answer::ok(diagnostics(
            "missing",
            77,
            0,
            Some("2026-11-10T09:00:00Z"),
        ))),
        _ => None,
    }))
    .await;
    let result = run(
        &stub,
        json!({"action": "email_unsubscribes", "operation": "report"}),
    )
    .await;
    let text = text_of(&result);
    assert!(!result.is_error, "{text}");
    assert!(
        text.starts_with("Opt-outs over the last 30 days, as of now:\n- Unsubscribed 4 · bounced 2 · spam complaints 1 · added by hand 0; 1 resubscribed."),
        "{text}"
    );
    assert!(
        text.contains("- Why they left: too many emails 2, not relevant 1, no reason given 1."),
        "{text}"
    );
    assert!(
        text.contains("- By campaign: “Winter sale” 3 unsubscribed, 1 spam complaint, of 37 sent (8.1%); “Autumn news” 1 unsubscribed, of 40 sent (2.5%)."),
        "{text}"
    );
    assert!(
        text.contains("- Can they be trusted? Unsubscribes are recorded by ContextStream's own unsubscribe link, so they don't depend on Resend's events. Bounces and complaints arrive through Resend's webhook: events are possibly missing: none arrived in the last 30 days although 77 emails were sent (the last one arrived on 2026-11-10 09:00 UTC): opens, clicks, bounces and complaints may be missing."),
        "{text}"
    );
    let out = structured(&result);
    assert_eq!(out["by_reason"]["bounced"], 2);
    assert_eq!(out["verdict"]["status"], "missing");
    assert!(
        stub.calls_to("GET", "/create/email/suppressions/summary")[0]
            .query
            .contains("days=30")
    );
    assert!(stub.calls_to("GET", "/create/email/webhook/diagnostics")[0]
        .query
        .contains("days=30"));
    assert!(stub.requests().iter().all(|r| r.method == "GET"), "a read");

    // A longer report still asks the webhook about the 30 days it keeps.
    run(
        &stub,
        json!({"action": "email_unsubscribes", "operation": "report", "days": 90}),
    )
    .await;
    assert!(
        stub.calls_to("GET", "/create/email/suppressions/summary")[1]
            .query
            .contains("days=90")
    );
    assert!(stub.calls_to("GET", "/create/email/webhook/diagnostics")[1]
        .query
        .contains("days=30"));
}

/// Today's main ignores `campaignId` on the summary: a campaign's numbers are then its own
/// report's, never the workspace's passed off as the campaign's.
#[tokio::test]
async fn a_campaigns_unsubscribe_report_is_never_the_workspaces() {
    let stub = Stub::start(over(|request| match request.path.as_str() {
        "/create/email/suppressions/summary" => Some(Answer::ok(summary())),
        "/create/email/webhook/diagnostics" => Some(Answer::ok(diagnostics(
            "healthy",
            37,
            52,
            Some("2026-11-17T09:05:00Z"),
        ))),
        _ => None,
    }))
    .await;
    let result = run(
        &stub,
        json!({"action": "email_unsubscribes", "operation": "report", "campaign_id": CAMPAIGN}),
    )
    .await;
    let text = text_of(&result);
    assert!(!result.is_error, "{text}");
    // The campaign's own report (the usual stub): 1 unsubscribed, 1 bounced, 1 complaint.
    assert!(
        text.starts_with("Opt-outs from “Winter sale”, as of now:\n- Unsubscribed 1 · bounced 1 · spam complaints 1, of 37 sent."),
        "{text}"
    );
    assert!(
        text.contains("- Why they left: too many emails 1."),
        "{text}"
    );
    assert!(!text.contains("Unsubscribed 4"), "{text}");
    let listed = stub.calls_to("GET", "/create/email/suppressions");
    assert!(listed[0].query.contains(&format!("campaignId={CAMPAIGN}")));
    assert!(listed[0].query.contains("reason=unsubscribed"));
    assert_eq!(structured(&result)["campaign_id"], CAMPAIGN);
}

#[tokio::test]
async fn a_newer_create_narrows_the_report_and_gives_the_verdict_itself() {
    let stub = Stub::start(over(|request| {
        (request.path == "/create/email/suppressions/summary").then(|| {
            let mut narrowed = summary();
            narrowed["byReason"] = json!({"unsubscribed": 3, "bounced": 0, "complained": 1, "manual": 0});
            narrowed["feedback"] = json!({"too_many": 2, "not_relevant": 0, "never_signed_up": 0, "other": 0, "none": 1});
            narrowed["sent"] = json!(37);
            narrowed["lastOptOutAt"] = json!("2026-11-12T10:00:00Z");
            narrowed["webhook"] = json!({
                "status": "arriving", "sentence": "Events are arriving: the last one at 2026-11-17T09:05:00Z.",
                "lastEventAt": "2026-11-17T09:05:00Z", "sent": 37, "sentSinceLastEvent": 0,
            });
            Answer::ok(narrowed)
        })
    }))
    .await;
    let result = run(
        &stub,
        json!({"action": "email_unsubscribes", "operation": "report", "campaign_id": CAMPAIGN, "days": 14}),
    )
    .await;
    let text = text_of(&result);
    assert!(
        text.starts_with("Opt-outs from “Winter sale” over the last 14 days, as of now:\n- Unsubscribed 3 · bounced 0 · spam complaints 1 · added by hand 0, of 37 sent; the last on 2026-11-12 10:00 UTC."),
        "{text}"
    );
    assert!(
        text.contains("Bounces and complaints arrive through Resend's webhook: Events are arriving: the last one at 2026-11-17T09:05:00Z."),
        "{text}"
    );
    let asked = stub.calls_to("GET", "/create/email/suppressions/summary");
    assert!(asked[0].query.contains(&format!("campaignId={CAMPAIGN}")));
    assert!(asked[0].query.contains("days=14"));
    assert!(
        stub.calls_to("GET", "/create/email/webhook/diagnostics")
            .is_empty(),
        "the summary already says"
    );
}

#[test]
fn diagnostics_and_the_report_are_listed_described_and_in_the_schema() {
    assert!(EMAIL_ACTIONS.contains(&"email_diagnostics"));
    assert!(VALID_ACTIONS.contains(&"email_diagnostics"));
    assert!(TOOL_REFERENCE.contains("- email_diagnostics:"));
    assert!(TOOL_REFERENCE.contains("report (days, campaign_id"));
    assert!(input_schema()["properties"]["operation"]["enum"]
        .as_array()
        .unwrap()
        .contains(&json!("report")));
}

// ============================================================================
// email_templates and email_ab
// ============================================================================

const TEMPLATE: &str = "15151515-1515-4515-8515-151515151515";

fn template(id: &str, name: &str, built_in: bool) -> Value {
    json!({
        "id": id, "name": name,
        "description": if built_in { json!("One piece of news, one thing to do.") } else { Value::Null },
        "subject": if built_in { "[Your news in a few words]" } else { "Our winter sale starts today" },
        "preheader": "Twenty percent off everything until Sunday.",
        "content": {"version": 1, "blocks": [
            {"id": "h", "type": "heading", "text": "Winter is here"},
            {"id": "b", "type": "button", "label": "Shop the sale", "url": "https://northwind.example/sale"},
        ]},
        "builtIn": built_in, "createdBy": null, "createdAt": null, "updatedAt": null,
    })
}

fn templates_api() -> Responder {
    over(|request| {
        let one = format!("/create/email/templates/{TEMPLATE}");
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/create/email/templates") => Some(Answer::ok(json!({"templates": [
                template(TEMPLATE, "Sale layout", false),
                template("builtin:announcement", "Announcement", true),
            ]}))),
            ("GET", path) if path == one => {
                Some(Answer::ok(template(TEMPLATE, "Sale layout", false)))
            }
            ("GET", "/create/email/templates/builtin:announcement") => Some(Answer::ok(template(
                "builtin:announcement",
                "Announcement",
                true,
            ))),
            ("GET", path) if path.starts_with("/create/email/templates/") => Some(Answer::error(
                404,
                "NOT_FOUND",
                "Not found: No such template.",
            )),
            ("POST", "/create/email/templates") => Some(Answer::status(
                201,
                template(TEMPLATE, request.body["name"].as_str().unwrap_or(""), false),
            )),
            ("PATCH", "/create/email/templates/builtin:announcement") => Some(Answer::error(
                409,
                "CONFLICT",
                "Conflict: A built-in template can't be changed; save a copy.",
            )),
            ("PATCH", path) if path == one => Some(Answer::ok(template(
                TEMPLATE,
                request.body["name"].as_str().unwrap_or("Sale layout"),
                false,
            ))),
            ("POST", "/create/email/campaigns") => {
                let mut draft = campaign("draft");
                draft["id"] = json!(COPY);
                draft["name"] = request
                    .body
                    .get("name")
                    .cloned()
                    .unwrap_or(json!("Sale layout"));
                Some(Answer::status(201, draft))
            }
            _ => None,
        }
    })
}

#[tokio::test]
async fn templates_are_listed_read_saved_used_and_renamed() {
    let stub = Stub::start(templates_api()).await;

    let listed = run(&stub, json!({"action": "email_templates"})).await;
    let text = text_of(&listed);
    assert!(!listed.is_error, "{text}");
    assert!(text.starts_with("2 templates as of now:"), "{text}");
    assert!(
        text.contains(&format!(
            "- Sale layout: subject “Our winter sale starts today” (template_id {TEMPLATE})"
        )),
        "{text}"
    );
    assert!(
        text.contains("- Announcement (built in): One piece of news, one thing to do. (template_id builtin:announcement)"),
        "{text}"
    );

    let read = run(
        &stub,
        json!({"action": "email_templates", "operation": "get", "template_id": "builtin:announcement"}),
    )
    .await;
    let text = text_of(&read);
    assert!(
        text.starts_with("The template “Announcement” (built in; template_id builtin:announcement): One piece of news, one thing to do."),
        "{text}"
    );
    assert!(
        text.contains("Subject: [Your news in a few words]"),
        "{text}"
    );
    assert!(text.contains("1. Heading: Winter is here"), "{text}");
    assert!(
        text.contains("Words in [square brackets] are placeholders"),
        "{text}"
    );

    // A campaign saved as a template.
    let saved = run(
        &stub,
        json!({"action": "email_templates", "operation": "save", "campaign_id": CAMPAIGN, "name": "Sale layout", "description": "The winter sale's shape"}),
    )
    .await;
    assert!(!saved.is_error, "{}", text_of(&saved));
    assert_eq!(
        stub.calls_to("POST", "/create/email/templates")[0].body,
        json!({"workspaceId": WORKSPACE, "name": "Sale layout", "description": "The winter sale's shape", "campaignId": CAMPAIGN})
    );
    assert!(
        text_of(&saved).starts_with(&format!(
            "Saved the template “Sale layout” (template_id {TEMPLATE}) from the campaign"
        )),
        "{}",
        text_of(&saved)
    );

    // A new draft from a template: nothing is sent.
    let used = run(
        &stub,
        json!({"action": "email_templates", "operation": "use", "template_id": TEMPLATE, "name": "Spring sale", "audience_id": AUDIENCE}),
    )
    .await;
    let text = text_of(&used);
    assert!(!used.is_error, "{text}");
    assert_eq!(
        stub.calls_to("POST", "/create/email/campaigns")[0].body,
        json!({"workspaceId": WORKSPACE, "templateId": TEMPLATE, "name": "Spring sale", "audienceId": AUDIENCE})
    );
    assert!(
        text.starts_with(&format!(
            "Started the draft “Spring sale” from the template (campaign_id {COPY}). Nothing is sent."
        )),
        "{text}"
    );
    assert!(
        text.contains("Subject: Our winter sale starts today"),
        "{text}"
    );
    assert_eq!(structured(&used)["campaign_id"], COPY);
    assert!(stub
        .requests()
        .iter()
        .all(|r| !r.path.ends_with("/schedule") && !r.path.ends_with("/test")));

    let renamed = run(
        &stub,
        json!({"action": "email_templates", "operation": "update", "template_id": TEMPLATE, "name": "Sale shape"}),
    )
    .await;
    assert!(!renamed.is_error, "{}", text_of(&renamed));
    assert_eq!(
        stub.calls_to("PATCH", &format!("/create/email/templates/{TEMPLATE}"))[0].body,
        json!({"workspaceId": WORKSPACE, "name": "Sale shape"})
    );
    assert!(
        text_of(&renamed).starts_with(&format!(
            "Changed the template: “Sale shape” (template_id {TEMPLATE})."
        )),
        "{}",
        text_of(&renamed)
    );

    // Create's refusals in one sentence each.
    let built_in = run(
        &stub,
        json!({"action": "email_templates", "operation": "update", "template_id": "builtin:announcement", "name": "Mine"}),
    )
    .await;
    assert_eq!(
        text_of(&built_in),
        "Not changed: A built-in template can't be changed; save a copy."
    );
    let missing = run(
        &stub,
        json!({"action": "email_templates", "operation": "get", "template_id": NEW_AUDIENCE}),
    )
    .await;
    assert_eq!(
        text_of(&missing),
        "There is no such template in this workspace (email_templates lists them)."
    );

    for (input, said) in [
        (
            json!({"action": "email_templates", "operation": "get"}),
            "template_id is required",
        ),
        (
            json!({"action": "email_templates", "operation": "get", "template_id": "../connection"}),
            "template_id is a template's id",
        ),
        (
            json!({"action": "email_templates", "operation": "save", "name": "X"}),
            "campaign_id is required",
        ),
        (
            json!({"action": "email_templates", "operation": "save", "campaign_id": CAMPAIGN}),
            "name is required",
        ),
        (
            json!({"action": "email_templates", "operation": "update", "template_id": TEMPLATE}),
            "needs something to change",
        ),
    ] {
        let refused = stub.tool().execute(input).await.unwrap_err();
        assert!(refused.to_string().contains(said), "{refused}");
    }
}

/// The campaign routes with an A/B test kept on the draft.
fn ab_api(status: &'static str, test: Option<Value>) -> Responder {
    let kept = Arc::new(Mutex::new(test));
    let base = api(Arc::new(Mutex::new(status.to_string())), 37);
    Arc::new(move |request: &Recorded| {
        let path = format!("/create/email/campaigns/{CAMPAIGN}");
        let with_test = |kept: &Option<Value>, subject: Option<&Value>| {
            let mut campaign = campaign(status);
            campaign["abTest"] = kept.clone().unwrap_or(Value::Null);
            if let Some(subject) = subject {
                campaign["subject"] = subject.clone();
            }
            campaign
        };
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", p) if p == path => Answer::ok(with_test(&kept.lock().unwrap(), None)),
            ("PATCH", p) if p == path => {
                let mut kept = kept.lock().unwrap();
                *kept = match request.body.get("abTest") {
                    Some(Value::Null) | None => None,
                    Some(asked) => Some(json!({
                        "subjects": asked["subjects"],
                        "samplePercent": asked.get("samplePercent").cloned().unwrap_or(json!(20)),
                        "waitHours": asked.get("waitHours").cloned().unwrap_or(json!(4)),
                        "metric": asked.get("metric").cloned().unwrap_or(json!("opens")),
                        "decideAt": null, "winner": null,
                    })),
                };
                let mut changed = with_test(&kept, request.body.get("subject"));
                changed["revision"] = json!(5);
                Answer::ok(changed)
            }
            _ => base(request),
        }
    })
}

#[tokio::test]
async fn a_drafts_subjects_are_tested_against_each_other() {
    let stub = Stub::start(ab_api("draft", None)).await;
    let path = format!("/create/email/campaigns/{CAMPAIGN}");

    let none = run(
        &stub,
        json!({"action": "email_ab", "campaign_id": CAMPAIGN}),
    )
    .await;
    assert!(
        text_of(&none).starts_with("“Winter sale” has no A/B test: it goes with its one subject, “Our winter sale starts today”."),
        "{}",
        text_of(&none)
    );
    assert_eq!(structured(&none)["ab_test"], Value::Null);

    // A is the email's own subject: listed first and unchanged, it isn't sent again.
    let set = run(
        &stub,
        json!({
            "action": "email_ab", "campaign_id": CAMPAIGN, "operation": "set",
            "subjects": ["Our winter sale starts today", " Twenty percent off, this week only "],
            "split": 30, "decide_after": 6, "metric": "Clicks",
        }),
    )
    .await;
    let text = text_of(&set);
    assert!(!set.is_error, "{text}");
    let patches = stub.calls_to("PATCH", &path);
    assert_eq!(
        patches[0].body,
        json!({
            "workspaceId": WORKSPACE, "revision": 4,
            "abTest": {"subjects": ["Twenty percent off, this week only"], "samplePercent": 30, "waitHours": 6, "metric": "clicks"},
        })
    );
    assert!(
        text.starts_with("Set the A/B test on “Winter sale”: A “Our winter sale starts today”, B “Twenty percent off, this week only”, tried on 30% of its recipients; after 6 hours the subject with the most clicks goes to the rest."),
        "{text}"
    );
    assert!(text.contains("Nothing is sent"), "{text}");

    // A new A becomes the email's subject, with the test, in one change.
    let three = run(
        &stub,
        json!({
            "action": "email_ab", "campaign_id": CAMPAIGN, "operation": "set",
            "subjects": ["Warm up with 20% off", "Our winter sale starts today", "Twenty percent off, this week only"],
        }),
    )
    .await;
    assert!(!three.is_error, "{}", text_of(&three));
    assert_eq!(
        stub.calls_to("PATCH", &path)[1].body,
        json!({
            "workspaceId": WORKSPACE, "revision": 4, "subject": "Warm up with 20% off",
            "abTest": {"subjects": ["Our winter sale starts today", "Twenty percent off, this week only"]},
        })
    );
    assert!(
        text_of(&three).contains("A “Warm up with 20% off”, B “Our winter sale starts today”, C “Twenty percent off, this week only”, tried on 20% of its recipients; after 4 hours the subject with the most opens goes to the rest."),
        "{}",
        text_of(&three)
    );

    let shown = run(
        &stub,
        json!({"action": "email_ab", "campaign_id": CAMPAIGN}),
    )
    .await;
    assert!(
        text_of(&shown).starts_with("“Winter sale” tests 3 subjects: A “Our winter sale starts today”, B “Our winter sale starts today”"),
        "{}",
        text_of(&shown)
    );

    let cleared = run(
        &stub,
        json!({"action": "email_ab", "campaign_id": CAMPAIGN, "operation": "clear"}),
    )
    .await;
    assert_eq!(
        stub.calls_to("PATCH", &path)[2].body,
        json!({"workspaceId": WORKSPACE, "revision": 4, "abTest": null})
    );
    assert!(
        text_of(&cleared)
            .starts_with("Cleared the A/B test on “Winter sale”: it goes with its one subject"),
        "{}",
        text_of(&cleared)
    );

    for (input, said) in [
        (
            json!({"action": "email_ab", "campaign_id": CAMPAIGN, "operation": "set", "subjects": ["Only one"]}),
            "subjects is 2 or 3 subject lines",
        ),
        (
            json!({"action": "email_ab", "campaign_id": CAMPAIGN, "operation": "set", "subjects": ["Same", "same "]}),
            "must differ",
        ),
        (
            json!({"action": "email_ab", "campaign_id": CAMPAIGN, "operation": "set", "subjects": ["A", "B"], "split": 60}),
            "split is 10 to 50",
        ),
        (
            json!({"action": "email_ab", "campaign_id": CAMPAIGN, "operation": "set", "subjects": ["A", "B"], "decide_after": 0}),
            "decide_after is 1 to 72",
        ),
        (
            json!({"action": "email_ab", "campaign_id": CAMPAIGN, "operation": "set", "subjects": ["A", "B"], "metric": "replies"}),
            "metric is opens or clicks",
        ),
        (
            json!({"action": "email_ab", "operation": "set", "subjects": ["A", "B"]}),
            "campaign_id is required",
        ),
    ] {
        let refused = stub.tool().execute(input).await.unwrap_err();
        assert!(refused.to_string().contains(said), "{refused}");
    }
    assert_eq!(stub.calls_to("PATCH", &path).len(), 3);
}

#[tokio::test]
async fn only_a_drafts_subjects_can_be_set_and_a_decided_test_names_its_winner() {
    let decided = json!({
        "subjects": ["Twenty percent off, this week only"], "samplePercent": 20, "waitHours": 4, "metric": "opens",
        "decideAt": "2026-11-02T13:00:00Z",
        "winner": {"variant": 1, "subject": "Twenty percent off, this week only", "decidedAt": "2026-11-02T13:00:00Z", "reason": "opens", "rates": []},
    });
    let stub = Stub::start(ab_api("sent", Some(decided))).await;
    let shown = run(
        &stub,
        json!({"action": "email_ab", "campaign_id": CAMPAIGN}),
    )
    .await;
    let text = text_of(&shown);
    assert!(
        text.contains("The winner was B “Twenty percent off, this week only” (the most opens), picked on 2026-11-02 13:00 UTC."),
        "{text}"
    );
    for operation in ["set", "clear"] {
        let refused = run(
            &stub,
            json!({"action": "email_ab", "campaign_id": CAMPAIGN, "operation": operation, "subjects": ["A", "B"]}),
        )
        .await;
        assert!(refused.is_error);
        assert!(
            text_of(&refused).starts_with(
                "Not changed: only a draft's subjects can be tested, and “Winter sale” is sent."
            ),
            "{}",
            text_of(&refused)
        );
    }
    assert!(stub
        .calls_to("PATCH", &format!("/create/email/campaigns/{CAMPAIGN}"))
        .is_empty());
}

#[test]
fn templates_and_ab_are_listed_described_and_in_the_schema() {
    for action in ["email_templates", "email_ab"] {
        assert!(EMAIL_ACTIONS.contains(&action), "{action}");
        assert!(VALID_ACTIONS.contains(&action), "{action}");
        assert!(TOOL_REFERENCE.contains(&format!("- {action}:")), "{action}");
    }
    let schema = input_schema();
    let properties = schema["properties"].as_object().unwrap();
    for field in ["template_id", "subjects", "split", "decide_after", "metric"] {
        assert!(properties.contains_key(field), "{field}");
    }
    let operations = properties["operation"]["enum"].as_array().unwrap();
    for operation in ["get", "save", "use", "show", "set", "clear"] {
        assert!(operations.contains(&json!(operation)), "{operation}");
    }
}

// ============================================================================
// email_pause, email_resume and email_connection
// ============================================================================

/// A campaign in flight: `sent` of 37 went, the rest wait. `newer` answers `remaining`
/// and takes `expectedRemaining`.
fn flight_api(status: &'static str, sent: i64, newer: bool) -> Responder {
    let now = Arc::new(Mutex::new(status.to_string()));
    let base = api(now.clone(), 37);
    Arc::new(move |request: &Recorded| {
        let path = format!("/create/email/campaigns/{CAMPAIGN}");
        let in_flight = |status: &str| {
            let mut campaign = campaign(status);
            let started = sent > 0;
            campaign["progress"] = json!({
                "recipients": if started { 40 } else { 0 }, "queued": if started { 37 - sent } else { 0 },
                "sent": sent, "failed": 0, "skipped": if started { 3 } else { 0 },
            });
            campaign["revision"] = json!(6);
            if !started {
                campaign["schedule"] =
                    json!({"sendAt": "2026-11-20T09:00:00Z", "timeZone": "Europe/London"});
            }
            if newer {
                campaign["remaining"] = json!(37 - sent);
            }
            campaign
        };
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", p) if p == path => Answer::ok(in_flight(&now.lock().unwrap())),
            ("POST", p) if p == format!("{path}/pause") => {
                let mut now = now.lock().unwrap();
                if *now == "paused" {
                    return Answer::error(
                        409,
                        "CONFLICT",
                        "Conflict: This campaign is already paused.",
                    );
                }
                *now = "paused".to_string();
                Answer::ok(in_flight("paused"))
            }
            ("POST", p) if p == format!("{path}/resume") => {
                if request.body.get("expectedRemaining").is_some() && !newer {
                    return unknown_field("expectedRemaining", "`workspaceId`");
                }
                if request.body.get("expectedRemaining") == Some(&json!(999)) {
                    return Answer::error(422, "email_recipients_changed", "This would now go to 25 people, not 999. Nothing was resumed; show who it reaches again and ask again.");
                }
                let mut now = now.lock().unwrap();
                *now = if sent > 0 { "sending" } else { "scheduled" }.to_string();
                Answer::ok(in_flight(&now))
            }
            _ => base(request),
        }
    })
}

#[tokio::test]
async fn a_send_in_flight_is_paused_and_says_how_many_went_and_how_many_wait() {
    let stub = Stub::start(flight_api("sending", 12, false)).await;
    let pause = format!("/create/email/campaigns/{CAMPAIGN}/pause");
    let paused = run(
        &stub,
        json!({"action": "email_pause", "campaign_id": CAMPAIGN}),
    )
    .await;
    let text = text_of(&paused);
    assert!(!paused.is_error, "{text}");
    let posts = stub.calls_to("POST", &pause);
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].body, json!({"workspaceId": WORKSPACE}));
    assert!(
        text.starts_with("Paused “Winter sale”: 12 sent, 25 still wait."),
        "{text}"
    );
    assert!(
        text.contains("email_resume sends to the rest, with the person's go"),
        "{text}"
    );
    let out = structured(&paused);
    assert_eq!(out["status"], "paused");
    assert_eq!(out["sent"], 12);
    assert_eq!(out["waiting"], 25);

    // Create's refusal is kept.
    let again = run(
        &stub,
        json!({"action": "email_pause", "campaign_id": CAMPAIGN}),
    )
    .await;
    assert_eq!(
        text_of(&again),
        "Not paused: This campaign is already paused."
    );

    // Paused before it started: nothing went.
    let scheduled = Stub::start(flight_api("scheduled", 0, true)).await;
    let paused = run(
        &scheduled,
        json!({"action": "email_pause", "campaign_id": CAMPAIGN}),
    )
    .await;
    assert!(
        text_of(&paused).starts_with(
            "Paused “Winter sale” before it started: nothing was sent, and 37 people wait."
        ),
        "{}",
        text_of(&paused)
    );

    assert!(matches!(
        stub.tool().execute(json!({"action": "email_pause"})).await,
        Err(Error::Validation(_))
    ));
}

#[tokio::test]
async fn a_paused_send_resumes_only_with_the_persons_go_for_who_still_waits() {
    let stub = Stub::start(flight_api("paused", 12, false)).await;
    let tool = stub.tool();
    let resume = format!("/create/email/campaigns/{CAMPAIGN}/resume");

    let unasked = tool
        .execute(json!({"action": "email_resume", "campaign_id": CAMPAIGN}))
        .await
        .unwrap();
    let text = text_of(&unasked);
    assert!(
        text.starts_with("Not resumed yet: the person's go is needed first. Show them: “Winter sale” (subject “Our winter sale starts today”) to the 25 people still waiting (12 of 37 sent so far)."),
        "{text}"
    );
    assert!(text.contains("confirm_remaining 25"), "{text}");
    assert_eq!(
        structured(&unasked)["confirm_with"]["confirm_remaining"],
        25
    );
    assert_eq!(structured(&unasked)["resumed"], false);

    let wrong = tool
        .execute(
            json!({"action": "email_resume", "campaign_id": CAMPAIGN, "confirm_remaining": 37}),
        )
        .await
        .unwrap();
    assert!(
        text_of(&wrong)
            .starts_with("Not resumed: 25 people still wait now, not the 37 the person agreed to."),
        "{}",
        text_of(&wrong)
    );
    assert!(stub.calls_to("POST", &resume).is_empty());

    let count_only = tool
        .execute(
            json!({"action": "email_resume", "campaign_id": CAMPAIGN, "confirm_remaining": 25}),
        )
        .await
        .unwrap();
    assert!(
        text_of(&count_only).starts_with("Not resumed yet:"),
        "{}",
        text_of(&count_only)
    );
    assert!(stub.calls_to("POST", &resume).is_empty());

    let resumed = tool
        .execute(structured(&unasked)["confirm_with"].clone())
        .await
        .unwrap();
    let text = text_of(&resumed);
    assert!(!resumed.is_error, "{text}");
    let posts = stub.calls_to("POST", &resume);
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].body, json!({"workspaceId": WORKSPACE}));
    assert_eq!(
        posts[0].header("idempotency-key"),
        Some(format!("email-resume-{CAMPAIGN}-r6").as_str())
    );
    assert!(
        text.starts_with("Resumed “Winter sale”: it goes on to the 25 people still waiting."),
        "{text}"
    );
    assert_eq!(structured(&resumed)["resumed"], true);

    // Not paused any more: nothing to resume, said plainly, and nothing is sent.
    let not_paused = tool
        .execute(
            json!({"action": "email_resume", "campaign_id": CAMPAIGN, "confirm_remaining": 25}),
        )
        .await
        .unwrap();
    assert!(not_paused.is_error);
    assert!(
        text_of(&not_paused).starts_with(
            "Not resumed: only a paused campaign can be resumed, and “Winter sale” is sending."
        ),
        "{}",
        text_of(&not_paused)
    );
    assert_eq!(stub.calls_to("POST", &resume).len(), 1);
}

#[tokio::test]
async fn a_resume_tells_a_newer_create_the_count_the_person_saw() {
    let stub = Stub::start(flight_api("paused", 12, true)).await;
    let resume = format!("/create/email/campaigns/{CAMPAIGN}/resume");
    let review = run(
        &stub,
        json!({"action": "email_resume", "campaign_id": CAMPAIGN}),
    )
    .await;
    let resumed = run(&stub, structured(&review)["confirm_with"].clone()).await;
    assert!(!resumed.is_error, "{}", text_of(&resumed));
    assert_eq!(
        stub.calls_to("POST", &resume)[0].body,
        json!({"workspaceId": WORKSPACE, "expectedRemaining": 25})
    );

    // Paused before it started: it goes back on its schedule, to who its audience holds now.
    let scheduled = Stub::start(flight_api("paused", 0, false)).await;
    let review = run(
        &scheduled,
        json!({"action": "email_resume", "campaign_id": CAMPAIGN}),
    )
    .await;
    assert!(
        text_of(&review).contains("Show them: “Winter sale” (subject “Our winter sale starts today”) back on its schedule (2026-11-20 09:00 UTC) to 37 people."),
        "{}",
        text_of(&review)
    );
    let resumed = run(&scheduled, structured(&review)["confirm_with"].clone()).await;
    assert!(
        text_of(&resumed).starts_with(
            "Resumed “Winter sale”: it is scheduled again, for 2026-11-20 09:00 UTC, to 37 people."
        ),
        "{}",
        text_of(&resumed)
    );
}

#[tokio::test]
async fn a_resume_asks_the_person_where_the_client_can() {
    let stub = Stub::start(flight_api("paused", 12, false)).await;
    let tool = stub.tool();
    let resume = format!("/create/email/campaigns/{CAMPAIGN}/resume");
    let unasked = tool
        .execute(json!({"action": "email_resume", "campaign_id": CAMPAIGN}))
        .await
        .unwrap();
    let confirmed = structured(&unasked)["confirm_with"].clone();

    // Where only the person's own yes counts, the agent's count resumes nothing.
    let asked_only = || mcp_types::config::ConfigOverride {
        go_ahead_asked_only: Some(true),
        ..Default::default()
    };
    let kept = mcp_client::run_with_config_override(asked_only(), || async {
        tool.execute(confirmed.clone()).await.unwrap()
    })
    .await;
    assert!(
        text_of(&kept).starts_with("Not resumed: on this connection a go-ahead counts only when the person is asked and says yes, and they could not be asked"),
        "{}",
        text_of(&kept)
    );

    // Asked live, their no wins over the agent's count.
    let client = Arc::new(FakeClient::default());
    *client.answer.lock().unwrap() = Some(json!({"action": "decline"}));
    let declined = run_with_call_peer(
        CallPeer::new(client.clone(), None, true),
        tool.execute(confirmed.clone()),
    )
    .await
    .unwrap();
    assert!(
        text_of(&declined).starts_with("Not resumed: the person didn't say go"),
        "{}",
        text_of(&declined)
    );
    let message = client.sent.lock().unwrap()[0].1["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        message.starts_with("Resume “Winter sale” (subject “Our winter sale starts today”) to the 25 people still waiting (12 of 37 sent so far)?"),
        "{message}"
    );
    assert!(stub.calls_to("POST", &resume).is_empty());

    // A stateless call without the count asks through its result, bound to this pause.
    let peer =
        CallPeer::new(Arc::new(FakeClient::default()), None, false).with_inline_input(&json!({}));
    let asking = run_with_call_peer(
        peer.clone(),
        tool.execute(json!({"action": "email_resume", "campaign_id": CAMPAIGN})),
    )
    .await
    .unwrap();
    assert!(text_of(&asking).starts_with("Asking the person before"));
    assert_eq!(
        peer.take_input_required().unwrap()["requestState"],
        format!("studio-email-resume:{CAMPAIGN}:6:25")
    );

    *client.answer.lock().unwrap() =
        Some(json!({"action": "accept", "content": {"confirm": true}}));
    let resumed = mcp_client::run_with_config_override(asked_only(), || {
        run_with_call_peer(
            CallPeer::new(client.clone(), None, true),
            tool.execute(json!({"action": "email_resume", "campaign_id": CAMPAIGN})),
        )
    })
    .await
    .unwrap();
    assert!(!resumed.is_error, "{}", text_of(&resumed));
    assert!(text_of(&resumed).contains("The person said go when the client asked them."));
    assert_eq!(stub.calls_to("POST", &resume).len(), 1);
}

#[tokio::test]
async fn a_send_that_is_paused_points_at_resume() {
    let stub = Stub::start(flight_api("paused", 12, false)).await;
    let refused = run(
        &stub,
        json!({"action": "email_send", "campaign_id": CAMPAIGN, "confirm_recipients": 37}),
    )
    .await;
    assert_eq!(
        text_of(&refused),
        "Not sent: “Winter sale” is paused. email_resume sends to the people still waiting, with the person's go; email_cancel stops it for good."
    );
}

fn connection_answer() -> Value {
    json!({
        "connected": true,
        "key": {"last4": "Qa7x", "access": "full_access", "verifiedAt": "2026-10-29T18:00:00Z"},
        "domains": [
            {"id": "d1", "name": "news.northwind.example", "status": "verified", "region": "us-east-1",
             "sending": true, "openTracking": true, "clickTracking": false, "records": []},
            {"id": "d2", "name": "shop.northwind.example", "status": "pending", "region": "us-east-1",
             "sending": true, "openTracking": false, "clickTracking": false, "records": []},
        ],
        "domainsReadAt": "2026-11-18T10:00:00Z",
        "sender": {"fromName": "Northwind", "fromEmail": "hello@news.northwind.example", "replyTo": "team@northwind.example",
                   "companyName": "Northwind Coffee Ltd", "postalAddress": null},
        "webhook": {"mode": "auto", "url": format!("https://api.contextstream.io/api/v1/create/email/webhooks/{WORKSPACE}"),
                    "secretSet": true, "lastEventAt": "2026-11-17T09:05:00Z"},
        "ready": false, "missing": ["postal_address"], "revision": 3,
    })
}

#[tokio::test]
async fn the_connection_is_shown_and_refreshed_and_never_a_key() {
    let stub = Stub::start(over(|request| {
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/create/email/connection") => Some(Answer::ok(connection_answer())),
            ("POST", "/create/email/connection/refresh") => {
                let mut refreshed = connection_answer();
                refreshed["domains"][1]["status"] = json!("verified");
                Some(Answer::ok(refreshed))
            }
            _ => None,
        }
    }))
    .await;
    let shown = run(&stub, json!({"action": "email_connection"})).await;
    let text = text_of(&shown);
    assert!(!shown.is_error, "{text}");
    assert!(
        text.starts_with("Resend is connected (key ending Qa7x), but campaigns can't be sent until the studio has a postal address for the footer: https://ctxproduct.com/studio/email/settings."),
        "{text}"
    );
    assert!(
        text.contains("- Sender: Northwind <hello@news.northwind.example>, replies to team@northwind.example."),
        "{text}"
    );
    assert!(
        text.contains("- Domains: news.northwind.example verified (opens tracked, clicks not tracked); shop.northwind.example pending."),
        "{text}"
    );
    assert!(
        text.contains("- Webhook: set up by Create, with its signing secret; last event on 2026-11-17 09:05 UTC."),
        "{text}"
    );
    assert!(!text.contains("api.contextstream.io"), "{text}");
    let out = structured(&shown);
    assert_eq!(out["connected"], true);
    assert_eq!(out["ready"], false);
    assert_eq!(out["domains"][1]["status"], "pending");
    assert!(
        out.get("key").is_none() && !out.to_string().contains("re_"),
        "{out}"
    );
    assert!(stub.requests().iter().all(|r| r.method == "GET"), "a read");

    let refreshed = run(
        &stub,
        json!({"action": "email_connection", "operation": "refresh"}),
    )
    .await;
    let text = text_of(&refreshed);
    assert!(!refreshed.is_error, "{text}");
    assert_eq!(
        stub.calls_to("POST", "/create/email/connection/refresh")[0].body,
        json!({"workspaceId": WORKSPACE})
    );
    assert!(
        text.starts_with("Asked Resend again.\nResend is connected"),
        "{text}"
    );
    assert!(text.contains("shop.northwind.example verified"), "{text}");

    // A key in the call is refused unread, as everywhere.
    let keyed = run(
        &stub,
        json!({"action": "email_connection", "operation": "refresh", "note": "re_Ab12Cd34_EfGh5678IjKl9012MnOp3456"}),
    )
    .await;
    assert_eq!(text_of(&keyed), KEY_REFUSED);
    assert_eq!(
        stub.calls_to("POST", "/create/email/connection/refresh")
            .len(),
        1
    );
}

#[tokio::test]
async fn a_connection_that_isnt_made_is_made_in_the_studio() {
    let stub = Stub::start(over(|request| {
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/create/email/connection") => Some(Answer::ok(json!({"connected": false}))),
            ("POST", "/create/email/connection/refresh") => Some(Answer::error(
                404,
                "NOT_FOUND",
                "Not found: This workspace hasn't connected Resend yet.",
            )),
            _ => None,
        }
    }))
    .await;
    let shown = run(&stub, json!({"action": "email_connection"})).await;
    assert!(
        text_of(&shown).starts_with(&format!("Resend isn't connected yet: the team connects its own Resend key in the studio ({CONNECT_URL}).")),
        "{}",
        text_of(&shown)
    );
    assert_eq!(structured(&shown)["connected"], false);
    let refreshed = run(
        &stub,
        json!({"action": "email_connection", "operation": "refresh"}),
    )
    .await;
    assert!(refreshed.is_error);
    assert_eq!(
        text_of(&refreshed),
        format!("Not refreshed: Resend isn't connected yet. The team connects it in the studio: {CONNECT_URL}")
    );
}

#[test]
fn pause_resume_and_the_connection_are_listed_described_and_in_the_schema() {
    for action in ["email_pause", "email_resume", "email_connection"] {
        assert!(EMAIL_ACTIONS.contains(&action), "{action}");
        assert!(VALID_ACTIONS.contains(&action), "{action}");
        assert!(TOOL_REFERENCE.contains(&format!("- {action}:")), "{action}");
    }
    let schema = input_schema();
    let properties = schema["properties"].as_object().unwrap();
    assert!(properties.contains_key("confirm_remaining"));
    assert!(properties["operation"]["enum"]
        .as_array()
        .unwrap()
        .contains(&json!("refresh")));
    assert!(!properties.keys().any(|k| k.contains("key")));
}

// ============================================================================
// email_send: the review, and a go for exactly what the person saw
// ============================================================================

/// The campaign as a draft that goes to `audience` (id, name, purpose) and maybe a segment.
/// `newer` is a Create that answers `remaining` and takes `expectedRecipients`.
fn send_api(
    audience_of: (&'static str, &'static str, Option<&'static str>),
    segment_of: Option<(&'static str, &'static str)>,
    name: &'static str,
    recipients: i64,
    newer: bool,
) -> Responder {
    let base = api(Arc::new(Mutex::new("draft".to_string())), recipients);
    Arc::new(move |request: &Recorded| {
        let path = format!("/create/email/campaigns/{CAMPAIGN}");
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", p) if p == path => {
                let (id, audience_name, purpose) = audience_of;
                let mut campaign = campaign("draft");
                campaign["name"] = json!(name);
                campaign["audienceId"] = json!(id);
                campaign["audience"] = json!({"id": id, "name": audience_name});
                if let Some((segment_id, segment_name)) = segment_of {
                    campaign["segmentId"] = json!(segment_id);
                    campaign["segment"] = json!({"id": segment_id, "name": segment_name});
                }
                if newer {
                    campaign["audience"]["purpose"] = json!(purpose);
                    campaign["remaining"] = json!(recipients);
                }
                Answer::ok(campaign)
            }
            ("GET", "/create/email/audiences") => Answer::ok(json!([
                audience(AUDIENCE, "Coffee club", Some("production"), 40, 37),
                audience(PROOF, "Proof", Some("test"), 3, 3),
            ])),
            ("POST", p) if p == format!("{path}/schedule") => {
                if request.body.get("expectedRecipients").is_some() && !newer {
                    return unknown_field(
                        "expectedRecipients",
                        "`workspaceId`, `sendAt`, `timeZone`, `revision`",
                    );
                }
                if request.body.get("expectedRecipients") == Some(&json!(999)) {
                    return Answer::error(422, "email_recipients_changed", "This would now go to 12 people, not 999. Nothing was scheduled; show who it reaches again and ask again.");
                }
                base(request)
            }
            _ => base(request),
        }
    })
}

#[tokio::test]
async fn the_send_review_names_the_audience_its_purpose_and_what_to_check() {
    // A test audience named for a real send.
    let stub = Stub::start(send_api(
        (PROOF, "Proof", Some("test")),
        None,
        "Winter sale",
        3,
        true,
    ))
    .await;
    let review = run(
        &stub,
        json!({"action": "email_send", "campaign_id": CAMPAIGN}),
    )
    .await;
    let text = text_of(&review);
    assert!(
        text.starts_with("Not sent yet: the person's go is needed first. Show them: “Winter sale” (subject “Our winter sale starts today”, from Northwind <hello@news.northwind.example>) to 3 people in the audience “Proof” (test) now."),
        "{text}"
    );
    assert!(
        text.contains(
            "Check: “Proof” is a test audience: this is a real send to the 3 people in it."
        ),
        "{text}"
    );
    assert!(
        text.contains("Check: The image “Beans in the snow” has no link."),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "call email_send again with campaign_id {CAMPAIGN}, send_at now, time_zone UTC, audience_id {PROOF}, confirm_recipients 3 and confirm_review send1.4.3."
        )),
        "{text}"
    );
    let out = structured(&review);
    assert_eq!(out["audience"]["purpose"], "test");
    assert_eq!(out["confirm_with"]["audience_id"], PROOF);
    assert_eq!(out["warnings"].as_array().unwrap().len(), 2);
    // Its purpose came with the campaign: the audiences aren't read again.
    assert!(stub.calls_to("GET", "/create/email/audiences").is_empty());

    // A production audience for what reads as a test. Today's main: the purpose is the
    // audience list's.
    let stub = Stub::start(send_api(
        (AUDIENCE, "Coffee club", Some("production")),
        None,
        "Layout test",
        37,
        false,
    ))
    .await;
    let text = text_of(
        &run(
            &stub,
            json!({"action": "email_send", "campaign_id": CAMPAIGN}),
        )
        .await,
    );
    assert!(
        text.contains("to 37 people in the audience “Coffee club” (production) now."),
        "{text}"
    );
    assert!(
        text.contains("Check: this reads as a test (its name or subject says “test”), and “Coffee club” is a production audience: 37 people would get it."),
        "{text}"
    );
    assert_eq!(stub.calls_to("GET", "/create/email/audiences").len(), 1);

    // A segment is named with its audience, and the person is asked the same words.
    let stub = Stub::start(send_api(
        (AUDIENCE, "Coffee club", Some("production")),
        Some((SEGMENT, "Regulars")),
        "Winter sale",
        10,
        true,
    ))
    .await;
    let client = Arc::new(FakeClient::default());
    *client.answer.lock().unwrap() = Some(json!({"action": "decline"}));
    run_with_call_peer(
        CallPeer::new(client.clone(), None, true),
        stub.tool()
            .execute(json!({"action": "email_send", "campaign_id": CAMPAIGN})),
    )
    .await
    .unwrap();
    let message = client.sent.lock().unwrap()[0].1["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        message.starts_with("Send “Winter sale” (subject “Our winter sale starts today”, from Northwind <hello@news.northwind.example>) to 10 people in the segment “Regulars” of the audience “Coffee club” (production) now?"),
        "{message}"
    );
    assert!(
        message.contains("Check: The image “Beans in the snow” has no link."),
        "{message}"
    );
    assert!(
        message.ends_with("Once it goes, it can't be unsent."),
        "{message}"
    );
    let peer =
        CallPeer::new(Arc::new(FakeClient::default()), None, false).with_inline_input(&json!({}));
    run_with_call_peer(
        peer.clone(),
        stub.tool()
            .execute(json!({"action": "email_send", "campaign_id": CAMPAIGN})),
    )
    .await
    .unwrap();
    assert_eq!(
        peer.take_input_required().unwrap()["requestState"],
        format!("studio-email-send:{CAMPAIGN}:4:10:now:UTC:{AUDIENCE}:{SEGMENT}")
    );
}

#[tokio::test]
async fn a_go_is_for_the_audience_the_person_saw() {
    let stub = Stub::start(send_api(
        (AUDIENCE, "Coffee club", Some("production")),
        None,
        "Winter sale",
        37,
        false,
    ))
    .await;
    let tool = stub.tool();
    let schedule = format!("/create/email/campaigns/{CAMPAIGN}/schedule");

    // The count is right, the audience is not the one the person agreed to: nothing goes.
    let other = tool
        .execute(json!({"action": "email_send", "campaign_id": CAMPAIGN, "audience_id": PROOF, "confirm_recipients": 37}))
        .await
        .unwrap();
    let text = text_of(&other);
    assert!(
        text.starts_with("Not sent: “Winter sale” now goes to the audience “Coffee club” (production), not the audience the person agreed to."),
        "{text}"
    );
    assert!(text.contains(&format!("audience_id {AUDIENCE}")), "{text}");
    let segmented = tool
        .execute(json!({"action": "email_send", "campaign_id": CAMPAIGN, "audience_id": AUDIENCE, "segment_id": SEGMENT, "confirm_recipients": 37}))
        .await
        .unwrap();
    assert!(
        text_of(&segmented).starts_with("Not sent: “Winter sale” now goes to the audience “Coffee club” (production), not the segment the person agreed to."),
        "{}",
        text_of(&segmented)
    );
    assert!(stub.calls_to("POST", &schedule).is_empty());

    // The audience they saw, and its review token: sent. Today's main is told no count.
    let sent = tool
        .execute(structured(&other)["confirm_with"].clone())
        .await
        .unwrap();
    assert!(!sent.is_error, "{}", text_of(&sent));
    let posts = stub.calls_to("POST", &schedule);
    assert_eq!(posts.len(), 1);
    assert!(posts[0].body.get("expectedRecipients").is_none());
    assert!(
        text_of(&sent).starts_with(
            "Sending “Winter sale” to 37 people in the audience “Coffee club” (production) now"
        ),
        "{}",
        text_of(&sent)
    );
}

#[tokio::test]
async fn a_go_asked_for_one_audience_isnt_taken_for_another() {
    let now = Arc::new(Mutex::new(AUDIENCE));
    let current = now.clone();
    let base = usual();
    let stub = Stub::start(Arc::new(move |request: &Recorded| {
        if request.method == "GET" && request.path == format!("/create/email/campaigns/{CAMPAIGN}")
        {
            let id = *current.lock().unwrap();
            let mut campaign = campaign("draft");
            campaign["audienceId"] = json!(id);
            campaign["audience"] =
                json!({"id": id, "name": if id == AUDIENCE { "Coffee club" } else { "Proof" }});
            return Answer::ok(campaign);
        }
        base(request)
    }))
    .await;
    let tool = stub.tool();
    let client = Arc::new(FakeClient::default());
    let first = CallPeer::new(client.clone(), None, false).with_inline_input(&json!({}));
    run_with_call_peer(
        first.clone(),
        tool.execute(json!({"action": "email_send", "campaign_id": CAMPAIGN})),
    )
    .await
    .unwrap();
    let question = first.take_input_required().unwrap();
    let state = question["requestState"].as_str().unwrap().to_string();

    // The campaign is pointed at another audience of the same size while the person is
    // asked: their yes was for the first one.
    *now.lock().unwrap() = PROOF;
    let stale = CallPeer::new(client, None, false).with_inline_input(&json!({
        "inputResponses": {(question["inputRequests"].as_object().unwrap().keys().next().unwrap()): {"action": "accept", "content": {"confirm": true}}},
        "requestState": state,
    }));
    let result = run_with_call_peer(
        stale.clone(),
        tool.execute(json!({"action": "email_send", "campaign_id": CAMPAIGN})),
    )
    .await
    .unwrap();
    assert!(
        text_of(&result).starts_with("Asking the person"),
        "{}",
        text_of(&result)
    );
    assert_eq!(
        stale.take_input_required().unwrap()["requestState"],
        format!("studio-email-send:{CAMPAIGN}:4:37:now:UTC:{PROOF}:-")
    );
    assert!(stub
        .calls_to(
            "POST",
            &format!("/create/email/campaigns/{CAMPAIGN}/schedule")
        )
        .is_empty());
}

#[tokio::test]
async fn a_send_tells_a_newer_create_the_count_the_person_saw() {
    let stub = Stub::start(send_api(
        (AUDIENCE, "Coffee club", Some("production")),
        None,
        "Winter sale",
        37,
        true,
    ))
    .await;
    let schedule = format!("/create/email/campaigns/{CAMPAIGN}/schedule");
    let review = run(
        &stub,
        json!({"action": "email_send", "campaign_id": CAMPAIGN}),
    )
    .await;
    let sent = run(&stub, structured(&review)["confirm_with"].clone()).await;
    assert!(!sent.is_error, "{}", text_of(&sent));
    let posts = stub.calls_to("POST", &schedule);
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].body["expectedRecipients"], 37);
    assert_eq!(posts[0].body["revision"], 4);

    // Create's own refusal of a count that changed is kept, with what to do next.
    let changed = Stub::start(send_api(
        (AUDIENCE, "Coffee club", Some("production")),
        None,
        "Winter sale",
        999,
        true,
    ))
    .await;
    let changed_review = run(
        &changed,
        json!({"action": "email_send", "campaign_id": CAMPAIGN}),
    )
    .await;
    let refused = run(
        &changed,
        structured(&changed_review)["confirm_with"].clone(),
    )
    .await;
    assert_eq!(
        text_of(&refused),
        "Not sent: This would now go to 12 people, not 999. Nothing was scheduled; show who it reaches again and ask again."
    );
}

#[tokio::test]
async fn a_send_without_a_recipient_count_cannot_be_confirmed_as_zero() {
    let stub = Stub::start(over(|r| {
        r.path.ends_with("/preview").then(|| {
            Answer::ok(json!({"subject": "Sale", "from": "hello@example.com", "warnings": []}))
        })
    }))
    .await;
    let result = run(
        &stub,
        json!({"action":"email_send","campaign_id":CAMPAIGN,"confirm_recipients":0}),
    )
    .await;
    assert!(text_of(&result).contains("not answered"));
    assert!(stub
        .calls_to(
            "POST",
            &format!("/create/email/campaigns/{CAMPAIGN}/schedule")
        )
        .is_empty());
}

#[tokio::test]
async fn only_the_delete_tool_erases_audiences_segments_and_templates() {
    let stub = Stub::start(over(|request| {
        (request.method == "DELETE").then(|| {
            if request.path.ends_with("builtin:announcement") {
                Answer::error(
                    409,
                    "CONFLICT",
                    "A built-in template can't be changed; save a copy.",
                )
            } else {
                Answer::ok(json!({"id": request.path.rsplit('/').next(), "deleted": true}))
            }
        })
    }))
    .await;
    let mut registry = crate::registry::ToolRegistry::new(&TestFixtures::test_config());
    registry.register(TOOL_NAME, Arc::new(stub.tool()));
    crate::domains::destructive::register_delete_tool(&mut registry);
    assert!(
        registry.get("delete").is_none(),
        "a studio-only connector has no delete tool"
    );
    registry.register("workspace", Arc::new(Bystander::workspace()));
    crate::domains::destructive::register_delete_tool(&mut registry);
    let schema = &registry.get("delete").unwrap().input_schema["properties"];
    for (kind, list, id) in [
        ("audience", "audiences", AUDIENCE),
        ("segment", "segments", SEGMENT),
        ("template", "templates", TEMPLATE),
    ] {
        let erase = format!("email_{kind}_erase");
        let action = format!("studio.{erase}");
        let field = format!("{kind}_id");
        assert!(
            schema["action"]["enum"]
                .as_array()
                .unwrap()
                .contains(&json!(action)),
            "{action}"
        );
        assert!(schema[&field].is_object());
        let request = json!({"action":action,&field:id});
        let before = stub.requests().len();
        for spelling in ["erase", " Delete ", "remove"] {
            let refusal = registry
                .execute(
                    TOOL_NAME,
                    json!({"action": format!("email_{list}"),"operation":spelling,&field:id}),
                )
                .await
                .unwrap_err();
            assert!(
                refusal.to_string().contains(&action)
                    && refusal
                        .to_string()
                        .contains("https://ctxproduct.com/studio/email"),
                "{refusal}"
            );
        }
        assert!(registry
            .execute(TOOL_NAME, json!({"action":erase,&field:id}))
            .await
            .is_err());
        let refused = mcp_client::run_with_config_override(
            mcp_types::config::ConfigOverride {
                go_ahead_asked_only: Some(true),
                ..Default::default()
            },
            || registry.execute("delete", request.clone()),
        )
        .await
        .unwrap();
        assert!(refused.is_error);
        assert_eq!(before, stub.requests().len());
        let erased = registry.execute("delete", request).await.unwrap();
        assert!(!erased.is_error, "{}", text_of(&erased));
        assert_eq!(structured(&erased)[&field], id);
        let calls = stub.calls_to("DELETE", &format!("/create/email/{list}/{id}"));
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].query, format!("workspaceId={WORKSPACE}"));
    }
    let builtin = registry
        .execute(
            "delete",
            json!({"action":"studio.email_template_erase","template_id":"builtin:announcement"}),
        )
        .await
        .unwrap();
    assert!(builtin.is_error && text_of(&builtin).contains("built-in"));
}

#[tokio::test]
async fn a_send_rechecks_the_reviewed_sender_and_count_before_scheduling() {
    let changed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let current = changed.clone();
    let stub = Stub::start(over(move |request| {
        if request.path.ends_with("/preview") && current.load(std::sync::atomic::Ordering::Relaxed)
        {
            let mut response = (usual())(request);
            response.body["data"]["from"] = json!("Other <other@northwind.example>");
            Some(response)
        } else {
            None
        }
    }))
    .await;
    let first = run(&stub, json!({"action":"email_send","campaign_id":CAMPAIGN})).await;
    let confirmation = structured(&first)["confirm_with"].clone();
    assert!(confirmation["confirm_review"].is_string());
    let client = Arc::new(FakeClient::default());
    let call = json!({"action":"email_send","campaign_id":CAMPAIGN});
    let peer = CallPeer::new(client.clone(), None, false).with_inline_input(&json!({}));
    run_with_call_peer(peer.clone(), stub.tool().execute(call.clone()))
        .await
        .unwrap();
    let question = peer.take_input_required().unwrap();
    let request_id = question["inputRequests"]
        .as_object()
        .unwrap()
        .keys()
        .next()
        .unwrap();

    changed.store(true, std::sync::atomic::Ordering::Relaxed);
    let peer = CallPeer::new(client, None, false).with_inline_input(&json!({
        "requestState":question["requestState"],
        "inputResponses":{request_id:{"action":"accept","content":{"confirm":true}}}
    }));
    run_with_call_peer(peer.clone(), stub.tool().execute(call))
        .await
        .unwrap();
    let fresh = peer
        .take_input_required()
        .expect("sender changed: ask again");
    assert!(fresh["inputRequests"].get(request_id).is_none());

    let result = run(&stub, confirmation).await;
    assert!(text_of(&result).contains("Other <other@northwind.example>"));
    let schedule = format!("/create/email/campaigns/{CAMPAIGN}/schedule");
    assert!(stub.calls_to("POST", &schedule).is_empty());

    // On a legacy API even the live question can outlast the count it showed.
    let previews = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let stub = Stub::start(over(move |request| {
        if !request.path.ends_with("/preview") {
            return None;
        }
        let mut response = (usual())(request);
        if previews.fetch_add(1, std::sync::atomic::Ordering::Relaxed) > 0 {
            response.body["data"]["recipients"] = json!(36);
        }
        Some(response)
    }))
    .await;
    let client = Arc::new(FakeClient::default());
    *client.answer.lock().unwrap() = Some(json!({"action":"accept","content":{"confirm":true}}));
    let result = run_with_call_peer(
        CallPeer::new(client, None, true),
        stub.tool()
            .execute(json!({"action":"email_send","campaign_id":CAMPAIGN})),
    )
    .await
    .unwrap();
    assert!(text_of(&result).contains("again"));
    assert!(stub.calls_to("POST", &schedule).is_empty());
}

#[tokio::test]
async fn unanswered_live_email_reads_are_not_empty_lists_or_disconnected_accounts() {
    let stub = Stub::start(over(|r| match r.path.as_str() {
        "/create/email/audiences" => Some(Answer::ok(Value::Null)),
        "/create/email/connection" => Some(Answer::ok(json!({}))),
        "/create/email/segments" => Some(Answer::ok(Value::Null)),
        "/create/email/templates" => Some(Answer::ok(json!({}))),
        _ => None,
    }))
    .await;
    for action in [
        "email_audiences",
        "email_connection",
        "email_segments",
        "email_templates",
    ] {
        let result = run(&stub, json!({"action":action})).await;
        assert!(text_of(&result).contains("not answered"), "{action}");
    }
}

#[tokio::test]
async fn email_api_responses_never_expose_a_key_in_free_text() {
    let secret = format!("re_{}", "x".repeat(32));
    let value = secret.clone();
    let stub = Stub::start(over(move |r| {
        (r.path == "/create/email/audiences")
            .then(|| Answer::ok(json!([audience(AUDIENCE, &value, Some("test"), 1, 1)])))
    }))
    .await;
    let result = stub
        .tool()
        .execute(json!({"action":"email_audiences"}))
        .await;
    match result {
        Ok(result) => assert!(
            !text_of(&result).contains(&secret)
                && !structured(&result).to_string().contains(&secret)
        ),
        Err(error) => assert!(!error.to_string().contains(&secret)),
    }
}

/// Runs `call` across a stateless client, a live client that never answers, and no client.
async fn across_non_answering_callers(stub: &Stub, call: Value) -> [ToolResult; 3] {
    let tool = stub.tool();
    let stateless =
        CallPeer::new(Arc::new(FakeClient::default()), None, false).with_inline_input(&json!({}));
    let on_stateless = tokio::time::timeout(
        Duration::from_secs(5),
        run_with_call_peer(stateless.clone(), tool.execute(call.clone())),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        stateless.take_input_required().is_none(),
        "a count-only call answers the review instead of hanging a stateless client"
    );

    let silent = Arc::new(FakeClient::default());
    let on_unanswered_live = tokio::time::timeout(
        Duration::from_secs(5),
        run_with_call_peer(
            CallPeer::new(silent.clone(), None, true),
            tool.execute(call.clone()),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        silent.sent.lock().unwrap().len(),
        1,
        "a live client is still asked first"
    );

    let on_no_client = tokio::time::timeout(Duration::from_secs(5), tool.execute(call))
        .await
        .unwrap()
        .unwrap();

    [on_stateless, on_unanswered_live, on_no_client]
}

#[tokio::test]
async fn count_only_send_never_sends_an_edited_campaign_at_the_same_count() {
    let rev = Arc::new(std::sync::atomic::AtomicI64::new(4));
    let rev_seen = rev.clone();
    let base = send_api((PROOF, "Proof", Some("test")), None, "Winter sale", 3, true);
    let stub = Stub::start(Arc::new(move |request: &Recorded| {
        let path = format!("/create/email/campaigns/{CAMPAIGN}");
        let r = rev_seen.load(std::sync::atomic::Ordering::Relaxed);
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", p) if p == path => {
                let mut answer = base(request);
                answer.body["data"]["revision"] = json!(r);
                if r == 5 {
                    answer.body["data"]["subject"] = json!("Different offer altogether");
                }
                answer
            }
            ("GET", p) if p == format!("{path}/preview") => {
                let mut answer = base(request);
                if r == 5 {
                    answer.body["data"]["subject"] = json!("Different offer altogether");
                    answer.body["data"]["from"] = json!("Other <other@news.northwind.example>");
                }
                answer
            }
            _ => base(request),
        }
    }))
    .await;
    let schedule = format!("/create/email/campaigns/{CAMPAIGN}/schedule");

    // Person reviews rev 4 scheduled for 2026-11-20T09:00 Europe/London.
    let reviewed = run(
        &stub,
        json!({
            "action": "email_send", "campaign_id": CAMPAIGN,
            "send_at": "2026-11-20T09:00", "time_zone": "Europe/London",
        }),
    )
    .await;
    let stale_confirm = structured(&reviewed)["confirm_with"].clone();
    assert!(stale_confirm["confirm_review"].is_string());

    // Campaign is edited to rev 5 (same 3 recipients) and agent calls with confirm_recipients=3 alone.
    rev.store(5, std::sync::atomic::Ordering::Relaxed);
    let count_only = json!({
        "action": "email_send", "campaign_id": CAMPAIGN, "confirm_recipients": 3,
    });
    for (mode, result) in ["stateless", "live_unanswered", "no_client"]
        .into_iter()
        .zip(across_non_answering_callers(&stub, count_only).await)
    {
        let text = text_of(&result);
        assert!(
            text.starts_with("Not sent yet:"),
            "{mode}: expected Not sent yet, got: {text}"
        );
        assert!(
            text.contains("Different offer altogether"),
            "{mode}: {text}"
        );
        assert!(text.contains("confirm_review"), "{mode}: {text}");
        assert_eq!(structured(&result)["sent"], false, "{mode}");
        assert!(
            structured(&result)["confirm_with"]["confirm_review"].is_string(),
            "{mode}"
        );
    }
    assert!(stub.calls_to("POST", &schedule).is_empty());

    // Passing the rev 4 token refuses and shows the rev 5 review.
    let stale = run(&stub, stale_confirm).await;
    assert!(
        text_of(&stale).starts_with("Not sent: the campaign changed since review."),
        "{}",
        text_of(&stale)
    );
    assert!(stub.calls_to("POST", &schedule).is_empty());

    // Approving with the rev 5 confirm_with dispatches using the token's revision and count.
    let fresh_confirm = structured(&stale)["confirm_with"].clone();
    let sent = run(&stub, fresh_confirm).await;
    assert!(!sent.is_error, "{}", text_of(&sent));
    let posts = stub.calls_to("POST", &schedule);
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].body["revision"], 5);
    assert_eq!(posts[0].body["expectedRecipients"], 3);
}

#[tokio::test]
async fn count_only_send_never_sends_to_another_audience_of_the_same_size() {
    let target = Arc::new(Mutex::new((PROOF, "Proof", "test")));
    let target_seen = target.clone();
    let base = send_api((PROOF, "Proof", Some("test")), None, "Winter sale", 3, true);
    let stub = Stub::start(Arc::new(move |request: &Recorded| {
        let path = format!("/create/email/campaigns/{CAMPAIGN}");
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", p) if p == path => {
                let (id, name, purpose) = *target_seen.lock().unwrap();
                let mut answer = base(request);
                answer.body["data"]["audienceId"] = json!(id);
                answer.body["data"]["audience"] =
                    json!({"id": id, "name": name, "purpose": purpose});
                answer.body["data"]["revision"] = json!(if id == PROOF { 4 } else { 5 });
                answer
            }
            ("GET", "/create/email/audiences") => Answer::ok(json!([
                audience(AUDIENCE, "VIPs", Some("production"), 3, 3),
                audience(PROOF, "Proof", Some("test"), 3, 3),
            ])),
            _ => base(request),
        }
    }))
    .await;
    let schedule = format!("/create/email/campaigns/{CAMPAIGN}/schedule");

    let reviewed = run(
        &stub,
        json!({"action": "email_send", "campaign_id": CAMPAIGN}),
    )
    .await;
    assert!(text_of(&reviewed).contains("“Proof” (test)"));
    let proof_token = structured(&reviewed)["confirm_with"]["confirm_review"]
        .as_str()
        .unwrap()
        .to_string();

    // Campaign moves from test audience "Proof" (3) to production audience "VIPs" (also 3).
    *target.lock().unwrap() = (AUDIENCE, "VIPs", "production");
    let count_only = json!({
        "action": "email_send", "campaign_id": CAMPAIGN, "confirm_recipients": 3,
    });
    for (mode, result) in ["stateless", "live_unanswered", "no_client"]
        .into_iter()
        .zip(across_non_answering_callers(&stub, count_only).await)
    {
        let text = text_of(&result);
        assert!(
            text.starts_with("Not sent yet:"),
            "{mode}: expected Not sent yet, got: {text}"
        );
        assert!(text.contains("“VIPs” (production)"), "{mode}: {text}");
        assert!(text.contains("confirm_review"), "{mode}: {text}");
    }
    let with_stale_token = run(
        &stub,
        json!({
            "action": "email_send", "campaign_id": CAMPAIGN,
            "confirm_recipients": 3, "confirm_review": proof_token,
        }),
    )
    .await;
    assert!(text_of(&with_stale_token).starts_with("Not sent:"));
    assert!(text_of(&with_stale_token).contains("“VIPs” (production)"));
    assert!(stub.calls_to("POST", &schedule).is_empty());
}

#[tokio::test]
async fn count_only_import_never_imports_a_changed_csv_at_the_same_eligible_count() {
    let stub = Stub::start(over(|request| {
        match (request.method.as_str(), request.path.as_str()) {
            ("POST", "/create/email/contacts/import/preview") => {
                let csv = request.body["csv"].as_str().unwrap_or("");
                let digest = if csv.contains("ada@example.com") {
                    "digest-ada-bo"
                } else {
                    "digest-dee-eve"
                };
                Some(Answer::ok(json!({
                    "columns": [{"index": 0, "header": "email"}],
                    "mapping": {"email": 0, "firstName": null, "lastName": null, "name": null, "tags": null, "attributes": {}},
                    "rows": 3, "valid": 3, "invalidCount": 0, "invalid": [],
                    "duplicatesInFile": 0, "existing": 0, "new": 3, "suppressed": 1,
                    "eligible": 2, "digest": digest,
                    "sample": [{"email": if csv.contains("ada@example.com") { "ada@example.com" } else { "dee@example.com" }}],
                })))
            }
            ("POST", "/create/email/contacts/import") => Some(Answer::ok(json!({
                "created": 2, "updated": 0, "added": 2, "invalidCount": 0,
                "duplicatesInFile": 0, "suppressed": 1, "eligible": 2,
            }))),
            _ => None,
        }
    }))
    .await;
    let folder = tempfile::tempdir().unwrap();
    let leads = folder.path().join("leads.csv");
    std::fs::write(
        &leads,
        "email\nada@example.com\nbo@example.com\ncy@example.com\n",
    )
    .unwrap();

    let preview = run(
        &stub,
        json!({
            "action": "email_contacts", "operation": "import_preview",
            "csv_path": leads.to_str().unwrap(), "audience_id": AUDIENCE,
            "consent_source": "form", "consent_note": NOTE,
        }),
    )
    .await;
    let first_token = structured(&preview)["confirm_with"]["confirm_review"]
        .as_str()
        .unwrap()
        .to_string();

    // Overwrite leads.csv with different people, keeping eligible == 2.
    std::fs::write(
        &leads,
        "email\ndee@example.com\neve@example.com\ncy@example.com\n",
    )
    .unwrap();
    let count_only = json!({
        "action": "email_contacts", "operation": "import",
        "csv_path": leads.to_str().unwrap(), "audience_id": AUDIENCE,
        "consent_source": "form", "consent_note": NOTE, "confirm_rows": 2,
    });
    for (mode, result) in ["stateless", "live_unanswered", "no_client"]
        .into_iter()
        .zip(across_non_answering_callers(&stub, count_only.clone()).await)
    {
        let text = text_of(&result);
        assert!(
            text.starts_with("Not imported yet:"),
            "{mode}: expected Not imported yet, got: {text}"
        );
        assert!(text.contains("confirm_review"), "{mode}: {text}");
        assert_eq!(structured(&result)["imported"], false, "{mode}");
    }
    assert!(stub
        .calls_to("POST", "/create/email/contacts/import")
        .is_empty());

    // Stale preview token refuses and shows the new review.
    let mut stale = count_only.clone();
    stale["confirm_review"] = json!(first_token);
    let refused = run(&stub, stale).await;
    assert!(text_of(&refused).starts_with("Not imported:"));
    assert!(text_of(&refused).contains("preview again"));
    assert!(stub
        .calls_to("POST", "/create/email/contacts/import")
        .is_empty());

    // Confirming with the new review's confirm_with sends the token's expectedDigest and expectedEligible.
    let mut approved = structured(&refused)["confirm_with"].clone();
    approved["csv_path"] = json!(leads.to_str().unwrap());
    let imported = run(&stub, approved).await;
    assert!(!imported.is_error, "{}", text_of(&imported));
    let posts = stub.calls_to("POST", "/create/email/contacts/import");
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].body["expectedEligible"], 2);
    assert_eq!(posts[0].body["expectedDigest"], "digest-dee-eve");
}

#[tokio::test]
async fn resume_requires_confirm_review_rechecks_after_live_ask_and_notes_legacy_counts() {
    let state = Arc::new(Mutex::new((
        2_i64,
        PROOF,
        "Proof",
        "hello@news.northwind.example",
    )));
    let state_seen = state.clone();
    let base = flight_api("paused", 0, false);
    let stub = Stub::start(Arc::new(move |request: &Recorded| {
        let path = format!("/create/email/campaigns/{CAMPAIGN}");
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", p) if p == path => {
                let (rev, aud_id, aud_name, from_email) = *state_seen.lock().unwrap();
                let mut answer = base(request);
                answer.body["data"]["revision"] = json!(rev);
                answer.body["data"]["audienceId"] = json!(aud_id);
                answer.body["data"]["audience"] = json!({"id": aud_id, "name": aud_name});
                answer.body["data"]["sender"] =
                    json!({"fromName": "Northwind", "fromEmail": from_email, "replyTo": null});
                answer
            }
            ("GET", p) if p == format!("{path}/preview") => {
                let (_, _, _, from_email) = *state_seen.lock().unwrap();
                let mut answer = base(request);
                answer.body["data"]["recipients"] = json!(3);
                answer.body["data"]["from"] = json!(format!("Northwind <{from_email}>"));
                answer
            }
            _ => base(request),
        }
    }))
    .await;
    let resume = format!("/create/email/campaigns/{CAMPAIGN}/resume");

    let review = run(
        &stub,
        json!({"action": "email_resume", "campaign_id": CAMPAIGN}),
    )
    .await;
    let stale_confirm = structured(&review)["confirm_with"].clone();
    assert!(stale_confirm["confirm_review"].is_string());

    // Campaign's sender and 3-person audience change and it is paused again at rev 4 (remaining 3).
    *state.lock().unwrap() = (4, AUDIENCE, "VIPs", "other@news.northwind.example");
    let count_only = json!({
        "action": "email_resume", "campaign_id": CAMPAIGN, "confirm_remaining": 3,
    });
    for (mode, result) in ["stateless", "live_unanswered", "no_client"]
        .into_iter()
        .zip(across_non_answering_callers(&stub, count_only).await)
    {
        let text = text_of(&result);
        assert!(
            text.starts_with("Not resumed yet:"),
            "{mode}: expected Not resumed yet, got: {text}"
        );
        assert!(text.contains("confirm_review"), "{mode}: {text}");
        assert_eq!(structured(&result)["resumed"], false, "{mode}");
    }
    let stale = run(&stub, stale_confirm).await;
    assert!(
        text_of(&stale).starts_with("Not resumed: the campaign changed since review."),
        "{}",
        text_of(&stale)
    );
    assert!(stub.calls_to("POST", &resume).is_empty());

    // Post-elicitation re-read catches a change during the live confirmation window.
    let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let reads_seen = reads.clone();
    let state_for_live = state.clone();
    let live_stub = Stub::start(Arc::new(move |request: &Recorded| {
        let path = format!("/create/email/campaigns/{CAMPAIGN}");
        if request.method == "GET" && request.path == path {
            if reads_seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 1 {
                let mut answer = (flight_api("paused", 0, false))(request);
                answer.body["data"]["revision"] = json!(5);
                return answer;
            }
        }
        let (rev, aud_id, aud_name, from_email) = *state_for_live.lock().unwrap();
        let mut answer = (flight_api("paused", 0, false))(request);
        if request.path == path {
            answer.body["data"]["revision"] = json!(rev);
            answer.body["data"]["audienceId"] = json!(aud_id);
            answer.body["data"]["audience"] = json!({"id": aud_id, "name": aud_name});
            answer.body["data"]["sender"] =
                json!({"fromName": "Northwind", "fromEmail": from_email, "replyTo": null});
        } else if request.path == format!("{path}/preview") {
            answer.body["data"]["recipients"] = json!(3);
        }
        answer
    }))
    .await;
    let client = Arc::new(FakeClient::default());
    *client.answer.lock().unwrap() =
        Some(json!({"action": "accept", "content": {"confirm": true}}));
    let changed_live = run_with_call_peer(
        CallPeer::new(client, None, true),
        live_stub
            .tool()
            .execute(json!({"action": "email_resume", "campaign_id": CAMPAIGN})),
    )
    .await
    .unwrap();
    assert!(
        text_of(&changed_live)
            .starts_with("Not resumed: the campaign changed during confirmation."),
        "{}",
        text_of(&changed_live)
    );
    assert!(live_stub.calls_to("POST", &resume).is_empty());

    // Valid rev 4 confirm_with resumes against legacy main and discloses unanswered live count check.
    let fresh_confirm = structured(&stale)["confirm_with"].clone();
    let resumed = run(&stub, fresh_confirm).await;
    let text = text_of(&resumed);
    assert!(!resumed.is_error, "{text}");
    assert!(
        text.contains("Remaining recipients at resume time: not answered by this API."),
        "{text}"
    );
    let posts = stub.calls_to("POST", &resume);
    assert_eq!(posts.len(), 1);
    assert_eq!(
        posts[0].header("idempotency-key"),
        Some(format!("email-resume-{CAMPAIGN}-r4").as_str())
    );
}

#[tokio::test]
async fn template_update_says_plainly_when_the_api_leaves_a_field_unchanged() {
    let clears_null = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let clears_seen = clears_null.clone();
    let stub = Stub::start(over(move |request| {
        let one = format!("/create/email/templates/{TEMPLATE}");
        (request.method == "PATCH" && request.path == one).then(|| {
            let mut row = template(
                TEMPLATE,
                request.body["name"].as_str().unwrap_or("Sale layout"),
                false,
            );
            // Mirrors TemplatePatch without deserialize_with = "present": JSON null is read as
            // omitted and preserves current.description ("Winter boilerplate").
            row["description"] = match request.body.get("description") {
                Some(Value::Null) if clears_seen.load(std::sync::atomic::Ordering::Relaxed) => {
                    Value::Null
                }
                Some(Value::String(s)) if s != "ignored" => json!(s),
                _ => json!("Winter boilerplate"),
            };
            Answer::ok(row)
        })
    }))
    .await;

    let uncleared = run(
        &stub,
        json!({
            "action": "email_templates", "operation": "update",
            "template_id": TEMPLATE, "description": "",
        }),
    )
    .await;
    let text = text_of(&uncleared);
    assert!(uncleared.is_error, "{text}");
    assert!(!text.contains("Changed"), "{text}");
    assert!(
        text.contains("the description was not cleared: this API did not take it"),
        "{text}"
    );
    assert_eq!(structured(&uncleared)["changed"], false);

    let partial = run(
        &stub,
        json!({
            "action": "email_templates", "operation": "update",
            "template_id": TEMPLATE, "name": "New name", "description": "",
        }),
    )
    .await;
    let text = text_of(&partial);
    assert!(!partial.is_error, "{text}");
    assert!(!text.starts_with("Changed the template:"), "{text}");
    assert!(text.contains("“New name”"), "{text}");
    assert!(
        text.contains("the description was not cleared: this API did not take it"),
        "{text}"
    );

    let unchanged_set = run(
        &stub,
        json!({
            "action": "email_templates", "operation": "update",
            "template_id": TEMPLATE, "description": "ignored",
        }),
    )
    .await;
    assert!(unchanged_set.is_error);
    assert!(text_of(&unchanged_set)
        .contains("the description was not changed: this API did not take it"));

    clears_null.store(true, std::sync::atomic::Ordering::Relaxed);
    let cleared = run(
        &stub,
        json!({
            "action": "email_templates", "operation": "update",
            "template_id": TEMPLATE, "description": "",
        }),
    )
    .await;
    assert!(!cleared.is_error, "{}", text_of(&cleared));
    assert!(
        text_of(&cleared).starts_with(&format!(
            "Changed the template: “Sale layout” (template_id {TEMPLATE})."
        )),
        "{}",
        text_of(&cleared)
    );
    assert_eq!(structured(&cleared)["changed"], true);
}

#[tokio::test]
async fn unanswered_fields_in_resume_ab_segments_and_import_never_default_to_guesses() {
    // 1. email_resume without revision is refused, and missing progress.sent says not answered.
    let missing_rev = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let missing_rev_seen = missing_rev.clone();
    let stub = Stub::start(over(move |request| {
        let path = format!("/create/email/campaigns/{CAMPAIGN}");
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", p) if p == path => {
                let mut row = campaign("paused");
                row["progress"] = json!({"recipients": 40, "queued": 25});
                row["remaining"] = json!(25);
                if missing_rev_seen.load(std::sync::atomic::Ordering::Relaxed) {
                    row.as_object_mut().unwrap().remove("revision");
                }
                row["abTest"] = json!({
                    "subjects": ["Alt subject"],
                    "samplePercent": 20,
                    "waitHours": 4,
                    "winner": {"subject": "Alt subject", "reason": "opens"},
                });
                Some(Answer::ok(row))
            }
            ("GET", "/create/email/segments") => Some(Answer::ok(json!([{
                "id": SEGMENT, "audienceId": AUDIENCE, "name": "Mystery",
                "rules": {"conditions": [{"field": "tag", "op": "has", "value": "vip"}]},
                "contacts": 5, "subscribed": 5,
            }]))),
            ("POST", "/create/email/contacts/import/preview") => {
                Some(Answer::ok(import_preview_answer()))
            }
            ("POST", "/create/email/contacts/import") => Some(Answer::ok(json!({
                "created": 110, "updated": 7, "added": 117, "suppressed": 3,
            }))),
            _ => None,
        }
    }))
    .await;

    let no_rev = run(
        &stub,
        json!({"action": "email_resume", "campaign_id": CAMPAIGN}),
    )
    .await;
    assert!(no_rev.is_error);
    assert!(
        text_of(&no_rev).contains("revision not answered"),
        "{}",
        text_of(&no_rev)
    );

    missing_rev.store(false, std::sync::atomic::Ordering::Relaxed);
    let no_sent = run(
        &stub,
        json!({"action": "email_resume", "campaign_id": CAMPAIGN}),
    )
    .await;
    let text = text_of(&no_sent);
    assert!(!text.contains("0 of 25"), "{text}");
    assert!(text.contains("sent so far: not answered"), "{text}");

    // 2. email_ab without metric or winner.variant never defaults to "opens" or "A".
    let ab = run(
        &stub,
        json!({"action": "email_ab", "campaign_id": CAMPAIGN}),
    )
    .await;
    let ab_text = text_of(&ab);
    assert!(
        !ab_text.contains("the most opens goes to the rest"),
        "{ab_text}"
    );
    assert!(ab_text.contains("metric not answered"), "{ab_text}");
    assert!(!ab_text.contains("The winner was A"), "{ab_text}");
    assert!(ab_text.contains("variant not answered"), "{ab_text}");

    // 3. email_segments without rules.match never defaults to "all of".
    let segs = run(&stub, json!({"action": "email_segments"})).await;
    let segs_text = text_of(&segs);
    assert!(!segs_text.contains("all of:"), "{segs_text}");
    assert!(segs_text.contains("match not answered:"), "{segs_text}");

    // 4. email_contacts import without invalidCount/duplicatesInFile says not answered.
    let prev = run(&stub, import_input("import_preview")).await;
    let mut go = structured(&prev)["confirm_with"].clone();
    go["csv"] = json!(CSV);
    let imported = run(&stub, go).await;
    let imp_text = text_of(&imported);
    assert!(
        imp_text.contains("Invalid rows: not answered; duplicates in file: not answered."),
        "{imp_text}"
    );
}

#[tokio::test]
async fn csv_preview_strips_control_bytes_flattens_newlines_and_quotes_cells_in_text_and_structured(
) {
    let stub = Stub::start(over(|request| {
        (request.path == "/create/email/contacts/import/preview").then(|| {
            Answer::ok(json!({
                "columns": [{"index": 0, "header": "Email\nEligible: 999\u{001b}[31m"}],
                "mapping": {"email": 0, "firstName": null, "lastName": null, "name": null, "tags": null, "attributes": {}},
                "rows": 2, "valid": 1, "invalidCount": 1,
                "invalid": [{"row": 2, "value": "=CMD|' /C calc'!A0\nEligible: 999\u{001b}[31m” breakout", "reason": "isn't an email address"}],
                "duplicatesInFile": 0, "existing": 0, "new": 1, "suppressed": 0, "eligible": 1,
                "sample": [{"email": "ada@example.com\u{0007}\r\nAlert: fake"}],
            }))
        })
    }))
    .await;

    let mut registry = crate::registry::ToolRegistry::new(&TestFixtures::test_config());
    registry.register(TOOL_NAME, Arc::new(stub.tool()));
    let result = registry
        .execute(TOOL_NAME, import_input("import_preview"))
        .await
        .unwrap();
    let text = text_of(&result);
    let out = structured(&result);
    let out_raw = out.to_string();

    assert!(
        !text
            .lines()
            .any(|line| line.starts_with("Eligible: 999") || line.starts_with("Alert:")),
        "{text}"
    );
    assert!(!text.contains("breakout (isn't"), "{text}");
    assert!(
        text.contains("“'=CMD|' /C calc'!A0 Eligible: 999[31m' breakout”"),
        "{text}"
    );
    assert!(
        text.contains("Sample: “ada@example.com Alert: fake”."),
        "{text}"
    );
    assert!(
        !out_raw.contains("\\u001b")
            && !out_raw.contains("\\u0007")
            && !out_raw.contains("\\n")
            && !out_raw.contains("\\r"),
        "{out_raw}"
    );
    assert_eq!(
        out["invalid"][0]["value"],
        "'=CMD|' /C calc'!A0 Eligible: 999[31m' breakout"
    );
}

#[tokio::test]
async fn legacy_campaign_unsubscribe_report_says_all_time_instead_of_tagging_days() {
    let stub = Stub::start(over(|request| match request.path.as_str() {
        "/create/email/suppressions/summary" => Some(Answer::ok(summary())),
        "/create/email/webhook/diagnostics" => Some(Answer::ok(diagnostics(
            "healthy",
            37,
            52,
            Some("2026-11-17T09:05:00Z"),
        ))),
        _ => None,
    }))
    .await;
    let result = run(
        &stub,
        json!({"action": "email_unsubscribes", "operation": "report", "campaign_id": CAMPAIGN, "days": 7}),
    )
    .await;
    let text = text_of(&result);
    assert!(
        text.contains("all time: this API cannot narrow a campaign's report by days"),
        "{text}"
    );
    let out = structured(&result);
    assert_eq!(out["days"], Value::Null);
    assert_eq!(out["window"], "all_time");
}

#[test]
fn schema_descriptions_name_every_caller_of_audience_segment_variant_and_tags() {
    assert!(DESCRIPTION.chars().count() <= 1024);
    assert!(DESCRIPTION.len() <= 1024);
    let schema = input_schema();
    let props = schema["properties"].as_object().unwrap();

    let aud = props["audience_id"]["description"].as_str().unwrap();
    for caller in [
        "email_draft",
        "email_templates",
        "email_send",
        "email_audiences",
        "email_contacts",
        "email_segments",
    ] {
        assert!(aud.contains(caller), "audience_id missing {caller}: {aud}");
    }

    let seg = props["segment_id"]["description"].as_str().unwrap();
    for caller in [
        "email_draft",
        "email_templates",
        "email_send",
        "email_segments",
    ] {
        assert!(seg.contains(caller), "segment_id missing {caller}: {seg}");
    }

    let var = props["variant"]["description"].as_str().unwrap();
    for caller in ["email_preview", "email_test"] {
        assert!(var.contains(caller), "variant missing {caller}: {var}");
    }

    let tags = props["tags"]["description"].as_str().unwrap();
    for caller in ["update", "import_preview", "import"] {
        assert!(tags.contains(caller), "tags missing {caller}: {tags}");
    }
}

#[tokio::test]
async fn exact_address_lookups_page_past_fifty_substring_matches_for_contacts_and_suppressions() {
    let stub = Stub::start(over(|request| {
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/create/email/contacts") => {
                if request.query.contains("cursor=50") {
                    Some(Answer::ok(json!({
                        "contacts": [{
                            "id": CONTACT,
                            "email": "ada@northwind.example",
                            "firstName": "Ada",
                            "lastName": "Lovelace",
                            "tags": [],
                            "audienceIds": [AUDIENCE],
                        }],
                        "nextCursor": null,
                        "total": 51,
                    })))
                } else {
                    let decoys: Vec<Value> = (0..50)
                        .map(|i| {
                            json!({
                                "id": format!("55555555-5555-4555-8555-{i:012}"),
                                "email": format!("prefix-{i}-ada@northwind.example"),
                                "audienceIds": [],
                            })
                        })
                        .collect();
                    Some(Answer::ok(json!({
                        "contacts": decoys,
                        "nextCursor": "50",
                        "total": 51,
                    })))
                }
            }
            ("PATCH", path) if path == format!("/create/email/contacts/{CONTACT}") => {
                Some(Answer::ok(patched_contact(&request.body)))
            }
            ("GET", "/create/email/suppressions") => {
                if request.query.contains("cursor=50") {
                    Some(Answer::ok(json!({
                        "suppressions": [{
                            "email": "ada@northwind.example",
                            "reason": "bounced",
                            "source": "webhook",
                            "detail": "550 Unknown",
                            "createdAt": "2026-10-19T08:00:00Z",
                        }],
                        "nextCursor": null,
                        "total": 51,
                    })))
                } else {
                    let decoys: Vec<Value> = (0..50)
                        .map(|i| {
                            json!({
                                "email": format!("prefix-{i}-ada@northwind.example"),
                                "reason": "bounced",
                                "source": "webhook",
                            })
                        })
                        .collect();
                    Some(Answer::ok(json!({
                        "suppressions": decoys,
                        "nextCursor": "50",
                        "total": 51,
                    })))
                }
            }
            ("POST", "/create/email/suppressions/remove") => {
                Some(Answer::ok(json!({"removed": true})))
            }
            _ => None,
        }
    }))
    .await;

    let updated = run(
        &stub,
        json!({
            "action": "email_contacts", "operation": "update",
            "email": "ada@northwind.example", "first_name": "Augusta",
        }),
    )
    .await;
    assert!(!updated.is_error, "{}", text_of(&updated));
    assert_eq!(
        stub.calls_to("PATCH", &format!("/create/email/contacts/{CONTACT}"))
            .len(),
        1
    );

    let removed = run(
        &stub,
        json!({
            "action": "email_unsubscribes", "operation": "remove",
            "email": "ada@northwind.example", "confirm_email": "ada@northwind.example",
        }),
    )
    .await;
    assert!(!removed.is_error, "{}", text_of(&removed));
    assert_eq!(
        stub.calls_to("POST", "/create/email/suppressions/remove")
            .len(),
        1
    );
}

#[tokio::test]
async fn email_draft_and_email_test_send_deterministic_idempotency_keys() {
    let stub = Stub::start(usual()).await;
    let draft_call = json!({
        "action": "email_draft",
        "prompt": "announce our winter sale to customers",
        "audience_id": AUDIENCE,
    });
    run(&stub, draft_call.clone()).await;
    run(&stub, draft_call).await;
    run(
        &stub,
        json!({
            "action": "email_draft",
            "prompt": "announce our spring sale to customers",
            "audience_id": AUDIENCE,
        }),
    )
    .await;
    let drafts = stub.calls_to("POST", "/create/email/campaigns/draft");
    assert_eq!(drafts.len(), 3);
    let draft_key = drafts[0].header("idempotency-key").expect("draft key");
    assert!(draft_key.starts_with("email-draft-"), "{draft_key}");
    assert!(draft_key.len() <= 200);
    assert_eq!(drafts[1].header("idempotency-key"), Some(draft_key));
    assert_ne!(drafts[2].header("idempotency-key"), Some(draft_key));

    let test_call = json!({
        "action": "email_test",
        "campaign_id": CAMPAIGN,
        "to": "delivered@resend.dev",
        "variant": 1,
    });
    run(&stub, test_call.clone()).await;
    run(&stub, test_call).await;
    run(
        &stub,
        json!({
            "action": "email_test",
            "campaign_id": CAMPAIGN,
            "to": "delivered@resend.dev",
            "variant": 0,
        }),
    )
    .await;
    let tests = stub.calls_to("POST", &format!("/create/email/campaigns/{CAMPAIGN}/test"));
    assert_eq!(tests.len(), 3);
    let test_key = tests[0].header("idempotency-key").expect("test key");
    assert!(test_key.starts_with("email-test-"), "{test_key}");
    assert!(test_key.len() <= 200);
    assert_eq!(tests[1].header("idempotency-key"), Some(test_key));
    assert_ne!(tests[2].header("idempotency-key"), Some(test_key));
}
