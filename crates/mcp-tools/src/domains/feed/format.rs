//! Compact `[FEED]` renderers for Context Feeds payloads.
//!
//! Every renderer accepts either the bare API `data` payload (what the client
//! returns) or the `{success, data}` envelope, and never dumps raw JSON — the
//! structured payload travels alongside the text in the tool result.

use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

/// Most feed items surfaced in `session(action="ground")`.
pub const GROUNDING_MAX_ITEMS: usize = 5;
/// Character budget for the `[FEED]` block in `session(action="ground")`.
pub const GROUNDING_MAX_CHARS: usize = 1400;

const SUMMARY_MAX_CHARS: usize = 200;
const WHY_MAX_CHARS: usize = 120;
const TITLE_MAX_CHARS: usize = 120;
const MAX_LISTED_FEEDS: usize = 25;

pub(super) fn payload(value: &Value) -> &Value {
    value.get("data").unwrap_or(value)
}

pub(super) fn str_field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

pub(super) fn i64_field(value: &Value, key: &str) -> Option<i64> {
    value.get(key).and_then(Value::as_i64)
}

fn array_field<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// Extract the feed id from a feed payload (bare or enveloped).
pub(super) fn feed_id(feed: &Value) -> Option<Uuid> {
    str_field(payload(feed), "id").and_then(|id| Uuid::parse_str(id).ok())
}

/// Truncate to `max_chars` characters, appending an ellipsis when cut.
pub(super) fn truncate(text: &str, max_chars: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() <= max_chars {
        return collapsed;
    }
    let mut out: String = collapsed
        .chars()
        .take(max_chars.saturating_sub(1))
        .collect();
    out.push('…');
    out
}

/// Human age label such as `just now`, `5m ago`, `3h ago`, `2d ago`.
pub(super) fn age_label(timestamp: Option<&str>) -> String {
    timestamp
        .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        .map(|ts| age_label_at(ts.with_timezone(&Utc), Utc::now()))
        .unwrap_or_else(|| "unknown age".to_string())
}

pub(super) fn age_label_at(timestamp: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let seconds = now.signed_duration_since(timestamp).num_seconds().max(0);
    match seconds {
        s if s < 60 => "just now".to_string(),
        s if s < 3_600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3_600),
        s if s < 30 * 86_400 => format!("{}d ago", s / 86_400),
        s if s < 365 * 86_400 => format!("{}mo ago", s / (30 * 86_400)),
        s => format!("{}y ago", s / (365 * 86_400)),
    }
}

fn items_hint(feed_id: &str) -> String {
    format!("feed(action=\"items\", feed_id=\"{feed_id}\")")
}

/// One-line feed summary: name, kind, access, follow state, and counters.
pub(super) fn feed_line(feed: &Value) -> String {
    let feed = payload(feed);
    let name = str_field(feed, "name").unwrap_or("Untitled feed");
    let kind = str_field(feed, "kind").unwrap_or("feed");
    let access = str_field(feed, "access").unwrap_or("owner");
    let id = str_field(feed, "id").unwrap_or("?");
    let unread = i64_field(feed, "unread_count").unwrap_or(0);
    let items = i64_field(feed, "item_count").unwrap_or(0);
    let mut flags = format!("{kind}, {access}");
    if feed
        .pointer("/follow/following")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        flags.push_str(", following");
    }
    if let Some(audience) = str_field(feed, "audience") {
        flags.push_str(&format!(", audience={audience}"));
    }
    if str_field(feed, "status").is_some_and(|status| status != "active") {
        flags.push_str(&format!(
            ", {}",
            str_field(feed, "status").unwrap_or("inactive")
        ));
    }
    format!(
        "[FEED] {} ({flags}) — {unread} unread / {items} item(s) · feed_id={id}",
        truncate(name, TITLE_MAX_CHARS)
    )
}

pub(super) fn format_feed_list(result: &Value, include: &str) -> String {
    let page = payload(result);
    let feeds = array_field(page, "items");
    let total = page
        .get("total")
        .and_then(Value::as_u64)
        .unwrap_or(feeds.len() as u64);
    let mut text = format!(
        "[FEED] {} of {total} feed(s) visible (include={include}).",
        feeds.len().min(MAX_LISTED_FEEDS)
    );
    if feeds.is_empty() {
        text.push_str(
            "\nNo feeds yet — feed(action=\"ensure\") creates the canonical feed for this scope.",
        );
        return text;
    }
    for feed in feeds.iter().take(MAX_LISTED_FEEDS) {
        text.push('\n');
        text.push_str(&feed_line(feed));
    }
    if let Some(next) = page.get("next_cursor").and_then(Value::as_u64) {
        text.push_str(&format!("\nMore: feed(action=\"list\", cursor={next})"));
    }
    text.push_str("\nRead one: feed(action=\"items\", feed_id=\"<feed_id>\", view=\"unread\")");
    text
}

