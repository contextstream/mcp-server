//! Command hints: how a tool the API defines says which shell commands it can
//! run, so that an editor hook can send those commands to it.
//!
//! A definition may carry
//! `"command_hints": {"argument": "command", "cwd_argument": "cwd", "patterns": ["^…", …]}`:
//! the name of the argument that takes the command, the name of the argument
//! that takes a folder (optional), and regular expressions (Rust `regex`
//! syntax, case-sensitive) of the commands the tool takes. A hint is only a
//! hint: the service may still decline, and the command then runs where it
//! would have run.
//!
//! The server writes the hints it read to a small file under `~/.contextstream`
//! for the editor hook, which must answer without a network call. The file is
//! tied to the account (a digest of its credential, never the credential) and
//! the API address, and is not used when it is old.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// The start of the name of the file the hints are kept in, under `~/.contextstream`;
/// each account has its own (`hosted-command-hints-<its digest>.json`), so that
/// two accounts on one machine do not overwrite each other's.
pub const CACHE_FILE: &str = "hosted-command-hints";

/// How long a written file is used for.
pub const CACHE_MAX_AGE: Duration = Duration::from_secs(12 * 3600);

/// Set to `off` to leave every command as it is.
pub const ENV_SWITCH: &str = "CONTEXTSTREAM_HOSTED_TOOLS";

const CACHE_VERSION: u32 = 1;
const MAX_PATTERNS: usize = 64;
const MAX_PATTERN_LEN: usize = 512;
/// Commands longer than this are never matched.
const MAX_COMMAND_LEN: usize = 8192;
const MAX_TOOLS: usize = 16;

/// Commands people type all day. A tool whose hints take any of them is taking
/// far more than a kind of job, so its hints are not used at all.
const EVERYDAY_COMMANDS: [&str; 16] = [
    "ls",
    "ls -la",
    "pwd",
    "cd ..",
    "cd src",
    "git status",
    "git diff",
    "git log",
    "cat README.md",
    "echo hello",
    "true",
    "whoami",
    "env",
    "which cargo",
    "mkdir build",
    "rm -rf build",
];

/// Whether the value of [`ENV_SWITCH`] turns the feature off.
pub fn switched_off(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        matches!(
            value.trim().to_ascii_lowercase().as_str(),
            "off" | "0" | "false" | "no" | "disabled"
        )
    })
}

/// The identity a local caller's cached reads are kept under: a digest of its
/// credential, never the credential.
pub fn local_identity(credential: &str) -> String {
    format!(
        "local:{}",
        &hex::encode(Sha256::digest(credential.as_bytes()))[..16]
    )
}

/// What a tool says of the commands it takes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hints {
    /// The argument that takes the command.
    pub argument: String,
    /// The argument that takes a folder inside the repository, if the tool has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd_argument: Option<String>,
    pub patterns: Vec<String>,
}

fn argument_name(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|name| {
            !name.is_empty()
                && name.len() <= 64
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
        .map(str::to_string)
}

impl Hints {
    /// The hints in a tool definition, when it has usable ones: a plain
    /// argument name and at least one pattern that compiles.
    pub fn from_definition(definition: &Value) -> Option<Hints> {
        let hints = definition.get("command_hints")?;
        let argument = argument_name(hints.get("argument"))?;
        let cwd_argument = argument_name(hints.get("cwd_argument"));
        let patterns: Vec<String> = hints
            .get("patterns")?
            .as_array()?
            .iter()
            .filter_map(Value::as_str)
            .filter(|pattern| !pattern.is_empty() && pattern.len() <= MAX_PATTERN_LEN)
            .filter(|pattern| compile(pattern).is_some())
            .take(MAX_PATTERNS)
            .map(str::to_string)
            .collect();
        if patterns.is_empty() {
            return None;
        }
        let hints = Hints {
            argument,
            cwd_argument,
            patterns,
        };
        if EVERYDAY_COMMANDS
            .iter()
            .any(|command| hints.matches(command))
        {
            return None;
        }
        Some(hints)
    }

    /// Whether `command` (as the caller typed it, trimmed here) is one the tool takes.
    pub fn matches(&self, command: &str) -> bool {
        let command = command.trim();
        if command.is_empty() || command.len() > MAX_COMMAND_LEN {
            return false;
        }
        self.patterns
            .iter()
            .take(MAX_PATTERNS)
            .filter_map(|pattern| compile(pattern))
            .any(|pattern| pattern.is_match(command))
    }
}

fn compile(pattern: &str) -> Option<regex::Regex> {
    regex::RegexBuilder::new(pattern)
        .size_limit(1 << 20)
        .build()
        .ok()
}

/// One tool, as much of it as the hook and the run command need.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedTool {
    pub name: String,
    /// `"api"` or `"stream"`.
    pub transport: String,
    #[serde(default)]
    pub attachments: Vec<String>,
    pub hints: Hints,
}

