//! Tests for the shared reference resolver: each grade, the boundary between
//! neighbouring grades, and what each outcome tells the agent.

use super::*;
use mcp_types::tool::ContentItem;

const DOCS: RecordKind<'static> = RecordKind::new("doc", "docs");
const NODES: RecordKind<'static> = RecordKind::new("node", "nodes");

fn id(n: u128) -> Uuid {
    Uuid::from_u128(n)
}

fn candidate(n: u128, title: &str) -> LookupCandidate {
    LookupCandidate::new(id(n), title)
}

fn found(n: u128, title: &str, grade: MatchGrade) -> Outcome {
    Outcome::Found {
        id: id(n),
        title: Some(title.to_string()),
        grade: Some(grade),
    }
}

/// The reason and the candidate ids, best first, of an unresolved outcome.
fn unresolved(outcome: Outcome) -> (Unresolved, Vec<Uuid>) {
    match outcome {
        Outcome::Unresolved { reason, candidates } => (
            reason,
            candidates
                .into_iter()
                .map(|ranked| ranked.candidate.id)
                .collect(),
        ),
        other => panic!("expected an unresolved outcome, got {other:?}"),
    }
}

fn grade_of(reference: &str, title: &str, kind: RecordKind<'_>) -> Option<MatchGrade> {
    rank(reference, &[candidate(1, title)], kind)
        .first()
        .map(|ranked| ranked.grade)
}

