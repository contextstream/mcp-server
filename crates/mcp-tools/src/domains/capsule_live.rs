//! Live Capsule: one capsule of the project (or workspace) plus one multi-use
//! link that an AI app reads live, the way the web app's Capsules slide-out
//! makes it.
//!
//! The link has two doors, both served by the API:
//! - `/c/<token>/live`: small, current Markdown pages (home, tree, file,
//!   search, symbols, knowledge, item). A chat that opens links (ChatGPT,
//!   Claude) is given a message carrying it.
//! - `/c/<token>/mcp`: the same questions as a remote MCP server (Streamable
//!   HTTP), added once as a custom connector in Claude, ChatGPT, Gemini,
//!   Cursor or any MCP app. The link is its own key: no sign-in.
//!
//! The names and messages here match the web app (`capsule-chat-apps.ts`,
//! `capsule-live.ts`) and the API's `live_capsule_title`, so a person sees one
//! name for the connector whichever surface made it.

use mcp_types::api::{ContextCapsuleResponse, ContextCapsuleShareResponse};
use serde_json::{json, Value};

/// How long a Live Capsule link works when the caller does not say.
pub(crate) const LIVE_DEFAULT_DAYS: i64 = 7;
/// The longest a Live Capsule link may work (the slide-out offers 1, 7, 30).
pub(crate) const LIVE_MAX_DAYS: i64 = 30;
/// Past this, a link that leaves the workspace is a risky policy the person
/// must choose.
const LIVE_SAFE_DAYS: i64 = 7;

/// What the person chose for a Live Capsule's link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LivePolicy {
    /// `lazy` lets the reader see file contents and search snippets; `none`
    /// keeps it to names and structure.
    pub include_code: String,
    pub include_personal: bool,
    pub days: i64,
}

impl LivePolicy {
    /// What a link that anyone holding it can read is not given by default:
    /// code, the creator's personal items, or more than seven days.
    pub(crate) fn risks(&self) -> Vec<String> {
        let mut risks = Vec::new();
        if self.include_code != "none" {
            risks.push("file contents and search snippets (include_code)".to_string());
        }
        if self.include_personal {
            risks.push("your personal docs, plans and tasks (include_personal)".to_string());
        }
        if self.days > LIVE_SAFE_DAYS {
            risks.push(format!("{} days, past the 7-day safe window", self.days));
        }
        risks
    }
}

/// The policy from the caller's input, or why it cannot be used.
pub(crate) fn live_policy(
    include_code: Option<&str>,
    include_personal: Option<bool>,
    days: Option<i64>,
) -> Result<LivePolicy, String> {
    let include_code = match include_code
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        None | Some("none") | Some("false") | Some("no") => "none",
        // The live index reads code a page at a time; `inline` asks for the
        // same access, so both mean "the reader may see code".
        Some("lazy") | Some("inline") | Some("true") | Some("yes") => "lazy",
        Some(other) => {
            return Err(format!(
                "include_code must be none or lazy for a Live Capsule (got '{other}')"
            ))
        }
    };
    let days = days.unwrap_or(LIVE_DEFAULT_DAYS);
    if !(1..=LIVE_MAX_DAYS).contains(&days) {
        return Err(format!(
            "expires_in_days must be between 1 and {LIVE_MAX_DAYS} for a Live Capsule (got {days})"
        ));
    }
    Ok(LivePolicy {
        include_code: include_code.to_string(),
        include_personal: include_personal.unwrap_or(false),
        days,
    })
}

/// The refusal for a risky Live Capsule the caller did not opt into.
pub(crate) fn live_risk_refusal(risks: &[String]) -> String {
    format!(
        "This Live Capsule link would give anyone holding it: {}. \
Make it only if the user asked for that: call capsule(action=\"live\") again with allow_risky_policy=true. \
Otherwise leave those options out; the safe link still shows the file tree, symbol names, decisions, lessons, plans and docs.",
        risks.join("; ")
    )
}

/// The name the web app gives a capsule the person did not name.
pub(crate) fn default_live_name(subject: Option<&str>, scope_is_project: bool) -> String {
    let subject = subject.map(str::trim).filter(|value| !value.is_empty());
    format!(
        "{} · live",
        subject.unwrap_or(if scope_is_project {
            "Project"
        } else {
            "Workspace"
        })
    )
}