/// The tools with command hints that the API defines for one caller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cache {
    pub version: u32,
    pub identity: String,
    pub api_url: String,
    /// Seconds since the epoch.
    pub written_at: u64,
    pub tools: Vec<CachedTool>,
}

impl Cache {
    pub fn new(identity: &str, api_url: &str, tools: Vec<CachedTool>) -> Self {
        Self {
            version: CACHE_VERSION,
            identity: identity.to_string(),
            api_url: normalized_url(api_url),
            written_at: now_secs(),
            tools: tools.into_iter().take(MAX_TOOLS).collect(),
        }
    }

    /// The first tool that takes `command`.
    pub fn tool_for(&self, command: &str) -> Option<&CachedTool> {
        self.tools.iter().find(|tool| tool.hints.matches(command))
    }

    /// The tool called `name`.
    pub fn tool_named(&self, name: &str) -> Option<&CachedTool> {
        self.tools.iter().find(|tool| tool.name == name)
    }
}

fn normalized_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_ascii_lowercase()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Where the file for `identity` lives.
pub fn cache_path(identity: &str) -> Option<PathBuf> {
    let digest: String = identity
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(32)
        .collect();
    dirs::home_dir().map(|home| {
        home.join(".contextstream")
            .join(format!("{CACHE_FILE}-{digest}.json"))
    })
}

/// Write `cache` to `path` unless the file already says the same (ignoring
/// when it was written). Returns whether it wrote. A symlink at `path` is
/// never written through.
pub fn write_to(path: &Path, cache: &Cache) -> std::io::Result<bool> {
    if let Ok(existing) = std::fs::read_to_string(path) {
        if let Ok(old) = serde_json::from_str::<Cache>(&existing) {
            let same = old.identity == cache.identity
                && old.api_url == cache.api_url
                && old.tools == cache.tools
                && old.version == cache.version;
            // A file nearly as old as it may be is written again, so a live session keeps it fresh.
            let fresh = now_secs().saturating_sub(old.written_at) < CACHE_MAX_AGE.as_secs() / 4;
            if same && fresh {
                return Ok(false);
            }
        }
    }
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        if metadata.file_type().is_symlink() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "the hints file is a symlink",
            ));
        }
    }
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "no folder"))?;
    std::fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    std::io::Write::write_all(
        &mut temporary,
        serde_json::to_string(cache)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?
            .as_bytes(),
    )?;
    temporary
        .persist(path)
        .map_err(|error| error.error)
        .map(|_| true)
}

/// The hints written for this caller and API, when they are not too old.
pub fn read_from(path: &Path, identity: &str, api_url: &str) -> Option<Cache> {
    let metadata = std::fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > 1 << 20 {
        return None;
    }
    let cache: Cache = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    let age = now_secs().saturating_sub(cache.written_at);
    (cache.version == CACHE_VERSION
        && cache.identity == identity
        && cache.api_url == normalized_url(api_url)
        && age < CACHE_MAX_AGE.as_secs())
    .then_some(cache)
}

/// [`write_to`] the default path for the cache's account.
pub fn write(cache: &Cache) -> std::io::Result<bool> {
    let path = cache_path(&cache.identity)
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no home folder"))?;
    write_to(&path, cache)
}

/// [`read_from`] the default path for `identity`.
pub fn read(identity: &str, api_url: &str) -> Option<Cache> {
    read_from(&cache_path(identity)?, identity, api_url)
}

/// Quote `text` as one word for a POSIX shell.
pub fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

#[cfg(test)]
#[path = "command_hints_tests.rs"]
mod tests;