pub(super) fn format_feed(result: &Value, headline: &str) -> String {
    let feed = payload(result);
    let mut text = format!("[FEED] {headline}\n{}", feed_line(feed));
    if let Some(description) = str_field(feed, "description") {
        text.push_str(&format!("\n{}", truncate(description, SUMMARY_MAX_CHARS)));
    }
    let revision = i64_field(feed, "revision").unwrap_or(0);
    let latest = i64_field(feed, "latest_sequence").unwrap_or(0);
    let last_read = feed
        .pointer("/follow/last_read_sequence")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    text.push_str(&format!(
        "\nrevision={revision} latest_sequence={latest} last_read_sequence={last_read}"
    ));
    if let Some(id) = str_field(feed, "id") {
        text.push_str(&format!("\nNext: {}", items_hint(id)));
    }
    text
}

fn item_summary(item: &Value) -> String {
    let summary = str_field(item, "summary")
        .or_else(|| str_field(item, "content_excerpt"))
        .unwrap_or("(no summary)");
    truncate(summary, SUMMARY_MAX_CHARS)
}

/// `title — summary (why: …) [age] · item_id=…`
pub(super) fn item_line(item: &Value) -> String {
    let title = truncate(
        str_field(item, "title").unwrap_or("Untitled"),
        TITLE_MAX_CHARS,
    );
    let mut line = format!("[FEED] {title} — {}", item_summary(item));
    if let Some(why) = str_field(item, "why_it_matters") {
        line.push_str(&format!(" (why: {})", truncate(why, WHY_MAX_CHARS)));
    }
    line.push_str(&format!(" [{}]", age_label(str_field(item, "occurred_at"))));
    if let Some(id) = str_field(item, "id").or_else(|| str_field(item, "item_id")) {
        line.push_str(&format!(" · item_id={id}"));
    }
    if let Some(kind) = str_field(item, "item_kind") {
        line.push_str(&format!(" #{kind}"));
    }
    if item.get("unread").and_then(Value::as_bool) == Some(true) {
        line.push_str(" · unread");
    }
    line
}

pub(super) fn format_items(
    feed: Option<&Value>,
    page: &Value,
    feed_id: Uuid,
    view: &str,
) -> String {
    let page = payload(page);
    let items = array_field(page, "items");
    let total = page
        .get("total")
        .and_then(Value::as_u64)
        .unwrap_or(items.len() as u64);
    let mut text = match feed {
        Some(feed) => feed_line(feed),
        None => format!("[FEED] feed_id={feed_id}"),
    };
    text.push_str(&format!(
        "\n[FEED] view={view}: {} of {total} item(s).",
        items.len()
    ));
    if items.is_empty() {
        text.push_str("\nNothing here yet. Try view=\"latest\" or feed(action=\"curate\", feed_id=\"...\") to refresh curation.");
        return text;
    }
    for item in items {
        text.push('\n');
        text.push_str(&item_line(item));
    }
    if let Some(next) = page.get("next_cursor").and_then(Value::as_u64) {
        text.push_str(&format!(
            "\nMore: feed(action=\"items\", feed_id=\"{feed_id}\", view=\"{view}\", cursor={next})"
        ));
    }
    let latest = items
        .iter()
        .filter_map(|item| i64_field(item, "sequence"))
        .max()
        .or_else(|| feed.and_then(|feed| i64_field(payload(feed), "latest_sequence")));
    if let Some(latest) = latest {
        text.push_str(&format!(
            "\nMark read: feed(action=\"read\", feed_id=\"{feed_id}\", last_read_sequence={latest})"
        ));
    }
    text.push_str(&format!(
        "\nFeedback: feed(action=\"feedback\", feed_id=\"{feed_id}\", item_id=\"<item_id>\", feedback_type=\"positive|dismiss|not_relevant|hard_ignore\")"
    ));
    text
}

