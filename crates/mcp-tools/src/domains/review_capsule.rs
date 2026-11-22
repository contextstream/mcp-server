//! Local git diff packaging, client-side secret scrubbing, and pre-upload
//! validation for Review Capsules (`0a290693`).
//!
//! Mirrors the verified backend contract (`contextstream-review-capsule` +
//! `contextstream-security::sensitive_filter`) so secrets, sensitive files,
//! and `.contextignore` entries are scrubbed or skipped locally before any
//! payload leaves the caller's machine.

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use mcp_client::ContextStreamClient;
use mcp_types::{
    api::{
        CreateReviewCapsuleRequest, ReviewExcerptInput, ReviewFileInput, ReviewFileStatus,
        ReviewHunkInput, ReviewInput, ReviewSkipReason, REVIEW_CAPSULE_CONTRACT_VERSION,
    },
    Error, Result,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::LazyLock as Lazy;
use tokio::process::Command;
use uuid::Uuid;

pub const REDACTED: &str = "[REDACTED]";

// Backend `Limits::default()` parity (`contextstream-review-capsule/src/model.rs`).
pub const MAX_FILES: usize = 400;
pub const MAX_HUNKS: usize = 4_000;
pub const MAX_LINES: usize = 120_000;
pub const MAX_PART_CHARS_MIN: usize = 4_000;
pub const MAX_PART_CHARS_MAX: usize = 200_000;
pub const MAX_TASK_CHARS: usize = 20_000;
pub const MAX_TITLE_CHARS: usize = 300;
pub const MAX_REPO_CHARS: usize = 200;
pub const MAX_ITEMS: usize = 40;
pub const MAX_ITEM_CHARS: usize = 2_000;
pub const MAX_PATH_CHARS: usize = 1_024;
pub const MAX_HEADING_CHARS: usize = 200;
pub const MAX_EXCERPTS_PER_FILE: usize = 16;
pub const MAX_EXCERPT_LINES: usize = 120;
pub const MAX_FILE_DIFF_LINES: usize = 20_000;
pub const MAX_FILE_DIFF_BYTES: usize = 250_000;
pub const MAX_FILE_HUNKS: usize = 1_000;
/// Leave safety headroom under the backend's 6 MiB body cap.
pub const MAX_UPLOAD_BODY_BYTES: usize = 5_800_000;

static COMMIT_ID_HEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^(?:[0-9a-fA-F]{40}|[0-9a-fA-F]{64})$").expect("valid regex"));
static EXCERPT_NAME_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_.:]{0,99}$").expect("valid regex"));
static ANSWER_FORM_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?im)^[\s>*#-]*(?:VERDICT\s*:\s*\**\s*(?:SAFE TO LAND|NOT SAFE)|CLAIM\s+C\d+\**\s*:\s*\**\s*(?:HOLDS|BROKEN|CANNOT JUDGE))",
    )
    .expect("valid regex")
});

/// Caller-supplied `review` payload on `capsule(action="create", review={...})`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReviewToolInput {
    pub base: String,
    pub head: Option<String>,
    pub two_dot: Option<bool>,
    pub title: Option<String>,
    pub task: String,
    pub claims: Option<Vec<String>>,
    pub evidence: Option<Vec<String>>,
    pub context_lines: Option<u32>,
    pub part_chars: Option<usize>,
    pub exclude: Option<Vec<String>>,
    pub excerpts: Option<Vec<ReviewExcerptToolInput>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ReviewExcerptToolInput {
    pub path: String,
    pub first_line: u32,
    pub last_line: Option<u32>,
    pub lines: Option<Vec<String>>,
    pub name: Option<String>,
}

pub fn sha256_hex(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hex::encode(hasher.finalize())
}

// ============================================================================
// Sensitive File Detection (Union of backend sensitive_filter + mcp-client)
// ============================================================================

const BACKEND_BLOCKED_BASENAMES: &[&str] = &[
    ".env",
    ".env.local",
    ".env.development",
    ".env.production",
    ".env.staging",
    ".env.test",
    ".env.example",
    ".env.sample",
    ".npmrc",
    ".pypirc",
    ".netrc",
    "credentials",
    "credentials.json",
    "credentials.yaml",
    "credentials.yml",
    "service-account.json",
    "service_account.json",
    "serviceaccountkey.json",
    "gcloud-service-key.json",
];

const BACKEND_BLOCKED_EXTENSIONS: &[&str] = &["pem", "key", "p12", "pfx", "jks", "keystore", "pub"];

const BACKEND_BLOCKED_PATH_SEGMENTS: &[&str] = &[
    ".ssh/",
    ".gnupg/",
    ".aws/",
    ".gcp/",
    ".azure/",
    ".config/gcloud/",
    ".docker/config.json",
    ".kube/config",
];

/// Union of backend `sensitive_filter::is_sensitive_file` and
/// `ContextStreamClient::is_secret_file_name`. Deliberately does NOT exempt
/// `.env.example` / `.env.sample` because backend `render.rs:380` blocks them.
pub fn is_sensitive_review_file(path: &str) -> bool {
    let normalized = path.replace('\\', "/");
    let lower = normalized.to_ascii_lowercase();
    let basename = lower.rsplit('/').next().unwrap_or(&lower);

    if BACKEND_BLOCKED_BASENAMES.contains(&basename) {
        return true;
    }
    if basename.starts_with(".env.") || basename.ends_with(".env") || basename.contains(".env.") {
        return true;
    }
    if let Some((_, ext)) = basename.rsplit_once('.') {
        if BACKEND_BLOCKED_EXTENSIONS.contains(&ext) {
            return true;
        }
    }
    if BACKEND_BLOCKED_PATH_SEGMENTS
        .iter()
        .any(|seg| lower.contains(seg))
    {
        return true;
    }
    ContextStreamClient::is_secret_file_name(basename)
}

// ============================================================================
// Client-Side Content Scrubbing (Parity with scrub.rs + sensitive_filter.rs)
// ============================================================================

