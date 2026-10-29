//! Shared helpers for hook handlers.

use chrono::{DateTime, Utc};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Compact human-readable age (e.g. `3h`, `2d`, `6w`, `4mo`) used when
/// rendering captured_at/created_at timestamps in session-start output.
/// Mirrors the server-side watcher so both surfaces agree on shape.
pub fn format_age(captured_at: DateTime<Utc>) -> String {
    let secs = Utc::now()
        .signed_duration_since(captured_at)
        .num_seconds()
        .max(0);
    if secs < 3600 {
        let m = (secs / 60).max(1);
        format!("{}m", m)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else if secs < 86_400 * 14 {
        format!("{}d", secs / 86_400)
    } else if secs < 86_400 * 90 {
        format!("{}w", secs / (86_400 * 7))
    } else {
        format!("{}mo", secs / (86_400 * 30))
    }
}

/// Extract an RFC3339 timestamp from a JSON value by key. Returns None when
/// the field is missing, not a string, or unparseable.
pub fn extract_age_suffix(json: &Value, key: &str) -> String {
    json.get(key)
        .and_then(|v| v.as_str())
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| format!(" ({} old)", format_age(dt.with_timezone(&Utc))))
        .unwrap_or_default()
}

/// The longest prefix of `value` that is at most `max_bytes` long and ends on a
/// character boundary. Slicing `&value[..max_bytes]` panics when a multi-byte
/// character (emoji, CJK, accented letter) straddles the cut; this steps back
/// to the start of that character instead.
pub fn prefix_within_bytes(value: &str, max_bytes: usize) -> &str {
    &value[..value.floor_char_boundary(max_bytes)]
}

/// `value` unchanged when it fits in `max_bytes`, otherwise its longest prefix
/// of at most `max_bytes` that ends on a character boundary, followed by `...`.
pub fn truncate_with_ellipsis(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_string();
    }
    format!("{}...", prefix_within_bytes(value, max_bytes))
}

#[derive(Debug, Clone)]
pub struct ApiConfig {
    pub api_key: String,
    pub api_url: String,
    pub workspace_id: Option<String>,
    pub project_id: Option<String>,
}

impl ApiConfig {
    pub fn is_configured(&self) -> bool {
        !self.api_key.is_empty()
    }
}

/// Configuration and effective transcript-saving policy resolved for
/// transcript-bearing lifecycle hooks (`pre-compact` and `session-end`).
#[derive(Debug, Clone)]
pub struct TranscriptHookConfig {
    pub api_key: String,
    pub api_url: String,
    pub workspace_id: Option<String>,
    pub project_id: Option<String>,
    pub save_transcript_default: bool,
}

#[derive(Default, Debug)]
struct McpConfigSnapshot {
    env_api_key: Option<String>,
    env_api_url: Option<String>,
    header_api_key: Option<String>,
    workspace_id: Option<String>,
    project_id: Option<String>,
    hook_override: Option<bool>,
    hook_transcripts_enabled: Option<bool>,
    transcripts_enabled: Option<bool>,
}

impl McpConfigSnapshot {
    fn entry_policy(&self) -> Option<bool> {
        if let Some(explicit) = self.hook_override {
            return Some(explicit);
        }
        if self.hook_transcripts_enabled == Some(false) || self.transcripts_enabled == Some(false) {
            return Some(false);
        }
        self.hook_transcripts_enabled.or(self.transcripts_enabled)
    }
}

