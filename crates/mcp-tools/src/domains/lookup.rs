//! One resolver for references given as "an id or a title".
//!
//! Many tool actions take a reference to a record: `doc_id`, `node_id`,
//! `lesson_id`, `plan_id`, an entity `id`. Agents pass either the record's id
//! or text they believe is its title. Every domain resolves that text here, by
//! one rule, so no action is applied to a guess.
//!
//! # The rule
//!
//! 1. A UUID is the record's id. Nothing else is consulted.
//! 2. Any other text is compared with the titles of the candidate records the
//!    caller fetched. Each candidate gets a [`MatchGrade`], best first:
//!    - [`ExactTitle`](MatchGrade::ExactTitle): the title is the text, letter
//!      case and spacing aside.
//!    - [`SameWords`](MatchGrade::SameWords): the same words in the same
//!      order; only punctuation or filler words differ ("the", "show me", the
//!      record's own noun).
//!    - [`AllWords`](MatchGrade::AllWords): every word of the text is in the
//!      title.
//!    - [`SomeWords`](MatchGrade::SomeWords): only some are.
//! 3. The reference resolves when exactly one candidate holds the best grade
//!    present, and that grade is enough for what the caller is about to do:
//!    - an action that removes or retires the record (delete, supersede,
//!      complete, archive, invalidate) needs `ExactTitle`;
//!    - a read or an edit needs `AllWords` or better.
//! 4. Otherwise nothing is done and the candidates are returned
//!    ([`Lookup::Candidates`]). That covers two records with the same exact
//!    title, two records sharing the best grade (the runner-up is as good as
//!    the leader), a title that shares only some words, and a destructive
//!    action given anything short of an exact title.
//!
//! The grade decides. [`RankedCandidate::score`] only orders the list shown to
//! the agent.
//!
//! The resolver sees only the candidates the caller fetched, which is usually
//! one page of a listing. An exact title is evidence on its own; "the best
//! partial match on this page" is not, which is why `SomeWords` never
//! resolves.

use mcp_types::tool::ToolResult;
use mcp_types::{Error, Result};
use serde_json::{json, Value};
use uuid::Uuid;

/// Most candidates listed in one `[CANDIDATES]` answer.
const MAX_LISTED_CANDIDATES: usize = 8;

/// Words that say nothing about which record is meant: articles, pronouns,
/// prepositions, and the words a request wraps a title in.
const FILLER_WORDS: &[&str] = &[
    "a",
    "an",
    "the",
    "this",
    "that",
    "my",
    "our",
    "your",
    "me",
    "us",
    "of",
    "for",
    "to",
    "in",
    "on",
    "at",
    "by",
    "with",
    "from",
    "about",
    "and",
    "or",
    "please",
    "show",
    "open",
    "get",
    "read",
    "see",
    "view",
    "find",
    "fetch",
    "pull",
    "look",
    "lookup",
    "up",
    "called",
    "named",
    "titled",
    "mcp",
    "contextstream",
];

/// How well a candidate's title matches a reference. Ordered worst to best.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum MatchGrade {
    /// Some words of the reference are in the title, but not all.
    SomeWords,
    /// Every word of the reference is in the title.
    AllWords,
    /// The same words in the same order; punctuation or filler words differ.
    SameWords,
    /// The title is the reference, letter case and spacing aside.
    ExactTitle,
}

impl MatchGrade {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::SomeWords => "some_words",
            Self::AllWords => "all_words",
            Self::SameWords => "same_words",
            Self::ExactTitle => "exact_title",
        }
    }
}

/// What a kind of record is called, for messages and for the filler words a
/// reference to it may carry ("the deploy runbook doc").
#[derive(Debug, Clone, Copy)]
pub(crate) struct RecordKind<'a> {
    pub singular: &'a str,
    pub plural: &'a str,
}

impl<'a> RecordKind<'a> {
    pub(crate) const fn new(singular: &'a str, plural: &'a str) -> Self {
        Self { singular, plural }
    }

    fn nouns(self) -> Vec<String> {
        let mut nouns = words(self.singular);
        nouns.extend(words(self.plural));
        nouns
    }
}

/// What the caller is about to do with the record, which sets how exact the
/// reference must be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LookupAction {
    /// The action removes or retires the record: only an id or an exact title
    /// names it.
    pub destructive: bool,
    /// Past participle for the "nothing was ..." sentence.
    pub done: &'static str,
}