static PEM_BEGIN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"-----BEGIN[A-Z ]*PRIVATE KEY").expect("valid regex"));
static PEM_END: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"-----END[A-Z ]*PRIVATE KEY").expect("valid regex"));
static KEY_MATERIAL: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"^(?:[\s"'+,]|\\n)*([A-Za-z0-9+/]{40,})={0,2}(?:[\s"'+,;]|\\n)*$"#)
        .expect("valid regex")
});
static BASE64_ONLY: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"^(?:[\s"'+,]|\\n)*([A-Za-z0-9+/]+)={0,2}(?:[\s"'+,;]|\\n)*$"#)
        .expect("valid regex")
});
static CREDENTIAL_WORD: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)secret|token|passw|pwd|api[_-]?key|credential|bearer|private[_-]?key|auth")
        .expect("valid regex")
});
static QUOTED_RUN: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"(["'`])([A-Za-z0-9+/=_.\-]{24,})(["'`])"#).expect("valid regex"));

static MULTILINE_PEM: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"-----BEGIN[A-Z \n]*PRIVATE KEY-----[\s\S]*?-----END[A-Z \n]*PRIVATE KEY-----")
        .expect("valid regex")
});
static MULTILINE_AWS_KEY: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?:^|[^A-Za-z0-9])((?:AKIA|ABIA|ACCA|ASIA)[A-Z0-9]{16})(?:[^A-Za-z0-9]|$)")
        .expect("valid regex")
});
static MULTILINE_AWS_SECRET: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r#"(?i)(?:aws_secret_access_key|aws_secret)\s*[=:]\s*['"]?([A-Za-z0-9/+=]{40})['"]?"#,
    )
    .expect("valid regex")
});
static MULTILINE_BEARER: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)Bearer\s+([A-Za-z0-9\-._~+/]+=*)").expect("valid regex"));
static MULTILINE_BASIC: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)Basic\s+([A-Za-z0-9+/]{16,}={0,3})").expect("valid regex"));
static MULTILINE_DATA_URL: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"data:[^;]{1,50};base64,([A-Za-z0-9+/=]{100,})").expect("valid regex")
});

static SECRET_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    vec![
        Regex::new(r"-----BEGIN[A-Z \n]*PRIVATE KEY-----[\s\S]*?-----END[A-Z \n]*PRIVATE KEY-----")
            .expect("valid regex"),
        Regex::new(r"(?:^|[^A-Za-z0-9])((?:AKIA|ABIA|ACCA|ASIA)[A-Z0-9]{16})(?:[^A-Za-z0-9]|$)")
            .expect("valid regex"),
        Regex::new(
            r#"(?i)(?:aws_secret_access_key|aws_secret)\s*[=:]\s*['"]?([A-Za-z0-9/+=]{40})['"]?"#,
        )
        .expect("valid regex"),
        Regex::new(r#"(?i)(?:postgres|mysql|mongodb|redis|amqp|mssql)://[^\s'"<>]+"#)
            .expect("valid regex"),
        Regex::new(r"(?i)Bearer\s+[A-Za-z0-9\-._~+/]+=*").expect("valid regex"),
        Regex::new(r"(?i)Basic\s+[A-Za-z0-9+/]{16,}={0,3}").expect("valid regex"),
        Regex::new(r"eyJ[A-Za-z0-9_-]{10,}\.eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]+")
            .expect("valid regex"),
        Regex::new(r"data:[^;]{1,50};base64,[A-Za-z0-9+/=]{100,}").expect("valid regex"),
    ]
});

static KNOWN_PREFIX_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    let prefixes = [
        "AKIA",
        "ABIA",
        "ACCA",
        "ASIA",
        "sk_live_",
        "sk_test_",
        "pk_live_",
        "pk_test_",
        "rk_live_",
        "rk_test_",
        "xoxb-",
        "xoxp-",
        "xoxs-",
        "xoxa-",
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "glpat-",
        "npm_",
        "pypi-",
        "SK",
        "SG.",
        "sbp_",
        "vercel_",
        "sk-",
        "sk-ant-",
        "hf_",
        "cf_",
        "heroku_",
        // ContextStream API keys & setup tokens
        "cbiq_",
        "cbst_",
    ];
    let mut out: Vec<Regex> = prefixes
        .iter()
        .map(|prefix| {
            let min_total = if prefix.len() <= 3 {
                20
            } else {
                prefix.len() + 8
            };
            let pat = format!(
                r"(^|[^A-Za-z0-9_])({}[A-Za-z0-9\-._~+/=]{{{},}})",
                regex::escape(prefix),
                min_total.saturating_sub(prefix.len())
            );
            Regex::new(&pat).expect("valid known-prefix regex")
        })
        .collect();
    // ContextStream share tokens / review credentials (`capsule_<32hex>` or `capsule_<uuid>`),
    // never matching code identifiers like `capsule_id` or `capsule_routed`.
    out.push(
        Regex::new(
            r"(^|[^A-Za-z0-9_])(capsule_(?:[0-9a-fA-F]{32}|[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}))(?:$|[^A-Za-z0-9_])",
        )
        .expect("valid capsule token regex"),
    );
    out
});

static SENSITIVE_KEY_NAMES: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(?:secret|token|password|passwd|pwd|api[_-]?key|apikey|auth[_-]?token|access[_-]?key|private[_-]?key|client[_-]?secret|signing[_-]?key|encryption[_-]?key|bearer|credentials?|connection[_-]?string|database[_-]?url|db[_-]?password|smtp[_-]?password|master[_-]?key|service[_-]?key|webhook[_-]?secret)",
    )
    .expect("valid regex")
});

static ASSIGNMENT_PATTERN: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r##"(?m)^[^#\n]*?(\b\w+)\s*[=:]\s*['"]?([^'"=\s#][^\n'"#]{7,}?)['"]?\s*(?:#.*)?$"##)
        .expect("valid regex")
});

static ENV_KV_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[A-Z_][A-Z0-9_]*\s*=").expect("valid regex"));

fn entropy(text: &str) -> f64 {
    if text.is_empty() {
        return 0.0;
    }
    let mut seen = [0u32; 256];
    for byte in text.bytes() {
        seen[byte as usize] += 1;
    }
    let len = text.len() as f64;
    seen.iter()
        .filter(|n| **n > 0)
        .map(|n| {
            let p = f64::from(*n) / len;
            -p * p.log2()
        })
        .sum()
}

fn is_key_material(line: &str) -> bool {
    KEY_MATERIAL.captures(line).is_some_and(|found| {
        let mut seen = [false; 256];
        for byte in found[1].bytes() {
            seen[byte as usize] = true;
        }
        seen.iter().filter(|s| **s).count() >= 10
    })
}

fn is_base64_only(line: &str, min: usize) -> bool {
    BASE64_ONLY
        .captures(line)
        .is_some_and(|found| found[1].len() >= min)
}

fn is_env_like_hunk(path: &str, lines: &[&str]) -> bool {
    let lower_path = path.to_ascii_lowercase();
    let basename = lower_path.rsplit('/').next().unwrap_or(&lower_path);
    if basename.starts_with(".env")
        || basename.ends_with(".env")
        || basename == "local.settings.json"
        || basename == "appsettings.development.json"
    {
        return true;
    }
    let non_empty: Vec<&str> = lines
        .iter()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    if non_empty.is_empty() {
        return false;
    }
    let kv_count = non_empty.iter().filter(|l| ENV_KV_RE.is_match(l)).count();
    kv_count as f64 / non_empty.len() as f64 >= 0.5
}

fn looks_like_code(line: &str) -> bool {
    let t = line.trim();
    if t.starts_with("fn ")
        || t.starts_with("def ")
        || t.starts_with("func ")
        || t.starts_with("function ")
        || t.starts_with("pub fn ")
        || t.starts_with("pub async fn ")
        || t.starts_with("async fn ")
        || t.starts_with("class ")
        || t.starts_with("impl ")
        || t.starts_with("trait ")
        || t.starts_with("interface ")
    {
        return true;
    }
    if (t.contains('(') && t.contains(')')) && (t.contains('{') || t.contains('}')) {
        return true;
    }
    if t.starts_with("use ") || t.starts_with("import ") || t.starts_with("require(") {
        return true;
    }
    false
}

fn looks_like_code_value(value: &str) -> bool {
    if value.contains("::") || value.contains("->") || value.contains("=>") {
        return true;
    }
    if value.contains('(') && value.contains(')') {
        return true;
    }
    value.starts_with("fn ")
        || value.starts_with("new ")
        || value.starts_with('&')
        || value.starts_with("vec!")
        || value.starts_with("Some(")
        || value.starts_with("None")
        || value.starts_with("Ok(")
        || value.starts_with("Err(")
}

fn is_sensitive_assignment_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    let is_reference = ["_ID", "_NAME", "_PATH"]
        .iter()
        .any(|suffix| upper.ends_with(suffix));
    !is_reference && SENSITIVE_KEY_NAMES.is_match(key)
}

fn looks_like_indirect_config_value(value: &str) -> bool {
    (value.contains("${") && value.contains('}'))
        || (value.contains("$(") && value.contains(')'))
        || (value.contains("{{") && value.contains("}}"))
}

fn redact_known_prefixes(content: &str) -> String {
    let mut result = content.to_string();
    for re in KNOWN_PREFIX_PATTERNS.iter() {
        let mut pos = 0;
        while pos < result.len() {
            let Some(caps) = re.captures_at(&result, pos) else {
                break;
            };
            let Some(token) = caps.get(2) else {
                break;
            };
            let start = token.start();
            let end = token.end();
            result.replace_range(start..end, REDACTED);
            pos = start + REDACTED.len();
        }
    }
    result
}

fn redact_assignments(content: &str, aggressive: bool) -> String {
    let mut lines: Vec<String> = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || trimmed.starts_with('#')
            || trimmed.starts_with("//")
            || trimmed.contains(REDACTED)
            || looks_like_code(trimmed)
        {
            lines.push(line.to_string());
            continue;
        }
        if let Some(caps) = ASSIGNMENT_PATTERN.captures(trimmed) {
            let key = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let value = caps.get(2).map(|m| m.as_str()).unwrap_or("");
            if value.len() < 8 || looks_like_code_value(value) {
                lines.push(line.to_string());
                continue;
            }
            let should_redact = is_sensitive_assignment_key(key)
                || (aggressive && !looks_like_indirect_config_value(value) && entropy(value) > 4.5);
            if should_redact {
                lines.push(line.replace(value, REDACTED));
            } else {
                lines.push(line.to_string());
            }
        } else {
            lines.push(line.to_string());
        }
    }
    lines.join("\n")
}

fn scrub_content_local(content: &str, file_path: &str) -> String {
    if content.is_empty() {
        return String::new();
    }
    let lines_slice: Vec<&str> = content.split('\n').collect();
    let is_env_like = is_env_like_hunk(file_path, &lines_slice);
    let mut result = content.to_string();
    for pattern in SECRET_PATTERNS.iter() {
        // AWS key pattern captures group 1 with boundary chars in group 0;
        // replace only group 1 so trailing delimiters remain available for adjacent tokens.
        if pattern.as_str().contains("AKIA|ABIA") {
            let mut pos = 0;
            while pos < result.len() {
                let Some(caps) = pattern.captures_at(&result, pos) else {
                    break;
                };
                let Some(key) = caps.get(1) else {
                    break;
                };
                let start = key.start();
                let end = key.end();
                result.replace_range(start..end, REDACTED);
                pos = start + REDACTED.len();
            }
        } else {
            result = pattern.replace_all(&result, REDACTED).into_owned();
        }
    }
    result = redact_known_prefixes(&result);
    redact_assignments(&result, is_env_like)
}

fn prescrub_multiline_patterns(text: &mut String) {
    let mut pos = 0;
    while let Some(m) = MULTILINE_PEM.find_at(text, pos) {
        if m.as_str().contains('\n') {
            let replacement = m
                .as_str()
                .split('\n')
                .map(|seg| if seg.trim().is_empty() { seg } else { REDACTED })
                .collect::<Vec<_>>()
                .join("\n");
            let end = m.start() + replacement.len();
            text.replace_range(m.start()..m.end(), &replacement);
            pos = end;
        } else {
            pos = m.end();
        }
    }

    let mut pos = 0;
    while let Some(caps) = MULTILINE_AWS_KEY.captures_at(text, pos) {
        let m = caps.get(0).expect("full match");
        let c1 = caps.get(1).expect("aws key capture");
        if m.as_str().contains('\n') {
            let c1_start = c1.start();
            text.replace_range(c1_start..c1.end(), REDACTED);
            pos = c1_start + REDACTED.len();
        } else {
            pos = c1.end();
        }
    }

    for pattern in [
        &*MULTILINE_AWS_SECRET,
        &*MULTILINE_BEARER,
        &*MULTILINE_BASIC,
        &*MULTILINE_DATA_URL,
    ] {
        let mut pos = 0;
        while let Some(caps) = pattern.captures_at(text, pos) {
            let m = caps.get(0).expect("full match");
            if m.as_str().contains('\n') {
                let c1 = caps.get(1).expect("secret capture");
                let c1_start = c1.start();
                text.replace_range(c1_start..c1.end(), REDACTED);
                pos = c1_start + REDACTED.len();
            } else {
                pos = m.end();
            }
        }
    }
}

fn scrub_credential_literals(line: &str) -> String {
    if !CREDENTIAL_WORD.is_match(line) || !QUOTED_RUN.is_match(line) {
        return line.to_string();
    }
    QUOTED_RUN
        .replace_all(line, |found: &regex::Captures<'_>| {
            let run = &found[2];
            let mixed = run.bytes().any(|b| b.is_ascii_lowercase())
                && run.bytes().any(|b| b.is_ascii_uppercase())
                && run.bytes().any(|b| b.is_ascii_digit());
            if mixed && entropy(run) >= 4.2 {
                format!("{}{REDACTED}{}", &found[1], &found[3])
            } else {
                found[0].to_string()
            }
        })
        .into_owned()
}

fn is_direction_control(c: char) -> bool {
    matches!(
        c,
        '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}' | '\u{061C}'
    )
}

pub fn show_direction_controls(text: &str) -> (String, u32) {
    if !text.chars().any(is_direction_control) {
        return (text.to_string(), 0);
    }
    let mut out = String::with_capacity(text.len() + 16);
    let mut count = 0u32;
    for c in text.chars() {
        if is_direction_control(c) {
            out.push_str(&format!("<U+{:04X}>", c as u32));
            count += 1;
        } else {
            out.push(c);
        }
    }
    (out, count)
}

/// Scrub code lines while preserving exact line count.
pub fn scrub_code_lines(path: &str, lines: &[String]) -> Vec<String> {
    let n = lines.len();
    let mut redacted_line = vec![false; n];
    let mut open: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        if PEM_BEGIN.is_match(line) && open.is_none() {
            open = Some(i);
        }
        if PEM_END.is_match(line) {
            let start = match open.take() {
                Some(start) => start,
                None => {
                    let mut start = i;
                    while start > 0 && is_base64_only(&lines[start - 1], 1) {
                        start -= 1;
                    }
                    start
                }
            };
            for slot in redacted_line.iter_mut().take(i + 1).skip(start) {
                *slot = true;
            }
        }
    }
    if let Some(start) = open {
        for slot in redacted_line.iter_mut().skip(start) {
            *slot = true;
        }
    }
    for (i, line) in lines.iter().enumerate() {
        if !redacted_line[i] && is_key_material(line) {
            redacted_line[i] = true;
        }
    }
    for i in 1..n {
        if !redacted_line[i] && redacted_line[i - 1] && is_base64_only(&lines[i], 8) {
            redacted_line[i] = true;
        }
    }

    let prepared: Vec<&str> = lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            if redacted_line[i] {
                REDACTED
            } else {
                line.as_str()
            }
        })
        .collect();
    let core_len = prepared
        .iter()
        .rposition(|line| !line.is_empty())
        .map_or(0, |idx| idx + 1);
    let mut out = Vec::with_capacity(n);
    if core_len > 0 {
        let mut working = prepared[..core_len].join("\n");
        prescrub_multiline_patterns(&mut working);
        let scrubbed = scrub_content_local(&working, path);
        let together: Vec<&str> = scrubbed.split('\n').collect();
        if together.len() == core_len {
            for text in together {
                out.push(text.to_string());
            }
        } else {
            let fallback_path = if is_env_like_hunk(path, &prepared[..core_len]) {
                ".env"
            } else {
                path
            };
            for text in working.split('\n') {
                let one = scrub_content_local(text, fallback_path);
                let mut single = one.replace('\n', " ");
                if !single.trim().is_empty() && !scrubbed.contains(single.trim()) {
                    single = REDACTED.to_string();
                }
                out.push(single);
            }
        }
    }
    for _ in core_len..n {
        out.push(String::new());
    }
    for text in out.iter_mut() {
        *text = scrub_credential_literals(text);
        let (shown, _) = show_direction_controls(text);
        *text = shown;
    }
    out
}

pub fn scrub_prose(text: &str) -> String {
    let lines: Vec<String> = text.split('\n').map(str::to_string).collect();
    scrub_code_lines("", &lines).join("\n")
}

// ============================================================================
// Privacy Ignore & Glob Exclusions
// ============================================================================

struct PrivacyIgnore {
    root: PathBuf,
    project: Option<Gitignore>,
    project_unreadable: bool,
    nested: HashMap<PathBuf, Option<Gitignore>>,
    unreadable: BTreeSet<PathBuf>,
}

impl PrivacyIgnore {
    fn new(root: &Path) -> Self {
        let file = root.join(".contextstream").join("ignore");
        let mut project_unreadable = false;
        let project = file.is_file().then(|| {
            let mut builder = GitignoreBuilder::new(root);
            project_unreadable = builder.add(&file).is_some();
            builder.build().unwrap_or_else(|_| {
                project_unreadable = true;
                Gitignore::empty()
            })
        });
        Self {
            root: root.to_path_buf(),
            project,
            project_unreadable,
            nested: HashMap::new(),
            unreadable: BTreeSet::new(),
        }
    }

    fn matcher_in(&mut self, dir: &Path) -> Option<&Gitignore> {
        if !self.nested.contains_key(dir) {
            let file = dir.join(".contextignore");
            let loaded = file.is_file().then(|| {
                let (matcher, error) = Gitignore::new(&file);
                if error.is_some() {
                    self.unreadable.insert(dir.to_path_buf());
                }
                matcher
            });
            self.nested.insert(dir.to_path_buf(), loaded);
        }
        self.nested.get(dir).and_then(Option::as_ref)
    }

    fn is_ignored(&mut self, relative: &str) -> Result<bool> {
        if self.project_unreadable {
            return Err(Error::Validation(
                ".contextstream/ignore could not be read; failing closed".to_string(),
            ));
        }
        let full = self.root.join(relative);
        if let Some(project) = &self.project {
            if project
                .matched_path_or_any_parents(&full, false)
                .is_ignore()
            {
                return Ok(true);
            }
        }
        let mut dir = self.root.clone();
        let mut dirs = vec![dir.clone()];
        if let Some(parent) = Path::new(relative).parent() {
            for part in parent.components() {
                dir.push(part);
                dirs.push(dir.clone());
            }
        }
        for dir in dirs {
            let ignored = self
                .matcher_in(&dir)
                .map(|m| m.matched_path_or_any_parents(&full, false).is_ignore());
            if self.unreadable.contains(&dir) {
                return Err(Error::Validation(format!(
                    "{}/.contextignore could not be read; failing closed",
                    dir.display()
                )));
            }
            if ignored == Some(true) {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

fn compile_exclude_globs(patterns: Option<&[String]>) -> Result<Option<globset::GlobSet>> {
    let Some(patterns) = patterns.filter(|p| !p.is_empty()) else {
        return Ok(None);
    };
    let mut builder = globset::GlobSetBuilder::new();
    for raw in patterns {
        let mut pat = raw.trim().trim_start_matches("./").to_string();
        if pat.is_empty() {
            continue;
        }
        if pat.ends_with('/') {
            pat.push_str("**");
        }
        let glob = globset::GlobBuilder::new(&pat)
            .literal_separator(false)
            .build()
            .map_err(|e| Error::Validation(format!("Invalid exclude glob '{raw}': {e}")))?;
        builder.add(glob);
        if !pat.contains('/') {
            if let Ok(nested) = globset::GlobBuilder::new(&format!("**/{pat}"))
                .literal_separator(false)
                .build()
            {
                builder.add(nested);
            }
        }
    }
    builder
        .build()
        .map(Some)
        .map_err(|e| Error::Validation(format!("Invalid exclude glob set: {e}")))
}

// ============================================================================
// Git Subprocess & Diff Parsing
// ============================================================================

async fn run_git(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let mut cmd = Command::new("git");
    cmd.args(args)
        .current_dir(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_COMMON_DIR")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = cmd
        .output()
        .await
        .map_err(|e| Error::Validation(format!("Failed to run git {}: {e}", args.join(" "))))?;
    if !output.status.success() {
        let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(Error::Validation(format!(
            "git {} failed: {}",
            args.first().copied().unwrap_or(""),
            if err.is_empty() {
                "non-zero exit".to_string()
            } else {
                err
            }
        )));
    }
    Ok(output.stdout)
}

async fn resolve_repo_slug(repo_root: &Path) -> String {
    if let Ok(bytes) = run_git(repo_root, &["config", "--get", "remote.origin.url"]).await {
        if let Ok(raw) = std::str::from_utf8(&bytes) {
            if let Some(slug) = slug_from_remote_url(raw.trim()) {
                return slug;
            }
        }
    }
    repo_root
        .file_name()
        .and_then(|s| s.to_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("repo")
        .to_string()
}

pub(crate) fn slug_from_remote_url(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        return None;
    }
    // Handle `https://user:pass@host/org/repo.git`, `ssh://git@host/org/repo.git`, `git@host:org/repo.git`
    let path_part = if let Some((_, after_scheme)) = trimmed.split_once("://") {
        let without_auth = after_scheme
            .split_once('@')
            .map_or(after_scheme, |(_, rest)| rest);
        without_auth.split_once('/').map(|(_, p)| p).unwrap_or("")
    } else if let Some((_, after_colon)) = trimmed.split_once(':') {
        after_colon
    } else {
        trimmed
    };
    let last = path_part.rsplit('/').next().unwrap_or(path_part);
    let stripped = last.strip_suffix(".git").unwrap_or(last).trim();
    if stripped.is_empty() {
        None
    } else {
        Some(stripped.to_string())
    }
}

