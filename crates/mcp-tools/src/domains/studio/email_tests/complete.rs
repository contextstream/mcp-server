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