pub(super) fn format_item_detail(result: &Value) -> String {
    let detail = payload(result);
    let item = detail.get("item").unwrap_or(detail);
    let mut text = item_line(item);
    if let Some(excerpt) = str_field(item, "content_excerpt") {
        text.push_str(&format!("\n{}", truncate(excerpt, 600)));
    }
    let tags: Vec<&str> = array_field(item, "tags")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    if !tags.is_empty() {
        text.push_str(&format!("\ntags: {}", tags.join(", ")));
    }
    if let Some(reference) = str_field(item, "safe_reference") {
        text.push_str(&format!("\nref: {reference}"));
    }
    let citations = array_field(detail, "citations");
    if !citations.is_empty() {
        text.push_str(&format!("\n{} citation(s):", citations.len()));
        for citation in citations.iter().take(10) {
            let title = str_field(citation, "title").unwrap_or("untitled");
            let kind = str_field(citation, "source_kind").unwrap_or("source");
            text.push_str(&format!(
                "\n  - {kind}: {}",
                truncate(title, TITLE_MAX_CHARS)
            ));
            if let Some(reference) = str_field(citation, "safe_reference") {
                text.push_str(&format!(" ({reference})"));
            }
        }
    }
    text
}

pub(super) fn format_post(result: &Value) -> String {
    let item = payload(result);
    let id = str_field(item, "id").unwrap_or("?");
    let feed = str_field(item, "feed_id").unwrap_or("?");
    let sequence = i64_field(item, "sequence").unwrap_or(0);
    format!(
        "[FEED] Posted \"{}\" to feed {feed} (item_id={id}, sequence={sequence}).\n{}",
        truncate(
            str_field(item, "title").unwrap_or("Untitled"),
            TITLE_MAX_CHARS
        ),
        items_hint(feed)
    )
}

pub(super) fn format_follow_state(result: &Value, feed_id: Uuid, verb: &str) -> String {
    let state = payload(result);
    let following = state
        .get("following")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let pinned = state
        .get("pinned_to_sidebar")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let digest = str_field(state, "digest_frequency").unwrap_or("realtime");
    let last_read = i64_field(state, "last_read_sequence").unwrap_or(0);
    let mut text = format!(
        "[FEED] {verb} feed {feed_id}: following={following} pinned={pinned} digest={digest} last_read_sequence={last_read}"
    );
    if let Some(muted) = str_field(state, "muted_until") {
        text.push_str(&format!(" muted_until={muted}"));
    }
    text
}

fn share_line(share: &Value) -> String {
    let id = str_field(share, "id").unwrap_or("?");
    let workspace = str_field(share, "target_workspace_name")
        .or_else(|| str_field(share, "target_workspace_id"))
        .unwrap_or("?");
    let mut target = workspace.to_string();
    if let Some(project) =
        str_field(share, "target_project_name").or_else(|| str_field(share, "target_project_id"))
    {
        target.push_str(&format!(" / {project}"));
    }
    let audience = str_field(share, "audience").unwrap_or("agents");
    let revoked = str_field(share, "revoked_at").is_some();
    format!(
        "[FEED] share {id} → {target} (audience={audience}{})",
        if revoked { ", revoked" } else { "" }
    )
}

pub(super) fn format_shares(result: &Value, feed_id: Uuid) -> String {
    let shares = array_field(payload(result), "shares");
    let mut text = format!("[FEED] {} share(s) on feed {feed_id}.", shares.len());
    for share in shares.iter().take(MAX_LISTED_FEEDS) {
        text.push('\n');
        text.push_str(&share_line(share));
    }
    text.push_str(&format!(
        "\nGrant: feed(action=\"share\", feed_id=\"{feed_id}\", target_workspace_id=\"<uuid>\", audience=\"agents|everyone\")"
    ));
    text
}

pub(super) fn format_share(result: &Value, verb: &str) -> String {
    format!("[FEED] {verb}: {}", share_line(payload(result)))
}

pub(super) fn format_feedback(result: &Value, feed_id: Uuid) -> String {
    let receipt = payload(result);
    let item = str_field(receipt, "item_id").unwrap_or("?");
    let feedback = str_field(receipt, "feedback_type").unwrap_or("?");
    let recorded = receipt
        .get("recorded")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    format!(
        "[FEED] Feedback {feedback} on item {item} in feed {feed_id} (recorded={recorded}). Curation ranking learns from this."
    )
}

pub(super) fn format_curation(result: &Value, feed_id: Uuid) -> String {
    let receipt = payload(result);
    let run = str_field(receipt, "run_id").unwrap_or("?");
    let status = str_field(receipt, "status").unwrap_or("queued");
    format!(
        "[FEED] Curation run {run} {status} for feed {feed_id}. Check: feed(action=\"runs\", feed_id=\"{feed_id}\")"
    )
}