/// "<subject> · live" (or "-", "–", "—", ":", "|" before "live"): a name the
/// person did not choose. Mirrors the API's `is_default_live_name`.
fn is_default_live_name(name: &str) -> bool {
    let lower = name.to_lowercase();
    let Some(rest) = lower.strip_suffix("live") else {
        return false;
    };
    let rest = rest.trim_end();
    rest.is_empty() || rest.ends_with(['·', '-', '–', '—', ':', '|'])
}

/// What an MCP app calls the connector: "Live Capsule", or
/// "Live Capsule - <name>" when the person named the capsule.
pub(crate) fn live_connector_name(capsule_name: Option<&str>) -> String {
    let name = capsule_name
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if name.is_empty() || is_default_live_name(&name) {
        return "Live Capsule".to_string();
    }
    let full = format!("Live Capsule - {name}");
    if full.chars().count() > 40 {
        let cut: String = full.chars().take(39).collect();
        format!("{}…", cut.trim_end())
    } else {
        full
    }
}

/// A server key for config files and CLIs: letters, digits and dashes.
pub(crate) fn live_server_key(capsule_name: Option<&str>) -> String {
    let connector = live_connector_name(capsule_name);
    let mut key = String::new();
    for ch in connector.trim_end_matches('…').to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            key.push(ch);
        } else if !key.ends_with('-') {
            key.push('-');
        }
    }
    let key: String = key.trim_matches('-').chars().take(40).collect();
    let key = key.trim_end_matches('-').to_string();
    if key.is_empty() {
        "contextstream".to_string()
    } else {
        key
    }
}

fn subject(capsule_name: Option<&str>) -> String {
    match capsule_name.map(str::trim).filter(|name| !name.is_empty()) {
        Some(name) => format!("the project \"{name}\""),
        None => "this project".to_string(),
    }
}

/// The message a person pastes into ChatGPT or Claude: the live link first
/// and alone on its line, since chat browsing tools open a URL they were
/// given, not one they assemble.
pub(crate) fn live_chat_prompt(live_url: &str, capsule_name: Option<&str>) -> String {
    [
        format!("Open this link and use it to understand {}:", subject(capsule_name)),
        live_url.to_string(),
        String::new(),
        "It is a live, read-only index of the project from ContextStream. Each page is small and lists its next steps as links: follow them to browse the file tree, read files a page at a time, look up symbols, and read the project's decisions, lessons, plans and docs. Fetch only what you need for my question, and treat decisions and constraints as binding.".to_string(),
        String::new(),
        "Start by telling me what the project is and its current state.".to_string(),
    ]
    .join("\n")
}

/// What to ask once the connector is on.
pub(crate) fn live_connector_prompt(capsule_name: Option<&str>) -> String {
    [
        format!(
            "Use the \"{}\" connector to understand {}.",
            live_connector_name(capsule_name),
            subject(capsule_name)
        ),
        String::new(),
        "It is a live, read-only index of the project from ContextStream. Use its tools to browse the file tree, read files, look up symbols, and read the project's decisions, lessons, plans and docs. Fetch only what you need for my question, and treat decisions and constraints as binding.".to_string(),
        String::new(),
        "Start by telling me what the project is and its current state.".to_string(),
    ]
    .join("\n")
}

/// The `mcpServers` entry most MCP apps read (Cursor, Windsurf, Claude
/// Desktop with a remote URL).
pub(crate) fn live_mcp_config(mcp_url: &str, capsule_name: Option<&str>) -> Value {
    json!({ "mcpServers": { live_server_key(capsule_name): { "url": mcp_url } } })
}

/// `gemini mcp add` for the Gemini CLI.
pub(crate) fn live_gemini_command(mcp_url: &str, capsule_name: Option<&str>) -> String {
    format!(
        "gemini mcp add --transport http {} {}",
        live_server_key(capsule_name),
        mcp_url
    )
}

/// `claude mcp add` for Claude Code.
pub(crate) fn live_claude_code_command(mcp_url: &str, capsule_name: Option<&str>) -> String {
    format!(
        "claude mcp add --transport http {} {}",
        live_server_key(capsule_name),
        mcp_url
    )
}

