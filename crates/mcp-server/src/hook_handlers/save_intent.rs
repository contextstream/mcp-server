//! Shared save-intent detection helpers for prompt hooks.

use serde_json::Value;
use std::io::{Read, Seek, SeekFrom};

/// Guidance injected for the more specific handoff intent before the generic
/// save/document routing below can send an agent toward a markdown file.
pub const HANDOFF_INTENT_GUIDANCE: &str = r#"[CONTEXTSTREAM CANONICAL HANDOFF]
The user is asking to hand work to another agent/session. Create the durable ContextStream handoff now:

```
mcp__contextstream__entity(
  kind="handoff",
  action="create",
  body={"title":"...","summary":"...","scope":"...","next_steps":[...]}
)
```

- Preserve verified facts, eliminated hypotheses, branch/commit state, environment gotchas, validation already run, blockers, and ordered next steps.
- Add `to_user_id` only when the recipient is known; omit it rather than inventing a recipient.
- `HANDOFF.md`, a scratch prompt, a generic doc/event, or prose alone is NOT a substitute for the ContextStream handoff.
- If the user explicitly requested a local handoff file, create the ContextStream handoff first and then write that exact file only as an additional artifact.
- If a portable bundle, capsule, or share link is requested, also call `mcp__contextstream__capsule(action="create", scope="session", session_id="<current session id>", purpose="handoff")` and return both results.
- If the user explicitly requested a capsule, a real capsule must be created; never replace it with only an entity or prose.
[END GUIDANCE]"#;