pub(super) fn format_runs(result: &Value, feed_id: Uuid) -> String {
    let runs = array_field(payload(result), "runs");
    let mut text = format!("[FEED] {} curation run(s) for feed {feed_id}.", runs.len());
    for run in runs.iter().take(MAX_LISTED_FEEDS) {
        let id = str_field(run, "id").unwrap_or("?");
        let status = str_field(run, "status").unwrap_or("?");
        let trigger = str_field(run, "trigger").unwrap_or("scheduled");
        let written = i64_field(run, "items_written").unwrap_or(0);
        let seen = i64_field(run, "candidates_seen").unwrap_or(0);
        let when = age_label(
            str_field(run, "finished_at")
                .or_else(|| str_field(run, "started_at"))
                .or_else(|| str_field(run, "created_at")),
        );
        text.push_str(&format!(
            "\n[FEED] run {id}: {status} ({trigger}) — {written} written / {seen} seen [{when}]"
        ));
        if let Some(code) = str_field(run, "error_code") {
            text.push_str(&format!(" error={code}"));
        }
    }
    text
}

fn source_line(source: &Value) -> String {
    let key = str_field(source, "source_key").unwrap_or("?");
    let workspace = str_field(source, "workspace_name")
        .or_else(|| str_field(source, "source_workspace_id"))
        .unwrap_or("?");
    let mut scope = workspace.to_string();
    if let Some(project) =
        str_field(source, "project_name").or_else(|| str_field(source, "source_project_id"))
    {
        scope.push_str(&format!(" / {project}"));
    }
    let origin = str_field(source, "origin").unwrap_or("explicit");
    let relevance = i64_field(source, "relevance_basis_points").unwrap_or(0);
    format!("[FEED] source {scope} (origin={origin}, relevance={relevance}bp) · source_key={key}")
}

pub(super) fn format_sources(result: &Value, feed_id: Uuid) -> String {
    let sources = array_field(payload(result), "sources");
    let mut text = format!(
        "[FEED] {} source scope(s) on feed {feed_id}.",
        sources.len()
    );
    for source in sources.iter().take(MAX_LISTED_FEEDS) {
        text.push('\n');
        text.push_str(&source_line(source));
    }
    text.push_str(&format!(
        "\nAdd: feed(action=\"sources\", feed_id=\"{feed_id}\", source_workspace_id=\"<uuid>\") · Remove: feed(action=\"sources\", feed_id=\"{feed_id}\", source_key=\"<key>\")"
    ));
    text
}

pub(super) fn format_source(result: &Value, verb: &str) -> String {
    format!("[FEED] {verb}: {}", source_line(payload(result)))
}

/// Bounded grounding items from a `/feeds/ground` payload.
pub fn grounding_items(result: &Value) -> Vec<Value> {
    array_field(payload(result), "items")
        .iter()
        .take(GROUNDING_MAX_ITEMS)
        .cloned()
        .collect()
}

fn grounding_line(item: &Value) -> String {
    let feed_name = truncate(str_field(item, "feed_name").unwrap_or("feed"), 60);
    let title = truncate(
        str_field(item, "title").unwrap_or("Untitled"),
        TITLE_MAX_CHARS,
    );
    let feed_id = str_field(item, "feed_id").unwrap_or("?");
    let mut line = format!("[FEED] {feed_name}: {title} — {}", item_summary(item));
    if let Some(why) = str_field(item, "why_it_matters") {
        line.push_str(&format!(" (why: {})", truncate(why, WHY_MAX_CHARS)));
    }
    line.push_str(&format!(" ({})", items_hint(feed_id)));
    line
}

/// `[FEED]` lines for the grounding bundle, bounded to
/// [`GROUNDING_MAX_ITEMS`] lines and [`GROUNDING_MAX_CHARS`] characters.
pub fn format_feed_grounding(items: &[Value]) -> String {
    let mut text = String::new();
    for item in items.iter().take(GROUNDING_MAX_ITEMS) {
        let line = grounding_line(item);
        let projected = text.chars().count() + line.chars().count() + 1;
        if !text.is_empty() && projected > GROUNDING_MAX_CHARS {
            break;
        }
        if text.is_empty() && line.chars().count() > GROUNDING_MAX_CHARS {
            text.push_str(&truncate(&line, GROUNDING_MAX_CHARS));
            text.push('\n');
            break;
        }
        text.push_str(&line);
        text.push('\n');
    }
    text
}

pub(super) fn format_ground(result: &Value, workspace_id: Uuid) -> String {
    let items = grounding_items(result);
    if items.is_empty() {
        return format!(
            "[FEED] No feed items ranked for workspace {workspace_id} yet. feed(action=\"list\") shows what is curated here."
        );
    }
    let mut text = format!("[FEED] {} grounding item(s):\n", items.len());
    text.push_str(&format_feed_grounding(&items));
    text.trim_end().to_string()
}
