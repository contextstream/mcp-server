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
    assert_eq!(patches.len(), 1);
    assert_eq!(
        patches[0].body,
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
        stub.calls_to("PATCH", &path)[1].body,
        json!({"workspaceId": WORKSPACE, "tags": []})
    );
    assert_eq!(stub.calls_to("GET", "/create/email/contacts").len(), before);
    assert!(
        text_of(&result).starts_with("Changed ada@northwind.example (Ada Lovelace). No tags."),
        "{}",
        text_of(&result)
    );

    // A membership change starts from the audiences the contact is in, which only its
    // address finds.
    for (input, said) in [
        (
            json!({"action": "email_contacts", "operation": "update", "contact_id": CONTACT, "add_to": [PROOF]}),
            "add_to and remove_from need email",
        ),
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
    assert_eq!(stub.calls_to("PATCH", &path).len(), 2);

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
        text.contains("Sample: ada@northwind.example, bob@northwind.example."),
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

    let mut go = import_input("import");
    go["confirm_rows"] = json!(114);
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
    let mut go = import_input("import");
    go["confirm_rows"] = json!(114);
    go["tags"] = json!(["spring-2026"]);
    let imported = tool.execute(go).await.unwrap();
    assert!(!imported.is_error, "{}", text_of(&imported));
    let posts = stub.calls_to("POST", "/create/email/contacts/import");
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].body["expectedEligible"], 114);
    assert_eq!(posts[0].body["expectedDigest"], "5f2c9a");
    assert_eq!(posts[0].body["tags"], json!(["spring-2026"]));
}

#[tokio::test]
async fn an_import_asks_the_person_where_the_client_can() {
    let stub = Stub::start(import_api(false)).await;
    let tool = stub.tool();
    let import = "/create/email/contacts/import";
    let client = Arc::new(FakeClient::default());

    // Asked live: their no stands, whatever the agent confirms.
    *client.answer.lock().unwrap() = Some(json!({"action": "decline"}));
    let mut confirmed = import_input("import");
    confirmed["confirm_rows"] = json!(114);
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
