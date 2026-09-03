//! Auto-grounding: parse `session_recall` payloads and render `[GROUNDING]` blocks.

use crate::domains::display_title::{
    extract_display_date, extract_display_title, extract_search_keywords, is_placeholder_title,
    normalize_recall_payload,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use std::cmp::Ordering;
use std::time::Duration;

/// One ranked prior-work hit for display in `context()`.
#[derive(Debug, Clone, Serialize)]
pub struct GroundingHit {
    pub kind: String,
    pub title: String,
    pub score: f64,
    /// UUID or natural key for follow-up tool calls.
    pub id_hint: Option<String>,
    /// Source field used for `id_hint`, when known.
    pub id_field: Option<String>,
    /// Optional ISO date for compact display.
    pub date: Option<String>,
    /// Best timestamp found on the source item, used for freshness checks.
    pub source_timestamp: Option<DateTime<Utc>>,
    /// Age in whole days at parse time when a source timestamp exists.
    pub age_days: Option<i64>,
    /// Whether this hit is time-sensitive and older than the local freshness window,
    /// or has been superseded by a newer item.
    pub stale: bool,
    /// Why the hit is stale: `superseded` (a `superseded_by` link or
    /// `status=superseded`), `historical_status`, or `aged`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale_reason: Option<String>,
    /// Whether this hit is a prior snapshot/status claim about unfinished or failed work.
    /// These are useful evidence, but must never be treated as current truth without refresh.
    pub historical_status_claim: bool,
    /// Keywords for search/recall tool hints when title is generic.
    pub search_keywords: String,
}

pub fn grounding_enabled() -> bool {
    !matches!(
        std::env::var("CONTEXTSTREAM_GROUNDING_ENABLED")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "0" | "false" | "no" | "off"
    )
}

pub fn grounding_min_score() -> f64 {
    std::env::var("CONTEXTSTREAM_GROUNDING_MIN_SCORE")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0.4)
}

pub fn grounding_max_hits() -> usize {
    std::env::var("CONTEXTSTREAM_GROUNDING_MAX_HITS")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(5)
        .clamp(1, 12)
}

