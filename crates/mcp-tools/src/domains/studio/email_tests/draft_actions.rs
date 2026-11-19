use super::*;

const DRAFT_SUBJECT: &str = "Our winter sale starts today";
const CAMPAIGNS: &str = "/create/email/campaigns";

fn listed_campaign(id: &str, status: &str, subject: &str, day: u8) -> Value {
    json!({
        "id": id,
        "status": status,
        "subject": subject,
        "createdAt": format!("2026-11-{day:02}T10:00:00Z"),
    })
}

fn active_campaigns() -> Vec<Value> {
    vec![
        listed_campaign(AUDIENCE, "scheduled", "Scheduled sale", 18),
        listed_campaign(SEGMENT, "sending", "Today's sale", 17),
        listed_campaign(CONTACT, "sent", "Yesterday's sale", 16),
    ]
}

async fn stub_with_campaigns(campaigns: Vec<Value>) -> Stub {
    let fallback = usual();
    Stub::start(Arc::new(move |request: &Recorded| {
        if request.method == "GET" && request.path == CAMPAIGNS {
            Answer::ok(json!(campaigns))
        } else {
            fallback(request)
        }
    }))
    .await
}

async fn assert_uses_newest_draft(input: Value, method: &str, endpoint: &str) {
    // Create orders newest first. Include non-drafts anyway to check the tool verifies status.
    let mut campaigns = active_campaigns();
    campaigns.push(listed_campaign(CAMPAIGN, "draft", DRAFT_SUBJECT, 15));
    campaigns.push(listed_campaign(COPY, "draft", "Older sale draft", 14));
    let stub = stub_with_campaigns(campaigns).await;

    let result = stub.tool().execute(input).await.unwrap();
    let text = text_of(&result);
    assert!(!result.is_error, "{text}");
    assert!(text.contains(DRAFT_SUBJECT), "{text}");
    assert!(text.contains(CAMPAIGN), "{text}");
    assert_eq!(structured(&result)["campaign_id"], CAMPAIGN);

    let lists = stub.calls_to("GET", CAMPAIGNS);
    assert_eq!(lists.len(), 1);
    assert_eq!(
        lists[0].query,
        format!("workspaceId={WORKSPACE}&status=draft")
    );
    assert_eq!(
        stub.calls_to(method, &format!("{CAMPAIGNS}/{CAMPAIGN}/{endpoint}"))
            .len(),
        1
    );
}

#[tokio::test]
async fn revising_without_an_id_uses_and_names_the_newest_draft() {
    assert_uses_newest_draft(
        json!({"action": "email_revise", "change": "shorter subject"}),
        "POST",
        "revise",
    )
    .await;
}

#[tokio::test]
async fn previewing_without_an_id_uses_and_names_the_newest_draft() {
    assert_uses_newest_draft(json!({"action": "email_preview"}), "GET", "preview").await;
}

#[tokio::test]
async fn testing_without_an_id_uses_and_names_the_newest_draft() {
    assert_uses_newest_draft(json!({"action": "email_test"}), "POST", "test").await;
}

#[tokio::test]
async fn draft_actions_suggest_email_draft_when_there_is_no_draft() {
    for campaigns in [Vec::new(), active_campaigns()] {
        let stub = stub_with_campaigns(campaigns).await;
        for input in [
            json!({"action": "email_revise", "change": "make it warmer"}),
            json!({"action": "email_preview"}),
            json!({"action": "email_test"}),
        ] {
            let result = stub.tool().execute(input).await.unwrap();
            let text = text_of(&result);
            assert!(result.is_error, "{text}");
            assert!(
                text.contains("No draft") && text.contains("email_draft"),
                "{text}"
            );
        }
        let requests = stub.email_calls();
        assert_eq!(requests.len(), 3);
        assert!(requests
            .iter()
            .all(|request| request.method == "GET" && request.path == CAMPAIGNS));
    }
}

const WRITER_CHANGE: &str = "Made the opening warmer.";
const WRITER_SUBJECT: &str = "Writer subject";
const WRITER_PREHEADER: &str = "Writer preheader";

