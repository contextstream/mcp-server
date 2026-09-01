//! PreToolUse hook handler.
//!
//! Redirects broad discovery tools to ContextStream search when the project
//! has indexed coverage (fresh or stale). Indexed-but-stale coverage is still
//! useful for existing code; only missing index coverage triggers a short
//! initial wait before local fallback. Nudges plan/task saving to ContextStream.
//!
//! Supports multiple editor formats:
//! - Claude Code: tool_name, tool_input, cwd
//! - Cline/Roo/Kilo: toolName, toolParameters, workspaceRoots
//! - Cursor: hook_event_name, parameters, workspace_roots
//! - Windsurf: pre_mcp_tool_use with exit-code based blocking

use anyhow::Result;
use serde_json::Value;
use std::path::Path;
use std::sync::OnceLock;

use super::compliance::{self, CheckResult, CheckType, ComplianceEvent, RuleClass};
use super::prompt_state;
use super::{read_stdin_json, write_stdout_json, HookOutput};

/// Discovery glob patterns that indicate broad file searching.
const DISCOVERY_GLOBS: &[&str] = &[
    "**/*",
    "**/",
    "**/**",
    "**/*.*",
    "src/**",
    "lib/**",
    "app/**",
    "packages/**",
    "components/**",
];

/// Prefixes that indicate a count query.
const COUNT_QUERY_PREFIXES: &[&str] = &["how many ", "count ", "count of ", "number of ", "total "];

/// Phrases that indicate the user wants all occurrences.
const ALL_MATCH_KEYWORDS: &[&str] = &[
    "all occurrences",
    "all matches",
    "find all",
    "every usage",
    "every occurrence",
    "all usages",
];

/// Phrases that indicate team/cross-workspace search.
const TEAM_QUERY_KEYWORDS: &[&str] = &[
    "team-wide",
    "teamwide",
    "cross-project",
    "cross project",
    "across projects",
    "all workspaces",
    "all projects",
];

/// Leading words that typically indicate semantic/natural-language search.
const QUESTION_WORDS: &[&str] = &[
    "how", "what", "where", "why", "when", "which", "who", "does", "is", "can", "should",
];

const DEFAULT_INDEX_WAIT_SECONDS: u64 = 20;
const MIN_INDEX_WAIT_SECONDS: u64 = 15;
const MAX_INDEX_WAIT_SECONDS: u64 = 20;

/// Check if a glob pattern is a broad discovery pattern.
/// Allows targeted patterns like "src/models/*.rs", "web/src/**/*sidebar*",
/// or "*.rs" (specific extension in directory).
/// Only blocks truly broad patterns like "**/*", "**/", "src/**", "**/foo".
fn is_discovery_glob(pattern: &str) -> bool {
    let lower = pattern.trim().to_lowercase();

    if DISCOVERY_GLOBS.iter().any(|&p| lower == p) {
        return true;
    }

    // Patterns starting with **/ are broad unless they also have a filename
    // filter (e.g. **/*sidebar* is still broad, but dir/**/*sidebar* is targeted).
    if lower.starts_with("**/") {
        return true;
    }

    // Patterns with ** in the middle (e.g. foo/**/bar) are recursive.
    // Allow them when BOTH a directory prefix AND a filename filter are present,
    // meaning the search is scoped: "web/src/**/*sidebar*", "app/**/*Button*.tsx".
    if lower.contains("**") {
        return !is_scoped_recursive_glob(&lower);
    }

    false
}

/// A recursive glob (containing **) is "scoped" when it has a non-trivial
/// directory prefix before the ** AND a non-trivial filename filter after it.
/// Examples:
///   "web/src/**/*sidebar*"  -> scoped (prefix=web/src, filter=*sidebar*)
///   "src/**/*.rs"           -> scoped (prefix=src, filter=*.rs)
///   "src/**"                -> NOT scoped (no filename filter)
///   "**/*sidebar*"          -> NOT scoped (no directory prefix)
fn is_scoped_recursive_glob(pattern: &str) -> bool {
    let Some(star_pos) = pattern.find("**") else {
        return false;
    };

    let prefix = &pattern[..star_pos];
    let has_dir_prefix = prefix
        .trim_end_matches('/')
        .chars()
        .any(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.');

    let suffix = &pattern[star_pos + 2..];
    let suffix_trimmed = suffix.trim_start_matches('/');
    let has_filename_filter = !suffix_trimmed.is_empty()
        && suffix_trimmed != "*"
        && suffix_trimmed != "*.*"
        && suffix_trimmed
            .chars()
            .any(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.');

    has_dir_prefix && has_filename_filter
}

/// Check if a glob pattern is broad and generic (no useful query signal).
fn is_generic_discovery_glob(pattern: &str) -> bool {
    let lower = pattern.trim().to_lowercase();
    DISCOVERY_GLOBS.iter().any(|&p| p == lower)
}

/// Check if a file path indicates broad discovery (no specific target).
fn is_discovery_path(file_path: &str) -> bool {
    let p = file_path.trim();
    p.is_empty()
        || p == "."
        || p == "./"
        || p == "*"
        || p == "**"
        || p.contains("**")
        || p.contains("*/*")
}

/// Check if a query is likely a code identifier/symbol.
fn is_identifier_query(query: &str) -> bool {
    let trimmed = query.trim();
    if trimmed.is_empty() || trimmed.contains(' ') || trimmed.len() < 2 {
        return false;
    }

    let is_valid = trimmed
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == ':');
    if !is_valid {
        return false;
    }

    let has_mixed_case =
        trimmed.chars().any(|c| c.is_uppercase()) && trimmed.chars().any(|c| c.is_lowercase());
    let has_underscore = trimmed.contains('_');
    let is_all_caps = trimmed.len() >= 3
        && trimmed
            .chars()
            .all(|c| c.is_uppercase() || c == '_' || c.is_numeric());

    has_mixed_case || has_underscore || is_all_caps
}

/// Check if a query appears to use regex metacharacters.
fn has_regex_characters(query: &str) -> bool {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return false;
    }

    if trimmed.contains("\\b")
        || trimmed.contains("\\s")
        || trimmed.contains("\\d")
        || trimmed.contains("\\w")
        || trimmed.contains("\\(")
        || trimmed.contains("\\)")
        || trimmed.contains("(?:")
        || trimmed.contains("(?=")
        || trimmed.contains("(?!")
        || trimmed.contains("(?<=")
        || trimmed.contains("(?<!")
        || trimmed.contains(".*")
        || trimmed.contains(".+")
    {
        return true;
    }

    if trimmed.starts_with('^') || trimmed.ends_with('$') {
        return true;
    }

    if trimmed.contains('|') && !trimmed.contains(" | ") {
        return true;
    }

    let open_brackets = trimmed.matches('[').count();
    let close_brackets = trimmed.matches(']').count();
    if open_brackets > 0
        && open_brackets == close_brackets
        && !trimmed.contains(char::is_whitespace)
    {
        return true;
    }

    if trimmed.contains('{') && trimmed.contains('}') && trimmed.chars().any(|c| c.is_ascii_digit())
    {
        return true;
    }

    // Parentheses alone are ambiguous (doc titles and function-call text often
    // contain them). Treat as regex only for balanced compact expressions.
    let open_count = trimmed.matches('(').count();
    let close_count = trimmed.matches(')').count();
    if open_count > 0 || close_count > 0 {
        let compact_group_like = !trimmed.chars().any(|c| c.is_whitespace()) && trimmed.len() <= 64;
        if open_count == close_count && compact_group_like {
            return true;
        }
    }

    if trimmed.contains('+')
        && !trimmed.chars().any(|c| c.is_whitespace())
        && trimmed.chars().any(|c| c.is_ascii_alphabetic())
    {
        return true;
    }

    // Treat '?' as regex only when it's not simple sentence punctuation.
    if trimmed.contains('?') {
        let trailing_only =
            trimmed.ends_with('?') && !trimmed[..trimmed.len().saturating_sub(1)].contains('?');
        let has_whitespace = trimmed.chars().any(|c| c.is_whitespace());
        return !trailing_only && !has_whitespace;
    }

    false
}

/// Check if a query appears to be a glob pattern.
fn is_glob_like(query: &str) -> bool {
    let trimmed = query.trim();
    trimmed.contains('*') || (trimmed.contains('?') && !trimmed.ends_with('?'))
}

/// Check if the query suggests team/cross-project search intent.
fn is_team_query(query_lower: &str) -> bool {
    TEAM_QUERY_KEYWORDS
        .iter()
        .any(|kw| query_lower.contains(kw))
}

/// Check if the query requests all matches.
fn is_all_matches_query(query_lower: &str) -> bool {
    ALL_MATCH_KEYWORDS.iter().any(|kw| query_lower.contains(kw))
}

/// Check if the query asks for counts.
fn is_count_query(query_lower: &str) -> bool {
    COUNT_QUERY_PREFIXES
        .iter()
        .any(|prefix| query_lower.starts_with(prefix))
        || (query_lower.contains("how many") && query_lower.contains("are there"))
}

/// Search modes for redirect recommendations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchMode {
    Hybrid,
    Semantic,
    Keyword,
    Pattern,
    Exhaustive,
    Refactor,
    Team,
}

impl SearchMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Hybrid => "hybrid",
            Self::Semantic => "semantic",
            Self::Keyword => "keyword",
            Self::Pattern => "pattern",
            Self::Exhaustive => "exhaustive",
            Self::Refactor => "refactor",
            Self::Team => "team",
        }
    }
}

/// Recommend a search mode using API-aligned heuristics.
fn recommend_search_mode(query: &str) -> (SearchMode, &'static str) {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return (SearchMode::Hybrid, "Use hybrid for broad discovery.");
    }

    let lower = trimmed.to_lowercase();
    let word_count = trimmed.split_whitespace().count();

    if is_team_query(&lower) {
        return (SearchMode::Team, "Cross-project intent detected.");
    }

    if is_all_matches_query(&lower) {
        return (
            SearchMode::Exhaustive,
            "All-occurrences intent detected. Exhaustive mode is most complete.",
        );
    }

    let quoted = (trimmed.starts_with('"') && trimmed.ends_with('"'))
        || (trimmed.starts_with('\'') && trimmed.ends_with('\''));
    if quoted {
        return (SearchMode::Keyword, "Quoted exact-match query detected.");
    }

    if is_glob_like(trimmed) || has_regex_characters(trimmed) {
        return (SearchMode::Pattern, "Pattern or regex syntax detected.");
    }

    if is_identifier_query(trimmed) {
        return (
            SearchMode::Refactor,
            "Identifier-style query detected. Refactor mode finds symbol usages precisely.",
        );
    }

    let starts_with_question = QUESTION_WORDS.iter().any(|w| lower.starts_with(w));
    if starts_with_question || trimmed.ends_with('?') || word_count >= 3 {
        return (
            SearchMode::Semantic,
            "Natural-language query detected. Semantic mode is a better fit.",
        );
    }

    (
        SearchMode::Hybrid,
        "Hybrid mode provides the best general coverage.",
    )
}

/// Suggest output format for token-efficient redirects.
fn suggest_output_format(query: &str, mode: SearchMode) -> Option<&'static str> {
    let lower = query.trim().to_lowercase();
    if is_count_query(&lower) {
        return Some("count");
    }

    if is_identifier_query(query) {
        return match mode {
            SearchMode::Refactor | SearchMode::Exhaustive => Some("paths"),
            _ => Some("minimal"),
        };
    }

    None
}

/// Stale threshold in days.
const STALE_THRESHOLD_DAYS: f64 = 7.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct IndexStatus {
    is_indexed: bool,
    is_stale: bool,
}

/// Process-local cache for the parsed indexed-projects.json data.
/// Each hook invocation is a separate process, so this caches within a single
/// invocation if get_index_status() is called multiple times.
static CACHED_INDEX_DATA: OnceLock<Option<Value>> = OnceLock::new();

/// Load and cache the indexed-projects.json file (once per process).
fn load_index_data() -> &'static Option<Value> {
    CACHED_INDEX_DATA.get_or_init(|| {
        let index_file =
            dirs::home_dir().map(|h| h.join(".contextstream").join("indexed-projects.json"));

        index_file.and_then(|path| {
            std::fs::read_to_string(&path)
                .ok()
                .and_then(|content| serde_json::from_str::<Value>(&content).ok())
        })
    })
}

fn is_index_timestamp_stale(indexed_at: Option<&str>) -> bool {
    let Some(indexed_at) = indexed_at else {
        return false;
    };

    let Ok(indexed_time) = chrono::DateTime::parse_from_rfc3339(indexed_at) else {
        return false;
    };

    let now = chrono::Utc::now();
    let diff = now.signed_duration_since(indexed_time.with_timezone(&chrono::Utc));
    let diff_days = diff.num_hours() as f64 / 24.0;
    diff_days > STALE_THRESHOLD_DAYS
}

fn index_status_from_data(folder_path: &str, data: &Value) -> IndexStatus {
    let Some(projects) = data.get("projects").and_then(|p| p.as_object()) else {
        return IndexStatus {
            is_indexed: false,
            is_stale: false,
        };
    };

    let folder = Path::new(folder_path);
    let mut best_match: Option<(usize, IndexStatus)> = None;

    for (project_path, info) in projects {
        let project_path = Path::new(project_path);
        if !folder.starts_with(project_path) {
            continue;
        }

        if std::fs::metadata(project_path)
            .map(|meta| meta.is_file())
            .unwrap_or(false)
        {
            continue;
        }

        let candidate_depth = project_path.components().count();
        let candidate_status = IndexStatus {
            is_indexed: true,
            is_stale: is_index_timestamp_stale(info.get("indexed_at").and_then(|t| t.as_str())),
        };

        match best_match {
            None => best_match = Some((candidate_depth, candidate_status)),
            Some((best_depth, _best_status)) if candidate_depth > best_depth => {
                best_match = Some((candidate_depth, candidate_status));
            }
            Some((best_depth, best_status))
                if candidate_depth == best_depth
                    && best_status.is_stale
                    && !candidate_status.is_stale =>
            {
                best_match = Some((candidate_depth, candidate_status));
            }
            _ => {}
        }
    }

    best_match.map(|(_, status)| status).unwrap_or(IndexStatus {
        is_indexed: false,
        is_stale: false,
    })
}

/// Check if a project is indexed in ContextStream.
fn get_index_status(folder_path: &str) -> IndexStatus {
    let Some(data) = load_index_data() else {
        return IndexStatus {
            is_indexed: false,
            is_stale: false,
        };
    };

    index_status_from_data(folder_path, data)
}

fn should_wait_for_initial_index(status: IndexStatus) -> bool {
    !status.is_indexed
}

/// Detect which editor format the input uses.
enum EditorFormat {
    Claude,
    Cline,
    Cursor,
    Windsurf,
}

fn supports_hard_first_call_enforcement(editor: &EditorFormat) -> bool {
    matches!(
        editor,
        EditorFormat::Claude | EditorFormat::Cursor | EditorFormat::Windsurf | EditorFormat::Cline
    )
}

fn detect_editor(input: &Value) -> EditorFormat {
    // 1. Payload-based detection (most specific — check all editors first)

    // Windsurf payload markers
    let windsurf_payload = input
        .get("hook_event_name")
        .and_then(|v| v.as_str())
        .map(|name| name.eq_ignore_ascii_case("pre_mcp_tool_use"))
        .unwrap_or(false)
        || input
            .get("hookEventName")
            .and_then(|v| v.as_str())
            .map(|name| name.eq_ignore_ascii_case("pre_mcp_tool_use"))
            .unwrap_or(false)
        || input.get("tool_info").is_some()
        || input.get("mcp_tool_name").is_some();

    if windsurf_payload {
        return EditorFormat::Windsurf;
    }

    // Cline/Roo/Kilo use camelCase
    if input.get("hookName").is_some() || input.get("toolName").is_some() {
        return EditorFormat::Cline;
    }
    // Cursor uses hook_event_name with different response format
    if input.get("hook_event_name").is_some() && input.get("tool_name").is_none() {
        return EditorFormat::Cursor;
    }
    // Claude Code uses tool_name (snake_case)
    if input.get("tool_name").is_some() {
        return EditorFormat::Claude;
    }

    // 2. Environment-based fallback (when payload has no editor-specific markers)
    // Only use WINDSURF_CASCADE_TERMINAL_KIND — this env var is set exclusively
    // by the Windsurf editor at runtime. Do NOT check PATH for "windsurf" as
    // having Windsurf installed doesn't mean we're running inside it.
    if std::env::var("WINDSURF_CASCADE_TERMINAL_KIND").is_ok() {
        return EditorFormat::Windsurf;
    }

    EditorFormat::Claude
}