fn text_of(result: &ToolResult) -> &str {
    match &result.content[0] {
        ContentItem::Text { text } => text,
        other => panic!("expected text content, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Ids
// ---------------------------------------------------------------------------

#[test]
fn an_id_names_its_record_without_consulting_candidates() {
    let reference = id(7).to_string();
    for destructive in [false, true] {
        assert_eq!(
            resolve(&reference, &[], DOCS, destructive),
            Outcome::Found {
                id: id(7),
                title: None,
                grade: None
            }
        );
    }
    // Surrounding whitespace and letter case do not stop an id being an id.
    let shouted = format!("  {}  ", reference.to_uppercase());
    assert!(matches!(
        resolve(&shouted, &[], DOCS, true),
        Outcome::Found { id: found, .. } if found == id(7)
    ));
}

// ---------------------------------------------------------------------------
// The grades and the boundaries between them
// ---------------------------------------------------------------------------

#[test]
fn exact_title_ignores_letter_case_and_spacing_only() {
    assert_eq!(
        grade_of("deploy RUNBOOK", "Deploy runbook", DOCS),
        Some(MatchGrade::ExactTitle)
    );
    assert_eq!(
        grade_of("  Deploy \t runbook ", "Deploy runbook", DOCS),
        Some(MatchGrade::ExactTitle)
    );
    assert_eq!(
        grade_of("ÉTAT DES LIEUX", "état des lieux", DOCS),
        Some(MatchGrade::ExactTitle)
    );
    // Punctuation is part of a title: one hyphen is the boundary between an
    // exact title and the same words.
    assert_eq!(
        grade_of("deploy-runbook", "Deploy runbook", DOCS),
        Some(MatchGrade::SameWords)
    );
    assert_eq!(
        grade_of("Deploy runbook.", "Deploy runbook", DOCS),
        Some(MatchGrade::SameWords)
    );
}

#[test]
fn titles_in_other_scripts_are_compared_in_full() {
    // Comparing only ASCII letters and digits would reduce both titles to
    // "v2" and call them the same.
    let candidates = [
        candidate(1, "デプロイ手順 v2"),
        candidate(2, "リリース手順 v2"),
    ];
    assert_eq!(
        resolve("リリース手順 v2", &candidates, DOCS, true),
        found(2, "リリース手順 v2", MatchGrade::ExactTitle)
    );
    assert_eq!(
        grade_of("リリース手順 v2", "デプロイ手順 v2", DOCS),
        Some(MatchGrade::SomeWords)
    );
}

#[test]
fn same_words_ignores_punctuation_fillers_and_the_records_own_noun() {
    assert_eq!(
        grade_of("show me the deploy runbook doc", "Deploy runbook", DOCS),
        Some(MatchGrade::SameWords)
    );
    assert_eq!(
        grade_of("deploy runbook", "The Deploy Runbook", DOCS),
        Some(MatchGrade::SameWords)
    );
    // The noun of another kind is a word like any other.
    assert_eq!(
        grade_of("deploy runbook node", "Deploy runbook", DOCS),
        Some(MatchGrade::SomeWords)
    );
    assert_eq!(
        grade_of("deploy runbook node", "Deploy runbook", NODES),
        Some(MatchGrade::SameWords)
    );
    // Word order is part of "the same words".
    assert_eq!(
        grade_of("runbook deploy", "Deploy runbook", DOCS),
        Some(MatchGrade::AllWords)
    );
    // One extra word in the title is the boundary to AllWords.
    assert_eq!(
        grade_of("deploy runbook", "Deploy runbook v2", DOCS),
        Some(MatchGrade::AllWords)
    );
}

#[test]
fn all_words_needs_every_word_of_the_reference_in_the_title() {
    assert_eq!(
        grade_of("staging deploy", "Deploy runbook for staging", DOCS),
        Some(MatchGrade::AllWords)
    );
    // One word of the reference missing from the title is the boundary to
    // SomeWords.
    assert_eq!(
        grade_of("deploy notes", "Deploy runbook", DOCS),
        Some(MatchGrade::SomeWords)
    );
    assert_eq!(
        grade_of(
            "staging deploy checklist",
            "Deploy runbook for staging",
            DOCS
        ),
        Some(MatchGrade::SomeWords)
    );
    // No shared word is no match at all.
    assert_eq!(grade_of("billing export", "Deploy runbook", DOCS), None);
}

#[test]
fn a_word_finds_longer_forms_of_itself_but_not_unrelated_words() {
    assert_eq!(
        grade_of("deploy", "Deployment guide", DOCS),
        Some(MatchGrade::AllWords)
    );
    assert_eq!(
        grade_of("log", "Audit logs", DOCS),
        Some(MatchGrade::AllWords)
    );
    // A word is found at the start of a title word, never inside one.
    assert_eq!(grade_of("log", "Blog ideas", DOCS), None);
    // Words shorter than three letters must be equal.
    assert_eq!(grade_of("ui", "Uikit migration", DOCS), None);
    assert_eq!(
        grade_of("ui", "UI migration", DOCS),
        Some(MatchGrade::AllWords)
    );
    // Words with digits must be equal, however long they are: a version or a
    // year is not a prefix of another one.
    assert_eq!(grade_of("v10", "Rollout v100", DOCS), None);
    assert_eq!(grade_of("2026", "Roadmap 20261", DOCS), None);
    assert_eq!(
        grade_of("v10", "Rollout v10", DOCS),
        Some(MatchGrade::AllWords)
    );
    assert_eq!(grade_of("v1", "Rollout v10", DOCS), None);
    assert_eq!(
        grade_of("v1", "Rollout v1", DOCS),
        Some(MatchGrade::AllWords)
    );
    assert_eq!(
        grade_of("1.0.18", "1.0.180", RecordKind::new("release", "releases")),
        Some(MatchGrade::SomeWords)
    );
}

#[test]
fn a_reference_made_only_of_fillers_keeps_its_words() {
    // "the plan" must not match every plan once "the" and "plan" are set
    // aside.
    let plans = RecordKind::new("plan", "plans");
    assert_eq!(grade_of("the plan", "Release cutover", plans), None);
    assert_eq!(
        grade_of("the plan", "The Plan", plans),
        Some(MatchGrade::ExactTitle)
    );
    assert_eq!(
        grade_of("the plan", "Plan", plans),
        Some(MatchGrade::SomeWords)
    );
}

// ---------------------------------------------------------------------------
// The rule: one leader, and a grade good enough for the action
// ---------------------------------------------------------------------------

#[test]
fn reads_and_edits_resolve_a_single_leader_of_all_words_or_better() {
    let candidates = [
        candidate(1, "Deploy runbook for staging"),
        candidate(2, "Billing export"),
    ];
    assert_eq!(
        resolve("deploy runbook", &candidates, DOCS, false),
        found(1, "Deploy runbook for staging", MatchGrade::AllWords)
    );
    assert_eq!(
        resolve("the deploy runbook for staging", &candidates, DOCS, false),
        found(1, "Deploy runbook for staging", MatchGrade::SameWords)
    );
}

#[test]
fn a_partial_match_never_resolves_even_when_it_is_the_only_one() {
    // The example from the report: one doc mentions "deploy", none is called
    // "deploy notes".
    let candidates = [
        candidate(1, "Deploy runbook"),
        candidate(2, "Billing export"),
    ];
    assert_eq!(
        unresolved(resolve("deploy notes", &candidates, DOCS, false)),
        (Unresolved::PartialMatch, vec![id(1)])
    );
    assert_eq!(
        unresolved(resolve("deploy notes", &candidates, DOCS, true)),
        (Unresolved::NotExact, vec![id(1)])
    );
}

#[test]
fn destructive_actions_accept_an_exact_title_and_nothing_less() {
    let candidates = [candidate(1, "Deploy runbook")];
    assert_eq!(
        resolve("deploy runbook", &candidates, DOCS, true),
        found(1, "Deploy runbook", MatchGrade::ExactTitle)
    );
    // Each weaker grade is refused, and the record is offered as a candidate.
    for reference in [
        "deploy-runbook",
        "the deploy runbook doc",
        "runbook",
        "deploy notes",
    ] {
        assert_eq!(
            unresolved(resolve(reference, &candidates, DOCS, true)),
            (Unresolved::NotExact, vec![id(1)]),
            "reference {reference:?}"
        );
    }
    assert_eq!(
        resolve("billing", &candidates, DOCS, true),
        Outcome::NoMatch
    );
}

#[test]
fn two_records_with_the_same_exact_title_are_returned_and_none_is_picked() {
    let candidates = [
        candidate(2, "Deploy runbook"),
        candidate(1, "deploy  RUNBOOK"),
        candidate(3, "Deploy runbook for staging"),
    ];
    for destructive in [false, true] {
        let (reason, ids) = unresolved(resolve("Deploy runbook", &candidates, DOCS, destructive));
        assert_eq!(reason, Unresolved::DuplicateTitle);
        // Both exact titles, and not the longer title that merely contains
        // the reference.
        assert_eq!(ids.len(), 2);
        assert!(ids.contains(&id(1)) && ids.contains(&id(2)));
    }
}

#[test]
fn a_tie_at_the_best_grade_is_ambiguous() {
    let all_words = [
        candidate(1, "Ledger database choice"),
        candidate(2, "Ledger database rollout"),
        candidate(3, "Ledger"),
    ];
    assert_eq!(
        unresolved(resolve("ledger database", &all_words, NODES, false)),
        (Unresolved::Ambiguous, vec![id(1), id(2)])
    );

    let same_words = [
        candidate(1, "On-call schedule"),
        candidate(2, "On call: schedule"),
    ];
    let (reason, mut ids) = unresolved(resolve("on call schedule", &same_words, DOCS, false));
    ids.sort();
    assert_eq!((reason, ids), (Unresolved::Ambiguous, vec![id(1), id(2)]));
}

#[test]
fn one_leader_wins_over_any_number_of_lower_grades() {
    let candidates = [
        candidate(1, "Deploy runbook for staging"),
        candidate(2, "Deploy runbook for production"),
        candidate(3, "Deploy-Runbook"),
        candidate(4, "Deploy runbook"),
    ];
    // An exact title beats the same words and every title that contains it.
    for destructive in [false, true] {
        assert_eq!(
            resolve("deploy runbook", &candidates, DOCS, destructive),
            found(4, "Deploy runbook", MatchGrade::ExactTitle)
        );
    }
    // Without the exact title, the same words beat the titles that contain
    // them, for a read. A destructive action still refuses.
    let without_exact = &candidates[..3];
    assert_eq!(
        resolve("deploy runbook", without_exact, DOCS, false),
        found(3, "Deploy-Runbook", MatchGrade::SameWords)
    );
    let (reason, ids) = unresolved(resolve("deploy runbook", without_exact, DOCS, true));
    assert_eq!(reason, Unresolved::NotExact);
    assert_eq!(ids, vec![id(3), id(2), id(1)]);
}

#[test]
fn the_same_record_listed_twice_is_one_candidate() {
    // A plan can come back in both the project and the workspace listing.
    let candidates = [
        candidate(1, "Release cutover"),
        candidate(1, "Release cutover"),
    ];
    assert_eq!(
        resolve("release cutover", &candidates, DOCS, true),
        found(1, "Release cutover", MatchGrade::ExactTitle)
    );
}

#[test]
fn a_record_answers_to_each_of_its_titles() {
    let skills = RecordKind::new("skill", "skills");
    let candidates = [
        LookupCandidate::new(id(1), "Deploy checker").also_known_as("deploy-checker"),
        LookupCandidate::new(id(2), "Deploy checker (legacy)").also_known_as("deploy-checker-v1"),
    ];
    assert_eq!(
        resolve("deploy-checker", &candidates, skills, true),
        found(1, "Deploy checker", MatchGrade::ExactTitle)
    );
    assert_eq!(
        resolve("deploy-checker-v1", &candidates, skills, true),
        found(2, "Deploy checker (legacy)", MatchGrade::ExactTitle)
    );
}

#[test]
fn a_record_without_a_title_is_named_only_by_its_id() {
    let untitled = LookupCandidate::from_item(
        &json!({"id": id(1).to_string(), "event_type": "decision", "content": "deploy notes"}),
        &["title", "summary"],
    )
    .expect("an item with a UUID id is a candidate");
    assert_eq!(untitled.title(), "(untitled)");
    assert_eq!(
        resolve("decision", std::slice::from_ref(&untitled), DOCS, false),
        Outcome::NoMatch
    );
    assert_eq!(
        resolve("(untitled)", std::slice::from_ref(&untitled), DOCS, true),
        Outcome::NoMatch
    );
    assert_eq!(
        exact_matches(&id(1).to_string(), std::slice::from_ref(&untitled)),
        vec![untitled]
    );
}

#[test]
fn candidates_come_from_the_first_title_field_that_has_text() {
    let items = [
        json!({"id": id(1).to_string(), "title": "  ", "summary": "Ledger database choice"}),
        json!({"id": "not-a-uuid", "title": "Skipped"}),
        json!({"id": id(2).to_string(), "title": "Caching note", "summary": "ignored"}),
    ];
    let candidates = candidates_from_items(&items, &["title", "summary"]);
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| (candidate.id, candidate.title()))
            .collect::<Vec<_>>(),
        vec![(id(1), "Ledger database choice"), (id(2), "Caching note")]
    );
}

// ---------------------------------------------------------------------------
// Bulk delete and retiring statuses
// ---------------------------------------------------------------------------

#[test]
fn exact_matches_are_the_id_or_every_exact_title() {
    let candidates = [
        candidate(1, "Infrastructure is AWS ONLY"),
        candidate(2, "infrastructure is aws only"),
        candidate(3, "Infrastructure is AWS only!"),
        candidate(4, "Infrastructure runbook for AWS onboarding"),
    ];
    let ids = |matches: Vec<LookupCandidate>| -> Vec<Uuid> {
        matches.into_iter().map(|candidate| candidate.id).collect()
    };
    assert_eq!(
        ids(exact_matches("Infrastructure is AWS ONLY", &candidates)),
        vec![id(1), id(2)]
    );
    assert_eq!(
        ids(exact_matches(&id(4).to_string(), &candidates)),
        vec![id(4)]
    );
    assert!(exact_matches("infrastructure", &candidates).is_empty());
    assert!(exact_matches(&id(9).to_string(), &candidates).is_empty());
}

#[test]
fn a_status_outside_the_open_set_retires_the_record() {
    let open = ["pending", "in_progress", "blocked"];
    for status in ["pending", "in_progress", "blocked", " In_Progress "] {
        assert!(!status_retires(Some(status), &open), "{status}");
    }
    for status in ["completed", "cancelled", "archived", "something_new"] {
        assert!(status_retires(Some(status), &open), "{status}");
    }
    // No status in the update means the update does not touch it.
    assert!(!status_retires(None, &open));
    assert!(!status_retires(Some("  "), &open));

    // An update that retires is resolved like a delete, and says why.
    assert_eq!(
        update_action(Some("blocked"), &open),
        (LookupAction::UPDATE, None)
    );
    let (action, hint) = update_action(Some("cancelled"), &open);
    assert!(action.destructive);
    assert!(hint.is_some_and(|hint| hint.contains("retires the record")));
    // With no open set known, every status change counts.
    assert!(update_action(Some("in_progress"), &[]).0.destructive);
    assert!(!update_action(None, &[]).0.destructive);
}

// ---------------------------------------------------------------------------
// What the caller and the agent get back
// ---------------------------------------------------------------------------

fn context(action: LookupAction) -> LookupContext<'static> {
    LookupContext {
        kind: DOCS,
        action,
        retry: "memory(action=\"delete_doc\", doc_id=\"<id>\")",
        hint: None,
    }
}