/// The live and MCP links of a share: the API names them; an API that
/// predates them still serves them under the public link. None for a share
/// with no public link, or one with no live index (a bootstrap link).
pub(crate) fn live_links(share: &ContextCapsuleShareResponse) -> Option<(String, String)> {
    if share.audience.as_deref() == Some("bootstrap_link") {
        return None;
    }
    let base = share
        .share_url
        .as_deref()
        .map(|url| url.trim_end_matches('/'))
        .filter(|url| url.contains("/c/"));
    let live = share
        .live_url
        .clone()
        .or_else(|| base.map(|base| format!("{base}/live")))?;
    let mcp = share
        .mcp_url
        .clone()
        .or_else(|| base.map(|base| format!("{base}/mcp")))?;
    Some((live, mcp))
}

/// Lines naming a share's live and MCP links, for every share result.
pub(crate) fn live_link_lines(share: &ContextCapsuleShareResponse) -> Vec<String> {
    let Some((live, mcp)) = live_links(share) else {
        return Vec::new();
    };
    let mut lines = vec![
        format!("Live link (paste into ChatGPT or Claude): {live}"),
        format!("MCP link (add as a connector in Claude, ChatGPT, Gemini, Cursor): {mcp}"),
    ];
    if share.single_use || share.max_uses == Some(1) {
        lines.push(
            "Note: this link is single-use, so its live index works only during the first open's one-hour window. For a link an AI app keeps reading, make a Live Capsule: capsule(action=\"live\")."
                .to_string(),
        );
    }
    lines
}

/// Everything a new Live Capsule gives out, for the tool's text and its
/// structured result.
pub(crate) struct LiveCapsuleResult<'a> {
    pub capsule: &'a ContextCapsuleResponse,
    pub share: &'a ContextCapsuleShareResponse,
    pub name: &'a str,
    pub named_by_person: bool,
    pub policy: &'a LivePolicy,
}