impl LookupAction {
    pub(crate) const READ: Self = Self::edit("opened");
    pub(crate) const UPDATE: Self = Self::edit("updated");
    pub(crate) const DELETE: Self = Self::retire("deleted");
    pub(crate) const SUPERSEDE: Self = Self::retire("superseded");
    pub(crate) const COMPLETE: Self = Self::retire("completed");

    /// A read or an in-place edit.
    pub(crate) const fn edit(done: &'static str) -> Self {
        Self {
            destructive: false,
            done,
        }
    }

    /// An action that removes or retires the record.
    pub(crate) const fn retire(done: &'static str) -> Self {
        Self {
            destructive: true,
            done,
        }
    }
}

/// Said next to the candidates when an update is refused because of the
/// status it sets.
const RETIRING_UPDATE_HINT: &str =
    "The status this update sets retires the record, so it needs an id or the exact title.";

/// The action, and the hint that explains it, for an update that may set a
/// status. Setting a status outside `open_statuses` takes the record out of
/// its open set, which retires it as surely as a delete does. A status this
/// server does not know counts as retiring.
pub(crate) fn update_action(
    status: Option<&str>,
    open_statuses: &[&str],
) -> (LookupAction, Option<&'static str>) {
    if status_retires(status, open_statuses) {
        (LookupAction::retire("updated"), Some(RETIRING_UPDATE_HINT))
    } else {
        (LookupAction::UPDATE, None)
    }
}

fn status_retires(status: Option<&str>, open_statuses: &[&str]) -> bool {
    status
        .map(str::trim)
        .filter(|status| !status.is_empty())
        .is_some_and(|status| {
            !open_statuses
                .iter()
                .any(|open| open.eq_ignore_ascii_case(status))
        })
}

/// A record a reference may name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LookupCandidate {
    pub id: Uuid,
    /// Titles the record answers to; the first is the one shown. Empty for a
    /// record without a title, which only its id can name.
    titles: Vec<String>,
    /// Shown after the title in a candidate list (a status, a scope).
    detail: Option<String>,
}

impl LookupCandidate {
    pub(crate) fn new(id: Uuid, title: &str) -> Self {
        Self {
            id,
            titles: Vec::new(),
            detail: None,
        }
        .also_known_as(title)
    }

    /// Add a second title the record answers to, such as a skill's name next
    /// to its display title.
    pub(crate) fn also_known_as(mut self, title: &str) -> Self {
        let title = title.trim();
        if !title.is_empty() && !self.titles.iter().any(|known| known == title) {
            self.titles.push(title.to_string());
        }
        self
    }

    pub(crate) fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// Build from an API item: its `id` and the first non-empty of
    /// `title_fields`. Items without a UUID id are skipped.
    pub(crate) fn from_item(item: &Value, title_fields: &[&str]) -> Option<Self> {
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .and_then(|raw| Uuid::parse_str(raw).ok())?;
        let title = title_fields
            .iter()
            .find_map(|field| {
                item.get(field)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
            })
            .unwrap_or_default();
        Some(Self::new(id, title))
    }

    /// The title shown for this record.
    pub(crate) fn title(&self) -> &str {
        self.titles
            .first()
            .map(String::as_str)
            .unwrap_or("(untitled)")
    }
}

/// Candidates for every item in an API listing. See
/// [`LookupCandidate::from_item`].
pub(crate) fn candidates_from_items(
    items: &[Value],
    title_fields: &[&str],
) -> Vec<LookupCandidate> {
    items
        .iter()
        .filter_map(|item| LookupCandidate::from_item(item, title_fields))
        .collect()
}

/// A candidate with its grade against one reference.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RankedCandidate {
    pub candidate: LookupCandidate,
    pub grade: MatchGrade,
    /// Words of the reference found in the title; orders `SomeWords` matches.
    matched_words: usize,
}

impl RankedCandidate {
    /// Orders a candidate list, best first. It decides nothing: the grade
    /// does.
    pub(crate) fn score(&self) -> i64 {
        match self.grade {
            MatchGrade::ExactTitle => 9_000,
            MatchGrade::SameWords => 8_000,
            MatchGrade::AllWords => 7_000,
            MatchGrade::SomeWords => 2_500 + 150 * self.matched_words.min(20) as i64,
        }
    }

    fn to_json(&self) -> Value {
        let mut value = json!({
            "id": self.candidate.id,
            "title": self.candidate.title(),
            "match": self.grade.as_str(),
            "score": self.score(),
        });
        if let Some(detail) = self.candidate.detail.as_deref() {
            value["detail"] = json!(detail);
        }
        value
    }
}