#[test]
fn an_unresolved_reference_is_answered_with_candidates_in_text_and_structure() {
    let candidates = [
        candidate(1, "Deploy runbook"),
        candidate(2, "Billing export"),
    ];
    let lookup = resolve_reference(
        &context(LookupAction::DELETE),
        "deploy notes",
        &candidates,
        || unreachable!("a partial match is not a miss"),
    )
    .expect("a candidate answer is not an error value");
    let Lookup::Candidates(result) = lookup else {
        panic!("a partial match must not resolve");
    };

    assert!(
        result.is_error,
        "nothing was done, so the call did not succeed"
    );
    let text = text_of(&result);
    assert!(text.starts_with(
        "[CANDIDATES] \"deploy notes\" is not the id or the exact title of any doc; nothing was deleted."
    ));
    assert!(text.contains(&format!("1. **Deploy runbook** (id: {})", id(1))));
    assert!(!text.contains("Billing export"));
    assert!(text.ends_with("Retry: memory(action=\"delete_doc\", doc_id=\"<id>\")"));

    let structured = result
        .structured_content
        .as_ref()
        .expect("structured result");
    assert_eq!(structured["resolved"], false);
    assert_eq!(structured["lookup"], "deploy notes");
    assert_eq!(structured["reason"], "not_exact");
    assert_eq!(
        structured["retry"],
        "memory(action=\"delete_doc\", doc_id=\"<id>\")"
    );
    assert!(structured["message"]
        .as_str()
        .is_some_and(|message| message.contains("nothing was deleted")));
    assert_eq!(structured["candidate_count"], 1);
    assert_eq!(
        structured["candidates"],
        json!([{
            "id": id(1),
            "title": "Deploy runbook",
            "match": "some_words",
            "score": 2_650,
        }])
    );
}

