//! Live Capsule intent for prompt hooks: the user asks for a Live Capsule (a
//! link ChatGPT, Claude, Gemini or another AI app reads live) or pastes one.
//!
//! Narrow on purpose: talk *about* capsules while building them ("the live
//! capsule endpoint returns 410") must not tell the agent to make one.

/// Guidance when the user asks for a Live Capsule.
pub const LIVE_CAPSULE_GUIDANCE: &str = r#"[CONTEXTSTREAM LIVE CAPSULE]
The user wants an AI app (ChatGPT, Claude, Gemini, Cursor, ...) or someone without ContextStream to read the project live. Make a Live Capsule in one call:

```
mcp__contextstream__capsule(action="live", name="<short name, optional>")
```

- Give the user what it returns: the live link (paste into ChatGPT or Claude), the MCP link (add as a custom connector in Claude, ChatGPT, Gemini, Cursor or any MCP app; no sign-in), the connector name, and the paste-ready message. Gemini does not open pasted links: it takes the MCP link as a connector.
- Defaults are safe: no file contents, no personal items, 7 days, read-only. Pass include_code="lazy", include_personal=true or expires_in_days up to 30 only if the user asked, with allow_risky_policy=true.
- Revoke with mcp__contextstream__delete(action="capsule.revoke_share", share_id="<share id>").
[END GUIDANCE]"#;

/// Guidance when the user pastes a Live Capsule link.
pub const LIVE_LINK_GUIDANCE: &str = r#"[CONTEXTSTREAM LIVE CAPSULE LINK]
The message has a Live Capsule link (/c/<token>/live or /c/<token>/mcp): a live, read-only index of a project. Read it with:

```
mcp__contextstream__capsule(action="live_read", url="<the link>")
```

- Start at the home page, then follow the links each page lists (pass any of them back as url), or ask directly: page="tree" (path), "file" (path, start_line, end_line), "search" (query), "symbols" (query), "knowledge" (kind=decisions|constraints|lessons|plans|tasks|docs), "item" (item_id).
- Fetch only what the question needs; treat the capsule's decisions and constraints as binding.
[END GUIDANCE]"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveCapsuleIntent {
    /// The user pasted a `/c/<token>/live` or `/c/<token>/mcp` link.
    PastedLink,
    /// The user asked for a Live Capsule.
    Create,
}

/// Asking for one, in words a person uses.
const CREATE_PHRASES: &[&str] = &[
    "make a live capsule",
    "make me a live capsule",
    "create a live capsule",
    "create live capsule",
    "new live capsule",
    "give me a live capsule",
    "generate a live capsule",
    "set up a live capsule",
    "share a live capsule",
    "send a live capsule",
    "live capsule for",
    "live capsule link",
    "live capsule so",
];

/// An AI app a person hands the project to.
const AI_APPS: &[&str] = &[
    "chatgpt",
    "gemini",
    "claude.ai",
    "claude desktop",
    "the claude app",
    "another ai",
    "other ai",
];

/// Handing the project to one of them.
const HANDING_OVER: &[&str] = &[
    "share the project with",
    "share this project with",
    "share my project with",
    "share the repo with",
    "share this repo with",
    "give chatgpt",
    "give gemini",
    "give claude",
    "let chatgpt read",
    "let gemini read",
    "let claude read",
    "connector link",
    "mcp link for",
    "connect the project to",
    "connect this project to",
];

/// Whether `prompt` carries a Live Capsule link: `/c/<token>/live` or
/// `/c/<token>/mcp`, or the API's `/capsules/shares/<token>/live`.
fn has_live_link(prompt: &str) -> bool {
    ["/c/", "/capsules/shares/"].iter().any(|pattern| {
        prompt.match_indices(pattern).any(|(at, _)| {
            let rest = &prompt[at + pattern.len()..];
            let mut segments = rest.split('/');
            let token = segments.next().unwrap_or("");
            let looks_like_token =
                (token.starts_with("capsule_") || token.starts_with("brain_")) && token.len() > 10;
            let door = segments
                .next()
                .unwrap_or("")
                .split(|c: char| !c.is_ascii_alphanumeric())
                .next()
                .unwrap_or("");
            looks_like_token && matches!(door, "live" | "mcp")
        })
    })
}

pub fn detect_live_capsule_intent(prompt: &str) -> Option<LiveCapsuleIntent> {
    if has_live_link(prompt) {
        return Some(LiveCapsuleIntent::PastedLink);
    }
    let lower = prompt.to_lowercase();
    if CREATE_PHRASES.iter().any(|phrase| lower.contains(phrase)) {
        return Some(LiveCapsuleIntent::Create);
    }
    let names_app = AI_APPS.iter().any(|app| lower.contains(app));
    let hands_over = HANDING_OVER.iter().any(|phrase| lower.contains(phrase));
    (names_app && hands_over).then_some(LiveCapsuleIntent::Create)
}

pub fn guidance_for_prompt(prompt: &str) -> Option<&'static str> {
    match detect_live_capsule_intent(prompt)? {
        LiveCapsuleIntent::PastedLink => Some(LIVE_LINK_GUIDANCE),
        LiveCapsuleIntent::Create => Some(LIVE_CAPSULE_GUIDANCE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asking_for_a_live_capsule_or_handing_the_project_to_an_ai_app() {
        for prompt in [
            "Make a live capsule of this project for my designer",
            "can you create a live capsule?",
            "I need a live capsule link to send to Sam",
            "Share the project with ChatGPT so it can answer questions",
            "Give Gemini access: I want a connector link for the repo",
            "let claude read this project from claude.ai",
        ] {
            assert_eq!(
                detect_live_capsule_intent(prompt),
                Some(LiveCapsuleIntent::Create),
                "{prompt}"
            );
            assert_eq!(guidance_for_prompt(prompt), Some(LIVE_CAPSULE_GUIDANCE));
        }
    }

    #[test]
    fn pasting_a_live_or_mcp_link() {
        for prompt in [
            "what does this project do? https://contextstream.io/c/capsule_9f8e7d6c5b4a/live",
            "use https://contextstream.io/c/capsule_9f8e7d6c5b4a/live/file?path=src/main.rs",
            "connector: https://contextstream.io/c/capsule_9f8e7d6c5b4a/mcp",
            "https://api.contextstream.io/api/v1/capsules/shares/capsule_9f8e7d6c5b4a/live/search?q=auth",
        ] {
            assert_eq!(
                detect_live_capsule_intent(prompt),
                Some(LiveCapsuleIntent::PastedLink),
                "{prompt}"
            );
            assert_eq!(guidance_for_prompt(prompt), Some(LIVE_LINK_GUIDANCE));
        }
    }

    #[test]
    fn building_or_discussing_capsules_is_not_a_request_for_one() {
        for prompt in [
            "The live capsule endpoint returns 410 after expiry; fix the handler",
            "MCP 1.0.87 has no Live Capsule code; add live capsule functionality to the mcp",
            "open https://contextstream.io/c/capsule_9f8e7d6c5b4a",
            "route /c/<token>/live in the web app",
            "Ask ChatGPT what it thinks of the error message",
            "Write the Gemini provider adapter",
        ] {
            assert_eq!(detect_live_capsule_intent(prompt), None, "{prompt}");
        }
    }
}