pub fn grounding_timeout() -> Duration {
    let ms: u64 = std::env::var("CONTEXTSTREAM_GROUNDING_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(3000);
    Duration::from_millis(ms.clamp(500, 10_000))
}

fn item_score(item: &Value) -> f64 {
    item.get("score")
        .and_then(|v| v.as_f64())
        .or_else(|| item.get("similarity").and_then(|v| v.as_f64()))
        .or_else(|| item.get("relevance").and_then(|v| v.as_f64()))
        .unwrap_or(0.0)
}

fn metadata_str(item: &Value, field: &str) -> Option<String> {
    item.get(field)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            item.get("metadata")
                .and_then(|m| m.get(field))
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
}

fn original_type(item: &Value) -> Option<String> {
    metadata_str(item, "original_type").map(|s| s.to_ascii_lowercase())
}

fn event_type(item: &Value) -> Option<String> {
    metadata_str(item, "event_type").map(|s| s.to_ascii_lowercase())
}

/// A hit whose source carries a `superseded_by` link (or `status=superseded`)
/// is stale regardless of age: a newer decision/lesson replaced it.
fn is_superseded(item: &Value) -> bool {
    if metadata_str(item, "superseded_by").is_some() {
        return true;
    }
    metadata_str(item, "status")
        .map(|status| status.eq_ignore_ascii_case("superseded"))
        .unwrap_or(false)
}

fn metadata_time(item: &Value) -> Option<DateTime<Utc>> {
    for field in [
        "occurred_at",
        "captured_at",
        "updated_at",
        "created_at",
        "timestamp",
    ] {
        if let Some(raw) = metadata_str(item, field) {
            if let Ok(ts) = DateTime::parse_from_rfc3339(&raw) {
                return Some(ts.with_timezone(&Utc));
            }
            if let Ok(date) = chrono::NaiveDate::parse_from_str(&raw, "%Y-%m-%d") {
                if let Some(naive) = date.and_hms_opt(0, 0, 0) {
                    return Some(DateTime::<Utc>::from_naive_utc_and_offset(naive, Utc));
                }
            }
        }
    }
    None
}

/// Operational/telemetry kinds that must prove freshness to ground anything.
/// Current servers exclude hook-sourced telemetry at the recall surface;
/// this renderer guard covers older servers and cached payloads.
fn is_operational_noise_kind(kind_or_type: &str) -> bool {
    matches!(
        kind_or_type,
        "operation" | "command_execution" | "file_operation" | "permission_request"
    ) || kind_or_type.contains("command execution")
        || kind_or_type.contains("file operation")
}

/// Freshness window for operational kinds in `[GROUNDING]` (days). A
/// permission prompt or subagent lifecycle event from months ago is never
/// "prior work for this message"; unknown age counts as stale.
const OPERATIONAL_KIND_MAX_AGE_DAYS: i64 = 7;

fn is_time_sensitive_kind(kind: &str) -> bool {
    kind.contains("decision")
        || kind.contains("transcript")
        || kind.contains("conversation")
        || kind.contains("session")
        || kind.contains("snapshot")
        || kind.contains("plan")
        || kind.contains("task")
}

fn is_snapshot_like_kind(kind: &str) -> bool {
    kind.contains("snapshot") || kind == "session_snapshot"
}

pub(crate) fn looks_like_historical_status_claim(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    [
        "unexecuted",
        "unrun",
        "unanswered",
        "no response",
        "no output",
        "zero output",
        "empty placeholder",
        "produced no response",
        "produced no output",
        "produced the empty placeholder",
        "assistant produced no response",
        "assistant produced no output",
        "checklist was not executed",
        "not executed",
        "not run",
        "remains fully unexecuted",
        "remains unanswered",
        "remains unrun",
        "remains incomplete",
        "remains pending",
        "status ping unanswered",
        "silent failure",
    ]
    .iter()
    .any(|phrase| value.contains(phrase))
}

fn append_status_text_field(out: &mut String, item: &Value, field: &str) {
    if let Some(value) = metadata_str(item, field) {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(&value);
    }
}

fn status_claim_text(item: &Value, title: &str, search_keywords: &str) -> String {
    let mut out = String::new();
    if !title.trim().is_empty() {
        out.push_str(title);
    }
    if !search_keywords.trim().is_empty() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(search_keywords);
    }
    for field in [
        "content",
        "content_preview",
        "summary",
        "description",
        "body",
        "text",
        "value",
        "preview",
    ] {
        append_status_text_field(&mut out, item, field);
    }
    out
}

fn stale_after_days(kind: &str) -> Option<i64> {
    if !is_time_sensitive_kind(kind) {
        return None;
    }
    let default_days = if kind.contains("plan") || kind.contains("task") {
        30
    } else {
        14
    };
    Some(
        std::env::var("CONTEXTSTREAM_GROUNDING_STALE_DAYS")
            .ok()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .unwrap_or(default_days)
            .clamp(1, 3650),
    )
}

fn age_days(timestamp: DateTime<Utc>) -> i64 {
    Utc::now()
        .signed_duration_since(timestamp)
        .num_days()
        .max(0)
}

/// Context Feed items carry `feed_id` plus a feed name/item id; they are
/// surfaced through the `feed` tool rather than memory lookups.
fn is_feed_item(item: &Value) -> bool {
    item.get("feed_id").and_then(Value::as_str).is_some()
        && (item.get("feed_name").is_some()
            || item.get("item_id").is_some()
            || item.get("item_kind").is_some())
}