#[test]
fn each_reason_says_what_matched_and_that_nothing_was_done() {
    let answer = |action: LookupAction, reference: &str, candidates: &[LookupCandidate]| {
        match resolve_reference(&context(action), reference, candidates, String::new) {
            Ok(Lookup::Candidates(result)) => text_of(&result).to_string(),
            other => panic!("expected candidates, got {other:?}"),
        }
    };
    let twins = [
        candidate(1, "Deploy runbook"),
        candidate(2, "Deploy runbook"),
    ];
    assert!(answer(LookupAction::DELETE, "deploy runbook", &twins).starts_with(
        "[CANDIDATES] 2 docs are titled \"deploy runbook\"; nothing was deleted. Pass the id of the one you mean:"
    ));
    let close = [
        candidate(1, "Deploy runbook for staging"),
        candidate(2, "Deploy runbook for production"),
    ];
    assert!(answer(LookupAction::UPDATE, "deploy runbook", &close).starts_with(
        "[CANDIDATES] 2 docs match \"deploy runbook\" equally well; nothing was updated. Pass the id of the one you mean:"
    ));
    assert!(
        answer(LookupAction::READ, "deploy notes", &close).starts_with(
            "[CANDIDATES] No doc title has every word of \"deploy notes\"; nothing was opened."
        )
    );
}