/// Extract tool name from any editor format.
fn extract_tool_name(input: &Value) -> String {
    let hook_event_name = input
        .get("hook_event_name")
        .or_else(|| input.get("hookEventName"))
        .or_else(|| input.get("agent_action_name"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if !hook_event_name.is_empty() {
        match hook_event_name.to_ascii_lowercase().as_str() {
            "pre_read_code" => return "Read".to_string(),
            "pre_write_code" => return "Write".to_string(),
            "pre_run_command" => return "Bash".to_string(),
            "pre_user_prompt" => return "UserPrompt".to_string(),
            "pre_mcp_tool_use" => {}
            _ => {}
        }
    }

    input
        .get("tool_name")
        .and_then(|v| v.as_str())
        .or_else(|| input.get("toolName").and_then(|v| v.as_str()))
        .or_else(|| input.get("mcp_tool_name").and_then(|v| v.as_str()))
        .or_else(|| {
            input
                .get("tool_info")
                .and_then(|v| v.get("mcp_tool_name"))
                .and_then(|v| v.as_str())
        })
        .or_else(|| input.get("tool").and_then(|v| v.as_str()))
        .or_else(|| {
            input
                .get("tool")
                .and_then(|v| v.get("name"))
                .and_then(|v| v.as_str())
        })
        .unwrap_or("")
        .to_string()
}

/// Extract tool input from any editor format.
fn extract_tool_input(input: &Value) -> Value {
    if let Some(tool_info) = input.get("tool_info") {
        let hook_event_name = input
            .get("hook_event_name")
            .or_else(|| input.get("hookEventName"))
            .or_else(|| input.get("agent_action_name"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        match hook_event_name.to_ascii_lowercase().as_str() {
            "pre_read_code" | "pre_write_code" => {
                return serde_json::json!({
                    "path": tool_info.get("file_path").and_then(|v| v.as_str()).unwrap_or(""),
                });
            }
            "pre_run_command" => {
                return serde_json::json!({
                    "command": tool_info.get("command_line").and_then(|v| v.as_str()).unwrap_or(""),
                    "cwd": tool_info.get("cwd").and_then(|v| v.as_str()).unwrap_or(""),
                });
            }
            "pre_user_prompt" => {
                return serde_json::json!({
                    "prompt": tool_info.get("user_prompt").and_then(|v| v.as_str()).unwrap_or(""),
                });
            }
            _ => {}
        }
    }

    input
        .get("tool_input")
        .or_else(|| input.get("parameters"))
        .or_else(|| input.get("toolParameters"))
        .or_else(|| input.get("args"))
        .or_else(|| input.get("arguments"))
        .or_else(|| {
            input
                .get("tool_info")
                .and_then(|v| v.get("mcp_tool_arguments"))
        })
        .or_else(|| input.get("tool").and_then(|v| v.get("parameters")))
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}))
}

/// Extract working directory from any editor format.
fn extract_cwd(input: &Value) -> String {
    input
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            input
                .get("tool_info")
                .and_then(|v| v.get("cwd"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .or_else(|| {
            input
                .get("workspace_roots")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .or_else(|| {
            input
                .get("workspaceRoots")
                .and_then(|v| v.as_array())
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        })
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .and_then(|p| p.to_str().map(|s| s.to_string()))
        })
        .unwrap_or_default()
}

/// Decision returned by PreToolUse.
enum HookDecision {
    Allow,
    AllowWithContext(String),
    BlockWithMessage(String),
}

/// Get the tool prefix for search calls in this editor.
fn search_tool_name(editor: &EditorFormat) -> &'static str {
    match editor {
        EditorFormat::Claude => "mcp__contextstream__search",
        EditorFormat::Cline | EditorFormat::Cursor | EditorFormat::Windsurf => "search",
    }
}

/// Get the tool prefix for session calls in this editor.
fn session_tool_name(editor: &EditorFormat) -> &'static str {
    match editor {
        EditorFormat::Claude => "mcp__contextstream__session",
        EditorFormat::Cline | EditorFormat::Cursor | EditorFormat::Windsurf => "session",
    }
}

fn contextstream_tool_name(editor: &EditorFormat, domain: &str) -> String {
    match editor {
        EditorFormat::Claude => format!("mcp__contextstream__{}", domain),
        EditorFormat::Cline | EditorFormat::Cursor | EditorFormat::Windsurf => domain.to_string(),
    }
}

fn stale_index_local_fallback_message(
    editor: &EditorFormat,
    search_call: &str,
    local_tool: &str,
) -> String {
    let project = contextstream_tool_name(editor, "project");
    format!(
        "Project index exists and remains usable even if it may be older than local edits. Use {search_call} before broad {local_tool}. \
         Use targeted local {local_tool} only after search misses a recently edited or newly created file. \
         Refresh in background with {project}(action=\"index\") when accuracy matters."
    )
}

fn search_first_redirect_decision(
    status: IndexStatus,
    block_message: String,
    stale_message: String,
) -> HookDecision {
    // Warn, don't deny. A hard block (deny) on one tool in a parallel
    // batch makes Claude Code cancel the *entire* batch — so a legit
    // sibling call dies because an unrelated grep was redirected. Emit a
    // non-blocking nudge instead: the tool still runs and the agent still
    // sees the steer toward ContextStream search.
    if status.is_stale {
        HookDecision::AllowWithContext(stale_message)
    } else {
        HookDecision::AllowWithContext(block_message)
    }
}

/// A log/diagnostic file argument — grepping these is filtering, never
/// code discovery, so the search-first guard must leave it alone.
fn is_log_or_text_target(token: &str) -> bool {
    let t = token.trim_matches(|c| c == '\'' || c == '"');
    let lower = t.to_ascii_lowercase();
    lower.ends_with(".log") || lower.ends_with(".txt") || lower.starts_with("/var/log")
}

/// A single concrete file target (has a filename extension, no glob/regex
/// metacharacters, not a directory). A non-recursive grep on one file is
/// targeted work — it mirrors the native Grep tool's "scoped path passes"
/// rule and is not broad code discovery.
fn is_concrete_file_target(token: &str) -> bool {
    let t = token.trim_matches(|c| c == '\'' || c == '"');
    if t.is_empty() || t.ends_with('/') {
        return false;
    }
    if t.contains(|c: char| {
        matches!(
            c,
            '*' | '?' | '[' | ']' | '(' | ')' | '|' | '\\' | '$' | '^'
        )
    }) {
        return false;
    }
    let last = t.rsplit('/').next().unwrap_or(t);
    match last.rfind('.') {
        Some(i) => i > 0 && i < last.len() - 1,
        None => false,
    }
}

/// Whether a grep/rg invocation carries a recursive flag (`-r`, `-R`,
/// combined short flags like `-rn`, or `--recursive`). Recursive greps are
/// tree-wide code discovery; non-recursive greps are usually file-scoped.
fn bash_search_has_recursive_flag(head: &str) -> bool {
    head.split_whitespace().any(|tok| {
        tok == "--recursive"
            || (tok.starts_with('-')
                && !tok.starts_with("--")
                && tok.len() > 1
                && tok[1..].chars().all(|c| c.is_ascii_alphabetic())
                && tok[1..].chars().any(|c| c == 'r' || c == 'R'))
    })
}

/// Detects when a Bash command is really code-search via the shell —
/// `grep -rn`, `rg`, `fd`, `find . -name "..."` — so the hook can
/// redirect to `mcp__contextstream__search`. Returns `Some((tool_name,
/// query_hint))` when the command looks like code-search, `None`
/// otherwise.
///
/// Heuristics:
/// - The shell-search tool must be the first token of the (possibly
///   `cd`-prefixed) command. Anything piped INTO grep/rg/etc. is
///   filter/formatting, not code search.
/// - `find` only counts when used with `-name`, `-iname`, `-path`, or
///   `-regex` flags — bare `find` (e.g. metadata search by `-newer`,
///   `-size`, `-mtime`) is legitimate filesystem work.
/// - `grep` piping output of another command (`ps aux | grep foo`,
///   `cat file | grep bar`) is filtering, not code search.
fn detect_bash_code_search(command: &str) -> Option<(&'static str, String)> {
    let trimmed = command.trim();
    if trimmed.is_empty() {
        return None;
    }

    // Strip leading `cd ... && ` / `cd ...; ` so we see the real
    // first command. Multiple stripping passes handle chained `cd`s.
    let mut head = trimmed;
    loop {
        let lowered = head.trim_start();
        if let Some(rest) = lowered
            .strip_prefix("cd ")
            .and_then(|s| s.split_once("&&").or_else(|| s.split_once(';')))
        {
            head = rest.1.trim_start();
            continue;
        }
        break;
    }

    // Bail when the search command is downstream of a pipe.
    // Anything piped INTO grep/rg/etc. is filtering, not code search.
    if head.contains('|') {
        // Only safe to flag when no pipe appears anywhere — too easy
        // to false-positive otherwise.
        return None;
    }

    let first_token = head.split_whitespace().next()?;
    let (tool_name, needs_find_flag) = match first_token {
        "grep" | "egrep" | "fgrep" => ("grep", false),
        "rg" | "ripgrep" => ("rg", false),
        "ag" => ("ag", false),
        "find" => ("find", true),
        "fd" | "fdfind" => ("fd", false),
        _ => return None,
    };

    // For `find`, require a name/path/regex flag — otherwise it's
    // doing filesystem metadata work (size, mtime, perms, etc.)
    // that ContextStream search doesn't replace.
    if needs_find_flag {
        let has_name_flag = head.contains(" -name ")
            || head.contains(" -iname ")
            || head.contains(" -path ")
            || head.contains(" -ipath ")
            || head.contains(" -regex ")
            || head.contains(" -iregex ");
        if !has_name_flag {
            return None;
        }
    }

    // Allowlist legitimate non-code uses so the guard never fires (and so a
    // parallel batch is never cancelled): grepping a log/diagnostic file, or
    // a single concrete file with a non-recursive grep, is filtering — not
    // broad code discovery. Skip silently (no nudge).
    let recursive = bash_search_has_recursive_flag(head);
    for tok in head.split_whitespace().skip(1) {
        if tok.starts_with('-') {
            continue;
        }
        if is_log_or_text_target(tok) {
            return None;
        }
        if !recursive && matches!(tool_name, "grep" | "rg" | "ag") && is_concrete_file_target(tok) {
            return None;
        }
    }

    // Pull a query hint from the command for the redirect message.
    // Best-effort — peek at the last quoted string or the last
    // argument, whichever is more informative.
    let query_hint = extract_query_hint(head).unwrap_or_default();
    Some((tool_name, query_hint))
}

/// Best-effort extraction of a search query from a shell command, used
/// to populate the redirect-suggestion message. Returns empty string
/// when we can't confidently isolate a query.
fn extract_query_hint(command: &str) -> Option<String> {
    // Prefer single- or double-quoted strings.
    if let Some(quoted) = extract_quoted(command, '\'') {
        return Some(quoted);
    }
    if let Some(quoted) = extract_quoted(command, '"') {
        return Some(quoted);
    }
    // Fall back to the last whitespace-separated token that doesn't
    // start with a `-` (so we skip flags) and isn't a path.
    command
        .split_whitespace()
        .rfind(|t| !t.starts_with('-') && !t.contains('/'))
        .map(|s| s.to_string())
}

fn extract_quoted(input: &str, q: char) -> Option<String> {
    let chars = input.char_indices();
    for (i, c) in chars {
        if c == q {
            let start = i + c.len_utf8();
            for (j, c2) in input[start..].char_indices() {
                if c2 == q {
                    let s = &input[start..start + j];
                    if !s.is_empty() {
                        return Some(s.to_string());
                    }
                }
            }
        }
    }
    None
}

fn context_tool_name(editor: &EditorFormat) -> &'static str {
    match editor {
        EditorFormat::Claude => "mcp__contextstream__context",
        EditorFormat::Cline | EditorFormat::Cursor | EditorFormat::Windsurf => "context",
    }
}

fn init_tool_name(editor: &EditorFormat) -> &'static str {
    match editor {
        EditorFormat::Claude => "mcp__contextstream__init",
        EditorFormat::Cline | EditorFormat::Cursor | EditorFormat::Windsurf => "init",
    }
}

fn extract_mcp_server_name(input: &Value) -> String {
    input
        .get("mcp_server_name")
        .and_then(|v| v.as_str())
        .or_else(|| {
            input
                .get("tool_info")
                .and_then(|v| v.get("mcp_server_name"))
                .and_then(|v| v.as_str())
        })
        .unwrap_or("")
        .to_string()
}

fn normalize_contextstream_tool_name(tool_name: &str) -> String {
    tool_name
        .strip_prefix("mcp__contextstream__")
        .unwrap_or(tool_name)
        .to_ascii_lowercase()
}

fn is_contextstream_server_call(server_name: &str, tool_name: &str) -> bool {
    server_name.eq_ignore_ascii_case("contextstream")
        || tool_name.starts_with("mcp__contextstream__")
}