#[derive(Debug, Clone)]
struct RawDiffEntry {
    path: String,
    old_path: Option<String>,
    status: ReviewFileStatus,
    is_submodule: bool,
}

fn parse_raw_z(bytes: &[u8]) -> Result<Vec<RawDiffEntry>> {
    let tokens: Vec<&[u8]> = bytes.split(|b| *b == 0).collect();
    let mut i = 0;
    let mut entries = Vec::new();
    while i < tokens.len() {
        let meta = tokens[i];
        i += 1;
        if meta.is_empty() {
            continue;
        }
        let meta_str = std::str::from_utf8(meta).map_err(|_| {
            Error::Validation("git diff --raw metadata contained invalid UTF-8".to_string())
        })?;
        // Format: `:<old_mode> <new_mode> <old_oid> <new_oid> <status>`
        let parts: Vec<&str> = meta_str
            .trim_start_matches(':')
            .split_whitespace()
            .collect();
        if parts.len() < 5 {
            continue;
        }
        let old_mode = parts[0];
        let new_mode = parts[1];
        let status_code = parts[4];
        let is_submodule = old_mode == "160000" || new_mode == "160000";
        let first_char = status_code.chars().next().unwrap_or('M');
        if matches!(first_char, 'R' | 'C') {
            let old_bytes = *tokens.get(i).ok_or_else(|| {
                Error::Validation("Unexpected EOF reading old_path from git diff --raw".to_string())
            })?;
            let new_bytes = *tokens.get(i + 1).ok_or_else(|| {
                Error::Validation("Unexpected EOF reading new_path from git diff --raw".to_string())
            })?;
            i += 2;
            let old_path = decode_path_bytes(old_bytes, "old_path")?;
            let new_path = decode_path_bytes(new_bytes, "path")?;
            let status = if first_char == 'R' {
                ReviewFileStatus::Renamed
            } else {
                ReviewFileStatus::Added
            };
            entries.push(RawDiffEntry {
                path: new_path,
                old_path: if first_char == 'R' {
                    Some(old_path)
                } else {
                    None
                },
                status,
                is_submodule,
            });
        } else {
            let path_bytes = *tokens.get(i).ok_or_else(|| {
                Error::Validation("Unexpected EOF reading path from git diff --raw".to_string())
            })?;
            i += 1;
            let path = decode_path_bytes(path_bytes, "path")?;
            let status = match first_char {
                'A' => ReviewFileStatus::Added,
                'D' => ReviewFileStatus::Deleted,
                _ => ReviewFileStatus::Modified,
            };
            entries.push(RawDiffEntry {
                path,
                old_path: None,
                status,
                is_submodule,
            });
        }
    }
    Ok(entries)
}

#[derive(Debug, Clone, Copy, Default)]
struct NumstatEntry {
    add: u32,
    del: u32,
    binary: bool,
}

fn parse_numstat_z(bytes: &[u8]) -> Result<BTreeMap<String, NumstatEntry>> {
    let tokens: Vec<&[u8]> = bytes.split(|b| *b == 0).collect();
    let mut i = 0;
    let mut out = BTreeMap::new();
    while i < tokens.len() {
        let tok = tokens[i];
        i += 1;
        if tok.is_empty() {
            continue;
        }
        let s = std::str::from_utf8(tok).map_err(|_| {
            Error::Validation("git diff --numstat contained invalid UTF-8".to_string())
        })?;
        let mut fields = s.splitn(3, '\t');
        let add_s = fields.next().unwrap_or("");
        let del_s = fields.next().unwrap_or("");
        let rest = fields.next().unwrap_or("");
        let (binary, add, del) = if add_s == "-" && del_s == "-" {
            (true, 0, 0)
        } else {
            (
                false,
                add_s.parse::<u32>().unwrap_or(0),
                del_s.parse::<u32>().unwrap_or(0),
            )
        };
        let path = if rest.is_empty() {
            // Rename/copy under `-z`: `<add>\t<del>\t\0<old_path>\0<new_path>\0`
            let _old_bytes = tokens.get(i).copied().unwrap_or(&[]);
            let new_bytes = tokens.get(i + 1).copied().unwrap_or(&[]);
            i += 2;
            decode_path_bytes(new_bytes, "numstat path")?
        } else {
            if unshowable(rest, false) {
                return Err(Error::Validation(
                    "Diff file path contains control or undecodable characters".to_string(),
                ));
            }
            rest.to_string()
        };
        out.insert(path, NumstatEntry { add, del, binary });
    }
    Ok(out)
}