#[test]
fn a_hint_and_a_long_list_are_both_reported() {
    let candidates: Vec<LookupCandidate> = (1..=11)
        .map(|n| candidate(n, &format!("Deploy runbook {n}")).with_detail("[runbook]"))
        .collect();
    let mut context = context(LookupAction::retire("updated"));
    context.hint = Some("Setting the status to \"archived\" retires the doc.");
    let Ok(Lookup::Candidates(result)) =
        resolve_reference(&context, "deploy runbook", &candidates, String::new)
    else {
        panic!("eleven titles that contain the reference must not resolve");
    };
    let text = text_of(&result);
    assert!(
        text.contains("nothing was updated. Setting the status to \"archived\" retires the doc.")
    );
    assert!(text.contains(&format!("(id: {}) [runbook]", id(1))));
    assert!(text.contains("... 3 more not shown."));
    let structured = result
        .structured_content
        .as_ref()
        .expect("structured result");
    assert_eq!(structured["candidate_count"], 11);
    assert_eq!(structured["candidates"].as_array().map(Vec::len), Some(8));
    assert_eq!(structured["candidates"][0]["detail"], "[runbook]");
}

#[test]
fn a_reference_that_matches_nothing_is_the_callers_not_found_error() {
    let error = resolve_reference(
        &context(LookupAction::READ),
        "billing export",
        &[candidate(1, "Deploy runbook")],
        || "No docs found matching \"billing export\".".to_string(),
    )
    .expect_err("no shared word is a miss");
    assert!(matches!(error, Error::Validation(message) if message.contains("No docs found")));
}