async fn combined_revision_stub(fail_patch: bool) -> Stub {
    let fallback = usual();
    let campaign_path = format!("{CAMPAIGNS}/{CAMPAIGN}");
    let revise_path = format!("{campaign_path}/revise");
    let mut revised = campaign("draft");
    revised["revision"] = json!(5);
    revised["subject"] = json!(WRITER_SUBJECT);
    revised["preheader"] = json!(WRITER_PREHEADER);
    revised["content"]["blocks"][0]["text"] = json!("Come in from the cold");
    Stub::start(Arc::new(move |request: &Recorded| {
        if request.method == "POST" && request.path == revise_path {
            Answer::ok(json!({"campaign": revised, "changes": [WRITER_CHANGE]}))
        } else if request.method == "PATCH" && request.path == campaign_path {
            if fail_patch || request.body["revision"] != 5 {
                return Answer::error(409, "CONFLICT", "This campaign changed meanwhile.");
            }
            let mut patched = revised.clone();
            patched["revision"] = json!(6);
            for key in ["subject", "preheader"] {
                if let Some(line) = request.body.get(key) {
                    patched[key] = line.clone();
                }
            }
            Answer::ok(patched)
        } else {
            fallback(request)
        }
    }))
    .await
}

#[tokio::test]
async fn revising_and_setting_inbox_lines_patches_the_rewritten_revision() {
    for (subject, preheader) in [
        (Some("Winter savings"), None),
        (None, Some("")),
        (Some("Winter savings"), Some("Warm up with 20% off.")),
    ] {
        let stub = combined_revision_stub(false).await;
        let mut input =
            json!({"action": "email_revise", "campaign_id": CAMPAIGN, "change": "warmer opening"});
        let mut patch = json!({"workspaceId": WORKSPACE, "revision": 5});
        if let Some(subject) = subject {
            input["subject"] = json!(subject);
            patch["subject"] = json!(subject);
        }
        if let Some(preheader) = preheader {
            input["preheader"] = json!(preheader);
            patch["preheader"] = json!(preheader);
        }

        let result = stub.tool().execute(input).await.unwrap();
        let text = text_of(&result);
        assert!(!result.is_error, "{text}");
        let data = structured(&result);
        assert_eq!(data["revision"], 6);
        assert_eq!(data["changes"], json!([WRITER_CHANGE]));
        assert_eq!(data["subject"], subject.unwrap_or(WRITER_SUBJECT));
        assert_eq!(data["preheader"], preheader.unwrap_or(WRITER_PREHEADER));
        assert!(text.contains(WRITER_CHANGE), "{text}");
        assert!(text.contains("Come in from the cold"), "{text}");
        assert!(text.contains(subject.unwrap_or(WRITER_SUBJECT)), "{text}");
        if let Some(line) = preheader.filter(|line| !line.is_empty()) {
            assert!(text.contains(line), "{text}");
        }

        let writes: Vec<_> = stub
            .email_calls()
            .into_iter()
            .filter(|request| request.method != "GET")
            .collect();
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[0].method, "POST");
        assert_eq!(writes[0].path, format!("{CAMPAIGNS}/{CAMPAIGN}/revise"));
        assert_eq!(writes[0].body["instruction"], "warmer opening");
        assert_eq!(writes[0].body["revision"], 4);
        assert_eq!(writes[1].method, "PATCH");
        assert_eq!(writes[1].path, format!("{CAMPAIGNS}/{CAMPAIGN}"));
        assert_eq!(writes[1].body, patch);
    }
}

#[tokio::test]
async fn a_failed_inbox_patch_still_reports_the_applied_rewrite() {
    let stub = combined_revision_stub(true).await;
    let result = stub
        .tool()
        .execute(json!({
            "action": "email_revise", "campaign_id": CAMPAIGN, "change": "warmer opening",
            "subject": "Winter savings",
        }))
        .await
        .unwrap();
    let text = text_of(&result);
    assert!(result.is_error, "{text}");
    assert!(text.contains(WRITER_CHANGE), "{text}");
    assert!(text.contains("could not be set"), "{text}");
    assert!(text.contains("This campaign changed meanwhile."), "{text}");
    assert_eq!(structured(&result)["revision"], 5);
    assert_eq!(structured(&result)["subject"], WRITER_SUBJECT);
}

#[tokio::test]
async fn delivery_and_other_campaign_actions_require_an_explicit_id() {
    let stub = Stub::start(usual()).await;
    for input in [
        json!({"action": "email_send"}),
        json!({"action": "email_send", "send_at": "2026-11-25T10:00", "time_zone": "Europe/London"}),
        json!({"action": "email_cancel"}),
        json!({"action": "email_pause"}),
        json!({"action": "email_resume"}),
        json!({"action": "email_duplicate"}),
        json!({"action": "email_recipients"}),
        json!({"action": "email_ab", "operation": "set", "subjects": ["A", "B"]}),
        json!({"action": "email_templates", "operation": "save", "name": "Winter sale"}),
    ] {
        let action = input["action"].to_string();
        let refused = stub.tool().execute(input).await.unwrap_err();
        assert!(
            matches!(&refused, Error::Validation(message) if message.contains("campaign_id is required")),
            "{action}: {refused}"
        );
    }
    assert!(stub.email_calls().is_empty());
}