fn unshowable(text: &str, lines_allowed: bool) -> bool {
    text.chars()
        .any(|c| c == '\u{FFFD}' || (c.is_control() && c != '\t' && !(lines_allowed && c == '\n')))
}

fn decode_path_bytes(bytes: &[u8], place: &str) -> Result<String> {
    let s = std::str::from_utf8(bytes).map_err(|_| {
        Error::Validation(format!(
            "File {place} is not valid UTF-8 and cannot be included in a review capsule"
        ))
    })?;
    if s.is_empty() || s != s.trim() || unshowable(s, false) || s.chars().any(is_direction_control)
    {
        return Err(Error::Validation(format!(
            "File {place} contains whitespace padding, direction controls, or control characters"
        )));
    }
    if s.chars().count() > MAX_PATH_CHARS {
        return Err(Error::Validation(format!(
            "File {place} exceeds {MAX_PATH_CHARS} characters"
        )));
    }
    if scrub_prose(s) != s {
        return Err(Error::Validation(format!(
            "File {place} looks like it contains a secret; exclude it via review.exclude"
        )));
    }
    Ok(s.to_string())
}

fn parse_hunk_header(line: &str) -> Option<(u32, u32, Option<String>)> {
    // Format: `@@ -old_start[,old_count] +new_start[,new_count] @@[ heading]`
    let rest = line.strip_prefix("@@ -")?;
    let (range_part, heading_part) = rest.split_once(" @@")?;
    let (old_part, new_part) = range_part.split_once(" +")?;
    let old_start = old_part
        .split_once(',')
        .map_or(old_part, |(start, _)| start)
        .parse::<u32>()
        .ok()?;
    let new_start = new_part
        .split_once(',')
        .map_or(new_part, |(start, _)| start)
        .parse::<u32>()
        .ok()?;
    let heading = heading_part.trim();
    let heading = if heading.is_empty() {
        None
    } else {
        Some(heading.to_string())
    };
    Some((old_start, new_start, heading))
}

enum ParsedDiffFile {
    Hunks(Vec<ReviewHunkInput>),
    Skipped(ReviewSkipReason),
}