#[test]
fn an_inexact_title_is_named_in_the_text_and_in_the_structured_result() {
    let candidates = [candidate(1, "Deploy runbook for staging")];
    let Ok(Lookup::Found(found)) = resolve_reference(
        &context(LookupAction::UPDATE),
        "deploy runbook",
        &candidates,
        String::new,
    ) else {
        panic!("a single title with every word resolves for an edit");
    };
    assert_eq!(found.id, id(1));
    let note = format!(
        "Resolved \"deploy runbook\" to doc **Deploy runbook for staging** (id: {}).",
        id(1)
    );
    assert_eq!(found.note.as_deref(), Some(note.as_str()));

    let result = found.tool_result("Doc updated.", json!({"id": id(1)}));
    assert!(!result.is_error);
    assert_eq!(text_of(&result), format!("{note}\n\nDoc updated."));
    assert_eq!(
        result
            .structured_content
            .as_ref()
            .expect("structured result")["lookup_resolution"],
        json!({
            "lookup": "deploy runbook",
            "resolved_id": id(1),
            "resolved_title": "Deploy runbook for staging",
            "match": "all_words",
        })
    );
}

#[test]
fn an_id_or_an_exact_title_needs_no_note() {
    let candidates = [candidate(1, "Deploy runbook")];
    for reference in [id(1).to_string(), "deploy runbook".to_string()] {
        let Ok(Lookup::Found(found)) = resolve_reference(
            &context(LookupAction::DELETE),
            &reference,
            &candidates,
            String::new,
        ) else {
            panic!("an id or an exact title resolves");
        };
        assert_eq!(found, Found::by_id(id(1)));
        let result = found.tool_result("Doc deleted successfully.", json!({"deleted": true}));
        assert_eq!(text_of(&result), "Doc deleted successfully.");
        assert_eq!(
            result
                .structured_content
                .as_ref()
                .expect("structured result"),
            &json!({"deleted": true})
        );
    }
}

#[test]
fn the_score_only_orders_the_list() {
    let candidates = [
        candidate(1, "Staging checklist"),
        candidate(2, "Deploy checklist for staging"),
        candidate(3, "Deploy checklist for staging and production rollout"),
        candidate(4, "Deploy-checklist: staging rollout"),
    ];
    let ranked = rank("deploy checklist staging rollout", &candidates, DOCS);
    assert_eq!(
        ranked
            .iter()
            .map(|ranked| (ranked.candidate.id, ranked.grade, ranked.score()))
            .collect::<Vec<_>>(),
        vec![
            (id(4), MatchGrade::SameWords, 8_000),
            (id(3), MatchGrade::AllWords, 7_000),
            (id(2), MatchGrade::SomeWords, 2_950),
            (id(1), MatchGrade::SomeWords, 2_800),
        ]
    );
}
