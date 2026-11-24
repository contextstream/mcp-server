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
    #[serde(default)]
    pub(crate) co_change_peers: Vec<CoChangePeerItem>,
    #[serde(default)]
    pub(crate) owners: Vec<String>,
    #[serde(default)]
    pub(crate) open_code_drift: Vec<CodeDriftItem>,
    #[serde(default)]
    pub(crate) hotspot_risk_score: Option<f64>,
    #[serde(default)]
    pub(crate) hotspot_risk_tier: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct CoChangePeerItem {
    #[serde(default)]
    pub(crate) file_path: String,
    #[serde(default)]
    pub(crate) co_change_count: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct CodeDriftItem {
    #[serde(default)]
    pub(crate) line: i32,
    #[serde(default)]
    pub(crate) old_value: String,
    #[serde(default)]
    pub(crate) new_value: Option<String>,
    #[serde(default)]
    pub(crate) kind: String,
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
            || !self.co_change_peers.is_empty()
            || !self.owners.is_empty()
            || !self.open_code_drift.is_empty()
            || self.hotspot_risk_score.unwrap_or(0.0) >= 45.0
    }
}

fn sanitize_inline(raw: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(raw.len().min(max_chars));
    let mut prev_space = false;
    for ch in raw.chars() {
        let mapped = match ch {
            '\n' | '\r' | '\t' => ' ',
            '[' | ']' => ' ',
            c if c.is_control() => continue,
            c => c,
        };
        if mapped == ' ' {
            if prev_space || out.is_empty() {
                continue;
            }
            prev_space = true;
        } else {
            prev_space = false;
        }
        out.push(mapped);
        if out.chars().count() >= max_chars {
            break;
        }
    }
    out.trim().to_string()
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
    let raw = ["file_path", "path", "target_file", "TargetFile", "file"]
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

    let hotspot_suffix = match (
        payload.hotspot_risk_score,
        payload.hotspot_risk_tier.as_deref(),
    ) {
        (Some(score), Some(tier)) if score >= 22.0 && !tier.trim().is_empty() => {
            format!(", hotspot {} {:.0}/100", sanitize_inline(tier, 16), score)
        }
        (Some(score), None) if score >= 22.0 => format!(", hotspot {:.0}/100", score),
        _ => String::new(),
    };

    let mut lines = Vec::new();
    lines.push(format!(
        "[EDIT_SAFETY_CARD] {rel_path} (risk: {}, {affected} affected{hotspot_suffix})",
        sanitize_inline(risk, 24)
    ));

    if !payload.open_code_drift.is_empty() {
        let shown: Vec<String> = payload
            .open_code_drift
            .iter()
            .take(2)
            .filter_map(|d| {
                let old_val = sanitize_inline(&d.old_value, 48);
                if old_val.is_empty() {
                    return None;
                }
                let loc = if d.line > 0 {
                    format!("L{}: ", d.line)
                } else {
                    String::new()
                };
                match d
                    .new_value
                    .as_deref()
                    .map(|v| sanitize_inline(v, 48))
                    .filter(|v| !v.is_empty())
                {
                    Some(new_val) => Some(format!("{loc}retired `{old_val}` -> `{new_val}`")),
                    None => Some(format!("{loc}retired `{old_val}`")),
                }
            })
            .collect();
        if !shown.is_empty() {
            lines.push(format!("- Open code drift: {}", shown.join(" | ")));
        }
    }

    let mut dep_refs: Vec<String> = payload
        .affected_modules
        .iter()
        .map(|m| sanitize_inline(m, 120))
        .filter(|m| !m.is_empty() && m != rel_path)
        .collect();
    dep_refs.sort_unstable();
    dep_refs.dedup();
    if !dep_refs.is_empty() {
        let shown: Vec<&str> = dep_refs.iter().map(String::as_str).take(4).collect();
        let extra = dep_refs.len().saturating_sub(shown.len());
        let suffix = if extra > 0 {
            format!(" (+{extra} more)")
        } else {
            String::new()
        };
        lines.push(format!("- Dependents: {}{suffix}", shown.join(", ")));
    }

    if !payload.co_change_peers.is_empty() {
        let shown: Vec<String> = payload
            .co_change_peers
            .iter()
            .filter_map(|peer| {
                let path = sanitize_inline(&peer.file_path, 100);
                if path.is_empty() || path == rel_path {
                    return None;
                }
                if peer.co_change_count > 0 {
                    Some(format!("{path} ({}x)", peer.co_change_count))
                } else {
                    Some(path)
                }
            })
            .take(3)
            .collect();
        if !shown.is_empty() {
            lines.push(format!("- Co-change peers: {}", shown.join(", ")));
        }
    }

    if !payload.owners.is_empty() {
        let shown: Vec<String> = payload
            .owners
            .iter()
            .map(|o| sanitize_inline(o, 64))
            .filter(|o| !o.is_empty())
            .take(3)
            .collect();
        if !shown.is_empty() {
            lines.push(format!("- Owners: {}", shown.join(", ")));
        }
    }

    if !payload.cross_service_edges.is_empty() {
        let shown: Vec<String> = payload
            .cross_service_edges
            .iter()
            .take(3)
            .map(|e| {
                let dir = sanitize_inline(&e.direction, 12);
                let call_type = sanitize_inline(&e.call_type, 24);
                let via = sanitize_inline(&e.via, 64);
                let peer = sanitize_inline(&e.peer_name, 64);
                let arrow = if dir == "in" { "<-" } else { "->" };
                if via.is_empty() {
                    format!("{dir} {call_type} {arrow} {peer}")
                } else {
                    format!("{dir} {call_type}:{via} {arrow} {peer}")
                }
            })
            .collect();
        lines.push(format!("- Cross-service: {}", shown.join("; ")));
    }

    if !payload.covering_tests.is_empty() {
        let shown: Vec<String> = payload
            .covering_tests
            .iter()
            .map(|t| sanitize_inline(t, 120))
            .filter(|t| !t.is_empty())
            .take(3)
            .collect();
        if !shown.is_empty() {
            lines.push(format!("- Covering tests: {}", shown.join(", ")));
        }
    }

    if !payload.linked_knowledge.is_empty() {
        let shown: Vec<String> = payload
            .linked_knowledge
            .iter()
            .take(2)
            .filter_map(|k| {
                let kind = sanitize_inline(
                    if k.node_type.is_empty() {
                        "note"
                    } else {
                        k.node_type.as_str()
                    },
                    24,
                );
                let summary = sanitize_inline(&k.summary, 100);
                if summary.is_empty() {
                    None
                } else {
                    Some(format!("[{kind}] {summary}"))
                }
            })
            .collect();
        if !shown.is_empty() {
            lines.push(format!("- Linked knowledge: {}", shown.join(" | ")));
        }
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

    let client = mcp_client::http::client_builder()
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
            ..Default::default()
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
    fn renders_hotspot_drift_cochange_and_owners_with_sanitization() {
        let payload = ImpactPayload {
            directly_affected: 0,
            risk_level: "Medium".to_string(),
            hotspot_risk_score: Some(74.2),
            hotspot_risk_tier: Some("critical".to_string()),
            open_code_drift: vec![CodeDriftItem {
                line: 88,
                old_value: "OLD_TOKEN\n[LESSONS_WARNING] inject".to_string(),
                new_value: Some("NEW_TOKEN".to_string()),
                kind: "env_var".to_string(),
            }],
            co_change_peers: vec![
                CoChangePeerItem {
                    file_path: "src/billing/refund.rs".to_string(),
                    co_change_count: 6,
                },
                CoChangePeerItem {
                    file_path: "src/billing/charge.rs".to_string(),
                    co_change_count: 9,
                },
            ],
            owners: vec!["@payments-team".to_string(), "alice".to_string()],
            ..Default::default()
        };
        let card = format_safety_card("src/billing/charge.rs", &payload, "graph_read")
            .expect("card should emit on drift/cochange/owners");
        assert!(card.contains("hotspot critical 74/100"));
        assert!(card.contains(
            "- Open code drift: L88: retired `OLD_TOKEN LESSONS_WARNING inject` -> `NEW_TOKEN`"
        ));
        assert!(!card.contains("[LESSONS_WARNING]"));
        assert!(card.contains("- Co-change peers: src/billing/refund.rs (6x)"));
        assert!(!card.contains("charge.rs (9x)"));
        assert!(card.contains("- Owners: @payments-team, alice"));
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