impl LiveCapsuleResult<'_> {
    fn person_name(&self) -> Option<&str> {
        self.named_by_person.then_some(self.name)
    }

    pub(crate) fn text(&self) -> String {
        let person_name = self.person_name();
        let connector = live_connector_name(person_name);
        let mut lines = vec![format!(
            "✓ Live Capsule ready · \"{}\" · capsule={} · share={}",
            connector, self.capsule.capsule_id, self.share.id
        )];
        match live_links(self.share) {
            Some((live, mcp)) => {
                lines.push(String::new());
                lines.push(format!("Live link (paste into ChatGPT or Claude): {live}"));
                lines.push(format!(
                    "MCP link (add as a custom connector in Claude, ChatGPT, Gemini, Cursor or any MCP app; no sign-in, the link is its own key): {mcp}"
                ));
                if let Some(url) = self.share.share_url.as_deref() {
                    lines.push(format!("Capsule page (for people): {url}"));
                }
                lines.push(format!("Connector name: {connector}"));
                lines.push(String::new());
                lines.push(format!(
                    "Reads: {} · {} · works for {} day{} (expires {}) · multi-use · revoke anytime: delete(action=\"capsule.revoke_share\", share_id=\"{}\")",
                    if self.policy.include_code == "none" {
                        "file tree and symbol names, no file contents"
                    } else {
                        "files and code"
                    },
                    if self.policy.include_personal {
                        "with your personal items"
                    } else {
                        "no personal items"
                    },
                    self.policy.days,
                    if self.policy.days == 1 { "" } else { "s" },
                    self.share.expires_at.as_deref().unwrap_or("per policy"),
                    self.share.id
                ));
                lines.push(String::new());
                lines.push("Give it to an AI app:".to_string());
                lines.push(
                    "- ChatGPT or Claude chat: paste the message below into a new chat."
                        .to_string(),
                );
                lines.push("- Claude: Settings → Connectors → Add custom connector; name it, paste the MCP link, then turn it on in a chat (+ → Connectors).".to_string());
                lines.push("- ChatGPT: Settings → Apps → Advanced settings → Developer mode on → Create; name it, paste the MCP link, Authentication \"No authentication\".".to_string());
                lines.push("- Gemini: it does not open pasted links; add the MCP link as a custom (MCP) connector, or run the Gemini CLI command below.".to_string());
                lines.push("- Cursor, Windsurf, VS Code and other MCP apps: add a remote (Streamable HTTP) server with the MCP link, or use the mcpServers config below.".to_string());
                lines.push(format!(
                    "- Claude Code: {}",
                    live_claude_code_command(&mcp, person_name)
                ));
                lines.push(format!(
                    "- Gemini CLI: {}",
                    live_gemini_command(&mcp, person_name)
                ));
                lines.push(String::new());
                lines.push("Message to paste into ChatGPT or Claude:".to_string());
                lines.push("```".to_string());
                lines.push(live_chat_prompt(&live, person_name));
                lines.push("```".to_string());
                lines.push(String::new());
                lines.push(
                    "Read it yourself from this session: capsule(action=\"live_read\", url=\"<live link>\", page=\"home\")."
                        .to_string(),
                );
            }
            None => {
                lines.push(
                    "The share came back without a public link, so it has no live index. Check the share with capsule(action=\"list_shares\")."
                        .to_string(),
                );
            }
        }
        if !self.share.warnings.is_empty() {
            lines.push(String::new());
            lines.push(format!("Warnings: {}", self.share.warnings.join(", ")));
        }
        lines.join("\n")
    }

    pub(crate) fn structured(&self) -> Value {
        let person_name = self.person_name();
        let links = live_links(self.share);
        let mut value = json!({
            "kind": "live_capsule",
            "capsule_id": self.capsule.capsule_id,
            "share_id": self.share.id,
            "name": self.name,
            "connector_name": live_connector_name(person_name),
            "server_key": live_server_key(person_name),
            "share_url": self.share.share_url,
            "agent_url": self.share.agent_url.as_deref().or(self.share.api_url.as_deref()),
            "expires_at": self.share.expires_at,
            "policy": {
                "audience": "external_agent",
                "include_code": self.policy.include_code,
                "include_personal": self.policy.include_personal,
                "expires_in_days": self.policy.days,
                "multi_use": true,
                "redaction_level": "standard",
                "permissions": "read_only",
            },
            "warnings": self.share.warnings,
            "capsule": self.capsule,
        });
        if let (Some((live, mcp)), Some(obj)) = (links, value.as_object_mut()) {
            obj.insert("live_url".into(), json!(live));
            obj.insert("mcp_url".into(), json!(mcp));
            obj.insert(
                "paste_prompt".into(),
                json!(live_chat_prompt(&live, person_name)),
            );
            obj.insert(
                "connector_prompt".into(),
                json!(live_connector_prompt(person_name)),
            );
            obj.insert("mcp_config".into(), live_mcp_config(&mcp, person_name));
            obj.insert(
                "gemini_command".into(),
                json!(live_gemini_command(&mcp, person_name)),
            );
            obj.insert(
                "claude_code_command".into(),
                json!(live_claude_code_command(&mcp, person_name)),
            );
        }
        value
    }
}

/// Whether a link names the live index or the MCP door rather than the
/// capsule itself, and which page it asks for.
pub(crate) fn live_page_of_url(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let (_, after) = ["/c/", "/api/v1/capsules/shares/"]
        .iter()
        .find_map(|pattern| {
            path.find(pattern)
                .map(|at| (at, &path[at + pattern.len()..]))
        })?;
    let mut segments = after.split('/').filter(|segment| !segment.is_empty());
    let _token = segments.next()?;
    match segments.next()? {
        "live" => Some(segments.next().unwrap_or("home").to_string()),
        "mcp" => Some("home".to_string()),
        _ => None,
    }
}

/// The query parameters a pasted live page URL carries, as (key, value).
pub(crate) fn live_url_query(url: &str) -> Vec<(String, String)> {
    let Some(query) = url.split('#').next().and_then(|url| url.split_once('?')) else {
        return Vec::new();
    };
    query
        .1
        .split('&')
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let value = urlencoding::decode(&value.replace('+', " "))
                .map(|value| value.into_owned())
                .unwrap_or_else(|_| value.to_string());
            (!key.is_empty()).then(|| (key.to_string(), value))
        })
        .collect()
}