/// Guidance injected when the prompt indicates intent to persist knowledge.
/// One line per kind, so the agent can route without a second lookup.
pub const SAVE_INTENT_GUIDANCE: &str = r#"[CONTEXTSTREAM DOCUMENT STORAGE]
Save durable knowledge to ContextStream, not to local files (no Claude auto-memory files, MEMORY.md, HANDOFF.md, or notes/*.md):
- Preference, "remember", "always/never" -> mcp__contextstream__session(action="remember", content="<the rule and why>")
- Lesson from a mistake or correction -> mcp__contextstream__session(action="capture_lesson", title="...", trigger="...", impact="...", prevention="...")
- Decision -> mcp__contextstream__session(action="capture", event_type="decision", title="...", content="...")
- Doc, runbook, spec, ADR -> mcp__contextstream__memory(action="create_doc", doc_type="runbook|spec|adr|general", title="...", content="...")
- Plan -> mcp__contextstream__session(action="capture_plan", title="...", steps=[...])
- Todo -> mcp__contextstream__memory(action="create_todo", title="...", content="...")
- Ticket or bug -> mcp__contextstream__entity(kind="ticket", action="create", body={"title":"...","kind":"bug|feature|task"})
- Handoff -> mcp__contextstream__entity(kind="handoff", action="create", body={"title":"...","summary":"...","scope":"...","next_steps":[...]})
Write a local file only when the user names its path. Repository runbooks, ADRs and notes are mirrored to ContextStream docs automatically.
[END GUIDANCE]"#;

/// Phrases that signal the user wants something kept for later. Bare words
/// that also describe ordinary coding work ("file", "path", "document",
/// "spec", "record") are deliberately absent: they fired on most prompts.
const SAVE_KEYWORDS: &[&str] = &[
    "save this",
    "save that",
    "save it",
    "save these",
    "save our",
    "save my",
    "store this",
    "store that",
    "store it",
    "capture this",
    "capture that",
    "capture the decision",
    "capture a lesson",
    "write down",
    "write this down",
    "note down",
    "make a note",
    "take a note",
    "remember",
    "don't forget",
    "do not forget",
    "for later reference",
    "for future reference",
    "from now on",
    "going forward",
    "lesson learned",
    "lessons learned",
    "record a decision",
    "record the decision",
    "record this decision",
    "we decided",
    "log a decision",
    "save a runbook",
    "write a runbook",
    "create a runbook",
    "update the runbook",
    "write a design doc",
    "write up a design doc",
    "create a ticket",
    "file a bug",
    "file a ticket",
    "add a todo",
    "add to my todos",
    "add a to-do",
];

/// Wording that asks for a local note-like file.
const LOCAL_FILE_HINTS: &[&str] = &[
    "notes.md",
    "todo.md",
    "decisions.md",
    "lessons.md",
    "memory.md",
    "runbook.md",
    "notes/",
    "into a markdown file",
    "in a markdown file",
    "to a markdown file",
    "local markdown",
];

const HANDOFF_TRIGGERS: &[&str] = &[
    "create a handoff",
    "create handoff",
    "create an agent handoff",
    "prepare a handoff",
    "prepare handoff",
    "prepare the handoff",
    "make a handoff",
    "give me a handoff",
    "write a handoff",
    "write up a handoff",
    "package context for handoff",
    "hand this over",
    "hand this work over",
    "hand it over",
    "hand my work over",
    "hand our work over",
    "hand the work over",
    "hand work over",
    "hand this off",
    "hand this work off",
    "hand it off",
    "hand my work off",
    "hand our work off",
    "hand the work off",
    "hand work off",
    "handoff to another",
    "handoff for another",
    "continue with another agent",
    "continue in another agent",
    "continue this in another session",
    "next agent",
    "fresh agent",
];

const EXPLICIT_LOCAL_HANDOFF_FILE_PHRASES: &[&str] = &[
    "create handoff.md",
    "write handoff.md",
    "save handoff.md",
    "update handoff.md",
    "put this in handoff.md",
    "put the handoff in handoff.md",
    "local handoff file",
    "markdown handoff file",
    "handoff file at",
];

const NEGATED_LOCAL_HANDOFF_FILE_PHRASES: &[&str] = &[
    "do not create handoff.md",
    "don't create handoff.md",
    "dont create handoff.md",
    "never create handoff.md",
    "not this: handoff.md",
    "instead of handoff.md",
    "without handoff.md",
];

const MAX_TRANSCRIPT_TAIL_BYTES: u64 = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoffIntent {
    None,
    Canonical,
    ExplicitLocalFile,
}

/// Bind handoff verbs to the canonical ContextStream object while preserving
/// the user's explicit authority to request an additional repository file.
pub fn detect_handoff_intent(prompt: &str) -> HandoffIntent {
    let prompt = prompt.to_ascii_lowercase();
    let local_file_is_negated = NEGATED_LOCAL_HANDOFF_FILE_PHRASES
        .iter()
        .any(|phrase| prompt.contains(phrase));
    let explicit_local_file = !local_file_is_negated
        && EXPLICIT_LOCAL_HANDOFF_FILE_PHRASES
            .iter()
            .any(|phrase| prompt.contains(phrase));
    if explicit_local_file {
        return HandoffIntent::ExplicitLocalFile;
    }

    let has_handoff = HANDOFF_TRIGGERS
        .iter()
        .any(|phrase| prompt.contains(phrase));
    if has_handoff {
        HandoffIntent::Canonical
    } else {
        HandoffIntent::None
    }
}

/// Return user prompt text from a hook payload if present.
pub fn extract_user_prompt(input: &Value) -> Option<String> {
    if let Some(prompt) = input
        .get("prompt")
        .or_else(|| input.get("user_message"))
        .or_else(|| input.get("message"))
        .and_then(|value| value.as_str())
    {
        let prompt = prompt.trim();
        if !prompt.is_empty() {
            return Some(prompt.to_string());
        }
    }

    let messages = input
        .get("session")
        .and_then(|session| session.get("messages"))
        .or_else(|| input.get("messages"))
        .and_then(|messages| messages.as_array())?;

    for message in messages.iter().rev() {
        let role = message.get("role").and_then(|value| value.as_str());
        if role != Some("user") {
            continue;
        }

        let content = message.get("content");
        if let Some(text) = content.and_then(|value| value.as_str()) {
            let text = text.trim();
            if !text.is_empty() {
                return Some(text.to_string());
            }
        }

        if let Some(blocks) = content.and_then(|value| value.as_array()) {
            for block in blocks {
                if block.get("type").and_then(|value| value.as_str()) != Some("text") {
                    continue;
                }
                if let Some(text) = block.get("text").and_then(|value| value.as_str()) {
                    let text = text.trim();
                    if !text.is_empty() {
                        return Some(text.to_string());
                    }
                }
            }
        }
    }

    None
}

fn transcript_user_prompt(value: &Value) -> Option<String> {
    if value
        .get("isMeta")
        .or_else(|| value.get("is_meta"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return None;
    }
    let message = value.get("message").unwrap_or(value);
    let is_user = value.get("type").and_then(Value::as_str) == Some("user")
        || message.get("role").and_then(Value::as_str) == Some("user");
    if !is_user {
        return None;
    }

    let content = message.get("content")?;
    if let Some(text) = content
        .as_str()
        .map(str::trim)
        .filter(|text| !text.is_empty())
    {
        return Some(text.to_string());
    }

    let text = content
        .as_array()?
        .iter()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

/// Read only a bounded tail of Claude's JSONL transcript and classify the most
/// recent real user-text message. Tool-result-only user records are skipped.
pub fn latest_handoff_intent_from_transcript(path: &str) -> Option<HandoffIntent> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(MAX_TRANSCRIPT_TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;

    let mut bytes = Vec::with_capacity((len - start).min(MAX_TRANSCRIPT_TAIL_BYTES) as usize);
    file.read_to_end(&mut bytes).ok()?;
    let tail = String::from_utf8_lossy(&bytes);
    let complete_tail = if start == 0 {
        tail.as_ref()
    } else {
        tail.split_once('\n').map(|(_, rest)| rest).unwrap_or("")
    };

    complete_tail.lines().rev().find_map(|line| {
        let value: Value = serde_json::from_str(line).ok()?;
        let prompt = transcript_user_prompt(&value)?;
        Some(detect_handoff_intent(&prompt))
    })
}

/// Determine whether prompt text likely indicates a save/documentation request.
pub fn detects_save_intent(prompt: &str) -> bool {
    let prompt_lower = prompt.to_lowercase();
    let has_keyword = SAVE_KEYWORDS
        .iter()
        .any(|keyword| prompt_lower.contains(keyword));

    // Local-file hints are treated as high confidence save intent.
    let has_file_hint = LOCAL_FILE_HINTS
        .iter()
        .any(|hint| prompt_lower.contains(hint));
    has_file_hint || has_keyword
}

/// Return save guidance for this hook payload when save intent is detected.
pub fn guidance_for_input(input: &Value) -> Option<String> {
    let prompt = extract_user_prompt(input)?;
    match detect_handoff_intent(&prompt) {
        HandoffIntent::Canonical | HandoffIntent::ExplicitLocalFile => {
            Some(HANDOFF_INTENT_GUIDANCE.to_string())
        }
        HandoffIntent::None if detects_save_intent(&prompt) => {
            Some(SAVE_INTENT_GUIDANCE.to_string())
        }
        HandoffIntent::None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_direct_save_intent() {
        for prompt in [
            "Please save this decision for future reference.",
            "Remember that the API tests need a local Redis.",
            "From now on, always use pnpm instead of npm.",
            "Lesson learned: check the filesystem before deleting.",
            "Add a todo to rotate the staging keys.",
            "Write down the release steps as a runbook.",
            "Put these notes into a markdown file.",
        ] {
            assert!(detects_save_intent(prompt), "{prompt}");
        }
    }

    #[test]
    fn ignores_ordinary_coding_prompts() {
        for prompt in [
            "Explain how this module works.",
            "Read the config file at ./config/app.json and fix the path handling.",
            "Write this to docs/architecture.md and keep a summary.",
            "Document the public API in the README.",
            "Update the spec test for the parser.",
            "Record the request latency in the metrics module.",
            "Fix the save button on the settings page.",
        ] {
            assert!(!detects_save_intent(prompt), "{prompt}");
        }
    }

    #[test]
    fn save_guidance_routes_every_durable_kind() {
        for call in [
            "action=\"remember\"",
            "action=\"capture_lesson\"",
            "event_type=\"decision\"",
            "action=\"create_doc\"",
            "action=\"capture_plan\"",
            "action=\"create_todo\"",
            "kind=\"ticket\"",
            "kind=\"handoff\"",
        ] {
            assert!(SAVE_INTENT_GUIDANCE.contains(call), "{call}");
        }
    }

    #[test]
    fn detects_generic_handoff_without_authorizing_a_local_file() {
        for prompt in [
            "Please prepare a handoff for the next agent.",
            "Prepare the handoff with everything Claude needs to continue.",
            "Write up a handoff for another session.",
            "Hand this work over so another agent can continue.",
            "Package context for handoff to a fresh agent.",
        ] {
            assert_eq!(detect_handoff_intent(prompt), HandoffIntent::Canonical);
        }
    }

    #[test]
    fn explicit_local_handoff_file_is_only_an_additional_artifact() {
        for prompt in [
            "Create HANDOFF.md at the repository root for the next agent.",
            "Write HANDOFF.md with the current state.",
            "Update HANDOFF.md before stopping.",
        ] {
            assert_eq!(
                detect_handoff_intent(prompt),
                HandoffIntent::ExplicitLocalFile
            );
        }
        let guidance = guidance_for_input(&serde_json::json!({
            "prompt": "Create HANDOFF.md at the repository root for the next agent."
        }))
        .expect("handoff guidance");
        assert!(guidance.contains("create the ContextStream handoff first"));
        assert!(guidance.contains("additional artifact"));
    }

    #[test]
    fn negated_handoff_md_does_not_grant_local_file_authority() {
        assert_eq!(
            detect_handoff_intent(
                "Prepare a handoff, but do not create HANDOFF.md; use ContextStream."
            ),
            HandoffIntent::Canonical
        );
        assert_eq!(
            detect_handoff_intent(
                "Agents should create a handoff and not this: HANDOFF.md at the root."
            ),
            HandoffIntent::Canonical
        );
    }

    #[test]
    fn handoff_guidance_routes_entity_and_optional_capsule() {
        let guidance = guidance_for_input(&serde_json::json!({
            "prompt": "Please prepare a handoff for another agent and give me a share link."
        }))
        .expect("handoff guidance");
        assert!(guidance.contains("mcp__contextstream__entity"));
        assert!(guidance.contains("kind=\"handoff\""));
        assert!(guidance.contains("mcp__contextstream__capsule"));
        assert!(guidance.contains("HANDOFF.md"));
        assert!(guidance.contains("NOT a substitute"));
    }

    #[test]
    fn extracts_prompt_from_session_messages() {
        let input = serde_json::json!({
            "session": {
                "messages": [
                    {"role": "assistant", "content": "hi"},
                    {"role": "user", "content": "save this decision"}
                ]
            }
        });

        assert_eq!(
            extract_user_prompt(&input).as_deref(),
            Some("save this decision")
        );
    }

    #[test]
    fn transcript_tail_skips_tool_results_and_reads_latest_user_text() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("session.jsonl");
        let transcript = [
            serde_json::json!({
                "type": "user",
                "message": {
                    "role": "user",
                    "content": [{"type": "text", "text": "Prepare a handoff for the next agent."}]
                }
            })
            .to_string(),
            serde_json::json!({
                "type": "assistant",
                "message": {"role": "assistant", "content": "Working"}
            })
            .to_string(),
            serde_json::json!({
                "type": "user",
                "message": {
                    "role": "user",
                    "content": [{"type": "tool_result", "content": "ok"}]
                }
            })
            .to_string(),
        ]
        .join("\n");
        std::fs::write(&path, transcript).expect("write transcript");

        assert_eq!(
            latest_handoff_intent_from_transcript(path.to_str().expect("path")),
            Some(HandoffIntent::Canonical)
        );
    }
}