fn split_diff_sections<'a>(
    diff_bytes: &'a [u8],
    entries: &[RawDiffEntry],
) -> Vec<(Option<usize>, &'a [u8])> {
    let mut header_offsets: Vec<(usize, Option<&'a str>, Option<usize>)> = Vec::new();
    let mut pos = 0;
    for line_bytes in diff_bytes.split(|b| *b == b'\n') {
        if line_bytes.starts_with(b"diff --git ") {
            let line_str = std::str::from_utf8(line_bytes).ok();
            header_offsets.push((pos, line_str, None));
        }
        pos += line_bytes.len() + 1;
    }

    let mut claimed = vec![false; entries.len()];

    // Pass 1: Exact `diff --git a/{old} b/{path}` match across all entries first
    // so short filenames like `aaa.txt` cannot steal `zzz b/aaa.txt`'s section.
    for (_, line_str_opt, matched) in &mut header_offsets {
        let Some(line_str) = *line_str_opt else {
            continue;
        };
        if let Some(entry_idx) = entries.iter().enumerate().position(|(idx, e)| {
            if claimed[idx] {
                return false;
            }
            let old = e.old_path.as_deref().unwrap_or(&e.path);
            let expected = format!("diff --git a/{old} b/{}", e.path);
            line_str == expected
        }) {
            claimed[entry_idx] = true;
            *matched = Some(entry_idx);
        }
    }

    // Pass 2: Fallback suffix match only for headers without a Pass 1 exact match,
    // considering only unclaimed entries and preferring the longest matching path.
    for (_, line_str_opt, matched) in &mut header_offsets {
        if matched.is_some() {
            continue;
        }
        let Some(line_str) = *line_str_opt else {
            continue;
        };
        if let Some((entry_idx, _)) = entries
            .iter()
            .enumerate()
            .filter(|(idx, e)| !claimed[*idx] && line_str.ends_with(&format!(" b/{}", e.path)))
            .max_by_key(|(_, e)| e.path.len())
        {
            claimed[entry_idx] = true;
            *matched = Some(entry_idx);
        }
    }

    let mut sections = Vec::with_capacity(header_offsets.len());
    for (idx, (start, _, entry_idx)) in header_offsets.iter().enumerate() {
        let end = header_offsets
            .get(idx + 1)
            .map_or(diff_bytes.len(), |(next, _, _)| *next);
        sections.push((*entry_idx, &diff_bytes[*start..end]));
    }
    sections
}

fn parse_single_file_diff(section: &[u8]) -> ParsedDiffFile {
    if section.len() > MAX_FILE_DIFF_BYTES {
        return ParsedDiffFile::Skipped(ReviewSkipReason::TooLarge);
    }
    let mut hunks: Vec<ReviewHunkInput> = Vec::new();
    let mut current_old_start = 0u32;
    let mut current_new_start = 0u32;
    let mut current_heading: Option<String> = None;
    let mut current_lines: Vec<String> = Vec::new();
    let mut in_hunk = false;
    let mut total_lines = 0usize;

    let flush_hunk = |in_hunk: &mut bool,
                      hunks: &mut Vec<ReviewHunkInput>,
                      old_start: u32,
                      new_start: u32,
                      heading: &mut Option<String>,
                      lines: &mut Vec<String>| {
        if *in_hunk && !lines.is_empty() {
            hunks.push(ReviewHunkInput {
                old_start,
                new_start,
                heading: heading.take(),
                lines: std::mem::take(lines),
            });
        } else {
            *heading = None;
            lines.clear();
        }
        *in_hunk = false;
    };

    for raw_line in section.split(|b| *b == b'\n') {
        let raw_line = raw_line.strip_suffix(b"\r").unwrap_or(raw_line);
        let Ok(line) = std::str::from_utf8(raw_line) else {
            return ParsedDiffFile::Skipped(ReviewSkipReason::Undecodable);
        };
        if unshowable(line, false) {
            return ParsedDiffFile::Skipped(ReviewSkipReason::Undecodable);
        }
        if !in_hunk {
            if line.starts_with("Binary files ") || line == "GIT binary patch" {
                return ParsedDiffFile::Skipped(ReviewSkipReason::Binary);
            }
            if line.starts_with("@@ ") {
                if let Some((old_start, new_start, heading)) = parse_hunk_header(line) {
                    in_hunk = true;
                    current_old_start = old_start;
                    current_new_start = new_start;
                    current_heading = heading;
                }
            }
            continue;
        }
        if line.starts_with("@@ ") {
            flush_hunk(
                &mut in_hunk,
                &mut hunks,
                current_old_start,
                current_new_start,
                &mut current_heading,
                &mut current_lines,
            );
            if hunks.len() >= MAX_FILE_HUNKS {
                return ParsedDiffFile::Skipped(ReviewSkipReason::TooLarge);
            }
            if let Some((old_start, new_start, heading)) = parse_hunk_header(line) {
                in_hunk = true;
                current_old_start = old_start;
                current_new_start = new_start;
                current_heading = heading;
            }
            continue;
        }
        if line.starts_with('\\') {
            // `\ No newline at end of file` meta-line
            continue;
        }
        if line.starts_with(' ') || line.starts_with('+') || line.starts_with('-') {
            total_lines += 1;
            if total_lines > MAX_FILE_DIFF_LINES {
                return ParsedDiffFile::Skipped(ReviewSkipReason::TooLarge);
            }
            current_lines.push(line.to_string());
        } else {
            // Unexpected trailing header/line ends current hunk
            flush_hunk(
                &mut in_hunk,
                &mut hunks,
                current_old_start,
                current_new_start,
                &mut current_heading,
                &mut current_lines,
            );
        }
    }
    flush_hunk(
        &mut in_hunk,
        &mut hunks,
        current_old_start,
        current_new_start,
        &mut current_heading,
        &mut current_lines,
    );
    if hunks.len() > MAX_FILE_HUNKS {
        return ParsedDiffFile::Skipped(ReviewSkipReason::TooLarge);
    }
    ParsedDiffFile::Hunks(hunks)
}

fn scrub_hunks(path: &str, hunks: Vec<ReviewHunkInput>) -> Vec<ReviewHunkInput> {
    hunks
        .into_iter()
        .map(|hunk| {
            let heading = hunk.heading.map(|h| {
                let (shown, _) = show_direction_controls(&h);
                let scrubbed = scrub_prose(&shown);
                let trimmed = scrubbed.trim();
                trimmed.chars().take(MAX_HEADING_CHARS).collect::<String>()
            });
            let heading = heading.filter(|h| !h.is_empty());

            let marks: Vec<char> = hunk
                .lines
                .iter()
                .map(|l| l.chars().next().unwrap_or(' '))
                .collect();
            let bodies: Vec<String> = hunk
                .lines
                .iter()
                .map(|l| l.chars().skip(1).collect())
                .collect();
            let scrubbed_bodies = scrub_code_lines(path, &bodies);
            let lines = marks
                .into_iter()
                .zip(scrubbed_bodies)
                .map(|(mark, body)| format!("{mark}{body}"))
                .collect();
            ReviewHunkInput {
                old_start: hunk.old_start,
                new_start: hunk.new_start,
                heading,
                lines,
            }
        })
        .collect()
}

fn validate_and_scrub_prose(raw: &str, max_chars: usize, place: &str) -> Result<String> {
    let normalized = raw.replace("\r\n", "\n");
    let trimmed = normalized.trim();
    if trimmed.is_empty() {
        return Err(Error::Validation(format!("{place} must not be empty")));
    }
    if unshowable(trimmed, true) {
        return Err(Error::Validation(format!(
            "{place} contains control characters or undecodable bytes"
        )));
    }
    if trimmed.chars().count() > max_chars {
        return Err(Error::Validation(format!(
            "{place} exceeds {max_chars} characters"
        )));
    }
    let (shown, _) = show_direction_controls(trimmed);
    let scrubbed = scrub_prose(&shown);
    if ANSWER_FORM_RE.is_match(&scrubbed) {
        return Err(Error::Validation(format!(
            "{place} contains answer-form lines (VERDICT:/CLAIM C#:); review briefs must not carry reviewer answers"
        )));
    }
    Ok(scrubbed)
}

fn validate_and_scrub_one_line(raw: &str, max_chars: usize, place: &str) -> Result<String> {
    let trimmed = raw.trim();
    if unshowable(trimmed, false) {
        return Err(Error::Validation(format!(
            "{place} contains a line break, control character, or undecodable bytes"
        )));
    }
    if trimmed.chars().count() > max_chars {
        return Err(Error::Validation(format!(
            "{place} exceeds {max_chars} characters"
        )));
    }
    let (shown, _) = show_direction_controls(trimmed);
    Ok(scrub_prose(&shown))
}

async fn resolve_excerpt_lines(
    repo_root: &Path,
    head_oid: &str,
    ex: &ReviewExcerptToolInput,
) -> Result<Vec<String>> {
    if ex.first_line == 0 {
        return Err(Error::Validation(
            "review.excerpts[].first_line must be >= 1".to_string(),
        ));
    }
    if let Some(name) = ex.name.as_deref() {
        if !EXCERPT_NAME_RE.is_match(name) {
            return Err(Error::Validation(format!(
                "Invalid excerpt name '{name}': must match ^[A-Za-z_][A-Za-z0-9_.:]{{0,99}}$"
            )));
        }
    }
    let raw_lines = if let Some(lines) = ex.lines.as_ref() {
        lines.clone()
    } else {
        let last_line = ex.last_line.ok_or_else(|| {
            Error::Validation(format!(
                "review.excerpts for '{}' requires either lines or last_line",
                ex.path
            ))
        })?;
        if last_line < ex.first_line {
            return Err(Error::Validation(format!(
                "review.excerpts for '{}' has last_line ({last_line}) < first_line ({})",
                ex.path, ex.first_line
            )));
        }
        let spec = format!("{head_oid}:{}", ex.path);
        let bytes = run_git(repo_root, &["show", "--end-of-options", &spec]).await?;
        let text = std::str::from_utf8(&bytes).map_err(|_| {
            Error::Validation(format!("Excerpt file '{}' is not valid UTF-8", ex.path))
        })?;
        let file_lines: Vec<String> = text
            .split('\n')
            .map(|l| l.strip_suffix('\r').unwrap_or(l).to_string())
            .collect();
        let start_idx = (ex.first_line - 1) as usize;
        let end_idx = last_line as usize;
        if end_idx > file_lines.len() {
            return Err(Error::Validation(format!(
                "Excerpt range {}-{} exceeds '{}' line count ({})",
                ex.first_line,
                last_line,
                ex.path,
                file_lines.len()
            )));
        }
        file_lines[start_idx..end_idx].to_vec()
    };

    if raw_lines.is_empty() || raw_lines.len() > MAX_EXCERPT_LINES {
        return Err(Error::Validation(format!(
            "Excerpt for '{}' must have 1..={MAX_EXCERPT_LINES} lines (got {})",
            ex.path,
            raw_lines.len()
        )));
    }
    if u64::from(ex.first_line) + raw_lines.len() as u64 - 1 > u64::from(u32::MAX) {
        return Err(Error::Validation(
            "Excerpt line range overflows u32".to_string(),
        ));
    }
    let mut cleaned = Vec::with_capacity(raw_lines.len());
    for line in raw_lines {
        let line = line.strip_suffix('\r').unwrap_or(&line);
        if unshowable(line, false) {
            return Err(Error::Validation(format!(
                "Excerpt in '{}' contains control characters or undecodable bytes",
                ex.path
            )));
        }
        cleaned.push(line.to_string());
    }
    Ok(scrub_code_lines(&ex.path, &cleaned))
}

/// Package a local git diff (`base..head`) into a scrubbed, limit-checked
/// `CreateReviewCapsuleRequest` ready for `POST /api/v1/capsules/review`.
pub async fn build_review_capsule_request(
    review: ReviewToolInput,
    project_id: Uuid,
    workspace_id: Option<Uuid>,
    name: Option<String>,
    folder_hint: Option<&str>,
) -> Result<(CreateReviewCapsuleRequest, PathBuf)> {
    let search_dir = match folder_hint.map(str::trim).filter(|s| !s.is_empty()) {
        Some(dir) => PathBuf::from(dir),
        None => std::env::current_dir()
            .map_err(|e| Error::Validation(format!("Cannot determine current directory: {e}")))?,
    };
    if !search_dir.exists() {
        return Err(Error::Validation(format!(
            "folder_path '{}' does not exist",
            search_dir.display()
        )));
    }
    let top_bytes = run_git(&search_dir, &["rev-parse", "--show-toplevel"]).await?;
    let repo_root_str = std::str::from_utf8(&top_bytes)
        .map_err(|_| Error::Validation("git repository path is not valid UTF-8".to_string()))?
        .trim();
    let repo_root = PathBuf::from(repo_root_str);

    let base_arg = review.base.trim();
    if base_arg.is_empty() {
        return Err(Error::Validation("review.base is required".to_string()));
    }
    let head_arg = review
        .head
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("HEAD");

    let base_commit_ref = format!("{base_arg}^{{commit}}");
    let head_commit_ref = format!("{head_arg}^{{commit}}");
    let base_oid = String::from_utf8(
        run_git(
            &repo_root,
            &[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &base_commit_ref,
            ],
        )
        .await?,
    )
    .map_err(|_| Error::Validation("git rev-parse base returned non-UTF-8".to_string()))?
    .trim()
    .to_ascii_lowercase();
    let head_oid = String::from_utf8(
        run_git(
            &repo_root,
            &[
                "rev-parse",
                "--verify",
                "--end-of-options",
                &head_commit_ref,
            ],
        )
        .await?,
    )
    .map_err(|_| Error::Validation("git rev-parse head returned non-UTF-8".to_string()))?
    .trim()
    .to_ascii_lowercase();

    if !COMMIT_ID_HEX.is_match(&base_oid) || !COMMIT_ID_HEX.is_match(&head_oid) {
        return Err(Error::Validation(
            "Resolved base/head commit OIDs must be 40 or 64 hex characters".to_string(),
        ));
    }

    let two_dot = review.two_dot.unwrap_or(false);
    let (merge_base, range) = if !two_dot {
        let mb_bytes = run_git(&repo_root, &["merge-base", &base_oid, &head_oid])
            .await
            .map_err(|_| {
                Error::Validation(
                    "base and head share no common ancestor; set review.two_dot=true to diff them directly."
                        .to_string(),
                )
            })?;
        let mb = String::from_utf8(mb_bytes)
            .map_err(|_| Error::Validation("git merge-base returned non-UTF-8".to_string()))?
            .trim()
            .to_ascii_lowercase();
        if !COMMIT_ID_HEX.is_match(&mb) {
            return Err(Error::Validation(
                "base and head share no common ancestor; set review.two_dot=true to diff them directly."
                    .to_string(),
            ));
        }
        let range = format!("{mb}..{head_oid}");
        (Some(mb), range)
    } else {
        (None, format!("{base_oid}..{head_oid}"))
    };

    let repo_raw = resolve_repo_slug(&repo_root).await;
    let repo = validate_and_scrub_one_line(&repo_raw, MAX_REPO_CHARS, "repo")?;
    if repo.is_empty() {
        return Err(Error::Validation("repo name resolved to empty".to_string()));
    }

    let title = review
        .title
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| validate_and_scrub_one_line(s, MAX_TITLE_CHARS, "review.title"))
        .transpose()?;
    let task = validate_and_scrub_prose(&review.task, MAX_TASK_CHARS, "review.task")?;

    let raw_claims = review.claims.unwrap_or_default();
    if raw_claims.len() > MAX_ITEMS {
        return Err(Error::Validation(format!(
            "review.claims exceeds maximum of {MAX_ITEMS} items"
        )));
    }
    let mut claims = Vec::with_capacity(raw_claims.len());
    for (idx, c) in raw_claims.iter().enumerate() {
        claims.push(validate_and_scrub_prose(
            c,
            MAX_ITEM_CHARS,
            &format!("review.claims[{}]", idx + 1),
        )?);
    }

    let raw_evidence = review.evidence.unwrap_or_default();
    if raw_evidence.len() > MAX_ITEMS {
        return Err(Error::Validation(format!(
            "review.evidence exceeds maximum of {MAX_ITEMS} items"
        )));
    }
    let mut evidence = Vec::with_capacity(raw_evidence.len());
    for (idx, e) in raw_evidence.iter().enumerate() {
        evidence.push(validate_and_scrub_prose(
            e,
            MAX_ITEM_CHARS,
            &format!("review.evidence[{}]", idx + 1),
        )?);
    }

    if let Some(part_chars) = review.part_chars {
        if !(MAX_PART_CHARS_MIN..=MAX_PART_CHARS_MAX).contains(&part_chars) {
            return Err(Error::Validation(format!(
                "review.part_chars must be between {MAX_PART_CHARS_MIN} and {MAX_PART_CHARS_MAX}"
            )));
        }
    }

    let raw_z = run_git(
        &repo_root,
        &[
            "-c",
            "core.quotePath=false",
            "diff",
            "--raw",
            "-z",
            "-M",
            &range,
        ],
    )
    .await?;
    let raw_entries = parse_raw_z(&raw_z)?;
    if raw_entries.is_empty() {
        return Err(Error::Validation(
            "No changes between base and head commits.".to_string(),
        ));
    }
    if raw_entries.len() > MAX_FILES {
        return Err(Error::Validation(format!(
            "Diff touches {} files, exceeding limit of {MAX_FILES}; pass review.exclude globs or narrow the commit range.",
            raw_entries.len()
        )));
    }

    let numstat_z = run_git(
        &repo_root,
        &[
            "-c",
            "core.quotePath=false",
            "diff",
            "--numstat",
            "-z",
            "-M",
            &range,
        ],
    )
    .await?;
    let numstats = parse_numstat_z(&numstat_z)?;

    let context_lines = review.context_lines.unwrap_or(3).min(50);
    let unified_arg = format!("-U{context_lines}");
    let diff_bytes = run_git(
        &repo_root,
        &[
            "-c",
            "diff.noprefix=false",
            "-c",
            "diff.mnemonicPrefix=false",
            "-c",
            "core.quotePath=false",
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "-M",
            &unified_arg,
            &range,
        ],
    )
    .await?;

    let mut privacy = PrivacyIgnore::new(&repo_root);
    let exclude_globs = compile_exclude_globs(review.exclude.as_deref())?;

    let sections = split_diff_sections(&diff_bytes, &raw_entries);
    let mut section_by_entry: BTreeMap<usize, &[u8]> = BTreeMap::new();
    for (idx_opt, bytes) in sections {
        if let Some(idx) = idx_opt {
            section_by_entry.insert(idx, bytes);
        }
    }

    let mut files: Vec<ReviewFileInput> = Vec::with_capacity(raw_entries.len());
    for (idx, entry) in raw_entries.iter().enumerate() {
        let stats = numstats.get(&entry.path).copied().unwrap_or_default();
        let excluded_by_privacy = privacy.is_ignored(&entry.path)?
            || entry
                .old_path
                .as_deref()
                .map(|p| privacy.is_ignored(p))
                .transpose()?
                .unwrap_or(false);
        let excluded_by_glob = exclude_globs.as_ref().is_some_and(|set| {
            set.is_match(&entry.path)
                || entry
                    .old_path
                    .as_deref()
                    .is_some_and(|old| set.is_match(old))
        });
        let sensitive = is_sensitive_review_file(&entry.path)
            || entry
                .old_path
                .as_deref()
                .is_some_and(is_sensitive_review_file);

        if entry.is_submodule || excluded_by_privacy || excluded_by_glob || sensitive {
            files.push(ReviewFileInput {
                path: entry.path.clone(),
                old_path: entry.old_path.clone(),
                status: entry.status,
                skipped: Some(ReviewSkipReason::Excluded),
                add: stats.add,
                del: stats.del,
                hunks: Vec::new(),
                excerpts: Vec::new(),
            });
            continue;
        }
        if stats.binary {
            files.push(ReviewFileInput {
                path: entry.path.clone(),
                old_path: entry.old_path.clone(),
                status: entry.status,
                skipped: Some(ReviewSkipReason::Binary),
                add: 0,
                del: 0,
                hunks: Vec::new(),
                excerpts: Vec::new(),
            });
            continue;
        }

        let section = section_by_entry.get(&idx).copied().unwrap_or(&[]);
        match parse_single_file_diff(section) {
            ParsedDiffFile::Skipped(reason) => {
                files.push(ReviewFileInput {
                    path: entry.path.clone(),
                    old_path: entry.old_path.clone(),
                    status: entry.status,
                    skipped: Some(reason),
                    add: if reason == ReviewSkipReason::Binary {
                        0
                    } else {
                        stats.add
                    },
                    del: if reason == ReviewSkipReason::Binary {
                        0
                    } else {
                        stats.del
                    },
                    hunks: Vec::new(),
                    excerpts: Vec::new(),
                });
            }
            ParsedDiffFile::Hunks(hunks) => {
                let scrubbed_hunks = scrub_hunks(&entry.path, hunks);
                files.push(ReviewFileInput {
                    path: entry.path.clone(),
                    old_path: entry.old_path.clone(),
                    status: entry.status,
                    skipped: None,
                    add: stats.add,
                    del: stats.del,
                    hunks: scrubbed_hunks,
                    excerpts: Vec::new(),
                });
            }
        }
    }

    // Attach caller excerpts only to shown files of the diff.
    if let Some(excerpts) = review.excerpts.as_ref() {
        for ex in excerpts {
            let Some(file) = files
                .iter_mut()
                .find(|f| f.path == ex.path && f.skipped.is_none())
            else {
                return Err(Error::Validation(format!(
                    "Excerpt path '{}' is not an unskipped file of the change",
                    ex.path
                )));
            };
            if file.excerpts.len() >= MAX_EXCERPTS_PER_FILE {
                return Err(Error::Validation(format!(
                    "File '{}' exceeds maximum of {MAX_EXCERPTS_PER_FILE} excerpts",
                    ex.path
                )));
            }
            let lines = resolve_excerpt_lines(&repo_root, &head_oid, ex).await?;
            file.excerpts.push(ReviewExcerptInput {
                path: None,
                name: ex.name.clone(),
                first_line: ex.first_line,
                lines,
            });
        }
    }

    // Backend `render.rs:566` rejects an unskipped non-sensitive file with 0 hunks
    // and 0 excerpts (e.g. mode-only change, empty 0-byte file, 100% pure rename).
    for file in &mut files {
        if file.skipped.is_none() && file.hunks.is_empty() && file.excerpts.is_empty() {
            file.skipped = Some(ReviewSkipReason::Excluded);
        }
    }

    if files.iter().all(|f| f.skipped.is_some()) {
        return Err(Error::Validation(
            "All changed files were skipped (binary, undecodable, excluded, or empty); nothing remains to review."
                .to_string(),
        ));
    }

    let total_hunks: usize = files.iter().map(|f| f.hunks.len()).sum();
    if total_hunks > MAX_HUNKS {
        return Err(Error::Validation(format!(
            "Diff has {total_hunks} hunks (limit {MAX_HUNKS}); pass review.exclude globs or narrow the commit range."
        )));
    }
    let total_lines: usize = files
        .iter()
        .map(|f| {
            f.hunks.iter().map(|h| h.lines.len()).sum::<usize>()
                + f.excerpts.iter().map(|x| x.lines.len()).sum::<usize>()
        })
        .sum();
    if total_lines > MAX_LINES {
        return Err(Error::Validation(format!(
            "Diff has {total_lines} lines (limit {MAX_LINES}); pass review.exclude globs or narrow the commit range."
        )));
    }

    let clean_name = name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| validate_and_scrub_one_line(s, 120, "name"))
        .transpose()?;

    let req = CreateReviewCapsuleRequest {
        project_id,
        workspace_id,
        name: clean_name,
        review: ReviewInput {
            contract_version: REVIEW_CAPSULE_CONTRACT_VERSION,
            repo,
            base: base_oid,
            head: head_oid,
            merge_base,
            two_dot,
            title,
            task,
            claims,
            evidence,
            context_lines: review.context_lines,
            part_chars: review.part_chars,
            files,
        },
    };

    let body_bytes = serde_json::to_vec(&req)?;
    if body_bytes.len() > MAX_UPLOAD_BODY_BYTES {
        return Err(Error::Validation(format!(
            "Review capsule payload is {} bytes, exceeding pre-upload limit of {MAX_UPLOAD_BODY_BYTES} bytes; exclude generated/large files or narrow the commit range.",
            body_bytes.len()
        )));
    }

    Ok((req, repo_root))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;

    fn git_sync(dir: &Path, args: &[&str]) -> String {
        let out = StdCommand::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "Tester")
            .env("GIT_AUTHOR_EMAIL", "tester@example.com")
            .env("GIT_COMMITTER_NAME", "Tester")
            .env("GIT_COMMITTER_EMAIL", "tester@example.com")
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    #[test]
    fn sensitive_file_union_blocks_dotenv_examples_and_keys() {
        for path in [
            ".env",
            ".env.local",
            ".env.example",
            ".env.sample",
            "config/prod.env.staging",
            "keys/server.pem",
            "keys/app.keystore",
            "keys/id_rsa.pub",
            ".ssh/config",
            ".aws/credentials",
            ".mcp.json",
            "credentials.yaml",
            "infra/terraform.tfstate",
        ] {
            assert!(is_sensitive_review_file(path), "expected sensitive: {path}");
        }
        for path in [
            "src/main.rs",
            "crates/mcp-tools/src/capsule.rs",
            "README.md",
        ] {
            assert!(
                !is_sensitive_review_file(path),
                "unexpected sensitive: {path}"
            );
        }
    }

    #[test]
    fn scrubbing_redacts_secrets_while_preserving_capsule_identifiers() {
        let pem_begin = format!("-----BEGIN {} KEY-----", "RSA PRIVATE");
        let pem_end = format!("-----END {} KEY-----", "RSA PRIVATE");
        let aws_key = format!("{}IOSFODNN7EXAMPLE", "AKIA");
        let stripe = format!("{}{}", "sk_live_", "abcdefghijklmnop123456");
        let cs_api = format!("{}{}", "cbiq_", "secretvalue1234567890");
        let cs_cap = format!("{}{}", "capsule_", "0123456789abcdef0123456789abcdef");

        let lines = vec![
            "let capsule_id = \"cap_1\";".to_string(),
            "let capsule_routed = true;".to_string(),
            pem_begin,
            "MIIBogIBAAJBALRiMLAHudeSA/x3hB2f+2NRkJMxEhCMbxAnssRBjBISBQ1Q".to_string(),
            pem_end,
            format!("const KEY = \"{aws_key}\";"),
            format!("const STRIPE = \"{stripe}\";"),
            format!("const CS_KEY = \"{cs_api}\";"),
            format!("const CAP_CRED = \"{cs_cap}\";"),
            "const DB = \"postgres://alice:hunter2pw@db.internal:5432/app\";".to_string(),
            "let ok = true;".to_string(),
        ];

        let scrubbed = scrub_code_lines("src/auth.rs", &lines);
        assert_eq!(scrubbed.len(), lines.len());
        assert_eq!(scrubbed[0], "let capsule_id = \"cap_1\";");
        assert_eq!(scrubbed[1], "let capsule_routed = true;");
        assert_eq!(scrubbed[2], REDACTED);
        assert_eq!(scrubbed[3], REDACTED);
        assert_eq!(scrubbed[4], REDACTED);
        assert!(scrubbed[5].contains(REDACTED) && !scrubbed[5].contains(&aws_key));
        assert!(scrubbed[6].contains(REDACTED) && !scrubbed[6].contains(&stripe));
        assert!(scrubbed[7].contains(REDACTED) && !scrubbed[7].contains(&cs_api));
        assert!(scrubbed[8].contains(REDACTED) && !scrubbed[8].contains(&cs_cap));
        assert!(scrubbed[9].contains(REDACTED) && !scrubbed[9].contains("hunter2pw"));
        assert_eq!(scrubbed[10], "let ok = true;");
    }

    #[test]
    fn repo_slug_strips_credentials_and_dot_git() {
        assert_eq!(
            slug_from_remote_url("https://user:secret@github.com/org/my-repo.git"),
            Some("my-repo".to_string())
        );
        assert_eq!(
            slug_from_remote_url("git@github.com:org/contextstream.git"),
            Some("contextstream".to_string())
        );
        assert_eq!(
            slug_from_remote_url("ssh://git@example.com:2222/team/app/"),
            Some("app".to_string())
        );
    }

    #[test]
    fn parse_numstat_z_handles_renames_and_binary() {
        let mut buf = Vec::new();
        buf.extend_from_slice(b"12\t3\t\0old/name.rs\0new/name.rs\0");
        buf.extend_from_slice(b"-\t-\tassets/logo.png\0");
        let parsed = parse_numstat_z(&buf).unwrap();
        assert_eq!(parsed["new/name.rs"].add, 12);
        assert_eq!(parsed["new/name.rs"].del, 3);
        assert!(!parsed["new/name.rs"].binary);
        assert!(parsed["assets/logo.png"].binary);
    }

    #[tokio::test]
    async fn throwaway_git_repo_packages_diff_skips_and_scrubs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git_sync(root, &["init", "-b", "main"]);
        git_sync(
            root,
            &[
                "remote",
                "add",
                "origin",
                "https://token@github.com/acme/widget.git",
            ],
        );
        // Poison local git config to verify `-c diff.noprefix=false` immunity.
        git_sync(root, &["config", "diff.noprefix", "true"]);
        git_sync(root, &["config", "core.quotePath", "true"]);

        std::fs::write(root.join("kept.rs"), "fn a() {}\n").unwrap();
        std::fs::write(
            root.join("to_rename.rs"),
            "fn renamed() {\n    let x = 1;\n}\n",
        )
        .unwrap();
        std::fs::write(root.join("ignored.txt"), "initial\n").unwrap();
        std::fs::write(root.join(".contextignore"), "ignored.txt\n").unwrap();
        git_sync(root, &["add", "."]);
        git_sync(root, &["commit", "-m", "base"]);
        let base_oid = git_sync(root, &["rev-parse", "HEAD"]);

        let secret = format!("{}{}", "ghp_", "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdef1234");
        std::fs::write(
            root.join("kept.rs"),
            format!("fn a() {{\n    let tok = \"{secret}\";\n}}\n"),
        )
        .unwrap();
        std::fs::rename(root.join("to_rename.rs"), root.join("renamed.rs")).unwrap();
        std::fs::write(root.join("ignored.txt"), "secret ignore\n").unwrap();
        std::fs::write(
            root.join(".env.example"),
            "API_KEY=something_secret_12345\n",
        )
        .unwrap();
        std::fs::write(root.join("empty.txt"), "").unwrap();
        std::fs::write(root.join("bin.dat"), [0u8, 159, 146, 150, 0, 1, 2]).unwrap();
        std::fs::write(root.join("bad_utf8.txt"), [0x66u8, 0x6f, 0x80, 0x0a]).unwrap();
        git_sync(root, &["add", "."]);
        git_sync(root, &["commit", "-m", "head"]);
        let head_oid = git_sync(root, &["rev-parse", "HEAD"]);

        let (req, _) = build_review_capsule_request(
            ReviewToolInput {
                base: base_oid.clone(),
                head: Some(head_oid.clone()),
                two_dot: Some(false),
                title: Some("Widget update".to_string()),
                task: "Review widget changes".to_string(),
                claims: Some(vec!["No secrets leaked".to_string()]),
                evidence: Some(vec!["cargo test".to_string()]),
                context_lines: Some(3),
                part_chars: None,
                exclude: None,
                excerpts: Some(vec![ReviewExcerptToolInput {
                    path: "kept.rs".to_string(),
                    first_line: 1,
                    last_line: Some(3),
                    lines: None,
                    name: Some("a".to_string()),
                }]),
            },
            Uuid::new_v4(),
            None,
            None,
            Some(root.to_str().unwrap()),
        )
        .await
        .expect("packaged review capsule");

        assert_eq!(req.review.repo, "widget");
        assert_eq!(req.review.merge_base.as_deref(), Some(base_oid.as_str()));
        let serialized = serde_json::to_string(&req).unwrap();
        assert!(!serialized.contains(&secret), "secret leaked in payload");

        let by_path: HashMap<&str, &ReviewFileInput> = req
            .review
            .files
            .iter()
            .map(|f| (f.path.as_str(), f))
            .collect();

        let kept = by_path["kept.rs"];
        assert!(kept.skipped.is_none());
        assert!(!kept.hunks.is_empty());
        assert_eq!(kept.excerpts.len(), 1);
        assert!(kept.hunks[0].lines.iter().any(|l| l.contains(REDACTED)));

        assert_eq!(
            by_path["ignored.txt"].skipped,
            Some(ReviewSkipReason::Excluded)
        );
        assert_eq!(
            by_path[".env.example"].skipped,
            Some(ReviewSkipReason::Excluded)
        );
        assert_eq!(
            by_path["empty.txt"].skipped,
            Some(ReviewSkipReason::Excluded)
        );
        assert_eq!(by_path["renamed.rs"].status, ReviewFileStatus::Renamed);
        assert_eq!(
            by_path["renamed.rs"].skipped,
            Some(ReviewSkipReason::Excluded)
        );
        assert_eq!(by_path["bin.dat"].skipped, Some(ReviewSkipReason::Binary));
        assert_eq!(
            by_path["bad_utf8.txt"].skipped,
            Some(ReviewSkipReason::Undecodable)
        );
    }

    #[tokio::test]
    async fn disjoint_history_requires_two_dot_flag() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git_sync(root, &["init", "-b", "main"]);
        std::fs::write(root.join("a.txt"), "one\n").unwrap();
        git_sync(root, &["add", "."]);
        git_sync(root, &["commit", "-m", "main"]);
        let base_oid = git_sync(root, &["rev-parse", "HEAD"]);

        git_sync(root, &["checkout", "--orphan", "orphan"]);
        git_sync(root, &["rm", "-rf", "."]);
        std::fs::write(root.join("b.txt"), "two\n").unwrap();
        git_sync(root, &["add", "."]);
        git_sync(root, &["commit", "-m", "orphan"]);
        let head_oid = git_sync(root, &["rev-parse", "HEAD"]);

        let err = build_review_capsule_request(
            ReviewToolInput {
                base: base_oid.clone(),
                head: Some(head_oid.clone()),
                two_dot: Some(false),
                task: "Disjoint check".to_string(),
                ..Default::default()
            },
            Uuid::new_v4(),
            None,
            None,
            Some(root.to_str().unwrap()),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("two_dot=true"), "{err}");

        let (ok, _) = build_review_capsule_request(
            ReviewToolInput {
                base: base_oid,
                head: Some(head_oid),
                two_dot: Some(true),
                task: "Disjoint check with two_dot".to_string(),
                ..Default::default()
            },
            Uuid::new_v4(),
            None,
            None,
            Some(root.to_str().unwrap()),
        )
        .await
        .expect("two_dot works on disjoint history");
        assert!(ok.review.two_dot);
        assert!(ok.review.merge_base.is_none());
    }

    #[tokio::test]
    async fn excludes_crlf_no_newline_and_excerpt_on_skipped_file_behave_correctly() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git_sync(root, &["init", "-b", "main"]);
        std::fs::write(root.join("removed.txt"), "old1\r\nold2").unwrap();
        std::fs::write(root.join("keep.rs"), "fn keep() {}\n").unwrap();
        git_sync(root, &["add", "."]);
        git_sync(root, &["commit", "-m", "base"]);
        let base_oid = git_sync(root, &["rev-parse", "HEAD"]);

        std::fs::remove_file(root.join("removed.txt")).unwrap();
        std::fs::write(root.join("keep.rs"), "fn keep() {}\r\n// no trailing nl").unwrap();
        std::fs::write(root.join("added.txt"), "new1\r\nnew2").unwrap();
        std::fs::create_dir_all(root.join("vendor")).unwrap();
        std::fs::write(root.join("vendor/lib.js"), "console.log(1);\n").unwrap();
        std::fs::write(root.join("types.gen.rs"), "pub struct Gen;\n").unwrap();
        git_sync(root, &["add", "."]);
        git_sync(root, &["commit", "-m", "head"]);
        let head_oid = git_sync(root, &["rev-parse", "HEAD"]);

        let (req, _) = build_review_capsule_request(
            ReviewToolInput {
                base: base_oid.clone(),
                head: Some(head_oid.clone()),
                two_dot: Some(false),
                task: "Verify exclude globs, CRLF, and no-newline handling".to_string(),
                exclude: Some(vec!["*.gen.rs".to_string(), "vendor/".to_string()]),
                ..Default::default()
            },
            Uuid::new_v4(),
            None,
            None,
            Some(root.to_str().unwrap()),
        )
        .await
        .expect("build review capsule");

        let by_path: HashMap<&str, &ReviewFileInput> = req
            .review
            .files
            .iter()
            .map(|f| (f.path.as_str(), f))
            .collect();

        assert_eq!(
            by_path["vendor/lib.js"].skipped,
            Some(ReviewSkipReason::Excluded)
        );
        assert_eq!(
            by_path["types.gen.rs"].skipped,
            Some(ReviewSkipReason::Excluded)
        );

        let added = by_path["added.txt"];
        assert!(added.skipped.is_none());
        assert_eq!(added.hunks[0].old_start, 0);
        assert_eq!(added.hunks[0].new_start, 1);
        assert!(added.hunks[0]
            .lines
            .iter()
            .all(|l| !l.ends_with('\r') && !l.starts_with('\\')));

        let removed = by_path["removed.txt"];
        assert!(removed.skipped.is_none());
        assert_eq!(removed.hunks[0].old_start, 1);
        assert_eq!(removed.hunks[0].new_start, 0);

        let keep = by_path["keep.rs"];
        assert!(keep.skipped.is_none());
        assert!(keep.hunks[0]
            .lines
            .iter()
            .all(|l| !l.ends_with('\r') && !l.starts_with('\\')));

        // Excerpt targeting an excluded file must fail pre-upload validation.
        let err = build_review_capsule_request(
            ReviewToolInput {
                base: base_oid,
                head: Some(head_oid),
                task: "Excerpt on skipped file".to_string(),
                exclude: Some(vec!["*.gen.rs".to_string()]),
                excerpts: Some(vec![ReviewExcerptToolInput {
                    path: "types.gen.rs".to_string(),
                    first_line: 1,
                    last_line: Some(1),
                    lines: None,
                    name: None,
                }]),
                ..Default::default()
            },
            Uuid::new_v4(),
            None,
            None,
            Some(root.to_str().unwrap()),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not an unskipped file"), "{err}");
    }

    #[test]
    fn scrubbing_redacts_back_to_back_prefixed_tokens_with_single_delimiters() {
        let cap_hex_a = format!("{}{}", "capsule_", "0123456789abcdef0123456789abcdef");
        let cap_hex_b = format!("{}{}", "capsule_", "fedcba9876543210fedcba9876543210");
        let cap_uuid_a = format!("{}{}", "capsule_", "11111111-2222-3333-4444-555555555555");
        let cap_uuid_b = format!("{}{}", "capsule_", "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
        let cbiq_a = format!("{}{}", "cbiq_", "firstsecretvalue1234");
        let cbiq_b = format!("{}{}", "cbiq_", "secondsecretvalue567");
        let cbst_a = format!("{}{}", "cbst_", "setupfirsttoken12345");
        let cbst_b = format!("{}{}", "cbst_", "setupsecondtoken6789");
        let stripe_a = format!("{}{}", "sk_live_", "abcdefghijklmnop1234");
        let stripe_b = format!("{}{}", "sk_live_", "zyxwvutsrqponmlk9876");
        let ghp_a = format!("{}{}", "ghp_", "aaaaaabbbbbbccccccdd");
        let ghp_b = format!("{}{}", "ghp_", "eeeeeeffffffgggggghh");
        let akia_a = format!("{}IOSFODNN7EXAMPLE", "AKIA");
        let akia_b = format!("{}IOSFODNN7EXAMPL2", "AKIA");

        let code_lines = vec![
            "let capsule_id = \"cap_1\";".to_string(),
            "let capsule_routed = true;".to_string(),
            format!("let pair_space = \"{cap_hex_a} {cap_hex_b}\";"),
            format!("let pair_comma = \"{cap_hex_a},{cap_hex_b}\";"),
            format!("let pair_semi = \"{cap_hex_a};{cap_hex_b}\";"),
            format!("let pair_eq = \"{cap_hex_a}={cap_hex_b}\";"),
            format!("let pair_quote = \"{cap_hex_a}\"{cap_hex_b}\";"),
            format!("let uuid_space = \"{cap_uuid_a} {cap_uuid_b}\";"),
            format!("let uuid_comma = \"{cap_uuid_a},{cap_uuid_b}\";"),
            format!("let cbiq_space = \"{cbiq_a} {cbiq_b}\";"),
            format!("let cbiq_comma = \"{cbiq_a},{cbiq_b}\";"),
            format!("let cbst_comma = \"{cbst_a},{cbst_b}\";"),
            format!("let stripe_space = \"{stripe_a} {stripe_b}\";"),
            format!("let stripe_comma = \"{stripe_a},{stripe_b}\";"),
            format!("let ghp_comma = \"{ghp_a},{ghp_b}\";"),
            format!("let akia_space = \"{akia_a} {akia_b}\";"),
            format!("let akia_under = \"{akia_a}_{akia_b}\";"),
            format!("let mixed = \"{cap_hex_a},{cbiq_a};{cap_uuid_b}={stripe_a}\";"),
            format!("let mixed_eq = \"{stripe_a}={cap_uuid_b}\";"),
        ];

        let scrubbed = scrub_code_lines("src/secrets.rs", &code_lines);
        assert_eq!(scrubbed.len(), code_lines.len());
        assert_eq!(scrubbed[0], "let capsule_id = \"cap_1\";");
        assert_eq!(scrubbed[1], "let capsule_routed = true;");
        assert_eq!(scrubbed[2], "let pair_space = \"[REDACTED] [REDACTED]\";");
        assert_eq!(scrubbed[3], "let pair_comma = \"[REDACTED],[REDACTED]\";");
        assert_eq!(scrubbed[4], "let pair_semi = \"[REDACTED];[REDACTED]\";");
        assert_eq!(scrubbed[5], "let pair_eq = \"[REDACTED]=[REDACTED]\";");
        assert_eq!(scrubbed[6], "let pair_quote = \"[REDACTED]\"[REDACTED]\";");
        assert_eq!(scrubbed[7], "let uuid_space = \"[REDACTED] [REDACTED]\";");
        assert_eq!(scrubbed[8], "let uuid_comma = \"[REDACTED],[REDACTED]\";");
        assert_eq!(scrubbed[9], "let cbiq_space = \"[REDACTED] [REDACTED]\";");
        assert_eq!(scrubbed[10], "let cbiq_comma = \"[REDACTED],[REDACTED]\";");
        assert_eq!(scrubbed[11], "let cbst_comma = \"[REDACTED],[REDACTED]\";");
        assert_eq!(
            scrubbed[12],
            "let stripe_space = \"[REDACTED] [REDACTED]\";"
        );
        assert_eq!(
            scrubbed[13],
            "let stripe_comma = \"[REDACTED],[REDACTED]\";"
        );
        assert_eq!(scrubbed[14], "let ghp_comma = \"[REDACTED],[REDACTED]\";");
        assert_eq!(scrubbed[15], "let akia_space = \"[REDACTED] [REDACTED]\";");
        assert_eq!(scrubbed[16], "let akia_under = \"[REDACTED]_[REDACTED]\";");
        assert_eq!(
            scrubbed[17],
            "let mixed = \"[REDACTED],[REDACTED];[REDACTED]=[REDACTED]\";"
        );
        assert_eq!(scrubbed[18], "let mixed_eq = \"[REDACTED]\";");

        let prose_in = format!(
            "capsule_id capsule_routed {cap_hex_a} {cap_hex_b} {cap_uuid_a},{cap_uuid_b} {cbiq_a},{cbiq_b} {stripe_a} {stripe_b}"
        );
        let prose_out = scrub_prose(&prose_in);
        assert_eq!(
            prose_out,
            "capsule_id capsule_routed [REDACTED] [REDACTED] [REDACTED],[REDACTED] [REDACTED],[REDACTED] [REDACTED] [REDACTED]"
        );
    }

    #[test]
    fn split_diff_sections_avoids_suffix_collision_across_pass1_and_pass2() {
        let entries = vec![
            RawDiffEntry {
                path: "aaa.txt".to_string(),
                old_path: None,
                status: ReviewFileStatus::Modified,
                is_submodule: false,
            },
            RawDiffEntry {
                path: "zzz b/aaa.txt".to_string(),
                old_path: None,
                status: ReviewFileStatus::Added,
                is_submodule: false,
            },
        ];

        // Pass 2 copy-like header for `zzz b/aaa.txt` appears BEFORE Pass 1 exact `aaa.txt` header.
        let diff = b"diff --git a/src.txt b/zzz b/aaa.txt\n@@ -1 +1 @@\n+zzz\n\
diff --git a/aaa.txt b/aaa.txt\n@@ -1 +1 @@\n-old\n+aaa\n";
        let sections = split_diff_sections(diff, &entries);
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].0, Some(1));
        assert!(std::str::from_utf8(sections[0].1).unwrap().contains("+zzz"));
        assert_eq!(sections[1].0, Some(0));
        assert!(std::str::from_utf8(sections[1].1).unwrap().contains("+aaa"));

        // Neither header matches Pass 1 exact; longest-path preference still assigns `zzz b/aaa.txt`.
        let fallback_only = b"diff --git a/src_z.txt b/zzz b/aaa.txt\n@@ -1 +1 @@\n+zzz\n\
diff --git a/src_a.txt b/aaa.txt\n@@ -1 +1 @@\n+aaa\n";
        let sections2 = split_diff_sections(fallback_only, &entries);
        assert_eq!(sections2.len(), 2);
        assert_eq!(sections2[0].0, Some(1));
        assert_eq!(sections2[1].0, Some(0));
    }

    #[tokio::test]
    async fn throwaway_git_repo_preserves_embedded_b_prefix_paths_and_renames() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        git_sync(root, &["init", "-b", "main"]);
        std::fs::create_dir_all(root.join("zzz b")).unwrap();
        std::fs::write(root.join("aaa.txt"), "orig aaa\n").unwrap();
        std::fs::write(root.join("zzz b/aaa.txt"), "orig zzz\n").unwrap();
        std::fs::write(
            root.join("rename_src.txt"),
            "line 1\nline 2\nline 3\nline 4\nold marker\n",
        )
        .unwrap();
        git_sync(root, &["add", "."]);
        git_sync(root, &["commit", "-m", "base"]);
        let base_oid = git_sync(root, &["rev-parse", "HEAD"]);

        std::fs::write(root.join("aaa.txt"), "mod aaa\n").unwrap();
        std::fs::write(root.join("zzz b/aaa.txt"), "mod zzz\n").unwrap();
        std::fs::create_dir_all(root.join("other b")).unwrap();
        std::fs::remove_file(root.join("rename_src.txt")).unwrap();
        std::fs::write(
            root.join("other b/aaa.txt"),
            "line 1\nline 2\nline 3\nline 4\nnew marker\n",
        )
        .unwrap();
        git_sync(root, &["add", "."]);
        git_sync(root, &["commit", "-m", "head"]);
        let head_oid = git_sync(root, &["rev-parse", "HEAD"]);

        let (req, _) = build_review_capsule_request(
            ReviewToolInput {
                base: base_oid.clone(),
                head: Some(head_oid.clone()),
                two_dot: Some(false),
                task: "Verify paths containing ' b/' do not collide".to_string(),
                ..Default::default()
            },
            Uuid::new_v4(),
            None,
            None,
            Some(root.to_str().unwrap()),
        )
        .await
        .expect("packaged review capsule");

        let by_path: HashMap<&str, &ReviewFileInput> = req
            .review
            .files
            .iter()
            .map(|f| (f.path.as_str(), f))
            .collect();

        let aaa = by_path["aaa.txt"];
        assert_eq!(aaa.skipped, None);
        assert_eq!(aaa.hunks.len(), 1);
        assert_eq!(aaa.hunks[0].lines, vec!["-orig aaa", "+mod aaa"]);

        let zzz = by_path["zzz b/aaa.txt"];
        assert_eq!(zzz.skipped, None);
        assert_eq!(zzz.hunks.len(), 1);
        assert_eq!(zzz.hunks[0].lines, vec!["-orig zzz", "+mod zzz"]);

        let renamed = by_path["other b/aaa.txt"];
        assert_eq!(renamed.status, ReviewFileStatus::Renamed);
        assert_eq!(renamed.old_path.as_deref(), Some("rename_src.txt"));
        assert_eq!(renamed.skipped, None);
        assert_eq!(renamed.hunks.len(), 1);
        assert!(renamed.hunks[0].lines.iter().any(|l| l == "+new marker"));

        // Reverse / swap rename variation: rename `zzz b/aaa.txt` -> `zzz b/bbb.txt`
        // while modifying `aaa.txt` and adding a fresh `zzz b/aaa.txt`.
        let swap_base = head_oid;
        std::fs::write(root.join("aaa.txt"), "mod aaa 2\n").unwrap();
        std::fs::rename(root.join("zzz b/aaa.txt"), root.join("zzz b/bbb.txt")).unwrap();
        std::fs::write(root.join("zzz b/bbb.txt"), "mod zzz renamed\n").unwrap();
        git_sync(root, &["add", "."]);
        git_sync(root, &["commit", "-m", "swap"]);
        let swap_head = git_sync(root, &["rev-parse", "HEAD"]);

        let (req2, _) = build_review_capsule_request(
            ReviewToolInput {
                base: swap_base,
                head: Some(swap_head),
                two_dot: Some(false),
                task: "Verify reverse rename with embedded ' b/'".to_string(),
                ..Default::default()
            },
            Uuid::new_v4(),
            None,
            None,
            Some(root.to_str().unwrap()),
        )
        .await
        .expect("packaged reverse rename capsule");

        let by_path2: HashMap<&str, &ReviewFileInput> = req2
            .review
            .files
            .iter()
            .map(|f| (f.path.as_str(), f))
            .collect();
        assert_eq!(by_path2["aaa.txt"].skipped, None);
        assert_eq!(
            by_path2["aaa.txt"].hunks[0].lines,
            vec!["-mod aaa", "+mod aaa 2"]
        );
        assert_eq!(by_path2["zzz b/bbb.txt"].skipped, None);
        assert!(by_path2["zzz b/bbb.txt"].hunks[0]
            .lines
            .iter()
            .any(|l| l == "+mod zzz renamed"));
    }
}