fn classify_kind(item: &Value) -> String {
    if is_feed_item(item) {
        return "feed_item".to_string();
    }
    if let Some(ot) = original_type(item) {
        return ot;
    }
    if let Some(et) = event_type(item) {
        return et;
    }
    item.get("kind")
        .or_else(|| item.get("type"))
        .or_else(|| item.get("source_type"))
        .or_else(|| item.get("result_type"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_else(|| "hit".to_string())
}

fn id_hint(item: &Value) -> (Option<String>, Option<String>) {
    for key in [
        "transcript_id",
        "session_transcript_id",
        "event_id",
        "memory_event_id",
        "doc_id",
        "feed_id",
        "id",
    ] {
        if let Some(v) = metadata_str(item, key) {
            return (Some(v), Some(key.to_string()));
        }
        if let Some(v) = item.get(key).and_then(|x| x.as_i64()) {
            return (Some(v.to_string()), Some(key.to_string()));
        }
    }
    (None, None)
}

/// Extract ranked hits from a `/session/recall` JSON body.
pub fn parse_recall_results(recall: &Value) -> Vec<GroundingHit> {
    let Some(results) = recall.get("results").and_then(|r| r.as_array()) else {
        return Vec::new();
    };

    let min = grounding_min_score();
    let mut hits: Vec<GroundingHit> = results
        .iter()
        .filter_map(|item| {
            let score = item_score(item);
            if score < min {
                return None;
            }
            let (id_hint, id_field) = id_hint(item);
            let kind = classify_kind(item);
            let title = extract_display_title(item);
            let search_keywords = extract_search_keywords(item);
            let historical_status_claim = is_snapshot_like_kind(&kind)
                && looks_like_historical_status_claim(&status_claim_text(
                    item,
                    &title,
                    &search_keywords,
                ));
            let source_timestamp = metadata_time(item);
            let age_days = source_timestamp.map(age_days);
            let operational_noise = std::iter::once(kind.clone())
                .chain(event_type(item))
                .chain(original_type(item))
                .any(|value| is_operational_noise_kind(&value));
            if operational_noise
                && age_days
                    .map(|age| age > OPERATIONAL_KIND_MAX_AGE_DAYS)
                    .unwrap_or(true)
            {
                return None;
            }
            let aged_stale = age_days
                .zip(stale_after_days(&kind))
                .map(|(age, limit)| age > limit)
                .unwrap_or(false);
            let superseded = is_superseded(item);
            let stale = historical_status_claim || aged_stale || superseded;
            let stale_reason = if superseded {
                Some("superseded".to_string())
            } else if historical_status_claim {
                Some("historical_status".to_string())
            } else if aged_stale {
                Some("aged".to_string())
            } else {
                None
            };
            Some(GroundingHit {
                kind,
                title,
                score,
                search_keywords,
                id_hint,
                id_field,
                date: extract_display_date(item),
                source_timestamp,
                age_days,
                stale,
                stale_reason,
                historical_status_claim,
            })
        })
        .collect();

    hits.sort_by(|a, b| {
        a.historical_status_claim
            .cmp(&b.historical_status_claim)
            .then_with(|| a.stale.cmp(&b.stale))
            .then_with(|| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal))
    });
    hits.truncate(grounding_max_hits());
    hits
}

fn append_label_part(prefix: &mut String, part: &str) {
    if !prefix.is_empty() {
        prefix.push_str(" · ");
    }
    prefix.push_str(part);
}

fn format_hit_label(hit: &GroundingHit) -> String {
    let mut prefix = String::new();
    if !hit.kind.is_empty() && hit.kind != "hit" {
        prefix.push_str(&hit.kind.replace('_', " "));
    }
    if let Some(age) = hit.age_days {
        if age == 0 {
            append_label_part(&mut prefix, "today");
        } else if age == 1 {
            append_label_part(&mut prefix, "1d old");
        } else {
            append_label_part(&mut prefix, &format!("{age}d old"));
        }
    } else if let Some(ref date) = hit.date {
        append_label_part(&mut prefix, date);
    }
    if hit.stale {
        append_label_part(&mut prefix, "stale-check");
    }
    if hit.stale_reason.as_deref() == Some("superseded") {
        append_label_part(&mut prefix, "superseded");
    }
    if hit.historical_status_claim {
        append_label_part(&mut prefix, "historical-status");
    }
    if prefix.is_empty() {
        hit.title.chars().take(120).collect()
    } else {
        format!(
            "[{}] {}",
            prefix,
            hit.title.chars().take(100).collect::<String>()
        )
    }
}

fn recall_hint_keywords(hit: &GroundingHit, item_keywords: &str) -> String {
    if is_placeholder_title(&hit.title) || hit.title.starts_with("Memory ") {
        item_keywords.replace('"', "'")
    } else {
        hit.title.replace('"', "'")
    }
}

fn action_hint(hit: &GroundingHit, search_keywords: &str) -> String {
    let k = hit.kind.as_str();
    let id = hit.id_hint.as_deref().unwrap_or("");
    let id_field = hit.id_field.as_deref().unwrap_or("");
    let q = recall_hint_keywords(hit, search_keywords);

    if k == "feed_item" || k.starts_with("feed_") {
        if id_field == "feed_id" && !id.is_empty() {
            return format!("feed(action=\"items\", feed_id=\"{id}\")");
        }
        return format!("feed(action=\"ground\", query=\"{q}\")");
    }
    if is_transcript_id_field(id_field) && !id.is_empty() {
        return format!("memory(action=\"get_transcript\", transcript_id=\"{id}\")");
    }
    if is_doc_id_field(id_field) && !id.is_empty() {
        return format!("memory(action=\"get_doc\", doc_id=\"{id}\")");
    }

    if (k.contains("transcript") || k.contains("conversation") || k == "session")
        && !is_event_id_field(id_field)
    {
        if !id.is_empty() {
            return format!("memory(action=\"get_transcript\", transcript_id=\"{id}\")");
        }
        return format!("memory(action=\"search_transcripts\", query=\"{q}\")");
    }
    if k.contains("snapshot") || k.contains("session_snapshot") {
        if !id.is_empty() {
            return format!("memory(action=\"get_event\", event_id=\"{id}\")");
        }
        return "memory(action=\"list_events\", event_type=\"session_snapshot\")".to_string();
    }
    if k.contains("decision") {
        return format!("memory(action=\"decisions\", query=\"{q}\")");
    }
    if k.contains("lesson") {
        return format!("session(action=\"get_lessons\", query=\"{q}\")");
    }
    if k.contains("doc") || k == "project_doc" || k.contains("document") {
        if !id.is_empty() {
            return format!("memory(action=\"get_doc\", doc_id=\"{id}\")");
        }
        return format!("memory(action=\"list_docs\", query=\"{q}\")");
    }
    if k.contains("linear") {
        return format!("integration(provider=\"linear\", action=\"issues\", query=\"{q}\")");
    }
    if k.contains("jira") {
        return format!("integration(provider=\"jira\", action=\"issues\", query=\"{q}\")");
    }
    if k.contains("figma") {
        return format!("integration(provider=\"figma\", action=\"files\", query=\"{q}\")");
    }

    if !id.is_empty()
        && (is_event_id_field(id_field) || (id_field == "id" && is_memory_event_kind(k)))
    {
        return format!("memory(action=\"get_event\", event_id=\"{id}\")");
    }

    if is_placeholder_title(&q) || q.starts_with("Memory ") || q.starts_with("Item ") {
        return "session(action=\"ground\", user_message=\"<same as current task>\")".to_string();
    }

    format!("session(action=\"recall\", query=\"{q}\")")
}

fn is_transcript_id_field(field: &str) -> bool {
    matches!(field, "transcript_id" | "session_transcript_id")
}

fn is_event_id_field(field: &str) -> bool {
    matches!(field, "event_id" | "memory_event_id")
}

fn is_doc_id_field(field: &str) -> bool {
    field == "doc_id"
}

fn is_memory_event_kind(kind: &str) -> bool {
    matches!(
        kind,
        "event"
            | "memory_event"
            | "note"
            | "insight"
            | "preference"
            | "uncategorized"
            | "general"
            | "manual_note"
            | "implementation"
            | "operation"
            | "command_execution"
            | "file_operation"
            | "task"
            | "bug"
            | "feature"
            | "plan"
            | "correction"
            | "warning"
            | "frustration"
            | "session_snapshot"
            | "standup"
            | "status_update"
            | "question"
            | "approval"
            | "feedback"
            | "discovery"
            | "achievement"
            | "conversation"
            | "linear_issue"
            | "jira_issue"
            | "figma_file"
            | "figma_comment"
    )
}

/// Render the `[GROUNDING]` user-visible block (empty if no hits).
pub fn format_grounding_block(hits: &[GroundingHit], compact: bool) -> String {
    if hits.is_empty() {
        return String::new();
    }

    let mut out = String::new();
    if compact {
        out.push_str("\n[GROUNDING] Prior work for this message — read BEFORE code search OR clarifying questions:");
        for (i, hit) in hits.iter().enumerate() {
            let hint = action_hint(hit, &hit.search_keywords);
            out.push_str(&format!(
                "\n  {}. {} (score {:.2}) → {}",
                i + 1,
                format_hit_label(hit),
                hit.score,
                hint
            ));
            if hit.historical_status_claim {
                out.push_str(
                    " [historical status claim; verify newer work before treating as current]",
                );
            } else if hit.stale_reason.as_deref() == Some("superseded") {
                out.push_str(" [superseded; follow the successor instead]");
            } else if hit.stale {
                out.push_str(" [verify freshness before relying]");
            }
        }
        out.push_str("\n  RULE: if a hit answers a question you were about to ask the user (which env? which region? which DB? what's the deadline?), READ IT INSTEAD. Asking the user something a runbook/decision/doc/transcript already records is wasted turns.");
        out.push_str("\n  FRESHNESS: time-sensitive hits marked stale-check are evidence, not current truth; refresh with the suggested tool call before planning or implementing from them.");
        out.push_str("\n  HISTORICAL-STATUS: no-output/unexecuted/unanswered snapshots describe prior failure state only; verify newer commits, events, or probes before treating them as current.");
        out.push_str("\n  Skip if irrelevant. session(action=\"ground\", user_message=\"...\") for a one-shot bundle.\n");
    } else {
        out.push_str(
            "🧭 [GROUNDING] Prior work matching your message — read these BEFORE searching code OR asking clarifying questions:\n",
        );
        for (i, hit) in hits.iter().enumerate() {
            let hint = action_hint(hit, &hit.search_keywords);
            out.push_str(&format!(
                "{}. **{}** (kind: `{}`, score: {:.2})\n   → {}\n",
                i + 1,
                format_hit_label(hit),
                hit.kind,
                hit.score,
                hint
            ));
            if hit.historical_status_claim {
                out.push_str("   ! Historical status claim: prior no-output/unexecuted/unanswered evidence only. Verify newer work before treating it as current.\n");
            } else if hit.stale_reason.as_deref() == Some("superseded") {
                out.push_str("   ! Superseded: a newer item replaced this one. Use the successor, not this hit.\n");
            } else if hit.stale {
                out.push_str("   ! Time-sensitive and stale: refresh this source before using it to plan or implement.\n");
            }
        }
        out.push_str(
            "\n⚠️ Anti-pattern: asking the user a fact a runbook / decision / doc / transcript already records.\n",
        );
        out.push_str(
            "If a hit above answers a question you were about to ask (which env? which DB? which region? when's the deadline?), READ THE HIT INSTEAD of asking the user. Each redundant clarifying turn costs the user time and breaks flow.\n",
        );
        out.push_str("\nFreshness rule: decisions, transcript continuity, snapshots, plans, and tasks are time-sensitive. When a hit is marked stale-check, use it as a lead and refresh it before relying on it for planning or implementation. Historical-status hits are prior failure/status evidence, not current truth.\n");
        out.push_str("\nSkip if truly irrelevant. Reading 1–2 anchors here often avoids both a multi-pass repo search and a clarifying-question round trip.\n");
        out.push_str("One-shot bundle: `session(action=\"ground\", user_message=\"...\")`.\n\n");
    }
    out
}

/// Build the cross-process summary used by hook nudges.
pub fn grounding_summary(hits: &[GroundingHit]) -> mcp_session::grounding_state::GroundingSummary {
    let mut top_kinds: Vec<String> = Vec::new();
    let mut newest_source_at: Option<DateTime<Utc>> = None;
    let mut oldest_source_at: Option<DateTime<Utc>> = None;

    for hit in hits {
        if !hit.kind.is_empty() && hit.kind != "hit" && !top_kinds.contains(&hit.kind) {
            top_kinds.push(hit.kind.clone());
            if top_kinds.len() >= 4 {
                break;
            }
        }
    }

    for hit in hits {
        let Some(source_at) = hit.source_timestamp else {
            continue;
        };
        newest_source_at = Some(
            newest_source_at
                .map(|current| current.max(source_at))
                .unwrap_or(source_at),
        );
        oldest_source_at = Some(
            oldest_source_at
                .map(|current| current.min(source_at))
                .unwrap_or(source_at),
        );
    }

    mcp_session::grounding_state::GroundingSummary {
        hit_count: hits.len() as u32,
        decision_count: hits
            .iter()
            .filter(|hit| hit.kind.contains("decision"))
            .count() as u32,
        stale_count: hits.iter().filter(|hit| hit.stale).count() as u32,
        newest_source_at: newest_source_at.map(|ts| ts.to_rfc3339()),
        oldest_source_at: oldest_source_at.map(|ts| ts.to_rfc3339()),
        top_kinds,
    }
}

/// Parse recall after normalizing nested metadata (call before `parse_recall_results`).
pub fn parse_recall_results_normalized(mut recall: Value) -> Vec<GroundingHit> {
    normalize_recall_payload(&mut recall);
    parse_recall_results(&recall)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn drops_subthreshold_hits() {
        let recall = json!({
            "results": [
                { "title": "High", "score": 0.9, "metadata": { "original_type": "transcript" }, "transcript_id": "t1" },
                { "title": "Low", "score": 0.1, "metadata": { "original_type": "transcript" } }
            ]
        });
        let hits = parse_recall_results(&recall);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "High");
    }

    #[test]
    fn metadata_only_title_not_untitled() {
        let recall = json!({
            "results": [{
                "score": 0.9,
                "metadata": {
                    "title": "Untitled",
                    "content_preview": "LLM audit: Gemini routing and Bedrock removal",
                    "event_type": "preference",
                    "occurred_at": "2026-05-12T10:00:00Z"
                }
            }]
        });
        let hits = parse_recall_results_normalized(recall);
        assert_eq!(
            hits[0].title,
            "LLM audit: Gemini routing and Bedrock removal"
        );
        assert_eq!(hits[0].date.as_deref(), Some("2026-05-12"));
        let s = format_grounding_block(&hits, true);
        assert!(!s.contains("→ session(action=\"recall\", query=\"Untitled\")"));
        assert!(s.contains("preference"));
    }

    #[test]
    fn stale_decision_is_labeled_for_refresh() {
        let recall = json!({
            "results": [{
                "score": 0.9,
                "metadata": {
                    "title": "Old deployment target decision",
                    "event_type": "decision",
                    "occurred_at": "2000-01-01T00:00:00Z"
                }
            }]
        });
        let hits = parse_recall_results_normalized(recall);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].stale);
        assert!(hits[0].age_days.unwrap_or_default() > 14);

        let compact = format_grounding_block(&hits, true);
        assert!(compact.contains("stale-check"));
        assert!(compact.contains("verify freshness before relying"));
        assert!(compact.contains("FRESHNESS"));
    }

    #[test]
    fn stale_operational_telemetry_never_grounds() {
        // The exact regression from 2026-07-01: months-old hook telemetry
        // (permission prompts, stop checkpoints, subagent lifecycle) surfaced
        // as TOP grounding hits at 0.87–0.95, burying real prior work.
        let recall = json!({
            "results": [
                { "score": 0.95, "title": "Permission request",
                  "metadata": { "event_type": "operation", "occurred_at": "2026-02-10T09:00:00Z" } },
                { "score": 0.92, "title": "Stop checkpoint",
                  "metadata": { "event_type": "command_execution", "occurred_at": "2026-04-25T09:00:00Z" } },
                { "score": 0.87, "title": "Subagent finished: explore",
                  "metadata": { "event_type": "file_operation", "occurred_at": "2026-02-10T09:00:00Z" } },
                { "score": 0.82, "title": "Route fast tier by role",
                  "metadata": { "event_type": "decision", "occurred_at": "2026-06-28T09:00:00Z" } }
            ]
        });
        let hits = parse_recall_results(&recall);
        assert_eq!(hits.len(), 1, "only the decision should survive");
        assert_eq!(hits[0].title, "Route fast tier by role");
    }

    #[test]
    fn operational_telemetry_without_timestamp_is_dropped() {
        // Unknown age counts as stale for telemetry — it must prove freshness.
        let recall = json!({
            "results": [{
                "score": 0.95,
                "title": "Permission request",
                "metadata": { "event_type": "operation" }
            }]
        });
        assert!(parse_recall_results(&recall).is_empty());
    }

    #[test]
    fn fresh_operational_event_still_grounds() {
        // A telemetry event from moments ago can be legitimate continuity
        // context (e.g. "the command you just ran"); only aged ones drop.
        let recall = json!({
            "results": [{
                "score": 0.9,
                "title": "Command executed",
                "metadata": {
                    "event_type": "operation",
                    "occurred_at": Utc::now().to_rfc3339()
                }
            }]
        });
        let hits = parse_recall_results(&recall);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn no_output_snapshot_is_historical_status_even_when_recent() {
        let now = Utc::now().to_rfc3339();
        let recall = json!({
            "results": [{
                "score": 0.99,
                "title": "Deployment request remains fully unexecuted",
                "event_type": "session_snapshot",
                "event_id": "snapshot-1",
                "occurred_at": now,
                "content": "Assistant produced no response and the checklist was not executed."
            }]
        });

        let hits = parse_recall_results(&recall);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].historical_status_claim);
        assert!(hits[0].stale);
        assert_eq!(hits[0].age_days, Some(0));

        let compact = format_grounding_block(&hits, true);
        assert!(compact.contains("historical-status"));
        assert!(compact.contains("historical status claim"));
        assert!(compact.contains("verify newer work"));
        assert!(compact.contains("HISTORICAL-STATUS"));
    }

    #[test]
    fn historical_status_snapshots_rank_below_non_status_hits() {
        let now = Utc::now().to_rfc3339();
        let recall = json!({
            "results": [
                {
                    "score": 0.99,
                    "title": "Setup request remains fully unexecuted",
                    "event_type": "session_snapshot",
                    "event_id": "snapshot-1",
                    "occurred_at": now,
                    "content": "Assistant produced no output."
                },
                {
                    "score": 0.70,
                    "title": "Completed setup and release verification",
                    "event_type": "implementation",
                    "event_id": "impl-1",
                    "occurred_at": now
                }
            ]
        });

        let hits = parse_recall_results(&recall);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "Completed setup and release verification");
        assert!(!hits[0].historical_status_claim);
        assert!(hits[1].historical_status_claim);
    }

    #[test]
    fn superseded_decision_is_stale_with_reason_even_when_recent() {
        let today = Utc::now().format("%Y-%m-%dT00:00:00Z").to_string();
        let recall = json!({
            "results": [{
                "score": 0.9,
                "metadata": {
                    "title": "Use Postgres for the ledger",
                    "event_type": "decision",
                    "occurred_at": today,
                    "superseded_by": "11111111-1111-4111-8111-111111111111"
                }
            }, {
                "score": 0.8,
                "id": "22222222-2222-4222-8222-222222222222",
                "status": "superseded",
                "metadata": {
                    "title": "Use Redis for the ledger",
                    "event_type": "decision",
                    "occurred_at": today
                }
            }]
        });
        let hits = parse_recall_results_normalized(recall);
        assert_eq!(hits.len(), 2);
        for hit in &hits {
            assert!(hit.stale, "{hit:?}");
            assert_eq!(hit.stale_reason.as_deref(), Some("superseded"));
        }
        let compact = format_grounding_block(&hits, true);
        assert!(compact.contains("superseded"));
        assert!(compact.contains("[superseded; follow the successor instead]"));
        let serialized = serde_json::to_value(&hits[0]).unwrap();
        assert_eq!(serialized["stale"], true);
        assert_eq!(serialized["stale_reason"], "superseded");
    }

    #[test]
    fn durable_doc_with_old_date_is_not_marked_stale() {
        let recall = json!({
            "results": [{
                "score": 0.9,
                "metadata": {
                    "title": "Legacy runbook",
                    "original_type": "doc",
                    "occurred_at": "2000-01-01T00:00:00Z"
                }
            }]
        });
        let hits = parse_recall_results_normalized(recall);
        assert_eq!(hits.len(), 1);
        assert!(!hits[0].stale);

        let compact = format_grounding_block(&hits, true);
        let hit_line = compact
            .lines()
            .find(|line| line.contains("Legacy runbook"))
            .expect("grounding block should include the doc hit");
        assert!(!hit_line.contains("stale-check"));
    }

    #[test]
    fn format_empty_when_no_hits() {
        assert!(format_grounding_block(&[], true).is_empty());
    }

    #[test]
    fn format_includes_action_hints() {
        let hits = vec![GroundingHit {
            kind: "transcript".to_string(),
            title: "Fix auth".to_string(),
            score: 0.8,
            id_hint: Some("abc".to_string()),
            id_field: Some("transcript_id".to_string()),
            date: None,
            source_timestamp: None,
            age_days: None,
            stale: false,
            stale_reason: None,
            historical_status_claim: false,
            search_keywords: "Fix auth".to_string(),
        }];
        let s = format_grounding_block(&hits, true);
        assert!(s.contains("[GROUNDING]"));
        assert!(s.contains("get_transcript"));
        assert!(s.contains("abc"));
    }

    #[test]
    fn unknown_generic_id_falls_back_to_recall_not_get_event() {
        let recall = json!({
            "results": [
                { "title": "Stale search hit", "score": 0.9, "kind": "hit", "id": "not-an-event" }
            ]
        });
        let hits = parse_recall_results(&recall);
        let s = format_grounding_block(&hits, true);
        assert!(s.contains("session(action=\"recall\"") || s.contains("session(action=\"ground\""));
        assert!(!s.contains("get_event"));
    }

    #[test]
    fn explicit_event_id_still_uses_get_event() {
        let recall = json!({
            "results": [
                { "title": "Saved note", "score": 0.9, "event_type": "note", "event_id": "evt-1" }
            ]
        });
        let hits = parse_recall_results(&recall);
        let s = format_grounding_block(&hits, true);
        assert!(s.contains("memory(action=\"get_event\", event_id=\"evt-1\")"));
    }

    #[test]
    fn grounding_block_warns_against_redundant_clarifying_questions() {
        let hits = vec![GroundingHit {
            kind: "doc".to_string(),
            title: "Prod DB migration runbook".to_string(),
            score: 0.95,
            id_hint: Some("doc-1".to_string()),
            id_field: Some("doc_id".to_string()),
            date: Some("2026-05-10".to_string()),
            source_timestamp: None,
            age_days: None,
            stale: false,
            stale_reason: None,
            historical_status_claim: false,
            search_keywords: "Prod DB migration runbook".to_string(),
        }];

        let verbose = format_grounding_block(&hits, false);
        assert!(
            verbose.contains("clarifying question") || verbose.contains("clarifying questions"),
            "verbose grounding block must mention clarifying questions"
        );
        assert!(
            verbose.to_lowercase().contains("anti-pattern"),
            "verbose grounding block must call out the anti-pattern"
        );
        assert!(
            verbose.contains("READ THE HIT INSTEAD"),
            "verbose grounding block must give a directive to read the hit"
        );

        let compact = format_grounding_block(&hits, true);
        assert!(
            compact.to_lowercase().contains("clarifying question"),
            "compact grounding block must mention clarifying questions"
        );
        assert!(
            compact.contains("READ IT INSTEAD"),
            "compact grounding block must give a directive to read the hit"
        );
    }

    #[test]
    fn conversation_event_id_uses_get_event_not_get_transcript() {
        let recall = json!({
            "results": [
                { "title": "Saved conversation event", "score": 0.9, "event_type": "conversation", "event_id": "evt-2" }
            ]
        });
        let hits = parse_recall_results(&recall);
        let s = format_grounding_block(&hits, true);
        assert!(s.contains("memory(action=\"get_event\", event_id=\"evt-2\")"));
        assert!(!s.contains("get_transcript"));
    }

    #[test]
    fn feed_item_hits_route_to_the_feed_tool() {
        let recall = json!({
            "results": [
                {
                    "score": 0.95,
                    "feed_id": "11111111-1111-1111-1111-111111111111",
                    "feed_name": "Engineering",
                    "item_id": "22222222-2222-2222-2222-222222222222",
                    "item_kind": "decision",
                    "title": "Auth refactor decided",
                    "summary": "Rotating JWTs everywhere",
                    "occurred_at": "2099-01-01T00:00:00Z"
                },
                { "title": "Feed digest", "score": 0.9, "kind": "feed_item" }
            ]
        });
        let hits = parse_recall_results(&recall);
        assert_eq!(hits.len(), 2);
        assert!(hits.iter().all(|hit| hit.kind == "feed_item"));
        assert_eq!(hits[0].id_field.as_deref(), Some("feed_id"));

        let block = format_grounding_block(&hits, true);
        assert!(block
            .contains("feed(action=\"items\", feed_id=\"11111111-1111-1111-1111-111111111111\")"));
        assert!(block.contains("feed(action=\"ground\", query=\""));
        assert!(!block.contains("get_event"));
    }
}