/// Why a reference that matched something still names no record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unresolved {
    /// Two or more records have exactly this title.
    DuplicateTitle,
    /// A destructive action was given text that is no record's exact title.
    NotExact,
    /// Two or more records share the best grade.
    Ambiguous,
    /// The best title shares only some words with the reference.
    PartialMatch,
}

impl Unresolved {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::DuplicateTitle => "duplicate_title",
            Self::NotExact => "not_exact",
            Self::Ambiguous => "ambiguous",
            Self::PartialMatch => "partial_match",
        }
    }
}

/// What a reference resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// One record is named. `grade` is `None` when the reference was its id.
    Found {
        id: Uuid,
        title: Option<String>,
        grade: Option<MatchGrade>,
    },
    /// Records match, but none may be acted on. Best first.
    Unresolved {
        reason: Unresolved,
        candidates: Vec<RankedCandidate>,
    },
    /// No candidate shares a word with the reference.
    NoMatch,
}

impl Outcome {
    /// The record to act on, when the reference named one.
    pub(crate) fn found(&self, kind: RecordKind<'_>, reference: &str) -> Option<Found> {
        match self {
            Self::Found {
                id,
                title: Some(title),
                grade: Some(grade),
            } => Some(Found::by_title(kind, reference.trim(), *id, title, *grade)),
            Self::Found { id, .. } => Some(Found::by_id(*id)),
            Self::Unresolved { .. } | Self::NoMatch => None,
        }
    }
}

/// Apply the rule in the module documentation to one reference.
pub(crate) fn resolve(
    reference: &str,
    candidates: &[LookupCandidate],
    kind: RecordKind<'_>,
    destructive: bool,
) -> Outcome {
    let reference = reference.trim();
    if let Ok(id) = Uuid::parse_str(reference) {
        return Outcome::Found {
            id,
            title: None,
            grade: None,
        };
    }

    let ranked = rank(reference, candidates, kind);
    let Some(best) = ranked.first() else {
        return Outcome::NoMatch;
    };
    let best_grade = best.grade;
    let leaders = ranked
        .iter()
        .filter(|candidate| candidate.grade == best_grade)
        .count();
    let needed = if destructive {
        MatchGrade::ExactTitle
    } else {
        MatchGrade::AllWords
    };

    let reason = if best_grade < needed {
        if destructive {
            Unresolved::NotExact
        } else {
            Unresolved::PartialMatch
        }
    } else if leaders == 1 {
        return Outcome::Found {
            id: best.candidate.id,
            title: Some(best.candidate.title().to_string()),
            grade: Some(best_grade),
        };
    } else if best_grade == MatchGrade::ExactTitle {
        Unresolved::DuplicateTitle
    } else {
        Unresolved::Ambiguous
    };
    let mut candidates = ranked;
    if matches!(reason, Unresolved::DuplicateTitle | Unresolved::Ambiguous) {
        // The choice is between the records that tie; weaker matches would
        // only blur it.
        candidates.truncate(leaders);
    }
    Outcome::Unresolved { reason, candidates }
}

/// Every candidate a reference names exactly: the one with that id, or all
/// with that exact title. This is what a bulk delete may act on.
pub(crate) fn exact_matches(
    reference: &str,
    candidates: &[LookupCandidate],
) -> Vec<LookupCandidate> {
    let reference = reference.trim();
    if let Ok(id) = Uuid::parse_str(reference) {
        return candidates
            .iter()
            .find(|candidate| candidate.id == id)
            .cloned()
            .into_iter()
            .collect();
    }
    rank(reference, candidates, RecordKind::new("", ""))
        .into_iter()
        .filter(|ranked| ranked.grade == MatchGrade::ExactTitle)
        .map(|ranked| ranked.candidate)
        .collect()
}

/// Grade every candidate against the reference, drop the ones that share no
/// word with it, and order the rest best first.
fn rank(
    reference: &str,
    candidates: &[LookupCandidate],
    kind: RecordKind<'_>,
) -> Vec<RankedCandidate> {
    let reference_key = exact_key(reference);
    if reference_key.is_empty() {
        return Vec::new();
    }
    let nouns = kind.nouns();
    let reference_words = words(reference);
    let significant_reference = significant(&reference_words, &nouns);

    let mut seen = std::collections::HashSet::new();
    let mut ranked: Vec<RankedCandidate> = candidates
        .iter()
        .filter(|candidate| seen.insert(candidate.id))
        .filter_map(|candidate| {
            let (grade, matched_words) = candidate
                .titles
                .iter()
                .filter_map(|title| {
                    grade_title(title, &reference_key, &significant_reference, &nouns)
                })
                .max()?;
            Some(RankedCandidate {
                candidate: candidate.clone(),
                grade,
                matched_words,
            })
        })
        .collect();
    ranked.sort_by(|left, right| {
        right
            .grade
            .cmp(&left.grade)
            .then_with(|| right.matched_words.cmp(&left.matched_words))
            .then_with(|| left.candidate.title().cmp(right.candidate.title()))
            .then_with(|| left.candidate.id.cmp(&right.candidate.id))
    });
    ranked
}

