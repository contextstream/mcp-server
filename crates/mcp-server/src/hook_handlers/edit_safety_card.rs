//! Pre-edit structural safety card for `PreToolUse` hooks.
//!
//! Before an agent edits a source file, looks up its 1-hop graph blast radius,
//! cross-service HTTP/event consumers, covering tests, and linked decisions/lessons.
//! Uses a local 5-minute TTL cache for sub-millisecond hits and a strict 150 ms
//! timeout on cold misses, failing open silently so edits are never blocked or slowed.

use super::common::ApiConfig;
use super::durable_capture::{state_dir, with_locked_json};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

const CACHE_TTL_SECS: i64 = 300;
const MAX_CACHE_ENTRIES: usize = 256;
const COLD_FETCH_TIMEOUT: Duration = Duration::from_millis(150);

const SAFETY_CARD_EXTENSIONS: &[&str] = &[
    "rs", "ts", "tsx", "js", "jsx", "mjs", "cjs", "py", "go", "java", "kt", "kts", "rb", "php",
    "cs", "cpp", "cc", "cxx", "c", "h", "hpp", "swift", "scala", "ex", "exs", "zig", "sql",
    "proto", "graphql", "gql", "vue", "svelte", "prisma", "tf",
];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SafetyCardCache {
    #[serde(default)]
    entries: BTreeMap<String, CachedSafetyCard>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedSafetyCard {
    cached_at_epoch: i64,
    payload: Option<ImpactPayload>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct ImpactPayload {
    #[serde(default)]
    pub(crate) directly_affected: usize,
    #[serde(default)]
    pub(crate) affected_modules: Vec<String>,
    #[serde(default)]
    pub(crate) affected_functions: Vec<String>,
    #[serde(default)]
    pub(crate) risk_level: String,
    #[serde(default)]
    pub(crate) linked_knowledge: Vec<LinkedKnowledgeItem>,
    #[serde(default)]
    pub(crate) cross_service_edges: Vec<CrossServiceEdgeItem>,
    #[serde(default)]
    pub(crate) covering_tests: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct LinkedKnowledgeItem {
    #[serde(default)]
    pub(crate) node_type: String,
    #[serde(default)]
    pub(crate) summary: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct CrossServiceEdgeItem {
    #[serde(default)]
    pub(crate) direction: String,
    #[serde(default)]
    pub(crate) call_type: String,
    #[serde(default)]
    pub(crate) via: String,
    #[serde(default)]
    pub(crate) peer_name: String,
}

impl ImpactPayload {
    pub(crate) fn has_signal(&self) -> bool {
        self.directly_affected > 0
            || !self.affected_modules.is_empty()
            || !self.affected_functions.is_empty()
            || !self.linked_knowledge.is_empty()
            || !self.cross_service_edges.is_empty()
            || !self.covering_tests.is_empty()
    }
}

pub(crate) fn is_enabled() -> bool {
    !matches!(
        std::env::var("CONTEXTSTREAM_EDIT_SAFETY_CARD")
            .ok()
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("0" | "false" | "off" | "no")
    )
}

pub(crate) fn is_code_edit_tool(tool_lower: &str) -> bool {
    matches!(
        tool_lower,
        "edit"
            | "multiedit"
            | "write"
            | "notebookedit"
            | "edit_file"
            | "write_file"
            | "write_to_file"
            | "replace_file_content"
            | "multi_replace_file_content"
    )
}

pub(crate) fn extract_edit_target_rel_path(tool_input: &Value, cwd: &str) -> Option<String> {
    let raw = [
        "file_path",
        "path",
        "target_file",
        "TargetFile",
        "file",
    ]
    .iter()
    .find_map(|key| tool_input.get(*key).and_then(Value::as_str))
    .map(str::trim)
    .filter(|s| !s.is_empty())?;

    let rel = to_repo_relative(raw, cwd)?;
    let ext = Path::new(&rel)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)?;
    if !SAFETY_CARD_EXTENSIONS.contains(&ext.as_str()) {
        return None;
    }
    Some(rel)
}

fn to_repo_relative(raw: &str, cwd: &str) -> Option<String> {
    let path = Path::new(raw);
    let rel = if path.is_absolute() {
        let cwd_path = Path::new(cwd);
        if let Ok(stripped) = path.strip_prefix(cwd_path) {
            stripped.to_path_buf()
        } else {
            let root = cwd_path.canonicalize().ok()?;
            let target = path.canonicalize().ok()?;
            target.strip_prefix(&root).ok()?.to_path_buf()
        }
    } else {
        path.to_path_buf()
    };
    let s = rel
        .to_string_lossy()
        .replace('\\', "/")
        .trim_start_matches("./")
        .to_string();
    if s.is_empty() || s.starts_with("..") {
        None
    } else {
        Some(s)
    }
}

fn cache_path() -> Option<PathBuf> {
    state_dir().map(|dir| dir.join("edit-safety-cache.json"))
}

fn now_epoch() -> i64 {
    chrono::Utc::now().timestamp()
}

fn read_cached(path: &Path, key: &str, now: i64) -> Option<Option<ImpactPayload>> {
    let bytes = std::fs::read(path).ok()?;
    let cache: SafetyCardCache = serde_json::from_slice(&bytes).ok()?;
    let entry = cache.entries.get(key)?;
    if now.saturating_sub(entry.cached_at_epoch) <= CACHE_TTL_SECS {
        Some(entry.payload.clone())
    } else {
        None
    }
}

fn write_cached(path: &Path, key: &str, payload: Option<ImpactPayload>, now: i64) {
    let _ = with_locked_json(path, |cache: &mut SafetyCardCache| {
        cache
            .entries
            .retain(|_, v| now.saturating_sub(v.cached_at_epoch) <= CACHE_TTL_SECS);
        while cache.entries.len() >= MAX_CACHE_ENTRIES {
            let Some(oldest) = cache
                .entries
                .iter()
                .min_by_key(|(_, v)| v.cached_at_epoch)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            cache.entries.remove(&oldest);
        }
        cache.entries.insert(
            key.to_string(),
            CachedSafetyCard {
                cached_at_epoch: now,
                payload,
            },
        );
    });
}

fn parse_impact_response(value: &Value) -> Option<ImpactPayload> {
    let data = value.get("data").unwrap_or(value);
    serde_json::from_value::<ImpactPayload>(data.clone()).ok()
}

/// Format a compact, scannable pre-edit safety card. Returns `None` when there is no
/// meaningful blast radius, cross-service dependency, covering test, or linked knowledge.
pub(crate) fn format_safety_card(
    rel_path: &str,
    payload: &ImpactPayload,
    graph_read_tool: &str,
) -> Option<String> {
    if !payload.has_signal() {
        return None;
    }

    let risk = if payload.risk_level.trim().is_empty() {
        "Info"
    } else {
        payload.risk_level.trim()
    };
    let affected = payload
        .directly_affected
        .max(payload.affected_modules.len());

    let mut lines = Vec::new();
    lines.push(format!(
        "[EDIT_SAFETY_CARD] {rel_path} (risk: {risk}, {affected} affected)"
    ));

    let mut dep_refs: Vec<&str> = payload
        .affected_modules
        .iter()
        .map(String::as_str)
        .filter(|m| !m.is_empty() && *m != rel_path)
        .collect();
    dep_refs.sort_unstable();
    dep_refs.dedup();
    if !dep_refs.is_empty() {
        let shown: Vec<&str> = dep_refs.iter().copied().take(4).collect();
        let extra = dep_refs.len().saturating_sub(shown.len());
        let suffix = if extra > 0 {
            format!(" (+{extra} more)")
        } else {
            String::new()
        };
        lines.push(format!("- Dependents: {}{suffix}", shown.join(", ")));
    }

    if !payload.cross_service_edges.is_empty() {
        let shown: Vec<String> = payload
            .cross_service_edges
            .iter()
            .take(3)
            .map(|e| {
                let arrow = if e.direction == "in" { "<-" } else { "->" };
                if e.via.is_empty() {
                    format!("{} {} {} {}", e.direction, e.call_type, arrow, e.peer_name)
                } else {
                    format!(
                        "{} {}:{} {} {}",
                        e.direction, e.call_type, e.via, arrow, e.peer_name
                    )
                }
            })
            .collect();
        lines.push(format!("- Cross-service: {}", shown.join("; ")));
    }

    if !payload.covering_tests.is_empty() {
        let shown: Vec<&str> = payload
            .covering_tests
            .iter()
            .map(String::as_str)
            .take(3)
            .collect();
        lines.push(format!("- Covering tests: {}", shown.join(", ")));
    }

    if !payload.linked_knowledge.is_empty() {
        let shown: Vec<String> = payload
            .linked_knowledge
            .iter()
            .take(2)
            .map(|k| {
                let kind = if k.node_type.is_empty() {
                    "note"
                } else {
                    k.node_type.as_str()
                };
                let summary: String = k.summary.trim().chars().take(100).collect();
                format!("[{kind}] {summary}")
            })
            .collect();
        lines.push(format!("- Linked knowledge: {}", shown.join(" | ")));
    }

    lines.push(format!(
        "Before signature/contract edits, inspect full blast radius with {graph_read_tool}(action=\"impact\", target_type=\"module\", target_id=\"{rel_path}\")."
    ));

    Some(lines.join("\n"))
}

/// Fetch (cached or tightly budgeted) pre-edit safety card for a file edit tool call.
pub(crate) async fn maybe_safety_card_for_edit(
    input: &Value,
    cwd: &str,
    tool_lower: &str,
    tool_input: &Value,
    config: &ApiConfig,
    graph_read_tool: &str,
) -> Option<String> {
    if !is_enabled() || !is_code_edit_tool(tool_lower) || !config.is_configured() {
        return None;
    }
    let project_id = config.project_id.as_deref()?.trim();
    let workspace_id = config.workspace_id.as_deref()?.trim();
    if project_id.is_empty() || workspace_id.is_empty() {
        return None;
    }
    let rel_path = extract_edit_target_rel_path(tool_input, cwd)?;

    // Emit at most once per file per session to prevent noise on consecutive edits.
    let once_token = format!("edit_safety:{rel_path}");
    if !super::mid_run_notices::claim_once_per_session(input, cwd, &once_token) {
        return None;
    }

    let cache_key = format!("{project_id}:{rel_path}");
    let now = now_epoch();
    let cache_file = cache_path();
    if let Some(ref path) = cache_file {
        if let Some(cached) = read_cached(path, &cache_key, now) {
            return cached.and_then(|p| format_safety_card(&rel_path, &p, graph_read_tool));
        }
    }

    let client = reqwest::Client::builder()
        .timeout(COLD_FETCH_TIMEOUT)
        .connect_timeout(Duration::from_millis(80))
        .build()
        .ok()?;
    let url = format!(
        "{}/api/v1/graph/impact-analysis",
        config.api_url.trim_end_matches('/')
    );
    let body = serde_json::json!({
        "change_type": "modify_signature",
        "target_id": rel_path,
        "workspace_id": workspace_id,
        "project_id": project_id,
        "max_depth": 1,
        "target": {
            "id": rel_path,
            "type": "module",
        }
    });
    let mut req = client.post(&url).json(&body);
    if !config.api_key.trim().is_empty() {
        req = req.header("X-API-Key", config.api_key.trim());
    }
    let resp = tokio::time::timeout(COLD_FETCH_TIMEOUT, req.send())
        .await
        .ok()?
        .ok()?;
    if !resp.status().is_success() {
        if let Some(ref path) = cache_file {
            // Cache empty negative result briefly so repeated edits to unindexed files stay <1ms.
            write_cached(path, &cache_key, None, now);
        }
        return None;
    }
    let json_val: Value = resp.json().await.ok()?;
    let payload = parse_impact_response(&json_val);
    if let Some(ref path) = cache_file {
        write_cached(path, &cache_key, payload.clone(), now);
    }
    payload.and_then(|p| format_safety_card(&rel_path, &p, graph_read_tool))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_repo_relative_code_paths_and_ignores_non_code_or_external_files() {
        let input = serde_json::json!({ "file_path": "/repo/src/billing/charge.rs" });
        assert_eq!(
            extract_edit_target_rel_path(&input, "/repo").as_deref(),
            Some("src/billing/charge.rs")
        );

        let relative = serde_json::json!({ "path": "./internal/auth/session.go" });
        assert_eq!(
            extract_edit_target_rel_path(&relative, "/repo").as_deref(),
            Some("internal/auth/session.go")
        );

        let markdown = serde_json::json!({ "file_path": "/repo/docs/README.md" });
        assert!(extract_edit_target_rel_path(&markdown, "/repo").is_none());

        let external = serde_json::json!({ "file_path": "/tmp/other/file.rs" });
        assert!(extract_edit_target_rel_path(&external, "/repo").is_none());
    }

    #[test]
    fn formats_compact_safety_card_with_cross_service_tests_and_decisions() {
        let payload = ImpactPayload {
            directly_affected: 12,
            affected_modules: vec![
                "src/api/checkout.rs".to_string(),
                "src/jobs/retry.rs".to_string(),
            ],
            affected_functions: vec![],
            risk_level: "High".to_string(),
            linked_knowledge: vec![LinkedKnowledgeItem {
                node_type: "decision".to_string(),
                summary: "Always idempotency-key Stripe charges".to_string(),
            }],
            cross_service_edges: vec![CrossServiceEdgeItem {
                direction: "out".to_string(),
                call_type: "event".to_string(),
                via: "order.paid".to_string(),
                peer_name: "notify_customer".to_string(),
            }],
            covering_tests: vec!["tests/billing_charge_test.rs".to_string()],
        };
        let card = format_safety_card(
            "src/billing/charge.rs",
            &payload,
            "mcp__contextstream__graph_read",
        )
        .expect("card should render");
        assert!(card.contains("[EDIT_SAFETY_CARD] src/billing/charge.rs (risk: High, 12 affected)"));
        assert!(card.contains("src/api/checkout.rs, src/jobs/retry.rs"));
        assert!(card.contains("out event:order.paid -> notify_customer"));
        assert!(card.contains("tests/billing_charge_test.rs"));
        assert!(card.contains("[decision] Always idempotency-key Stripe charges"));
        assert!(card.contains("mcp__contextstream__graph_read(action=\"impact\""));
    }

    #[test]
    fn omits_safety_card_when_impact_has_no_signal() {
        let empty = ImpactPayload::default();
        assert!(format_safety_card("src/leaf.rs", &empty, "graph_read").is_none());
    }

    #[test]
    fn local_cache_roundtrips_and_expires() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("edit-safety-cache.json");
        let payload = ImpactPayload {
            directly_affected: 3,
            risk_level: "Medium".to_string(),
            ..Default::default()
        };
        write_cached(&path, "proj:src/a.rs", Some(payload.clone()), 1_000);
        let hit = read_cached(&path, "proj:src/a.rs", 1_100).expect("fresh hit");
        assert_eq!(hit.unwrap().directly_affected, 3);
        assert!(read_cached(&path, "proj:src/a.rs", 1_400).is_none());
    }
}