pub(crate) fn parse_hook_bool(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn nonempty_trimmed(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn strip_bearer(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.len() > 7 && trimmed[..7].eq_ignore_ascii_case("bearer ") {
        nonempty_trimmed(&trimmed[7..])
    } else {
        None
    }
}

fn json_container_value(entry: &Value, containers: &[&str], key_ci: &str) -> Option<String> {
    for container in containers {
        let Some(map) = entry.get(*container).and_then(Value::as_object) else {
            continue;
        };
        for (k, v) in map {
            if k.eq_ignore_ascii_case(key_ci) {
                if let Some(s) = v.as_str() {
                    return Some(s.to_string());
                }
                if let Some(b) = v.as_bool() {
                    return Some(if b { "true" } else { "false" }.to_string());
                }
            }
        }
    }
    None
}

fn snapshot_from_json_entry(entry: &Value, hook_override_env: &str) -> McpConfigSnapshot {
    let env_containers = ["env", "environment"];
    let header_containers = ["headers"];
    let header_api_key = json_container_value(entry, &header_containers, "X-ContextStream-API-Key")
        .and_then(|v| nonempty_trimmed(&v))
        .or_else(|| {
            json_container_value(entry, &header_containers, "X-API-Key")
                .and_then(|v| nonempty_trimmed(&v))
        })
        .or_else(|| {
            json_container_value(entry, &header_containers, "Authorization")
                .and_then(|v| strip_bearer(&v))
        });

    McpConfigSnapshot {
        env_api_key: json_container_value(entry, &env_containers, "CONTEXTSTREAM_API_KEY")
            .and_then(|v| nonempty_trimmed(&v)),
        env_api_url: json_container_value(entry, &env_containers, "CONTEXTSTREAM_API_URL")
            .and_then(|v| nonempty_trimmed(&v)),
        header_api_key,
        workspace_id: json_container_value(entry, &env_containers, "CONTEXTSTREAM_WORKSPACE_ID")
            .and_then(|v| nonempty_trimmed(&v))
            .or_else(|| {
                json_container_value(entry, &header_containers, "X-ContextStream-Workspace-Id")
                    .and_then(|v| nonempty_trimmed(&v))
            }),
        project_id: json_container_value(entry, &env_containers, "CONTEXTSTREAM_PROJECT_ID")
            .and_then(|v| nonempty_trimmed(&v))
            .or_else(|| {
                json_container_value(entry, &header_containers, "X-ContextStream-Project-Id")
                    .and_then(|v| nonempty_trimmed(&v))
            }),
        hook_override: (!hook_override_env.is_empty())
            .then(|| json_container_value(entry, &env_containers, hook_override_env))
            .flatten()
            .and_then(|v| parse_hook_bool(&v)),
        hook_transcripts_enabled: json_container_value(
            entry,
            &env_containers,
            "CONTEXTSTREAM_HOOK_TRANSCRIPTS_ENABLED",
        )
        .and_then(|v| parse_hook_bool(&v))
        .or_else(|| {
            json_container_value(
                entry,
                &header_containers,
                "X-ContextStream-Hook-Transcripts-Enabled",
            )
            .and_then(|v| parse_hook_bool(&v))
        }),
        transcripts_enabled: json_container_value(
            entry,
            &env_containers,
            "CONTEXTSTREAM_TRANSCRIPTS_ENABLED",
        )
        .and_then(|v| parse_hook_bool(&v))
        .or_else(|| {
            json_container_value(
                entry,
                &header_containers,
                "X-ContextStream-Transcripts-Enabled",
            )
            .and_then(|v| parse_hook_bool(&v))
        }),
    }
}

fn snapshots_from_mcp_file(
    path: &Path,
    cwd: &str,
    hook_override_env: &str,
) -> Vec<McpConfigSnapshot> {
    if !path.is_file() {
        return Vec::new();
    }
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    if path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("toml"))
    {
        let Ok(doc) = content.parse::<toml_edit::DocumentMut>() else {
            return Vec::new();
        };
        let Some(entry) = doc
            .get("mcp_servers")
            .and_then(toml_edit::Item::as_table_like)
            .and_then(|servers| servers.get("contextstream"))
            .and_then(toml_edit::Item::as_table_like)
        else {
            return Vec::new();
        };
        let toml_scalar_to_string = |val: &toml_edit::Value| -> Option<String> {
            if let Some(s) = val.as_str() {
                return Some(s.to_string());
            }
            if let Some(b) = val.as_bool() {
                return Some(if b { "true" } else { "false" }.to_string());
            }
            None
        };
        let toml_val = |tables: &[&str], key_ci: &str| -> Option<String> {
            for table_name in tables {
                let Some(sub_item) = entry.get(table_name) else {
                    continue;
                };
                if let Some(sub) = sub_item.as_table_like() {
                    for (k, item) in sub.iter() {
                        if k.eq_ignore_ascii_case(key_ci) {
                            if let Some(val) = item.as_value().and_then(toml_scalar_to_string) {
                                return Some(val);
                            }
                        }
                    }
                }
                if let Some(inline) = sub_item
                    .as_value()
                    .and_then(toml_edit::Value::as_inline_table)
                {
                    for (k, val) in inline.iter() {
                        if k.eq_ignore_ascii_case(key_ci) {
                            if let Some(out) = toml_scalar_to_string(val) {
                                return Some(out);
                            }
                        }
                    }
                }
            }
            None
        };
        let header_tables = ["http_headers", "headers"];
        let env_tables = ["env"];
        let header_api_key = toml_val(&header_tables, "X-ContextStream-API-Key")
            .and_then(|v| nonempty_trimmed(&v))
            .or_else(|| toml_val(&header_tables, "X-API-Key").and_then(|v| nonempty_trimmed(&v)))
            .or_else(|| toml_val(&header_tables, "Authorization").and_then(|v| strip_bearer(&v)));
        return vec![McpConfigSnapshot {
            env_api_key: toml_val(&env_tables, "CONTEXTSTREAM_API_KEY")
                .and_then(|v| nonempty_trimmed(&v)),
            env_api_url: toml_val(&env_tables, "CONTEXTSTREAM_API_URL")
                .and_then(|v| nonempty_trimmed(&v)),
            header_api_key,
            workspace_id: toml_val(&env_tables, "CONTEXTSTREAM_WORKSPACE_ID")
                .and_then(|v| nonempty_trimmed(&v))
                .or_else(|| {
                    toml_val(&header_tables, "X-ContextStream-Workspace-Id")
                        .and_then(|v| nonempty_trimmed(&v))
                }),
            project_id: toml_val(&env_tables, "CONTEXTSTREAM_PROJECT_ID")
                .and_then(|v| nonempty_trimmed(&v))
                .or_else(|| {
                    toml_val(&header_tables, "X-ContextStream-Project-Id")
                        .and_then(|v| nonempty_trimmed(&v))
                }),
            hook_override: (!hook_override_env.is_empty())
                .then(|| toml_val(&env_tables, hook_override_env))
                .flatten()
                .and_then(|v| parse_hook_bool(&v)),
            hook_transcripts_enabled: toml_val(
                &env_tables,
                "CONTEXTSTREAM_HOOK_TRANSCRIPTS_ENABLED",
            )
            .and_then(|v| parse_hook_bool(&v))
            .or_else(|| {
                toml_val(&header_tables, "X-ContextStream-Hook-Transcripts-Enabled")
                    .and_then(|v| parse_hook_bool(&v))
            }),
            transcripts_enabled: toml_val(&env_tables, "CONTEXTSTREAM_TRANSCRIPTS_ENABLED")
                .and_then(|v| parse_hook_bool(&v))
                .or_else(|| {
                    toml_val(&header_tables, "X-ContextStream-Transcripts-Enabled")
                        .and_then(|v| parse_hook_bool(&v))
                }),
        }];
    }

    let Ok(parsed) = crate::setup::mcp_config::try_parse_json_like(&content) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    // Per-project entry inside `~/.claude.json` (`projects[<cwd>].mcpServers.contextstream`) if present.
    if !cwd.is_empty() {
        if let Some(proj_entry) = parsed
            .get("projects")
            .and_then(Value::as_object)
            .and_then(|projects| projects.get(cwd))
            .and_then(|proj| proj.get("mcpServers"))
            .and_then(|servers| servers.get("contextstream"))
        {
            out.push(snapshot_from_json_entry(proj_entry, hook_override_env));
        }
    }
    for container_path in [
        &["mcpServers", "contextstream"][..],
        &["servers", "contextstream"][..],
        &["context_servers", "contextstream"][..],
        &["mcp", "contextstream"][..],
        &["mcp", "servers", "contextstream"][..],
    ] {
        if let Some(entry) = container_path
            .iter()
            .try_fold(&parsed, |node, key| node.get(*key))
            .filter(|v| v.is_object())
        {
            out.push(snapshot_from_json_entry(entry, hook_override_env));
            break;
        }
    }
    out
}

/// Resolve API credentials, workspace/project IDs, and effective transcript
/// policy across process env, project/global editor MCP configs (both remote
/// HTTP `headers` and local stdio `env`), and `~/.contextstream/credentials.json`.
pub fn load_transcript_hook_config(
    cwd: &str,
    hook_override_env: &str,
    default_enabled: bool,
) -> TranscriptHookConfig {
    let mut api_key = std::env::var("CONTEXTSTREAM_API_KEY")
        .ok()
        .and_then(|v| nonempty_trimmed(&v))
        .unwrap_or_default();
    let explicit_api_url = std::env::var("CONTEXTSTREAM_API_URL")
        .ok()
        .and_then(|v| nonempty_trimmed(&v));
    let mut api_url: Option<String> = explicit_api_url.clone();
    let mut workspace_id = std::env::var("CONTEXTSTREAM_WORKSPACE_ID")
        .ok()
        .and_then(|v| nonempty_trimmed(&v));
    let mut project_id = std::env::var("CONTEXTSTREAM_PROJECT_ID")
        .ok()
        .and_then(|v| nonempty_trimmed(&v));

    let env_policy = (!hook_override_env.is_empty())
        .then(|| std::env::var(hook_override_env).ok())
        .flatten()
        .and_then(|v| parse_hook_bool(&v))
        .or_else(|| {
            std::env::var("CONTEXTSTREAM_HOOK_TRANSCRIPTS_ENABLED")
                .ok()
                .and_then(|v| parse_hook_bool(&v))
        })
        .or_else(|| {
            std::env::var("CONTEXTSTREAM_TRANSCRIPTS_ENABLED")
                .ok()
                .and_then(|v| parse_hook_bool(&v))
        });

    let mut seen_paths = std::collections::HashSet::new();
    let mut project_snapshots = Vec::new();
    let mut search_dir = PathBuf::from(cwd);
    if !cwd.is_empty() {
        for _ in 0..8 {
            let mut candidates = vec![
                search_dir.join(".mcp.json"),
                search_dir.join(".codex").join("config.toml"),
            ];
            for editor in crate::setup::editors::Editor::all() {
                if let Some(p) = editor.project_mcp_config_path(&search_dir) {
                    candidates.push(p);
                }
            }
            for candidate in candidates {
                if seen_paths.insert(candidate.clone()) {
                    project_snapshots.extend(snapshots_from_mcp_file(
                        &candidate,
                        cwd,
                        hook_override_env,
                    ));
                }
            }

            if workspace_id.is_none() || project_id.is_none() {
                let cs_config = search_dir.join(".contextstream").join("config.json");
                if let Ok(content) = std::fs::read_to_string(&cs_config) {
                    if let Ok(cfg) = serde_json::from_str::<Value>(&content) {
                        if workspace_id.is_none() {
                            workspace_id = cfg
                                .get("workspace_id")
                                .and_then(Value::as_str)
                                .and_then(nonempty_trimmed);
                        }
                        if project_id.is_none() {
                            project_id = cfg
                                .get("project_id")
                                .and_then(Value::as_str)
                                .and_then(nonempty_trimmed);
                        }
                    }
                }
            }

            if !search_dir.pop() {
                break;
            }
        }
    }

    let mut global_snapshots = Vec::new();
    if let Some(home) = home_dir() {
        let mut candidates = vec![home.join(".mcp.json")];
        for editor in crate::setup::editors::Editor::all() {
            if let Some(p) = editor.mcp_config_path() {
                candidates.push(p);
            }
        }
        for candidate in candidates {
            if seen_paths.insert(candidate.clone()) {
                global_snapshots.extend(snapshots_from_mcp_file(
                    &candidate,
                    cwd,
                    hook_override_env,
                ));
            }
        }
    }

    // 1. Local stdio `env` credentials in project -> global MCP configs.
    for snap in project_snapshots.iter().chain(global_snapshots.iter()) {
        if api_key.is_empty() {
            if let Some(ref key) = snap.env_api_key {
                api_key = key.clone();
                if api_url.is_none() {
                    api_url = snap.env_api_url.clone();
                }
            }
        }
        if api_url.is_none() {
            api_url = snap.env_api_url.clone();
        }
    }

    // 2. Saved `~/.contextstream/credentials.json` (`api_key` + `api_url`).
    if let Some(home) = home_dir() {
        let creds_path = home.join(".contextstream").join("credentials.json");
        if let Ok(content) = std::fs::read_to_string(&creds_path) {
            if let Ok(creds) = serde_json::from_str::<Value>(&content) {
                if api_key.is_empty() {
                    if let Some(key) = creds
                        .get("api_key")
                        .and_then(Value::as_str)
                        .and_then(nonempty_trimmed)
                    {
                        api_key = key;
                    }
                }
                if api_url.is_none() {
                    api_url = creds
                        .get("api_url")
                        .and_then(Value::as_str)
                        .and_then(nonempty_trimmed);
                }
            }
        }
    }

    // 3. Remote HTTP `headers` auth fallback (`--preauth` configs without credentials.json).
    if api_key.is_empty() {
        for snap in project_snapshots.iter().chain(global_snapshots.iter()) {
            if let Some(ref key) = snap.header_api_key {
                api_key = key.clone();
                break;
            }
        }
    }

    // 4. Workspace/project fallback from MCP configs when `.contextstream/config.json` is absent.
    for snap in &project_snapshots {
        if workspace_id.is_none() {
            workspace_id = snap.workspace_id.clone();
        }
        if project_id.is_none() {
            project_id = snap.project_id.clone();
        }
    }
    if workspace_id.is_none() {
        for snap in &global_snapshots {
            if snap.workspace_id.is_some() {
                workspace_id = snap.workspace_id.clone();
                break;
            }
        }
    }

    // 5. Transcript policy resolution:
    //    - Explicit process env wins.
    //    - Next, explicit hook-specific `env` override (`CONTEXTSTREAM_PRECOMPACT_AUTO_SAVE` /
    //      `CONTEXTSTREAM_SESSION_END_SAVE_TRANSCRIPT`) in project -> global MCP config.
    //    - Next, fail-closed opt-out across project and global MCP configs: if ANY
    //      configured editor MCP entry has `false` (e.g. `configure --transcripts off`
    //      or `configure --hook-transcripts off` at `--scope=global`, `--scope=project`,
    //      or `--scope=all`), resolve to `false`.
    //    - Next, any explicit `true` entry, otherwise `default_enabled`.
    let save_transcript_default = if let Some(explicit_env) = env_policy {
        explicit_env
    } else if let Some(hook_specific) = project_snapshots
        .iter()
        .chain(global_snapshots.iter())
        .find_map(|s| s.hook_override)
    {
        hook_specific
    } else {
        let policies: Vec<bool> = project_snapshots
            .iter()
            .chain(global_snapshots.iter())
            .filter_map(McpConfigSnapshot::entry_policy)
            .collect();
        if policies.contains(&false) {
            false
        } else if policies.contains(&true) {
            true
        } else {
            default_enabled
        }
    };

    let resolved_url = api_url
        .unwrap_or_else(|| "https://api.contextstream.io".to_string())
        .trim_end_matches('/')
        .to_string();

    TranscriptHookConfig {
        api_key,
        api_url: resolved_url,
        workspace_id,
        project_id,
        save_transcript_default,
    }
}

/// The host's own id for the conversation: Claude Code and Codex send
/// `session_id` (or `sessionId`), Cursor `conversation_id`. This is not the id
/// `init` returns, which is what a transcript and a resume card use.
pub fn host_session_key(input: &Value) -> Option<String> {
    [
        "session_id",
        "sessionId",
        "conversation_id",
        "conversationId",
    ]
    .iter()
    .find_map(|key| {
        input
            .get(*key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

/// The label a git event carries for the harness that ran the hook: `cursor`,
/// `codex` (its tool events carry a `turn_id`) or `claude_code`.
pub fn agent_label(input: &Value) -> &'static str {
    if super::input_is_cursor(input) {
        "cursor"
    } else if input.get("turn_id").is_some() {
        "codex"
    } else {
        "claude_code"
    }
}

/// Extract working directory from a hook payload.
pub fn extract_cwd(input: &Value) -> String {
    input
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| {
            input
                .get("workspace_roots")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .or_else(|| {
            input
                .get("workspaceRoots")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .or_else(|| {
            input
                .get("workspacePaths")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .and_then(|p| p.to_str().map(String::from))
        })
        .unwrap_or_default()
}

/// Load API credentials and workspace/project IDs from env and local config files.
pub fn load_config(cwd: &str) -> ApiConfig {
    let mut api_key = std::env::var("CONTEXTSTREAM_API_KEY").unwrap_or_default();
    let mut api_url = std::env::var("CONTEXTSTREAM_API_URL")
        .unwrap_or_else(|_| "https://api.contextstream.io".to_string());
    let mut workspace_id: Option<String> = std::env::var("CONTEXTSTREAM_WORKSPACE_ID").ok();
    let mut project_id: Option<String> = std::env::var("CONTEXTSTREAM_PROJECT_ID").ok();

    let mut search_dir = PathBuf::from(cwd);
    for _ in 0..8 {
        if api_key.is_empty() {
            let mcp_path = search_dir.join(".mcp.json");
            if let Some((key, url)) = read_mcp_json_credentials(&mcp_path) {
                api_key = key;
                if let Some(u) = url {
                    api_url = u;
                }
            }
        }

        if workspace_id.is_none() || project_id.is_none() {
            let cs_config = search_dir.join(".contextstream").join("config.json");
            if let Ok(content) = std::fs::read_to_string(&cs_config) {
                if let Ok(cfg) = serde_json::from_str::<Value>(&content) {
                    if workspace_id.is_none() {
                        workspace_id = cfg
                            .get("workspace_id")
                            .and_then(|w| w.as_str())
                            .map(String::from);
                    }
                    if project_id.is_none() {
                        project_id = cfg
                            .get("project_id")
                            .and_then(|p| p.as_str())
                            .map(String::from);
                    }
                }
            }
        }

        if !search_dir.pop() {
            break;
        }
    }

    if api_key.is_empty() {
        if let Some(home) = home_dir() {
            let home_mcp = home.join(".mcp.json");
            if let Some((key, url)) = read_mcp_json_credentials(&home_mcp) {
                api_key = key;
                if let Some(u) = url {
                    api_url = u;
                }
            }
        }
    }

    if api_key.is_empty() {
        if let Some(home) = home_dir() {
            let creds_path = home.join(".contextstream").join("credentials.json");
            if let Ok(content) = std::fs::read_to_string(&creds_path) {
                if let Ok(creds) = serde_json::from_str::<Value>(&content) {
                    if let Some(key) = creds.get("api_key").and_then(|k| k.as_str()) {
                        api_key = key.to_string();
                    }
                    if let Some(url) = creds.get("api_url").and_then(|u| u.as_str()) {
                        api_url = url.to_string();
                    }
                }
            }
        }
    }

    if api_key.is_empty() {
        let fallback = load_transcript_hook_config(cwd, "", true);
        if !fallback.api_key.is_empty() {
            api_key = fallback.api_key;
        }
        if workspace_id.is_none() {
            workspace_id = fallback.workspace_id;
        }
        if project_id.is_none() {
            project_id = fallback.project_id;
        }
    }

    ApiConfig {
        api_key,
        api_url: api_url.trim_end_matches('/').to_string(),
        workspace_id,
        project_id,
    }
}

fn read_mcp_json_credentials(path: &Path) -> Option<(String, Option<String>)> {
    let content = std::fs::read_to_string(path).ok()?;
    let config: Value = serde_json::from_str(&content).ok()?;
    let env = config.get("mcpServers")?.get("contextstream")?.get("env")?;
    let key = env
        .get("CONTEXTSTREAM_API_KEY")
        .and_then(|k| k.as_str())?
        .to_string();
    let url = env
        .get("CONTEXTSTREAM_API_URL")
        .and_then(|u| u.as_str())
        .map(String::from);
    Some((key, url))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn add_workspace_and_project(payload: &mut serde_json::Map<String, Value>, config: &ApiConfig) {
    if let Some(ref ws_id) = config.workspace_id {
        payload.insert("workspace_id".to_string(), Value::String(ws_id.clone()));
    }
    if let Some(ref project_id) = config.project_id {
        payload.insert("project_id".to_string(), Value::String(project_id.clone()));
    }
}

/// Extract an entity ID from common API response shapes.
pub fn extract_id(value: &Value) -> Option<String> {
    value
        .get("id")
        .and_then(|v| v.as_str())
        .or_else(|| {
            value
                .get("data")
                .and_then(|d| d.get("id"))
                .and_then(|v| v.as_str())
        })
        .or_else(|| {
            value
                .get("task")
                .and_then(|d| d.get("id"))
                .and_then(|v| v.as_str())
        })
        .or_else(|| {
            value
                .get("plan")
                .and_then(|d| d.get("id"))
                .and_then(|v| v.as_str())
        })
        .map(String::from)
}

/// Extract an array of items from common API response shapes.
pub fn extract_items(value: &Value) -> Vec<Value> {
    if let Some(items) = value.as_array() {
        return items.clone();
    }

    if let Some(items) = value.get("items").and_then(|v| v.as_array()) {
        return items.clone();
    }

    if let Some(items) = value
        .get("data")
        .and_then(|v| v.get("items"))
        .and_then(|v| v.as_array())
    {
        return items.clone();
    }

    if let Some(items) = value.get("data").and_then(|v| v.as_array()) {
        return items.clone();
    }

    if let Some(items) = value.get("tasks").and_then(|v| v.as_array()) {
        return items.clone();
    }

    Vec::new()
}

/// Send a low-importance memory event. Network/API errors are ignored.
pub async fn post_memory_event(config: &ApiConfig, title: &str, content: Value, tags: &[&str]) {
    if !config.is_configured() {
        return;
    }

    let mut payload = serde_json::Map::new();
    // Hook captures are operational telemetry, not knowledge: the explicit
    // `operation` type (plus source_type=hook below) lets ranking downweight
    // them and the recall/grounding surface exclude them, while list_events
    // keeps them fully queryable.
    payload.insert(
        "event_type".to_string(),
        Value::String("operation".to_string()),
    );
    payload.insert("title".to_string(), Value::String(title.to_string()));
    payload.insert("content".to_string(), Value::String(content.to_string()));
    payload.insert("importance".to_string(), Value::String("low".to_string()));
    payload.insert("source_type".to_string(), Value::String("hook".to_string()));
    payload.insert(
        "tags".to_string(),
        Value::Array(
            tags.iter()
                .map(|t| Value::String((*t).to_string()))
                .collect(),
        ),
    );
    add_workspace_and_project(&mut payload, config);

    let client = super::api_http_client();
    let _ = client
        .post(format!("{}/api/v1/memory/events", config.api_url))
        .header("X-API-Key", &config.api_key)
        .header("Content-Type", "application/json")
        .json(&payload)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await;
}

/// Create a plan. Returns the new plan ID when available.
pub async fn create_plan(config: &ApiConfig, title: &str, description: &str) -> Option<String> {
    if !config.is_configured() {
        return None;
    }

    let mut payload = serde_json::Map::new();
    payload.insert("title".to_string(), Value::String(title.to_string()));
    payload.insert(
        "description".to_string(),
        Value::String(description.to_string()),
    );
    payload.insert(
        "tags".to_string(),
        serde_json::json!(["hook", "subagent_plan"]),
    );
    add_workspace_and_project(&mut payload, config);

    let client = super::api_http_client();
    let response = client
        .post(format!("{}/api/v1/plans", config.api_url))
        .header("X-API-Key", &config.api_key)
        .header("Content-Type", "application/json")
        .json(&payload)
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await
        .ok()?;

    if !response.status().is_success() {
        return None;
    }

    let value: Value = response.json().await.ok()?;
    extract_id(&value)
}

/// Create a task and optionally attach it to a plan.
pub async fn create_task(
    config: &ApiConfig,
    title: &str,
    description: Option<&str>,
    plan_id: Option<&str>,
    status: Option<&str>,
) -> Option<String> {
    if !config.is_configured() {
        return None;
    }

    let mut payload = serde_json::Map::new();
    payload.insert("title".to_string(), Value::String(title.to_string()));
    if let Some(desc) = description {
        payload.insert("description".to_string(), Value::String(desc.to_string()));
    }
    if let Some(plan_id) = plan_id {
        payload.insert("plan_id".to_string(), Value::String(plan_id.to_string()));
    }
    if let Some(status) = status {
        payload.insert("status".to_string(), Value::String(status.to_string()));
    }
    payload.insert("tags".to_string(), serde_json::json!(["hook"]));
    add_workspace_and_project(&mut payload, config);

    let client = super::api_http_client();
    let response = client
        .post(format!("{}/api/v1/tasks", config.api_url))
        .header("X-API-Key", &config.api_key)
        .header("Content-Type", "application/json")
        .json(&payload)
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await
        .ok()?;

    if !response.status().is_success() {
        return None;
    }

    let value: Value = response.json().await.ok()?;
    extract_id(&value)
}

/// Update task status by ID. Returns true if the API call succeeded.
pub async fn update_task_status(
    config: &ApiConfig,
    task_id: &str,
    status: &str,
    title: Option<&str>,
    description: Option<&str>,
) -> bool {
    if !config.is_configured() || task_id.is_empty() {
        return false;
    }

    let mut payload = serde_json::Map::new();
    payload.insert("status".to_string(), Value::String(status.to_string()));
    if let Some(title) = title {
        payload.insert("title".to_string(), Value::String(title.to_string()));
    }
    if let Some(description) = description {
        payload.insert(
            "description".to_string(),
            Value::String(description.to_string()),
        );
    }

    let client = super::api_http_client();
    let response = client
        .put(format!("{}/api/v1/tasks/{}", config.api_url, task_id))
        .header("X-API-Key", &config.api_key)
        .header("Content-Type", "application/json")
        .json(&payload)
        .timeout(std::time::Duration::from_secs(8))
        .send()
        .await;

    match response {
        Ok(resp) => resp.status().is_success(),
        Err(_) => false,
    }
}

/// List pending tasks for the active workspace/project.
pub async fn list_pending_tasks(config: &ApiConfig, limit: usize) -> Vec<Value> {
    if !config.is_configured() {
        return Vec::new();
    }

    let mut params = vec![
        "status=pending".to_string(),
        format!("limit={}", limit.max(1)),
    ];
    if let Some(ref ws_id) = config.workspace_id {
        params.push(format!("workspace_id={}", ws_id));
    }
    if let Some(ref project_id) = config.project_id {
        params.push(format!("project_id={}", project_id));
    }

    let url = format!("{}/api/v1/tasks?{}", config.api_url, params.join("&"));

    let client = super::api_http_client();
    let response = match client
        .get(url)
        .header("X-API-Key", &config.api_key)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
    {
        Ok(resp) => resp,
        Err(_) => return Vec::new(),
    };

    if !response.status().is_success() {
        return Vec::new();
    }

    let value: Value = match response.json().await {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    extract_items(&value)
}

// ---------------------------------------------------------------------------
// Account-setup secret scrubbing for captured transcripts
// ---------------------------------------------------------------------------

/// Tool names (bare and host-prefixed) whose calls carry account-setup data.
pub const ACCOUNT_SETUP_TOOL_NAMES: [&str; 2] = ["account", "mcp__contextstream__account"];

pub fn is_account_setup_tool(name: &str) -> bool {
    let name = name.trim();
    ACCOUNT_SETUP_TOOL_NAMES.contains(&name) || name.ends_with("__account")
}

const DEVICE_CODE_ALPHABET: &str = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

fn is_boundary(c: Option<char>) -> bool {
    match c {
        None => true,
        Some(c) => !c.is_ascii_alphanumeric() && c != '_' && c != '-' && c != '+',
    }
}

/// Redact credential-shaped tokens that must never be persisted anywhere:
/// API keys (`cbiq_…`) and setup tokens (`cbst_…`).
pub fn scrub_credential_tokens(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let rest: String = chars[i..chars.len().min(i + 5)].iter().collect();
        if (rest == "cbiq_" || rest == "cbst_")
            && is_boundary(chars.get(i.wrapping_sub(1)).copied().filter(|_| i > 0))
        {
            let mut j = i + 5;
            while j < chars.len()
                && (chars[j].is_ascii_alphanumeric() || chars[j] == '_' || chars[j] == '-')
            {
                j += 1;
            }
            out.push_str("[redacted-credential]");
            i = j;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Full scrub for sessions that contain an account-setup flow: credential
/// tokens, standalone 6-digit verification codes, device user codes
/// (`ABCD-EFGH`), and phone numbers (`+` followed by 8-15 digits).
pub fn scrub_setup_secrets(text: &str) -> String {
    let text = scrub_credential_tokens(text);
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let prev = if i == 0 { None } else { Some(chars[i - 1]) };
        // Phone number: + then 8-15 digits (spaces/dashes allowed between).
        if chars[i] == '+' && is_boundary(prev) {
            let mut j = i + 1;
            let mut digits = 0;
            while j < chars.len()
                && (chars[j].is_ascii_digit()
                    || chars[j] == ' '
                    || chars[j] == '-'
                    || chars[j] == '('
                    || chars[j] == ')')
            {
                if chars[j].is_ascii_digit() {
                    digits += 1;
                }
                j += 1;
            }
            if (8..=15).contains(&digits) {
                out.push_str("[phone]");
                i = j;
                continue;
            }
        }
        // Six-digit code.
        if chars[i].is_ascii_digit() && is_boundary(prev) {
            let mut j = i;
            while j < chars.len() && chars[j].is_ascii_digit() {
                j += 1;
            }
            if j - i == 6 && is_boundary(chars.get(j).copied()) {
                out.push_str("[code]");
                i = j;
                continue;
            }
        }
        // Device user code ABCD-EFGH.
        if i + 9 <= chars.len()
            && is_boundary(prev)
            && chars[i + 4] == '-'
            && chars[i..i + 4]
                .iter()
                .chain(chars[i + 5..i + 9].iter())
                .all(|c| DEVICE_CODE_ALPHABET.contains(*c))
            && is_boundary(chars.get(i + 9).copied())
        {
            out.push_str("[device-code]");
            i += 9;
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Detect account calls even in nested host transcript formats before text
/// extraction drops tool-use blocks or their correlation identifiers.
pub fn transcript_has_account_setup(text: &str) -> bool {
    fn contains_call(value: &serde_json::Value) -> bool {
        match value {
            serde_json::Value::Object(object) => {
                ["name", "tool_name", "toolName"].iter().any(|key| {
                    object
                        .get(*key)
                        .and_then(|v| v.as_str())
                        .is_some_and(is_account_setup_tool)
                }) || object.values().any(contains_call)
            }
            serde_json::Value::Array(values) => values.iter().any(contains_call),
            _ => false,
        }
    }
    text.lines().any(|line| {
        serde_json::from_str(line)
            .ok()
            .as_ref()
            .is_some_and(contains_call)
    })
}

fn message_tool_name(message: &serde_json::Value) -> Option<&str> {
    message
        .get("tool_calls")
        .and_then(|t| t.get("name"))
        .or_else(|| message.get("tool_results").and_then(|t| t.get("name")))
        .and_then(|n| n.as_str())
}

/// Post-process captured transcript messages: account-tool inputs and
/// results are replaced wholesale, and when such a call is present every
/// message body is scrubbed of codes, phone numbers and credentials. Every
/// session gets the credential-token scrub regardless.
pub fn scrub_account_setup_messages(messages: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    scrub_account_setup_messages_with_hint(messages, false)
}

pub fn scrub_account_setup_messages_with_hint(
    mut messages: Vec<serde_json::Value>,
    setup_hint: bool,
) -> Vec<serde_json::Value> {
    let setup_flow_present = setup_hint
        || messages
            .iter()
            .any(|m| message_tool_name(m).is_some_and(is_account_setup_tool));
    for message in messages.iter_mut() {
        let is_setup_call = message_tool_name(message).is_some_and(is_account_setup_tool);
        if is_setup_call {
            if let Some(calls) = message.get_mut("tool_calls") {
                if let Some(obj) = calls.as_object_mut() {
                    obj.insert(
                        "input".to_string(),
                        serde_json::json!({"redacted": "account-setup"}),
                    );
                }
            }
        }
        // Hosts may omit the result's tool name. During setup, withhold such
        // payloads rather than relying on an unavailable name/id correlation.
        if setup_flow_present && message.get("tool_results").is_some() {
            message["content"] =
                serde_json::Value::String("[account-setup result redacted]".to_string());
            message["tool_results"] = serde_json::json!({"redacted": "account-setup"});
        }
        if let Some(content) = message.get("content").and_then(|c| c.as_str()) {
            let scrubbed = if setup_flow_present {
                scrub_setup_secrets(content)
            } else {
                scrub_credential_tokens(content)
            };
            if scrubbed != content {
                message["content"] = serde_json::Value::String(scrubbed);
            }
        }
    }
    messages
}

#[cfg(test)]
mod account_scrub_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn credential_tokens_are_always_redacted() {
        let text = "key cbiq_abcDEF123-_x and token cbst_zzz end";
        assert_eq!(
            scrub_credential_tokens(text),
            "key [redacted-credential] and token [redacted-credential] end"
        );
        assert_eq!(
            scrub_credential_tokens("no secrets 123456"),
            "no secrets 123456"
        );
    }

    #[test]
    fn full_scrub_covers_codes_phones_and_device_codes() {
        let text = "code is 483920, phone +1 (555) 010-1234, device ABCD-EFGH, id 1234567 keeps";
        let scrubbed = scrub_setup_secrets(text);
        assert!(scrubbed.contains("[code]"), "{scrubbed}");
        assert!(scrubbed.contains("[phone]"), "{scrubbed}");
        assert!(scrubbed.contains("[device-code]"), "{scrubbed}");
        assert!(scrubbed.contains("1234567 keeps"), "{scrubbed}");
    }

    #[test]
    fn account_tool_messages_are_redacted_and_session_is_scrubbed() {
        let messages = vec![
            json!({"role": "user", "content": "my code is 112233"}),
            json!({"role": "assistant", "content": "[Tool: mcp__contextstream__account]",
                   "tool_calls": {"name": "mcp__contextstream__account", "input": {"action": "signup_verify_phone", "code": "112233"}}}),
            json!({"role": "tool", "content": "{\"status\":\"connected\"}", "tool_results": {"name": "mcp__contextstream__account"}}),
        ];
        let out = scrub_account_setup_messages(messages);
        assert_eq!(out[0]["content"], "my code is [code]");
        assert_eq!(out[1]["tool_calls"]["input"]["redacted"], "account-setup");
        assert_eq!(out[2]["content"], "[account-setup result redacted]");
    }

    #[test]
    fn nested_host_calls_scrub_codes_and_unnamed_results() {
        let raw = json!({"type": "assistant", "message": {"content": [
            {"type": "tool_use", "name": "mcp__contextstream__account", "input": {"code": "112233"}}
        ]}})
        .to_string();
        assert!(transcript_has_account_setup(&raw));
        let messages = vec![
            json!({"role": "user", "content": "112233"}),
            json!({"role": "tool", "content": "arbitrary-client-secret", "tool_results": {"name": ""}}),
        ];
        let result =
            scrub_account_setup_messages_with_hint(messages, transcript_has_account_setup(&raw));
        let output = serde_json::to_string(&result).unwrap();
        assert!(!output.contains("112233"));
        assert!(!output.contains("arbitrary-client-secret"));
    }

    #[test]
    fn sessions_without_setup_flow_keep_six_digit_numbers() {
        let messages = vec![json!({"role": "user", "content": "issue 123456 and cbiq_secret"})];
        let out = scrub_account_setup_messages(messages);
        assert_eq!(out[0]["content"], "issue 123456 and [redacted-credential]");
    }
}

#[cfg(test)]
mod truncation_tests {
    use super::*;

    /// An emoji (4 bytes), a CJK character (3) and an accented letter (2).
    const MULTI_BYTE_CHARS: [&str; 3] = ["\u{1F600}", "\u{4E16}", "\u{E9}"];

    #[test]
    fn prefix_steps_back_to_the_start_of_a_straddling_character() {
        for ch in MULTI_BYTE_CHARS {
            for inside in 1..ch.len() {
                let head = "a".repeat(10 - inside);
                let value = format!("{head}{ch}tail");
                assert_eq!(
                    prefix_within_bytes(&value, 10),
                    head,
                    "{ch:?} cut {inside} byte(s) in"
                );
            }
        }
    }

    #[test]
    fn prefix_keeps_a_character_that_ends_exactly_at_the_limit() {
        for ch in MULTI_BYTE_CHARS {
            let head = "a".repeat(10 - ch.len());
            let value = format!("{head}{ch}tail");
            assert_eq!(prefix_within_bytes(&value, 10), format!("{head}{ch}"));
        }
    }

    #[test]
    fn prefix_handles_a_limit_inside_the_first_character_and_past_the_end() {
        assert_eq!(prefix_within_bytes("\u{1F600}x", 3), "");
        assert_eq!(prefix_within_bytes("\u{E9}x", 1), "");
        assert_eq!(prefix_within_bytes("", 5), "");
        assert_eq!(prefix_within_bytes("short", 100), "short");
        assert_eq!(prefix_within_bytes("abc", 0), "");
    }

    #[test]
    fn ellipsis_is_added_only_when_text_was_cut() {
        assert_eq!(truncate_with_ellipsis("abcdef", 3), "abc...");
        assert_eq!(truncate_with_ellipsis("abc", 3), "abc");
        assert_eq!(truncate_with_ellipsis("abc", 10), "abc");
        assert_eq!(truncate_with_ellipsis("abc", 0), "...");
        assert_eq!(truncate_with_ellipsis("", 0), "");
    }

    #[test]
    fn ellipsis_cut_never_splits_a_multi_byte_character() {
        for ch in MULTI_BYTE_CHARS {
            for inside in 1..ch.len() {
                let head = "a".repeat(10 - inside);
                let value = format!("{head}{ch}tail");
                assert_eq!(
                    truncate_with_ellipsis(&value, 10),
                    format!("{head}..."),
                    "{ch:?} cut {inside} byte(s) in"
                );
            }
        }
    }

    #[test]
    fn a_text_made_only_of_multi_byte_characters_stays_valid_utf8_at_every_limit() {
        let value = "\u{1F600}\u{4E16}\u{E9}".repeat(8);
        for limit in 0..=value.len() + 2 {
            let cut = truncate_with_ellipsis(&value, limit);
            let kept = cut.strip_suffix("...").unwrap_or(&cut);
            assert!(kept.len() <= limit, "limit {limit}");
            assert!(value.starts_with(kept), "limit {limit}");
        }
    }
}