fn grade_title(
    title: &str,
    reference_key: &str,
    significant_reference: &[&str],
    nouns: &[String],
) -> Option<(MatchGrade, usize)> {
    if exact_key(title) == reference_key {
        return Some((MatchGrade::ExactTitle, significant_reference.len()));
    }
    if significant_reference.is_empty() {
        return None;
    }
    let title_words = words(title);
    if significant(&title_words, nouns) == significant_reference {
        return Some((MatchGrade::SameWords, significant_reference.len()));
    }
    let matched = significant_reference
        .iter()
        .filter(|word| {
            title_words
                .iter()
                .any(|title_word| word_matches(word, title_word))
        })
        .count();
    if matched == significant_reference.len() {
        Some((MatchGrade::AllWords, matched))
    } else if matched > 0 {
        Some((MatchGrade::SomeWords, matched))
    } else {
        None
    }
}

/// Letter case and spacing aside: lowercase, with whitespace runs collapsed.
fn exact_key(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// The lowercase words of `text`; anything that is not a letter or a digit
/// separates words.
fn words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    for ch in text.chars() {
        if ch.is_alphanumeric() {
            current.extend(ch.to_lowercase());
        } else if !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

/// `words` without filler words and the record's own nouns. Text made only of
/// those keeps every word, so it can still match itself.
fn significant<'a>(words: &'a [String], nouns: &[String]) -> Vec<&'a str> {
    let kept: Vec<&str> = words
        .iter()
        .map(String::as_str)
        .filter(|word| !FILLER_WORDS.contains(word) && !nouns.iter().any(|noun| noun == word))
        .collect();
    if kept.is_empty() {
        words.iter().map(String::as_str).collect()
    } else {
        kept
    }
}

/// A word of the reference is in the title when a title word equals it, or
/// starts with it ("deploy" finds "deployment"). Short words and words with
/// digits must be equal, so "v1" does not find "v10".
fn word_matches(reference_word: &str, title_word: &str) -> bool {
    reference_word == title_word
        || (reference_word.chars().count() >= 3
            && reference_word.chars().all(char::is_alphabetic)
            && title_word.starts_with(reference_word))
}

/// The messages around one lookup: what is being looked up, what for, and how
/// to retry.
#[derive(Debug, Clone, Copy)]
pub(crate) struct LookupContext<'a> {
    pub kind: RecordKind<'a>,
    pub action: LookupAction,
    /// The call to retry with an id, such as
    /// `memory(action="delete_doc", doc_id="<id>")`.
    pub retry: &'a str,
    /// One more sentence for the candidate answer, such as why this update
    /// needs an exact title.
    pub hint: Option<&'a str>,
}

/// A resolved reference.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Found {
    pub id: Uuid,
    /// Says which record a title that is not an exact match was taken to
    /// mean. `None` for an id or an exact title.
    pub note: Option<String>,
    /// The same statement for the structured result.
    resolution: Option<Value>,
}

impl Found {
    /// A record named by its id.
    pub(crate) fn by_id(id: Uuid) -> Self {
        Self {
            id,
            note: None,
            resolution: None,
        }
    }

    fn by_title(
        kind: RecordKind<'_>,
        reference: &str,
        id: Uuid,
        title: &str,
        grade: MatchGrade,
    ) -> Self {
        if grade == MatchGrade::ExactTitle {
            return Self::by_id(id);
        }
        Self {
            id,
            note: Some(format!(
                "Resolved \"{reference}\" to {} **{title}** (id: {id}).",
                kind.singular
            )),
            resolution: Some(json!({
                "lookup": reference,
                "resolved_id": id,
                "resolved_title": title,
                "match": grade.as_str(),
            })),
        }
    }

    /// Record under `lookup_resolution` which record an inexact title was
    /// taken to mean. Clients that show only the structured result never see
    /// the note in the text.
    pub(crate) fn annotate(&self, structured: &mut Value) {
        if let (Some(resolution), Some(object)) = (&self.resolution, structured.as_object_mut()) {
            object.insert("lookup_resolution".to_string(), resolution.clone());
        }
    }