fn is_windsurf_pre_mcp_tool_use(input: &Value) -> bool {
    let hook_event_name = input
        .get("hook_event_name")
        .or_else(|| input.get("hookEventName"))
        .or_else(|| input.get("agent_action_name"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    hook_event_name.eq_ignore_ascii_case("pre_mcp_tool_use")
        || input
            .get("tool_info")
            .and_then(|v| v.get("mcp_tool_name"))
            .is_some()
        || input.get("mcp_tool_name").is_some()
}

fn is_known_contextstream_windsurf_tool(normalized_tool: &str) -> bool {
    matches!(
        normalized_tool,
        "init"
            | "context"
            | "session"
            | "memory"
            | "project"
            | "media"
            | "capsule"
            | "skill"
            | "entity"
            | "qa"
            | "instruct"
            | "flash"
            | "ram"
    ) || normalized_tool.starts_with("search_")
        || normalized_tool.starts_with("session_")
}

fn has_contextstream_search_shape(tool_input: &Value) -> bool {
    first_non_empty_str(
        tool_input,
        &[
            "mode",
            "workspace_id",
            "workspaceId",
            "project_id",
            "projectId",
            "output_format",
            "outputFormat",
        ],
    )
    .is_some()
        || tool_input.get("include_content").is_some()
        || tool_input.get("includeContent").is_some()
        || tool_input.get("include_memory").is_some()
        || tool_input.get("includeMemory").is_some()
        || tool_input.get("file_types").is_some()
        || tool_input.get("fileTypes").is_some()
        || tool_input.get("context_lines").is_some()
        || tool_input.get("contextLines").is_some()
        || tool_input.get("content_max_chars").is_some()
        || tool_input.get("contentMaxChars").is_some()
        || tool_input.get("exact_match_boost").is_some()
        || tool_input.get("exactMatchBoost").is_some()
}

fn is_windsurf_contextstream_tool_call(input: &Value, tool_name: &str, tool_input: &Value) -> bool {
    if !is_windsurf_pre_mcp_tool_use(input) {
        return false;
    }

    let normalized = normalize_contextstream_tool_name(tool_name);
    if is_known_contextstream_windsurf_tool(&normalized) {
        return true;
    }

    normalized == "search" && has_contextstream_search_shape(tool_input)
}

fn is_contextstream_tool_call(
    editor: &EditorFormat,
    input: &Value,
    server_name: &str,
    tool_name: &str,
    tool_input: &Value,
) -> bool {
    is_contextstream_server_call(server_name, tool_name)
        || (matches!(editor, EditorFormat::Windsurf)
            && is_windsurf_contextstream_tool_call(input, tool_name, tool_input))
}

fn is_mcp_tool_call(input: &Value, server_name: &str, tool_name: &str) -> bool {
    if is_contextstream_server_call(server_name, tool_name) {
        return true;
    }

    if !server_name.trim().is_empty() || tool_name.starts_with("mcp__") {
        return true;
    }

    input
        .get("tool_info")
        .and_then(|v| v.get("mcp_tool_name"))
        .is_some()
        || input.get("mcp_tool_name").is_some()
}

fn is_search_first_applicable(tool_lower: &str, tool_input: &Value) -> bool {
    match tool_lower {
        "glob"
        | "grep"
        | "search"
        | "grep_search"
        | "code_search"
        | "read"
        | "read_file"
        | "semanticsearch"
        | "codebase_search"
        | "list_files"
        | "search_files"
        | "search_files_content"
        | "find_files"
        | "find_by_name" => true,
        "task" => first_str(tool_input, &["subagent_type", "agent", "type"])
            .map(|subagent| subagent.eq_ignore_ascii_case("explore"))
            .unwrap_or(false),
        _ => false,
    }
}

/// Tools that fan out into code/file discovery — nudge unread `[GROUNDING]` first.
/// ContextStream's own `search` tool is excluded (it is the preferred path, not a bypass).
fn is_grounding_target_tool(
    is_contextstream_call: bool,
    tool_lower: &str,
    tool_input: &Value,
) -> bool {
    if is_contextstream_call && tool_lower == "search" {
        return false;
    }
    match tool_lower {
        "grep" | "read" | "read_file" | "semanticsearch" | "codebase_search" | "code_search"
        | "grep_search" | "find_by_name" => true,
        "task" => first_str(tool_input, &["subagent_type", "agent", "type"])
            .map(|subagent| subagent.eq_ignore_ascii_case("explore"))
            .unwrap_or(false),
        _ => false,
    }
}

/// Upgrade bare `Allow` when unread `[GROUNDING]` hits exist (never overrides blocks).
fn maybe_nudge_unread_grounding(
    decision: HookDecision,
    unread: Option<mcp_session::grounding_state::GroundingSummary>,
    session_tool: &str,
) -> HookDecision {
    match (decision, unread) {
        (HookDecision::Allow, Some(summary)) if summary.hit_count > 0 => {
            let source_summary = if summary.top_kinds.is_empty() {
                String::new()
            } else {
                format!(" Sources: {}.", summary.top_kinds.join(", "))
            };
            let decision_summary = if summary.decision_count > 0 {
                format!(" Includes {} decision hit(s).", summary.decision_count)
            } else {
                String::new()
            };
            let stale_summary = if summary.stale_count > 0 {
                format!(
                    " {} stale/time-sensitive hit(s) need a freshness refresh before planning or implementing from them.",
                    summary.stale_count
                )
            } else {
                String::new()
            };
            let date_summary = match (
                summary.newest_source_at.as_deref(),
                summary.oldest_source_at.as_deref(),
            ) {
                (Some(newest), Some(oldest)) if newest != oldest => {
                    format!(" Source dates: {oldest} to {newest}.")
                }
                (Some(only), _) => format!(" Source date: {only}."),
                _ => String::new(),
            };
            HookDecision::AllowWithContext(format!(
                "[GROUNDING_AVAILABLE] Your last context() call surfaced {} prior-work hits in [GROUNDING] you haven't read yet.{}{}{}{} Read those first or call {session_tool}(action=\"ground\", user_message=\"...\") for a one-shot bundle. If a stale decision or transcript would affect the plan, refresh with {session_tool}(action=\"ground\", user_message=\"...\") or the suggested memory/session call before relying on it.",
                summary.hit_count,
                source_summary,
                decision_summary,
                stale_summary,
                date_summary,
            ))
        }
        (d, _) => d,
    }
}

fn configured_index_wait_seconds() -> u64 {
    std::env::var("CONTEXTSTREAM_INDEX_WAIT_SECONDS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .map(|seconds| seconds.clamp(MIN_INDEX_WAIT_SECONDS, MAX_INDEX_WAIT_SECONDS))
        .unwrap_or(DEFAULT_INDEX_WAIT_SECONDS)
}

fn is_local_discovery_tool_during_index_wait(tool_lower: &str, tool_input: &Value) -> bool {
    match tool_lower {
        "glob" | "semanticsearch" | "codebase_search" | "explore" => true,
        "task" => first_str(tool_input, &["subagent_type", "agent", "type"])
            .map(|subagent| subagent.eq_ignore_ascii_case("explore"))
            .unwrap_or(false),
        "search" | "grep_search" | "code_search" => {
            let path = first_str(tool_input, &["path", "file_path"])
                .unwrap_or("")
                .trim();
            is_discovery_path(path)
        }
        // Native grep is intentionally excluded: the PreToolUse hook never
        // stops or nudges Grep, so the index-refresh gate must not block it
        // either.
        "grep" => false,
        "read" | "read_file" => {
            let path = first_str(tool_input, &["file_path", "path", "file", "target_file"])
                .unwrap_or("")
                .trim();
            is_discovery_path(path)
        }
        "list_files" | "search_files" | "search_files_content" | "find_files" | "find_by_name" => {
            let query = first_str(
                tool_input,
                &["path", "regex", "pattern", "query", "text", "directory"],
            )
            .unwrap_or("")
            .trim();
            query.is_empty() || is_discovery_path(query) || is_discovery_glob(query)
        }
        _ => false,
    }
}

fn is_contextstream_read_only_operation(tool_name: &str, tool_input: &Value) -> bool {
    let action = first_str(tool_input, &["action"])
        .unwrap_or("")
        .to_ascii_lowercase();
    match tool_name {
        "workspace" => matches!(action.as_str(), "list" | "get"),
        "memory" => matches!(
            action.as_str(),
            "list_docs"
                | "list_events"
                | "list_todos"
                | "list_tasks"
                | "list_transcripts"
                | "list_nodes"
                | "decisions"
                | "get_doc"
                | "get_event"
                | "get_task"
                | "get_todo"
                | "get_transcript"
        ),
        "session" => matches!(
            action.as_str(),
            "get_lessons" | "get_plan" | "list_plans" | "recall"
        ),
        "help" => matches!(action.as_str(), "version" | "tools" | "auth"),
        "project" => matches!(action.as_str(), "list" | "get" | "index_status"),
        "reminder" => matches!(action.as_str(), "list" | "active"),
        "instruct" | "flash" | "ram" => matches!(action.as_str(), "get" | "stats"),
        "context" | "init" => true,
        _ => false,
    }
}

fn is_likely_state_changing_tool(
    tool_lower: &str,
    tool_input: &Value,
    is_contextstream_call: bool,
    normalized_contextstream_tool: &str,
) -> bool {
    if is_contextstream_call {
        return !is_contextstream_read_only_operation(normalized_contextstream_tool, tool_input);
    }

    if matches!(
        tool_lower,
        "read"
            | "read_file"
            | "grep"
            | "glob"
            | "search"
            | "grep_search"
            | "code_search"
            | "semanticsearch"
            | "codebase_search"
            | "list_files"
            | "search_files"
            | "search_files_content"
            | "find_files"
            | "find_by_name"
            | "ls"
            | "cat"
            | "view"
    ) {
        return false;
    }

    let write_markers = [
        "write", "edit", "create", "delete", "remove", "rename", "move", "patch", "apply",
        "insert", "append", "replace", "update", "commit", "push", "install", "exec", "run",
        "bash", "shell",
    ];
    write_markers
        .iter()
        .any(|marker| tool_lower.contains(marker))
}

/// Escape text for a double-quoted function argument.
fn escape_for_double_quotes(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Build a search call snippet for messages.
fn search_call(editor: &EditorFormat, mode: SearchMode, query: &str) -> String {
    let mode_name = if query.trim().is_empty() && mode == SearchMode::Hybrid {
        "auto"
    } else {
        mode.as_str()
    };
    let mut args = vec![format!("mode=\"{}\"", mode_name)];
    if query.trim().is_empty() {
        args.push("query=\"...\"".to_string());
    } else {
        args.push(format!(
            "query=\"{}\"",
            escape_for_double_quotes(query.trim())
        ));
    }

    if let Some(output_format) = suggest_output_format(query, mode) {
        args.push(format!("output_format=\"{}\"", output_format));
    }

    format!("{}({})", search_tool_name(editor), args.join(", "))
}

/// Build a session plan capture call snippet.
fn plan_capture_call(editor: &EditorFormat) -> String {
    format!(
        "{}(action=\"capture_plan\", title=\"...\", description=\"scope, constraints, affected areas, acceptance criteria, verification\", goals=[...], steps=[{{\"id\":\"plan-step-1\",\"title\":\"...\",\"order\":1,\"description\":\"scope, concrete work, acceptance criteria, verification\"}}], create_tasks=true)",
        session_tool_name(editor)
    )
}

fn plan_task_create_call(editor: &EditorFormat) -> String {
    format!(
        "{}(action=\"create_task\", title=\"...\", description=\"concrete work, acceptance criteria, verification\", plan_id=\"...\", plan_step_id=\"plan-step-1\", priority=\"medium\", task_status=\"pending\")",
        contextstream_tool_name(editor, "memory")
    )
}

fn is_conventional_handoff_file_path(file_path: &str) -> bool {
    let Some(file_name) = Path::new(file_path)
        .file_name()
        .and_then(|name| name.to_str())
    else {
        return false;
    };
    matches!(
        file_name.to_ascii_lowercase().as_str(),
        "handoff.md" | "handoff.txt" | "agent-handoff.md" | "agent_handoff.md" | ".handoff.md"
    )
}

fn handoff_file_write_decision(
    editor: &EditorFormat,
    hook_input: &Value,
    file_path: &str,
) -> Option<HookDecision> {
    if !is_conventional_handoff_file_path(file_path) {
        return None;
    }

    let transcript_path = first_non_empty_str(hook_input, &["transcript_path", "transcriptPath"]);
    let intent = transcript_path
        .and_then(super::save_intent::latest_handoff_intent_from_transcript)
        .unwrap_or(super::save_intent::HandoffIntent::None);
    let entity = contextstream_tool_name(editor, "entity");
    let capsule = contextstream_tool_name(editor, "capsule");
    let entity_call = format!(
        "{entity}(kind=\"handoff\", action=\"create\", body={{\"title\":\"...\",\"summary\":\"...\",\"scope\":\"...\",\"next_steps\":[...]}})"
    );
    let capsule_call = format!(
        "{capsule}(action=\"create\", scope=\"session\", session_id=\"<current session id>\", purpose=\"handoff\")"
    );

    match intent {
        super::save_intent::HandoffIntent::Canonical => Some(HookDecision::BlockWithMessage(
            format!(
                "Blocked local handoff substitute: the user requested an agent/session handoff, not a local handoff file. Create the canonical ContextStream record with {entity_call}. Preserve verified facts, eliminated hypotheses, branch/commit state, environment gotchas, validation, blockers, and ordered next steps. If a portable bundle or share link was requested, also call {capsule_call}. Do not replace either with HANDOFF.md, a generic document/event, a scratch prompt, or prose."
            ),
        )),
        super::save_intent::HandoffIntent::ExplicitLocalFile => {
            Some(HookDecision::AllowWithContext(format!(
                "The user explicitly requested this local handoff file, so it may be an additional artifact. The canonical handoff must still be created first with {entity_call}; add {capsule_call} when a portable bundle or share link was requested."
            )))
        }
        super::save_intent::HandoffIntent::None => Some(HookDecision::AllowWithContext(format!(
            "A local HANDOFF.md-style file is not a canonical ContextStream handoff. If this file is intended to hand work to another agent/session, create {entity_call} and use {capsule_call} only for a requested portable bundle/share link. Keep the local file only when the user explicitly requested it."
        ))),
    }
}

fn contextstream_surface_nudge(
    editor: &EditorFormat,
    normalized_tool: &str,
    action: &str,
    tool_input: &Value,
) -> Option<String> {
    match normalized_tool {
        "session" if action.eq_ignore_ascii_case("capture_plan") || action.eq_ignore_ascii_case("update_plan") => {
            Some(
                "Plan linked_items support is available. Keep refs in indexed form and prefer kinds: doc, diagram, runbook, handoff (each with kind+id; optional title_snapshot/status_snapshot/updated_at). Team guidance is also surfaced via session(action=\"context\") instructions/team fields."
                    .to_string(),
            )
        }
        "entity" => {
            let kind = first_str(tool_input, &["kind"]).unwrap_or("").trim().to_ascii_lowercase();
            if kind == "handoff" {
                let capsule = contextstream_tool_name(editor, "capsule");
                Some(format!(
                    "Canonical handoff reminder: preserve verified facts, eliminated hypotheses, scope, branch/commit and validation state, blockers, and ordered next_steps. Omit unknown to_user_id. If the user requested a portable bundle/capsule/share link, additionally create {capsule}(action=\"create\", scope=\"session\", session_id=\"<current session id>\", purpose=\"handoff\"); do not create HANDOFF.md as a substitute."
                ))
            } else if kind == "ticket" {
                Some(
                    "Ticket linked_items support includes diagram refs. Use indexed refs (kind+id) and optional snapshots; avoid URL-only links. Team context/instructions are the primary surfacing channel for non-hook agents."
                        .to_string(),
                )
            } else {
                None
            }
        }
        "skill" if action.eq_ignore_ascii_case("share") => Some(format!(
            "Team skill sharing reminder: share supports scope=team|public. \
             For team scope, ensure workspace_id is resolved (run {}(action=\"init\", folder_path=\"...\") first when needed).",
            session_tool_name(editor)
        )),
        "skill" if action.eq_ignore_ascii_case("list") || action.eq_ignore_ascii_case("get") => {
            Some(
                "Skill governance cues are surfaced in results (scope/visibility/workspace/owner when available). Prefer these fields when choosing shared team skills; context/instructions remain the primary source for broad team guidance."
                    .to_string(),
            )
        }
        _ => None,
    }
}

fn contextstream_action(tool_input: &Value) -> String {
    first_str(tool_input, &["action"])
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

fn is_plan_event_type(tool_input: &Value) -> bool {
    first_str(tool_input, &["event_type", "eventType"])
        .map(str::trim)
        .map(|event_type| event_type.eq_ignore_ascii_case("plan"))
        .unwrap_or(false)
}

fn misrouted_plan_event_message(
    editor: &EditorFormat,
    normalized_tool: &str,
    action: &str,
    tool_input: &Value,
) -> Option<String> {
    let is_misrouted = ((normalized_tool == "session" && action == "capture")
        || normalized_tool == "session_capture"
        || (normalized_tool == "memory" && action == "create_event"))
        && is_plan_event_type(tool_input);

    if is_misrouted {
        Some(format!(
            "Do not save plans as generic events. Use {} so the plan is stored as a real plan, then verify linked tasks with {}.",
            plan_capture_call(editor),
            plan_task_create_call(editor)
        ))
    } else {
        None
    }
}

fn string_value_has_detail(value: Option<&Value>, min_words: usize, min_chars: usize) -> bool {
    let Some(value) = value.and_then(|value| value.as_str()).map(str::trim) else {
        return false;
    };
    value.chars().count() >= min_chars && value.split_whitespace().count() >= min_words
}

fn capture_plan_quality_block_message(
    editor: &EditorFormat,
    normalized_tool: &str,
    action: &str,
    tool_input: &Value,
) -> Option<String> {
    let is_capture_plan = (normalized_tool == "session" && action == "capture_plan")
        || normalized_tool == "capture_plan";
    if !is_capture_plan {
        return None;
    }

    let steps = tool_input.get("steps").and_then(|value| value.as_array());
    let Some(steps) = steps.filter(|steps| !steps.is_empty()) else {
        return Some(format!(
            "capture_plan requires detailed structured steps. Use {} and let create_tasks=true create linked tasks.",
            plan_capture_call(editor)
        ));
    };

    for step in steps {
        if !string_value_has_detail(step.get("description"), 6, 35) {
            return Some(format!(
                "Each capture_plan step needs a useful description with scope, concrete work, acceptance criteria, and verification. Use {}.",
                plan_capture_call(editor)
            ));
        }
    }

    None
}

fn is_visibility_sensitive_contextstream_call(normalized_tool: &str, action: &str) -> bool {
    match normalized_tool {
        "memory" => matches!(
            action,
            "create_node"
                | "update_node"
                | "create_event"
                | "update_event"
                | "import_batch"
                | "create_task"
                | "update_task"
                | "create_todo"
                | "update_todo"
                | "create_diagram"
                | "update_diagram"
                | "create_doc"
                | "update_doc"
                | "create_roadmap"
        ),
        "session" => matches!(
            action,
            "capture" | "capture_lesson" | "remember" | "capture_plan" | "update_plan"
        ),
        "project" => matches!(action, "index" | "ingest_local"),
        "media" => action == "index",
        // Dedicated tool names that may be exposed in some editors.
        "session_capture"
        | "session_capture_lesson"
        | "session_remember"
        | "capture_plan"
        | "update_plan" => true,
        _ => false,
    }
}

fn visibility_follow_up_call(
    editor: &EditorFormat,
    normalized_tool: &str,
    action: &str,
) -> Option<String> {
    let memory_tool = contextstream_tool_name(editor, "memory");
    let session_tool = contextstream_tool_name(editor, "session");
    let project_tool = contextstream_tool_name(editor, "project");
    let media_tool = contextstream_tool_name(editor, "media");

    match (normalized_tool, action) {
        ("memory", "create_doc") | ("memory", "update_doc") | ("memory", "create_roadmap") => {
            Some(format!("{}(action=\"list_docs\", limit=20)", memory_tool))
        }
        ("memory", "create_task") | ("memory", "update_task") => {
            Some(format!("{}(action=\"list_tasks\", limit=20)", memory_tool))
        }
        ("memory", "create_todo") | ("memory", "update_todo") => {
            Some(format!("{}(action=\"list_todos\", limit=20)", memory_tool))
        }
        ("memory", "create_node") | ("memory", "update_node") => {
            Some(format!("{}(action=\"list_nodes\", limit=20)", memory_tool))
        }
        ("memory", "create_event") | ("memory", "update_event") | ("memory", "import_batch") => {
            Some(format!("{}(action=\"list_events\", limit=20)", memory_tool))
        }
        ("memory", "create_diagram") | ("memory", "update_diagram") => Some(format!(
            "{}(action=\"list_diagrams\", limit=20)",
            memory_tool
        )),
        ("session", "capture_plan")
        | ("session", "update_plan")
        | ("capture_plan", _)
        | ("update_plan", _) => Some(format!(
            "{}(action=\"list_plans\", include_tasks=true, limit=20)",
            session_tool
        )),
        ("session", "capture_lesson") | ("session_capture_lesson", _) => Some(format!(
            "{}(action=\"get_lessons\", limit=10)",
            session_tool
        )),
        ("session", "capture") | ("session_capture", _) => {
            Some(format!("{}(action=\"list_events\", limit=20)", memory_tool))
        }
        ("session", "remember") | ("session_remember", _) => Some(format!(
            "{}(action=\"recall\", query=\"...\", limit=5)",
            session_tool
        )),
        ("project", "index") | ("project", "ingest_local") => {
            Some(format!("{}(action=\"index_status\")", project_tool))
        }
        ("media", "index") => Some(format!(
            "{}(action=\"status\", content_id=\"...\")",
            media_tool
        )),
        _ => None,
    }
}

fn collect_string_values(value: &Value, values: &mut Vec<String>) {
    match value {
        Value::String(s) => values.push(s.to_ascii_lowercase()),
        Value::Array(items) => {
            for item in items {
                collect_string_values(item, values);
            }
        }
        Value::Object(map) => {
            for item in map.values() {
                collect_string_values(item, values);
            }
        }
        _ => {}
    }
}

fn should_nudge_todo_persistence(tool_input: &Value, in_plan_subagent: bool) -> bool {
    if in_plan_subagent {
        return true;
    }
    if tool_input.get("plan_id").is_some() || tool_input.get("planId").is_some() {
        return true;
    }

    let mut values = Vec::new();
    collect_string_values(tool_input, &mut values);
    values.iter().any(|value| {
        value.contains("plan")
            || value.contains("roadmap")
            || value.contains("milestone")
            || value.contains("deliverable")
            || value.contains("phase ")
    })
}

fn tool_input_contains_remember_language(tool_input: &Value) -> bool {
    let mut values = Vec::new();
    collect_string_values(tool_input, &mut values);
    values.iter().any(|value| {
        value.contains("remember")
            || value.contains("don't forget")
            || value.contains("dont forget")
            || value.contains("keep in mind")
            || value.contains("save for later")
            || value.contains("save to memory")
            || value.contains("note this down")
    })
}

/// Read first string value from a list of keys.
fn first_str<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| value.get(*k).and_then(|v| v.as_str()))
}

/// Read first non-empty string value from a list of keys.
fn first_non_empty_str<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    first_str(value, keys)
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

fn clears_init_gate(is_contextstream_call: bool, normalized_tool: &str) -> bool {
    is_contextstream_call && matches!(normalized_tool, "init" | "context")
}

fn clears_context_gate(is_contextstream_call: bool, normalized_tool: &str) -> bool {
    is_contextstream_call && normalized_tool == "context"
}

fn allowed_before_context_without_clearing(
    is_contextstream_call: bool,
    normalized_tool: &str,
) -> bool {
    is_contextstream_call && normalized_tool == "init"
}

fn is_scope_sensitive_contextstream_tool(tool_name: &str) -> bool {
    matches!(
        tool_name,
        "context"
            | "search"
            | "graph"
            | "project"
            | "session"
            | "memory"
            | "integration"
            | "media"
            | "capsule"
            | "entity"
            | "instruct"
            | "flash"
            | "ram"
    )
}

/// Build Cursor's `preToolUse` / `beforeMCPExecution` / `beforeShellExecution`
/// output.
///
/// Cursor's contract is `{ "permission": "allow"|"deny", "agent_message",
/// "user_message" }`. The old `{ "decision", "reason" }` shape is silently
/// ignored by current Cursor, so denies never fired and messages never reached
/// the agent. Note that `allow` cannot inject context — only `deny.agent_message`
/// (here) and `postToolUse.additional_context` can — so allow-with-context is
/// downgraded to a plain allow and the nudge is delivered post-tool.
fn cursor_pre_tool_output(decision: &HookDecision) -> Value {
    match decision {
        HookDecision::Allow | HookDecision::AllowWithContext(_) => {
            serde_json::json!({ "permission": "allow" })
        }
        HookDecision::BlockWithMessage(msg) => serde_json::json!({
            "permission": "deny",
            "agent_message": msg,
            "user_message": msg,
        }),
    }
}

/// Write output in the appropriate editor format.
fn write_output(editor: &EditorFormat, decision: HookDecision) -> Result<()> {
    match editor {
        EditorFormat::Claude => match decision {
            HookDecision::Allow => write_stdout_json(&HookOutput::empty())?,
            HookDecision::AllowWithContext(msg) => {
                write_stdout_json(&HookOutput::context(msg))?;
            }
            HookDecision::BlockWithMessage(msg) => {
                write_stdout_json(&HookOutput::deny_pre_tool_use(msg))?;
            }
        },
        EditorFormat::Cline => match decision {
            HookDecision::Allow => {
                println!(
                    "{}",
                    serde_json::to_string(&serde_json::json!({"cancel": false}))?
                );
            }
            HookDecision::AllowWithContext(msg) => {
                let output = serde_json::json!({
                    "cancel": false,
                    "contextModification": format!("[CONTEXTSTREAM] {}", msg),
                });
                println!("{}", serde_json::to_string(&output)?);
            }
            HookDecision::BlockWithMessage(msg) => {
                let output = serde_json::json!({
                    "cancel": true,
                    "errorMessage": msg,
                    "contextModification": format!("[CONTEXTSTREAM] {}", msg),
                });
                println!("{}", serde_json::to_string(&output)?);
            }
        },
        EditorFormat::Cursor => {
            println!(
                "{}",
                serde_json::to_string(&cursor_pre_tool_output(&decision))?
            );
        }
        EditorFormat::Windsurf => match decision {
            HookDecision::Allow => {}
            HookDecision::AllowWithContext(msg) => {
                eprintln!("[CONTEXTSTREAM] {}", msg);
            }
            HookDecision::BlockWithMessage(msg) => {
                eprintln!("{}", msg);
                std::process::exit(2);
            }
        },
    }
    Ok(())
}

async fn team_memory_action_guard(
    normalized_tool: &str,
    tool_input: &Value,
    config: &super::common::ApiConfig,
) -> Option<String> {
    if normalized_tool != "memory" {
        return None;
    }
    let action = tool_input
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if !action.starts_with("team_") {
        return None;
    }

    if matches!(
        std::env::var("CONTEXTSTREAM_ACCOUNT_MODE")
            .ok()
            .as_deref()
            .map(str::trim)
            .map(|s| s.to_ascii_lowercase()),
        Some(mode) if mode == "personal"
    ) {
        return Some(
            "Team memory action blocked: CONTEXTSTREAM_ACCOUNT_MODE=personal environment variable is set. This overrides session settings. To enable team actions, unset or change CONTEXTSTREAM_ACCOUNT_MODE to 'team' or 'auto'."
                .to_string(),
        );
    }

    if !config.is_configured() {
        return None;
    }

    let client_config = mcp_types::Config {
        api_url: config.api_url.clone(),
        api_key: Some(config.api_key.clone()),
        default_workspace_id: config
            .workspace_id
            .as_ref()
            .and_then(|id| uuid::Uuid::parse_str(id).ok()),
        default_project_id: config
            .project_id
            .as_ref()
            .and_then(|id| uuid::Uuid::parse_str(id).ok()),
        ..Default::default()
    };
    let client = mcp_client::ContextStreamClient::new(client_config);
    let ctx = client.get_account_context().await.ok().flatten()?;
    if !ctx.team_features_available() {
        return Some(
            "Team memory action blocked: authenticated account has no active team membership. Using personal scope."
                .to_string(),
        );
    }
    None
}

/// Handle the PreToolUse hook.
/// Whether the pre-search index drain should fire before this tool runs: only
/// for the ContextStream search/context calls that read the code index, so a
/// just-made edit is searchable by the request that follows.
fn should_predrain_for_tool(is_contextstream_call: bool, normalized_tool: &str) -> bool {
    is_contextstream_call && matches!(normalized_tool, "search" | "context")
}

pub async fn handle() -> Result<()> {
    let input = read_stdin_json()?;

    let editor = detect_editor(&input);
    let tool = extract_tool_name(&input);
    let tool_input = extract_tool_input(&input);
    let cwd = extract_cwd(&input);
    let mcp_server_name = extract_mcp_server_name(&input);
    let normalized_contextstream_tool = normalize_contextstream_tool_name(&tool);
    let tool_lower = tool.to_ascii_lowercase();
    let is_contextstream_call =
        is_contextstream_tool_call(&editor, &input, &mcp_server_name, &tool, &tool_input);
    let is_mcp_tool = is_mcp_tool_call(&input, &mcp_server_name, &tool);
    let record_state_change = is_likely_state_changing_tool(
        &tool_lower,
        &tool_input,
        is_contextstream_call,
        &normalized_contextstream_tool,
    );

    // Keep the index hot for the turn's searches: when the agent is about to
    // call ContextStream search/context, synchronously flush this folder's
    // pending edits first so just-edited code is searchable by the request that
    // follows. Bounded (<=1.5s) and fails open — it never blocks the tool.
    if should_predrain_for_tool(is_contextstream_call, &normalized_contextstream_tool) {
        super::dirty_drain::drain_now_sync(&cwd, std::time::Duration::from_millis(1500)).await;
    }

    // Load config once for compliance emission (best-effort, lazy).
    let compliance_config = super::common::load_config(&cwd);

    // Resolve the active model id once per hook invocation. The result is
    // also written into the file-backed session model cache so subsequent
    // hooks (different processes) inherit it. `None` here means we never
    // saw a registry-recognized model — emit() will leave model_id absent
    // and the API will tag the row as `unknown`.
    let resolved_model_id =
        compliance::resolve_model_id(None, Some(&input), Some("PreToolUse"), None);
    let session_id_for_compliance = input
        .get("session_id")
        .or_else(|| input.get("sessionId"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let emit_compliance = |mut event: ComplianceEvent| {
        if event.model_id.is_none() {
            event.model_id = resolved_model_id.clone();
        }
        if event.session_id.is_none() {
            event.session_id = session_id_for_compliance.clone();
        }
        compliance::emit_for_hook(&compliance_config, Some(&input), Some("PreToolUse"), event);
    };

    let emit = |decision: HookDecision| -> Result<()> {
        if !matches!(decision, HookDecision::BlockWithMessage(_)) && record_state_change {
            prompt_state::mark_state_changed(&cwd);
        }
        write_output(&editor, decision)
    };

    if is_contextstream_call {
        if let Some(msg) = team_memory_action_guard(
            &normalized_contextstream_tool,
            &tool_input,
            &compliance_config,
        )
        .await
        {
            return emit(HookDecision::BlockWithMessage(msg));
        }
    }

    // Session-start requirement: enforce init(...) before other MCP calls.
    // Prompt requirement: enforce context(...) before other MCP calls.
    if supports_hard_first_call_enforcement(&editor) {
        prompt_state::cleanup_stale(180);
        if prompt_state::is_init_required(&cwd) {
            if clears_init_gate(is_contextstream_call, &normalized_contextstream_tool) {
                prompt_state::clear_init_required(&cwd);
                emit_compliance(ComplianceEvent {
                    rule_key: compliance::RULE_INIT_REQUIRED,
                    rule_class: RuleClass::Procedural,
                    check_type: CheckType::Deterministic,
                    result: CheckResult::Pass,
                    severity: 1,
                    metadata: Some(serde_json::json!({
                        "tool": tool,
                        "reason": "init_gate_satisfied_by_init_or_context"
                    })),
                    ..Default::default()
                });
            } else if is_mcp_tool || is_contextstream_call {
                let required = format!("{}(...)", init_tool_name(&editor));
                let quick = format!(
                    "{}(user_message=\"...\", session_id=\"session-...\")",
                    context_tool_name(&editor)
                );
                let msg = format!(
                    "First call required for this session: {} or {} for quick-start. \
                     Call one of them before other MCP tools. If you use {}, call {} later when you need explicit session/index setup.",
                    required, quick, quick, required,
                );
                emit_compliance(ComplianceEvent {
                    rule_key: compliance::RULE_INIT_REQUIRED,
                    rule_class: RuleClass::Hard,
                    check_type: CheckType::Deterministic,
                    result: CheckResult::Fail,
                    severity: 5,
                    metadata: Some(serde_json::json!({
                        "blocked_tool": tool,
                        "reason": "blocked_non_init_while_init_required"
                    })),
                    ..Default::default()
                });
                emit(HookDecision::BlockWithMessage(msg))?;
                return Ok(());
            }
        }

        if prompt_state::is_context_required(&cwd) {
            if clears_context_gate(is_contextstream_call, &normalized_contextstream_tool) {
                prompt_state::clear_context_required(&cwd);
                emit_compliance(ComplianceEvent {
                    rule_key: compliance::RULE_CONTEXT_REQUIRED,
                    rule_class: RuleClass::Procedural,
                    check_type: CheckType::Deterministic,
                    result: CheckResult::Pass,
                    severity: 1,
                    metadata: Some(serde_json::json!({
                        "tool": tool,
                        "reason": "context_gate_satisfied_by_context"
                    })),
                    ..Default::default()
                });
            } else if allowed_before_context_without_clearing(
                is_contextstream_call,
                &normalized_contextstream_tool,
            ) {
                // init(...) is allowed before context(...) so the required order
                // can be init -> context on the first message in a session.
            } else if !is_mcp_tool {
                // Local/builtin tools should not hard-fail on first-turn policy
                // reminders. Keep the requirement pending until the next MCP tool.
            } else {
                // Warn, don't block. Forcing context() before every
                // capture/decision/memory write (and denying otherwise)
                // fired mid-task and cost an extra round-trip just to record
                // something. Let the write proceed with a one-time nudge, and
                // clear the gate so the reminder shows at most once per prompt
                // rather than on every subsequent tool call this turn.
                prompt_state::clear_context_required(&cwd);
                let required = format!(
                    "{}(user_message=\"...\", session_id=\"session-...\")",
                    context_tool_name(&editor)
                );
                let msg = format!(
                    "Tip: call {} at the start of a turn for the best context (lessons, decisions, matched skills). \
                     Proceeding without it; reuse the same session_id for follow-up turns.",
                    required,
                );
                emit_compliance(ComplianceEvent {
                    rule_key: compliance::RULE_CONTEXT_REQUIRED,
                    rule_class: RuleClass::Soft,
                    check_type: CheckType::Deterministic,
                    result: CheckResult::Fail,
                    severity: 2,
                    metadata: Some(serde_json::json!({
                        "tool": tool,
                        "reason": "context_not_called_first_soft_nudge"
                    })),
                    ..Default::default()
                });
                emit(HookDecision::AllowWithContext(msg))?;
                return Ok(());
            }
        }
    }

    if is_contextstream_call {
        if normalized_contextstream_tool == "context" {
            if first_non_empty_str(&tool_input, &["session_id", "sessionId"]).is_some() {
                emit_compliance(ComplianceEvent {
                    rule_key: compliance::RULE_SESSION_CONTINUITY,
                    rule_class: RuleClass::Procedural,
                    check_type: CheckType::Deterministic,
                    result: CheckResult::Pass,
                    severity: 1,
                    metadata: None,
                    ..Default::default()
                });
            } else {
                emit_compliance(ComplianceEvent {
                    rule_key: compliance::RULE_SESSION_CONTINUITY,
                    rule_class: RuleClass::Procedural,
                    check_type: CheckType::Deterministic,
                    result: CheckResult::Fail,
                    severity: 2,
                    metadata: Some(serde_json::json!({
                        "tool": tool,
                        "reason": "missing_session_id",
                    })),
                    ..Default::default()
                });
            }
        }

        if is_scope_sensitive_contextstream_tool(&normalized_contextstream_tool) {
            let req_workspace_id =
                first_non_empty_str(&tool_input, &["workspace_id", "workspaceId"]);
            let req_project_id = first_non_empty_str(&tool_input, &["project_id", "projectId"]);

            if let Some(project_id) = req_project_id {
                if req_workspace_id.is_none() {
                    // Compliance event only — don't surface a user-visible
                    // advisory. The server auto-resolves workspace_id from
                    // project_id, so rendering a "scope alignment" notice on
                    // every call reads as a warning without changing behavior.
                    emit_compliance(ComplianceEvent {
                        rule_key: compliance::RULE_SCOPE_ALIGNMENT,
                        rule_class: RuleClass::Procedural,
                        check_type: CheckType::Deterministic,
                        result: CheckResult::Fail,
                        severity: 2,
                        metadata: Some(serde_json::json!({
                            "tool": tool,
                            "reason": "project_without_workspace",
                            "project_id": project_id,
                        })),
                        ..Default::default()
                    });
                }
            }

            if let (Some(req_ws), Some(req_pid), Some(cfg_ws), Some(cfg_pid)) = (
                req_workspace_id,
                req_project_id,
                compliance_config.workspace_id.as_deref(),
                compliance_config.project_id.as_deref(),
            ) {
                if req_ws != cfg_ws || req_pid != cfg_pid {
                    emit_compliance(ComplianceEvent {
                        rule_key: compliance::RULE_SCOPE_ALIGNMENT,
                        rule_class: RuleClass::Procedural,
                        check_type: CheckType::Deterministic,
                        result: CheckResult::Fail,
                        severity: 2,
                        metadata: Some(serde_json::json!({
                            "tool": tool,
                            "reason": "scope_mismatch",
                            "requested_workspace_id": req_ws,
                            "requested_project_id": req_pid,
                            "configured_workspace_id": cfg_ws,
                            "configured_project_id": cfg_pid,
                        })),
                        ..Default::default()
                    });
                    let msg = format!(
                        "Scope alignment warning: requested workspace/project differs from local \
                         ContextStream config. If this is unintentional, use workspace_id=\"{}\" \
                         and project_id=\"{}\" for this repo.",
                        cfg_ws, cfg_pid
                    );
                    emit(HookDecision::AllowWithContext(msg))?;
                    return Ok(());
                }
            }

            if req_workspace_id.is_some() && req_project_id.is_some() {
                emit_compliance(ComplianceEvent {
                    rule_key: compliance::RULE_SCOPE_ALIGNMENT,
                    rule_class: RuleClass::Procedural,
                    check_type: CheckType::Deterministic,
                    result: CheckResult::Pass,
                    severity: 1,
                    metadata: None,
                    ..Default::default()
                });
            }
        }
    }

    if is_contextstream_call {
        let action = contextstream_action(&tool_input);
        if let Some(msg) = misrouted_plan_event_message(
            &editor,
            &normalized_contextstream_tool,
            &action,
            &tool_input,
        ) {
            emit_compliance(ComplianceEvent {
                rule_key: compliance::RULE_PLAN_PERSISTENCE,
                rule_class: RuleClass::Hard,
                check_type: CheckType::Deterministic,
                result: CheckResult::Fail,
                severity: 4,
                metadata: Some(serde_json::json!({
                    "tool": tool,
                    "action": action,
                    "reason": "plan_saved_as_generic_event",
                })),
                ..Default::default()
            });
            emit(HookDecision::BlockWithMessage(msg))?;
            return Ok(());
        }

        if let Some(msg) = capture_plan_quality_block_message(
            &editor,
            &normalized_contextstream_tool,
            &action,
            &tool_input,
        ) {
            emit_compliance(ComplianceEvent {
                rule_key: compliance::RULE_PLAN_PERSISTENCE,
                rule_class: RuleClass::Hard,
                check_type: CheckType::Deterministic,
                result: CheckResult::Fail,
                severity: 4,
                metadata: Some(serde_json::json!({
                    "tool": tool,
                    "action": action,
                    "reason": "thin_or_missing_plan_steps",
                })),
                ..Default::default()
            });
            emit(HookDecision::BlockWithMessage(msg))?;
            return Ok(());
        }
    }

    // Planning tools: let them execute, then nudge saving to ContextStream.
    // Each editor has its own planning tools:
    //   Claude Code: EnterPlanMode
    //   Cursor: SwitchMode, TodoWrite
    //   Windsurf: todo_list, exitplanmode
    //   Cline/Roo/Kilo: plan_mode_respond, plan_mode_start
    if tool == "EnterPlanMode"
        || tool == "SwitchMode"
        || tool_lower == "plan_mode_respond"
        || tool_lower == "plan_mode_start"
    {
        let search = search_call(&editor, SearchMode::Hybrid, "");
        let msg = format!(
            "Plan mode does NOT bypass ContextStream search-first. Start with {} before broad file reads \
             and avoid Explore/file-by-file scans. After finalizing your plan, save it to ContextStream \
             (not a local markdown file): {}. \
             Then create tasks with {}.",
            search,
            plan_capture_call(&editor),
            plan_task_create_call(&editor)
        );
        emit_compliance(ComplianceEvent {
            rule_key: compliance::RULE_PLAN_PERSISTENCE,
            rule_class: RuleClass::Soft,
            check_type: CheckType::Deterministic,
            result: CheckResult::Pass,
            severity: 2,
            metadata: Some(serde_json::json!({ "tool": tool })),
            ..Default::default()
        });
        emit(HookDecision::AllowWithContext(msg))?;
        return Ok(());
    }

    let in_plan_subagent = super::subagent_state::get_active_subagent(&cwd)
        .map(|a| a.agent_type.eq_ignore_ascii_case("plan"))
        .unwrap_or(false);
    if (tool_lower == "todo_list" || tool_lower == "todowrite")
        && should_nudge_todo_persistence(&tool_input, in_plan_subagent)
    {
        let msg = format!(
            "Also save these plan/tasks to ContextStream so they persist across sessions: {}. \
             Create individual tasks with {}.",
            plan_capture_call(&editor),
            plan_task_create_call(&editor)
        );
        emit(HookDecision::AllowWithContext(msg))?;
        return Ok(());
    }

    if tool_lower == "exitplanmode" {
        let msg = format!(
            "Before exiting plan mode, make sure the plan is saved to ContextStream: {}. \
             Plans in ContextStream persist across sessions and are searchable.",
            plan_capture_call(&editor)
        );
        emit(HookDecision::AllowWithContext(msg))?;
        return Ok(());
    }

    // Memory tools: nudge to use ContextStream instead of editor built-in memory.
    //   Windsurf: create_memory
    if tool_lower == "create_memory" {
        let memory_tool = session_tool_name(&editor).replace("session", "memory");
        let msg = format!(
            "Also save this to ContextStream so it persists across sessions and stays searchable: \
             {}(action=\"capture\", event_type=\"decision|insight|operation|uncategorized\", title=\"...\", content=\"...\") \
             or {}(action=\"create_node\", node_type=\"fact|preference\", title=\"...\", content=\"...\").",
            session_tool_name(&editor),
            memory_tool
        );
        emit(HookDecision::AllowWithContext(msg))?;
        return Ok(());
    }

    if (tool_lower == "create_memory" || tool_lower == "todowrite" || tool_lower == "todo_list")
        && tool_input_contains_remember_language(&tool_input)
    {
        let session = session_tool_name(&editor);
        let msg = format!(
            "The content contains language like \"remember\" or \"don't forget\". \
             For best persistence across sessions, also use {}(action=\"remember\", content=\"...\") \
             which tags it for automatic surfacing in future context.",
            session
        );
        emit(HookDecision::AllowWithContext(msg))?;
        return Ok(());
    }

    // Doc/spec file writes: nudge saving to ContextStream.
    // (Compliance: doc_persistence rule — emitted inline below for coverage.)

    // Long-running persistence actions: require user-visible progress updates.
    let contextstream_action_name = contextstream_action(&tool_input);
    if is_contextstream_call
        && is_visibility_sensitive_contextstream_call(
            &normalized_contextstream_tool,
            &contextstream_action_name,
        )
    {
        let op_label = if contextstream_action_name.is_empty() {
            normalized_contextstream_tool.as_str()
        } else {
            contextstream_action_name.as_str()
        };
        let follow_up = visibility_follow_up_call(
            &editor,
            &normalized_contextstream_tool,
            &contextstream_action_name,
        );
        let msg = if let Some(follow_up_call) = follow_up {
            format!(
                "User visibility requirement: this ContextStream operation ({}) can take a few seconds \
                 or run in background. Post a brief progress update before and after the tool call \
                 (for example: 'Saving to ContextStream...' then 'Saved successfully'). \
                 If the response is queued/accepted, tell the user processing continues in background \
                 and verify completion with {}.",
                op_label, follow_up_call
            )
        } else {
            format!(
                "User visibility requirement: this ContextStream operation ({}) can take a few seconds \
                 or run in background. Post a brief progress update before and after the tool call \
                 (for example: 'Saving to ContextStream...' then 'Saved successfully').",
                op_label
            )
        };
        emit_compliance(ComplianceEvent {
            rule_key: compliance::RULE_VISIBILITY,
            rule_class: RuleClass::Soft,
            check_type: CheckType::Deterministic,
            result: CheckResult::Pass,
            severity: 1,
            metadata: Some(serde_json::json!({ "operation": op_label })),
            ..Default::default()
        });
        emit(HookDecision::AllowWithContext(msg))?;
        return Ok(());
    }

    // Intercept plan file writes and doc file writes
    if matches!(
        tool_lower.as_str(),
        "write_to_file" | "create_file" | "write" | "edit" | "multiedit" | "notebookedit"
    ) {
        let raw_file_path = first_str(
            &tool_input,
            &["file_path", "path", "target_file", "TargetFile"],
        )
        .unwrap_or("");

        if let Some(decision) = handoff_file_write_decision(&editor, &input, raw_file_path) {
            match &decision {
                HookDecision::BlockWithMessage(_) => emit_compliance(ComplianceEvent {
                    rule_key: compliance::RULE_HANDOFF_PERSISTENCE,
                    rule_class: RuleClass::Hard,
                    check_type: CheckType::Deterministic,
                    result: CheckResult::Fail,
                    severity: 4,
                    metadata: Some(serde_json::json!({
                        "tool": tool,
                        "artifact": "handoff_file",
                        "reason": "local_handoff_substitute_blocked",
                    })),
                    ..Default::default()
                }),
                HookDecision::Allow | HookDecision::AllowWithContext(_) => {
                    emit_compliance(ComplianceEvent {
                        rule_key: compliance::RULE_HANDOFF_PERSISTENCE,
                        rule_class: RuleClass::Soft,
                        check_type: CheckType::Deterministic,
                        result: CheckResult::Pass,
                        severity: 2,
                        metadata: Some(serde_json::json!({
                            "tool": tool,
                            "artifact": "handoff_file",
                            "reason": "local_handoff_file_nudged",
                        })),
                        ..Default::default()
                    })
                }
            }
            emit(decision)?;
            return Ok(());
        }

        let file_path = raw_file_path.to_lowercase();

        // Plan file writes (e.g. .windsurf/plans/, plan*.md)
        if file_path.contains(".windsurf/plans")
            || file_path.contains(".cursor/plans")
            || (file_path.ends_with(".md") && file_path.contains("plan"))
        {
            let msg = format!(
                "Instead of writing a plan to a local file, save it to ContextStream where it \
                 persists across sessions: {}. \
                 Then create tasks with {}.",
                plan_capture_call(&editor),
                plan_task_create_call(&editor)
            );
            emit(HookDecision::AllowWithContext(msg))?;
            return Ok(());
        }

        // Doc/spec/notes file writes
        if file_path.contains("docs/")
            || file_path.contains("notes/")
            || file_path.contains("specs/")
            || file_path.ends_with(".spec.md")
            || (file_path.ends_with(".md")
                && (file_path.contains("implementation")
                    || file_path.contains("design")
                    || file_path.contains("architecture")
                    || file_path.contains("spec")))
        {
            let memory_tool = session_tool_name(&editor).replace("session", "memory");
            let msg = format!(
                "Consider saving this document to ContextStream where it persists across sessions \
                 and is searchable: {}(action=\"create_doc\", title=\"...\", content=\"...\", \
                 doc_type=\"spec|general\"). Only write to a local file if the user explicitly \
                 requested a specific file path.",
                memory_tool
            );
            emit_compliance(ComplianceEvent {
                rule_key: compliance::RULE_DOC_PERSISTENCE,
                rule_class: RuleClass::Soft,
                check_type: CheckType::Deterministic,
                result: CheckResult::Pass,
                severity: 2,
                metadata: Some(serde_json::json!({
                    "tool": tool,
                    "file_path": file_path,
                })),
                ..Default::default()
            });
            emit(HookDecision::AllowWithContext(msg))?;
            return Ok(());
        }
    }

    // ContextStream's own tools must never be blocked by search-first redirection
    // or the initial index-wait gate. The hook redirects non-CS tools TO
    // ContextStream search; blocking CS tools would create a circular block.
    if is_contextstream_call {
        let action = contextstream_action_name.clone();
        if let Some(msg) = contextstream_surface_nudge(
            &editor,
            &normalized_contextstream_tool,
            &action,
            &tool_input,
        ) {
            emit(HookDecision::AllowWithContext(msg))?;
        } else {
            emit(HookDecision::Allow)?;
        }
        return Ok(());
    }

    // Check index status. Indexed-but-stale coverage remains usable for
    // existing code, so only missing coverage gets the initial wait window.
    let status = get_index_status(&cwd);
    if !should_wait_for_initial_index(status) {
        prompt_state::clear_index_wait_window(&cwd);
    } else {
        let wait_seconds = configured_index_wait_seconds();
        let is_local_discovery_tool =
            is_local_discovery_tool_during_index_wait(&tool_lower, &tool_input);
        if is_local_discovery_tool {
            let was_already_active = prompt_state::index_wait_remaining_seconds(&cwd).is_some();
            prompt_state::start_index_wait_window(&cwd, wait_seconds);
            if let Some(remaining) = prompt_state::index_wait_remaining_seconds(&cwd) {
                // First call in the window: emit a strong block so the agent
                // pivots to ContextStream. Subsequent calls within the same
                // window allow-with-context (silent steer) so the user doesn't
                // see a wall of "blocking error" noise from repeated retries.
                let search = search_call(&editor, SearchMode::Hybrid, "");
                if !was_already_active {
                    let msg = format!(
                        "Project index is warming up (~{remaining}s). Use {search} — it returns committed results now and fills in as indexing completes. Use local discovery only if search itself returns nothing."
                    );
                    emit(HookDecision::BlockWithMessage(msg))?;
                } else {
                    emit(HookDecision::AllowWithContext(format!(
                        "Project index is still warming up (~{remaining}s). Run {search} first — it returns committed results now and fills in as indexing completes. Use local discovery only if search itself returns nothing."
                    )))?;
                }
                return Ok(());
            }
            emit(HookDecision::AllowWithContext(format!(
                "Indexing is still catching up (~{}s elapsed). Prefer ContextStream search; use local discovery only if search itself returns nothing — don't skip ContextStream.",
                wait_seconds
            )))?;
            return Ok(());
        }

        // Non-discovery tools should continue while refresh runs in background.
        emit(HookDecision::Allow)?;
        return Ok(());
    }

    // === Project has usable indexed coverage - redirect broad discovery to ContextStream ===

    // Clean up stale subagent entries (safety valve for crashed agents)
    super::subagent_state::cleanup_stale_subagents(30);

    // Check if we're inside an active Explore subagent
    let in_explore = super::subagent_state::get_active_subagent(&cwd)
        .map(|a| a.agent_type.eq_ignore_ascii_case("explore"))
        .unwrap_or(false);

    let decision = match tool_lower.as_str() {
        // Claude Code tools
        "glob" => {
            let pattern = first_str(&tool_input, &["pattern", "path"])
                .unwrap_or("")
                .trim();

            // Inside Explore: block ALL glob patterns, redirect to search
            if in_explore {
                let query = if is_generic_discovery_glob(pattern) || pattern.is_empty() {
                    ""
                } else {
                    pattern
                };
                let (mode, _) = recommend_search_mode(query);
                let call = search_call(&editor, mode, query);
                search_first_redirect_decision(
                    status,
                    format!(
                        "This project index is usable. Use {} instead of Glob for faster, richer code results.",
                        call
                    ),
                    stale_index_local_fallback_message(&editor, &call, &tool),
                )
            } else if is_discovery_glob(pattern) {
                let query = if is_generic_discovery_glob(pattern) {
                    ""
                } else {
                    pattern
                };
                let (mode, reason) = recommend_search_mode(query);
                let call = search_call(&editor, mode, query);
                search_first_redirect_decision(
                    status,
                    format!(
                        "Project index is usable. Use {} instead of broad glob \"{}\". {}",
                        call, pattern, reason
                    ),
                    stale_index_local_fallback_message(&editor, &call, &tool),
                )
            } else {
                HookDecision::Allow
            }
        }
        "grep" => {
            // Claude Code's native Grep tool. Earlier this was a hard
            // pass-through because any PreToolUse output on Grep
            // surfaces as a UI notice the user can read as a warning.
            // That trade-off was reconsidered: the cost of letting
            // broad discovery grep through is users paying for
            // premium ContextStream search and then watching agents
            // bypass it. We now mirror the `glob` / `grep_search`
            // dispatch — scoped Grep on a known file passes, broad
            // Grep across the repo gets redirected with a
            // BlockWithMessage that points at the right
            // mcp__contextstream__search call.
            let pattern = first_str(&tool_input, &["pattern", "query", "regex"])
                .unwrap_or("")
                .trim();
            let path = first_str(&tool_input, &["path", "file_path"])
                .unwrap_or("")
                .trim();

            // Targeted Grep on a known file/scoped path: allow.
            if !path.is_empty() && !is_discovery_path(path) {
                HookDecision::Allow
            } else if !pattern.is_empty() {
                let (mode, reason) = recommend_search_mode(pattern);
                let call = search_call(&editor, mode, pattern);
                search_first_redirect_decision(
                    status,
                    format!(
                        "Project index is usable. Use {} instead of broad Grep on the repo. {}",
                        call, reason
                    ),
                    stale_index_local_fallback_message(&editor, &call, &tool),
                )
            } else {
                HookDecision::Allow
            }
        }
        "bash" => {
            // Bash invocations that are really code-search in disguise
            // (`grep -rn`, `rg`, `fd`, `find ... -name`) bypass
            // ContextStream's premium search just as visibly as the
            // built-in Grep tool. Detect the search-intent pattern
            // and redirect.
            //
            // Heuristics — keep the false-positive surface narrow:
            // - First token is grep/find/rg/fd/ag (not piped INTO).
            // - Pipes / process-list filters are excluded (`ps | grep`,
            //   `cat file | grep`).
            // - find without code-search flags (`-name`, `-iname`,
            //   `-path`, `-regex`) passes (e.g. `find -newer`).
            let command = first_str(&tool_input, &["command"]).unwrap_or("").trim();
            if let Some((tool_name, query_hint)) = detect_bash_code_search(command) {
                let (mode, _) = recommend_search_mode(&query_hint);
                let call = search_call(&editor, mode, &query_hint);
                search_first_redirect_decision(
                    status,
                    format!(
                        "Use {} instead of shell `{}` for code discovery — the project index already has the answer, ranked. Local shell search is still available for non-code uses (process lists, log filtering, etc.).",
                        call, tool_name
                    ),
                    stale_index_local_fallback_message(&editor, &call, &tool),
                )
            } else {
                HookDecision::Allow
            }
        }
        "search" | "grep_search" | "code_search" => {
            let pattern = first_str(&tool_input, &["pattern", "query", "regex", "text"])
                .unwrap_or("")
                .trim();
            let path = first_str(&tool_input, &["path", "file_path"])
                .unwrap_or("")
                .trim();

            // Targeted: allow.
            if !path.is_empty() && !is_discovery_path(path) {
                HookDecision::Allow
            } else if in_explore
                && (is_discovery_path(path) || path.is_empty())
                && !pattern.is_empty()
            {
                let (mode, _) = recommend_search_mode(pattern);
                let call = search_call(&editor, mode, pattern);
                search_first_redirect_decision(
                    status,
                    format!(
                        "This project index is usable. Use {} instead of {} for faster, richer code results.",
                        call, tool
                    ),
                    stale_index_local_fallback_message(&editor, &call, &tool),
                )
            } else if is_discovery_path(path) && !pattern.is_empty() {
                let (mode, reason) = recommend_search_mode(pattern);
                let call = search_call(&editor, mode, pattern);
                search_first_redirect_decision(
                    status,
                    format!(
                        "Project index is usable. Use {} instead of broad {}. {}",
                        call, tool, reason
                    ),
                    stale_index_local_fallback_message(&editor, &call, &tool),
                )
            } else {
                HookDecision::Allow
            }
        }
        "read" | "read_file" => {
            let path = first_str(&tool_input, &["file_path", "path", "file", "target_file"])
                .unwrap_or("")
                .trim();

            if is_discovery_path(path) {
                let call = search_call(&editor, SearchMode::Hybrid, "");
                search_first_redirect_decision(
                    status,
                    format!(
                        "Project index is usable. Use {} before broad reads over project roots.",
                        call
                    ),
                    stale_index_local_fallback_message(&editor, &call, &tool),
                )
            } else if in_explore {
                // Inside Explore: allow reads but nudge toward search
                let call = search_call(&editor, SearchMode::Hybrid, "");
                HookDecision::AllowWithContext(format!(
                    "Avoid file-by-file reads in Explore. Use {} with include_content=true first, \
                     then read only targeted file ranges.",
                    call
                ))
            } else {
                HookDecision::Allow
            }
        }
        "task" => {
            let subagent = first_str(&tool_input, &["subagent_type", "agent", "type"])
                .unwrap_or("")
                .to_lowercase();
            match subagent.to_lowercase().as_str() {
                "explore" => {
                    // Allow Explore agents — SubagentStart hook will inject
                    // ContextStream search context into the subagent.
                    // Blocking prevents SubagentStart from ever firing.
                    let call = search_call(&editor, SearchMode::Hybrid, "");
                    HookDecision::AllowWithContext(format!(
                        "Prefer {} instead of launching Explore for broad code discovery. \
                         If Explore is still used, keep it narrow and avoid file-by-file scans; \
                         the SubagentStart hook will enforce search-first guidance.",
                        call
                    ))
                }
                "plan" => HookDecision::AllowWithContext(format!(
                    "After your plan is ready, save it: {}",
                    plan_capture_call(&editor)
                )),
                _ => HookDecision::Allow,
            }
        }

        // Cursor's built-in semantic search — redirect to ContextStream search
        "semanticsearch" | "codebase_search" => {
            let query = first_str(&tool_input, &["query"]).unwrap_or("").trim();
            if !query.is_empty() {
                let (mode, reason) = recommend_search_mode(query);
                let call = search_call(&editor, mode, query);
                search_first_redirect_decision(
                    status,
                    format!(
                        "Project index is usable. Use {} instead of SemanticSearch. \
                         ContextStream search is faster and returns richer results for indexed projects. {}",
                        call, reason
                    ),
                    stale_index_local_fallback_message(&editor, &call, &tool),
                )
            } else {
                let call = search_call(&editor, SearchMode::Hybrid, "");
                search_first_redirect_decision(
                    status,
                    format!(
                        "Project index is usable. Use {} instead of SemanticSearch.",
                        call
                    ),
                    stale_index_local_fallback_message(&editor, &call, &tool),
                )
            }
        }

        // Cline/Roo/Kilo tool names + Windsurf find_by_name
        "list_files" | "search_files" | "search_files_content" | "find_files" | "find_by_name" => {
            let pattern = first_str(
                &tool_input,
                &["path", "regex", "pattern", "query", "text", "directory"],
            )
            .unwrap_or("")
            .trim();
            if is_discovery_glob(pattern) || is_discovery_path(pattern) || pattern.is_empty() {
                let query = if pattern.is_empty()
                    || is_discovery_path(pattern)
                    || is_generic_discovery_glob(pattern)
                {
                    ""
                } else {
                    pattern
                };
                let (mode, reason) = recommend_search_mode(query);
                let call = search_call(&editor, mode, query);
                search_first_redirect_decision(
                    status,
                    format!(
                        "Project index is usable. Use {} instead of {} for broad discovery. {}",
                        call, tool, reason
                    ),
                    stale_index_local_fallback_message(&editor, &call, &tool),
                )
            } else {
                HookDecision::Allow
            }
        }

        _ => HookDecision::Allow,
    };

    // Auto-grounding nudge: when `context()` left unread `[GROUNDING]` hits,
    // upgrade a bare `Allow` to `AllowWithContext` for local code-discovery tools.
    let grounding_target =
        is_grounding_target_tool(is_contextstream_call, &tool_lower, &tool_input);
    let unread_grounding = if grounding_target {
        mcp_session::grounding_state::peek_unread_summary(&cwd)
    } else {
        None
    };
    let had_unread_grounding = unread_grounding.is_some();

    let mut decision = decision;
    if grounding_target {
        let session = session_tool_name(&editor);
        decision = maybe_nudge_unread_grounding(decision, unread_grounding, session);
        mcp_session::grounding_state::record_grounding_target_tool(&cwd);
    }

    if grounding_target && had_unread_grounding {
        let (result, severity, meta) = match &decision {
            HookDecision::AllowWithContext(msg) if msg.contains("[GROUNDING_AVAILABLE]") => (
                CheckResult::Pass,
                2,
                serde_json::json!({ "nudged_tool": tool, "kind": "grounding_nudge" }),
            ),
            HookDecision::BlockWithMessage(_) => (
                CheckResult::Pass,
                2,
                serde_json::json!({ "blocked_tool": tool, "kind": "grounding_nudge_skipped" }),
            ),
            HookDecision::AllowWithContext(_) => (
                CheckResult::Pass,
                2,
                serde_json::json!({ "nudged_tool": tool, "kind": "grounding_nudge_skipped_existing_context" }),
            ),
            HookDecision::Allow => (
                CheckResult::Pass,
                1,
                serde_json::json!({ "note": "grounding_unexpected_allow" }),
            ),
        };
        emit_compliance(ComplianceEvent {
            rule_key: compliance::RULE_GROUNDING_FIRST,
            rule_class: RuleClass::Procedural,
            check_type: CheckType::Heuristic,
            result,
            severity,
            metadata: Some(meta),
            ..Default::default()
        });
    }

    // Emit compliance events for search-first only when that rule was applicable
    // for the current tool path (prevents false pass inflation).
    let search_first_applicable = is_search_first_applicable(&tool_lower, &tool_input);

    if search_first_applicable {
        match &decision {
            HookDecision::BlockWithMessage(_) => {
                emit_compliance(ComplianceEvent {
                    rule_key: compliance::RULE_SEARCH_FIRST,
                    rule_class: RuleClass::Procedural,
                    check_type: CheckType::Heuristic,
                    result: CheckResult::Fail,
                    severity: 3,
                    metadata: Some(serde_json::json!({ "blocked_tool": tool })),
                    ..Default::default()
                });
            }
            HookDecision::Allow => {
                emit_compliance(ComplianceEvent {
                    rule_key: compliance::RULE_SEARCH_FIRST,
                    rule_class: RuleClass::Procedural,
                    check_type: CheckType::Heuristic,
                    result: CheckResult::Pass,
                    severity: 1,
                    metadata: None,
                    ..Default::default()
                });
            }
            HookDecision::AllowWithContext(_) => {
                // Soft nudges (e.g., Explore subagent) — pass with guidance.
                emit_compliance(ComplianceEvent {
                    rule_key: compliance::RULE_SEARCH_FIRST,
                    rule_class: RuleClass::Procedural,
                    check_type: CheckType::Heuristic,
                    result: CheckResult::Pass,
                    severity: 2,
                    metadata: Some(serde_json::json!({ "nudged_tool": tool })),
                    ..Default::default()
                });
            }
        }
    }

    emit(decision)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn predrain_fires_only_for_contextstream_search_and_context() {
        // The pre-search drain runs ahead of the index-reading tools.
        assert!(should_predrain_for_tool(true, "search"));
        assert!(should_predrain_for_tool(true, "context"));
        // Not for other ContextStream tools, and not for non-ContextStream tools.
        assert!(!should_predrain_for_tool(true, "memory"));
        assert!(!should_predrain_for_tool(true, "init"));
        assert!(!should_predrain_for_tool(false, "search"));
    }

    #[test]
    fn mode_recommendation_keyword_for_quoted_text() {
        let (mode, _) = recommend_search_mode("\"exact string\"");
        assert_eq!(mode, SearchMode::Keyword);
    }

    // ---- detect_bash_code_search ----

    #[test]
    fn bash_search_detects_grep_recursive() {
        let (tool, hint) = detect_bash_code_search("grep -rn handle_oauth crates/").unwrap();
        assert_eq!(tool, "grep");
        // Last non-flag, non-path token is the query.
        assert_eq!(hint, "handle_oauth");
    }

    #[test]
    fn bash_search_detects_grep_with_quoted_query() {
        let (tool, hint) = detect_bash_code_search("grep -nE 'pub async fn handle' src/").unwrap();
        assert_eq!(tool, "grep");
        // Quoted query takes precedence over last-token.
        assert_eq!(hint, "pub async fn handle");
    }

    #[test]
    fn bash_search_detects_ripgrep() {
        let (tool, _hint) = detect_bash_code_search("rg --type rust 'PgPool' .").unwrap();
        assert_eq!(tool, "rg");
    }

    #[test]
    fn bash_search_detects_fd() {
        let (tool, _hint) = detect_bash_code_search("fd '\\.rs$' crates/").unwrap();
        assert_eq!(tool, "fd");
    }

    #[test]
    fn bash_search_detects_find_with_name() {
        let (tool, _hint) = detect_bash_code_search("find . -name '*.rs' -type f").unwrap();
        assert_eq!(tool, "find");
    }

    #[test]
    fn bash_search_strips_cd_prefix() {
        let (tool, _) = detect_bash_code_search("cd /home/foo && grep -rn foo .").unwrap();
        assert_eq!(tool, "grep");
    }

    // ---- negative cases (false-positive guard) ----

    #[test]
    fn bash_search_skips_piped_grep_filter() {
        // `ps aux | grep contextstream` is filtering, not code search.
        assert!(detect_bash_code_search("ps aux | grep contextstream").is_none());
    }

    #[test]
    fn bash_search_skips_cat_pipe_to_grep() {
        // Reading a file's output into grep is filter/extract, not
        // discovery search.
        assert!(detect_bash_code_search("cat /tmp/log | grep ERROR").is_none());
    }

    #[test]
    fn bash_search_skips_find_without_name_flag() {
        // `find` for metadata (size, mtime, owner) is legitimate
        // filesystem work, not code discovery — pass through.
        assert!(detect_bash_code_search("find . -mtime -7 -type f").is_none());
        assert!(detect_bash_code_search("find /var/log -size +10M").is_none());
    }

    #[test]
    fn bash_search_skips_log_file_grep() {
        // Grepping a log/diagnostic file is filtering, not code discovery —
        // this is the exact case that wrongly cancelled a parallel batch.
        assert!(detect_bash_code_search("grep -n ERROR /tmp/app.log").is_none());
        assert!(detect_bash_code_search("grep -rn ERROR /var/log/syslog").is_none());
        assert!(detect_bash_code_search("grep -i warning deploy.txt").is_none());
    }

    #[test]
    fn bash_search_skips_targeted_single_file_grep() {
        // A non-recursive grep on one concrete file is targeted work,
        // mirroring the native Grep tool's "scoped path passes" rule.
        assert!(detect_bash_code_search("grep -n foo src/main.rs").is_none());
        assert!(detect_bash_code_search("grep -n handler ./crates/api/lib.rs").is_none());
    }

    #[test]
    fn bash_search_still_flags_recursive_code_grep() {
        // Recursive/tree-wide code discovery must still be detected.
        assert!(detect_bash_code_search("grep -rn handle_oauth crates/").is_some());
        assert!(detect_bash_code_search("grep -nE 'pub async fn handle' src/").is_some());
        assert!(detect_bash_code_search("rg --type rust 'PgPool' .").is_some());
    }

    #[test]
    fn bash_search_skips_unrelated_commands() {
        assert!(detect_bash_code_search("git status").is_none());
        assert!(detect_bash_code_search("cargo build").is_none());
        assert!(detect_bash_code_search("npm test").is_none());
    }

    #[test]
    fn bash_search_handles_empty_and_whitespace() {
        assert!(detect_bash_code_search("").is_none());
        assert!(detect_bash_code_search("   ").is_none());
    }

    #[test]
    fn blocks_plan_saved_as_session_event() {
        let input = serde_json::json!({
            "action": "capture",
            "event_type": "plan",
            "title": "Bad plan",
            "content": "This should use capture_plan"
        });

        let msg = misrouted_plan_event_message(&EditorFormat::Claude, "session", "capture", &input)
            .expect("plan event should be blocked");

        assert!(msg.contains("Do not save plans as generic events"));
        assert!(msg.contains("capture_plan"));
    }

    #[test]
    fn blocks_plan_saved_as_memory_event() {
        let input = serde_json::json!({
            "action": "create_event",
            "event_type": "plan",
            "title": "Bad plan"
        });

        let msg =
            misrouted_plan_event_message(&EditorFormat::Claude, "memory", "create_event", &input)
                .expect("memory plan event should be blocked");

        assert!(msg.contains("capture_plan"));
    }

    #[test]
    fn blocks_capture_plan_without_detailed_steps() {
        let input = serde_json::json!({
            "action": "capture_plan",
            "title": "Thin plan",
            "steps": [{"id": "plan-step-1", "title": "Do it", "order": 1}]
        });

        let msg = capture_plan_quality_block_message(
            &EditorFormat::Claude,
            "session",
            "capture_plan",
            &input,
        )
        .expect("thin capture_plan should be blocked");

        assert!(msg.contains("Each capture_plan step"));
        assert!(msg.contains("acceptance criteria"));
    }

    fn hook_input_with_transcript(prompt: &str) -> (tempfile::TempDir, Value) {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("transcript.jsonl");
        let record = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": prompt}]
            }
        });
        std::fs::write(&path, format!("{record}\n")).expect("write transcript");
        let input = serde_json::json!({"transcript_path": path});
        (temp, input)
    }

    #[test]
    fn blocks_handoff_md_for_a_generic_agent_handoff() {
        let (_temp, input) =
            hook_input_with_transcript("Please prepare a handoff for the next agent.");
        let decision =
            handoff_file_write_decision(&EditorFormat::Claude, &input, "/tmp/project/HANDOFF.md")
                .expect("handoff decision");
        match decision {
            HookDecision::BlockWithMessage(message) => {
                assert!(message.contains("Blocked local handoff substitute"));
                assert!(message.contains("mcp__contextstream__entity"));
                assert!(message.contains("mcp__contextstream__capsule"));
                assert!(message.contains("HANDOFF.md"));
            }
            _ => panic!("generic handoff file must be blocked"),
        }
    }

    #[test]
    fn explicit_handoff_md_request_is_allowed_only_as_an_additional_artifact() {
        let (_temp, input) = hook_input_with_transcript(
            "Create HANDOFF.md at the repository root for the next agent.",
        );
        let decision = handoff_file_write_decision(&EditorFormat::Claude, &input, "HANDOFF.md")
            .expect("handoff decision");
        match decision {
            HookDecision::AllowWithContext(message) => {
                assert!(message.contains("explicitly requested"));
                assert!(message.contains("additional artifact"));
                assert!(message.contains("canonical handoff must still be created first"));
            }
            _ => panic!("explicit local handoff file should be allowed with guidance"),
        }
    }

    #[test]
    fn handoff_guard_ignores_non_conventional_repository_docs() {
        let (_temp, input) =
            hook_input_with_transcript("Document the handoff protocol for this service.");
        assert!(handoff_file_write_decision(
            &EditorFormat::Claude,
            &input,
            "/tmp/project/docs/handoff-protocol.md"
        )
        .is_none());
    }

    #[test]
    fn mode_recommendation_pattern_for_regex() {
        let (mode, _) = recommend_search_mode("foo\\s+bar");
        assert_eq!(mode, SearchMode::Pattern);
    }

    #[test]
    fn mode_recommendation_not_pattern_for_doc_title_with_parens() {
        let (mode, _) = recommend_search_mode(
            "Search Reliability Fix Plan (Server-Side) - Active Scope (2026-02-17)",
        );
        assert_ne!(mode, SearchMode::Pattern);
        assert_eq!(mode, SearchMode::Semantic);
    }

    #[test]
    fn mode_recommendation_pattern_for_regex_with_parens() {
        let (mode, _) = recommend_search_mode("(error|warning)\\s+handler");
        assert_eq!(mode, SearchMode::Pattern);
    }

    #[test]
    fn mode_recommendation_not_pattern_for_unbalanced_function_like_paren_query() {
        let (mode, _) = recommend_search_mode("project_files(\"/projects/{}/files\" tests");
        assert_ne!(mode, SearchMode::Pattern);
        assert_eq!(mode, SearchMode::Hybrid);
    }

    #[test]
    fn mode_recommendation_refactor_for_identifier() {
        let (mode, _) = recommend_search_mode("handleRequest");
        assert_eq!(mode, SearchMode::Refactor);
    }

    #[test]
    fn mode_recommendation_semantic_for_question() {
        let (mode, _) = recommend_search_mode("how does auth middleware work?");
        assert_eq!(mode, SearchMode::Semantic);
    }

    #[test]
    fn mode_recommendation_exhaustive_for_all_matches_query() {
        let (mode, _) = recommend_search_mode("find all occurrences of TODO");
        assert_eq!(mode, SearchMode::Exhaustive);
    }

    #[test]
    fn mode_recommendation_team_for_cross_project_query() {
        let (mode, _) = recommend_search_mode("search across projects for billing");
        assert_eq!(mode, SearchMode::Team);
    }

    #[test]
    fn mode_recommendation_not_team_for_team_word_in_title() {
        let (mode, _) = recommend_search_mode("PR8 Full Team Context Fix Plan");
        assert_ne!(mode, SearchMode::Team);
    }

    #[test]
    fn count_query_suggests_count_output() {
        let output = suggest_output_format("how many auth handlers", SearchMode::Hybrid);
        assert_eq!(output, Some("count"));
    }

    #[test]
    fn refactor_identifier_suggests_paths_output() {
        let output = suggest_output_format("UserService", SearchMode::Refactor);
        assert_eq!(output, Some("paths"));
    }

    #[test]
    fn search_call_uses_editor_specific_tool_name() {
        let call = search_call(&EditorFormat::Claude, SearchMode::Hybrid, "auth");
        assert!(call.starts_with("mcp__contextstream__search("));

        let call = search_call(&EditorFormat::Cline, SearchMode::Hybrid, "auth");
        assert!(call.starts_with("search("));
    }

    #[test]
    fn search_call_uses_auto_for_empty_broad_query() {
        let call = search_call(&EditorFormat::Windsurf, SearchMode::Hybrid, "");
        assert!(call.contains("mode=\"auto\""));
        assert!(call.contains("query=\"...\""));

        let targeted = search_call(&EditorFormat::Windsurf, SearchMode::Hybrid, "auth");
        assert!(targeted.contains("mode=\"hybrid\""));
    }

    #[test]
    fn init_tool_name_uses_editor_specific_prefix() {
        assert_eq!(
            init_tool_name(&EditorFormat::Claude),
            "mcp__contextstream__init"
        );
        assert_eq!(init_tool_name(&EditorFormat::Cursor), "init");
    }

    #[test]
    fn extract_tool_fields_from_windsurf_tool_info_shape() {
        let input = serde_json::json!({
            "hook_event_name": "pre_mcp_tool_use",
            "tool_info": {
                "mcp_server_name": "contextstream",
                "mcp_tool_name": "workspace",
                "mcp_tool_arguments": {
                    "action": "list"
                }
            }
        });

        assert_eq!(extract_tool_name(&input), "workspace");
        assert_eq!(extract_mcp_server_name(&input), "contextstream");
        assert_eq!(
            first_str(&extract_tool_input(&input), &["action"]),
            Some("list")
        );
    }

    #[test]
    fn mcp_tool_detection_requires_server_metadata_or_prefix() {
        let bash = serde_json::json!({
            "tool_name": "Bash"
        });
        assert!(!is_mcp_tool_call(&bash, "", "Bash"));

        let prefixed = serde_json::json!({
            "tool_name": "mcp__contextstream__search"
        });
        assert!(is_mcp_tool_call(
            &prefixed,
            "",
            "mcp__contextstream__search"
        ));

        let foreign_mcp = serde_json::json!({
            "tool_name": "fetch_pr",
            "mcp_server_name": "github"
        });
        assert!(is_mcp_tool_call(&foreign_mcp, "github", "fetch_pr"));
    }

    #[test]
    fn windsurf_mcp_tool_detection_uses_tool_info_shape() {
        let input = serde_json::json!({
            "hook_event_name": "pre_mcp_tool_use",
            "tool_info": {
                "mcp_tool_name": "search"
            }
        });

        assert!(is_mcp_tool_call(&input, "", "search"));
    }

    #[test]
    fn windsurf_contextstream_search_without_server_name_detected_for_hybrid() {
        let input = serde_json::json!({
            "hook_event_name": "pre_mcp_tool_use",
            "tool_info": {
                "mcp_tool_name": "search",
                "mcp_tool_arguments": {
                    "query": "auth",
                    "mode": "hybrid"
                }
            }
        });
        let tool = extract_tool_name(&input);
        let tool_input = extract_tool_input(&input);

        assert_eq!(tool, "search");
        assert!(is_contextstream_tool_call(
            &EditorFormat::Windsurf,
            &input,
            "",
            &tool,
            &tool_input
        ));
        assert!(!is_grounding_target_tool(true, "search", &tool_input));
    }

    #[test]
    fn windsurf_contextstream_search_without_server_name_detected_for_auto() {
        let input = serde_json::json!({
            "hook_event_name": "pre_mcp_tool_use",
            "tool_info": {
                "mcp_tool_name": "search",
                "mcp_tool_arguments": {
                    "query": "auth",
                    "mode": "auto"
                }
            }
        });
        let tool = extract_tool_name(&input);
        let tool_input = extract_tool_input(&input);

        assert!(is_contextstream_tool_call(
            &EditorFormat::Windsurf,
            &input,
            "",
            &tool,
            &tool_input
        ));
    }

    #[test]
    fn windsurf_explicit_contextstream_server_still_detected() {
        let input = serde_json::json!({
            "hook_event_name": "pre_mcp_tool_use",
            "tool_info": {
                "mcp_server_name": "contextstream",
                "mcp_tool_name": "search",
                "mcp_tool_arguments": {
                    "query": "auth"
                }
            }
        });
        let tool = extract_tool_name(&input);
        let tool_input = extract_tool_input(&input);
        let server_name = extract_mcp_server_name(&input);

        assert!(is_contextstream_tool_call(
            &EditorFormat::Windsurf,
            &input,
            &server_name,
            &tool,
            &tool_input
        ));
    }

    #[test]
    fn windsurf_core_contextstream_tool_without_server_name_is_detected() {
        let input = serde_json::json!({
            "hook_event_name": "pre_mcp_tool_use",
            "tool_info": {
                "mcp_tool_name": "context",
                "mcp_tool_arguments": {
                    "user_message": "hello"
                }
            }
        });
        let tool = extract_tool_name(&input);
        let tool_input = extract_tool_input(&input);

        assert!(is_contextstream_tool_call(
            &EditorFormat::Windsurf,
            &input,
            "",
            &tool,
            &tool_input
        ));
    }

    #[test]
    fn windsurf_plain_foreign_search_is_not_contextstream() {
        let input = serde_json::json!({
            "hook_event_name": "pre_mcp_tool_use",
            "tool_info": {
                "mcp_tool_name": "search",
                "mcp_tool_arguments": {
                    "query": "auth"
                }
            }
        });
        let tool = extract_tool_name(&input);
        let tool_input = extract_tool_input(&input);

        assert!(!is_contextstream_tool_call(
            &EditorFormat::Windsurf,
            &input,
            "",
            &tool,
            &tool_input
        ));
        assert!(is_mcp_tool_call(&input, "", &tool));
        assert!(is_search_first_applicable("search", &tool_input));
    }

    #[test]
    fn hard_first_call_enforcement_supported_for_tier_a_editor_formats() {
        assert!(supports_hard_first_call_enforcement(&EditorFormat::Claude));
        assert!(supports_hard_first_call_enforcement(&EditorFormat::Cursor));
        assert!(supports_hard_first_call_enforcement(
            &EditorFormat::Windsurf
        ));
        assert!(supports_hard_first_call_enforcement(&EditorFormat::Cline));
    }

    #[test]
    fn visibility_sensitive_includes_background_memory_writes() {
        assert!(is_visibility_sensitive_contextstream_call(
            "memory",
            "create_task"
        ));
        assert!(!is_visibility_sensitive_contextstream_call(
            "memory",
            "list_docs"
        ));
    }

    #[test]
    fn visibility_follow_up_uses_editor_specific_prefixes() {
        let claude = visibility_follow_up_call(&EditorFormat::Claude, "project", "index")
            .expect("project index follow-up");
        assert_eq!(
            claude,
            "mcp__contextstream__project(action=\"index_status\")"
        );

        let cursor = visibility_follow_up_call(&EditorFormat::Cursor, "project", "index")
            .expect("project index follow-up");
        assert_eq!(cursor, "project(action=\"index_status\")");
    }

    #[test]
    fn discovery_glob_blocks_broad_recursive_patterns() {
        assert!(is_discovery_glob("**/*"));
        assert!(is_discovery_glob("**/"));
        assert!(is_discovery_glob("**/*.ts"));
        assert!(is_discovery_glob("**/foo"));
        assert!(is_discovery_glob("src/**"));
    }

    #[test]
    fn discovery_glob_allows_scoped_recursive_patterns() {
        assert!(
            !is_discovery_glob("web/src/**/*sidebar*"),
            "dir prefix + filename filter = targeted, not broad"
        );
        assert!(
            !is_discovery_glob("web/src/**/*Sidebar*"),
            "case variant should also be allowed"
        );
        assert!(
            !is_discovery_glob("app/components/**/*Button*.tsx"),
            "component search with extension"
        );
        assert!(
            !is_discovery_glob("src/**/*.rs"),
            "directory-scoped extension search"
        );
        assert!(
            !is_discovery_glob("crates/mcp-server/**/*hook*"),
            "deep directory with filename pattern"
        );
    }

    #[test]
    fn discovery_glob_blocks_unscoped_recursive_patterns() {
        assert!(
            is_discovery_glob("src/**"),
            "no filename filter after ** = broad"
        );
        assert!(
            is_discovery_glob("**/*sidebar*"),
            "no directory prefix before ** = broad"
        );
    }

    #[test]
    fn scoped_recursive_glob_requires_both_prefix_and_filter() {
        assert!(is_scoped_recursive_glob("web/src/**/*sidebar*"));
        assert!(is_scoped_recursive_glob("src/**/*.rs"));
        assert!(!is_scoped_recursive_glob("**/*sidebar*"));
        assert!(!is_scoped_recursive_glob("src/**"));
        assert!(!is_scoped_recursive_glob("src/**/*"));
        assert!(!is_scoped_recursive_glob("src/**/*.*"));
    }

    #[test]
    fn discovery_glob_allows_simple_non_recursive_patterns() {
        assert!(!is_discovery_glob("src/models/*.rs"));
        assert!(!is_discovery_glob("*.rs"));
        assert!(!is_discovery_glob("package.json"));
    }

    #[test]
    fn search_first_applicability_is_scoped_to_discovery_tools() {
        assert!(is_search_first_applicable("glob", &serde_json::json!({})));
        assert!(is_search_first_applicable(
            "read_file",
            &serde_json::json!({})
        ));
        assert!(!is_search_first_applicable(
            "write_to_file",
            &serde_json::json!({})
        ));
        assert!(!is_search_first_applicable(
            "memory",
            &serde_json::json!({})
        ));
    }

    #[test]
    fn search_first_applicability_for_task_is_explore_only() {
        assert!(is_search_first_applicable(
            "task",
            &serde_json::json!({ "subagent_type": "explore" })
        ));
        assert!(!is_search_first_applicable(
            "task",
            &serde_json::json!({ "subagent_type": "plan" })
        ));
    }

    #[test]
    fn index_status_prefers_most_specific_match() {
        let stale = (chrono::Utc::now() - chrono::Duration::days(30)).to_rfc3339();
        let fresh = (chrono::Utc::now() - chrono::Duration::hours(2)).to_rfc3339();
        let data = serde_json::json!({
            "projects": {
                "/home/alice/projects": { "indexed_at": stale },
                "/home/alice/projects/example-app": { "indexed_at": fresh }
            }
        });

        let status = index_status_from_data("/home/alice/projects/example-app/src", &data);
        assert_eq!(
            status,
            IndexStatus {
                is_indexed: true,
                is_stale: false
            }
        );
    }

    #[test]
    fn index_status_marks_project_stale_when_only_match_is_stale() {
        let stale = (chrono::Utc::now() - chrono::Duration::days(30)).to_rfc3339();
        let data = serde_json::json!({
            "projects": {
                "/home/alice/projects": { "indexed_at": stale }
            }
        });

        let status = index_status_from_data("/home/alice/projects/example-app/src", &data);
        assert_eq!(
            status,
            IndexStatus {
                is_indexed: true,
                is_stale: true
            }
        );
    }

    #[test]
    fn index_wait_only_blocks_missing_index_coverage() {
        assert!(should_wait_for_initial_index(IndexStatus {
            is_indexed: false,
            is_stale: false,
        }));
        assert!(!should_wait_for_initial_index(IndexStatus {
            is_indexed: true,
            is_stale: false,
        }));
        assert!(!should_wait_for_initial_index(IndexStatus {
            is_indexed: true,
            is_stale: true,
        }));
    }

    #[test]
    fn stale_index_redirect_warns_not_denies() {
        let status = IndexStatus {
            is_indexed: true,
            is_stale: true,
        };
        let search = search_call(&EditorFormat::Claude, SearchMode::Hybrid, "");
        let decision = search_first_redirect_decision(
            status,
            "block".to_string(),
            stale_index_local_fallback_message(&EditorFormat::Claude, &search, "Glob"),
        );

        // Warn, don't deny: the redirect must be a non-blocking nudge so one
        // redirected call never cancels a whole parallel batch. The steer
        // toward ContextStream search is still present in the message.
        match decision {
            HookDecision::AllowWithContext(message) => {
                assert!(message.contains("Project index exists and remains usable"));
                assert!(message.contains("Use mcp__contextstream__search"));
                assert!(message.contains("before broad Glob"));
            }
            HookDecision::BlockWithMessage(_) | HookDecision::Allow => {
                panic!("search-first redirects must warn (AllowWithContext), never deny")
            }
        }
    }

    #[test]
    fn fresh_index_redirect_warns_not_denies() {
        let status = IndexStatus {
            is_indexed: true,
            is_stale: false,
        };
        let decision = search_first_redirect_decision(
            status,
            "use search first".to_string(),
            "fallback allowed".to_string(),
        );

        match decision {
            HookDecision::AllowWithContext(message) => assert_eq!(message, "use search first"),
            HookDecision::BlockWithMessage(_) | HookDecision::Allow => {
                panic!("search-first redirects must warn (AllowWithContext), never deny")
            }
        }
    }

    #[test]
    fn todo_persistence_nudge_is_scoped_to_plan_like_content() {
        let generic = serde_json::json!({
            "todos": [{ "content": "Fix failing test" }]
        });
        assert!(!should_nudge_todo_persistence(&generic, false));

        let plan_like = serde_json::json!({
            "todos": [{ "content": "Phase 1 rollout checklist" }]
        });
        assert!(should_nudge_todo_persistence(&plan_like, false));
    }

    #[test]
    fn todo_persistence_nudge_always_on_for_plan_subagent() {
        let generic = serde_json::json!({
            "todos": [{ "content": "Fix failing test" }]
        });
        assert!(should_nudge_todo_persistence(&generic, true));
    }

    #[test]
    fn first_non_empty_str_trims_and_filters_blank_values() {
        let input = serde_json::json!({
            "workspace_id": "   ",
            "project_id": "  abc-123  "
        });
        assert_eq!(first_non_empty_str(&input, &["workspace_id"]), None);
        assert_eq!(
            first_non_empty_str(&input, &["project_id"]),
            Some("abc-123")
        );
    }

    #[test]
    fn scope_sensitive_tools_are_detected() {
        assert!(is_scope_sensitive_contextstream_tool("context"));
        assert!(is_scope_sensitive_contextstream_tool("search"));
        assert!(is_scope_sensitive_contextstream_tool("memory"));
        assert!(!is_scope_sensitive_contextstream_tool("workspace"));
        assert!(!is_scope_sensitive_contextstream_tool("help"));
    }

    #[test]
    fn index_wait_blocks_broad_local_discovery_tools() {
        assert!(is_local_discovery_tool_during_index_wait(
            "glob",
            &serde_json::json!({ "pattern": "**/*" })
        ));
        assert!(is_local_discovery_tool_during_index_wait(
            "read_file",
            &serde_json::json!({ "path": "." })
        ));
        assert!(is_local_discovery_tool_during_index_wait(
            "task",
            &serde_json::json!({ "subagent_type": "explore" })
        ));
    }

    #[test]
    fn index_wait_never_blocks_grep() {
        assert!(!is_local_discovery_tool_during_index_wait(
            "grep",
            &serde_json::json!({ "pattern": "foo", "path": "." })
        ));
        assert!(!is_local_discovery_tool_during_index_wait(
            "grep",
            &serde_json::json!({ "pattern": "foo", "path": "src/main.rs" })
        ));
    }

    #[test]
    fn index_wait_allows_targeted_local_reads() {
        assert!(!is_local_discovery_tool_during_index_wait(
            "read_file",
            &serde_json::json!({ "path": "src/main.rs" })
        ));
        assert!(!is_local_discovery_tool_during_index_wait(
            "task",
            &serde_json::json!({ "subagent_type": "plan" })
        ));
    }

    #[test]
    fn tool_input_remember_language_detected() {
        assert!(tool_input_contains_remember_language(
            &serde_json::json!({ "content": "remember this for later" })
        ));
        assert!(tool_input_contains_remember_language(
            &serde_json::json!({ "content": "don't forget we use postgres" })
        ));
        assert!(tool_input_contains_remember_language(
            &serde_json::json!({ "content": "keep in mind the API is v2" })
        ));
        assert!(tool_input_contains_remember_language(
            &serde_json::json!({ "content": "save for later" })
        ));
        assert!(tool_input_contains_remember_language(
            &serde_json::json!({ "todos": [{ "content": "remember to use tabs" }] })
        ));
    }

    #[test]
    fn tool_input_no_remember_language() {
        assert!(!tool_input_contains_remember_language(
            &serde_json::json!({ "content": "implement the login flow" })
        ));
        assert!(!tool_input_contains_remember_language(
            &serde_json::json!({ "content": "fix the bug in auth module" })
        ));
        assert!(!tool_input_contains_remember_language(
            &serde_json::json!({ "todos": [{ "content": "add unit tests" }] })
        ));
    }

    #[test]
    fn init_gate_only_clears_on_init_or_context() {
        for tool in &["init", "context"] {
            assert!(
                clears_init_gate(true, tool),
                "{} should satisfy the startup gate",
                tool
            );
        }

        for tool in &[
            "skill",
            "memory",
            "session",
            "search",
            "project",
            "workspace",
        ] {
            assert!(
                !clears_init_gate(true, tool),
                "{} must not satisfy the startup gate",
                tool
            );
        }

        assert!(
            !clears_init_gate(false, "init"),
            "foreign init-like tools must not satisfy the ContextStream gate"
        );
    }

    #[test]
    fn init_gate_blocks_non_cs_mcp_tools() {
        let is_contextstream_call = false;
        let is_mcp_tool = true;

        let should_hard_block = !is_contextstream_call && is_mcp_tool;
        assert!(
            should_hard_block,
            "Non-CS MCP tools must still be hard-blocked when init is required"
        );
    }

    #[test]
    fn init_gate_skips_local_tools() {
        let is_contextstream_call = false;
        let is_mcp_tool = false;

        let should_block_or_nudge = is_contextstream_call || is_mcp_tool;
        assert!(
            !should_block_or_nudge,
            "Local/builtin tools are not affected by init gate"
        );
    }

    #[test]
    fn context_gate_only_clears_on_context() {
        assert!(
            clears_context_gate(true, "context"),
            "context() should satisfy the per-prompt gate"
        );

        for tool in &[
            "init",
            "skill",
            "memory",
            "session",
            "search",
            "project",
            "workspace",
        ] {
            assert!(
                !clears_context_gate(true, tool),
                "{} must not satisfy the per-prompt context gate",
                tool
            );
        }

        assert!(
            !clears_context_gate(false, "context"),
            "foreign context-like tools must not satisfy the ContextStream gate"
        );
    }

    #[test]
    fn context_gate_allows_init_without_clearing() {
        assert!(
            allowed_before_context_without_clearing(true, "init"),
            "init() is allowed during context gate without clearing it"
        );
        assert!(
            !allowed_before_context_without_clearing(true, "search"),
            "search() must not bypass the context gate"
        );
        assert!(
            !allowed_before_context_without_clearing(false, "init"),
            "foreign init-like tools must not bypass the context gate"
        );
    }

    #[test]
    fn context_gate_blocks_non_cs_mcp_tools() {
        let is_contextstream_call = false;
        let is_mcp_tool = true;

        let should_hard_block = !is_contextstream_call && is_mcp_tool;
        assert!(
            should_hard_block,
            "Non-CS MCP tools must still be hard-blocked when context is required"
        );
    }

    #[test]
    fn context_gate_skips_local_tools() {
        let is_contextstream_call = false;
        let is_mcp_tool = false;

        let should_block_or_nudge = is_contextstream_call || is_mcp_tool;
        assert!(
            !should_block_or_nudge,
            "Local/builtin tools pass through context gate unaffected"
        );
    }

    #[test]
    fn strict_turn_flow_blocks_contextstream_tools_until_context_runs() {
        let context_required = true;
        let init_required = false;

        assert!(!init_required);
        assert!(context_required);
        assert!(!clears_context_gate(true, "search"));
        assert!(!allowed_before_context_without_clearing(true, "search"));

        let would_block_search = context_required
            && !clears_context_gate(true, "search")
            && !allowed_before_context_without_clearing(true, "search");
        assert!(
            would_block_search,
            "search must wait until the agent calls context() for this prompt"
        );

        assert!(
            clears_context_gate(true, "context"),
            "context clears the gate so subsequent tools can run"
        );
    }

    #[test]
    fn compliance_event_severity_matches_soft_vs_hard_policy() {
        let init_satisfied_severity = 1u8;
        let hard_init_severity = 5u8;
        let context_satisfied_severity = 1u8;
        let hard_context_severity = 4u8;

        assert!(
            init_satisfied_severity < hard_init_severity,
            "Satisfied init gate severity must be lower than hard block"
        );
        assert!(
            context_satisfied_severity < hard_context_severity,
            "Satisfied context gate severity must be lower than hard block"
        );
    }

    #[test]
    fn grounding_target_tools_exclude_contextstream_search() {
        assert!(is_grounding_target_tool(
            false,
            "read_file",
            &serde_json::json!({ "path": "src/lib.rs" })
        ));
        assert!(is_grounding_target_tool(
            false,
            "task",
            &serde_json::json!({ "subagent_type": "explore" })
        ));
        assert!(!is_grounding_target_tool(
            true,
            "search",
            &serde_json::json!({ "mode": "auto", "query": "foo" })
        ));
        assert!(!is_grounding_target_tool(
            false,
            "glob",
            &serde_json::json!({ "pattern": "**/*" })
        ));
    }

    #[test]
    fn maybe_nudge_unread_grounding_only_upgrades_allow() {
        let session = "mcp__contextstream__session";
        let blocked = HookDecision::BlockWithMessage("use search".into());
        let out = maybe_nudge_unread_grounding(
            blocked,
            Some(mcp_session::grounding_state::GroundingSummary::from_hit_count(3)),
            session,
        );
        assert!(matches!(out, HookDecision::BlockWithMessage(_)));

        let with_ctx = HookDecision::AllowWithContext("other".into());
        let out = maybe_nudge_unread_grounding(
            with_ctx,
            Some(mcp_session::grounding_state::GroundingSummary::from_hit_count(3)),
            session,
        );
        assert!(matches!(out, HookDecision::AllowWithContext(msg) if msg == "other"));

        let allow = HookDecision::Allow;
        let out = maybe_nudge_unread_grounding(
            allow,
            Some(mcp_session::grounding_state::GroundingSummary::from_hit_count(2)),
            session,
        );
        assert!(
            matches!(out, HookDecision::AllowWithContext(msg) if msg.contains("[GROUNDING_AVAILABLE]") && msg.contains(session))
        );
    }

    #[test]
    fn maybe_nudge_unread_grounding_includes_freshness_summary() {
        let session = "mcp__contextstream__session";
        let out = maybe_nudge_unread_grounding(
            HookDecision::Allow,
            Some(mcp_session::grounding_state::GroundingSummary {
                hit_count: 4,
                decision_count: 2,
                stale_count: 1,
                newest_source_at: Some("2026-05-24T00:00:00Z".to_string()),
                oldest_source_at: Some("2026-04-01T00:00:00Z".to_string()),
                top_kinds: vec!["decision".to_string(), "transcript".to_string()],
            }),
            session,
        );
        assert!(matches!(
            out,
            HookDecision::AllowWithContext(msg)
                if msg.contains("2 decision")
                    && msg.contains("1 stale/time-sensitive")
                    && msg.contains("decision, transcript")
                    && msg.contains("freshness refresh")
        ));
    }

    #[test]
    fn grounding_state_decays_after_three_target_tool_calls() {
        // Mutates the process-global CONTEXTSTREAM_GROUNDING_STATE_FILE; serialize
        // with all other env-touching tests.
        let _env_guard = crate::env_test_mutex()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("grounding-state.json");
        std::env::set_var(
            "CONTEXTSTREAM_GROUNDING_STATE_FILE",
            state_path.to_str().unwrap(),
        );
        let cwd = "/tmp/cs-grounding-decay-test";
        mcp_session::grounding_state::mark_grounding_emitted(cwd, 2);
        assert_eq!(mcp_session::grounding_state::peek_unread_hits(cwd), Some(2));
        mcp_session::grounding_state::record_grounding_target_tool(cwd);
        mcp_session::grounding_state::record_grounding_target_tool(cwd);
        assert_eq!(mcp_session::grounding_state::peek_unread_hits(cwd), Some(2));
        mcp_session::grounding_state::record_grounding_target_tool(cwd);
        assert_eq!(mcp_session::grounding_state::peek_unread_hits(cwd), None);
        std::env::remove_var("CONTEXTSTREAM_GROUNDING_STATE_FILE");
    }

    #[test]
    fn cursor_pre_tool_output_uses_permission_schema() {
        // Deny must use Cursor's `permission`/`agent_message` schema, not the
        // stale `decision`/`reason` shape that current Cursor ignores.
        let deny =
            cursor_pre_tool_output(&HookDecision::BlockWithMessage("call context first".into()));
        assert_eq!(deny["permission"], "deny");
        assert_eq!(deny["agent_message"], "call context first");
        assert_eq!(deny["user_message"], "call context first");
        assert!(deny.get("decision").is_none());
        assert!(deny.get("reason").is_none());

        // Allow (and allow-with-context) is a plain permission grant — Cursor's
        // preToolUse allow cannot inject context.
        let allow = cursor_pre_tool_output(&HookDecision::Allow);
        assert_eq!(allow["permission"], "allow");
        let allow_ctx = cursor_pre_tool_output(&HookDecision::AllowWithContext("fyi".into()));
        assert_eq!(allow_ctx["permission"], "allow");
        assert!(allow_ctx.get("agent_message").is_none());
    }
}
