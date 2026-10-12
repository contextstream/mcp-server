//! Coaching turns: hand each finished turn to ContextStream's coaching loop.
//!
//! The coaching evaluator on the server judges the person's message against
//! the agent's previous reply. It learns a coached rule when the person
//! corrected the agent, and it records whether the reply repeated a mistake
//! that was already coached. A hook sees the two halves at different times,
//! so the Stop hook keeps each reply here and the next prompt hook sends it
//! together with the new prompt. ContextStream's setup installs no Codex
//! hooks; Codex's `notify` program instead receives a whole turn at once, so
//! `codex-notify` pairs this turn's messages with the reply kept from the turn
//! before, then keeps this turn's reply.
//!
//! The Stop and prompt hooks send turns only when
//! `CONTEXTSTREAM_COACHING_TURNS=1`, so a prompt and reply leave the machine
//! only for people who turned coaching on (Agent Coach Kit sets it). Installing
//! `codex-notify` is itself that choice. The server still learns nothing
//! unless the person opted in to account learning; when it answers
//! `learning_off`, sending pauses for a day.
//!
//! State: `~/.contextstream/coaching-turns.json`, written atomically under a
//! sibling lock file.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::common::{extract_cwd, load_config, prefix_within_bytes};

/// Environment switch for the Stop and prompt hooks.
pub const ENABLE_ENV: &str = "CONTEXTSTREAM_COACHING_TURNS";

/// The server reads the first 4,000 characters of each message. Keeping a
/// little more than that bounds the state file without losing anything.
const MAX_KEPT_BYTES: usize = 16 * 1024;

/// Replies older than this are never paired with a prompt.
const PENDING_REPLY_MAX_AGE_HOURS: i64 = 24;

/// Sessions remembered at once; the oldest are dropped first.
const MAX_SESSIONS: usize = 200;

/// How long sending pauses after the server says learning is off.
const LEARNING_OFF_PAUSE_HOURS: i64 = 24;

const SEND_TIMEOUT: Duration = Duration::from_secs(3);

/// Whether the Stop and prompt hooks hand turns to the coaching loop.
pub fn enabled() -> bool {
    flag_is_on(std::env::var(ENABLE_ENV).ok().as_deref())
}

/// Whether `codex-notify` sends turns: on unless explicitly switched off,
/// because only Agent Coach Kit installs it.
fn codex_notify_enabled() -> bool {
    !flag_is_off(std::env::var(ENABLE_ENV).ok().as_deref())
}