    /// The tool result for the resolved record: `text` after the resolution
    /// note, and `structured` with `lookup_resolution` added.
    pub(crate) fn tool_result(&self, text: impl Into<String>, mut structured: Value) -> ToolResult {
        self.annotate(&mut structured);
        let text = text.into();
        match &self.note {
            Some(note) => ToolResult::with_structured(format!("{note}\n\n{text}"), structured),
            None => ToolResult::with_structured(text, structured),
        }
    }
}

/// How a caller proceeds after [`resolve_reference`].
#[derive(Debug, Clone)]
pub(crate) enum Lookup {
    /// Act on this record.
    Found(Found),
    /// Answer the call with this candidate list. Nothing was done.
    Candidates(ToolResult),
}

/// Resolve a reference for a tool action. A reference that matches nothing is
/// a validation error with the caller's `not_found` message; one that matches
/// without naming a single record is answered with the candidates.
pub(crate) fn resolve_reference(
    context: &LookupContext<'_>,
    reference: &str,
    candidates: &[LookupCandidate],
    not_found: impl FnOnce() -> String,
) -> Result<Lookup> {
    let reference = reference.trim();
    let outcome = resolve(
        reference,
        candidates,
        context.kind,
        context.action.destructive,
    );
    if let Some(found) = outcome.found(context.kind, reference) {
        return Ok(Lookup::Found(found));
    }
    match outcome {
        Outcome::Unresolved { reason, candidates } => Ok(Lookup::Candidates(candidates_result(
            context,
            reference,
            reason,
            &candidates,
        ))),
        Outcome::Found { .. } | Outcome::NoMatch => Err(Error::Validation(not_found())),
    }
}

/// The `[CANDIDATES]` answer: what matched, that nothing was done, and how to
/// retry. The text and the structured result carry the same facts, because a
/// client may show only one of them.
pub(crate) fn candidates_result(
    context: &LookupContext<'_>,
    reference: &str,
    reason: Unresolved,
    candidates: &[RankedCandidate],
) -> ToolResult {
    let RecordKind { singular, plural } = context.kind;
    let done = context.action.done;
    let count = candidates.len();
    let (headline, list_label) = match reason {
        Unresolved::DuplicateTitle => (
            format!("{count} {plural} are titled \"{reference}\"; nothing was {done}."),
            "Pass the id of the one you mean:".to_string(),
        ),
        Unresolved::NotExact => (
            format!(
                "\"{reference}\" is not the id or the exact title of any {singular}; nothing was {done}."
            ),
            format!("Pass an id or the exact title (letter case aside). Closest {plural}:"),
        ),
        Unresolved::Ambiguous => (
            format!("{count} {plural} match \"{reference}\" equally well; nothing was {done}."),
            "Pass the id of the one you mean:".to_string(),
        ),
        Unresolved::PartialMatch => (
            format!("No {singular} title has every word of \"{reference}\"; nothing was {done}."),
            format!("Pass an id or more of the title. Closest {plural}:"),
        ),
    };

    let mut message = headline;
    if let Some(hint) = context.hint {
        message.push(' ');
        message.push_str(hint);
    }

    let listed = &candidates[..count.min(MAX_LISTED_CANDIDATES)];
    let mut text = format!("[CANDIDATES] {message} {list_label}\n");
    for (index, ranked) in listed.iter().enumerate() {
        text.push_str(&format!(
            "{}. **{}** (id: {})",
            index + 1,
            ranked.candidate.title(),
            ranked.candidate.id
        ));
        if let Some(detail) = ranked.candidate.detail.as_deref() {
            text.push_str(&format!(" {detail}"));
        }
        text.push('\n');
    }
    if count > listed.len() {
        text.push_str(&format!("... {} more not shown.\n", count - listed.len()));
    }
    text.push_str(&format!("Retry: {}", context.retry));

    let mut result = ToolResult::with_structured(
        text,
        json!({
            "resolved": false,
            "lookup": reference,
            "reason": reason.as_str(),
            "message": message,
            "retry": context.retry,
            "candidate_count": count,
            "candidates": listed.iter().map(RankedCandidate::to_json).collect::<Vec<_>>(),
        }),
    );
    // The requested action did not happen; the caller has to correct the
    // reference and call again.
    result.is_error = true;
    result
}

#[cfg(test)]
#[path = "lookup_tests.rs"]
mod tests;