fn flag_is_on(raw: Option<&str>) -> bool {
    matches!(
        raw.map(|value| value.trim().to_ascii_lowercase())
            .as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

fn flag_is_off(raw: Option<&str>) -> bool {
    matches!(
        raw.map(|value| value.trim().to_ascii_lowercase())
            .as_deref(),
        Some("0" | "false" | "no" | "off")
    )
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct SessionEntry {
    /// The agent's last reply, waiting for the person's next message.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending_reply: Option<String>,
    /// Replies seen in this session; the server keys judged turns on it.
    #[serde(default)]
    turns: u32,
    updated_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
struct CoachingTurnState {
    #[serde(default)]
    sessions: HashMap<String, SessionEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    learning_off_until: Option<String>,
}

fn state_path() -> Option<PathBuf> {
    dirs::home_dir().map(|home| home.join(".contextstream").join("coaching-turns.json"))
}

fn parse_time(value: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|time| time.with_timezone(&Utc))
}

fn prune(state: &mut CoachingTurnState, now: DateTime<Utc>) {
    let oldest_kept = now - chrono::Duration::hours(PENDING_REPLY_MAX_AGE_HOURS);
    state.sessions.retain(|_, entry| {
        parse_time(&entry.updated_at).is_some_and(|updated| updated >= oldest_kept)
    });
    if state.sessions.len() > MAX_SESSIONS {
        let mut by_age: Vec<(String, String)> = state
            .sessions
            .iter()
            .map(|(key, entry)| (entry.updated_at.clone(), key.clone()))
            .collect();
        by_age.sort_unstable();
        let excess = by_age.len() - MAX_SESSIONS;
        for (_, key) in by_age.into_iter().take(excess) {
            state.sessions.remove(&key);
        }
    }
    if state
        .learning_off_until
        .as_deref()
        .and_then(parse_time)
        .is_none_or(|until| until <= now)
    {
        state.learning_off_until = None;
    }
}

fn write_atomically(path: &Path, state: &CoachingTurnState) -> std::io::Result<()> {
    let json = serde_json::to_vec(state).map_err(std::io::Error::other)?;
    let temporary =
        path.with_file_name(format!("coaching-turns.json.{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&json)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// Read, update and write the state under an exclusive lock on a stable
/// sibling file, so hooks from parallel sessions never lose each other's
/// entries.
fn with_state_at<T>(
    path: &Path,
    now: DateTime<Utc>,
    update: impl FnOnce(&mut CoachingTurnState) -> T,
) -> std::io::Result<T> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let lock: File = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("lock"))?;
    fs2::FileExt::lock_exclusive(&lock)?;
    // An unreadable file is replaced rather than blocking every later turn.
    let mut state = match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => CoachingTurnState::default(),
        Err(error) => return Err(error),
    };
    prune(&mut state, now);
    let result = update(&mut state);
    write_atomically(path, &state)?;
    Ok(result)
}

fn remember_reply_at(path: &Path, now: DateTime<Utc>, session: &str, reply: &str) {
    let reply = prefix_within_bytes(reply.trim(), MAX_KEPT_BYTES).to_string();
    if reply.is_empty() {
        return;
    }
    let _ = with_state_at(path, now, |state| {
        let entry = state.sessions.entry(session.to_string()).or_default();
        entry.pending_reply = Some(reply);
        entry.turns = entry.turns.saturating_add(1);
        entry.updated_at = now.to_rfc3339();
    });
}

/// The reply waiting for this session's next message, and its turn number.
/// Taking it clears it, so one reply is judged against one message.
fn take_reply_at(path: &Path, now: DateTime<Utc>, session: &str) -> Option<(String, u32)> {
    with_state_at(path, now, |state| {
        let paused = state.learning_off_until.is_some();
        let entry = state.sessions.get_mut(session)?;
        let reply = entry.pending_reply.take()?;
        entry.updated_at = now.to_rfc3339();
        (!paused).then_some((reply, entry.turns))
    })
    .ok()
    .flatten()
}

fn pause_for_learning_off_at(path: &Path, now: DateTime<Utc>) {
    let until = now + chrono::Duration::hours(LEARNING_OFF_PAUSE_HOURS);
    let _ = with_state_at(path, now, |state| {
        state.learning_off_until = Some(until.to_rfc3339());
    });
}

/// Keep the agent's reply for this session until the person answers it.
pub fn remember_reply(session: &str, reply: &str) {
    if let Some(path) = state_path() {
        remember_reply_at(&path, Utc::now(), session, reply);
    }
}

/// Take the reply the person's new message answers, if one is waiting.
pub fn take_reply(session: &str) -> Option<(String, u32)> {
    state_path().and_then(|path| take_reply_at(&path, Utc::now(), session))
}

/// Where and as whom a turn is sent.
pub struct Destination<'a> {
    pub api_url: &'a str,
    pub api_key: &'a str,
    pub workspace_id: Option<&'a str>,
    pub project_id: Option<&'a str>,
}

/// One turn as the server judges it: the person's message and the reply it
/// answers.
pub struct Turn<'a> {
    pub session_id: &'a str,
    pub turn_number: u32,
    pub user_message: &'a str,
    pub assistant_message: &'a str,
}

fn request_body(destination: &Destination<'_>, turn: &Turn<'_>) -> Value {
    let mut body = serde_json::json!({
        "session_id": turn.session_id,
        "turn_number": turn.turn_number,
        "user_message": prefix_within_bytes(turn.user_message, MAX_KEPT_BYTES),
        "assistant_message": prefix_within_bytes(turn.assistant_message, MAX_KEPT_BYTES),
    });
    if let Some(workspace_id) = destination.workspace_id {
        body["workspace_id"] = Value::String(workspace_id.to_string());
    }
    if let Some(project_id) = destination.project_id {
        body["project_id"] = Value::String(project_id.to_string());
    }
    body
}

/// Whether the server's answer says the person has not opted in to account
/// learning (`{"data": {"accepted": false, "reason": "learning_off"}}`).
fn says_learning_off(response: &Value) -> bool {
    let data = response.get("data").unwrap_or(response);
    data.get("accepted").and_then(Value::as_bool) == Some(false)
        && data.get("reason").and_then(Value::as_str) == Some("learning_off")
}

/// Send one turn. Best effort: network and API errors are ignored.
pub async fn send(destination: &Destination<'_>, turn: &Turn<'_>) {
    if destination.api_key.is_empty()
        || turn.user_message.trim().is_empty()
        || turn.assistant_message.trim().is_empty()
    {
        return;
    }
    let response = super::api_http_client()
        .post(format!(
            "{}/api/v1/context/coaching/turn",
            destination.api_url.trim_end_matches('/')
        ))
        .header("X-API-Key", destination.api_key)
        .header("Content-Type", "application/json")
        .json(&request_body(destination, turn))
        .timeout(SEND_TIMEOUT)
        .send()
        .await;
    let Ok(response) = response else { return };
    let Ok(body) = response.json::<Value>().await else {
        return;
    };
    if says_learning_off(&body) {
        if let Some(path) = state_path() {
            pause_for_learning_off_at(&path, Utc::now());
        }
    }
}

/// A Codex `agent-turn-complete` notification reduced to what coaching needs.
#[derive(Debug, PartialEq)]
struct CodexTurn {
    session: String,
    user_message: String,
    reply: String,
    cwd: Option<String>,
}

fn codex_turn(payload: &Value) -> Option<CodexTurn> {
    if payload.get("type").and_then(Value::as_str) != Some("agent-turn-complete") {
        return None;
    }
    let text = |keys: &[&str]| {
        keys.iter().find_map(|key| {
            payload
                .get(*key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        })
    };
    let cwd = text(&["cwd"]);
    let session = text(&["thread-id", "thread_id", "session_id", "conversation_id"])
        .or_else(|| cwd.as_ref().map(|cwd| format!("codex:{cwd}")))?;
    let user_message = payload
        .get("input-messages")
        .or_else(|| payload.get("input_messages"))
        .and_then(Value::as_array)
        .map(|messages| {
            messages
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|message| !message.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .unwrap_or_default();
    let reply = text(&["last-assistant-message", "last_assistant_message"]).unwrap_or_default();
    if user_message.is_empty() && reply.is_empty() {
        return None;
    }
    Some(CodexTurn {
        session,
        user_message,
        reply,
        cwd,
    })
}

/// The notification JSON: Codex passes it as the last argument. Piped stdin is
/// accepted too so the handler can be driven by hand; a terminal is never
/// read, so a stray manual run cannot hang.
fn codex_payload() -> Option<Value> {
    use std::io::IsTerminal;
    super::git_common::hook_args()
        .iter()
        .rev()
        .find_map(|arg| serde_json::from_str::<Value>(arg.trim()).ok())
        .or_else(|| {
            if std::io::stdin().is_terminal() {
                return None;
            }
            let raw = super::git_common::read_stdin_raw();
            serde_json::from_str::<Value>(raw.trim()).ok()
        })
}

/// `contextstream-mcp hook codex-notify '<json>'`, run by Codex's `notify`.
pub async fn handle_codex_notify() -> Result<()> {
    if !codex_notify_enabled() {
        return Ok(());
    }
    let Some(turn) = codex_payload().as_ref().and_then(codex_turn) else {
        return Ok(());
    };
    let cwd = turn
        .cwd
        .clone()
        .unwrap_or_else(|| extract_cwd(&serde_json::json!({})));
    let config = load_config(&cwd);
    if !config.is_configured() {
        return Ok(());
    }
    if !turn.user_message.is_empty() {
        if let Some((previous_reply, turn_number)) = take_reply(&turn.session) {
            send(
                &Destination {
                    api_url: &config.api_url,
                    api_key: &config.api_key,
                    workspace_id: config.workspace_id.as_deref(),
                    project_id: config.project_id.as_deref(),
                },
                &Turn {
                    session_id: &turn.session,
                    turn_number,
                    user_message: &turn.user_message,
                    assistant_message: &previous_reply,
                },
            )
            .await;
        }
    }
    if !turn.reply.is_empty() {
        remember_reply(&turn.session, &turn.reply);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(hours: i64) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-12T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
            + chrono::Duration::hours(hours)
    }

    fn state_file() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("coaching-turns.json");
        (dir, path)
    }

    #[test]
    fn the_hook_switch_is_off_unless_turned_on() {
        assert!(!flag_is_on(None));
        assert!(!flag_is_on(Some("")));
        assert!(!flag_is_on(Some("0")));
        assert!(flag_is_on(Some("1")));
        assert!(flag_is_on(Some(" TRUE ")));
        assert!(!flag_is_off(None));
        assert!(flag_is_off(Some("false")));
        assert!(flag_is_off(Some("0")));
    }

    #[test]
    fn a_reply_is_paired_with_the_next_message_once() {
        let (_dir, path) = state_file();
        assert_eq!(take_reply_at(&path, at(0), "s1"), None);
        remember_reply_at(&path, at(0), "s1", "  I used spaces.  ");
        assert_eq!(
            take_reply_at(&path, at(0), "s1"),
            Some(("I used spaces.".to_string(), 1))
        );
        assert_eq!(take_reply_at(&path, at(0), "s1"), None);
        remember_reply_at(&path, at(1), "s1", "Switched to tabs.");
        assert_eq!(
            take_reply_at(&path, at(1), "s1"),
            Some(("Switched to tabs.".to_string(), 2))
        );
    }

    #[test]
    fn sessions_do_not_share_replies() {
        let (_dir, path) = state_file();
        remember_reply_at(&path, at(0), "a", "reply a");
        remember_reply_at(&path, at(0), "b", "reply b");
        assert_eq!(take_reply_at(&path, at(0), "b").unwrap().0, "reply b");
        assert_eq!(take_reply_at(&path, at(0), "a").unwrap().0, "reply a");
    }

    #[test]
    fn a_blank_reply_is_not_kept() {
        let (_dir, path) = state_file();
        remember_reply_at(&path, at(0), "s1", " \n ");
        assert_eq!(take_reply_at(&path, at(0), "s1"), None);
    }

    #[test]
    fn a_stale_reply_is_dropped() {
        let (_dir, path) = state_file();
        remember_reply_at(&path, at(0), "s1", "old reply");
        assert_eq!(
            take_reply_at(&path, at(PENDING_REPLY_MAX_AGE_HOURS + 1), "s1"),
            None
        );
    }

    #[test]
    fn the_oldest_sessions_go_first_past_the_limit() {
        let (_dir, path) = state_file();
        let start = at(0);
        for index in 0..(MAX_SESSIONS + 5) {
            let now = start + chrono::Duration::seconds(index as i64);
            remember_reply_at(&path, now, &format!("s{index}"), "reply");
        }
        let now = start + chrono::Duration::seconds((MAX_SESSIONS + 5) as i64);
        assert_eq!(take_reply_at(&path, now, "s0"), None);
        assert!(take_reply_at(&path, now, &format!("s{}", MAX_SESSIONS + 4)).is_some());
    }

    #[test]
    fn a_long_reply_is_cut_at_a_character_boundary() {
        let (_dir, path) = state_file();
        let long = "é".repeat(MAX_KEPT_BYTES);
        remember_reply_at(&path, at(0), "s1", &long);
        let (kept, _) = take_reply_at(&path, at(0), "s1").unwrap();
        assert!(kept.len() <= MAX_KEPT_BYTES);
        assert!(kept.chars().all(|ch| ch == 'é'));
    }

    #[test]
    fn learning_off_pauses_sending_for_a_day() {
        let (_dir, path) = state_file();
        pause_for_learning_off_at(&path, at(0));
        remember_reply_at(&path, at(1), "s1", "reply");
        assert_eq!(take_reply_at(&path, at(2), "s1"), None);
        remember_reply_at(&path, at(LEARNING_OFF_PAUSE_HOURS + 1), "s1", "later reply");
        assert_eq!(
            take_reply_at(&path, at(LEARNING_OFF_PAUSE_HOURS + 1), "s1").map(|(reply, _)| reply),
            Some("later reply".to_string())
        );
    }

    #[test]
    fn an_unreadable_state_file_is_replaced() {
        let (_dir, path) = state_file();
        std::fs::write(&path, b"not json").unwrap();
        remember_reply_at(&path, at(0), "s1", "reply");
        assert!(take_reply_at(&path, at(0), "s1").is_some());
    }

    #[test]
    fn the_request_carries_the_turn_and_its_scope() {
        let destination = Destination {
            api_url: "https://api.example",
            api_key: "key",
            workspace_id: Some("ws"),
            project_id: None,
        };
        let turn = Turn {
            session_id: "s1",
            turn_number: 3,
            user_message: "no, use tabs",
            assistant_message: "I used spaces.",
        };
        assert_eq!(
            request_body(&destination, &turn),
            serde_json::json!({
                "session_id": "s1",
                "turn_number": 3,
                "user_message": "no, use tabs",
                "assistant_message": "I used spaces.",
                "workspace_id": "ws",
            })
        );
    }

    #[test]
    fn only_a_learning_off_answer_pauses() {
        assert!(says_learning_off(&serde_json::json!({
            "success": true,
            "data": {"accepted": false, "reason": "learning_off"}
        })));
        assert!(!says_learning_off(&serde_json::json!({
            "success": true,
            "data": {"accepted": true}
        })));
        assert!(!says_learning_off(
            &serde_json::json!({"error": "rate limited"})
        ));
    }

    #[test]
    fn a_codex_turn_is_read_from_its_notification() {
        let payload = serde_json::json!({
            "type": "agent-turn-complete",
            "thread-id": "t-1",
            "turn-id": "u-9",
            "cwd": "/repo",
            "input-messages": ["no, use tabs", "  "],
            "last-assistant-message": "Switched to tabs."
        });
        assert_eq!(
            codex_turn(&payload),
            Some(CodexTurn {
                session: "t-1".to_string(),
                user_message: "no, use tabs".to_string(),
                reply: "Switched to tabs.".to_string(),
                cwd: Some("/repo".to_string()),
            })
        );
    }

    #[test]
    fn a_codex_turn_without_a_thread_is_keyed_by_its_folder() {
        let payload = serde_json::json!({
            "type": "agent-turn-complete",
            "cwd": "/repo",
            "input-messages": ["hi"],
            "last-assistant-message": "hello"
        });
        assert_eq!(codex_turn(&payload).unwrap().session, "codex:/repo");
    }

    #[test]
    fn other_codex_notifications_are_ignored() {
        assert_eq!(
            codex_turn(&serde_json::json!({"type": "approval-requested"})),
            None
        );
        assert_eq!(
            codex_turn(&serde_json::json!({
                "type": "agent-turn-complete",
                "thread-id": "t-1",
                "input-messages": [],
                "last-assistant-message": ""
            })),
            None
        );
        assert_eq!(
            codex_turn(&serde_json::json!({
                "type": "agent-turn-complete",
                "input-messages": ["hi"],
                "last-assistant-message": "hello"
            })),
            None,
            "no thread and no folder leaves nothing to pair turns by"
        );
    }
}
